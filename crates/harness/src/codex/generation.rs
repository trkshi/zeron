//! Opt-in, read-only codex-lb metrics joined to this thread's rollout response IDs.
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::{Client, StatusCode, Url, header};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use zeron_proto::GenerationUsage;

use crate::jsonrpc::RpcClient;

const MAX_LINE: usize = 64 * 1024;
const READ_BUDGET: usize = 1024 * 1024;
const MAX_BODY: usize = 4096;
const MAX_ATTEMPTS: u8 = 8;

#[derive(Deserialize)]
struct OptIn {
    enabled: bool,
    origin: String,
}

#[derive(Deserialize)]
struct RolloutRecord {
    #[serde(rename = "type")]
    kind: String,
    payload: UsageRecord,
}

#[derive(Deserialize)]
struct UsageRecord {
    thread_id: String,
    turn_id: String,
    response_id: String,
}

#[derive(Deserialize)]
struct ResponseMetrics {
    response_id: String,
    output_tokens: Option<u64>,
    reasoning_output_tokens: Option<u64>,
    elapsed_ms: Option<u64>,
    ttft_ms: Option<u64>,
}

impl ResponseMetrics {
    fn generation(self, response_id: &str) -> Option<GenerationUsage> {
        if self.response_id != response_id {
            return None;
        }
        let usage = GenerationUsage {
            output_tokens: self.output_tokens?,
            reasoning_output_tokens: self.reasoning_output_tokens,
            elapsed_ms: self.elapsed_ms?,
            ttft_ms: self.ttft_ms?,
        };
        usage.tps().map(|_| usage)
    }
}

#[derive(Default)]
struct RolloutTail {
    offset: u64,
    line: Vec<u8>,
    discard_line: bool,
}

impl RolloutTail {
    async fn latest(&mut self, path: &Path, thread: &str, turn: &str) -> Option<String> {
        let mut file = tokio::fs::File::open(path).await.ok()?;
        let size = file.metadata().await.ok()?.len();
        if size < self.offset {
            *self = Self::default();
        }
        file.seek(std::io::SeekFrom::Start(self.offset))
            .await
            .ok()?;
        let mut chunk = [0; 16 * 1024];
        let mut budget = READ_BUDGET;
        let mut latest = None;
        while budget > 0 {
            let limit = chunk.len().min(budget);
            let len = file.read(&mut chunk[..limit]).await.ok()?;
            if len == 0 {
                break;
            }
            self.offset += len as u64;
            budget -= len;
            if let Some(id) = self.consume(&chunk[..len], thread, turn) {
                latest = Some(id);
            }
        }
        latest
    }

    fn consume(&mut self, bytes: &[u8], thread: &str, turn: &str) -> Option<String> {
        let mut latest = None;
        for &byte in bytes {
            if byte == b'\n' {
                if !self.discard_line
                    && let Ok(record) = serde_json::from_slice::<RolloutRecord>(&self.line)
                    && record.kind == "token_usage_record"
                    && record.payload.thread_id == thread
                    && record.payload.turn_id == turn
                    && valid_response_id(&record.payload.response_id)
                {
                    latest = Some(record.payload.response_id);
                }
                self.line.clear();
                self.discard_line = false;
            } else if !self.discard_line {
                if self.line.len() == MAX_LINE {
                    // Large prompt/tool records are irrelevant to metrics.
                    self.line.clear();
                    self.discard_line = true;
                } else {
                    self.line.push(byte);
                }
            }
        }
        latest
    }
}

fn valid_response_id(id: &str) -> bool {
    id.starts_with("resp_")
        && (6..=200).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn approved_endpoint(opt_in: &OptIn, config: &Value, provider: &str) -> Option<Url> {
    if !opt_in.enabled || provider != "codex-lb" {
        return None;
    }
    let provider = config.get("model_providers")?.get(provider)?;
    if provider.get("env_key")?.as_str()? != "CODEX_LB_API_KEY" {
        return None;
    }
    let base = Url::parse(provider.get("base_url")?.as_str()?).ok()?;
    let approved = Url::parse(&opt_in.origin).ok()?;
    for url in [&base, &approved] {
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return None;
        }
    }
    if approved.path() != "/" || approved.origin() != base.origin() {
        return None;
    }
    base.join("/v1/responses/").ok()
}

