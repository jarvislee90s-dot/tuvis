//! OpenCode 原生契约：逐操作能力判定与原生表单解析（任务 1）。
//!
//! 判定纪律（计划「关键判定规则」）：
//! * 只看本端**实际消费**的路径/形状/决定词/语义规则；版本号与无关描述不参与判定，
//!   因此 2.0.22 只是已测基线，不是以后唯一可用版本号。
//! * 每个 [`NativeOperationId`] 独立判定：缺一个 reply 形状只关对应写能力，不连带关只读。
//! * 表单只覆盖任务 0 与 T3 已证形态；未证 `when`/`hidden`/未覆盖类型与约束一律 `Unsupported`，
//!   不静默丢弃、不降级成 `Text`。
//! * 证据串只含结构性规则（路径/形状），不含正文、路径实参或凭据。

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::types::{
    codes, Capabilities, FieldKind, FormField, FormOption, FormSpec, NativeError,
    NativeOperationId, OperationAvailability,
};

const PROMPT_PATH: &str = "/api/session/{sessionID}/prompt";
const MESSAGE_PATH: &str = "/api/session/{sessionID}/message";
const INBOX_PATH: &str = "/api/session/{sessionID}/inbox";
const INBOX_ITEM_PATH: &str = "/api/session/{sessionID}/inbox/{inboxID}";
const FORM_LIST_PATH: &str = "/api/session/{sessionID}/form";
const FORM_REPLY_PATH: &str = "/api/session/{sessionID}/form/{formID}/reply";
const PERMISSION_LIST_PATH: &str = "/api/session/{sessionID}/permission";
const PERMISSION_REPLY_PATH: &str = "/api/session/{sessionID}/permission/{requestID}/reply";
const INTERRUPT_PATH: &str = "/api/session/{sessionID}/interrupt";
const AGENT_PATH: &str = "/api/session/{sessionID}/agent";
const AGENT_LIST_PATH: &str = "/api/agent";

/// 逐操作能力判定（纯函数，零 IO）。
pub fn evaluate_contract(openapi: &Value) -> Capabilities {
    let messages_ok = list_readable(openapi, MESSAGE_PATH);

    let prompt_shape_ok = operation(openapi, PROMPT_PATH, "post")
        .is_some_and(|op| request_schema(op).is_some() && success_json_schema(op).is_some());
    // `client_message_id` 是 Confirmed 的唯一确定性依据：请求体必须接受 id、2xx 必须回显 id。
    let id_gaps = correlation_id_gaps(openapi);
    // delivery 词集合出现 {queue,steer} 之外的词 ⇒ 运行期 DeliveryKind 反序列化会失败。
    // delivery 是展示与投递判定的消费字段：queue/steer 两个词缺一不可猜，且集合必须穷尽。
    // steer 与 queue 共用 Send 能力：缺任一投递词就不放行发送，避免 UI 出现无法提交的档位。
    let delivery_known = delivery_words(openapi).is_some_and(|words| {
        has_word(words, "queue") && has_word(words, "steer") && only_delivery_words(words)
    });
    let send_ok = prompt_shape_ok
        && id_gaps.is_empty()
        && messages_ok
        && delivery_known
        && !has_foreign_delivery_word(openapi);
    let inbox_ok = list_readable(openapi, INBOX_PATH) && delivery_known;

    // DELETE 对未知/已投递 id 都返回 204，204 本身不是撤回证据 ⇒ 必须能回读 inbox。
    let retract_ok =
        inbox_ok && operation(openapi, INBOX_ITEM_PATH, "delete").is_some_and(has_success_response);
    // 原生插队 = PATCH 既有 inbox 项的 delivery（queue → steer）。同样要求能回读 inbox，
    // 且 PATCH 必须声明成功响应——否则「改投递」没有可证事实。
    let promote_ok =
        inbox_ok && operation(openapi, INBOX_ITEM_PATH, "patch").is_some_and(has_success_response);

    let form_readable = list_readable(openapi, FORM_LIST_PATH) && form_detail_known(openapi);
    let permission_readable =
        list_readable(openapi, PERMISSION_LIST_PATH) && permission_request_known(openapi);
    let pending_ok = form_readable && permission_readable;

    // 答题必须能读回表单、且 reply 体明确要求 `answer`（缺形状就整组不可答）。
    let answer_ok = form_readable
        && operation(openapi, FORM_REPLY_PATH, "post").is_some_and(|op| {
            has_success_response(op)
                && request_schema(op)
                    .and_then(|schema| resolve(openapi, schema))
                    .and_then(|schema| schema.get("required"))
                    .and_then(Value::as_array)
                    .is_some_and(|required| has_word(required, "answer"))
        });

    let reply_post = operation(openapi, PERMISSION_REPLY_PATH, "post")
        .filter(|op| request_schema(op).is_some() && has_success_response(op));
    // once / reject / always 分开判；每个词都要有 permission read + reply 形状。
    let decisions = decision_words(openapi);
    let once_ok = permission_readable
        && reply_post.is_some()
        && decisions.is_some_and(|words| has_word(words, "once"));
    let reject_ok = permission_readable
        && reply_post.is_some()
        && decisions.is_some_and(|words| has_word(words, "reject"));

    let mut caps = Capabilities::new();
    caps.insert(
        NativeOperationId::ReadMessages,
        verdict(
            messages_ok,
            "GET /api/session/{sessionID}/message (data[])",
            "missing GET /api/session/{sessionID}/message with data[]",
        ),
    );
    caps.insert(
        NativeOperationId::Send,
        if send_ok {
            verdict(
                true,
                "POST /api/session/{sessionID}/prompt (id in + echo) + message readback",
                "",
            )
        } else if prompt_shape_ok && !id_gaps.is_empty() {
            OperationAvailability {
                allowed: false,
                evidence: format!(
                    "prompt contract lacks client id support: {}",
                    id_gaps.join(", ")
                ),
                reason_code: Some(codes::OC_CONTRACT_ID_UNSUPPORTED.to_string()),
            }
        } else {
            verdict(
                false,
                "",
                "missing POST /api/session/{sessionID}/prompt or its message readback",
            )
        },
    );
    caps.insert(
        NativeOperationId::ReadInbox,
        verdict(
            inbox_ok,
            "GET /api/session/{sessionID}/inbox + delivery queue/steer",
            "missing GET /api/session/{sessionID}/inbox or delivery words",
        ),
    );
    caps.insert(
        NativeOperationId::Retract,
        verdict(
            retract_ok,
            "DELETE /api/session/{sessionID}/inbox/{inboxID} + inbox readback",
            "missing DELETE /api/session/{sessionID}/inbox/{inboxID} or inbox readback",
        ),
    );
    caps.insert(
        NativeOperationId::PromoteInbox,
        verdict(
            promote_ok,
            "PATCH /api/session/{sessionID}/inbox/{inboxID} (delivery) + inbox readback",
            "missing PATCH /api/session/{sessionID}/inbox/{inboxID} or inbox readback",
        ),
    );
    caps.insert(
        NativeOperationId::ReadPending,
        verdict(
            pending_ok,
            "GET .../form + GET .../permission (session scoped)",
            "missing session-scoped form/permission read",
        ),
    );
    caps.insert(
        NativeOperationId::PermissionOnce,
        verdict(
            once_ok,
            "POST .../permission/{requestID}/reply decision=once",
            "missing once decision or permission read",
        ),
    );
    caps.insert(
        NativeOperationId::PermissionReject,
        verdict(
            reject_ok,
            "POST .../permission/{requestID}/reply decision=reject",
            "missing reject decision or permission read",
        ),
    );
    caps.insert(
        NativeOperationId::AnswerGroup,
        verdict(
            answer_ok,
            "POST .../form/{formID}/reply (Form.Reply.answer) + form read",
            "missing Form.Reply.answer shape or form read",
        ),
    );
    let stop_ok = operation(openapi, INTERRUPT_PATH, "post").is_some_and(|op| {
        success_json_schema(op)
            .and_then(|schema| resolve(openapi, schema))
            .is_some_and(|schema| {
                schema
                    .get("properties")
                    .and_then(|p| p.get("interrupted"))
                    .and_then(|v| v.get("type"))
                    .and_then(Value::as_str)
                    == Some("boolean")
            })
    });
    caps.insert(
        NativeOperationId::StopSession,
        verdict(
            stop_ok,
            "POST .../interrupt returns {interrupted: boolean}",
            "missing session interrupt response",
        ),
    );
    let set_agent_ok = operation(openapi, AGENT_PATH, "post").is_some_and(|op| {
        request_schema(op)
            .and_then(|schema| schema.get("properties"))
            .is_some_and(|properties| properties.get("agent").is_some())
    });
    caps.insert(
        NativeOperationId::SetAgent,
        verdict(
            set_agent_ok,
            "POST .../agent accepts {agent}",
            "missing session agent switch shape",
        ),
    );
    // T7 尚未完成权限读取/准确映射/受控切换；always 属永久授权，按计划
    // “不顺带开放”关闭产品能力。保留键位为未放行，UI/调用方缺键即不可用。
    caps.insert(
        NativeOperationId::PermissionAlways,
        verdict(
            false,
            "T7 permission mapping is not implemented; permanent authorization is closed",
            "permission modes require controlled mapping",
        ),
    );
    let _ = AGENT_LIST_PATH;
    caps
}

