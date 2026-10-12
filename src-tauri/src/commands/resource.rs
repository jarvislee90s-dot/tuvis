// 资源管理命令

// 递归技能扫描（深度 4 / SKILL.md 判定 / 跳软链点目录 / 结果排序）已上收至
// services::resource 与回填共用（预设编辑弹窗与资源视图同源口径）
use crate::services::resource::scan_skill_dirs;

/// 原生（未纳管）资源的扫描结果 DTO
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeExtensionRecord {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub description: Option<String>,
    pub source_path: String,
    pub source_tool: String,
    pub detected_at: String,
    pub imported: bool,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionWithAssignments {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub description: Option<String>,
    pub source_path: String,
    pub suite: Option<String>,
    pub source_tool: Option<String>,
    pub tags: Option<String>,
    /// 原生（未纳管）资源标记：预设编辑弹窗「原生技能」分组的数据源（T7）
    pub is_native: bool,
    pub assignments: Vec<AssignmentSummary>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssignmentSummary {
    pub agent_tool_id: String,
    pub enabled: bool,
    pub link_status: String,
}

#[tauri::command]
pub fn list_extensions_with_assignments() -> Vec<ExtensionWithAssignments> {
    let extensions = crate::database::list_extensions();
    let assignments = crate::database::list_all_assignments();
    extensions
        .iter()
        .map(|ext| {
            let ext_assignments: Vec<AssignmentSummary> = assignments
                .iter()
                .filter(|a| a.extension_id == ext.id)
                .map(|a| AssignmentSummary {
                    agent_tool_id: a.agent_tool_id.clone(),
                    enabled: a.enabled,
                    link_status: a.link_status.clone(),
                })
                .collect();
            ExtensionWithAssignments {
                id: ext.id.clone(),
                kind: ext.kind.clone(),
                name: ext.name.clone(),
                description: ext.description.clone(),
                source_path: ext.source_path.clone(),
                suite: ext.suite.clone(),
                source_tool: ext.source_tool.clone(),
                tags: ext.tags.clone(),
                is_native: ext.is_native,
                assignments: ext_assignments,
            }
        })
        .collect()
}

#[tauri::command]
pub fn scan_native_resources(tool_id: String) -> Result<Vec<NativeExtensionRecord>, String> {
    // issue #36-1：与同文件 disable/enable_skill_for_tool 同守卫
    crate::services::tool_settings::ensure_tool_enabled(&tool_id)?;
    let mut results = Vec::new();
    let skill_dir = crate::adapter::primary_skill_dir(&tool_id);
    if let Some(dir) = skill_dir {
        if dir.exists() {
            let existing = crate::database::list_extensions();
            let skill_names = scan_skill_dirs(&dir);
            for name in skill_names {
                let path = dir.join(&name);
                let ext_id = format!("skill-{}", name);
                let exists = existing.iter().any(|e| e.id == ext_id);
                if !exists {
                    results.push(NativeExtensionRecord {
                        id: ext_id,
                        kind: "skill".to_string(),
                        name: name.clone(),
                        description: None,
                        source_path: path.to_string_lossy().to_string(),
                        source_tool: tool_id.clone(),
                        detected_at: chrono::Utc::now().to_rfc3339(),
                        imported: false,
                    });
                }
            }
        }
    }
    Ok(results)
}

#[tauri::command]
pub fn import_native_resources(
    items: Vec<(String, String, String)>,
) -> Result<crate::services::ImportStats, String> {
    // issue #36-1：按来源工具预检——任一禁用工具即整体拒绝，避免部分导入后状态混杂
    let mut seen = std::collections::HashSet::new();
    for (_, _, tool) in &items {
        if seen.insert(tool.clone()) {
            crate::services::tool_settings::ensure_tool_enabled(tool)?;
        }
    }
    let mut imported = 0;
    let mut skipped = 0;
    // frontmatter 专属预填建议（spec §6，2026-09-15 裁决：手动导入当场确认）：
    // 多项目导入只取首个命中项（brief 语义「弹一个提示」），其余命中项由
    // list_frontmatter_suggestions 进体检卡片兜底
    let mut suggestion = None;
    for (source_path, name, source_tool) in items {
        let path = std::path::Path::new(&source_path);
        if !path.exists() {
            skipped += 1;
            continue;
        }
        if let Err(e) = crate::linker::install_to_repo(path, &name, false) {
            log::warn!("导入 {} 失败: {}", name, e);
            skipped += 1;
            continue;
        }
        let ext = crate::database::ExtensionRecord {
            id: format!("skill-{}", name),
            kind: "skill".to_string(),
            name: name.clone(),
            description: None,
            source_path: source_path.clone(),
            source_url: None,
            version: None,
            tags: None,
            suite: None,
            source_tool: Some(source_tool.clone()),
            is_native: true,
        };
        if let Err(e) = crate::database::insert_extension(&ext) {
            log::warn!("原生技能 {} 登记失败: {}", name, e);
        }
        // 默认按来源工具自动创建工具目录链接，让 harness 立即读取 SSOT 中的 skill
        // 用户主动导入时，按来源工具自动把原生目录替换为 兔维斯 软链接
        if let Err(e) = crate::services::enable_skill_for_tool(&name, &source_tool) {
            log::warn!("导入 {} 后为 {} 创建链接失败: {}", name, source_tool, e);
        }
        imported += 1;
        // SSOT 内容已落位（install_to_repo 成功），读 SKILL.md 判定；
        // 判定失败（非技能/IO 错误）→ None，不阻断导入
        if suggestion.is_none() {
            suggestion = crate::services::resource::frontmatter::suggest_for_skill(&name);
        }
    }
    Ok(crate::services::ImportStats {
        imported,
        newly_added: imported,
        skipped_dup: skipped,
        source_counts: vec![],
        suggestion,
    })
}

/// frontmatter 存量「待确认专属建议」（spec §6/§13；2026-09-15 裁决：启动扫描
/// 只列建议、不自动写绑定表）。扫 SSOT 仓库顶层各技能目录的 SKILL.md，
/// frontmatter 命中且 resource_bindings 无该行 → 列出；单目录读取失败容错跳过
#[tauri::command]
pub fn list_frontmatter_suggestions(
) -> Vec<crate::services::resource::frontmatter::FrontmatterSuggestion> {
    let repo = crate::linker::ensure_repo_dir();
    let Ok(entries) = std::fs::read_dir(&repo) else {
        return Vec::new();
    };
    // 绑定表一次拉取（判定核心 frontmatter::suggestion_for_content 逐技能复用）
    let bound_ids: std::collections::HashSet<String> = crate::database::list_resource_bindings()
        .into_iter()
        .map(|b| b.extension_id)
        .collect();
    let mut suggestions: Vec<_> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                return None;
            }
            let content = std::fs::read_to_string(e.path().join("SKILL.md")).ok()?;
            crate::services::resource::frontmatter::suggestion_for_content(
                &name, &content, &bound_ids,
            )
        })
        .collect();
    suggestions.sort_by(|a, b| a.extension_id.cmp(&b.extension_id));
    suggestions
}

