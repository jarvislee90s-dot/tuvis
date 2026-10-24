// Services 统一入口 - 业务逻辑按功能域拆分到子目录

pub mod codex_control;
pub mod codex_desktop_ipc;
pub mod manifest;
pub mod mcp;
pub mod pet;
pub mod plugin;
pub mod preset;
pub mod resource;
pub mod skill;
pub mod tool_settings;
pub mod usage;
// ZCode 统一远控的配置合同（A4）：官方链接格式校验 + settings 持久化
pub mod zcode_control;

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

/// 子 Agent 检测结果（#66）：区分「工具不支持子 Agent」与「支持但没有」。
/// 空数组无法区分这两种成因（kimi swarm 会话内 / 未覆盖工具均返回空），
/// 前端需要不同文案（不支持 vs 空态）——判定口径表见 PresetList 消费侧。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentDetection {
    /// adapter.subagent_dir().is_some()（未知工具 id 也为 false）
    pub supported: bool,
    /// 既有口径不变：目录存在则全收 file_stem
    pub agents: Vec<String>,
}

/// 检测工具的子 Agent 列表
pub fn detect_subagents(tool_id: &str) -> SubagentDetection {
    let Some(adapter) = crate::adapter::adapter_by_id(tool_id) else {
        return SubagentDetection {
            supported: false,
            agents: Vec::new(),
        };
    };
    if let Some(dir) = adapter.subagent_dir() {
        // 目录不存在 = 支持但从未 spawn 过子 Agent → supported=true + 空列表；
        // read_dir 失败（目录在但读不了）与「无」同呈现，如实不报错
        if dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                return SubagentDetection {
                    supported: true,
                    agents: entries
                        .flatten()
                        .filter_map(|e| {
                            e.path()
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .map(String::from)
                        })
                        .collect(),
                };
            }
        }
        return SubagentDetection {
            supported: true,
            agents: Vec::new(),
        };
    }
    SubagentDetection {
        supported: false,
        agents: Vec::new(),
    }
}

#[cfg(test)]
mod subagent_detection_tests {
    use super::*;

    /// #66 判定口径定案表逐行断言（spec §1.1）：
    /// | 场景 | supported | agents |
    /// | 未知工具 id | false | [] |
    /// | subagent_dir()=None（kimi 代表） | false | [] |
    /// | 目录不存在（支持但从未 spawn） | true | [] |
    /// | 目录存在 | true | [...] |
    /// 前两行零文件系统依赖直接断言；后两行经 HOME 重定向控制 `~/.claude/agents`
    /// （claude 的 subagent_dir 硬编码 home，无 path 缝——见 monitor::test_env 注释）。
    #[test]
    fn detect_subagents_unknown_tool_is_unsupported() {
        let d = detect_subagents("no-such-tool-id");
        assert!(!d.supported, "未知工具 id → 不支持");
        assert!(d.agents.is_empty());
    }

    #[test]
    fn detect_subagents_none_dir_tool_is_unsupported() {
        // kimi：子 agent 为会话内 swarm，subagent_dir()=None → 不支持（非空态）
        let d = detect_subagents("kimi");
        assert!(!d.supported, "subagent_dir()=None → 不支持");
        assert!(d.agents.is_empty());
    }

    #[test]
    fn detect_subagents_missing_dir_is_supported_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let guard = crate::monitor::test_env::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let d = crate::monitor::test_env::with_home(tmp.path(), || {
            detect_subagents("claude") // tempdir 下无 ~/.claude/agents
        });
        drop(guard);
        assert!(
            d.supported,
            "目录不存在 = 支持但从未 spawn → supported=true"
        );
        assert!(d.agents.is_empty(), "空列表（可区分「不支持」的新契约点）");
    }

    #[test]
    fn detect_subagents_existing_dir_lists_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join(".claude").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(agents_dir.join("frontend-dev.md"), "").unwrap();
        std::fs::write(agents_dir.join("backend-dev.md"), "").unwrap();
        let guard = crate::monitor::test_env::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let d = crate::monitor::test_env::with_home(tmp.path(), || detect_subagents("claude"));
        drop(guard);
        assert!(d.supported, "目录存在 → 支持");
        let mut names = d.agents.clone();
        names.sort(); // read_dir 顺序不保证，排序后比较
        assert_eq!(names, ["backend-dev", "frontend-dev"], "全收 file_stem");
    }
}
