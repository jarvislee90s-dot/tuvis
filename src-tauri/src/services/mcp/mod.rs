// MCP 配置格式转换器 — JSON (Claude) / TOML (Codex) / JSONC (OpenCode)
//
// JSON 类的服务器段位置由 adapter 的 mcp_json_section() 声明（默认顶层 mcpServers；
// ZCode 为嵌套 mcp.servers）——读-改-写只动该子树，未知键与原键序保留
// （serde_json preserve_order），解析失败只报错不落盘（绝不写坏用户主配置）

pub mod jsonc;

use crate::adapter::{adapter_by_id, McpFormat};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// MCP 内部统一格式
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// 写入 MCP 配置到工具
pub fn write_mcp(tool_id: &str, mcp_name: &str, config: &McpConfig) -> Result<(), String> {
    let adapter = adapter_by_id(tool_id)
        .ok_or_else(|| format!("CONTENT_UNKNOWN_TOOL: 未知工具: {}", tool_id))?;
    let config_path = adapter.mcp_config_path().ok_or("工具不支持 MCP 配置")?;
    match adapter.mcp_format() {
        McpFormat::Json => {
            write_mcp_json(&config_path, adapter.mcp_json_section(), mcp_name, config)
        }
        McpFormat::Toml => write_mcp_toml(&config_path, mcp_name, config),
        McpFormat::Jsonc => {
            write_mcp_jsonc(&config_path, adapter.mcp_json_section(), mcp_name, config)
        }
    }
}

/// 移除 MCP 配置
pub fn remove_mcp(tool_id: &str, mcp_name: &str) -> Result<(), String> {
    let adapter = adapter_by_id(tool_id)
        .ok_or_else(|| format!("CONTENT_UNKNOWN_TOOL: 未知工具: {}", tool_id))?;
    let config_path = adapter.mcp_config_path().ok_or("工具不支持 MCP 配置")?;
    match adapter.mcp_format() {
        McpFormat::Json => remove_mcp_json(&config_path, adapter.mcp_json_section(), mcp_name),
        McpFormat::Toml => remove_mcp_toml(&config_path, mcp_name),
        McpFormat::Jsonc => remove_mcp_jsonc(&config_path, adapter.mcp_json_section(), mcp_name),
    }
}

/// 工具的 MCP 配置文件路径（供"打开配置文件"等入口使用）
pub fn tool_mcp_config_path(tool_id: &str) -> Result<std::path::PathBuf, String> {
    let adapter = adapter_by_id(tool_id)
        .ok_or_else(|| format!("CONTENT_UNKNOWN_TOOL: 未知工具: {}", tool_id))?;
    adapter
        .mcp_config_path()
        .ok_or_else(|| "工具不支持 MCP 配置".to_string())
}

// ===== JSON 段导航（mcp_json_section 声明的键路径，读-改-写只动该子树） =====

/// 只读导航：按键路径取服务器段对象（任一层缺失/类型不符 → None）
pub fn json_section<'a>(
    root: &'a serde_json::Value,
    section: &[&str],
) -> Option<&'a serde_json::Map<String, serde_json::Value>> {
    let mut cur = root;
    for key in section {
        cur = cur.get(key)?;
    }
    cur.as_object()
}

/// 可写导航：按键路径取服务器段对象。缺失层逐层补建为空对象；
/// 中间层被非对象值占用 → None（不得静默覆盖用户数据，调用方报错不落盘）
fn json_section_mut<'a>(
    root: &'a mut serde_json::Value,
    section: &[&str],
) -> Option<&'a mut serde_json::Map<String, serde_json::Value>> {
    let mut cur = root;
    for key in section {
        match cur.get(key) {
            None => {
                cur.as_object_mut()?
                    .insert(key.to_string(), serde_json::json!({}));
            }
            Some(v) if v.is_object() => {}
            Some(_) => return None, // 非对象值占用 → 拒绝写入
        }
        cur = cur.get_mut(key)?;
    }
    cur.as_object_mut()
}

/// 解析 JSON 配置内容（空内容按空对象；解析失败 → Err，调用方只报错不落盘）
fn parse_json_config(content: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(content).map_err(|e| format!("MCP_PARSE_JSON: 解析 JSON 配置失败: {}", e))
}

/// 段查找（删除路径用）：Ok(Some)=段存在且为对象；Ok(None)=段缺失（无可删条目）；
/// Err=任一层被非对象值占用（与写路径对称，拒绝操作不落盘）
fn json_section_present<'a>(
    root: &'a serde_json::Value,
    section: &[&str],
) -> Result<Option<&'a serde_json::Map<String, serde_json::Value>>, String> {
    let mut cur = root;
    for key in section {
        match cur.get(key) {
            None => return Ok(None),
            Some(v) if v.is_object() => cur = v,
            Some(_) => {
                return Err(format!(
                    "MCP 配置段 {} 被非对象值占用，拒绝删除操作",
                    section.join(".")
                ));
            }
        }
    }
    Ok(cur.as_object())
}

