# 预设组 v2 · M2+M3 完结 实施计划（前端 + 对账体系 + 托盘/frontmatter）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 完成 `docs/engineering/specs/2026-09-14-preset-groups-v2-design.md` 的全部剩余内容（28 项缺口表 A-E 组）：预设区 v2 UI（双分区/滑块开关/编辑弹窗/确认弹窗 v2）、资源卡片专属/常驻、账本-磁盘对账体系（§13）、托盘接线、frontmatter 预填、用户手册——做完后用户可完成全部手工测试。

**Architecture:** 后端补少量命令（对账扫描/处置、激活状态批量查询、frontmatter 解析、托盘刷新），前端在既有 React Query + radix Dialog + sonner 惯例上重写 PresetList 与确认弹窗、新增编辑弹窗/体检卡片/资源卡片增强。M1 后端语义已就绪，本计划只消费不改动。

**Tech Stack:** React 19 + TS + Tailwind v4 + radix(shadcn 风格 ui/dialog、ui/switch) + React Query + sonner + i18next；Rust/Tauri 2；vitest + @testing-library/react。

**Spec:** `docs/engineering/specs/2026-09-14-preset-groups-v2-design.md`（§5.5 开关 / §6 专属 / §7 UI / §13 对账；裁决见 §12 表末四行）。**任务的功能细节以 spec 为准，计划不重复抄写；每任务的「验收条件」是完成的硬标准。**

## Global Constraints

- **禁止 push**：所有提交留在本地分支；未经用户明确指示不得 push、不得改云端 PR
- 注释中文、标识符英文；commit message 可英文；i18n 新 key 必须 zh/en **成对**添加（无 parity 测试，每任务验收含 node 对比脚本）
- 前端惯例（已核实）：queryKey 用导出的 const 元组；toast 用 `import { toast } from "sonner"` + `formatInvokeError(e, t)`；弹窗用 `@/components/ui/dialog`（无 Select/Textarea 原语，用原生 `<textarea>/<select>`，参照 ResourceByKindView）；组件测试用 `vi.hoisted` + `vi.mock("@tauri-apps/api/core")` 惯例（参照 `tests/legacySkillMigrationDialog.test.tsx`）
- 后端惯例：命令 `#[tauri::command]` + serde camelCase，注册进 `lib.rs` invoke_handler；测试资源名 `v2m2-` 前缀；集成断言 `contains`
- **托盘坑（必读）**：菜单 id 是 `preset-{preset.id}` 而 `preset.id` 本身以 `preset-` 开头 → 实际双前缀 `preset-preset-...`，`strip_prefix("preset-")` 恰好一次；`update_tray_menu`（语言/宠物路径）会**抹掉**预设菜单项——接线时收敛为统一重建入口
- 复用优先：能力门控照抄 `ResourceByKindView.tsx:152-158` 的 `kindSupported` 写法（不抽公共模块，避免顺手重构）；对账处置复用 `enable/disable_skill_for_tool` 既有服务与守卫；处置/开关后失效既有 queryKey（`PRESETS_KEY`/`ACTIVE_PRESETS_KEY`/`SSOT_RESOURCES_KEY`），不自造刷新机制
- 每任务收尾门禁：涉 Rust `cd src-tauri && cargo test`；涉前端 `pnpm lint && pnpm build`；全过才 commit（用计划给定 message）并勾计划文件对应 `- [x]`
- M1 已实现命令（消费不重造）：`get_preset/update_preset/restore_preset/get_active_preset/preview_apply_preset/set_resource_binding/list_resource_bindings/delete_resource_binding/set_tool_resident/list_tool_residents`（`commands/preset.rs`）；`create_preset` 已收 `description/scope/boundTool` 可选参数

---

## Phase 0 · Rust 前置

### Task 1: MCP / plugin 的 sweep→restore 端到端测试（裁决 2 开工前置）

**Files:**
- Test: `src-tauri/tests/preset_v2_test.rs`（追加）

**Interfaces:** Consumes M1 服务；Produces 无新接口——纯回归锁。若测试暴露 M1 真缺陷 → **停下报告**，不自行改 services。

**验收条件：**
- [x] `cargo test mcp_sweep_restore && cargo test v2m2_plug` 绿：MCP 配置段往返（清扫后 assignment disabled 且配置段移除 → 恢复后重写且 enabled）+ file 型插件链接往返（清扫断链 → 恢复链接回）
- [x] config 型 plugin 覆盖与否及原因写入 commit message（工具配置格式无法构造时允许跳过，file 型为必测）
- [x] 全量 `cargo test` 绿

- [x] **Step 1: MCP 测试**（`v2m2-mcp-a` 经 `toggle_mcp` 启用为基底 → 空预设 apply → 断言 `r.disabled` 含 `mcp-v2m2-mcp-a` 且 assignment disabled → `restore_tool` → 断言 `rr.restored_mam` 含之且 assignment enabled）
- [x] **Step 2: plugin 测试**（`~/.mam/plugins/v2m2-plug-a/` + tags=Some("file") 注册 → `toggle_plugin` 启用 → apply → 断言 disabled + 工具插件目录项消失 → restore → 链接回来 + enabled）
- [x] **Step 3: Commit** `test(preset-v2): MCP/plugin sweep-restore roundtrips (M2 precondition, ruling #2)`

