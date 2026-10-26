//! 注入路由表（W3 纯核 + Codex 兼容无头路由）：会话属性 → 通道决策 + 可见性预期。
//! 路由按宿主形态与工具门判定，**工具无关的终端宿主一律优先终端注入**（无头对已开 TUI
//! 会分叉，spec 裁决「路由规则按宿主形态，工具无关」）。
//!
//! ## Codex 兼容无头路由（本轮范围）
//! 只有 Codex APP 形态入无头族：APP 托管会话的普通消息走 `queue`（首选，APP 原生排队
//! 可见），Desktop 不在场时改道 `exec resume`（兼容兜底）。**CLI 形态保持在产终端注入**
//! （W3：无头对已开 TUI 不路由）；`pid == 0` 的 CLI 会话仍判 `no_process`。
//!
//! 三条纪律：
//! 1. **无头不需要 pid**：子进程由 Tuvis 自己 spawn，故无头归属判定先于 `pid == 0`
//!    早退——「会话在册但无存活进程」正是无头主场景；
//! 2. **判定只有一个出口**：`route()` 的结论即真相，H3 门与端点分派经
//!    [`headless_kind_of`] 读取，不得另立工具/形态表；
//! 3. 其他工具（workbuddy / dsh / claude-kimi-opencode 无头、codex app-server）
//!    的变体留在 PR #117 续作，不在此注册；**ZCode 不注册任何注入通道**——
//!    只走官方控制接口（工具门 = `official_control_required`）。

use crate::session::model::ProcessForm;

/// Codex 兼容无头通道族（H8）：只说「这条会话归哪个兼容通道驱动」，通道本体
/// （argv 构造 / 子进程生命周期 / 回执解析）在 `inject::headless::codex`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessKind {
    /// H8：`codex queue --thread <UUID> --message …`（Desktop 在场首选；回执只证明
    /// 入队不证明投递）
    CodexQueue,
    /// H8 改道：`codex -C <项目> exec resume <UUID> …`（Desktop 不在场；`-C` 是全局
    /// flag 必须前置）
    CodexExec,
}

