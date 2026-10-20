use rusqlite::Connection;

fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    conn.prepare(&format!("SELECT {} FROM {} LIMIT 0", column, table))
        .is_ok()
}

/// 列是否 NOT NULL（老库形状探测用）：表或列不存在 → None
fn column_notnull(conn: &Connection, table: &str, column: &str) -> Option<bool> {
    conn.prepare(&format!("PRAGMA table_info({})", table))
        .ok()?
        .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(3)?)))
        .ok()?
        .filter_map(Result::ok)
        .find(|(name, _)| name == column)
        .map(|(_, notnull)| notnull != 0)
}

pub fn migrate(conn: &Connection) -> Result<(), String> {
    // ---- 用量账本（计划①）①：预览版明细表**形状不可就地迁移** ----
    // 主键要加 project_key（记录级归属，GC 18-R2）、user_est 要从 NOT NULL DEFAULT 0 改成可空（R1）
    // ——SQLite 改不了主键、也去不掉 NOT NULL。处理：整表重建（明细本就是「源文件还在、可重采」的
    // 增量账，且账本从未随版本发布过），四张表数据一并清空——**否则**「明细被删但游标已推进」会留下
    // 永久缺口、日聚合还会被重采的同一批记录二次累加。
    //
    // ⚠️ 本段**必须**在下面 `schema::init` **之前**执行：schema.rs 会在 usage_detail 上建
    // `idx_usage_detail_project(project_key, day_key)`，而预览版明细表没有 project_key 列，
    // 那时建索引会以 `no such column: project_key` 让 `schema::init`（内部 `.expect`）直接 panic。
    // （任务书 Step 4 把本段放在 migrate 末尾 → 永远不可达，实测见报告「偏差申报」。）
    let preview_detail = column_exists(conn, "usage_detail", "hour_key")
        && (!column_exists(conn, "usage_detail", "project_key")
            || column_notnull(conn, "usage_detail", "user_est") == Some(true));
    if preview_detail {
        // DROP 而非 DELETE 其余三表：预览版四表由同一条 DDL 批量建立，形状同样可能不合规
        // （缺列同样会让 init 建索引/后续读列失败）；DROP 后由紧随的 init 按新 DDL 重建，
        // 语义与「四表清空」完全一致（数据一律不保留）
        conn.execute_batch(
            "DROP TABLE IF EXISTS usage_detail;
             DROP TABLE IF EXISTS usage_daily;
             DROP TABLE IF EXISTS usage_cursor;
             DROP TABLE IF EXISTS usage_session;",
        )
        .map_err(|e| format!("MIGRATE_PREVIEW_LEDGER_FAIL: 重建预览版用量账本失败: {}", e))?;
        log::warn!("usage: 检测到预览版账本形状，已重建（四表清空，下次采集重算）");
    }

    // 先幂等重放 schema.rs 全量 DDL（全部 IF NOT EXISTS）：老库/裸连接补建新表，
    // 已存在的表保持原样（缺列由下方 ALTER 迁移补齐），与生产启动顺序（schema::init → migrate）一致。
    // 上面 DROP 掉的预览版账本四表，由这次 init 按新 DDL 重建为空表。
    crate::database::schema::init(conn);

    // 检查 extension 表是否有 manifest_path 列
    if !column_exists(conn, "extensions", "manifest_path") {
        conn.execute("ALTER TABLE extensions ADD COLUMN manifest_path TEXT", [])
            .map_err(|e| format!("MIGRATE_FAIL: 迁移失败: {}", e))?;
    }
    if !column_exists(conn, "extensions", "permissions") {
        conn.execute("ALTER TABLE extensions ADD COLUMN permissions TEXT", [])
            .map_err(|e| format!("MIGRATE_FAIL: 迁移失败: {}", e))?;
    }
    if !column_exists(conn, "extensions", "min_runtime") {
        conn.execute("ALTER TABLE extensions ADD COLUMN min_runtime TEXT", [])
            .map_err(|e| format!("MIGRATE_FAIL: 迁移失败: {}", e))?;
    }

    // 旧版 extensions 表没有 suite/source_tool/is_native，会导致资源列表和补链读不到历史数据
    if !column_exists(conn, "extensions", "suite") {
        conn.execute("ALTER TABLE extensions ADD COLUMN suite TEXT", [])
            .map_err(|e| format!("MIGRATE_EXT_FIELDS_FAIL: 迁移扩展资源字段失败: {}", e))?;
    }
    if !column_exists(conn, "extensions", "source_tool") {
        conn.execute("ALTER TABLE extensions ADD COLUMN source_tool TEXT", [])
            .map_err(|e| format!("MIGRATE_EXT_FIELDS_FAIL: 迁移扩展资源字段失败: {}", e))?;
    }
    if !column_exists(conn, "extensions", "is_native") {
        conn.execute(
            "ALTER TABLE extensions ADD COLUMN is_native INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .map_err(|e| format!("MIGRATE_EXT_FIELDS_FAIL: 迁移扩展资源字段失败: {}", e))?;
    }

    // 预设 v2（spec §4）：presets 补描述/类型/绑定工具三列（新表由 schema.rs IF NOT EXISTS 覆盖）
    for (col, ddl) in [
        (
            "description",
            "ALTER TABLE presets ADD COLUMN description TEXT NOT NULL DEFAULT ''",
        ),
        (
            "scope",
            "ALTER TABLE presets ADD COLUMN scope TEXT NOT NULL DEFAULT 'universal'",
        ),
        (
            "bound_tool",
            "ALTER TABLE presets ADD COLUMN bound_tool TEXT",
        ),
    ] {
        if !column_exists(conn, "presets", col) {
            conn.execute(ddl, []).map_err(|e| {
                format!("MIGRATE_PRESET_COL_FAIL: 迁移 presets.{} 失败: {}", col, e)
            })?;
        }
    }

    // 根据旧表记录回填来源工具
    conn.execute_batch(
        "UPDATE extensions SET source_tool = 'claude'
            WHERE kind = 'skill' AND source_tool IS NULL
              AND (source_path LIKE '%/.claude/skills/%' OR tags = 'claude');
         UPDATE extensions SET source_tool = 'codex'
            WHERE kind = 'skill' AND source_tool IS NULL
              AND (source_path LIKE '%/.codex/skills/%'
                   OR source_path LIKE '%/.agents/skills/%'
                   OR tags = 'codex');
         UPDATE extensions SET source_tool = 'opencode'
            WHERE kind = 'skill' AND source_tool IS NULL
              AND (source_path LIKE '%/.config/opencode/skills/%' OR tags = 'opencode');
         UPDATE extensions SET source_tool = 'openclaw'
            WHERE kind = 'skill' AND source_tool IS NULL
              AND (source_path LIKE '%/.openclaw/skills/%' OR tags = 'openclaw');",
    )
    .map_err(|e| format!("MIGRATE_SOURCE_TOOL_FAIL: 回填来源工具失败: {}", e))?;

    // 015：native_extensions 表从未被业务写入，移除（历史库中 DROP）
    conn.execute_batch("DROP TABLE IF EXISTS native_extensions;")
        .map_err(|e| {
            format!(
                "MIGRATE_DROP_NATIVE_EXT_FAIL: 移除 native_extensions 失败: {}",
                e
            )
        })?;

    // M2（远程接入）：已配对设备表（cookie deviceId 跨重启持久）
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS remote_devices (
             id TEXT PRIMARY KEY,
             name TEXT NOT NULL DEFAULT '',
             ua TEXT NOT NULL DEFAULT '',
             origin_ip TEXT NOT NULL DEFAULT '',
             fingerprint TEXT NOT NULL DEFAULT '',
             via TEXT NOT NULL DEFAULT '',
             first_paired_at INTEGER NOT NULL DEFAULT 0,
             last_seen_at INTEGER NOT NULL DEFAULT 0,
             revoked INTEGER NOT NULL DEFAULT 0
         );",
    )
    .map_err(|e| format!("MIGRATE_REMOTE_DEVICES_FAIL: 建 remote_devices 失败: {}", e))?;

    // M5 A1：设备指纹（sha256(UA|"|"|origin_ip) hex 小写，同浏览器重绑 upsert 的去重键）
    // 与接入通道（ASCII 枚举 "local"|"lan"|"quick"|"named"）两列——
    // 存量库走 ALTER（新库已由上方建表语句直建，column_exists 短路）。
    // SQLite 加 NOT NULL 列必须带 DEFAULT。存量行取 DEFAULT 空串：指纹算法在 Rust 侧
    // 无法在 SQL 内回填，空串也永不与真实指纹撞键（只影响首次重连多一行，可接受）
    if !column_exists(conn, "remote_devices", "fingerprint") {
        conn.execute(
            "ALTER TABLE remote_devices ADD COLUMN fingerprint TEXT NOT NULL DEFAULT ''",
            [],
        )
        .map_err(|e| format!("MIGRATE_DEVICE_FINGERPRINT_FAIL: 迁移设备指纹列失败: {}", e))?;
    }
    if !column_exists(conn, "remote_devices", "via") {
        conn.execute(
            "ALTER TABLE remote_devices ADD COLUMN via TEXT NOT NULL DEFAULT ''",
            [],
        )
        .map_err(|e| format!("MIGRATE_DEVICE_VIA_FAIL: 迁移设备 via 列失败: {}", e))?;
    }

    // ---- 用量账本（计划①）②：其余列逐列补齐 ----
    // 新库已由 schema.rs 直建，column_exists 全部短路。**幂等**是硬要求（重复调用不得重复加列）。
    // 注：usage_detail 的 day_key 那条已被上面的重建分支覆盖（且它是索引引用列，缺失时 init 会先
    // panic → 实际不可达，仅作纵深防御保留）；tool_stats 不进任何索引，是本循环里唯一可达的
    // usage_detail 补列路径。
    for (table, col, ddl) in [
        (
            "usage_detail",
            "day_key",
            "ALTER TABLE usage_detail ADD COLUMN day_key TEXT NOT NULL DEFAULT ''",
        ),
        (
            "usage_detail",
            "tool_stats",
            "ALTER TABLE usage_detail ADD COLUMN tool_stats TEXT NOT NULL DEFAULT ''",
        ),
        (
            "usage_daily",
            "user_est",
            // 可空、无默认（R1）：老库补列后该列全为 NULL = 「不可得」，与语义一致
            "ALTER TABLE usage_daily ADD COLUMN user_est INTEGER",
        ),
        (
            "usage_cursor",
            "mtime_ms",
            "ALTER TABLE usage_cursor ADD COLUMN mtime_ms INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "usage_cursor",
            "file_size",
            "ALTER TABLE usage_cursor ADD COLUMN file_size INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "usage_cursor",
            "state_json",
            "ALTER TABLE usage_cursor ADD COLUMN state_json TEXT NOT NULL DEFAULT ''",
        ),
        (
            "usage_session",
            "originator",
            "ALTER TABLE usage_session ADD COLUMN originator TEXT",
        ),
        (
            "usage_session",
            "project_path_raw",
            "ALTER TABLE usage_session ADD COLUMN project_path_raw TEXT NOT NULL DEFAULT ''",
        ),
        (
            "usage_session",
            "project_realpath",
            "ALTER TABLE usage_session ADD COLUMN project_realpath TEXT NOT NULL DEFAULT ''",
        ),
    ] {
        if !column_exists(conn, table, col) {
            conn.execute(ddl, []).map_err(|e| {
                format!("MIGRATE_TABLE_COL_FAIL: 迁移 {}.{} 失败: {}", table, col, e)
            })?;
        }
    }
    // Codex 动作账本：人工解除屏障标记列——additive 幂等迁移；可空、无默认
    // （存量行全 NULL = 从未人工解除，无回填需求）。
    if !column_exists(conn, "codex_actions", "barrier_force_released_at") {
        conn.execute(
            "ALTER TABLE codex_actions ADD COLUMN barrier_force_released_at INTEGER",
            [],
        )
        .map_err(|e| {
            format!(
                "MIGRATE_CODEX_BARRIER_RELEASE_FAIL: 迁移 codex_actions.barrier_force_released_at 失败: {}",
                e
            )
        })?;
    }
    // 签名开关冻结值（2026-10-20 复审）：登记时固定该动作的签名形态，排队期间
    // 改设置/重启/同 actionId 重试都不改变既定正文。旧行补列后保持 NULL = 升级前
    // 语义（恒带签名），不误改已完成动作。
    if !column_exists(conn, "codex_actions", "signature_applied") {
        conn.execute(
            "ALTER TABLE codex_actions ADD COLUMN signature_applied INTEGER",
            [],
        )
        .map_err(|e| {
            format!(
                "MIGRATE_SIGNATURE_APPLIED_FAIL: 迁移 codex_actions.signature_applied 失败: {}",
                e
            )
        })?;
    }

    // 老库 day_key 空串回填（派生列口径：strftime 不可用于 'YYYY-MM-DDTHH' 形态，用 substr）
    conn.execute(
        "UPDATE usage_detail SET day_key = substr(hour_key, 1, 10) WHERE day_key = ''",
        [],
    )
    .map_err(|e| {
        format!(
            "MIGRATE_USAGE_DAYKEY_FAIL: 回填 usage_detail.day_key 失败: {}",
            e
        )
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_adds_skill_source_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE extensions (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                description TEXT,
                source_path TEXT NOT NULL,
                source_url TEXT,
                version TEXT,
                tags TEXT,
                installed_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );",
        )
        .unwrap();
        migrate(&conn).unwrap();
        assert!(conn
            .prepare("SELECT source_tool FROM extensions LIMIT 0")
            .is_ok());
        assert!(conn
            .prepare("SELECT is_native FROM extensions LIMIT 0")
            .is_ok());
    }

    #[test]
    fn migrate_backfills_source_tool_from_path() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE extensions (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                description TEXT,
                source_path TEXT NOT NULL,
                source_url TEXT,
                version TEXT,
                tags TEXT,
                installed_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            INSERT INTO extensions (id, kind, name, source_path, tags, installed_at, updated_at)
            VALUES ('skill-old', 'skill', 'old', '/Users/test/.agents/skills/old', 'codex', 'now', 'now');"
        ).unwrap();

        migrate(&conn).unwrap();

        let source_tool: Option<String> = conn
            .query_row(
                "SELECT source_tool FROM extensions WHERE id = 'skill-old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(source_tool.as_deref(), Some("codex"));
    }

    /// M5 A1：旧结构 remote_devices（M4，无 fingerprint/via）经迁移后两列存在，
    /// 存量行取 DEFAULT 空串（无法回填指纹——算法在 Rust 侧，且空串永不与真实指纹撞键）
    #[test]
    fn migrate_adds_remote_device_fingerprint_and_via_columns() {
        let conn = Connection::open_in_memory().unwrap();
        // migrate 首段是 ALTER TABLE extensions——沿用本文件既有测试做法先建 extensions
        conn.execute_batch(
            "CREATE TABLE extensions (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                description TEXT,
                source_path TEXT NOT NULL,
                source_url TEXT,
                version TEXT,
                tags TEXT,
                installed_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );",
        )
        .unwrap();
        // 旧结构表：M4 原始形状（故意不含两新列），并预置一行存量数据
        conn.execute_batch(
            "CREATE TABLE remote_devices (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL DEFAULT '',
                ua TEXT NOT NULL DEFAULT '',
                origin_ip TEXT NOT NULL DEFAULT '',
                first_paired_at INTEGER NOT NULL DEFAULT 0,
                last_seen_at INTEGER NOT NULL DEFAULT 0,
                revoked INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO remote_devices (id, name) VALUES ('legacy', '旧设备');",
        )
        .unwrap();
        migrate(&conn).unwrap();
        assert!(conn
            .prepare("SELECT fingerprint FROM remote_devices LIMIT 0")
            .is_ok());
        assert!(conn
            .prepare("SELECT via FROM remote_devices LIMIT 0")
            .is_ok());
        // 幂等：迁移两遍不报错（column_exists 短路 ALTER）
        migrate(&conn).unwrap();
        // 存量行新列为 DEFAULT 空串
        let (fp, via): (String, String) = conn
            .query_row(
                "SELECT fingerprint, via FROM remote_devices WHERE id = 'legacy'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((fp.as_str(), via.as_str()), ("", ""));
    }

    /// M5 A1：全新库（schema::init → migrate）建出的 remote_devices 直含两列，
    /// 无需走 ALTER 路径
    #[test]
    fn fresh_remote_devices_table_includes_fingerprint_and_via() {
        let conn = Connection::open_in_memory().unwrap();
        // 真机调用序：schema::init 建全部表 → migration::migrate 增量迁移
        crate::database::schema::init(&conn);
        migrate(&conn).unwrap();
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(remote_devices)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            cols.contains(&"fingerprint".to_string()),
            "缺 fingerprint 列: {cols:?}"
        );
        assert!(cols.contains(&"via".to_string()), "缺 via 列: {cols:?}");
    }

    #[test]
    fn migrate_drops_native_extensions() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE extensions (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                description TEXT,
                source_path TEXT NOT NULL,
                source_url TEXT,
                version TEXT,
                tags TEXT,
                installed_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE native_extensions (id TEXT PRIMARY KEY);",
        )
        .unwrap();
        migrate(&conn).unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='native_extensions'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!exists);
    }

    /// 预设 v2（spec §4）：老库迁移补 presets 3 列 + 5 张新表
    #[test]
    fn migrate_adds_preset_v2_columns_and_tables() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE extensions (
                id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL,
                description TEXT, source_path TEXT NOT NULL, source_url TEXT,
                version TEXT, tags TEXT, installed_at TEXT NOT NULL, updated_at TEXT NOT NULL
            );
            CREATE TABLE presets (
                id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at TEXT NOT NULL
            );
            CREATE TABLE preset_items (
                id TEXT PRIMARY KEY, preset_id TEXT NOT NULL,
                extension_id TEXT NOT NULL, kind TEXT NOT NULL
            );",
        )
        .unwrap();

        migrate(&conn).unwrap();

        for col in ["description", "scope", "bound_tool"] {
            assert!(
                conn.prepare(&format!("SELECT {} FROM presets LIMIT 0", col))
                    .is_ok(),
                "presets 缺列 {}",
                col
            );
        }
        for table in [
            "resource_bindings",
            "tool_residents",
            "tool_base_snapshots",
            "tool_base_snapshot_items",
            "stash_journal",
        ] {
            let exists: bool = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='{}'",
                        table
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(exists, "缺表 {}", table);
        }
    }

    /// 列是否 NOT NULL（形状断言用）：表或列不存在 → panic（不静默当成 false）
    fn notnull(conn: &Connection, table: &str, col: &str) -> bool {
        conn.prepare(&format!("PRAGMA table_info({})", table))
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(3)?)))
            .unwrap()
            .filter_map(Result::ok)
            .find(|(name, _)| name == col)
            .map(|(_, nn)| nn != 0)
            .unwrap_or_else(|| panic!("{table} 缺列 {col}"))
    }

    /// 用量账本 4 张表：全新库（schema::init → migrate）建齐全部列，且 migrate 幂等
    #[test]
    fn usage_tables_created_with_expected_columns() {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // 幂等：跑两遍不报错

        let cols = |table: &str| -> Vec<String> {
            conn.prepare(&format!("PRAGMA table_info({})", table))
                .unwrap()
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        for t in [
            "usage_detail",
            "usage_daily",
            "usage_cursor",
            "usage_session",
        ] {
            // 注意：任务书此处写的是 assert_eq!(…, true)；clippy 的 bool_assert_comparison 在
            // 本任务门禁 `--all-targets -- -D warnings` 下会直接报 error（实测 migration.rs:486），
            // 故等价改写为 assert!（期望值未变：仍要求该查询为 true）
            assert!(
                conn.query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
                    [t],
                    |r| r.get::<_, bool>(0)
                )
                .unwrap(),
                "缺表 {t}"
            );
        }
        // 明细表：**project_key 为第三列（记录级，进主键，D21 规则④）**、小时桶第四列，
        // 列序与主键列序同契约 §4；day_key 为派生冗余列（见计划 Global Constraints 15/18-R2）
        assert_eq!(
            cols("usage_detail"),
            vec![
                "source_id",
                "session_id",
                "project_key",
                "hour_key",
                "day_key",
                "model",
                "provider",
                "provider_kind",
                "input_fresh",
                "cache_read",
                "cache_write",
                "output",
                "request_total",
                "requests",
                "user_est",
                "cache_semantics",
                "turns",
                "error_model",
                "error_turn",
                "error_tool",
                "interrupted",
                "tool_calls",
                "tool_ms",
                "turn_ms",
                "tool_stats",
                "first_seen_at",
                "updated_at"
            ]
        );
        // R1：`user_est` 在**明细表与日聚合表**都必须可空（拿不到就是 NULL，绝不 NOT NULL DEFAULT 0）
        assert!(
            !notnull(&conn, "usage_detail", "user_est"),
            "R1：明细 user_est 必须可空"
        );
        // 日聚合表：永久保留，按（日 × 源 × 项目 × 供应商 × 模型）折叠
        assert!(cols("usage_daily").contains(&"project_key".to_string()));
        assert!(cols("usage_daily").contains(&"request_total".to_string()));
        assert!(
            cols("usage_daily").contains(&"provider_kind".to_string()),
            "三态标记随日聚合保留"
        );
        assert!(
            cols("usage_daily").contains(&"user_est".to_string()),
            "R1：日聚合必须有 user_est 列"
        );
        assert!(
            !notnull(&conn, "usage_daily", "user_est"),
            "R1：日聚合 user_est 必须可空"
        );
        // 日聚合**刻意不带**的源能力相关列（逐条说明，见 Step 3 的 DDL 注释）：
        // provider_kind 已在（三态）；计数类（turns/error_*/interrupted/tool_*/turn_ms）与
        // cache_semantics 不入日聚合——它们的口径是「只在明细保留期内可得」（遗留点 10 已登记取舍）
        for c in [
            "turns",
            "error_model",
            "tool_calls",
            "cache_semantics",
            "turn_ms",
        ] {
            assert!(
                !cols("usage_daily").contains(&c.to_string()),
                "usage_daily 不应有 {c}（口径见 DDL 注释）"
            );
        }
        // 游标表：字节双水位 + 尾指纹 + 文件代际（mtime/size，未变即跳过读取）+ 续读状态
        for c in [
            "fingerprint",
            "byte_offset",
            "ordinal",
            "last_cumulative",
            "mtime_ms",
            "file_size",
            "state_json",
        ] {
            assert!(
                cols("usage_cursor").contains(&c.to_string()),
                "usage_cursor 缺列 {c}"
            );
        }
        // 会话维度表：D17 分层所需字段 + D21 项目四列 + Codex originator（§5.2 CLI/APP 拆分留档）
        for c in [
            "project_key",
            "project_label",
            "project_path_raw",
            "project_realpath",
            "title",
            "is_subagent",
            "parent_session_id",
            "originator",
        ] {
            assert!(
                cols("usage_session").contains(&c.to_string()),
                "usage_session 缺列 {c}"
            );
        }
    }

    /// 明细表唯一键：同一 (源,会话,小时,**项目**,模型,供应商) 只允许一行；不同小时 / 不同项目各行独立
    #[test]
    fn usage_detail_unique_key_is_hourly() {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        let ins = |hour: &str, project: &str| {
            conn.execute(
                "INSERT INTO usage_detail
                 (source_id, session_id, hour_key, project_key, day_key, model, provider,
                  input_fresh, cache_read, cache_write, output, request_total, requests, user_est,
                  cache_semantics, first_seen_at, updated_at)
                 VALUES ('claude','s1',?1,?2,substr(?1,1,10),'m','p',1,0,0,1,2,1,NULL,
                         'exclusive',0,0)",
                rusqlite::params![hour, project],
            )
        };
        assert!(ins("2026-10-03T09", "a").is_ok());
        assert!(ins("2026-10-03T10", "a").is_ok(), "不同小时桶必须各行独立");
        assert!(
            ins("2026-10-03T09", "a").is_err(),
            "同一小时桶重复插入必须撞主键"
        );
        // R2：同一会话同一小时里的**两个 cwd** 必须是两行（否则记录级归属会被折叠掉）
        assert!(
            ins("2026-10-03T09", "b").is_ok(),
            "同一会话同一小时的两个项目必须各行独立"
        );
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_detail", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3);
        // R1：无值写入就是 NULL，不是 0
        let nulls: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_detail WHERE user_est IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(nulls, 3, "拿不到 userEst 时必须落 NULL");
    }

    /// R1/R2 迁移：预览版明细表（无 project_key、user_est NOT NULL DEFAULT 0）→ **整表重建**成新形状，
    /// 且四张表数据一并清空（否则「明细被删但游标已推进」会留永久缺口、日聚合会被重采记录二次累加）
    #[test]
    fn migrate_rebuilds_preview_ledger_shape() {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        // 造一个预览版形状（旧 DDL：无 project_key、user_est NOT NULL DEFAULT 0），并塞入残留数据
        conn.execute_batch(
            "DROP TABLE usage_detail;
             CREATE TABLE usage_detail (
                 source_id TEXT NOT NULL, session_id TEXT NOT NULL, hour_key TEXT NOT NULL,
                 day_key TEXT NOT NULL, model TEXT NOT NULL DEFAULT '',
                 provider TEXT NOT NULL DEFAULT '', user_est INTEGER NOT NULL DEFAULT 0,
                 updated_at INTEGER NOT NULL,
                 PRIMARY KEY (source_id, session_id, hour_key, model, provider));
             INSERT INTO usage_detail VALUES
                 ('claude','s1','2026-10-03T09','2026-10-03','m','p',0,1);
             INSERT INTO usage_daily (day_key, source_id, project_key, provider, model, updated_at)
                 VALUES ('2026-10-03','claude','a','p','m',1);
             INSERT INTO usage_cursor (source_id, session_id, updated_at) VALUES ('claude','f1',1);
             INSERT INTO usage_session (source_id, session_id, first_seen_at, last_seen_at)
                 VALUES ('claude','s1',1,1);",
        )
        .unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // 幂等：第二次不再重建（形状已合规）
                                 // R2：project_key 已进主键（同会话同小时两个项目可并存）
        conn.execute(
            "INSERT INTO usage_detail
             (source_id, session_id, hour_key, project_key, day_key, model, provider,
              first_seen_at, updated_at)
             VALUES ('claude','s1','2026-10-03T09','a','2026-10-03','m','p',0,0),
                    ('claude','s1','2026-10-03T09','b','2026-10-03','m','p',0,0)",
            [],
        )
        .unwrap();
        assert!(
            !notnull(&conn, "usage_detail", "user_est"),
            "R1：重建后 user_est 必须可空"
        );
        assert!(
            !notnull(&conn, "usage_daily", "user_est"),
            "R1：日聚合 user_est 必须可空"
        );
        // 四表清空（重建的语义完整性，不只清明细）
        for t in [
            "usage_detail",
            "usage_daily",
            "usage_cursor",
            "usage_session",
        ] {
            // 明细已被上面两条新插入占位 → 单独判
            if t == "usage_detail" {
                let n: i64 = conn
                    .query_row("SELECT COUNT(*) FROM usage_detail", [], |r| r.get(0))
                    .unwrap();
                assert_eq!(n, 2, "重建后只应有测试新插入的两行");
                continue;
            }
            let n: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {}", t), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "预览版残留必须清空: {t}");
        }
    }

    /// 老库补列路径（形状已是新主键、但缺若干列）：migrate 逐列补齐、**不丢已有数据**、重复调用幂等。
    /// 任务书的两条用例分别锁「全新库」与「预览版整表重建」；本用例补第三条路径——上面 9 条 ALTER
    /// 若不在此走到，语句写错也没有任何用例会发现（全新库全部短路、预览版整表 DROP 后由 init 重建）。
    /// 私有内存库，不碰 tests/support.rs 的共享库（GC 19）。
    #[test]
    fn migrate_adds_missing_ledger_columns_without_data_loss() {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        // 造「新形状但缺列」的老库：SQLite 3.35+ 支持 DROP COLUMN，但**被索引引用的列删不掉**
        // （day_key / project_key 都进了索引），故这里只删不进任何索引的列
        conn.execute_batch(
            "ALTER TABLE usage_detail DROP COLUMN tool_stats;
             ALTER TABLE usage_daily DROP COLUMN user_est;
             ALTER TABLE usage_cursor DROP COLUMN mtime_ms;
             ALTER TABLE usage_cursor DROP COLUMN file_size;
             ALTER TABLE usage_cursor DROP COLUMN state_json;
             ALTER TABLE usage_session DROP COLUMN originator;
             ALTER TABLE usage_session DROP COLUMN project_path_raw;
             ALTER TABLE usage_session DROP COLUMN project_realpath;
             INSERT INTO usage_detail
                 (source_id, session_id, project_key, hour_key, day_key, model, provider,
                  input_fresh, user_est, first_seen_at, updated_at)
                 VALUES ('claude','s1','a','2026-10-03T09','2026-10-03','m','p',7,42,0,0);
             INSERT INTO usage_cursor (source_id, session_id, updated_at) VALUES ('claude','f1',0);
             INSERT INTO usage_session (source_id, session_id, first_seen_at, last_seen_at)
                 VALUES ('claude','s1',0,0);",
        )
        .unwrap();
        // 前提断言（防止假绿）：这些列在迁移前**确实**不在，否则后面的「已补齐」断言什么都没锁住
        for (t, c) in [
            ("usage_detail", "tool_stats"),
            ("usage_daily", "user_est"),
            ("usage_cursor", "state_json"),
            ("usage_session", "originator"),
        ] {
            assert!(
                !column_exists(&conn, t, c),
                "前提不成立：{t}.{c} 本应已被删除"
            );
        }
        migrate(&conn).unwrap();
        // 幂等：再跑一遍（补过的列不再 ALTER——重复 ADD COLUMN 会报 duplicate column name）
        migrate(&conn).unwrap();
        assert!(
            notnull(&conn, "usage_detail", "tool_stats"),
            "补列后 tool_stats 应存在"
        );
        // R1：补列路径也必须让 user_est 可空（ADD COLUMN 时不给 DEFAULT，老行落 NULL = 不可得）
        assert!(
            !notnull(&conn, "usage_daily", "user_est"),
            "R1：补列后日聚合 user_est 必须仍可空"
        );
        // notnull() 在表/列缺失时 panic → 这几条同时锁「列已补齐」
        for (t, c) in [
            ("usage_cursor", "mtime_ms"),
            ("usage_cursor", "file_size"),
            ("usage_cursor", "state_json"),
            ("usage_session", "originator"),
            ("usage_session", "project_path_raw"),
            ("usage_session", "project_realpath"),
        ] {
            let _ = notnull(&conn, t, c);
        }
        // 与「预览版重建」路径的分野：补列路径**不得**清掉存量数据
        let (fresh, est): (i64, Option<i64>) = conn
            .query_row(
                "SELECT input_fresh, user_est FROM usage_detail
                 WHERE source_id='claude' AND session_id='s1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((fresh, est), (7, Some(42)), "补列不得丢存量明细行");
        let cursors: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_cursor", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            cursors, 1,
            "补列不得清空游标（清了会重采，与重建路径的取舍不同）"
        );
    }

    /// Codex 动作账本：旧结构（**两列都不存在**）经迁移补列（barrier + signature）——
    /// `barrier_force_released_at` 与 `signature_applied` 都可空、存量行保留 NULL；
    /// 幂等两遍不报错。
    #[test]
    fn migrate_adds_codex_barrier_and_signature_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE codex_actions (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                action_id TEXT UNIQUE NOT NULL,
                device_id TEXT NOT NULL,
                device_name TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                request_key TEXT,
                context_token TEXT,
                payload_hash TEXT NOT NULL,
                client_message_id TEXT NOT NULL,
                state TEXT NOT NULL,
                route TEXT NOT NULL DEFAULT 'none',
                queued_text TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                receipt_json TEXT,
                compat_outstanding INTEGER NOT NULL DEFAULT 0,
                compat_message_id TEXT,
                compat_turn_id TEXT,
                compat_rollout_path TEXT,
                compat_rollout_baseline INTEGER,
                compat_wire_hash TEXT
            );
            INSERT INTO codex_actions (action_id, device_id, device_name, thread_id, kind,
                 payload_hash, client_message_id, state, created_at, updated_at, compat_outstanding)
             VALUES ('legacy-a', 'd', 'iPhone', 't', 'send', 'h', 'cm', 'unknown', 1, 1, 1);",
        )
        .unwrap();
        migrate(&conn).unwrap();
        let (flag, ts, signed): (i64, Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT compat_outstanding, barrier_force_released_at, signature_applied \
                 FROM codex_actions WHERE action_id = 'legacy-a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (flag, ts, signed),
            (1, None, None),
            "存量行保留，两列均为 NULL（旧行签名语义 = 升级前恒签名）"
        );
        // 幂等：迁移两遍不报错
        migrate(&conn).unwrap();
    }
}
