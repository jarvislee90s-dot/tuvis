# MultiAgents Manager 剩余功能实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 补齐 Spec 中已定义但尚未实现的剩余功能：Plugin 管理、Layer 2/3 目录结构落地、子 Agent 级预设组、MCP 配置前端 UI、可配置提示音。

**架构：** 在现有 Tauri 2 + Rust + React 19 架构上增量补齐。Plugin 采用与 Skill 相同的三层映射模式；Layer 2/3 通过 `~/.mam/active/` 中间目录统一 Skill 映射入口；子 Agent 级预设扩展现有 `apply_preset` 接口；前端新增 MCP 管理面板和提示音配置。

**技术栈：** Rust 2021 + Tauri 2 + React 19 + TypeScript 5.8 + Tailwind CSS v4 + shadcn/ui + Zustand + rusqlite + toml_edit

---

## 自检前置：当前完成度快照

基于 `specs/001-multi-agent-platform/spec.md` 和实际代码扫描，以下功能**已完成**：
- US1 会话监控看板（SessionCard, SessionGrid, 轮询）
- US2 通知与提醒（Web Audio + tauri-plugin-notification + 去重）
- US3 终端跳转（iTerm2, Terminal.app, tmux）
- FR-1 会话发现与状态检测（Claude/Codex/OpenCode 三工具 + Hook）
- FR-2 统一看板 + 托盘聚合状态
- FR-3 通知系统（声音 + 桌面通知）
- FR-4 快速跳转
- FR-5 Skill 统一仓库 + MCP 配置格式转换（JSON/TOML/JSONC）
- FR-6 预设组创建/应用/取消/删除（Skill + MCP）
- FR-6 托盘菜单显示预设组
- FR-7 子 Agent 检测 + 分配约束
- FR-8 安全路径检查
- 数据库层（SQLite 9 张表）
- IPC 命令体系
- i18n / 主题 / 热键 / 自动更新 / 无边框窗口

以下功能**缺失或部分缺失**，本计划逐一补齐：

| 缺失项 | Spec 引用 | 优先级 |
|--------|----------|--------|
| Plugin 管理（文件型 + 配置型） | FR-5 #20 | P1 |
| Layer 2/3 目录结构落地 | FR-5 #17-18, Plan 三层架构 | P1 |
| 子 Agent 级预设组应用 | FR-7 #31 | P2 |
| MCP 配置前端 UI（添加/编辑/删除） | FR-5 #19 | P2 |
| 用户可配置提示音 | FR-3 #10 | P3 |
| 插件纳入预设组 | FR-6 #24 | P1 |
| 通知可点击跳转验证 | FR-3 #12 | P2 |

---

## 文件结构（新增/修改清单）

### 后端（Rust）

| 文件 | 职责 | 操作 |
|------|------|------|
| `src-tauri/src/manager/plugin.rs` | Plugin 管理：文件型 symlink + 配置型写入工具配置 | **创建** |
| `src-tauri/src/manager/mod.rs` | 统一入口：扩展 `toggle_plugin`，接入 `auto_import_plugins` | **修改** |
| `src-tauri/src/linker/mod.rs` | 增加 Layer 2/3 目录管理：`ensure_active_dir()`, `ensure_subagent_dir()` | **修改** |
| `src-tauri/src/linker/layer2.rs` | Layer 2 工具级激活目录管理（Skill symlink 到 `~/.mam/active/<tool>/`） | **创建** |
| `src-tauri/src/linker/layer3.rs` | Layer 3 子 Agent 级激活目录管理 | **创建** |
| `src-tauri/src/manager/preset.rs` | 扩展 `apply_preset` / `deactivate_preset` 支持 `sub_agent_id` 和 `plugin` kind | **修改** |
| `src-tauri/src/commands.rs` | 新增 `toggle_plugin`, `add_mcp_server`, `edit_mcp_server`, `delete_mcp_server`, `apply_preset_to_subagent`, `deactivate_preset_from_subagent` | **修改** |
| `src-tauri/src/lib.rs` | 注册新增 IPC 命令 | **修改** |
| `src-tauri/src/store.rs` | 扩展 `preset_applications` 表支持 `sub_agent_id`；新增 `plugin` kind 支持 | **修改** |
| `src-tauri/src/adapter/mod.rs` | 扩展 `AgentAdapter` trait 增加 `plugin_dirs()` 和 `plugin_config_paths()` | **修改** |
| `src-tauri/src/adapter/claude.rs` | 实现 `plugin_dirs()` / `plugin_config_paths()` | **修改** |
| `src-tauri/src/adapter/codex.rs` | 实现 `plugin_dirs()` / `plugin_config_paths()` | **修改** |
| `src-tauri/src/adapter/opencode.rs` | 实现 `plugin_dirs()` / `plugin_config_paths()` | **修改** |
| `src-tauri/src/plugins/system_tray.rs` | 托盘菜单预设点击支持子 Agent 级（如需要） | **可能修改** |

### 前端（TypeScript/React）

| 文件 | 职责 | 操作 |
|------|------|------|
| `src/components/McpManager.tsx` | MCP 服务器添加/编辑/删除面板 | **创建** |
| `src/components/ExtensionList.tsx` | 扩展 Plugin 显示 + MCP 管理入口 + 子 Agent 预设操作 | **修改** |
| `src/components/PresetList.tsx` | 扩展预设组创建：可选 Plugin；扩展应用：支持子 Agent | **修改** |
| `src/pages/settings.tsx` | 新增提示音频率配置 | **修改** |
| `src/lib/audio.ts` | 扩展为可配置频率的提示音 | **修改** |
| `src/types/extension.ts` | 扩展类型定义（如有需要） | **可能修改** |
| `src/types/preset.ts` | 扩展 `PresetApplyResult` / `PresetItem`（如有需要） | **可能修改** |

---

## 任务分解

### 任务 1：扩展 AgentAdapter trait — 增加 Plugin 支持接口

**文件：**
- 修改：`src-tauri/src/adapter/mod.rs`
- 修改：`src-tauri/src/adapter/claude.rs`
- 修改：`src-tauri/src/adapter/codex.rs`
- 修改：`src-tauri/src/adapter/opencode.rs`

**背景：** Spec FR-5 #20 要求插件管理。Plugin 分两种：文件型（用 symlink 映射到工具插件目录）和配置型（写入工具配置文件）。每个工具需要声明自己的插件目录和配置文件路径。

- [ ] **步骤 1：修改 trait 定义**

在 `src-tauri/src/adapter/mod.rs` 的 `AgentAdapter` trait 中增加两个默认方法：

```rust
fn plugin_dirs(&self) -> Vec<std::path::PathBuf> { Vec::new() }
fn plugin_config_paths(&self) -> Vec<std::path::PathBuf> { Vec::new() }
```

添加到 `AgentAdapter` trait 中 `subagent_dir()` 之后：

```rust
fn subagent_dir(&self) -> Option<std::path::PathBuf> { None }

fn plugin_dirs(&self) -> Vec<std::path::PathBuf> { Vec::new() }
fn plugin_config_paths(&self) -> Vec<std::path::PathBuf> { Vec::new() }
```

- [ ] **步骤 2：为 Claude 实现 plugin 路径**

在 `src-tauri/src/adapter/claude.rs` 的 `impl AgentAdapter for ClaudeAdapter` 末尾添加：

```rust
fn plugin_dirs(&self) -> Vec<std::path::PathBuf> {
    vec![self.base_dir().join("plugins")]
}
fn plugin_config_paths(&self) -> Vec<std::path::PathBuf> {
    vec![self.base_dir().join("settings.json")]
}
```

- [ ] **步骤 3：为 Codex 实现 plugin 路径**

在 `src-tauri/src/adapter/codex.rs` 末尾添加：

```rust
fn plugin_dirs(&self) -> Vec<std::path::PathBuf> {
    vec![self.base_dir().join("plugins")]
}
fn plugin_config_paths(&self) -> Vec<std::path::PathBuf> {
    vec![self.base_dir().join("config.toml")]
}
```

