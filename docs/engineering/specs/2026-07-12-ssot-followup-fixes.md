# SSOT 仓库后续修复与改进

## 背景

SSOT 集中仓库架构完善（第一批 9 个任务）完成后，用户在实际使用中发现三个问题：

1. **Bug：导入后工具页面未刷新 + 重复检测不工作** — skill 导入到公共库后，工具视图未刷新重复检测；codex 工具的扫描路径不一致
2. **UI 重复：MAM 仓库概览与下方列表上下重复** — `SsotRepoOverview` 在页面顶部，下方"按资源"视图又有 Skill/MCP/Plugin 三栏，内容重复
3. **套件文件夹结构缺失** — `superpowers/` 等套件目录的子 skill 未被递归发现和正确展示

---

## 第 1 部分：Bug 修复 — 导入后刷新 + codex 路径修正

### 1.1 根因 A：前端导入后未刷新重复检测

**文件：** `src/components/resources/ResourceByToolView.tsx`

`handleImport` 函数（第 54 行）导入成功后只调了 `loadToolResources(toolId)`，没有调 `loadDuplicates(toolId)`。所以导入后重复检测列表不更新。

**修复：** 在 `handleImport` 成功分支中添加 `await loadDuplicates(toolId)`：

```tsx
if (result.imported > 0) {
  toast.success(`"${item.name}" 导入成功`);
  await loadToolResources(toolId);
  await loadDuplicates(toolId);  // 新增
}
```

### 1.2 根因 B：codex 扫描路径不一致

**文件：** `src-tauri/src/commands/resource.rs`

`scan_native_resources` 第 52 行 codex 路径硬编码为 `~/.codex/skills/`，但 `CodexAdapter.skill_dirs()` 返回 `~/.agents/skills/`，`detect_duplicate_skills` 和 `cleanup_duplicate_skills` 也用的是 `~/.agents/skills/`。

**修复：** 将 `scan_native_resources` 中的硬编码路径改为使用 `adapter.skill_dirs()`，与 `detect_duplicate_skills` 保持一致。具体做法：

- 在 `scan_native_resources` 中用 `crate::adapter::get_all_adapters()` 遍历，匹配 `tool_id`，取 `skill_dirs()[0]` 作为扫描路径
- 同理将 `detect_duplicate_skills` 和 `cleanup_duplicate_skills` 中的硬编码路径也改为使用 adapter

这样消除三处硬编码路径不一致，后续添加新工具也不需要改这些函数。

### 1.3 涉及文件

| 文件 | 改动 |
|------|------|
| `src/components/resources/ResourceByToolView.tsx` | `handleImport` 添加 `loadDuplicates` 调用 |
| `src-tauri/src/commands/resource.rs` | `scan_native_resources`、`detect_duplicate_skills`、`cleanup_duplicate_skills` 改用 adapter 获取路径 |

---

## 第 2 部分：UI 布局 — 合并 SsotRepoOverview 到"按资源"视图

### 2.1 当前问题

```
ExtensionList:
  ┌─ SsotRepoOverview（SSOT 仓库概览）── 顶部 ─┐
  │ Skills (12)  MCP (3)  Plugins (5)            │
  │ systematic-debugging  ✓ Claude, Codex        │
  │ ...                                          │
  └──────────────────────────────────────────────┘
  ┌─ Toolbar ───────────────────────────────────┐
  │ [按资源] [按工具]          [扫描原生资源]    │
  └──────────────────────────────────────────────┘
  ┌─ ResourceByKindView ────────────────────────┐
  │ Skill (12)  MCP (3)  Plugins (5)  ← 重复！  │
  │ systematic-debugging  [Claude] [Codex]      │
  │ ...                                          │
  └──────────────────────────────────────────────┘
```

上下两个面板内容重复。

### 2.2 设计方案：合并

去掉独立的 `SsotRepoOverview` 组件，将 SSOT 仓库内容直接合并到 `ResourceByKindView` 中：

```
ExtensionList:
  ┌─ Toolbar ───────────────────────────────────┐
  │ [按资源] [按工具]          [扫描原生资源]    │
  └──────────────────────────────────────────────┘
  ┌─ ResourceByKindView（含 SSOT 仓库数据）─────┐
  │ Skill (12)                                   │
  │ superpowers: brainstorming  [Claude] [Codex]│
  │ speckit-analyze             [Claude]         │
  │ ui-ux-pro-max               [Claude]         │
  │ MCP (3)                                      │
  │ ...                                          │
  │ Plugins (5)                                  │
  │ ...                                          │
  └──────────────────────────────────────────────┘
```

### 2.3 具体改动

1. **`ExtensionList.tsx`**：
   - 移除 `SsotRepoOverview` 导入和 `<SsotRepoOverview />` 渲染
   - 移除 `extensions` state 和 `load` 函数（数据加载下放到 `ResourceByKindView`）
   - `ResourceByKindView` 不再接收 `extensions` prop

2. **`ResourceByKindView.tsx`**：
   - 自行调用 `listSsotResources()` API 获取数据（替代从父组件传入的 `extensions`）
   - 内部管理 `resources` state 和加载逻辑
   - 每行展示 skill 名称（含冒号格式）+ 已接入工具按钮
   - 保留搜索功能
   - 按首字母升序排列

