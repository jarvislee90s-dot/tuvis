mod support; // 若 tests/ 下 support.rs 非共享 mod，按 dao_test.rs 的引用方式对齐

/// P2②：兼容判定改查 resource_bindings——claude 来源的 skill 不再对 codex 误报不兼容
#[test]
fn check_compatibility_uses_bindings_not_tags() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::check_compatibility;

    // 造一个 tags="claude"（来源工具）的 skill —— 旧逻辑会误判 codex 不兼容
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m1-compat".into(),
        kind: "skill".into(),
        name: "v2m1-compat".into(),
        description: None,
        source_path: "/tmp/x".into(),
        source_url: None,
        version: None,
        tags: Some("claude".into()),
        suite: None,
        source_tool: Some("claude".into()),
        is_native: false,
    })
    .unwrap();
    let pid = database::create_preset(
        "v2m1-compat-preset",
        &[("skill-v2m1-compat".into(), "skill".into())],
    )
    .unwrap();

    // 无绑定 → 通用，codex 兼容（旧逻辑这里是 incompatible）
    let report = check_compatibility(&pid, "codex");
    assert_eq!(
        report.compatible.len(),
        1,
        "tags=claude 不应再挡 codex: {:?}",
        report.incompatible
    );
    assert!(report.incompatible.is_empty());

    // 绑定 codex 专属后 → claude 不兼容且带原因
    database::upsert_resource_binding("skill-v2m1-compat", "codex", Some("依赖 codex App"))
        .unwrap();
    let report = check_compatibility(&pid, "claude");
    assert_eq!(report.incompatible.len(), 1);
    assert_eq!(report.incompatible[0].id, "skill-v2m1-compat");
    assert!(report.incompatible[0].reason.contains("codex"));
}

use std::sync::{Mutex, OnceLock};

/// 共享进程级 fake HOME 与全局状态的测试（stash 账本按 tool_id 查询无法用目录名
/// 区分；基底快照与工具 skill 目录同为共享态），并行跑会互相踩踏 —— 用互斥锁强制串行
static PRESET_V2_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// 按类型分派删除「链接或残留项」（③④ Windows 红灯修复）：
/// - **目录型链接**（Windows junction / symlink_dir）必须 `remove_dir`——只删链接
///   本身不删目标，Windows/Linux 语义一致；对目录链接调 `std::fs::remove_file`
///   在 Windows 返回 os error 5（PermissionDenied）；
/// - Unix 符号链接（不论指向目录还是文件）一律 `remove_file`；
/// - 残留真目录（上次红跑中断遗留）`remove_dir_all` 整删，现场可重排。
///
/// 幂等：目标不在场时直接返回。
///
/// Windows 判定细节：junction 的 `symlink_metadata().file_type().is_dir()` 被
/// `is_symlink` 短路恒 false，**目录型**必须看跟随后的 `metadata().is_dir()`；
/// 删除 API 本身（remove_dir）只作用于链接路径，不会跟随删目标。
fn remove_link_or_leftover(p: &std::path::Path) {
    let Ok(md) = std::fs::symlink_metadata(p) else {
        return; // 不在场即幂等通过
    };
    #[cfg(windows)]
    {
        if md.file_type().is_symlink() {
            if std::fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false) {
                std::fs::remove_dir(p).unwrap();
            } else {
                std::fs::remove_file(p).unwrap();
            }
        } else if md.is_dir() {
            std::fs::remove_dir_all(p).unwrap();
        } else {
            std::fs::remove_file(p).unwrap();
        }
    }
    #[cfg(not(windows))]
    {
        if md.file_type().is_symlink() || md.is_file() {
            std::fs::remove_file(p).unwrap();
        } else {
            std::fs::remove_dir_all(p).unwrap();
        }
    }
}

/// 暂存往返：真目录移入 ~/.tuvis/stash/<tool>/skills 再移回；账本同步
#[test]
fn stash_and_restore_roundtrip() {
    let _ledger = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::stash;

    // 工具原生技能目录（fake HOME 下）
    let tool_dir = dirs::home_dir().unwrap().join(".codex/skills");
    std::fs::create_dir_all(tool_dir.join("v2m1-native-a")).unwrap();
    std::fs::write(tool_dir.join("v2m1-native-a/SKILL.md"), "hi").unwrap();

    // 暂存
    stash::stash_native_skill("codex", "v2m1-native-a", &tool_dir.join("v2m1-native-a")).unwrap();
    assert!(!tool_dir.join("v2m1-native-a").exists(), "工具目录应读不到");
    let stashed = stash::stash_dir("codex").join("v2m1-native-a");
    assert!(stashed.is_dir(), "暂存区应有该目录");
    assert_eq!(database::unrestored_stash(Some("codex")).len(), 1);

    // 回移
    let (restored, conflicts) = stash::restore_all_for_tool("codex");
    assert_eq!(restored, vec!["v2m1-native-a".to_string()]);
    assert!(conflicts.is_empty());
    assert!(tool_dir.join("v2m1-native-a/SKILL.md").exists());
    assert!(database::unrestored_stash(Some("codex")).is_empty());
    assert!(!stashed.exists(), "暂存区应清空");
}

/// 原位被占：不覆盖，留在暂存区，报告冲突
#[test]
fn stash_restore_conflict_keeps_stash() {
    let _ledger = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::stash;

    let tool_dir = dirs::home_dir().unwrap().join(".codex/skills");
    std::fs::create_dir_all(tool_dir.join("v2m1-native-b")).unwrap();
    std::fs::write(tool_dir.join("v2m1-native-b/SKILL.md"), "origin").unwrap();
    stash::stash_native_skill("codex", "v2m1-native-b", &tool_dir.join("v2m1-native-b")).unwrap();

    // 原位被别的目录占了
    std::fs::create_dir_all(tool_dir.join("v2m1-native-b")).unwrap();
    std::fs::write(tool_dir.join("v2m1-native-b/SKILL.md"), "intruder").unwrap();

    let (restored, conflicts) = stash::restore_all_for_tool("codex");
    assert!(restored.is_empty());
    assert_eq!(conflicts.len(), 1);
    assert!(conflicts[0].contains("v2m1-native-b"));
    // 不覆盖 + 暂存保留 + 账本未消
    assert_eq!(
        std::fs::read_to_string(tool_dir.join("v2m1-native-b/SKILL.md")).unwrap(),
        "intruder"
    );
    assert!(stash::stash_dir("codex").join("v2m1-native-b").exists());
    assert_eq!(database::unrestored_stash(Some("codex")).len(), 1);

    // 人工清场后孤儿恢复可补刀
    std::fs::remove_dir_all(tool_dir.join("v2m1-native-b")).unwrap();
    let n = stash::recover_orphans();
    assert!(n >= 1);
    assert!(tool_dir.join("v2m1-native-b/SKILL.md").exists());
    assert!(database::unrestored_stash(None)
        .iter()
        .all(|e| e.skill_name != "v2m1-native-b"));
}

/// 状态扫描：兔维斯 启用项 + 原生真目录都要进基底；链接不重复计为原生
#[test]
fn scan_tool_state_captures_mam_and_native() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::snapshot;
    use multi_agents_manager_lib::services::{disable_skill_for_tool, enable_skill_for_tool};

    // SSOT 造一个 兔维斯 skill 并为 claude 启用（建链接）
    let ssot = dirs::home_dir().unwrap().join(".tuvis/skills/v2m1-scan-a");
    std::fs::create_dir_all(&ssot).unwrap();
    std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m1-scan-a".into(),
        kind: "skill".into(),
        name: "v2m1-scan-a".into(),
        description: None,
        source_path: ssot.to_string_lossy().to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: None,
        is_native: false,
    })
    .unwrap();
    enable_skill_for_tool("v2m1-scan-a", "claude").unwrap();
    // 子 Agent 分配行不得让同一 ext_id 重复入基底（快照 PK 冲突防护）
    database::upsert_assignment_with_subagent(
        "skill-v2m1-scan-a",
        "claude",
        "v2m1-scan-sub",
        true,
        "valid",
    )
    .unwrap();

    // claude 目录再放一个原生真目录（登记线，用户裁决 2026-09-16：原生目录须
    // 经 兔维斯 导入登记 is_native=1 AND source_tool 才参与快照/暂存）
    let claude_dir = dirs::home_dir().unwrap().join(".claude/skills");
    std::fs::create_dir_all(claude_dir.join("v2m1-scan-native")).unwrap();
    std::fs::write(claude_dir.join("v2m1-scan-native/SKILL.md"), "y").unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m1-scan-native".into(),
        kind: "skill".into(),
        name: "v2m1-scan-native".into(),
        description: None,
        source_path: claude_dir
            .join("v2m1-scan-native")
            .to_string_lossy()
            .to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: Some("claude".into()),
        is_native: true,
    })
    .unwrap();

    let state = snapshot::scan_tool_state("claude");
    let find = |id: &str| state.iter().find(|i| i.extension_id == id);

    let mam = find("skill-v2m1-scan-a").expect("兔维斯 启用项应入基底");
    assert_eq!(mam.origin, "mam");
    assert_eq!(mam.kind, "skill");
    assert_eq!(
        state
            .iter()
            .filter(|i| i.extension_id == "skill-v2m1-scan-a")
            .count(),
        1,
        "子 Agent 分配行不得让同一 ext_id 重复计入"
    );
    let native = find("skill-v2m1-scan-native").expect("原生真目录应入基底");
    assert_eq!(native.origin, "native");

    // 拍快照 → 可读回
    snapshot::capture_base_snapshot("claude").unwrap();
    let (active, items) = database::get_base_snapshot("claude").unwrap();
    assert!(active.is_none());
    assert!(items
        .iter()
        .any(|i| i.extension_id == "skill-v2m1-scan-a" && i.origin == "mam"));
    assert!(items
        .iter()
        .any(|i| i.extension_id == "skill-v2m1-scan-native" && i.origin == "native"));

    // 清场，避免影响其他测试
    let _ = database::disable_subagent_assignment("skill-v2m1-scan-a", "claude", "v2m1-scan-sub");
    disable_skill_for_tool("v2m1-scan-a", "claude").unwrap();
    let _ = database::delete_extension("skill-v2m1-scan-native");
    database::destroy_base_snapshot("claude").unwrap();
}

/// 清扫差集：预设外的 兔维斯 skill 停用、原生真目录暂存；常驻豁免两项都不动
#[test]
fn sweep_stashes_native_and_disables_mam_except_resident() {
    let _ledger = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::sweep;
    use multi_agents_manager_lib::services::{disable_skill_for_tool, enable_skill_for_tool};

    // 兔维斯 skill A（预设内）+ 兔维斯 skill B（预设外）为 claude 启用
    for name in ["v2m1-sw-a", "v2m1-sw-b"] {
        let ssot = dirs::home_dir().unwrap().join(".tuvis/skills").join(name);
        std::fs::create_dir_all(&ssot).unwrap();
        std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
        database::insert_extension(&database::ExtensionRecord {
            id: format!("skill-{}", name),
            kind: "skill".into(),
            name: name.into(),
            description: None,
            source_path: ssot.to_string_lossy().to_string(),
            source_url: None,
            version: None,
            tags: None,
            suite: None,
            source_tool: None,
            is_native: false,
        })
        .unwrap();
        enable_skill_for_tool(name, "claude").unwrap();
    }
    // 原生真目录 C（预设外）与 D（常驻）——登记线（2026-09-16 裁决）：原生目录
    // 须经 兔维斯 登记才参与快照/暂存，未登记即视为常驻
    let claude_dir = dirs::home_dir().unwrap().join(".claude/skills");
    for name in ["v2m1-sw-c", "v2m1-sw-d"] {
        std::fs::create_dir_all(claude_dir.join(name)).unwrap();
        std::fs::write(claude_dir.join(name).join("SKILL.md"), "n").unwrap();
        database::insert_extension(&database::ExtensionRecord {
            id: format!("skill-{}", name),
            kind: "skill".into(),
            name: name.into(),
            description: None,
            source_path: claude_dir.join(name).to_string_lossy().to_string(),
            source_url: None,
            version: None,
            tags: None,
            suite: None,
            source_tool: Some("claude".into()),
            is_native: true,
        })
        .unwrap();
    }
    database::set_tool_resident("claude", "skill-v2m1-sw-d", true).unwrap();

    // 计划：keep 只有 A（断言用 contains——集成测试共享 HOME，其他测试可能残留原生目录）
    let keep = vec![("skill-v2m1-sw-a".to_string(), "skill".to_string())];
    let plan = sweep::plan_sweep("claude", &keep);
    assert!(plan
        .disable_mam
        .contains(&("skill-v2m1-sw-b".to_string(), "skill".to_string())));
    assert!(plan.stash_native.contains(&"v2m1-sw-c".to_string()));
    assert!(
        !plan.stash_native.contains(&"v2m1-sw-d".to_string()),
        "常驻项不得进暂存计划"
    );

    // 执行：B 断链、C 暂存、D 不动
    let (disabled, stashed, failures) = sweep::execute_sweep("claude", &plan);
    assert!(disabled.contains(&"skill-v2m1-sw-b".to_string()));
    assert!(stashed.contains(&"v2m1-sw-c".to_string()));
    assert!(failures.is_empty(), "{:?}", failures);
    assert!(!claude_dir.join("v2m1-sw-b").exists(), "B 链接应已断");
    assert!(!claude_dir.join("v2m1-sw-c").exists(), "C 应已暂存");
    assert!(claude_dir.join("v2m1-sw-d").is_dir(), "常驻 D 不得动");

    // 清场
    database::set_tool_resident("claude", "skill-v2m1-sw-d", false).unwrap();
    disable_skill_for_tool("v2m1-sw-a", "claude").unwrap();
    let _ = database::delete_extension("skill-v2m1-sw-c");
    let _ = database::delete_extension("skill-v2m1-sw-d");
    let _ = database::destroy_base_snapshot("claude");
}

