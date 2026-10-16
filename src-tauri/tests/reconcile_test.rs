// 账本-磁盘对账测试：L1-L4 漂移语义建立在 Unix symlink（read_link 目标判定）之上，
// fixture 全程用 std::os::unix::fs::symlink 构造——文件级 cfg 门（照 linker_test 惯例），
// Windows 交叉编译门禁（ci.yml x86_64-pc-windows-gnu --all-targets）跳过本文件。
#![cfg(unix)]

mod support;

use std::sync::{Mutex, OnceLock};

/// reconcile_test 是独立测试二进制（自己的进程 + 自己的 fake HOME + 全局 SQLite），
/// 二进制内各测试并行跑会共享同一 DB 与磁盘目录互相踩踏 —— 互斥锁强制串行
/// （同 preset_v2_test.rs 的 OnceLock<Mutex<()>> 锁惯例）
static RECONCILE_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// 每个测试第一条语句先拿锁，再 support::setup()
fn acquire_lock() -> std::sync::MutexGuard<'static, ()> {
    RECONCILE_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// L1 缺链：账本 enabled + 磁盘既无链接也无真目录 → kind "L1"
#[test]
fn reconcile_scan_l1_missing_link() {
    let _guard = acquire_lock();
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::resource::reconcile::scan_drift;

    let tool = "codex";
    database::set_tool_enabled(tool, true);
    std::fs::create_dir_all(multi_agents_manager_lib::adapter::primary_skill_dir(tool).unwrap())
        .unwrap();
    database::upsert_assignment("skill-v2m2-rc-l1", tool, true, "valid").unwrap();

    let items = scan_drift();
    let hit = items.iter().find(|d| d.extension_id == "skill-v2m2-rc-l1");
    let hit = hit.expect("L1 缺链应被报告（账本 enabled、磁盘无条目）");
    assert_eq!(hit.kind, "L1");
    assert_eq!(hit.tool_id, tool);
    assert!(
        hit.path.contains("v2m2-rc-l1"),
        "path 应指向缺失条目: {}",
        hit.path
    );

    // 清理（尽力而为，镜像 preset_v2_test.rs 纪律）
    let _ = database::delete_assignments_for("skill-v2m2-rc-l1");
}

/// L2 占位：账本 enabled + 磁盘同名真目录 → kind "L2"
#[test]
fn reconcile_scan_l2_placeholder_dir() {
    let _guard = acquire_lock();
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::resource::reconcile::scan_drift;

    let tool = "codex";
    database::set_tool_enabled(tool, true);
    let dir = multi_agents_manager_lib::adapter::primary_skill_dir(tool).unwrap();
    std::fs::create_dir_all(dir.join("v2m2-rc-l2")).unwrap();
    database::upsert_assignment("skill-v2m2-rc-l2", tool, true, "valid").unwrap();

    let items = scan_drift();
    let hit = items.iter().find(|d| d.extension_id == "skill-v2m2-rc-l2");
    let hit = hit.expect("L2 占位应被报告（账本 enabled、磁盘同名真目录）");
    assert_eq!(hit.kind, "L2");
    assert_eq!(hit.tool_id, tool);
    assert!(
        hit.path.contains("v2m2-rc-l2"),
        "path 应指向占位目录: {}",
        hit.path
    );

    // 清理
    let _ = std::fs::remove_dir_all(dir.join("v2m2-rc-l2"));
    let _ = database::delete_assignments_for("skill-v2m2-rc-l2");
}

