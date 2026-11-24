//! G1-Mac 真机驱动（证据工具，非产品功能）。
//! 使用 `cargo run --example dsh-g1-driver` 单独启动。
//!
//! 只调用生产入口（`production_env/manager/auth` + `identity::discover` +
//! `transport::connect`），用于在真实官方 Desktop 上执行计划 T06 的 G1 门槛：
//! 自有插件取得同 Host 独立 API Peer、真实读/发送、撤权后旧连接拒绝、重启换代。
//! 各阶段独立进程运行；绑定账本落在 `~/.tuvis/dsh/`（debug 未设 `TUVIS_HOME` 时
//! 生产根回落 `~/.tuvis`，与插件默认根一致——Mac 补探测 §6）。
//!
//! 纪律：**不打印任何秘密**（cookie/token/私钥）；binding_id 与 display_code 非
//! 秘密（账本键与人工核对码）。真实发送带明确「可忽略」标注并随后 cancel。
//! 客户端身份重建：公钥取账本行（非秘密），pid/exe/启动时刻取 `discover` 的
//! **当前**宿主进程（换代后账本行里的旧 PID 不再有效——`authorize_grant` 按
//! 进程表现值核验）。

use std::time::Duration;

use multi_agents_manager_lib::services::dsh_control as dsh;
use multi_agents_manager_lib::services::dsh_control::identity;
use multi_agents_manager_lib::services::dsh_control::install::DshPluginManager;
use multi_agents_manager_lib::services::dsh_control::model::{
    ClientIdentity, ManagementKind, ManagementPhase, NativeResult,
};
use multi_agents_manager_lib::services::dsh_control::transport as dsh_transport;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 读取 `<state_dir>/bindings.json` 的最新 bound 行（非秘密字段；自有 0600 账本）。
/// 状态目录按 R10 的唯一规则本地解析（与生产 `resolve_state_dir` 同语义）。
fn read_bound_row() -> Result<(String, String), String> {
    let dir = std::env::var_os("TUVIS_DSH_STATE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".tuvis")
                .join("dsh")
        });
    let raw = std::fs::read_to_string(dir.join("bindings.json"))
        .map_err(|e| format!("read bindings.json: {e}"))?;
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| format!("parse: {e}"))?;
    let rows = v
        .get("bindings")
        .and_then(|b| b.as_array())
        .ok_or("bindings.json 缺 bindings 数组")?;
    let bound = rows
        .iter()
        .rev()
        .find(|r| r.get("state").and_then(|s| s.as_str()) == Some("bound"))
        .ok_or("无 bound 行")?;
    let id = bound
        .get("bindingId")
        .and_then(|x| x.as_str())
        .ok_or("缺 bindingId")?
        .to_string();
    let spki = bound
        .get("publicKeySpki")
        .and_then(|x| x.as_str())
        .ok_or("缺 publicKeySpki")?
        .to_string();
    Ok((id, spki))
}

