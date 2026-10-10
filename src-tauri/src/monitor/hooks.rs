// Hook 系统 — 事件注册 + 共享脚本 + 事件文件读取
// Claude Code: settings.json (PascalCase) / Codex CLI: hooks.json (PascalCase，
// F3 修正——0.155.x 解析要求 PascalCase 键) / Kimi Code: config.toml `[[hooks]]`
// (T2 接入，PascalCase 事件名 + command 直启 helper)

use log::{debug, info, warn};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::monitor::hook_listener;

/// Hook 脚本内容（从 stdin 读 JSON，写入事件文件）
/// **T8 处置申报**：bash 兜底脚本**不承载问答通道**——grep/sed 提取嵌套 tool_input
/// 不可靠（引号/转义多层嵌套无解析保证），按 kimi「helper 缺席跳过」同款口径：helper
/// 在场（claude 直启 / codex commandWindows / kimi 直启）才有问答卡；bash 兜底仅写
/// 事件名等基础字段（问答识别回落通道 B=端点扫描会话消息，见 remote/api.rs 端点注释）
const HOOK_SCRIPT: &str = r#"#!/bin/bash
# MultiAgents Manager 状态 Hook 脚本
# 从 stdin 读取 JSON，写入 ~/.tuvis/events/<session_id>.json
EVENTS_DIR="$HOME/.tuvis/events"
mkdir -p "$EVENTS_DIR"
INPUT=$(cat)
EVENT=$(echo "$INPUT" | grep -o '"hook_event_name"[[:space:]]*:[[:space:]]*"[^"]*"' | sed 's/.*"\([^"]*\)"$/\1/')
SESSION_ID=$(echo "$INPUT" | grep -o '"session_id"[[:space:]]*:[[:space:]]*"[^"]*"' | sed 's/.*"\([^"]*\)"$/\1/')
CWD=$(echo "$INPUT" | grep -o '"cwd"[[:space:]]*:[[:space:]]*"[^"]*"' | sed 's/.*"\([^"]*\)"$/\1/')
TS=$(date +%s)
LAST_EVENT_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
# 事件以 session_id 为键（$PPID 在 claude 脱管 hook 进程里恒为 1，多会话互覆——
# 2026-09-12 第三轮探测 C2 实证；session_id 来自 stdin）。字符白名单外的值
# 直接丢弃（防路径注入；`.`/`/`/`\` 仍拒绝，无穿越面）。下划线为 kimi 形态
# （`session_<uuid>`，F8 实机取证）——helper 侧 session_id_allowed 同口径
# 同会话覆盖=保留最新状态
if printf '%s' "$SESSION_ID" | grep -qE '^[A-Za-z0-9_-]+$'; then
  echo "{\"event\":\"$EVENT\",\"session_id\":\"$SESSION_ID\",\"cwd\":\"$CWD\",\"ts\":$TS,\"last_event_at\":\"$LAST_EVENT_AT\"}" > "$EVENTS_DIR/$SESSION_ID.json"
fi
"#;

/// 确保 Hook 脚本和事件目录存在
pub fn ensure_hook_script() -> PathBuf {
    let mam_dir = dirs::home_dir().unwrap_or_default().join(".tuvis");
    let hooks_dir = mam_dir.join("hooks");
    // 事件目录与 helper 写侧同源（hook_listener::default_events_dir，TUVIS_HOME
    // debug 重定向同 connection.rs 先例）。bash 版脚本内部仍硬编码 $HOME——
    // Windows/正式链路已由原生 helper 承载（批次甲 T1），unix dev 重定向场景属
    // 已知边界（脚本目录 hooks/ 保持真实家目录，避免 unix 存量注册路径漂移）
    let events_dir = hook_listener::default_events_dir();
    let _ = fs::create_dir_all(&hooks_dir);
    let _ = fs::create_dir_all(&events_dir);

    let script_path = hooks_dir.join("status-hook.sh");
    // 无条件重写：脚本由应用托管，幂等重写保证升级后新脚本内容生效
    let _ = fs::write(&script_path, HOOK_SCRIPT);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = fs::metadata(&script_path) {
            let mut perms = metadata.permissions();
            perms.set_mode(0o755);
            let _ = fs::set_permissions(&script_path, perms);
        }
    }
    // marker helper 安装（issue #43）：把与主程序同目录的 tuvis-marker 拷到 ~/.tuvis/bin/
    // 供 兔维斯 主进程跳转按需注入调用（window/win32.rs::inject_marker_on_demand）。
    // helper 未构建/未随包分发是合法状态——主进程检测不到即跳过注入，
    // 跳转链完整回落既有消歧层（零回归）。无条件覆盖保证升级后新版 helper 生效
    install_marker_helper();
    // hook 事件 helper 安装（批次甲 T1）：同一分发管道落盘 tuvis-hook-listener，
    // 供 hook 配置直启（零 shell 依赖，替代 bash 脚本）。缺失同样是合法状态——
    // 注册侧检测不到即回落 bash 形态命令（零回归）
    install_hook_listener_helper();
    script_path
}

/// helper 安装目标目录解析（纯函数）：home → `~/.tuvis/bin`
fn helper_install_dir(home: &std::path::Path) -> PathBuf {
    home.join(".tuvis").join("bin")
}

/// 拷贝 tuvis-marker helper 到 ~/.tuvis/bin/（存在才拷；返回目标路径）
fn install_marker_helper() -> Option<PathBuf> {
    install_helper_bin(&["tuvis-marker.exe", "tuvis-marker"])
}

/// 拷贝 tuvis-hook-listener helper 到 ~/.tuvis/bin/（批次甲 T1；返回目标路径）。
/// 未构建/未随包分发 → None（注册回落 bash 形态，零回归）
pub(crate) fn install_hook_listener_helper() -> Option<PathBuf> {
    install_helper_bin(&["tuvis-hook-listener.exe", "tuvis-hook-listener"])
}

/// helper 分发入口（marker / hook-listener 共用核心）：从当前 exe 同目录拷候选名
/// 到 `~/.tuvis/bin/`（无条件覆盖，升级后新版 helper 生效）
fn install_helper_bin(names: &[&str]) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let src_dir = exe.parent()?.to_path_buf();
    let bin_dir = helper_install_dir(&dirs::home_dir()?);
    install_helper_from_to(&src_dir, &bin_dir, names)
}

/// helper 安装路径解析 + 拷贝核心（tempdir 可测缝）：`names` 依序探测 src_dir 下
/// 候选（Windows 发行 .exe 在前、非 Windows 开发态裸名在后），首个存在者拷到
/// dst_dir 同名（无条件覆盖；unix 补 0755）
fn install_helper_from_to(
    src_dir: &std::path::Path,
    dst_dir: &std::path::Path,
    names: &[&str],
) -> Option<PathBuf> {
    let _ = fs::create_dir_all(dst_dir);
    for name in names {
        let src = src_dir.join(name);
        if src.is_file() {
            let dst = dst_dir.join(name);
            if fs::copy(&src, &dst).is_ok() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(metadata) = fs::metadata(&dst) {
                        let mut perms = metadata.permissions();
                        perms.set_mode(0o755);
                        let _ = fs::set_permissions(&dst, perms);
                    }
                }
                return Some(dst);
            }
        }
    }
    // **已有安装副本回退**（2026-10-03 事故修复）：源目录没有候选（如无 feature 的
    // `cargo build` 不产出 helper exe——cargo clean 后必然发生）但 `~/.tuvis/bin/` 里
    // 躺着**先前安装的副本**时，返回该副本而非 None。返回 None 会让注册规格回落
    // bash 形态，启动注册把用户 settings 里**更丰富的 helper 条目原地改写成 bash**
    //（问答通道的 tool_name/tool_input 全丢——多选题误判审批卡的事故根因）。
    // 副本版本偏旧可接受：helper 的事件文件协议自 T8 起稳定，比 bash 兜底（零富
    // 字段）严格更优。
    for name in names {
        let dst = dst_dir.join(name);
        if dst.is_file() {
            return Some(dst);
        }
    }
    None
}

/// Windows hook 命令：**正斜杠**路径，含空格才加引号。两层转义约束（第四轮实机
/// 验收实证）：① 引号在 claude 的 `powershell -Command "<command>"` 包装下破坏
/// 外层配对（SessionStart 报错根因）；② 裸反斜杠路径在 bash 端被当转义序列吃掉
/// （`C:\Users` → `C:Users`，exit=127 通道全断）。正斜杠两层皆安全（bash 原生
/// 接受、powershell 不转义），第四轮离线探针三层全通。含空格路径必须保引号
/// （已知残留：该形态下 SessionStart 报错可能复现，spec §3.4 边界）
fn quote_bash_command(path_str: &str) -> String {
    let normalized = path_str.replace('\\', "/");
    if normalized.contains(' ') {
        format!("bash \"{normalized}\"")
    } else {
        format!("bash {normalized}")
    }
}

/// 当前平台注册的 hook 命令（注册 / 核验 / 去重三处同源，单一事实源）
fn hook_command_for(script_path: &std::path::Path) -> String {
    let s = script_path.to_string_lossy().to_string();
    if cfg!(windows) {
        quote_bash_command(&s)
    } else {
        s
    }
}

/// helper 直启命令（批次甲 T1 零 shell 依赖）：正斜杠归一 + 含空格才加引号——
/// 与 quote_bash_command 同一套两层转义实证结论（claude 的 powershell -Command
/// 包装 / codex 直接 spawn 两层皆安全），去掉 bash 前缀。含空格路径的引号残留
/// 边界同 bash 形态（spec §3.4 已知残留，非本批处理面）
fn helper_command_for(helper_path: &std::path::Path) -> String {
    let normalized = helper_path.to_string_lossy().replace('\\', "/");
    if normalized.contains(' ') {
        format!("\"{normalized}\"")
    } else {
        normalized
    }
}

/// Hook 命令规格（T1 单一事实源：注册 / 核验 / 迁移三处同源）
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HookCommandSpec {
    /// 注册命令主体：claude `command` 字段；codex 的非 Windows 落地形态
    command: String,
    /// codex 官方 `commandWindows` 字段（Windows 直启覆盖，codex hook_config.rs
    /// L167 实证；直接 spawn 零 shell——issue #74 根因 2 的根治面）。claude 无此
    /// 字段 → None
    command_windows: Option<String>,
    /// helper 落盘绝对路径原文（我方条目判据 / 迁移标记用；bash 兜底形态 → None）
    helper_path: Option<String>,
}

/// 按工具构造命令规格（生产入口：平台语义取 cfg!(windows)；跨平台可测纯函数见
/// [`hook_command_spec_for_impl`])
fn hook_command_spec_for(
    tool_id: &str,
    script_path: &std::path::Path,
    helper_path: Option<&std::path::Path>,
) -> HookCommandSpec {
    hook_command_spec_for_impl(tool_id, script_path, helper_path, cfg!(windows))
}

/// 规格构造纯决策（`windows_semantics` 由生产入口传 `cfg!(windows)`，测试双平台
/// 语义均可显式驱动）：
/// - **claude + Windows + helper**：`command` 直启 helper（powershell 包装下正斜杠
///   安全，helper_command_for）；helper 缺席回落 bash 形态（零回归）
/// - **codex + Windows + helper**：`command` 恒为 bash 形态（非 Windows 落地用，
///   unix 行为不变）；Windows 由官方 `commandWindows` 直启 helper
/// - **kimi + helper**（T2）：`command` 直启 helper（config.toml `[[hooks]]` 无
///   commandWindows 字段——官方仅 event/matcher/command/timeout 四字段，两平台
///   同一 command 落地；正斜杠归一 + 含空格引号与 helper_command_for 同一套实证）。
///   helper 缺席**不回落**：kimi 此前无 hook 通道，注册一条已知跑不起来的 bash
///   形态命令比不注册更糟（TUI 内钩子报错噪音）——调用方（register_all_hooks）对
///   helper 缺席直接跳过 kimi 注册，维持「无通道」原状即零回归
/// - **兜底**（非 Windows / helper 未随包分发 / 其余工具）：bash 形态、无覆盖字段
///   ——与 T1 前行为完全一致
fn hook_command_spec_for_impl(
    tool_id: &str,
    script_path: &std::path::Path,
    helper_path: Option<&std::path::Path>,
    windows_semantics: bool,
) -> HookCommandSpec {
    let bash_form = hook_command_for(script_path);
    let helper_str = helper_path.map(|p| p.to_string_lossy().to_string());
    match (tool_id, windows_semantics, helper_path) {
        ("claude", true, Some(h)) => HookCommandSpec {
            command: helper_command_for(h),
            command_windows: None,
            helper_path: helper_str,
        },
        ("codex", true, Some(h)) => HookCommandSpec {
            command: bash_form,
            command_windows: Some(helper_command_for(h)),
            helper_path: helper_str,
        },
        // kimi：[[hooks]] 无 commandWindows 字段，command 两平台同形（直启 helper）
        ("kimi", _, Some(h)) => HookCommandSpec {
            command: helper_command_for(h),
            command_windows: None,
            helper_path: helper_str,
        },
        _ => HookCommandSpec {
            command: bash_form,
            command_windows: None,
            helper_path: None,
        },
    }
}

/// 我方条目判据标记集：脚本绝对路径 + helper 落盘路径，各配正反斜杠双形态（F3
/// 双形态判据的 helper 扩展——历史条目可能以任一路径、任一斜杠形态在册）；外加
/// helper 文件名标记（T2）：helper 换位升级时旧条目 command 只含旧路径，与新规格
/// 无前缀交集——claude/kimi 的 command **就是** helper 路径（无脚本路径兜底），
/// codex 的 Windows 覆盖字段同理，靠文件名仍认得出我方条目，迁移才不会退化成
/// 追加双条目。语义安全面：含该文件名的命令本就是在调我们的 helper，视为我方
/// 条目（刷新到当前路径）正是期望行为
fn ours_markers(script_path_str: &str, spec: &HookCommandSpec) -> Vec<String> {
    let mut markers = vec![
        script_path_str.to_string(),
        script_path_str.replace('\\', "/"),
    ];
    if let Some(hp) = &spec.helper_path {
        markers.push(hp.clone());
        markers.push(hp.replace('\\', "/"));
        if let Some(name) = std::path::Path::new(hp)
            .file_name()
            .and_then(|n| n.to_str())
        {
            if !name.is_empty() {
                markers.push(name.to_string());
            }
        }
    }
    // **改名前版本的孤儿条目迁移标记**（2026-10-10 实机）：rebrand（.mam→.tuvis）
    // 之前注册的条目指向 `~/.mam/` 老家 + 老监听器名 `mam-hook-listener.exe`——
    // 新版本按新路径认不出它们是自家的，迁移步跳过 → 孤儿条目永远指向不存在的
    // 文件，每次工具调用报 hook error 且问题通道（富事件）静默失效。老家路径与
    // 老监听器名也认作自家，让迁移步把它们原地改写成新规格。`contains` 语义下
    // 新命令（tuvis-hook-listener.exe）不含这些片段，无误伤。
    markers.extend([
        ".mam/hooks/status-hook.sh".to_string(),
        ".mam/bin/mam-hook-listener.exe".to_string(),
        "mam-hook-listener.exe".to_string(),
    ]);
    markers
}

/// 条目命令是否 兔维斯 注册（含任一标记即算——`contains` 语义与 F3 迁移判据同源）
fn command_is_ours(command: &str, markers: &[String]) -> bool {
    markers.iter().any(|m| command.contains(m.as_str()))
}

/// F3 旧键迁移纯函数（PascalCase 注册形态专用；跨平台可测）：把 event（如 "Stop"）
/// 的首字母小写旧键（"stop"）从 hooks 配置移除——**仅当旧键全部条目都是 兔维斯 注册**
/// （每条 command 命中我方标记集 [`ours_markers`]：脚本路径/helper 路径 × 正反斜杠
/// 双形态）；混有用户条目 → 保守不动（codex 对未知键不触发，残留无害）。旧键不
/// 存在 → false。返回 true 表示发生了移除（计入 migrated 保证纯迁移场景也持久化）。
fn remove_legacy_camel_key(
    hooks_obj: &mut serde_json::Map<String, serde_json::Value>,
    event: &str,
    markers: &[String],
) -> bool {
    let legacy: String = {
        let mut chars = event.chars();
        match chars.next() {
            Some(first) => first.to_lowercase().chain(chars).collect::<String>(),
            None => return false,
        }
    };
    if legacy == event {
        return false; // 本就全小写的事件名无 twins（防御）
    }
    let all_ours = hooks_obj
        .get(&legacy)
        .and_then(|v| v.as_array())
        .map(|arr| {
            !arr.is_empty()
                && arr.iter().all(|entry| {
                    entry
                        .get("hooks")
                        .and_then(|h| h.as_array())
                        .map(|cmds| {
                            !cmds.is_empty()
                                && cmds.iter().all(|h| {
                                    h.get("command")
                                        .and_then(|c| c.as_str())
                                        .map(|s| command_is_ours(s, markers))
                                        .unwrap_or(false)
                                })
                        })
                        .unwrap_or(false)
                })
        });
    if all_ours == Some(true) {
        hooks_obj.remove(&legacy).is_some()
    } else {
        false
    }
}