struct Lookup {
    response_id: String,
    attempts: u8,
}

// The client contains a sensitive default Authorization header; never Debug/log it.
pub(super) struct Monitor {
    http: Client,
    endpoint: Url,
    path: PathBuf,
    thread: String,
    turn: String,
    tail: RolloutTail,
    last_response: Option<String>,
    pending: Option<Lookup>,
    suspended: bool,
    disabled: bool,
}

impl Monitor {
    pub async fn from_thread(client: &RpcClient, cwd: &str, thread: &Value) -> Option<Self> {
        let home = std::env::var_os("CODEX_HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .or_else(|| crate::executable::home_dir().map(|home| home.join(".codex")))?;
        let opt_path = home.join("zeron-codex-lb-usage.toml");
        if tokio::fs::metadata(&opt_path).await.ok()?.len() > MAX_BODY as u64 {
            return None;
        }
        let opt_in: OptIn =
            toml::from_str(&tokio::fs::read_to_string(opt_path).await.ok()?).ok()?;
        if !opt_in.enabled {
            return None;
        }
        let path = PathBuf::from(thread.pointer("/thread/path")?.as_str()?);
        let path = tokio::fs::canonicalize(path).await.ok()?;
        let home = tokio::fs::canonicalize(home).await.ok()?;
        if !path.starts_with(home.join("sessions"))
            && !path.starts_with(home.join("archived_sessions"))
        {
            return None;
        }
        let config = tokio::time::timeout(
            Duration::from_secs(1),
            client.request("config/read", json!({"includeLayers": false, "cwd": cwd})),
        )
        .await
        .ok()?
        .ok()?;
        let endpoint = approved_endpoint(
            &opt_in,
            config.get("config")?,
            thread.get("modelProvider")?.as_str()?,
        )?;
        let key = std::env::var("CODEX_LB_API_KEY")
            .ok()
            .filter(|key| !key.is_empty())?;
        let mut authorization = header::HeaderValue::from_str(&format!("Bearer {key}")).ok()?;
        authorization.set_sensitive(true);
        let mut headers = header::HeaderMap::new();
        headers.insert(header::AUTHORIZATION, authorization);
        let http = Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_millis(400))
            .build()
            .ok()?;
        // Start at EOF before the first turn, not at historical prompts or reports.
        let mut file = tokio::fs::File::open(&path).await.ok()?;
        let offset = file.metadata().await.ok()?.len();
        let mut discard_line = false;
        if offset > 0 {
            file.seek(std::io::SeekFrom::Start(offset - 1)).await.ok()?;
            let mut byte = [0];
            file.read_exact(&mut byte).await.ok()?;
            discard_line = byte[0] != b'\n';
        }
        Some(Self {
            http,
            endpoint,
            path,
            thread: thread.pointer("/thread/id")?.as_str()?.to_owned(),
            turn: String::new(),
            tail: RolloutTail {
                offset,
                discard_line,
                ..Default::default()
            },
            last_response: None,
            pending: None,
            suspended: false,
            disabled: false,
        })
    }

    pub fn suspend(&mut self, turn: &str) {
        self.set_turn(turn);
        self.suspended = true;
        self.pending = None;
    }

    fn set_turn(&mut self, turn: &str) {
        if self.turn != turn {
            self.turn = turn.to_owned();
            self.pending = None;
            self.last_response = None;
            self.suspended = false;
        }
    }

