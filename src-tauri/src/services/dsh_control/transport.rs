//! T07：官方 HTTP/Mux 协议与连接生命周期（计划 §2.3 `transport::connect` 生产入口）。
//!
//! ## 协议事实（E2/E3 探测报告，`docs/engineering/evidence/dsh-native-rpc/received/`）
//!
//! * 一元 RPC：`POST /api/{ns}/{method}` + owner cookie；响应 = JSON 信封
//!   `{type:"server-response",rpcId,result}`（T01 [`codec::decode_response`] 身份闸）
//!   **或二进制**（按 content-type 分流，不硬解）；流式方法走一元通道 → 宿主
//!   typed failure `gateway/signature-invalid`（macos 报告 §4.1 实测），mux 帧形态
//!   落在一元通道 → 客户端侧拒绝（[`codec::is_stream_signature_shape`]）。
//! * Mux：`ws://…/api/remote.mux`——一 Peer **一条归属 socket**；上行帧
//!   `{type:"open",streamId,endpoint,payload:{args}}` / `{type:"cancel",streamId}`；
//!   下行帧 `{type:"item",streamId,value}` / `{type:"end",streamId}` / 错误帧
//!   `{"type":"error","streamId",error:{code,message}}`（windows 报告 §3 实测键形）；
//!   open `$events` 后首个 item 为 `{type:"ready",clientId,host}`（macos 报告
//!   §3.2 步骤 4）。**`end` 帧完整键形未在报告钉死 → G1 夹具复核项**（本实现按
//!   `{type,end,streamId}` 接收，兼容携带 value）。
//! * 生命周期：`/connect?token&interactions` → 302 + 签名 API cookie（[`redeem_launch_token`]
//!   显式处理；G1 前 cookie 由宿主内插件经本机 IPC 交付，transport 不重认证）；
//!   重复关联 socket → 409；socket 关闭即移除 Peer，此后该 cookie 的 HTTP/升级
//!   一律 401（终态）。
//!
//! ## 安全硬约束（计划全局约束）
//!
//! * HTTP 客户端 `.no_proxy()`——DSH loopback **绝不经系统代理**（crate 的
//!   reqwest 全局带 `system-proxy` feature，客户端实例必须显式覆盖）；WS 客户端
//!   （tokio-tungstenite）本身直连 TCP，无代理路径；
//! * `redirect::none`：不盲随任何 3xx——`/connect` 的 302 显式取 cookie/location；
//!   其余重定向 = `DSH_REDIRECT_BLOCKED`（外部命中数由测试边界观测，恒 0）；
//! * cookie 只发往钉定 authority 的 URL/升级目标（结构性保证：URL 只由
//!   [`AuthorizedPeer::authority`] 拼出，调用方无法指定其他目标）；
//! * cookie/token 不入任何日志与错误 detail（reqwest 错误一律 `without_url()`）。
//!
//! ## 生命周期与限界
//!
//! * 401/409 = 终态（不自动重连/重试）；读侧恢复退避 [`recovery_backoff`]
//!   500ms 起指数翻倍、10 秒封顶；单次一元请求 10 秒超时，写超时 =
//!   `DSH_WRITE_TIMEOUT`（送达未知，映射上游 `ActionState::UnknownDelivery`，
//!   不自动重投）；JSON 帧 2MiB 上限（socket 层 + 帧处理双层有界拒绝）；
//!   旧代际晚帧经 [`mirror::EventMirror`] 丢弃（计划「旧代际不污染」）。
//! * 连接关闭（显式 [`PeerConnection::close`] 或 Drop 全部句柄）：取消全部流、
//!   关 socket，reader/writer 任务有界 join（无遗留任务）。

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode as HttpStatus};
use tokio_tungstenite::tungstenite::{Error as WsError, Message as WsMessage};
use tokio_tungstenite::{connect_async_with_config, MaybeTlsStream, WebSocketStream};

use crate::services::dsh_control::auth::random_u64;
use crate::services::dsh_control::codec;
use crate::services::dsh_control::error::{codes, DshError};
use crate::services::dsh_control::mirror::{EventMirror, MirroredEvent};
use crate::services::dsh_control::model::{AuthorizedPeer, NativeResponse};

/// JSON 帧上限（计划：JSON 帧 2MiB；单一来源 = [`codec::MAX_JSON_FRAME_BYTES`]）。
pub const MAX_JSON_FRAME_BYTES: usize = codec::MAX_JSON_FRAME_BYTES;
/// 单次一元请求超时（计划：单次请求 10 秒）。
pub const UNARY_TIMEOUT: Duration = Duration::from_secs(10);
/// 只读恢复退避上限（计划：上限 10 秒）。
pub const RECOVERY_BACKOFF_CAP: Duration = Duration::from_secs(10);
/// 只读恢复退避基值（500ms 起，指数翻倍，封顶 [`RECOVERY_BACKOFF_CAP`]）。
pub const RECOVERY_BACKOFF_BASE_MS: u64 = 500;
/// 建连/升级的有界等待（宿主无响应时不悬挂；与一元超时同量级）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// `close()` 有界 join 上限：超时即 abort（连接关闭绝不悬挂）。
const CLOSE_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
/// 每条流的入站缓冲（消费方不读时只舍该流，不拖垮共享 reader）。
const STREAM_BUFFER: usize = 256;
/// writer 的控制帧通道容量（open/cancel 极小；64 足够且天然背压）。
const CONTROL_BUFFER: usize = 64;

/// 只读恢复退避（第 `attempt` 次重试前的等待）：500ms 起指数翻倍（500/1s/2s/4s/8s），
/// 10 秒封顶。显式表驱动——无位移溢出、天然单调。
pub fn recovery_backoff(attempt: u32) -> Duration {
    let cap_ms = RECOVERY_BACKOFF_CAP.as_millis() as u64;
    let ms = match attempt {
        0 => RECOVERY_BACKOFF_BASE_MS,
        1 => RECOVERY_BACKOFF_BASE_MS * 2,
        2 => RECOVERY_BACKOFF_BASE_MS * 4,
        3 => RECOVERY_BACKOFF_BASE_MS * 8,
        4 => RECOVERY_BACKOFF_BASE_MS * 16,
        _ => cap_ms,
    };
    Duration::from_millis(ms.min(cap_ms))
}

/// 传输配置（测试注入短超时；生产默认 [`TransportConfig::default`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportConfig {
    /// 单次一元请求超时。
    pub unary_timeout: Duration,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            unary_timeout: UNARY_TIMEOUT,
        }
    }
}

