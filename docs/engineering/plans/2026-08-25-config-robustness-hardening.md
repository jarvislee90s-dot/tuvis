# 配置写入健壮性加固 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实施 `specs/016-config-robustness-hardening/spec.md`：写前备份与回滚入口、严格读取拒写保护、DB 自动备份、MCP 单条校验、配置漂移检测、每工具配置互斥锁（借鉴 cc-switch 的多级防护）。

**Architecture:** 三块基础设施优先（备份 / 严格读取 / 锁，Task 1-4、6），再叠功能（DB 备份 Task 4、备份管理 UI Task 5、MCP 校验 Task 7、漂移检测 Task 8）。所有对工具配置文件的写入收敛到「per-tool 进程内锁 → 严格读取 → 修改 → 备份 → 原子写」单一管线。

**Tech Stack:** Rust（tauri 2、rusqlite `VACUUM INTO`、dashmap、once_cell、fs2）/ React 19 + TypeScript。

**环境**：Windows（Git Bash）。Rust 命令在 `src-tauri/` 下执行。debug 构建（`cargo test`）下 `MAM_HOME` 环境变量可重定向 `~/.mam`，测试用互斥锁保护 env 变更。前端 `pnpm check`。

**与 015 的依赖关系**（spec 建议顺序：016 基础设施 → 015 → 016 收尾）：
- Task 2 会替换 015 Task 5 改造过的 JSONC 读取行——两个计划先后的行号可能漂移，替换以「写路径中所有 `unwrap_or_else(|_| "{}".to_string())` / `unwrap_or_default()` 模式」为准。
- Task 8 依赖 015 Task 8 的 `LinkHealth` / `check_link_health`；若 015 未执行，先单独落地 015 Task 8。
- Task 8 的 `install_to_repo(..., false)` 三参形式依赖 015 Task 10；未执行时去掉第三参。

---

### Task 1: 写前备份核心 + write_config_locked 集成（P0，spec §1 / 用户故事 1 场景 1、5）

**Files:**
- Modify: `src-tauri/src/database/connection.rs`（`app_data_home` 改 pub + `mam_dir`）
- Create: `src-tauri/src/linker/backup.rs`
- Modify: `src-tauri/src/linker/mod.rs`（声明模块 + `write_config_locked` 集成）

- [ ] **Step 1: 暴露 mam_dir**

`src-tauri/src/database/connection.rs`：`fn app_data_home()` 改为 `pub fn app_data_home()`，并在其后追加：

```rust
/// MAM 数据目录（~/.mam，debug 构建尊重 MAM_HOME 重定向）
pub fn mam_dir() -> std::path::PathBuf {
    app_data_home().join(".mam")
}
```

- [ ] **Step 2: 写失败测试**

新建 `src-tauri/src/linker/backup.rs`，先写测试与空实现：

```rust
// 配置备份：write_config_locked 写入前的旧内容快照 + 保留策略
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const CONFIG_BACKUP_KEEP: usize = 10;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigBackupEntry {
    pub original_path: String,
    pub file: String,
    pub size: u64,
}

/// 写入前备份（尽力而为：任何失败仅告警，不阻断主写入）
pub fn backup_config_file(path: &Path) {
    let _ = path;
}

pub fn list_backups() -> Vec<ConfigBackupEntry> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// env 变更互斥：MAM_HOME 是进程级变量，测试串行使用
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_mam_home<T>(f: impl FnOnce() -> T) -> T {
        let _g = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("MAM_HOME", tmp.path());
        let r = f();
        std::env::remove_var("MAM_HOME");
        r
    }

    #[test]
    fn write_creates_backup_and_retention() {
        with_mam_home(|| {
            let cfg = tempfile::tempdir().unwrap();
            let path = cfg.path().join("app.json");
            std::fs::write(&path, "{}").unwrap();
            for i in 0..12 {
                let content = format!("{{\"i\":{}}}", i);
                crate::linker::write_config_locked(&path, &content).unwrap();
            }
            let entries = list_backups();
            assert!(!entries.is_empty());
            assert!(entries.len() <= CONFIG_BACKUP_KEEP);
            assert_eq!(entries[0].original_path, path.to_string_lossy().to_string());
        });
    }
}
```

- [ ] **Step 3: 运行测试确认失败**

Run: `cd src-tauri && cargo test backup::tests`
Expected: FAIL（`entries.is_empty()` 断言失败——backup_config_file 是空实现）

- [ ] **Step 4: 实现备份核心**

`backup.rs` 顶部实现替换：

```rust
// 配置备份：write_config_locked 写入前的旧内容快照 + 保留策略
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const CONFIG_BACKUP_KEEP: usize = 10;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigBackupEntry {
    pub original_path: String,
    pub file: String,
    pub size: u64,
}

fn backup_root() -> PathBuf {
    crate::database::connection::mam_dir().join("backups").join("config")
}

/// 目标文件标识：全路径转安全目录名（每个目标文件一个子目录）
fn key_of(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect()
}

/// 写入前备份（尽力而为：任何失败仅告警，不阻断主写入）
pub fn backup_config_file(path: &Path) {
    let content = match std::fs::read(path) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("读取待备份配置失败 {:?}: {}", path, e);
            return;
        }
    };
    let dir = backup_root().join(key_of(path));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("创建备份目录失败 {:?}: {}", dir, e);
        return;
    }
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let mut dest = dir.join(format!("{}.bak", ts));
    let mut seq = 1;
    while dest.exists() {
        dest = dir.join(format!("{}-{}.bak", ts, seq));
        seq += 1;
    }
    if let Err(e) = std::fs::write(&dest, &content) {
        log::warn!("写入备份失败 {:?}: {}", dest, e);
        return;
    }
    enforce_retention(&dir);
}

/// 同秒冲突时序号即文件名字典序，直接按名排序淘汰最旧
fn enforce_retention(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |e| e == "bak"))
        .collect();
    files.sort();
    while files.len() > CONFIG_BACKUP_KEEP {
        let oldest = files.remove(0);
        let _ = std::fs::remove_file(oldest);
    }
}

pub fn list_backups() -> Vec<ConfigBackupEntry> {
    let root = backup_root();
    let mut out = Vec::new();
    if let Ok(keys) = std::fs::read_dir(&root) {
        for key_dir in keys.flatten() {
            let original_path = key_dir.file_name().to_string_lossy().to_string();
            if let Ok(files) = std::fs::read_dir(key_dir.path()) {
                for f in files.flatten() {
                    if let Ok(meta) = f.metadata() {
                        out.push(ConfigBackupEntry {
                            original_path: original_path.clone(),
                            file: f.file_name().to_string_lossy().to_string(),
                            size: meta.len(),
                        });
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| b.file.cmp(&a.file));
    out
}

/// 读取一份备份内容（恢复用）
pub fn read_backup(original_path: &str, file: &str) -> Result<Vec<u8>, String> {
    let p = backup_root().join(key_of(Path::new(original_path))).join(file);
    std::fs::read(&p).map_err(|e| format!("读取备份失败: {}", e))
}

/// 删除一份备份
pub fn delete_backup(original_path: &str, file: &str) -> Result<(), String> {
    let p = backup_root().join(key_of(Path::new(original_path))).join(file);
    std::fs::remove_file(&p).map_err(|e| format!("删除备份失败: {}", e))
}
```