/// 启动核验判据（纯函数，跨平台可测）：**每个期望事件的我方条目恰好一条且已等于
/// 当前规格**——matcher 符合期望、（claude/kimi）command 等于规格命令、（codex）
/// commandWindows 等于规格覆盖。键形态核验是 F3 存量迁移的可达性前提；
/// commandWindows 核验是 T1 存量迁移的可达性前提；**我方条目数核验（2026-09-24）
/// 是双注册残留收敛的可达性前提**——旧判据只查「规格命令是否在文件某处出现」，
/// helper 条目在场即判已核验，与 helper 并存的旧 bash 条目（同为我方标记）就永远
/// 不进注册路径：两 hook 同事件双触发、双写同一事件文件，bash 兜底只写基础字段且
/// 后写覆盖 helper 的富载荷（`tool_name`/`tool_input` 丢失 → claude AUQ 被误判成
/// 审批，取证见 `research/refs/phase2-消息注入/2026-09-24-claude多选多题键序-用户
/// 实机取证.md` 关联根因）。判据从字符串级升级为 JSON 解析级（注册文件恒为本
/// 注册器/官方工具产出的 pretty JSON）。
fn hooks_file_verified(
    content: &str,
    script_path_str: &str,
    spec: &HookCommandSpec,
    events: &[&str],
    is_pascal_case: bool,
    matchers: &[(&str, &str)],
) -> bool {
    let Ok(config) = serde_json::from_str::<serde_json::Value>(content) else {
        return false;
    };
    let Some(hooks) = config.get("hooks") else {
        return false;
    };
    let markers = ours_markers(script_path_str, spec);
    events.iter().all(|e| {
        let key = if is_pascal_case {
            (*e).to_string()
        } else {
            let mut chars = e.chars();
            match chars.next() {
                Some(first) => first.to_lowercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        };
        let expected_matcher = matchers
            .iter()
            .find(|(ev, _)| *ev == *e)
            .map(|(_, m)| *m)
            .unwrap_or("");
        event_hooks_converged(hooks, &key, &markers, spec, expected_matcher)
    })
}

/// 单事件的「我方条目收敛」判据（核验侧纯函数）：该事件下我方条目（条目内任一
/// command 命中标记集）**恰好一条**，且该条目 matcher 符合期望（缺 matcher 字段按
/// 空串——注册器产出的条目恒带 matcher，历史/官方文件可能缺省）、其内**所有我方
/// command 及 codex 的 commandWindows** 等于当前规格。多于一条 = 双注册残留；
/// 一条但形态旧 = 待迁移；零条 = 待追加——三种形态都判未核验，注册路径保持可达。
fn event_hooks_converged(
    hooks: &serde_json::Value,
    event_key: &str,
    markers: &[String],
    spec: &HookCommandSpec,
    expected_matcher: &str,
) -> bool {
    let Some(arr) = hooks.get(event_key).and_then(|v| v.as_array()) else {
        return false;
    };
    let mut ours_entries = 0usize;
    let mut converged = false;
    for entry in arr {
        let Some(cmds) = entry.get("hooks").and_then(|h| h.as_array()) else {
            continue;
        };
        let has_ours = cmds.iter().any(|h| hook_command_is_ours(h, markers));
        if !has_ours {
            continue;
        }
        ours_entries += 1;
        let matcher_ok =
            entry.get("matcher").and_then(|m| m.as_str()).unwrap_or("") == expected_matcher;
        let cmds_ok = cmds
            .iter()
            .all(|h| hook_command_matches_spec(h, markers, spec));
        if matcher_ok && cmds_ok {
            converged = true;
        }
    }
    ours_entries == 1 && converged
}

/// 单个 hook 命令对象是否 兔维斯 注册（command 命中我方标记集）
fn hook_command_is_ours(h: &serde_json::Value, markers: &[String]) -> bool {
    h.get("command")
        .and_then(|c| c.as_str())
        .map(|s| command_is_ours(s, markers))
        .unwrap_or(false)
}

/// 单个 hook 命令对象是否已等于当前规格：非我方命令（用户自己的）不判过；
/// 我方 command 必须相等，且（spec 有 Windows 覆盖时）commandWindows 必须在场且相等
fn hook_command_matches_spec(
    h: &serde_json::Value,
    markers: &[String],
    spec: &HookCommandSpec,
) -> bool {
    let Some(c) = h.get("command").and_then(|c| c.as_str()) else {
        return true; // 无 command 字段的条目不是我们产出的命令，交给用户语义
    };
    if !command_is_ours(c, markers) {
        return true; // 用户命令混在我方条目内：不判（注册侧也只动我方命令）
    }
    if c != spec.command {
        return false;
    }
    match (
        &spec.command_windows,
        h.get("commandWindows").and_then(|w| w.as_str()),
    ) {
        (Some(want), Some(cur)) => cur == want,
        (Some(_), None) => false,
        (None, _) => true,
    }
}

/// 为指定工具注册 Hook（生产入口：脚本落盘 + helper 安装 + 命令规格注入核心）。
/// `spec` 由调用方按工具构造（[`hook_command_spec_for`]）——claude/codex 的命令
/// 形态不同（command 直启 vs commandWindows 覆盖），规格单一事实源避免注册/核验/
/// 迁移三处口径漂移。`matchers` 为 (事件名, matcher) 对（T2：claude Notification
/// → permission_prompt；无 matcher 的事件传 `&[]`）
/// 用户 hook 配置文件的**读入 + UTF-8 BOM 剥离**（2026-10-11 评审 Important 2）：
/// Windows 编辑器（Notepad/部分 PowerShell `Out-File`）写出的文件带
/// `EF BB BF` 前缀——`serde_json::from_str`/`toml_edit::parse` 都不剥，
/// 直接「解析配置文件失败: expected value at line 1 column 1」（2026-10-10
/// 22:16 实机：codex hooks.json 带 BOM → 每次启动注册失败）。四个用户配置
/// 读取点（JSON 注册 / TOML 注册 / 核验 / 诊断）统一走本函数；自家事件文件
/// 不经此（写入方无 BOM 面）。
fn read_user_config_text(path: &std::path::Path) -> std::io::Result<String> {
    Ok(std::fs::read_to_string(path)?
        .trim_start_matches('\u{feff}')
        .to_string())
}

pub(crate) fn register_hooks_for_tool(
    config_path: &std::path::Path,
    events: &[&str],
    is_pascal_case: bool,
    spec: &HookCommandSpec,
    matchers: &[(&str, &str)],
) -> Result<(), String> {
    let script_path = ensure_hook_script();
    let script_path_str = script_path.to_string_lossy().to_string();
    register_hooks_in_file(
        config_path,
        events,
        is_pascal_case,
        &script_path_str,
        spec,
        matchers,
    )
    .map(|_| ())
}

/// 注册核心（tempfile 可测缝：脚本路径/命令规格显式注入，零接触真实 ~/.tuvis）。
/// 返回 (新增条目数, 迁移条目数)。
fn register_hooks_in_file(
    config_path: &std::path::Path,
    events: &[&str],
    is_pascal_case: bool,
    script_path_str: &str,
    spec: &HookCommandSpec,
    matchers: &[(&str, &str)],
) -> Result<(usize, usize), String> {
    // 读取现有配置（不存在则创建空对象）
    let existing = read_user_config_text(config_path).unwrap_or_else(|_| "{}".to_string());
    let mut config: serde_json::Value =
        serde_json::from_str(&existing).map_err(|e| format!("解析配置文件失败: {}", e))?;

    // 确保 hooks 对象存在
    if config.get("hooks").is_none() {
        config["hooks"] = serde_json::json!({});
    }
    let hooks = config.get_mut("hooks").ok_or("hooks 字段不存在")?;
    let hooks_obj = hooks.as_object_mut().ok_or("hooks 字段不是对象")?;

    let markers = ours_markers(script_path_str, spec);
    let mut added = 0;
    // 原地迁移的旧条目计数（处）：仅用于成功日志区分「新注册」与「迁移」，
    // 不改变控制流语义（0 ⇔ 原来的 migrated_any=false）
    let mut migrated = 0usize;
    for &event in events {
        let event_name = if is_pascal_case {
            event.to_string()
        } else {
            // PascalCase → camelCase: 首字母小写
            let mut chars = event.chars();
            match chars.next() {
                Some(first) => first.to_lowercase().chain(chars).collect::<String>(),
                None => continue,
            }
        };
        // T2：该事件的期望 matcher（无 → 空，与现行形态一致——注册条目恒带
        // matcher 字段，claude/codex 官方形态均如此）
        let expected_matcher = matchers
            .iter()
            .find(|(ev, _)| *ev == event)
            .map(|(_, m)| *m)
            .unwrap_or("");

        // 已注册检测 + 旧形态迁移：条目 command 命中我方标记集（脚本路径/helper
        // 路径 × 正反斜杠，[`command_is_ours`]）即视为我们注册的。与当前命令一致
        // 且 Windows 覆盖字段齐备 → 跳过；形态旧（如 bash 脚本命令、缺
        // commandWindows）→ 原地改写为当前规格（追加会造成双写事件且旧条目继续
        // 触发报错）
        let mut already = false;
        let mut migrated_this_event = 0usize;

        // F3 旧键迁移（仅 PascalCase 注册形态；codex hook_event_case CamelCase→
        // PascalCase 存量修正，2026-09-20）：旧注册把 兔维斯 条目写在首字母小写键下
        // （如 "stop"），codex 0.155.x 只认 PascalCase 键——旧键永不触发但残留
        // 文件。移除判据见 [`remove_legacy_camel_key`]。
        // 跨键移除只计入迁移日志（migrated），**不参与下方 skip 守卫**：旧键条目
        // 已删除、新键可能尚不存在（纯存量 camelCase 文件），必须走下方追加建键
        // ——共用计数会把纯存量文件迁成空 {"hooks":{}}（复评 P1-1，2026-09-20）
        let legacy_removed = is_pascal_case && remove_legacy_camel_key(hooks_obj, event, &markers);
        if let Some(arr) = hooks_obj
            .get_mut(&event_name)
            .and_then(|v| v.as_array_mut())
        {
            for entry in arr.iter_mut() {
                // T2 matcher 迁移（独立借用域，先于 cmds 可变借用）：我方条目
                // （command 命中标记集）的条目级 matcher 与期望不符（历史空
                // matcher / 漂移）→ 原地改写。判据与 command 迁移同源：只动我方
                // 条目，用户条目（含其 matcher 语义）永不触碰
                let matcher_fixed = {
                    let has_ours = entry
                        .get("hooks")
                        .and_then(|h| h.as_array())
                        .is_some_and(|cmds| cmds.iter().any(|h| hook_command_is_ours(h, &markers)));
                    if has_ours
                        && entry.get("matcher").and_then(|m| m.as_str()) != Some(expected_matcher)
                    {
                        entry["matcher"] = serde_json::json!(expected_matcher);
                        true
                    } else {
                        false
                    }
                };
                migrated_this_event += usize::from(matcher_fixed);
                let Some(cmds) = entry.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
                    continue;
                };
                for h in cmds.iter_mut() {
                    let Some(c) = h
                        .get("command")
                        .and_then(|c| c.as_str())
                        .map(|s| s.to_string())
                    else {
                        continue;
                    };
                    // 用户自己的 hook 条目，不动
                    if !command_is_ours(&c, &markers) {
                        continue;
                    }
                    let cmd_ok = c == spec.command;
                    // commandWindows 期望形态比对：spec 无覆盖 → 不强求（unix 上
                    // 该字段惰性，历史残留无害不折腾）；spec 有覆盖 → 必须在场且
                    // 一致（缺失/旧值都算未迁移——T1 bash→helper 迁移的主形态）
                    let win_ok = match (
                        &spec.command_windows,
                        h.get("commandWindows").and_then(|w| w.as_str()),
                    ) {
                        (Some(want), Some(cur)) => cur == want,
                        (Some(_), None) => false,
                        (None, _) => true,
                    };
                    if cmd_ok && win_ok {
                        already = true;
                        continue;
                    }
                    if !cmd_ok {
                        h["command"] = serde_json::json!(spec.command);
                        migrated_this_event += 1;
                    }
                    if !win_ok {
                        if let Some(want) = &spec.command_windows {
                            h["commandWindows"] = serde_json::json!(want);
                        }
                        migrated_this_event += 1;
                    }
                }
            }
        }
        // 2026-09-24 双注册残留收敛（先于 skip 守卫与计数汇总）：同一事件下我方
        // **命令**只保留一条（第一条——上方迁移段已把它改写为当前规格），其余条目
        // 内的我方命令移除、被掏空的条目整条删除——双条目会让两个 hook 同事件双
        // 触发并双写同一事件文件，bash 兜底只写基础字段且后写覆盖 helper 的富载荷
        // （tool_name/tool_input 丢失 = claude AUQ 被误判审批的根因，取证见
        // 2026-09-24-claude多选多题键序-用户实机取证）。用户命令永不触碰。
        let pruned = prune_extra_ours_entries(hooks_obj.get_mut(&event_name), &markers);
        migrated_this_event += pruned;
        migrated += migrated_this_event + usize::from(legacy_removed);
        // 已注册或本轮完成原地迁移：条目已等于当前规格，再追加会产生同命令重复
        // 条目（同事件双触发、双写事件），直接进入下一事件；持久化由 migrated 计数保证
        if already || migrated_this_event > 0 {
            debug!("Hook 已注册/已迁移: {}", event_name);
            continue;
        }

        // 合并式追加：用户已有同事件 hooks 时保留其条目，仅追加我们的（不整组替换）
        let mut handler = serde_json::json!({ "type": "command", "command": &spec.command });
        if let Some(want) = &spec.command_windows {
            handler["commandWindows"] = serde_json::json!(want);
        }
        // T2：注册不得带 async（红线 3——codex 跳过 async 钩子；本注册器从未产出
        // 该字段，此注释为红线锚点）。matcher 按事件期望落字段（官方形态
        // `{"Notification":[{"matcher":"permission_prompt","hooks":[...]}]}`）
        let our_entry = serde_json::json!({
            "matcher": expected_matcher,
            "hooks": [handler]
        });
        match hooks_obj.get_mut(&event_name) {
            Some(arr) if arr.is_array() => {
                arr.as_array_mut().unwrap().push(our_entry);
            }
            _ => {
                hooks_obj.insert(event_name, serde_json::json!([our_entry]));
            }
        }
        added += 1;
    }

    if added > 0 || migrated > 0 {
        // 创建备份（防止写入失败导致配置丢失）
        if config_path.exists() {
            let backup = config_path.with_extension("json.bak");
            let _ = fs::copy(config_path, &backup);
        }
        let pretty =
            serde_json::to_string_pretty(&config).map_err(|e| format!("序列化配置失败: {}", e))?;
        crate::linker::write_config_locked(config_path, &pretty)
            .map_err(|e| format!("写入配置文件失败: {}", e))?;
        // 仅迁移（added=0）时「已注册 0 个」有误导：日志区分迁移条目数
        if migrated > 0 {
            info!(
                "已注册 {} 个 Hook（迁移旧条目 {} 处）到 {:?}",
                added, migrated, config_path
            );
        } else {
            info!("已注册 {} 个 Hook 到 {:?}", added, config_path);
        }
    }

    Ok((added, migrated))
}

/// 同一事件下「我方命令多于一条」的收敛（注册侧，2026-09-24）：跨全部条目**只保留
/// 第一条我方命令**（迁移段已把它改写为当前规格），其余我方命令逐一移除；条目被
/// 掏空（hooks 数组为空）则整条移除。返回移除的我方命令数（0 = 无残留）。
/// 用户命令（不命中标记集）永不触碰；混合条目只掏我方命令、用户命令原地保留。
///
/// 实机形态（claude settings.json，2026-09-23 取证）：每个事件下 helper 条目与
/// 旧 bash 兜底条目**并存**——两者同触发、写同一事件文件，bash 后写覆盖 helper 的
/// 富载荷（无 tool_name/tool_input），问答通道因此整体失效。本函数与
/// [`hooks_file_verified`] 的「我方条目恰好一条」判据配套：核验发现残留 → 注册
/// 路径进来 → 迁移改写 + 本函数收敛 → 下次核验通过。
fn prune_extra_ours_entries(arr: Option<&mut serde_json::Value>, markers: &[String]) -> usize {
    let Some(arr) = arr.and_then(|v| v.as_array_mut()) else {
        return 0;
    };
    let mut seen_ours = false;
    let mut pruned = 0usize;
    let mut empty_entries: Vec<usize> = Vec::new();
    for (idx, entry) in arr.iter_mut().enumerate() {
        let Some(cmds) = entry.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
            continue;
        };
        let before = cmds.len();
        cmds.retain(|h| {
            if !hook_command_is_ours(h, markers) {
                return true; // 用户命令保留
            }
            if !seen_ours {
                seen_ours = true;
                return true; // 全事件第一条我方命令保留
            }
            false // 其余我方命令移除（含同条目内的重复形态）
        });
        pruned += before - cmds.len();
        if before > 0 && cmds.is_empty() {
            empty_entries.push(idx);
        }
    }
    for idx in empty_entries.into_iter().rev() {
        arr.remove(idx);
    }
    pruned
}

/// kimi hooks 注册（T2 接入，config.toml `[[hooks]]` 数组表）：事件名 PascalCase
/// 原样写入（kimi 无大小写变形），command 直启 helper（[[hooks]] 无 commandWindows
/// 字段，两平台同形）。**helper 必须在场**——调用方保证（register_all_hooks 对
/// helper 缺席跳过 kimi，见 hook_command_spec_for_impl kimi 分支注释）。
pub(crate) fn register_kimi_hooks_for_tool(
    config_path: &std::path::Path,
    events: &[&str],
    spec: &HookCommandSpec,
) -> Result<(), String> {
    let script_path = ensure_hook_script();
    let script_path_str = script_path.to_string_lossy().to_string();
    register_kimi_hooks_in_file(config_path, events, &script_path_str, spec).map(|_| ())
}

/// kimi 注册核心（tempfile 可测缝）。判据与 JSON 路径同构：条目 command 命中我方
/// 标记集（[`ours_markers`]）即视为我方条目——command 与当前规格一致 → 跳过；
/// 漂移（helper 换位升级）→ 原地改写不追加；用户条目永不触碰。返回 (新增, 迁移)。
/// kimi 此前无 hook 通道 → 无存量 bash 迁移面（issue #74 / T2 任务书）
fn register_kimi_hooks_in_file(
    config_path: &std::path::Path,
    events: &[&str],
    script_path_str: &str,
    spec: &HookCommandSpec,
) -> Result<(usize, usize), String> {
    // toml_edit 保注释保格式（codex config.toml MCP 写链同款先例，services/mcp）
    let content = read_user_config_text(config_path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = content
        .parse()
        .map_err(|e| format!("解析 TOML 失败: {}", e))?;
    if doc.get("hooks").is_none() {
        doc["hooks"] = toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
    }
    let aot = doc["hooks"]
        .as_array_of_tables_mut()
        .ok_or("hooks 段不是 [[hooks]] 数组表")?;

    let markers = ours_markers(script_path_str, spec);
    let mut added = 0usize;
    let mut migrated = 0usize;
    // 事件 → 本轮是否已见我方条目（条目在场的决定依据）
    let mut present: Vec<bool> = vec![false; events.len()];

    for table in aot.iter_mut() {
        let cmd = table
            .get("command")
            .and_then(|i| i.as_str())
            .unwrap_or_default();
        if !command_is_ours(cmd, &markers) {
            continue; // 用户条目不动
        }
        let Some(ev) = table.get("event").and_then(|i| i.as_str()) else {
            continue;
        };
        if let Some(idx) = events.iter().position(|e| *e == ev) {
            present[idx] = true;
            if cmd != spec.command {
                // helper 路径漂移（升级换位）：原地刷新，不产生双条目
                table["command"] = toml_edit::value(&spec.command);
                migrated += 1;
            }
        }
        // 我方条目但事件不在注册清单（历史遗留）→ 保守保留，行为与 JSON 路径一致
    }
    for (idx, &event) in events.iter().enumerate() {
        if present[idx] {
            continue;
        }
        let mut table = toml_edit::Table::new();
        // 官方 [[hooks]] 仅 event/matcher/command/timeout 四字段；审批事件无需
        // matcher 过滤，timeout 沿官方默认（helper 毫秒级完成，红线 2 下无超时面）
        table["event"] = toml_edit::value(event);
        table["command"] = toml_edit::value(&spec.command);
        aot.push(table);
        added += 1;
    }

    if added > 0 || migrated > 0 {
        if config_path.exists() {
            let backup = config_path.with_extension("toml.bak");
            let _ = fs::copy(config_path, &backup);
        }
        crate::linker::write_config_locked(config_path, &doc.to_string())
            .map_err(|e| format!("写入配置文件失败: {}", e))?;
        if migrated > 0 {
            info!(
                "已注册 {} 个 Hook（迁移旧条目 {} 处）到 {:?}",
                added, migrated, config_path
            );
        } else {
            info!("已注册 {} 个 Hook 到 {:?}", added, config_path);
        }
    }

    Ok((added, migrated))
}

/// kimi hooks 启动核验（TOML 版 hooks_file_verified）：全部期望事件均有「event 命中
/// 且 command 等于当前规格」的我方条目才算核验通过（任一漂移 → 走注册修复）
fn hooks_toml_verified(content: &str, spec_command: &str, events: &[&str]) -> bool {
    let doc: toml_edit::DocumentMut = match content.parse() {
        Ok(d) => d,
        Err(_) => return false,
    };
    let Some(aot) = doc.get("hooks").and_then(|i| i.as_array_of_tables()) else {
        return false;
    };
    events.iter().all(|e| {
        aot.iter().any(|t| {
            t.get("event").and_then(|i| i.as_str()) == Some(*e)
                && t.get("command").and_then(|i| i.as_str()) == Some(spec_command)
        })
    })
}

/// 读取所有 Hook 事件文件，返回 session_id → 事件数据的映射（键由脚本侧
/// 文件名承载；旧 PPID 形态文件 30s TTL 内短暂并存、键永不匹配任何会话，无害）。
/// 目录定位与 helper 写侧同源（hook_listener::default_events_dir，TUVIS_HOME debug
/// 重定向口径一致——写读两侧经同一函数出路径，任何配置下互不脱靶）
pub fn read_hook_events() -> HashMap<String, HookEvent> {
    let events_dir = hook_listener::default_events_dir();
    read_hook_events_from(&events_dir)
}

/// 核心逻辑（tempdir 可测）：文件名即 session_id，白名单校验 + 30s TTL 过滤
fn read_hook_events_from(events_dir: &std::path::Path) -> HashMap<String, HookEvent> {
    let mut events = HashMap::new();
    if !events_dir.exists() {
        return events;
    }
    // 白名单谓词与 helper 写侧同一函数（hook_listener::session_id_allowed——
    // 含 T1 新增的 128 字符防御上限，两侧同口径）
    let valid_sid = hook_listener::session_id_allowed;
    if let Ok(entries) = fs::read_dir(events_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(filename) = path.file_name().and_then(|n| n.to_str()) {
                let Some(sid) = filename.strip_suffix(".json") else {
                    continue; // helper 原子写的 *.tmp 中间文件在此天然跳过
                };
                if !valid_sid(sid) {
                    continue;
                }
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(event) = serde_json::from_str::<HookEvent>(&content) {
                        let now = chrono::Utc::now().timestamp();
                        if now - event.ts < 30 {
                            events.insert(sid.to_string(), event);
                        }
                    }
                }
            }
        }
    }
    events
}

/// Hook 事件数据
#[derive(Debug, Deserialize)]
pub struct HookEvent {
    pub event: String,
    pub ts: i64,
    pub last_event_at: String,
    /// 事件携带的工具名（T8 问答通道：helper 写侧全事件照收；bash 兜底/旧事件文件
    /// 缺该键 → default 空串，向后兼容）。**T1 起它还是问答进入判据的主锚点**：
    /// claude 对 AskUserQuestion 待答投递 PermissionRequest(AUQ)（实机取证携带
    /// tool_name + 完整 tool_input）与 Notification(permission_prompt)（不带工具名），
    /// helper 的未决问答承接窗会把 AUQ 工具名带到 Notification 那一跳
    /// （hook_listener::with_carried_question_fields）
    #[serde(default)]
    pub tool_name: String,
    /// `tool_input` 原文 JSON 串（T8：**仅** PreToolUse ∧ tool_name==AskUserQuestion
    /// 时由 helper 附加，64KB 上限；其余事件恒 None。消费方=adapter 状态链的
    /// AskUserQuestion 专属分支——questions 载荷随标记落 DB，端点据此出问答卡）
    #[serde(default)]
    pub tool_input: Option<String>,
    /// 通知正文（批次丙 T1：**仅** Notification 事件由 helper 附加，4KB 前缀截断；
    /// 其余事件/旧事件文件/bash 兜底缺该键 → None）。诊断留痕 + 兼容旧判据形态。
    /// **实测语义边界**：claude 的 permission_prompt 对「真实审批」与
    /// 「AskUserQuestion 待答」发出的 message **逐字相同**（均为
    /// `Claude needs your permission`）——本字段**不参与**问答裁决（裁决锚点是
    /// tool_name，见 [`HookEvent::tool_name`]），仅作取证/日志依据
    #[serde(default)]
    pub message: Option<String>,
}