/// 生产迁移边界路径：从真实家目录构建四字段（测试用 tempdir 注入，见 migration 核心）
fn migration_paths() -> crate::services::resource::migration::MigrationPaths {
    let home = dirs::home_dir().unwrap_or_default();
    crate::services::resource::migration::MigrationPaths {
        agents_dir: home.join(".agents").join("skills"),
        codex_skills_dir: home.join(".codex").join("skills"),
        layer1_dir: home.join(".tuvis").join("skills"),
        layer2_root: home.join(".tuvis").join("active").join("codex"),
    }
}

/// 检测 .agents/skills 下指向 ~/.tuvis/active/codex 的 兔维斯 遗留链接（spec §4.3）。
/// 纯文件系统检测，零 DB 访问（F5：工具侧链接只是投影）
#[tauri::command]
pub fn detect_legacy_agents_links() -> Vec<String> {
    crate::services::resource::migration::detect_legacy_links(&migration_paths())
}

/// 执行迁移/保留（mode: "migrate" | "keep"），返回逐条报告。
/// 纯文件系统操作，DB assignments 与 Layer 2 完全不动（F5，spec §4.3）
#[tauri::command]
pub fn migrate_legacy_agents_links(
    mode: String,
) -> Result<Vec<crate::services::resource::migration::MigrationItemReport>, String> {
    if mode != "migrate" && mode != "keep" {
        return Err(format!("未知模式: {}（仅支持 migrate | keep）", mode));
    }
    Ok(crate::services::resource::migration::migrate_legacy_links(
        &migration_paths(),
        &mode,
    ))
}

#[tauri::command]
pub fn list_tool_resources(tool_id: String) -> serde_json::Value {
    let global = crate::database::list_extensions();
    // issue #36-1：scan 现带 W5 守卫（返回 Result）——本列表仅对已勾选工具展示，
    // 防御场景（禁用工具的直连调用）native 置空即可
    let native = scan_native_resources(tool_id.clone()).unwrap_or_default();
    let assignments = crate::database::list_assignments(&tool_id);

    // 补充 SSOT 仓库中已有但未在 DB extensions 中的 skill
    let mam_skills = dirs::home_dir()
        .unwrap_or_default()
        .join(".tuvis")
        .join("skills");
    let ssot_skill_names = scan_skill_dirs(&mam_skills);
    let mut global_with_status: Vec<_> = global
        .iter()
        .map(|e| {
            let assignment = assignments.iter().find(|a| a.extension_id == e.id);
            serde_json::json!({
                "id": e.id,
                "kind": e.kind,
                "name": e.name,
                "description": e.description,
                "sourcePath": e.source_path,
                "sourceTool": e.source_tool,
                "suite": e.suite,
                "tags": e.tags,
                "assignments": assignment.map(|a| vec![serde_json::json!({
                    "agentToolId": a.agent_tool_id,
                    "enabled": a.enabled,
                    "linkStatus": a.link_status,
                })]).unwrap_or_default(),
            })
        })
        .collect();

    // 补充 SSOT 中的 skill（不在 DB extensions 里的）
    for name in &ssot_skill_names {
        let ext_id = format!("skill-{}", name);
        if !global_with_status
            .iter()
            .any(|g| g["id"].as_str() == Some(&ext_id))
        {
            let assignment = assignments.iter().find(|a| a.extension_id == ext_id);
            global_with_status.push(serde_json::json!({
                "id": ext_id,
                "kind": "skill",
                "name": name,
                "description": null,
                "sourcePath": mam_skills.join(name).to_string_lossy(),
                "sourceTool": null,
                "suite": null,
                "tags": null,
                "assignments": assignment.map(|a| vec![serde_json::json!({
                    "agentToolId": a.agent_tool_id,
                    "enabled": a.enabled,
                    "linkStatus": a.link_status,
                })]).unwrap_or_default(),
            }));
        }
    }

    serde_json::json!({ "global": global_with_status, "native": native })
}