/// 完整生命周期（spec §3.2/§5）：开（拍基底+独占）→ 切换（沿用基底）→ 关（精确回基底+销毁）→ 再开（重拍新基底）
#[test]
fn apply_switch_restore_full_lifecycle() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::{apply_preset, restore_tool};
    use multi_agents_manager_lib::services::{disable_skill_for_tool, enable_skill_for_tool};

    let home = dirs::home_dir().unwrap();
    let claude_dir = home.join(".claude/skills");

    // 基底现场：兔维斯 skill base-1 已启用（含一条子 Agent 分配行）+ 原生真目录 native-1
    //（保留单元素 for：与多资源场景的 setup 写法同构，便于扩展）
    #[allow(clippy::single_element_loop)]
    for name in ["v2m1-lc-base1"] {
        let ssot = dirs::home_dir().unwrap().join(".tuvis/skills").join(name);
        std::fs::create_dir_all(&ssot).unwrap();
        std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
        database::insert_extension(&database::ExtensionRecord {
            id: format!("skill-{}", name),
            kind: "skill".into(),
            name: name.into(),
            description: None,
            source_path: ssot.to_string_lossy().to_string(),
            source_url: None,
            version: None,
            tags: None,
            suite: None,
            source_tool: None,
            is_native: false,
        })
        .unwrap();
        enable_skill_for_tool(name, "claude").unwrap();
    }
    // 子 Agent 分配行：应用预设时随工具级清扫断链，恢复默认后必须重建回来
    database::upsert_assignment_with_subagent(
        "skill-v2m1-lc-base1",
        "claude",
        "v2m1-lc-sub",
        true,
        "valid",
    )
    .unwrap();
    let sub_target = claude_dir.join("subagents/v2m1-lc-sub/v2m1-lc-base1");
    std::fs::create_dir_all(claude_dir.join("v2m1-lc-native1")).unwrap();
    std::fs::write(claude_dir.join("v2m1-lc-native1/SKILL.md"), "n").unwrap();
    // 登记线（2026-09-16 裁决）：原生目录须登记才参与快照/暂存
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m1-lc-native1".into(),
        kind: "skill".into(),
        name: "v2m1-lc-native1".into(),
        description: None,
        source_path: claude_dir
            .join("v2m1-lc-native1")
            .to_string_lossy()
            .to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: Some("claude".into()),
        is_native: true,
    })
    .unwrap();

    // 预设 A：skill-a
    let mk = |name: &str| {
        let ssot = home.join(".tuvis/skills").join(name);
        std::fs::create_dir_all(&ssot).unwrap();
        std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
        database::insert_extension(&database::ExtensionRecord {
            id: format!("skill-{}", name),
            kind: "skill".into(),
            name: name.into(),
            description: None,
            source_path: ssot.to_string_lossy().to_string(),
            source_url: None,
            version: None,
            tags: None,
            suite: None,
            source_tool: None,
            is_native: false,
        })
        .unwrap();
    };
    mk("v2m1-lc-a");
    mk("v2m1-lc-b");
    let preset_a =
        database::create_preset("v2m1-lc-A", &[("skill-v2m1-lc-a".into(), "skill".into())])
            .unwrap();
    let preset_b =
        database::create_preset("v2m1-lc-B", &[("skill-v2m1-lc-b".into(), "skill".into())])
            .unwrap();

    // 开 A：base-1 断链（含子 Agent 链级联清理）、native-1 暂存、a 启用；快照在、active=A
    let r = apply_preset(&preset_a, "claude").unwrap();
    assert!(r.success >= 1);
    assert!(
        r.disabled.contains(&"skill-v2m1-lc-base1".to_string()),
        "{:?}",
        r.disabled
    );
    assert!(
        r.stashed.contains(&"v2m1-lc-native1".to_string()),
        "{:?}",
        r.stashed
    );
    assert!(claude_dir.join("v2m1-lc-a").exists());
    assert!(!claude_dir.join("v2m1-lc-base1").exists());
    let (active, items) = database::get_base_snapshot("claude").unwrap();
    assert_eq!(active.as_deref(), Some(preset_a.as_str()));
    assert!(items
        .iter()
        .any(|i| i.extension_id == "skill-v2m1-lc-base1" && i.origin == "mam"));
    assert!(items
        .iter()
        .any(|i| i.extension_id == "skill-v2m1-lc-native1" && i.origin == "native"));

    // 切 B：a 断、b 启；基底沿用（native-1 仍暂存，base-1 仍不在）
    let r2 = apply_preset(&preset_b, "claude").unwrap();
    assert!(r2.disabled.contains(&"skill-v2m1-lc-a".to_string()));
    assert!(!claude_dir.join("v2m1-lc-a").exists());
    assert!(claude_dir.join("v2m1-lc-b").exists());
    assert!(
        !claude_dir.join("v2m1-lc-native1").exists(),
        "切换不清算基底，原生仍暂存"
    );
    let (active_b, _) = database::get_base_snapshot("claude").unwrap();
    assert_eq!(active_b.as_deref(), Some(preset_b.as_str()));

    // 会话期间手动漂移：再启用 base-1
    enable_skill_for_tool("v2m1-lc-base1", "claude").unwrap();
    assert!(claude_dir.join("v2m1-lc-base1").exists());

    // 关：精确回基底——base-1 回来（含子 Agent 链接重建）、native-1 回来、b/a 都不在；快照销毁
    let rr = restore_tool("claude").unwrap();
    assert!(
        claude_dir.join("v2m1-lc-base1").exists(),
        "兔维斯 基底项应重建"
    );
    assert!(
        claude_dir.join("v2m1-lc-native1").exists(),
        "原生暂存应回移"
    );
    assert!(rr.restored_native.contains(&"v2m1-lc-native1".to_string()));
    assert!(sub_target.exists(), "子 Agent 链接应随基底重建（Layer3）");
    assert!(!claude_dir.join("v2m1-lc-a").exists());
    assert!(!claude_dir.join("v2m1-lc-b").exists());
    assert!(
        database::get_base_snapshot("claude").is_none(),
        "恢复后快照销毁（会话级）"
    );

    // 再开 A：重拍新基底（= 刚恢复的状态）
    apply_preset(&preset_a, "claude").unwrap();
    let (active2, items2) = database::get_base_snapshot("claude").unwrap();
    assert_eq!(active2.as_deref(), Some(preset_a.as_str()));
    assert!(items2
        .iter()
        .any(|i| i.extension_id == "skill-v2m1-lc-base1"));
    // 清场
    let _ = restore_tool("claude");
    disable_skill_for_tool("v2m1-lc-base1", "claude").unwrap();
    let _ = database::delete_extension("skill-v2m1-lc-native1");
}

/// scope 守卫：tool 私有预设不可应用到别的工具（硬错误）
#[test]
fn apply_rejects_cross_tool_for_tool_scoped_preset() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::apply_preset;

    let id =
        database::create_preset_with_meta("v2m1-scope", "", "tool", Some("codex"), &[]).unwrap();
    let err = apply_preset(&id, "claude").unwrap_err();
    assert!(err.contains("绑定"), "应拒绝跨工具: {}", err);
}

/// 专属过滤：预设项对目标工具不兼容 → 剔除进 conflicts，不启用
#[test]
fn apply_filters_incompatible_items() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::apply_preset;

    let home = dirs::home_dir().unwrap();
    let ssot = home.join(".tuvis/skills/v2m1-filt-a");
    std::fs::create_dir_all(&ssot).unwrap();
    std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m1-filt-a".into(),
        kind: "skill".into(),
        name: "v2m1-filt-a".into(),
        description: None,
        source_path: ssot.to_string_lossy().to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: None,
        is_native: false,
    })
    .unwrap();
    database::upsert_resource_binding("skill-v2m1-filt-a", "codex", Some("专属 codex")).unwrap();

    let pid = database::create_preset("v2m1-filt", &[("skill-v2m1-filt-a".into(), "skill".into())])
        .unwrap();
    let r = apply_preset(&pid, "claude").unwrap();
    assert_eq!(r.success, 0);
    assert_eq!(r.conflicts.len(), 1);
    assert!(r.conflicts[0].contains("v2m1-filt-a"));
    let claude_dir = home.join(".claude/skills");
    assert!(!claude_dir.join("v2m1-filt-a").exists(), "被过滤项不得启用");
    // 清场辅助返回 ()，let _ 仅为表达「忽略即丢弃」的意图
    #[allow(clippy::let_unit_value)]
    let _ = restore_tool_cleanup_only("claude");
}

/// 测试辅助：只销毁快照不动文件（清场用）
fn restore_tool_cleanup_only(tool_id: &str) {
    let _ = multi_agents_manager_lib::database::destroy_base_snapshot(tool_id);
    let _ = multi_agents_manager_lib::services::preset::stash::restore_all_for_tool(tool_id);
}

/// 激活中的预设不可删除（spec §5.4）——先恢复默认再删
#[test]
fn delete_rejects_active_preset() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::commands::preset as cmd;
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::apply_preset;

    let home = dirs::home_dir().unwrap();
    let ssot = home.join(".tuvis/skills/v2m1-del-a");
    std::fs::create_dir_all(&ssot).unwrap();
    std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m1-del-a".into(),
        kind: "skill".into(),
        name: "v2m1-del-a".into(),
        description: None,
        source_path: ssot.to_string_lossy().to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: None,
        is_native: false,
    })
    .unwrap();
    let pid = database::create_preset("v2m1-del", &[("skill-v2m1-del-a".into(), "skill".into())])
        .unwrap();
    apply_preset(&pid, "claude").unwrap();

    let err = cmd::delete_preset(pid.clone()).unwrap_err();
    assert!(err.contains("激活"), "{}", err);

    let _ = multi_agents_manager_lib::services::preset::restore_tool("claude");
    cmd::delete_preset(pid).unwrap(); // 恢复后可删
}

/// 预览（dry-run）不动现场；get_active_preset 反映开关状态
#[test]
fn preview_is_dryrun_and_active_preset_queryable() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::commands::preset as cmd;
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::{apply_preset, preview_apply, restore_tool};

    assert_eq!(cmd::get_active_preset("claude".into()), None);

    let home = dirs::home_dir().unwrap();
    let claude_dir = home.join(".claude/skills");
    std::fs::create_dir_all(claude_dir.join("v2m1-pv-native")).unwrap();
    // 登记线（2026-09-16 裁决）：原生目录须登记才参与快照/暂存
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m1-pv-native".into(),
        kind: "skill".into(),
        name: "v2m1-pv-native".into(),
        description: None,
        source_path: claude_dir
            .join("v2m1-pv-native")
            .to_string_lossy()
            .to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: Some("claude".into()),
        is_native: true,
    })
    .unwrap();
    let ssot = home.join(".tuvis/skills/v2m1-pv-a");
    std::fs::create_dir_all(&ssot).unwrap();
    std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m1-pv-a".into(),
        kind: "skill".into(),
        name: "v2m1-pv-a".into(),
        description: None,
        source_path: ssot.to_string_lossy().to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: None,
        is_native: false,
    })
    .unwrap();
    let pid =
        database::create_preset("v2m1-pv", &[("skill-v2m1-pv-a".into(), "skill".into())]).unwrap();

    let pv = preview_apply(&pid, "claude").unwrap();
    assert_eq!(pv.to_enable, vec!["skill-v2m1-pv-a".to_string()]);
    assert!(pv.to_stash.contains(&"v2m1-pv-native".to_string()));
    assert!(pv.filtered.is_empty());
    assert!(claude_dir.join("v2m1-pv-native").exists(), "预览不得动现场");

    apply_preset(&pid, "claude").unwrap();
    assert_eq!(cmd::get_active_preset("claude".into()), Some(pid.clone()));
    let _ = restore_tool("claude");
    assert_eq!(cmd::get_active_preset("claude".into()), None);
    let _ = database::delete_extension("skill-v2m1-pv-native");
}

