//! 模型／思考档位切换的**纯命令构造**（T6 Slice A1）。
//!
//! 合同来源：`scripts/zcode-bridge-probe/selectionBridge.ts`（B4 纯合同层）与
//! `task-T6-brief.md`：
//!
//! - 目标模型必须在**当前会话 provider 目录**里，目标档位必须属于**该模型自己的**
//!   `thought_levels`——另一个模型的档位不得放行，空档位列表一律拒绝；
//! - 命令恰为 `{type:"switchModelConfig",payload:{provider,model,thought},baseRevision}`
//!   （`command.ts:254-258,315-340`）：provider 只取自会话选择视图，**绝不**由客户端
//!   提供；CAS 只带 `baseRevision`，**禁止** `baseLogEpoch`；
//! - 产品保留用户选择：不要求原 thought 可观测，也不在切换后恢复原值（恢复原值只是
//!   探针清理，不进入产品）；T3 负责补 commandId/clientId/sessionId/issuedAt 信封。

use serde_json::{json, Value};

use super::types::{codes, SessionView, ZResult, ZcodeError};

/// 官方模型配置切换命令类型（`command.ts:254`）。
pub const SWITCH_MODEL_CONFIG_TYPE: &str = "switchModelConfig";

/// 构造一次 `switchModelConfig` 命令（纯函数：零 IO、零帧、绝不触发恢复写）。
///
/// 错误码（冻结集内）：
/// - 选择视图缺失或 `revision` 缺失 → `not_ready`（投影尚无可用的 CAS 依据）；
/// - 未知模型／空档位列表／目标档位不属于该模型 → `invalid_action`（零帧）。
pub fn build_selection_command(view: &SessionView, model: &str, thought: &str) -> ZResult<Value> {
    let selection = view
        .selection
        .as_ref()
        .ok_or_else(|| ZcodeError::of(codes::NOT_READY))?;
    // CAS 依据：只认投影携带的最新 revision；缺失即无可信基线（零帧）。
    let revision = view
        .revision
        .ok_or_else(|| ZcodeError::of(codes::NOT_READY))?;
    // provider 只取自会话视图（本类型只含当前 provider 目录），绝不接受客户端指定。
    let choice = selection
        .models
        .iter()
        .find(|choice| choice.model_id == model)
        .ok_or_else(|| ZcodeError::of(codes::INVALID_ACTION))?;
    // 档位必须是**该模型自己**的合法显式值：空列表 = 宿主未宣告，一律拒绝；
    // 其他模型的档位不在此列表里，因而天然无法放行。
    if choice.thought_levels.is_empty()
        || !choice.thought_levels.iter().any(|level| level == thought)
    {
        return Err(ZcodeError::of(codes::INVALID_ACTION));
    }
    // 信封只含 type/payload/baseRevision；T3 负责补 commandId/clientId/sessionId/
    // issuedAt。绝不带 baseLogEpoch，也不回带恢复原档位的写帧。
    Ok(json!({
        "type": SWITCH_MODEL_CONFIG_TYPE,
        "payload": {
            "provider": selection.provider,
            "model": model,
            "thought": thought,
        },
        "baseRevision": revision,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::zcode_control::types::{
        CapabilityView, ModelChoice, SelectionView, SessionView,
    };

    const PROVIDER: &str = "provider-host";

    fn choice(model_id: &str, levels: &[&str]) -> ModelChoice {
        ModelChoice {
            model_id: model_id.to_owned(),
            thought_levels: levels.iter().map(|level| (*level).to_owned()).collect(),
        }
    }

    fn selection(model: &str, thought: Option<&str>, models: Vec<ModelChoice>) -> SelectionView {
        SelectionView {
            provider: PROVIDER.to_owned(),
            model: model.to_owned(),
            thought: thought.map(str::to_owned),
            models,
        }
    }

    fn view(revision: Option<u64>, selection: Option<SelectionView>) -> SessionView {
        SessionView {
            session_id: "sess_test".to_owned(),
            generation: 1,
            revision,
            ready: true,
            pending: Vec::new(),
            control: None,
            selection,
            capabilities: CapabilityView::default(),
        }
    }

    fn code_of(result: ZResult<Value>) -> String {
        result.expect_err("expected rejection").code
    }

    #[test]
    fn only_target_models_own_levels_are_allowed() {
        let view = view(
            Some(9),
            Some(selection(
                "model-a",
                Some("low"),
                vec![
                    choice("model-a", &["low", "high"]),
                    // 仅 model-b 支持的档位不得授权给 model-a。
                    choice("model-b", &["only-for-b"]),
                ],
            )),
        );

        assert_eq!(
            code_of(build_selection_command(&view, "model-a", "only-for-b")),
            codes::INVALID_ACTION
        );
        let envelope = build_selection_command(&view, "model-a", "low").expect("legal target");
        let payload = envelope.get("payload").expect("payload");
        assert_eq!(payload.get("provider"), Some(&Value::from(PROVIDER)));
        assert_eq!(payload.get("model"), Some(&Value::from("model-a")));
        assert_eq!(payload.get("thought"), Some(&Value::from("low")));
    }

    #[test]
    fn switching_is_cas_and_does_not_restore() {
        let view = view(
            Some(7),
            Some(selection(
                "model-a",
                Some("low"),
                vec![choice("model-a", &["low", "high"])],
            )),
        );

        let envelope = build_selection_command(&view, "model-a", "high").expect("switch");
        assert_eq!(
            envelope.get("type"),
            Some(&Value::from(SWITCH_MODEL_CONFIG_TYPE))
        );
        assert_eq!(envelope.get("baseRevision"), Some(&Value::from(7_u64)));
        assert!(envelope.get("baseRevision").is_some());
        assert!(envelope.get("baseLogEpoch").is_none());
        // 产品只有用户显式选择这一条写帧：不回带恢复原 thought 的命令。
        let restore_frames = ["restore", "previousThought", "originalThought"]
            .iter()
            .filter(|key| envelope.get(**key).is_some())
            .count();
        assert_eq!(restore_frames, 0);
    }

    #[test]
    fn no_original_thought_is_not_a_probe_cleanup_requirement() {
        let view = view(
            Some(3),
            Some(selection(
                "model-a",
                None,
                vec![choice("model-a", &["low", "high"])],
            )),
        );

        assert!(build_selection_command(&view, "model-a", "low").is_ok());
    }

    #[test]
    fn empty_levels_reject_any_thought() {
        let view = view(
            Some(3),
            Some(selection("model-a", None, vec![choice("model-a", &[])])),
        );

        assert_eq!(
            code_of(build_selection_command(&view, "model-a", "low")),
            codes::INVALID_ACTION
        );
        assert!(build_selection_command(&view, "model-a", "").is_err());
    }

    #[test]
    fn cross_provider_or_unknown_model_is_rejected() {
        // 视图只含当前 provider 目录；另一 provider 的模型名在此不可见。
        let view = view(
            Some(3),
            Some(selection(
                "model-a",
                Some("low"),
                vec![choice("model-a", &["low"])],
            )),
        );

        assert_eq!(
            code_of(build_selection_command(&view, "model-b", "low")),
            codes::INVALID_ACTION
        );
        // 同一目录内的已知模型仍可正常构造（拒绝不是因为整体失效）。
        assert!(build_selection_command(&view, "model-a", "low").is_ok());
    }

    #[test]
    fn unknown_thought_is_rejected() {
        let view = view(
            Some(3),
            Some(selection(
                "model-a",
                Some("low"),
                vec![choice("model-a", &["low", "high"])],
            )),
        );

        assert_eq!(
            code_of(build_selection_command(&view, "model-a", "max")),
            codes::INVALID_ACTION
        );
        assert!(build_selection_command(&view, "model-a", "high").is_ok());
    }

    #[test]
    fn missing_revision_is_not_ready() {
        let selection = selection("model-a", Some("low"), vec![choice("model-a", &["low"])]);
        let without_revision = view(None, Some(selection.clone()));

        assert_eq!(
            code_of(build_selection_command(&without_revision, "model-a", "low")),
            codes::NOT_READY
        );
        let without_selection = view(Some(3), None);
        assert_eq!(
            code_of(build_selection_command(
                &without_selection,
                "model-a",
                "low"
            )),
            codes::NOT_READY
        );
    }
}