/// 账本-磁盘对账扫描（spec §13）：返回 enabled 工具的 L1-L4 漂移清单
#[tauri::command]
pub fn scan_ledger_drift() -> Vec<crate::services::resource::reconcile::DriftItem> {
    crate::services::resource::reconcile::scan_drift()
}

/// 单条对账处置（spec §13）：mode "a" 账本为准修磁盘 | "b" 磁盘为准回写账本
#[tauri::command]
pub fn reconcile_item(
    item: crate::services::resource::reconcile::DriftItem,
    mode: String,
) -> crate::services::resource::reconcile::ReconcileOutcome {
    crate::services::resource::reconcile::reconcile_one(&item, &mode)
}

/// 批量对账处置（spec §13）：该工具全部漂移逐条按同一 mode 处置（L4 恒 needs_manual；单条失败不中断）
#[tauri::command]
pub fn reconcile_tool_batch(
    tool_id: String,
    mode: String,
) -> Vec<crate::services::resource::reconcile::ReconcileOutcome> {
    crate::services::resource::reconcile::reconcile_tool_batch(&tool_id, &mode)
}

/// 空目录扫描（wave33 Item 2）：兔维斯 skill 仓库与启用工具 skill 目录中的
/// 可清理空目录清单
#[tauri::command]
pub fn scan_empty_dirs() -> Vec<crate::services::resource::reconcile::EmptyDirItem> {
    crate::services::resource::reconcile::scan_empty_dirs()
}

/// 空目录清理（wave33 Item 2）：白名单根校验（越界 Err）+ 自底向上反复
/// remove_dir；非空/不存在跳过；返回实际删除数
#[tauri::command]
pub fn clean_empty_dirs(paths: Vec<String>) -> Result<usize, String> {
    crate::services::resource::reconcile::clean_empty_dirs(paths)
}