- [ ] **步骤 4：为 OpenCode 实现 plugin 路径**

在 `src-tauri/src/adapter/opencode.rs` 末尾添加：

```rust
fn plugin_dirs(&self) -> Vec<std::path::PathBuf> {
    vec![self.base_dir().join("plugins")]
}
fn plugin_config_paths(&self) -> Vec<std::path::PathBuf> {
    vec![self.base_dir().join("opencode.json")]
}
```

- [ ] **步骤 5：编译验证**

运行：`cd /Users/jarvis/Documents/MultiAgents-Manager && cargo check`
预期：PASS（无编译错误）

- [ ] **步骤 6：Commit**

```bash
git add src-tauri/src/adapter/
git commit -m "feat(adapter): add plugin_dirs() and plugin_config_paths() to AgentAdapter trait"
```

---

### 任务 2：创建 Plugin 管理模块（文件型 + 配置型）

**文件：**
- 创建：`src-tauri/src/manager/plugin.rs`
- 修改：`src-tauri/src/manager/mod.rs`

**背景：** Spec FR-5 #20 要求插件根据类型处理：文件型插件用 symlink 映射，配置型插件写入工具配置。Plugin 和 MCP 类似，但 Plugin 是文件/配置混合，MCP 是纯配置。

- [ ] **步骤 1：创建 plugin.rs**

在 `src-tauri/src/manager/plugin.rs` 写入：

```rust
// Plugin 管理 — 文件型 symlink + 配置型写入工具配置
// 与 MCP 不同：Plugin 可能是文件/目录（用 symlink）或配置条目（写入 JSON/TOML）

use crate::adapter::{AgentAdapter, claude::ClaudeAdapter, codex::CodexAdapter, opencode::OpenCodeAdapter};
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
pub fn install_plugin_to_repo(source: &Path, name: &str) -> Result<(), String> {
    linker::install_to_repo(source, name)
}

/// 为工具启用文件型插件（创建 symlink）
pub fn enable_file_plugin(plugin_name: &str, tool_id: &str) -> Result<(), String> {
    let repo = linker::ensure_repo_dir();
    let source = repo.join(plugin_name);
    if !source.exists() {
        return Err(format!("Plugin 不在全局仓库中: {}", plugin_name));
    }

    let adapter: Box<dyn AgentAdapter> = match tool_id {
        "claude" => Box::new(ClaudeAdapter),
        "codex" => Box::new(CodexAdapter),
        "opencode" => Box::new(OpenCodeAdapter),
        _ => return Err(format!("未知工具: {}", tool_id)),
    };

    let plugin_dirs = adapter.plugin_dirs();
    if plugin_dirs.is_empty() {
        return Err(format!("工具 {} 不支持文件型插件", tool_id));
    }

    let target_dir = &plugin_dirs[0];
    let _ = std::fs::create_dir_all(target_dir);
    let target = target_dir.join(plugin_name);
    linker::create_link(&source, &target)?;

    let ext_id = format!("plugin-{}", plugin_name);
    crate::store::upsert_assignment(&ext_id, tool_id, true, "valid")?;
    log::info!("文件型 Plugin {} 已为 {} 启用", plugin_name, tool_id);
    Ok(())
}

/// 为工具禁用文件型插件（移除 symlink）
pub fn disable_file_plugin(plugin_name: &str, tool_id: &str) -> Result<(), String> {
    let adapter: Box<dyn AgentAdapter> = match tool_id {
        "claude" => Box::new(ClaudeAdapter),
        "codex" => Box::new(CodexAdapter),
        "opencode" => Box::new(OpenCodeAdapter),
        _ => return Err(format!("未知工具: {}", tool_id)),
    };

    let plugin_dirs = adapter.plugin_dirs();
    if plugin_dirs.is_empty() {
        return Err(format!("工具 {} 不支持文件型插件", tool_id));
    }

    let target = plugin_dirs[0].join(plugin_name);
    linker::remove_link(&target)?;

    let ext_id = format!("plugin-{}", plugin_name);
    crate::store::upsert_assignment(&ext_id, tool_id, false, "missing")?;
    log::info!("文件型 Plugin {} 已为 {} 禁用", plugin_name, tool_id);
    Ok(())
}

/// 为工具启用配置型插件（写入工具配置文件的 plugins 段）
/// 目前仅支持 JSON 格式（Claude / OpenCode），TOML 格式（Codex）后续扩展
pub fn enable_config_plugin(plugin_name: &str, tool_id: &str, entries: &BTreeMap<String, serde_json::Value>) -> Result<(), String> {
    let adapter: Box<dyn AgentAdapter> = match tool_id {
        "claude" => Box::new(ClaudeAdapter),
        "codex" => Box::new(CodexAdapter),
        "opencode" => Box::new(OpenCodeAdapter),
        _ => return Err(format!("未知工具: {}", tool_id)),
    };

    let config_paths = adapter.plugin_config_paths();
    if config_paths.is_empty() {
        return Err(format!("工具 {} 不支持配置型插件", tool_id));
    }

    let config_path = &config_paths[0];
    let content = std::fs::read_to_string(config_path).unwrap_or_else(|_| "{}".to_string());

    match adapter.mcp_format() {
        crate::adapter::McpFormat::Json | crate::adapter::McpFormat::Jsonc => {
            let mut root: serde_json::Value = serde_json::from_str(&content)
                .map_err(|e| format!("解析 JSON 配置失败: {}", e))?;
            if root.get("plugins").is_none() {
                root["plugins"] = serde_json::json!({});
            }
            root["plugins"][plugin_name] = serde_json::to_value(entries)
                .map_err(|e| e.to_string())?;
            let pretty = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
            linker::write_atomic(config_path, &pretty)?;
        }
        crate::adapter::McpFormat::Toml => {
            // TOML 格式：写入 [plugins.<name>] 段
            let content = std::fs::read_to_string(config_path).unwrap_or_default();
            let mut doc: toml_edit::DocumentMut = content.parse()
                .map_err(|e| format!("解析 TOML 失败: {}", e))?;
            if doc.get("plugins").is_none() {
                doc["plugins"] = toml_edit::Item::Table(toml_edit::Table::new());
            }
            let plugin_table = &mut doc["plugins"][plugin_name];
            for (k, v) in entries {
                plugin_table[k] = toml_edit::value(&v.to_string());
            }
            let toml_str = doc.to_string();
            linker::write_atomic(config_path, &toml_str)?;
        }
    }

    let ext_id = format!("plugin-{}", plugin_name);
    crate::store::upsert_assignment(&ext_id, tool_id, true, "valid")?;
    log::info!("配置型 Plugin {} 已为 {} 启用", plugin_name, tool_id);
    Ok(())
}

/// 为工具禁用配置型插件（从配置文件中移除 plugins 段）
pub fn disable_config_plugin(plugin_name: &str, tool_id: &str) -> Result<(), String> {
    let adapter: Box<dyn AgentAdapter> = match tool_id {
        "claude" => Box::new(ClaudeAdapter),
        "codex" => Box::new(CodexAdapter),
        "opencode" => Box::new(OpenCodeAdapter),
        _ => return Err(format!("未知工具: {}", tool_id)),
    };

    let config_paths = adapter.plugin_config_paths();
    if config_paths.is_empty() {
        return Err(format!("工具 {} 不支持配置型插件", tool_id));
    }

    let config_path = &config_paths[0];

    match adapter.mcp_format() {
        crate::adapter::McpFormat::Json | crate::adapter::McpFormat::Jsonc => {
            let content = std::fs::read_to_string(config_path).unwrap_or_else(|_| "{}".to_string());
            let mut root: serde_json::Value = serde_json::from_str(&content)
                .map_err(|e| format!("解析 JSON 配置失败: {}", e))?;
            if let Some(plugins) = root.get_mut("plugins").and_then(|p| p.as_object_mut()) {
                plugins.remove(plugin_name);
            }
            let pretty = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
            linker::write_atomic(config_path, &pretty)?;
        }
        crate::adapter::McpFormat::Toml => {
            let content = std::fs::read_to_string(config_path).unwrap_or_default();
            let mut doc: toml_edit::DocumentMut = content.parse()
                .map_err(|e| format!("解析 TOML 失败: {}", e))?;
            if let Some(plugins) = doc.get_mut("plugins").and_then(|p| p.as_table_mut()) {
                plugins.remove(plugin_name);
            }
            let toml_str = doc.to_string();
            linker::write_atomic(config_path, &toml_str)?;
        }
    }

    let ext_id = format!("plugin-{}", plugin_name);
    crate::store::upsert_assignment(&ext_id, tool_id, false, "missing")?;
    log::info!("配置型 Plugin {} 已为 {} 禁用", plugin_name, tool_id);
    Ok(())
}

/// 统一 toggle 入口
pub fn toggle_plugin(plugin_name: &str, tool_id: &str, enabled: bool, kind: &str) -> Result<(), String> {
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
                let repo = dirs::home_dir().unwrap_or_default().join(".mam").join("plugins").join(format!("{}.json", plugin_name));
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
        _ => Err(format!("未知 Plugin 类型: {}", kind)),
    }
}
```