/// L3 多链：磁盘有指向 ~/.tuvis 的 兔维斯 链接但账本无 enabled 行 → kind "L3"
#[test]
fn reconcile_scan_l3_unmanaged_link() {
    let _guard = acquire_lock();
    support::setup();
    use multi_agents_manager_lib::services::resource::reconcile::scan_drift;

    let tool = "codex";
    multi_agents_manager_lib::database::set_tool_enabled(tool, true);
    let home = dirs::home_dir().unwrap();
    let tool_dir = multi_agents_manager_lib::adapter::primary_skill_dir(tool).unwrap();
    std::fs::create_dir_all(&tool_dir).unwrap();
    let ssot = home.join(".tuvis/skills/v2m2-rc-l3");
    std::fs::create_dir_all(&ssot).unwrap();
    let link = tool_dir.join("v2m2-rc-l3");
    std::os::unix::fs::symlink(&ssot, &link).unwrap(); // 无 assignment 行（scan 只读 assignments + 磁盘，无需 extensions 行）

    let items = scan_drift();
    let hit = items.iter().find(|d| d.extension_id == "skill-v2m2-rc-l3");
    let hit = hit.expect("L3 多链应被报告（磁盘 兔维斯 链接、账本无行）");
    assert_eq!(hit.kind, "L3");
    assert_eq!(hit.tool_id, tool);
    assert!(
        hit.path.contains("v2m2-rc-l3"),
        "path 应指向链接: {}",
        hit.path
    );

    // 清理
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_dir_all(&ssot);
}

/// L4 外链：磁盘链接指向 ~/.tuvis 之外 → kind "L4"（只报告不接管，账本无关）
#[test]
fn reconcile_scan_l4_external_link() {
    let _guard = acquire_lock();
    support::setup();
    use multi_agents_manager_lib::services::resource::reconcile::scan_drift;

    let tool = "codex";
    multi_agents_manager_lib::database::set_tool_enabled(tool, true);
    let tool_dir = multi_agents_manager_lib::adapter::primary_skill_dir(tool).unwrap();
    std::fs::create_dir_all(&tool_dir).unwrap();
    let outside = std::env::temp_dir().join("v2m2-rc-l4-target");
    std::fs::create_dir_all(&outside).unwrap();
    let link = tool_dir.join("v2m2-rc-l4");
    std::os::unix::fs::symlink(&outside, &link).unwrap();

    let items = scan_drift();
    let hit = items.iter().find(|d| d.extension_id == "skill-v2m2-rc-l4");
    let hit = hit.expect("L4 外链应被报告（链接指向 ~/.tuvis 之外）");
    assert_eq!(hit.kind, "L4");
    assert_eq!(hit.tool_id, tool);
    assert!(
        hit.path.contains("v2m2-rc-l4"),
        "path 应指向链接: {}",
        hit.path
    );

    // 清理
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_dir_all(&outside);
}

/// L1-a：账本 enabled + 磁盘无条目 → 账本为准修磁盘 → 链接重建（指向 ~/.tuvis）
#[test]
fn reconcile_l1_mode_a_rebuilds_link() {
    let _guard = acquire_lock();
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::resource::reconcile::{reconcile_one, scan_drift};

    let tool = "codex";
    let home = dirs::home_dir().unwrap();
    database::set_tool_enabled(tool, true);
    let tool_dir = multi_agents_manager_lib::adapter::primary_skill_dir(tool).unwrap();
    std::fs::create_dir_all(&tool_dir).unwrap();
    // enable 管线要求 SSOT 仓库存在该技能
    let ssot = home.join(".tuvis/skills/v2m2-rc-l1a");
    std::fs::create_dir_all(&ssot).unwrap();
    database::upsert_assignment("skill-v2m2-rc-l1a", tool, true, "valid").unwrap();

    let items = scan_drift();
    let item = items
        .iter()
        .find(|d| d.extension_id == "skill-v2m2-rc-l1a" && d.kind == "L1")
        .expect("L1 缺链应先被扫描报告（构造自检）")
        .clone();

    let outcome = reconcile_one(&item, "a");
    // 终审 Minor #4：结构化字段随结果回带（前端行键映射不再解析 message 文本）
    assert_eq!(
        outcome.extension_id, "skill-v2m2-rc-l1a",
        "L1-a outcome 应携带漂移条目 extension_id"
    );
    assert_eq!(outcome.tool_id, tool, "L1-a outcome 应携带漂移条目 tool_id");
    assert!(
        outcome.fixed,
        "L1-a 应修复（重建链接）: {:?}",
        outcome.message
    );
    assert!(
        !outcome.needs_manual,
        "L1-a 不应升级人工: {:?}",
        outcome.message
    );

    let link = tool_dir.join("v2m2-rc-l1a");
    assert!(
        link.is_symlink(),
        "L1-a 后磁盘应出现链接: {}",
        link.display()
    );
    let target = std::fs::read_link(&link).unwrap();
    assert!(
        target.starts_with(home.join(".tuvis")),
        "链接应指向 ~/.tuvis 之下: {:?}",
        target
    );

    // 清理（尽力而为）
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_file(
        multi_agents_manager_lib::linker::layer2::tool_active_dir(tool).join("v2m2-rc-l1a"),
    );
    let _ = std::fs::remove_dir_all(&ssot);
    let _ = database::delete_assignments_for("skill-v2m2-rc-l1a");
}

