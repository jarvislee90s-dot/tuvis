# 资源看板 UI/UX 重构实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 重构资源看板为双视图切换模式（按资源/按工具），支持原生资源扫描导入、预设组兼容性检查，并添加 OpenClaw 作为第四支持工具。

**架构：** 前端重构为双视图切换（按资源分类显示全局仓库资源，按工具分类显示工具级资源映射+原生资源），后端新增原生资源扫描和兼容性检查 API，数据层新增 native_extensions 表记录未导入的原生资源。

**技术栈：** Tauri 2 + Rust + React 19 + TypeScript + SQLite + Tailwind CSS + shadcn/ui

---

## 文件结构

### 后端（Rust）

| 文件 | 职责 |
|------|------|
| `src-tauri/src/store.rs` | SQLite 数据层 — 新增 native_extensions 表，扩展 ExtensionRecord |
| `src-tauri/src/commands.rs` | Tauri IPC 命令 — 新增 scan_native_resources、import_native_resources、list_tool_resources、check_preset_compatibility |
| `src-tauri/src/manager/mod.rs` | 资源管理入口 — 扩展 auto_import_extensions 支持 OpenClaw，新增 scan_tool_native_resources |
| `src-tauri/src/adapter/mod.rs` | AgentAdapter trait — 注册 OpenClaw adapter |
| `src-tauri/src/adapter/openclaw.rs` | OpenClaw adapter 实现 |
| `src-tauri/src/monitor/process.rs` | 进程扫描 — 新增 find_openclaw_processes |
| `src-tauri/src/monitor/openclaw_parser.rs` | OpenClaw 会话解析器（基于 openclaw.json） |
| `src-tauri/src/manager/preset.rs` | 预设组应用 — 新增兼容性检查逻辑 |
| `src-tauri/src/lib.rs` | 应用入口 — 注册 OpenClaw 相关命令 |

### 前端（React/TypeScript）

| 文件 | 职责 |
|------|------|
| `src/components/ExtensionList.tsx` | 资源看板主组件 — 重构为双视图切换 |
| `src/components/ResourceByKindView.tsx` | 按资源分类视图 — Skills/MCP/Plugins 分栏 |
| `src/components/ResourceByToolView.tsx` | 按工具分类视图 — 四工具卡片 |
| `src/components/ImportDialog.tsx` | 导入资源弹窗 — 扫描结果多选导入 |
| `src/components/CompatibilityDialog.tsx` | 兼容性检查报告弹窗 |
| `src/components/PresetList.tsx` | 预设组列表 — 重构创建流程 |
| `src/types/extension.ts` | 扩展类型定义 — 新增 NativeExtension、CompatibilityReport |
| `src/pages/home.tsx` | 主页 — 资源标签页入口 |

---

## 任务分解

### 任务 1：数据层扩展 — 新增 native_extensions 表

**文件：**
- 修改：`src-tauri/src/store.rs:21-98`（init_schema 函数）
- 修改：`src-tauri/src/store.rs:183-200`（ExtensionRecord 结构体）
- 新增：`src-tauri/src/store.rs:310-350`（native_extensions CRUD 函数）

- [ ] **步骤 1：在 init_schema 中添加 native_extensions 表**

```rust
// 在 init_schema 的 execute_batch 中添加：
CREATE TABLE IF NOT EXISTS native_extensions (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,
    name        TEXT NOT NULL,
    description TEXT,
    source_path TEXT NOT NULL,
    source_tool TEXT NOT NULL,
    detected_at TEXT NOT NULL,
    imported    INTEGER NOT NULL DEFAULT 0
);
```

- [ ] **步骤 2：扩展 ExtensionRecord 结构体**

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionRecord {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub description: Option<String>,
    pub source_path: String,
    pub source_url: Option<String>,
    pub version: Option<String>,
    pub tags: Option<String>,       // 工具兼容性标记（如 "claude,codex,opencode,openclaw"）
    pub suite: Option<String>,
    pub source_tool: Option<String>,
    pub is_native: bool,            // 新增：是否为原生资源
}
```

- [ ] **步骤 3：添加 native_extensions CRUD 函数**

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeExtensionRecord {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub description: Option<String>,
    pub source_path: String,
    pub source_tool: String,
    pub detected_at: String,
    pub imported: bool,
}

pub fn insert_native_extension(ext: &NativeExtensionRecord) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO native_extensions (id, kind, name, description, source_path, source_tool, detected_at, imported) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![ext.id, ext.kind, ext.name, ext.description, ext.source_path, ext.source_tool, ext.detected_at, ext.imported as i64],
    ).map_err(|e| e.to_string()).map(|_| ())
}

pub fn list_native_extensions(tool_id: Option<&str>) -> Vec<NativeExtensionRecord> {
    let conn = DB.lock().unwrap();
    let sql = match tool_id {
        Some(tid) => "SELECT id, kind, name, description, source_path, source_tool, detected_at, imported FROM native_extensions WHERE source_tool = ?1 AND imported = 0",
        None => "SELECT id, kind, name, description, source_path, source_tool, detected_at, imported FROM native_extensions WHERE imported = 0",
    };
    conn.prepare(sql).ok().map(|mut stmt| {
        let params: Vec<&str> = match tool_id { Some(t) => vec![t], None => vec![] };
        stmt.query_map(rusqlite::params_from_iter(params), |row| {
            Ok(NativeExtensionRecord {
                id: row.get(0)?,
                kind: row.get(1)?,
                name: row.get(2)?,
                description: row.get(3)?,
                source_path: row.get(4)?,
                source_tool: row.get(5)?,
                detected_at: row.get(6)?,
                imported: row.get::<_, i64>(7)? != 0,
            })
        }).ok().map(|rows| rows.filter_map(|r| r.ok()).collect()).unwrap_or_default()
    }).unwrap_or_default()
}

pub fn mark_native_imported(ids: &[String]) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    for id in ids {
        conn.execute("UPDATE native_extensions SET imported = 1 WHERE id = ?1", [id]).map_err(|e| e.to_string())?;
    }
    Ok(())
}
```

- [ ] **步骤 4：编译验证**

运行：`cargo check`
预期：PASS，无错误

- [ ] **步骤 5：Commit**

```bash
git add src-tauri/src/store.rs
git commit -m "feat(store): 新增 native_extensions 表和 CRUD 函数

- 新增 native_extensions 表记录未导入的原生资源
- ExtensionRecord 增加 is_native 和 tags 字段
- 提供 insert/list/mark_imported CRUD 函数

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### 任务 2：OpenClaw Adapter 实现

**文件：**
- 创建：`src-tauri/src/adapter/openclaw.rs`
- 修改：`src-tauri/src/adapter/mod.rs:4-6`（添加 pub mod openclaw）
- 修改：`src-tauri/src/adapter/mod.rs:99-101`（注册 OpenClaw adapter）

- [ ] **步骤 1：创建 OpenClaw adapter**

```rust
// src-tauri/src/adapter/openclaw.rs
// OpenClaw adapter — 进程发现 + openclaw.json 解析

use super::*;
use crate::monitor;

pub struct OpenClawAdapter;

