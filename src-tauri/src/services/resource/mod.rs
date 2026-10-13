// 资源管理服务 - 自动扫描导入 skills 和 plugins

pub mod frontmatter;
pub mod migration;
pub mod reconcile;

use crate::linker;

/// 递归扫描目录，找到所有直接包含 SKILL.md 的子目录
/// 返回相对路径列表（如 "brainstorming", "superpowers/brainstorming"）
/// 深度上限 4 层，symlink 目录不跟随（防循环）
/// 资源视图（list_ssot_resources）与注册表回填（backfill_registry）共用同一扫描，
/// 保证「卡片看到的」与「登记进表的」口径一致
pub fn scan_skill_dirs(base: &std::path::Path) -> Vec<String> {
    const SCAN_MAX_DEPTH: usize = 4;
    let mut results = Vec::new();
    fn recurse(
        dir: &std::path::Path,
        base: &std::path::Path,
        depth: usize,
        results: &mut Vec<String>,
    ) {
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
    results.sort();
    results
}

/// SKILL.md 元数据
struct SkillMeta {
    name: String,
    description: Option<String>,
}

/// 从 SKILL.md 提取 name 和 description（YAML front matter）
fn parse_skill_meta(skill_md_path: &std::path::Path) -> Option<SkillMeta> {
    let content = std::fs::read_to_string(skill_md_path).ok()?;
    let front_matter = if let Some(after) = content.strip_prefix("---") {
        &after[..after.find("---")?]
    } else {
        &content[..]
    };
    let name = front_matter.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("name:")
            .map(|v| v.trim().trim_matches(char::from(34)).to_string())
    })?;
    if name.is_empty() {
        return None;
    }
    let description = front_matter
        .lines()
        .find_map(|line| {
            let trimmed = line.trim();
            trimmed
                .strip_prefix("description:")
                .map(|v| v.trim().trim_matches(char::from(34)).to_string())
        })
        .filter(|s| !s.is_empty());
    Some(SkillMeta { name, description })
}

/// 检测套件名称
fn detect_suite(
    skill_name: &str,
    skill_path: &std::path::Path,
    skills_root: &std::path::Path,
) -> Option<String> {
    if let Ok(relative) = skill_path.strip_prefix(skills_root) {
        let components: Vec<_> = relative.components().collect();
        if components.len() > 1 {
            return components[0].as_os_str().to_str().map(String::from);
        }
    }
    if let Some(dash_pos) = skill_name.find('-') {
        let prefix = &skill_name[..dash_pos];
        let known_suites = ["speckit"];
        if known_suites.contains(&prefix) {
            return Some(prefix.to_string());
        }
    }
    None
}

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
                // `~/.codex/skills/.system` 是 codex 官方捆绑的系统技能（skill-creator /
                // skill-installer / imagegen 等，随版本更新生成，spec F2），不属于用户
                // 技能，不入 兔维斯 SSOT；本函数为递归扫描，`.system/<skill>/SKILL.md`
                // 会被命中，故显式跳过（spec 2026-09-09 §4.1）。收窄到扫描根顶层
                // （depth == 0）：官方捆绑只出现在根顶，更深处的同名目录是用户内容，
                // 不应被一刀切排除（review Minor）
                if depth == 0 && path.file_name().and_then(|n| n.to_str()) == Some(".system") {
                    continue;
                }
                let skill_md = path.join("SKILL.md");
                if skill_md.exists() {
                    if let Some(meta) = parse_skill_meta(&skill_md) {
                        let _ = detect_suite(&meta.name, &path, skills_root);
                        results.push((path.clone(), meta.name));
                    }
                }
                results.extend(scan_skills_recursive(&path, skills_root, depth + 1));
            }
        }
    }
    results
}

/// 技能导入统计
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportStats {
    pub imported: usize,
    pub newly_added: usize,
    pub skipped_dup: usize,
    pub source_counts: Vec<(String, usize)>,
    /// frontmatter 专属预填建议（spec §6，Task 17）：仅手动导入路径
    /// （import_native_resources）取首个命中项填充——brief 语义「弹一个提示」，
    /// 其余命中项交由 list_frontmatter_suggestions 进体检卡片兜底；
    /// 启动自动导入（rescan_skills）恒 None（自动识别只建议不强制）
    pub suggestion: Option<crate::services::resource::frontmatter::FrontmatterSuggestion>,
}

/// 单次导入决策：SSOT 有无 + 工具显式禁用状态决定复制/补链/跳过
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

/// 从源路径推断 skill 所属工具。
/// 债修复（ZCode 接入轮）：原实现硬编码 ["claude","codex","opencode","openclaw"]，
/// kimi/workbuddy 来源的历史行回溯恒 None → 跳过补链；改走 TOOL_IDS 注册表
/// （zcode 登记后自动纳入，新工具不再逐个补）
fn detect_source_tool(source_path: &str) -> Option<String> {
    let path = std::path::Path::new(source_path);
    for tool_id in crate::adapter::TOOL_IDS {
        if let Some(dir) = crate::adapter::primary_skill_dir(tool_id) {
            if path.starts_with(&dir) {
                return Some(tool_id.to_string());
            }
        }
    }
    None
}

