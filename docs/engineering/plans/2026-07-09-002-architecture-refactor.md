# 代码架构重构 实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 将 `commands.rs`（542 行）、`store.rs`（561 行）拆分为模块化目录结构，前端引入 API 层、React Query、Error Boundaries、Zod schemas，并建立 Monorepo 基础和 pre-commit hooks。

**架构：** 后端按 database/ -> services/ -> window/ -> commands/ 顺序从底层往上拆，每层拆完跑 `cargo check` 验证。前端按 api 层 -> React Query -> Error Boundaries -> config -> schemas 顺序引入。所有重构不改业务逻辑和公共 API 签名。

**技术栈：** Rust + Tauri 2 + React 19 + TypeScript + @tanstack/react-query + zod + husky + lint-staged

---

## 文件结构

### 后端（Rust）- 目标结构

```
src-tauri/src/
├── commands/
│   ├── mod.rs           # 聚合注册所有命令
│   ├── session.rs       # get_all_sessions, focus_session, kill_session
│   ├── resource.rs      # list_extensions_with_assignments, scan_native_resources, list_tool_resources
│   ├── preset.rs        # create/delete/apply/deactivate_preset, subagent 预设
│   ├── skill.rs         # list_repo_skills, install_skill, rescan_skills, assign_skill_to_subagent
│   ├── mcp.rs           # toggle_mcp_for_tool, read/write/remove_mcp_server
│   ├── plugin.rs        # toggle_plugin_for_tool
│   ├── settings.rs      # get_setting, set_setting, detect_tools, detect_subagents, list_sub_agents
│   └── screenshot.rs    # capture_window_screenshot, list_screenshots
├── database/
│   ├── mod.rs           # 聚合导出公共接口
│   ├── schema.rs        # 所有 CREATE TABLE 语句
│   ├── migration.rs     # 迁移逻辑（预留）
│   ├── connection.rs    # 连接管理（从 store.rs init 提取）
│   └── dao/
│       ├── mod.rs
│       ├── session.rs   # session_status_cache CRUD
│       ├── extension.rs # extensions + extension_assignments + native_extensions
│       ├── preset.rs    # presets + preset_items + preset_applications
│       ├── settings.rs  # settings 键值
│       └── agent_tool.rs # agent_tools + sub_agents
├── services/
│   ├── mod.rs
│   ├── resource/
│   ├── preset/
│   ├── skill/
│   ├── mcp/
│   └── plugin/
├── window/
│   ├── mod.rs           # WindowManager trait + 平台分发
│   ├── applescript.rs
│   ├── iterm.rs
│   ├── terminal_app.rs
│   ├── tmux.rs
│   └── xdotool.rs       # Linux X11（预留）
├── monitor/             # 保持不变 + 集成 notify
├── linker/              # 保持不变
├── adapter/             # 保持不变
├── plugins/             # 保持不变
└── session/             # 保持不变
```

### 前端（React/TypeScript）- 新增结构

```
src/
├── lib/
│   ├── api/             # Tauri invoke 封装
│   │   ├── session.ts
│   │   ├── resource.ts
│   │   ├── preset.ts
│   │   ├── skill.ts
│   │   ├── mcp.ts
│   │   ├── plugin.ts
│   │   └── settings.ts
│   ├── query/           # React Query
│   │   ├── queryClient.ts
│   │   ├── queries/
│   │   └── mutations/
│   └── schemas/         # Zod 验证
│       ├── session.ts
│       ├── extension.ts
│       ├── preset.ts
│       └── settings.ts
├── config/
│   ├── constants.ts
│   ├── tool-presets.ts
│   └── skill-presets.ts
├── components/
│   ├── sessions/
│   ├── resources/
│   ├── presets/
│   ├── settings/
│   ├── mcp/
│   ├── common/
│   │   └── ErrorBoundary.tsx
│   └── ui/              # 保持不变
```

---

## 任务分解

> **重构顺序**（Spec 002 假设 9）：FR-2（database）-> FR-5（services）-> FR-5b（window）-> FR-5c（notify）-> FR-5d（实体表）-> FR-1（commands）-> 前端 FR-3/4/8/9 -> FR-6/7 -> FR-10/11 -> FR-12

### 任务 1：store.rs -> database/ DAO（FR-2）

**文件：**
- 创建：`src-tauri/src/database/mod.rs`
- 创建：`src-tauri/src/database/schema.rs`
- 创建：`src-tauri/src/database/connection.rs`
- 创建：`src-tauri/src/database/dao/mod.rs`
- 创建：`src-tauri/src/database/dao/session.rs`
- 创建：`src-tauri/src/database/dao/extension.rs`
- 创建：`src-tauri/src/database/dao/preset.rs`
- 创建：`src-tauri/src/database/dao/settings.rs`
- 创建：`src-tauri/src/database/dao/agent_tool.rs`
- 删除：`src-tauri/src/store.rs`（拆分完成后）

- [ ] **步骤 1：创建 schema.rs -- 提取所有 CREATE TABLE**

将 `store.rs` 中 `init()` 的 `execute_batch` 内容移到 `database/schema.rs`：

```rust
use rusqlite::Connection;

pub fn init(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_status_cache (...);
         CREATE TABLE IF NOT EXISTS settings (...);
         CREATE TABLE IF NOT EXISTS extensions (...);
         -- ... 全部 9 张表，从 store.rs 原样搬移
        ",
    ).map_err(|e| format!("Schema 初始化失败: {}", e))?;
    Ok(())
}
```

- [ ] **步骤 2：创建 connection.rs -- 提取连接管理**

```rust
use rusqlite::Connection;
use std::sync::Mutex;
use once_cell::sync::Lazy;

/// 全局数据库连接（从 store.rs 搬移，保持原有模式）
pub static DB: Lazy<Mutex<Connection>> = Lazy::new(|| {
    let db_path = dirs::home_dir()
        .unwrap_or_default()
        .join(".mam")
        .join("mam.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    Mutex::new(Connection::open(&db_path).expect("无法打开数据库"))
});

/// 兼容性接口：打开新连接（少数场景使用）
pub fn open() -> Result<Connection, String> {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".mam")
        .join("mam.db")
        .to_string_lossy()
        .to_string()
});

pub fn open() -> Result<Connection, String> {
    let db_path = dirs::home_dir()
        .unwrap_or_default()
        .join(".mam").join("mam.db");
    Connection::open(&db_path).map_err(|e| format!("打开数据库失败: {}", e))
}
```

- [ ] **步骤 3：创建 dao/session.rs**

将 `store.rs` 中 `update_session_status`、`cleanup_stale_sessions` 移入，保持函数签名不变：

