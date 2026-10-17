//! Custom Anthropic-compatible endpoints for Claude Code (DeepSeek, gateways).
//!
//! Each endpoint is a base URL, a bearer token and the models it serves,
//! stored per device in `{data_dir}/claude-endpoints.json` (0600, never
//! synced). Its models join the Claude Code catalog under namespaced ids
//! (`endpoint:deepseek:deepseek-v4-pro`). A run on one launches `claude` with
//! the endpoint in its environment, so the CLI, its tools and the transcript
//! format stay the same while requests go to that backend.
//!
//! A chat stays on one backend: providers sign the thinking blocks they
//! write, so a conversation resumed on another backend fails. [`backend`]
//! tells them apart for the picker's lock and the runtime identity.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError, RwLock};

use serde::{Deserialize, Serialize};
use zeron_proto::{Model, ReasoningLevel};

use crate::HarnessError;

const PREFIX: &str = "endpoint:";
const FILE: &str = "claude-endpoints.json";

/// Set once by the engine at startup; unset (tests, probes) means none.
static PATH: RwLock<Option<PathBuf>> = RwLock::new(None);
/// Serializes read-modify-write saves.
static WRITE: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    pub id: String,
    pub label: String,
    pub base_url: String,
    pub token: String,
    #[serde(default)]
    pub models: Vec<String>,
    /// Background and haiku-class requests; the selected model otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub small_model: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct EndpointsFile {
    #[serde(default)]
    endpoints: Vec<Endpoint>,
}

/// What Settings sees: everything except the token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointSummary {
    pub id: String,
    pub label: String,
    pub base_url: String,
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub small_model: Option<String>,
    /// The token's last four characters.
    pub token_hint: String,
}

/// A save from Settings, keyed by the label's slug. An omitted or empty
/// token keeps the saved one. `replaces` names the endpoint being edited, so
/// a rename moves it instead of adding a second one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EndpointInput {
    pub replaces: Option<String>,
    pub label: String,
    pub base_url: String,
    pub token: Option<String>,
    pub models: Vec<String>,
    pub small_model: Option<String>,
}

/// Remember the engine data dir that holds the endpoints file.
pub fn init(data_dir: &Path) {
    *PATH.write().unwrap_or_else(PoisonError::into_inner) = Some(data_dir.join(FILE));
}

pub(crate) fn path() -> Option<PathBuf> {
    PATH.read().unwrap_or_else(PoisonError::into_inner).clone()
}

/// The saved endpoints; a missing or unreadable file has none.
pub fn list() -> Vec<Endpoint> {
    path().map(|path| read(&path)).unwrap_or_default()
}

pub fn summaries() -> Vec<EndpointSummary> {
    list().iter().map(summary).collect()
}

/// Add or replace the endpoint named by `input.label`.
pub fn save(input: EndpointInput) -> Result<Vec<EndpointSummary>, String> {
    let path = path().ok_or("custom endpoints are unavailable on this engine")?;
    let _guard = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
    let mut endpoints = read(&path);
    let replaces = input.replaces.clone();
    let previous = replaces
        .clone()
        .or_else(|| slug(&input.label))
        .and_then(|id| endpoints.iter().find(|e| e.id == id))
        .map(|e| e.token.clone());
    let endpoint = validate(input, previous)?;
    if let Some(old) = replaces.filter(|old| *old != endpoint.id) {
        endpoints.retain(|e| e.id != old);
    }
    match endpoints.iter_mut().find(|e| e.id == endpoint.id) {
        Some(slot) => *slot = endpoint,
        None => endpoints.push(endpoint),
    }
    write(&path, &endpoints).map_err(|error| format!("couldn't save endpoints: {error}"))?;
    Ok(endpoints.iter().map(summary).collect())
}

pub fn remove(id: &str) -> Result<Vec<EndpointSummary>, String> {
    let path = path().ok_or("custom endpoints are unavailable on this engine")?;
    let _guard = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
    let mut endpoints = read(&path);
    endpoints.retain(|e| e.id != id);
    write(&path, &endpoints).map_err(|error| format!("couldn't save endpoints: {error}"))?;
    Ok(endpoints.iter().map(summary).collect())
}

/// The catalog id of an endpoint's model.
pub fn model_id(endpoint: &str, model: &str) -> String {
    format!("{PREFIX}{endpoint}:{model}")
}

/// `(endpoint id, provider model)` of a namespaced catalog id.
pub fn split(model: &str) -> Option<(&str, &str)> {
    model
        .strip_prefix(PREFIX)?
        .split_once(':')
        .filter(|(id, model)| !id.is_empty() && !model.is_empty())
}

