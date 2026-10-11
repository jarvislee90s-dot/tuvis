use tauri::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuItem, MenuItemKind, PredefinedMenuItem, Submenu,
};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{
    plugin::{Builder, TauriPlugin},
    AppHandle, Emitter, Manager, Runtime,
};
use tauri_plugin_autostart::ManagerExt;

/// 托盘菜单标签集（Task 16 i18n 参数化）：真值源在前端（i18next），经 refresh_tray
/// 下发并持久化到 settings；托盘事件线程 / session.rs 启发式等无前端语境的重建
/// 路径沿用最近一次持久化值（此前预设重建会把基础项打回中文占位、语言切换会
/// 丢预设项——统一重建后两个问题一并消除）。M4 T4 并入后新增 remote 字段
///（远程开关项标签，勾选态/地址 Rust 侧自查 remote::tray_display）。
/// 2026-10-11 并入 autostart 字段（开机自启开关项标签；勾选态 = 重建时实时读
/// autostart 插件状态，同样 Rust 侧自查——真值源在 OS 侧，不落 KV）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct TrayLabels {
    /// 预设分区标签（"预设" / "Presets"）
    pub presets: String,
    pub show: String,
    pub pet: String,
    pub quit: String,
    /// 远程接入开关项标签（"远程接入" / "Remote Access"）
    pub remote: String,
    /// 开机自启开关项标签（"开机自动启动" / "Launch at Login"）
    pub autostart: String,
}

impl Default for TrayLabels {
    fn default() -> Self {
        // 中文兜底：与历史硬编码占位一致（任何前端调用之前的首次重建使用）
        Self {
            presets: "预设".into(),
            show: "显示窗口".into(),
            pet: "显示/隐藏桌宠".into(),
            quit: "退出".into(),
            remote: "远程接入".into(),
            autostart: "开机自动启动".into(),
        }
    }
}

const TRAY_LABELS_KEY: &str = "tray_labels";

/// 启动期托盘标签决策（终审 Minor #2，纯函数）：解析 settings KV 里的持久化
/// 标签——存在且可解析 → Some（init 走统一重建路径，首屏即正确语言且含预设
/// 项）；缺失或损坏（首次启动 / 脏数据）→ None → init 维持英文基础菜单兜底
///（历史行为：首启时尚无前端语境，语言未知，不硬造首屏）。注意与
/// `saved_tray_labels` 的差别：本函数保留「有无持久化值」的信息，后者会把
/// 缺失折叠成中文 Default，无法区分首启与已本地化
fn startup_tray_labels(raw: Option<String>) -> Option<TrayLabels> {
    raw.and_then(|s| serde_json::from_str(&s).ok())
}

fn saved_tray_labels() -> TrayLabels {
    startup_tray_labels(crate::database::get_setting(TRAY_LABELS_KEY)).unwrap_or_default()
}

impl TrayLabels {
    /// 合并语义：传入的标签覆盖持久化值，None 沿用最近一次值，并回写持久化。
    /// 供 refresh_tray 使用——预设变更处可只追加 presetsLabel 一行，
    /// 无需关心基础项文案
    pub fn merged(
        presets: Option<String>,
        show: Option<String>,
        pet: Option<String>,
        quit: Option<String>,
        remote: Option<String>,
        autostart: Option<String>,
    ) -> Self {
        let mut labels = saved_tray_labels();
        if let Some(v) = presets {
            labels.presets = v;
        }
        if let Some(v) = show {
            labels.show = v;
        }
        if let Some(v) = pet {
            labels.pet = v;
        }
        if let Some(v) = quit {
            labels.quit = v;
        }
        if let Some(v) = remote {
            labels.remote = v;
        }
        if let Some(v) = autostart {
            labels.autostart = v;
        }
        // set_setting 返回 ()（内部自兜错），持久化失败不影响本次重建
        crate::database::set_setting(
            TRAY_LABELS_KEY,
            &serde_json::to_string(&labels).unwrap_or_default(),
        );
        labels
    }
}