/// T5 信号健康度：per-tool hook 通道状态（设置页「信号健康度」分区下发结构）
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSignalHealth {
    pub tool_id: String,
    /// 工具显示名（adapter.name()，前端免二次查询）
    pub label: String,
    /// 注册 KV `hooks_registered_{tool_id}` = "true"（verified / 注册成功路径均置位）
    pub registered: bool,
    /// 该工具当前存在「活跃会话」（等待/运行/思考/压缩；Idle/Finished 不算）
    pub has_active_sessions: bool,
    /// 该工具会话最近一次 hook 事件时间（RFC3339 UTC；30s TTL 目录内无匹配事件 → None）
    pub last_event_at: Option<String>,
}

/// 活跃会话判定：Idle（进程开着但空闲）与 Finished 不算——空闲会话本就长时间无
/// 事件，计入会把「正常空闲」误报成「待信任」；判据的意图是捕捉「正在干活却收
/// 不到任何事件」的通道断流（信任门未过的典型症状）
fn session_is_active(status: &crate::session::SessionStatus) -> bool {
    !matches!(
        status,
        crate::session::SessionStatus::Idle | crate::session::SessionStatus::Finished
    )
}

/// 信号健康度核心逻辑（纯函数，tempdir 事件目录 + 注入闭包可测，不触全局状态）：
/// 事件按 session_id 键 → 经会话列表建立 session→tool 归属 → per-tool 取 ts 最大
/// 事件的时间。未匹配任何会话的事件（孤儿/旧 PPID 形态）不归属任何工具；同工具
/// 多事件取最近一条。registered 由调用方注入（生产=读 KV，测试=闭包）
pub fn compute_tool_signal_health(
    tools: &[(&str, &str)],
    registered_of: &dyn Fn(&str) -> bool,
    events: &HashMap<String, HookEvent>,
    sessions: &[crate::session::Session],
) -> Vec<ToolSignalHealth> {
    tools
        .iter()
        .map(|&(tool_id, label)| {
            let tool_sessions: Vec<&crate::session::Session> = sessions
                .iter()
                .filter(|s| s.agent_type.tool_id() == tool_id)
                .collect();
            let has_active_sessions = tool_sessions.iter().any(|s| session_is_active(&s.status));
            let last_event_at = tool_sessions
                .iter()
                .filter_map(|s| events.get(&s.id))
                .max_by_key(|e| e.ts)
                .map(|e| e.last_event_at.clone());
            ToolSignalHealth {
                tool_id: tool_id.to_string(),
                label: label.to_string(),
                registered: registered_of(tool_id),
                has_active_sessions,
                last_event_at,
            }
        })
        .collect()
}

/// codex 信任门一次性通知的 KV 键（T5）。shown=已示过（永不再示）；pending=注册期
/// 置位、setup 期消费——register_all_hooks 在 run() 早期执行，彼时 builder 尚未
/// 构建、AppHandle 不可得，通知必须延迟到 setup 闭包（consume_codex_trust_notice）
const CODEX_NOTICE_SHOWN_KEY: &str = "codex_hook_notice_shown";
const CODEX_NOTICE_PENDING_KEY: &str = "codex_hook_notice_pending";

/// 一次性判定（纯函数，内存库可测）：shown 已置 "true" → 永不再提醒。后续待办
/// 状态常驻设置页信号健康度分区，不靠重复弹窗
fn codex_notice_should_enqueue(shown: Option<&str>) -> bool {
    shown != Some("true")
}

/// conn 级内层（对齐 dao::settings 的 *_conn 模式）：KV 读写在同一连接内完成，
/// 内存库单测走真实路径——防键名/条件改错后测试仍绿；生产壳持全局 DB 锁传入
fn enqueue_codex_trust_notice_conn(conn: &rusqlite::Connection) {
    if codex_notice_should_enqueue(
        crate::database::dao::settings::get_setting_conn(conn, CODEX_NOTICE_SHOWN_KEY).as_deref(),
    ) {
        crate::database::dao::settings::set_setting_conn(conn, CODEX_NOTICE_PENDING_KEY, "true");
    }
}

/// codex 注册成功路径调用：未示过 → 置 pending（登记「启动后要示一次」）
fn enqueue_codex_trust_notice() {
    let conn = crate::database::connection::DB.lock().unwrap();
    enqueue_codex_trust_notice_conn(&conn);
}

/// setup 期消费（AppHandle 已得）：pending 在场 → 发一次系统通知 → 落 shown。
/// 通知发送失败也落 shown：一次性语义优先（失败重试会变成每次启动轰炸），待办
/// 状态在设置页信号健康度分区常驻可见，信息不因通知失败而丢失。
/// 文案不接 i18n：Rust 侧无 i18n 基建，通知一次性发出，双语完整文案在设置页分区
pub fn consume_codex_trust_notice(app: &tauri::AppHandle) {
    if crate::database::get_setting(CODEX_NOTICE_PENDING_KEY).as_deref() != Some("true") {
        return;
    }
    use tauri_plugin_notification::NotificationExt;
    if let Err(e) = app
        .notification()
        .builder()
        .title("Tuvis")
        .body(
            "Codex 钩子已注册，需信任后事件才会触发：请在 Codex 终端输入 /hooks \
             并信任兔维斯条目（一次性）",
        )
        .show()
    {
        warn!("codex 信任门一次性通知发送失败: {e}");
    }
    crate::database::set_setting(CODEX_NOTICE_SHOWN_KEY, "true");
    crate::database::set_setting(CODEX_NOTICE_PENDING_KEY, "false");
}

/// 为所有支持 Hook 的工具注册 Hook（在应用启动时调用）
/// 核验实际配置状态而非信任 DB 标志：修复"全局单标志 + 永不核验"导致的假阳性
/// （此前 claude 注册失败后因 codex 成功置位而永不重试）
pub fn register_all_hooks() {
    use crate::adapter::{claude::ClaudeAdapter, codex::CodexAdapter, kimi::KimiAdapter};
    use crate::adapter::{AgentAdapter, HookEventCase};

    let adapters: Vec<Box<dyn AgentAdapter>> = vec![
        Box::new(ClaudeAdapter),
        Box::new(CodexAdapter),
        Box::new(KimiAdapter),
    ];
    let script_path = ensure_hook_script();
    // T1 原生 helper：随启动分发管道落盘（tuvis-hook-listener）；未构建/未随包分发
    // 是合法状态 → None → claude/codex 规格回落 bash 形态命令（零回归）；kimi 无
    // bash 兜底通道（T2 接入前本就无 hook）→ 跳过注册维持原状
    let helper_path = install_hook_listener_helper();

    for adapter in &adapters {
        if !adapter.hook_supported() {
            continue;
        }
        let Some(config_path) = adapter.hook_config_path() else {
            continue;
        };
        let tool_id = adapter.agent_type().tool_id();
        let tool_key = format!("hooks_registered_{tool_id}");

        let events = adapter.hook_events();
        let is_pascal = matches!(adapter.hook_event_case(), HookEventCase::PascalCase);
        // T2 matcher 注册面（hook_event_matcher 单一事实源：claude Notification →
        // permission_prompt；其余事件/工具为空集）
        let matchers: Vec<(&str, &str)> = events
            .iter()
            .filter_map(|e| adapter.hook_event_matcher(e).map(|m| (*e, m)))
            .collect();
        // T1 命令规格（按工具单一事实源）：claude → command 直启 helper；codex →
        // commandWindows 直启 helper（command 保持 bash 形态供非 Windows 落地）；
        // kimi → command 直启 helper（TOML [[hooks]]）。核验/注册/迁移三处共用同一规格
        let spec = hook_command_spec_for(tool_id, &script_path, helper_path.as_deref());
        // kimi 红线（T2）：helper 缺席不注册（bash 形态在 kimi 通道无落地语义，
        // 见 hook_command_spec_for_impl 注释）；helper 在场是 kimi 注册的前置
        if tool_id == "kimi" && helper_path.is_none() {
            debug!("kimi helper 未随包分发，跳过 hooks 注册（维持无通道原状）");
            continue;
        }
        // 启动核验：配置文件实际引用**当前命令规格**（含 codex 的 commandWindows）、
        // 脚本存在、**事件键形态在场**（F3 键形态核验，2026-09-20）、且**每个事件
        // 的我方条目恰好一条**（2026-09-24 双注册残留核验——helper 与旧 bash 兜底
        // 并存会双写同一事件文件、后者覆盖前者的富载荷，claude AUQ 误判审批的根因）
        // 才跳过。kimi 走 TOML 核验（[[hooks]] 事件+命令逐条在场）
        let verified = read_user_config_text(&config_path)
            .map(|c| {
                if tool_id == "kimi" {
                    hooks_toml_verified(&c, &spec.command, &events)
                } else {
                    hooks_file_verified(
                        &c,
                        &script_path.to_string_lossy(),
                        &spec,
                        &events,
                        is_pascal,
                        &matchers,
                    )
                }
            })
            .unwrap_or(false)
            && script_path.exists();
        if verified {
            crate::database::set_setting(&tool_key, "true");
            debug!("{} Hook 已确认: {:?}", adapter.name(), config_path);
            continue;
        }

        let registered = if tool_id == "kimi" {
            register_kimi_hooks_for_tool(&config_path, &events, &spec)
        } else {
            register_hooks_for_tool(&config_path, &events, is_pascal, &spec, &matchers)
        };
        match registered {
            Ok(()) => {
                info!("Hook 注册成功: {} → {:?}", adapter.name(), config_path);
                crate::database::set_setting(&tool_key, "true");
                // codex 信任门引导（T2，issue #74 调研 §5 不确定点 2 / C-8 根因③）：
                // 0.155.x 的 hooks.json 默认 Untrusted，未 trust 的钩子不运行——
                // 注册成功 ≠ 事件会触发。trust 状态落用户层 config（哈希记账），
                // 仅需在 TUI 内人工信任一次；每次（重）注册后都提醒，核验跳过路径不提醒
                if tool_id == "codex" {
                    warn!(
                        "codex 需在 TUI 内 /hooks 审阅并信任兔维斯钩子一次，事件才会触发（trust 后 hash 落用户层 config）"
                    );
                    // T5 一次性桌面通知：仅首次（未示过）登记 pending，setup 期
                    // AppHandle 就绪后消费发出；重复注册/重启不再弹（KV 一次性）
                    enqueue_codex_trust_notice();
                }
            }
            Err(e) => warn!(
                "Hook 注册失败 {} → {:?}: {}",
                adapter.name(),
                config_path,
                e
            ),
        }
    }
}

/// codex 核验式信任专用的**结构化**我方判据（评审 P2 收紧，2026-10-03）：
/// [`command_is_ours`] 的 contains 语义会把 `bash <我方脚本>; /tmp/x.sh` 这类
/// 嫁接命令也判我方——而本判据的 true 直接驱动「代用户 Trust all」，必须收紧。
/// 结构匹配 = 命令 trim 后与注册形态**等值**：`<marker>` / `bash <marker>` /
/// `sh <marker>`（各含引号包裹变体，覆盖 quote_bash_command / helper_command_for
/// 的含空格形态）。标记集由调用方给（脚本/helper 路径双斜杠形态）。
fn command_is_pure_ours(command: &str, markers: &[String]) -> bool {
    let c = command.trim();
    !c.is_empty()
        && markers.iter().any(|m| {
            c == m.as_str()
                || c == format!("\"{m}\"")
                || c == format!("bash {m}")
                || c == format!("sh {m}")
                || c == format!("bash \"{m}\"")
                || c == format!("sh \"{m}\"")
        })
}