impl HeadlessKind {
    /// 通道 wire/审计名（`session-send-info` 的 channels 数组与审计 channel 列**同源**）。
    pub const fn wire_name(self) -> &'static str {
        match self {
            HeadlessKind::CodexQueue => "headless_codex_queue",
            HeadlessKind::CodexExec => "headless_codex_exec",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Tmux,
    Iterm2,
    TerminalApp,
    WindowsConsole,
    /// 无头通道：进程由 Tuvis spawn，**不依赖终端宿主**（平台/版本支持由通道适配器的
    /// H6 版本门控判定）
    Headless(HeadlessKind),
}

impl Channel {
    /// 终端注入通道判定（无头候选一律不算）——端点分派与展示/审计共用的单一判据
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Channel::Headless(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Realtime,
    AfterRefresh,
}

impl Visibility {
    /// 可见性提示文案（**后端给文案、前端只渲染**——与 H3 的 `HEADLESS_DISABLED_REASON`
    /// 同款单一措辞出口）。词表取自 spec 附录 A「有头可见性」列。
    pub const fn note(self) -> &'static str {
        match self {
            Visibility::Realtime => "实时",
            Visibility::AfterRefresh => "刷新后可见",
        }
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

/// 黑盒/另评工具（宪法附 A 矩阵）：WorkBuddy ❌、dsh ◐ 另评、OpenClaw ◐ gateway、
/// ZCode 改走官方控制接口（永不注入终端/CLI，M11 已退役）。**Codex 不在此表**——
/// 它按形态分派。
fn tool_gate(tool: &str) -> Option<(&'static str, String)> {
    match tool {
        "workbuddy" => Some(("blackbox", "WorkBuddy 黑盒，无外部写 API".into())),
        "dsh" => Some(("blackbox", "dsh 写通道另评（web API/插件生态）".into())),
        "openclaw" => Some(("blackbox", "OpenClaw 走 gateway，另评".into())),
        // 官方配对远控是唯一写通道：不注册 HeadlessKind、不落终端注入
        "zcode" => Some((
            "official_control_required",
            "ZCode 需使用配对后的官方控制接口（不支持终端/无头注入）".into(),
        )),
        _ => None,
    }
}

/// **Codex 兼容无头路由**：`Some` = 本工具/形态的注入面由无头通道决定；`None` = 落
/// 终端分支（[`route`] 继续往下判）。
///
/// **为什么排在 `pid == 0` 早退之前**：无头通道自己 spawn 进程，不看 pid；「会话在册但
/// 无存活进程」正是 APP 托管会话的常见形态。
///
/// **codex 按形态分派**（H8）：APP 托管 → `queue`（首选）+ `exec resume`（Desktop 不在
/// 场的改道候选，二者按此优先级排列）；CLI 形态保持在产终端注入。Desktop 是否在场不在
/// 本纯核的输入里——该判定归端点/适配器（`codex::production_presence`）。
fn headless_route(tool: &str, form: ProcessForm, pid: u32) -> Option<RouteOutcome> {
    use HeadlessKind as K;
    let (candidates, visibility) = match tool {
        "codex" if form == ProcessForm::App => (
            vec![
                Channel::Headless(K::CodexQueue),
                Channel::Headless(K::CodexExec),
            ],
            Visibility::Realtime, // APP 原生排队（打开态立即入队、忙态回合后投递）
        ),
        // CLI 形态 + pid == 0 仍是 no_process（CodexExec 的触发条件是「Desktop 不在场」，
        // 不是「会话进程不在」；路由核看不到 Desktop 在场性）
        _ => return None,
    };
    let _ = pid;
    Some(RouteOutcome::Injectable {
        candidates,
        visibility,
    })
}

/// 路由决策（W3 纯核 + Codex 兼容无头路由）。
///
/// # 输入契约（前置条件，调用方必须满足）
/// - `agent_tool_id`：**小写精确匹配**（对齐 `AgentType` serde lowercase 形态，如
///   `"claude"`/`"workbuddy"`）；大小写不符会静默绕过工具门，调用方必须原样传
///   会话的 agent_type 小写标识
/// - `form`：会话宿主形态（adapter 判定产物）
/// - `pid`：会话 CLI 进程 pid；0 = 进程不在/未读卡。无头通道自己 spawn 进程（不看 pid）；
///   终端路由以 `pid == 0` 判不可注入
/// - `platform`：`std::env::consts::OS` 原值（`"macos"`/`"windows"`，其他一律不支持）。
///   **只约束终端通道**——无头通道不用终端宿主，其平台/版本支持由适配器在 H6 门控处判定
///
/// # 判定顺序（勿随意改：每条都有判据依赖）
/// 1. Codex 无头归属（[`headless_route`]）——无头不需要 pid，**必须**先于 pid 早退；
/// 2. `pid == 0` → `no_process`；
/// 3. 黑盒门（[`tool_gate`]）；
/// 4. APP 形态 → `app_form`（**残余门**，只管无头归属之外的形态）；
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

/// **无头绑定的唯一判据出口**（H3 无头总开关的判定源）：本路由结论是否由无头通道驱动
/// ——`Some(首个无头候选)`（候选按优先级排序，首位 = 首选通道，codex APP = queue → exec）；
/// `None` = 终端通道或不可注入。
pub fn headless_kind_of(outcome: &RouteOutcome) -> Option<HeadlessKind> {
    match outcome {
        RouteOutcome::Injectable { candidates, .. } => candidates.iter().find_map(|c| match c {
            Channel::Headless(k) => Some(*k),
            _ => None,
        }),
        RouteOutcome::NotInjectable { .. } => None,
    }
}

/// 是否存在终端候选（端点分派：无头候选**且无终端候选** = 只能经无头通道投递）
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

    /// APP 形态不可注入（WorkBuddy/Codex CLI 之外的 APP 等——computer-use 唯一通路
    /// 属 D7 应急级）；**Codex APP 已由无头通道接走**，见下方 codex 用例
    #[test]
    fn app_form_not_injectable() {
        let r = route("claude", app(), 100, "macos");
        assert!(matches!(
            r,
            RouteOutcome::NotInjectable {
                reason_code: "app_form",
                ..
            }
        ));
    }

    /// WorkBuddy 黑盒 / dsh 另评 / ZCode 走官方控制接口 / OpenClaw gateway 另评
    #[test]
    fn tool_gates() {
        for (tool, code) in [
            ("workbuddy", "blackbox"),
            ("dsh", "blackbox"),
            ("zcode", "official_control_required"),
            ("openclaw", "blackbox"),
        ] {
            let r = route(tool, cli(), 100, "macos");
            assert!(
                matches!(r, RouteOutcome::NotInjectable { reason_code, .. } if reason_code == code),
                "{tool}"
            );
        }
    }

