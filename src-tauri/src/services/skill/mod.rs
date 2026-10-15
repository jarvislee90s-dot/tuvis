// Skill 管理服务 - 安装、启用、禁用、子 Agent 分配

use crate::database;
use crate::linker;
use log::info;

/// 获取工具的 skill 目录
fn get_tool_skill_dir(tool_id: &str) -> Option<std::path::PathBuf> {
    crate::adapter::primary_skill_dir(tool_id)
}

/// 安装 skill 到全局仓库
pub fn install_skill(source_path: &str, name: &str, overwrite: bool) -> Result<(), String> {
    let source = std::path::Path::new(source_path);
    if !source.exists() {
        return Err(format!(
            "LINKER_SOURCE_MISSING: 源路径不存在: {}",
            source_path
        ));
    }
    linker::install_to_repo(source, name, overwrite)?;
    let ext = database::ExtensionRecord {
        id: format!("skill-{}", name),
        kind: "skill".to_string(),
        name: name.to_string(),
        description: None,
        source_path: source_path.to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: None,
        is_native: false,
    };
    database::insert_extension(&ext)?;
    info!("Skill 安装到全局仓库: {}", name);
    Ok(())
}

/// 为工具启用 skill（创建 Layer 2 symlink）。
/// 派发拍平（用户裁决 2026-09-17）：工具目录侧目标一律按拍平名
/// （linker::dispatch_target），source/账本侧保持嵌套规范名
pub fn enable_skill_for_tool(skill_name: &str, tool_id: &str) -> Result<(), String> {
    // 拍平碰撞守卫（用户裁决 2026-09-17）：仓库里存在与拍平名同名的另一技能
    // （如 v2m2-flat-c/skill 与字面平铺的 v2m2-flat-c-skill/ 并存）时，两条
    // 账目会争同一个磁盘链接名（派发后互相覆盖链接）→ 拒绝且零副作用。
    // 仅嵌套名需要判定（dispatch_name != skill_name）；平铺技能拍平名即自身，
    // 恒不触发
    let flat = crate::linker::dispatch_name(skill_name);
    if flat != skill_name && crate::linker::ensure_repo_dir().join(&flat).exists() {
        return Err(format!(
            "SKILL_FLATTEN_CONFLICT: 拍平名与现有技能冲突：{} 的派发名 {} 已被仓库内另一平铺技能占用",
            skill_name, flat
        ));
    }

    // 反向拍平碰撞守卫（终审 Important#1，控制器裁决 2026-09-17）：正向守卫挡
    // 「启用嵌套 x/y 时仓库已有平铺 x-y」；反向——先启用的嵌套技能已在工具
    // 目录留下拍平链接 x-y，后安装并启用字面平铺技能 x-y——放行会让
    // create_link 静默删掉嵌套技能的拍平链接换挂平铺内容（错误内容派发 +
    // 账本双 enabled + drift 不可见）。O(1) 判定，不加扫描：仅当 skill_name
    // 为平铺名（不含 /）且工具目录派发目标已是 symlink 时，解析其最终指向；
    // 指向 ≠ 本次要挂的 SSOT 技能（典型：嵌套技能的拍平链）→ 拒绝。指向相同
    // （重复启用幂等）与非 symlink（真目录走既有内容比对守卫）不受影响；
    // 悬空链接 canonicalize 失败不视为冲突（走既有清理路径）
    if !skill_name.contains('/') {
        if let Some(tool_skill_dir) = get_tool_skill_dir(tool_id) {
            let tool_target = crate::linker::dispatch_target(&tool_skill_dir, skill_name);
            if tool_target.is_symlink() {
                let occupied_by_other = match (
                    tool_target.canonicalize(),
                    crate::linker::ensure_repo_dir()
                        .join(skill_name)
                        .canonicalize(),
                ) {
                    (Ok(existing), Ok(want)) => existing != want,
                    _ => false,
                };
                if occupied_by_other {
                    return Err(format!(
                        "SKILL_FLATTEN_LINK_TAKEN: 拍平名冲突：{} 已被另一技能的链接占用（如嵌套技能的拍平链接），请先停用占用该名字的技能",
                        tool_target.display()
                    ));
                }
            }
        }
    }

    // 工具内建原生技能守卫（用户裁决 2026-09-16）：识别即常驻、不可启停。
    // 置于链接动作之前——命中即拒绝，Layer 2 链接与工具目录零副作用（内建目录
    // 永不被替换为链接/删除）；仅对真实目录判定（链接/不存在路径与非内建同路）。
    // disable 侧有同款对称守卫：remove_link 对真目录走 remove_dir_all，无守卫
    // 会递归删掉内建目录（见 disable_skill_for_tool）
    if let Some(tool_skill_dir) = get_tool_skill_dir(tool_id) {
        let tool_target = crate::linker::dispatch_target(&tool_skill_dir, skill_name);
        if tool_target.is_dir()
            && !tool_target.is_symlink()
            && crate::adapter::is_builtin_native_skill(tool_id, skill_name, &tool_target)
        {
            return Err(format!(
                "SKILL_BUILTIN_READONLY: 工具内建技能，不可操作: {}",
                tool_target.display()
            ));
        }
    }

    let layer2_path = crate::linker::layer2::link_skill_to_layer2(skill_name, tool_id)?;

    if let Some(tool_skill_dir) = get_tool_skill_dir(tool_id) {
        let _ = std::fs::create_dir_all(&tool_skill_dir);
        let tool_target = crate::linker::dispatch_target(&tool_skill_dir, skill_name);
        let mut should_create_tool_link = true;
        if tool_target.exists() || tool_target.is_symlink() {
            // 若父级已是链接（历史形态：套件整体派发为父目录链接），目标路径可能
            // 穿透到 SSOT；此时子技能已经可用，不应再次删除 SSOT 中的真实目录。
            // 【拍平派发（2026-09-17）后预计不可达——嵌套名不再产生父目录链接，
            // 保留作纵深防御（历史防删保护）】
            let repo_skill = crate::linker::ensure_repo_dir().join(skill_name);
            let reaches_ssot = !tool_target.is_symlink()
                && tool_target.canonicalize().ok() == repo_skill.canonicalize().ok();
            if reaches_ssot {
                should_create_tool_link = false;
            } else if !tool_target.is_symlink() && tool_target.is_dir() {
                // review I-1：真目录可能是用户手装技能，或 W5 停用后还原的内容
                // （用户可能已就地修改）。与 SSOT 内容一致（还原副本/刚导入的拷贝）
                // → 安全替换为链接（W5 停用/启用周期照常）；不一致 → 拒绝并报告，
                // 绝不静默删除用户数据后挂链（spec §4.3「跳过并报告」语义延伸到管线）。
                // 纳管走资源面板的显式清理流程（cleanup_duplicate_skills）
                if !crate::linker::dir_contents_equal(&tool_target, &repo_skill) {
                    return Err(format!(
                        "SKILL_DIR_DIRTY: {} 为真实目录且与共享仓库内容不一致，已拒绝替换为链接（防止覆盖本地修改）；如需纳管请先在资源面板处理同名技能",
                        tool_target.display()
                    ));
                }
                let _ = crate::linker::remove_link(&tool_target);
            } else {
                let _ = crate::linker::remove_link(&tool_target);
            }
        }
        if should_create_tool_link {
            crate::linker::create_link(&layer2_path, &tool_target)?;
        }
    }

    let ext_id = format!("skill-{}", skill_name);
    database::upsert_assignment(&ext_id, tool_id, true, "valid")?;
    info!("Skill {} 已为 {} 启用（Layer 2）", skill_name, tool_id);
    Ok(())
}