/// 补链门（P0-1 可测核心）：工具停用 → 一律不补链不修链——W5 停用已把工具 skill 目录
/// 还原为真实内容（用户可能已就地修改），此时目标非链接态、already_linked 不成立，
/// 若放行重建会 remove_link 删除用户真实目录并重挂链接，用户数据静默丢失；
/// 已链接 → 无需处理；否则执行 enable 重建。返回是否执行了重建
fn ensure_skill_relink(
    tool_id: &str,
    tool_enabled: &dyn Fn(&str) -> bool,
    already_linked: &dyn Fn() -> bool,
    enable: &dyn Fn() -> Result<(), String>,
) -> Result<bool, String> {
    if !tool_enabled(tool_id) {
        return Ok(false);
    }
    if already_linked() {
        return Ok(false);
    }
    enable()?;
    Ok(true)
}

/// 为已导入但尚未建立工具链接的 skill 补链（尊重用户显式禁用）
pub fn sync_imported_skill_links() {
    let tool_enabled = |tool_id: &str| crate::database::dao::agent_tool::get_tool_enabled(tool_id);
    sync_imported_skill_links_with(&tool_enabled);
}

/// 可测核心（P0-1）：tool_enabled 注入。真实实现读 agent_tools 表（W5 单一事实源）
pub fn sync_imported_skill_links_with(tool_enabled: &dyn Fn(&str) -> bool) {
    for ext in crate::database::list_extensions() {
        if ext.kind != "skill" {
            continue;
        }

        let source_tool = ext
            .source_tool
            .clone()
            .or_else(|| detect_source_tool(&ext.source_path));
        let Some(tool_id) = source_tool else {
            continue;
        };

        // P0-1：工具已停用（W5 勾选门）→ 跳过该工具的全部补链与断链修复。
        // 停用还原出的真实目录被重建 = 用户数据丢失（见 ensure_skill_relink 注释）
        if !tool_enabled(&tool_id) {
            continue;
        }

        let assignments = crate::database::list_assignments(&tool_id);
        if assignments
            .iter()
            .any(|a| a.extension_id == ext.id && !a.enabled)
        {
            continue;
        }

        // 断链检测与自动修复：SSOT 仍在则重建，SSOT 缺失则清链接并标记。
        // 工具目录目标按拍平名定位（派发拍平，2026-09-17 裁决）；下方 repo 侧保持嵌套原路径
        let tool_target = crate::adapter::primary_skill_dir(&tool_id)
            .map(|d| crate::linker::dispatch_target(&d, &ext.name));
        if let Some(t) = &tool_target {
            if crate::linker::check_link_health(t) == crate::linker::LinkHealth::Dangling {
                let repo_exists = crate::linker::ensure_repo_dir().join(&ext.name).exists();
                if let Err(e) = crate::linker::remove_link(t) {
                    log::warn!("移除断链 {} 失败: {}", t.display(), e);
                }
                if repo_exists {
                    if let Err(e) = crate::services::enable_skill_for_tool(&ext.name, &tool_id) {
                        log::warn!("重建 {} → {} 断链失败: {}", ext.name, tool_id, e);
                        let _ =
                            crate::database::upsert_assignment(&ext.id, &tool_id, true, "dangling");
                    }
                } else {
                    let _ = crate::linker::layer2::unlink_skill_from_layer2(&ext.name, &tool_id);
                    let _ = crate::database::upsert_assignment(&ext.id, &tool_id, false, "missing");
                }
                continue;
            }
        }

        let already_linked = crate::adapter::primary_skill_dir(&tool_id)
            .map(|dir| crate::linker::dispatch_target(&dir, &ext.name).is_symlink())
            .unwrap_or(false);
        // P0-1：补链统一走勾选门（停用工具不得重建，见 ensure_skill_relink）
        let _ = ensure_skill_relink(&tool_id, &tool_enabled, &|| already_linked, &|| {
            crate::services::enable_skill_for_tool(&ext.name, &tool_id)
        })
        .inspect_err(|e| log::warn!("补链 {} 到 {} 失败: {}", ext.name, tool_id, e));
    }

    // 兼容历史数据：assignment 表里可能已有 skill 记录，但 extensions 表没有对应行
    let mut assignments: Vec<_> = crate::database::list_all_assignments();
    // 排序为历史遗留（嵌套派发期需先顶层后嵌套、避免父目录被建成真实目录）；
    // 拍平派发（2026-09-17）后嵌套名不再产生父目录，排序保留无害
    assignments.sort_by_key(|a| a.extension_id.matches('/').count());
    for assignment in assignments {
        if !assignment.enabled {
            continue;
        }
        // P0-1：勾选门同样适用于历史 assignment 行（停用工具不得重建）
        if !tool_enabled(&assignment.agent_tool_id) {
            continue;
        }
        let Some(skill_name) = assignment.extension_id.strip_prefix("skill-") else {
            continue;
        };
        let repo_skill = crate::linker::ensure_repo_dir().join(skill_name);
        if !repo_skill.exists() {
            continue;
        }

        // 工具目录目标按拍平名定位（派发拍平，2026-09-17 裁决）
        let already_linked = crate::adapter::primary_skill_dir(&assignment.agent_tool_id)
            .map(|dir| crate::linker::dispatch_target(&dir, skill_name).is_symlink())
            .unwrap_or(false);
        if let Err(e) = ensure_skill_relink(
            &assignment.agent_tool_id,
            &tool_enabled,
            &|| already_linked,
            &|| crate::services::enable_skill_for_tool(skill_name, &assignment.agent_tool_id),
        ) {
            log::warn!(
                "补链 {} 到 {} 失败: {}",
                skill_name,
                assignment.agent_tool_id,
                e
            );
        }
    }
}

