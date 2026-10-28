//! ZCode 能力证据账本（spec R6；生产计划 T2）：`zcode_capability_evidence` 表的读写。
//!
//! 每个 `(key, capability)` 一行，upsert 覆盖不追加；读失败/缺表返回空账本
//! （能力账本是优化面，绝不阻塞连接、绝不 panic）。写入侧带受限校验硬闸：
//! `evidence`/`reason_code` 只允许结构化判据标识，正文/凭据/链接一律拒绝落库。
#![cfg_attr(not(test), allow(dead_code))]

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};

use crate::remote::pairing::DeviceStore;
use crate::services::zcode_control::capability::{
    Capability, CapabilityKey, CapabilityRecord, CapabilityState,
};

/// 判据标识整体最大长度：判据名/状态码组合远短于此，正文/JWT 通常远长于此。
const EVIDENCE_MAX_LEN: usize = 256;
/// 单个判据段（按 `+`/`.`/`-` 分隔）最大长度：再长即疑似 base64/JWT 片段。
const EVIDENCE_SEGMENT_MAX_LEN: usize = 40;

/// 账本存取口（T4 resolver 依赖的最小面；测试可注入内存实现）。
pub(crate) trait CapabilityLedger: Send + Sync {
    /// 读一个 key 下的全部能力记录（≤8 行，按 capability 名稳定排序）；
    /// 读失败/缺表 → 空 Vec（不 panic——账本是优化面，不阻塞连接）。
    fn get(&self, key: &CapabilityKey) -> Vec<CapabilityRecord>;

    /// upsert 一条记录；写失败（含 evidence/reason_code 未过受限校验）→ Err(String)。
    fn put(&self, key: &CapabilityKey, record: &CapabilityRecord) -> Result<(), String>;
}

/// SQLite 账本实现（`zcode_capability_evidence` 表，spec R6）。
///
/// `DeviceStore` 注入缝与远程域惯例一致：生产 `Global`（真实 `~/.tuvis/tuvis.db`）、
/// 测试 `memory()`（init+migrate 全套表）与裸 `Owned`（缺表场景的降级路径）。
pub(crate) struct DbCapabilityLedger {
    store: DeviceStore,
}

impl DbCapabilityLedger {
    /// 注入存储（`DeviceStore` 可 clone 共享同一连接，不产生第二个库）。
    pub(crate) fn new(store: DeviceStore) -> Self {
        Self { store }
    }
}

impl CapabilityLedger for DbCapabilityLedger {
    fn get(&self, key: &CapabilityKey) -> Vec<CapabilityRecord> {
        let storage_key = key.to_storage_key();
        self.store
            .with(|conn| get_conn(conn, &storage_key))
            .unwrap_or_default()
    }

    fn put(&self, key: &CapabilityKey, record: &CapabilityRecord) -> Result<(), String> {
        // 写入侧硬闸（spec R6）：两个自由形状列都只收结构化判据标识，违例整条拒绝
        // （不剥离降级写入——调用方拿到 Err 后自行净化重试，状态信息宁缺毋滥）
        if let Some(evidence) = record.evidence.as_deref() {
            validate_evidence_token(evidence).map_err(|e| {
                format!(
                    "EVIDENCE_REJECTED: capability={} evidence 未过受限校验: {e}",
                    record.capability.as_str()
                )
            })?;
        }
        if let Some(reason) = record.reason_code.as_deref() {
            validate_evidence_token(reason).map_err(|e| {
                format!(
                    "REASON_CODE_REJECTED: capability={} reason_code 未过受限校验: {e}",
                    record.capability.as_str()
                )
            })?;
        }
        let storage_key = key.to_storage_key();
        self.store.with(|conn| put_conn(conn, &storage_key, record))
    }
}

/// 读一个 storage key 的全部行（capability 名稳定排序）；表缺失/查询失败 → Err
/// （由 trait 实现侧折成空 Vec）。
fn get_conn(conn: &Connection, storage_key: &str) -> Result<Vec<CapabilityRecord>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT capability, state, reason_code, evidence
             FROM zcode_capability_evidence WHERE key = ?1 ORDER BY capability",
        )
        .map_err(|e| format!("CAPABILITY_LEDGER_READ_FAIL: {e}"))?;
    let rows = stmt
        .query_map(params![storage_key], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|e| format!("CAPABILITY_LEDGER_READ_FAIL: {e}"))?;
    let mut records = Vec::new();
    for row in rows {
        let (capability, state, reason_code, evidence) =
            row.map_err(|e| format!("CAPABILITY_LEDGER_READ_FAIL: {e}"))?;
        // 只认冻结词表：capability/state 任一解析不出 → 跳过该行（不猜语义、不 panic）
        let (Some(capability), Some(state)) =
            (capability_from_str(&capability), state_from_str(&state))
        else {
            continue;
        };
        records.push(CapabilityRecord {
            capability,
            state,
            reason_code,
            evidence,
        });
    }
    Ok(records)
}