/// 原生表单解析。未覆盖规则只标 [`FieldKind::Unsupported`]，整组由调用方判不可答。
///
/// 注意：[`FormSpec::is_answerable`] 只表达「字段层面可答」，**不含**生命周期状态——
/// `FormSpec` 刻意不带 `state`（计划定义即 `{id, fields, content_hash}`）。调用方必须先用
/// `PendingView::Form.state == "pending"` 过滤，再对通过过滤的表单调用本函数。
pub fn parse_form(value: &Value) -> Result<FormSpec, NativeError> {
    let root = value
        .get("data")
        .filter(|inner| inner.is_object())
        .unwrap_or(value);
    let id = root
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| NativeError::new(codes::OC_FORM_INVALID, "form id missing"))?;
    let raw_fields = root
        .get("fields")
        .and_then(Value::as_array)
        .ok_or_else(|| NativeError::new(codes::OC_FORM_INVALID, "form fields missing"))?;

    let mut fields = Vec::with_capacity(raw_fields.len());
    for raw in raw_fields {
        let mut field = parse_field(raw)?;
        if let Some(when) = raw.get("when") {
            field.when = parse_condition(when, &fields);
            if field.when.is_none() || !matches!(field.kind, FieldKind::Text { .. }) {
                field.kind = FieldKind::Unsupported;
            }
        }
        fields.push(field);
    }

    Ok(FormSpec {
        id: id.to_string(),
        content_hash: content_hash(id, &fields),
        fields,
    })
}

fn parse_field(raw: &Value) -> Result<FormField, NativeError> {
    let key = raw
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| NativeError::new(codes::OC_FORM_INVALID, "form field key missing"))?;
    let title = raw
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or(key)
        .to_string();
    // 题干正文（官方可选字段）：非字符串属未覆盖规则，与 required 同口径不猜。
    let (description, description_known) = match raw.get("description") {
        None | Some(Value::Null) => (None, true),
        Some(Value::String(text)) => (Some(text.clone()), true),
        Some(_) => (None, false),
    };
    // 原始三态：只有 bool 才是已声明；其它类型视为未覆盖规则。
    let (required, required_known) = match raw.get("required") {
        None => (None, true),
        Some(Value::Bool(flag)) => (Some(*flag), true),
        Some(_) => (None, false),
    };
    let kind = if required_known && description_known {
        parse_kind(raw)
    } else {
        FieldKind::Unsupported
    };
    Ok(FormField {
        key: key.to_string(),
        title,
        description,
        required,
        kind,
        when: None,
    })
}

// T3 仅证明单个 eq、前置必答且无条件的 string。其它形态继续只读。
fn parse_condition(raw: &Value, previous: &[FormField]) -> Option<super::types::FormCondition> {
    let conditions = raw.as_array()?;
    if conditions.len() != 1 {
        return None;
    }
    let condition = conditions[0].as_object()?;
    if condition.len() != 3 || condition.get("op")?.as_str()? != "eq" {
        return None;
    }
    let key = condition.get("key")?.as_str()?;
    let value = condition.get("value")?.as_str()?;
    let source = previous.iter().find(|field| field.key == key)?;
    if source.when.is_some()
        || source.required != Some(true)
        || !matches!(source.kind, FieldKind::Text { .. })
    {
        return None;
    }
    Some(super::types::FormCondition {
        key: key.to_string(),
        value: value.to_string(),
    })
}

fn parse_kind(raw: &Value) -> FieldKind {
    // hidden 不豁免必答，展示语义未定。
    if raw.get("hidden").and_then(Value::as_bool) == Some(true) {
        return FieldKind::Unsupported;
    }
    let custom = raw.get("custom").and_then(Value::as_bool).unwrap_or(false);

    match raw.get("type").and_then(Value::as_str) {
        Some("string") => {
            if has_any(
                raw,
                &["format", "minLength", "maxLength", "pattern", "default"],
            ) {
                return FieldKind::Unsupported;
            }
            match parse_options(raw.get("options")) {
                Some(options) => FieldKind::Text { options, custom },
                None => FieldKind::Unsupported,
            }
        }
        Some("multiselect") => {
            if has_any(raw, &["minItems", "maxItems", "default"]) {
                return FieldKind::Unsupported;
            }
            match parse_options(raw.get("options")) {
                Some(options) => FieldKind::Multi { options, custom },
                None => FieldKind::Unsupported,
            }
        }
        // number/integer/boolean/external 与未知类型：不猜。
        _ => FieldKind::Unsupported,
    }
}

fn parse_options(raw: Option<&Value>) -> Option<Vec<FormOption>> {
    let Some(raw) = raw else {
        return Some(Vec::new());
    };
    let items = raw.as_array()?;
    let mut options = Vec::with_capacity(items.len());
    for item in items {
        let value = item.get("value").and_then(Value::as_str)?;
        // 缺 label 时回退展示 value；提交值始终是原生 value。
        let label = item.get("label").and_then(Value::as_str).unwrap_or(value);
        let description = match item.get("description") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => Some(text.clone()),
            Some(_) => return None,
        };
        options.push(FormOption {
            value: value.to_string(),
            label: label.to_string(),
            description,
        });
    }
    Some(options)
}