## Phase A · 前端基建

### Task 2: 类型 / API 封装 / Query / Mock / 小命令 / 死代码

**Files:**
- Modify: `src/types/preset.ts`（全文替换）、`src/types/extension.ts`（追加绑定类型）
- Modify: `src/lib/api/preset.ts`、`src/lib/query/queries/presets.ts`（全文替换）
- Create: `src/lib/query/queries/bindings.ts`
- Modify: `src-tauri/src/commands/preset.rs` + `lib.rs`（两个小命令）
- Delete: `src/lib/schemas/preset.ts`
- Modify: `src/tauri-mock.ts`、`tests/msw/tauriMocks.ts`

**Interfaces:**

```ts
// src/types/preset.ts（全文替换）
export interface PresetItem { extensionId: string; kind: string; extensionName: string; }
export interface PresetRecord { id: string; name: string; description: string; scope: "universal" | "tool"; boundTool: string | null; items: PresetItem[]; }
export interface PresetApplyResult { successCount: number; failures: string[]; conflicts: string[]; stashed: string[]; disabled: string[]; restoredNative: string[]; }
export interface RestoreResult { restoredMam: string[]; restoredNative: string[]; conflicts: string[]; }
export interface ApplyPreview { toEnable: string[]; filtered: string[]; toDisable: string[]; toStash: string[]; residentExempt: string[]; }
```

```ts
// src/types/extension.ts 追加
export interface ResourceBinding { extensionId: string; exclusiveTools: string; reason: string | null; updatedAt: string; }
export interface ActivePreset { toolId: string; presetId: string; }
```

```ts
// src/lib/api/preset.ts 追加/修改（保留现有函数名，扩展签名；以下为新增/变更全集）
createPreset(name, items: [string,string][], description?: string, scope?: string, boundTool?: string): Promise<string>
getPreset(presetId): Promise<PresetRecord | null>
updatePreset(id, name, description, scope, boundTool: string | null, items: [string,string][]): Promise<void>
restorePreset(toolId): Promise<RestoreResult>
getActivePreset(toolId): Promise<string | null>
listActivePresets(): Promise<ActivePreset[]>                      // 新 Rust 命令
getToolActiveResources(toolId): Promise<[string,string,string][]> // 新 Rust 命令 (extId, kind, origin)
previewApplyPreset(presetId, toolId): Promise<ApplyPreview>
setResourceBinding(extensionId, exclusiveTools: string[], reason?: string): Promise<void>
listResourceBindings(): Promise<ResourceBinding[]>
deleteResourceBinding(extensionId): Promise<void>
setToolResident(toolId, extensionId, resident: boolean): Promise<void>
listToolResidents(toolId): Promise<string[]>
```

```rust
// commands/preset.rs 追加（lib.rs 注册）
/// 全工具激活预设（开关状态批量数据源，避免前端 N 次 invoke）
#[tauri::command]
pub fn list_active_presets() -> Vec<serde_json::Value> {
    crate::adapter::TOOL_IDS
        .iter()
        .filter_map(|tool| {
            crate::database::get_base_snapshot(tool)
                .and_then(|(active, _)| active)
                .map(|p| serde_json::json!({ "toolId": tool, "presetId": p }))
        })
        .collect()
}

/// 工具当前生效资源集合（FR-24「存为预设」预填数据源）
#[tauri::command]
pub fn get_tool_active_resources(tool_id: String) -> Vec<(String, String, String)> {
    crate::services::preset::snapshot::scan_tool_state(&tool_id)
        .into_iter()
        .map(|i| (i.extension_id, i.kind, i.origin))
        .collect()
}
```

```ts
// queries/presets.ts（全文替换）+ queries/bindings.ts（新建）
export const PRESETS_KEY = ["presets"] as const;
export const ACTIVE_PRESETS_KEY = ["active-presets"] as const;
export const usePresetsQuery = () => useQuery({ queryKey: PRESETS_KEY, queryFn: listPresets, staleTime: 5000 });
export const useActivePresetsQuery = () => useQuery({ queryKey: ACTIVE_PRESETS_KEY, queryFn: listActivePresets, staleTime: 5000 });
export const BINDINGS_KEY = ["resource-bindings"] as const;
export const useResourceBindingsQuery = () => useQuery({ queryKey: BINDINGS_KEY, queryFn: listResourceBindings, staleTime: 10000 });
```

**验收条件：**
- [x] `cargo test` 绿（两命令编译注册）；`pnpm lint && pnpm build` 绿
- [x] `grep -rn "schemas/preset" src/ tests/` 零结果（死代码已删）
- [x] `src/tauri-mock.ts` 的 `list_presets` 返回 v2 形状（含 description/scope/boundTool，1 通用 + 1 tool 私有样例）；`tests/msw/tauriMocks.ts` 的 `mockPresets` 同步 v2；新读命令有 fixture case、写命令入 `:117-123` 分组
- [x] `check_preset_compatibility` mock 修正为 `{id,name,kind}` 形状

