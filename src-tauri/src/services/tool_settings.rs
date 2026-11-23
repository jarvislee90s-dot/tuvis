// 工具勾选管理（spec W5）：查询（含 managed 标志）与保存时的清理/重建。
// 取消勾选 = skill/文件型插件的「兔维斯 链接」还原为真实文件 + 兔维斯 管理的 MCP 条目移除 + 未读卡清除；
// SSOT 仓库与 DB 分配关系全部保留（禁止删除）；重新勾选按原分配幂等重建。

use crate::database::dao::{agent_tool, extension};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSetting {
    pub tool_id: String,
    pub name: String,
    pub enabled: bool,
    pub installed: bool,
    pub managed: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSettingChange {
    pub tool_id: String,
    pub enabled: bool,
}

#[derive(Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyResult {
    pub restored: Vec<String>,
    pub restored_mcps: Vec<String>,
    pub rebuild_failed: Vec<String>,
    /// 未能还原但现场完好（SSOT 缺失/暂存失败/移除链接失败，链接保持不变）——
    /// 保存结果中逐项报告（spec W5 清理语义 1 + §9）
    pub skipped_kept: Vec<String>,
    /// 还原中断且现场已失（移除链接成功后落位失败、且重建链接恢复也失败，
    /// 工具目录空缺）——逐项报告并提示需重新勾选重建（issue #36-4）
    pub skipped_lost: Vec<String>,
}

pub fn get_tool_settings() -> Vec<ToolSetting> {
    agent_tool::ensure_tool_rows();
    crate::adapter::TOOL_IDS
        .iter()
        .filter_map(|id| {
            let adapter = crate::adapter::adapter_by_id(id)?;
            Some(ToolSetting {
                tool_id: id.to_string(),
                name: adapter.name().to_string(),
                enabled: agent_tool::get_tool_enabled(id),
                // issue #36-7：与 detect_all_tools 同口径（dir OR CLI），装了 CLI
                // 但从未运行过（无 base 目录）的工具不再显示「未检测到」
                installed: crate::linker::detector::is_tool_installed(adapter.as_ref()),
                managed: tool_has_managed_content(id),
            })
        })
        .collect()
}

/// managed = 该工具存在启用的分配（skill/文件型插件链接或 MCP 条目）
fn tool_has_managed_content(tool_id: &str) -> bool {
    extension::list_all_assignments()
        .iter()
        .any(|a| a.agent_tool_id == tool_id && a.enabled)
}

/// toggle 类命令守卫：未勾选工具的资源管理操作直接拒绝
/// （spec W5「enable/disable 类命令返回明确错误」；仅守卫命令入口，
/// get_tool_settings / apply_tool_changes 本身不走此守卫，保证工具可重新开启）
/// 连接参数变体（review F4：内存库可测）
pub fn ensure_tool_enabled_conn(conn: &rusqlite::Connection, tool_id: &str) -> Result<(), String> {
    if agent_tool::get_tool_enabled_conn(conn, tool_id) {
        Ok(())
    } else {
        // issue #36-3：错误码 + 参数（前端 i18n 渲染），后端不再硬编码中文文案；
        // 前端 formatInvokeError（src/lib/invokeError.ts）据此映射本地化文案
        Err(format!("W5_TOOL_DISABLED:{}", tool_id))
    }
}

pub fn ensure_tool_enabled(tool_id: &str) -> Result<(), String> {
    let conn = crate::database::connection::DB.lock().unwrap();
    ensure_tool_enabled_conn(&conn, tool_id)
}

pub fn apply_tool_changes(changes: Vec<ToolSettingChange>) -> ApplyResult {
    apply_tool_changes_with(changes, &crate::services::preset::restore_tool)
}