/// 托盘点击翻转判定（spec §5.5 开关模型 + 裁决 2026-09-15「点击=直接执行」）：
/// 该工具当前激活的正是此预设 → "restore"（关 = 恢复默认）；否则 → "apply"
///（开 = 独占应用；激活的是其他预设时同样走 apply，独占语义下旧开关自动翻关）
pub fn preset_toggle_action(active: Option<&str>, preset_id: &str) -> &'static str {
    if active == Some(preset_id) {
        "restore"
    } else {
        "apply"
    }
}

/// 通用预设子项 id（`preset-tool-{preset_id}|{tool_id}`）解析 → (preset_id, tool_id)。
/// 工具 id 与预设 id（`preset-{ts}-{seq}`）均不含 `|`，split_once 划分无歧义
fn parse_universal_child_id(id: &str) -> Option<(&str, &str)> {
    id.strip_prefix("preset-tool-")
        .and_then(|rest| rest.split_once('|'))
}

/// 工具私有预设顶层项 id（`preset-{preset_id}`）解析 → preset_id。
/// 双前缀陷阱：preset.id 本身以 "preset-" 开头（DAO 生成 `preset-{ts}-{seq}`），
/// 菜单 id 因此形如 `preset-preset-…`——strip_prefix 恰好一次即得 preset.id
fn parse_private_item_id(id: &str) -> Option<&str> {
    id.strip_prefix("preset-")
}

/// 工具当前激活预设（托盘选中态数据源；无基底快照 / 无激活 = None）
fn active_preset_of(tool_id: &str) -> Option<String> {
    crate::database::get_base_snapshot(tool_id).and_then(|(active, _)| active)
}

/// 托盘预设点击 = 直接执行（spec §7.5 + 裁决 2026-09-15，无窗口语境不弹确认）。
/// 执行链路与 commands::preset::apply_preset / restore_preset 完全一致（工具启用
/// 守卫 → service 层；restore 走 restore_tool 而非 command）；成败只记日志不
/// panic；最后统一重建菜单刷新选中态——失败时项也回到当前真实状态，可重试
fn toggle_preset_for_tool<R: Runtime>(app: &AppHandle<R>, preset_id: &str, tool_id: &str) {
    let active = active_preset_of(tool_id);
    let action = preset_toggle_action(active.as_deref(), preset_id);
    let outcome = match action {
        "restore" => crate::services::tool_settings::ensure_tool_enabled(tool_id)
            .and_then(|()| crate::services::preset::restore_tool(tool_id).map(|_| ())),
        _ => crate::services::tool_settings::ensure_tool_enabled(tool_id)
            .and_then(|()| crate::services::preset::apply_preset(preset_id, tool_id).map(|_| ())),
    };
    match outcome {
        Ok(()) => log::info!("托盘预设翻转完成（{action}）: {preset_id} @ {tool_id}"),
        Err(e) => log::warn!("托盘预设翻转失败（{action}）: {preset_id} @ {tool_id}: {e}"),
    }
    let labels = saved_tray_labels();
    if let Err(e) = update_tray_with_presets_labeled(app, &labels) {
        log::warn!("托盘菜单重建失败: {e}");
    }
}

