use serde::{Deserialize, Serialize};

use crate::database::connection::DB;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubAgentRecord {
    pub id: String,
    pub name: String,
    pub agent_tool_id: String,
    pub config_path: String,
    pub format: String,
}

pub fn list_sub_agents(tool_id: &str) -> Vec<SubAgentRecord> {
    let conn = DB.lock().unwrap();
    conn.prepare("SELECT id, name, agent_tool_id, config_path, format FROM sub_agents WHERE agent_tool_id = ?1")
        .ok()
        .map(|mut stmt| {
            stmt.query_map([tool_id], |row| {
                Ok(SubAgentRecord {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    agent_tool_id: row.get(2)?,
                    config_path: row.get(3)?,
                    format: row.get(4)?,
                })
            })
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        })
        .unwrap_or_default()
}

// ===== 工具启用状态（spec W5：勾选管理） =====

/// 种子行：全部注册工具 enabled=1（INSERT OR IGNORE 幂等，应用启动时调用；
/// 已有行不受影响，缺行默认启用 → 老用户升级零感知）。
/// F3：返回 Result——启动路径（有界初始化内）需要种子失败上浮为结构化失败；
/// 运行时兜底路径（set_tool_enabled 等）照旧吞错（`let _ =`）。
pub fn ensure_tool_rows_conn(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    for id in crate::adapter::TOOL_IDS {
        if let Some(adapter) = crate::adapter::adapter_by_id(id) {
            conn.execute(
                "INSERT OR IGNORE INTO agent_tools (id, name, process_name, base_dir, hook_supported, mcp_format, enabled)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
                rusqlite::params![
                    id,
                    adapter.name(),
                    // 防御空切片越界：空串不会匹配任何进程（INSERT OR IGNORE 语义不变）
                    adapter.process_names().first().copied().unwrap_or(""),
                    adapter.base_dir().to_string_lossy(),
                    adapter.hook_supported() as i64,
                    format!("{:?}", adapter.mcp_format()).to_lowercase(),
                ],
            )?;
        }
    }
    Ok(())
}

/// 单工具是否启用（行缺失视为启用，防御旧库/被删行）
pub fn get_tool_enabled_conn(conn: &rusqlite::Connection, tool_id: &str) -> bool {
    conn.query_row(
        "SELECT enabled FROM agent_tools WHERE id = ?1",
        [tool_id],
        |row| row.get::<_, i64>(0),
    )
    .map(|v| v != 0)
    .unwrap_or(true)
}

/// 启用工具的 id 列表（按种子顺序）
pub fn enabled_tool_ids_conn(conn: &rusqlite::Connection) -> Vec<String> {
    let Ok(mut stmt) = conn.prepare("SELECT id FROM agent_tools WHERE enabled = 1 ORDER BY rowid")
    else {
        return Vec::new();
    };
    stmt.query_map([], |row| row.get::<_, String>(0))
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

/// 写入启用状态（行不存在时先种子，防御旧库）
pub fn set_tool_enabled_conn(conn: &rusqlite::Connection, tool_id: &str, enabled: bool) {
    // 运行时兜底路径照旧吞错（种子失败不改变本函数的容错语义）
    let _ = ensure_tool_rows_conn(conn);
    let _ = conn.execute(
        "UPDATE agent_tools SET enabled = ?2 WHERE id = ?1",
        rusqlite::params![tool_id, enabled as i64],
    );
}

// ---- 全局连接包装（业务侧零锁代码，风格同 unread DAO） ----

pub fn ensure_tool_rows() {
    let conn = DB.lock().unwrap();
    // 运行时兜底路径照旧吞错（启动路径的错误上浮走有界初始化内的 conn 变体）
    let _ = ensure_tool_rows_conn(&conn);
}

pub fn get_tool_enabled(tool_id: &str) -> bool {
    let conn = DB.lock().unwrap();
    get_tool_enabled_conn(&conn, tool_id)
}

pub fn enabled_tool_ids() -> Vec<String> {
    let conn = DB.lock().unwrap();
    enabled_tool_ids_conn(&conn)
}

pub fn set_tool_enabled(tool_id: &str, enabled: bool) {
    let conn = DB.lock().unwrap();
    set_tool_enabled_conn(&conn, tool_id, enabled);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn).unwrap();
        conn
    }

    #[test]
    fn enabled_tool_ids_seeds_and_filters() {
        let conn = mem_conn();
        ensure_tool_rows_conn(&conn).expect("种子工具行写入失败");
        // 默认全部启用（老用户升级零感知）
        assert_eq!(
            enabled_tool_ids_conn(&conn).len(),
            crate::adapter::TOOL_IDS.len()
        );
        set_tool_enabled_conn(&conn, "workbuddy", false);
        assert!(!get_tool_enabled_conn(&conn, "workbuddy"));
        let ids = enabled_tool_ids_conn(&conn);
        assert!(!ids.contains(&"workbuddy".to_string()));
        assert!(ids.contains(&"claude".to_string()));
    }
}