/// 单行 upsert（`ON CONFLICT(key, capability) DO UPDATE` 覆盖，不追加）。
fn put_conn(conn: &Connection, storage_key: &str, record: &CapabilityRecord) -> Result<(), String> {
    conn.execute(
        "INSERT INTO zcode_capability_evidence
            (key, capability, state, reason_code, evidence, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(key, capability) DO UPDATE SET
            state       = excluded.state,
            reason_code = excluded.reason_code,
            evidence    = excluded.evidence,
            updated_at  = excluded.updated_at",
        params![
            storage_key,
            record.capability.as_str(),
            state_to_str(record.state),
            record.reason_code,
            record.evidence,
            now_ms(),
        ],
    )
    .map_err(|e| format!("CAPABILITY_LEDGER_WRITE_FAIL: {e}"))?;
    Ok(())
}

/// state 落库词（spec R6：小写冻结词表；capability 列直接用 `Capability::as_str`）。
fn state_to_str(state: CapabilityState) -> &'static str {
    match state {
        CapabilityState::Unsupported => "unsupported",
        CapabilityState::InterfaceMatched => "interface_matched",
        CapabilityState::Confirmed => "confirmed",
        CapabilityState::Degraded => "degraded",
        CapabilityState::Inconclusive => "inconclusive",
    }
}

/// 解析 state 落库词；未知词 → None（读侧跳过该行，前向兼容不猜语义）。
fn state_from_str(raw: &str) -> Option<CapabilityState> {
    match raw {
        "unsupported" => Some(CapabilityState::Unsupported),
        "interface_matched" => Some(CapabilityState::InterfaceMatched),
        "confirmed" => Some(CapabilityState::Confirmed),
        "degraded" => Some(CapabilityState::Degraded),
        "inconclusive" => Some(CapabilityState::Inconclusive),
        _ => None,
    }
}

/// 解析 capability 冻结词（`Capability::as_str` 的逆映射）；未知词 → None。
fn capability_from_str(raw: &str) -> Option<Capability> {
    match raw {
        "send" => Some(Capability::Send),
        "single_select" => Some(Capability::SingleSelect),
        "multi_select" => Some(Capability::MultiSelect),
        "free_text" => Some(Capability::FreeText),
        "interrupt" => Some(Capability::Interrupt),
        "approval_reply" => Some(Capability::ApprovalReply),
        "model" => Some(Capability::Model),
        "thought_level" => Some(Capability::ThoughtLevel),
        _ => None,
    }
}

/// 写入侧受限校验硬闸（spec R6）：`evidence`/`reason_code` 只允许**结构化判据
/// 标识**——判据名、状态码、`+` 连接的判据组合（如 `revision_advanced+new_user_message`）。
/// 违例形状一律 Err，整条 put 拒绝落库：
/// * 字符集白名单 `[A-Za-z0-9_.+-]`——空格/引号/括号/CJK 直接出局（正文、句子、JSON）；
/// * `:` 与 `/` 不在白名单 → URL/链接形状出局；
/// * 单段（`+`/`.`/`-` 分隔）长度上限 + `eyJ` 前缀黑名单 → JWT/base64 长块出局
///   （JWT 各段是长 base64 块，且 header 段恒以 `eyJ`（`{"` 的 base64）起头）。
fn validate_evidence_token(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("判据标识为空".to_owned());
    }
    if value.len() > EVIDENCE_MAX_LEN {
        return Err(format!(
            "判据标识超长（{}/{}）",
            value.len(),
            EVIDENCE_MAX_LEN
        ));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'+' | b'.' | b'-'))
    {
        return Err(format!(
            "判据标识含白名单外字符（只允许 [A-Za-z0-9_.+-]）: {value:?}"
        ));
    }
    for segment in value.split(['+', '.', '-']) {
        if segment.len() > EVIDENCE_SEGMENT_MAX_LEN {
            return Err(format!(
                "判据段超长（>{}，疑似 base64/JWT 片段）: {segment:?}",
                EVIDENCE_SEGMENT_MAX_LEN
            ));
        }
        if segment.starts_with("eyJ") {
            return Err("判据段含疑似 JWT 片段（eyJ 前缀）".to_owned());
        }
    }
    Ok(())
}