/// 远程接入托盘区（M4 T4）：分隔线 → 开关 CheckMenuItem（勾选态与地址
/// Rust 侧自查 remote::tray_display：隧道开地址优先，关=空串）→ 地址复制项。
/// 泛型 R 为最小适配：init 默认菜单在泛型 Runtime 上构造；显式 'a 为编译适配：
/// `&mut Vec<&dyn>` 不变性要求三个 owned 容器与收集表同享元素生存期；
/// 容器由调用方声明——菜单项必须存活到 Menu 构建完成（借用语义）
fn push_remote_items<'a, R: Runtime>(
    app: &AppHandle<R>,
    items: &mut Vec<&'a dyn IsMenuItem<R>>,
    owned: &'a mut Vec<CheckMenuItem<R>>, // owned 存活容器
    addr_owned: &'a mut Vec<MenuItem<R>>,
    sep_owned: &'a mut Vec<PredefinedMenuItem<R>>,
    remote_on_text: &str,
) -> Result<(), String> {
    let (enabled, addr) = crate::remote::tray_display();
    let sep = PredefinedMenuItem::separator(app).map_err(|e| e.to_string())?;
    let toggle = CheckMenuItem::with_id(app, "remote", remote_on_text, true, enabled, None::<&str>)
        .map_err(|e| e.to_string())?;
    // 地址为空（远程关）时展示占位「—」并禁用点击复制
    // （先取 enabled 与标签再构造：addr 在拼标签时被移动，
    // 行内写法 `&if …else { addr }` 后再读 addr 是 E0382，语义不变仅调序）
    let addr_enabled = !addr.is_empty();
    let addr_label = if addr.is_empty() {
        "—".to_string()
    } else {
        addr
    };
    let addr_item = MenuItem::with_id(app, "remote-addr", &addr_label, addr_enabled, None::<&str>)
        .map_err(|e| e.to_string())?;
    sep_owned.push(sep);
    owned.push(toggle);
    addr_owned.push(addr_item);
    items.push(sep_owned.last().unwrap() as _);
    items.push(owned.last().unwrap() as _);
    items.push(addr_owned.last().unwrap() as _);
    Ok(())
}

/// 开机自启勾选项（2026-10-11）：插在「显示主窗口」之后紧跟（用户裁决）。
/// 勾选态 = `autostart` 插件实时读的 OS 侧真值（注册表 Run 键 / LaunchAgent
/// 实际注册与否）——插件 `ManagerExt::autolaunch` 提供。三条重建路径统一走本
/// helper 防漏挂（对齐 push_remote_items 模式；owned 容器存活语义见其注释）
fn push_autostart_item<'a, R: Runtime>(
    app: &AppHandle<R>,
    items: &mut Vec<&'a dyn IsMenuItem<R>>,
    owned: &'a mut Vec<CheckMenuItem<R>>,
    label: &str,
) -> Result<(), String> {
    let checked = app.autolaunch().is_enabled().unwrap_or(false);
    let item = CheckMenuItem::with_id(app, "autostart", label, true, checked, None::<&str>)
        .map_err(|e| e.to_string())?;
    owned.push(item);
    items.push(owned.last().unwrap() as _);
    Ok(())
}

