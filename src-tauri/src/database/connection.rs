use log::debug;
use once_cell::sync::Lazy;
use rusqlite::Connection;
use std::sync::Mutex;

/// 应用数据主目录：TUVIS_HOME 环境变量仅在 debug/test 构建（debug_assertions）生效，
/// 用于集成测试重定向数据目录（Windows 下 dirs::home_dir 无法用 HOME 重定向）；
/// release 生产构建一律使用真实用户目录，防止环境变量误设导致 DB 与 skills/plugins 数据割裂
pub(crate) fn app_data_home() -> std::path::PathBuf {
    // release 构建里 `TUVIS_HOME` 根本不参与解析 ⇒ 连环境变量都不读（与改动前逐字同义）。
    // `cfg!` 是编译期常量，本分支在 release 会被整个折掉。
    let mam_home = if cfg!(debug_assertions) {
        std::env::var_os("TUVIS_HOME")
    } else {
        None
    };
    app_data_home_with(mam_home)
}

/// `app_data_home()` 的**纯函数内核**（Task 16 Step 2）：把「env 取值」与「路径解析」
/// 分开，单测才能对「`TUVIS_HOME` **未设置**」这一支**直接**断言——不必改进程环境
/// （`std::env::set_var` 是进程级共享状态，在并行测试里会与别的用例互踩，改它比改代码更危险）。
/// 生产入口恒为 `app_data_home()`；本函数不额外暴露任何行为。
fn app_data_home_with(mam_home: Option<std::ffi::OsString>) -> std::path::PathBuf {
    if cfg!(debug_assertions) {
        if let Some(home) = mam_home {
            if !home.is_empty() {
                return std::path::PathBuf::from(home);
            }
        }
        // **Task 16 Step 2（默认 hermetic）**：lib 单测是**独立 crate**、不经
        // `tests/support.rs::setup()` 的环境重定向 ⇒ 没有本分支时，首次取 `DB` 就把进程级
        // 连接绑到用户**真实** `~/.tuvis/tuvis.db`（2026-10-03 00:11 事故的**账本**半边；
        // 当时 `TUVIS_HOME=$(mktemp -d)` 只保住了账本，**读源**那一半没保住 → 见 `collect.rs`
        // 的采集拒绝闸）。`TUVIS_HOME` 未设置时落到**进程私有** temp 目录。
        //
        // 形态纪律（照抄既有成对范式，不自创第二套）：仍在 `cfg!(debug_assertions)`
        // 门控**之内**（否则 release 会被重定向）；**不得**实现成「未设置就 panic」——
        // 那会打断全仓每一个（哪怕只是间接）依赖真实 home 的既有 lib 单测，爆炸半径不可控。
        // 集成测试（`src-tauri/tests/*.rs`）链接的是**非 test** 构建 ⇒ 本分支在那边不存在，
        // 它们照旧走 `setup()` 的 `TUVIS_HOME` 重定向。
        #[cfg(test)]
        return std::env::temp_dir().join(format!("tuvis-test-{}", std::process::id()));
    }
    dirs::home_dir().unwrap_or_default()
}

/// 全局数据库连接（从 store.rs 搬移，保持原有模式）
pub static DB: Lazy<Mutex<Connection>> = Lazy::new(|| {
    // 必须在品牌迁移及建库之前取证；旧库缺设置键也不等于首次安装。
    let fresh = super::first_run::is_fresh_home(&app_data_home());
    migrate_legacy_data_home();
    let db_dir = app_data_home().join(".tuvis");
    let _ = std::fs::create_dir_all(&db_dir);
    let db_path = db_dir.join("tuvis.db");
    let conn = Connection::open(&db_path).expect("Failed to open mam database");
    crate::database::schema::init(&conn);
    super::first_run::seed_defaults(&conn, fresh)
        .expect("Failed to initialize first-install defaults");
    Mutex::new(conn)
});

/// 品牌更名的一次性数据迁移（2026-10，issue #76）：`~/.mam` → `~/.tuvis`
/// （含 `mam.db` → `tuvis.db`）。触发条件：旧目录存在**且**新目录不存在——
/// 新目录已存在 = 已迁移或已在用，绝不动它。失败（旧版进程占用/权限）只记
/// 日志不 panic：旧数据原地保留，用户关掉旧版后下次启动自动重试。
/// `TUVIS_HOME` 重定向与 `cfg(test)` 下不迁移（hermetic 纪律：测试不得搬真实 home）。
pub fn migrate_legacy_data_home() {
    if cfg!(test) || std::env::var_os("TUVIS_HOME").is_some() {
        return;
    }
    let Some(home) = dirs::home_dir() else {
        return;
    };
    migrate_legacy_data_home_at(&home.join(".mam"), &home.join(".tuvis"));
}

/// 迁移内核（路径注入便于测试）：搬目录 + 改账本文件名，均可重入。
fn migrate_legacy_data_home_at(legacy: &std::path::Path, new: &std::path::Path) {
    if !legacy.is_dir() || new.exists() {
        return;
    }
    if let Err(e) = std::fs::rename(legacy, new) {
        log::warn!(
            "数据目录迁移 ~/.mam → ~/.tuvis 失败（{e}）；本版将以空目录启动，旧数据原地保留在 ~/.mam"
        );
        return;
    }
    log::info!("数据目录已迁移：~/.mam → ~/.tuvis");
    let old_db = new.join("mam.db");
    let new_db = new.join("tuvis.db");
    if old_db.exists() && !new_db.exists() {
        if let Err(e) = std::fs::rename(&old_db, &new_db) {
            log::warn!("mam.db → tuvis.db 改名失败（{e}）");
        }
    }
}

