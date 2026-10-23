//! #172 真机「send 闭环」受控探针（**`#[ignore]`、需用户在场 + 显式同意**）。
//!
//! 目的：给未知 build 补上**最后一项缺失证据**——`send`（真发一条消息）。
//! 6573 已有：读链 P0–P3 ✅、官方 schema ✅、命令审批 decision 闭环 ✅；只差 send。
//! 补上后该 build 就能进 shipped 表（走「已验证」路径，而不是「未知 build 兼容路径」）。
//!
//! **纪律**（与两个既有真机探针同款）：
//! * **显式同意闸**：`TUVIS_LIVE_SEND=1` + `TUVIS_CAPTURE_THREAD=<thread-uuid>` 两者齐备
//!   才动手；缺任一项 → 立即退出并打印原因，**零帧**；
//! * **只发一次**：单个 `start-turn` 帧，**不重试**、不补发；
//! * 走**生产代码路径**：生产 adapter（`ensure_thread` 取 owner/lease → `start_turn_compat`），
//!   wire 形态与生产逐字一致（`start_turn_compat` 只放开「写能力未取证」这一道门，
//!   其余判定、租约、参数组装全部是生产实现）；
//! * 事后**核对生效**：等新 snapshot 出现新 turn / 出现我们那条 user message →
//!   才算「送达」；只拿到 `Accepted` 不算（受理 ≠ 生效，与审批回执同纪律）。
//!
//! 运行方式（在 `src-tauri/` 下）：
//!
//! ```powershell
//! $env:TUVIS_LIVE_SEND = "1"
//! $env:TUVIS_CAPTURE_THREAD = "<thread-uuid>"
//! $env:TUVIS_SEND_TEXT = "（可选）自定义正文"
//! cargo test --features marker-helper --test codex_ipc_live_send -- --ignored --nocapture
//! ```

use std::sync::Arc;

use multi_agents_manager_lib::services::codex_desktop_ipc::adapter::{
    DesktopIpcAdapter, ProductionConnector,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::profile::ProtocolProfile;
use multi_agents_manager_lib::services::codex_desktop_ipc::transport::EndpointResolver;
use multi_agents_manager_lib::services::codex_desktop_ipc::types::{Delivery, ThreadKey};

#[cfg(target_os = "macos")]
fn production_resolver() -> Arc<dyn EndpointResolver> {
    Arc::new(
        multi_agents_manager_lib::services::codex_desktop_ipc::transport::unix::UnixEndpointResolver {
            codex_home: None,
            legacy_tmpdir: None,
        },
    )
}

#[cfg(target_os = "windows")]
fn production_resolver() -> Arc<dyn EndpointResolver> {
    Arc::new(
        multi_agents_manager_lib::services::codex_desktop_ipc::transport::windows::WindowsEndpointResolver,
    )
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn production_resolver() -> Arc<dyn EndpointResolver> {
    panic!("本探针只支持 Windows / macOS");
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

#[tokio::test]
#[ignore = "真机 send 闭环：需用户在场 + 显式同意（TUVIS_LIVE_SEND=1 + thread id）"]
async fn live_send_closure() {
    // ── 显式同意闸：缺任一项 → 零帧退出 ───────────────────────────────
    if !env_flag("TUVIS_LIVE_SEND") {
        panic!("未设置 TUVIS_LIVE_SEND=1 —— 未获授权，**零帧退出**");
    }
    let thread_id = std::env::var("TUVIS_CAPTURE_THREAD")
        .unwrap_or_default()
        .trim()
        .to_string();
    if thread_id.is_empty() {
        panic!("未设置 TUVIS_CAPTURE_THREAD —— 不知道往哪条会话发，**零帧退出**");
    }
    let text = std::env::var("TUVIS_SEND_TEXT")
        .unwrap_or_default()
        .trim()
        .to_string();
    let text = if text.is_empty() {
        "【兔维斯取证】这是一条 #172 未知 build 兼容能力的取证消息，可忽略。".to_string()
    } else {
        text
    };

    let resolver = production_resolver();
    let endpoint = resolver
        .resolve()
        .expect("Desktop IPC endpoint 不可达（Desktop 没起？）");
    let platform = endpoint.platform.clone();
    let build = endpoint.build.clone().unwrap_or_default();
    println!("[send-probe] platform={platform} build={build}");

    // 生产 adapter：未知 build → 「尚无证据」档（可探测；写能力未取证）
    let profile = ProtocolProfile::unverified(&platform, &build);
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(ProductionConnector {
            resolver: resolver.clone(),
        }),
        profile,
    ));

    let key = ThreadKey {
        host_id: "local".into(),
        thread_id: thread_id.clone(),
    };

    // ① 生产跟随（取 owner + 租约 + 首帧投影）；失败=该线程没被 Desktop 持有
    let before = adapter
        .ensure_thread(&key)
        .await
        .expect("ensure_thread 失败（该线程是否由 Desktop 持有？）");
    let revision_before = before.revision;
    let messages_before = before.messages.len();
    println!("[send-probe] 已跟随：revision={revision_before:?} messages={messages_before}");

    // ② **只发一次** start-turn（生产路径：wire 形态与生产逐字一致）
    let action_id = format!("live-send-{}", uuid_like());
    let client_message_id = uuid_like();
    let started = std::time::Instant::now();
    let outcome = adapter
        .start_turn_compat(&key, &action_id, &text, &client_message_id)
        .await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    println!(
        "[send-probe] start-turn 结果：delivery={:?} elapsedMs={elapsed_ms} result={:?}",
        outcome.delivery, outcome.result
    );

    // ③ 事后核对「生效」：等新 snapshot（revision 前进 / 消息变多）——受理 ≠ 生效
    let mut turned = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let p = adapter.projection(&key).await;
        let revision_moved = match (revision_before, p.revision) {
            (Some(a), Some(b)) => b > a,
            (None, Some(_)) => true,
            _ => false,
        };
        if revision_moved || p.messages.len() > messages_before {
            turned = true;
            break;
        }
    }
    println!("[send-probe] 生效核对：状态前进={turned}");

    let report = serde_json::json!({
        "_source": "Tuvis 真机 send 闭环受控探针（生产 adapter 路径；只发一帧）",
        "platform": platform,
        "build": build,
        "threadId": thread_id,
        "delivery": format!("{:?}", outcome.delivery),
        "accepted": matches!(outcome.delivery, Delivery::Accepted),
        "elapsedMs": elapsed_ms,
        "revisionBefore": revision_before,
        "messagesBefore": messages_before,
        "stateAdvanced": turned,
        "note": "text 长度与内容不入盘（不落会话正文）",
        "textLen": text.chars().count(),
    });
    let out = std::env::temp_dir().join(format!("tuvis-live-send-{platform}-{build}.json"));
    std::fs::write(
        &out,
        format!("{}\n", serde_json::to_string_pretty(&report).unwrap()),
    )
    .expect("写证据文件失败");
    println!("[send-probe] 证据文件：{}", out.display());
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}

/// 无依赖的 uuid 形态串（v4 形）；只需**唯一**，不需密码学强度。
fn uuid_like() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let raw = format!(
        "{:032x}",
        (nanos << 32) ^ (pid << 17) ^ nanos.rotate_left(7)
    );
    format!(
        "{}-{}-4{}-a{}-{}",
        &raw[0..8],
        &raw[8..12],
        &raw[13..16],
        &raw[17..20],
        &raw[20..32]
    )
}
