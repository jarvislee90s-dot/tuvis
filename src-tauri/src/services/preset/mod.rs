// 预设组应用逻辑 — 独占应用（spec §5.1）+ 基底恢复（spec §5.2）+ 部分成功处理

pub mod snapshot;
pub mod stash;
pub mod sweep;

use crate::database;
use crate::services;
use log::info;

/// 应用预设（独占语义，spec §5.1）：开 = 拍基底（若无）→ 差集清扫 → 启用预设项
#[derive(Default, Debug)]
pub struct ApplyResult {
    pub success: usize,
    pub failures: Vec<String>,
    pub conflicts: Vec<String>,
    /// 本次被暂存的原生技能名
    pub stashed: Vec<String>,
    /// 本次被停用的 兔维斯 资源 extension_id
    pub disabled: Vec<String>,
    /// 预设原生技能项从暂存区接回的名（ensure-present 语义）
    pub restored_native: Vec<String>,
}

/// 统一的 兔维斯 资源启停分派（DRY，缺口表 #28 / Minor#7）：apply_preset 与
/// restore_tool 里三处按 kind 路由 enable/disable 的 match 块收敛到此。
/// name = extension_id 剥掉 "<kind>-" 前缀（无前缀则原样）；plugin 的子类型
/// 参数取 extensions.tags（历史约定），缺省 "file"。未知 kind 返回 Err——
/// restore 路径旧闭包的 `_ => Ok(())` 致因 kind 由 extension_id 前缀推导、
/// 必为 skill/mcp/plugin 三选一而不可达，合并后行为等价。
fn toggle_ext(ext_id: &str, kind: &str, tool_id: &str, on: bool) -> Result<(), String> {
    let name = ext_id.strip_prefix(&format!("{}-", kind)).unwrap_or(ext_id);
    match kind {
        "skill" => {
            if on {
                services::enable_skill_for_tool(name, tool_id)
            } else {
                services::disable_skill_for_tool(name, tool_id)
            }
        }
        "mcp" => services::toggle_mcp(name, tool_id, on),
        "plugin" => {
            let plugin_kind = database::list_extensions()
                .into_iter()
                .find(|e| e.id == ext_id)
                .and_then(|e| e.tags.clone())
                .unwrap_or_else(|| "file".to_string());
            crate::services::plugin::toggle_plugin(name, tool_id, on, &plugin_kind)
        }
        _ => Err(format!("PRESET_KIND_UNKNOWN: 未知类型: {}", kind)),
    }
}

