# SSOT 后续修复与改进 实现计划

> **面向 AI 代理的工作者：** 使用 subagent-driven-development 逐任务实现此计划。

**目标：** 修复导入后刷新 bug + codex 路径不一致、合并 SsotRepoOverview 到按资源视图、套件文件夹递归扫描 + 冒号显示

**技术栈：** Tauri 2 + Rust + React 19 + TypeScript + shadcn/ui + Tailwind CSS v4

---

## 文件结构

| 文件 | 操作 | 职责 |
|------|------|------|
| `src-tauri/src/commands/resource.rs` | 修改 | codex 路径修正、递归扫描、导入保留路径 |
| `src-tauri/src/linker/mod.rs` | 修改 | `install_to_repo` 允许 `/` |
| `src/components/resources/ResourceByToolView.tsx` | 修改 | handleImport 加 loadDuplicates、冒号显示 |
| `src/components/resources/ResourceByKindView.tsx` | 修改 | 自行加载 SSOT 数据、冒号显示 |
| `src/components/resources/ExtensionList.tsx` | 修改 | 移除 SsotRepoOverview |
| `src/components/resources/SsotRepoOverview.tsx` | 删除 | 功能已合并 |

---

### 任务 1：修复 handleImport 未刷新重复检测 + codex 扫描路径

**文件：** `src/components/resources/ResourceByToolView.tsx`, `src-tauri/src/commands/resource.rs`

- [ ] **步骤 1：前端 handleImport 添加 loadDuplicates 调用**

在 `handleImport` 函数的成功分支中，`await loadToolResources(toolId)` 之后添加：
```tsx
await loadDuplicates(toolId);
```

- [ ] **步骤 2：后端 scan_native_resources 路径改用 adapter**

将 `scan_native_resources` 中的硬编码路径匹配改为使用 adapter。在函数开头加入 adapter 获取逻辑：
```rust
let adapters = crate::adapter::get_all_adapters();
let adapter = adapters.iter().find(|a| a.id() == tool_id);
let skill_dir = adapter.map(|a| a.skill_dirs().into_iter().next());
```

替换原有的 `match tool_id.as_str() { ... }` 块。

- [ ] **步骤 3：后端 detect_duplicate_skills 路径改用 adapter**

同理，将 `detect_duplicate_skills` 中的硬编码路径改为使用 adapter：
```rust
let adapters = crate::adapter::get_all_adapters();
let tool_skill_dir = adapters.iter()
    .find(|a| a.id() == tool_id)
    .and_then(|a| a.skill_dirs().into_iter().next());
```

替换原有的 `match tool_id.as_str() { ... }` 块。

- [ ] **步骤 4：后端 cleanup_duplicate_skills 路径改用 adapter**

同理修改 `cleanup_duplicate_skills` 中的路径获取。

- [ ] **步骤 5：验证**

```bash
cd src-tauri && cargo check 2>&1
```
预期：0 errors

```bash
npx tsc --noEmit 2>&1
```
预期：0 errors

- [ ] **步骤 6：Commit**

```bash
git add src/components/resources/ResourceByToolView.tsx src-tauri/src/commands/resource.rs
git commit -m "fix: refresh duplicates after import and use adapter for tool paths"
```

---

### 任务 2：递归扫描含 SKILL.md 的目录

**文件：** `src-tauri/src/commands/resource.rs`

- [ ] **步骤 1：添加递归扫描辅助函数**

在 `resource.rs` 顶部（结构体定义之前）添加一个辅助函数：
```rust
/// 递归扫描目录，找到所有直接包含 SKILL.md 的子目录
/// 返回相对路径列表（如 "brainstorming", "superpowers/brainstorming"）
fn scan_skill_dirs(base: &std::path::Path) -> Vec<String> {
    let mut results = Vec::new();
    fn recurse(dir: &std::path::Path, base: &std::path::Path, results: &mut Vec<String>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with('.') { continue; }
                    // 检查该目录是否直接含 SKILL.md
                    if path.join("SKILL.md").exists() {
                        if let Ok(rel) = path.strip_prefix(base) {
                            results.push(rel.to_string_lossy().to_string());
                        }
                    } else {
                        // 递归进入子目录
                        recurse(&path, base, results);
                    }
                }
            }
        }
    }
    recurse(base, base, &mut results);
    results.sort();
    results
}
```

