//! echo-challenge 归属写门（H03）：**每一次对目标 OpenCode 服务的写动作**在实际 HTTP
//! 写之前，必须先通过一轮「探针会话回声挑战」归属核验；核验不过 → fail-closed 拒绝
//! （零新动作、零新队列项、零用户会话写）。
//!
//! 机制依据 = H02 证据文档
//! `docs/engineering/evidence/opencode-native/2026-11-14-h02-echo-challenge-attribution.md`
//! （判据定义 §8、协议参数 §7/§9、R1 复核 §12）。协议：
//!
//! 1. **challenge**：兔维斯经目标服务的 HTTP API 向**探针会话**（兔维斯自建专用
//!    session，非用户会话）覆盖写标题 `chal:<nonce>:<writetime_epoch_ms>`（PATCH，
//!    204 无 body）。该写由目标实例执行并在其总线发布。
//! 2. **ack**：TUI 侧插件（`monitor/opencode_tui_route.mjs`）每 tick 轮询
//!    `ctx.data.session.get(探针会话)`（**缓存层，只随 SSE 失效刷新**——这是
//!    v2.2.5 的实测行为事实、非正式契约，已 hash 锚定；失效为 **per-session**
//!    粒度，R1 E1c 实测），看到未应答的自己的 challenge 后用 fresh
//!    `ctx.client.session.update` 写回 `ack:<nonce>:<tui-pid>:<acktime>`（写路径落
//!    哪个实例无所谓，**判据看内容**）。
//! 3. **守卫三件套 + 硬截止**：nonce 匹配 ∧ ack 的 pid == 锁定 TUI pid ∧
//!    `writetime ≤ acktime ≤ now ≤ writetime+W`（未来时间戳、时钟异常、超窗、
//!    截止后取得的观测一律拒绝）。整轮受**单调时钟 deadline** 约束：所有轮询与
//!    IO 都不得越过剩余预算。三件齐且未过截止 → 放行；否则 fail-closed 拒绝。
//!
//! 不变量（H02 复审裁定，必须维持）：
//!
//! * 探针会话**只允许**兔维斯（challenge）与锁定 TUI（ack）写**标题**；零非标题
//!   写入、零向探针会话发消息/改元数据的路径。本模块与插件两侧共同看守；
//!   探针会话 id 在 [`GateRuntime::is_probe_session`] 注册，普通写入口据此拒绝
//!   探针目标（`OC_GATE_PROBE_TARGET`）。
//! * 拒绝判定**不依赖 lsof**；lsof SSE 长连观察仅可用于取证/拒绝条件（既有裁决，
//!   本模块不使用 lsof）。
//! * 锁定 TUI 身份 = pid + 启动时间 + boot（换代事前核对）；pid 是判据中唯一的
//!   TUI 身份防线（E4），锁定来源 = 观察注册表的新鲜 `Opened` 窗口（与活跃卡同源）。
//! * 探针会话写纪律：标题单槽、覆盖写、限频（同一目标在飞 challenge 唯一——进程内
//!   全局单飞 + 写前槽位退避检查）、每次 challenge 换新 nonce。
//! * **fail-closed**：任何 IO 失败、超时、解析失败、非协议形态标题 → 拒绝，绝不降级
//!   放行。探针读错误分诊：仅**明确删除（404）**允许受控重建一次；响应无效/其他
//!   错误立即结构化拒绝，绝不「失败混入重建后放行」。
//! * 探针控制文件授权生命周期（`<pid>.probe`）：绑定轮次（nonce）+ 进程代际
//!   （expectPid + expectBoot）+ 有效期（deadlineMs），每轮结束（无论成败）撤销。
//!   换锁定窗口后旧窗口不再持有效授权；插件写 ack 前复核授权与槽位。
//! * 失效方向：v2.2.5 行为前提单独失效（缓存不随 SSE 刷新/粒度变粗）→ challenge
//!   收不到 ack → fail-closed（可用性损失）；与语义漂移叠加才可能错误放行——
//!   升级锚定二进制后必须复验 E1/E1c（H02 结论速览 9）。
//!
//! 同机时钟假设：生产形态兔维斯与 TUI 同机，毫秒时间戳可直接比较（H02 §7）；
//! 跨机部署需先做时钟对齐，本模块不做。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;

use super::types::{codes, NativeError, RuntimeKey};

/// 归属窗口默认值（毫秒）：H02 §7 建议 W=10s（保守上限 15s），超窗一律 fail-closed。
pub(crate) const DEFAULT_WINDOW_MS: i64 = 10_000;
/// 守卫轮询间隔（毫秒）：H02 实测 ack 写 → 外部可读 ≤ 0.3s（外部轮询粒度）。
pub(crate) const DEFAULT_POLL_INTERVAL_MS: u64 = 300;
/// nonce 字节数（hex 编码后 32 位小写十六进制）。
const NONCE_BYTES: usize = 16;
/// 探针会话 ID 交付控制文件后缀（`<pid>.probe`，不带 .json——采集循环只枚举
/// `*.json`，本文件对采集零干扰）。
pub(crate) const PROBE_CONTROL_SUFFIX: &str = ".probe";
/// 探针会话初始标题（兔维斯创建时的受控初始值；「新建探针仅接受已知初始标题」）。
pub(crate) const PROBE_SESSION_TITLE: &str = "probe:tuvis";
/// 探针身份持久注册表文件（`.json` 会被采集循环枚举但解析失败即跳过，零干扰）。
const PROBE_REGISTRY_FILE: &str = "probe-registry.json";
/// 持久注册表容量上限（最新 N 个；有界防无界增长）。
const PROBE_REGISTRY_CAP: usize = 64;

/// 一扇被写门锁定的 TUI 窗口身份（观察注册表新鲜 `Opened` 产物）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockedWindow {
    pub pid: u32,
    pub started_at_ms: i64,
    /// 该窗口的插件 boot id（控制文件 `expectBoot`；插件自检进程代际）。
    pub boot: Option<String>,
}

/// 探针标题协议形态（唯一合法的两种 + 非协议形态）。写纪律：守卫**拒绝接受**任何
/// 非本协议形态的探针标题内容（验证轮内观测到 Foreign = 协议被破坏 → 拒绝）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProbeTitle {
    Challenge {
        nonce: String,
        writetime_ms: i64,
    },
    Ack {
        nonce: String,
        pid: u32,
        acktime_ms: i64,
    },
    /// 非 `chal:`/`ack:` 形态（空、噪音）。**不是任何证据**；已知初始标题
    /// [`PROBE_SESSION_TITLE`] 由编排层单独放行（无在飞 challenge）。
    Foreign,
}

/// 守卫评估结论（纯逻辑）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RoundOutcome {
    /// 三件套齐且未过截止：放行。
    Pass,
    /// 尚未见效：标题仍是自己的 challenge 且未过截止 → 继续等。
    Wait,
    /// fail-closed 拒绝（原因码可区分）。
    Reject(Refusal),
}

/// 轮内拒绝原因（窗口评估层）。编排层的其余拒绝原因（服务不可达 / 探针不可写 /
/// 锁定 TUI 不可核 / 单飞忙）直接落对应受控码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// 截止（writetime+W）内零有效 ack。
    NoAck,
    /// ack 超窗 / acktime < writetime（时钟异常）/ acktime 在未来 / 截止后取得的观测。
    LateAck,
    /// ack 的 pid ≠ 锁定 TUI pid（E4：写错实例会得到错误 TUI 的合法 ack）。
    PidMismatch,
    /// 槽位被他者 challenge/其他 nonce 的 ack 抢占。
    NonceMismatch,
    /// 槽位出现非协议形态标题（写纪律被破坏；含成功响应缺标题/响应不可解析）。
    TitleInvalid,
}

impl Refusal {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Refusal::NoAck => codes::OC_GATE_NO_ACK,
            Refusal::LateAck => codes::OC_GATE_LATE_ACK,
            Refusal::PidMismatch => codes::OC_GATE_PID_MISMATCH,
            Refusal::NonceMismatch => codes::OC_GATE_NONCE_MISMATCH,
            Refusal::TitleInvalid => codes::OC_GATE_TITLE_INVALID,
        }
    }

    fn message(self) -> &'static str {
        match self {
            Refusal::NoAck => "binding gate timed out without a valid ack",
            Refusal::LateAck => "binding gate ack arrived outside the window",
            Refusal::PidMismatch => "binding gate ack came from another tui",
            Refusal::NonceMismatch => "binding gate probe slot was taken by another round",
            Refusal::TitleInvalid => "binding gate probe slot holds a non-protocol title",
        }
    }
}

/// 受控码 → 'static 原因词（认领路径的 `declined_reason` 消费；非门码返回 None）。
pub(crate) fn reason_static(code: &str) -> Option<&'static str> {
    const GATE_CODES: [&str; 11] = [
        codes::OC_GATE_NO_ACK,
        codes::OC_GATE_LATE_ACK,
        codes::OC_GATE_PID_MISMATCH,
        codes::OC_GATE_NONCE_MISMATCH,
        codes::OC_GATE_TITLE_INVALID,
        codes::OC_GATE_PROBE_UNWRITABLE,
        codes::OC_GATE_UNREACHABLE,
        codes::OC_GATE_BUSY,
        codes::OC_GATE_PID_UNVERIFIED,
        codes::OC_GATE_PROBE_TARGET,
        codes::OC_GATE_SEMANTICS_UNPROVEN,
    ];
    GATE_CODES.iter().find(|c| **c == code).copied()
}

/// 探针目标标题形态识别（I4 双识别的第二识别，跨进程残迹兜底）：会话卡片标题是
/// 已知探针初始值或协议形态（`chal:`/`ack:`）→ 高度疑似探针会话，写入口按
/// `OC_GATE_PROBE_TARGET` 结构化拒绝（fail-closed；普通会话不取这些名字的残余
/// 边界如实记录）。
pub(crate) fn looks_like_probe_title(title: Option<&str>) -> bool {
    match title {
        Some(title) => {
            title == PROBE_SESSION_TITLE || !matches!(parse_probe_title(title), ProbeTitle::Foreign)
        }
        None => false,
    }
}