注：`key_of` 把原始路径编进目录名，恢复时直接用 entry 的 `original_path` 字段还原 key，无需反向解码。

- [ ] **Step 5: 集成到 write_config_locked**

`src-tauri/src/linker/mod.rs`：模块声明区加 `pub mod backup;`。`write_config_locked`（175-196 行）在 `file.lock_exclusive()` 成功之后、闭包执行之前插入一行：

```rust
    file.lock_exclusive()
        .map_err(|e| format!("获取文件锁失败: {}", e))?;
    let result = (|| {
        // 写入前备份旧内容（尽力而为）
        if path.exists() {
            backup::backup_config_file(path);
        }
        let temp = path.with_extension("tmp");
```

- [ ] **Step 6: 运行测试确认通过**

Run: `cd src-tauri && cargo test backup::tests`
Expected: PASS

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/database/connection.rs src-tauri/src/linker/backup.rs src-tauri/src/linker/mod.rs
git commit -m "feat(linker): pre-write config backups with 10-file retention in write_config_locked"
```

---

### Task 2: 严格读取 read_config_exact（P0，spec §2 / 用户故事 2）

**Files:**
- Modify: `src-tauri/src/linker/mod.rs`（新增函数 + 测试）
- Modify: `src-tauri/src/services/mcp/mod.rs`（6 处）
- Modify: `src-tauri/src/services/plugin/mod.rs`（4 处）
- Modify: `src-tauri/src/commands/resource.rs`（`import_mcp_to_ssot` 内读取）

- [ ] **Step 1: 写失败测试**

`linker/mod.rs` 末尾追加：

```rust
#[cfg(test)]
mod read_exact_tests {
    use super::*;

    #[test]
    fn missing_file_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_config_exact(&tmp.path().join("none.json")).unwrap().is_none());
    }

    #[test]
    fn existing_file_is_some() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.json");
        std::fs::write(&p, "{}").unwrap();
        assert_eq!(read_config_exact(&p).unwrap().as_deref(), Some("{}"));
    }

    #[test]
    fn unreadable_target_is_error_not_none() {
        let tmp = tempfile::tempdir().unwrap();
        // 目录不是可读文件：read_to_string 报错且非 NotFound → 必须 Err
        assert!(read_config_exact(tmp.path()).is_err());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test read_exact_tests`
Expected: 编译失败 `cannot find function read_config_exact`

- [ ] **Step 3: 实现**

`linker/mod.rs` 的 `write_config_locked` 之前追加：

```rust
/// 严格读取配置文件：仅 NotFound 视为"文件不存在"（调用方可起草新文件），
/// 其余错误（权限/被锁/编码）一律 Err，禁止用空对象覆写用户配置。
pub fn read_config_exact(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("读取配置文件失败 {:?}: {}", path, e)),
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd src-tauri && cargo test read_exact_tests`
Expected: PASS（3 个）

- [ ] **Step 5: 替换写路径的全部宽松读取**

统一模式（JSON）：

```rust
let content = crate::linker::read_config_exact(path)?.unwrap_or_else(|| "{}".to_string());
```

TOML 用 `.unwrap_or_default()`。逐处替换：

1. `services/mcp/mod.rs`：`write_mcp_json` / `remove_mcp_json` / `write_mcp_toml` / `remove_mcp_toml` / `write_mcp_jsonc` / `remove_mcp_jsonc` 六个函数开头的 `std::fs::read_to_string(path).unwrap_or_else(|_| "{}".to_string())` 与 `.unwrap_or_default()`（若 015 Task 5 已改造后两个函数，替换其现有读取行，jsonc span 编辑逻辑不动）。
2. `services/plugin/mod.rs`：`enable_config_plugin` / `disable_config_plugin` 中 JSON、Jsonc、TOML 分支的 4 处读取（`config_path` 变量），模式同上。
3. `commands/resource.rs` `import_mcp_to_ssot`（约 570-628 行）：从工具配置提取 MCP 的读取改为 `read_config_exact`，`None` 时返回 `Err("工具配置文件不存在")`（该函数输出会写入 SSOT，必须严格）。

仅用于展示的读取（`list_ssot_resources`、`read_mcp_servers`）**保持宽松不动**。

- [ ] **Step 6: 全量测试 + 提交**

Run: `cd src-tauri && cargo test && cargo clippy`
Expected: PASS

```bash
git add src-tauri/src/linker/mod.rs src-tauri/src/services/mcp/mod.rs src-tauri/src/services/plugin/mod.rs src-tauri/src/commands/resource.rs
git commit -m "fix(config): strict config reads — non-NotFound errors reject writes instead of drafting empty objects"
```

---

### Task 3: hook 备份统一（P0，spec §1 场景 4）

**Files:**
- Modify: `src-tauri/src/monitor/hooks.rs:146-151`

- [ ] **Step 1: 删除独立 .bak 逻辑**

`monitor/hooks.rs` 的 `if added > 0 {` 块中删除：

```rust
        // 创建备份（防止写入失败导致配置丢失）
        if config_path.exists() {
            let backup = config_path.with_extension("json.bak");
            let _ = fs::copy(config_path, &backup);
        }
```

（`write_config_locked` 已在 Task 1 统一做写前备份，且带保留策略；保留此处会造成双份备份。）

- [ ] **Step 2: 编译 + 提交**

Run: `cd src-tauri && cargo check`
Expected: 通过（若 `fs` import 因删除而未使用，移除该 import）

```bash
git add src-tauri/src/monitor/hooks.rs
git commit -m "refactor(hooks): rely on unified pre-write backup in write_config_locked"
```

---

### Task 4: 数据库自动备份（P1，spec §3 / 用户故事 3）

**Files:**
- Create: `src-tauri/src/database/backup.rs`
- Modify: `src-tauri/src/database/mod.rs`（声明 + init 集成）
- Modify: `src-tauri/src/lib.rs`（周期备份后台线程，并入 015 Task 7 的启动线程）

- [ ] **Step 1: 写失败测试**

新建 `src-tauri/src/database/backup.rs`：

```rust
// 数据库备份：迁移前 + 周期（24h），VACUUM INTO 一致性快照（含 WAL 内容）
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const DB_BACKUP_KEEP: usize = 10;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbBackupEntry {
    pub file: String,
    pub size: u64,
}

pub fn db_backup_dir() -> PathBuf {
    crate::database::connection::mam_dir().join("backups").join("db")
}

pub fn backup_database(conn: &rusqlite::Connection) -> Result<PathBuf, String> {
    let _ = conn;
    Ok(PathBuf::new())
}

pub fn periodic_backup_if_needed() {
    let _ = "";
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_mam_home<T>(f: impl FnOnce() -> T) -> T {
        let _g = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("MAM_HOME", tmp.path());
        let r = f();
        std::env::remove_var("MAM_HOME");
        r
    }

    #[test]
    fn vacuum_backup_is_openable_and_complete() {
        with_mam_home(|| {
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            crate::database::schema::init(&conn);
            conn.execute(
                "INSERT INTO extensions (id, kind, name, source_path, installed_at, updated_at) \
                 VALUES ('skill-t','skill','t','/t','now','now')",
                [],
            )
            .unwrap();
            let dest = backup_database(&conn).unwrap();
            assert!(dest.exists());
            let backup = rusqlite::Connection::open(&dest).unwrap();
            let n: i64 = backup
                .query_row("SELECT COUNT(*) FROM extensions WHERE id='skill-t'", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 1);
        });
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test backup_database_tests`
Expected: FAIL（`dest.exists()` 为 false）

- [ ] **Step 3: 实现**

`backup.rs` 中 `backup_database` / `periodic_backup_if_needed` 替换为：

```rust
pub fn backup_database(conn: &rusqlite::Connection) -> Result<PathBuf, String> {
    let dir = db_backup_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建 DB 备份目录失败: {}", e))?;
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let mut dest = dir.join(format!("mam_{}.db", ts));
    let mut seq = 1;
    while dest.exists() {
        dest = dir.join(format!("mam_{}-{}.db", ts, seq));
        seq += 1;
    }
    // VACUUM INTO 产出含 WAL 内容的一致性快照，无需 checkpoint
    conn.execute("VACUUM INTO ?1", [dest.to_string_lossy().to_string()])
        .map_err(|e| format!("VACUUM INTO 失败: {}", e))?;
    enforce_retention(&dir);
    Ok(dest)
}

/// 距最近备份超过 24h 时补一次（用独立连接，避免长持 DB Mutex）
pub fn periodic_backup_if_needed() {
    let dir = db_backup_dir();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let latest = entries
            .flatten()
            .filter_map(|e| e.metadata().ok().and_then(|m| m.modified().ok()))
            .max();
        if let Some(t) = latest {
            if t.elapsed().map(|d| d.as_secs() < 24 * 3600).unwrap_or(true) {
                return;
            }
        }
    }
    match crate::database::connection::open() {
        Ok(conn) => {
            if let Err(e) = backup_database(&conn) {
                log::error!("周期 DB 备份失败: {}", e);
            }
        }
        Err(e) => log::error!("打开 DB 失败，跳过周期备份: {}", e),
    }
}

fn enforce_retention(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |e| e == "db"))
        .collect();
    files.sort();
    while files.len() > DB_BACKUP_KEEP {
        let oldest = files.remove(0);
        let _ = std::fs::remove_file(oldest);
    }
}

pub fn list_db_backups() -> Vec<DbBackupEntry> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(db_backup_dir()) {
        for e in entries.flatten() {
            if let Ok(meta) = e.metadata() {
                out.push(DbBackupEntry {
                    file: e.file_name().to_string_lossy().to_string(),
                    size: meta.len(),
                });
            }
        }
    }
    out.sort_by(|a, b| b.file.cmp(&a.file));
    out
}
```

（测试模块名 `backup_database_tests` 不存在——直接用 `mod tests`，Step 2 命令相应为 `cargo test database::backup`。）

- [ ] **Step 4: 运行测试确认通过**

Run: `cd src-tauri && cargo test database::backup`
Expected: PASS

- [ ] **Step 5: 迁移前备份 + 周期备份接线**

`src-tauri/src/database/mod.rs`：顶部加 `pub mod backup;`；`init`（33-38 行）替换为：

```rust
pub fn init() {
    Lazy::force(&DB);
    if let Ok(conn) = connection::open() {
        // 迁移前必做完整备份（失败不阻断迁移——迁移本身幂等）
        if let Err(e) = backup::backup_database(&conn) {
            log::error!("迁移前 DB 备份失败: {}", e);
        }
        let _ = migration::migrate(&conn);
    }
}
```

`src-tauri/src/lib.rs`：015 Task 7 的启动后台线程中追加一行（若 015 未执行，则在 `database::init()` 后新建同样线程）：

```rust
    std::thread::spawn(|| {
        crate::database::backup::periodic_backup_if_needed();
        services::auto_import_extensions(false);
        services::sync_imported_skill_links();
    });