- [x] **Step 1: Rust 两命令 + 注册 + cargo test**
- [x] **Step 2: 类型/api/query/删死代码**
- [x] **Step 3: 双 mock 更新**
- [x] **Step 4: 门禁 + Commit** `feat(preset-v2): frontend plumbing — types/api/queries/mocks + list_active_presets & get_tool_active_resources commands`

### Task 3: i18n 全量 key（zh/en 成对）

**Files:** `src/i18n/locales/zh.json`、`en.json`

**Interfaces:** Produces（后续任务消费，嵌套组惯例参照 `resources.migration.*`）：
- `presets` 扩展：`universalSection`、`toolScopedSection`、`description`、`descriptionPlaceholder`、`edit`、`editTitle`、`type`、`typeUniversal`、`typeTool`、`boundTool`、`addItem`、`removeItem`、`nativeSkills`、`exclusiveBadge`、`residentBadge`、`switchToToolScopedHint`、`activeOn`、`saveFailedActive`、`cannotDeleteActive`、`whiteboardWarning`、`applyResult`、`restoreDone`、`restoreConflicts`、`filteredCount`、`toStashCount`、`toDisableCount`、`residentExemptCount`、`saveAsPreset`
- `resources` 扩展：`binding.*`（editTitle/tools/reason/reasonPlaceholder/save/clear）、`resident.toggle`、`health.*`（title/runNow/ok/L1/L2/L3/L4/fixA/fixB/fixC/needsManual/batchA/batchB/invariantBroken/stashPending/suggestion）
- `tray` 扩展：`presetsLabel`、`on`、`off`

**验收条件：**
- [x] 一次性 node 脚本比对 zh/en 叶子路径集合：0 差异（命令贴进报告，不留测试文件）
- [x] `pnpm lint` 绿

- [x] **Step 1: zh 写入** → **Step 2: en 成对** → **Step 3: parity 自检** → **Step 4: Commit** `chore(preset-v2): i18n keys zh/en for M2+M3`

## Phase B · 预设区 v2

### Task 4: PresetList v2——双分区 + 预设卡片 + 数据层迁移

**Files:**
- Rewrite: `src/components/presets/PresetList.tsx`
- Unchanged: `src/components/resources/ExtensionList.tsx`（继续传 `extensions` prop——T7 编辑弹窗消费）

**Interfaces:** Consumes Task 2/3。Produces `PresetList`（默认导出，无 props）：
- 标题行 + 「+ 创建」（开编辑弹窗空表单，T7 后接线；本任务先留 handler 占位 TODO 不实现——验收不含此项）
- 通用区（scope==="universal"）/ 私有区（按 boundTool 分组，组头 = 工具名）
- 预设卡片：名称 + 描述摘要（1 行截断）+ items 计数徽标 + 删除按钮（带确认 Dialog；catch `PRESET_ACTIVE` 错误 → toast `presets.cannotDeleteActive`）；整卡可点 → onEdit（T7 接线）
- `SubAgentPresetActions` 原样保留迁移
- 数据层迁 `usePresetsQuery`；变更后 `qc.invalidateQueries({queryKey: PRESETS_KEY})`

**验收条件：**
- [x] `pnpm lint && pnpm build` 绿
- [x] dev 目验（或并入 T9 断言）：mock 数据下两分区标题渲染、私有预设出现在绑定工具组、描述摘要显示、React Query 生效（同数据不重复 invoke）

- [x] **Step 1: 骨架实现** → **Step 2: 门禁** → **Step 3: Commit** `feat(preset-v2): PresetList v2 dual-zone skeleton on React Query`

### Task 5: 应用确认弹窗 v2（preview 数据 + 白板强警示）

**Files:**
- Create: `src/components/presets/ApplyConfirmDialog.tsx`
- Delete: `src/components/resources/CompatibilityDialog.tsx`（确认无其他引用后）
- Modify: `src/lib/api/resource.ts`（删 `checkPresetCompatibility`——Rust 命令保留不删）

**Interfaces:** `ApplyConfirmDialog({open, presetId, toolId, toolName, onClose, onConfirm})`，`open` 时 useEffect 拉 `previewApplyPreset`（loading 态照旧 CompatibilityDialog 模式）。渲染（spec §7.3 + 2026-09-15 白板裁决）：
- 五段清单（空段隐藏）：将启用 `toEnable` / 被过滤 `filtered`（逐条原因）/ 将停用 `toDisable` / 将暂存 `toStash`（点名）/ 常驻豁免 `residentExempt`（计数）
- 白板：`toEnable.length===0 && (toDisable.length||toStash.length)` → 顶部警示块 `presets.whiteboardWarning`，确认按钮 destructive variant（允许继续）
- 确认按钮 loading 态；取消恒可用

**验收条件：**
- [x] `pnpm lint && pnpm build` 绿；`grep -rn "CompatibilityDialog" src/` 零结果
- [x] T9 用例锁：正常 preview 五段渲染；白板 preview 警示出现且按钮 destructive

- [x] **Step 1: 实现** → **Step 2: 删旧** → **Step 3: 门禁 + Commit** `feat(preset-v2): apply confirm dialog v2 — preview-driven with whiteboard warning`

### Task 6: 滑块开关——开=确认弹窗、关=直接恢复 + toast

