//! ZCode 官方桥动作的写审计（生产计划 T3）。
//!
//! 复用既有 `write_audit` 表与 [`record_conn_checked`]（与动作预留同事务；审计失败
//! ⇒ 动作整体回滚、零投递）。纪律：
//!
//! - `channel` 固定 `zcode_official`（与终端注入、codex 通道区分）；
//! - `summary` **只**含动作类别与字数／选项数量，绝不摘正文、答案、JWT、URL；
//! - 动作 id 只进 `action` 词干（`zcode_send` 等），不拼接用户内容。

use rusqlite::Connection;

use crate::database::dao::write_audit;

/// 审计通道名（固定；用户可见账本据此区分官方 relay 写入）。
pub const CHANNEL: &str = "zcode_official";

/// 审计 `agent_type` 列（官方桥属于 ZCode 工具域）。
pub const AGENT: &str = "zcode";

/// 审计 `action` 列词干前缀（`zcode_send` / `zcode_answer` / …）。
const ACTION_PREFIX: &str = "zcode_";

/// 记录一条「动作已预留、即将写出」的起始审计（**调用方必须把它放进预留事务**：
/// 返回 `Err` 时事务回滚，动作零写出）。
///
/// `summary` 由调用方按 [`super::action::audit_summary`] 生成（kind+计数，无正文）。
pub fn record_start_conn(
    conn: &Connection,
    ts: i64,
    device_id: &str,
    device_name: &str,
    session_id: &str,
    kind: &str,
    summary: &str,
) -> rusqlite::Result<()> {
    let action = format!("{ACTION_PREFIX}{kind}");
    write_audit::record_conn_checked(
        conn,
        ts,
        device_id,
        device_name,
        AGENT,
        session_id,
        CHANNEL,
        &action,
        summary,
        "started",
    )
}
