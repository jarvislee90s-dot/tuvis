// ZCode 官方远控的桌面命令面：A4 链接配置 + T7.1 统一控制（C2a）。
//
// 合同（T7.1 接口冻结）：
// - `zcode_remote_config_get` 保持原 DTO：只读 settings KV，读取即复验，不发外部 HTTP；
// - `zcode_remote_config_set` 先持久化（同步块内完成并释放 DB 锁），**成功后才**通知
//   同一个 Hub `config_changed()`；失败原样回错，且不触碰旧连接（旧 JWT/relay 不动）；
// - `zcode_control_authorize/connect/disconnect/status` 只回 `ConnectionView`，
//   绝不返回凭据；授权只在用户显式点击时经 Rust 调 opener 打开系统浏览器，
//   启动路径零 OAuth、零连接（Hub 构造本身也不自动授权/连接）；
// - 错误统一映射 `ZCODE_CONTROL:<冻结码>`（errcode.rs 约定）；官方上游原文一律丢弃，
//   不拼进错误串（`ZcodeError` 本身只携带冻结码）。
//
// 本切片只交付命令；`app.manage(Arc<ZcodeHub>)`（与 RemoteState 共用同一 Arc）在
// 下一片 C2b 装配。未装配时：四个控制命令由 Tauri 返回「state not managed」；
// `config_set` 仍照常保存、只是没有旧连接可通知（不引入任何隐藏自动连接）。

use std::sync::Arc;

use rusqlite::Connection;
use tauri::{AppHandle, Manager, State};

use crate::errcode::ZCODE_CONTROL_PREFIX;
use crate::services::zcode_control::{
    codes, get_config, set_config, ConnectionView, OAuthProvider, OpenAuthorize, ZResult,
    ZcodeError, ZcodeHub, ZcodeRemoteConfig,
};

/// 读取本机 ZCode 官方远控链接配置（读取即复验：历史坏值 → Invalid/url null）。
/// 只读 settings KV，不读取官方账号、不发任何外部 HTTP。
#[tauri::command]
pub fn zcode_remote_config_get() -> Result<ZcodeRemoteConfig, String> {
    let conn = crate::database::connection::DB.lock().unwrap();
    Ok(get_config(&conn))
}

/// 保存 ZCode 官方远控链接（空白 = 删除）。校验通过才写入；失败回传码前缀错误
/// （不含原 URL），已存合法值保持不变；保存成功才通知 Hub 推进代际并断开旧连接。
#[tauri::command]
pub async fn zcode_remote_config_set(
    app: AppHandle,
    raw_url: String,
) -> Result<ZcodeRemoteConfig, String> {
    // C2b 才装配 managed Hub：未装配时无既有连接可通知，保存本身照常成功。
    let hub = app.try_state::<Arc<ZcodeHub>>();
    let hub: Option<&ZcodeHub> = hub.as_ref().map(|state| state.inner().as_ref());
    config_set_with(
        || {
            let conn = crate::database::connection::DB.lock().unwrap();
            persist_config(&conn, &raw_url)
        },
        hub,
    )
    .await
}

/// 命令内核：先持久化（闭包内同步完成，DB 锁随闭包返回释放），**成功后才**通知 Hub。
///
/// 失败（格式/写库）直接回错且零通知；旧代际连接由 Hub 保持原样。
async fn config_set_with(
    persist: impl FnOnce() -> Result<ZcodeRemoteConfig, String>,
    hub: Option<&ZcodeHub>,
) -> Result<ZcodeRemoteConfig, String> {
    let saved = persist()?;
    if let Some(hub) = hub {
        hub.config_changed().await;
    }
    Ok(saved)
}

/// 持久化配置（校验/写库/错误码全在 config 服务内）；调用方不得持锁跨越 await。
fn persist_config(conn: &Connection, raw_url: &str) -> Result<ZcodeRemoteConfig, String> {
    set_config(conn, raw_url).map_err(|error| error.to_string())
}

/// 走官方授权（先生成回环回调 URL，再交给系统浏览器）。
///
/// 只在用户点击授权时调用；`provider` 只接受官方两个通道字面量，其余
/// `invalid_action`（零浏览器、零后端调用）。
#[tauri::command]
pub async fn zcode_control_authorize(
    provider: String,
    state: State<'_, Arc<ZcodeHub>>,
) -> Result<ConnectionView, String> {
    authorize_with(&state, &provider, system_browser_opener()).await
}