/// 交付产物（销项复审 P1；Important-6 修订措辞）：`request_deliver` 在
/// **交付沿**（请求体交出 HTTP 栈——hyper 取走全部请求字节进入写缓冲）后
/// 返回。此沿严格早于响应头（Hang 宿主下响应悬到超时，交付早已完成），提交
/// 许可据此在交付与响应等待之间释放。**底层 flush 时机不由本沿保证**（残余
/// 窗口见 [`PeerConnection::request_deliver`] 文档的降级申报）。响应的到达与
/// 分类经 [`DeliveredResponse::classify`]/[`classify_raw`]（**许可区外**）进行。
///
/// **为什么不是「`.send()` 应答」作交付沿**：reqwest 的 `send()` 在响应头
/// 到达时才应答——慢包/不应答的宿主会把许可拖满整个单请求超时（正是本轮
/// 要修的「关闭要等慢包」），且无法更早。请求体交出 hyper 是 reqwest/hyper
/// 下可观测的最早可靠交付沿，严格强于旧实现的「首轮 poll」近似（首轮 poll
/// 在建连 pending 时即返回，请求字节尚未交出）。
pub struct DeliveredResponse {
    /// 响应到达 future（Ok = 已读全的原始 HTTP 响应；Err = 交付沿后的传输
    /// 失败——一律按 DeliveryUnknown 分类，错误码已由 http1 层映射）。
    /// 分类时恰好 await 一次（无双轮询面）。
    response_fut: Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        crate::services::dsh_control::http1::RawUnaryResponse,
                        DshError,
                    >,
                > + Send,
        >,
    >,
    rpc_id: String,
}

impl DeliveredResponse {
    /// 等待并分类响应（**许可区外**调用）：3xx → NotExecuted（REDIRECT_
    /// BLOCKED）；401/409 → NotExecuted 终态；其余非 2xx → DeliveryUnknown；
    /// 体读/超限 → DeliveryUnknown；二进制 content-type 不是 server-response
    /// 信封 → DeliveryUnknown；流签名 → DeliveryUnknown；信封解码（含 rpcId
    /// 身份闸）→ DeliveryUnknown。
    pub async fn classify(self) -> Result<NativeResponse, UnaryFailure> {
        match self.classify_raw().await? {
            UnaryReply::Envelope(resp) => Ok(resp),
            UnaryReply::Binary { content_type, .. } => {
                Err(UnaryFailure::DeliveryUnknown(DshError::new(
                    codes::FRAME_INVALID,
                    format!("binary response ({content_type}) is not a server-response envelope"),
                )))
            }
        }
    }

    /// 原始分类（UnaryReply 消费者；`request_raw` 面）：二进制按 content-type
    /// 原样返回，不硬解成信封。
    pub async fn classify_raw(self) -> Result<UnaryReply, UnaryFailure> {
        use UnaryFailure::{DeliveryUnknown, NotExecuted};
        // 响应头等待/交付后传输失败：请求已交付（交付沿 = write_all 完成）
        // ——一律按结局未知处理（错误码由 http1 层按阶段映射）。
        let response = match self.response_fut.await {
            Ok(response) => response,
            Err(e) => return Err(DeliveryUnknown(e)),
        };
        let rpc = self.rpc_id;
        let status = response.status;
        if (300..400).contains(&status) {
            // 不盲随：任何 3xx 都是被钉定的 authority 之外（或未预期的形态）。
            // 3xx **通常**由路由层产生（处理器未运行）——此为工程假定而非协议
            // 保证（Min1：未见宿主实测反例；若未来取证推翻，应改判 DeliveryUnknown）。
            let location = response.location.clone().unwrap_or_default();
            return Err(NotExecuted(DshError::new(
                codes::REDIRECT_BLOCKED,
                format!("unary got HTTP {status} redirect to `{location}`; authority is pinned"),
            )));
        }
        match status {
            // 401/409 = 宿主闸门拒绝（凭据/一 Peer 一 socket）——方法未运行。
            401 => {
                return Err(NotExecuted(DshError::new(
                    codes::PEER_UNAUTHORIZED,
                    "host refused the peer cookie (HTTP 401); terminal, no retry",
                )))
            }
            409 => {
                return Err(NotExecuted(DshError::new(
                    codes::PEER_CONFLICT,
                    "host refused the unary call (HTTP 409); one-peer-one-socket gate assumed",
                )))
            }
            // 其余非 2xx（5xx 等）：处理器可能已运行且产生副作用——保守按未知。
            s if !(200..300).contains(&s) => {
                return Err(DeliveryUnknown(DshError::new(
                    codes::CONNECT_FAILED,
                    format!("unary got HTTP {s}"),
                )))
            }
            _ => {}
        }
        let content_type = response.content_type.clone();
        // 响应体读取/超限已在 http1 层完成：任何截断/畸形/超限都映射为
        // DeliveryUnknown（错误到达本处即未知）。
        let bytes = response.body;
        if !content_type.starts_with("application/json") {
            // 二进制响应按 content-type 原样分流（不硬解成信封）。
            return Ok(UnaryReply::Binary {
                content_type,
                bytes,
            });
        }
        let raw: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
            DeliveryUnknown(DshError::new(
                codes::FRAME_INVALID,
                format!("unary body is not valid JSON: {e}"),
            ))
        })?;
        if codec::is_stream_signature_shape(&raw) {
            // 流签名落在错通道：拒绝，不误解释（E2 `gateway/signature-invalid`
            // 的客户端侧对偶；流式方法必须走 subscribe()）。
            return Err(DeliveryUnknown(DshError::new(
                codes::FRAME_INVALID,
                "stream signature frame on the unary carrier; open stream methods through subscribe()",
            )));
        }
        // 信封解码（含 rpcId 身份闸）：响应不可用 = 结局未知。
        let resp = codec::decode_response(&bytes, &rpc).map_err(DeliveryUnknown)?;
        Ok(UnaryReply::Envelope(resp))
    }
}

/// 一元响应（按 content-type 分流；二进制不硬解成信封）。
#[derive(Debug, Clone, PartialEq)]
pub enum UnaryReply {
    /// JSON 信封（已过 rpcId 身份闸）。
    Envelope(NativeResponse),
    /// 二进制响应（如上传/附件精确路由；≤ [`MAX_JSON_FRAME_BYTES`]）。
    Binary {
        /// 响应 content-type（原样）。
        content_type: String,
        /// 原始字节。
        bytes: Vec<u8>,
    },
}

/// `transport::connect`（§2.3 固定入口）：开该 Peer 的归属连接集——
/// no-proxy HTTP 客户端 + `/api/remote.mux` 唯一 socket。不重认证
/// （cookie 已由授权域交付）；401/409 为终态错误。
pub async fn connect(peer: AuthorizedPeer) -> Result<PeerConnection, DshError> {
    connect_with(peer, TransportConfig::default()).await
}