```

- [ ] **Step 6: 验证 + 提交**

Run: `cd src-tauri && cargo test && cargo check`
Expected: PASS

```bash
git add src-tauri/src/database/backup.rs src-tauri/src/database/mod.rs src-tauri/src/lib.rs
git commit -m "feat(db): pre-migration and periodic (24h) VACUUM INTO backups with 10-file retention"
```

---

### Task 5: 备份管理 IPC + 设置页「备份」区块（P0/P1，spec §1 场景 2-3、§3 场景 4）

**Files:**
- Create: `src-tauri/src/commands/backup.rs`
- Modify: `src-tauri/src/commands/mod.rs`（`pub mod backup;`）、`src-tauri/src/lib.rs`（注册 4 个命令）
- Create: `src/lib/api/backup.ts`
- Create: `src/components/settings/ConfigBackupSection.tsx`
- Modify: `src/pages/settings.tsx`、i18n 两文件

- [ ] **Step 1: 后端 IPC**

`src-tauri/src/commands/backup.rs`：

```rust
// 备份管理命令

#[tauri::command]
pub fn list_config_backups() -> Vec<crate::linker::backup::ConfigBackupEntry> {
    crate::linker::backup::list_backups()
}

/// 恢复一份配置备份：读 .bak 内容 → 原子写回原路径（恢复前会再备份当前内容，双向可退）
#[tauri::command]
pub fn restore_config_backup(original_path: String, file: String) -> Result<(), String> {
    let content = crate::linker::backup::read_backup(&original_path, &file)?;
    let text = String::from_utf8(content).map_err(|e| format!("备份内容非 UTF-8: {}", e))?;
    crate::linker::write_config_locked(std::path::Path::new(&original_path), &text)
}

#[tauri::command]
pub fn delete_config_backup(original_path: String, file: String) -> Result<(), String> {
    crate::linker::backup::delete_backup(&original_path, &file)
}

#[tauri::command]
pub fn list_db_backups() -> Vec<crate::database::backup::DbBackupEntry> {
    crate::database::backup::list_db_backups()
}