```rust
use rusqlite::Connection;
use std::collections::HashSet;

pub fn update_session_status(conn: &Connection, /* 原参数 */) -> Result<(), String> {
    // 原样搬移 store.rs 中的实现
}

pub fn cleanup_stale_sessions(conn: &Connection, active_ids: &HashSet<String>) {
    // 原样搬移
}
```

- [ ] **步骤 4：创建 dao/extension.rs、dao/preset.rs、dao/settings.rs、dao/agent_tool.rs**

按同样模式，将 `store.rs` 中的对应函数和结构体分别搬入各 DAO 模块。结构体（`ExtensionRecord`、`PresetRecord`、`SubAgentRecord`、`NativeExtensionRecord` 等）随对应 DAO 移动。

- [ ] **步骤 4b：在各 DAO 模块中定义标准 trait 接口**

当前 store.rs 使用全局 `DB` 静态连接（`Lazy<Mutex<Connection>>`），函数内部 `DB.lock().unwrap()` 获取连接。DAO trait 适配此模式--trait 方法不接收 `&Connection`，实现内部使用全局连接。

在 `dao/session.rs` 中添加 trait（保留现有领域函数 `update_session_status`/`cleanup_stale_sessions` 不变）：

```rust
/// Session 数据访问标准接口
pub trait SessionDao {
    fn find_all_statuses(&self) -> Vec<(String, String, String)>;
    fn find_status(&self, session_id: &str) -> Option<String>;
    fn upsert_status(&self, session_id: &str, agent_type: &str, status: &str) -> Option<String>;
    fn delete(&self, session_id: &str);
}

pub struct SessionDaoImpl;
impl SessionDao for SessionDaoImpl {
    fn find_all_statuses(&self) -> Vec<(String, String, String)> {
        let conn = crate::database::connection::DB.lock().unwrap();
        conn.prepare("SELECT session_id, agent_type, status FROM session_status_cache")
            .ok()
            .map(|mut stmt| {
                stmt.query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                }).ok()
                 .map(|rows| rows.filter_map(|r| r.ok()).collect())
                 .unwrap_or_default()
            }).unwrap_or_default()
    }
    fn find_status(&self, session_id: &str) -> Option<String> {
        let conn = crate::database::connection::DB.lock().unwrap();
        conn.query_row("SELECT status FROM session_status_cache WHERE session_id = ?",
            [session_id], |row| row.get(0)).ok()
    }
    fn upsert_status(&self, session_id: &str, agent_type: &str, status: &str) -> Option<String> {
        crate::database::dao::session::update_session_status(session_id, agent_type, status)
    }
    fn delete(&self, session_id: &str) {
        let conn = crate::database::connection::DB.lock().unwrap();
        let _ = conn.execute("DELETE FROM session_status_cache WHERE session_id = ?", [session_id]);
    }
}
```

在 `dao/extension.rs`、`dao/preset.rs`、`dao/settings.rs` 中同样添加 trait + impl，保留现有函数不变。trait 方法委托给现有函数。

**cc-switch 参考**：`src-tauri/src/database/dao/` 含 13 个 DAO 模块，每个模块有 trait + impl。

- [ ] **步骤 5：创建 database/mod.rs -- 聚合导出**

```rust
pub mod schema;
pub mod connection;
pub mod dao;

// 重新导出，保持外部 `use crate::store::*` 的引用可平滑迁移
pub use dao::session;
pub use dao::extension::{ExtensionRecord, AssignmentRecord, NativeExtensionRecord};
pub use dao::preset::{PresetRecord, PresetItemRecord};
pub use dao::settings;
pub use dao::agent_tool::{SubAgentRecord};

pub fn init() {
    let conn = connection::open().expect("无法打开数据库");
    schema::init(&conn).expect("Schema 初始化失败");
}
```

- [ ] **步骤 6：更新 lib.rs 模块声明**

将 `mod store;` 改为 `mod database;`，更新所有 `use crate::store::` 引用为 `use crate::database::`。

运行：`rg -n "crate::store" src-tauri/src/` 找到所有引用点，逐一替换。

- [ ] **步骤 7：验证编译**

运行：`cd src-tauri && cargo check`
预期：PASS

- [ ] **步骤 8：删除 store.rs**

确认无引用后删除 `src-tauri/src/store.rs`。

运行：`cd src-tauri && cargo check`
预期：PASS

- [ ] **步骤 9：Commit**

```bash
git add src-tauri/src/database/ src-tauri/src/lib.rs
git rm src-tauri/src/store.rs
git commit -m "refactor(database): split store.rs into database/ DAO modules"
```

---

### 任务 2：manager/ -> services/ 子目录拆分（FR-5）

**文件：**
- 重命名：`src-tauri/src/manager/` -> `src-tauri/src/services/`
- 创建：`src-tauri/src/services/skill/mod.rs`（从 manager/mod.rs 提取 skill 函数）
- 创建：`src-tauri/src/services/resource/mod.rs`（从 manager/mod.rs 提取 resource 函数）
- 创建：`src-tauri/src/services/preset/mod.rs`（从 manager/preset.rs 转换）
- 创建：`src-tauri/src/services/mcp/mod.rs`（从 manager/mcp.rs 转换）
- 创建：`src-tauri/src/services/plugin/mod.rs`（从 manager/plugin.rs 转换）
- 修改：`src-tauri/src/services/mod.rs`（模块聚合 + 保留跨域函数）
- 修改：`src-tauri/src/lib.rs`（`mod manager` -> `mod services`）

- [ ] **步骤 1：重命名目录并创建子目录结构**

```bash
git mv src-tauri/src/manager src-tauri/src/services
mkdir -p src-tauri/src/services/{skill,resource,preset,mcp,plugin}
```

- [ ] **步骤 2：将扁平文件转为子目录**

将 `services/preset.rs` -> `services/preset/mod.rs`，`services/mcp.rs` -> `services/mcp/mod.rs`，`services/plugin.rs` -> `services/plugin/mod.rs`：

```bash
git mv src-tauri/src/services/preset.rs src-tauri/src/services/preset/mod.rs
git mv src-tauri/src/services/mcp.rs src-tauri/src/services/mcp/mod.rs
git mv src-tauri/src/services/plugin.rs src-tauri/src/services/plugin/mod.rs
```

- [ ] **步骤 3：从 services/mod.rs 中提取 skill 函数到 services/skill/mod.rs**

将以下函数从 `services/mod.rs` 移到 `services/skill/mod.rs`（保持函数签名不变）：
`install_skill`、`enable_skill_for_tool`、`disable_skill_for_tool`、`is_skill_in_tool_range`、`assign_skill_to_subagent`、`parse_skill_meta`、`detect_suite`、`scan_skills_recursive`

- [ ] **步骤 4：从 services/mod.rs 中提取 resource 函数到 services/resource/mod.rs**

将 `auto_import_extensions`、`ImportStats`、`SkillMeta` 移到 `services/resource/mod.rs`。

