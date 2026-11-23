use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::services::opencode_native::binding_gate::LockedWindow;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Committed {
    pub binding_token: String,
    pub route_version: i64,
    pub window: LockedWindow,
}

pub(crate) enum Receipt {
    InProgress,
    NotApplied,
    Committed(Committed),
}

pub(crate) fn reserve(
    conn: &Connection,
    device: &str,
    session: &str,
    request: &str,
) -> rusqlite::Result<bool> {
    conn.execute(
        "INSERT INTO opencode_native_reselections (device_id, session_id, request_id, state) VALUES (?, ?, ?, 'in_progress') ON CONFLICT DO NOTHING",
        params![device, session, request],
    ).map(|count| count == 1)
}

pub(crate) fn get(
    conn: &Connection,
    device: &str,
    session: &str,
    request: &str,
) -> rusqlite::Result<Option<Receipt>> {
    let row: Option<(String, Option<String>)> = conn.query_row(
        "SELECT state, receipt FROM opencode_native_reselections WHERE device_id = ? AND session_id = ? AND request_id = ?",
        params![device, session, request],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
    match row {
        None => Ok(None),
        Some((state, _)) if state == "in_progress" => Ok(Some(Receipt::InProgress)),
        Some((state, _)) if state == "not_applied" => Ok(Some(Receipt::NotApplied)),
        Some((state, Some(json))) if state == "committed" => serde_json::from_str(&json)
            .map(|receipt| Some(Receipt::Committed(receipt)))
            .map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    Box::new(err),
                )
            }),
        Some(_) => Err(rusqlite::Error::InvalidQuery),
    }
}

pub(crate) fn finish(
    conn: &Connection,
    device: &str,
    session: &str,
    request: &str,
    committed: Option<&Committed>,
) -> rusqlite::Result<bool> {
    let receipt = committed
        .map(serde_json::to_string)
        .transpose()
        .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?;
    let state = if committed.is_some() {
        "committed"
    } else {
        "not_applied"
    };
    conn.execute(
        "UPDATE opencode_native_reselections SET state = ?, receipt = ? WHERE device_id = ? AND session_id = ? AND request_id = ? AND state = 'in_progress'",
        params![state, receipt, device, session, request],
    ).map(|count| count == 1)
}