- [ ] **步骤 2：list_ssot_resources 使用递归扫描**

将 `list_ssot_resources` 中的 `scan_dir` 闭包改为使用 `scan_skill_dirs`：
```rust
let scan_dir = |dir: &std::path::Path, kind: &str| -> Vec<SsotResource> {
    let names = scan_skill_dirs(dir);
    names.into_iter().map(|name| {
        let ext_id = format!("{}-{}", kind, name);
        let enabled_tools: Vec<String> = assignments.iter()
            .filter(|a| a.extension_id == ext_id && a.enabled)
            .map(|a| a.agent_tool_id.clone())
            .collect();
        SsotResource { name, kind: kind.to_string(), enabled_tools }
    }).collect()
};
```

注意：MCP 和 Plugin 暂时也走这个函数（它们目前没有子目录结构，结果和之前一样）。

- [ ] **步骤 3：scan_native_resources 使用递归扫描**

在 `scan_native_resources` 中，将顶层遍历改为递归扫描。原有逻辑是遍历 `skill_dir` 下的直接子目录，改为：
```rust
let skill_names = scan_skill_dirs(&dir);
for name in skill_names {
    let path = dir.join(&name);
    let ext_id = format!("skill-{}", name);
    let exists = existing.iter().any(|e| e.id == ext_id);
    if !exists {
        results.push(crate::database::NativeExtensionRecord {
            id: ext_id, kind: "skill".to_string(), name: name.clone(),
            description: None, source_path: path.to_string_lossy().to_string(),
            source_tool: tool_id.clone(), detected_at: chrono::Utc::now().to_rfc3339(),
            imported: false,
        });
    }
}
```

- [ ] **步骤 4：detect_duplicate_skills 使用递归扫描**

将 `detect_duplicate_skills` 中的遍历改为使用 `scan_skill_dirs`：
```rust
let ssot_skills = scan_skill_dirs(&repo);
for name in ssot_skills {
    let tool_path = tool_skill_dir.join(&name);
    if tool_path.exists() && !tool_path.is_symlink() {
        duplicates.push(name);
    }
}
```

- [ ] **步骤 5：cleanup_duplicate_skills 适配**

`cleanup_duplicate_skills` 中的路径拼接不需要改动——它已经是 `repo.join(name)` 和 `tool_skill_dir.join(name)`，现在 name 可能含 `/`，拼接出的路径仍然正确。

- [ ] **步骤 6：验证**

```bash
cd src-tauri && cargo check 2>&1
```

- [ ] **步骤 7：Commit**

```bash
git add src-tauri/src/commands/resource.rs
git commit -m "feat: recursive scan for SKILL.md in suite directories"
```

---

### 任务 3：import_native_resources 保留套件相对路径 + install_to_repo 允许 /

**文件：** `src-tauri/src/commands/resource.rs`, `src-tauri/src/linker/mod.rs`

