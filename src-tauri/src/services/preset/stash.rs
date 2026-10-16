// 暂存区引擎（spec §3.1/§5.3）：原生技能目录整体移动（同盘 rename，零拷贝），
// 非压缩包；stash_journal 是唯一账本，崩溃后凭账本恢复
use std::path::Path;

use crate::database::{self, StashEntryRecord};

/// 暂存区：~/.tuvis/stash/<tool>/skills
pub fn stash_dir(tool_id: &str) -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".tuvis")
        .join("stash")
        .join(tool_id)
        .join("skills")
}

/// 把原生技能目录移入暂存区。先移动后记账；记账失败回滚移动（不留无账暂存）
pub fn stash_native_skill(
    tool_id: &str,
    skill_name: &str,
    original_path: &Path,
) -> Result<(), String> {
    if !original_path.is_dir() {
        return Err(format!(
            "STASH_NATIVE_DIR_MISSING: 原生技能目录不存在: {}",
            original_path.display()
        ));
    }
    let dir = stash_dir(tool_id);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stashed = dir.join(skill_name);
    if stashed.exists() {
        return Err(format!(
            "STASH_NAME_TAKEN: 暂存区已有同名项: {}",
            stashed.display()
        ));
    }
    std::fs::rename(original_path, &stashed).map_err(|e| e.to_string())?;
    if let Err(e) = database::record_stash(
        tool_id,
        skill_name,
        &stashed.to_string_lossy(),
        &original_path.to_string_lossy(),
    ) {
        // 回滚移动，宁可不暂存也不留无账目录
        let _ = std::fs::rename(&stashed, original_path);
        return Err(format!("STASH_JOURNAL_FAIL: 暂存记账失败已回滚: {}", e));
    }
    log::info!("原生技能 {} 已暂存: {}", skill_name, stashed.display());
    Ok(())
}

/// 回移单条。原位被占（同名且存在）→ Err（不覆盖，调用方留待人工处理）
pub fn restore_stashed_skill(entry: &StashEntryRecord) -> Result<(), String> {
    let stashed = Path::new(&entry.stashed_path);
    let original = Path::new(&entry.original_path);
    if !stashed.exists() {
        return Err(format!(
            "STASH_ITEM_MISSING: 暂存区已无此项: {}",
            stashed.display()
        ));
    }
    if original.exists() {
        return Err(format!(
            "STASH_SLOT_TAKEN: 原位已被占用: {}",
            original.display()
        ));
    }
    if let Some(parent) = original.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::rename(stashed, original).map_err(|e| e.to_string())?;
    database::mark_stash_restored(entry.id)?;
    log::info!(
        "暂存技能 {} 已回移 {}",
        entry.skill_name,
        original.display()
    );
    Ok(())
}

/// 回移该工具全部未恢复项；冲突项不覆盖、留在暂存区并报 "name: 原因"
pub fn restore_all_for_tool(tool_id: &str) -> (Vec<String>, Vec<String>) {
    let mut restored = Vec::new();
    let mut conflicts = Vec::new();
    for entry in database::unrestored_stash(Some(tool_id)) {
        match restore_stashed_skill(&entry) {
            Ok(()) => restored.push(entry.skill_name),
            Err(e) => conflicts.push(format!("{}: {}", entry.skill_name, e)),
        }
    }
    (restored, conflicts)
}

/// 工具是否处于预设会话中（快照行存在即算——含首次应用「快照已存、active 未设」
/// 的瞬态，防止启动线程在该窗口回移暂存、拆掉进行中的应用；评审 Minor 1。
/// 「快照在而 active 为空」的崩溃残留由不变量检查器标记，见 check_snapshot_invariants）
fn tool_in_active_session(tool_id: &str) -> bool {
    database::get_base_snapshot(tool_id).is_some()
}