- [ ] **步骤 2：修改 manager/mod.rs 注册 plugin 模块**

在 `src-tauri/src/manager/mod.rs` 顶部添加：

```rust
pub mod plugin;
```

在 `manager/mod.rs` 中 `toggle_mcp()` 之后添加统一 toggle 入口：

```rust
/// 为工具启用/禁用 Plugin
pub fn toggle_plugin(plugin_name: &str, tool_id: &str, enabled: bool, kind: &str) -> Result<(), String> {
    plugin::toggle_plugin(plugin_name, tool_id, enabled, kind)
}
```

- [ ] **步骤 3：编译验证**

运行：`cargo check`
预期：PASS

- [ ] **步骤 4：Commit**

```bash
git add src-tauri/src/manager/
git commit -m "feat(plugin): add plugin management module (file + config types)"
```

---

### 任务 3：Layer 2/3 目录结构落地

**文件：**
- 创建：`src-tauri/src/linker/layer2.rs`
- 创建：`src-tauri/src/linker/layer3.rs`
- 修改：`src-tauri/src/linker/mod.rs`

**背景：** Plan 中设计了三层映射架构：Layer 1（SSOT `~/.mam/skills/`）→ Layer 2（工具级 `~/.mam/active/<tool>/`）→ Layer 3（子 Agent 级 `~/.mam/active/<tool>/<subagent>/`）。当前代码中 Skill 直接 symlink 到工具实际目录（如 `~/.claude/skills/`），没有经过 Layer 2。需要改为：Skill 原始文件在 Layer 1，Layer 2 存放 symlink，工具实际目录再 symlink 到 Layer 2。

**注意：** 这是一个破坏性变更——已存在的 symlink 需要迁移。为简化，本计划采用"渐进式"策略：新启用 Skill 走 Layer 2，已有 Skill 在下次重新应用预设时迁移。

- [ ] **步骤 1：创建 layer2.rs**

```rust
// Layer 2：工具级激活目录管理
// ~/.mam/active/<tool>/ 存放该工具已启用的 skill 链接

use std::path::{Path, PathBuf};

/// 获取 Layer 2 基础目录
pub fn active_base_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".mam").join("active")
}

/// 获取指定工具的 Layer 2 目录
pub fn tool_active_dir(tool_id: &str) -> PathBuf {
    active_base_dir().join(tool_id)
}

/// 确保 Layer 2 目录存在
pub fn ensure_tool_active_dir(tool_id: &str) -> PathBuf {
    let dir = tool_active_dir(tool_id);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 创建 Layer 2 symlink：从 Layer 1 源文件 → Layer 2 工具目录
pub fn link_skill_to_layer2(skill_name: &str, tool_id: &str) -> Result<PathBuf, String> {
    let repo = super::ensure_repo_dir();
    let source = repo.join(skill_name);
    if !source.exists() {
        return Err(format!("Skill 不在全局仓库: {}", skill_name));
    }
    let layer2_dir = ensure_tool_active_dir(tool_id);
    let target = layer2_dir.join(skill_name);
    super::create_link(&source, &target)?;
    Ok(target)
}

/// 从 Layer 2 移除 skill 链接
pub fn unlink_skill_from_layer2(skill_name: &str, tool_id: &str) -> Result<(), String> {
    let target = tool_active_dir(tool_id).join(skill_name);
    super::remove_link(&target)
}

/// 列出工具在 Layer 2 中已启用的 skill
pub fn list_layer2_skills(tool_id: &str) -> Vec<String> {
    let dir = tool_active_dir(tool_id);
    let mut skills = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_symlink() || path.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    skills.push(name.to_string());
                }
            }
        }
    }
    skills.sort();
    skills
}
```

- [ ] **步骤 2：创建 layer3.rs**

```rust
// Layer 3：子 Agent 级激活目录管理
// ~/.mam/active/<tool>/<subagent>/ 存放子 Agent 已启用的 skill 链接
// 仅 Hermes 和 OpenCode 等支持子 Agent 独立 skill 目录的工具有此层
// Claude Code 和 Codex CLI 不支持子 Agent 独立目录，此层对其为"仅 UI 记录"

use std::path::PathBuf;

/// 获取指定工具+子 Agent 的 Layer 3 目录
pub fn subagent_active_dir(tool_id: &str, sub_agent_id: &str) -> PathBuf {
    super::layer2::tool_active_dir(tool_id).join(sub_agent_id)
}

/// 确保 Layer 3 目录存在
pub fn ensure_subagent_active_dir(tool_id: &str, sub_agent_id: &str) -> PathBuf {
    let dir = subagent_active_dir(tool_id, sub_agent_id);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 创建 Layer 3 symlink：从 Layer 1 源文件 → Layer 3 子 Agent 目录
/// 约束：Layer 3 只能链接 Layer 2 中已存在的 skill（工具级范围的子集）
pub fn link_skill_to_layer3(skill_name: &str, tool_id: &str, sub_agent_id: &str) -> Result<PathBuf, String> {
    // 检查工具级是否已启用
    let layer2_skills = super::layer2::list_layer2_skills(tool_id);
    if !layer2_skills.contains(&skill_name.to_string()) {
        return Err(format!(
            "Skill {} 未在 {} 的工具级分配中启用，无法分配给子 Agent {}",
            skill_name, tool_id, sub_agent_id
        ));
    }

    let repo = super::ensure_repo_dir();
    let source = repo.join(skill_name);
    let layer3_dir = ensure_subagent_active_dir(tool_id, sub_agent_id);
    let target = layer3_dir.join(skill_name);
    super::create_link(&source, &target)?;
    Ok(target)
}

/// 从 Layer 3 移除 skill 链接
pub fn unlink_skill_from_layer3(skill_name: &str, tool_id: &str, sub_agent_id: &str) -> Result<(), String> {
    let target = subagent_active_dir(tool_id, sub_agent_id).join(skill_name);
    super::remove_link(&target)
}

/// 列出子 Agent 在 Layer 3 中已启用的 skill
pub fn list_layer3_skills(tool_id: &str, sub_agent_id: &str) -> Vec<String> {
    let dir = subagent_active_dir(tool_id, sub_agent_id);
    let mut skills = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_symlink() || path.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    skills.push(name.to_string());
                }
            }
        }
    }
    skills.sort();
    skills
}

/// 当工具级禁用 skill 时，自动从所有子 Agent 中移除
pub fn cleanup_layer3_on_tool_disable(skill_name: &str, tool_id: &str) -> Result<(), String> {
    let tool_dir = super::layer2::tool_active_dir(tool_id);
    if !tool_dir.exists() {
        return Ok(());
    }
    let subagents: Vec<String> = std::fs::read_dir(&tool_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.is_dir() && !path.is_symlink() {
                e.file_name().to_str().map(String::from)
            } else {
                None
            }
        })
        .collect();

    for sub in &subagents {
        let target = subagent_active_dir(tool_id, sub).join(skill_name);
        if target.exists() || target.is_symlink() {
            let _ = super::remove_link(&target);
        }
    }
    Ok(())
}
```