// ============================================================
// 撤权闸门（R08：撤销必须贯穿传输层，而非只清授权域的内存句柄）
// ============================================================

/// 每 binding 一份的撤权闸门（auth 持有并翻转，transport 咨询并登记活连接）。
///
/// ## 存在理由（R08）
///
/// 旧 `revoke` 只清 `current_peer` 与一条本机 IPC 连接；**已经交付出去的句柄**、
/// 官方 [`PeerConnection`]、Mux socket 全都不受影响——一旦连上原生通道，旧连接
/// 还能继续发请求。撤权必须在这里落地三件事：
///
/// ① [`PeerGate::revoke`] 翻转后，任何带该闸门的 [`AuthorizedPeer`] 在
///    [`connect`] 第一步就拒绝（专用码 [`codes::PEER_REVOKED`]）；
/// ② 已登记的在途连接（含 mux socket）被**立即关闭**（复用 transport 自己的
///    关闸路径 `Core::request_shutdown`，writer 会发 WS Close 并关 socket）；
/// ③ 重连同样是 ①（闸门永久为 revoked，撤权后的公钥回不到「已授权」）。
///
/// ## 如实申报的边界（spec §5）
///
/// **已经放行并落地的原生操作不承诺回滚**：撤权阻止的是此后的一切新写与新连接，
/// 不会撤销宿主侧已经在跑的回合。这里不假装能回滚。
pub struct PeerGate {
    /// 撤权旗标（单调 true；不提供复位——复位等于悄悄重开权限）。
    revoked: AtomicBool,
    revocation: tokio::sync::watch::Sender<bool>,
    /// 活连接登记（弱引用：连接本体被 drop 后自动失效，不需显式注销）。
    live: Mutex<Vec<Weak<Core>>>,
}

impl PeerGate {
    /// 新建闸门（每 binding 一份；auth 与 transport 共享同一 `Arc`）。
    pub fn new() -> Arc<Self> {
        let (revocation, _) = tokio::sync::watch::channel(false);
        Arc::new(Self {
            revoked: AtomicBool::new(false),
            revocation,
            live: Mutex::new(Vec::new()),
        })
    }

    /// 是否已撤权（历史撤权永久生效）。
    pub fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::SeqCst)
    }

    pub fn subscribe_revocation(&self) -> tokio::sync::watch::Receiver<bool> {
        self.revocation.subscribe()
    }

    /// 当前登记且仍存活的连接数（测试与诊断观测面）。
    pub fn live_connections(&self) -> usize {
        let mut live = self.live.lock().unwrap();
        live.retain(|core| core.strong_count() > 0);
        live.len()
    }

    /// 撤权：翻转旗标 → 关闭全部在途连接。幂等（重复调用只是再关一遍）。
    ///
    /// 关闭走 [`Core::request_shutdown`]（transport 既有路径）：writer 收尾发
    /// WS Close 帧并关 socket，reader 退出后观察面 `write_available` 立即为 false。
    pub fn revoke(&self) {
        self.revoked.store(true, Ordering::SeqCst);
        self.revocation.send_replace(true);
        let cores: Vec<Arc<Core>> = {
            let mut live = self.live.lock().unwrap();
            let cores: Vec<Arc<Core>> = live.iter().filter_map(Weak::upgrade).collect();
            live.clear();
            cores
        };
        for core in cores {
            core.request_shutdown();
        }
    }

    /// 登记一条连接；闸门已撤权 → 返回 false（调用方拒绝建连，不登记）。
    fn register(&self, core: &Arc<Core>) -> bool {
        let mut live = self.live.lock().unwrap();
        if self.revoked.load(Ordering::SeqCst) {
            // 双检：revoke 可能在注册前刚完成（建连与撤权竞争时以撤权为准）。
            return false;
        }
        // 顺手清掉已失效的登记（连接被提前 drop 的路径）。
        live.retain(|registered| registered.strong_count() > 0);
        live.push(Arc::downgrade(core));
        true
    }
}

impl std::fmt::Debug for PeerGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerGate")
            .field("revoked", &self.is_revoked())
            .field("live_connections", &self.live_connections())
            .finish()
    }
}

/// 带配置的 [`connect`]（测试注入短超时；生产走默认 10 秒）。
pub async fn connect_with(
    peer: AuthorizedPeer,
    config: TransportConfig,
) -> Result<PeerConnection, DshError> {
    let authority = peer.authority().trim().to_string();
    let epoch = peer.epoch();
    if authority.is_empty() {
        return Err(DshError::new(
            codes::CONNECT_FAILED,
            "authorized peer carries no pinned loopback authority",
        ));
    }
    // 撤权闸门第一查：已撤权的句柄连建连尝试都不做（不向宿主发任何请求）。
    let gate = peer.gate().map(Arc::clone);
    if let Some(gate) = gate.as_ref() {
        if gate.is_revoked() {
            return Err(DshError::new(
                codes::PEER_REVOKED,
                "this binding was revoked; reconnect refused",
            ));
        }
    }
    // cookie 秘密载荷只进内存请求头，不进日志（错误 detail 一律不含它）。
    let cookie = String::from_utf8(peer.secret().to_vec())
        .map_err(|_| DshError::new(codes::CONNECT_FAILED, "peer cookie material is not UTF-8"))?;
    let ws = upgrade_mux_socket(&authority, &cookie).await?;

    // 分半：writer 持 sink（控制帧入口 + 关闭闸），reader 持 stream（demux + 镜像守卫）。
    let (sink, stream) = ws.split();
    let (control_tx, control_rx) = mpsc::channel(CONTROL_BUFFER);
    let (close_tx, close_rx) = tokio::sync::watch::channel(false);
    let core = Arc::new(Core {
        authority,
        cookie,
        epoch,
        host_id: peer.host_id().to_string(),
        config,
        mirror: Arc::new(EventMirror::new(epoch)),
        control: control_tx,
        closed: Arc::new(AtomicBool::new(false)),
        close_tx,
        client_id: Mutex::new(None),
        streams: Mutex::new(HashMap::new()),
        stream_seq: AtomicU64::new(0),
        tasks: AsyncMutex::new(None),
    });
    let writer = tokio::spawn(writer_loop(sink, control_rx, close_rx));
    let reader = tokio::spawn(reader_loop(stream, core.clone()));
    *core.tasks.lock().await = Some((writer, reader));
    // 登记到撤权闸门（② 在途连接可被撤权关闭）。登记与撤权并发时以撤权为准：
    // register 拒绝 → 立即关掉刚建好的连接再返回 PEER_REVOKED（不留下无主 socket）。
    if let Some(gate) = gate.as_ref() {
        if !gate.register(&core) {
            core.request_shutdown();
            if let Some((writer, reader)) = core.tasks.lock().await.take() {
                join_bounded(writer).await;
                join_bounded(reader).await;
            }
            return Err(DshError::new(
                codes::PEER_REVOKED,
                "binding was revoked while connecting; refused",
            ));
        }
    }
    Ok(PeerConnection { core })
}

