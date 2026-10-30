// 会话相关命令

use crate::session::SessionsResponse;
// Manager 的导入在下方 macOS cfg 块内局部引入：get_webview_window 仅 macOS
// 路径消费，顶层导入在 Linux/Windows CI 上是 unused import（-D warnings 即挂）
use tauri::Emitter;

#[tauri::command]
pub async fn get_all_sessions(app: tauri::AppHandle) -> SessionsResponse {
    // 读扫描总线的共享快照（常驻扫描线程见 adapter::scan_bus::start）；只有冷启动
    // 尚无快照时才同步补扫一次兜底。spawn_blocking 仍保留：兜底扫描（sysinfo 全进程
    // 刷新 + 各工具文件解析，dsh 冷启动数秒）不得占主线程。托盘状态已迁入扫描线程
    // （常驻线程内部 update_tray），命令层只剩预设刷新这类轻量 UI 操作
    let response = tauri::async_runtime::spawn_blocking(crate::adapter::scan_bus::current_or_scan)
        .await
        .unwrap_or_else(|e| {
            ::log::error!("会话快照读取任务异常: {e}");
            SessionsResponse {
                sessions: Vec::new(),
                total_count: 0,
                waiting_count: 0,
            }
        });
    let preset_count = crate::database::list_presets().len();
    let last_count = crate::database::get_setting("last_preset_count")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(usize::MAX);
    if preset_count != last_count {
        let _ = crate::plugins::system_tray::update_tray_with_presets(&app);
        crate::database::set_setting("last_preset_count", &preset_count.to_string());
    }
    response
}

/// 扫描总线状态（前端档位跟随的观测口）：mode 为 active/idle/away（serde lowercase），
/// version 为快照发布版本（单调递增，前端据此判断数据是否已刷新）。
/// F3 裁决「诊断字段即可」：`degraded` = 扫描疑似 hang（在飞超 120s，需重启恢复）；
/// `snapshotAgeMs` = 最近快照龄（毫秒；无快照为 null）——补扫超时降级时可查
#[tauri::command]
pub fn scan_bus_status() -> serde_json::Value {
    let snapshot_age_ms =
        crate::adapter::scan_bus::snapshot().map(|s| s.scanned_at.elapsed().as_millis() as u64);
    serde_json::json!({
        "mode": crate::adapter::scan_bus::mode(),
        "version": crate::adapter::scan_bus::version(),
        "degraded": crate::adapter::scan_bus::is_degraded(),
        "snapshotAgeMs": snapshot_age_ms,
    })
}

/// 前端可见性心跳（主窗可见且聚焦 / 浮窗展开）：置「有人在看」，TTL 内节奏降档到 idle。
/// 上升沿（**总体**未在看 → 在看）立即请求一次事件补扫——窗口重新聚焦时数据立刻新鲜，
/// 不必等下一个档位周期（最坏 30s）。
///
/// `source` 区分心跳来源：主窗 "main"（缺省）、宠物浮窗 "pet"。两个 WebView 共用同一后端
/// 状态，按源记账才能让单源退出（浮窗收起 / 主窗失焦）不清掉另一源的「在看」
#[tauri::command]
pub fn scan_bus_watch(watching: bool, source: Option<String>) {
    let source = normalize_watch_source(source);
    if crate::adapter::scan_bus::set_user_watching(&source, watching) {
        crate::adapter::scan_bus::request_scan(crate::adapter::scan_bus::ScanReason::Event);
    }
}

/// 心跳源归一：缺省（老前端 / 远端调用）一律记到主窗；空白串同缺省，避免造出无名源
fn normalize_watch_source(source: Option<String>) -> String {
    match source.map(|s| s.trim().to_string()) {
        Some(s) if !s.is_empty() => s,
        _ => "main".to_string(),
    }
}

#[cfg(test)]
mod f5_immediate_scan_tests {
    use super::*;