/// 建立唯一物理连接（同 Hub 的并发调用幂等复用；`conflict` 终态不自动重试）。
#[tauri::command]
pub async fn zcode_control_connect(
    state: State<'_, Arc<ZcodeHub>>,
) -> Result<ConnectionView, String> {
    connect_with(&state).await
}

/// 用户显式断开（保留本机内存 JWT，可再次连接）。
#[tauri::command]
pub async fn zcode_control_disconnect(
    state: State<'_, Arc<ZcodeHub>>,
) -> Result<ConnectionView, String> {
    Ok(disconnect_with(&state).await)
}

/// 只读连接状态（不含凭据/URL；桌面与手机看到同一个 Hub 视图）。
#[tauri::command]
pub async fn zcode_control_status(
    state: State<'_, Arc<ZcodeHub>>,
) -> Result<ConnectionView, String> {
    Ok(status_with(&state))
}

/// T8.2 **仅 debug 证据构建**：桌面为指定会话/代际/能力发放一条内存验收许可。
///
/// - 只存在于 `zcode-bridge-evidence` + debug 构建（release 由 zcode_control/mod.rs
///   编译门直接拒绝；普通构建不存在本命令）；
/// - 不写 settings/DB、不写日志，也不提供任何手机 HTTP 入口；
/// - 许可 120 秒有效、一次用户动作即消费（snooze 只绑题不消费）；
/// - 只放宽能力闸，目标/权限 `fullAccess`/pending 等守卫全部照常执行。
#[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
#[tauri::command]
pub fn zcode_control_arm_evidence(
    state: State<'_, Arc<ZcodeHub>>,
    session_id: String,
    generation: u64,
    capability: String,
) -> Result<(), String> {
    state
        .arm_evidence(&session_id, generation, &capability)
        .map(|_| ())
        .map_err(ipc_error)
}

/// provider 解析单点：只认 `bigmodel`/`zai`，大小写/别名/空串一律拒绝。
fn parse_provider(raw: &str) -> ZResult<OAuthProvider> {
    match raw {
        "bigmodel" => Ok(OAuthProvider::Bigmodel),
        "zai" => Ok(OAuthProvider::Zai),
        _ => Err(ZcodeError::of(codes::INVALID_ACTION)),
    }
}

/// 生产 opener：唯一打开系统浏览器的位置（Rust 侧调用，不做脚本化登录/自动点击）。
fn system_browser_opener() -> OpenAuthorize {
    Arc::new(|url: String| {
        tauri_plugin_opener::open_url(url, None::<&str>).map_err(|error| {
            log::warn!("打开 ZCode 授权页失败：{error}");
            ZcodeError::of(codes::UNKNOWN)
        })
    })
}

/// 冻结码 → IPC 错误串（本地码，绝不续接官方上游原文）。
fn ipc_error(error: ZcodeError) -> String {
    format!("{ZCODE_CONTROL_PREFIX}{}", error.code)
}

async fn authorize_with(
    hub: &ZcodeHub,
    raw_provider: &str,
    open: OpenAuthorize,
) -> Result<ConnectionView, String> {
    let provider = parse_provider(raw_provider).map_err(ipc_error)?;
    hub.authorize(provider, open).await.map_err(ipc_error)
}

async fn connect_with(hub: &ZcodeHub) -> Result<ConnectionView, String> {
    hub.connect().await.map_err(ipc_error)
}

async fn disconnect_with(hub: &ZcodeHub) -> ConnectionView {
    hub.disconnect().await
}