pub struct RedeemedConnect {
    /// 宿主签名的 API cookie（`name=value` 全串；秘密）。
    pub cookie: String,
    /// 302 location（官方 = `./api/remote.mux`）。
    pub mux_location: String,
}

impl std::fmt::Debug for RedeemedConnect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // mux_location 非秘密（官方固定形态），可显示；cookie 只报长度。
        f.debug_struct("RedeemedConnect")
            .field(
                "cookie",
                &format_args!("<redacted:len={}>", self.cookie.len()),
            )
            .field("mux_location", &self.mux_location)
            .finish()
    }
}

/// `/connect` 显式交换（G1 接线点）：0.2.1-rc.3 的真实 cookie 兑换由宿主内插件
/// 交付（T06），transport **不重认证**；本入口供后续把兑换收回 Rust 侧时复用。
/// 协议要求 token 走查询串（官方认证交换形态）——必须脱敏：请求错误一律
/// `without_url()`，token/cookie 不进任何 detail。
///
/// HTTP 客户端（安全硬约束的落点）：
/// * `.no_proxy()`——crate 全局 reqwest 带 `system-proxy` feature（读环境变量与
///   OS 代理设置），**DSH loopback 客户端实例必须显式覆盖**；
/// * `redirect::none`——3xx 一律不盲随：`/connect` 的 302 显式取 cookie/location
///   （[`redeem_launch_token`]），其余重定向 = `DSH_REDIRECT_BLOCKED`。
///
/// 仅测试面：一元传输已改走 http1 直连直写，生产 reqwest 客户端只剩
/// `redeem_launch_token` 测试注入面——保留构造器供其断言代理/重定向策略。
#[cfg(test)]
fn build_http_client() -> Result<reqwest::Client, DshError> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| {
            DshError::new(
                codes::CONNECT_FAILED,
                format!("http client build failed: {e}"),
            )
        })
}

/// 唯一归属 socket：`ws://<钉定 authority>/api/remote.mux` WS 升级。
/// 401/409 → 终态错误映射；其余失败 → `DSH_CONNECT_FAILED`。
async fn upgrade_mux_socket(
    authority: &str,
    cookie: &str,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, DshError> {
    let mut request = format!("ws://{authority}/api/remote.mux")
        .into_client_request()
        .map_err(|e| {
            DshError::new(
                codes::CONNECT_FAILED,
                format!("mux upgrade request is invalid: {e}"),
            )
        })?;
    request.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::COOKIE,
        HeaderValue::from_str(cookie).map_err(|_| {
            DshError::new(
                codes::CONNECT_FAILED,
                "cookie material is not a legal header value",
            )
        })?,
    );
    // socket 层硬界：入站消息 = 协议帧上限（合法帧 ≤ 2MiB 必过，超限在
    // tungstenite 内部直接拒绝，不做无界缓冲；帧处理层另有显式护栏）。
    let ws_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(codec::MAX_JSON_FRAME_BYTES))
        .max_frame_size(Some(codec::MAX_JSON_FRAME_BYTES));
    let upgrade = tokio::time::timeout(
        CONNECT_TIMEOUT,
        connect_async_with_config(request, Some(ws_config), false),
    )
    .await
    .map_err(|_| DshError::new(codes::CONNECT_FAILED, "mux upgrade timed out"))?;
    match upgrade {
        Ok((ws, _upgrade_resp)) => Ok(ws),
        Err(WsError::Http(resp)) => Err(map_upgrade_status(resp.status())),
        Err(e) => Err(DshError::new(
            codes::CONNECT_FAILED,
            format!("mux upgrade failed: {e}"),
        )),
    }
}

/// 升级拒绝映射：401 = 凭据被拒（终态）、409 = 一 Peer 一 socket（终态）、
/// 其余 = 建连失败（403 栅栏等）。detail 不含 cookie。
fn map_upgrade_status(status: HttpStatus) -> DshError {
    match status.as_u16() {
        401 => DshError::new(
            codes::PEER_UNAUTHORIZED,
            "host refused the peer cookie (HTTP 401); terminal, no retry",
        ),
        409 => DshError::new(
            codes::PEER_CONFLICT,
            "host already owns an API socket for this peer (HTTP 409); terminal, no reconnect",
        ),
        s => DshError::new(
            codes::CONNECT_FAILED,
            format!("mux upgrade rejected with HTTP {s}"),
        ),
    }
}

pub async fn redeem_launch_token(
    http: &reqwest::Client,
    authority: &str,
    launch_token: &str,
    interactions: &str,
) -> Result<RedeemedConnect, DshError> {
    if !matches!(interactions, "answer" | "none") {
        return Err(DshError::new(
            codes::CONNECT_FAILED,
            "interactions must be `answer` or `none`",
        ));
    }
    // launch token 是 base64url（无保留字符），按官方形态原样进查询串。
    let url =
        format!("http://{authority}/connect?token={launch_token}&interactions={interactions}");
    let resp = http.get(&url).send().await.map_err(|e| {
        DshError::new(
            codes::CONNECT_FAILED,
            format!("/connect failed: {}", e.without_url()),
        )
    })?;
    let status = resp.status();
    match status.as_u16() {
        401 => Err(DshError::new(
            codes::PEER_UNAUTHORIZED,
            "launch token refused (HTTP 401); terminal",
        )),
        s if (300..400).contains(&s) => {
            // 302 显式处理：只接受回到本 authority 的 ./api/remote.mux 形态；
            // 跳到别处一律拒绝（不盲随）。
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            if !(location.starts_with("./api/remote.mux")
                || location.starts_with("/api/remote.mux"))
            {
                return Err(DshError::new(
                    codes::REDIRECT_BLOCKED,
                    format!("/connect redirect leaves the pinned mux shape: {location}"),
                ));
            }
            let cookie = extract_set_cookie(resp.headers()).ok_or_else(|| {
                DshError::new(codes::CONNECT_FAILED, "/connect 3xx carries no API cookie")
            })?;
            Ok(RedeemedConnect {
                cookie,
                mux_location: location,
            })
        }
        s => Err(DshError::new(
            codes::CONNECT_FAILED,
            format!("/connect returned HTTP {s} (expected 302)"),
        )),
    }
}