/// L2-b：账本 enabled + 同名真目录（内容与 SSOT 不同）→ 磁盘为准回写账本
/// → assignment disabled/missing 且真目录原样保留
#[test]
fn reconcile_l2_mode_b_disables_assignment_keeps_dir() {
    let _guard = acquire_lock();
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::resource::reconcile::{reconcile_one, scan_drift};

    let tool = "codex";
    let home = dirs::home_dir().unwrap();
    database::set_tool_enabled(tool, true);
    let tool_dir = multi_agents_manager_lib::adapter::primary_skill_dir(tool).unwrap();
    let ssot = home.join(".tuvis/skills/v2m2-rc-l2b");
    std::fs::create_dir_all(&ssot).unwrap();
    std::fs::write(ssot.join("SKILL.md"), "ssot-content").unwrap();
    let real_dir = tool_dir.join("v2m2-rc-l2b");
    std::fs::create_dir_all(&real_dir).unwrap();
    std::fs::write(real_dir.join("SKILL.md"), "local-content").unwrap();
    database::upsert_assignment("skill-v2m2-rc-l2b", tool, true, "valid").unwrap();

    let items = scan_drift();
    let item = items
        .iter()
        .find(|d| d.extension_id == "skill-v2m2-rc-l2b" && d.kind == "L2")
        .expect("L2 占位应先被扫描报告（构造自检）")
        .clone();

    let outcome = reconcile_one(&item, "b");
    assert!(
        outcome.fixed,
        "L2-b 应回写账本视为已修复: {:?}",
        outcome.message
    );
    assert!(
        !outcome.needs_manual,
        "L2-b 不应升级人工: {:?}",
        outcome.message
    );

    let row = database::list_assignments(tool)
        .into_iter()
        .find(|a| a.extension_id == "skill-v2m2-rc-l2b")
        .expect("assignment 行应存在");
    assert!(
        !row.enabled,
        "mode b 应以磁盘为准把 assignment 置为 disabled"
    );
    assert_eq!(row.link_status, "missing", "回写状态应为 missing");
    assert_eq!(
        std::fs::read_to_string(real_dir.join("SKILL.md")).unwrap(),
        "local-content",
        "真目录内容必须原样保留"
    );

    // 清理
    let _ = std::fs::remove_dir_all(&real_dir);
    let _ = std::fs::remove_dir_all(&ssot);
    let _ = database::delete_assignments_for("skill-v2m2-rc-l2b");
}