/// codex hooks.json 全条目我方核验（C8 用户在场裁决 2026-10-02：**核验式自动
/// 信任**）。`~/.codex/hooks.json` 存在且**每个 hook 命令都命中我方注册形态**
/// （[`command_is_pure_ours`] 结构化等值判据——评审 P2 收紧后 contains 不再
/// 用于此场景）→ true：新建管线遇「Hooks need review」审查框可代发 '2'
/// （Trust all——信任的确实是 兔维斯 自己注册的 hooks，远程创建的状态上报闭环）；
/// 文件缺失/损坏/空事件/混有非我方条目/嫁接命令（我方路径后接私货）→ false：
/// esc 跳过（屏面明示 `esc skip`，不信任只解锁 composer，保守不代用户做混杂态
/// 的信任决定）。
/// 可见性说明：`pub` + doc(hidden) 仅为集成测试（tests/create_e2e.rs）可达——
/// 对齐 `inject::e2e_support` 先例；crate 内生产调用走 `pub(crate)` 语义即可。
#[doc(hidden)]
pub fn codex_hooks_all_ours(home: &std::path::Path) -> bool {
    let path = home.join(".codex").join("hooks.json");
    let Ok(text) = read_user_config_text(&path) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    let Some(events) = v.get("hooks").and_then(|h| h.as_object()) else {
        return false;
    };
    if events.is_empty() {
        return false;
    }
    // 标记集 = 脚本路径 + helper 落盘路径（.exe/无扩展双候选），各正反斜杠双形态
    // ——注册形态单一事实源 hook_command_spec_for 的 codex 两形态（command=bash
    // 脚本 / commandWindows=helper 直启）全覆盖
    let mut markers = Vec::new();
    for p in [
        home.join(".tuvis").join("hooks").join("status-hook.sh"),
        home.join(".tuvis")
            .join("bin")
            .join("tuvis-hook-listener.exe"),
        home.join(".tuvis").join("bin").join("tuvis-hook-listener"),
    ] {
        let s = p.to_string_lossy().to_string();
        markers.push(s.replace('\\', "/"));
        markers.push(s.replace('/', "\\"));
    }
    events.values().all(|entries| {
        entries
            .as_array()
            .map(|arr| {
                !arr.is_empty()
                    && arr.iter().all(|e| {
                        e.get("hooks")
                            .and_then(|h| h.as_array())
                            .map(|hs| {
                                !hs.is_empty()
                                    && hs.iter().all(|h| {
                                        // command 必在且我方；commandWindows 缺席合法
                                        // （非 Windows 形态），在场也须我方——
                                        // 结构化等值判据（评审 P2 收紧）
                                        h.get("command")
                                            .and_then(|c| c.as_str())
                                            .map(|s| command_is_pure_ours(s, &markers))
                                            .unwrap_or(false)
                                            && h.get("commandWindows")
                                                .and_then(|c| c.as_str())
                                                .map(|s| command_is_pure_ours(s, &markers))
                                                .unwrap_or(true)
                                    })
                            })
                            .unwrap_or(false)
                    })
            })
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod command_quote_tests {
    use super::{codex_hooks_all_ours, quote_bash_command};

    #[test]
    fn no_space_path_becomes_forward_slash_unquoted() {
        // 第四轮实测：裸反斜杠路径被 bash 当转义序列吃掉（C:\Users → C:Users，
        // exit=127 通道全断）；正斜杠 + 无引号在 powershell 包装 / bash 两层皆安全
        assert_eq!(
            quote_bash_command(r"C:\Users\bunny\.tuvis\hooks\status-hook.sh"),
            r"bash C:/Users/bunny/.tuvis/hooks/status-hook.sh"
        );
    }

    /// codex hooks 全条目我方核验（C8 核验式自动信任的纯核）：全我方 → true；
    /// 混入非我方条目 / 文件缺失 → false（esc 跳过保守臂）。fixture 命令必须
    /// 含 **tempdir 家目录** 的脚本路径——marker 判据按 home 推导
    #[test]
    fn codex_hooks_all_ours_verifies_entries() {
        let td = tempfile::tempdir().unwrap();
        let cfg = td.path().join(".codex").join("hooks.json");
        std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        let ours_cmd = format!(
            "bash {}/.tuvis/hooks/status-hook.sh",
            td.path().to_string_lossy().replace('\\', "/")
        );
        let ours_win = format!(
            "{}/.tuvis/bin/tuvis-hook-listener.exe",
            td.path().to_string_lossy().replace('\\', "/")
        );
        // JSON 夹具用占位符 + replace 组装（format! 的 JSON 花括号转义不可读）
        let ours_only = r#"{"hooks":{"PreToolUse":[{"matcher":"","hooks":[{"type":"command","command":"__SCRIPT__","commandWindows":"__WIN__"}]}]}}"#
            .replace("__SCRIPT__", &ours_cmd)
            .replace("__WIN__", &ours_win);
        // 全我方（脚本路径 + Windows helper 双形态，实机 hooks.json 同款形态）
        std::fs::write(&cfg, &ours_only).unwrap();
        assert!(codex_hooks_all_ours(td.path()));
        // 混入非我方条目 → false
        let mixed = r#"{"hooks":{"PreToolUse":[{"matcher":"","hooks":[{"type":"command","command":"__SCRIPT__","commandWindows":"__WIN__"},{"type":"command","command":"node /tmp/other-tool/hook.js"}]}]}}"#
            .replace("__SCRIPT__", &ours_cmd)
            .replace("__WIN__", &ours_win);
        std::fs::write(&cfg, mixed).unwrap();
        assert!(!codex_hooks_all_ours(td.path()));
        // 文件缺失 → false
        std::fs::remove_file(&cfg).unwrap();
        assert!(!codex_hooks_all_ours(td.path()));
    }

    /// 评审 P2 收紧（2026-10-03）：contains 判据会把「我方路径后接私货」的嫁接
    /// 命令也判我方 → 代用户 Trust all 是不可接受的假阳；结构化等值判据必须拒
    #[test]
    fn codex_hooks_all_ours_rejects_grafted_commands() {
        let td = tempfile::tempdir().unwrap();
        let cfg = td.path().join(".codex").join("hooks.json");
        std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        let home_fwd = td.path().to_string_lossy().replace('\\', "/");
        let ours_win = format!("{home_fwd}/.tuvis/bin/tuvis-hook-listener.exe");
        for graft in [
            // 我方路径 + 链式私货（contains 时代的假阳形态）
            format!("bash {home_fwd}/.tuvis/hooks/status-hook.sh; /tmp/x.sh"),
            format!("{home_fwd}/.tuvis/bin/tuvis-hook-listener.exe && curl evil.example"),
            // 我方路径仅作参数夹带
            format!("node /tmp/x.js {home_fwd}/.tuvis/hooks/status-hook.sh"),
        ] {
            let json = r#"{"hooks":{"PreToolUse":[{"matcher":"","hooks":[{"type":"command","command":"__G__","commandWindows":"__WIN__"}]}]}}"#
                .replace("__G__", &graft)
                .replace("__WIN__", &ours_win);
            std::fs::write(&cfg, &json).unwrap();
            assert!(
                !codex_hooks_all_ours(td.path()),
                "嫁接命令必须拒（代用户信任场景不允 contains 假阳）：{graft}"
            );
        }
        // 对照：纯我方形态（bash 脚本 + helper 直启 + 引号包裹变体）仍放行
        let ok = r#"{"hooks":{"SessionEnd":[{"matcher":"","hooks":[{"type":"command","command":"bash __HOME__/.tuvis/hooks/status-hook.sh","commandWindows":"__WIN__"}]}]}}"#
            .replace("__HOME__", &home_fwd)
            .replace("__WIN__", &ours_win);
        std::fs::write(&cfg, &ok).unwrap();
        assert!(codex_hooks_all_ours(td.path()));
        let ok2 = serde_json::json!({
            "hooks": {
                "SessionEnd": [
                    {
                        "matcher": "",
                        "hooks": [
                            // 含空格路径的注册形态：helper_command_for 会给整路径加引号
                            {"type": "command", "command": format!("\"{ours_win}\"")}
                        ]
                    }
                ]
            }
        })
        .to_string();
        std::fs::write(&cfg, &ok2).unwrap();
        assert!(
            codex_hooks_all_ours(td.path()),
            "引号包裹 helper 直启须放行"
        );
    }

    #[test]
    fn spaced_path_keeps_quotes_forward_slash() {
        // 含空格路径必须保引号（已知残留：该形态 SessionStart 报错可能复现，spec 3.4）；
        // 分隔符仍归一为正斜杠（bash 端语义一致）
        assert_eq!(
            quote_bash_command(r"C:\Users\John Doe\.tuvis\hooks\status-hook.sh"),
            r#"bash "C:/Users/John Doe/.tuvis/hooks/status-hook.sh""#
        );
    }
}

#[cfg(test)]
mod event_channel_tests {
    use super::*;
    use std::io::Write;

    fn write_event(dir: &std::path::Path, name: &str, event: &str, age_secs: i64) {
        let ts = chrono::Utc::now().timestamp() - age_secs;
        let body = format!(
            r#"{{"event":"{event}","session_id":"sid-x","cwd":"/tmp","ts":{ts},"last_event_at":"2026-09-12T00:00:00Z"}}"#
        );
        let mut f = std::fs::File::create(dir.join(format!("{name}.json"))).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    /// 用独立 tempdir 作为 events 目录跑 read_hook_events（测试以 tempdir 直注核心
    /// 逻辑，零接触真实 ~/.tuvis；T1 起 read_hook_events 目录定位与 helper 写侧同源
    /// ——hook_listener::default_events_dir，TUVIS_HOME debug 重定向两侧口径一致）
    #[test]
    fn events_are_keyed_by_session_id_with_ttl() {
        let tmp = tempfile::tempdir().unwrap();
        write_event(tmp.path(), "01a08083-5ca0", "Stop", 0);
        write_event(tmp.path(), "0f1e2d3c-4b5a", "Stop", 120); // 过期
        write_event(tmp.path(), "1", "Stop", 0); // 旧 PPID 形态孤儿（合法字符，30s 后自然消失）
        let m = read_hook_events_from(tmp.path());
        assert_eq!(m.len(), 2);
        assert!(m.contains_key("01a08083-5ca0"));
        assert!(m.contains_key("1"));
        assert!(!m.contains_key("0f1e2d3c-4b5a"));
    }

    #[test]
    fn script_uses_session_id_key_and_has_no_marker_block() {
        // spec 改动三：hook 周期注入退役；事件键 session_id 化（脚本内容回归锁）
        assert!(HOOK_SCRIPT.contains("$SESSION_ID.json"));
        assert!(!HOOK_SCRIPT.contains("MAM_MARKER"));
        assert!(!HOOK_SCRIPT.contains("tuvis-marker"));
        assert!(HOOK_SCRIPT.contains("^[A-Za-z0-9_-]+$")); // 白名单守卫在场（含 kimi 下划线）
    }
}

#[cfg(test)]
mod legacy_camel_key_tests {
    use super::{ours_markers, remove_legacy_camel_key, HookCommandSpec};

    fn our_entry(cmd: &str) -> serde_json::Value {
        serde_json::json!({ "matcher": "", "hooks": [{ "type": "command", "command": cmd }] })
    }

    /// bash 兜底规格（T1 前形态：无 helper、无 Windows 覆盖）——既有 F3 用例语义不变
    fn bash_spec(script_path_str: &str, command_str: &str) -> (Vec<String>, HookCommandSpec) {
        (
            ours_markers(
                script_path_str,
                &HookCommandSpec {
                    command: command_str.to_string(),
                    command_windows: None,
                    helper_path: None,
                },
            ),
            HookCommandSpec {
                command: command_str.to_string(),
                command_windows: None,
                helper_path: None,
            },
        )
    }

    #[test]
    fn removes_legacy_key_when_all_entries_ours() {
        // F3 存量形态：codex 旧注册把条目写在 "stop"（camelCase）下；
        // 条目命令与传入脚本路径同源（正斜杠形态经 fwd 判据命中）
        let mut obj = serde_json::Map::new();
        obj.insert(
            "stop".into(),
            serde_json::json!([our_entry("bash C:/Users/u/.tuvis/hooks/status-hook.sh")]),
        );
        let (markers, _) = bash_spec(r"C:\Users\u\.tuvis\hooks\status-hook.sh", "x");
        assert!(remove_legacy_camel_key(&mut obj, "Stop", &markers));
        assert!(obj.get("stop").is_none(), "全我们条目的旧键必须移除");
    }

    #[test]
    fn keeps_legacy_key_with_user_entries() {
        // 混有用户条目（command 不含脚本路径）→ 保守不动
        let mut obj = serde_json::Map::new();
        obj.insert(
            "stop".into(),
            serde_json::json!([
                our_entry("bash /home/u/.tuvis/hooks/status-hook.sh"),
                { "matcher": "", "hooks": [{ "type": "command", "command": "user-own-script" }] }
            ]),
        );
        let (markers, _) = bash_spec(r"C:\u\.tuvis\hooks\status-hook.sh", "x");
        assert!(!remove_legacy_camel_key(&mut obj, "Stop", &markers));
        assert!(obj.get("stop").is_some(), "混用户条目不得移除");
    }

    #[test]
    fn absent_or_wrong_case_legacy_key_is_noop() {
        let mut obj = serde_json::Map::new();
        let (markers, _) = bash_spec("/x/status-hook.sh", "x");
        assert!(!remove_legacy_camel_key(&mut obj, "Stop", &markers));
        // PascalCase 键名与 legacy 相同（防御：本就全小写事件名无 twins）
        let mut obj2 = serde_json::Map::new();
        obj2.insert("stop".into(), serde_json::json!([our_entry("x")]));
        let (markers2, _) = bash_spec("/x/status-hook.sh", "x");
        assert!(!remove_legacy_camel_key(&mut obj2, "stop", &markers2));
        assert!(obj2.get("stop").is_some());
    }

    /// T1 扩展：helper 形态条目（command 为 helper 直启命令）同样命中我方判据
    /// ——旧键迁移对 bash→helper 迁移后的文件依然可达
    #[test]
    fn helper_form_entries_are_recognized_as_ours() {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "stop".into(),
            serde_json::json!([our_entry("C:/Users/u/.tuvis/bin/tuvis-hook-listener.exe")]),
        );
        let spec = HookCommandSpec {
            command: "C:/Users/u/.tuvis/bin/tuvis-hook-listener.exe".to_string(),
            command_windows: None,
            helper_path: Some(r"C:\Users\u\.tuvis\bin\tuvis-hook-listener.exe".to_string()),
        };
        assert!(remove_legacy_camel_key(
            &mut obj,
            "Stop",
            &ours_markers("/x/s", &spec)
        ));
        assert!(obj.get("stop").is_none(), "helper 形态旧键也必须移除");
    }

    /// 复评 P1-1 回归锁（2026-09-20）：纯存量 camelCase 文件（条目 command 与
    /// 当前完全一致、只有键是旧形态）必须被完整迁移为 PascalCase 键——旧实现
    /// 跨键移除计数误触 skip 守卫，六事件走完后落盘 {"hooks":{}}（迁空）。
    #[test]
    fn legacy_camel_full_migration_rebuilds_pascal_keys() {
        use super::register_hooks_in_file;
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("hooks.json");
        let marker = "/fake-mam/hooks/status-hook.sh";
        let cmd = format!("bash {marker}");
        let legacy = serde_json::json!({"hooks": {
            "stop": [our_entry(&cmd)],
            "preToolUse": [our_entry(&cmd)],
        }});
        std::fs::write(&cfg, legacy.to_string()).unwrap();

        let (_, spec) = bash_spec(marker, &cmd);
        register_hooks_in_file(&cfg, &["Stop", "PreToolUse"], true, marker, &spec, &[]).unwrap();

        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let hooks = out.get("hooks").unwrap();
        assert!(
            hooks.get("Stop").is_some(),
            "PascalCase Stop 必须重建: {out}"
        );
        assert!(
            hooks.get("PreToolUse").is_some(),
            "PascalCase PreToolUse 必须重建: {out}"
        );
        assert!(
            hooks.get("stop").is_none() && hooks.get("preToolUse").is_none(),
            "旧 camelCase 键应移除: {out}"
        );
        assert!(
            std::fs::read_to_string(&cfg).unwrap().matches(&cmd).count() >= 2,
            "重建条目须携带当前命令"
        );
    }

    /// 复评 P1-2 回归锁（2026-09-20）：command 在场但事件键是旧 camelCase 形态
    /// → 未核验（须走注册迁移）；全期望键按形态在场 → 才核验跳过。
    #[test]
    fn hooks_file_verified_requires_event_key_form() {
        use super::hooks_file_verified;
        let script = "/x/status-hook.sh";
        let (_, spec) = bash_spec(script, "bash /x/status-hook.sh");
        let legacy = r#"{"hooks":{"stop":[{"hooks":[{"command":"bash /x/status-hook.sh"}]}]}}"#;
        let modern = r#"{"hooks":{"Stop":[{"hooks":[{"command":"bash /x/status-hook.sh"}]}]}}"#;
        // PascalCase 工具 + 旧 camel 键：command 在场也不得核验（迁移入口保持可达）
        assert!(!hooks_file_verified(
            legacy,
            script,
            &spec,
            &["Stop"],
            true,
            &[]
        ));
        // 全期望键在场：核验
        assert!(hooks_file_verified(
            modern,
            script,
            &spec,
            &["Stop"],
            true,
            &[]
        ));
        // 多事件任缺一键：不核验
        assert!(!hooks_file_verified(
            modern,
            script,
            &spec,
            &["Stop", "PreToolUse"],
            true,
            &[]
        ));
        // camelCase 形态工具按 camel 键核验（形态匹配即核验）
        assert!(hooks_file_verified(
            legacy,
            script,
            &spec,
            &["stop"],
            false,
            &[]
        ));
        // command 缺席：不核验
        let (_, other) = bash_spec(script, "bash /other.sh");
        assert!(!hooks_file_verified(
            modern,
            script,
            &other,
            &["Stop"],
            true,
            &[]
        ));
        // T2：带 matcher 的事件要求期望 matcher 在场——缺/漂移均不核验
        // （matcher 修复入口保持可达），matcher 命中才核验通过
        let notif_legacy = r#"{"hooks":{"Notification":[{"matcher":"","hooks":[{"command":"bash /x/status-hook.sh"}]}]}}"#;
        let notif_ok = r#"{"hooks":{"Notification":[{"matcher":"permission_prompt","hooks":[{"command":"bash /x/status-hook.sh"}]}]}}"#;
        assert!(!hooks_file_verified(
            notif_legacy,
            script,
            &spec,
            &["Notification"],
            true,
            &[("Notification", "permission_prompt")]
        ));
        assert!(hooks_file_verified(
            notif_ok,
            script,
            &spec,
            &["Notification"],
            true,
            &[("Notification", "permission_prompt")]
        ));
    }

    /// T1 存量迁移可达性：codex 规格下旧 bash 条目（command 与规格 command 完全
    /// 相同、仅缺 commandWindows）不得被判已核验——否则 commandWindows 迁移永不
    /// 触达（与 P1-2 键形态核验同一逻辑的 Windows 覆盖字段版）
    #[test]
    fn hooks_file_verified_requires_command_windows_when_spec_has_one() {
        use super::hooks_file_verified;
        let script = "/x/status-hook.sh";
        let legacy = r#"{"hooks":{"Stop":[{"hooks":[{"command":"bash /x/status-hook.sh"}]}]}}"#;
        let migrated = r#"{"hooks":{"Stop":[{"hooks":[{"command":"bash /x/status-hook.sh","commandWindows":"C:/u/.tuvis/bin/tuvis-hook-listener.exe"}]}]}}"#;
        let spec = HookCommandSpec {
            command: "bash /x/status-hook.sh".to_string(),
            command_windows: Some("C:/u/.tuvis/bin/tuvis-hook-listener.exe".to_string()),
            helper_path: Some(r"C:\u\.tuvis\bin\tuvis-hook-listener.exe".to_string()),
        };
        assert!(
            !hooks_file_verified(legacy, script, &spec, &["Stop"], true, &[]),
            "缺 commandWindows 的旧条目不得核验通过"
        );
        assert!(
            hooks_file_verified(migrated, script, &spec, &["Stop"], true, &[]),
            "commandWindows 齐备才核验通过"
        );
    }

    /// 2026-09-24 双注册残留核验（实机根因回归锁）：helper 条目在场但同事件下
    /// **另有**我方 bash 兜底条目 → 不得核验——旧判据（字符串 contains 规格命令）
    /// 会放行，残留 bash 后写覆盖 helper 富载荷，claude AUQ 被误判成审批。
    /// 收敛后（单条 helper）才核验通过；同条目内两条我方命令同样算残留。
    #[test]
    fn hooks_file_verified_rejects_duplicate_our_entries() {
        use super::hooks_file_verified;
        let script = "/u/.tuvis/hooks/status-hook.sh";
        let helper_cmd = "C:/u/.tuvis/bin/tuvis-hook-listener.exe";
        let spec = HookCommandSpec {
            command: helper_cmd.to_string(),
            command_windows: None,
            helper_path: Some(r"C:\u\.tuvis\bin\tuvis-hook-listener.exe".to_string()),
        };
        // 实机形态（2026-09-23 claude settings.json）：helper 条目 + bash 兜底条目并存
        let live = serde_json::json!({"hooks": {"PreToolUse": [
            { "matcher": "", "hooks": [{ "type": "command", "command": helper_cmd }] },
            { "matcher": "", "hooks": [{ "type": "command", "command": format!("bash {script}") }] },
        ]}});
        assert!(
            !hooks_file_verified(&live.to_string(), script, &spec, &["PreToolUse"], true, &[]),
            "我方条目两条并存不得核验（收敛入口必须可达）"
        );
        // 同一条目内两条我方命令：同样算残留
        let same_entry = serde_json::json!({"hooks": {"PreToolUse": [
            { "matcher": "", "hooks": [
                { "type": "command", "command": helper_cmd },
                { "type": "command", "command": format!("bash {script}") },
            ]},
        ]}});
        assert!(
            !hooks_file_verified(
                &same_entry.to_string(),
                script,
                &spec,
                &["PreToolUse"],
                true,
                &[]
            ),
            "同条目两条我方命令不得核验"
        );
        // 收敛形态（单条 helper）：核验通过
        let converged = serde_json::json!({"hooks": {"PreToolUse": [
            { "matcher": "", "hooks": [{ "type": "command", "command": helper_cmd }] },
        ]}});
        assert!(hooks_file_verified(
            &converged.to_string(),
            script,
            &spec,
            &["PreToolUse"],
            true,
            &[]
        ));
    }

    /// 2026-09-24 双注册残留收敛（注册侧回归锁）：helper + bash 并存 + 用户条目
    /// 混排的实机形态 → 注册一轮后必须收敛为**单条**我方命令（bash 移除、被掏空
    /// 条目整条删除、用户条目原样保留），且收敛结果通过核验（幂等）。
    #[test]
    fn register_hooks_prunes_duplicate_bash_residual_and_keeps_user_entries() {
        use super::{hooks_file_verified, register_hooks_in_file};
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("settings.json");
        let script = "/u/.tuvis/hooks/status-hook.sh";
        let helper_cmd = "C:/u/.tuvis/bin/tuvis-hook-listener.exe";
        let user_cmd = "node ~/my-own-hook.js";
        // 实机形态：helper（已等于规格）+ bash 兜底 + 用户条目，同一事件数组
        let live = serde_json::json!({"hooks": {"PreToolUse": [
            { "matcher": "", "hooks": [{ "type": "command", "command": helper_cmd }] },
            { "matcher": "", "hooks": [{ "type": "command", "command": format!("bash {script}") }] },
            { "matcher": "", "hooks": [{ "type": "command", "command": user_cmd }] },
        ]}});
        std::fs::write(&cfg, live.to_string()).unwrap();

        let spec = HookCommandSpec {
            command: helper_cmd.to_string(),
            command_windows: None,
            helper_path: Some(r"C:\u\.tuvis\bin\tuvis-hook-listener.exe".to_string()),
        };
        let (added, migrated) =
            register_hooks_in_file(&cfg, &["PreToolUse"], true, script, &spec, &[]).unwrap();
        assert_eq!(added, 0, "已有我方条目，不得再追加");
        assert!(migrated >= 2, "bash 改写 + 收敛移除都计入迁移：{migrated}");

        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let entries = out["hooks"]["PreToolUse"].as_array().unwrap();
        let ours: Vec<&str> = entries
            .iter()
            .flat_map(|e| e["hooks"].as_array().unwrap())
            .filter_map(|h| h["command"].as_str())
            .filter(|c| c.contains("status-hook.sh") || c.contains("tuvis-hook-listener"))
            .collect();
        assert_eq!(
            ours,
            vec![helper_cmd],
            "我方命令必须收敛为单条 helper：{out}"
        );
        assert!(
            entries.iter().any(|e| e["hooks"]
                .as_array()
                .is_some_and(|a| a.iter().any(|h| h["command"].as_str() == Some(user_cmd)))),
            "用户条目必须原样保留：{out}"
        );
        // 幂等：收敛后的文件通过核验（下一轮启动直接跳过）
        assert!(hooks_file_verified(
            &std::fs::read_to_string(&cfg).unwrap(),
            script,
            &spec,
            &["PreToolUse"],
            true,
            &[]
        ));
    }

    /// 收敛的混合条目面：我方命令与用户命令**同条目**混排时只掏我方命令，
    /// 用户命令留在原条目里（不整条删除）。
    #[test]
    fn prune_keeps_user_commands_inside_mixed_entries() {
        use super::register_hooks_in_file;
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("settings.json");
        let script = "/u/.tuvis/hooks/status-hook.sh";
        let helper_cmd = "C:/u/.tuvis/bin/tuvis-hook-listener.exe";
        let user_cmd = "node ~/my-own-hook.js";
        let live = serde_json::json!({"hooks": {"Stop": [
            { "matcher": "", "hooks": [
                { "type": "command", "command": helper_cmd },
                { "type": "command", "command": user_cmd },
            ]},
            { "matcher": "", "hooks": [{ "type": "command", "command": format!("bash {script}") }] },
        ]}});
        std::fs::write(&cfg, live.to_string()).unwrap();

        let spec = HookCommandSpec {
            command: helper_cmd.to_string(),
            command_windows: None,
            helper_path: Some(r"C:\u\.tuvis\bin\tuvis-hook-listener.exe".to_string()),
        };
        register_hooks_in_file(&cfg, &["Stop"], true, script, &spec, &[]).unwrap();

        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let commands: Vec<&str> = out["hooks"]["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|e| e["hooks"].as_array().unwrap())
            .filter_map(|h| h["command"].as_str())
            .collect();
        assert_eq!(
            commands,
            vec![helper_cmd, user_cmd],
            "混合条目只掏我方命令、用户命令原地保留：{out}"
        );
    }
}

/// F3 实机验证（#[ignore]：显式实机跑，M9R ffi_hop 先例；**M1A 前置**）——
/// 注册后跑一次真实 codex 会话确认钩子真触发：
/// ① 本测试（`cargo test --lib monitor::hooks::codex_pascal -- --ignored`）：
///    按生产装配对真实 `~/.codex/hooks.json` 注册 codex 全部事件（PascalCase，
///    T2 起 8 键：六状态键 + PermissionRequest/Interrupt），
///    断言文件落盘 PascalCase 键 + 旧 camelCase 键被迁移清除；
/// ② 人工步骤（Mac 回传清单 C-16/C-17）：跑一次真实 codex 交互会话，确认
///    `~/.tuvis/events/<session_id>.json` 出现（hook 真触发；**须先在 TUI 内
///    /hooks 信任 兔维斯 钩子一次**——信任门见 C-17）。
/// 本机无 codex 时测试失败（前置自检 `codex --version`）。
#[test]
#[ignore = "实机验证：改写真实 ~/.codex/hooks.json（M1A 前置，显式 --ignored 跑）"]
fn codex_pascal_case_registration_real_machine() {
    use crate::adapter::AgentAdapter;

    // 前置自检：codex 在场
    // 灰1 同款：npm 全局 codex 在 Windows 是 .cmd 垫片，CreateProcess 只补 .exe——
    // 裸名 spawn 失败回退 `cmd /c codex`（与 approve.rs probe_cli_version 同源）
    let npm_dir = std::env::var("USERPROFILE")
        .map(|u| format!("{}\\AppData\\Roaming\\npm", u))
        .unwrap_or_default();
    let aug_path = |cmd: &mut std::process::Command| {
        let p = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{npm_dir};{p}"));
    };
    let spawn_codex = |args: &[&str]| -> std::io::Result<std::process::Output> {
        let mut bare = std::process::Command::new("codex");
        bare.args(args).stdin(std::process::Stdio::null());
        aug_path(&mut bare);
        bare.output().or_else(|_| {
            let mut sh = std::process::Command::new("cmd");
            sh.args(["/c", "codex"])
                .args(args)
                .stdin(std::process::Stdio::null());
            aug_path(&mut sh);
            sh.output()
        })
    };
    let ver = spawn_codex(&["--version"]).expect("codex 命令不可用——本测试需要实机安装 codex");
    assert!(ver.status.success(), "codex --version 失败");

    let adapter = crate::adapter::codex::CodexAdapter;
    let path = adapter
        .hook_config_path()
        .expect("codex 必须有 hooks 配置路径");
    let events = adapter.hook_events();
    let is_pascal = matches!(
        adapter.hook_event_case(),
        crate::adapter::HookEventCase::PascalCase
    );
    assert!(is_pascal, "F3 修复后 codex 必须是 PascalCase 注册形态");

    let script_path = ensure_hook_script();
    let helper = install_hook_listener_helper();
    let spec = hook_command_spec_for("codex", &script_path, helper.as_deref());
    register_hooks_for_tool(&path, &events, is_pascal, &spec, &[]).expect("codex hooks 注册失败");

    let raw = std::fs::read_to_string(&path).expect("hooks.json 应存在");
    let cfg: serde_json::Value = serde_json::from_str(&raw).expect("hooks.json 合法 JSON");
    let hooks = cfg
        .get("hooks")
        .and_then(|h| h.as_object())
        .expect("hooks 对象");
    for ev in events {
        assert!(
            hooks.get(ev).is_some(),
            "PascalCase 键 {ev} 必须在注册后出现：{}",
            hooks.keys().cloned().collect::<Vec<_>>().join(",")
        );
        let legacy: String = {
            let mut chars = ev.chars();
            let first = chars.next().unwrap().to_lowercase();
            first.chain(chars).collect()
        };
        assert!(
            hooks.get(&legacy).is_none(),
            "旧 camelCase 键 {legacy} 必须被 F3 迁移清除"
        );
    }
}

/// T2 事件真触发自检（#[ignore]：显式实机跑；**本任务只落测试代码不实跑，实跑归
/// T7**）——注册后跑一次真实 codex 会话，验证钩子真的触发且事件文件落盘。
///
/// 全程沙箱（零接触真实 ~/.codex 与 ~/.tuvis）：CODEX_HOME/TUVIS_HOME 均指 tempdir
/// （CODEX_HOME 是 codex 官方配置根重定向；TUVIS_HOME 重定向仅 debug 构建的 helper
/// 生效——hook_listener::app_data_home）。前置条件（任缺即 fail 并给指引）：
/// ① codex 已安装且已登录（`codex exec` 无头跑一回合）；② helper 已按 debug 构建
/// （`cargo build --bin tuvis-hook-listener --features hook-listener`）；③ 若历史
/// 会话已在 TUI 内 /hooks 信任过同 hash 钩子则免信任——否则 untrusted 钩子不触发，
/// 测试会以信任门提示失败（这正是 C-17 验收项的自动形态）。
#[test]
#[ignore = "实机验证：跑真实 codex exec 会话验证事件落盘（实跑归 T7；前置=debug helper + codex 登录 + 信任门已过）"]
fn codex_hook_events_really_fire_in_real_session() {
    use crate::adapter::AgentAdapter;

    // 前置自检：codex 在场
    // 灰1 同款垫片回退 + PATH 追加 npm 全局目录（后台/沙箱环境 PATH 常缺
    // %USERPROFILE%\AppData\Roaming\npm，codex.cmd 解析不到）
    let npm_dir = std::env::var("USERPROFILE")
        .map(|u| format!("{}\\AppData\\Roaming\\npm", u))
        .unwrap_or_default();
    let aug_path = |cmd: &mut std::process::Command| {
        let p = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{npm_dir};{p}"));
    };
    let spawn_codex = |args: &[&str]| -> std::io::Result<std::process::Output> {
        let mut bare = std::process::Command::new("codex");
        bare.args(args).stdin(std::process::Stdio::null());
        aug_path(&mut bare);
        bare.output().or_else(|_| {
            let mut sh = std::process::Command::new("cmd");
            sh.args(["/c", "codex"])
                .args(args)
                .stdin(std::process::Stdio::null());
            aug_path(&mut sh);
            sh.output()
        })
    };
    let ver = spawn_codex(&["--version"]).expect("codex 命令不可用——本测试需要实机安装 codex");
    assert!(ver.status.success(), "codex --version 失败");

    // helper 必须已构建（debug）：TUVIS_HOME 重定向仅 debug 生效，release helper 会
    // 把事件写进真实 ~/.tuvis——绝不接受
    let exe_name = if cfg!(windows) {
        "tuvis-hook-listener.exe"
    } else {
        "tuvis-hook-listener"
    };
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
    let helper = target_dir.join("debug").join(exe_name);
    assert!(
        helper.is_file(),
        "helper 未构建：先跑 cargo build --bin tuvis-hook-listener --features hook-listener \
         （必须 debug 构建——TUVIS_HOME 重定向仅 debug 生效，release helper 会写真实 ~/.tuvis）"
    );

    // 沙箱：codex 配置根 / 兔维斯 数据根 / 工作目录 全部 tempdir
    let codex_home = tempfile::tempdir().unwrap();
    let mam_home = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();

    // 注册：CODEX_HOME/hooks.json 按 codex 规格（helper 指向已构建 debug helper；
    // 脚本路径仅作 command 字段占位——Windows 由 commandWindows 承载，不落盘脚本）
    let fake_script = codex_home.path().join("status-hook.sh");
    let spec = hook_command_spec_for("codex", &fake_script, Some(&helper));
    let adapter = crate::adapter::codex::CodexAdapter;
    let events = adapter.hook_events();
    let hooks_json = codex_home.path().join("hooks.json");
    register_hooks_in_file(
        &hooks_json,
        &events,
        true,
        &fake_script.to_string_lossy(),
        &spec,
        &[],
    )
    .expect("沙箱 hooks.json 注册失败");

    // 跑真实 codex 无头会话（env 注入 CODEX_HOME/TUVIS_HOME，子进程继承）
    // exec 会话同样走垫片回退（spawn 失败 → cmd /c codex exec …）
    let mut bare_exec = std::process::Command::new("codex");
    bare_exec
        .args(["exec", "--skip-git-repo-check"])
        .arg("-C")
        .arg(workdir.path())
        .arg("Reply with the single word: ok")
        .env("CODEX_HOME", codex_home.path())
        .env("TUVIS_HOME", mam_home.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    aug_path(&mut bare_exec);
    let mut sh_exec = std::process::Command::new("cmd");
    sh_exec
        .args(["/c", "codex", "exec", "--skip-git-repo-check"])
        .arg("-C")
        .arg(workdir.path())
        .arg("Reply with the single word: ok")
        .env("CODEX_HOME", codex_home.path())
        .env("TUVIS_HOME", mam_home.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    aug_path(&mut sh_exec);
    let mut child = bare_exec
        .spawn()
        .or_else(|_| sh_exec.spawn())
        .expect("codex exec 启动失败（检查 codex 登录态与 npm 全局目录是否在 PATH）");

    // 轮询事件目录 ≤180s（SessionStart/UserPromptSubmit 等会话期事件即应落盘；
    // 等 codex 自然退出再判，避免误杀慢启动）
    let events_dir = mam_home.path().join(".tuvis").join("events");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    let mut landed: Vec<std::path::PathBuf> = Vec::new();
    while std::time::Instant::now() < deadline {
        if let Ok(entries) = std::fs::read_dir(&events_dir) {
            landed = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "json"))
                .collect();
            if !landed.is_empty() {
                break;
            }
        }
        if let Ok(Some(_)) = child.try_wait() {
            // codex 已退出：再给 2s 余量后按落盘结果判
            std::thread::sleep(std::time::Duration::from_secs(2));
            if let Ok(entries) = std::fs::read_dir(&events_dir) {
                landed = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "json"))
                    .collect();
            }
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    // kill 后必须 wait 回收（clippy zombie_processes；同时 try_wait 分支已自行
    // 收割，此处 wait 对已退出子进程幂等）
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        !landed.is_empty(),
        "180s 内 ~/.tuvis/events 无事件文件落盘——钩子未触发。按序排查：\
         ① codex 信任门未过（TUI 内 /hooks 审阅并信任 兔维斯 钩子一次，C-17）；\
         ② codex 版本无 hooks 系统（<0.155）；③ codex exec 输出见上"
    );
    // 事件文件形态抽验：文件名即 session_id（白名单）、内容为读取侧格式
    for path in &landed {
        let sid = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        assert!(
            crate::monitor::hook_listener::session_id_allowed(sid),
            "落盘文件名必须是白名单 session_id: {sid:?}"
        );
        let body = std::fs::read_to_string(path).expect("事件文件可读");
        let v: serde_json::Value = serde_json::from_str(&body).expect("事件文件是合法 JSON");
        assert!(
            v["event"].as_str().is_some_and(|e| !e.is_empty()),
            "event 字段非空: {body}"
        );
    }
}

/// F8 实机自检（claude）：注册真实 helper 钩子 → 跑一次真实 claude 会话 →
/// 断言 **事件文件真落盘**（全链取证：注册形态 → CLI 触发 → helper 管道 →
/// `~/.tuvis/events/<session_id>.json`）。
///
/// 与 codex 版（[`codex_hook_events_really_fire_in_real_session`]）的三处差异，
/// 均为 claude 侧实机取证结论（2026-09-21）：
/// ① **沙箱靠 `--settings <file>` 而非 env**：claude 无 `CLAUDE_CONFIG_DIR` 语义
///    （实测该变量对 settings.json 定位无效），`--settings` 显式指向沙箱 JSON 是
///    零接触真实 `~/.claude` 的唯一干净路径；
/// ② **无信任门**：claude 不校验钩子来源（codex 0.155 的 untrusted 门是 codex 独有），
///    故本测试在裸环境即可全绿——这正是它与 codex 版最大的行为差；
/// ③ **print 模式（`-p`）不发 stdin 且不出发审批事件**（实测：`-p` 下 SessionStart
///    类钩子的 stdin 为空、PermissionRequest 不触发）——但 **SessionEnd 等生命周期
///    事件照常触发**且 stdin 完整，故本测试以「有事件落盘」为判据即可覆盖管道全链；
///    审批类事件的真触发归人工交互会话（验收清单 C-7/C-17 同族）。
///
/// 前置：claude 已安装（无需登录——实测未登录态钩子照常触发）；helper 已 debug
/// 构建（TUVIS_HOME 重定向仅 debug 生效，release helper 会写真实 ~/.tuvis）。
#[test]
#[ignore = "实机验证：跑真实 claude 会话验证事件落盘（前置=debug helper；claude 无需登录）"]
fn claude_hook_events_really_fire_in_real_session() {
    run_live_event_check(LiveTool::Claude);
}

/// F8 实机自检（kimi）：注册真实 helper 钩子 → 跑一次真实 kimi 会话 → 断言事件落盘。
///
/// kimi 侧四处实机取证结论（2026-09-21，全部并入 F8 台账）：
/// ① **沙箱靠 `KIMI_CODE_HOME`**（官方数据根重定向，config.toml 随根走）——但
///    **必须带可用模型配置**：空根下 kimi 直接 `No model configured` 退出、钩子
///    根本不进（实测），故本测试**复制真实 `~/.kimi-code/config.toml` 到沙箱**后
///    追加 `[[hooks]]`（只读复制、不写真实文件；config 内含 provider key，仅本地
///    拷贝不做任何外传，测试结束随 tempdir 回收）；
/// ② **session_id 是 `session_<uuid>`**（下划线前缀）——helper 白名单 F8 前拒收
///    该形态，kimi 事件曾全量静默丢弃（见 `hook_listener::session_id_allowed`）；
///    本测试即该修复的端到端回归锁；
/// ③ **stdin JSON 三家同形**（实测 kimi `-p` 模式：hook_event_name/session_id/cwd
///    齐备，与 claude/codex 同管道）——helper 薄管道零特判即兼容；
/// ④ **审批事件在无头模式不触发**（实测 `-p` 下 PermissionRequest/PermissionResult
///    静默，仅 SessionStart/UserPromptSubmit/Stop 触发）——故本测试**额外注册三个
///    生命周期事件**作为管道探针（kimi 生产注册面仍只有两个审批事件，不动），
///    审批事件的真触发归人工交互会话（验收清单三家矩阵）。
///
/// 前置：kimi 已安装且 `~/.kimi-code/config.toml` 有可用模型（否则沙箱复制后仍
/// `No model configured`，测试以清晰提示失败）；helper 已 debug 构建。
#[test]
#[ignore = "实机验证：跑真实 kimi 会话验证事件落盘（前置=debug helper + kimi 可用模型配置）"]
fn kimi_hook_events_really_fire_in_real_session() {
    run_live_event_check(LiveTool::Kimi);
}

/// F8 实机自检目标工具（两家共用同一取证骨架：沙箱装配 → 注册 → 跑会话 → 判落盘）
#[cfg(test)]
#[derive(Clone, Copy)]
enum LiveTool {
    Claude,
    Kimi,
}

#[cfg(test)]
impl LiveTool {
    fn tool_id(self) -> &'static str {
        match self {
            LiveTool::Claude => "claude",
            LiveTool::Kimi => "kimi",
        }
    }
    /// CLI 可执行名（PATH 上的入口；Windows 下 claude/kimi 均为 .cmd/裸 exe 形态）
    fn cli(self) -> &'static str {
        self.tool_id()
    }
    /// print 模式单回合命令（无头跑一回合即退；两家参数形态实测同构）
    fn print_args(self) -> Vec<&'static str> {
        vec!["-p", "Reply with the single word: ok"]
    }
}

