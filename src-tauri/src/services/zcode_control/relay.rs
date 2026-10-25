//! ZCode 官方 relay terminal 的单连接生命周期（生产计划 T1.2）。
//!
//! # 合同（证据 E1：`relayAuth.ts`／`auth-check.ts`；合同 `auth`/`route` 段；决策 §6.9／§8.10）
//!
//! - **单连接、单 owner**：同一 `device_sid`/role 只允许一条 terminal socket。本模块没有
//!   自动重连/退避路径：KICKED、认证失败、路由窗超时、心跳超时、网络掉线全部终态收口。
//! - **每次连接现取 token**：`connect` 每次都向 [`AuthService::terminal_token`] 申请新的
//!   一次性 relay token（不缓存）；token 只出现在 WS 升级 URL 的 `token` 查询参数里，
//!   不落内存字段、不进日志/错误/事件。
//! - **生产端点固定**：`wss://zcode.z.ai/api/v1/remote/ws2?token=<token>&mid=<mid>`；
//!   `mid` 仅在已保存链接的 `mid` trim 非空时附加（官方条件性）。测试端点注入
//!   （[`RelayClient::with_endpoint`]）不注册任何配置/IPC 入口。
//! - **握手**：`auth_init{device_sid,meta{platform,version,name},client_ts}` →
//!   `auth_ack{device_sid,pair_status:waiting|matched}`；`auth_ack` 必须回显同一
//!   `device_sid`，否则连接立即拒绝且**零后续帧**。`client_ts` 是 epoch **秒**（官方单位，
//!   不是 Tuvis DTO 的毫秒）。
//! - **waiting 不是人工批准**：`waiting` 是合法配对前态（设备不可路由/离线），唯一已配对
//!   状态是 `matched`；`waiting` 在 Tuvis 侧映射为 [`ConnectionState::Connecting`]。
//! - **30s 路由窗**：`auth_ack(waiting)` 后 30s 内未 `matched` → 终态 `offline`，零重连。
//! - **心跳**：`pair_status_query{client_ts}` 基础间隔 5,000ms ± 1,000ms jitter；每次
//!   `auth_ack`/`pair_status_ack` 重置 15,000ms 看门狗，15s 无 ack → `offline` 并断连接。
//!   断开/KICKED/任何终态都取消计时器，零自动重连（依据固定源码 `heartbeat.ts` 与
//!   `heartbeatTiming.ts`；本模块是长驻连接，不以「短时探针无心跳」冒充可用）。
//! - **数据面门禁**：`matched` 之前不发送任何 `data` 帧；[`RelayClient::send_data`] 在未
//!   配对时返回错误且**写零帧**。收帧只在已配对时上报 [`RelayEvent::Data`]。
//! - **错误卫生**：WS/HTTP 原始错误可能含 URL/token；一律丢弃原文，只映射 [`codes`] 的
//!   冻结码。SID/MID/token/上游文案不进入错误、`Debug` 或事件。
//! - **代际**：显式连接/断开推进 `generation`；迟到的 token 结果、WS 握手结果与旧帧一律
//!   丢弃（不回退状态、不开第二条 socket）。这也是 T1.1 的衔接约束：clear 之后代际已变的
//!   relay token 不得再建立活连接。
//!
//! # 并发
//!
//! `connect` 用一把异步单飞锁串行化「开始新连接」；**断开不等待该锁**，只持状态锁推进
//! 代际并通过 oneshot 通知连接任务退出，因此不会被网络等待拖住。连接任务独占 socket
//! （split 成 sink/source），`send_data` 只经有界命令通道投递，绝不并发写 socket。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

use super::auth::AuthService;
use super::config::validate_official_url;
use super::types::{codes, ConnectionState, ConnectionView, ZResult, ZcodeError};

/// 官方 relay WebSocket 路径（固定源码 `shared/web-remote-relay/contract.ts:102`）。
pub const OFFICIAL_RELAY_WS_PATH: &str = "/api/v1/remote/ws2";
/// 官方 relay WebSocket origin（生产固定值，无配置/IPC 入口；https origin 的 wss 对应）。
pub const OFFICIAL_RELAY_WS_ORIGIN: &str = "wss://zcode.z.ai";

/// 官方 relay 升级后的客户端流（ws/wss 统一为 `MaybeTlsStream`）。
type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
/// 连接任务独占的写半边。
type WsSink = futures::stream::SplitSink<WsStream, Message>;
/// 连接任务独占的读半边。
type WsSource = futures::stream::SplitStream<WsStream>;

/// 从**已保存的官方链接**解析出的连接目标（唯一生产来源，不接受手机自报）。
///
/// 不实现 `Debug`/`Serialize`：`sid`/`mid` 只是定位信息，不写日志、不下发手机。
#[derive(Clone, PartialEq, Eq)]
pub struct DeviceRoute {
    /// 官方链接 `sid` 参数（`auth_init` 的 `device_sid`）。
    pub sid: String,
    /// 官方链接 `mid` 参数（条件性路由；缺失时连接仍可建立但通常保持 `waiting`）。
    pub mid: Option<String>,
    /// 官方链接 `name` 参数（仅供显示，不参与绑定）。
    pub label: Option<String>,
}

impl DeviceRoute {
    /// 从已保存的官方链接解析目标；格式不合法/缺少 `sid` → `needs_config`。
    ///
    /// 只接受与 A4 相同的官方链接格式（`https` + `zcode.z.ai` + `/remote/vN` + 唯一非空
    /// `sid`）；`mid`/`name` 为空白时按缺失处理（官方 `deviceMid?.trim()` 语义）。
    pub fn from_saved_url(url: &str) -> ZResult<Self> {
        let normalized =
            validate_official_url(url).map_err(|_| ZcodeError::of(codes::NEEDS_CONFIG))?;
        let Some(normalized) = normalized else {
            return Err(ZcodeError::of(codes::NEEDS_CONFIG));
        };
        let parsed =
            reqwest::Url::parse(&normalized).map_err(|_| ZcodeError::of(codes::NEEDS_CONFIG))?;
        let mut sid: Option<String> = None;
        let mut mid: Option<String> = None;
        let mut label: Option<String> = None;
        for (key, value) in parsed.query_pairs() {
            match key.as_ref() {
                "sid" => sid = Some(value.into_owned()),
                "mid" => mid = Some(value.into_owned()),
                "name" => label = Some(value.into_owned()),
                _ => {}
            }
        }
        let sid = sid
            .filter(|sid| !sid.trim().is_empty())
            .ok_or_else(|| ZcodeError::of(codes::NEEDS_CONFIG))?;
        Ok(Self {
            sid,
            mid: mid.filter(|mid| !mid.trim().is_empty()),
            label: label.filter(|label| !label.trim().is_empty()),
        })
    }
}

/// relay 事件（Hub 的唯一输入口；绝无 JWT/token/URL）。
pub enum RelayEvent {
    /// 已配对的 `data` 帧 payload（官方 `{type:"data",payload}` 的 `payload`）。
    Data(Value),
    /// 连接状态变化（携带 generation，消费方必须丢弃旧代际事件）。
    State(ConnectionView),
}

/// 连接任务命令（唯一写 socket 的路径；有界通道，不在状态锁内 await 网络）。
enum Command {
    SendData {
        payload: Value,
        reply: oneshot::Sender<ZResult<()>>,
    },
}

/// 握手结果（`connect` 等它返回）。
enum HandshakeOutcome {
    Established(ConnectionView),
    Failed(ZcodeError),
}

/// 生产时序（可测试注入；默认值全部来自官方源码）。
#[derive(Debug, Clone, Copy)]
struct RelayTiming {
    /// `pair_status_query` 基础间隔：5,000ms。
    heartbeat_interval: Duration,
    /// 心跳 jitter：±1,000ms。
    heartbeat_jitter: Duration,
    /// `auth_ack`/`pair_status_ack` 看门狗：15,000ms。
    ack_timeout: Duration,
    /// `waiting` 路由窗：30,000ms。
    pair_window: Duration,
    /// WS 升级超时（生产 15s；测试可缩短）。
    connect_timeout: Duration,
}

impl RelayTiming {
    /// 官方默认时序。
    const fn official() -> Self {
        Self {
            heartbeat_interval: Duration::from_millis(5_000),
            heartbeat_jitter: Duration::from_millis(1_000),
            ack_timeout: Duration::from_millis(15_000),
            pair_window: Duration::from_secs(30),
            connect_timeout: Duration::from_millis(15_000),
        }
    }
}

/// 单 relay 连接的客户端（进程内单实例；跨任务共享）。
///
/// 生产构造只有 [`RelayClient::new`]（固定官方端点）；[`RelayClient::with_endpoint`] 仅
/// 测试可见，不注册配置/IPC。
pub struct RelayClient {
    shared: Arc<Shared>,
}

/// 跨连接共享的可见状态（事件通道、端点、当前视图）。
struct Shared {
    auth: Arc<AuthService>,
    events: mpsc::Sender<RelayEvent>,
    ws_origin: String,
    timing: RelayTiming,
    state: Mutex<InnerState>,
    /// 单飞锁：同一时刻只允许一个「开始新连接」流程（含 token 与 WS 升级）。
    connect_lock: tokio::sync::Mutex<()>,
    /// 状态事件排序锁：状态更新与事件发送在它下面线性化，避免旧代际事件后到覆盖新状态。
    emit_lock: tokio::sync::Mutex<()>,
}

struct InnerState {
    /// 显式连接/断开推进；迟到结果依此丢弃。
    generation: u64,
    view: ConnectionView,
    live: Option<Arc<LiveConnection>>,
}

/// 当前活连接的写入口与取消开关（只在代际匹配时存在）。
struct LiveConnection {
    /// 唯一的「已配对」标志；数据写入门禁在 actor 内再次复核。
    paired: Arc<AtomicBool>,
    commands: mpsc::Sender<Command>,
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
}

impl LiveConnection {
    /// 通知连接任务退出（可为重复调用；不阻塞、不需要异步锁）。
    fn signal_shutdown(&self) {
        let sender = self
            .shutdown
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(());
        }
    }
}

/// 连接任务（独占 socket 的 sink/source 与全部计时器）。
struct ConnectionTask {
    shared: Arc<Shared>,
    sink: WsSink,
    source: WsSource,
    device: DeviceRoute,
    generation: u64,
    paired: Arc<AtomicBool>,
    commands: mpsc::Receiver<Command>,
    shutdown: oneshot::Receiver<()>,
    handshake: Option<oneshot::Sender<HandshakeOutcome>>,
}

