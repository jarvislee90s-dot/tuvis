//! 注入路由表（W3，纯核）：会话属性 → 通道决策 + 可见性预期。路由按宿主形态与
//! 工具门判定，**工具无关的终端宿主一律优先终端注入**（无头对已开 TUI 会分叉，
//! spec 裁决「路由规则按宿主形态，工具无关」）。M11 无头落地后此处仅追加分支。

use crate::session::model::ProcessForm;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Tmux,
    Iterm2,
    TerminalApp,
    WindowsConsole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Realtime,
    AfterRefresh,
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
/// ZCode 固定走无头（D6 在册限定，M11）
fn tool_gate(tool: &str) -> Option<(&'static str, String)> {
    match tool {
        "workbuddy" => Some(("blackbox", "WorkBuddy 黑盒，无外部写 API".into())),
        "dsh" => Some(("blackbox", "dsh 写通道另评（web API/插件生态）".into())),
        "openclaw" => Some(("blackbox", "OpenClaw 走 gateway，另评".into())),
        "zcode" => Some(("headless_only", "ZCode 走无头通道（M11）".into())),
        _ => None,
    }
}

/// 路由决策（W3 纯核）。
///
/// # 输入契约（前置条件，调用方必须满足）
/// - `agent_tool_id`：**小写精确匹配**（对齐 `AgentType` serde lowercase 形态，如
///   `"claude"`/`"workbuddy"`）；大小写不符会静默绕过工具门，调用方必须原样传
///   会话的 agent_type 小写标识
/// - `form`：会话宿主形态（adapter 判定产物）
/// - `pid`：会话 CLI 进程 pid；0 = 进程不在/未读卡
/// - `platform`：`std::env::consts::OS` 原值（`"macos"`/`"windows"`，其他一律不支持）
pub fn route(agent_tool_id: &str, form: ProcessForm, pid: u32, platform: &str) -> RouteOutcome {
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

/// **无头绑定判定（H3 无头总开关的判定源）**：该工具/形态组合**是否会被无头通道
/// 驱动**——决定「无头通道总开关」关着时要不要拦（spec H3：无头默认关，显式开启；
/// 门在前）。
///
/// # ⚠️ 临时谓词（provisional）——Task 7 的**义务清单**（必须逐条兑现）
///
/// 真正的无头路由（`Channel::Headless(HeadlessKind)`）与无头 runner 都尚未存在
/// （Task 6/7 交付），本函数是过渡期的最小近似，**它与路由表并不一致**——已知分歧：
/// workbuddy / dsh 在 `tool_gate` 是 `blackbox`（本函数对 workbuddy 判 true）、
/// codex APP 形态是 `app_form`（本函数判 true）。这些分歧方向上是「本函数先拦」，
/// 今日无活体危害；但**反向缺口是真实的**：H11（claude / kimi / opencode 无头，
/// spec:169-173「全部走 H3–H6 底座」）与 H13（dsh 无头）今天**不在本判定内**，无头
/// 总开关因此管不到它们的无头面。故 Task 7 必须：
///
/// 1. **判定一律从路由结论派生**（`route()` 判出 `Channel::Headless` 即无头绑定），
///    不要维护本函数这样的**平行工具/形态表**——两处判据必然漂移（今天就已分歧）；
/// 2. **补齐 H11（claude / kimi / opencode）与 H13（dsh）的 MAM 无头会话**：这三家
///    **同时存在 TUI 会话**，判据不能是 `ProcessForm`，必须是「本会话是否由 MAM 经
///    无头通道驱动」（路由/会话来源给出的结论），否则总开关漏管其无头面；
/// 3. **重核 workbuddy / dsh 的归属**：`blackbox`（本函数判 true）与路由表落地后的
///    实际分派必须一致，不一致处按「实际会不会走无头通道」定夺；
/// 4. **两个门点同步**（`remote::api` 的 session-send 403 与 session-send-info 置灰
///    共用 `headless_blocked`）：开关关闭时仍拒绝无头绑定会话，终端注入四家仍完全
///    不经此门；
/// 5. **裁决 flush 循环问题**：开关关闭时，**已在队列中的条目仍会被 flush 循环投递**
///    （`inject/queue.rs` 的 `flush_one` 无 H3 门）——今日无头路由尚不存在故无活体
///    危害；Task 7 须决定是否在投递侧补门（若补，判据同样从路由结论派生）。
///
/// 本函数若被删除，其 `headless_bound_covers_app_tools_only` 断言应平移为路由表断言。
///
/// 口径（与 Task 7 计划的路由表同向）：
/// - `zcode`（H7 在册注入 / H10 无头新建）：工具级走无头——**不看形态**（ZCode 恒为
///   APP 内嵌 CLI，无真实 CLI 宿主形态；`route` 的 zcode 工具门亦在形态判定之前）
/// - `workbuddy`（H9 路线 A · ACP）：同上（恒 APP 形态，无终端注入面）
/// - `codex`（H8）：**按形态分派**——APP 托管走 queue 无头通道；CLI 形态保持在产终端
///   注入（W3：无头对已开 TUI 不路由）
/// - 其余（claude / kimi / opencode 终端四家、dsh、openclaw）：恒 false——**过渡期
///   口径**：今日只覆盖已具备无头通道归属的工具；H11/H13 的无头面由义务清单 2 补
pub fn is_headless_bound(tool: &str, form: ProcessForm) -> bool {
    match tool {
        "zcode" | "workbuddy" => true,
        "codex" => form == ProcessForm::App,
        _ => false,
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

    /// APP 形态不可注入（WorkBuddy/Codex APP 等——computer-use 唯一通路属 D7 应急级）
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

    /// WorkBuddy 黑盒 / dsh 另评 / ZCode 走无头（M11）/ OpenClaw gateway 另评
    #[test]
    fn tool_gates() {
        for (tool, code) in [
            ("workbuddy", "blackbox"),
            ("dsh", "blackbox"),
            ("zcode", "headless_only"),
            ("openclaw", "blackbox"),
        ] {
            let r = route(tool, cli(), 100, "macos");
            assert!(
                matches!(r, RouteOutcome::NotInjectable { reason_code, .. } if reason_code == code),
                "{tool}"
            );
        }
    }

    /// pid=0（未读卡兜底/进程不在）不可注入
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

    /// 其他平台不支持
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

    /// H3 门判定（**临时谓词**，Task 7 换真路由后语义须不变）：
    /// - zcode（H7/H10）与 workbuddy（H9）工具级走无头通道——两种形态都判无头绑定
    ///   （二者无真实 CLI 宿主形态：zcode 恒 APP 内嵌 CLI、workbuddy 恒 APP）；
    /// - codex 按形态分派（H8）：APP 托管走 queue 无头通道，CLI 形态保持在产终端注入；
    /// - 终端注入四家 + dsh/openclaw 恒 false（spec H3 边界：不经无头总开关）
    #[test]
    fn headless_bound_covers_app_tools_only() {
        for tool in ["zcode", "workbuddy"] {
            assert!(is_headless_bound(tool, cli()), "{tool} CLI 形态判无头绑定");
            assert!(is_headless_bound(tool, app()), "{tool} APP 形态判无头绑定");
        }
        assert!(
            is_headless_bound("codex", app()),
            "codex APP 形态走无头（H8）"
        );
        assert!(
            !is_headless_bound("codex", cli()),
            "codex CLI 形态保持在产终端注入（W3：无头对已开 TUI 不路由）"
        );
        for tool in ["claude", "kimi", "opencode", "dsh", "openclaw"] {
            assert!(!is_headless_bound(tool, cli()), "{tool} 不经无头总开关");
            assert!(!is_headless_bound(tool, app()), "{tool} 不经无头总开关");
        }
    }
}
