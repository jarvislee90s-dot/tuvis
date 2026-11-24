//! DSH Desktop 远控控制面（计划「DSH Desktop 远控接入」§2.1 文件布局的模块根）。
//!
//! T01 交付：§2.2 固定领域类型（[`model`]）、错误边界（[`error`]）、原生信封
//! 编解码（[`codec`]）。T02 交付安装/实例定位（[`identity`]）；T04 交付固定包
//! 管理作业（[`install`]）。其余文件（auth/transport/contract/capability/store/
//! actor/mirror/actions/attachments/catalog/session_ops/commands）由后续任务按
//! 装配顺序落地（§2.5），本模块根不预建空文件。
//!
//! 占位边界（§2.5「T01 先定义只返回 Unsupported 的 hub 类型边界及 disabled_arc，
//! 让后续依赖可编译」）：[`DshControlHub`] 的每个入口都显式返回
//! `DSH_UNSUPPORTED`——**任何阶段不得把占位返回值显示成已支持**。完整 hub 与
//! 共享生产装配在 T09 落地（届时可移入 actor.rs）。

#[cfg(all(feature = "dsh-test-probes", not(debug_assertions)))]
compile_error!("dsh-test-probes is only allowed in debug test builds");

pub mod codec;
// 目标 2（写入边界）：手写 HTTP/1.1 一元传输——交付沿 = write_all 完成，
// 取消 = drop 连接；`/connect` 兑换与 mux 升级不经此模块。
pub mod http1;
// T08：逐操作契约识别（只读有预算的包契约识别）与纯计算能力判定（§2.3 固定签名）
pub mod capability;
pub mod contract;
pub mod error;
pub mod identity;
pub mod install;
pub mod instances;
pub mod model;
pub mod process_environment;
#[cfg(unix)]
mod profile_activity_lock;
// 生产装配单点（T05；§2.5「manager 和 hub 共用注入 env」——T09 的 production_hub
// 也从这里取 env 与 auth handle，不另起第二套生产全局）
pub mod production;
pub mod question_events;
pub mod questions;
pub mod session_settings;
pub mod switch;
// T06：本机双向身份授权（auth）与本机 IPC 传输面（local_ipc；对 §2.1 布局的
// 文档化扩展——auth 域的套接字/管道抽象，与 T08 transport.rs 的 loopback
// 连接面是两回事）
pub mod auth;
pub mod local_ipc;
// T07：官方 HTTP/Mux 协议与连接生命周期（§2.3 `transport::connect` 生产入口）
// 与代际镜像守卫（旧代际晚帧不污染；§2.1 布局的 mirror.rs）
pub mod mirror;
// T09：动作账本 store（dsh_actions / dsh_claims）与单会话 actor
pub mod actions;
pub mod actor;
pub mod store;
pub mod transport;
#[cfg(windows)]
pub mod windows_security;

use std::sync::Arc;

use error::codes;

/// P1-2/M6/P1-b（第四轮复审）：契约识别缓存条目——(键三元组：描述符指纹/
/// epoch/executable，整份识别报告，写入时刻，**包文件 stat 快照**)。命中须
/// 规则代际未换代、TTL 窗口内、且**当前包文件 stat 与缓存时一致**——mtime/size
/// 变化 = 包内容被替换的事实，立即失效重扫（TTL 只能收窄复用面，stat 才证明
/// 「内容未变」；描述符快哈希不感知 asar 替换）。
type ContractCacheEntry = (
    String,
    u64,
    std::path::PathBuf,
    contract::ContractReadReport,
    std::time::Instant,
    Option<(std::time::SystemTime, u64)>,
);

/// 当前包文件的 stat 快照（mtime, size）；不可读 = None（命中时 None≠Some 也
/// 判失效——保守重扫）。
fn package_stat_snapshot(target: &LocalHost) -> Option<(std::time::SystemTime, u64)> {
    let path = contract::package_path(target).ok()?;
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// P1-2（第三轮复审）：契约识别缓存的复用窗口——描述符快哈希不感知整包内容
/// 替换（同 Info.plist 重打包 asar），TTL 过后强制重扫，把「内容已变但描述符
/// 未变」的复用面收窄到窗口内。窗口内复用是吞吐取舍（整包哈希不便宜），窗口外
/// 一定重扫是归因正确性。
const CONTRACT_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);
use error::DshError;
use model::{
    ActionReceipt, ActionRequest, ActionView, CatalogKind, CatalogResult, CreateReceipt,
    CreateRequest, DeviceContext, LocalHost, SendPayload, SessionSnapshot, Target, UploadInput,
    UploadReceipt,
};

/// hub 的数据库访问模型（复审「连接模型」）。
///
/// **与项目主库同源**：`Global` 直接锁 `database::connection::DB`（全局单连接、
/// 短事务），不再自开第二条写连接——非 WAL 下第二条写连接会与 3 秒轮询/门禁
/// 写形成 `SQLITE_BUSY` 竞争（用户裁决：优先复用既有短事务连接模式）。
/// `Owned` 供测试注入内存/临时库（零接触真实数据目录）。
#[derive(Clone)]
pub enum DshDb {
    /// 生产：全局单连接（与 `DeviceStore::Global` 同一模式）。
    Global,
    /// 测试：注入自建库。
    Owned(Arc<std::sync::Mutex<rusqlite::Connection>>),
}

impl std::fmt::Debug for DshDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DshDb::Global => f.write_str("DshDb::Global"),
            DshDb::Owned(_) => f.write_str("DshDb::Owned(<conn>)"),
        }
    }
}

impl DshDb {
    /// 在连接上执行一个**同步短事务**闭包。
    ///
    /// 锁只在闭包期间持有（`Connection` 非 `Sync`，必须经 `Mutex` 串行）；
    /// 闭包内不得做网络/await——本函数是同步的。
    fn with<T>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> Result<T, DshError>,
    ) -> Result<T, DshError> {
        match self {
            // 全局库：锁优先级中毒也恢复（不让一次 panic 永久废掉远控）。
            DshDb::Global => {
                let guard = crate::database::connection::DB
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                f(&guard)
            }
            DshDb::Owned(conn) => {
                let guard = conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                f(&guard)
            }
        }
    }
}

/// DSH 控制面 hub（§2.3 固定入口）。
///
/// **两种构造**：
/// * [`DshControlHub::disabled_arc`]——零依赖禁用实例（测试与「未启用装配」共用，
///   或**启动恢复失败时关闭写入口**）；
/// * [`DshControlHub::production`] / [`DshControlHub::with_db`]——真实装配。
///
/// 业务入口按 T10—T17 逐个接线；未接线的仍显式 `DSH_UNSUPPORTED`（不把占位返回值
/// 显示成已支持）。已接线：动作账本（dispatch/action/recover）。
/// 会话归属解析器（审查 I4③）：新写入口在组 Target 前经它把 (实例, 会话) 绑定
/// 并复核——生产装配 = `production_registry().resolve_session`；`None` = 测试装配
/// 回归基线（生产恒装配）。
pub type SessionResolver = Arc<dyn Fn(&str, &str) -> Result<model::Target, DshError> + Send + Sync>;
pub type ReadonlyRootResolver =
    Arc<dyn Fn(&str, &str) -> Result<std::path::PathBuf, DshError> + Send + Sync>;

/// T3 发送预检的当代握手/发送凭据三态探针（注入面）：参数 = 已选实例 ID，返回
/// `Ready / Recovering / NotAuthorized`。生产 = `auth_for_target →
/// auth::control_peer_state`；测试注入可编程结论。**内含阻塞发现**（进程扫描）
/// ——调用方必须在 blocking 池执行。
pub type SendCredentialsProbe = Arc<dyn Fn(&str) -> auth::ControlPeerState + Send + Sync>;

pub struct DshControlHub {
    /// `None` = 禁用实例（未装配）。
    db: Option<DshDb>,
    /// 单会话 actor：`dispatch` 在**同一车道**内完成「登记 + 占用」，
    /// 故同一会话的两个不同 actionId 不会同时占用（T09 验收缺口修复）。
    actor: actor::DshSessionActor,
    /// **仅测试**：在车道内、登记前执行一次（用于稳定构造「同会话并发」窗口，
    /// 证明 dispatch 真的走串行车道——去掉 actor 后该用例会失败）。
    #[cfg(test)]
    dispatch_hook: Option<Box<dyn Fn() + Send + Sync>>,
    /// **仅测试**：DB 段执行时是否仍在车道内（复审 M3 变异曾把 DB 段移出车道
    /// 而用例仍绿）。dispatch 在车道闭包里置 true，出闭包置 false。
    /// **仅测试**：DB 段进入次数（证明 DB 段真的执行过，而不是被跳过）。
    #[cfg(test)]
    pub(crate) db_lane_entries: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// **仅测试**：车道作用域标记——车道闭包内置 true、出闭包复位；DB 段进入时
    /// 读取它，若为 false 说明 DB 段跑在车道外（复审 M3 的判别点）。
    #[cfg(test)]
    pub(crate) lane_scope: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// **仅测试**：观察到「DB 段跑在车道外」的次数（应为 0）。
    #[cfg(test)]
    pub(crate) db_outside_lane: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// 恢复一次性标记（复审 Major 1）：`startup_recovery()` 只允许**消费一次**——
    /// 二次调用直接短路返回 `Ok(0)`。否则门开启后的再次恢复会把期间**真实在途**
    /// 的 reserved/accepted/running 动作一并打成 unknown_delivery 并删 claim。
    recovery_consumed: std::sync::atomic::AtomicBool,
    /// T10：原生发送器（None = 仅账本模式：dispatch 到 Reserved 为止，不接原生；
    /// 生产装配 = 长生命周期 PeerChannel，测试注入 FakeNative）。
    native: Option<Arc<dyn actions::NativeDispatch>>,
    /// T10：Host/epoch 探针（原生发送前复核）。
    host_probe: Option<actions::HostProbe>,
    /// I4③：会话归属解析器（新写入口先绑定并复核实例+会话；None = 测试装配
    /// 回归基线，生产恒装配——缺装配的新写按未核验拒绝，fail-closed）。
    session_resolver: Option<SessionResolver>,
    readonly_root: std::sync::RwLock<Option<ReadonlyRootResolver>>,
    /// P1-D：原生通道就绪探针（泵每轮预检——凭据未就绪保留 Queued 不误标 Failed）。
    native_ready: Option<actions::NativeReadyProbe>,
    /// P1-A：契约识别探针（能力门输入；None = 未装配能力门——测试/禁用实例）。
    contract_read: Option<actions::ContractReadProbe>,
    /// P1-A/M6+P1-2（第三轮复审）：契约识别缓存——**整份 report**（m1：保留
    /// undecided 集），键 = (描述符指纹, epoch, executable)，命中另须规则代际
    /// 未换代且 TTL 窗口内（[`CONTRACT_CACHE_TTL`]）——描述符快哈希不感知整包
    /// 内容替换（同 Info.plist 重打包 asar），窗口外强制重扫归因。
    contract_cache: std::sync::Mutex<Option<ContractCacheEntry>>,
    /// P1-B：效果源（None = 不自动确认效果——收刈器兜底仍生效）。
    effects: Option<Arc<dyn actions::EffectSource>>,
    /// P1-2：自动适配开关读取面（最终边界用；生产 = 全局影子库 DSH 键，测试 =
    /// AtomicBool 注入——泵路径与直发同源，不经过 RemoteState）。RwLock 包装：
    /// 测试用例在 fixture 构建后覆盖注入（生产恒缺省实现，无写竞争）。
    auto_adapt_source: std::sync::RwLock<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// T3 发送预检的当代握手/发送凭据三态探针（send-info 区分「正在恢复连接」
    /// 与「请在电脑上检查所选实例」用）。RwLock 包装：测试在构建后覆盖注入。
    /// `None` = 未装配（测试/禁用实例）→ 按 Ready 处理（不阻断，交由派发边界
    /// 的 PeerChannel 复核兜底）。
    credentials_probe: std::sync::RwLock<Option<SendCredentialsProbe>>,
    /// P1-1 开关执行门（发送最终派发复核 / 附件写盘 / 控制台准入共用注入槽）：
    /// 实例 ID → `Ok` 放行 / `Err` 拒绝（生产 = [`super::switch::enforcement_gate`]，
    /// 读持久化开关记录）。`None` = 未装配（disabled hub 与既有测试基线）→ 放行
    /// ——生产装配恒注入；拒绝事实源（开关记录）由 switch 域保证。
    switch_gate: std::sync::RwLock<Option<SwitchExecutionGate>>,
    /// 启动恢复门：`false` = 崩溃遗留的 reserved 尚未恢复，写入口关闭。
    ///
    /// 复审「启动恢复顺序」：必须先恢复、失败则拒绝写，再开放服务——否则新请求
    /// 可能落进恢复窗口。
    write_gate: std::sync::atomic::AtomicBool,
    /// 实时问答 waterfall（T-live）：常驻 `$events` 订阅 + `$events/result` 回执。
    ///
    /// **与 [`questions`] 的分工不可混用**：本字段承载「此刻正在等用户」的提问
    /// （`user-questions/request` waterfall），[`questions`] 承载回合结束后
    /// `continued` 的补答（`userQuestions/answer`）。详见
    /// [`question_events`] 模块文档的两条路径对照表。
    live_questions: Arc<question_events::LiveQuestionService>,
}

/// P1-1 开关执行门注入面（hub 持有；发送/附件/控制台三类执行点共用）：
/// 实例 ID → `Ok` 放行 / `Err`（`DSH_SWITCH_OFF` 等）拒绝。生产装配 =
/// `switch::enforcement_gate`（持久化记录判定）；测试注入基桩/夹具服务闭包。
pub type SwitchExecutionGate = Arc<dyn Fn(&str) -> Result<(), DshError> + Send + Sync>;

impl DshControlHub {
    /// 全能力禁用实例（§2.5）：测试与「未启用装配」共用；除 `dispatch` 外任何
    /// 入口 → `DSH_UNSUPPORTED`。`dispatch` 先判装配，故同样返回 `DSH_UNSUPPORTED`
    /// （写门判定在其后，仅对已装配实例生效）。
    pub fn disabled_arc() -> Arc<Self> {
        Arc::new(Self {
            db: None,
            actor: actor::DshSessionActor::new(),
            #[cfg(test)]
            dispatch_hook: None,
            #[cfg(test)]
            db_lane_entries: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            lane_scope: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            db_outside_lane: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            recovery_consumed: std::sync::atomic::AtomicBool::new(false),
            native: None,
            host_probe: None,
            session_resolver: None,
            readonly_root: std::sync::RwLock::new(None),
            native_ready: None,
            contract_read: None,
            contract_cache: std::sync::Mutex::new(None),
            effects: None,
            auto_adapt_source: std::sync::RwLock::new(Arc::new(production::dsh_auto_adapt_enabled)),
            credentials_probe: std::sync::RwLock::new(None),
            switch_gate: std::sync::RwLock::new(None),
            write_gate: std::sync::atomic::AtomicBool::new(false),
            // 测试/禁用装配：实时问答未接线（ensure_subscribed/answer 恒
            // DSH_UNSUPPORTED，待答表可查且恒空）。生产装配见 production()。
            live_questions: Arc::new(question_events::LiveQuestionService::new()),
        })
    }

