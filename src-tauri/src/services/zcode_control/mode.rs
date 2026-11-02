//! 协作模式切换（R10）：纯构造 + 规范化 + 标签。
//!
//! 官方命令：`{type:"switchCollaborationMode", payload:{mode}, baseRevision}`。
//! 本模块零 I/O；写前新鲜视图与生效确认在 `CommandSender::execute_mode`。
use serde_json::{json, Value};

use super::types::{codes, CollaborationMode, ModeView, SessionView, ZResult, ZcodeError};

/// 由 `config.mode` + `planEnabled` 规范化生效模式。
///
/// G0 证据：`planEnabled=true` 优先映射 Plan；`auto` 输出 build；缺省/未知保持 None
/// （不把 None 当 false，也不猜更宽模式）。
pub fn normalize_mode(raw: Option<&str>, plan_enabled: Option<bool>) -> Option<CollaborationMode> {
    if plan_enabled == Some(true) {
        return Some(CollaborationMode::Plan);
    }
    match raw {
        Some("plan") => Some(CollaborationMode::Plan),
        Some("build") | Some("auto") => Some(CollaborationMode::Build),
        Some("edit") => Some(CollaborationMode::Edit),
        Some("guarded") => Some(CollaborationMode::Guarded),
        Some("yolo") => Some(CollaborationMode::Yolo),
        _ => None,
    }
}

/// 解析 config 的 mode 形状；mode 字段缺失 = 未宣告（None）。
pub fn parse_mode_view(config: &Value) -> Option<ModeView> {
    let object = config.as_object()?;
    let raw_mode = object
        .get("mode")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .and_then(|text| match text {
            "plan" => Some(CollaborationMode::Plan),
            "build" => Some(CollaborationMode::Build),
            "edit" => Some(CollaborationMode::Edit),
            "guarded" => Some(CollaborationMode::Guarded),
            "yolo" => Some(CollaborationMode::Yolo),
            _ => None,
        });
    if raw_mode.is_none() && !object.contains_key("planEnabled") {
        return None;
    }
    let plan_enabled = object.get("planEnabled").and_then(Value::as_bool);
    let effective_mode = normalize_mode(object.get("mode").and_then(Value::as_str), plan_enabled);
    Some(ModeView {
        raw_mode,
        plan_enabled,
        effective_mode,
    })
}

/// 命令构造：payload **只**含 `{mode}`，带 baseRevision；扩展权限必须显式确认。
pub fn build_mode_command(
    view: &SessionView,
    mode: CollaborationMode,
    confirm_full_access: bool,
) -> ZResult<Value> {
    let revision = view
        .revision
        .ok_or_else(|| ZcodeError::of(codes::NOT_READY))?;
    if !mode.writable() {
        return Err(ZcodeError::of(codes::INVALID_ACTION));
    }
    if mode == CollaborationMode::Yolo && !confirm_full_access {
        return Err(ZcodeError::of(codes::INVALID_ACTION));
    }
    Ok(json!({
        "type": "switchCollaborationMode",
        "payload": { "mode": mode },
        "baseRevision": revision,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::zcode_control::types::{CapabilityView, SelectionView};

    fn view(revision: Option<u64>, mode: Option<ModeView>) -> SessionView {
        SessionView {
            session_id: "s".into(),
            generation: 1,
            revision,
            ready: true,
            pending: vec![],
            control: Some(crate::services::zcode_control::types::ControlView {
                active_works: vec![],
            }),
            selection: Some(SelectionView {
                provider: "p".into(),
                model: "m".into(),
                thought: None,
                models: vec![],
                providers: vec![],
                mode,
            }),
            capabilities: CapabilityView::default(),
            queue: Default::default(),
            execution_terminal: None,
            input_routing: None,
        }
    }

    #[test]
    fn mode_mapping_matches_installed_labels() {
        assert_eq!(CollaborationMode::Build.ui_label(), "变更前确认");
        assert_eq!(CollaborationMode::Guarded.ui_label(), "自主模式");
        assert_eq!(CollaborationMode::Plan.ui_label(), "计划模式");
        assert_eq!(CollaborationMode::Yolo.ui_label(), "完全访问");
        assert_eq!(CollaborationMode::Edit.ui_label(), "自动编辑");
    }

    #[test]
    fn plan_enabled_prevents_false_build_confirmation() {
        assert_eq!(
            normalize_mode(Some("build"), Some(true)),
            Some(CollaborationMode::Plan)
        );
        assert_eq!(
            normalize_mode(Some("plan"), Some(false)),
            Some(CollaborationMode::Plan)
        );
        assert_eq!(
            normalize_mode(Some("auto"), None),
            Some(CollaborationMode::Build)
        );
        assert_eq!(normalize_mode(Some("future"), None), None);
    }

    #[test]
    fn full_access_requires_explicit_confirmation() {
        assert!(build_mode_command(&view(Some(7), None), CollaborationMode::Yolo, false).is_err());
        let cmd = build_mode_command(&view(Some(7), None), CollaborationMode::Yolo, true).unwrap();
        assert_eq!(cmd["payload"], json!({"mode": "yolo"}));
        assert_eq!(cmd["baseRevision"], 7);
        assert!(build_mode_command(&view(Some(7), None), CollaborationMode::Edit, true).is_err());
    }
}