#[tauri::command]
pub fn check_preset_compatibility(
    preset_id: String,
    tool_id: String,
) -> crate::services::preset::CompatibilityReport {
    crate::services::preset::check_compatibility(&preset_id, &tool_id)
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SsotResource {
    pub name: String,
    pub kind: String,
    pub enabled_tools: Vec<String>,
    #[serde(rename = "brokenTools")]
    pub broken_tools: Vec<String>,
    /// plugin 子类型（file | config），仅 kind == "plugin" 时有值
    #[serde(rename = "pluginType", skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
    /// MCP 条目在存储 JSON 里带 enable:false（工具侧停用标记原样入库，M7 口径：
    /// 禁用配置也是有效资产、照常导入，UI 如实标记「源已停用」）。仅 kind == "mcp"
    /// 且为 true 时序列化
    #[serde(rename = "sourceDisabled", skip_serializing_if = "std::ops::Not::not")]
    pub source_disabled: bool,
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
    let mam = dirs::home_dir().unwrap_or_default().join(".tuvis");
    let assignments = crate::database::list_all_assignments();
    let extensions = crate::database::list_extensions();

    // W5：工具列与已勾选工具求交集 — 未勾选工具的「按资源分布」列不返回，
    // 分配数据本身保留在 DB（重新勾选后按原分配恢复显示）
    let enabled_ids: std::collections::HashSet<String> =
        crate::database::dao::agent_tool::enabled_tool_ids()
            .into_iter()
            .collect();

    // 构建工具 → skill 目录映射，用于检测原生生效的 skill（仅已勾选工具参与）
    let tool_skill_dirs: Vec<(&str, std::path::PathBuf)> = crate::adapter::all_adapters_with_ids()
        .into_iter()
        .filter(|(id, _)| enabled_ids.contains(*id))
        .filter_map(|(id, a)| a.skill_dirs().into_iter().next().map(|d| (id, d)))
        .collect();

    let scan_skills = |dir: &std::path::Path| -> Vec<SsotResource> {
        let names = scan_skill_dirs(dir);
        names
            .into_iter()
            .map(|name| {
                let ext_id = format!("skill-{}", name);
                // 1) DB 中有 enabled=true 的记录（仅已勾选工具的列）
                let mut enabled_tools: Vec<String> = assignments
                    .iter()
                    .filter(|a| a.extension_id == ext_id && a.enabled)
                    .filter(|a| enabled_ids.contains(&a.agent_tool_id))
                    .map(|a| a.agent_tool_id.clone())
                    .collect();
                // 2) 补充：检查各工具原生 skill 目录中是否存在（非符号链接的实际目录也算已生效）。
                //    按拍平名探测（派发拍平，2026-09-17 裁决）——嵌套名的派发落点是拍平链接
                for (tool_id, tool_dir) in &tool_skill_dirs {
                    if enabled_tools.iter().any(|t| t == tool_id) {
                        continue;
                    }
                    if crate::linker::dispatch_target(tool_dir, &name).exists() {
                        enabled_tools.push(tool_id.to_string());
                    }
                }
                // 断链检测：DB 中 enabled 且链接状态为 dangling 的工具（仅已勾选工具的列）
                let broken_tools: Vec<String> = assignments
                    .iter()
                    .filter(|a| {
                        a.extension_id == ext_id && a.enabled && a.link_status == "dangling"
                    })
                    .filter(|a| enabled_ids.contains(&a.agent_tool_id))
                    .map(|a| a.agent_tool_id.clone())
                    .collect();
                SsotResource {
                    name,
                    kind: "skill".to_string(),
                    enabled_tools,
                    broken_tools,
                    plugin_type: None,
                    source_disabled: false,
                }
            })
            .collect()
    };

    // 构建工具 → MCP 配置路径映射，用于扫描各工具已有的 MCP 服务器（仅已勾选工具参与）
    let tool_mcp_configs: Vec<(
        &str,
        std::path::PathBuf,
        crate::adapter::McpFormat,
        &'static [&'static str],
    )> = crate::adapter::all_adapters_with_ids()
        .into_iter()
        .filter(|(id, _)| enabled_ids.contains(*id))
        .filter_map(|(id, a)| {
            let path = a.mcp_config_path()?;
            Some((id, path, a.mcp_format(), a.mcp_json_section()))
        })
        .collect();

    // MCP 扫描：以 ~/.tuvis/mcp/ 为基础数据源，工具配置文件仅作补充
    let scan_mcp = || -> Vec<SsotResource> {
        let mut all_mcps: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();

        // 1) 从 ~/.tuvis/mcp/ 目录扫描 SSOT 管理的 MCP（排除 DB assignment 中已禁用的）。
        //    M7：存储体里带 enable:false（导入时原样保留的工具侧停用标记）→ 记入
        //    source_disabled 供 UI「源已停用」标记
        let mut mcp_source_disabled: std::collections::BTreeMap<String, bool> =
            std::collections::BTreeMap::new();
        let mcp_repo = mam.join("mcp");
        if let Ok(entries) = std::fs::read_dir(&mcp_repo) {
            for entry in entries.flatten() {
                let fname = entry.file_name().to_string_lossy().to_string();
                if fname.ends_with(".json") {
                    let name = fname.strip_suffix(".json").unwrap_or(&fname).to_string();
                    if !name.starts_with('.') {
                        all_mcps.entry(name.clone()).or_default();
                        let disabled = std::fs::read_to_string(entry.path())
                            .ok()
                            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
                            .map(|v| crate::services::mcp::entry_disabled_by_tool(&v))
                            .unwrap_or(false);
                        if disabled {
                            mcp_source_disabled.insert(name, true);
                        }
                    }
                }
            }
        }

        // 2) 从各工具配置文件中读取已有 MCP（补充 SSOT 中尚未记录的）。
        //    段定位走 adapter 声明的键路径（ZCode=mcp.servers 嵌套子树），
        //    兼容既有工具历史形态的顶层键探测
        for (tool_id, config_path, format, section) in &tool_mcp_configs {
            let content = std::fs::read_to_string(config_path).unwrap_or_default();
            let servers: serde_json::Value = match format {
                crate::adapter::McpFormat::Json | crate::adapter::McpFormat::Jsonc => {
                    serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
                }
                crate::adapter::McpFormat::Toml => {
                    let toml_val: Result<toml::Value, _> = content.parse();
                    toml_val
                        .map(|v| {
                            let json_str = serde_json::to_string(&v).unwrap_or_default();
                            serde_json::from_str(&json_str).unwrap_or(serde_json::json!({}))
                        })
                        .unwrap_or(serde_json::json!({}))
                }
            };
            let mcp_obj = crate::services::mcp::json_section(&servers, section).or_else(|| {
                servers
                    .get("mcpServers")
                    .or_else(|| servers.get("mcp_servers"))
                    .or_else(|| servers.get("mcp"))
                    .and_then(|v| v.as_object())
            });
            if let Some(obj) = mcp_obj {
                for (name, value) in obj {
                    // ZCode 可选 enable:false 停用标记（无字段 = 启用）：停用条目
                    // 不计入该工具的启用列（如实展示为停用）；read_mcp_servers
                    // 原样透传条目（含 enable 字段）供面板展示
                    if crate::services::mcp::entry_disabled_by_tool(value) {
                        continue;
                    }
                    let entry = all_mcps.entry(name.clone()).or_default();
                    if !entry.contains(&tool_id.to_string()) {
                        entry.push((*tool_id).to_string());
                    }
                }
            }
        }

        // 3) 合并 DB assignment（覆盖工具配置文件扫描结果；仅已勾选工具的列）
        for assignment in assignments.iter().filter(|a| {
            a.extension_id.starts_with("mcp-") && enabled_ids.contains(&a.agent_tool_id)
        }) {
            let name = assignment.extension_id.strip_prefix("mcp-").unwrap_or("");
            let entry = all_mcps.entry(name.to_string()).or_default();
            if assignment.enabled {
                if !entry.contains(&assignment.agent_tool_id) {
                    entry.push(assignment.agent_tool_id.clone());
                }
            } else {
                entry.retain(|t| t != &assignment.agent_tool_id);
            }
        }

        let mut resources: Vec<SsotResource> = all_mcps
            .into_iter()
            .map(|(name, tools)| SsotResource {
                source_disabled: mcp_source_disabled.get(&name).copied().unwrap_or(false),
                name,
                kind: "mcp".to_string(),
                enabled_tools: tools,
                broken_tools: vec![],
                plugin_type: None,
            })
            .collect();
        resources.sort_by(|a, b| a.name.cmp(&b.name));
        resources
    };

    let scan_simple = |dir: &std::path::Path, kind: &str| -> Vec<SsotResource> {
        let mut resources = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                let ext_id = format!("{}-{}", kind, name);
                let enabled_tools: Vec<String> = assignments
                    .iter()
                    .filter(|a| a.extension_id == ext_id && a.enabled)
                    .filter(|a| enabled_ids.contains(&a.agent_tool_id))
                    .map(|a| a.agent_tool_id.clone())
                    .collect();
                // plugin 子类型从 DB tags 读出（file | config），缺失时前端回退 file
                let plugin_type = if kind == "plugin" {
                    extensions
                        .iter()
                        .find(|e| e.kind == "plugin" && e.name == name)
                        .and_then(|e| e.tags.clone())
                } else {
                    None
                };
                resources.push(SsotResource {
                    name,
                    kind: kind.to_string(),
                    enabled_tools,
                    broken_tools: vec![],
                    plugin_type,
                    source_disabled: false,
                });
            }
        }
        resources.sort_by(|a, b| a.name.cmp(&b.name));
        resources
    };

    SsotResources {
        skills: scan_skills(&mam.join("skills")),
        mcp: scan_mcp(),
        plugins: scan_simple(&mam.join("plugins"), "plugin"),
    }
}