    /// 生产实例：复用全局数据库单连接（不新开写连接）。
    ///
    /// **T10 复审 P1-1 裁决：生产真接原生发送**——长生命周期 [`actions::PeerChannel`]
    /// （T07 档案证据「cookie 与一条物理 socket 生死绑定」→ per-message 建连是
    /// 结构性错误；全部一元写串行复用一条长持有连接）。授权域初始化与连接建立
    /// 都**惰性到首次发送**（构造 hub 零授权域副作用；auth 域坏 → 首发如实报
    /// `DSH_IPC_STATE` 等启动期码，不冒充可发送）。
    pub fn production() -> Arc<Self> {
        let native: Option<Arc<dyn actions::NativeDispatch>> =
            Some(Arc::new(actions::InstancePeerRouter));
        // Host/epoch 探针 = 生产实例登记的已选实例核验（H04：不再直接 discover——
        // 单候选兼容口径在 resolve_selected 内；已选实例消失/身份不连续时报错，
        // 绝不回退首项。阻塞进程扫描；探针在 blocking 池执行）。
        let probe: Option<actions::HostProbe> = Some(Arc::new(|| {
            let registry = production::production_registry()?;
            registry.resolve_selected()
        }));
        // I4③：新写入口的会话归属解析器 = 生产登记的 resolve_session（绑定并复核
        // 实例+会话；归属缺失的新写在此拒绝，读路径不受影响）。
        let session_resolver: SessionResolver = Arc::new(|instance_id: &str, session_id: &str| {
            let registry = production::production_registry()?;
            registry.resolve_session(instance_id, session_id)
        });
        // P1-D：就绪探针 = 授权域当前代际句柄存在（peer_for_control）。
        let ready: Option<actions::NativeReadyProbe> = Some(Arc::new(|| {
            Box::pin(async move {
                let Ok(registry) = production::production_registry() else {
                    return false;
                };
                let Ok(host) = registry.resolve_selected() else {
                    return false;
                };
                actions::PeerChannel::for_instance(host.host_id)
                    .ready()
                    .await
            })
        }));
        // T3 发送预检三态探针：当代握手/发送凭据（与上面布尔就绪探针的区别：
        // 三态能把「正在恢复连接」（旧代凭据待重交付）与「请在电脑上检查所选
        // 实例」（无绑定/已撤权/宿主不可证）分开呈现，不折叠成同一个失败）。
        let credentials_probe: Option<SendCredentialsProbe> = Some(Arc::new(
            |instance_id: &str| match production::auth_for_target(instance_id) {
                Ok(svc) => svc.control_peer_state(),
                Err(_) => auth::ControlPeerState::NotAuthorized,
            },
        ));
        // P1-A：契约识别 = 有预算的包扫描（阻塞 IO——调用方包 spawn_blocking）。
        let contract_read: Option<actions::ContractReadProbe> = Some(Arc::new(|host| {
            contract::read_report(host, contract::CONTRACT_RULE_VERSION)
        }));
        // P1-B：效果源 = 同一长生命周期信道上的 session/follow 订阅。
        let effects: Option<Arc<dyn actions::EffectSource>> =
            Some(Arc::new(actions::InstancePeerRouter));
        // P1-1：开关执行门（发送最终派发复核/附件写盘/控制台准入共用）——生产
        // 装配 = 持久化开关记录判定（`switch::enforcement_gate`）。
        let switch_gate: SwitchExecutionGate =
            Arc::new(|instance_id: &str| switch::enforcement_gate(instance_id));
        Arc::new(Self {
            db: Some(DshDb::Global),
            actor: actor::DshSessionActor::new(),
            #[cfg(test)]
            dispatch_hook: None,
            #[cfg(test)]
            db_lane_entries: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            lane_scope: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            db_outside_lane: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            recovery_consumed: std::sync::atomic::AtomicBool::new(false),
            native,
            host_probe: probe,
            session_resolver: Some(session_resolver),
            readonly_root: std::sync::RwLock::new(Some(Arc::new(|id, sid| {
                production::production_registry()?.resolve_readonly_root(id, sid)
            }))),
            native_ready: ready,
            contract_read,
            contract_cache: std::sync::Mutex::new(None),
            effects,
            auto_adapt_source: std::sync::RwLock::new(Arc::new(production::dsh_auto_adapt_enabled)),
            credentials_probe: std::sync::RwLock::new(credentials_probe),
            switch_gate: std::sync::RwLock::new(Some(switch_gate)),
            write_gate: std::sync::atomic::AtomicBool::new(false),
            // 实时问答：常驻订阅与回执都经每实例 PeerChannel 单例（网关要求回执方
            // 与开流方是同一 Peer）；不开流前零副作用（惰性到首个 ensure_subscribed）。
            live_questions: Arc::new(question_events::LiveQuestionService::production()),
        })
    }

    /// 注入库实例（测试用；生产用 [`Self::production`]）。
    ///
    /// **写门默认关闭（fail-closed）**：任何路径（含测试）都必须先过启动恢复
    /// `startup_recovery()` 才放行写——与生产顺序一致，杜绝「构造即开闸」
    /// 让恢复前写入可达（复审 M3）。测试若只测非恢复路径，显式 `open_write_gate()`。
    pub fn with_db(db: DshDb) -> Arc<Self> {
        Arc::new(Self {
            db: Some(db),
            actor: actor::DshSessionActor::new(),
            #[cfg(test)]
            dispatch_hook: None,
            #[cfg(test)]
            db_lane_entries: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            lane_scope: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            db_outside_lane: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            recovery_consumed: std::sync::atomic::AtomicBool::new(false),
            native: None,
            host_probe: None,
            session_resolver: None,
            readonly_root: std::sync::RwLock::new(None),
            native_ready: None,
            contract_read: None,
            contract_cache: std::sync::Mutex::new(None),
            effects: None,
            auto_adapt_source: std::sync::RwLock::new(Arc::new(production::dsh_auto_adapt_enabled)),
            credentials_probe: std::sync::RwLock::new(None),
            switch_gate: std::sync::RwLock::new(None),
            write_gate: std::sync::atomic::AtomicBool::new(false),
            // 测试/禁用装配：实时问答未接线（ensure_subscribed/answer 恒
            // DSH_UNSUPPORTED，待答表可查且恒空）。生产装配见 production()。
            live_questions: Arc::new(question_events::LiveQuestionService::new()),
        })
    }

    /// T10 构造：注入原生发送器与 Host 探针（dispatch 走「复核 → 原生发送 →
    /// 终态 → 泵推进」全流程）。写门默认关——先 `startup_recovery()` 或显式开闸。
    pub fn with_native(
        db: DshDb,
        native: Arc<dyn actions::NativeDispatch>,
        host_probe: actions::HostProbe,
        session_resolver: Option<SessionResolver>,
    ) -> Arc<Self> {
        Arc::new(Self {
            db: Some(db),
            actor: actor::DshSessionActor::new(),
            #[cfg(test)]
            dispatch_hook: None,
            #[cfg(test)]
            db_lane_entries: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            lane_scope: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            db_outside_lane: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            recovery_consumed: std::sync::atomic::AtomicBool::new(false),
            native: Some(native),
            host_probe: Some(host_probe),
            session_resolver,
            readonly_root: std::sync::RwLock::new(None),
            native_ready: None,
            contract_read: None,
            contract_cache: std::sync::Mutex::new(None),
            effects: None,
            auto_adapt_source: std::sync::RwLock::new(Arc::new(production::dsh_auto_adapt_enabled)),
            credentials_probe: std::sync::RwLock::new(None),
            switch_gate: std::sync::RwLock::new(None),
            write_gate: std::sync::atomic::AtomicBool::new(false),
            // 测试/禁用装配：实时问答未接线（ensure_subscribed/answer 恒
            // DSH_UNSUPPORTED，待答表可查且恒空）。生产装配见 production()。
            live_questions: Arc::new(question_events::LiveQuestionService::new()),
        })
    }

    /// P1-D 测试构造：显式注入就绪探针（凭据延迟交付回归）。
    pub fn with_readiness(
        db: DshDb,
        native: Arc<dyn actions::NativeDispatch>,
        host_probe: actions::HostProbe,
        ready: Arc<std::sync::atomic::AtomicBool>,
    ) -> Arc<Self> {
        let probe: actions::NativeReadyProbe = Arc::new(move || {
            let flag = Arc::clone(&ready);
            Box::pin(async move { flag.load(std::sync::atomic::Ordering::SeqCst) })
        });
        Arc::new(Self {
            db: Some(db),
            actor: actor::DshSessionActor::new(),
            #[cfg(test)]
            dispatch_hook: None,
            #[cfg(test)]
            db_lane_entries: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            lane_scope: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            db_outside_lane: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            recovery_consumed: std::sync::atomic::AtomicBool::new(false),
            native: Some(native),
            host_probe: Some(host_probe),
            session_resolver: None,
            readonly_root: std::sync::RwLock::new(None),
            native_ready: Some(probe),
            contract_read: None,
            contract_cache: std::sync::Mutex::new(None),
            effects: None,
            auto_adapt_source: std::sync::RwLock::new(Arc::new(production::dsh_auto_adapt_enabled)),
            credentials_probe: std::sync::RwLock::new(None),
            switch_gate: std::sync::RwLock::new(None),
            write_gate: std::sync::atomic::AtomicBool::new(false),
            // 测试/禁用装配：实时问答未接线（ensure_subscribed/answer 恒
            // DSH_UNSUPPORTED，待答表可查且恒空）。生产装配见 production()。
            live_questions: Arc::new(question_events::LiveQuestionService::new()),
        })
    }

    /// P1-2 测试面：覆盖自动适配开关读取（泵路径与直发同步生效；生产恒缺省
    /// 实现——集成测试注入本用例的 AtomicBool 投影；doc(hidden) 防误用）。
    #[doc(hidden)]
    pub fn set_auto_adapt_source(&self, source: Arc<dyn Fn() -> bool + Send + Sync>) {
        *self
            .auto_adapt_source
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = source;
    }

    /// T3 测试面：覆盖发送凭据三态探针（发送预检区分「恢复中/未授权/就绪」的
    /// 回归注入缝；生产恒缺省实现；doc(hidden) 防误用）。
    #[doc(hidden)]
    pub fn set_credentials_probe(&self, probe: SendCredentialsProbe) {
        *self
            .credentials_probe
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(probe);
    }

    /// P1-1 注入面：覆盖开关执行门（生产在 `production()` 装配真实判定；测试
    /// 注入基桩/夹具服务闭包；doc(hidden) 防误用）。
    #[doc(hidden)]
    pub fn set_switch_gate(&self, gate: Option<SwitchExecutionGate>) {
        *self
            .switch_gate
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = gate;
    }

    /// P1-1 开关执行门（发送最终派发复核 / 附件写盘 / 控制台准入三类执行点共用）：
    /// 生产装配恒走持久化开关记录判定；未装配（disabled hub / 既有测试基线）→
    /// 放行——拒绝事实源由开关记录本身保证，不在此伪造。
    pub fn check_switch_gate(&self, instance_id: &str) -> Result<(), DshError> {
        let gate = self
            .switch_gate
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        match gate {
            Some(gate) => gate(instance_id),
            None => Ok(()),
        }
    }

    /// P1-D：原生通道是否就绪（未装配探针 = 就绪——仅账本模式泵本就不发原生）。
    /// **M4（复审）**：生产探针内含阻塞进程扫描（peer_for_control → discover），
    /// 必须 spawn_blocking——不得在 async worker 上直接跑（泵每轮/启动等待每 2s
    /// 都会调它）。
    pub async fn native_ready(&self) -> bool {
        match &self.native_ready {
            None => true,
            // M4：探针 future 内含阻塞扫描（peer_for_control → discover 的 sysinfo
            // 全量刷新）。spawn_blocking 里 `block_on` 驱动它——阻塞面全部落在
            // blocking 池线程，async worker 不被占住。
            Some(probe) => {
                let probe = Arc::clone(probe);
                match tokio::task::spawn_blocking(move || {
                    tokio::runtime::Builder::new_current_thread()
                        .build()
                        .map(|rt| rt.block_on(probe()))
                        .unwrap_or(false)
                })
                .await
                {
                    Ok(ready) => ready,
                    Err(e) => {
                        log::warn!("dsh native_ready probe task failed: {e}");
                        false
                    }
                }
            }
        }
    }

    /// P1-A/P1-B 测试构造：注入契约识别 canned 探针与效果源（门/闭环回归）。
    pub fn with_contract_read(
        db: DshDb,
        native: Arc<dyn actions::NativeDispatch>,
        host_probe: actions::HostProbe,
        read: actions::ContractReadProbe,
        session_resolver: Option<SessionResolver>,
    ) -> Arc<Self> {
        Self::with_pipeline(db, native, host_probe, read, None, session_resolver)
    }

    /// P1-B 全量测试构造：契约探针 + 效果源。
    pub fn with_pipeline(
        db: DshDb,
        native: Arc<dyn actions::NativeDispatch>,
        host_probe: actions::HostProbe,
        read: actions::ContractReadProbe,
        effects: Option<Arc<dyn actions::EffectSource>>,
        session_resolver: Option<SessionResolver>,
    ) -> Arc<Self> {
        Arc::new(Self {
            db: Some(db),
            actor: actor::DshSessionActor::new(),
            #[cfg(test)]
            dispatch_hook: None,
            #[cfg(test)]
            db_lane_entries: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            lane_scope: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            db_outside_lane: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            recovery_consumed: std::sync::atomic::AtomicBool::new(false),
            native: Some(native),
            host_probe: Some(host_probe),
            session_resolver,
            readonly_root: std::sync::RwLock::new(None),
            native_ready: None,
            contract_read: Some(read),
            contract_cache: std::sync::Mutex::new(None),
            effects,
            auto_adapt_source: std::sync::RwLock::new(Arc::new(production::dsh_auto_adapt_enabled)),
            credentials_probe: std::sync::RwLock::new(None),
            switch_gate: std::sync::RwLock::new(None),
            write_gate: std::sync::atomic::AtomicBool::new(false),
            // 测试/禁用装配：实时问答未接线（ensure_subscribed/answer 恒
            // DSH_UNSUPPORTED，待答表可查且恒空）。生产装配见 production()。
            live_questions: Arc::new(question_events::LiveQuestionService::new()),
        })
    }

    /// **仅测试**：注入车道内钩子（见 `dispatch_hook` 字段说明）。
    #[cfg(test)]
    pub fn set_dispatch_hook(&mut self, hook: Box<dyn Fn() + Send + Sync>) {
        self.dispatch_hook = Some(hook);
    }

    /// **仅测试**：生产装配是否走 `DshDb::Global`（复审 M2：`production()` 改成
    /// `Owned(新连接)` 此前不会红——本探针把它锁住）。
    #[cfg(test)]
    pub(crate) fn db_is_global(&self) -> bool {
        matches!(self.db, Some(DshDb::Global))
    }

    /// **仅测试**：生产装配四件套是否齐全（M6-d + M3 复审：native/探针/就绪
    /// 探针/契约识别/效果源——任一被摘即红；退回仅账本模式必须在此暴露）。
    /// P1-1：开关执行门同属生产装配（发送/附件/控制台三类执行点的拒绝事实源）。
    #[cfg(test)]
    pub(crate) fn production_pipeline_wired(&self) -> bool {
        self.native.is_some()
            && self.host_probe.is_some()
            && self.native_ready.is_some()
            && self.contract_read.is_some()
            && self.effects.is_some()
            && self
                .switch_gate
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some()
    }

    /// 是否已装配（禁用实例恒 false；前端与 hub 入口据此显示「未接入」）。
    pub fn is_wired(&self) -> bool {
        self.db.is_some()
    }

    /// 注入只读登记解析器，供隔离环境复用同一归属检查。
    pub fn set_readonly_root_resolver(&self, resolver: ReadonlyRootResolver) {
        *self
            .readonly_root
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(resolver);
    }