/// `.agents` 开放标准共享技能目录的导入源标签（spec 2026-09-09 §4.2）
pub(crate) const AGENTS_SHARED_SOURCE: &str = "agents-shared";

/// 单个 skill 扫描源：注册表工具源（导入后为该工具建链）或 `.agents` 共享源
/// （只读入库、不归属任何工具——review 建议 enum 化，替代「label 字符串 + is_shared
/// 布尔」双轨与对"伪工具 id 查 DB 天然为空"的隐含依赖）
pub(crate) enum SkillScanSource {
    Tool { id: String, dir: std::path::PathBuf },
    Shared { dir: std::path::PathBuf },
}

impl SkillScanSource {
    fn dir(&self) -> &std::path::Path {
        match self {
            SkillScanSource::Tool { dir, .. } | SkillScanSource::Shared { dir } => dir,
        }
    }

    /// 来源标签：工具 id 或 AGENTS_SHARED_SOURCE（source_counts / 日志用）
    fn label(&self) -> &str {
        match self {
            SkillScanSource::Tool { id, .. } => id,
            SkillScanSource::Shared { .. } => AGENTS_SHARED_SOURCE,
        }
    }
}

/// skill 导入依赖注入（可测核心，spec §7.7）：真实实现接全局 DB 与 enable 服务，
/// 测试用内存闭包替代——全局 DB 指向真实 ~/.tuvis/tuvis.db，测试禁触。
/// install_to_repo 同样注入：真实实现会写 ~/.tuvis/skills（ensure_repo_dir），
/// 不注入则测试仍会触真实家目录
pub(crate) struct SkillImportDeps<'a> {
    pub list_extensions: &'a dyn Fn() -> Vec<crate::database::ExtensionRecord>,
    pub list_assignments: &'a dyn Fn(&str) -> Vec<crate::database::AssignmentRecord>,
    pub insert_extension: &'a dyn Fn(&crate::database::ExtensionRecord),
    pub install_to_repo: &'a dyn Fn(&std::path::Path, &str, bool) -> Result<(), String>,
    pub enable_skill: &'a dyn Fn(&str, &str) -> Result<(), String>,
}

/// 可测核心（spec §7.7）：按源清单扫描导入 skill 到全局仓库（含去重）。
/// 返回 (imported, skipped_dup, source_counts)；
/// seen_names 播种逻辑留在核心内：只按 skill 的 name 播种，避免跨类型同名
/// （plugin/mcp/native 与 skill 共用一张表）误判为已导入
fn import_skills_from_sources(
    sources: &[SkillScanSource],
    force: bool,
    deps: &SkillImportDeps<'_>,
) -> (usize, usize, Vec<(String, usize)>) {
    // 增量模式（force=false 且 DB 已有数据）：已存在的 name 只补链不重导（Task 6 的 LinkOnly）；
    // force=true 全量重扫保持覆盖导入语义
    let mut seen_names: std::collections::HashSet<String> = if force {
        std::collections::HashSet::new()
    } else {
        (deps.list_extensions)()
            .iter()
            .filter(|e| e.kind == "skill")
            .map(|e| e.name.clone())
            .collect()
    };
    let mut imported: usize = 0;
    let mut skipped_dup: usize = 0;
    let mut source_counts: Vec<(String, usize)> = Vec::new();

    for source in sources {
        if !source.dir().exists() {
            continue;
        }
        let found = scan_skills_recursive(source.dir(), source.dir(), 0);
        log::info!(
            "扫描 {} ({}): 找到 {} 个 SKILL.md",
            source.label(),
            source.dir().display(),
            found.len()
        );
        source_counts.push((source.label().to_string(), found.len()));

        // `.agents` 共享源（spec §4.2 / F1/F3）：兔维斯 对其只读，发现的技能「入库不归属」
        // ——source_tool 置 None、tags 记 agents-shared、任何分支都不为工具建链
        // （Task 1 后 detect_source_tool 对该路径返回 None，
        // sync_imported_skill_links 同样不会为其补链）
        let (tool_id, is_shared) = match source {
            SkillScanSource::Tool { id, .. } => (id.as_str(), false),
            SkillScanSource::Shared { .. } => (AGENTS_SHARED_SOURCE, true),
        };

        for (skill_path, skill_name) in &found {
            // 共享源非真实工具 id，list_assignments 天然为空 → tool_enabled 为
            // None → plan 只会落在 ImportAndLink/LinkOnly，不会被误判为显式禁用
            let tool_enabled = (deps.list_assignments)(tool_id)
                .iter()
                .find(|a| a.extension_id == format!("skill-{}", skill_name))
                .map(|a| a.enabled);
            match plan_skill_import(skill_name, &seen_names, tool_enabled) {
                SkillImportPlan::ImportAndLink => {
                    seen_names.insert(skill_name.clone());

                    let meta = parse_skill_meta(&skill_path.join("SKILL.md"));
                    let description = meta.as_ref().and_then(|m| m.description.clone());
                    let suite = detect_suite(skill_name, skill_path, source.dir());

                    if let Err(e) = (deps.install_to_repo)(skill_path, skill_name, force) {
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
                        // 共享源「入库不归属」：source_tool 置 None（sync 补链随之跳过）
                        source_tool: if is_shared {
                            None
                        } else {
                            Some(tool_id.to_string())
                        },
                        is_native: false,
                    };
                    (deps.insert_extension)(&ext);
                    // 默认按来源工具自动创建工具目录链接，让 harness 立即可用
                    // （共享源「入库不归属」，跳过建链）
                    if !is_shared {
                        if let Err(e) = (deps.enable_skill)(skill_name, tool_id) {
                            log::warn!("导入 {} 后为 {} 创建链接失败: {}", skill_name, tool_id, e);
                        }
                    }
                    imported += 1;
                }
                SkillImportPlan::LinkOnly => {
                    // 共享源不归属任何工具，补链 no-op（静默）
                    if !is_shared {
                        if let Err(e) = (deps.enable_skill)(skill_name, tool_id) {
                            log::warn!("为 {} 补建 {} 链接失败: {}", skill_name, tool_id, e);
                        }
                    }
                }
                SkillImportPlan::Skip => {
                    skipped_dup += 1;
                }
            }
        }
    }

    (imported, skipped_dup, source_counts)
}