/// 长度前缀编码（`len:bytes`）：字段内容自带分隔符时也不会跨字段边界碰撞。
fn hash_text(hasher: &mut Sha256, text: &str) {
    hasher.update(text.len().to_string().as_bytes());
    hasher.update(b":");
    hasher.update(text.as_bytes());
}

fn content_hash(id: &str, fields: &[FormField]) -> String {
    let mut hasher = Sha256::new();
    hash_text(&mut hasher, id);
    for field in fields {
        hash_text(&mut hasher, &field.key);
        hash_text(&mut hasher, &field.title);
        hash_text(
            &mut hasher,
            match field.required {
                None => "unset",
                Some(true) => "true",
                Some(false) => "false",
            },
        );
        hash_text(
            &mut hasher,
            &serde_json::to_string(&field.when).expect("condition serializes"),
        );
        match &field.kind {
            FieldKind::Text { options, custom } => {
                hash_text(&mut hasher, "text");
                hash_text(&mut hasher, if *custom { "1" } else { "0" });
                hash_options(&mut hasher, options);
            }
            FieldKind::Multi { options, custom } => {
                hash_text(&mut hasher, "multi");
                hash_text(&mut hasher, if *custom { "1" } else { "0" });
                hash_options(&mut hasher, options);
            }
            FieldKind::Unsupported => hash_text(&mut hasher, "unsupported"),
        }
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn hash_options(hasher: &mut Sha256, options: &[FormOption]) {
    for option in options {
        hash_text(hasher, &option.value);
        hash_text(hasher, &option.label);
        hash_text(hasher, option.description.as_deref().unwrap_or(""));
    }
}

fn has_any(raw: &Value, keys: &[&str]) -> bool {
    keys.iter().any(|key| raw.get(*key).is_some())
}

fn verdict(allowed: bool, present: &str, missing: &str) -> OperationAvailability {
    OperationAvailability {
        allowed,
        evidence: if allowed { present } else { missing }.to_string(),
        reason_code: (!allowed).then(|| codes::OC_CONTRACT_MISSING.to_string()),
    }
}

fn operation<'a>(doc: &'a Value, path: &str, method: &str) -> Option<&'a Value> {
    doc.get("paths")?.get(path)?.get(method)
}

fn schema_by_name<'a>(doc: &'a Value, name: &str) -> Option<&'a Value> {
    doc.get("components")?.get("schemas")?.get(name)
}

fn request_schema(op: &Value) -> Option<&Value> {
    op.get("requestBody")?
        .get("content")?
        .get("application/json")?
        .get("schema")
}

fn success_json_schema(op: &Value) -> Option<&Value> {
    op.get("responses")?
        .as_object()?
        .iter()
        .filter(|(status, _)| status.starts_with('2'))
        .find_map(|(_, response)| {
            response
                .get("content")?
                .get("application/json")?
                .get("schema")
        })
}

fn has_success_response(op: &Value) -> bool {
    op.get("responses")
        .and_then(Value::as_object)
        .is_some_and(|map| map.keys().any(|status| status.starts_with('2')))
}

/// `send` 的关联 id 缺口（按**消费字段**判定，与版本号无关）。
///
/// `client_message_id` 是 `Confirmed` 的唯一确定性依据：请求体必须接受 `id`
/// （有 `properties.id`，或未用 `additionalProperties: false` 封死），2xx 必须在
/// 顶层或 `data` 对象内回显 `id`（真实 2.0.22 使用 `data` 信封，`data` 可本地 `$ref`）。
fn correlation_id_gaps(doc: &Value) -> Vec<&'static str> {
    let prompt = operation(doc, PROMPT_PATH, "post");
    let request_accepts_id = prompt
        .and_then(request_schema)
        .and_then(|schema| resolve(doc, schema))
        .is_some_and(|schema| {
            schema
                .get("properties")
                .and_then(|properties| properties.get("id"))
                .is_some()
                || schema.get("additionalProperties") != Some(&Value::Bool(false))
        });
    let response_echoes_id = prompt
        .and_then(success_json_schema)
        .and_then(|schema| resolve(doc, schema))
        .is_some_and(|schema| {
            let properties = schema.get("properties");
            properties
                .and_then(|properties| properties.get("id"))
                .is_some()
                || properties
                    .and_then(|properties| properties.get("data"))
                    .and_then(|data| resolve(doc, data))
                    .and_then(|data| data.get("properties"))
                    .and_then(|properties| properties.get("id"))
                    .is_some()
        });

    let mut gaps = Vec::new();
    if !request_accepts_id {
        gaps.push("requestBody.properties.id");
    }
    if !response_echoes_id {
        gaps.push("2xx.properties.id");
    }
    gaps
}

/// 本地 `$ref` 解析：只认 `#/...` 形式的文档内指针（真实文档既可内联也可引用）。
/// 远程/相对引用（`other.json#/...`、`https://...`、`#anchor`）一律不可解析——
/// 不按最后一段猜名字，猜错会把不可用契约判成可用。
fn resolve<'a>(doc: &'a Value, schema: &'a Value) -> Option<&'a Value> {
    match schema.get("$ref") {
        None => Some(schema),
        Some(Value::String(reference)) => doc.pointer(reference.strip_prefix('#')?),
        Some(_) => None,
    }
}

/// 3.1 允许 `type: ["array"]`，数组也可能藏在 `anyOf`/`oneOf` 分支里。
fn is_array_schema(doc: &Value, schema: &Value) -> bool {
    let Some(schema) = resolve(doc, schema) else {
        return false;
    };
    let typed_array = match schema.get("type") {
        Some(Value::String(kind)) => kind == "array",
        Some(Value::Array(kinds)) => has_word(kinds, "array"),
        _ => false,
    };
    typed_array
        || ["anyOf", "oneOf"].iter().any(|key| {
            schema
                .get(*key)
                .and_then(Value::as_array)
                .is_some_and(|variants| {
                    variants.iter().any(|variant| is_array_schema(doc, variant))
                })
        })
}

/// 列表读：2xx JSON 响应体带 `data` 数组（本端消费的唯一形状）。
fn list_readable(doc: &Value, path: &str) -> bool {
    operation(doc, path, "get")
        .and_then(success_json_schema)
        .and_then(|schema| resolve(doc, schema))
        .and_then(|schema| schema.get("properties"))
        .and_then(|properties| properties.get("data"))
        .is_some_and(|data| is_array_schema(doc, data))
}

/// inbox 项上的 delivery 枚举（优先响应内联，回退具名组件）。
fn delivery_words(doc: &Value) -> Option<&Vec<Value>> {
    operation(doc, INBOX_PATH, "get")
        .and_then(success_json_schema)
        .and_then(|schema| resolve(doc, schema))
        .and_then(|schema| schema.get("properties"))
        .and_then(|properties| properties.get("data"))
        .and_then(|data| resolve(doc, data))
        .and_then(|data| data.get("items"))
        .and_then(|items| resolve(doc, items))
        .and_then(|items| items.get("properties"))
        .and_then(|properties| properties.get("delivery"))
        .and_then(|delivery| resolve(doc, delivery))
        .and_then(|delivery| delivery.get("enum"))
        .and_then(Value::as_array)
        .or_else(|| enum_words(doc, "Session.Inbox.Delivery"))
}