- [ ] **步骤 3：修改 linker/mod.rs 导出 layer2/layer3**

在 `src-tauri/src/linker/mod.rs` 顶部添加：

```rust
pub mod layer2;
pub mod layer3;
```

- [ ] **步骤 4：修改 manager/mod.rs 的 enable_skill_for_tool 走 Layer 2**

将 `src-tauri/src/manager/mod.rs` 中的 `enable_skill_for_tool` 改为使用 Layer 2：

```rust
/// 为工具启用 skill（创建 Layer 2 symlink）
pub fn enable_skill_for_tool(skill_name: &str, tool_id: &str) -> Result<(), String> {
    let layer2_path = crate::linker::layer2::link_skill_to_layer2(skill_name, tool_id)?;

    // 同时创建到工具实际目录的 symlink（保持向后兼容）
    if let Some(tool_skill_dir) = get_tool_skill_dir(tool_id) {
        let _ = std::fs::create_dir_all(&tool_skill_dir);
        let tool_target = tool_skill_dir.join(skill_name);
        // 如果工具目录已有同名链接，先移除
        if tool_target.exists() || tool_target.is_symlink() {
            let _ = crate::linker::remove_link(&tool_target);
        }
        // 工具目录 symlink 指向 Layer 2（而非直接指向 Layer 1）
        crate::linker::create_link(&layer2_path, &tool_target)?;
    }

    let ext_id = format!("skill-{}", skill_name);
    store::upsert_assignment(&ext_id, tool_id, true, "valid")?;
    log::info!("Skill {} 已为 {} 启用（Layer 2）", skill_name, tool_id);
    Ok(())
}

/// 为工具禁用 skill（移除 Layer 2 symlink + 工具目录 symlink）
pub fn disable_skill_for_tool(skill_name: &str, tool_id: &str) -> Result<(), String> {
    // 移除工具实际目录的 symlink
    if let Some(tool_skill_dir) = get_tool_skill_dir(tool_id) {
        let tool_target = tool_skill_dir.join(skill_name);
        let _ = crate::linker::remove_link(&tool_target);
    }

    // 清理 Layer 3（所有子 Agent）
    let _ = crate::linker::layer3::cleanup_layer3_on_tool_disable(skill_name, tool_id);

    // 移除 Layer 2 symlink
    crate::linker::layer2::unlink_skill_from_layer2(skill_name, tool_id)?;

    let ext_id = format!("skill-{}", skill_name);
    store::upsert_assignment(&ext_id, tool_id, false, "missing")?;
    log::info!("Skill {} 已为 {} 禁用", skill_name, tool_id);
    Ok(())
}
```

- [ ] **步骤 5：修改 assign_skill_to_subagent 走 Layer 3**

将 `manager/mod.rs` 中的 `assign_skill_to_subagent` 改为使用 Layer 3：

```rust
/// 为子 Agent 分配 skill（带约束检查，走 Layer 3）
pub fn assign_skill_to_subagent(skill_name: &str, tool_id: &str, sub_agent_id: &str) -> Result<(), String> {
    // 约束：必须在工具级范围内
    if !is_skill_in_tool_range(skill_name, tool_id) {
        return Err(format!("Skill {} 未在 {} 的工具级分配中启用，无法分配给子 Agent", skill_name, tool_id));
    }

    let adapter: Box<dyn AgentAdapter> = match tool_id {
        "claude" => Box::new(ClaudeAdapter),
        "codex" => Box::new(CodexAdapter),
        "opencode" => Box::new(OpenCodeAdapter),
        _ => return Err(format!("未知工具: {}", tool_id)),
    };

    // 对于不支持子 Agent 独立目录的工具（Claude, Codex），仅记录到数据库
    let has_subagent_dir = adapter.subagent_dir().is_some();

    if has_subagent_dir {
        // 走 Layer 3 目录结构
        crate::linker::layer3::link_skill_to_layer3(skill_name, tool_id, sub_agent_id)?;

        // 同时创建到工具子 Agent 实际目录的 symlink
        if let Some(skill_dir) = adapter.skill_dirs().into_iter().next() {
            let subagent_dir = skill_dir.join("subagents").join(sub_agent_id);
            let _ = std::fs::create_dir_all(&subagent_dir);
            let tool_target = subagent_dir.join(skill_name);
            let layer3_path = crate::linker::layer3::subagent_active_dir(tool_id, sub_agent_id).join(skill_name);
            if tool_target.exists() || tool_target.is_symlink() {
                let _ = crate::linker::remove_link(&tool_target);
            }
            crate::linker::create_link(&layer3_path, &tool_target)?;
        }
    }

    let ext_id = format!("skill-{}", skill_name);
    // 子 Agent 分配记录到 assignments 表（sub_agent_id 字段）
    let conn = crate::store::DB.lock().unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    let id = format!("{}-{}-{}", ext_id, tool_id, sub_agent_id);
    let _ = conn.execute(
        "INSERT OR REPLACE INTO extension_assignments (id, extension_id, agent_tool_id, sub_agent_id, enabled, link_status, assigned_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![id, ext_id, tool_id, sub_agent_id, 1i64, if has_subagent_dir { "valid" } else { "ui-only" }, now],
    );

    log::info!("Skill {} 已分配给子 Agent {}（{}）", skill_name, sub_agent_id, if has_subagent_dir { "Layer 3" } else { "UI-only" });
    Ok(())
}
```

- [ ] **步骤 6：编译验证**

运行：`cargo check`
预期：PASS（可能需要处理 borrow checker 问题，按编译错误修复）

- [ ] **步骤 7：Commit**

```bash
git add src-tauri/src/linker/ src-tauri/src/manager/
git commit -m "feat(linker): implement Layer 2/3 directory structure for skill mapping"
```

---

### 任务 4：扩展预设组支持 Plugin 和子 Agent 级

**文件：**
- 修改：`src-tauri/src/manager/preset.rs`
- 修改：`src-tauri/src/commands.rs`
- 修改：`src-tauri/src/store.rs`
- 修改：`src-tauri/src/lib.rs`

**背景：** Spec FR-6 #24 要求预设组可含 skill + MCP + 插件。当前 `preset.rs` 只处理 `skill` 和 `mcp`。需要扩展支持 `plugin`。同时 FR-7 #31 要求子 Agent 级也支持预设组操作。

- [ ] **步骤 1：修改 preset.rs 支持 plugin kind**

在 `src-tauri/src/manager/preset.rs` 的 `apply_preset` 中，在 `match kind.as_str()` 分支添加 `plugin`：

```rust
"plugin" => {
    let name = ext_id.strip_prefix("plugin-").unwrap_or(ext_id);
    // 从 extensions 表读取 plugin 的 kind 字段（file 或 config）
    let plugin_kind = crate::store::list_extensions()
        .iter()
        .find(|e| e.id == *ext_id)
        .and_then(|e| e.kind.clone().into())
        .unwrap_or_else(|| "file".to_string());
    crate::manager::plugin::toggle_plugin(name, tool_id, true, &plugin_kind)
}
```

在 `deactivate_preset` 中同样添加 `plugin` 分支：

```rust
"plugin" => {
    let name = ext_id.strip_prefix("plugin-").unwrap_or(ext_id);
    let plugin_kind = crate::store::list_extensions()
        .iter()
        .find(|e| e.id == *ext_id)
        .map(|e| e.kind.clone())
        .unwrap_or_else(|| "file".to_string());
    crate::manager::plugin::toggle_plugin(name, tool_id, false, &plugin_kind)
}
```

- [ ] **步骤 2：创建子 Agent 级 apply_preset 函数**

在 `preset.rs` 末尾添加：

