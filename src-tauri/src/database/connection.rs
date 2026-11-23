use log::debug;
use once_cell::sync::Lazy;
use rusqlite::Connection;
use std::sync::Mutex;
use std::time::Duration;

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
///
/// M4（2026-11-16）：初始化走 [`open_and_init_with_backoff`] 有界退避重试——
/// 另一进程（重叠实例 / 引导期长写）对同一库持锁时，短暂锁在预算内自动恢复，
/// 持续占锁耗尽预算后**诚实退出**（结构化日志 + 固定退出码，非 panic、不假启动）。
/// 此前 `Connection::open`/`schema::init`/`seed_defaults` 任一失败都 `.expect` panic：
/// panic 会把 `Lazy` 的 init 闭包永久丢失（`init.take()` 已消费、cell 仍空），
/// 该进程此后每一次 DB 访问都 panic（N7 实录：写路径整期降级、读面照常的僵尸态）。
pub static DB: Lazy<Mutex<Connection>> = Lazy::new(|| {
    // 必须在品牌迁移及建库之前取证；旧库缺设置键也不等于首次安装。
    let fresh = super::first_run::is_fresh_home(&app_data_home());
    migrate_legacy_data_home();
    let db_dir = app_data_home().join(".tuvis");
    let _ = std::fs::create_dir_all(&db_dir);
    let db_path = db_dir.join("tuvis.db");
    match open_and_init_with_backoff(
        &db_path,
        fresh,
        INIT_RETRY_MAX_ATTEMPTS,
        Duration::from_millis(INIT_RETRY_DELAY_MS),
        // 与 rusqlite 0.41.1 开连接默认值（5s）同值：显式化只为内核可测试注入，
        // **不是**修复主体——对峙局 busy_timeout 机制性无效（见 schema::init 文档），
        // 修复主体是下面的整轮退避重试 + 耗尽诚实退出。
        Duration::from_secs(5),
    ) {
        Ok(conn) => Mutex::new(conn),
        Err(err) => {
            // 诚实失败：结构化日志 + 固定退出码（3），绝不 panic（避免 Lazy 毒化
            // 僵尸）、绝不带病假启动。可读性：日志先落 mam.log 再退出。
            // F3：重试耗尽与不可重试步骤失败（DB_INIT_FATAL 前缀）分列两种日志前缀，
            // 诊断面不把「没重试过」误述为「重试后仍失败」；退出码同为 3。
            if err.starts_with("DB_INIT_FATAL") {
                log::error!(
                    "DB_INIT_FATAL_EXIT: 数据库初始化在不可重试步骤失败，进程拒绝假启动: {err}"
                );
            } else {
                log::error!(
                    "DB_INIT_RETRY_EXHAUSTED: 数据库初始化在有界重试后仍失败，进程拒绝假启动: {err}"
                );
            }
            std::process::exit(3);
        }
    }
});

/// 初始化重试轮数（短暂锁/重叠实例退净的现实窗远小于此；持续占锁 ~2 分钟内诚实退出）。
const INIT_RETRY_MAX_ATTEMPTS: u32 = 25;
/// 重试轮间退避。
const INIT_RETRY_DELAY_MS: u64 = 100;

/// F3：单轮初始化的步骤级错误——步骤名 + 原因 + 是否临时锁争用。
/// 临时锁争用（SQLITE_BUSY/LOCKED）→ 可整轮重试；其余（schema 冲突/约束失败/
/// 迁移错误）→ 快速失败，不烧重试预算。
pub(crate) struct InitStepError {
    step: &'static str,
    reason: String,
    transient_lock: bool,
}

impl std::fmt::Display for InitStepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.step, self.reason)
    }
}

impl InitStepError {
    /// rusqlite 错误分类：DatabaseBusy/DatabaseLocked = 临时锁争用（重试），
    /// 其余（约束/schema 冲突等）= 持续错误（快速失败）。
    fn from_rusqlite(step: &'static str, e: rusqlite::Error) -> Self {
        let transient_lock = matches!(
            e.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy) | Some(rusqlite::ErrorCode::DatabaseLocked)
        );
        Self {
            step,
            reason: e.to_string(),
            transient_lock,
        }
    }

    /// migration::migrate 的 String 错误包装了底层 rusqlite 错误（类型信息已丢），
    /// 锁争用形态按子串识别（与 M4 测试的断言锚同一字符串族）。
    fn from_migrate(step: &'static str, msg: String) -> Self {
        let transient_lock = msg.contains("database is locked")
            || msg.contains("database table is locked")
            || msg.contains("DatabaseBusy");
        Self {
            step,
            reason: msg,
            transient_lock,
        }
    }
}