/// 生成一次 challenge 的 nonce：32 位小写十六进制（每次换新，协议硬约束）。
pub(crate) fn new_nonce() -> String {
    let mut bytes = [0u8; NONCE_BYTES];
    rand::fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// challenge 标题（协议第 1 步的写形态）。
pub(crate) fn challenge_title(nonce: &str, writetime_ms: i64) -> String {
    format!("chal:{nonce}:{writetime_ms}")
}

/// 解析探针标题（严格形态）：`chal:<32位小写hex>:<纯数字毫秒>` /
/// `ack:<32位小写hex>:<正整数pid>:<纯数字毫秒>`；其余一律 [`ProbeTitle::Foreign`]。
pub(crate) fn parse_probe_title(raw: &str) -> ProbeTitle {
    fn is_nonce(s: &str) -> bool {
        s.len() == NONCE_BYTES * 2 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    }

    fn is_ms(s: &str) -> bool {
        // 纯数字毫秒时间戳：长度受限（防畸形长串），可解析进 i64。
        !s.is_empty()
            && s.len() <= 16
            && s.bytes().all(|b| b.is_ascii_digit())
            && s.parse::<i64>().is_ok()
    }

    let Some((prefix, rest)) = raw.split_once(':') else {
        return ProbeTitle::Foreign;
    };
    match prefix {
        "chal" => {
            let Some((nonce, writetime)) = rest.split_once(':') else {
                return ProbeTitle::Foreign;
            };
            if !is_nonce(nonce) || !is_ms(writetime) {
                return ProbeTitle::Foreign;
            }
            ProbeTitle::Challenge {
                nonce: nonce.to_string(),
                writetime_ms: writetime.parse().unwrap_or_default(),
            }
        }
        "ack" => {
            let Some((nonce, rest)) = rest.split_once(':') else {
                return ProbeTitle::Foreign;
            };
            let Some((pid, acktime)) = rest.split_once(':') else {
                return ProbeTitle::Foreign;
            };
            if !is_nonce(nonce) || !is_ms(acktime) {
                return ProbeTitle::Foreign;
            }
            // pid 必须是正整数（u32；0 不是合法进程号）。
            let Ok(pid) = pid.parse::<u32>() else {
                return ProbeTitle::Foreign;
            };
            if pid == 0 {
                return ProbeTitle::Foreign;
            }
            ProbeTitle::Ack {
                nonce: nonce.to_string(),
                pid,
                acktime_ms: acktime.parse().unwrap_or_default(),
            }
        }
        _ => ProbeTitle::Foreign,
    }
}

/// 窗口评估（守卫三件套 + 硬截止的纯逻辑核心，单测主战场）。
///
/// * `nonce` / `writetime_ms`：本轮自己发出的 challenge；
/// * `locked_pid`：锁定 TUI pid（锁定来源在编排层：观察注册表新鲜 `Opened` 窗口）；
/// * `observed`：当前探针槽位标题（`None` = 读不到标题字段 → 结构化拒绝）；
/// * `now_ms` / `window_ms`：**IO 完成后重取**的当前时刻与窗口宽度。
///
/// 截止判据先于内容边界：`now > writetime+W` 之后的任何观测都不构成放行证据
///（截止后取得的 ack = [`Refusal::LateAck`]；截止后仍是自家 challenge = [`Refusal::NoAck`]）。
/// 内容边界保留：`writetime ≤ acktime ≤ writetime+W` 且 `acktime ≤ now`（未来时间戳
/// 与时钟异常同判 [`Refusal::LateAck`]）。
pub(crate) fn evaluate_observation(
    nonce: &str,
    writetime_ms: i64,
    locked_pid: u32,
    observed: Option<&str>,
    now_ms: i64,
    window_ms: i64,
) -> RoundOutcome {
    let deadline = writetime_ms.saturating_add(window_ms);
    match parse_probe_title(observed.unwrap_or_default()) {
        ProbeTitle::Ack {
            nonce: ack_nonce,
            pid,
            acktime_ms,
        } => {
            if ack_nonce != nonce {
                // 槽位是**其他轮**的 ack：本轮 challenge 已被他者覆盖写。
                return RoundOutcome::Reject(Refusal::NonceMismatch);
            }
            if pid != locked_pid {
                // E4：错误实例的 challenge 会得到错误 TUI 的合法 ack——pid 是唯一防线。
                return RoundOutcome::Reject(Refusal::PidMismatch);
            }
            // 时钟异常（ack 早于 challenge）/ 未来时间戳（ack 晚于当前时刻）/
            // 内容超窗（Δ > W）/ 截止后取得的观测（now > writetime+W）——一律拒绝。
            if acktime_ms < writetime_ms
                || acktime_ms > now_ms
                || acktime_ms.saturating_sub(writetime_ms) > window_ms
                || now_ms > deadline
            {
                return RoundOutcome::Reject(Refusal::LateAck);
            }
            RoundOutcome::Pass
        }
        ProbeTitle::Challenge {
            nonce: slot_nonce, ..
        } => {
            if slot_nonce != nonce {
                return RoundOutcome::Reject(Refusal::NonceMismatch);
            }
            if now_ms > deadline {
                return RoundOutcome::Reject(Refusal::NoAck);
            }
            RoundOutcome::Wait
        }
        // 非协议形态（空/噪音）：轮内出现即协议被破坏。读不到标题字段（None）同样
        // fail-closed。
        ProbeTitle::Foreign => RoundOutcome::Reject(Refusal::TitleInvalid),
    }
}

/// 探针 IO 的受控错误（I3 分诊：不再把「明确删除」与其他失败混为一谈）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateIoError {
    /// 传输失败（服务不可达/超时/凭据失效）。写调用零重试。
    Unreachable,
    /// 探针会话明确不存在（读 404）。**唯一**允许受控重建一次的情形。
    NotFound,
    /// 目标服务给了非 2xx（≠404）。内容不可作为证据。
    Status,
    /// 2xx 但响应不可解析 / 缺标题字段 / 形态非法：探针写纪律被破坏或形态漂移。
    InvalidBody,
}

/// 探针通道（IO 与纯逻辑分离的可测缝）。生产形态由 [`GateTransport::for_client`]
/// 用 [`NativeClient`] 构造（PATCH 改标题走 `patch_once`，与既有写纪律同款：
/// 只发一次、不重试、不跟随重定向、401 作废连接）。
/// 探针读/建会话的 future 形态（clippy type_complexity 折叠）。
pub(crate) type ProbeReadFuture =
    Box<dyn Fn(&str) -> BoxFuture<'static, Result<String, GateIoError>> + Send + Sync>;
/// 创建探针会话的 future 形态（入参 = 目标目录）。
pub(crate) type ProbeCreateFuture =
    Box<dyn Fn(&Path) -> BoxFuture<'static, Result<String, GateIoError>> + Send + Sync>;
/// 探针标题覆盖写（PATCH）的 future 形态。
pub(crate) type ProbeWriteFuture =
    Box<dyn Fn(&str, &str) -> BoxFuture<'static, Result<(), GateIoError>> + Send + Sync>;

pub(crate) struct GateTransport {
    /// 读探针会话标题：`Ok(title)`；缺失/删除/坏响应按 [`GateIoError`] 分诊。
    pub read_title: ProbeReadFuture,
    /// 创建探针会话：`Ok(id)` = 服务端新会话 id（`data.id`）；缺 id = InvalidBody。
    pub create_probe: ProbeCreateFuture,
    /// 探针标题覆盖写（PATCH）：`Ok(())` = 2xx 受理。
    pub write_title: ProbeWriteFuture,
}

/// 锁定窗口查询：目标会话 → 新鲜 `Opened` 窗口的 TUI 身份。`None` = 无新鲜确认的
/// 窗口拥有该会话（归属不可证）。
pub type LockedWindowLookup = Arc<dyn Fn(&str) -> Option<LockedWindow> + Send + Sync>;
/// TUI 进程事实：pid → 启动毫秒（`None` = 进程已死；换代/PID 复用核对用）。
pub(crate) type TuiProcessFact = Arc<dyn Fn(u32) -> Option<i64> + Send + Sync>;

/// 归属写门运行态（每个 [`NativeHub`] 一份）。
pub(crate) struct GateRuntime {
    /// 归属窗口（毫秒）。
    window_ms: i64,
    /// 守卫轮询间隔（毫秒）。
    poll_interval_ms: u64,
    /// 探针控制文件根（生产 = 用户 HOME；测试 = 临时目录）。真实 `~/.tuvis` 零读写
    /// 是测试硬约束，装门用例必须注入临时目录。
    home: PathBuf,
    /// 时钟（毫秒，墙钟）。生产 = 系统钟；测试注入。轮次的**硬截止**另用单调时钟
    ///（`tokio::time::Instant`）约束，见 [`GateRuntime::verify`]。
    now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// 进程内单飞：同一时刻全进程至多一个在飞 challenge（多目标也串行——共用探针
    /// 槽位的并发语义在单飞下天然成立，brief「并发退避」的实现口径）。
    flight: tokio::sync::Mutex<()>,
    /// H5 分段计时：单飞**等待**累计值（毫秒，只增）。观测面——`acquire_flight`
    /// 每次有界获取把等待时长累加进来，动作链经差分把它计入「排队」段。
    /// 绝不参与任何判定。
    flight_wait_total_ms: std::sync::atomic::AtomicI64,
    /// 每代际探针会话缓存（RuntimeKey → 探针 session id）。创建一次、复用；
    /// **明确删除（读 404）** → 就地重建一次。缓存同时是探针目标身份注册表
    ///（[`GateRuntime::is_probe_session`]，普通写入口据此拒绝探针目标）。
    probes: Mutex<HashMap<RuntimeKey, String>>,
    /// 探针身份**持久注册表**（I4）：跨兔维斯进程重启仍可识别历史探针会话，
    /// 堵住「重启 → 注册表空 → 旧探针被当普通会话认领/发送」的残余通道。
    /// 惰性从磁盘加载、创建时写穿；文件损坏按空表处理（残余边界如实申报）。
    probes_known: Mutex<Option<Vec<String>>>,
}

impl GateRuntime {
    /// 生产参数（窗口 10s / 轮询 300ms / 系统 HOME / 系统钟）。
    pub(crate) fn production() -> Self {
        Self::with(
            DEFAULT_WINDOW_MS,
            DEFAULT_POLL_INTERVAL_MS,
            dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")),
            Arc::new(|| chrono::Utc::now().timestamp_millis()),
        )
    }

    pub(crate) fn with(
        window_ms: i64,
        poll_interval_ms: u64,
        home: PathBuf,
        now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self {
            window_ms,
            poll_interval_ms,
            home,
            now_ms,
            flight: tokio::sync::Mutex::new(()),
            flight_wait_total_ms: std::sync::atomic::AtomicI64::new(0),
            probes: Mutex::new(HashMap::new()),
            probes_known: Mutex::new(None),
        }
    }

    /// 单飞等待累计值（毫秒）：H5 分段计时读数——调用方在每次门入口前后取差分，
    /// 把「排队（含单飞等待）」段的等待耗时交进动作计时账本。只增不减。
    pub(crate) fn flight_wait_total_ms(&self) -> i64 {
        self.flight_wait_total_ms
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 探针目标身份注册：该会话 id 是否是已知探针会话（本进程内存 ∪ 持久注册表）。
    /// 普通写入口据此拒绝把探针会话当动作目标（`OC_GATE_PROBE_TARGET`）。
    pub(crate) fn is_probe_session(&self, session_id: &str) -> bool {
        if self
            .probes
            .lock()
            .unwrap()
            .values()
            .any(|probe| probe == session_id)
        {
            return true;
        }
        self.known_probes().iter().any(|probe| probe == session_id)
    }

    /// 持久注册表（惰性加载；文件损坏/缺席按空表处理——残余边界如实申报：
    /// 注册表丢失时历史探针回退标题形态识别）。
    fn known_probes(&self) -> Vec<String> {
        let mut known = self.probes_known.lock().unwrap();
        if known.is_none() {
            let loaded = std::fs::read(self.registry_path())
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Vec<String>>(&bytes).ok())
                .unwrap_or_default();
            *known = Some(loaded);
        }
        known.clone().unwrap_or_default()
    }

    fn registry_path(&self) -> PathBuf {
        crate::monitor::opencode_observation::registration_dir_for(&self.home)
            .join(PROBE_REGISTRY_FILE)
    }