```rust
/// 应用预设组到子 Agent
pub fn apply_preset_to_subagent(preset_id: &str, tool_id: &str, sub_agent_id: &str) -> ApplyResult {
    let items = store::get_preset_items(preset_id);
    let mut success = 0;
    let mut failures = Vec::new();
    let mut conflicts = Vec::new();

    for (ext_id, kind) in &items {
        // 子 Agent 级只支持 skill（MCP 和 Plugin 是工具级配置）
        if kind != "skill" {
            conflicts.push(format!("{} 类型 {} 不支持子 Agent 级分配", ext_id, kind));
            continue;
        }

        // 检查是否在工具级范围内
        let name = ext_id.strip_prefix("skill-").unwrap_or(ext_id);
        if !crate::manager::is_skill_in_tool_range(name, tool_id) {
            failures.push(format!("{}: 该 skill 未在工具级启用", ext_id));
            continue;
        }

        let result = crate::manager::assign_skill_to_subagent(name, tool_id, sub_agent_id);
        match result {
            Ok(()) => success += 1,
            Err(e) => failures.push(format!("{}: {}", ext_id, e)),
        }
    }

    let _ = store::record_preset_application_subagent(preset_id, tool_id, sub_agent_id, true);
    log::info!("预设组 {} → {}:{} — 成功 {} 失败 {} 冲突 {}",
        preset_id, tool_id, sub_agent_id, success, failures.len(), conflicts.len());
    ApplyResult { success, failures, conflicts }
}

/// 取消激活子 Agent 级预设组
pub fn deactivate_preset_from_subagent(preset_id: &str, tool_id: &str, sub_agent_id: &str) -> Result<(), String> {
    let items = store::get_preset_items(preset_id);
    let mut errors = Vec::new();
    for (ext_id, kind) in &items {
        if kind != "skill" { continue; }
        let name = ext_id.strip_prefix("skill-").unwrap_or(ext_id);
        let result = crate::linker::layer3::unlink_skill_from_layer3(name, tool_id, sub_agent_id);
        if let Err(e) = result {
            errors.push(format!("{}: {}", ext_id, e));
        }
        // 更新数据库记录
        let conn = crate::store::DB.lock().unwrap();
        let id = format!("{}-{}-{}", ext_id, tool_id, sub_agent_id);
        let _ = conn.execute(
            "UPDATE extension_assignments SET enabled = 0, link_status = 'missing' WHERE id = ?1",
            rusqlite::params![id],
        );
    }
    store::record_preset_application_subagent(preset_id, tool_id, sub_agent_id, false)?;
    if !errors.is_empty() {
        log::warn!("deactivate_preset_from_subagent 部分失败: {:?}", errors);
    }
    log::info!("预设组 {} 从 {}:{} 取消激活", preset_id, tool_id, sub_agent_id);
    Ok(())
}
```

- [ ] **步骤 3：修改 store.rs 增加子 Agent 级预设记录**

在 `src-tauri/src/store.rs` 的 `record_preset_application` 之后添加：

```rust
pub fn record_preset_application_subagent(preset_id: &str, tool_id: &str, sub_agent_id: &str, active: bool) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    let id = format!("{}-{}-{}", preset_id, tool_id, sub_agent_id);
    conn.execute(
        "INSERT OR REPLACE INTO preset_applications (id, preset_id, agent_tool_id, sub_agent_id, applied_at, active) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, preset_id, tool_id, sub_agent_id, now, active as i64],
    ).map_err(|e| e.to_string())?;
    Ok(())
}
```

注意：`preset_applications` 表已有 `sub_agent_id` 字段（见 `init_schema`），无需改表结构。

- [ ] **步骤 4：修改 commands.rs 添加 IPC 命令**

在 `src-tauri/src/commands.rs` 中添加：

```rust
/// 为工具启用/禁用 Plugin
#[tauri::command]
pub fn toggle_plugin_for_tool(plugin_name: String, tool_id: String, enabled: bool, kind: String) -> Result<(), String> {
    crate::manager::plugin::toggle_plugin(&plugin_name, &tool_id, enabled, &kind)
}

/// 应用预设组到子 Agent
#[tauri::command]
pub fn apply_preset_to_subagent(preset_id: String, tool_id: String, sub_agent_id: String) -> PresetApplyResult {
    let result = crate::manager::preset::apply_preset_to_subagent(&preset_id, &tool_id, &sub_agent_id);
    PresetApplyResult { success_count: result.success, failures: result.failures, conflicts: result.conflicts }
}

/// 取消激活子 Agent 级预设组
#[tauri::command]
pub fn deactivate_preset_from_subagent(preset_id: String, tool_id: String, sub_agent_id: String) -> Result<(), String> {
    crate::manager::preset::deactivate_preset_from_subagent(&preset_id, &tool_id, &sub_agent_id)
}
```

- [ ] **步骤 5：修改 lib.rs 注册新命令**

在 `src-tauri/src/lib.rs` 的 `invoke_handler` 中添加：

```rust
commands::toggle_plugin_for_tool,
commands::apply_preset_to_subagent,
commands::deactivate_preset_from_subagent,
```

- [ ] **步骤 6：编译验证**

运行：`cargo check`
预期：PASS

- [ ] **步骤 7：Commit**

```bash
git add src-tauri/src/
git commit -m "feat(preset): support plugin in presets + subagent-level preset apply/deactivate"
```

---

### 任务 5：MCP 配置前端 UI（添加/编辑/删除面板）

**文件：**
- 创建：`src/components/McpManager.tsx`
- 修改：`src/components/ExtensionList.tsx`
- 修改：`src/types/extension.ts`

**背景：** 后端已有 `read_mcp_servers`, `write_mcp_server`, `remove_mcp_server` 命令，但前端没有调用它们的 UI。用户无法在前端添加/编辑/删除 MCP 服务器。

- [ ] **步骤 1：扩展 extension 类型**

在 `src/types/extension.ts` 末尾添加 MCP 配置类型：

```typescript
export interface McpServerConfig {
  command: string;
  args: string[];
  env: Record<string, string>;
}

export interface McpServer {
  name: string;
  config: McpServerConfig;
}
```

- [ ] **步骤 2：创建 McpManager 组件**

在 `src/components/McpManager.tsx` 写入：