/// 工具私有预设的跨工具应用属硬错误（spec §3.3）
pub fn apply_preset(preset_id: &str, tool_id: &str) -> Result<ApplyResult, String> {
    let preset =
        database::get_preset(preset_id).ok_or_else(|| format!("预设不存在: {}", preset_id))?;
    if preset.scope == "tool" && preset.bound_tool.as_deref() != Some(tool_id) {
        return Err(format!(
            "预设 {} 绑定 {}，不能应用到 {}",
            preset.name,
            preset.bound_tool.as_deref().unwrap_or("?"),
            tool_id
        ));
    }
    let mut result = ApplyResult::default();

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
            result.conflicts.push(format!(
                "PRESET_EXCLUSIVE_CONFLICT: {}（{}）：专属绑定不兼容 {}",
                name, kind, tool_id
            ));
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

    // 4) 启用预设项：兔维斯 走既有服务；原生技能 = 确保在场（spec §3.3）
    for (ext_id, kind) in &apply_items {
        let name = ext_id.strip_prefix(&format!("{}-", kind)).unwrap_or(ext_id);
        let is_native_item = kind == "skill"
            && extensions
                .iter()
                .find(|e| &e.id == ext_id)
                .map(|e| e.is_native && e.source_tool.as_deref() == Some(tool_id))
                .unwrap_or(false);
        let outcome = if is_native_item {
            ensure_native_present(tool_id, name, &mut result.restored_native)
        } else {
            toggle_ext(ext_id, kind, tool_id, true)
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
        preset_id,
        tool_id,
        result.success,
        result.disabled.len(),
        result.stashed.len(),
        result.failures.len(),
        result.conflicts.len()
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

/// 应用预览（spec §7.3 差异确认弹窗数据源）：dry-run，不执行、不动现场
#[derive(Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyPreview {
    /// 将启用（专属过滤后）的 extension_id
    pub to_enable: Vec<String>,
    /// 被专属绑定过滤的项 "id: 原因"
    pub filtered: Vec<String>,
    /// 将停用的 兔维斯 资源 extension_id
    pub to_disable: Vec<String>,
    /// 将暂存的原生技能名
    pub to_stash: Vec<String>,
    /// 常驻豁免（当前生效、不在预设、但受常驻保护）的 extension_id
    pub resident_exempt: Vec<String>,
}

pub fn preview_apply(preset_id: &str, tool_id: &str) -> Result<ApplyPreview, String> {
    let preset =
        database::get_preset(preset_id).ok_or_else(|| format!("预设不存在: {}", preset_id))?;
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
            out.filtered.push(format!(
                "PRESET_EXCLUSIVE_CONFLICT: {}: 专属绑定不兼容 {}",
                ext_id, tool_id
            ));
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

    // 2) 兔维斯 资源精确重建到基底集合（spec §5.2 步骤3）：
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

    let enable_one = |ext_id: &str, kind: &str| toggle_ext(ext_id, kind, tool_id, true);
    let disable_one = |ext_id: &str, kind: &str| toggle_ext(ext_id, kind, tool_id, false);

    for (id, kind) in &target {
        if !current.iter().any(|(cid, _)| cid == id) {
            match enable_one(id, kind) {
                Ok(()) => out.restored_mam.push(id.clone()),
                Err(e) => out
                    .conflicts
                    .push(format!("PRESET_REBUILD_FAIL: {}: 重建失败 {}", id, e)),
            }
        }
    }
    for (id, kind) in &current {
        if target.iter().any(|(tid, _)| tid == id) {
            continue;
        }
        // 漂移防护（终审 Critical）：快照内任意 origin 的项都不属于「会话新增」——
        // 按 native 记录的漂移项（账本 enabled + 真目录）恢复后真目录已回移，
        // 走 disable 会让 remove_link 删掉真目录（数据丢失）。基底态=两项并存。
        // 漂移本身的治理属 M2 对账体系
        if base_items.iter().any(|i| &i.extension_id == id) {
            continue;
        }
        // 常驻豁免（review Finding 2）：镜像 plan_sweep 的计划层豁免检查——
        // 泄漏面 = 快照之后新启用且后标常驻的资源落在此处，直呼 disable_one 会
        // 被常驻守卫拒并误报 conflicts「停用失败…常驻…」；终态本就保持启用
        //（保护胜出），须与清扫计划层静默豁免 + residentExempt 报告口径一致。
        // 软报告选型：RestoreResult 无「豁免」集合字段，最小侵入 = 仅 log
        //（不动序列化结构，前端零改动），口径「常驻=恢复默认同样豁免，
        // 与清扫计划层一致」
        if database::is_tool_resident(tool_id, id) {
            info!(
                "[常驻豁免] 恢复默认跳过停用 {}/{}（常驻=恢复默认同样豁免，与清扫计划层一致）",
                tool_id, id
            );
            continue;
        }
        if let Err(e) = disable_one(id, kind) {
            out.conflicts
                .push(format!("PRESET_DISABLE_FAIL: {}: 停用失败 {}", id, e));
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
                out.conflicts.push(format!(
                    "{}#{}: 子 Agent 链接重建失败 {}",
                    a.extension_id, sub, e
                ));
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
            return Err(format!(
                "PRESET_OTHER_ACTIVE: 工具 {} 当前激活的是其他预设",
                tool_id
            ));
        }
        _ => {}
    }
    restore_tool(tool_id).map(|_| ())
}

/// 应用预设组到子 Agent
pub fn apply_preset_to_subagent(preset_id: &str, tool_id: &str, sub_agent_id: &str) -> ApplyResult {
    let items = database::get_preset_items(preset_id);
    let mut success = 0;
    let mut failures = Vec::new();
    let mut conflicts = Vec::new();

    for (ext_id, kind) in &items {
        // 子 Agent 级只支持 skill（MCP 和 Plugin 是工具级配置）
        if kind != "skill" {
            conflicts.push(format!(
                "PRESET_SUBAGENT_UNSUPPORTED: {} 类型 {} 不支持子 Agent 级分配",
                ext_id, kind
            ));
            continue;
        }

        // 检查是否在工具级范围内
        let name = ext_id.strip_prefix("skill-").unwrap_or(ext_id);
        if !crate::services::is_skill_in_tool_range(name, tool_id) {
            failures.push(format!(
                "PRESET_SKILL_NOT_ENABLED: {}: 该 skill 未在工具级启用",
                ext_id
            ));
            continue;
        }

        let result = crate::services::assign_skill_to_subagent(name, tool_id, sub_agent_id);
        match result {
            Ok(()) => success += 1,
            Err(e) => failures.push(format!("{}: {}", ext_id, e)),
        }
    }

    let _ = database::record_preset_application_subagent(preset_id, tool_id, sub_agent_id, true);
    info!(
        "预设组 {} -> {}:{} -- 成功 {} 失败 {} 冲突 {}",
        preset_id,
        tool_id,
        sub_agent_id,
        success,
        failures.len(),
        conflicts.len()
    );
    ApplyResult {
        success,
        failures,
        conflicts,
        ..Default::default()
    }
}

/// 兼容性检查结果
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompatibilityReport {
    pub compatible: Vec<CompatibleItem>,
    pub incompatible: Vec<IncompatibleItem>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompatibleItem {
    pub id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IncompatibleItem {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub reason: String,
}

/// 检查预设组与工具的兼容性
pub fn check_compatibility(preset_id: &str, tool_id: &str) -> CompatibilityReport {
    let items = database::get_preset_items(preset_id);
    let mut compatible = Vec::new();
    let mut incompatible = Vec::new();

    for (ext_id, kind) in items {
        // 获取资源信息
        let ext = database::list_extensions()
            .into_iter()
            .find(|e| e.id == ext_id);
        let name = ext
            .as_ref()
            .map(|e| e.name.clone())
            .unwrap_or_else(|| ext_id.clone());

        // 兼容判定（spec §6）：真值源是 resource_bindings（手动标记），
        // extensions.tags 不再参与（其语义是来源工具/插件子类型，历史误用）
        let is_compatible = crate::database::tool_allowed(&ext_id, tool_id);

        if is_compatible {
            compatible.push(CompatibleItem {
                id: ext_id.clone(),
                name,
                kind,
            });
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
    }

    CompatibilityReport {
        compatible,
        incompatible,
    }
}
pub fn deactivate_preset_from_subagent(
    preset_id: &str,
    tool_id: &str,
    sub_agent_id: &str,
) -> Result<(), String> {
    let items = database::get_preset_items(preset_id);
    let mut errors = Vec::new();
    for (ext_id, kind) in &items {
        if kind != "skill" {
            continue;
        }
        let name = ext_id.strip_prefix("skill-").unwrap_or(ext_id);
        let result = crate::linker::layer3::unlink_skill_from_layer3(name, tool_id, sub_agent_id);
        if let Err(e) = result {
            errors.push(format!("{}: {}", ext_id, e));
        }
        // 更新数据库记录
        let _ = crate::database::disable_subagent_assignment(ext_id, tool_id, sub_agent_id);
    }
    database::record_preset_application_subagent(preset_id, tool_id, sub_agent_id, false)?;
    if !errors.is_empty() {
        log::warn!("deactivate_preset_from_subagent 部分失败: {:?}", errors);
    }
    info!(
        "预设组 {} 从 {}:{} 取消激活",
        preset_id, tool_id, sub_agent_id
    );
    Ok(())
}

/// 快照不变量检查（spec §3.2）：存在基底快照 ⟺ 存在激活预设。
/// 违背项描述列表；启动时 log::warn，M2 UI 提示恢复或废弃
pub fn check_snapshot_invariants() -> Vec<String> {
    let mut broken = Vec::new();
    for tool in crate::adapter::TOOL_IDS {
        // clippy::single_match：等价 match 改写为 if let（Some((Some(_),_)) | None 臂为空）
        if let Some((None, _)) = database::get_base_snapshot(tool) {
            broken.push(format!(
                "PRESET_SNAPSHOT_ORPHAN: {}：快照存在但无激活预设",
                tool
            ));
        }
    }
    broken
}

/// 预设健康聚合（spec §13 检测侧收口）：不变量违背 + 未恢复暂存 + 账本-磁盘漂移
/// 三源合一，供体检卡片（Task 15）与设置页「立即体检」单次 invoke 取数
#[derive(Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetHealth {
    /// 快照不变量违背项（check_snapshot_invariants）
    pub invariants: Vec<String>,
    /// 跨全部工具的未恢复暂存条目（unrestored_stash(None)）
    pub stash_pending: Vec<crate::database::StashEntryRecord>,
    /// 账本-磁盘漂移（scan_drift，L1-L4）
    pub drift: Vec<crate::services::resource::reconcile::DriftItem>,
    /// 空目录（wave33 Item 2）：兔维斯 仓库与启用工具 skill 目录中的可清理空目录
    pub empty_dirs: Vec<crate::services::resource::reconcile::EmptyDirItem>,
}

/// 四源聚合（spec §13 + wave33 空目录）；顺序即结构体字段序，无短路
pub fn preset_health() -> PresetHealth {
    PresetHealth {
        invariants: check_snapshot_invariants(),
        stash_pending: database::unrestored_stash(None),
        drift: services::resource::reconcile::scan_drift(),
        empty_dirs: services::resource::reconcile::scan_empty_dirs(),
    }
}
