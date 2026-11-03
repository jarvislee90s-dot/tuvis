# Skill 标签点击启用/取消 + 删除移至回收站 实现计划

> **面向 AI 代理的工作者：** 使用 subagent-driven-development 逐任务实现此计划。

**目标：** 在 MAM 仓库 Skill 视图中，工具标签支持点击切换启用/取消状态；取消时区分符号链接和原生目录，原生目录移至回收站而非直接删除。

**技术栈：** Tauri 2 + Rust + React 19 + TypeScript + shadcn/ui + Tailwind CSS v4

---

## 文件结构

| 文件 | 操作 | 职责 |
|------|------|------|
| `src-tauri/src/commands/resource.rs` | 修改 | 新增 `check_skill_target_type`、`disable_skill_for_tool` 命令 |
| `src-tauri/src/lib.rs` | 修改 | `generate_handler!` 注册新命令 |
| `src/lib/api/resource.ts` | 修改 | 新增 `checkSkillTargetType`、`disableSkillForTool` API |
| `src/components/resources/ResourceByKindView.tsx` | 修改 | 工具标签 onClick + 两种弹窗 |

---

### 任务 1：新增 Rust 后端命令

**文件：** `src-tauri/src/commands/resource.rs`

- [ ] **步骤 1：添加 `check_skill_target_type` 命令**

```rust
/// 检查 skill 在工具目录中的类型：symlink | native | missing
#[tauri::command]
pub fn check_skill_target_type(tool_id: String, skill_name: String) -> String {
    let tool_skill_dir = match tool_id.as_str() {
        "claude" => dirs::home_dir().unwrap_or_default().join(".claude").join("skills"),
        "codex" => dirs::home_dir().unwrap_or_default().join(".agents").join("skills"),
        "opencode" => dirs::home_dir().unwrap_or_default().join(".config").join("opencode").join("skills"),
        "openclaw" => dirs::home_dir().unwrap_or_default().join(".openclaw").join("skills"),
        _ => return "missing".to_string(),
    };
    let target = tool_skill_dir.join(&skill_name);
    if !target.exists() {
        "missing".to_string()
    } else if target.is_symlink() {
        "symlink".to_string()
    } else {
        "native".to_string()
    }
}
```

- [ ] **步骤 2：添加 `disable_skill_for_tool` 命令（移至回收站）**

```rust
/// 取消 skill 的工具配置：移至回收站 + 更新 DB
#[tauri::command]
pub fn disable_skill_for_tool(tool_id: String, skill_name: String) -> Result<String, String> {
    let tool_skill_dir = match tool_id.as_str() {
        "claude" => dirs::home_dir().unwrap_or_default().join(".claude").join("skills"),
        "codex" => dirs::home_dir().unwrap_or_default().join(".agents").join("skills"),
        "opencode" => dirs::home_dir().unwrap_or_default().join(".config").join("opencode").join("skills"),
        "openclaw" => dirs::home_dir().unwrap_or_default().join(".openclaw").join("skills"),
        _ => return Err(format!("未知工具: {}", tool_id)),
    };
    let target = tool_skill_dir.join(&skill_name);
    if !target.exists() {
        return Err("目标路径不存在".to_string());
    }

    let target_type = if target.is_symlink() { "symlink" } else { "native" };

    // 移至回收站（macOS trash 命令）
    let result = std::process::Command::new("trash")
        .arg(&target)
        .output();

    match result {
        Ok(output) if output.status.success() => {
            let ext_id = format!("skill-{}", skill_name);
            let _ = crate::database::upsert_assignment(&ext_id, &tool_id, false, "missing");
            Ok(target_type.to_string())
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!("移入回收站失败: {}", stderr))
        }
        Err(e) => {
            // trash 命令不可用时回退到直接删除
            log::warn!("trash 命令不可用，回退到直接删除: {}", e);
            crate::linker::remove_link(&target)?;
            let ext_id = format!("skill-{}", skill_name);
            let _ = crate::database::upsert_assignment(&ext_id, &tool_id, false, "missing");
            Ok(format!("{}-fallback-rm", target_type))
        }
    }
}
```

- [ ] **步骤 3：验证**

```bash
cd src-tauri && cargo check 2>&1
```

- [ ] **步骤 4：Commit**

---

### 任务 2：在 lib.rs 中注册新命令

**文件：** `src-tauri/src/lib.rs`

- [ ] **步骤 1：添加注册**

在 `commands::resource::check_preset_compatibility,` 之后、`commands::preset::create_preset,` 之前添加：
```rust
        commands::resource::check_skill_target_type,
        commands::resource::disable_skill_for_tool,
```

- [ ] **步骤 2：验证 + Commit**

---

### 任务 3：添加 TypeScript API 函数

**文件：** `src/lib/api/resource.ts`

- [ ] **步骤 1：添加两个 API 函数**

```typescript
export async function checkSkillTargetType(toolId: string, skillName: string) { return await invoke<string>("check_skill_target_type", { toolId, skillName }); }
export async function disableSkillForTool(toolId: string, skillName: string) { return await invoke<string>("disable_skill_for_tool", { toolId, skillName }); }
```

- [ ] **步骤 2：验证 tsc + Commit**

---

### 任务 4：前端点击交互 + 弹窗

**文件：** `src/components/resources/ResourceByKindView.tsx`

- [ ] **步骤 1：启用 skill（灰 → 亮）**

灰色标签点击 → 调用 `invoke("toggle_skill_for_tool", ...)` 或复用现有的 `toggle_skill_for_tool` 命令（`commands/skill.rs` 中的 `enable_skill_for_tool`）。

需要检查是否有现成的启用命令——`services/skill/mod.rs` 中有 `enable_skill_for_tool`，但可能需要一个 Tauri command 包装。

如果没有，新增启用逻辑或复用 `command::skill::install_skill`？实际上最简单的做法是新增一个统一的 toggle 命令。

**步骤 1a：如果 `enable_skill_for_tool` 已在 commands 中暴露** → 直接用
**步骤 1b：如果没有** → 新增 `toggle_skill_for_tool` 命令（或在前端直接调用 `invoke("install_skill", ...)`）

检查后发现 `commands/skill::install_skill` 存在但参数是 `(name: String, tool_id: String)`。可以复用。

- [ ] **步骤 2：取消 skill（亮 → 灰）**

亮起标签点击 → 先调用 `checkSkillTargetType` 确定类型 → 展示对应弹窗 → 确认后调用 `disableSkillForTool` → 刷新数据

**symlink 弹窗：**
```
标题：移除链接
内容：确定要移除 "{skill}" 在 {tool} 中的符号链接吗？
按钮：取消 | 移除链接
```

**native 弹窗（红色警告）：**
```
标题：⚠️ 删除原生 skill
内容（红色）：此操作将删除你手动安装的 "{skill}" 目录，文件将移至回收站。
              {tool} 将不再加载此 skill。你可以从回收站恢复。
按钮：取消 | 移至回收站并移除
```

- [ ] **步骤 3：验证 tsc + Commit**

---

### 任务 5：最终验证

- [ ] cargo check
- [ ] tsc --noEmit
- [ ] 确认所有命令在 generate_handler! 中
