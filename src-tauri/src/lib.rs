pub mod adapter;
pub mod commands;
pub mod database;
pub mod errcode;
pub mod inject;
pub mod linker;
pub mod monitor;
pub mod plugins;
pub mod remote;
pub mod services;
pub mod session;
pub mod window;

/// 测试专用工具（临时目录工厂等）：仅测试构建可见，不进发布产物。
#[cfg(test)]
pub(crate) mod test_support;

use tauri::Manager;
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

/// 应用进程本次运行的稳定标识（进程内恒定、跨进程重启必变；首调时刻的纳秒时间戳）。
/// 桌宠「物理坠落」按应用重启归位（2026-10-13 用户裁决）：前端启动时拿它比对
/// localStorage 的运行标记，不一致 ⇒ 新一轮运行 ⇒ gravity 重置为关（会话内可再开）。
/// 纯进程内时间戳，不落盘。
#[tauri::command]
fn app_run_id() -> String {
    static RUN_ID: once_cell::sync::Lazy<String> = once_cell::sync::Lazy::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos().to_string())
            .unwrap_or_default()
    });
    RUN_ID.clone()
}

#[tauri::command]
fn update_tray_menu(
    app: tauri::AppHandle,
    show_text: String,
    quit_text: String,
    pet_text: String,
    remote_on_text: String,
    autostart_text: String,
) -> Result<(), String> {
    plugins::system_tray::update_tray_menu(
        &app,
        &show_text,
        &quit_text,
        &pet_text,
        &remote_on_text,
        &autostart_text,
    )
}

