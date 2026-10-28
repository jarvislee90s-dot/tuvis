//! ZCode 官方 OAuth 授权、凭据保存与恢复（生产计划 T1.1；C2 起复用持久身份）。
//!
//! # 合同（证据 E1：`oauthFlow.ts`／`relayAuth.ts`；合同 `auth` 段；决策 §6.3／§8.10）
//!
//! - 两通道**共用**同一交换端点与 body 形状，唯一差异 = `provider` 字面量与授权 URL 参数：
//!   - BigModel：`{origin}/login?redirect=<回环>&appId=zcode&state=<state>`
//!     （无 `response_type`／`client_id`；origin 固定 `https://bigmodel.cn`）。
//!   - ZAI：`{origin}/api/oauth/authorize?redirect_uri&response_type=code&client_id&state`
//!     （origin 固定 `https://chat.z.ai`；`client_id` 是官方第一方网页公开配置，不是凭据）。
//!     **ZAI 通道只有源码/fake 依据，未做实跑，不得宣称 live-proven。**
//! - 交换：`POST {zcode_origin}/api/v1/oauth/token`，body `{provider,code,redirect_uri,state}`，
//!   只读 `data.token`（账号 JWT）。
//! - 终端 token：`POST {zcode_origin}/api/v1/remote/token`，`Authorization: Bearer <JWT>`，
//!   body `{"role":"terminal"}`，只读 `data.token` 与正整数 `data.expires_in`；每次调用现取。
//!
//! # 安全纪律（不可协商）
//!
//! - 账号 JWT 只在进程内存或系统凭据存储（C1 [`CredentialStore`]）；不写
//!   settings/DB/日志/文件，不 Serialize/Debug，也不经 [`AuthService::authorize`] 上抛，
//!   不进入 [`AuthStatus`]。唯一外出的凭据是短期 relay token
//!   （[`TerminalToken`] 不实现 Debug/Serialize）。
//! - 错误只含冻结码；`reqwest` 原始错误（可能携 URL／认证头）一律丢弃，不挂 cause。
//! - 回环监听只 bind `127.0.0.1:0`；state 精确一次消费；错误/缺失 state 既不消费也不交换；
//!   回调响应为静态中文页（成功/失败两版），不回显任何参数；[`AUTHORIZE_TIMEOUT`] 到点关闭监听
//!   （120s 是 **Tuvis 新策略**，不冒称官方 TTL）。
//! - [`AuthService::clear`] 经授权 epoch + 取消通知让在途授权立即失效：clear 之后的迟到
//!   回调不产生任何交换，交换已完成但 epoch 已变的 JWT 不落内存（review focus 1）。
//! - **凭据代际**：JWT 落内存时同时登记所属 epoch；[`AuthService::terminal_token`] 带着该
//!   epoch 走完全程，并在**持锁的交接点**最终复核——请求在途/响应体读取期间 clear/新授权
//!   → 结果丢弃（返回 `needs_auth`，token 不逃出；`clear` 返回后不可能再交出旧代 token）；
//!   旧 JWT 的迟到 401 只允许清仍是同一代的凭据，绝不触碰新 JWT 或取消新授权。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};

use super::credential_store::{
    CredentialError, CredentialStore, StoreResult, StoredCredential, SCHEMA_VERSION,
};
use super::types::{codes, ZResult, ZcodeError};

/// 授权回调等待上限：到点关闭回环监听并以 `needs_auth` 收尾。
/// 这是 Tuvis 自定的新策略（不冒称官方 TTL）。
pub const AUTHORIZE_TIMEOUT: Duration = Duration::from_secs(120);

/// ZCode 端点 origin（官方默认；交换与 relay 的派生基座）。
pub const OFFICIAL_ZCODE_ORIGIN: &str = "https://zcode.z.ai";
/// ZAI 通道授权入口 origin。
pub const OFFICIAL_ZAI_ORIGIN: &str = "https://chat.z.ai";
/// BigModel（国内智谱）通道授权入口 origin。
pub const OFFICIAL_BIGMODEL_ORIGIN: &str = "https://bigmodel.cn";
/// ZAI 通道授权路径。
pub const OFFICIAL_ZAI_AUTHORIZE_PATH: &str = "/api/oauth/authorize";
/// BigModel 通道授权路径。
pub const OFFICIAL_BIGMODEL_AUTHORIZE_PATH: &str = "/login";
/// 官方第一方网页公开 OAuth client（公开配置，不是凭据）。
pub const OFFICIAL_OAUTH_CLIENT_ID: &str = "client_P8X5CMWmlaRO9gyO-KSqtg";
/// BigModel 通道 `appId`（官方桌面端在用值）。
pub const OFFICIAL_BIGMODEL_APP_ID: &str = "zcode";
/// OAuth 换取账号 JWT 的路径。
pub const OAUTH_TOKEN_PATH: &str = "/api/v1/oauth/token";
/// 申请一次性终端 token 的路径。
pub const RELAY_TOKEN_PATH: &str = "/api/v1/remote/token";

/// 单次 HTTP 请求上限（交换／终端 token）。
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// 单个回调连接读取请求的上限（防止半开连接拖住监听）。
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// 回调请求字节上限。
const MAX_CALLBACK_REQUEST_BYTES: usize = 8 * 1024;
/// JWT 提前失效窗口（秒）：临期凭据不再发起请求。
const JWT_EXPIRY_SKEW_SECS: u64 = 30;

/// 授权成功的静态中文页（不含任何参数；参数值绝不回显）。
const CALLBACK_SUCCESS_PAGE: &str = "<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\">\
<title>兔维斯</title><p>兔维斯已收到授权回调，可以关闭此页面并返回应用。</p>";

/// 授权失败/被拒的静态中文页（不含任何参数；参数值绝不回显）。
const CALLBACK_ERROR_PAGE: &str = "<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\">\
<title>兔维斯</title><p>授权未完成，请关闭此页面并回到兔维斯重试。</p>";

/// 回调静态页的选择（成功/失败）。
#[derive(Clone, Copy)]
enum CallbackPage {
    Success,
    Failed,
}

impl CallbackPage {
    fn body(self) -> &'static str {
        match self {
            Self::Success => CALLBACK_SUCCESS_PAGE,
            Self::Failed => CALLBACK_ERROR_PAGE,
        }
    }
}

/// OAuth 授权通道（对应官方 `WebOAuthProviderId` 两值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OAuthProvider {
    /// 国内智谱（BigModel）通道：`bigmodel.cn/login`。
    Bigmodel,
    /// ZAI 通道：`chat.z.ai/api/oauth/authorize`。
    Zai,
}

impl OAuthProvider {
    /// 交换 body 里的官方 provider 字面量。
    pub fn literal(self) -> &'static str {
        match self {
            Self::Bigmodel => "bigmodel",
            Self::Zai => "zai",
        }
    }

    /// 生产环境的授权入口 origin（固定官方值，无配置入口）。
    fn default_authorize_origin(self) -> &'static str {
        match self {
            Self::Bigmodel => OFFICIAL_BIGMODEL_ORIGIN,
            Self::Zai => OFFICIAL_ZAI_ORIGIN,
        }
    }

    /// 授权入口路径。
    fn authorize_path(self) -> &'static str {
        match self {
            Self::Bigmodel => OFFICIAL_BIGMODEL_AUTHORIZE_PATH,
            Self::Zai => OFFICIAL_ZAI_AUTHORIZE_PATH,
        }
    }
}

/// 授权状态问题（封闭枚举；不含 JWT／原生错误文本）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthProblem {
    /// 凭据已过期（本地 exp 判定）。
    Expired,
    /// 官方 401 撤销（terminal token 申请被拒）。
    Revoked,
    /// 系统保险箱锁定或访问被拒。
    LockedOrDenied,
    /// 系统保险箱不可用。
    Unavailable,
    /// 凭据存储/非敏感标记读写失败。
    StorageFailed,
    /// 持久删除失败。
    DeleteFailed,
    /// 记录损坏或无法识别。
    InvalidRecord,
    /// 记录超出平台容量上限。
    TooLarge,
}

/// 授权状态：唯一新增授权 DTO（serde camelCase；**不含 JWT**）。
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStatus {
    /// 当前进程内存里是否有账号 JWT。
    pub authorized: bool,
    /// 内存授权是否已成功写入系统存储（写失败不假报）。
    pub remembered: bool,
    /// 是否正在从系统存储恢复。
    pub restoring: bool,
    /// 当前凭据所属 OAuth 通道。
    pub provider: Option<OAuthProvider>,
    /// 最近一次失效/存储问题的可见原因。
    pub problem: Option<AuthProblem>,
}

impl AuthProblem {
    /// 凭据存储的封闭错误 → 授权状态 problem（不携带原生错误/bytes）。
    fn from_credential(error: CredentialError) -> Self {
        match error {
            CredentialError::LockedOrDenied => Self::LockedOrDenied,
            CredentialError::Unavailable => Self::Unavailable,
            CredentialError::StorageFailed => Self::StorageFailed,
            CredentialError::InvalidRecord => Self::InvalidRecord,
            CredentialError::TooLarge => Self::TooLarge,
            CredentialError::DeleteFailed => Self::DeleteFailed,
        }
    }
}

/// 打开系统浏览器完成官方授权的回调：入参是完整授权 URL。
///
/// 生产接线只允许「交给系统浏览器打开」；不得注入自动点击/脚本化登录。
pub type OpenAuthorize = Arc<dyn Fn(String) -> ZResult<()> + Send + Sync>;

/// 一次性终端 relay token（`role=terminal`）。
///
/// **不实现 Debug/Serialize**：禁止任何形式的日志/序列化/手机响应外泄；值只能由
/// 消费方（relay 连接）直接取用，用后即弃。
pub struct TerminalToken {
    pub value: String,
    pub expires_in: u64,
}

/// 官方 OAuth 授权服务：持有回环监听参数与**唯一一份**账号 JWT（进程内存；
/// 可选注入 [`CredentialStore`] 后保存/恢复，默认构造不访问 OS）。
///
/// 生产构造只有 [`AuthService::new`]（固定官方 origin，无 settings/IPC 入口）；
/// 端点注入仅测试可见。
pub struct AuthService {
    http: reqwest::Client,
    zcode_origin: String,
    /// 测试专用授权 origin 覆盖（生产恒为 `None`，即固定官方 origin）。
    authorize_origin_override: Option<String>,
    callback_timeout: Duration,
    /// 系统凭据存储适配口：默认不接（构造不访问 OS 存储），生产装配显式注入。
    credential_store: Option<Arc<dyn CredentialStore>>,
    /// 系统存储 I/O 单飞锁：owned guard 随 blocking 任务存活到实际 OS 调用结束，
    /// 调用方超时/取消不会让仍在执行的写入脱离串行约束。`AuthState` 锁不跨越 I/O。
    store_lock: Arc<AsyncMutex<()>>,
    /// 授权状态；`Arc` 让阻塞存储任务能在**仍持存储单飞锁**时落账「删除义务已完成」
    /// （见 [`Self::purge_pending`]），排队的第二个 purge 复核时必然看到最新状态。
    state: Arc<Mutex<AuthState>>,
}

#[derive(Default)]
struct AuthState {
    /// 账号 JWT：进程内存中的当前凭据，`clear`/失效即丢弃。
    jwt: Option<String>,
    /// 该 JWT 落内存时的授权 epoch（凭据代际）。
    ///
    /// `terminal_token` 必须带着它一起使用：epoch 已变（clear/新授权）时，这份凭据
    /// 及其 HTTP 结果一律作废，既不得返回 relay token，也不得清掉新凭据。
    jwt_epoch: Option<u64>,
    /// 当前 JWT 所属 OAuth 通道（`AuthStatus.provider` 的来源）。
    provider: Option<OAuthProvider>,
    /// 授权 epoch：clear/新授权递增，旧 epoch 的结果一律作废。
    epoch: u64,
    /// 在途授权的取消发送端（clear/新授权触发）。
    cancel: Option<oneshot::Sender<()>>,
    /// 当前内存 JWT 是否已成功写入系统存储；写失败绝不假报。
    /// `status` 只读本字段，不访问 OS。
    remembered: bool,
    /// 是否正在从系统存储恢复（只用于状态展示）。
    restoring: bool,
    /// 最近一次失效/存储问题的可见原因（封闭枚举，不含 JWT/原生错误）。
    problem: Option<AuthProblem>,
    /// forget 后的内存级恢复禁用；新授权保存成功才重新允许恢复。
    restore_blocked: bool,
    /// 尚未完成的**删除义务**（forget 登记）：epoch 推进不取消它。
    ///
    /// 旧实现让删除跟随 epoch 判定是否执行，新授权一旦推进 epoch，忘记排队的删除就被
    /// 跳过，旧记录可能留在存储里、在重启后被恢复。只有两个解除点，且都在持
    /// `store_lock` 的窗口内落账：删除成功，或新授权保存成功（新记录覆盖旧记录）。
    purge_pending: bool,
}

/// 回环回调的终局。
enum CallbackOutcome {
    /// 官方回调带回了授权码。
    Code(String),
    /// 官方回调带回了 `error`（授权被拒/取消）。
    Rejected,
}