/// 扫描各工具的 skill 目录与 `~/.agents/skills` 共享目录，递归导入到全局仓库（含去重）
pub fn auto_import_extensions(force: bool) -> ImportStats {
    let _repo = linker::ensure_repo_dir();
    // 指标用：全部 kind 的导入前 name 基线（与播种集分离，避免被 kind 过滤影响；
    // 播种逻辑已随导入循环下沉到 import_skills_from_sources）
    let all_names_before: std::collections::HashSet<String> = crate::database::list_extensions()
        .iter()
        .map(|e| e.name.clone())
        .collect();

    // 注册表源：各工具主 skill 目录（新工具登记 adapter 后自动纳入扫描）
    let mut skill_sources: Vec<SkillScanSource> = crate::adapter::TOOL_IDS
        .iter()
        .copied()
        .filter_map(|tool_id| {
            crate::adapter::primary_skill_dir(tool_id).map(|dir| SkillScanSource::Tool {
                id: tool_id.to_string(),
                dir,
            })
        })
        .collect();
    // 追加共享源（spec §4.2）：`~/.agents/skills` 是 Agent Skills 开放标准共享目录
    // （codex/zcode 等工具同读），兔维斯 只读导入，「入库不归属」任何工具
    skill_sources.push(SkillScanSource::Shared {
        dir: dirs::home_dir()
            .unwrap_or_default()
            .join(".agents")
            .join("skills"),
    });

    // 真实依赖：接全局 DB 与 enable 服务（测试用内存闭包替换）
    let deps = SkillImportDeps {
        list_extensions: &crate::database::list_extensions,
        list_assignments: &crate::database::list_assignments,
        insert_extension: &|ext| {
            let _ = crate::database::insert_extension(ext);
        },
        install_to_repo: &linker::install_to_repo,
        enable_skill: &crate::services::enable_skill_for_tool,
    };
    let (mut imported, mut skipped_dup, source_counts) =
        import_skills_from_sources(&skill_sources, force, &deps);

    // Plugin 扫描
    // Plugin 去重使用独立集合，避免与 skill 同名互相吞掉
    let mut plugin_seen: std::collections::HashSet<String> = if force {
        std::collections::HashSet::new()
    } else {
        // 增量模式：只按 plugin 的 name 播种，已导入的插件不重导（避免每次启动覆写 SSOT）
        crate::database::list_extensions()
            .iter()
            .filter(|e| e.kind == "plugin")
            .map(|e| e.name.clone())
            .collect()
    };
    // 各工具插件目录取 adapter.plugin_dirs() 首项（与既有硬编码路径逐工具一致），
    // 新增工具登记 adapter 后自动纳入扫描
    let plugin_sources: Vec<(String, std::path::PathBuf)> = crate::adapter::all_adapters_with_ids()
        .into_iter()
        .filter_map(|(tool_id, adapter)| {
            adapter
                .plugin_dirs()
                .into_iter()
                .next()
                .map(|dir| (tool_id.to_string(), dir))
        })
        .collect();

    for (tool_id, plugins_dir) in &plugin_sources {
        if !plugins_dir.exists() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(plugins_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if plugin_seen.contains(&name) {
                    continue;
                }
                plugin_seen.insert(name.clone());

                let kind = if path.is_dir() { "file" } else { "config" };
                let plugin_repo = dirs::home_dir()
                    .unwrap_or_default()
                    .join(".tuvis")
                    .join("plugins");
                let _ = std::fs::create_dir_all(&plugin_repo);
                let dest = plugin_repo.join(&name);
                if dest.exists() {
                    if !force {
                        // 增量模式：SSOT 已有同名插件目录（孤儿/未纳管），不覆盖
                        skipped_dup += 1;
                        continue;
                    }
                    let _ = std::fs::remove_dir_all(&dest);
                }
                if path.is_dir() {
                    let _ = crate::linker::copy_dir_recursive(&path, &dest);
                } else {
                    let _ = std::fs::copy(&path, &dest);
                }

                let ext = crate::database::ExtensionRecord {
                    id: format!("plugin-{}", name),
                    kind: "plugin".to_string(),
                    name: name.clone(),
                    description: None,
                    source_path: path.to_string_lossy().to_string(),
                    source_url: None,
                    version: None,
                    tags: Some(kind.to_string()),
                    suite: None,
                    source_tool: Some(tool_id.to_string()),
                    is_native: false,
                };
                let _ = crate::database::insert_extension(&ext);
                imported += 1;
            }
        }
    }

    let existing_after: std::collections::HashSet<String> = crate::database::list_extensions()
        .iter()
        .map(|e| e.name.clone())
        .collect();
    let newly_added = existing_after.difference(&all_names_before).count();

    if imported > 0 {
        log::info!(
            "扫描完成: 处理 {} 个（新增 {} 个，跳过 {} 个重复）",
            imported,
            newly_added,
            skipped_dup
        );
    }
    ImportStats {
        imported,
        newly_added,
        skipped_dup,
        source_counts,
        // 自动导入只建议不强制（spec §6 / 2026-09-15 裁决）：存量建议走体检卡片
        suggestion: None,
    }
}

