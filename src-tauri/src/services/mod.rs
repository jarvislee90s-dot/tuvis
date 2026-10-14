// Services 统一入口 - 业务逻辑按功能域拆分到子目录

pub mod manifest;
pub mod mcp;
pub mod pet;
pub mod plugin;
pub mod preset;
pub mod resource;
pub mod skill;
pub mod tool_settings;
pub mod usage;

use log::info;

// 重新导出 skill 函数（保持 crate::services::install_skill 向后兼容）
pub use skill::{
    assign_skill_to_subagent, disable_skill_for_tool, enable_skill_for_tool, install_skill,
    is_skill_in_tool_range,
};
// 重新导出 resource 函数
pub use resource::sync_imported_skill_links;
pub use resource::{auto_import_extensions, ImportStats};

/// 为工具启用/禁用 Plugin（委托到 plugin 子模块）
pub fn toggle_plugin(
    plugin_name: &str,
    tool_id: &str,
    enabled: bool,
    kind: &str,
) -> Result<(), String> {
    plugin::toggle_plugin(plugin_name, tool_id, enabled, kind)
}

/// 为工具启用/禁用 MCP（委托到 mcp 子模块）
pub fn toggle_mcp(mcp_name: &str, tool_id: &str, enabled: bool) -> Result<(), String> {
    let ext_id = format!("mcp-{}", mcp_name);
    if !enabled
        // 常驻=停用防护（用户裁决 2026-09-18）：常驻 on 时手动停用被拒，需先关闭
        // 常驻；启用方向不受影响。守卫置于 remove_mcp 之前——命中即零副作用。
        // W5 整工具停用的还原清理（disable_tool_cleanup）直接调 mcp::remove_mcp，
        // 不走本函数，不受此守卫约束；独占清扫（sweep）在计划层已剔除常驻项
        && crate::database::is_tool_resident(tool_id, &ext_id)
    {
        return Err(format!(
            "MCP_RESIDENT_CONFLICT: MCP {} 是 {} 的常驻资源，先关闭常驻再停用",
            mcp_name, tool_id
        ));
    }
    if enabled {
        let repo = dirs::home_dir()
            .unwrap_or_default()
            .join(".tuvis")
            .join("mcp");
        let config_path = repo.join(format!("{}.json", mcp_name));
        if !config_path.exists() {
            return Err(format!(
                "MCP_NOT_IN_REPO: MCP 配置不在全局仓库中: {}",
                mcp_name
            ));
        }
        let content = std::fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
        let config: mcp::McpConfig = serde_json::from_str(&content).map_err(|e| e.to_string())?;
        mcp::write_mcp(tool_id, mcp_name, &config)?;
    } else {
        mcp::remove_mcp(tool_id, mcp_name)?;
    }
    crate::database::upsert_assignment(
        &ext_id,
        tool_id,
        enabled,
        if enabled { "valid" } else { "missing" },
    )?;
    info!(
        "MCP {} 已为 {} {}",
        mcp_name,
        tool_id,
        if enabled { "启用" } else { "禁用" }
    );
    Ok(())
}

/// 检测工具的子 Agent 列表
pub fn detect_subagents(tool_id: &str) -> Vec<String> {
    let Some(adapter) = crate::adapter::adapter_by_id(tool_id) else {
        return Vec::new();
    };
    if let Some(dir) = adapter.subagent_dir() {
        if dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                return entries
                    .flatten()
                    .filter_map(|e| {
                        e.path()
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .map(String::from)
                    })
                    .collect();
            }
        }
    }
    Vec::new()
}