/// 从 Set-Cookie 里取 `dsh-auth-<b64u>=<签名>` 的 name=value 全串
/// （参数不保留；cookie 值不进日志——调用方只把它装进 [`AuthorizedPeer`]）。
fn extract_set_cookie(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            let text = v.to_str().ok()?;
            let first = text.split(';').next()?.trim();
            (first.starts_with("dsh-auth-") && first.contains('=')).then(|| first.to_string())
        })
}

// ============================================================
// PeerConnection（归属连接集）
// ============================================================

/// Peer 归属连接（no-proxy HTTP 客户端 + 一条 Mux socket）。
/// Drop 全部句柄（连接本体与各订阅句柄）= 取消流、关 socket。
pub struct PeerConnection {
    core: Arc<Core>,
}

impl std::fmt::Debug for PeerConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Debug 不携带 cookie（秘密纪律）。
        f.debug_struct("PeerConnection").finish_non_exhaustive()
    }
}

impl Drop for PeerConnection {
    fn drop(&mut self) {
        // 纯 Drop 路径（未经 close()）也要关 socket：reader 持有 Arc<Core>，
        // 不主动关闸就没有任何一方能让 writer 退出（任务遗留活锁）——
        // 关闭信号走 watch 通道，不依赖控制帧缓冲余量。
        self.core.request_shutdown();
    }
}

/// 连接核心（reader/writer 任务与句柄共享的归属状态）。**不含 Debug**——
/// 内有 cookie 明文，绝不进日志。
struct Core {
    authority: String,
    cookie: String,
    epoch: u64,
    /// P1-c（第四轮复审）：建连时的宿主身份（AuthorizedPeer.host_id）——信道
    /// 选定连接后与动作 target 核对（换代/换宿主即拒，防旧动作被送入新代际）。
    host_id: String,
    config: TransportConfig,
    mirror: Arc<EventMirror>,
    control: mpsc::Sender<ControlFrame>,
    closed: Arc<AtomicBool>,
    /// 关闭信号（writer 的 select 闸；Drop/close 都走这里，不依赖缓冲余量）。
    close_tx: tokio::sync::watch::Sender<bool>,
    client_id: Mutex<Option<String>>,
    streams: Mutex<HashMap<String, mpsc::Sender<Result<MirroredEvent, DshError>>>>,
    stream_seq: AtomicU64,
    /// reader/writer 的 join 句柄（close() 有界 join；None = 已收过）。
    tasks: AsyncMutex<Option<(JoinHandle<()>, JoinHandle<()>)>>,
}

impl Core {
    /// 请求关闭：翻转观测面 + 触发 writer 退出（writer 收尾发 WS Close 帧）。
    fn request_shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = self.close_tx.send(true);
    }
}

/// writer 控制帧。
enum ControlFrame {
    /// 编码好的 mux 上行帧（open/cancel）。
    Text(Vec<u8>),
}

/// 帧路由结果（Fatal = 连接级协议违规，reader 断开）。
enum RouteOutcome {
    Routed,
    Fatal,
}

impl Core {
    /// 标记连接已死并释放全部流（senders 掉线 → 各流 next() 得 None 终结）。
    fn mark_closed(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.streams.lock().unwrap().clear();
    }

    /// 把连接级错误送达全部活跃流（各自收到一条 Err 后终结）。
    fn fail_all_streams(&self, code: &'static str, detail: String) {
        let mut map = self.streams.lock().unwrap();
        for (_id, tx) in map.drain() {
            let _ = tx.try_send(Err(DshError::new(code, detail.clone())));
        }
    }

    /// 投递到指定流；缓冲满/已关闭 → 只舍该流（不拖垮共享 reader）。
    fn deliver(&self, stream_id: &str, item: Result<MirroredEvent, DshError>) {
        let sender = self.streams.lock().unwrap().get(stream_id).cloned();
        if let Some(tx) = sender {
            if tx.try_send(item).is_err() {
                self.streams.lock().unwrap().remove(stream_id);
            }
        }
    }

    /// mux 下行帧路由（E2/E3 键形；畸形帧受限拒绝，不毒化连接）。
    fn route_frame(&self, bytes: &[u8]) -> RouteOutcome {
        // 体积护栏（socket 层之外的第二道；理论上 socket 层先拒）。
        if bytes.len() > codec::MAX_JSON_FRAME_BYTES {
            self.fail_all_streams(
                codes::FRAME_OVERSIZE,
                format!(
                    "mux frame is {} bytes (cap {})",
                    bytes.len(),
                    codec::MAX_JSON_FRAME_BYTES
                ),
            );
            return RouteOutcome::Fatal;
        }
        let raw: serde_json::Value = match serde_json::from_slice(bytes) {
            Ok(v) => v,
            Err(_) => {
                // 畸形 JSON：无可靠 streamId 可路由 → 丢弃（受限拒绝，不断连接）。
                return RouteOutcome::Routed;
            }
        };
        let frame_type = raw.get("type").and_then(|v| v.as_str());
        let stream_id = raw.get("streamId").and_then(|v| v.as_str());
        match (frame_type, stream_id) {
            (Some("item"), Some(sid)) => {
                let value = raw.get("value").cloned().unwrap_or(serde_json::Value::Null);
                // ready 事件：宿主在 open $events 后下发，携带本 Peer 的 clientId。
                if value.get("type").and_then(|v| v.as_str()) == Some("ready") {
                    if let Some(client) = value.get("clientId").and_then(|v| v.as_str()) {
                        *self.client_id.lock().unwrap() = Some(client.to_string());
                    }
                }
                // 代际守卫：旧代际晚帧 → 不入镜、不路由（计划「旧代际不污染」）。
                let Some(event) = self.mirror.apply(self.epoch, value) else {
                    return RouteOutcome::Routed;
                };
                self.deliver(sid, Ok(event));
                RouteOutcome::Routed
            }
            // end/cancel = 流终结（end 完整键形未钉死 → G1 夹具复核；兼容带 value）。
            (Some("end") | Some("cancel"), Some(sid)) => {
                self.streams.lock().unwrap().remove(sid);
                RouteOutcome::Routed
            }
            (Some("error"), Some(sid)) => {
                // 宿主流错误帧：{"type":"error","streamId",error:{code,message}}
                // （windows 报告 §3 实测键形）→ 流级错误后终结。
                let native_code = raw
                    .pointer("/error/code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let message = raw
                    .pointer("/error/message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let err = DshError::new(
                    codes::STREAM_ERROR,
                    format!("native stream error code={native_code} message={message}"),
                );
                self.deliver(sid, Err(err));
                self.streams.lock().unwrap().remove(sid);
                RouteOutcome::Routed
            }
            _ => {
                // 未知/畸形形态：有主 → 报给该流；无主 → 丢弃（受限拒绝）。
                match stream_id {
                    Some(sid) => {
                        self.deliver(
                            sid,
                            Err(DshError::new(
                                codes::FRAME_INVALID,
                                format!("unrecognized mux frame type {frame_type:?}"),
                            )),
                        );
                        RouteOutcome::Routed
                    }
                    None => RouteOutcome::Routed,
                }
            }
        }
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        // 兜底：观测面即刻翻转（write_available → false）；真正的关闸由
        // PeerConnection::drop 的 request_shutdown 负责。
        self.closed.store(true, Ordering::SeqCst);
    }
}

/// 一元请求失败的**相位**分类（T10 复审 P1-2：错误码本身不区分「请求是否
/// 可能已送达」——同一 `FRAME_OVERSIZE` 既可能是「请求体编码超限（未发）」
/// 也可能是「响应体超限（请求已到达宿主）」，只有产生错误的**位置**知道相位）。
///
/// 消费方（[`crate::services::dsh_control::actions`]）的落账纪律：
/// * [`UnaryFailure::NotExecuted`] → 明确 `Failed`（确定未发送/宿主闸门拒绝）；
/// * [`UnaryFailure::DeliveryUnknown`] → `UnknownDelivery`（**不得记明确失败，
///   禁止自动重发**——POST 可能已经送达）。
pub enum UnaryFailure {
    /// 请求确定未被执行：发送前失败（本地拒写/请求编码失败）或宿主闸门拒绝
    /// （401 凭据 / 409 一 Peer 一 socket / 3xx 路由层重定向——方法未运行）。
    NotExecuted(DshError),
    /// 请求已交出但结局未知：等响应超时、传输中断、或响应不可解读
    /// （非法 JSON / 二进制信封缺失 / rpcId 不匹配 / 响应超限）。
    DeliveryUnknown(DshError),
}

impl UnaryFailure {
    /// 取出底层错误（丢弃相位——`request_raw` 等不关心相位的调用面用）。
    pub fn into_error(self) -> DshError {
        match self {
            UnaryFailure::NotExecuted(e) | UnaryFailure::DeliveryUnknown(e) => e,
        }
    }
}

impl std::fmt::Debug for UnaryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnaryFailure::NotExecuted(e) => write!(f, "NotExecuted({}: {})", e.code, e.detail),
            UnaryFailure::DeliveryUnknown(e) => {
                write!(f, "DeliveryUnknown({}: {})", e.code, e.detail)
            }
        }
    }
}