/// 实机取证骨架（两家共用）：tempdir 沙箱（配置根 / 兔维斯 数据根 / 工作目录）→
/// 按生产规格注册全部事件 → spawn 真实 CLI 无头会话（env 注入沙箱根）→ 轮询
/// events 目录 → 断言落盘 + 形态合法。零接触真实 `~/.tuvis`（TUVIS_HOME 重定向）与
/// 真实工具配置（claude `--settings` / kimi 沙箱 KIMI_CODE_HOME）。
#[cfg(test)]
fn run_live_event_check(tool: LiveTool) {
    use crate::adapter::AgentAdapter;

    // 前置自检：CLI 在场（npm 全局目录补 PATH——后台/沙箱环境常缺）
    let npm_dir = std::env::var("USERPROFILE")
        .map(|u| format!("{}\\AppData\\Roaming\\npm", u))
        .unwrap_or_default();
    let aug_path = move |cmd: &mut std::process::Command| {
        let p = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{npm_dir};{p}"));
    };
    let spawn_cli = |args: &[&str]| -> std::io::Result<std::process::Output> {
        let mut bare = std::process::Command::new(tool.cli());
        bare.args(args).stdin(std::process::Stdio::null());
        aug_path(&mut bare);
        let out = bare.output();
        if out.is_ok() {
            return out;
        }
        // .cmd 垫片回退（CreateProcess 不解析 .cmd；codex 版同款教训）
        let mut sh = std::process::Command::new("cmd");
        sh.args(["/c", tool.cli()])
            .args(args)
            .stdin(std::process::Stdio::null());
        aug_path(&mut sh);
        sh.output()
    };
    let ver = spawn_cli(&["--version"])
        .unwrap_or_else(|e| panic!("{} 命令不可用（本测试需实机安装）：{e}", tool.cli()));
    assert!(ver.status.success(), "{} --version 失败", tool.cli());

    // helper 必须已 debug 构建（TUVIS_HOME 重定向仅 debug 生效）
    let exe_name = if cfg!(windows) {
        "tuvis-hook-listener.exe"
    } else {
        "tuvis-hook-listener"
    };
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
    let helper = target_dir.join("debug").join(exe_name);
    assert!(
        helper.is_file(),
        "helper 未构建：先跑 cargo build --bin tuvis-hook-listener --features hook-listener \
         （必须 debug 构建——TUVIS_HOME 重定向仅 debug 生效）"
    );

    let cfg_home = tempfile::tempdir().unwrap();
    let mam_home = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let adapter: &dyn AgentAdapter = match tool {
        LiveTool::Claude => &crate::adapter::claude::ClaudeAdapter,
        LiveTool::Kimi => &crate::adapter::kimi::KimiAdapter,
    };
    let events = adapter.hook_events();

    // ---- 注册：按生产规格（claude JSON / kimi TOML），helper 指向已构建实体 ----
    // claude 的注册目标不是 hooks 配置路径（settings.json）而是 --settings 文件；
    // kimi 的注册目标即沙箱 config.toml
    let settings_path = match tool {
        LiveTool::Claude => cfg_home.path().join("settings.json"),
        // kimi：复制真实 config.toml（含可用模型）后追加 [[hooks]]
        LiveTool::Kimi => {
            let real = dirs::home_dir()
                .unwrap_or_default()
                .join(".kimi-code")
                .join("config.toml");
            let dst = cfg_home.path().join("config.toml");
            let real_content = std::fs::read_to_string(&real).unwrap_or_else(|e| {
                panic!(
                    "读不到真实 kimi 配置 {}（前置：kimi 已装且有可用模型）：{e}",
                    real.display()
                )
            });
            std::fs::write(&dst, real_content).unwrap();
            dst
        }
    };
    let fake_script = cfg_home.path().join("status-hook.sh");
    let spec = hook_command_spec_for(tool.tool_id(), &fake_script, Some(&helper));
    match tool {
        LiveTool::Claude => {
            let matchers: Vec<(&str, &str)> = events
                .iter()
                .filter_map(|e| adapter.hook_event_matcher(e).map(|m| (*e, m)))
                .collect();
            register_hooks_for_tool(&settings_path, &events, true, &spec, &matchers)
                .expect("claude 沙箱注册失败");
        }
        LiveTool::Kimi => {
            // 探针事件并入（见 kimi 测试 doc ④）：审批事件无头模式不触发，注册面
            // 补三个生命周期事件做管道取证；生产注册面（hook_events）不变
            let mut probe: Vec<&str> = events.clone();
            for extra in ["SessionStart", "UserPromptSubmit", "Stop"] {
                if !probe.contains(&extra) {
                    probe.push(extra);
                }
            }
            register_kimi_hooks_for_tool(&settings_path, &probe, &spec).expect("kimi 沙箱注册失败");
        }
    }

    // ---- 跑真实无头会话（沙箱 env 注入）----
    let args = tool.print_args();
    let mut bare = std::process::Command::new(tool.cli());
    bare.args(&args)
        .env("TUVIS_HOME", mam_home.path())
        .env("KIMI_CODE_HOME", cfg_home.path())
        .current_dir(workdir.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let LiveTool::Claude = tool {
        bare.arg("--settings").arg(&settings_path);
    }
    aug_path(&mut bare);
    let mut sh = std::process::Command::new("cmd");
    sh.args(["/c", tool.cli()])
        .args(&args)
        .env("TUVIS_HOME", mam_home.path())
        .env("KIMI_CODE_HOME", cfg_home.path())
        .current_dir(workdir.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let LiveTool::Claude = tool {
        sh.arg("--settings").arg(&settings_path);
    }
    aug_path(&mut sh);
    let mut child = bare
        .spawn()
        .or_else(|_| sh.spawn())
        .unwrap_or_else(|e| panic!("{} 无头会话启动失败：{e}", tool.cli()));

    // ---- 轮询事件目录 ≤180s（与 codex 版同预算；会话自然退出后再判）----
    let events_dir = mam_home.path().join(".tuvis").join("events");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    let mut landed: Vec<std::path::PathBuf> = Vec::new();
    let mut early_exit = false;
    let mut cli_output = String::new();
    while std::time::Instant::now() < deadline {
        if let Ok(entries) = std::fs::read_dir(&events_dir) {
            landed = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "json"))
                .collect();
            if !landed.is_empty() {
                break;
            }
        }
        if let Ok(Some(_)) = child.try_wait() {
            early_exit = true;
            // 会话已退出：再给 2s 余量后按落盘结果判
            std::thread::sleep(std::time::Duration::from_secs(2));
            if let Ok(entries) = std::fs::read_dir(&events_dir) {
                landed = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "json"))
                    .collect();
            }
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    // 失败时给出 CLI 输出（kimi 的 No model configured 等前置问题一眼可辨）
    if landed.is_empty() {
        if let Ok(out) = child.wait_with_output() {
            cli_output = String::from_utf8_lossy(&out.stdout)
                .chars()
                .take(600)
                .collect();
        }
    } else {
        let _ = child.kill();
        let _ = child.wait();
    }

    let hint = match tool {
        LiveTool::Claude => {
            "排查：① claude 版本无 hooks 系统；② --settings 路径未被接受".to_string()
        }
        LiveTool::Kimi => format!(
            "排查：① 沙箱 config.toml 缺可用模型（复制自真实配置，见前置）；\
             ② kimi 版本无 [[hooks]] 支持；③ CLI 输出：{cli_output}"
        ),
    };
    assert!(
        !landed.is_empty(),
        "180s 内 ~/.tuvis/events 无事件文件落盘——{} 的钩子未触发（early_exit={early_exit}）。{hint}",
        tool.cli()
    );
    // 事件文件形态抽验：文件名即 session_id（白名单）、内容为读取侧格式
    for path in &landed {
        let sid = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        assert!(
            crate::monitor::hook_listener::session_id_allowed(sid),
            "落盘文件名必须是白名单 session_id: {sid:?}"
        );
        if matches!(tool, LiveTool::Kimi) {
            assert!(
                sid.starts_with("session_"),
                "kimi session_id 应为 session_<uuid> 形态（F8 实机取证）：{sid:?}"
            );
        }
        let body = std::fs::read_to_string(path).expect("事件文件可读");
        let v: serde_json::Value = serde_json::from_str(&body).expect("事件文件是合法 JSON");
        assert!(
            v["event"].as_str().is_some_and(|e| !e.is_empty()),
            "event 字段非空: {body}"
        );
    }
}