/// P2①：MCP 入 SSOT 必须落 extensions 行——预设创建列表与卡片从此同源；
/// 注册表回填把历史无行的 skill/mcp 补进表
#[test]
fn mcp_import_and_backfill_register_rows() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::resource::backfill_registry;

    let repo = dirs::home_dir().unwrap().join(".tuvis/mcp");
    let skill = dirs::home_dir()
        .unwrap()
        .join(".tuvis/skills/v2m1-backfill-skill");

    // 场地自复位（⑧ flake 根因）：Windows 下 HOME 重定向对 dirs::home_dir()
    // 无效（dirs-sys 0.5 走 SHGetKnownFolderPath，不读环境变量）⇒ 本用例的
    // SSOT 文件落在**真实 home** 且原先从不清理，跨运行残留。而互斥锁只串行
    // 化执行、不串行化**顺序**（多线程同时阻塞在锁上，唤醒顺序不定）：整包
    // 并行跑时，别的 backfill 用例若先拿锁，会把残留文件登记进共享库，击穿
    // 下方「前置：表里无行」断言（单跑无别的用例 ⇒ 恒过）。故先清残留（文件
    // + 目录 + 库行，全部幂等），使断言与执行顺序无关。
    let _ = std::fs::remove_file(repo.join("v2m1-backfill-mcp.json"));
    let _ = std::fs::remove_file(repo.join("v2m1-direct-mcp.json"));
    let _ = std::fs::remove_dir_all(&skill);
    let _ = database::delete_extension("mcp-v2m1-backfill-mcp");
    let _ = database::delete_extension("skill-v2m1-backfill-skill");
    let _ = database::delete_extension("mcp-v2m1-direct-mcp");

    // 手工放一个 MCP 配置文件（历史上 toggle_mcp 只写 assignment 不写表）
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("v2m1-backfill-mcp.json"), r#"{"command":"x"}"#).unwrap();
    // 手工放一个无行的 skill 目录（历史残留/手工放置）
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "x").unwrap();

    assert!(database::list_extensions()
        .iter()
        .all(|e| e.id != "mcp-v2m1-backfill-mcp"));
    backfill_registry();

    let ids: Vec<String> = database::list_extensions()
        .iter()
        .map(|e| e.id.clone())
        .collect();
    assert!(
        ids.contains(&"mcp-v2m1-backfill-mcp".to_string()),
        "MCP 应回填入表"
    );
    assert!(
        ids.contains(&"skill-v2m1-backfill-skill".to_string()),
        "无行 skill 应回填"
    );

    // 幂等：再跑不重复
    backfill_registry();
    let n_mcp = database::list_extensions()
        .iter()
        .filter(|e| e.id == "mcp-v2m1-backfill-mcp")
        .count();
    assert_eq!(n_mcp, 1);

    // save_mcp_config 命令直接落表（新导入路径）
    multi_agents_manager_lib::commands::resource::save_mcp_config(
        "v2m1-direct-mcp".into(),
        "node".into(),
        vec![],
        Default::default(),
    )
    .unwrap();
    assert!(
        database::list_extensions()
            .iter()
            .any(|e| e.id == "mcp-v2m1-direct-mcp"),
        "save_mcp_config 应写 extensions 行"
    );

    // 清场（真实 home 路径 + 库行，不留下一轮的「表里已有行」陷阱）
    let _ = std::fs::remove_file(repo.join("v2m1-backfill-mcp.json"));
    let _ = std::fs::remove_file(repo.join("v2m1-direct-mcp.json"));
    let _ = std::fs::remove_dir_all(&skill);
    let _ = database::delete_extension("mcp-v2m1-backfill-mcp");
    let _ = database::delete_extension("skill-v2m1-backfill-skill");
    let _ = database::delete_extension("mcp-v2m1-direct-mcp");
}

/// P2① 嵌套套件口径（M2）：回填与资源视图同用递归扫描——嵌套套件技能
/// （v2m2-suite-x/inner）也要登记入表，且套件目录本身不得被误登记为技能
#[test]
fn backfill_registry_covers_nested_suite_skills() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::resource::backfill_registry;

    // 平铺技能（顶层直接含 SKILL.md）与嵌套套件技能（suite/inner/SKILL.md）
    let flat = dirs::home_dir().unwrap().join(".tuvis/skills/v2m2-flat-x");
    std::fs::create_dir_all(&flat).unwrap();
    std::fs::write(flat.join("SKILL.md"), "x").unwrap();
    let inner = dirs::home_dir()
        .unwrap()
        .join(".tuvis/skills/v2m2-suite-x/inner");
    std::fs::create_dir_all(&inner).unwrap();
    std::fs::write(inner.join("SKILL.md"), "y").unwrap();

    backfill_registry();

    let exts = database::list_extensions();
    let ids: Vec<String> = exts.iter().map(|e| e.id.clone()).collect();
    assert!(
        ids.contains(&"skill-v2m2-flat-x".to_string()),
        "平铺技能应回填入表"
    );
    let nested = exts.iter().find(|e| e.id == "skill-v2m2-suite-x/inner");
    assert!(
        nested.is_some(),
        "嵌套套件技能应按相对路径回填入表: {:?}",
        ids
    );
    let nested = nested.unwrap();
    assert_eq!(nested.name, "v2m2-suite-x/inner", "name = 相对路径");
    assert_eq!(nested.kind, "skill");
    assert!(!nested.is_native);
    // source_path 断言用 Path 的**组件级** ends_with：backfill 登记的 source_path
    // 是原生分隔符（Windows 反斜杠），字符串后缀 ".tuvis/skills/..." 在 Windows
    // 恒不匹配；Path::ends_with 按组件比较，跨分隔符成立
    assert!(std::path::Path::new(&nested.source_path)
        .ends_with(std::path::Path::new(".tuvis/skills/v2m2-suite-x/inner")));

    // 套件目录本身（无 SKILL.md）不得被误登记为名为 v2m2-suite-x 的技能
    assert!(
        exts.iter().all(|e| e.id != "skill-v2m2-suite-x"),
        "套件目录不得误登记"
    );
    assert!(
        exts.iter().all(|e| e.name != "v2m2-suite-x"),
        "不得存在名为 v2m2-suite-x 的误登记行"
    );

    // 清理测试数据
    let _ = database::delete_extension("skill-v2m2-flat-x");
    let _ = database::delete_extension("skill-v2m2-suite-x/inner");
    let _ = std::fs::remove_dir_all(flat);
    let _ = std::fs::remove_dir_all(inner.parent().unwrap());
}

/// 不变量（spec §3.2）：快照在而 active 为空（或反之）→ 检查器报告
#[test]
fn snapshot_invariant_detector_reports_broken_state() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database::{self, BaseSnapshotItemRecord};
    use multi_agents_manager_lib::services::preset::check_snapshot_invariants;

    // 人为构造破坏态：快照在、active 为 None
    //（检查器只扫已注册工具 TOOL_IDS，故工具 id 须取注册表内的 dsh）
    database::save_base_snapshot(
        "dsh",
        None,
        &[BaseSnapshotItemRecord {
            extension_id: "skill-v2m1-inv".into(),
            kind: "skill".into(),
            origin: "mam".into(),
        }],
    )
    .unwrap();
    let broken = check_snapshot_invariants();
    assert!(broken.iter().any(|s| s.contains("dsh")), "{:?}", broken);
    database::destroy_base_snapshot("dsh").unwrap();
    assert!(check_snapshot_invariants().is_empty());
}

/// Patch 1a（评审裁决 1）会话守卫：激活会话进行中，孤儿恢复不得回移该工具的
/// 暂存项；销毁快照（会话结束）后恢复
#[test]
fn recover_orphans_skips_active_session_then_restores_after_destroy() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::stash;

    let codex_dir = dirs::home_dir().unwrap().join(".codex/skills");
    std::fs::create_dir_all(codex_dir.join("v2m1-gd-native")).unwrap();
    std::fs::write(codex_dir.join("v2m1-gd-native/SKILL.md"), "n").unwrap();
    stash::stash_native_skill("codex", "v2m1-gd-native", &codex_dir.join("v2m1-gd-native"))
        .unwrap();
    // 激活会话进行中：快照在且 active 非空
    database::save_base_snapshot("codex", Some("preset-v2m1-gd"), &[]).unwrap();

    let n_guarded = stash::recover_orphans();
    assert!(
        stash::stash_dir("codex").join("v2m1-gd-native").is_dir(),
        "激活会话期间孤儿恢复不得回移暂存项"
    );
    assert!(!codex_dir.join("v2m1-gd-native").exists(), "原位不得被触碰");
    assert_eq!(database::unrestored_stash(Some("codex")).len(), 1);

    // 会话结束（快照销毁）→ 孤儿恢复放行
    database::destroy_base_snapshot("codex").unwrap();
    let n = stash::recover_orphans();
    assert!(
        codex_dir.join("v2m1-gd-native/SKILL.md").exists(),
        "会话结束后应回移"
    );
    assert!(
        !stash::stash_dir("codex").join("v2m1-gd-native").exists(),
        "暂存区应清空"
    );
    assert!(database::unrestored_stash(Some("codex")).is_empty());
    assert!(
        n >= 1 && n_guarded == 0,
        "守卫期计数 0，放行后计数 >= 1（{n_guarded}/{n}）"
    );
}

/// Patch 1b（评审裁决 1）账本自愈：回移已落位但未销账的硬崩溃残留——
/// 暂存文件不在且原位在 → 销账，unrestored 不再含它（只写账不动文件）
#[test]
fn recover_orphans_self_heals_stale_ledger_entries() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::stash;

    let codex_dir = dirs::home_dir().unwrap().join(".codex/skills");
    std::fs::create_dir_all(codex_dir.join("v2m1-heal-orig")).unwrap();
    std::fs::write(codex_dir.join("v2m1-heal-orig/SKILL.md"), "n").unwrap();
    // 账目在而暂存文件不在（伪造「移完没销账即崩溃」）
    let ghost_stash = stash::stash_dir("codex").join("v2m1-heal-ghost");
    database::record_stash(
        "codex",
        "v2m1-heal-orig",
        &ghost_stash.to_string_lossy(),
        &codex_dir.join("v2m1-heal-orig").to_string_lossy(),
    )
    .unwrap();
    assert!(database::unrestored_stash(Some("codex"))
        .iter()
        .any(|e| e.skill_name == "v2m1-heal-orig"));

    stash::recover_orphans();
    assert!(
        !database::unrestored_stash(Some("codex"))
            .iter()
            .any(|e| e.skill_name == "v2m1-heal-orig"),
        "自愈后该账目应已销账"
    );
    assert!(
        codex_dir.join("v2m1-heal-orig/SKILL.md").exists(),
        "自愈不动文件"
    );
}

/// Patch 1c（评审裁决 1）无账孤儿对账：rename 与记账之间硬崩溃留下的
/// 无账暂存目录 → 移回原位 + 补记审计；原位被占则保留待人工
#[test]
fn recover_orphans_reclaims_unledgered_stash_dirs() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::stash;

    // 场地自复位（⑥ Windows 红灯根因）：support::setup 的 HOME 重定向对
    // dirs::home_dir() 在 Windows 无效（dirs-sys 0.5 走 SHGetKnownFolderPath，
    // 不读任何环境变量）⇒ stash_dir 与 recover_orphans 的回移目标
    // （primary_skill_dir → 真实 ~/.codex/skills）都落在**真实 home**。本用例
    // 成功路径会把 ghost-a 移回 ~/.codex/skills 且从不清理——上一轮遗留即占了
    // 本轮的回移目标，孤儿被判「原位被占」留在暂存区，击穿「暂存区应清空」。
    // 故开跑前清掉两侧四个历史路径 + 销掉可能残留的 codex 快照（会话守卫会
    // 跳过整段无账对账），保证「原位空闲」前置成立且与执行顺序无关。
    let codex_dir = dirs::home_dir().unwrap().join(".codex/skills");
    let ghost_a = stash::stash_dir("codex").join("v2m1-ghost-a");
    let ghost_b = stash::stash_dir("codex").join("v2m1-ghost-b");
    let _ = std::fs::remove_dir_all(&ghost_a);
    let _ = std::fs::remove_dir_all(&ghost_b);
    let _ = std::fs::remove_dir_all(codex_dir.join("v2m1-ghost-a"));
    let _ = std::fs::remove_dir_all(codex_dir.join("v2m1-ghost-b"));
    let _ = database::destroy_base_snapshot("codex");

    // 无账孤儿 A（原位空闲）与无账孤儿 B（原位被占）
    std::fs::create_dir_all(&ghost_a).unwrap();
    std::fs::write(ghost_a.join("SKILL.md"), "g").unwrap();
    std::fs::create_dir_all(&ghost_b).unwrap();
    std::fs::write(ghost_b.join("SKILL.md"), "g").unwrap();
    std::fs::create_dir_all(codex_dir.join("v2m1-ghost-b")).unwrap();
    std::fs::write(codex_dir.join("v2m1-ghost-b/SKILL.md"), "intruder").unwrap();

    stash::recover_orphans();

    // A：移回原位 + 补记审计（已销账，不在 unrestored）
    assert!(
        codex_dir.join("v2m1-ghost-a/SKILL.md").exists(),
        "无账孤儿应移回原位"
    );
    assert!(!ghost_a.exists(), "暂存区应清空");
    assert!(
        !database::unrestored_stash(Some("codex"))
            .iter()
            .any(|e| e.skill_name == "v2m1-ghost-a"),
        "补记审计应已销账（restored 状态）"
    );
    // B：原位被占 → 不覆盖、留在暂存区
    assert!(ghost_b.is_dir(), "原位被占的孤儿应留在暂存区");
    assert_eq!(
        std::fs::read_to_string(codex_dir.join("v2m1-ghost-b/SKILL.md")).unwrap(),
        "intruder",
        "原位内容不得被覆盖"
    );

    // 清场（真实 home 路径，不给下一轮留占用）：A 的回移落点 + B 的两侧
    let _ = std::fs::remove_dir_all(codex_dir.join("v2m1-ghost-a"));
    let _ = std::fs::remove_dir_all(&ghost_b);
    let _ = std::fs::remove_dir_all(codex_dir.join("v2m1-ghost-b"));
}