/// 单轮全量初始化（F3）：open → schema → seed → **migrate → 工具行种子**。
/// 任一步失败返回结构化 [`InitStepError`]（步骤名+原因+是否临时锁争用）。
/// 连接中途失败即 drop（锁随连接释放），全部语句幂等，整轮重试安全。
/// 与 M4 形态的差异：migrate 与工具行种子从 init 外层（失败只记日志/吞错）纳入
/// 单轮预算——「启动成功 = 全部迁移与种子实际完成」。
pub(crate) fn open_and_init_once(
    db_path: &std::path::Path,
    fresh: bool,
    busy_timeout: Duration,
) -> Result<Connection, InitStepError> {
    let conn =
        Connection::open(db_path).map_err(|e| InitStepError::from_rusqlite("DB_OPEN_FAIL", e))?;
    conn.busy_timeout(busy_timeout)
        .map_err(|e| InitStepError::from_rusqlite("DB_BUSY_TIMEOUT_FAIL", e))?;
    // H5（2026-11-18，Ruling H2 证据驱动）：journal_mode=WAL。测量依据（真实库副本
    // 微观基准，task-HE-report §测量）：delete 模式下写窗内读者 p95 49ms / max 103ms
    // （读者被 SHARED→EXCLUSIVE 升级对峙拉住），写提交 p50 0.28ms；WAL 下读者零阻塞
    //（p95 2.2ms）、写提交 p50 0.01ms。GFix 活体证据（外部锁 40s 对峙 stop 39.6s、
    // 风暴中计时器同域拉伸）与本基准同指向 DB 层同域拉伸 ⇒ 按 Ruling H2 预期分支
    // 实施连接层 WAL。语义与申报：
    // * journal_mode 是**文件级持久属性**：主连接 init 处设置一次即可，其余连接
    //   （`database::open()` / sqlite3 CLI / usage 采集）自动继承；幂等可重入。
    // * busy_timeout 语义不变：WAL 消除的是「读者 ↔ 写者」互斥域；写-写互斥
    //   （含外部进程独占锁对峙）仍走 busy handler——微观基准 phase3 双模式同形
    //   （2.74s vs 2.91s）。
    // * checkpoint 走 SQLite 默认自动检查点（1000 页）；进程正常关闭由 SQLite
    //   收敛 -wal/-shm。外部只读读者（`sqlite3 -readonly`）实测可读（含 -shm/-wal
    //   在场与干净关闭后两形态）。
    // * 失败语义：模式切换需短暂独占，撞锁按 DatabaseBusy/DatabaseLocked 分类为
    //   瞬态 → 整轮重试（M4 既有机制）自动覆盖。
    // * :memory:（DeviceStore::memory / lib 单测）PRAGMA 返回 "memory"，零变化。
    let journal_mode: String = conn
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .map_err(|e| InitStepError::from_rusqlite("DB_WAL_MODE_FAIL", e))?;
    debug!("DB journal_mode={journal_mode}");
    crate::database::schema::init(&conn)
        .map_err(|e| InitStepError::from_rusqlite("DB_SCHEMA_INIT_FAIL", e))?;
    super::first_run::seed_defaults(&conn, fresh)
        .map_err(|e| InitStepError::from_rusqlite("DB_SEED_DEFAULTS_FAIL", e))?;
    super::migration::migrate(&conn)
        .map_err(|e| InitStepError::from_migrate("DB_MIGRATE_FAIL", e))?;
    super::dao::agent_tool::ensure_tool_rows_conn(&conn)
        .map_err(|e| InitStepError::from_rusqlite("DB_TOOL_ROWS_SEED_FAIL", e))?;
    Ok(conn)
}

