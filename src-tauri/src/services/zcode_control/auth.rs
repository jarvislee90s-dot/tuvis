//! ZCode 官方 OAuth 授权与内存凭据（生产计划 T1.1）。
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
//! - 账号 JWT 只在进程内存；不写 settings/DB/keychain/日志/文件，不 Serialize/Debug，
//!   也不经 [`AuthService::authorize`] 上抛。唯一外出的凭据是短期 relay token
//!   （[`TerminalToken`] 不实现 Debug/Serialize）。
//! - 错误只含冻结码；`reqwest` 原始错误（可能携 URL／认证头）一律丢弃，不挂 cause。
//! - 回环监听只 bind `127.0.0.1:0`；state 精确一次消费；错误/缺失 state 既不消费也不交换；
//!   回调响应为静态文案、不回显任何参数；[`AUTHORIZE_TIMEOUT`] 到点关闭监听
//!   （120s 是 **Tuvis 新策略**，不冒称官方 TTL）。
//! - [`AuthService::clear`] 经授权 epoch + 取消通知让在途授权立即失效：clear 之后的迟到
//!   回调不产生任何交换，交换已完成但 epoch 已变的 JWT 不落内存（review focus 1）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

use super::types::{ZResult, ZcodeError};

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

/// 回调响应的静态文案（绝不回显任何 query 参数值）。
const CALLBACK_PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>Tuvis</title>\
<p>Tuvis received the sign-in callback. You can close this tab and return to Tuvis.</p>";

/// OAuth 授权通道（对应官方 `WebOAuthProviderId` 两值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// 官方 OAuth 授权服务：持有回环监听参数与**唯一一份**账号 JWT（进程内存）。
///
/// 生产构造只有 [`AuthService::new`]（固定官方 origin，无 settings/IPC 入口）；
/// 端点注入仅测试可见。
pub struct AuthService {
    http: reqwest::Client,
    zcode_origin: String,
    /// 测试专用授权 origin 覆盖（生产恒为 `None`，即固定官方 origin）。
    authorize_origin_override: Option<String>,
    callback_timeout: Duration,
    state: Mutex<AuthState>,
}