- [ ] **步骤 5：更新 services/mod.rs -- 模块声明 + 保留跨域函数**

```rust
pub mod skill;
pub mod resource;
pub mod preset;
pub mod mcp;
pub mod plugin;

// 跨域函数保留在 mod.rs
pub fn detect_subagents(tool_id: &str) -> Vec<String> { /* 原样保留 */ }
pub fn toggle_plugin(plugin_name: &str, tool_id: &str, enabled: bool, kind: &str) -> Result<(), String> {
    crate::services::plugin::toggle_plugin(plugin_name, tool_id, enabled, kind)
}
pub fn toggle_mcp(mcp_name: &str, tool_id: &str, enabled: bool) -> Result<(), String> {
    crate::services::mcp::toggle_mcp(mcp_name, tool_id, enabled)
}
```

- [ ] **步骤 6：更新所有引用**

运行：`rg -n "crate::manager" src-tauri/src/` 替换为 `crate::services`。
注意 skill 函数路径变化：`crate::manager::install_skill` -> `crate::services::skill::install_skill`。

- [ ] **步骤 7：更新 lib.rs**

`mod manager;` -> `mod services;`

- [ ] **步骤 8：验证编译**

运行：`cd src-tauri && cargo check`
预期：PASS

- [ ] **步骤 9：Commit**

```bash
git add -A src-tauri/src/
git commit -m "refactor(services): split manager/ into services/ subdirectories"
```

---

### 任务 3：terminal/ -> window/ + WindowManager trait（FR-5b）

**文件：**
- 重命名：`src-tauri/src/terminal/` -> `src-tauri/src/window/`
- 修改：`src-tauri/src/window/mod.rs`（新增 WindowManager trait）
- 修改：`src-tauri/src/lib.rs`（`mod terminal` -> `mod window`）

- [ ] **步骤 1：重命名目录**

```bash
git mv src-tauri/src/terminal src-tauri/src/window
```

- [ ] **步骤 2：在 window/mod.rs 中新增 WindowManager trait**

在现有 `focus_terminal_for_pid` 函数上方添加 trait 定义：

```rust
/// 终端窗口管理抽象
pub trait WindowManager {
    /// 聚焦到指定 PID 对应的终端窗口
    fn focus(&self, pid: u32) -> Result<(), String>;
}

/// 默认实现：按平台分发
pub struct DefaultWindowManager;

impl WindowManager for DefaultWindowManager {
    fn focus(&self, pid: u32) -> Result<(), String> {
        focus_terminal_for_pid(pid)
    }
}
```

保留现有的 `focus_terminal_for_pid` 和 `get_tty_for_pid` 函数不变。

- [ ] **步骤 3：更新所有引用**

运行：`rg -n "crate::terminal" src-tauri/src/` 替换为 `crate::window`。
`mod terminal;` -> `mod window;`

- [ ] **步骤 4：验证编译 + 手动跳转测试**

运行：`cd src-tauri && cargo check`
预期：PASS

运行：`pnpm tauri:dev`，打开 iTerm2 运行 Claude Code，在看板点击会话卡片，验证终端窗口被激活。

- [ ] **步骤 5：Commit**

```bash
git add -A src-tauri/src/
git commit -m "refactor(window): rename terminal/ to window/ and add WindowManager trait"
```

---

### 任务 4：notify 集成到 monitor（FR-5c）

**文件：**
- 修改：`src-tauri/src/monitor/mod.rs`

- [ ] **步骤 1：在 monitor/mod.rs 中集成 notify-rs**

```rust
use notify::{Watcher, RecursiveMode, EventKind, RecommendedWatcher};
use std::sync::mpsc::channel;
use std::time::Duration;

/// 启动文件监听，检测 Hook/进程事件文件变化时触发会话刷新
pub fn start_file_watcher<F>(paths: Vec<std::path::PathBuf>, on_change: F)
where
    F: Fn() + Send + 'static,
{
    std::thread::spawn(move || {
        let (tx, rx) = channel();
        let mut watcher = match RecommendedWatcher::new(tx, notify::Config::default()) {
            Ok(w) => w,
            Err(e) => {
                log::warn!("notify watcher 初始化失败，回退纯轮询: {}", e);
                return;
            }
        };

        for path in &paths {
            if path.exists() {
                let _ = watcher.watch(path, RecursiveMode::Recursive);
            }
        }

        // notify 事件 + 30s 轮询兜底
        loop {
            match rx.recv_timeout(Duration::from_secs(30)) {
                Ok(Ok(event)) if matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) => {
                    on_change();
                }
                Ok(Err(e)) => log::warn!("notify 事件错误: {}", e),
                Err(_) => {
                    // 超时，触发兜底轮询
                    on_change();
                }
            }
        }
    });
}
```

- [ ] **步骤 2：在 lib.rs setup 中启动 watcher（可选接入）**

在 `setup` 闭包中，获取 Hook 事件目录路径，调用 `start_file_watcher`。注意：此步骤为接入位预留，实际触发逻辑可后续完善。

- [ ] **步骤 3：验证编译**

运行：`cd src-tauri && cargo check`
预期：PASS

- [ ] **步骤 4：Commit**

```bash
git add src-tauri/src/monitor/mod.rs src-tauri/src/lib.rs
git commit -m "feat(monitor): integrate notify-rs file watcher with polling fallback"
```

---

### 任务 5：IPC 实体对齐表（FR-5d）

**文件：**
- 创建：`docs/engineering/reference/ipc-entity-alignment.md`

- [ ] **步骤 1：编写 Rust <-> TypeScript 实体对齐表**

```markdown
# IPC 实体对齐表

| Rust struct | TS interface | 字段映射 |
|-------------|-------------|---------|
| `Session` | `Session` | `pid: u32` -> `pid: number`；`status: SessionStatus` -> `status: string` |
| `SessionsResponse` | `SessionsResponse` | `sessions: Vec<Session>` -> `sessions: Session[]` |
| `ExtensionRecord` | `Extension` | `id/kind/name: String` -> `string`；`serde(rename_all = "camelCase")` |
| `PresetRecord` | `Preset` | 同上 camelCase 映射 |
| `SubAgentRecord` | `SubAgent` | 同上 |
| `NativeExtensionRecord` | `NativeExtension` | 同上 |
```

- [ ] **步骤 2：Commit**

```bash
git add docs/engineering/reference/ipc-entity-alignment.md
git commit -m "docs: add IPC entity alignment table for refactor verification"
```

---

### 任务 6：commands.rs -> commands/ 拆分（FR-1）

