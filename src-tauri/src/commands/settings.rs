// 设置、工具检测、子 Agent 命令

use crate::database::SubAgentRecord;
use tauri::Emitter;

#[tauri::command]
pub fn get_setting(key: String) -> Option<String> {
    crate::database::get_setting(&key)
}

#[tauri::command]
pub fn set_setting(key: String, value: String) {
    crate::database::set_setting(&key, &value);
}

/// 主题 SSOT 写入（issue #3）：主题事实源是 DB settings KV（`ui_theme`），
/// 主窗口/设置窗口等所有窗口共享同一份。写库后广播全局事件 `tuvis-theme-changed`，
/// 各窗口（独立 WebView，storage 事件不互通）凭此实时跟随；payload 为
/// `{ theme: "dark"|"light"|"system" }`，与前端 theme-provider 的约定一致。
#[tauri::command]
pub fn set_theme(app: tauri::AppHandle, theme: String) {
    crate::database::set_setting("ui_theme", &theme);
    let _ = app.emit("tuvis-theme-changed", serde_json::json!({ "theme": theme }));
}

#[tauri::command]
pub fn detect_tools() -> Vec<crate::linker::detector::ToolDetection> {
    crate::linker::detector::detect_all_tools()
}

#[tauri::command]
pub fn detect_subagents(tool_id: String) -> Vec<String> {
    crate::services::detect_subagents(&tool_id)
}

#[tauri::command]
pub fn list_sub_agents(tool_id: String) -> Vec<SubAgentRecord> {
    crate::database::list_sub_agents(&tool_id)
}

/// 手动关闭未读卡（X 按钮）→ 标记已读（删除未读行）。
/// T1：删行后广播 session-read——看板/宠物是独立 WebView，宠物窗口凭此事件
/// 同步已读置位（否则宠物头顶卡在别处已读后仍滞留，与看板不一致）
#[tauri::command]
pub fn mark_session_read(app: tauri::AppHandle, agent_type: String, session_id: String) {
    crate::database::dao::unread::delete(&agent_type.to_lowercase(), &session_id);
    // issue #35-1：已读墓碑——缓存失忆（长间隙清缓存 / 兔维斯 重启）后
    // Insert 边沿与补偿据此不再复活已读未读卡
    crate::database::dao::unread::mark_read(&agent_type.to_lowercase(), &session_id);
    let _ = app.emit(
        "session-read",
        serde_json::json!({ "agentType": agent_type, "sessionId": session_id }),
    );
}

/// 工具勾选列表（含 managed 标志：是否存在启用的分配）
#[tauri::command]
pub fn get_tool_settings() -> Vec<crate::services::tool_settings::ToolSetting> {
    crate::services::tool_settings::get_tool_settings()
}

/// 批量保存重入互斥（review-2 Important 1）：异步化后两次调用可真并行，而
/// 还原/回滚路径的 create_link 是「先删后建」（linker/mod.rs），并发方会在
/// 竞态窗口内删掉对方刚还原的真实目录——同一时刻只允许一次批量保存
static APPLY_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 批量保存工具勾选（取消勾选清理 / 重新勾选重建，返回明细）
#[tauri::command]
pub async fn update_tool_settings(
    app: tauri::AppHandle,
    changes: Vec<crate::services::tool_settings::ToolSettingChange>,
) -> Result<crate::services::tool_settings::ApplyResult, String> {
    // issue #36-2：非 async 命令在主线程执行，apply 内含 copy_dir_recursive 等大目录
    // 文件操作，保存期间会冻结全部窗口的 IPC——移到运行时阻塞线程池执行（Tauri 官方
    // 对重活的建议），主线程与 async worker 均不受阻。
    // 守卫在阻塞线程内获取（std MutexGuard 不可跨 await）
    let result = tauri::async_runtime::spawn_blocking(move || {
        // review-3：区分两种 try_lock 失败——WouldBlock 是真重入（拒绝）；
        // Poisoned 是前次持有者 panic 后锁已空闲，而守卫数据是 ()（无可损坏状态），
        // 中毒保护无意义，into_inner 恢复放行重试，否则一次 panic 会让后续保存
        // 永久误报「进行中」直到重启
        let _guard = match APPLY_GUARD.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                return Err(
                    "W5_APPLY_IN_PROGRESS: tool settings save already in progress".to_string(),
                )
            }
        };
        Ok::<crate::services::tool_settings::ApplyResult, String>(
            crate::services::tool_settings::apply_tool_changes(changes),
        )
    })
    .await
    .map_err(|e| {
        // JoinError（后台任务 panic）不得伪装成「已应用」：走 Err 让前端报错
        //（review-2 Important 2：空 ApplyResult + 无条件 success toast = 伪成功）。
        // 结构化错误码由前端 i18n 渲染（同 issue #36-3 模式，review-3）
        log::error!("update_tool_settings background task failed: {e}");
        format!("W5_APPLY_TASK_FAILED: {e}")
    })??;
    // N2：跨窗口广播工具勾选变化。设置窗口与主窗口是独立 WebView、各持 QueryClient，
    // 设置页本地的 invalidateQueries 触达不到主窗口——主窗口靠此事件失效缓存
    let _ = app.emit("tools-changed", ());
    Ok(result)
}

