// Plugin 管理 — 文件型 symlink + 配置型写入工具配置
// 与 MCP 不同：Plugin 可能是文件/目录（用 symlink）或配置条目（写入 JSON/TOML）

use crate::linker;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Plugin 统一配置格式（配置型插件用）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginConfig {
    /// 插件名称
    pub name: String,
    /// 插件类型：file | config
    pub kind: String,
    /// 对于文件型：源路径（在全局仓库中）
    pub source_path: Option<String>,
    /// 对于配置型：配置条目（键值对）
    pub config_entries: Option<BTreeMap<String, serde_json::Value>>,
}

/// 安装插件到全局仓库
pub fn install_plugin_to_repo(source: &Path, name: &str, overwrite: bool) -> Result<(), String> {
    let repo = dirs::home_dir()
        .unwrap_or_default()
        .join(".tuvis")
        .join("plugins");
    let _ = std::fs::create_dir_all(&repo);
    let dest = repo.join(name);
    if dest.exists() {
        if !overwrite {
            return Err(format!("LINKER_NAME_TAKEN: 已存在同名资源: {}", name));
        }
        if dest.is_dir() {
            let _ = std::fs::remove_dir_all(&dest);
        } else {
            let _ = std::fs::remove_file(&dest);
        }
    }
    if source.is_dir() {
        crate::linker::copy_dir_recursive(source, &dest)
    } else {
        std::fs::copy(source, &dest).map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// 返回 Plugin 的 SSOT 仓库目录 `~/.tuvis/plugins/`，不存在则创建
fn ensure_plugin_repo_dir() -> std::path::PathBuf {
    let repo = dirs::home_dir()
        .unwrap_or_default()
        .join(".tuvis")
        .join("plugins");
    let _ = std::fs::create_dir_all(&repo);
    repo
}

/// 为工具启用文件型插件（创建 symlink）
pub fn enable_file_plugin(plugin_name: &str, tool_id: &str) -> Result<(), String> {
    let repo = ensure_plugin_repo_dir();
    let source = repo.join(plugin_name);
    if !source.exists() {
        return Err(format!(
            "PLUGIN_NOT_IN_REPO: Plugin 不在全局仓库中: {}",
            plugin_name
        ));
    }

    let adapter =
        crate::adapter::adapter_by_id(tool_id).ok_or_else(|| format!("未知工具: {}", tool_id))?;

    let plugin_dirs = adapter.plugin_dirs();
    if plugin_dirs.is_empty() {
        return Err(format!(
            "PLUGIN_FILE_UNSUPPORTED: 工具 {} 不支持文件型插件",
            tool_id
        ));
    }

    let target_dir = &plugin_dirs[0];
    let _ = std::fs::create_dir_all(target_dir);
    let target = target_dir.join(plugin_name);
    linker::create_link(&source, &target)?;

    let ext_id = format!("plugin-{}", plugin_name);
    crate::database::upsert_assignment(&ext_id, tool_id, true, "valid")?;
    log::info!("文件型 Plugin {} 已为 {} 启用", plugin_name, tool_id);
    Ok(())
}

/// 为工具禁用文件型插件（移除 symlink）
pub fn disable_file_plugin(plugin_name: &str, tool_id: &str) -> Result<(), String> {
    let adapter =
        crate::adapter::adapter_by_id(tool_id).ok_or_else(|| format!("未知工具: {}", tool_id))?;

    let plugin_dirs = adapter.plugin_dirs();
    if plugin_dirs.is_empty() {
        return Err(format!(
            "PLUGIN_FILE_UNSUPPORTED: 工具 {} 不支持文件型插件",
            tool_id
        ));
    }

    let target = plugin_dirs[0].join(plugin_name);
    linker::remove_link(&target)?;

    let ext_id = format!("plugin-{}", plugin_name);
    crate::database::upsert_assignment(&ext_id, tool_id, false, "missing")?;
    log::info!("文件型 Plugin {} 已为 {} 禁用", plugin_name, tool_id);
    Ok(())
}

/// 为工具启用配置型插件（写入工具配置文件的 plugins 段）
/// 目前仅支持 JSON 格式（Claude / OpenCode），TOML 格式（Codex）后续扩展
pub fn enable_config_plugin(
    plugin_name: &str,
    tool_id: &str,
    entries: &BTreeMap<String, serde_json::Value>,
) -> Result<(), String> {
    let adapter =
        crate::adapter::adapter_by_id(tool_id).ok_or_else(|| format!("未知工具: {}", tool_id))?;

    let config_paths = adapter.plugin_config_paths();
    if config_paths.is_empty() {
        return Err(format!(
            "PLUGIN_CONFIG_UNSUPPORTED: 工具 {} 不支持配置型插件",
            tool_id
        ));
    }

    let config_path = &config_paths[0];
    let content = std::fs::read_to_string(config_path).unwrap_or_else(|_| "{}".to_string());

    match adapter.mcp_format() {
        crate::adapter::McpFormat::Json => {
            let mut root: serde_json::Value = serde_json::from_str(&content)
                .map_err(|e| format!("MCP_PARSE_JSON: 解析 JSON 配置失败: {}", e))?;
            if root.get("plugins").is_none() {
                root["plugins"] = serde_json::json!({});
            }
            root["plugins"][plugin_name] =
                serde_json::to_value(entries).map_err(|e| e.to_string())?;
            let pretty = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
            linker::write_config_locked(config_path, &pretty)?;
        }
        crate::adapter::McpFormat::Jsonc => {
            let value = serde_json::to_value(entries)
                .map_err(|e| e.to_string())?
                .to_string();
            let next = crate::services::mcp::jsonc::upsert_entry(
                &content,
                "plugins",
                plugin_name,
                &value,
            )?;
            linker::write_config_locked(config_path, &next)?;
        }
        crate::adapter::McpFormat::Toml => {
            // TOML 格式：写入 [plugins.<name>] 段
            let content = std::fs::read_to_string(config_path).unwrap_or_default();
            let mut doc: toml_edit::DocumentMut = content
                .parse()
                .map_err(|e| format!("MCP_PARSE_TOML: 解析 TOML 失败: {}", e))?;
            if doc.get("plugins").is_none() {
                doc["plugins"] = toml_edit::Item::Table(toml_edit::Table::new());
            }
            let plugin_table = &mut doc["plugins"][plugin_name];
            /// 将 serde_json::Value 转换为 toml_edit::Value
            fn json_to_toml_value(v: &serde_json::Value) -> toml_edit::Value {
                match v {
                    serde_json::Value::Null => toml_edit::Value::from(""),
                    serde_json::Value::Bool(b) => toml_edit::Value::from(*b),
                    serde_json::Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            toml_edit::Value::from(i)
                        } else if let Some(f) = n.as_f64() {
                            toml_edit::Value::from(f)
                        } else {
                            toml_edit::Value::from(n.to_string())
                        }
                    }
                    serde_json::Value::String(s) => toml_edit::Value::from(s.clone()),
                    serde_json::Value::Array(arr) => {
                        let mut toml_arr = toml_edit::Array::new();
                        for item in arr {
                            toml_arr.push(json_to_toml_value(item));
                        }
                        toml_edit::Value::Array(toml_arr)
                    }
                    serde_json::Value::Object(map) => {
                        let mut table = toml_edit::InlineTable::new();
                        for (k, v) in map {
                            table.insert(k, json_to_toml_value(v));
                        }
                        toml_edit::Value::InlineTable(table)
                    }
                }
            }

            for (k, v) in entries {
                plugin_table[k] = toml_edit::value(json_to_toml_value(v));
            }
            let toml_str = doc.to_string();
            linker::write_config_locked(config_path, &toml_str)?;
        }
    }

    let ext_id = format!("plugin-{}", plugin_name);
    crate::database::upsert_assignment(&ext_id, tool_id, true, "valid")?;
    log::info!("配置型 Plugin {} 已为 {} 启用", plugin_name, tool_id);
    Ok(())
}

