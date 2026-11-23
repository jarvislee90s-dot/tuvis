//! 有界本地 HTTP 传输（计划任务 3）：本批次唯一直接与 OpenCode 本地服务说话的模块。
//!
//! 只负责「一次受控请求」和「一条可重连的事件流」，不做投影、不做动作账本、不暴露
//! 手机路由。纪律：
//!
//! * **路径只由后端拼装**：`read_json` / `write_once` 收的是绝对路径而不是 URL；
//!   绝对 URL、相对路径、控制字符（含 CR/LF）、穿越段一律在发请求前拒绝——手机不能
//!   自带 URL 或 method。
//! * **不新增运行时依赖**：复用已配置的 `reqwest` 0.12；`.no_proxy()` 关掉系统代理
//!   （`system-proxy` feature 默认会读系统代理，本机回环请求不能绕出去），
//!   `redirect::Policy::none()` 关掉重定向（Basic 头不能被搬到别的端口/主机）。
//! * **预算是本计划的有界工程预算，不是性能承诺**：读 5s、写 10s、事件流建连 5s、
//!   事件流 60s 无任何字节即重连、响应 JSON 上限 32 MiB。**写超时不重试**——此时服务端
//!   是否已收到不可知，重试会制造重复动作。
//! * **401 表示凭据代际已消失**：作废本连接（[`NativeClient::invalidate`]），由调用方
//!   安排下一次**只读**重新发现；绝不因此重放写。
//! * **代际隔离**：`read_events` 只输出产生该事件的代际 [`RuntimeKey`]，连接一旦作废
//!   就停止转发——传输层没有把陈旧代际的事件改标成当前代际的能力。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, watch};

use super::types::{codes, NativeError, RuntimeKey, ServiceDescriptor};

/// 只读调用的整体预算（连接 + 响应头 + 响应体）。
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// 写调用的整体预算。超时即失败，**不重试**。
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// 事件流建连预算。
const SSE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// 事件流静默预算：60s 内没有任何字节即重连（这是重连，不是错误）。
const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// 对端主动断开后的重连间隔：只用来避免「连上就断」打成忙等，不改变静默预算。
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// 单个原生响应体上限（32 MiB，R3）：消息读回里的附件按 base64 约 1.33× 膨胀，
/// 单条 16 MiB 附件的分页读回约 22.4 MB，旧的 4 MiB 上限会让合法附件消息永远读不回。
/// 声明长度与分块累计两条路径共用同一个值。
pub(crate) const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// 事件流端点。与路径一样是受控常量，调用方（更不是手机）不能替换。
const EVENT_PATH: &str = "/api/event";
/// OpenCode 的 HTTP Basic 用户名固定为 `opencode`，凭据是登记文件里的 password。
const BASIC_USER: &str = "opencode";
/// 全局 pending 集合端点：必须带 `location[directory]`（任务 0 回填第 6 条）。
const SCOPED_COLLECTION_PATHS: [&str; 2] = ["/api/form", "/api/permission/request"];
/// 作用域参数是 deepObject，裸 `location=` 任务 0 已证为 400。
const SCOPE_QUERY_KEY: &str = "location[directory]";

fn path_invalid() -> NativeError {
    NativeError::new(codes::OC_PATH_INVALID, "native path is not accepted")
}

fn scope_missing() -> NativeError {
    NativeError::new(
        codes::OC_SCOPE_MISSING,
        "global pending read requires location scope",
    )
}

fn transport_timeout() -> NativeError {
    NativeError::new(codes::OC_TRANSPORT_TIMEOUT, "native transport timed out")
}

pub(crate) fn transport_failed() -> NativeError {
    NativeError::new(codes::OC_TRANSPORT_FAILED, "native transport failed")
}

pub(crate) fn response_too_large() -> NativeError {
    NativeError::new(
        codes::OC_RESPONSE_TOO_LARGE,
        "native response exceeds size cap",
    )
}

fn response_invalid() -> NativeError {
    NativeError::new(
        codes::OC_RESPONSE_INVALID,
        "native response is not valid json",
    )
}

fn unauthorized() -> NativeError {
    NativeError::new(
        codes::OC_UNAUTHORIZED,
        "native service credential is no longer valid",
    )
}

fn http_status() -> NativeError {
    NativeError::new(
        codes::OC_HTTP_STATUS,
        "native service returned an unexpected status",
    )
}

fn not_loopback() -> NativeError {
    NativeError::new(
        codes::OC_ENDPOINT_NOT_LOOPBACK,
        "opencode service endpoint is not loopback",
    )
}

fn invalid_endpoint() -> NativeError {
    NativeError::new(
        codes::OC_ENDPOINT_INVALID,
        "opencode service endpoint is malformed",
    )
}

/// 一次写调用的应答。`status` **必须**由调用方检查：只有 2xx 才算服务端受理；
/// 400/409 之类的语义（缺答、已定案）留给动作层判定，传输层不代判。
#[derive(Debug, Clone, PartialEq)]
pub struct NativeWriteReply {
    pub status: u16,
    pub body: Option<Value>,
}

/// 单代际的本机原生客户端。持有凭据，**不派生** `Debug`/`Serialize`（手写 Debug 打码）。
pub struct NativeClient {
    runtime: RuntimeKey,
    endpoint: reqwest::Url,
    password: String,
    http: reqwest::Client,
    invalidated: AtomicBool,
}

impl std::fmt::Debug for NativeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeClient")
            .field("runtime", &self.runtime)
            .field("endpoint", &self.endpoint)
            .field("password", &"<redacted>")
            .field("invalidated", &self.is_invalidated())
            .finish()
    }
}

impl NativeClient {
    /// 建立一条代际内连接：只校验端点形态，不发 IO。
    pub fn new(descriptor: ServiceDescriptor) -> Result<Self, NativeError> {
        Self::with_builder(descriptor, Self::local_http_builder())
    }

    /// 创建会话时 `/api/info`、POST 和立即回读必须共用已核验的 TCP 连接。
    /// 关闭连接后即使同地址出现新监听者，也不允许这个客户端发起新连接。
    pub(crate) fn for_create(descriptor: ServiceDescriptor) -> Result<Self, NativeError> {
        let connected = std::sync::Arc::new(AtomicBool::new(false));
        let connect_once = tower::ServiceBuilder::new().filter(move |request| {
            if connected.swap(true, Ordering::SeqCst) {
                Err(std::io::Error::other(
                    "the verified OpenCode connection was closed",
                ))
            } else {
                Ok(request)
            }
        });
        Self::with_builder(
            descriptor,
            Self::local_http_builder()
                .http1_only()
                .retry(reqwest::retry::never())
                .connector_layer(connect_once),
        )
    }

