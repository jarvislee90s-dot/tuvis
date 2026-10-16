// 基底快照（spec §3.2/§4）：生命周期 = 一次预设会话；恢复默认时销毁
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::database::connection::DB;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BaseSnapshotItemRecord {
    pub extension_id: String,
    pub kind: String,
    pub origin: String, // "mam" | "native"
}

/// 幂等保存：先删该工具旧快照再插入（拍基底/重存同一路径）
pub fn save_base_snapshot(
    tool_id: &str,
    active_preset_id: Option<&str>,
    items: &[BaseSnapshotItemRecord],
) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    // 事务包裹删除+插入：保证基底账不出现半截态，中断/失败即整体回滚
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("DB_SNAPSHOT_SAVE_FAIL: 保存基底快照失败: {}", e))?;
    tx.execute(
        "DELETE FROM tool_base_snapshot_items WHERE tool_id = ?1",
        [tool_id],
    )
    .map_err(|e| format!("DB_SNAPSHOT_SAVE_FAIL: 保存基底快照失败: {}", e))?;
    tx.execute(
        "INSERT OR REPLACE INTO tool_base_snapshots (tool_id, active_preset_id, created_at) VALUES (?1, ?2, ?3)",
        params![tool_id, active_preset_id, now],
    )
    .map_err(|e| format!("DB_SNAPSHOT_SAVE_FAIL: 保存基底快照失败: {}", e))?;
    for it in items {
        tx.execute(
            "INSERT INTO tool_base_snapshot_items (tool_id, extension_id, kind, origin) VALUES (?1, ?2, ?3, ?4)",
            params![tool_id, it.extension_id, it.kind, it.origin],
        )
        .map_err(|e| format!("DB_SNAPSHOT_SAVE_FAIL: 保存基底快照失败: {}", e))?;
    }
    tx.commit()
        .map_err(|e| format!("DB_SNAPSHOT_SAVE_FAIL: 保存基底快照失败: {}", e))?;
    Ok(())
}

pub fn get_base_snapshot(tool_id: &str) -> Option<(Option<String>, Vec<BaseSnapshotItemRecord>)> {
    let conn = DB.lock().unwrap();
    let active: Option<String> = conn
        .query_row(
            "SELECT active_preset_id FROM tool_base_snapshots WHERE tool_id = ?1",
            [tool_id],
            |r| r.get(0),
        )
        .ok()?;
    let items = conn
        .prepare(
            "SELECT extension_id, kind, origin FROM tool_base_snapshot_items WHERE tool_id = ?1",
        )
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([tool_id], |row| {
                Ok(BaseSnapshotItemRecord {
                    extension_id: row.get(0)?,
                    kind: row.get(1)?,
                    origin: row.get(2)?,
                })
            })
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    Some((active, items))
}

pub fn set_active_preset(tool_id: &str, preset_id: &str) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    conn.execute(
        "UPDATE tool_base_snapshots SET active_preset_id = ?2 WHERE tool_id = ?1",
        params![tool_id, preset_id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn destroy_base_snapshot(tool_id: &str) -> Result<(), String> {
    let conn = DB.lock().unwrap();
    // 事务（评审裁决 4）：两删半截崩溃会留下「空 items 快照」，restore 会把
    // target 当空集把工具全停用——与 save 对称包事务
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("DB_SNAPSHOT_DESTROY_FAIL: 销毁基底快照失败: {}", e))?;
    tx.execute(
        "DELETE FROM tool_base_snapshot_items WHERE tool_id = ?1",
        [tool_id],
    )
    .map_err(|e| format!("DB_SNAPSHOT_DESTROY_FAIL: 销毁基底快照失败: {}", e))?;
    tx.execute(
        "DELETE FROM tool_base_snapshots WHERE tool_id = ?1",
        [tool_id],
    )
    .map_err(|e| format!("DB_SNAPSHOT_DESTROY_FAIL: 销毁基底快照失败: {}", e))?;
    tx.commit()
        .map_err(|e| format!("DB_SNAPSHOT_DESTROY_FAIL: 销毁基底快照失败: {}", e))?;
    Ok(())
}