    pub fn resolve_readonly_root(
        &self,
        instance_id: &str,
        session_id: &str,
    ) -> Result<std::path::PathBuf, DshError> {
        let resolver = self
            .readonly_root
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| Self::unsupported("readonly root"))?;
        resolver(instance_id, session_id)
    }

    /// 复核选定实例与会话归属，供附件上传复用发送的同一解析链。
    pub fn resolve_owned_session(
        &self,
        instance_id: &str,
        session_id: &str,
    ) -> Result<model::Target, DshError> {
        let host = self
            .host_probe
            .as_ref()
            .ok_or_else(|| Self::unsupported("host probe"))?()?;
        if host.host_id != instance_id {
            return Err(DshError::new(
                codes::INSTANCE_SWITCHED,
                "session instance is not selected",
            ));
        }
        let target = self
            .session_resolver
            .as_ref()
            .ok_or_else(|| Self::unsupported("session resolver"))?(
            instance_id, session_id
        )?;
        if target.host_id != host.host_id
            || target.session_id != session_id
            || target.epoch != host.epoch
        {
            return Err(DshError::new(
                codes::SESSION_OWNERSHIP_UNPROVEN,
                "session target does not match selected host",
            ));
        }
        Ok(target)
    }

    /// 写入口是否已放行（启动恢复门）。
    pub fn write_gate_open(&self) -> bool {
        self.write_gate.load(std::sync::atomic::Ordering::Acquire)
    }

    fn unsupported(op: &str) -> DshError {
        DshError::new(
            codes::UNSUPPORTED,
            format!("dsh control hub: {op} is not wired yet"),
        )
    }

    /// 写入口关闭（启动恢复失败时调用；此后新写一律拒绝直到重新放行）。
    pub fn close_write_gate(&self) {
        self.write_gate
            .store(false, std::sync::atomic::Ordering::Release);
    }

    /// 放行写入口（启动恢复成功后调用）。
    pub fn open_write_gate(&self) {
        self.write_gate
            .store(true, std::sync::atomic::Ordering::Release);
    }

    fn db(&self, op: &str) -> Result<&DshDb, DshError> {
        self.db.as_ref().ok_or_else(|| Self::unsupported(op))
    }

    /// 会话快照（§2.3：`snapshot(session_id)` → Snapshot）。T10 接线。
    pub async fn snapshot(&self, _session_id: &str) -> Result<SessionSnapshot, DshError> {
        Err(Self::unsupported("snapshot"))
    }

    /// 动作派发（§2.3：`dispatch(device, request)` → Receipt）。
    ///
    /// **单会话 actor（T09 验收）**：登记与占用都在该会话的串行车道内完成——
    /// 同一会话的两个不同 actionId 不会同时占用；不同会话互不阻塞。
    ///
    /// 原生派发（HTTP/Mux 写）由 T10 接在占用成功之后；当前占用成功返回
    /// `Reserved` 回执，**不谎称已受理/成功**。
    async fn dispatch_replayed(
        self: &Arc<Self>,
        device: DeviceContext,
        request: ActionRequest,
        acceptance: Option<production::SendAcceptance>,
        replay_queued: bool,
    ) -> Result<(ActionReceipt, bool), DshError> {
        // 动作占用之后必须结账，即使原 HTTP 请求先断开。关闭仍由实例代次取消未提交写入。
        let hub = Arc::clone(self);
        tokio::spawn(async move {
            hub.dispatch_and_settle(device, request, acceptance, replay_queued)
                .await
        })
        .await
        .map_err(|error| {
            DshError::new(
                codes::STATE_IO,
                format!("send settlement task failed: {error}"),
            )
        })?
    }

    async fn dispatch_and_settle(
        self: &Arc<Self>,
        device: DeviceContext,
        request: ActionRequest,
        acceptance: Option<production::SendAcceptance>,
        replay_queued: bool,
    ) -> Result<(ActionReceipt, bool), DshError> {
        // 先判装配（未装配 = 能力未接入 → UNSUPPORTED），再判写门（已装配但
        // 恢复未完成/失败 → WRITE_GATED）。顺序反了会让「未装配」被误报成
        // 「恢复失败」（复审 m1）。
        // clone 一份 DshDb（廉价：Global 是零数据变体、Owned 持一个 Arc）——
        // `&DshDb` 借自 self，不能 move 进 spawn_blocking 的 'static 闭包。
        let db = self.db("dispatch")?.clone();
        if !self.write_gate_open() {
            return Err(DshError::new(
                codes::ACTION_WRITE_GATED,
                "dsh write gate is closed (startup recovery pending or failed)",
            ));
        }
        // **M5（复审）**：Approval/Answer 载荷的 `RequestKey` 自带 host/epoch/session
        // 三元组——必须与请求 target 一致，否则账本记 target 会话、原生写却按 key
        // 指向别的会话（错会话作答）。
        match &request.payload {
            model::ActionPayload::Approval(p) if p.key.target() != request.target => {
                return Err(DshError::new(
                    codes::ACTION_INVALID,
                    "approval request key does not match the request target",
                ));
            }
            model::ActionPayload::Answer(p) if p.key.target() != request.target => {
                return Err(DshError::new(
                    codes::ACTION_INVALID,
                    "answer request key does not match the request target",
                ));
            }
            _ => {}
        }
        let session_id = request.target.session_id.clone();
        // T10 消息全流程在 blocking 段之后还需要载荷与设备——先留克隆。
        let request_after = request.clone();
        let device_after = device.clone();
        // **修复 2（T10 写入口前门）**：整个短同步段（车道互斥 + DB 事务）移入
        // `spawn_blocking`——async worker 上同步等 `Mutex`/SQLite 会饿死同 worker
        // 的其它任务（RED 用例：持锁期心跳任务 0 拍）。锁序不变（车道 → DB），
        // 事务内无网络、无 await；JoinError 显式转错误（不假成功）。
        let hub = Arc::clone(self);
        let blocking = tokio::task::spawn_blocking(move || {
            hub.actor.run_serialized_try(&session_id, || {
                // m9：入口写门检查在 spawn_blocking 之外——闭包内复核，堵住
                // 「检查后、执行前被关门」的窗口（纵深防御）。
                if !hub.write_gate_open() {
                    return Err(DshError::new(
                        codes::ACTION_WRITE_GATED,
                        "dsh write gate is closed (startup recovery pending or failed)",
                    ));
                }
                #[cfg(test)]
                hub.lane_scope
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                #[cfg(test)]
                if let Some(hook) = &hub.dispatch_hook {
                    hook();
                }
                let result = db.with(
                    |conn| -> Result<
                        (
                            model::ActionReceipt,
                            bool,
                            bool,
                            Option<production::SendAcceptance>,
                        ),
                        DshError,
                    > {
                        // 复审 Mj2：观测点必须在 **db.with 闭包内**——放在闭包外只能证明
                        // 「开始写库时在车道」，把整段 DB 移出车道仍绿（迟到复审变异 4a
                        // 实证）；放这里后该迁移会红（4c 实证 db_outside_lane ≥ 1）。
                        #[cfg(test)]
                        {
                            if !hub.lane_scope.load(std::sync::atomic::Ordering::SeqCst) {
                                hub.db_outside_lane
                                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            }
                            hub.db_lane_entries
                                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                        let store = store::DshActionStore::new(conn);
                        if replay_queued && matches!(request.payload, model::ActionPayload::Send(_))
                        {
                            let json =
                                serde_json::to_string(&request.payload).map_err(|error| {
                                    DshError::new(codes::STATE_IO, error.to_string())
                                })?;
                            let identity = store::payload_identity_digest(&json);
                            if let Some((mut receipt, confidence)) = store
                                .existing_action_for_instance(
                                    &device,
                                    &request.action_id,
                                    &request.target.session_id,
                                    &identity,
                                    &json,
                                    Some(&request.target.host_id),
                                )?
                            {
                                if confidence == store::ReplayConfidence::Unverified {
                                    receipt.reason_code = Some("identity_unverified".into());
                                }
                                return Ok((receipt, false, true, None));
                            }
                        }
                        let registered = store.register_with_acceptance(
                            &device,
                            &request,
                            acceptance.as_ref(),
                        )?;
                        if registered.state != model::ActionState::Queued {
                            // 幂等重试：原样返回既有状态（不再占用）。replayed = true
                            // 供远程层标记 duplicate（B1：移动端据此区分重放与新接单）。
                            return Ok((registered, false, true, None));
                        }
                        let claim_id = format!("claim-{}", request.action_id);
                        let reserved = store.reserve_for_dispatch_for(
                            &device,
                            &request.action_id,
                            &claim_id,
                            request.target.epoch,
                            &device.device_id,
                            &request.action_id,
                            0,
                        )?;
                        // CAS 抢输（Ok(None)）：**重读当前状态**再返回，不得把抢先读到的
                        // `Queued` 当结果（复审「抢输回执」）。此时动作已被别的请求占用。
                        //
                        // **T10 依赖申报（复审 Mj1）**：`Ok(None)` 的主路径是同会话消息类
                        // 占用被拒——动作停在 `Queued` 是**正确暂态**，但**不会自动推进**；
                        // T10 必须实现队列泵（上一个消息类动作终态后自动 reserve 下一条
                        // queued），不能要求用户/客户端重试。
                        match reserved {
                            Some(receipt) => {
                                let accepted = store.send_acceptance(&request.action_id)?;
                                Ok((receipt, true, false, accepted))
                            }
                            None => Ok((
                                store.view(&device, &request.action_id)?.receipt,
                                false,
                                false,
                                None,
                            )),
                        }
                    },
                );
                // 复位必须在**车道闭包内**（互斥仍持有）——放到闭包外会在锁释放后
                // 与下一个进入者的置位竞态，产生假阳性 db_outside_lane（复审 m2 的
                // 共享标志风险在此真实发生过）。
                #[cfg(test)]
                hub.lane_scope
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                result
            })
        });
        let (receipt, did_reserve, replayed, accepted) = blocking.await.map_err(|e| {
            DshError::new(
                codes::STATE_IO,
                format!("dispatch blocking task failed: {e}"),
            )
        })??;
        // **T10 消息全流程**：本次 dispatch 赢得占用、载荷是普通消息、且接了原生
        // 发送器 → 复核 → 原生发送 → 终态 → 泵推进。其余情况（幂等重试 / 排队 /
        // 控制类 / 仅账本模式）原样返回。
        if did_reserve && self.native.is_some() {
            if let model::ActionPayload::Send(send) = &request_after.payload {
                let dev = Arc::new(device_after.clone());
                let out = self
                    .message_flow(dev, request_after.clone(), send, accepted.as_ref())
                    .await?;
                // 首条终态（拒绝/未知/陈旧/探针错）→ 迭代泵推进排队中的后续消息。
                if matches!(
                    out.state,
                    model::ActionState::Failed | model::ActionState::UnknownDelivery
                ) {
                    self.pump_session(&request_after.target).await;
                }
                // P1-B/B3（复审）：watcher 挂载在 message_flow 唯一出口——
                // dispatch 与泵自动推进的消息**都**会被监视（只挂 dispatch 会让
                // 泵发出的第 2..N 条 Accepted 永远等不到确认，队列从第二条起停住）。
                return Ok((out, replayed));
            }
        }
        Ok((receipt, replayed))
    }

    /// 动作派发（§2.3：`dispatch(device, request)` → Receipt）——公共面丢弃
    /// replayed 标记（B1 内部穿线），行为与既有契约一致。
    pub async fn dispatch(
        self: &Arc<Self>,
        device: DeviceContext,
        request: ActionRequest,
    ) -> Result<ActionReceipt, DshError> {
        let acceptance = matches!(request.payload, model::ActionPayload::Send(_))
            .then(|| production::capture_send_acceptance(&request.target.host_id, None));
        self.dispatch_replayed(device, request, acceptance, false)
            .await
            .map(|(receipt, _replayed)| receipt)
    }

    /// P1-A（T10 复审）：**能力门**——按 T08 逐操作契约与自动适配策略判定当前
    /// build 是否允许 `Send`。判定输入全部来自已装载事实：契约识别探针（生产 =
    /// 有预算的包扫描，**同一 build 只扫一次**，结果缓存于 `contract_cache` 并
    /// 审计落库）、自动适配开关生效值（参数，由远程层从设置读取）。
    /// **阻塞**：调用方必须 spawn_blocking（生产 read_report 是阻塞 IO）。
    ///
    /// 拒绝语义（fail-closed，映射见 capability::evaluate_one）：无指纹/无证据 →
    /// `operation_no_evidence`；有指纹但该操作不在匹配表 →
    /// `operation_contract_unmatched`；契约匹配但开关关闭 →
    /// `operation_auto_adapt_off`；识别失败 → `operation_contract_read_failed`；
    /// 预算受限 → `operation_inconclusive`。
    /// shipped/observed 档暂无生产数据源（本批申报：Send 只认契约匹配档）。
    pub fn evaluate_send_gate(
        &self,
        host: &LocalHost,
        auto_adapt: bool,
    ) -> Result<model::OperationStatus, DshError> {
        self.evaluate_operation_gate(host, auto_adapt, model::Operation::Send)
    }

    fn evaluate_operation_gate(
        &self,
        host: &LocalHost,
        auto_adapt: bool,
        operation: model::Operation,
    ) -> Result<model::OperationStatus, DshError> {
        use std::collections::BTreeSet;
        // ① 证据装载（缓存命中 → 直接复用；未命中 → 识别 + 审计落库 + 缓存）。
        let now = std::time::Instant::now();
        // M6：键 = (描述符指纹, epoch, executable)——换代/换路径即失效；
        // m1：缓存整份 report,命中保留 undecided 集（「暂不可识别」不退化）。
        let cache_key = (
            host.package_sha256.clone(),
            host.epoch,
            host.executable.clone(),
        );
        let cached = self
            .contract_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .filter(|(fp, ep, exe, report, at, pkg_stat)| {
                (fp.clone(), *ep, exe.clone()) == cache_key
                    && report.evidence.rule_version == contract::CONTRACT_RULE_VERSION
                    && now.duration_since(*at) < CONTRACT_CACHE_TTL
                    // P1-b/P2（第五轮复审）：快照必须**有效**（Some——None 无法
                    // 证明任何代际）且与当前包 stat 一致（None==None 不成立）。
                    && pkg_stat.is_some()
                    && *pkg_stat == package_stat_snapshot(host)
            })
            .map(|(_, _, _, report, _, _)| report.clone());
        let (evidence, inconclusive, read_error): (
            Option<model::ContractEvidence>,
            BTreeSet<model::Operation>,
            Option<&'static str>,
        ) = match cached {
            Some(report) => (Some(report.evidence.clone()), report.undecided(), None),
            None => {
                let read = self
                    .contract_read
                    .as_ref()
                    .ok_or_else(|| Self::unsupported("capability"))?;
                // P1（第六轮复审）：**扫描前**取包文件 stat（有效快照）——扫描后
                // 重取比对；前后不全等（缺失/被替换/路径不可知）= 无法证明本次
                // 证据属于哪一个文件代际 → **拒绝本次发送**（contract_error →
                // operation_contract_read_failed），不只是不缓存。
                let pre_stat = package_stat_snapshot(host);
                match read(host) {
                    Ok(report) => {
                        let post_stat = package_stat_snapshot(host);
                        // P1（第七轮复审）：pre==post 验证通过后的 `post_stat` 就是
                        // 本次证据的**唯一合法快照**——审计等待期间包可能被替换，
                        // 缓存绝不允许再重采（第三次采样会得到「A 证据 + B stat」
                        // 的错配对，后续错误命中）。直接存已验证的 post_stat。
                        let verified_stat = post_stat;
                        if report.evidence.package_sha256.is_empty() {
                            // 预算受限/未采到指纹：**不缓存不落库**（非永久结论，
                            // 下次发送重试识别）；按「暂不可识别」拒绝。
                            (None, report.undecided(), None)
                        } else if pre_stat.is_none() || post_stat.is_none() || pre_stat != post_stat
                        {
                            // 代际无法确认：fail-closed——本次发送拒绝，不缓存不
                            // 落库不使用该证据；下次调用重扫（拿到稳定快照才放行）。
                            (
                                None,
                                report.undecided(),
                                Some("package generation unverifiable during scan"),
                            )
                        } else {
                            let ev = report.evidence.clone();
                            // MJ-1（复审）：undecided 集在 report 被 move 进缓存前取出
                            // ——首扫与缓存命中同语义（预算受限不退化成「未匹配」）。
                            let undecided = report.undecided();
                            // 审计落库（best-effort；缓存已足够本次判定）。
                            let store_ev = ev.clone();
                            let host_id = host.host_id.clone();
                            let platform = host.platform.clone();
                            let _ = self.db("capability").and_then(|db| {
                                db.with(|conn| {
                                    store::DshContractEvidenceStore::new(conn)
                                        .record(&host_id, &platform, &store_ev)
                                })
                            });
                            // m1/M6：缓存整份 report（undecided 集随行），键带
                            // epoch + executable——换代/换路径即失效重扫。
                            *self
                                .contract_cache
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((
                                host.package_sha256.clone(),
                                host.epoch,
                                host.executable.clone(),
                                report,
                                std::time::Instant::now(),
                                verified_stat,
                            ));
                            (Some(ev), undecided, None)
                        }
                    }
                    Err(e) => {
                        // 识别失败（包不可读/多义/平台不支持）：只记机器码（detail
                        // 可能含本机路径——不进上下文）。
                        (None, BTreeSet::new(), Some(e.code))
                    }
                }
            }
        };
        // ② 上下文装配（全部已装载事实；Send 是本批唯一接线操作）。
        let context = capability::OperationContext {
            // target 由同一探针结果构造——fresh 由构造保证（陈旧在 dispatch 内再复核）。
            target_fresh: true,
            auto_adapt_enabled: auto_adapt,
            read_evidence: false,
            implemented: BTreeSet::from([
                model::Operation::Send,
                model::Operation::CancelCurrent,
                model::Operation::ModelSelect,
                model::Operation::ReasoningSelect,
                model::Operation::CommandExecute,
            ]),
            current_package_sha256: evidence
                .as_ref()
                .map(|ev| ev.package_sha256.clone())
                .unwrap_or_default(),
            contract_rule_version: contract::CONTRACT_RULE_VERSION,
            shipped: None,
            observed: None,
            contract: evidence,
            contract_inconclusive: inconclusive,
            contract_error: read_error.map(|c| c.to_string()),
            drifted: BTreeSet::new(),
            auth_invalid: false,
        };
        Ok(capability::evaluate(&context)
            .get(operation)
            .cloned()
            .expect("操作必在 evaluate 的全操作表内"))
    }

    /// P1-A：发送能力门状态面（send-info 与发送同源同判定； Err = 探针/装配层
    /// 环境故障——调用方如实降级为不可发送）。
    pub async fn send_gate_status(
        self: &Arc<Self>,
        auto_adapt: bool,
    ) -> Result<model::OperationStatus, DshError> {
        let (_host, status) = self.probe_and_gate(auto_adapt).await?;
        Ok(status)
    }

    /// 发送预检同时复核当前会话归属；能力可用不等于该会话可写。
    ///
    /// T3 四轴区分（各自独立呈现，不折叠成一个通用失败）。求值序与拒绝序
    /// 分开陈述：契约门在 `probe_and_gate` 内**先求值**（一次 blocking 段），
    /// 但**拒绝顺序** = ① 实例归属（`DSH_INSTANCE_SWITCHED`，错实例的契约
    /// 结论无意义）→ ② Send 契约（`DSH_CAPABILITY_DENIED` + capability
    /// reason）→ ③ 会话归属（`DSH_SESSION_OWNERSHIP_UNPROVEN`）→
    /// ④ 当代握手/发送凭据就绪（`DSH_SEND_RECONNECTING`「正在恢复连接」/
    /// `DSH_SEND_NOT_AUTHORIZED`「请在电脑上检查所选实例」）。
    /// 最终派发前的复核在 `message_flow` 内保持不变（本函数只是展示面预检，
    /// 不是派发边界的替代）。
    pub async fn send_session_gate_status(
        self: &Arc<Self>,
        auto_adapt: bool,
        session_id: &str,
        instance_id: Option<&str>,
    ) -> Result<model::OperationStatus, DshError> {
        let (host, status) = self.probe_and_gate(auto_adapt).await?;
        if instance_id.is_some_and(|id| id != host.host_id) {
            return Err(DshError::new(
                codes::INSTANCE_SWITCHED,
                "session instance is not selected",
            ));
        }
        if !status.allowed {
            return Ok(status);
        }
        let hub = Arc::clone(self);
        let sid = session_id.to_owned();
        let id = host.host_id.clone();
        let target = tokio::task::spawn_blocking(move || hub.resolve_owned_session(&id, &sid))
            .await
            .map_err(|e| {
                DshError::new(codes::STATE_IO, format!("session probe task failed: {e}"))
            })??;
        if target.epoch != host.epoch || target.host_id != host.host_id {
            return Err(DshError::new(
                codes::SESSION_OWNERSHIP_UNPROVEN,
                "host changed during send preflight",
            ));
        }
        // T3 ④：当代握手/发送凭据就绪（恢复中/未授权两态分开）。探针内含阻塞
        // 发现 → blocking 池；探针任务失败按 NotAuthorized fail-closed（不假绿）。
        match self.send_credentials_state(&host.host_id).await {
            auth::ControlPeerState::Ready => Ok(status),
            auth::ControlPeerState::Recovering => Err(DshError::new(
                codes::SEND_RECONNECTING,
                "send credentials are being restored for the current host generation",
            )),
            auth::ControlPeerState::NotAuthorized => Err(DshError::new(
                codes::SEND_NOT_AUTHORIZED,
                "no current-generation send credentials; check the selected instance on desktop",
            )),
        }
    }

    /// T3：发送凭据/当代握手三态（未装配探针 = Ready——仅账本/测试装配不在此
    /// 阻断，交由派发边界的 PeerChannel 复核兜底）。探针内含阻塞发现，调用方
    /// 必须经由本函数（内部已 spawn_blocking），不得在 async worker 直接跑。
    async fn send_credentials_state(&self, instance_id: &str) -> auth::ControlPeerState {
        let probe = self
            .credentials_probe
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        match probe {
            None => auth::ControlPeerState::Ready,
            Some(probe) => {
                let id = instance_id.to_owned();
                tokio::task::spawn_blocking(move || probe(&id))
                    .await
                    .unwrap_or(auth::ControlPeerState::NotAuthorized)
            }
        }
    }

    /// 探针 + 能力门（一次 spawn_blocking 段完成；发送与 send-info 共用同一判定）。
    async fn probe_and_gate(
        self: &Arc<Self>,
        auto_adapt: bool,
    ) -> Result<(LocalHost, model::OperationStatus), DshError> {
        let probe = self
            .host_probe
            .clone()
            .ok_or_else(|| Self::unsupported("dispatch"))?;
        let probed: Result<Result<LocalHost, DshError>, _> =
            tokio::task::spawn_blocking(move || probe()).await;
        let host = probed
            .map_err(|e| DshError::new(codes::STATE_IO, format!("probe task failed: {e}")))??;
        let hub = Arc::clone(self);
        let h2 = host.clone();
        let status = tokio::task::spawn_blocking(move || hub.evaluate_send_gate(&h2, auto_adapt))
            .await
            .map_err(|e| DshError::new(codes::STATE_IO, format!("gate task failed: {e}")))??;
        Ok((host, status))
    }

    /// T10/P1-1c：远程发送入口的便捷面——用 hub 自己的 Host 探针定位当代 Host，
    /// 组出 target 后走 [`Self::dispatch`]（远程层不自行 discover：探针注入面唯一，
    /// 测试用假探针即可全覆盖；生产探针 = `production_registry().resolve_selected()`
    /// ——H04 起按已选实例核验，Target.host_id = 稳定实例 ID）。
    ///
    /// 探针失败（宿主未启动/监听缺失等）原样上抛——调用方如实拒绝，不排队不猜测。
    pub async fn dispatch_session_send(
        self: &Arc<Self>,
        device: DeviceContext,
        session_id: &str,
        action_id: &str,
        payload: SendPayload,
        auto_adapt: bool,
    ) -> Result<(ActionReceipt, bool), DshError> {
        self.dispatch_session_send_for_instance(
            device, session_id, action_id, payload, auto_adapt, None,
        )
        .await
    }

    /// 仅核对并读取已存在的发送动作，不执行新写准入或派发。
    pub async fn replay_session_send(
        &self,
        device: DeviceContext,
        session_id: &str,
        action_id: &str,
        payload: SendPayload,
        instance_id: Option<&str>,
    ) -> Result<Option<ActionReceipt>, DshError> {
        let db = self.db("dispatch")?.clone();
        let sid = session_id.to_owned();
        let action = action_id.to_owned();
        let instance = instance_id.map(str::to_owned);
        let json = serde_json::to_string(&model::ActionPayload::Send(payload))
            .map_err(|e| DshError::new(codes::STATE_IO, e.to_string()))?;
        let identity = store::payload_identity_digest(&json);
        let result = tokio::task::spawn_blocking(move || {
            db.with(|conn| {
                store::DshActionStore::new(conn).existing_action_for_instance(
                    &device,
                    &action,
                    &sid,
                    &identity,
                    &json,
                    instance.as_deref(),
                )
            })
        })
        .await
        .map_err(|e| DshError::new(codes::STATE_IO, e.to_string()))??;
        Ok(result.map(|(mut receipt, confidence)| {
            if confidence == store::ReplayConfidence::Unverified {
                receipt.reason_code = Some("identity_unverified".into());
            }
            receipt
        }))
    }

    pub async fn dispatch_session_send_for_instance(
        self: &Arc<Self>,
        device: DeviceContext,
        session_id: &str,
        action_id: &str,
        payload: SendPayload,
        auto_adapt: bool,
        instance_id: Option<&str>,
    ) -> Result<(ActionReceipt, bool), DshError> {
        self.dispatch_session_send_with_admission(
            device,
            session_id,
            action_id,
            payload,
            auto_adapt,
            instance_id,
            production::SendAdmission::capture(instance_id),
        )
        .await
    }

    pub(crate) fn capture_http_send_admission(
        &self,
        instance_id: Option<&str>,
    ) -> production::SendAdmission {
        production::SendAdmission::capture_http(instance_id, matches!(self.db, Some(DshDb::Global)))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn dispatch_session_send_with_admission(
        self: &Arc<Self>,
        device: DeviceContext,
        session_id: &str,
        action_id: &str,
        payload: SendPayload,
        auto_adapt: bool,
        instance_id: Option<&str>,
        admission: production::SendAdmission,
    ) -> Result<(ActionReceipt, bool), DshError> {
        // 既有动作先查账重放。入口受理代次只限制首次派发。
        let db0 = self.db("dispatch")?.clone();
        let dev0 = device.clone();
        let id0 = action_id.to_string();
        // P1-a（第五轮复审）：载荷身份 = tagged 载荷 JSON 摘要（换代无关）——
        // 与账本注册值核对，异会话/异载荷报冲突，不冒用同 ID。
        let incoming_payload_json =
            serde_json::to_string(&model::ActionPayload::Send(payload.clone()))
                .map_err(|e| DshError::new(codes::STATE_IO, format!("payload serialize: {e}")))?;
        let incoming_identity = store::payload_identity_digest(&incoming_payload_json);
        let sid0 = session_id.to_string();
        let instance0 = instance_id.map(str::to_owned);
        let existing = tokio::task::spawn_blocking(move || {
            db0.with(|conn| {
                store::DshActionStore::new(conn).existing_action_for_instance(
                    &dev0,
                    &id0,
                    &sid0,
                    &incoming_identity,
                    &incoming_payload_json,
                    instance0.as_deref(),
                )
            })
        })
        .await
        .map_err(|e| DshError::new(codes::STATE_IO, format!("replay probe failed: {e}")))??;
        if let Some((receipt, confidence)) = existing {
            // P2（第六轮复审）：旧终态行（payload_identity=NULL，迁移无法回填）
            // 的回退核对只能给出 Unverified——回执 reason_code 标记
            // `identity_unverified`，移动端据此**保留原锚**（同 ID 重按永远幂等
            // 重放，绝不换新 ID 双投）。Verified 与 MJ-2 收口行原样重放。
            if confidence == store::ReplayConfidence::Unverified {
                return Ok((
                    model::ActionReceipt {
                        reason_code: Some("identity_unverified".into()),
                        ..receipt
                    },
                    true,
                ));
            }
            return Ok((receipt, true));
        }
        let admitted = async {
            let (host, gate) = self.probe_and_gate(auto_adapt).await?;
            if instance_id.is_some_and(|id| id != host.host_id) {
                return Err(DshError::new(codes::INSTANCE_SWITCHED, "instance_switched"));
            }
            if !gate.allowed {
                // P1-A：能力门拒绝 = 确定未执行的预检拒绝（远程层 403，reasonCode 携
                // 门码，reason 携 capability 细分原因——ASCII 机器码，不含本机路径）。
                return Err(DshError::new(
                    codes::CAPABILITY_DENIED,
                    gate.reason_code
                        .unwrap_or_else(|| "capability_denied".into()),
                ));
            }
            let acceptance = admission.bind(&host)?;
            // I4③：新写先经会话归属解析器**绑定并复核实例+会话**（生产恒装配 =
            // `production_registry().resolve_session`；归属缺失/未核验/未装配的新写
            // 一律拒绝，读路径不受影响）。解析含登记锁与活体发现（阻塞）→ blocking 池。
            let resolver = self.session_resolver.clone().ok_or_else(|| {
                DshError::new(
                    codes::UNSUPPORTED,
                    "session ownership resolver not wired; new dsh writes refused",
                )
            })?;
            let target = {
                let instance_id = host.host_id.clone();
                let sid = session_id.to_string();
                let probed: Result<Result<model::Target, DshError>, _> =
                    tokio::task::spawn_blocking(move || resolver(&instance_id, &sid)).await;
                probed.map_err(|e| {
                    DshError::new(codes::STATE_IO, format!("resolver task failed: {e}"))
                })??
            };
            // 防御：解析器必须绑定**同一**实例与**同一**会话（错绑 = 归属不可证）。
            if target.host_id != host.host_id || target.session_id != session_id {
                return Err(DshError::new(
                    codes::SESSION_OWNERSHIP_UNPROVEN,
                    format!(
                    "session resolver bound instance {}/{session_id} to {}/{}; ownership unproven",
                    host.host_id, target.host_id, target.session_id
                ),
                ));
            }
            if target.epoch != host.epoch {
                return Err(DshError::new(
                    codes::HOST_NOT_RUNNING,
                    "host generation changed during send admission",
                ));
            }
            let request = ActionRequest {
                target,
                action_id: action_id.to_string(),
                expected_revision: None,
                payload: model::ActionPayload::Send(payload.clone()),
            };
            // B1/M1/MJ-2（复审）：replayed = 账本幂等重放——落库失败的行已在
            // mark_ledger_write_failed 直接收口 unknown_delivery，重放按账本状态
            // 原样回答（客户端保留锚，零双投）。
            self.dispatch_replayed(device.clone(), request, Some(acceptance.clone()), true)
                .await
        }
        .await;
        match admitted {
            Ok(receipt) => Ok(receipt),
            Err(error) => {
                // Another request can register this action while admission is discovering its
                // host. Read the ledger again before returning an error that could release the ID.
                match self
                    .replay_session_send(device, session_id, action_id, payload, instance_id)
                    .await?
                {
                    Some(receipt) => Ok((receipt, true)),
                    None => Err(error),
                }
            }
        }
    }

    /// T10 消息全流程：复核 → 原生发送 → 终态落账 → 推进同会话队列。
    ///
    /// 锁纪律：账本短段各自 spawn_blocking；原生发送是异步网络 IO，在**不持
    /// 任何锁**的状态下直接 await——任何时刻不持 DB 锁/车道做网络。
    async fn message_flow(
        self: &Arc<Self>,
        device: Arc<DeviceContext>,
        request: ActionRequest,
        send: &model::SendPayload,
        acceptance: Option<&production::SendAcceptance>,
    ) -> Result<ActionReceipt, DshError> {
        let action_id = request.action_id.clone();
        let target = request.target.clone();
        let accepted = acceptance
            .ok_or("dsh_acceptance_unproven")
            .and_then(|accepted| {
                production::with_send_acceptance(&target.host_id, accepted, || accepted)
                    .map_err(|error| error.code)
            });
        let acceptance = match accepted {
            Ok(accepted) => accepted,
            Err(reason) => {
                let db = self.db("dispatch")?.clone();
                let dev = (*device).clone();
                return Self::ledger(db, move |conn| {
                    store::DshActionStore::new(conn).mark_failed_for(&dev, &action_id, reason)
                })
                .await;
            }
        };
        // ① 复核（T10-B）：Host/epoch freshness + **能力门**（P1-2 第三轮复审：
        // 门必须落在**所有发送路径共用的最终边界**——泵推进的排队消息与直发
        // 一样重新读取自动适配设置并求值契约；「排队后关闭自动适配」必须在此
        // 拦下，不能只靠 dispatch 入口的一次判定）。探针/识别都是阻塞 IO，进
        // 同一段 blocking。
        let probe = self
            .host_probe
            .clone()
            .ok_or_else(|| Self::unsupported("dispatch"))?;
        let gate_read = self.contract_read.clone();
        let fresh_and_allowed: Result<(bool, Option<model::OperationStatus>), DshError> = {
            let t = target.clone();
            let hub = Arc::clone(self);
            let gated = gate_read.is_some();
            let probed: Result<Result<(bool, Option<model::OperationStatus>), DshError>, _> =
                tokio::task::spawn_blocking(move || {
                    let host = probe()?;
                    let fresh = actions::target_is_fresh(&t, &host);
                    if fresh {
                        if let Some(resolver) = &hub.session_resolver {
                            let resolved = resolver(&t.host_id, &t.session_id)?;
                            if resolved.host_id != t.host_id
                                || resolved.session_id != t.session_id
                                || resolved.epoch != t.epoch
                            {
                                return Err(DshError::new(
                                    codes::SESSION_OWNERSHIP_UNPROVEN,
                                    "session ownership changed before dispatch",
                                ));
                            }
                        }
                    }
                    // 能力门只在「已装配 + 目标新鲜」时求值（陈旧目标先按陈旧拒，
                    // 门码无意义）；识别失败按 fail-closed 处理（见下）。
                    let gate = if fresh && gated {
                        let source = hub
                            .auto_adapt_source
                            .read()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .clone();
                        match hub.evaluate_send_gate(&host, source()) {
                            Ok(status) => Some(status),
                            Err(e) => return Err(e),
                        }
                    } else {
                        None
                    };
                    Ok((fresh, gate))
                })
                .await;
            probed.map_err(|e| DshError::new(codes::STATE_IO, format!("probe task failed: {e}")))?
        };
        // B4：探针错误（宿主未启动/域坏）与陈旧目标/门拒一样是**终态原因**——
        // 落 failed 后返回，泵由调用方推进；不得把动作留在 Reserved 楔死会话。
        match fresh_and_allowed {
            Ok((true, gate)) => {
                // 能力门判定（装配了门才判；未装配 = 既有仅账本路径不变）。
                if let Some(status) = gate {
                    if !status.allowed {
                        let reason = format!(
                            "dsh_capability_denied:{}",
                            status.reason_code.unwrap_or_else(|| "denied".into())
                        );
                        let db = self.db("dispatch")?.clone();
                        let dev = (*device).clone();
                        let id = action_id.clone();
                        let receipt = Self::ledger(db, move |conn| {
                            store::DshActionStore::new(conn).mark_failed_for(&dev, &id, &reason)
                        })
                        .await?;
                        return Ok(receipt);
                    }
                }
            }
            Ok((false, _)) => {
                let db = self.db("dispatch")?.clone();
                let dev = (*device).clone();
                let id = action_id.clone();
                let receipt = Self::ledger(db, move |conn| {
                    store::DshActionStore::new(conn).mark_failed_for(&dev, &id, "dsh_target_stale")
                })
                .await?;
                return Ok(receipt);
            }
            Err(e) => {
                let db = self.db("dispatch")?.clone();
                let dev = (*device).clone();
                let id = action_id.clone();
                let reason = format!("dsh_probe_failed:{}", e.code);
                let receipt = Self::ledger(db, move |conn| {
                    store::DshActionStore::new(conn).mark_failed_for(&dev, &id, &reason)
                })
                .await?;
                return Ok(receipt);
            }
        }
        // P1-4（T10 复审）：**最终派发边界复核写门**——探针等待期间关门
        // （恢复失败/停机闸）不得再发原生请求。此处「未发送」是确定事实，
        // 落明确 Failed（不是未知投递）；泵由调用方推进（泵自身先过写门）。
        if !self.write_gate_open() {
            let db = self.db("dispatch")?.clone();
            let dev = (*device).clone();
            let id = action_id.clone();
            let receipt = Self::ledger(db, move |conn| {
                store::DshActionStore::new(conn).mark_failed_for(&dev, &id, "dsh_write_gated")
            })
            .await?;
            return Ok(receipt);
        }
        // P1-1 开关执行门（发送最终派发复核——最后一道预派发验证）：开关关闭
        // （含撤权写盘失败的 Failed(Revoke)）的实例不再发出任何原生写。此处
        // 「未发送」是确定事实——落明确 Failed（reason 携机器码），泵由调用方
        // 推进（泵路径同样经本函数过门）。判定含小文件读（缺席派生还会盘点
        // 插件事实）→ spawn_blocking，不在 async worker 上做同步 IO。join 失败
        // 按 fail-closed 收口：动作终态化（不得楔死会话），绝不冒充「门放行」。
        let gate_hub = Arc::clone(self);
        let gate_target = target.host_id.clone();
        let switch_gate =
            match tokio::task::spawn_blocking(move || gate_hub.check_switch_gate(&gate_target))
                .await
            {
                Ok(inner) => inner,
                Err(e) => Err(DshError::new(
                    codes::STATE_IO,
                    format!("switch gate task failed: {e}"),
                )),
            };
        if let Err(error) = switch_gate {
            let db = self.db("dispatch")?.clone();
            let dev = (*device).clone();
            let id = action_id.clone();
            let reason = format!("dsh_switch_off:{}", error.code);
            let receipt = Self::ledger(db, move |conn| {
                store::DshActionStore::new(conn).mark_failed_for(&dev, &id, &reason)
            })
            .await?;
            return Ok(receipt);
        }
        // ② 原生发送（异步网络 IO；此刻不持车道/DB 锁——锁纪律不破）。
        // 「发送后结局未知」的相位分类是信道责任（transport::UnaryFailure）。
        let native = Arc::clone(self.native.as_ref().expect("checked above"));
        let sent: Result<actions::NativeSendOutcome, DshError> = native
            .send_message_accepted(&target, send, &action_id, acceptance)
            .await;
        // B4：非未知投递类的 Err（401/409/协议错/授权域坏…）也必须终态化——留在
        // Reserved 会把同会话队列永久楔死。
        let outcome = match sent {
            Ok(o) => o,
            Err(e) => {
                let db = self.db("dispatch")?.clone();
                let dev = (*device).clone();
                let id2 = action_id.clone();
                let reason = format!("dsh_native_error:{}", e.code);
                let receipt = Self::ledger(db, move |conn| {
                    store::DshActionStore::new(conn).mark_failed_for(&dev, &id2, &reason)
                })
                .await?;
                return Ok(receipt);
            }
        };
        // ③ 终态落账（短阻塞段）。
        //
        // **P1-C（复审）**：原生调用已发出——此后的落账失败**不得**以 5xx 形态
        // 泄回调用方（客户端无法分辨执行相位，重按会换新 ID 造成双投）。处理：
        // 有界重试（DB 锁/瞬时故障），仍失败则**合成** `UnknownDelivery` 回执
        // （客户端语义 = 结局不可追踪 → 保留幂等键，绝不换 ID 重发）；账本行
        // 保持 Reserved，由重启恢复收口（恢复也会因库坏而失败 → 写门保持关闭，
        // 后续写全部显式拒绝——fail-closed 链条完整）。
        let db = self.db("dispatch")?.clone();
        let dev = (*device).clone();
        let id = action_id.clone();
        let outcome_owned = outcome.clone();
        let write = async move {
            let mut attempt = 0;
            loop {
                let db2 = db.clone();
                let dev2 = dev.clone();
                let id2 = id.clone();
                let outcome_iter = outcome_owned.clone();
                let result = Self::ledger(db2, move |conn| {
                    let store = store::DshActionStore::new(conn);
                    match &outcome_iter {
                        actions::NativeSendOutcome::Accepted { native_id } => {
                            store.mark_accepted_for(&dev2, &id2, native_id.as_deref())
                        }
                        actions::NativeSendOutcome::Rejected { code } => {
                            store.mark_failed_for(&dev2, &id2, code.as_str())
                        }
                        actions::NativeSendOutcome::UnknownDelivery => {
                            store.mark_unknown_delivery_for(&dev2, &id2, "dsh_delivery_unknown")
                        }
                    }
                })
                .await;
                match result {
                    Ok(receipt) => return Ok(receipt),
                    Err(e) => {
                        // m10（复审）：状态冲突（ACTION_STATE_CONFLICT——CAS 竞争）
                        // 是确定性答案不是瞬时故障，重试无意义；只有 IO 类才重试。
                        let retryable = e.code != codes::ACTION_STATE_CONFLICT
                            && e.code != codes::ACTION_NOT_FOUND;
                        attempt += 1;
                        if attempt >= 3 || !retryable {
                            return Err(e);
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        }
        .await;
        let receipt = match write {
            Ok(receipt) => receipt,
            Err(e) => {
                log::error!(
                    "dsh ledger write failed AFTER native outcome ({outcome:?}) for {action_id}: {}:{}",
                    e.code,
                    e.detail
                );
                // M1（复审）：尽力落一个「已发未追踪」持久标记——三次终态写都失败
                // 后，用最小 UPDATE（reason_code 单字段）把残留行标出来；成功则重启
                // 恢复不把它误收口成「未发送」，重放路径也能按标记回答 unknown。
                // 失败（库彻底不可写）→ 只能合成回执 + 让写门/恢复兜底。
                let mark = Self::ledger(self.db("dispatch")?.clone(), {
                    let dev = (*device).clone();
                    let id = action_id.clone();
                    move |conn| store::DshActionStore::new(conn).mark_ledger_write_failed(&dev, &id)
                })
                .await;
                if let Err(mark_err) = mark {
                    log::warn!(
                        "dsh ledger {action_id}: mark_ledger_write_failed also failed: {}:{}",
                        mark_err.code,
                        mark_err.detail
                    );
                }
                model::ActionReceipt {
                    action_id: action_id.clone(),
                    state: model::ActionState::UnknownDelivery,
                    native_id: native_id_of_outcome(&outcome),
                    reason_code: Some("dsh_ledger_write_failed".into()),
                }
            }
        };
        // P1-B/B3（复审）：受理后挂效果监视——**唯一挂载点**（dispatch 首条与泵
        // 推进的每一条都经此处）；超期未确认由收刈器兜底（不楔死会话）。
        if receipt.state == model::ActionState::Accepted {
            self.spawn_effect_watcher(
                (*device).clone(),
                target.clone(),
                action_id.clone(),
                receipt.native_id.clone(),
            );
        }
        // 终态（Failed/UnknownDelivery）时由**调用方**的迭代泵推进下一条——
        // message_flow 自身不递归泵（B1：递归深度随队列线性增长，150 条即栈溢出）。
        Ok(receipt)
    }

    /// T10 泵：同会话排队消息**迭代**自动推进（每条走完整 `message_flow`）。
    ///
    /// - **迭代不递归**（B1）：`message_flow` 内不再回泵，外层 loop 逐条排水，
    ///   深队列（如 200 条）不会栈溢出；
    /// - 只推进 **queued** 动作——`unknown_delivery` 属「不自动重发」纪律；
    /// - **过写门**（M5）：门关（恢复失败/未完成）时零原生发送。
    async fn pump_session(self: &Arc<Self>, target: &Target) {
        self.pump_session_for_delivery(target, None).await;
    }

    async fn pump_session_for_delivery(
        self: &Arc<Self>,
        target: &Target,
        delivery_generation: Option<u64>,
    ) {
        let delivery_current = || {
            delivery_generation.is_none_or(|generation| {
                production::with_open_instance_generation(&target.host_id, generation, || ())
                    .is_ok()
            })
        };
        loop {
            if !self.write_gate_open() || !delivery_current() {
                return;
            }
            let db = match self.db("dispatch") {
                Ok(db) => db.clone(),
                Err(_) => return,
            };
            let host = target.host_id.clone();
            let epoch = target.epoch.to_string();
            let session = target.session_id.clone();
            let next = match Self::ledger(db.clone(), move |conn| {
                store::DshActionStore::new(conn).next_queued_message(&host, &epoch, &session)
            })
            .await
            {
                Ok(next) => next,
                Err(e) => {
                    log::warn!("dsh pump: next_queued failed: {}: {}", e.code, e.detail);
                    return;
                }
            };
            let Some(queued) = next else { return };
            if !delivery_current() {
                return;
            }
            let reason = match queued.acceptance.as_ref() {
                None => Some("dsh_acceptance_unproven"),
                Some(accepted) => {
                    production::with_send_acceptance(&target.host_id, accepted, || ())
                        .err()
                        .map(|error| error.code)
                }
            };
            if let Some(reason) = reason {
                let device = model::DeviceContext {
                    device_id: queued.device_id,
                    device_name: queued.device_name,
                };
                let id = queued.action_id;
                if Self::ledger(
                    self.db("dispatch").expect("pump has a ledger").clone(),
                    move |conn| {
                        store::DshActionStore::new(conn).fail_queued_for(&device, &id, reason)
                    },
                )
                .await
                .is_err()
                {
                    return;
                }
                continue;
            }
            if !self.native_ready().await {
                return;
            }
            // M6：旧库 queued 行无载荷本体（或载荷损坏/非消息载荷）——用
            // **queued 专用失败收口**（mark_failed_for 的状态门只认 in-flight，
            // 对 queued 行无效；忽略其错误继续循环会变成对同一行的死循环——
            // 本轮排障实测）把该行移出队列后继续下一条。
            let send = match queued
                .payload_json
                .as_deref()
                .and_then(|json| serde_json::from_str::<model::ActionPayload>(json).ok())
            {
                Some(model::ActionPayload::Send(send)) => send,
                _ => {
                    let db2 = match self.db("dispatch") {
                        Ok(d) => d.clone(),
                        Err(_) => return,
                    };
                    let dev = model::DeviceContext {
                        device_id: queued.device_id.clone(),
                        device_name: queued.device_name.clone(),
                    };
                    let id = queued.action_id.clone();
                    let fail = Self::ledger(db2, move |conn| {
                        store::DshActionStore::new(conn).fail_queued_for(
                            &dev,
                            &id,
                            "dsh_payload_unusable",
                        )
                    })
                    .await;
                    if fail.is_err() {
                        // 收口失败（库不可用等）——继续循环只会对着同一行空转，退出。
                        log::warn!(
                            "dsh pump: fail_queued_for {} 失败，泵退出",
                            queued.action_id
                        );
                        return;
                    }
                    continue;
                }
            };
            let device = model::DeviceContext {
                device_id: queued.device_id.clone(),
                device_name: queued.device_name.clone(),
            };
            let request = ActionRequest {
                target: target.clone(),
                action_id: queued.action_id.clone(),
                expected_revision: None,
                payload: model::ActionPayload::Send(send.clone()),
            };
            // 本会话车道内 register + reserve（锁序同 dispatch）。
            let dev0 = device.clone();
            let req = request.clone();
            let wake_instance = target.host_id.clone();
            let session_id = target.session_id.clone();
            // 不走 register：next_queued 只返回**已存在**的 queued 行，register 的
            // 身份摘要比对会与任何非 dispatch 路径写入的行（迁移/旧库/测试直插）
            // 冲突成 ACTION_ID_CONFLICT——直接 reserve（存在性/归属在 reserve 内核验）。
            let res = self
                .ledger_in_lane(&session_id, move |conn| {
                    if delivery_generation.is_some_and(|generation| {
                        production::with_open_instance_generation(&wake_instance, generation, || ())
                            .is_err()
                    }) {
                        return Ok(None);
                    }
                    let store = store::DshActionStore::new(conn);
                    let claim = format!("claim-{}", req.action_id);
                    store
                        .reserve_for_dispatch_for(
                            &dev0,
                            &req.action_id,
                            &claim,
                            req.target.epoch,
                            &dev0.device_id,
                            &req.action_id,
                            0,
                        )
                        .map(|r| r.map(|receipt| (receipt, dev0.clone())))
                })
                .await;
            let reserved = match res {
                Ok(Some(pair)) => pair,
                _ => return,
            };
            let (receipt, dev) = reserved;
            if receipt.state != model::ActionState::Reserved {
                return;
            }
            // 全流程（含复核/发送/终态/效果挂载）——它不再回泵；终态后回到外层
            // loop 取下一条。
            let hub = Arc::clone(self);
            if hub
                .message_flow(Arc::new(dev), request, &send, queued.acceptance.as_ref())
                .await
                .is_err()
            {
                return;
            }
        }
    }

    /// T10：显式效果证据 → Succeeded（受理不算成功），随后推进该会话队列。
    pub async fn record_effect(
        self: &Arc<Self>,
        device: &DeviceContext,
        action_id: &str,
        evidence: &model::EffectEvidence,
    ) -> Result<model::ActionView, DshError> {
        // M5：效果提升与泵同样过写门——恢复失败（在途未知）期间不得继续派发队列。
        if !self.write_gate_open() {
            return Err(DshError::new(
                codes::ACTION_WRITE_GATED,
                "dsh write gate is closed (startup recovery pending or failed)",
            ));
        }
        let db = self.db("action")?.clone();
        let dev = device.clone();
        let id = action_id.to_string();
        let ev = evidence.clone();
        let view = Self::ledger(db, move |conn| {
            let store = store::DshActionStore::new(conn);
            store.succeed_with_evidence_for(&dev, &id, &ev)?;
            store.view(&dev, &id)
        })
        .await?;
        // 效果落地 = 终态 → 排队中的下一条自动推进（T10-A）。
        self.pump_session(&view.target).await;
        Ok(view)
    }

    /// 启动后全量推进：把崩溃前排队中的消息挨个送出（在途崩溃残留已被恢复收口
    /// 为 unknown_delivery，**不自动重发**；只有从未发送过的 queued 才推进）。
    pub async fn advance_all_queues(self: &Arc<Self>) {
        self.advance_queues(None).await;
    }

    pub(crate) async fn on_instance_credentials_delivered(
        self: &Arc<Self>,
        instance: &str,
        generation: u64,
    ) {
        // 新代际凭据 = 新 Peer = 新 `$events` 流：旧流的 clientId 已随旧 Peer 失效，
        // 旧待答表里的 eventId 绝不能在新代际被作答 → 作废旧表并停掉旧订阅。
        // **不在此主动开流**：订阅由读取/作答路径的 `ensure_subscribed` 惰性建立，
        // 避免「用户没用远控也常驻一条流」；下次查询会重订，宿主补投未决事件。
        self.reset_live_questions(instance);
        self.advance_queues(Some((instance, generation))).await;
    }

    async fn advance_queues(self: &Arc<Self>, delivery: Option<(&str, u64)>) {
        if !self.write_gate_open() {
            return;
        }
        let db = match self.db("dispatch") {
            Ok(d) => d.clone(),
            Err(_) => return,
        };
        let sessions = match Self::ledger(db, move |conn| {
            store::DshActionStore::new(conn).queued_message_sessions()
        })
        .await
        {
            Ok(s) => s,
            Err(_) => return,
        };
        for (host, epoch, session) in sessions {
            if let Some((instance, generation)) = delivery {
                if host != instance {
                    continue;
                }
                if production::with_open_instance_generation(instance, generation, || ()).is_err() {
                    return;
                }
            }
            let target = model::Target {
                host_id: host,
                epoch: epoch.parse().unwrap_or(0),
                session_id: session,
            };
            self.pump_session_for_delivery(&target, delivery.map(|(_, generation)| generation))
                .await;
        }
    }

    /// 账本短段（终态/查询/效果）：DB 短事务进阻塞池。
    async fn ledger<T, F>(db: DshDb, f: F) -> Result<T, DshError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<T, DshError> + Send + 'static,
        T: Send + 'static,
    {
        tokio::task::spawn_blocking(move || db.with(f))
            .await
            .map_err(|e| DshError::new(codes::STATE_IO, format!("ledger task failed: {e}")))?
    }

    /// 车道内账本短段（register + reserve 的泵路径，锁序 = 车道 → DB）。
    async fn ledger_in_lane<T, F>(&self, session_id: &str, f: F) -> Result<T, DshError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<T, DshError> + Send + 'static,
        T: Send + 'static,
    {
        let lane = self.actor.lane_for(session_id);
        let db = self.db("dispatch")?.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = lane.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            db.with(f)
        })
        .await
        .map_err(|e| DshError::new(codes::STATE_IO, format!("lane ledger task failed: {e}")))?
    }

    // --------------------------------------------------------
    // 原生问答（T03）：投影只读查询 + 整组作答提交
    // --------------------------------------------------------

    /// 读取当前可作答题组（只读）：按 `instanceId + sessionId` 核验归属后读官方
    /// `session/projections` 的 `userQuestions` 桶。
    ///
    /// 返回 `None` = 宿主未装配/投影不可读（调用方按「无已验证题组」处理，前端
    /// 回退只读卡）；返回 `Some(snapshot)` = 已核验的当前投影（`groups` 可能为空 =
    /// 确实没有可作答题组）。
    ///
    /// **三态区分（卡片闪烁修复）**：读取失败与「确实没有题组」必须分开——
    /// * `Ok(Some(snapshot))` 且 `groups` 空 = 已确证当前没有可作答题组；
    /// * `Err(_)` = 读取本身失败（PeerChannel 被占/超时/宿主拒绝），调用方必须
    ///   **保留上一份快照**并显示可诊断状态，不得据此卸载卡片（否则读一次失败
    ///   就让卡片闪一下消失）。
    ///
    /// **不使用 `attachWait`**：那是前台倒计时的独立 stream，关流即释放 claim，
    /// 不承载持久状态；查询只走投影读。
    pub async fn read_question_snapshot(
        &self,
        instance_id: &str,
        session_id: &str,
    ) -> Result<questions::DshQuestionSnapshot, DshError> {
        let target = self.resolve_owned_session(instance_id, session_id)?;
        let channel = actions::PeerChannel::for_instance(instance_id.to_string());
        let args = serde_json::json!({ "request": { "sessionId": session_id } });
        let response = channel
            .request_classified(
                &format!("dsh-questions-{}", chrono::Utc::now().timestamp_millis()),
                "session/projections",
                &args,
                Some(&target),
            )
            .await?;
        let value = match response {
            Ok(model::NativeResponse {
                result: model::NativeResult::Success { value },
                ..
            }) => value,
            // 宿主明确拒绝/信封失败：这是**读取失败**，不是「没有题组」。
            Ok(model::NativeResponse {
                result: model::NativeResult::Failure { error },
                ..
            }) => {
                return Err(DshError::new(
                    codes::ACTION_STATE_CONFLICT,
                    format!("question projection read failed: {}", error.code),
                ))
            }
            Err(failure) => {
                return Err(failure.into_error());
            }
        };
        let groups: Vec<questions::DshQuestionGroup> = questions::groups_from_projection(&value)
            .into_iter()
            .filter(|group| group.answerable())
            .collect();
        let as_of_seq = value
            .get("asOfSeq")
            .and_then(|v| v.as_i64())
            .unwrap_or_default();
        let revision_token = questions::revision_token(as_of_seq, &groups);
        Ok(questions::DshQuestionSnapshot {
            instance_id: instance_id.to_string(),
            session_id: session_id.to_string(),
            groups,
            revision_token,
            as_of_seq,
        })
    }

    /// 提交一整组问答（独立于普通消息队列的提交路径）。
    ///
    /// **前置复核**（任一不成立即拒绝，宿主零执行）：
    /// 1. 写门已放行（启动恢复完成）；2. 实例开关门放行；3. 会话归属可证；
    /// 4. 题组仍为 `continued` 且 `revisionToken` 与当前投影一致（旧视图不得
    ///    作答新投影）；5. 整组答案通过 `validate_answer_batch`。
    ///
    /// **幂等**：同 `device + actionId + 同载荷` 返回原回执（账本 `register` 语义）；
    /// 同 id 异载荷 → `ACTION_ID_CONFLICT`。**投递不明** → 账本落
    /// `UnknownDelivery` 并保留占用，绝不自动重投。
    ///
    /// **并发**：同一题组的两个并发提交由账本按 `actionId` 收敛；不同 `actionId`
    /// 抢同一题组时，后到者会因宿主侧 `REPLY_QUEUED` 被明确拒绝（宿主是最终裁决）。
    pub async fn dispatch_question_answer(
        self: &Arc<Self>,
        device: &DeviceContext,
        request: &questions::DshQuestionAnswerRequest,
    ) -> Result<questions::QuestionAnswerOutcome, DshError> {
        if !self.write_gate_open() {
            return Err(DshError::new(
                codes::ACTION_WRITE_GATED,
                "dsh write gate is closed (startup recovery pending or failed)",
            ));
        }
        let acceptance = production::capture_send_acceptance(&request.instance_id, None);
        self.check_switch_gate(&request.instance_id)?;
        let target = self.resolve_owned_session(&request.instance_id, &request.session_id)?;

        // 账本登记：同 device+actionId 幂等；异载荷冲突；跨设备按不存在拒绝。
        let action_request = model::ActionRequest {
            target: target.clone(),
            action_id: request.action_id.clone(),
            expected_revision: None,
            payload: model::ActionPayload::Answer(model::AnswerPayload {
                key: model::RequestKey {
                    client_id: device.device_id.clone(),
                    event_id: request.group_id.clone(),
                    agent_id: String::new(),
                    revision: 0,
                    host_id: request.instance_id.clone(),
                    epoch: 0,
                    session_id: request.session_id.clone(),
                },
                revision_token: Some(request.revision_token.clone()),
                answers: request
                    .answers
                    .iter()
                    .map(|answer| model::QuestionAnswer {
                        id: answer.id.clone(),
                        selected: answer.selected.clone(),
                        custom: answer.custom.clone(),
                    })
                    .collect(),
            }),
        };
        let db = self.db("question answer")?.clone();
        let dev = device.clone();
        let payload = serde_json::to_string(&action_request.payload).map_err(|error| {
            DshError::new(
                codes::STATE_IO,
                format!("question request identity: {error}"),
            )
        })?;
        let identity = store::payload_identity_digest(&payload);
        let replay_db = db.clone();
        let replay_id = request.action_id.clone();
        let replay_session = request.session_id.clone();
        let replay_instance = request.instance_id.clone();
        let replay_device = dev.clone();
        let replay = Self::ledger(replay_db, move |conn| {
            let store = store::DshActionStore::new(conn);
            match store.existing_action_for_instance(
                &replay_device,
                &replay_id,
                &replay_session,
                &identity,
                &payload,
                Some(&replay_instance),
            )? {
                Some((receipt, _)) if receipt.state != model::ActionState::Queued => {
                    store.view(&replay_device, &replay_id).map(Some)
                }
                _ => Ok(None),
            }
        })
        .await?;
        if let Some(view) = replay {
            let host_accepted = matches!(
                view.receipt.state,
                model::ActionState::Accepted | model::ActionState::Succeeded
            );
            return Ok(questions::QuestionAnswerOutcome {
                view,
                host_accepted,
            });
        }

        // 4. 当前投影复核：题组必须仍在待答集且修订令牌一致。
        // 读取失败 = 不可作答（单次提交不沿用旧快照：宁可拒绝，也不拿过期题组写宿主）。
        let snapshot = self
            .read_question_snapshot(&request.instance_id, &request.session_id)
            .await
            .map_err(|e| {
                DshError::new(
                    codes::ACTION_STATE_CONFLICT,
                    format!(
                        "question projection is unavailable ({}); refusing to answer unverified questions",
                        e.code
                    ),
                )
            })?;
        if snapshot.revision_token != request.revision_token {
            return Err(DshError::new(
                codes::ACTION_STATE_CONFLICT,
                "question revision changed since the client read it; re-read before answering",
            ));
        }
        let group = snapshot
            .groups
            .iter()
            .find(|group| group.call_id == request.group_id)
            .ok_or_else(|| {
                DshError::new(
                    codes::ACTION_STATE_CONFLICT,
                    format!(
                        "question group {} is no longer pending (answered, cancelled or expired)",
                        request.group_id
                    ),
                )
            })?;
        // 5. 整组答案对位校验（宿主 BAD_ANSWER 的前置版）。
        questions::validate_answer_batch(group, &request.answers)?;

        let id = request.action_id.clone();
        let claim_id = format!("dsh-question:{id}");
        let epoch = target.epoch;
        let group_id = request.group_id.clone();
        let revision = snapshot.as_of_seq.max(0) as u64;
        // 派发：官方 `userQuestions/answer(agent, callId, {answers})`。
        let channel = actions::PeerChannel::for_instance(request.instance_id.clone());
        let native = questions::native_answers(group, &request.answers);
        let args = serde_json::json!({
            "request": {
                "sessionId": request.session_id,
                "callId": request.group_id,
                "answer": { "answers": native },
            }
        });
        let settle_id = id.clone();
        let settle_device = dev.clone();
        tokio::spawn(async move {
            let replay = Self::ledger(db.clone(), move |conn| {
                let store = store::DshActionStore::new(conn);
                store.register(&dev, &action_request)?;
                match store.reserve_for_dispatch_for(
                    &dev,
                    &id,
                    &claim_id,
                    epoch,
                    &dev.device_id,
                    &group_id,
                    revision,
                )? {
                    Some(_) => Ok(None),
                    None => store.view(&dev, &id).map(Some),
                }
            })
            .await?;
            if let Some(view) = replay {
                let host_accepted = matches!(
                    view.receipt.state,
                    model::ActionState::Accepted | model::ActionState::Succeeded
                );
                return Ok(questions::QuestionAnswerOutcome {
                    view,
                    host_accepted,
                });
            }

            let id = settle_id;
            let dev = settle_device;
            let dispatched = channel
                .request_classified_accepted(
                    &id,
                    "userQuestions/answer",
                    &args,
                    Some((&target, &acceptance)),
                )
                .await;
            let outcome = match dispatched {
                Ok(result) => questions::classify_answer_result(result),
                Err(error) => actions::NativeSendOutcome::Rejected {
                    code: error.code.to_string(),
                },
            };
            let host_accepted = matches!(outcome, actions::NativeSendOutcome::Accepted { .. });
            let view = Self::ledger(db, move |conn| {
                let store = store::DshActionStore::new(conn);
                match outcome {
                    actions::NativeSendOutcome::Accepted { native_id } => {
                        store.mark_accepted_for(&dev, &id, native_id.as_deref())?;
                    }
                    actions::NativeSendOutcome::Rejected { code } => {
                        store.mark_failed_for(&dev, &id, &code)?;
                    }
                    actions::NativeSendOutcome::UnknownDelivery => {
                        store.mark_unknown_delivery_for(&dev, &id, "delivery_unknown")?;
                    }
                }
                store.view(&dev, &id)
            })
            .await?;
            Ok(questions::QuestionAnswerOutcome {
                view,
                host_accepted,
            })
        })
        .await
        .map_err(|error| {
            DshError::new(
                codes::STATE_IO,
                format!("continued question task failed: {error}"),
            )
        })?
    }

    // --------------------------------------------------------
    // 实时问答 waterfall（T-live）：$events 常驻订阅 + $events/result 回执
    // --------------------------------------------------------

    /// 读取**实时**待答列表（`user-questions/request` waterfall）。
    ///
    /// **什么时候用这条而不是 [`Self::read_question_snapshot`]**：
    /// * 本方法 = 回合**仍在跑**、用户当场待答的提问（`ask_user_question` 尚未返回）。
    ///   这类调用不进 `userQuestions` 投影（官方只对 timed 工具建投影条目），所以
    ///   投影路径永远看不到它们；
    /// * [`Self::read_question_snapshot`] = 回合已结束、投影里 `state == "continued"`
    ///   的补答（走 `userQuestions/answer`）。两条路互不兜底。
    ///
    /// **归属**：先 `resolve_owned_session` 核验实例与会话（与投影路径同口径），
    /// 再按 `agentId == sessionId` 精确筛选（官方语义见 [`question_events`] 模块
    /// 文档「会话归属」）。
    ///
    /// **顺带保活**：查询即确保 `$events` 订阅在位（幂等）。订阅失败不影响读取
    /// 结论——待答表仍按现有内容返回（可能是空），前端不需要为此报错。
    ///
    /// **原子读（P2-1）**：`live` 与 `pending` 取自**同一次**状态快照，避免响应里
    /// 混出「旧代待答 + 新代 live」。
    pub async fn live_questions(
        &self,
        instance_id: &str,
        session_id: &str,
    ) -> Result<question_events::LiveQuestionSnapshot, DshError> {
        let acceptance = production::capture_send_acceptance(instance_id, None);
        self.check_switch_gate(instance_id)?;
        self.resolve_owned_session(instance_id, session_id)?;
        let subscribed = production::with_send_acceptance(instance_id, &acceptance, || {
            self.live_questions.ensure_subscribed(instance_id)
        })?;
        if let Err(error) = subscribed {
            log::debug!(
                "dsh live questions: cannot subscribe to $events ({})",
                error.code
            );
        }
        Ok(self.live_questions.snapshot(instance_id, session_id))
    }

    /// 回执一条**实时**待答（用户已在手机端作答）。
    ///
    /// **与 [`Self::dispatch_question_answer`] 的区别**（别接错）：
    /// * 本方法 = 实时 waterfall 的 `$events/result`（`eventId` + `clientId` 配对，
    ///   必须与开流是同一个 Peer）。**不写动作账本**——waterfall 没有 `actionId`
    ///   语义，幂等由「同一 `eventId` 只允许一次回执」保证（重复回执明确拒绝，
    ///   不重复写宿主）；
    /// * `dispatch_question_answer` = `continued` 补答，走 `userQuestions/answer` +
    ///   `actionId` 账本。
    ///
    /// 前置复核与派发同口径：写门 [`Self::write_gate_open`] → 开关门
    /// [`Self::check_switch_gate`] → 归属 [`Self::resolve_owned_session`]。
    /// 答案与题面的对位复用 [`questions::validate_answers_for_questions`]。
    pub async fn answer_live_question(
        &self,
        _device: &DeviceContext,
        instance_id: &str,
        session_id: &str,
        event_id: &str,
        answers: &[questions::DshQuestionAnswerItem],
    ) -> Result<question_events::LiveQuestionAnswerOutcome, DshError> {
        if !self.write_gate_open() {
            return Err(DshError::new(
                codes::ACTION_WRITE_GATED,
                "dsh write gate is closed (startup recovery pending or failed)",
            ));
        }
        let acceptance = production::capture_send_acceptance(instance_id, None);
        self.check_switch_gate(instance_id)?;
        let target = self.resolve_owned_session(instance_id, session_id)?;
        production::with_send_acceptance(instance_id, &acceptance, || {
            self.live_questions.ensure_subscribed(instance_id)
        })??;
        self.live_questions
            .answer_admitted(
                instance_id,
                session_id,
                &target,
                event_id,
                answers,
                Some(acceptance),
            )
            .await
    }

    pub async fn stop_current_turn(
        self: &Arc<Self>,
        device: DeviceContext,
        instance_id: String,
        session_id: String,
        action_id: String,
        expected_turn: String,
    ) -> Result<ActionView, DshError> {
        if !self.write_gate_open() {
            return Err(DshError::new(
                codes::ACTION_WRITE_GATED,
                "dsh write gate is closed",
            ));
        }
        let acceptance = production::capture_send_acceptance(&instance_id, None);
        let db = self.db("stop current turn")?.clone();
        let hub = Arc::clone(self);
        tokio::spawn(async move {
            let checker = Arc::clone(&hub);
            let checked_instance = instance_id.clone();
            let checked_session = session_id.clone();
            let (target, root) = tokio::task::spawn_blocking(move || {
                checker.check_switch_gate(&checked_instance)?;
                let target = checker.resolve_owned_session(&checked_instance, &checked_session)?;
                let host = checker
                    .host_probe
                    .as_ref()
                    .ok_or_else(|| Self::unsupported("stop"))?()?;
                if host.host_id != target.host_id || host.epoch != target.epoch {
                    return Err(DshError::new(
                        codes::INSTANCE_SWITCHED,
                        "dsh stop target changed",
                    ));
                }
                let capability = checker.evaluate_operation_gate(
                    &host,
                    production::dsh_auto_adapt_enabled(),
                    model::Operation::CancelCurrent,
                )?;
                if !capability.allowed {
                    return Err(DshError::new(
                        codes::CAPABILITY_DENIED,
                        "dsh stop capability unavailable",
                    ));
                }
                let root = checker.resolve_readonly_root(&checked_instance, &checked_session)?;
                Ok::<_, DshError>((target, root))
            })
            .await
            .map_err(|_| DshError::new(codes::STATE_IO, "dsh stop probe failed"))??;
            use sha2::Digest as _;
            let turn_digest = sha2::Sha256::digest(expected_turn.as_bytes());
            let turn_revision = u64::from_le_bytes(turn_digest[..8].try_into().expect("fixed SHA-256 digest"));
            let request = model::ActionRequest {
                target: target.clone(),
                action_id: action_id.clone(),
                expected_revision: Some(turn_revision),
                payload: model::ActionPayload::CancelCurrent,
            };
            let ledger_device = device.clone();
            let claim = format!("dsh-stop:{action_id}");
            let event = format!("turn:{expected_turn}");
            let reservation = Self::ledger(db.clone(), move |conn| {
                let store = store::DshActionStore::new(conn);
                store.register(&ledger_device, &request)?;
                store.reserve_for_dispatch_for(&ledger_device, &request.action_id, &claim, request.target.epoch, &ledger_device.device_id, &event, 0)
            }).await?;
            if reservation.is_none() {
                return hub.action(&device, &action_id).await;
            }
            let check: actions::PreSubmitGate = Arc::new(move |target| {
                let root = root.clone();
                let expected = expected_turn.clone();
                let hub = Arc::clone(&hub);
                Box::pin(async move {
                    tokio::task::spawn_blocking(move || {
                        if hub.write_gate_open() && hub.check_switch_gate(&target.host_id).is_ok()
                            && matches!(crate::remote::content::dsh_open_turn(&root, &target.session_id), Ok(Some(ref turn)) if format!("{}:{turn}", target.epoch) == expected) {
                            return Ok(());
                        }
                        Err(DshError::new(codes::ACTION_STATE_CONFLICT, "dsh stop target is no longer the observed turn"))
                    }).await.map_err(|_| DshError::new(codes::STATE_IO, "dsh stop final probe failed"))?
                })
            });
            let outcome = match actions::PeerChannel::for_instance(instance_id)
                .request_classified_checked(&action_id, "session/cancel", &serde_json::json!({"request":{"sessionId": session_id}}), Some((&target, &acceptance)), Some(check), None).await {
                    Ok(Ok(model::NativeResponse {
                        result: model::NativeResult::Success { value },
                        ..
                    })) if value.get("accepted").and_then(|v| v.as_bool()) == Some(true) => {
                        actions::NativeSendOutcome::Accepted { native_id: None }
                    }
                    Ok(Err(transport::UnaryFailure::DeliveryUnknown(_))) => {
                        actions::NativeSendOutcome::UnknownDelivery
                    }
                    Ok(Ok(model::NativeResponse {
                        result: model::NativeResult::Success { .. },
                        ..
                    })) => actions::NativeSendOutcome::UnknownDelivery,
                Err(error) | Ok(Err(transport::UnaryFailure::NotExecuted(error))) => actions::NativeSendOutcome::Rejected { code: error.code.to_owned() },
                Ok(Ok(model::NativeResponse { result: model::NativeResult::Failure { error }, .. })) => actions::NativeSendOutcome::Rejected { code: error.code },
            };
            Self::ledger(db, move |conn| {
                let store = store::DshActionStore::new(conn);
                match outcome {
                    actions::NativeSendOutcome::Accepted { .. } => {
                        store.mark_accepted_for(&device, &action_id, None)?
                    }
                    actions::NativeSendOutcome::UnknownDelivery => {
                        store.mark_unknown_delivery_for(&device, &action_id, "delivery_unknown")?
                    }
                    actions::NativeSendOutcome::Rejected { code } => {
                        store.mark_failed_for(&device, &action_id, &code)?
                    }
                };
                store.view(&device, &action_id)
            })
            .await
        })
        .await
        .map_err(|_| DshError::new(codes::STATE_IO, "dsh stop task failed"))?
    }

    /// 实例换代/撤权：作废实时待答表并停掉常驻订阅（旧提问绝不在新代际被作答）。
    ///
    /// **调用点（P1-1）**：凭据交付、实例切换、关闭/撤权都必须调它。实现内部
    /// **先停驱动再清状态**（见 [`question_events::LiveQuestionService::reset_instance`]），
    /// 否则旧任务可能在两步之间把一条 ready 帧写进新代际。
    pub fn reset_live_questions(&self, instance_id: &str) {
        self.live_questions.reset_instance(instance_id);
    }

    /// 实时订阅是否已就绪（`true` = 已收到 `$events` ready，能收到新提问）。
    ///
    /// **需要独立于 `pending` 的用例**：确认 `live` 与 `pending` 必须同源时请用
    /// [`Self::live_questions`]（它返回一次原子快照）。
    pub fn live_questions_ready(&self, instance_id: &str) -> bool {
        self.live_questions.is_live(instance_id)
    }

    /// 动作查询（§2.3：`action(device, id)` → ActionView；校验发起设备/目标授权）。
    pub async fn action(&self, device: &DeviceContext, id: &str) -> Result<ActionView, DshError> {
        let db = self.db("action")?.clone();
        let device = device.clone();
        let id = id.to_string();
        let blocking = tokio::task::spawn_blocking(move || {
            db.with(|conn| store::DshActionStore::new(conn).view(&device, &id))
        });
        blocking.await.map_err(|e| {
            DshError::new(codes::STATE_IO, format!("action blocking task failed: {e}"))
        })?
    }

    /// 附件上传（§2.3：`upload(device, target, input)` → UploadReceipt）。T13 接线。
    pub async fn upload(
        &self,
        _device: DeviceContext,
        _target: Target,
        _input: UploadInput,
    ) -> Result<UploadReceipt, DshError> {
        Err(Self::unsupported("upload"))
    }

    /// 目录读取（§2.3：`catalog(device, target, kind)` → 目录 DTO）。T14/T16 接线。
    pub async fn catalog(
        &self,
        _device: &DeviceContext,
        _target: &Target,
        _kind: CatalogKind,
    ) -> Result<CatalogResult, DshError> {
        Err(Self::unsupported("catalog"))
    }

    /// 新建会话（§2.3：`create(device, request)` → CreateReceipt）。T16 接线。
    pub async fn create(
        &self,
        _device: DeviceContext,
        _request: CreateRequest,
    ) -> Result<CreateReceipt, DshError> {
        Err(Self::unsupported("create"))
    }

    /// P1-B：挂效果监视任务——订阅会话事件流，匹配本动作的确认事件后
    /// record_effect（唯一成功入口 → 泵自动推进）。预算内未确认 → 静默退出
    /// （收刈器按同一预算兜底收口，两处共用 [`actions::EFFECT_WATCH_BUDGET`]）。
    fn spawn_effect_watcher(
        self: &Arc<Self>,
        device: DeviceContext,
        target: Target,
        action_id: String,
        native_id: Option<String>,
    ) {
        let Some(src) = self.effects.clone() else {
            return;
        };
        let hub = Arc::clone(self);
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + actions::EFFECT_WATCH_BUDGET;
            let mut stream = match src.follow_target(&target).await {
                Ok(stream) => stream,
                Err(e) => {
                    log::warn!(
                        "dsh effect watch: follow {} failed: {}: {}（收刈器兜底）",
                        action_id,
                        e.code,
                        e.detail
                    );
                    return;
                }
            };
            loop {
                match tokio::time::timeout_at(deadline, stream.next()).await {
                    Ok(Some(event)) => {
                        // m5（复审）：流级错误（如 gateway/interactions-forbidden）
                        // 不再静默——记 warn 便于诊断「为何 15 分钟没确认」。
                        if let Some(err) = event.get("_streamError") {
                            log::warn!("dsh effect watch {action_id}: stream error: {err}");
                            return;
                        }
                        if actions::effect_matches(&event, &action_id) {
                            let evidence = model::EffectEvidence {
                                operation: model::Operation::Send,
                                native_id: native_id.clone(),
                                epoch: target.epoch,
                                effect_kind: model::EffectKind::Message,
                            };
                            match hub.record_effect(&device, &action_id, &evidence).await {
                                Ok(_) => log::info!("dsh effect confirmed for {action_id}"),
                                Err(e) => log::warn!(
                                    "dsh effect confirm {action_id} failed: {}: {}",
                                    e.code,
                                    e.detail
                                ),
                            }
                            return;
                        }
                    }
                    _ => return, // 流终结或预算到——交收刈器
                }
            }
        });
    }

    /// P1-B 收刈：把超期未确认效果的 accepted 动作收口为 unknown_delivery，
    /// 随后全量推进被释放会话的排队消息。返回收刈条数。
    pub async fn reap_unconfirmed_effects(self: &Arc<Self>) -> usize {
        let cutoff = crate::services::dsh_control::store::now_ms_public()
            - actions::EFFECT_WATCH_BUDGET.as_millis() as i64;
        let db = match self.db("dispatch") {
            Ok(db) => db.clone(),
            Err(_) => return 0,
        };
        match Self::ledger(db, move |conn| {
            store::DshActionStore::new(conn).reap_unconfirmed_accepted(cutoff)
        })
        .await
        {
            Ok(0) => 0,
            Ok(n) => {
                log::info!("dsh effect reaper: {n} accepted action(s) -> unknown_delivery");
                self.advance_all_queues().await;
                n
            }
            Err(e) => {
                log::warn!("dsh effect reaper failed: {}: {}", e.code, e.detail);
                0
            }
        }
    }

    /// M5（复审）：凭据交付后的事件驱动推进出口——auth 域在 cookie 交付成功时
    /// 调用（远晚于启动 120s 等待窗口也照样推进；排队消息不永久滞留 Queued）。
    pub async fn on_credentials_delivered(self: &Arc<Self>) {
        if !self.write_gate_open() {
            return;
        }
        log::info!("dsh credentials delivered; draining queued messages");
        self.advance_all_queues().await;
    }

    /// 应用启动时恢复崩溃遗留的 `reserved` 动作（→ `UnknownDelivery`，不重投）。
    ///
    /// **启动顺序契约**：本方法返回 Ok 后才可 `open_write_gate()`；失败则由调用方
    /// `close_write_gate()` 并保持关闭（写入口不可用，读路径仍可服务）。
    pub fn recover_actions_after_restart(&self) -> Result<usize, DshError> {
        let db = self.db("recover")?;
        db.with(|conn| store::DshActionStore::new(conn).recover_reserved_after_restart())
    }

    /// 应用启动时的**恢复门**：先恢复，成功 → 放行写；失败 → 关闭写并上抛。
    pub fn startup_recovery(&self) -> Result<usize, DshError> {
        // 复审 Major 1：一次性消费——**仅在成功后**置标记（M4：失败须可重试，
        // 否则「失败→永久 Ok(0)+门关」无自愈路径）；成功后的二次调用短路返回
        // Ok(0)，绝不再触碰在途动作（否则会把门开启后真实 reserved/accepted/
        // running 的动作误打成 unknown）。
        if self
            .recovery_consumed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(0);
        }
        match self.recover_actions_after_restart() {
            Ok(n) => {
                self.recovery_consumed
                    .store(true, std::sync::atomic::Ordering::Release);
                self.open_write_gate();
                Ok(n)
            }
            Err(e) => {
                self.close_write_gate();
                Err(e)
            }
        }
    }
}

