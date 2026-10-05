// 无头注入底座（H4 生命周期 / H6 回执·审计·版本门控）——Task 6。
//
// **范围**：本模块只做**共享底座**，不含任何工具通道（zcode/codex/WB/CLI 四家
// 分属 Task 8/9/11/13）。四层结构：
// - [`receipt`]：回执归一（纯函数；前缀跳过 JSON 解析、未知帧不猜、200 字截断）；
// - [`runner`]：进程生命周期（全局并发上限、watchdog 超时、取消、kill 进程树）；
// - [`gate`]：版本门控探针（`--prompt` 干跑 / `queue --help` 子命令在场 + 结果缓存）；
// - [`turn`]：共享回合件（Task 9 复审上提：执行缝 / 回执→审计词 / 串行锁登记表 /
//   证据头）——各通道**只依赖底座**，通道之间不互相依赖。
//
// **审计归属（Task 6 裁决 A）**：无头动作落**既有的** `write_audit` 表（W5 单一账本，
// 9 列 NOT NULL；该表 `action` 列无 CHECK 约束——见 `database/schema.rs`，故**无需
// migration**）。设备身份（device_id/device_name）来自移动端 gate 上下文，
// **runner 不自造**：runner 只归一回执 + 记录终止方，由端点（持有 gate 上下文与连接）
// 经 [`audit_headless`] 落 `headless` / `headless_cancel` 两行。
pub mod codex;
pub mod gate;
pub mod receipt;
pub mod runner;
/// 共享回合件（Task 9 复审上提：执行缝 / 审计词 / 串行锁登记表）——zcode 与 codex 现共用，
/// WB（Task 11）/ H11 三家（Task 13）接入时同规，禁止再从 `zcode.rs` 取
pub mod turn;
/// H9 WorkBuddy ACP 通道（Task 11；zcode/codex/CLI 三家分属 Task 8/9/13）
pub mod wb_acp;
/// H7 zcode 无头通道（Task 8；codex/workbuddy/CLI 三家分属 Task 9/11/13）
pub mod zcode;
/// H10 zcode 无头**新建**（Task 12）：候选列表/手填校验纯核 + 新建回合编排
/// （命令形态复用 [`zcode::build_create_argv`] 的 `resume = None` 形态）
pub mod zcode_create;

/// 无头动作审计词（H6）：turn 落账（终态 = 回执终态）
pub const ACTION_HEADLESS: &str = "headless";
/// 无头取消审计词（H4/H6）：移动端主动取消（**先到者生效**——watchdog 先到则记
/// `headless` 且 stage=timeout，不落本词）
pub const ACTION_HEADLESS_CANCEL: &str = "headless_cancel";

/// watchdog 默认超时（H4 / 裁决 15：600s；两端实测 turn 仅 8–23s，留足余量）
pub const DEFAULT_TIMEOUT_MS: u64 = 600_000;
/// 超时下界（1s；0 会让 watchdog 即刻到点）
pub const MIN_TIMEOUT_MS: u64 = 1_000;
/// 超时上界（1h；防手滑写成天数级看门狗形同虚设）
pub const MAX_TIMEOUT_MS: u64 = 3_600_000;
/// 全局并发上限默认值（H4：默认 2）
pub const DEFAULT_CONCURRENCY: usize = 2;
/// 并发下界（1；0 会让所有请求永久排队）
pub const MIN_CONCURRENCY: usize = 1;
/// 并发上界（8；防手机端连点多会话打爆机器）
pub const MAX_CONCURRENCY: usize = 8;

/// 超时 clamp（越界一律收进区间——与 `remote.max_devices` 的 clamp 口径一致）
pub fn clamp_timeout_ms(v: u64) -> u64 {
    v.clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS)
}

/// 并发上限 clamp
pub fn clamp_concurrency(v: usize) -> usize {
    v.clamp(MIN_CONCURRENCY, MAX_CONCURRENCY)
}

/// H4 配置落点取值（watchdog 超时 + 全局并发上限）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadlessLimits {
    pub timeout_ms: u64,
    pub concurrency: usize,
}