    /// 取函数体源码（按花括号配对；签名必须唯一命中实现处）
    fn function_body<'a>(src: &'a str, signature: &str) -> &'a str {
        let start = src.find(signature).expect("函数签名缺失");
        let open = src[start..].find('{').expect("函数体缺失") + start;
        let mut depth = 0usize;
        for (i, c) in src[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &src[open..=open + i];
                    }
                }
                _ => {}
            }
        }
        panic!("函数体未闭合");
    }

    /// Fix 4 / F5：隐藏卡片必须在命令返回前**同步**补扫——即发即忘的 request_scan
    /// 让前端可能先读到上一版快照，卡片最多残留一个档位周期（Away 30s）。
    #[test]
    fn dismiss_session_card_scans_synchronously_before_returning() {
        // 签名用 concat! 拆分：include_str! 含本测试自身，整串字面量会先命中测试代码
        let signature = concat!("fn dismiss_session", "_card(");
        let body = function_body(include_str!("session.rs"), signature);
        assert!(
            body.contains("scan_now_blocking"),
            "dismiss_session_card 必须在返回前同步补扫（F5 ≤1s 收口），实际: {body}"
        );
        assert!(
            !body.contains("request_scan"),
            "同步补扫已覆盖即时性，dismiss 不应再留即发即忘的 request_scan"
        );
    }
}

#[cfg(test)]
mod scan_bus_status_tests {
    use super::*;

    /// Fix 2 诊断字段回归锁：`scan_bus_status` 必须给出降级标志与快照龄——
    /// F3 裁决是「诊断字段即可」，前端/巡检据此区分「新鲜」与「补扫超时降级」。
    #[test]
    fn status_reports_degraded_flag_and_snapshot_age() {
        let _guard = crate::adapter::scan_bus::test_guard();
        crate::adapter::scan_bus::reset_for_test();
        crate::adapter::scan_bus::publish_for_test(|| SessionsResponse {
            sessions: Vec::new(),
            total_count: 0,
            waiting_count: 0,
        });
        let v = scan_bus_status();
        assert_eq!(
            v.get("degraded").and_then(|x| x.as_bool()),
            Some(false),
            "无在飞扫描时不得标记 degraded"
        );
        assert!(
            v.get("snapshotAgeMs").and_then(|x| x.as_u64()).is_some(),
            "有快照时必须给出 snapshotAgeMs"
        );
        assert_eq!(v.get("version").and_then(|x| x.as_u64()), Some(1));
    }
}

/// 跳转成功 → 标记该会话已读（仅删除对应未读行；同工具其他未读卡保留，spec W4）。
/// T1：删行后广播 session-read，宠物等辅助窗口据此同步已读置位（卡片行为与看板一致）
fn mark_read_on_jump(
    app: &tauri::AppHandle,
    session_id: &Option<String>,
    agent_type: &Option<String>,
) {
    if let (Some(sid), Some(agent)) = (session_id, agent_type) {
        crate::database::dao::unread::delete(&agent.to_lowercase(), sid);
        // issue #35-1：已读墓碑——缓存失忆（长间隙清缓存 / 兔维斯 重启）后
        // Insert 边沿与补偿据此不再复活已读未读卡
        crate::database::dao::unread::mark_read(&agent.to_lowercase(), sid);
        let _ = app.emit(
            "session-read",
            serde_json::json!({ "agentType": agent, "sessionId": sid }),
        );
        // F5 即时重算：已读落库后请求一次动作补扫（Action 不吃事件合并节流），
        // 前端已有的 invalidateQueries(["sessions"]) 下一帧即读到新快照，
        // 不必等下一个档位周期（最坏 30s）
        crate::adapter::scan_bus::request_scan(crate::adapter::scan_bus::ScanReason::Action);
    }
}