/// 批次丙 T1 实机自检（claude）：跑一次**交互式**沙箱会话驱动 AskUserQuestion，
/// 断言真实 Notification payload 的 `message` 落盘、并记录它与真实审批的判别力。
///
/// 与 [`claude_hook_events_really_fire_in_real_session`] 的差异：那条走 `-p` 无头
/// （只验管道通；审批/问答类事件在无头模式不发），本条必须**交互式 TUI**——因为
/// 只有真实待答才发 `Notification(permission_prompt)`。故本测试是**半自动**的：
/// 它装配沙箱 + 拉起交互式 claude + 注入一轮提示 + 轮询事件目录，最后由**人**在
/// 弹出的问题 UI 上作答/取消（或测试超时后 taskkill 清场）。
///
/// **实测结论（2026-09-21，本测试的取证来源）**：claude 对 AskUserQuestion 待答的
/// 事件序是 `PreToolUse(AUQ)` → `PermissionRequest(AUQ，带 tool_name+tool_input)`
/// → `Notification(permission_prompt，message="Claude needs your permission")`；
/// **真实审批**（如 Write）的序与字段完全同形，Notification.message 与前者
/// **逐字相同**——故 message 不具判别力，判据必须落在 tool_name 上
/// （adapter::is_question_entry_event）。完整档案：
/// research/refs/phase2-消息注入/2026-09-21-claude-notification-message-取证.md
///
/// 全程沙箱（`--settings` + TUVIS_HOME 重定向，零接触真实 `~/.claude` 与 `~/.tuvis`）。
/// 前置：claude 已安装；helper 已 debug 构建（TUVIS_HOME 重定向仅 debug 生效）。
#[test]
#[ignore = "实机验证：交互式 claude 会话驱动 AskUserQuestion，取证 Notification.message 原文（前置=debug helper + claude 已装；需人工作答）"]
fn claude_notification_message_really_fires_in_real_session() {
    // 前置：claude 在场（npm 全局目录补 PATH——后台/沙箱环境常缺 .cmd 垫片）
    let npm_dir = std::env::var("USERPROFILE")
        .map(|u| format!("{}\\AppData\\Roaming\\npm", u))
        .unwrap_or_default();
    let aug_path = move |cmd: &mut std::process::Command| {
        let p = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{npm_dir};{p}"));
    };
    // 启动器脚本里也要用（conhost 子进程不经过 aug_path）——单独留存一份
    let npm_dir_for_launcher = std::env::var("USERPROFILE")
        .map(|u| format!("{}\\AppData\\Roaming\\npm", u))
        .unwrap_or_default();
    let spawn_cli = |args: &[&str]| -> std::io::Result<std::process::Output> {
        let mut bare = std::process::Command::new("claude");
        bare.args(args).stdin(std::process::Stdio::null());
        aug_path(&mut bare);
        let out = bare.output();
        if out.is_ok() {
            return out;
        }
        let mut sh = std::process::Command::new("cmd");
        sh.args(["/c", "claude"])
            .args(args)
            .stdin(std::process::Stdio::null());
        aug_path(&mut sh);
        sh.output()
    };
    let ver = spawn_cli(&["--version"]).expect("claude 命令不可用——本测试需实机安装");
    assert!(ver.status.success(), "claude --version 失败");

    // helper 必须已 debug 构建（TUVIS_HOME 重定向仅 debug 生效）
    let exe_name = if cfg!(windows) {
        "tuvis-hook-listener.exe"
    } else {
        "tuvis-hook-listener"
    };
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
    let helper = target_dir.join("debug").join(exe_name);
    assert!(
        helper.is_file(),
        "helper 未构建：先跑 cargo build --bin tuvis-hook-listener --features hook-listener \
         （必须 debug 构建——TUVIS_HOME 重定向仅 debug 生效）"
    );

    // 沙箱：settings（空 matcher 收全量 Notification，便于取证）+ 兔维斯 数据根 + 工作目录。
    // **工作目录固定**（非 tempdir）：claude 的工作区信任门按**工程路径**记在真实
    // `~/.claude.json`，tempdir 每次变名 → 永远过不了门。固定路径使「信任一次、
    // 之后每次可跑」（与 codex 版的信任门处置同一先例：首次失败给出信任指引，这
    // 正是 C-17 验收项的自动形态）
    let cfg_home = tempfile::tempdir().unwrap();
    let mam_home = tempfile::tempdir().unwrap();
    let workdir = std::path::PathBuf::from(std::env::var("TEMP").unwrap_or_else(|_| ".".into()))
        .join("mam-t1-live-notif")
        .join("proj");
    std::fs::create_dir_all(&workdir).expect("固定探测工作目录创建失败");
    let settings_path = cfg_home.path().join("settings.json");
    let fake_script = cfg_home.path().join("status-hook.sh");
    let spec = hook_command_spec_for("claude", &fake_script, Some(&helper));
    let mut hooks = serde_json::Map::new();
    for ev in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Stop",
    ] {
        hooks.insert(
            ev.to_string(),
            serde_json::json!([{ "matcher": "", "hooks": [{ "type": "command", "command": spec.command }] }]),
        );
    }
    // Notification / PermissionRequest 用**空 matcher**（生产是 permission_prompt；
    // 取证要收全量，判据面才完整）
    for ev in ["Notification", "PermissionRequest"] {
        hooks.insert(
            ev.to_string(),
            serde_json::json!([{ "matcher": "", "hooks": [{ "type": "command", "command": spec.command }] }]),
        );
    }
    std::fs::write(
        &settings_path,
        serde_json::to_string_pretty(&serde_json::json!({ "hooks": hooks })).unwrap(),
    )
    .unwrap();

    // 拉起交互式 TUI（**不能**用 `-p`：实测无头模式根本不提供 AskUserQuestion 工具）。
    //
    // **启动形态的两个实机坑（本测试的教训）**：
    // ① Rust `Command::args` 会把整条命令行当**单个参数**加引号 →
    //    `cmd /k "<整条命令>"` 被当字面 token，claude 收不到参数。故用 `.cmd` 启动器
    //    把引号面收敛到一个路径（`cmd /k <launcher>`）；
    // ② Rust 直接 spawn `conhost.exe` 会**附着到测试进程自己的控制台**（TUI 转义
    //    序列泻进调用方终端）且 claude 拿不到干净 TTY。故经 PowerShell
    //    `Start-Process` 拉起（它创建独立控制台窗口，是探测套件验证过的路径）。
    //
    // 该 PowerShell 进程立即返回（-PassThru 仅用于记录 PID），claude 在独立控制台
    // 里跑；清场按「窗口标题/命令行匹配本测试沙箱路径」精确杀，不碰其他会话。
    let prompt =
        "Use the AskUserQuestion tool to ask which fruit I prefer, one question, two options.";
    let launcher = cfg_home.path().join("launch.cmd");
    // 启动器内**必须**先补 PATH（npm 全局目录放前面）：后台/cargo 环境常缺
    // `%USERPROFILE%\AppData\Roaming\npm`，`.cmd` 垫片解析不到 → claude 静默起不来
    std::fs::write(
        &launcher,
        format!(
            "@echo off\r\nset \"PATH={npm_dir_for_launcher};%PATH%\"\r\nclaude \"{prompt}\" --settings \"{}\"\r\n",
            settings_path.to_string_lossy()
        ),
    )
    .unwrap();
    let ps = format!(
        "$env:TUVIS_HOME='{}'; Start-Process -FilePath 'conhost.exe' -ArgumentList @('cmd.exe','/k','{}') -WorkingDirectory '{}'",
        mam_home.path().to_string_lossy(),
        launcher.to_string_lossy(),
        workdir.to_string_lossy(),
    );
    let ps_out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &ps])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("powershell 不可用（本测试需 Windows + PowerShell 拉起独立控制台）");
    assert!(
        ps_out.status.success(),
        "conhost 拉起失败：{}",
        String::from_utf8_lossy(&ps_out.stderr)
    );

    // 轮询事件目录 ≤240s：等 Notification 落盘（claude 冷启动 + 模型回合 ≈ 30s；
    // 预算留足慢模型）。命中即停——问题 UI 保持 pending，不需要人工作答。
    //
    // **记录所有观察到的版本**（不只看第一条）：本沙箱的 claude 会**同时**读沙箱
    // `--settings` 与用户真实 `~/.claude/settings.json`，后者若也注册了 兔维斯 钩子，
    // 就会出现**两个 helper 写同一个事件文件**（生产 helper 是 release 语义、无视
    // TUVIS_HOME，但它与沙箱 helper 可能落到不同目录；同目录时后写者胜）。故本测试
    // 对事件文件的**任一版本**做断言，而非假定唯一写者。
    let events_dir = mam_home.path().join(".tuvis").join("events");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
    let mut seen: Vec<String> = Vec::new();
    while std::time::Instant::now() < deadline {
        if let Ok(entries) = std::fs::read_dir(&events_dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "json") {
                    if let Ok(body) = std::fs::read_to_string(&p) {
                        if body.contains("\"event\":\"Notification\"") && !seen.contains(&body) {
                            seen.push(body);
                        }
                    }
                }
            }
        }
        // 停条件：Notification 已落盘即可停（承接版归属见文末说明，不作断言）
        if !seen.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    // 清场（八条铁律 6）：只杀本轮沙箱的 claude/cmd——按**命令行含本次 cfg 目录**
    // 精确匹配（每个测试用例的 tempdir 唯一），绝不碰用户自己的终端/会话
    let cfg_dir = cfg_home.path().to_string_lossy().replace('/', "\\");
    let kill_ps = format!(
        "Get-CimInstance Win32_Process -Filter \"Name='claude.exe'\" | Where-Object {{ $_.CommandLine -like '*{cfg_dir}*' }} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force }}; \
         Get-CimInstance Win32_Process -Filter \"Name='cmd.exe'\" | Where-Object {{ $_.CommandLine -like '*{cfg_dir}*' }} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force }}"
    );
    let _ = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &kill_ps,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    let body = seen
        .iter()
        .find(|b| b.contains("\"tool_name\":\"AskUserQuestion\""))
        .or_else(|| seen.first())
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "240s 内无 Notification 事件落盘——交互式会话未产生待答。\
                 排查：① conhost/cmd 未能拉起 claude（本测试需真实控制台）；\
                 ② claude 工作区信任门拦住了首轮（沙箱工程目录首次进入会弹信任提示，\
                 需先在真实 ~/.claude.json 的 projects 段预信任该目录）；\
                 ③ claude 版本无 Notification 钩子；④ 模型太慢（加大超时预算）"
            )
        });
    let v: serde_json::Value = serde_json::from_str(&body).expect("事件文件是合法 JSON");
    assert_eq!(v["event"], "Notification");
    // 实测锚点：message 原文（实机取证锚点；若 claude 改文案，此断言失败即提示
    // 需复核判据面——**判据本身不依赖该文案**，见测试 doc）
    assert_eq!(
        v["message"], "Claude needs your permission",
        "Notification.message 原文（实机取证锚点）: {body}"
    );
    // 判别力断言：message 不含工具名 → 单凭 message 无法判问答（这正是 T1 把判据
    // 落在 tool_name 上的实测依据）
    assert!(
        !v["message"]
            .as_str()
            .unwrap_or("")
            .contains(crate::monitor::hook_listener::ASK_USER_QUESTION_TOOL),
        "message 不得含工具名（否则可用它判问答；实机已证不含）: {body}"
    );
    // **T1 承接面**：本测试**不断言**承接结果——原因（实机发现的真实约束，属 T2
    // 范围）：用户的真实 `~/.claude/settings.json` 也注册了 兔维斯 钩子，claude 会把
    // `--settings` 与真实 settings **合并**触发，于是**两个 helper 写同一个事件文件**
    // （生产 helper 若为 debug 构建会同样认 TUVIS_HOME → 落同一沙箱目录），后写者胜。
    // 部署的 helper 滞后于本源码时，承接版会被覆盖成非承接版 → 断言必然 flaky。
    //
    // 承接的确定性验证在别处，且更可靠：
    // ① 单元/bin 测试（hook_listener::tests::notification_carries_forward_*、
    //    bin 的 run_carries_forward_pending_question_fields_into_notification）；
    // ② 取证档案里的**真实 payload 回放**（stdin-raw 提取原文 → 重放 → 承接生效）。
    // 此处只把观察到的版本全部打印出来，作为部署状态的诊断依据（若全部版本都缺
    // tool_name，说明生产 helper 未同步，正是 T2 要落实的事）。
    eprintln!(
        "[T1] 观察到的 Notification 事件版本数={}，版本列表：",
        seen.len()
    );
    for b in &seen {
        eprintln!("[T1]   {b}");
    }
    if !seen.iter().any(|b| b.contains("AskUserQuestion")) {
        eprintln!(
            "[T1] 警告：所有版本都未承接 AUQ 工具名——请确认生产 helper \
             （~/.tuvis/bin/tuvis-hook-listener.exe）已同步到含承接窗的版本（T2 职责）"
        );
    }
}

/// 批次丙 T2 实机自检：**部署链路**端到端——PreToolUse(AUQ) 载荷经**部署的**
/// `~/.tuvis/bin/tuvis-hook-listener.exe` 落盘，带 `tool_name` + `tool_input`。
///
/// 这是批次乙遗留的未验段：既有实机测试（`claude_hook_events_really_fire_*` 家族）
/// 全部把 helper 指向 **target/debug 的构建产物**，从未验证过 `~/.tuvis/bin/` 里
/// **部署的那一份**。而部署滞后正是图2/图3 的根因 2（`~/.tuvis/bin` 为 00:45 旧构建，
/// 事件无 tool_name → 问答分支永不识别）。
///
/// # 本测试断言什么
///
/// 1. 部署的 helper 存在（`~/.tuvis/bin/`）且**产物不旧于源码**（mtime 晚于本文件
///    所在 crate 的 Cargo.toml——粗粒度但足以捕获「旧构建」这一类真实故障）；
/// 2. 用**真实 AUQ payload**（形态取自 T1 实机取证的原始 stdin）经部署的 helper
///    回放 → 事件文件带 `tool_name="AskUserQuestion"` 且 `tool_input` 可解析出
///    questions。
///
/// # 为何是「回放」而非「拉起真实会话」
///
/// 真实会话驱动 AUQ 需要交互式 TUI + 人工作答（见 T1 测试），而本测试要验的是
/// **部署的那份二进制**——把真实 payload 喂给它，验的正是「部署产物的载荷能力」
/// 这一段链路（会话→helper 的那一段已由 T1 测试与通道 A 覆盖）。两段合起来即
/// 通道 A 端到端。
///
/// # 隔离与纪律（八条铁律）
///
/// 部署的 helper 是 **release 语义**（`app_data_home()` 的 TUVIS_HOME 重定向仅
/// `#[cfg(debug_assertions)]` 生效）——若它恰好是 debug 构建则认 TUVIS_HOME，若为
/// release 则写**真实** `~/.tuvis/events/`。为不污染真实目录，本测试用**专属探针
/// session_id**（`mam-t2-selftest-<pid>`）并在**测试末尾删除自己那一个文件**
/// （单点删除，绝不触碰其他事件文件）；这正是 T1 探测档案用过的处置纪律。
///
/// 前置：应用已启动过一次（`ensure_hook_script` → `install_helper_bin` 完成部署），
/// 或手动 `cp target/debug/tuvis-hook-listener.exe ~/.tuvis/bin/`。
#[test]
#[ignore = "实机验证：部署的 helper（~/.tuvis/bin）载荷能力——真实 AUQ payload 回放（前置=已部署）"]
fn deployed_helper_writes_question_channel_payload() {
    let exe_name = if cfg!(windows) {
        "tuvis-hook-listener.exe"
    } else {
        "tuvis-hook-listener"
    };
    let home = dirs::home_dir().expect("home_dir 可用");
    let deployed = home.join(".tuvis").join("bin").join(exe_name);
    assert!(
        deployed.is_file(),
        "部署的 helper 不在场：{}——先启动一次应用（ensure_hook_script 会安装），\
         或 cargo build --bin tuvis-hook-listener --features hook-listener 后手动 cp",
        deployed.display()
    );

    // 部署新鲜度：helper mtime 不应早于本 crate 的 Cargo.toml（粗粒度陈旧检测）
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let mtime = |p: &std::path::Path| -> Option<std::time::SystemTime> {
        std::fs::metadata(p).ok().and_then(|m| m.modified().ok())
    };
    if let (Some(dep_m), Some(src_m)) = (mtime(&deployed), mtime(&manifest)) {
        assert!(
            dep_m >= src_m,
            "部署的 helper（{}）早于源码 Cargo.toml（{}）——疑似旧构建（图2/图3 根因 2）。\
             重构建后重启应用，或手动 cp 覆盖",
            deployed.display(),
            manifest.display()
        );
    }

    // 真实 AUQ payload（形态取自 T1 实机取证的原始 stdin：PreToolUse ∧ AUQ ∧
    // 完整 tool_input.questions）
    let sid = format!("mam-t2-selftest-{}", std::process::id());
    let payload = format!(
        concat!(
            r#"{{"session_id":"{sid}","hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","cwd":"/w","#,
            r#""tool_input":{{"questions":[{{"header":"Next step","multiSelect":false,"#,
            r#""options":[{{"description":"d","label":"Tool demo"}}],"#,
            r#""question":"What would you like to do next?"}}]}}}}"#
        ),
        sid = sid
    );

    let out = std::process::Command::new(&deployed)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .expect("stdin 管道")
                .write_all(payload.as_bytes())?;
            child.wait_with_output()
        })
        .unwrap_or_else(|e| panic!("部署的 helper 执行失败：{e}"));

    // 红线 1 回归：helper 必须 exit 0 且 stdout 为空（codex 侧 exit≠0 会被当 Deny）
    assert!(
        out.status.success(),
        "部署的 helper 必须 exit 0（红线 1）；stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "部署的 helper 必须零 stdout（红线 1）；got={:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    // 事件文件定位：debug 构建认 TUVIS_HOME（未设则真实目录）；release 恒真实目录。
    // 两个候选都探，命中即用；测试末尾单点删除自己那一个
    let real_dir = home.join(".tuvis").join("events");
    let candidates = [real_dir.clone()];
    let mut body = None;
    let mut found_path = None;
    for dir in &candidates {
        let p = dir.join(format!("{sid}.json"));
        if let Ok(b) = std::fs::read_to_string(&p) {
            body = Some(b);
            found_path = Some(p);
            break;
        }
    }
    let body = body.unwrap_or_else(|| {
        panic!(
            "部署的 helper 未产出事件文件（查过：{}）——部署产物可能不是本构建",
            candidates
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    });
    let v: serde_json::Value = serde_json::from_str(&body).expect("事件文件是合法 JSON");
    assert_eq!(
        v["tool_name"], "AskUserQuestion",
        "部署的 helper 必须携带 tool_name（T8 载荷能力）: {body}"
    );
    let ti: serde_json::Value =
        serde_json::from_str(v["tool_input"].as_str().expect("tool_input 为 JSON 串"))
            .expect("tool_input 可解析");
    assert_eq!(
        ti["questions"][0]["options"][0]["label"], "Tool demo",
        "tool_input 必须含完整 questions 载荷（问答卡通道 A 的数据源）: {body}"
    );

    // 清场：单点删除本次探针事件文件（绝不触碰其他文件）
    if let Some(p) = found_path {
        let _ = std::fs::remove_file(p);
    }
}

/// T1 命令规格纯决策（windows_semantics 显式驱动，双平台语义任意平台可测）
#[cfg(test)]
mod helper_command_spec_tests {
    use super::{helper_command_for, hook_command_spec_for_impl};

    const SCRIPT: &str = r"C:\Users\u\.tuvis\hooks\status-hook.sh";
    const HELPER: &str = r"C:\Users\u\.tuvis\bin\tuvis-hook-listener.exe";

    #[test]
    fn helper_command_normalizes_slashes_and_quotes_spaces() {
        // 与 quote_bash_command 同一套实证结论（正斜杠两层安全 + 含空格保引号），
        // 去掉 bash 前缀——直启形态
        assert_eq!(
            helper_command_for(std::path::Path::new(HELPER)),
            "C:/Users/u/.tuvis/bin/tuvis-hook-listener.exe"
        );
        assert_eq!(
            helper_command_for(std::path::Path::new(
                r"C:\Users\John Doe\.tuvis\bin\tuvis-hook-listener.exe"
            )),
            "\"C:/Users/John Doe/.tuvis/bin/tuvis-hook-listener.exe\""
        );
    }

    #[test]
    fn claude_windows_semantics_spawns_helper_directly() {
        let spec = hook_command_spec_for_impl(
            "claude",
            std::path::Path::new(SCRIPT),
            Some(std::path::Path::new(HELPER)),
            true,
        );
        assert_eq!(
            spec.command, "C:/Users/u/.tuvis/bin/tuvis-hook-listener.exe",
            "claude 的 command 必须直启 helper（零 shell 依赖）"
        );
        assert!(
            spec.command_windows.is_none(),
            "claude 无 commandWindows 字段"
        );
        assert_eq!(spec.helper_path.as_deref(), Some(HELPER));
    }

    #[test]
    #[cfg_attr(
        not(windows),
        ignore = "断言 Windows 形态（bash 包装/反斜杠分隔符语义）；非 Windows 平台行为另测"
    )]
    fn codex_windows_semantics_keeps_bash_command_and_sets_command_windows() {
        let spec = hook_command_spec_for_impl(
            "codex",
            std::path::Path::new(SCRIPT),
            Some(std::path::Path::new(HELPER)),
            true,
        );
        assert_eq!(
            spec.command, "bash C:/Users/u/.tuvis/hooks/status-hook.sh",
            "codex 的 command 恒为 bash 形态（非 Windows 落地用，行为不变）"
        );
        assert_eq!(
            spec.command_windows.as_deref(),
            Some("C:/Users/u/.tuvis/bin/tuvis-hook-listener.exe"),
            "Windows 走官方 commandWindows 直启 helper（cmd 包装下裸 bash 不可解析的根治）"
        );
        assert_eq!(spec.helper_path.as_deref(), Some(HELPER));
    }

    #[test]
    fn unix_semantics_keep_bash_form_without_windows_field() {
        // windows_semantics=false：两工具 command 都回到 hook_command_for 产物
        // （平台原生的 bash 形态——本断言以同一函数为期望，不拼平台路径），
        // 且不得携带 commandWindows / helper 标记（unix 行为与 T1 前完全一致）
        for tool in ["claude", "codex"] {
            let spec = hook_command_spec_for_impl(
                tool,
                std::path::Path::new(SCRIPT),
                Some(std::path::Path::new(HELPER)),
                false,
            );
            assert_eq!(
                spec.command,
                super::hook_command_for(std::path::Path::new(SCRIPT)),
                "unix 语义 = bash 兜底形态"
            );
            assert_eq!(spec.command_windows, None);
            assert_eq!(spec.helper_path, None);
        }
    }

    #[test]
    #[cfg_attr(
        not(windows),
        ignore = "断言 Windows 形态（bash 包装/反斜杠分隔符语义）；非 Windows 平台行为另测"
    )]
    fn helper_absent_falls_back_to_bash_on_both_platforms() {
        // helper 未随包分发是合法状态：Windows 语义也必须回落 bash 形态（零回归）
        for (tool, win) in [
            ("claude", true),
            ("codex", true),
            ("claude", false),
            ("codex", false),
        ] {
            let spec = hook_command_spec_for_impl(tool, std::path::Path::new(SCRIPT), None, win);
            assert_eq!(spec.command, "bash C:/Users/u/.tuvis/hooks/status-hook.sh");
            assert_eq!(spec.command_windows, None);
            assert_eq!(spec.helper_path, None);
        }
    }

    #[test]
    fn kimi_semantics_direct_helper_without_windows_field() {
        // T2：kimi + helper → command 直启 helper（[[hooks]] 无 commandWindows
        // 字段，两平台同形）；helper 缺席 → 兜底 bash 形态（但生产侧 register_all_hooks
        // 对 kimi+helper 缺席直接跳过注册，兜底规格不会落盘——见 impl 注释）
        let spec = hook_command_spec_for_impl(
            "kimi",
            std::path::Path::new(SCRIPT),
            Some(std::path::Path::new(HELPER)),
            true,
        );
        assert_eq!(
            spec.command, "C:/Users/u/.tuvis/bin/tuvis-hook-listener.exe",
            "kimi 的 command 必须直启 helper"
        );
        assert_eq!(
            spec.command_windows, None,
            "kimi 官方 [[hooks]] 无 commandWindows 字段"
        );
        assert_eq!(spec.helper_path.as_deref(), Some(HELPER));
        // unix 语义同形（同一 command 落地）
        let unix = hook_command_spec_for_impl(
            "kimi",
            std::path::Path::new(SCRIPT),
            Some(std::path::Path::new(HELPER)),
            false,
        );
        assert_eq!(unix.command, spec.command);
    }

    #[test]
    #[cfg_attr(
        not(windows),
        ignore = "断言 Windows 形态（bash 包装/反斜杠分隔符语义）；非 Windows 平台行为另测"
    )]
    fn other_tools_get_bash_fallback_even_with_helper() {
        // 未接 hook 通道的工具（opencode 等）：规格兜底 bash 形态（T2 起 kimi 已
        // 接入自有通道，不再是兜底成员）
        let spec = hook_command_spec_for_impl(
            "opencode",
            std::path::Path::new(SCRIPT),
            Some(std::path::Path::new(HELPER)),
            true,
        );
        assert_eq!(spec.command, "bash C:/Users/u/.tuvis/hooks/status-hook.sh");
        assert_eq!(spec.command_windows, None);
        assert_eq!(spec.helper_path, None);
    }
}

