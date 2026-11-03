# 资源管理功能修复 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实施 `specs/015-resource-management-fixes/spec.md`：P0 回收站修复、P1 五项功能缺陷（卸载重写、JSONC 保注释、去重修复、增量导入、断链检测）、P2 工程质量（死代码清理接线、扫描安装加固、批量操作与搜索）。

**Architecture:** 全部为对现有模块的修复与增强，不引入新架构。新增 3 个后端单元：`services/mcp/jsonc.rs`（JSONC 文本 span 编辑器）、DAO 删除函数（`*_on` 模式）、`LinkHealth` 链接健康模型。前端改动集中在 `ResourceByKindView.tsx` 与查询封装。

**Tech Stack:** Rust（tauri 2、rusqlite、toml_edit、trash crate、semver crate、junction）/ React 19 + TypeScript + React Query。

**环境**：Windows（Git Bash）。Rust 命令在 `src-tauri/` 下执行；测试用 `cargo test`（debug 构建，`MAM_HOME` 环境变量可重定向数据目录）。前端检查 `pnpm check`。**macOS/Linux 零回归**：所有 `#[cfg(windows)]` 代码必须带 `#[cfg(unix)]` 对应分支。

**执行顺序**：本计划 Task 1-3、5-11 为后端，Task 4、12-16 为前端与清理，Task 17 收尾。与 `016-config-robustness-hardening` 的关系：016 会把本计划中出现的 `unwrap_or_else(|_| "{}")` 宽松读取替换为严格读取（016 Task 3），两计划可先后独立执行。

---

### Task 1: 禁用原生 skill 改走系统回收站（P0，spec 用户故事 1）

**Files:**
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/src/commands/resource.rs:518-559`（`disable_skill_for_tool` 重写）
- Test: `src-tauri/src/commands/resource.rs`（文件末尾追加 tests 模块）

- [ ] **Step 1: 添加 trash 依赖**

`src-tauri/Cargo.toml` 的 `[dependencies]` 末尾（`toml_edit = "0.22"` 之后）追加：

```toml
trash = "5"
```

- [ ] **Step 2: 写失败测试（链接分支不进回收站）**

`src-tauri/src/commands/resource.rs` 文件末尾追加：

```rust
#[cfg(test)]
mod remove_target_tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn symlink_target_removed_directly() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let link = tmp.path().join("skill-link");
        junction::create(&src, &link).unwrap();
        assert!(link.is_symlink());
        let ty = remove_skill_target(&link).unwrap();
        assert_eq!(ty, "symlink");
        assert!(!link.exists() && !link.is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_target_removed_directly() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let link = tmp.path().join("skill-link");
        std::os::unix::fs::symlink(&src, &link).unwrap();
        let ty = remove_skill_target(&link).unwrap();
        assert_eq!(ty, "symlink");
        assert!(!link.exists() && !link.is_symlink());
    }
}
```

- [ ] **Step 3: 运行测试确认失败**

Run: `cd src-tauri && cargo test remove_target_tests`
Expected: 编译失败 `cannot find function remove_skill_target`

- [ ] **Step 4: 实现 helper 并重写命令**

在 `commands/resource.rs` 的 `disable_skill_for_tool` 上方新增 helper，并整体替换该命令（原实现位于 518-559 行）：

```rust
/// 移除工具目录中的 skill 目标：链接直接移除（无数据可丢），原生目录移入系统回收站。
/// 回收站失败返回错误，绝不静默降级为永久删除。
fn remove_skill_target(target: &std::path::Path) -> Result<String, String> {
    let target_type = if target.is_symlink() { "symlink" } else { "native" };
    if target_type == "symlink" {
        crate::linker::remove_link(target)?;
    } else {
        trash::delete(target).map_err(|e| format!("移入回收站失败: {}", e))?;
    }
    Ok(target_type.to_string())
}

/// 取消 skill 的工具配置：回收站/移除链接 + 更新 DB
#[tauri::command]
pub fn disable_skill_for_tool(tool_id: String, skill_name: String) -> Result<String, String> {
    let tool_skill_dir = crate::adapter::primary_skill_dir(&tool_id)
        .ok_or_else(|| format!("未知工具: {}", tool_id))?;
    let target = tool_skill_dir.join(&skill_name);
    if !target.exists() && !target.is_symlink() {
        return Err("目标路径不存在".to_string());
    }

    let target_type = remove_skill_target(&target)?;
    let _ = crate::linker::layer3::cleanup_layer3_on_tool_disable(&skill_name, &tool_id);
    let _ = crate::linker::layer2::unlink_skill_from_layer2(&skill_name, &tool_id);
    let ext_id = format!("skill-{}", skill_name);
    let _ = crate::database::upsert_assignment(&ext_id, &tool_id, false, "missing");
    Ok(target_type)
}
```

同时删除文件顶部对 `std::process::Command` 的隐式使用（原 535 行 `std::process::Command::new("trash")` 已随重写移除）。

- [ ] **Step 5: 运行测试确认通过**

Run: `cd src-tauri && cargo test remove_target_tests`
Expected: PASS（2 个平台各 1 个）

- [ ] **Step 6: 提交**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/commands/resource.rs
git commit -m "fix(skill): disable native skill via trash crate instead of permanent delete"
```

---

### Task 2: 新增删除 DAO（`*_on` 测试模式）

**Files:**
- Modify: `src-tauri/src/database/dao/extension.rs`（`mark_native_imported` 之后、文件末尾追加函数与 tests）
- Modify: `src-tauri/src/database/mod.rs:20-24`（导出）

- [ ] **Step 1: 写失败测试**

`src-tauri/src/database/dao/extension.rs` 文件末尾追加：

```rust
#[cfg(test)]
mod delete_tests {
    use super::*;
    use crate::database::schema;

    fn mem_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        schema::init(&conn);
        conn
    }

    #[test]
    fn delete_extension_removes_row() {
        let conn = mem_conn();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO extensions (id, kind, name, source_path, installed_at, updated_at) \
             VALUES ('skill-x','skill','x','/tmp/x',?1,?1)",
            [&now],
        )
        .unwrap();
        delete_extension_on(&conn, "skill-x").unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM extensions WHERE id='skill-x'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn delete_assignments_covers_subagent_rows() {
        let conn = mem_conn();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO extension_assignments \
             (id, extension_id, agent_tool_id, sub_agent_id, enabled, link_status, assigned_at) VALUES \
             ('a','skill-x','claude',NULL,1,'valid',?1),\
             ('b','skill-x','claude','sub1',1,'valid',?1)",
            [&now],
        )
        .unwrap();
        delete_assignments_for_on(&conn, "skill-x").unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM extension_assignments WHERE extension_id='skill-x'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test delete_tests`
Expected: 编译失败 `cannot find function delete_extension_on`

- [ ] **Step 3: 实现删除 DAO**

`src-tauri/src/database/dao/extension.rs` 的 `disable_subagent_assignment` 之后追加：

```rust
pub fn delete_extension(ext_id: &str) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    delete_extension_on(&conn, ext_id)
}

pub fn delete_extension_on(conn: &rusqlite::Connection, ext_id: &str) -> Result<(), String> {
    conn.execute("DELETE FROM extensions WHERE id = ?1", params![ext_id])
        .map_err(|e| e.to_string())
        .map(|_| ())
}

/// 删除某资源的全部 assignment（含子 Agent 维度）
pub fn delete_assignments_for(ext_id: &str) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    delete_assignments_for_on(&conn, ext_id)
}

pub fn delete_assignments_for_on(
    conn: &rusqlite::Connection,
    ext_id: &str,
) -> Result<(), String> {
    conn.execute(
        "DELETE FROM extension_assignments WHERE extension_id = ?1",
        params![ext_id],
    )
    .map_err(|e| e.to_string())
    .map(|_| ())
}
```

- [ ] **Step 4: 导出**

`src-tauri/src/database/mod.rs` 第 20-24 行的 `pub use dao::extension::{...}` 列表中追加 `delete_assignments_for, delete_extension,`（保持字母序）。

- [ ] **Step 5: 运行测试确认通过**

Run: `cd src-tauri && cargo test delete_tests`
Expected: PASS

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/database/dao/extension.rs src-tauri/src/database/mod.rs
git commit -m "feat(db): delete_extension and delete_assignments_for DAOs"
```

---

### Task 3: 重写 uninstall_resource（P1，spec 用户故事 2）

**Files:**
- Modify: `src-tauri/src/commands/manifest.rs:77-119`（整体替换 `uninstall_resource`）
- Modify: `src/lib/api/manifest.ts:20-22`
- Test: `src-tauri/src/commands/manifest.rs`（追加 tests）

- [ ] **Step 1: 写失败测试（路径解析）**

`src-tauri/src/commands/manifest.rs` 文件末尾追加：

```rust
#[cfg(test)]
mod uninstall_tests {
    use super::*;

    fn norm(p: &std::path::Path) -> String {
        p.to_string_lossy().replace('\\', "/")
    }