async fn wait_job(manager: &DshPluginManager, id: &str) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    loop {
        match manager.job(id) {
            Ok(j) => match j.phase {
                ManagementPhase::Succeeded => {
                    println!("G1 install job_phase=Succeeded kind={:?}", j.kind);
                    return Ok(());
                }
                ManagementPhase::Failed | ManagementPhase::Cancelled => {
                    return Err(format!("job 终态 {:?} reason={:?}", j.phase, j.reason_code));
                }
                _ => {}
            },
            Err(e) => return Err(format!("job 查询失败: {e}")),
        }
        if std::time::Instant::now() > deadline {
            return Err("管理作业超时（300s）".to_string());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn cmd_status() -> Result<(), String> {
    let st = dsh::production::production_manager()
        .map_err(|e| e.to_string())?
        .status()
        .map_err(|e| e.to_string())?;
    println!(
        "G1 status installation={:?} activation={:?} authorization={:?} desktop={:?} plugin={:?} reason={:?}",
        st.installation, st.activation, st.authorization, st.desktop_version, st.plugin_version, st.reason_code
    );
    Ok(())
}

async fn cmd_install() -> Result<(), String> {
    let manager = dsh::production::production_manager().map_err(|e| e.to_string())?;
    let id = manager
        .start(ManagementKind::Install)
        .map_err(|e| format!("start(Install): {e}"))?;
    println!("G1 install job_id={id}");
    wait_job(&manager, &id).await?;
    cmd_status().await
}

/// 句柄获取轮询：批准/重连交付 cookie 后 `authorize_grant` 才成功。
async fn wait_peer(
    svc: &std::sync::Arc<dsh::auth::DshAuthService>,
    host: &dsh::model::LocalHost,
    client: &ClientIdentity,
    timeout: Duration,
) -> Result<dsh::model::AuthorizedPeer, String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match svc.authorize_grant(host, client).await {
            Ok(p) => return Ok(p),
            Err(e) => {
                if std::time::Instant::now() > deadline {
                    return Err(format!("等待授权句柄超时: 最后错误 {}", e.code));
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
}

/// 首绑（rebind=false）：serve → 等 pending →（外部截图窗口）→ approve → 句柄 → 真实读/写。
/// 换代重绑（rebind=true）：钉定键免批准，直接等句柄。
async fn cmd_bind(rebind: bool) -> Result<(), String> {
    let env = dsh::production::production_env();
    let svc = dsh::production::production_auth().map_err(|e| format!("auth 域初始化: {e}"))?;
    svc.serve().await.map_err(|e| format!("serve: {e}"))?;
    println!("G1 bind serving");

    let (binding_id, client) = if !rebind {
        let wait: u64 = std::env::var("TUVIS_G1_BIND_WAIT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(150);
        let deadline = std::time::Instant::now() + Duration::from_secs(wait);
        let view = loop {
            if let Some(v) = svc.pending_binding() {
                break v;
            }
            if std::time::Instant::now() > deadline {
                return Err(format!("等待插件 pending 超时（{wait}s）"));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        println!(
            "G1 bind pending binding_id={} display_code={} expires_at_ms={}",
            view.binding.binding_id, view.binding.display_code, view.binding.expires_at_ms
        );
        println!("G1 bind screenshot_window_until_ms={}", now_ms() + 20_000);
        tokio::time::sleep(Duration::from_secs(20)).await; // 外部在此窗口截取 Desktop 授权卡
        let id = view.binding.binding_id.clone();
        let client = view.client.clone();
        svc.approve(&id)
            .await
            .map_err(|e| format!("approve: {e}"))?;
        println!("G1 bind approved binding_id={id}");
        (id, client)
    } else {
        let (id, spki) = read_bound_row()?;
        let host = identity::discover(&env).map_err(|e| format!("discover: {e}"))?;
        println!(
            "G1 rebind pinned binding_id={id} host_authority={} host_epoch={}",
            host.authority, host.epoch
        );
        // 账本行存的是 SPKI 的 base64url 字符串；ClientIdentity 要原始 DER 字节。
        use base64::Engine as _;
        let spki_der = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(spki.trim_end_matches('='))
            .map_err(|e| format!("decode ledger spki: {e}"))?;
        (
            id,
            ClientIdentity {
                public_key_spki: spki_der,
                executable: host.executable.clone(),
                pid: host.pid,
                started_at_ms: host.started_at_ms,
            },
        )
    };

    let host = identity::discover(&env).map_err(|e| format!("discover: {e}"))?;
    let peer_wait: u64 = std::env::var("TUVIS_G1_BIND_WAIT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(90);
    let peer = wait_peer(&svc, &host, &client, Duration::from_secs(peer_wait)).await?;
    println!(
        "G1 peer authority={} epoch={}",
        peer.authority(),
        peer.epoch()
    );

    let conn = dsh_transport::connect(peer)
        .await
        .map_err(|e| format!("transport::connect: {e}"))?;
    println!("G1 connect write_available={}", conn.write_available());

    // 真实读：modelCatalog（E2 验证形状）+ session/list（E2：保留参数名 `_request`）。
    for (rid, method, args) in [
        ("g1-read-1", "session/modelCatalog", serde_json::json!({})),
        (
            "g1-read-2",
            "session/list",
            serde_json::json!({"_request":{}}),
        ),
    ] {
        match conn.request(rid, method, &args).await {
            Ok(r) => println!(
                "G1 read {method} rpc_id={} ok={}",
                r.rpc_id,
                matches!(r.result, NativeResult::Success { .. })
            ),
            Err(e) => println!(
                "G1 read {method} FAILED code={} detail={}",
                e.code, e.detail
            ),
        }
    }

    // 真实写：新建明确标注的验证会话 → 真实发送 → cancel 停回合。
    let g1_dir = std::env::temp_dir().join("tuvis-g1-workspace");
    let _ = std::fs::create_dir_all(&g1_dir);
    let created = conn
        .request(
            "g1-write-1",
            "session/create",
            &serde_json::json!({
                "request": {"cwd": g1_dir.to_string_lossy(), "agentPreset": "standard"}
            }),
        )
        .await;
    let reused = std::env::var("TUVIS_G1_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty());
    let session_id = reused.or_else(|| {
        created.ok().and_then(|r| match r.result {
            NativeResult::Success { value } => value
                .get("sessionId")
                .and_then(|s| s.as_str())
                .map(str::to_string),
            _ => None,
        })
    });
    match session_id {
        Some(sid) => {
            println!("G1 write session/create sessionId={sid}");
            let prompt = conn
                .request(
                    "g1-write-2",
                    "session/prompt",
                    &serde_json::json!({
                        "request": {
                            "requestId": format!("tuvis-g1-{}", now_ms()),
                            "sessionId": sid,
                            "mode": "queue",
                            "content": [{"type":"text","text":"【兔维斯 G1 连通性验证】本条消息由兔维斯远控验证驱动发送，可忽略本会话。"}],
                            "clientTimeZone": "Asia/Shanghai"
                        }
                    }),
                )
                .await;
            match &prompt {
                Ok(r) => println!(
                    "G1 write session/prompt rpc_id={} result={:?}",
                    r.rpc_id, r.result
                ),
                Err(e) => println!(
                    "G1 write session/prompt FAILED code={} detail={}",
                    e.code, e.detail
                ),
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
            match conn
                .request(
                    "g1-write-3",
                    "session/cancel",
                    &serde_json::json!({"request":{"sessionId": sid}}),
                )
                .await
            {
                Ok(r) => println!(
                    "G1 write session/cancel rpc_id={} ok={}",
                    r.rpc_id,
                    matches!(r.result, NativeResult::Success { .. })
                ),
                Err(e) => println!(
                    "G1 write session/cancel FAILED code={} detail={}",
                    e.code, e.detail
                ),
            }
        }
        None => println!("G1 write session/create FAILED（无 sessionId）——写面按失败记录"),
    }

    conn.close().await;
    println!(
        "G1 bind done authority={} epoch={}",
        host.authority, host.epoch
    );
    let _ = binding_id;
    Ok(())
}

/// 撤权：serve（插件重连交付新代 cookie）→ 持活连接 → 撤 → 活连接被关 →
/// 旧句柄重连拒绝 → 插件落 tombstone（identity.json 只读非秘密标记）。
async fn cmd_revoke() -> Result<(), String> {
    let env = dsh::production::production_env();
    let svc = dsh::production::production_auth().map_err(|e| format!("auth 域初始化: {e}"))?;
    svc.serve().await.map_err(|e| format!("serve: {e}"))?;
    let (binding_id, spki) = read_bound_row()?;
    let host = identity::discover(&env).map_err(|e| format!("discover: {e}"))?;
    use base64::Engine as _;
    let spki_der = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(spki.trim_end_matches('='))
        .map_err(|e| format!("decode ledger spki: {e}"))?;
    let client = ClientIdentity {
        public_key_spki: spki_der,
        executable: host.executable.clone(),
        pid: host.pid,
        started_at_ms: host.started_at_ms,
    };
    let peer = wait_peer(&svc, &host, &client, Duration::from_secs(150)).await?;
    let conn = dsh_transport::connect(peer.clone())
        .await
        .map_err(|e| format!("connect: {e}"))?;
    println!(
        "G1 revoke pre_revoke write_available={}",
        conn.write_available()
    );

    svc.revoke(&binding_id)
        .await
        .map_err(|e| format!("revoke: {e}"))?;
    println!("G1 revoke called binding_id={binding_id}");
    tokio::time::sleep(Duration::from_secs(3)).await;
    println!(
        "G1 revoke post_revoke write_available={}",
        conn.write_available()
    );
    match conn
        .request(
            "g1-post-revoke",
            "session/modelCatalog",
            &serde_json::json!({}),
        )
        .await
    {
        Ok(_) => {
            println!("G1 revoke post_revoke_request=UNEXPECTED_OK（撤后活连接仍可请求——R08 违约）")
        }
        Err(e) => println!("G1 revoke post_revoke_request_refused code={}", e.code),
    }
    conn.close().await;
    match dsh_transport::connect(peer).await {
        Ok(_) => println!("G1 revoke old_peer_reconnect=UNEXPECTED_OK（R08 违约）"),
        Err(e) => println!("G1 revoke old_peer_reconnect_refused code={}", e.code),
    }
    match svc.authorize_grant(&host, &client).await {
        Ok(_) => println!("G1 revoke authorize_grant=UNEXPECTED_OK"),
        Err(e) => println!("G1 revoke authorize_grant_refused code={}", e.code),
    }
    // 插件侧 tombstone 证据：identity.json 的非秘密标记。
    let profile = host.profile_dir.join("tuvis-remote").join("identity.json");
    match std::fs::read_to_string(&profile) {
        Ok(raw) => {
            let v: serde_json::Value =
                serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
            println!(
                "G1 revoke plugin_identity_revoked={} plugin_revoked_binding_id={}",
                v.get("revoked").and_then(|b| b.as_bool()).unwrap_or(false),
                v.get("revokedBindingId")
                    .and_then(|b| b.as_str())
                    .unwrap_or("null")
            );
        }
        Err(e) => println!("G1 revoke plugin_identity_read_failed={e}"),
    }
    Ok(())
}

async fn cmd_cleanup() -> Result<(), String> {
    let manager = dsh::production::production_manager().map_err(|e| e.to_string())?;
    let id = manager
        .start(ManagementKind::Uninstall)
        .map_err(|e| format!("start(Uninstall): {e}"))?;
    wait_job(&manager, &id).await?;
    cmd_status().await
}

#[tokio::main]
async fn main() {
    let phase = std::env::args().nth(1).unwrap_or_default();
    let res = match phase.as_str() {
        "status" => cmd_status().await,
        "install" => cmd_install().await,
        "bind" => cmd_bind(false).await,
        "rebind" => cmd_bind(true).await,
        "revoke" => cmd_revoke().await,
        "cleanup" => cmd_cleanup().await,
        other => Err(format!(
            "未知阶段 {other}（status|install|bind|rebind|revoke|cleanup）"
        )),
    };
    if let Err(e) = res {
        println!("G1 {phase} ERROR {e}");
        std::process::exit(1);
    }
}
