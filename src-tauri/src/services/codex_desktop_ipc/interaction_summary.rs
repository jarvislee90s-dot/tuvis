//! F04／F06：**安全摘要**（T05）——命令、文件变更与计划请求的可展示摘要。
//!
//! 纪律（spec §9）：
//! * 只白名单提取可展示字段（命令文本／cwd／原因／变更路径），**不 dump 原始
//!   params 对象**，不输出凭据类字段；
//! * 摘要长度上限 [`MAX_SUMMARY_CHARS`]（4096 Unicode 字符），超出截断并标注；
//! * 文件请求的权威内容不在 pending `params` 上（本 build 实测无 `changes`）——
//!   必须按**原 thread／turn／item** 唯一关联 canonical `fileChange` 条目；
//!   关联不到或摘要不足 → `None`（上层据此移除"批准"，只留可靠拒绝/中止）。

use serde_json::Value;

use super::history_view::HistoryView;

/// 摘要长度上限（Unicode 字符）。
pub const MAX_SUMMARY_CHARS: usize = 4096;

/// 单条摘要里最多列出的变更文件数（其余以"等 N 项"概括）。
pub const MAX_SUMMARY_FILES: usize = 20;

/// 截断到上限（含省略标记）。
fn clamp_summary(text: String) -> String {
    if text.chars().count() <= MAX_SUMMARY_CHARS {
        return text;
    }
    let kept: String = text
        .chars()
        .take(MAX_SUMMARY_CHARS.saturating_sub(1))
        .collect();
    format!("{kept}…")
}