    #[test]
    fn resolves_dir_candidates_in_order() {
        let ps = resolve_ssot_paths("skill", "foo", Some("foo-1.0"));
        assert!(norm(&ps[0]).ends_with(".mam/skills/foo"));
        assert!(norm(&ps[1]).ends_with(".mam/skills/foo-1.0"));
    }

    #[test]
    fn resolves_mcp_json_file_only() {
        let ps = resolve_ssot_paths("mcp", "firecrawl", None);
        assert_eq!(ps.len(), 1);
        assert!(norm(&ps[0]).ends_with(".mam/mcp/firecrawl.json"));
    }

    #[test]
    fn unknown_kind_yields_no_candidates() {
        assert!(resolve_ssot_paths("widget", "x", None).is_empty());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test uninstall_tests`
Expected: 编译失败 `cannot find function resolve_ssot_paths`

- [ ] **Step 3: 实现路径解析与新卸载命令**

整体替换 `src-tauri/src/commands/manifest.rs:77-119` 的 `uninstall_resource`：

```rust
/// SSOT 路径候选：目录类资源 [<name>, <id>]（普通安装用 name，manifest 安装用 id），
/// MCP 为 <name>.json / <id>.json 文件。取第一个存在者删除。
fn resolve_ssot_paths(kind: &str, name: &str, record_id: Option<&str>) -> Vec<std::path::PathBuf> {
    let mam = dirs::home_dir().unwrap_or_default().join(".mam");
    let mut candidates = Vec::new();
    match kind {
        "skill" | "plugin" => {
            let dir = if kind == "skill" { "skills" } else { "plugins" };
            candidates.push(mam.join(dir).join(name));
            if let Some(id) = record_id {
                candidates.push(mam.join(dir).join(id));
            }
        }
        "mcp" => {
            candidates.push(mam.join("mcp").join(format!("{}.json", name)));
            if let Some(id) = record_id {
                candidates.push(mam.join("mcp").join(format!("{}.json", id)));
            }
        }
        _ => {}
    }
    candidates
}

/// 卸载资源：清理所有工具的分配与配置 → 删 SSOT → 删 DB 行 → 删 store 索引
#[tauri::command]
pub fn uninstall_resource(kind: String, name: String) -> Result<(), String> {
    if !["skill", "mcp", "plugin"].contains(&kind.as_str()) {
        return Err(format!("未知资源类型: {}", kind));
    }
    let ext_id = format!("{}-{}", kind, name);
    let record = crate::database::list_extensions()
        .into_iter()
        .find(|e| e.kind == kind && e.name == name);

    // 1) 按工具清理（一律用 name，assignment 键约定为 kind-name）
    let tools: Vec<String> = crate::database::list_all_assignments()
        .iter()
        .filter(|a| a.extension_id == ext_id)
        .map(|a| a.agent_tool_id.clone())
        .collect();
    for tool_id in tools {
        let result = match kind.as_str() {
            "skill" => crate::services::skill::disable_skill_for_tool(&name, &tool_id),
            "mcp" => crate::services::mcp::remove_mcp(&tool_id, &name),
            "plugin" => {
                let plugin_kind = record
                    .as_ref()
                    .and_then(|r| r.tags.clone())
                    .unwrap_or_else(|| "file".to_string());
                crate::services::plugin::toggle_plugin(&name, &tool_id, false, &plugin_kind)
            }
            _ => unreachable!(),
        };
        if let Err(e) = result {
            log::warn!("卸载清理 {} ({}) 失败: {}", name, tool_id, e);
        }
    }

    // 2) 删除 SSOT 文件/目录（取第一个存在的候选）
    for path in resolve_ssot_paths(&kind, &name, record.as_ref().map(|r| r.id.as_str())) {
        if path.is_file() {
            let _ = std::fs::remove_file(&path);
            break;
        }
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
            break;
        }
    }

    // 3) 删除 DB 行（约定 id 与 manifest 安装 id 两种）
    let _ = crate::database::delete_assignments_for(&ext_id);
    let _ = crate::database::delete_extension(&ext_id);
    if let Some(ref r) = record {
        if r.id != ext_id {
            let _ = crate::database::delete_assignments_for(&r.id);
            let _ = crate::database::delete_extension(&r.id);
        }
    }

    // 4) store 索引（manifest 安装才有；无条目时忽略）
    let store_id = record.as_ref().map(|r| r.id.clone()).unwrap_or(ext_id);
    if let Err(e) = crate::services::manifest::store::remove_entry(&store_id) {
        log::debug!("store 索引无 {} 条目，跳过: {}", name, e);
    }
    log::info!("资源已卸载: {} ({})", name, kind);
    Ok(())
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd src-tauri && cargo test uninstall_tests`
Expected: PASS（3 个）

- [ ] **Step 5: 更新前端 API 封装**

`src/lib/api/manifest.ts:20-22` 替换为：

```ts
export async function uninstallResource(kind: string, name: string): Promise<void> {
  return await invoke("uninstall_resource", { kind, name });
}
```

- [ ] **Step 6: 编译检查 + 提交**

Run: `cd src-tauri && cargo check && cd .. && pnpm build`
Expected: 均无错误

```bash
git add src-tauri/src/commands/manifest.rs src/lib/api/manifest.ts
git commit -m "fix(resource): rewrite uninstall_resource with correct name/id resolution and DB cleanup"
```

---

### Task 4: 卸载 UI 入口（P1，spec 用户故事 2 场景 5）

**Files:**
- Modify: `src/components/resources/ResourceByKindView.tsx`
- Modify: `src/i18n/locales/zh.json`、`src/i18n/locales/en.json`

- [ ] **Step 1: 添加 i18n 键**

`zh.json` 的 `resources` 对象内追加：

```json
"uninstall": "卸载",
"uninstallTitle": "卸载资源",
"uninstallDesc": "将从所有工具移除「{{name}}」并删除 MAM 仓库副本（影响 {{n}} 个工具）。此操作不可撤销。",
"uninstallSuccess": "已卸载 {{name}}"
```

`en.json` 对应追加：

```json
"uninstall": "Uninstall",
"uninstallTitle": "Uninstall resource",
"uninstallDesc": "This removes \"{{name}}\" from all tools and deletes the MAM repo copy ({{n}} tool(s) affected). This cannot be undone.",
"uninstallSuccess": "Uninstalled {{name}}"
```

- [ ] **Step 2: 添加状态与处理器**

`ResourceByKindView.tsx`：import 区追加 `uninstallResource`（来自 `@/lib/api/manifest`）与 `Trash2`（lucide-react，追加到现有图标 import）。组件 state 区（`mcpDialogOpen` 之后）追加：

```tsx
const [pendingUninstall, setPendingUninstall] = useState<{
  kind: string;
  name: string;
  count: number;
} | null>(null);
```

处理器区（`confirmDisable` 之后）追加：

```tsx
const confirmUninstall = async () => {
  if (!pendingUninstall) return;
  try {
    await uninstallResource(pendingUninstall.kind, pendingUninstall.name);
    toast.success(t("resources.uninstallSuccess", { name: pendingUninstall.name }));
    const fresh = await listSsotResources();
    setResources(fresh);
  } catch (e) {
    toast.error(t("common.operationFailed", { error: e }));
  } finally {
    setPendingUninstall(null);
  }
};
```

- [ ] **Step 3: 三处资源行加卸载按钮**

skill 行（`<span className="font-medium">...</span>` 与工具按钮组 `<div className="flex gap-1">` 之间）插入；mcp 行、plugin 行同样位置插入（kind 分别为 `"mcp"`、`"plugin"`）：

```tsx
<Button
  variant="ghost"
  size="sm"
  className="h-6 px-1.5 text-[10px] text-destructive"
  title={t("resources.uninstall")}
  onClick={() =>
    setPendingUninstall({
      kind: "skill", // mcp 行改为 "mcp"，plugin 行改为 "plugin"
      name: skill.name, // 对应 mcp.name / plugin.name
      count: skill.enabledTools.length,
    })
  }
>
  <Trash2 className="h-3 w-3" />
</Button>
```

注：行容器 `flex items-center justify-between` 中名称 span 与按钮组需包一层 `<div className="flex items-center gap-1">` 容纳新按钮。

- [ ] **Step 4: 卸载确认弹窗**

文件末尾（MCP 弹窗 Dialog 之后）追加：

```tsx
<Dialog open={!!pendingUninstall} onOpenChange={(o) => !o && setPendingUninstall(null)}>
  <DialogContent className="max-w-sm">
    <DialogHeader>
      <DialogTitle className="text-red-600">{t("resources.uninstallTitle")}</DialogTitle>
      <DialogDescription className="pt-2 text-sm">
        {t("resources.uninstallDesc", {
          name: pendingUninstall?.name,
          n: pendingUninstall?.count ?? 0,
        })}
      </DialogDescription>
    </DialogHeader>
    <DialogFooter className="gap-2">
      <Button variant="outline" size="sm" onClick={() => setPendingUninstall(null)}>
        {t("common.cancel")}
      </Button>
      <Button variant="destructive" size="sm" onClick={confirmUninstall}>
        {t("resources.uninstall")}
      </Button>
    </DialogFooter>
  </DialogContent>
</Dialog>
```

- [ ] **Step 5: 验证 + 提交**

Run: `pnpm check`
Expected: 通过（含 i18n 键对齐）

```bash
git add src/components/resources/ResourceByKindView.tsx src/i18n/locales/zh.json src/i18n/locales/en.json
git commit -m "feat(ui): uninstall entry on resource rows with confirmation dialog"
```

---

### Task 5: JSONC 无损编辑器 + 接线（P1，spec 用户故事 3）

**Files:**
- Create: `src-tauri/src/services/mcp/jsonc.rs`
- Modify: `src-tauri/src/services/mcp/mod.rs`（声明模块 + 改造 4 个 JSONC 函数）
- Modify: `src-tauri/src/services/plugin/mod.rs:139-150, 224-234`（Jsonc 分支拆分）

- [ ] **Step 1: 写失败测试**

新建 `src-tauri/src/services/mcp/jsonc.rs`，先只写测试与空实现框架：

```rust
// JSONC 无损编辑：基于文本 span 定位键值，保留注释与既有格式
// 仅支持「顶层对象 → 对象型 section → 任意值 entry」两级结构（覆盖 mcp / plugins 场景）

#[derive(Debug, Clone, Copy, PartialEq)]
struct Span {
    start: usize, // 字节下标（含）
    end: usize,   // 字节下标（不含）
}

/// 在顶层对象的 section 中插入/覆盖 entry（值为紧凑 JSON 字符串）
pub fn upsert_entry(content: &str, section: &str, key: &str, value_json: &str) -> Result<String, String>;
/// 从顶层对象的 section 中删除 entry；section/entry 不存在视为成功（幂等）
pub fn remove_entry(content: &str, section: &str, key: &str) -> Result<String, String>;

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
  // top comment
  "$schema": "https://x.dev/schema.json",
  "mcp": {
    /* block comment */
    "old": { "type": "local", "command": ["a"] }
  },
  "plugins": {
    "p1": true
  }
}
"#;