// 参数即 Tauri IPC 契约（前端 invoke 逐名传参），不宜打包为结构体
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub fn focus_session(
    app: tauri::AppHandle,
    pid: u32,
    session_id: Option<String>,
    agent_type: Option<String>,
    project_name: Option<String>,
    last_message: Option<String>,
    title: Option<String>,
    form: Option<String>,
    unread: Option<bool>,
) -> Result<serde_json::Value, String> {
    // dsh（H1）：桌面端宿主在场 → 聚焦桌面 APP 窗口；不在场才回落 web 宿主路径。
    // 插入点在平台 cfg 分派之前（跨平台前置）；macOS web 回落路径与 Windows
    // 无桌面宿主时的行为（现有：无 dsh 跳转 → 维持原样）均不变
    if agent_type.as_deref() == Some("dsh") {
        let system = sysinfo::System::new_all();
        if let Some(pid) = crate::monitor::dsh::find_dsh_desktop_host_pid(&system) {
            #[cfg(windows)]
            {
                match crate::window::win32::focus_window_for_pid(pid) {
                    Ok(()) => {
                        mark_read_on_jump(&app, &session_id, &agent_type);
                        return Ok(serde_json::json!({ "via": "dsh-desktop" }));
                    }
                    Err(e) => {
                        return Err(format!(
                            "SESSION_FOCUS_FAIL: 无法聚焦 DeepSeek Harness 窗口：{e}"
                        ))
                    }
                }
            }
            #[cfg(target_os = "macos")]
            {
                if let Some(p) = system.process(sysinfo::Pid::from_u32(pid)) {
                    if let Some(exe) = p.exe().and_then(|e| e.to_str()) {
                        if let Some(bundle) =
                            crate::window::app_activation::app_bundle_from_exe(exe)
                        {
                            if crate::window::app_activation::activate_app_bundle(&bundle).is_ok() {
                                mark_read_on_jump(&app, &session_id, &agent_type);
                                return Ok(serde_json::json!({ "via": "dsh-desktop" }));
                            }
                        }
                    }
                }
                // macOS 激活失败 → 落入下方 web 路径（不 return）
            }
            #[cfg(not(any(windows, target_os = "macos")))]
            {
                let _ = pid; // Linux 不支持聚焦（window/mod 既有口径）
            }
        }
    }
    #[cfg(windows)]
    {
        // mut：P2-1 验证轮询/兜底前会在原地刷新进程表（快照不再是一次性照片）
        let mut system = sysinfo::System::new_all();
        // unread 参数（未读标记）在 Windows 深度链接路径暂不消费（已读回标统一在
        // 跳转成功分支执行）；保留签名以与前端 JumpTarget.unread 对齐
        let _ = unread;
        // P1-1（Windows）：App 形态且带 sessionId 且工具为 workbuddy/codex 时，
        // 深度链接为第一顺位（session 级直达）：handler 校验通过 → 派发 → 前台验证成功
        // → 标已读返回；任一步失败无缝落回现有 resolve_and_focus → reactivate_tool_app 链路。
        // codex 由 handler 校验天然门控（无 handler 机器自动跳过，实测语义见 P1-2）。
        // zcode 不在此列（2026-09-09 实机验收后移出）：zcode://workspace/open 每次
        // 无条件弹信任确认、落点为新会话 composer（非定位已有会话）、每次拉起一个
        // 转发进程——跳转改为下方 resolve_and_focus 的 pid 单窗口路径直接聚焦唯一
        // 窗口（ZCode 单窗口多标签，卡片携带宿主 pid，窗口零歧义），失败由
        // reactivate_tool_app 兜底（zcode 已在其谓词与认领关键字内）
        if form.as_deref() == Some("app") {
            if let (Some(sid), Some(agent)) = (session_id.as_deref(), agent_type.as_deref()) {
                if matches!(agent, "workbuddy" | "codex")
                    && crate::window::deep_link::scheme_handler_exists(agent)
                {
                    let url = crate::window::deep_link::session_url(agent, sid);
                    if let Some(url) = url {
                        if crate::window::deep_link::open_url(&url).is_ok() {
                            if crate::window::win32::verify_foreground_tool(
                                &mut system,
                                agent,
                                2_000,
                                250,
                            ) {
                                mark_read_on_jump(&app, &session_id, &agent_type);
                                return Ok(serde_json::json!({
                                    "type": "focused",
                                    "via": "deep-link"
                                }));
                            }
                            // P2-1（B 兜底保险，issue #34）：派发已发生但前台验证未确认
                            // ——宿主可能恰在验证窗口后被冷启动。落保底链路前重刷快照，
                            // 使 running_projects / resolve_and_focus / reactivate_
                            // tool_app 能认出派发期间新出现的宿主进程；everything 全量
                            // 刷新，下游消费的 cwd/exe 均取最新
                            system.refresh_processes_specifics(
                                sysinfo::ProcessesToUpdate::All,
                                true,
                                sysinfo::ProcessRefreshKind::everything(),
                            );
                        }
                    }
                }
            }
        }
        // marker 与按需注入 helper 贴的标题标记一致：TUVIS:<session_id 剥连字符后
        // 前 12 位>。口径单一出处 = window::disambig::marker_from_session_id（#57
        // 漂移封口：本处（匹配侧）与 tuvis-marker helper（注入侧）走同一函数，
        // 两端各有字面向量测试；12 位理由与剥连字符的口径见该函数文档）
        let marker = session_id
            .as_deref()
            .map(crate::window::disambig::marker_from_session_id);
        // 面板反推：当前所有运行会话的 (工具id, 项目名)，用于排除其他工具的终端窗口
        // （codex 终端标题=项目名，无 "codex" 关键词可静态认领）。进程扫描即可，无文件解析开销
        let running_projects = running_projects_from_processes(&system);
        // 配对不确定门（issue #48）：同工具同项目 ≥2 个进程在跑时，卡片 pid 与会话的
        // 启发式配对（kimi=wire mtime 最新 / opencode=time_updated DESC）可能互换——
        // 跳转禁用一切演绎锁定（单窗口即锁/幸存者推理），只认正向证据，否则交选择器。
        // 项目名比对忽略大小写（Windows 路径不区分大小写），与 running_projects 的
        // cwd file_name 同源
        let require_evidence = {
            let agent = agent_type.as_deref().unwrap_or_default().to_lowercase();
            match project_name.as_deref().map(str::to_lowercase) {
                Some(p) if !p.is_empty() => {
                    running_projects
                        .iter()
                        .filter(|(a, pr)| a.to_lowercase() == agent && pr.to_lowercase() == p)
                        .count()
                        >= 2
                }
                _ => false,
            }
        };
        // 按需注入（spec 2026-09-12 §3.2）：claude/codex CLI 会话在判定链前贴
        // marker，① 层即可精确命中。**配对不确定时必须一并禁用**（第四轮 3c 实证）：
        // 注入目标取自卡片 pid，pid 本身可能配对交叉 → marker 贴到兄弟会话的终端，
        // ① 层随之高置信锁错窗（marker 命中构成自证循环，不是正向证据）。禁用后
        // 该场景回落双命中仲裁/选择器，"锁对或选择器、绝不锁错"契约恢复。
        // helper 缺失/失败/超时仍然全部静默回落
        if on_demand_injection_allowed(agent_type.as_deref(), form.as_deref(), require_evidence) {
            if let Some(sid) = session_id.as_deref() {
                if let Some(m) = marker.as_deref() {
                    crate::window::win32::inject_marker_on_demand(sid, pid, m);
                }
            }
        }
        let hints = crate::window::win32::JumpHints {
            session_marker: marker.as_deref(),
            agent_keyword: agent_type.as_deref(),
            project_name: project_name.as_deref(),
            last_message: last_message.as_deref(),
            title: title.as_deref(),
            require_positive_evidence: require_evidence,
        };
        match crate::window::win32::resolve_and_focus(&system, pid, &hints, &running_projects) {
            Ok(crate::window::win32::FocusOutcome::Focused) => {
                // 聚焦成功清痕（round-5）：剥掉本次注入的 marker，标题回到干净态
                //（下次跳转重新注入，按需注入本就是一次性模式）。清除范围与注入的
                // 工具/形态门一致；Ambiguous 选择器分支不清——marker 还在候选标题上，
                // 供用户辨认，关选择器后自然过期/下次注入时被剥
                if on_demand_marker_applies(agent_type.as_deref(), form.as_deref()) {
                    crate::window::win32::clear_marker_after_focus(pid);
                }
                mark_read_on_jump(&app, &session_id, &agent_type);
                Ok(serde_json::json!({ "type": "focused" }))
            }
            Ok(crate::window::win32::FocusOutcome::Ambiguous(windows)) => {
                Ok(serde_json::json!({ "type": "ambiguous", "windows": windows }))
            }
            Err(e) => {
                // pid 失效兜底（W2）：pid 已死时按工具激活宿主 APP 窗口
                let pid_dead = system.process(sysinfo::Pid::from_u32(pid)).is_none();
                if pid_dead
                    && crate::window::win32::reactivate_tool_app(&system, agent_type.as_deref())
                        .is_ok()
                {
                    mark_read_on_jump(&app, &session_id, &agent_type);
                    Ok(serde_json::json!({ "type": "focused" }))
                } else {
                    Err(e)
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (project_name, last_message, title, form, unread);
        // dsh（M1）：宿主是终端启动的 node 进程，通常持有 TTY——若 TTY 链路
        // 先行会聚焦终端并误标已读，dsh 路由永远不可达（review Important）。
        // 故 dsh 精确匹配必须在 TTY 链路之前短路；也不走 APP 激活链路——
        // 聚焦/打开 dsh web 标签页（无 per-session URL，设计 P4 定案）；失败给出提示
        #[cfg(target_os = "macos")]
        if agent_type.as_deref() == Some("dsh") {
            // get_webview_window 的 Manager trait 导入收在 cfg 块内（见文件头注释）
            use tauri::Manager;
            match crate::window::dsh_tab::focus_dsh_tab() {
                Ok(mut out) => {
                    mark_read_on_jump(&app, &session_id, &agent_type);
                    // 跳转成功即隐藏看板（用户 2026-09-14 验收裁决）：dsh 落点是
                    // 浏览器标签，看板窗口留在原地遮挡视线；收进托盘（点托盘可
                    // 再唤出），与「关窗即隐藏」的托盘应用形态一致
                    if let Some(w) = app.get_webview_window("main") {
                        let _ = w.hide();
                    }
                    out["via"] = serde_json::Value::String("dsh".into());
                    return Ok(out);
                }
                Err(e) => {
                    return Err(format!(
                        "SESSION_DSH_FOCUS_FAIL: 无法聚焦 dsh 页面：{e}（请手动打开 dsh web）"
                    ))
                }
            }
        }
        // CLI 形态：TTY 链路（tmux/iTerm2/Terminal.app）
        if crate::window::focus_terminal_for_pid(pid).is_ok() {
            mark_read_on_jump(&app, &session_id, &agent_type);
            return Ok(serde_json::json!({ "type": "focused", "via": "tty" }));
        }
        // APP 形态 / pid 失效兜底：深度链接 → bundle 激活 → 按工具枚举（W2）。
        // via=app-fallback：CLI 会话 TTY 聚焦失败走到这里的 UX 提示依据（review M3）
        #[cfg(target_os = "macos")]
        if let Some(mut out) =
            crate::window::activate_agent_app(pid, agent_type.as_deref(), session_id.as_deref())
        {
            // P1-2（review）：macOS 深链无前台验证，路由成败不可证 → 不标已读
            //（宁可让用户再点一次/X 关闭，不可误删未读卡）；bundle/枚举兜底激活
            // 仍按 spec §5「兜底激活同样算跳转成功」回标已读
            let via = out.get("via").and_then(|v| v.as_str());
            if macos_deep_link_marks_read(via) {
                mark_read_on_jump(&app, &session_id, &agent_type);
                out["via"] = serde_json::Value::String("app-fallback".into());
            }
            return Ok(out);
        }
        // 非 macOS 桌面平台无 APP 激活链路，防未使用告警
        #[cfg(not(target_os = "macos"))]
        let _ = (agent_type, session_id);
        // 措辞兼容两种场景：pid 存活的 CLI 会话（有 TTY 但聚焦失败，P2-2 后不再做
        // APP 兜底）与 pid=0/已死（TTY 无从聚焦 + 宿主 APP 枚举未命中）
        Err(format!(
            "无法聚焦目标（pid={}）：终端与宿主 APP 均未能聚焦",
            pid
        ))
    }
}

/// 按需注入工具/形态门（spec §3.2）：仅无可靠静态标题键的 claude/codex 且 CLI
/// 形态。kimi/opencode 标题键已实测够用，注入只会污染其标题
#[cfg(any(windows, test))]
fn on_demand_marker_applies(agent: Option<&str>, form: Option<&str>) -> bool {
    matches!(agent, Some("claude" | "codex")) && form != Some("app")
}

/// 按需注入总门 = 工具/形态门 ∧ ¬配对不确定。配对不确定（同工具同项目 ≥2 进程）
/// 时卡片 pid 可能与会话交叉，注入目标随之错配，marker 命中构成自证循环——
/// 第四轮 3c 实证 codex 同项目双开两次静默锁错，故此场景禁注入、回落既有层
#[cfg(any(windows, test))]
fn on_demand_injection_allowed(
    agent: Option<&str>,
    form: Option<&str>,
    pairing_ambiguous: bool,
) -> bool {
    on_demand_marker_applies(agent, form) && !pairing_ambiguous
}

/// macOS 深链成功是否回标已读（P1-2 判定核心，cfg(test) 使其 Windows 侧可测）：
/// via=deep-link → 不标（无前台验证，路由成败不可证）；其余（bundle/枚举兜底）→ 标
#[cfg(any(target_os = "macos", test))]
fn macos_deep_link_marks_read(via: Option<&str>) -> bool {
    via != Some("deep-link")
}

/// 窗口选择器点选后按句柄聚焦
#[tauri::command]
pub fn focus_hwnd(hwnd: isize) -> Result<(), String> {
    #[cfg(windows)]
    {
        crate::window::win32::focus_hwnd(hwnd)
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        Err("SESSION_PLATFORM_UNSUPPORTED: 当前平台不支持".to_string())
    }
}

/// 手动关闭活跃 App 卡（T2 X 按钮，「暂离不提示」）：写入进程内 dismiss 集合，
/// 从看板与宠物隐藏；同一会话状态变化后 key 不匹配自然重现。
/// 不碰 unread_sessions 表（未读卡的 X 走 mark_session_read 已读语义）。
/// status 用前端 SessionStatus 字符串（serde lowercase，如 "waiting"），
/// 与 filter_dismissed_cards 的归一化（Debug 形态转小写）一致
#[tauri::command]
pub fn dismiss_session_card(agent_type: String, session_id: String, status: String) {
    crate::monitor::SESSION_DISMISALS.lock().unwrap().insert((
        agent_type.to_lowercase(),
        session_id,
        status.to_lowercase(),
    ));
    // F5 即时重算：隐藏集合是扫描内的过滤条件（filter_dismissed_cards），
    // **同步**补扫后本命令才返回——前端随后的 invalidateQueries(["sessions"])
    // 一定能读到不含该卡的新快照（即发即忘会让卡片残留一个档位周期，Away 最坏 30s）
    crate::adapter::scan_bus::scan_now_blocking();
}

/// R5 一键 resume（桌面卡「在电脑上打开」）：按 session_id 在当前快照里找会话，
/// 交 inject::resume 核心开终端恢复（Windows wt/conhost、macOS iTerm2/Terminal）。
/// 数据同源：动作路径经扫描总线的 ensure_fresh 取新鲜快照（龄超 2s 则补扫并等待，
/// 上限 3s；超时按现有快照降级继续），与看板同一份数据；id 跨工具撞名取第一个
/// 匹配（远端 session-open 端点同口径）。命令表未收录的
/// 工具 / 无 cwd 由核心返回哨兵串，转译为前端可展示的中文文案；本机路径不写审计
/// （audit_write 需设备身份，审计口径 = 远端设备动作留痕，见 inject::resume 模块注释）。
/// 评审 M3：快照读取 + 核心并进同一 spawn_blocking——核心内 `where wt` 首调探测是
/// 同步进程等待，留在闭包里不占 tokio worker（冷启动兜底扫描同样不进主线程）。
#[tauri::command]
pub async fn session_open(app: tauri::AppHandle, session_id: String) -> Result<(), String> {
    let sid = session_id.trim().to_string();
    if sid.is_empty() {
        return Err("SESSION_ID_MISSING: 缺少 session_id".to_string());
    }
    tauri::async_runtime::spawn_blocking(move || {
        // mut：T3 信任归一在 spawn 前原地改写 project_path（claude 专属，只读
        // ~/.claude.json 永不写）
        let mut session = crate::adapter::scan_bus::ensure_fresh(
            std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(3),
        )
        .data
        .sessions
        .iter()
        .find(|s| s.id == sid)
        .cloned()
        .ok_or_else(|| "会话不在当前列表（可能已退出）".to_string())?;
        // T3 信任归一：命中已信任条款复用其精确 casing（防 claude 信任弹窗挂起）。
        // 桌面发起 → 桌面 surface：不冒用会话状态卡浮窗（NotificationPayload 形态
        // 是会话卡，信任提醒非会话卡）——走系统通知 toast（consume_codex_trust_notice
        // 同款 builder 直发通道，评审修复：预检提醒桌面侧用户可见承载）
        let home = dirs::home_dir();
        let trust_prompt =
            crate::inject::resume::normalize_cwd_for_trust(&mut session, home.as_deref());
        let r = crate::inject::resume::open_session_terminal(&session);
        if trust_prompt && r.is_ok() {
            log::warn!(
                "一键打开未信任预检（session={}）：{}",
                session.id,
                crate::inject::resume::TRUST_PROMPT_REMINDER
            );
            // 通知发送失败降级 log（文案已在 log 存证，重开会话卡状态不丢失）
            use tauri_plugin_notification::NotificationExt;
            if let Err(e) = app
                .notification()
                .builder()
                .title("Tuvis")
                .body(crate::inject::resume::TRUST_PROMPT_REMINDER)
                .show()
            {
                log::warn!("未信任预检系统通知发送失败: {e}");
            }
        }
        r
    })
    .await
    .map_err(|e| format!("SESSION_SCAN_FAIL: 会话扫描任务异常: {e}"))?
    .map_err(|e| match e.as_str() {
        // 哨兵串转译（前端 toast 直展；与移动端禁用原因文案同义）
        "no_resume_command" => "该工具 resume 命令待查证，暂不支持一键打开".to_string(),
        "no_cwd" => "该会话没有项目目录信息，无法在电脑上打开".to_string(),
        other => other.to_string(),
    })
}

/// 从进程快照收集运行会话的 (工具id, 项目目录名)——仅进程扫描，无文件解析开销
#[cfg(windows)]
pub(crate) fn running_projects_from_processes(system: &sysinfo::System) -> Vec<(String, String)> {
    use crate::monitor::process as monitor_process;
    let mut v = Vec::new();
    for (agent, procs) in [
        ("claude", monitor_process::find_claude_processes(system)),
        ("codex", monitor_process::find_codex_processes(system)),
        ("opencode", monitor_process::find_opencode_processes(system)),
        ("openclaw", monitor_process::find_openclaw_processes(system)),
        ("kimi", monitor_process::find_kimi_processes(system)),
    ] {
        for p in procs {
            if let Some(name) = p
                .cwd
                .as_ref()
                .and_then(|c| c.file_name())
                .map(|n| n.to_string_lossy().to_string())
            {
                if !name.is_empty() {
                    v.push((agent.to_string(), name));
                }
            }
        }
    }
    v
}

/// 杀进程内核（CLI 会话硬杀）：桌面 kill_session 与远程 /session-close 共用。
/// sysinfo kill_with(Signal::Term)：macOS/Linux=SIGTERM；Windows 不支持 Term 信号
/// （kill_with 返回 None）→ 回落 process.kill()（TerminateProcess），两平台都保证结束。
pub fn kill_pid(pid: u32) -> Result<(), String> {
    use sysinfo::{Pid, Signal};
    if let Some(process) = sysinfo::System::new_all().process(Pid::from_u32(pid)) {
        if process.kill_with(Signal::Term).is_none() {
            process.kill();
        }
        Ok(())
    } else {
        Err(format!("SESSION_PID_GONE:{pid}: 进程 {} 不存在", pid))
    }
}

#[tauri::command]
pub fn kill_session(pid: u32) -> Result<(), String> {
    let r = kill_pid(pid);
    if r.is_ok() {
        // F5 即时重算：进程已杀 → 动作补扫让该会话卡随新快照立即消失；
        // 失败（进程已不在）不改动数据，无需补扫
        crate::adapter::scan_bus::request_scan(crate::adapter::scan_bus::ScanReason::Action);
    }
    r
}

#[cfg(test)]
mod macos_deep_link_read_tests {
    use super::macos_deep_link_marks_read;

    /// P1-2 回归锁：macOS 深链（无前台验证，路由成败不可证）成功也不得回标已读；
    /// bundle/枚举兜底激活（spec §5「同样算跳转成功」）与 TTY 直达照旧标已读
    #[test]
    fn deep_link_via_never_marks_read() {
        assert!(!macos_deep_link_marks_read(Some("deep-link")));
        assert!(macos_deep_link_marks_read(None)); // bundle/枚举兜底（无 via 标记）
        assert!(macos_deep_link_marks_read(Some("tty")));
        assert!(macos_deep_link_marks_read(Some("app-fallback")));
    }
}

#[cfg(test)]
mod on_demand_tests {
    // 门控契约（spec §3.2）：仅 claude/codex 且仅 CLI 形态注入；其余零开销跳过
    use super::{on_demand_injection_allowed, on_demand_marker_applies};

    #[test]
    fn applies_to_claude_and_codex_cli_only() {
        assert!(on_demand_marker_applies(Some("claude"), Some("cli")));
        assert!(on_demand_marker_applies(Some("codex"), Some("cli")));
        assert!(on_demand_marker_applies(Some("claude"), None)); // form 缺失按 CLI 处理
        assert!(!on_demand_marker_applies(Some("claude"), Some("app")));
        assert!(!on_demand_marker_applies(Some("kimi"), Some("cli"))); // 标题键够用
        assert!(!on_demand_marker_applies(Some("opencode"), Some("cli"))); // 同上
        assert!(!on_demand_marker_applies(None, Some("cli")));
    }

    #[test]
    fn pairing_ambiguous_disables_injection() {
        // 第四轮 3c 回归锁：同工具同项目双开（配对不确定）时禁用注入——
        // 错配 pid 注入会把 marker 贴到兄弟会话终端，① 层自证循环锁错窗
        assert!(on_demand_injection_allowed(
            Some("codex"),
            Some("cli"),
            false
        ));
        assert!(!on_demand_injection_allowed(
            Some("codex"),
            Some("cli"),
            true
        ));
        assert!(!on_demand_injection_allowed(
            Some("claude"),
            Some("cli"),
            true
        ));
        assert!(!on_demand_injection_allowed(
            Some("kimi"),
            Some("cli"),
            false
        ));
    }
}

#[cfg(test)]
mod scan_watch_source_tests {
    use super::normalize_watch_source;

    /// 心跳源契约（Task 9）：缺省/空白 → "main"（老前端不带 source 也落主窗源，
    /// 不会造出无名源干扰按源记账）；显式 source 原样保留（宠物浮窗报 "pet"）
    #[test]
    fn defaults_to_main_and_trims_explicit_source() {
        assert_eq!(normalize_watch_source(None), "main");
        assert_eq!(normalize_watch_source(Some(String::new())), "main");
        assert_eq!(normalize_watch_source(Some("   ".to_string())), "main");
        assert_eq!(normalize_watch_source(Some("pet".to_string())), "pet");
        assert_eq!(normalize_watch_source(Some(" pet ".to_string())), "pet");
    }
}

#[cfg(test)]
mod session_snapshot_lock_tests {
    /// 源码锁：命令层不得再直调扫描（每请求全量扫描 = 前端 3 秒轮询下的 CPU 抖动
    /// 根因），一律读 scan_bus 共享快照。
    /// 断言串用 `concat!` 拼接：`include_str!` 会把本文件（含本测试）一并纳入被检
    /// 源码，整串字面量直接写在断言里会自我命中——负向断言永远失败、正向断言永远
    /// 通过（恒真恒假两条废锁）
    #[test]
    fn sessions_command_reads_snapshot_instead_of_scanning() {
        let src = include_str!("session.rs");
        let direct_scan = concat!("adapter::", "get_all_sessions");
        let snapshot_read = concat!("scan_bus::", "current_or_scan");
        assert!(
            !src.contains(direct_scan),
            "命令层不得再直调扫描；只允许读 scan_bus 共享快照"
        );
        assert!(src.contains(snapshot_read), "命令层应经共享快照读取会话");
    }
}

#[cfg(test)]
mod marker_literal_tests {
    // 匹配侧 marker 构造回归锁（与本文件 focus_session 的内联构造逐字一致；
    // 口径内联于命令内，按互引纪律不抽取 helper，靠本锁镜像防漂移）。
    // 与注入侧锁成对：src/bin/tuvis-marker.rs marker_strips_hyphens_and_takes_12
    // 对同一 session id 断言同一字面量，两侧改动必须同步（互引：focus_session
    // marker 注释 / tuvis-marker.rs 模块注释）
    #[test]
    fn matching_side_marker_literal_matches_helper_side() {
        let session_id = "01a08083-5ca0-4948-8276-9a0b8c7d6e5f";
        let marker = format!(
            "TUVIS:{}",
            session_id
                .chars()
                .filter(|c| *c != '-')
                .take(12)
                .collect::<String>()
        );
        assert_eq!(marker, "TUVIS:01a080835ca0");
    }
}