/// L2-a 防删回归锁：真目录内容与 SSOT 不一致 → mode a → needs_manual、fixed=false、
/// 真目录仍在且内容原样（绝不删除现场）
#[test]
fn reconcile_l2_mode_a_mismatch_needs_manual_keeps_scene() {
    let _guard = acquire_lock();
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::resource::reconcile::{reconcile_one, scan_drift};

    let tool = "codex";
    let home = dirs::home_dir().unwrap();
    database::set_tool_enabled(tool, true);
    let tool_dir = multi_agents_manager_lib::adapter::primary_skill_dir(tool).unwrap();
    let ssot = home.join(".tuvis/skills/v2m2-rc-l2a");
    std::fs::create_dir_all(&ssot).unwrap();
    std::fs::write(ssot.join("SKILL.md"), "ssot-content").unwrap();
    let real_dir = tool_dir.join("v2m2-rc-l2a");
    std::fs::create_dir_all(&real_dir).unwrap();
    std::fs::write(real_dir.join("SKILL.md"), "local-content").unwrap();
    database::upsert_assignment("skill-v2m2-rc-l2a", tool, true, "valid").unwrap();

    let items = scan_drift();
    let item = items
        .iter()
        .find(|d| d.extension_id == "skill-v2m2-rc-l2a" && d.kind == "L2")
        .expect("L2 占位应先被扫描报告（构造自检）")
        .clone();

    let outcome = reconcile_one(&item, "a");
    // 终审 Minor #4：needs_manual 路径同样回带结构化字段（L2-a 守卫臂）
    assert_eq!(outcome.extension_id, "skill-v2m2-rc-l2a");
    assert_eq!(outcome.tool_id, tool);
    assert!(
        outcome.needs_manual,
        "L2-a 内容不一致应升级人工: {:?}",
        outcome
    );
    assert!(
        !outcome.fixed,
        "L2-a 内容不一致不算已修复: {:?}",
        outcome.message
    );
    assert!(
        outcome.message.contains("人工"),
        "message 应说明需人工处理: {}",
        outcome.message
    );
    assert!(
        real_dir.is_dir() && !real_dir.is_symlink(),
        "真目录必须仍在且未被替换为链接（绝不删除现场）"
    );
    assert_eq!(
        std::fs::read_to_string(real_dir.join("SKILL.md")).unwrap(),
        "local-content",
        "真目录内容必须原样"
    );

    // 清理
    let _ = std::fs::remove_dir_all(&real_dir);
    let _ = std::fs::remove_dir_all(&ssot);
    let _ = database::delete_assignments_for("skill-v2m2-rc-l2a");
}

/// L3-a：磁盘 兔维斯 链接 + 账本无行 → 账本为准修磁盘 → 链接被清除（目录项消失）
#[test]
fn reconcile_l3_mode_a_removes_link() {
    let _guard = acquire_lock();
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::resource::reconcile::{reconcile_one, scan_drift};

    let tool = "codex";
    let home = dirs::home_dir().unwrap();
    database::set_tool_enabled(tool, true);
    let tool_dir = multi_agents_manager_lib::adapter::primary_skill_dir(tool).unwrap();
    std::fs::create_dir_all(&tool_dir).unwrap();
    let ssot = home.join(".tuvis/skills/v2m2-rc-l3a");
    std::fs::create_dir_all(&ssot).unwrap();
    let link = tool_dir.join("v2m2-rc-l3a");
    std::os::unix::fs::symlink(&ssot, &link).unwrap();

    let items = scan_drift();
    let item = items
        .iter()
        .find(|d| d.extension_id == "skill-v2m2-rc-l3a" && d.kind == "L3")
        .expect("L3 多链应先被扫描报告（构造自检）")
        .clone();

    let outcome = reconcile_one(&item, "a");
    assert!(
        outcome.fixed,
        "L3-a 应清链视为已修复: {:?}",
        outcome.message
    );
    assert!(
        !outcome.needs_manual,
        "L3-a 不应升级人工: {:?}",
        outcome.message
    );
    assert!(
        !link.is_symlink(),
        "L3-a 后链接应被清除（disable 语义含 Layer2 级联）"
    );
    assert!(!link.exists(), "disable 后目录项应消失");

    // 清理（disable 会写一行 disabled assignment，一并清掉）
    let _ = std::fs::remove_dir_all(&ssot);
    let _ = database::delete_assignments_for("skill-v2m2-rc-l3a");
}