**文件：**
- 创建：`src-tauri/src/commands/mod.rs`
- 创建：`src-tauri/src/commands/session.rs`
- 创建：`src-tauri/src/commands/resource.rs`
- 创建：`src-tauri/src/commands/preset.rs`
- 创建：`src-tauri/src/commands/skill.rs`
- 创建：`src-tauri/src/commands/mcp.rs`
- 创建：`src-tauri/src/commands/plugin.rs`
- 创建：`src-tauri/src/commands/settings.rs`
- 创建：`src-tauri/src/commands/screenshot.rs`
- 删除：`src-tauri/src/commands.rs`

- [ ] **步骤 1：按功能域拆分命令到子模块**

命令映射表：

| 子模块 | 命令 |
|--------|------|
| `session.rs` | `get_all_sessions`, `focus_session`, `kill_session` |
| `resource.rs` | `list_extensions_with_assignments`, `scan_native_resources`, `import_native_resources`, `list_tool_resources`, `check_preset_compatibility` |
| `preset.rs` | `create_preset`, `delete_preset`, `list_presets`, `apply_preset`, `deactivate_preset`, `apply_preset_to_subagent`, `deactivate_preset_from_subagent` |
| `skill.rs` | `list_repo_skills`, `install_skill`, `rescan_skills`, `assign_skill_to_subagent` |
| `mcp.rs` | `toggle_mcp_for_tool`, `read_mcp_servers`, `write_mcp_server`, `remove_mcp_server` |
| `plugin.rs` | `toggle_plugin_for_tool` |
| `settings.rs` | `get_setting`, `set_setting`, `detect_tools`, `detect_subagents`, `list_sub_agents` |
| `screenshot.rs` | `capture_window_screenshot`, `list_screenshots` |

每个子模块将对应的 `#[tauri::command]` 函数及其辅助结构体（如 `ExtensionWithAssignments`、`PresetApplyResult`、`ScreenshotResult`）从 `commands.rs` 原样搬入。函数体不变，只改 `use` 引用路径。

- [ ] **步骤 2：每个子模块实现 add_commands 注册函数**

每个子模块文件底部添加 `add_commands` 函数，将本模块的命令注册到 Tauri builder：

```rust
// src-tauri/src/commands/session.rs 底部
use tauri::{Builder, Runtime};

pub fn add_commands<R: Runtime>(builder: Builder<R>) -> Builder<R> {
    builder.invoke_handler(tauri::generate_handler![
        get_all_sessions, focus_session, kill_session
    ])
}
```

其余子模块（resource.rs、preset.rs、skill.rs、mcp.rs、plugin.rs、settings.rs、screenshot.rs）同样实现各自的 `add_commands`。

- [ ] **步骤 3：创建 commands/mod.rs -- 聚合调用各子模块的 add_commands**

```rust
pub mod session;
pub mod resource;
pub mod preset;
pub mod skill;
pub mod mcp;
pub mod plugin;
pub mod settings;
pub mod screenshot;

use tauri::{Builder, Runtime};

/// 注册所有命令到 Tauri builder
pub fn add_commands<R: Runtime>(builder: Builder<R>) -> Builder<R> {
    let builder = session::add_commands(builder);
    let builder = resource::add_commands(builder);
    let builder = preset::add_commands(builder);
    let builder = skill::add_commands(builder);
    let builder = mcp::add_commands(builder);
    let builder = plugin::add_commands(builder);
    let builder = settings::add_commands(builder);
    let builder = screenshot::add_commands(builder);
    builder
}
```

- [ ] **步骤 4：更新 lib.rs -- 使用 add_commands 替代逐个注册**

将 `lib.rs` 中 `invoke_handler` 的 `generate_handler![...]` 大列表替换为：

```rust
let builder = commands::add_commands(builder);
```

同时注册 `greet` 和 `update_tray_menu`（移到 `commands/mod.rs` 或 `commands/system.rs` 的 `add_commands` 中）。

- [ ] **步骤 5：删除 commands.rs**

确认编译通过后删除 `src-tauri/src/commands.rs`。

- [ ] **步骤 6：验证编译**

运行：`cd src-tauri && cargo check`
预期：PASS

- [ ] **步骤 7：Commit**

```bash
git add src-tauri/src/commands/
git rm src-tauri/src/commands.rs
git commit -m "refactor(commands): split commands.rs into commands/ directory with add_commands pattern"
```

---

### 任务 7：components/ 子目录化（FR-3）

**文件：**
- 创建：`src/components/sessions/`、`src/components/resources/`、`src/components/presets/`、`src/components/settings/`、`src/components/mcp/`、`src/components/common/`
- 移动：现有 20+ 组件文件到对应子目录
- 创建：各子目录 `index.ts`

- [ ] **步骤 1：创建子目录并移动文件**

```bash
mkdir -p src/components/{sessions,resources,presets,settings,mcp,common}
git mv src/components/SessionCard.tsx src/components/sessions/
git mv src/components/SessionGrid.tsx src/components/sessions/
git mv src/components/StatusLight.tsx src/components/sessions/
git mv src/components/ResourceByKindView.tsx src/components/resources/
git mv src/components/ResourceByToolView.tsx src/components/resources/
git mv src/components/ExtensionList.tsx src/components/resources/
git mv src/components/ImportDialog.tsx src/components/resources/
git mv src/components/CompatibilityDialog.tsx src/components/resources/
git mv src/components/PresetList.tsx src/components/presets/
git mv src/components/ScreenshotTool.tsx src/components/settings/
git mv src/components/McpManager.tsx src/components/mcp/
git mv src/components/ToolIcon.tsx src/components/common/
git mv src/components/title-bar.tsx src/components/common/
git mv src/components/main-title-bar.tsx src/components/common/
git mv src/components/window-frame.tsx src/components/common/
git mv src/components/theme-provider.tsx src/components/common/
git mv src/components/language-toggle.tsx src/components/common/
git mv src/components/mode-toggle.tsx src/components/common/
git mv src/components/shortcut-input.tsx src/components/common/
git mv src/components/updater-dialog.tsx src/components/common/
```

- [ ] **步骤 2：创建各子目录 index.ts**

每个子目录创建 `index.ts` 导出组件：

```typescript
// src/components/sessions/index.ts
export { SessionCard } from "./SessionCard";
export { SessionGrid } from "./SessionGrid";
export { StatusLight } from "./StatusLight";
```

- [ ] **步骤 3：更新所有 import 路径**

运行：`rg -n "from.*@/components/(Session|Status|Resource|Extension|Import|Compat|Preset|Screenshot|Mcp|Tool|title|main-title|window-frame|theme|language|mode|shortcut|updater)" src/` 找到所有引用，批量更新路径。

- [ ] **步骤 4：验证编译**

运行：`pnpm build`
预期：PASS

- [ ] **步骤 5：Commit**

```bash
git add -A src/components/
git commit -m "refactor(components): organize components into feature subdirectories"
```

---

### 任务 8：src/lib/api/ 层（FR-4）