/// 连接任务内的握手/心跳计时状态。
struct SessionState {
    authenticated: bool,
    matched: bool,
    auth_deadline: Option<Instant>,
    pair_deadline: Option<Instant>,
    heartbeat_due: Option<Instant>,
    ack_deadline: Option<Instant>,
}

impl RelayClient {
    /// 生产工厂：固定 `wss://zcode.z.ai`，不读取任何配置。
    pub fn new(auth: Arc<AuthService>, events: mpsc::Sender<RelayEvent>) -> Self {
        Self::with_parts(
            auth,
            events,
            OFFICIAL_RELAY_WS_ORIGIN.to_owned(),
            RelayTiming::official(),
        )
    }

    /// 测试专用端点注入（生产不得调用；不注册任何配置/IPC 入口）。
    #[cfg(test)]
    pub fn with_endpoint(
        auth: Arc<AuthService>,
        events: mpsc::Sender<RelayEvent>,
        ws_origin: String,
    ) -> Self {
        Self::with_parts(auth, events, ws_origin, RelayTiming::official())
    }

    /// 测试专用时序注入（心跳/路由窗/看门狗缩短，测试不真等 15s/30s）。
    #[cfg(test)]
    fn with_endpoint_timing(
        auth: Arc<AuthService>,
        events: mpsc::Sender<RelayEvent>,
        ws_origin: String,
        timing: RelayTiming,
    ) -> Self {
        Self::with_parts(auth, events, ws_origin, timing)
    }