impl Default for HeadlessLimits {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_TIMEOUT_MS,
            concurrency: DEFAULT_CONCURRENCY,
        }
    }
}

impl HeadlessLimits {
    /// 运行期读取（runner 启动时用；spec H4「runner 启动时读取」）。
    /// 与 `remote_status` 下发**同一条读取路径**（`remote::*_conn`）——杜绝
    /// 「设置页显示 5 分钟、runner 却按 10 分钟跑」的双轨漂移。
    pub fn from_conn(conn: &rusqlite::Connection) -> Self {
        Self {
            timeout_ms: crate::remote::headless_timeout_ms_conn(conn),
            concurrency: crate::remote::headless_concurrency_conn(conn),
        }
    }
}

/// 无头审计上下文（**由调用方注入**）：设备身份来自移动端 gate 上下文，
/// runner 与本模块都不自造设备名/会话号——机器自发动作不得冒充某台手机。
pub struct HeadlessAuditCtx {
    pub device_id: String,
    pub device_name: String,
    pub agent_type: String,
    pub session_id: String,
    /// 无头通道名（Task 7/8 的 HeadlessKind 展示名，如 `headless_zcode`）——
    /// **不是**终端注入器名（那对无头动作是假值）
    pub channel: String,
    /// 命令形态/消息原文：摘要按 W5 口径在 [`crate::inject::audit_write_channel`] 内现算
    pub content: String,
}

/// **耗时口径（H6「耗时」；评审 Important 2）**：`write_audit` 表**没有 duration 列**
/// （9 列全 NOT NULL，见 `database/schema.rs`），故把耗时**显式编码进 `result` 列**：
/// `"<终态> · <duration_ms>ms"`（如 `ok · 1234ms`）。选 result 而非 summary 的理由：
/// summary 是**内容摘要**（W5 口径，80 字截断，`dao/write_audit.rs` 词表注释已声明），
/// 混入耗时会让摘要口径漂移；result 本就承载「回执终态」，追加可读的秒表读数最自然，
/// 审计页原样展示即可读，且 `· <n>ms` 可 grep。口径由本函数**单点产生**并被测试钉死
/// （`audit_result_pins_duration_encoding`）。若将来要独立成列 → 需 migration + 审计
/// 视图同步（此处登记，不擅自改表）。
pub fn audit_result(result: &str, duration_ms: u64) -> String {
    format!("{result} · {duration_ms}ms")
}

/// 无头审计落账（**单一账本**：既有 `write_audit` 表 + 既有 W5 摘要口径）。
/// 端点侧在回合终结（`ACTION_HEADLESS`）与取消生效（`ACTION_HEADLESS_CANCEL`）
/// 两处调用；runner 不落账（它没有设备身份，也没有连接）。
/// `duration_ms` 由 `Receipt::duration_ms` 原样传入（耗时口径见 [`audit_result`]）。
///
/// **登记（Task 6 现状）**：本函数**目前没有生产调用者**——Task 6 只交付底座，接线在
/// Task 8/9/11/13 的端点（`pub fn` 不触发 dead_code，未接线既不会编译报错也不会测试红，
/// 故在此显式登记，别让它被漏掉）。
pub fn audit_headless(
    conn: &rusqlite::Connection,
    ctx: &HeadlessAuditCtx,
    action: &str,
    result: &str,
    duration_ms: u64,
) {
    crate::inject::audit_write_channel(
        conn,
        &ctx.device_id,
        &ctx.device_name,
        &ctx.agent_type,
        &ctx.session_id,
        &ctx.channel,
        &ctx.content,
        action,
        &audit_result(result, duration_ms),
    );
}