/// 可注入缝隙（照 sync_imported_skill_links_with 先例）：restore 结果可注入，
/// 供测试验证取消勾选分支对前置恢复失败的防御
pub fn apply_tool_changes_with(
    changes: Vec<ToolSettingChange>,
    restore_preset: &dyn Fn(&str) -> Result<crate::services::preset::RestoreResult, String>,
) -> ApplyResult {
    let mut result = ApplyResult::default();
    for c in &changes {
        let was = agent_tool::get_tool_enabled(&c.tool_id);
        // 状态未变跳过
        if was == c.enabled {
            continue;
        }
        if !c.enabled {
            // 预设 v2（spec §5.4）：取消勾选前先恢复基底——若有激活预设，
            // 暂存的原生技能与独占停用项必须先归位，再做 W5 还原清理。
            // 恢复失败（Err 仅发生在文件已归位后的快照销账步，DB 级）→ 不清理、
            // 不落 disabled，工具保持启用，重试可愈；Ok（conflicts 是软性报告）
            // → 照常清理（评审裁决 2 + Minor 2 文案精度）
            if let Err(e) = restore_preset(&c.tool_id) {
                log::warn!("取消勾选 {} 前恢复基底失败，工具保持启用: {}", c.tool_id, e);
                result.skipped_kept.push(format!(
                    "{}: 预设基底已还原但快照销账失败（陈旧基底），工具保持启用，重试可愈",
                    c.tool_id
                ));
                continue;
            }
            // 取消勾选：清理为 best-effort（跳过项逐项报告，spec §9），随后落 DB
            disable_tool_cleanup(&c.tool_id, &mut result);
            agent_tool::set_tool_enabled(&c.tool_id, false);
        } else {
            // issue #36-8：文件操作成功后再写 DB——重建全部成功才置 enabled；
            // 部分失败则回滚本次产物（对刚建的链接/MCP 条目执行一次停用清理，
            // 恰为 W5 还原语义）并保持 DB 未启用，用户重新勾选即整体幂等重试，
            // 不再需要「先关再开」。重建链路不读工具启用态（已逐一核验
            // enable_skill_for_tool / assign_skill_to_subagent / toggle_plugin /
            // write_mcp），故先文件后 DB 的次序对重建链路无影响。
            // 门控只看本次工具的失败增量：rebuild_failed 在整个批量保存中跨工具
            // 累积，直接 is_empty() 会让前面工具的失败错误回滚后面成功的工具
            let failed_before = result.rebuild_failed.len();
            rebuild_tool_links(&c.tool_id, &mut result);
            if enable_allowed(failed_before, result.rebuild_failed.len()) {
                agent_tool::set_tool_enabled(&c.tool_id, true);
            } else {
                disable_tool_cleanup(&c.tool_id, &mut result);
            }
        }
    }
    result
}

/// 启用门控纯核（review Critical 回归锁）：仅当本次重建零新增失败才置 enabled。
/// rebuild_failed 跨工具累积，门控必须对比本次前后的失败数而非判全表为空
fn enable_allowed(failed_before: usize, failed_after: usize) -> bool {
    failed_after == failed_before
}

