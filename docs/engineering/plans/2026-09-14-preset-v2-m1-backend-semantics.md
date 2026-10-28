# 预设组 v2 · M1 后端语义 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把预设组从「增量启用器」升级为「独占套件」：滑块开关语义（开=独占应用、关=恢复默认）、会话级基底快照、原生技能暂存区、专属绑定表、数据源统一——全部后端能力 + Rust 测试，不含前端（M2）与托盘/frontmatter（M3）。

**Architecture:** 在现有三层链接架构上叠加「状态账本」：SQLite 记录基底快照/暂存日志/专属绑定/常驻名单，应用预设 = 拍快照（若无）→ 差集清扫（断 MAM 链 / 移原生目录入 `~/.mam/stash/`）→ 启用预设项；恢复 = 按快照精确重建并销毁快照。文件操作全部复用既有服务（`enable/disable_skill_for_tool`、`toggle_mcp`、`toggle_plugin`），新代码只做编排与记账。

**Tech Stack:** Rust (Tauri 2 后端)、rusqlite、tempfile（测试）。

**Spec:** `docs/engineering/specs/2026-09-14-preset-groups-v2-design.md`（§3 概念模型 / §4 数据模型 / §5 流程 / §8 bug 修复）

## Global Constraints

- 注释用中文、标识符用英文；commit message 可英文（AGENTS.md 语言规范）
- IPC 命令：Rust 侧 snake_case、serde camelCase 对前端（现有模式）
- 记账先于动手：基底快照写入成功后才允许清扫动作；stash 移动成功后必须落账（失败则回滚移动）
- 所有文件/DB 操作幂等：中断重跑安全（spec §5.3）
- 测试共用全局 DB（`tests/support.rs` 的 `Once` 初始化），跨测试数据会串扰——每个测试用唯一的名字字面量
- 每个任务收尾必须过：`cd src-tauri && cargo test`；最终门禁 `cargo clippy` + `cargo fmt`
- 不碰 `monitor/`（会话扫描预算契约与本计划无关）
- MCP 分配 id 形如 `mcp-<name>`、skill `skill-<name>`、plugin `plugin-<name>`（全库约定，新代码必须沿用）
- 测试中取家目录一律 `dirs::home_dir().unwrap()`（`tests/support.rs` 对 Windows 设 `MAM_HOME`，直接读 `env HOME` 不可移植）
- 集成测试共享同一 HOME 与全局 DB，测试执行顺序不定——**断言禁用精确集合相等，一律用 `contains`**（其他测试残留的原生目录/启用项会让 `==` flaky）

---

### Task 1: Schema 与迁移——5 张新表 + presets 3 列

**Files:**
- Modify: `src-tauri/src/database/schema.rs`（presets CREATE 加 3 列；追加 5 张新表）
- Modify: `src-tauri/src/database/migration.rs`（presets 3 列 ALTER，沿用 `column_exists` 模式）

**Interfaces:**
- Produces: 表 `presets(description, scope, bound_tool)`、`resource_bindings`、`tool_residents`、`tool_base_snapshots`、`tool_base_snapshot_items`、`stash_journal`——后续所有 DAO 任务依赖这些表名与列名（Task 2-4 直接照抄本任务的 DDL）

- [x] **Step 1: 写失败测试（老库迁移出 3 列 + 5 表）**

在 `src-tauri/src/database/migration.rs` 的 `mod tests` 末尾追加：

```rust
    /// 预设 v2（spec §4）：老库迁移补 presets 3 列 + 5 张新表
    #[test]
    fn migrate_adds_preset_v2_columns_and_tables() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE extensions (
                id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL,
                description TEXT, source_path TEXT NOT NULL, source_url TEXT,
                version TEXT, tags TEXT, installed_at TEXT NOT NULL, updated_at TEXT NOT NULL
            );
            CREATE TABLE presets (
                id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at TEXT NOT NULL
            );
            CREATE TABLE preset_items (
                id TEXT PRIMARY KEY, preset_id TEXT NOT NULL,
                extension_id TEXT NOT NULL, kind TEXT NOT NULL
            );",
        )
        .unwrap();

        migrate(&conn).unwrap();

        for col in ["description", "scope", "bound_tool"] {
            assert!(
                conn.prepare(&format!("SELECT {} FROM presets LIMIT 0", col)).is_ok(),
                "presets 缺列 {}",
                col
            );
        }
        for table in [
            "resource_bindings",
            "tool_residents",
            "tool_base_snapshots",
            "tool_base_snapshot_items",
            "stash_journal",
        ] {
            let exists: bool = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='{}'",
                        table
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(exists, "缺表 {}", table);
        }
    }
```

- [x] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test migrate_adds_preset_v2 -- --nocapture`（注意目录是 `src-tauri`）
Expected: FAIL——`presets 缺列 description`

- [x] **Step 3: schema.rs——presets CREATE 补列 + 追加 5 张表**

`schema.rs:64-68` 的 presets 建表改为：

```sql
        CREATE TABLE IF NOT EXISTS presets (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            description TEXT NOT NULL DEFAULT '',
            scope       TEXT NOT NULL DEFAULT 'universal',
            bound_tool  TEXT,
            created_at  TEXT NOT NULL
        );