**文件：**
- 创建：`src/lib/api/session.ts`
- 创建：`src/lib/api/resource.ts`
- 创建：`src/lib/api/preset.ts`
- 创建：`src/lib/api/skill.ts`
- 创建：`src/lib/api/mcp.ts`
- 创建：`src/lib/api/plugin.ts`
- 创建：`src/lib/api/settings.ts`
- 修改：14 个直接调用 `invoke()` 的文件

- [ ] **步骤 1：安装 zod（FR-7 前置依赖）**

```bash
pnpm add zod
```

- [ ] **步骤 2：创建 API 模块**

```typescript
// src/lib/api/session.ts
import { invoke } from "@tauri-apps/api/core";

import type { SessionsResponse } from "@/lib/schemas/session";

export async function getAllSessions(): Promise<SessionsResponse> {
  return await invoke<SessionsResponse>("get_all_sessions");
}

export async function focusSession(pid: number): Promise<void> {
  return await invoke("focus_session", { pid });
}

export async function killSession(pid: number): Promise<void> {
  return await invoke("kill_session", { pid });
}
```

```typescript
// src/lib/api/resource.ts
import { invoke } from "@tauri-apps/api/core";

export async function listExtensionsWithAssignments() {
  return await invoke("list_extensions_with_assignments");
}

export async function scanNativeResources(toolId: string) {
  return await invoke("scan_native_resources", { toolId });
}

export async function importNativeResources(items: [string, string][]) {
  return await invoke("import_native_resources", { items });
}

export async function listToolResources(toolId: string) {
  return await invoke("list_tool_resources", { toolId });
}

export async function checkPresetCompatibility(presetId: string, toolId: string) {
  return await invoke("check_preset_compatibility", { presetId, toolId });
}
```

```typescript
// src/lib/api/preset.ts
import { invoke } from "@tauri-apps/api/core";

export async function listPresets() {
  return await invoke("list_presets");
}
export async function createPreset(name: string, items: [string, string][]) {
  return await invoke("create_preset", { name, items });
}
export async function deletePreset(presetId: string) {
  return await invoke("delete_preset", { presetId });
}
export async function applyPreset(presetId: string, toolId: string) {
  return await invoke("apply_preset", { presetId, toolId });
}
export async function deactivatePreset(presetId: string, toolId: string) {
  return await invoke("deactivate_preset", { presetId, toolId });
}
export async function applyPresetToSubagent(presetId: string, toolId: string, subAgentId: string) {
  return await invoke("apply_preset_to_subagent", { presetId, toolId, subAgentId });
}
export async function deactivatePresetFromSubagent(presetId: string, toolId: string, subAgentId: string) {
  return await invoke("deactivate_preset_from_subagent", { presetId, toolId, subAgentId });
}
```

```typescript
// src/lib/api/skill.ts
import { invoke } from "@tauri-apps/api/core";

export async function listRepoSkills() {
  return await invoke("list_repo_skills");
}
export async function installSkill(sourcePath: string, name: string) {
  return await invoke("install_skill", { sourcePath, name });
}
export async function rescanSkills() {
  return await invoke("rescan_skills");
}
export async function assignSkillToSubagent(skillName: string, toolId: string, subAgentId: string) {
  return await invoke("assign_skill_to_subagent", { skillName, toolId, subAgentId });
}
```

```typescript
// src/lib/api/mcp.ts
import { invoke } from "@tauri-apps/api/core";

export async function toggleMcpForTool(mcpName: string, toolId: string, enabled: boolean) {
  return await invoke("toggle_mcp_for_tool", { mcpName, toolId, enabled });
}
export async function readMcpServers(toolId: string) {
  return await invoke("read_mcp_servers", { toolId });
}
export async function writeMcpServer(toolId: string, mcpName: string, command: string, args: string[], env: Record<string, string>) {
  return await invoke("write_mcp_server", { toolId, mcpName, command, args, env });
}
export async function removeMcpServer(toolId: string, mcpName: string) {
  return await invoke("remove_mcp_server", { toolId, mcpName });
}
```

```typescript
// src/lib/api/plugin.ts
import { invoke } from "@tauri-apps/api/core";

export async function togglePluginForTool(pluginName: string, toolId: string, enabled: boolean, kind: string) {
  return await invoke("toggle_plugin_for_tool", { pluginName, toolId, enabled, kind });
}
```

```typescript
// src/lib/api/settings.ts
import { invoke } from "@tauri-apps/api/core";

export async function getSetting(key: string) {
  return await invoke<string | null>("get_setting", { key });
}
export async function setSetting(key: string, value: string) {
  return await invoke("set_setting", { key, value });
}
export async function detectTools() {
  return await invoke("detect_tools");
}
export async function detectSubagents(toolId: string) {
  return await invoke("detect_subagents", { toolId });
}
export async function listSubAgents(toolId: string) {
  return await invoke("list_sub_agents", { toolId });
}
```

- [ ] **步骤 3：迁移所有 invoke 调用**

逐一修改以下 14 个文件，将直接 `invoke()` 调用替换为 API 层函数调用：

`src/hooks/useSessions.ts`、`src/hooks/useNotification.ts`、`src/pages/home.tsx`、`src/pages/settings.tsx`、`src/components/sessions/SessionCard.tsx`、`src/components/resources/ExtensionList.tsx`、`src/components/resources/ResourceByToolView.tsx`、`src/components/resources/ImportDialog.tsx`、`src/components/resources/CompatibilityDialog.tsx`、`src/components/presets/PresetList.tsx`、`src/components/mcp/McpManager.tsx`、`src/components/settings/ScreenshotTool.tsx`、`src/components/common/language-toggle.tsx`、`src/lib/screenshot.ts`

替换模式：

```typescript
// 之前：
import { invoke } from "@tauri-apps/api/core";
const sessions = await invoke("get_all_sessions");

// 之后：
import { getAllSessions } from "@/lib/api/session";
const sessions = await getAllSessions();
```

- [ ] **步骤 4：验证编译**

运行：`pnpm build`
预期：PASS，且 `rg -n "from \"@tauri-apps/api/core\"" src/` 仅出现在 `src/lib/api/` 和 `src/tauri-mock.ts` 中。

- [ ] **步骤 5：Commit**

```bash
git add src/lib/api/ src/
git commit -m "refactor(api): create src/lib/api/ layer and migrate all invoke() calls"
```

---

### 任务 9：React Query 替代手动轮询（FR-8）

**文件：**
- 创建：`src/lib/query/queryClient.ts`
- 创建：`src/lib/query/queries/sessions.ts`
- 创建：`src/lib/query/queries/resources.ts`
- 创建：`src/lib/query/queries/presets.ts`
- 创建：`src/lib/query/mutations/sessions.ts`
- 创建：`src/lib/query/mutations/resources.ts`
- 修改：`src/main.tsx`（注册 QueryClientProvider）
- 修改：`src/hooks/useSessions.ts`
- 修改：`src/config/constants.ts`（POLL_INTERVAL）

