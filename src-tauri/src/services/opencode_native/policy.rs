//! macOS 的 OpenCode 会话写入固定走原生；尚未退役的平台保留旧交互准入与在飞登记。
use super::NativeError;
use crate::{remote::pairing::DeviceStore, session::AgentType};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, Weak};

pub const POLICY_KEY: &str = "remote.opencode_interaction_policy";
pub const LEGACY_DISABLED: &str = "OC_LEGACY_DISABLED";
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenCodeInteractionPolicy {
    NativeOnly,
    LegacyCompatible,
}

pub(crate) const fn legacy_retired() -> bool {
    cfg!(target_os = "macos")
}

pub fn read_policy(store: &DeviceStore) -> OpenCodeInteractionPolicy {
    if legacy_retired() {
        return OpenCodeInteractionPolicy::NativeOnly;
    }
    match store
        .with(|c| crate::database::dao::settings::get_setting_conn(c, POLICY_KEY))
        .as_deref()
    {
        Some("legacy_compatible") => OpenCodeInteractionPolicy::LegacyCompatible,
        _ => OpenCodeInteractionPolicy::NativeOnly,
    }
}
pub fn legacy_interaction_allowed(store: &DeviceStore, agent: AgentType) -> bool {
    agent != AgentType::OpenCode
        || read_policy(store) == OpenCodeInteractionPolicy::LegacyCompatible
}

// Owned 以共享连接身份区分；Weak 避免测试库退出后保留状态。生产 Global 固定为 0。
static GATES: LazyLock<Mutex<HashMap<usize, Weak<Mutex<usize>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
fn gate(store: &DeviceStore) -> Arc<Mutex<usize>> {
    let key = match store {
        DeviceStore::Global => 0,
        DeviceStore::Owned(c) => Arc::as_ptr(c) as usize,
    };
    let mut gates = GATES.lock().unwrap_or_else(|e| e.into_inner());
    gates.retain(|_, gate| gate.strong_count() != 0);
    if let Some(gate) = gates.get(&key).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(Mutex::new(0));
    gates.insert(key, Arc::downgrade(&gate));
    gate
}
pub struct LegacyPermit {
    gate: Arc<Mutex<usize>>,
}
impl Drop for LegacyPermit {
    fn drop(&mut self) {
        *self.gate.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
    }
}
pub type Permit = Option<Arc<LegacyPermit>>;
pub fn acquire_legacy(store: &DeviceStore, agent: AgentType) -> Result<Permit, NativeError> {
    if agent != AgentType::OpenCode {
        return Ok(None);
    }
    if legacy_retired() {
        return Err(legacy_disabled());
    }
    let gate = gate(store);
    let mut active = gate.lock().unwrap_or_else(|e| e.into_inner());
    if !legacy_interaction_allowed(store, agent) {
        return Err(legacy_disabled());
    }
    *active += 1;
    drop(active);
    Ok(Some(Arc::new(LegacyPermit { gate })))
}
pub(crate) fn legacy_disabled() -> NativeError {
    NativeError::new(
        LEGACY_DISABLED,
        "OC_LEGACY_DISABLED: OpenCode 当前仅允许原生交互，旧终端交互已停用",
    )
}

pub fn write_policy(
    store: &DeviceStore,
    policy: OpenCodeInteractionPolicy,
) -> Result<(), NativeError> {
    if legacy_retired() {
        // 不改写历史设置；macOS 不允许旧版设置面恢复终端写权限。
        return match policy {
            OpenCodeInteractionPolicy::NativeOnly => Ok(()),
            OpenCodeInteractionPolicy::LegacyCompatible => Err(legacy_disabled()),
        };
    }
    let gate = gate(store);
    let active = gate.lock().unwrap_or_else(|e| e.into_inner());
    if read_policy(store) == policy {
        return Ok(());
    }
    if *active != 0 {
        return Err(NativeError::new(
            "OC_POLICY_BUSY",
            "OC_POLICY_BUSY: OpenCode 旧交互仍在执行，请等待完成后再切换策略",
        ));
    }
    let value = match policy {
        OpenCodeInteractionPolicy::NativeOnly => "native_only",
        OpenCodeInteractionPolicy::LegacyCompatible => "legacy_compatible",
    };
    store
        .with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO settings (key,value) VALUES (?1,?2)",
                [POLICY_KEY, value],
            )
        })
        .map_err(|_| {
            NativeError::new(
                "OC_POLICY_STORAGE",
                "OC_POLICY_STORAGE: 无法保存 OpenCode 交互策略",
            )
        })?;
    let _ = POLICY_EVENTS.send(store_identity(store));
    Ok(())
}