    /// 注册新探针（内存 + 持久写穿，有界保留最新 [`PROBE_REGISTRY_CAP`] 个；
    /// 持久化失败只损失跨重启识别，不影响本轮判定）。
    fn register_probe(&self, runtime: &RuntimeKey, id: &str) {
        self.probes
            .lock()
            .unwrap()
            .insert(runtime.clone(), id.to_string());
        let mut known = self.probes_known.lock().unwrap();
        let mut list = match known.take() {
            Some(list) => list,
            None => std::fs::read(self.registry_path())
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Vec<String>>(&bytes).ok())
                .unwrap_or_default(),
        };
        if !list.iter().any(|probe| probe == id) {
            list.push(id.to_string());
        }
        let len = list.len();
        if len > PROBE_REGISTRY_CAP {
            list.drain(..len - PROBE_REGISTRY_CAP);
        }
        let dir = crate::monitor::opencode_observation::registration_dir_for(&self.home);
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(body) = serde_json::to_vec(&list) {
            use std::io::Write as _;
            let path = self.registry_path();
            // 顺手项（fix3）：临时文件（0600）+ rename 原子发布，读方只见完整旧版
            // 或完整新版，绝无截断中间态。
            let temp = path.with_extension("json.tmp");
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            if let Ok(mut file) = options.open(&temp) {
                if file.write_all(&body).is_ok() {
                    let _ = std::fs::rename(&temp, &path);
                } else {
                    let _ = std::fs::remove_file(&temp);
                }
            }
        }
        *known = Some(list);
    }

    /// 一轮完整归属核验（查询式入口）。`Ok((pid, started_at_ms))` = 放行（有成对
    /// 证据），返回本轮锁定的 TUI 身份供调用方做 ACK 后终验；`Err` = fail-closed
    /// 结构化拒绝。
    ///
    /// M2 起生产写权链不再经此入口——`verify_write_authority_using` 把锁定窗口
    /// 解析**一次**（与语义准入共用，消除双选窗）并走 [`Self::verify_with_locked`]；
    /// 本入口保留给门单元测试（解析语义与共用尾段零改动）。
    ///
    /// 次序（fail-closed：任何一步不过即拒绝，零后续动作）：
    /// 锁定窗口（零 IO）→ 换代核对 → 探针会话（缺则建一次）→ 写前槽位判读（分诊 +
    /// 退避）→ 控制文件交付（轮次授权）→ challenge 覆盖写 → 单调截止内轮询 ack →
    /// **撤销控制文件**（无论成败）。
    #[cfg(test)]
    pub(crate) async fn verify(
        &self,
        runtime: &RuntimeKey,
        transport: &GateTransport,
        target_session_id: &str,
        target_directory: &Path,
        locked_window: &LockedWindowLookup,
        tui_fact: &TuiProcessFact,
    ) -> Result<(u32, i64), NativeError> {
        // 进程内单飞：同一时刻全进程至多一轮 challenge（brief「并发退避」口径；
        // 后到者在此**有界**等待（见 [`Self::acquire_flight`]），不会互相覆盖探针槽位；
        // 预算耗尽按 OC_GATE_BUSY 受控退避——零写、意图可重试，绝不无限排队。
        let _flight = self.acquire_flight().await?;
        // 1) 锁定窗口（零 IO，先于一切探针写）：无新鲜确认窗口拥有该会话 = 归属不可证。
        let Some(locked) = locked_window(target_session_id) else {
            return Err(gate_error(
                codes::OC_GATE_PID_UNVERIFIED,
                "no freshly observed tui window owns this session",
            ));
        };
        self.run_verified_round(runtime, transport, target_directory, &locked, tui_fact)
            .await
    }

    /// M2 同轮单一窗口身份入口：调用方（`verify_write_authority_using`）已把锁定
    /// 窗口解析**一次**并与语义准入共用，本入口直接消费该窗口。门内判据（单飞、
    /// 换代核对、探针、ack pid 比对）与本入口之前逐字一致，零放宽；`None`（生产
    /// 不可达——准入先于门且对缺身份 fail-closed）保持与 `verify` 相同的拒绝语义。
    pub(crate) async fn verify_with_locked(
        &self,
        runtime: &RuntimeKey,
        transport: &GateTransport,
        target_directory: &Path,
        locked_window: Option<&LockedWindow>,
        tui_fact: &TuiProcessFact,
        recheck_locked: impl FnOnce(&LockedWindow) -> Result<(), NativeError> + Send,
    ) -> Result<(u32, i64), NativeError> {
        // 生产写权链的单飞获取同样有界（GFix）：见 [`Self::acquire_flight`]。
        let _flight = self.acquire_flight().await?;
        let Some(locked) = locked_window else {
            return Err(gate_error(
                codes::OC_GATE_PID_UNVERIFIED,
                "no freshly observed tui window owns this session",
            ));
        };
        recheck_locked(locked)?;
        self.run_verified_round(runtime, transport, target_directory, locked, tui_fact)
            .await
    }

    /// 单飞获取预算（GFix）：2 × 轮窗口 + 余量。合法并发（第二台设备/第二意图）
    /// 通常最多等一个完整窗口（10s）；预算给两窗 + 余量，绝不无限排队。
    fn flight_acquire_budget(&self) -> Duration {
        Duration::from_millis(self.window_ms.max(0) as u64 * 2) + Duration::from_millis(250)
    }

    /// 进程内单飞的有界获取（GFix）：同一时刻全进程至多一轮 challenge；在飞轮次
    /// 被外部条件拖住（挂起 / IO 风暴下计时器 stretched）时，后到写按预算拿
    /// [`codes::OC_GATE_BUSY`] 受控退避——零探针创建、零 challenge 写、经入口的
    /// `resolve_entry_failure` 落确定性墓碑、意图可重试；绝不把整条写路径无限期
    /// 串在一个被拖穿的轮次后面（GE 事故指纹：并发 stop/send 全 hang、零动作行、
    /// 零墓碑、reads 正常）。
    async fn acquire_flight(&self) -> Result<tokio::sync::MutexGuard<'_, ()>, NativeError> {
        // H5 分段计时：排队段观测——有界获取的等待时长累进读数（差分消费）。
        let wait_started = std::time::Instant::now();
        match tokio::time::timeout(self.flight_acquire_budget(), self.flight.lock()).await {
            Ok(guard) => {
                self.flight_wait_total_ms.fetch_add(
                    wait_started.elapsed().as_millis() as i64,
                    std::sync::atomic::Ordering::Relaxed,
                );
                Ok(guard)
            }
            Err(_) => {
                self.flight_wait_total_ms.fetch_add(
                    wait_started.elapsed().as_millis() as i64,
                    std::sync::atomic::Ordering::Relaxed,
                );
                Err(gate_error(
                    codes::OC_GATE_BUSY,
                    "another challenge is already in flight",
                ))
            }
        }
    }

    /// `verify` / `verify_with_locked` 的共用尾段：换代事前核对 → 单调截止 →
    /// 控制文件守卫 → 一轮探针回声。
    async fn run_verified_round(
        &self,
        runtime: &RuntimeKey,
        transport: &GateTransport,
        target_directory: &Path,
        locked: &LockedWindow,
        tui_fact: &TuiProcessFact,
    ) -> Result<(u32, i64), NativeError> {
        // 2) 换代事前核对（H02 E5：pid + 启动时间是换代唯一事前核对）。
        if tui_fact(locked.pid) != Some(locked.started_at_ms) {
            return Err(gate_error(
                codes::OC_GATE_PID_UNVERIFIED,
                "locked tui process generation changed",
            ));
        }
        // I5：整轮用单调时钟定硬截止——所有等待与 IO 受剩余预算约束，任何一段
        // 挂起都不会把窗口拖穿。
        let deadline = tokio::time::Instant::now() + Duration::from_millis(self.window_ms as u64);
        // 统一出口（N3）：RAII 守卫持本轮锁定身份，**无论正常返回还是 future 被
        // abort/drop** 都撤销控制文件授权（换锁定窗口后旧窗口不再应答）；失败早于
        // 控制文件写出时，remove 是幂等空操作。
        let _revoke_guard = ProbeControlGuard {
            home: self.home.clone(),
            pid: locked.pid,
        };
        self.run_round(runtime, transport, target_directory, locked, deadline)
            .await
    }

    async fn run_round(
        &self,
        runtime: &RuntimeKey,
        transport: &GateTransport,
        target_directory: &Path,
        locked: &LockedWindow,
        deadline: tokio::time::Instant,
    ) -> Result<(u32, i64), NativeError> {
        let remaining = || deadline.saturating_duration_since(tokio::time::Instant::now());
        // 3) 探针会话：每代际一个（创建一次、复用）。
        let (mut probe, mut created_now) = self
            .ensure_probe(runtime, transport, target_directory, deadline)
            .await?;
        // I2：创建 IO 完成后复查剩余预算——耗尽即拒绝，不带着零预算继续本轮。
        if remaining().is_zero() {
            return Err(gate_error(
                Refusal::NoAck.code(),
                "binding gate window exhausted before the challenge",
            ));
        }
        // 4) 写前槽位判读（I3 分诊）：
        //    * NotFound（明确删除）→ 受控重建一次，重读；
        //    * Status/Unreachable → 立即拒绝（不重建）；
        //    * InvalidBody（缺标题/不可解析）→ 立即拒绝（写纪律/形态漂移）；
        //    * 成功 → 新建探针只接受已知初始标题；存量槽位按协议形态分诊（退避/可覆盖）。
        let mut preread = self.read_bounded(transport, &probe, deadline).await;
        if matches!(&preread, Err(GateIoError::NotFound)) {
            // 探针可能已被删除（兔维斯删会话/换代的重建规则）：受控重建一次。
            self.probes.lock().unwrap().remove(runtime);
            let (recreated, created) = self
                .ensure_probe(runtime, transport, target_directory, deadline)
                .await?;
            probe = recreated;
            created_now = created;
            // I2：重建 IO 完成后同样复查剩余预算。
            if remaining().is_zero() {
                return Err(gate_error(
                    Refusal::NoAck.code(),
                    "binding gate window exhausted before the challenge",
                ));
            }
            preread = self.read_bounded(transport, &probe, deadline).await;
        }
        let title = match preread {
            Ok(title) => title,
            Err(GateIoError::NotFound) => {
                return Err(gate_error(
                    codes::OC_GATE_PROBE_UNWRITABLE,
                    "recreated probe session is still missing",
                ));
            }
            Err(GateIoError::Status) => return Err(unreachable_error()),
            Err(GateIoError::Unreachable) => return Err(unreachable_error()),
            Err(GateIoError::InvalidBody) => {
                return Err(gate_error(
                    Refusal::TitleInvalid.code(),
                    "probe session response has no usable title",
                ));
            }
        };
        if created_now && title != PROBE_SESSION_TITLE {
            // 新建探针只接受已知初始标题：其余内容 = 服务行为异常，结构化拒绝。
            return Err(gate_error(
                Refusal::TitleInvalid.code(),
                "freshly created probe session carries an unexpected title",
            ));
        }
        if !created_now && title != PROBE_SESSION_TITLE {
            // 存量槽位：ack（上一轮产物）→ 可覆盖；未过期他者 challenge → 退避；
            // 过期 challenge → 可覆盖；其余非协议形态（噪音/篡改）→ 结构化拒绝。
            match parse_probe_title(&title) {
                ProbeTitle::Challenge { writetime_ms, .. }
                    if writetime_ms.saturating_add(self.window_ms) > (self.now_ms)() =>
                {
                    return Err(gate_error(
                        codes::OC_GATE_BUSY,
                        "another challenge is already in flight",
                    ));
                }
                ProbeTitle::Challenge { .. } | ProbeTitle::Ack { .. } => {}
                ProbeTitle::Foreign => {
                    return Err(gate_error(
                        Refusal::TitleInvalid.code(),
                        "probe slot holds a non-protocol title",
                    ));
                }
            }
        }
        // 5) challenge 准备（先定 nonce/writetime，控制文件与 challenge 绑定同一轮次）。
        let nonce = new_nonce();
        let writetime_ms = (self.now_ms)();
        let deadline_wall = writetime_ms.saturating_add(self.window_ms);
        // 6) 探针会话 ID 交付控制文件（I5：绑定轮次 nonce + 进程代际 expectPid/expectBoot
        //    + 有效期 deadlineMs；每轮重写，结束后撤销）。写失败 = 通道不可建立。
        if write_probe_control(
            &self.home,
            locked.pid,
            &probe,
            &nonce,
            deadline_wall,
            locked.boot.as_deref(),
        )
        .is_err()
        {
            return Err(gate_error(
                codes::OC_GATE_PROBE_UNWRITABLE,
                "probe control file could not be written",
            ));
        }
        // 7) challenge 覆盖写（每次换新 nonce；写纪律：单槽、限频、一次尝试）。
        let challenge = challenge_title(&nonce, writetime_ms);
        match tokio::time::timeout(remaining(), (transport.write_title)(&probe, &challenge)).await {
            Ok(Ok(())) => {}
            Ok(Err(GateIoError::Unreachable)) => return Err(unreachable_error()),
            // PATCH 打到已删除的探针上：本轮已死，受控拒绝（不重建不重试）。
            Ok(Err(_)) => return Err(probe_unwritable_error()),
            Err(_elapsed) => return Err(unreachable_error()),
        }
        // 8) 单调截止内轮询 ack：三件套（nonce / pid / 窗口）+ 硬截止全由
        //    evaluate_observation 判定（now 在 IO 完成后重取）；任何读失败/超时都
        //    归入拒绝（fail-closed，不降级）。
        loop {
            if remaining().is_zero() {
                return Err(gate_error(Refusal::NoAck.code(), Refusal::NoAck.message()));
            }
            let nap = Duration::from_millis(self.poll_interval_ms).min(remaining());
            tokio::time::sleep(nap).await;
            if remaining().is_zero() {
                return Err(gate_error(Refusal::NoAck.code(), Refusal::NoAck.message()));
            }
            let read = (transport.read_title)(&probe);
            let remaining_now = deadline.saturating_duration_since(tokio::time::Instant::now());
            let title = match tokio::time::timeout(remaining_now, read).await {
                // 截止耗尽（读被预算切断）= 窗口关闭 → NoAck（区别于服务不可达）。
                Err(_elapsed) => {
                    return Err(gate_error(Refusal::NoAck.code(), Refusal::NoAck.message()))
                }
                Ok(Ok(title)) => title,
                Ok(Err(GateIoError::Unreachable | GateIoError::Status)) => {
                    return Err(unreachable_error())
                }
                Ok(Err(GateIoError::NotFound | GateIoError::InvalidBody)) => {
                    return Err(probe_unwritable_error());
                }
            };
            let now_ms = (self.now_ms)();
            match evaluate_observation(
                &nonce,
                writetime_ms,
                locked.pid,
                Some(&title),
                now_ms,
                self.window_ms,
            ) {
                RoundOutcome::Pass => return Ok((locked.pid, locked.started_at_ms)),
                RoundOutcome::Wait => {
                    if now_ms > writetime_ms.saturating_add(self.window_ms) {
                        return Err(gate_error(Refusal::NoAck.code(), Refusal::NoAck.message()));
                    }
                }
                RoundOutcome::Reject(refusal) => {
                    return Err(gate_error(refusal.code(), refusal.message()));
                }
            }
        }
    }

    /// 受预算约束的探针读：响应等待时间计入整轮剩余预算（I2），超时按不可达拒绝。
    async fn read_bounded(
        &self,
        transport: &GateTransport,
        probe: &str,
        deadline: tokio::time::Instant,
    ) -> Result<String, GateIoError> {
        let read = (transport.read_title)(probe);
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, read).await {
            Ok(result) => result,
            Err(_elapsed) => Err(GateIoError::Unreachable),
        }
    }

    /// 取（或建）本代际的探针会话 id 与「是否本次新建」。创建一次、复用；响应里
    /// 没有可用 id / id 形态非法 → 结构化拒绝；传输失败 → 不可达拒绝。
    /// 取（或建）本代际探针会话。创建超时受**整轮同一单调 deadline** 的剩余预算
    /// 约束（I2：不再各发一整个窗口），创建完成后由调用方复查剩余预算。
    async fn ensure_probe(
        &self,
        runtime: &RuntimeKey,
        transport: &GateTransport,
        target_directory: &Path,
        deadline: tokio::time::Instant,
    ) -> Result<(String, bool), NativeError> {
        if let Some(id) = self.probes.lock().unwrap().get(runtime) {
            return Ok((id.clone(), false));
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let created =
            tokio::time::timeout(remaining, (transport.create_probe)(target_directory)).await;
        let id = match created {
            Ok(Ok(id)) => id,
            Ok(Err(_)) => return Err(probe_unwritable_error()),
            Err(_elapsed) => return Err(unreachable_error()),
        };
        // id 形态必须可安全进路径段（与原生 id 同一判据）；不合法 = 服务行为异常。
        if super::projection::segment(&id).is_err() {
            return Err(probe_unwritable_error());
        }
        self.register_probe(runtime, &id);
        Ok((id, true))
    }
}