/// 合成回执携带的归因键（accepted 时已知 rpcId；其余 None）。
fn native_id_of_outcome(outcome: &actions::NativeSendOutcome) -> Option<String> {
    match outcome {
        actions::NativeSendOutcome::Accepted { native_id } => native_id.clone(),
        _ => None,
    }
}

/// 共享禁用单例（§2.5）。
pub fn disabled_arc() -> Arc<DshControlHub> {
    DshControlHub::disabled_arc()
}

static PRODUCTION_HUB: std::sync::OnceLock<Arc<DshControlHub>> = std::sync::OnceLock::new();

/// 生产单例装配点（§2.3：Tauri 与 RemoteState 使用**同一 Arc**）。
///
/// T09 落地进程级 `OnceLock` 单例：Tauri 的 `manage(...)` 与 `RemoteState`
/// 都从这里取，任何一处的 `Arc::ptr_eq` 为真（验收⑤有测试锁定）。
/// hub 的各业务入口在 T10—T17 逐个接线；未接线的仍显式 `DSH_UNSUPPORTED`
/// （不把占位返回值显示成已支持）。
pub fn production_hub() -> Arc<DshControlHub> {
    Arc::clone(PRODUCTION_HUB.get_or_init(DshControlHub::production))
}

/// 已构造的生产 hub（**不触发装配**）。
///
/// 供「实例换代/关闭/撤权」这类**必须同步封存**的路径使用（P1-1）：这些路径在
/// 进程早期就可能跑（启动恢复/首次切换），不能为了清一张可能为空的待答表去
/// 初始化整个 hub（那会连带拉起授权域）。因此只在 hub 已存在时封存。
pub fn existing_production_hub() -> Option<Arc<DshControlHub>> {
    PRODUCTION_HUB.get().cloned()
}