/// 注册表回填（spec §8.1 P2①）：SSOT 目录里有、extensions 表里无行的资源补登记。
/// 启动时调用，幂等——预设创建列表（查表）与资源卡片（扫目录）从此同源
pub fn backfill_registry() {
    let home = dirs::home_dir().unwrap_or_default();

    // skill：~/.tuvis/skills 递归扫描（与资源视图 list_ssot_resources 同一
    // scan_skill_dirs 口径），嵌套套件技能（如 superpowers/brainstorming）按
    // 相对路径登记（id/name 均带斜杠路径）；套件目录本身（无 SKILL.md）不入表。
    // 旧实现只扫顶层，嵌套套件永远进不了 extensions 表，还会把套件目录误登记为技能
    let skills = home.join(".tuvis").join("skills");
    if skills.is_dir() {
        for rel in scan_skill_dirs(&skills) {
            let path = skills.join(&rel);
            // 登记口径恒为**斜杠**相对路径（见上「id/name 均带斜杠路径」）：
            // Windows 下 scan_skill_dirs 产出原生 `\`，这里统一转 `/`，保证 id 跨平台一致
            //（预设绑定/资源卡片按 id 匹配，反斜杠 id 在 macOS 导入的预设里永远对不上）。
            // 存量旧行（反斜杠 id）就地迁移，避免同一资源双行（斜杠新行 + 反斜杠旧行）
            let slashed = rel.replace('\\', "/");
            if slashed != rel {
                if let Err(err) = crate::database::migrate_legacy_extension_id(
                    &format!("skill-{}", rel),
                    &format!("skill-{}", slashed),
                    &slashed,
                ) {
                    log::warn!("回填 skill {} 旧 id 迁移失败: {}", rel, err);
                }
            }
            // no-swallowed-errors（评审裁决 3）：回填失败逐条报告，不再吞错
            if let Err(err) = crate::database::ensure_extension(&crate::database::ExtensionRecord {
                id: format!("skill-{}", slashed),
                kind: "skill".to_string(),
                name: slashed.clone(),
                description: None,
                source_path: path.to_string_lossy().to_string(),
                source_url: None,
                version: None,
                tags: None,
                suite: None,
                source_tool: None,
                is_native: false,
            }) {
                log::warn!("回填 skill {} 登记 extensions 失败: {}", rel, err);
            }
        }
    }

    // mcp：~/.tuvis/mcp/<stem>.json
    let mcp = home.join(".tuvis").join("mcp");
    if mcp.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&mcp) {
            for e in entries.flatten() {
                let path = e.path();
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    // no-swallowed-errors（评审裁决 3）：回填失败逐条报告，不再吞错
                    if let Err(err) =
                        crate::database::ensure_extension(&crate::database::ExtensionRecord {
                            id: format!("mcp-{}", stem),
                            kind: "mcp".to_string(),
                            name: stem.to_string(),
                            description: None,
                            source_path: path.to_string_lossy().to_string(),
                            source_url: None,
                            version: None,
                            tags: None,
                            suite: None,
                            source_tool: None,
                            is_native: false,
                        })
                    {
                        log::warn!("回填 mcp {} 登记 extensions 失败: {}", stem, err);
                    }
                }
            }
        }
    }

    // 陈旧行修剪（wave33 Item 3，修「预设编辑名单重复」根因）：source_path 非空
    // 且路径不存在 → 死行——与新登记行并存会让预设编辑弹窗双列。判定口径：
    // Path::exists()（跟随 symlink，死链同样判不存在）。删除该行并连带清
    // extension_assignments / tool_residents / resource_bindings（防孤儿）；
    // is_native 的 source_path 指向工具目录、MCP 行指向 ~/.tuvis/mcp/*.json、
    // 插件行指向插件目录——统一按存在性判定，不做 kind 特判
    let mut pruned = 0usize;
    for ext in crate::database::list_extensions() {
        if ext.source_path.is_empty() {
            continue;
        }
        if std::path::Path::new(&ext.source_path).exists() {
            continue;
        }
        if let Err(e) = crate::database::delete_extension(&ext.id) {
            log::warn!("修剪 {} 删除 extensions 行失败: {}", ext.id, e);
            continue;
        }
        // 连带清理全部 best-effort：单条失败只降日志，不阻断其余修剪
        if let Err(e) = crate::database::delete_assignments_for(&ext.id) {
            log::warn!("修剪 {} 连带清 assignments 失败: {}", ext.id, e);
        }
        if let Err(e) = crate::database::delete_tool_residents_for(&ext.id) {
            log::warn!("修剪 {} 连带清 tool_residents 失败: {}", ext.id, e);
        }
        if let Err(e) = crate::database::delete_resource_binding(&ext.id) {
            log::warn!("修剪 {} 连带清 resource_bindings 失败: {}", ext.id, e);
        }
        log::info!(
            "注册表回填：修剪陈旧行 {}（source_path 失效: {}）",
            ext.id,
            ext.source_path
        );
        pruned += 1;
    }
    if pruned > 0 {
        log::info!("注册表回填：共修剪 {} 条 source_path 失效的陈旧行", pruned);
    }
}

