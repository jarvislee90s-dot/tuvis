# SSOT 集中仓库架构完善 实现计划

> **面向 AI 代理的工作者：** 使用 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 建立 SSOT 仓库的完整 UI 展示 + Skill 清理机制（符号链接替换原始文件）+ Plugin 路径修正

**架构：** 新增 `list_ssot_resources`、`detect_duplicate_skills`、`cleanup_duplicate_skills` 三个 Rust 命令。前端新增 `SsotRepoOverview` 组件展示 SSOT 仓库，在按工具视图中新增"清理"按钮。修复 `enable_file_plugin` 的仓库路径 bug。

**技术栈：** Tauri 2 + Rust + React 19 + TypeScript + shadcn/ui + Tailwind CSS v4

---

## 文件结构

| 文件 | 操作 | 职责 |
|------|------|------|
| `src-tauri/src/linker/mod.rs` | 修改 | 新增 `replace_with_symlink` 函数 |
| `src-tauri/src/services/plugin/mod.rs` | 修改 | 修复 `enable_file_plugin` 路径 bug |
| `src-tauri/src/commands/resource.rs` | 修改 | 新增 `list_ssot_resources`、`detect_duplicate_skills`、`cleanup_duplicate_skills` 命令 |
| `src-tauri/src/lib.rs` | 修改 | `generate_handler!` 中注册 3 个新命令 |
| `src/components/resources/SsotRepoOverview.tsx` | 新建 | SSOT 仓库概览组件 |
| `src/components/resources/ResourceByToolView.tsx` | 修改 | 加入"清理"按钮，展示重复检测结果 |
| `src/components/resources/ExtensionList.tsx` | 修改 | 页面顶部加入 `SsotRepoOverview` |
| `src/types/extension.ts` | 修改 | 新增 `SsotResource`、`SsotResources`、`DuplicateSkill` 类型 |
| `src/lib/api/resource.ts` | 修改 | 新增 3 个 API 函数 |

---

### 任务 1：修复 `enable_file_plugin` 的 Plugin 仓库路径 bug

**文件：** `src-tauri/src/services/plugin/mod.rs:46`

`enable_file_plugin` 第 46 行用 `linker::ensure_repo_dir()` 获取源路径，该函数返回 `~/.mam/skills/`，但 Plugin 的 SSOT 仓库应该是 `~/.mam/plugins/`。

- [ ] **步骤 1：修复源路径**

将第 46 行：
```rust
let repo = linker::ensure_repo_dir();
```
替换为：
```rust
fn ensure_plugin_repo_dir() -> std::path::PathBuf {
    let repo = dirs::home_dir().unwrap_or_default().join(".mam").join("plugins");
    let _ = std::fs::create_dir_all(&repo);
    repo
}
```
并将第 46 行改为：
```rust
let repo = ensure_plugin_repo_dir();
```

同时在文件末尾的 `disable_file_plugin`（第 77 行附近）不需要改动——它只是删除目标路径的链接，不涉及仓库。

- [ ] **步骤 2：运行 cargo check 验证**

```bash
cd src-tauri && cargo check 2>&1
```
预期：0 errors

- [ ] **步骤 3：Commit**

```bash
git add src-tauri/src/services/plugin/mod.rs
git commit -m "fix: enable_file_plugin use plugins/ repo dir instead of skills/"
```

---

### 任务 2：新增 `replace_with_symlink` 函数到 linker

**文件：** `src-tauri/src/linker/mod.rs`

- [ ] **步骤 1：在 `remove_link` 之后插入 `replace_with_symlink`**

在 `remove_link` 函数（第 84 行之后）后面插入：

```rust
/// 将原始目录替换为指向 SSOT 仓库的符号链接
///
/// 安全校验：target 必须存在且不是符号链接（防止重复清理）
/// 操作流程：删除 target 目录 → 创建符号链接 target → source
pub fn replace_with_symlink(source: &Path, target: &Path) -> Result<(), String> {
    if !target.exists() {
        return Err(format!("目标路径不存在: {}", target.display()));
    }
    if target.is_symlink() {
        return Err(format!("目标已是符号链接，无需清理: {}", target.display()));
    }
    if !source.exists() {
        return Err(format!("源路径不存在: {}", source.display()));
    }

    // 删除原始目录
    if target.is_dir() {
        std::fs::remove_dir_all(target)
            .map_err(|e| format!("删除原始目录失败: {}", e))?;
    } else {
        std::fs::remove_file(target)
            .map_err(|e| format!("删除原始文件失败: {}", e))?;
    }

    // 创建符号链接
    create_link(source, target)
}
```