/// **H4 配置生效内核（可测核）**：从连接读两键 → 把**并发上限落到给定的全局名额** →
/// 返回限值。生产出口两处：① app 启动（`remote::init_headless_limits` ← `lib.rs` setup）
/// ② 设置页写入后（`remote::set_headless_limits_core`）。**并发是全局属性**（名额在
/// [`runner::global_sem`] 上），所以「改设置即生效」= 在这里 `set_cap`，不是每回合重建。
pub fn apply_limits_to(conn: &rusqlite::Connection, sem: &runner::GlobalSem) -> HeadlessLimits {
    let limits = HeadlessLimits::from_conn(conn);
    sem.set_cap(limits.concurrency);
    limits
}

/// **runner 启动装配（生产入口，计划 Step 4「runner 启动时读取」）**：一次把 H4 两键
/// 落到回合配置（**超时**随回合）与全局名额（**并发**全局瞬时）。端点每回合 spawn 前
/// 调用本函数而不是 [`runner::RunnerCfg::new`]——否则超时永远是默认 600s（设置形同虚设）。
///
/// **Task 8/9/11/13 的义务（登记：目前只由本文档约束，无类型强制）**：① 每回合经本函数
/// 建 runner（拿设置里的 watchdog 超时）；② 并发经 [`runner::global_sem`] 取名额（别自建
/// `GlobalSem`，否则「全局上限 2」名存实亡）；③ 回合终结/取消落账走 [`audit_headless`]
/// （含 `duration_ms`）。三件事都没有编译期强制——Task 7 的路由与 Task 8 的端点接线时
/// 请按此清单对齐。
pub fn runner_from_conn(program: &str, conn: &rusqlite::Connection) -> runner::RunnerCfg {
    let mut cfg = runner::RunnerCfg::new(program);
    let limits = apply_limits_to(conn, &runner::global_sem());
    cfg.apply_limits(limits);
    cfg
}

// ============================================================
// H5 权限档：通道 spawn 时**选定**的权限/审批面（Task 10）
// ============================================================
//
// **本模块只描述「档」**——不做审批流，也不实现 claude 的双向协议（后者归 Task 13 / C4）。
// 一个无头通道 spawn 时的权限行为由三件事决定（spec H5）：
//   ① claude = 审批走 stdio **双向桥**（`control_request` → 移动端审批卡 → `control_response`，
//      进程存活至 turn 结束 = 裁决 8 特例）——**argv 与协议归 Task 13**，本批只定义档
//      （[`ClaudeApprovalMode::Stdio`]）与移动端接口（`src/mobile/api.ts` 的
//      `HeadlessApprovalRequest`）；
//   ② codex exec / kimi / opencode = **策略驱动**：档在 spawn 时给定 ⇒ turn 不因审批阻塞，
//      需批准/被拒的事件回流回执、可调策略重试（**本批只交付「spawn 时选档」**：旗子注入
//      已接线；**档位展示面与改档/重试面都尚未接线**——展示随审批卡归 Task 13（C4），
//      改档属后续批次。勿据 `tier()` 的存在推断已有展示/选择通路）；
//   ③ zcode = `--mode` 档位，**唯一档 yolo**（裁决 14：Mac 实测 `build` 档在无 permission
//      client 时阻断全部工具执行 —— `No permission client configured for Bash`）。
//
// **边界（落码于此，勿在别处另说一套）**：
// - **codex queue 通道没有审批面**（H8 定案）：`queue` 只是把消息入队的**短命进程**，turn
//   执行（含工具审批）归 codex APP 自身、按 APP 自己的配置走 ⇒ MAM 侧没有可注入的档位旗子
//   （见 `codex::queue_argv`，测试 `queue_plan_carries_no_permission_flag` 钉死）；
// - **zcode yolo 没有审批面**（裁决 14）：无头 argv 只表达档位，审批交互只存在于 APP 内；
//   安全面由 H3 总开关（默认关）+ 开启知情文案承担（设置页 `settings.remote.headlessConfirm`）
//   ——不在这里造审批流；
// - **本批不做移动端主动切档**（三期 F3.1）：只有 spawn 时选档（旗子注入）已接线；
//   **档位展示面待 Task 13（C4）接线**（`tier()` 目前无生产调用者，见其文档）。