#[cfg(test)]
mod import_plan_tests {
    use super::*;

    #[test]
    fn new_name_imports_and_links() {
        let seen = std::collections::HashSet::new();
        assert_eq!(
            plan_skill_import("foo", &seen, None),
            SkillImportPlan::ImportAndLink
        );
    }

    #[test]
    fn known_name_links_second_tool() {
        let seen: std::collections::HashSet<String> = ["foo".to_string()].into_iter().collect();
        assert_eq!(
            plan_skill_import("foo", &seen, None),
            SkillImportPlan::LinkOnly
        );
        assert_eq!(
            plan_skill_import("foo", &seen, Some(true)),
            SkillImportPlan::LinkOnly
        );
    }

    #[test]
    fn known_name_respects_explicit_disable() {
        let seen: std::collections::HashSet<String> = ["foo".to_string()].into_iter().collect();
        assert_eq!(
            plan_skill_import("foo", &seen, Some(false)),
            SkillImportPlan::Skip
        );
    }
}

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
        assert!(found
            .iter()
            .all(|(p, _)| p.components().count() <= tmp.path().components().count() + 5));
    }
}

// ---- P0-1 回归锁：停用工具的补链不得重建 ----
// W5 停用把工具 skill 目录还原为真实目录；启动补链若不查工具勾选状态，会把真实目录
// remove_link 删除并重建链接（用户就地修改静默丢失）。评审复核时测试清单恰缺此用例
#[cfg(test)]
mod relink_gate_tests {
    use super::ensure_skill_relink;

    #[test]
    fn disabled_tool_never_relinks() {
        let enable_called = std::cell::Cell::new(0);
        let out = ensure_skill_relink(
            "workbuddy",
            &|tool| tool != "workbuddy", // workbuddy 已停用
            &|| false,                   // 还原出的真实目录：非链接态
            &|| {
                enable_called.set(enable_called.get() + 1);
                Ok(())
            },
        );
        assert_eq!(out, Ok(false));
        assert_eq!(
            enable_called.get(),
            0,
            "停用工具不得触发补链（否则删除用户还原目录）"
        );
    }

    #[test]
    fn enabled_unlinked_tool_relinks() {
        let out = ensure_skill_relink("claude", &|_| true, &|| false, &|| Ok(()));
        assert_eq!(out, Ok(true));
    }

    #[test]
    fn already_linked_skips_relink() {
        let out = ensure_skill_relink("claude", &|_| true, &|| true, &|| Ok(()));
        assert_eq!(out, Ok(false));
    }

    #[test]
    fn enable_failure_propagates() {
        let out = ensure_skill_relink("claude", &|_| true, &|| false, &|| Err("boom".into()));
        assert_eq!(out, Err("boom".into()));
    }
}

#[cfg(test)]
mod detect_source_tool_tests {
    use super::detect_source_tool;