// Update tray menu with localized text（遗留基础路径：lib.rs 命令仍注册，
// 前端已改走 refresh_tray 统一重建；保留至下个清理窗口与命令一并移除）
pub fn update_tray_menu(
    app: &AppHandle,
    show_text: &str,
    quit_text: &str,
    pet_text: &str,
    remote_on_text: &str,
    autostart_text: &str,
) -> Result<(), String> {
    // owned 容器 + 引用收集：自启项紧跟 show，远程区项（sep → 开关 → 地址）统一插在 quit 之前
    let show =
        MenuItem::with_id(app, "show", show_text, true, None::<&str>).map_err(|e| e.to_string())?;
    let pet =
        MenuItem::with_id(app, "pet", pet_text, true, None::<&str>).map_err(|e| e.to_string())?;
    let sep1 = PredefinedMenuItem::separator(app).map_err(|e| e.to_string())?;
    let quit =
        MenuItem::with_id(app, "quit", quit_text, true, None::<&str>).map_err(|e| e.to_string())?;

    let mut items: Vec<&dyn IsMenuItem<tauri::Wry>> = vec![&show];
    let mut autostart_owned: Vec<CheckMenuItem<tauri::Wry>> = Vec::new();
    push_autostart_item(app, &mut items, &mut autostart_owned, autostart_text)?;
    items.push(&pet);
    items.push(&sep1);
    let mut remote_owned: Vec<CheckMenuItem<tauri::Wry>> = Vec::new();
    let mut remote_addr_owned: Vec<MenuItem<tauri::Wry>> = Vec::new();
    let mut remote_seps: Vec<PredefinedMenuItem<tauri::Wry>> = Vec::new();
    push_remote_items(
        app,
        &mut items,
        &mut remote_owned,
        &mut remote_addr_owned,
        &mut remote_seps,
        remote_on_text,
    )?;
    items.push(&quit);

    let menu = Menu::with_id_and_items(app, "system-tray", &items).map_err(|e| e.to_string())?;

    if let Some(tray) = app.tray_by_id("main-tray") {
        tray.set_menu(Some(menu)).map_err(|e| e.to_string())?;
    }

    Ok(())
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("system-tray")
        .setup(|app, _| {
            // Create tray menu with default English text
            // 三重建路径统一（M4 T4）：默认菜单同样追加远程区项，否则前端首次重建前
            // 托盘缺远程入口（文本为英文占位，主窗口挂载后由前端本地化重建）
            let show = MenuItem::with_id(app, "show", "Show Window", true, None::<&str>)?;
            let pet = MenuItem::with_id(app, "pet", "Show Pet", true, None::<&str>)?;
            let sep1 = PredefinedMenuItem::separator(app)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;

            let mut items: Vec<&dyn IsMenuItem<R>> = vec![&show];
            // 开机自启勾选项（2026-10-11）：勾选态 = OS 侧实时真值；文本为英文
            // 占位（与同路径其它基础项一致），主窗口挂载后由前端本地化重建
            let mut autostart_owned: Vec<CheckMenuItem<R>> = Vec::new();
            push_autostart_item(app, &mut items, &mut autostart_owned, "Launch at Login")?;
            items.push(&pet);
            items.push(&sep1);
            let mut remote_owned: Vec<CheckMenuItem<R>> = Vec::new();
            let mut remote_addr_owned: Vec<MenuItem<R>> = Vec::new();
            let mut remote_seps: Vec<PredefinedMenuItem<R>> = Vec::new();
            push_remote_items(
                app,
                &mut items,
                &mut remote_owned,
                &mut remote_addr_owned,
                &mut remote_seps,
                "Remote Access",
            )?;
            items.push(&quit);

            let menu = Menu::with_id_and_items(app, "system-tray", &items)?;

            // Build tray icon
            TrayIconBuilder::with_id("main-tray")
                .menu(&menu)
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Tuvis")
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        // Left click to show main window
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.unminimize();
                            let _ = window.set_focus();
                        }
                    }
                })
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.unminimize();
                            let _ = window.set_focus();
                        }
                    }
                    "pet" => {
                        // 托盘切换桌宠显隐：以窗口实际可见性为准，并广播给前端同步（spec §10.2）
                        if let Some(w) = app.get_webview_window("pet") {
                            let visible = w.is_visible().unwrap_or(false);
                            let next = !visible;
                            if next {
                                let _ = w.show();
                            } else {
                                let _ = w.hide();
                            }
                            let _ = app.emit(
                                "pet-visibility-changed",
                                serde_json::json!({ "visible": next }),
                            );
                        }
                    }
                    "quit" => {
                        app.exit(0);
                    }
                    "remote" => {
                        // 托盘开关：与设置页同一命令链路（remote_toggle 内含 TLS 门与回滚）。
                        // 菜单勾选态经前端回环刷新（remote-changed 监听重建，见 home.tsx 接线）
                        let next = !crate::remote::tray_display().0;
                        match crate::remote::remote_toggle(next) {
                            Ok(()) => {
                                crate::remote::events::emit_ui(
                                    "remote-changed",
                                    serde_json::json!({ "enabled": next }),
                                );
                            }
                            Err(e) => {
                                crate::remote::events::emit_ui(
                                    "remote-toggle-failed",
                                    serde_json::json!({ "error": e }),
                                );
                            }
                        }
                    }
                    "remote-addr" => {
                        let (_, addr) = crate::remote::tray_display();
                        if !addr.is_empty() {
                            crate::remote::events::emit_ui(
                                "remote-copy-addr",
                                serde_json::json!({ "url": addr }),
                            );
                        }
                    }
                    "autostart" => {
                        // 开机自启翻转（2026-10-11）：读写全链收在 flip_and_sync 单点
                        //（含 Windows 引号修复 I1 / debug 拒写门 N3 / 事件广播）。
                        // 读失败不猜方向（评审 M1）：只告警，不翻转——随后统一重建让
                        // 勾选态回落真实状态；翻转失败同样靠重建回落
                        match app.autolaunch().is_enabled() {
                            Ok(current) => {
                                if let Err(e) =
                                    crate::commands::autostart::flip_and_sync(app, !current)
                                {
                                    log::warn!("托盘开机自启翻转失败: {e}");
                                } else {
                                    log::info!("托盘开机自启翻转完成: {} → {}", current, !current);
                                }
                            }
                            Err(e) => log::warn!("托盘开机自启：当前档不可判定，不翻转: {e}"),
                        }
                        let labels = saved_tray_labels();
                        if let Err(e) = update_tray_with_presets_labeled(app, &labels) {
                            log::warn!("托盘菜单重建失败: {e}");
                        }
                    }
                    // 预设接线（spec §7.5，P5）：点击 = 直接执行翻转（裁决 2026-09-15）。
                    // 通用预设子项 id `preset-tool-{preset_id}|{tool_id}` → 对该工具翻转
                    id if id.starts_with("preset-tool-") => {
                        if let Some((preset_id, tool_id)) = parse_universal_child_id(id) {
                            toggle_preset_for_tool(app, preset_id, tool_id);
                        } else {
                            log::warn!("托盘预设子项 id 无法解析: {id}");
                        }
                    }
                    // 通用预设子菜单标题：点击仅展开，无动作（防御性吞掉，防止误入下方私有分支）
                    id if id.starts_with("preset-group-") => {}
                    // 工具私有预设顶层项：strip 一次得 preset.id（双前缀陷阱见
                    // parse_private_item_id 注释），作用于其绑定工具
                    id if id.starts_with("preset-") => {
                        if let Some(preset_id) = parse_private_item_id(id) {
                            match crate::database::get_preset(preset_id) {
                                Some(preset) => match preset.bound_tool.as_deref() {
                                    Some(tool_id) => {
                                        toggle_preset_for_tool(app, preset_id, tool_id);
                                    }
                                    None => {
                                        log::warn!("预设 {} 无绑定工具，忽略托盘点击", preset.id)
                                    }
                                },
                                None => log::warn!("托盘预设项无对应预设: {preset_id}"),
                            }
                        }
                    }
                    _ => {}
                })
                .build(app)?;

            // 启动即本地化（终审 Minor #2）：有持久化标签 → 复用统一重建路径
            //（update_tray_with_presets_labeled，不另写第二套菜单组装），首屏
            // 即正确语言且含预设项；无持久化（首次启动）→ 维持上方英文基础菜单
            // 兜底。必须放在 build 之后（set_menu 需已注册的 "main-tray"，
            // TrayIconBuilder::build 会同步注册）；失败仅告警不 panic，基础菜单
            // 仍在。DB 在 run() 里先于 Builder 构建（database::init），此处
            // get_setting 已可用；plugin setup 收到的 app 即 &AppHandle，直接复用
            if let Some(labels) = startup_tray_labels(crate::database::get_setting(TRAY_LABELS_KEY))
            {
                if let Err(e) = update_tray_with_presets_labeled(app, &labels) {
                    log::warn!("托盘启动重建失败，维持默认基础菜单: {e}");
                }
            }
            Ok(())
        })
        .on_window_ready(move |window| {
            let window_clone = window.clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    // Hide window instead of exiting when close is requested
                    let _ = window_clone.hide();
                    api.prevent_close();
                }
            });
        })
        .build()
}