- [ ] **步骤 2：运行 cargo check 验证**

```bash
cd src-tauri && cargo check 2>&1
```
预期：0 errors

- [ ] **步骤 3：Commit**

```bash
git add src-tauri/src/linker/mod.rs
git commit -m "feat: add replace_with_symlink function to linker"
```

---

### 任务 3：新增三个 Rust 后端命令

**文件：** `src-tauri/src/commands/resource.rs`

- [ ] **步骤 1：在 `check_preset_compatibility` 后面添加 `list_ssot_resources` 命令**

```rust
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SsotResource {
    pub name: String,
    pub kind: String,
    pub enabled_tools: Vec<String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SsotResources {
    pub skills: Vec<SsotResource>,
    pub mcp: Vec<SsotResource>,
    pub plugins: Vec<SsotResource>,
}

/// 扫描 SSOT 仓库目录，返回三类资源的完整清单
#[tauri::command]
pub fn list_ssot_resources() -> SsotResources {
    let mam = dirs::home_dir().unwrap_or_default().join(".mam");
    let assignments = crate::database::list_all_assignments();

    let scan_dir = |dir: &std::path::Path, kind: &str| -> Vec<SsotResource> {
        let mut resources = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') { continue; }
                let ext_id = format!("{}-{}", kind, name);
                let enabled_tools: Vec<String> = assignments.iter()
                    .filter(|a| a.extension_id == ext_id && a.enabled)
                    .map(|a| a.agent_tool_id.clone())
                    .collect();
                resources.push(SsotResource {
                    name,
                    kind: kind.to_string(),
                    enabled_tools,
                });
            }
        }
        resources.sort_by(|a, b| a.name.cmp(&b.name));
        resources
    };

    SsotResources {
        skills: scan_dir(&mam.join("skills"), "skill"),
        mcp: scan_dir(&mam.join("mcp"), "mcp"),
        plugins: scan_dir(&mam.join("plugins"), "plugin"),
    }
}
```

- [ ] **步骤 2：添加 `detect_duplicate_skills` 命令**

```rust
/// 检测指定工具下所有在 SSOT 和原始目录中都存在的重复 skill
#[tauri::command]
pub fn detect_duplicate_skills(tool_id: String) -> Vec<String> {
    let repo = dirs::home_dir().unwrap_or_default().join(".mam").join("skills");
    let tool_skill_dir = match tool_id.as_str() {
        "claude" => Some(dirs::home_dir().unwrap_or_default().join(".claude").join("skills")),
        "codex" => Some(dirs::home_dir().unwrap_or_default().join(".agents").join("skills")),
        "opencode" => Some(dirs::home_dir().unwrap_or_default().join(".config").join("opencode").join("skills")),
        "openclaw" => Some(dirs::home_dir().unwrap_or_default().join(".openclaw").join("skills")),
        _ => None,
    };

    let tool_skill_dir = match tool_skill_dir {
        Some(d) => d,
        None => return Vec::new(),
    };

    if !repo.exists() || !tool_skill_dir.exists() {
        return Vec::new();
    }

    let mut duplicates = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&repo) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') { continue; }
            let tool_path = tool_skill_dir.join(&name);
            if tool_path.exists() && !tool_path.is_symlink() {
                duplicates.push(name);
            }
        }
    }
    duplicates.sort();
    duplicates
}
```

- [ ] **步骤 3：添加 `cleanup_duplicate_skills` 命令**