- [ ] **步骤 1：install_to_repo 允许名称含 /**

在 `src-tauri/src/linker/mod.rs` 的 `install_to_repo` 函数中，第 131 行：
```rust
if name.contains("..") || name.contains('/') || name.contains('\\') {
```
改为：
```rust
if name.contains("..") || name.contains('\\') {
```

同时确保 `dest = repo.join(name)` 在 name 含 `/` 时能正确创建嵌套目录。由于 `copy_dir_recursive` 内部有 `fs::create_dir_all`，这已经能处理。

- [ ] **步骤 2：import_native_resources 保留相对路径**

修改 `import_native_resources`，计算源路径相对于工具 skill 目录的相对路径，用作 SSOT 中的目标路径。

在函数中，对每个 `(source_path, name)` 项：
```rust
let path = std::path::Path::new(&source_path);
if !path.exists() { skipped += 1; continue; }

// 计算 relative_path：如果源路径在某个工具的 skill 目录下，取相对路径
// 否则用 name 作为顶层路径
let relative_path = name;

if let Err(e) = crate::linker::install_to_repo(path, &relative_path) {
    log::warn!("导入 {} 失败: {}", relative_path, e); skipped += 1; continue;
}
let ext = crate::database::ExtensionRecord {
    id: format!("skill-{}", relative_path), kind: "skill".to_string(),
    name: relative_path.clone(), description: None,
    source_path: source_path.clone(), source_url: None,
    version: None, tags: None, suite: None, source_tool: None, is_native: true,
};
let _ = crate::database::insert_extension(&ext);
imported += 1;
```

注意：前端传入的 `name` 已经是相对于工具 skill 目录的相对路径（由 `scan_native_resources` 的递归扫描产生），所以直接用 `name` 作为 `relative_path` 即可。

- [ ] **步骤 3：验证**

```bash
cd src-tauri && cargo check 2>&1
```

- [ ] **步骤 4：Commit**

```bash
git add src-tauri/src/linker/mod.rs src-tauri/src/commands/resource.rs
git commit -m "feat: preserve suite relative path on import, allow / in skill names"
```

---

### 任务 4：删除 SsotRepoOverview，合并到 ResourceByKindView

**文件：** `src/components/resources/ExtensionList.tsx`, `src/components/resources/ResourceByKindView.tsx`, `src/components/resources/SsotRepoOverview.tsx`

- [ ] **步骤 1：ExtensionList 移除 SsotRepoOverview**

在 `ExtensionList.tsx` 中：
- 移除 `import { SsotRepoOverview } from "./SsotRepoOverview";`
- 移除 `<SsotRepoOverview />` 渲染
- 移除 `extensions` state 和 `load` 函数
- `ResourceByKindView` 不再接收 `extensions`、`onToggleMcp`、`onTogglePlugin` props

- [ ] **步骤 2：ResourceByKindView 自行加载 SSOT 数据**

重写 `ResourceByKindView.tsx`：
- 导入 `listSsotResources` from `@/lib/api/resource`
- 导入 `SsotResources`, `SsotResource` 类型
- 添加 `useEffect` 加载 `listSsotResources()`
- 三栏数据来自 `resources.skills`、`resources.mcp`、`resources.plugins`
- 每行显示：skill 名称（冒号格式）+ 已接入工具列表
- 保留搜索功能
- `handleToggleMcp` 和 `handleTogglePlugin` 内部实现（从父组件移入）

- [ ] **步骤 3：删除 SsotRepoOverview.tsx**

```bash
rm src/components/resources/SsotRepoOverview.tsx
```

- [ ] **步骤 4：验证**

```bash
npx tsc --noEmit 2>&1
```

- [ ] **步骤 5：Commit**

```bash
git add -A
git commit -m "refactor: merge SsotRepoOverview into ResourceByKindView"
```

---

### 任务 5：前端冒号显示

**文件：** `src/components/resources/ResourceByKindView.tsx`, `src/components/resources/ResourceByToolView.tsx`

- [ ] **步骤 1：添加冒号格式化工具函数**

在 `ResourceByKindView.tsx` 中添加：
```tsx
function formatSkillName(name: string): string {
  return name.includes("/") ? name.replace("/", ": ") : name;
}
```

- [ ] **步骤 2：ResourceByKindView 中使用格式化**

在展示 skill 名称时调用 `formatSkillName(s.name)`。

- [ ] **步骤 3：ResourceByToolView 中使用格式化**

在 `ToolResourceList` 中展示 skill 名称时也调用 `formatSkillName`。需要将此函数导出或在 `ResourceByToolView.tsx` 中也添加。

- [ ] **步骤 4：验证**

```bash
npx tsc --noEmit 2>&1
```

- [ ] **步骤 5：Commit**

```bash
git add src/components/resources/ResourceByKindView.tsx src/components/resources/ResourceByToolView.tsx
git commit -m "feat: display nested skill names with colon separator"
```

---

### 任务 6：最终验证

- [ ] **步骤 1：Rust 编译验证**

```bash
cd src-tauri && cargo check 2>&1
```

- [ ] **步骤 2：TS 类型验证**

```bash
npx tsc --noEmit 2>&1
```

- [ ] **步骤 3：确认无残留引用**

```bash
grep -r "SsotRepoOverview" src/ 2>/dev/null
```
预期：无结果

---

## 不在此计划中的内容

- MCP SSOT 导入/导出 — Phase 3 独立计划
- 已有 DB 记录的 ext_id 迁移 — 旧记录在重新导入前不自动迁移
