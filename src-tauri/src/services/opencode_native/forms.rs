//! 整组答题的可信完整性校验（计划任务 6）。
//!
//! 纪律（任务 0 已证，见 `docs/engineering/evidence/opencode-native/followup/`）：
//!
//! * **合法跳过 = 省略键**：`DraftAnswer::Skip` 编码成「不写入 answer 对象」；
//!   `""` 对 `required:true` 字段是 400 `Missing required`，因此空串不是跳过编码，
//!   也永不作为一个值被编码出去。
//! * **必答集 = `required:true` ∧ 适用**。仅覆盖单个前置必答 string 的 eq；其余规则含
//!   [`FieldKind::Unsupported`] 字段的表单一律整组拒答、零派发：不挑可答字段先写一次。
//!   `required:None`/`required:false` 都不是必答。
//! * **提交原生 `value`**，不用展示 `label`；旧选项集里记住的答案若不在当前选项集内，
//!   必须拒绝而不是静默提交。
//! * 校验是纯函数：不做 I/O、不看时钟，也不回读原生响应。
//!
//! 编码表（`DraftAnswer` → 原生 `answer` 对象里的值）：
//!
//! * `Skip` → 省略该键（任意字段类型）；必答字段因此变成「缺答」被拒。
//! * `Text("")` → 省略该键；必答字段拒（`OC_ANSWER_INCOMPLETE`）。
//! * `Multi([])` → 省略该键；必答字段拒。
//! * `Text(v)` + `Text` 字段 → `"v"`。
//! * `Multi([v, ..])` + `Multi` 字段 → `["v", ..]`。
//!
//! 返回的 `Value` 就是 reply 请求体里 `answer` 的值（外层 `{"answer": ...}` 由派发方拼装）。
//!
//! # 残余边界：`custom:true` 的自由文本无法证伪
//!
//! 任务 0 只证明「选项之外的值不会被服务端拒绝」这一侧（金样 q0 就是 `custom:true`），
//! 没有证明服务端会拒绝任意非选项串。因此本层对 `custom:true` 字段**接受**非选项的自由
//! 文本——拒绝它会把合法答案一并挡掉。能探测到的唯一误用是「展示 label 被当成 value」：
//! 若提交串等于某个当前选项的 `label` 且不等于该选项的 `value`，说明前端把展示文案当值发
//! 了，本层以 `OC_ANSWER_INVALID` 拒绝（见 [`check_value`]）。
//!
//! 由此推出两条下游义务（任务 8/9/10 必须遵守）：手机端只能提交原生 `value`；「清除某项」
//! 必须编码成 `Skip`（省略键），**绝不**用 `Text("")`。在 `custom:true` 字段上，一个任意的
//! 非选项串（包括旧选项集里遗留的值）无法与本端自由文本区分，本层不会拦——它由「提交
//! 原生 value」的前端职责和任务 4 的 token 轮换共同约束，而不是由本函数兜底。
//!
//! # 错误码边界（与 `types.rs` 同源声明）
//!
//! 同一个 `!is_answerable()` 谓词有两个出口，用途不同、不得混用：
//!
//! * `OC_FORM_UNSUPPORTED`（[`FormSpec::unsupported_error`]）= **读取/展示期**：这一份表单按
//!   读到的样子不可答，卡片告诉手机「只读」。
//! * `OC_ANSWER_UNSUPPORTED`（本模块 [`validate_answers`]）= **提交期**：一次提交因同一原因被
//!   拒，零派发。
//!
//! 想对用户显示一条统一文案的调用方（任务 9/10）按 `code` 归属到同一个「表单规则暂不支持」
//! 提示即可，但内部控制流不应互换：读取期不该出现 `OC_ANSWER_*`，提交期不该出现
//! `OC_FORM_*`。

use serde_json::{Map, Value};

use super::projection::{context_token as derive_context_token, is_content_hash, is_context_token};
use super::types::{
    codes, Answers, DraftAnswer, FieldKind, FormField, FormOption, FormSpec, NativeError,
};

/// 整组答案校验：不通过就没有任何写出机会（调用方只在本函数成功后才 reserve/派发）。
///
/// 检查顺序固定，错误码因此可预期：整组不支持 → 传入答案的形状/取值 → 必答齐备。
pub fn validate_answers(form: &FormSpec, answers: &Answers) -> Result<Value, NativeError> {
    // 未覆盖规则（未证 when/hidden/非 bool required）⇒ 适用集算不出来 ⇒ 整组拒答。
    // 绝不「先挑可答字段写一次、下一次再补」：那会把一次整组提交拆成两次真写。
    // 这里是**提交期**，故用 OC_ANSWER_UNSUPPORTED（读取期用 OC_FORM_UNSUPPORTED）。
    if !form.is_answerable() {
        return Err(NativeError::new(
            codes::OC_ANSWER_UNSUPPORTED,
            "form rules are not covered by this client",
        ));
    }

    let mut encoded = Map::new();
    for (key, draft) in answers {
        let field = form
            .fields
            .iter()
            .find(|field| &field.key == key)
            .ok_or_else(invalid_answer)?;
        if !field_is_active(field, answers) {
            if !matches!(draft, DraftAnswer::Skip) {
                return Err(invalid_answer());
            }
            continue;
        }
        // `None` = 合法跳过（省略键）；空串/空列表也只走这条路。
        if let Some(value) = encode_answer(field, draft)? {
            encoded.insert(key.clone(), value);
        }
    }

    for field in &form.fields {
        // 只有 `required:true` 才是必答；`None`（服务端未指定）与 `false` 都不是。
        if field_is_active(field, answers)
            && field.required == Some(true)
            && !encoded.contains_key(&field.key)
        {
            return Err(NativeError::new(
                codes::OC_ANSWER_INCOMPLETE,
                "required answer is missing",
            ));
        }
    }

    Ok(Value::Object(encoded))
}

