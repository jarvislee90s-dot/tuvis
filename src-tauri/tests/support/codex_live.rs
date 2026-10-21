//! 真机探针共用件：脱敏口径 + 会话候选枚举 + following 声明/撤销。
//!
//! 供两个真机探针共用（**脱敏口径必须只有一份**，否则两处夹具的脱敏强度会漂移）：
//! * `codex_ipc_live_capture.rs`——#172 只读抓取探针（不发任何写帧）；
//! * `codex_ipc_live_decision.rs`——T10-A 审批决策闭环探针（发**一次**决策帧）。
//!
//! 本模块**不含**任何真机连接动作：只提供纯函数（脱敏/枚举）与两个只读 broadcast
//! 助手（following 声明/撤销）。真机连接由各探针自己负责。

#![allow(dead_code)] // 两个测试二进制各用其中一部分（跨二进制 unused 警告兜底）

use multi_agents_manager_lib::services::codex_desktop_ipc::connection::ConnectionHandle;
use multi_agents_manager_lib::services::codex_desktop_ipc::types::ThreadKey;
use std::path::PathBuf;
use std::time::SystemTime;

/// 结构词白名单：这些字符串是**协议词汇**（方法名/类型/枚举），保留原值；其余字符串
/// 一律脱敏成 `<redacted:len>`（命令正文、理由、路径等用户内容不落盘）。
pub const KEEP_STRINGS: &[&str] = &[
    "method",
    "type",
    "kind",
    "status",
    "role",
    "decision",
    "availableDecisions",
    "approvalPolicy",
    "sandboxPolicy",
    "collaborationMode",
    "clientId",
    "hostId",
    "requestId",
    "conversationId",
    "threadId",
    "turnId",
    "itemId",
];

/// 数组最多保留的元素数（防超大数组把夹具撑爆）。
pub const MAX_ARRAY: usize = 8;
/// 对象最多保留的键数（同上）。
pub const MAX_OBJECT_KEYS: usize = 40;

/// 脱敏：保留键与类型，字符串按白名单决定去留；数组/对象有界。
pub fn sanitize(value: &serde_json::Value, key: Option<&str>) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            if key.is_some_and(|k| KEEP_STRINGS.contains(&k)) {
                serde_json::Value::String(s.clone())
            } else {
                serde_json::json!(format!("<redacted:{}>", s.chars().count()))
            }
        }
        serde_json::Value::Number(n) => serde_json::Value::Number(n.clone()),
        serde_json::Value::Bool(b) => serde_json::Value::Bool(*b),
        serde_json::Value::Null => serde_json::Value::Null,
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .take(MAX_ARRAY)
                .map(|v| sanitize(v, key))
                .collect(),
        ),
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map.iter().take(MAX_OBJECT_KEYS) {
                out.insert(k.clone(), sanitize(v, Some(k)));
            }
            serde_json::Value::Object(out)
        }
    }
}

/// 列候选会话 id（`~/.codex/sessions/**/rollout-*-<uuid>.jsonl` 的文件名尾 UUID，按
/// mtime 倒序）。只读、不连 IPC——用于「用户不知道 thread id」时的提示。
pub fn candidate_threads(limit: usize) -> Vec<String> {
    let home = dirs::home_dir().unwrap_or_default();
    let root = home.join(".codex").join("sessions");
    let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
    collect_rollouts(&root, &mut files, 0);
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    files
        .into_iter()
        .filter_map(|(_, path)| {
            let name = path.file_name()?.to_string_lossy().to_string();
            uuid_from_rollout_name(&name)
        })
        .take(limit)
        .collect()
}

/// 从 rollout 文件名取 thread UUID：文件名形如
/// `rollout-2026-10-21T15-52-41-<uuid>.jsonl`——**UUID 自己也被 `-` 切碎**，所以只能
/// 取「去掉 `.jsonl` 后的末 36 字符」再校验 UUID 形态（按 `-` 切段取尾会只剩 12 位 hex，
/// 这正是 2026-10-21 子代理探测时候选列表为空的 bug）。
pub fn uuid_from_rollout_name(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".jsonl")?;
    if stem.len() < 36 {
        return None;
    }
    let tail = &stem[stem.len() - 36..];
    let bytes = tail.as_bytes();
    let shaped = bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        });
    shaped.then(|| tail.to_string())
}

fn collect_rollouts(dir: &std::path::Path, out: &mut Vec<(SystemTime, PathBuf)>, depth: usize) {
    if depth > 4 {
        return;
    }
    let Ok(iter) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in iter.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollouts(&path, out, depth + 1);
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("rollout-") && name.ends_with(".jsonl") {
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            out.push((modified, path));
        }
    }
}

/// 请求首帧（Desktop 只在状态变化时推送；静默时用它再要一次）。
pub async fn status_requested(conn: &ConnectionHandle, key: &ThreadKey) -> Result<(), String> {
    let version = conn
        .version_of("thread-stream-following-status-requested")
        .map_err(|e| e.code().to_string())?;
    conn.broadcast(
        "thread-stream-following-status-requested",
        version,
        serde_json::json!({ "conversationId": key.thread_id, "hostId": key.host_id }),
    )
    .await
    .map_err(|e| e.code().to_string())
}

/// 撤销订阅（探针退出前的收尾动作；失败不重试）。
pub async fn unfollow(conn: &ConnectionHandle, key: &ThreadKey) -> Result<(), String> {
    let version = conn
        .version_of("thread-stream-following-changed")
        .map_err(|e| e.code().to_string())?;
    conn.broadcast(
        "thread-stream-following-changed",
        version,
        serde_json::json!({
            "conversationId": key.thread_id,
            "hostId": key.host_id,
            "following": false,
        }),
    )
    .await
    .map_err(|e| e.code().to_string())
}