/// 阻塞任务即使 HTTP 被取消也继续持有许可，直到实际屏读/写入及记账结束。
pub(crate) fn spawn_blocking<F, R>(permit: Permit, f: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        f()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_policy_defaults_native_only() {
        let store = DeviceStore::memory();
        for value in [None, Some("broken"), Some("true")] {
            store.with(|c| {
                c.execute("DELETE FROM settings WHERE key=?1", [POLICY_KEY]).unwrap();
                if let Some(value) = value { c.execute("INSERT INTO settings(key,value) VALUES (?1,?2)", [POLICY_KEY,value]).unwrap(); }
                c.execute("INSERT OR REPLACE INTO settings(key,value) VALUES ('remote.opencode_native_enabled','false')", []).unwrap();
            });
            assert_eq!(read_policy(&store), OpenCodeInteractionPolicy::NativeOnly);
            assert!(!legacy_interaction_allowed(&store, AgentType::OpenCode));
            for agent in [AgentType::Claude, AgentType::Codex, AgentType::Kimi] {
                assert!(legacy_interaction_allowed(&store, agent));
            }
        }
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn persisted_legacy_policy_cannot_enable_opencode_on_macos() {
        let store = DeviceStore::memory();
        store.with(|c| {
            c.execute(
                "INSERT INTO settings(key,value) VALUES (?1,?2)",
                [POLICY_KEY, "legacy_compatible"],
            )
            .unwrap();
        });
        assert_eq!(read_policy(&store), OpenCodeInteractionPolicy::NativeOnly);
        assert!(!legacy_interaction_allowed(&store, AgentType::OpenCode));
        match acquire_legacy(&store, AgentType::OpenCode) {
            Err(err) => assert_eq!(err.code, LEGACY_DISABLED),
            Ok(_) => panic!("macOS 不得准入 OpenCode 旧交互"),
        }
        assert_eq!(
            write_policy(&store, OpenCodeInteractionPolicy::LegacyCompatible)
                .unwrap_err()
                .code,
            LEGACY_DISABLED
        );
        write_policy(&store, OpenCodeInteractionPolicy::NativeOnly).unwrap();
        assert_eq!(read_policy(&store), OpenCodeInteractionPolicy::NativeOnly);
        assert_eq!(
            store
                .with(|c| crate::database::dao::settings::get_setting_conn(c, POLICY_KEY))
                .as_deref(),
            Some("legacy_compatible")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn opencode_retirement_does_not_gate_other_agents_on_macos() {
        let store = DeviceStore::memory();
        for agent in [AgentType::Claude, AgentType::Codex, AgentType::Kimi] {
            assert!(legacy_interaction_allowed(&store, agent.clone()));
            assert!(acquire_legacy(&store, agent).unwrap().is_none());
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn policy_flip_cannot_revoke_existing_permit() {
        let store = DeviceStore::memory();
        write_policy(&store, OpenCodeInteractionPolicy::LegacyCompatible).unwrap();
        let permit = acquire_legacy(&store, AgentType::OpenCode).unwrap();
        let inflight = permit.clone();
        drop(permit);
        assert_eq!(
            write_policy(&store, OpenCodeInteractionPolicy::NativeOnly)
                .unwrap_err()
                .code,
            "OC_POLICY_BUSY"
        );
        assert_eq!(
            read_policy(&store),
            OpenCodeInteractionPolicy::LegacyCompatible
        );
        drop(inflight);
        write_policy(&store, OpenCodeInteractionPolicy::NativeOnly).unwrap();
        assert!(acquire_legacy(&store, AgentType::OpenCode).is_err());
    }
    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn canceled_http_keeps_blocking_work_registered() {
        let store = DeviceStore::memory();
        write_policy(&store, OpenCodeInteractionPolicy::LegacyCompatible).unwrap();
        let permit = acquire_legacy(&store, AgentType::OpenCode).unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        let task = spawn_blocking(permit, move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            finished_tx.send(()).unwrap();
        });
        started_rx.await.unwrap();
        task.abort();
        assert_eq!(
            write_policy(&store, OpenCodeInteractionPolicy::NativeOnly)
                .unwrap_err()
                .code,
            "OC_POLICY_BUSY"
        );
        release_tx.send(()).unwrap();
        finished_rx.await.unwrap();
        let _ = task.await;
        write_policy(&store, OpenCodeInteractionPolicy::NativeOnly).unwrap();
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn policy_survives_reopening_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT)", [])
            .unwrap();
        let store = DeviceStore::Owned(Arc::new(Mutex::new(conn)));
        write_policy(&store, OpenCodeInteractionPolicy::LegacyCompatible).unwrap();
        drop(store);
        let reopened = DeviceStore::Owned(Arc::new(Mutex::new(
            rusqlite::Connection::open(&path).unwrap(),
        )));
        assert_eq!(
            read_policy(&reopened),
            OpenCodeInteractionPolicy::LegacyCompatible
        );
    }
    #[test]
    fn policy_wire_rejects_unknown_values() {
        for value in ["", "native", "true", "NATIVE_ONLY"] {
            assert!(
                serde_json::from_value::<OpenCodeInteractionPolicy>(serde_json::json!(value))
                    .is_err()
            );
        }
    }
}

// 只发送失效通知，不携带策略、会话或凭据；各订阅仅接收自己数据库的变化。
static POLICY_EVENTS: LazyLock<tokio::sync::broadcast::Sender<usize>> =
    LazyLock::new(|| tokio::sync::broadcast::channel(64).0);
fn store_identity(store: &DeviceStore) -> usize {
    match store {
        DeviceStore::Global => 0,
        DeviceStore::Owned(conn) => Arc::as_ptr(conn) as usize,
    }
}
pub fn subscribe_policy(store: &DeviceStore) -> (usize, tokio::sync::broadcast::Receiver<usize>) {
    (store_identity(store), POLICY_EVENTS.subscribe())
}