/// zcode 权限档（H5 / 裁决 14）：**只此一档**——yolo。
///
/// 为什么唯一：Mac 实测 `build` 档在**无 permission client** 时阻断全部工具执行
/// （`No permission client configured for Bash`）⇒ 无头通道不可用；yolo 保可用性。
/// 安全面由 H3 总开关（默认关）+ 开启知情文案承担（**无头通道无审批面**）。
/// `plan→yolo` 的粘滞疑云归版本门控复核；后续若要收紧档，**加变体 + 证据**，
/// 不要在 `zcode.rs` 里写第二份字面量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZcodePermissionMode {
    Yolo,
}

impl ZcodePermissionMode {
    /// argv 值/展示词**单点**（`--mode <wire>`）——argv 与移动端展示同源
    pub const fn wire(self) -> &'static str {
        match self {
            ZcodePermissionMode::Yolo => "yolo",
        }
    }

    /// 本档追加的 argv 旗子（调用方负责放到实测位置：`--cwd` 之后、`--json` 之前）
    pub fn flags(self) -> Vec<&'static str> {
        vec!["--mode", self.wire()]
    }
}

/// codex 审批策略档（H5）：**spawn 时给定**（策略驱动）⇒ turn 不因审批阻塞。
///
/// 值集取自本机 `codex-cli 0.160.0`：`-a/--ask-for-approval` 帮助的 `Possible values`
/// 只列 `on-request` / `never`，二进制内 `AskForApproval` 枚举另有 `untrusted`/`granular`
/// ——**本批只收 CLI 明示的两档**（可引证）；其余档名在 0.160.0 的 `-a` 面不出现、语义未经
/// 取证 ⇒ 不按版本猜测（新增档须同样有证据）。
///
/// `OnRequest` 是 spec H5 的默认档：`codex exec` 是非交互面，模型请求批准时**没有审批
/// 客户端** ⇒ 该请求即刻被拒并把失败返回给模型（回合不阻塞）——这正是「策略驱动」的含义；
/// 拒绝/需批准事件随回执/审计可见，用户可换档重发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexApprovalPolicy {
    /// 模型按需请求批准（**默认档**，spec H5）
    OnRequest,
    /// 从不请求批准（执行失败直接返回模型）。
    ///
    /// **当前生产不可达（Task 10 现状，如实登记）**：唯一生产入口
    /// [`PermissionSpec::codex_exec_default`] 恒 `OnRequest`，本批**没有**配置/环境变量/
    /// 移动端的档位选择面（spec H5 边界：不做移动端主动切档）。`Never` 是「策略可调」面
    /// 预留的合法档，只在测试与文档里被构造——**勿据此推断已有选择通路**。
    Never,
}

impl CodexApprovalPolicy {
    /// 配置值单点（`~/.codex/config.toml` 的 `approval_policy` 取值；旗子形如
    /// `-c approval_policy=<wire>`）
    pub const fn wire(self) -> &'static str {
        match self {
            CodexApprovalPolicy::OnRequest => "on-request",
            CodexApprovalPolicy::Never => "never",
        }
    }

    /// 旗子的**配置项原文**（`approval_policy=<wire>`；由 [`Self::wire`] 同源派生，
    /// 测试钉死「配置值 = approval_policy=<展示词>」不得漂移）
    pub const fn config_arg(self) -> &'static str {
        match self {
            CodexApprovalPolicy::OnRequest => "approval_policy=on-request",
            CodexApprovalPolicy::Never => "approval_policy=never",
        }
    }

    /// 本档追加的 argv 旗子。**位置有实测依据**（见 `codex::exec_resume_argv`）：
    /// `-c/--config` 在 `exec resume` 的选项表里在场 ⇒ 置于 `resume` **之后**（子命令内选项）
    pub fn flags(self) -> Vec<&'static str> {
        vec!["-c", self.config_arg()]
    }
}

/// claude 审批桥形态（H5 / C4）：`Stdio` = `--permission-prompt-tool stdio` 双向桥
/// （附录 E-①：还须恒带 `--permission-mode` 等一组 flag——**全集归 Task 13**）。
/// 本批只定义档与移动端接口，[`PermissionSpec::flags`] 对它**返回空**——绝不假装已接线。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeApprovalMode {
    Stdio,
}