/// 检测指定工具下所有在 SSOT 和原始目录中都存在的重复 skill
#[tauri::command]
pub fn detect_duplicate_skills(tool_id: String) -> Vec<String> {
    let repo = dirs::home_dir()
        .unwrap_or_default()
        .join(".tuvis")
        .join("skills");
    let Some(tool_skill_dir) = crate::adapter::primary_skill_dir(&tool_id) else {
        return Vec::new();
    };

    if !repo.exists() || !tool_skill_dir.exists() {
        return Vec::new();
    }

    let mut duplicates = Vec::new();
    let ssot_skills = scan_skill_dirs(&repo);
    for name in ssot_skills {
        // 工具目录侧按拍平名探测（派发拍平，2026-09-17 裁决：嵌套名的派发落点）
        let tool_path = crate::linker::dispatch_target(&tool_skill_dir, &name);
        if tool_path.exists() && !tool_path.is_symlink() {
            duplicates.push(name);
        }
    }
    duplicates.sort();
    duplicates
}

/// 清理指定工具下的重复 skill（delete 原始目录，替换为符号链接）
#[tauri::command]
pub fn cleanup_duplicate_skills(tool_id: String, names: Vec<String>) -> Result<(), String> {
    let repo = dirs::home_dir()
        .unwrap_or_default()
        .join(".tuvis")
        .join("skills");
    let tool_skill_dir = crate::adapter::primary_skill_dir(&tool_id)
        .ok_or_else(|| format!("未知工具: {}", tool_id))?;

    let mut cleaned = 0;
    let mut errors = Vec::new();

    for name in &names {
        // ssot 侧保持嵌套原路径；工具目录目标按拍平名（派发拍平，2026-09-17 裁决）
        let ssot_path = repo.join(name);
        let tool_path = crate::linker::dispatch_target(&tool_skill_dir, name);

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
        Err(format!(
            "部分清理失败 (成功 {}/{}): {}",
            cleaned,
            cleaned + errors.len(),
            errors.join("; ")
        ))
    } else {
        Ok(())
    }
}

/// 检查 skill 在工具目录中的类型：symlink | native | missing
#[tauri::command]
pub fn check_skill_target_type(tool_id: String, skill_name: String) -> String {
    let Some(tool_skill_dir) = crate::adapter::primary_skill_dir(&tool_id) else {
        return "missing".to_string();
    };
    // 工具目录目标按拍平名定位（派发拍平，2026-09-17 裁决）
    let target = crate::linker::dispatch_target(&tool_skill_dir, &skill_name);
    if !target.exists() {
        "missing".to_string()
    } else if target.is_symlink() {
        "symlink".to_string()
    } else {
        "native".to_string()
    }
}

/// 移除工具目录中的 skill 目标：链接直接移除（无数据可丢），原生目录移入系统回收站。
/// 回收站失败返回错误，绝不静默降级为永久删除。
fn remove_skill_target(target: &std::path::Path) -> Result<String, String> {
    let target_type = if target.is_symlink() {
        "symlink"
    } else {
        "native"
    };
    if target_type == "symlink" {
        crate::linker::remove_link(target)?;
    } else {
        trash::delete(target).map_err(|e| format!("移入回收站失败: {}", e))?;
    }
    Ok(target_type.to_string())
}