/// 托盘菜单统一重建（Task 16）：基础项 + 预设项（带开/关选中态）一次成型。
/// 标签合并语义：传入的标签覆盖持久化值，未传的沿用最近一次值——预设增删/
/// 开关变化处可只追加 presetsLabel 一行调用，无需关心基础项文案。
/// M4 T4 并入：新增 remote_on_text（远程开关项标签，勾选态/地址 Rust 侧自查）。
/// 2026-10-11 并入：autostart_text（开机自启开关项标签，勾选态 Rust 侧实时
/// 自查 autostart 插件状态）；未传沿用持久化值（旧持久化值缺字段按默认补齐）。
/// `update_tray_menu` 保留但前端已不再调用（保留至下个清理窗口移除）
#[tauri::command]
fn refresh_tray(
    app: tauri::AppHandle,
    presets_label: Option<String>,
    show_text: Option<String>,
    pet_text: Option<String>,
    quit_text: Option<String>,
    remote_on_text: Option<String>,
    autostart_text: Option<String>,
) -> Result<(), String> {
    let labels = plugins::system_tray::TrayLabels::merged(
        presets_label,
        show_text,
        pet_text,
        quit_text,
        remote_on_text,
        autostart_text,
    );
    plugins::system_tray::update_tray_with_presets_labeled(&app, &labels)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 日志**写文件**而非 stderr（2026-10-03 终端污染事故）：debug exe 以分离方式
    // 启动时没有自己的控制台，AttachConsole 读屏/注入期间其它线程的 stderr 写入
    // 会落进**被附加的外部终端**（活体实证：claude TUI 表单行被 兔维斯 WARN 日志
    // 覆盖 → 屏读解析失败 → 自由作答中止）。写 ~/.tuvis/logs/mam.log 彻底杜绝，
    // 且日志可回查。文件打不开（权限等）→ 回落 stderr（旧行为）。
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    let opened = dirs::home_dir().and_then(|h| {
        let dir = h.join(".tuvis").join("logs");
        let _ = std::fs::create_dir_all(&dir);
        let log = dir.join("mam.log");
        // **5MB 轮转**（评审 M4）：info 级每轮扫描都有行，无上限会无限膨胀——
        // 超限把当前文件挪去 mam.log.old（单代轮转，够回查）再重新开
        if let Ok(meta) = std::fs::metadata(&log) {
            if meta.len() > 5 * 1024 * 1024 {
                let _ = std::fs::rename(&log, dir.join("mam.log.old"));
            }
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .ok()
    });
    let _ = match opened {
        Some(file) => builder
            .target(env_logger::Target::Pipe(Box::new(file)))
            .try_init(),
        None => builder.try_init(), // 回落 stderr：终端污染可能复发（M4 申报——可检测）
    };
    // **panic 消息也写日志文件**（评审 M3）：默认 panic hook 写 stderr，AttachConsole
    // 竞态下同样会污染外部终端。包一层：先落文件再交还原 hook。
    let panic_target = dirs::home_dir().map(|h| h.join(".tuvis").join("logs").join("mam.log"));
    // 默认 hook 在**非 panicking 时**先取下保存——hook 内调用 take_hook 在新版
    // Rust 会 panic（cannot modify the panic hook from a panicking thread），
    // 递归 panic 直接 abort 且吞掉真实错误（12:47 启动实录）
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(path) = &panic_target {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                use std::io::Write;
                let _ = writeln!(f, "[PANIC] {info}");
            }
        }
        default_hook(info);
    }));
    database::init();
    // 后台增量导入（仅导入 DB 中不存在的 name）+ 补链，不阻塞启动
    std::thread::spawn(|| {
        // 清扫 .import-staging 崩溃残留（issue #32-3）：必须先于增量导入/补链执行，
        // 二者可能耗时数秒，期间 IPC 已可用、用户可能已发起导入，晚清扫会误删活跃暂存区
        services::pet::sweep_staging();
        // 预设 v2：孤儿暂存回移 + 注册表回填（先于导入/补链，保证表口径就绪）
        services::preset::stash::recover_orphans();
        for msg in services::preset::check_snapshot_invariants() {
            log::warn!("预设快照不变量违背: {}", msg);
        }
        services::resource::backfill_registry();
        // 预设 v2（spec §13）：账本-磁盘漂移扫描，启动时 warn 收口
        for d in services::resource::reconcile::scan_drift() {
            log::warn!("[漂移{}] {} {}", d.kind, d.extension_id, d.path);
        }
        services::auto_import_extensions(false);
        services::sync_imported_skill_links();
    });
    monitor::hooks::register_all_hooks();

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_focus();
                let _ = window.unminimize();
                let _ = window.show();
            }
        }))
        .setup(|app| {
            // 开机自启静默面（2026-10-11）：自启注册项带 --tuvis-autostart 识别参数，
            // 检测到 → 主窗口不弹（托盘常驻，需要时从托盘点开）。手动启动无此参数，
            // 永远正常显窗；桌宠不受影响（按其自身开关照常出现）。窗口虽以
            // visible:true 静态创建，但 setup 先于事件循环首轮绘制，此处 hide 不闪窗
            let argv: Vec<String> = std::env::args().collect();
            if commands::autostart::is_autostart_launch(&argv) {
                log::info!("开机自启启动：主窗口静默（托盘常驻）");
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }
            // 在 dev 模式下自动打开 devtools，启用 CDP 远程调试
            #[cfg(debug_assertions)]
            {
                if let Some(window) = app.get_webview_window("main") {
                    window.open_devtools();
                }
            }
            // 桌宠窗口：延迟创建（spec §4.1）。不能在 setup 里立即建——Windows 上主窗口
            // WebView2 控制器初始化期存在竞态，立即创建会偶发 E_INVALIDARG 且被
            // tauri 吞错成幽灵窗口（详见 commands/pet.rs 模块注释）；延迟 800ms 避开
            let pet_handle = app.handle().clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(800));
                if let Err(e) = commands::pet::create_pet_window(&pet_handle) {
                    log::warn!("pet window create failed: {}", e);
                }
            });
            // M4：全局句柄落位（先于 restore_on_launch——隧道自启即可发通知）
            let _ = crate::remote::events::APP_HANDLE.set(app.handle().clone());
            // T5（2A 改操作卡）：codex 信任门常驻操作卡——register_all_hooks 在
            // run() 早期执行（彼时 builder 未构建、AppHandle 不可得）只置 pending
            // KV，此处句柄就绪后消费：pending 且未销账（shown）才弹；销账只发生在
            // 用户点「复制命令」/「不再提醒」（codex_trust_notice_ack 命令）
            monitor::hooks::consume_codex_trust_notice(app.handle());
            // M2 远程接入：按设置恢复远程服务器（开机自启语义；内部用
            // tauri::async_runtime，无 runtime 上下文的主线程可安全调用）
            crate::remote::restore_on_launch();
            // 用量账本：应用启动采集一次（按需路径，绝不进 3 秒轮询）
            crate::services::usage::collect::spawn_initial_collection();
            // 升级残留清理：Windows 升级流把安装包留在 %TEMP% 的
            // `{app}-*-updater-*` 目录（插件装完 exit(0) 不回收），启动即扫除
            crate::commands::updater::cleanup_updater_temp_dirs(app.handle());
            Ok(())
        })
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        // 开机自动启动（2026-10-11）：注册时携带 --tuvis-autostart 识别参数，
        // 插件把它写进平台自启项（注册表 Run 键 / launchd plist）
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![commands::autostart::AUTOSTART_ARG]),
        ))
        .plugin(plugins::system_tray::init());
    let builder = builder.invoke_handler(tauri::generate_handler![
        greet,
        // 物理坠落按应用重启归位的前端运行标记源（见上方 app_run_id 注释）
        app_run_id,
        update_tray_menu,
        refresh_tray,
        commands::session::get_all_sessions,
        commands::session::focus_session,
        commands::session::focus_hwnd,
        commands::session::kill_session,
        commands::session::dismiss_session_card,
        // M6R–M9R Task 11：R5 一键 resume（桌面卡「在电脑上打开」）
        commands::session::session_open,
        commands::notification::show_notification_window,
        // 2A：codex 信任门操作卡销账（前端操作卡按钮 ack）
        commands::notification::codex_trust_notice_ack,
        commands::pet::set_pet_visible,
        commands::pet::set_pet_always_on_top,
        commands::pet::pet_list_pets,
        commands::pet::pet_list_codex_pets,
        commands::pet::pet_scan,
        commands::pet::pet_read_manifest,
        commands::pet::pet_stage_from_folder,
        commands::pet::pet_stage_from_zip,
        commands::pet::pet_stage_from_codex,
        commands::pet::pet_stage_from_petdex,
        commands::pet::pet_stage_audio,
        commands::pet::pet_remove_staged_audio,
        commands::pet::pet_finalize_import,
        commands::pet::pet_cancel_import,
        commands::pet::pet_update_manifest,
        commands::pet::pet_rename_pet,
        commands::pet::pet_delete_pet,
        commands::pet::pet_add_voice_files,
        commands::pet::pet_remove_voice_file,
        commands::pet::pet_reveal_folder,
        commands::resource::list_extensions_with_assignments,
        commands::resource::open_tool_resource,
        commands::resource::scan_native_resources,
        commands::resource::import_native_resources,
        commands::resource::list_frontmatter_suggestions,
        commands::resource::list_tool_resources,
        commands::resource::check_preset_compatibility,
        commands::resource::list_ssot_resources,
        commands::resource::scan_ledger_drift,
        commands::resource::reconcile_item,
        commands::resource::reconcile_tool_batch,
        commands::resource::scan_empty_dirs,
        commands::resource::clean_empty_dirs,
        commands::resource::reveal_dir,
        commands::resource::detect_duplicate_skills,
        commands::resource::cleanup_duplicate_skills,
        commands::resource::check_skill_target_type,
        commands::resource::disable_skill_for_tool,
        commands::resource::enable_skill_for_tool_cmd,
        commands::resource::import_mcp_to_ssot,
        commands::resource::save_mcp_config,
        commands::resource::detect_legacy_agents_links,
        commands::resource::migrate_legacy_agents_links,
        commands::preset::create_preset,
        commands::preset::get_preset,
        commands::preset::update_preset,
        commands::preset::restore_preset,
        commands::preset::get_active_preset,
        commands::preset::list_active_presets,
        commands::preset::get_tool_active_resources,
        commands::preset::preview_apply_preset,
        commands::preset::set_resource_binding,
        commands::preset::list_resource_bindings,
        commands::preset::delete_resource_binding,
        commands::preset::set_tool_resident,
        commands::preset::list_tool_residents,
        commands::preset::delete_preset,
        commands::preset::list_presets,
        commands::preset::apply_preset,
        commands::preset::deactivate_preset,
        commands::preset::apply_preset_to_subagent,
        commands::preset::deactivate_preset_from_subagent,
        commands::preset::get_preset_health,
        commands::preset::restore_stash_entry,
        commands::skill::list_repo_skills,
        commands::skill::install_skill,
        commands::skill::rescan_skills,
        commands::skill::assign_skill_to_subagent,
        commands::mcp::toggle_mcp_for_tool,
        commands::mcp::read_mcp_servers,
        commands::mcp::write_mcp_server,
        commands::mcp::remove_mcp_server,
        // 开机自动启动（2026-10-11）：真值源 = OS 侧插件状态，设置页与托盘共用
        commands::autostart::get_autostart_enabled,
        commands::autostart::set_autostart_enabled,
        commands::plugin::toggle_plugin_for_tool,
        commands::settings::get_setting,
        commands::settings::set_setting,
        commands::settings::set_theme,
        commands::settings::detect_tools,
        commands::settings::detect_subagents,
        // 2026-09-20：数据管理首版（移动端附件占用列出/清理——路径服务端解析，
        // 清理目标必须命中上传索引，不接受客户端任意路径）
        commands::data_management::list_attachment_projects,
        commands::data_management::clean_attachment_project,
        commands::settings::list_sub_agents,
        commands::settings::mark_session_read,
        commands::settings::get_tool_settings,
        commands::settings::update_tool_settings,
        commands::settings::list_enabled_tools,
        // T5 信号健康度：per-tool hook 通道状态（设置页「信号健康度」分区按需查询）
        commands::settings::hook_signal_health,
        commands::screenshot::capture_window_screenshot,
        commands::screenshot::list_screenshots,
        commands::manifest::validate_manifest,
        commands::manifest::install_resource_from_manifest,
        commands::manifest::uninstall_resource,
        commands::manifest::get_store_index,
        remote::remote_toggle,
        remote::remote_status,
        remote::remote_confirm_public,
        remote::remote_devices,
        remote::remote_revoke_device,
        remote::remote_revoke_all_devices,
        // M5 A4：访问密码设置 / 重置设备 / 设备重命名（吊销收窄后的新口径命令）
        remote::remote_set_pin,
        remote::remote_reset_devices,
        remote::remote_rename_device,
        // M5 A5：三通道独立开关（旧 remote_set_channel 单值三选一已随之下线）
        remote::remote_toggle_channel,
        // §C2：Tailscale 首次配置引导一条龙（向导探测 + 单步触发）
        remote::remote_ts_probe,
        remote::remote_ts_run_step,
        // M7 W5：桌面端写审计查看（最近 N 条，只读）
        inject::inject_list_audit,
        // 用量域（计划①）：采集 / 大看板 / 记录页 / CSV / 设置读写
        commands::usage::usage_collect,
        commands::usage::usage_dashboard,
        commands::usage::usage_records,
        commands::usage::usage_export_csv,
        commands::usage::usage_get_settings,
        commands::usage::usage_set_settings,
        // 导出落盘（计划①，契约 §3 新增）：文本/二进制写导出目录（2026-10-07 A2 起 = 系统下载目录）
        commands::export::export_save_text,
        commands::export::export_save_bytes,
        // 升级检查（prerelease 渠道）：GitHub 发现层 + 动态端点一键升级
        commands::updater::check_for_github_update,
        commands::updater::install_github_update,
    ]);

    // 仅 release 构建注册 updater（评审 I3 修正，2026-10-08）：
    // 原先的 TAURI_SIGNING_PRIVATE_KEY 运行时门（8dc1147）防的是「conf 里还是占位
    // URL」时代——现在 conf 已是真实端点+公钥，且签名私钥仅构建期需要（bundler 签
    // latest.json 用），终端用户机器运行时不存在该变量；保留 env 门会让所有用户的
    // 应用内升级恒失败。debug 构建仍不注册（无签名产物可验，install 命令有降级提示）
    #[cfg(not(debug_assertions))]
    let builder = builder.plugin(tauri_plugin_updater::Builder::new().build());

    // M4：退出钩子（spec §8 应用退出清理子进程与电源锁）——旧 `.run(ctx)` 无事件回调，
    // 改为 build + run 回调：RunEvent::Exit 时停对外通道（M5 A5 隧道双通道 stop_all +
    // §C1 tailscale::stop_all；kill_on_drop 兜不住进程级退出）与电源锁（caffeinate
    // kill / 执行状态清除 + 磁盘代设还原），不留孤儿进程、不失电源锁
    let app = builder
        .build(tauri::generate_context!())
        .expect("error while building tauri application");
    app.run(|_app, event| {
        if let tauri::RunEvent::Exit = event {
            exit_cleanup();
        }
    });
}

/// 退出清理四件套：停对外通道（隧道 + tailscale Funnel）、收 `tailscale login`
/// 等待者与放电源锁。RunEvent::Exit 与 Windows 应用内升级共用——插件安装路径
/// 内部直接 `process::exit(0)`，不经过事件循环，靠 commands::updater 安装链上
/// 的 `on_before_exit` 钩子调用本函数，保证不留孤儿进程、不失电源锁。
pub(crate) fn exit_cleanup() {
    crate::remote::tunnel::stop_all();
    // §C1 修复轮 1 Finding 2②：Funnel 无子进程可 kill_on_drop（守护 =
    // 轮询线程 + tailscaled 常驻配置），进程退出必须显式撤 + 收 DESIRED
    crate::remote::tailscale::stop_all();
    // I1（2026-10-08 架构评审）：`tailscale login` 的等待者**不是通道**，
    // stop_all 收不到它——它是本模块唯一长期存在的子进程（`--timeout 15s` 有界，
    // 但 15 秒内 兔维斯 退出就是一个孤儿进程）。kill + wait 收掉；并进
    // exit_cleanup 后升级安装路径（on_before_exit）同样不孤儿化它。
    crate::remote::tailscale::cancel_login_attempt();
    crate::remote::power::release();
}