/// 是否像凭据/密钥字段（**绝不展示**）。
fn looks_secret(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "api_key",
        "apikey",
        "authorization",
        "cookie",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn text_field(params: &Value, key: &str) -> Option<String> {
    if looks_secret(key) {
        return None;
    }
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// 由 pending 请求参数生成**安全摘要**（不输出原始对象）。
///
/// 覆盖种类：
/// * `item/commandExecution/requestApproval`：命令 + cwd + 原因；
/// * `item/fileChange/requestApproval`：`changes` 存在时的条数/路径（缺失 → `None`，
///   由 [`resolve_file_summary`] 从 canonical 条目补）；
/// * `item/plan/requestImplementation`：计划正文前若干字符（长度受限）；
/// * 其他：`None`（只显示请求类型）。
pub fn safe_pending_summary(method: &str, params: &Value) -> Option<String> {
    match method {
        "item/commandExecution/requestApproval" => {
            let command = text_field(params, "command")?;
            let mut summary = command;
            if let Some(cwd) = text_field(params, "cwd") {
                // 摘要正文保持语言中立的 ASCII 标签：前端按界面语言本地化标签，
                // 避免把"展示内容"混入错误码词表。
                summary.push_str(&format!("\n(cwd: {cwd})"));
            }
            if let Some(reason) = text_field(params, "reason") {
                summary.push_str(&format!("\n(reason: {reason})"));
            }
            Some(clamp_summary(summary))
        }
        "item/fileChange/requestApproval" => {
            let changes = params.get("changes")?.as_array()?;
            if changes.is_empty() {
                return None;
            }
            // R11：只有条数/只有 kind 的摘要**不构成可批准的依据**（不知道改什么）。
            Some(clamp_summary(paths_summary(changes)?))
        }
        "item/plan/requestImplementation" => {
            let plan = text_field(params, "planContent").or_else(|| text_field(params, "plan"))?;
            Some(clamp_summary(plan))
        }
        _ => None,
    }
}

/// 摘要（只含可定位的变更）。**任意一条无法定位 → None**（不是 "0/N changes"）：
/// 数量本身不能说明要批准什么，而"部分可定位、部分不可定位"等于让用户批准看不
/// 到的内容（R11/H07）。展示可截断，但**可定位性校验覆盖完整列表**。
fn paths_summary(changes: &[Value]) -> Option<String> {
    let change_path = |change: &Value| -> Option<String> {
        change
            .get("path")
            .or_else(|| change.get("filePath"))
            .or_else(|| change.get("filename"))
            .and_then(Value::as_str)
            .filter(|path| !path.trim().is_empty())
            .map(str::to_string)
    };
    // 先校验完整列表（含 MAX_SUMMARY_FILES 之后），再截断展示。
    let all_paths: Vec<String> = changes
        .iter()
        .map(change_path)
        .collect::<Option<Vec<_>>>()?;
    if all_paths.is_empty() {
        return None;
    }
    let shown = all_paths.len().min(MAX_SUMMARY_FILES);
    let mut summary = all_paths[..shown].join("\n");
    if all_paths.len() > shown {
        summary.push_str(&format!("\n…+{} more", all_paths.len() - shown));
    }
    Some(summary)
}

/// 按**原 thread／turn／item**唯一关联 canonical `fileChange` 条目，生成文件摘要。
///
/// 返回 `None` 的条件（不猜）：
/// * 历史不可读 / 找不到指定 turn；
/// * 该 turn 里没有 `id == item_id` 的 `fileChange` 条目；
/// * 条目内容为空（没有可展示的变更）。
///
/// `thread_id` 只用于防御性校验（canonical 视图本来就绑定该线程状态）。
/// N09：canonical 关联的三态——**Missing**（找不到关联，可用参数摘要兜底）与
/// **Unsafe**（找到了但内容不可完整定位，必须撤批准）不能混为一谈。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileSummaryState {
    Missing,
    Unsafe,
    Valid(String),
}

pub fn resolve_file_summary_state(
    state: &Value,
    thread_id: &str,
    turn_id: &str,
    item_id: &str,
) -> FileSummaryState {
    if thread_id.is_empty() || turn_id.is_empty() || item_id.is_empty() {
        return FileSummaryState::Missing;
    }
    let view = HistoryView::new(state);
    if !view.readable() {
        return FileSummaryState::Missing;
    }
    let mut found: Option<Value> = None;
    for turn in view.turns() {
        if turn.turn_id != turn_id {
            continue;
        }
        for item in turn.items {
            if item.get("id").and_then(Value::as_str) != Some(item_id) {
                continue;
            }
            if item.get("type").and_then(Value::as_str) != Some("fileChange") {
                continue;
            }
            // 同一 (turn, item) 出现两份时**多义=不安全**（不猜、也不回退子集）。
            if found.is_some() {
                return FileSummaryState::Unsafe;
            }
            found = Some(item.clone());
        }
    }
    let Some(item) = found else {
        return FileSummaryState::Missing;
    };
    let Some(changes) = item
        .get("changes")
        .or_else(|| item.get("diff"))
        .and_then(Value::as_array)
    else {
        return FileSummaryState::Unsafe;
    };
    if changes.is_empty() {
        return FileSummaryState::Unsafe;
    }
    // R11/N09：没有完整可定位变更（空对象/只有 kind/字段名不符）→ 摘要不足。
    match paths_summary(changes) {
        Some(summary) => FileSummaryState::Valid(clamp_summary(summary)),
        None => FileSummaryState::Unsafe,
    }
}

/// 兼容包装：只有 Valid 才给出摘要（Missing/Unsafe 都是 None）。
pub fn resolve_file_summary(
    state: &Value,
    thread_id: &str,
    turn_id: &str,
    item_id: &str,
) -> Option<String> {
    match resolve_file_summary_state(state, thread_id, turn_id, item_id) {
        FileSummaryState::Valid(summary) => Some(summary),
        _ => None,
    }
}

/// H07：关联 `fileChange` 的**完整内容指纹**（路径 + kind + 内容；不受展示截断影响）。
///
/// 权限版本必须绑定真实内容而不是展示文本——同路径 diff 变化、截断区外的变更都
/// 要换 token，旧卡不得批准新内容。
pub fn resolve_file_change_fingerprint(
    state: &Value,
    thread_id: &str,
    turn_id: &str,
    item_id: &str,
) -> Option<u64> {
    if thread_id.is_empty() || turn_id.is_empty() || item_id.is_empty() {
        return None;
    }
    let view = HistoryView::new(state);
    if !view.readable() {
        return None;
    }
    let mut found: Option<Value> = None;
    for turn in view.turns() {
        if turn.turn_id != turn_id {
            continue;
        }
        for item in turn.items {
            if item.get("id").and_then(Value::as_str) != Some(item_id) {
                continue;
            }
            if item.get("type").and_then(Value::as_str) != Some("fileChange") {
                continue;
            }
            if found.is_some() {
                return None; // 多义不猜
            }
            found = Some(item.clone());
        }
    }
    let item = found?;
    let changes = item
        .get("changes")
        .or_else(|| item.get("diff"))
        .and_then(Value::as_array)?;
    // 规范化序列化（键序稳定由 serde_json::Map 的 BTreeMap 语义保证）→ FNV-1a。
    let canonical = serde_json::to_string(changes).ok()?;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in canonical.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    Some(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn canonical_with_file_change(turn_id: &str, item_id: &str, changes: Value) -> Value {
        json!({
            "turnHistory": {"history": {
                "isComplete": true,
                "entitiesByKey": {format!("turn:{turn_id}"): {
                    "turnId": turn_id,
                    "status": "active",
                    "items": [
                        {"type": "userMessage", "id": "u1", "content": [{"type": "text", "text": "hi"}]},
                        {"type": "fileChange", "id": item_id, "changes": changes}
                    ]
                }},
                "islands": [{"id": "i", "entries": [{"key": format!("turn:{turn_id}"), "value": format!("turn:{turn_id}")}]}]
            }}
        })
    }

    /// 文件摘要按原 thread／turn／item 唯一关联；错 turn / 缺条目 → None。
    #[test]
    fn f08p_t05_file_summary_missing_or_wrong_turn_blocks_accept() {
        let state = canonical_with_file_change(
            "turn-1",
            "item-1",
            json!([{"path": "src/a.rs", "kind": "update"}, {"path": "src/b.rs"}]),
        );
        let summary = resolve_file_summary(&state, "thread-1", "turn-1", "item-1")
            .expect("unique fileChange summary");
        assert!(summary.contains("src/a.rs") && summary.contains("src/b.rs"));
        assert!(resolve_file_summary(&state, "thread-1", "turn-2", "item-1").is_none());
        assert!(resolve_file_summary(&state, "thread-1", "turn-1", "item-2").is_none());
        assert!(resolve_file_summary(&state, "", "turn-1", "item-1").is_none());
        // 空 changes → 无摘要（不猜内容）。
        let empty = canonical_with_file_change("turn-1", "item-1", json!([]));
        assert!(resolve_file_summary(&empty, "thread-1", "turn-1", "item-1").is_none());
    }

    /// 摘要只白名单字段：不输出原始对象、不泄露凭据、长度有界。
    #[test]
    fn f08p_t05_safe_summary_is_whitelisted_and_bounded() {
        let params = json!({
            "command": "pnpm test",
            "cwd": "/work/project",
            "reason": "run tests",
            "api_key": "sk-should-not-appear",
            "env": {"SECRET_TOKEN": "nope"},
        });
        let summary = safe_pending_summary("item/commandExecution/requestApproval", &params)
            .expect("command summary");
        assert!(summary.contains("pnpm test"));
        assert!(!summary.contains("sk-should-not-appear"));
        assert!(!summary.contains("SECRET_TOKEN"));

        let long = "x".repeat(MAX_SUMMARY_CHARS + 100);
        let params = json!({ "command": long });
        let summary = safe_pending_summary("item/commandExecution/requestApproval", &params)
            .expect("long summary");
        assert_eq!(summary.chars().count(), MAX_SUMMARY_CHARS);

        // 文件请求缺 changes → None（由 canonical 关联补；补不到就不能批准）。
        assert!(safe_pending_summary(
            "item/fileChange/requestApproval",
            &json!({"threadId": "t", "turnId": "x"})
        )
        .is_none());
    }
}

#[cfg(test)]
mod h07_mixed_locatable_tests {
    use super::*;

    fn change(path: &str) -> Value {
        serde_json::json!({ "path": path, "kind": "update" })
    }

    /// H07：任意一条不可定位 → 不可批准（摘要 None），不能把部分内容概括成 more。
    #[test]
    fn h07_mixed_unlocatable_changes_yield_no_summary() {
        let mixed = serde_json::json!([
            { "path": "a.rs", "kind": "update" },
            {}
        ]);
        assert_eq!(
            safe_pending_summary(
                "item/fileChange/requestApproval",
                &serde_json::json!({
                    "changes": mixed,
                    "threadId": "t", "turnId": "q", "itemId": "i"
                })
            ),
            None
        );
        let kind_only = serde_json::json!([
            { "path": "a.rs" },
            { "kind": "update" }
        ]);
        assert_eq!(
            safe_pending_summary(
                "item/fileChange/requestApproval",
                &serde_json::json!({
                    "changes": kind_only
                })
            ),
            None
        );
    }

    /// 第 21 条（展示截断之后）不可定位同样撤批准。
    #[test]
    fn h07_unlocatable_beyond_display_limit_removes_approval() {
        let mut changes: Vec<Value> = (0..MAX_SUMMARY_FILES)
            .map(|i| change(&format!("src/f{i}.rs")))
            .collect();
        changes.push(serde_json::json!({}));
        assert_eq!(
            safe_pending_summary(
                "item/fileChange/requestApproval",
                &serde_json::json!({ "changes": changes })
            ),
            None
        );
    }

    /// 全部合法的大列表可限长展示（展示截断，校验不截断）。
    #[test]
    fn h07_all_locatable_large_list_is_truncated_for_display() {
        let changes: Vec<Value> = (0..(MAX_SUMMARY_FILES + 3))
            .map(|i| change(&format!("src/f{i}.rs")))
            .collect();
        let summary = safe_pending_summary(
            "item/fileChange/requestApproval",
            &serde_json::json!({ "changes": changes }),
        )
        .expect("summary");
        assert!(summary.contains("src/f0.rs"));
        assert!(summary.contains("…+3 more"));
        assert!(!summary.contains("src/f20.rs"));
    }

    /// canonical 关联路径同样要求完整可定位。
    #[test]
    fn h07_canonical_mixed_changes_are_not_approvable() {
        let state = serde_json::json!({ "turnHistory": { "history": {
            "isComplete": true,
            "entitiesByKey": {"turn:t1": {
                "turnId": "t1", "status": "completed",
                "items": [{
                    "type": "fileChange", "id": "item-1",
                    "changes": [{"path": "a.rs"}, {}]
                }]
            }},
            "islands": [{"id": "i", "entries": [{"key": "turn:t1", "value": "turn:t1"}]}]
        }}});
        assert_eq!(resolve_file_summary(&state, "th", "t1", "item-1"), None);
    }
}