/// 取消勾选：链接还原 + MCP 条目移除 + 未读卡清除（spec W5 清理语义）
fn disable_tool_cleanup(tool_id: &str, result: &mut ApplyResult) {
    let home = dirs::home_dir().unwrap_or_default();
    let assignments: Vec<_> = extension::list_all_assignments()
        .into_iter()
        .filter(|a| a.agent_tool_id == tool_id && a.enabled)
        .collect();
    let extensions = extension::list_extensions();

    for a in &assignments {
        // MCP 分配没有 extensions 行（仅 assignment，id 形如 mcp-<name>），按前缀识别
        if let Some(mcp_name) = a.extension_id.strip_prefix("mcp-") {
            if crate::services::mcp::remove_mcp(tool_id, mcp_name).is_ok() {
                result.restored_mcps.push(mcp_name.to_string());
            }
            continue;
        }
        let Some(ext) = extensions.iter().find(|e| e.id == a.extension_id) else {
            continue;
        };
        match ext.kind.as_str() {
            "skill" => {
                if let Some(dir) = crate::adapter::skill_dir_for_tool(tool_id, &home) {
                    // SSOT skill 仓库即 ensure_repo_dir()（~/.tuvis/skills/<name>）
                    let ssot = crate::linker::ensure_repo_dir().join(&ext.name);
                    // P1-4：子 Agent 分配的 Layer 3 目标（skill_dir/subagents/<sub>/<name>，
                    // 经 ~/.tuvis/active/<tool>/<sub>/ 直通 SSOT）同样要还原——工具级还原后
                    // 该链仍可解析，「彻底隐藏」被绕过
                    match a.sub_agent_id.as_deref() {
                        Some(sub) => {
                            let target = subagent_skill_target(&dir, sub, &ext.name);
                            report_restore(
                                restore_mam_link(&ssot, &target, &ext.name),
                                &format!("{}/{}", ext.name, sub),
                                result,
                            );
                        }
                        None => report_restore(
                            // 派发拍平（2026-09-17）：工具目录目标按拍平名定位磁盘链接；
                            // 上方 ssot 侧保持嵌套原路径（SSOT 仓库层级不动）
                            restore_mam_link(
                                &ssot,
                                &crate::linker::dispatch_target(&dir, &ext.name),
                                &ext.name,
                            ),
                            &ext.name,
                            result,
                        ),
                    }
                }
            }
            "plugin" => {
                if let Some(adapter) = crate::adapter::adapter_by_id(tool_id) {
                    if let Some(dir) = adapter.plugin_dirs().first() {
                        // 文件型插件 SSOT：~/.tuvis/plugins/<name>
                        let ssot = home.join(".tuvis").join("plugins").join(&ext.name);
                        report_restore(
                            restore_mam_link(&ssot, &dir.join(&ext.name), &ext.name),
                            &ext.name,
                            result,
                        );
                    }
                }
            }
            _ => {}
        }
    }
    // 未读卡一并清除（取消勾选立即彻底隐藏）
    crate::database::dao::unread::clear_tool(tool_id);
    // 同步清空 W4 心跳消失补偿的观测表：只清未读表不够——停用后任务随即完成、prewarm
    // 回池删除心跳文件时，下一轮补偿会凭残留的 pid→session 记录为已停用工具重新
    // upsert 未读行，让刚清掉的未读卡「复活」。按工具隔离清理（P2-3）：只移除属于
    // 该工具的条目，保留其他工具（未来心跳驱动工具）的观测记录
    crate::monitor::workbuddy_parser::LAST_SEEN_SESSIONS
        .lock()
        .unwrap()
        .retain(|_, (tool, _)| tool != tool_id);
}

/// 还原单项结果（spec W5 清理语义 1 + §9：SSOT 缺失跳过并在保存结果中逐项报告）。
/// issue #36-4：区分「现场完好」与「现场已失」——原文案「链接保持不变」对
/// 「移除链接成功后落位失败」子场景不成立，二阶段改为按真实现场分账
#[derive(Debug, Clone, PartialEq, Eq)]
enum RestoreOutcome {
    /// 已完整还原为真实内容
    Restored,
    /// 未能还原且现场完好（链接保持不变），计入 skipped_kept 逐项报告
    SkippedKept,
    /// 还原中断且现场已失（链接已移除、内容落位失败、重建链接恢复也失败），
    /// 计入 skipped_lost 逐项报告并提示重建
    SkippedLost,
    /// 无需处理：目标非 兔维斯 链接态（原生目录或不存在），不报告也不计数
    NotApplicable,
}

/// 按还原结果归账：完全还原计入 restored；跳过按现场完好/已失分别计入
/// skipped_kept / skipped_lost 供前端分级提示（kept=warning，lost=error）
fn report_restore(outcome: RestoreOutcome, name: &str, result: &mut ApplyResult) {
    match outcome {
        RestoreOutcome::Restored => result.restored.push(name.to_string()),
        RestoreOutcome::SkippedKept => result.skipped_kept.push(name.to_string()),
        RestoreOutcome::SkippedLost => result.skipped_lost.push(name.to_string()),
        RestoreOutcome::NotApplicable => {}
    }
}