/// 更新托盘聚合状态（红/黄/绿灯 + 等待数）
pub fn update_tray_status(
    app: &AppHandle,
    waiting_count: usize,
    total_count: usize,
    has_processing: bool,
) {
    if let Some(tray) = app.tray_by_id("main-tray") {
        // 托盘标题：🔴N 等待 / 🟡 运行中 / 🟢 空闲 / 空
        let title = if waiting_count > 0 {
            format!("\u{1F534}{}", waiting_count)
        } else if has_processing {
            "\u{1F7E1}".to_string()
        } else if total_count > 0 {
            "\u{1F7E2}".to_string()
        } else {
            String::new()
        };
        let _ = tray.set_title(if title.is_empty() { None } else { Some(&title) });

        // 托盘提示
        let tooltip = if total_count == 0 {
            "Tuvis".to_string()
        } else if waiting_count > 0 {
            format!(
                "Tuvis \u{2014} {} sessions, {} waiting",
                total_count, waiting_count
            )
        } else {
            format!("Tuvis \u{2014} {} sessions", total_count)
        };
        let _ = tray.set_tooltip(Some(&tooltip));
    }
}

/// 兜底重建（无前端语境的调用方：session.rs 计数启发式 / 托盘翻转后自刷新）：
/// 沿用最近一次前端下发的标签集（TrayLabels），首次（无持久化值）用中文默认
pub fn update_tray_with_presets(app: &AppHandle) -> Result<(), String> {
    let labels = saved_tray_labels();
    update_tray_with_presets_labeled(app, &labels)
}

