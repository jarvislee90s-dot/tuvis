//! Codex Desktop IPC 集成测试共用夹具构造（2026-10-18 真机形态适配批）。
//!
//! 职责：把各集成套件**沿用至今的旧手写实体模型**（`type:"turn"` + items 字符串
//! id 表 + entries 字符串数组 + turnHistory 层 isComplete）机械转换为 **2026-10-18
//! 真机金样形态**（`observed/` 实证）：实体按合成键索引、turn 实体无 `type` 以
//! `turnId` 字段标识、islands.entries 为 `{key,value}` 对象、items 为内联
//! camelCase 对象、isComplete 在 history 层。
//!
//! 为什么是转换器而不是逐点重写：脚本面（ScriptedPeer）只关心时序与状态机语义，
//! 不关心实体载体形态——转换器让全部既有脚本一次性获得真实形态，杜绝「一半新一半
//! 旧」的混合形态再次假绿。**新增脚本请直接构造真实形态**（参考
//! `tests/fixtures/codex-desktop-ipc/observed/`），不要依赖本转换器。

use serde_json::{json, Map, Value};

/// 旧模型 → 真实 canonical 形态。输入为旧手写 entitiesByKey（turn/item 实体平铺，
/// `type` 字段区分）与 islands 的字符串 entries 数组；输出 (entitiesByKey, islands)。
///
/// 转换规则（与金样逐条对齐）：
/// * `type=="turn"` 的实体 → 键改 `turn:<id>`、去 `type`、补 `turnId` 字段、
///   `items` 内联化（字符串 id → 被引用 item 实体的内联对象，camelCase 映射）；
/// * `user_message` → `userMessage`（`text` → `content:[{type:"text",text}]`）；
///   `agent_message` → `agentMessage`（保留 `text`）；其余 item 类型原样内联
///   （安全摘要语义由 mirror 投影层负责，夹具不裁剪）；
/// * islands entries：字符串 id → `{key,value}`（key=value=`turn:<id>`）；
/// * 未被 turn 引用的 item 实体丢弃（旧模型里它们只作被引用的正文载体）。
pub fn to_canonical(entities: Value, turn_ids: Value) -> (Value, Value) {
    let mut old = entities.as_object().cloned().unwrap_or_else(Map::new);
    // 兼容两种调用形态：字符串 id 数组 `["t1", "t2"]`，或旧 islands 字面量
    // `[{ "entries": ["t1", "t2"] }]`（机械改造点传入的是后者的原样文本）
    let mut ids: Vec<String> = Vec::new();
    if let Some(arr) = turn_ids.as_array() {
        for el in arr {
            if let Some(id) = el.as_str() {
                ids.push(id.to_string());
            } else if let Some(entries) = el.get("entries").and_then(Value::as_array) {
                for e in entries {
                    if let Some(id) = e.as_str() {
                        ids.push(id.to_string());
                    }
                }
            }
        }
    }
    let mut new_entities = Map::new();
    let mut entries = Vec::new();
    for id in &ids {
        let Some(mut entity) = old.remove(id.as_str()) else {
            continue; // 引用失效：与旧 reader 同样「缺实体跳过，不猜」
        };
        if let Some(obj) = entity.as_object_mut() {
            obj.remove("type");
            obj.remove("id");
            obj.insert("turnId".into(), json!(id));
            // items 字符串 id 表 → 内联对象
            if let Some(items) = obj.get("items").and_then(Value::as_array).cloned() {
                let inline: Vec<Value> = items
                    .iter()
                    .filter_map(|ref_id| {
                        let ref_id = ref_id.as_str()?;
                        let mut item = old.remove(ref_id)?;
                        let obj = item.as_object_mut()?;
                        let kind = obj.get("type").and_then(Value::as_str)?.to_string();
                        obj.remove("type");
                        obj.remove("id");
                        // camelCase + user 正文块化（旧 user_message.text → content 块）
                        let converted = match kind.as_str() {
                            "user_message" => {
                                let text = obj.remove("text").unwrap_or(json!(""));
                                let mut m = Map::new();
                                m.insert("type".into(), json!("userMessage"));
                                m.insert("id".into(), json!(ref_id));
                                m.insert(
                                    "content".into(),
                                    json!([{ "type": "text", "text": text }]),
                                );
                                Value::Object(m)
                            }
                            "agent_message" => {
                                obj.insert("type".into(), json!("agentMessage"));
                                obj.insert("id".into(), json!(ref_id));
                                item
                            }
                            other => {
                                obj.insert("type".into(), json!(other));
                                obj.insert("id".into(), json!(ref_id));
                                item
                            }
                        };
                        Some(converted)
                    })
                    .collect();
                obj.insert("items".into(), json!(inline));
            }
        }
        let key = format!("turn:{id}");
        entries.push(json!({ "key": key, "value": key }));
        new_entities.insert(key, entity);
    }
    (Value::Object(new_entities), json!([{ "entries": entries }]))
}

/// 真实形态 snapshot 帧的 conversationState.turnHistory（isComplete=true）。
pub fn canonical_turn_history(entities: Value, turn_ids: Value) -> Value {
    let (e, islands) = to_canonical(entities, turn_ids);
    json!({
        "kind": "canonical",
        "history": { "generation": 0, "isComplete": true,
                     "entitiesByKey": e, "islands": islands }
    })
}