impl AuthService {
    /// 生产工厂：固定官方 origin，不读取任何配置、不暴露 IPC/settings 入口。
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            zcode_origin: OFFICIAL_ZCODE_ORIGIN.to_owned(),
            authorize_origin_override: None,
            callback_timeout: AUTHORIZE_TIMEOUT,
            credential_store: None,
            store_lock: Arc::new(AsyncMutex::new(())),
            state: Arc::new(Mutex::new(AuthState::default())),
        }
    }

    /// 注入系统凭据存储（builder；默认构造不接，测试注入 fake）。
    ///
    /// `CredentialStore` 是 crate 内部敏感类型，故本 builder 只在 crate 内可用；
    /// 生产装配（[`super::hub::PartsBackend::production`]）显式注入 `OsCredentialStore`。
    pub(crate) fn with_credential_store(mut self, store: Arc<dyn CredentialStore>) -> Self {
        self.credential_store = Some(store);
        self
    }

    /// 测试专用端点注入（生产不得调用；不注册任何配置/IPC 入口）。
    #[cfg(test)]
    pub fn with_endpoints(
        http: reqwest::Client,
        authorize_origin: String,
        zcode_origin: String,
    ) -> Self {
        Self {
            http,
            zcode_origin,
            authorize_origin_override: Some(authorize_origin),
            callback_timeout: AUTHORIZE_TIMEOUT,
            credential_store: None,
            store_lock: Arc::new(AsyncMutex::new(())),
            state: Arc::new(Mutex::new(AuthState::default())),
        }
    }

    /// 测试专用回调等待上限（120s 生产策略不缩短；测试不留悬挂任务）。
    #[cfg(test)]
    fn with_callback_timeout(mut self, timeout: Duration) -> Self {
        self.callback_timeout = timeout;
        self
    }

    /// 走一次官方授权：打开浏览器 → 等待回环回调 → 用授权码换账号 JWT（先入内存，
    /// 注入存储后随即可持久保存；保存失败不影响本次授权）。
    ///
    /// 失败一律以冻结码收尾（超时/被取消/被拒 → `needs_auth`，网络不可达 → `offline`，
    /// 响应形态异常 → `unknown`）；错误不含 URL/state/code/JWT。
    pub async fn authorize(&self, provider: OAuthProvider, open: OpenAuthorize) -> ZResult<()> {
        // 0) 忘记遗留的删除义务必须先完成：旧记录没删掉就开新授权，一旦新授权失败/
        //    被取消，重启可能把旧记录恢复成新身份。删除失败时零浏览器、零 OAuth。
        self.flush_pending_purge().await?;

        // 1) 回环监听：只 bind 127.0.0.1 的随机空闲端口（不接受外部地址）
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|_| ZcodeError::unknown())?;
        let port = listener
            .local_addr()
            .map_err(|_| ZcodeError::unknown())?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");
        let state = generate_oauth_state();
        let url = build_authorize_url(
            provider,
            self.authorize_origin(provider),
            &redirect_uri,
            &state,
        )?;

        // 2) 登记本次授权：epoch 递增让旧结果作废，并取消上一笔在途授权
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        let epoch = self.begin_authorization(cancel_tx);

        // 3) 交给系统浏览器（生产只允许真实浏览器打开，不做脚本化登录）
        if let Err(err) = open(url.to_string()) {
            self.end_authorization(epoch);
            return Err(err);
        }

        // 4) 有界等待回调；超时/取消都会关闭监听
        let code = match self
            .wait_for_callback(listener, state.clone(), &mut cancel_rx)
            .await
        {
            Ok(code) => code,
            Err(err) => {
                self.end_authorization(epoch);
                return Err(err);
            }
        };

        // 5) 交换前再核 epoch：clear() 之后的在途授权不得再发交换请求
        if !self.epoch_is_current(epoch) {
            self.end_authorization(epoch);
            return Err(ZcodeError::needs_auth());
        }
        let exchanged = self.exchange(provider, &code, &redirect_uri, &state).await;
        self.end_authorization(epoch);
        let jwt = exchanged?;

        // 6) 只有 epoch 未变才落内存（交换在途时 clear/新授权 → 结果作废）
        self.commit_jwt(epoch, jwt, provider)?;
        // 7) 先落内存再保存：保存失败只回写 problem，不把本次 OAuth 变成失败。
        self.persist_authorized(epoch, provider).await;
        Ok(())
    }

    /// 以内存中的账号 JWT 现取一次性终端 token（逐连接新取，不缓存、用后即弃）。
    ///
    /// 入口把 JWT 与它所属的授权 epoch **一起捕获**，此后只认这一代：
    /// - 已过期 JWT 在任何 HTTP 请求之前返回 `needs_auth`；
    /// - 请求在途/响应体读取期间 clear/新授权发生（epoch 已变）→ 结果作废，返回
    ///   `needs_auth`；最终交接在状态锁内复核代际（[`Self::handoff_terminal_token`]），
    ///   因此 `clear()` 返回后绝不可能再交出旧代的 relay token；
    /// - 401 只清仍属本代的凭据并持久删除；旧 JWT 的迟到 401 不得清掉新 JWT、
    ///   取消新授权或删除新保存的记录；offline/5xx/403 不删除持久记录。
    pub async fn terminal_token(&self) -> ZResult<TerminalToken> {
        let (jwt, credential_epoch) = {
            let state = self.lock_state();
            match (state.jwt.clone(), state.jwt_epoch) {
                (Some(jwt), Some(epoch)) => (jwt, epoch),
                _ => return Err(ZcodeError::needs_auth()),
            }
        };
        if jwt_is_expired(&jwt, now_secs()) {
            self.invalidate(credential_epoch, AuthProblem::Expired)
                .await;
            return Err(ZcodeError::needs_auth());
        }

        let response = self
            .http
            .post(endpoint_url(&self.zcode_origin, RELAY_TOKEN_PATH))
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {jwt}"))
            .json(&serde_json::json!({ "role": "terminal" }))
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            // reqwest 错误可能含 URL/认证头：整体丢弃，只映射常量码
            .map_err(|_| ZcodeError::offline())?;
        // 请求在途时 clear/新授权：本结果属于旧代际，丢弃且不触碰当前凭据/授权
        if !self.epoch_is_current(credential_epoch) {
            return Err(ZcodeError::needs_auth());
        }
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            self.invalidate(credential_epoch, AuthProblem::Revoked)
                .await;
            return Err(ZcodeError::needs_auth());
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            return Err(ZcodeError::needs_auth());
        }
        if !status.is_success() {
            return Err(ZcodeError::offline());
        }
        let value: serde_json::Value = response.json().await.map_err(|_| ZcodeError::unknown())?;
        // 交接点：body 解析完成后再持锁复核代际，clear/新授权一旦返回，旧结果必定作废
        self.handoff_terminal_token(credential_epoch, &value)
    }

    /// 一次性终端 token 的**唯一交接点**：持状态锁复核凭据代际，命中才构造并返回。
    ///
    /// `clear()`／新授权与这里争用同一把锁，因此交接是线性化的：
    /// - `clear()` 先拿到锁 → 本次复核看到新 epoch，返回 `needs_auth`（token 不构造、不逃出）；
    /// - 交接先拿到锁 → `clear()` 只能在 token 构造之后返回，逻辑上晚于本次发放。
    ///
    /// 响应体读取（`json().await`）期间的 clear/新授权由这里兜住；构造与返回都在锁内，
    /// 不在 guard 内做任何 await。
    fn handoff_terminal_token(
        &self,
        credential_epoch: u64,
        value: &serde_json::Value,
    ) -> ZResult<TerminalToken> {
        let state = self.lock_state();
        if state.epoch != credential_epoch || state.jwt_epoch != Some(credential_epoch) {
            return Err(ZcodeError::needs_auth());
        }
        relay_payload(value)
    }

    /// 清除内存中的账号凭据并使在途授权失效。
    ///
    /// **只清运行时内存**：持久记录与恢复标记不动（`forget` 才删除）；`remembered`
    /// 保留（持久记录仍在），仅清掉本次会话的临时 `problem`。401/过期走
    /// [`Self::invalidate`] 的持久删除入口。
    pub fn clear(&self) {
        let mut state = self.lock_state();
        state.jwt = None;
        state.jwt_epoch = None;
        state.provider = None;
        state.restoring = false;
        state.problem = None;
        state.epoch = state.epoch.wrapping_add(1);
        if let Some(cancel) = state.cancel.take() {
            let _ = cancel.send(());
        }
    }

    /// 授权状态快照：只读内存，不访问系统存储。
    pub fn status(&self) -> AuthStatus {
        let state = self.lock_state();
        status_snapshot(&state)
    }

    /// 启动/显式重试时恢复已保存的授权。
    ///
    /// - 已有当前代有效内存 JWT：直接返回，不读系统存储（保存失败不覆盖本次身份）；
    /// - 内存 JWT 已过期：按失效清理（删除持久记录并报 `expired`），不再读存储；
    /// - 无存储适配口、恢复已被忘记禁用、或有在途授权：只返回内存快照；
    /// - 读取问题（锁定/不可用/损坏/超限）如实报 `problem`，绝不自动打开浏览器。
    pub async fn restore(&self) -> AuthStatus {
        // 0) 忘记遗留的删除义务：旧记录未真正删除之前不得读取，否则本次恢复会把它
        //    当成当前身份（删除失败如实报 problem，零 load）。
        let (purge_pending, purge_epoch) = {
            let state = self.lock_state();
            (state.purge_pending, state.epoch)
        };
        if purge_pending && self.purge_pending(purge_epoch).await.is_err() {
            return self.status();
        }

        // 1) 已有当前代有效内存 JWT：直接返回，不读 OS。
        //
        // 返回前必须收尾 `restoring`：并发恢复可能在早退前把标记置位（上一轮恢复
        // 尚未返回时，新一轮授权已把有效 JWT 落内存），否则状态轮询会永久显示「恢复中」。
        let expired_memory_epoch = {
            let mut state = self.lock_state();
            let memory_jwt_current = match (state.jwt.as_deref(), state.jwt_epoch) {
                (Some(jwt), Some(_)) => !jwt_is_expired(jwt, now_secs()),
                _ => false,
            };
            if memory_jwt_current {
                state.restoring = false;
                return status_snapshot(&state);
            }
            match (state.jwt.as_deref(), state.jwt_epoch) {
                (Some(_), Some(epoch)) => Some(epoch),
                _ => None,
            }
        };
        if let Some(epoch) = expired_memory_epoch {
            self.invalidate(epoch, AuthProblem::Expired).await;
            return self.status();
        }

        let Some(store) = self.credential_store.clone() else {
            return self.status();
        };

        // 2) 标记恢复中；忘记后的记录不再读取，在途授权旁不插队恢复。
        let epoch = {
            let mut state = self.lock_state();
            if state.restore_blocked || state.cancel.is_some() {
                return status_snapshot(&state);
            }
            state.restoring = true;
            state.epoch
        };

        // 3) 存储单飞锁内读 OS：调用前复核 epoch，guard 随 blocking 任务存活。
        let loaded = match self
            .store_io_if_current(epoch, store, |store| store.load())
            .await
        {
            Some(result) => result,
            // 开始前已有新授权/忘记接管：不得覆盖，也不回写旧状态。
            None => return self.status(),
        };

        // 4) 完成复核：epoch 已变或新 JWT 已落地（并发授权完成）→ 丢弃本次恢复。
        //
        // 决策与状态回写在同一把锁内完成，**锁在 await 之前释放**：本 future 必须
        // `Send`（Hub 的 `BoxFuture` 端口会跨线程持有）。过期记录的持久删除放到锁外，
        // 删除本身仍由 `delete_persisted_if_current` 复核 epoch。
        let expired_delete_epoch = {
            let mut state = self.lock_state();
            if state.epoch != epoch || state.jwt.is_some() {
                // 同代已有 JWT（并发授权先落内存）→ 本次恢复让位，但必须收尾自己置位的
                // `restoring`；epoch 已变则代表别的路径已接管状态，不碰任何字段。
                if state.epoch == epoch && state.jwt.is_some() {
                    state.restoring = false;
                }
                return status_snapshot(&state);
            }
            state.restoring = false;
            match loaded {
                Ok(None) => {
                    state.remembered = false;
                    state.problem = None;
                    None
                }
                Ok(Some(record)) if jwt_is_expired(&record.jwt, now_secs()) => {
                    // 过期持久记录：作废当前代并排队删除；不自动打开浏览器。
                    state.jwt = None;
                    state.jwt_epoch = None;
                    state.provider = None;
                    state.remembered = false;
                    state.problem = Some(AuthProblem::Expired);
                    state.epoch = state.epoch.wrapping_add(1);
                    Some(state.epoch)
                }
                Ok(Some(record)) => {
                    state.jwt = Some(record.jwt);
                    state.jwt_epoch = Some(epoch);
                    state.provider = Some(record.provider);
                    state.remembered = true;
                    state.problem = None;
                    None
                }
                Err(error) => {
                    state.remembered = false;
                    state.problem = Some(AuthProblem::from_credential(error));
                    None
                }
            }
        };
        if let Some(delete_epoch) = expired_delete_epoch {
            let _ = self.delete_persisted_if_current(delete_epoch).await;
        }
        self.status()
    }

    /// 忘记授权：先清内存/取消旧 epoch 并禁用恢复，再排队删除持久记录。
    ///
    /// 删除失败如实报 `delete_failed` 且保持恢复禁用（不得读回旧记录）。
    pub async fn forget(&self) -> AuthStatus {
        let delete_epoch = {
            let mut state = self.lock_state();
            state.jwt = None;
            state.jwt_epoch = None;
            state.provider = None;
            state.remembered = false;
            state.restoring = false;
            state.problem = None;
            state.restore_blocked = true;
            // 删除义务：epoch 推进（新授权/清除）不取消它，只有真正删除成功才解除。
            state.purge_pending = true;
            state.epoch = state.epoch.wrapping_add(1);
            if let Some(cancel) = state.cancel.take() {
                let _ = cancel.send(());
            }
            state.epoch
        };
        // 删除义务不看 epoch（与 `store_io_if_current` 的关键差别）：新授权代际不得让
        // 忘记的删除被跳过。失败保留 `purge_pending` 并在状态里如实报 problem。
        let _ = self.purge_pending(delete_epoch).await;
        self.status()
    }

    /// 执行忘记遗留的**删除义务**。
    ///
    /// 与 [`Self::store_io_if_current`] 的关键差别：删除义务持续存在，**不再用 epoch
    /// 决定是否跳过**——新授权推进 epoch 也不取消这次删除（否则旧记录可能在重启后复活）。
    /// 只要 `purge_pending` 仍为真就执行删除（不得用「内存里已有新代 JWT」当捷径：
    /// 新 JWT 保存失败时旧记录会留下来，重启即可被恢复）。
    ///
    /// 唯一的跳过条件是义务已被解除：并发 purge 的删除成功、或新授权保存成功——两者
    /// 都在**持 `store_lock` 的窗口内**清 `purge_pending`，因此这里看到 pending=false
    /// 就说明无需再删（也保证不会把新授权刚保存的记录当成旧记录删掉）。
    async fn purge_pending(&self, expected_epoch: u64) -> StoreResult<()> {
        let Some(store) = self.credential_store.clone() else {
            // 没有存储适配口 = 没有持久记录可删，义务视为已完成。
            self.lock_state().purge_pending = false;
            return Ok(());
        };
        let guard = self.store_lock.clone().lock_owned().await;
        if !self.lock_state().purge_pending {
            return Ok(());
        }
        // 「删除成功」与「义务解除」在同一个串行窗口内落账：guard 释放前先清
        // `purge_pending`，排队的第二个 purge 复核时必然看到已完成，不会重复删除。
        let state = self.state.clone();
        let joined = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let result = store.delete();
            if result.is_ok() {
                lock_state(&state).purge_pending = false;
            }
            result
        })
        .await;
        let result = joined.unwrap_or(Err(CredentialError::Unavailable));
        let mut state = self.lock_state();
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                // 只有绑定同一义务代际的失败才保留义务 / 回写问题：新授权或新忘记已接管
                // 时（epoch 已变且义务已清）不得把已解除的义务复活成僵尸。
                if state.epoch == expected_epoch {
                    state.purge_pending = true;
                    state.problem = Some(AuthProblem::from_credential(error));
                }
                Err(error)
            }
        }
    }

    /// 新授权开始前的删除义务闸：未完成则 `storage_failed`（零浏览器/零 OAuth）。
    async fn flush_pending_purge(&self) -> ZResult<()> {
        let (pending, epoch) = {
            let state = self.lock_state();
            (state.purge_pending, state.epoch)
        };
        if !pending {
            return Ok(());
        }
        self.purge_pending(epoch)
            .await
            .map_err(|_| ZcodeError::of(codes::STORAGE_FAILED))
    }

    /// 生产授权 origin：测试覆盖优先，否则固定官方常量（无配置/IPC 入口）。
    fn authorize_origin(&self, provider: OAuthProvider) -> &str {
        self.authorize_origin_override
            .as_deref()
            .unwrap_or_else(|| provider.default_authorize_origin())
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, AuthState> {
        lock_state(&self.state)
    }

    /// 开始一次授权：epoch 递增（旧 epoch 的一切结果作废），并取消上一笔在途授权。
    fn begin_authorization(&self, cancel: oneshot::Sender<()>) -> u64 {
        let mut state = self.lock_state();
        state.epoch = state.epoch.wrapping_add(1);
        state.restoring = false;
        state.problem = None;
        if let Some(previous) = state.cancel.replace(cancel) {
            let _ = previous.send(());
        }
        state.epoch
    }

    /// 收尾：只清理仍属于本 epoch 的取消通道（clear/新授权已接管时不动）。
    fn end_authorization(&self, epoch: u64) {
        let mut state = self.lock_state();
        if state.epoch == epoch {
            state.cancel = None;
        }
    }

    fn epoch_is_current(&self, epoch: u64) -> bool {
        self.lock_state().epoch == epoch
    }

    /// 落入内存的唯一入口：epoch 不符（clear/新授权已发生）即作废。
    ///
    /// 成功时把 JWT、所属 epoch 与 provider 一起登记（凭据代际），供
    /// [`Self::terminal_token`] 判定「这份凭据是否仍属于当前授权」。
    fn commit_jwt(&self, epoch: u64, jwt: String, provider: OAuthProvider) -> ZResult<()> {
        let mut state = self.lock_state();
        if state.epoch != epoch {
            return Err(ZcodeError::needs_auth());
        }
        state.jwt = Some(jwt);
        state.jwt_epoch = Some(epoch);
        state.provider = Some(provider);
        state.remembered = false;
        state.restoring = false;
        state.problem = None;
        Ok(())
    }

    /// 授权成功后持久保存当前代 JWT；失败只回写 `problem`，不改变本次身份。
    async fn persist_authorized(&self, epoch: u64, provider: OAuthProvider) {
        let Some(store) = self.credential_store.clone() else {
            return;
        };
        let jwt = {
            let state = self.lock_state();
            if state.epoch != epoch || state.jwt_epoch != Some(epoch) {
                return;
            }
            state.jwt.clone()
        };
        let Some(jwt) = jwt else {
            return;
        };
        let record = StoredCredential {
            schema_version: SCHEMA_VERSION,
            provider,
            jwt,
        };
        match self.save_authorized_record(epoch, store, record).await {
            // 保存成功的状态提交已在存储锁窗口内完成（见 `save_authorized_record`）。
            Some(Ok(())) => {}
            Some(Err(error)) => {
                let mut state = self.lock_state();
                if state.epoch == epoch && state.jwt_epoch == Some(epoch) {
                    state.remembered = false;
                    state.problem = Some(AuthProblem::from_credential(error));
                }
            }
            // 开始前已有新授权/忘记接管：旧保存不得回写「已记住」。
            None => {}
        }
    }

    /// 保存新授权记录：**状态提交与保存同在存储单飞锁窗口内**完成。
    ///
    /// 保存成功的提交（remembered/problem/restore_blocked/purge_pending）由阻塞任务在
    /// 仍持 `store_lock` 时落账，guard 释放后调用方不再补写——否则排队的
    /// [`Self::purge_pending`] 会在「记录已写入、状态未提交」的窗口看到
    /// `purge_pending=true`，把刚保存的新记录当成旧记录删掉。
    ///
    /// `None` 与 [`Self::store_io_if_current`] 同语义：开始前代际已变，未触碰 OS。
    async fn save_authorized_record(
        &self,
        epoch: u64,
        store: Arc<dyn CredentialStore>,
        record: StoredCredential,
    ) -> Option<StoreResult<()>> {
        let guard = self.store_lock.clone().lock_owned().await;
        if !self.epoch_is_current(epoch) {
            return None;
        }
        let state = self.state.clone();
        let joined = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let result = store.save(&record);
            if result.is_ok() {
                let mut state = lock_state(&state);
                // 保存期间 clear/忘记已接管：记录虽写入，但不得回写「已记住」；该记录
                // 由忘记的删除义务在锁后清理。
                if state.epoch == epoch && state.jwt_epoch == Some(epoch) {
                    state.remembered = true;
                    state.problem = None;
                    // 只有新授权保存成功才重新允许恢复（forget 的禁用到此解除）。
                    state.restore_blocked = false;
                    // 新 JWT 已落库：忘记遗留的删除义务到此被新记录取代。
                    state.purge_pending = false;
                }
            }
            result
        })
        .await;
        Some(joined.unwrap_or(Err(CredentialError::Unavailable)))
    }

    /// 串行删除持久记录（只删仍属 `expected_epoch` 的那一代）。
    async fn delete_persisted_if_current(&self, expected_epoch: u64) -> Option<StoreResult<()>> {
        let store = self.credential_store.clone()?;
        self.store_io_if_current(expected_epoch, store, |store| store.delete())
            .await
    }

    /// 在存储单飞锁内复核 epoch 后执行一次阻塞存储调用。
    ///
    /// `None` = 开始前 epoch 已变（放弃，不触碰 OS）；`Some(result)` = 实际调用结果。
    /// `OwnedMutexGuard` 被移进 blocking 任务，直到实际 OS 调用结束才释放：调用方超时/
    /// 取消不会让仍在执行的写入脱离串行约束。全程不持 `AuthState` 或 DB 锁。
    async fn store_io_if_current<T, F>(
        &self,
        expected_epoch: u64,
        store: Arc<dyn CredentialStore>,
        op: F,
    ) -> Option<StoreResult<T>>
    where
        T: Send + 'static,
        F: FnOnce(&dyn CredentialStore) -> StoreResult<T> + Send + 'static,
    {
        let guard = self.store_lock.clone().lock_owned().await;
        if !self.epoch_is_current(expected_epoch) {
            return None;
        }
        let joined = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            op(store.as_ref())
        })
        .await;
        Some(joined.unwrap_or(Err(CredentialError::Unavailable)))
    }

    /// 凭据失效（过期/401）：只清仍属**同一代际**的凭据与在途授权，并持久删除。
    ///
    /// 旧 JWT 的迟到结果（epoch 已变）不得清掉新 JWT、不得取消新授权，也不得删除
    /// 新授权保存的记录——删除在存储单飞锁内复核 epoch 后才执行。401/过期是授权事实，
    /// 删除失败不改写 `problem`，但必须禁用恢复，防止已撤销 JWT 被 restore 重新授权。
    async fn invalidate(&self, credential_epoch: u64, problem: AuthProblem) {
        let delete_epoch = {
            let mut state = self.lock_state();
            if state.epoch != credential_epoch || state.jwt_epoch != Some(credential_epoch) {
                return;
            }
            state.jwt = None;
            state.jwt_epoch = None;
            state.provider = None;
            state.remembered = false;
            state.restoring = false;
            state.problem = Some(problem);
            state.epoch = state.epoch.wrapping_add(1);
            if let Some(cancel) = state.cancel.take() {
                let _ = cancel.send(());
            }
            state.epoch
        };
        match self.delete_persisted_if_current(delete_epoch).await {
            Some(Ok(())) => {}
            Some(Err(_)) => {
                let mut state = self.lock_state();
                // 删除失败（尤其恢复禁用标记写失败）：记录可能仍在，禁止 restore 把
                // 已撤销的 JWT 重新授权；problem 保持 revoked/expired，不假报删除成功。
                if state.epoch == delete_epoch {
                    state.restore_blocked = true;
                }
            }
            // 新授权/忘记已接管：旧删除结果不得回写状态。
            None => {}
        }
    }

    /// 有界等待回调：得到授权码 / 被取消 / 超时 / 被拒，任何出口都关闭监听。
    async fn wait_for_callback(
        &self,
        listener: TcpListener,
        expected_state: String,
        cancel: &mut oneshot::Receiver<()>,
    ) -> ZResult<String> {
        let (outcomes, mut outcome_rx) = mpsc::channel::<CallbackOutcome>(1);
        let listener_task = tokio::spawn(serve_callbacks(listener, expected_state, outcomes));
        let result = tokio::select! {
            // clear()/新授权触发取消（或取消通道被丢弃 = 已被取代）
            _ = &mut *cancel => Err(ZcodeError::needs_auth()),
            received = tokio::time::timeout(self.callback_timeout, outcome_rx.recv()) => {
                match received {
                    Err(_) => Err(ZcodeError::needs_auth()),
                    Ok(None) => Err(ZcodeError::unknown()),
                    Ok(Some(CallbackOutcome::Code(code))) => Ok(code),
                    Ok(Some(CallbackOutcome::Rejected)) => Err(ZcodeError::needs_auth()),
                }
            }
        };
        listener_task.abort();
        // 等任务真正退出：保证监听 socket 已释放（端口立即不可再连）
        let _ = listener_task.await;
        result
    }

    /// `POST {zcode_origin}/api/v1/oauth/token`，只读 `data.token`（账号 JWT）。
    async fn exchange(
        &self,
        provider: OAuthProvider,
        code: &str,
        redirect_uri: &str,
        oauth_state: &str,
    ) -> ZResult<String> {
        let body = serde_json::json!({
            "provider": provider.literal(),
            "code": code,
            "redirect_uri": redirect_uri,
            "state": oauth_state,
        });
        let response = self
            .http
            .post(endpoint_url(&self.zcode_origin, OAUTH_TOKEN_PATH))
            .json(&body)
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .map_err(|_| ZcodeError::offline())?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ZcodeError::needs_auth());
        }
        if !status.is_success() {
            return Err(ZcodeError::unknown());
        }
        let value: serde_json::Value = response.json().await.map_err(|_| ZcodeError::unknown())?;
        exchange_payload(&value)
    }
}

