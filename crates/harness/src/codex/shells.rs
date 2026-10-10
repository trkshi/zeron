use serde_json::{Value, json};
use std::collections::HashSet;
use zeron_proto::ShellTaskStatus;

use crate::{HarnessError, jsonrpc::RpcClient, shells::ShellMonitor};

pub(super) fn observe(monitor: &ShellMonitor, method: &str, params: &Value) {
    if method == "item/commandExecution/outputDelta" {
        if let (Some(id), Some(delta)) = (
            params.get("itemId").and_then(Value::as_str),
            params.get("delta").and_then(Value::as_str),
        ) {
            monitor.append(id, delta);
        }
        return;
    }
    if !matches!(method, "item/started" | "item/completed") {
        return;
    }
    let Some(item) = params.get("item") else {
        return;
    };
    if !matches!(
        item.get("type").and_then(Value::as_str),
        Some("commandExecution" | "command_execution")
    ) {
        return;
    }
    let Some(id) = item.get("id").and_then(Value::as_str) else {
        return;
    };
    let command = item
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("Shell command");
    if method == "item/started" {
        monitor.start(id, command, false, false);
        return;
    }
    if !monitor.contains(id) {
        monitor.start(id, command, true, false);
    }
    if let Some(output) = item.get("aggregatedOutput").and_then(Value::as_str) {
        monitor.replace_output(id, output);
    }
    let exit = item.get("exitCode").and_then(Value::as_i64);
    let status = item.get("status").and_then(Value::as_str);
    if exit.is_some() || status == Some("failed") {
        monitor.finish(
            id,
            if exit == Some(0) && status != Some("failed") {
                ShellTaskStatus::Completed
            } else {
                ShellTaskStatus::Failed
            },
            exit,
        );
    } else if item.get("processId").and_then(Value::as_str).is_some() {
        // A yielded exec can complete its item while its process keeps running.
        monitor.yielded(id);
    } else {
        monitor.finish(
            id,
            if status == Some("declined") {
                ShellTaskStatus::Stopped
            } else {
                ShellTaskStatus::Completed
            },
            None,
        );
    }
}

pub(super) async fn inventory(
    client: RpcClient,
    thread_id: String,
) -> Result<Vec<Value>, HarnessError> {
    let mut tasks = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..4 {
        let result = client
            .request(
                "thread/backgroundTerminals/list",
                json!({"threadId":thread_id,"cursor":cursor,"limit":64}),
            )
            .await?;
        let Some(data) = result.get("data").and_then(Value::as_array) else {
            return Err(HarnessError::Protocol(
                "invalid background terminal inventory".into(),
            ));
        };
        if data
            .iter()
            .any(|task| task.get("itemId").and_then(Value::as_str).is_none())
            || tasks.len() + data.len() > 256
        {
            return Err(HarnessError::Protocol(
                "invalid background terminal inventory entries".into(),
            ));
        }
        tasks.extend(data.iter().cloned());
        cursor = result
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if cursor.is_none() {
            return Ok(tasks);
        }
    }
    Err(HarnessError::Protocol(
        "background terminal inventory is too large".into(),
    ))
}

pub(super) fn reconcile(monitor: &ShellMonitor, tasks: Vec<Value>) {
    let mut ids = HashSet::new();
    for task in tasks {
        let Some(id) = task.get("itemId").and_then(Value::as_str) else {
            continue;
        };
        let command = task
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("Background shell");
        monitor.start(id, command, true, true);
        ids.insert(id.to_owned());
    }
    monitor.reconcile(&ids, true);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yielded_process_remains_live_until_native_inventory_says_otherwise() {
        let monitor = ShellMonitor::default();
        observe(
            &monitor,
            "item/started",
            &json!({"item":{"type":"commandExecution","id":"cmd","command":"server"}}),
        );
        observe(
            &monitor,
            "item/commandExecution/outputDelta",
            &json!({"itemId":"cmd","delta":"listening"}),
        );
        observe(
            &monitor,
            "item/completed",
            &json!({"item":{"type":"commandExecution","id":"cmd","processId":"42","exitCode":null}}),
        );
        assert_eq!(monitor.snapshot().0[0].status, ShellTaskStatus::Unknown);
        reconcile(
            &monitor,
            vec![json!({"itemId":"cmd","command":"server","processId":"42"})],
        );
        assert_eq!(monitor.snapshot().0[0].status, ShellTaskStatus::Running);
        assert_eq!(monitor.output("cmd").unwrap().text, "listening");
        reconcile(&monitor, vec![]);
        assert_eq!(monitor.snapshot().0[0].status, ShellTaskStatus::Unknown);
        observe(
            &monitor,
            "item/completed",
            &json!({"item":{"type":"commandExecution","id":"cmd","exitCode":0,"aggregatedOutput":"listening\nbye"}}),
        );
        assert_eq!(monitor.snapshot().0[0].status, ShellTaskStatus::Completed);
        assert_eq!(monitor.output("cmd").unwrap().text, "listening\nbye");
    }
}