**Files:**
- Modify: `src/components/presets/PresetList.tsx`（消费 Task 5 的 ApplyConfirmDialog）

**Interfaces:** 开关契约（spec §5.5）：

```tsx
const onSwitch = async (presetId: string, toolId: string, next: boolean) => {
  if (next) { setConfirm({presetId, toolId}); return; }   // 开 → T5 弹窗
  const rr = await restorePreset(toolId);                  // 关 → 直接执行
  toast.success(t("presets.restoreDone", { m: rr.restoredMam.length, n: rr.restoredNative.length }));
  if (rr.conflicts.length) toast.warning(t("presets.restoreConflicts"), { description: rr.conflicts.join("\n") });
  qc.invalidateQueries({ queryKey: ACTIVE_PRESETS_KEY });
};
// 确认 onConfirm → applyPreset → toast t("presets.applyResult", {n,d,s})（PresetApplyResult 三计数）
// checked = activePresets.find(a => a.toolId===tool.id)?.presetId === preset.id（useActivePresetsQuery）
// 同工具开新预设：apply 后 invalidate，旧开关自动翻关
// 失败 catch → toast.error(t("presets.applyFailed", {error: formatInvokeError(e, t)}))
```

能力门控：`kindSupported`（照抄 ResourceByKindView 写法）——预设 items 含某类资源而工具不支持 → 该工具 Switch `disabled` + title；私有预设绑定工具未启用 → 不渲染开关。

**验收条件：**
- [x] `pnpm lint && pnpm build` 绿
- [x] 开→弹窗→确认→apply→toast→开关亮；关→直接恢复→toast→开关灭（dev 目验，T9 断言 checked 逻辑与门控 disabled）
- [x] dev 目验：同工具开 B 后 A 开关自动翻关

- [x] **Step 1: 实现** → **Step 2: 门禁 + Commit** `feat(preset-v2): preset×tool switch — on=confirm dialog, off=direct restore+toast`

### Task 7: 预设编辑弹窗（新建/编辑共用）

**Files:**
- Create: `src/components/presets/PresetEditDialog.tsx`
- Modify: `src/components/presets/PresetList.tsx`（卡片点击/创建按钮接线）
- Modify: `src-tauri/src/commands/resource.rs`（`ExtensionWithAssignments` 结构体与映射补 `is_native` 一行）+ `src/types/extension.ts`（`isNative: boolean`）

**Interfaces:** `PresetEditDialog({open, preset: PresetRecord | null, presetExtensions?: ExtensionWithAssignments[] /*经 props 从 ExtensionList 链传入*/, onClose, prefill?: {toolId, items}})`。表单（spec §7.2 全量细节以 spec 为准）：
- 类型 Radio + boundTool 原生 `<select>`（仅启用工具，typeTool 必选）；名称必填；描述 `<textarea>`
- 三组复选（extensions props 数据源）：通用=仅非 isNative + 专属徽标（不禁选）；私有=不适配 disabled+原因 title + 追加原生组（`isNative && sourceTool===boundTool`）
- 转类型建议：通用模式勾选专属资源 → 提示条 + 「转工具私有」快捷
- 保存：新建 `createPreset` / 编辑 `updatePreset`；catch 激活守卫 → toast `presets.saveFailedActive`；≥1 项校验

**验收条件：**
- [x] `cargo test` 绿（is_native 透出）+ `pnpm lint && pnpm build` 绿
- [x] dev 目验：编辑已有预设改名改描述保存 → 列表反映；私有模式不适配项 disabled、原生组仅显绑定工具的；通用勾专属出现转类型提示；激活中编辑被拒并 toast
- [x] T9 用例锁：空名不触发 create、create args 含 meta、私有不适配 disabled

- [x] **Step 1: Rust 透出 is_native** → **Step 2: 弹窗实现** → **Step 3: 接线 + 门禁** → **Step 4: Commit** `feat(preset-v2): preset edit dialog — type/description/suite list with native skills & binding-aware gating`

### Task 8: FR-24 存为预设 + 收尾清理

**Files:**
- Modify: `src/components/resources/ResourceByToolView.tsx`（工具组头部；若布局不适则放 `ResourceByKindView`，实现者二选一记录 progress）
- Modify: `src/components/presets/PresetList.tsx`、`src/lib/api/preset.ts`、`src/tauri-mock.ts`、`tests/msw/tauriMocks.ts`（清 `deactivatePreset` 前端路径；**Rust 命令保留**）

**Interfaces:** 工具组头部 `presets.saveAsPreset` 按钮 → `getToolActiveResources(toolId)` → 打开 `PresetEditDialog`（preset=null、prefill={toolId, items}，scope 预选 tool）。

**验收条件：**
- [x] `pnpm lint && pnpm build` 绿；`grep -rn "deactivatePreset\|deactivate_preset" src/` 零结果
- [x] dev 目验：按钮 → 弹窗预填正确 → 保存后列表出现新预设

- [x] **Step 1: 实现 + 清理** → **Step 2: 门禁 + Commit** `feat(preset-v2): save-active-combo-as-preset entry (FR-24) + retire frontend deactivate path`

### Task 9: 预设前端组件测试