/// 当前 Unix 毫秒（INTEGER 时间列惯例，见 codex_ipc_probe；不依赖本地时区；
/// 时钟异常取 0，只影响 updated_at 的排序价值，不影响账本正确性）。
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::remote::pairing::DeviceStore;
    use crate::services::zcode_control::capability::{
        Capability, CapabilityKey, CapabilityRecord, CapabilityState,
    };

    use super::*;

    /// brief Step 1 的固定 key 助手：已知 build + 测试指纹。
    fn caps_key() -> CapabilityKey {
        CapabilityKey::new("macos", "3.16.2.8769", 3, "shape-test", 1)
    }

    /// brief Step 1 逐字用例：upsert 覆盖同一 key+capability，不产生多行。
    #[test]
    fn upsert_is_idempotent_and_overwrites_state() {
        let store = DeviceStore::memory();
        let ledger = DbCapabilityLedger::new(store.clone());
        let key = caps_key();
        ledger
            .put(&key, &CapabilityRecord::confirmed(Capability::Send))
            .unwrap();
        ledger
            .put(
                &key,
                &CapabilityRecord::degraded(Capability::Send, "shape_drift".to_owned()),
            )
            .unwrap();
        let rows = ledger.get(&key);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, CapabilityState::Degraded);
    }

    /// brief Step 1 逐字用例：缺表/裸连接读账本返回空 Vec，不 panic。
    #[test]
    fn missing_table_is_empty_not_panic() {
        let broken = DeviceStore::Owned(Arc::new(Mutex::new(
            rusqlite::Connection::open_in_memory().unwrap(),
        )));
        assert!(DbCapabilityLedger::new(broken).get(&caps_key()).is_empty());
    }

    /// 验收「migration 幂等，重复启动不报错」：schema::init + migrate 幂等重放
    /// （生产重启 = 同一批 IF NOT EXISTS DDL 再跑一遍）后既有行保留、读写照常。
    #[test]
    fn schema_and_migration_replay_is_idempotent() {
        let store = DeviceStore::memory();
        let ledger = DbCapabilityLedger::new(store.clone());
        let key = caps_key();
        ledger
            .put(&key, &CapabilityRecord::confirmed(Capability::Model))
            .unwrap();
        // 模拟下一次启动：init + migrate 幂等重放
        store
            .with(|conn| {
                crate::database::schema::init(conn);
                crate::database::migration::migrate(conn)
            })
            .unwrap();
        let rows = ledger.get(&key);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, CapabilityState::Confirmed);
        // 重放后仍可写
        ledger
            .put(
                &key,
                &CapabilityRecord::degraded(Capability::Model, "action_failed".to_owned()),
            )
            .unwrap();
        assert_eq!(ledger.get(&key)[0].state, CapabilityState::Degraded);
    }

    /// 验收「key 变化不命中旧记录」：五元组任一变化即另一份账本，互不串行。
    #[test]
    fn key_change_does_not_hit_old_records() {
        let ledger = DbCapabilityLedger::new(DeviceStore::memory());
        let old = caps_key();
        ledger
            .put(&old, &CapabilityRecord::confirmed(Capability::Send))
            .unwrap();
        let new_build = CapabilityKey::new("macos", "3.16.2.9999", 3, "shape-test", 1);
        assert!(
            ledger.get(&new_build).is_empty(),
            "build 变化后不得命中旧记录"
        );
        ledger
            .put(
                &new_build,
                &CapabilityRecord::degraded(Capability::Send, "shape_drift".to_owned()),
            )
            .unwrap();
        // 两份账本互不干扰：旧 key 仍是旧状态，新 key 是新状态
        assert_eq!(ledger.get(&old)[0].state, CapabilityState::Confirmed);
        assert_eq!(ledger.get(&new_build)[0].state, CapabilityState::Degraded);
    }

    /// 八项能力全字段落库往返：state/reason_code/evidence 经 SQLite 后不变形
    /// （同时钉住 capability/state 落库词表与 as_str 的互逆关系）。
    #[test]
    fn all_capabilities_round_trip_with_reason_and_evidence() {
        let ledger = DbCapabilityLedger::new(DeviceStore::memory());
        let key = caps_key();
        let capabilities = [
            Capability::Send,
            Capability::SingleSelect,
            Capability::MultiSelect,
            Capability::FreeText,
            Capability::Interrupt,
            Capability::ApprovalReply,
            Capability::Model,
            Capability::ThoughtLevel,
        ];
        for (i, capability) in capabilities.iter().enumerate() {
            let record = CapabilityRecord {
                capability: *capability,
                state: if i % 2 == 0 {
                    CapabilityState::Inconclusive
                } else {
                    CapabilityState::Degraded
                },
                reason_code: Some("timeout_no_ack".to_owned()),
                evidence: Some("revision_advanced+new_user_message".to_owned()),
            };
            ledger.put(&key, &record).unwrap();
        }
        let rows = ledger.get(&key);
        assert_eq!(rows.len(), 8, "一个 key 下八项能力各一行");
        for (i, capability) in capabilities.iter().enumerate() {
            let row = rows
                .iter()
                .find(|r| r.capability == *capability)
                .unwrap_or_else(|| panic!("缺 {} 行", capability.as_str()));
            let expected = if i % 2 == 0 {
                CapabilityState::Inconclusive
            } else {
                CapabilityState::Degraded
            };
            assert_eq!(row.state, expected);
            assert_eq!(row.reason_code.as_deref(), Some("timeout_no_ack"));
            assert_eq!(
                row.evidence.as_deref(),
                Some("revision_advanced+new_user_message")
            );
        }
    }

    /// spec R6 写入侧硬闸：正文/题目/选项/答案、JWT、链接形状的 evidence 一律
    /// 拒绝落库（Err），且拒绝时**不写任何行**；合法判据形状正常通过。
    #[test]
    fn evidence_with_body_or_credentials_is_rejected() {
        let ledger = DbCapabilityLedger::new(DeviceStore::memory());
        let key = caps_key();
        let rejected: Vec<(&str, &str)> = vec![
            (
                "jwt",
                "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2Q",
            ),
            ("中文正文", "用户把第 2 题的答案改成了 B"),
            ("带空格正文", "the user picked option B"),
            ("链接", "https://example.com/path?q=1"),
            ("空串", ""),
            (
                "超长片段",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
        ];
        for (label, evidence) in rejected {
            let record = CapabilityRecord {
                capability: Capability::Send,
                state: CapabilityState::Confirmed,
                reason_code: None,
                evidence: Some(evidence.to_owned()),
            };
            assert!(
                ledger.put(&key, &record).is_err(),
                "{label} 形状的 evidence 必须被拒"
            );
        }
        assert!(
            ledger.get(&key).is_empty(),
            "被拒的 evidence 不得留下任何行"
        );
        // 合法形状：判据名 / + 组合 / 点分层级 / 状态码
        for evidence in [
            "revision_advanced+new_user_message",
            "probe_p3.schema_match",
            "ack_ok",
        ] {
            let record = CapabilityRecord {
                capability: Capability::Send,
                state: CapabilityState::Confirmed,
                reason_code: None,
                evidence: Some(evidence.to_owned()),
            };
            assert!(
                ledger.put(&key, &record).is_ok(),
                "合法判据 {evidence} 不应被拒"
            );
        }
    }

    /// reason_code 同受硬闸约束（degraded 的冻结错误码是短判据词，不是自由文本）。
    #[test]
    fn reason_code_is_gated_too() {
        let ledger = DbCapabilityLedger::new(DeviceStore::memory());
        let key = caps_key();
        let record = CapabilityRecord {
            capability: Capability::Interrupt,
            state: CapabilityState::Degraded,
            reason_code: Some("eyJhbGciOiJIUzI1NiJ9".to_owned()),
            evidence: None,
        };
        assert!(
            ledger.put(&key, &record).is_err(),
            "疑似 JWT 的 reason_code 必须被拒"
        );
        // 正常错误码可写、可读回
        let ok = CapabilityRecord::degraded(Capability::Interrupt, "stop_ack_timeout".to_owned());
        ledger.put(&key, &ok).unwrap();
        assert_eq!(
            ledger.get(&key)[0].reason_code.as_deref(),
            Some("stop_ack_timeout")
        );
    }

    /// 读侧保守契约：state 落库词表外的行跳过（不猜语义、不 panic）。
    #[test]
    fn unknown_state_word_row_is_skipped() {
        let store = DeviceStore::memory();
        let ledger = DbCapabilityLedger::new(store.clone());
        let key = caps_key();
        ledger
            .put(&key, &CapabilityRecord::confirmed(Capability::Send))
            .unwrap();
        store
            .with(|conn| {
                conn.execute(
                    "UPDATE zcode_capability_evidence SET state = 'future_state'
                     WHERE capability = 'send'",
                    [],
                )
            })
            .unwrap();
        assert!(ledger.get(&key).is_empty(), "未知 state 词的行必须跳过");
    }
}