impl ClaudeApprovalMode {
    /// 展示词单点（`stdio` = 审批走标准输入输出的双向控制面）
    pub const fn wire(self) -> &'static str {
        match self {
            ClaudeApprovalMode::Stdio => "stdio",
        }
    }
}

/// **H5 权限档参数面**：一个无头通道 spawn 时选定的档——**纯描述**（可测、可展示），
/// 由各通道的 argv 构造器消费（`zcode::build_argv` / `codex::exec_resume_argv`）。
/// 审批**流**不在这里：claude 的双向桥归 Task 13（本批只留接口与占位渲染）。
///
/// **env 面**：本批各档都是 argv 旗子，**没有 env 面**——若将来某档需要 env（如 claude 桥的
/// 开关），加 `env()` 方法并在此登记，**不预造空壳方法**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionSpec {
    /// zcode（H7/H10）：`--mode <档>`；唯一档 = yolo（裁决 14；**无审批面**）
    Zcode(ZcodePermissionMode),
    /// codex `exec resume`（H8 兜底路）：`-c approval_policy=<档>`（策略驱动，默认 on-request）；
    /// **queue 路没有审批面**（H8 定案）⇒ 该路不取本档
    CodexExec(CodexApprovalPolicy),
    /// claude `-p`（H11/C4）：审批走 stdio 双向桥（`control_request` ← → 移动端审批卡）。
    /// 本批只定义档与移动端接口；**argv 全集与协议归 Task 13**
    ClaudeP(ClaudeApprovalMode),
    /// kimi `-p`（H11/C4）：默认档——策略旗子形态**未取证**（Task 13 实机首步），不猜
    KimiDefault,
    /// opencode `run`（H11/C4）：默认档——同上
    OpencodeDefault,
}

impl PermissionSpec {
    /// zcode 通道的 spawn 档（**唯一来源**：`zcode.rs` 不再写字面 `yolo`）
    pub const fn zcode_default() -> Self {
        PermissionSpec::Zcode(ZcodePermissionMode::Yolo)
    }

    /// codex `exec resume` 的 spawn 档（**默认 on-request**，spec H5）
    pub const fn codex_exec_default() -> Self {
        PermissionSpec::CodexExec(CodexApprovalPolicy::OnRequest)
    }

    /// 本档追加的 argv 旗子（**不含**程序名/子命令；调用方按实测位置插入）。
    /// **空的两种情形都是有意为之**，不得读成「忘了填」：
    /// - [`PermissionSpec::ClaudeP`]：argv 全集归 Task 13（本批只留接口）；
    /// - kimi/opencode：策略旗子形态未取证（不猜、不发）。
    pub fn flags(self) -> Vec<&'static str> {
        match self {
            PermissionSpec::Zcode(m) => m.flags(),
            PermissionSpec::CodexExec(p) => p.flags(),
            PermissionSpec::ClaudeP(_) => Vec::new(),
            PermissionSpec::KimiDefault | PermissionSpec::OpencodeDefault => Vec::new(),
        }
    }

    /// 档的**展示词**（移动端/回执/审计读它——单一来源，勿另抄）：
    /// `yolo` / `on-request|never` / `stdio` / `default`。
    ///
    /// `default` 是 **MAM 侧占位词**（「不追加任何策略旗子、用 CLI 自身默认档」），
    /// **不是** CLI 自己的枚举词——C4 实测到 kimi/opencode 的旗子后按实测改词。
    ///
    /// **接线状态（Task 10 现状，如实登记）**：本函数**目前没有生产调用者**（与
    /// `audit_headless` 同款的显式登记；`pub` 不触发 dead_code，未接线既不会编译报错也不会
    /// 测试红，故别让它被漏掉）——**档位展示面尚未接线**：展示随审批卡归 **Task 13（C4）**
    /// （`HeadlessApprovalRequest.tier` 就是它的消费位）。本批只有测试与文档在约束它，
    /// **勿据其存在推断已有展示通路**。
    pub const fn tier(self) -> &'static str {
        match self {
            PermissionSpec::Zcode(m) => m.wire(),
            PermissionSpec::CodexExec(p) => p.wire(),
            PermissionSpec::ClaudeP(m) => m.wire(),
            PermissionSpec::KimiDefault | PermissionSpec::OpencodeDefault => "default",
        }
    }
}