**Files:** Create `tests/presetList.test.tsx`、`tests/presetEditDialog.test.tsx`、`tests/applyConfirmDialog.test.tsx`

**Interfaces:** 照 `tests/legacySkillMigrationDialog.test.tsx` 惯例（vi.hoisted + mock core + i18n zh）。

**验收条件：**
- [x] `npx vitest run` 全绿；每文件 ≥3 用例：
  1. presetList：双分区渲染 + 私有归属组 + activePresets 驱动 checked（on 工具/off 工具各断言）
  2. presetEditDialog：空名不 invoke / create args 含 meta / 私有不适配 disabled
  3. applyConfirmDialog：五段清单渲染 / 白板警示 + destructive 按钮

- [x] **Step 1: 写测试** → **Step 2: 全绿 + lint** → **Step 3: Commit** `test(preset-v2): frontend component tests — dual zone, edit gating, whiteboard warning`

## Phase C · 资源卡片增强

### Task 10: 专属徽标 + 编辑弹层 + 不适配置灰

**Files:** Modify `src/components/resources/ResourceByKindView.tsx`（skill/mcp/plugin 三行渲染处）

**Interfaces:** Consumes `useResourceBindingsQuery`/`setResourceBinding`/`deleteResourceBinding`。Produces：资源名旁徽标（binding 且 exclusiveTools 非空）→ 点击小 Dialog（允许工具复选 + 原因 textarea + 保存/清除）；行内对不适配工具启停按钮 `disabled`（`kindSupported` 同款 title 模式）。

**验收条件：**
- [x] `pnpm lint && pnpm build` 绿
- [ ] dev 目验：设绑定→徽标出现→对不适配工具按钮置灰；弹层保存→`list_resource_bindings` 反映；清除→徽标消失

- [x] **Step 1: 实现** → **Step 2: 门禁 + Commit** `feat(preset-v2): exclusive badge + binding editor + per-tool gating on resource cards`

### Task 11: 常驻锁开关

**Files:** Modify `src/components/resources/ResourceByKindView.tsx`（三行渲染处，紧邻启停按钮）

**Interfaces:** Consumes `setToolResident/listToolResidents`（新 query `["tool-residents", toolId]` 写入 bindings.ts）。Produces：每资源行 × 每工具一枚锁开关（title=`resources.resident.toggle`），on 时加 `presets.residentBadge` 徽标。

**验收条件：**
- [x] `pnpm lint && pnpm build` 绿
- [ ] dev 目验：开关持久化（刷新仍 on）；invoke args 正确（toolId/extensionId/resident）
- [x] Rust 侧已有常驻豁免测试护航，前端仅传参

- [x] **Step 1: 实现** → **Step 2: 门禁 + Commit** `feat(preset-v2): per-tool resident lock switch on resource cards`

## Phase D · 账本-磁盘对账（spec §13）

### Task 12: reconcile 扫描引擎 + scan 命令

**Files:**
- Create: `src-tauri/src/services/resource/reconcile.rs`（mod.rs 加 `pub mod reconcile;`）
- Modify: `src-tauri/src/commands/resource.rs` + `lib.rs`
- Test: Create `src-tauri/tests/reconcile_test.rs`（`mod support;`）

**Interfaces:**

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriftItem {
    pub tool_id: String,
    pub kind: String,          // "L1"|"L2"|"L3"|"L4"
    pub extension_id: String,
    pub path: String,
}

/// 扫描 enabled 工具的账本-磁盘漂移（§13）：disabled 工具排除（W5 名册语义）；
/// 只看 skill（mcp/plugin 后续版本）；L4 外链只报告不接管
pub fn scan_drift() -> Vec<DriftItem> {
    let home = dirs::home_dir().unwrap_or_default();
    let mam_root = home.join(".mam");
    let mut out = Vec::new();
    for tool in crate::adapter::TOOL_IDS {
        if !crate::database::get_tool_enabled(tool) {
            continue; // W5 停用 = 名册保留语义，不算漂移
        }
        let ledger: Vec<String> = crate::database::list_assignments(tool)
            .into_iter()
            .filter(|a| a.enabled && a.sub_agent_id.is_none() && a.extension_id.starts_with("skill-"))
            .map(|a| a.extension_id)
            .collect();
        let Some(dir) = crate::adapter::primary_skill_dir(tool) else { continue };
        let mut disk_mam_links: Vec<String> = Vec::new();
        if dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for e in entries.flatten() {
                    let path = e.path();
                    let Some(name) = e.file_name().to_str() else { continue };
                    if name == "subagents" { continue; }
                    let ext_id = format!("skill-{}", name);
                    if path.is_symlink() {
                        let target = std::fs::read_link(&path).unwrap_or_default();
                        if target.starts_with(&mam_root) {
                            disk_mam_links.push(ext_id);
                        } else {
                            out.push(DriftItem { tool_id: tool.to_string(), kind: "L4".into(), extension_id: ext_id, path: path.to_string_lossy().to_string() });
                        }
                    } else if path.is_dir() && ledger.iter().any(|l| l == &ext_id) {
                        out.push(DriftItem { tool_id: tool.to_string(), kind: "L2".into(), extension_id: ext_id, path: path.to_string_lossy().to_string() });
                    }
                }
            }
        }
        for ext_id in &ledger {
            if !disk_mam_links.contains(ext_id) {
                let name = ext_id.strip_prefix("skill-").unwrap_or(ext_id);
                if !dir.join(name).exists() {
                    out.push(DriftItem { tool_id: tool.to_string(), kind: "L1".into(), extension_id: ext_id.clone(), path: dir.join(name).to_string_lossy().to_string() });
                }
            }
        }
        for ext_id in &disk_mam_links {
            if !ledger.contains(ext_id) {
                let name = ext_id.strip_prefix("skill-").unwrap_or(ext_id);
                out.push(DriftItem { tool_id: tool.to_string(), kind: "L3".into(), extension_id: ext_id.clone(), path: dir.join(name).to_string_lossy().to_string() });
            }
        }
    }
    out
}
```

命令：`#[tauri::command] pub fn scan_ledger_drift() -> Vec<DriftItem>`（commands/resource.rs + 注册）。