/// I5/N3：控制文件授权撤销守卫——持有本轮锁定身份，Drop（含 future abort/drop
/// 路径）即撤销授权，取消不会遗留可被误读的旧授权。
struct ProbeControlGuard {
    home: PathBuf,
    pid: u32,
}

impl Drop for ProbeControlGuard {
    fn drop(&mut self) {
        remove_probe_control(&self.home, self.pid);
    }
}

fn gate_error(code: &str, message: &str) -> NativeError {
    NativeError::new(code, message)
}

fn unreachable_error() -> NativeError {
    gate_error(
        codes::OC_GATE_UNREACHABLE,
        "opencode native service is unreachable",
    )
}

fn probe_unwritable_error() -> NativeError {
    gate_error(
        codes::OC_GATE_PROBE_UNWRITABLE,
        "opencode probe session is not writable",
    )
}

impl GateTransport {
    /// 生产传输：全部经 [`NativeClient`]（写纪律与动作写同款：一次尝试、不重试、
    /// 不跟随重定向、401 作废连接）。
    pub(crate) fn for_client(client: Arc<super::client::NativeClient>, directory: PathBuf) -> Self {
        let read_client = Arc::clone(&client);
        let create_client = Arc::clone(&client);
        let write_client = Arc::clone(&client);
        Self {
            read_title: Box::new(move |probe| {
                let client = Arc::clone(&read_client);
                let path = format!("/api/session/{probe}");
                Box::pin(async move {
                    match client.read_json_statused(&path).await {
                        // GET 包一层 `data`（任务 0 实证形态）。标题缺失 = 形态漂移。
                        Ok((_, Some(value))) => {
                            let root = value.get("data").unwrap_or(&value);
                            match root.get("title").and_then(serde_json::Value::as_str) {
                                Some(title) => Ok(title.to_owned()),
                                None => Err(GateIoError::InvalidBody),
                            }
                        }
                        Ok((404, None)) => Err(GateIoError::NotFound),
                        Ok((_, None)) => Err(GateIoError::Status),
                        Err(err) => Err(map_native_io(&err)),
                    }
                })
            }),
            create_probe: Box::new(move |_| {
                let client = Arc::clone(&create_client);
                let directory = directory.clone();
                Box::pin(async move {
                    let body = serde_json::json!({
                        "title": PROBE_SESSION_TITLE,
                        "location": {
                            "directory": directory.to_string_lossy(),
                        },
                    });
                    match client.write_once("/api/session", &body).await {
                        Ok(reply) if (200..300).contains(&reply.status) => {
                            // POST 包一层 `data`（H02 复现命令口径）；缺 id = 形态漂移。
                            let root = reply.body.as_ref().and_then(|b| b.get("data"));
                            let id = root
                                .and_then(|r| r.get("id"))
                                .or_else(|| reply.body.as_ref().and_then(|b| b.get("id")))
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned)
                                .ok_or(GateIoError::InvalidBody)?;
                            Ok(id)
                        }
                        Ok(_) => Err(GateIoError::Status),
                        Err(err) => Err(map_native_io(&err)),
                    }
                })
            }),
            write_title: Box::new(move |probe, title| {
                let client = Arc::clone(&write_client);
                let path = format!("/api/session/{probe}");
                let body = serde_json::json!({ "title": title });
                Box::pin(async move {
                    match client.patch_once(&path, &body).await {
                        // PATCH 改标题返回 204 无 body（b1/H02 实证）；2xx 即受理。
                        Ok(reply) if (200..300).contains(&reply.status) => Ok(()),
                        Ok(_) => Err(GateIoError::Status),
                        Err(err) => Err(map_native_io(&err)),
                    }
                })
            }),
        }
    }
}

/// NativeError → 受控 IO 错误：传输类/凭据类归 Unreachable；响应体不可解析归
/// InvalidBody；其余（非 2xx 状态等）归 Status。
fn map_native_io(err: &NativeError) -> GateIoError {
    match err.code.as_str() {
        codes::OC_TRANSPORT_TIMEOUT
        | codes::OC_TRANSPORT_FAILED
        | codes::OC_UNAUTHORIZED
        | codes::OC_RESPONSE_TOO_LARGE
        | codes::OC_ENDPOINT_INVALID
        | codes::OC_PATH_INVALID => GateIoError::Unreachable,
        codes::OC_RESPONSE_INVALID => GateIoError::InvalidBody,
        _ => GateIoError::Status,
    }
}

/// 探针会话 ID 交付控制文件（兔维斯 → 插件，克制的登记通道扩展）：写
/// `<home>/.tuvis/opencode-tui/<pid>.probe`，内容
/// `{"sessionID","expectPid","expectBoot","nonce","deadlineMs"}`，0600（目录 0700）。
///
/// 授权生命周期（I5）：绑定**轮次**（nonce——插件只答当前授权轮）、**进程代际**
/// （expectPid + expectBoot——pid 复用/换窗口不继承授权）与**有效期**
/// （deadlineMs——过期不应答）；每轮结束撤销（[`remove_probe_control`]），换锁定
/// 窗口后旧窗口不再持有效授权，不会互踩 ack 槽位。插件写 ack 前复核授权与槽位。
///
/// 文件名不带 `.json` 后缀：观察采集循环只枚举 `*.json`，本文件对采集零干扰；
/// 登记残迹清理（同 pid 的 `<pid>.json` 被清理）时同步清理本文件。
pub(crate) fn write_probe_control(
    home: &Path,
    pid: u32,
    probe_session: &str,
    nonce: &str,
    deadline_ms: i64,
    boot: Option<&str>,
) -> Result<(), ()> {
    use std::io::Write as _;
    let dir = crate::monitor::opencode_observation::registration_dir_for(home);
    std::fs::create_dir_all(&dir).map_err(|_| ())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let path = dir.join(format!("{pid}{PROBE_CONTROL_SUFFIX}"));
    let body = serde_json::json!({
        "sessionID": probe_session,
        "expectPid": pid,
        "expectBoot": boot,
        "nonce": nonce,
        "deadlineMs": deadline_ms,
    });
    // 直接以 0600 创建/截断写（不经 0644 中间态）。
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|_| ())?;
    file.write_all(body.to_string().as_bytes())
        .map_err(|_| ())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// 撤销一轮控制文件授权（尽力而为的幂等清理；每轮结束无论成败都调用）。
pub(crate) fn remove_probe_control(home: &Path, pid: u32) {
    let dir = crate::monitor::opencode_observation::registration_dir_for(home);
    let _ = std::fs::remove_file(dir.join(format!("{pid}{PROBE_CONTROL_SUFFIX}")));
}

/// 登记残迹清理（采集循环 prune 时调用）：同 pid 的探针控制文件一并清理，
/// 不留可被下一窗口误读的旧授权。
pub(crate) fn remove_probe_control_residue(home: &Path, pid: u32) {
    remove_probe_control(home, pid);
}

#[cfg(test)]
mod gate_pure_tests {
    use super::*;

    const NONCE: &str = "0123456789abcdef0123456789abcdef";
    const OTHER_NONCE: &str = "ffffffffffffffffffffffffffffffff";
    const WRITE_AT: i64 = 1_700_000_000_000;
    const WINDOW: i64 = 10_000;
    const LOCKED_PID: u32 = 4242;

    fn ack_title(nonce: &str, pid: u32, ack_at: i64) -> String {
        format!("ack:{nonce}:{pid}:{ack_at}")
    }

    // ---- 解析 ----

    #[test]
    fn challenge_title_round_trips_through_parser() {
        let parsed = parse_probe_title(&challenge_title(NONCE, WRITE_AT));
        assert_eq!(
            parsed,
            ProbeTitle::Challenge {
                nonce: NONCE.to_string(),
                writetime_ms: WRITE_AT
            }
        );
    }

    #[test]
    fn ack_title_parses_nonce_pid_and_time() {
        let parsed = parse_probe_title(&ack_title(NONCE, LOCKED_PID, WRITE_AT + 640));
        assert_eq!(
            parsed,
            ProbeTitle::Ack {
                nonce: NONCE.to_string(),
                pid: LOCKED_PID,
                acktime_ms: WRITE_AT + 640
            }
        );
    }