    /// 债修复回归锁：溯源遍历 TOOL_IDS 注册表——kimi/workbuddy/zcode 来源路径
    /// 不再回溯恒 None（原四工具硬编码清单的缺口）。纯路径前缀比较，零文件系统
    /// 访问（primary_skill_dir 只做 home_dir 路径拼接）
    #[test]
    fn registry_covers_late_registered_tools() {
        let home = dirs::home_dir().unwrap_or_default();
        let case = |rel: &str, expect: &str| {
            let path = home
                .join(rel)
                .join("my-skill")
                .to_string_lossy()
                .to_string();
            assert_eq!(
                detect_source_tool(&path).as_deref(),
                Some(expect),
                "源路径 {path} 应回溯到 {expect}"
            );
        };
        case(".kimi-code/skills", "kimi");
        case(".workbuddy/skills", "workbuddy");
        case(".zcode/skills", "zcode");
        // 既有工具零回归（原硬编码清单覆盖的四个）
        case(".claude/skills", "claude");
        case(".openclaw/skills", "openclaw");
        // codex 注册表已切至私有目录（spec 2026-09-09 §4.1）
        case(".codex/skills", "codex");
        // 共享目录 ~/.agents/skills 不再是 codex 激活目标：其来源回溯为 None
        // （新语义 = 共享目录发现的技能「入库不归属」，不参与补链）
        let agents_path = home
            .join(".agents/skills")
            .join("my-skill")
            .to_string_lossy()
            .to_string();
        assert_eq!(
            detect_source_tool(&agents_path),
            None,
            "共享目录 {agents_path} 不归属任何工具"
        );
        // 无关路径 → None
        assert_eq!(detect_source_tool("/tmp/nowhere/skill"), None);
    }
}

#[cfg(test)]
mod scan_skills_tests {
    use super::scan_skills_recursive;

    /// `.system` 排除锁（spec §4.1 / F2）：`~/.codex/skills/.system` 是 codex 官方
    /// 捆绑的系统技能（skill-creator / skill-installer / imagegen 等，随版本更新
    /// 生成），不入 兔维斯 SSOT。导入扫描是递归的（深度上限 4），`.system/<skill>/
    /// SKILL.md` 的两层结构会被命中，必须显式跳过 `.system` 条目连同其子树。
    /// tempdir fixture，零真实家目录访问
    #[test]
    fn scan_excludes_codex_system_bundled_skills() {
        let tmp = tempfile::tempdir().expect("创建临时目录失败");
        let root = tmp.path();

        // 用户技能：正常被发现
        std::fs::create_dir_all(root.join("foo")).unwrap();
        std::fs::write(
            root.join("foo").join("SKILL.md"),
            "---\nname: foo\ndescription: 用户技能\n---\n正文",
        )
        .unwrap();

        // 官方捆绑系统技能：两层结构，必须连同子树被排除
        std::fs::create_dir_all(root.join(".system").join("skill-creator")).unwrap();
        std::fs::write(
            root.join(".system").join("skill-creator").join("SKILL.md"),
            "---\nname: skill-creator\ndescription: 官方捆绑系统技能\n---\n正文",
        )
        .unwrap();

        let results = scan_skills_recursive(root, root, 0);
        let names: Vec<&str> = results.iter().map(|(_, name)| name.as_str()).collect();
        assert_eq!(
            names,
            vec!["foo"],
            "只应发现用户技能 foo，.system 子树整体排除"
        );
    }
}

// ---- 共享导入源测试（spec §4.2 / §7.7）：全 tempdir + 内存闭包，零真实 DB 与家目录访问 ----
#[cfg(test)]
mod shared_source_import_tests {
    use super::{
        import_skills_from_sources, SkillImportDeps, SkillScanSource, AGENTS_SHARED_SOURCE,
    };
    use crate::database::{AssignmentRecord, ExtensionRecord};
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// 内存版 DB / enable 依赖：list_extensions / list_assignments 提供播种与禁用查询，
    /// insert_extension / enable_skill / install_to_repo 只记录调用供断言
    #[derive(Default)]
    struct MemDeps {
        extensions: RefCell<Vec<ExtensionRecord>>,
        assignments: RefCell<HashMap<String, Vec<AssignmentRecord>>>,
        inserted: RefCell<Vec<ExtensionRecord>>,
        enable_calls: RefCell<Vec<(String, String)>>,
    }

    /// 在测试函数体内展开构造 SkillImportDeps：闭包临时值以 let 绑定存活到测试块
    /// 结束（不能从 helper 函数返回引用其局部闭包的结构，E0515）
    macro_rules! mem_deps {
        ($mem:expr) => {
            SkillImportDeps {
                list_extensions: &|| $mem.extensions.borrow().to_vec(),
                list_assignments: &|tool_id| {
                    $mem.assignments
                        .borrow()
                        .get(tool_id)
                        .cloned()
                        .unwrap_or_default()
                },
                insert_extension: &|ext| $mem.inserted.borrow_mut().push(ext.clone()),
                install_to_repo: &|_source, _name, _overwrite| Ok(()),
                enable_skill: &|skill_name, tool_id| {
                    $mem.enable_calls
                        .borrow_mut()
                        .push((skill_name.to_string(), tool_id.to_string()));
                    Ok(())
                },
            }
        };
    }