    #[test]
    fn upsert_new_entry_keeps_comments() {
        let out = upsert_entry(SAMPLE, "mcp", "new", r#"{"type":"local","command":["b"]}"#).unwrap();
        assert!(out.contains("// top comment"));
        assert!(out.contains("/* block comment */"));
        assert!(out.contains("\"new\": {\"type\":\"local\",\"command\":[\"b\"]}"));
        assert!(out.contains("\"old\""));
        // 结果仍是合法 JSON（测试样本无注释干扰新段）
        let json_only = out.replace("// top comment", "").replace("/* block comment */", "");
        let v: serde_json::Value = serde_json::from_str(&json_only).unwrap();
        assert_eq!(v["mcp"]["new"]["command"][0], "b");
    }

    #[test]
    fn upsert_replaces_existing_value_only() {
        let out = upsert_entry(SAMPLE, "mcp", "old", r#"{"x":1}"#).unwrap();
        assert!(out.contains("\"old\": {\"x\":1}"));
        assert!(!out.contains("\"command\": [\"a\"]"));
        assert!(out.contains("// top comment"));
    }

    #[test]
    fn remove_entry_deletes_key_and_comma() {
        let out = remove_entry(SAMPLE, "mcp", "old").unwrap();
        assert!(!out.contains("\"old\""));
        assert!(out.contains("\"mcp\""));
        assert!(out.contains("// top comment"));
    }

    #[test]
    fn remove_missing_entry_is_noop() {
        let out = remove_entry(SAMPLE, "mcp", "ghost").unwrap();
        assert_eq!(out, SAMPLE);
    }

    #[test]
    fn section_missing_creates_section_on_upsert() {
        let out = upsert_entry("{\n  \"a\": 1\n}\n", "mcp", "n", "{}").unwrap();
        assert!(out.contains("\"mcp\": {\"n\": {}}"));
        assert!(out.contains("\"a\": 1"));
    }

    #[test]
    fn non_object_root_is_rejected() {
        assert!(upsert_entry("[1,2]", "mcp", "n", "{}").is_err());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test jsonc`
Expected: 编译失败（函数无实现体）

- [ ] **Step 3: 实现编辑器**

将 `jsonc.rs` 中的两个 pub 函数签名替换为完整实现（Span 保留，追加以下内容）：

```rust
struct Cursor<'a> {
    s: &'a str,
    b: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(s: &'a str) -> Self {
        Cursor { s, b: s.as_bytes(), pos: 0 }
    }

    /// 跳过空白与注释
    fn skip_trivia(&mut self) {
        loop {
            while self.pos < self.b.len()
                && matches!(self.b[self.pos], b' ' | b'\t' | b'\r' | b'\n')
            {
                self.pos += 1;
            }
            if self.pos + 1 < self.b.len() && self.b[self.pos] == b'/' {
                if self.b[self.pos + 1] == b'/' {
                    while self.pos < self.b.len() && self.b[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                    continue;
                }
                if self.b[self.pos + 1] == b'*' {
                    self.pos += 2;
                    while self.pos + 1 < self.b.len()
                        && !(self.b[self.pos] == b'*' && self.b[self.pos + 1] == b'/')
                    {
                        self.pos += 1;
                    }
                    self.pos = (self.pos + 2).min(self.b.len());
                    continue;
                }
            }
            break;
        }
    }

    /// 读取字符串字面量（pos 位于 '"'），返回内容并推进到引号后
    fn read_string(&mut self) -> Option<String> {
        if self.pos >= self.b.len() || self.b[self.pos] != b'"' {
            return None;
        }
        self.pos += 1;
        let mut out = String::new();
        while self.pos < self.b.len() {
            match self.b[self.pos] {
                b'\\' => {
                    self.pos += 1;
                    if self.pos < self.b.len() {
                        out.push(self.b[self.pos] as char);
                        self.pos += 1;
                    }
                }
                b'"' => {
                    self.pos += 1;
                    return Some(out);
                }
                c if c < 0x80 => {
                    out.push(c as char);
                    self.pos += 1;
                }
                _ => {
                    let ch = self.s[self.pos..].chars().next()?;
                    out.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
        None
    }

    /// 跳过一个值，返回其 span（已去除前导 trivia 与尾随空白）
    fn skip_value(&mut self) -> Option<Span> {
        self.skip_trivia();
        let value_start = self.pos;
        if self.pos >= self.b.len() {
            return None;
        }
        match self.b[self.pos] {
            b'"' => {
                self.read_string()?;
            }
            b'{' | b'[' => {
                let open = self.b[self.pos];
                let close = if open == b'{' { b'}' } else { b']' };
                let mut depth = 0usize;
                loop {
                    self.skip_trivia();
                    if self.pos >= self.b.len() {
                        return None;
                    }
                    let c = self.b[self.pos];
                    if c == b'"' {
                        self.read_string()?;
                        continue;
                    }
                    if c == open {
                        depth += 1;
                        self.pos += 1;
                        continue;
                    }
                    if c == close {
                        depth -= 1;
                        self.pos += 1;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        self.pos += 1;
                    }
                }
            }
            _ => {
                while self.pos < self.b.len() {
                    let c = self.b[self.pos];
                    if c == b',' || c == b'}' || c == b']' {
                        break;
                    }
                    if c == b'/'
                        && self.pos + 1 < self.b.len()
                        && (self.b[self.pos + 1] == b'/' || self.b[self.pos + 1] == b'*')
                    {
                        break;
                    }
                    self.pos += 1;
                }
                while self.pos > value_start
                    && matches!(self.b[self.pos - 1], b' ' | b'\t' | b'\r' | b'\n')
                {
                    self.pos -= 1;
                }
            }
        }
        Some(Span { start: value_start, end: self.pos })
    }
}

struct Entry {
    key: String,
    key_start: usize,
    value: Span,
}

/// 遍历对象（cur.pos 位于 '{'），返回 (对象闭合 span, entries)
fn walk_object(cur: &mut Cursor) -> Option<(Span, Vec<Entry>)> {
    let obj_start = cur.pos;
    cur.pos += 1; // 跳过 '{'
    let mut entries = Vec::new();
    loop {
        cur.skip_trivia();
        if cur.pos >= cur.b.len() {
            return None;
        }
        if cur.b[cur.pos] == b'}' {
            cur.pos += 1;
            break;
        }
        let key_start = cur.pos;
        let key = cur.read_string()?;
        cur.skip_trivia();
        if cur.pos >= cur.b.len() || cur.b[cur.pos] != b':' {
            return None;
        }
        cur.pos += 1;
        let value = cur.skip_value()?;
        entries.push(Entry { key, key_start, value });
        cur.skip_trivia();
        if cur.pos < cur.b.len() && cur.b[cur.pos] == b',' {
            cur.pos += 1;
        }
    }
    Some((Span { start: obj_start, end: cur.pos }, entries))
}

fn parse_root(content: &str) -> Result<(Span, Vec<Entry>), String> {
    let mut cur = Cursor::new(content);
    cur.skip_trivia();
    if cur.pos >= cur.b.len() || cur.b[cur.pos] != b'{' {
        return Err("无法安全编辑该 JSONC 文件：根节点不是对象".to_string());
    }
    walk_object(&mut cur).ok_or_else(|| "无法安全编辑该 JSONC 文件：对象结构不完整".to_string())
}

pub fn upsert_entry(content: &str, section: &str, key: &str, value_json: &str) -> Result<String, String> {
    let (root, root_entries) = parse_root(content)?;
    let section_entry = root_entries.iter().find(|e| e.key == section);
    let mut out = String::new();
    match section_entry {
        Some(se) => {
            let mut cur = Cursor::new(content);
            cur.pos = se.value.start;
            if cur.b[cur.pos] != b'{' {
                return Err(format!("无法安全编辑：section \"{}\" 不是对象", section));
            }
            let (_, entries) = walk_object(&mut cur)
                .ok_or_else(|| format!("无法安全编辑：section \"{}\" 结构不完整", section))?;
            if let Some(existing) = entries.iter().find(|e| e.key == key) {
                // 覆盖：只替换 value span
                out.push_str(&content[..existing.value.start]);
                out.push_str(value_json);
                out.push_str(&content[existing.value.end..]);
                return Ok(out);
            }
            // 插入到 section 收尾 '}' 之前
            let close = se.value.end - 1;
            out.push_str(&content[..close]);
            if !entries.is_empty() {
                out.push(',');
            }
            out.push_str(&format!(" \"{}\": {} ", key, value_json));
            out.push_str(&content[close..]);
        }
        None => {
            // section 不存在：插到 root 收尾 '}' 之前
            let close = root.end - 1;
            out.push_str(&content[..close]);
            if !root_entries.is_empty() {
                out.push(',');
            }
            out.push_str(&format!(" \"{}\": {{ \"{}\": {} }} ", section, key, value_json));
            out.push_str(&content[close..]);
        }
    }
    Ok(out)
}

pub fn remove_entry(content: &str, section: &str, key: &str) -> Result<String, String> {
    let (_, root_entries) = parse_root(content)?;
    let Some(se) = root_entries.iter().find(|e| e.key == section) else {
        return Ok(content.to_string());
    };
    let mut cur = Cursor::new(content);
    cur.pos = se.value.start;
    let Some((_, entries)) = walk_object(&mut cur) else {
        return Err(format!("无法安全编辑：section \"{}\" 结构不完整", section));
    };
    let Some(target) = entries.iter().find(|e| e.key == key) else {
        return Ok(content.to_string());
    };

    // 删除范围：entry key 前（含前导逗号）到 value 结束（含后导逗号）
    let mut remove_start = target.key_start;
    let mut remove_end = target.value.end;
    let has_prev = entries.iter().any(|e| e.value.end <= target.key_start);
    let has_next = entries.iter().any(|e| e.key_start >= target.value.end);
    // 向左吃掉逗号 + 紧邻空白
    let mut i = remove_start;
    while i > se.value.start {
        let c = content.as_bytes()[i - 1];
        if c == b' ' || c == b'\t' || c == b'\r' || c == b'\n' {
            i -= 1;
        } else {
            break;
        }
    }
    if content.as_bytes()[i - 1] == b',' {
        remove_start = i - 1;
    } else if has_next {
        // 首个 entry 且后面还有：向右吃掉逗号
        let mut j = remove_end;
        let bytes = content.as_bytes();
        while j < se.value.end && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\r' || bytes[j] == b'\n') {
            j += 1;
        }
        if j < se.value.end && bytes[j] == b',' {
            remove_end = j + 1;
        }
    }
    let _ = has_prev;
    let mut out = String::new();
    out.push_str(&content[..remove_start]);
    out.push_str(&content[remove_end..]);
    Ok(out)
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd src-tauri && cargo test jsonc`
Expected: PASS（6 个）。若 `remove_entry_deletes_key_and_comma` 断言失败，检查删除范围是否残留连续逗号，修正逗号吸收方向。

- [ ] **Step 5: 声明模块并接线 MCP**

`src-tauri/src/services/mcp/mod.rs` 顶部（`use` 之前）加 `pub mod jsonc;`，替换 `write_mcp_jsonc` / `remove_mcp_jsonc`（134-162 行）：

```rust
fn write_mcp_jsonc(path: &std::path::Path, name: &str, config: &McpConfig) -> Result<(), String> {
    let content = std::fs::read_to_string(path).unwrap_or_else(|_| "{}".to_string());
    // OpenCode 格式：command 是数组，env 是 environment
    let mut cmd_array = vec![config.command.clone()];
    cmd_array.extend(config.args.iter().cloned());
    let value = serde_json::json!({
        "type": "local",
        "command": cmd_array,
        "environment": config.env,
    });
    let next = jsonc::upsert_entry(&content, "mcp", name, &value.to_string())?;
    crate::linker::write_config_locked(path, &next)
}

fn remove_mcp_jsonc(path: &std::path::Path, name: &str) -> Result<(), String> {
    let content = std::fs::read_to_string(path).unwrap_or_else(|_| "{}".to_string());
    let next = jsonc::remove_entry(&content, "mcp", name)?;
    crate::linker::write_config_locked(path, &next)
}
```

- [ ] **Step 6: 接线 Plugin 的 Jsonc 分支**

`src-tauri/src/services/plugin/mod.rs`：

`enable_config_plugin`（139-150 行）的 `Json | Jsonc` 合并分支拆开：

```rust
match adapter.mcp_format() {
    crate::adapter::McpFormat::Json => {
        let mut root: serde_json::Value =
            serde_json::from_str(&content).map_err(|e| format!("解析 JSON 配置失败: {}", e))?;
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
            .and_then(|v| v.to_string().replace(char::is_whitespace, " "))
            .map_err(|e| e.to_string())?;
        let next = crate::services::mcp::jsonc::upsert_entry(&content, "plugins", plugin_name, &value)?;
        linker::write_config_locked(config_path, &next)?;
    }
    crate::adapter::McpFormat::Toml => { /* 原 TOML 分支保持不变 */ }
}
```

注：`to_string()` 结果本身无多余空白，`.replace` 仅防御；可简化为 `serde_json::to_value(entries).map_err(...)?.to_string()`。

`disable_config_plugin`（224-234 行）同样拆分：Json 分支保持 serde_json 逻辑，新增：

```rust
crate::adapter::McpFormat::Jsonc => {
    let next = crate::services::mcp::jsonc::remove_entry(&content, "plugins", plugin_name)?;
    linker::write_config_locked(config_path, &next)?;
}
```

（原 `Json | Jsonc` 合并匹配改为 `Json` 单独匹配 + 上述 Jsonc 分支；TOML 分支不变。）

- [ ] **Step 7: 全量测试 + 提交**

Run: `cd src-tauri && cargo test && cargo clippy`
Expected: 全部 PASS、无新 clippy 告警

```bash
git add src-tauri/src/services/mcp/jsonc.rs src-tauri/src/services/mcp/mod.rs src-tauri/src/services/plugin/mod.rs
git commit -m "feat(mcp): lossless JSONC span editing preserves comments in opencode.json"
```

---

### Task 6: 扫描去重按 kind 分离 + 同名补链（P1，spec 用户故事 4）

**Files:**
- Modify: `src-tauri/src/services/resource/mod.rs:181-366`（`auto_import_extensions`）
- Test: `src-tauri/src/services/resource/mod.rs`（追加 tests）

- [ ] **Step 1: 写失败测试（纯函数）**

`services/resource/mod.rs` 文件末尾追加：

```rust
#[cfg(test)]
mod import_plan_tests {
    use super::*;

    #[test]
    fn new_name_imports_and_links() {
        let seen = std::collections::HashSet::new();
        assert_eq!(plan_skill_import("foo", &seen, None), SkillImportPlan::ImportAndLink);
    }

    #[test]
    fn known_name_links_second_tool() {
        let seen: std::collections::HashSet<String> = ["foo".to_string()].into_iter().collect();
        assert_eq!(plan_skill_import("foo", &seen, None), SkillImportPlan::LinkOnly);
        assert_eq!(plan_skill_import("foo", &seen, Some(true)), SkillImportPlan::LinkOnly);
    }

    #[test]
    fn known_name_respects_explicit_disable() {
        let seen: std::collections::HashSet<String> = ["foo".to_string()].into_iter().collect();
        assert_eq!(plan_skill_import("foo", &seen, Some(false)), SkillImportPlan::Skip);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test import_plan_tests`
Expected: 编译失败 `cannot find type SkillImportPlan`

- [ ] **Step 3: 实现纯函数并改造循环**

`services/resource/mod.rs` 的 `ImportStats` 定义之后追加：

```rust
#[derive(Debug, PartialEq)]
enum SkillImportPlan {
    /// SSOT 无此 name：复制入库 + 建链
    ImportAndLink,
    /// SSOT 已有：跳过复制，仅为当前工具补链（未被显式禁用时）
    LinkOnly,
    /// 已有且该工具被显式禁用：不动
    Skip,
}

fn plan_skill_import(
    name: &str,
    seen: &std::collections::HashSet<String>,
    tool_enabled: Option<bool>,
) -> SkillImportPlan {
    if !seen.contains(name) {
        SkillImportPlan::ImportAndLink
    } else if tool_enabled == Some(false) {
        SkillImportPlan::Skip
    } else {
        SkillImportPlan::LinkOnly
    }
}
```

`auto_import_extensions` skill 循环（227-262 行）替换为：

```rust
for (skill_path, skill_name) in &found {
    let tool_enabled = crate::database::list_assignments(tool_id)
        .iter()
        .find(|a| a.extension_id == format!("skill-{}", skill_name))
        .map(|a| a.enabled);
    match plan_skill_import(skill_name, &seen_names, tool_enabled) {
        SkillImportPlan::ImportAndLink => {
            seen_names.insert(skill_name.clone());
            let meta = parse_skill_meta(&skill_path.join("SKILL.md"));
            let description = meta.as_ref().and_then(|m| m.description.clone());
            let suite = detect_suite(skill_name, skill_path, skills_dir);
            if let Err(e) = linker::install_to_repo(skill_path, skill_name) {
                log::warn!("导入 skill {} 失败: {}", skill_name, e);
                continue;
            }
            let ext = crate::database::ExtensionRecord {
                id: format!("skill-{}", skill_name),
                kind: "skill".to_string(),
                name: skill_name.clone(),
                description,
                source_path: skill_path.to_string_lossy().to_string(),
                source_url: None,
                version: None,
                tags: Some(tool_id.to_string()),
                suite,
                source_tool: Some(tool_id.to_string()),
                is_native: false,
            };
            let _ = crate::database::insert_extension(&ext);
            if let Err(e) = crate::services::enable_skill_for_tool(skill_name, tool_id) {
                log::warn!("导入 {} 后为 {} 创建链接失败: {}", skill_name, tool_id, e);
            }
            imported += 1;
        }
        SkillImportPlan::LinkOnly => {
            if let Err(e) = crate::services::enable_skill_for_tool(skill_name, tool_id) {
                log::warn!("为 {} 补建 {} 链接失败: {}", skill_name, tool_id, e);
            }
        }
        SkillImportPlan::Skip => {
            skipped_dup += 1;
        }
    }
}
```

（此处保持双参调用；Task 10 给 `install_to_repo` 加 `overwrite` 第三参时会统一更新本调用点为 `install_to_repo(skill_path, skill_name, force)`。）

plugin 循环（306-309 行）的去重改为独立集合：`seen_names.contains(&name)` / `seen_names.insert(...)` 替换为 `plugin_seen`：

```rust
let mut plugin_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
```

（声明放在 plugin 扫描 `for (tool_id, plugins_dir) in &plugin_sources` 之前；循环内 `seen_names` 两处对应替换为 `plugin_seen`。）

- [ ] **Step 4: 运行测试确认通过**

Run: `cd src-tauri && cargo test import_plan_tests`
Expected: PASS（3 个）

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/services/resource/mod.rs
git commit -m "fix(resource): per-kind dedup sets and cross-tool same-name skill linking"
```

---

### Task 7: 启动增量导入（P1，spec 用户故事 5）

**Files:**
- Modify: `src-tauri/src/services/resource/mod.rs:181-199`（移除整体跳过 + seen 播种）
- Modify: `src-tauri/src/lib.rs:31-32`（后台线程）

- [ ] **Step 1: 移除「DB 非空即整体返回」**

`auto_import_extensions` 开头（181-199 行）替换为：

```rust
pub fn auto_import_extensions(force: bool) -> ImportStats {
    let _repo = linker::ensure_repo_dir();
    let existing_before: std::collections::HashSet<String> = crate::database::list_extensions()
        .iter()
        .map(|e| e.name.clone())
        .collect();

    // 增量模式（force=false 且 DB 已有数据）：已存在的 name 只补链不重导（Task 6 的 LinkOnly）；
    // force=true 全量重扫保持覆盖导入语义
    let mut seen_names: std::collections::HashSet<String> = if force {
        std::collections::HashSet::new()
    } else {
        existing_before.iter().cloned().collect()
    };
    let mut imported: usize = 0;
    let mut skipped_dup: usize = 0;
    let mut source_counts: Vec<(String, usize)> = Vec::new();
```

（原 `if !force && !existing_before.is_empty() { ... return ... }` 块整体删除；原 209 行 `let mut seen_names ... = HashSet::new()` 声明删除，已上移。）

- [ ] **Step 2: 启动调用移入后台线程**

`src-tauri/src/lib.rs:30-33` 替换为：

```rust
    database::init();
    // 后台增量导入（仅导入 DB 中不存在的新 skill）+ 补链，不阻塞启动
    std::thread::spawn(|| {
        services::auto_import_extensions(false);
        services::sync_imported_skill_links();
    });
    monitor::hooks::register_all_hooks();
```

- [ ] **Step 3: 验证 + 提交**

Run: `cd src-tauri && cargo test && cargo check`
Expected: 编译通过、既有测试无回归

```bash
git add src-tauri/src/services/resource/mod.rs src-tauri/src/lib.rs
git commit -m "feat(resource): incremental startup import on background thread"
```

---

### Task 8: 链接健康检测 + 自动修复（P1，spec 用户故事 6）

**Files:**
- Modify: `src-tauri/src/linker/mod.rs`（新增 `LinkHealth`，文件 `pub mod` 声明之后、`ensure_repo_dir` 之前）
- Modify: `src-tauri/src/services/resource/mod.rs`（`sync_imported_skill_links` 第一循环扩展）
- Modify: `src-tauri/src/commands/resource.rs`（`SsotResource` 增字段 + `scan_skills` 计算损坏工具）
- Modify: `src/types/extension.ts`、`src/components/resources/ResourceByKindView.tsx`、i18n 两文件

- [ ] **Step 1: 写失败测试**

`src-tauri/src/linker/mod.rs` 文件末尾追加：

```rust
#[cfg(test)]
mod link_health_tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn detects_dangling_junction() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let link = tmp.path().join("lnk");
        junction::create(&src, &link).unwrap();
        assert_eq!(check_link_health(&link), LinkHealth::Valid);
        std::fs::remove_dir_all(&src).unwrap();
        assert_eq!(check_link_health(&link), LinkHealth::Dangling);
    }

    #[cfg(unix)]
    #[test]
    fn detects_dangling_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let link = tmp.path().join("lnk");
        std::os::unix::fs::symlink(&src, &link).unwrap();
        assert_eq!(check_link_health(&link), LinkHealth::Valid);
        std::fs::remove_dir_all(&src).unwrap();
        assert_eq!(check_link_health(&link), LinkHealth::Dangling);
    }

    #[test]
    fn missing_and_native_paths() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(check_link_health(&tmp.path().join("none")), LinkHealth::Missing);
        let native = tmp.path().join("real");
        std::fs::create_dir_all(&native).unwrap();
        assert_eq!(check_link_health(&native), LinkHealth::NotLink);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test link_health_tests`
Expected: 编译失败 `cannot find type LinkHealth`

- [ ] **Step 3: 实现 LinkHealth**

`src-tauri/src/linker/mod.rs` 顶部（模块声明后）追加：

```rust
/// 链接健康状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkHealth {
    /// 链接存在且目标可达
    Valid,
    /// 链接存在但目标不可达（SSOT 被删/移动）
    Dangling,
    /// 路径存在但不是链接（原生目录/文件）
    NotLink,
    /// 路径不存在
    Missing,
}

pub fn check_link_health(target: &Path) -> LinkHealth {
    if !target.exists() && !target.is_symlink() {
        return LinkHealth::Missing;
    }
    if !target.is_symlink() {
        return LinkHealth::NotLink;
    }
    if fs::metadata(target).is_ok() {
        LinkHealth::Valid
    } else {
        LinkHealth::Dangling
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd src-tauri && cargo test link_health_tests`
Expected: PASS

- [ ] **Step 5: 启动同步修复断链**

`services/resource/mod.rs` 的 `sync_imported_skill_links` 第一循环中，`let already_linked = ...`（132-134 行）之前插入：

```rust
        // 断链检测与自动修复：SSOT 仍在则重建，SSOT 缺失则清链接并标记
        let tool_target = crate::adapter::primary_skill_dir(&tool_id).map(|d| d.join(&ext.name));
        if let Some(t) = &tool_target {
            if crate::linker::check_link_health(t) == crate::linker::LinkHealth::Dangling {
                let repo_exists = crate::linker::ensure_repo_dir().join(&ext.name).exists();
                let _ = crate::linker::remove_link(t);
                if repo_exists {
                    if let Err(e) = crate::services::enable_skill_for_tool(&ext.name, &tool_id) {
                        log::warn!("重建 {} → {} 断链失败: {}", ext.name, tool_id, e);
                        let _ = crate::database::upsert_assignment(&ext.id, &tool_id, true, "dangling");
                    }
                } else {
                    let _ = crate::linker::layer2::unlink_skill_from_layer2(&ext.name, &tool_id);
                    let _ = crate::database::upsert_assignment(&ext.id, &tool_id, false, "missing");
                }
                continue;
            }
        }
```

- [ ] **Step 6: 后端暴露 brokenTools**

`src-tauri/src/commands/resource.rs` 的 `SsotResource` 结构体（232-238 行）追加字段：

```rust
pub struct SsotResource {
    pub name: String,
    pub kind: String,
    pub enabled_tools: Vec<String>,
    #[serde(rename = "brokenTools")]
    pub broken_tools: Vec<String>,
}
```

`list_ssot_resources` 中：`scan_skills` 闭包的 `SsotResource { ... }` 构造（293-297 行）追加：

```rust
                let broken_tools: Vec<String> = assignments
                    .iter()
                    .filter(|a| {
                        a.extension_id == ext_id && a.enabled && a.link_status == "dangling"
                    })
                    .map(|a| a.agent_tool_id.clone())
                    .collect();
```

（并在构造体中加 `broken_tools`。）`scan_mcp` 的构造（391-395 行）与 `scan_simple` 的构造（415-419 行）各追加 `broken_tools: vec![]`。

- [ ] **Step 7: 前端损坏徽标**

`src/types/extension.ts` 的 `SsotResource`（71-75 行）追加 `brokenTools?: string[];`。

`ResourceByKindView.tsx` skill 行名称 span 之后追加：

```tsx
{skill.brokenTools && skill.brokenTools.length > 0 && (
  <span
    className="rounded bg-amber-500/15 px-1.5 py-0.5 text-[10px] text-amber-500"
    title={t("resources.linkBrokenTooltip", { tools: skill.brokenTools.join(", ") })}
  >
    {t("resources.linkBroken")}
  </span>
)}
```

i18n：`zh.json` resources 追加 `"linkBroken": "链接损坏"`, `"linkBrokenTooltip": "以下工具的链接已断开：{{tools}}，重新扫描可自动修复"`；`en.json` 追加 `"linkBroken": "Broken link"`, `"linkBrokenTooltip": "Links are broken for: {{tools}}. Rescan will auto-repair."`。

- [ ] **Step 8: 验证 + 提交**

Run: `cd src-tauri && cargo test && cargo clippy && cd .. && pnpm check`
Expected: 全部通过

```bash
git add src-tauri/src/linker/mod.rs src-tauri/src/services/resource/mod.rs src-tauri/src/commands/resource.rs src/types/extension.ts src/components/resources/ResourceByKindView.tsx src/i18n/locales/zh.json src/i18n/locales/en.json
git commit -m "feat(linker): dangling link detection with startup auto-repair and broken badge"
```

---

### Task 9: 扫描深度限制 + 不跟随 symlink（P2，spec 用户故事 8 场景 1）

**Files:**
- Modify: `src-tauri/src/services/resource/mod.rs:62-84`（`scan_skills_recursive`）
- Modify: `src-tauri/src/commands/resource.rs:5-30`（`scan_skill_dirs`）
- Test: `src-tauri/src/services/resource/mod.rs`

- [ ] **Step 1: 写失败测试**

`services/resource/mod.rs` tests 区追加：

```rust
#[cfg(test)]
mod scan_depth_tests {
    use super::*;

    #[test]
    fn deep_nesting_stops_at_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let mut dir = tmp.path().to_path_buf();
        for i in 0..7 {
            dir = dir.join(format!("d{}", i));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), "---\nname: too-deep\n---\n").unwrap();
        }
        let found = scan_skills_recursive(tmp.path(), tmp.path(), 0);
        assert!(found.iter().all(|(p, _)| p.components().count() <= tmp.path().components().count() + 5));
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test scan_depth_tests`
Expected: 编译失败（函数是双参）

- [ ] **Step 3: 实现深度限制**

`services/resource/mod.rs` 的 `scan_skills_recursive`（62-84 行）替换为：

```rust
/// 递归扫描目录下的所有 SKILL.md 文件；深度上限 4 层，symlink 目录不跟随（防循环）
const SCAN_MAX_DEPTH: usize = 4;

fn scan_skills_recursive(
    dir: &std::path::Path,
    skills_root: &std::path::Path,
    depth: usize,
) -> Vec<(std::path::PathBuf, String)> {
    let mut results = Vec::new();
    if depth > SCAN_MAX_DEPTH {
        log::warn!("扫描深度超过 {} 层，跳过: {:?}", SCAN_MAX_DEPTH, dir);
        return results;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_symlink() {
                continue;
            }
            if path.is_dir() {
                let skill_md = path.join("SKILL.md");
                if skill_md.exists() {
                    if let Some(meta) = parse_skill_meta(&skill_md) {
                        results.push((path.clone(), meta.name));
                    }
                }
                results.extend(scan_skills_recursive(&path, skills_root, depth + 1));
            }
        }
    }
    results
}
```

（注意：原实现找到 SKILL.md 后**仍继续下探**子目录，保留该语义。）

调用点更新：`services/resource/mod.rs:218` 改为 `scan_skills_recursive(skills_dir, skills_dir, 0)`。

`commands/resource.rs` 的 `scan_skill_dirs` 内部 `recurse` 函数同样加 `depth` 参数与 `SCAN_MAX_DEPTH` 上限、`if path.is_symlink() { continue; }`：

```rust
    const SCAN_MAX_DEPTH: usize = 4;
    fn recurse(dir: &std::path::Path, base: &std::path::Path, depth: usize, results: &mut Vec<String>) {
        if depth > SCAN_MAX_DEPTH {
            log::warn!("扫描深度超过 {} 层，跳过: {:?}", SCAN_MAX_DEPTH, dir);
            return;
        }
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_symlink() {
                    continue;
                }
                if path.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with('.') {
                        continue;
                    }
                    if path.join("SKILL.md").exists() {
                        if let Ok(rel) = path.strip_prefix(base) {
                            results.push(rel.to_string_lossy().to_string());
                        }
                    } else {
                        recurse(&path, base, depth + 1, results);
                    }
                }
            }
        }
    }
    recurse(base, base, 0, &mut results);
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd src-tauri && cargo test scan_depth_tests && cargo check`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/services/resource/mod.rs src-tauri/src/commands/resource.rs
git commit -m "fix(resource): scan depth limit and symlink loop protection"
```

---

### Task 10: 安装覆盖需确认（overwrite 参数）（P2，spec 用户故事 8 场景 2）

**Files:**
- Modify: `src-tauri/src/linker/mod.rs:126-163`（`install_to_repo`）
- Modify: `src-tauri/src/services/skill/mod.rs:17-39`、`src-tauri/src/commands/skill.rs:8-11`
- Modify: `src-tauri/src/services/plugin/mod.rs:28-48`（`install_plugin_to_repo`）
- Modify: `src-tauri/src/commands/resource.rs:127`（native 导入传 false）

- [ ] **Step 1: linker 加 overwrite 参数**

`install_to_repo`（126-163 行）的安全检查之后、复制之前替换 dest 处理：

```rust
pub fn install_to_repo(source: &Path, name: &str, overwrite: bool) -> Result<(), String> {
    // ……原路径穿越检查与名称校验保持不变……

    let repo = ensure_repo_dir();
    let dest = repo.join(name);

    if dest.exists() {
        if !overwrite {
            return Err(format!("已存在同名资源: {}", name));
        }
        fs::remove_dir_all(&dest).map_err(|e| format!("清理旧目录失败: {}", e))?;
    }

    copy_dir_recursive(&canonical, &dest)?;
    Ok(())
}
```

- [ ] **Step 2: 调用点透传**

- `services/skill/mod.rs` `install_skill` 签名改 `(source_path: &str, name: &str, overwrite: bool)`，`linker::install_to_repo(source, name, overwrite)?`；`services/mod.rs` 的 re-export 不变。
- `commands/skill.rs`：`pub fn install_skill(source_path: String, name: String, overwrite: Option<bool>)`，内部 `crate::services::install_skill(&source_path, &name, overwrite.unwrap_or(false))`。
- `services/plugin/mod.rs` `install_plugin_to_repo(source, name, overwrite: bool)`：dest 存在且 `!overwrite` → `Err(format!("已存在同名资源: {}", name))`，否则保持原删除逻辑。
- `commands/resource.rs:127` `import_native_resources`：`install_to_repo(path, &name, false)`（native 导入前已确认不存在，false 安全）。
- `services/resource/mod.rs`：Task 6 中的 auto_import 调用 `install_to_repo(skill_path, skill_name)` 改为三参 `install_to_repo(skill_path, skill_name, force)`（force 全量重扫保持覆盖导入语义）。

- [ ] **Step 3: 编译与全量测试**

Run: `cd src-tauri && cargo test && cargo clippy`
Expected: PASS（如有测试直接调用 install_to_repo 双参，补第三参）

- [ ] **Step 4: 提交**

```bash
git add src-tauri/src/linker/mod.rs src-tauri/src/services/skill/mod.rs src-tauri/src/services/mod.rs src-tauri/src/commands/skill.rs src-tauri/src/services/plugin/mod.rs src-tauri/src/commands/resource.rs src-tauri/src/services/resource/mod.rs
git commit -m "feat(resource): install requires explicit overwrite when name exists"
```

---

### Task 11: semver 校验强化（P2，spec 用户故事 8 场景 3）

**Files:**
- Modify: `src-tauri/Cargo.toml`（`semver = "1"`，追加在 `trash = "5"` 之后）
- Modify: `src-tauri/src/services/manifest/validator.rs:146-149`
- Test: `src-tauri/src/services/manifest/validator.rs`

- [ ] **Step 1: 写失败测试**

`validator.rs` 文件末尾追加：

```rust
#[cfg(test)]
mod semver_tests {
    use super::*;

    #[test]
    fn accepts_v_prefix_prerelease_and_build() {
        for ok in ["1.2.3", "v1.2.3", "0.0.1", "1.2.3-alpha.1", "v2.0.0-rc.1+build.5"] {
            assert!(is_valid_semver(ok), "应通过: {}", ok);
        }
        for bad in ["1.2", "1", "abc", "1.2.x", "1.2.3.4"] {
            assert!(!is_valid_semver(bad), "应拒绝: {}", bad);
        }
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test semver_tests`
Expected: FAIL（旧实现拒绝 `v1.2.3` / `1.2.3-alpha.1`）

- [ ] **Step 3: 实现新校验**

`validator.rs` 原 `is_valid_semver` 函数体替换为：

```rust
fn is_valid_semver(v: &str) -> bool {
    v.trim_start_matches('v').parse::<semver::Version>().is_ok()
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd src-tauri && cargo test semver_tests`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/services/manifest/validator.rs
git commit -m "feat(manifest): strict semver validation via semver crate"
```

---

### Task 12: 删除孤儿组件 McpManager（P2，spec 用户故事 7 场景 1）

**Files:**
- Delete: `src/components/mcp/McpManager.tsx`

- [ ] **Step 1: 确认无引用后删除**

Run: `grep -rn "McpManager" src/ --include="*.tsx" --include="*.ts"`
Expected: 仅 `src/components/mcp/McpManager.tsx` 自身

```bash
rm src/components/mcp/McpManager.tsx
rmdir src/components/mcp 2>/dev/null || true
```

- [ ] **Step 2: 构建验证 + 提交**

Run: `pnpm build`
Expected: 无错误

```bash
git add -A src/components/mcp
git commit -m "chore(ui): remove orphan McpManager component"
```

---

### Task 13: 接线 ManifestInstallDialog（P2，spec 用户故事 7 场景 2）

**Files:**
- Modify: `src/components/resources/ResourceByKindView.tsx`
- Modify: `src/i18n/locales/zh.json`、`src/i18n/locales/en.json`

- [ ] **Step 1: i18n 键**

`zh.json` resources 追加：`"installFromManifest": "从 Manifest 安装"`, `"manifestPathLabel": "资源目录中 mam.json 的路径"`, `"manifestPathPlaceholder": "D:\\resources\\my-skill\\mam.json"`；`en.json` 对应 `"Install from manifest"`, `"Path to mam.json in the resource directory"`, `"D:\\resources\\my-skill\\mam.json"`。

- [ ] **Step 2: 状态 + 入口按钮 + 弹窗**

`ResourceByKindView.tsx`：import `ManifestInstallDialog`（`./ManifestInstallDialog`）与 `FileJson` 图标。state 追加：

```tsx
const [manifestDlgOpen, setManifestDlgOpen] = useState(false);
const [manifestPath, setManifestPath] = useState("");
const [installDlgPath, setInstallDlgPath] = useState<string | null>(null);
const [installDlgOpen, setInstallDlgOpen] = useState(false);
```

`<h3>` 标题行（183 行）之后追加工具栏行：

```tsx
<div className="mb-3 flex justify-end">
  <Button size="sm" variant="outline" className="h-6 px-2 text-[10px]" onClick={() => setManifestDlgOpen(true)}>
    <FileJson className="mr-1 h-3 w-3" />
    {t("resources.installFromManifest")}
  </Button>
</div>
```

文件末尾追加两个弹窗：

```tsx
<Dialog open={manifestDlgOpen} onOpenChange={setManifestDlgOpen}>
  <DialogContent className="max-w-md">
    <DialogHeader>
      <DialogTitle>{t("resources.installFromManifest")}</DialogTitle>
    </DialogHeader>
    <div className="py-2">
      <label className="text-xs font-medium">{t("resources.manifestPathLabel")}</label>
      <input
        value={manifestPath}
        onChange={(e) => setManifestPath(e.currentTarget.value)}
        placeholder={t("resources.manifestPathPlaceholder")}
        className="h-8 w-full rounded border px-2 text-xs"
      />
    </div>
    <DialogFooter className="gap-2">
      <Button variant="outline" size="sm" onClick={() => setManifestDlgOpen(false)}>
        {t("common.cancel")}
      </Button>
      <Button
        size="sm"
        disabled={!manifestPath.trim()}
        onClick={() => {
          setInstallDlgPath(manifestPath.trim());
          setInstallDlgOpen(true);
          setManifestDlgOpen(false);
        }}
      >
        {t("common.confirm")}
      </Button>
    </DialogFooter>
  </DialogContent>
</Dialog>

<ManifestInstallDialog
  path={installDlgPath}
  open={installDlgOpen}
  onOpenChange={setInstallDlgOpen}
  onInstalled={async () => {
    const fresh = await listSsotResources();
    setResources(fresh);
  }}
/>
```

（若 `common.confirm` 键不存在，两语言文件 common 段各补 `"confirm": "确定"` / `"confirm": "Confirm"`。）

- [ ] **Step 3: 验证 + 提交**

Run: `pnpm check`
Expected: 通过

```bash
git add src/components/resources/ResourceByKindView.tsx src/i18n/locales/zh.json src/i18n/locales/en.json
git commit -m "feat(ui): wire ManifestInstallDialog with path entry on resources page"
```

---

### Task 14: 移除 native_extensions 死代码（P2，spec 用户故事 7 场景 3）

**Files:**
- Modify: `src-tauri/src/database/dao/extension.rs`（删 161-242 行 + 结构体迁移）
- Modify: `src-tauri/src/database/mod.rs:15,21-22`（删导出）
- Modify: `src-tauri/src/database/schema.rs:83-92`（删建表）
- Modify: `src-tauri/src/database/migration.rs`（DROP TABLE）
- Modify: `src-tauri/src/commands/resource.rs:86,98`（DTO 本地化）

- [ ] **Step 1: 结构体迁移到 commands**

`commands/resource.rs` 顶部追加（并保留 schema.rs 中 `NativeExtensionRecord` 用途注释语义）：

```rust
/// 原生（未纳管）资源的扫描结果 DTO
#[derive(Debug, Clone, serde::Serialize)]
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
```

`scan_native_resources`（86、98 行）两处 `crate::database::NativeExtensionRecord` 替换为 `NativeExtensionRecord`。

- [ ] **Step 2: 删除 DAO 与导出**

- `dao/extension.rs`：删除 161-242 行（`NativeExtensionRecord` 结构体、`insert_native_extension`、`list_native_extensions`、`mark_native_imported`）。
- `database/mod.rs`：15 行导出列表删 `NativeExtensionRecord`；21-22 行删 `insert_native_extension, list_native_extensions, mark_native_imported`。

- [ ] **Step 3: 删建表 + migration DROP**

- `schema.rs`：删除 83-92 行 `CREATE TABLE IF NOT EXISTS native_extensions (...)` 语句。
- `migration.rs` 的 `migrate` 函数末尾（`Ok(())` 之前）追加：

```rust
    // 015：native_extensions 表从未被业务写入，移除（历史库中 DROP）
    conn.execute_batch("DROP TABLE IF EXISTS native_extensions;")
        .map_err(|e| format!("移除 native_extensions 失败: {}", e))?;
```

`migration.rs` tests 模块追加：

```rust
    #[test]
    fn migrate_drops_native_extensions() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE native_extensions (id TEXT PRIMARY KEY);").unwrap();
        migrate(&conn).unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='native_extensions'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!exists);
    }
```

- [ ] **Step 4: 验证 + 提交**

Run: `cd src-tauri && cargo test && cargo clippy`
Expected: PASS

```bash
git add src-tauri/src/database/dao/extension.rs src-tauri/src/database/mod.rs src-tauri/src/database/schema.rs src-tauri/src/database/migration.rs src-tauri/src/commands/resource.rs
git commit -m "chore(db): remove unused native_extensions table and DAO"
```

---

### Task 15: React Query 接线（P2，spec 用户故事 7 场景 4）

**Files:**
- Modify: `src/lib/query/queries/resources.ts`（改造为 ssotResources 查询）
- Modify: `src/lib/query/mutations/resources.ts`（invalidate 键更新）
- Modify: `src/components/resources/ResourceByKindView.tsx`
- Modify: `docs/engineering/architecture/adr/004-react-query-vs-polling.md`（追加状态注记）

- [ ] **Step 1: 改造查询封装**

`queries/resources.ts` 整体替换：

```ts
import { useQuery } from "@tanstack/react-query";
import { listSsotResources } from "@/lib/api/resource";
import type { SsotResources } from "@/types/extension";

export const SSOT_RESOURCES_KEY = ["ssotResources"] as const;

export function useSsotResourcesQuery() {
  return useQuery<SsotResources>({
    queryKey: SSOT_RESOURCES_KEY,
    queryFn: listSsotResources,
    staleTime: 5000,
  });
}
```

`mutations/resources.ts` 的 `onSuccess` 替换为 `qc.invalidateQueries({ queryKey: ["ssotResources"] })`。

- [ ] **Step 2: ResourceByKindView 接入**

import 替换：`useEffect` 移除，新增 `useQueryClient`（`@tanstack/react-query`）与 `useSsotResourcesQuery`。组件内：

```tsx
const qc = useQueryClient();
const { data: resources } = useSsotResourcesQuery();
const refresh = () => qc.invalidateQueries({ queryKey: ["ssotResources"] });
```

原 `const [resources, setResources] = useState...`、`useEffect` 删除；所有 `const fresh = await listSsotResources(); setResources(fresh);` 替换为 `await refresh();`。`handleToggleMcp` 改用 `useToggleMcpMutation`：

```tsx
const toggleMcp = useToggleMcpMutation();
// handleToggleMcp 内 invoke(...) 替换为：
await toggleMcp.mutateAsync({ mcpName: name, toolId, enabled });
```

- [ ] **Step 3: ADR 注记**

`docs/engineering/architecture/adr/004-react-query-vs-polling.md` 末尾追加：

```markdown
## 状态更新（2026-08-25）

资源页（ResourceByKindView）已接入 React Query：`useSsotResourcesQuery`（queryKey `ssotResources`）+ `useToggleMcpMutation`，写操作后按 key invalidate。会话轮询仍为自定义 hook，维持原结论"会话场景保留轮询"。
```

- [ ] **Step 4: 验证 + 提交**

Run: `pnpm check`
Expected: 通过

```bash
git add src/lib/query/queries/resources.ts src/lib/query/mutations/resources.ts src/components/resources/ResourceByKindView.tsx docs/engineering/architecture/adr/004-react-query-vs-polling.md
git commit -m "refactor(ui): resource list backed by React Query with invalidation"
```

---

### Task 16: 批量启停 + 搜索扩展（P2，spec 用户故事 9）

**Files:**
- Modify: `src/components/resources/ResourceByKindView.tsx`
- Modify: `src/i18n/locales/zh.json`、`src/i18n/locales/en.json`

- [ ] **Step 1: i18n 键**

`zh.json` resources 追加：`"allToolsOn": "全部启用"`, `"allToolsOff": "全部禁用"`, `"batchDone": "完成 {{ok}} 项，跳过 {{skipped}} 项"`, `"batchFailed": "{{n}} 项失败"`；`en.json` 对应 `"Enable all"`, `"Disable all"`, "Done: {{ok}}, skipped: {{skipped}}", "{{n}} failed"。

- [ ] **Step 2: MCP / Plugin 搜索过滤**

现有 `filteredSkills`（62-66 行）之后追加：

```tsx
const filterFn = (r: { name: string; enabledTools: string[] }) => {
    if (!search.trim()) return true;
    const q = search.toLowerCase();
    return [r.name, ...r.enabledTools].some((x) => x.toLowerCase().includes(q));
  };
  const filteredMcp = resources.mcp.filter(filterFn);
  const filteredPlugins = resources.plugins.filter(filterFn);
```

（`filteredSkills` 改为 `resources.skills.filter(filterFn)`。）MCP/Plugin 列表的 `.map` 数据源与计数（`resources.mcp.length` → `filteredMcp.length`、`resources.plugins.length` → `filteredPlugins.length`）同步替换；搜索框从 Skills 小节上移到标题行右侧（与「从 Manifest 安装」同行），三个列表共用。

- [ ] **Step 3: 批量启停处理器**

处理器区追加：

```tsx
const handleToggleAll = async (res: SsotResource, enable: boolean) => {
  let ok = 0;
  let skipped = 0;
  let failed = 0;
  for (const tool of TOOLS) {
    const isEnabled = res.enabledTools.includes(tool.id);
    if (enable === isEnabled) continue;
    try {
      if (res.kind === "skill") {
        if (enable) {
          await enableSkillForTool(res.name, tool.id);
        } else {
          const ty = await checkSkillTargetType(tool.id, res.name);
          if (ty === "native") {
            skipped++; // 原生目录不批量删除，跳过
            continue;
          }
          await disableSkillForTool(tool.id, res.name);
        }
      } else if (res.kind === "mcp") {
        if (enable) {
          try {
            await importMcpToSsot(res.name);
          } catch (_) { /* 已导入 */ }
        }
        await invoke("toggle_mcp_for_tool", { mcpName: res.name, toolId: tool.id, enabled: enable });
      } else {
        await invoke("toggle_plugin_for_tool", { pluginName: res.name, toolId: tool.id, enabled: enable, kind: "file" });
      }
      ok++;
    } catch (e) {
      failed++;
      console.error(e);
    }
  }
  if (failed > 0) toast.error(t("resources.batchFailed", { n: failed }));
  toast.success(t("resources.batchDone", { ok, skipped }));
  await refresh();
};
```

（import 区需补 `SsotResource` 类型：`import type { SsotResources, SsotResource } from "@/types/extension";`。）

- [ ] **Step 4: 三处资源行加批量按钮**

每个资源行工具按钮组 `<div className="flex gap-1">` 内、TOOLS.map 之前追加（skill/mcp/plugin 三处，变量名对应替换）：

```tsx
<Button
  variant="ghost"
  size="sm"
  className="h-6 px-1.5 text-[10px]"
  title={skill.enabledTools.length === TOOLS.length ? t("resources.allToolsOff") : t("resources.allToolsOn")}
  onClick={() => handleToggleAll(skill, skill.enabledTools.length !== TOOLS.length)}
>
  {skill.enabledTools.length === TOOLS.length ? t("resources.allToolsOff") : t("resources.allToolsOn")}
</Button>
```

- [ ] **Step 5: 验证 + 提交**

Run: `pnpm check`
Expected: 通过

```bash
git add src/components/resources/ResourceByKindView.tsx src/i18n/locales/zh.json src/i18n/locales/en.json
git commit -m "feat(ui): per-row enable/disable-all and search across all resource kinds"
```

---

### Task 17: 收尾验证

- [ ] **Step 1: 后端全量**

Run: `cd src-tauri && cargo fmt && cargo test && cargo clippy -- -D warnings`
Expected: 全部通过（clippy 以仓库现有告警基线为准，不得新增）

- [ ] **Step 2: 前端全量**

Run: `pnpm check`
Expected: format + lint + build + i18n 对齐全部通过

- [ ] **Step 3: 手工验证清单（pnpm tauri:dev）**

1. Windows 上对一个 native skill 点禁用 → 资源管理器回收站可找回（spec 故事 1 场景 1）
2. 分别卸载：普通导入 skill / MCP / file plugin → SSOT 目录与 DB 行消失、工具配置键移除（故事 2 场景 1/3/4）
3. 在 `opencode.json` 手工加注释 → 启停 MCP → 注释保留（故事 3 场景 1）
4. claude 与 codex 放同名 skill → rescan → 两工具都亮（故事 4 场景 1）
5. CLI 新装 skill 后重启应用 → 资源页自动出现（故事 5 场景 1）
6. 删除 SSOT 中某 skill 目录后重启 → 资源行显示「链接损坏」/链接被清理（故事 6 场景 1）
7. 「从 Manifest 安装」输入合法 mam.json 路径 → 权限弹窗 → 安装成功（故事 7 场景 2）
8. 搜索框在 MCP/Plugin 页签过滤生效；行级全部启停含 native 跳过提示（故事 9）

- [ ] **Step 4: 提交（如有格式化改动）**

```bash
git add -A
git commit -m "chore: formatting after 015 implementation"
```

---

## 自检记录

- **Spec 覆盖**：故事 1→Task 1；故事 2→Task 3/4；故事 3→Task 5；故事 4→Task 6；故事 5→Task 7；故事 6→Task 8；故事 7（场景 1-4）→Task 12/13/14/15；故事 8→Task 9/10/11；故事 9→Task 16。015 spec 的 P0-②/P0-③ 由 016 承接（spec 输入已注明）。
- **类型一致性**：`install_to_repo(source, name, overwrite)` 三参在 Task 6/10 中调用一致；`uninstall_resource(kind, name)` 前后端一致；`SsotResource.broken_tools` 与前端 `brokenTools` 经 serde rename 对齐。
- **占位符**：无 TBD/TODO；plugin TOML 分支标注"保持不变"处均为既有代码不需修改。