/// 为工具禁用配置型插件（从配置文件中移除 plugins 段）
pub fn disable_config_plugin(plugin_name: &str, tool_id: &str) -> Result<(), String> {
    let adapter =
        crate::adapter::adapter_by_id(tool_id).ok_or_else(|| format!("未知工具: {}", tool_id))?;

    let config_paths = adapter.plugin_config_paths();
    if config_paths.is_empty() {
        return Err(format!(
            "PLUGIN_CONFIG_UNSUPPORTED: 工具 {} 不支持配置型插件",
            tool_id
        ));
    }

    let config_path = &config_paths[0];

    match adapter.mcp_format() {
        crate::adapter::McpFormat::Json => {
            let content = std::fs::read_to_string(config_path).unwrap_or_else(|_| "{}".to_string());
            let mut root: serde_json::Value = serde_json::from_str(&content)
                .map_err(|e| format!("MCP_PARSE_JSON: 解析 JSON 配置失败: {}", e))?;
            if let Some(plugins) = root.get_mut("plugins").and_then(|p| p.as_object_mut()) {
                plugins.remove(plugin_name);
            }
            let pretty = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
            linker::write_config_locked(config_path, &pretty)?;
        }
        crate::adapter::McpFormat::Jsonc => {
            let content = std::fs::read_to_string(config_path).unwrap_or_else(|_| "{}".to_string());
            let next = crate::services::mcp::jsonc::remove_entry(&content, "plugins", plugin_name)?;
            linker::write_config_locked(config_path, &next)?;
        }
        crate::adapter::McpFormat::Toml => {
            let content = std::fs::read_to_string(config_path).unwrap_or_default();
            let mut doc: toml_edit::DocumentMut = content
                .parse()
                .map_err(|e| format!("MCP_PARSE_TOML: 解析 TOML 失败: {}", e))?;
            if let Some(plugins) = doc.get_mut("plugins").and_then(|p| p.as_table_mut()) {
                plugins.remove(plugin_name);
            }
            let toml_str = doc.to_string();
            linker::write_config_locked(config_path, &toml_str)?;
        }
    }

    let ext_id = format!("plugin-{}", plugin_name);
    crate::database::upsert_assignment(&ext_id, tool_id, false, "missing")?;
    log::info!("配置型 Plugin {} 已为 {} 禁用", plugin_name, tool_id);
    Ok(())
}

