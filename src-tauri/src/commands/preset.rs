// 预设组命令

use crate::database::{PresetRecord, ResourceBindingRecord};
use crate::services::preset::{ApplyPreview, PresetHealth, RestoreResult};

/// #64-3 悬空键防御（2026-10-14）：**通用预设不得包含原生技能**（is_native=1 行）。
/// 前端编辑弹窗已双层封死（切类型剪除 + 保存过滤，回归锁在案），但仅 UI 层——
/// 经「已修复的泄漏路径创建预设 → 后删除该扩展」等绕过路径仍可能持久化出含
/// 原生项的通用预设（应用侧此前只落 failures 列表非破坏性）。此处 Rust 侧直接拒绝。
/// 未登记 id（查询 None）**不拦**：预设项允许前瞻引用既有行为。口径与 apply 路径
/// （`services/preset`，`is_native && source_tool==工具` 双条件）的差异**如实申报**：
/// 命令层按 `is_native` 单条件拦——更严（无 source_tool 的 is_native 行也进不了
/// 通用预设），防御方向宁严勿松，非缺陷。
fn reject_native_items_in_universal(scope: &str, items: &[(String, String)]) -> Result<(), String> {
    if scope != "universal" {
        return Ok(());
    }
    for (ext_id, _) in items {
        if crate::database::dao::extension::extension_is_native(ext_id).unwrap_or(false) {
            return Err(format!(
                "通用预设不得包含原生技能 {ext_id}（原生项只属于工具私有预设；\
                 PRESET_NATIVE_IN_UNIVERSAL:{ext_id}）"
            ));
        }
    }
    Ok(())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetApplyResult {
    pub success_count: usize,
    pub failures: Vec<String>,
    pub conflicts: Vec<String>,
    pub stashed: Vec<String>,
    pub disabled: Vec<String>,
    pub restored_native: Vec<String>,
}

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
    reject_native_items_in_universal(&scope, &items)?;
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
    // Interfaces 契约：激活中的预设不可改（任一快照 active == id）——先恢复默认
    for tool in crate::adapter::TOOL_IDS {
        if let Some((Some(active), _)) = crate::database::get_base_snapshot(tool) {
            if active == id {
                return Err(format!(
                    "预设正在 {} 上激活，请先恢复默认再修改（PRESET_ACTIVE:{}）",
                    tool, id
                ));
            }
        }
    }
    if scope == "tool" && bound_tool.is_none() {
        return Err("工具私有预设必须指定绑定工具".into());
    }
    reject_native_items_in_universal(&scope, &items)?;
    crate::database::update_preset(
        &id,
        &name,
        &description,
        &scope,
        bound_tool.as_deref(),
        &items,
    )
}

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

/// 全工具激活预设（开关状态批量数据源，避免前端 N 次 invoke）
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivePreset {
    pub tool_id: String,
    pub preset_id: String,
}

#[tauri::command]
pub fn list_active_presets() -> Vec<ActivePreset> {
    crate::adapter::TOOL_IDS
        .iter()
        .filter_map(|tool| {
            crate::database::get_base_snapshot(tool)
                .and_then(|(active, _)| active)
                .map(|preset_id| ActivePreset {
                    tool_id: tool.to_string(),
                    preset_id,
                })
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

/// 应用预览（确认弹窗数据源，dry-run）
#[tauri::command]
pub fn preview_apply_preset(preset_id: String, tool_id: String) -> Result<ApplyPreview, String> {
    // 与 apply_preset 对齐（评审裁决 4）：停用工具不该拿到执行不了的预览
    crate::services::tool_settings::ensure_tool_enabled(&tool_id)?;
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

#[tauri::command]
pub fn list_presets() -> Vec<PresetRecord> {
    crate::database::list_presets()
}

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

#[tauri::command]
pub fn deactivate_preset(preset_id: String, tool_id: String) -> Result<(), String> {
    crate::services::tool_settings::ensure_tool_enabled(&tool_id)?;
    crate::services::preset::deactivate_preset(&preset_id, &tool_id)
}

#[tauri::command]
pub fn apply_preset_to_subagent(
    preset_id: String,
    tool_id: String,
    sub_agent_id: String,
) -> Result<PresetApplyResult, String> {
    crate::services::tool_settings::ensure_tool_enabled(&tool_id)?;
    let result =
        crate::services::preset::apply_preset_to_subagent(&preset_id, &tool_id, &sub_agent_id);
    Ok(PresetApplyResult {
        success_count: result.success,
        failures: result.failures,
        conflicts: result.conflicts,
        // 子 Agent 级应用不触发独占清扫/暂存（工具级专属语义），恒为空
        stashed: Vec::new(),
        disabled: Vec::new(),
        restored_native: Vec::new(),
    })
}

#[tauri::command]
pub fn deactivate_preset_from_subagent(
    preset_id: String,
    tool_id: String,
    sub_agent_id: String,
) -> Result<(), String> {
    crate::services::tool_settings::ensure_tool_enabled(&tool_id)?;
    crate::services::preset::deactivate_preset_from_subagent(&preset_id, &tool_id, &sub_agent_id)
}

/// 预设健康聚合（spec §13 检测侧收口）：不变量 + 未恢复暂存 + 账本-磁盘漂移三源合一
#[tauri::command]
pub fn get_preset_health() -> PresetHealth {
    crate::services::preset::preset_health()
}

/// 暂存条目回移（spec §13 体检卡片 ③；Task 15 裁决采方案 i）：
/// 按 id 在跨工具未恢复账目中查 entry（复用 unrestored_stash(None)，不新增 DAO），
/// 转发 stash::restore_stashed_skill；原位占用 / 暂存缺失等冲突原样报错
///（不覆盖现场），由前端 toast 透出
#[tauri::command]
pub fn restore_stash_entry(id: i64) -> Result<(), String> {
    let entry = crate::database::unrestored_stash(None)
        .into_iter()
        .find(|e| e.id == id)
        .ok_or_else(|| format!("暂存账本无此未恢复条目: id={}", id))?;
    crate::services::preset::stash::restore_stashed_skill(&entry)
}
