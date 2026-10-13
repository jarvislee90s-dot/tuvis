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

/// 旧平台分隔符（Windows 反斜杠）skill id **就地迁移**为斜杠口径（backfill_registry 专用）：
/// spec §8.1 约定 id/name 均带斜杠路径，2026-10-13 前的 Windows 机器上嵌套套件技能
/// 登记出了 `skill-a\b` 形态的 id。schema 无外键约束（先例：引用表都是裸 TEXT 关联），
/// 引用搬迁必须显式做——搬迁面 = 引用 extension_id 的**全部五张表**（2026-10-13 评审
/// P1 补齐 `extension_assignments` / `tool_base_snapshot_items`，漏搬 = 技能启用态失联）。
/// 迁移顺序 = 先搬引用再改主行；主行改 id 失败时引用已指向新 id、旧行下次启动重试收敛。
/// 目标 id 已存在（病态双行）时按「斜杠行为准」合并：引用搬完删旧行。
/// 全程单事务（中途失败整体回滚，下次启动重跑）；返回是否发生了迁移。
pub fn migrate_legacy_extension_id(
    old_id: &str,
    new_id: &str,
    new_name: &str,
) -> Result<bool, String> {
    if old_id == new_id {
        return Ok(false);
    }
    let mut conn = DB.lock().unwrap();
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
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    // 引用搬迁。`extension_assignments` / `resource_bindings` / `tool_residents` /
    // `tool_base_snapshot_items` 都有含 extension_id 的唯一键（UNIQUE/PK）→ `OR IGNORE`
    // 兼位「新 id 已有同引用行」的冲突，被跳过的行成为孤儿引用（仅病态双行情形，
    // 如实登记不掩盖）；`preset_items` 对 extension_id **无**唯一约束（唯一键只有行
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