impl PeerConnection {
    /// P1-c：连接身份（建连时的 host_id/epoch/authority）——调用方在实际写出
    /// 前与动作 target 核对（不匹配 = 换代/换宿主，拒绝，防跨代误投）。
    pub fn identity(&self) -> (String, u64, String) {
        (
            self.core.host_id.clone(),
            self.core.epoch,
            self.core.authority.clone(),
        )
    }

    /// 归属 socket 是否仍可写（断链/关闭后恒 false——后续写入立即拒绝，不排队）。
    pub fn write_available(&self) -> bool {
        !self.core.closed.load(Ordering::SeqCst)
    }

    /// 宿主 ready 事件携带的 clientId（尚未观测到 ready → None）。
    pub fn client_id(&self) -> Option<String> {
        self.core.client_id.lock().unwrap().clone()
    }

    /// 代际镜像守卫（连接共享；换代推进与晚帧断言的观测面）。
    pub fn mirror(&self) -> Arc<EventMirror> {
        self.core.mirror.clone()
    }

    /// 一元请求（JSON 信封路径；二进制响应不硬解 → `DSH_FRAME_INVALID`，
    /// 用 [`PeerConnection::request_raw`] 取原始字节）。
    pub async fn request(
        &self,
        rpc_id: &str,
        method: &str,
        args: &serde_json::Value,
    ) -> Result<NativeResponse, DshError> {
        match self.request_raw(rpc_id, method, args).await? {
            UnaryReply::Envelope(resp) => Ok(resp),
            UnaryReply::Binary { content_type, .. } => Err(DshError::new(
                codes::FRAME_INVALID,
                format!(
                    "binary response ({content_type}) is not a server-response envelope; use request_raw"
                ),
            )),
        }
    }

    /// 一元请求的**相位分类**版（P1-2）：交付 → 分类（响应二进制不硬解 →
    /// 发送后未知）。
    pub async fn request_classified(
        &self,
        rpc_id: &str,
        method: &str,
        args: &serde_json::Value,
    ) -> Result<NativeResponse, UnaryFailure> {
        self.request_deliver(rpc_id, method, args)
            .await?
            .classify()
            .await
    }

    /// 一元请求（content-type 分流）：`POST /api/{ns}/{method}` + cookie，只发往
    /// 钉定 authority；10 秒超时 = `DSH_WRITE_TIMEOUT`（送达未知，不自动重投）；
    /// 3xx 一律 `DSH_REDIRECT_BLOCKED`；401/409 终态映射。
    pub async fn request_raw(
        &self,
        rpc_id: &str,
        method: &str,
        args: &serde_json::Value,
    ) -> Result<UnaryReply, DshError> {
        self.request_deliver(rpc_id, method, args)
            .await
            .map_err(UnaryFailure::into_error)?
            .classify_raw()
            .await
            .map_err(UnaryFailure::into_error)
    }

    /// 交付一元请求（销项复审 P1 交付边界）：闭检查/编码同步完成后发出请求，
    /// **交付沿 = 请求体交出 HTTP 栈**（hyper 取走全部请求字节进入写缓冲）
    /// 即返回 [`DeliveredResponse`]；响应的等待与分类在
    /// [`DeliveredResponse::classify`/`classify_raw`]（调用方决定时机——提交
    /// 许可区外）。提交前失败 = NotExecuted（本地拒绝，**零字节出门可证**：
    /// 连接已关/编码错/建连失败——每请求独立 socket，建连失败即无字节）；
    /// 交付沿后失败 = DeliveryUnknown（部分写出可能——保守未知，不自动重投）。
    ///
    /// **交付沿（目标 2 升级，取代旧「请求体交出 HTTP 栈」近似）**：一元面
    /// 改走 [`http1::start`]——手写 HTTP/1.1 到 TokioTcpStream，整帧
    /// （头+body 单缓冲）`write_all` 完成 = 字节交内核 = **真实交付沿**
    /// （tokio TcpStream 无用户态写缓冲，回环下宿主 socket 立即可见）。
    /// 旧实现的「hyper 取走请求体」残余窗（许可释放后、栈内 flush 前仍可能
    /// 写向旧实例）**就此归零**；部分写后的失败/取消由连接关闭收口（宿主按
    /// Content-Length 不齐丢弃，业务零执行）。取消语义：`request_deliver`
    /// future 被 drop（调用方 select! 分支）→ 建连/写 future 随之 drop →
    /// socket 关闭，零残留续写。
    pub async fn request_deliver(
        &self,
        rpc_id: &str,
        method: &str,
        args: &serde_json::Value,
    ) -> Result<DeliveredResponse, UnaryFailure> {
        self.request_deliver_accepted(rpc_id, method, args, None)
            .await
    }