/// 取授权状态锁（中毒锁按内部值继续：状态只是内存快照，不因 panic 丢语义）。
fn lock_state(state: &Mutex<AuthState>) -> std::sync::MutexGuard<'_, AuthState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 由内存状态构造授权状态快照（只读：不访问系统存储，也不写回状态）。
///
/// `authorized` 含本地 exp 判定：过期 JWT 在 1.5 秒状态轮询里不得继续显示为已授权；
/// 若此前没有更具体的 problem，快照报 `Expired`。失效清理由 `terminal_token`/
/// `restore` 的持久删除入口负责，本函数不改内存状态。
fn status_snapshot(state: &AuthState) -> AuthStatus {
    let authorized = state
        .jwt
        .as_deref()
        .map(|jwt| !jwt_is_expired(jwt, now_secs()))
        .unwrap_or(false);
    let problem = match state.problem {
        None if state.jwt.is_some() && !authorized => Some(AuthProblem::Expired),
        problem => problem,
    };
    AuthStatus {
        authorized,
        remembered: state.remembered,
        restoring: state.restoring,
        provider: state.provider,
        problem,
    }
}

/// 按通道构造官方授权 URL（BigModel 与 ZAI 的**参数名不同**）：
/// - BigModel：`{origin}/login?redirect&appId=zcode&state`（无 response_type/client_id）
/// - ZAI：`{origin}/api/oauth/authorize?redirect_uri&response_type=code&client_id&state`
///
/// **仅模块内可见**（测试经此校验 URL 形状）：生产路径只允许传固定官方 origin，
/// 不暴露给 settings/IPC/手机，避免任意 origin 注入。
fn build_authorize_url(
    provider: OAuthProvider,
    origin: &str,
    redirect_uri: &str,
    state: &str,
) -> ZResult<reqwest::Url> {
    let base = format!(
        "{}{}",
        origin.trim_end_matches('/'),
        provider.authorize_path()
    );
    let mut url = reqwest::Url::parse(&base).map_err(|_| ZcodeError::unknown())?;
    {
        let mut query = url.query_pairs_mut();
        match provider {
            OAuthProvider::Bigmodel => {
                query.append_pair("redirect", redirect_uri);
                query.append_pair("appId", OFFICIAL_BIGMODEL_APP_ID);
            }
            OAuthProvider::Zai => {
                query.append_pair("redirect_uri", redirect_uri);
                query.append_pair("response_type", "code");
                query.append_pair("client_id", OFFICIAL_OAUTH_CLIENT_ID);
            }
        }
        query.append_pair("state", state);
    }
    Ok(url)
}

/// 解析交换响应：只认 `data.token`（非空字符串）；`code` 出现时必须为 0。
fn exchange_payload(value: &serde_json::Value) -> ZResult<String> {
    if let Some(server_code) = value.get("code") {
        if server_code.as_i64() != Some(0) {
            return Err(ZcodeError::unknown());
        }
    }
    value
        .pointer("/data/token")
        .and_then(|token| token.as_str())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .ok_or_else(ZcodeError::unknown)
}

/// 解析终端 token 响应：`data.token` 非空 + `data.expires_in` 为正整数，否则拒绝。
fn relay_payload(value: &serde_json::Value) -> ZResult<TerminalToken> {
    if let Some(server_code) = value.get("code") {
        if server_code.as_i64() != Some(0) {
            return Err(ZcodeError::unknown());
        }
    }
    let data = value.get("data").ok_or_else(ZcodeError::unknown)?;
    let token = data
        .get("token")
        .and_then(|token| token.as_str())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(ZcodeError::unknown)?;
    let expires_in = data
        .get("expires_in")
        .and_then(|expires_in| expires_in.as_u64())
        .filter(|expires_in| *expires_in > 0)
        .ok_or_else(ZcodeError::unknown)?;
    Ok(TerminalToken {
        value: token.to_owned(),
        expires_in,
    })
}

/// 生成单次 OAuth state（16 字节随机 → 22 字符 base64url）。
fn generate_oauth_state() -> String {
    let mut bytes = [0u8; 16];
    rand::fill(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn endpoint_url(origin: &str, path: &str) -> String {
    format!("{}{}", origin.trim_end_matches('/'), path)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// JWT 是否已过期（读 `exp`，含提前失效窗口）。
///
/// 非 JWT/无 `exp` 的凭据不在此判定，交由服务端 401 收口（不猜、不伪造）。
fn jwt_is_expired(jwt: &str, now_secs: u64) -> bool {
    let Some(payload) = jwt.split('.').nth(1) else {
        return false;
    };
    let Ok(decoded) =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('='))
    else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&decoded) else {
        return false;
    };
    match value.get("exp").and_then(|exp| exp.as_u64()) {
        Some(exp) => exp <= now_secs.saturating_add(JWT_EXPIRY_SKEW_SECS),
        None => false,
    }
}

/// 回调监听主循环：只绑回环、单用途；每连接独立处理，慢连接不阻塞其他回调。
async fn serve_callbacks(
    listener: TcpListener,
    expected_state: String,
    outcomes: mpsc::Sender<CallbackOutcome>,
) {
    let consumed = Arc::new(AtomicBool::new(false));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let state = expected_state.clone();
        let outcomes = outcomes.clone();
        let consumed = Arc::clone(&consumed);
        tokio::spawn(async move {
            handle_callback(stream, &state, &consumed, outcomes).await;
        });
    }
}

/// 处理单个回调请求：state 精确匹配才消费；错误/缺失 state 与重复回调都不交换。
async fn handle_callback(
    mut stream: TcpStream,
    expected_state: &str,
    consumed: &AtomicBool,
    outcomes: mpsc::Sender<CallbackOutcome>,
) {
    let Some(target) = read_request_target(&mut stream).await else {
        respond(&mut stream, 400, CallbackPage::Failed).await;
        return;
    };
    let Ok(parsed) = reqwest::Url::parse(&format!("http://127.0.0.1{target}")) else {
        respond(&mut stream, 400, CallbackPage::Failed).await;
        return;
    };
    let params: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
    let value = |name: &str| -> Option<String> {
        params
            .get(name)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };

    // state 不匹配/缺失：忽略且**不消费**（继续等待真正的回调）
    if value("state").as_deref() != Some(expected_state) {
        respond(&mut stream, 400, CallbackPage::Failed).await;
        return;
    }
    let code = value("code").or_else(|| value("authCode"));
    let error = value("error");
    if code.is_none() && error.is_none() {
        // state 匹配但既无 code 也无 error：不消费，继续等待
        respond(&mut stream, 400, CallbackPage::Failed).await;
        return;
    }
    // state 精确一次消费：重复回调一律作废，不产生第二次交换
    if consumed.swap(true, Ordering::SeqCst) {
        respond(&mut stream, 410, CallbackPage::Failed).await;
        return;
    }
    let (outcome, page) = match code {
        Some(code) => (CallbackOutcome::Code(code), CallbackPage::Success),
        None => (CallbackOutcome::Rejected, CallbackPage::Failed),
    };
    let _ = outcomes.send(outcome).await;
    respond(&mut stream, 200, page).await;
}

/// 读回调请求行（只接受 GET + 绝对路径），带读取上限与超时。
async fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = tokio::time::timeout(CALLBACK_READ_TIMEOUT, stream.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..read]);
        if buf.len() > MAX_CALLBACK_REQUEST_BYTES {
            return None;
        }
        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let request_line = head.lines().next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;
    if !method.eq_ignore_ascii_case("GET") || !target.starts_with('/') {
        return None;
    }
    Some(target.to_string())
}

/// 静态回调响应：绝不回显任何 query 参数值（只区分状态码与成功/失败页）。
async fn respond(stream: &mut TcpStream, status: u16, page: CallbackPage) {
    let reason = match status {
        200 => "OK",
        410 => "Gone",
        _ => "Bad Request",
    };
    let body = page.body();
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let write = async {
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream.write_all(body.as_bytes()).await;
        let _ = stream.flush().await;
    };
    let _ = tokio::time::timeout(CALLBACK_READ_TIMEOUT, write).await;
}

#[cfg(test)]
mod tests {
    use super::super::types::codes;
    use super::*;
    use tokio::sync::Semaphore;

    // ───────────────────────── 合成夹具（无真实账号/网络/DB） ─────────────────────────