/// 初始化数据库（在应用启动时调用）
pub fn init() {
    Lazy::force(&DB);
    debug!("Database initialized at ~/.tuvis/tuvis.db");
}

/// 打开新连接（少数场景使用）
pub fn open() -> Result<Connection, String> {
    let db_path = app_data_home().join(".tuvis").join("tuvis.db");
    Connection::open(&db_path).map_err(|e| format!("DB_OPEN_FAIL: 打开数据库失败: {}", e))
}

/// **Task 16 Step 2 的锁（锁表第 1 条「路径隔离」）**：`cfg(test)` 下解析出的 app 数据
/// 目录**必须落在 `std::env::temp_dir()` 之下**（进程私有 temp；Windows 上 temp 本就在
/// 真实 home 内，故判据锚定 temp 而非「不在 home 下」）。
///
/// **为什么必须落在 lib 单测里**（而不是 `src-tauri/tests/`）：本用例断言的是
/// `#[cfg(test)]` 分支的行为，而集成测试链接的是**非 test** 构建 —— 那边根本没有这个分支，
/// 断言什么都是假绿（`#[cfg(test)]` 只在 lib 单测里为真）。
#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    /// 未设置 / 空串 `TUVIS_HOME` → **进程私有 temp 目录**，而不是真实 home。
    /// 这是 2026-10-03 事故的账本半边的封堵点：lib 单测不再可能把进程级 `DB` 绑到
    /// 用户真实 `~/.tuvis/tuvis.db`。
    ///
    /// 隔离判据（平台无关）：解析结果必须落在 `std::env::temp_dir()` **之下**。
    /// 不用「不在真实 home 之下」——Windows 上 temp 本就在用户 profile 内
    /// （`C:\Users\<u>\AppData\Local\Temp` ⊂ `C:\Users\<u>`），该判据恒假；
    /// 实现真正承诺的是「进程私有 temp 目录」，故以 temp 为锚。
    #[test]
    fn tests_never_resolve_the_real_users_home_when_mam_home_is_unset() {
        let temp = std::env::temp_dir();
        let expected = temp.join(format!("tuvis-test-{}", std::process::id()));

        for (case, input) in [
            ("未设置", None),
            (
                "空串（与既有 `!home.is_empty()` 语义同义）",
                Some(OsString::from("")),
            ),
        ] {
            let resolved = app_data_home_with(input);
            assert_eq!(
                resolved,
                expected,
                "`TUVIS_HOME` {case} 时必须落到进程私有 temp 目录 <{}>",
                expected.display()
            );
            assert!(
                resolved.starts_with(&temp),
                "**路径隔离锁**：`TUVIS_HOME` {case} 时解析出的 app 数据目录 <{}> 必须落在\
                 进程私有 temp 目录 <{}> 之下（Windows 上 temp 本就在真实 home 内，\
                 隔离承诺锚定 temp 而非「不在 home 下」）——落到别处 = 进程级 DB 可能又绑回\
                 用户真实账本（2026-10-03 事故）",
                resolved.display(),
                temp.display()
            );
        }
    }

    /// 既有的 `TUVIS_HOME` 重定向语义**不得被踩掉**：非空值仍然优先（集成测试的
    /// `setup()` 与用例私有库都靠它）。
    #[test]
    fn explicit_mam_home_still_wins() {
        assert_eq!(
            app_data_home_with(Some(OsString::from("/tmp/mam-explicit"))),
            std::path::PathBuf::from("/tmp/mam-explicit"),
            "非空 `TUVIS_HOME` 必须仍然优先于新的 cfg(test) 兜底分支"
        );
    }

    /// 生产入口 `app_data_home()` 在本进程（= lib 单测构建）里也必须满足隔离性质。
    /// 与上面那条的分工：那条钉**分支语义**，这条钉**真实入口**（防有人把兜底分支删了
    /// 却保留纯函数内核 ⇒ 上一条仍绿而实际入口已回到真实 home）。
    ///
    /// 判据同上：必须落在 `std::env::temp_dir()` 之下（Windows 上 temp 在真实 home
    /// 之内，「不在 home 下」不是可实现的不变量；隔离承诺 = 进程私有 temp）。
    #[test]
    fn real_entry_point_is_isolated_in_this_process() {
        let resolved = app_data_home();
        let temp = std::env::temp_dir();
        assert!(
            resolved.starts_with(&temp),
            "生产入口 `app_data_home()` 在 lib 单测进程里解析出 <{}>，未落在进程私有 temp \
             目录 <{}> 之下：隔离分支被删了或没接上（本用例是「分支存在」与「入口真的\
             走它」之间的那道桥）",
            resolved.display(),
            temp.display()
        );
    }

    /// 改名迁移内核：搬目录 + 改账本名一次性完成，且对新目录已存在的重入免疫。
    #[test]
    fn migrates_legacy_home_once_and_renames_db() {
        let base = tempfile::tempdir().unwrap();
        let legacy = base.path().join("home").join(".mam");
        let new = base.path().join("home").join(".tuvis");
        std::fs::create_dir_all(legacy.join("skills")).unwrap();
        std::fs::write(legacy.join("mam.db"), "db").unwrap();

        migrate_legacy_data_home_at(&legacy, &new);

        assert!(!legacy.exists(), "旧目录应已被搬走");
        assert!(new.join("tuvis.db").exists(), "账本应改名为 tuvis.db");
        assert!(new.join("skills").exists(), "子目录应整体随迁");

        // 幂等：新目录已存在时再执行不得触碰
        std::fs::write(new.join("tuvis.db"), "db2").unwrap();
        migrate_legacy_data_home_at(&legacy, &new);
        assert_eq!(
            std::fs::read_to_string(new.join("tuvis.db")).unwrap(),
            "db2"
        );
    }
}