```

并在 `preset_applications` 建表之后、`unread_sessions` 之前追加（spec §4 DDL 原样）：

```sql
        CREATE TABLE IF NOT EXISTS resource_bindings (
            extension_id    TEXT PRIMARY KEY,
            exclusive_tools TEXT NOT NULL,
            reason          TEXT,
            updated_at      TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS tool_residents (
            tool_id       TEXT NOT NULL,
            extension_id  TEXT NOT NULL,
            PRIMARY KEY (tool_id, extension_id)
        );
        CREATE TABLE IF NOT EXISTS tool_base_snapshots (
            tool_id          TEXT PRIMARY KEY,
            active_preset_id TEXT,
            created_at       TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS tool_base_snapshot_items (
            tool_id       TEXT NOT NULL,
            extension_id  TEXT NOT NULL,
            kind          TEXT NOT NULL,
            origin        TEXT NOT NULL,
            PRIMARY KEY (tool_id, extension_id)
        );
        CREATE TABLE IF NOT EXISTS stash_journal (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            tool_id       TEXT NOT NULL,
            skill_name    TEXT NOT NULL,
            stashed_path  TEXT NOT NULL,
            original_path TEXT NOT NULL,
            created_at    TEXT NOT NULL,
            restored_at   TEXT
        );
```

- [x] **Step 4: migration.rs——老库 ALTER（column_exists 模式）**

在 `migrate()` 的 is_native 迁移块之后追加：

```rust
    // 预设 v2（spec §4）：presets 补描述/类型/绑定工具三列（新表由 schema.rs IF NOT EXISTS 覆盖）
    for (col, ddl) in [
        ("description", "ALTER TABLE presets ADD COLUMN description TEXT NOT NULL DEFAULT ''"),
        ("scope", "ALTER TABLE presets ADD COLUMN scope TEXT NOT NULL DEFAULT 'universal'"),
        ("bound_tool", "ALTER TABLE presets ADD COLUMN bound_tool TEXT"),
    ] {
        if !column_exists(conn, "presets", col) {
            conn.execute(ddl, [])
                .map_err(|e| format!("迁移 presets.{} 失败: {}", col, e))?;
        }
    }
```

- [x] **Step 5: 跑测试 + 全量回归**

Run: `cd src-tauri && cargo test migrate && cargo test`
Expected: 新测试 PASS，存量全绿

- [x] **Step 6: Commit**

```bash
git add src-tauri/src/database/schema.rs src-tauri/src/database/migration.rs
git commit -m "feat(preset-v2): schema/migration — presets 3 columns + bindings/residents/snapshots/stash tables"
```

---

### Task 2: preset DAO v2——字段扩展 + update/get/带元信息创建

**Files:**
- Modify: `src-tauri/src/database/dao/preset.rs`
- Modify: `src-tauri/src/database/mod.rs:27-30`（re-export 新函数）
- Test: `src-tauri/tests/dao_test.rs`（追加）

**Interfaces:**
- Produces:
  - `PresetRecord { id, name, description: String, scope: String, bound_tool: Option<String>, items: Vec<PresetItemRecord> }`（serde camelCase——后续任务按此读字段）
  - `create_preset_with_meta(name: &str, description: &str, scope: &str, bound_tool: Option<&str>, items: &[(String, String)]) -> Result<String, String>`
  - `update_preset(id: &str, name: &str, description: &str, scope: &str, bound_tool: Option<&str>, items: &[(String, String)]) -> Result<(), String>`
  - `get_preset(preset_id: &str) -> Option<PresetRecord>`
  - 旧 `create_preset(name, items)` 保留并委托（默认 universal），`list_presets` 返回扩展字段

- [x] **Step 1: 写失败测试**

`src-tauri/tests/dao_test.rs` 末尾追加（沿用文件既有 `support::setup()` 模式；名字带 `v2m1` 前缀避免与存量 `test_preset_crud` 串扰）：

```rust
#[test]
fn test_preset_v2_crud_roundtrip() {
    crate::support::setup();
    use multi_agents_manager_lib::database;

    let items = vec![
        ("skill-v2m1-a".to_string(), "skill".to_string()),
        ("mcp-v2m1-b".to_string(), "mcp".to_string()),
    ];
    // 带元信息创建：工具私有预设
    let id = database::create_preset_with_meta(
        "v2m1-设计套件",
        "做设计时的备忘",
        "tool",
        Some("codex"),
        &items,
    )
    .unwrap();
    let p = database::get_preset(&id).expect("get_preset 应返回");
    assert_eq!(p.name, "v2m1-设计套件");
    assert_eq!(p.description, "做设计时的备忘");
    assert_eq!(p.scope, "tool");
    assert_eq!(p.bound_tool.as_deref(), Some("codex"));
    assert_eq!(p.items.len(), 2);

    // 更新：改类型为 universal + 换 items
    database::update_preset(
        &id,
        "v2m1-改名",
        "新描述",
        "universal",
        None,
        &[("skill-v2m1-c".to_string(), "skill".to_string())],
    )
    .unwrap();
    let p2 = database::get_preset(&id).unwrap();
    assert_eq!(p2.name, "v2m1-改名");
    assert_eq!(p2.scope, "universal");
    assert!(p2.bound_tool.is_none());
    assert_eq!(p2.items.len(), 1);
    assert_eq!(p2.items[0].extension_id, "skill-v2m1-c");

    // 旧入口仍可用，默认 universal
    let legacy_id = database::create_preset("v2m1-旧入口", &items).unwrap();
    assert_eq!(database::get_preset(&legacy_id).unwrap().scope, "universal");

    // list_presets 返回扩展字段
    let listed = database::list_presets();
    assert!(listed.iter().any(|p| p.id == id && p.scope == "universal"));

    database::delete_preset(&id).unwrap();
    assert!(database::get_preset(&id).is_none());
}
```

注意：若 `dao_test.rs` 顶部没有 `mod support` / 已有公用 use，按文件现状对齐；`support::setup()` 的调用方式照抄同文件 `test_preset_crud`。

- [x] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test test_preset_v2_crud_roundtrip`
Expected: 编译失败——`create_preset_with_meta` 不存在

- [x] **Step 3: 实现 DAO**

`dao/preset.rs`：`PresetRecord` 扩 3 字段；`create_preset_with_meta` / `update_preset` / `get_preset` / `list_presets` 改列。关键新代码：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetRecord {
    pub id: String,
    pub name: String,
    pub description: String,
    pub scope: String, // "universal" | "tool"
    pub bound_tool: Option<String>,
    pub items: Vec<PresetItemRecord>,
}

pub fn create_preset_with_meta(
    name: &str,
    description: &str,
    scope: &str,
    bound_tool: Option<&str>,
    items: &[(String, String)],
) -> Result<String, String> {
    let conn = DB.lock().unwrap();
    let id = format!("preset-{}", chrono::Utc::now().timestamp_millis());
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO presets (id, name, description, scope, bound_tool, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, name, description, scope, bound_tool, now],
    )
    .map_err(|e| e.to_string())?;
    insert_items(&conn, &id, items)?;
    Ok(id)
}

/// 旧入口委托：默认通用预设（前端 M2 前的兼容层）
pub fn create_preset(name: &str, items: &[(String, String)]) -> Result<String, String> {
    create_preset_with_meta(name, "", "universal", None, items)
}

fn insert_items(conn: &rusqlite::Connection, id: &str, items: &[(String, String)]) -> Result<(), String> {
    for (ext_id, kind) in items {
        let item_id = format!("{}-{}", id, ext_id);
        conn.execute(
            "INSERT INTO preset_items (id, preset_id, extension_id, kind) VALUES (?1, ?2, ?3, ?4)",
            params![item_id, id, ext_id, kind],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn update_preset(
    id: &str,
    name: &str,
    description: &str,
    scope: &str,
    bound_tool: Option<&str>,
    items: &[(String, String)],
) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    let n = conn
        .execute(
            "UPDATE presets SET name=?2, description=?3, scope=?4, bound_tool=?5 WHERE id=?1",
            params![id, name, description, scope, bound_tool],
        )
        .map_err(|e| e.to_string())?;
    if n == 0 {
        return Err(format!("预设不存在: {}", id));
    }
    conn.execute("DELETE FROM preset_items WHERE preset_id = ?1", [id])
        .map_err(|e| e.to_string())?;
    insert_items(&conn, id, items)
}

pub fn get_preset(preset_id: &str) -> Option<PresetRecord> {
    let conn = DB.lock().unwrap();
    conn.query_row(
        "SELECT id, name, description, scope, bound_tool FROM presets WHERE id = ?1",
        [preset_id],
        |row| {
            Ok(PresetRecord {
                id: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                scope: row.get(3)?,
                bound_tool: row.get(4)?,
                items: Vec::new(),
            })
        },
    )
    .ok()
    .map(|mut p| {
        p.items = load_items(&conn, &p.id);
        p
    })
}

fn load_items(conn: &rusqlite::Connection, id: &str) -> Vec<PresetItemRecord> {
    conn.prepare(
        "SELECT pi.extension_id, pi.kind, e.name FROM preset_items pi LEFT JOIN extensions e ON pi.extension_id = e.id WHERE pi.preset_id = ?1",
    )
    .ok()
    .and_then(|mut stmt| {
        stmt.query_map([id], |row| {
            Ok(PresetItemRecord {
                extension_id: row.get(0)?,
                kind: row.get(1)?,
                extension_name: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            })
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
    })
    .unwrap_or_default()
}
```

`list_presets` 改为 `SELECT id, name, description, scope, bound_tool FROM presets ORDER BY created_at DESC`，组装 PresetRecord（items 用 `load_items`）。`database/mod.rs` re-export 增加 `create_preset_with_meta, get_preset, update_preset`。

- [x] **Step 4: 跑测试 + 全量**

Run: `cd src-tauri && cargo test test_preset_v2 && cargo test`
Expected: PASS；注意 `get_preset_items`（旧函数）被 Task 9 继续使用，勿删

- [x] **Step 5: Commit**

```bash
git add src-tauri/src/database/dao/preset.rs src-tauri/src/database/mod.rs src-tauri/tests/dao_test.rs
git commit -m "feat(preset-v2): preset DAO — description/scope/bound_tool + update/get/create_with_meta"
```

---

### Task 3: 专属绑定 + 常驻名单 DAO

**Files:**
- Create: `src-tauri/src/database/dao/resource_binding.rs`
- Create: `src-tauri/src/database/dao/tool_resident.rs`
- Modify: `src-tauri/src/database/dao/mod.rs`（注册模块——若该文件按目录自动聚合则确认写法一致）
- Modify: `src-tauri/src/database/mod.rs`（re-export）
- Test: `src-tauri/tests/dao_test.rs`（追加）

**Interfaces:**
- Produces:
  - `ResourceBindingRecord { extension_id, exclusive_tools: String /*逗号分隔，空=通用*/, reason: Option<String>, updated_at }`
  - `upsert_resource_binding(extension_id: &str, exclusive_tools: &str, reason: Option<&str>) -> Result<(), String>`
  - `get_resource_binding(extension_id: &str) -> Option<ResourceBindingRecord>`
  - `delete_resource_binding(extension_id: &str) -> Result<(), String>`
  - `list_resource_bindings() -> Vec<ResourceBindingRecord>`
  - `tool_allowed(extension_id: &str, tool_id: &str) -> bool`（无行或 exclusive_tools 空 → true）
  - `set_tool_resident(tool_id: &str, extension_id: &str, resident: bool) -> Result<(), String>`
  - `is_tool_resident(tool_id: &str, extension_id: &str) -> bool`
  - `list_tool_residents(tool_id: &str) -> Vec<String>`

- [x] **Step 1: 写失败测试**

`tests/dao_test.rs` 追加：

```rust
#[test]
fn test_resource_binding_and_resident_dao() {
    crate::support::setup();
    use multi_agents_manager_lib::database;

    // 默认无行 = 通用
    assert!(database::tool_allowed("skill-v2m1-x", "codex"));

    // 标记专属 codex + 原因
    database::upsert_resource_binding("skill-v2m1-x", "codex", Some("依赖 codex App + MCP"))
        .unwrap();
    assert!(database::tool_allowed("skill-v2m1-x", "codex"));
    assert!(!database::tool_allowed("skill-v2m1-x", "claude"));
    let b = database::get_resource_binding("skill-v2m1-x").unwrap();
    assert_eq!(b.exclusive_tools, "codex");
    assert_eq!(b.reason.as_deref(), Some("依赖 codex App + MCP"));

    // 多工具逗号分隔
    database::upsert_resource_binding("skill-v2m1-y", "codex,claude", None).unwrap();
    assert!(database::tool_allowed("skill-v2m1-y", "claude"));
    assert!(database::tool_allowed("skill-v2m1-y", "codex"));
    assert!(!database::tool_allowed("skill-v2m1-y", "opencode"));

    // 清空专属 = 回到通用
    database::upsert_resource_binding("skill-v2m1-x", "", None).unwrap();
    assert!(database::tool_allowed("skill-v2m1-x", "claude"));

    // 删除绑定
    database::delete_resource_binding("skill-v2m1-y").unwrap();
    assert!(database::get_resource_binding("skill-v2m1-y").is_none());

    // 常驻名单
    assert!(!database::is_tool_resident("codex", "skill-v2m1-x"));
    database::set_tool_resident("codex", "skill-v2m1-x", true).unwrap();
    assert!(database::is_tool_resident("codex", "skill-v2m1-x"));
    assert_eq!(
        database::list_tool_residents("codex"),
        vec!["skill-v2m1-x".to_string()]
    );
    // 关闭 = 删行
    database::set_tool_resident("codex", "skill-v2m1-x", false).unwrap();
    assert!(!database::is_tool_resident("codex", "skill-v2m1-x"));
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test test_resource_binding_and_resident_dao`
Expected: 编译失败——函数不存在

- [x] **Step 3: 实现 `dao/resource_binding.rs`**

```rust
// 专属绑定（spec §3.4/§4）：资源 → 允许工具列表（空 = 通用可迁移），手动为真值源
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::database::connection::DB;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceBindingRecord {
    pub extension_id: String,
    pub exclusive_tools: String,
    pub reason: Option<String>,
    pub updated_at: String,
}

pub fn upsert_resource_binding(
    extension_id: &str,
    exclusive_tools: &str,
    reason: Option<&str>,
) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR REPLACE INTO resource_bindings (extension_id, exclusive_tools, reason, updated_at) VALUES (?1, ?2, ?3, ?4)",
        params![extension_id, exclusive_tools, reason, now],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn get_resource_binding(extension_id: &str) -> Option<ResourceBindingRecord> {
    let conn = DB.lock().unwrap();
    conn.query_row(
        "SELECT extension_id, exclusive_tools, reason, updated_at FROM resource_bindings WHERE extension_id = ?1",
        [extension_id],
        |row| {
            Ok(ResourceBindingRecord {
                extension_id: row.get(0)?,
                exclusive_tools: row.get(1)?,
                reason: row.get(2)?,
                updated_at: row.get(3)?,
            })
        },
    )
    .ok()
}

pub fn delete_resource_binding(extension_id: &str) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    conn.execute("DELETE FROM resource_bindings WHERE extension_id = ?1", [extension_id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn list_resource_bindings() -> Vec<ResourceBindingRecord> {
    let conn = DB.lock().unwrap();
    conn.prepare("SELECT extension_id, exclusive_tools, reason, updated_at FROM resource_bindings")
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok(ResourceBindingRecord {
                    extension_id: row.get(0)?,
                    exclusive_tools: row.get(1)?,
                    reason: row.get(2)?,
                    updated_at: row.get(3)?,
                })
            })
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default()
}

/// 工具是否可用该资源：无绑定或绑定列表为空 → 通用放行
pub fn tool_allowed(extension_id: &str, tool_id: &str) -> bool {
    match get_resource_binding(extension_id) {
        None => true,
        Some(b) => {
            let list = b.exclusive_tools.trim();
            list.is_empty() || list.split(',').any(|t| t.trim() == tool_id)
        }
    }
}
```

- [x] **Step 4: 实现 `dao/tool_resident.rs`**

```rust
// 常驻名单（spec §3.4）：工具 × 资源豁免独占清扫；原生 MCP 段/自装插件定义上即常驻，不进本表
use rusqlite::params;

use crate::database::connection::DB;

pub fn set_tool_resident(tool_id: &str, extension_id: &str, resident: bool) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    if resident {
        conn.execute(
            "INSERT OR IGNORE INTO tool_residents (tool_id, extension_id) VALUES (?1, ?2)",
            params![tool_id, extension_id],
        )
        .map_err(|e| e.to_string())?;
    } else {
        conn.execute(
            "DELETE FROM tool_residents WHERE tool_id = ?1 AND extension_id = ?2",
            params![tool_id, extension_id],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn is_tool_resident(tool_id: &str, extension_id: &str) -> bool {
    let conn = DB.lock().unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM tool_residents WHERE tool_id = ?1 AND extension_id = ?2",
        params![tool_id, extension_id],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

pub fn list_tool_residents(tool_id: &str) -> Vec<String> {
    let conn = DB.lock().unwrap();
    conn.prepare("SELECT extension_id FROM tool_residents WHERE tool_id = ?1")
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([tool_id], |row| row.get(0))
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default()
}
```

`dao/mod.rs` 加 `pub mod resource_binding; pub mod tool_resident;`；`database/mod.rs` re-export：`pub use dao::resource_binding::{delete_resource_binding, get_resource_binding, list_resource_bindings, tool_allowed, upsert_resource_binding, ResourceBindingRecord};` 与 `pub use dao::tool_resident::{is_tool_resident, list_tool_residents, set_tool_resident};`

- [x] **Step 5: 跑测试 + Commit**

Run: `cd src-tauri && cargo test test_resource_binding && cargo test`
Expected: PASS

```bash
git add src-tauri/src/database/dao/ src-tauri/src/database/mod.rs src-tauri/tests/dao_test.rs
git commit -m "feat(preset-v2): resource binding + tool resident DAO"
```

---

### Task 4: 基底快照 + 暂存日志 DAO

**Files:**
- Create: `src-tauri/src/database/dao/base_snapshot.rs`
- Create: `src-tauri/src/database/dao/stash.rs`
- Modify: `src-tauri/src/database/dao/mod.rs`、`src-tauri/src/database/mod.rs`
- Test: `src-tauri/tests/dao_test.rs`（追加）

**Interfaces:**
- Produces:
  - `BaseSnapshotItemRecord { extension_id, kind, origin }`（origin: `"mam" | "native"`）
  - `save_base_snapshot(tool_id, active_preset_id: Option<&str>, items: &[BaseSnapshotItemRecord]) -> Result<(), String>`（幂等：先删后插）
  - `get_base_snapshot(tool_id) -> Option<(Option<String> /*active_preset_id*/, Vec<BaseSnapshotItemRecord>)>`
  - `set_active_preset(tool_id, preset_id: &str) -> Result<(), String>`
  - `destroy_base_snapshot(tool_id) -> Result<(), String>`
  - `StashEntryRecord { id: i64, tool_id, skill_name, stashed_path, original_path, created_at, restored_at: Option<String> }`
  - `record_stash(tool_id, skill_name, stashed_path: &str, original_path: &str) -> Result<i64, String>`
  - `mark_stash_restored(id: i64) -> Result<(), String>`
  - `unrestored_stash(tool_id: Option<&str>) -> Vec<StashEntryRecord>`（None = 全部工具，孤儿扫描用）

- [x] **Step 1: 写失败测试**

`tests/dao_test.rs` 追加：

```rust
#[test]
fn test_base_snapshot_and_stash_dao() {
    crate::support::setup();
    use multi_agents_manager_lib::database::{BaseSnapshotItemRecord, StashEntryRecord};

    let items = vec![
        BaseSnapshotItemRecord {
            extension_id: "skill-v2m1-a".into(),
            kind: "skill".into(),
            origin: "mam".into(),
        },
        BaseSnapshotItemRecord {
            extension_id: "skill-v2m1-native".into(),
            kind: "skill".into(),
            origin: "native".into(),
        },
    ];
    // 无快照 → None
    assert!(multi_agents_manager_lib::database::get_base_snapshot("codex").is_none());
    // 保存（拍基底，active=None）
    multi_agents_manager_lib::database::save_base_snapshot("codex", None, &items).unwrap();
    let (active, got) =
        multi_agents_manager_lib::database::get_base_snapshot("codex").unwrap();
    assert!(active.is_none());
    assert_eq!(got.len(), 2);
    // 幂等重存（会话内切换不动快照；重存=覆盖）
    multi_agents_manager_lib::database::save_base_snapshot("codex", None, &items).unwrap();
    assert_eq!(
        multi_agents_manager_lib::database::get_base_snapshot("codex").unwrap().1.len(),
        2
    );
    // 设激活 + 读回
    multi_agents_manager_lib::database::set_active_preset("codex", "preset-v2m1").unwrap();
    assert_eq!(
        multi_agents_manager_lib::database::get_base_snapshot("codex").unwrap().0,
        Some("preset-v2m1".into())
    );
    // 销毁（恢复默认后快照不复存在——会话级生命周期）
    multi_agents_manager_lib::database::destroy_base_snapshot("codex").unwrap();
    assert!(multi_agents_manager_lib::database::get_base_snapshot("codex").is_none());

    // 暂存日志
    let id = multi_agents_manager_lib::database::record_stash(
        "codex",
        "v2m1-native",
        "/tmp/stash/v2m1-native",
        "/home/u/.codex/skills/v2m1-native",
    )
    .unwrap();
    let entries = multi_agents_manager_lib::database::unrestored_stash(Some("codex"));
    assert_eq!(entries.len(), 1);
    let e: &StashEntryRecord = &entries[0];
    assert_eq!(e.skill_name, "v2m1-native");
    assert!(e.restored_at.is_none());
    // 全工具扫描（孤儿恢复用）
    assert!(!multi_agents_manager_lib::database::unrestored_stash(None).is_empty());
    // 标记恢复后不再出现
    multi_agents_manager_lib::database::mark_stash_restored(id).unwrap();
    assert!(multi_agents_manager_lib::database::unrestored_stash(Some("codex")).is_empty());
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test test_base_snapshot_and_stash_dao`
Expected: 编译失败

- [x] **Step 3: 实现 `dao/base_snapshot.rs`**

```rust
// 基底快照（spec §3.2/§4）：生命周期 = 一次预设会话；恢复默认时销毁
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::database::connection::DB;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BaseSnapshotItemRecord {
    pub extension_id: String,
    pub kind: String,
    pub origin: String, // "mam" | "native"
}

/// 幂等保存：先删该工具旧快照再插入（拍基底/重存同一路径）
pub fn save_base_snapshot(
    tool_id: &str,
    active_preset_id: Option<&str>,
    items: &[BaseSnapshotItemRecord],
) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute("DELETE FROM tool_base_snapshot_items WHERE tool_id = ?1", [tool_id])
        .map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT OR REPLACE INTO tool_base_snapshots (tool_id, active_preset_id, created_at) VALUES (?1, ?2, ?3)",
        params![tool_id, active_preset_id, now],
    )
    .map_err(|e| e.to_string())?;
    for it in items {
        conn.execute(
            "INSERT INTO tool_base_snapshot_items (tool_id, extension_id, kind, origin) VALUES (?1, ?2, ?3, ?4)",
            params![tool_id, it.extension_id, it.kind, it.origin],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn get_base_snapshot(tool_id: &str) -> Option<(Option<String>, Vec<BaseSnapshotItemRecord>)> {
    let conn = DB.lock().unwrap();
    let active: Option<String> = conn
        .query_row(
            "SELECT active_preset_id FROM tool_base_snapshots WHERE tool_id = ?1",
            [tool_id],
            |r| r.get(0),
        )
        .ok()?;
    let items = conn
        .prepare("SELECT extension_id, kind, origin FROM tool_base_snapshot_items WHERE tool_id = ?1")
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([tool_id], |row| {
                Ok(BaseSnapshotItemRecord {
                    extension_id: row.get(0)?,
                    kind: row.get(1)?,
                    origin: row.get(2)?,
                })
            })
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    Some((active, items))
}

pub fn set_active_preset(tool_id: &str, preset_id: &str) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    conn.execute(
        "UPDATE tool_base_snapshots SET active_preset_id = ?2 WHERE tool_id = ?1",
        params![tool_id, preset_id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn destroy_base_snapshot(tool_id: &str) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    conn.execute("DELETE FROM tool_base_snapshot_items WHERE tool_id = ?1", [tool_id])
        .map_err(|e| e.to_string())?;
    conn.execute("DELETE FROM tool_base_snapshots WHERE tool_id = ?1", [tool_id])
        .map_err(|e| e.to_string())?;
    Ok(())
}
```

- [x] **Step 4: 实现 `dao/stash.rs`**

```rust
// 暂存日志（spec §3.4/§5.3）：暂存区唯一账本，崩溃恢复依据
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::database::connection::DB;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StashEntryRecord {
    pub id: i64,
    pub tool_id: String,
    pub skill_name: String,
    pub stashed_path: String,
    pub original_path: String,
    pub created_at: String,
    pub restored_at: Option<String>,
}

pub fn record_stash(
    tool_id: &str,
    skill_name: &str,
    stashed_path: &str,
    original_path: &str,
) -> Result<i64, String> {
    let conn = DB.lock().unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO stash_journal (tool_id, skill_name, stashed_path, original_path, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![tool_id, skill_name, stashed_path, original_path, now],
    )
    .map_err(|e| e.to_string())?;
    Ok(conn.last_insert_rowid())
}

pub fn mark_stash_restored(id: i64) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE stash_journal SET restored_at = ?2 WHERE id = ?1",
        params![id, now],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 未恢复条目；tool_id = None 时跨全部工具（启动孤儿扫描）
pub fn unrestored_stash(tool_id: Option<&str>) -> Vec<StashEntryRecord> {
    let conn = DB.lock().unwrap();
    let sql = match tool_id {
        Some(_) => "SELECT id, tool_id, skill_name, stashed_path, original_path, created_at, restored_at FROM stash_journal WHERE restored_at IS NULL AND tool_id = ?1",
        None => "SELECT id, tool_id, skill_name, stashed_path, original_path, created_at, restored_at FROM stash_journal WHERE restored_at IS NULL",
    };
    conn.prepare(sql)
        .ok()
        .and_then(|mut stmt| {
            let map_row = |row: &rusqlite::Row| {
                Ok(StashEntryRecord {
                    id: row.get(0)?,
                    tool_id: row.get(1)?,
                    skill_name: row.get(2)?,
                    stashed_path: row.get(3)?,
                    original_path: row.get(4)?,
                    created_at: row.get(5)?,
                    restored_at: row.get(6)?,
                })
            };
            let rows = match tool_id {
                Some(t) => stmt.query_map([t], map_row),
                None => stmt.query_map([], map_row),
            };
            rows.ok().map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default()
}
```

`dao/mod.rs` 注册两模块；`database/mod.rs` re-export 上述全部函数与两个 Record 类型。

- [x] **Step 5: 跑测试 + Commit**

Run: `cd src-tauri && cargo test test_base_snapshot && cargo test`
Expected: PASS

```bash
git add src-tauri/src/database/dao/ src-tauri/src/database/mod.rs src-tauri/tests/dao_test.rs
git commit -m "feat(preset-v2): base snapshot + stash journal DAO"
```

---

### Task 5: 兼容检查切换到 resource_bindings（P2② 修复）

**Files:**
- Modify: `src-tauri/src/services/preset/mod.rs:241-284`（`check_compatibility` 重写，删 tags 逻辑）
- Test: `src-tauri/tests/preset_v2_test.rs`（新建集成测试文件）

**Interfaces:**
- Consumes: Task 3 的 `tool_allowed`
- Produces: `check_compatibility(preset_id, tool_id) -> CompatibilityReport` 签名不变（`commands/resource.rs:297` 调用方无感）；行为变化：判定源从 `extensions.tags` 换成 `resource_bindings`

- [x] **Step 1: 写失败测试**

新建 `src-tauri/tests/preset_v2_test.rs`：

```rust
mod support; // 若 tests/ 下 support.rs 非共享 mod，按 dao_test.rs 的引用方式对齐

/// P2②：兼容判定改查 resource_bindings——claude 来源的 skill 不再对 codex 误报不兼容
#[test]
fn check_compatibility_uses_bindings_not_tags() {
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::check_compatibility;

    // 造一个 tags="claude"（来源工具）的 skill —— 旧逻辑会误判 codex 不兼容
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m1-compat".into(),
        kind: "skill".into(),
        name: "v2m1-compat".into(),
        description: None,
        source_path: "/tmp/x".into(),
        source_url: None,
        version: None,
        tags: Some("claude".into()),
        suite: None,
        source_tool: Some("claude".into()),
        is_native: false,
    })
    .unwrap();
    let pid = database::create_preset(
        "v2m1-compat-preset",
        &[("skill-v2m1-compat".into(), "skill".into())],
    )
    .unwrap();

    // 无绑定 → 通用，codex 兼容（旧逻辑这里是 incompatible）
    let report = check_compatibility(&pid, "codex");
    assert_eq!(report.compatible.len(), 1, "tags=claude 不应再挡 codex: {:?}", report.incompatible);
    assert!(report.incompatible.is_empty());

    // 绑定 codex 专属后 → claude 不兼容且带原因
    database::upsert_resource_binding("skill-v2m1-compat", "codex", Some("依赖 codex App"))
        .unwrap();
    let report = check_compatibility(&pid, "claude");
    assert_eq!(report.incompatible.len(), 1);
    assert_eq!(report.incompatible[0].id, "skill-v2m1-compat");
    assert!(report.incompatible[0].reason.contains("codex"));
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test check_compatibility_uses_bindings`
Expected: FAIL——旧 tags 逻辑把 `tags="claude"` 判为 claude 专属，codex 走 incompatible

- [x] **Step 3: 重写 `check_compatibility`**

`services/preset/mod.rs` 中 `check_compatibility` 的判定段（原 257-262 行的 tags 分支）替换为：

```rust
        // 兼容判定（spec §6）：真值源是 resource_bindings（手动标记），
        // extensions.tags 不再参与（其语义是来源工具/插件子类型，历史误用）
        let is_compatible = crate::database::tool_allowed(&ext_id, tool_id);
```

`incompatible` 的 reason 改为携带绑定信息：

```rust
        if is_compatible {
            compatible.push(CompatibleItem { id: ext_id.clone(), name, kind });
        } else {
            let bound = crate::database::get_resource_binding(&ext_id)
                .map(|b| b.exclusive_tools)
                .unwrap_or_default();
            incompatible.push(IncompatibleItem {
                id: ext_id,
                name,
                kind,
                reason: format!("专属 {}，不支持 {}", bound, tool_id),
            });
        }
```

- [x] **Step 4: 跑测试 + 全量 + Commit**

Run: `cd src-tauri && cargo test check_compatibility && cargo test`
Expected: PASS

```bash
git add src-tauri/src/services/preset/mod.rs src-tauri/tests/preset_v2_test.rs
git commit -m "fix(preset-v2): compatibility check reads resource_bindings, retire tags misuse (P2)"
```

---

### Task 6: 暂存区引擎 stash

**Files:**
- Create: `src-tauri/src/services/preset/stash.rs`
- Modify: `src-tauri/src/services/preset/mod.rs`（顶部加 `pub mod stash;`）
- Test: `src-tauri/tests/preset_v2_test.rs`（追加）

**Interfaces:**
- Consumes: Task 4 的 `record_stash / mark_stash_restored / unrestored_stash / StashEntryRecord`
- Produces:
  - `stash_dir(tool_id: &str) -> std::path::PathBuf`（`~/.mam/stash/<tool>/skills`）
  - `stash_native_skill(tool_id: &str, skill_name: &str, original_path: &Path) -> Result<(), String>`
  - `restore_stashed_skill(entry: &StashEntryRecord) -> Result<(), String>`（原位被占 → Err，不覆盖）
  - `restore_all_for_tool(tool_id: &str) -> (Vec<String> /*restored*/, Vec<String> /*conflicts: "name: 原因"*/)`
  - `recover_orphans() -> usize`（启动钩子，Task 12 接线）

- [x] **Step 1: 写失败测试**

`tests/preset_v2_test.rs` 追加：

```rust
use std::path::PathBuf;

/// 暂存往返：真目录移入 ~/.mam/stash/<tool>/skills 再移回；账本同步
#[test]
fn stash_and_restore_roundtrip() {
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::stash;

    // 工具原生技能目录（fake HOME 下）
    let tool_dir = dirs::home_dir().unwrap().join(".codex/skills");
    std::fs::create_dir_all(tool_dir.join("v2m1-native-a")).unwrap();
    std::fs::write(tool_dir.join("v2m1-native-a/SKILL.md"), "hi").unwrap();

    // 暂存
    stash::stash_native_skill("codex", "v2m1-native-a", &tool_dir.join("v2m1-native-a")).unwrap();
    assert!(!tool_dir.join("v2m1-native-a").exists(), "工具目录应读不到");
    let stashed = stash::stash_dir("codex").join("v2m1-native-a");
    assert!(stashed.is_dir(), "暂存区应有该目录");
    assert_eq!(database::unrestored_stash(Some("codex")).len(), 1);

    // 回移
    let (restored, conflicts) = stash::restore_all_for_tool("codex");
    assert_eq!(restored, vec!["v2m1-native-a".to_string()]);
    assert!(conflicts.is_empty());
    assert!(tool_dir.join("v2m1-native-a/SKILL.md").exists());
    assert!(database::unrestored_stash(Some("codex")).is_empty());
    assert!(!stashed.exists(), "暂存区应清空");
}

/// 原位被占：不覆盖，留在暂存区，报告冲突
#[test]
fn stash_restore_conflict_keeps_stash() {
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::stash;

    let tool_dir = dirs::home_dir().unwrap().join(".codex/skills");
    std::fs::create_dir_all(tool_dir.join("v2m1-native-b")).unwrap();
    std::fs::write(tool_dir.join("v2m1-native-b/SKILL.md"), "origin").unwrap();
    stash::stash_native_skill("codex", "v2m1-native-b", &tool_dir.join("v2m1-native-b")).unwrap();

    // 原位被别的目录占了
    std::fs::create_dir_all(tool_dir.join("v2m1-native-b")).unwrap();
    std::fs::write(tool_dir.join("v2m1-native-b/SKILL.md"), "intruder").unwrap();

    let (restored, conflicts) = stash::restore_all_for_tool("codex");
    assert!(restored.is_empty());
    assert_eq!(conflicts.len(), 1);
    assert!(conflicts[0].contains("v2m1-native-b"));
    // 不覆盖 + 暂存保留 + 账本未消
    assert_eq!(std::fs::read_to_string(tool_dir.join("v2m1-native-b/SKILL.md")).unwrap(), "intruder");
    assert!(stash::stash_dir("codex").join("v2m1-native-b").exists());
    assert_eq!(database::unrestored_stash(Some("codex")).len(), 1);

    // 人工清场后孤儿恢复可补刀
    std::fs::remove_dir_all(tool_dir.join("v2m1-native-b")).unwrap();
    let n = stash::recover_orphans();
    assert!(n >= 1);
    assert!(tool_dir.join("v2m1-native-b/SKILL.md").exists());
    assert!(database::unrestored_stash(None).iter().all(|e| e.skill_name != "v2m1-native-b"));
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test stash_`
Expected: 编译失败——`stash` 模块不存在

- [x] **Step 3: 实现 `services/preset/stash.rs`**

先在 `src-tauri/Cargo.toml` 的 `[dev-dependencies]`（87 行附近）补一行 `dirs = "5.0"`（与主依赖同版本）——本计划集成测试直接取家目录，而 `dirs` 此前只在 `[dependencies]`，集成测试 crate 引用不到。

```rust
// 暂存区引擎（spec §3.1/§5.3）：原生技能目录整体移动（同盘 rename，零拷贝），
// 非压缩包；stash_journal 是唯一账本，崩溃后凭账本恢复
use std::path::Path;

use crate::database::{self, StashEntryRecord};

/// 暂存区：~/.mam/stash/<tool>/skills
pub fn stash_dir(tool_id: &str) -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".mam")
        .join("stash")
        .join(tool_id)
        .join("skills")
}

/// 把原生技能目录移入暂存区。先移动后记账；记账失败回滚移动（不留无账暂存）
pub fn stash_native_skill(tool_id: &str, skill_name: &str, original_path: &Path) -> Result<(), String> {
    if !original_path.is_dir() {
        return Err(format!("原生技能目录不存在: {}", original_path.display()));
    }
    let dir = stash_dir(tool_id);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stashed = dir.join(skill_name);
    if stashed.exists() {
        return Err(format!("暂存区已有同名项: {}", stashed.display()));
    }
    std::fs::rename(original_path, &stashed).map_err(|e| e.to_string())?;
    if let Err(e) = database::record_stash(tool_id, skill_name, &stashed.to_string_lossy(), &original_path.to_string_lossy()) {
        // 回滚移动，宁可不暂存也不留无账目录
        let _ = std::fs::rename(&stashed, original_path);
        return Err(format!("暂存记账失败已回滚: {}", e));
    }
    log::info!("原生技能 {} 已暂存: {}", skill_name, stashed.display());
    Ok(())
}

/// 回移单条。原位被占（同名且存在）→ Err（不覆盖，调用方留待人工处理）
pub fn restore_stashed_skill(entry: &StashEntryRecord) -> Result<(), String> {
    let stashed = Path::new(&entry.stashed_path);
    let original = Path::new(&entry.original_path);
    if !stashed.exists() {
        return Err(format!("暂存区已无此项: {}", stashed.display()));
    }
    if original.exists() {
        return Err(format!("原位已被占用: {}", original.display()));
    }
    if let Some(parent) = original.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::rename(stashed, original).map_err(|e| e.to_string())?;
    database::mark_stash_restored(entry.id)?;
    log::info!("暂存技能 {} 已回移 {}", entry.skill_name, original.display());
    Ok(())
}

/// 回移该工具全部未恢复项；冲突项不覆盖、留在暂存区并报 "name: 原因"
pub fn restore_all_for_tool(tool_id: &str) -> (Vec<String>, Vec<String>) {
    let mut restored = Vec::new();
    let mut conflicts = Vec::new();
    for entry in database::unrestored_stash(Some(tool_id)) {
        match restore_stashed_skill(&entry) {
            Ok(()) => restored.push(entry.skill_name),
            Err(e) => conflicts.push(format!("{}: {}", entry.skill_name, e)),
        }
    }
    (restored, conflicts)
}

/// 启动孤儿恢复（spec §5.3）：全工具扫未恢复账目，暂存文件在且原位空 → 回移；
/// 返回成功恢复条数。冲突项保持暂存并 log（M2 UI 列出人工处理）
pub fn recover_orphans() -> usize {
    let mut n = 0;
    for entry in database::unrestored_stash(None) {
        match restore_stashed_skill(&entry) {
            Ok(()) => n += 1,
            Err(e) => log::warn!("孤儿暂存 {} 未恢复: {}", entry.skill_name, e),
        }
    }
    n
}
```

`services/preset/mod.rs` 顶部（`use` 之前）加 `pub mod stash;`。

- [x] **Step 4: 跑测试 + Commit**

Run: `cd src-tauri && cargo test stash_ && cargo test`
Expected: PASS

```bash
git add src-tauri/src/services/preset/ src-tauri/tests/preset_v2_test.rs
git commit -m "feat(preset-v2): native skill stash engine with journal + orphan recovery"
```

---

### Task 7: 状态扫描与基底拍快照

**Files:**
- Create: `src-tauri/src/services/preset/snapshot.rs`
- Modify: `src-tauri/src/services/preset/mod.rs`（加 `pub mod snapshot;`）
- Test: `src-tauri/tests/preset_v2_test.rs`（追加）

**Interfaces:**
- Produces:
  - `scan_tool_state(tool_id: &str) -> Vec<BaseSnapshotItemRecord>`——MAM 项来自 `list_assignments(tool_id)` 的 enabled 行（kind 按 id 前缀 `skill-`/`mcp-`/`plugin-`），原生项来自 `primary_skill_dir` 下的真目录（非 symlink 且非链接穿透），`origin = "mam" | "native"`
  - `capture_base_snapshot(tool_id: &str) -> Result<(), String>`——scan + `save_base_snapshot(tool_id, None, items)`

- [x] **Step 1: 写失败测试**

`tests/preset_v2_test.rs` 追加：

```rust
/// 状态扫描：MAM 启用项 + 原生真目录都要进基底；链接不重复计为原生
#[test]
fn scan_tool_state_captures_mam_and_native() {
    support::setup();
    use multi_agents_manager_lib::database::{self, BaseSnapshotItemRecord};
    use multi_agents_manager_lib::services::preset::snapshot;
    use multi_agents_manager_lib::services::{disable_skill_for_tool, enable_skill_for_tool};

    // SSOT 造一个 MAM skill 并为 claude 启用（建链接）
    let repo = database::list_extensions; // 引用防未用告警（可删）
    let _ = repo;
    let ssot = dirs::home_dir().unwrap().join(".mam/skills/v2m1-scan-a");
    std::fs::create_dir_all(&ssot).unwrap();
    std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m1-scan-a".into(),
        kind: "skill".into(),
        name: "v2m1-scan-a".into(),
        description: None,
        source_path: ssot.to_string_lossy().to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: None,
        is_native: false,
    })
    .unwrap();
    enable_skill_for_tool("v2m1-scan-a", "claude").unwrap();
    // 子 Agent 分配行不得让同一 ext_id 重复入基底（快照 PK 冲突防护）
    database::upsert_assignment_with_subagent("skill-v2m1-scan-a", "claude", "v2m1-scan-sub", true, "valid")
        .unwrap();

    // claude 目录再放一个原生真目录
    let claude_dir = dirs::home_dir().unwrap().join(".claude/skills");
    std::fs::create_dir_all(claude_dir.join("v2m1-scan-native")).unwrap();
    std::fs::write(claude_dir.join("v2m1-scan-native/SKILL.md"), "y").unwrap();

    let state = snapshot::scan_tool_state("claude");
    let find = |id: &str| state.iter().find(|i| i.extension_id == id);

    let mam = find("skill-v2m1-scan-a").expect("MAM 启用项应入基底");
    assert_eq!(mam.origin, "mam");
    assert_eq!(mam.kind, "skill");
    assert_eq!(
        state.iter().filter(|i| i.extension_id == "skill-v2m1-scan-a").count(),
        1,
        "子 Agent 分配行不得让同一 ext_id 重复计入"
    );
    let native = find("skill-v2m1-scan-native").expect("原生真目录应入基底");
    assert_eq!(native.origin, "native");

    // 拍快照 → 可读回
    snapshot::capture_base_snapshot("claude").unwrap();
    let (active, items) = database::get_base_snapshot("claude").unwrap();
    assert!(active.is_none());
    assert!(items.iter().any(|i| i.extension_id == "skill-v2m1-scan-a" && i.origin == "mam"));
    assert!(items.iter().any(|i| i.extension_id == "skill-v2m1-scan-native" && i.origin == "native"));

    // 清场，避免影响其他测试
    let _ = database::disable_subagent_assignment("skill-v2m1-scan-a", "claude", "v2m1-scan-sub");
    disable_skill_for_tool("v2m1-scan-a", "claude").unwrap();
    database::destroy_base_snapshot("claude").unwrap();
}
```

注意：测试里的 `let repo = database::list_extensions;` 是防告警的丑写法——实现时直接删掉这两行。

- [x] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test scan_tool_state`
Expected: 编译失败

- [x] **Step 3: 实现 `services/preset/snapshot.rs`**

```rust
// 状态扫描与基底拍快照（spec §3.2/§5.1 步骤3）：拍快照先于任何清扫动作
use crate::database::{self, BaseSnapshotItemRecord};

/// 扫描工具当前完整资源状态：MAM 启用项（assignment）+ 原生真目录（skill 目录里的非链接目录）
pub fn scan_tool_state(tool_id: &str) -> Vec<BaseSnapshotItemRecord> {
    let mut items = Vec::new();

    // 1) MAM 管理项：enabled assignment（id 前缀即 kind，全库约定）。
    //    只记工具级行——list_assignments 返回含子 Agent 行（同一 ext_id 两行：
    //    工具级 + sub_agent 级，见 dao/extension.rs:71-91），不过滤会对快照
    //    PK (tool_id, extension_id) 二次插入报 UNIQUE 冲突；子 Agent 链接由
    //    工具级启停级联清理，恢复时单独重建（restore_tool 的子 Agent 重建段）
    for a in database::list_assignments(tool_id) {
        if !a.enabled || a.sub_agent_id.is_some() {
            continue;
        }
        let kind = if a.extension_id.starts_with("skill-") {
            "skill"
        } else if a.extension_id.starts_with("mcp-") {
            "mcp"
        } else if a.extension_id.starts_with("plugin-") {
            "plugin"
        } else {
            continue;
        };
        items.push(BaseSnapshotItemRecord {
            extension_id: a.extension_id,
            kind: kind.to_string(),
            origin: "mam".to_string(),
        });
    }

    // 2) 原生技能：主 skill 目录下的真目录（非符号链接）。MAM 启用项在目录里是链接，
    //    与真目录天然不重叠；链接穿透套件（父目录是链接）不在此层出现
    if let Some(dir) = crate::adapter::primary_skill_dir(tool_id) {
        if dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for e in entries.flatten() {
                    let path = e.path();
                    // 只认真目录：符号链接是 MAM（或用户外链），不暂存
                    if path.is_dir() && !path.is_symlink() {
                        if let Some(name) = e.file_name().to_str() {
                            // 子 Agent 布局目录（subagents/）不是技能，跳过
                            if name == "subagents" {
                                continue;
                            }
                            items.push(BaseSnapshotItemRecord {
                                extension_id: format!("skill-{}", name),
                                kind: "skill".to_string(),
                                origin: "native".to_string(),
                            });
                        }
                    }
                }
            }
        }
    }

    items
}

/// 拍基底快照（active=None；设激活由应用流程负责）
pub fn capture_base_snapshot(tool_id: &str) -> Result<(), String> {
    let items = scan_tool_state(tool_id);
    database::save_base_snapshot(tool_id, None, &items)
}
```

- [x] **Step 4: 跑测试 + Commit**

Run: `cd src-tauri && cargo test scan_tool_state && cargo test`
Expected: PASS

```bash
git add src-tauri/src/services/preset/ src-tauri/tests/preset_v2_test.rs
git commit -m "feat(preset-v2): tool state scan + base snapshot capture"
```

---

### Task 8: 独占清扫 sweep

**Files:**
- Create: `src-tauri/src/services/preset/sweep.rs`
- Modify: `src-tauri/src/services/preset/mod.rs`（加 `pub mod sweep;`）
- Test: `src-tauri/tests/preset_v2_test.rs`（追加）

**Interfaces:**
- Consumes: Task 3 `is_tool_resident`、Task 6 `stash_native_skill`、Task 7 `scan_tool_state`
- Produces:
  - `SweepPlan { disable_mam: Vec<(String, String)>, stash_native: Vec<String> }`
  - `plan_sweep(tool_id: &str, keep: &[(String, String)]) -> SweepPlan`——keep 为（过滤后的）预设项 `(extension_id, kind)`；差集 = 当前状态 − keep − 常驻
  - `execute_sweep(tool_id: &str, plan: &SweepPlan) -> (Vec<String> /*disabled: ext_id*/, Vec<String> /*stashed: skill 名*/, Vec<String> /*failures*/)`

- [x] **Step 1: 写失败测试**

`tests/preset_v2_test.rs` 追加：

```rust
/// 清扫差集：预设外的 MAM skill 停用、原生真目录暂存；常驻豁免两项都不动
#[test]
fn sweep_stashes_native_and_disables_mam_except_resident() {
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::{snapshot, sweep};
    use multi_agents_manager_lib::services::{disable_skill_for_tool, enable_skill_for_tool};

    // MAM skill A（预设内）+ MAM skill B（预设外）为 claude 启用
    for name in ["v2m1-sw-a", "v2m1-sw-b"] {
        let ssot = dirs::home_dir().unwrap().join(".mam/skills").join(name);
        std::fs::create_dir_all(&ssot).unwrap();
        std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
        database::insert_extension(&database::ExtensionRecord {
            id: format!("skill-{}", name),
            kind: "skill".into(),
            name: name.into(),
            description: None,
            source_path: ssot.to_string_lossy().to_string(),
            source_url: None,
            version: None,
            tags: None,
            suite: None,
            source_tool: None,
            is_native: false,
        })
        .unwrap();
        enable_skill_for_tool(name, "claude").unwrap();
    }
    // 原生真目录 C（预设外）与 D（常驻）
    let claude_dir = dirs::home_dir().unwrap().join(".claude/skills");
    for name in ["v2m1-sw-c", "v2m1-sw-d"] {
        std::fs::create_dir_all(claude_dir.join(name)).unwrap();
        std::fs::write(claude_dir.join(name).join("SKILL.md"), "n").unwrap();
    }
    database::set_tool_resident("claude", "skill-v2m1-sw-d", true).unwrap();

    // 计划：keep 只有 A（断言用 contains——集成测试共享 HOME，其他测试可能残留原生目录）
    let keep = vec![("skill-v2m1-sw-a".to_string(), "skill".to_string())];
    let plan = sweep::plan_sweep("claude", &keep);
    assert!(plan.disable_mam.contains(&("skill-v2m1-sw-b".to_string(), "skill".to_string())));
    assert!(plan.stash_native.contains(&"v2m1-sw-c".to_string()));
    assert!(!plan.stash_native.contains(&"v2m1-sw-d".to_string()), "常驻项不得进暂存计划");

    // 执行：B 断链、C 暂存、D 不动
    let (disabled, stashed, failures) = sweep::execute_sweep("claude", &plan);
    assert!(disabled.contains(&"skill-v2m1-sw-b".to_string()));
    assert!(stashed.contains(&"v2m1-sw-c".to_string()));
    assert!(failures.is_empty(), "{:?}", failures);
    assert!(!claude_dir.join("v2m1-sw-b").exists(), "B 链接应已断");
    assert!(!claude_dir.join("v2m1-sw-c").exists(), "C 应已暂存");
    assert!(claude_dir.join("v2m1-sw-d").is_dir(), "常驻 D 不得动");

    // 清场
    database::set_tool_resident("claude", "skill-v2m1-sw-d", false).unwrap();
    disable_skill_for_tool("v2m1-sw-a", "claude").unwrap();
    let _ = database::destroy_base_snapshot("claude");
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test sweep_`
Expected: 编译失败

- [x] **Step 3: 实现 `services/preset/sweep.rs`**

```rust
// 独占清扫（spec §5.1 步骤4）：差集 = 当前生效资源 − 预设项 − 常驻；
// MAM 资源走既有停用服务（含 Layer3 级联），原生技能走暂存引擎
use crate::database;

#[derive(Debug, Default)]
pub struct SweepPlan {
    /// 要停用的 MAM 资源 (extension_id, kind)
    pub disable_mam: Vec<(String, String)>,
    /// 要暂存的原生技能名（目录名，不带 skill- 前缀）
    pub stash_native: Vec<String>,
}

pub fn plan_sweep(tool_id: &str, keep: &[(String, String)]) -> SweepPlan {
    let mut plan = SweepPlan::default();
    let keep_ids: Vec<&str> = keep.iter().map(|(id, _)| id.as_str()).collect();
    for item in super::snapshot::scan_tool_state(tool_id) {
        if keep_ids.contains(&item.extension_id.as_str()) {
            continue;
        }
        if database::is_tool_resident(tool_id, &item.extension_id) {
            continue;
        }
        if item.origin == "mam" {
            plan.disable_mam.push((item.extension_id, item.kind));
        } else {
            // native 项 extension_id = "skill-<name>"
            let name = item.extension_id.strip_prefix("skill-").unwrap_or(&item.extension_id);
            plan.stash_native.push(name.to_string());
        }
    }
    plan
}

/// 执行清扫：逐项 best-effort，失败进 failures 不阻断（spec §9，FR-6.32 部分成功）
pub fn execute_sweep(
    tool_id: &str,
    plan: &SweepPlan,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut disabled = Vec::new();
    let mut stashed = Vec::new();
    let mut failures = Vec::new();

    for (ext_id, kind) in &plan.disable_mam {
        let name = ext_id
            .strip_prefix(&format!("{}-", kind))
            .unwrap_or(ext_id);
        let result = match kind.as_str() {
            "skill" => crate::services::disable_skill_for_tool(name, tool_id),
            "mcp" => crate::services::toggle_mcp(name, tool_id, false),
            "plugin" => {
                let plugin_kind = database::list_extensions()
                    .iter()
                    .find(|e| &e.id == ext_id)
                    .and_then(|e| e.tags.clone())
                    .unwrap_or_else(|| "file".to_string());
                crate::services::toggle_plugin(name, tool_id, false, &plugin_kind)
            }
            _ => Ok(()),
        };
        match result {
            Ok(()) => disabled.push(ext_id.clone()),
            Err(e) => failures.push(format!("{}: {}", ext_id, e)),
        }
    }

    if let Some(dir) = crate::adapter::primary_skill_dir(tool_id) {
        for name in &plan.stash_native {
            match super::stash::stash_native_skill(tool_id, name, &dir.join(name)) {
                Ok(()) => stashed.push(name.clone()),
                Err(e) => failures.push(format!("{}: {}", name, e)),
            }
        }
    }

    (disabled, stashed, failures)
}
```

- [x] **Step 4: 跑测试 + Commit**

Run: `cd src-tauri && cargo test sweep_ && cargo test`
Expected: PASS

```bash
git add src-tauri/src/services/preset/ src-tauri/tests/preset_v2_test.rs
git commit -m "feat(preset-v2): exclusive sweep — diff plan + best-effort execution"
```

---

### Task 9: 应用/恢复编排重写——滑块开关语义 + W5 前置恢复

**Files:**
- Modify: `src-tauri/src/services/preset/mod.rs`（`apply_preset` 重写、新增 `restore_tool`、`deactivate_preset` 委托、`ApplyResult` 扩字段、新增 `RestoreResult`）
- Modify: `src-tauri/src/services/tool_settings.rs:91-94`（W5 取消勾选前先恢复基底）
- Test: `src-tauri/tests/preset_v2_test.rs`（追加集成测试）

**Interfaces:**
- Consumes: Task 2 `get_preset`、Task 3 `tool_allowed`、Task 4 快照 DAO、Task 6 stash、Task 7 snapshot、Task 8 sweep
- Produces:
  - `ApplyResult { success: usize, failures: Vec<String>, conflicts: Vec<String>, stashed: Vec<String>, disabled: Vec<String>, restored_native: Vec<String> }`
  - `apply_preset(preset_id: &str, tool_id: &str) -> Result<ApplyResult, String>`——签名从 `ApplyResult` 改为 `Result<ApplyResult, String>`（scope 守卫硬错误）；**调用方 `commands/preset.rs:32` 在本任务同步适配**
  - `restore_tool(tool_id: &str) -> Result<RestoreResult, String>`——无快照时返回空结果（幂等，不报错）
  - `RestoreResult { restored_mam: Vec<String>, restored_native: Vec<String>, conflicts: Vec<String> }`
  - `deactivate_preset(preset_id, tool_id)` 保留为兼容委托（校验激活中 → restore_tool），M2 移除

- [x] **Step 1: 写失败测试（完整生命周期）**

`tests/preset_v2_test.rs` 追加：

```rust
/// 完整生命周期（spec §3.2/§5）：开（拍基底+独占）→ 切换（沿用基底）→ 关（精确回基底+销毁）→ 再开（重拍新基底）
#[test]
fn apply_switch_restore_full_lifecycle() {
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::{apply_preset, restore_tool};
    use multi_agents_manager_lib::services::{disable_skill_for_tool, enable_skill_for_tool};

    let home = dirs::home_dir().unwrap();
    let claude_dir = home.join(".claude/skills");

    // 基底现场：MAM skill base-1 已启用（含一条子 Agent 分配行）+ 原生真目录 native-1
    for name in ["v2m1-lc-base1"] {
        let ssot = dirs::home_dir().unwrap().join(".mam/skills").join(name);
        std::fs::create_dir_all(&ssot).unwrap();
        std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
        database::insert_extension(&database::ExtensionRecord {
            id: format!("skill-{}", name), kind: "skill".into(), name: name.into(),
            description: None, source_path: ssot.to_string_lossy().to_string(),
            source_url: None, version: None, tags: None, suite: None,
            source_tool: None, is_native: false,
        }).unwrap();
        enable_skill_for_tool(name, "claude").unwrap();
    }
    // 子 Agent 分配行：应用预设时随工具级清扫断链，恢复默认后必须重建回来
    database::upsert_assignment_with_subagent("skill-v2m1-lc-base1", "claude", "v2m1-lc-sub", true, "valid")
        .unwrap();
    let sub_target = claude_dir.join("subagents/v2m1-lc-sub/v2m1-lc-base1");
    std::fs::create_dir_all(claude_dir.join("v2m1-lc-native1")).unwrap();
    std::fs::write(claude_dir.join("v2m1-lc-native1/SKILL.md"), "n").unwrap();

    // 预设 A：skill-a
    let mk = |name: &str| {
        let ssot = home.join(".mam/skills").join(name);
        std::fs::create_dir_all(&ssot).unwrap();
        std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
        database::insert_extension(&database::ExtensionRecord {
            id: format!("skill-{}", name), kind: "skill".into(), name: name.into(),
            description: None, source_path: ssot.to_string_lossy().to_string(),
            source_url: None, version: None, tags: None, suite: None,
            source_tool: None, is_native: false,
        }).unwrap();
    };
    mk("v2m1-lc-a");
    mk("v2m1-lc-b");
    let preset_a = database::create_preset("v2m1-lc-A", &[("skill-v2m1-lc-a".into(), "skill".into())]).unwrap();
    let preset_b = database::create_preset("v2m1-lc-B", &[("skill-v2m1-lc-b".into(), "skill".into())]).unwrap();

    // 开 A：base-1 断链（含子 Agent 链级联清理）、native-1 暂存、a 启用；快照在、active=A
    let r = apply_preset(&preset_a, "claude").unwrap();
    assert!(r.success >= 1);
    assert!(r.disabled.contains(&"skill-v2m1-lc-base1".to_string()), "{:?}", r.disabled);
    assert!(r.stashed.contains(&"v2m1-lc-native1".to_string()), "{:?}", r.stashed);
    assert!(claude_dir.join("v2m1-lc-a").exists());
    assert!(!claude_dir.join("v2m1-lc-base1").exists());
    let (active, items) = database::get_base_snapshot("claude").unwrap();
    assert_eq!(active.as_deref(), Some(preset_a.as_str()));
    assert!(items.iter().any(|i| i.extension_id == "skill-v2m1-lc-base1" && i.origin == "mam"));
    assert!(items.iter().any(|i| i.extension_id == "skill-v2m1-lc-native1" && i.origin == "native"));

    // 切 B：a 断、b 启；基底沿用（native-1 仍暂存，base-1 仍不在）
    let r2 = apply_preset(&preset_b, "claude").unwrap();
    assert!(r2.disabled.contains(&"skill-v2m1-lc-a".to_string()));
    assert!(!claude_dir.join("v2m1-lc-a").exists());
    assert!(claude_dir.join("v2m1-lc-b").exists());
    assert!(!claude_dir.join("v2m1-lc-native1").exists(), "切换不清算基底，原生仍暂存");
    let (active_b, _) = database::get_base_snapshot("claude").unwrap();
    assert_eq!(active_b.as_deref(), Some(preset_b.as_str()));

    // 会话期间手动漂移：再启用 base-1
    enable_skill_for_tool("v2m1-lc-base1", "claude").unwrap();
    assert!(claude_dir.join("v2m1-lc-base1").exists());

    // 关：精确回基底——base-1 回来（含子 Agent 链接重建）、native-1 回来、b/a 都不在；快照销毁
    let rr = restore_tool("claude").unwrap();
    assert!(claude_dir.join("v2m1-lc-base1").exists(), "MAM 基底项应重建");
    assert!(claude_dir.join("v2m1-lc-native1").exists(), "原生暂存应回移");
    assert!(rr.restored_native.contains(&"v2m1-lc-native1".to_string()));
    assert!(sub_target.exists(), "子 Agent 链接应随基底重建（Layer3）");
    assert!(!claude_dir.join("v2m1-lc-a").exists());
    assert!(!claude_dir.join("v2m1-lc-b").exists());
    assert!(database::get_base_snapshot("claude").is_none(), "恢复后快照销毁（会话级）");

    // 再开 A：重拍新基底（= 刚恢复的状态）
    apply_preset(&preset_a, "claude").unwrap();
    let (active2, items2) = database::get_base_snapshot("claude").unwrap();
    assert_eq!(active2.as_deref(), Some(preset_a.as_str()));
    assert!(items2.iter().any(|i| i.extension_id == "skill-v2m1-lc-base1"));
    // 清场
    let _ = restore_tool("claude");
    disable_skill_for_tool("v2m1-lc-base1", "claude").unwrap();
}
```

以及 scope 守卫与专属过滤的测试：

```rust
/// scope 守卫：tool 私有预设不可应用到别的工具（硬错误）
#[test]
fn apply_rejects_cross_tool_for_tool_scoped_preset() {
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::apply_preset;

    let id = database::create_preset_with_meta("v2m1-scope", "", "tool", Some("codex"), &[])
        .unwrap();
    let err = apply_preset(&id, "claude").unwrap_err();
    assert!(err.contains("绑定"), "应拒绝跨工具: {}", err);
}

/// 专属过滤：预设项对目标工具不兼容 → 剔除进 conflicts，不启用
#[test]
fn apply_filters_incompatible_items() {
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::apply_preset;

    let home = dirs::home_dir().unwrap();
    let ssot = home.join(".mam/skills/v2m1-filt-a");
    std::fs::create_dir_all(&ssot).unwrap();
    std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m1-filt-a".into(), kind: "skill".into(), name: "v2m1-filt-a".into(),
        description: None, source_path: ssot.to_string_lossy().to_string(),
        source_url: None, version: None, tags: None, suite: None,
        source_tool: None, is_native: false,
    }).unwrap();
    database::upsert_resource_binding("skill-v2m1-filt-a", "codex", Some("专属 codex")).unwrap();

    let pid = database::create_preset("v2m1-filt", &[("skill-v2m1-filt-a".into(), "skill".into())]).unwrap();
    let r = apply_preset(&pid, "claude").unwrap();
    assert_eq!(r.success, 0);
    assert_eq!(r.conflicts.len(), 1);
    assert!(r.conflicts[0].contains("v2m1-filt-a"));
    let claude_dir = home.join(".claude/skills");
    assert!(!claude_dir.join("v2m1-filt-a").exists(), "被过滤项不得启用");
    let _ = restore_tool_cleanup_only("claude");
}

/// 测试辅助：只销毁快照不动文件（清场用）
fn restore_tool_cleanup_only(tool_id: &str) {
    let _ = multi_agents_manager_lib::database::destroy_base_snapshot(tool_id);
    let _ = multi_agents_manager_lib::services::preset::stash::restore_all_for_tool(tool_id);
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test apply_ && cargo test apply_switch`
Expected: 编译/断言失败——旧 apply_preset 是增量语义且返回非 Result

- [x] **Step 3: 重写 `services/preset/mod.rs` 的 apply/deactivate**

保留 `apply_preset_to_subagent` / `deactivate_preset_from_subagent` / `check_compatibility`（Task 5 已改）不动；`apply_preset` 整体替换为：

```rust
/// 应用预设（独占语义，spec §5.1）：开 = 拍基底（若无）→ 差集清扫 → 启用预设项
#[derive(Default)]
pub struct ApplyResult {
    pub success: usize,
    pub failures: Vec<String>,
    pub conflicts: Vec<String>,
    /// 本次被暂存的原生技能名
    pub stashed: Vec<String>,
    /// 本次被停用的 MAM 资源 extension_id
    pub disabled: Vec<String>,
    /// 预设原生技能项从暂存区接回的名（ensure-present 语义）
    pub restored_native: Vec<String>,
}

/// 工具私有预设的跨工具应用属硬错误（spec §3.3）
pub fn apply_preset(preset_id: &str, tool_id: &str) -> Result<ApplyResult, String> {
    let preset = database::get_preset(preset_id).ok_or_else(|| format!("预设不存在: {}", preset_id))?;
    if preset.scope == "tool" && preset.bound_tool.as_deref() != Some(tool_id) {
        return Err(format!(
            "预设 {} 绑定 {}，不能应用到 {}",
            preset.name,
            preset.bound_tool.as_deref().unwrap_or("?"),
            tool_id
        ));
    }
    let mut result = ApplyResult {
        success: 0,
        failures: Vec::new(),
        conflicts: Vec::new(),
        stashed: Vec::new(),
        disabled: Vec::new(),
        restored_native: Vec::new(),
    };

    // 1) 专属过滤（spec §6）：不兼容项剔除进 conflicts
    let all_items = database::get_preset_items(preset_id);
    let extensions = database::list_extensions();
    let mut apply_items: Vec<(String, String)> = Vec::new();
    for (ext_id, kind) in &all_items {
        if database::tool_allowed(ext_id, tool_id) {
            apply_items.push((ext_id.clone(), kind.clone()));
        } else {
            let name = extensions
                .iter()
                .find(|e| &e.id == ext_id)
                .map(|e| e.name.clone())
                .unwrap_or_else(|| ext_id.clone());
            result
                .conflicts
                .push(format!("{}（{}）：专属绑定不兼容 {}", name, kind, tool_id));
        }
    }

    // 2) 基底快照：无 → 拍（会话开始）；有 → 沿用，仅旧预设历史置 inactive（spec §3.2）
    match database::get_base_snapshot(tool_id) {
        None => snapshot::capture_base_snapshot(tool_id)?,
        Some((Some(old), _)) => {
            if old != preset_id {
                let _ = database::record_preset_application(&old, tool_id, false);
            }
        }
        Some((None, _)) => { /* 快照在而无激活（异常残留）——沿用快照即可 */ }
    }

    // 3) 独占清扫（差集 = 当前 − 预设项 − 常驻）
    let plan = sweep::plan_sweep(tool_id, &apply_items);
    let (disabled, stashed, sweep_failures) = sweep::execute_sweep(tool_id, &plan);
    result.disabled = disabled;
    result.stashed = stashed;
    result.failures.extend(sweep_failures);

    // 4) 启用预设项：MAM 走既有服务；原生技能 = 确保在场（spec §3.3）
    for (ext_id, kind) in &apply_items {
        let name = ext_id
            .strip_prefix(&format!("{}-", kind))
            .unwrap_or(ext_id);
        let is_native_item = kind == "skill"
            && extensions
                .iter()
                .find(|e| &e.id == ext_id)
                .map(|e| e.is_native && e.source_tool.as_deref() == Some(tool_id))
                .unwrap_or(false);
        let outcome = if is_native_item {
            ensure_native_present(tool_id, name, &mut result.restored_native)
        } else {
            match kind.as_str() {
                "skill" => services::enable_skill_for_tool(name, tool_id),
                "mcp" => services::toggle_mcp(name, tool_id, true),
                "plugin" => {
                    let plugin_kind = extensions
                        .iter()
                        .find(|e| &e.id == ext_id)
                        .and_then(|e| e.tags.clone())
                        .unwrap_or_else(|| "file".to_string());
                    crate::services::plugin::toggle_plugin(name, tool_id, true, &plugin_kind)
                }
                _ => Err(format!("未知类型: {}", kind)),
            }
        };
        match outcome {
            Ok(()) => result.success += 1,
            Err(e) => result.failures.push(format!("{}: {}", ext_id, e)),
        }
    }

    // 5) 记激活 + 历史
    database::set_active_preset(tool_id, preset_id)?;
    let _ = database::record_preset_application(preset_id, tool_id, true);
    info!(
        "预设组 {} → {}（独占）— 成功 {} 停用 {} 暂存 {} 失败 {} 冲突 {}",
        preset_id, tool_id, result.success, result.disabled.len(),
        result.stashed.len(), result.failures.len(), result.conflicts.len()
    );
    Ok(result)
}

/// 原生技能项的「确保在场」：目录在 → Ok；在暂存区 → 接回；都没有 → 失败
fn ensure_native_present(
    tool_id: &str,
    name: &str,
    restored_native: &mut Vec<String>,
) -> Result<(), String> {
    let dir = crate::adapter::primary_skill_dir(tool_id)
        .ok_or_else(|| format!("工具 {} 无 skill 目录", tool_id))?;
    if dir.join(name).exists() {
        return Ok(());
    }
    let entry = database::unrestored_stash(Some(tool_id))
        .into_iter()
        .find(|e| e.skill_name == name)
        .ok_or_else(|| format!("原生技能 {} 既不在工具目录也不在暂存区", name))?;
    stash::restore_stashed_skill(&entry)?;
    restored_native.push(name.to_string());
    Ok(())
}

/// 恢复默认（关，spec §5.2）：对齐基底 → 销毁快照。幂等：无快照返回空结果
#[derive(Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreResult {
    pub restored_mam: Vec<String>,
    pub restored_native: Vec<String>,
    pub conflicts: Vec<String>,
}

pub fn restore_tool(tool_id: &str) -> Result<RestoreResult, String> {
    let mut out = RestoreResult::default();
    let Some((active_preset, base_items)) = database::get_base_snapshot(tool_id) else {
        return Ok(out); // 无激活预设：幂等空操作
    };

    // 1) 暂存回移（冲突不覆盖，留待人工处理）
    let (restored_native, conflicts) = stash::restore_all_for_tool(tool_id);
    out.restored_native = restored_native;
    out.conflicts = conflicts;

    // 2) MAM 资源精确重建到基底集合（spec §5.2 步骤3）：
    //    快照 mam 集 = 目标；当前 enabled 集 = 现状；差异双向处理
    let target: Vec<(String, String)> = base_items
        .iter()
        .filter(|i| i.origin == "mam")
        .map(|i| (i.extension_id.clone(), i.kind.clone()))
        .collect();
    let current: Vec<(String, String)> = database::list_assignments(tool_id)
        .into_iter()
        // 只看工具级行：子 Agent 行由下面的重建段处理（与 scan_tool_state 同口径）
        .filter(|a| a.enabled && a.sub_agent_id.is_none())
        .map(|a| {
            let kind = if a.extension_id.starts_with("skill-") {
                "skill"
            } else if a.extension_id.starts_with("mcp-") {
                "mcp"
            } else {
                "plugin"
            };
            (a.extension_id, kind.to_string())
        })
        .collect();

    let extensions = database::list_extensions();
    let enable_one = |ext_id: &str, kind: &str| -> Result<(), String> {
        let name = ext_id.strip_prefix(&format!("{}-", kind)).unwrap_or(ext_id);
        match kind {
            "skill" => services::enable_skill_for_tool(name, tool_id),
            "mcp" => services::toggle_mcp(name, tool_id, true),
            "plugin" => {
                let plugin_kind = extensions
                    .iter()
                    .find(|e| e.id == ext_id)
                    .and_then(|e| e.tags.clone())
                    .unwrap_or_else(|| "file".to_string());
                crate::services::plugin::toggle_plugin(name, tool_id, true, &plugin_kind)
            }
            _ => Ok(()),
        }
    };
    let disable_one = |ext_id: &str, kind: &str| -> Result<(), String> {
        let name = ext_id.strip_prefix(&format!("{}-", kind)).unwrap_or(ext_id);
        match kind {
            "skill" => services::disable_skill_for_tool(name, tool_id),
            "mcp" => services::toggle_mcp(name, tool_id, false),
            "plugin" => {
                let plugin_kind = extensions
                    .iter()
                    .find(|e| e.id == ext_id)
                    .and_then(|e| e.tags.clone())
                    .unwrap_or_else(|| "file".to_string());
                crate::services::plugin::toggle_plugin(name, tool_id, false, &plugin_kind)
            }
            _ => Ok(()),
        }
    };

    for (id, kind) in &target {
        if !current.iter().any(|(cid, _)| cid == id) {
            match enable_one(id, kind) {
                Ok(()) => out.restored_mam.push(id.clone()),
                Err(e) => out.conflicts.push(format!("{}: 重建失败 {}", id, e)),
            }
        }
    }
    for (id, kind) in &current {
        if !target.iter().any(|(tid, _)| tid == id) {
            if let Err(e) = disable_one(id, kind) {
                out.conflicts.push(format!("{}: 停用失败 {}", id, e));
            }
        }
    }

    // 2.5) 子 Agent 链接重建：清扫的工具级禁用会级联断 Layer3（cleanup_layer3_on_tool_disable），
    //      工具级恢复后对「基底内且仍 enabled」的子 Agent 分配行重建链接——与 W5
    //      rebuild_tool_links 同思路（先工具级后子 Agent）；幂等，链在则替换。
    //      基底外技能的子 Agent 行不重建（其工具级已被本流程停用）
    for a in database::list_assignments(tool_id) {
        if !a.enabled || a.sub_agent_id.is_none() {
            continue;
        }
        if !target.iter().any(|(tid, _)| tid == &a.extension_id) {
            continue;
        }
        if let Some(name) = a.extension_id.strip_prefix("skill-") {
            let sub = a.sub_agent_id.clone().unwrap_or_default();
            if let Err(e) = services::assign_skill_to_subagent(name, tool_id, &sub) {
                out.conflicts.push(format!("{}#{}: 子 Agent 链接重建失败 {}", a.extension_id, sub, e));
            }
        }
    }

    // 3) 销毁快照（会话结束）+ 历史置 inactive
    database::destroy_base_snapshot(tool_id)?;
    if let Some(p) = active_preset {
        let _ = database::record_preset_application(&p, tool_id, false);
    }
    info!("工具 {} 已恢复默认（基底对齐完成）", tool_id);
    Ok(out)
}

/// 兼容委托（M2 移除）：旧命令入口校验激活中再走 restore_tool
pub fn deactivate_preset(preset_id: &str, tool_id: &str) -> Result<(), String> {
    match database::get_base_snapshot(tool_id) {
        Some((Some(active), _)) if active != preset_id => {
            return Err(format!("工具 {} 当前激活的是其他预设", tool_id));
        }
        _ => {}
    }
    restore_tool(tool_id).map(|_| ())
}
```

同时：`mod.rs` 顶部补 `pub mod snapshot; pub mod sweep;`（stash 已在 Task 6 加过）、删掉旧 `apply_preset` 里被替换的 `check_conflict`（`deactivate_preset` 旧实现整体删除）。

**注意**：`apply_preset_to_subagent`（mod.rs:172-214，本期不改语义）末尾的 `ApplyResult { success, failures, conflicts }` 构造会因字段扩到 6 个而编译失败——改为：

```rust
    ApplyResult {
        success,
        failures,
        conflicts,
        ..Default::default()
    }
```

- [x] **Step 4: 适配调用方 + W5 前置恢复**

`commands/preset.rs:29-38` 的 `apply_preset` 命令改为：

```rust
#[tauri::command]
pub fn apply_preset(preset_id: String, tool_id: String) -> Result<PresetApplyResult, String> {
    // review F4：停用工具的预设写操作一律拒绝（W5 生效范围）
    crate::services::tool_settings::ensure_tool_enabled(&tool_id)?;
    let result = crate::services::preset::apply_preset(&preset_id, &tool_id)?;
    Ok(PresetApplyResult {
        success_count: result.success,
        failures: result.failures,
        conflicts: result.conflicts,
        stashed: result.stashed,
        disabled: result.disabled,
        restored_native: result.restored_native,
    })
}
```

`PresetApplyResult` 扩三字段（serde camelCase 自动转 `stashed/disabled/restoredNative`）。

`services/tool_settings.rs:91-94`（`apply_tool_changes` 取消勾选分支）改为：

```rust
        if !c.enabled {
            // 预设 v2（spec §5.4）：取消勾选前先恢复基底——若有激活预设，
            // 暂存的原生技能与独占停用项必须先归位，再做 W5 还原清理
            if let Err(e) = crate::services::preset::restore_tool(&c.tool_id) {
                log::warn!("取消勾选 {} 前恢复基底失败: {}", c.tool_id, e);
            }
            // 取消勾选：清理为 best-effort（跳过项逐项报告，spec §9），随后落 DB
            disable_tool_cleanup(&c.tool_id, &mut result);
            agent_tool::set_tool_enabled(&c.tool_id, false);
        }
```

- [x] **Step 5: 跑测试 + 全量 + Commit**

Run: `cd src-tauri && cargo test apply_ && cargo test apply_switch && cargo test`
Expected: 全 PASS（存量 `dao_test::test_preset_crud` 不受影响——它只测 DAO）

```bash
git add src-tauri/src/services/preset/mod.rs src-tauri/src/services/tool_settings.rs src-tauri/src/commands/preset.rs src-tauri/tests/preset_v2_test.rs
git commit -m "feat(preset-v2): exclusive apply/restore orchestration + switch semantics + W5 pre-restore"
```

---

### Task 10: 命令层与注册——update/get/restore + 绑定/常驻命令

**Files:**
- Modify: `src-tauri/src/commands/preset.rs`
- Modify: `src-tauri/src/lib.rs:120-126`（invoke_handler 注册）
- Test: `src-tauri/tests/preset_v2_test.rs`（追加命令层可达性测试）

**Interfaces:**
- Consumes: Task 2/3/9 的服务与 DAO
- Produces（全部 `#[tauri::command]`，serde camelCase）:
  - `create_preset(name, items, description: Option<String>, scope: Option<String>, bound_tool: Option<String>) -> Result<String, String>`（旧两参调用兼容：Option 缺省）
  - `update_preset(id, name, description, scope, bound_tool: Option<String>, items) -> Result<(), String>`——激活中（任一快照 active == id）拒绝
  - `get_preset(preset_id) -> Option<PresetRecord>`
  - `restore_preset(tool_id) -> Result<RestoreResult, String>`（守卫 `ensure_tool_enabled`；关=安全动作直接执行）
  - `get_active_preset(tool_id) -> Option<String>`——M2 开关状态渲染的数据源（spec §4「基底快照查询」）
  - `preview_apply_preset(preset_id, tool_id) -> Result<ApplyPreview, String>`——应用确认弹窗（spec §7.3）的 dry-run 数据源：纯计算不执行
  - `ApplyPreview { to_enable: Vec<String>, filtered: Vec<String>, to_disable: Vec<String>, to_stash: Vec<String>, resident_exempt: Vec<String> }`（services 层 `preview_apply` 同构）
  - `set_resource_binding(extension_id, exclusive_tools: Vec<String>, reason: Option<String>)`、`list_resource_bindings() -> Vec<ResourceBindingRecord>`、`delete_resource_binding(extension_id)`
  - `set_tool_resident(tool_id, extension_id, resident: bool)`、`list_tool_residents(tool_id) -> Vec<String>`
  - `delete_preset` 增守卫：激活中的预设不可删（提示先恢复）

- [x] **Step 1: 写失败测试**

`tests/preset_v2_test.rs` 追加（命令层函数是纯函数转发，直接调用验证守卫逻辑）：

```rust
/// 激活中的预设不可删除（spec §5.4）——先恢复默认再删
#[test]
fn delete_rejects_active_preset() {
    support::setup();
    use multi_agents_manager_lib::commands::preset as cmd;
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::apply_preset;

    let home = dirs::home_dir().unwrap();
    let ssot = home.join(".mam/skills/v2m1-del-a");
    std::fs::create_dir_all(&ssot).unwrap();
    std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m1-del-a".into(), kind: "skill".into(), name: "v2m1-del-a".into(),
        description: None, source_path: ssot.to_string_lossy().to_string(),
        source_url: None, version: None, tags: None, suite: None,
        source_tool: None, is_native: false,
    }).unwrap();
    let pid = database::create_preset("v2m1-del", &[("skill-v2m1-del-a".into(), "skill".into())]).unwrap();
    apply_preset(&pid, "claude").unwrap();

    let err = cmd::delete_preset(pid.clone()).unwrap_err();
    assert!(err.contains("激活"), "{}", err);

    let _ = multi_agents_manager_lib::services::preset::restore_tool("claude");
    cmd::delete_preset(pid).unwrap(); // 恢复后可删
}
```

再加预览与开关状态命令的测试：

```rust
/// 预览（dry-run）不动现场；get_active_preset 反映开关状态
#[test]
fn preview_is_dryrun_and_active_preset_queryable() {
    support::setup();
    use multi_agents_manager_lib::commands::preset as cmd;
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::{apply_preset, preview_apply, restore_tool};

    assert_eq!(cmd::get_active_preset("claude".into()), None);

    let home = dirs::home_dir().unwrap();
    let claude_dir = home.join(".claude/skills");
    std::fs::create_dir_all(claude_dir.join("v2m1-pv-native")).unwrap();
    let ssot = home.join(".mam/skills/v2m1-pv-a");
    std::fs::create_dir_all(&ssot).unwrap();
    std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m1-pv-a".into(), kind: "skill".into(), name: "v2m1-pv-a".into(),
        description: None, source_path: ssot.to_string_lossy().to_string(),
        source_url: None, version: None, tags: None, suite: None,
        source_tool: None, is_native: false,
    }).unwrap();
    let pid = database::create_preset("v2m1-pv", &[("skill-v2m1-pv-a".into(), "skill".into())]).unwrap();

    let pv = preview_apply(&pid, "claude").unwrap();
    assert_eq!(pv.to_enable, vec!["skill-v2m1-pv-a".to_string()]);
    assert!(pv.to_stash.contains(&"v2m1-pv-native".to_string()));
    assert!(pv.filtered.is_empty());
    assert!(claude_dir.join("v2m1-pv-native").exists(), "预览不得动现场");

    apply_preset(&pid, "claude").unwrap();
    assert_eq!(cmd::get_active_preset("claude".into()), Some(pid.clone()));
    let _ = restore_tool("claude");
    assert_eq!(cmd::get_active_preset("claude".into()), None);
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test delete_rejects`
Expected: FAIL——旧 delete_preset 无守卫直接删

- [x] **Step 3: 实现命令**

`commands/preset.rs` 追加/修改（`use` 段补 `RestoreResult, ApplyPreview, ResourceBindingRecord, PresetRecord` 路径 `crate::services::preset` / `crate::database`）。

先在 `services/preset/mod.rs` 加预览（纯计算，复用 T5 兼容判定 + T8 清扫计划，不执行任何文件/DB 写操作）：

```rust
/// 应用预览（spec §7.3 差异确认弹窗数据源）：dry-run，不执行、不动现场
#[derive(Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyPreview {
    /// 将启用（专属过滤后）的 extension_id
    pub to_enable: Vec<String>,
    /// 被专属绑定过滤的项 "id: 原因"
    pub filtered: Vec<String>,
    /// 将停用的 MAM 资源 extension_id
    pub to_disable: Vec<String>,
    /// 将暂存的原生技能名
    pub to_stash: Vec<String>,
    /// 常驻豁免（当前生效、不在预设、但受常驻保护）的 extension_id
    pub resident_exempt: Vec<String>,
}

pub fn preview_apply(preset_id: &str, tool_id: &str) -> Result<ApplyPreview, String> {
    let preset = database::get_preset(preset_id).ok_or_else(|| format!("预设不存在: {}", preset_id))?;
    if preset.scope == "tool" && preset.bound_tool.as_deref() != Some(tool_id) {
        return Err(format!(
            "预设 {} 绑定 {}，不能应用到 {}",
            preset.name,
            preset.bound_tool.as_deref().unwrap_or("?"),
            tool_id
        ));
    }
    let mut apply_items: Vec<(String, String)> = Vec::new();
    let mut out = ApplyPreview::default();
    for (ext_id, kind) in database::get_preset_items(preset_id) {
        if database::tool_allowed(&ext_id, tool_id) {
            apply_items.push((ext_id, kind));
        } else {
            out.filtered.push(format!("{}: 专属绑定不兼容 {}", ext_id, tool_id));
        }
    }
    let plan = sweep::plan_sweep(tool_id, &apply_items);
    out.to_enable = apply_items.into_iter().map(|(id, _)| id).collect();
    out.to_disable = plan.disable_mam.into_iter().map(|(id, _)| id).collect();
    out.to_stash = plan.stash_native;
    // 常驻豁免清单：与 plan_sweep 的跳过逻辑对齐（当前生效 − 预设 − 常驻保护）
    let keep: Vec<String> = out.to_enable.clone();
    for item in snapshot::scan_tool_state(tool_id) {
        if keep.contains(&item.extension_id) {
            continue;
        }
        if database::is_tool_resident(tool_id, &item.extension_id) {
            out.resident_exempt.push(item.extension_id);
        }
    }
    Ok(out)
}
```

命令层追加：

```rust
#[tauri::command]
pub fn create_preset(
    name: String,
    items: Vec<(String, String)>,
    description: Option<String>,
    scope: Option<String>,
    bound_tool: Option<String>,
) -> Result<String, String> {
    let scope = scope.unwrap_or_else(|| "universal".to_string());
    if scope == "tool" && bound_tool.is_none() {
        return Err("工具私有预设必须指定绑定工具".into());
    }
    crate::database::create_preset_with_meta(
        &name,
        &description.unwrap_or_default(),
        &scope,
        bound_tool.as_deref(),
        &items,
    )
}

#[tauri::command]
pub fn get_preset(preset_id: String) -> Option<PresetRecord> {
    crate::database::get_preset(&preset_id)
}

#[tauri::command]
pub fn update_preset(
    id: String,
    name: String,
    description: String,
    scope: String,
    bound_tool: Option<String>,
    items: Vec<(String, String)>,
) -> Result<(), String> {
    if scope == "tool" && bound_tool.is_none() {
        return Err("工具私有预设必须指定绑定工具".into());
    }
    crate::database::update_preset(&id, &name, &description, &scope, bound_tool.as_deref(), &items)
}

#[tauri::command]
pub fn restore_preset(tool_id: String) -> Result<RestoreResult, String> {
    crate::services::tool_settings::ensure_tool_enabled(&tool_id)?;
    crate::services::preset::restore_tool(&tool_id)
}

/// 工具当前激活的预设（开关状态数据源；无激活 = None）
#[tauri::command]
pub fn get_active_preset(tool_id: String) -> Option<String> {
    crate::database::get_base_snapshot(&tool_id).and_then(|(active, _)| active)
}

/// 应用预览（确认弹窗数据源，dry-run）
#[tauri::command]
pub fn preview_apply_preset(preset_id: String, tool_id: String) -> Result<ApplyPreview, String> {
    crate::services::preset::preview_apply(&preset_id, &tool_id)
}

#[tauri::command]
pub fn set_resource_binding(
    extension_id: String,
    exclusive_tools: Vec<String>,
    reason: Option<String>,
) -> Result<(), String> {
    crate::database::upsert_resource_binding(
        &extension_id,
        &exclusive_tools.join(","),
        reason.as_deref(),
    )
}

#[tauri::command]
pub fn list_resource_bindings() -> Vec<ResourceBindingRecord> {
    crate::database::list_resource_bindings()
}

#[tauri::command]
pub fn delete_resource_binding(extension_id: String) -> Result<(), String> {
    crate::database::delete_resource_binding(&extension_id)
}

#[tauri::command]
pub fn set_tool_resident(
    tool_id: String,
    extension_id: String,
    resident: bool,
) -> Result<(), String> {
    crate::services::tool_settings::ensure_tool_enabled(&tool_id)?;
    crate::database::set_tool_resident(&tool_id, &extension_id, resident)
}

#[tauri::command]
pub fn list_tool_residents(tool_id: String) -> Vec<String> {
    crate::database::list_tool_residents(&tool_id)
}
```

`delete_preset` 加守卫（替换现有直接转发）：

```rust
#[tauri::command]
pub fn delete_preset(preset_id: String) -> Result<(), String> {
    // spec §5.4：激活中的预设不可删——先恢复默认
    for tool in crate::adapter::TOOL_IDS {
        if let Some((Some(active), _)) = crate::database::get_base_snapshot(tool) {
            if active == preset_id {
                return Err(format!(
                    "预设正在 {} 上激活，请先恢复默认再删除（PRESET_ACTIVE:{}）",
                    tool, preset_id
                ));
            }
        }
    }
    crate::database::delete_preset(&preset_id)
}
```

`deactivate_preset` 命令改为委托（M2 移除）：

```rust
#[tauri::command]
pub fn deactivate_preset(preset_id: String, tool_id: String) -> Result<(), String> {
    crate::services::tool_settings::ensure_tool_enabled(&tool_id)?;
    crate::services::preset::deactivate_preset(&preset_id, &tool_id)
}
```

`lib.rs` invoke_handler 在 `commands::preset::create_preset,` 一组里追加：

```rust
        commands::preset::get_preset,
        commands::preset::update_preset,
        commands::preset::restore_preset,
        commands::preset::get_active_preset,
        commands::preset::preview_apply_preset,
        commands::preset::set_resource_binding,
        commands::preset::list_resource_bindings,
        commands::preset::delete_resource_binding,
        commands::preset::set_tool_resident,
        commands::preset::list_tool_residents,
```

- [x] **Step 4: 跑测试 + 全量 + Commit**

Run: `cd src-tauri && cargo test delete_rejects && cargo test`
Expected: PASS

```bash
git add src-tauri/src/commands/preset.rs src-tauri/src/lib.rs src-tauri/tests/preset_v2_test.rs
git commit -m "feat(preset-v2): IPC commands — update/get/restore + bindings/residents + active-preset guards"
```

---

### Task 11: 数据源统一——MCP 导入写表 + 吞错修复 + 注册表回填（P2① 修复）

**Files:**
- Modify: `src-tauri/src/database/dao/extension.rs`（加 `ensure_extension`）
- Modify: `src-tauri/src/database/mod.rs`（re-export）
- Modify: `src-tauri/src/commands/resource.rs`（`import_mcp_to_ssot` / `save_mcp_config` 落表；`:182` 吞错修复）
- Modify: `src-tauri/src/services/resource/mod.rs`（加 `backfill_registry`）
- Modify: `src-tauri/src/lib.rs:33-39`（启动线程接线）
- Test: `src-tauri/tests/preset_v2_test.rs`（追加）

**Interfaces:**
- Produces:
  - `ensure_extension(ext: &ExtensionRecord) -> Result<(), String>`——INSERT OR IGNORE（不覆盖已有行，幂等回填专用）
  - `services::resource::backfill_registry()`——扫描 `~/.mam/skills/<dir>` 与 `~/.mam/mcp/*.json`，无行则 ensure；启动时调用
  - `import_mcp_to_ssot` / `save_mcp_config` 成功写文件后落 `extensions` 行（`kind="mcp"`）

- [x] **Step 1: 写失败测试**

`tests/preset_v2_test.rs` 追加：

```rust
/// P2①：MCP 入 SSOT 必须落 extensions 行——预设创建列表与卡片从此同源；
/// 注册表回填把历史无行的 skill/mcp 补进表
#[test]
fn mcp_import_and_backfill_register_rows() {
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::resource::backfill_registry;

    // 手工放一个 MCP 配置文件（历史上 toggle_mcp 只写 assignment 不写表）
    let repo = dirs::home_dir().unwrap().join(".mam/mcp");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("v2m1-backfill-mcp.json"), r#"{"command":"x"}"#).unwrap();
    // 手工放一个无行的 skill 目录（历史残留/手工放置）
    let skill = dirs::home_dir().unwrap().join(".mam/skills/v2m1-backfill-skill");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "x").unwrap();

    assert!(database::list_extensions().iter().all(|e| e.id != "mcp-v2m1-backfill-mcp"));
    backfill_registry();

    let ids: Vec<String> = database::list_extensions().iter().map(|e| e.id.clone()).collect();
    assert!(ids.contains(&"mcp-v2m1-backfill-mcp".to_string()), "MCP 应回填入表");
    assert!(ids.contains(&"skill-v2m1-backfill-skill".to_string()), "无行 skill 应回填");

    // 幂等：再跑不重复
    backfill_registry();
    let n_mcp = database::list_extensions()
        .iter()
        .filter(|e| e.id == "mcp-v2m1-backfill-mcp")
        .count();
    assert_eq!(n_mcp, 1);

    // save_mcp_config 命令直接落表（新导入路径）
    multi_agents_manager_lib::commands::resource::save_mcp_config(
        "v2m1-direct-mcp".into(),
        "node".into(),
        vec![],
        Default::default(),
    )
    .unwrap();
    assert!(
        database::list_extensions().iter().any(|e| e.id == "mcp-v2m1-direct-mcp"),
        "save_mcp_config 应写 extensions 行"
    );
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test mcp_import_and_backfill`
Expected: 编译失败（backfill_registry 不存在）/ 断言失败（save_mcp_config 不落表）

- [x] **Step 3: 实现**

`dao/extension.rs`（`insert_extension` 旁）追加：

```rust
/// 幂等回填：行不存在才插入（INSERT OR IGNORE），不覆盖既有元数据。
/// 预设 v2 数据源统一（spec §8.1）：注册表以 extensions 表为唯一真值源
pub fn ensure_extension(ext: &ExtensionRecord) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR IGNORE INTO extensions (id, kind, name, description, source_path, source_url, version, tags, suite, source_tool, is_native, installed_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
        params![ext.id, ext.kind, ext.name, ext.description, ext.source_path, ext.source_url, ext.version, ext.tags, ext.suite, ext.source_tool, ext.is_native as i64, &now],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}
```

`services/resource/mod.rs` 追加：

```rust
/// 注册表回填（spec §8.1 P2①）：SSOT 目录里有、extensions 表里无行的资源补登记。
/// 启动时调用，幂等——预设创建列表（查表）与资源卡片（扫目录）从此同源
pub fn backfill_registry() {
    let home = dirs::home_dir().unwrap_or_default();

    // skill：~/.mam/skills/<dir>
    let skills = home.join(".mam").join("skills");
    if skills.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&skills) {
            for e in entries.flatten() {
                let path = e.path();
                if !path.is_dir() {
                    continue;
                }
                if let Some(name) = e.file_name().to_str() {
                    let _ = crate::database::ensure_extension(&crate::database::ExtensionRecord {
                        id: format!("skill-{}", name),
                        kind: "skill".to_string(),
                        name: name.to_string(),
                        description: None,
                        source_path: path.to_string_lossy().to_string(),
                        source_url: None,
                        version: None,
                        tags: None,
                        suite: None,
                        source_tool: None,
                        is_native: false,
                    });
                }
            }
        }
    }

    // mcp：~/.mam/mcp/<stem>.json
    let mcp = home.join(".mam").join("mcp");
    if mcp.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&mcp) {
            for e in entries.flatten() {
                let path = e.path();
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    let _ = crate::database::ensure_extension(&crate::database::ExtensionRecord {
                        id: format!("mcp-{}", stem),
                        kind: "mcp".to_string(),
                        name: stem.to_string(),
                        description: None,
                        source_path: path.to_string_lossy().to_string(),
                        source_url: None,
                        version: None,
                        tags: None,
                        suite: None,
                        source_tool: None,
                        is_native: false,
                    });
                }
            }
        }
    }
}
```

`commands/resource.rs`——`import_mcp_to_ssot` 在 `std::fs::write(&config_file, &pretty)` 成功后、`return Ok(())` 前加：

```rust
            // 预设 v2 数据源统一（spec §8.1）：MCP 入 SSOT 必须登记，否则预设列表看不到
            if let Err(e) = crate::database::insert_extension(&crate::database::ExtensionRecord {
                id: format!("mcp-{}", mcp_name),
                kind: "mcp".to_string(),
                name: mcp_name.clone(),
                description: None,
                source_path: config_file.to_string_lossy().to_string(),
                source_url: None,
                version: None,
                tags: None,
                suite: None,
                source_tool: None,
                is_native: false,
            }) {
                log::warn!("MCP {} 登记 extensions 失败: {}", mcp_name, e);
            }
```

`save_mcp_config` 在 `std::fs::write` 成功后加同样的 `insert_extension`（用 `?` 传播错误——新建路径不该静默）。

`:182` 的 `let _ = crate::database::insert_extension(&ext);` 改为：

```rust
        if let Err(e) = crate::database::insert_extension(&ext) {
            log::warn!("原生技能 {} 登记失败: {}", name, e);
        }
```

`lib.rs:33-39` 启动线程改为：

```rust
    std::thread::spawn(|| {
        services::pet::sweep_staging();
        // 预设 v2：孤儿暂存回移 + 注册表回填（先于导入/补链，保证表口径就绪）
        services::preset::stash::recover_orphans();
        services::resource::backfill_registry();
        services::auto_import_extensions(false);
        services::sync_imported_skill_links();
    });
```

`database/mod.rs` re-export 补 `ensure_extension`。

- [x] **Step 4: 跑测试 + 全量 + Commit**

Run: `cd src-tauri && cargo test mcp_import_and_backfill && cargo test`
Expected: PASS

```bash
git add src-tauri/src/database/dao/extension.rs src-tauri/src/database/mod.rs src-tauri/src/commands/resource.rs src-tauri/src/services/resource/mod.rs src-tauri/src/lib.rs src-tauri/tests/preset_v2_test.rs
git commit -m "fix(preset-v2): unify data source — MCP import registers rows + registry backfill + no-swallowed-errors (P2)"
```

---

### Task 12: 收尾——不变量守卫、AGENTS.md、全量门禁

**Files:**
- Modify: `src-tauri/src/services/preset/mod.rs`（启动不变量检查入口）
- Modify: `AGENTS.md`（数据目录表补 `~/.mam/stash/`）
- Modify: `docs/engineering/specs/2026-09-14-preset-groups-v2-design.md`（状态行改「M1 已实施」）
- Test: `src-tauri/tests/preset_v2_test.rs`（追加不变量测试）

**Interfaces:**
- Produces: `services::preset::check_snapshot_invariants() -> Vec<String>`——「存在快照 ⟺ 存在激活预设」违背项列表（启动日志告警；spec §3.2 不变量）

- [x] **Step 1: 写失败测试**

```rust
/// 不变量（spec §3.2）：快照在而 active 为空（或反之）→ 检查器报告
#[test]
fn snapshot_invariant_detector_reports_broken_state() {
    support::setup();
    use multi_agents_manager_lib::database::{self, BaseSnapshotItemRecord};
    use multi_agents_manager_lib::services::preset::check_snapshot_invariants;

    // 人为构造破坏态：快照在、active 为 None
    database::save_base_snapshot(
        "v2m1-inv-tool",
        None,
        &[BaseSnapshotItemRecord {
            extension_id: "skill-v2m1-inv".into(),
            kind: "skill".into(),
            origin: "mam".into(),
        }],
    )
    .unwrap();
    let broken = check_snapshot_invariants();
    assert!(broken.iter().any(|s| s.contains("v2m1-inv-tool")), "{:?}", broken);
    database::destroy_base_snapshot("v2m1-inv-tool").unwrap();
    assert!(check_snapshot_invariants().is_empty());
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test snapshot_invariant`
Expected: 编译失败

- [x] **Step 3: 实现守卫并接线**

`services/preset/mod.rs` 追加：

```rust
/// 快照不变量检查（spec §3.2）：存在基底快照 ⟺ 存在激活预设。
/// 违背项描述列表；启动时 log::warn，M2 UI 提示恢复或废弃
pub fn check_snapshot_invariants() -> Vec<String> {
    let mut broken = Vec::new();
    for tool in crate::adapter::TOOL_IDS {
        match database::get_base_snapshot(tool) {
            Some((None, _)) => broken.push(format!("{}：快照存在但无激活预设", tool)),
            Some((Some(_), _)) | None => {}
        }
    }
    broken
}
```

`lib.rs` 启动线程（Task 11 改过的块）里 `recover_orphans()` 之后加：

```rust
        for msg in services::preset::check_snapshot_invariants() {
            log::warn!("预设快照不变量违背: {}", msg);
        }
```

- [x] **Step 4: 文档更新**

`AGENTS.md` 数据目录表（`~/.mam/hooks/status-hook.sh` 行后）加：

```markdown
| `~/.mam/stash/` | 预设独占模式的原生技能暂存区（应用时移入、恢复时回移，stash_journal 记账） |
```

spec 头部 `- 状态：待用户审阅` 改为 `- 状态：已批准；M1（后端语义）已实施`。

- [x] **Step 5: 全量门禁**

Run: `cd src-tauri && cargo test && cargo clippy -- -D warnings && cargo fmt --check`
Expected: 全绿（fmt 若有 diff 先 `cargo fmt` 再提交）

Run: `cd /Users/jarvis/Documents/MultiAgents-Manager && pnpm format:check && pnpm lint`
Expected: PASS（本计划未动前端，确认无意外波及）

- [x] **Step 6: Commit**

```bash
git add AGENTS.md docs/engineering/specs/2026-09-14-preset-groups-v2-design.md src-tauri/src/services/preset/mod.rs src-tauri/src/lib.rs src-tauri/tests/preset_v2_test.rs
git commit -m "chore(preset-v2): snapshot invariant guard + AGENTS.md stash dir + spec status"
```

---

## 执行顺序与依赖

```
Task 1 (schema) ─→ Task 2/3/4 (DAO，可并行) ─→ Task 5 (兼容) ─→ Task 6 (stash) ─→ Task 7 (snapshot) ─→ Task 8 (sweep) ─→ Task 9 (编排+W5) ─→ Task 10 (命令) ─→ Task 11 (数据源) ─→ Task 12 (收尾)
```

Task 9 是最大风险点（重写核心编排 + 改 W5），若执行中发现清扫/恢复与 W5 清理语义打架（恢复基底后又立即被 W5 还原），以「先 restore_tool 再 disable_tool_cleanup」的顺序为准并在该任务内补集成测试锁死。