    /// 合成授权码（只为断言「不出现在错误/响应里」，不是任何真实值）。
    const SYNTHETIC_CODE: &str = "synthetic-code-0001";

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn base64url(bytes: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    /// 合成 JWT：三段形状，payload 只含合成 sub 与 exp。
    fn synthetic_jwt(exp: u64) -> String {
        format!(
            "{}.{}.synthetic-signature",
            base64url(br#"{"alg":"none","typ":"JWT"}"#),
            base64url(format!(r#"{{"sub":"synthetic-account","exp":{exp}}}"#).as_bytes())
        )
    }

    fn valid_jwt() -> String {
        synthetic_jwt(now_secs() + 3600)
    }

    fn expired_jwt() -> String {
        synthetic_jwt(now_secs().saturating_sub(3600))
    }

    fn exchange_ok(token: &str) -> String {
        format!(r#"{{"code":0,"msg":"ok","data":{{"token":"{token}"}}}}"#)
    }

    fn relay_ok(token: &str, expires_in: u64) -> String {
        format!(r#"{{"code":0,"msg":"ok","data":{{"token":"{token}","expires_in":{expires_in}}}}}"#)
    }

    const SYNTHETIC_RELAY_TOKEN: &str = "syntheticrelaytoken01";

    // ───────────────────────── 本机 fake HTTP（交换 + 终端 token） ─────────────────────────

    /// 终端 token 响应的两阶段闸门：先写响应头并通知 `headers_sent`，
    /// 再等 `body_gate` 放行后才写 body（把 `clear()` 精确塞进「头已到、体未到」窗口）。
    #[derive(Clone)]
    struct TwoPhaseReply {
        /// 响应头 flush 后放行一个许可（测试据此得知 headers 已交给客户端）。
        headers_sent: Arc<Semaphore>,
        /// 拿到许可后才写 JSON body（在此之前客户端只会停在 response body 读取上）。
        body_gate: Arc<Semaphore>,
    }

    #[derive(Clone)]
    struct FakeConfig {
        exchange_status: u16,
        exchange_body: String,
        exchange_delay: Duration,
        /// 非零许可的闸门：请求已记录后挂起，直到测试显式放行（确定性时序）。
        exchange_gate: Option<Arc<Semaphore>>,
        relay_status: u16,
        relay_body: String,
        relay_delay: Duration,
        relay_gate: Option<Arc<Semaphore>>,
        /// 两阶段响应（仅新用例使用；默认一次性写完 headers+body）。
        relay_two_phase: Option<TwoPhaseReply>,
    }

    impl Default for FakeConfig {
        fn default() -> Self {
            Self {
                exchange_status: 200,
                exchange_body: exchange_ok(&valid_jwt()),
                exchange_delay: Duration::ZERO,
                exchange_gate: None,
                relay_status: 200,
                relay_body: relay_ok(SYNTHETIC_RELAY_TOKEN, 60),
                relay_delay: Duration::ZERO,
                relay_gate: None,
                relay_two_phase: None,
            }
        }
    }

    #[derive(Clone)]
    struct Recorded {
        method: String,
        path: String,
        authorization: Option<String>,
        body: String,
    }

    impl Recorded {
        fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
        }
    }

    struct FakeZcode {
        origin: String,
        config: Arc<Mutex<FakeConfig>>,
        requests: Arc<Mutex<Vec<Recorded>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl FakeZcode {
        async fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("fake 端口");
            let port = listener.local_addr().expect("fake 地址").port();
            let config = Arc::new(Mutex::new(FakeConfig::default()));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let task = tokio::spawn(serve_fake(listener, config.clone(), requests.clone()));
            Self {
                origin: format!("http://127.0.0.1:{port}"),
                config,
                requests,
                task,
            }
        }

        fn set_exchange(&self, status: u16, body: impl Into<String>) {
            let mut config = self.config.lock().unwrap();
            config.exchange_status = status;
            config.exchange_body = body.into();
        }

        fn set_exchange_delay(&self, delay: Duration) {
            self.config.lock().unwrap().exchange_delay = delay;
        }

        fn set_relay(&self, status: u16, body: impl Into<String>) {
            let mut config = self.config.lock().unwrap();
            config.relay_status = status;
            config.relay_body = body.into();
        }

        /// 挂起交换响应（测试放行前不返回），返回放行句柄。
        fn hold_exchange(&self) -> Arc<Semaphore> {
            let gate = Arc::new(Semaphore::new(0));
            self.config.lock().unwrap().exchange_gate = Some(gate.clone());
            gate
        }

        /// 挂起终端 token 响应（测试放行前不返回），返回放行句柄。
        fn hold_relay(&self) -> Arc<Semaphore> {
            let gate = Arc::new(Semaphore::new(0));
            self.config.lock().unwrap().relay_gate = Some(gate.clone());
            gate
        }

        /// 终端 token 响应改为两阶段：先写响应头并通知，body 等测试显式放行。
        fn hold_relay_body(&self) -> TwoPhaseReply {
            let reply = TwoPhaseReply {
                headers_sent: Arc::new(Semaphore::new(0)),
                body_gate: Arc::new(Semaphore::new(0)),
            };
            self.config.lock().unwrap().relay_two_phase = Some(reply.clone());
            reply
        }

        /// 撤下终端 token 闸门（后续请求直接放行）。
        fn clear_relay_gate(&self) {
            self.config.lock().unwrap().relay_gate = None;
        }

        fn requests_matching(&self, path: &str) -> Vec<Recorded> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|recorded| recorded.path.starts_with(path))
                .cloned()
                .collect()
        }

        fn exchange_count(&self) -> usize {
            self.requests_matching(OAUTH_TOKEN_PATH).len()
        }

        fn relay_count(&self) -> usize {
            self.requests_matching(RELAY_TOKEN_PATH).len()
        }

        fn last_exchange(&self) -> Option<Recorded> {
            self.requests_matching(OAUTH_TOKEN_PATH).pop()
        }

        fn last_relay(&self) -> Option<Recorded> {
            self.requests_matching(RELAY_TOKEN_PATH).pop()
        }
    }

    impl Drop for FakeZcode {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn serve_fake(
        listener: TcpListener,
        config: Arc<Mutex<FakeConfig>>,
        requests: Arc<Mutex<Vec<Recorded>>>,
    ) {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let config = config.clone();
            let requests = requests.clone();
            tokio::spawn(async move {
                handle_fake_request(stream, config, requests).await;
            });
        }
    }

    async fn handle_fake_request(
        mut stream: TcpStream,
        config: Arc<Mutex<FakeConfig>>,
        requests: Arc<Mutex<Vec<Recorded>>>,
    ) {
        let Some((head, body)) = read_full_request(&mut stream).await else {
            return;
        };
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or_default().to_string();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let path = parts.next().unwrap_or_default().to_string();
        let authorization = lines.find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("authorization")
                .then(|| value.trim().to_string())
        });
        requests.lock().unwrap().push(Recorded {
            method,
            path: path.clone(),
            authorization,
            body: String::from_utf8_lossy(&body).to_string(),
        });
        let (status, delay, response_body, gate, two_phase) = {
            let config = config.lock().unwrap();
            if path.starts_with(OAUTH_TOKEN_PATH) {
                (
                    config.exchange_status,
                    config.exchange_delay,
                    config.exchange_body.clone(),
                    config.exchange_gate.clone(),
                    None,
                )
            } else if path.starts_with(RELAY_TOKEN_PATH) {
                (
                    config.relay_status,
                    config.relay_delay,
                    config.relay_body.clone(),
                    config.relay_gate.clone(),
                    config.relay_two_phase.clone(),
                )
            } else {
                (404, Duration::ZERO, "{}".to_string(), None, None)
            }
        };
        // 请求已记录：测试可见，再挂起等待显式放行（不发真实等待/睡眠）
        if let Some(gate) = gate {
            let _ = gate.acquire().await;
        }
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let header = format!(
            "HTTP/1.1 {status} FAKE\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response_body.len()
        );
        let _ = stream.write_all(header.as_bytes()).await;
        let _ = stream.flush().await;
        if let Some(two_phase) = two_phase {
            // 响应头已交给客户端；body 必须等测试放行（它把 clear() 放进这个窗口）
            two_phase.headers_sent.add_permits(1);
            let _ = two_phase.body_gate.acquire().await;
        }
        let _ = stream.write_all(response_body.as_bytes()).await;
        let _ = stream.flush().await;
    }

    async fn read_full_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut chunk))
                .await
                .ok()?
                .ok()?;
            if read == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..read]);
            if let Some(position) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
            if buf.len() > 64 * 1024 {
                return None;
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
        let content_length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        let mut body = buf[header_end..].to_vec();
        while body.len() < content_length {
            let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut chunk))
                .await
                .ok()?
                .ok()?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }
        Some((head, body))
    }

    // ───────────────────────── 辅助 ─────────────────────────

    /// 测试客户端：显式禁用系统代理——否则 macOS 系统代理会把 127.0.0.1 请求
    /// 也转发出去（502），与本域行为无关。生产的回调请求由浏览器直连回环，不经此路。
    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("测试 HTTP 客户端")
    }

    fn test_service(fake: &FakeZcode) -> AuthService {
        AuthService::with_endpoints(test_client(), fake.origin.clone(), fake.origin.clone())
    }

    /// 从授权 URL 取出回环 redirect 与 state（两通道参数名不同）。
    fn oauth_params(url: &str) -> (String, String) {
        let parsed = reqwest::Url::parse(url).expect("授权 URL 应合法");
        let params: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
        let redirect = params
            .get("redirect_uri")
            .or_else(|| params.get("redirect"))
            .expect("授权 URL 必带回环 redirect")
            .clone();
        let state = params.get("state").expect("授权 URL 必带 state").clone();
        (redirect, state)
    }

    /// 向回环回调投递一次请求，返回（状态码，静态响应体）。
    async fn deliver(redirect: &str, query: &str) -> Option<(u16, String)> {
        let request = test_client().get(format!("{redirect}?{query}")).send();
        match tokio::time::timeout(Duration::from_secs(2), request).await {
            Ok(Ok(response)) => {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();
                Some((status, body))
            }
            _ => None,
        }
    }

    async fn deliver_status(redirect: &str, query: &str) -> Option<u16> {
        deliver(redirect, query).await.map(|(status, _)| status)
    }

    /// 授权回调 URL 槽：`open` 回调把完整授权 URL 记下，测试体据此投递回调。
    #[derive(Clone, Default)]
    struct UrlSlot(Arc<Mutex<Option<String>>>);

    impl UrlSlot {
        fn new() -> Self {
            Self::default()
        }

        fn get(&self) -> Option<String> {
            self.0.lock().unwrap().clone()
        }

        async fn wait(&self) -> String {
            for _ in 0..600 {
                if let Some(url) = self.get() {
                    return url;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("授权 URL 未在时限内产生");
        }
    }

    fn spawn_authorize(
        auth: Arc<AuthService>,
        provider: OAuthProvider,
        slot: UrlSlot,
    ) -> tokio::task::JoinHandle<ZResult<()>> {
        tokio::spawn(async move {
            let open: OpenAuthorize = Arc::new(move |url: String| {
                *slot.0.lock().unwrap() = Some(url);
                Ok(())
            });
            auth.authorize(provider, open).await
        })
    }

    /// 走完一次成功授权（投递正确 state 的回调，返回授权结果）。
    async fn run_authorize(auth: &Arc<AuthService>, provider: OAuthProvider) -> ZResult<()> {
        let slot = UrlSlot::new();
        let handle = spawn_authorize(auth.clone(), provider, slot.clone());
        let (redirect, state) = oauth_params(&slot.wait().await);
        let status =
            deliver_status(&redirect, &format!("state={state}&code={SYNTHETIC_CODE}")).await;
        assert_eq!(status, Some(200), "正确 state 的回调应被接受");
        handle.await.expect("授权任务不应 panic")
    }

    async fn wait_until(mut predicate: impl FnMut() -> bool) {
        for _ in 0..400 {
            if predicate() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("等待条件超时");
    }

    /// `TerminalToken` 不实现 Debug（安全要求），故不能用 `unwrap_err()`。
    fn err_code<T>(result: ZResult<T>) -> String {
        match result {
            Ok(_) => panic!("预期失败，实际成功"),
            Err(err) => err.code,
        }
    }

    fn assert_secret_free(err: &ZcodeError) {
        let text = format!("{err} {err:?}");
        for secret in [
            "synthetic-code",
            "synthetic-wrong-state",
            "synthetic-jwt",
            "synthetic-upstream",
            "127.0.0.1",
            "http://",
            "https://",
            "Bearer",
        ] {
            assert!(!text.contains(secret), "错误不得包含 {secret}：{text}");
        }
    }

    // ───────────────────────── 必需断言 1 ─────────────────────────

    /// 错误/缺失 state 一律不消费、不交换；正确 state 只交换一次；响应不回显参数。
    #[tokio::test]
    async fn wrong_state_never_exchanges() {
        let fake = FakeZcode::start().await;
        let auth = Arc::new(test_service(&fake));
        let slot = UrlSlot::new();
        let handle = spawn_authorize(auth, OAuthProvider::Bigmodel, slot.clone());
        let (redirect, state) = oauth_params(&slot.wait().await);

        // 错误 state：400 忽略，且不消费
        let (status, body) = deliver(
            &redirect,
            "state=synthetic-wrong-state&code=synthetic-code-wrong",
        )
        .await
        .expect("错误 state 也应有静态响应");
        assert_eq!(status, 400);
        assert!(
            !body.contains("synthetic-wrong-state"),
            "响应不得回显 state"
        );
        assert!(!body.contains("synthetic-code-wrong"), "响应不得回显 code");
        let exchange_count_after_wrong_state = fake.exchange_count();
        assert_eq!(exchange_count_after_wrong_state, 0);

        // 缺 state：同样忽略且不消费
        let status = deliver_status(&redirect, "code=synthetic-code-missing-state").await;
        assert_eq!(status, Some(400));
        assert_eq!(fake.exchange_count(), 0);

        // 正确 state：消费并只交换一次
        let status =
            deliver_status(&redirect, &format!("state={state}&code={SYNTHETIC_CODE}")).await;
        assert_eq!(status, Some(200));
        assert!(handle.await.expect("授权任务不应 panic").is_ok());
        assert_eq!(fake.exchange_count(), 1);
    }

    // ───────────────────────── 必需断言 2 ─────────────────────────

    /// 两通道授权 URL 的参数名不同（BigModel: redirect/appId；ZAI: redirect_uri/client_id）。
    #[test]
    fn oauth_provider_urls() {
        let redirect = "http://127.0.0.1:43210/callback";

        let bigmodel = build_authorize_url(
            OAuthProvider::Bigmodel,
            OFFICIAL_BIGMODEL_ORIGIN,
            redirect,
            "synthetic-state-bigmodel",
        )
        .expect("BigModel 授权 URL 应可构造");
        assert_eq!(bigmodel.path(), "/login");
        assert_eq!(bigmodel.host_str(), Some("bigmodel.cn"));
        assert!(bigmodel
            .query_pairs()
            .any(|(key, value)| key == "appId" && value == "zcode"));
        assert!(bigmodel
            .query_pairs()
            .any(|(key, value)| key == "redirect" && value == redirect));
        assert!(bigmodel
            .query_pairs()
            .any(|(key, value)| key == "state" && value == "synthetic-state-bigmodel"));
        assert!(
            !bigmodel
                .query_pairs()
                .any(|(key, _)| key == "response_type" || key == "client_id"),
            "BigModel 通道不得携带 ZAI 参数"
        );

        let zai = build_authorize_url(
            OAuthProvider::Zai,
            OFFICIAL_ZAI_ORIGIN,
            redirect,
            "synthetic-state-zai",
        )
        .expect("ZAI 授权 URL 应可构造");
        assert_eq!(zai.path(), "/api/oauth/authorize");
        assert_eq!(zai.host_str(), Some("chat.z.ai"));
        assert!(zai
            .query_pairs()
            .any(|(key, value)| key == "client_id" && value == OFFICIAL_OAUTH_CLIENT_ID));
        assert!(zai
            .query_pairs()
            .any(|(key, value)| key == "response_type" && value == "code"));
        assert!(zai
            .query_pairs()
            .any(|(key, value)| key == "redirect_uri" && value == redirect));
        assert!(zai
            .query_pairs()
            .any(|(key, value)| key == "state" && value == "synthetic-state-zai"));
        assert!(!zai.query_pairs().any(|(key, _)| key == "appId"));
    }

    // ───────────────────────── 必需断言 3 ─────────────────────────

    /// 过期 JWT 在任何 HTTP 请求之前就返回 needs_auth，并清空凭据。
    #[tokio::test]
    async fn expired_jwt_requires_auth() {
        let fake = FakeZcode::start().await;
        fake.set_exchange(200, exchange_ok(&expired_jwt()));
        let auth = Arc::new(test_service(&fake));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("交换本身成功（过期判定在终端 token 阶段）");

        assert_eq!(err_code(auth.terminal_token().await), "needs_auth");
        assert_eq!(fake.relay_count(), 0, "过期 JWT 不得发起任何请求");

        // 过期凭据已被清除：再次申请仍 needs_auth 且仍零请求
        assert_eq!(err_code(auth.terminal_token().await), "needs_auth");
        assert_eq!(fake.relay_count(), 0);
    }

    // ───────────────────────── 必需断言 4 ─────────────────────────

    /// clear 中止在途授权；clear 之后迟到的正确回调零交换。
    #[tokio::test]
    async fn canceled_authorization_discards_late_callback() {
        let fake = FakeZcode::start().await;
        let auth = Arc::new(test_service(&fake));
        let slot = UrlSlot::new();
        let handle = spawn_authorize(auth.clone(), OAuthProvider::Bigmodel, slot.clone());
        let (redirect, state) = oauth_params(&slot.wait().await);

        auth.clear();
        let err = handle
            .await
            .expect("授权任务不应 panic")
            .expect_err("clear 应中止在途授权");
        assert_eq!(err.code, "needs_auth");

        // 迟到的正确回调：监听已关闭/epoch 已变，不得产生任何交换
        let _ = tokio::time::timeout(
            Duration::from_millis(300),
            deliver_status(&redirect, &format!("state={state}&code={SYNTHETIC_CODE}")),
        )
        .await;
        let exchanges_after_clear_and_late_callback = fake.exchange_count();
        assert_eq!(exchanges_after_clear_and_late_callback, 0);
    }

    // ───────────────────────── 其余 fake 覆盖 ─────────────────────────

    /// 正确流程：回调只交换一次；交换 body 与终端 token 请求形状符合合同。
    #[tokio::test]
    async fn correct_state_exchanges_once_and_shapes_requests() {
        for (provider, expected_path) in [
            (OAuthProvider::Bigmodel, OFFICIAL_BIGMODEL_AUTHORIZE_PATH),
            (OAuthProvider::Zai, OFFICIAL_ZAI_AUTHORIZE_PATH),
        ] {
            let fake = FakeZcode::start().await;
            let jwt = valid_jwt();
            fake.set_exchange(200, exchange_ok(&jwt));
            let auth = Arc::new(test_service(&fake));

            let slot = UrlSlot::new();
            let handle = spawn_authorize(auth.clone(), provider, slot.clone());
            let authorize_url = slot.wait().await;
            assert_eq!(
                reqwest::Url::parse(&authorize_url)
                    .expect("授权 URL")
                    .path(),
                expected_path
            );
            let (redirect, state) = oauth_params(&authorize_url);
            assert!(
                redirect.starts_with("http://127.0.0.1:"),
                "回调必须是回环地址"
            );
            assert!(redirect.ends_with("/callback"));

            let status =
                deliver_status(&redirect, &format!("state={state}&code={SYNTHETIC_CODE}")).await;
            assert_eq!(status, Some(200));
            handle.await.expect("授权任务不应 panic").expect("授权成功");

            assert_eq!(fake.exchange_count(), 1, "只交换一次");
            let exchange = fake.last_exchange().expect("已记录交换请求");
            assert_eq!(exchange.method, "POST");
            assert_eq!(exchange.path, OAUTH_TOKEN_PATH);
            let body = exchange.json();
            assert_eq!(body["provider"], provider.literal());
            // 断言用布尔比较：失败时不打印授权码/state 的合成值
            assert!(
                body["code"].as_str() == Some(SYNTHETIC_CODE),
                "交换 body 必须携带授权码"
            );
            assert!(
                body["state"].as_str() == Some(state.as_str()),
                "交换 body 必须携带本次 state"
            );
            assert_eq!(body["redirect_uri"], redirect);

            let token = auth.terminal_token().await.expect("终端 token 申请成功");
            // 返回值是短期 relay token（不是账号 JWT），逐次现取
            assert!(
                token.value == SYNTHETIC_RELAY_TOKEN,
                "终端 token 必须是本次 relay 响应值"
            );
            assert_eq!(token.expires_in, 60);
            let relay = fake.last_relay().expect("已记录终端 token 请求");
            assert_eq!(relay.method, "POST");
            assert_eq!(relay.path, RELAY_TOKEN_PATH);
            assert!(
                relay.authorization.as_deref() == Some(format!("Bearer {jwt}").as_str()),
                "终端 token 请求必须携带 Bearer JWT"
            );
            assert_eq!(relay.json()["role"], "terminal");
        }
    }

    /// 同一 state 的重复回调只消费一次，不得二次交换。
    #[tokio::test]
    async fn duplicate_callback_exchanges_once() {
        let fake = FakeZcode::start().await;
        let auth = Arc::new(test_service(&fake));
        let slot = UrlSlot::new();
        let handle = spawn_authorize(auth, OAuthProvider::Bigmodel, slot.clone());
        let (redirect, state) = oauth_params(&slot.wait().await);
        let query = format!("state={state}&code={SYNTHETIC_CODE}");

        let (first, second) = tokio::join!(
            deliver_status(&redirect, &query),
            deliver_status(&redirect, &query)
        );
        assert_eq!(first, Some(200));
        assert_ne!(second, Some(200), "重复回调不得被当作第二次成功");
        handle.await.expect("授权任务不应 panic").expect("授权成功");
        assert_eq!(fake.exchange_count(), 1);
    }

    /// 120s 策略可注入（测试用短窗）：到点以 needs_auth 收尾并关闭监听。
    #[tokio::test]
    async fn authorization_timeout_closes_listener() {
        let fake = FakeZcode::start().await;
        let auth = Arc::new(test_service(&fake).with_callback_timeout(Duration::from_millis(80)));
        let slot = UrlSlot::new();
        let handle = spawn_authorize(auth, OAuthProvider::Zai, slot.clone());
        let (redirect, _state) = oauth_params(&slot.wait().await);

        let err = handle
            .await
            .expect("授权任务不应 panic")
            .expect_err("无回调必须有界超时");
        assert_eq!(err.code, "needs_auth");
        assert_eq!(fake.exchange_count(), 0);

        let port = reqwest::Url::parse(&redirect)
            .expect("回调 URL")
            .port()
            .expect("回环端口");
        let mut closed = false;
        for _ in 0..200 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err()
            {
                closed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(closed, "超时后回调监听应关闭");
    }

    /// 终端 token 401：清空 JWT 并返回 needs_auth，后续不再发请求。
    #[tokio::test]
    async fn terminal_token_401_clears_jwt() {
        let fake = FakeZcode::start().await;
        let jwt = valid_jwt();
        fake.set_exchange(200, exchange_ok(&jwt));
        fake.set_relay(401, r#"{"code":401,"msg":"synthetic-upstream"}"#);
        let auth = Arc::new(test_service(&fake));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("授权成功");

        assert_eq!(err_code(auth.terminal_token().await), "needs_auth");
        assert_eq!(fake.relay_count(), 1);
        assert_eq!(err_code(auth.terminal_token().await), "needs_auth");
        assert_eq!(fake.relay_count(), 1, "401 后凭据已清除，不得再请求");
    }

    /// token 字段畸形：交换阶段失败不落 JWT；relay 阶段失败保留 JWT（非认证失败）。
    #[tokio::test]
    async fn malformed_token_fields_are_rejected() {
        // 交换阶段畸形：授权失败，且不得留下任何凭据
        let exchange_cases: [&str; 5] = [
            r#"{"code":7,"msg":"synthetic-upstream","data":{"token":"synthetic-token"}}"#,
            r#"{"code":0,"msg":"ok","data":{}}"#,
            r#"{"code":0,"msg":"ok","data":{"token":"   "}}"#,
            r#"{"code":0,"msg":"ok","data":{"token":123}}"#,
            "not-json",
        ];
        for body in exchange_cases {
            let fake = FakeZcode::start().await;
            fake.set_exchange(200, body);
            let auth = Arc::new(test_service(&fake));
            let err = run_authorize(&auth, OAuthProvider::Bigmodel)
                .await
                .expect_err("畸形交换响应必须失败");
            assert_eq!(err.code, codes::UNKNOWN);
            assert_eq!(fake.exchange_count(), 1);
            assert_eq!(err_code(auth.terminal_token().await), "needs_auth");
            assert_eq!(fake.relay_count(), 0, "失败交换不得留下凭据");
        }

        // relay 阶段畸形：每次现取都应拒绝（JWT 未被清，因为不是认证失败）
        let relay_cases: [&str; 7] = [
            r#"{"code":0,"data":{"token":"synthetic-relay"}}"#,
            r#"{"code":0,"data":{"token":"synthetic-relay","expires_in":0}}"#,
            r#"{"code":0,"data":{"token":"synthetic-relay","expires_in":-5}}"#,
            r#"{"code":0,"data":{"token":"synthetic-relay","expires_in":60.5}}"#,
            r#"{"code":0,"data":{"token":"synthetic-relay","expires_in":"60"}}"#,
            r#"{"code":0,"data":{"expires_in":60}}"#,
            r#"{"code":9,"data":{"token":"synthetic-relay","expires_in":60}}"#,
        ];
        for body in relay_cases {
            let fake = FakeZcode::start().await;
            fake.set_exchange(200, exchange_ok(&valid_jwt()));
            fake.set_relay(200, body);
            let auth = Arc::new(test_service(&fake));
            run_authorize(&auth, OAuthProvider::Bigmodel)
                .await
                .expect("授权成功");

            let err = match auth.terminal_token().await {
                Ok(_) => panic!("畸形 relay 响应必须失败"),
                Err(err) => err,
            };
            assert_eq!(err.code, codes::UNKNOWN);
            assert_eq!(fake.relay_count(), 1);
            // 非认证失败：JWT 保留，下次仍会现取新 token
            let _ = auth.terminal_token().await;
            assert_eq!(fake.relay_count(), 2);
        }
    }

    /// 失败路径的错误与 Debug/Display 都不含 code/state/JWT/URL/上游原文。
    #[tokio::test]
    async fn errors_never_carry_secrets() {
        // 交换 HTTP 失败
        let fake = FakeZcode::start().await;
        fake.set_exchange(500, r#"{"code":1,"msg":"synthetic-upstream-text"}"#);
        let auth = Arc::new(test_service(&fake));
        let err = run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect_err("500 必须失败");
        assert_eq!(err.code, codes::UNKNOWN);
        assert_secret_free(&err);

        // 终端 token 401：错误不得携带 JWT/URL
        let fake = FakeZcode::start().await;
        let jwt = "synthetic-jwt-secret-marker";
        fake.set_exchange(200, exchange_ok(jwt));
        fake.set_relay(401, r#"{"code":401,"msg":"synthetic-upstream-text"}"#);
        let auth = Arc::new(test_service(&fake));
        run_authorize(&auth, OAuthProvider::Zai)
            .await
            .expect("授权成功");
        let err = match auth.terminal_token().await {
            Ok(_) => panic!("401 必须失败"),
            Err(err) => err,
        };
        assert_eq!(err.code, "needs_auth");
        assert_secret_free(&err);
        let text = format!("{err} {err:?}");
        assert!(!text.contains(jwt), "错误不得包含 JWT");
        assert!(!text.contains("Bearer"));
    }

    /// clear 发生在交换在途时：交换结果作废，不落内存。
    #[tokio::test]
    async fn clear_during_exchange_discards_token() {
        let fake = FakeZcode::start().await;
        fake.set_exchange(200, exchange_ok(&valid_jwt()));
        fake.set_exchange_delay(Duration::from_millis(300));
        let auth = Arc::new(test_service(&fake));
        let slot = UrlSlot::new();
        let handle = spawn_authorize(auth.clone(), OAuthProvider::Bigmodel, slot.clone());
        let (redirect, state) = oauth_params(&slot.wait().await);
        let status =
            deliver_status(&redirect, &format!("state={state}&code={SYNTHETIC_CODE}")).await;
        assert_eq!(status, Some(200));

        wait_until(|| fake.exchange_count() == 1).await;
        auth.clear();
        assert_eq!(
            err_code(handle.await.expect("授权任务不应 panic")),
            "needs_auth"
        );
        assert_eq!(err_code(auth.terminal_token().await), "needs_auth");
        assert_eq!(fake.relay_count(), 0, "clear 后的交换结果不得落内存");
    }

    // ───────────────────────── fix round 1：凭据生命周期 epoch 绑定 ─────────────────────────

    /// 旧 JWT 的迟到 401 不得清掉新授权刚落地的新 JWT。
    ///
    /// 时序（全闸门控制，无真实睡眠）：旧 JWT 申请终端 token（响应挂起）→ 新授权开始
    /// （epoch 已递增、旧 JWT 尚未被替换）→ 新授权完成并落地 JWT2 → 放行迟到的 401。
    #[tokio::test]
    async fn stale_terminal_token_cannot_clear_fresh_jwt() {
        let fake = FakeZcode::start().await;
        let first_jwt = valid_jwt();
        let second_jwt = synthetic_jwt(now_secs() + 7200);
        fake.set_exchange(200, exchange_ok(&first_jwt));
        let auth = Arc::new(test_service(&fake));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("首次授权成功");

        // 旧 JWT 的终端 token 请求：401，但响应先挂起
        fake.set_relay(401, r#"{"code":401,"msg":"synthetic-upstream"}"#);
        let relay_gate = fake.hold_relay();

        // 新授权：交换响应先挂起（epoch 已递增，JWT2 尚未落内存）
        fake.set_exchange(200, exchange_ok(&second_jwt));
        let exchange_gate = fake.hold_exchange();
        let slot = UrlSlot::new();
        let reauth = spawn_authorize(auth.clone(), OAuthProvider::Bigmodel, slot.clone());
        let (redirect, state) = oauth_params(&slot.wait().await);
        let status =
            deliver_status(&redirect, &format!("state={state}&code={SYNTHETIC_CODE}")).await;
        assert_eq!(status, Some(200));
        wait_until(|| fake.exchange_count() == 2).await;

        // 旧 JWT 的 relay 请求在「新授权进行中」发出并挂起
        let stale = tokio::spawn({
            let auth = auth.clone();
            async move { auth.terminal_token().await }
        });
        wait_until(|| fake.relay_count() == 1).await;

        // 新授权完成：JWT2 落内存
        exchange_gate.add_permits(1);
        reauth
            .await
            .expect("授权任务不应 panic")
            .expect("二次授权成功");

        // 迟到的 401 到达：只允许作废它自己，不得清掉新 JWT
        relay_gate.add_permits(1);
        assert_eq!(
            err_code(stale.await.expect("旧请求任务不应 panic")),
            "needs_auth"
        );

        // 新 JWT 仍可用于下一次终端 token 申请
        fake.clear_relay_gate();
        fake.set_relay(200, relay_ok(SYNTHETIC_RELAY_TOKEN, 60));
        match auth.terminal_token().await {
            Ok(token) => assert!(
                token.value == SYNTHETIC_RELAY_TOKEN,
                "终端 token 必须来自本次 relay 响应"
            ),
            Err(err) => panic!("新 JWT 必须仍可用，实际失败码 {}", err.code),
        }
        assert_eq!(fake.relay_count(), 2);
    }

    /// clear 发生在终端 token 请求在途时：成功响应必须被丢弃，不得逃出 token。
    #[tokio::test]
    async fn clear_during_terminal_exchange_discards_result() {
        let fake = FakeZcode::start().await;
        fake.set_exchange(200, exchange_ok(&valid_jwt()));
        let auth = Arc::new(test_service(&fake));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("授权成功");

        fake.set_relay(200, relay_ok(SYNTHETIC_RELAY_TOKEN, 60));
        let relay_gate = fake.hold_relay();
        let pending = tokio::spawn({
            let auth = auth.clone();
            async move { auth.terminal_token().await }
        });
        wait_until(|| fake.relay_count() == 1).await;

        auth.clear();
        relay_gate.add_permits(1);
        assert_eq!(
            err_code(pending.await.expect("请求任务不应 panic")),
            "needs_auth"
        );

        // 结果已丢弃：凭据为空，且不得再发出请求
        assert_eq!(err_code(auth.terminal_token().await), "needs_auth");
        assert_eq!(fake.relay_count(), 1);
    }

    // ───────────────────────── fix round 2：token 交接与代际线性化 ─────────────────────────

    /// clear 落在「响应头已到、JSON body 未到」的窗口：200 响应必须作废，token 不得逃出。
    ///
    /// 时序全由闸门控制（两阶段 fake）：先 flush 响应头并通知 → 测试 `clear()` →
    /// 才放行 body。因此 body 解析完成时凭据代际已变，交接点必须看到这一点；
    /// 旧实现只在响应头之后复核一次，body 读完后就直接返回 token。
    #[tokio::test]
    async fn clear_during_token_response_body_discards_result() {
        let fake = FakeZcode::start().await;
        fake.set_exchange(200, exchange_ok(&valid_jwt()));
        let auth = Arc::new(test_service(&fake));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("授权成功");

        fake.set_relay(200, relay_ok(SYNTHETIC_RELAY_TOKEN, 60));
        let two_phase = fake.hold_relay_body();
        let pending = tokio::spawn({
            let auth = auth.clone();
            async move { auth.terminal_token().await }
        });
        wait_until(|| fake.relay_count() == 1).await;

        // 响应头已 flush：客户端 `send()` 先拿到 headers，随后停在 body 读取上
        assert!(
            two_phase.headers_sent.acquire().await.is_ok(),
            "响应头闸门应正常放行"
        );
        // 让出调度：让客户端把「headers 已到」推进到 body 读取；body 仍被闸门挂起
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(fake.relay_count(), 1);

        // clear 落在 body 读取期间；此后才发布 body
        auth.clear();
        two_phase.body_gate.add_permits(1);

        assert_eq!(
            err_code(pending.await.expect("请求任务不应 panic")),
            "needs_auth"
        );
        // token 未逃出：凭据已清，后续零新请求
        assert_eq!(err_code(auth.terminal_token().await), "needs_auth");
        assert_eq!(fake.relay_count(), 1);
    }

    // ───────────────────────── fix round 1：回调静态页与日志卫生 ─────────────────────────

    /// `error=` 回调：以 needs_auth 收尾、零交换，响应是固定中文错误页且不回显参数。
    #[tokio::test]
    async fn error_callback_requires_auth() {
        let fake = FakeZcode::start().await;
        let auth = Arc::new(test_service(&fake));
        let slot = UrlSlot::new();
        let handle = spawn_authorize(auth, OAuthProvider::Zai, slot.clone());
        let (redirect, state) = oauth_params(&slot.wait().await);

        let (status, body) = deliver(&redirect, &format!("state={state}&error=access_denied"))
            .await
            .expect("error 回调也应有静态响应");
        assert_eq!(status, 200);
        assert!(
            body == CALLBACK_ERROR_PAGE,
            "error 回调响应必须是固定的错误静态页"
        );
        assert!(!body.contains(&state), "响应不得回显 state");
        assert!(!body.contains("access_denied"), "响应不得回显 error 值");

        assert_eq!(
            err_code(handle.await.expect("授权任务不应 panic")),
            "needs_auth"
        );
        assert_eq!(fake.exchange_count(), 0);
    }

    /// 成功回调响应是固定中文页，且不含任何 state/code。
    #[tokio::test]
    async fn callback_success_page_is_static_and_does_not_echo() {
        let fake = FakeZcode::start().await;
        let auth = Arc::new(test_service(&fake));
        let slot = UrlSlot::new();
        let handle = spawn_authorize(auth, OAuthProvider::Bigmodel, slot.clone());
        let (redirect, state) = oauth_params(&slot.wait().await);

        let (status, body) = deliver(&redirect, &format!("state={state}&code={SYNTHETIC_CODE}"))
            .await
            .expect("成功回调应有静态响应");
        assert_eq!(status, 200);
        assert!(
            body == CALLBACK_SUCCESS_PAGE,
            "成功回调响应必须是固定的成功静态页"
        );
        assert!(!body.contains(&state), "响应不得回显 state");
        assert!(!body.contains(SYNTHETIC_CODE), "响应不得回显 code");

        handle.await.expect("授权任务不应 panic").expect("授权成功");
    }

    // ───────────────── C2：授权保存、启动恢复、失效与忘记 ─────────────────

    use super::super::credential_store::{
        CredentialError, MemoryCredentialStore, StoredCredential, SCHEMA_VERSION,
    };

    /// 已预置一条持久记录的 fake 存储（只在测试内使用，不接触真实保险箱）。
    fn store_with(provider: OAuthProvider, jwt: &str) -> Arc<MemoryCredentialStore> {
        let store = Arc::new(MemoryCredentialStore::new());
        store.seed(StoredCredential {
            schema_version: SCHEMA_VERSION,
            provider,
            jwt: jwt.to_owned(),
        });
        store
    }

    /// 恢复持久身份：不需要任何 OAuth 交换，status 只读内存快照。
    #[tokio::test]
    async fn restore_uses_saved_identity_without_oauth() {
        let store = store_with(OAuthProvider::Bigmodel, &valid_jwt());
        let auth = AuthService::new(test_client()).with_credential_store(store.clone());

        let status = auth.restore().await;
        assert!(status.authorized);
        assert!(status.remembered);
        assert!(!status.restoring);
        assert_eq!(status.provider, Some(OAuthProvider::Bigmodel));
        assert_eq!(status.problem, None);
        assert_eq!(store.load_count(), 1);
        assert_eq!(store.save_count(), 0);
        assert_eq!(store.delete_count(), 0);

        // 之后的状态读取只来自内存：不得再次触碰存储。
        assert!(auth.status().authorized);
        assert!(auth.status().remembered);
        assert_eq!(store.load_count(), 1);
    }

    /// 恢复出的 JWT 可直接换 terminal token：恢复路径零 OAuth 交换。
    #[tokio::test]
    async fn restored_credential_requests_terminal_token_without_oauth() {
        let fake = FakeZcode::start().await;
        let jwt = valid_jwt();
        let store = store_with(OAuthProvider::Zai, &jwt);
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));

        assert!(auth.restore().await.authorized);
        let token = auth.terminal_token().await.expect("恢复的 JWT 应可用");
        assert!(token.value == SYNTHETIC_RELAY_TOKEN);
        assert_eq!(fake.exchange_count(), 0, "恢复路径不得 OAuth");
        assert_eq!(fake.relay_count(), 1);
        let relay = fake.last_relay().expect("已记录 relay");
        assert!(
            relay.authorization.as_deref() == Some(format!("Bearer {jwt}").as_str()),
            "终端 token 请求必须携带恢复出的 JWT"
        );
    }

    /// 默认构造不接系统存储：restore 不访问 OS，也不自行打开浏览器。
    #[tokio::test]
    async fn restore_without_store_stays_unauthorized() {
        let auth = AuthService::new(test_client());
        let status = auth.restore().await;
        assert!(!status.authorized);
        assert!(!status.remembered);
        assert!(!status.restoring);
        assert_eq!(status.provider, None);
        assert_eq!(status.problem, None);
        assert_eq!(auth.status(), status);
    }

    /// 授权成功：先落内存再保存；保存成功才标记 remembered。
    #[tokio::test]
    async fn authorize_saves_credential_and_marks_remembered() {
        let fake = FakeZcode::start().await;
        let jwt = valid_jwt();
        fake.set_exchange(200, exchange_ok(&jwt));
        let store = Arc::new(MemoryCredentialStore::new());
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("授权成功");

        let status = auth.status();
        assert!(status.authorized);
        assert!(status.remembered);
        assert!(!status.restoring);
        assert_eq!(status.provider, Some(OAuthProvider::Bigmodel));
        assert_eq!(status.problem, None);
        assert_eq!(store.save_count(), 1);
        let saved = store.stored().expect("应已保存");
        assert_eq!(saved.provider, OAuthProvider::Bigmodel);
        assert!(saved.jwt == jwt, "持久记录必须是本次交换得到的 JWT");
    }

    /// 保存失败：本次授权仍可用、不假报记住、不把 OAuth 变失败。
    #[tokio::test]
    async fn save_failure_keeps_current_authorization() {
        let fake = FakeZcode::start().await;
        fake.set_exchange(200, exchange_ok(&valid_jwt()));
        let store = Arc::new(MemoryCredentialStore::new());
        store.fail_next(CredentialError::StorageFailed);
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));

        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("存储失败不得让授权失败");
        let status = auth.status();
        assert!(status.authorized);
        assert!(!status.remembered);
        assert_eq!(status.problem, Some(AuthProblem::StorageFailed));
        assert_eq!(store.save_count(), 1);
        assert!(store.stored().is_none());

        // 本次身份仍可换 terminal token。
        let token = auth.terminal_token().await.expect("本次授权仍可用");
        assert!(token.value == SYNTHETIC_RELAY_TOKEN);
        assert_eq!(fake.relay_count(), 1);
    }

    /// status 只读内存快照：未授权时也绝不访问存储。
    #[tokio::test]
    async fn status_never_touches_store() {
        let store = store_with(OAuthProvider::Zai, &valid_jwt());
        let auth = AuthService::new(test_client()).with_credential_store(store.clone());
        for _ in 0..3 {
            let status = auth.status();
            assert!(!status.authorized);
            assert!(!status.remembered);
            assert!(!status.restoring);
        }
        assert_eq!(store.load_count(), 0);
        assert_eq!(store.save_count(), 0);
        assert_eq!(store.delete_count(), 0);
    }

    /// 已有当前代有效内存 JWT 时 restore 直接返回，不读存储。
    #[tokio::test]
    async fn restore_with_current_memory_jwt_skips_store() {
        let fake = FakeZcode::start().await;
        fake.set_exchange(200, exchange_ok(&valid_jwt()));
        let store = Arc::new(MemoryCredentialStore::new());
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("授权成功");

        let status = auth.restore().await;
        assert!(status.authorized);
        assert!(status.remembered);
        assert_eq!(status.provider, Some(OAuthProvider::Bigmodel));
        assert_eq!(store.load_count(), 0, "已有有效内存 JWT 不得读存储");
    }

    /// 回归：已有有效内存 JWT 的早退路径必须收尾 `restoring`（旧实现会永久卡在 true）。
    ///
    /// 合成「上一轮恢复已置位 restoring + 当前代有效 JWT 已落内存」：调用 restore 时
    /// 走第一个早退分支，必须清标记并保持 authorized；旧代码直接返回快照，
    /// `restoring` 仍为 true，本用例即失败。
    #[tokio::test]
    async fn restore_clears_stale_restoring_when_memory_jwt_is_current() {
        let auth = AuthService::new(test_client());
        {
            let mut state = auth.lock_state();
            state.epoch = 7;
            state.jwt = Some(valid_jwt());
            state.jwt_epoch = Some(7);
            state.provider = Some(OAuthProvider::Bigmodel);
            state.restoring = true;
        }

        let status = auth.restore().await;
        assert!(status.authorized, "有效内存 JWT 必须保持授权");
        assert!(!status.restoring, "restore 早退必须收尾 restoring");
        assert_eq!(status.provider, Some(OAuthProvider::Bigmodel));
        assert_eq!(auth.status(), status);
    }

    /// 过期持久记录：删除并报 expired，不自动打开浏览器。
    #[tokio::test]
    async fn restore_deletes_expired_record_and_reports_expired() {
        let store = store_with(OAuthProvider::Zai, &expired_jwt());
        let auth = AuthService::new(test_client()).with_credential_store(store.clone());

        let status = auth.restore().await;
        assert!(!status.authorized);
        assert!(!status.remembered);
        assert!(!status.restoring);
        assert_eq!(status.problem, Some(AuthProblem::Expired));
        assert_eq!(status.provider, None);
        assert_eq!(store.load_count(), 1);
        assert_eq!(store.delete_count(), 1, "过期持久记录必须删除");
        assert!(store.stored().is_none());
        assert_eq!(auth.status().problem, Some(AuthProblem::Expired));
    }

    /// 存储锁定/不可用/读坏/超限：映射为对应 problem，且读取失败不删除记录。
    #[tokio::test]
    async fn restore_reports_storage_problems_without_browser() {
        for (error, problem) in [
            (CredentialError::LockedOrDenied, AuthProblem::LockedOrDenied),
            (CredentialError::Unavailable, AuthProblem::Unavailable),
            (CredentialError::StorageFailed, AuthProblem::StorageFailed),
            (CredentialError::InvalidRecord, AuthProblem::InvalidRecord),
            (CredentialError::TooLarge, AuthProblem::TooLarge),
        ] {
            let store = Arc::new(MemoryCredentialStore::new());
            store.fail_next(error);
            let auth = AuthService::new(test_client()).with_credential_store(store.clone());
            let status = auth.restore().await;
            assert!(!status.authorized, "读取失败不得授权");
            assert!(!status.restoring, "读取失败必须收尾 restoring");
            assert_eq!(status.problem, Some(problem));
            assert_eq!(store.load_count(), 1);
            assert_eq!(store.delete_count(), 0, "读取失败不得删除记录");
        }
    }

    /// 当前凭据 401：删除持久记录并报 revoked；5xx/网络错误不删。
    #[tokio::test]
    async fn current_401_deletes_persisted_but_network_error_does_not() {
        // 401：当前代凭据失效 → 持久删除。
        let fake = FakeZcode::start().await;
        fake.set_exchange(200, exchange_ok(&valid_jwt()));
        fake.set_relay(401, r#"{"code":401,"msg":"synthetic-upstream"}"#);
        let store = Arc::new(MemoryCredentialStore::new());
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("授权成功");
        assert_eq!(store.save_count(), 1);
        assert!(auth.status().remembered);

        assert_eq!(err_code(auth.terminal_token().await), "needs_auth");
        let status = auth.status();
        assert!(!status.authorized);
        assert!(!status.remembered);
        assert_eq!(status.problem, Some(AuthProblem::Revoked));
        assert_eq!(store.delete_count(), 1);
        assert!(store.stored().is_none());

        // 5xx/offline：保留凭据与授权，不删。
        let fake = FakeZcode::start().await;
        fake.set_exchange(200, exchange_ok(&valid_jwt()));
        fake.set_relay(503, r#"{"code":503,"msg":"synthetic-upstream"}"#);
        let store = Arc::new(MemoryCredentialStore::new());
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("授权成功");
        assert_eq!(err_code(auth.terminal_token().await), "offline");
        assert_eq!(store.delete_count(), 0, "网络/5xx 不得删除凭据");
        let status = auth.status();
        assert!(status.authorized);
        assert!(status.remembered);
        assert_eq!(status.problem, None);
    }

    /// 内存 JWT 过期（terminal_token 入口）：删除持久记录并报 expired。
    #[tokio::test]
    async fn expired_current_credential_deletes_persisted_record() {
        let fake = FakeZcode::start().await;
        fake.set_exchange(200, exchange_ok(&expired_jwt()));
        let store = Arc::new(MemoryCredentialStore::new());
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("交换成功");
        assert_eq!(store.save_count(), 1);

        assert_eq!(err_code(auth.terminal_token().await), "needs_auth");
        assert_eq!(fake.relay_count(), 0, "过期 JWT 不得发起请求");
        assert_eq!(store.delete_count(), 1);
        assert!(store.stored().is_none());
        let status = auth.status();
        assert!(!status.authorized);
        assert!(!status.remembered);
        assert_eq!(status.problem, Some(AuthProblem::Expired));
    }

    /// 迟到的旧 401 不得删除新授权保存的凭据。
    #[tokio::test]
    async fn late_old_401_never_deletes_new_credential() {
        let fake = FakeZcode::start().await;
        let first_jwt = valid_jwt();
        let second_jwt = synthetic_jwt(now_secs() + 7200);
        fake.set_exchange(200, exchange_ok(&first_jwt));
        let store = Arc::new(MemoryCredentialStore::new());
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("首次授权成功");
        assert_eq!(store.save_count(), 1);

        // 旧 JWT 的 401 响应挂起（尚未交给客户端）。
        fake.set_relay(401, r#"{"code":401,"msg":"synthetic-upstream"}"#);
        let relay_gate = fake.hold_relay();
        let stale = tokio::spawn({
            let auth = auth.clone();
            async move { auth.terminal_token().await }
        });
        wait_until(|| fake.relay_count() == 1).await;

        // 新授权完成：交换先挂起，回调投递后再放行。
        fake.set_exchange(200, exchange_ok(&second_jwt));
        let exchange_gate = fake.hold_exchange();
        let slot = UrlSlot::new();
        let reauth = spawn_authorize(auth.clone(), OAuthProvider::Bigmodel, slot.clone());
        let (redirect, state) = oauth_params(&slot.wait().await);
        let status =
            deliver_status(&redirect, &format!("state={state}&code={SYNTHETIC_CODE}")).await;
        assert_eq!(status, Some(200));
        wait_until(|| fake.exchange_count() == 2).await;
        exchange_gate.add_permits(1);
        reauth
            .await
            .expect("授权任务不应 panic")
            .expect("二次授权成功");
        assert_eq!(store.save_count(), 2);
        assert!(auth.status().remembered);

        // 迟到的 401 到达：只作废它自己，不得删除新凭据。
        relay_gate.add_permits(1);
        assert_eq!(
            err_code(stale.await.expect("旧请求任务不应 panic")),
            "needs_auth"
        );
        assert_eq!(store.delete_count(), 0, "旧 401 不得删除新凭据");
        let status = auth.status();
        assert!(status.authorized, "新授权不得被旧 401 清掉");
        assert!(status.remembered);
        assert!(
            store.stored().expect("新凭据仍应保存").jwt == second_jwt,
            "持久记录必须仍是新 JWT"
        );

        // 新凭据仍可用。
        fake.clear_relay_gate();
        fake.set_relay(200, relay_ok(SYNTHETIC_RELAY_TOKEN, 60));
        auth.terminal_token().await.expect("新凭据仍可用");
    }

    /// 忘记授权：清内存并删除持久记录，之后 restore 不再访问存储。
    #[tokio::test]
    async fn forget_deletes_persisted_record_and_blocks_restore() {
        let fake = FakeZcode::start().await;
        fake.set_exchange(200, exchange_ok(&valid_jwt()));
        let store = Arc::new(MemoryCredentialStore::new());
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("授权成功");

        let status = auth.forget().await;
        assert!(!status.authorized);
        assert!(!status.remembered);
        assert!(!status.restoring);
        assert_eq!(status.provider, None);
        assert_eq!(status.problem, None);
        assert_eq!(store.delete_count(), 1);
        assert!(store.stored().is_none());

        let restored = auth.restore().await;
        assert!(!restored.authorized);
        assert_eq!(store.load_count(), 0, "忘记后不得再从存储恢复");
    }

    /// 忘记时删除失败：如实报 delete_failed，并保持恢复禁用。
    #[tokio::test]
    async fn forget_reports_delete_failure_and_stays_blocked() {
        let fake = FakeZcode::start().await;
        fake.set_exchange(200, exchange_ok(&valid_jwt()));
        let store = Arc::new(MemoryCredentialStore::new());
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("授权成功");
        store.fail_next(CredentialError::DeleteFailed);

        let status = auth.forget().await;
        assert!(!status.authorized);
        assert!(!status.remembered);
        assert_eq!(status.problem, Some(AuthProblem::DeleteFailed));
        assert_eq!(store.delete_count(), 1);
        assert!(store.stored().is_some(), "删除失败时记录可能仍在");

        // 恢复仍被禁用：不得读回旧记录。
        let restored = auth.restore().await;
        assert!(!restored.authorized);
        assert_eq!(restored.problem, Some(AuthProblem::DeleteFailed));
        assert_eq!(store.load_count(), 0);
    }

    /// 忘记后迟到的授权结果不得落库复活。
    #[tokio::test]
    async fn forget_during_exchange_never_resurrects_credential() {
        let fake = FakeZcode::start().await;
        fake.set_exchange(200, exchange_ok(&valid_jwt()));
        fake.set_exchange_delay(Duration::from_millis(300));
        let store = Arc::new(MemoryCredentialStore::new());
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));
        let slot = UrlSlot::new();
        let handle = spawn_authorize(auth.clone(), OAuthProvider::Bigmodel, slot.clone());
        let (redirect, state) = oauth_params(&slot.wait().await);
        let status =
            deliver_status(&redirect, &format!("state={state}&code={SYNTHETIC_CODE}")).await;
        assert_eq!(status, Some(200));
        wait_until(|| fake.exchange_count() == 1).await;

        let _ = auth.forget().await;
        assert_eq!(
            err_code(handle.await.expect("授权任务不应 panic")),
            "needs_auth"
        );
        assert_eq!(store.save_count(), 0, "忘记后旧授权不得落库");
        assert!(store.stored().is_none());
        assert_eq!(auth.status().problem, None);
    }

    /// AuthStatus 是唯一新增 DTO：camelCase、problem snake_case、provider 小写，且绝不含 JWT。
    #[tokio::test]
    async fn auth_status_never_carries_jwt() {
        let fake = FakeZcode::start().await;
        let jwt = valid_jwt();
        fake.set_exchange(200, exchange_ok(&jwt));
        let store = Arc::new(MemoryCredentialStore::new());
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("授权成功");

        let status = auth.status();
        let json = serde_json::to_string(&status).expect("AuthStatus 必须可序列化");
        assert!(!json.contains(&jwt), "AuthStatus 不得携带 JWT");
        assert!(!json.contains("synthetic-signature"), "JWT 段不得进入 DTO");
        assert!(!format!("{status:?}").contains(&jwt), "Debug 也不得含 JWT");
        let value: serde_json::Value = serde_json::from_str(&json).expect("合法 JSON");
        assert_eq!(value["authorized"], true);
        assert_eq!(value["remembered"], true);
        assert_eq!(value["restoring"], false);
        assert_eq!(value["provider"], "bigmodel");
        assert!(value.get("problem").is_some(), "problem 字段必须始终存在");
        assert!(value["problem"].is_null());
        assert!(value.get("jwt").is_none(), "DTO 不得出现 jwt 字段");

        assert_eq!(
            serde_json::to_string(&AuthProblem::LockedOrDenied).unwrap(),
            "\"locked_or_denied\""
        );
        assert_eq!(
            serde_json::to_string(&AuthProblem::TooLarge).unwrap(),
            "\"too_large\""
        );
    }

    // ───────────────── C2 复审修整：失效删除失败/过期快照/403/clear ─────────────────

    /// 401/过期删除失败：禁止 restore 把已撤销 JWT 重新授权（problem 保留原失效原因）。
    #[tokio::test]
    async fn invalidated_delete_failure_blocks_restore() {
        // 401 → invalidate → 删除失败。
        let fake = FakeZcode::start().await;
        fake.set_exchange(200, exchange_ok(&valid_jwt()));
        fake.set_relay(401, r#"{"code":401,"msg":"synthetic-upstream"}"#);
        let store = Arc::new(MemoryCredentialStore::new());
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("授权成功");
        store.fail_next(CredentialError::DeleteFailed);

        assert_eq!(err_code(auth.terminal_token().await), "needs_auth");
        assert_eq!(store.delete_count(), 1);
        assert!(store.stored().is_some(), "删除失败时记录可能仍在");
        let status = auth.status();
        assert!(!status.authorized);
        assert_eq!(status.problem, Some(AuthProblem::Revoked));

        let restored = auth.restore().await;
        assert!(!restored.authorized);
        assert_eq!(restored.problem, Some(AuthProblem::Revoked));
        assert_eq!(store.load_count(), 0, "删除失败后不得 restore 旧记录");

        // 内存 JWT 过期 → invalidate → 删除失败。
        let fake = FakeZcode::start().await;
        fake.set_exchange(200, exchange_ok(&expired_jwt()));
        let store = Arc::new(MemoryCredentialStore::new());
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));
        run_authorize(&auth, OAuthProvider::Zai)
            .await
            .expect("交换成功");
        store.fail_next(CredentialError::DeleteFailed);

        assert_eq!(err_code(auth.terminal_token().await), "needs_auth");
        assert_eq!(store.delete_count(), 1);
        assert!(store.stored().is_some(), "删除失败时记录可能仍在");
        assert_eq!(auth.status().problem, Some(AuthProblem::Expired));

        let restored = auth.restore().await;
        assert!(!restored.authorized);
        assert_eq!(restored.problem, Some(AuthProblem::Expired));
        assert_eq!(store.load_count(), 0, "过期记录删除失败后不得 restore");
    }

    /// 状态轮询遇到内存里已过期 JWT：authorized=false 且快照报 expired（不写状态）。
    #[tokio::test]
    async fn status_expired_memory_jwt_reports_expired() {
        let fake = FakeZcode::start().await;
        fake.set_exchange(200, exchange_ok(&expired_jwt()));
        let auth = Arc::new(test_service(&fake));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("交换成功");

        let first = auth.status();
        assert!(!first.authorized, "过期 JWT 不得在状态轮询中显示已授权");
        assert_eq!(first.problem, Some(AuthProblem::Expired));
        // 快照只读：再次读取结果一致（没有偷偷改写状态）。
        assert_eq!(auth.status(), first);
        // 真正清理由 terminal_token 的失效入口完成。
        assert_eq!(err_code(auth.terminal_token().await), "needs_auth");
    }

    /// 403 不删除持久记录，也不把 JWT 判为失效。
    #[tokio::test]
    async fn forbidden_terminal_token_keeps_persisted_record() {
        let fake = FakeZcode::start().await;
        fake.set_exchange(200, exchange_ok(&valid_jwt()));
        fake.set_relay(403, r#"{"code":403,"msg":"synthetic-upstream"}"#);
        let store = Arc::new(MemoryCredentialStore::new());
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("授权成功");

        assert_eq!(err_code(auth.terminal_token().await), "needs_auth");
        assert_eq!(store.delete_count(), 0, "403 不得删除持久记录");
        assert!(store.stored().is_some());
        let status = auth.status();
        assert!(status.authorized);
        assert!(status.remembered);
        assert_eq!(status.problem, None);
    }

    /// clear 只清运行时内存：持久记录仍在、remembered 保留，restore 可复用。
    #[tokio::test]
    async fn clear_does_not_delete_persisted_record() {
        let fake = FakeZcode::start().await;
        fake.set_exchange(200, exchange_ok(&valid_jwt()));
        let store = Arc::new(MemoryCredentialStore::new());
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("授权成功");

        auth.clear();
        assert_eq!(store.delete_count(), 0, "clear 不得删除持久记录");
        assert!(store.stored().is_some());
        let status = auth.status();
        assert!(!status.authorized);
        assert!(status.remembered, "持久记录仍在，remembered 必须保留");
        assert_eq!(status.problem, None);
        assert_eq!(status.provider, None);

        // 持久身份仍可恢复。
        let restored = auth.restore().await;
        assert!(restored.authorized);
        assert!(restored.remembered);
        assert_eq!(store.load_count(), 1);
    }

    /// clear 清掉本次会话的临时 problem。
    #[tokio::test]
    async fn clear_clears_transient_problem() {
        let fake = FakeZcode::start().await;
        fake.set_exchange(200, exchange_ok(&valid_jwt()));
        let store = Arc::new(MemoryCredentialStore::new());
        store.fail_next(CredentialError::StorageFailed);
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("保存失败不影响授权");
        assert_eq!(auth.status().problem, Some(AuthProblem::StorageFailed));

        auth.clear();
        let status = auth.status();
        assert!(!status.authorized);
        assert!(!status.remembered);
        assert_eq!(status.problem, None, "clear 必须清掉临时 problem");
    }

    // ───────────────── H2：受控竞态与故障矩阵（闸门精确交错，无真实睡眠） ─────────────────

    /// 把一次状态快照纳入「不得泄漏 secret」的输出捕获（serde + Debug）。
    fn capture_status(output: &mut String, status: &AuthStatus) {
        output.push_str(&serde_json::to_string(status).expect("AuthStatus 必须可序列化"));
        output.push(' ');
        output.push_str(&format!("{status:?}"));
        output.push(' ');
    }

    /// 旧 restore 的 load 在途时新授权已落地：放行旧 load 后不得覆盖新 provider/JWT。
    ///
    /// 闸门控制：旧 `load` 进入 fake 后挂起并持有存储串行 → 新授权完成交换、JWT 落内存，
    /// 其保存排队 → 放行旧 `load`。旧 restore 必须丢弃结果（epoch 已变 + 新 JWT 已落地），
    /// 后续 terminal token 必须携带**新** JWT，持久记录也只能是新 JWT。
    #[tokio::test]
    async fn late_restore_cannot_override_new_authorization() {
        let fake = FakeZcode::start().await;
        let old_jwt = valid_jwt();
        let new_jwt = synthetic_jwt(now_secs() + 7200);
        let store = store_with(OAuthProvider::Zai, &old_jwt);
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));

        // 旧 restore 的 load 停在 fake 内，持有存储单飞锁。
        let load_gate = store.gate_load();
        let restore = tokio::spawn({
            let auth = auth.clone();
            async move { auth.restore().await }
        });
        load_gate.wait_entered().await;

        // 新授权：交换立即返回，新 JWT 落内存；保存必须排在在途 load 之后。
        fake.set_exchange(200, exchange_ok(&new_jwt));
        let slot = UrlSlot::new();
        let reauth = spawn_authorize(auth.clone(), OAuthProvider::Bigmodel, slot.clone());
        let (redirect, state) = oauth_params(&slot.wait().await);
        let delivered =
            deliver_status(&redirect, &format!("state={state}&code={SYNTHETIC_CODE}")).await;
        assert_eq!(delivered, Some(200), "回调应被接受");
        wait_until(|| auth.status().authorized).await;
        assert_eq!(
            auth.status().provider,
            Some(OAuthProvider::Bigmodel),
            "新授权的 provider 已落内存"
        );
        assert_eq!(store.save_count(), 0, "旧 load 未完成前新保存必须排队");
        assert!(!reauth.is_finished(), "新授权必须等待存储串行");

        // 放行旧 load：旧 restore 必须丢弃结果，不得覆盖新身份。
        load_gate.release();
        let status_after_old_restore = restore.await.expect("旧 restore 任务不应 panic");
        assert!(status_after_old_restore.authorized, "新身份仍应处于已授权");
        assert_eq!(
            status_after_old_restore.provider,
            Some(OAuthProvider::Bigmodel),
            "旧 restore 不得用旧记录的 provider 覆盖新授权"
        );
        let status = auth.status();
        assert!(status.authorized);
        assert_eq!(status.provider, Some(OAuthProvider::Bigmodel));
        assert_eq!(status.problem, None);

        reauth
            .await
            .expect("授权任务不应 panic")
            .expect("新授权成功");
        assert_eq!(store.save_count(), 1, "排队的保存应在旧 load 之后执行一次");
        assert!(
            store.stored().expect("应保存新记录").jwt == new_jwt,
            "持久记录必须来自新授权"
        );

        // 内存里实际持有的必须是新 JWT（用下一次 relay 请求的 Authorization 证明）。
        fake.set_relay(200, relay_ok(SYNTHETIC_RELAY_TOKEN, 60));
        let token = auth.terminal_token().await.expect("新 JWT 仍可用");
        assert!(token.value == SYNTHETIC_RELAY_TOKEN);
        let relay = fake.last_relay().expect("已记录 relay 请求");
        assert!(
            relay.authorization.as_deref() == Some(format!("Bearer {new_jwt}").as_str()),
            "终端 token 请求必须携带新 JWT，而不是旧 restore 的记录"
        );

        // 旧记录不得在后续恢复中复活。
        auth.clear();
        let restored = auth.restore().await;
        assert!(restored.authorized);
        assert_eq!(restored.provider, Some(OAuthProvider::Bigmodel));
        assert!(store.stored().expect("记录仍在").jwt == new_jwt);
    }

    /// 慢保存在途时用户忘记授权：保存不得复活旧记录，重启后也不得恢复。
    ///
    /// 闸门控制：保存进入 fake 后挂起（尚未写记录）→ `forget` 先清内存/禁用恢复，
    /// 其删除排队 → 放行保存（写旧记录）→ 排队的删除必须把它删掉；旧保存不得回写
    /// `remembered`。随后新建 AuthService 模拟重启，旧记录不得被恢复。
    #[tokio::test]
    async fn forget_during_slow_save_never_restores_old_record() {
        let fake = FakeZcode::start().await;
        let jwt = valid_jwt();
        fake.set_exchange(200, exchange_ok(&jwt));
        let store = Arc::new(MemoryCredentialStore::new());
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));

        // 保存停在「原生写完成前」：内存已授权，持久记录尚未落地。
        let save_gate = store.gate_save();
        let slot = UrlSlot::new();
        let authorize = spawn_authorize(auth.clone(), OAuthProvider::Bigmodel, slot.clone());
        let (redirect, state) = oauth_params(&slot.wait().await);
        let delivered =
            deliver_status(&redirect, &format!("state={state}&code={SYNTHETIC_CODE}")).await;
        assert_eq!(delivered, Some(200));
        save_gate.wait_entered().await;
        assert!(auth.status().authorized, "保存前 JWT 已落内存");
        assert_eq!(store.save_count(), 1, "保存已进入 fake");
        assert!(store.stored().is_none(), "保存在放行前不得写入记录");

        // 忘记：清内存并禁用恢复，删除必须排在在途保存之后。
        let forget = tokio::spawn({
            let auth = auth.clone();
            async move { auth.forget().await }
        });
        wait_until(|| !auth.status().authorized).await;
        assert_eq!(store.delete_count(), 0, "在途保存未完成前删除必须排队");
        assert_eq!(store.max_concurrent_store_calls(), 1, "存储调用必须串行");

        // 放行保存：写旧记录 → 排队的删除执行 → 旧保存不得回写 remembered。
        save_gate.release();
        authorize
            .await
            .expect("授权任务不应 panic")
            .expect("授权本身成功");
        let status_after_forget = forget.await.expect("忘记任务不应 panic");
        assert!(!status_after_forget.authorized);
        assert!(!status_after_forget.remembered);
        assert_eq!(status_after_forget.problem, None);
        assert_eq!(store.save_count(), 1, "旧保存只执行一次");
        assert_eq!(store.delete_count(), 1, "排队的删除必须在保存之后执行");
        assert!(store.stored().is_none(), "旧保存的记录必须被删除");
        assert_eq!(store.max_concurrent_store_calls(), 1);

        // 模拟重启：全新 AuthService 对同一存储 restore，旧记录不得复活。
        let restarted = AuthService::new(test_client()).with_credential_store(store.clone());
        let after_restart = restarted.restore().await;
        let restored_after_forget = usize::from(after_restart.authorized);
        assert!(!after_restart.authorized);
        assert_eq!(restored_after_forget, 0);
        assert!(!after_restart.remembered);
        assert_eq!(after_restart.provider, None);
        assert_eq!(after_restart.problem, None);
        assert_eq!(store.load_count(), 1, "重启只读一次存储");
        assert!(store.stored().is_none(), "旧记录不得被迟到的保存复活");
        assert_eq!(store.max_concurrent_store_calls(), 1);
    }

    /// 慢 load 在途时用户忘记授权：旧记录不得被提交为当前身份，删除仍须执行。
    ///
    /// 闸门控制：`load` 进入 fake 后挂起 → 先驱动 `forget` 的第一次轮询（清内存/禁用恢复/
    /// 推进 epoch 都在它首个 await 之前完成；其删除排在在途 load 之后，故此刻不可能完成）
    /// → 放行 load。旧 load 的结果只能被 epoch 复核丢弃；删除随后真正执行，重启无恢复。
    #[tokio::test]
    async fn forget_during_slow_load_never_restores_old_record() {
        let fake = FakeZcode::start().await;
        let old_jwt = valid_jwt();
        let store = store_with(OAuthProvider::Zai, &old_jwt);
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));

        let load_gate = store.gate_load();
        let restore = tokio::spawn({
            let auth = auth.clone();
            async move { auth.restore().await }
        });
        load_gate.wait_entered().await;
        assert_eq!(store.load_count(), 1, "load 已进入 fake");

        // 驱动 forget 一次：状态更新同步完成并停在「等待存储串行」上。
        let mut forget = Box::pin(auth.forget());
        tokio::select! {
            biased;
            status = &mut forget => panic!("在途 load 完成前 forget 不得完成：{status:?}"),
            _ = tokio::task::yield_now() => {}
        }
        assert_eq!(store.delete_count(), 0, "在途 load 未完成前删除必须排队");
        assert_eq!(store.max_concurrent_store_calls(), 1);

        // 放行 load：旧记录不得被提交（epoch 已变），随后排队的删除真正执行。
        load_gate.release();
        let restored = restore.await.expect("旧 restore 任务不应 panic");
        assert!(!restored.authorized, "旧 load 的记录不得被提交为当前身份");
        assert!(!restored.remembered);
        assert_eq!(restored.provider, None);
        assert_eq!(restored.problem, None);
        assert!(!auth.status().authorized);

        let status_after_forget = forget.await;
        assert!(!status_after_forget.authorized);
        assert!(!status_after_forget.remembered);
        assert_eq!(status_after_forget.problem, None);
        assert_eq!(store.load_count(), 1, "忘记流程不得再次读存储");
        assert_eq!(store.delete_count(), 1, "排队的删除必须在 load 之后执行");
        assert!(store.stored().is_none(), "旧记录必须被删除");
        assert_eq!(store.max_concurrent_store_calls(), 1);

        // 模拟重启：全新 AuthService 对同一存储 restore，旧记录不得恢复。
        let restarted = AuthService::new(test_client()).with_credential_store(store.clone());
        let after_restart = restarted.restore().await;
        assert!(!after_restart.authorized);
        assert!(!after_restart.remembered);
        assert_eq!(after_restart.provider, None);
        assert_eq!(after_restart.problem, None);
        assert_eq!(store.load_count(), 2, "重启只读一次存储");
        assert!(store.stored().is_none());
        assert_eq!(store.max_concurrent_store_calls(), 1);
    }

    /// 旧 JWT 的 401 迟到时新授权已保存：不得删除新记录。
    ///
    /// 401 是**按响应头**判定失效的（响应体从不读取，两阶段闸门证明旧请求在 body
    /// 发布之前就已收尾），所以受控窗口是「整个 401 响应尚未交付」：先让新授权完整
    /// 保存新记录，再放行旧 401 的响应头。客户端在头后的代际复核处丢弃旧代结果，
    /// 只能作废它自己，不得清掉新 JWT、删除新记录或给新身份挂 `revoked`。
    #[tokio::test]
    async fn late_401_during_new_save_does_not_delete_new_record() {
        let fake = FakeZcode::start().await;
        let first_jwt = valid_jwt();
        let second_jwt = synthetic_jwt(now_secs() + 7200);
        fake.set_exchange(200, exchange_ok(&first_jwt));
        let store = Arc::new(MemoryCredentialStore::new());
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("首次授权成功");
        assert_eq!(store.save_count(), 1);

        // 旧 JWT 的 401：整个响应先挂起（headers 与 body 都未交付），body 闸门待命。
        fake.set_relay(401, r#"{"code":401,"msg":"synthetic-upstream"}"#);
        let relay_gate = fake.hold_relay();
        let two_phase = fake.hold_relay_body();
        let stale = tokio::spawn({
            let auth = auth.clone();
            async move { auth.terminal_token().await }
        });
        wait_until(|| fake.relay_count() == 1).await;
        assert!(!stale.is_finished(), "旧 401 响应交付前不得收尾");

        // 新授权完成并保存新记录（此时旧 401 响应仍未交付）。
        fake.set_exchange(200, exchange_ok(&second_jwt));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("新授权成功");
        assert_eq!(store.save_count(), 2);
        assert!(
            store.stored().expect("新记录").jwt == second_jwt,
            "新记录必须已保存"
        );

        // 放行旧 401 的响应头：按时按代际复核丢弃，不得删除新记录。
        relay_gate.add_permits(1);
        assert!(
            two_phase.headers_sent.acquire().await.is_ok(),
            "401 响应头闸门应正常放行"
        );
        assert_eq!(
            err_code(stale.await.expect("旧请求任务不应 panic")),
            "needs_auth"
        );
        // 旧请求在 body 发布之前就收尾：401 从不读取响应体。
        let new_record_deleted = store.delete_count();
        assert_eq!(new_record_deleted, 0, "迟到的旧 401 不得删除新记录");
        assert!(
            store.stored().expect("新记录仍应存在").jwt == second_jwt,
            "持久记录必须仍是新 JWT"
        );
        let status = auth.status();
        assert!(status.authorized, "新授权不得被旧 401 清掉");
        assert!(status.remembered);
        assert_eq!(status.provider, Some(OAuthProvider::Bigmodel));
        assert_eq!(
            status.problem, None,
            "旧 401 不得给新身份挂 revoked/storage 问题"
        );

        // 清理仍挂起的 body（连接已关闭，写入失败不影响断言）。
        two_phase.body_gate.add_permits(1);
    }

    /// 调用方 future 取消后，仍在执行的原生调用继续持有串行所有权；最大并发必须为 1。
    ///
    /// 三段串联：load 在途取消 → save 排队；save 在途取消 → delete 排队；
    /// 每次放行后下一段才进入 fake。`max_concurrent_store_calls` 由 fake 的探针记录，
    /// 提前释放 owned guard 会让它变成 2。
    #[tokio::test]
    async fn cancellation_keeps_native_io_serial_until_actual_completion() {
        let fake = FakeZcode::start().await;
        let store = store_with(OAuthProvider::Zai, &valid_jwt());
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));

        // ① load 在途，restore 的调用方被取消（future drop，阻塞任务继续执行）。
        let load_gate = store.gate_load();
        let restore = tokio::spawn({
            let auth = auth.clone();
            async move { auth.restore().await }
        });
        load_gate.wait_entered().await;
        restore.abort();
        assert!(
            restore
                .await
                .expect_err("已取消的任务必须报 JoinError")
                .is_cancelled(),
            "restore 调用方必须已取消"
        );

        // ② 新授权保存排队：被取消的 load 仍持串行，保存不得进入 fake。
        fake.set_exchange(200, exchange_ok(&valid_jwt()));
        let save_gate = store.gate_save();
        let slot = UrlSlot::new();
        let authorize = spawn_authorize(auth.clone(), OAuthProvider::Bigmodel, slot.clone());
        let (redirect, state) = oauth_params(&slot.wait().await);
        let delivered =
            deliver_status(&redirect, &format!("state={state}&code={SYNTHETIC_CODE}")).await;
        assert_eq!(delivered, Some(200));
        wait_until(|| auth.status().authorized).await;
        assert_eq!(store.save_count(), 0, "被取消的 load 未完成前保存不得进入");
        assert_eq!(store.max_concurrent_store_calls(), 1);

        // 放行 load：保存进入并挂起；保存的调用方同样被取消。
        load_gate.release();
        save_gate.wait_entered().await;
        authorize.abort();
        assert!(authorize
            .await
            .expect_err("已取消的授权必须报 JoinError")
            .is_cancelled());

        // ③ 忘记的删除排队：在途保存仍持串行，删除不得进入 fake。
        let delete_gate = store.gate_delete();
        let forget = tokio::spawn({
            let auth = auth.clone();
            async move { auth.forget().await }
        });
        wait_until(|| !auth.status().authorized).await;
        assert_eq!(store.delete_count(), 0, "在途保存未完成前删除不得进入");
        assert_eq!(store.max_concurrent_store_calls(), 1);

        // 放行保存 → 删除进入 → 放行删除；全程只有一次调用在 fake 内。
        save_gate.release();
        delete_gate.wait_entered().await;
        assert_eq!(store.save_count(), 1, "被取消的保存仍须完成一次");
        assert_eq!(store.delete_count(), 1);
        delete_gate.release();
        let status = forget.await.expect("忘记任务不应 panic");
        assert!(!status.authorized);
        assert_eq!(status.problem, None);
        assert_eq!(store.load_count(), 1);
        assert!(store.stored().is_none(), "删除必须在保存之后真正完成");
        assert_eq!(
            store.max_concurrent_store_calls(),
            1,
            "取消调用方不得让底层原生调用脱离串行"
        );
    }

    /// 故障矩阵：存储失败绝不删除有效身份，也绝不把合成 JWT 泄漏到输出。
    ///
    /// 组合覆盖 locked / unavailable / invalid_record / too_large 读取失败、
    /// storage_failed / locked 保存失败、delete_failed 删除失败；每次失败后都验证
    /// 既有有效记录仍可恢复、problem 如实映射，最后核对捕获的状态/错误输出无 JWT。
    #[tokio::test]
    async fn store_failures_never_delete_valid_identity_or_leak_secret() {
        let fake = FakeZcode::start().await;
        let saved_jwt = valid_jwt();
        fake.set_exchange(200, exchange_ok(&saved_jwt));
        let store = Arc::new(MemoryCredentialStore::new());
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));
        run_authorize(&auth, OAuthProvider::Zai)
            .await
            .expect("首次授权成功");
        assert!(store.stored().is_some(), "前置：有效身份已保存");

        let mut captured_output = String::new();
        let mut issued_jwts = vec![saved_jwt.clone()];

        // 读取类故障：各自映射 problem；读取失败不得删除有效身份。
        for (error, problem) in [
            (CredentialError::LockedOrDenied, AuthProblem::LockedOrDenied),
            (CredentialError::Unavailable, AuthProblem::Unavailable),
            (CredentialError::InvalidRecord, AuthProblem::InvalidRecord),
            (CredentialError::TooLarge, AuthProblem::TooLarge),
        ] {
            auth.clear();
            store.fail_next(error);
            let status = auth.restore().await;
            capture_status(&mut captured_output, &status);
            assert!(!status.authorized, "读取失败不得授权");
            assert!(!status.remembered);
            assert!(!status.restoring);
            assert_eq!(status.problem, Some(problem), "problem 必须如实映射");
        }
        let deletes_after_locked_or_network_failure = store.delete_count();
        assert_eq!(
            deletes_after_locked_or_network_failure, 0,
            "读取失败不得删除有效身份"
        );

        // 故障是一次性的：有效身份仍可恢复（证明未被误删）。
        auth.clear();
        let recovered = auth.restore().await;
        capture_status(&mut captured_output, &recovered);
        assert!(recovered.authorized);
        assert!(recovered.remembered);
        assert_eq!(recovered.provider, Some(OAuthProvider::Zai));

        // 保存失败：本次身份仍可用、不得假报已记住、旧有效记录不得被删。
        for error in [
            CredentialError::StorageFailed,
            CredentialError::LockedOrDenied,
        ] {
            let next_jwt = synthetic_jwt(now_secs() + 7200);
            issued_jwts.push(next_jwt.clone());
            fake.set_exchange(200, exchange_ok(&next_jwt));
            store.fail_next(error);
            run_authorize(&auth, OAuthProvider::Bigmodel)
                .await
                .expect("保存失败不得让 OAuth 失败");
            let status = auth.status();
            capture_status(&mut captured_output, &status);
            assert!(status.authorized, "保存失败不得丢掉本次身份");
            assert!(!status.remembered, "保存失败不得假报已记住");
            assert_eq!(status.problem, Some(AuthProblem::from_credential(error)));
            assert_eq!(store.delete_count(), 0, "保存失败不得删除旧记录");
            assert!(
                store.stored().expect("旧有效记录仍应存在").jwt == saved_jwt,
                "保存失败不得覆盖旧有效记录"
            );
        }

        // 删除失败：如实报 delete_failed、记录可能仍在、恢复保持禁用（零 load）。
        let loads_before_forget = store.load_count();
        store.fail_next(CredentialError::DeleteFailed);
        let status = auth.forget().await;
        capture_status(&mut captured_output, &status);
        assert!(!status.authorized);
        assert!(!status.remembered);
        assert_eq!(status.problem, Some(AuthProblem::DeleteFailed));
        assert!(store.stored().is_some(), "删除失败时记录可能仍在");
        let blocked = auth.restore().await;
        capture_status(&mut captured_output, &blocked);
        assert!(!blocked.authorized);
        assert_eq!(blocked.problem, Some(AuthProblem::DeleteFailed));
        assert_eq!(
            store.load_count(),
            loads_before_forget,
            "删除失败后不得 restore 旧记录"
        );

        // 未授权错误同样不得携带 secret。
        match auth.terminal_token().await {
            Ok(_) => panic!("忘记后不得再取得终端 token"),
            Err(err) => {
                capture_status(&mut captured_output, &auth.status());
                captured_output.push_str(&format!("{err} {err:?}"));
                assert_secret_free(&err);
            }
        }

        // 捕获到的状态/错误/日志输出不得包含任何合成 JWT。
        for jwt in &issued_jwts {
            assert!(
                !captured_output.contains(jwt.as_str()),
                "状态/错误输出不得包含合成 JWT：{captured_output}"
            );
        }
        assert!(!captured_output.contains("synthetic-signature"));
        assert!(!captured_output.contains("Bearer"));
    }

    // ───────────────── P2-1：忘记的删除义务不因新授权而取消 ─────────────────

    /// P2-1 a：慢保存在途 → 忘记（删除排队）→ 新授权在开始前失败。
    ///
    /// 旧实现让删除跟随 epoch：新授权一旦推进 epoch，忘记排队的删除就被跳过，旧记录
    /// 留在存储里，重启即被恢复。删除义务必须独立于新授权代际，删除成功才解除。
    #[tokio::test]
    async fn forget_during_slow_save_then_failed_authorize_keeps_purge_obligation() {
        let fake = FakeZcode::start().await;
        let jwt = valid_jwt();
        fake.set_exchange(200, exchange_ok(&jwt));
        let store = Arc::new(MemoryCredentialStore::new());
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));

        // 旧授权的保存停在「原生写完成前」：记录尚未落地，保存持有存储单飞锁。
        let save_gate = store.gate_save();
        let slot = UrlSlot::new();
        let authorize = spawn_authorize(auth.clone(), OAuthProvider::Bigmodel, slot.clone());
        let (redirect, state) = oauth_params(&slot.wait().await);
        let delivered =
            deliver_status(&redirect, &format!("state={state}&code={SYNTHETIC_CODE}")).await;
        assert_eq!(delivered, Some(200));
        save_gate.wait_entered().await;
        assert!(store.stored().is_none(), "保存在放行前不得写入记录");

        // 忘记：清内存并登记删除义务；删除排在被挂起的保存之后。
        let forget = tokio::spawn({
            let auth = auth.clone();
            async move { auth.forget().await }
        });
        wait_until(|| !auth.status().authorized).await;
        assert_eq!(store.delete_count(), 0, "在途保存未完成前删除必须排队");

        // 新授权：删除义务未完成前不得开始 OAuth；open 直接失败让本次授权以错误收尾
        // （不等待真实浏览器回调，RED 下也不会悬挂）。
        let failed_authorize = tokio::spawn({
            let auth = auth.clone();
            async move {
                let open: OpenAuthorize = Arc::new(|_url: String| Err(ZcodeError::unknown()));
                auth.authorize(OAuthProvider::Bigmodel, open).await
            }
        });
        // 让新授权先跑到「等待删除义务」（旧实现则会在这里推进 epoch）。不用固定 sleep。
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        // 放行旧保存：写入旧记录 → 排队的删除必须真正执行（不得因 epoch 变化跳过）。
        save_gate.release();
        authorize
            .await
            .expect("旧授权任务不应 panic")
            .expect("旧授权本身成功");
        let status_after_forget = forget.await.expect("忘记任务不应 panic");
        assert!(!status_after_forget.authorized);
        assert_eq!(status_after_forget.problem, None, "删除成功不得留问题");
        assert_eq!(store.save_count(), 1, "旧保存只执行一次");
        assert_eq!(store.delete_count(), 1, "删除义务必须执行一次");
        assert!(store.stored().is_none(), "忘记的旧记录必须被删除");

        // 新授权以错误收尾，未产生任何交换。
        assert!(
            failed_authorize
                .await
                .expect("新授权任务不应 panic")
                .is_err(),
            "新授权本身失败"
        );
        assert_eq!(fake.exchange_count(), 1, "失败的授权不得发起交换");
        assert!(store.stored().is_none(), "新授权失败后不得留下旧记录");

        // 重启：全新 AuthService 对同一存储恢复，零授权（旧记录不得复活）。
        let restarted = AuthService::new(test_client()).with_credential_store(store.clone());
        let after_restart = restarted.restore().await;
        assert!(!after_restart.authorized, "重启不得恢复被忘记的旧记录");
        assert!(!after_restart.remembered);
        assert_eq!(after_restart.provider, None);
        assert_eq!(store.load_count(), 1, "重启只读一次存储");
        assert_eq!(store.max_concurrent_store_calls(), 1, "存储调用必须串行");
    }

    /// P2-1 b：删除义务未完成时新授权必须 `storage_failed`（零浏览器/零交换/零保存），
    /// 重试成功后可以继续授权。
    #[tokio::test]
    async fn forget_delete_failure_blocks_new_authorize_and_retries() {
        let fake = FakeZcode::start().await;
        fake.set_exchange(200, exchange_ok(&valid_jwt()));
        let store = Arc::new(MemoryCredentialStore::new());
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("首次授权成功");
        assert_eq!(store.save_count(), 1);

        // 忘记时删除失败：义务保留，记录可能仍在。
        store.fail_next(CredentialError::DeleteFailed);
        let status = auth.forget().await;
        assert_eq!(status.problem, Some(AuthProblem::DeleteFailed));
        assert_eq!(store.delete_count(), 1);
        assert!(store.stored().is_some(), "删除失败时记录可能仍在");

        // 新授权：删除义务未完成 → storage_failed，零浏览器、零交换、零保存。
        // 再注入一次删除失败：义务重试仍失败，授权不得越过它开始。
        store.fail_next(CredentialError::DeleteFailed);
        let open_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let open: OpenAuthorize = {
            let open_calls = open_calls.clone();
            Arc::new(move |_url: String| {
                open_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(ZcodeError::unknown())
            })
        };
        let code = err_code(auth.authorize(OAuthProvider::Bigmodel, open).await);
        assert_eq!(
            code, "storage_failed",
            "删除义务未完成必须报 storage_failed"
        );
        assert_eq!(
            open_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "删除义务未完成不得打开浏览器"
        );
        assert_eq!(fake.exchange_count(), 1, "删除义务未完成不得发起交换");
        assert_eq!(store.save_count(), 1, "删除义务未完成不得写入新记录");
        assert_eq!(store.delete_count(), 2, "authorize 必须先重试删除义务");
        assert!(store.stored().is_some(), "旧记录仍在（尚未删除）");

        // 再忘记：重试删除成功，义务解除、problem 清空、记录真正删除。
        let retried = auth.forget().await;
        assert_eq!(retried.problem, None);
        assert_eq!(store.delete_count(), 3, "删除义务必须重试");
        assert!(store.stored().is_none(), "重试成功后旧记录必须删除");

        // 之后的新授权可以正常继续（交换 + 保存 + 恢复标记解开）。
        let next_jwt = synthetic_jwt(now_secs() + 7200);
        fake.set_exchange(200, exchange_ok(&next_jwt));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("删除义务解除后授权必须可继续");
        assert_eq!(store.save_count(), 2);
        let status = auth.status();
        assert!(status.authorized);
        assert!(status.remembered);
        assert_eq!(status.problem, None);
        assert!(store.stored().expect("新记录应已保存").jwt == next_jwt);
    }

    /// P2-1 c：restore 在读取旧记录之前必须先完成待办删除；删除未完成时零读取，
    /// 成功后也不得恢复旧身份。
    #[tokio::test]
    async fn restore_retries_pending_purge_before_loading() {
        let fake = FakeZcode::start().await;
        let jwt = valid_jwt();
        fake.set_exchange(200, exchange_ok(&jwt));
        let store = Arc::new(MemoryCredentialStore::new());
        let auth = Arc::new(test_service(&fake).with_credential_store(store.clone()));
        run_authorize(&auth, OAuthProvider::Bigmodel)
            .await
            .expect("授权成功");
        assert_eq!(store.save_count(), 1);

        // 忘记时删除失败：义务保留，旧记录仍在存储里。
        store.fail_next(CredentialError::DeleteFailed);
        let status = auth.forget().await;
        assert_eq!(status.problem, Some(AuthProblem::DeleteFailed));
        assert!(store.stored().is_some());

        // 待办删除再次失败：restore 如实报 problem 且不读旧记录。
        store.fail_next(CredentialError::DeleteFailed);
        let blocked = auth.restore().await;
        assert!(!blocked.authorized);
        assert_eq!(blocked.problem, Some(AuthProblem::DeleteFailed));
        assert_eq!(store.load_count(), 0, "删除义务未完成前不得读取旧记录");
        assert_eq!(store.delete_count(), 2, "restore 必须重试待办删除");

        // 待办删除挂起：restore 停在删除里（零 load），放行后才收尾。
        let delete_gate = store.gate_delete();
        let restore = tokio::spawn({
            let auth = auth.clone();
            async move { auth.restore().await }
        });
        delete_gate.wait_entered().await;
        assert_eq!(store.load_count(), 0, "删除在途时不得读取旧记录");
        assert_eq!(store.delete_count(), 3, "restore 必须再次重试待办删除");
        delete_gate.release();
        let after = restore.await.expect("restore 任务不应 panic");
        assert!(!after.authorized, "待办删除完成后不得恢复旧记录");
        assert!(!after.remembered);
        assert_eq!(after.provider, None);
        assert_eq!(store.load_count(), 0, "恢复仍被忘记禁用，不得读取旧记录");
        assert!(store.stored().is_none(), "待办删除必须真正删除旧记录");

        // 重启：零授权。
        let restarted = AuthService::new(test_client()).with_credential_store(store.clone());
        let restarted_status = restarted.restore().await;
        assert!(!restarted_status.authorized, "重启不得恢复旧记录");
        assert!(!restarted_status.remembered);
        assert_eq!(store.load_count(), 1);
    }

    /// P2-1 复审回归：新代 JWT 只在内存（尚未保存成功）时，删除义务不得被
    /// 「内存里已有新凭据」捷径跳过——旧记录必须真正删除。
    ///
    /// 触发条件：`forget` 登记的删除用旧代 epoch 排队，新授权已把新 JWT 落内存但保存
    /// 未成功；旧实现据此把 `purge_pending` 清零且零删除，旧记录留在存储里。
    #[tokio::test]
    async fn pending_purge_deletes_old_record_with_unsaved_current_jwt() {
        let old_jwt = valid_jwt();
        let store = store_with(OAuthProvider::Zai, &old_jwt);
        let auth = Arc::new(AuthService::new(test_client()).with_credential_store(store.clone()));
        {
            let mut state = auth.lock_state();
            state.epoch = 10;
            state.jwt = Some(synthetic_jwt(now_secs() + 7200));
            state.jwt_epoch = Some(10);
            state.provider = Some(OAuthProvider::Bigmodel);
            state.remembered = false; // 新代 JWT 只在内存，保存尚未成功
            state.purge_pending = true;
        }

        let delete_gate = store.gate_delete();
        let purge = tokio::spawn({
            let auth = auth.clone();
            async move { auth.purge_pending(9).await } // 旧代排队中的删除义务
        });
        // 旧实现走 newer_credential 捷径：零删除，闸门永远等不到。
        delete_gate.wait_entered().await;
        assert_eq!(store.delete_count(), 1, "删除义务必须真正执行");
        delete_gate.release();
        purge
            .await
            .expect("purge 任务不应 panic")
            .expect("删除应成功");
        assert!(store.stored().is_none(), "旧记录必须删除");
        assert!(!auth.lock_state().purge_pending, "删除成功后义务解除");
    }

    /// P2-1 复审回归：save 失败后删除义务仍在（不得被内存 JWT 视为已完成）；随后
    /// 执行删除，重启不得恢复旧记录。
    #[tokio::test]
    async fn save_failure_keeps_purge_obligation_and_restart_never_restores() {
        let old_jwt = valid_jwt();
        let store = store_with(OAuthProvider::Zai, &old_jwt);
        let auth = Arc::new(AuthService::new(test_client()).with_credential_store(store.clone()));
        let epoch = {
            let mut state = auth.lock_state();
            state.epoch = 10;
            state.jwt = Some(synthetic_jwt(now_secs() + 7200));
            state.jwt_epoch = Some(10);
            state.provider = Some(OAuthProvider::Bigmodel);
            state.remembered = false;
            state.purge_pending = true;
            state.epoch
        };

        // 新 JWT 保存失败：不得宣称已记住，也不得把删除义务当成已完成。
        store.fail_next(CredentialError::StorageFailed);
        auth.persist_authorized(epoch, OAuthProvider::Bigmodel)
            .await;
        let status = auth.status();
        assert!(!status.remembered, "保存失败不得假报已记住");
        assert_eq!(status.problem, Some(AuthProblem::StorageFailed));
        assert!(store.stored().is_some(), "保存失败时旧记录仍在");
        assert!(
            auth.lock_state().purge_pending,
            "save 失败后删除义务必须保留"
        );

        // 义务仍为真 → 删除必须执行，旧记录消失。
        auth.purge_pending(epoch).await.expect("删除应成功");
        assert!(store.stored().is_none(), "删除义务必须真正删除旧记录");
        assert!(!auth.lock_state().purge_pending);

        // 重启：旧记录已删除，零授权。
        let restarted = AuthService::new(test_client()).with_credential_store(store.clone());
        let after_restart = restarted.restore().await;
        assert!(!after_restart.authorized, "重启不得恢复已删除的旧记录");
        assert!(!after_restart.remembered);
        assert_eq!(store.load_count(), 1);
    }

    /// P2-1 复审回归：save 成功必须在**持 store_lock 的窗口内**清删除义务——
    /// 排队的 purge 之后一定看到「已保存」，不得删除新记录。
    #[tokio::test]
    async fn save_success_clears_purge_obligation_inside_store_lock() {
        let new_jwt = synthetic_jwt(now_secs() + 7200);
        let store = store_with(OAuthProvider::Zai, &valid_jwt());
        let auth = Arc::new(AuthService::new(test_client()).with_credential_store(store.clone()));
        let epoch = {
            let mut state = auth.lock_state();
            state.epoch = 10;
            state.jwt = Some(new_jwt.clone());
            state.jwt_epoch = Some(10);
            state.provider = Some(OAuthProvider::Bigmodel);
            state.remembered = false;
            state.purge_pending = true;
            state.epoch
        };

        // 保存停在 fake 内（持有存储单飞锁）。
        let save_gate = store.gate_save();
        let persist = tokio::spawn({
            let auth = auth.clone();
            async move {
                auth.persist_authorized(epoch, OAuthProvider::Bigmodel)
                    .await
            }
        });
        save_gate.wait_entered().await;

        // 排队的 purge 必须等 save 完成；save 成功后不得删除新记录。
        let purge = tokio::spawn({
            let auth = auth.clone();
            async move { auth.purge_pending(epoch).await }
        });
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(store.delete_count(), 0, "save 在途时 purge 必须排队");
        save_gate.release();
        purge
            .await
            .expect("purge 任务不应 panic")
            .expect("save 成功后 purge 必须零删除完成");
        persist.await.expect("persist 任务不应 panic");

        assert_eq!(store.delete_count(), 0, "save 成功后不得删除新记录");
        assert!(
            store.stored().expect("新记录仍在").jwt == new_jwt,
            "持久记录必须仍是新 JWT"
        );
        let status = auth.status();
        assert!(status.remembered);
        assert_eq!(status.problem, None);
        assert!(
            !auth.lock_state().purge_pending,
            "save 成功必须解除删除义务"
        );
    }

    /// P2-1 复审 Minor：删除失败只在**同一义务代际**保留 pending/problem，
    /// 已被新授权接管时不得把已解除的义务复活成僵尸。
    ///
    /// 交错：旧代 purge 的删除停在本机 fake 内（持存储单飞锁）→ 期间新授权保存成功
    /// 的提交解锁义务（`purge_pending=false`）→ 放行删除并注入失败。旧实现无条件把
    /// `pending` 置回 true，重启/后续 authorize 会被僵尸义务永久阻塞。
    #[tokio::test]
    async fn purge_failure_never_revives_cleared_obligation() {
        let store = store_with(OAuthProvider::Zai, &valid_jwt());
        let auth = Arc::new(AuthService::new(test_client()).with_credential_store(store.clone()));
        {
            let mut state = auth.lock_state();
            state.epoch = 10;
            state.purge_pending = true;
        }

        store.fail_next(CredentialError::DeleteFailed);
        let delete_gate = store.gate_delete();
        let purge = tokio::spawn({
            let auth = auth.clone();
            async move { auth.purge_pending(9).await }
        });
        delete_gate.wait_entered().await;

        // 删除在途期间新授权保存成功（状态提交由 save 的锁内窗口完成）。
        {
            let mut state = auth.lock_state();
            state.epoch = 11;
            state.jwt = Some(synthetic_jwt(now_secs() + 7200));
            state.jwt_epoch = Some(11);
            state.remembered = true;
            state.purge_pending = false;
        }
        delete_gate.release();
        assert!(
            purge.await.expect("purge 任务不应 panic").is_err(),
            "删除本身失败"
        );
        let state = auth.lock_state();
        assert!(!state.purge_pending, "新代已清义务，旧代失败不得复活");
        assert_eq!(state.problem, None, "旧代失败不得污染新代状态");
    }
}