/// 有界退避重试内核：最多 `attempts` 轮，轮间 sleep `retry_delay`。
/// 语义（M4 + F3）：短暂锁/对峙解除 → 某轮成功（自动恢复）；持续占锁 →
/// 结构化错误（`DB_INIT_RETRY_EXHAUSTED` 前缀 + 尝试次数 + 底层原因），由调用方
/// 诚实失败；**非锁争用的持续错误（迁移失败/约束冲突）→ 立即结构化失败**
///（`DB_INIT_FATAL` 前缀 + 步骤名 + 原因），不烧重试预算。
fn open_and_init_with_backoff(
    db_path: &std::path::Path,
    fresh: bool,
    attempts: u32,
    retry_delay: Duration,
    busy_timeout: Duration,
) -> Result<Connection, String> {
    let mut last_err = String::new();
    for attempt in 1..=attempts.max(1) {
        match open_and_init_once(db_path, fresh, busy_timeout) {
            Ok(conn) => {
                if attempt > 1 {
                    log::info!("DB_INIT_RETRY_OK: 数据库初始化在第 {attempt}/{attempts} 轮成功（此前撞锁，短锁自动恢复）");
                }
                return Ok(conn);
            }
            Err(e) => {
                last_err = e.to_string();
                if !e.transient_lock {
                    log::error!(
                        "DB_INIT_FATAL: 数据库初始化在步骤 {} 失败（非锁争用，不重试）: {e}",
                        e.step
                    );
                    return Err(format!(
                        "DB_INIT_FATAL: 初始化在步骤 {} 失败（非锁争用，不重试）: {e}",
                        e.step
                    ));
                }
                log::warn!(
                    "DB_INIT_RETRY: 数据库初始化第 {attempt}/{attempts} 轮失败（{last_err}），{}ms 后重试",
                    retry_delay.as_millis()
                );
                if attempt < attempts {
                    std::thread::sleep(retry_delay);
                }
            }
        }
    }
    Err(format!(
        "DB_INIT_RETRY_EXHAUSTED: 初始化重试 {attempts} 轮全部失败（attempted={attempts}）: {last_err}"
    ))
}

/// 候选脚本语境（`TUVIS_CANDIDATE_ROOT` 在场，见 candidate_guard）在**完整初始化**
/// （schema + seed + migrate + 工具行种子全部完成，M4 + F3）之后写 `init-complete`
/// 标记（F3：内容 `pid=<n>\nstarted=<epoch 秒>\n`，PID 绑定 + 启动时刻防 PID 复用），
/// 供 run-candidate.sh 的引导/正式启动就绪判定使用（脚本只认「标记内 PID ==
/// 本次 spawn 的候选 PID 且 started 吻合」的标记，陈旧标记忽略）。
/// 正式版无该环境变量，零行为变化。标记写失败只记日志（warn），不影响初始化；
/// 脚本侧将按未就绪继续等待/判失败（fail-closed），mam.log 有对应 warn 可诊断。
pub fn mark_init_complete_if_candidate() {
    let Some(root) = std::env::var_os("TUVIS_CANDIDATE_ROOT") else {
        return;
    };
    if root.is_empty() {
        return;
    }
    if let Err(e) = write_init_complete_marker_at(std::path::Path::new(&root)) {
        log::warn!("init-complete 标记写入失败（脚本就绪判定将回落轮询超时）: {e}");
    }
}

/// 本进程启动时刻（epoch 秒）——init-complete 标记的防 PID 复用绑定字段。
/// sysinfo 与 `ps -o lstart`（脚本侧核验用）同源自进程 birthtime，秒级同值；
/// 与 `monitor::opencode_observation::process_fact` 同款实现（启动秒，0/负拒绝）。
/// 读不到 = 极罕见异常 → None（调用方拒写无 started 绑定的标记，fail-closed）。
fn self_start_epoch() -> Option<u64> {
    let pid = sysinfo::Pid::from_u32(std::process::id());
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    let process = system.process(pid)?;
    let started_secs = process.start_time();
    (started_secs > 0).then_some(started_secs)
}