#[tauri::command]
pub fn open_db_backup_dir() -> Result<(), String> {
    let dir = crate::database::backup::db_backup_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    tauri_plugin_opener::open_path(dir.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}
```

`commands/mod.rs` 加 `pub mod backup;`；`lib.rs` 的 `generate_handler!` 列表追加：

```rust
        commands::backup::list_config_backups,
        commands::backup::restore_config_backup,
        commands::backup::delete_config_backup,
        commands::backup::list_db_backups,
        commands::backup::open_db_backup_dir,
```

- [ ] **Step 2: 前端 API**

`src/lib/api/backup.ts`：

```ts
import { invoke } from "@tauri-apps/api/core";

export interface ConfigBackupEntry {
  originalPath: string;
  file: string;
  size: number;
}
export interface DbBackupEntry {
  file: string;
  size: number;
}

export async function listConfigBackups() {
  return await invoke<ConfigBackupEntry[]>("list_config_backups");
}
export async function restoreConfigBackup(originalPath: string, file: string) {
  return await invoke("restore_config_backup", { originalPath, file });
}
export async function deleteConfigBackup(originalPath: string, file: string) {
  return await invoke("delete_config_backup", { originalPath, file });
}
export async function listDbBackups() {
  return await invoke<DbBackupEntry[]>("list_db_backups");
}
export async function openDbBackupDir() {
  return await invoke("open_db_backup_dir");
}
```

- [ ] **Step 3: 设置区块组件**

`src/components/settings/ConfigBackupSection.tsx`：

```tsx
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { RefreshCw, RotateCcw, Trash2, FolderOpen } from "lucide-react";
import {
  listConfigBackups,
  restoreConfigBackup,
  deleteConfigBackup,
  listDbBackups,
  openDbBackupDir,
  type ConfigBackupEntry,
  type DbBackupEntry,
} from "@/lib/api/backup";

function fmtSize(bytes: number) {
  return bytes < 1024 ? `${bytes} B` : `${(bytes / 1024).toFixed(1)} KB`;
}

export function ConfigBackupSection() {
  const { t } = useTranslation();
  const [configBackups, setConfigBackups] = useState<ConfigBackupEntry[]>([]);
  const [dbBackups, setDbBackups] = useState<DbBackupEntry[]>([]);

  const load = async () => {
    try {
      setConfigBackups(await listConfigBackups());
      setDbBackups(await listDbBackups());
    } catch (e) {
      console.error(e);
    }
  };
  useEffect(() => {
    load();
  }, []);

  const restore = async (b: ConfigBackupEntry) => {
    if (!window.confirm(t("settings.backupRestoreConfirm", { file: b.originalPath }))) return;
    try {
      await restoreConfigBackup(b.originalPath, b.file);
      toast.success(t("settings.backupRestored"));
      load();
    } catch (e) {
      toast.error(t("common.operationFailed", { error: e }));
    }
  };

  const remove = async (b: ConfigBackupEntry) => {
    try {
      await deleteConfigBackup(b.originalPath, b.file);
      load();
    } catch (e) {
      toast.error(t("common.operationFailed", { error: e }));
    }
  };

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h3 className="text-sm font-semibold">{t("settings.configBackupTitle")}</h3>
        <Button variant="ghost" size="sm" onClick={load}>
          <RefreshCw className="mr-1 h-3.5 w-3.5" />
          {t("common.refresh")}
        </Button>
      </div>

      {configBackups.length === 0 ? (
        <p className="text-muted-foreground text-xs">{t("settings.noConfigBackups")}</p>
      ) : (
        <div className="space-y-1">
          {configBackups.map((b) => (
            <div key={`${b.originalPath}/${b.file}`} className="flex items-center justify-between rounded border p-2 text-xs">
              <div className="min-w-0">
                <p className="truncate font-medium">{b.originalPath}</p>
                <p className="text-muted-foreground">
                  {b.file} · {fmtSize(b.size)}
                </p>
              </div>
              <div className="flex gap-1">
                <Button variant="outline" size="sm" className="h-6 px-2 text-[10px]" onClick={() => restore(b)}>
                  <RotateCcw className="mr-1 h-3 w-3" />
                  {t("settings.backupRestore")}
                </Button>
                <Button variant="ghost" size="sm" className="h-6 px-2 text-[10px] text-destructive" onClick={() => remove(b)}>
                  <Trash2 className="h-3 w-3" />
                </Button>
              </div>
            </div>
          ))}
        </div>
      )}

      <div className="flex items-center justify-between pt-2">
        <h3 className="text-sm font-semibold">{t("settings.dbBackupTitle")}</h3>
        <Button variant="ghost" size="sm" onClick={() => openDbBackupDir().catch(console.error)}>
          <FolderOpen className="mr-1 h-3.5 w-3.5" />
          {t("settings.openBackupDir")}
        </Button>
      </div>
      {dbBackups.length === 0 ? (
        <p className="text-muted-foreground text-xs">{t("settings.noDbBackups")}</p>
      ) : (
        <div className="space-y-1">
          {dbBackups.map((b) => (
            <div key={b.file} className="flex items-center justify-between rounded border p-2 text-xs">
              <span>{b.file}</span>
              <span className="text-muted-foreground">{fmtSize(b.size)}</span>
            </div>
          ))}
        </div>
      )}
      <p className="text-muted-foreground text-[10px]">{t("settings.dbBackupHint")}</p>
    </div>
  );
}
```

- [ ] **Step 4: 挂到设置页**

`src/pages/settings.tsx`：`SettingSection` 类型改为 `"appearance" | "shortcut" | "notifications" | "backup"`；在现有三个区块导航按钮旁（同一容器、同样式）追加导航按钮：

```tsx
<Button
  variant={activeSection === "backup" ? "default" : "ghost"}
  size="sm"
  onClick={() => setActiveSection("backup")}
>
  <DatabaseBackup className="mr-1 h-3.5 w-3.5" />
  {t("settings.backup")}
</Button>
```

（lucide import 追加 `DatabaseBackup`；按钮样式对齐相邻 appearance/shortcut/notifications 按钮。）区块渲染处（现有 `activeSection === "xxx"` 条件渲染旁）追加：

```tsx
{activeSection === "backup" && <ConfigBackupSection />}
```

（import：`import { ConfigBackupSection } from "@/components/settings/ConfigBackupSection";`）

- [ ] **Step 5: i18n 键**

`zh.json` settings 段追加：

```json
"backup": "备份",
"configBackupTitle": "配置文件备份（写入前自动快照，每文件保留 10 份）",
"noConfigBackups": "暂无配置备份",
"backupRestore": "恢复",
"backupRestoreConfirm": "将用备份覆盖当前文件：\n{{file}}\n恢复前会自动再备份当前内容。继续？",
"backupRestored": "已恢复",
"dbBackupTitle": "数据库备份（迁移前 + 每 24 小时，保留 10 份）",
"noDbBackups": "暂无数据库备份",
"openBackupDir": "打开备份目录",
"dbBackupHint": "恢复数据库需先完全退出应用，再将备份文件复制替换 ~/.mam/mam.db。"
```

`en.json` 对应：`"Backup"`, `"Config file backups (auto snapshot before writes, keep 10 per file)"`, `"No config backups"`, `"Restore"`, "Overwrite the current file with this backup?\n{{file}}\nThe current content will be backed up first. Continue?", `"Restored"`, `"Database backups (pre-migration + every 24h, keep 10)"`, `"No database backups"`, `"Open backup folder"`, "To restore the database, fully quit the app first, then copy a backup over ~/.mam/mam.db."

`common` 段若缺 `refresh` 键则两语言各补 `"refresh": "刷新"` / `"refresh": "Refresh"`。

- [ ] **Step 6: 验证 + 提交**

Run: `cd src-tauri && cargo check && cd .. && pnpm check`
Expected: 通过

```bash
git add src-tauri/src/commands/backup.rs src-tauri/src/commands/mod.rs src-tauri/src/lib.rs src/lib/api/backup.ts src/components/settings/ConfigBackupSection.tsx src/pages/settings.tsx src/i18n/locales/zh.json src/i18n/locales/en.json
git commit -m "feat(settings): backup management UI — config restore/delete and db backup list"
```

---

### Task 6: 每工具配置互斥锁（P1，spec §6 / 用户故事 6）

**Files:**
- Create: `src-tauri/src/linker/tool_config_lock.rs`
- Modify: `src-tauri/src/linker/mod.rs`（声明）
- Modify: `src-tauri/src/services/mcp/mod.rs`（`write_mcp` / `remove_mcp` 包裹）
- Modify: `src-tauri/src/services/plugin/mod.rs`（config 启停包裹）
- Modify: `src-tauri/src/monitor/hooks.rs`（注册写入包裹 `"claude"`）
- Test: `src-tauri/src/services/mcp/mod.rs`（并发测试）

- [ ] **Step 1: 写失败测试**

`services/mcp/mod.rs` 末尾追加：

```rust
#[cfg(test)]
mod tool_lock_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn cfg(tag: &str) -> McpConfig {
        McpConfig {
            command: format!("cmd-{}", tag),
            args: vec![],
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn concurrent_writes_same_tool_both_keys_survive() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("app.json");
        std::fs::write(&path, "{}").unwrap();
        let p1 = path.clone();
        let p2 = path.clone();
        let h1 = std::thread::spawn(move || {
            crate::linker::tool_config_lock::with_tool_config_lock("claude", || {
                write_mcp_json(&p1, "alpha", &cfg("a"))
            })
        });
        let h2 = std::thread::spawn(move || {
            crate::linker::tool_config_lock::with_tool_config_lock("claude", || {
                write_mcp_json(&p2, "beta", &cfg("b"))
            })
        });
        h1.join().unwrap().unwrap();
        h2.join().unwrap().unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"alpha\""), "丢失 alpha: {}", content);
        assert!(content.contains("\"beta\""), "丢失 beta: {}", content);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test tool_lock_tests`
Expected: 编译失败 `could not find tool_config_lock`

- [ ] **Step 3: 实现锁模块**

新建 `src-tauri/src/linker/tool_config_lock.rs`：

```rust
// 每工具配置互斥锁：进程内串行化对同一工具配置文件的完整 读→改→写 序列。
// 文件锁（write_config_locked）保留为跨进程最后防线；本锁修 MAM 自身 read 在文件锁外的竞态。