    #[test]
    fn malformed_titles_are_foreign() {
        let cases = [
            "",
            "probe:tuvis",
            "chal",
            "chal:",
            "chal:abc:123",
            "chal:0123456789ABCDEF0123456789ABCDEF:123",
            "chal:short:123",
            &format!("chal:{NONCE}:12a3"),
            &format!("chal:{NONCE}"),
            &format!("chal:{NONCE}:123:extra"),
            &format!("chal:{NONCE}:123 "),
            &format!(" chal:{NONCE}:123"),
            &format!("ack:{NONCE}:123"),
            &format!("ack:{NONCE}:0:123"),
            &format!("ack:{NONCE}:abc:123"),
            &format!("ack:{NONCE}:4242"),
            &format!("ack:{NONCE}:4242:12x"),
            &format!("ackx:{NONCE}:4242:123"),
            "ack::4242:123",
            "CHAL:0123456789abcdef0123456789abcdef:123",
        ];
        for case in cases {
            assert_eq!(
                parse_probe_title(case),
                ProbeTitle::Foreign,
                "形态 {case:?} 必须按非协议标题处理"
            );
        }
    }

    #[test]
    fn nonce_is_strictly_shaped_and_unique() {
        let first = new_nonce();
        assert_eq!(first.len(), NONCE_BYTES * 2);
        assert!(first
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        assert_ne!(first, new_nonce(), "每次 challenge 必须换新 nonce");
    }

    // ---- 三件套 + 硬截止评估 ----

    #[test]
    fn matching_ack_within_content_window_passes_before_deadline() {
        // 内容边界 Δ ∈ [0, W] 且观测时刻 ≤ 截止（含等号）。
        for delta in [0i64, 1, 56, 640, WINDOW - 1, WINDOW] {
            let observed = ack_title(NONCE, LOCKED_PID, WRITE_AT + delta);
            let now = WRITE_AT + delta; // 未过截止（== 截止含等号）
            assert_eq!(
                evaluate_observation(NONCE, WRITE_AT, LOCKED_PID, Some(&observed), now, WINDOW),
                RoundOutcome::Pass,
                "Δ={delta} 且未过截止必须放行"
            );
        }
    }

    #[test]
    fn future_ack_is_rejected() {
        // I2：acktime 在未来（> now）= 时钟异常，绝不放行。
        let observed = ack_title(NONCE, LOCKED_PID, WRITE_AT + 500);
        assert_eq!(
            evaluate_observation(
                NONCE,
                WRITE_AT,
                LOCKED_PID,
                Some(&observed),
                WRITE_AT + 400, // now < acktime
                WINDOW
            ),
            RoundOutcome::Reject(Refusal::LateAck)
        );
    }

    #[test]
    fn ack_observed_after_deadline_is_rejected() {
        // I2：Δ=W 的内容边界可保留，但截止后取得的观测不构成放行证据。
        let observed = ack_title(NONCE, LOCKED_PID, WRITE_AT + WINDOW);
        assert_eq!(
            evaluate_observation(
                NONCE,
                WRITE_AT,
                LOCKED_PID,
                Some(&observed),
                WRITE_AT + WINDOW + 1,
                WINDOW
            ),
            RoundOutcome::Reject(Refusal::LateAck)
        );
    }

    #[test]
    fn late_content_ack_is_rejected() {
        let observed = ack_title(NONCE, LOCKED_PID, WRITE_AT + WINDOW + 1);
        assert_eq!(
            evaluate_observation(
                NONCE,
                WRITE_AT,
                LOCKED_PID,
                Some(&observed),
                WRITE_AT + WINDOW + 1,
                WINDOW
            ),
            RoundOutcome::Reject(Refusal::LateAck)
        );
    }

    #[test]
    fn ack_before_challenge_time_is_rejected() {
        // acktime < writetime：时钟异常，绝不放行。
        let observed = ack_title(NONCE, LOCKED_PID, WRITE_AT - 1);
        assert_eq!(
            evaluate_observation(
                NONCE,
                WRITE_AT,
                LOCKED_PID,
                Some(&observed),
                WRITE_AT,
                WINDOW
            ),
            RoundOutcome::Reject(Refusal::LateAck)
        );
    }

    #[test]
    fn pid_mismatch_is_rejected_immediately() {
        // E4：写错实例会得到错误 TUI 的合法 ack——pid 是唯一 TUI 身份防线。
        let observed = ack_title(NONCE, LOCKED_PID + 1, WRITE_AT + 100);
        assert_eq!(
            evaluate_observation(
                NONCE,
                WRITE_AT,
                LOCKED_PID,
                Some(&observed),
                WRITE_AT + 101,
                WINDOW
            ),
            RoundOutcome::Reject(Refusal::PidMismatch)
        );
    }

    #[test]
    fn nonce_mismatch_ack_is_rejected() {
        let observed = ack_title(OTHER_NONCE, LOCKED_PID, WRITE_AT + 100);
        assert_eq!(
            evaluate_observation(
                NONCE,
                WRITE_AT,
                LOCKED_PID,
                Some(&observed),
                WRITE_AT + 101,
                WINDOW
            ),
            RoundOutcome::Reject(Refusal::NonceMismatch)
        );
    }

    #[test]
    fn foreign_challenge_in_slot_is_rejected_mid_round() {
        let observed = challenge_title(OTHER_NONCE, WRITE_AT + 50);
        assert_eq!(
            evaluate_observation(
                NONCE,
                WRITE_AT,
                LOCKED_PID,
                Some(&observed),
                WRITE_AT + 60,
                WINDOW
            ),
            RoundOutcome::Reject(Refusal::NonceMismatch),
            "槽位被他者 challenge 抢占 = 本轮作废"
        );
    }

    #[test]
    fn non_protocol_title_mid_round_is_rejected() {
        for observed in ["noise:e3x", ""] {
            assert_eq!(
                evaluate_observation(
                    NONCE,
                    WRITE_AT,
                    LOCKED_PID,
                    Some(observed),
                    WRITE_AT + 60,
                    WINDOW
                ),
                RoundOutcome::Reject(Refusal::TitleInvalid),
                "轮内非协议标题 {observed:?} 必须拒绝（写纪律）"
            );
        }
        // 读不到标题字段同样 fail-closed。
        assert_eq!(
            evaluate_observation(NONCE, WRITE_AT, LOCKED_PID, None, WRITE_AT + 60, WINDOW),
            RoundOutcome::Reject(Refusal::TitleInvalid)
        );
    }

    #[test]
    fn own_unacked_challenge_keeps_waiting_before_deadline() {
        let observed = challenge_title(NONCE, WRITE_AT);
        assert_eq!(
            evaluate_observation(
                NONCE,
                WRITE_AT,
                LOCKED_PID,
                Some(&observed),
                WRITE_AT + 1_200,
                WINDOW
            ),
            RoundOutcome::Wait,
            "实测典型 ack ≤1.2s：窗口内未见 ack 继续等"
        );
    }

    #[test]
    fn deadline_past_own_challenge_rejects_no_ack() {
        let observed = challenge_title(NONCE, WRITE_AT);
        assert_eq!(
            evaluate_observation(
                NONCE,
                WRITE_AT,
                LOCKED_PID,
                Some(&observed),
                WRITE_AT + WINDOW + 1,
                WINDOW
            ),
            RoundOutcome::Reject(Refusal::NoAck),
            "超截止仍是自家 challenge = 窗口内零 ack（fail-closed）"
        );
    }

    #[test]
    fn repeated_acks_still_pass_on_first_valid_observation() {
        // 重复 ack 幂等：槽位里是自己 nonce 的合法 ack（插件可能覆盖写多次）——
        // 守卫按内容评估，通过与否与 ack 出现次数无关。
        for _ in 0..3 {
            let observed = ack_title(NONCE, LOCKED_PID, WRITE_AT + 400);
            assert_eq!(
                evaluate_observation(
                    NONCE,
                    WRITE_AT,
                    LOCKED_PID,
                    Some(&observed),
                    WRITE_AT + 400,
                    WINDOW
                ),
                RoundOutcome::Pass
            );
        }
    }

    #[test]
    fn reason_static_maps_only_gate_codes() {
        assert_eq!(
            reason_static(codes::OC_GATE_BUSY),
            Some(codes::OC_GATE_BUSY)
        );
        assert_eq!(reason_static("OC_BINDING_STALE"), None);
    }
}

#[cfg(test)]
mod gate_runtime_tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;
    use std::time::Duration as StdDuration;

    /// 真实推进时钟（毫秒）：超窗判定依赖时钟前进，测试一律用真实钟。
    fn now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    const LOCKED_PID: u32 = 4242;
    const STARTED: i64 = 1_000;

    struct FakeProbe {
        /// 探针会话标题（按会话 id 建模：每次 create 产出独立 id，与生产「每代际
        /// 一个探针会话」一致）。
        titles: std::sync::Mutex<HashMap<String, String>>,
        /// 已删除的探针会话 id（读/写 404 → 受控重建路径）。
        deleted: std::sync::Mutex<std::collections::HashSet<String>>,
        /// create 调用计数（复用/重建判据），也用于派生独立会话 id。
        creates: AtomicUsize,
        /// PATCH 计数。
        writes: AtomicUsize,
        /// 在飞串行取证：进入传输段的并发数峰值（真挂起 15ms 保证可测）。
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        /// create 是否成功（false = 探针不可写用例）。
        create_ok: bool,
        /// 读是否传输失败（服务不可达用例）。
        read_unreachable: bool,
        /// 读是否返回非 2xx（≠404）状态（I3 Status 分诊用例）。
        read_status_500: bool,
        /// 读是否 2xx 但缺标题字段（I3 InvalidBody 分诊用例）。
        read_missing_title: bool,
        /// 读挂起时长（毫秒；I2 预算约束取证）。
        read_delay_ms: u64,
        /// create 挂起时长（毫秒；I2 创建预算取证）。
        create_delay_ms: u64,
    }

    impl FakeProbe {
        fn new() -> Self {
            Self {
                titles: std::sync::Mutex::new(HashMap::new()),
                deleted: std::sync::Mutex::new(std::collections::HashSet::new()),
                creates: AtomicUsize::new(0),
                writes: AtomicUsize::new(0),
                in_flight: AtomicUsize::new(0),
                max_in_flight: AtomicUsize::new(0),
                create_ok: true,
                read_unreachable: false,
                read_status_500: false,
                read_missing_title: false,
                read_delay_ms: 0,
                create_delay_ms: 0,
            }
        }

        fn transport(self: &StdArc<Self>) -> GateTransport {
            let read_probe = StdArc::clone(self);
            let create_probe = StdArc::clone(self);
            let write_probe = StdArc::clone(self);
            GateTransport {
                read_title: Box::new(move |probe| {
                    let probe = probe.to_string();
                    let state = StdArc::clone(&read_probe);
                    Box::pin(async move {
                        state.enter_flight().await;
                        let verdict = state.read_once(&probe).await;
                        state.leave_flight();
                        verdict
                    })
                }),
                create_probe: Box::new(move |_| {
                    let state = StdArc::clone(&create_probe);
                    Box::pin(async move {
                        let index = state.creates.fetch_add(1, Ordering::SeqCst);
                        if state.create_delay_ms > 0 {
                            tokio::time::sleep(StdDuration::from_millis(state.create_delay_ms))
                                .await;
                        }
                        if !state.create_ok {
                            return Err(GateIoError::Status);
                        }
                        // 首个探针沿用惯用 id（既有用例引用），后续独立派生。
                        let id = if index == 0 {
                            "ses_probe".to_string()
                        } else {
                            format!("ses_probe_{}", index + 1)
                        };
                        state
                            .titles
                            .lock()
                            .unwrap()
                            .insert(id.clone(), "probe:tuvis".to_string());
                        Ok(id)
                    })
                }),
                write_title: Box::new(move |probe, title| {
                    let probe = probe.to_string();
                    let title = title.to_string();
                    let state = StdArc::clone(&write_probe);
                    Box::pin(async move {
                        state.enter_flight().await;
                        let verdict = state.write_once(&probe, &title).await;
                        state.leave_flight();
                        verdict
                    })
                }),
            }
        }