/// The backend a Claude Code model runs on: `None` is Anthropic itself.
pub fn backend(model: Option<&str>) -> Option<&str> {
    model.and_then(split).map(|(id, _)| id)
}

/// The model name the CLI receives: the provider's own for endpoint models.
pub(crate) fn provider_model(model: &str) -> &str {
    split(model).map_or(model, |(_, model)| model)
}

/// Catalog rows for every endpoint's models.
pub(crate) fn models(endpoints: &[Endpoint]) -> Vec<Model> {
    endpoints
        .iter()
        .flat_map(|endpoint| {
            endpoint.models.iter().map(|model| Model {
                id: model_id(&endpoint.id, model),
                label: model.clone(),
                description: Some(format!("{} endpoint", endpoint.label)),
                reasoning_levels: vec![
                    ReasoningLevel::Low,
                    ReasoningLevel::Medium,
                    ReasoningLevel::High,
                    ReasoningLevel::Max,
                ],
                options: vec![],
            })
        })
        .collect()
}

/// Environment for a run on an endpoint model, `None` for Anthropic models.
/// Every model slot points at the endpoint so background and subagent
/// requests never fall back to Anthropic. The listed variables are removed
/// so an inherited key or provider switch can't take precedence.
pub(crate) fn launch_env(
    model: Option<&str>,
    endpoints: &[Endpoint],
) -> Result<Option<LaunchEnv>, HarnessError> {
    let Some((id, model)) = model.and_then(split) else {
        return Ok(None);
    };
    let endpoint = endpoints.iter().find(|e| e.id == id).ok_or_else(|| {
        HarnessError::Protocol(format!(
            "the Claude Code endpoint \"{id}\" no longer exists on this device; \
             add it again in Settings > Providers or start a new chat"
        ))
    })?;
    let small = endpoint.small_model.as_deref().unwrap_or(model);
    let set = [
        ("ANTHROPIC_BASE_URL", endpoint.base_url.as_str()),
        ("ANTHROPIC_AUTH_TOKEN", endpoint.token.as_str()),
        ("ANTHROPIC_MODEL", model),
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", model),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", model),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", small),
        ("ANTHROPIC_SMALL_FAST_MODEL", small),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ]
    .map(|(key, value)| (key, value.to_owned()))
    .to_vec();
    Ok(Some(LaunchEnv {
        set,
        remove: &[
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ],
    }))
}

pub(crate) struct LaunchEnv {
    pub set: Vec<(&'static str, String)>,
    pub remove: &'static [&'static str],
}

fn summary(endpoint: &Endpoint) -> EndpointSummary {
    let chars: Vec<char> = endpoint.token.chars().collect();
    let tail: String = chars[chars.len().saturating_sub(4)..].iter().collect();
    EndpointSummary {
        id: endpoint.id.clone(),
        label: endpoint.label.clone(),
        base_url: endpoint.base_url.clone(),
        models: endpoint.models.clone(),
        small_model: endpoint.small_model.clone(),
        token_hint: tail,
    }
}

/// Lowercase letters, digits and dashes; `None` when nothing is left.
fn slug(label: &str) -> Option<String> {
    let mut slug = String::new();
    for c in label.trim().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-').to_owned();
    (!slug.is_empty()).then_some(slug)
}

fn validate(input: EndpointInput, previous_token: Option<String>) -> Result<Endpoint, String> {
    let label = input.label.trim().to_owned();
    let id = slug(&label).ok_or("give the endpoint a name")?;
    let base_url = input.base_url.trim().trim_end_matches('/').to_owned();
    let url = reqwest::Url::parse(&base_url).map_err(|_| "enter a valid base URL")?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if url.scheme() != "https" && !(url.scheme() == "http" && local) {
        return Err("the base URL must use https (http only for localhost)".into());
    }
    let token = input
        .token
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty())
        .or(previous_token)
        .ok_or("enter the API key")?;
    let model_name = |model: &str| {
        let model = model.trim();
        (!model.is_empty() && !model.chars().any(char::is_whitespace)).then(|| model.to_owned())
    };
    let mut models: Vec<String> = Vec::new();
    for model in input.models.iter().filter_map(|m| model_name(m.as_str())) {
        if !models.contains(&model) {
            models.push(model);
        }
    }
    if models.is_empty() {
        return Err("list at least one model".into());
    }
    Ok(Endpoint {
        id,
        label,
        base_url,
        token,
        models,
        small_model: input.small_model.as_deref().and_then(model_name),
    })
}