/// L4：外链（指向 ~/.tuvis 之外）→ 任意 mode → needs_manual=true 且现场不动
#[test]
fn reconcile_l4_any_mode_needs_manual() {
    let _guard = acquire_lock();
    support::setup();
    use multi_agents_manager_lib::services::resource::reconcile::{reconcile_one, scan_drift};

    let tool = "codex";
    multi_agents_manager_lib::database::set_tool_enabled(tool, true);
    let tool_dir = multi_agents_manager_lib::adapter::primary_skill_dir(tool).unwrap();
    std::fs::create_dir_all(&tool_dir).unwrap();
    let outside = std::env::temp_dir().join("v2m2-rc-l4m-target");
    std::fs::create_dir_all(&outside).unwrap();
    let link = tool_dir.join("v2m2-rc-l4m");
    std::os::unix::fs::symlink(&outside, &link).unwrap();

    let items = scan_drift();
    let item = items
        .iter()
        .find(|d| d.extension_id == "skill-v2m2-rc-l4m" && d.kind == "L4")
        .expect("L4 外链应先被扫描报告（构造自检）")
        .clone();

    for mode in ["a", "b"] {
        let outcome = reconcile_one(&item, mode);
        // 终审 Minor #4：L4 升级人工臂也必须回带结构化字段
        assert_eq!(
            outcome.extension_id, "skill-v2m2-rc-l4m",
            "L4 outcome 应携带 extension_id（mode={}）",
            mode
        );
        assert_eq!(
            outcome.tool_id, tool,
            "L4 outcome 应携带 tool_id（mode={}）",
            mode
        );
        assert!(
            outcome.needs_manual,
            "L4 任意 mode 都应 needs_manual（mode={}）: {:?}",
            mode, outcome
        );
        assert!(
            !outcome.fixed,
            "L4 不算已修复（mode={}）: {:?}",
            mode, outcome.message
        );
        assert!(
            outcome.message.contains("外链"),
            "message 应说明外链不接管（mode={}）: {}",
            mode,
            outcome.message
        );
        assert!(link.is_symlink(), "L4 处置不得动现场（mode={}）", mode);
    }

    // 清理
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_dir_all(&outside);
}

/// #64-1 别名回归：HOME 路径经 symlink 别名（symlink home / macOS `/var →
/// /private/var` 同族）而链接目标以真实拼写落在 ~/.tuvis 之下——字面
/// `starts_with` 两侧拼写不同会把这个 兔维斯 自建链接误报成 L4 外链；修复后
/// 两侧 canonicalize 归一到真实落点，正确识别为 L3（多链，账本无行）。
/// 还原动作（变异）：L4 判定改回字面比较 → 本测试红（hit.kind 变 "L4"）。
#[test]
fn reconcile_scan_alias_home_no_false_l4() {
    let _guard = acquire_lock();
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::resource::reconcile::scan_drift;

    let tool = "codex";
    database::set_tool_enabled(tool, true);
    let real_home = dirs::home_dir().unwrap();
    let tool_dir = multi_agents_manager_lib::adapter::primary_skill_dir(tool).unwrap();
    std::fs::create_dir_all(&tool_dir).unwrap();
    let ssot = real_home.join(".tuvis/skills/v2m2-rc-al4");
    std::fs::create_dir_all(&ssot).unwrap();
    let link = tool_dir.join("v2m2-rc-al4");
    std::os::unix::fs::symlink(&ssot, &link).unwrap(); // 目标用真实拼写（无 assignment 行）

    // 别名 home：alias → 真实 home；HOME 指到别名后 mam_root 按别名词法派生，
    // 与链接目标的真实拼写形成「拼写不同、落点相同」的误报形态
    let alias = std::env::temp_dir().join("v2m2-rc-al4-home");
    let _ = std::fs::remove_file(&alias);
    std::os::unix::fs::symlink(&real_home, &alias).unwrap();
    let prev_home = std::env::var("HOME").unwrap_or_default();
    std::env::set_var("HOME", &alias);

    let items = scan_drift();
    let hit = items
        .iter()
        .find(|d| d.extension_id == "skill-v2m2-rc-al4")
        .expect("账本无行 + 磁盘 兔维斯 链接 → 应报 L3 多链");
    assert_eq!(
        hit.kind, "L3",
        "别名 home 下 兔维斯 链接应识别为 L3（多链），不得误报 L4 外链: {:?}",
        hit
    );

    // 还原 HOME（本文件测试串行，后续用例不受别名影响）+ 现场清理
    std::env::set_var("HOME", &prev_home);
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_dir_all(&ssot);
    let _ = std::fs::remove_file(&alias);
}