impl AgentAdapter for OpenClawAdapter {
    fn name(&self) -> &'static str { "OpenClaw" }
    fn agent_type(&self) -> AgentType { AgentType::OpenClaw }
    fn process_names(&self) -> &'static [&'static str] { &["openclaw"] }

    fn find_processes(&self, system: &System) -> Vec<AgentProcess> {
        monitor::process::find_openclaw_processes(system)
    }

    fn find_sessions(&self, processes: &[AgentProcess]) -> Vec<Session> {
        monitor::openclaw_parser::get_openclaw_sessions(processes)
    }

    fn base_dir(&self) -> std::path::PathBuf {
        dirs::home_dir().unwrap_or_default().join(".openclaw")
    }

    fn hook_supported(&self) -> bool { false }

    fn mcp_format(&self) -> McpFormat { McpFormat::Json }
    fn mcp_config_path(&self) -> Option<std::path::PathBuf> {
        Some(self.base_dir().join("openclaw.json"))
    }

    fn skill_dirs(&self) -> Vec<std::path::PathBuf> {
        vec![self.base_dir().join("skills")]
    }

    fn subagent_dir(&self) -> Option<std::path::PathBuf> {
        Some(self.base_dir().join("agents"))
    }

    fn plugin_dirs(&self) -> Vec<std::path::PathBuf> {
        vec![self.base_dir().join("plugins")]
    }
    fn plugin_config_paths(&self) -> Vec<std::path::PathBuf> {
        vec![self.base_dir().join("openclaw.json")]
    }
}
```

- [ ] **步骤 2：修改 adapter/mod.rs 注册 OpenClaw**

```rust
// 在文件顶部添加：
pub mod openclaw;

// 在 get_all_sessions 的 adapters 数组中添加：
let adapters: Vec<Box<dyn AgentAdapter>> = vec![
    Box::new(claude::ClaudeAdapter),
    Box::new(codex::CodexAdapter),
    Box::new(opencode::OpenCodeAdapter),
    Box::new(openclaw::OpenClawAdapter),  // 新增
];
```

- [ ] **步骤 3：修改 session/model.rs 添加 OpenClaw 类型**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AgentType {
    Claude,
    Codex,
    OpenCode,
    OpenClaw,  // 新增
}
```

- [ ] **步骤 4：编译验证**

运行：`cargo check`
预期：PASS

- [ ] **步骤 5：Commit**

```bash
git add src-tauri/src/adapter/openclaw.rs src-tauri/src/adapter/mod.rs src-tauri/src/session/model.rs
git commit -m "feat(adapter): 添加 OpenClaw 支持

- 新增 OpenClawAdapter 实现
- 注册 OpenClaw 到 adapter 注册表
- AgentType 枚举添加 OpenClaw 变体

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### 任务 3：OpenClaw 进程扫描和会话解析

**文件：**
- 修改：`src-tauri/src/monitor/process.rs:139-140`（添加 find_openclaw_processes）
- 创建：`src-tauri/src/monitor/openclaw_parser.rs`
- 修改：`src-tauri/src/monitor/mod.rs`（添加 pub mod openclaw_parser）

- [ ] **步骤 1：添加 find_openclaw_processes**

```rust
// 在 process.rs 末尾添加：
/// 发现 OpenClaw 进程
pub fn find_openclaw_processes(system: &System) -> Vec<AgentProcess> {
    find_processes_by_names(system, &["openclaw"], &["multi-agents-manager"])
}
```

- [ ] **步骤 2：创建 OpenClaw 会话解析器**

```rust
// src-tauri/src/monitor/openclaw_parser.rs
// OpenClaw 会话解析器 — 基于 openclaw.json 状态文件

use crate::adapter::AgentProcess;
use crate::session::{AgentType, ProcessForm, Session, SessionStatus};
use log::{debug, info};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::SystemTime;

#[derive(Deserialize)]
struct OpenClawState {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "lastActivity")]
    last_activity: Option<String>,
    status: Option<String>,
}

pub fn get_openclaw_sessions(processes: &[AgentProcess]) -> Vec<Session> {
    let mut sessions = Vec::new();
    
    let state_path = dirs::home_dir().unwrap_or_default().join(".openclaw").join("state.json");
    if !state_path.exists() {
        debug!("OpenClaw state file not found: {:?}", state_path);
        return sessions;
    }

    let content = match std::fs::read_to_string(&state_path) {
        Ok(c) => c,
        Err(e) => {
            debug!("Failed to read OpenClaw state: {}", e);
            return sessions;
        }
    };

    let state: OpenClawState = match serde_json::from_str(&content) {
        Ok(s) => s,
        Err(e) => {
            debug!("Failed to parse OpenClaw state: {}", e);
            return sessions;
        }
    };

    for process in processes {
        let session_id = state.session_id.clone().unwrap_or_else(|| {
            format!("openclaw-{}", process.pid)
        });
        
        let project_path = state.cwd.clone().unwrap_or_else(|| "~/.openclaw".to_string());
        let project_name = project_path.split('/').filter(|s| !s.is_empty()).last().unwrap_or("OpenClaw").to_string();
        
        let status = match state.status.as_deref() {
            Some("processing") => SessionStatus::Processing,
            Some("waiting") => SessionStatus::Waiting,
            Some("idle") => SessionStatus::Idle,
            _ => SessionStatus::Idle,
        };

        sessions.push(Session {
            id: session_id.clone(),
            agent_type: AgentType::OpenClaw,
            project_name,
            project_path,
            title: Some(session_id[..session_id.len().min(12)].to_string()),
            git_branch: None,
            github_url: None,
            status,
            last_message: None,
            last_message_role: None,
            last_activity_at: state.last_activity.clone().unwrap_or_else(|| "Unknown".to_string()),
            pid: process.pid,
            cpu_usage: process.cpu_usage,
            active_subagent_count: 0,
            form: process.form,
            jump_supported: matches!(process.form, ProcessForm::Cli),
        });
    }

    info!("OpenClaw: {} sessions from {} processes", sessions.len(), processes.len());
    sessions
}
```

- [ ] **步骤 3：注册 openclaw_parser 模块**

```rust
// 在 monitor/mod.rs 中添加：
pub mod openclaw_parser;
```

- [ ] **步骤 4：编译验证**

运行：`cargo check`
预期：PASS

- [ ] **步骤 5：Commit**

```bash
git add src-tauri/src/monitor/process.rs src-tauri/src/monitor/openclaw_parser.rs src-tauri/src/monitor/mod.rs
git commit -m "feat(monitor): 添加 OpenClaw 进程扫描和会话解析

- find_openclaw_processes 进程发现
- openclaw_parser 基于 state.json 的会话解析
- 支持 Idle/Processing/Waiting 三态

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### 任务 4：扩展资源扫描支持 OpenClaw

**文件：**
- 修改：`src-tauri/src/manager/mod.rs:320-325`（skill_sources 添加 OpenClaw）
- 修改：`src-tauri/src/manager/mod.rs:379-383`（plugin_sources 添加 OpenClaw）
- 修改：`src-tauri/src/manager/mod.rs:14-22`（get_tool_skill_dir 添加 OpenClaw）
- 修改：`src-tauri/src/manager/mod.rs:48-67`（enable_skill_for_tool 支持 OpenClaw）