/// T1 存量迁移（register_hooks_in_file 判据扩展：脚本路径 + helper 路径双标记）
#[cfg(test)]
mod helper_migration_tests {
    use super::{register_hooks_in_file, HookCommandSpec};

    const SCRIPT: &str = r"C:\Users\u\.tuvis\hooks\status-hook.sh";
    const HELPER: &str = r"C:\Users\u\.tuvis\bin\tuvis-hook-listener.exe";
    const HELPER_CMD: &str = "C:/Users/u/.tuvis/bin/tuvis-hook-listener.exe";

    fn our_entry(cmd: &str) -> serde_json::Value {
        serde_json::json!({ "matcher": "", "hooks": [{ "type": "command", "command": cmd }] })
    }

    fn claude_spec() -> HookCommandSpec {
        HookCommandSpec {
            command: HELPER_CMD.to_string(),
            command_windows: None,
            helper_path: Some(HELPER.to_string()),
        }
    }

    fn codex_spec() -> HookCommandSpec {
        HookCommandSpec {
            command: "bash C:/Users/u/.tuvis/hooks/status-hook.sh".to_string(),
            command_windows: Some(HELPER_CMD.to_string()),
            helper_path: Some(HELPER.to_string()),
        }
    }

    /// claude 形态迁移：settings.json 旧 bash 条目（正斜杠历史形态）→ command
    /// 原地改写为 helper 直启命令；用户条目不受影响
    #[test]
    fn claude_bash_entry_migrates_to_helper_command() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("settings.json");
        std::fs::write(
            &cfg,
            serde_json::json!({"hooks": {"Stop": [
                our_entry("bash C:/Users/u/.tuvis/hooks/status-hook.sh"),
                { "matcher": "Bash", "hooks": [{ "type": "command", "command": "user-own" }] }
            ]}})
            .to_string(),
        )
        .unwrap();

        let (added, migrated) =
            register_hooks_in_file(&cfg, &["Stop"], true, SCRIPT, &claude_spec(), &[]).unwrap();
        assert_eq!((added, migrated), (0, 1), "应为纯迁移零新增");
        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let entry = &out["hooks"]["Stop"][0]["hooks"][0];
        assert_eq!(
            entry["command"], HELPER_CMD,
            "command 必须改写为 helper 直启"
        );
        assert!(
            entry.get("commandWindows").is_none(),
            "claude 条目不得带 commandWindows 字段"
        );
        assert_eq!(
            out["hooks"]["Stop"][1]["hooks"][0]["command"], "user-own",
            "用户条目必须原样保留"
        );
    }

    /// codex 形态迁移：hooks.json 旧 bash 条目 → command 保持 bash（非 Windows
    /// 落地用）+ 注入官方 commandWindows 字段指向 helper
    #[test]
    fn codex_bash_entry_gains_command_windows() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("hooks.json");
        std::fs::write(
            &cfg,
            serde_json::json!({"hooks": {"Stop": [our_entry("bash C:/Users/u/.tuvis/hooks/status-hook.sh")]}})
                .to_string(),
        )
        .unwrap();

        let (added, migrated) =
            register_hooks_in_file(&cfg, &["Stop"], true, SCRIPT, &codex_spec(), &[]).unwrap();
        assert_eq!((added, migrated), (0, 1), "commandWindows 注入计入迁移");
        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let entry = &out["hooks"]["Stop"][0]["hooks"][0];
        assert_eq!(
            entry["command"], "bash C:/Users/u/.tuvis/hooks/status-hook.sh",
            "codex 的 command 保持 bash 形态（unix 行为不变）"
        );
        assert_eq!(entry["commandWindows"], HELPER_CMD);
    }

    /// codex 完全迁移后的稳定态：command + commandWindows 均与规格一致 → already
    /// 跳过、零改动（文件不重写，字节不变）
    #[test]
    fn codex_fully_migrated_entry_is_stable() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("hooks.json");
        let before = serde_json::json!({"hooks": {"Stop": [serde_json::json!({
            "matcher": "",
            "hooks": [{ "type": "command", "command": "bash C:/Users/u/.tuvis/hooks/status-hook.sh",
                        "commandWindows": HELPER_CMD }]
        })]}})
        .to_string();
        std::fs::write(&cfg, &before).unwrap();

        let (added, migrated) =
            register_hooks_in_file(&cfg, &["Stop"], true, SCRIPT, &codex_spec(), &[]).unwrap();
        assert_eq!((added, migrated), (0, 0), "稳定态零新增零迁移");
        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            before,
            "稳定态不得重写文件"
        );
    }

    /// helper 路径变更（升级换位置）等场景：旧 commandWindows 值原地刷新
    #[test]
    fn stale_command_windows_value_is_refreshed() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("hooks.json");
        std::fs::write(
            &cfg,
            serde_json::json!({"hooks": {"Stop": [our_entry("bash C:/Users/u/.tuvis/hooks/status-hook.sh")]}})
                .to_string(),
        )
        .unwrap();
        // 第一轮：注入 commandWindows
        register_hooks_in_file(&cfg, &["Stop"], true, SCRIPT, &codex_spec(), &[]).unwrap();
        // 第二轮（规格换新 helper 路径）：旧值必须被刷新，不产生双条目
        let new_spec = HookCommandSpec {
            command: "bash C:/Users/u/.tuvis/hooks/status-hook.sh".to_string(),
            command_windows: Some("C:/new/bin/tuvis-hook-listener.exe".to_string()),
            helper_path: Some(r"C:\new\bin\tuvis-hook-listener.exe".to_string()),
        };
        let (added, migrated) =
            register_hooks_in_file(&cfg, &["Stop"], true, SCRIPT, &new_spec, &[]).unwrap();
        assert_eq!((added, migrated), (0, 1));
        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let entries = out["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "不得追加双条目");
        assert_eq!(
            entries[0]["hooks"][0]["commandWindows"],
            "C:/new/bin/tuvis-hook-listener.exe"
        );
    }

    /// 非我方条目不动（helper 形态含脚本路径误写用户命令的边界也不误伤——判据
    /// 是「含标记」而非「等于标记」与 F3 语义一致，但用户命令不含任何标记）
    #[test]
    fn non_mam_entries_are_untouched_and_still_get_ours_appended() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("settings.json");
        std::fs::write(
            &cfg,
            serde_json::json!({"hooks": {"Stop": [
                { "matcher": "", "hooks": [{ "type": "command", "command": "my-own-listener" }] }
            ]}})
            .to_string(),
        )
        .unwrap();

        let (added, migrated) =
            register_hooks_in_file(&cfg, &["Stop"], true, SCRIPT, &claude_spec(), &[]).unwrap();
        assert_eq!((added, migrated), (1, 0), "仅追加我方条目");
        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let entries = out["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0]["hooks"][0]["command"], "my-own-listener",
            "用户条目不动"
        );
        assert_eq!(entries[1]["hooks"][0]["command"], HELPER_CMD);
    }

    /// 空配置冷注册：claude 规格直接落 helper 命令条目
    #[test]
    fn fresh_claude_registration_writes_helper_command() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("settings.json");
        let (added, migrated) =
            register_hooks_in_file(&cfg, &["Stop"], true, SCRIPT, &claude_spec(), &[]).unwrap();
        assert_eq!((added, migrated), (1, 0));
        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(out["hooks"]["Stop"][0]["hooks"][0]["command"], HELPER_CMD);
    }
}

/// T1 helper 安装管道（install_marker_helper 先例扩展的共用核心）
#[cfg(test)]
mod helper_install_tests {
    use super::{helper_install_dir, install_helper_from_to};

    #[test]
    fn install_dir_is_home_mam_bin() {
        let home = std::path::Path::new("/home/u");
        assert_eq!(
            helper_install_dir(home),
            std::path::Path::new("/home/u").join(".tuvis").join("bin")
        );
    }

    #[test]
    fn copies_first_existing_candidate_with_dotted_names() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        // 两个候选都在：Windows 发行 .exe 序在前（与 tuvis-marker 候选序一致）
        std::fs::write(src.path().join("tuvis-hook-listener.exe"), b"exe").unwrap();
        std::fs::write(src.path().join("tuvis-hook-listener"), b"bare").unwrap();
        let names = ["tuvis-hook-listener.exe", "tuvis-hook-listener"];
        let installed =
            install_helper_from_to(src.path(), dst.path(), &names).expect("候选在场必须安装成功");
        assert_eq!(
            installed,
            dst.path().join("tuvis-hook-listener.exe"),
            "按候选序取首个存在者"
        );
        assert_eq!(
            std::fs::read(dst.path().join("tuvis-hook-listener.exe")).unwrap(),
            b"exe"
        );
    }

    /// **已有安装副本回退**（2026-10-03 事故修复回归锁）：源目录无候选 + dst 已有
    /// 先前安装的副本 → 返回该副本（而非 None → bash 规格回落改写 settings）。
    /// 还原动作（变异）：删掉 dst 回退循环 → 返回 None、本用例先红。
    #[test]
    fn falls_back_to_existing_installed_copy_when_src_absent() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        // src 空（无 feature 构建形态）；dst 有先前安装的副本
        std::fs::write(dst.path().join("tuvis-hook-listener.exe"), b"installed").unwrap();
        let names = ["tuvis-hook-listener.exe", "tuvis-hook-listener"];
        let installed = install_helper_from_to(src.path(), dst.path(), &names)
            .expect("dst 已有副本必须回退成功");
        assert_eq!(
            installed,
            dst.path().join("tuvis-hook-listener.exe"),
            "回退到已有安装副本"
        );
        assert_eq!(
            std::fs::read(dst.path().join("tuvis-hook-listener.exe")).unwrap(),
            b"installed",
            "回退不覆盖既有副本"
        );
    }

    #[test]
    fn falls_back_to_bare_name_when_exe_absent() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tuvis-hook-listener"), b"bare").unwrap();
        let names = ["tuvis-hook-listener.exe", "tuvis-hook-listener"];
        let installed = install_helper_from_to(src.path(), dst.path(), &names)
            .expect("裸名候选在场必须安装成功");
        assert_eq!(installed, dst.path().join("tuvis-hook-listener"));
    }

    #[test]
    fn missing_candidates_yield_none_and_dst_dir_created() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let target = dst.path().join("bin");
        assert!(
            install_helper_from_to(src.path(), &target, &["tuvis-hook-listener.exe"]).is_none(),
            "候选全缺 → None（注册回落 bash 形态的合法状态）"
        );
        assert!(target.is_dir(), "目标目录仍应创建（幂等分发语义）");
    }

    #[test]
    fn overwrite_keeps_installed_helper_fresh() {
        // 升级覆盖语义：无条件重拷保证新版 helper 生效（tuvis-marker 先例）
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let names = ["tuvis-hook-listener.exe"];
        std::fs::write(src.path().join("tuvis-hook-listener.exe"), b"v1").unwrap();
        install_helper_from_to(src.path(), dst.path(), &names).unwrap();
        std::fs::write(src.path().join("tuvis-hook-listener.exe"), b"v2").unwrap();
        install_helper_from_to(src.path(), dst.path(), &names).unwrap();
        assert_eq!(
            std::fs::read(dst.path().join("tuvis-hook-listener.exe")).unwrap(),
            b"v2",
            "重装必须覆盖旧版"
        );
    }
}

/// T1 事件格式 round-trip：helper 产出的事件文件必须被读取侧原样认得
#[cfg(test)]
mod helper_event_roundtrip_tests {
    use super::read_hook_events_from;
    use crate::monitor::hook_listener::{event_body, now_unix, parse_hook_stdin, write_event_file};

    /// 写侧（helper 内核）→ 读侧（read_hook_events_from）全链：文件名键 / event /
    /// ts / last_event_at 逐字段一致；同会话覆盖写后读到最新
    #[test]
    fn helper_event_file_roundtrips_through_reader() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_payload = r#"{"session_id":"01a08083-5ca0-4948-8276-9a0b8c7d6e5f",
            "hook_event_name":"Stop","cwd":"E:\\proj","transcript_path":"/x.jsonl"}"#;
        let parsed = parse_hook_stdin(claude_payload).expect("claude 样本必须可解析");

        let ts1 = now_unix();
        write_event_file(tmp.path(), &parsed, ts1).unwrap();
        let m = read_hook_events_from(tmp.path());
        assert_eq!(m.len(), 1, "单会话单卡");
        let ev = m
            .get("01a08083-5ca0-4948-8276-9a0b8c7d6e5f")
            .expect("文件名即键");
        assert_eq!(ev.event, "Stop");
        assert_eq!(ev.ts, ts1);
        assert_eq!(
            ev.last_event_at,
            event_body(&parsed, ts1)
                .split_once("\"last_event_at\":\"")
                .and_then(|(_, rest)| rest.split('"').next())
                .unwrap_or_default(),
            "last_event_at 与写侧同源"
        );

        // 覆盖写（同会话新事件）→ 读取侧拿到最新 ts（保留最新状态语义）
        let ts2 = ts1 + 2;
        write_event_file(tmp.path(), &parsed, ts2).unwrap();
        let m = read_hook_events_from(tmp.path());
        assert_eq!(m["01a08083-5ca0-4948-8276-9a0b8c7d6e5f"].ts, ts2);
    }

    /// 白名单/临时文件边界：helper 不会为非法 sid 落盘；原子写中间态（*.tmp）
    /// 永不入读取侧键集
    #[test]
    fn reader_skips_helper_tmp_files_and_illegal_names() {
        let tmp = tempfile::tempdir().unwrap();
        let ts = now_unix();
        let body = format!(
            r#"{{"event":"Stop","session_id":"sid-ok","cwd":"","ts":{ts},"last_event_at":"2026-09-20T00:00:00Z"}}"#
        );
        std::fs::write(tmp.path().join("sid-ok.json"), &body).unwrap();
        // helper 原子写中间态形态（<sid>.<pid>.tmp）
        std::fs::write(tmp.path().join("sid-ok.424242.tmp"), &body).unwrap();
        // 非法 sid 文件（路径注入形态）
        std::fs::write(tmp.path().join("bad.name.json"), &body).unwrap();

        let m = read_hook_events_from(tmp.path());
        assert_eq!(m.len(), 1, "只认白名单 sid 的 .json: {:?}", m.keys());
        assert!(m.contains_key("sid-ok"));
    }
}

/// T2 审批事件注册扩展（批次甲，issue #74）：三家注册形态快照 + codex 新事件
/// 存量迁移扩展 + kimi TOML 注册 round-trip。全部 tempdir 缝，零接触真实 ~/.tuvis
#[cfg(test)]
mod t2_approval_registration_tests {
    use super::{
        hooks_toml_verified, register_hooks_in_file, register_kimi_hooks_in_file, HookCommandSpec,
    };
    use crate::adapter::AgentAdapter;
    use crate::adapter::{claude::ClaudeAdapter, codex::CodexAdapter, kimi::KimiAdapter};

    const SCRIPT: &str = r"C:\Users\u\.tuvis\hooks\status-hook.sh";
    const HELPER: &str = r"C:\Users\u\.tuvis\bin\tuvis-hook-listener.exe";
    const HELPER_CMD: &str = "C:/Users/u/.tuvis/bin/tuvis-hook-listener.exe";
    const BASH_CMD: &str = "bash C:/Users/u/.tuvis/hooks/status-hook.sh";

    fn claude_spec() -> HookCommandSpec {
        HookCommandSpec {
            command: HELPER_CMD.to_string(),
            command_windows: None,
            helper_path: Some(HELPER.to_string()),
        }
    }

    fn codex_spec() -> HookCommandSpec {
        HookCommandSpec {
            command: BASH_CMD.to_string(),
            command_windows: Some(HELPER_CMD.to_string()),
            helper_path: Some(HELPER.to_string()),
        }
    }

    fn kimi_spec() -> HookCommandSpec {
        HookCommandSpec {
            command: HELPER_CMD.to_string(),
            command_windows: None,
            helper_path: Some(HELPER.to_string()),
        }
    }