    fn local_http_builder() -> reqwest::ClientBuilder {
        // 本机回环请求和凭据均不经过代理或重定向。
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
    }

    fn with_builder(
        descriptor: ServiceDescriptor,
        builder: reqwest::ClientBuilder,
    ) -> Result<Self, NativeError> {
        validate_endpoint(&descriptor.endpoint)?;
        let http = builder.build().map_err(|_| transport_failed())?;
        Ok(Self {
            runtime: descriptor.runtime,
            endpoint: descriptor.endpoint,
            password: descriptor.password,
            http,
            invalidated: AtomicBool::new(false),
        })
    }

    /// 本连接所属代际。调用方用它给读结果与事件归类，代际变化后丢弃旧结果。
    pub fn runtime(&self) -> &RuntimeKey {
        &self.runtime
    }

    /// 作废本连接：401（凭据代际已消失）或调用方已发现新代际时调用。
    /// 作废后所有调用都不再触碰网络，等待调用方以新描述符重建客户端。
    pub fn invalidate(&self) {
        self.invalidated.store(true, Ordering::SeqCst);
    }

    pub fn is_invalidated(&self) -> bool {
        self.invalidated.load(Ordering::SeqCst)
    }

    /// 受控只读调用：`path` 是后端拼装的绝对路径，`query` 是已编码好的键值对。
    pub async fn read_json(
        &self,
        path: &str,
        query: &[(String, String)],
    ) -> Result<Value, NativeError> {
        self.read_json_within(path, query, MAX_RESPONSE_BYTES).await
    }

    /// 受控只读调用（保留状态码）：echo-challenge 探针通道用。
    ///
    /// 与 [`NativeClient::read_json`] 同一套纪律（预算/路径校验/401 作废连接），差别
    /// 只在**非 2xx 不折叠**：404（探针明确删除）必须与其他状态分开，调用方据此决定
    /// 是否受控重建。2xx + 解析失败仍按 `OC_RESPONSE_INVALID` 报错（内容不可作证据）。
    pub(crate) async fn read_json_statused(
        &self,
        path: &str,
    ) -> Result<(u16, Option<Value>), NativeError> {
        self.ensure_live()?;
        let path = validate_path(path)?;
        let url = self.url_for(path, &[]);
        let request = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .basic_auth(BASIC_USER, Some(&self.password));
        let exchange =
            tokio::time::timeout(READ_TIMEOUT, self.exchange(request, MAX_RESPONSE_BYTES))
                .await
                .map_err(|_| transport_timeout())?;
        let (status, body) = exchange?;
        if !(200..300).contains(&status) {
            // 非 2xx 的内容不能当证据：原样交状态码，由调用方分诊（404=删除）。
            return Ok((status, None));
        }
        let value = serde_json::from_slice(&body)
            .map_err(|_| response_invalid())
            .map(Some)?;
        Ok((status, value))
    }

    /// 同 [`NativeClient::read_json`]，但允许调用方给出更小的响应体预算。
    ///
    /// 生产读路径只走 [`NativeClient::read_json`]（32 MiB 整包）；分页降档需要一个能在
    /// 小夹具上触发 `OC_RESPONSE_TOO_LARGE` 的 seam，这个入口只对 crate 内开放。
    pub(crate) async fn read_json_within(
        &self,
        path: &str,
        query: &[(String, String)],
        max_bytes: usize,
    ) -> Result<Value, NativeError> {
        self.ensure_live()?;
        let path = validate_path(path)?;
        Self::ensure_scope(path, query)?;
        let url = self.url_for(path, query);
        let request = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .basic_auth(BASIC_USER, Some(&self.password));

        let exchange = tokio::time::timeout(READ_TIMEOUT, self.exchange(request, max_bytes))
            .await
            .map_err(|_| transport_timeout())?;
        let (status, body) = exchange?;
        if !(200..300).contains(&status) {
            // 非 2xx 的内容不能当证据：错误状态一律不往上报成「读到了空」。
            return Err(http_status());
        }
        serde_json::from_slice(&body).map_err(|_| response_invalid())
    }