/// Patch 2（评审裁决 2）：取消勾选时前置恢复失败（Err）→ 不清理、不落 disabled，
/// 工具保持启用并计入 skipped_kept（可重试）
#[test]
fn apply_tool_changes_keeps_tool_enabled_when_preset_restore_fails() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::tool_settings::{
        apply_tool_changes_with, ToolSettingChange,
    };
    use multi_agents_manager_lib::services::{disable_skill_for_tool, enable_skill_for_tool};

    // 现场：兔维斯 skill 已启用（有链接可被清理），工具启用中
    let ssot = dirs::home_dir().unwrap().join(".tuvis/skills/v2m1-w5-a");
    std::fs::create_dir_all(&ssot).unwrap();
    std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m1-w5-a".into(),
        kind: "skill".into(),
        name: "v2m1-w5-a".into(),
        description: None,
        source_path: ssot.to_string_lossy().to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: None,
        is_native: false,
    })
    .unwrap();
    enable_skill_for_tool("v2m1-w5-a", "claude").unwrap();
    assert!(database::get_tool_enabled("claude"), "前置：工具启用中");
    let claude_link = dirs::home_dir().unwrap().join(".claude/skills/v2m1-w5-a");
    assert!(claude_link.exists(), "前置：链接在场");

    // 注入恢复失败
    let changes = vec![ToolSettingChange {
        tool_id: "claude".into(),
        enabled: false,
    }];
    let result = apply_tool_changes_with(changes, &|_tool: &str| Err("boom".to_string()));

    assert!(
        database::get_tool_enabled("claude"),
        "恢复失败时工具必须保持启用"
    );
    assert!(
        result
            .skipped_kept
            .iter()
            .any(|s| s.contains("claude") && s.contains("快照销账失败") && s.contains("重试可愈")),
        "skipped_kept 应含精确文案: {:?}",
        result.skipped_kept
    );
    // W5 清理未执行：工具目录里仍是 兔维斯 链接（未被还原/未被动过）
    let link_meta = std::fs::symlink_metadata(&claude_link).expect("链接应仍在场");
    assert!(
        link_meta.file_type().is_symlink(),
        "恢复失败时链接必须原样保留"
    );
    // 清场：注入 Ok 走正常清理；W5 还原会把链接还原成真实内容且 assignment 行
    // 仍为 enabled——必须禁用 assignment 并移除真实目录，否则后续测试扫描 claude
    // 会出现同 ext_id 双条目（拍基底 UNIQUE 失败）
    let _ = apply_tool_changes_with(
        vec![ToolSettingChange {
            tool_id: "claude".into(),
            enabled: false,
        }],
        &|_tool: &str| Ok(Default::default()),
    );
    let _ = disable_skill_for_tool("v2m1-w5-a", "claude");
    let _ = std::fs::remove_dir_all(&claude_link);
    database::set_tool_enabled("claude", true);
}

/// Patch 2（评审裁决 2）真实路径：无激活预设时 restore 返回 Ok（空 conflicts）
/// → 取消勾选照常清理并落 disabled
#[test]
fn apply_tool_changes_disables_tool_when_preset_restore_succeeds() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::{
        disable_skill_for_tool, enable_skill_for_tool, tool_settings,
    };

    let ssot = dirs::home_dir().unwrap().join(".tuvis/skills/v2m1-w5-b");
    std::fs::create_dir_all(&ssot).unwrap();
    std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m1-w5-b".into(),
        kind: "skill".into(),
        name: "v2m1-w5-b".into(),
        description: None,
        source_path: ssot.to_string_lossy().to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: None,
        is_native: false,
    })
    .unwrap();
    enable_skill_for_tool("v2m1-w5-b", "claude").unwrap();

    let changes = vec![tool_settings::ToolSettingChange {
        tool_id: "claude".into(),
        enabled: false,
    }];
    let result = tool_settings::apply_tool_changes(changes);

    assert!(
        !database::get_tool_enabled("claude"),
        "恢复 Ok 时应照常落 disabled"
    );
    // W5 清理语义：链接被还原为真实内容（内容保留、兔维斯 接管解除），而非删除
    let w5_path = dirs::home_dir().unwrap().join(".claude/skills/v2m1-w5-b");
    let w5_meta = std::fs::symlink_metadata(&w5_path).expect("W5 还原后原位应有真实内容");
    assert!(
        !w5_meta.file_type().is_symlink(),
        "兔维斯 链接应已解除（还原为真实目录）"
    );
    assert!(w5_path.join("SKILL.md").exists(), "SSOT 内容应落回原位");
    assert!(
        result.restored.contains(&"v2m1-w5-b".to_string()),
        "清理结果应报告还原项: {:?}",
        result.restored
    );
    // 清场：禁用 assignment 并移除 W5 还原出的真实目录（同上，避免污染
    // 后续测试的 scan_tool_state），再恢复工具启用态
    let _ = disable_skill_for_tool("v2m1-w5-b", "claude");
    let _ = std::fs::remove_dir_all(&w5_path);
    database::set_tool_enabled("claude", true);
}

/// Patch 3（评审裁决 3）：幂等重保存 MCP 不得抹掉用户编辑过的元数据——
/// INSERT OR REPLACE 会清空 description/suite，必须走 INSERT OR IGNORE
#[test]
fn save_mcp_config_preserves_user_metadata() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::commands::resource::save_mcp_config;
    use multi_agents_manager_lib::database;

    // 预置带用户备注的 MCP 行（历史编辑过的元数据）
    database::insert_extension(&database::ExtensionRecord {
        id: "mcp-v2m1-meta".into(),
        kind: "mcp".into(),
        name: "v2m1-meta".into(),
        description: Some("用户备注".into()),
        source_path: "/tmp/old".into(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: None,
        is_native: false,
    })
    .unwrap();

    // 同名重保存配置
    save_mcp_config(
        "v2m1-meta".into(),
        "node".into(),
        vec![],
        Default::default(),
    )
    .unwrap();

    let row = database::list_extensions()
        .into_iter()
        .find(|e| e.id == "mcp-v2m1-meta")
        .expect("重保存后行应存在");
    assert_eq!(
        row.description.as_deref(),
        Some("用户备注"),
        "重保存不得抹掉用户元数据"
    );
}

/// Patch 5 修改 1（设计裁决：磁盘实况优先）：账本 enabled 但磁盘为真目录的
/// 漂移态，拍基底不再 UNIQUE 崩——剔除 兔维斯 条目、按 native 记录恰一条，
/// sweep 对它的处置走「暂存」而非「停用」
#[test]
fn scan_tool_state_dedups_drifted_ledger_entry_to_native() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::{snapshot, sweep};
    use multi_agents_manager_lib::services::{disable_skill_for_tool, enable_skill_for_tool};

    // 漂移现场：账本 enabled + 同名真目录（W5 还原内容后名册未销的形态）。
    // 登记线（2026-09-16 裁决）：行须为登记原生（is_native=1 AND source_tool），
    // 真目录才参与快照/暂存判定
    let ssot = dirs::home_dir().unwrap().join(".tuvis/skills/v2m1-drift-x");
    std::fs::create_dir_all(&ssot).unwrap();
    std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m1-drift-x".into(),
        kind: "skill".into(),
        name: "v2m1-drift-x".into(),
        description: None,
        source_path: ssot.to_string_lossy().to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: Some("claude".into()),
        is_native: true,
    })
    .unwrap();
    enable_skill_for_tool("v2m1-drift-x", "claude").unwrap();
    let drift = dirs::home_dir()
        .unwrap()
        .join(".claude/skills/v2m1-drift-x");
    remove_link_or_leftover(&drift);
    std::fs::create_dir_all(&drift).unwrap();
    std::fs::write(drift.join("SKILL.md"), "real").unwrap();

    // 当前实现：兔维斯 条目 + 原生条目同 ext_id → 拍基底 UNIQUE 崩（红）
    snapshot::capture_base_snapshot("claude").unwrap();
    let (_, items) = database::get_base_snapshot("claude").unwrap();
    let hits: Vec<_> = items
        .iter()
        .filter(|i| i.extension_id == "skill-v2m1-drift-x")
        .collect();
    assert_eq!(hits.len(), 1, "漂移项快照恰一条: {:?}", items);
    assert_eq!(hits[0].origin, "native", "按磁盘实况记为 native");

    // sweep 计划：走暂存（native 路径）而非停用
    let plan = sweep::plan_sweep("claude", &[]);
    assert!(
        plan.stash_native.contains(&"v2m1-drift-x".to_string()),
        "漂移项应进暂存计划: {:?}",
        plan.stash_native
    );
    assert!(
        !plan
            .disable_mam
            .iter()
            .any(|(id, _)| id == "skill-v2m1-drift-x"),
        "漂移项不得进停用计划: {:?}",
        plan.disable_mam
    );

    // 清场：禁用 assignment、移除真目录与快照，补删 SSOT 目录与 extensions 行
    database::destroy_base_snapshot("claude").unwrap();
    let _ = disable_skill_for_tool("v2m1-drift-x", "claude");
    let _ = std::fs::remove_dir_all(&drift);
    let _ = std::fs::remove_dir_all(&ssot);
    let _ = database::delete_extension("skill-v2m1-drift-x");
}

/// Patch 5 修改 2（评审 Minor 1）：首次应用的瞬态（快照已存、active 未设）
/// 也视为会话中，孤儿恢复不得回移——否则启动线程会拆掉进行中的应用
#[test]
fn recover_orphans_treats_unactivated_snapshot_as_in_session() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::stash;

    let codex_dir = dirs::home_dir().unwrap().join(".codex/skills");
    std::fs::create_dir_all(codex_dir.join("v2m1-tr-native")).unwrap();
    std::fs::write(codex_dir.join("v2m1-tr-native/SKILL.md"), "n").unwrap();
    stash::stash_native_skill("codex", "v2m1-tr-native", &codex_dir.join("v2m1-tr-native"))
        .unwrap();
    // 瞬态：快照在、active 未设（apply 的拍基底与设激活之间）
    database::save_base_snapshot("codex", None, &[]).unwrap();

    let n = stash::recover_orphans();
    assert_eq!(n, 0, "瞬态期不得回移");
    assert!(
        stash::stash_dir("codex").join("v2m1-tr-native").is_dir(),
        "瞬态期暂存项应保留"
    );
    assert_eq!(database::unrestored_stash(Some("codex")).len(), 1);

    // 清场：销快照（守卫解除）后回移，恢复现场
    database::destroy_base_snapshot("codex").unwrap();
    stash::restore_all_for_tool("codex");
    assert!(codex_dir.join("v2m1-tr-native/SKILL.md").exists());
}

/// Patch 6（终审 Critical 数据丢失修复）：恢复对账循环的漂移防护——
/// 快照内任意 origin 的项都不属于「会话新增」，按 native 记录的漂移项
/// （账本 enabled + 真目录）恢复后真目录已回移，走 disable 会把真目录删掉
#[test]
fn restore_roundtrip_preserves_drifted_real_dir() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::enable_skill_for_tool;
    use multi_agents_manager_lib::services::preset::{apply_preset, restore_tool};

    // 漂移现场：skill-v2m1-dl-x 账本 enabled + 同名真目录（有内容）。
    // 登记线（2026-09-16 裁决）：行须为登记原生，真目录才参与快照/暂存判定
    let ssot = dirs::home_dir().unwrap().join(".tuvis/skills/v2m1-dl-x");
    std::fs::create_dir_all(&ssot).unwrap();
    std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m1-dl-x".into(),
        kind: "skill".into(),
        name: "v2m1-dl-x".into(),
        description: None,
        source_path: ssot.to_string_lossy().to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: Some("claude".into()),
        is_native: true,
    })
    .unwrap();
    enable_skill_for_tool("v2m1-dl-x", "claude").unwrap();
    let drift_dir = dirs::home_dir().unwrap().join(".claude/skills/v2m1-dl-x");
    remove_link_or_leftover(&drift_dir);
    std::fs::create_dir_all(&drift_dir).unwrap();
    std::fs::write(drift_dir.join("SKILL.md"), "用户的真实内容").unwrap();

    // 预设只含另一项（不含漂移项）→ 漂移项按 native 暂存
    let ssot_a = dirs::home_dir().unwrap().join(".tuvis/skills/v2m1-dl-a");
    std::fs::create_dir_all(&ssot_a).unwrap();
    std::fs::write(ssot_a.join("SKILL.md"), "x").unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m1-dl-a".into(),
        kind: "skill".into(),
        name: "v2m1-dl-a".into(),
        description: None,
        source_path: ssot_a.to_string_lossy().to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: None,
        is_native: false,
    })
    .unwrap();
    let pid =
        database::create_preset("v2m1-dl", &[("skill-v2m1-dl-a".into(), "skill".into())]).unwrap();
    apply_preset(&pid, "claude").unwrap();
    assert!(
        !drift_dir.exists()
            && database::unrestored_stash(Some("claude"))
                .iter()
                .any(|e| e.skill_name == "v2m1-dl-x"),
        "前置：漂移真目录应已被独占清扫暂存"
    );

    // 恢复默认：真目录回移 = 基底态两项并存（账本 enabled + 真目录）
    restore_tool("claude").unwrap();

    assert!(
        drift_dir.join("SKILL.md").exists()
            && std::fs::read_to_string(drift_dir.join("SKILL.md")).unwrap() == "用户的真实内容",
        "漂移真目录必须原样保留（当前实现会被 disable 循环删除）"
    );
    let assignments = database::list_assignments("claude");
    assert!(
        assignments
            .iter()
            .any(|a| a.extension_id == "skill-v2m1-dl-x" && a.enabled),
        "账本 enabled 是基底态的一部分，不得被停用"
    );
    assert!(
        database::get_base_snapshot("claude").is_none(),
        "恢复后快照销毁（会话级生命周期不变）"
    );

    // 清场
    let _ = database::upsert_assignment("skill-v2m1-dl-x", "claude", false, "missing");
    let _ = database::upsert_assignment("skill-v2m1-dl-a", "claude", false, "missing");
    let _ = std::fs::remove_dir_all(&drift_dir);
    let _ = std::fs::remove_dir_all(&ssot_a);
}