/// 取消 skill 的工具配置：回收站/移除链接 + 更新 DB
#[tauri::command]
pub fn disable_skill_for_tool(tool_id: String, skill_name: String) -> Result<String, String> {
    // W5：未勾选工具的资源管理操作直接拒绝（数据保留在 DB）
    crate::services::tool_settings::ensure_tool_enabled(&tool_id)?;
    let tool_skill_dir = crate::adapter::primary_skill_dir(&tool_id)
        .ok_or_else(|| format!("未知工具: {}", tool_id))?;
    // 工具目录目标按拍平名定位（派发拍平，2026-09-17 裁决）
    let target = crate::linker::dispatch_target(&tool_skill_dir, &skill_name);
    if !target.exists() && !target.is_symlink() {
        return Err("目标路径不存在".to_string());
    }

    let target_type = remove_skill_target(&target)?;
    let _ = crate::linker::layer3::cleanup_layer3_on_tool_disable(&skill_name, &tool_id);
    let _ = crate::linker::layer2::unlink_skill_from_layer2(&skill_name, &tool_id);
    let ext_id = format!("skill-{}", skill_name);
    let _ = crate::database::upsert_assignment(&ext_id, &tool_id, false, "missing");
    Ok(target_type)
}

/// 为工具启用 skill（创建符号链接 + DB 记录）
#[tauri::command]
pub fn enable_skill_for_tool_cmd(skill_name: String, tool_id: String) -> Result<(), String> {
    // W5：未勾选工具的资源管理操作直接拒绝（数据保留在 DB）
    crate::services::tool_settings::ensure_tool_enabled(&tool_id)?;
    crate::services::enable_skill_for_tool(&skill_name, &tool_id)
}

/// 从任意工具配置文件中提取 MCP 配置并保存到 SSOT 仓库
/// 扫描所有工具，找到第一个包含该 MCP 的配置文件，提取配置写入 ~/.tuvis/mcp/<name>.json。
/// enable:false 条目口径（M7，2026-09-11 拍板）：照常导入、原样保留 enable 字段——
/// 禁用的配置也是有效资产，值得入库备用；与扫描侧「停用条目不计入启用列」口径
/// 的差异是有意为之（展示如实、资产照收），UI 侧以「源已停用」标记区分
#[tauri::command]
pub fn import_mcp_to_ssot(mcp_name: String) -> Result<(), String> {
    let adapters = crate::adapter::all_adapters_with_ids();

    for (_tool_id, adapter) in &adapters {
        let config_path = match adapter.mcp_config_path() {
            Some(p) => p,
            None => continue,
        };
        let content = std::fs::read_to_string(&config_path).unwrap_or_default();
        let servers: serde_json::Value = match adapter.mcp_format() {
            crate::adapter::McpFormat::Json | crate::adapter::McpFormat::Jsonc => {
                serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
            }
            crate::adapter::McpFormat::Toml => {
                let toml_val: Result<toml::Value, _> = content.parse();
                toml_val
                    .map(|v| {
                        let json_str = serde_json::to_string(&v).unwrap_or_default();
                        serde_json::from_str(&json_str).unwrap_or(serde_json::json!({}))
                    })
                    .unwrap_or(serde_json::json!({}))
            }
        };
        let mcp_obj = crate::services::mcp::json_section(&servers, adapter.mcp_json_section())
            .and_then(|m| m.get(&mcp_name))
            .or_else(|| {
                servers
                    .get("mcpServers")
                    .or_else(|| servers.get("mcp_servers"))
                    .or_else(|| servers.get("mcp"))
                    .and_then(|v| v.get(&mcp_name))
            });

        if let Some(config) = mcp_obj {
            let repo = dirs::home_dir()
                .unwrap_or_default()
                .join(".tuvis")
                .join("mcp");
            let _ = std::fs::create_dir_all(&repo);
            let config_file = repo.join(format!("{}.json", mcp_name));
            let pretty = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
            std::fs::write(&config_file, &pretty).map_err(|e| e.to_string())?;
            log::info!(
                "MCP {} 配置已导入到 SSOT: {}",
                mcp_name,
                config_file.display()
            );
            // 预设 v2 数据源统一（spec §8.1）：MCP 入 SSOT 必须登记，否则预设列表看不到。
            // ensure_extension（INSERT OR IGNORE）：幂等重导入不抹掉用户编辑过的元数据（评审裁决 3）
            if let Err(e) = crate::database::ensure_extension(&crate::database::ExtensionRecord {
                id: format!("mcp-{}", mcp_name),
                kind: "mcp".to_string(),
                name: mcp_name.clone(),
                description: None,
                source_path: config_file.to_string_lossy().to_string(),
                source_url: None,
                version: None,
                tags: None,
                suite: None,
                source_tool: None,
                is_native: false,
            }) {
                log::warn!("MCP {} 登记 extensions 失败: {}", mcp_name, e);
            }
            return Ok(());
        }
    }

    Err(format!("未在任何工具配置中找到 MCP: {}", mcp_name))
}

