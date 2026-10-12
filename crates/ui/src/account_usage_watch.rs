//! One account snapshot stream for the shell's current host, shared by its views.

use std::time::Duration;

use gpui::{Context, Entity, Task};
use zeron_proto::AgentAccountsSnapshot;
use zeron_rpc::methods;

use crate::settings::accounts::AccountsSnapshotCache;
use crate::state::AppState;

pub(crate) fn host_refresh_supported(state: &AppState, target: &Option<String>) -> bool {
    target
        .as_deref()
        .or(state.local_device_id.as_deref())
        .is_some_and(|device| {
            state.device_supports(device, zeron_proto::capabilities::ACCOUNT_USAGE_REFRESH_V1)
        })
}

pub(crate) struct AccountUsageWatch {
    state: Entity<AppState>,
    target: Option<String>,
    online: bool,
    generation: u64,
    task: Option<Task<()>>,
}

impl AccountUsageWatch {
    pub(crate) fn new(state: Entity<AppState>) -> Self {
        Self {
            state,
            target: None,
            online: false,
            generation: 0,
            task: None,
        }
    }

    pub(crate) fn track(&mut self, device: Option<String>, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let target = device
            .clone()
            .filter(|id| Some(id) != state.local_device_id.as_ref());
        let online = matches!(state.connection, zeron_proto::view::ConnectionStatus::Ready)
            && state.engine().is_some()
            && host_refresh_supported(state, &target)
            && device
                .as_ref()
                .is_none_or(|id| state.device_online(id, chrono::Utc::now()));
        if self.target != target || self.online != online {
            self.target = target;
            self.online = online;
            self.generation = self.generation.wrapping_add(1);
            self.task = None;
        }
        if !online || self.task.is_some() {
            return;
        }
        let Some(engine) = state.engine().cloned() else {
            return;
        };
        let target = self.target.clone();
        let generation = self.generation;
        self.task = Some(cx.spawn(async move |this, cx| {
            loop {
                let request = engine.client().subscribe_checked(
                    methods::WATCH_AGENT_ACCOUNTS,
                    serde_json::json!({"targetDeviceId": target}),
                );
                let result = match futures::future::select(
                    Box::pin(request),
                    Box::pin(cx.background_executor().timer(Duration::from_secs(35))),
                )
                .await
                {
                    futures::future::Either::Left((result, _)) => result,
                    futures::future::Either::Right(_) => Err(zeron_rpc::RpcError::Failed(
                        "Account usage stream connection timed out".into(),
                    )),
                };
                if matches!(&result, Err(zeron_rpc::RpcError::UnknownMethod(_))) {
                    return;
                }
                if let Ok(mut stream) = result {
                    while let Some(value) = stream.recv().await {
                        let Ok(snapshot) = serde_json::from_value::<AgentAccountsSnapshot>(value)
                        else {
                            break;
                        };
                        if this
                            .update(cx, |watch, cx| {
                                if watch.generation == generation {
                                    cx.default_global::<AccountsSnapshotCache>()
                                        .0
                                        .insert(target.clone(), snapshot);
                                }
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                cx.background_executor().timer(Duration::from_secs(5)).await;
            }
        }));
    }
}