/// 审批决定词（优先 reply 请求体内联，回退具名组件）。
fn decision_words(doc: &Value) -> Option<&Vec<Value>> {
    operation(doc, PERMISSION_REPLY_PATH, "post")
        .and_then(request_schema)
        .and_then(|schema| resolve(doc, schema))
        .and_then(|schema| schema.get("properties"))
        .and_then(|properties| properties.get("decision"))
        .and_then(|decision| resolve(doc, decision))
        .and_then(|decision| decision.get("enum"))
        .and_then(Value::as_array)
        .or_else(|| enum_words(doc, "Permission.Reply"))
}

/// 词集合是否只含本端 `DeliveryKind` 认识的词（穷尽判断，不是「含有即可」）。
fn only_delivery_words(words: &[Value]) -> bool {
    words
        .iter()
        .all(|word| matches!(word.as_str(), Some("queue") | Some("steer")))
}

/// 出现 {queue,steer} 之外的词：运行期 `DeliveryKind` 反序列化会失败，不能当已知。
fn has_foreign_delivery_word(doc: &Value) -> bool {
    delivery_words(doc).is_some_and(|words| !only_delivery_words(words))
}

/// 表单详情必须能给出本端消费的 `id`（`parse_form` 必需）与 `fields`。
fn form_detail_known(doc: &Value) -> bool {
    schema_by_name(doc, "Form.Detail")
        .and_then(|s| s.get("properties"))
        .is_some_and(|properties| {
            properties.get("id").is_some() && properties.get("fields").is_some()
        })
}

/// 审批请求必须给出视图实际消费的 `id` 与 `action`；`resources` 可选，存在时必须是数组。
fn permission_request_known(doc: &Value) -> bool {
    schema_by_name(doc, "Permission.Request")
        .and_then(|s| s.get("properties"))
        .is_some_and(|properties| {
            let resources_ok = match properties.get("resources") {
                None => true,
                Some(schema) => is_array_schema(doc, schema),
            };
            properties.get("id").is_some() && properties.get("action").is_some() && resources_ok
        })
}

fn enum_words<'a>(doc: &'a Value, name: &str) -> Option<&'a Vec<Value>> {
    schema_by_name(doc, name)?.get("enum")?.as_array()
}