**验收条件：**
- [x] `cargo test reconcile` 绿：L1/L2/L3/L4 四类各命中对应 extension_id；disabled 工具的同类漂移**不出现**
- [x] 命令注册、全量 `cargo test` 绿

- [x] **Step 1: 测试先写（`v2m2-rc-*`）** → **Step 2: 失败→实现→绿** → **Step 3: 注册 + Commit** `feat(preset-v2): ledger-disk drift scan (L1-L4) + scan_ledger_drift command`

### Task 13: 对账处置命令（逐条 a/b + 批量）

**Files:** Modify `reconcile.rs`、`commands/resource.rs` + `lib.rs`；Test `reconcile_test.rs`

**Interfaces:**

```rust
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileOutcome { pub fixed: bool, pub needs_manual: bool, pub message: String }

/// 单条处置（§13）。mode: "a" 账本为准修磁盘 | "b" 磁盘为准回写账本
pub fn reconcile_one(item: &DriftItem, mode: &str) -> ReconcileOutcome {
    let name = item.extension_id.strip_prefix("skill-").unwrap_or(&item.extension_id);
    match (item.kind.as_str(), mode) {
        ("L1", "a") => enable_skill_for_tool(name, &item.tool_id) → fixed / 失败 message,
        ("L1", "b") => upsert_assignment(ext_id, tool, false, "missing") → fixed,
        ("L2", "a") => enable_skill_for_tool（守卫：内容一致才替换；Err 含「不一致」→ needs_manual=true 保留现场）→ fixed,
        ("L2", "b") => upsert_assignment(..., false, "missing")（真目录保留原生态）→ fixed,
        ("L3", "a") => disable_skill_for_tool(name, tool)（清链含 Layer2 级联）→ fixed,
        ("L3", "b") => upsert_assignment(ext_id, tool, true, "valid") → fixed,
        ("L4", _)   => needs_manual=true（外链 MAM 不接管）,
        _ => message "未知组合",
    }
}
#[tauri::command] pub fn reconcile_item(item: DriftItem, mode: String) -> ReconcileOutcome
#[tauri::command] pub fn reconcile_tool_batch(tool_id: String, mode: String) -> Vec<ReconcileOutcome> // 该工具全部 L1-L3 同 mode；L4 恒 needs_manual
```

**验收条件：**
- [x] `cargo test reconcile` 绿，五组断言：L1-a 链接重建 / L2-b 账本 disabled 且真目录原样 / **L2-a 内容不一致 → needs_manual 且真目录仍在（防删回归锁）** / L3-a 清链 / L4 任意 mode → needs_manual
- [x] 命令注册、全量绿

- [x] **Step 1: 测试先写** → **Step 2: 实现→绿** → **Step 3: Commit** `feat(preset-v2): reconcile actions — per-item & batch a/b with needs-manual escalation`

### Task 14: preset health 聚合命令 + 启动接线

**Files:** Modify `src-tauri/src/services/preset/mod.rs`、`commands/preset.rs` + `lib.rs`、`lib.rs` 启动线程

**Interfaces:**

```rust
#[derive(Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetHealth {
    pub invariants: Vec<String>,                                // check_snapshot_invariants()
    pub stash_pending: Vec<crate::database::StashEntryRecord>,  // unrestored_stash(None)
    pub drift: Vec<crate::services::resource::reconcile::DriftItem>,
}
pub fn preset_health() -> PresetHealth { /* 三源聚合 */ }
#[tauri::command] pub fn get_preset_health() -> PresetHealth
```

启动线程在 backfill 后加：`for d in reconcile::scan_drift() { log::warn!("[漂移{}] {} {}", d.kind, d.extension_id, d.path); }`

**验收条件：**
- [x] `cargo test` 绿（preset_health 聚合单测：三源各造一条断言齐出）
- [x] 命令注册；本地 dev 启动日志可见漂移 warn（构造一个 L1 后目验，可选）

- [x] **Step 1: 实现+测试** → **Step 2: 启动接线** → **Step 3: Commit** `feat(preset-v2): preset health aggregation command + startup drift logging`

### Task 15: 一致性体检卡片 + 设置页「立即体检」