    pub(crate) async fn request_deliver_accepted(
        &self,
        rpc_id: &str,
        method: &str,
        args: &serde_json::Value,
        acceptance: Option<(&str, &super::production::SendAcceptance)>,
    ) -> Result<DeliveredResponse, UnaryFailure> {
        self.request_deliver_checked(rpc_id, method, args, acceptance, None, None)
            .await
    }

    pub(crate) async fn request_deliver_checked(
        &self,
        rpc_id: &str,
        method: &str,
        args: &serde_json::Value,
        acceptance: Option<(&str, &super::production::SendAcceptance)>,
        first_write_check: Option<(super::model::Target, super::actions::PreSubmitGate)>,
        device: Option<(&crate::remote::pairing::DeviceStore, &str)>,
    ) -> Result<DeliveredResponse, UnaryFailure> {
        use UnaryFailure::{DeliveryUnknown, NotExecuted};
        if self.core.closed.load(Ordering::SeqCst) {
            return Err(NotExecuted(DshError::new(
                codes::CONNECTION_CLOSED,
                "peer connection is closed; unary refused before send",
            )));
        }
        // 请求编码失败 = 发送前失败（本地拒绝，零字节出门）。
        // 编码失败保留原码（DSH_FRAME_OVERSIZE 等——语义面由 codec 决定）。
        let body = codec::encode_request(rpc_id, method, args).map_err(NotExecuted)?;
        let core = self.core.clone();
        let rpc = rpc_id.to_string();
        // URL 只由钉定 authority 拼出——cookie 结构性不可能发给其他宿主。
        let flight = super::http1::start_checked(
            &core.authority,
            &format!("/api/{method}"),
            &core.cookie,
            body,
            [CONNECT_TIMEOUT, core.config.unary_timeout],
            super::http1::WriteGuard {
                rpc_id,
                acceptance,
                closed: &core.closed,
                first_write_check,
                device,
            },
        )
        .await
        .map_err(|e| match e.phase {
            // 建连段 = 零字节出门可证 → NotExecuted；写段 = 部分写出可能
            // → DeliveryUnknown（保守未知，不自动重投）。
            crate::services::dsh_control::http1::StartPhase::Connect => NotExecuted(e.error),
            crate::services::dsh_control::http1::StartPhase::Write => DeliveryUnknown(e.error),
        })?;
        Ok(DeliveredResponse {
            response_fut: Box::pin(flight.finish()),
            rpc_id: rpc,
        })
    }

    /// 有界订阅：open 一条 mux 逻辑流并返回事件句柄。Drop 句柄尽力发 cancel；
    /// 断链后拒绝（不排队）。
    pub async fn subscribe(
        &self,
        endpoint: &str,
        args: &serde_json::Value,
    ) -> Result<StreamHandle, DshError> {
        if self.core.closed.load(Ordering::SeqCst) {
            return Err(DshError::new(
                codes::CONNECTION_CLOSED,
                "peer connection is closed; subscribe refused",
            ));
        }
        let stream_id = format!(
            "s-{:016x}-{:04x}",
            random_u64(),
            self.core.stream_seq.fetch_add(1, Ordering::SeqCst)
        );
        let (tx, rx) = mpsc::channel(STREAM_BUFFER);
        self.core
            .streams
            .lock()
            .unwrap()
            .insert(stream_id.clone(), tx);
        let frame = match codec::encode_open(&stream_id, endpoint, args) {
            Ok(frame) => frame,
            Err(e) => {
                self.core.streams.lock().unwrap().remove(&stream_id);
                return Err(e);
            }
        };
        if self
            .core
            .control
            .send(ControlFrame::Text(frame))
            .await
            .is_err()
        {
            self.core.streams.lock().unwrap().remove(&stream_id);
            return Err(DshError::new(
                codes::CONNECTION_CLOSED,
                "mux writer is gone",
            ));
        }
        Ok(StreamHandle {
            stream_id,
            rx,
            control: self.core.control.clone(),
            closed: Arc::clone(&self.core.closed),
        })
    }

    /// 显式关闭：取消全部流、关 socket、有界 join reader/writer（无遗留任务；
    /// 超时兜底 abort，绝不悬挂）。
    pub async fn close(&self) {
        self.core.request_shutdown();
        if let Some((writer, reader)) = self.core.tasks.lock().await.take() {
            join_bounded(writer).await;
            join_bounded(reader).await;
        }
        self.core.streams.lock().unwrap().clear();
    }
}

/// 有界 join：完成即证明任务退出（无遗留）；超时兜底 abort。
async fn join_bounded(mut handle: JoinHandle<()>) {
    if tokio::time::timeout(CLOSE_JOIN_TIMEOUT, &mut handle)
        .await
        .is_err()
    {
        handle.abort();
    }
}

// ============================================================
// reader / writer 任务（每条归属连接恰两个；close 可 join）
// ============================================================

type MuxSink = futures::stream::SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, WsMessage>;
type MuxStream = futures::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

/// writer：控制帧唯一出口；关闭信号（watch）或通道断供 → 发 WS Close 帧并
/// 冲刷关闭（收尾对两条退出路径统一）。
async fn writer_loop(
    mut sink: MuxSink,
    mut control_rx: mpsc::Receiver<ControlFrame>,
    mut close_rx: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            frame = control_rx.recv() => match frame {
                Some(ControlFrame::Text(bytes)) => {
                    if sink
                        .send(WsMessage::text(String::from_utf8_lossy(&bytes).into_owned()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                None => break,
            },
            _ = close_rx.changed() => break,
        }
    }
    let _ = sink.send(WsMessage::Close(None)).await;
    let _ = sink.close().await;
}

/// reader：下行帧 demux（item/end/cancel/error + 代际守卫）；socket 关闭或
/// 协议级违规 → 标死连接并释放全部流。
async fn reader_loop(mut stream: MuxStream, core: Arc<Core>) {
    'read: while let Some(msg) = stream.next().await {
        let bytes: Vec<u8> = match msg {
            Ok(WsMessage::Text(text)) => text.as_bytes().to_vec(),
            Ok(WsMessage::Binary(bin)) => bin.to_vec(),
            Ok(WsMessage::Close(_)) => break,
            Ok(_) => continue,
            Err(WsError::Capacity(_)) => {
                // socket 层超限（2MiB 硬界）：有界拒绝 + 断开。
                core.fail_all_streams(
                    codes::FRAME_OVERSIZE,
                    format!(
                        "mux frame exceeds the socket-level cap of {} bytes",
                        codec::MAX_JSON_FRAME_BYTES
                    ),
                );
                break 'read;
            }
            Err(e) => {
                core.fail_all_streams(codes::CONNECTION_CLOSED, format!("mux read failed: {e}"));
                break 'read;
            }
        };
        match core.route_frame(&bytes) {
            RouteOutcome::Routed => {}
            RouteOutcome::Fatal => {
                core.request_shutdown();
                break 'read;
            }
        }
    }
    core.mark_closed();
}