    /// 受控写调用：只提交一次。重定向不跟随，超时不重试，401 不重放。
    ///
    /// 非 2xx 也按 [`NativeWriteReply`] 原样返回，状态由调用方判定；401 例外——那说明
    /// 凭据代际已消失，连接随之作废。
    pub async fn write_once(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<NativeWriteReply, NativeError> {
        self.ensure_live()?;
        let path = validate_path(path)?;
        let url = self.url_for(path, &[]);
        let request = self
            .http
            .post(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .basic_auth(BASIC_USER, Some(&self.password))
            .json(body);
        self.send_write(request).await
    }

    /// 受控撤回调用（任务 7）：`DELETE /api/session/{id}/inbox/{item}`。
    ///
    /// 与 [`NativeClient::write_once`] 共用同一套纪律（只发一次、不重试、401 不重放），
    /// 只是方法为 DELETE。任务 0 已证：DELETE 对未知/已投递 id 都返回 **204**，
    /// 204 不是撤回证据——调用方必须回读 inbox 与消息列表才能定性。
    pub async fn delete_once(&self, path: &str) -> Result<NativeWriteReply, NativeError> {
        self.ensure_live()?;
        let path = validate_path(path)?;
        let url = self.url_for(path, &[]);
        let request = self
            .http
            .delete(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .basic_auth(BASIC_USER, Some(&self.password));
        self.send_write(request).await
    }

    /// 受控标题写（H03 echo-challenge 归属门）：`PATCH /api/session/{id}` 改会话标题。
    ///
    /// 与 [`NativeClient::write_once`] / [`NativeClient::delete_once`] 完全同一套纪律
    /// （只发一次、超时不重试、不跟随重定向、401 作废连接）。b1/H02 实证：改标题返回
    /// **204 无 body**——调用方按状态判定受理。
    pub async fn patch_once(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<NativeWriteReply, NativeError> {
        self.ensure_live()?;
        let path = validate_path(path)?;
        let url = self.url_for(path, &[]);
        let request = self
            .http
            .patch(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .basic_auth(BASIC_USER, Some(&self.password))
            .json(body);
        self.send_write(request).await
    }

    /// 发出一次写调用并读完整应答：不重试、不跟随重定向、超限即停。
    async fn send_write(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<NativeWriteReply, NativeError> {
        let exchange =
            tokio::time::timeout(WRITE_TIMEOUT, self.exchange(request, MAX_RESPONSE_BYTES))
                .await
                .map_err(|_| transport_timeout())?;
        let (status, raw) = exchange?;
        let accepted = (200..300).contains(&status);
        let body = if raw.is_empty() {
            None
        } else {
            match serde_json::from_slice(&raw) {
                Ok(value) => Some(value),
                // 2xx 的体解析不了就无法核对关联 id，必须失败；非 2xx 的体只作诊断。
                Err(_) if accepted => return Err(response_invalid()),
                Err(_) => None,
            }
        };
        Ok(NativeWriteReply { status, body })
    }

    /// 事件流：每个事件都以**产生它的代际**交给调用方；静默 60s 重连，取消即结束。
    ///
    /// 只输出 [`RuntimeKey`] 而不透传事件内容：本层只负责「有变化」的信号与代际归属，
    /// 内容一律由会话级只读端点补读。
    ///
    /// 结束语义：取消 → `Ok(())`；代际已作废 → `Err(OC_UNAUTHORIZED)`。
    pub async fn read_events(
        &self,
        mut cancel: watch::Receiver<bool>,
        tx: mpsc::Sender<RuntimeKey>,
    ) -> Result<(), NativeError> {
        self.ensure_live()?;
        // 建连时冻结代际：这条流上的每个事件都只可能标成这个代际。
        let generation = self.runtime.clone();
        loop {
            // 每一轮（含静默/断开后的**重连**）都必须重查作废守卫：作废后重新建连等于
            // 拿着旧凭据再打一次服务——同端口换了进程时，会每个静默预算再打一次，直到
            // 新服务回 401 才停。这里与入口、读、写保持同一失败语义：一律
            // `OC_UNAUTHORIZED`，由调用方安排只读重新发现，而不是在这里等待或重试。
            self.ensure_live()?;
            if *cancel.borrow() {
                return Ok(());
            }
            let url = self.url_for(EVENT_PATH, &[]);
            let request = self
                .http
                .get(url)
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .basic_auth(BASIC_USER, Some(&self.password));
            let response = tokio::select! {
                _ = cancel.changed() => return Ok(()),
                result = tokio::time::timeout(SSE_CONNECT_TIMEOUT, request.send()) => {
                    match result {
                        Ok(Ok(response)) => response,
                        Ok(Err(_)) => return Err(transport_failed()),
                        Err(_) => return Err(transport_timeout()),
                    }
                }
            };
            let status = response.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                self.invalidate();
                return Err(unauthorized());
            }
            if !status.is_success() {
                return Err(http_status());
            }

            match self.drain(response, &generation, &mut cancel, &tx).await {
                StreamEnd::Cancelled | StreamEnd::Stale => return Ok(()),
                StreamEnd::Closed => {
                    // 对端主动断开：稍等再连，避免忙等。
                    tokio::select! {
                        _ = cancel.changed() => return Ok(()),
                        _ = tokio::time::sleep(RECONNECT_DELAY) => {}
                    }
                }
                // 静默预算已用完：立即重连补读。
                StreamEnd::Idle => {}
            }
        }
    }

    /// 读一条已建立的事件流，直到取消、断开、静默超限或代际被作废。
    async fn drain(
        &self,
        mut response: reqwest::Response,
        generation: &RuntimeKey,
        cancel: &mut watch::Receiver<bool>,
        tx: &mpsc::Sender<RuntimeKey>,
    ) -> StreamEnd {
        let mut frames = SseFrames::default();
        loop {
            let chunk = tokio::select! {
                _ = cancel.changed() => return StreamEnd::Cancelled,
                result = tokio::time::timeout(SSE_IDLE_TIMEOUT, response.chunk()) => {
                    match result {
                        Ok(Ok(Some(chunk))) => chunk,
                        Ok(Ok(None)) => return StreamEnd::Closed,
                        Ok(Err(_)) => return StreamEnd::Closed,
                        Err(_) => return StreamEnd::Idle,
                    }
                }
            };
            for _ in 0..frames.push(&chunk) {
                // 代际守卫：连接作废后不再转发，陈旧事件没有机会被标成当前代际。
                let Some(key) = self.admit_event(generation) else {
                    return StreamEnd::Stale;
                };
                tokio::select! {
                    _ = cancel.changed() => return StreamEnd::Cancelled,
                    result = tx.send(key) => {
                        if result.is_err() {
                            // 接收方已关闭：本层没有任何可做的了。
                            return StreamEnd::Cancelled;
                        }
                    }
                }
            }
        }
    }

    /// 代际守卫：只允许**建立这条流时所属代际**的事件通过，且只在连接未被作废时通过。
    /// 标签只能来自产生事件的代际本身；本层没有改标的能力。
    fn admit_event(&self, generation: &RuntimeKey) -> Option<RuntimeKey> {
        (generation == &self.runtime && !self.is_invalidated()).then(|| generation.clone())
    }

    /// 作废后立即失败，不再触碰网络。
    fn ensure_live(&self) -> Result<(), NativeError> {
        if self.is_invalidated() {
            Err(unauthorized())
        } else {
            Ok(())
        }
    }

    /// 全局 pending 集合读必须带 `location[directory]`（绝对目录）：缺 scope 时服务端
    /// 默认落 HOME 并返回空列表，而空列表在该状态下不是「无待处理请求」的证据。
    /// 所以这里在**发请求之前**就拒绝，绝不让 `Ok(empty)` 冒充成功。
    fn ensure_scope(path: &str, query: &[(String, String)]) -> Result<(), NativeError> {
        if !SCOPED_COLLECTION_PATHS.contains(&path) {
            return Ok(());
        }
        let scoped = query
            .iter()
            .any(|(key, value)| key == SCOPE_QUERY_KEY && value.starts_with('/'));
        if scoped {
            Ok(())
        } else {
            Err(scope_missing())
        }
    }

    /// 用受控路径拼 URL：`set_path` 整体替换，不解析相对引用，`..`/`//` 无法逃逸。
    fn url_for(&self, path: &str, query: &[(String, String)]) -> reqwest::Url {
        let mut url = self.endpoint.clone();
        url.set_path(path);
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in query {
                pairs.append_pair(key, value);
            }
        }
        url
    }

    /// 发出一次请求并读完整响应。不重试、不跟随重定向、超限即停。
    async fn exchange(
        &self,
        request: reqwest::RequestBuilder,
        max_bytes: usize,
    ) -> Result<(u16, Vec<u8>), NativeError> {
        let response = request.send().await.map_err(|_| transport_failed())?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            self.invalidate();
            return Err(unauthorized());
        }
        let body = read_body_capped(response, max_bytes).await?;
        Ok((status.as_u16(), body))
    }
}

/// 一条事件流的结束原因。
enum StreamEnd {
    /// 调用方取消。
    Cancelled,
    /// 对端断开或读流出错：稍后重连。
    Closed,
    /// 静默预算用完：立即重连。
    Idle,
    /// 代际已作废：整条流正常结束，不再转发。
    Stale,
}

/// 响应体读取：先看声明长度，再按累计字节停下（分块响应也能在上限处失败）。
async fn read_body_capped(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, NativeError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(response_too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| transport_failed())? {
        if body.len() + chunk.len() > max_bytes {
            return Err(response_too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// 受控路径校验：必须是 `/` 开头的绝对路径，且不含任何可以改写请求语义的字符。
fn validate_path(path: &str) -> Result<&str, NativeError> {
    if !path.starts_with('/') || path.starts_with("//") {
        return Err(path_invalid());
    }
    if path
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return Err(path_invalid());
    }
    if path.contains("://") || path.contains('?') || path.contains('#') {
        return Err(path_invalid());
    }
    if path
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return Err(path_invalid());
    }
    Ok(path)
}

/// 端点必须是「本机回环 + 根路径 + 无内嵌凭据」：Basic 头只允许发到这样的端点。
fn validate_endpoint(url: &reqwest::Url) -> Result<(), NativeError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid_endpoint());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid_endpoint());
    }
    if url.query().is_some() || url.fragment().is_some() || !matches!(url.path(), "" | "/") {
        return Err(invalid_endpoint());
    }
    let Some(host) = url.host_str() else {
        return Err(invalid_endpoint());
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = match bare.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => bare.eq_ignore_ascii_case("localhost"),
    };
    if loopback {
        Ok(())
    } else {
        Err(not_loopback())
    }
}

/// SSE 帧的最小解析：本层只需要知道「这一帧是不是一个事件」。
///
/// 纯注释帧（`: keepalive`）只算心跳，不产生事件；帧内容不解析、不缓存（只负责代际
/// 标记）。空行结束一帧；帧末尾没有空行就不算完整事件（少推比错推安全）。
#[derive(Debug, Default)]
struct SseFrames {
    frame_has_event: bool,
    line_is_comment: bool,
    line_has_bytes: bool,
}

impl SseFrames {
    /// 喂入一段字节，返回其中**完整**事件的个数。
    fn push(&mut self, bytes: &[u8]) -> usize {
        let mut events = 0;
        for &byte in bytes {
            match byte {
                b'\n' => {
                    if self.line_has_bytes {
                        if !self.line_is_comment {
                            self.frame_has_event = true;
                        }
                    } else if self.frame_has_event {
                        events += 1;
                        self.frame_has_event = false;
                    }
                    self.line_is_comment = false;
                    self.line_has_bytes = false;
                }
                b'\r' => {}
                _ => {
                    if !self.line_has_bytes {
                        self.line_is_comment = byte == b':';
                    }
                    self.line_has_bytes = true;
                }
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use base64::Engine;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::{mpsc, watch, Notify};

    use super::*;

    /// 测试专用凭据：任何错误文案/日志里出现即失败。
    const PASSWORD: &str = "s3cret-credential-value";
    const SERVICE_ID: &str = "87bd7e45-27bf-490d-bced-58d79c108910";
    const STARTED_AT_MS: i64 = 1_793_289_345_000;
    /// 作废后允许的重连窗口（足够覆盖一次 RECONNECT_DELAY + 建连）。
    const RECONNECT_WINDOW: Duration = Duration::from_secs(3);
    const SSE_HEAD: &str =
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n";

    fn runtime_key(pid: u32, started_at_ms: i64) -> RuntimeKey {
        RuntimeKey {
            service_id: SERVICE_ID.to_string(),
            pid,
            started_at_ms,
        }
    }

    fn descriptor(port: u16, runtime: RuntimeKey) -> ServiceDescriptor {
        ServiceDescriptor {
            runtime,
            endpoint: reqwest::Url::parse(&format!("http://127.0.0.1:{port}")).unwrap(),
            password: PASSWORD.to_string(),
            version: "2.0.22".to_string(),
            explicit: false,
        }
    }

    /// 一次性请求记录：断言方法、目标、凭据头与请求体都以此为准（不看返回值自证）。
    #[derive(Clone, Debug, Default)]
    struct Recorded {
        method: String,
        target: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Recorded {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        }
    }

    type RequestLog = Arc<Mutex<Vec<Recorded>>>;
    type Handler = Arc<
        dyn Fn(
                usize,
                Recorded,
                TcpStream,
                RequestLog,
            ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            + Send
            + Sync,
    >;

    /// 手写回环假 peer：只解析请求头与声明长度的请求体，响应由每个用例自己写。
    ///
    /// 请求计数在**服务端**累加，因此「401 后仍只有一次 POST」「没有第二个请求到达」
    /// 这类断言证明的是线上真实发生的事，不是客户端的返回值自述。
    struct Peer {
        port: u16,
        requests: RequestLog,
        connections: Arc<AtomicUsize>,
    }

    impl Peer {
        async fn start<H, F>(handler: H) -> Self
        where
            H: Fn(usize, Recorded, TcpStream, RequestLog) -> F + Send + Sync + 'static,
            F: std::future::Future<Output = ()> + Send + 'static,
        {
            let listener = TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind peer");
            let port = listener.local_addr().expect("local addr").port();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let connections = Arc::new(AtomicUsize::new(0));
            let handler: Handler = Arc::new(move |index, recorded, stream, log| {
                Box::pin(handler(index, recorded, stream, log))
            });
            let recorded_sink = Arc::clone(&requests);
            let connection_sink = Arc::clone(&connections);
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        break;
                    };
                    let index = connection_sink.fetch_add(1, Ordering::SeqCst);
                    let Some(recorded) = read_request(&mut stream).await else {
                        continue;
                    };
                    recorded_sink.lock().unwrap().push(recorded.clone());
                    let handler = Arc::clone(&handler);
                    let log = Arc::clone(&recorded_sink);
                    tokio::spawn(async move { handler(index, recorded, stream, log).await });
                }
            });
            Self {
                port,
                requests,
                connections,
            }
        }

        fn count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }

        fn take(&self) -> Vec<Recorded> {
            self.requests.lock().unwrap().clone()
        }

        fn connections(&self) -> usize {
            self.connections.load(Ordering::SeqCst)
        }
    }

    fn head_end(buffer: &[u8]) -> Option<usize> {
        buffer.windows(4).position(|window| window == b"\r\n\r\n")
    }

    async fn read_request(stream: &mut TcpStream) -> Option<Recorded> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            if let Some(end) = head_end(&buffer) {
                let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
                let mut lines = head.split("\r\n");
                let mut request_line = lines.next()?.split_whitespace();
                let method = request_line.next()?.to_string();
                let target = request_line.next()?.to_string();
                let mut headers = Vec::new();
                let mut declared = 0usize;
                for line in lines {
                    let (name, value) = line.split_once(':')?;
                    let name = name.trim().to_ascii_lowercase();
                    let value = value.trim().to_string();
                    if name == "content-length" {
                        declared = value.parse().ok()?;
                    }
                    headers.push((name, value));
                }
                let mut body = buffer[end + 4..].to_vec();
                while body.len() < declared {
                    let read = stream.read(&mut chunk).await.ok()?;
                    if read == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..read]);
                }
                body.truncate(declared);
                return Some(Recorded {
                    method,
                    target,
                    headers,
                    body,
                });
            }
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
    }

    async fn write_raw(stream: &mut TcpStream, raw: &str) {
        let _ = stream.write_all(raw.as_bytes()).await;
        let _ = stream.flush().await;
    }

    async fn write_json(stream: &mut TcpStream, status: &str, body: &str) {
        write_raw(
            stream,
            &format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
        .await;
    }

    /// 在一条 keep-alive 连接上持续应答：假 peer 也要能处理复用连接上的后续请求。
    async fn serve_json(stream: &mut TcpStream, log: &RequestLog, status: &str, body: &str) {
        write_json(stream, status, body).await;
        while let Some(request) = read_request(stream).await {
            log.lock().unwrap().push(request);
            write_json(stream, status, body).await;
        }
    }

    /// 客户端在用例结束前不会被回收：连接保持打开，任何重放/重连都会再次到达并计数。
    async fn hold_forever() {
        std::future::pending::<()>().await
    }

    fn native_client(peer: &Peer, runtime: RuntimeKey) -> NativeClient {
        NativeClient::new(descriptor(peer.port, runtime)).expect("loopback descriptor is accepted")
    }

    /// 401 = 凭据代际消失：POST 只能发生一次，错误码必须是未授权，且连接作废。
    #[tokio::test]
    async fn unauthorized_write_is_never_replayed() {
        let peer = Peer::start(|_index, _request, mut stream, _log| async move {
            // 401 后关闭连接：任何重放都必须重新建连，从而被服务端计数抓到。
            write_raw(
                &mut stream,
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await;
            hold_forever().await;
        })
        .await;
        let client = native_client(&peer, runtime_key(60541, STARTED_AT_MS));

        let err = client
            .write_once(
                "/api/session/ses_x/prompt",
                &json!({ "id": "msg_a", "text": "hi" }),
            )
            .await
            .expect_err("401 必须失败");
        assert_eq!(err.code, codes::OC_UNAUTHORIZED);
        assert!(!err.message.contains(PASSWORD), "{}", err.message);

        // 服务端计数：重放会在这里变成 2。
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(peer.count(), 1, "401 之后不得重放写请求");
        assert!(client.is_invalidated(), "401 必须作废本连接");

        // 作废后的任何调用都不再触碰网络：等待只读重新发现。
        let again = client
            .write_once("/api/session/ses_x/prompt", &json!({ "text": "hi" }))
            .await
            .expect_err("作废后写调用必须失败");
        assert_eq!(again.code, codes::OC_UNAUTHORIZED);
        assert_eq!(peer.count(), 1, "作废后不得再发请求");
    }

    /// 302 不得被跟随：凭据只允许发给已校验的回环端点，不能借重定向搬到别的端口。
    #[tokio::test]
    async fn redirect_does_not_forward_basic_auth() {
        let sink = Peer::start(|_index, _request, mut stream, _log| async move {
            write_json(&mut stream, "200 OK", "{\"leaked\":true}").await;
            hold_forever().await;
        })
        .await;
        let target = format!("http://127.0.0.1:{}/api/leak", sink.port);
        let redirect = Peer::start(move |_index, _request, mut stream, _log| {
            let target = target.clone();
            async move {
                write_raw(
                    &mut stream,
                    &format!(
                        "HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\n\r\n"
                    ),
                )
                .await;
                hold_forever().await;
            }
        })
        .await;
        let client = native_client(&redirect, runtime_key(60541, STARTED_AT_MS));

        let reply = client
            .write_once("/api/session/ses_x/prompt", &json!({ "text": "hi" }))
            .await
            .expect("302 作为一次应答返回，而不是被跟随");
        assert_eq!(
            reply.status, 302,
            "客户端必须原样交出 302，不得跟随后返回 200"
        );
        assert_eq!(reply.body, None);

        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(redirect.count(), 1);
        assert_eq!(sink.count(), 0, "重定向目标不得收到任何请求");
        let sent = redirect.take();
        assert!(
            sent[0].header("authorization").is_some(),
            "凭据只发给可信端点"
        );
    }

    /// 全局 pending 读必须带 `location[directory]`（任务 0 回填第 6 条）：缺 scope 时
    /// 服务端会默认落 HOME 并返回空列表，空列表在该状态下不是「无待处理请求」的证据。
    #[tokio::test]
    async fn unscoped_pending_read_is_not_empty_success() {
        let peer = Peer::start(|_index, _request, mut stream, log| async move {
            // 复用连接上还会来第二个请求：假 peer 必须继续应答，否则客户端会干等。
            serve_json(&mut stream, &log, "200 OK", "[]").await;
        })
        .await;
        let client = native_client(&peer, runtime_key(60541, STARTED_AT_MS));

        let err = client
            .read_json("/api/form", &[])
            .await
            .expect_err("缺 scope 的全局 pending 读不得报成空成功");
        assert_eq!(err.code, codes::OC_SCOPE_MISSING);
        assert_eq!(peer.count(), 0, "缺 scope 时连请求都不该发出去");

        // 裸 `location=` 任务 0 已证为 400，同样不算 scope。
        let bare = vec![("location".to_string(), "/tmp/project".to_string())];
        let err = client
            .read_json("/api/form", &bare)
            .await
            .expect_err("裸 location 不算 scope");
        assert_eq!(err.code, codes::OC_SCOPE_MISSING);
        // 相对目录会默认落 HOME，同样拒绝。
        let relative = vec![("location[directory]".to_string(), "project".to_string())];
        let err = client
            .read_json("/api/form", &relative)
            .await
            .expect_err("相对目录不算 scope");
        assert_eq!(err.code, codes::OC_SCOPE_MISSING);
        assert_eq!(peer.count(), 0);

        // 会话级端点不受这条约束。
        assert_eq!(
            client
                .read_json("/api/session/ses_a/form", &[])
                .await
                .unwrap(),
            json!([])
        );
        // 带 scope 的全局读可以正常拿到空列表，且 deepObject 参数按原样编码。
        let scoped = vec![(
            "location[directory]".to_string(),
            "/tmp/project".to_string(),
        )];
        assert_eq!(
            client.read_json("/api/form", &scoped).await.unwrap(),
            json!([])
        );
        assert_eq!(peer.count(), 2);
        let recorded = peer.take();
        assert!(
            recorded[1]
                .target
                .contains("location%5Bdirectory%5D=%2Ftmp%2Fproject"),
            "deepObject 参数编码不对：{}",
            recorded[1].target
        );
    }

    /// Ruling F2：任务 3 还没有 view/发布层，本用例只证计划要的那件事——
    /// 旧代际产生的事件带着**自己的** `RuntimeKey`，并在到达调用方之前被代际守卫丢弃；
    /// 传输层没有把陈旧事件标成当前代际的能力。
    #[tokio::test]
    async fn old_runtime_event_cannot_publish_new_view() {
        let release = Arc::new(Notify::new());
        let gate = Arc::clone(&release);
        let peer = Peer::start(move |_index, _request, mut stream, _log| {
            let gate = Arc::clone(&gate);
            async move {
                write_raw(&mut stream, SSE_HEAD).await;
                write_raw(&mut stream, "event: session.updated\ndata: {\"n\":1}\n\n").await;
                gate.notified().await;
                write_raw(&mut stream, "event: session.updated\ndata: {\"n\":2}\n\n").await;
                hold_forever().await;
            }
        })
        .await;

        let old_runtime = runtime_key(60541, STARTED_AT_MS);
        let new_runtime = runtime_key(69541, STARTED_AT_MS + 60_000);
        let old = Arc::new(native_client(&peer, old_runtime.clone()));
        let current = native_client(&peer, new_runtime.clone());

        let (_cancel, cancel_rx) = watch::channel(false);
        let (tx, mut rx) = mpsc::channel(8);
        let handle = {
            let old = Arc::clone(&old);
            tokio::spawn(async move { old.read_events(cancel_rx, tx).await })
        };

        let first = rx.recv().await.expect("首个事件");
        assert_eq!(first, old_runtime, "事件必须带产生它的代际");
        assert_ne!(first, *current.runtime(), "旧代际不得与当前代际混同");

        // 调用方重新发现到新代际：旧连接作废，它的事件不得再到达调用方。
        old.invalidate();
        release.notify_one();
        assert_eq!(
            rx.recv().await,
            None,
            "陈旧代际的事件必须被守卫丢弃，不能改标成当前代际"
        );
        assert!(handle.await.expect("join").is_ok(), "作废是正常结束");
        assert_eq!(*current.runtime(), new_runtime);
    }

    /// 慢响应必须按读预算失败，而不是无限等；读也不重试。
    #[tokio::test(start_paused = true)]
    async fn slow_read_is_bounded_by_read_budget() {
        let peer = Peer::start(|_index, _request, _stream, _log| async move {
            hold_forever().await;
        })
        .await;
        let client = native_client(&peer, runtime_key(60541, STARTED_AT_MS));

        let started = tokio::time::Instant::now();
        let err = client
            .read_json("/api/info", &[])
            .await
            .expect_err("慢响应必须超时");
        let elapsed = started.elapsed();
        assert_eq!(err.code, codes::OC_TRANSPORT_TIMEOUT);
        assert!(elapsed >= READ_TIMEOUT, "读预算不得短于 {READ_TIMEOUT:?}");
        assert!(
            elapsed < WRITE_TIMEOUT,
            "读调用用的是读预算而不是写预算：{elapsed:?}"
        );
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(
            peer.connections() <= 1,
            "读超时不得重试：{} 次连接",
            peer.connections()
        );
    }

    /// 写超时即失败且永不重试：此时服务端是否已收到不可知，重试会制造重复。
    #[tokio::test(start_paused = true)]
    async fn slow_write_is_bounded_and_never_retried() {
        let peer = Peer::start(|_index, _request, _stream, _log| async move {
            hold_forever().await;
        })
        .await;
        let client = native_client(&peer, runtime_key(60541, STARTED_AT_MS));

        let started = tokio::time::Instant::now();
        let err = client
            .write_once("/api/session/ses_x/prompt", &json!({ "text": "hi" }))
            .await
            .expect_err("慢写必须超时");
        let elapsed = started.elapsed();
        assert_eq!(err.code, codes::OC_TRANSPORT_TIMEOUT);
        assert!(elapsed >= WRITE_TIMEOUT, "写预算不得短于 {WRITE_TIMEOUT:?}");
        assert!(elapsed < WRITE_TIMEOUT * 2, "写预算不得被放大：{elapsed:?}");
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(
            peer.connections() <= 1,
            "写超时不得重试：{} 次连接",
            peer.connections()
        );
    }

    /// R3：整包预算必须是 32 MiB——声明长度与分块（无 Content-Length）两条路径一致；
    /// 超限在读完之前就拒绝，不能先把整个体收进内存再说。
    #[tokio::test]
    async fn response_budget_is_32_mib() {
        assert_eq!(
            MAX_RESPONSE_BYTES,
            32 * 1024 * 1024,
            "R3：原生响应整包上限 32 MiB（4 MiB 会截断合法附件消息）"
        );
        let declared = Peer::start(|_index, _request, mut stream, _log| async move {
            write_raw(
                &mut stream,
                &format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                    MAX_RESPONSE_BYTES + 1
                ),
            )
            .await;
            hold_forever().await;
        })
        .await;
        let client = native_client(&declared, runtime_key(60541, STARTED_AT_MS));
        let err = client
            .read_json("/api/info", &[])
            .await
            .expect_err("声明超限必须拒绝");
        assert_eq!(err.code, codes::OC_RESPONSE_TOO_LARGE);

        let streamed = Peer::start(|_index, _request, mut stream, _log| async move {
            write_raw(
                &mut stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .await;
            let block = vec![b'x'; 64 * 1024];
            let mut remaining = MAX_RESPONSE_BYTES + 1;
            while remaining > 0 {
                let size = remaining.min(block.len());
                let head = format!("{size:x}\r\n");
                if stream.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                if stream.write_all(&block[..size]).await.is_err() {
                    return;
                }
                if stream.write_all(b"\r\n").await.is_err() {
                    return;
                }
                remaining -= size;
            }
            let _ = stream.write_all(b"0\r\n\r\n").await;
            hold_forever().await;
        })
        .await;
        let client = native_client(&streamed, runtime_key(60541, STARTED_AT_MS));
        let err = client
            .read_json("/api/info", &[])
            .await
            .expect_err("分块超限必须拒绝");
        assert_eq!(err.code, codes::OC_RESPONSE_TOO_LARGE);
    }

    /// 构造恰好 `len` 字节的合法 JSON 体（`{"pad":"..."}`）：预算边界要能按字节断言。
    fn json_body_of_len(len: usize) -> String {
        let (prefix, suffix) = ("{\"pad\":\"", "\"}");
        assert!(
            len >= prefix.len() + suffix.len(),
            "预算太小造不出合法 JSON"
        );
        format!(
            "{prefix}{}{suffix}",
            "x".repeat(len - prefix.len() - suffix.len())
        )
    }

    /// 声明 Content-Length 的假 peer：固定回一个 JSON 体后保持连接。
    async fn declared_peer(body: String) -> Peer {
        Peer::start(move |_index, _request, mut stream, _log| {
            let body = body.clone();
            async move {
                write_json(&mut stream, "200 OK", &body).await;
                hold_forever().await;
            }
        })
        .await
    }

    /// 分块（无 Content-Length）的假 peer：固定回一个 JSON 体后保持连接。
    async fn chunked_peer(body: String) -> Peer {
        Peer::start(move |_index, _request, mut stream, _log| {
            let body = body.clone();
            async move {
                write_raw(
                    &mut stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
                )
                .await;
                let head = format!("{:x}\r\n", body.len());
                if stream.write_all(head.as_bytes()).await.is_err()
                    || stream.write_all(body.as_bytes()).await.is_err()
                    || stream.write_all(b"\r\n0\r\n\r\n").await.is_err()
                {
                    return;
                }
                hold_forever().await;
            }
        })
        .await
    }

    /// R3 预算边界必须是**包含**的：响应体恰好等于预算要接受，多 1 字节才拒绝；
    /// 声明长度与分块两路同一口径（防 32 MiB 边界 off-by-one）。
    #[tokio::test]
    async fn response_budget_boundary_is_inclusive() {
        const BUDGET: usize = 128;
        let exact = json_body_of_len(BUDGET);
        let over = json_body_of_len(BUDGET + 1);
        let fill_len = BUDGET - "{\"pad\":\"".len() - "\"}".len();

        for peer in [
            declared_peer(exact.clone()).await,
            chunked_peer(exact.clone()).await,
        ] {
            let client = native_client(&peer, runtime_key(60541, STARTED_AT_MS));
            let value = client
                .read_json_within("/api/info", &[], BUDGET)
                .await
                .expect("响应体恰好等于预算必须接受");
            assert_eq!(value["pad"].as_str().map(str::len), Some(fill_len));
        }

        for peer in [declared_peer(over.clone()).await, chunked_peer(over).await] {
            let client = native_client(&peer, runtime_key(60541, STARTED_AT_MS));
            let err = client
                .read_json_within("/api/info", &[], BUDGET)
                .await
                .expect_err("响应体比预算多 1 字节必须拒绝");
            assert_eq!(err.code, codes::OC_RESPONSE_TOO_LARGE);
        }
    }

    /// 原生路径只由后端拼装：绝对 URL、相对路径、控制字符、穿越段一律在发请求前拒绝。
    #[tokio::test]
    async fn native_paths_never_accept_urls_or_control_characters() {
        let peer = Peer::start(|_index, _request, mut stream, _log| async move {
            write_json(&mut stream, "200 OK", "{}").await;
            hold_forever().await;
        })
        .await;
        let client = native_client(&peer, runtime_key(60541, STARTED_AT_MS));

        let evil = [
            format!("http://127.0.0.1:{}/api/info", peer.port),
            "https://example.com/api/info".to_string(),
            "api/info".to_string(),
            "//example.com/api/info".to_string(),
            "/api/info\r\nX-Injected: 1".to_string(),
            "/api/info\nX-Injected: 1".to_string(),
            "/api/\u{7}info".to_string(),
            "/api/../info".to_string(),
            "/api/in fo".to_string(),
            "/api/info?x=1".to_string(),
            String::new(),
        ];
        for path in evil {
            let err = client
                .read_json(&path, &[])
                .await
                .expect_err("非法路径必须拒绝");
            assert_eq!(err.code, codes::OC_PATH_INVALID, "path={path:?}");
            assert!(!err.message.contains(PASSWORD), "{}", err.message);
            let err = client
                .write_once(&path, &json!({}))
                .await
                .expect_err("非法路径必须拒绝");
            assert_eq!(err.code, codes::OC_PATH_INVALID, "path={path:?}");
        }
        assert_eq!(peer.count(), 0, "非法路径不得产生任何请求");
    }

    /// Basic 凭据只发给已校验的回环端点：非回环描述符在构造时即拒绝。
    #[tokio::test]
    async fn client_refuses_non_loopback_endpoint() {
        for url in [
            "http://10.0.0.5:49617",
            "http://192.168.1.20:49617",
            "https://example.com:49617",
            "http://127.0.0.1:49617/nested",
            "http://127.0.0.1:49617/?x=1",
        ] {
            let descriptor = ServiceDescriptor {
                runtime: runtime_key(60541, STARTED_AT_MS),
                endpoint: reqwest::Url::parse(url).unwrap(),
                password: PASSWORD.to_string(),
                version: "2.0.22".to_string(),
                explicit: false,
            };
            let err = NativeClient::new(descriptor).expect_err("非法端点必须拒绝");
            let expected = if url.starts_with("http://127.0.0.1") {
                codes::OC_ENDPOINT_INVALID
            } else {
                codes::OC_ENDPOINT_NOT_LOOPBACK
            };
            assert_eq!(err.code, expected, "url={url}");
            assert!(!err.message.contains(PASSWORD), "{}", err.message);
        }
    }

    /// 事件流建连也受 5s 预算约束：连上但不回头不能无限等。
    #[tokio::test(start_paused = true)]
    async fn event_stream_connect_is_bounded() {
        let peer = Peer::start(|_index, _request, _stream, _log| async move {
            hold_forever().await;
        })
        .await;
        let client = native_client(&peer, runtime_key(60541, STARTED_AT_MS));
        let (_cancel, cancel_rx) = watch::channel(false);
        let (tx, _rx) = mpsc::channel(8);

        let started = tokio::time::Instant::now();
        let err = client
            .read_events(cancel_rx, tx)
            .await
            .expect_err("建连超时必须失败");
        let elapsed = started.elapsed();
        assert_eq!(err.code, codes::OC_TRANSPORT_TIMEOUT);
        assert!(elapsed >= SSE_CONNECT_TIMEOUT, "建连预算不得更短");
        assert!(
            elapsed < SSE_IDLE_TIMEOUT,
            "建连用的是建连预算而不是静默预算：{elapsed:?}"
        );
    }

    /// 60s 内没有任何字节即重连；重连后的事件带同一代际；注释帧只是心跳，不是事件。
    ///
    /// 时钟只在「静默」这一段被暂停并推进一个静默预算：真实建连的握手不能与暂停时钟
    /// 赛跑（否则用例测的是调度而不是预算）。
    #[tokio::test]
    async fn silent_event_stream_reconnects_after_idle_budget() {
        let peer = Peer::start(|_index, _request, mut stream, _log| async move {
            write_raw(&mut stream, SSE_HEAD).await;
            write_raw(&mut stream, ": keepalive\n\n").await;
            write_raw(&mut stream, "data: {\"n\":1}\n\n").await;
            hold_forever().await;
        })
        .await;
        let expected = runtime_key(60541, STARTED_AT_MS);
        let client = Arc::new(native_client(&peer, expected.clone()));
        let (cancel, cancel_rx) = watch::channel(false);
        let (tx, mut rx) = mpsc::channel(8);
        let handle = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.read_events(cancel_rx, tx).await })
        };

        assert_eq!(rx.recv().await, Some(expected.clone()), "注释帧不算事件");
        assert_eq!(peer.connections(), 1, "静默预算未用完前不得重连");

        // 恰好推进一个静默预算：这时才允许重连。
        tokio::time::pause();
        tokio::time::advance(SSE_IDLE_TIMEOUT).await;
        tokio::time::resume();

        assert_eq!(
            rx.recv().await,
            Some(expected.clone()),
            "静默 60s 后必须重连并补上新事件"
        );
        assert!(peer.connections() >= 2, "必须重新建立事件流连接");

        cancel.send(true).expect("cancel");
        assert!(handle.await.expect("join").is_ok());
    }

    /// 取消即时生效：不等静默预算，也不留后台任务。
    #[tokio::test]
    async fn read_events_stops_when_cancelled() {
        let peer = Peer::start(|_index, _request, mut stream, _log| async move {
            write_raw(&mut stream, SSE_HEAD).await;
            write_raw(&mut stream, "data: {}\n\n").await;
            hold_forever().await;
        })
        .await;
        let client = Arc::new(native_client(&peer, runtime_key(60541, STARTED_AT_MS)));
        let (cancel, cancel_rx) = watch::channel(false);
        let (tx, mut rx) = mpsc::channel(8);
        let handle = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.read_events(cancel_rx, tx).await })
        };

        assert!(rx.recv().await.is_some());
        cancel.send(true).expect("cancel");
        let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("取消必须及时结束")
            .expect("join");
        assert!(outcome.is_ok());
        assert_eq!(rx.recv().await, None);
    }

    /// 作废的连接**不得**再建立新连接：否则旧代际的凭据会在每个静默预算（或每次对端
    /// 断开）后再打一次服务；同端口换进程时一直打到新服务回 401 才停。
    #[tokio::test]
    async fn invalidated_stream_stops_reconnecting() {
        let release = Arc::new(Notify::new());
        let gate = Arc::clone(&release);
        let peer = Peer::start(move |_index, _request, mut stream, _log| {
            let gate = Arc::clone(&gate);
            async move {
                write_raw(&mut stream, SSE_HEAD).await;
                write_raw(&mut stream, "data: {\"n\":1}\n\n").await;
                // 由测试决定何时断开：先确保事件已送达，再让「对端关闭」触发重连路径。
                gate.notified().await;
            }
        })
        .await;
        let expected = runtime_key(60541, STARTED_AT_MS);
        let client = Arc::new(native_client(&peer, expected.clone()));
        let (_cancel, cancel_rx) = watch::channel(false);
        let (tx, mut rx) = mpsc::channel(8);
        let handle = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.read_events(cancel_rx, tx).await })
        };

        assert_eq!(rx.recv().await, Some(expected), "首个事件");
        assert_eq!(peer.connections(), 1, "只有一条事件流连接");

        // 调用方重新发现到新代际：作废本连接，然后让对端断开，把客户端逼上重连路径。
        client.invalidate();
        release.notify_one();

        let outcome = tokio::time::timeout(RECONNECT_WINDOW, handle)
            .await
            .expect("作废后事件流不得再重连（必须在窗口内结束）")
            .expect("join");
        // 连接数先断言：作废后哪怕只重连一次，旧凭据也已经又发出去了一遍。
        assert_eq!(peer.connections(), 1, "作废后不得再用旧凭据建立新连接");
        let err = outcome.expect_err("作废必须以未授权结束");
        assert_eq!(err.code, codes::OC_UNAUTHORIZED);
    }

    /// 写应答原样交出状态与请求体往返；凭据以 Basic 发往回环端点。
    #[tokio::test]
    async fn write_reply_carries_status_and_body() {
        let peer = Peer::start(|_index, _request, mut stream, _log| async move {
            write_json(&mut stream, "200 OK", "{\"id\":\"msg_a\"}").await;
            hold_forever().await;
        })
        .await;
        let client = native_client(&peer, runtime_key(60541, STARTED_AT_MS));

        let reply = client
            .write_once(
                "/api/session/ses_a/prompt",
                &json!({ "id": "msg_a", "text": "hi" }),
            )
            .await
            .expect("200 必须成功");
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, Some(json!({ "id": "msg_a" })));

        let recorded = peer.take();
        assert_eq!(recorded[0].method, "POST");
        assert_eq!(recorded[0].target, "/api/session/ses_a/prompt");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&recorded[0].body).unwrap(),
            json!({ "id": "msg_a", "text": "hi" })
        );
        let authorization = recorded[0].header("authorization").expect("必须有凭据头");
        let encoded = authorization.strip_prefix("Basic ").expect("必须是 Basic");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("base64");
        assert_eq!(decoded, format!("opencode:{PASSWORD}").into_bytes());

        // 凭据永不进错误文案。
        let err = client.read_json("not-a-path", &[]).await.unwrap_err();
        assert!(!err.message.contains(PASSWORD), "{}", err.message);
        assert!(!format!("{client:?}").contains(PASSWORD), "Debug 泄露凭据");
    }
}
