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

use super::types::{codes, ModelChoice, SelectionView, SessionView, ZResult, ZcodeError};

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

/// 解析官方 `model-selection.getView` 目录（T6 Slice A2；纯函数：零 IO、零帧）。
///
/// `raw` 是调用方组装的会话选择载荷（Tuvis 内部形状，绝不原样下发手机）：
///
/// ```json
/// {
///   "config": {"provider": "...", "model": "...", "thought": "..."?, "thoughtLevels": ["..."]},
///   "providers": [{"providerId": "...", "models": [{"modelId": "...", "config": {
///     "optionSpecs": {"reasoningLevel": {"values": ["..."]}}}}]}]
/// }
/// ```
///
/// 规则：
/// - 只保留**当前会话 provider**（`config.provider`）的目录；其他账号 provider 一律丢弃；
/// - 档位逐模型取自 `config.optionSpecs.reasoningLevel.values`，**绝不**做 provider 级并集；
/// - `config.thoughtLevels` 只作**当前模型**的回退（目录未给该模型档位时），不喂其他模型；
/// - `providers` 缺失/为空/无当前 provider → 退化为「当前模型 + 快照档位」；
/// - 模型 id 与档位都去重（保留首次出现，不合并重复项的档位），字符串有长度与数量上界；
/// - 只对**命中当前 provider** 的条目做严格校验（缺失/形状不符 → `needs_evidence`）；
///   兄弟/其他账号 provider 的坏形状直接跳过，不据此丢弃当前会话目录；
/// - wire 顶层形状不符（raw 非对象、`providers` 非数组）→ `unsupported_protocol`，
///   绝不 panic、不猜。
///
/// 本函数是**纯解析**：会话/代际/revision 由调用方持有（CAS 基线另取 `SessionView`），
/// 不参与目录解析。
pub fn parse_selection_view(raw: &Value) -> ZResult<SelectionView> {
    if !raw.is_object() {
        return Err(unsupported());
    }
    let config = parse_session_config(raw)?;
    let models = catalog_models(raw, &config)?;
    Ok(SelectionView {
        provider: config.provider.to_owned(),
        model: config.model.to_owned(),
        thought: config.thought.map(str::to_owned),
        models,
    })
}

/// 目录最多扫描的 provider / 模型 / 档位数量与 id 长度上界（有界，防巨帧撑爆内存）。
const MAX_PROVIDERS: usize = 16;
const MAX_MODELS: usize = 64;
const MAX_LEVELS: usize = 16;
const MAX_ID_CHARS: usize = 128;
const MAX_LEVEL_CHARS: usize = 64;

fn needs_evidence() -> ZcodeError {
    ZcodeError::of(codes::NEEDS_EVIDENCE)
}

fn unsupported() -> ZcodeError {
    ZcodeError::of(codes::UNSUPPORTED_PROTOCOL)
}

/// 会话快照 `config` 的只读解析结果（权威当前值；provider/model 必填）。
struct SessionConfig<'a> {
    provider: &'a str,
    model: &'a str,
    thought: Option<&'a str>,
    thought_levels: Vec<String>,
}

fn parse_session_config(raw: &Value) -> ZResult<SessionConfig<'_>> {
    let config = raw
        .get("config")
        .and_then(Value::as_object)
        .ok_or_else(needs_evidence)?;
    let provider = non_empty_id(config.get("provider")).ok_or_else(needs_evidence)?;
    let model = non_empty_id(config.get("model")).ok_or_else(needs_evidence)?;
    // 官方空串 = 无显式档位；非字符串同样按「无」处理（默认命令路径仍要求显式值）。
    let thought = config
        .get("thought")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty());
    Ok(SessionConfig {
        provider,
        model,
        thought,
        thought_levels: level_ids(config.get("thoughtLevels")),
    })
}

/// 非空且不超长的 id 字符串（只认 wire id，不认显示名）。
fn non_empty_id(value: Option<&Value>) -> Option<&str> {
    let text = value?.as_str()?;
    if text.is_empty() || text.chars().count() > MAX_ID_CHARS {
        return None;
    }
    Some(text)
}