    /// 写入一个合法手装技能（SKILL.md frontmatter 同 Task 1 fixture 要求）
    fn write_skill(root: &std::path::Path, name: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {}\ndescription: 手装共享技能\n---\n正文", name),
        )
        .unwrap();
    }

    /// 构造一条已入库的 skill 记录（供增量播种 seen_names）
    fn ext_record(name: &str) -> ExtensionRecord {
        ExtensionRecord {
            id: format!("skill-{}", name),
            kind: "skill".to_string(),
            name: name.to_string(),
            description: None,
            source_path: format!("/tmp/{}", name),
            source_url: None,
            version: None,
            tags: None,
            suite: None,
            source_tool: None,
            is_native: false,
        }
    }

    /// §7.7a：agents 共享源发现的技能「入库不归属」——insert 的记录
    /// source_tool=None、tags=agents-shared，且从不为任何工具建链
    #[test]
    fn agents_shared_source_imports_without_binding() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join("skills");
        write_skill(&agents_dir, "handmade");

        let sources = vec![SkillScanSource::Shared { dir: agents_dir }];
        let mem = MemDeps::default();
        let deps = mem_deps!(mem);
        let (imported, skipped_dup, source_counts) =
            import_skills_from_sources(&sources, false, &deps);

        assert_eq!(imported, 1);
        assert_eq!(skipped_dup, 0);
        let inserted = mem.inserted.borrow();
        assert_eq!(inserted.len(), 1);
        assert_eq!(inserted[0].id, "skill-handmade");
        assert_eq!(inserted[0].kind, "skill");
        assert_eq!(inserted[0].source_tool, None, "共享源入库不归属任何工具");
        assert_eq!(
            inserted[0].tags.as_deref(),
            Some(AGENTS_SHARED_SOURCE),
            "共享源 tags 记 agents-shared"
        );
        assert!(
            mem.enable_calls.borrow().is_empty(),
            "共享源 ImportAndLink 分支不得建链"
        );
        assert!(source_counts.contains(&(AGENTS_SHARED_SOURCE.to_string(), 1)));
    }

    /// §7.7b（同源判定）：技能已在库（seen）→ LinkOnly 分支，但共享源不归属
    /// 任何工具 → 补链 no-op：enable 不被调、imported 不增、不重复 insert
    #[test]
    fn agents_shared_seen_skill_stays_silent() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join("skills");
        write_skill(&agents_dir, "handmade");

        let sources = vec![SkillScanSource::Shared { dir: agents_dir }];
        let mem = MemDeps::default();
        mem.extensions.borrow_mut().push(ext_record("handmade"));
        let deps = mem_deps!(mem);
        let (imported, skipped_dup, _) = import_skills_from_sources(&sources, false, &deps);

        assert_eq!(imported, 0, "seen 技能走 LinkOnly，不重导");
        assert_eq!(skipped_dup, 0, "LinkOnly 不是 Skip，不计数");
        assert!(mem.inserted.borrow().is_empty(), "不重复 insert");
        assert!(
            mem.enable_calls.borrow().is_empty(),
            "共享源 LinkOnly 分支静默 no-op，不得补链任何工具"
        );
    }

    /// 回归锁：tool 源与 agents 共享源并存时，tool 源行为不变——
    /// ImportAndLink：enable 被调 + source_tool/tags=Some(tool_id)；
    /// LinkOnly：补链照旧；共享源两分支均不建链
    #[test]
    fn tool_source_behavior_unchanged_alongside_agents_shared() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join("claude-skills");
        write_skill(&claude_dir, "tool-new");
        write_skill(&claude_dir, "tool-seen");
        let agents_dir = tmp.path().join("agents-skills");
        write_skill(&agents_dir, "handmade");

        let sources = vec![
            SkillScanSource::Tool {
                id: "claude".to_string(),
                dir: claude_dir,
            },
            SkillScanSource::Shared { dir: agents_dir },
        ];
        let mem = MemDeps::default();
        mem.extensions.borrow_mut().push(ext_record("tool-seen"));
        let deps = mem_deps!(mem);
        let (imported, skipped_dup, source_counts) =
            import_skills_from_sources(&sources, false, &deps);

        // tool-new（ImportAndLink）+ handmade（共享源 ImportAndLink）各计一次导入
        assert_eq!(imported, 2);
        assert_eq!(skipped_dup, 0);

        let inserted = mem.inserted.borrow();
        assert_eq!(inserted.len(), 2);
        let tool_new = inserted.iter().find(|e| e.name == "tool-new").unwrap();
        assert_eq!(
            tool_new.source_tool.as_deref(),
            Some("claude"),
            "工具源入库归属来源工具"
        );
        assert_eq!(tool_new.tags.as_deref(), Some("claude"));
        let handmade = inserted.iter().find(|e| e.name == "handmade").unwrap();
        assert_eq!(handmade.source_tool, None);
        assert_eq!(handmade.tags.as_deref(), Some(AGENTS_SHARED_SOURCE));

        // claude 两分支都建链（tool-new 导入建链 + tool-seen 补链）；共享源不建链
        let enables = mem.enable_calls.borrow();
        assert!(enables.contains(&("tool-new".to_string(), "claude".to_string())));
        assert!(enables.contains(&("tool-seen".to_string(), "claude".to_string())));
        assert_eq!(enables.len(), 2, "只有工具源建链，共享源静默");

        assert!(source_counts.contains(&("claude".to_string(), 2)));
        assert!(source_counts.contains(&(AGENTS_SHARED_SOURCE.to_string(), 1)));
    }
}