/// 统一重建托盘菜单（Task 16）：基础项 + 预设项合并成型，预设项带开/关选中态
///（CheckMenuItem，checked = 该预设在此工具上激活）。通用预设 = 每个已启用工具
/// 一个子项（子菜单展开，子项 id `preset-tool-{preset_id}|{tool_id}`）；
/// 工具私有预设 = 顶层 CheckMenuItem（id `preset-{preset_id}`，作用于绑定工具）。
/// 点击语义在 on_menu_event 的 preset 臂（直接执行，不弹确认）。
/// M4 T4 并入：quit 前追加远程区项（分隔线 → 开关 → 地址），三重建路径统一
pub fn update_tray_with_presets_labeled<R: Runtime>(
    app: &AppHandle<R>,
    labels: &TrayLabels,
) -> Result<(), String> {
    let presets = crate::database::list_presets();

    // 基础项（owned，存活于本函数作用域内）
    let show = MenuItem::with_id(app, "show", &labels.show, true, None::<&str>)
        .map_err(|e| e.to_string())?;
    let pet = MenuItem::with_id(app, "pet", &labels.pet, true, None::<&str>)
        .map_err(|e| e.to_string())?;
    let sep1 = PredefinedMenuItem::separator(app).map_err(|e| e.to_string())?;
    let quit = MenuItem::with_id(app, "quit", &labels.quit, true, None::<&str>)
        .map_err(|e| e.to_string())?;

    // 自启项紧跟 show（用户裁决位置；勾选态 = OS 侧实时真值）
    let mut autostart_owned: Vec<CheckMenuItem<R>> = Vec::new();
    let mut items: Vec<&dyn IsMenuItem<R>> = Vec::new();
    items.push(&show);
    push_autostart_item(app, &mut items, &mut autostart_owned, &labels.autostart)?;
    items.push(&pet);
    items.push(&sep1);

    // 已启用工具（通用预设子菜单的子项集；与 commands::settings::list_enabled_tools 同口径）
    let enabled_tools: Vec<(String, String)> = crate::adapter::TOOL_IDS
        .iter()
        .filter(|id| crate::database::dao::agent_tool::get_tool_enabled(id))
        .filter_map(|id| {
            crate::adapter::adapter_by_id(id).map(|a| (id.to_string(), a.name().to_string()))
        })
        .collect();

    // 预设项统一收集为 MenuItemKind 保序（通用 = 子菜单 / 私有 = CheckMenuItem，
    // 与 list_presets 的 created_at DESC 序一致）
    let mut preset_items: Vec<MenuItemKind<R>> = Vec::new();

    for preset in &presets {
        let label = format!("{}: {}", labels.presets, preset.name);
        match preset.scope.as_str() {
            "universal" => {
                let submenu =
                    Submenu::with_id(app, format!("preset-group-{}", preset.id), &label, true)
                        .map_err(|e| e.to_string())?;
                let mut children = 0usize;
                for (tool_id, tool_name) in &enabled_tools {
                    let checked = active_preset_of(tool_id).as_deref() == Some(preset.id.as_str());
                    let child = CheckMenuItem::with_id(
                        app,
                        format!("preset-tool-{}|{}", preset.id, tool_id),
                        tool_name,
                        true,
                        checked,
                        None::<&str>,
                    )
                    .map_err(|e| e.to_string())?;
                    submenu.append(&child).map_err(|e| e.to_string())?;
                    children += 1;
                }
                // 无已启用工具时不渲染空子菜单
                if children > 0 {
                    preset_items.push(submenu.kind());
                }
            }
            "tool" => {
                let Some(tool_id) = preset.bound_tool.as_deref() else {
                    // bound_tool 缺失的脏数据：无处作用的开关不渲染
                    continue;
                };
                let checked = active_preset_of(tool_id).as_deref() == Some(preset.id.as_str());
                let item = CheckMenuItem::with_id(
                    app,
                    format!("preset-{}", preset.id),
                    &label,
                    true,
                    checked,
                    None::<&str>,
                )
                .map_err(|e| e.to_string())?;
                preset_items.push(item.kind());
            }
            other => log::warn!("未知预设 scope `{other}`（id={}），托盘跳过", preset.id),
        }
    }

    // 预设区与基础区之间的分隔线（仅确有预设项时插入，避免相邻双分隔线）
    let sep2 = if preset_items.is_empty() {
        None
    } else {
        Some(PredefinedMenuItem::separator(app).map_err(|e| e.to_string())?)
    };

    // 预设项接在基础项（show → 自启 → pet → sep1）之后
    for item in &preset_items {
        items.push(item);
    }
    if let Some(ref s) = sep2 {
        items.push(s);
    }
    // 远程区（M4 T4）：统一插在 quit 之前——本路径漏加则任何统一重建抹掉远程项
    let mut remote_owned: Vec<CheckMenuItem<R>> = Vec::new();
    let mut remote_addr_owned: Vec<MenuItem<R>> = Vec::new();
    let mut remote_seps: Vec<PredefinedMenuItem<R>> = Vec::new();
    push_remote_items(
        app,
        &mut items,
        &mut remote_owned,
        &mut remote_addr_owned,
        &mut remote_seps,
        &labels.remote,
    )?;
    items.push(&quit);

    let menu = Menu::with_items(app, &items).map_err(|e| e.to_string())?;

    if let Some(tray) = app.tray_by_id("main-tray") {
        tray.set_menu(Some(menu)).map_err(|e| e.to_string())?;
    }

    Ok(())
}