/// 为工具禁用 skill（移除 Layer 2 symlink + 工具目录 symlink）。
/// 工具目录目标按拍平名定位（与 enable 同口径）
pub fn disable_skill_for_tool(skill_name: &str, tool_id: &str) -> Result<(), String> {
    // 常驻=停用防护（用户裁决 2026-09-18）：常驻 on 时手动停用被拒，需先关闭
    // 常驻；启用方向不受影响。守卫置于一切断链动作之前——命中即零副作用。
    // W5 整工具停用的还原清理（disable_tool_cleanup）不走本函数（直接
    // restore_mam_link / remove_mcp），不受此守卫约束；独占清扫（sweep）在
    // 计划层已剔除常驻项，同样不会触达
    let ext_id = format!("skill-{}", skill_name);
    if crate::database::is_tool_resident(tool_id, &ext_id) {
        return Err(format!(
            "SKILL_RESIDENT_CONFLICT: skill {} 是 {} 的常驻资源，先关闭常驻再停用",
            skill_name, tool_id
        ));
    }
    // 工具内建原生技能守卫（终审 Important#2，与 enable 侧对称；用户裁决
    // 2026-09-16「不可启停」）：remove_link 对真目录走 remove_dir_all——以
    // .system/_shared/marker 名调用会递归删掉工具内建目录。当前调用方均
    // 不可达（内建项不进账本/快照/清扫计划），属纵深防御；仅对真实目录判定
    if let Some(tool_skill_dir) = get_tool_skill_dir(tool_id) {
        let tool_target = crate::linker::dispatch_target(&tool_skill_dir, skill_name);
        if tool_target.is_dir()
            && !tool_target.is_symlink()
            && crate::adapter::is_builtin_native_skill(tool_id, skill_name, &tool_target)
        {
            return Err(format!(
                "SKILL_BUILTIN_READONLY: 工具内建技能，不可操作: {}",
                tool_target.display()
            ));
        }
    }
    if let Some(tool_skill_dir) = get_tool_skill_dir(tool_id) {
        let tool_target = crate::linker::dispatch_target(&tool_skill_dir, skill_name);
        let _ = crate::linker::remove_link(&tool_target);
    }
    let _ = crate::linker::layer3::cleanup_layer3_on_tool_disable(skill_name, tool_id);
    crate::linker::layer2::unlink_skill_from_layer2(skill_name, tool_id)?;

    database::upsert_assignment(&ext_id, tool_id, false, "missing")?;
    info!("Skill {} 已为 {} 禁用", skill_name, tool_id);
    Ok(())
}