/// 档位 id 列表：只收非空短字符串，去重（保留首次出现），有界。
fn level_ids(value: Option<&Value>) -> Vec<String> {
    let mut levels: Vec<String> = Vec::new();
    let Some(items) = value.and_then(Value::as_array) else {
        return levels;
    };
    for item in items {
        let Some(level) = item
            .as_str()
            .filter(|text| !text.is_empty() && text.chars().count() <= MAX_LEVEL_CHARS)
        else {
            continue;
        };
        if !levels.iter().any(|seen| seen == level) {
            levels.push(level.to_owned());
        }
        if levels.len() >= MAX_LEVELS {
            break;
        }
    }
    levels
}

/// 定位 getView 的 `providers`：官方 channel 应答可能带 `data`/`result` 包装
/// （与探针 `resolveCatalogFromView` 同规则，最多解 3 层）。
fn providers_value(raw: &Value) -> Option<&Value> {
    let mut current = raw;
    for _ in 0..3 {
        if let Some(providers) = current.get("providers") {
            return Some(providers);
        }
        let next = current
            .get("data")
            .filter(|value| value.is_object())
            .or_else(|| current.get("result").filter(|value| value.is_object()))?;
        current = next;
    }
    current.get("providers")
}

/// 目录不可用（缺失/空/无当前 provider）时的退化目录：只含当前模型 + 快照档位。
fn fallback_models(config: &SessionConfig<'_>) -> Vec<ModelChoice> {
    vec![ModelChoice {
        model_id: config.model.to_owned(),
        thought_levels: config.thought_levels.clone(),
    }]
}