#[cfg(test)]
mod preset_toggle_tests {
    use super::{parse_private_item_id, parse_universal_child_id, preset_toggle_action};

    /// spec §5.5：无激活 → 开（应用）
    #[test]
    fn no_active_means_apply() {
        assert_eq!(preset_toggle_action(None, "preset-1726000000-0"), "apply");
    }

    /// spec §5.5：激活的是其他预设 → 仍开（独占切换，旧开关自动翻关）
    #[test]
    fn other_active_means_apply() {
        assert_eq!(
            preset_toggle_action(Some("preset-1726000000-1"), "preset-1726000000-0"),
            "apply"
        );
    }

    /// spec §5.5：激活的正是本预设 → 关（恢复默认）
    #[test]
    fn same_active_means_restore() {
        assert_eq!(
            preset_toggle_action(Some("preset-1726000000-0"), "preset-1726000000-0"),
            "restore"
        );
    }

    /// 通用子项 id 解析：`preset-tool-{preset_id}|{tool_id}` 按首个 `|` 二分
    #[test]
    fn universal_child_id_splits_on_first_pipe() {
        let (preset_id, tool_id) =
            parse_universal_child_id("preset-tool-preset-1726000000-0|codex").unwrap();
        assert_eq!(preset_id, "preset-1726000000-0");
        assert_eq!(tool_id, "codex");
    }