/// M2 开工前置回归锁（裁决 2 / plan Task 1）：MCP 配置段独占往返——
/// 空预设 apply 清扫：assignment disabled + ~/.claude.json mcpServers 段移除；
/// restore_tool 精确重建：restored_mam 含之 + assignment enabled + 配置段重写
#[test]
fn mcp_sweep_restore_roundtrip() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::mcp::tool_mcp_config_path;
    use multi_agents_manager_lib::services::preset::{apply_preset, restore_tool};
    use multi_agents_manager_lib::services::toggle_mcp;

    let claude_json = tool_mcp_config_path("claude").unwrap();
    let section_has = |name: &str| {
        let content = std::fs::read_to_string(&claude_json).unwrap_or_default();
        let root: serde_json::Value = serde_json::from_str(&content).unwrap_or_default();
        root["mcpServers"].get(name).is_some()
    };

    // 基底：MCP 经真实导入路径入仓（save_mcp_config 落仓库文件 + 注册表行），再为 claude 启用
    multi_agents_manager_lib::commands::resource::save_mcp_config(
        "v2m2-mcp-a".into(),
        "npx".into(),
        vec![],
        Default::default(),
    )
    .unwrap();
    assert!(
        database::list_extensions()
            .iter()
            .any(|e| e.id == "mcp-v2m2-mcp-a"),
        "前置：注册表行应在（save_mcp_config 落表）"
    );
    toggle_mcp("v2m2-mcp-a", "claude", true).unwrap();
    assert!(
        database::list_assignments("claude")
            .iter()
            .any(|a| a.extension_id == "mcp-v2m2-mcp-a" && a.enabled),
        "前置：assignment 应 enabled"
    );
    assert!(section_has("v2m2-mcp-a"), "前置：mcpServers 段应已写入");
    assert!(database::get_tool_enabled("claude"), "前置：工具启用中");

    // 空预设 apply → 独占清扫：assignment disabled + 配置段移除
    //（断言用 contains——共享 HOME 下其他测试可能残留启用项同被清扫）
    let pid = database::create_preset("v2m2-mcp-sweep", &[]).unwrap();
    let r = apply_preset(&pid, "claude").unwrap();
    assert!(
        r.disabled.contains(&"mcp-v2m2-mcp-a".to_string()),
        "disabled 应含 MCP 项: {:?}",
        r.disabled
    );
    assert!(
        !r.failures.iter().any(|f| f.contains("v2m2-mcp-a")),
        "MCP 清扫不得失败: {:?}",
        r.failures
    );
    let asg = database::list_assignments("claude")
        .into_iter()
        .find(|a| a.extension_id == "mcp-v2m2-mcp-a")
        .expect("assignment 行应存在");
    assert!(!asg.enabled, "清扫后 assignment 应 disabled");
    assert_eq!(asg.link_status, "missing");
    assert!(!section_has("v2m2-mcp-a"), "清扫后配置段应移除");

    // restore → 精确重建：配置段重写 + assignment enabled
    let rr = restore_tool("claude").unwrap();
    assert!(
        rr.restored_mam.contains(&"mcp-v2m2-mcp-a".to_string()),
        "restored_mam 应含 MCP 项: {:?}",
        rr.restored_mam
    );
    let asg = database::list_assignments("claude")
        .into_iter()
        .find(|a| a.extension_id == "mcp-v2m2-mcp-a")
        .expect("assignment 行应存在");
    assert!(asg.enabled, "恢复后 assignment 应 enabled");
    assert_eq!(asg.link_status, "valid");
    assert!(section_has("v2m2-mcp-a"), "恢复后配置段应重写");
    let root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&claude_json).unwrap()).unwrap();
    assert_eq!(
        root["mcpServers"]["v2m2-mcp-a"]["command"], "npx",
        "重写的配置段内容应与仓库一致"
    );

    // 清场
    let _ = toggle_mcp("v2m2-mcp-a", "claude", false);
    let _ = database::delete_extension("mcp-v2m2-mcp-a");
    let _ = std::fs::remove_file(dirs::home_dir().unwrap().join(".tuvis/mcp/v2m2-mcp-a.json"));
}

/// M2 开工前置回归锁（裁决 2 / plan Task 1）：file 型插件独占往返——
/// 空预设 apply 清扫断链（工具插件目录项消失）→ restore 链接回来（enabled）
#[test]
fn plugin_file_sweep_restore_v2m2_plug() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::{apply_preset, restore_tool};
    use multi_agents_manager_lib::services::toggle_plugin;

    let home = dirs::home_dir().unwrap();
    // SSOT 仓库放真目录 + 注册表行（与生产扫描登记 file 型插件同形态：tags=Some("file")）
    let ssot = home.join(".tuvis/plugins/v2m2-plug-a");
    std::fs::create_dir_all(&ssot).unwrap();
    std::fs::write(ssot.join("plugin.json"), "{}").unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "plugin-v2m2-plug-a".into(),
        kind: "plugin".into(),
        name: "v2m2-plug-a".into(),
        description: None,
        source_path: ssot.to_string_lossy().to_string(),
        source_url: None,
        version: None,
        tags: Some("file".into()),
        suite: None,
        source_tool: Some("claude".into()),
        is_native: false,
    })
    .unwrap();
    toggle_plugin("v2m2-plug-a", "claude", true, "file").unwrap();
    let target = home.join(".claude/plugins/v2m2-plug-a");
    assert!(
        target.join("plugin.json").exists(),
        "前置：链接在场且可穿透"
    );
    assert!(database::get_tool_enabled("claude"), "前置：工具启用中");

    // 空预设 apply → 独占清扫：断链 + assignment disabled
    let pid = database::create_preset("v2m2-plug-sweep", &[]).unwrap();
    let r = apply_preset(&pid, "claude").unwrap();
    assert!(
        r.disabled.contains(&"plugin-v2m2-plug-a".to_string()),
        "disabled 应含插件项: {:?}",
        r.disabled
    );
    assert!(
        !r.failures.iter().any(|f| f.contains("v2m2-plug-a")),
        "插件清扫不得失败: {:?}",
        r.failures
    );
    assert!(
        std::fs::symlink_metadata(&target).is_err(),
        "清扫后工具插件目录项应消失"
    );
    let asg = database::list_assignments("claude")
        .into_iter()
        .find(|a| a.extension_id == "plugin-v2m2-plug-a")
        .expect("assignment 行应存在");
    assert!(!asg.enabled, "清扫后 assignment 应 disabled");

    // restore → 链接回来 + assignment enabled
    let rr = restore_tool("claude").unwrap();
    assert!(
        rr.restored_mam.contains(&"plugin-v2m2-plug-a".to_string()),
        "restored_mam 应含插件项: {:?}",
        rr.restored_mam
    );
    let meta = std::fs::symlink_metadata(&target).expect("恢复后工具插件目录项应在场");
    assert!(meta.file_type().is_symlink(), "恢复的应是符号链接");
    assert!(target.join("plugin.json").exists(), "链接应可穿透到 SSOT");
    let asg = database::list_assignments("claude")
        .into_iter()
        .find(|a| a.extension_id == "plugin-v2m2-plug-a")
        .expect("assignment 行应存在");
    assert!(asg.enabled, "恢复后 assignment 应 enabled");
    assert_eq!(asg.link_status, "valid");

    // 清场
    let _ = toggle_plugin("v2m2-plug-a", "claude", false, "file");
    let _ = std::fs::remove_dir_all(&ssot);
    let _ = database::delete_extension("plugin-v2m2-plug-a");
}

/// M2 开工前置回归锁（裁决 2 / plan Task 1）：config 型插件独占往返——
/// SSOT 为仓库内 .json 条目文件（生产扫描对文件形态登记 tags=Some("config")），
/// 空预设 apply 清扫摘 ~/.claude/settings.json plugins 段条目 → restore 重写 + enabled
#[test]
fn plugin_config_sweep_restore_v2m2_plugcfg() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::{apply_preset, restore_tool};
    use multi_agents_manager_lib::services::toggle_plugin;

    let home = dirs::home_dir().unwrap();
    // config 型插件的 SSOT 是仓库内的 .json 条目文件（toggle 从中读 entries）
    let repo_json = home.join(".tuvis/plugins/v2m2-plug-cfg.json");
    std::fs::write(&repo_json, r#"{"source":"v2m2"}"#).unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "plugin-v2m2-plug-cfg".into(),
        kind: "plugin".into(),
        name: "v2m2-plug-cfg".into(),
        description: None,
        source_path: repo_json.to_string_lossy().to_string(),
        source_url: None,
        version: None,
        tags: Some("config".into()),
        suite: None,
        source_tool: Some("claude".into()),
        is_native: false,
    })
    .unwrap();
    toggle_plugin("v2m2-plug-cfg", "claude", true, "config").unwrap();
    let settings = home.join(".claude/settings.json");
    let plugins_has = |name: &str| {
        let content = std::fs::read_to_string(&settings).unwrap_or_default();
        let root: serde_json::Value = serde_json::from_str(&content).unwrap_or_default();
        root["plugins"].get(name).is_some()
    };
    assert!(plugins_has("v2m2-plug-cfg"), "前置：plugins 段条目应已写入");
    assert!(database::get_tool_enabled("claude"), "前置：工具启用中");

    // 空预设 apply → 独占清扫：条目摘除 + assignment disabled
    let pid = database::create_preset("v2m2-plugcfg-sweep", &[]).unwrap();
    let r = apply_preset(&pid, "claude").unwrap();
    assert!(
        r.disabled.contains(&"plugin-v2m2-plug-cfg".to_string()),
        "disabled 应含插件项: {:?}",
        r.disabled
    );
    assert!(
        !r.failures.iter().any(|f| f.contains("v2m2-plug-cfg")),
        "插件清扫不得失败: {:?}",
        r.failures
    );
    assert!(!plugins_has("v2m2-plug-cfg"), "清扫后 plugins 段条目应移除");
    let asg = database::list_assignments("claude")
        .into_iter()
        .find(|a| a.extension_id == "plugin-v2m2-plug-cfg")
        .expect("assignment 行应存在");
    assert!(!asg.enabled, "清扫后 assignment 应 disabled");

    // restore → 条目重写 + assignment enabled
    let rr = restore_tool("claude").unwrap();
    assert!(
        rr.restored_mam
            .contains(&"plugin-v2m2-plug-cfg".to_string()),
        "restored_mam 应含插件项: {:?}",
        rr.restored_mam
    );
    assert!(plugins_has("v2m2-plug-cfg"), "恢复后 plugins 段条目应重写");
    let root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(
        root["plugins"]["v2m2-plug-cfg"]["source"], "v2m2",
        "重写的条目内容应与仓库一致"
    );
    let asg = database::list_assignments("claude")
        .into_iter()
        .find(|a| a.extension_id == "plugin-v2m2-plug-cfg")
        .expect("assignment 行应存在");
    assert!(asg.enabled, "恢复后 assignment 应 enabled");
    assert_eq!(asg.link_status, "valid");

    // 清场
    let _ = toggle_plugin("v2m2-plug-cfg", "claude", false, "config");
    let _ = std::fs::remove_file(&repo_json);
    let _ = database::delete_extension("plugin-v2m2-plug-cfg");
}