#[derive(Default)]
struct AuthState {
    /// 账号 JWT：只在内存，`clear`/失效即丢弃。
    jwt: Option<String>,
    /// 授权 epoch：clear/新授权递增，旧 epoch 的结果一律作废。
    epoch: u64,
    /// 在途授权的取消发送端（clear/新授权触发）。
    cancel: Option<oneshot::Sender<()>>,
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
            state: Mutex::new(AuthState::default()),
        }
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
            state: Mutex::new(AuthState::default()),
        }
    }

    /// 测试专用回调等待上限（120s 生产策略不缩短；测试不留悬挂任务）。
    #[cfg(test)]
    fn with_callback_timeout(mut self, timeout: Duration) -> Self {
        self.callback_timeout = timeout;
        self
    }

    /// 走一次官方授权：打开浏览器 → 等待回环回调 → 用授权码换账号 JWT（只入内存）。
    ///
    /// 失败一律以冻结码收尾（超时/被取消/被拒 → `needs_auth`，网络不可达 → `offline`，
    /// 响应形态异常 → `unknown`）；错误不含 URL/state/code/JWT。
    pub async fn authorize(&self, provider: OAuthProvider, open: OpenAuthorize) -> ZResult<()> {
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
        self.commit_jwt(epoch, jwt)
    }

    /// 以内存中的账号 JWT 现取一次性终端 token（逐连接新取，不缓存、用后即弃）。
    ///
    /// 已过期 JWT 在任何 HTTP 请求之前返回 `needs_auth`；401 清空凭据并返回
    /// `needs_auth`（不缓存、不重试、不携带服务端正文）。
    pub async fn terminal_token(&self) -> ZResult<TerminalToken> {
        let (jwt, epoch) = {
            let state = self.lock_state();
            match state.jwt.clone() {
                Some(jwt) => (jwt, state.epoch),
                None => return Err(ZcodeError::needs_auth()),
            }
        };
        if jwt_is_expired(&jwt, now_secs()) {
            self.invalidate(epoch);
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
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            self.invalidate(epoch);
            return Err(ZcodeError::needs_auth());
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            return Err(ZcodeError::needs_auth());
        }
        if !status.is_success() {
            return Err(ZcodeError::offline());
        }
        let value: serde_json::Value = response.json().await.map_err(|_| ZcodeError::unknown())?;
        relay_payload(&value)
    }

    /// 清除账号凭据并使在途授权失效（退出/登出/401/保存新链接时调用）。
    pub fn clear(&self) {
        let mut state = self.lock_state();
        state.jwt = None;
        state.epoch = state.epoch.wrapping_add(1);
        if let Some(cancel) = state.cancel.take() {
            let _ = cancel.send(());
        }
    }

    /// 生产授权 origin：测试覆盖优先，否则固定官方常量（无配置/IPC 入口）。
    fn authorize_origin(&self, provider: OAuthProvider) -> &str {
        self.authorize_origin_override
            .as_deref()
            .unwrap_or_else(|| provider.default_authorize_origin())
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, AuthState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 开始一次授权：epoch 递增（旧 epoch 的一切结果作废），并取消上一笔在途授权。
    fn begin_authorization(&self, cancel: oneshot::Sender<()>) -> u64 {
        let mut state = self.lock_state();
        state.epoch = state.epoch.wrapping_add(1);
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
    fn commit_jwt(&self, epoch: u64, jwt: String) -> ZResult<()> {
        let mut state = self.lock_state();
        if state.epoch != epoch {
            return Err(ZcodeError::needs_auth());
        }
        state.jwt = Some(jwt);
        Ok(())
    }

    /// 凭据失效（过期/401）：只清仍属本 epoch 的凭据与在途授权。
    fn invalidate(&self, epoch: u64) {
        let mut state = self.lock_state();
        if state.epoch == epoch {
            state.jwt = None;
            state.epoch = state.epoch.wrapping_add(1);
            if let Some(cancel) = state.cancel.take() {
                let _ = cancel.send(());
            }
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

/// 按通道构造官方授权 URL（BigModel 与 ZAI 的**参数名不同**）：
/// - BigModel：`{origin}/login?redirect&appId=zcode&state`（无 response_type/client_id）
/// - ZAI：`{origin}/api/oauth/authorize?redirect_uri&response_type=code&client_id&state`
pub fn build_authorize_url(
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
        respond(&mut stream, 400).await;
        return;
    };
    let Ok(parsed) = reqwest::Url::parse(&format!("http://127.0.0.1{target}")) else {
        respond(&mut stream, 400).await;
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
        respond(&mut stream, 400).await;
        return;
    }
    let code = value("code").or_else(|| value("authCode"));
    let error = value("error");
    if code.is_none() && error.is_none() {
        // state 匹配但既无 code 也无 error：不消费，继续等待
        respond(&mut stream, 400).await;
        return;
    }
    // state 精确一次消费：重复回调一律作废，不产生第二次交换
    if consumed.swap(true, Ordering::SeqCst) {
        respond(&mut stream, 410).await;
        return;
    }
    let outcome = match code {
        Some(code) => CallbackOutcome::Code(code),
        None => CallbackOutcome::Rejected,
    };
    let _ = outcomes.send(outcome).await;
    respond(&mut stream, 200).await;
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

/// 静态回调响应：绝不回显任何 query 参数值（只区分状态码）。
async fn respond(stream: &mut TcpStream, status: u16) {
    let reason = match status {
        200 => "OK",
        410 => "Gone",
        _ => "Bad Request",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        CALLBACK_PAGE.len()
    );
    let write = async {
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream.write_all(CALLBACK_PAGE.as_bytes()).await;
        let _ = stream.flush().await;
    };
    let _ = tokio::time::timeout(CALLBACK_READ_TIMEOUT, write).await;
}

#[cfg(test)]
mod tests {
    use super::super::types::codes;
    use super::*;

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

    #[derive(Clone)]
    struct FakeConfig {
        exchange_status: u16,
        exchange_body: String,
        exchange_delay: Duration,
        relay_status: u16,
        relay_body: String,
        relay_delay: Duration,
    }

    impl Default for FakeConfig {
        fn default() -> Self {
            Self {
                exchange_status: 200,
                exchange_body: exchange_ok(&valid_jwt()),
                exchange_delay: Duration::ZERO,
                relay_status: 200,
                relay_body: relay_ok(SYNTHETIC_RELAY_TOKEN, 60),
                relay_delay: Duration::ZERO,
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
        let (status, delay, response_body) = {
            let config = config.lock().unwrap();
            if path.starts_with(OAUTH_TOKEN_PATH) {
                (
                    config.exchange_status,
                    config.exchange_delay,
                    config.exchange_body.clone(),
                )
            } else if path.starts_with(RELAY_TOKEN_PATH) {
                (
                    config.relay_status,
                    config.relay_delay,
                    config.relay_body.clone(),
                )
            } else {
                (404, Duration::ZERO, "{}".to_string())
            }
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let response = format!(
            "HTTP/1.1 {status} FAKE\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
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
            assert_eq!(body["code"], SYNTHETIC_CODE);
            assert_eq!(body["state"], state);
            assert_eq!(body["redirect_uri"], redirect);

            let token = auth.terminal_token().await.expect("终端 token 申请成功");
            // 返回值是短期 relay token（不是账号 JWT），逐次现取
            assert_eq!(token.value, SYNTHETIC_RELAY_TOKEN);
            assert_eq!(token.expires_in, 60);
            let relay = fake.last_relay().expect("已记录终端 token 请求");
            assert_eq!(relay.method, "POST");
            assert_eq!(relay.path, RELAY_TOKEN_PATH);
            assert_eq!(relay.authorization, Some(format!("Bearer {jwt}")));
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
}
