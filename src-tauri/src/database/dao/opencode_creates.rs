use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::{
    inject::create_path::CreateDirectory,
    services::opencode_native::types::{RuntimeKey, ServiceDescriptor},
};
use sha2::{Digest, Sha256};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct CreateRecord {
    pub request_id: String,
    pub project: CreateDirectory,
    pub runtime: RuntimeKey,
    #[serde(default)]
    original_runtime: Option<RuntimeKey>,
    pub endpoint: String,
    pub version: String,
    credential_hash: [u8; 32],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_key: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub creation_verified: bool,
    pub session_id: Option<String>,
    pub window: CreateWindow,
    #[serde(default)]
    pub window_attempted: bool,
    #[serde(default)]
    pub confirmed_window: Option<crate::services::opencode_native::binding_gate::LockedWindow>,
    pub reason_code: Option<String>,
    pub reason: Option<String>,
    pub first_sentence: Option<String>,
    #[serde(default)]
    sentence_hash: Option<[u8; 32]>,
    pub first_action_id: String,
    pub first_sentence_status: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CreateWindow {
    Pending,
    Created,
    Opening,
    Opened,
    Failed,
    Blocked,
}

impl CreateRecord {
    pub fn new(
        device_id: &str,
        request_id: String,
        project: CreateDirectory,
        service: &ServiceDescriptor,
        first_sentence: Option<String>,
    ) -> Self {
        let digest = Sha256::digest(format!("{device_id}\0{request_id}").as_bytes());
        let action_key: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        Self {
            request_id,
            project,
            runtime: service.runtime.clone(),
            original_runtime: Some(service.runtime.clone()),
            endpoint: service.endpoint.to_string(),
            version: service.version.clone(),
            credential_hash: Sha256::digest(service.password.as_bytes()).into(),
            correlation_key: Some(crate::remote::random_hex_16()),
            creation_verified: false,
            session_id: None,
            window: CreateWindow::Pending,
            window_attempted: false,
            confirmed_window: None,
            reason_code: None,
            reason: None,
            first_sentence_status: first_sentence.as_ref().map(|_| "pending".to_owned()),
            sentence_hash: first_sentence
                .as_deref()
                .map(|text| Sha256::digest(text.as_bytes()).into()),
            first_sentence,
            first_action_id: format!("act_create_{action_key}"),
        }
    }

    pub(crate) fn has_verified_creation(&self) -> bool {
        self.session_id.is_some() && (self.creation_verified || self.window == CreateWindow::Opened)
    }

    pub(crate) fn expected_session_id(&self) -> Option<String> {
        self.correlation_key
            .as_ref()
            .map(|key| format!("ses_tuvis_{key}"))
    }

    pub fn matches_intent(&self, raw_directory: &str, first_sentence: Option<&str>) -> bool {
        let text = first_sentence
            .map(str::trim)
            .filter(|text| !text.is_empty());
        let digest: Option<[u8; 32]> = text.map(|text| Sha256::digest(text.as_bytes()).into());
        let directory_matches = raw_directory.trim() == self.project.path
            || crate::inject::create_path::validate_existing(raw_directory)
                .is_ok_and(|candidate| candidate == self.project);
        directory_matches && digest == self.sentence_hash
    }

    pub fn adopt_service(&mut self, service: &ServiceDescriptor) {
        self.original_runtime
            .get_or_insert_with(|| self.runtime.clone());
        self.runtime = service.runtime.clone();
        self.endpoint = service.endpoint.to_string();
        self.version = service.version.clone();
        self.credential_hash = Sha256::digest(service.password.as_bytes()).into();
        self.window = if self.window_attempted && self.window != CreateWindow::Failed {
            CreateWindow::Pending
        } else {
            CreateWindow::Created
        };
        self.confirmed_window = None;
        self.reason_code = None;
        self.reason = None;
    }

    pub(crate) fn matches_bound_window(
        &self,
        target: &crate::services::opencode_native::types::NativeTarget,
        window: &crate::services::opencode_native::binding_gate::LockedWindow,
    ) -> bool {
        self.window == CreateWindow::Opened
            && self.session_id.as_deref() == Some(target.session_id.as_str())
            && self.runtime == target.runtime
            && self.project.path == target.directory.to_string_lossy()
            && self.confirmed_window.as_ref() == Some(window)
    }

    pub fn matches_service(&self, service: &ServiceDescriptor) -> bool {
        let credential_hash: [u8; 32] = Sha256::digest(service.password.as_bytes()).into();
        self.runtime == service.runtime
            && self.endpoint == service.endpoint.as_str()
            && self.version == service.version
            && self.credential_hash == credential_hash
    }
}

fn encode(record: &CreateRecord) -> rusqlite::Result<String> {
    serde_json::to_string(record)
        .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))
}