/// Task 14（spec §13 检测侧收口）：preset_health 三源聚合——不变量违背 /
/// 未恢复暂存 / 账本-磁盘漂移各造一条，断言齐出
#[test]
fn preset_health_aggregates_three_sources_v2m2() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database::{self, BaseSnapshotItemRecord};
    use multi_agents_manager_lib::services::preset::preset_health;

    // ① 不变量违背：快照在、active 为 None（检查器只扫 TOOL_IDS，用注册表内的 dsh）
    database::save_base_snapshot(
        "dsh",
        None,
        &[BaseSnapshotItemRecord {
            extension_id: "skill-v2m2-health".into(),
            kind: "skill".into(),
            origin: "mam".into(),
        }],
    )
    .unwrap();

    // ② 未恢复暂存：直接落账本行（unrestored_stash 为纯 SQL 查询，不校验物理文件存在）
    let stash_id = database::record_stash(
        "codex",
        "v2m2-health-native",
        "/tmp/v2m2-health-stash/v2m2-health-native",
        "/tmp/v2m2-health-codex-skills/v2m2-health-native",
    )
    .unwrap();

    // ③ 漂移 L1 缺链：enabled 工具 + enabled assignment + 磁盘无链接无真目录
    //（镜像 reconcile_test.rs 的 L1 造数配方）
    database::set_tool_enabled("codex", true);
    std::fs::create_dir_all(multi_agents_manager_lib::adapter::primary_skill_dir("codex").unwrap())
        .unwrap();
    database::upsert_assignment("skill-v2m2-health-l1", "codex", true, "valid").unwrap();

    let health = preset_health();
    assert!(
        health.invariants.iter().any(|s| s.contains("dsh")),
        "不变量源应含 dsh 违背项: {:?}",
        health.invariants
    );
    assert!(
        health
            .stash_pending
            .iter()
            .any(|s| s.id == stash_id && s.skill_name == "v2m2-health-native"),
        "暂存源应含未恢复条目: {:?}",
        health.stash_pending
    );
    assert!(
        health
            .drift
            .iter()
            .any(|d| d.kind == "L1" && d.extension_id == "skill-v2m2-health-l1"),
        "漂移源应含 L1 缺链项: {:?}",
        health.drift
    );

    // 清场（尽力而为，镜像文件纪律）
    let _ = database::destroy_base_snapshot("dsh");
    let _ = database::mark_stash_restored(stash_id);
    let _ = database::delete_assignments_for("skill-v2m2-health-l1");
}

/// 工具内建原生技能保护（用户裁决 2026-09-16）：识别即常驻——
/// ① codex 内建目录 `.system`（静态清单命中，带 `.codex-system-skills.marker`
/// 实证标记）不进快照；② 未登记的磁盘原生目录（无 is_native=1 AND
/// source_tool 行）登记线兜底同样视为常驻、不进快照；③ 登记的原生目录
/// 照常进快照（工具私有预设的消费前提不受影响）
#[test]
fn builtin_and_unregistered_native_dirs_never_enter_snapshot_v2m2() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::snapshot;

    let codex_dir = dirs::home_dir().unwrap().join(".codex/skills");
    // ① 内建 .system：静态清单 + marker 双证据
    std::fs::create_dir_all(codex_dir.join(".system")).unwrap();
    std::fs::write(codex_dir.join(".system/.codex-system-skills.marker"), "").unwrap();
    // ② 未登记普通目录（用户手装、兔维斯 无账）
    std::fs::create_dir_all(codex_dir.join("v2m2-plain-x")).unwrap();
    std::fs::write(codex_dir.join("v2m2-plain-x/SKILL.md"), "plain").unwrap();
    // ③ 登记目录（导入登记的标准形态：is_native=1 AND source_tool=codex）
    std::fs::create_dir_all(codex_dir.join("v2m2-reg-x")).unwrap();
    std::fs::write(codex_dir.join("v2m2-reg-x/SKILL.md"), "reg").unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m2-reg-x".into(),
        kind: "skill".into(),
        name: "v2m2-reg-x".into(),
        description: None,
        source_path: "/tmp/v2m2-reg-x".into(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: Some("codex".into()),
        is_native: true,
    })
    .unwrap();

    let state = snapshot::scan_tool_state("codex");
    let ids: Vec<String> = state.iter().map(|i| i.extension_id.clone()).collect();
    assert!(
        !ids.iter().any(|id| id == "skill-.system"),
        "内建 .system 不得进快照: {:?}",
        ids
    );
    assert!(
        !ids.iter().any(|id| id == "skill-v2m2-plain-x"),
        "未登记原生目录视为常驻，不得进快照: {:?}",
        ids
    );
    let reg = state
        .iter()
        .find(|i| i.extension_id == "skill-v2m2-reg-x")
        .expect("登记原生目录应照常进快照");
    assert_eq!(reg.origin, "native");

    // 清场（目录与登记行都收走，避免污染后续测试的扫描现场）
    let _ = database::delete_extension("skill-v2m2-reg-x");
    let _ = std::fs::remove_dir_all(codex_dir.join(".system"));
    let _ = std::fs::remove_dir_all(codex_dir.join("v2m2-plain-x"));
    let _ = std::fs::remove_dir_all(codex_dir.join("v2m2-reg-x"));
}

/// 派发拍平（用户裁决 2026-09-17）：SSOT 仓库保持套件层级原样，但派发到
/// 工具的一切磁盘链接名（工具 skill 目录 / Layer2）一律拍平为 套件-技能名
/// （`/` → `-`）；账本身份（assignment 的 extension_id）保持嵌套规范名
#[test]
fn nested_skill_dispatches_flat_links() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::enable_skill_for_tool;

    let home = dirs::home_dir().unwrap();
    // SSOT 嵌套套件技能（仓库层级不动）+ extensions 登记（嵌套规范名）
    let inner = home.join(".tuvis/skills/v2m2-suite9/inner");
    std::fs::create_dir_all(&inner).unwrap();
    std::fs::write(inner.join("SKILL.md"), "x").unwrap();
    database::ensure_extension(&database::ExtensionRecord {
        id: "skill-v2m2-suite9/inner".into(),
        kind: "skill".into(),
        name: "v2m2-suite9/inner".into(),
        description: None,
        source_path: inner.to_string_lossy().to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: None,
        is_native: false,
    })
    .unwrap();

    enable_skill_for_tool("v2m2-suite9/inner", "claude").unwrap();

    // 工具 skill 目录：拍平名链接在场，且未建嵌套目录
    let claude_dir = home.join(".claude/skills");
    let tool_link = claude_dir.join("v2m2-suite9-inner");
    assert!(
        tool_link.is_symlink() && tool_link.exists(),
        "工具目录应为拍平名符号链接: {}",
        tool_link.display()
    );
    assert!(
        tool_link.join("SKILL.md").exists(),
        "链接应可穿透到 SSOT 内容"
    );
    assert!(
        !claude_dir.join("v2m2-suite9").exists(),
        "不得在工具目录建嵌套套件目录"
    );
    // Layer2：同为拍平名链接
    let layer2_link = home.join(".tuvis/active/claude/v2m2-suite9-inner");
    assert!(
        layer2_link.is_symlink(),
        "Layer2 应为拍平名链接: {}",
        layer2_link.display()
    );
    assert!(!home.join(".tuvis/active/claude/v2m2-suite9").exists());
    // 账本身份：assignment 行 extension_id 仍是嵌套规范名
    let asg = database::list_assignments("claude")
        .into_iter()
        .find(|a| a.extension_id == "skill-v2m2-suite9/inner")
        .expect("assignment 行应按嵌套规范名记账");
    assert!(asg.enabled);

    // 清理（删目录 + delete_extension + assignments）
    let _ =
        multi_agents_manager_lib::services::disable_skill_for_tool("v2m2-suite9/inner", "claude");
    let _ = database::delete_assignments_for("skill-v2m2-suite9/inner");
    let _ = database::delete_extension("skill-v2m2-suite9/inner");
    let _ = std::fs::remove_dir_all(inner.parent().unwrap());
}

/// 派发拍平的禁用侧：disable 按拍平名清链——工具目录与 Layer2 的拍平链接
/// 都消失（账本行落 disabled，extension_id 仍为嵌套规范名）
#[test]
fn nested_skill_disable_removes_flat_links() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::{disable_skill_for_tool, enable_skill_for_tool};

    let home = dirs::home_dir().unwrap();
    let inner = home.join(".tuvis/skills/v2m2-suite9/inner");
    std::fs::create_dir_all(&inner).unwrap();
    std::fs::write(inner.join("SKILL.md"), "x").unwrap();
    database::ensure_extension(&database::ExtensionRecord {
        id: "skill-v2m2-suite9/inner".into(),
        kind: "skill".into(),
        name: "v2m2-suite9/inner".into(),
        description: None,
        source_path: inner.to_string_lossy().to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: None,
        is_native: false,
    })
    .unwrap();
    enable_skill_for_tool("v2m2-suite9/inner", "claude").unwrap();
    let claude_dir = home.join(".claude/skills");
    let layer2_dir = home.join(".tuvis/active/claude");
    assert!(
        claude_dir.join("v2m2-suite9-inner").is_symlink(),
        "前置：工具目录拍平链接在场"
    );
    assert!(
        layer2_dir.join("v2m2-suite9-inner").is_symlink(),
        "前置：Layer2 拍平链接在场"
    );

    disable_skill_for_tool("v2m2-suite9/inner", "claude").unwrap();

    assert!(
        !claude_dir.join("v2m2-suite9-inner").exists(),
        "禁用后工具目录拍平链接应消失"
    );
    assert!(
        !layer2_dir.join("v2m2-suite9-inner").exists(),
        "禁用后 Layer2 拍平链接应消失"
    );
    let asg = database::list_assignments("claude")
        .into_iter()
        .find(|a| a.extension_id == "skill-v2m2-suite9/inner")
        .expect("assignment 行应仍在（disabled）");
    assert!(!asg.enabled);

    // 清理
    let _ = database::delete_assignments_for("skill-v2m2-suite9/inner");
    let _ = database::delete_extension("skill-v2m2-suite9/inner");
    let _ = std::fs::remove_dir_all(inner.parent().unwrap());
}

/// 拍平碰撞守卫（用户裁决 2026-09-17）：仓库内存在与拍平名同名的另一平铺
/// 技能（v2m2-flat-c-skill/）时，派发嵌套技能（v2m2-flat-c/skill）会被拒绝
/// ——两条账目将争同一个磁盘链接名；拒绝时零副作用（不建任何链接）
#[test]
fn nested_dispatch_collision_guard_rejects_flat_namesake() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::services::enable_skill_for_tool;

    let home = dirs::home_dir().unwrap();
    // 嵌套技能 v2m2-flat-c/skill 与字面同名平铺技能 v2m2-flat-c-skill 并存
    let nested = home.join(".tuvis/skills/v2m2-flat-c/skill");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("SKILL.md"), "nested").unwrap();
    let namesake = home.join(".tuvis/skills/v2m2-flat-c-skill");
    std::fs::create_dir_all(&namesake).unwrap();
    std::fs::write(namesake.join("SKILL.md"), "flat").unwrap();

    let err = enable_skill_for_tool("v2m2-flat-c/skill", "claude").unwrap_err();
    assert!(err.contains("拍平名"), "错误应点名拍平名冲突: {}", err);
    // 零副作用：工具目录与 Layer2 均未建链接（两种名形都不在）
    let claude_dir = home.join(".claude/skills");
    assert!(!claude_dir.join("v2m2-flat-c-skill").exists());
    assert!(!claude_dir.join("v2m2-flat-c").exists());
    assert!(!home.join(".tuvis/active/claude/v2m2-flat-c-skill").exists());
    // SSOT 双方原样保留
    assert!(nested.join("SKILL.md").exists() && namesake.join("SKILL.md").exists());

    // 清理
    let _ = std::fs::remove_dir_all(nested.parent().unwrap());
    let _ = std::fs::remove_dir_all(&namesake);
}