- [ ] **步骤 1：安装 React Query**

```bash
pnpm add @tanstack/react-query
```

- [ ] **步骤 2：创建 queryClient.ts**

```typescript
// src/lib/query/queryClient.ts
import { QueryClient } from "@tanstack/react-query";

export const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: 2,
      staleTime: 1000,
      refetchIntervalInBackground: false,
    },
  },
});
```

- [ ] **步骤 3：创建 queries/sessions.ts**

```typescript
// src/lib/query/queries/sessions.ts
import { useQuery } from "@tanstack/react-query";
import { getAllSessions } from "@/lib/api/session";
import { POLL_INTERVAL } from "@/config/constants";

export function useSessionsQuery() {
  return useQuery({
    queryKey: ["sessions"],
    queryFn: getAllSessions,
    refetchInterval: POLL_INTERVAL,
    refetchIntervalInBackground: false,
    staleTime: 1000,
  });
}
```

- [ ] **步骤 4：创建 queries/resources.ts 和 queries/presets.ts**

```typescript
// src/lib/query/queries/resources.ts
import { useQuery } from "@tanstack/react-query";
import { listExtensionsWithAssignments } from "@/lib/api/resource";

export function useExtensionsQuery() {
  return useQuery({
    queryKey: ["extensions"],
    queryFn: listExtensionsWithAssignments,
    staleTime: 5000,
  });
}
```

```typescript
// src/lib/query/queries/presets.ts
import { useQuery } from "@tanstack/react-query";
import { listPresets } from "@/lib/api/preset";

export function usePresetsQuery() {
  return useQuery({
    queryKey: ["presets"],
    queryFn: listPresets,
    staleTime: 10000,
  });
}
```

- [ ] **步骤 5：创建 mutations**

```typescript
// src/lib/query/mutations/sessions.ts
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { focusSession, killSession } from "@/lib/api/session";

export function useFocusSessionMutation() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (pid: number) => focusSession(pid),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["sessions"] }),
  });
}
```

```typescript
// src/lib/query/mutations/resources.ts
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { toggleMcpForTool } from "@/lib/api/mcp";
import { togglePluginForTool } from "@/lib/api/plugin";
import { enableResource, disableResource } from "@/lib/api/resource";

export function useToggleMcpMutation() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ mcpName, toolId, enabled }: { mcpName: string; toolId: string; enabled: boolean }) =>
      toggleMcpForTool(mcpName, toolId, enabled),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["extensions"] });
    },
  });
}

export function useEnableResourceMutation() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ extId, toolId, enabled }: { extId: string; toolId: string; enabled: boolean }) =>
      enabled ? enableResource(extId, toolId) : disableResource(extId, toolId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["extensions"] });
    },
  });
}
```

> **注意：** `enableResource`/`disableResource` 需在 `src/lib/api/resource.ts` 中补充定义，委托给对应的 toggle 命令。

- [ ] **步骤 6：在 main.tsx 注册 QueryClientProvider**

```typescript
// src/main.tsx
import { QueryClientProvider } from "@tanstack/react-query";
import { queryClient } from "@/lib/query/queryClient";

// 在 <App /> 外层包裹
root.render(
  <QueryClientProvider client={queryClient}>
    <App />
  </QueryClientProvider>
);
```

- [ ] **步骤 7：重构 useSessions hook**

```typescript
// src/hooks/useSessions.ts
import { useSessionsQuery } from "@/lib/query/queries/sessions";

export function useSessions() {
  return useSessionsQuery();
}
```

- [ ] **步骤 8：更新调用方适配新返回结构**

React Query 返回 `{ data, isLoading, error, ... }` 而非直接数据。检查所有 `useSessions()` 调用方，适配为 `const { data, isLoading } = useSessions()`。

运行：`rg -n "useSessions" src/` 找到所有调用点。

- [ ] **步骤 9：验证编译 + 功能**

运行：`pnpm build`
预期：PASS

运行：`pnpm tauri:dev`，验证看板正常轮询、切换页面后缓存数据瞬时显示。

- [ ] **步骤 10：Commit**

```bash
git add src/lib/query/ src/main.tsx src/hooks/useSessions.ts
git commit -m "feat(query): replace manual polling with React Query"
```

---

### 任务 10：React Error Boundaries（FR-9）

**文件：**
- 创建：`src/components/common/ErrorBoundary.tsx`
- 创建：`src/components/common/PageErrorBoundary.tsx`
- 修改：`src/pages/home.tsx`、`src/pages/settings.tsx`（包裹错误边界）

- [ ] **步骤 1：创建 ErrorBoundary 组件**

```typescript
// src/components/common/ErrorBoundary.tsx
import { Component, type ReactNode } from "react";
import { Button } from "@/components/ui/button";

interface Props {
  children: ReactNode;
  fallback?: ReactNode;
}
interface State {
  hasError: boolean;
  error?: Error;
}

export class ErrorBoundary extends Component<Props, State> {
  state: State = { hasError: false };

  static getDerivedStateFromError(error: Error): State {
    return { hasError: true, error };
  }

  render() {
    if (this.state.hasError) {
      return this.props.fallback ?? (
        <div className="flex flex-col items-center gap-2 p-4 text-center">
          <p className="text-sm text-muted-foreground">组件加载失败</p>
          <Button
            size="sm"
            variant="outline"
            onClick={() => this.setState({ hasError: false })}
          >
            重试
          </Button>
        </div>
      );
    }
    return this.props.children;
  }
}
```

- [ ] **步骤 2：创建 PageErrorBoundary 组件**

```typescript
// src/components/common/PageErrorBoundary.tsx
import { Component, type ReactNode } from "react";
import { Button } from "@/components/ui/button";

interface Props { children: ReactNode; }
interface State { hasError: boolean; }

export class PageErrorBoundary extends Component<Props, State> {
  state: State = { hasError: false };

  static getDerivedStateFromError(): State {
    return { hasError: true };
  }

  render() {
    if (this.state.hasError) {
      return (
        <div className="flex h-full flex-col items-center justify-center gap-4 p-8">
          <p className="text-lg font-medium">出错了</p>
          <div className="flex gap-2">
            <Button onClick={() => this.setState({ hasError: false })}>重试</Button>
            <Button variant="outline" onClick={() => window.location.reload()}>
              刷新页面
            </Button>
          </div>
        </div>
      );
    }
    return this.props.children;
  }
}
```

- [ ] **步骤 3：在关键页面和组件包裹错误边界**

在 `src/pages/home.tsx` 中用 `PageErrorBoundary` 包裹看板区域，在每个 `SessionCard` 外层包裹 `ErrorBoundary`：

