//! 注入路由表（W3 纯核 + H 系无头路由）：会话属性 → 通道决策 + 可见性预期。路由按宿主
//! 形态与工具门判定，**工具无关的终端宿主一律优先终端注入**（无头对已开 TUI 会分叉，
//! spec 裁决「路由规则按宿主形态，工具无关」）。
//!
//! ## H 系无头路由（Task 7 落地）
//! 无头通道（H7–H11）与终端通道同表分派：**一家一条判据**（见 [`headless_route`] 的
//! 逐家口径），无头结论一律 `Channel::Headless(HeadlessKind)` + 该工具按其宿主给的
//! 可见性档（[`Visibility`]，文案经 [`Visibility::note`]）。三条铁律：
//!
//! 1. **无头不需要 pid**：子进程由 MAM 自己 spawn（裁决 8），故无头判定**先于** `pid == 0`
//!    早退——「会话在册但无存活进程」正是无头的主场景（Task 7 义务 2 的重排序）；
//! 2. **判定只有一个出口**：`route()` 的结论即真相，H3 门经 [`headless_kind_of`] 读取
//!    （Task 5 的平行谓词 `is_headless_bound` 已删除，勿复活）；
//! 3. **dsh 刻意无无头通道**：H13（ACP stdio）不在本批 → 需要无头的 dsh 场合一律
//!    `dsh_headless_pending`（H3 开关因此今天不管 dsh——范围裁决，见 [`HeadlessKind`]）。

use crate::session::model::ProcessForm;

/// H 系无头通道族（spec H7–H13）：**路由词表**——只说「谁的会话归谁驱动」，通道本体
/// （argv 构造 / 子进程生命周期 / 回执解析）在 `inject::headless` 底座与各工具适配器
/// （Task 8/9/11/13）。
///
/// **H13（dsh · ACP stdio）刻意缺席**：dsh 写侧不在本批范围，故不给 dsh 任何无头变体
/// ——需要无头的 dsh 场合一律 `NotInjectable(dsh_headless_pending)`（见 [`headless_route`]）。
/// 这是**范围裁决而非遗漏**：dsh 因此不进无头绑定集，H3 总开关今天管不到它；H13 落地时
/// 加变体 + 路由分支即可（计划已注明「若审阅裁 H13 并入 → 改路由 `Headless(DshAcp)`」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessKind {
    /// H7 在册注入 / H10 无头新建：`ELECTRON_RUN_AS_NODE=1 <ZCode> <…>/zcode.cjs --prompt …`
    Zcode,
    /// H8：`codex queue --thread <UUID> --message …`（APP 在场首选；回执只证明入队不证明投递）
    CodexQueue,
    /// H8 改道：`codex -C <项目> exec resume <UUID> …`（APP 不在场；`-C` 是全局 flag 必须前置）
    CodexExec,
    /// H9 路线 A：WorkBuddy ACP（`POST <endpoint>/api/v1/acp/connect` → `session/prompt`）
    WbAcp,
    /// H11：`claude -p --resume <id> --output-format stream-json`（审批走 stdio 双向）
    ClaudeP,
    /// H11：`kimi -p -S <id>`
    KimiP,
    /// H11：`opencode run`
    OpencodeRun,
}