    pub async fn poll(&mut self, turn: &str) -> Option<GenerationUsage> {
        self.set_turn(turn);
        if self.disabled || self.suspended {
            return None;
        }
        if let Some(id) = self.tail.latest(&self.path, &self.thread, turn).await
            && self.last_response.as_deref() != Some(&id)
        {
            self.last_response = Some(id.clone());
            self.pending = Some(Lookup {
                response_id: id,
                attempts: 0,
            });
        }
        let lookup = self.pending.as_mut()?;
        lookup.attempts += 1;
        let mut url = self.endpoint.clone();
        url.path_segments_mut()
            .ok()?
            .pop_if_empty()
            .push(&lookup.response_id)
            .push("metrics");
        let result = fetch(&self.http, url, &lookup.response_id).await;
        match result {
            Fetch::Ready(usage) => {
                self.pending = None;
                usage
            }
            Fetch::Disabled => {
                self.disabled = true;
                self.pending = None;
                None
            }
            Fetch::Retry => {
                if lookup.attempts >= MAX_ATTEMPTS {
                    self.pending = None;
                }
                None
            }
        }
    }

    pub async fn finish(&mut self, turn: &str) -> Option<GenerationUsage> {
        // Rollout and request-log writes can lag the app-server completion.
        // The caller bounds this entire flush and makes it interruptible.
        for _ in 0..4 {
            if let Some(usage) = self.poll(turn).await {
                return Some(usage);
            }
            if self.disabled || self.suspended {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
    }
}

enum Fetch {
    Ready(Option<GenerationUsage>),
    Retry,
    Disabled,
}

async fn fetch(http: &Client, url: Url, response_id: &str) -> Fetch {
    let Ok(mut response) = http.get(url).send().await else {
        return Fetch::Retry;
    };
    match response.status() {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => return Fetch::Disabled,
        StatusCode::NOT_FOUND => return Fetch::Retry,
        status if status.is_redirection() => return Fetch::Disabled,
        status if !status.is_success() => return Fetch::Retry,
        _ => {}
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if body.len() + chunk.len() <= MAX_BODY => {
                body.extend_from_slice(&chunk)
            }
            Ok(None) => break,
            _ => return Fetch::Ready(None),
        }
    }
    Fetch::Ready(
        serde_json::from_slice::<ResponseMetrics>(&body)
            .ok()
            .and_then(|metrics| metrics.generation(response_id)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn record(thread: &str, turn: &str, response: &str) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(&json!({
            "type": "token_usage_record",
            "payload": {"thread_id": thread, "turn_id": turn, "response_id": response},
        }))
        .unwrap();
        bytes.push(b'\n');
        bytes
    }

    #[test]
    fn rollout_matches_thread_turn_and_latest_response_without_reading_history() {
        let mut tail = RolloutTail::default();
        let mut bytes = record("parent", "old", "resp_old");
        bytes.extend(record("child", "turn", "resp_child"));
        bytes.extend(record("parent", "turn", "resp_first"));
        bytes.extend(record("parent", "turn", "resp_latest"));
        bytes.extend(record("parent", "turn", "resp_bad/path"));
        assert_eq!(
            tail.consume(&bytes, "parent", "turn"),
            Some("resp_latest".into())
        );
    }

    #[test]
    fn tail_handles_partial_and_oversized_lines() {
        let mut tail = RolloutTail::default();
        let bytes = record("parent", "turn", "resp_partial");
        assert_eq!(tail.consume(&bytes[..10], "parent", "turn"), None);
        assert_eq!(
            tail.consume(&bytes[10..], "parent", "turn"),
            Some("resp_partial".into())
        );
        assert_eq!(
            tail.consume(&vec![b'x'; MAX_LINE + 10], "parent", "turn"),
            None
        );
        assert!(tail.line.is_empty());
        let mut next = vec![b'\n'];
        next.extend(record("parent", "turn", "resp_next"));
        assert_eq!(
            tail.consume(&next, "parent", "turn"),
            Some("resp_next".into())
        );
    }

    #[test]
    fn only_the_approved_provider_origin_can_receive_the_key() {
        let opt = OptIn {
            enabled: true,
            origin: "http://localhost:2455".into(),
        };
        let config = json!({"model_providers": {"codex-lb": {
            "env_key": "CODEX_LB_API_KEY", "base_url": "http://localhost:2455/backend-api/codex",
        }}});
        assert_eq!(
            approved_endpoint(&opt, &config, "codex-lb")
                .unwrap()
                .as_str(),
            "http://localhost:2455/v1/responses/"
        );
        assert!(approved_endpoint(&opt, &config, "openai").is_none());
        for base in [
            "https://localhost:2455",
            "http://other-host:2455",
            "http://localhost:2456",
            "http://user@localhost:2455",
            "http://localhost:2455/?secret=x",
        ] {
            let mut changed = config.clone();
            changed["model_providers"]["codex-lb"]["base_url"] = base.into();
            assert!(approved_endpoint(&opt, &changed, "codex-lb").is_none());
        }
    }

    #[test]
    fn metrics_require_exact_response_and_real_generation_window() {
        let parse = |value| serde_json::from_value::<ResponseMetrics>(value).unwrap();
        let value = json!({"response_id": "resp_test", "output_tokens": 100,
            "reasoning_output_tokens": 40, "elapsed_ms": 1000, "ttft_ms": 200});
        assert_eq!(
            parse(value.clone()).generation("resp_test").unwrap().tps(),
            Some(75.0)
        );
        assert!(parse(value.clone()).generation("resp_other").is_none());
        for ttft in [Value::Null, json!(1000), json!(1001)] {
            let mut changed = value.clone();
            changed["ttft_ms"] = ttft;
            assert!(parse(changed).generation("resp_test").is_none());
        }
    }

    async fn server(
        replies: Vec<(&'static str, &'static str)>,
    ) -> (Url, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!(
            "http://{}/v1/responses/",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let worker = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                    assert!(request.len() < MAX_BODY);
                }
                requests.push(String::from_utf8(request).unwrap());
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
            requests
        });
        (url, worker)
    }