fn read(path: &Path) -> Vec<Endpoint> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<EndpointsFile>(&bytes)
            .map(|file| file.endpoints)
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "unreadable claude-endpoints.json; ignoring it");
                Vec::new()
            }),
        Err(_) => Vec::new(),
    }
}

/// Write through a 0600 staging file so a crash never leaves half a file
/// and the token is never readable by other users.
fn write(path: &Path, endpoints: &[Endpoint]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(&EndpointsFile {
        endpoints: endpoints.to_vec(),
    })?;
    let staging = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&staging)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&staging, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deepseek() -> Endpoint {
        Endpoint {
            id: "deepseek".into(),
            label: "DeepSeek".into(),
            base_url: "https://api.deepseek.com/anthropic".into(),
            token: "sk-secret-1234".into(),
            models: vec!["deepseek-v4-pro".into(), "deepseek-flash".into()],
            small_model: Some("deepseek-flash".into()),
        }
    }

    #[test]
    fn ids_round_trip_and_name_their_backend() {
        let id = model_id("deepseek", "deepseek-v4-pro");
        assert_eq!(id, "endpoint:deepseek:deepseek-v4-pro");
        assert_eq!(split(&id), Some(("deepseek", "deepseek-v4-pro")));
        assert_eq!(provider_model(&id), "deepseek-v4-pro");
        assert_eq!(provider_model("claude-opus-5-5"), "claude-opus-5-5");
        assert_eq!(backend(Some(&id)), Some("deepseek"));
        assert_eq!(backend(Some("opus")), None);
        assert_eq!(backend(None), None);
        assert_eq!(split("endpoint::model"), None);
    }

    #[test]
    fn launch_points_every_model_slot_at_the_endpoint() {
        let endpoints = [deepseek()];
        assert!(launch_env(Some("opus"), &endpoints).unwrap().is_none());
        let env = launch_env(Some("endpoint:deepseek:deepseek-v4-pro"), &endpoints)
            .unwrap()
            .unwrap();
        let get = |key| env.set.iter().find(|(k, _)| *k == key).map(|(_, v)| v.as_str());
        assert_eq!(get("ANTHROPIC_BASE_URL"), Some("https://api.deepseek.com/anthropic"));
        assert_eq!(get("ANTHROPIC_AUTH_TOKEN"), Some("sk-secret-1234"));
        assert_eq!(get("ANTHROPIC_DEFAULT_OPUS_MODEL"), Some("deepseek-v4-pro"));
        assert_eq!(get("ANTHROPIC_DEFAULT_HAIKU_MODEL"), Some("deepseek-flash"));
        assert!(env.remove.contains(&"ANTHROPIC_API_KEY"));
        assert!(launch_env(Some("endpoint:gone:model"), &endpoints).is_err());
    }

    #[test]
    fn saves_validate_and_keep_the_token_hidden() {
        let input = |token: Option<&str>| EndpointInput {
            label: " DeepSeek ".into(),
            base_url: "https://api.deepseek.com/anthropic/".into(),
            token: token.map(str::to_owned),
            models: vec!["deepseek-v4-pro".into(), " ".into(), "deepseek-v4-pro".into()],
            small_model: Some("".into()),
            replaces: None,
        };
        assert!(validate(input(None), None).is_err());
        let endpoint = validate(input(Some("sk-new-abcd")), None).unwrap();
        assert_eq!(endpoint.id, "deepseek");
        assert_eq!(endpoint.base_url, "https://api.deepseek.com/anthropic");
        assert_eq!(endpoint.models, ["deepseek-v4-pro"]);
        assert_eq!(endpoint.small_model, None);
        assert_eq!(summary(&endpoint).token_hint, "abcd");
        // An empty token on an existing endpoint keeps the saved one.
        let kept = validate(input(Some("")), Some("sk-old".into())).unwrap();
        assert_eq!(kept.token, "sk-old");
        let mut insecure = input(Some("k"));
        insecure.base_url = "http://api.example.com".into();
        assert!(validate(insecure, None).is_err());
        let mut local = input(Some("k"));
        local.base_url = "http://localhost:4000".into();
        assert!(validate(local, None).is_ok());
    }

    #[test]
    fn file_round_trips_privately() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        write(&path, &[deepseek()]).unwrap();
        assert_eq!(read(&path), [deepseek()]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(&path, "invalid").unwrap();
        assert!(read(&path).is_empty());
        let rows = models(&[deepseek()]);
        assert_eq!(rows[0].id, "endpoint:deepseek:deepseek-v4-pro");
        assert_eq!(rows[0].description.as_deref(), Some("DeepSeek endpoint"));
    }
}