    /// 无 `|`（或缺前缀）的子项 id 必须解析失败，走 warn 而非 panic
    #[test]
    fn universal_child_id_without_pipe_is_none() {
        assert!(parse_universal_child_id("preset-tool-preset-1726000000-0").is_none());
        assert!(parse_universal_child_id("show").is_none());
    }

    /// 双前缀陷阱回归锁：preset.id 以 "preset-" 开头，菜单 id 形如
    /// `preset-preset-…`——strip 恰好一次必须还原完整 preset.id
    #[test]
    fn private_item_id_strips_exactly_once() {
        assert_eq!(
            parse_private_item_id("preset-preset-1726000000-0"),
            Some("preset-1726000000-0")
        );
        assert_eq!(parse_private_item_id("show"), None);
    }
}

#[cfg(test)]
mod startup_tray_labels_tests {
    use super::{startup_tray_labels, TrayLabels};

    /// 终审 Minor #2：有持久化标签 → Some → init 走统一重建（首屏本地化 + 预设项）
    #[test]
    fn persisted_labels_parse_to_some() {
        let raw = serde_json::json!({
            "presets": "Presets",
            "show": "Show Window",
            "pet": "Toggle Pet",
            "quit": "Quit"
        })
        .to_string();
        let labels = startup_tray_labels(Some(raw)).expect("完整持久化值应解析为 Some");
        assert_eq!(labels.presets, "Presets");
        assert_eq!(labels.show, "Show Window");
        assert_eq!(labels.pet, "Toggle Pet");
        assert_eq!(labels.quit, "Quit");
    }

    /// 首次启动（KV 无值）→ None → init 维持英文基础菜单兜底（历史行为）
    #[test]
    fn missing_value_is_none() {
        assert!(startup_tray_labels(None).is_none());
    }

    /// 持久化值损坏 → None（同样兜底，不 panic）
    #[test]
    fn corrupt_value_is_none() {
        assert!(startup_tray_labels(Some("not-json".into())).is_none());
    }

    /// serde(default) 容错口径：半截 JSON（缺字段）按 Default 补齐仍可用，
    /// 不判为损坏——语言切换只回写部分字段的旧版本数据也能正常恢复。
    /// M4 T4 并入后 remote 同样享受缺省补齐（旧持久化值无 remote 字段）；
    /// 2026-10-11 autostart 同（升级用户的旧 tray_labels 值无该字段）
    #[test]
    fn partial_json_fills_defaults() {
        let labels =
            startup_tray_labels(Some(r#"{"presets":"预设"}"#.into())).expect("缺字段应按默认补齐");
        assert_eq!(labels.presets, "预设");
        assert_eq!(labels.show, TrayLabels::default().show);
        assert_eq!(labels.quit, TrayLabels::default().quit);
        assert_eq!(labels.remote, TrayLabels::default().remote);
        assert_eq!(labels.autostart, TrayLabels::default().autostart);
    }
}