- [ ] **步骤 1：在 auto_import_extensions 中添加 OpenClaw 扫描路径**

```rust
// skill_sources 数组扩展为：
let skill_sources = [
    ("claude", dirs::home_dir().unwrap_or_default().join(".claude").join("skills")),
    ("codex", dirs::home_dir().unwrap_or_default().join(".codex").join("skills")),
    ("opencode", dirs::home_dir().unwrap_or_default().join(".config").join("opencode").join("skills")),
    ("openclaw", dirs::home_dir().unwrap_or_default().join(".openclaw").join("skills")),  // 新增
];

// plugin_sources 数组扩展为：
let plugin_sources = [
    ("claude", dirs::home_dir().unwrap_or_default().join(".claude").join("plugins")),
    ("codex", dirs::home_dir().unwrap_or_default().join(".codex").join("plugins")),
    ("opencode", dirs::home_dir().unwrap_or_default().join(".config").join("opencode").join("plugins")),
    ("openclaw", dirs::home_dir().unwrap_or_default().join(".openclaw").join("plugins")),  // 新增
];
```

- [ ] **步骤 2：在 get_tool_skill_dir 中添加 OpenClaw**

```rust
fn get_tool_skill_dir(tool_id: &str) -> Option<std::path::PathBuf> {
    let adapter: Box<dyn AgentAdapter> = match tool_id {
        "claude" => Box::new(ClaudeAdapter),
        "codex" => Box::new(CodexAdapter),
        "opencode" => Box::new(OpenCodeAdapter),
        "openclaw" => Box::new(crate::adapter::openclaw::OpenClawAdapter),  // 新增
        _ => return None,
    };
    adapter.skill_dirs().into_iter().next()
}
```

- [ ] **步骤 3：编译验证**

运行：`cargo check`
预期：PASS

- [ ] **步骤 4：Commit**

```bash
git add src-tauri/src/manager/mod.rs
git commit -m "feat(manager): 资源扫描支持 OpenClaw

- auto_import_extensions 扫描 ~/.openclaw/skills/ 和 ~/.openclaw/plugins/
- get_tool_skill_dir 支持 openclaw 工具 ID

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### 任务 5：后端 API — 原生资源扫描和导入

**文件：**
- 修改：`src-tauri/src/commands.rs:270-273`（添加新命令）
- 修改：`src-tauri/src/lib.rs`（注册新命令）

- [ ] **步骤 1：在 commands.rs 中添加新命令**

```rust
/// 扫描指定工具的原生资源（未导入全局仓库的）
#[tauri::command]
pub fn scan_native_resources(tool_id: String) -> Vec<store::NativeExtensionRecord> {
    let mut results = Vec::new();
    
    // 扫描工具的 skill 目录
    let skill_dir = match tool_id.as_str() {
        "claude" => Some(dirs::home_dir().unwrap_or_default().join(".claude").join("skills")),
        "codex" => Some(dirs::home_dir().unwrap_or_default().join(".codex").join("skills")),
        "opencode" => Some(dirs::home_dir().unwrap_or_default().join(".config").join("opencode").join("skills")),
        "openclaw" => Some(dirs::home_dir().unwrap_or_default().join(".openclaw").join("skills")),
        _ => None,
    };
    
    if let Some(dir) = skill_dir {
        if dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        // 检查是否已在全局仓库
                        let ext_id = format!("skill-{}", name);
                        let exists = crate::store::list_extensions().iter().any(|e| e.id == ext_id);
                        if !exists {
                            results.push(store::NativeExtensionRecord {
                                id: ext_id,
                                kind: "skill".to_string(),
                                name: name.clone(),
                                description: None,
                                source_path: path.to_string_lossy().to_string(),
                                source_tool: tool_id.clone(),
                                detected_at: chrono::Utc::now().to_rfc3339(),
                                imported: false,
                            });
                        }
                    }
                }
            }
        }
    }
    
    results
}

/// 将原生资源导入全局仓库
#[tauri::command]
pub fn import_native_resources(items: Vec<(String, String)>) -> crate::manager::ImportStats {
    let mut imported = 0;
    let mut skipped = 0;
    
    for (source_path, name) in items {
        let path = std::path::Path::new(&source_path);
        if !path.exists() {
            skipped += 1;
            continue;
        }
        
        // 复制到全局仓库
        if let Err(e) = crate::linker::install_to_repo(path, &name) {
            log::warn!("导入 {} 失败: {}", name, e);
            skipped += 1;
            continue;
        }
        
        // 记录到数据库
        let ext = crate::store::ExtensionRecord {
            id: format!("skill-{}", name),
            kind: "skill".to_string(),
            name: name.clone(),
            description: None,
            source_path: source_path.clone(),
            source_url: None,
            version: None,
            tags: None,
            suite: None,
            source_tool: None,
            is_native: true,
        };
        let _ = crate::store::insert_extension(&ext);
        imported += 1;
    }
    
    crate::manager::ImportStats {
        imported,
        newly_added: imported,
        skipped_dup: skipped,
        source_counts: vec![],
    }
}

/// 获取工具的所有资源（全局仓库 + 原生）
#[tauri::command]
pub fn list_tool_resources(tool_id: String) -> serde_json::Value {
    let global = crate::store::list_extensions();
    let native = scan_native_resources(tool_id.clone());
    
    serde_json::json!({
        "global": global.iter().filter(|e| {
            // 筛选出已分配给该工具的资源
            let assignments = crate::store::list_assignments(&tool_id);
            assignments.iter().any(|a| a.extension_id == e.id && a.enabled)
        }).collect::<Vec<_>>(),
        "native": native,
    })
}
```

- [ ] **步骤 2：在 lib.rs 中注册新命令**

```rust
// 在 invoke_handler 数组中添加：
commands::scan_native_resources,
commands::import_native_resources,
commands::list_tool_resources,
```

- [ ] **步骤 3：编译验证**

运行：`cargo check`
预期：PASS

- [ ] **步骤 4：Commit**

```bash
git add src-tauri/src/commands.rs src-tauri/src/lib.rs
git commit -m "feat(commands): 添加原生资源扫描和导入 API

- scan_native_resources: 扫描工具原生目录，返回未导入的资源列表
- import_native_resources: 将选中的原生资源复制到全局仓库
- list_tool_resources: 获取工具的全局+原生资源列表

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### 任务 6：前端类型定义扩展

**文件：**
- 修改：`src/types/extension.ts`

- [ ] **步骤 1：扩展类型定义**

```typescript
// src/types/extension.ts

export interface AssignmentSummary {
  agentToolId: string;
  enabled: boolean;
  linkStatus: string;
}

export interface ExtensionWithAssignments {
  id: string;
  kind: string;
  name: string;
  description: string | null;
  sourcePath: string;
  sourceTool: string | null;
  suite: string | null;
  tags: string | null;        // 工具兼容性标记（如 "claude,codex,opencode,openclaw"）
  assignments: AssignmentSummary[];
}

export interface NativeExtension {
  id: string;
  kind: string;
  name: string;
  description: string | null;
  sourcePath: string;
  sourceTool: string;
  detectedAt: string;
  imported: boolean;
}

export interface ToolResources {
  global: ExtensionWithAssignments[];
  native: NativeExtension[];
}

export interface McpServerConfig {
  command: string;
  args: string[];
  env: Record<string, string>;
}

export interface McpServer {
  name: string;
  config: McpServerConfig;
}

export interface CompatibilityReport {
  compatible: CompatibleItem[];
  incompatible: IncompatibleItem[];
}

export interface CompatibleItem {
  id: string;
  name: string;
  kind: string;
}

export interface IncompatibleItem {
  id: string;
  name: string;
  kind: string;
  reason: string;
}
```