use dashmap::DashMap;
use once_cell::sync::Lazy;
use std::sync::{Arc, Mutex};

static LOCKS: Lazy<DashMap<String, Arc<Mutex<()>>>> = Lazy::new(DashMap::new);

pub fn with_tool_config_lock<T>(tool_id: &str, f: impl FnOnce() -> T) -> T {
    let lock = LOCKS
        .entry(tool_id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    let _guard = lock.lock().unwrap();
    f()
}
```

`linker/mod.rs` 模块声明区加 `pub mod tool_config_lock;`。

- [ ] **Step 4: 包裹写路径入口**

1. `services/mcp/mod.rs`：`write_mcp` 与 `remove_mcp` 的函数体整体包进锁（格式判定在外，业务在内）：

```rust
pub fn write_mcp(tool_id: &str, mcp_name: &str, config: &McpConfig) -> Result<(), String> {
    let (format, config_path) = get_tool_mcp_info(tool_id)?;
    crate::linker::tool_config_lock::with_tool_config_lock(tool_id, || match format {
        McpFormat::Json => write_mcp_json(&config_path, mcp_name, config),
        McpFormat::Toml => write_mcp_toml(&config_path, mcp_name, config),
        McpFormat::Jsonc => write_mcp_jsonc(&config_path, mcp_name, config),
    })
}
```

`remove_mcp` 同样处理。

2. `services/plugin/mod.rs`：`enable_config_plugin` / `disable_config_plugin` 中从读取 `content` 开始到 `write_config_locked` 结束的段落包进 `crate::linker::tool_config_lock::with_tool_config_lock(tool_id, || { ... })`（adapter 与 `config_paths` 判定可在锁外；锁内包含读→改→写全序列）。
3. `monitor/hooks.rs`：最终写配置的 `write_config_locked(...)` 一行包进 `with_tool_config_lock("claude", || ...)`（hook 只写 Claude 的 settings.json）。

- [ ] **Step 5: 运行测试确认通过**

Run: `cd src-tauri && cargo test tool_lock_tests && cargo test`
Expected: PASS

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/linker/tool_config_lock.rs src-tauri/src/linker/mod.rs src-tauri/src/services/mcp/mod.rs src-tauri/src/services/plugin/mod.rs src-tauri/src/monitor/hooks.rs
git commit -m "feat(config): per-tool in-process lock serializes full read-modify-write on tool configs"
```

---

### Task 7: MCP 单条校验（P1，spec §4 / 用户故事 4）

**Files:**
- Modify: `src-tauri/src/services/mcp/mod.rs`（validate + find_in_path + 接入 write_mcp + 测试）
- Modify: `src-tauri/src/commands/mcp.rs`（新增 IPC）
- Modify: `src-tauri/src/lib.rs`（注册）
- Modify: `src-tauri/src/commands/resource.rs`（`save_mcp_config` 校验）
- Modify: `src/components/resources/ResourceByKindView.tsx`

- [ ] **Step 1: 写失败测试**

`services/mcp/mod.rs` tests 区追加：

```rust
#[cfg(test)]
mod validate_tests {
    use super::*;

    fn cfg(cmd: &str) -> McpConfig {
        McpConfig { command: cmd.into(), args: vec![], env: Default::default() }
    }

    #[test]
    fn empty_command_is_error() {
        let v = validate_mcp_config(&cfg("   "));
        assert!(!v.errors.is_empty());
    }

    #[test]
    fn valid_command_no_error() {
        let v = validate_mcp_config(&cfg("npx"));
        assert!(v.errors.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn pathext_resolution_finds_exe() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mytool.cmd"), "@echo hi").unwrap();
        assert!(try_extensions(dir.path(), "mytool").is_some());
        assert!(try_extensions(dir.path(), "mytool.exe").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn direct_hit_in_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mytool"), "#!/bin/sh").unwrap();
        assert!(try_extensions(dir.path(), "mytool").is_some());
        assert!(try_extensions(dir.path(), "nope").is_none());
    }

    #[test]
    fn absolute_path_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = if cfg!(windows) { tmp.path().join("tool.exe") } else { tmp.path().join("tool") };
        std::fs::write(&exe, "x").unwrap();
        assert!(find_in_path(exe.to_string_lossy().as_ref()).is_some());
        assert!(find_in_path("Z:/definitely/not/here/tool.exe").is_none());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test validate_tests`
Expected: 编译失败 `cannot find function validate_mcp_config`

- [ ] **Step 3: 实现校验**

`services/mcp/mod.rs` 顶部（`McpConfig` 定义之后）追加：

```rust
/// MCP 单条校验结果：errors 阻断写入，warnings 放行但记录
#[derive(Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpValidation {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn validate_mcp_config(config: &McpConfig) -> McpValidation {
    let mut v = McpValidation::default();
    if config.command.trim().is_empty() {
        v.errors.push("command 不能为空".to_string());
    }
    if find_in_path(&config.command).is_none() {
        v.warnings.push(format!(
            "command '{}' 未在 PATH 中找到，请确认可执行文件存在",
            config.command
        ));
    }
    v
}

/// PATH 查找；带路径分隔符时按文件存在性判断，Windows 按 PATHEXT 补扩展
pub fn find_in_path(command: &str) -> Option<std::path::PathBuf> {
    let as_path = std::path::Path::new(command);
    if command.contains('/') || command.contains('\\') {
        return if as_path.exists() { Some(as_path.to_path_buf()) } else { None };
    }
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        if let Some(hit) = try_extensions(&dir, command) {
            return Some(hit);
        }
    }
    None
}

pub(crate) fn try_extensions(dir: &std::path::Path, command: &str) -> Option<std::path::PathBuf> {
    let direct = dir.join(command);
    if direct.is_file() {
        return Some(direct);
    }
    #[cfg(windows)]
    for ext in [".exe", ".cmd", ".bat", ".ps1"] {
        let p = dir.join(format!("{}{}", command, ext));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd src-tauri && cargo test validate_tests`
Expected: PASS

- [ ] **Step 5: 接入写入口**

`write_mcp`（Task 6 已含锁包裹）在 `get_tool_mcp_info` 之后插入：

```rust
    let validation = validate_mcp_config(config);
    if !validation.errors.is_empty() {
        return Err(format!("MCP 校验失败（{}）: {}", mcp_name, validation.errors.join("; ")));
    }
    for w in &validation.warnings {
        log::warn!("MCP 校验警告（{}）: {}", mcp_name, w);
    }
```

`commands/resource.rs` `save_mcp_config`：构造 `McpConfig` 后、写 SSOT json 之前加同样校验（errors → `Err`）。

- [ ] **Step 6: 校验 IPC + 前端表单接入**

`commands/mcp.rs` 追加：

```rust
#[tauri::command]
pub fn validate_mcp(
    command: String,
    args: Vec<String>,
    env: std::collections::BTreeMap<String, String>,
) -> crate::services::mcp::McpValidation {
    let config = crate::services::mcp::McpConfig { command, args, env };
    crate::services::mcp::validate_mcp_config(&config)
}
```

`lib.rs` 注册 `commands::mcp::validate_mcp,`。

`ResourceByKindView.tsx` 的 `handleAddMcp` 在 `saveMcpConfig` 之前插入：

```tsx
      const v = await invoke<{ errors: string[]; warnings: string[] }>("validate_mcp", {
        command: newMcp.command.trim(),
        args,
        env,
      });
      if (v.errors.length > 0) {
        toast.error(v.errors.join("; "));
        return;
      }
      if (v.warnings.length > 0 && !window.confirm(v.warnings.join("\n"))) {
        return;
      }
```

- [ ] **Step 7: 验证 + 提交**

Run: `cd src-tauri && cargo test && cd .. && pnpm check`
Expected: 通过

```bash
git add src-tauri/src/services/mcp/mod.rs src-tauri/src/commands/mcp.rs src-tauri/src/lib.rs src-tauri/src/commands/resource.rs src/components/resources/ResourceByKindView.tsx
git commit -m "feat(mcp): per-server validation with PATH probing before writing tool configs"
```

---

### Task 8: 配置漂移检测（P1，spec §5 / 用户故事 5）

**Files:**
- Create: `src-tauri/src/services/drift.rs`
- Modify: `src-tauri/src/services/mod.rs`（`pub mod drift;`）
- Create: `src-tauri/src/commands/drift.rs` + `commands/mod.rs` + `lib.rs` 注册
- Create: `src/components/resources/DriftBanner.tsx`
- Modify: `src/components/resources/ExtensionList.tsx`（挂载横幅）
- Modify: i18n 两文件

**前置**：依赖 015 Task 8 的 `linker::check_link_health` / `LinkHealth`（未执行则先落地该任务）。

- [ ] **Step 1: 写失败测试（分类纯函数）**

新建 `src-tauri/src/services/drift.rs`：

```rust
// 配置漂移检测：DB 期望状态 vs 文件系统 / 工具配置实际状态
use crate::linker::LinkHealth;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DriftType {
    LinkMissing,
    LinkDangling,
    RepoMissing,
    McpKeyMissing,
    UnmanagedMcpKey,
    UnmanagedSkillDir,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriftItem {
    pub ext_id: String,
    pub kind: String,
    pub name: String,
    pub tool_id: String,
    pub drift_type: DriftType,
}

pub fn classify_skill(enabled: bool, health: LinkHealth, repo_exists: bool) -> Option<DriftType> {
    match (enabled, health, repo_exists) {
        (true, LinkHealth::Dangling, _) => Some(DriftType::LinkDangling),
        (true, LinkHealth::Missing, _) => Some(DriftType::LinkMissing),
        (_, LinkHealth::NotLink, _) => Some(DriftType::UnmanagedSkillDir),
        (true, LinkHealth::Valid, false) => Some(DriftType::RepoMissing),
        _ => None,
    }
}

pub fn classify_mcp(db_enabled: bool, key_present: bool) -> Option<DriftType> {
    match (db_enabled, key_present) {
        (true, false) => Some(DriftType::McpKeyMissing),
        (false, true) => Some(DriftType::UnmanagedMcpKey),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_classifications() {
        assert_eq!(classify_skill(true, LinkHealth::Valid, true), None);
        assert_eq!(classify_skill(true, LinkHealth::Missing, true), Some(DriftType::LinkMissing));
        assert_eq!(classify_skill(true, LinkHealth::Dangling, true), Some(DriftType::LinkDangling));
        assert_eq!(classify_skill(true, LinkHealth::Valid, false), Some(DriftType::RepoMissing));
        assert_eq!(classify_skill(false, LinkHealth::NotLink, true), Some(DriftType::UnmanagedSkillDir));
        assert_eq!(classify_skill(false, LinkHealth::Missing, true), None);
    }

    #[test]
    fn mcp_classifications() {
        assert_eq!(classify_mcp(true, true), None);
        assert_eq!(classify_mcp(false, false), None);
        assert_eq!(classify_mcp(true, false), Some(DriftType::McpKeyMissing));
        assert_eq!(classify_mcp(false, true), Some(DriftType::UnmanagedMcpKey));
    }
}
```

- [ ] **Step 2: 运行分类测试（随实现一次通过，作为防回归基线）**

Run: `cd src-tauri && cargo test drift::tests`
Expected: PASS（2 个）。这两个纯函数测试与实现同文件落地，是后续 detect/resolve 改动的防回归基线；若失败说明 match 分支与断言不符，先修正分支再继续。

- [ ] **Step 3: 实现 detect_drift 与 resolve_drift**

`drift.rs` 追加（先工具函数，再 detect/resolve）：

```rust
/// 工具配置中的 MCP 键集合（宽容读取：JSONC 解析失败时退化为逐行文本提取，仅用于检测报告，
/// 不参与写路径——写路径走 Task 2 的严格读取）
fn mcp_keys(tool_id: &str) -> Vec<String> {
    use crate::adapter::AgentAdapter;
    let adapter: Box<dyn AgentAdapter> = match tool_id {
        "claude" => Box::new(crate::adapter::claude::ClaudeAdapter),
        "codex" => Box::new(crate::adapter::codex::CodexAdapter),
        "opencode" => Box::new(crate::adapter::opencode::OpenCodeAdapter),
        "openclaw" => Box::new(crate::adapter::openclaw::OpenClawAdapter),
        _ => return Vec::new(),
    };
    let Some(path) = adapter.mcp_config_path() else { return Vec::new() };
    let Ok(content) = std::fs::read_to_string(&path) else { return Vec::new() };
    let section = |v: &serde_json::Value| {
        v.get("mcpServers")
            .or_else(|| v.get("mcp_servers"))
            .or_else(|| v.get("mcp"))
            .and_then(|s| s.as_object().cloned())
    };
    match adapter.mcp_format() {
        crate::adapter::McpFormat::Json => serde_json::from_str::<serde_json::Value>(&content)
            .ok()
            .and_then(|v| section(&v))
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default(),
        crate::adapter::McpFormat::Toml => content
            .parse::<toml::Value>()
            .ok()
            .and_then(|v| serde_json::to_value(v).ok())
            .and_then(|v| section(&v))
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default(),
        // JSONC：注释会让 serde_json 失败，退化为逐行提取 "key": 形态的键
        crate::adapter::McpFormat::Jsonc => content
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim_start();
                let rest = trimmed.strip_prefix('"')?;
                let end = rest.find('"')?;
                Some(rest[..end].to_string())
            })
            .collect(),
    }
}

继续追加：

```rust
pub fn detect_drift() -> Vec<DriftItem> {
    let assignments = crate::database::list_all_assignments();
    let repo = crate::linker::ensure_repo_dir();
    let mut items = Vec::new();

    // skill：assignment 期望 vs 工具目录链接健康 vs SSOT 存在
    for a in assignments.iter().filter(|a| a.extension_id.starts_with("skill-")) {
        let Some(name) = a.extension_id.strip_prefix("skill-") else { continue };
        let Some(tool_dir) = crate::adapter::primary_skill_dir(&a.agent_tool_id) else { continue };
        let health = crate::linker::check_link_health(&tool_dir.join(name));
        let repo_exists = repo.join(name).exists();
        if let Some(ty) = classify_skill(a.enabled, health, repo_exists) {
            items.push(DriftItem {
                ext_id: a.extension_id.clone(),
                kind: "skill".to_string(),
                name: name.to_string(),
                tool_id: a.agent_tool_id.clone(),
                drift_type: ty,
            });
        }
    }

    // mcp：assignment 期望 vs 工具配置键存在
    for a in assignments.iter().filter(|a| a.extension_id.starts_with("mcp-")) {
        let Some(name) = a.extension_id.strip_prefix("mcp-") else { continue };
        let present = mcp_keys(&a.agent_tool_id).iter().any(|k| k == name);
        if let Some(ty) = classify_mcp(a.enabled, present) {
            items.push(DriftItem {
                ext_id: a.extension_id.clone(),
                kind: "mcp".to_string(),
                name: name.to_string(),
                tool_id: a.agent_tool_id.clone(),
                drift_type: ty,
            });
        }
    }

    // 工具配置中存在但 DB 无任何 assignment 的 MCP（用户手加，完全未纳管）
    for tool_id in ["claude", "codex", "opencode", "openclaw"] {
        for key in mcp_keys(tool_id) {
            let ext_id = format!("mcp-{}", key);
            if !assignments.iter().any(|a| a.extension_id == ext_id) {
                items.push(DriftItem {
                    ext_id,
                    kind: "mcp".to_string(),
                    name: key,
                    tool_id: tool_id.to_string(),
                    drift_type: DriftType::UnmanagedMcpKey,
                });
            }
        }
    }

    items.sort_by(|a, b| a.name.cmp(&b.name));
    items
}

/// 处理一条漂移。mam_first=true 以 MAM 记录为准（修复链接/回写配置/移除多余键），
/// false 以工具现状为准（导入 SSOT / 更新 assignment）。
pub fn resolve_drift(item: &DriftItem, mam_first: bool) -> Result<(), String> {
    match item.drift_type {
        DriftType::LinkMissing | DriftType::LinkDangling => {
            if mam_first {
                crate::services::enable_skill_for_tool(&item.name, &item.tool_id)
            } else {
                crate::database::upsert_assignment(&item.ext_id, &item.tool_id, false, "missing")
                    .map(|_| ())
            }
        }
        DriftType::RepoMissing => {
            if mam_first {
                Err(format!(
                    "SSOT 仓库中已无 {}，无法按 MAM 修复；请选择「以工具为准」或重新安装",
                    item.name
                ))
            } else {
                if let Some(dir) = crate::adapter::primary_skill_dir(&item.tool_id) {
                    let _ = crate::linker::remove_link(&dir.join(&item.name));
                }
                let _ = crate::linker::layer2::unlink_skill_from_layer2(&item.name, &item.tool_id);
                crate::database::upsert_assignment(&item.ext_id, &item.tool_id, false, "missing")
                    .map(|_| ())
            }
        }
        DriftType::McpKeyMissing => {
            if mam_first {
                crate::services::toggle_mcp(&item.name, &item.tool_id, true)
            } else {
                crate::database::upsert_assignment(&item.ext_id, &item.tool_id, false, "missing")
                    .map(|_| ())
            }
        }
        DriftType::UnmanagedMcpKey => {
            if mam_first {
                crate::services::mcp::remove_mcp(&item.tool_id, &item.name)
            } else {
                crate::commands::resource::import_mcp_to_ssot(item.name.clone())?;
                crate::database::upsert_assignment(&item.ext_id, &item.tool_id, true, "valid")
                    .map(|_| ())
            }
        }
        DriftType::UnmanagedSkillDir => {
            let tool_dir = crate::adapter::primary_skill_dir(&item.tool_id)
                .ok_or_else(|| format!("未知工具: {}", item.tool_id))?;
            if mam_first {
                // 复用既有"替换为链接"清理：会删除工具侧原生目录（UI 确认文案已警示）
                let repo_skill = crate::linker::ensure_repo_dir().join(&item.name);
                if !repo_skill.exists() {
                    return Err("SSOT 中无此 skill，无法替换为链接".to_string());
                }
                crate::linker::replace_with_symlink(&repo_skill, &tool_dir.join(&item.name))
            } else {
                let source = tool_dir.join(&item.name);
                crate::linker::install_to_repo(&source, &item.name, false)?;
                let ext = crate::database::ExtensionRecord {
                    id: item.ext_id.clone(),
                    kind: "skill".to_string(),
                    name: item.name.clone(),
                    description: None,
                    source_path: source.to_string_lossy().to_string(),
                    source_url: None,
                    version: None,
                    tags: None,
                    suite: None,
                    source_tool: Some(item.tool_id.clone()),
                    is_native: true,
                };
                let _ = crate::database::insert_extension(&ext);
                crate::services::enable_skill_for_tool(&item.name, &item.tool_id)
            }
        }
    }
}
```

注：`install_to_repo` 三参依赖 015 Task 10（未执行时去掉第三参）；`import_mcp_to_ssot` 是 commands 层纯函数，可直接调用。

- [ ] **Step 4: IPC + 注册**

`src-tauri/src/commands/drift.rs`：

```rust
// 漂移检测命令

#[tauri::command]
pub fn list_drift() -> Vec<crate::services::drift::DriftItem> {
    crate::services::drift::detect_drift()
}

#[tauri::command]
pub fn resolve_drift(
    item: crate::services::drift::DriftItem,
    mam_first: bool,
) -> Result<(), String> {
    crate::services::drift::resolve_drift(&item, mam_first)
}
```

`commands/mod.rs` 加 `pub mod drift;`；`services/mod.rs` 加 `pub mod drift;`；`lib.rs` 注册 `commands::drift::list_drift, commands::drift::resolve_drift,`。

- [ ] **Step 5: 前端横幅组件 + 挂载**

`src/components/resources/DriftBanner.tsx`：

```tsx
import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke } from "@tauri-apps/api/core";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { AlertTriangle } from "lucide-react";