```rust
/// 清理指定工具下的重复 skill（delete 原始目录，替换为符号链接）
#[tauri::command]
pub fn cleanup_duplicate_skills(tool_id: String, names: Vec<String>) -> Result<(), String> {
    let repo = dirs::home_dir().unwrap_or_default().join(".mam").join("skills");
    let tool_skill_dir = match tool_id.as_str() {
        "claude" => dirs::home_dir().unwrap_or_default().join(".claude").join("skills"),
        "codex" => dirs::home_dir().unwrap_or_default().join(".agents").join("skills"),
        "opencode" => dirs::home_dir().unwrap_or_default().join(".config").join("opencode").join("skills"),
        "openclaw" => dirs::home_dir().unwrap_or_default().join(".openclaw").join("skills"),
        _ => return Err(format!("未知工具: {}", tool_id)),
    };

    let mut cleaned = 0;
    let mut errors = Vec::new();

    for name in &names {
        let ssot_path = repo.join(name);
        let tool_path = tool_skill_dir.join(name);

        match crate::linker::replace_with_symlink(&ssot_path, &tool_path) {
            Ok(()) => {
                let ext_id = format!("skill-{}", name);
                let _ = crate::database::upsert_assignment(&ext_id, &tool_id, true, "symlinked");
                cleaned += 1;
            }
            Err(e) => {
                log::warn!("清理 skill {} 失败: {}", name, e);
                errors.push(format!("{}: {}", name, e));
            }
        }
    }

    if !errors.is_empty() {
        Err(format!("部分清理失败 (成功 {}/{}): {}", cleaned, cleaned + errors.len(), errors.join("; ")))
    } else {
        Ok(())
    }
}
```

- [ ] **步骤 4：运行 cargo check 验证**

```bash
cd src-tauri && cargo check 2>&1
```
预期：0 errors（只有之前的 5 个已存在的 warning）

- [ ] **步骤 5：Commit**

```bash
git add src-tauri/src/commands/resource.rs
git commit -m "feat: add list_ssot_resources, detect_duplicate_skills, cleanup_duplicate_skills commands"
```

---

### 任务 4：在 lib.rs 中注册新命令

**文件：** `src-tauri/src/lib.rs:58-95`

- [ ] **步骤 1：在 `generate_handler!` 中添加 3 个新命令**

在 `commands::resource::check_preset_compatibility,` 之后添加四行，在 `commands::manifest::validate_manifest,` 之前添加：

patch for lines 67-91:
```
        commands::resource::check_preset_compatibility,
        commands::resource::list_ssot_resources,
        commands::resource::detect_duplicate_skills,
        commands::resource::cleanup_duplicate_skills,
        commands::preset::create_preset,
```

- [ ] **步骤 2：运行 cargo check 验证**

```bash
cd src-tauri && cargo check 2>&1
```
预期：0 errors

- [ ] **步骤 3：Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat: register new SSOT commands in generate_handler"
```

---

### 任务 5：添加 TypeScript 类型和新 API 函数

**文件：** `src/types/extension.ts`, `src/lib/api/resource.ts`

- [ ] **步骤 1：在 `extension.ts` 中添加新类型**

在文件末尾添加：
```typescript
export interface SsotResource {
  name: string;
  kind: string;
  enabledTools: string[];
}

export interface SsotResources {
  skills: SsotResource[];
  mcp: SsotResource[];
  plugins: SsotResource[];
}
```

- [ ] **步骤 2：在 `resource.ts` 中添加新 API 函数**

在现有 `import` 之后、`export async function` 之后添加：
```typescript
import type { ImportStats, SsotResources } from "@/types/extension";
```

在文件末尾添加三个新函数：
```typescript
export async function listSsotResources() { return await invoke<SsotResources>("list_ssot_resources"); }
export async function detectDuplicateSkills(toolId: string) { return await invoke<string[]>("detect_duplicate_skills", { toolId }); }
export async function cleanupDuplicateSkills(toolId: string, names: string[]) { return await invoke("cleanup_duplicate_skills", { toolId, names }); }
```

- [ ] **步骤 3：运行 tsc 类型检查**

```bash
npx tsc --noEmit 2>&1
```
预期：0 errors

- [ ] **步骤 4：Commit**

```bash
git add src/types/extension.ts src/lib/api/resource.ts
git commit -m "feat: add SsotResource types and SSOT API functions"
```

---

### 任务 6：创建 SSOT 仓库概览前端组件

**文件：** 新建 `src/components/resources/SsotRepoOverview.tsx`

- [ ] **步骤 1：创建组件**

```tsx
import { useEffect, useState } from "react";
import { Package, Link2, Plug } from "lucide-react";
import { listSsotResources } from "@/lib/api/resource";
import type { SsotResources } from "@/types/extension";