        /// 真实挂起的在飞窗口：并发峰值只在传输段真正重叠时才会 >1（M1）。
        async fn enter_flight(&self) {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(StdDuration::from_millis(15)).await;
        }

        fn leave_flight(&self) {
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
        }

        async fn read_once(&self, probe: &str) -> Result<String, GateIoError> {
            if self.read_delay_ms > 0 {
                tokio::time::sleep(StdDuration::from_millis(self.read_delay_ms)).await;
            }
            if self.read_unreachable {
                return Err(GateIoError::Unreachable);
            }
            if self.read_status_500 {
                return Err(GateIoError::Status);
            }
            if self.deleted.lock().unwrap().contains(probe) {
                return Err(GateIoError::NotFound); // 明确删除（404）
            }
            match self.titles.lock().unwrap().get(probe).cloned() {
                Some(title) if !self.read_missing_title => Ok(title),
                _ => Err(GateIoError::InvalidBody), // 2xx 缺标题
            }
        }

        async fn write_once(&self, probe: &str, title: &str) -> Result<(), GateIoError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            if self.deleted.lock().unwrap().contains(probe) {
                return Err(GateIoError::NotFound);
            }
            // 单次取锁：match 暂临时卫会存活到 match 结束，二次 lock 会自锁死。
            let mut titles = self.titles.lock().unwrap();
            match titles.get(probe) {
                Some(_) => {
                    titles.insert(probe.to_string(), title.to_string());
                    Ok(())
                }
                None => Err(GateIoError::NotFound),
            }
        }

        fn set_title(&self, id: &str, title: &str) {
            self.titles
                .lock()
                .unwrap()
                .insert(id.to_string(), title.to_string());
        }

        fn first_probe_id(&self) -> Option<String> {
            self.titles.lock().unwrap().keys().next().cloned()
        }

        fn ack_when_challenge_read(&self, pid: u32) {
            // 模拟 TUI：读到 challenge 后槽位翻转为 ack（守卫下一次读可见）；
            // acktime = 真实当前时刻（与守卫的 challenge writetime 同钟）。
            let mut titles = self.titles.lock().unwrap();
            let ids: Vec<String> = titles.keys().cloned().collect();
            for id in ids {
                if let Some(title) = titles.get(&id).cloned() {
                    if let ProbeTitle::Challenge { nonce, .. } = parse_probe_title(&title) {
                        let ack_at = chrono::Utc::now().timestamp_millis();
                        titles.insert(id, format!("ack:{nonce}:{pid}:{ack_at}"));
                    }
                }
            }
        }

        fn create_count(&self) -> usize {
            self.creates.load(Ordering::SeqCst)
        }

