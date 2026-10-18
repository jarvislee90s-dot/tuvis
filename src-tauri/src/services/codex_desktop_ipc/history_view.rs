//! canonical 历史只读视图（2026-10-18 真机形态适配）。
//!
//! 职责仅为一件事：借用 mirror 保存的原始 `conversationState`，按官方 Desktop 的
//! **实际**canonical 模型集中读取。所有消费者（终态证据 / 忙态 / 消息投影 /
//! 完整性）共用本视图，不再各自维护实体判断。
//!
//! 真实形态（真机抓包金样 `tests/fixtures/codex-desktop-ipc/observed/`，Desktop
//! 自身读法来自 app.asar 静态核对）：
//! 1. canonical 顺序 = `turnHistory.history.islands[].entries` 数组序，引用键取
//!    **`entry.value`**（`entry.key` 是条目自身标识，可与 value 不同——不参与查找）；
//! 2. turn 实体以 **`turnId` 字段**认领，**没有 `type` 字段**（不能要求 `type=="turn"`，
//!    也不能从 map key（`turn:<uuid>` / `tail:0:local:<uuid>`）解析业务 ID——tail 键尾
//!    UUID 与实体 turnId 可以不同）；
//! 3. 消息正文在 `turn.items[]` **内联对象**上（camelCase）：`userMessage.content[]`
//!    的 text 块、`agentMessage.text`；`error.message` 为安全摘要来源；
//! 4. 完整性分层：Turn 列表完整性 = `turnHistory.history.isComplete === true`
//!    （缺字段 ≠ 完整）；单 Turn 的 items 完整性看 `itemsPagination`
//!    （`hasLoadedOldest==false` / `isLoadingOlder==true` / `evicted==true` 即不完整；
//!    字段缺省——流式尾部常态——不作不完整论）。
//!
//! **刻意不做**：不改写 map key、不给 raw turn 补 `type`、不把内联 items 搬进另一
//! 模型——mirror 仍原样保存 conversationState，真实 patch 仍对原始路径应用；本视图
//! 只是读取侧的统一焦点，避免每个消费者再长出一套私有判断。

use serde_json::Value;

/// 原始 conversationState 的 canonical 历史视图（借用，零拷贝）。
pub struct HistoryView<'a> {
    state: &'a Value,
}

impl<'a> HistoryView<'a> {
    pub fn new(state: &'a Value) -> Self {
        Self { state }
    }

    fn history(&self) -> Option<&'a Value> {
        self.state.get("turnHistory")?.get("history")
    }

    fn entities(&self) -> Option<&'a Value> {
        self.history()?.get("entitiesByKey")
    }

    /// 形态可读：`turnHistory.history` 在场且为对象。不在场 = 读取异常
    /// （unsupported），不得呈现为「真实空历史」。
    pub fn readable(&self) -> bool {
        self.history().is_some_and(Value::is_object)
    }

    /// Turn 列表完整性：`history.isComplete === true`。缺字段 / 非true = 不完整
    /// （hydrate 触发判据，不能缺省当完整）。
    pub fn turn_list_complete(&self) -> bool {
        self.history()
            .and_then(|h| h.get("isComplete"))
            .and_then(Value::as_bool)
            == Some(true)
    }

    /// canonical 引用键（`entry.value`），去重保序。entries 元素非对象 / value 非字符串
    /// → 跳过该条（引用失效不是证据来源，见 [`Self::turn_status`]）。
    fn referenced_keys(&self) -> Vec<&'a str> {
        let mut keys: Vec<&'a str> = Vec::new();
        let Some(islands) = self
            .history()
            .and_then(|h| h.get("islands"))
            .and_then(Value::as_array)
        else {
            return keys;
        };
        for island in islands {
            let Some(entries) = island.get("entries").and_then(Value::as_array) else {
                continue;
            };
            for entry in entries {
                let Some(value) = entry.get("value").and_then(Value::as_str) else {
                    continue;
                };
                if !keys.contains(&value) {
                    keys.push(value);
                }
            }
        }
        keys
    }

    /// canonical 顺序遍历被引用的 turn 实体。引用失效 / 实体缺失 / 非对象 /
    /// 无 `turnId` 字段 → 跳过（不猜）。
    pub fn turns(&self) -> Vec<TurnRef<'a>> {
        let Some(entities) = self.entities().and_then(Value::as_object) else {
            return Vec::new();
        };
        self.referenced_keys()
            .into_iter()
            .filter_map(|key| {
                let entity = entities.get(key)?;
                Some(TurnRef {
                    turn_id: entity.get("turnId")?.as_str()?,
                    status: entity.get("status").and_then(Value::as_str),
                    items: entity
                        .get("items")
                        .and_then(Value::as_array)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]),
                    pagination: entity.get("itemsPagination"),
                })
            })
            .collect()
    }

    /// 目标 turnId 的状态。只认 canonical 引用链上的实体——未被任何 island 引用的
    /// 孤立实体不作为任何判定依据；引用失效（key 命中 / value 悬空）同样无证据。
    pub fn turn_status(&self, turn_id: &str) -> Option<&'a str> {
        self.turns()
            .into_iter()
            .find(|turn| turn.turn_id == turn_id)?
            .status
    }

    /// items 覆盖：任一被引用 turn 的分页标记未载完 / 在载 / 淘汰 → 不完整。
    pub fn items_incomplete(&self) -> bool {
        self.turns().iter().any(TurnRef::items_incomplete)
    }
}

/// canonical 引用链上的单个 turn 实体（借用视图）。
pub struct TurnRef<'a> {
    pub turn_id: &'a str,
    pub status: Option<&'a str>,
    /// 内联 item 对象数组（真实形态：正文在 items 上，不是字符串 id 表）。
    pub items: &'a [Value],
    pagination: Option<&'a Value>,
}

impl<'a> TurnRef<'a> {
    /// 单 turn 的 items 覆盖：分页在场且明确未载完 / 在载 / 淘汰 → 不完整；
    /// `itemsPagination` 缺省（流式尾部常态，金样如实记录）不作不完整论——
    /// 不完整必须有正信号，不靠缺字段猜。
    fn items_incomplete(&self) -> bool {
        let Some(p) = self.pagination else {
            return false;
        };
        p.get("hasLoadedOldest").and_then(Value::as_bool) == Some(false)
            || p.get("isLoadingOlder").and_then(Value::as_bool) == Some(true)
            || p.get("evicted").and_then(Value::as_bool) == Some(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state_with(value: Value) -> Value {
        value
    }

    #[test]
    fn turn_list_complete_requires_explicit_true() {
        let complete = state_with(json!({
            "turnHistory": { "kind": "canonical",
                "history": { "isComplete": true, "entitiesByKey": {}, "islands": [] } }
        }));
        assert!(HistoryView::new(&complete).turn_list_complete());
        let missing = state_with(json!({
            "turnHistory": { "kind": "canonical",
                "history": { "entitiesByKey": {}, "islands": [] } }
        }));
        assert!(!HistoryView::new(&missing).turn_list_complete());
        let false_ = state_with(json!({
            "turnHistory": { "history": { "isComplete": false } }
        }));
        assert!(!HistoryView::new(&false_).turn_list_complete());
    }

    #[test]
    fn unreadable_history_is_not_empty_ready() {
        let no_history = state_with(json!({ "threadRuntimeStatus": { "type": "idle" } }));
        assert!(!HistoryView::new(&no_history).readable());
    }
}