export function SsotRepoOverview() {
  const [resources, setResources] = useState<SsotResources | null>(null);

  useEffect(() => {
    listSsotResources().then(setResources).catch(console.error);
  }, []);

  if (!resources) return null;

  const totalSkills = resources.skills.length;
  const totalMcp = resources.mcp.length;
  const totalPlugins = resources.plugins.length;
  if (totalSkills + totalMcp + totalPlugins === 0) return null;

  return (
    <div className="rounded-lg border bg-card p-4">
      <h3 className="mb-3 text-sm font-semibold">MAM 仓库</h3>

      {/* Skills */}
      <div className="mb-3">
        <h4 className="mb-1 flex items-center gap-1.5 text-xs font-medium text-muted-foreground">
          <Package className="h-3.5 w-3.5" />
          Skills ({totalSkills})
        </h4>
        <div className="space-y-0.5">
          {resources.skills.map((s) => (
            <div key={s.name} className="flex items-center justify-between rounded bg-accent/30 px-2 py-0.5 text-xs">
              <span>{s.name}</span>
              <span className="text-muted-foreground">
                {s.enabledTools.length > 0
                  ? `已接入 ${s.enabledTools.join(", ")}`
                  : "未启用"}
              </span>
            </div>
          ))}
        </div>
      </div>

      {/* MCP */}
      {totalMcp > 0 && (
        <div className="mb-3">
          <h4 className="mb-1 flex items-center gap-1.5 text-xs font-medium text-muted-foreground">
            <Link2 className="h-3.5 w-3.5" />
            MCP ({totalMcp})
          </h4>
          <div className="space-y-0.5">
            {resources.mcp.map((m) => (
              <div key={m.name} className="flex items-center justify-between rounded bg-accent/30 px-2 py-0.5 text-xs">
                <span>{m.name}</span>
                <span className="text-muted-foreground">
                  {m.enabledTools.length > 0
                    ? `已接入 ${m.enabledTools.join(", ")}`
                    : "未启用"}
                </span>
              </div>
            ))}
          </div>
        </div>
      )}

      {/* Plugins */}
      {totalPlugins > 0 && (
        <div>
          <h4 className="mb-1 flex items-center gap-1.5 text-xs font-medium text-muted-foreground">
            <Plug className="h-3.5 w-3.5" />
            Plugins ({totalPlugins})
          </h4>
          <div className="space-y-0.5">
            {resources.plugins.map((p) => (
              <div key={p.name} className="flex items-center justify-between rounded bg-accent/30 px-2 py-0.5 text-xs">
                <span>{p.name}</span>
                <span className="text-muted-foreground">
                  {p.enabledTools.length > 0
                    ? `已接入 ${p.enabledTools.join(", ")}`
                    : "未启用"}
                </span>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
```

- [ ] **步骤 2：运行 tsc 类型检查**

```bash
npx tsc --noEmit 2>&1
```
预期：0 errors

- [ ] **步骤 3：Commit**

```bash
git add src/components/resources/SsotRepoOverview.tsx
git commit -m "feat: add SsotRepoOverview component showing SSOT warehouse contents"
```

---

### 任务 7：在 ExtensionList 中集成 SSOT 概览

**文件：** `src/components/resources/ExtensionList.tsx`

- [ ] **步骤 1：导入并插入 SsotRepoOverview**

在 `import` 区域添加：
```tsx
import { SsotRepoOverview } from "./SsotRepoOverview";
```

在 JSX 中 `<div className="space-y-4">` 下方，Toolbar 之前插入：
```tsx
      <SsotRepoOverview />
```

效果：SSOT 仓库概览在页面顶部，工具栏和第二视图在下方。

- [ ] **步骤 2：运行 tsc 类型检查**

```bash
npx tsc --noEmit 2>&1
```
预期：0 errors

- [ ] **步骤 3：Commit**

```bash
git add src/components/resources/ExtensionList.tsx
git commit -m "feat: integrate SsotRepoOverview into ExtensionList page"
```

---

### 任务 8：在按工具视图中加入"清理重复"功能

**文件：** `src/components/resources/ResourceByToolView.tsx`

- [ ] **步骤 1：添加重复检测和清理逻辑**

在组件开头添加新 `import`：
```tsx
import { detectDuplicateSkills, cleanupDuplicateSkills } from "@/lib/api/resource";
```

在 `handleImport` 函数后面添加：
```tsx
  const [duplicates, setDuplicates] = useState<Record<string, string[]>>({});

  const loadDuplicates = useCallback(async (toolId: string) => {
    try {
      const dups = await detectDuplicateSkills(toolId);
      setDuplicates((prev) => ({ ...prev, [toolId]: dups }));
    } catch (e) {
      console.error(`Failed to detect duplicates for ${toolId}:`, e);
    }
  }, []);

  // 挂载时检测所有工具的重复
  useEffect(() => {
    TOOLS.forEach((tool) => {
      loadDuplicates(tool.id);
    });
  }, [loadDuplicates]);

  const handleCleanupSingle = async (toolId: string, name: string) => {
    try {
      await cleanupDuplicateSkills(toolId, [name]);
      toast.success(`"${name}" 已清理`);
      await loadDuplicates(toolId);
      await loadToolResources(toolId);
    } catch (e) {
      toast.error(`清理失败: ${e}`);
    }
  };

  const handleCleanupAll = async (toolId: string) => {
    const dups = duplicates[toolId] || [];
    if (dups.length === 0) return;
    try {
      await cleanupDuplicateSkills(toolId, dups);
      toast.success(`已清理 ${dups.length} 个重复 skill`);
      await loadDuplicates(toolId);
      await loadToolResources(toolId);
    } catch (e) {
      toast.error(`清理失败: ${e}`);
    }
  };
```

- [ ] **步骤 2：在工具卡片中展示重复 skill 和清理按钮**

在每个工具的 `<div key={tool.id}>` 卡片中，`<ToolResourceList>` 组件之后添加重复 skill 区域：

```tsx
          {/* 重复 skill 清理区 */}
          {(duplicates[tool.id]?.length ?? 0) > 0 && (
            <div className="mt-2 rounded border border-orange-500/30 bg-orange-500/5 p-2">
              <div className="mb-1 flex items-center justify-between">
                <span className="text-xs font-medium text-orange-600">
                  ⚠ {duplicates[tool.id]!.length} 个重复 skill（SSOT 和本地同时存在）
                </span>
                <Button
                  size="sm"
                  variant="ghost"
                  className="h-5 px-1 text-[10px] text-orange-600"
                  onClick={() => handleCleanupAll(tool.id)}
                >
                  全部清理
                </Button>
              </div>
              <div className="space-y-0.5">
                {duplicates[tool.id]!.map((name) => (
                  <div key={name} className="flex items-center justify-between text-xs">
                    <span className="text-muted-foreground">{name}</span>
                    <Button
                      size="sm"
                      variant="ghost"
                      className="h-5 px-1 text-[10px]"
                      onClick={() => handleCleanupSingle(tool.id, name)}
                    >
                      清理
                    </Button>
                  </div>
                ))}
              </div>
            </div>
          )}
```

同时将 `duplicates` state 和回调函数传递给 `ToolResourceList` 时不需要改动——重复信息直接在工具卡片中展示，不在 `ToolResourceList` 内部。

- [ ] **步骤 3：运行 tsc 类型检查**

```bash
npx tsc --noEmit 2>&1
```
预期：0 errors

- [ ] **步骤 4：Commit**

```bash
git add src/components/resources/ResourceByToolView.tsx
git commit -m "feat: add duplicate skill detection and cleanup buttons in tool view"
```

---

### 任务 9：最终验证

- [ ] **步骤 1：Rust 编译验证**

```bash
cd src-tauri && cargo check 2>&1
```
预期：0 errors

- [ ] **步骤 2：TS 类型验证**

```bash
npx tsc --noEmit 2>&1
```
预期：0 errors

- [ ] **步骤 3：tracing——确认所有新命令都在 generate_handler 中**

```bash
grep -c "commands::resource::" src-tauri/src/lib.rs
```
预期：返回 >= 8（包括原有的 5 个 resource 命令 + 3 个新的）

- [ ] **步骤 4：Commit**

```bash
git add -A
git commit -m "chore: final verification of SSOT architecture refinement"
```

---

## 不在此计划中的内容

- **Phase 3 的 MCP SSOT 导入/导出** — 将作为独立计划执行，不在本计划范围内
- 子 Agent 级 Layer 3 清理 — 已由 `cleanup_layer3_on_tool_disable` 处理，无需改动
- `list_tool_resources` 的 `serde_json::json!` 动态 JSON 重构 — 不在本次范围内