interface DriftItem {
  extId: string;
  kind: string;
  name: string;
  toolId: string;
  driftType: string;
}

export function DriftBanner() {
  const { t } = useTranslation();
  const [items, setItems] = useState<DriftItem[]>([]);
  const [open, setOpen] = useState(false);

  const load = useCallback(() => {
    invoke<DriftItem[]>("list_drift").then(setItems).catch(console.error);
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  if (items.length === 0) return null;

  const resolve = async (item: DriftItem, mamFirst: boolean) => {
    try {
      await invoke("resolve_drift", { item, mamFirst });
      toast.success(t("drift.resolved"));
      load();
    } catch (e) {
      toast.error(t("common.operationFailed", { error: e }));
    }
  };

  return (
    <div className="bg-card mb-3 rounded-lg border border-amber-500/40 p-3">
      <button
        className="flex w-full items-center gap-2 text-left text-sm text-amber-500"
        onClick={() => setOpen(!open)}
      >
        <AlertTriangle className="h-4 w-4" />
        {t("drift.banner", { n: items.length })}
      </button>
      {open && (
        <div className="mt-2 space-y-1">
          {items.map((it) => (
            <div
              key={`${it.extId}-${it.toolId}-${it.driftType}`}
              className="flex items-center justify-between rounded border p-2 text-xs"
            >
              <span>
                <span className="font-medium">{it.name}</span>
                <span className="text-muted-foreground">
                  {" "}
                  · {it.toolId} · {t(`drift.type.${it.driftType}`)}
                </span>
              </span>
              <span className="flex gap-1">
                <Button
                  variant="outline"
                  size="sm"
                  className="h-6 px-2 text-[10px]"
                  onClick={() => resolve(it, true)}
                >
                  {t("drift.mamFirst")}
                </Button>
                <Button
                  variant="outline"
                  size="sm"
                  className="h-6 px-2 text-[10px]"
                  onClick={() => resolve(it, false)}
                >
                  {t("drift.toolFirst")}
                </Button>
              </span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
```

`src/components/resources/ExtensionList.tsx`：顶部 import `import { DriftBanner } from "./DriftBanner";`，并在其渲染容器的最上方（视图切换行之下、`ResourceByKindView` / `ResourceByToolView` 之前）插入 `<DriftBanner />`。

- [ ] **Step 6: i18n 键**

`zh.json` 顶层追加 `drift` 段：

```json
"drift": {
  "banner": "检测到 {{n}} 处配置漂移",
  "resolved": "已处理",
  "mamFirst": "以 MAM 为准",
  "toolFirst": "以工具为准",
  "type": {
    "linkMissing": "链接缺失",
    "linkDangling": "链接损坏",
    "repoMissing": "仓库缺失",
    "mcpKeyMissing": "MCP 配置被移除",
    "unmanagedMcpKey": "未纳管的 MCP",
    "unmanagedSkillDir": "未纳管的 skill 目录"
  }
}
```

`en.json` 对应：`"{{n}} configuration drifts detected"`, `"Resolved"`, `"MAM wins"`, `"Tool wins"`, `"Missing link"`, `"Broken link"`, `"Repo missing"`, `"MCP key removed"`, `"Unmanaged MCP"`, `"Unmanaged skill dir"`。

- [ ] **Step 7: 验证 + 提交**

Run: `cd src-tauri && cargo test && cd .. && pnpm check`
Expected: 通过

```bash
git add src-tauri/src/services/drift.rs src-tauri/src/services/mod.rs src-tauri/src/commands/drift.rs src-tauri/src/commands/mod.rs src-tauri/src/lib.rs src/components/resources/DriftBanner.tsx src/components/resources/ExtensionList.tsx src/i18n/locales/zh.json src/i18n/locales/en.json
git commit -m "feat(drift): configuration drift detection with per-item mam-first/tool-first resolution"
```

---

### Task 9: 收尾验证

- [ ] **Step 1: 后端全量**

Run: `cd src-tauri && cargo fmt && cargo test && cargo clippy`
Expected: 全部通过，无新增 clippy 告警

- [ ] **Step 2: 前端全量**

Run: `pnpm check`
Expected: format + lint + build + i18n 对齐通过

- [ ] **Step 3: 手工验证清单（pnpm tauri:dev）**

1. 启停一个 MCP → `~/.mam/backups/config/` 出现目标文件的 `.bak`；设置页「备份」可见并可恢复（恢复后文件字节回到操作前）（spec 故事 1 场景 1-3、故事 3）
2. 将某工具配置文件改为非 UTF-8 内容 → 启停 MCP 报错且文件不变（故事 2 场景 1）
3. 启动应用两次（第二次触发 24h 内跳过逻辑）→ `~/.mam/backups/db/` 有迁移前备份且数量 ≤10（故事 3 场景 1、3）
4. MCP 表单提交空 command → 红错阻断；提交不存在的 command → 警告确认（故事 4 场景 1-2）
5. 手删某工具配置中的 MCP 键 / 手装一个 skill 目录 → 资源页顶部出现漂移横幅，「以 MAM 为准」「以工具为准」双向处理正确（故事 5 场景 1-4）
6. 快速连点两个不同 MCP 开关到同一工具 → 两键共存无丢失（故事 6 场景 1）
7. hook 注册后确认不再产生 `settings.json.bak`（统一走 backups 目录）（故事 1 场景 4）

- [ ] **Step 4: 提交（如有格式化改动）**

```bash
git add -A
git commit -m "chore: formatting after 016 implementation"
```

---

## 自检记录

- **Spec 覆盖**：§1（写前备份/回滚）→Task 1/3/5；§2（严格读取）→Task 2；§3（DB 备份）→Task 4/5；§4（MCP 校验）→Task 7；§5（漂移检测）→Task 8；§6（per-tool 锁）→Task 6。范围外（GitHub ZIP/content_hash、云同步、DB 一键还原 UI）均未纳入，符合 spec「范围外」。
- **类型一致性**：`ConfigBackupEntry.original_path` 贯穿 list/read/delete/restore/前端；`DriftItem` 双 derive（Serialize+Deserialize）以支持 IPC 传回；`with_tool_config_lock(tool_id, closure)` 全计划签名一致；`read_config_exact` 返回 `Result<Option<String>>`。
- **已知偏差（收尾时记录到 spec 或 CHANGELOG）**：①「复制到指定位置」以「打开备份目录」替代；②漂移检测为打开资源页时按需计算（非启动常驻缓存），启动自动修复仍由 015 的 sync 承担；③JSONC 漂移检测用文本兜底（仅报告用途，不参与写路径）。
- **占位符**：无（初稿中 `mcp_keys` 的占位实现已收敛为最终三分支版本）。