/// 批量：reconcile_tool_batch 过滤该工具逐条处置；单条失败不中断；L4 恒 needs_manual
#[test]
fn reconcile_batch_filters_tool_and_l4_manual() {
    let _guard = acquire_lock();
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::resource::reconcile::reconcile_tool_batch;

    let tool = "codex";
    let home = dirs::home_dir().unwrap();
    database::set_tool_enabled(tool, true);
    let tool_dir = multi_agents_manager_lib::adapter::primary_skill_dir(tool).unwrap();
    std::fs::create_dir_all(&tool_dir).unwrap();

    // 植入一条 L1（账本 enabled、磁盘无条目）
    let ssot = home.join(".tuvis/skills/v2m2-rc-b-l1");
    std::fs::create_dir_all(&ssot).unwrap();
    database::upsert_assignment("skill-v2m2-rc-b-l1", tool, true, "valid").unwrap();
    // 植入一条 L4（外链）
    let outside = std::env::temp_dir().join("v2m2-rc-b-l4-target");
    std::fs::create_dir_all(&outside).unwrap();
    let l4_link = tool_dir.join("v2m2-rc-b-l4");
    std::os::unix::fs::symlink(&outside, &l4_link).unwrap();

    let outcomes = reconcile_tool_batch(tool, "a");
    assert!(
        outcomes.len() >= 2,
        "批量应至少处置两条植入漂移，实得 {} 条: {:?}",
        outcomes.len(),
        outcomes
    );
    let l1_done = outcomes
        .iter()
        .any(|o| o.message.contains("skill-v2m2-rc-b-l1") && o.fixed && !o.needs_manual);
    assert!(l1_done, "批量内 L1-a 应 fixed: {:?}", outcomes);
    let l4_manual = outcomes
        .iter()
        .any(|o| o.message.contains("skill-v2m2-rc-b-l4") && o.needs_manual && !o.fixed);
    assert!(l4_manual, "批量内 L4 应恒 needs_manual: {:?}", outcomes);
    // 终审 Minor #4：批量经 reconcile_one 自然携带结构化字段——逐条非空且与漂移条目一致，
    // 前端行键映射（toolId|extensionId）不再依赖 message 文本格式
    assert!(
        outcomes
            .iter()
            .all(|o| !o.extension_id.is_empty() && !o.tool_id.is_empty()),
        "批量每条 outcome 都应携带非空结构化字段: {:?}",
        outcomes
    );
    assert!(
        outcomes
            .iter()
            .any(|o| o.extension_id == "skill-v2m2-rc-b-l1" && o.tool_id == tool && o.fixed),
        "批量 L1 条目结构化字段应正确: {:?}",
        outcomes
    );
    assert!(
        outcomes
            .iter()
            .any(|o| o.extension_id == "skill-v2m2-rc-b-l4" && o.tool_id == tool && o.needs_manual),
        "批量 L4 条目结构化字段应正确: {:?}",
        outcomes
    );
    assert!(
        tool_dir.join("v2m2-rc-b-l1").is_symlink(),
        "批量后 L1 链接应已重建"
    );
    assert!(l4_link.is_symlink(), "批量后 L4 外链应原样（不接管）");

    // 清理
    let _ = std::fs::remove_file(tool_dir.join("v2m2-rc-b-l1"));
    let _ = std::fs::remove_file(
        multi_agents_manager_lib::linker::layer2::tool_active_dir(tool).join("v2m2-rc-b-l1"),
    );
    let _ = std::fs::remove_dir_all(&ssot);
    let _ = std::fs::remove_file(&l4_link);
    let _ = std::fs::remove_dir_all(&outside);
    let _ = database::delete_assignments_for("skill-v2m2-rc-b-l1");
}