**Files:**
- Create: `src/components/resources/HealthCheckCard.tsx`
- Modify: `src/components/resources/ExtensionList.tsx`（资源 tab 顶部挂载 + 角标）、`src/pages/settings.tsx`（`SettingSection` 加 `"health"` + menuItems + 区块）
- Modify: `src/lib/api/`（`scanLedgerDrift/reconcileItem/reconcileToolBatch/getPresetHealth` 封装）、双 mock

**Interfaces:** `HealthCheckCard`（query `["preset-health"]` → `getPresetHealth`）：
- 无异常：折叠一行 `resources.health.ok` + 「立即体检」（refetch）；有异常展开三段：
  ① 漂移明细（按 toolId 分组表格：分类徽标/资源/路径/操作）；逐行 a/b/c 三钮（c=收起该行）；`needsManual` → 行转橙色无按钮；组头批量 batchA/batchB；处置后 invalidate `["preset-health"]` + PRESETS/ACTIVE/SSOT 相关 key
  ② 不变量违背：逐条 + 「一键修复」= `restorePreset(工具)`（工具 disabled 时提示先启用）
  ③ 残留暂存：列条目 + 「回移」——实现者二选一并记录 progress：(i) 新命令 `restore_stash_entry(id)`（账本查 entry → `stash::restore_stashed_skill`）；(ii) 提示重启自动对账。推荐 (i)
- 设置页 health 区块：同数据只读摘要 + `resources.health.runNow`

**验收条件：**
- [x] `cargo test`（若选 i）+ `pnpm lint && pnpm build` 绿
- [ ] dev 目验：构造 L1+L3 → 卡片分组展示；逐条 a 修复后刷新消失；L2 不一致 → 需人工态无按钮；设置页区块可用；无异常时折叠一行

- [x] **Step 1: Rust（若选 i）** → **Step 2: 前端 + mock** → **Step 3: 门禁 + Commit** `feat(preset-v2): health check card — drift triage a/b/c with batch, invariant & stash panels`

## Phase E · M3

### Task 16: 托盘接线（选中态 + 直接执行 + 刷新收敛）

**Files:**
- Modify: `src-tauri/src/plugins/system_tray.rs`、`src-tauri/src/commands/session.rs`、`src-tauri/src/lib.rs`
- Modify: `src/pages/home.tsx`、`src/components/common/language-toggle.tsx`（改调 `refresh_tray`）
- Test: system_tray 纯函数单测（模块内 `#[cfg(test)]`）

**Interfaces:**
1. `on_menu_event` 加 preset arm（:74-101 的 `_` 之前）：点击=**直接执行**（裁决 2026-09-15）。id 双前缀：`strip_prefix("preset-")` 恰好一次得 preset_id。universal 预设按 §7.5 子菜单展开（子项 id `preset-tool-{preset_id}|{tool_id}`，点击=该工具翻转）；私有 = 顶层 CheckMenuItem 点击翻转绑定工具
2. 翻转判定抽纯函数 + 单测：`pub fn preset_toggle_action(active: Option<&str>, preset_id: &str) -> &'static str`（active==preset → "restore" else "apply"）
3. `update_tray_with_presets`：CheckMenuItem（checked = `get_active_preset(tool)==preset_id`；若 Tauri 2 CheckMenuItem 组装受阻 → 退化普通 MenuItem + `(开)/(关)` 后缀，记录 progress）；label 参数化去硬编码中文
4. 统一重建：新命令 `refresh_tray(app, presets_label: String)` = 基础项 + 预设项合并重建；`home.tsx`/`language-toggle.tsx`/预设变更处统一改调它（`update_tray_menu` 命令保留但前端不再用）
5. 刷新触发：session.rs 计数启发式保留兜底；前端在 PRESETS/ACTIVE invalidate 处追加 `invoke("refresh_tray", {presetsLabel: t("tray.presetsLabel")})`

**验收条件：**
- [x] `preset_toggle_action` 单测绿；`cargo test` + `pnpm lint && pnpm build` 绿
- [ ] dev 目验：托盘显示预设项与选中态；点击私有预设直接翻转（开→关无需确认）；语言切换后预设项仍在；增删预设/开关变化后托盘刷新

- [x] **Step 1: Rust 改造+单测** → **Step 2: 前端调用替换** → **Step 3: 门禁 + Commit** `feat(preset-v2): tray wiring — check state, direct toggle, unified rebuild, i18n label`
- [x] **Step 4: 竞态复评（Minor#10）**：确认 Patch 5 的 `is_some()` 守卫已关闭启动竞态窗口，结论一行记 progress.md

### Task 17: frontmatter 解析 + 导入提示 + 存量建议入体检

**Files:**
- Create: `src-tauri/src/services/resource/frontmatter.rs`（mod.rs 注册；模块内 `#[cfg(test)]`）
- Modify: `src-tauri/src/commands/resource.rs` 的 `import_native_resources` 与 `src-tauri/src/commands/skill.rs` 的 `install_skill`（**两条手动导入路径都挂**；返回类型扩 `ImportOutcome { ..., suggestion: Option<{extension_id, tools}> }`，TS 调用方同步——grep `invoke("install_skill")` 定位）
- Modify: `commands/resource.rs` + `lib.rs`：`list_frontmatter_suggestions()` 命令（扫 `~/.mam/skills/*/SKILL.md`：parse 命中且 bindings 无行 → `{extensionId, tools}`）
- Modify: `HealthCheckCard.tsx`（建议段：逐条「设为专属/忽略本轮」）+ 前端导入调用处（Dialog 非 toast：确认=`setResourceBinding`；忽略=关闭）