impl HeadlessKind {
    /// 通道 wire/审计名（`session-send-info` 的 channels 数组与审计 channel 列**同源**）：
    /// 与 Task 6 约定一致（`headless::HeadlessAuditCtx.channel` 文档：「Task 7/8 的
    /// HeadlessKind 展示名，如 `headless_zcode`」）——单一来源，端点不得另抄一份。
    pub const fn wire_name(self) -> &'static str {
        match self {
            HeadlessKind::Zcode => "headless_zcode",
            HeadlessKind::CodexQueue => "headless_codex_queue",
            HeadlessKind::CodexExec => "headless_codex_exec",
            HeadlessKind::WbAcp => "headless_wb_acp",
            HeadlessKind::ClaudeP => "headless_claude_p",
            HeadlessKind::KimiP => "headless_kimi_p",
            HeadlessKind::OpencodeRun => "headless_opencode_run",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Tmux,
    Iterm2,
    TerminalApp,
    WindowsConsole,
    /// H 系无头通道：进程由 MAM spawn，**不依赖终端宿主**（故不经理赔终端平台矩阵，
    /// 平台/版本支持由各通道适配器的 H6 版本门控判定）
    Headless(HeadlessKind),
}

impl Channel {
    /// 终端注入通道判定（无头候选一律不算）——端点分派（[`has_terminal_candidate`]）
    /// 与展示/审计共用的单一判据
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Channel::Headless(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Realtime,
    AfterRefresh,
    /// H7/H10 zcode **已信任工作区**：重启 ZCode APP 后可见（两端定案逐字，见 [`Visibility::note`]）
    AfterRestart,
    /// H7/H10 zcode **未信任工作区**：仅 MAM 可见（APP 永不收录——两端定案）
    TuvisOnly,
}

impl Visibility {
    /// 可见性提示文案（计划里的 `visibility_note` 元数据；spec H7/H10「两端定案文案」）：
    /// **后端给文案、前端只渲染**——与 H3 的 `HEADLESS_DISABLED_REASON` 同款单一措辞出口
    /// （移动页当前硬编码中文、i18n 随 M3 完善，故后端不另造 key 体系）。词表取自 spec
    /// 附录 A「有头可见性」列（实时 / 刷新后 / 重启级 / 仅 MAM 可见）；zcode 两档为
    /// **两端定案逐字文案，勿改写**。
    pub const fn note(self) -> &'static str {
        match self {
            Visibility::Realtime => "实时",
            Visibility::AfterRefresh => "刷新后可见",
            Visibility::AfterRestart => "已信任工作区：重启 ZCode 应用后可见",
            Visibility::TuvisOnly => "未信任工作区：仅兔维斯可见",
        }
    }
}

/// zcode 可见性两档的**单点映射**（H7/H10 两端定案）：已信任 → 重启 APP 后可见；
/// 未信任 → 仅 MAM 可见。信任判定（`~/.zcode/v2/setting.json` 的 recentProjects 只读
/// 探针，Task 8/12）**不是** `route` 的输入（纯路由核的输入契约只有 tool/form/pid/platform），
/// 故 [`headless_route`] 取已信任默认档，端点按探针结果经本函数降级——变体与文案只此一处。
pub const fn zcode_visibility(trusted: bool) -> Visibility {
    if trusted {
        Visibility::AfterRestart
    } else {
        Visibility::TuvisOnly
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteOutcome {
    Injectable {
        candidates: Vec<Channel>,
        visibility: Visibility,
    },
    NotInjectable {
        reason_code: &'static str,
        reason: String,
    },
}

/// 黑盒门（宪法附 A 矩阵）：**OpenClaw** ◐ gateway——写通道有条件存在（PL 定案，默认关，
/// 文档级），无外部写 API 故不可注入。
///
/// **Task 7 起本表只剩这一行**（义务 3）：workbuddy / dsh / zcode 的原 `blackbox` /
/// `headless_only` 两码已按**实际分派**改写（无头路由见 [`headless_route`]、dsh 见
/// `dsh_headless_pending`）——`blackbox` 只保留在**真的没有外部写 API** 的家上，别再把
/// 「有通道、只是本批不做」的工具塞进同一个码（Task 5 评审：那是第二真相源）。
fn tool_gate(tool: &str) -> Option<(&'static str, String)> {
    match tool {
        "openclaw" => Some(("blackbox", "OpenClaw 走 gateway，另评".into())),
        _ => None,
    }
}

/// **H 系无头路由表**（Task 7）：`Some` = 本工具/形态的注入面由无头通道决定（`Injectable`
/// 或明确的不可注入原因）；`None` = 落终端分支（[`route`] 继续往下判）。
///
/// # 为什么必须排在 `pid == 0` 早退之前（义务 2 的重排序）
/// 无头通道**不需要进程在场**——子进程由 MAM 自己 spawn（裁决 8，turn 生命周期 = 进程
/// 生命周期）。「会话在册但无存活进程」（`pid = 0`：未读卡 / DB-only 哨兵卡）恰恰是无头的
/// **主场景**；旧顺序（pid 早退在最前）会让这批会话永远读不到无头通道。
///
/// # 逐家判据（工具 × 形态 × 进程在场）
/// - `zcode`：工具级走无头，**不看形态**（H7/H10：ZCode 恒为 APP 内嵌 CLI，无真实终端
///   宿主形态）
/// - `workbuddy`：工具级走无头（H9 路线 A · ACP；恒 APP 形态，无终端注入面）
/// - `codex`：**按形态分派**（H8）——APP 托管 → `queue`（首选）+ `exec resume`（APP 不在
///   场的改道候选，二者按此优先级排列）；CLI 形态保持在产终端注入（W3：无头对已开 TUI
///   不路由）。CLI 形态 + `pid == 0` 仍是 `no_process`（[`HeadlessKind::CodexExec`] 的
///   触发条件是「APP 不在场」，不是「进程不在」，路由核看不到 APP 在场性——该判定归
///   Task 9 的端点/适配器）
/// - `claude` / `kimi` / `opencode`：**判据只能是「有无活进程可写」**（H11：三家同时存在
///   TUI 会话，`ProcessForm` 区分不了）——有活 pid = 已开 TUI = 终端注入；`pid == 0` =
///   没有可写的 TUI，无头是唯一通路（spec:169-173「全部走 H3–H6 底座」）
/// - `dsh`：**刻意不给无头通道**（H13 写侧不在本批，见 [`HeadlessKind`]）——CLI 形态 +
///   活进程的格子保留终端注入；APP 形态或进程不在（即「需要无头」的两种场合）→
///   `dsh_headless_pending`。**生产现状**：`monitor::dsh` 恒判 APP 形态（桌面宿主 pid，
///   无宿主则 0），故今天 dsh 一律落 `dsh_headless_pending`——CLI 形态那格是路由表口径，
///   若将来 dsh 出现真实终端宿主形态须复核（宪法附 A：dsh 注入 ◐ 另评，未定案）
/// - 其余工具（含 openclaw）：`None`
///
/// 可见性按 spec 附录 A「有头可见性」列取档；候选首位 = 首选通道（与既有终端三通道的
/// 「由简到繁」同款优先级语义）。
fn headless_route(tool: &str, form: ProcessForm, pid: u32) -> Option<RouteOutcome> {
    use HeadlessKind as K;
    let (candidates, visibility) = match tool {
        "zcode" => (
            vec![Channel::Headless(K::Zcode)],
            // 已信任默认档；未信任经 [`zcode_visibility`] 降级为 TuvisOnly（Task 8/12）
            zcode_visibility(true),
        ),
        "workbuddy" => (
            vec![Channel::Headless(K::WbAcp)],
            Visibility::Realtime, // ACP 写入 APP 内可见（端点即宿主运行时）
        ),
        "codex" if form == ProcessForm::App => (
            vec![
                Channel::Headless(K::CodexQueue),
                Channel::Headless(K::CodexExec),
            ],
            Visibility::Realtime, // APP 原生排队（打开态立即入队、忙态回合后投递）
        ),
        // H11 三家：pid == 0 才入无头（有活进程 = 已开 TUI = 终端通道，W3）
        "claude" if pid == 0 => (
            vec![Channel::Headless(K::ClaudeP)],
            Visibility::AfterRefresh, // 注入实时 / 无头读链路可见（附录 A）
        ),
        "kimi" if pid == 0 => (
            vec![Channel::Headless(K::KimiP)],
            Visibility::AfterRefresh, // 刷新后（附录 A）
        ),
        "opencode" if pid == 0 => (
            vec![Channel::Headless(K::OpencodeRun)],
            Visibility::Realtime, // 官方 web 实时（附录 A）
        ),
        "dsh" if form == ProcessForm::App || pid == 0 => {
            return Some(RouteOutcome::NotInjectable {
                reason_code: "dsh_headless_pending",
                reason: "dsh 无头写通道（H13 · ACP stdio）不在本批，暂不可注入".into(),
            })
        }
        _ => return None,
    };
    Some(RouteOutcome::Injectable {
        candidates,
        visibility,
    })
}

/// 路由决策（W3 纯核 + H 系无头路由）。
///
/// # 输入契约（前置条件，调用方必须满足）
/// - `agent_tool_id`：**小写精确匹配**（对齐 `AgentType` serde lowercase 形态，如
///   `"claude"`/`"workbuddy"`）；大小写不符会静默绕过工具门，调用方必须原样传
///   会话的 agent_type 小写标识
/// - `form`：会话宿主形态（adapter 判定产物）
/// - `pid`：会话 CLI 进程 pid；0 = 进程不在/未读卡。无头通道**自己 spawn 进程**（不看
///   pid）——唯一例外是 H11 三家：`pid == 0`（没有可写的 TUI）正是它们入无头的判据，
///   详见 [`headless_route`]；终端路由以 `pid == 0` 判不可注入
/// - `platform`：`std::env::consts::OS` 原值（`"macos"`/`"windows"`，其他一律不支持）。
///   **只约束终端通道**——无头通道不用终端宿主，其平台/版本支持由各通道适配器在 H6
///   版本门控处判定（Task 8/9/11/13）
///
/// # 判定顺序（勿随意改：每条都有判据依赖）
/// 1. 无头归属（[`headless_route`]）——无头不需要 pid，**必须**先于 pid 早退；
/// 2. `pid == 0` → `no_process`（终端专属家：没有可写的 TUI 就没有注入面）；
/// 3. 黑盒门（[`tool_gate`]：openclaw）；
/// 4. APP 形态 → `app_form`（**残余门**，Task 7 起只管无头归属之外的形态——四家 APP
///    形态工具已在第 1 步被无头通道接走）；
/// 5. 平台 → 终端候选。
pub fn route(agent_tool_id: &str, form: ProcessForm, pid: u32, platform: &str) -> RouteOutcome {
    if let Some(headless) = headless_route(agent_tool_id, form, pid) {
        return headless;
    }
    if pid == 0 {
        return RouteOutcome::NotInjectable {
            reason_code: "no_process",
            reason: "会话进程不存在".into(),
        };
    }
    if let Some((code, reason)) = tool_gate(agent_tool_id) {
        return RouteOutcome::NotInjectable {
            reason_code: code,
            reason,
        };
    }
    if form == ProcessForm::App {
        return RouteOutcome::NotInjectable {
            reason_code: "app_form",
            reason: "APP 形态无外部写 API（computer-use 属应急预案 D7）".into(),
        };
    }
    match platform {
        "macos" => RouteOutcome::Injectable {
            candidates: vec![Channel::Tmux, Channel::Iterm2, Channel::TerminalApp],
            visibility: Visibility::Realtime,
        },
        "windows" => RouteOutcome::Injectable {
            candidates: vec![Channel::WindowsConsole],
            visibility: Visibility::Realtime,
        },
        _ => RouteOutcome::NotInjectable {
            reason_code: "platform",
            reason: "当前平台不支持注入".into(),
        },
    }
}

/// **无头绑定的唯一判据出口**（H3 无头总开关的判定源，Task 7 义务 1）：本路由结论是否由
/// 无头通道驱动——`Some(首个无头候选)`（候选按优先级排序，首位 = 首选通道，如 codex APP =
/// queue → exec）；`None` = 终端通道或不可注入。
///
/// # 为什么必须有本函数（勿再立平行工具/形态表）
/// Task 5 的过渡谓词 `is_headless_bound(tool, form)` 是**第二真相源**，与路由表必然漂移
/// （当时已分歧：workbuddy 判 true 而路由是 `blackbox`；codex APP 判 true 而路由是
/// `app_form`），且**反向缺口更危险**——H11（claude/kimi/opencode 无头）与 H13 的无头面不在
/// 其判定内，H3 总开关管不到它们。Task 7 已删除该谓词：**「这条会话会不会被无头通道驱动」
/// 只有 `route()` 一个答案**，消费方（`remote::api::headless_blocked`，两个门点共用）经本
/// 函数读取。
///
/// **dsh 刻意落在 `None`**：H13（dsh 写侧）不在本批，dsh 需要无头的场合路由判
/// `dsh_headless_pending`（不可注入 → 本函数 `None`）——即 H3 开关今天**不管 dsh，这是范围
/// 裁决而非疏漏**（H13 落地、加 `DshAcp` 变体与路由分支后自然入集）。
pub fn headless_kind_of(outcome: &RouteOutcome) -> Option<HeadlessKind> {
    match outcome {
        RouteOutcome::Injectable { candidates, .. } => candidates.iter().find_map(|c| match c {
            Channel::Headless(kind) => Some(*kind),
            _ => None,
        }),
        RouteOutcome::NotInjectable { .. } => None,
    }
}

/// 路由结论是否含**终端注入候选**（Task 7 端点分派判据，Task 8 无头分派的镜像）：
/// `false` = 本条只能经无头通道投递（终端注入臂不得接单——zcode/workbuddy 的 pid 不是
/// 终端宿主，落终端臂等于打错窗口）。
pub fn has_terminal_candidate(outcome: &RouteOutcome) -> bool {
    match outcome {
        RouteOutcome::Injectable { candidates, .. } => candidates.iter().any(|c| c.is_terminal()),
        RouteOutcome::NotInjectable { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli() -> ProcessForm {
        ProcessForm::Cli
    }
    fn app() -> ProcessForm {
        ProcessForm::App
    }

    /// 计划 Step 1 的钉值测试（Task 7）：**四家 APP 形态入无头通道**（不再一律 `app_form`
    /// 拒绝，裁决 1/12/16）；zcode 唯一候选 + 可见性 = 重启级
    #[test]
    fn app_tools_route_to_headless_channels() {
        // 裁决 1/12/16：四家 APP 形态入无头通道（不再一律 app_form 拒绝）
        let r = route("zcode", app(), 100, "windows");
        assert!(
            matches!(r, RouteOutcome::Injectable { candidates, visibility }
            if candidates == vec![Channel::Headless(HeadlessKind::Zcode)] && visibility == Visibility::AfterRestart)
        );
        let r = route("codex", app(), 100, "windows");
        assert!(matches!(r, RouteOutcome::Injectable { candidates, .. }
            if candidates.contains(&Channel::Headless(HeadlessKind::CodexQueue))));
        let r = route("workbuddy", app(), 100, "windows");
        assert!(matches!(r, RouteOutcome::Injectable { candidates, .. }
            if candidates.contains(&Channel::Headless(HeadlessKind::WbAcp))));
    }

    /// 计划 Step 1 的钉值测试（Task 7）：终端形态不动（W3：无头对已开 TUI 不路由）
    #[test]
    fn cli_forms_keep_terminal_channels() {
        // 终端形态不动（W3：无头对已开 TUI 不路由）
        let r = route("claude", cli(), 100, "macos");
        assert!(matches!(r, RouteOutcome::Injectable { .. }));
    }

    /// `app_form` 门**语义翻转**（原断言：APP 形态一律 `app_form` 拒绝——Task 7 起不成立）：
    /// 四家 APP 形态工具已在无头路由接走，本门只剩**无头归属之外**的残余形态
    /// （未知工具 APP / claude 等「有 TUI 就不是无头」的 App 形态）
    #[test]
    fn app_form_refusal_survives_only_outside_headless_table() {
        // 翻转面：四家 APP 形态工具不再是 app_form 拒绝
        for tool in ["zcode", "workbuddy", "codex"] {
            let r = route(tool, app(), 100, "macos");
            assert!(
                !matches!(
                    r,
                    RouteOutcome::NotInjectable {
                        reason_code: "app_form",
                        ..
                    }
                ),
                "{tool} APP 形态已入无头通道，不得再落 app_form：{r:?}"
            );
            assert!(
                headless_kind_of(&r).is_some(),
                "{tool} APP 形态应判无头绑定：{r:?}"
            );
        }
        // 残余面（本门仍在：有 TUI 可写的家 + 未知工具的 APP 形态）
        for tool in ["claude", "antigravity"] {
            assert!(
                matches!(
                    route(tool, app(), 100, "macos"),
                    RouteOutcome::NotInjectable {
                        reason_code: "app_form",
                        ..
                    }
                ),
                "{tool} APP 形态（无无头归属）仍应落 app_form"
            );
        }
    }

    /// 工具门（Task 7 重写后）：**`blackbox` 只剩 openclaw**（真的没有外部写 API 的家）；
    /// workbuddy / zcode 已入无头通道；dsh 需要无头的场合如实报 `dsh_headless_pending`
    #[test]
    fn tool_gates_after_task7() {
        // openclaw：黑盒（gateway 另评），两种形态 + 活进程都判黑盒
        for form in [cli(), app()] {
            assert!(matches!(
                route("openclaw", form, 100, "macos"),
                RouteOutcome::NotInjectable {
                    reason_code: "blackbox",
                    ..
                }
            ));
        }
        // zcode / workbuddy：不再是 blackbox / headless_only——已入无头路由
        for tool in ["zcode", "workbuddy"] {
            for form in [cli(), app()] {
                let r = route(tool, form, 100, "macos");
                assert!(
                    headless_kind_of(&r).is_some(),
                    "{tool} 应判无头绑定（不再是黑盒/无头限定码）：{r:?}"
                );
            }
        }
        // dsh：CLI 形态 + 活进程 = 在产终端注入保留；需要无头的两种场合 → dsh_headless_pending
        assert!(
            has_terminal_candidate(&route("dsh", cli(), 100, "macos")),
            "dsh CLI 形态 + 活进程必须保留终端注入（在产能力不动）"
        );
        for (form, pid) in [(app(), 100), (cli(), 0)] {
            let r = route("dsh", form, pid, "macos");
            assert!(
                matches!(r, RouteOutcome::NotInjectable { reason_code, .. } if reason_code == "dsh_headless_pending"),
                "dsh 需要无头的场合（APP 形态 / 进程不在）必须如实报 dsh_headless_pending：{r:?}"
            );
            assert!(
                headless_kind_of(&r).is_none(),
                "H13 不在本批：dsh 刻意不进无头绑定集（H3 开关今天不管它——范围裁决）"
            );
        }
    }

    /// `pid == 0` → `no_process`（**终端专属家**）：Task 7 的重排序只把无头归属的家提前，
    /// 没有无头面的家（未知工具 / openclaw）照旧判无进程
    #[test]
    fn dead_process_not_injectable() {
        for tool in ["antigravity", "openclaw"] {
            assert!(
                matches!(
                    route(tool, cli(), 0, "macos"),
                    RouteOutcome::NotInjectable {
                        reason_code: "no_process",
                        ..
                    }
                ),
                "{tool} 无进程 = 无终端可写、且无无头通道 → no_process"
            );
        }
        // **codex CLI + pid == 0 也在本格**（Minor 1 钉值）：`CodexExec` 的触发条件是
        // 「APP 不在场」而非「进程不在」（路由核看不到 APP 在场性，判定归 Task 9），
        // 故这一格**没有无头兜底**——旧行为（no_process）原样保留，别误以为 APP 形态的
        // 无头分派会顺带接走它
        assert!(
            matches!(
                route("codex", cli(), 0, "macos"),
                RouteOutcome::NotInjectable {
                    reason_code: "no_process",
                    ..
                }
            ),
            "codex CLI + pid=0 必须仍是 no_process（exec 改道判据是 APP 在场性，不是进程在场）"
        );
        assert!(
            headless_kind_of(&route("codex", cli(), 0, "macos")).is_none(),
            "codex CLI + pid=0 不进无头绑定集（H3 开关不管它）"
        );
    }

    /// **义务 2 的两向覆盖**：H11 三家（claude/kimi/opencode）与 APP 形态三家——无头路由
    /// **不需要活进程**（`pid == 0` 正是主场景）；有活进程的 H11 家保持终端通道（W3）
    #[test]
    fn headless_routes_do_not_need_a_live_process() {
        // pid == 0 → 无头通道（不看形态：未读卡 / DB-only 哨兵卡都会落这里）
        for form in [cli(), app()] {
            assert_eq!(
                headless_kind_of(&route("zcode", form, 0, "windows")),
                Some(HeadlessKind::Zcode)
            );
            assert_eq!(
                headless_kind_of(&route("workbuddy", form, 0, "windows")),
                Some(HeadlessKind::WbAcp)
            );
        }
        assert_eq!(
            headless_kind_of(&route("codex", app(), 0, "windows")),
            Some(HeadlessKind::CodexQueue)
        );
        // H11 三家：pid == 0 → 各自的无头通道（spec:169-173 全部走 H3–H6 底座）
        assert_eq!(
            headless_kind_of(&route("claude", cli(), 0, "windows")),
            Some(HeadlessKind::ClaudeP)
        );
        assert_eq!(
            headless_kind_of(&route("kimi", cli(), 0, "windows")),
            Some(HeadlessKind::KimiP)
        );
        assert_eq!(
            headless_kind_of(&route("opencode", cli(), 0, "windows")),
            Some(HeadlessKind::OpencodeRun)
        );
        // 活进程 → 终端通道（已开 TUI：无头会分叉，W3 不路由）
        for tool in ["claude", "kimi", "opencode"] {
            let r = route(tool, cli(), 100, "windows");
            assert!(
                headless_kind_of(&r).is_none() && has_terminal_candidate(&r),
                "{tool} 有活进程必须落终端通道：{r:?}"
            );
        }
        // codex CLI 形态同理（H8：CLI 托管保持在产终端注入）
        let r = route("codex", cli(), 100, "windows");
        assert!(
            headless_kind_of(&r).is_none() && has_terminal_candidate(&r),
            "codex CLI 形态必须落终端通道：{r:?}"
        );
    }

    /// **义务 1 的判据平移**（原 `headless_bound_covers_app_tools_only` 断言 `is_headless_bound`
    /// 临时谓词）：无头绑定**一律从路由结论派生**（`headless_kind_of`）——zcode/workbuddy
    /// 工具级（不看形态）、codex 按形态、H11 三家按进程在场、dsh/openclaw 恒非无头
    #[test]
    fn headless_binding_derives_from_route() {
        for tool in ["zcode", "workbuddy"] {
            for form in [cli(), app()] {
                assert!(
                    headless_kind_of(&route(tool, form, 100, "macos")).is_some(),
                    "{tool} 工具级走无头（不看形态）"
                );
            }
        }
        assert_eq!(
            headless_kind_of(&route("codex", app(), 100, "macos")),
            Some(HeadlessKind::CodexQueue),
            "codex APP 形态首选 queue（exec 为改道候选，见候选顺序断言）"
        );
        assert!(
            headless_kind_of(&route("codex", cli(), 100, "macos")).is_none(),
            "codex CLI 形态保持在产终端注入（W3）"
        );
        for tool in ["dsh", "openclaw"] {
            assert!(
                headless_kind_of(&route(tool, cli(), 100, "macos")).is_none(),
                "{tool} 不进无头绑定集（dsh：H13 不在本批——范围裁决）"
            );
        }
    }

    /// codex APP 候选顺序 = **优先级**（queue 首选 → exec 改道）：`headless_kind_of` 取
    /// 首位，即「首选通道」语义
    #[test]
    fn codex_app_candidates_are_priority_ordered() {
        let RouteOutcome::Injectable { candidates, .. } = route("codex", app(), 100, "macos")
        else {
            panic!("codex APP 形态应可注入（无头 queue）")
        };
        assert_eq!(
            candidates,
            vec![
                Channel::Headless(HeadlessKind::CodexQueue),
                Channel::Headless(HeadlessKind::CodexExec),
            ],
            "H8：APP 在场 = queue 首选；APP 不在场 = exec resume 改道（Task 9 按在场性选）"
        );
    }

    /// 无头通道**不经理赔终端平台矩阵**（它不是终端宿主）：终端三家在 linux 落 `platform`
    /// 拒绝，无头路由照判——各通道的平台/版本支持归适配器的 H6 版本门控（Task 8/9/11/13）
    #[test]
    fn headless_routes_ignore_terminal_platform_matrix() {
        assert!(matches!(
            route("claude", cli(), 42, "linux"),
            RouteOutcome::NotInjectable {
                reason_code: "platform",
                ..
            }
        ));
        assert_eq!(
            headless_kind_of(&route("zcode", app(), 0, "linux")),
            Some(HeadlessKind::Zcode),
            "无头路由与终端宿主矩阵无关（平台支持由通道适配器判）"
        );
    }

    /// zcode 可见性两档（H7/H10 两端定案）+ 文案元数据：`route` 取已信任默认档，未信任经
    /// [`zcode_visibility`] 降级——变体与逐字文案单点
    #[test]
    fn zcode_visibility_tiers_and_notes() {
        assert_eq!(zcode_visibility(true), Visibility::AfterRestart);
        assert_eq!(zcode_visibility(false), Visibility::TuvisOnly);
        assert_eq!(
            Visibility::AfterRestart.note(),
            "已信任工作区：重启 ZCode 应用后可见"
        );
        assert_eq!(Visibility::TuvisOnly.note(), "未信任工作区：仅兔维斯可见");
        // route 的默认档（信任探针不是纯路由核的输入）
        assert!(matches!(
            route("zcode", app(), 100, "windows"),
            RouteOutcome::Injectable {
                visibility: Visibility::AfterRestart,
                ..
            }
        ));
    }

    /// 无头通道 wire/审计名钉值（审计 channel 列 + send-info channels 数组同源）：
    /// 词表漂移会让审计页与移动端对不上
    #[test]
    fn headless_wire_names_are_pinned() {
        for (kind, name) in [
            (HeadlessKind::Zcode, "headless_zcode"),
            (HeadlessKind::CodexQueue, "headless_codex_queue"),
            (HeadlessKind::CodexExec, "headless_codex_exec"),
            (HeadlessKind::WbAcp, "headless_wb_acp"),
            (HeadlessKind::ClaudeP, "headless_claude_p"),
            (HeadlessKind::KimiP, "headless_kimi_p"),
            (HeadlessKind::OpencodeRun, "headless_opencode_run"),
        ] {
            assert_eq!(kind.wire_name(), name);
        }
        // 终端通道判定（端点分派判据）：终端候选 true / 纯无头候选 false
        assert!(Channel::WindowsConsole.is_terminal());
        assert!(!Channel::Headless(HeadlessKind::Zcode).is_terminal());
        assert!(has_terminal_candidate(&route(
            "claude",
            cli(),
            100,
            "macos"
        )));
        assert!(!has_terminal_candidate(&route(
            "zcode",
            app(),
            100,
            "macos"
        )));
        assert!(!has_terminal_candidate(&route("dsh", app(), 100, "macos")));
    }

    /// macOS 可注入家（claude/codex/opencode/kimi CLI）：三通道候选按由简到繁（裁决 14）
    #[test]
    fn macos_candidates_ordered() {
        let r = route("kimi", cli(), 42, "macos");
        let RouteOutcome::Injectable {
            candidates,
            visibility,
        } = r
        else {
            panic!()
        };
        assert_eq!(
            candidates,
            vec![Channel::Tmux, Channel::Iterm2, Channel::TerminalApp]
        );
        assert!(matches!(visibility, Visibility::Realtime));
    }

    /// Windows：单通道候选（宿主可达性由 M6 结论与引擎层处理）
    #[test]
    fn windows_single_channel() {
        assert!(matches!(route("claude", cli(), 42, "windows"),
            RouteOutcome::Injectable { candidates, .. } if candidates == vec![Channel::WindowsConsole]));
    }

    /// 其他平台不支持（终端通道）
    #[test]
    fn linux_not_supported() {
        assert!(matches!(
            route("claude", cli(), 42, "linux"),
            RouteOutcome::NotInjectable {
                reason_code: "platform",
                ..
            }
        ));
    }
}