fn has_word(values: &[Value], word: &str) -> bool {
    values.iter().any(|value| value.as_str() == Some(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use serde_json::{json, Value};

    use crate::remote::content::SessionMessage;
    use crate::services::opencode_native::types::{
        codes, AnswerPayload, DecidePayload, DeliveryKind, DraftAnswer, FieldKind, InboxView,
        NativeCommand, NativeOperation, NativeOperationId, OperationAvailability, PendingView,
        PublicNativeView, RetractPayload, RuntimeKey, SendPayload, ServiceDescriptor,
    };

    /// 金样读取器：仅本测试模块使用，不作为生产公共接口。
    fn fixture(name: &str) -> Value {
        let path = format!(
            "{}/tests/fixtures/opencode-native/{}",
            env!("CARGO_MANIFEST_DIR"),
            name
        );
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
    }

    fn allowed(caps: &Capabilities, id: NativeOperationId) -> bool {
        caps[&id].allowed
    }

    /// T12：脱敏 Windows 2.2.2 原始 OpenAPI 进入生产契约判定，不代表平台准入。
    #[test]
    fn t12_windows_real_contract_replay() {
        let caps = evaluate_contract(&fixture("windows-openapi-2.2.2.json"));
        for id in [
            NativeOperationId::ReadMessages,
            NativeOperationId::Send,
            NativeOperationId::AnswerGroup,
            NativeOperationId::PermissionOnce,
        ] {
            assert!(allowed(&caps, id), "{id:?}");
        }
    }

    /// T3 当前真服务 OpenAPI：走生产判定器，版本/附加字段不冒充语义能力。
    #[test]
    fn product_real_225_contract_is_version_independent_and_locally_restricted() {
        let doc = fixture("product-openapi-2.2.5.json");
        let baseline = evaluate_contract(&doc);
        assert!(allowed(&baseline, NativeOperationId::ReadMessages));
        assert!(allowed(&baseline, NativeOperationId::Send));
        assert!(allowed(&baseline, NativeOperationId::AnswerGroup));
        let mut newer = doc.clone();
        newer["info"]["version"] = json!("99.0.0");
        newer["x-future-unconsumed"] = json!({"feature": true});
        assert_eq!(baseline, evaluate_contract(&newer));

        let mut missing = doc.clone();
        missing["components"]["schemas"]
            .as_object_mut()
            .unwrap()
            .remove("Form.Reply");
        let restricted = evaluate_contract(&missing);
        assert!(!allowed(&restricted, NativeOperationId::AnswerGroup));
        assert!(allowed(&restricted, NativeOperationId::ReadMessages));
        assert!(allowed(&restricted, NativeOperationId::Send));
        assert!(allowed(&restricted, NativeOperationId::PermissionOnce));

        let mut unknown = doc;
        unknown["components"]["schemas"]["Session.Inbox.Delivery"]["enum"] =
            json!(["queue", "steer", "future-delivery"]);
        let restricted = evaluate_contract(&unknown);
        assert!(!allowed(&restricted, NativeOperationId::Send));
        assert!(!allowed(&restricted, NativeOperationId::ReadInbox));
        assert!(allowed(&restricted, NativeOperationId::ReadMessages));
    }

    /// T3 真服务单 eq 条件题可答；变异未知类型继续拒答，不冒充新上游实测。
    #[test]
    fn product_real_conditional_form_supported_and_unknown_field_remains_unanswerable() {
        let raw = fixture("product-form-conditional.json");
        let form = parse_form(&raw).unwrap();
        assert_eq!(form.fields.len(), 2);
        assert_eq!(form.fields[0].required, Some(true));
        assert!(form.fields[1].when.is_some());
        assert!(form.is_answerable());
        let mut mutated = raw;
        mutated["data"]["fields"][0]["type"] = json!("future-type");
        assert!(matches!(
            parse_form(&mutated).unwrap().fields[0].kind,
            FieldKind::Unsupported
        ));
        mutated["data"].as_object_mut().unwrap().remove("fields");
        assert!(parse_form(&mutated).is_err());
    }

    /// 只改版本号与无关描述，消费的路径/形状不变 → 所有能力结论必须不变。
    #[test]
    fn same_consumed_contract_new_version_remains_usable() {
        let baseline = evaluate_contract(&fixture("openapi.contract.json"));
        assert!(
            allowed(&baseline, NativeOperationId::Send),
            "金样基线必须放行 send"
        );

        let mut bumped = fixture("openapi.contract.json");
        bumped["info"]["version"] = json!("9.9.9");
        bumped["info"]["description"] = json!("future build, unrelated wording");
        bumped["paths"]["/api/session/{sessionID}/prompt"]["post"]["description"] =
            json!("admit text, prose only");

        let after = evaluate_contract(&bumped);
        assert!(
            allowed(&after, NativeOperationId::Send),
            "版本号变化不得关闭 send"
        );
        assert_eq!(baseline, after, "只改版本/无关描述不得改变任何能力");
    }

    /// 缺 reply 形状只关答题写能力，不得连带关只读（消息/待答/审批）。
    #[test]
    fn missing_reply_shape_closes_answer_only() {
        let mut doc = fixture("openapi.contract.json");
        doc["components"]["schemas"]
            .as_object_mut()
            .unwrap()
            .remove("Form.Reply");

        let caps = evaluate_contract(&doc);
        assert!(
            !allowed(&caps, NativeOperationId::AnswerGroup),
            "缺 Form.Reply 必须关闭整组答题"
        );
        assert_eq!(
            caps[&NativeOperationId::AnswerGroup].reason_code.as_deref(),
            Some(codes::OC_CONTRACT_MISSING),
            "关闭原因必须是受控错误码"
        );
        assert!(
            allowed(&caps, NativeOperationId::ReadMessages),
            "局部不兼容不影响其余读能力"
        );
        assert!(allowed(&caps, NativeOperationId::ReadPending));
        assert!(allowed(&caps, NativeOperationId::PermissionOnce));
        assert!(allowed(&caps, NativeOperationId::PermissionReject));
        assert!(allowed(&caps, NativeOperationId::Send));
    }

    /// when / hidden / format 都是未覆盖规则：字段必须标 Unsupported，整份表单不可答。
    #[test]
    fn unknown_form_constraint_is_not_dropped() {
        let form = parse_form(&fixture("form.unsupported.json")).expect("表单仍可解析用于只读展示");

        let unsupported: Vec<&str> = form
            .fields
            .iter()
            .filter(|f| f.kind == FieldKind::Unsupported)
            .map(|f| f.key.as_str())
            .collect();
        assert_eq!(
            unsupported,
            ["q1", "q2", "q3"],
            "when/hidden/format 不得被静默丢弃或降级成 Text"
        );
        assert_eq!(form.fields.len(), 4, "未覆盖字段仍在结果里可见");
        assert!(
            matches!(form.fields[0].kind, FieldKind::Text { .. }),
            "可覆盖字段保持可答类型"
        );
        assert!(!form.is_answerable(), "含未覆盖规则的表单不能当成可答");
        assert_eq!(
            form.unsupported_error().map(|e| e.code),
            Some(codes::OC_FORM_UNSUPPORTED.to_string())
        );
    }

    /// 选项展示 label、提交 value；label 不得覆盖原生 value。
    #[test]
    fn option_label_does_not_replace_native_value() {
        let form = parse_form(&fixture("form.options.label.json")).expect("选项夹具可解析");
        let FieldKind::Text { options, custom } = &form.fields[0].kind else {
            panic!("q0 应为文本选择题");
        };
        assert_eq!(options[0].value, "q0-red");
        assert_eq!(options[0].label, "Red");
        assert_ne!(options[0].value, options[0].label, "提交值必须是原生 value");
        assert_eq!(options[0].description.as_deref(), Some("warm"));
        assert!(options[1].description.is_none());
        assert!(*custom);
    }

    /// 字段题干正文：官方 `Form.StringField` / `Form.MultiselectField` 的 `description`
    /// 才是实际题干（`title` 只是短标题）。它必须穿透解析与序列化；同时**不参与**
    /// `content_hash`：老载荷（没有该键）的历史 hash 是身份锚，不能因本端多读一个键而换代。
    ///
    /// 旧 hash 字面量 = 本夹具去掉全部 `description` 后、改动前 `parse_form` 的输出；
    /// 该断言把「无 description 保持旧 hash」钉死在测试里，防止后续重构顺手把题干塞进指纹。
    #[test]
    fn field_description_surfaces_without_changing_content_hash() {
        let with_description = fixture("form.pending.json");
        let form = parse_form(&with_description).expect("金样可解析");
        assert_eq!(form.fields[0].description.as_deref(), Some("Pick a color"));
        assert!(form.fields[1].description.is_none(), "未声明 description = None");
        assert!(form.fields[2].description.is_none());

        let mut legacy = with_description.clone();
        for field in legacy["data"]["fields"].as_array_mut().unwrap() {
            field.as_object_mut().unwrap().remove("description");
        }
        let legacy_form = parse_form(&legacy).expect("老载荷可解析");
        assert_eq!(
            form.content_hash, legacy_form.content_hash,
            "description 不参与内容指纹：有无该键必须同 hash"
        );
        assert_eq!(
            form.content_hash, "4930a121dab12a31f8345f9dd450453686d6b1fe23132504cdc500bef19acf45",
            "老载荷的历史 hash 必须原样保持"
        );

        // 序列化穿透：手机端要能读到题干；无 description 的字段不得凭空出现该键。
        let rendered = serde_json::to_value(&form.fields[0]).unwrap();
        assert_eq!(rendered["description"], "Pick a color");
        let absent = serde_json::to_value(&form.fields[1]).unwrap();
        assert!(
            absent.get("description").is_none(),
            "缺省不序列化，旧载荷形状保持不变"
        );

        // multiselect 同口径（官方 Form.MultiselectField 也有 description）。
        let multi = parse_form(&json!({
            "data": {
                "id": "frm_multi_desc",
                "fields": [{
                    "key": "q0",
                    "title": "TAGS",
                    "description": "Pick tags",
                    "type": "multiselect",
                    "options": [{ "value": "a", "label": "A" }]
                }]
            }
        }))
        .expect("多选表单可解析");
        assert_eq!(multi.fields[0].description.as_deref(), Some("Pick tags"));

        // 非字符串 description 与 required 非 bool 同口径：未覆盖规则，不猜。
        let mut malformed = with_description;
        malformed["data"]["fields"][0]["description"] = json!(7);
        assert!(
            !parse_form(&malformed).unwrap().is_answerable(),
            "非字符串题干不得被当成可答"
        );
    }

    /// 公开视图序列化不得出现认证字段（键集固定）。
    #[test]
    fn credentials_never_appear_in_public_view_serialization() {
        let view = sample_view();
        let json = serde_json::to_value(&view).unwrap();

        let mut keys = Vec::new();
        collect_keys(&json, &mut keys);
        for key in &keys {
            let lower = key.to_ascii_lowercase();
            for forbidden in ["password", "authorization", "basic"] {
                assert!(
                    !lower.contains(forbidden),
                    "公开视图不得序列化认证字段: {key}"
                );
            }
        }

        let top: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            top,
            [
                "messageIds",
                "sessionId",
                "serviceId",
                "servicePid",
                "serviceUrl",
                "directory",
                "bindingRequired",
                "desktopBound",
                "bindingToken",
                "revision",
                "complete",
                "capabilities",
                "messages",
                "pending",
                "inbox",
                "runtimeState",
                "permissionRules",
                "permissionRulesKnown",
                "files",
                "toolCallIds"
            ]
        );
        assert!(
            !json.to_string().contains("resources"),
            "不透传 raw permission"
        );
    }

    /// R5：完整性标记必须真的出现在公开 JSON 里（手机端据它显示不完整提示）。
    #[test]
    fn public_view_exposes_complete() {
        let mut view = sample_view();
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["complete"], true, "完整投影序列化为 complete=true");

        view.complete = false;
        let partial = serde_json::to_value(&view).unwrap();
        assert_eq!(
            partial["complete"], false,
            "部分历史必须序列化为 complete=false"
        );
    }

    /// 认证值不派生 Debug：日志/诊断打码。
    #[test]
    fn service_descriptor_debug_redacts_password() {
        let descriptor = ServiceDescriptor {
            runtime: RuntimeKey {
                service_id: "87bd7e45-27bf-490d-bced-58d79c108910".into(),
                pid: 60541,
                started_at_ms: 1_793_289_345_000,
            },
            endpoint: reqwest::Url::parse("http://127.0.0.1:49617").unwrap(),
            password: "s3cret-credential-value".into(),
            version: "2.0.22".into(),
            explicit: false,
        };
        let rendered = format!("{descriptor:?}");
        assert!(
            !rendered.contains("s3cret-credential-value"),
            "凭据不得进 Debug"
        );
        assert!(rendered.contains("redacted"));
    }

    /// HTTP 命令：camelCase、kind 标签、skip 无 value、拒绝未知字段。
    #[test]
    fn native_command_serialization_is_camel_and_strict() {
        let command = NativeCommand {
            action_id: "act_1".into(),
            session_id: "ses_1".into(),
            binding_token: "bind_1".into(),
            operation: NativeOperation::Send(SendPayload {
                text: "hi".into(),
                attachment_paths: Vec::new(),
                delivery: DeliveryKind::Queue,
                client_message_id: "msg_act_1".into(),
            }),
        };
        let json = serde_json::to_value(&command).unwrap();
        assert_eq!(json["actionId"], "act_1");
        assert_eq!(json["bindingToken"], "bind_1");
        assert_eq!(json["operation"]["kind"], "send");
        assert_eq!(json["operation"]["clientMessageId"], "msg_act_1");
        assert_eq!(json["operation"]["attachmentPaths"], json!([]));
        assert_eq!(json["operation"]["delivery"], "queue");
        // 形状锁定：内部标签 + newtype struct 载荷必须扁平化，不能出现嵌套对象。
        assert_eq!(
            json["operation"],
            json!({
                "kind": "send",
                "text": "hi",
                "attachmentPaths": [],
                "delivery": "queue",
                "clientMessageId": "msg_act_1"
            }),
            "operation 必须保持扁平结构"
        );

        assert_eq!(
            serde_json::to_value(DraftAnswer::Skip).unwrap(),
            json!({"kind": "skip"})
        );
        assert_eq!(
            serde_json::to_value(DraftAnswer::Text("blue".into())).unwrap(),
            json!({"kind": "text", "value": "blue"})
        );
        assert_eq!(
            serde_json::to_value(DraftAnswer::Multi(vec!["a".into()])).unwrap(),
            json!({"kind": "multi", "value": ["a"]})
        );

        let mut unknown = json;
        unknown["endpoint"] = json!("http://evil.invalid");
        assert!(
            serde_json::from_value::<NativeCommand>(unknown).is_err(),
            "HTTP 命令必须拒绝未知字段"
        );
    }

    /// 每个变体都要拒绝自身未知字段；顶层未知字段同样拒绝（deny_unknown_fields 必须落到变体）。
    #[test]
    fn native_operation_rejects_unknown_fields() {
        let cases = [
            (
                "send",
                json!({
                    "kind": "send",
                    "text": "hi",
                    "attachmentPaths": [],
                    "delivery": "queue",
                    "clientMessageId": "msg_act_1"
                }),
            ),
            (
                "answer",
                json!({
                    "kind": "answer",
                    "formId": "frm_1",
                    "contextToken": "ctx_1",
                    "answers": { "q0": { "kind": "skip" } }
                }),
            ),
            (
                "decide",
                json!({
                    "kind": "decide",
                    "requestId": "per_1",
                    "contextToken": "ctx_1",
                    "decision": "once"
                }),
            ),
            ("retract", json!({ "kind": "retract", "itemId": "msg_1" })),
        ];
        for (name, value) in cases {
            serde_json::from_value::<NativeOperation>(value.clone())
                .unwrap_or_else(|e| panic!("{name} 合法形状应可反序列化: {e}"));
            let mut bogus = value;
            bogus["bogus"] = json!(1);
            assert!(
                serde_json::from_value::<NativeOperation>(bogus).is_err(),
                "{name} 变体内的未知字段必须拒绝"
            );
        }

        let mut top_level = json!({
            "actionId": "act_1",
            "sessionId": "ses_1",
            "bindingToken": "bind_1",
            "operation": { "kind": "retract", "itemId": "msg_1" }
        });
        assert!(serde_json::from_value::<NativeCommand>(top_level.clone()).is_ok());
        top_level["endpoint"] = json!("http://evil.invalid");
        assert!(
            serde_json::from_value::<NativeCommand>(top_level).is_err(),
            "顶层未知字段必须拒绝"
        );
    }

    /// 载荷结构体化后序列化形状不变（内部标签 + newtype struct 扁平化）。
    #[test]
    fn native_operation_round_trips_flat_shape() {
        let cases = [
            (
                NativeOperation::Send(SendPayload {
                    text: "hi".into(),
                    attachment_paths: vec!["/tmp/a.png".into()],
                    delivery: DeliveryKind::Queue,
                    client_message_id: "msg_act_1".into(),
                }),
                json!({
                    "kind": "send",
                    "text": "hi",
                    "attachmentPaths": ["/tmp/a.png"],
                    "delivery": "queue",
                    "clientMessageId": "msg_act_1"
                }),
            ),
            (
                NativeOperation::Answer(AnswerPayload {
                    form_id: "frm_1".into(),
                    context_token: "ctx_1".into(),
                    answers: BTreeMap::from([("q0".to_string(), DraftAnswer::Text("blue".into()))]),
                }),
                json!({
                    "kind": "answer",
                    "formId": "frm_1",
                    "contextToken": "ctx_1",
                    "answers": { "q0": { "kind": "text", "value": "blue" } }
                }),
            ),
            (
                NativeOperation::Decide(DecidePayload {
                    request_id: "per_1".into(),
                    context_token: "ctx_1".into(),
                    decision: "once".into(),
                }),
                json!({
                    "kind": "decide",
                    "requestId": "per_1",
                    "contextToken": "ctx_1",
                    "decision": "once"
                }),
            ),
            (
                NativeOperation::Retract(RetractPayload {
                    item_id: "msg_1".into(),
                }),
                json!({ "kind": "retract", "itemId": "msg_1" }),
            ),
        ];
        for (operation, expected) in cases {
            let json = serde_json::to_value(&operation).unwrap();
            assert_eq!(json, expected, "扁平序列化形状不得改变");
            let back: NativeOperation = serde_json::from_value(json).unwrap();
            assert_eq!(back, operation);
        }
    }

    /// 关联 id 是 Confirmed 的唯一确定性依据：缺请求 id 或缺响应回显都必须关 send。
    #[test]
    fn send_requires_correlation_id() {
        let baseline = evaluate_contract(&fixture("openapi.contract.json"));
        assert!(
            allowed(&baseline, NativeOperationId::Send),
            "金样两处都满足 id 能力，必须仍放行"
        );

        let mut no_request_id = fixture("openapi.contract.json");
        no_request_id["paths"]["/api/session/{sessionID}/prompt"]["post"]["requestBody"]["content"]
            ["application/json"]["schema"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("id");
        let caps = evaluate_contract(&no_request_id);
        assert!(
            !allowed(&caps, NativeOperationId::Send),
            "请求体不接受 id 必须关 send"
        );
        assert_eq!(
            caps[&NativeOperationId::Send].reason_code.as_deref(),
            Some(codes::OC_CONTRACT_ID_UNSUPPORTED),
            "关闭原因必须是 id 不支持"
        );
        assert!(
            caps[&NativeOperationId::Send]
                .evidence
                .contains("requestBody.properties.id"),
            "证据必须指出缺哪一项: {}",
            caps[&NativeOperationId::Send].evidence
        );
        assert!(
            allowed(&caps, NativeOperationId::ReadMessages),
            "缺 id 能力不得连带关只读"
        );

        let mut no_echo = fixture("openapi.contract.json");
        no_echo["paths"]["/api/session/{sessionID}/prompt"]["post"]["responses"]["200"]
            ["content"]["application/json"]["schema"] = json!({
            "type": "object",
            "properties": {
                "sessionID": { "type": "string" },
                "payload": { "type": "object" }
            }
        });
        let caps = evaluate_contract(&no_echo);
        assert!(
            !allowed(&caps, NativeOperationId::Send),
            "2xx 不回显 id 必须关 send"
        );
        assert_eq!(
            caps[&NativeOperationId::Send].reason_code.as_deref(),
            Some(codes::OC_CONTRACT_ID_UNSUPPORTED)
        );
        assert!(
            caps[&NativeOperationId::Send]
                .evidence
                .contains("2xx.properties.id"),
            "证据必须指出缺哪一项: {}",
            caps[&NativeOperationId::Send].evidence
        );
        assert!(allowed(&caps, NativeOperationId::ReadMessages));
    }

    /// 真实 2.0.22：prompt 成功响应把回显 id 包在 `data` 内（`data` 可本地 `$ref`）。
    #[test]
    fn prompt_response_id_inside_data_envelope_counts() {
        let mut doc = fixture("openapi.contract.json");
        doc["components"]["schemas"]["PromptResult"] = json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "pattern": "^msg_" },
                "sessionID": { "type": "string" }
            },
            "required": ["id", "sessionID"],
            "additionalProperties": false
        });
        doc["paths"]["/api/session/{sessionID}/prompt"]["post"]["responses"]["200"]["content"]
            ["application/json"]["schema"] = json!({
            "type": "object",
            "properties": {
                "data": { "$ref": "#/components/schemas/PromptResult" }
            },
            "required": ["data"],
            "additionalProperties": false
        });

        let caps = evaluate_contract(&doc);
        assert!(
            allowed(&caps, NativeOperationId::Send),
            "data.id 是真实回显形状，必须放行 send: {}",
            caps[&NativeOperationId::Send].evidence
        );
        assert_eq!(caps[&NativeOperationId::Send].reason_code, None);
        assert!(
            !caps[&NativeOperationId::Send]
                .evidence
                .contains("2xx.properties.id"),
            "不得残留 2xx.properties.id 缺口: {}",
            caps[&NativeOperationId::Send].evidence
        );
    }

    /// 顶层内联 `properties.id` 仍算回显（既有契约形状，不得被 data 分支破坏）。
    #[test]
    fn prompt_response_flat_id_counts() {
        let mut doc = fixture("openapi.contract.json");
        doc["paths"]["/api/session/{sessionID}/prompt"]["post"]["responses"]["200"]["content"]
            ["application/json"]["schema"] = json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "pattern": "^msg_" },
                "sessionID": { "type": "string" }
            }
        });

        let caps = evaluate_contract(&doc);
        assert!(
            allowed(&caps, NativeOperationId::Send),
            "顶层 id 必须是有效回显: {}",
            caps[&NativeOperationId::Send].evidence
        );
        assert!(
            !caps[&NativeOperationId::Send]
                .evidence
                .contains("2xx.properties.id"),
            "顶层 id 不得报缺口: {}",
            caps[&NativeOperationId::Send].evidence
        );
    }

    /// 顶层与 `data` 内都没有 id ⇒ 缺口仍在，send 保持关闭。
    #[test]
    fn prompt_response_without_flat_or_data_id_keeps_gap() {
        let mut doc = fixture("openapi.contract.json");
        doc["paths"]["/api/session/{sessionID}/prompt"]["post"]["responses"]["200"]["content"]
            ["application/json"]["schema"] = json!({
            "type": "object",
            "properties": {
                "data": {
                    "type": "object",
                    "properties": {
                        "sessionID": { "type": "string" }
                    }
                }
            }
        });

        let caps = evaluate_contract(&doc);
        assert!(
            !allowed(&caps, NativeOperationId::Send),
            "两处都没有 id 必须关 send"
        );
        assert_eq!(
            caps[&NativeOperationId::Send].reason_code.as_deref(),
            Some(codes::OC_CONTRACT_ID_UNSUPPORTED)
        );
        assert!(
            caps[&NativeOperationId::Send]
                .evidence
                .contains("2xx.properties.id"),
            "证据必须指出缺哪一项: {}",
            caps[&NativeOperationId::Send].evidence
        );
    }

    /// delivery 词集合必须穷尽：出现第三种词 ⇒ 运行期 DeliveryKind 反序列化会失败。
    #[test]
    fn foreign_delivery_word_closes_send() {
        let mut doc = fixture("openapi.contract.json");
        doc["components"]["schemas"]["Session.Inbox.Delivery"]["enum"] =
            json!(["queue", "steer", "broadcast"]);

        let caps = evaluate_contract(&doc);
        assert!(
            !allowed(&caps, NativeOperationId::Send),
            "第三种 delivery 词必须关闭 send"
        );
        assert!(
            !allowed(&caps, NativeOperationId::ReadInbox),
            "inbox 项反序列化会失败，必须关闭只读 inbox"
        );
        assert!(allowed(&caps, NativeOperationId::ReadMessages));
    }

    /// OpenAPI 3.1：`type: ["array"]` 与 anyOf/oneOf 里的数组 schema 都算可读。
    #[test]
    fn array_schema_variants_are_readable() {
        fn message_data(doc: &mut Value) -> &mut Value {
            &mut doc["paths"]["/api/session/{sessionID}/message"]["get"]["responses"]["200"]
                ["content"]["application/json"]["schema"]["properties"]["data"]
        }

        let mut typed = fixture("openapi.contract.json");
        *message_data(&mut typed) =
            json!({ "type": ["array", "null"], "items": { "type": "object" } });
        assert!(
            allowed(&evaluate_contract(&typed), NativeOperationId::ReadMessages),
            "type: [array, null] 必须可读"
        );

        let mut any_of = fixture("openapi.contract.json");
        *message_data(&mut any_of) = json!({
            "anyOf": [
                { "type": "array", "items": { "type": "object" } },
                { "type": "null" }
            ]
        });
        assert!(
            allowed(&evaluate_contract(&any_of), NativeOperationId::ReadMessages),
            "anyOf 内的数组分支必须可读"
        );

        let mut one_of = fixture("openapi.contract.json");
        *message_data(&mut one_of) = json!({
            "oneOf": [{ "$ref": "#/components/schemas/MessageList" }]
        });
        one_of["components"]["schemas"]["MessageList"] =
            json!({ "type": "array", "items": { "type": "object" } });
        assert!(
            allowed(&evaluate_contract(&one_of), NativeOperationId::ReadMessages),
            "oneOf 内经 $ref 的数组分支必须可读"
        );

        let mut scalar = fixture("openapi.contract.json");
        *message_data(&mut scalar) = json!({ "type": "object" });
        assert!(
            !allowed(&evaluate_contract(&scalar), NativeOperationId::ReadMessages),
            "非数组形状必须关闭只读"
        );
    }

    /// 表单详情缺 id、审批请求缺 id/action 或 resources 非数组都必须关对应能力。
    #[test]
    fn pending_read_requires_consumed_detail_fields() {
        let mut no_form_id = fixture("openapi.contract.json");
        no_form_id["components"]["schemas"]["Form.Detail"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("id");
        let caps = evaluate_contract(&no_form_id);
        assert!(
            !allowed(&caps, NativeOperationId::ReadPending),
            "Form.Detail 缺 id 必须关待处理读"
        );
        assert!(!allowed(&caps, NativeOperationId::AnswerGroup));

        let mut no_action = fixture("openapi.contract.json");
        no_action["components"]["schemas"]["Permission.Request"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("action");
        let caps = evaluate_contract(&no_action);
        assert!(
            !allowed(&caps, NativeOperationId::ReadPending),
            "Permission.Request 缺 action 必须关待处理读"
        );
        assert!(!allowed(&caps, NativeOperationId::PermissionOnce));
        assert!(!allowed(&caps, NativeOperationId::PermissionReject));

        let mut no_permission_id = fixture("openapi.contract.json");
        no_permission_id["components"]["schemas"]["Permission.Request"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("id");
        assert!(!allowed(
            &evaluate_contract(&no_permission_id),
            NativeOperationId::ReadPending
        ));

        let mut bad_resources = fixture("openapi.contract.json");
        bad_resources["components"]["schemas"]["Permission.Request"]["properties"]["resources"] =
            json!({ "type": "string" });
        assert!(
            !allowed(
                &evaluate_contract(&bad_resources),
                NativeOperationId::ReadPending
            ),
            "resources 存在时必须是数组"
        );

        let mut no_resources = fixture("openapi.contract.json");
        no_resources["components"]["schemas"]["Permission.Request"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("resources");
        assert!(
            allowed(
                &evaluate_contract(&no_resources),
                NativeOperationId::ReadPending
            ),
            "resources 可选，缺失不得关能力"
        );
    }

    /// content_hash 用长度前缀编码：字段内容含分隔符也不得碰撞。
    #[test]
    fn content_hash_separator_ambiguity_does_not_collide() {
        let split_key = json!({
            "id": "frm_1",
            "fields": [{ "key": "a\tb", "title": "c", "type": "string" }]
        });
        let split_title = json!({
            "id": "frm_1",
            "fields": [{ "key": "a", "title": "b\tc", "type": "string" }]
        });
        let left = parse_form(&split_key).unwrap();
        let right = parse_form(&split_title).unwrap();
        assert_ne!(
            left.content_hash, right.content_hash,
            "分隔符在字段间移动不得产生同一 hash"
        );

        let again = parse_form(&split_key).unwrap();
        assert_eq!(left.content_hash, again.content_hash, "同内容必须同 hash");
    }

    /// `$ref` 只认文档内 `#/...`；远程/相对引用不可解析，必须关对应能力。
    #[test]
    fn non_local_ref_is_unresolvable() {
        let mut remote = fixture("openapi.contract.json");
        remote["paths"]["/api/session/{sessionID}/message"]["get"]["responses"]["200"]["content"]
            ["application/json"]["schema"] = json!({
            "$ref": "https://example.invalid/openapi.json#/components/schemas/Messages"
        });
        assert!(
            !allowed(&evaluate_contract(&remote), NativeOperationId::ReadMessages),
            "远程 $ref 不得被解析"
        );

        let mut relative = fixture("openapi.contract.json");
        relative["paths"]["/api/session/{sessionID}/message"]["get"]["responses"]["200"]
            ["content"]["application/json"]["schema"] =
            json!({ "$ref": "components/schemas/Session.Inbox.User" });
        assert!(
            !allowed(
                &evaluate_contract(&relative),
                NativeOperationId::ReadMessages
            ),
            "缺 #/ 前缀的引用不得按尾段猜名字"
        );
    }

    fn sample_view() -> PublicNativeView {
        let permission = fixture("permission.pending.json");
        let item = &permission["data"][0];
        let form = parse_form(&fixture("form.pending.json")).unwrap();

        let mut capabilities = BTreeMap::new();
        for id in NativeOperationId::ALL {
            capabilities.insert(
                id,
                OperationAvailability {
                    allowed: true,
                    evidence: "fixture".into(),
                    reason_code: None,
                },
            );
        }

        PublicNativeView {
            message_ids: BTreeMap::new(),
            session_id: "ses_e77a75f50ffe4ZcyQUoGpdpdpK".into(),
            model: None,
            service_id: Some("svc-fixture".into()),
            service_pid: Some(1),
            service_url: Some("http://127.0.0.1:1".into()),
            directory: Some("/tmp/project".into()),
            binding_required: true,
            desktop_bound: false,
            binding_token: "bind_token_fixture".into(),
            revision: 7,
            partial_reason: None,
            complete: true,
            capabilities,
            messages: vec![SessionMessage {
                seq: 0,
                role: "user".into(),
                kind: "user".into(),
                content: "hello".into(),
                ts: Some(1_793_289_227_200),
                tool_name: None,
                tool_args: None,
                collapsed: false,
            }],
            pending: vec![
                PendingView::Permission {
                    id: item["id"].as_str().unwrap().into(),
                    title: format!(
                        "{} {}",
                        item["action"].as_str().unwrap(),
                        item["resources"][0].as_str().unwrap()
                    ),
                    decisions: vec!["once".into(), "reject".into()],
                    context_token: "ctx_perm".into(),
                },
                PendingView::Form {
                    id: form.id.clone(),
                    title: "G2-fixture-required".into(),
                    fields: form.fields,
                    context_token: "ctx_form".into(),
                    state: "pending".into(),
                },
            ],
            inbox: vec![InboxView {
                id: "msg_1885e0f70001tnqXIovOVgHuZh".into(),
                text: "FIXTURE-QUEUED-ITEM".into(),
                delivery: DeliveryKind::Queue,
                state: "waiting".into(),
            }],
            runtime_state: "ready".into(),
            permission_rules: Vec::new(),
            permission_rules_known: true,
            files: BTreeMap::new(),
            tool_call_ids: BTreeMap::new(),
            question_answers: BTreeMap::new(),
        }
    }

    fn collect_keys(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, inner) in map {
                    out.push(key.clone());
                    collect_keys(inner, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    collect_keys(item, out);
                }
            }
            _ => {}
        }
    }
}
