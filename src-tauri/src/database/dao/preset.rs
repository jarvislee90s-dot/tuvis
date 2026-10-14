use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::database::connection::DB;

/// 预设 id 进程序列号：时间戳只有毫秒粒度，同毫秒连续创建会撞 UNIQUE（实测 flake），追加序列保证唯一
static PRESET_ID_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetRecord {
    pub id: String,
    pub name: String,
    pub description: String,
    pub scope: String, // "universal" | "tool"
    pub bound_tool: Option<String>,
    pub items: Vec<PresetItemRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetItemRecord {
    pub extension_id: String,
    pub kind: String,
    pub extension_name: String,
}

pub fn create_preset_with_meta(
    name: &str,
    description: &str,
    scope: &str,
    bound_tool: Option<&str>,
    items: &[(String, String)],
) -> Result<String, String> {
    let conn = DB.lock().unwrap();
    let seq = PRESET_ID_SEQ.fetch_add(1, Ordering::Relaxed);
    let id = format!("preset-{}-{}", chrono::Utc::now().timestamp_millis(), seq);
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO presets (id, name, description, scope, bound_tool, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, name, description, scope, bound_tool, now],
    )
    .map_err(|e| e.to_string())?;
    insert_items(&conn, &id, items)?;
    Ok(id)
}

/// 旧入口委托：默认通用预设（前端 M2 前的兼容层）
pub fn create_preset(name: &str, items: &[(String, String)]) -> Result<String, String> {
    create_preset_with_meta(name, "", "universal", None, items)
}

fn insert_items(
    conn: &rusqlite::Connection,
    id: &str,
    items: &[(String, String)],
) -> Result<(), String> {
    for (ext_id, kind) in items {
        let item_id = format!("{}-{}", id, ext_id);
        conn.execute(
            "INSERT INTO preset_items (id, preset_id, extension_id, kind) VALUES (?1, ?2, ?3, ?4)",
            params![item_id, id, ext_id, kind],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn update_preset(
    id: &str,
    name: &str,
    description: &str,
    scope: &str,
    bound_tool: Option<&str>,
    items: &[(String, String)],
) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    // 事务（评审裁决 4）：UPDATE→清 items→重插 的中途失败会留下半截预设
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("DB_PRESET_UPDATE_FAIL: 更新预设失败: {}", e))?;
    let n = tx
        .execute(
            "UPDATE presets SET name=?2, description=?3, scope=?4, bound_tool=?5 WHERE id=?1",
            params![id, name, description, scope, bound_tool],
        )
        .map_err(|e| format!("DB_PRESET_UPDATE_FAIL: 更新预设失败: {}", e))?;
    if n == 0 {
        return Err(format!("DB_PRESET_MISSING:{}: 预设 {} 不存在", id, id));
    }
    tx.execute("DELETE FROM preset_items WHERE preset_id = ?1", [id])
        .map_err(|e| format!("DB_PRESET_UPDATE_FAIL: 更新预设失败: {}", e))?;
    insert_items(&tx, id, items)?;
    tx.commit()
        .map_err(|e| format!("DB_PRESET_UPDATE_FAIL: 更新预设失败: {}", e))?;
    Ok(())
}

pub fn get_preset(preset_id: &str) -> Option<PresetRecord> {
    let conn = DB.lock().unwrap();
    conn.query_row(
        "SELECT id, name, description, scope, bound_tool FROM presets WHERE id = ?1",
        [preset_id],
        |row| {
            Ok(PresetRecord {
                id: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                scope: row.get(3)?,
                bound_tool: row.get(4)?,
                items: Vec::new(),
            })
        },
    )
    .ok()
    .map(|mut p| {
        p.items = load_items(&conn, &p.id);
        p
    })
}

pub fn list_presets() -> Vec<PresetRecord> {
    let conn = DB.lock().unwrap();
    let presets: Vec<PresetRecord> = conn
        .prepare(
            "SELECT id, name, description, scope, bound_tool FROM presets ORDER BY created_at DESC",
        )
        .ok()
        .map(|mut stmt| {
            stmt.query_map([], |row| {
                Ok(PresetRecord {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    description: row.get(2)?,
                    scope: row.get(3)?,
                    bound_tool: row.get(4)?,
                    items: Vec::new(),
                })
            })
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        })
        .unwrap_or_default();

    presets
        .into_iter()
        .map(|mut p| {
            p.items = load_items(&conn, &p.id);
            p
        })
        .collect()
}

fn load_items(conn: &rusqlite::Connection, id: &str) -> Vec<PresetItemRecord> {
    conn.prepare(
        "SELECT pi.extension_id, pi.kind, e.name FROM preset_items pi LEFT JOIN extensions e ON pi.extension_id = e.id WHERE pi.preset_id = ?1",
    )
    .ok()
    .and_then(|mut stmt| {
        stmt.query_map([id], |row| {
            Ok(PresetItemRecord {
                extension_id: row.get(0)?,
                kind: row.get(1)?,
                extension_name: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            })
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
    })
    .unwrap_or_default()
}

pub fn delete_preset(preset_id: &str) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    // 事务（评审裁决 4）：三条 DELETE 半截失败会留下孤儿 items/applications
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("DB_PRESET_DELETE_FAIL: 删除预设失败: {}", e))?;
    tx.execute("DELETE FROM preset_items WHERE preset_id = ?1", [preset_id])
        .map_err(|e| format!("DB_PRESET_DELETE_FAIL: 删除预设失败: {}", e))?;
    tx.execute(
        "DELETE FROM preset_applications WHERE preset_id = ?1",
        [preset_id],
    )
    .map_err(|e| format!("DB_PRESET_DELETE_FAIL: 删除预设失败: {}", e))?;
    tx.execute("DELETE FROM presets WHERE id = ?1", [preset_id])
        .map_err(|e| format!("DB_PRESET_DELETE_FAIL: 删除预设失败: {}", e))?;
    tx.commit()
        .map_err(|e| format!("DB_PRESET_DELETE_FAIL: 删除预设失败: {}", e))?;
    Ok(())
}

pub fn get_preset_items(preset_id: &str) -> Vec<(String, String)> {
    let conn = DB.lock().unwrap();
    conn.prepare("SELECT extension_id, kind FROM preset_items WHERE preset_id = ?1")
        .ok()
        .map(|mut stmt| {
            stmt.query_map([preset_id], |row| Ok((row.get(0)?, row.get(1)?)))
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

pub fn record_preset_application(
    preset_id: &str,
    tool_id: &str,
    active: bool,
) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    let id = format!("{}-{}", preset_id, tool_id);
    conn.execute(
        "INSERT OR REPLACE INTO preset_applications (id, preset_id, agent_tool_id, applied_at, active) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, preset_id, tool_id, now, active as i64],
    ).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn record_preset_application_subagent(
    preset_id: &str,
    tool_id: &str,
    sub_agent_id: &str,
    active: bool,
) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    let id = format!("{}-{}-{}", preset_id, tool_id, sub_agent_id);
    conn.execute(
        "INSERT OR REPLACE INTO preset_applications (id, preset_id, agent_tool_id, sub_agent_id, applied_at, active) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, preset_id, tool_id, sub_agent_id, now, active as i64],
    ).map_err(|e| e.to_string())?;
    Ok(())
}