/// 还原单个「兔维斯 建的链接」为真实内容：仅链接态（Valid/Dangling）处理，
/// 原生目录（NotLink）与不存在（Missing）不动（NotApplicable）；
/// SSOT 缺失或还原中途失败 → SkippedKept（链接保持不变）；
/// 移除链接成功后落位失败 → 先尝试重建链接恢复现场（issue #36-4），恢复成功
/// 仍计 SkippedKept，恢复失败才是 SkippedLost（工具目录空缺），由调用方分级报告。
/// 先把 SSOT 内容暂存到目标旁的临时路径（目录走 copy_dir_recursive，
/// 子 Agent 分配的 Layer 3 用户可见目标（与 services::skill::assign_skill_to_subagent
/// 的落位布局一致：工具 skill 目录下 subagents/<sub>/<拍平名>，P1-4 停用还原用。
/// 派发拍平口径见 linker::dispatch_name，2026-09-17 裁决）
fn subagent_skill_target(
    tool_skill_dir: &std::path::Path,
    sub_agent_id: &str,
    skill_name: &str,
) -> std::path::PathBuf {
    crate::linker::dispatch_target(
        &tool_skill_dir.join("subagents").join(sub_agent_id),
        skill_name,
    )
}

/// 单文件如配置型插件的 .json 走 fs::copy），暂存成功才移除链接并原子落位；
/// 任一步失败保持现场并 log::warn。
fn restore_mam_link(
    ssot: &std::path::Path,
    target: &std::path::Path,
    name: &str,
) -> RestoreOutcome {
    let health = crate::linker::check_link_health(target);
    if !matches!(
        health,
        crate::linker::LinkHealth::Valid | crate::linker::LinkHealth::Dangling
    ) {
        return RestoreOutcome::NotApplicable;
    }
    if !ssot.exists() {
        log::warn!(
            "还原 {} 跳过：SSOT 缺失（{}），链接保持不变",
            name,
            ssot.display()
        );
        return RestoreOutcome::SkippedKept;
    }
    // 记录原链接目标（review Important）：工具级目标原链 Layer 2、子 Agent 目标原链
    // Layer 3——恢复必须重建到原目标以保持层级拓扑，直连 SSOT 会静默绕过层级间接
    //（工具级断链不再隐藏该内容）。read_link 读链接本身，Dangling 亦可得
    let original_target = std::fs::read_link(target).ok();
    let tmp = target.with_extension("mam_restore_tmp");
    // 清理可能的历史残留，保证后续 rename 可落位
    let _ = crate::linker::remove_link(&tmp);
    // 先暂存（copy FIRST）：目录复制目录，单文件直接复制文件
    let staged = if ssot.is_dir() {
        crate::linker::copy_dir_recursive(ssot, &tmp)
    } else {
        std::fs::copy(ssot, &tmp)
            .map(|_| ())
            .map_err(|e| e.to_string())
    };
    if let Err(e) = staged {
        // 暂存失败：不动链接，工具内容保持原样
        let _ = crate::linker::remove_link(&tmp);
        log::warn!("还原 {} 跳过：SSOT 暂存出错（{}），链接保持不变", name, e);
        return RestoreOutcome::SkippedKept;
    }
    if let Err(e) = crate::linker::remove_link(target) {
        let _ = crate::linker::remove_link(&tmp);
        log::warn!("还原 {} 跳过：移除旧链接出错（{}），链接保持不变", name, e);
        return RestoreOutcome::SkippedKept;
    }
    if let Err(e) = std::fs::rename(&tmp, target) {
        // issue #36-4：此刻链接已被移除、现场缺失——先按原链接目标重建恢复原状
        //（保持 Layer2/3 拓扑），原目标不可知或重建失败再兜底直连 SSOT；
        // 恢复成功等同「链接保持不变」，仅两次恢复都失败才计入 SkippedLost
        //（真丢失，需重新勾选重建）
        let _ = crate::linker::remove_link(&tmp);
        let recovered = match original_target.as_deref() {
            Some(orig) if crate::linker::create_link(orig, target).is_ok() => true,
            _ => crate::linker::create_link(ssot, target).is_ok(),
        };
        if recovered {
            log::warn!(
                "还原 {} 落位失败（{}），已重建链接恢复现场，链接保持不变",
                name,
                e
            );
            return RestoreOutcome::SkippedKept;
        }
        log::warn!(
            "还原 {} 落位失败（{}）且链接恢复失败，工具目录空缺，需重新勾选后重建",
            name,
            e
        );
        return RestoreOutcome::SkippedLost;
    }
    RestoreOutcome::Restored
}