/// [`mark_init_complete_if_candidate`] 的路径注入内核（可测试）：原子写
///（同目录临时文件 + rename）`$root/.tuvis/init-complete`。
/// **F3 格式（与 run-candidate.sh `marker_field` 解析逐字对齐，双方原文同录）**：
/// ```text
/// pid=<进程 PID>
/// started=<进程启动时刻 epoch 秒>
/// ```
/// 只在全部迁移+种子成功后由调用方写出（见 mod.rs::init）；本进程启动时刻读不到
/// → 拒写无 started 绑定的标记（fail-closed，脚本侧按未就绪继续等/判失败）。
pub(crate) fn write_init_complete_marker_at(root: &std::path::Path) -> std::io::Result<()> {
    let dir = root.join(".tuvis");
    std::fs::create_dir_all(&dir)?;
    let started = self_start_epoch().ok_or_else(|| {
        std::io::Error::other("本进程启动时刻不可读（sysinfo），拒绝写无 started 绑定的标记")
    })?;
    let tmp = dir.join(".init-complete.tmp");
    let content = format!("pid={}\nstarted={}\n", std::process::id(), started);
    std::fs::write(&tmp, content)?;
    match std::fs::rename(&tmp, dir.join("init-complete")) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

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

    // ===== M4（OpenCode 操控收口）：DB 初始化撞锁的 RED→GREEN 回归 =====
    // 机制（2026-11-16 诊断定案）：另一进程/连接对同一库持长写锁时，初始化的
    // 首个写（全新库 = schema DDL，已完整库 = seed INSERT）与对方进入
    // 「我持 SHARED 想升 RESERVED / 对方持 RESERVED 想升 EXCLUSIVE」对峙——
    // busy handler 无法破局（SQLite 官方语义：可能判定死锁直接返回 BUSY，
    // 或空转到 busy_timeout 耗尽），5s 默认超时救不了；旧代码 `.expect` 直接
    // panic 且 once_cell Lazy 的 init 已被 take → 该进程 DB 永久毒化（僵尸）。
    // 修复 = 整轮 init 有界退避重试 + 耗尽诚实退出。以下测试用**同进程第二
    // 连接**持 RESERVED（SQLite 文件锁与进程无关，等价跨进程对峙）。

    /// 第二连接持 RESERVED 写锁的守卫：drop（或显式 ROLLBACK）即释放。
    /// 未提交事务中途 drop 会自动回滚，探针表不残留、不影响被测 init。
    struct HeldWriteLock(Connection);

    impl HeldWriteLock {
        fn acquire(path: &std::path::Path) -> Self {
            let conn = Connection::open(path).unwrap();
            conn.execute_batch(
                "BEGIN;
                 CREATE TABLE IF NOT EXISTS m4_lock_probe (k TEXT);
                 INSERT INTO m4_lock_probe VALUES ('held');",
            )
            .unwrap();
            HeldWriteLock(conn)
        }
    }

    impl Drop for HeldWriteLock {
        fn drop(&mut self) {
            let _ = self.0.execute_batch("ROLLBACK");
        }
    }

    /// 短暂/中段长锁：锁在重试预算内释放 → 某一轮初始化成功，且此后写入正常
    ///（回归 1「短暂锁释放后正常启动」的内核形态；N7 场景的自愈路径）。
    #[test]
    fn init_recovers_when_a_long_write_lock_is_released_within_the_retry_budget() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db");
        // 先完整初始化一次（首轮无锁竞争的基线库），后续轮在「已完整库」上重试
        open_and_init_with_backoff(&db, true, 1, Duration::ZERO, Duration::from_millis(200))
            .expect("基线初始化应成功");

        // 后台线程持锁 6s 后释放（> busy_timeout 200ms：期间多轮失败，释放后某轮成功）
        let lock_path = db.clone();
        let holder = std::thread::spawn(move || {
            let _guard = HeldWriteLock::acquire(&lock_path);
            std::thread::sleep(Duration::from_millis(6000));
        });
        std::thread::sleep(Duration::from_millis(300)); // 让锁先落位

        let t0 = std::time::Instant::now();
        let conn = open_and_init_with_backoff(
            &db,
            false,
            40,
            Duration::from_millis(100),
            Duration::from_millis(200),
        )
        .expect("锁在重试预算内释放：某轮初始化必须成功");
        let elapsed = t0.elapsed();
        holder.join().unwrap();

        assert!(
            elapsed >= Duration::from_millis(4500),
            "持锁 6s 期间不应提前成功（实测 {elapsed:?}）"
        );
        // 恢复后写入正常（写路径真实可用，而非假启动）
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('m4.probe', 'ok')",
            [],
        )
        .expect("恢复后的连接必须可写");
        let v: String = conn
            .query_row("SELECT value FROM settings WHERE key='m4.probe'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(v, "ok");
    }

    /// 持续占锁：重试预算耗尽 → **结构化错误**（非 panic、含失败原因与尝试次数）
    ///（回归 2「持续占锁时明确失败」的内核形态）。
    #[test]
    fn init_reports_structured_error_when_the_lock_is_held_beyond_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db");
        open_and_init_with_backoff(&db, true, 1, Duration::ZERO, Duration::from_millis(200))
            .expect("基线初始化应成功");

        // 本线程持锁不释放（测试结束 drop 即回滚释放）
        let _held = HeldWriteLock::acquire(&db);

        let err = open_and_init_with_backoff(
            &db,
            false,
            3,
            Duration::from_millis(50),
            Duration::from_millis(200),
        )
        .expect_err("持续占锁必须重试耗尽后返回结构化错误");
        assert!(
            err.contains("DB_INIT_RETRY_EXHAUSTED"),
            "错误必须带结构化前缀（非 panic 栈）: {err}"
        );
        assert!(err.contains("attempted=3"), "错误必须带尝试次数: {err}");
        assert!(
            err.contains("database is locked") || err.contains("DatabaseBusy"),
            "错误必须带底层锁失败原因: {err}"
        );
    }

    // （M4 的 init_complete_marker_is_written_atomically_with_pid_and_ts 已被下方
    // F3 版 init_complete_marker_is_written_atomically_with_pid_and_started 取代：
    // 标记格式按 F3 任务书升级为 pid=+started= 双绑定；原子写/无 .tmp 残留/
    // 本进程 PID 三项保证全保留并新增 started 防 PID 复用断言。）

    // ===== F3（2026-11-17 五项收口联验）：启动成功以实际完成迁移为准 =====
    // 缺口（用户复审）：①关键迁移（migration::migrate）与启动种子（工具行）在 M4 的
    // 有界初始化**之外**——迁移失败只记日志、标记照写（假完成信号）；②标记无进程
    // 启动时刻防 PID 复用字段。以下用例钉住：全量初始化（open→schema→seed→migrate→
    // 工具行）整体进有界重试；迁移持续失败 = 结构化快速失败（步骤名，非锁争用不烧
    // 重试预算）；标记 = pid= + started= 双绑定（与 run-candidate.sh marker_field
    // 解析逐字对齐）。

    /// F3-①：有界初始化完成后，启动种子（agent_tools 工具行）必须已在库。
    /// 旧形态工具行种子在 init 外层（mod.rs）且错误被吞——种子未落库也「成功」。
    #[test]
    fn full_bounded_init_seeds_tool_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db");
        let conn =
            open_and_init_with_backoff(&db, true, 1, Duration::ZERO, Duration::from_millis(200))
                .expect("全量初始化应成功");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM agent_tools", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            n as usize,
            crate::adapter::TOOL_IDS.len(),
            "有界初始化完成后工具行种子必须已在库（F3：种子写入纳入预算）"
        );
    }

    /// F3-①：迁移失败（注入 BEFORE UPDATE 触发器 ABORT source_tool 回填 UPDATE）
    /// 必须让有界初始化**结构化快速失败**（步骤名 DB_MIGRATE_FAIL + 迁移子步骤 +
    /// 注入原因），而不是旧形态的「初始化 Ok、迁移错误只记日志、标记照写」；
    /// 且非锁争用错误**不进重试**（快速失败，不烧 25 轮预算）。
    #[test]
    fn migration_failure_fails_fast_with_step_name_without_retry_exhaustion() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db");
        open_and_init_with_backoff(&db, true, 1, Duration::ZERO, Duration::from_millis(200))
            .expect("基线初始化应成功");
        {
            let conn = Connection::open(&db).unwrap();
            // 命中 migrate 的 source_tool 回填 UPDATE：
            // kind='skill' + source_tool IS NULL + source_path LIKE '%/.claude/skills/%'
            conn.execute(
                "INSERT INTO extensions (id, kind, name, source_path, installed_at, updated_at)
                 VALUES ('f3-fail', 'skill', 'x', '/Users/f3/.claude/skills/x', 't', 't')",
                [],
            )
            .unwrap();
            conn.execute_batch(
                "CREATE TRIGGER f3_block_backfill BEFORE UPDATE ON extensions
                 BEGIN SELECT RAISE(ABORT, 'f3-injected-migrate-failure'); END;",
            )
            .unwrap();
        }
        let t0 = std::time::Instant::now();
        let err = open_and_init_with_backoff(
            &db,
            false,
            25,
            Duration::from_millis(100),
            Duration::from_millis(200),
        )
        .expect_err("迁移失败必须结构化失败（而非假成功）");
        let elapsed = t0.elapsed();
        assert!(
            err.contains("DB_MIGRATE_FAIL"),
            "错误必须带初始化步骤名 DB_MIGRATE_FAIL: {err}"
        );
        assert!(
            err.contains("MIGRATE_SOURCE_TOOL_FAIL") && err.contains("f3-injected-migrate-failure"),
            "错误必须携带迁移子步骤与注入原因: {err}"
        );
        assert!(
            !err.contains("DB_INIT_RETRY_EXHAUSTED"),
            "非锁争用的迁移失败必须快速失败（不进重试循环）: {err}"
        );
        assert!(
            elapsed < Duration::from_millis(2000),
            "快速失败不得烧完重试预算（25×100ms+busy 应远大于此，实测 {elapsed:?}）"
        );
    }

    /// F3-②：init-complete 标记 = PID 绑定 + 进程启动时刻防重用双字段
    /// （`pid=<n>\nstarted=<epoch 秒>\n`，与 run-candidate.sh marker_field 解析
    /// 逐字对齐）；原子写不变（无 .tmp 残留）。started 必须可读且不晚于当前时刻。
    #[test]
    fn init_complete_marker_is_written_atomically_with_pid_and_started() {
        let dir = tempfile::tempdir().unwrap();
        write_init_complete_marker_at(dir.path()).expect("标记应写入成功");
        let marker = dir.path().join(".tuvis").join("init-complete");
        let content = std::fs::read_to_string(&marker).unwrap();
        let mut pid: Option<String> = None;
        let mut started: Option<String> = None;
        for line in content.lines() {
            if let Some(v) = line.strip_prefix("pid=") {
                pid = Some(v.to_string());
            }
            if let Some(v) = line.strip_prefix("started=") {
                started = Some(v.to_string());
            }
        }
        assert_eq!(
            pid.as_deref(),
            Some(std::process::id().to_string().as_str()),
            "pid= 行必须等于本进程 PID（防陈旧标记误判就绪的绑定字段）: {content:?}"
        );
        let started = started.unwrap_or_else(|| {
            panic!("标记必须含 started=<epoch 秒> 行（防 PID 复用的绑定字段）: {content:?}")
        });
        let started: u64 = started
            .parse()
            .unwrap_or_else(|e| panic!("started 必须是 epoch 秒整数: {started:?}（{e}）"));
        let now: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        assert!(
            started > 0 && started <= now,
            "started 必须是合法 epoch 秒且不晚于现在: {started} vs {now}"
        );
        assert!(
            !dir.path()
                .join(".tuvis")
                .join(".init-complete.tmp")
                .exists(),
            "原子写不得残留临时文件"
        );
    }

    /// H5（Ruling H2）：文件库 init 后 journal_mode 必须 = WAL，且作为**文件级持久
    /// 属性**被第二连接（`database::open()` 同形态）自动继承。
    #[test]
    fn h5_file_backed_init_persists_wal_journal_mode() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db");
        let conn =
            open_and_init_with_backoff(&db, true, 1, Duration::ZERO, Duration::from_millis(200))
                .expect("全量初始化应成功");
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal", "文件库 init 必须落 WAL（Ruling H2 证据驱动）");
        drop(conn);
        let conn2 = Connection::open(&db).unwrap();
        let mode2: String = conn2
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode2, "wal", "journal_mode 是文件级属性：第二连接自动继承");
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