```tsx
import { useState, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Card } from "@/components/ui/card";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Plus, Trash2, Edit2, Server } from "lucide-react";
import type { McpServerConfig } from "@/types/extension";

const TOOLS = [
  { id: "claude", label: "Claude" },
  { id: "codex", label: "Codex" },
  { id: "opencode", label: "OpenCode" },
];

interface McpEntry {
  name: string;
  config: McpServerConfig;
}

export function McpManager({ toolId }: { toolId: string }) {
  const [servers, setServers] = useState<Record<string, McpServerConfig>>({});
  const [showAdd, setShowAdd] = useState(false);
  const [editingName, setEditingName] = useState<string | null>(null);
  const [form, setForm] = useState<McpServerConfig>({ command: "", args: [], env: {} });

  const load = useCallback(async () => {
    try {
      const data = await invoke<Record<string, McpServerConfig>>("read_mcp_servers", { toolId });
      // 后端返回的可能是 { mcpServers: {...} } 或 { mcp_servers: {...} } 或 { mcp: {...} }
      // 需要提取实际的 servers 对象
      const serversObj = data.mcpServers || data.mcp_servers || data.mcp || data;
      setServers(typeof serversObj === "object" && serversObj !== null ? serversObj : {});
    } catch (e) {
      console.error("Failed to load MCP servers:", e);
      setServers({});
    }
  }, [toolId]);

  const handleAdd = async () => {
    if (!form.command || !editingName) {
      toast.error("请填写名称和命令");
      return;
    }
    try {
      await invoke("write_mcp_server", {
        toolId,
        mcpName: editingName,
        command: form.command,
        args: form.args,
        env: form.env,
      });
      toast.success(`MCP "${editingName}" 已保存`);
      setShowAdd(false);
      setEditingName(null);
      setForm({ command: "", args: [], env: {} });
      load();
    } catch (e) {
      toast.error(`保存失败: ${e}`);
    }
  };

  const handleDelete = async (name: string) => {
    try {
      await invoke("remove_mcp_server", { toolId, mcpName: name });
      toast.success(`MCP "${name}" 已删除`);
      load();
    } catch (e) {
      toast.error(`删除失败: ${e}`);
    }
  };

  const openEdit = (name: string, config: McpServerConfig) => {
    setEditingName(name);
    setForm(config);
    setShowAdd(true);
  };

  const openAdd = () => {
    setEditingName("");
    setForm({ command: "", args: [], env: {} });
    setShowAdd(true);
  };

  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between">
        <h4 className="flex items-center gap-1.5 text-xs font-semibold">
          <Server className="h-3.5 w-3.5" />
          MCP 服务器
        </h4>
        <Button size="sm" variant="ghost" className="h-6 px-2 text-[10px]" onClick={openAdd}>
          <Plus className="mr-1 h-3 w-3" />
          添加
        </Button>
      </div>

      {Object.entries(servers).length === 0 ? (
        <p className="text-muted-foreground text-[10px]">暂无 MCP 服务器</p>
      ) : (
        <div className="space-y-1">
          {Object.entries(servers).map(([name, config]) => (
            <div key={name} className="flex items-center justify-between rounded border px-2 py-1 text-xs">
              <div className="min-w-0">
                <span className="font-medium">{name}</span>
                <span className="text-muted-foreground ml-1 text-[10px]">{config.command}</span>
              </div>
              <div className="flex shrink-0 gap-1">
                <Button size="sm" variant="ghost" className="h-5 w-5 p-0" onClick={() => openEdit(name, config)}>
                  <Edit2 className="h-3 w-3" />
                </Button>
                <Button size="sm" variant="ghost" className="h-5 w-5 p-0 text-red-500" onClick={() => handleDelete(name)}>
                  <Trash2 className="h-3 w-3" />
                </Button>
              </div>
            </div>
          ))}
        </div>
      )}

      <Dialog open={showAdd} onOpenChange={setShowAdd}>
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle className="text-sm">{editingName ? `编辑 ${editingName}` : "添加 MCP 服务器"}</DialogTitle>
          </DialogHeader>
          <div className="space-y-2">
            {!editingName && (
              <Input
                placeholder="名称（如 filesystem）"
                value={editingName ?? ""}
                onChange={(e) => setEditingName(e.currentTarget.value)}
                className="text-xs"
              />
            )}
            <Input
              placeholder="命令（如 npx）"
              value={form.command}
              onChange={(e) => setForm((f) => ({ ...f, command: e.currentTarget.value }))}
              className="text-xs"
            />
            <Input
              placeholder="参数（逗号分隔，如 -y,@modelcontextprotocol/server-filesystem）"
              value={form.args.join(",")}
              onChange={(e) => setForm((f) => ({ ...f, args: e.currentTarget.value.split(",").filter(Boolean) }))}
              className="text-xs"
            />
            <div className="flex justify-end gap-2">
              <Button size="sm" variant="outline" onClick={() => setShowAdd(false)}>取消</Button>
              <Button size="sm" onClick={handleAdd}>保存</Button>
            </div>
          </div>
        </DialogContent>
      </Dialog>
    </div>
  );
}
```

- [ ] **步骤 3：修改 ExtensionList.tsx 集成 McpManager**

在 `src/components/ExtensionList.tsx` 的 MCP 区域下方添加 McpManager：

找到 MCP 区域的结束位置（`</div>` 后），在其后添加：

```tsx
{/* MCP 管理面板 */}
<div className="mt-2">
  {TOOLS.map((tool) => (
    <div key={tool.id} className="mb-2">
      <McpManager toolId={tool.id} />
    </div>
  ))}
</div>
```

并在文件顶部添加 import：

```tsx
import { McpManager } from "@/components/McpManager";
```

- [ ] **步骤 4：TypeScript 编译验证**

运行：`cd /Users/jarvis/Documents/MultiAgents-Manager && pnpm tsc --noEmit`
预期：PASS（无类型错误）

- [ ] **步骤 5：Commit**

```bash
git add src/components/McpManager.tsx src/components/ExtensionList.tsx src/types/extension.ts
git commit -m "feat(ui): add MCP server add/edit/delete panel"
```

---

### 任务 6：前端扩展 Plugin 显示 + 子 Agent 预设操作

**文件：**
- 修改：`src/components/ExtensionList.tsx`
- 修改：`src/components/PresetList.tsx`

**背景：** ExtensionList 目前只显示 Skill 和 MCP，不显示 Plugin。PresetList 创建时只列出 extensions（包含所有 kind），但 UI 上需要明确区分。同时需要为子 Agent 添加预设应用入口。

- [ ] **步骤 1：修改 ExtensionList.tsx 显示 Plugin**

在 `src/components/ExtensionList.tsx` 中，在 MCP 区域后添加 Plugin 区域：

```tsx
{/* Plugin */}
<div>
  <h3 className="mb-2 flex items-center gap-2 text-sm font-semibold">
    <Plug className="h-4 w-4" />
    插件 ({plugins.length})
  </h3>
  {plugins.length === 0 ? (
    <div className="text-muted-foreground flex items-center gap-2 py-4 text-xs">
      <Info className="h-3.5 w-3.5" />
      暂无插件
    </div>
  ) : (
    <div className="space-y-1">
      {plugins.map((plugin) => (
        <div
          key={plugin.id}
          className="flex items-center justify-between rounded border p-2 text-sm"
        >
          <div className="min-w-0">
            <span className="font-medium">{plugin.name}</span>
            {plugin.description && (
              <span className="text-muted-foreground ml-2 text-xs">{plugin.description}</span>
            )}
          </div>
          <div className="flex shrink-0 gap-1">
            {TOOLS.map((tool) => {
              const assignment = plugin.assignments.find((a) => a.agentToolId === tool.id);
              const enabled = assignment?.enabled ?? false;
              return (
                <Button
                  key={tool.id}
                  variant={enabled ? "default" : "outline"}
                  size="sm"
                  className="h-6 px-2 text-[10px]"
                  onClick={() => togglePlugin(plugin.name, tool.id, !enabled, plugin.kind)}
                >
                  {tool.label}
                </Button>
              );
            })}
          </div>
        </div>
      ))}
    </div>
  )}
</div>
```

在文件顶部添加 `Plug` import：

```tsx
import { Package, Link2, FolderPlus, Info, RefreshCw, Plug } from "lucide-react";
```

在 `ExtensionList` 组件内部添加 `plugins` 过滤和 `togglePlugin` 函数：

```tsx
const plugins = extensions.filter((e) => e.kind === "plugin");

const togglePlugin = async (pluginName: string, toolId: string, enabled: boolean, kind: string) => {
  try {
    await invoke("toggle_plugin_for_tool", { pluginName, toolId, enabled, kind });
    toast.success(`${pluginName} 已${enabled ? "启用" : "禁用"} for ${toolId}`);
    load();
  } catch (e) {
    toast.error(`操作失败: ${e}`);
  }
};
```

- [ ] **步骤 2：修改 PresetList.tsx 支持子 Agent 级应用**

在 `src/components/PresetList.tsx` 中，每个预设的工具按钮区域扩展为支持子 Agent：

找到 `TOOLS.map((tool) => (...))` 区域，将其改为：

```tsx
{TOOLS.map((tool) => (
  <div key={tool.id} className="space-y-1">
    <div className="flex gap-0.5">
      <Button size="sm" variant="outline" className="h-6 px-2 text-[10px]"
        onClick={() => handleApply(preset.id, preset.name, tool.id)}>
        <Play className="mr-1 h-2.5 w-2.5" />
        {tool.label}
      </Button>
      <Button size="sm" variant="ghost" className="h-6 w-6 p-0 text-[10px]"
        onClick={() => handleDeactivate(preset.id, preset.name, tool.id)}>
        <X className="h-2.5 w-2.5" />
      </Button>
    </div>
    {/* 子 Agent 级操作 */}
    <SubAgentPresetActions presetId={preset.id} presetName={preset.name} toolId={tool.id} />
  </div>
))}
```