    /// 生产同款 matcher 注册面（register_all_hooks 的 filter_map 同构）
    fn adapter_matchers(
        adapter: &dyn AgentAdapter,
        events: &[&'static str],
    ) -> Vec<(&'static str, &'static str)> {
        events
            .iter()
            .filter_map(|e| adapter.hook_event_matcher(e).map(|m| (*e, m)))
            .collect()
    }

    // ---------- 事件清单快照（三家） ----------

    #[test]
    fn hook_event_lists_snapshot() {
        // claude：六既有事件 + T2 三事件（PostToolUseFailure 清除链补齐 /
        // PermissionRequest 即时信号 / Notification 晚 6s 文本信号）
        assert_eq!(
            ClaudeAdapter.hook_events(),
            vec![
                "Stop",
                "UserPromptSubmit",
                "SessionStart",
                "SessionEnd",
                "PreToolUse",
                "PostToolUse",
                "PostToolUseFailure",
                "PermissionRequest",
                "Notification",
            ]
        );
        // codex：六既有事件 + T2 双事件（PermissionRequest / Interrupt）
        assert_eq!(
            CodexAdapter.hook_events(),
            vec![
                "Stop",
                "UserPromptSubmit",
                "SessionStart",
                "SessionEnd",
                "PreToolUse",
                "PostToolUse",
                "PermissionRequest",
                "Interrupt",
            ]
        );
        // kimi：T2 新接入（审批进入 + 审批完成）
        assert_eq!(
            KimiAdapter.hook_events(),
            vec!["PermissionRequest", "PermissionResult"]
        );
        // 三家键形态全 PascalCase（kimi/codex 由 F3/T2 定案）
        for a in [
            ClaudeAdapter.hook_event_case(),
            CodexAdapter.hook_event_case(),
            KimiAdapter.hook_event_case(),
        ] {
            assert_eq!(a, crate::adapter::HookEventCase::PascalCase);
        }
    }

    #[test]
    fn matcher_surface_is_claude_notification_only() {
        let events = ClaudeAdapter.hook_events();
        for e in &events {
            let want = if *e == "Notification" {
                Some("permission_prompt")
            } else {
                None
            };
            assert_eq!(ClaudeAdapter.hook_event_matcher(e), want, "claude {e}");
        }
        for e in CodexAdapter.hook_events() {
            assert_eq!(CodexAdapter.hook_event_matcher(e), None, "codex {e}");
        }
        for e in KimiAdapter.hook_events() {
            assert_eq!(KimiAdapter.hook_event_matcher(e), None, "kimi {e}");
        }
    }

    // ---------- claude 注册 JSON 快照（逐字段） ----------

    #[test]
    fn claude_registration_snapshot_fields() {
        let events = ClaudeAdapter.hook_events();
        let matchers = adapter_matchers(&ClaudeAdapter, &events);
        assert_eq!(matchers, vec![("Notification", "permission_prompt")]);

        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("settings.json");
        register_hooks_in_file(&cfg, &events, true, SCRIPT, &claude_spec(), &matchers).unwrap();

        let raw = std::fs::read_to_string(&cfg).unwrap();
        // 红线 3：注册形态零 async（codex 跳过 async 钩子；claude 同样不需要）
        assert!(!raw.contains("async"), "注册 JSON 不得出现 async: {raw}");
        assert!(
            !raw.contains("commandWindows"),
            "claude 官方无 commandWindows 字段"
        );
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let hooks = v["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), events.len(), "每事件恰一我方条目组");
        for e in &events {
            let entry = &hooks[*e][0];
            let want_matcher = if *e == "Notification" {
                "permission_prompt"
            } else {
                ""
            };
            assert_eq!(entry["matcher"], want_matcher, "{e} 的 matcher");
            let h = &entry["hooks"][0];
            assert_eq!(h["type"], "command");
            assert_eq!(h["command"], HELPER_CMD, "{e} 直启 helper");
            assert!(h.get("commandWindows").is_none(), "{e} 无 Windows 覆盖");
            assert!(h.get("async").is_none(), "{e} 无 async 字段");
        }
        // Notification 的 matcher 注册形态与官方同构：事件键 → [{matcher, hooks}]
        assert_eq!(hooks["Notification"][0]["matcher"], "permission_prompt");
    }

    // ---------- codex 注册 JSON 快照（逐字段） ----------

    #[test]
    fn codex_registration_snapshot_fields() {
        let events = CodexAdapter.hook_events();
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("hooks.json");
        register_hooks_in_file(&cfg, &events, true, SCRIPT, &codex_spec(), &[]).unwrap();

        let raw = std::fs::read_to_string(&cfg).unwrap();
        // 红线 3：零 async（T1 已满足，注册键扩展后必须仍然成立）
        assert!(!raw.contains("async"), "注册 JSON 不得出现 async: {raw}");
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let hooks = v["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), events.len());
        for e in &events {
            let entry = &hooks[*e][0];
            assert_eq!(entry["matcher"], "", "codex 审批事件无需 matcher");
            let h = &entry["hooks"][0];
            assert_eq!(h["type"], "command");
            // codex 形态：command 恒 bash（非 Windows 落地）+ commandWindows 直启
            assert_eq!(h["command"], BASH_CMD);
            assert_eq!(h["commandWindows"], HELPER_CMD);
            assert!(h.get("async").is_none());
        }
        // T2 双事件确在场（PascalCase 键）
        assert!(hooks.contains_key("PermissionRequest"));
        assert!(hooks.contains_key("Interrupt"));
    }

    // ---------- codex 新增事件的存量 camelCase 键迁移（T2 扩迁移用例） ----------

    #[test]
    fn codex_new_events_legacy_camel_keys_migrate() {
        // 存量文件：T2 新事件的旧 camelCase 键（permissionRequest/interrupt，形如
        // F3 前 camelCase 注册期的产物）+ 我方 bash 条目 → 注册后必须改键为
        // PascalCase 且旧键清除；我方标记集同时认得旧条目（迁移）与新条目（跳过）
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("hooks.json");
        let legacy = serde_json::json!({"hooks": {
            "permissionRequest": [serde_json::json!({
                "matcher": "", "hooks": [{"type": "command", "command": BASH_CMD}]
            })],
            "interrupt": [serde_json::json!({
                "matcher": "", "hooks": [{"type": "command", "command": BASH_CMD}]
            })],
        }});
        std::fs::write(&cfg, legacy.to_string()).unwrap();

        let (added, migrated) = register_hooks_in_file(
            &cfg,
            &["PermissionRequest", "Interrupt"],
            true,
            SCRIPT,
            &codex_spec(),
            &[],
        )
        .unwrap();
        assert_eq!(
            (added, migrated),
            (2, 2),
            "纯存量迁移：旧 camelCase 键清除 2 处（计入迁移）+ 新 PascalCase 键追加 2 条——跨键移除不参与 skip 守卫（复评 P1-1 语义），迁移经「删旧键+建新键」两步完成"
        );

        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let hooks = out["hooks"].as_object().unwrap();
        assert!(hooks.contains_key("PermissionRequest"), "{hooks:?}");
        assert!(hooks.contains_key("Interrupt"), "{hooks:?}");
        assert!(!hooks.contains_key("permissionRequest"));
        assert!(!hooks.contains_key("interrupt"));
        assert_eq!(
            hooks["PermissionRequest"][0]["hooks"][0]["commandWindows"], HELPER_CMD,
            "迁移顺带补齐 commandWindows（T1 形态）"
        );
        // 迁移后再注册 → 稳定态（零新增零迁移）
        let (added2, migrated2) = register_hooks_in_file(
            &cfg,
            &["PermissionRequest", "Interrupt"],
            true,
            SCRIPT,
            &codex_spec(),
            &[],
        )
        .unwrap();
        assert_eq!((added2, migrated2), (0, 0));
    }

    // ---------- kimi TOML 注册 round-trip ----------

    #[test]
    fn kimi_toml_registration_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config.toml");
        // 前置：用户已有 config.toml（顶层键 + 自有 [[hooks]] 条目）——保注释保
        // 格式编辑下必须原样保留
        std::fs::write(
            &cfg,
            "model = \"k2\"\n# 用户注释\n\n[[hooks]]\nevent = \"Stop\"\ncommand = \"user-own.sh\"\n",
        )
        .unwrap();

        let events = KimiAdapter.hook_events();
        let (added, migrated) =
            register_kimi_hooks_in_file(&cfg, &events, SCRIPT, &kimi_spec()).unwrap();
        assert_eq!((added, migrated), (2, 0), "两审批事件全新增");

        // round-trip：toml crate 独立解析（非写侧 toml_edit 自证）
        let raw = std::fs::read_to_string(&cfg).unwrap();
        assert!(!raw.contains("async"), "TOML 注册形态零 async: {raw}");
        let parsed: toml::Value = toml::from_str(&raw).unwrap();
        let hooks = parsed["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 3, "用户条目 + 两我方条目");
        assert_eq!(
            hooks[0]["event"].as_str(),
            Some("Stop"),
            "用户条目在前且原样保留"
        );
        assert_eq!(hooks[0]["command"].as_str(), Some("user-own.sh"));
        for (i, e) in events.iter().enumerate() {
            assert_eq!(hooks[i + 1]["event"].as_str(), Some(*e));
            assert_eq!(hooks[i + 1]["command"].as_str(), Some(HELPER_CMD));
            assert!(hooks[i + 1].get("matcher").is_none(), "不写多余字段");
        }
        assert_eq!(parsed["model"].as_str(), Some("k2"), "顶层键保留");
        assert!(raw.contains("# 用户注释"), "注释保留（toml_edit 语义）");

        // 核验判据：全事件在场 → 通过
        assert!(hooks_toml_verified(&raw, HELPER_CMD, &events));
        // 幂等：再注册零新增零迁移、字节不变
        let before = std::fs::read_to_string(&cfg).unwrap();
        let (added2, migrated2) =
            register_kimi_hooks_in_file(&cfg, &events, SCRIPT, &kimi_spec()).unwrap();
        assert_eq!((added2, migrated2), (0, 0));
        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            before,
            "稳定态不重写"
        );
    }

    #[test]
    #[cfg_attr(
        not(windows),
        ignore = "断言 Windows 形态（bash 包装/反斜杠分隔符语义）；非 Windows 平台行为另测"
    )]
    fn kimi_stale_helper_command_is_refreshed_in_place() {
        // helper 换位升级：旧 command 原地刷新、不追加双条目
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config.toml");
        std::fs::write(&cfg, "").unwrap();
        let events = KimiAdapter.hook_events();
        register_kimi_hooks_in_file(&cfg, &events, SCRIPT, &kimi_spec()).unwrap();

        let new_spec = HookCommandSpec {
            command: "C:/new/bin/tuvis-hook-listener.exe".to_string(),
            command_windows: None,
            helper_path: Some(r"C:\new\bin\tuvis-hook-listener.exe".to_string()),
        };
        let (added, migrated) =
            register_kimi_hooks_in_file(&cfg, &events, SCRIPT, &new_spec).unwrap();
        assert_eq!((added, migrated), (0, 2), "两处 command 刷新计入迁移");
        let parsed: toml::Value = toml::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let hooks = parsed["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 2, "不得追加双条目");
        for h in hooks {
            assert_eq!(
                h["command"].as_str(),
                Some("C:/new/bin/tuvis-hook-listener.exe")
            );
        }
    }

    #[test]
    fn kimi_toml_verified_gates() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config.toml");
        std::fs::write(&cfg, "").unwrap();
        let events = KimiAdapter.hook_events();
        register_kimi_hooks_in_file(&cfg, &events, SCRIPT, &kimi_spec()).unwrap();
        let raw = std::fs::read_to_string(&cfg).unwrap();

        assert!(hooks_toml_verified(&raw, HELPER_CMD, &events));
        // command 漂移（旧 helper 路径）→ 不核验（注册修复入口可达）
        assert!(!hooks_toml_verified(
            &raw,
            "C:/old/bin/tuvis-hook-listener.exe",
            &events
        ));
        // 未注册事件 → 不核验
        assert!(!hooks_toml_verified(
            &raw,
            HELPER_CMD,
            &["UserPromptSubmit"]
        ));
        // 非 TOML 垃圾 → 不核验（不 panic）
        assert!(!hooks_toml_verified(
            "not [ valid toml",
            HELPER_CMD,
            &events
        ));
        // 无 [[hooks]] 段 → 不核验
        assert!(!hooks_toml_verified(
            "[other]\nk = 1\n",
            HELPER_CMD,
            &events
        ));
    }

    #[test]
    fn kimi_user_entries_are_never_touched() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config.toml");
        std::fs::write(
            &cfg,
            "[[hooks]]\nevent = \"PreToolUse\"\ncommand = \"my-own-listener\"\n",
        )
        .unwrap();
        let events = KimiAdapter.hook_events();
        let (added, migrated) =
            register_kimi_hooks_in_file(&cfg, &events, SCRIPT, &kimi_spec()).unwrap();
        assert_eq!((added, migrated), (2, 0), "用户条目不计入我方账目");
        let parsed: toml::Value = toml::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let hooks = parsed["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 3);
        assert_eq!(
            hooks[0]["command"].as_str(),
            Some("my-own-listener"),
            "用户条目原样"
        );
    }

    // ---------- matcher 迁移（我方条目 matcher 漂移修复） ----------

    #[test]
    fn claude_stale_matcher_entry_is_repaired_and_user_matcher_untouched() {
        // 我方 Notification 条目 matcher 为空（历史形态/漂移）→ 原地改写为
        // permission_prompt 计入迁移；同事件用户条目的自有 matcher 不动
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("settings.json");
        std::fs::write(
            &cfg,
            serde_json::json!({"hooks": {"Notification": [
                { "matcher": "", "hooks": [{ "type": "command", "command": HELPER_CMD }] },
                { "matcher": "idle_prompt", "hooks": [{ "type": "command", "command": "user-own" }] }
            ]}})
            .to_string(),
        )
        .unwrap();

        let (added, migrated) = register_hooks_in_file(
            &cfg,
            &["Notification"],
            true,
            SCRIPT,
            &claude_spec(),
            &[("Notification", "permission_prompt")],
        )
        .unwrap();
        assert_eq!((added, migrated), (0, 1), "仅 matcher 修复计入迁移");
        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let entries = out["hooks"]["Notification"].as_array().unwrap();
        assert_eq!(entries[0]["matcher"], "permission_prompt", "我方条目修复");
        assert_eq!(entries[1]["matcher"], "idle_prompt", "用户 matcher 不动");
        assert_eq!(entries[1]["hooks"][0]["command"], "user-own");
        // 修复后重跑 → 稳定态
        let (added2, migrated2) = register_hooks_in_file(
            &cfg,
            &["Notification"],
            true,
            SCRIPT,
            &claude_spec(),
            &[("Notification", "permission_prompt")],
        )
        .unwrap();
        assert_eq!((added2, migrated2), (0, 0));
    }

    /// **UTF-8 BOM 兼容回归**（2026-10-11 评审 Important 2）：Windows 编辑器写出的
    /// 配置带 `EF BB BF` 前缀——注册必须照常成功（2026-10-10 22:16 实机：codex
    /// hooks.json 带 BOM → 每次启动「解析配置文件失败」）。还原动作：把
    /// `read_user_config_text` 的剥 BOM 去掉 → 本用例先红。
    #[test]
    fn register_tolerates_utf8_bom_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("hooks.json");
        let body = serde_json::json!({"hooks": {}}).to_string();
        let mut with_bom = "\u{feff}".as_bytes().to_vec();
        with_bom.extend_from_slice(body.as_bytes());
        std::fs::write(&cfg, &with_bom).unwrap();

        let (added, _) = register_hooks_in_file(
            &cfg,
            &["Notification"],
            true,
            SCRIPT,
            &claude_spec(),
            &[("Notification", "permission_prompt")],
        )
        .expect("带 BOM 的配置必须照常注册成功");
        assert!(added > 0, "BOM 不应吞掉注册");
        // 注册后的文件仍合法 JSON（读回验证）
        let reread: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap())
                .expect("注册产物应是合法 JSON");
        assert!(reread.get("hooks").is_some());
    }
}

/// T5 信号健康度：核心纯逻辑（tempdir 事件目录 + 注入闭包/内存库，零触真实
/// ~/.tuvis）与 codex 一次性通知 KV 标志
#[cfg(test)]
mod signal_health_tests {
    use super::*;
    use crate::database::dao::settings::{get_setting_conn, set_setting_conn};
    use crate::session::{AgentType, ProcessForm, Session, SessionStatus};
    use std::io::Write;

    /// 内存库（settings 表就绪），供 KV 一次性通知标志断言
    fn mem_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn
    }

    /// 写事件文件（文件名 = session_id；last_event_at 定值供断言）
    fn write_event(dir: &std::path::Path, sid: &str, age_secs: i64) {
        let ts = chrono::Utc::now().timestamp() - age_secs;
        let body = format!(
            r#"{{"event":"Stop","session_id":"{sid}","cwd":"/tmp","ts":{ts},"last_event_at":"2026-09-20T00:00:00Z"}}"#
        );
        let mut f = std::fs::File::create(dir.join(format!("{sid}.json"))).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    /// 测试会话构造（15 字段全列；Session 无 Default）
    fn session(tool: AgentType, id: &str, status: SessionStatus) -> Session {
        Session {
            id: id.to_string(),
            agent_type: tool,
            project_name: "proj".to_string(),
            project_path: "/proj".to_string(),
            title: None,
            git_branch: None,
            github_url: None,
            status,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: "2026-09-20T00:00:00Z".to_string(),
            pid: 1,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::Cli,
            jump_supported: false,
            unread: false,
        }
    }

    fn tools() -> Vec<(&'static str, &'static str)> {
        vec![("codex", "Codex"), ("claude", "Claude Code")]
    }

    /// 注册 KV 真（codex 注册、claude 未注册）× 事件目录有该工具会话事件
    /// → registered=true、has_active=true、last_event_at=Some（正常态）
    #[test]
    fn registered_with_matching_session_event_reports_last_event() {
        let tmp = tempfile::tempdir().unwrap();
        write_event(tmp.path(), "codex-sid-1", 0);
        write_event(tmp.path(), "codex-sid-1", 0); // 同键覆盖（helper 语义），不重复
        let events = read_hook_events_from(tmp.path());
        let sessions = vec![
            session(AgentType::Codex, "codex-sid-1", SessionStatus::Processing),
            session(AgentType::Claude, "claude-sid-9", SessionStatus::Idle),
        ];
        let registered_of = |tool_id: &str| tool_id == "codex"; // codex 已注册，claude 未注册
        let out = compute_tool_signal_health(&tools(), &registered_of, &events, &sessions);
        assert_eq!(out.len(), 2);
        let codex = out.iter().find(|h| h.tool_id == "codex").unwrap();
        assert!(codex.registered);
        assert!(codex.has_active_sessions);
        assert_eq!(codex.last_event_at.as_deref(), Some("2026-09-20T00:00:00Z"));
        assert_eq!(codex.label, "Codex");
        let claude = out.iter().find(|h| h.tool_id == "claude").unwrap();
        assert!(!claude.registered);
        assert!(!claude.has_active_sessions, "Idle 会话不算活跃");
    }

    /// 注册真 × 事件目录空 × 有活跃会话 → last_event_at=None + has_active=true
    ///（前端待办态「需信任」的判据输入）
    #[test]
    fn registered_active_session_zero_events_is_todo_input() {
        let tmp = tempfile::tempdir().unwrap(); // 目录存在但无事件文件
        let events = read_hook_events_from(tmp.path());
        let sessions = vec![session(
            AgentType::Codex,
            "codex-sid-2",
            SessionStatus::Waiting,
        )];
        let registered_of = |_tool_id: &str| true;
        let out = compute_tool_signal_health(&tools(), &registered_of, &events, &sessions);
        let codex = out.iter().find(|h| h.tool_id == "codex").unwrap();
        assert!(codex.registered);
        assert!(codex.has_active_sessions);
        assert!(codex.last_event_at.is_none());
    }

    /// 注册假 × 事件目录有事件 → registered=false（未注册态与事件无关）
    #[test]
    fn unregistered_reports_false_even_with_events() {
        let tmp = tempfile::tempdir().unwrap();
        write_event(tmp.path(), "codex-sid-3", 0);
        let events = read_hook_events_from(tmp.path());
        let sessions = vec![session(
            AgentType::Codex,
            "codex-sid-3",
            SessionStatus::Waiting,
        )];
        let registered_of = |_tool_id: &str| false;
        let out = compute_tool_signal_health(&tools(), &registered_of, &events, &sessions);
        let codex = out.iter().find(|h| h.tool_id == "codex").unwrap();
        assert!(!codex.registered);
        assert_eq!(codex.last_event_at.as_deref(), Some("2026-09-20T00:00:00Z"));
    }

    /// 会话归属相关性：事件只归属 session_id 匹配的工具——claude 会话的事件
    /// 不得漏计到同场出卡的 codex 头上（否则 codex 会因别家事件伪装「正常」）
    #[test]
    fn events_do_not_leak_across_tools() {
        let tmp = tempfile::tempdir().unwrap();
        write_event(tmp.path(), "claude-sid-a", 0);
        let events = read_hook_events_from(tmp.path());
        let sessions = vec![
            session(AgentType::Claude, "claude-sid-a", SessionStatus::Idle),
            session(AgentType::Codex, "codex-sid-b", SessionStatus::Processing),
        ];
        let registered_of = |_tool_id: &str| true;
        let out = compute_tool_signal_health(&tools(), &registered_of, &events, &sessions);
        let codex = out.iter().find(|h| h.tool_id == "codex").unwrap();
        assert!(codex.last_event_at.is_none(), "claude 的事件不得归属 codex");
        assert!(codex.has_active_sessions);
        let claude = out.iter().find(|h| h.tool_id == "claude").unwrap();
        assert_eq!(
            claude.last_event_at.as_deref(),
            Some("2026-09-20T00:00:00Z")
        );
    }

    /// 同工具多事件取 ts 最大者（多会话并发时最近一条生效）
    #[test]
    fn multiple_events_pick_latest_by_ts() {
        let tmp = tempfile::tempdir().unwrap();
        write_event(tmp.path(), "codex-sid-old", 20);
        write_event(tmp.path(), "codex-sid-new", 1);
        let events = read_hook_events_from(tmp.path());
        let sessions = vec![
            session(AgentType::Codex, "codex-sid-old", SessionStatus::Idle),
            session(AgentType::Codex, "codex-sid-new", SessionStatus::Idle),
        ];
        let registered_of = |_tool_id: &str| true;
        let out = compute_tool_signal_health(&tools(), &registered_of, &events, &sessions);
        let codex = out.iter().find(|h| h.tool_id == "codex").unwrap();
        assert!(codex.last_event_at.is_some());
        // Idle 会话不算活跃，但事件归属照常成立（正常态展示）
        assert!(!codex.has_active_sessions);
    }

    /// codex 一次性通知 KV：未示过 → 置 pending；示过（shown=true）→ 永不再置
    ///（内存库写断言，一次性语义）。走真实 enqueue_codex_trust_notice_conn 路径
    ///（*_conn 模式）——键名/条件改错时本测试同步变红，不复刻逻辑
    #[test]
    fn codex_notice_kv_is_one_shot() {
        let conn = mem_conn();
        // 首次注册：未示过（键缺省）→ 置 pending
        enqueue_codex_trust_notice_conn(&conn);
        assert_eq!(
            get_setting_conn(&conn, CODEX_NOTICE_PENDING_KEY).as_deref(),
            Some("true"),
            "首次注册必须登记 pending"
        );
        // setup 消费：发通知 → 落 shown + 清 pending（consume 的 KV 侧语义）
        set_setting_conn(&conn, CODEX_NOTICE_SHOWN_KEY, "true");
        set_setting_conn(&conn, CODEX_NOTICE_PENDING_KEY, "false");
        // 后续再次注册（重启/重注册）：已示过 → 不得再置 pending
        enqueue_codex_trust_notice_conn(&conn);
        assert_eq!(
            get_setting_conn(&conn, CODEX_NOTICE_PENDING_KEY).as_deref(),
            Some("false"),
            "已示过后不得再登记 pending（一次性）"
        );
        // 判定纯函数边界：shown 缺省/"false" 都算未示过（防半态脏数据卡死提醒）
        assert!(codex_notice_should_enqueue(None));
        assert!(codex_notice_should_enqueue(Some("false")));
        assert!(!codex_notice_should_enqueue(Some("true")));
    }
}