/// 前端工具列下发项（资源视图 / 设置声音区的唯一样式来源）
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnabledTool {
    pub id: String,
    pub label: String,
    /// 资源能力标志（前端据其渲染「暂不支持」，不按工具 id 硬编码）。
    /// skill 启停：dsh 已放开（用户 2026-09-14 验收裁决要求 UI 增减链接管理；
    /// 依据：①后台补链已事实性写 ~/.dsh/skills 且链接有效 ②dsh skill-filesystem
    /// 源码有专用符号链接处理分支 + followSymlinks 配置，跟随符号链接是受支持形态；
    /// 随注册表演化，有 skill_dir 分支的工具即为支持）
    pub skill_toggle_supported: bool,
    pub mcp_supported: bool,
    pub plugin_supported: bool,
}

/// 前端工具列的唯一下发源（W5：勾选状态驱动，替代三处硬编码 TOOLS）
#[tauri::command]
pub fn list_enabled_tools() -> Vec<EnabledTool> {
    crate::adapter::TOOL_IDS
        .iter()
        .filter(|id| crate::database::dao::agent_tool::get_tool_enabled(id))
        .filter_map(|id| {
            crate::adapter::adapter_by_id(id).map(|a| EnabledTool {
                id: id.to_string(),
                label: a.name().to_string(),
                skill_toggle_supported: crate::adapter::primary_skill_dir(id).is_some(),
                mcp_supported: a.mcp_config_path().is_some(),
                plugin_supported: !a.plugin_dirs().is_empty(),
            })
        })
        .collect()
}

/// T5 信号健康度：per-tool hook 通道状态（设置页「信号健康度」分区按需查询；
/// 无轮询——仅分区打开/手动刷新时调用）。数据链路全复用既有设施：注册状态读
/// KV、事件读 30s TTL 事件目录、会话归属走 adapter::get_all_sessions（自带单飞
/// 护栏，与看板轮询共用快照/复扫同一份）——零新增扫描预算
#[tauri::command]
pub async fn hook_signal_health() -> Vec<crate::monitor::hooks::ToolSignalHealth> {
    // 会话扫描是重活（sysinfo 全进程刷新 + 文件解析），同步命令在主线程执行会
    // 冻结全部窗口 IPC——移到运行时阻塞线程池执行（同 get_all_sessions 命令惯例）
    tauri::async_runtime::spawn_blocking(|| {
        let tools: Vec<(String, String)> = crate::adapter::all_adapters()
            .into_iter()
            .filter(|a| a.hook_supported())
            .map(|a| (a.agent_type().tool_id().to_string(), a.name().to_string()))
            .collect();
        let response = crate::adapter::get_all_sessions();
        let registered_of = |tool_id: &str| {
            crate::database::get_setting(&format!("hooks_registered_{tool_id}")).as_deref()
                == Some("true")
        };
        let tools_ref: Vec<(&str, &str)> = tools
            .iter()
            .map(|(id, label)| (id.as_str(), label.as_str()))
            .collect();
        let mut health = crate::monitor::hooks::compute_tool_signal_health(
            &tools_ref,
            &registered_of,
            &crate::monitor::hooks::read_hook_events(),
            &response.sessions,
        );
        // 1A 未注册原因在命令层后处理（而非塞进 compute 纯函数），两个理由：
        // ① compute_tool_signal_health 的签名/注入口径不动，既有 signal_health_tests
        // 零改动（测试面最小）；② reason 的输入 helper 探测依赖 current_exe/home，
        // 属环境敏感信息，不宜混进 tempdir/内存可测的纯函数——探测单独跑一次，
        // 结论经 unregistered_reason_for 纯函数（hooks.rs 侧可测）落到行上
        let helper_present = crate::monitor::hooks::probe_hook_listener_helper();
        for h in &mut health {
            h.unregistered_reason = crate::monitor::hooks::unregistered_reason_for(
                &h.tool_id,
                h.registered,
                helper_present,
            );
        }
        health
    })
    .await
    .unwrap_or_else(|e| {
        // JoinError（后台任务 panic）→ 空表（前端渲染空态，不伪装成功数据）
        ::log::error!("hook_signal_health 扫描任务异常: {e}");
        Vec::new()
    })
}