/// P1-1：实例换代/关闭/撤权时**同步封存**该实例的实时问答（先停驱动再清状态）。
///
/// 幂等、无副作用（hub 未装配时什么也不做）。三个触发点都调它：
/// * 实例切换（`commands::dsh::instance_select_core`）；
/// * 关闭/撤权（`switch::RemoteSwitchService::disable` / `mark_closing`）；
/// * 凭据交付（`on_instance_credentials_delivered`）。
pub fn seal_live_questions(instance_id: &str) {
    if let Some(hub) = existing_production_hub() {
        hub.reset_live_questions(instance_id);
    }
}

#[cfg(test)]
mod tests {
    /// 复审 Ma4：B3「Tauri 与 RemoteState 共用同一 hub」的**结构性**断言——
    /// 从远程层真实取用点（`remote::state_for_test()` 背后的 `STATE`）取字段，
    /// 与 `production_hub()` 比指针。字段被赋值但无人读、或路由删了字段还在，
    /// 这条断言都会按真实装配结果说话。
    /// M6-d + M3（复审变异 5 全绿的收口）：生产 hub 必须真接**全管线五件**——
    /// 原生发送器/Host 探针/就绪探针/契约识别/效果源。任一被摘（含退回仅账本
    /// 模式）必须在此红。
    #[test]
    fn production_hub_wires_native_and_probe() {
        let hub = super::production_hub();
        assert!(
            hub.production_pipeline_wired(),
            "production() 必须装配原生发送器、Host 探针、就绪探针、契约识别与效果源"
        );
    }