// ============================================================
// StreamHandle（有界订阅）
// ============================================================

/// 订阅流句柄：`next()` 逐事件取入镜事件；None = 流终结（end/断链/取消）。
/// Drop 时尽力发 `{type:"cancel",streamId}`（有界通道 try_send，绝不阻塞）。
pub struct StreamHandle {
    stream_id: String,
    rx: mpsc::Receiver<Result<MirroredEvent, DshError>>,
    control: mpsc::Sender<ControlFrame>,
    closed: Arc<AtomicBool>,
}

impl std::fmt::Debug for StreamHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamHandle")
            .field("stream_id", &self.stream_id)
            .finish_non_exhaustive()
    }
}

impl StreamHandle {
    /// 下一条入镜事件（或流级错误）；None = 流终结。
    pub async fn next(&mut self) -> Option<Result<MirroredEvent, DshError>> {
        self.rx.recv().await
    }

    /// 流 ID（mux streamId）。
    pub fn stream_id(&self) -> &str {
        &self.stream_id
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        // 连接还活着才值得发 cancel（已死时 try_send 到死通道无害地失败）。
        if !self.closed.load(Ordering::SeqCst) {
            let _ = self
                .control
                .try_send(ControlFrame::Text(codec::encode_cancel(&self.stream_id)));
        }
    }
}

// ============================================================
// 单元测试：/connect 显式交换 + authority 钉定边界（G1 接线点的本地验证）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::dsh_control::error::codes;
    use axum::http::header;
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::Router;
    use std::collections::HashMap;

    /// 本地 /connect 假路由（unit 测试专用；127.0.0.1 临时端口）。
    async fn spawn_connect_stub(script: &'static str) -> String {
        let app = Router::new().route(
            "/connect",
            get(
                move |_params: axum::extract::Query<HashMap<String, String>>| async move {
                    match script {
                        // 302 + cookie + 官方 location 形态
                        "ok" => Response::builder()
                            .status(302)
                            .header(header::LOCATION, "./api/remote.mux")
                            .header(
                                header::SET_COOKIE,
                                "dsh-auth-unitfake=signed-value; Path=/; HttpOnly; SameSite=Strict",
                            )
                            .body(axum::body::Body::empty())
                            .unwrap(),
                        // 跳出钉定形态的重定向
                        "off_authority" => Response::builder()
                            .status(302)
                            .header(header::LOCATION, "http://10.9.9.9:1/evil")
                            .body(axum::body::Body::empty())
                            .unwrap(),
                        // interactions 非法（E2：400）
                        "bad_interactions" => {
                            (axum::http::StatusCode::BAD_REQUEST, "bad").into_response()
                        }
                        _ => {
                            (axum::http::StatusCode::UNAUTHORIZED, "token refused").into_response()
                        }
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn redeem_captures_cookie_and_mux_location_explicitly() {
        let authority = spawn_connect_stub("ok").await;
        let http = build_http_client().unwrap();
        let redeemed = redeem_launch_token(&http, &authority, "dG9rZW4tdmFsdWU", "answer")
            .await
            .expect("302 显式捕获");
        assert_eq!(redeemed.cookie, "dsh-auth-unitfake=signed-value");
        assert_eq!(redeemed.mux_location, "./api/remote.mux");
    }

    #[tokio::test]
    async fn redeem_rejects_redirect_leaving_pinned_shape() {
        let authority = spawn_connect_stub("off_authority").await;
        let http = build_http_client().unwrap();
        let err = redeem_launch_token(&http, &authority, "dG9rZW4tdmFsdWU", "none")
            .await
            .expect_err("跳出钉定形态的重定向必须拒绝");
        assert_eq!(err.code, codes::REDIRECT_BLOCKED);
    }

    #[tokio::test]
    async fn redeem_maps_401_and_rejects_bad_interactions() {
        let authority = spawn_connect_stub("unauthorized").await;
        let http = build_http_client().unwrap();
        let err = redeem_launch_token(&http, &authority, "short", "answer")
            .await
            .expect_err("token 被拒");
        assert_eq!(err.code, codes::PEER_UNAUTHORIZED);

        let err2 = redeem_launch_token(&http, &authority, "short", "bogus")
            .await
            .expect_err("interactions 非法");
        assert_eq!(err2.code, codes::CONNECT_FAILED);
    }

    /// R12：兑换产物的 Debug 不得打印明文 cookie（与 `AuthorizedPeer` 同款脱敏
    /// 纪律）；`mux_location` 非秘密，照常显示。长度可以显示（便于诊断「cookie
    /// 取到了没有」），字节值一律不出现。
    #[test]
    fn redeemed_connect_debug_redacts_cookie() {
        let redeemed = RedeemedConnect {
            cookie: "dsh-auth-unitfake=super-secret-signed-value".to_string(),
            mux_location: "./api/remote.mux".to_string(),
        };
        let rendered = format!("{redeemed:?}");
        assert!(
            !rendered.contains("super-secret-signed-value"),
            "Debug 泄露了 cookie 明文：{rendered}"
        );
        assert!(
            !rendered.contains("dsh-auth-unitfake"),
            "Debug 泄露了 cookie 名（同样可关联宿主凭据）：{rendered}"
        );
        assert!(
            rendered.contains("redacted"),
            "必须留有脱敏标记：{rendered}"
        );
        assert!(
            rendered.contains("./api/remote.mux"),
            "mux_location 非秘密，照常显示：{rendered}"
        );
    }

    /// 退避调度：500ms 起翻倍、10 秒封顶（不无限增长、单调不回退）。
    #[test]
    fn backoff_schedule_is_capped_and_monotonic() {
        let mut previous = Duration::ZERO;
        for attempt in 0u32..=70 {
            let delay = recovery_backoff(attempt);
            assert!(delay <= RECOVERY_BACKOFF_CAP, "{attempt} → {delay:?}");
            assert!(delay >= previous);
            previous = delay;
        }
        assert_eq!(recovery_backoff(5), RECOVERY_BACKOFF_CAP);
    }
}