/// 创建/更新 MCP 配置到 SSOT 仓库
#[tauri::command]
pub fn save_mcp_config(
    name: String,
    command: String,
    args: Vec<String>,
    env: std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
    let repo = dirs::home_dir()
        .unwrap_or_default()
        .join(".tuvis")
        .join("mcp");
    let _ = std::fs::create_dir_all(&repo);
    let config_file = repo.join(format!("{}.json", name));
    let config = serde_json::json!({
        "command": command,
        "args": args,
        "env": env,
    });
    let pretty = serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?;
    std::fs::write(&config_file, &pretty).map_err(|e| e.to_string())?;
    // 预设 v2 数据源统一（spec §8.1）：MCP 入 SSOT 必须登记，否则预设列表看不到。
    // 新建路径不该静默——登记失败以 `?` 传播，让前端拿到明确错误。
    // ensure_extension（INSERT OR IGNORE）：同名重保存不抹掉用户编辑过的元数据（评审裁决 3）
    crate::database::ensure_extension(&crate::database::ExtensionRecord {
        id: format!("mcp-{}", name),
        kind: "mcp".to_string(),
        name: name.clone(),
        description: None,
        source_path: config_file.to_string_lossy().to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: None,
        is_native: false,
    })?;
    log::info!("MCP 配置已保存: {}", config_file.display());
    Ok(())
}

/// 解析"工具 × 资源类型"对应的目标位置：skill → 工具 skills 目录、mcp → 配置文件、
/// plugin → 工具插件目录（adapter.plugin_dirs[0]，与启用插件的 symlink 目标同源）。
/// 返回 (路径, 是否为文件)；目录类调用方负责 create_dir_all。
/// mcp/plugin 的路径由 adapter 内部基于真实用户目录解析
fn resolve_tool_resource_path(
    tool_id: &str,
    kind: &str,
) -> Result<(std::path::PathBuf, bool), String> {
    match kind {
        "skill" => crate::adapter::primary_skill_dir(tool_id)
            .map(|p| (p, false))
            .ok_or_else(|| format!("未知工具: {}", tool_id)),
        "mcp" => crate::services::mcp::tool_mcp_config_path(tool_id).map(|p| (p, true)),
        "plugin" => {
            let adapter = crate::adapter::adapter_by_id(tool_id)
                .ok_or_else(|| format!("未知工具: {}", tool_id))?;
            let dir = adapter
                .plugin_dirs()
                .first()
                .ok_or_else(|| format!("工具 {} 不支持插件", tool_id))?
                .clone();
            Ok((dir, false))
        }
        _ => Err(format!("未知资源类型: {}", kind)),
    }
}

/// 用系统文件管理器打开目录（Explorer / Finder / xdg-open）
fn open_dir_in_system(path: &std::path::Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use crate::window::detached::{spawn_detached, ConsoleMode};
        // release 版是 windows_subsystem="windows" GUI 进程（无控制台），继承来的 std
        // 句柄不可用（真机症状：os error 6）。走统一出口
        // [`crate::window::detached::spawn_detached`]（Detached 臂显式置 null stdio，
        // 不再触碰继承句柄；根因机制与定案状态见该模块文档）；explorer 是 GUI 程序，
        // NO_WINDOW 位对它无效，取 Detached 只为与同族派发点同款
        let mut cmd = std::process::Command::new("explorer");
        cmd.arg(path);
        spawn_detached(&mut cmd, ConsoleMode::Detached)
            .map_err(|e| format!("打开目录失败: {}", e))?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(path)
            .spawn()
            .map_err(|e| format!("打开目录失败: {}", e))?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(path)
            .spawn()
            .map_err(|e| format!("打开目录失败: {}", e))?;
    }
    let _ = path;
    Ok(())
}

/// 用系统默认程序打开文件（如 MCP 配置 JSON/TOML）
fn open_file_in_system(path: &std::path::Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use crate::window::detached::{spawn_detached, ConsoleMode};
        // start 的首个带引号参数会被当作窗口标题，须补空标题；stdio 置 null 的原因
        // 同 open_dir_in_system（GUI 进程继承句柄不可用），走统一出口
        //（cmd 是控制台程序且不等回执 → Detached 臂顺带防闪黑框）
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "start", "", &path.to_string_lossy()]);
        spawn_detached(&mut cmd, ConsoleMode::Detached)
            .map_err(|e| format!("打开文件失败: {}", e))?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(path)
            .spawn()
            .map_err(|e| format!("打开文件失败: {}", e))?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(path)
            .spawn()
            .map_err(|e| format!("打开文件失败: {}", e))?;
    }
    let _ = path;
    Ok(())
}

/// 打开某工具对应资源的位置（资源管理器表头的目录定位按钮）：
/// skill → 工具 skills 目录（不存在则自动创建）；mcp → 配置文件（系统默认程序，
/// 不存在则报错）；plugin → 工具插件目录（同 symlink 目标，不存在则自动创建）。
/// 返回实际打开的路径供前端 toast 展示
#[tauri::command]
pub async fn open_tool_resource(tool_id: String, kind: String) -> Result<String, String> {
    // issue #36-1：未勾选工具的资源定位（含目录自动创建副作用）直接拒绝
    crate::services::tool_settings::ensure_tool_enabled(&tool_id)?;
    let (path, is_file) = resolve_tool_resource_path(&tool_id, &kind)?;
    if is_file {
        if !path.exists() {
            return Err(format!("配置文件尚未创建: {}", path.display()));
        }
        open_file_in_system(&path)?;
    } else {
        std::fs::create_dir_all(&path).map_err(|e| format!("创建目录失败: {}", e))?;
        open_dir_in_system(&path)?;
    }
    Ok(path.to_string_lossy().to_string())
}