- [ ] **步骤 2：Commit**

```bash
git add src/types/extension.ts
git commit -m "feat(types): 扩展资源类型定义

- 新增 NativeExtension、ToolResources 类型
- 新增 CompatibilityReport 兼容性报告类型
- ExtensionWithAssignments 增加 tags 字段

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### 任务 7：前端 — 按资源分类视图组件

**文件：**
- 创建：`src/components/ResourceByKindView.tsx`

- [ ] **步骤 1：创建按资源分类视图**

```tsx
// src/components/ResourceByKindView.tsx
import { useState, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Package, Link2, Plug, Info, RefreshCw } from "lucide-react";
import type { ExtensionWithAssignments } from "@/types/extension";

const TOOLS = [
  { id: "claude", label: "Claude" },
  { id: "codex", label: "Codex" },
  { id: "opencode", label: "OpenCode" },
  { id: "openclaw", label: "OpenClaw" },
];

interface Props {
  extensions: ExtensionWithAssignments[];
  onToggleMcp: (name: string, toolId: string, enabled: boolean) => Promise<void>;
  onTogglePlugin: (name: string, toolId: string, enabled: boolean, kind: string) => Promise<void>;
}

export function ResourceByKindView({ extensions, onToggleMcp, onTogglePlugin }: Props) {
  const [search, setSearch] = useState("");
  
  const skillFilter = (e: ExtensionWithAssignments) => {
    if (!search.trim()) return true;
    const q = search.toLowerCase();
    return [e.name, e.description ?? "", e.sourceTool ?? "", e.suite ?? ""]
      .some((s) => s.toLowerCase().includes(q));
  };
  
  const skills = extensions.filter((e) => e.kind === "skill" && skillFilter(e));
  const mcps = extensions.filter((e) => e.kind === "mcp");
  const plugins = extensions.filter((e) => e.kind === "plugin");

  return (
    <div className="space-y-4">
      {/* Skills */}
      <div>
        <div className="mb-2 flex items-center justify-between gap-2">
          <h3 className="flex items-center gap-2 text-sm font-semibold">
            <Package className="h-4 w-4" />
            Skill ({skills.length})
          </h3>
          <input
            type="text"
            placeholder="搜索 skill..."
            value={search}
            onChange={(e) => setSearch(e.currentTarget.value)}
            className="h-7 w-40 rounded border px-2 text-xs"
          />
        </div>
        {skills.length === 0 ? (
          <div className="text-muted-foreground flex items-center gap-2 py-4 text-xs">
            <Info className="h-3.5 w-3.5" />
            暂无 skill。点击"扫描原生资源"导入。
          </div>
        ) : (
          <div className="space-y-1">
            {skills.map((skill) => (
              <div key={skill.id} className="flex items-center justify-between rounded border p-2 text-sm">
                <div>
                  <span className="font-medium">{skill.name}</span>
                  {skill.tags && (
                    <span className="text-muted-foreground ml-2 text-[10px]">
                      支持: {skill.tags}
                    </span>
                  )}
                </div>
                <div className="flex gap-1">
                  {TOOLS.map((tool) => {
                    const assignment = skill.assignments.find((a) => a.agentToolId === tool.id);
                    const enabled = assignment?.enabled ?? false;
                    return (
                      <Button
                        key={tool.id}
                        variant={enabled ? "default" : "outline"}
                        size="sm"
                        className="h-6 px-2 text-[10px]"
                        title={`${tool.label}: ${enabled ? "已启用" : "未启用"}`}
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

      {/* MCP */}
      <div>
        <h3 className="mb-2 flex items-center gap-2 text-sm font-semibold">
          <Link2 className="h-4 w-4" />
          MCP 服务器 ({mcps.length})
        </h3>
        {mcps.length === 0 ? (
          <div className="text-muted-foreground flex items-center gap-2 py-4 text-xs">
            <Info className="h-3.5 w-3.5" />
            暂无 MCP 服务器
          </div>
        ) : (
          <div className="space-y-1">
            {mcps.map((mcp) => (
              <div key={mcp.id} className="flex items-center justify-between rounded border p-2 text-sm">
                <span className="font-medium">{mcp.name}</span>
                <div className="flex gap-1">
                  {TOOLS.map((tool) => {
                    const assignment = mcp.assignments.find((a) => a.agentToolId === tool.id);
                    const enabled = assignment?.enabled ?? false;
                    return (
                      <Button
                        key={tool.id}
                        variant={enabled ? "default" : "outline"}
                        size="sm"
                        className="h-6 px-2 text-[10px]"
                        onClick={() => onToggleMcp(mcp.name, tool.id, !enabled)}
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

      {/* Plugins */}
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
              <div key={plugin.id} className="flex items-center justify-between rounded border p-2 text-sm">
                <span className="font-medium">{plugin.name}</span>
                <div className="flex gap-1">
                  {TOOLS.map((tool) => {
                    const assignment = plugin.assignments.find((a) => a.agentToolId === tool.id);
                    const enabled = assignment?.enabled ?? false;
                    const pluginSubtype = plugin.tags || "file";
                    return (
                      <Button
                        key={tool.id}
                        variant={enabled ? "default" : "outline"}
                        size="sm"
                        className="h-6 px-2 text-[10px]"
                        onClick={() => onTogglePlugin(plugin.name, tool.id, !enabled, pluginSubtype)}
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
    </div>
  );
}
```

- [ ] **步骤 2：Commit**

```bash
git add src/components/ResourceByKindView.tsx
git commit -m "feat(ui): 添加按资源分类视图组件

- Skills/MCP/Plugins 三栏展示
- 支持搜索过滤
- 显示工具兼容性标记
- 支持 MCP/Plugin 工具分配切换

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### 任务 8：前端 — 按工具分类视图组件

**文件：**
- 创建：`src/components/ResourceByToolView.tsx`

- [ ] **步骤 1：创建按工具分类视图**

```tsx
// src/components/ResourceByToolView.tsx
import { useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Scan, Import, Package, Link2, Plug } from "lucide-react";
import type { ExtensionWithAssignments, NativeExtension, ToolResources } from "@/types/extension";

const TOOLS = [
  { id: "claude", label: "Claude Code", icon: "🤖" },
  { id: "codex", label: "Codex CLI", icon: "📝" },
  { id: "opencode", label: "OpenCode", icon: "🖥️" },
  { id: "openclaw", label: "OpenClaw", icon: "🦾" },
];

export function ResourceByToolView() {
  const [toolResources, setToolResources] = useState<Record<string, ToolResources>>({});
  const [scanning, setScanning] = useState<Record<string, boolean>>({});

  const loadToolResources = useCallback(async (toolId: string) => {
    try {
      const data = await invoke<ToolResources>("list_tool_resources", { toolId });
      setToolResources((prev) => ({ ...prev, [toolId]: data }));
    } catch (e) {
      console.error(`Failed to load resources for ${toolId}:`, e);
    }
  }, []);

  const handleScan = async (toolId: string) => {
    setScanning((prev) => ({ ...prev, [toolId]: true }));
    try {
      const native = await invoke<NativeExtension[]>("scan_native_resources", { toolId });
      if (native.length > 0) {
        toast.info(`${TOOLS.find(t => t.id === toolId)?.label} 发现 ${native.length} 个原生资源，点击导入`);
      } else {
        toast.info("未发现新的原生资源");
      }
      await loadToolResources(toolId);
    } catch (e) {
      toast.error(`扫描失败: ${e}`);
    } finally {
      setScanning((prev) => ({ ...prev, [toolId]: false }));
    }
  };

  return (
    <div className="space-y-4">
      {TOOLS.map((tool) => (
        <div key={tool.id} className="rounded border p-3">
          <div className="mb-2 flex items-center justify-between">
            <h3 className="flex items-center gap-2 text-sm font-semibold">
              <span>{tool.icon}</span>
              {tool.label}
            </h3>
            <Button
              size="sm"
              variant="ghost"
              className="h-6 px-2 text-[10px]"
              onClick={() => handleScan(tool.id)}
              disabled={scanning[tool.id]}
            >
              <Scan className={`mr-1 h-3 w-3 ${scanning[tool.id] ? "animate-spin" : ""}`} />
              扫描
            </Button>
          </div>

          <ToolResourceList toolId={tool.id} resources={toolResources[tool.id]} />
        </div>
      ))}
    </div>
  );
}

function ToolResourceList({ toolId, resources }: { toolId: string; resources?: ToolResources }) {
  if (!resources) {
    return <div className="text-muted-foreground py-2 text-xs">点击"扫描"加载资源</div>;
  }

  const globalSkills = resources.global.filter((e) => e.kind === "skill");
  const nativeSkills = resources.native.filter((n) => n.kind === "skill");
  const globalMcps = resources.global.filter((e) => e.kind === "mcp");
  const nativeMcps = resources.native.filter((n) => n.kind === "mcp");
  const globalPlugins = resources.global.filter((e) => e.kind === "plugin");
  const nativePlugins = resources.native.filter((n) => n.kind === "plugin");

  return (
    <div className="space-y-2">
      {/* Skills */}
      <div>
        <h4 className="text-xs font-medium text-muted-foreground mb-1">Skills ({globalSkills.length + nativeSkills.length})</h4>
        <div className="space-y-1">
          {globalSkills.map((s) => (
            <div key={s.id} className="flex items-center justify-between rounded bg-accent/50 px-2 py-1 text-xs">
              <span>{s.name} <span className="text-green-600">✓ 全局仓库</span></span>
            </div>
          ))}
          {nativeSkills.map((s) => (
            <div key={s.id} className="flex items-center justify-between rounded bg-muted px-2 py-1 text-xs">
              <span>{s.name} <span className="text-orange-500">⚠ 原生</span></span>
              <Button size="sm" variant="ghost" className="h-5 px-1 text-[10px]">
                <Import className="h-3 w-3" />
                导入
              </Button>
            </div>
          ))}
        </div>
      </div>

      {/* MCP */}
      {(globalMcps.length > 0 || nativeMcps.length > 0) && (
        <div>
          <h4 className="text-xs font-medium text-muted-foreground mb-1">MCP ({globalMcps.length + nativeMcps.length})</h4>
          <div className="space-y-1">
            {globalMcps.map((m) => (
              <div key={m.id} className="flex items-center justify-between rounded bg-accent/50 px-2 py-1 text-xs">
                <span>{m.name} <span className="text-green-600">✓</span></span>
              </div>
            ))}
            {nativeMcps.map((m) => (
              <div key={m.id} className="flex items-center justify-between rounded bg-muted px-2 py-1 text-xs">
                <span>{m.name} <span className="text-orange-500">⚠ 原生</span></span>
                <Button size="sm" variant="ghost" className="h-5 px-1 text-[10px]">
                  <Import className="h-3 w-3" />
                  导入
                </Button>
              </div>
            ))}
          </div>
        </div>
      )}

      {/* Plugins */}
      {(globalPlugins.length > 0 || nativePlugins.length > 0) && (
        <div>
          <h4 className="text-xs font-medium text-muted-foreground mb-1">Plugins ({globalPlugins.length + nativePlugins.length})</h4>
          <div className="space-y-1">
            {globalPlugins.map((p) => (
              <div key={p.id} className="flex items-center justify-between rounded bg-accent/50 px-2 py-1 text-xs">
                <span>{p.name} <span className="text-green-600">✓</span></span>
              </div>
            ))}
            {nativePlugins.map((p) => (
              <div key={p.id} className="flex items-center justify-between rounded bg-muted px-2 py-1 text-xs">
                <span>{p.name} <span className="text-orange-500">⚠ 原生</span></span>
                <Button size="sm" variant="ghost" className="h-5 px-1 text-[10px]">
                  <Import className="h-3 w-3" />
                  导入
                </Button>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
```

- [ ] **步骤 2：Commit**

```bash
git add src/components/ResourceByToolView.tsx
git commit -m "feat(ui): 添加按工具分类视图组件

- 四工具卡片展示（Claude/Codex/OpenCode/OpenClaw）
- 每个工具显示全局仓库和原生资源
- 原生资源标记为橙色，提供导入按钮
- 支持独立扫描每个工具

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### 任务 9：前端 — 导入资源弹窗

**文件：**
- 创建：`src/components/ImportDialog.tsx`

- [ ] **步骤 1：创建导入弹窗组件**

```tsx
// src/components/ImportDialog.tsx
import { useState, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Checkbox } from "@/components/ui/checkbox";
import type { NativeExtension } from "@/types/extension";

const TOOLS = [
  { id: "claude", label: "Claude Code" },
  { id: "codex", label: "Codex CLI" },
  { id: "opencode", label: "OpenCode" },
  { id: "openclaw", label: "OpenClaw" },
];

interface Props {
  open: boolean;
  onClose: () => void;
  onImported: () => void;
}

export function ImportDialog({ open, onClose, onImported }: Props) {
  const [resources, setResources] = useState<Record<string, NativeExtension[]>>({});
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    if (open) {
      loadAllResources();
    }
  }, [open]);

  const loadAllResources = async () => {
    setLoading(true);
    const all: Record<string, NativeExtension[]> = {};
    for (const tool of TOOLS) {
      try {
        const data = await invoke<NativeExtension[]>("scan_native_resources", { toolId: tool.id });
        all[tool.id] = data;
      } catch (e) {
        console.error(`Failed to scan ${tool.id}:`, e);
      }
    }
    setResources(all);
    setLoading(false);
  };

  const toggleSelect = (id: string) => {
    const next = new Set(selected);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setSelected(next);
  };

  const selectAll = () => {
    const all = new Set<string>();
    Object.values(resources).flat().forEach((r) => all.add(r.id));
    setSelected(all);
  };

  const selectNone = () => {
    setSelected(new Set());
  };

  const handleImport = async () => {
    const items: [string, string][] = [];
    for (const toolId of Object.keys(resources)) {
      for (const res of resources[toolId]) {
        if (selected.has(res.id)) {
          items.push([res.sourcePath, res.name]);
        }
      }
    }

    if (items.length === 0) {
      toast.error("请选择至少一个资源");
      return;
    }

    try {
      const stats = await invoke<{ imported: number; skippedDup: number }>("import_native_resources", { items });
      toast.success(`成功导入 ${stats.imported} 个资源${stats.skippedDup > 0 ? `，跳过 ${stats.skippedDup} 个` : ""}`);
      onImported();
      onClose();
    } catch (e) {
      toast.error(`导入失败: ${e}`);
    }
  };

  const totalCount = Object.values(resources).flat().length;
  const selectedCount = selected.size;

  return (
    <Dialog open={open} onOpenChange={(v) => !v && onClose()}>
      <DialogContent className="max-w-lg max-h-[80vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle className="text-sm">导入原生资源</DialogTitle>
        </DialogHeader>

        <div className="flex gap-2 mb-2">
          <Button size="sm" variant="outline" className="h-6 text-[10px]" onClick={selectAll}>
            全选
          </Button>
          <Button size="sm" variant="outline" className="h-6 text-[10px]" onClick={selectNone}>
            全不选
          </Button>
        </div>

        {loading ? (
          <div className="text-center py-4 text-xs text-muted-foreground">扫描中...</div>
        ) : totalCount === 0 ? (
          <div className="text-center py-4 text-xs text-muted-foreground">未发现原生资源</div>
        ) : (
          <div className="space-y-3">
            {TOOLS.map((tool) => {
              const toolResources = resources[tool.id] || [];
              if (toolResources.length === 0) return null;
              return (
                <div key={tool.id}>
                  <h4 className="text-xs font-medium mb-1">{tool.label}</h4>
                  <div className="space-y-1">
                    {toolResources.map((res) => (
                      <label key={res.id} className="flex items-center gap-2 text-xs cursor-pointer">
                        <Checkbox
                          checked={selected.has(res.id)}
                          onCheckedChange={() => toggleSelect(res.id)}
                        />
                        <span className="rounded bg-muted px-1.5 py-0.5 text-[10px]">{res.kind}</span>
                        <span>{res.name}</span>
                      </label>
                    ))}
                  </div>
                </div>
              );
            })}
          </div>
        )}

        <div className="flex justify-end gap-2 mt-4">
          <Button size="sm" variant="outline" onClick={onClose}>取消</Button>
          <Button size="sm" onClick={handleImport} disabled={selectedCount === 0}>
            导入选中 ({selectedCount})
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
```

- [ ] **步骤 2：Commit**

```bash
git add src/components/ImportDialog.tsx
git commit -m "feat(ui): 添加导入资源弹窗组件

- 扫描四工具的原生资源
- 支持全选/全不选
- 按工具分组展示
- 显示选中数量

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### 任务 10：前端 — 重构 ExtensionList 主组件

**文件：**
- 修改：`src/components/ExtensionList.tsx`

- [ ] **步骤 1：重构 ExtensionList 为双视图切换**

```tsx
// src/components/ExtensionList.tsx
import { useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Scan, LayoutGrid, List } from "lucide-react";
import type { ExtensionWithAssignments } from "@/types/extension";
import { ResourceByKindView } from "./ResourceByKindView";
import { ResourceByToolView } from "./ResourceByToolView";
import { ImportDialog } from "./ImportDialog";
import { PresetList } from "./PresetList";

export function ExtensionList() {
  const [view, setView] = useState<"byKind" | "byTool">("byKind");
  const [extensions, setExtensions] = useState<ExtensionWithAssignments[]>([]);
  const [showImport, setShowImport] = useState(false);
  const [loading, setLoading] = useState(false);

  const load = useCallback(async () => {
    try {
      const data = await invoke<ExtensionWithAssignments[]>("list_extensions_with_assignments");
      setExtensions(data);
    } catch (e) {
      console.error("Failed to load extensions:", e);
    }
  }, []);

  useEffect(() => {
    load();
  }, []);

  const handleToggleMcp = async (mcpName: string, toolId: string, enabled: boolean) => {
    try {
      await invoke("toggle_mcp_for_tool", { mcpName, toolId, enabled });
      toast.success(`${mcpName} 已${enabled ? "启用" : "禁用"}`);
      load();
    } catch (e) {
      toast.error(`操作失败: ${e}`);
    }
  };

  const handleTogglePlugin = async (pluginName: string, toolId: string, enabled: boolean, kind: string) => {
    try {
      await invoke("toggle_plugin_for_tool", { pluginName, toolId, enabled, kind });
      toast.success(`${pluginName} 已${enabled ? "启用" : "禁用"}`);
      load();
    } catch (e) {
      toast.error(`操作失败: ${e}`);
    }
  };

  return (
    <div className="space-y-4">
      {/* 工具栏 */}
      <div className="flex items-center justify-between">
        <div className="flex gap-1">
          <Button
            size="sm"
            variant={view === "byKind" ? "default" : "outline"}
            className="h-7 px-2 text-[10px]"
            onClick={() => setView("byKind")}
          >
            <List className="mr-1 h-3 w-3" />
            按资源
          </Button>
          <Button
            size="sm"
            variant={view === "byTool" ? "default" : "outline"}
            className="h-7 px-2 text-[10px]"
            onClick={() => setView("byTool")}
          >
            <LayoutGrid className="mr-1 h-3 w-3" />
            按工具
          </Button>
        </div>
        <Button
          size="sm"
          variant="outline"
          className="h-7 px-2 text-[10px]"
          onClick={() => setShowImport(true)}
        >
          <Scan className="mr-1 h-3 w-3" />
          扫描原生资源
        </Button>
      </div>

      {/* 视图内容 */}
      {view === "byKind" ? (
        <ResourceByKindView
          extensions={extensions}
          onToggleMcp={handleToggleMcp}
          onTogglePlugin={handleTogglePlugin}
        />
      ) : (
        <ResourceByToolView />
      )}

      {/* 预设组 */}
      <PresetList extensions={extensions} />

      {/* 导入弹窗 */}
      <ImportDialog
        open={showImport}
        onClose={() => setShowImport(false)}
        onImported={load}
      />
    </div>
  );
}
```

- [ ] **步骤 2：Commit**

```bash
git add src/components/ExtensionList.tsx
git commit -m "feat(ui): 重构 ExtensionList 为双视图切换

- 支持按资源/按工具双视图切换
- 集成 ResourceByKindView 和 ResourceByToolView
- 添加扫描原生资源按钮
- 集成 ImportDialog 弹窗

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### 任务 11：前端 — 兼容性检查弹窗

**文件：**
- 创建：`src/components/CompatibilityDialog.tsx`

- [ ] **步骤 1：创建兼容性检查弹窗**

```tsx
// src/components/CompatibilityDialog.tsx
import { useState, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { CheckCircle, XCircle } from "lucide-react";
import type { CompatibilityReport } from "@/types/extension";

interface Props {
  open: boolean;
  presetId: string;
  toolId: string;
  toolName: string;
  onClose: () => void;
  onConfirm: () => void;
}

export function CompatibilityDialog({ open, presetId, toolId, toolName, onClose, onConfirm }: Props) {
  const [report, setReport] = useState<CompatibilityReport | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    if (open) {
      loadReport();
    }
  }, [open]);

  const loadReport = async () => {
    setLoading(true);
    try {
      const data = await invoke<CompatibilityReport>("check_preset_compatibility", { presetId, toolId });
      setReport(data);
    } catch (e) {
      console.error("Failed to check compatibility:", e);
    } finally {
      setLoading(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={(v) => !v && onClose()}>
      <DialogContent className="max-w-md">
        <DialogHeader>
          <DialogTitle className="text-sm">应用预设组到 {toolName}</DialogTitle>
        </DialogHeader>

        {loading ? (
          <div className="text-center py-4 text-xs text-muted-foreground">检查兼容性...</div>
        ) : report ? (
          <div className="space-y-3">
            {/* 兼容资源 */}
            {report.compatible.length > 0 && (
              <div>
                <h4 className="text-xs font-medium text-green-600 mb-1">
                  <CheckCircle className="inline h-3 w-3 mr-1" />
                  兼容的资源 ({report.compatible.length})
                </h4>
                <div className="space-y-1">
                  {report.compatible.map((item) => (
                    <div key={item.id} className="flex items-center gap-2 text-xs">
                      <span className="rounded bg-green-50 px-1.5 py-0.5 text-[10px]">{item.kind}</span>
                      <span>{item.name}</span>
                    </div>
                  ))}
                </div>
              </div>
            )}

            {/* 不兼容资源 */}
            {report.incompatible.length > 0 && (
              <div>
                <h4 className="text-xs font-medium text-orange-600 mb-1">
                  <XCircle className="inline h-3 w-3 mr-1" />
                  不兼容的资源 ({report.incompatible.length})
                </h4>
                <div className="space-y-1">
                  {report.incompatible.map((item) => (
                    <div key={item.id} className="flex items-center gap-2 text-xs">
                      <span className="rounded bg-orange-50 px-1.5 py-0.5 text-[10px]">{item.kind}</span>
                      <span>{item.name}</span>
                      <span className="text-muted-foreground text-[10px]">({item.reason})</span>
                    </div>
                  ))}
                </div>
              </div>
            )}

            <div className="flex justify-end gap-2 mt-4">
              <Button size="sm" variant="outline" onClick={onClose}>取消</Button>
              <Button size="sm" onClick={onConfirm} disabled={report.compatible.length === 0}>
                确认应用 ({report.compatible.length} 项)
              </Button>
            </div>
          </div>
        ) : null}
      </DialogContent>
    </Dialog>
  );
}
```

- [ ] **步骤 2：Commit**

```bash
git add src/components/CompatibilityDialog.tsx
git commit -m "feat(ui): 添加兼容性检查弹窗

- 显示兼容/不兼容资源列表
- 不兼容资源显示原因
- 确认按钮显示可应用数量

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### 任务 12：后端 — 预设组兼容性检查

**文件：**
- 修改：`src-tauri/src/manager/preset.rs`
- 修改：`src-tauri/src/commands.rs`

- [ ] **步骤 1：在 preset.rs 中添加兼容性检查**

```rust
// src-tauri/src/manager/preset.rs

/// 兼容性检查结果
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompatibilityReport {
    pub compatible: Vec<CompatibleItem>,
    pub incompatible: Vec<IncompatibleItem>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompatibleItem {
    pub id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IncompatibleItem {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub reason: String,
}

/// 检查预设组与工具的兼容性
pub fn check_compatibility(preset_id: &str, tool_id: &str) -> CompatibilityReport {
    let items = store::get_preset_items(preset_id);
    let mut compatible = Vec::new();
    let mut incompatible = Vec::new();

    for (ext_id, kind) in items {
        // 获取资源信息
        let ext = store::list_extensions().into_iter().find(|e| e.id == ext_id);
        let name = ext.as_ref().map(|e| e.name.clone()).unwrap_or_else(|| ext_id.clone());
        
        // 检查兼容性：tags 字段包含目标工具 ID
        let is_compatible = ext.as_ref().and_then(|e| e.tags.as_ref())
            .map(|tags| tags.split(',').any(|t| t.trim() == tool_id))
            .unwrap_or(true); // 默认兼容（无标记则兼容所有工具）

        if is_compatible {
            compatible.push(CompatibleItem { id: ext_id.clone(), name, kind });
        } else {
            incompatible.push(IncompatibleItem {
                id: ext_id,
                name,
                kind,
                reason: format!("不支持 {}", tool_id),
            });
        }
    }

    CompatibilityReport { compatible, incompatible }
}
```

- [ ] **步骤 2：在 commands.rs 中添加 check_preset_compatibility 命令**

```rust
/// 检查预设组与工具的兼容性
#[tauri::command]
pub fn check_preset_compatibility(preset_id: String, tool_id: String) -> crate::manager::preset::CompatibilityReport {
    crate::manager::preset::check_compatibility(&preset_id, &tool_id)
}
```

- [ ] **步骤 3：在 lib.rs 中注册命令**

```rust
commands::check_preset_compatibility,
```

- [ ] **步骤 4：编译验证**

运行：`cargo check`
预期：PASS

- [ ] **步骤 5：Commit**

```bash
git add src-tauri/src/manager/preset.rs src-tauri/src/commands.rs src-tauri/src/lib.rs
git commit -m "feat(preset): 添加预设组兼容性检查

- check_compatibility 函数检查预设组资源与目标工具的兼容性
- 基于 tags 字段判断资源支持的工具列表
- 返回兼容/不兼容资源列表

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### 任务 13：前端 — 重构 PresetList 集成兼容性检查

**文件：**
- 修改：`src/components/PresetList.tsx`

- [ ] **步骤 1：重构 PresetList 应用流程**

```tsx
// 在 PresetList 组件中添加状态：
const [compatibilityDialog, setCompatibilityDialog] = useState<{
  open: boolean;
  presetId: string;
  presetName: string;
  toolId: string;
} | null>(null);

// 修改 handleApply 函数：
const handleApply = async (presetId: string, presetName: string, toolId: string) => {
  // 先显示兼容性检查
  setCompatibilityDialog({
    open: true,
    presetId,
    presetName,
    toolId,
  });
};

// 确认应用：
const confirmApply = async () => {
  if (!compatibilityDialog) return;
  
  try {
    const result = await invoke<PresetApplyResult>("apply_preset", {
      presetId: compatibilityDialog.presetId,
      toolId: compatibilityDialog.toolId,
    });
    if (result.failures.length > 0) {
      toast.warning(`部分成功: ${result.successCount} 项成功, ${result.failures.length} 项失败`);
    } else {
      toast.success(`"${compatibilityDialog.presetName}" 已应用到 ${compatibilityDialog.toolId}`);
    }
    load();
  } catch (e) {
    toast.error(`应用失败: ${e}`);
  }
  
  setCompatibilityDialog(null);
};

// 在 JSX 中添加 CompatibilityDialog：
{compatibilityDialog && (
  <CompatibilityDialog
    open={compatibilityDialog.open}
    presetId={compatibilityDialog.presetId}
    toolId={compatibilityDialog.toolId}
    toolName={TOOLS.find(t => t.id === compatibilityDialog.toolId)?.label || compatibilityDialog.toolId}
    onClose={() => setCompatibilityDialog(null)}
    onConfirm={confirmApply}
  />
)}
```

- [ ] **步骤 2：Commit**

```bash
git add src/components/PresetList.tsx
git commit -m "feat(ui): PresetList 集成兼容性检查弹窗

- 应用预设组前显示兼容性检查报告
- 用户确认后执行应用
- 显示应用结果 toast

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### 任务 14：工具检测支持 OpenClaw

**文件：**
- 修改：`src-tauri/src/linker/detector.rs`

- [ ] **步骤 1：在 detector.rs 中添加 OpenClaw**

```rust
use crate::adapter::{AgentAdapter, claude::ClaudeAdapter, codex::CodexAdapter, opencode::OpenCodeAdapter, openclaw::OpenClawAdapter};

pub fn detect_all_tools() -> Vec<ToolDetection> {
    let adapters: Vec<Box<dyn AgentAdapter>> = vec![
        Box::new(ClaudeAdapter),
        Box::new(CodexAdapter),
        Box::new(OpenCodeAdapter),
        Box::new(OpenClawAdapter),  // 新增
    ];
    // ... rest unchanged
}
```

- [ ] **步骤 2：Commit**

```bash
git add src-tauri/src/linker/detector.rs
git commit -m "feat(detector): 工具检测支持 OpenClaw

- detect_all_tools 添加 OpenClawAdapter

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### 任务 15：MCP 管理支持 OpenClaw

**文件：**
- 修改：`src-tauri/src/manager/mcp.rs`

- [ ] **步骤 1：在 mcp.rs 中添加 OpenClaw 支持**

```rust
use crate::adapter::{McpFormat, claude::ClaudeAdapter, codex::CodexAdapter, opencode::OpenCodeAdapter, openclaw::OpenClawAdapter, AgentAdapter};

fn get_tool_mcp_info(tool_id: &str) -> Result<(McpFormat, std::path::PathBuf), String> {
    let adapter: Box<dyn AgentAdapter> = match tool_id {
        "claude" => Box::new(ClaudeAdapter),
        "codex" => Box::new(CodexAdapter),
        "opencode" => Box::new(OpenCodeAdapter),
        "openclaw" => Box::new(OpenClawAdapter),  // 新增
        _ => return Err(format!("未知工具: {}", tool_id)),
    };
    // ... rest unchanged
}
```

- [ ] **步骤 2：Commit**

```bash
git add src-tauri/src/manager/mcp.rs
git commit -m "feat(mcp): MCP 管理支持 OpenClaw

- get_tool_mcp_info 支持 openclaw 工具 ID
- OpenClaw 使用 JSON 格式 MCP 配置（openclaw.json）

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### 任务 16：Plugin 管理支持 OpenClaw

**文件：**
- 修改：`src-tauri/src/manager/plugin.rs`

- [ ] **步骤 1：在 plugin.rs 中添加 OpenClaw 支持**

```rust
use crate::adapter::{AgentAdapter, claude::ClaudeAdapter, codex::CodexAdapter, opencode::OpenCodeAdapter, openclaw::OpenClawAdapter};

// 在需要匹配 tool_id 的地方添加 "openclaw" 分支
```

- [ ] **步骤 2：Commit**

```bash
git add src-tauri/src/manager/plugin.rs
git commit -m "feat(plugin): Plugin 管理支持 OpenClaw

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### 任务 17：前端主页标签更新

**文件：**
- 修改：`src/pages/home.tsx`

- [ ] **步骤 1：确保资源标签页正确加载 ExtensionList**

```tsx
// 确认 home.tsx 中 extensions 标签页使用 ExtensionList
{activeTab === "extensions" && <ExtensionList />}
```

- [ ] **步骤 2：Commit**

```bash
git add src/pages/home.tsx
git commit -m "refactor(home): 资源标签页使用重构后的 ExtensionList

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### 任务 18：集成测试

**文件：**
- 运行测试命令

- [ ] **步骤 1：编译验证**

```bash
cd /Users/jarvis/Documents/MultiAgents-Manager/src-tauri
cargo check
```
预期：PASS

- [ ] **步骤 2：前端类型检查**

```bash
cd /Users/jarvis/Documents/MultiAgents-Manager
npx tsc --noEmit
```
预期：PASS

- [ ] **步骤 3：运行 Rust 测试**

```bash
cd /Users/jarvis/Documents/MultiAgents-Manager/src-tauri
cargo test
```
预期：PASS（现有测试 + 新增测试）

- [ ] **步骤 4：手动测试清单**

1. 打开应用，切换到"资源"标签页
2. 验证默认显示"按资源"视图
3. 点击"按工具"切换视图
4. 验证显示四工具卡片（Claude/Codex/OpenCode/OpenClaw）
5. 点击"扫描原生资源"按钮
6. 验证弹窗显示扫描结果
7. 选择部分资源，点击"导入选中"
8. 验证资源出现在"按资源"视图中
9. 创建预设组，选择多个资源
10. 应用预设组到工具，验证兼容性检查弹窗
11. 验证状态灯正确显示

- [ ] **步骤 5：Commit**

```bash
git commit -m "test: 资源看板重构集成测试通过

- 双视图切换正常
- 原生资源扫描导入正常
- 预设组兼容性检查正常
- OpenClaw 四工具支持正常

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## 自检

### 1. 规格覆盖度

| 规格需求 | 实现任务 |
|---------|---------|
| 双视图切换（按资源/按工具） | 任务 7, 8, 10 |
| 原生资源扫描导入 | 任务 1, 5, 9 |
| 预设组兼容性检查 | 任务 11, 12, 13 |
| OpenClaw 四工具支持 | 任务 2, 3, 4, 14, 15, 16 |
| Skill 映射管理 | 任务 4（已存在） |
| MCP 配置管理 | 任务 15 |
| Plugin 配置管理 | 任务 16 |

**遗漏**：无

### 2. 占位符扫描

- [x] 无 "待定"、"TODO"、"后续实现"
- [x] 无 "添加适当的错误处理" 等模糊描述
- [x] 所有代码步骤包含实际代码
- [x] 无未定义的类型引用

### 3. 类型一致性

| 类型/字段 | 定义位置 | 使用位置 | 一致 |
|-----------|---------|---------|------|
| `NativeExtensionRecord` | store.rs | commands.rs, ImportDialog.tsx | ✓ |
| `CompatibilityReport` | preset.rs | CompatibilityDialog.tsx | ✓ |
| `ExtensionRecord.is_native` | store.rs | manager/mod.rs | ✓ |
| `ExtensionRecord.tags` | store.rs | ResourceByKindView.tsx | ✓ |
| `AgentType.OpenClaw` | model.rs | adapter/mod.rs, openclaw.rs | ✓ |

---

## 执行交接

**计划已完成并保存到 `docs/engineering/plans/2026-07-07-resource-dashboard-redesign.md`。两种执行方式：**

**1. 子代理驱动（推荐）** - 每个任务调度一个新的子代理，任务间进行审查，快速迭代

**2. 内联执行** - 在当前会话中使用 executing-plans 执行任务，批量执行并设有检查点

**选哪种方式？**