fn status_with(hub: &ZcodeHub) -> ConnectionView {
    hub.connection()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;

    use futures::future::BoxFuture;

    use crate::remote::pairing::DeviceStore;
    use crate::services::zcode_control::config::set_config as store_set_config;
    use crate::services::zcode_control::{
        ActionContext, ActionReceipt, AuthStatus, BoundTarget, CapabilityView, ConfigState,
        ConnectionState, ControlAction, DeviceRoute, HubBackend, SessionView, TargetKey,
    };

    const URL: &str = "https://zcode.z.ai/remote/v4?sid=synthetic-cmd-0001&name=SyntheticCmd";
    const URL_NEXT: &str = "https://zcode.z.ai/remote/v4?sid=synthetic-cmd-0002&name=SyntheticCmd2";

    #[derive(Default)]
    struct FakeBackend {
        authorize_calls: AtomicUsize,
        reset_calls: AtomicUsize,
        reset_generations: Mutex<Vec<u64>>,
        /// 授权路径的真实状态：authorize 置 true，forget 置 false；
        /// `config_changed` 之后 Hub 必须据此保留/清除缓存（不得默认未授权）。
        authorized: AtomicBool,
    }

    impl FakeBackend {
        fn status(&self) -> AuthStatus {
            AuthStatus {
                authorized: self.authorized.load(Ordering::SeqCst),
                ..AuthStatus::default()
            }
        }
    }

    impl HubBackend for FakeBackend {
        fn authorize<'a>(
            &'a self,
            _provider: OAuthProvider,
            _open: OpenAuthorize,
        ) -> BoxFuture<'a, ZResult<()>> {
            self.authorize_calls.fetch_add(1, Ordering::SeqCst);
            self.authorized.store(true, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }

        fn restore_authorization<'a>(&'a self) -> BoxFuture<'a, AuthStatus> {
            let status = self.status();
            Box::pin(async move { status })
        }

        fn auth_status(&self) -> AuthStatus {
            self.status()
        }

        fn forget_authorization<'a>(&'a self) -> BoxFuture<'a, AuthStatus> {
            self.authorized.store(false, Ordering::SeqCst);
            let status = self.status();
            Box::pin(async move { status })
        }

        fn connect<'a>(&'a self, _route: DeviceRoute) -> BoxFuture<'a, ZResult<ConnectionView>> {
            Box::pin(async {
                Ok(ConnectionView {
                    state: ConnectionState::Paired,
                    generation: 1,
                    reason_code: None,
                    device_label: Some("synthetic-device".into()),
                })
            })
        }

        fn disconnect<'a>(&'a self) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }

        fn reset<'a>(&'a self, generation: u64) -> BoxFuture<'a, ()> {
            self.reset_calls.fetch_add(1, Ordering::SeqCst);
            self.reset_generations.lock().unwrap().push(generation);
            Box::pin(async {})
        }

        fn bind_session<'a>(&'a self, _key: &'a TargetKey) -> BoxFuture<'a, ZResult<BoundTarget>> {
            Box::pin(async { Err(ZcodeError::of(codes::NO_SESSION)) })
        }

        fn session<'a>(&'a self, _target: &'a BoundTarget) -> BoxFuture<'a, ZResult<SessionView>> {
            Box::pin(async { Err(ZcodeError::of(codes::NOT_READY)) })
        }

        fn execute<'a>(
            &'a self,
            _ctx: &'a ActionContext,
            _target: &'a BoundTarget,
            _action: &'a ControlAction,
        ) -> BoxFuture<'a, ZResult<ActionReceipt>> {
            Box::pin(async { Err(ZcodeError::of(codes::NOT_READY)) })
        }

        fn policy(&self) -> CapabilityView {
            CapabilityView::default()
        }
    }

    fn memory_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn
    }

    fn stored(url: &str) -> DeviceStore {
        let store = DeviceStore::memory();
        store.with(|conn| store_set_config(conn, url).unwrap());
        store
    }

    fn noop_open() -> OpenAuthorize {
        Arc::new(|_| Ok(()))
    }

    fn hub_with(fake: &Arc<FakeBackend>, url: &str) -> ZcodeHub {
        ZcodeHub::with_parts(stored(url), fake.clone(), CapabilityView::default())
    }

    /// 非法 provider（含大小写/空白/别名）一律 `invalid_action`，且在触达 Hub 之前拒绝：
    /// 零授权调用、零浏览器。
    #[tokio::test]
    async fn invalid_provider_is_rejected() {
        assert_eq!(parse_provider("bigmodel").unwrap(), OAuthProvider::Bigmodel);
        assert_eq!(parse_provider("zai").unwrap(), OAuthProvider::Zai);
        for bad in [
            "",
            "BigModel",
            "bigmodel ",
            "zai\n",
            "openai",
            "z.ai",
            "big-model",
        ] {
            let error = parse_provider(bad).unwrap_err();
            assert_eq!(error.code, codes::INVALID_ACTION, "provider={bad:?}");
            assert_eq!(ipc_error(error), "ZCODE_CONTROL:invalid_action");
        }

        let fake = Arc::new(FakeBackend::default());
        let hub = hub_with(&fake, URL);
        let error = authorize_with(&hub, "openai", noop_open())
            .await
            .unwrap_err();
        assert_eq!(error, "ZCODE_CONTROL:invalid_action");
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 0);
        assert_eq!(hub.connection().state, ConnectionState::NeedsAuth);
    }

    /// 持久化失败（无 schema 的库 / 非法链接）：回码前缀错误，Hub 零通知、代际不变。
    #[tokio::test]
    async fn config_set_failure_does_not_notify_hub() {
        let fake = Arc::new(FakeBackend::default());
        let hub = hub_with(&fake, URL);
        let before = hub.connection();

        // settings 表不存在 → ZCODE_URL_STORAGE（绝不假报 Saved/Missing）。
        let broken = Connection::open_in_memory().unwrap();
        let error = config_set_with(|| persist_config(&broken, URL_NEXT), Some(&hub))
            .await
            .unwrap_err();
        assert!(error.contains("ZCODE_URL_STORAGE"), "{error}");

        // 格式拒绝同样不通知。
        let conn = memory_db();
        let error = config_set_with(
            || persist_config(&conn, "http://zcode.z.ai/remote/v4?sid=x"),
            Some(&hub),
        )
        .await
        .unwrap_err();
        assert!(error.starts_with("ZCODE_URL_SCHEME"), "{error}");

        assert_eq!(fake.reset_calls.load(Ordering::SeqCst), 0);
        assert!(fake.reset_generations.lock().unwrap().is_empty());
        assert_eq!(hub.connection(), before, "失败保存不得改动旧连接状态");
    }

    /// 保存成功：先落库、再通知同一 Hub；代际推进、断旧 relay/作废投影，**账号保留**。
    #[tokio::test]
    async fn config_set_success_notifies_and_advances_generation() {
        let fake = Arc::new(FakeBackend::default());
        let hub = hub_with(&fake, URL);
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        assert_eq!(
            fake.authorize_calls.load(Ordering::SeqCst),
            1,
            "前置：已授权一次"
        );
        let before = hub.connection().generation;

        let conn = memory_db();
        let saved = config_set_with(|| persist_config(&conn, URL_NEXT), Some(&hub))
            .await
            .unwrap();
        assert_eq!(saved.config_state, ConfigState::Saved);
        assert_eq!(saved.url.as_deref(), Some(URL_NEXT));
        assert_eq!(get_config(&conn).url.as_deref(), Some(URL_NEXT));

        let after = hub.connection();
        assert!(after.generation > before, "配置变更必须推进代际");
        assert_eq!(
            after.state,
            ConnectionState::Offline,
            "换链接保留账号 JWT（不再退回 needs_auth）"
        );
        assert!(
            hub.auth_status().authorized,
            "Hub 授权缓存必须按后端真实状态保留"
        );
        assert_eq!(fake.reset_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *fake.reset_generations.lock().unwrap(),
            vec![after.generation],
            "reset 必须拿到新代际（断 relay/作废投影）"
        );
        // 旧 JWT 保留：无需重新授权即可连新链接，OAuth 次数不增加。
        assert!(hub.connect().await.unwrap().is_paired());
        assert_eq!(
            fake.authorize_calls.load(Ordering::SeqCst),
            1,
            "换链接不得重新 OAuth"
        );
    }

    /// status 只读返回 `ConnectionView` 四字段，序列化后绝无凭据/URL/token。
    #[tokio::test]
    async fn status_returns_view_without_credentials() {
        let fake = Arc::new(FakeBackend::default());
        let hub = hub_with(&fake, URL);
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();

        let view = status_with(&hub);
        assert_eq!(view, hub.connection());
        assert_eq!(view.state, ConnectionState::Offline);
        assert_eq!(view.reason_code, None);

        let wire = serde_json::to_string(&view).unwrap();
        for forbidden in [
            "token",
            "jwt",
            "credential",
            "secret",
            "authorization",
            "sid=",
        ] {
            assert!(
                !wire.contains(forbidden),
                "status 不得含 {forbidden}: {wire}"
            );
        }
        let value = serde_json::to_value(&view).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("ConnectionView 是对象")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["deviceLabel", "generation", "reasonCode", "state"]);
    }
}