/// 重新勾选：按原分配重建（幂等；失败项记录 rebuild_failed 不中断）
fn rebuild_tool_links(tool_id: &str, result: &mut ApplyResult) {
    let mut assignments: Vec<_> = extension::list_all_assignments()
        .into_iter()
        .filter(|a| a.agent_tool_id == tool_id && a.enabled)
        .collect();
    // P1-4：先重建工具级（sub_agent_id 空）、再重建子 Agent 分配——
    // assign_skill_to_subagent 的 is_skill_in_tool_range 依赖工具级行已启用
    assignments.sort_by_key(|a| a.sub_agent_id.is_some());
    let extensions = extension::list_extensions();

    for a in &assignments {
        if let Some(mcp_name) = a.extension_id.strip_prefix("mcp-") {
            // SSOT MCP 配置：~/.tuvis/mcp/<name>.json（与 save_mcp_config / import_mcp_to_ssot 一致）
            let path = dirs::home_dir()
                .unwrap_or_default()
                .join(".tuvis")
                .join("mcp")
                .join(format!("{}.json", mcp_name));
            let ok = match std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str::<crate::services::mcp::McpConfig>(&s).ok())
            {
                Some(config) => crate::services::mcp::write_mcp(tool_id, mcp_name, &config).is_ok(),
                None => false,
            };
            if !ok {
                result.rebuild_failed.push(mcp_name.to_string());
            }
            continue;
        }
        let Some(ext) = extensions.iter().find(|e| e.id == a.extension_id) else {
            continue;
        };
        let ok = match ext.kind.as_str() {
            // P1-4：带子 Agent 分配的行走 Layer 3 重建（assign_skill_to_subagent：
            // 链 ~/.tuvis/active/<tool>/<sub>/ + 工具 skill 目录 subagents/<sub>/<name>），
            // 工具级行走 enable_skill_for_tool（Layer 2 + 工具级目标）
            "skill" => match a.sub_agent_id.as_deref() {
                Some(sub) => {
                    crate::services::skill::assign_skill_to_subagent(&ext.name, tool_id, sub)
                        .is_ok()
                }
                None => crate::services::skill::enable_skill_for_tool(&ext.name, tool_id).is_ok(),
            },
            // 插件重建按 extensions.tags 区分子类型（"file" | "config"），缺失/歧义回退 file
            // （与 preset/mod.rs 的 plugin_kind 读取一致）
            "plugin" => {
                let kind = if ext.tags.as_deref().map(str::trim) == Some("config") {
                    "config"
                } else {
                    "file"
                };
                crate::services::toggle_plugin(&ext.name, tool_id, true, kind).is_ok()
            }
            _ => true,
        };
        if !ok {
            result.rebuild_failed.push(ext.name.clone());
        }
    }
}

#[cfg(test)]
mod restore_tests {
    use super::*;

    /// 目录型链接的平台无关建链：Unix 走 symlink、Windows 走 junction（P1-3 A：create_link 抽象）。
    /// 注意 create_link 对「单文件」在 Windows 上是 fs::copy 而非链接——文件型还原测试因此
    /// 只断言「还原为真实文件」的结果（不要求先存在链接，见 restores_single_file_link_to_real_file）
    fn create_dir_link(source: &std::path::Path, target: &std::path::Path) {
        crate::linker::create_link(source, target).unwrap();
    }

    /// 目录型 SSOT：链接还原为真实目录（含内容），返回 Restored，临时目录不残留
    #[test]
    fn restores_dir_link_to_real_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let ssot = tmp.path().join("repo").join("skill-a");
        std::fs::create_dir_all(&ssot).unwrap();
        std::fs::write(ssot.join("SKILL.md"), "hello").unwrap();
        let target = tmp.path().join("tools").join("skill-a");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        create_dir_link(&ssot, &target);

        let outcome = restore_mam_link(&ssot, &target, "skill-a");