fn catalog_models(raw: &Value, config: &SessionConfig<'_>) -> ZResult<Vec<ModelChoice>> {
    let Some(providers) = providers_value(raw) else {
        return Ok(fallback_models(config));
    };
    let providers = providers.as_array().ok_or_else(unsupported)?;
    if providers.is_empty() {
        return Ok(fallback_models(config));
    }
    // 只对**命中当前 provider** 的条目做严格校验：兄弟/其他账号 provider 的坏形状
    // 不得否决当前会话可用的目录（它们本来就会被丢弃）。
    let mut current_models: Option<&[Value]> = None;
    for provider in providers.iter().take(MAX_PROVIDERS) {
        let Some(record) = provider.as_object() else {
            continue;
        };
        // 官方 ModelSelectionView：providers[].providerId / models[].modelId。
        let Some(provider_id) = non_empty_id(record.get("providerId")) else {
            continue;
        };
        if provider_id != config.provider {
            continue;
        }
        let models = record
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(needs_evidence)?;
        current_models = Some(models);
    }
    let Some(models) = current_models else {
        // 目录里没有当前会话 provider（可能只暴露了其他账号）：退化，绝不借用别家目录。
        return Ok(fallback_models(config));
    };
    if models.is_empty() {
        return Ok(fallback_models(config));
    }
    let mut choices: Vec<ModelChoice> = Vec::new();
    for model in models.iter().take(MAX_MODELS) {
        let record = model.as_object().ok_or_else(needs_evidence)?;
        let model_id = non_empty_id(record.get("modelId")).ok_or_else(needs_evidence)?;
        let declared = level_ids(
            record
                .get("config")
                .and_then(|config| config.get("optionSpecs"))
                .and_then(|specs| specs.get("reasoningLevel"))
                .and_then(|level| level.get("values")),
        );
        // 快照档位只回退给**当前模型**；其他模型保持空列表（命令构造会拒绝其任何档位）。
        let thought_levels = if declared.is_empty() && model_id == config.model {
            config.thought_levels.clone()
        } else {
            declared
        };
        // 重复 id 保留首次出现，且不合并重复项的档位（合并即 provider 级并集）。
        if choices.iter().any(|choice| choice.model_id == model_id) {
            continue;
        }
        choices.push(ModelChoice {
            model_id: model_id.to_owned(),
            thought_levels,
        });
    }
    Ok(choices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::zcode_control::types::{
        CapabilityView, ModelChoice, QueueView, SelectionView, SessionView,
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
            queue: QueueView::default(),
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
        // 信封键集合恰为 {type,payload,baseRevision}（不是「找不到 restore 键」的空断言）：
        // 任何恢复原值的附加字段都会破坏这个集合。
        let mut keys: Vec<&str> = envelope
            .as_object()
            .expect("envelope object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["baseRevision", "payload", "type"]);
        let mut payload_keys: Vec<&str> = envelope["payload"]
            .as_object()
            .expect("payload object")
            .keys()
            .map(String::as_str)
            .collect();
        payload_keys.sort_unstable();
        assert_eq!(payload_keys, ["model", "provider", "thought"]);
        // 目标 thought=high，原值 low 绝不回写。
        assert_eq!(envelope["payload"]["thought"], Value::from("high"));
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

    // -----------------------------------------------------------------------
    // T6 Slice A2：官方 model-selection.getView 目录解析
    // -----------------------------------------------------------------------

    const CURRENT_PROVIDER: &str = "acct-current";

    fn model_entry(model_id: &str, levels: Option<&[&str]>) -> Value {
        let mut entry = json!({ "modelId": model_id });
        if let Some(levels) = levels {
            entry["config"] = json!({ "optionSpecs": { "reasoningLevel": { "values": levels } } });
        }
        entry
    }

    fn session_config(model: &str, thought: Option<&str>, levels: &[&str]) -> Value {
        json!({
            "provider": CURRENT_PROVIDER,
            "model": model,
            "thought": thought,
            "thoughtLevels": levels,
        })
    }

    fn raw_view(config: Value, providers: Option<Value>) -> Value {
        let mut raw = json!({ "config": config });
        if let Some(providers) = providers {
            raw["providers"] = providers;
        }
        raw
    }

    fn parse(raw: &Value) -> ZResult<SelectionView> {
        parse_selection_view(raw)
    }

    fn levels_of(view: &SelectionView, model_id: &str) -> Vec<String> {
        view.models
            .iter()
            .find(|choice| choice.model_id == model_id)
            .map(|choice| choice.thought_levels.clone())
            .unwrap_or_default()
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parse_selection_view_keeps_only_current_provider() {
        let raw = raw_view(
            session_config("model-a", Some("low"), &["low"]),
            Some(json!([
                {
                    "providerId": "acct-other-account",
                    "models": [model_entry("other-1", Some(&["x"]))],
                },
                {
                    "providerId": CURRENT_PROVIDER,
                    "models": [
                        model_entry("model-a", Some(&["low", "high"])),
                        model_entry("model-b", Some(&["low"])),
                    ],
                },
            ])),
        );

        let view = parse(&raw).expect("catalog");
        assert_eq!(view.provider, CURRENT_PROVIDER);
        assert_eq!(view.model, "model-a");
        assert_eq!(view.thought.as_deref(), Some("low"));
        let ids: Vec<&str> = view
            .models
            .iter()
            .map(|choice| choice.model_id.as_str())
            .collect();
        assert_eq!(ids, ["model-a", "model-b"]);
        assert!(!ids.contains(&"other-1"));
    }

    #[test]
    fn per_model_levels_are_not_unioned() {
        let raw = raw_view(
            session_config("model-a", Some("low"), &["low"]),
            Some(json!([{
                "providerId": CURRENT_PROVIDER,
                "models": [
                    model_entry("model-a", Some(&["low"])),
                    model_entry("model-b", Some(&["low", "max"])),
                ],
            }])),
        );

        let view = parse(&raw).expect("catalog");
        assert_eq!(levels_of(&view, "model-a"), strings(&["low"]));
        assert_eq!(levels_of(&view, "model-b"), strings(&["low", "max"]));
    }

    #[test]
    fn fallback_thought_levels_apply_only_to_current_model() {
        let raw = raw_view(
            session_config("model-a", Some("low"), &["low", "high"]),
            Some(json!([{
                "providerId": CURRENT_PROVIDER,
                "models": [
                    // 目录未宣告当前模型档位：可用快照档位回退。
                    model_entry("model-a", None),
                    // 另一个模型不得借用当前模型的快照档位。
                    model_entry("model-b", None),
                ],
            }])),
        );

        let view = parse(&raw).expect("catalog");
        assert_eq!(levels_of(&view, "model-a"), strings(&["low", "high"]));
        assert!(levels_of(&view, "model-b").is_empty());
    }

    #[test]
    fn malformed_catalog_is_needs_evidence() {
        // 当前 provider 条目自身形状不符（models 不是数组）→ 目录不可信。
        let bad_current = raw_view(
            session_config("model-a", None, &[]),
            Some(json!([{ "providerId": CURRENT_PROVIDER, "models": "not-an-array" }])),
        );
        assert!(parse(&bad_current)
            .expect_err("bad current provider")
            .is(codes::NEEDS_EVIDENCE));
        // 模型条目缺 modelId。
        let without_model_id = raw_view(
            session_config("model-a", None, &[]),
            Some(json!([{
                "providerId": CURRENT_PROVIDER,
                "models": [{ "config": { "optionSpecs": {} } }],
            }])),
        );
        assert!(parse(&without_model_id)
            .expect_err("no modelId")
            .is(codes::NEEDS_EVIDENCE));
        // wire 形状不符（providers 不是数组）→ unsupported_protocol。
        let not_an_array = raw_view(
            session_config("model-a", None, &[]),
            Some(json!("not-an-array")),
        );
        assert!(parse(&not_an_array)
            .expect_err("providers shape")
            .is(codes::UNSUPPORTED_PROTOCOL));
    }

    #[test]
    fn malformed_sibling_provider_does_not_discard_current_catalog() {
        // 其他账号/兄弟 provider 的坏形状不得否决当前会话可用的目录。
        let raw = raw_view(
            session_config("model-a", Some("low"), &["low"]),
            Some(json!([
                "not-an-object",
                { "models": [{ "modelId": "sibling-model" }] },
                { "providerId": "acct-other", "models": "not-an-array" },
                {
                    "providerId": CURRENT_PROVIDER,
                    "models": [
                        model_entry("model-a", Some(&["low", "high"])),
                        model_entry("model-b", Some(&["low"])),
                    ],
                },
            ])),
        );

        let view = parse(&raw).expect("malformed siblings must not discard the catalog");
        assert_eq!(view.provider, CURRENT_PROVIDER);
        let ids: Vec<&str> = view
            .models
            .iter()
            .map(|choice| choice.model_id.as_str())
            .collect();
        assert_eq!(ids, ["model-a", "model-b"]);
        assert_eq!(levels_of(&view, "model-a"), strings(&["low", "high"]));
    }

    #[test]
    fn duplicate_model_ids_are_deduped() {
        let raw = raw_view(
            session_config("model-a", Some("low"), &["low"]),
            Some(json!([{
                "providerId": CURRENT_PROVIDER,
                "models": [
                    // 同一 id 的重复档位先去重；重复模型 id 只保留首次出现。
                    model_entry("model-a", Some(&["low", "low", "high"])),
                    model_entry("model-a", Some(&["max"])),
                ],
            }])),
        );

        let view = parse(&raw).expect("catalog");
        assert_eq!(view.models.len(), 1);
        assert_eq!(levels_of(&view, "model-a"), strings(&["low", "high"]));
    }

    #[test]
    fn current_thought_may_be_absent() {
        // thought 缺失 → None，不构造空串。
        let absent = raw_view(
            json!({
                "provider": CURRENT_PROVIDER,
                "model": "model-a",
                "thoughtLevels": ["low"],
            }),
            None,
        );
        let view = parse(&absent).expect("catalog");
        assert_eq!(view.thought, None);
        // thought 为空串 → None（官方空串 = 无显式档位）。
        let empty = raw_view(session_config("model-a", Some(""), &["low"]), None);
        assert_eq!(parse(&empty).expect("catalog").thought, None);
    }
}