/// 派发拍平对账口径（用户裁决 2026-09-17）：账本 enabled 的嵌套技能先正向
/// 映射到拍平名（skill-套件-技能名）再与磁盘比较——磁盘拍平链接在场时不得
/// 误报 L1（嵌套名）/ L3（拍平名）；拍平链接被删后报 L1，extension_id 保持
/// 嵌套规范名
#[test]
fn drift_scan_uses_flat_names_no_false_l1l3() {
    let _guard = acquire_lock();
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::resource::reconcile::scan_drift;

    let tool = "codex";
    let home = dirs::home_dir().unwrap();
    database::set_tool_enabled(tool, true);
    let tool_dir = multi_agents_manager_lib::adapter::primary_skill_dir(tool).unwrap();
    std::fs::create_dir_all(&tool_dir).unwrap();

    // SSOT 嵌套技能 + 磁盘按拍平名挂好链接（工具目录与 Layer2，均指向 ~/.tuvis 之下）
    let ssot = home.join(".tuvis/skills/v2m2-flat9/inner");
    std::fs::create_dir_all(&ssot).unwrap();
    let flat_name = "v2m2-flat9-inner";
    let tool_link = tool_dir.join(flat_name);
    std::os::unix::fs::symlink(&ssot, &tool_link).unwrap();
    let layer2_link =
        multi_agents_manager_lib::linker::layer2::tool_active_dir(tool).join(flat_name);
    std::fs::create_dir_all(layer2_link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&ssot, &layer2_link).unwrap();
    // 账本：嵌套规范名 enabled
    database::upsert_assignment("skill-v2m2-flat9/inner", tool, true, "valid").unwrap();

    // 拍平链接在场：对账两侧按拍平名吻合——不报 L1 也不报 L3（负断言）
    let items = scan_drift();
    assert!(
        !items
            .iter()
            .any(|d| d.extension_id == "skill-v2m2-flat9/inner"),
        "不得误报 L1（账本嵌套名）: {:?}",
        items
    );
    assert!(
        !items
            .iter()
            .any(|d| d.extension_id == "skill-v2m2-flat9-inner"),
        "不得误报 L3（磁盘拍平名）: {:?}",
        items
    );

    // 拍平链接被删 → 报 L1；extension_id 为嵌套规范名，path 保留磁盘拍平名实况
    std::fs::remove_file(&tool_link).unwrap();
    let items = scan_drift();
    let hit = items
        .iter()
        .find(|d| d.extension_id == "skill-v2m2-flat9/inner" && d.kind == "L1")
        .expect("缺链应报 L1 且 extension_id 为嵌套规范名");
    assert!(
        hit.path.contains(flat_name),
        "path 应指向拍平名落点: {}",
        hit.path
    );

    // 清理
    let _ = std::fs::remove_file(&layer2_link);
    let _ = std::fs::remove_dir_all(ssot.parent().unwrap());
    let _ = database::delete_assignments_for("skill-v2m2-flat9/inner");
}

/// W5 名册语义：disabled 工具的同类漂移（此处 L1）不出现
#[test]
fn reconcile_scan_disabled_tool_excluded() {
    let _guard = acquire_lock();
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::resource::reconcile::scan_drift;

    let tool = "kimi";
    database::set_tool_enabled(tool, true);
    std::fs::create_dir_all(multi_agents_manager_lib::adapter::primary_skill_dir(tool).unwrap())
        .unwrap();
    database::upsert_assignment("skill-v2m2-rc-dis", tool, true, "valid").unwrap();

    // 先确认 enabled 时 L1 在报（构造有效性自检）
    let items = scan_drift();
    assert!(
        items
            .iter()
            .any(|d| d.extension_id == "skill-v2m2-rc-dis" && d.kind == "L1"),
        "enabled 时 L1 应在报: {:?}",
        items
    );

    // 停用工具 → 同一漂移不再出现
    database::set_tool_enabled(tool, false);
    let items = scan_drift();
    assert!(
        !items.iter().any(|d| d.extension_id == "skill-v2m2-rc-dis"),
        "disabled 工具的漂移不应出现: {:?}",
        items
    );

    // 清理
    let _ = database::delete_assignments_for("skill-v2m2-rc-dis");
}