    fn monitor(endpoint: Url, path: PathBuf) -> Monitor {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_static("Bearer fixture-key"),
        );
        Monitor {
            http: Client::builder()
                .default_headers(headers)
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_millis(400))
                .build()
                .unwrap(),
            endpoint,
            path,
            thread: "parent".into(),
            turn: String::new(),
            tail: RolloutTail::default(),
            last_response: None,
            pending: None,
            suspended: false,
            disabled: false,
        }
    }

    #[tokio::test]
    async fn delayed_log_lookup_joins_only_current_turn_and_does_not_recount() {
        let (endpoint, server) = server(vec![
            ("404 Not Found", "{}"),
            ("200 OK", r#"{"response_id":"resp_live","output_tokens":100,"reasoning_output_tokens":40,"elapsed_ms":1000,"ttft_ms":200}"#),
        ]).await;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollout.jsonl");
        let mut records = record("parent", "old", "resp_old");
        records.extend(record("child", "turn", "resp_child"));
        records.extend(record("parent", "turn", "resp_live"));
        tokio::fs::write(&path, records).await.unwrap();
        let mut monitor = monitor(endpoint, path);
        assert!(monitor.poll("turn").await.is_none());
        assert_eq!(monitor.poll("turn").await.unwrap().tps(), Some(75.0));
        assert!(monitor.poll("turn").await.is_none());
        assert!(monitor.poll("next-turn").await.is_none());
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests {
            assert!(request.starts_with("GET /v1/responses/resp_live/metrics HTTP/1.1\r\n"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer fixture-key\r\n")
            );
        }
    }

    #[tokio::test]
    async fn redirects_and_failed_auth_disable_lookups_without_following() {
        for status in ["302 Found", "401 Unauthorized", "403 Forbidden"] {
            let (endpoint, server) = server(vec![(status, "{}")]).await;
            assert!(matches!(
                fetch(
                    &monitor(endpoint.clone(), PathBuf::new()).http,
                    endpoint,
                    "resp_test"
                )
                .await,
                Fetch::Disabled
            ));
            assert_eq!(server.await.unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn inplace_steering_suspends_telemetry_until_a_distinct_turn() {
        let mut monitor = monitor(
            Url::parse("http://127.0.0.1:1/v1/responses/").unwrap(),
            PathBuf::new(),
        );
        monitor.suspend("same-turn");
        assert!(monitor.poll("same-turn").await.is_none());
        assert!(monitor.suspended);
        assert!(monitor.poll("next-turn").await.is_none());
        assert!(!monitor.suspended);
    }
}