3. **`SsotRepoOverview.tsx`**：删除此组件（功能已合并到 `ResourceByKindView`）

### 2.4 涉及文件

| 文件 | 改动 |
|------|------|
| `src/components/resources/ExtensionList.tsx` | 移除 SsotRepoOverview，移除 extensions state |
| `src/components/resources/ResourceByKindView.tsx` | 自行加载 SSOT 数据，替代 extensions prop |
| `src/components/resources/SsotRepoOverview.tsx` | 删除 |

---

## 第 3 部分：套件文件夹结构保留 + 嵌套层级用冒号显示

### 3.1 当前问题

`~/.mam/skills/` 中已有 `superpowers/` 目录（含 20 个子 skill 如 `brainstorming/SKILL.md`），但 `list_ssot_resources` 和 `scan_native_resources` 只扫描顶层目录，不会递归发现 `superpowers/brainstorming`。

### 3.2 冒号显示规则

只有当 skill 嵌套在子目录中时（相对路径含 `/`），显示为 `父目录: 子目录`。扁平命名的 skill 保持原样不变。

| 文件系统路径 | 显示名称 |
|-------------|---------|
| `superpowers/brainstorming/` | `superpowers: brainstorming` |
| `superpowers/systematic-debugging/` | `superpowers: systematic-debugging` |
| `speckit-analyze/` | `speckit-analyze` |
| `ckm:banner-design/` | `ckm:banner-design` |
| `brainstorming/` | `brainstorming` |

### 3.3 递归扫描设计

`list_ssot_resources` 的 `scan_dir` 闭包改为递归扫描：

```
扫描算法：
1. 读取目录下的所有条目
2. 对每个条目：
   a. 如果是目录：
      - 如果该目录直接含 SKILL.md → 这是一个 skill，记录相对路径
      - 否则递归进入子目录扫描
   b. 跳过文件和非目录条目
3. 相对路径 = 相对于仓库根目录的路径（如 "superpowers/brainstorming"）
```

### 3.4 ext_id 格式

DB 中 `extension_id` 统一用 `skill-{relative_path}`：

| skill 位置 | ext_id |
|-----------|--------|
| `~/.mam/skills/brainstorming/` | `skill-brainstorming` |
| `~/.mam/skills/superpowers/brainstorming/` | `skill-superpowers/brainstorming` |
| `~/.mam/skills/speckit-analyze/` | `skill-speckit-analyze` |

### 3.5 导入保留结构

`import_native_resources` 中，源路径如果是套件下的子目录（如 `~/.agents/skills/superpowers/brainstorming/`），复制到 SSOT 时保留相对路径（`~/.mam/skills/superpowers/brainstorming/`）。如果导入的是整个套件目录（如 `~/.agents/skills/superpowers/`），则递归扫描其中的子 skill，逐个复制保留路径。

**`install_to_repo` 修改：** 当前 `install_to_repo` 第 131 行检查 `name.contains('/')` 会拒绝含 `/` 的路径。需要修改为只禁止 `..` 和 `\`，允许 `/`（因为相对路径需要它）。路径穿越检查（`canonicalize` + `starts_with` 敏感目录）仍然有效。

**导入整个套件目录时的 DB 处理：** 递归发现每个子 skill 后，对每个子 skill 单独创建 DB 记录（ext_id = `skill-{relative_path}`），而不是为套件目录本身创建记录。

### 3.6 重复检测适配

`detect_duplicate_skills` 也递归扫描 SSOT 仓库，对每个含 `SKILL.md` 的目录，检查工具 skill 目录中是否有同名的相对路径结构。`cleanup_duplicate_skills` 同理。

### 3.7 前端冒号显示

前端展示时，对 `name` 字段中含 `/` 的，替换为 `: `（斜杠 + 空格）。不含 `/` 的保持原样。`enabledTools` 等其他字段不变。

### 3.8 排序

所有 skill 列表按显示名称的首字母升序排列。冒号前缀参与排序（如 `superpowers: brainstorming` 排在 `s` 开头的位置，`speckit-analyze` 也排在 `s` 开头的位置）。

### 3.9 涉及文件

| 文件 | 改动 |
|------|------|
| `src-tauri/src/commands/resource.rs` | `list_ssot_resources`、`scan_native_resources`、`detect_duplicate_skills`、`cleanup_duplicate_skills` 改为递归扫描 |
| `src-tauri/src/commands/resource.rs` | `import_native_resources` 保留套件相对路径 |
| `src-tauri/src/linker/mod.rs` | `install_to_repo` 修改名称检查，允许 `/` 但仍禁止 `..` 和 `\` |
| `src/components/resources/ResourceByKindView.tsx` | skill 名称显示时 `/` → `: ` |
| `src/components/resources/ResourceByToolView.tsx` | skill 名称显示时 `/` → `: ` |

---

## 不在此计划中的内容

- **MCP SSOT 导入/导出** — Phase 3 独立计划
- **前端 SsotRepoOverview 组件删除后的导入清理** — 确保无残留引用
- **已有 DB 记录的 ext_id 迁移** — 之前导入的 skill 使用旧的 `skill-{name}` 格式（不含路径），递归扫描后新导入的 skill 使用 `skill-{relative_path}` 格式。旧记录在重新导入前不会自动迁移，但不影响功能——`list_ssot_resources` 直接扫描文件系统，不依赖 DB 记录
