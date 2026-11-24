//! 官方会话设置的受限读写。权限只派发经目录核实的 `/permission`。
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{
    actions::{PeerChannel, PreSubmitGate},
    error::{codes, DshError},
    model::{
        ActionPayload, ActionRequest, ActionState, CommandPayload, DeviceContext, NativeResult,
        Operation, SelectModelPayload, Target,
    },
    production,
    store::DshActionStore,
    transport::UnaryFailure,
    DshControlHub,
};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Selection {
    provider: String,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelProjection {
    next: Option<Selection>,
    last_used: Option<Selection>,
}

#[derive(Default, Deserialize)]
struct ModelCatalog {
    default: Option<Selection>,
    #[serde(default)]
    groups: Vec<ModelGroup>,
}

#[derive(Deserialize, Serialize)]
struct ModelGroup {
    id: String,
    name: String,
    models: Vec<ModelOption>,
}

#[derive(Deserialize, Serialize)]
struct ModelOption {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Reasoning>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Reasoning {
    efforts: Vec<Effort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_effort: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct Effort {
    id: String,
    name: String,
}

#[derive(Deserialize, Serialize)]
struct Preset {
    value: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

#[derive(Deserialize)]
struct PermissionCatalog {
    options: Vec<Preset>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PermissionSettings {
    current_value: String,
    options: Vec<Preset>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelSettings {
    next: Option<Selection>,
    last_used: Option<Selection>,
    default: Option<Selection>,
    groups: Vec<ModelGroup>,
}

#[derive(Serialize)]
pub struct SessionSettings {
    permission: Option<PermissionSettings>,
    model: ModelSettings,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettingsTarget {
    pub instance_id: String,
    pub session_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChangeRequest {
    pub instance_id: String,
    pub session_id: String,
    pub action_id: String,
    pub change: Change,
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Change {
    Permission {
        preset: String,
        acknowledged: bool,
    },
    Model {
        provider: String,
        model: String,
        reasoning_effort: Option<String>,
        for_current_model: Option<Selection>,
    },
}

impl Change {
    fn operation(&self) -> Operation {
        match self {
            Self::Permission { .. } => Operation::CommandExecute,
            Self::Model {
                reasoning_effort: Some(_),
                ..
            } => Operation::ReasoningSelect,
            Self::Model { .. } => Operation::ModelSelect,
        }
    }

    fn payload(&self) -> ActionPayload {
        match self {
            Self::Permission { preset, .. } => ActionPayload::Command(CommandPayload {
                name: "permission".into(),
                args: [("preset".into(), json!(preset))].into(),
                receipt_ids: Vec::new(),
            }),
            Self::Model {
                provider,
                model,
                reasoning_effort,
                ..
            } => ActionPayload::SelectModel(SelectModelPayload {
                provider: provider.clone(),
                model: model.clone(),
                reasoning_effort: reasoning_effort.clone(),
            }),
        }
    }

    fn rpc(
        &self,
        target: &Target,
        settings: &SessionSettings,
    ) -> Result<(&'static str, Value), DshError> {
        match self {
            Self::Permission {
                preset,
                acknowledged,
            } if *acknowledged
                && valid_preset(preset)
                && settings.permission.as_ref().is_some_and(|permission| {
                    permission
                        .options
                        .iter()
                        .any(|option| &option.value == preset)
                }) =>
            {
                Ok((
                    "commands/execute",
                    json!({"agentId":target.session_id,"line":format!("/permission {preset}"),"submittedAttachments":[]}),
                ))
            }
            Self::Model {
                provider,
                model,
                reasoning_effort,
                for_current_model,
            } if (reasoning_effort.is_none() || for_current_model.is_some())
                && for_current_model.as_ref().is_none_or(|expected| {
                    expected.provider == *provider
                        && expected.model == *model
                        && settings
                            .model
                            .next
                            .as_ref()
                            .or(settings.model.default.as_ref())
                            == Some(expected)
                })
                && settings.model.groups.iter().any(|group| {
                    &group.id == provider
                        && group.models.iter().any(|option| {
                            &option.id == model
                                && reasoning_effort.as_ref().is_none_or(|effort| {
                                    option.reasoning.as_ref().is_some_and(|reasoning| {
                                        reasoning.efforts.iter().any(|entry| &entry.id == effort)
                                    })
                                })
                        })
                }) =>
            {
                let mut request =
                    json!({"sessionId":target.session_id,"provider":provider,"model":model});
                if let Some(effort) = reasoning_effort {
                    request["reasoningEffort"] = json!(effort);
                }
                Ok(("session/selectModel", json!({"request":request})))
            }
            _ => Err(DshError::new(
                codes::ACTION_INVALID,
                "selection or confirmation no longer valid",
            )),
        }
    }

    fn confirmed(&self, projection: &Value) -> bool {
        match self {
            Self::Permission { preset, .. } => {
                current_permission(projection) == Some(preset.as_str())
            }
            Self::Model {
                provider,
                model,
                reasoning_effort,
                ..
            } => projection
                .pointer("/values/modelSelection/next")
                .and_then(|value| serde_json::from_value::<Selection>(value.clone()).ok())
                .is_some_and(|value| {
                    value.provider == *provider
                        && value.model == *model
                        && value.reasoning_effort == *reasoning_effort
                }),
        }
    }
}

fn valid_preset(value: &str) -> bool {
    !value.is_empty()
        && value != "custom"
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn current_permission(projection: &Value) -> Option<&str> {
    projection
        .pointer("/values/permissions/currentValue")?
        .as_str()
}

fn display_settings(
    projection: Value,
    permissions: Option<Value>,
    models: Option<Value>,
) -> SessionSettings {
    let model = projection
        .pointer("/values/modelSelection")
        .and_then(|value| serde_json::from_value::<ModelProjection>(value.clone()).ok())
        .unwrap_or_default();
    let catalog = models
        .and_then(|value| serde_json::from_value::<ModelCatalog>(value).ok())
        .unwrap_or_default();
    let permission = current_permission(&projection)
        .filter(|value| !value.is_empty())
        .map(|value| PermissionSettings {
            current_value: value.to_owned(),
            options: permissions
                .and_then(|value| serde_json::from_value::<PermissionCatalog>(value).ok())
                .map(|catalog| {
                    catalog
                        .options
                        .into_iter()
                        .filter(|option| valid_preset(&option.value))
                        .collect()
                })
                .unwrap_or_default(),
        });
    SessionSettings {
        permission,
        model: ModelSettings {
            next: model.next,
            last_used: model.last_used,
            default: catalog.default,
            groups: catalog.groups,
        },
    }
}

async fn read(
    channel: &PeerChannel,
    target: &Target,
    method: &str,
    args: Value,
) -> Result<Value, DshError> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let rpc_id = format!(
        "dsh-settings-{}",
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    match channel
        .request_classified(&rpc_id, method, &args, Some(target))
        .await?
    {
        Ok(response) => match response.result {
            NativeResult::Success { value } => Ok(value),
            NativeResult::Failure { .. } => Err(DshError::new(
                codes::CAPABILITY_DENIED,
                "host refused settings read",
            )),
        },
        Err(error) => Err(error.into_error()),
    }
}

async fn read_projection(channel: &PeerChannel, target: &Target) -> Result<Value, DshError> {
    let value = read(
        channel,
        target,
        "session/projections",
        json!({"request":{"sessionId":target.session_id}}),
    )
    .await?;
    if value.get("kind").and_then(Value::as_str) != Some("sequenced") {
        return Err(DshError::new(
            codes::ACTION_STATE_CONFLICT,
            "settings projection is not current",
        ));
    }
    Ok(value)
}

async fn read_settings(
    channel: &PeerChannel,
    target: &Target,
    include_catalogs: bool,
) -> Result<SessionSettings, DshError> {
    let projection = read_projection(channel, target).await?;
    let permissions = if include_catalogs
        && projection
            .pointer("/values/permissions/currentValue")
            .is_some()
    {
        read(channel, target, "permissionPresets/catalog", json!({}))
            .await
            .ok()
    } else {
        None
    };
    let models = if include_catalogs
        || projection
            .pointer("/values/modelSelection/next")
            .is_some_and(Value::is_null)
    {
        read(channel, target, "session/modelCatalog", json!({}))
            .await
            .ok()
    } else {
        None
    };
    let mut settings = display_settings(projection, permissions, models);
    if !include_catalogs {
        settings.model.groups.clear();
    }
    Ok(settings)
}

#[derive(Serialize)]
pub struct ChangeResult {
    pub state: ActionState,
    pub confirmed: bool,
}

// 串行化同一宿主会话的设置读前置、派发与读回。其他会话保持独立。
fn change_lane(target: &Target) -> Arc<tokio::sync::Mutex<()>> {
    type Lane = tokio::sync::Mutex<()>;
    type WeakLanes = std::collections::HashMap<(String, String), std::sync::Weak<Lane>>;
    static LANES: std::sync::LazyLock<std::sync::Mutex<WeakLanes>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(WeakLanes::new()));
    let mut lanes = LANES.lock().unwrap_or_else(|error| error.into_inner());
    let key = (target.host_id.clone(), target.session_id.clone());
    if let Some(lane) = lanes.get(&key).and_then(std::sync::Weak::upgrade) {
        return lane;
    }
    lanes.retain(|_, lane| lane.strong_count() > 0);
    let lane = Arc::new(Lane::new(()));
    lanes.insert(key, Arc::downgrade(&lane));
    lane
}

impl DshControlHub {
    pub async fn dsh_session_settings(
        self: &Arc<Self>,
        target: SettingsTarget,
        catalogs: bool,
    ) -> Result<SessionSettings, DshError> {
        let hub = Arc::clone(self);
        let target = tokio::task::spawn_blocking(move || {
            hub.check_switch_gate(&target.instance_id)?;
            hub.resolve_owned_session(&target.instance_id, &target.session_id)
        })
        .await
        .map_err(|_| DshError::new(codes::STATE_IO, "settings target resolution failed"))??;
        let channel = PeerChannel::for_instance(target.host_id.clone());
        read_settings(&channel, &target, catalogs).await
    }

    pub async fn change_dsh_session_settings(
        self: &Arc<Self>,
        device: DeviceContext,
        devices: crate::remote::pairing::DeviceStore,
        req: ChangeRequest,
    ) -> Result<ChangeResult, DshError> {
        if !self.write_gate_open() {
            return Err(DshError::new(
                codes::ACTION_WRITE_GATED,
                "dsh write gate is closed",
            ));
        }
        let hub = Arc::clone(self);
        let db = self.db("session settings")?.clone();
        let admission = production::capture_send_acceptance(&req.instance_id, None);
        tokio::spawn(async move {
            let resolver = Arc::clone(&hub);
            let instance = req.instance_id.clone();
            let session = req.session_id.clone();
            let operation = req.change.operation();
            let (target, root) = tokio::task::spawn_blocking(move || {
                resolver.check_switch_gate(&instance)?;
                let target = resolver.resolve_owned_session(&instance, &session)?;
                let host = resolver
                    .host_probe
                    .as_ref()
                    .ok_or_else(|| Self::unsupported("settings"))?()?;
                if target.host_id != host.host_id || target.epoch != host.epoch {
                    return Err(DshError::new(
                        codes::INSTANCE_SWITCHED,
                        "settings target changed",
                    ));
                }
                if operation == Operation::ReasoningSelect
                    && !resolver
                        .evaluate_operation_gate(
                            &host,
                            production::dsh_auto_adapt_enabled(),
                            Operation::ModelSelect,
                        )?
                        .allowed
                {
                    return Err(DshError::new(
                        codes::CAPABILITY_DENIED,
                        "model selection contract unavailable",
                    ));
                }
                if !resolver
                    .evaluate_operation_gate(
                        &host,
                        production::dsh_auto_adapt_enabled(),
                        operation,
                    )?
                    .allowed
                {
                    return Err(DshError::new(
                        codes::CAPABILITY_DENIED,
                        "settings contract unavailable",
                    ));
                }
                let root = resolver.resolve_readonly_root(&instance, &session)?;
                Ok::<_, DshError>((target, root))
            })
            .await
            .map_err(|_| DshError::new(codes::STATE_IO, "settings target resolution failed"))??;
            let lane = change_lane(&target);
            let _guard = lane.lock().await;
            let channel = PeerChannel::for_instance(target.host_id.clone());
            let action = ActionRequest {
                target: target.clone(),
                action_id: req.action_id.clone(),
                expected_revision: None,
                payload: req.change.payload(),
            };
            let payload = serde_json::to_string(&action.payload).map_err(|_| {
                DshError::new(codes::STATE_IO, "settings action identity unavailable")
            })?;
            let identity = super::store::payload_identity_digest(&payload);
            let replay_device = device.clone();
            let replay_target = target.clone();
            let replay_id = req.action_id.clone();
            let replay = Self::ledger(db.clone(), move |conn| {
                DshActionStore::new(conn).existing_action_for_instance(
                    &replay_device,
                    &replay_id,
                    &replay_target.session_id,
                    &identity,
                    &payload,
                    Some(&replay_target.host_id),
                )
            })
            .await?;
            if let Some((receipt, _)) = replay {
                let confirmed = matches!(
                    receipt.state,
                    ActionState::Accepted | ActionState::Succeeded | ActionState::UnknownDelivery
                ) && read_projection(&channel, &target)
                    .await
                    .is_ok_and(|projection| req.change.confirmed(&projection));
                return Ok(ChangeResult {
                    state: receipt.state,
                    confirmed,
                });
            }
            let settings = read_settings(&channel, &target, true).await?;
            let (method, args) = req.change.rpc(&target, &settings)?;
            let device_for_ledger = device.clone();
            let id = action.action_id.clone();
            let reservation = Self::ledger(db.clone(), move |conn| {
                let store = DshActionStore::new(conn);
                store.register(&device_for_ledger, &action)?;
                let claim_id = format!("dsh-settings:{id}");
                match store.reserve_for_dispatch_for(
                    &device_for_ledger,
                    &id,
                    &claim_id,
                    action.target.epoch,
                    &device_for_ledger.device_id,
                    &claim_id,
                    0,
                )? {
                    Some(_) => Ok(None),
                    None => {
                        let state = store.view(&device_for_ledger, &id)?.receipt.state;
                        if state == ActionState::Queued {
                            store.fail_queued_for(
                                &device_for_ledger,
                                &id,
                                codes::ACTION_STATE_CONFLICT,
                            )?;
                            Ok(Some(ActionState::Failed))
                        } else {
                            Ok(Some(state))
                        }
                    }
                }
            })
            .await?;
            let state = if let Some(state) = reservation {
                state
            } else {
                let checker = Arc::clone(&hub);
                let before_write: PreSubmitGate = Arc::new(move |target| {
                    let root = root.clone();
                    let checker = Arc::clone(&checker);
                    Box::pin(async move {
                        tokio::task::spawn_blocking(move || {
                            if checker.write_gate_open()
                                && checker.check_switch_gate(&target.host_id).is_ok()
                                && matches!(
                                    crate::remote::content::dsh_open_turn(
                                        &root,
                                        &target.session_id
                                    ),
                                    Ok(None)
                                )
                            {
                                return Ok(());
                            }
                            Err(DshError::new(
                                codes::ACTION_STATE_CONFLICT,
                                "the session is active or unavailable",
                            ))
                        })
                        .await
                        .map_err(|_| {
                            DshError::new(codes::STATE_IO, "settings pre-write check failed")
                        })?
                    })
                });
                let outcome = channel
                    .request_classified_checked(
                        &req.action_id,
                        method,
                        &args,
                        Some((&target, &admission)),
                        Some(before_write),
                        Some((&devices, device.device_id.as_str())),
                    )
                    .await;
                let (state, failure) = match outcome {
                    Ok(Ok(response)) => match response.result {
                        NativeResult::Success { value }
                            if (method == "commands/execute"
                                && value.pointer("/result/kind").and_then(Value::as_str)
                                    == Some("success"))
                                || (method == "session/selectModel"
                                    && value.get("selected").is_some_and(Value::is_object)) =>
                        {
                            (ActionState::Accepted, None)
                        }
                        _ => (ActionState::Failed, Some(codes::CAPABILITY_DENIED)),
                    },
                    Ok(Err(UnaryFailure::DeliveryUnknown(_))) => {
                        (ActionState::UnknownDelivery, None)
                    }
                    Err(error) | Ok(Err(UnaryFailure::NotExecuted(error))) => {
                        (ActionState::Failed, Some(error.code))
                    }
                };
                let id = req.action_id.clone();
                let settlement_device = device.clone();
                Self::ledger(db.clone(), move |conn| {
                    let store = DshActionStore::new(conn);
                    match state {
                        ActionState::Accepted => {
                            store.mark_accepted_for(&settlement_device, &id, None)
                        }
                        ActionState::UnknownDelivery => store.mark_unknown_delivery_for(
                            &settlement_device,
                            &id,
                            "delivery_unknown",
                        ),
                        _ => store.mark_failed_for(
                            &settlement_device,
                            &id,
                            failure.unwrap_or(codes::STATE_IO),
                        ),
                    }
                    .map(|_| ())
                })
                .await?;
                state
            };
            let confirmed = matches!(
                state,
                ActionState::Accepted | ActionState::Succeeded | ActionState::UnknownDelivery
            ) && read_projection(&channel, &target)
                .await
                .is_ok_and(|projection| req.change.confirmed(&projection));
            let state = if confirmed
                && matches!(state, ActionState::Accepted | ActionState::UnknownDelivery)
            {
                Self::ledger(db, move |conn| {
                    DshActionStore::new(conn).mark_settings_readback_for(&device, &req.action_id)
                })
                .await?
            } else {
                state
            };
            Ok(ChangeResult { state, confirmed })
        })
        .await
        .map_err(|_| DshError::new(codes::STATE_IO, "settings change task failed"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target {
            host_id: "host".into(),
            epoch: 1,
            session_id: "session-test".into(),
        }
    }

    fn view() -> SessionSettings {
        display_settings(
            json!({"values":{
                "permissions":{"currentValue":"workspace-write"},
                "modelSelection":{"next":{"provider":"p","model":"m2"},"lastUsed":{"provider":"p","model":"m1","reasoningEffort":"low"}}
            }}),
            Some(
                json!({"options":[{"value":"workspace-write","name":"Workspace"},{"value":"danger-full-access","name":"Full"},{"value":"custom","name":"Custom"},{"value":"x\n/shell","name":"Invalid"}]}),
            ),
            Some(
                json!({"groups":[{"id":"p","name":"P","models":[{"id":"m2","name":"M2","reasoning":{"efforts":[{"id":"high","name":"High"}]}}]}]}),
            ),
        )
    }

    #[test]
    fn current_and_last_used_are_distinct_and_missing_effort_stays_missing() {
        let payload = serde_json::to_value(view()).unwrap();
        assert_eq!(payload.pointer("/model/next/model").unwrap(), "m2");
        assert!(payload.pointer("/model/next/reasoningEffort").is_none());
        assert_eq!(
            payload.pointer("/model/lastUsed/reasoningEffort").unwrap(),
            "low"
        );
        assert_eq!(
            payload
                .pointer("/permission/options")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn validates_catalog_and_confirmation_before_building_native_request() {
        let change = |acknowledged| Change::Permission {
            preset: "danger-full-access".into(),
            acknowledged,
        };
        assert!(change(false).rpc(&target(), &view()).is_err());
        assert_eq!(
            change(true).rpc(&target(), &view()).unwrap(),
            (
                "commands/execute",
                json!({"agentId":"session-test","line":"/permission danger-full-access","submittedAttachments":[]})
            )
        );
        assert!(Change::Permission {
            preset: "x\n/shell".into(),
            acknowledged: true
        }
        .rpc(&target(), &view())
        .is_err());
        let model = |effort: &str| Change::Model {
            provider: "p".into(),
            model: "m2".into(),
            reasoning_effort: Some(effort.into()),
            for_current_model: Some(Selection {
                provider: "p".into(),
                model: "m2".into(),
                reasoning_effort: None,
            }),
        };
        assert!(model("low").rpc(&target(), &view()).is_err());
        assert_eq!(
            model("high")
                .rpc(&target(), &view())
                .unwrap()
                .1
                .pointer("/request/reasoningEffort")
                .unwrap(),
            "high"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires a local installed DSH app; reads only the official package"]
    fn installed_dsh_settings_contracts_are_recognized() {
        let host = super::super::model::LocalHost {
            host_id: "isolated-contract".into(),
            platform: "macos".into(),
            profile_dir: std::path::PathBuf::new(),
            executable: "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness".into(),
            pid: 1,
            started_at_ms: 1,
            epoch: 1,
            authority: "127.0.0.1:1".into(),
            package_sha256: String::new(),
        };
        let report = super::super::contract::read_report(
            &host,
            super::super::contract::CONTRACT_RULE_VERSION,
        )
        .unwrap();
        for operation in [
            Operation::ModelSelect,
            Operation::ReasoningSelect,
            Operation::CommandExecute,
        ] {
            assert!(
                report.evidence.matches.contains_key(&operation),
                "installed contract not recognized: {operation:?}, undecided: {:?}",
                report.undecided()
            );
        }
    }
}