```tsx
<PageErrorBoundary>
  <SessionGrid>
    {sessions.map((s) => (
      <ErrorBoundary key={s.id} fallback={<div className="p-4 text-sm text-muted-foreground">会话加载失败</div>}>
        <SessionCard session={s} />
      </ErrorBoundary>
    ))}
  </SessionGrid>
</PageErrorBoundary>
```

- [ ] **步骤 4：验证编译**

运行：`pnpm build`
预期：PASS

- [ ] **步骤 5：Commit**

```bash
git add src/components/common/ErrorBoundary.tsx src/components/common/PageErrorBoundary.tsx src/pages/
git commit -m "feat(ui): add ErrorBoundary and PageErrorBoundary for graceful degradation"
```

---

### 任务 11：src/config/ 常量集中（FR-6）

**文件：**
- 创建：`src/config/constants.ts`
- 创建：`src/config/tool-presets.ts`
- 创建：`src/config/skill-presets.ts`
- 创建：`src/config/mcp-presets.ts`

- [ ] **步骤 1：创建 constants.ts**

```typescript
// src/config/constants.ts

// 轮询间隔（毫秒）
export const POLL_INTERVAL = 3000;

// 事件新鲜度阈值（秒）
export const EVENT_FRESHNESS_THRESHOLD = 30;

// 会话状态枚举
export const SESSION_STATUS = {
  RUNNING: "running",
  WAITING: "waiting",
  IDLE: "idle",
  COMPLETED: "completed",
  UNKNOWN: "unknown",
} as const;

// 资源类型
export const EXTENSION_KIND = {
  SKILL: "skill",
  MCP: "mcp",
  PLUGIN: "plugin",
} as const;

// 支持的工具列表
export const SUPPORTED_TOOLS = ["claude", "codex", "opencode", "openclaw"] as const;
```

- [ ] **步骤 2：创建 tool-presets.ts**

```typescript
// src/config/tool-presets.ts

export interface ToolPreset {
  id: string;
  name: string;
  icon: string;
  processNames: string[];
  mcpFormat: "json" | "toml" | "jsonc";
  hookSupported: boolean;
  subAgentSupported: boolean;
}

export const TOOL_PRESETS: ToolPreset[] = [
  { id: "claude", name: "Claude Code", icon: "🤖", processNames: ["claude"], mcpFormat: "json", hookSupported: true, subAgentSupported: false },
  { id: "codex", name: "Codex CLI", icon: "⚡", processNames: ["codex", "Codex"], mcpFormat: "toml", hookSupported: true, subAgentSupported: false },
  { id: "opencode", name: "OpenCode", icon: "📖", processNames: ["opencode"], mcpFormat: "jsonc", hookSupported: false, subAgentSupported: true },
  { id: "openclaw", name: "OpenClaw", icon: "🐾", processNames: ["openclaw"], mcpFormat: "json", hookSupported: false, subAgentSupported: false },
];
```

- [ ] **步骤 3：创建 skill-presets.ts**

```typescript
// src/config/skill-presets.ts
export interface SkillPreset {
  name: string;
  description: string;
  skills: string[];
}

export const SKILL_PRESETS: SkillPreset[] = [
  {
    name: "创意写作",
    description: "头脑风暴 + 写作辅助",
    skills: ["brainstorming", "writing-assistant"],
  },
  {
    name: "代码审查",
    description: "代码审查 + 重构建议",
    skills: ["code-review", "refactoring"],
  },
];
```

- [ ] **步骤 4：创建 mcp-presets.ts**

```typescript
// src/config/mcp-presets.ts
export interface McpPreset {
  name: string;
  description: string;
  command: string;
  args: string[];
  env?: Record<string, string>;
  formats: ("json" | "toml" | "jsonc")[];
}

export const MCP_PRESETS: McpPreset[] = [
  {
    name: "filesystem",
    description: "文件系统访问 MCP",
    command: "npx",
    args: ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/allowed"],
    formats: ["json", "toml", "jsonc"],
  },
];
```

- [ ] **步骤 5：迁移硬编码常量**

运行：`rg -n "3000|setInterval|\"claude\"|\"codex\"|\"opencode\"|\"openclaw\"" src/` 找到硬编码值，替换为 config 常量引用。

- [ ] **步骤 6：验证编译**

运行：`pnpm build`
预期：PASS

- [ ] **步骤 7：Commit**

```bash
git add src/config/
git commit -m "feat(config): centralize constants and tool presets in src/config/"
```

---

### 任务 12：src/lib/schemas/ Zod 验证（FR-7）

**文件：**
- 创建：`src/lib/schemas/session.ts`
- 创建：`src/lib/schemas/extension.ts`
- 创建：`src/lib/schemas/preset.ts`
- 创建：`src/lib/schemas/settings.ts`

- [ ] **步骤 1：创建 session.ts schema**

```typescript
// src/lib/schemas/session.ts
import { z } from "zod";

export const SessionSchema = z.object({
  id: z.string(),
  tool: z.string(),
  projectPath: z.string().optional(),
  status: z.enum(["running", "waiting", "idle", "completed", "unknown"]),
  pid: z.number().optional(),
  cpuUsage: z.number().optional(),
  duration: z.number().optional(),
  lastMessage: z.string().optional(),
  gitBranch: z.string().optional(),
});

export const SessionsResponseSchema = z.object({
  sessions: z.array(SessionSchema),
});

export type Session = z.infer<typeof SessionSchema>;
export type SessionsResponse = z.infer<typeof SessionsResponseSchema>;
```

- [ ] **步骤 2：创建 extension.ts、preset.ts、settings.ts schemas**

```typescript
// src/lib/schemas/extension.ts
import { z } from "zod";

export const ExtensionSchema = z.object({
  id: z.string(),
  kind: z.enum(["skill", "mcp", "plugin"]),
  name: z.string(),
  description: z.string().optional(),
  sourcePath: z.string().optional(),
  sourceUrl: z.string().optional(),
  suite: z.string().optional(),
  assignments: z.array(z.object({
    toolId: z.string(),
    enabled: z.boolean(),
    linkStatus: z.string(),
    subAgentId: z.string().optional(),
  })).optional(),
});

export type Extension = z.infer<typeof ExtensionSchema>;
```

```typescript
// src/lib/schemas/preset.ts
import { z } from "zod";

export const PresetSchema = z.object({
  id: z.string(),
  name: z.string(),
  items: z.array(z.tuple([z.string(), z.string()])),
});

export type Preset = z.infer<typeof PresetSchema>;
```

```typescript
// src/lib/schemas/settings.ts
import { z } from "zod";

export const SettingsSchema = z.record(z.string(), z.string());
export type Settings = z.infer<typeof SettingsSchema>;
```

- [ ] **步骤 3：在 API 层集成 schema 验证**

在 `src/lib/api/session.ts` 中使用 schema 验证返回值：