/// 检查 skill 是否在工具级范围内
pub fn is_skill_in_tool_range(skill_name: &str, tool_id: &str) -> bool {
    let ext_id = format!("skill-{}", skill_name);
    let assignments = crate::database::list_assignments(tool_id);
    assignments
        .iter()
        .any(|a| a.extension_id == ext_id && a.enabled)
}

/// 为子 Agent 分配 skill（带约束检查，走 Layer 3）
pub fn assign_skill_to_subagent(
    skill_name: &str,
    tool_id: &str,
    sub_agent_id: &str,
) -> Result<(), String> {
    if !is_skill_in_tool_range(skill_name, tool_id) {
        return Err(format!(
            "SKILL_NOT_TOOL_ENABLED: Skill {} 未在 {} 的工具级分配中启用，无法分配给子 Agent",
            skill_name, tool_id
        ));
    }

    let adapter = crate::adapter::adapter_by_id(tool_id)
        .ok_or_else(|| format!("CONTENT_UNKNOWN_TOOL: 未知工具: {}", tool_id))?;

    let has_subagent_dir = adapter.subagent_dir().is_some();

    if has_subagent_dir {
        crate::linker::layer3::link_skill_to_layer3(skill_name, tool_id, sub_agent_id)?;
        if let Some(skill_dir) = adapter.skill_dirs().into_iter().next() {
            let subagent_dir = skill_dir.join("subagents").join(sub_agent_id);
            let _ = std::fs::create_dir_all(&subagent_dir);
            // 派发拍平（2026-09-17）：子 Agent 工具目录目标与 Layer3 链接源
            // 都按拍平名定位（Layer3 落盘即拍平链接）
            let tool_target = crate::linker::dispatch_target(&subagent_dir, skill_name);
            let layer3_path = crate::linker::dispatch_target(
                &crate::linker::layer3::subagent_active_dir(tool_id, sub_agent_id),
                skill_name,
            );
            if tool_target.exists() || tool_target.is_symlink() {
                let _ = crate::linker::remove_link(&tool_target);
            }
            crate::linker::create_link(&layer3_path, &tool_target)?;
        }
    }

    let ext_id = format!("skill-{}", skill_name);
    crate::database::upsert_assignment_with_subagent(
        &ext_id,
        tool_id,
        sub_agent_id,
        true,
        if has_subagent_dir { "valid" } else { "ui-only" },
    )?;
    info!(
        "Skill {} 已分配给子 Agent {}（{}）",
        skill_name,
        sub_agent_id,
        if has_subagent_dir {
            "Layer 3"
        } else {
            "UI-only"
        }
    );
    Ok(())
}
