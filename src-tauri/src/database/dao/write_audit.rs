// 写审计表 DAO（M7）：移动端注入动作的只追加账本（谁在何时经哪个通道对哪个会话做了什么）
// action 词表：send | queue | flush | jump | retract | approve | reject | fail | key | open | answer | mode | slash | create | dialog
//（由调用方约束，本层不校验；answer = 批次乙 T8 问答应答；mode = 批次丙 T6 模式切换；
//  **slash = 丁T3 斜杠命令裸注入**——裁2：`/` 开头消息不带签名（前后缀都会破坏命令与
//  参数），终端不留痕是可接受的，溯源只此一处：本表 action=slash + device_name 列）
// Task 6（H4/H6）追加 **headless | headless_cancel**：无头 turn 落账与移动端取消
//（词表常量与落账出口单点在 `inject::headless`；channel 列记无头通道名而非终端注入器名）。
// **action 列是 TEXT NOT NULL、无 CHECK 约束**（见 database/schema.rs）——扩词只需改本
// 注释与调用方，**不需要 migration**。
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

/// 审计行（不含 id / device_id：对外展示只要时间戳与设备名）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditRow {
    pub ts: i64,
    pub device_name: String,
    pub agent_type: String,
    pub session_id: String,
    pub channel: String,
    pub action: String,
    pub summary: String,
    pub result: String,
}

/// 记录一条审计（只追加，不更新不删除）
#[allow(clippy::too_many_arguments)]
pub fn record_conn(
    conn: &Connection,
    ts: i64,
    device_id: &str,
    device_name: &str,
    agent_type: &str,
    session_id: &str,
    channel: &str,
    action: &str,
    summary: &str,
    result: &str,
) {
    if let Err(e) = conn.execute(
        "INSERT INTO write_audit (ts, device_id, device_name, agent_type, session_id, channel, action, summary, result) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![ts, device_id, device_name, agent_type, session_id, channel, action, summary, result],
    ) {
        log::error!("write_audit 记录失败: {e}");
    }
}

/// 最近 limit 条审计，最新在前（id 单调递增，按 id 倒序）
pub fn recent_conn(conn: &Connection, limit: i64) -> Vec<AuditRow> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT ts, device_name, agent_type, session_id, channel, action, summary, result FROM write_audit ORDER BY id DESC LIMIT ?1",
    ) else {
        log::error!("write_audit 查询失败（prepare）");
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([limit], |row| {
        Ok(AuditRow {
            ts: row.get(0)?,
            device_name: row.get(1)?,
            agent_type: row.get(2)?,
            session_id: row.get(3)?,
            channel: row.get(4)?,
            action: row.get(5)?,
            summary: row.get(6)?,
            result: row.get(7)?,
        })
    }) else {
        log::error!("write_audit 查询失败（query）");
        return Vec::new();
    };
    rows.filter_map(|r| r.ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn
    }

    /// 记录后按最新在前读出，字段往返一致
    #[test]
    fn record_and_recent_desc() {
        let c = mem();
        record_conn(
            &c,
            1000,
            "d1",
            "iPhone",
            "claude",
            "s1",
            "tmux",
            "send",
            "hi [mobile iPhone]",
            "ok",
        );
        record_conn(
            &c,
            1001,
            "d1",
            "iPhone",
            "codex",
            "s2",
            "tmux",
            "queue",
            "hi [mobile iPhone]",
            "ok",
        );
        let rows = recent_conn(&c, 10);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].ts, 1001); // 最新在前
        assert_eq!(rows[0].agent_type, "codex");
        assert_eq!(rows[0].session_id, "s2");
        assert_eq!(rows[0].device_name, "iPhone");
        assert_eq!(rows[0].channel, "tmux");
        assert_eq!(rows[0].action, "queue");
        assert_eq!(rows[0].summary, "hi [mobile iPhone]");
        assert_eq!(rows[0].result, "ok");
        assert_eq!(rows[1].ts, 1000);
        assert_eq!(rows[1].action, "send");
    }

    /// limit 截断：只留最新的 limit 条
    #[test]
    fn recent_limit_caps_rows() {
        let c = mem();
        for i in 0..5 {
            record_conn(
                &c,
                1000 + i,
                "d1",
                "iPhone",
                "claude",
                "s1",
                "tmux",
                "queue",
                "sum",
                "ok",
            );
        }
        let rows = recent_conn(&c, 3);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].ts, 1004); // 最新在前
        assert_eq!(rows[2].ts, 1002);
        // limit 为 0 / 空表均安全
        assert!(recent_conn(&c, 0).is_empty());
        let empty = mem();
        assert!(recent_conn(&empty, 10).is_empty());
    }
}