/// 反向拍平碰撞守卫（终审 Important#1，控制器裁决）：正向守卫挡「仓库已有
/// 平铺同名」；反向——先启用的嵌套技能已在工具目录留下拍平链接，后启用
/// 字面平铺同名技能——必须拒绝（否则 create_link 静默删掉嵌套技能的拍平
/// 链接换挂平铺内容：错误内容派发 + 账本双 enabled + drift 不可见）。
/// 先停用占用名字的嵌套技能后放行
#[test]
fn flat_namesake_enable_rejected_while_nested_flat_link_present_v2m2() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::{disable_skill_for_tool, enable_skill_for_tool};

    let home = dirs::home_dir().unwrap();
    let claude_dir = home.join(".claude/skills");
    let mk_ext = |id: &str, ssot: &std::path::Path| {
        database::ensure_extension(&database::ExtensionRecord {
            id: id.into(),
            kind: "skill".into(),
            name: id.trim_start_matches("skill-").into(),
            description: None,
            source_path: ssot.to_string_lossy().to_string(),
            source_url: None,
            version: None,
            tags: None,
            suite: None,
            source_tool: None,
            is_native: false,
        })
        .unwrap();
    };

    // ① 先启用嵌套技能 v2m2-flat2/a → 工具目录与 Layer2 产生拍平链接 v2m2-flat2-a
    let nested = home.join(".tuvis/skills/v2m2-flat2/a");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("SKILL.md"), "nested").unwrap();
    mk_ext("skill-v2m2-flat2/a", &nested);
    enable_skill_for_tool("v2m2-flat2/a", "claude").unwrap();
    let flat_link = claude_dir.join("v2m2-flat2-a");
    let layer2_link = home.join(".tuvis/active/claude/v2m2-flat2-a");
    assert!(flat_link.is_symlink(), "前置：嵌套技能的拍平链接在场");

    // ② 再造平铺技能 v2m2-flat2-a（SSOT）并启用 → 拒绝（拍平名冲突）
    let flat = home.join(".tuvis/skills/v2m2-flat2-a");
    std::fs::create_dir_all(&flat).unwrap();
    std::fs::write(flat.join("SKILL.md"), "flat").unwrap();
    mk_ext("skill-v2m2-flat2-a", &flat);
    let err = enable_skill_for_tool("v2m2-flat2-a", "claude").unwrap_err();
    assert!(err.contains("拍平名冲突"), "错误应点名拍平名冲突: {}", err);
    // 零副作用：拍平链接仍换挂着嵌套技能内容，Layer2 未被换挂
    assert_eq!(
        std::fs::read_to_string(flat_link.join("SKILL.md")).unwrap(),
        "nested",
        "被拒后工具目录链接不得换挂平铺技能内容"
    );
    assert_eq!(
        std::fs::read_to_string(layer2_link.join("SKILL.md")).unwrap(),
        "nested",
        "被拒后 Layer2 链接不得换挂"
    );
    // 账本不得双 enabled：嵌套行 enabled、平铺行无 enabled
    let asgs = database::list_assignments("claude");
    assert!(
        asgs.iter()
            .any(|a| a.extension_id == "skill-v2m2-flat2/a" && a.enabled),
        "嵌套技能行应保持 enabled"
    );
    assert!(
        !asgs
            .iter()
            .any(|a| a.extension_id == "skill-v2m2-flat2-a" && a.enabled),
        "平铺技能不得落 enabled 行: {:?}",
        asgs
    );

    // ③ 反向解除占用：停用嵌套技能后启用平铺 → 成功且内容正确
    disable_skill_for_tool("v2m2-flat2/a", "claude").unwrap();
    assert!(!flat_link.exists(), "停用嵌套技能应清掉拍平链接");
    enable_skill_for_tool("v2m2-flat2-a", "claude").unwrap();
    assert!(flat_link.is_symlink(), "停用占用者后启用应放行");
    assert_eq!(
        std::fs::read_to_string(flat_link.join("SKILL.md")).unwrap(),
        "flat",
        "放行后应派发平铺技能内容"
    );

    // 清理
    disable_skill_for_tool("v2m2-flat2-a", "claude").unwrap();
    let _ = database::delete_assignments_for("skill-v2m2-flat2/a");
    let _ = database::delete_assignments_for("skill-v2m2-flat2-a");
    let _ = database::delete_extension("skill-v2m2-flat2/a");
    let _ = database::delete_extension("skill-v2m2-flat2-a");
    let _ = std::fs::remove_dir_all(nested.parent().unwrap());
    let _ = std::fs::remove_dir_all(&flat);
}

/// 常驻=停用防护（用户裁决 2026-09-18）：常驻 on 时手动停用被拒（Err 含
/// 「常驻」），需先关闭常驻；skill / MCP / plugin 三类资源同构
#[test]
fn resident_guard_rejects_manual_disable_v2m2() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::{
        disable_skill_for_tool, enable_skill_for_tool, toggle_mcp, toggle_plugin,
    };

    let home = dirs::home_dir().unwrap();

    // ---- skill：SSOT 真目录 + 登记行，启用后开常驻 ----
    let ssot = home.join(".tuvis/skills/v2m2-res-skill");
    std::fs::create_dir_all(&ssot).unwrap();
    std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m2-res-skill".into(),
        kind: "skill".into(),
        name: "v2m2-res-skill".into(),
        description: None,
        source_path: ssot.to_string_lossy().to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: None,
        is_native: false,
    })
    .unwrap();
    enable_skill_for_tool("v2m2-res-skill", "claude").unwrap();
    let claude_link = home.join(".claude/skills/v2m2-res-skill");
    assert!(claude_link.exists(), "前置：链接在场");

    database::set_tool_resident("claude", "skill-v2m2-res-skill", true).unwrap();
    let err = disable_skill_for_tool("v2m2-res-skill", "claude").unwrap_err();
    assert!(err.contains("常驻"), "skill 停用应被常驻守卫拒绝: {}", err);
    assert!(claude_link.exists(), "被拒后链接不得被动");
    // 关常驻 → 停用成功
    database::set_tool_resident("claude", "skill-v2m2-res-skill", false).unwrap();
    disable_skill_for_tool("v2m2-res-skill", "claude").unwrap();
    assert!(!claude_link.exists(), "关常驻后停用应生效");

    // ---- MCP：真实导入路径入仓 + 启用 + 常驻 ----
    multi_agents_manager_lib::commands::resource::save_mcp_config(
        "v2m2-res-mcp".into(),
        "npx".into(),
        vec![],
        Default::default(),
    )
    .unwrap();
    toggle_mcp("v2m2-res-mcp", "claude", true).unwrap();
    database::set_tool_resident("claude", "mcp-v2m2-res-mcp", true).unwrap();
    let err = toggle_mcp("v2m2-res-mcp", "claude", false).unwrap_err();
    assert!(err.contains("常驻"), "MCP 停用应被常驻守卫拒绝: {}", err);
    assert!(
        database::list_assignments("claude")
            .iter()
            .any(|a| a.extension_id == "mcp-v2m2-res-mcp" && a.enabled),
        "被拒后 assignment 应保持 enabled"
    );
    database::set_tool_resident("claude", "mcp-v2m2-res-mcp", false).unwrap();
    toggle_mcp("v2m2-res-mcp", "claude", false).unwrap();

    // ---- plugin（file 型，照既有 plugin 测试惯例）----
    let plug_ssot = home.join(".tuvis/plugins/v2m2-res-plug");
    std::fs::create_dir_all(&plug_ssot).unwrap();
    std::fs::write(plug_ssot.join("plugin.json"), "{}").unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "plugin-v2m2-res-plug".into(),
        kind: "plugin".into(),
        name: "v2m2-res-plug".into(),
        description: None,
        source_path: plug_ssot.to_string_lossy().to_string(),
        source_url: None,
        version: None,
        tags: Some("file".into()),
        suite: None,
        source_tool: Some("claude".into()),
        is_native: false,
    })
    .unwrap();
    toggle_plugin("v2m2-res-plug", "claude", true, "file").unwrap();
    let plug_target = home.join(".claude/plugins/v2m2-res-plug");
    assert!(plug_target.exists(), "前置：插件链接在场");
    database::set_tool_resident("claude", "plugin-v2m2-res-plug", true).unwrap();
    let err = toggle_plugin("v2m2-res-plug", "claude", false, "file").unwrap_err();
    assert!(err.contains("常驻"), "plugin 停用应被常驻守卫拒绝: {}", err);
    assert!(plug_target.exists(), "被拒后插件链接不得被动");
    database::set_tool_resident("claude", "plugin-v2m2-res-plug", false).unwrap();
    toggle_plugin("v2m2-res-plug", "claude", false, "file").unwrap();

    // 清场
    let _ = database::delete_extension("skill-v2m2-res-skill");
    let _ = database::delete_extension("mcp-v2m2-res-mcp");
    let _ = database::delete_extension("plugin-v2m2-res-plug");
    let _ = std::fs::remove_dir_all(&ssot);
    let _ = std::fs::remove_file(home.join(".tuvis/mcp/v2m2-res-mcp.json"));
    let _ = std::fs::remove_dir_all(&plug_ssot);
}

/// 常驻守卫的启用方向不受影响：常驻 on 时 enable 照常放行
#[test]
fn resident_guard_allows_enable_direction_v2m2() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::{disable_skill_for_tool, enable_skill_for_tool};

    let home = dirs::home_dir().unwrap();
    let ssot = home.join(".tuvis/skills/v2m2-res-en");
    std::fs::create_dir_all(&ssot).unwrap();
    std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m2-res-en".into(),
        kind: "skill".into(),
        name: "v2m2-res-en".into(),
        description: None,
        source_path: ssot.to_string_lossy().to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: None,
        is_native: false,
    })
    .unwrap();

    database::set_tool_resident("claude", "skill-v2m2-res-en", true).unwrap();
    enable_skill_for_tool("v2m2-res-en", "claude").unwrap();
    assert!(
        home.join(".claude/skills/v2m2-res-en").exists(),
        "常驻不拦启用方向"
    );

    // 清场（先关常驻再停用）
    database::set_tool_resident("claude", "skill-v2m2-res-en", false).unwrap();
    disable_skill_for_tool("v2m2-res-en", "claude").unwrap();
    let _ = database::delete_extension("skill-v2m2-res-en");
    let _ = std::fs::remove_dir_all(&ssot);
}

/// 启停守卫（用户裁决 2026-09-16）：enable 目标命中工具内建原生技能 →
/// 拒绝且目录原样保留（识别即保护，兔维斯 不得接管/替换/删除）
#[test]
fn enable_skill_rejects_builtin_native_dir_v2m2() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::services::enable_skill_for_tool;

    let codex_dir = dirs::home_dir().unwrap().join(".codex/skills");
    std::fs::create_dir_all(codex_dir.join(".system")).unwrap();
    std::fs::write(codex_dir.join(".system/SKILL.md"), "builtin").unwrap();

    let err = enable_skill_for_tool(".system", "codex").unwrap_err();
    assert!(err.contains("内建"), "错误应点名内建保护: {}", err);
    assert!(
        codex_dir.join(".system").is_dir() && codex_dir.join(".system/SKILL.md").exists(),
        "内建目录必须原样保留，不得被替换为链接或删除"
    );
    assert!(
        !codex_dir.join(".system").is_symlink(),
        "内建目录不得被替换为链接"
    );

    // 清场
    let _ = std::fs::remove_dir_all(codex_dir.join(".system"));
}

/// 启停守卫对称（终审 Important#2）：disable 以内建目录名调用 → 拒绝且目录
/// 逐字节原样——remove_link 对真目录走 remove_dir_all，无守卫会递归删掉
/// 工具内建目录（当前调用方不可达，纵深防御补口；spec 不变量：不可启停）
#[test]
fn disable_skill_rejects_builtin_native_dir_v2m2() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::services::disable_skill_for_tool;

    let codex_dir = dirs::home_dir().unwrap().join(".codex/skills");
    let sys = codex_dir.join(".system");
    std::fs::create_dir_all(&sys).unwrap();
    std::fs::write(sys.join("SKILL.md"), "builtin").unwrap();
    std::fs::write(sys.join(".codex-system-skills.marker"), "v2 marker bytes").unwrap();

    let err = disable_skill_for_tool(".system", "codex").unwrap_err();
    assert!(err.contains("内建"), "错误应点名内建保护: {}", err);
    // 目录逐字节原样：真目录未被递归删除、内容未变
    assert!(sys.is_dir() && !sys.is_symlink(), "内建目录必须原样保留");
    assert_eq!(
        std::fs::read_to_string(sys.join("SKILL.md")).unwrap(),
        "builtin"
    );
    assert_eq!(
        std::fs::read_to_string(sys.join(".codex-system-skills.marker")).unwrap(),
        "v2 marker bytes"
    );

    // 清场
    let _ = std::fs::remove_dir_all(&sys);
}

/// reveal_dir 白名单校验（wave33 Item 4）：canonicalize 后必须以 ~/.tuvis 或
/// ~/.agents 为前缀；越界拒绝、不存在拒绝、白名单内放行（纯校验核，
/// 不触发系统打开动作）
#[test]
fn reveal_dir_whitelist_validation_v2m2() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::commands::resource::ensure_reveal_allowed;

    let home = dirs::home_dir().unwrap();
    // 白名单内：fake HOME 的 ~/.tuvis（support::setup 已建目录树）→ 放行
    let allowed = home.join(".tuvis/skills").to_string_lossy().to_string();
    let ok = ensure_reveal_allowed(&allowed);
    assert!(ok.is_ok(), "白名单内路径应放行: {:?}", ok.err());

    // 越界且存在 → 拒绝并点名范围。用 temp 目录做平台无关的越界样本：白名单 =
    // ~/.tuvis、~/.agents 与导出目录（系统下载目录），temp 不落在任何一个之下
    // （Windows 上 temp 虽在用户 profile 内，但在 home\.tuvis / home\.agents /
    // Downloads 之外）；原先硬编码 Unix 路径 /tmp，Windows 上 canonicalize 直接
    // 报「路径不存在」走不进越界分支
    let out_of_bounds = std::env::temp_dir().to_string_lossy().to_string();
    let err = ensure_reveal_allowed(&out_of_bounds).unwrap_err();
    assert!(err.contains("范围"), "越界应报范围错误: {}", err);

    // 越界且不存在 → 同样拒绝（不静默放行）
    let err = ensure_reveal_allowed(
        &std::env::temp_dir()
            .join("tuvis-nonexistent-dir-for-reveal-test")
            .to_string_lossy(),
    )
    .unwrap_err();
    assert!(!err.is_empty(), "不存在的越界路径必须拒绝");

    // 白名单内但不存在 → 拒绝（路径不存在）
    let missing = home
        .join(".tuvis/v2m2-reveal-nonexistent")
        .to_string_lossy()
        .to_string();
    let err = ensure_reveal_allowed(&missing).unwrap_err();
    assert!(err.contains("不存在"), "白名单内不存在应报不存在: {}", err);
}

