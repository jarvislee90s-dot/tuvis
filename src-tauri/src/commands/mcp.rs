// MCP 管理命令

#[tauri::command]
pub fn toggle_mcp_for_tool(mcp_name: String, tool_id: String, enabled: bool) -> Result<(), String> {
    // W5：未勾选工具的 toggle 操作直接拒绝（数据保留在 DB）
    crate::services::tool_settings::ensure_tool_enabled(&tool_id)?;
    crate::services::toggle_mcp(&mcp_name, &tool_id, enabled)
}

#[tauri::command]
pub fn read_mcp_servers(tool_id: String) -> Result<serde_json::Value, String> {
    // 债修复（ZCode 接入轮）：原实现硬编码 claude/codex/opencode 三工具清单，
    // openclaw/kimi/workbuddy 的 MCP 读取全部落「未知工具」错误分支——改走
    // adapter 注册表统一分发（新工具登记后自动纳入，不再逐个补 arm）
    let adapter = crate::adapter::adapter_by_id(&tool_id)
        .ok_or_else(|| format!("CONTENT_UNKNOWN_TOOL: 未知工具: {}", tool_id))?;
    let path = adapter.mcp_config_path().ok_or("工具不支持 MCP")?;
    let content = std::fs::read_to_string(&path).unwrap_or_else(|_| "{}".to_string());
    let raw: serde_json::Value = match adapter.mcp_format() {
        crate::adapter::McpFormat::Json | crate::adapter::McpFormat::Jsonc => {
            serde_json::from_str(&content).map_err(|e| e.to_string())?
        }
        crate::adapter::McpFormat::Toml => {
            let toml_val: toml::Value = content
                .parse()
                .map_err(|e: toml::de::Error| e.to_string())?;
            let json_str = serde_json::to_string(&toml_val).map_err(|e| e.to_string())?;
            serde_json::from_str(&json_str).map_err(|e| e.to_string())?
        }
    };
    // 段定位：优先 adapter 声明的键路径（ZCode=mcp.servers 嵌套子树），
    // 兼容既有工具的历史形态（mcpServers/mcp_servers/mcp/servers 顶层键）
    let servers = crate::services::mcp::json_section(&raw, adapter.mcp_json_section())
        .map(|m| serde_json::Value::Object(m.clone()))
        .or_else(|| {
            raw.get("mcpServers")
                .or_else(|| raw.get("mcp_servers"))
                .or_else(|| raw.get("mcp"))
                .or_else(|| raw.get("servers"))
                .cloned()
        })
        .unwrap_or_else(|| serde_json::json!({}));
    // 条目原样透传：ZCode 可选 enable:false 停用标记随值下发，前端如实展示为停用
    Ok(serde_json::json!({ "servers": servers }))
}

#[tauri::command]
pub fn write_mcp_server(
    tool_id: String,
    mcp_name: String,
    command: String,
    args: Vec<String>,
    env: std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
    // issue #36-1：与 toggle_mcp_for_tool 同守卫（纵深防御，当前 UI 未直达）
    crate::services::tool_settings::ensure_tool_enabled(&tool_id)?;
    let config = crate::services::mcp::McpConfig { command, args, env };
    crate::services::mcp::write_mcp(&tool_id, &mcp_name, &config)
}

#[tauri::command]
pub fn remove_mcp_server(tool_id: String, mcp_name: String) -> Result<(), String> {
    // issue #36-1：与 toggle_mcp_for_tool 同守卫（纵深防御，当前 UI 未直达）
    crate::services::tool_settings::ensure_tool_enabled(&tool_id)?;
    crate::services::mcp::remove_mcp(&tool_id, &mcp_name)
}

#[cfg(test)]
mod read_mcp_servers_registry_tests {
    use super::read_mcp_servers;

    /// 债修复回归锁：读取链路由 adapter 注册表分发——未注册工具报「未知工具」，
    /// 注册工具不再落该分支（七工具全部可达；不触真实文件系统：本用例只走
    /// 未注册分支，注册工具的路径解析由 mcp::json_section_tests 的 fixture 覆盖）
    #[test]
    fn unknown_tool_rejected_via_registry() {
        let err = read_mcp_servers("ghost-tool".to_string()).unwrap_err();
        assert_eq!(err, "CONTENT_UNKNOWN_TOOL: 未知工具: ghost-tool");
    }

    /// 注册表可达性：TOOL_IDS 全部工具（含 zcode）都能经 adapter_by_id 解析——
    /// read_mcp_servers 的分发门即此注册表（原硬编码 match 落「未知工具」的
    /// openclaw/kimi/workbuddy/zcode 现全部可达）。零文件系统访问：不对注册工具
    /// 调用 read_mcp_servers 本体（那会读真实用户配置，违反测试数据安全约束），
    /// 路径解析与段导航由 mcp::json_section_tests 的 tempdir fixture 覆盖
    #[test]
    fn all_registered_tools_resolve_via_registry() {
        for tool in crate::adapter::TOOL_IDS {
            assert!(
                crate::adapter::adapter_by_id(tool).is_some(),
                "注册工具 {tool} 必须可经 adapter_by_id 解析"
            );
        }
        // zcode 显式断言（原硬编码清单缺它时此处即债的复现点）
        let zcode = crate::adapter::adapter_by_id("zcode").unwrap();
        assert_eq!(zcode.mcp_format(), crate::adapter::McpFormat::Json);
        assert!(zcode.mcp_config_path().is_some());
    }
}