        fn write_count(&self) -> usize {
            self.writes.load(Ordering::SeqCst)
        }
    }

    fn runtime(home: &Path) -> GateRuntime {
        GateRuntime::with(
            10_000,
            1, // 轮询间隔最小化（测试不测真实时延）
            home.to_path_buf(),
            StdArc::new(now_ms),
        )
    }

    fn locked_window(pid: u32, started: i64) -> LockedWindow {
        LockedWindow {
            pid,
            started_at_ms: started,
            boot: Some("boot-gate".to_string()),
        }
    }

    fn lookup(pid: u32, started: i64) -> LockedWindowLookup {
        StdArc::new(move |sid| (sid == "ses-1").then(|| locked_window(pid, started)))
    }

    fn fact(started: i64) -> TuiProcessFact {
        StdArc::new(move |pid| (pid == 4242).then_some(started))
    }

    fn key(pid: u32) -> RuntimeKey {
        RuntimeKey {
            service_id: "svc".into(),
            pid,
            started_at_ms: STARTED,
        }
    }

    #[tokio::test]
    async fn pass_round_writes_challenge_and_control_file() {
        let home = tempfile::tempdir().unwrap();
        let probe = StdArc::new(FakeProbe::new());
        let rt = runtime(home.path());
        let transport = probe.transport();
        // 模拟 TUI ack：challenge 写出后（writes==1）槽位翻转为正确 pid 的 ack。
        let probe_for_ack = StdArc::clone(&probe);
        let watcher = tokio::spawn(async move {
            loop {
                tokio::time::sleep(StdDuration::from_millis(5)).await;
                if probe_for_ack.write_count() >= 1 {
                    probe_for_ack.ack_when_challenge_read(LOCKED_PID);
                    return;
                }
            }
        });
        let verdict = rt
            .verify(
                &key(7),
                &transport,
                "ses-1",
                Path::new("/tmp/project"),
                &lookup(LOCKED_PID, STARTED),
                &fact(STARTED),
            )
            .await;
        watcher.abort();
        let locked = verdict.expect("三件套齐必须放行");
        assert_eq!(locked, (LOCKED_PID, STARTED), "返回锁定身份供 ACK 后终验");
        assert_eq!(probe.write_count(), 1, "恰好一轮 challenge");
        assert_eq!(probe.create_count(), 1, "探针创建一次");
        // I5：轮次结束撤销控制文件（本轮成功）。
        let control = control_path(home.path(), LOCKED_PID);
        assert!(!control.exists(), "轮次结束必须撤销控制文件授权");
        // 轮内控制文件内容：在守卫等待期挂起一次挑战来取证（见 hold 专用用例）。
    }

    fn control_path(home: &Path, pid: u32) -> PathBuf {
        home.join(".tuvis/opencode-tui")
            .join(format!("{pid}.probe"))
    }

    /// I5：控制文件在轮内存在且绑定轮次；两把锁定窗口先后取得控制权时，
    /// 旧窗口的授权在旧轮结束即撤销，不与新窗口并存。
    #[tokio::test]
    async fn control_file_lifecycle_holds_across_round() {
        let home = tempfile::tempdir().unwrap();
        let probe = StdArc::new(FakeProbe::new());
        let rt = StdArc::new(runtime(home.path()));
        let transport = probe.transport();

        // 第一轮：锁定 TUI A——ack 悬挂直到我们取证控制文件。
        let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let probe_for_ack = StdArc::clone(&probe);
        // JoinHandle 不再消费：tokio 任务 detach 继续运行（观察者自灭于 ack 应答）。
        let _watcher = tokio::spawn(async move {
            loop {
                tokio::time::sleep(StdDuration::from_millis(2)).await;
                if probe_for_ack.write_count() >= 1 {
                    let _ = held_tx.send(());
                    let _ = release_rx.await;
                    probe_for_ack.ack_when_challenge_read(LOCKED_PID);
                    return;
                }
            }
        });
        let rt_in_round = StdArc::clone(&rt);
        let probe_in_round = StdArc::clone(&probe);
        let held = async move {
            let probe = probe_in_round;
            let rt = rt_in_round;
            let transport = GateTransport {
                read_title: Box::new({
                    let p = StdArc::clone(&probe);
                    move |probe| {
                        let probe = probe.to_string();
                        let p = StdArc::clone(&p);
                        Box::pin(async move { p.read_once(&probe).await })
                    }
                }),
                create_probe: Box::new({
                    let p = StdArc::clone(&probe);
                    move |dir| {
                        let _ = dir;
                        let p = StdArc::clone(&p);
                        Box::pin(async move {
                            let index = p.creates.fetch_add(1, Ordering::SeqCst);
                            let id = if index == 0 {
                                "ses_probe".to_string()
                            } else {
                                format!("ses_probe_{}", index + 1)
                            };
                            p.titles
                                .lock()
                                .unwrap()
                                .insert(id.clone(), "probe:tuvis".to_string());
                            Ok(id)
                        })
                    }
                }),
                write_title: Box::new({
                    let p = StdArc::clone(&probe);
                    move |probe, title| {
                        let probe = probe.to_string();
                        let title = title.to_string();
                        let p = StdArc::clone(&p);
                        Box::pin(async move { p.write_once(&probe, &title).await })
                    }
                }),
            };
            rt.verify(
                &key(7),
                &transport,
                "ses-1",
                Path::new("/tmp/project"),
                &lookup(LOCKED_PID, STARTED),
                &fact(STARTED),
            )
            .await
        };
        let round_a = tokio::spawn(held);
        held_rx.await.expect("challenge 已写出");
        // 轮内取证：控制文件在场，绑定本轮 nonce + 代际 + 有效期，0600。
        let control = control_path(home.path(), LOCKED_PID);
        let raw = std::fs::read(&control).expect("轮内控制文件必须在场");
        let parsed: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(parsed["sessionID"], "ses_probe");
        assert_eq!(parsed["expectPid"], LOCKED_PID);
        assert_eq!(parsed["expectBoot"], "boot-gate");
        let nonce_in_file = parsed["nonce"]
            .as_str()
            .expect("授权必须绑定轮次 nonce")
            .to_string();
        let deadline = parsed["deadlineMs"].as_i64().expect("授权必须带有效期");
        assert!(deadline > now_ms(), "有效期必须在未来");
        let probe_id = probe.first_probe_id().unwrap();
        let challenge = probe
            .titles
            .lock()
            .unwrap()
            .get(&probe_id)
            .cloned()
            .unwrap();
        assert!(
            challenge.starts_with(&format!("chal:{nonce_in_file}:")),
            "控制文件 nonce 与 challenge 同轮：{challenge}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&control).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "控制文件必须 0600");
        }
        release_tx.send(()).unwrap();
        round_a.await.unwrap().expect("第一轮放行");
        assert!(!control.exists(), "旧轮结束即撤销 A 的授权");

        // 第二轮：换锁定窗口 B（不同 pid/boot）——B 取得控制权，A 无授权可循。
        let other_pid = LOCKED_PID + 9;
        let other_locked = LockedWindow {
            pid: other_pid,
            started_at_ms: STARTED,
            boot: Some("boot-gate-b".to_string()),
        };
        let other_lookup: LockedWindowLookup =
            StdArc::new(move |sid: &str| (sid == "ses-1").then(|| other_locked.clone()));
        let other_fact: TuiProcessFact =
            StdArc::new(move |pid: u32| (pid == other_pid).then_some(STARTED));
        let probe_b = StdArc::clone(&probe);
        let watcher_b = tokio::spawn(async move {
            loop {
                tokio::time::sleep(StdDuration::from_millis(2)).await;
                if probe_b.write_count() >= 2 {
                    probe_b.ack_when_challenge_read(other_pid);
                    return;
                }
            }
        });
        rt.verify(
            &key(8),
            &transport,
            "ses-1",
            Path::new("/tmp/project"),
            &other_lookup,
            &other_fact,
        )
        .await
        .expect("第二轮（新锁定窗口）放行");
        watcher_b.abort();
        assert!(
            !control_path(home.path(), LOCKED_PID).exists(),
            "旧锁定窗口 A 不再持有授权"
        );
        assert!(
            !control_path(home.path(), other_pid).exists(),
            "第二轮结束同样撤销 B 的授权"
        );
    }

    #[tokio::test]
    async fn no_ack_within_window_rejects_fail_closed() {
        let home = tempfile::tempdir().unwrap();
        let probe = StdArc::new(FakeProbe::new());
        let rt = GateRuntime::with(
            300, // 短窗：拒绝路径不拖慢测试（fake 传输段每次挂起 15ms）
            1,
            home.path().to_path_buf(),
            StdArc::new(now_ms),
        );
        let transport = probe.transport();
        let verdict = rt
            .verify(
                &key(7),
                &transport,
                "ses-1",
                Path::new("/tmp/project"),
                &lookup(LOCKED_PID, STARTED),
                &fact(STARTED),
            )
            .await;
        let err = verdict.expect_err("零 ack 必须拒绝");
        assert_eq!(err.code, codes::OC_GATE_NO_ACK);
        // 拒绝路径零用户会话写：传输层只发生过探针标题写；控制文件已撤销。
        assert_eq!(probe.write_count(), 1);
        assert!(
            !control_path(home.path(), LOCKED_PID).exists(),
            "失败轮同样撤销授权"
        );
    }

    #[tokio::test]
    async fn wrong_pid_ack_rejects_pid_mismatch() {
        let home = tempfile::tempdir().unwrap();
        let probe = StdArc::new(FakeProbe::new());
        let rt = runtime(home.path());
        let transport = probe.transport();
        let probe_for_ack = StdArc::clone(&probe);
        let watcher = tokio::spawn(async move {
            loop {
                tokio::time::sleep(StdDuration::from_millis(5)).await;
                if probe_for_ack.write_count() >= 1 {
                    // 错误 TUI 的合法 ack（E4：pid 是唯一防线）。
                    probe_for_ack.ack_when_challenge_read(LOCKED_PID + 1);
                    return;
                }
            }
        });
        let verdict = rt
            .verify(
                &key(7),
                &transport,
                "ses-1",
                Path::new("/tmp/project"),
                &lookup(LOCKED_PID, STARTED),
                &fact(STARTED),
            )
            .await;
        watcher.abort();
        assert_eq!(
            verdict.expect_err("pid 不匹配必须拒绝").code,
            codes::OC_GATE_PID_MISMATCH
        );
    }

    #[tokio::test]
    async fn locked_window_absent_rejects_before_any_probe_io() {
        let home = tempfile::tempdir().unwrap();
        let probe = StdArc::new(FakeProbe::new());
        let rt = runtime(home.path());
        let none_lookup: LockedWindowLookup = StdArc::new(|_sid: &str| None);
        let transport = probe.transport();
        let tui_fact = fact(STARTED);
        let verdict = rt
            .verify(
                &key(7),
                &transport,
                "ses-1",
                Path::new("/tmp/project"),
                &none_lookup,
                &tui_fact,
            )
            .await;
        assert_eq!(
            verdict.expect_err("无新鲜锁定窗口必须拒绝").code,
            codes::OC_GATE_PID_UNVERIFIED
        );
        assert_eq!(probe.create_count(), 0, "拒绝发生在任何探针 IO 之前");
        assert_eq!(probe.write_count(), 0);
    }

    #[tokio::test]
    async fn tui_generation_mismatch_rejects_pid_unverified() {
        let home = tempfile::tempdir().unwrap();
        let probe = StdArc::new(FakeProbe::new());
        let rt = runtime(home.path());
        // 锁定 started=1000，进程事实=2000（换代/PID 复用）→ 拒绝。
        let verdict = rt
            .verify(
                &key(7),
                &probe.transport(),
                "ses-1",
                Path::new("/tmp/project"),
                &lookup(LOCKED_PID, STARTED),
                &fact(2_000),
            )
            .await;
        assert_eq!(
            verdict.expect_err("换代核对不过必须拒绝").code,
            codes::OC_GATE_PID_UNVERIFIED
        );
        assert_eq!(probe.write_count(), 0);
    }

    #[tokio::test]
    async fn unreachable_service_rejects_fail_closed() {
        let home = tempfile::tempdir().unwrap();
        let mut probe = StdArc::new(FakeProbe::new());
        Arc::get_mut(&mut probe).unwrap().read_unreachable = true;
        let rt = runtime(home.path());
        let transport = probe.transport();
        let tui_fact = fact(STARTED);
        let locked = lookup(LOCKED_PID, STARTED);
        let verdict = rt
            .verify(
                &key(7),
                &transport,
                "ses-1",
                Path::new("/tmp/project"),
                &locked,
                &tui_fact,
            )
            .await;
        assert_eq!(
            verdict.expect_err("服务不可达必须拒绝").code,
            codes::OC_GATE_UNREACHABLE
        );
        assert_eq!(probe.write_count(), 0);
    }

    /// I3：预读非 2xx（≠404）= Status 分诊 → 立即拒绝，不重建不继续本轮。
    #[tokio::test]
    async fn probe_read_status_error_rejects_without_rebuild() {
        let home = tempfile::tempdir().unwrap();
        let mut probe = StdArc::new(FakeProbe::new());
        {
            let p = Arc::get_mut(&mut probe).unwrap();
            p.read_status_500 = true;
        }
        let rt = runtime(home.path());
        let transport = probe.transport();
        let tui_fact = fact(STARTED);
        let locked = lookup(LOCKED_PID, STARTED);
        let verdict = rt
            .verify(
                &key(7),
                &transport,
                "ses-1",
                Path::new("/tmp/project"),
                &locked,
                &tui_fact,
            )
            .await;
        assert_eq!(
            verdict.expect_err("非 404 状态错误必须拒绝").code,
            codes::OC_GATE_UNREACHABLE
        );
        assert_eq!(probe.create_count(), 1, "首次建探针后不得再重建");
        assert_eq!(probe.write_count(), 0, "绝不带着未分诊的失败继续本轮");
    }

    /// I3：2xx 缺标题 = InvalidBody → 结构化拒绝，不重建、不覆盖写。
    #[tokio::test]
    async fn probe_read_missing_title_rejects_as_title_invalid() {
        let home = tempfile::tempdir().unwrap();
        let mut probe = StdArc::new(FakeProbe::new());
        {
            let p = Arc::get_mut(&mut probe).unwrap();
            p.read_missing_title = true;
        }
        let rt = runtime(home.path());
        let transport = probe.transport();
        let tui_fact = fact(STARTED);
        let locked = lookup(LOCKED_PID, STARTED);
        let verdict = rt
            .verify(
                &key(7),
                &transport,
                "ses-1",
                Path::new("/tmp/project"),
                &locked,
                &tui_fact,
            )
            .await;
        assert_eq!(
            verdict.expect_err("缺标题必须结构化拒绝").code,
            codes::OC_GATE_TITLE_INVALID
        );
        assert_eq!(probe.write_count(), 0, "缺标题绝不当「不忙」直接覆盖");
    }

    #[tokio::test]
    async fn probe_create_failure_rejects_probe_unwritable() {
        let home = tempfile::tempdir().unwrap();
        let mut probe = StdArc::new(FakeProbe::new());
        Arc::get_mut(&mut probe).unwrap().create_ok = false;
        let rt = runtime(home.path());
        let transport = probe.transport();
        let tui_fact = fact(STARTED);
        let locked = lookup(LOCKED_PID, STARTED);
        let verdict = rt
            .verify(
                &key(7),
                &transport,
                "ses-1",
                Path::new("/tmp/project"),
                &locked,
                &tui_fact,
            )
            .await;
        assert_eq!(
            verdict.expect_err("探针不可写必须拒绝").code,
            codes::OC_GATE_PROBE_UNWRITABLE
        );
    }

    /// I3/M1：探针在**缓存已建立**后被删除 → 下轮恰好受控重建一次（不多不少）。
    #[tokio::test]
    async fn cached_probe_deleted_is_recreated_exactly_once() {
        let home = tempfile::tempdir().unwrap();
        let probe = StdArc::new(FakeProbe::new());
        let rt = runtime(home.path());
        // 第一轮：建缓存 + 放行。
        let transport = probe.transport();
        let probe_for_ack = StdArc::clone(&probe);
        let watcher = tokio::spawn(async move {
            loop {
                tokio::time::sleep(StdDuration::from_millis(2)).await;
                if probe_for_ack.write_count() >= 1 {
                    probe_for_ack.ack_when_challenge_read(LOCKED_PID);
                    return;
                }
            }
        });
        rt.verify(
            &key(7),
            &transport,
            "ses-1",
            Path::new("/tmp/project"),
            &lookup(LOCKED_PID, STARTED),
            &fact(STARTED),
        )
        .await
        .expect("第一轮放行");
        watcher.abort();
        assert_eq!(probe.create_count(), 1);
        // 探针被删（下一轮预读 404 → 受控重建一次）。
        let probe_id = probe.first_probe_id().unwrap();
        probe.deleted.lock().unwrap().insert(probe_id);
        let probe_for_ack = StdArc::clone(&probe);
        let watcher2 = tokio::spawn(async move {
            loop {
                tokio::time::sleep(StdDuration::from_millis(2)).await;
                if probe_for_ack.write_count() >= 2 {
                    probe_for_ack.ack_when_challenge_read(LOCKED_PID);
                    return;
                }
            }
        });
        rt.verify(
            &key(7),
            &transport,
            "ses-1",
            Path::new("/tmp/project"),
            &lookup(LOCKED_PID, STARTED),
            &fact(STARTED),
        )
        .await
        .expect("第二轮（重建后）放行");
        watcher2.abort();
        assert_eq!(
            probe.create_count(),
            2,
            "恰重建一次（第 1 轮 + 第 2 轮重建）"
        );
    }

    /// I2：整轮受单调截止约束——读挂起超过剩余预算即拒绝，绝不无限等。
    #[tokio::test]
    async fn slow_reads_are_bounded_by_monotonic_deadline() {
        let home = tempfile::tempdir().unwrap();
        let mut probe = StdArc::new(FakeProbe::new());
        {
            let p = Arc::get_mut(&mut probe).unwrap();
            // 预读挂 80ms > 窗口 30ms：预算耗尽即拒。
            p.read_delay_ms = 80;
        }
        let rt = GateRuntime::with(30, 1, home.path().to_path_buf(), StdArc::new(now_ms));
        let transport = probe.transport();
        let tui_fact = fact(STARTED);
        let locked = lookup(LOCKED_PID, STARTED);
        let started = std::time::Instant::now();
        let verdict = rt
            .verify(
                &key(7),
                &transport,
                "ses-1",
                Path::new("/tmp/project"),
                &locked,
                &tui_fact,
            )
            .await;
        assert_eq!(
            verdict.expect_err("超预算读必须拒绝").code,
            codes::OC_GATE_UNREACHABLE
        );
        assert!(
            started.elapsed() < StdDuration::from_millis(2_000),
            "拒绝必须及时返回（实测 {:?}）",
            started.elapsed()
        );
        assert_eq!(probe.write_count(), 0, "预算耗尽不写 challenge");
    }

    /// I3：未过期他者 challenge 在飞 → BUSY 退避（零新 challenge 写）。
    #[tokio::test]
    async fn live_foreign_challenge_at_preread_rejects_busy() {
        let home = tempfile::tempdir().unwrap();
        let probe = StdArc::new(FakeProbe::new());
        let rt = runtime(home.path());
        // 先跑一轮建缓存（探针槽位翻转为 ack），再放未过期他者 challenge——
        // BUSY 判据只对**存量**槽位生效（新建探针必须带初始标题，I3 规则更严）。
        {
            let transport = probe.transport();
            let tui_fact = fact(STARTED);
            let locked = lookup(LOCKED_PID, STARTED);
            let probe_for_ack = StdArc::clone(&probe);
            let watcher = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(StdDuration::from_millis(2)).await;
                    if probe_for_ack.write_count() >= 1 {
                        probe_for_ack.ack_when_challenge_read(LOCKED_PID);
                        return;
                    }
                }
            });
            rt.verify(
                &key(7),
                &transport,
                "ses-1",
                Path::new("/tmp/project"),
                &locked,
                &tui_fact,
            )
            .await
            .expect("第一轮建缓存");
            watcher.abort();
        }
        let probe_id = probe.first_probe_id().expect("第一轮已建探针");
        probe.set_title(
            &probe_id,
            &challenge_title(
                "ffffffffffffffffffffffffffffffff",
                now_ms() - 2_000, // 未过期（窗 10s）的他者 challenge
            ),
        );
        let transport = probe.transport();
        let tui_fact = fact(STARTED);
        let locked = lookup(LOCKED_PID, STARTED);
        let verdict = rt
            .verify(
                &key(7),
                &transport,
                "ses-1",
                Path::new("/tmp/project"),
                &locked,
                &tui_fact,
            )
            .await;
        assert_eq!(
            verdict.expect_err("在飞他者 challenge 必须退避拒绝").code,
            codes::OC_GATE_BUSY
        );
        assert_eq!(
            probe.write_count(),
            1,
            "只有第一轮的 challenge；退避轮零新 challenge 写"
        );
    }

    /// GFix：单飞**获取**必须有界——在飞轮次被外部条件拖住（永不归还执行权）时，
    /// 后续写必须按预算拿 OC_GATE_BUSY 受控退避（零探针创建、零 challenge 写、
    /// 落确定性墓碑、意图可重试），而不是无限排队挂死整条写路径。
    /// 事故指纹（task-GE-report §5）：并发 stop/send 全部 hang ≥6min、零动作行、
    /// 零墓碑、reads 正常——唯一的全写路径串行点就是本 flight。
    #[tokio::test]
    async fn flight_acquire_is_bounded_when_a_round_never_finishes() {
        let home = tempfile::tempdir().unwrap();
        let probe = StdArc::new(FakeProbe::new());
        // 窗 30ms → 获取预算 = 2×窗 + 余量（小，测试快）；hold 不放行 = 一轮永不结束。
        let rt = StdArc::new(GateRuntime::with(
            30,
            1,
            home.path().to_path_buf(),
            StdArc::new(now_ms),
        ));
        let hold = rt.flight.lock().await;
        let rt_for_verify = StdArc::clone(&rt);
        let transport = probe.transport();
        let tui_fact = fact(STARTED);
        let locked = lookup(LOCKED_PID, STARTED);
        let started = std::time::Instant::now();
        let task = tokio::spawn(async move {
            rt_for_verify
                .verify(
                    &key(7),
                    &transport,
                    "ses-1",
                    Path::new("/tmp/project"),
                    &locked,
                    &tui_fact,
                )
                .await
        });
        let verdict = match tokio::time::timeout(StdDuration::from_secs(5), task).await {
            Ok(inner) => inner.expect("verify 任务不 panic"),
            Err(_elapsed) => {
                drop(hold);
                panic!("单飞被占时写路径无限排队（GE 事故指纹复现）：5s 内未拿到受控退避");
            }
        };
        let elapsed = started.elapsed();
        drop(hold);
        let err = verdict.expect_err("单飞被占时必须有界受控拒绝，而不是无限排队");
        assert_eq!(err.code, codes::OC_GATE_BUSY);
        assert!(
            elapsed < StdDuration::from_secs(4),
            "获取预算必须生效（实测 {elapsed:?}）"
        );
        assert_eq!(probe.write_count(), 0, "溢出退避零 challenge 写");
        assert_eq!(
            probe.creates.load(Ordering::SeqCst),
            0,
            "溢出退避零探针创建"
        );
    }

    /// GFix：捕获形态入口（`verify_with_locked`，生产写权链实际走的入口）与查询式
    /// 入口共用同一有界获取——两入口都要受预算约束。
    #[tokio::test]
    async fn flight_acquire_is_bounded_on_locked_entry_too() {
        let home = tempfile::tempdir().unwrap();
        let probe = StdArc::new(FakeProbe::new());
        let rt = StdArc::new(GateRuntime::with(
            30,
            1,
            home.path().to_path_buf(),
            StdArc::new(now_ms),
        ));
        let hold = rt.flight.lock().await;
        let rt_for_verify = StdArc::clone(&rt);
        let transport = probe.transport();
        let tui_fact = fact(STARTED);
        let locked = locked_window(LOCKED_PID, STARTED);
        let task = tokio::spawn(async move {
            rt_for_verify
                .verify_with_locked(
                    &key(7),
                    &transport,
                    Path::new("/tmp/project"),
                    Some(&locked),
                    &tui_fact,
                    |_| Ok(()),
                )
                .await
        });
        let verdict = match tokio::time::timeout(StdDuration::from_secs(5), task).await {
            Ok(inner) => inner.expect("verify_with_locked 任务不 panic"),
            Err(_elapsed) => {
                drop(hold);
                panic!("捕获形态入口同样必须受获取预算约束：5s 内未拿到受控退避");
            }
        };
        drop(hold);
        let err = verdict.expect_err("单飞被占时捕获形态必须有界受控拒绝");
        assert_eq!(err.code, codes::OC_GATE_BUSY);
        assert_eq!(probe.write_count(), 0, "溢出退避零 challenge 写");
    }

    /// M1：单飞取证用真挂起的在飞窗口——并发峰值必须为 1。
    #[tokio::test]
    async fn concurrent_verifies_serialize_single_flight() {
        let home = tempfile::tempdir().unwrap();
        let probe = StdArc::new(FakeProbe::new());
        let rt = StdArc::new(runtime(home.path()));
        let transport_a = probe.transport();
        let transport_b = probe.transport();
        let probe_for_ack = StdArc::clone(&probe);
        let watcher = tokio::spawn(async move {
            loop {
                tokio::time::sleep(StdDuration::from_millis(2)).await;
                // 每次读到 challenge 都立即翻转 ack（两轮各自闭环）。
                probe_for_ack.ack_when_challenge_read(LOCKED_PID);
            }
        });
        let a = StdArc::clone(&rt);
        let b = StdArc::clone(&rt);
        let key_a = key(7);
        let key_b = key(8);
        let locked = lookup(LOCKED_PID, STARTED);
        let tui_fact = fact(STARTED);
        let (ra, rb) = tokio::join!(
            a.verify(
                &key_a,
                &transport_a,
                "ses-1",
                Path::new("/tmp/project"),
                &locked,
                &tui_fact,
            ),
            b.verify(
                &key_b,
                &transport_b,
                "ses-1",
                Path::new("/tmp/project"),
                &locked,
                &tui_fact,
            )
        );
        watcher.abort();
        ra.expect("第一轮放行");
        rb.expect("第二轮放行（串行）");
        assert!(
            probe.max_in_flight.load(Ordering::SeqCst) <= 1,
            "单飞：传输段并发峰值必须为 1（实测 {}）",
            probe.max_in_flight.load(Ordering::SeqCst)
        );
    }

    /// I2 残余：探针创建挂起超预算 → 受整轮同一单调截止约束（不再各发一整窗）。
    #[tokio::test]
    async fn slow_probe_create_is_bounded_by_deadline() {
        let home = tempfile::tempdir().unwrap();
        let mut probe = StdArc::new(FakeProbe::new());
        {
            let p = Arc::get_mut(&mut probe).unwrap();
            p.create_delay_ms = 80; // 创建挂 80ms > 窗口 30ms
        }
        let rt = GateRuntime::with(30, 1, home.path().to_path_buf(), StdArc::new(now_ms));
        let transport = probe.transport();
        let tui_fact = fact(STARTED);
        let locked = lookup(LOCKED_PID, STARTED);
        let started = std::time::Instant::now();
        let verdict = rt
            .verify(
                &key(7),
                &transport,
                "ses-1",
                Path::new("/tmp/project"),
                &locked,
                &tui_fact,
            )
            .await;
        assert_eq!(
            verdict.expect_err("超预算创建必须拒绝").code,
            codes::OC_GATE_UNREACHABLE
        );
        assert!(
            started.elapsed() < StdDuration::from_millis(2_000),
            "拒绝必须及时返回（实测 {:?}）",
            started.elapsed()
        );
        assert_eq!(probe.write_count(), 0, "预算耗尽不写 challenge");
    }

    /// I4 残余：探针身份持久注册表——跨 GateRuntime 实例（兔维斯重启模拟）仍可识别
    /// 历史探针会话，堵住「重启后旧探针被当普通会话认领/发送」的残余通道。
    #[tokio::test]
    async fn probe_registry_persists_across_gate_instances() {
        let home = tempfile::tempdir().unwrap();
        let probe = StdArc::new(FakeProbe::new());
        let rt1 = runtime(home.path());
        {
            let transport = probe.transport();
            let probe_for_ack = StdArc::clone(&probe);
            let watcher = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(StdDuration::from_millis(2)).await;
                    if probe_for_ack.write_count() >= 1 {
                        probe_for_ack.ack_when_challenge_read(LOCKED_PID);
                        return;
                    }
                }
            });
            rt1.verify(
                &key(7),
                &transport,
                "ses-1",
                Path::new("/tmp/project"),
                &lookup(LOCKED_PID, STARTED),
                &fact(STARTED),
            )
            .await
            .expect("第一轮放行");
            watcher.abort();
        }
        assert!(rt1.is_probe_session("ses_probe"), "本进程内已注册");
        // 注册表文件在场且 0600、含探针 id。
        let registry = home
            .path()
            .join(".tuvis/opencode-tui")
            .join("probe-registry.json");
        let raw = std::fs::read(&registry).expect("注册表文件必须在场");
        let list: Vec<String> = serde_json::from_slice(&raw).unwrap();
        assert!(list.iter().any(|id| id == "ses_probe"), "注册表含探针 id");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&registry).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "注册表必须 0600");
        }
        // 新实例（模拟重启）：零轮次即可识别历史探针。
        let rt2 = runtime(home.path());
        assert!(
            rt2.is_probe_session("ses_probe"),
            "跨实例（重启）仍必须识别历史探针"
        );
        assert!(!rt2.is_probe_session("ses_other"), "普通会话不得误报");
    }

    /// N3：round future 被 abort 时 RAII 守卫撤销控制文件授权（取消路径不留旧授权）。
    #[tokio::test]
    async fn aborting_round_revokes_control_file() {
        let home = tempfile::tempdir().unwrap();
        let mut probe = StdArc::new(FakeProbe::new());
        {
            let p = Arc::get_mut(&mut probe).unwrap();
            p.read_delay_ms = 400; // 挑战写出后轮询读挂起，制造可 abort 的在飞窗口
        }
        let rt = StdArc::new(GateRuntime::with(
            10_000,
            1,
            home.path().to_path_buf(),
            StdArc::new(now_ms),
        ));
        let transport = probe.transport();
        let probe_for_ack = StdArc::clone(&probe);
        let watcher = tokio::spawn(async move {
            loop {
                tokio::time::sleep(StdDuration::from_millis(2)).await;
                if probe_for_ack.write_count() >= 1 {
                    probe_for_ack.ack_when_challenge_read(LOCKED_PID);
                    return;
                }
            }
        });
        let tui_fact = fact(STARTED);
        let locked = lookup(LOCKED_PID, STARTED);
        let task = tokio::spawn(async move {
            rt.verify(
                &key(7),
                &transport,
                "ses-1",
                Path::new("/tmp/project"),
                &locked,
                &tui_fact,
            )
            .await
        });
        // 等挑战写出（控制文件已写入），确认在场。
        let control = control_path(home.path(), LOCKED_PID);
        let mut waited = StdDuration::from_millis(0);
        while !control.exists() && waited < StdDuration::from_secs(5) {
            tokio::time::sleep(StdDuration::from_millis(5)).await;
            waited += StdDuration::from_millis(5);
        }
        assert!(control.exists(), "轮内控制文件必须在场");
        // abort → future drop → RAII 守卫撤销。
        task.abort();
        tokio::time::sleep(StdDuration::from_millis(50)).await;
        watcher.abort();
        assert!(
            !control.exists(),
            "future abort 必须撤销控制文件授权（RAII 守卫覆盖取消路径）"
        );
    }

    /// I4：探针目标身份注册——创建过的探针会话 id 可被写入口识别。    /// I4：探针目标身份注册——创建过的探针会话 id 可被写入口识别。
    #[tokio::test]
    async fn probe_session_is_registered_for_write_entry_check() {
        let home = tempfile::tempdir().unwrap();
        let probe = StdArc::new(FakeProbe::new());
        let rt = runtime(home.path());
        assert!(!rt.is_probe_session("ses_probe"), "未创建前不得误报");
        let transport = probe.transport();
        let probe_for_ack = StdArc::clone(&probe);
        let watcher = tokio::spawn(async move {
            loop {
                tokio::time::sleep(StdDuration::from_millis(2)).await;
                if probe_for_ack.write_count() >= 1 {
                    probe_for_ack.ack_when_challenge_read(LOCKED_PID);
                    return;
                }
            }
        });
        rt.verify(
            &key(7),
            &transport,
            "ses-1",
            Path::new("/tmp/project"),
            &lookup(LOCKED_PID, STARTED),
            &fact(STARTED),
        )
        .await
        .expect("放行");
        watcher.abort();
        assert!(
            rt.is_probe_session("ses_probe"),
            "创建过的探针会话必须注册为探针目标"
        );
        assert!(!rt.is_probe_session("ses-1"), "普通会话不得误报");
    }
}