/// 启动孤儿恢复（spec §5.3 + 评审裁决 1）三段对账：
/// 1) 账本自愈（只写账不受会话守卫限）：暂存已不在而原位在 = 回移落位未销账的
///    硬崩溃残留，补销账；两侧皆不在则 log 保留待人工；
/// 2) 有账未恢复项回移（激活会话中的工具跳过，暂存是合法状态）；
/// 3) 无账孤儿对账（受会话守卫约束）：rename 与记账之间硬崩溃留下的暂存目录，
///    原位空闲则移回并补记审计（record 后立即销账），原位被占保留待人工。
///
/// 返回实际回移的技能数（账本自愈只写账不计入）
pub fn recover_orphans() -> usize {
    let mut recovered = 0usize;

    // 1) 账本自愈
    for entry in database::unrestored_stash(None) {
        let stashed = Path::new(&entry.stashed_path);
        let original = Path::new(&entry.original_path);
        if !stashed.exists() && original.exists() {
            if database::mark_stash_restored(entry.id).is_ok() {
                log::info!("账本自愈：{} 回移已落位但未销账，已补记", entry.skill_name);
            }
        } else if !stashed.exists() && !original.exists() {
            log::warn!(
                "账本异常：{} 暂存与原位皆不在，保留账目待人工处理",
                entry.skill_name
            );
        }
    }

    // 2) 有账未恢复项：按工具会话守卫回移
    for entry in database::unrestored_stash(None) {
        if tool_in_active_session(&entry.tool_id) {
            continue;
        }
        match restore_stashed_skill(&entry) {
            Ok(()) => recovered += 1,
            Err(e) => log::warn!("孤儿暂存 {} 未恢复: {}", entry.skill_name, e),
        }
    }

    // 3) 无账孤儿：扫暂存区目录对账
    let stash_root = dirs::home_dir()
        .unwrap_or_default()
        .join(".tuvis")
        .join("stash");
    if !stash_root.is_dir() {
        return recovered;
    }
    let tool_dirs = match std::fs::read_dir(&stash_root) {
        Ok(entries) => entries,
        Err(e) => {
            log::warn!("暂存区 {} 读取失败: {}", stash_root.display(), e);
            return recovered;
        }
    };
    for tool_entry in tool_dirs.flatten() {
        let tool_file_name = tool_entry.file_name();
        let Some(tool_id) = tool_file_name.to_str() else {
            continue;
        };
        if !crate::adapter::TOOL_IDS.contains(&tool_id) {
            log::warn!("暂存区发现未知工具目录 {}，跳过", tool_id);
            continue;
        }
        if tool_in_active_session(tool_id) {
            continue;
        }
        let skills_dir = tool_entry.path().join("skills");
        if !skills_dir.is_dir() {
            continue;
        }
        let ledgered: Vec<String> = database::unrestored_stash(Some(tool_id))
            .iter()
            .map(|e| e.skill_name.clone())
            .collect();
        let items = match std::fs::read_dir(&skills_dir) {
            Ok(entries) => entries,
            Err(e) => {
                log::warn!("暂存目录 {} 读取失败: {}", skills_dir.display(), e);
                continue;
            }
        };
        for item in items.flatten() {
            let stashed_path = item.path();
            // 暂存项一律是技能目录，只对账目录项
            if !stashed_path.is_dir() {
                continue;
            }
            let item_file_name = item.file_name();
            let Some(name) = item_file_name.to_str() else {
                continue;
            };
            if ledgered.iter().any(|n| n == name) {
                continue;
            }
            let Some(skill_root) = crate::adapter::primary_skill_dir(tool_id) else {
                log::warn!("工具 {} 无 skill 目录，无账孤儿 {} 跳过", tool_id, name);
                continue;
            };
            let original = skill_root.join(name);
            if original.exists() {
                log::warn!(
                    "无账暂存孤儿 {} 原位已被占用，保留待人工处理: {}",
                    name,
                    original.display()
                );
                continue;
            }
            if let Some(parent) = original.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    log::warn!("无账孤儿 {} 创建原位父目录失败: {}", name, e);
                    continue;
                }
            }
            match std::fs::rename(&stashed_path, &original) {
                Ok(()) => {
                    // 补记审计：先落账再立即销账，回移历史在账本可查
                    match database::record_stash(
                        tool_id,
                        name,
                        &stashed_path.to_string_lossy(),
                        &original.to_string_lossy(),
                    ) {
                        Ok(id) => {
                            if let Err(e) = database::mark_stash_restored(id) {
                                log::warn!("无账孤儿 {} 补记销账失败: {}", name, e);
                            }
                        }
                        Err(e) => log::warn!("无账孤儿 {} 补记账目失败: {}", name, e),
                    }
                    recovered += 1;
                    log::info!("无账暂存孤儿 {} 已移回 {}", name, original.display());
                }
                Err(e) => log::warn!("无账暂存孤儿 {} 回移失败: {}", name, e),
            }
        }
    }

    recovered
}