/// 空目录健康检查（wave33 Item 2）：scan 报告 mam 根与启用工具 skill 目录下的
/// 空目录（嵌套链只报无任何条目的叶子；symlink / 点目录跳过）；clean 白名单
/// 根校验（越界 Err）+ 自底向上反复 remove_dir；PresetHealth 聚合 emptyDirs；
/// 含文件的目录不被删
#[test]
fn empty_dir_scan_clean_and_aggregate() {
    let _guard = acquire_lock();
    support::setup();
    use multi_agents_manager_lib::database;
    use multi_agents_manager_lib::services::preset::preset_health;
    use multi_agents_manager_lib::services::resource::reconcile::{
        clean_empty_dirs, scan_empty_dirs,
    };

    let home = dirs::home_dir().unwrap();
    let tool = "codex";
    database::set_tool_enabled(tool, true);

    // 嵌套空目录：tool 根一条（v2m2-ed-t/leaf）+ mam 根一条（v2m2-ed-m/leaf）
    let tool_leaf = multi_agents_manager_lib::adapter::primary_skill_dir(tool)
        .unwrap()
        .join("v2m2-ed-t/leaf");
    std::fs::create_dir_all(&tool_leaf).unwrap();
    let mam_leaf = home.join(".tuvis/skills/v2m2-ed-m/leaf");
    std::fs::create_dir_all(&mam_leaf).unwrap();
    // 含 SKILL.md 的兄弟目录：非空，不得命中、不得被删
    let sibling = home.join(".tuvis/skills/v2m2-ed-sib");
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::write(sibling.join("SKILL.md"), "x").unwrap();

    // scan：两条各归其主（contains 断言——共享 HOME 下其他残留不干扰）
    let items = scan_empty_dirs();
    let tool_hit = items
        .iter()
        .find(|i| i.path.contains("v2m2-ed-t"))
        .expect("工具根空目录应命中");
    assert_eq!(tool_hit.owner, "tool:codex");
    assert!(tool_hit.path.contains("v2m2-ed-t/leaf"));
    let mam_hit = items
        .iter()
        .find(|i| i.path.contains("v2m2-ed-m"))
        .expect("兔维斯 根空目录应命中");
    assert_eq!(mam_hit.owner, "mam");
    assert!(
        !items.iter().any(|i| i.path.contains("v2m2-ed-sib")),
        "含 SKILL.md 的目录不得命中: {:?}",
        items
    );

    // 聚合：preset_health 的 emptyDirs 与 scan 同源
    assert!(
        preset_health()
            .empty_dirs
            .iter()
            .any(|i| i.path.contains("v2m2-ed-t")),
        "PresetHealth 应聚合空目录源"
    );

    // clean：传入 scan 的两条 → 恰好删 2、叶子消失；父目录变空属下一轮迭代语义
    let n = clean_empty_dirs(vec![
        tool_leaf.to_string_lossy().to_string(),
        mam_leaf.to_string_lossy().to_string(),
    ])
    .unwrap();
    assert_eq!(n, 2, "应恰好删除两条空目录");
    assert!(!tool_leaf.exists() && !mam_leaf.exists(), "空目录应已消失");

    // 含文件的目录即使显式传入也不被删（返回 0）
    let n_sib = clean_empty_dirs(vec![sibling.to_string_lossy().to_string()]).unwrap();
    assert_eq!(n_sib, 0, "非空目录应被跳过");
    assert!(
        sibling.join("SKILL.md").exists(),
        "含 SKILL.md 的目录不得被删"
    );

    // 越界路径（/tmp 下）→ Err（白名单根校验先于存在性跳过）
    let err = clean_empty_dirs(vec!["/tmp/v2m2-ed-out".into()]).unwrap_err();
    assert!(err.contains("范围"), "越界应报范围错误: {}", err);

    // 清理
    let _ = std::fs::remove_dir_all(tool_leaf.parent().unwrap());
    let _ = std::fs::remove_dir_all(mam_leaf.parent().unwrap());
    let _ = std::fs::remove_dir_all(&sibling);
}
