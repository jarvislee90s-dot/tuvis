use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::database::connection::DB;

// ===== 扩展资源 =====

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionRecord {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub description: Option<String>,
    pub source_path: String,
    pub source_url: Option<String>,
    pub version: Option<String>,
    pub tags: Option<String>,
    pub suite: Option<String>,
    pub source_tool: Option<String>,
    pub is_native: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssignmentRecord {
    pub id: String,
    pub extension_id: String,
    pub agent_tool_id: String,
    pub sub_agent_id: Option<String>,
    pub enabled: bool,
    pub link_status: String,
}

pub fn insert_extension(ext: &ExtensionRecord) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR REPLACE INTO extensions (id, kind, name, description, source_path, source_url, version, tags, suite, source_tool, is_native, installed_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![ext.id, ext.kind, ext.name, ext.description, ext.source_path, ext.source_url, ext.version, ext.tags, ext.suite, ext.source_tool, ext.is_native as i64, &now, &now],
    ).map_err(|e| e.to_string()).map(|_| ())
}

/// 幂等回填：行不存在才插入（INSERT OR IGNORE），不覆盖既有元数据。
/// 预设 v2 数据源统一（spec §8.1）：注册表以 extensions 表为唯一真值源
pub fn ensure_extension(ext: &ExtensionRecord) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR IGNORE INTO extensions (id, kind, name, description, source_path, source_url, version, tags, suite, source_tool, is_native, installed_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
        params![ext.id, ext.kind, ext.name, ext.description, ext.source_path, ext.source_url, ext.version, ext.tags, ext.suite, ext.source_tool, ext.is_native as i64, &now],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 旧平台分隔符（Windows 反斜杠）skill id **就地迁移**为斜杠口径（`migrate_backslash_skill_ids`
/// 清单迁移的逐行核）：spec §8.1 约定 id/name 均带斜杠路径，2026-10-13 前的 Windows 机器上
/// 嵌套套件技能登记出了 `skill-a\b` 形态的 id。schema 无外键约束（先例：引用表都是裸 TEXT
/// 关联），引用搬迁必须显式做——搬迁面 = 引用 extension_id 的**全部五张表**（2026-10-13 评审
/// P1 补齐 `extension_assignments` / `tool_base_snapshot_items`，漏搬 = 技能启用态失联）。
/// 迁移顺序 = 先搬引用再改主行，**全程单事务**——中途失败整体回滚（引用与主行一起
/// 留在旧 id，不存在「引用已指向新 id、主行还是旧 id」的半截态），下次启动按同一
/// 清单重跑收敛。目标 id 已存在（病态双行）时按「斜杠行为准」合并：引用搬完删旧行。
/// 返回是否发生了迁移。连接版核（先例 `delete_extension_on`）：DB 锁留给调用方——生产
/// 调用点已持有 `DB` 锁。事务取 `&Connection` 上的 `unchecked_transaction`
/// （先例 `dao/preset.rs::update_preset`）
pub fn migrate_legacy_extension_id_on(
    conn: &rusqlite::Connection,
    old_id: &str,
    new_id: &str,
    new_name: &str,
) -> Result<bool, String> {
    if old_id == new_id {
        return Ok(false);
    }
    let old_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM extensions WHERE id = ?1 AND kind = 'skill'",
            params![old_id],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .map_err(|e| e.to_string())?;
    if !old_exists {
        return Ok(false);
    }
    let new_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM extensions WHERE id = ?1",
            params![new_id],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .map_err(|e| e.to_string())?;
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    // 引用搬迁。`extension_assignments` / `resource_bindings` / `tool_residents` /
    // `tool_base_snapshot_items` 都有含 extension_id 的唯一键（UNIQUE/PK）→ `OR IGNORE`
    // 兼位「新 id 已有同引用行」的冲突，被跳过的行成为孤儿引用（仅病态双行情形，
    // 如实登记不掩盖；注意 SQLite 的 UNIQUE 把 NULL 视作互不相同，
    // `extension_assignments` 的工具级行 `sub_agent_id IS NULL` 撞不上车——搬迁行照搬，
    // 落成同维度重复行，无孤儿）；`preset_items` 对 extension_id **无**唯一约束（唯一键只有行
    // id）→ `OR IGNORE` 对它是空操作，必须普通 UPDATE 全搬——病态「同一预设同时含
    // 新旧两个 id」会留成重复条目，量级个位数、可由用户编辑消除。
    tx.execute(
        "UPDATE preset_items SET extension_id = ?2 WHERE extension_id = ?1",
        params![old_id, new_id],
    )
    .map_err(|e| format!("迁移 preset_items 引用失败: {e}"))?;
    for table in [
        "extension_assignments",
        "resource_bindings",
        "tool_residents",
        "tool_base_snapshot_items",
    ] {
        tx.execute(
            &format!("UPDATE OR IGNORE {table} SET extension_id = ?2 WHERE extension_id = ?1"),
            params![old_id, new_id],
        )
        .map_err(|e| format!("迁移 {table} 引用失败: {e}"))?;
    }
    if new_exists {
        // 病态双行：斜杠行为准，删反斜杠旧行（引用已全部指向新 id）
        tx.execute("DELETE FROM extensions WHERE id = ?1", params![old_id])
            .map_err(|e| e.to_string())?;
    } else {
        tx.execute(
            "UPDATE extensions SET id = ?2, name = ?3 WHERE id = ?1 AND kind = 'skill'",
            params![old_id, new_id, new_name],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(true)
}

/// 存量反斜杠 skill id 清单（`migrate_backslash_skill_ids` 用）：`kind='skill'` 且 id 含
/// `\` 的行。SQLite 的 LIKE 默认**无 ESCAPE 字符**，模式里的 `\` 就是字面量（`'%\%'`
/// 匹配任何含反斜杠的 id，无需再写 ESCAPE 子句）——磁盘扫描触达不了目录已删的存量行，
/// 得靠这份 DB 清单兜底（2026-10-13 评审跟进）
pub fn list_backslash_skill_ids_on(conn: &rusqlite::Connection) -> Vec<String> {
    conn.prepare("SELECT id FROM extensions WHERE kind = 'skill' AND id LIKE '%\\%'")
        .ok()
        .map(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(0))
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

/// 存量反斜杠 skill id 迁移（backfill_registry 专用）：按上面的 DB 清单逐行调
/// `migrate_legacy_extension_id_on`，new_id = 旧 id 反斜杠换斜杠、new_name = 同步的斜杠 rel。
/// **与磁盘目录是否存在无关**——skill 目录已删的存量行同样收敛（磁盘扫描触达不了它们）。
/// 返回实际迁移行数；单行失败只降日志不中断（其余行照常收敛，下次启动重跑）
pub fn migrate_backslash_skill_ids() -> usize {
    let conn = DB.lock().unwrap();
    migrate_backslash_skill_ids_on(&conn)
}

pub fn migrate_backslash_skill_ids_on(conn: &rusqlite::Connection) -> usize {
    let mut moved = 0usize;
    for old_id in list_backslash_skill_ids_on(conn) {
        let rel = old_id.strip_prefix("skill-").unwrap_or(&old_id);
        // 口径归一复用服务层助手（单一出处；db 层引用服务层纯函数的先例：
        // `dao/usage.rs` 引 `services::usage::{delta,model}` 的类型）
        let slashed = crate::services::resource::slash_rel(rel);
        match migrate_legacy_extension_id_on(conn, &old_id, &format!("skill-{}", slashed), &slashed)
        {
            Ok(true) => moved += 1,
            Ok(false) => {}
            Err(err) => log::warn!("回填 skill {} 旧 id 迁移失败: {}", old_id, err),
        }
    }
    moved
}

/// 单行查询：某 id 的 is_native（#64-3 预设命令层防御用——universal 预设不得含
/// 原生项）。None = 行不存在**或 DB 查询出错**（均降级放行——防御是宁严勿松方向
/// 的加拦，不是必拦，查询失败不该把预设功能整个堵死）
pub fn extension_is_native(ext_id: &str) -> Option<bool> {
    let conn = DB.lock().unwrap();
    conn.query_row(
        "SELECT is_native FROM extensions WHERE id = ?1",
        params![ext_id],
        |r| r.get::<_, i64>(0),
    )
    .ok()
    .map(|n| n != 0)
}

pub fn list_extensions() -> Vec<ExtensionRecord> {
    let conn = DB.lock().unwrap();
    conn.prepare("SELECT id, kind, name, description, source_path, source_url, version, tags, suite, source_tool, is_native FROM extensions")
        .ok()
        .map(|mut stmt| {
            stmt.query_map([], |row| {
                Ok(ExtensionRecord {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    name: row.get(2)?,
                    description: row.get(3)?,
                    source_path: row.get(4)?,
                    source_url: row.get(5)?,
                    version: row.get(6)?,
                    tags: row.get(7)?,
                    suite: row.get(8)?,
                    source_tool: row.get(9)?,
                    is_native: row.get::<_, i64>(10)? != 0,
                })
            })
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        })
        .unwrap_or_default()
}

/// 登记的原生技能名集合（`is_native=1 AND source_tool=tool_id` 行的 name 列）——
/// 预设扫描的「登记线」兜底数据源（用户裁决 2026-09-16）：磁盘原生目录只有
/// 登记在案（经理 兔维斯 导入）才参与快照/暂存，未登记者视为常驻不参与
pub fn list_registered_native_names(tool_id: &str) -> Vec<String> {
    let conn = DB.lock().unwrap();
    conn.prepare(
        "SELECT name FROM extensions WHERE is_native = 1 AND source_tool = ?1 AND kind = 'skill'",
    )
    .ok()
    .map(|mut stmt| {
        stmt.query_map([tool_id], |row| row.get::<_, String>(0))
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    })
    .unwrap_or_default()
}

pub fn list_assignments(tool_id: &str) -> Vec<AssignmentRecord> {
    let conn = DB.lock().unwrap();
    conn.prepare("SELECT id, extension_id, agent_tool_id, sub_agent_id, enabled, link_status FROM extension_assignments WHERE agent_tool_id = ?1")
        .ok()
        .map(|mut stmt| {
            stmt.query_map([tool_id], |row| {
                Ok(AssignmentRecord {
                    id: row.get(0)?,
                    extension_id: row.get(1)?,
                    agent_tool_id: row.get(2)?,
                    sub_agent_id: row.get(3)?,
                    enabled: row.get::<_, i64>(4)? != 0,
                    link_status: row.get(5)?,
                })
            })
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        })
        .unwrap_or_default()
}

pub fn upsert_assignment_with_subagent(
    ext_id: &str,
    tool_id: &str,
    sub_agent_id: &str,
    enabled: bool,
    link_status: &str,
) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    let id = format!("{}-{}-{}", ext_id, tool_id, sub_agent_id);
    conn.execute(
        "INSERT OR REPLACE INTO extension_assignments (id, extension_id, agent_tool_id, sub_agent_id, enabled, link_status, assigned_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, ext_id, tool_id, sub_agent_id, enabled as i64, link_status, now],
    ).map_err(|e| e.to_string()).map(|_| ())
}

pub fn upsert_assignment(
    ext_id: &str,
    tool_id: &str,
    enabled: bool,
    link_status: &str,
) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    let id = format!("{}-{}", ext_id, tool_id);
    conn.execute(
        "INSERT OR REPLACE INTO extension_assignments (id, extension_id, agent_tool_id, enabled, link_status, assigned_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, ext_id, tool_id, enabled as i64, link_status, now],
    ).map_err(|e| e.to_string()).map(|_| ())
}

pub fn list_all_assignments() -> Vec<AssignmentRecord> {
    let conn = DB.lock().unwrap();
    conn.prepare("SELECT id, extension_id, agent_tool_id, sub_agent_id, enabled, link_status FROM extension_assignments")
        .ok()
        .map(|mut stmt| {
            stmt.query_map([], |row| {
                Ok(AssignmentRecord {
                    id: row.get(0)?,
                    extension_id: row.get(1)?,
                    agent_tool_id: row.get(2)?,
                    sub_agent_id: row.get(3)?,
                    enabled: row.get::<_, i64>(4)? != 0,
                    link_status: row.get(5)?,
                })
            })
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        })
        .unwrap_or_default()
}

pub fn disable_subagent_assignment(
    ext_id: &str,
    tool_id: &str,
    sub_agent_id: &str,
) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    let id = format!("{}-{}-{}", ext_id, tool_id, sub_agent_id);
    conn.execute(
        "UPDATE extension_assignments SET enabled = 0, link_status = 'missing' WHERE id = ?1",
        params![id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn delete_extension(ext_id: &str) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    delete_extension_on(&conn, ext_id)
}

pub fn delete_extension_on(conn: &rusqlite::Connection, ext_id: &str) -> Result<(), String> {
    conn.execute("DELETE FROM extensions WHERE id = ?1", params![ext_id])
        .map_err(|e| e.to_string())
        .map(|_| ())
}

/// 删除某资源的全部 assignment（含子 Agent 维度）
pub fn delete_assignments_for(ext_id: &str) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    delete_assignments_for_on(&conn, ext_id)
}

pub fn delete_assignments_for_on(conn: &rusqlite::Connection, ext_id: &str) -> Result<(), String> {
    conn.execute(
        "DELETE FROM extension_assignments WHERE extension_id = ?1",
        params![ext_id],
    )
    .map_err(|e| e.to_string())
    .map(|_| ())
}

#[cfg(test)]
mod delete_tests {
    use super::*;
    use crate::database::schema;

    fn mem_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        schema::init(&conn);
        conn
    }

    #[test]
    fn delete_extension_removes_row() {
        let conn = mem_conn();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO extensions (id, kind, name, source_path, installed_at, updated_at) \
             VALUES ('skill-x','skill','x','/tmp/x',?1,?1)",
            [&now],
        )
        .unwrap();
        delete_extension_on(&conn, "skill-x").unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM extensions WHERE id='skill-x'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn delete_assignments_covers_subagent_rows() {
        let conn = mem_conn();
        let now = chrono::Utc::now().to_rfc3339();
        conn
            .execute(
                "INSERT INTO extension_assignments \
                 (id, extension_id, agent_tool_id, sub_agent_id, enabled, link_status, assigned_at) VALUES \
                 ('a','skill-x','claude',NULL,1,'valid',?1),\
                 ('b','skill-x','claude','sub1',1,'valid',?1)",
                [&now],
            )
            .unwrap();
        delete_assignments_for_on(&conn, "skill-x").unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM extension_assignments WHERE extension_id='skill-x'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }
}

/// 生产数据迁移 `migrate_legacy_extension_id_on` 的表驱动覆盖（2026-10-13 评审 Important：
/// 这段迁移此前零测试）——内存库 + `schema::init`，零真实数据库与家目录访问；
/// 迁移只认 id、不碰磁盘，故 source_path 一律播种为不存在的路径
#[cfg(test)]
mod migration_tests {
    use super::*;
    use crate::database::schema;

    fn mem_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        schema::init(&conn);
        conn
    }

    /// 播种一行 extensions 主行（只给必填列，kind 恒为 skill）
    fn seed_extension(conn: &rusqlite::Connection, id: &str, name: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO extensions (id, kind, name, source_path, installed_at, updated_at) \
             VALUES (?1,'skill',?2,'/tmp/tuvis-nowhere',?3,?3)",
            params![id, name, now],
        )
        .unwrap();
    }

    /// 五张引用表各播一行指向 `ext_id`（只给必填列，其余走 DEFAULT/NULL）。
    /// 表驱动集中一处：新增引用表时在此补一行，漏搬某表 = 该表的引用断言当场红
    fn seed_ref_rows(conn: &rusqlite::Connection, ext_id: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO preset_items (id, preset_id, extension_id, kind) \
             VALUES ('pi-1','p-1',?1,'skill')",
            params![ext_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO extension_assignments \
             (id, extension_id, agent_tool_id, sub_agent_id, enabled, link_status, assigned_at) \
             VALUES ('asg-1',?1,'claude',NULL,1,'valid',?2)",
            params![ext_id, now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO resource_bindings (extension_id, exclusive_tools, reason, updated_at) \
             VALUES (?1,'[\"claude\"]',NULL,?2)",
            params![ext_id, now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tool_residents (tool_id, extension_id) VALUES ('claude',?1)",
            params![ext_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tool_base_snapshot_items (tool_id, extension_id, kind, origin) \
             VALUES ('claude',?1,'skill','native')",
            params![ext_id],
        )
        .unwrap();
    }

    /// 五张引用表「指向某 id 的行数」——搬迁断言与孤儿断言共用；表名随行返回，
    /// 失败消息可直接定位是哪张表漏搬
    fn ref_counts(conn: &rusqlite::Connection, ext_id: &str) -> Vec<(&'static str, i64)> {
        [
            (
                "preset_items",
                "SELECT COUNT(*) FROM preset_items WHERE extension_id = ?1",
            ),
            (
                "extension_assignments",
                "SELECT COUNT(*) FROM extension_assignments WHERE extension_id = ?1",
            ),
            (
                "resource_bindings",
                "SELECT COUNT(*) FROM resource_bindings WHERE extension_id = ?1",
            ),
            (
                "tool_residents",
                "SELECT COUNT(*) FROM tool_residents WHERE extension_id = ?1",
            ),
            (
                "tool_base_snapshot_items",
                "SELECT COUNT(*) FROM tool_base_snapshot_items WHERE extension_id = ?1",
            ),
        ]
        .into_iter()
        .map(|(table, sql)| {
            let n: i64 = conn.query_row(sql, params![ext_id], |r| r.get(0)).unwrap();
            (table, n)
        })
        .collect()
    }

    /// 五张引用表指向 `ext_id` 的行数都必须等于 `want`
    fn assert_refs(conn: &rusqlite::Connection, ext_id: &str, want: i64) {
        for (table, n) in ref_counts(conn, ext_id) {
            assert_eq!(n, want, "{table} 指向 {ext_id} 的引用行数不符");
        }
    }

    fn all_ids(conn: &rusqlite::Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT id FROM extensions ORDER BY id")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.filter_map(|r| r.ok()).collect()
    }

    /// 五表全量搬迁：五张引用表各一行指向旧 id → 迁移后引用全部改指新 id，
    /// 主行 id/name 同步更新（name 取斜杠 rel）
    #[test]
    fn migrates_all_five_reference_tables() {
        let conn = mem_conn();
        seed_extension(&conn, r"skill-a\b", r"skill-a\b");
        seed_ref_rows(&conn, r"skill-a\b");
        assert_refs(&conn, r"skill-a\b", 1);
        assert_eq!(
            ref_counts(&conn, r"skill-a\b")
                .iter()
                .map(|(_, n)| n)
                .sum::<i64>(),
            5,
            "五张引用表都必须有播种行（漏播 = 本用例静默假绿）"
        );

        let moved =
            migrate_legacy_extension_id_on(&conn, r"skill-a\b", "skill-a/b", "skill-a/b").unwrap();
        assert!(moved, "旧主行存在 → 必须报告发生了迁移");

        assert_refs(&conn, "skill-a/b", 1);
        assert_refs(&conn, r"skill-a\b", 0);
        assert_eq!(
            all_ids(&conn),
            vec!["skill-a/b".to_string()],
            "旧主行消失且不留新行"
        );
        let name: String = conn
            .query_row(
                "SELECT name FROM extensions WHERE id='skill-a/b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(name, "skill-a/b", "name 同步取斜杠 rel");
    }

    /// 幂等：对迁移后的库再跑一次 → `Ok(false)` 且数据零变化
    #[test]
    fn second_run_is_a_noop() {
        let conn = mem_conn();
        seed_extension(&conn, r"skill-a\b", r"skill-a\b");
        seed_ref_rows(&conn, r"skill-a\b");
        assert!(
            migrate_legacy_extension_id_on(&conn, r"skill-a\b", "skill-a/b", "skill-a/b").unwrap()
        );

        let again =
            migrate_legacy_extension_id_on(&conn, r"skill-a\b", "skill-a/b", "skill-a/b").unwrap();
        assert!(!again, "旧 id 已不存在 → 再跑必须 Ok(false)（幂等）");

        assert_eq!(all_ids(&conn), vec!["skill-a/b".to_string()]);
        assert_refs(&conn, "skill-a/b", 1);
        assert_refs(&conn, r"skill-a\b", 0);
        assert_eq!(
            ref_counts(&conn, "skill-a/b")
                .iter()
                .map(|(_, n)| n)
                .sum::<i64>(),
            5,
            "零变化：五表仍是各 1 行（不得又搬一遍变 2 行）"
        );
    }

    /// 病态双行合并：旧行 `skill-a\b` 与新行 `skill-a/b` 并存，且四张唯一键表在新 id
    /// 侧**已有同键引用行**（撞车面）→ 斜杠行为准：主行删旧留新；撞车行被 `OR IGNORE`
    /// 跳过，**如实留成孤儿引用**（断言其存在，不掩盖）。
    /// 撞车面刻意都取非 NULL 列（`resource_bindings` 的 PK、`extension_assignments` 的
    /// `sub_agent_id`、两张表的 `(tool_id, extension_id)` PK）——工具级行的 NULL 维度
    /// 撞不上车，见下面 `null_subagent_dimension_does_not_collide`
    #[test]
    fn pathological_dual_rows_leave_or_ignored_orphans() {
        let conn = mem_conn();
        let now = chrono::Utc::now().to_rfc3339();
        seed_extension(&conn, r"skill-a\b", r"skill-a\b");
        seed_extension(&conn, "skill-a/b", "skill-a/b");
        // 旧 id 侧：五表各一行（每行都与新 id 侧的同键行相撞；不用 `seed_ref_rows`
        // 是因为它的工具级 assignment 行 `sub_agent_id IS NULL` 撞不上车，会多搬一行）
        conn.execute(
            "INSERT INTO preset_items (id, preset_id, extension_id, kind) \
             VALUES ('pi-old','p-1',?1,'skill')",
            params![r"skill-a\b"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO extension_assignments \
             (id, extension_id, agent_tool_id, sub_agent_id, enabled, link_status, assigned_at) \
             VALUES ('asg-old',?1,'claude','sub1',1,'valid',?2)",
            params![r"skill-a\b", now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO resource_bindings (extension_id, exclusive_tools, reason, updated_at) \
             VALUES (?1,'[\"claude\"]',NULL,?2)",
            params![r"skill-a\b", now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tool_residents (tool_id, extension_id) VALUES ('claude',?1)",
            params![r"skill-a\b"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tool_base_snapshot_items (tool_id, extension_id, kind, origin) \
             VALUES ('claude',?1,'skill','native')",
            params![r"skill-a\b"],
        )
        .unwrap();
        // 新 id 侧的同键引用行（唯一键/PK 都会与搬迁行相撞）
        conn.execute(
            "INSERT INTO extension_assignments \
             (id, extension_id, agent_tool_id, sub_agent_id, enabled, link_status, assigned_at) \
             VALUES ('asg-new','skill-a/b','claude','sub1',1,'valid',?1)",
            params![now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO resource_bindings (extension_id, exclusive_tools, reason, updated_at) \
             VALUES ('skill-a/b','[\"codex\"]',NULL,?1)",
            params![now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tool_residents (tool_id, extension_id) VALUES ('claude','skill-a/b')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tool_base_snapshot_items (tool_id, extension_id, kind, origin) \
             VALUES ('claude','skill-a/b','skill','mam')",
            [],
        )
        .unwrap();

        let moved =
            migrate_legacy_extension_id_on(&conn, r"skill-a\b", "skill-a/b", "skill-a/b").unwrap();
        assert!(moved);

        assert_eq!(
            all_ids(&conn),
            vec!["skill-a/b".to_string()],
            "病态双行合并为斜杠行"
        );
        // 五表的新 id 引用各 1 行：四张唯一键表靠 OR IGNORE 兼位（播种的新行保留），
        // preset_items 无唯一键 → 普通 UPDATE 全搬
        assert_refs(&conn, "skill-a/b", 1);

        // 被 OR IGNORE 跳过的那一行留在旧 id 上，如实登记为孤儿引用（评审要求：不掩盖）
        for (table, id_col, orphan_id) in [
            ("extension_assignments", "id", "asg-old"),
            ("resource_bindings", "extension_id", r"skill-a\b"),
            ("tool_residents", "extension_id", r"skill-a\b"),
            ("tool_base_snapshot_items", "extension_id", r"skill-a\b"),
        ] {
            let n: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE {id_col} = ?1"),
                    params![orphan_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                n, 1,
                "{table} 上被跳过的旧行引用必须如实存在（孤儿 {orphan_id}）"
            );
        }
        // preset_items 无唯一键 → 旧行照搬不孤儿，旧 id 上必须零引用
        let stale_preset: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM preset_items WHERE extension_id = ?1",
                params![r"skill-a\b"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stale_preset, 0, "preset_items 无唯一键，必须搬空");
        // 撞车行是「跳过」不是「删除」：四张唯一键表各两行（新行 + 孤儿）
        for table in [
            "extension_assignments",
            "resource_bindings",
            "tool_residents",
            "tool_base_snapshot_items",
        ] {
            let n: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 2, "{table} 撞车行被跳过而非删除（新行 + 孤儿）");
        }
    }

    /// SQLite 的 UNIQUE 索引把 NULL 视作**互不相同**（官方语义：所有 NULL 彼此不相等），
    /// 故 `extension_assignments` 的工具级行（`sub_agent_id IS NULL`）撞不上车：
    /// `OR IGNORE` 无事可做，两行都落在新 id（如实登记为同维度重复行，无孤儿）。
    /// 「兼位」只对非 NULL 维度成立——孤儿面由上面那个用例覆盖
    #[test]
    fn null_subagent_dimension_does_not_collide() {
        let conn = mem_conn();
        let now = chrono::Utc::now().to_rfc3339();
        seed_extension(&conn, r"skill-a\b", r"skill-a\b");
        seed_extension(&conn, "skill-a/b", "skill-a/b");
        for (id, ext_id) in [("asg-old", r"skill-a\b"), ("asg-new", "skill-a/b")] {
            conn.execute(
                "INSERT INTO extension_assignments \
                 (id, extension_id, agent_tool_id, sub_agent_id, enabled, link_status, assigned_at) \
                 VALUES (?1,?2,'claude',NULL,1,'valid',?3)",
                params![id, ext_id, now],
            )
            .unwrap();
        }

        assert!(
            migrate_legacy_extension_id_on(&conn, r"skill-a\b", "skill-a/b", "skill-a/b").unwrap()
        );

        let on_new: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM extension_assignments WHERE extension_id='skill-a/b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            on_new, 2,
            "NULL 维度不算撞车：两行都在新 id（重复行如实保留）"
        );
        let on_old: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM extension_assignments WHERE extension_id=?1",
                params![r"skill-a\b"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(on_old, 0, "没有孤儿：搬迁行没被跳过");
    }

    /// `preset_items` 对 extension_id **无**唯一约束 → 必须普通 UPDATE 全搬：
    /// 病态「同一预设同时含新旧两个 id」会留成重复条目（既有注释如实登记的取舍），
    /// 这里把该行为钉住，防评审后误改回 `OR IGNORE` 静默丢行
    #[test]
    fn preset_items_moves_duplicate_rows_verbatim() {
        let conn = mem_conn();
        seed_extension(&conn, r"skill-a\b", r"skill-a\b");
        seed_extension(&conn, "skill-a/b", "skill-a/b");
        conn.execute(
            "INSERT INTO preset_items (id, preset_id, extension_id, kind) \
             VALUES ('pi-old','p-1',?1,'skill')",
            params![r"skill-a\b"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO preset_items (id, preset_id, extension_id, kind) \
             VALUES ('pi-new','p-1','skill-a/b','skill')",
            [],
        )
        .unwrap();

        assert!(
            migrate_legacy_extension_id_on(&conn, r"skill-a\b", "skill-a/b", "skill-a/b").unwrap()
        );

        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM preset_items WHERE preset_id='p-1' AND extension_id='skill-a/b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2, "两条都搬到新 id（重复条目如实保留，不静默丢行）");
        let stale: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM preset_items WHERE extension_id=?1",
                params![r"skill-a\b"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stale, 0, "旧 id 引用必须搬空");
    }

    /// 廉价分支：`old_id == new_id` 直接 false（不碰库）；旧 id 不存在 → false（零副作用）
    #[test]
    fn identity_and_absent_old_id_return_false() {
        let conn = mem_conn();
        assert!(
            !migrate_legacy_extension_id_on(&conn, "skill-a/b", "skill-a/b", "skill-a/b").unwrap(),
            "old == new → Ok(false)"
        );

        seed_extension(&conn, "skill-a/b", "skill-a/b");
        assert!(
            !migrate_legacy_extension_id_on(&conn, r"skill-a\b", "skill-a/b", "skill-a/b").unwrap(),
            "旧 id 不存在 → Ok(false)"
        );
        assert_eq!(
            all_ids(&conn),
            vec!["skill-a/b".to_string()],
            "空跑不得改动既有行"
        );
    }

    /// DB 清单只取 `kind='skill'` 且 id 含反斜杠的行（LIKE 无 ESCAPE，`\` 即字面量）
    #[test]
    fn backslash_id_list_filters_kind_and_separator() {
        let conn = mem_conn();
        seed_extension(&conn, r"skill-a\b", r"skill-a\b");
        seed_extension(&conn, "skill-flat", "skill-flat");
        seed_extension(&conn, "skill-a/b", "skill-a/b");
        let now = chrono::Utc::now().to_rfc3339();
        // 非 skill 的反斜杠 id 不在搬迁面内（kind 过滤）
        conn.execute(
            "INSERT INTO extensions (id, kind, name, source_path, installed_at, updated_at) \
             VALUES (?1,'mcp','m','/tmp/tuvis-nowhere',?2,?2)",
            params![r"mcp-a\b", now],
        )
        .unwrap();

        assert_eq!(
            list_backslash_skill_ids_on(&conn),
            vec![r"skill-a\b".to_string()]
        );
    }

    /// 磁盘已删的存量行（评审跟进 3）：主行 `skill-gone\old` 在磁盘上**无对应 skill
    /// 目录**（source_path 不存在 ⇒ backfill_registry 的逐目录分支永不触达）——
    /// 仅凭 DB 清单（list + 逐行迁移）即完成迁移；再跑一次清单已空 → 0 行
    #[test]
    fn disk_deleted_legacy_row_migrates_from_db_list_alone() {
        let conn = mem_conn();
        let now = chrono::Utc::now().to_rfc3339();
        let gone = "/tmp/tuvis-nowhere/skills/gone/old";
        conn.execute(
            "INSERT INTO extensions (id, kind, name, source_path, installed_at, updated_at) \
             VALUES (?1,'skill',?1,?2,?3,?3)",
            params![r"skill-gone\old", gone, now],
        )
        .unwrap();
        assert!(
            !std::path::Path::new(gone).exists(),
            "前提：该路径在磁盘上不存在（模拟 skill 目录已删）"
        );

        assert_eq!(
            migrate_backslash_skill_ids_on(&conn),
            1,
            "仅凭 DB 清单迁移 1 行（不经任何磁盘扫描）"
        );
        assert_eq!(all_ids(&conn), vec!["skill-gone/old".to_string()]);
        let name: String = conn
            .query_row("SELECT name FROM extensions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            name, "gone/old",
            "name 取斜杠 rel（与 backfill_registry 逐目录分支同口径：去 `skill-` 前缀的相对路径）"
        );
        assert_eq!(
            migrate_backslash_skill_ids_on(&conn),
            0,
            "清单已空 → 再跑 0 行（与逐目录分支互为幂等）"
        );
    }
}