    #[test]
    fn production_hub_singleton_is_shared_with_remote_state() {
        let state = crate::remote::state_for_test();
        let hub = super::production_hub();
        assert!(
            std::sync::Arc::ptr_eq(&state.dsh_control, &hub),
            "RemoteState.dsh_control 必须就是 Tauri manage 的同一 production_hub()"
        );
        assert!(hub.is_wired(), "生产 hub 必须已装配（非 disabled 占位）");
        let existing = super::existing_production_hub().expect("已装配，撤权路径必须能获取它");
        assert!(std::sync::Arc::ptr_eq(&existing, &hub));
    }

    /// 修复 2：`spawn_blocking` 任务 panic（JoinError）必须显式转错误——
    /// 不能把 join 失败吞成假成功。
    #[test]
    fn dispatch_join_error_becomes_an_error_not_fake_success() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("tuvis.db");
        {
            let conn = rusqlite::Connection::open(&db_path).expect("建库");
            crate::database::schema::init(&conn).unwrap();
        }
        let mut hub = crate::services::dsh_control::DshControlHub::with_db(
            crate::services::dsh_control::DshDb::Owned(std::sync::Arc::new(std::sync::Mutex::new(
                rusqlite::Connection::open(&db_path).expect("连接"),
            ))),
        );
        hub.open_write_gate();
        std::sync::Arc::get_mut(&mut hub)
            .expect("独占")
            .set_dispatch_hook(Box::new(|| panic!("boom")));
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("rt");
        let err = rt
            .block_on(hub.dispatch(
                crate::services::dsh_control::model::DeviceContext {
                    device_id: "d".into(),
                    device_name: "D".into(),
                },
                crate::services::dsh_control::model::ActionRequest {
                    target: crate::services::dsh_control::model::Target {
                        host_id: "local".into(),
                        epoch: 1,
                        session_id: "s".into(),
                    },
                    action_id: "a".into(),
                    expected_revision: None,
                    payload: crate::services::dsh_control::model::ActionPayload::Archive,
                },
            ))
            .expect_err("JoinError 必须转成错误");
        assert!(
            err.detail.contains("blocking task failed"),
            "必须是 join 失败的显式错误：{err:?}"
        );
    }

    /// 连接模型（用户裁决）：生产 hub 必须复用全局单连接，不得自开第二条写连接。
    ///
    /// **判别性做法**（复审 M2：读写探针在同文件任意连接上都成立，抓不到新开连接）：
    /// 另一线程持住 `connection::DB` 锁，再经 `DshDb::Global.with` 访问——若真的
    /// 复用同一把锁，本次访问必须**阻塞**到对方释放；若新开了连接则立即返回。
    #[test]
    fn production_hub_reuses_the_global_db_connection() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _guard = crate::database::connection::DB.lock().expect("global db");
            started_tx.send(()).expect("signal");
            // 持锁直到主线程读完耗时（最多 2s 兜底）。
            let _ = release_rx.recv_timeout(Duration::from_secs(2));
        });
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("holder 已持锁");

        let began = Instant::now();
        let _ = crate::services::dsh_control::DshDb::Global.with(|conn| {
            // 能进到这里说明拿到了同一把锁（若新开连接则不会等对方）。
            let _ = conn.query_row("SELECT COUNT(*) FROM settings", [], |r| r.get::<_, i64>(0));
            Ok(())
        });
        let waited = began.elapsed();
        let _ = release_tx.send(());
        let _ = holder.join();

        assert!(
            waited >= Duration::from_millis(200),
            "DshDb::Global 必须与主库共用同一把锁；实测只等了 {waited:?}，疑似新开了连接"
        );
        let production = crate::services::dsh_control::production_hub();
        assert!(production.is_wired(), "生产 hub 必须装配");
        assert!(
            production.db_is_global(),
            "production() 必须走 DshDb::Global（不得自开连接）"
        );
    }

    /// T09 验收「单会话 actor」的**行为**证明：dispatch 真的走会话车道。
    ///
    /// 做法：注入车道内钩子把「进入临界区」计数并短暂阻塞；同会话两请求并发
    /// 派发时，若串行正确则任一时刻临界区内恰 1 个；若把 actor 从 dispatch 移除，
    /// 两个钩子会同时进入 → 断言失败（该变异已在本地验证会红）。
    #[test]
    fn dispatch_runs_inside_the_session_lane() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("tuvis.db");
        {
            let conn = rusqlite::Connection::open(&db_path).expect("建库");
            crate::database::schema::init(&conn).unwrap();
        }
        let conn = rusqlite::Connection::open(&db_path).expect("hub 连接");
        let mut hub = crate::services::dsh_control::DshControlHub::with_db(
            crate::services::dsh_control::DshDb::Owned(std::sync::Arc::new(std::sync::Mutex::new(
                conn,
            ))),
        );
        hub.open_write_gate();

        let inside = std::sync::Arc::new(AtomicUsize::new(0));
        let max_inside = std::sync::Arc::new(AtomicUsize::new(0));
        let (i2, m2) = (
            std::sync::Arc::clone(&inside),
            std::sync::Arc::clone(&max_inside),
        );
        std::sync::Arc::get_mut(&mut hub)
            .expect("独占")
            .set_dispatch_hook(Box::new(move || {
                // 复审 m6：不靠 sleep 猜重叠。进入后**主动等待**另一个线程也进入
                // （最长 300ms）；串行正确时第二个永远进不来，只会等满超时。
                let now = i2.fetch_add(1, Ordering::SeqCst) + 1;
                m2.fetch_max(now, Ordering::SeqCst);
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
                while i2.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                i2.fetch_sub(1, Ordering::SeqCst);
                // 「DB 段在车道内」由 dispatch 的 lane_scope/db_outside_lane 观测
                // 断言（见下方）；本钩子只负责并发度窗口。
            }));

        let device = crate::services::dsh_control::model::DeviceContext {
            device_id: "dev-1".into(),
            device_name: "Dev".into(),
        };
        let make = |id: &str| crate::services::dsh_control::model::ActionRequest {
            target: crate::services::dsh_control::model::Target {
                host_id: "local".into(),
                epoch: 1,
                session_id: "s-lane".into(),
            },
            action_id: id.into(),
            expected_revision: None,
            payload: crate::services::dsh_control::model::ActionPayload::Archive,
        };

        let hub = std::sync::Arc::new(hub);
        let (h1, h2) = (std::sync::Arc::clone(&hub), std::sync::Arc::clone(&hub));
        let (d1, d2) = (device.clone(), device);
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let rt = tokio::runtime::Runtime::new().expect("rt");
                rt.block_on(h1.dispatch(d1, make("act-lane-1"))).expect("1");
            });
            scope.spawn(move || {
                let rt = tokio::runtime::Runtime::new().expect("rt");
                rt.block_on(h2.dispatch(d2, make("act-lane-2"))).expect("2");
            });
        });

        assert_eq!(
            max_inside.load(Ordering::SeqCst),
            1,
            "同会话两个 dispatch 的临界区不得重叠——actor 未接入 dispatch"
        );
        // 复审 M3：DB 段必须**在车道内**执行（只把钩子放车道里不算接入）。
        // dispatch 结束时标记复位，故这里改为「执行期间从未越过车道」的等价证据：
        // 两个并发 dispatch 全程临界区并发度恒为 1（上面已断言），且 DB 段标记
        // 只在车道闭包内置位——若把 db.with 移出车道，钩子与 DB 段会分属不同作用域，
        // 见 `db_inside_lane` 字段文档与 mutation 记录。
        // 复审 M3：两个并发 dispatch 的 DB 段都真的执行过（不是被跳过），
        // 且全程车道并发度为 1（上面断言）——二者合起来才证明「登记+占用」在
        // 车道内串行，而不是只把钩子塞进车道。
        assert_eq!(
            hub.db_lane_entries.load(Ordering::SeqCst),
            2,
            "两次 dispatch 都必须真的进入 DB 段"
        );
        assert_eq!(
            hub.db_outside_lane.load(Ordering::SeqCst),
            0,
            "DB 段必须在车道作用域内执行（移出车道即被判为车道外）"
        );
    }

    /// P1-1（回归 d/发送侧）：开关执行门在**最终派发边界**拒绝时——写门复核之后、
    /// 原生写出之前——动作用明确 Failed 终态化（reason 携 `DSH_SWITCH_OFF`），
    /// 原生发送器零调用；门放行时照常受理（无误伤对照）。
    #[tokio::test]
    async fn dispatch_refuses_at_final_boundary_when_switch_gate_refuses() {
        use super::production;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use super::actions;
        use super::actions::{NativeDispatch, NativeSendOutcome};
        use super::error::codes;
        use super::error::DshError;
        use super::model::{
            ActionPayload, ActionRequest, ActionState, DeviceContext, LocalHost, SendPayload,
            Target,
        };
        use super::DshControlHub;
        use super::DshDb;

        struct RecordingNative(std::sync::Arc<AtomicUsize>);
        impl NativeDispatch for RecordingNative {
            fn send_message<'a>(
                &'a self,
                _target: &'a Target,
                _payload: &'a SendPayload,
                _action_id: &'a str,
            ) -> futures::future::BoxFuture<'a, Result<NativeSendOutcome, DshError>> {
                Box::pin(async move {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    Ok(NativeSendOutcome::Accepted {
                        native_id: Some("rpc-gate-1".into()),
                    })
                })
            }
        }

        struct RestoreSelection(Option<String>);
        impl Drop for RestoreSelection {
            fn drop(&mut self) {
                production::set_selected_mirror(self.0.as_deref());
            }
        }
        let _selection_serial = production::tests::TEST_BOUNDARY_SERIAL.lock().await;
        let _selection = RestoreSelection(production::selected_mirror());
        production::set_selected_mirror(Some("inst-gate"));

        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("tuvis.db");
        {
            let conn = rusqlite::Connection::open(&db_path).expect("建库");
            crate::database::schema::init(&conn).unwrap();
        }
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let host = LocalHost {
            host_id: "inst-gate".into(),
            platform: "macos".into(),
            profile_dir: Default::default(),
            executable: Default::default(),
            pid: 42,
            started_at_ms: 1,
            epoch: 3,
            authority: "127.0.0.1:9".into(),
            package_sha256: String::new(),
        };
        let probe: actions::HostProbe = std::sync::Arc::new(move || Ok(host.clone()));
        let hub = DshControlHub::with_native(
            DshDb::Owned(std::sync::Arc::new(std::sync::Mutex::new(
                rusqlite::Connection::open(&db_path).expect("连接"),
            ))),
            std::sync::Arc::new(RecordingNative(std::sync::Arc::clone(&calls))),
            probe,
            None,
        );
        hub.open_write_gate();
        let device = DeviceContext {
            device_id: "d".into(),
            device_name: "D".into(),
        };
        let request = |action_id: &str| ActionRequest {
            target: Target {
                host_id: "inst-gate".into(),
                epoch: 3,
                session_id: "session-gate".into(),
            },
            action_id: action_id.into(),
            expected_revision: None,
            payload: ActionPayload::Send(SendPayload {
                text: "hello".into(),
                receipt_ids: Vec::new(),
                queue_only: false,
                bypass_paused_queue: false,
            }),
        };

        // 开关关闭：最终派发边界拒绝——明确 Failed（reason 携机器码），零原生调用。
        hub.set_switch_gate(Some(std::sync::Arc::new(|_: &str| {
            Err(DshError::new(
                codes::SWITCH_OFF,
                "remote-control switch is off",
            ))
        })));
        let receipt = hub
            .dispatch(device.clone(), request("act-gate-off"))
            .await
            .expect("回执");
        assert_eq!(
            receipt.state,
            ActionState::Failed,
            "必须明确 Failed: {receipt:?}"
        );
        assert!(
            receipt
                .reason_code
                .as_deref()
                .is_some_and(|reason| reason.contains("DSH_SWITCH_OFF")),
            "reason 必须携带开关错误码: {receipt:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "开关关闭时原生发送不得发生"
        );

        // 无误伤对照：门放行 → 照常受理。
        hub.set_switch_gate(Some(std::sync::Arc::new(|_: &str| Ok(()))));
        let receipt = hub
            .dispatch(device.clone(), request("act-gate-on"))
            .await
            .expect("回执");
        assert_eq!(receipt.state, ActionState::Accepted, "门放行必须照常受理");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "门放行时原生发送恰好一次");
    }
}