        assert_eq!(outcome, RestoreOutcome::Restored);
        assert!(target.is_dir());
        assert!(
            !crate::linker::link_marker_is_present(&target),
            "还原后应不再是链接"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("SKILL.md")).unwrap(),
            "hello"
        );
        assert!(!tmp
            .path()
            .join("tools")
            .join("skill-a.tuvis_restore_tmp")
            .exists());
    }

    /// 单文件型 SSOT（配置型插件 .json）：走 fs::copy 暂存后还原为真实文件。
    /// 说明：Windows 上 create_link 对文件目标即 fs::copy（无文件链接），因此本测试
    /// 在双平台都以「还原为真实文件且内容正确」为断言，不依赖先存在链接
    #[test]
    fn restores_single_file_link_to_real_file() {
        let tmp = tempfile::tempdir().unwrap();
        let ssot = tmp.path().join("repo").join("my-plugin.json");
        std::fs::create_dir_all(ssot.parent().unwrap()).unwrap();
        std::fs::write(&ssot, r#"{"k":"v"}"#).unwrap();
        let target = tmp.path().join("tools").join("my-plugin.json");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        // Windows 上 create_link 对文件是 copy（不是链接），restore 视为 NotApplicable——
        // 直接测「文件目标由 copy 建链后 restore 为真实文件」的等价行为：Unix 建 symlink，
        // Windows 建 copy 目标，restore 后都是真实文件
        #[cfg(unix)]
        std::os::unix::fs::symlink(&ssot, &target).unwrap();
        #[cfg(windows)]
        std::fs::copy(&ssot, &target).unwrap();

        // Unix：链接 → 还原为真实文件；Windows：已是真实副本 → NotApplicable（不报告）
        #[cfg(unix)]
        {
            let outcome = restore_mam_link(&ssot, &target, "my-plugin.json");
            assert_eq!(outcome, RestoreOutcome::Restored);
        }
        #[cfg(windows)]
        {
            let outcome = restore_mam_link(&ssot, &target, "my-plugin.json");
            assert_eq!(outcome, RestoreOutcome::NotApplicable);
        }
        assert!(target.is_file());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), r#"{"k":"v"}"#);
    }

    /// 暂存失败（SSOT 目录含悬空链接）：链接保持原样，返回 Skipped（调用方计入 skipped 逐项报告）
    #[test]
    fn keeps_link_when_staging_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let ssot = tmp.path().join("repo").join("broken");
        std::fs::create_dir_all(&ssot).unwrap();
        // 目录内含悬空链接 → copy_dir_recursive 的 fs::copy 必然失败
        let dangling_target = tmp.path().join("no-such-file");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&dangling_target, ssot.join("bad")).unwrap();
        #[cfg(windows)]
        {
            // Windows 无 dangling symlink 直接构造；junction 须指向真实存在的源——
            // 用「先建后删」制造悬空 junction 目标
            std::fs::create_dir_all(&dangling_target).unwrap();
            crate::linker::create_link(&dangling_target, &ssot.join("bad")).unwrap();
            std::fs::remove_dir_all(&dangling_target).unwrap();
        }
        let target = tmp.path().join("tools").join("broken");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        create_dir_link(&ssot, &target);

        let outcome = restore_mam_link(&ssot, &target, "broken");

        assert_eq!(outcome, RestoreOutcome::SkippedKept);
        assert!(
            crate::linker::link_marker_is_present(&target),
            "暂存失败时链接不应被移除"
        );
    }

    /// SSOT 缺失（spec W5 清理语义 1 + §9）：链接保持原样，返回 Skipped 供调用方逐项报告
    #[test]
    fn skips_and_keeps_link_when_ssot_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // SSOT 未创建（~/.tuvis/skills/<name> 被用户删除的场景），链接悬空
        let ssot = tmp.path().join("repo").join("gone");
        let target = tmp.path().join("tools").join("gone");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&ssot, &target).unwrap();
        #[cfg(windows)]
        {
            // junction 须指向存在的源：先建目录再建 junction 后删除源 → 悬空
            std::fs::create_dir_all(&ssot).unwrap();
            create_dir_link(&ssot, &target);
            std::fs::remove_dir_all(&ssot).unwrap();
        }

        let outcome = restore_mam_link(&ssot, &target, "gone");

        assert_eq!(outcome, RestoreOutcome::SkippedKept);
        assert!(
            crate::linker::link_marker_is_present(&target),
            "SSOT 缺失时链接不应被动"
        );
    }

    /// 目标为原生目录（非 兔维斯 链接态）：无需还原，返回 NotApplicable（不报告也不计入 restored）
    #[test]
    fn not_applicable_for_native_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let ssot = tmp.path().join("repo").join("native");
        std::fs::create_dir_all(&ssot).unwrap();
        let target = tmp.path().join("tools").join("native");
        std::fs::create_dir_all(&target).unwrap(); // 原生目录，非链接

        let outcome = restore_mam_link(&ssot, &target, "native");

        assert_eq!(outcome, RestoreOutcome::NotApplicable);
        assert!(target.is_dir());
        assert!(!crate::linker::link_marker_is_present(&target));
    }

    // ---- P1-3 B：Windows junction 专项（#[cfg(windows)]） ----

    /// junction 的 Valid/Dangling 健康判定 + remove_link 只删链接不删 SSOT 内容
    #[cfg(windows)]
    #[test]
    fn windows_junction_health_and_remove_only_link() {
        let tmp = tempfile::tempdir().unwrap();
        let ssot = tmp.path().join("repo").join("skill");
        std::fs::create_dir_all(&ssot).unwrap();
        std::fs::write(ssot.join("SKILL.md"), "ssot content").unwrap();
        let target = tmp.path().join("tools").join("skill");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        create_dir_link(&ssot, &target);

        // Valid：junction 目标可达
        assert_eq!(
            crate::linker::check_link_health(&target),
            crate::linker::LinkHealth::Valid
        );
        // 删除 SSOT → Dangling
        std::fs::remove_dir_all(&ssot).unwrap();
        assert_eq!(
            crate::linker::check_link_health(&target),
            crate::linker::LinkHealth::Dangling
        );
        // remove_link 只删链接本身，不碰 SSOT（SSOT 已被删，验证不误删）
        crate::linker::remove_link(&target).unwrap();
        assert!(!target.exists() && !target.is_symlink());
    }

    /// 跨 junction 的 restore_mam_link：暂存 + rename 落位为真实内容
    #[cfg(windows)]
    #[test]
    fn windows_restore_across_junction_materializes_real_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let ssot = tmp.path().join("repo").join("skill-a");
        std::fs::create_dir_all(&ssot).unwrap();
        std::fs::write(ssot.join("SKILL.md"), "hello").unwrap();
        let target = tmp.path().join("tools").join("skill-a");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        create_dir_link(&ssot, &target); // junction

        let outcome = restore_mam_link(&ssot, &target, "skill-a");

        assert_eq!(outcome, RestoreOutcome::Restored);
        assert!(target.is_dir());
        assert!(!crate::linker::link_marker_is_present(&target));
        assert_eq!(
            std::fs::read_to_string(target.join("SKILL.md")).unwrap(),
            "hello"
        );
    }

    /// SSOT 缺失时 Windows junction 还原报告 Skipped 且链接不动
    #[cfg(windows)]
    #[test]
    fn windows_ssot_missing_reports_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let ssot = tmp.path().join("repo").join("gone");
        let target = tmp.path().join("tools").join("gone");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        // 先建源再建 junction，随后删源 → 悬空 junction（Windows 不能直接建悬空 junction）
        std::fs::create_dir_all(&ssot).unwrap();
        create_dir_link(&ssot, &target);
        std::fs::remove_dir_all(&ssot).unwrap();

        let outcome = restore_mam_link(&ssot, &target, "gone");

        assert_eq!(outcome, RestoreOutcome::SkippedKept);
        assert!(crate::linker::link_marker_is_present(&target));
    }

    /// review-2 Important 3 回归锁（rename 失败恢复分支）：暂存目录被句柄钉住时
    /// rename 必然失败（Windows 子树存在打开句柄）→ 恢复分支按原目标重建链接 →
    /// SkippedKept 且链接恢复在场
    #[cfg(windows)]
    #[test]
    fn windows_rename_failure_recovers_link_reports_kept() {
        use std::os::windows::fs::OpenOptionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let ssot = tmp.path().join("repo").join("skill-a");
        std::fs::create_dir_all(&ssot).unwrap();
        std::fs::write(ssot.join("SKILL.md"), "hello").unwrap();
        let target = tmp.path().join("tools").join("skill-a");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        create_dir_link(&ssot, &target); // junction，原链接目标即 SSOT

        // 预置暂存目录并独占打开其中文件（share_mode(0)）：
        // restore 开头的 remove_link(tmp) 删不掉它，rename(暂存→目标) 因子树句柄失败
        let staging = target.with_extension("mam_restore_tmp");
        std::fs::create_dir_all(&staging).unwrap();
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(0)
            .open(staging.join("lock.txt"))
            .unwrap();

        let outcome = restore_mam_link(&ssot, &target, "skill-a");

        drop(lock);
        let _ = std::fs::remove_dir_all(&staging); // 清理测试残留（被钉住的暂存目录）

        assert_eq!(outcome, RestoreOutcome::SkippedKept);
        assert!(
            crate::linker::link_marker_is_present(&target),
            "恢复分支应重建链接"
        );
        assert_eq!(
            crate::linker::check_link_health(&target),
            crate::linker::LinkHealth::Valid
        );
    }
}

