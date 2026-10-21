//! #172 真机夹具抓取探针（**只读、`#[ignore]`、需用户在场**）。
//!
//! 用途：T7 的 pending 解析必须有「真实待审批快照」（`conversationState.requests` 非空
//! 的 `thread-stream-state-changed`）。仓库现有的真实快照 `requests: []`，合成夹具按
//! 纪律不能当解析依据——所以用本探针在真机上抓一次**派生**夹具（只保留 pending 请求的
//! **形态**：键、类型、枚举词；ID 之外的自由文本一律脱敏）。
//!
//! **不发任何写帧**：只有 `thread-owner-discovery`（只读询问 owner）、
//! `thread-stream-following-changed {following:true/false}`（订阅声明/撤销）、以及静默
//! 时的 `thread-stream-following-status-requested`（请求首帧）。**不点审批、不发
//! start-turn、不改任何设置**。
//!
//! 运行方式（在 `src-tauri/` 下）：
//!
//! ```powershell
//! $env:TUVIS_CAPTURE_THREAD="<thread-uuid>"      # 必填；不填会列出候选会话 id
//! $env:TUVIS_CAPTURE_LABEL="windows-4894-pending-command"   # 可选，输出文件名
//! $env:TUVIS_CAPTURE_WAIT_SECS="120"             # 可选，等待挂起请求的秒数（默认 60）
//! cargo test --features marker-helper --test codex_ipc_live_capture -- --ignored --nocapture
//! ```
//!
//! 建议顺序：**先跑探针，再在 Desktop 里让 Codex 发起一条需要审批的命令**（例如
//! `echo hi`）——探针会在等待窗口里自动抓下第一帧「requests 非空」的快照。抓完输出文件
//! 路径（`%TEMP%\tuvis-capture-<label>.json`）与摘要；文件内容已脱敏，可直接进仓。
//!
//! 纪律（施工单 §3/§7）：没有用户在场不得运行；不抓凭据；不点审批。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use multi_agents_manager_lib::services::codex_desktop_ipc::adapter::{
    Connector, ProductionConnector,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::connection::{
    spawn, ConnectionEvent, ConnectionHandle,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::owners::{
    declare_following, discover_owner,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::profile::{
    profile_for, ProtocolProfile,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::transport::EndpointResolver;
use multi_agents_manager_lib::services::codex_desktop_ipc::types::ThreadKey;

/// 结构词白名单：这些字符串是**协议词汇**（方法名/类型/枚举），保留原值；其余字符串
/// 一律脱敏成 `<redacted:len>`（命令正文、理由、路径等用户内容不落盘）。
const KEEP_STRINGS: &[&str] = &[
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

const MAX_ARRAY: usize = 8;
const MAX_OBJECT_KEYS: usize = 40;

fn env_usize(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// 列候选会话 id（`~/.codex/sessions/**/rollout-*-<uuid>.jsonl` 的文件名尾 UUID，按
/// mtime 倒序）。只读、不连 IPC——用于「用户不知道 thread id」时的提示。
fn candidate_threads(limit: usize) -> Vec<String> {
    let home = dirs::home_dir().unwrap_or_default();
    let root = home.join(".codex").join("sessions");
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
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
fn uuid_from_rollout_name(name: &str) -> Option<String> {
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

fn collect_rollouts(
    dir: &std::path::Path,
    out: &mut Vec<(std::time::SystemTime, PathBuf)>,
    depth: usize,
) {
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
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            out.push((modified, path));
        }
    }
}

/// 脱敏：保留键与类型，字符串按白名单决定去留；数组/对象有界。
fn sanitize(value: &serde_json::Value, key: Option<&str>) -> serde_json::Value {
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

/// 从一帧 snapshot 里派生「pending 请求 + 空会话骨架」的夹具（其余正文全部丢弃）。
fn derive_fixture(
    frame: &serde_json::Value,
    thread_id: &str,
    label: &str,
) -> Option<serde_json::Value> {
    let change = frame.get("params")?.get("change")?;
    if change.get("type").and_then(|t| t.as_str()) != Some("snapshot") {
        return None;
    }
    let state = change.get("conversationState")?;
    let requests = state.get("requests")?.as_array()?;
    if requests.is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "_source": format!("DERIVED: 真机只读探针抓取（{label}）；正文/ID 之外的自由文本已脱敏，键与类型保留"),
        "_captured_from": "thread-stream-state-changed / conversationState.requests",
        "type": "broadcast",
        "method": "thread-stream-state-changed",
        "params": {
            "conversationId": thread_id,
            "change": {
                "type": "snapshot",
                "revision": change.get("revision").cloned().unwrap_or(serde_json::json!(0)),
                "conversationState": {
                    "id": thread_id,
                    "turns": [],
                    "requests": sanitize(&serde_json::Value::Array(requests.clone()), Some("requests")),
                    "turnHistory": { "kind": "canonical", "history": { "isComplete": true } }
                }
            }
        }
    }))
}

#[tokio::test]
#[ignore = "真机只读探针：需用户在场且有挂起审批；用 --ignored 显式运行"]
async fn capture_pending_snapshot() {
    let thread_id = std::env::var("TUVIS_CAPTURE_THREAD").unwrap_or_default();
    let thread_id = thread_id.trim().to_string();
    if thread_id.is_empty() {
        let candidates = candidate_threads(5);
        panic!(
            "未设置 TUVIS_CAPTURE_THREAD。可从下列最近会话 id 选一个（或从 Tuvis 会话详情取）：\n{}",
            candidates.join("\n")
        );
    }
    let label = std::env::var("TUVIS_CAPTURE_LABEL")
        .unwrap_or_else(|_| "capture-pending-approval".to_string());
    let wait_secs = env_usize("TUVIS_CAPTURE_WAIT_SECS", 60);

    // 生产 endpoint（Windows named pipe / macOS unix socket；含服务端身份校验）。
    // 按 target_os 分开构造：`UnixEndpointResolver` 的 `legacy_tmpdir` 字段只在 unix 存在。
    #[cfg(target_os = "macos")]
    let resolver: Arc<dyn EndpointResolver> = Arc::new(
        multi_agents_manager_lib::services::codex_desktop_ipc::transport::unix::UnixEndpointResolver {
            codex_home: None,
            legacy_tmpdir: None,
        },
    );
    #[cfg(target_os = "windows")]
    let resolver: Arc<dyn EndpointResolver> = Arc::new(
        multi_agents_manager_lib::services::codex_desktop_ipc::transport::windows::WindowsEndpointResolver,
    );
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let resolver: Arc<dyn EndpointResolver> = panic!("本探针只支持 Windows / macOS");
    let endpoint = resolver
        .resolve()
        .expect("Desktop IPC endpoint 不可达（Desktop 没起？）");
    let build = endpoint.build.clone().unwrap_or_default();
    let profile = profile_for(&endpoint.platform, &build)
        .unwrap_or_else(|| ProtocolProfile::unverified(&endpoint.platform, &build));
    let (io, _endpoint) = ProductionConnector {
        resolver: resolver.clone(),
    }
    .connect_endpoint()
    .await
    .expect("连接 Desktop IPC 失败（管道/身份校验）");
    let conn = spawn(io, profile, 1);
    eprintln!(
        "已连接：platform={} build={}（只读抓取，不发写帧）",
        endpoint.platform, build
    );

    // 握手（initialize 由连接 actor 内部完成）
    let init_deadline = Instant::now() + Duration::from_secs(5);
    while conn.initialized_client().is_none() {
        assert!(Instant::now() < init_deadline, "initialize 超时");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let key = ThreadKey {
        host_id: "local".into(),
        thread_id: thread_id.clone(),
    };
    let owner = discover_owner(&conn, &key).await;
    match owner {
        Ok(owner) => eprintln!("owner={owner}"),
        Err(e) => panic!("owner discovery 失败（{e}）——该线程是否由 Desktop 持有？"),
    }
    let mut rx = conn.subscribe();
    declare_following(&conn, &key)
        .await
        .expect("声明 following 失败");
    let _ = status_requested(&conn, &key).await;
    eprintln!(
        "已声明 following，等待「requests 非空」的 snapshot（最多 {wait_secs}s）……\n\
         现在请在 Desktop 里让 Codex 发起一条需要审批的命令（例如 echo hi），并停在审批框上。"
    );

    let deadline = Instant::now() + Duration::from_secs(wait_secs);
    let mut last_revision = None;
    let mut derived = None;
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left.min(Duration::from_secs(5)), rx.recv()).await {
            Ok(Ok(ConnectionEvent::Broadcast { frame, .. }))
                if frame.get("method").and_then(|m| m.as_str())
                    == Some("thread-stream-state-changed")
                    && frame["params"]["conversationId"] == serde_json::json!(thread_id) =>
            {
                last_revision = frame["params"]["change"]["revision"].as_u64();
                if let Some(fixture) = derive_fixture(&frame, &thread_id, &label) {
                    derived = Some(fixture);
                    break;
                }
                // 静默/空 requests：再要一次首帧（Desktop 只在状态变化时推送）
                let _ = status_requested(&conn, &key).await;
            }
            _ => continue,
        }
    }

    // 撤销临时订阅（无论成败）
    let _ = unfollow(&conn, &key).await;

    let Some(fixture) = derived else {
        conn.shutdown().await;
        panic!(
            "等待 {wait_secs}s 没有抓到「requests 非空」的 snapshot（最后 revision={last_revision:?}）。\n\
             常见原因：① thread id 不对；② Desktop 界面上没有停着审批框；③ 该审批不是 \
             item/commandExecution|item/fileChange 类。"
        );
    };

    let out = std::env::temp_dir().join(format!("tuvis-capture-{label}.json"));
    std::fs::write(&out, serde_json::to_string_pretty(&fixture).unwrap()).expect("写输出文件失败");
    println!("\n===== 已抓取（脱敏）=====");
    println!("{}", serde_json::to_string_pretty(&fixture).unwrap());
    println!("\n输出文件：{}", out.display());
    let requests = fixture["params"]["change"]["conversationState"]["requests"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    println!(
        "摘要：pending 请求 {} 条；methods={:?}；id 类型={:?}",
        requests.len(),
        requests
            .iter()
            .filter_map(|r| r.get("method").and_then(|m| m.as_str()))
            .collect::<Vec<_>>(),
        requests
            .iter()
            .map(|r| match r.get("id") {
                Some(serde_json::Value::Number(_)) => "number",
                Some(serde_json::Value::String(_)) => "string",
                _ => "missing",
            })
            .collect::<Vec<_>>()
    );
    conn.shutdown().await;
}

async fn status_requested(conn: &ConnectionHandle, key: &ThreadKey) -> Result<(), String> {
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

async fn unfollow(conn: &ConnectionHandle, key: &ThreadKey) -> Result<(), String> {
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

/// 文件名取 thread UUID 的回归（真机 rollout 名样例）：UUID 自身被 `-` 切碎，必须取
/// 「去后缀后末 36 字符」再校验形态——按 `-` 切段取尾只会剩 12 位 hex（2026-10-21 探针
/// 的候选列表因此为空）。
#[test]
fn rollout_filename_yields_thread_uuid() {
    assert_eq!(
        uuid_from_rollout_name(
            "rollout-2026-10-21T15-52-41-01a153d7-78d4-7d60-9709-5500cd0aed0f.jsonl"
        )
        .as_deref(),
        Some("01a153d7-78d4-7d60-9709-5500cd0aed0f")
    );
    assert_eq!(
        uuid_from_rollout_name("rollout-2026-10-21T15-52-41-not-a-uuid.jsonl"),
        None
    );
    assert_eq!(uuid_from_rollout_name("short.jsonl"), None);
    assert_eq!(uuid_from_rollout_name("rollout-x.jsonl.bak"), None);
}