/// reveal_dir 白名单**根**的唯一出处（2026-10-07 A2 抽出）：`~/.tuvis`、`~/.agents`，
/// 以及**导出目录**（A2 之后 = 系统下载目录，见 `commands/export.rs::exports_dir()`）。
///
/// 抽成独立函数是为了让「白名单根包含导出目录」这条不变量**可被零 IO 地测到** ——
/// 导出目录搬家时最容易漏的就是这里：落盘成功、提示「已保存」，用户点「打开所在目录」却被拒。
/// 用例见 `export.rs::reveal_whitelist_contains_exports_dir`。
///
/// ⚠️ 导出目录这一项**刻意是 `exports_dir()` 本身**（同一个函数），不是硬编码的 `~/Downloads`：
/// debug 构建下 `TUVIS_HOME` 会把导出目录重定向到 `$TUVIS_HOME/Downloads`，硬编码会让测试环境里
/// 「白名单放行的目录」与「实际落盘的目录」错开（正是本函数要防的那类缺陷）。
pub(crate) fn reveal_allowed_roots() -> Vec<std::path::PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    vec![
        home.join(".tuvis"),
        home.join(".agents"),
        crate::commands::export::exports_dir(),
    ]
}

/// reveal_dir 白名单校验核（wave33 Item 4，可测）：canonicalize 后必须以
/// ~/.tuvis、~/.agents 或导出目录（A2 后 = 系统下载目录）为前缀
/// （安全白名单——前端快捷跳转只允许 兔维斯 管辖目录与**用户自己的导出落点**）。
/// 词法预检 + canonicalize 复核双道（对不存在的路径 canonicalize 失败 → 报
/// 不存在；越界路径无论存在与否一律拒绝）
pub fn ensure_reveal_allowed(path: &str) -> Result<std::path::PathBuf, String> {
    let raw_roots = reveal_allowed_roots();
    let p = std::path::Path::new(path);
    let lexically_allowed = raw_roots.iter().any(|r| p.starts_with(r));
    // canonicalize 对不存在路径失败 → 明确报不存在（而非静默放行/误报越界）
    let canonical = std::fs::canonicalize(p).map_err(|_| format!("路径不存在: {}", path))?;
    let root_canons: Vec<std::path::PathBuf> = raw_roots
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .collect();
    if !lexically_allowed && !root_canons.iter().any(|r| canonical.starts_with(r)) {
        return Err(format!(
            "路径不在允许打开的范围（~/.tuvis、~/.agents 或导出目录）: {}",
            path
        ));
    }
    // symlink 穿透复核：词法命中但 canonicalize 落到白名单外 → 拒绝
    if !root_canons.iter().any(|r| canonical.starts_with(r)) {
        return Err(format!(
            "路径不在允许打开的范围（~/.tuvis、~/.agents 或导出目录）: {}",
            path
        ));
    }
    Ok(canonical)
}

/// 快捷跳转（wave33 Item 4，前端波消费）：用系统文件管理器/默认程序打开
/// 白名单内的目录或文件——打开机制照抄 open_tool_resource（目录走
/// open_dir_in_system，文件走 open_file_in_system），但加了 ~/.tuvis / ~/.agents
/// 前缀白名单（open_tool_resource 的目标由 adapter 推导、天然受限；本命令
/// 接受任意路径，必须显式校验）
#[tauri::command]
pub fn reveal_dir(path: String) -> Result<(), String> {
    let target = ensure_reveal_allowed(&path)?;
    if target.is_dir() {
        open_dir_in_system(&target)
    } else {
        open_file_in_system(&target)
    }
}

#[cfg(test)]
mod remove_target_tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn symlink_target_removed_directly() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let link = tmp.path().join("skill-link");
        junction::create(&src, &link).unwrap();
        assert!(link.is_symlink());
        let ty = remove_skill_target(&link).unwrap();
        assert_eq!(ty, "symlink");
        assert!(!link.exists() && !link.is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_target_removed_directly() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let link = tmp.path().join("skill-link");
        std::os::unix::fs::symlink(&src, &link).unwrap();
        let ty = remove_skill_target(&link).unwrap();
        assert_eq!(ty, "symlink");
        assert!(!link.exists() && !link.is_symlink());
    }
}

#[cfg(test)]
mod resolve_tool_resource_tests {
    use super::*;

    #[test]
    fn skill_resolves_to_tool_skills_dir() {
        let home = dirs::home_dir().unwrap_or_default();
        let (p, is_file) = resolve_tool_resource_path("claude", "skill").unwrap();
        assert_eq!(p, home.join(".claude").join("skills"));
        assert!(!is_file);
    }

    #[test]
    fn plugin_resolves_to_tool_plugin_dir() {
        let home = dirs::home_dir().unwrap_or_default();
        let (p, is_file) = resolve_tool_resource_path("claude", "plugin").unwrap();
        assert_eq!(p, home.join(".claude").join("plugins"));
        assert!(!is_file);
    }

    #[test]
    fn mcp_resolves_to_config_file() {
        let home = dirs::home_dir().unwrap_or_default();
        let (p, is_file) = resolve_tool_resource_path("claude", "mcp").unwrap();
        assert_eq!(p, home.join(".claude.json"));
        assert!(is_file);
    }

    #[test]
    fn unknown_tool_or_kind_is_rejected() {
        assert!(resolve_tool_resource_path("nope", "skill").is_err());
        assert!(resolve_tool_resource_path("claude", "nope").is_err());
    }
}