fn field_is_active(field: &FormField, answers: &Answers) -> bool {
    field.when.as_ref().is_none_or(|condition| {
        matches!(answers.get(&condition.key), Some(DraftAnswer::Text(value)) if value == &condition.value)
    })
}

/// 单个草稿 → 线上的值。`Ok(None)` 表示「不写入 answer 对象」。
fn encode_answer(field: &FormField, draft: &DraftAnswer) -> Result<Option<Value>, NativeError> {
    match (draft, &field.kind) {
        // 合法跳过 = 省略键；必答字段由上面的齐备检查兜住。
        (DraftAnswer::Skip, _) => Ok(None),
        (DraftAnswer::Text(text), FieldKind::Text { options, custom }) => {
            // 空串对 required 字段是 400 `Missing required`，所以它不是「跳过」，也不编成值。
            if text.is_empty() {
                return empty_answer(field);
            }
            check_value(options, *custom, text)?;
            Ok(Some(Value::String(text.clone())))
        }
        (DraftAnswer::Multi(values), FieldKind::Multi { options, custom }) => {
            // 空列表同样不是答案：必答拒、可选省略。
            if values.is_empty() {
                return empty_answer(field);
            }
            for value in values {
                check_value(options, *custom, value)?;
            }
            Ok(Some(Value::Array(
                values.iter().cloned().map(Value::String).collect(),
            )))
        }
        // 类型不符（含把文本塞给多选、把列表塞给单选）：不发，也不降级。
        (DraftAnswer::Text(_), _) | (DraftAnswer::Multi(_), _) => Err(invalid_answer()),
    }
}

/// 空值：必答字段等于没答（`OC_ANSWER_INCOMPLETE`），可选字段省略该键。
fn empty_answer(field: &FormField) -> Result<Option<Value>, NativeError> {
    if field.required == Some(true) {
        Err(NativeError::new(
            codes::OC_ANSWER_INCOMPLETE,
            "required answer is empty",
        ))
    } else {
        Ok(None)
    }
}

/// 该文本能否作为此字段的答案。
///
/// 1. 命中当前选项的原生 `value` ⇒ 接受（金样里 `label == value` 的选项也走这一支）。
/// 2. 只命中某个选项的展示 `label`（且与该选项 `value` 不同）⇒ 拒绝：这是前端把展示文案
///    当值提交，`custom:true` 时尤其危险——自由文本框会把 `"Red"` 当成合法文本存下去，
///    于是存进一个语义错误但没有 400 的答案。
/// 3. 其余：`custom:true` 或选项列表为空（纯文本框）⇒ 按自由文本接受（服务端不拒，见模块
///    文档「残余边界」）；否则拒绝（旧选项集的陈旧值落在这里）。
fn check_value(options: &[FormOption], custom: bool, value: &str) -> Result<(), NativeError> {
    if options.iter().any(|option| option.value == value) {
        return Ok(());
    }
    if options.iter().any(|option| option.label == value) {
        return Err(invalid_answer());
    }
    if custom || options.is_empty() {
        return Ok(());
    }
    Err(invalid_answer())
}

fn invalid_answer() -> NativeError {
    NativeError::new(codes::OC_ANSWER_INVALID, "answer does not match the form")
}

/// 两个调用方给的 token 是否指向同一上下文，且 `form` 是解析产物（纯函数，零 IO）。
///
/// 规则：`form.id` 非空、`form.content_hash` 是 64 位**小写**十六进制（`parse_form` 的实际
/// 产物），两个 token 都是投影产出的 token 形状且逐字相等。
///
/// **能证明**：两个 token 是同一上下文的投影产物，且 `form` 确实是 `parse_form` 解析出来的
/// 表单（id 与内容锚形状都成立）。
/// **不能证明**：这两个 token 属于**这一个** `form`。token 的可比对部分只是 32 位摘要有无
/// 与相等；别的表单 id、甚至 permission（`projection::read_pending` 用同一个
/// `context_token` 配方、同样的形状）产出的 token，只要两处传的是同一个串，本函数就会返回
/// true。要绑定到本表单必须用 [`same_form_context_for`]（带 `binding_token` 重算），
/// 派发路径（任务 7）用后者，本函数只作为「两个串是否同一上下文」的形状+相等检查保留（计划
/// 指定的签名）。
pub fn same_form_context(form: &FormSpec, context_token: &str, current_token: &str) -> bool {
    if form.id.is_empty() || !is_content_hash(&form.content_hash) {
        return false;
    }
    is_context_token(context_token)
        && is_context_token(current_token)
        && context_token == current_token
}

