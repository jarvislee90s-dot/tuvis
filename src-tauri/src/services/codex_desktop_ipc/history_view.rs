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
                    params: entity.get("params"),
                    items: entity
                        .get("items")
                        .and_then(Value::as_array)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]),
                    pagination: entity.get("itemsPagination"),
                    items_view: entity.get("itemsView").and_then(Value::as_str),
                })
            })
            .collect()
    }

    /// 目标 turnId 的状态。只认 canonical 引用链上的实体——未被任何 island 引用的
    /// 孤立实体不作为任何判定依据；引用失效（key 命中 / value 悬空）同样无证据。
    /// **多义不猜**（2026-10-18 复审 P2-2）：同一 turnId 被多个被引用实体认领
    /// （键迁移双引用窗口）且状态不一致时返回 None——不按 canonical 首个放行。
    /// 2026-10-19 复审 P1：**顺序无关**的多义判据——`seen` 与 `status` 分开记录；
    /// 任一目标副本缺 status 即保守无证据（旧实现把「未见目标」与「见过但缺
    /// status」都记成 None，缺 status 副本在前时会被后到的 completed 放行）。
    pub fn turn_status(&self, turn_id: &str) -> Option<&'a str> {
        let mut seen = false;
        let mut status: Option<&'a str> = None;
        for turn in self.turns() {
            if turn.turn_id != turn_id {
                continue;
            }
            seen = true;
            // 任一目标副本缺 status：多义，顺序无关
            let s = turn.status?;
            match status {
                None => status = Some(s),
                Some(prev) if prev == s => {}
                Some(_) => return None, // 状态冲突：多义
            }
        }
        if seen {
            status
        } else {
            None
        }
    }

    /// items 覆盖：任一被引用 turn 的分页标记未载完 / 在载 / 淘汰 / 明确未加载
    /// 标记 → 不完整。
    pub fn items_incomplete(&self) -> bool {
        self.turns().iter().any(TurnRef::items_incomplete)
    }

    /// 结构健康（2026-10-19 复审 P2-3）：区分「合法空历史」与「非法结构」。只对
    /// **内容健康**（contentState）负责——目标终态证据不因无关 Turn 的内容异常
    /// 被整体拒绝（`turn_status` 不受本方法影响）。
    pub fn health(&self) -> ViewHealth {
        let Some(history) = self.history().and_then(Value::as_object) else {
            return ViewHealth::StructuralError;
        };
        let entities = history.get("entitiesByKey").and_then(Value::as_object);
        match history.get("islands") {
            None => {
                // 无 islands：仅当实体表缺失/为空才是合法空历史；有实体却无可引用
                // 结构 → 无法读取，不冒充空会话
                match entities {
                    None => ViewHealth::Ok,
                    Some(m) if m.is_empty() => ViewHealth::Ok,
                    Some(_) => ViewHealth::StructuralError,
                }
            }
            Some(Value::Array(islands)) => {
                for island in islands {
                    let Some(entries) = island.get("entries") else {
                        continue;
                    };
                    let Some(entries) = entries.as_array() else {
                        return ViewHealth::StructuralError; // entries 非数组
                    };
                    for entry in entries {
                        let Some(value) = entry.get("value").and_then(Value::as_str) else {
                            return ViewHealth::StructuralError; // 条目形态非法
                        };
                        let Some(entities) = entities else {
                            return ViewHealth::StructuralError;
                        };
                        let Some(entity) = entities.get(value) else {
                            return ViewHealth::StructuralError; // 悬空引用
                        };
                        if !entity.get("turnId").is_some_and(Value::is_string) {
                            return ViewHealth::StructuralError; // 无法认领的实体
                        }
                        match entity.get("items") {
                            None => {}
                            Some(v) if v.is_array() => {}
                            Some(_) => return ViewHealth::StructuralError, // items 非数组
                        }
                    }
                }
                ViewHealth::Ok
            }
            Some(_) => ViewHealth::StructuralError, // islands 非数组
        }
    }

    /// 内容缺口判据（adapter 只读 hydrate 触发源，单一判据）：列表不完整、任一
    /// turn 的 items 未加载/淘汰，或结构不可读 → 需要只读 load-complete-history。
    /// 不包含「目标 Turn 终态证据」语义——完成核对独立于正文加载。
    pub fn needs_hydration(&self) -> bool {
        !self.turn_list_complete() || self.items_incomplete() || self.health() != ViewHealth::Ok
    }
}

/// 只读视图的结构健康（供 contentState 派生）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewHealth {
    /// 合法（含合法空历史：islands=[] / entries=[] / 无实体表）
    Ok,
    /// 结构不可读（entries/items 非预期、悬空引用、无法认领的实体）——显示
    /// Unsupported，不冒充真空会话
    StructuralError,
}

/// canonical 引用链上的单个 turn 实体（借用视图）。
pub struct TurnRef<'a> {
    pub turn_id: &'a str,
    pub status: Option<&'a str>,
    /// Turn 的 `params`（start-turn 请求侧事实：`input` 数组 + `clientUserMessageId`）。
    /// **只作开场正文的后备显示来源**——不参与状态/完整性/送达判定（见 mirror 的
    /// `opening_input`）。历史 Turn 通常缺省（Desktop 只在有请求上下文时写入）。
    pub params: Option<&'a Value>,
    /// 内联 item 对象数组（真实形态：正文在 items 上，不是字符串 id 表）。
    pub items: &'a [Value],
    pagination: Option<&'a Value>,
    /// 明确未加载标记（金样实证：start-turn 回执的 `itemsView:"notLoaded"`）
    items_view: Option<&'a str>,
}

impl<'a> TurnRef<'a> {
    /// 单 turn 的 items 覆盖：分页在场且明确未载完 / 在载 / 淘汰 → 不完整；
    /// `itemsPagination` 缺省（流式尾部常态，金样如实记录）不作不完整论——
    /// 不完整必须有正信号，不靠缺字段猜。
    fn items_incomplete(&self) -> bool {
        // 明确未加载标记（不因缺分页字段就当正文完整）
        if matches!(self.items_view, Some("notLoaded") | Some("summary")) {
            return true;
        }
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
