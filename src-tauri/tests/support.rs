use std::sync::Once;

static INIT: Once = Once::new();

/// 初始化测试环境：设置 HOME 到临时目录，初始化全局数据库
/// TempDir 通过 Box::leak 保持存活，避免被清理
pub fn setup() {
    INIT.call_once(|| {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        std::env::set_var("HOME", &home);
        // Windows 下 dirs::home_dir 忽略 HOME 环境变量，用专用覆盖变量 TUVIS_HOME 重定向应用数据目录
        std::env::set_var("TUVIS_HOME", &home);
        // 创建 ~/.tuvis 目录结构
        std::fs::create_dir_all(home.join(".tuvis/skills")).unwrap();
        std::fs::create_dir_all(home.join(".tuvis/mcp")).unwrap();
        std::fs::create_dir_all(home.join(".tuvis/plugins")).unwrap();
        std::fs::create_dir_all(home.join(".tuvis/active")).unwrap();
        // 初始化数据库（Lazy 只初始化一次）
        multi_agents_manager_lib::database::init();
        // 泄漏 TempDir 防止它被清理（测试期间需要保持数据库文件存在）
        std::mem::forget(temp);
    });
}

// ---- 用例私有账本库（Task 4 / GC 19：账本断言不再共用 setup() 的那一个库）----
use rusqlite::Connection;
use std::path::PathBuf;

/// **每条集成用例独占一个账本库**：返回一个建好表的私有连接（真实 DB 文件，路径带 tag）。
///
/// 与 `setup()` 的分工（两者可以同时用，互不干扰）：
/// * `setup()` 管**进程级**环境与全局 `DB`（`Once`，一个二进制只生效一次）——凡是会碰
///   **全局设置库**（`settings::load` / `settings::save`，采集器内部都会读它）或
///   `dirs::home_dir()` 的用例才调它；
/// * 本函数给的是**用例私有**的库文件：不设 env、不碰全局 `DB`，因此并行安全，且
///   `count_rows_conn` 这类绝对行数断言在这里是稳定的。
///
/// （为什么不用 `setup()` 重定向出独立库：`DB` 是 `Lazy<Mutex<Connection>>`，
/// 首次取值时就把路径定死了，同一个二进制内没法"每条用例换一个库"。）
#[allow(dead_code)]
pub fn open_ledger_db(tag: &str) -> Connection {
    let home = unique_home(tag);
    let conn =
        Connection::open(home.join(".tuvis").join("tuvis.db")).expect("打开用例私有账本库失败");
    // 与生产启动路径同款：schema::init 建齐（含 Task 2 追加的 4 张用量账本表）
    multi_agents_manager_lib::database::schema::init(&conn);
    conn
}

/// 用例私有且**泄漏保活**的数据目录（`<temp>/<tag>` 带上用例名：失败信息里能直接看出是谁）
#[allow(dead_code)]
fn unique_home(tag: &str) -> PathBuf {
    let temp = tempfile::tempdir().expect("创建用例私有 tempdir 失败");
    let home = temp.path().join(tag);
    std::fs::create_dir_all(home.join(".tuvis")).expect("创建 .tuvis 目录失败");
    std::mem::forget(temp); // 与 setup() 同款：用例存活期间目录不得被清理
    home
}

/// 集成测试专用**采集读源根**（`TUVIS_USAGE_FIXTURE_ROOT` 的落点，collect.rs 缝）：
/// 泄漏保活的一次性 tempdir，同一个测试二进制共用一份（env 是进程全局的，
/// `serial()` 里幂等设成它）。Windows 下 `dirs::home_dir()` 不认 HOME 重定向，
/// 采集器只看这个根才不会去扫开发机真实 profile（2026-10-07 事故族）。
#[allow(dead_code)]
pub fn usage_fixture_root() -> &'static PathBuf {
    use std::sync::OnceLock;
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let temp = tempfile::tempdir().expect("创建采集 fixture 根 tempdir 失败");
        let root = temp.path().join("usage-fixture-root");
        std::fs::create_dir_all(&root).expect("创建采集 fixture 根目录失败");
        std::mem::forget(temp); // 与 setup() 同款：测试存活期间目录不得被清理
        root
    })
}