**Interfaces:**

```rust
/// 解析 SKILL.md frontmatter 的专属声明。识别键（宽容别名）：
/// supported_agents / agents / compatible_tools / tools；
/// 值：YAML 数组 ["a","b"] 或逗号串 "a, b"；键缺失/坏 YAML/无 frontmatter → None
pub fn parse_supported_agents(skill_md: &str) -> Option<Vec<String>>
// 手写轻量解析（只取顶层标量/数组字段），不新增 serde_yaml 依赖
```

**验收条件：**
- [x] 解析器单测绿：标准数组 / 逗号串 / 四别名各一 / 坏 YAML / 缺键 / 无 frontmatter → 各断言
- [x] `cargo test` + `pnpm lint && pnpm build` 绿
- [ ] dev 目验：导入带声明的技能 → 确认弹窗 → 设为专属后徽标出现；体检卡片建议段出现存量建议，「设为专属」生效、「忽略本轮」不写绑定

- [x] **Step 1: 解析器 TDD** → **Step 2: 两导入路径 + 存量命令** → **Step 3: 前端两处** → **Step 4: 门禁 + Commit** `feat(preset-v2): frontmatter exclusive prefill — import-time prompt + stock suggestions in health card`

### Task 18: 用户手册 + DRY + 终门禁

**Files:**
- Modify: `docs/product/manual/zh/06-presets.md`、`en/06-presets.md`（占位补真：概念/双分区/开关/暂存区/常驻/专属/体检/托盘，对齐最终实现）
- Modify: `src-tauri/src/services/preset/mod.rs`（DRY）

**验收条件：**
- [x] 手册双语成文（非占位模板），结构与 `docs/product/manual/zh/` 其他章节一致
- [x] DRY（缺口表 #28 / Minor#7）：三处 enable/disable 分派闭包抽 `fn toggle_ext(ext_id, kind, tool_id, on) -> Result<(), String>`，行为不变、全量测试绿
- [x] `cargo test && cargo clippy -- -D warnings && cargo fmt --check`；`pnpm format:check && pnpm lint && pnpm build`

- [x] **Step 1: 手册** → **Step 2: DRY** → **Step 3: 门禁 + Commit** `docs(preset-v2): user manual zh/en + toggle_ext DRY + final gate`

### Task 19: 整体关门验收（本地，收官前最后一步）

**Files:**
- Create: `.superpowers/sdd/manual-test-checklist-v2.md`（手工测试清单终版）
- Modify: `docs/engineering/specs/2026-09-14-preset-groups-v2-design.md`（状态行）

**验收条件（全部满足才算 M2+M3 完结）：**
- [x] **全量门禁复跑**：`cd src-tauri && cargo test && cargo clippy -- -D warnings && cargo fmt --check` + `pnpm format:check && pnpm lint && pnpm build && npx vitest run`——全部绿，数字记入 progress.md
- [x] **i18n parity**：zh/en 叶子路径比对 0 差异
- [x] **mock parity**：`src/tauri-mock.ts` 与 `tests/msw/tauriMocks.ts` 对新命令的覆盖一致（读命令双 mock 有 fixture）
- [x] **自动化冒烟**：`preset_v2_test` + `reconcile_test` 全套作为端到端链路证明（apply→switch→restore / 漂移→处置 / MCP+plugin 往返 / 暂存对账）
- [x] **手工测试清单 v2** 成文：覆盖 28 项缺口表的 UI 目验点 + 原 PR 9 项手工点，注明「测试方法」（备份 `~/.mam/` 或 `MAM_HOME` 隔离）——收官 push 时用于替换 PR #63 描述
- [x] spec 状态行更新「M1-M3 全部已实施（待手工验收）」
- [x] **全程本地**：无 push、无 PR 变更，等待用户收官指令

- [x] **Step 1: 逐项执行上述清单** → **Step 2: Commit** `chore(preset-v2): M2+M3 closeout — full gates, parity checks, manual checklist v2, spec status`

---

## 执行顺序与依赖

```
T1（独立，最先）→ T2 → T3 → T4 → T5（弹窗）→ T6（开关，消费 T5）→ T7 → T8 → T9
                          T2/T3 ─→ T10 → T11（可与 Phase B 后半并行）
T12（可与 Phase B 并行启动）→ T13 → T14 → T15
T15 → T16 → T17（改 HealthCheckCard，须在 T15 后）→ T18 → T19
```

风险点：(1) T7 的 `is_native` 透出是 Rust 一行 + 类型同步，漏了会让私有预设选不到原生技能；(2) T16 `CheckMenuItem` 是 Tauri 2 API 细节，受阻则退化方案已备；(3) T17 禁新增 Cargo 依赖，frontmatter 手写解析；(4) T15 的暂存回移二选一推荐 (i) 新命令，(ii) 为降级方案。