/// 绑定到「这个表单」的版本：用 `binding_token` 重算出本表单应有的 token，要求两个调用方
/// token **都**等于它（纯函数，零 IO，fail-closed）。
///
/// 重算不需要任何秘密：`context_token` 的预像是 `("context", binding, kind, id, content)` 的
/// 长度前缀拼接，其中 `binding_token` 本来就随 [`crate::services::opencode_native::PublicNativeView`]
/// 发给手机，`form.id`/`form.content_hash` 也都在 `FormSpec` 里。因此本函数能挡住：
/// 别的表单的 token、permission 的 token（`kind` 不同）、换代际前的旧 token、以及任何手搓
/// 的 32 位 hex 串。唯一前提是 `binding_token` 来自当前投影（旧 binding 会重算出不同结果，
/// 于是恰好也被挡住）。
pub fn same_form_context_for(
    form: &FormSpec,
    binding_token: &str,
    context_token: &str,
    current_token: &str,
) -> bool {
    if form.id.is_empty() || !is_content_hash(&form.content_hash) || binding_token.is_empty() {
        return false;
    }
    if !is_context_token(context_token) || !is_context_token(current_token) {
        return false;
    }
    let expected = derive_context_token(binding_token, "form", &form.id, &form.content_hash);
    context_token == expected && current_token == expected
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;

    use crate::services::opencode_native::contract::parse_form;
    use crate::services::opencode_native::types::{Answers, DraftAnswer, FieldKind, FormOption};

    #[test]
    fn t10_real_string_eq_applicability() {
        let raw: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/opencode-native/product-form-conditional.json"
        ))
        .unwrap();
        let form = parse_form(&raw).unwrap();
        assert_eq!(
            validate_answers(&form, &answers(vec![("q0", text("no"))])).unwrap(),
            json!({"q0":"no"})
        );
        assert_eq!(
            validate_answers(&form, &answers(vec![("q0", text("yes"))]))
                .unwrap_err()
                .code,
            codes::OC_ANSWER_INCOMPLETE
        );
        assert_eq!(
            validate_answers(
                &form,
                &answers(vec![("q0", text("no")), ("q1", text("extra"))])
            )
            .unwrap_err()
            .code,
            codes::OC_ANSWER_INVALID
        );
        assert_eq!(
            validate_answers(
                &form,
                &answers(vec![("q0", text("yes")), ("q1", text("ok"))])
            )
            .unwrap(),
            json!({"q0":"yes","q1":"ok"})
        );
    }

    #[test]
    fn t10_unknown_conditions_stay_blocked_and_condition_changes_rotate_hash() {
        let raw: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/opencode-native/product-form-conditional.json"
        ))
        .unwrap();
        let original = parse_form(&raw).unwrap();
        let mut changed = raw.clone();
        changed["data"]["fields"][1]["when"][0]["value"] = json!("different");
        assert_ne!(
            original.content_hash,
            parse_form(&changed).unwrap().content_hash
        );
        for when in [
            json!([]),
            json!([{ "key":"q0", "op":"neq", "value":"yes"}]),
            json!([{ "key":"q0", "op":"eq", "value":1}]),
            json!([{ "key":"q1", "op":"eq", "value":"yes"}]),
            json!([{ "key":"q0", "op":"eq", "value":"yes", "extra":true}]),
        ] {
            let mut mutated = raw.clone();
            mutated["data"]["fields"][1]["when"] = when;
            assert!(!parse_form(&mutated).unwrap().is_answerable());
        }
    }

    /// 任务 0 金样 `followup/fixtures/form.pending.json` 的仓库内形态（已去敏）：
    /// 完整三字段表单（q0 选项 + `custom:true`、q1 必答自由文本、q2 未声明 required）。
    const GOLDEN_PENDING: &str =
        include_str!("../../../tests/fixtures/opencode-native/form.pending.json");
    /// 选项 `label ≠ value` 的夹具（`custom:true`，与金样 q0 同一形态）。
    const GOLDEN_LABEL: &str =
        include_str!("../../../tests/fixtures/opencode-native/form.options.label.json");

    /// 各用例共用的答案构造：保持原生 `answer` 对象的键序无关性（BTreeMap）。
    fn answers(pairs: Vec<(&str, DraftAnswer)>) -> Answers {
        pairs
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect()
    }

    fn text(value: &str) -> DraftAnswer {
        DraftAnswer::Text(value.to_string())
    }

    fn multi(values: &[&str]) -> DraftAnswer {
        DraftAnswer::Multi(values.iter().map(|value| value.to_string()).collect())
    }

    fn golden(raw: &str) -> Value {
        serde_json::from_str(raw).expect("金样夹具必须是合法 JSON")
    }

    /// 从金样 `data.fields` 里原样取出某个字段（含 description/options/custom），
    /// 夹具因此不会与证据漂移。
    fn golden_field(raw: &str, key: &str) -> Value {
        golden(raw)
            .get("data")
            .and_then(|data| data.get("fields"))
            .and_then(Value::as_array)
            .expect("金样必须有 data.fields 数组")
            .iter()
            .find(|field| field.get("key").and_then(Value::as_str) == Some(key))
            .unwrap_or_else(|| panic!("金样字段数组里没有 {key}"))
            .clone()
    }

    /// 用金样字段拼一份派生表单。`id` 显式给出：`content_hash` 含 id，派生表单不沿用整份
    /// 金样的身份（否则同 id 两种内容会共用名字，读起来像同一张卡）。
    fn form_from_golden_fields(id: &str, fields: Vec<Value>) -> FormSpec {
        parse_form(&json!({ "data": { "id": id, "fields": fields } }))
            .expect("由金样字段拼出的表单可解析")
    }

    /// 计划示意用例的表单：金样 q0（选项 + `custom:true`，`required:true`）与金样 q1
    /// （自由文本，`required:true`）。金样还有第三个字段 q2，这里刻意**不放**：示意用例的
    /// `unknown_q2()` 要求 q2 不在表单里。
    fn required_two_text_fields() -> FormSpec {
        form_from_golden_fields(
            "frm_fixture_required",
            vec![
                golden_field(GOLDEN_PENDING, "q0"),
                golden_field(GOLDEN_PENDING, "q1"),
            ],
        )
    }

    /// 金样 q0 + 金样 q2：q2 在证据里**未声明** `required`（`None`），因此是可选项。
    /// 证据：`form.optional-partial.json`（q1 未声明 required，省略后 204，`state.answer`
    /// 只有实际提交的键）。
    fn required_with_optional_note() -> FormSpec {
        form_from_golden_fields(
            "frm_fixture_optional-note",
            vec![
                golden_field(GOLDEN_PENDING, "q0"),
                golden_field(GOLDEN_PENDING, "q2"),
            ],
        )
    }

    /// **合成**（无金样）：`required:false` 的显式可选字段。金样只证到
    /// 「未声明 required 可省略」（`form.optional-partial.json`）；`form.rules.matrix.json`
    /// 的 `explicit-optional-omitted` 只有 `reply {} → 204`，**没有 create body**，证明不了
    /// 那一例是 `required:false`。本端对 `None` 和 `false` 都不当必答（安全侧：少发一个键）。
    fn explicit_optional_form() -> FormSpec {
        parse_form(&json!({
            "data": {
                "id": "frm_fixture_explicit-optional",
                "fields": [
                    { "key": "q0", "title": "NOTE", "type": "string", "required": false }
                ]
            }
        }))
        .expect("显式可选表单可解析")
    }

    /// 金样 `form.rules.matrix.json` 的 `when-false-required-omitted` create 体：`q1` 带 `when`。
    fn conditional_form() -> FormSpec {
        parse_form(&json!({
            "data": {
                "id": "frm_fixture_when",
                "fields": [
                    {
                        "key": "q0",
                        "title": "COLOR",
                        "type": "string",
                        "options": [
                            { "value": "red", "label": "red" },
                            { "value": "blue", "label": "blue" }
                        ]
                    },
                    {
                        "key": "q1",
                        "title": "WHY-RED",
                        "type": "string",
                        "required": true,
                        "when": [{ "key": "q0", "op": "eq", "value": "red" }]
                    }
                ]
            }
        }))
        .expect("含 when 的表单仍可解析用于只读展示")
    }

    /// 金样 `form.rules.matrix.json` 的 `hidden-required-omitted` create 体：`hidden` 不豁免必答。
    fn hidden_required_form() -> FormSpec {
        parse_form(&json!({
            "data": {
                "id": "frm_fixture_hidden",
                "fields": [
                    { "key": "q0", "title": "VISIBLE", "type": "string" },
                    { "key": "q1", "title": "HIDDEN", "type": "string", "hidden": true, "required": true }
                ]
            }
        }))
        .expect("含 hidden 的表单仍可解析用于只读展示")
    }

    /// **合成**（无金样）：金样 q0 的选项集被换掉（`red` 已不存在），`custom:false` 使成员
    /// 判定可观察。手机端从旧选项集记住的答案属于陈旧值。
    fn stale_options_form() -> FormSpec {
        parse_form(&json!({
            "data": {
                "id": "frm_fixture_stale-options",
                "fields": [
                    {
                        "key": "q0",
                        "title": "COLOR",
                        "type": "string",
                        "required": true,
                        "options": [
                            { "value": "blue", "label": "blue" },
                            { "value": "green", "label": "green" }
                        ]
                    }
                ]
            }
        }))
        .expect("当前选项集表单可解析")
    }

    /// **合成**（无金样）：`custom:false` + `label ≠ value`，用于断言「只收 value」的成员
    /// 判定确实生效（`custom:true` 的自由文本口径见 [`labeled_options_form`]）。
    fn strict_options_form() -> FormSpec {
        parse_form(&json!({
            "data": {
                "id": "frm_fixture_strict-options",
                "fields": [
                    {
                        "key": "q0",
                        "title": "COLOR",
                        "type": "string",
                        "required": true,
                        "options": [
                            { "value": "q0-red", "label": "Red", "description": "warm" },
                            { "value": "q0-blue", "label": "Blue" }
                        ],
                        "custom": false
                    }
                ]
            }
        }))
        .expect("严格选项表单可解析")
    }

    /// **金样**（`form.options.label.json`，`custom:true`）：展示 label 与提交 value 不同。
    fn labeled_options_form() -> FormSpec {
        parse_form(&golden(GOLDEN_LABEL)).expect("label≠value 金样可解析")
    }

    /// **合成**（无金样）：`multiselect` 未被任务 0 逐类验证，编码按类型对称推断。
    fn multi_form(required: Option<bool>) -> FormSpec {
        let mut field = json!({
            "key": "q1",
            "title": "TAGS",
            "type": "multiselect",
            "options": [
                { "value": "a", "label": "A" },
                { "value": "b", "label": "B" }
            ]
        });
        // `required` 三态：未指定就不写该键（与金样一致）。
        if let Some(flag) = required {
            field["required"] = json!(flag);
        }
        parse_form(&json!({
            "data": { "id": "frm_fixture_multi", "fields": [field] }
        }))
        .expect("多选表单可解析")
    }

    /// 计划示意用例的固定输入：只答 q0。
    fn only_q0() -> Answers {
        answers(vec![("q0", text("red"))])
    }

    /// 计划示意用例的固定输入：两个必答都答（金样 `form.reply.request.json` 的 answer 形态）。
    fn both_answers() -> Answers {
        answers(vec![("q0", text("red")), ("q1", text("Alice"))])
    }

    /// 计划示意用例的固定输入：必有键齐备，外加一个表单里不存在的 q2。
    fn unknown_q2() -> Answers {
        answers(vec![
            ("q0", text("red")),
            ("q1", text("Alice")),
            ("q2", text("note")),
        ])
    }

    /// 投影产出的 token：测试里用**同一个**派生函数造，避免手抄哈希配方。
    fn token_for(binding: &str, form: &FormSpec) -> String {
        derive_context_token(binding, "form", &form.id, &form.content_hash)
    }

    /// 对外错误码是契约：字符串值不得随重构漂移。
    #[test]
    fn answer_error_codes_are_stable() {
        assert_eq!(codes::OC_ANSWER_INCOMPLETE, "OC_ANSWER_INCOMPLETE");
        assert_eq!(codes::OC_ANSWER_INVALID, "OC_ANSWER_INVALID");
        assert_eq!(codes::OC_ANSWER_UNSUPPORTED, "OC_ANSWER_UNSUPPORTED");
    }

    /// 同一不可答谓词、两个出口：读取/展示期 `OC_FORM_UNSUPPORTED`，提交期
    /// `OC_ANSWER_UNSUPPORTED`。两边都钉住，任务 9/10 才能安全地映射成一条文案。
    #[test]
    fn unsupported_form_maps_to_read_time_and_submit_time_codes() {
        let form = conditional_form();
        assert_eq!(
            form.unsupported_error().expect("读取期应报不可答").code,
            "OC_FORM_UNSUPPORTED"
        );
        assert_eq!(
            validate_answers(&form, &only_q0()).unwrap_err().code,
            "OC_ANSWER_UNSUPPORTED"
        );
    }

    /// 计划示意用例原样：缺必答 → INCOMPLETE；齐备 → 原样 answer 对象；未知键 → INVALID。
    #[test]
    fn required_fields_missing_or_unknown_key_are_refused() {
        let form = required_two_text_fields();
        assert_eq!(
            validate_answers(&form, &only_q0()).unwrap_err().code,
            "OC_ANSWER_INCOMPLETE"
        );
        assert_eq!(
            validate_answers(&form, &both_answers()).unwrap(),
            json!({ "q0": "red", "q1": "Alice" }),
            "answer 对象形态必须与金样 form.reply.request.json 一致"
        );
        assert_eq!(
            validate_answers(&form, &unknown_q2()).unwrap_err().code,
            "OC_ANSWER_INVALID"
        );
    }

    /// 合法跳过 = 省略键：`Skip` 与缺键等价，绝不编码空串；必答空串不是跳过。
    #[test]
    fn optional_skip_never_becomes_implicit_empty_text() {
        let form = required_with_optional_note();

        let skipped = validate_answers(
            &form,
            &answers(vec![("q0", text("blue")), ("q2", DraftAnswer::Skip)]),
        )
        .expect("可选项 Skip 是合法跳过");
        assert_eq!(skipped, json!({ "q0": "blue" }));
        assert!(
            skipped.get("q2").is_none(),
            "Skip 必须编码成省略键，不得写成空串"
        );

        // 直接省略该键与 Skip 完全等价。
        let omitted = validate_answers(&form, &answers(vec![("q0", text("blue"))]))
            .expect("可选项缺键是合法跳过");
        assert_eq!(omitted, skipped);

        // 可选文本被清空也不编码：空串既不是值也不是跳过编码。
        let cleared = validate_answers(
            &form,
            &answers(vec![("q0", text("blue")), ("q2", text(""))]),
        )
        .expect("可选空文本等价于跳过");
        assert_eq!(cleared, json!({ "q0": "blue" }));

        // 必答字段的空串不是跳过：服务端会 400 Missing required，本端不得先发出去。
        let err = validate_answers(&form, &answers(vec![("q0", text("")), ("q2", text("x"))]))
            .expect_err("必答空串必须拒绝");
        assert_eq!(err.code, "OC_ANSWER_INCOMPLETE");

        // 必答字段 Skip 同理。
        let err = validate_answers(
            &form,
            &answers(vec![("q0", DraftAnswer::Skip), ("q2", text("x"))]),
        )
        .expect_err("必答字段 Skip 不能算答");
        assert_eq!(err.code, "OC_ANSWER_INCOMPLETE");
    }

    /// 省略可选字段得到空 answer 对象，覆盖 `required` 的两个非必答三态：
    ///
    /// * `required:None`（未声明）：证据 `form.optional-partial.json`——q1 未声明 required，
    ///   只答 q0 时 204，`state.answer` 只有 `{"q0":"red"}`。
    /// * `required:false`（显式）：**无金样**（见 [`explicit_optional_form`] 注释）。本端按
    ///   「只有 `required:true` 才算必答」处理，故两者都编码成省略键。
    #[test]
    fn optional_omitted_is_an_empty_answer_object() {
        let unset = required_with_optional_note();
        assert_eq!(
            validate_answers(&unset, &answers(vec![("q0", text("blue"))])).unwrap(),
            json!({ "q0": "blue" })
        );

        let explicit_false = explicit_optional_form();
        assert_eq!(
            validate_answers(&explicit_false, &Answers::new()).unwrap(),
            json!({})
        );
        assert_eq!(
            validate_answers(&explicit_false, &answers(vec![("q0", DraftAnswer::Skip)])).unwrap(),
            json!({})
        );
        assert_eq!(
            validate_answers(&explicit_false, &answers(vec![("q0", text("v"))])).unwrap(),
            json!({ "q0": "v" })
        );
    }

    /// `when`/`hidden` 未覆盖 ⇒ 适用集未知 ⇒ 整组拒答、零派发（不挑字段先写一次）。
    #[test]
    fn unsupported_conditional_form_has_zero_dispatch() {
        let cases = [
            ("when", conditional_form()),
            ("hidden", hidden_required_form()),
        ];
        for (name, form) in cases {
            assert!(
                !form.is_answerable(),
                "{name}: 含未覆盖规则的表单必须整组不可答"
            );
            assert!(
                form.fields
                    .iter()
                    .any(|field| matches!(field.kind, FieldKind::Unsupported)),
                "{name}: 未覆盖字段仍在结果里可见（不静默丢弃）"
            );

            // 看起来齐备的答案也不发：只要适用集算不出来，就不猜必答集。
            let complete = answers(vec![("q0", text("blue")), ("q1", text("because"))]);
            assert_eq!(
                validate_answers(&form, &complete).unwrap_err().code,
                "OC_ANSWER_UNSUPPORTED",
                "{name}: 齐备答案也必须整组拒答"
            );

            // 缺答不得退化成 INCOMPLETE：那会让人以为「补答一次就能发」。
            let partial = answers(vec![("q0", text("blue"))]);
            assert_eq!(
                validate_answers(&form, &partial).unwrap_err().code,
                "OC_ANSWER_UNSUPPORTED",
                "{name}: 缺答同样是整组不支持，不是可补的缺答"
            );
        }
    }

    /// 旧选项集里记住的答案（现已不在可见选项内）必须拒绝，不能静默提交。
    ///
    /// 名字沿用计划命名。注意：**夹具里没有 `hidden` 字段**，本用例只覆盖「旧选项集」这一侧
    /// （`hidden` 的必答语义由 [`unsupported_conditional_form_has_zero_dispatch`] 覆盖）。
    /// 另外这一侧可观察的前提是 `custom:false`：在 `custom:true` 字段上旧值与非选项自由文本
    /// 不可区分，本层不拦（见模块文档「残余边界」）。
    #[test]
    fn stale_options_reject_hidden_old_answer() {
        let form = stale_options_form();
        let remembered = answers(vec![("q0", text("red"))]);
        assert_eq!(
            validate_answers(&form, &remembered).unwrap_err().code,
            "OC_ANSWER_INVALID",
            "旧选项集的值不得被提交到新选项集"
        );
        assert_eq!(
            validate_answers(&form, &answers(vec![("q0", text("blue"))])).unwrap(),
            json!({ "q0": "blue" }),
            "当前选项集内的 value 仍必须可答"
        );
    }

    /// `custom:true` 金样：自由文本被接受，但「label 顶替 value」必须拒绝。
    ///
    /// 这是本端唯一能探测到的 label 误用：提交串等于某选项 `label` 而不等于其 `value`。
    /// 任意非选项自由文本仍然会通过（模块文档「残余边界」），因此下游必须只发原生 value。
    #[test]
    fn option_label_never_replaces_native_value() {
        let form = labeled_options_form();
        let FieldKind::Text { options, custom } = &form.fields[0].kind else {
            panic!("q0 应为文本选择题");
        };
        assert!(
            *custom,
            "label-vs-value 金样的 q0 是 custom:true（任务 0 形态），不得改写成 false"
        );
        let FormOption { value, label, .. } = &options[0];
        assert_ne!(value, label, "夹具的 label 必须与 value 不同");

        assert_eq!(
            validate_answers(&form, &answers(vec![("q0", text(label))]))
                .unwrap_err()
                .code,
            "OC_ANSWER_INVALID",
            "custom:true 下把展示 label 当 value 提交也必须拒绝"
        );
        assert_eq!(
            validate_answers(&form, &answers(vec![("q0", text(value))])).unwrap(),
            json!({ "q0": "q0-red" }),
            "提交值必须是原生 value"
        );
        // 非选项自由文本：服务端不拒（无拒绝证据），本端按自由文本接受（残余边界）。
        assert_eq!(
            validate_answers(&form, &answers(vec![("q0", text("deep blue"))])).unwrap(),
            json!({ "q0": "deep blue" })
        );
    }

    /// `custom:false`（**合成**夹具）：成员判定生效——只有当前选项的原生 value 能过。
    #[test]
    fn option_membership_is_enforced_when_custom_is_false() {
        let form = strict_options_form();
        let FieldKind::Text { options, custom } = &form.fields[0].kind else {
            panic!("q0 应为文本选择题");
        };
        assert!(!*custom, "合成夹具是 custom:false");
        let FormOption { value, label, .. } = &options[0];
        assert_ne!(value, label, "夹具的 label 必须与 value 不同");

        assert_eq!(
            validate_answers(&form, &answers(vec![("q0", text(label))]))
                .unwrap_err()
                .code,
            "OC_ANSWER_INVALID",
            "custom:false 时 label 不是选项 value"
        );
        assert_eq!(
            validate_answers(&form, &answers(vec![("q0", text("deep blue"))]))
                .unwrap_err()
                .code,
            "OC_ANSWER_INVALID",
            "custom:false 时非选项自由文本必须拒绝"
        );
        assert_eq!(
            validate_answers(&form, &answers(vec![("q0", text(value))])).unwrap(),
            json!({ "q0": "q0-red" })
        );
    }

    /// 多选只接受列表，元素也必须是当前选项的原生 value。
    #[test]
    fn multi_field_takes_list_of_current_option_values() {
        let form = multi_form(Some(true));
        assert_eq!(
            validate_answers(&form, &answers(vec![("q1", text("a"))]))
                .unwrap_err()
                .code,
            "OC_ANSWER_INVALID",
            "文本草稿不得塞给多选字段"
        );
        assert_eq!(
            validate_answers(&form, &answers(vec![("q1", multi(&["a", "c"]))]))
                .unwrap_err()
                .code,
            "OC_ANSWER_INVALID",
            "列表元素不在当前选项集必须拒绝"
        );
        assert_eq!(
            validate_answers(&form, &answers(vec![("q1", multi(&["A"]))]))
                .unwrap_err()
                .code,
            "OC_ANSWER_INVALID",
            "多选元素用展示 label 同样拒绝"
        );
        assert_eq!(
            validate_answers(&form, &answers(vec![("q1", multi(&["a", "b"]))])).unwrap(),
            json!({ "q1": ["a", "b"] })
        );
        assert_eq!(
            validate_answers(&form, &answers(vec![("q1", multi(&[]))]))
                .unwrap_err()
                .code,
            "OC_ANSWER_INCOMPLETE",
            "必答多选的空列表等于没答"
        );

        // 未声明 required 的多选：空列表/缺键都是合法跳过，编码为空对象。
        let optional = multi_form(None);
        assert_eq!(
            validate_answers(&optional, &answers(vec![("q1", multi(&[]))])).unwrap(),
            json!({})
        );
    }

    /// 本地数组编码回归（合成输入，不作为上游多选语义证据）。
    #[test]
    fn t10_multi_custom_and_empty_options_keep_backend_validation() {
        for (custom, options) in [
            (true, json!([{"value":"a","label":"A"}])),
            (false, json!([])),
        ] {
            let form = parse_form(&json!({"id":"frm_multi_local", "fields":[{"key":"q1", "type":"multiselect", "required":true, "custom":custom, "options":options}]})).unwrap();
            assert_eq!(
                validate_answers(
                    &form,
                    &answers(vec![("q1", multi(&["free", "two\nlines"]))])
                )
                .unwrap(),
                json!({"q1":["free", "two\nlines"]})
            );
            assert_eq!(
                validate_answers(&form, &answers(vec![("q1", text("free"))]))
                    .unwrap_err()
                    .code,
                codes::OC_ANSWER_INVALID
            );
            assert_eq!(
                validate_answers(&form, &answers(vec![("q1", DraftAnswer::Skip)]))
                    .unwrap_err()
                    .code,
                codes::OC_ANSWER_INCOMPLETE
            );
        }
    }

    /// 比较版：同一 token、且 `form` 是解析产物才算同一上下文；token 轮换/形状不对/内容锚
    /// 不合规都拒绝。它**不**绑定到本表单（见函数文档与下一个用例）。
    #[test]
    fn same_form_context_requires_current_token_and_parsed_form() {
        let form = required_two_text_fields();
        let current = token_for("binding-under-test", &form);
        let other_form = required_with_optional_note();
        let rotated = token_for("binding-under-test", &other_form);

        assert_eq!(current.len(), 32, "投影 token 是 32 位小写十六进制");
        assert!(
            same_form_context(&form, &current, &current),
            "同一 token 即同一上下文"
        );
        assert!(
            !same_form_context(&form, &current, &rotated),
            "内容变化后 token 轮换，旧表单不得可答"
        );
        assert!(
            !same_form_context(&form, &rotated, &current),
            "反过来同样不成立"
        );
        assert!(
            !same_form_context(&form, "", ""),
            "空 token 不是同一上下文（缺 token ≠ 未变化）"
        );
        assert!(
            !same_form_context(&form, "frm_fixture_required", "frm_fixture_required"),
            "表单 id 不是 token，不能被当成上下文凭证"
        );
        assert!(
            !same_form_context(&form, &current.to_uppercase(), &current),
            "token 形状不对（大写）就不算投影产物"
        );

        // 未经 parse_form 的 FormSpec 没有内容锚：不能作为可答身份。
        let empty_hash = FormSpec {
            id: "frm_fabricated".to_string(),
            fields: form.fields.clone(),
            content_hash: String::new(),
        };
        assert!(
            !same_form_context(&empty_hash, &current, &current),
            "缺 content_hash 的 FormSpec 不是可答身份"
        );

        // `parse_form` 只产出小写 hex：大写 64 位串不是它的产物（哪怕长度对）。
        let uppercase_hash = FormSpec {
            id: form.id.clone(),
            fields: form.fields.clone(),
            content_hash: form.content_hash.to_uppercase(),
        };
        assert!(
            !same_form_context(&uppercase_hash, &current, &current),
            "大写 content_hash 不是 parse_form 的产物"
        );
        assert!(
            !same_form_context_for(&uppercase_hash, "binding-under-test", &current, &current),
            "绑定版同样不接受大写 content_hash"
        );
    }

    /// 绑定版：用 `binding_token` 重算本表单的 token，别的表单、permission、手搓串都不通过。
    ///
    /// 同时钉住比较版的已知边界（I2）：它只比两个串，所以「同一个 permission token 传两遍」
    /// 会返回 true——派发路径因此必须用 [`same_form_context_for`]。
    #[test]
    fn binding_aware_context_rejects_foreign_tokens() {
        let form = required_two_text_fields();
        let binding = "binding-under-test";
        let mine = token_for(binding, &form);

        assert!(
            same_form_context_for(&form, binding, &mine, &mine),
            "本表单的当前 token 必须通过绑定版"
        );

        // 另一个表单（同 binding、同 session 世界，只是 id/内容不同）的 token。
        let other_form = required_with_optional_note();
        let other = token_for(binding, &other_form);
        assert_ne!(other, mine, "不同表单的 token 必须不同");
        assert!(
            !same_form_context_for(&form, binding, &other, &other),
            "别的表单的 token 不能答这张表单"
        );

        // permission 用同一个 context_token 配方、同样的 32 位形状（projection 里 kind 不同）。
        let permission = derive_context_token(binding, "permission", "per_1", "once|reject");
        assert_eq!(permission.len(), 32);
        assert!(
            !same_form_context_for(&form, binding, &permission, &permission),
            "permission 的 token 不能答表单"
        );
        // 比较版区分不了（这正是必须用绑定版的原因）。
        assert!(
            same_form_context(&form, &permission, &permission),
            "比较版只看形状与相等：这是已知边界，不是期望行为"
        );

        // 换 binding（换代际/换会话）后旧 token 失效。
        assert!(
            !same_form_context_for(&form, "binding-after-restart", &mine, &mine),
            "旧 binding 的 token 不能答新代际"
        );

        // 手搓的 32 位 hex 串不是任何投影产物。
        let hand_made = "0f0e0d0c0b0a09080706050403020100";
        assert!(
            !same_form_context_for(&form, binding, hand_made, hand_made),
            "手搓 token 不能通过绑定版"
        );
        // 两个参数不一致（一个当前、一个旧的）同样拒绝。
        assert!(
            !same_form_context_for(&form, binding, &mine, &other),
            "任一侧不是本表单的当前 token 都拒绝"
        );
        // 空 binding 不能通过（否则重算就变成无身份哈希）。
        assert!(!same_form_context_for(&form, "", &mine, &mine));
    }
}