/// 统一 toggle 入口
pub fn toggle_plugin(
    plugin_name: &str,
    tool_id: &str,
    enabled: bool,
    kind: &str,
) -> Result<(), String> {
    // 常驻=停用防护（用户裁决 2026-09-18）：常驻 on 时手动停用被拒（file/config
    // 两型同护），需先关闭常驻；启用方向不受影响。守卫置于分派之前——命中即
    // 零副作用。W5 整工具停用的还原清理（disable_tool_cleanup）直接
    // restore_mam_link，不走本函数；独占清扫（sweep）在计划层已剔除常驻项
    if !enabled {
        let ext_id = format!("plugin-{}", plugin_name);
        if crate::database::is_tool_resident(tool_id, &ext_id) {
            return Err(format!(
                "插件 {} 是 {} 的常驻资源，先关闭常驻再停用",
                plugin_name, tool_id
            ));
        }
    }
    match kind {
        "file" => {
            if enabled {
                enable_file_plugin(plugin_name, tool_id)
            } else {
                disable_file_plugin(plugin_name, tool_id)
            }
        }
        "config" => {
            if enabled {
                // config 型 toggle 需要 entries，这里简化：entries 从全局仓库读取
                let repo = dirs::home_dir()
                    .unwrap_or_default()
                    .join(".tuvis")
                    .join("plugins")
                    .join(format!("{}.json", plugin_name));
                let entries: BTreeMap<String, serde_json::Value> = if repo.exists() {
                    let content = std::fs::read_to_string(&repo).map_err(|e| e.to_string())?;
                    serde_json::from_str(&content).map_err(|e| e.to_string())?
                } else {
                    BTreeMap::new()
                };
                enable_config_plugin(plugin_name, tool_id, &entries)
            } else {
                disable_config_plugin(plugin_name, tool_id)
            }
        }
        _ => Err(format!("PLUGIN_KIND_UNKNOWN: 未知 Plugin 类型: {}", kind)),
    }
}