/// 测试专用串行锁（仅 cfg(test)；与 `inject::queue::LOOP_HANDLE_TEST_LOCK` 同型同用法，
/// 故置于模块顶层而非 tests 内——`remote::mod` 的测试要跨模块取它）。理由：H4 的并发
/// 上限住在**进程级单例** `runner::global_sem()` 上，两个用例会改它并断言其值（本模块的
/// runner 启动装配、`remote::tests` 的写入内核）——并行跑时断言窗口可能交错（一个改 3、
/// 另一个读回 1）。**两测全程持本锁强制串行**（确定性方案，不靠重试启发式）。
/// 不持锁的那一个（`configured_concurrency_cap_actually_gates_second_turn`）用**自建**
/// `GlobalSem`，不涉全局单例。
#[cfg(test)]
pub(crate) static HEADLESS_CAP_TEST_LOCK: once_cell::sync::Lazy<std::sync::Mutex<()>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(()));

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        crate::database::migration::migrate(&conn).unwrap();
        conn
    }

    /// H4 配置落点的数字单点：默认值/上下界在此钉死（设置层只做 KV 解析与转发）
    #[test]
    fn limits_defaults_and_clamp_are_pinned() {
        assert_eq!(DEFAULT_TIMEOUT_MS, 600_000, "裁决 15：watchdog 默认 600s");
        assert_eq!(DEFAULT_CONCURRENCY, 2, "H4：全局并发默认 2");
        assert_eq!(HeadlessLimits::default().timeout_ms, DEFAULT_TIMEOUT_MS);
        assert_eq!(HeadlessLimits::default().concurrency, DEFAULT_CONCURRENCY);
        assert_eq!(
            clamp_timeout_ms(0),
            MIN_TIMEOUT_MS,
            "0 会即刻超时——抬到下界"
        );
        assert_eq!(clamp_timeout_ms(u64::MAX), MAX_TIMEOUT_MS);
        assert_eq!(clamp_timeout_ms(120_000), 120_000);
        assert_eq!(
            clamp_concurrency(0),
            MIN_CONCURRENCY,
            "0 会让所有请求永久排队"
        );
        assert_eq!(clamp_concurrency(99), MAX_CONCURRENCY);
        assert_eq!(clamp_concurrency(3), 3);
    }

    /// 审计落账单点（裁决 A）：与终端注入**同表同摘要口径**（W5 80 字摘要）；
    /// 设备/会话/通道/动作/终态/**耗时**逐列可查，不另造平行账本。
    #[test]
    fn audit_headless_writes_the_single_w5_ledger_row() {
        let conn = mem_conn();
        let ctx = HeadlessAuditCtx {
            device_id: "d1".into(),
            device_name: "iPhone".into(),
            agent_type: "zcode".into(),
            session_id: "sess_1".into(),
            channel: "headless_zcode".into(),
            content: format!("{}——超长消息不得整段落库", "改".repeat(200)),
        };
        audit_headless(&conn, &ctx, ACTION_HEADLESS, "ok", 1_234);
        audit_headless(&conn, &ctx, ACTION_HEADLESS_CANCEL, "cancelled", 800);
        let rows = crate::database::dao::write_audit::recent_conn(&conn, 10);
        assert_eq!(rows.len(), 2, "无头动作必须落既有 write_audit 表");
        // 最新在前：取消行
        assert_eq!(rows[0].action, ACTION_HEADLESS_CANCEL);
        assert_eq!(rows[0].result, "cancelled · 800ms", "耗时口径必须钉死");
        assert_eq!(rows[1].action, ACTION_HEADLESS);
        assert_eq!(rows[1].device_name, "iPhone");
        assert_eq!(rows[1].agent_type, "zcode");
        assert_eq!(rows[1].session_id, "sess_1");
        assert_eq!(rows[1].channel, "headless_zcode", "通道列 = 无头通道名");
        assert_eq!(rows[1].result, "ok · 1234ms", "H6「耗时」必须落账");
        assert!(
            rows[1].summary.chars().count() <= crate::inject::normalize::AUDIT_SUMMARY_CHARS + 1,
            "摘要必须走 W5 截断口径（耗时不得塞进 summary 污染摘要口径）: {}",
            rows[1].summary
        );
    }

    /// H6「耗时」编码口径（评审 Important 2）：`"<终态> · <n>ms"`——单点产生、纯函数可钉
    #[test]
    fn audit_result_pins_duration_encoding() {
        assert_eq!(audit_result("ok", 1_234), "ok · 1234ms");
        assert_eq!(audit_result("cancelled", 0), "cancelled · 0ms");
        assert_eq!(
            audit_result("failed(timeout)", 600_000),
            "failed(timeout) · 600000ms"
        );
    }

    /// 动作词表钉死（扩展词表时同步改 dao/write_audit.rs 注释与设置页口径）
    #[test]
    fn audit_actions_are_pinned() {
        assert_eq!(ACTION_HEADLESS, "headless");
        assert_eq!(ACTION_HEADLESS_CANCEL, "headless_cancel");
    }

    /// 评审 Important 3：H4「可配」必须**真生效**——设置值落到全局名额，
    /// 上限 1 时第二个并发回合**即时排队**（位置 2），不静默丢也不无限等
    #[test]
    fn configured_concurrency_cap_actually_gates_second_turn() {
        let conn = mem_conn();
        crate::database::dao::settings::set_setting_conn(
            &conn,
            crate::remote::KEY_HEADLESS_CONCURRENCY,
            "1",
        );
        let sem = runner::GlobalSem::new(DEFAULT_CONCURRENCY);
        let limits = apply_limits_to(&conn, &sem);
        assert_eq!(limits.concurrency, 1);
        assert_eq!(sem.cap(), 1, "设置值必须落到全局名额上限");
        let hold = sem.acquire(); // 占掉唯一名额 = 在飞的第一个回合
        let mut r = runner::RunnerCfg::for_test().sem(sem.clone());
        let out = r.run_once(|_| {});
        assert_eq!(
            out.status,
            receipt::ReceiptStatus::Queued,
            "上限 1 时第二个回合必须排队: {out:?}"
        );
        assert_eq!(out.queue_position(), Some(2));
        drop(hold);
        assert_eq!(sem.in_flight(), 0);
    }

    /// 计划 Step 4「runner 启动时读取」的生产入口：一次装配**超时 + 全局名额**（同一条
    /// 读取路径，杜绝「设置页 90s、runner 仍跑 600s」双轨）。
    /// 持 [`HEADLESS_CAP_TEST_LOCK`]：本测改**进程级单例**名额并与 remote 侧同型用例互斥。
    #[test]
    fn runner_from_conn_reads_both_keys_at_start() {
        let _serial = HEADLESS_CAP_TEST_LOCK.lock().unwrap();
        let conn = mem_conn();
        crate::database::dao::settings::set_setting_conn(
            &conn,
            crate::remote::KEY_HEADLESS_TIMEOUT_MS,
            "90000",
        );
        crate::database::dao::settings::set_setting_conn(
            &conn,
            crate::remote::KEY_HEADLESS_CONCURRENCY,
            "3",
        );
        let cfg = runner_from_conn("mam-test-stub", &conn);
        assert_eq!(
            cfg.watchdog_timeout_ms(),
            90_000,
            "超时必须来自设置（不是默认 600s）"
        );
        assert_eq!(
            runner::global_sem().cap(),
            3,
            "并发上限必须同时落到全局名额"
        );
        // 缺键 → 回默认（不把半配置状态当 0）
        let empty = mem_conn();
        let d = runner_from_conn("mam-test-stub", &empty);
        assert_eq!(d.watchdog_timeout_ms(), DEFAULT_TIMEOUT_MS);
        assert_eq!(runner::global_sem().cap(), DEFAULT_CONCURRENCY);
        // 复位全局名额（进程级单例，别把 3 留给后续用例）
        runner::global_sem().set_cap(DEFAULT_CONCURRENCY);
    }

    // ===== H5 权限档（Task 10）=====

    /// H5 权限档参数面（**纯描述、可测**）：每档的旗子 / 展示词 / 默认值逐项钉死。
    /// zcode 恒 `--mode yolo`（裁决 14，**唯一档**）；codex exec 默认 `on-request`（spec H5）；
    /// claude / kimi / opencode 的 argv 面归 C4（Task 13）——本批**空旗子**是「未接线」的
    /// 诚实表达（不假装已发），但**档位词仍须可展示**（移动端要读）。
    #[test]
    fn permission_spec_flags_and_tiers_are_pinned() {
        // zcode：唯一合法档 = yolo（裁决 14）
        let z = PermissionSpec::zcode_default();
        assert_eq!(z, PermissionSpec::Zcode(ZcodePermissionMode::Yolo));
        assert_eq!(z.flags(), vec!["--mode", "yolo"]);
        assert_eq!(z.tier(), "yolo");
        // codex exec：策略驱动，默认 on-request（spec H5）
        let c = PermissionSpec::codex_exec_default();
        assert_eq!(c, PermissionSpec::CodexExec(CodexApprovalPolicy::OnRequest));
        assert_eq!(c.flags(), vec!["-c", "approval_policy=on-request"]);
        assert_eq!(c.tier(), "on-request");
        // 策略**可调**（本轮只选档 + 展示；改档重试属后续批次）
        assert_eq!(
            PermissionSpec::CodexExec(CodexApprovalPolicy::Never).flags(),
            vec!["-c", "approval_policy=never"]
        );
        assert_eq!(
            PermissionSpec::CodexExec(CodexApprovalPolicy::Never).tier(),
            "never"
        );
        // claude：stdio 双向桥档（C4 接线；本批零旗子）
        let cl = PermissionSpec::ClaudeP(ClaudeApprovalMode::Stdio);
        assert_eq!(cl.tier(), "stdio");
        assert!(
            cl.flags().is_empty(),
            "claude 的 `--permission-prompt-tool stdio` argv 全集归 Task 13（附录 E-①）——\
             本批只定义档与移动端接口，空旗子即「未接线」的诚实表达"
        );
        // kimi / opencode：默认档（策略旗子形态待 C4 实机取证——不猜、不发）
        for spec in [PermissionSpec::KimiDefault, PermissionSpec::OpencodeDefault] {
            assert!(
                spec.flags().is_empty(),
                "C4 未取证前不得凭空发旗子: {spec:?}"
            );
            assert_eq!(spec.tier(), "default");
        }
    }

    /// 档位**可展示**（移动端要读）：展示词与旗子里的配置值**同源**——单一来源，勿另抄一份
    #[test]
    fn permission_tier_words_match_their_flags() {
        let z = PermissionSpec::zcode_default();
        assert_eq!(z.flags(), vec!["--mode", ZcodePermissionMode::Yolo.wire()]);
        assert_eq!(z.tier(), ZcodePermissionMode::Yolo.wire());
        let c = PermissionSpec::codex_exec_default();
        assert_eq!(
            c.flags(),
            vec!["-c", CodexApprovalPolicy::OnRequest.config_arg()],
            "旗子里的配置值必须由档单点产生"
        );
        assert_eq!(
            CodexApprovalPolicy::OnRequest.config_arg(),
            format!("approval_policy={}", c.tier()),
            "配置值 = approval_policy=<展示词>（两处不得漂移）"
        );
        assert_eq!(
            PermissionSpec::ClaudeP(ClaudeApprovalMode::Stdio).tier(),
            ClaudeApprovalMode::Stdio.wire()
        );
    }
}