    /// **ZCode 永不回退终端/CLI 注入**（M11 已退役）：任何形态/平台都判不可注入，
    /// 原因码指向官方控制接口；绝不注册 ZCode HeadlessKind，也没有终端候选。
    #[test]
    fn zcode_never_falls_back_to_headless() {
        for (form, pid, platform) in [
            (cli(), 42, "macos"),
            (cli(), 42, "windows"),
            (app(), 42, "macos"),
        ] {
            let r = route("zcode", form, pid, platform);
            let RouteOutcome::NotInjectable {
                reason_code,
                reason,
            } = r.clone()
            else {
                panic!("ZCode 必须不可注入（无终端/CLI 回退）: {r:?}");
            };
            assert_eq!(
                reason_code, "official_control_required",
                "ZCode 原因码指向官方控制接口"
            );
            assert!(!reason_code.contains("headless_only"));
            assert!(!reason.contains("M11"), "M11 已退役：{reason}");
            assert!(!reason.contains("headless_only"), "不得再提无头通道");
            // NotInjectable 无语义候选；终端候选恒空 = 不存在回退通道
            assert!(!has_terminal_candidate(&r), "ZCode 不得有终端候选");
            assert!(
                headless_kind_of(&r).is_none(),
                "不得为 ZCode 注册 HeadlessKind"
            );
        }
        // pid=0 仍先判 no_process（不可注入、无回退），不构成可注入出口
        let dead = route("zcode", cli(), 0, "macos");
        assert!(matches!(dead, RouteOutcome::NotInjectable { .. }));
        assert!(headless_kind_of(&dead).is_none());
        assert!(!has_terminal_candidate(&dead));
    }

    /// pid=0（未读卡兜底/进程不在）不可注入——**终端家**；Codex APP 例外见下
    #[test]
    fn dead_process_not_injectable() {
        assert!(matches!(
            route("claude", cli(), 0, "macos"),
            RouteOutcome::NotInjectable {
                reason_code: "no_process",
                ..
            }
        ));
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

    /// **Codex APP → 无头族**（H8）：queue 首选、exec 改道候选；不因 pid=0 早退
    /// （无头不需要会话进程在场——它自己 spawn）
    #[test]
    fn codex_app_routes_to_headless_queue_then_exec() {
        let r = route("codex", app(), 0, "windows");
        let RouteOutcome::Injectable {
            candidates,
            visibility,
        } = r.clone()
        else {
            panic!("Codex APP 必须路由到无头族: {r:?}")
        };
        assert_eq!(
            candidates,
            vec![
                Channel::Headless(HeadlessKind::CodexQueue),
                Channel::Headless(HeadlessKind::CodexExec),
            ],
            "queue 首选、exec 改道候选（顺序即优先级）"
        );
        assert!(matches!(visibility, Visibility::Realtime));
        assert_eq!(
            headless_kind_of(&r),
            Some(HeadlessKind::CodexQueue),
            "H3 判定源取首选通道"
        );
        assert!(
            !has_terminal_candidate(&r),
            "Codex APP 没有终端候选（只能经无头通道投递）"
        );
        assert_eq!(HeadlessKind::CodexQueue.wire_name(), "headless_codex_queue");
        assert_eq!(HeadlessKind::CodexExec.wire_name(), "headless_codex_exec");
    }

    /// **Codex CLI 形态保持终端注入**（W3：无头对已开 TUI 不路由）
    #[test]
    fn codex_cli_stays_on_terminal_channels() {
        let r = route("codex", cli(), 42, "windows");
        assert!(matches!(
            &r,
            RouteOutcome::Injectable { candidates, .. }
                if *candidates == vec![Channel::WindowsConsole]
        ));
        assert_eq!(headless_kind_of(&r), None);
        assert!(has_terminal_candidate(&r));
        // CLI + pid=0 仍判 no_process（无头触发条件是 Desktop 缺席，不是会话进程缺席）
        assert!(matches!(
            route("codex", cli(), 0, "windows"),
            RouteOutcome::NotInjectable {
                reason_code: "no_process",
                ..
            }
        ));
    }
}