在文件末尾添加子组件：

```tsx
function SubAgentPresetActions({ presetId, presetName, toolId }: { presetId: string; presetName: string; toolId: string }) {
  const [subAgents, setSubAgents] = useState<string[]>([]);
  const [expanded, setExpanded] = useState(false);

  const loadSubAgents = async () => {
    try {
      const data = await invoke<string[]>("detect_subagents", { toolId });
      setSubAgents(data);
    } catch (e) {
      console.error("Failed to load subagents:", e);
    }
  };

  const handleApplyToSubagent = async (subAgentId: string) => {
    try {
      const result = await invoke<PresetApplyResult>("apply_preset_to_subagent", { presetId, toolId, subAgentId });
      if (result.failures.length > 0) {
        toast.warning(`部分成功: ${result.successCount} 项成功, ${result.failures.length} 项失败`);
      } else {
        toast.success(`"${presetName}" 已应用到 ${toolId}:${subAgentId}`);
      }
    } catch (e) {
      toast.error(`应用失败: ${e}`);
    }
  };

  const handleDeactivateFromSubagent = async (subAgentId: string) => {
    try {
      await invoke("deactivate_preset_from_subagent", { presetId, toolId, subAgentId });
      toast.success(`"${presetName}" 已从 ${toolId}:${subAgentId} 取消`);
    } catch (e) {
      toast.error(`取消失败: ${e}`);
    }
  };

  if (subAgents.length === 0) return null;

  return (
    <div>
      <button
        onClick={() => { setExpanded(!expanded); if (!expanded) loadSubAgents(); }}
        className="text-muted-foreground text-[10px] hover:text-foreground"
      >
        {expanded ? "收起子 Agent" : "子 Agent ▼"}
      </button>
      {expanded && (
        <div className="ml-2 space-y-0.5 border-l pl-2">
          {subAgents.map((sa) => (
            <div key={sa} className="flex items-center gap-1 text-[10px]">
              <span className="text-muted-foreground">{sa}</span>
              <Button size="sm" variant="ghost" className="h-4 px-1 text-[9px]"
                onClick={() => handleApplyToSubagent(sa)}>
                <Play className="mr-0.5 h-2 w-2" />应用
              </Button>
              <Button size="sm" variant="ghost" className="h-4 w-4 p-0 text-[9px]"
                onClick={() => handleDeactivateFromSubagent(sa)}>
                <X className="h-2 w-2" />
              </Button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
```

- [ ] **步骤 3：TypeScript 编译验证**

运行：`pnpm tsc --noEmit`
预期：PASS

- [ ] **步骤 4：Commit**

```bash
git add src/components/ExtensionList.tsx src/components/PresetList.tsx
git commit -m "feat(ui): display plugins in ExtensionList + subagent preset actions"
```

---

### 任务 7：用户可配置提示音

**文件：**
- 修改：`src/lib/audio.ts`
- 修改：`src/pages/settings.tsx`

**背景：** Spec FR-3 #10 要求用户能为不同状态配置不同的提示音。当前 `audio.ts` 只有固定频率的 `playWaitingSound()` 和 `playFinishedSound()`。需要让用户在 Settings 页配置频率。

- [ ] **步骤 1：修改 audio.ts 支持可配置频率**

将 `src/lib/audio.ts` 改为：

```typescript
// Web Audio API 提示音 — 支持用户配置频率

let audioCtx: AudioContext | null = null;

function getAudioContext(): AudioContext | null {
  if (typeof window === "undefined") return null;
  if (!audioCtx) {
    audioCtx = new AudioContext();
  }
  if (audioCtx.state === "suspended") {
    audioCtx.resume();
  }
  return audioCtx;
}

/** 播放单音 */
function playTone(frequency: number, duration: number, delay: number = 0) {
  const ctx = getAudioContext();
  if (!ctx) return;

  const oscillator = ctx.createOscillator();
  const gain = ctx.createGain();

  oscillator.connect(gain);
  gain.connect(ctx.destination);

  const startTime = ctx.currentTime + delay;
  oscillator.frequency.value = frequency;
  oscillator.type = "sine";

  gain.gain.setValueAtTime(0, startTime);
  gain.gain.linearRampToValueAtTime(0.3, startTime + 0.01);
  gain.gain.exponentialRampToValueAtTime(0.001, startTime + duration);

  oscillator.start(startTime);
  oscillator.stop(startTime + duration);
}

// === 默认频率配置 ===
const DEFAULT_FREQUENCIES = {
  waiting: { primary: 880, secondary: 1174.66 },
  finished: { primary: 440 },
};

/** 从 localStorage 读取用户配置 */
function getUserFrequencies() {
  try {
    const saved = localStorage.getItem("mam-audio-frequencies");
    if (saved) {
      return { ...DEFAULT_FREQUENCIES, ...JSON.parse(saved) };
    }
  } catch {
    // ignore parse error
  }
  return DEFAULT_FREQUENCIES;
}

/** 保存用户配置到 localStorage */
export function saveUserFrequencies(config: typeof DEFAULT_FREQUENCIES) {
  localStorage.setItem("mam-audio-frequencies", JSON.stringify(config));
}

/** 获取当前频率配置 */
export function getAudioConfig() {
  return getUserFrequencies();
}

/** 等待用户输入 — 双音 chime */
export function playWaitingSound() {
  const cfg = getUserFrequencies().waiting;
  playTone(cfg.primary, 0.15, 0);
  playTone(cfg.secondary, 0.3, 0.12);
}

/** 任务完成 — 低音单音 */
export function playFinishedSound() {
  const cfg = getUserFrequencies().finished;
  playTone(cfg.primary, 0.4, 0);
}

/** 测试提示音 */
export function playTestSound() {
  playTone(660, 0.15, 0);
  playTone(880, 0.2, 0.1);
}

/** 按状态播放对应提示音 */
export function playSoundForStatus(status: string) {
  switch (status) {
    case "waiting":
      playWaitingSound();
      break;
    case "finished":
      playFinishedSound();
      break;
    default:
      break;
  }
}
```

- [ ] **步骤 2：修改 settings.tsx 添加提示音配置**

在 `src/pages/settings.tsx` 的 `notifications` section 中，在"提示音测试"下方添加频率配置：

```tsx
<div className="border-t" />
<div className="space-y-2 py-2.5">
  <label className="text-sm font-medium">提示音频率配置</label>
  <div className="space-y-1">
    <div className="flex items-center gap-2">
      <span className="text-muted-foreground w-20 text-xs">等待状态</span>
      <Input
        type="number"
        className="h-7 w-24 text-xs"
        value={audioConfig.waiting.primary}
        onChange={(e) => updateAudioConfig("waiting", "primary", parseFloat(e.currentTarget.value) || 880)}
      />
      <span className="text-muted-foreground text-xs">Hz</span>
      <Input
        type="number"
        className="h-7 w-24 text-xs"
        value={audioConfig.waiting.secondary}
        onChange={(e) => updateAudioConfig("waiting", "secondary", parseFloat(e.currentTarget.value) || 1174.66)}
      />
      <span className="text-muted-foreground text-xs">Hz</span>
    </div>
    <div className="flex items-center gap-2">
      <span className="text-muted-foreground w-20 text-xs">完成状态</span>
      <Input
        type="number"
        className="h-7 w-24 text-xs"
        value={audioConfig.finished.primary}
        onChange={(e) => updateAudioConfig("finished", "primary", parseFloat(e.currentTarget.value) || 440)}
      />
      <span className="text-muted-foreground text-xs">Hz</span>
    </div>
  </div>
  <Button variant="outline" size="sm" onClick={() => playWaitingSound()}>
    <Volume2 className="mr-1.5 h-3.5 w-3.5" />
    测试等待音
  </Button>
  <Button variant="outline" size="sm" className="ml-2" onClick={() => playFinishedSound()}>
    <Volume2 className="mr-1.5 h-3.5 w-3.5" />
    测试完成音
  </Button>
</div>
```