    fn with_parts(
        auth: Arc<AuthService>,
        events: mpsc::Sender<RelayEvent>,
        ws_origin: String,
        timing: RelayTiming,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                auth,
                events,
                ws_origin,
                timing,
                state: Mutex::new(InnerState {
                    generation: 0,
                    view: ConnectionView {
                        state: ConnectionState::Offline,
                        generation: 0,
                        reason_code: None,
                        device_label: None,
                    },
                    live: None,
                }),
                connect_lock: tokio::sync::Mutex::new(()),
                emit_lock: tokio::sync::Mutex::new(()),
            }),
        }
    }

    /// 建立单条 relay 连接（先取新 token，再走官方握手）。
    ///
    /// - 已有活连接（`connecting`/`paired`）时是幂等的：返回当前视图，不取 token、
    ///   不开第二条 socket（同 role 第二条 terminal socket 会被官方相互踢掉）。
    /// - 返回 `Ok(Connecting)` 表示 `auth_ack(waiting)`（合法配对前态），连接保持并继续
    ///   心跳等待 `matched`；`Ok(Paired)` 表示已配对。
    /// - `Err(code)` 表示 token/网络/协议失败；对应状态已通过 [`RelayEvent::State`] 上报。
    /// - 迟到的 token/握手结果（代际已变）返回 `stale_generation`，绝不回退状态。
    pub async fn connect(&self, device: DeviceRoute) -> ZResult<ConnectionView> {
        if device.sid.trim().is_empty() {
            return Err(ZcodeError::of(codes::NEEDS_CONFIG));
        }
        // 单飞：并发 connect 只允许一个真正开始新连接；等待者随后复用活连接。
        let _single_flight = self.shared.connect_lock.lock().await;

        {
            let inner = self.shared.lock();
            if inner.live.is_some()
                && matches!(
                    inner.view.state,
                    ConnectionState::Connecting | ConnectionState::Paired
                )
            {
                return Ok(inner.view.clone());
            }
        }

        let connecting = self.shared.begin_attempt(&device);
        let generation = connecting.generation;
        self.shared.announce_connecting(connecting).await;

        // 1) 每次连接现取一次性 terminal token（不缓存、用后即弃）。
        let token_result = self.shared.auth.terminal_token().await;
        if !self.shared.is_current(generation) {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        let token = match token_result {
            Ok(token) => token,
            Err(err) => {
                let state = state_for_code(&err.code);
                self.shared.fail_attempt(generation, state, &err.code).await;
                if !self.shared.is_current(generation) {
                    return Err(ZcodeError::of(codes::STALE_GENERATION));
                }
                return Err(err);
            }
        };

        // 2) 官方升级 URL（token 只在 URL 里，随后立即丢弃持有）。
        let url = match build_relay_url(&self.shared.ws_origin, &token.value, &device) {
            Ok(url) => url,
            Err(err) => {
                self.shared
                    .fail_attempt(generation, ConnectionState::Unknown, codes::UNKNOWN)
                    .await;
                return Err(err);
            }
        };
        drop(token);

        // 3) WS 升级（有界等待；原始错误可能含 URL，一律丢弃只映射常量码）。
        let connected = tokio::time::timeout(
            self.shared.timing.connect_timeout,
            tokio_tungstenite::connect_async(url),
        )
        .await;
        let socket = match connected {
            Ok(Ok((socket, _response))) => socket,
            Ok(Err(_)) | Err(_) => {
                self.shared
                    .fail_attempt(generation, ConnectionState::Offline, codes::OFFLINE)
                    .await;
                if !self.shared.is_current(generation) {
                    return Err(ZcodeError::of(codes::STALE_GENERATION));
                }
                return Err(ZcodeError::offline());
            }
        };
        if !self.shared.is_current(generation) {
            close_ws(socket).await;
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }

        // 4) 登记活连接并启动唯一 actor（socket 的 sink/source 都由它独占）。
        let paired = Arc::new(AtomicBool::new(false));
        let (commands, commands_rx) = mpsc::channel(8);
        let (shutdown, shutdown_rx) = oneshot::channel();
        let live = Arc::new(LiveConnection {
            paired: paired.clone(),
            commands,
            shutdown: Mutex::new(Some(shutdown)),
        });
        if !self.shared.install_live(generation, live.clone()) {
            live.signal_shutdown();
            close_ws(socket).await;
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        let (handshake, handshake_rx) = oneshot::channel();
        let (sink, source) = socket.split();
        tokio::spawn(run_connection(ConnectionTask {
            shared: self.shared.clone(),
            sink,
            source,
            device,
            generation,
            paired,
            commands: commands_rx,
            shutdown: shutdown_rx,
            handshake: Some(handshake),
        }));

        // 5) 等握手结果；代际在返回前再核一次（旧结果不得覆盖新状态）。
        match handshake_rx.await {
            Ok(HandshakeOutcome::Established(view)) => {
                if !self.shared.is_current(generation) {
                    return Err(ZcodeError::of(codes::STALE_GENERATION));
                }
                Ok(view)
            }
            Ok(HandshakeOutcome::Failed(err)) => {
                if !self.shared.is_current(generation) {
                    return Err(ZcodeError::of(codes::STALE_GENERATION));
                }
                Err(err)
            }
            Err(_) => {
                if !self.shared.is_current(generation) {
                    Err(ZcodeError::of(codes::STALE_GENERATION))
                } else {
                    Err(ZcodeError::offline())
                }
            }
        }
    }

    /// 发送一条 `data` 帧；仅 `matched` 后允许，未配对返回错误且不写帧。
    pub async fn send_data(&self, payload: Value) -> ZResult<()> {
        let live = {
            let inner = self.shared.lock();
            match &inner.live {
                Some(live)
                    if inner.view.state == ConnectionState::Paired
                        && live.paired.load(Ordering::SeqCst) =>
                {
                    live.clone()
                }
                _ => return Err(ZcodeError::of(codes::NOT_READY)),
            }
        };
        let (reply, reply_rx) = oneshot::channel();
        if live
            .commands
            .send(Command::SendData { payload, reply })
            .await
            .is_err()
        {
            return Err(ZcodeError::offline());
        }
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => Err(ZcodeError::offline()),
        }
    }

    /// 显式断开：推进 generation、取消所有计时器并关闭 socket；无自动重连。
    ///
    /// 不等待 `connect` 的单飞锁，因此即使连接流程正卡在网络等待也能立即返回；
    /// 在途结果随后因代际不符被丢弃。
    pub async fn disconnect(&self) {
        let live = {
            let _emit = self.shared.emit_lock.lock().await;
            let (view, live) = {
                let mut inner = self.shared.lock();
                inner.generation = inner.generation.wrapping_add(1);
                inner.view.state = ConnectionState::Offline;
                inner.view.reason_code = None;
                inner.view.generation = inner.generation;
                (inner.view.clone(), inner.live.take())
            };
            if let Some(live) = &live {
                live.signal_shutdown();
            }
            let _ = self.shared.events.send(RelayEvent::State(view)).await;
            live
        };
        drop(live);
    }

    /// 测试用当前视图（生产消费方走事件/上层 `connection()`）。
    #[cfg(test)]
    fn view(&self) -> ConnectionView {
        self.shared.current_view()
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, InnerState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn is_current(&self, generation: u64) -> bool {
        self.lock().generation == generation
    }

    fn current_view(&self) -> ConnectionView {
        self.lock().view.clone()
    }

    /// 开始一次新连接：推进代际、登记 `connecting` 视图、停掉任何遗留活连接。
    fn begin_attempt(&self, device: &DeviceRoute) -> ConnectionView {
        let mut inner = self.lock();
        inner.generation = inner.generation.wrapping_add(1);
        let view = ConnectionView {
            state: ConnectionState::Connecting,
            generation: inner.generation,
            reason_code: None,
            device_label: device.label.clone(),
        };
        inner.view = view.clone();
        if let Some(previous) = inner.live.take() {
            previous.signal_shutdown();
        }
        view
    }

    fn install_live(&self, generation: u64, live: Arc<LiveConnection>) -> bool {
        let mut inner = self.lock();
        if inner.generation != generation {
            return false;
        }
        inner.live = Some(live);
        true
    }

    fn clear_live(&self, generation: u64) {
        let mut inner = self.lock();
        if inner.generation == generation {
            inner.live = None;
        }
    }

    /// 发出 `connecting` 事件（若代际/视图已被更新的动作取代则丢弃）。
    async fn announce_connecting(&self, view: ConnectionView) {
        let _emit = self.emit_lock.lock().await;
        {
            let inner = self.lock();
            if inner.generation != view.generation || inner.view != view {
                return;
            }
        }
        let _ = self.events.send(RelayEvent::State(view)).await;
    }

    /// 线性化状态更新与事件发送；只有状态真的变化才发事件。
    async fn transition_state(
        &self,
        generation: u64,
        state: ConnectionState,
        reason: Option<&str>,
    ) -> bool {
        let _emit = self.emit_lock.lock().await;
        let view = {
            let mut inner = self.lock();
            if inner.generation != generation {
                return false;
            }
            let reason_code = reason.map(str::to_owned);
            let changed = inner.view.state != state || inner.view.reason_code != reason_code;
            inner.view.state = state;
            inner.view.reason_code = reason_code;
            if let Some(live) = &inner.live {
                live.paired
                    .store(state == ConnectionState::Paired, Ordering::SeqCst);
            }
            if !changed {
                return false;
            }
            inner.view.clone()
        };
        let _ = self.events.send(RelayEvent::State(view)).await;
        true
    }

    async fn emit_data(&self, generation: u64, payload: Value) {
        // 数据与状态事件共用排序锁：断开/终态之后绝不把旧代际 data 上报给桥。
        let _emit = self.emit_lock.lock().await;
        {
            let inner = self.lock();
            if inner.generation != generation || inner.view.state != ConnectionState::Paired {
                return;
            }
        }
        let _ = self.events.send(RelayEvent::Data(payload)).await;
    }

    async fn fail_attempt(&self, generation: u64, state: ConnectionState, reason: &str) {
        self.transition_state(generation, state, Some(reason)).await;
        self.clear_live(generation);
    }
}

impl ConnectionTask {
    /// 写一条已配对的数据帧（actor 内复核 paired，避免 check-then-write 漂移）。
    async fn send_data_frame(&mut self, payload: Value, matched: bool) -> ZResult<()> {
        if !matched || !self.paired.load(Ordering::SeqCst) {
            return Err(ZcodeError::of(codes::NOT_READY));
        }
        let frame = json!({
            "type": "data",
            "payload": payload,
            "client_ts": now_secs(),
        });
        send_json(&mut self.sink, &frame)
            .await
            .map_err(|_| ZcodeError::offline())
    }

    /// 终态收口：置非配对、上报冻结状态、唤醒 connect、清活连接、关 socket。
    async fn fail(&mut self, state: ConnectionState, reason: &'static str, error: ZcodeError) {
        self.paired.store(false, Ordering::SeqCst);
        self.shared
            .transition_state(self.generation, state, Some(reason))
            .await;
        self.complete(HandshakeOutcome::Failed(error));
        self.shared.clear_live(self.generation);
        close_ws_sink(&mut self.sink).await;
    }

    /// 握手成功（waiting/matched）：上报状态并唤醒 connect。
    async fn establish(&mut self, state: ConnectionState) {
        self.shared
            .transition_state(self.generation, state, None)
            .await;
        let view = self.shared.current_view();
        self.complete(HandshakeOutcome::Established(view));
    }

    fn complete(&mut self, outcome: HandshakeOutcome) {
        if let Some(sender) = self.handshake.take() {
            let _ = sender.send(outcome);
        }
    }
}

/// 连接 actor：独占 socket、心跳与全部计时器；任何出口都终态且不重连。
async fn run_connection(mut task: ConnectionTask) {
    let generation = task.generation;
    // 任务真正开跑前再核一次代际：spawn 与断开竞态时不得再发 auth_init。
    if !task.shared.is_current(generation) {
        close_ws_sink(&mut task.sink).await;
        task.shared.clear_live(generation);
        return;
    }
    let auth_init = json!({
        "type": "auth_init",
        "device_sid": task.device.sid,
        "meta": {
            "platform": "tuvis",
            "version": env!("CARGO_PKG_VERSION"),
            "name": "tuvis-zcode-bridge",
        },
        "client_ts": now_secs(),
    });
    if send_json(&mut task.sink, &auth_init).await.is_err() {
        task.fail(
            ConnectionState::Offline,
            codes::OFFLINE,
            ZcodeError::offline(),
        )
        .await;
        return;
    }

    let timing = task.shared.timing;
    let mut session = SessionState {
        authenticated: false,
        matched: false,
        auth_deadline: Some(Instant::now() + timing.ack_timeout),
        pair_deadline: None,
        heartbeat_due: None,
        ack_deadline: None,
    };

    loop {
        let now = Instant::now();
        if !session.authenticated {
            if session
                .auth_deadline
                .is_some_and(|deadline| now >= deadline)
            {
                // 升级后迟迟拿不到 auth_ack：按离线终态收口（官方握手定时器同源 15s）。
                task.fail(
                    ConnectionState::Offline,
                    codes::OFFLINE,
                    ZcodeError::offline(),
                )
                .await;
                return;
            }
        } else {
            if session.ack_deadline.is_some_and(|deadline| now >= deadline)
                || session
                    .pair_deadline
                    .is_some_and(|deadline| now >= deadline)
            {
                // 心跳看门狗超时或 30s 路由窗超时：offline 并断连接，零重连。
                task.fail(
                    ConnectionState::Offline,
                    codes::OFFLINE,
                    ZcodeError::offline(),
                )
                .await;
                return;
            }
            if session
                .heartbeat_due
                .is_some_and(|deadline| now >= deadline)
            {
                let query = json!({
                    "type": "pair_status_query",
                    "client_ts": now_secs(),
                });
                if send_json(&mut task.sink, &query).await.is_err() {
                    task.fail(
                        ConnectionState::Offline,
                        codes::OFFLINE,
                        ZcodeError::offline(),
                    )
                    .await;
                    return;
                }
                session.heartbeat_due = Some(Instant::now() + jittered_heartbeat_delay(&timing));
                continue;
            }
        }

        let next_wake = next_wake(&session);
        let sleep = async move {
            match next_wake {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = &mut task.shutdown => {
                // 显式断开/换代：状态已由 disconnect/begin_attempt 处理，这里只负责收尸。
                close_ws_sink(&mut task.sink).await;
                task.shared.clear_live(generation);
                return;
            }
            command = task.commands.recv() => {
                match command {
                    Some(Command::SendData { payload, reply }) => {
                        let result = task.send_data_frame(payload, session.matched).await;
                        let _ = reply.send(result);
                    }
                    None => {
                        close_ws_sink(&mut task.sink).await;
                        task.shared.clear_live(generation);
                        return;
                    }
                }
            }
            incoming = task.source.next() => {
                match incoming {
                    Some(Ok(message)) => {
                        if !handle_incoming(&mut task, message, &mut session).await {
                            return;
                        }
                    }
                    _ => {
                        // 网络掉线/对端关闭：offline 终态，不重连。
                        task.fail(
                            ConnectionState::Offline,
                            codes::OFFLINE,
                            ZcodeError::offline(),
                        )
                        .await;
                        return;
                    }
                }
            }
            _ = sleep => { continue; }
        }
    }
}

/// 处理一条入站消息；返回 `false` 表示 actor 已终态退出。
async fn handle_incoming(
    task: &mut ConnectionTask,
    message: Message,
    session: &mut SessionState,
) -> bool {
    let value = match message {
        Message::Text(text) => serde_json::from_str::<Value>(text.as_str()).ok(),
        Message::Binary(bytes) => serde_json::from_slice::<Value>(&bytes).ok(),
        Message::Ping(payload) => {
            let _ = task.sink.send(Message::Pong(payload)).await;
            return true;
        }
        Message::Pong(_) | Message::Frame(_) => return true,
        Message::Close(_) => {
            task.fail(
                ConnectionState::Offline,
                codes::OFFLINE,
                ZcodeError::offline(),
            )
            .await;
            return false;
        }
    };
    let Some(value) = value else {
        // 非 JSON 帧：官方客户端同样忽略，不据此断言连接损坏。
        return true;
    };

    match value.get("type").and_then(Value::as_str) {
        Some("auth_ack") => {
            if session.authenticated {
                // 官方只在 authenticating 态消费 auth_ack（重复帧忽略）。
                return true;
            }
            let echoed = value.get("device_sid").and_then(Value::as_str);
            if echoed != Some(task.device.sid.as_str()) {
                // 回显不符：拒绝连接，且不进入任何后续帧（心跳/data 都不发）。
                task.fail(
                    ConnectionState::Unknown,
                    codes::UNKNOWN,
                    ZcodeError::unknown(),
                )
                .await;
                return false;
            }
            let Some(pair_status) = value.get("pair_status").and_then(Value::as_str) else {
                task.fail(
                    ConnectionState::Unknown,
                    codes::UNKNOWN,
                    ZcodeError::unknown(),
                )
                .await;
                return false;
            };
            let now = Instant::now();
            session.authenticated = true;
            session.auth_deadline = None;
            session.ack_deadline = Some(now + task.shared.timing.ack_timeout);
            session.heartbeat_due = Some(now + jittered_heartbeat_delay(&task.shared.timing));
            match pair_status {
                "matched" => {
                    session.matched = true;
                    session.pair_deadline = None;
                    task.establish(ConnectionState::Paired).await;
                }
                "waiting" => {
                    // waiting 是合法配对前态：保持连接 + 心跳，等待 matched（路由窗 30s）。
                    session.pair_deadline = Some(now + task.shared.timing.pair_window);
                    task.establish(ConnectionState::Connecting).await;
                }
                _ => {
                    task.fail(
                        ConnectionState::Unknown,
                        codes::UNKNOWN,
                        ZcodeError::unknown(),
                    )
                    .await;
                    return false;
                }
            }
        }
        Some("pair_status_ack") => {
            if !session.authenticated {
                return true;
            }
            let Some(pair_status) = value.get("pair_status").and_then(Value::as_str) else {
                return true;
            };
            let now = Instant::now();
            session.ack_deadline = Some(now + task.shared.timing.ack_timeout);
            match pair_status {
                "matched" => {
                    session.matched = true;
                    session.pair_deadline = None;
                    task.shared
                        .transition_state(task.generation, ConnectionState::Paired, None)
                        .await;
                }
                "waiting" => {
                    if session.matched {
                        // 已配对后降级为 waiting 是官方认可的合法状态；Tuvis 不自动重连，
                        // 重新进入路由窗，窗口内仍未 matched 即 offline 终态。
                        session.matched = false;
                        task.shared
                            .transition_state(task.generation, ConnectionState::Connecting, None)
                            .await;
                    }
                    session
                        .pair_deadline
                        .get_or_insert(now + task.shared.timing.pair_window);
                }
                _ => {
                    task.fail(
                        ConnectionState::Unknown,
                        codes::UNKNOWN,
                        ZcodeError::unknown(),
                    )
                    .await;
                    return false;
                }
            }
        }
        Some("data") => {
            if session.authenticated && session.matched {
                if let Some(payload) = value.get("payload") {
                    task.shared
                        .emit_data(task.generation, payload.clone())
                        .await;
                }
            }
        }
        Some("error") => {
            let code = value.get("code").and_then(Value::as_str).unwrap_or("");
            let (state, reason, error) = match code {
                "KICKED" => (
                    ConnectionState::Conflict,
                    codes::CONFLICT,
                    ZcodeError::of(codes::CONFLICT),
                ),
                "AUTH_FAILED" => (
                    ConnectionState::NeedsAuth,
                    codes::NEEDS_AUTH,
                    ZcodeError::needs_auth(),
                ),
                "WRONG_PARAM" => (
                    ConnectionState::Unknown,
                    codes::UNKNOWN,
                    ZcodeError::unknown(),
                ),
                "INTERNAL" | "DEVICE_OFFLINE" => (
                    ConnectionState::Offline,
                    codes::OFFLINE,
                    ZcodeError::offline(),
                ),
                _ => (
                    ConnectionState::Unknown,
                    codes::UNKNOWN,
                    ZcodeError::unknown(),
                ),
            };
            task.fail(state, reason, error).await;
            return false;
        }
        _ => {}
    }
    true
}

fn next_wake(session: &SessionState) -> Option<Instant> {
    if !session.authenticated {
        return session.auth_deadline;
    }
    let mut next = session.ack_deadline;
    for candidate in [session.pair_deadline, session.heartbeat_due]
        .into_iter()
        .flatten()
    {
        next = Some(match next {
            Some(current) => current.min(candidate),
            None => candidate,
        });
    }
    next
}

fn state_for_code(code: &str) -> ConnectionState {
    match code {
        codes::NEEDS_CONFIG => ConnectionState::NeedsConfig,
        codes::NEEDS_AUTH => ConnectionState::NeedsAuth,
        codes::OFFLINE => ConnectionState::Offline,
        codes::CONFLICT => ConnectionState::Conflict,
        _ => ConnectionState::Unknown,
    }
}

/// 构造官方 WS 升级 URL：`{origin}/api/v1/remote/ws2?token=...&mid=...`。
fn build_relay_url(origin: &str, token: &str, device: &DeviceRoute) -> ZResult<String> {
    let base = format!("{}{}", origin.trim_end_matches('/'), OFFICIAL_RELAY_WS_PATH);
    let mut url = reqwest::Url::parse(&base).map_err(|_| ZcodeError::unknown())?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("token", token);
        if let Some(mid) = device.mid.as_deref().filter(|mid| !mid.trim().is_empty()) {
            query.append_pair("mid", mid);
        }
    }
    Ok(url.to_string())
}

/// 下一次心跳延迟：`interval ± jitter`（官方 `heartbeatTiming` 公式，下界至少 1ms）。
fn jittered_heartbeat_delay(timing: &RelayTiming) -> Duration {
    let base = u64::try_from(timing.heartbeat_interval.as_millis())
        .unwrap_or(u64::MAX)
        .max(1);
    let jitter = u64::try_from(timing.heartbeat_jitter.as_millis()).unwrap_or(0);
    if jitter == 0 {
        return Duration::from_millis(base);
    }
    let low = base.saturating_sub(jitter).max(1);
    let high = base.saturating_add(jitter);
    Duration::from_millis(rand::random_range(low..=high))
}

async fn send_json(sink: &mut WsSink, value: &Value) -> Result<(), ()> {
    let text = serde_json::to_string(value).map_err(|_| ())?;
    sink.send(Message::Text(text.into())).await.map_err(|_| ())
}

async fn close_ws_sink(sink: &mut WsSink) {
    let _ = sink.send(Message::Close(None)).await;
}

async fn close_ws(mut socket: WsStream) {
    let _ = socket.send(Message::Close(None)).await;
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::super::auth::{AuthService, OAuthProvider, OpenAuthorize};
    use super::*;
    use base64::Engine as _;
    use futures::{SinkExt, StreamExt};
    use std::collections::{HashMap, VecDeque};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::{mpsc, Semaphore};
    use tokio::task::JoinHandle;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    use tokio_tungstenite::tungstenite::Message;

    // ───────────────────────── 合成夹具（无真实账号/网络/DB/链接） ─────────────────────────

    const SYNTHETIC_CODE: &str = "synthetic-code-0001";
    const SYNTHETIC_TOKEN_1: &str = "syntheticrelaytoken01";
    const SYNTHETIC_TOKEN_2: &str = "syntheticrelaytoken02";
    const SYNTHETIC_SID: &str = "synthetic-sid-0001";
    const SYNTHETIC_MID: &str = "synthetic-mid-0001";

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

    fn test_client() -> reqwest::Client {
        // 与 auth 测试同口径：显式禁用系统代理，避免本机代理劫持 127.0.0.1
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("测试 HTTP 客户端")
    }

    fn route(mid: Option<&str>) -> DeviceRoute {
        DeviceRoute {
            sid: SYNTHETIC_SID.to_string(),
            mid: mid.map(str::to_string),
            label: Some("Synthetic Device".to_string()),
        }
    }

    fn fast_timing(
        heartbeat_interval: Duration,
        ack_timeout: Duration,
        pair_window: Duration,
    ) -> RelayTiming {
        RelayTiming {
            heartbeat_interval,
            heartbeat_jitter: Duration::ZERO,
            ack_timeout,
            pair_window,
            connect_timeout: Duration::from_secs(2),
        }
    }

    async fn wait_until(mut condition: impl FnMut() -> bool, what: &str) {
        for _ in 0..800 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("等待条件超时：{what}");
    }

    // ───────────────────────── fake 授权/令牌 HTTP ─────────────────────────

    struct FakeAuthState {
        jwt: String,
        exchange_count: usize,
        token_count: usize,
        tokens: VecDeque<String>,
        token_status: u16,
        token_gate: Option<Arc<Semaphore>>,
    }

    struct FakeAuthHttp {
        origin: String,
        state: Arc<Mutex<FakeAuthState>>,
        task: JoinHandle<()>,
    }

    impl Drop for FakeAuthHttp {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl FakeAuthHttp {
        async fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("fake 端口");
            let port = listener.local_addr().expect("fake 地址").port();
            let state = Arc::new(Mutex::new(FakeAuthState {
                jwt: valid_jwt(),
                exchange_count: 0,
                token_count: 0,
                tokens: VecDeque::from([SYNTHETIC_TOKEN_1.to_string()]),
                token_status: 200,
                token_gate: None,
            }));
            let task = tokio::spawn(serve_auth(listener, state.clone()));
            Self {
                origin: format!("http://127.0.0.1:{port}"),
                state,
                task,
            }
        }

        fn set_jwt(&self, jwt: impl Into<String>) {
            self.state.lock().unwrap().jwt = jwt.into();
        }

        fn set_tokens(&self, tokens: &[&str]) {
            self.state.lock().unwrap().tokens = tokens.iter().map(|t| (*t).to_string()).collect();
        }

        fn set_token_status(&self, status: u16) {
            self.state.lock().unwrap().token_status = status;
        }

        /// 挂起下一次终端 token 响应（测试放行前不返回），返回放行句柄。
        fn hold_next_token(&self) -> Arc<Semaphore> {
            let gate = Arc::new(Semaphore::new(0));
            self.state.lock().unwrap().token_gate = Some(gate.clone());
            gate
        }

        fn exchange_count(&self) -> usize {
            self.state.lock().unwrap().exchange_count
        }

        fn token_count(&self) -> usize {
            self.state.lock().unwrap().token_count
        }
    }

    async fn serve_auth(listener: TcpListener, state: Arc<Mutex<FakeAuthState>>) {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let state = state.clone();
            tokio::spawn(async move {
                handle_auth_request(stream, state).await;
            });
        }
    }

    async fn handle_auth_request(mut stream: TcpStream, state: Arc<Mutex<FakeAuthState>>) {
        let Some((_method, path, _body)) = read_http_request(&mut stream).await else {
            return;
        };
        let (status, body, gate) = {
            let mut state = state.lock().unwrap();
            if path.starts_with(super::super::auth::OAUTH_TOKEN_PATH) {
                state.exchange_count += 1;
                (
                    200,
                    format!(
                        r#"{{"code":0,"msg":"ok","data":{{"token":"{}"}}}}"#,
                        state.jwt
                    ),
                    None,
                )
            } else if path.starts_with(super::super::auth::RELAY_TOKEN_PATH) {
                state.token_count += 1;
                let gate = state.token_gate.take();
                if state.token_status == 401 {
                    (
                        401,
                        r#"{"code":401,"msg":"synthetic-upstream"}"#.to_string(),
                        gate,
                    )
                } else {
                    let token = state
                        .tokens
                        .pop_front()
                        .unwrap_or_else(|| SYNTHETIC_TOKEN_1.to_string());
                    (
                        200,
                        format!(
                            r#"{{"code":0,"msg":"ok","data":{{"token":"{token}","expires_in":60}}}}"#
                        ),
                        gate,
                    )
                }
            } else {
                (404, "{}".to_string(), None)
            }
        };
        if let Some(gate) = gate {
            let _ = gate.acquire().await;
        }
        let head = format!(
            "HTTP/1.1 {status} FAKE\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream.write_all(body.as_bytes()).await;
        let _ = stream.flush().await;
    }

    async fn read_http_request(stream: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 2048];
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
        let request_line = head.lines().next()?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next()?.to_string();
        let path = parts.next()?.to_string();
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
        Some((method, path, body))
    }

    async fn wait_for_authorize_url(slot: &Arc<Mutex<Option<String>>>) -> String {
        for _ in 0..600 {
            if let Some(url) = slot.lock().unwrap().clone() {
                return url;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("授权 URL 未在时限内产生");
    }

    /// 走一次完整 fake OAuth（BigModel 通道），返回已持有内存 JWT 的 AuthService。
    async fn authorized_service(fake: &FakeAuthHttp) -> Arc<AuthService> {
        let auth = Arc::new(AuthService::with_endpoints(
            test_client(),
            fake.origin.clone(),
            fake.origin.clone(),
        ));
        let slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let open_slot = slot.clone();
        let open: OpenAuthorize = Arc::new(move |url: String| {
            *open_slot.lock().unwrap() = Some(url);
            Ok(())
        });
        let task = tokio::spawn({
            let auth = auth.clone();
            async move { auth.authorize(OAuthProvider::Bigmodel, open).await }
        });
        let authorize_url = wait_for_authorize_url(&slot).await;
        let parsed = reqwest::Url::parse(&authorize_url).expect("授权 URL");
        let params: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
        let redirect = params
            .get("redirect")
            .or_else(|| params.get("redirect_uri"))
            .expect("授权 URL 必带回环 redirect")
            .clone();
        let state = params.get("state").expect("授权 URL 必带 state").clone();
        let response = test_client()
            .get(format!("{redirect}?state={state}&code={SYNTHETIC_CODE}"))
            .send()
            .await
            .expect("投递回调");
        assert_eq!(response.status(), 200, "正确 state 的回调应被接受");
        task.await
            .expect("授权任务不应 panic")
            .expect("fake 授权应成功");
        assert_eq!(fake.exchange_count(), 1);
        auth
    }

    // ───────────────────────── fake relay WebSocket ─────────────────────────

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum HandshakeMode {
        Waiting,
        Matched,
        WrongSidWaiting,
        MissingSidWaiting,
        Kicked,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum HeartbeatReply {
        Waiting,
        Matched,
        Ignore,
    }

    #[derive(Clone)]
    struct RelayScript {
        handshake: HandshakeMode,
        heartbeat: HeartbeatReply,
        match_after_heartbeats: Option<usize>,
        kick_after_heartbeats: Option<usize>,
        close_after_match: bool,
        data_before_ack: Option<Value>,
        data_after_match: Option<Value>,
        ack_gate: Option<Arc<Semaphore>>,
    }

    impl Default for RelayScript {
        fn default() -> Self {
            Self {
                handshake: HandshakeMode::Waiting,
                heartbeat: HeartbeatReply::Waiting,
                match_after_heartbeats: None,
                kick_after_heartbeats: None,
                close_after_match: false,
                data_before_ack: None,
                data_after_match: None,
                ack_gate: None,
            }
        }
    }

    struct RelayState {
        script: RelayScript,
        sockets_opened: usize,
        connections_closed: usize,
        uris: Vec<String>,
        client_frames: Vec<Value>,
        heartbeats: usize,
        data_payloads: Vec<Value>,
    }

    struct FakeRelay {
        origin: String,
        state: Arc<Mutex<RelayState>>,
        task: JoinHandle<()>,
    }

    impl Drop for FakeRelay {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl FakeRelay {
        async fn start(script: RelayScript) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("fake WS 端口");
            let port = listener.local_addr().expect("fake WS 地址").port();
            let state = Arc::new(Mutex::new(RelayState {
                script,
                sockets_opened: 0,
                connections_closed: 0,
                uris: Vec::new(),
                client_frames: Vec::new(),
                heartbeats: 0,
                data_payloads: Vec::new(),
            }));
            let task = tokio::spawn(serve_relay(listener, state.clone()));
            Self {
                origin: format!("ws://127.0.0.1:{port}"),
                state,
                task,
            }
        }

        fn hold_ack(&self) -> Arc<Semaphore> {
            let gate = Arc::new(Semaphore::new(0));
            self.state.lock().unwrap().script.ack_gate = Some(gate.clone());
            gate
        }

        fn sockets_opened(&self) -> usize {
            self.state.lock().unwrap().sockets_opened
        }

        fn connections_closed(&self) -> usize {
            self.state.lock().unwrap().connections_closed
        }

        fn uris(&self) -> Vec<String> {
            self.state.lock().unwrap().uris.clone()
        }

        fn client_frames(&self) -> Vec<Value> {
            self.state.lock().unwrap().client_frames.clone()
        }

        fn heartbeats(&self) -> usize {
            self.state.lock().unwrap().heartbeats
        }

        fn data_payloads(&self) -> Vec<Value> {
            self.state.lock().unwrap().data_payloads.clone()
        }
    }

    async fn serve_relay(listener: TcpListener, state: Arc<Mutex<RelayState>>) {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let state = state.clone();
            tokio::spawn(async move {
                handle_relay_connection(stream, state).await;
            });
        }
    }

    fn json_message(message: Message) -> Option<Value> {
        match message {
            Message::Text(text) => serde_json::from_str(text.as_str()).ok(),
            Message::Binary(bytes) => serde_json::from_slice(bytes.as_ref()).ok(),
            _ => None,
        }
    }

    async fn send_server_json(
        socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
        value: &Value,
    ) {
        let _ = socket.send(Message::text(value.to_string())).await;
    }

    async fn close_server_socket(socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>) {
        let _ = socket.close(None).await;
    }

    // accept_hdr_async 的 Err 侧固定为 Response（约 136B），测试夹具无优化空间。
    #[allow(clippy::result_large_err)]
    async fn handle_relay_connection(stream: TcpStream, state: Arc<Mutex<RelayState>>) {
        let uri_slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let captured = uri_slot.clone();
        let Ok(mut socket) = tokio_tungstenite::accept_hdr_async(
            stream,
            move |request: &Request, response: Response| {
                *captured.lock().unwrap() = Some(request.uri().to_string());
                Ok(response)
            },
        )
        .await
        else {
            return;
        };
        {
            let mut state = state.lock().unwrap();
            state.sockets_opened += 1;
            if let Some(uri) = uri_slot.lock().unwrap().take() {
                state.uris.push(uri);
            }
        }

        // 首个 JSON 帧必须是 auth_init（客户端在 open 后立即发）。
        let mut auth_init = None;
        while let Some(message) = socket.next().await {
            let Ok(message) = message else {
                break;
            };
            if let Some(value) = json_message(message) {
                state.lock().unwrap().client_frames.push(value.clone());
                if value.get("type").and_then(Value::as_str) == Some("auth_init") {
                    auth_init = Some(value);
                    break;
                }
            }
        }
        let Some(auth_init) = auth_init else {
            return;
        };
        let sid = auth_init
            .get("device_sid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let script = state.lock().unwrap().script.clone();

        if let Some(payload) = script.data_before_ack.clone() {
            send_server_json(&mut socket, &json!({"type": "data", "payload": payload})).await;
        }
        if let Some(gate) = script.ack_gate.clone() {
            let _ = gate.acquire().await;
        }

        let mut matched = match script.handshake {
            HandshakeMode::Kicked => {
                send_server_json(&mut socket, &json!({"type": "error", "code": "KICKED"})).await;
                close_server_socket(&mut socket).await;
                state.lock().unwrap().connections_closed += 1;
                return;
            }
            mode => {
                let pair_status = if mode == HandshakeMode::Matched {
                    "matched"
                } else {
                    "waiting"
                };
                let mut ack = json!({"type": "auth_ack", "pair_status": pair_status});
                match mode {
                    HandshakeMode::WrongSidWaiting => {
                        ack["device_sid"] = json!("synthetic-sid-wrong");
                    }
                    HandshakeMode::MissingSidWaiting => {}
                    _ => {
                        ack["device_sid"] = json!(sid);
                    }
                }
                send_server_json(&mut socket, &ack).await;
                mode == HandshakeMode::Matched
            }
        };
        if matched {
            if let Some(payload) = script.data_after_match.clone() {
                send_server_json(&mut socket, &json!({"type": "data", "payload": payload})).await;
            }
            if script.close_after_match {
                close_server_socket(&mut socket).await;
                state.lock().unwrap().connections_closed += 1;
                return;
            }
        }

        let mut heartbeat_count = 0usize;
        while let Some(message) = socket.next().await {
            let Ok(message) = message else {
                break;
            };
            match message {
                Message::Close(_) => break,
                Message::Text(_) | Message::Binary(_) => {}
                _ => continue,
            }
            let Some(value) = json_message(message) else {
                continue;
            };
            state.lock().unwrap().client_frames.push(value.clone());
            match value.get("type").and_then(Value::as_str) {
                Some("pair_status_query") => {
                    heartbeat_count += 1;
                    state.lock().unwrap().heartbeats += 1;
                    if let Some(threshold) = script.kick_after_heartbeats {
                        if heartbeat_count >= threshold {
                            send_server_json(
                                &mut socket,
                                &json!({"type": "error", "code": "KICKED"}),
                            )
                            .await;
                            close_server_socket(&mut socket).await;
                            state.lock().unwrap().connections_closed += 1;
                            return;
                        }
                    }
                    let reply = match script.match_after_heartbeats {
                        Some(threshold) if heartbeat_count >= threshold => HeartbeatReply::Matched,
                        _ => script.heartbeat,
                    };
                    match reply {
                        HeartbeatReply::Waiting => {
                            send_server_json(
                                &mut socket,
                                &json!({"type": "pair_status_ack", "pair_status": "waiting"}),
                            )
                            .await;
                        }
                        HeartbeatReply::Matched => {
                            send_server_json(
                                &mut socket,
                                &json!({"type": "pair_status_ack", "pair_status": "matched"}),
                            )
                            .await;
                            matched = true;
                        }
                        HeartbeatReply::Ignore => {}
                    }
                }
                Some("data") => {
                    let payload = value.get("payload").cloned().unwrap_or(Value::Null);
                    state.lock().unwrap().data_payloads.push(payload);
                }
                _ => {}
            }
        }
        let _ = matched;
        state.lock().unwrap().connections_closed += 1;
    }

    // ───────────────────────── 事件收集 ─────────────────────────

    struct EventLog {
        states: Arc<Mutex<Vec<ConnectionView>>>,
        data: Arc<Mutex<Vec<Value>>>,
        task: JoinHandle<()>,
    }

    impl Drop for EventLog {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl EventLog {
        fn start() -> (mpsc::Sender<RelayEvent>, Self) {
            let (sender, mut receiver) = mpsc::channel(128);
            let states = Arc::new(Mutex::new(Vec::new()));
            let data = Arc::new(Mutex::new(Vec::new()));
            let task = tokio::spawn({
                let states = states.clone();
                let data = data.clone();
                async move {
                    while let Some(event) = receiver.recv().await {
                        match event {
                            RelayEvent::State(view) => states.lock().unwrap().push(view),
                            RelayEvent::Data(payload) => data.lock().unwrap().push(payload),
                        }
                    }
                }
            });
            (sender, Self { states, data, task })
        }

        fn states(&self) -> Vec<ConnectionView> {
            self.states.lock().unwrap().clone()
        }

        fn last_state(&self) -> Option<ConnectionView> {
            self.states.lock().unwrap().last().cloned()
        }

        fn data(&self) -> Vec<Value> {
            self.data.lock().unwrap().clone()
        }

        async fn wait_state(
            &self,
            predicate: impl Fn(&ConnectionView) -> bool,
            what: &str,
        ) -> ConnectionView {
            for _ in 0..1000 {
                if let Some(view) = self.states().into_iter().rev().find(|view| predicate(view)) {
                    return view;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            panic!("等待状态超时：{what}");
        }

        async fn wait_data(&self, count: usize, what: &str) -> Vec<Value> {
            for _ in 0..1000 {
                let data = self.data();
                if data.len() >= count {
                    return data;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            panic!("等待数据超时：{what}");
        }
    }

    // ───────────────────────── 测试辅助 ─────────────────────────

    fn relay_client(
        auth: Arc<AuthService>,
        events: mpsc::Sender<RelayEvent>,
        origin: &str,
    ) -> RelayClient {
        RelayClient::with_endpoint(auth, events, origin.to_string())
    }

    fn relay_client_with_timing(
        auth: Arc<AuthService>,
        events: mpsc::Sender<RelayEvent>,
        origin: &str,
        timing: RelayTiming,
    ) -> RelayClient {
        RelayClient::with_endpoint_timing(auth, events, origin.to_string(), timing)
    }

    fn query_params(uri: &str) -> HashMap<String, String> {
        let absolute = if uri.starts_with('/') {
            format!("ws://placeholder{uri}")
        } else {
            uri.to_string()
        };
        reqwest::Url::parse(&absolute)
            .expect("fake relay URI 应可解析")
            .query_pairs()
            .into_owned()
            .collect()
    }

    fn assert_secret_free(err: &ZcodeError, secrets: &[&str]) {
        let text = format!("{err} {err:?}");
        for secret in secrets {
            assert!(!text.contains(secret), "错误不得包含 {secret}：{text}");
        }
    }

    // ───────────────────────── 必需断言 1 ─────────────────────────

    /// 两个并发 connect 只开一条 socket，且每连接只取一次 token。
    #[tokio::test]
    async fn concurrent_connect_uses_one_socket() {
        let fake_auth = FakeAuthHttp::start().await;
        let auth = authorized_service(&fake_auth).await;
        let fake_relay = FakeRelay::start(RelayScript::default()).await;
        let gate = fake_relay.hold_ack();
        let (events, log) = EventLog::start();
        let client = Arc::new(relay_client(auth, events, &fake_relay.origin));

        let first = tokio::spawn({
            let client = client.clone();
            async move { client.connect(route(Some(SYNTHETIC_MID))).await }
        });
        let second = tokio::spawn({
            let client = client.clone();
            async move { client.connect(route(Some(SYNTHETIC_MID))).await }
        });

        wait_until(|| fake_relay.sockets_opened() == 1, "首条 socket 建立").await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        let sockets_opened_for_two_connects = fake_relay.sockets_opened();
        assert_eq!(sockets_opened_for_two_connects, 1);

        gate.add_permits(1);
        let first_view = first.await.expect("任务不应 panic").expect("首个 connect");
        let second_view = second
            .await
            .expect("任务不应 panic")
            .expect("第二个 connect");
        assert_eq!(first_view.state, ConnectionState::Connecting);
        assert_eq!(first_view.generation, second_view.generation);
        assert_eq!(fake_relay.sockets_opened(), 1);
        assert_eq!(fake_auth.token_count(), 1);
        assert_eq!(
            log.last_state().map(|view| view.state),
            Some(ConnectionState::Connecting)
        );

        client.disconnect().await;
    }

    // ───────────────────────── 必需断言 2 ─────────────────────────

    /// waiting 是合法配对前态，不能当作人工批准/已配对。
    #[tokio::test]
    async fn waiting_is_not_human_approval() {
        let fake_auth = FakeAuthHttp::start().await;
        let auth = authorized_service(&fake_auth).await;
        let fake_relay = FakeRelay::start(RelayScript::default()).await;
        let (events, log) = EventLog::start();
        let client = relay_client(auth, events, &fake_relay.origin);

        let view = client
            .connect(route(Some(SYNTHETIC_MID)))
            .await
            .expect("waiting 应建立连接");
        let waiting_connection_is_paired = view.is_paired();
        assert!(!waiting_connection_is_paired);
        assert_eq!(view.state, ConnectionState::Connecting);

        let observed = log
            .wait_state(
                |view| view.state == ConnectionState::Connecting,
                "connecting",
            )
            .await;
        assert_eq!(observed.state, ConnectionState::Connecting);
        assert!(!observed.is_paired());
        client.disconnect().await;
    }

    // ───────────────────────── 必需断言 3 ─────────────────────────

    /// KICKED 立即冲突终态，零自动重连、零后续心跳。
    #[tokio::test]
    async fn kicked_is_terminal() {
        let fake_auth = FakeAuthHttp::start().await;
        let auth = authorized_service(&fake_auth).await;
        let fake_relay = FakeRelay::start(RelayScript {
            handshake: HandshakeMode::Kicked,
            ..RelayScript::default()
        })
        .await;
        let (events, log) = EventLog::start();
        let client = relay_client_with_timing(
            auth,
            events,
            &fake_relay.origin,
            fast_timing(
                Duration::from_millis(20),
                Duration::from_millis(100),
                Duration::from_secs(5),
            ),
        );

        let err = client
            .connect(route(Some(SYNTHETIC_MID)))
            .await
            .expect_err("KICKED 必须终态失败");
        assert_eq!(err.code, codes::CONFLICT);
        let view = log
            .wait_state(|view| view.state == ConnectionState::Conflict, "conflict")
            .await;
        assert_eq!(view.state, ConnectionState::Conflict);
        assert_eq!(view.reason_code.as_deref(), Some(codes::CONFLICT));

        tokio::time::sleep(Duration::from_millis(120)).await;
        let reconnects_after_kicked = fake_relay.sockets_opened().saturating_sub(1);
        assert_eq!(reconnects_after_kicked, 0);
        assert_eq!(fake_relay.heartbeats(), 0, "KICKED 后不得再发心跳");
    }

    // ───────────────────────── 必需断言 4 ─────────────────────────

    /// 未 paired 时发送返回错误且写零 data 帧。
    #[tokio::test]
    async fn unpaired_cannot_send_data() {
        let fake_auth = FakeAuthHttp::start().await;
        let auth = authorized_service(&fake_auth).await;
        let fake_relay = FakeRelay::start(RelayScript::default()).await;
        let (events, _log) = EventLog::start();
        let client = relay_client(auth, events, &fake_relay.origin);

        let view = client
            .connect(route(Some(SYNTHETIC_MID)))
            .await
            .expect("waiting 应建立连接");
        assert!(!view.is_paired());
        let err = client
            .send_data(json!({"hello": "world"}))
            .await
            .expect_err("未配对发送必须失败");
        assert_eq!(err.code, codes::NOT_READY);

        tokio::time::sleep(Duration::from_millis(30)).await;
        let unpaired_data_frames = fake_relay.data_payloads().len();
        assert_eq!(unpaired_data_frames, 0);
        let data_frames = fake_relay
            .client_frames()
            .into_iter()
            .filter(|frame| frame.get("type").and_then(Value::as_str) == Some("data"))
            .count();
        assert_eq!(data_frames, 0);
        client.disconnect().await;
    }

    // ───────────────────────── 必需断言 5 ─────────────────────────

    /// 15s（测试注入短窗）无 pair_status_ack → offline 并断连接；关闭后零心跳、零重连。
    #[tokio::test]
    async fn heartbeat_missing_ack_is_offline_without_reconnect() {
        let fake_auth = FakeAuthHttp::start().await;
        let auth = authorized_service(&fake_auth).await;
        let fake_relay = FakeRelay::start(RelayScript {
            heartbeat: HeartbeatReply::Ignore,
            ..RelayScript::default()
        })
        .await;
        let (events, log) = EventLog::start();
        let client = relay_client_with_timing(
            auth,
            events,
            &fake_relay.origin,
            fast_timing(
                Duration::from_millis(20),
                Duration::from_millis(100),
                Duration::from_secs(5),
            ),
        );

        let view = client
            .connect(route(Some(SYNTHETIC_MID)))
            .await
            .expect("waiting 应建立连接");
        assert!(!view.is_paired());
        assert!(fake_relay.heartbeats() == 0, "心跳只应在 auth_ack 之后开始");

        let after_15s_without_pair_status = log
            .wait_state(
                |view| view.state == ConnectionState::Offline,
                "心跳超时 offline",
            )
            .await;
        assert_eq!(
            after_15s_without_pair_status.state,
            ConnectionState::Offline
        );

        let heartbeat_at_close = fake_relay.heartbeats();
        assert!(heartbeat_at_close >= 1, "超时前应已发出心跳");
        tokio::time::sleep(Duration::from_millis(120)).await;
        let heartbeat_after_close = fake_relay.heartbeats().saturating_sub(heartbeat_at_close);
        assert_eq!(heartbeat_after_close, 0);
        assert_eq!(fake_relay.sockets_opened(), 1);
    }

    // ───────────────────────── 其余生命周期覆盖 ─────────────────────────

    /// SID 回显不符：连接被拒绝，且客户端零后续帧（不发心跳/数据）。
    #[tokio::test]
    async fn sid_echo_mismatch_rejects() {
        let fake_auth = FakeAuthHttp::start().await;
        let auth = authorized_service(&fake_auth).await;
        let fake_relay = FakeRelay::start(RelayScript {
            handshake: HandshakeMode::WrongSidWaiting,
            ..RelayScript::default()
        })
        .await;
        let (events, log) = EventLog::start();
        let client = relay_client(auth, events, &fake_relay.origin);

        let err = client
            .connect(route(Some(SYNTHETIC_MID)))
            .await
            .expect_err("SID 回显不符必须拒绝");
        assert_eq!(err.code, codes::UNKNOWN);
        let view = log
            .wait_state(|view| view.state == ConnectionState::Unknown, "unknown")
            .await;
        assert_eq!(view.state, ConnectionState::Unknown);

        tokio::time::sleep(Duration::from_millis(40)).await;
        let frames = fake_relay.client_frames();
        let after_auth_init = frames
            .iter()
            .filter(|frame| frame.get("type").and_then(Value::as_str) != Some("auth_init"))
            .count();
        assert_eq!(after_auth_init, 0, "拒绝后不得再发任何帧");
        assert_eq!(fake_relay.sockets_opened(), 1);
    }

    /// auth_ack 完全缺少 device_sid：同样拒绝（回显是硬要求），零后续帧。
    #[tokio::test]
    async fn missing_sid_ack_rejects() {
        let fake_auth = FakeAuthHttp::start().await;
        let auth = authorized_service(&fake_auth).await;
        let fake_relay = FakeRelay::start(RelayScript {
            handshake: HandshakeMode::MissingSidWaiting,
            ..RelayScript::default()
        })
        .await;
        let (events, log) = EventLog::start();
        let client = relay_client(auth, events, &fake_relay.origin);

        let err = client
            .connect(route(Some(SYNTHETIC_MID)))
            .await
            .expect_err("缺少 SID 回显必须拒绝");
        assert_eq!(err.code, codes::UNKNOWN);
        log.wait_state(|view| view.state == ConnectionState::Unknown, "unknown")
            .await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            fake_relay
                .client_frames()
                .iter()
                .filter(|frame| frame.get("type").and_then(Value::as_str) != Some("auth_init"))
                .count(),
            0
        );
    }

    /// 网络掉线（配对后被断开）→ offline，零重连。
    #[tokio::test]
    async fn network_drop_is_offline() {
        let fake_auth = FakeAuthHttp::start().await;
        let auth = authorized_service(&fake_auth).await;
        let fake_relay = FakeRelay::start(RelayScript {
            handshake: HandshakeMode::Matched,
            close_after_match: true,
            ..RelayScript::default()
        })
        .await;
        let (events, log) = EventLog::start();
        let client = relay_client(auth, events, &fake_relay.origin);

        let view = client
            .connect(route(Some(SYNTHETIC_MID)))
            .await
            .expect("matched 应建立连接");
        assert!(view.is_paired());
        log.wait_state(|view| view.state == ConnectionState::Paired, "paired")
            .await;
        let offline = log
            .wait_state(|view| view.state == ConnectionState::Offline, "offline")
            .await;
        assert_eq!(offline.state, ConnectionState::Offline);
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(fake_relay.sockets_opened(), 1);
    }

    /// 缺 mid 仍可连接，但 waiting 不得被当作 paired；auth_init 形状/秒级时间戳正确。
    #[tokio::test]
    async fn missing_mid_still_connects_but_is_not_paired() {
        let fake_auth = FakeAuthHttp::start().await;
        let auth = authorized_service(&fake_auth).await;
        let fake_relay = FakeRelay::start(RelayScript::default()).await;
        let (events, _log) = EventLog::start();
        let client = relay_client(auth, events, &fake_relay.origin);

        let view = client
            .connect(route(None))
            .await
            .expect("缺 mid 也应连上 relay");
        assert_eq!(view.state, ConnectionState::Connecting);
        assert!(!view.is_paired());

        let uris = fake_relay.uris();
        let params = query_params(&uris[0]);
        assert_eq!(
            params.get("token").map(String::as_str),
            Some(SYNTHETIC_TOKEN_1)
        );
        assert!(!params.contains_key("mid"), "缺省 mid 不得写入升级 URL");
        assert_eq!(
            uris[0].split('?').next().unwrap_or_default(),
            OFFICIAL_RELAY_WS_PATH
        );

        let frames = fake_relay.client_frames();
        let auth_init = frames
            .iter()
            .find(|frame| frame.get("type").and_then(Value::as_str) == Some("auth_init"))
            .expect("必须发送 auth_init");
        assert_eq!(auth_init["device_sid"], SYNTHETIC_SID);
        assert_eq!(auth_init["meta"]["platform"], "tuvis");
        let client_ts = auth_init["client_ts"]
            .as_u64()
            .expect("client_ts 必须是整数");
        assert!(
            client_ts < 10_000_000_000,
            "client_ts 必须是 epoch 秒而不是毫秒"
        );
        assert!(client_ts + 5 >= now_secs(), "client_ts 不得落后当前时间");
        client.disconnect().await;
    }

    /// 每次 connect 现取新 token（不缓存、不复用旧 token）。
    #[tokio::test]
    async fn each_connect_mints_a_new_token() {
        let fake_auth = FakeAuthHttp::start().await;
        fake_auth.set_tokens(&[SYNTHETIC_TOKEN_1, SYNTHETIC_TOKEN_2]);
        let auth = authorized_service(&fake_auth).await;
        let fake_relay = FakeRelay::start(RelayScript {
            handshake: HandshakeMode::Matched,
            ..RelayScript::default()
        })
        .await;
        let (events, _log) = EventLog::start();
        let client = relay_client(auth, events, &fake_relay.origin);

        assert!(client
            .connect(route(Some(SYNTHETIC_MID)))
            .await
            .expect("首次连接")
            .is_paired());
        client.disconnect().await;
        wait_until(|| fake_relay.connections_closed() >= 1, "首条 socket 关闭").await;

        assert!(client
            .connect(route(Some(SYNTHETIC_MID)))
            .await
            .expect("第二次连接")
            .is_paired());
        assert_eq!(fake_auth.token_count(), 2);
        let tokens: Vec<Option<String>> = fake_relay
            .uris()
            .iter()
            .map(|uri| query_params(uri).get("token").cloned())
            .collect();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].as_deref(), Some(SYNTHETIC_TOKEN_1));
        assert_eq!(tokens[1].as_deref(), Some(SYNTHETIC_TOKEN_2));
        assert_ne!(tokens[0], tokens[1]);
        client.disconnect().await;
    }

    /// 已过期的账号 JWT：在任何 HTTP 请求之前 needs_auth，且零 socket。
    #[tokio::test]
    async fn expired_token_requires_auth() {
        let fake_auth = FakeAuthHttp::start().await;
        fake_auth.set_jwt(expired_jwt());
        let auth = authorized_service(&fake_auth).await;
        let fake_relay = FakeRelay::start(RelayScript::default()).await;
        let (events, log) = EventLog::start();
        let client = relay_client(auth, events, &fake_relay.origin);

        let err = client
            .connect(route(Some(SYNTHETIC_MID)))
            .await
            .expect_err("过期凭据必须 needs_auth");
        assert_eq!(err.code, codes::NEEDS_AUTH);
        assert_eq!(fake_auth.token_count(), 0, "过期凭据不得发起 token 请求");
        let view = log
            .wait_state(
                |view| view.state == ConnectionState::NeedsAuth,
                "needs_auth",
            )
            .await;
        assert_eq!(view.state, ConnectionState::NeedsAuth);
        assert_eq!(fake_relay.sockets_opened(), 0);
    }

    /// 已配对时重复 connect：复用同一条 socket、不再取 token。
    #[tokio::test]
    async fn duplicate_connect_reuses_live_socket() {
        let fake_auth = FakeAuthHttp::start().await;
        let auth = authorized_service(&fake_auth).await;
        let fake_relay = FakeRelay::start(RelayScript {
            handshake: HandshakeMode::Matched,
            ..RelayScript::default()
        })
        .await;
        let (events, _log) = EventLog::start();
        let client = relay_client(auth, events, &fake_relay.origin);

        let first = client
            .connect(route(Some(SYNTHETIC_MID)))
            .await
            .expect("首次连接");
        let second = client
            .connect(route(Some(SYNTHETIC_MID)))
            .await
            .expect("重复 connect");
        assert!(first.is_paired());
        assert_eq!(first, second);
        assert_eq!(fake_relay.sockets_opened(), 1);
        assert_eq!(fake_auth.token_count(), 1);
        client.disconnect().await;
    }

    /// disconnect：推进 generation、offline、取消计时器（关闭后零心跳）。
    #[tokio::test]
    async fn disconnect_clears_timers() {
        let fake_auth = FakeAuthHttp::start().await;
        let auth = authorized_service(&fake_auth).await;
        let fake_relay = FakeRelay::start(RelayScript::default()).await;
        let (events, log) = EventLog::start();
        let client = relay_client_with_timing(
            auth,
            events,
            &fake_relay.origin,
            fast_timing(
                Duration::from_millis(20),
                Duration::from_secs(5),
                Duration::from_secs(5),
            ),
        );

        let before = client
            .connect(route(Some(SYNTHETIC_MID)))
            .await
            .expect("waiting 应建立连接");
        wait_until(|| fake_relay.heartbeats() >= 1, "心跳启动").await;
        client.disconnect().await;
        let after = client.view();
        assert_eq!(after.state, ConnectionState::Offline);
        assert!(
            after.generation > before.generation,
            "断开必须推进 generation"
        );
        log.wait_state(|view| view.state == ConnectionState::Offline, "offline")
            .await;

        wait_until(|| fake_relay.connections_closed() >= 1, "socket 关闭").await;
        let heartbeats_at_close = fake_relay.heartbeats();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(fake_relay.heartbeats(), heartbeats_at_close);
        assert_eq!(fake_relay.sockets_opened(), 1);
    }

    /// 迟到的 token 结果（已经现取但代际已变）不得建立活连接。
    #[tokio::test]
    async fn late_connect_after_generation_change_is_discarded() {
        let fake_auth = FakeAuthHttp::start().await;
        let auth = authorized_service(&fake_auth).await;
        let fake_relay = FakeRelay::start(RelayScript::default()).await;
        let gate = fake_auth.hold_next_token();
        let (events, log) = EventLog::start();
        let client = Arc::new(relay_client(auth, events, &fake_relay.origin));

        let pending = tokio::spawn({
            let client = client.clone();
            async move { client.connect(route(Some(SYNTHETIC_MID))).await }
        });
        wait_until(|| fake_auth.token_count() == 1, "token 请求在途").await;

        client.disconnect().await;
        gate.add_permits(1);
        let err = pending
            .await
            .expect("任务不应 panic")
            .expect_err("旧代际连接结果必须丢弃");
        assert_eq!(err.code, codes::STALE_GENERATION);

        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(fake_relay.sockets_opened(), 0, "代际已变后不得开 socket");
        assert_eq!(
            log.last_state().map(|view| view.state),
            Some(ConnectionState::Offline)
        );
    }

    /// 迟到的握手结果（disconnect 后 ack 才到）不得回退状态/产生 paired 事件。
    #[tokio::test]
    async fn late_handshake_after_disconnect_is_discarded() {
        let fake_auth = FakeAuthHttp::start().await;
        let auth = authorized_service(&fake_auth).await;
        let fake_relay = FakeRelay::start(RelayScript::default()).await;
        let gate = fake_relay.hold_ack();
        let (events, log) = EventLog::start();
        let client = Arc::new(relay_client(auth, events, &fake_relay.origin));

        let pending = tokio::spawn({
            let client = client.clone();
            async move { client.connect(route(Some(SYNTHETIC_MID))).await }
        });
        wait_until(
            || {
                fake_relay.sockets_opened() == 1
                    && fake_relay
                        .client_frames()
                        .iter()
                        .any(|frame| frame.get("type").and_then(Value::as_str) == Some("auth_init"))
            },
            "握手在途",
        )
        .await;

        client.disconnect().await;
        gate.add_permits(1);
        let err = pending
            .await
            .expect("任务不应 panic")
            .expect_err("旧代际握手结果必须丢弃");
        assert_eq!(err.code, codes::STALE_GENERATION);

        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(fake_relay.sockets_opened(), 1);
        assert!(
            !log.states()
                .iter()
                .any(|view| view.state == ConnectionState::Paired),
            "旧代际不得产生 paired 事件"
        );
        assert_eq!(
            log.last_state().map(|view| view.state),
            Some(ConnectionState::Offline)
        );
    }

    /// 30s 路由窗（测试注入短窗）无 matched → offline，零重连。
    #[tokio::test]
    async fn pair_window_timeout_is_offline_without_reconnect() {
        let fake_auth = FakeAuthHttp::start().await;
        let auth = authorized_service(&fake_auth).await;
        let fake_relay = FakeRelay::start(RelayScript {
            heartbeat: HeartbeatReply::Ignore,
            ..RelayScript::default()
        })
        .await;
        let (events, log) = EventLog::start();
        let client = relay_client_with_timing(
            auth,
            events,
            &fake_relay.origin,
            fast_timing(
                Duration::from_secs(5),
                Duration::from_secs(5),
                Duration::from_millis(80),
            ),
        );

        let view = client
            .connect(route(Some(SYNTHETIC_MID)))
            .await
            .expect("waiting 应建立连接");
        assert!(!view.is_paired());
        let offline = log
            .wait_state(
                |view| view.state == ConnectionState::Offline,
                "路由窗 offline",
            )
            .await;
        assert_eq!(offline.state, ConnectionState::Offline);
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(fake_relay.sockets_opened(), 1);
    }

    /// waiting 后由心跳 pair_status_ack 升级为 matched（唯一 paired 来源）。
    #[tokio::test]
    async fn heartbeat_matched_upgrades_waiting_to_paired() {
        let fake_auth = FakeAuthHttp::start().await;
        let auth = authorized_service(&fake_auth).await;
        let fake_relay = FakeRelay::start(RelayScript {
            heartbeat: HeartbeatReply::Waiting,
            match_after_heartbeats: Some(2),
            ..RelayScript::default()
        })
        .await;
        let (events, log) = EventLog::start();
        let client = relay_client_with_timing(
            auth,
            events,
            &fake_relay.origin,
            fast_timing(
                Duration::from_millis(10),
                Duration::from_secs(5),
                Duration::from_secs(5),
            ),
        );

        let view = client
            .connect(route(Some(SYNTHETIC_MID)))
            .await
            .expect("waiting 应建立连接");
        assert!(!view.is_paired());
        let paired = log
            .wait_state(|view| view.is_paired(), "heartbeat paired")
            .await;
        assert_eq!(paired.state, ConnectionState::Paired);
        assert!(fake_relay.heartbeats() >= 2);
        client.disconnect().await;
    }

    /// 数据帧只在 paired 后上报（配对前的 data 帧忽略）。
    #[tokio::test]
    async fn data_events_only_when_paired() {
        let fake_auth = FakeAuthHttp::start().await;
        let auth = authorized_service(&fake_auth).await;
        let fake_relay = FakeRelay::start(RelayScript {
            handshake: HandshakeMode::Matched,
            data_before_ack: Some(json!({"pre": "ignored"})),
            data_after_match: Some(json!({"post": "delivered"})),
            ..RelayScript::default()
        })
        .await;
        let (events, log) = EventLog::start();
        let client = relay_client(auth, events, &fake_relay.origin);

        assert!(client
            .connect(route(Some(SYNTHETIC_MID)))
            .await
            .expect("matched 应建立连接")
            .is_paired());
        let data = log.wait_data(1, "paired data").await;
        assert_eq!(data.len(), 1);
        assert_eq!(data[0], json!({"post": "delivered"}));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(log.data().len(), 1, "配对前的 data 帧不得上报");
        client.disconnect().await;
    }

    /// 错误（Display/Debug）不含 token/SID/MID/回环地址/WS 原文/Bearer。
    #[tokio::test]
    async fn errors_never_contain_secrets() {
        let secrets = [
            SYNTHETIC_TOKEN_1,
            SYNTHETIC_SID,
            SYNTHETIC_MID,
            "127.0.0.1",
            "ws://",
            "wss://",
            "Bearer",
            "synthetic-upstream",
        ];

        // 401：AUTH 失败只映射 needs_auth
        let fake_auth = FakeAuthHttp::start().await;
        let auth = authorized_service(&fake_auth).await;
        fake_auth.set_token_status(401);
        let fake_relay = FakeRelay::start(RelayScript::default()).await;
        let (events, log) = EventLog::start();
        let client = relay_client(auth, events, &fake_relay.origin);
        let err = client
            .connect(route(Some(SYNTHETIC_MID)))
            .await
            .expect_err("401 必须失败");
        assert_eq!(err.code, codes::NEEDS_AUTH);
        assert_secret_free(&err, &secrets);
        log.wait_state(
            |view| view.state == ConnectionState::NeedsAuth,
            "needs_auth",
        )
        .await;

        // WS 连接被拒：只映射 offline，不回显 endpoint
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("临时端口");
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let fake_auth = FakeAuthHttp::start().await;
        let auth = authorized_service(&fake_auth).await;
        let (events, _log) = EventLog::start();
        let client = relay_client(auth, events, &format!("ws://127.0.0.1:{port}"));
        let err = client
            .connect(route(Some(SYNTHETIC_MID)))
            .await
            .expect_err("连接拒绝必须失败");
        assert_eq!(err.code, codes::OFFLINE);
        assert_secret_free(&err, &secrets);
    }

    /// 生产时序与官方源码一致；心跳 jitter 落在 4,000–6,000ms。
    #[test]
    fn official_timing_matches_source() {
        let timing = RelayTiming::official();
        assert_eq!(timing.heartbeat_interval, Duration::from_millis(5_000));
        assert_eq!(timing.heartbeat_jitter, Duration::from_millis(1_000));
        assert_eq!(timing.ack_timeout, Duration::from_millis(15_000));
        assert_eq!(timing.pair_window, Duration::from_secs(30));
        for _ in 0..64 {
            let delay = jittered_heartbeat_delay(&timing);
            assert!(delay >= Duration::from_millis(4_000));
            assert!(delay <= Duration::from_millis(6_000));
        }
    }

    /// 生产 URL 形状：wss + 官方主机/路径 + token；mid 条件性附加。
    #[test]
    fn production_url_shape() {
        let with_mid = build_relay_url(
            OFFICIAL_RELAY_WS_ORIGIN,
            SYNTHETIC_TOKEN_1,
            &route(Some(SYNTHETIC_MID)),
        )
        .expect("URL 应可构造");
        let parsed = reqwest::Url::parse(&with_mid).expect("URL 应合法");
        assert_eq!(parsed.scheme(), "wss");
        assert_eq!(parsed.host_str(), Some("zcode.z.ai"));
        assert_eq!(parsed.path(), OFFICIAL_RELAY_WS_PATH);
        let params = query_params(&with_mid);
        assert_eq!(
            params.get("token").map(String::as_str),
            Some(SYNTHETIC_TOKEN_1)
        );
        assert_eq!(params.get("mid").map(String::as_str), Some(SYNTHETIC_MID));

        let without_mid =
            build_relay_url(OFFICIAL_RELAY_WS_ORIGIN, SYNTHETIC_TOKEN_1, &route(None))
                .expect("URL 应可构造");
        assert!(!query_params(&without_mid).contains_key("mid"));
        let blank_mid = build_relay_url(
            OFFICIAL_RELAY_WS_ORIGIN,
            SYNTHETIC_TOKEN_1,
            &route(Some("   ")),
        )
        .expect("URL 应可构造");
        assert!(!query_params(&blank_mid).contains_key("mid"));
    }

    /// 目标只能来自已保存的官方链接；mid/name 缺失或空白时按缺失处理。
    #[test]
    fn device_route_from_saved_url() {
        let url = format!(
            "https://zcode.z.ai/remote/v4?sid={SYNTHETIC_SID}&mid={SYNTHETIC_MID}&name=%E5%85%94%E5%AD%90"
        );
        let route = DeviceRoute::from_saved_url(&url).expect("官方链接应可解析");
        assert_eq!(route.sid, SYNTHETIC_SID);
        assert_eq!(route.mid.as_deref(), Some(SYNTHETIC_MID));
        assert_eq!(route.label.as_deref(), Some("兔子"));

        let no_mid = format!("https://zcode.z.ai/remote/v5?sid={SYNTHETIC_SID}&mid=%20");
        let route = DeviceRoute::from_saved_url(&no_mid).expect("官方链接应可解析");
        assert_eq!(route.mid, None);
        assert_eq!(route.label, None);

        let err = match DeviceRoute::from_saved_url(&format!(
            "http://zcode.z.ai/remote/v4?sid={SYNTHETIC_SID}"
        )) {
            Ok(_) => panic!("非 https 链接必须拒绝"),
            Err(err) => err,
        };
        assert_eq!(err.code, codes::NEEDS_CONFIG);
    }

    /// wire DTO：state 为 snake_case，字段为 camelCase。
    #[test]
    fn connection_view_serializes_wire_shape() {
        let view = ConnectionView {
            state: ConnectionState::NeedsAuth,
            generation: 7,
            reason_code: Some(codes::NEEDS_AUTH.to_string()),
            device_label: Some("Synthetic Device".to_string()),
        };
        let json = serde_json::to_value(&view).expect("应可序列化");
        assert_eq!(json["state"], "needs_auth");
        assert_eq!(json["generation"], 7);
        assert_eq!(json["reasonCode"], "needs_auth");
        assert_eq!(json["deviceLabel"], "Synthetic Device");
        assert!(!view.is_paired());
    }

    /// 配对后 send_data 才写帧；帧形状为官方 data 信封，client_ts 为秒。
    #[tokio::test]
    async fn send_data_after_paired_writes_official_frame() {
        let fake_auth = FakeAuthHttp::start().await;
        let auth = authorized_service(&fake_auth).await;
        let fake_relay = FakeRelay::start(RelayScript {
            handshake: HandshakeMode::Matched,
            ..RelayScript::default()
        })
        .await;
        let (events, _log) = EventLog::start();
        let client = relay_client(auth, events, &fake_relay.origin);

        assert!(client
            .connect(route(Some(SYNTHETIC_MID)))
            .await
            .expect("matched 应建立连接")
            .is_paired());
        client
            .send_data(json!({"action": "synthetic"}))
            .await
            .expect("配对后发送应成功");
        wait_until(|| fake_relay.data_payloads().len() == 1, "data 帧到达").await;
        assert_eq!(
            fake_relay.data_payloads()[0],
            json!({"action": "synthetic"})
        );

        let frames = fake_relay.client_frames();
        let data_frame = frames
            .iter()
            .find(|frame| frame.get("type").and_then(Value::as_str) == Some("data"))
            .expect("必须写出 data 帧");
        let client_ts = data_frame["client_ts"].as_u64().expect("data client_ts");
        assert!(client_ts < 10_000_000_000);
        client.disconnect().await;
    }
}