#[cfg(test)]
mod ensure_guard_tests {
    use super::*;

    /// review F4：守卫的连接参数变体——停用工具 → 结构化错误码（issue #36-3，
    /// 文案由前端 i18n 渲染）；启用/缺行 → 放行
    #[test]
    fn ensure_tool_enabled_conn_blocks_disabled_tool() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn).unwrap();
        let _ = agent_tool::ensure_tool_rows_conn(&conn);
        agent_tool::set_tool_enabled_conn(&conn, "opencode", false);
        let err = ensure_tool_enabled_conn(&conn, "opencode").unwrap_err();
        assert_eq!(
            err, "W5_TOOL_DISABLED:opencode",
            "错误码须可被前端解析且携带工具 id"
        );
        // 缺行防御视为启用
        assert!(ensure_tool_enabled_conn(&conn, "nonexistent").is_ok());
        // 重新启用 → 放行
        agent_tool::set_tool_enabled_conn(&conn, "opencode", true);
        assert!(ensure_tool_enabled_conn(&conn, "opencode").is_ok());
    }

    /// review Critical 回归锁：启用门控看本次失败增量而非累积全表——
    /// 批量保存中前面工具的 rebuild_failed 不得回滚后面重建成功的工具
    #[test]
    fn enable_gate_uses_increment_not_accumulated_table() {
        assert!(enable_allowed(0, 0));
        assert!(!enable_allowed(0, 1));
        // 前面工具已贡献 2 个失败，本次零新增 → 仍放行
        assert!(enable_allowed(2, 2));
        assert!(!enable_allowed(2, 3));
    }
}