在 `SettingsPage` 组件顶部添加 state：

```tsx
import { getAudioConfig, saveUserFrequencies, playWaitingSound, playFinishedSound } from "@/lib/audio";

// 在 SettingsPage 组件内：
const [audioConfig, setAudioConfig] = useState(getAudioConfig());

const updateAudioConfig = (status: "waiting" | "finished", key: "primary" | "secondary", value: number) => {
  const next = {
    ...audioConfig,
    [status]: {
      ...audioConfig[status],
      [key]: value,
    },
  };
  setAudioConfig(next);
  saveUserFrequencies(next);
};
```

- [ ] **步骤 3：TypeScript 编译验证**

运行：`pnpm tsc --noEmit`
预期：PASS

- [ ] **步骤 4：Commit**

```bash
git add src/lib/audio.ts src/pages/settings.tsx
git commit -m "feat(settings): configurable notification sound frequencies"
```

---

### 任务 8：Plugin 自动导入（与 Skill 导入对齐）

**文件：**
- 修改：`src-tauri/src/manager/mod.rs`

**背景：** Skill 有 `auto_import_skills()` 在首次启动时自动扫描各工具目录导入。Plugin 也需要类似的自动导入机制。

- [ ] **步骤 1：扩展 auto_import_skills 为 auto_import_extensions**

在 `src-tauri/src/manager/mod.rs` 中，将 `auto_import_skills` 改名为 `auto_import_extensions`，并增加 Plugin 扫描：

在 `auto_import_skills` 函数末尾（`ImportStats` 返回之前），添加 Plugin 扫描逻辑：

```rust
    // ===== Plugin 扫描 =====
    let plugin_sources = [
        ("claude", dirs::home_dir().unwrap_or_default().join(".claude").join("plugins")),
        ("codex", dirs::home_dir().unwrap_or_default().join(".codex").join("plugins")),
        ("opencode", dirs::home_dir().unwrap_or_default().join(".config").join("opencode").join("plugins")),
    ];

    for (tool_id, plugins_dir) in &plugin_sources {
        if !plugins_dir.exists() { continue; }
        if let Ok(entries) = std::fs::read_dir(plugins_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if seen_names.contains(&name) { continue; }
                seen_names.insert(name.clone());

                // 判断是文件型还是配置型：有子文件/目录 → file，只有单个 .json → config
                let kind = if path.is_dir() { "file" } else { "config" };

                // 复制到全局仓库 ~/.mam/plugins/
                let plugin_repo = dirs::home_dir().unwrap_or_default().join(".mam").join("plugins");
                let _ = std::fs::create_dir_all(&plugin_repo);
                let dest = plugin_repo.join(&name);
                if dest.exists() {
                    let _ = std::fs::remove_dir_all(&dest);
                }
                if path.is_dir() {
                    let _ = crate::linker::copy_dir_recursive(&path, &dest);
                } else {
                    let _ = std::fs::copy(&path, &dest);
                }

                let ext = crate::store::ExtensionRecord {
                    id: format!("plugin-{}", name),
                    kind: "plugin".to_string(),
                    name: name.clone(),
                    description: None,
                    source_path: path.to_string_lossy().to_string(),
                    source_url: None,
                    version: None,
                    tags: Some(tool_id.to_string()),
                    suite: None,
                    source_tool: Some(tool_id.to_string()),
                };
                let _ = crate::store::insert_extension(&ext);
                imported += 1;
            }
        }
    }
```

同时修改 `ImportStats` 中的 `source_counts` 类型以支持 plugin 计数（已有 `Vec<(String, usize)>`，可直接复用）。

- [ ] **步骤 2：修改 lib.rs 调用**

`lib.rs` 中的 `manager::auto_import_skills(false)` 改为 `manager::auto_import_extensions(false)`。

- [ ] **步骤 3：编译验证**

运行：`cargo check`
预期：PASS

- [ ] **步骤 4：Commit**

```bash
git add src-tauri/src/manager/mod.rs src-tauri/src/lib.rs
git commit -m "feat(plugin): auto-import plugins from tool directories on first launch"
```

---

### 任务 9：端到端集成测试

**文件：**
- 修改：`src-tauri/src/adapter/mod.rs`（测试模块）

**背景：** 需要验证新增功能的基本正确性。

- [ ] **步骤 1：添加 Layer 2/3 单元测试**

在 `src-tauri/src/linker/layer2.rs` 末尾添加：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_active_dir() {
        let dir = tool_active_dir("claude");
        assert!(dir.to_string_lossy().contains(".mam/active/claude"));
    }

    #[test]
    fn test_ensure_tool_active_dir() {
        let dir = ensure_tool_active_dir("test_tool");
        assert!(dir.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
```

在 `src-tauri/src/linker/layer3.rs` 末尾添加：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subagent_active_dir() {
        let dir = subagent_active_dir("opencode", "researcher");
        assert!(dir.to_string_lossy().contains(".mam/active/opencode/researcher"));
    }
}
```

- [ ] **步骤 2：运行 Rust 测试**

运行：`cargo test`
预期：所有测试 PASS（已有测试 + 新增测试）

- [ ] **步骤 3：前端构建验证**

运行：`pnpm build`
预期：PASS（无构建错误）

- [ ] **步骤 4：Commit**

```bash
git add src-tauri/src/linker/
git commit -m "test(linker): add unit tests for Layer 2/3 directory management"
```

---

### 任务 10：Spec 覆盖度自检与最终 Commit

**文件：** 无新文件，纯检查

- [ ] **步骤 1：对照 Spec 逐条检查**

| Spec 需求 | 实现任务 | 状态 |
|-----------|---------|------|
| FR-5 #20 插件管理（文件型 symlink + 配置型写入） | 任务 2 | ✅ |
| FR-5 #17-18 Layer 2/3 目录结构 | 任务 3 | ✅ |
| FR-6 #24 预设组含 Skill+MCP+Plugin | 任务 4 | ✅ |
| FR-7 #31 子 Agent 级预设组 | 任务 4 | ✅ |
| FR-5 #19 MCP 配置前端 UI | 任务 5 | ✅ |
| FR-3 #10 用户可配置提示音 | 任务 7 | ✅ |
| FR-5 #22 更新资源后自动同步 | 已有（symlink 天然支持） | ✅ |
| FR-6 #28 预设组部分成功处理 | 已有（ApplyResult） | ✅ |
| FR-8 #36 安全扫描路径检查 | 已有（linker/install_to_repo） | ✅ |

- [ ] **步骤 2：检查占位符**

搜索计划中的红旗词汇：
- "待定" / "TODO" / "后续实现" / "补充细节" — 无
- "添加适当的错误处理" — 无
- "类似任务 N" — 无
- 所有代码步骤都有实际代码块 — 是

- [ ] **步骤 3：类型一致性检查**

- `ApplyResult` 在任务 4 前后一致（`success`, `failures`, `conflicts`）
- `ExtensionRecord` 的 `kind` 字段值 `"plugin"` 与数据库 schema 一致
- `toggle_plugin` 签名前后一致（`plugin_name, tool_id, enabled, kind`）
- IPC 命令名与前端 `invoke` 调用一致

- [ ] **步骤 4：最终 Commit**

```bash
git add specs/001-multi-agent-platform/
git commit -m "docs(plan): add implementation plan for remaining features (plugin, layer2/3, subagent preset, mcp ui, audio config)"
```

---

## 执行交接

**计划已完成并保存到 `docs/engineering/plans/2026-07-06-remaining-features.md`。两种执行方式：**

**1. 子代理驱动（推荐）** - 每个任务调度一个新的子代理，任务间进行审查，快速迭代

**2. 内联执行** - 在当前会话中使用 executing-plans 执行任务，批量执行并设有检查点供审查

**选哪种方式？**