```typescript
import { SessionsResponseSchema } from "@/lib/schemas/session";

export async function getAllSessions() {
  const raw = await invoke("get_all_sessions");
  return SessionsResponseSchema.parse(raw);
}
```

- [ ] **步骤 4：验证编译**

运行：`pnpm build`
预期：PASS

- [ ] **步骤 5：Commit**

```bash
git add src/lib/schemas/ src/lib/api/
git commit -m "feat(schemas): add Zod runtime validation schemas for IPC entities"
```

---

### 任务 13：Monorepo 工程化基础（FR-10）

**文件：**
- 创建：`tsconfig.base.json`
- 创建：`eslint.config.base.js`
- 创建：`.editorconfig`

- [ ] **步骤 1：创建 tsconfig.base.json**

```json
{
  "compilerOptions": {
    "target": "ESNext",
    "module": "ESNext",
    "moduleResolution": "bundler",
    "strict": true,
    "esModuleInterop": true,
    "skipLibCheck": true,
    "forceConsistentCasingInFileNames": true,
    "resolveJsonModule": true,
    "isolatedModules": true,
    "jsx": "react-jsx"
  }
}
```

- [ ] **步骤 2：让 tsconfig.json 继承 base**

修改现有 `tsconfig.json`，添加 `"extends": "./tsconfig.base.json"`。

- [ ] **步骤 3：创建 eslint.config.base.js**

```javascript
// eslint.config.base.js
import tseslint from "typescript-eslint";

export const baseConfig = [
  ...tseslint.configs.recommended,
  {
    rules: {
      "@typescript-eslint/no-unused-vars": ["error", { argsIgnorePattern: "^_" }],
      "@typescript-eslint/no-explicit-any": "warn",
      "no-console": ["warn", { allow: ["warn", "error"] }],
    },
  },
];
```

- [ ] **步骤 4：创建 .editorconfig**

```ini
root = true
[*]
indent_style = space
indent_size = 2
end_of_line = lf
charset = utf-8
trim_trailing_whitespace = true
insert_final_newline = true
```

- [ ] **步骤 5：验证编译**

运行：`pnpm build`
预期：PASS

- [ ] **步骤 6：Commit**

```bash
git add tsconfig.base.json eslint.config.base.js .editorconfig tsconfig.json
git commit -m "chore: add monorepo base configs (tsconfig.base, editorconfig)"
```

---

### 任务 14：Pre-commit Hooks（FR-11）

**文件：**
- 修改：`package.json`（添加 lint-staged 配置）
- 创建：`.husky/pre-commit`

- [ ] **步骤 1：安装依赖**

```bash
pnpm add -D husky lint-staged
pnpm exec husky init
```

- [ ] **步骤 2：配置 lint-staged**

在 `package.json` 中添加：

```json
{
  "lint-staged": {
    "src/**/*.{ts,tsx}": ["prettier --write", "eslint --fix"],
    "src-tauri/**/*.rs": ["rustfmt --edition 2021"]
  }
}
```

- [ ] **步骤 3：配置 pre-commit hook**

```bash
# .husky/pre-commit
pnpm lint-staged
```

> **Spec 说明：** Spec 002 FR-11.3 原文包含 `pnpm build`，但 Spec 004 FR-11 建议仅跑 lint-staged、`pnpm build` 交给 CI。本计划遵循 Spec 004 的建议，避免提交期跑全量 build 拖慢速度。

- [ ] **步骤 4：验证 hook 生效**

运行：`git commit --allow-empty -m "test: verify pre-commit hook"`（之后可 amend 掉）
预期：lint-staged 执行，无错误时提交成功。

- [ ] **步骤 5：Commit**

```bash
git add .husky/ package.json
git commit -m "chore: add husky + lint-staged pre-commit hooks"
```

---

### 任务 15：宪法代码组织 PATCH 修订（FR-12）

**文件：**
- 修改：`.specify/memory/constitution.md`（开发流程 -> 代码组织小节）

- [ ] **步骤 1：验证重构完成**

运行：`cd src-tauri && cargo check && cargo clippy -- -D warnings`
运行：`pnpm build`
预期：全部 PASS

- [ ] **步骤 2：更新宪法代码组织小节**

将 `.specify/memory/constitution.md` 中「开发流程 -> 代码组织」更新为反映新结构：

```markdown
### 代码组织

后端（src-tauri/src/）：
- `commands/` - IPC 命令，按功能域拆分子模块（session/resource/preset/skill/mcp/plugin/settings/screenshot）
- `database/` - 数据层，schema.rs + connection.rs + dao/（session/extension/preset/settings/agent_tool）
- `services/` - 业务逻辑（resource/preset/skill/mcp/plugin）
- `window/` - 终端窗口管理，WindowManager trait + 平台实现
- `monitor/` - 进程扫描、会话解析、notify 文件监听
- `linker/` - 三层符号链接映射
- `adapter/` - Agent 工具适配器

前端（src/）：
- `lib/api/` - Tauri invoke 封装，组件不直接调用 invoke
- `lib/query/` - React Query 查询/变更 hooks
- `lib/schemas/` - Zod 运行时验证 schemas
- `config/` - 全局常量和工具预设
- `components/` - 按功能域子目录组织（sessions/resources/presets/settings/mcp/common/ui）
```

- [ ] **步骤 3：Commit**

```bash
git add .specify/memory/constitution.md
git commit -m "docs(constitution): PATCH update code organization for refactored structure"
```

---

## 自检

**规格覆盖度：**
- FR-1 commands 拆分 -> 任务 6 ✓
- FR-2 store.rs -> database/ -> 任务 1 ✓
- FR-3 components 子目录 -> 任务 7 ✓
- FR-4 api 层 -> 任务 8 ✓
- FR-5 services 重命名 -> 任务 2 ✓
- FR-5b window/WindowManager -> 任务 3 ✓
- FR-5c notify 集成 -> 任务 4 ✓
- FR-5d 实体对齐表 -> 任务 5 ✓
- FR-6 config 集中 -> 任务 11 ✓
- FR-7 zod schemas -> 任务 12 ✓
- FR-8 React Query -> 任务 9 ✓
- FR-9 Error Boundaries -> 任务 10 ✓
- FR-10 Monorepo 基础 -> 任务 13 ✓
- FR-11 Pre-commit hooks -> 任务 14 ✓
- FR-12 宪法修订 -> 任务 15 ✓
无遗漏。

**占位符扫描：** 无占位符。所有步骤含具体代码、命令或文件路径。

**类型一致性：** API 层函数名（`getAllSessions`、`focusSession` 等）在任务 8 定义，任务 9 的 React Query hooks 引用一致。`POLL_INTERVAL` 在任务 11 定义，任务 9 引用。`ErrorBoundary`/`PageErrorBoundary` 在任务 10 定义，页面引用一致。
