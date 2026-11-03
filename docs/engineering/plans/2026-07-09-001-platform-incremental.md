# 多 Agent 平台增量补全 实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 补全 Spec 001 中尚未实现且不被 Spec 002 重构覆盖的增量需求--fs2 文件锁防止 MCP/插件配置并发写入。

**架构：** Spec 001 的核心功能（会话监控、通知、终端跳转、三层映射、预设组、子 Agent 分配）均已实现。增量项中，notify 三重策略由 Spec 002 FR-5c 覆盖、WindowManager trait 由 Spec 002 FR-5b 覆盖、write-to-temp+rename 原子写入已在 `linker/mod.rs:118` 实现。唯一独立缺口是 FR-5.27 中 MCP/插件配置写入的 fs2 文件锁（`fs2` 已在 `Cargo.toml` 但代码未使用）。

**技术栈：** Rust + fs2 + toml_edit + serde_json

> **执行顺序：** 本计划必须在 Plan 2（架构重构）之前执行。Plan 2 会将 `manager/` 重命名为 `services/`，届时本计划中的 `src-tauri/src/manager/mcp.rs` 路径将变为 `src-tauri/src/services/mcp.rs`。如 Plan 2 已执行，请将文中所有 `manager/` 替换为 `services/`。

---

## 分析说明

Spec 001 修改项与实现状态对照：

| 修改项 | 状态 | 处理方式 |
|--------|------|---------|
| FR-1.5 notify 三重策略 | 未实现，notify crate 已在 Cargo.toml | 由 Spec 002 FR-5c 覆盖 |
| FR-2.10 资源映射看板 | 已实现（ResourceByKindView / ResourceByToolView） | 无需处理 |
| FR-4.18 WindowManager trait | 未实现 | 由 Spec 002 FR-5b 覆盖 |
| FR-5.24 per-tool 启用/禁用 | 未验证（"保存为预设"功能需确认） | 执行时验证 UI 是否支持 |
| FR-5.27 原子化更新 | 部分实现：`write_atomic` 已有，fs2 锁缺失 | **本计划处理** |
| FR-5.27 预设组批量原子性 | 已实现（manager/preset.rs 逐项操作，失败保留） | 无需处理 |

---

## 文件结构

| 文件 | 职责 |
|------|------|
| 修改：`src-tauri/src/manager/mcp.rs` | MCP 配置写入添加 fs2 文件锁 |
| 修改：`src-tauri/src/manager/plugin.rs` | 插件配置写入添加 fs2 文件锁 |
| 修改：`src-tauri/src/linker/mod.rs` | 新增 `write_config_locked` 辅助函数 |

---

### 任务 1：新增 fs2 文件锁辅助函数

**文件：**
- 修改：`src-tauri/src/linker/mod.rs`（在 `write_atomic` 函数后新增）

- [ ] **步骤 1：编写文件锁辅助函数**

在 `src-tauri/src/linker/mod.rs` 的 `write_atomic` 函数之后添加：

```rust
use fs2::FileExt;

/// 对配置文件加排他锁后执行写入操作，防止多个 MAM 实例并发写同一配置
pub fn write_config_locked(path: &Path, content: &str) -> Result<(), String> {
    // 确保父目录存在
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {}", e))?;
    }

    // 以读写模式打开（不存在则创建），加排他锁
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .open(path)
        .map_err(|e| format!("打开配置文件失败: {}", e))?;

    file.lock_exclusive()
        .map_err(|e| format!("获取文件锁失败: {}", e))?;

    // 写入临时文件 + rename，保证原子性
    let result = (|| {
        let temp = path.with_extension("tmp");
        fs::write(&temp, content).map_err(|e| format!("写入临时文件失败: {}", e))?;
        fs::rename(&temp, path).map_err(|e| format!("重命名失败: {}", e))?;
        Ok(())
    })();

    // 无论成功失败都释放锁
    let _ = file.unlock();
    result
}
```

- [ ] **步骤 2：验证编译**

运行：`cd src-tauri && cargo check`
预期：PASS，无编译错误

- [ ] **步骤 3：Commit**

```bash
git add src-tauri/src/linker/mod.rs
git commit -m "feat(linker): add fs2 file lock helper for config writes"
```

---

### 任务 2：MCP 配置写入使用文件锁

**文件：**
- 修改：`src-tauri/src/manager/mcp.rs`

- [ ] **步骤 1：定位 MCP 配置写入逻辑**

运行：`rg -n "write|fs::write|write_atomic" src-tauri/src/manager/mcp.rs`

- [ ] **步骤 2：替换为带锁写入**

将 mcp.rs 中所有写入工具配置文件（`~/.claude.json`、`~/.codex/config.toml`、`opencode.json`）的 `fs::write` 或 `write_atomic` 调用替换为 `linker::write_config_locked`。

典型替换模式：

```rust
// 之前：
fs::write(&config_path, content).map_err(|e| e.to_string())?;
// 或
linker::write_atomic(&config_path, &content)?;

// 之后：
linker::write_config_locked(&config_path, &content)?;
```

- [ ] **步骤 3：验证编译 + 功能**

运行：`cd src-tauri && cargo check`
预期：PASS

运行：`pnpm tauri:dev`，在资源管理页面为 Claude Code 和 Codex CLI 分别启用/禁用同一个 MCP 服务器，验证配置文件正确写入且无损坏。

- [ ] **步骤 4：Commit**

```bash
git add src-tauri/src/manager/mcp.rs
git commit -m "feat(mcp): use fs2 file lock for concurrent config writes"
```

---

### 任务 3：插件配置写入使用文件锁

**文件：**
- 修改：`src-tauri/src/manager/plugin.rs`

- [ ] **步骤 1：定位插件配置写入逻辑**

运行：`rg -n "write|fs::write|write_atomic" src-tauri/src/manager/plugin.rs`

- [ ] **步骤 2：替换为带锁写入**

将 plugin.rs 中写入工具配置文件的调用替换为 `linker::write_config_locked`，模式同任务 2。

- [ ] **步骤 3：验证编译 + 功能**

运行：`cd src-tauri && cargo check`
预期：PASS

运行：`pnpm tauri:dev`，为工具启用/禁用插件，验证配置文件正确写入。

- [ ] **步骤 4：Commit**

```bash
git add src-tauri/src/manager/plugin.rs
git commit -m "feat(plugin): use fs2 file lock for concurrent config writes"
```

---

## 自检

**规格覆盖度：** Spec 001 中 FR-5.27 的 fs2 文件锁部分由任务 1-3 覆盖。其余增量项（notify、WindowManager）由 Spec 002 覆盖，已实现项无需处理。无遗漏。

**占位符扫描：** 无占位符。所有步骤含具体代码和命令。

**类型一致性：** `write_config_locked` 在任务 1 定义，任务 2/3 引用，签名一致。