/// backfill 陈旧行修剪（wave33 Item 3，修「预设编辑名单重复」根因）：
/// extensions 行 source_path 非空且路径不存在（含 symlink 死链——exists()
/// 跟随链接，死链判不存在）→ 回填时删除该行并连带清 assignments /
/// tool_residents / resource_bindings（防孤儿）；在场技能行不受影响
#[test]
fn backfill_prunes_stale_rows_with_dead_source_paths() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::resource::backfill_registry;

    let home = dirs::home_dir().unwrap();

    // 死行 A：source_path 指向不存在的路径
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m2-prune-dead".into(),
        kind: "skill".into(),
        name: "v2m2-prune-dead".into(),
        description: None,
        source_path: home
            .join(".claude/skills/v2m2-prune-dead")
            .to_string_lossy()
            .to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: Some("claude".into()),
        is_native: true,
    })
    .unwrap();
    // 连带行：assignment + 常驻 + 专属绑定（防孤儿断言用）
    database::upsert_assignment("skill-v2m2-prune-dead", "claude", true, "valid").unwrap();
    database::set_tool_resident("claude", "skill-v2m2-prune-dead", true).unwrap();
    database::upsert_resource_binding("skill-v2m2-prune-dead", "claude", Some("陈旧")).unwrap();

    // 死行 B：symlink 死链（路径项存在但链接目标已消失）
    let dangling = home.join(".tuvis/skills/v2m2-prune-dangling");
    #[cfg(unix)]
    std::os::unix::fs::symlink(home.join(".tuvis/skills/v2m2-prune-nowhere"), &dangling).unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m2-prune-dangling".into(),
        kind: "skill".into(),
        name: "v2m2-prune-dangling".into(),
        description: None,
        source_path: dangling.to_string_lossy().to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: None,
        is_native: false,
    })
    .unwrap();

    // 在场技能行：SSOT 真目录 + SKILL.md → 不得被修剪
    let live = home.join(".tuvis/skills/v2m2-prune-live");
    std::fs::create_dir_all(&live).unwrap();
    std::fs::write(live.join("SKILL.md"), "x").unwrap();

    backfill_registry();

    let exts = database::list_extensions();
    assert!(
        exts.iter().all(|e| e.id != "skill-v2m2-prune-dead"),
        "source_path 失效的行应被修剪: {:?}",
        exts.iter().map(|e| e.id.clone()).collect::<Vec<_>>()
    );
    assert!(
        exts.iter().all(|e| e.id != "skill-v2m2-prune-dangling"),
        "symlink 死链的行应被修剪（exists() 跟随链接判不存在）"
    );
    assert!(
        exts.iter().any(|e| e.id == "skill-v2m2-prune-live"),
        "在场技能行应保留（backfill 已登记）"
    );
    // 连带行清除：assignment / 常驻 / 绑定不留孤儿
    assert!(
        !database::list_assignments("claude")
            .iter()
            .any(|a| a.extension_id == "skill-v2m2-prune-dead"),
        "死行的 assignment 应连带删除"
    );
    assert!(
        !database::list_tool_residents("claude").contains(&"skill-v2m2-prune-dead".to_string()),
        "死行的常驻行应连带删除"
    );
    assert!(
        database::get_resource_binding("skill-v2m2-prune-dead").is_none(),
        "死行的专属绑定应连带删除"
    );

    // 清场
    let _ = database::delete_extension("skill-v2m2-prune-live");
    let _ = std::fs::remove_dir_all(&live);
    let _ = std::fs::remove_file(&dangling);
}

/// 卸载级联常驻行（review Finding 1，裁决修法 a）：常驻 on 的 skill 被卸载 →
/// 常驻行先于停用清理级联删除（守卫自然放行）→ 清链成功 + SSOT 删除 + 无孤儿。
/// 不修则：step1 停用被守卫拒（仅 warn）、step2 仍删 SSOT → 工具侧悬空链接 +
/// 孤儿常驻行使 is_tool_resident 恒 true → reconcile L3-a 永久 Err「先关闭常驻」
/// 而 UI 中资源已消失无从关闭（死局，需手改 DB）
#[test]
fn uninstall_cascades_resident_rows_v2m2() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::commands::manifest::uninstall_resource;
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::enable_skill_for_tool;

    let home = dirs::home_dir().unwrap();
    // 造 skill：SSOT 真目录 + 登记行 + 为 claude 启用 + 开常驻
    let ssot = home.join(".tuvis/skills/v2m2-uni-res");
    std::fs::create_dir_all(&ssot).unwrap();
    std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
    database::insert_extension(&database::ExtensionRecord {
        id: "skill-v2m2-uni-res".into(),
        kind: "skill".into(),
        name: "v2m2-uni-res".into(),
        description: None,
        source_path: ssot.to_string_lossy().to_string(),
        source_url: None,
        version: None,
        tags: None,
        suite: None,
        source_tool: None,
        is_native: false,
    })
    .unwrap();
    enable_skill_for_tool("v2m2-uni-res", "claude").unwrap();
    let tool_link = home.join(".claude/skills/v2m2-uni-res");
    assert!(tool_link.exists(), "前置：工具目录链接在场");
    database::set_tool_resident("claude", "skill-v2m2-uni-res", true).unwrap();

    // 卸载（无 .agents 直链 → 无需确认）
    let outcome = uninstall_resource("skill".into(), "v2m2-uni-res".into(), None).unwrap();
    assert!(!outcome.needs_confirmation, "卸载应一步完成");

    // SSOT 目录已删
    assert!(!ssot.exists(), "SSOT 目录应已删除");
    // 工具目录链接已消失（停用清链成功，不修则守卫拒绝停用留下悬空链接——
    // exists() 跟随链接会漏判悬空，须用 symlink_metadata 判链接本体）
    assert!(
        std::fs::symlink_metadata(&tool_link).is_err(),
        "工具目录链接应随卸载清链消失（常驻行级联后守卫放行），不得留悬空链接"
    );
    // tool_residents 表无该行（无孤儿，is_tool_resident 不再恒 true）
    assert!(
        !database::list_tool_residents("claude").contains(&"skill-v2m2-uni-res".to_string()),
        "常驻行应随卸载级联删除"
    );
    assert!(!database::is_tool_resident("claude", "skill-v2m2-uni-res"));
    // extensions 行也已删（卸载既有语义）
    assert!(
        database::list_extensions()
            .iter()
            .all(|e| e.id != "skill-v2m2-uni-res"),
        "extensions 行应已删除"
    );
}

/// 恢复默认的常驻豁免（review Finding 2）：快照之后新启用且标常驻的资源 X
/// 落在 current−target−base_items 的停用循环——须镜像清扫计划层（sweep.rs）
/// 的 is_tool_resident 豁免检查：跳过停用（保持启用=保护胜出）、不进
/// conflicts（终态本正确，不修则误报「停用失败…常驻…」，与 sweep 静默豁免
/// + residentExempt 报告口径不一致）；快照照常销毁
#[test]
fn restore_exempts_post_snapshot_resident_v2m2() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::{apply_preset, restore_tool};
    use multi_agents_manager_lib::services::{disable_skill_for_tool, enable_skill_for_tool};

    let home = dirs::home_dir().unwrap();
    // 预设项 A + 会话中途才启用的资源 X（此刻都未启用 → 都不在基底）
    for name in ["v2m2-rx-a", "v2m2-rx-x"] {
        let ssot = home.join(".tuvis/skills").join(name);
        std::fs::create_dir_all(&ssot).unwrap();
        std::fs::write(ssot.join("SKILL.md"), "x").unwrap();
        database::insert_extension(&database::ExtensionRecord {
            id: format!("skill-{}", name),
            kind: "skill".into(),
            name: name.into(),
            description: None,
            source_path: ssot.to_string_lossy().to_string(),
            source_url: None,
            version: None,
            tags: None,
            suite: None,
            source_tool: None,
            is_native: false,
        })
        .unwrap();
    }
    let pid =
        database::create_preset("v2m2-rx", &[("skill-v2m2-rx-a".into(), "skill".into())]).unwrap();

    // 1) 应用预设（拍基底：X 未启用、不在 base_items）
    apply_preset(&pid, "claude").unwrap();

    // 2) 会话中途：手动启用 X + 标常驻
    enable_skill_for_tool("v2m2-rx-x", "claude").unwrap();
    let x_link = home.join(".claude/skills/v2m2-rx-x");
    assert!(x_link.exists(), "前置：X 已启用（链接在场）");
    database::set_tool_resident("claude", "skill-v2m2-rx-x", true).unwrap();

    // 3) 恢复默认：X 豁免停用（保持启用）、不误报冲突、快照销毁
    let rr = restore_tool("claude").unwrap();
    assert!(x_link.exists(), "常驻 X 应保持启用（链接在场）");
    assert!(
        !rr.conflicts.iter().any(|c| c.contains("v2m2-rx-x")),
        "常驻 X 不得误报停用冲突: {:?}",
        rr.conflicts
    );
    assert!(
        database::list_assignments("claude")
            .iter()
            .any(|a| a.extension_id == "skill-v2m2-rx-x" && a.enabled),
        "常驻 X 的 assignment 应保持 enabled"
    );
    assert!(
        database::get_base_snapshot("claude").is_none(),
        "恢复后快照应销毁"
    );

    // 清场
    database::set_tool_resident("claude", "skill-v2m2-rx-x", false).unwrap();
    let _ = disable_skill_for_tool("v2m2-rx-x", "claude");
    let _ = disable_skill_for_tool("v2m2-rx-a", "claude");
    let _ = database::delete_extension("skill-v2m2-rx-a");
    let _ = database::delete_extension("skill-v2m2-rx-x");
    let _ = std::fs::remove_dir_all(home.join(".tuvis/skills/v2m2-rx-a"));
    let _ = std::fs::remove_dir_all(home.join(".tuvis/skills/v2m2-rx-x"));
}

/// #64-3 悬空键防御（2026-10-14）：universal 预设含 is_native=1 行 → 命令层直接
/// 拒绝（create 与 update 两口都拦）；工具私有预设含原生项合法（原生项只属于
/// 工具预设）；未登记 id 不拦（前瞻引用既有行为）。
/// 还原动作（变异）：删掉 reject_native_items_in_universal 调用 → 前两段断言红。
#[test]
fn preset_commands_reject_native_items_in_universal() {
    let _guard = PRESET_V2_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    support::setup();
    use multi_agents_manager_lib::commands::preset as preset_cmd;
    use multi_agents_manager_lib::database;

    // 播种：一个原生 skill（is_native=1 + source_tool）+ 一个普通 skill
    for (id, native) in [("skill-v2m3-native-x", true), ("skill-v2m3-plain-x", false)] {
        database::insert_extension(&database::ExtensionRecord {
            id: id.into(),
            kind: "skill".into(),
            name: id.trim_start_matches("skill-").into(),
            description: None,
            source_path: format!("/tmp/{id}"),
            source_url: None,
            version: None,
            tags: None,
            suite: None,
            source_tool: (id.ends_with("native-x")).then(|| "claude".to_string()),
            is_native: native,
        })
        .unwrap();
    }

    // ① create：universal + 原生项 → Err（结构化码可检索）
    let err = preset_cmd::create_preset(
        "v2m3-def-native".into(),
        vec![("skill-v2m3-native-x".into(), "skill".into())],
        None,
        Some("universal".into()),
        None,
    )
    .expect_err("universal 预设含原生项必须被命令层拒绝");
    assert!(
        err.contains("PRESET_NATIVE_IN_UNIVERSAL:skill-v2m3-native-x"),
        "错误应携带结构化码与违规项 id：{err}"
    );

    // ② create：universal + 普通项（含未登记 id）→ Ok（前瞻引用不拦）
    let ok_pid = preset_cmd::create_preset(
        "v2m3-def-ok".into(),
        vec![
            ("skill-v2m3-plain-x".into(), "skill".into()),
            ("skill-v2m3-unregistered".into(), "skill".into()),
        ],
        None,
        Some("universal".into()),
        None,
    )
    .expect("universal + 普通项/未登记项不得误拦");

    // ③ create：工具私有 + 原生项 → Ok（原生项只属于工具预设）
    let tool_pid = preset_cmd::create_preset(
        "v2m3-def-tool".into(),
        vec![("skill-v2m3-native-x".into(), "skill".into())],
        None,
        Some("tool".into()),
        Some("claude".into()),
    )
    .expect("工具私有预设含原生项是合法的");

    // ④ update：把已存在的通用预设改成含原生项 → 同样拒绝
    let err = preset_cmd::update_preset(
        ok_pid.clone(),
        "v2m3-def-ok".into(),
        String::new(),
        "universal".into(),
        None,
        vec![("skill-v2m3-native-x".into(), "skill".into())],
    )
    .expect_err("update 口同样必须拒绝原生项入通用预设");
    assert!(err.contains("PRESET_NATIVE_IN_UNIVERSAL"));

    // 清场
    for pid in [ok_pid, tool_pid] {
        let _ = database::delete_preset(&pid);
    }
    let _ = database::delete_extension("skill-v2m3-native-x");
    let _ = database::delete_extension("skill-v2m3-plain-x");
}
