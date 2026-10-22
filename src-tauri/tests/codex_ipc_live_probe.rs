//! #172 真机「未知 build 能力探测」只读探针（**`#[ignore]`、需用户在��**）。
//!
//! 目的：在一个**真实**的未知 build 上，跑**生产代码路径**，产出可归档的证据：
//! 1. endpoint 身份（platform / build）；
//! 2. 指纹（`cli_id` + 账本键 `platform|build|cli_id|epoch`）——生产 `probe::fingerprint`；
//! 3. 官方 schema 探针（内容指纹 + command/file 的 decision 枚举）——生产 `probe::schema_probe`；
//! 4. **只读探测执行器**（P0→P1→P2→P3）——生产 adapter 的 `ensure_read_evidence`，
//!    走真实 IPC（owner discovery → 临时 following → 首个 snapshot 判据）。
//!
//! **只读**：不发任何写帧（不 start-turn、不点审批、不答问题）；不点任何 Desktop 界面；
//! 不改 Desktop 设置；临时 following 立即撤销。
//!
//! 运行方式（在 `src-tauri/` 下）：
//!
//! ```powershell
//! $env:TUVIS_CAPTURE_THREAD = "<thread-uuid>"   # 可选：不设则自动枚举最近的会话候选
//! cargo test --features marker-helper --test codex_ipc_live_probe -- --ignored --nocapture
//! ```
//!
//! 产物：打印摘要 + 写 `%TEMP%\tuvis-live-probe-<platform>-<build>.json`（**不含任何会话
//! 正文**：只有 build/指纹/枚举词/探测结论）。

use std::sync::Arc;

use multi_agents_manager_lib::services::codex_desktop_ipc::adapter::{
    DesktopIpcAdapter, ProductionConnector,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::probe::fingerprint::probe_key;
use multi_agents_manager_lib::services::codex_desktop_ipc::probe::schema_probe::{
    probe_schema, SchemaProbeOutcome,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::profile::ProtocolProfile;
use multi_agents_manager_lib::services::codex_desktop_ipc::transport::EndpointResolver;
use multi_agents_manager_lib::services::codex_desktop_ipc::types::ThreadKey;

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

/// 会话候选（最近的 rollout 文件名 UUID）——与 live_capture/decision 探针同源
/// （复用 support::codex_live 的枚举口径，避免三处各写一份）。
#[path = "support/codex_live.rs"]
mod codex_live;
use codex_live::candidate_threads;

#[tokio::test]
#[ignore = "真机只读探针：需 Desktop 在运行；用 --ignored 显式运行"]
async fn probe_live_unknown_build() {
    let resolver = production_resolver();
    let endpoint = resolver
        .resolve()
        .expect("Desktop IPC endpoint 不可达（Desktop 没起？）");
    let platform = endpoint.platform.clone();
    let build = endpoint.build.clone().unwrap_or_default();
    println!("[probe] platform={platform} build={build}");

    // 2) 指纹（生产代码：cli_id 可能要跑一次 `codex --version`，≤2s）
    let key = probe_key(&platform, Some(build.as_str()));
    let key_str = key.to_key();
    println!("[probe] cli_id={:?} probe_key={key_str}", key.cli_id);

    // 3) 官方 schema 探针（只读：跑随包 CLI 的 generate-json-schema，≤3s）
    let schema = probe_schema().await;
    let schema_json = match &schema {
        SchemaProbeOutcome::Known(ev) => serde_json::json!({
            "known": true,
            "fingerprint": ev.fingerprint,
            "commandDecisions": ev.command_decisions,
            "fileDecisions": ev.file_decisions,
        }),
        SchemaProbeOutcome::Unknown { error_code } => serde_json::json!({
            "known": false,
            "errorCode": error_code,
        }),
    };
    match &schema {
        SchemaProbeOutcome::Known(ev) => println!(
            "[probe] schema fingerprint={} command={:?} file={:?}",
            ev.fingerprint, ev.command_decisions, ev.file_decisions
        ),
        SchemaProbeOutcome::Unknown { error_code } => {
            println!("[probe] schema 未取证（{error_code}）")
        }
    }

    // 4) 只读探测执行器（生产 adapter；「尚无证据」档 → 允许探测）
    let profile = ProtocolProfile::unverified(&platform, &build);
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(ProductionConnector {
            resolver: resolver.clone(),
        }),
        profile,
    ));

    let mut candidates: Vec<String> = std::env::var("TUVIS_CAPTURE_THREAD")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .into_iter()
        .collect();
    if candidates.is_empty() {
        candidates = candidate_threads(5);
    }
    assert!(!candidates.is_empty(), "没有可试的会话候选 id");

    let mut probe_json = serde_json::json!(null);
    for thread_id in candidates {
        let tkey = ThreadKey {
            host_id: "local".into(),
            thread_id: thread_id.clone(),
        };
        adapter.ensure_read_evidence(&tkey).await;
        let status = adapter.probe_status().await;
        let caps = adapter.capabilities();
        println!(
            "[probe] thread={thread_id} status={:?} stage={:?} reason={:?} caps send={} state={} command={}",
            status.state, status.stage, status.reason_code, caps.send, caps.state, caps.command
        );
        probe_json = serde_json::json!({
            "threadId": thread_id,
            "state": status.state,
            "stage": status.stage,
            "reasonCode": status.reason_code,
            "capabilities": {
                "send": caps.send,
                "state": caps.state,
                "command": caps.command,
                "permissions": caps.permissions,
                "blockingInput": caps.blocking_input,
            },
        });
        // 读链取证成功即可收工（后续线程不必再探）
        if caps.state {
            break;
        }
    }

    // 5) 产物（不含会话正文：只有 build / 指纹 / 枚举词 / 探测结论）
    let report = serde_json::json!({
        "_source": "Tuvis 真机只读探测（生产代码路径）：endpoint 身份 + 指纹 + 官方 schema + 只读探测 P0..P3",
        "platform": platform,
        "build": build,
        "cliId": key.cli_id,
        "probeKey": key_str,
        "epoch": key.epoch,
        "schema": schema_json,
        "readProbe": probe_json,
    });
    let out = std::env::temp_dir().join(format!("tuvis-live-probe-{platform}-{build}.json"));
    std::fs::write(&out, format!("{}\n", serde_json::to_string_pretty(&report).unwrap()))
        .expect("写证据文件失败");
    println!("[probe] 证据文件：{}", out.display());
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}