// ---- P1-4 回归锁：子 Agent（Layer 3）分配的还原/重建 ----

#[cfg(test)]
mod subagent_assign_tests {
    use super::*;

    /// 还原往返：Layer 3 用户可见目标（subagents/<sub>/<name>，junction 指向 SSOT）
    /// 经 restore_mam_link 还原为真实内容——与工具级目标同语义
    #[test]
    fn layer3_target_restores_to_real_content() {
        let tmp = tempfile::tempdir().unwrap();
        let ssot = tmp.path().join("repo").join("skill-a");
        std::fs::create_dir_all(&ssot).unwrap();
        std::fs::write(ssot.join("SKILL.md"), "hello").unwrap();
        // Layer 3 目标：tools/subagents/sub-1/skill-a（junction 链，Windows 自动 junction）
        let tool_dir = tmp.path().join("tools");
        let target = subagent_skill_target(&tool_dir, "sub-1", "skill-a");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        crate::linker::create_link(&ssot, &target).unwrap();

        let outcome = restore_mam_link(&ssot, &target, "skill-a");

        assert_eq!(outcome, RestoreOutcome::Restored);
        assert!(target.is_dir());
        assert!(!crate::linker::link_marker_is_present(&target));
        assert_eq!(
            std::fs::read_to_string(target.join("SKILL.md")).unwrap(),
            "hello"
        );
    }

    /// 目标路径布局与 services::skill::assign_skill_to_subagent 的落位一致
    ///（subagents/<sub>/<name>），保证还原与重建作用于同一路径
    #[test]
    fn subagent_target_layout_matches_assign_service() {
        let dir = std::path::Path::new("/home/u/.claude/skills");
        let t = subagent_skill_target(dir, "reviewer", "brainstorming");
        assert_eq!(
            t,
            std::path::PathBuf::from("/home/u/.claude/skills/subagents/reviewer/brainstorming")
        );
    }
}