pub(crate) fn get(
    conn: &Connection,
    device_id: &str,
    request_id: &str,
) -> rusqlite::Result<Option<CreateRecord>> {
    let json: Option<String> = conn
        .query_row(
            "SELECT record_json FROM opencode_native_creates WHERE device_id = ? AND request_id = ?",
            params![device_id, request_id],
            |row| row.get(0),
        )
        .optional()?;
    json.map(|json| {
        serde_json::from_str(&json).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(err))
        })
    })
    .transpose()
}

/// 在 mkdir 前固定请求意图，进程退出或创建回执尚未生成时也不能换目录重用 ID。
pub(crate) fn reserve_directory_intent(
    conn: &Connection,
    device_id: &str,
    request_id: &str,
    directory: &str,
    first_sentence: Option<&str>,
) -> rusqlite::Result<bool> {
    let digest = Sha256::digest(
        serde_json::to_vec(&(directory, first_sentence))
            .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?,
    );
    conn.execute(
        "INSERT INTO opencode_native_directory_intents (device_id, request_id, intent_sha256)
         VALUES (?1, ?2, ?3) ON CONFLICT(device_id, request_id) DO NOTHING",
        params![device_id, request_id, &digest[..]],
    )?;
    conn.query_row(
        "SELECT intent_sha256 = ?3 FROM opencode_native_directory_intents
         WHERE device_id = ?1 AND request_id = ?2",
        params![device_id, request_id, &digest[..]],
        |row| row.get(0),
    )
}

pub(crate) fn reserve(
    conn: &Connection,
    device_id: &str,
    record: &CreateRecord,
) -> rusqlite::Result<bool> {
    let json = encode(record)?;
    conn.execute(
        "INSERT INTO opencode_native_creates (device_id, request_id, record_json)
         VALUES (?, ?, ?) ON CONFLICT(device_id, request_id) DO NOTHING",
        params![device_id, record.request_id, json],
    )
    .map(|rows| rows == 1)
}

/// 整行比较防止迟到的恢复或首句观测覆盖更新状态。
pub(crate) fn update(
    conn: &Connection,
    device_id: &str,
    before: &CreateRecord,
    after: &CreateRecord,
) -> rusqlite::Result<bool> {
    let before_json = encode(before)?;
    let after_json = encode(after)?;
    conn.execute(
        "UPDATE opencode_native_creates SET session_id = ?, record_json = ?
         WHERE device_id = ? AND request_id = ? AND record_json = ?",
        params![
            after.session_id,
            after_json,
            device_id,
            before.request_id,
            before_json
        ],
    )
    .map(|rows| rows == 1)
}

/// 首句包括持久待发项都只允许取得同一窗口的派发权。
pub(crate) fn first_sentence_target(
    conn: &Connection,
    device_id: &str,
    action_id: &str,
) -> rusqlite::Result<Option<CreateRecord>> {
    let id: Option<String> = conn.query_row(
        "SELECT request_id FROM opencode_native_creates WHERE device_id = ? AND json_extract(record_json, '$.first_action_id') = ?",
        params![device_id, action_id],
        |row| row.get(0),
    ).optional()?;
    id.map(|request_id| get(conn, device_id, &request_id))
        .transpose()
        .map(Option::flatten)
}

/// 用完整属主与会话边界核对首句正式回执，避免读取或解析另一个设备的动作。
pub(crate) fn first_sentence_confirmed(
    conn: &Connection,
    device: &str,
    record: &CreateRecord,
) -> rusqlite::Result<bool> {
    let Some(session_id) = record.session_id.as_deref() else {
        return Ok(false);
    };
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM opencode_native_actions
          WHERE action_id = ?1 AND device_id = ?2 AND session_id = ?3
            AND directory = ?4 AND operation = 'send' AND state = 'confirmed'
            AND native_id = 'msg_' || ?1)",
        params![
            record.first_action_id,
            device,
            session_id,
            record.project.path
        ],
        |row| row.get(0),
    )
}

// 串联原生 ID 入账与通用窗口交付的最终授权。只在首次记入 ID 时取锁。
pub(crate) static WINDOW_DELIVERY: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn contains_session(conn: &Connection, session_id: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM opencode_native_creates WHERE session_id = ?)",
        [session_id],
        |row| row.get(0),
    )
}