/// 条目是否被工具侧停用标记禁用（ZCode 可选 `enable: false`；无字段 = 启用）。
/// 仅布尔 false 视为停用——其他类型/缺失一律按启用（防御私有格式演进，
/// 宁可多展示不可静默隐藏用户配置）。扫描侧据此不计入「该工具已启用」列，
/// 读取侧原样透传条目（含 enable 字段）如实展示为停用
pub fn entry_disabled_by_tool(value: &serde_json::Value) -> bool {
    value.get("enable").and_then(|e| e.as_bool()) == Some(false)
}

// ===== JSON (Claude Code: ~/.claude.json mcpServers；ZCode: cli/config.json mcp.servers) =====

fn write_mcp_json(
    path: &std::path::Path,
    section: &[&'static str],
    name: &str,
    config: &McpConfig,
) -> Result<(), String> {
    let content = std::fs::read_to_string(path).unwrap_or_else(|_| "{}".to_string());
    let mut root = parse_json_config(&content)?;
    let servers = json_section_mut(&mut root, section)
        .ok_or_else(|| format!("MCP 配置段 {} 被非对象值占用，拒绝写入", section.join(".")))?;
    servers.insert(
        name.to_string(),
        serde_json::json!({
            "command": config.command,
            "args": config.args,
            "env": config.env,
        }),
    );
    let pretty = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    crate::linker::write_config_locked(path, &pretty)
}

fn remove_mcp_json(
    path: &std::path::Path,
    section: &[&'static str],
    name: &str,
) -> Result<(), String> {
    let content = std::fs::read_to_string(path).unwrap_or_else(|_| "{}".to_string());
    let mut root = parse_json_config(&content)?;
    // 与写路径对称，且不做无谓重写（防 pretty 化扰动用户主配置）：
    // 段被非对象占用 → 报错不落盘；段或条目缺失 → 幂等成功，不重写文件
    match json_section_present(&root, section)? {
        Some(servers) if servers.contains_key(name) => {}
        _ => return Ok(()),
    }
    if let Some(servers) = json_section_mut(&mut root, section) {
        servers.remove(name);
    }
    let pretty = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    crate::linker::write_config_locked(path, &pretty)
}

// ===== TOML (Codex CLI: config.toml mcp_servers) =====

fn write_mcp_toml(path: &std::path::Path, name: &str, config: &McpConfig) -> Result<(), String> {
    // 使用 toml_edit 保留原文件注释和格式
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = content
        .parse()
        .map_err(|e| format!("MCP_PARSE_TOML: 解析 TOML 失败: {}", e))?;

    // 确保 mcp_servers 段存在
    if doc.get("mcp_servers").is_none() {
        doc["mcp_servers"] = toml_edit::Item::Table(toml_edit::Table::new());
    }

    // 写入服务器配置（覆盖同名）
    {
        let server = &mut doc["mcp_servers"][name];
        server["command"] = toml_edit::value(&config.command);
        let args_array: toml_edit::Array = config
            .args
            .iter()
            .map(|a| toml_edit::Value::String(toml_edit::Formatted::new(a.clone())))
            .collect();
        server["args"] = toml_edit::Item::Value(toml_edit::Value::Array(args_array));
        if !config.env.is_empty() {
            let mut env_table = toml_edit::Table::new();
            for (k, v) in &config.env {
                env_table.insert(k, toml_edit::value(v));
            }
            server["env"] = toml_edit::Item::Table(env_table);
        }
    }

    let toml_str = doc.to_string();
    crate::linker::write_config_locked(path, &toml_str)
}

fn remove_mcp_toml(path: &std::path::Path, name: &str) -> Result<(), String> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = content
        .parse()
        .map_err(|e| format!("MCP_PARSE_TOML: 解析 TOML 失败: {}", e))?;
    if let Some(servers) = doc.get_mut("mcp_servers").and_then(|s| s.as_table_mut()) {
        servers.remove(name);
    }
    let toml_str = doc.to_string();
    crate::linker::write_config_locked(path, &toml_str)
}

// ===== JSONC (OpenCode: opencode.json mcp) =====

fn write_mcp_jsonc(
    path: &std::path::Path,
    section: &[&'static str],
    name: &str,
    config: &McpConfig,
) -> Result<(), String> {
    let content = std::fs::read_to_string(path).unwrap_or_else(|_| "{}".to_string());
    // OpenCode 格式：command 是数组，env 是 environment
    let mut cmd_array = vec![config.command.clone()];
    cmd_array.extend(config.args.iter().cloned());
    let value = serde_json::json!({
        "type": "local",
        "command": cmd_array,
        "environment": config.env,
    });
    let next = jsonc::upsert_entry(&content, single_section(section)?, name, &value.to_string())?;
    crate::linker::write_config_locked(path, &next)
}

fn remove_mcp_jsonc(
    path: &std::path::Path,
    section: &[&'static str],
    name: &str,
) -> Result<(), String> {
    let content = std::fs::read_to_string(path).unwrap_or_else(|_| "{}".to_string());
    let next = jsonc::remove_entry(&content, single_section(section)?, name)?;
    crate::linker::write_config_locked(path, &next)
}

/// JSONC 写链的节段约束（PR #46 review M5）：读链已注册表驱动（mcp_json_section
/// 可声明多级路径），但 jsonc 保注释编辑引擎仅支持顶层单段 section。写链消费声明
/// 而非硬编码 "mcp"——未来任一 JSONC 工具声明多段路径时在此**响亮失败**，而不是
/// 静默写错位置（读得回、写不进）
fn single_section(section: &[&'static str]) -> Result<&'static str, String> {
    match section {
        [single] => Ok(single),
        _ => Err(format!(
            "JSONC 写链暂不支持多级 section 路径（声明 {}），需先扩展 jsonc 编辑引擎",
            section.join(".")
        )),
    }
}

#[cfg(test)]
mod json_section_tests {
    use super::*;

    fn demo_config() -> McpConfig {
        McpConfig {
            command: "npx".into(),
            args: vec!["-y".into(), "demo-mcp".into()],
            env: BTreeMap::new(),
        }
    }

    /// ZCode 主配置形态：mcp.servers 嵌套子树与 plugins 等顶层键共存。
    /// 写入只动 mcp.servers 子树，未知键与原键序保留（preserve_order）
    #[test]
    fn write_into_nested_section_preserves_unknown_keys_and_order() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
  "zeta": 1,
  "plugins": { "p1": {} },
  "mcp": { "timeout": 30, "servers": { "existing": { "command": "old" } } }
}"#,
        )
        .unwrap();

        write_mcp_json(&path, &["mcp", "servers"], "demo", &demo_config()).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let root: serde_json::Value = serde_json::from_str(&content).unwrap();

        // 新条目落 mcp.servers 子树
        let demo = &root["mcp"]["servers"]["demo"];
        assert_eq!(demo["command"], "npx");
        assert_eq!(demo["args"][1], "demo-mcp");
        // 同子树既有条目保留
        assert_eq!(root["mcp"]["servers"]["existing"]["command"], "old");
        // 未知键（顶层与 mcp 层）保留
        assert_eq!(root["zeta"], 1);
        assert!(root["plugins"]["p1"].is_object());
        assert_eq!(root["mcp"]["timeout"], 30);
        // 原键序保留：zeta → plugins → mcp（IndexMap 序 = 读取序）
        let keys: Vec<&String> = root.as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["zeta", "plugins", "mcp"]);
    }

    /// 缺失层逐层补建：无 mcp 键的主配置写入 mcp.servers 子树
    #[test]
    fn write_creates_missing_intermediate_layers() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, r#"{ "zeta": 1 }"#).unwrap();

        write_mcp_json(&path, &["mcp", "servers"], "demo", &demo_config()).unwrap();
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["mcp"]["servers"]["demo"]["command"], "npx");
        assert_eq!(root["zeta"], 1);
    }

    /// 解析失败只报错不落盘（绝不写坏用户主配置）
    #[test]
    fn parse_failure_errors_without_touching_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        let broken = r#"{ "mcp": { "servers": { oops"#;
        std::fs::write(&path, broken).unwrap();

        let err = write_mcp_json(&path, &["mcp", "servers"], "demo", &demo_config());
        assert!(err.is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            broken,
            "解析失败不得落盘"
        );

        let err = remove_mcp_json(&path, &["mcp", "servers"], "demo");
        assert!(err.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
    }

    /// 中间层被非对象值占用 → 拒绝写入（不静默覆盖用户数据）
    #[test]
    fn non_object_intermediate_layer_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, r#"{ "mcp": "corrupted" }"#).unwrap();

        let err = write_mcp_json(&path, &["mcp", "servers"], "demo", &demo_config()).unwrap_err();
        assert!(err.contains("mcp.servers"));
        // 原文件未被改写
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["mcp"], "corrupted");
    }

    /// 移除只删目标条目：同子树其他条目与未知键保留
    #[test]
    fn remove_only_target_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{ "zeta": 1, "mcp": { "servers": { "demo": { "command": "npx" }, "keep": { "command": "k" } } } }"#,
        )
        .unwrap();

        remove_mcp_json(&path, &["mcp", "servers"], "demo").unwrap();
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(root["mcp"]["servers"].get("demo").is_none());
        assert_eq!(root["mcp"]["servers"]["keep"]["command"], "k");
        assert_eq!(root["zeta"], 1);
    }

    /// 删除路径与写入对称：段路径被非对象值占用 → 报错且文件逐字节不变；
    /// 段/条目不存在 → 幂等成功且不重写文件（不做无谓 pretty 化扰动）
    #[test]
    fn remove_rejects_corrupt_section_and_skips_noop_rewrite() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        // 损坏形态：mcp 或 mcp.servers 被非对象占用 → 报错、文件逐字节不变
        for corrupt in [r#"{"mcp":"not-an-object"}"#, r#"{"mcp":{"servers":[1,2]}}"#] {
            std::fs::write(&path, corrupt).unwrap();
            let before = std::fs::read_to_string(&path).unwrap();
            assert!(
                remove_mcp_json(&path, &["mcp", "servers"], "s1").is_err(),
                "损坏形态 {corrupt} 上删除必须报错"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        }
        // 无该子树 / 段存在但无该条目 → 幂等成功，文件保持原样（未重写）
        for idempotent in [
            r#"{"plugins":{}}"#,
            r#"{"mcp":{"servers":{"other":{"command":"x"}}}}"#,
        ] {
            std::fs::write(&path, idempotent).unwrap();
            let before = std::fs::read_to_string(&path).unwrap();
            remove_mcp_json(&path, &["mcp", "servers"], "s1").unwrap();
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                before,
                "无可删条目时不得重写文件"
            );
        }
    }

    /// 只读导航：任意层缺失/类型不符 → None（读取端降级为空段，不报错）
    #[test]
    fn readonly_navigation() {
        let root: serde_json::Value =
            serde_json::from_str(r#"{ "mcp": { "servers": { "a": {} } } }"#).unwrap();
        assert!(json_section(&root, &["mcp", "servers"]).is_some());
        assert!(json_section(&root, &["mcpServers"]).is_none());
        assert!(json_section(&root, &["mcp", "servers", "a", "deeper"]).is_none());
        let empty: serde_json::Value = serde_json::json!({});
        assert!(json_section(&empty, &["mcp", "servers"]).is_none());
    }

    /// 默认段（顶层 mcpServers，既有 Json 工具）新旧行为一致：
    /// 写入/移除往返与旧硬编码 root["mcpServers"] 等价
    #[test]
    fn default_section_roundtrip_matches_legacy_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        std::fs::write(&path, r#"{ "mcpServers": { "old": { "command": "o" } } }"#).unwrap();

        write_mcp_json(&path, &["mcpServers"], "demo", &demo_config()).unwrap();
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["mcpServers"]["demo"]["command"], "npx");
        assert_eq!(root["mcpServers"]["old"]["command"], "o");

        remove_mcp_json(&path, &["mcpServers"], "demo").unwrap();
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(root["mcpServers"].get("demo").is_none());
        assert_eq!(root["mcpServers"]["old"]["command"], "o");
    }

    /// 注册表分发：zcode → mcp.servers 段声明 + cli/config.json 路径
    #[test]
    fn zcode_adapter_declares_nested_section() {
        let adapter = crate::adapter::adapter_by_id("zcode").unwrap();
        assert_eq!(adapter.mcp_format(), McpFormat::Json);
        assert_eq!(adapter.mcp_json_section(), &["mcp", "servers"]);
        let path = adapter.mcp_config_path().unwrap();
        assert!(path.ends_with(".zcode/cli/config.json"));
    }
}

#[cfg(test)]
mod entry_disabled_tests {
    use super::entry_disabled_by_tool;

    /// ZCode enable:false 停用标记如实识别；无字段 = 启用；防御非布尔类型
    #[test]
    fn enable_flag_recognized_defensively() {
        let v = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        // 显式停用
        assert!(entry_disabled_by_tool(&v(
            r#"{"command":"npx","enable":false}"#
        )));
        // 无字段 = 启用（ZCode 语义）
        assert!(!entry_disabled_by_tool(&v(r#"{"command":"npx"}"#)));
        // 显式启用
        assert!(!entry_disabled_by_tool(&v(
            r#"{"command":"npx","enable":true}"#
        )));
        // 非布尔类型（格式漂移防御）→ 按启用，不静默隐藏用户配置
        assert!(!entry_disabled_by_tool(&v(
            r#"{"command":"npx","enable":"false"}"#
        )));
        assert!(!entry_disabled_by_tool(&v(
            r#"{"command":"npx","enable":0}"#
        )));
    }
}
