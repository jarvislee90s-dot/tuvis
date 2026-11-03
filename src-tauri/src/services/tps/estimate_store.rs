//! 正文估算事实与捕获许可。
use crate::services::usage::error::UsageError;
use rusqlite::Connection;
use std::sync::{Arc, Mutex};

/// 对宿主、会话、回合和消息身份分别做确定性单向指纹，不持久化原始值。
pub fn stable_identity(domain: &str, value: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"tuvis.tps.body.identity.v1\0");
    hash.update((domain.len() as u64).to_be_bytes());
    hash.update(domain.as_bytes());
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value.as_bytes());
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone)]
pub struct EstimateFact {
    pub host_key: String,
    pub thread_key: String,
    pub turn_key: String,
    pub item_key: String,
    pub model: Option<String>,
    pub channel: Option<String>,
    pub model_source: String,
    pub reference_profile_id: String,
    pub reference_tokens: u64,
    pub duration_ns: u64,
    pub ended_at_ms: i64,
    pub run_id: String,
    pub source_build: String,
    pub short_window: bool,
    pub small_sample: bool,
}
#[derive(Default)]
struct GateState {
    generation: u64,
    master_enabled: bool,
    source_enabled: bool,
    exhausted: bool,
    updates: Option<tokio::sync::watch::Sender<u64>>,
    plaintext_jobs: usize,
}
#[derive(Default)]
struct GateShared {
    state: Mutex<GateState>,
    plaintext_zero: std::sync::Condvar,
}
#[derive(Clone, Default)]
pub struct CaptureGate(Arc<GateShared>);
#[derive(Clone)]
pub struct CapturePermit {
    gate: CaptureGate,
    generation: u64,
}
/// 任务携带正文期间的许可；Drop 在正文释放后通知停采等待者。
pub(crate) struct PlaintextGuard {
    gate: CaptureGate,
}
impl Drop for PlaintextGuard {
    fn drop(&mut self) {
        let mut state = self.gate.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.plaintext_jobs = state.plaintext_jobs.saturating_sub(1);
        if state.plaintext_jobs == 0 {
            self.gate.0.plaintext_zero.notify_all();
        }
    }
}
impl CapturePermit {
    pub(crate) fn acquire_plaintext(&self) -> Option<PlaintextGuard> {
        let mut state = self.gate.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.master_enabled
            || !state.source_enabled
            || state.exhausted
            || state.generation != self.generation
        {
            return None;
        }
        state.plaintext_jobs = state.plaintext_jobs.checked_add(1)?;
        Some(PlaintextGuard {
            gate: self.gate.clone(),
        })
    }
    pub fn is_current(&self) -> bool {
        let state = self.gate.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.master_enabled
            && state.source_enabled
            && !state.exhausted
            && state.generation == self.generation
    }
}
impl CaptureGate {
    pub(crate) fn owns(&self, permit: &CapturePermit) -> bool {
        Arc::ptr_eq(&self.0, &permit.gate.0)
    }
    pub fn new(enabled: bool) -> Self {
        Self(Arc::new(GateShared {
            state: Mutex::new(GateState {
                generation: 1,
                master_enabled: enabled,
                source_enabled: true,
                exhausted: false,
                updates: Some(tokio::sync::watch::channel(1).0),
                plaintext_jobs: 0,
            }),
            plaintext_zero: std::sync::Condvar::new(),
        }))
    }
    pub fn permit(&self) -> Option<CapturePermit> {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.master_enabled && state.source_enabled && !state.exhausted).then(|| CapturePermit {
            gate: self.clone(),
            generation: state.generation,
        })
    }
    /// 结束当前捕获运行，但不修改持久设置。
    pub fn invalidate_capture(&self) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::advance(&mut state);
    }
    pub fn subscribe(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .updates
            .as_ref()
            .map(|tx| tx.subscribe())
    }
    fn advance(state: &mut GateState) {
        if let Some(next) = state.generation.checked_add(1) {
            state.generation = next;
        } else {
            state.exhausted = true;
            state.master_enabled = false;
            state.source_enabled = false;
        }
        if let Some(updates) = &state.updates {
            updates.send_replace(state.generation);
        }
    }
    pub fn set_master_enabled(&self, enabled: bool) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.master_enabled != enabled {
            Self::advance(&mut state);
            if !state.exhausted {
                state.master_enabled = enabled;
            }
        }
    }
    pub fn set_source_enabled(&self, enabled: bool) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.source_enabled != enabled {
            Self::advance(&mut state);
            if !state.exhausted {
                state.source_enabled = enabled;
            }
        }
    }
    fn save_source_with(
        &self,
        enabled: bool,
        persist: impl FnOnce() -> Result<(), UsageError>,
    ) -> Result<(), UsageError> {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if !enabled && state.source_enabled {
            Self::advance(&mut state);
            state.source_enabled = false;
        }
        if !enabled {
            while state.plaintext_jobs > 0 {
                state = self
                    .0
                    .plaintext_zero
                    .wait(state)
                    .unwrap_or_else(|e| e.into_inner());
            }
        }
        persist()?;
        if enabled && !state.source_enabled && !state.exhausted {
            Self::advance(&mut state);
            if !state.exhausted {
                state.source_enabled = true;
            }
        }
        Ok(())
    }
    /// 调用方先持数据库连接，再进入捕获门；与事实提交的锁序一致。
    pub(crate) fn save_master_with(
        &self,
        enabled: bool,
        persist: impl FnOnce() -> Result<(), UsageError>,
    ) -> Result<(), UsageError> {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if !enabled && state.master_enabled {
            Self::advance(&mut state);
            state.master_enabled = false;
        }
        if !enabled {
            while state.plaintext_jobs > 0 {
                state = self
                    .0
                    .plaintext_zero
                    .wait(state)
                    .unwrap_or_else(|e| e.into_inner());
            }
        }
        persist()?;
        if enabled && !state.master_enabled && !state.exhausted {
            Self::advance(&mut state);
            if !state.exhausted {
                state.master_enabled = true;
            }
        }
        Ok(())
    }
}
/// 生产捕获门在首次使用时读取既有设置，调用方不得持有全局数据库锁初始化。
pub fn global_capture_gate() -> &'static CaptureGate {
    static GATE: std::sync::OnceLock<CaptureGate> = std::sync::OnceLock::new();
    GATE.get_or_init(|| {
        let master = crate::services::usage::settings::load().enabled;
        let source = {
            let conn = crate::database::connection::DB
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match super::store::load_state(&conn, ENABLED_KEY) {
                Ok(None) => true,
                Ok(Some(value)) => value.as_bool().unwrap_or(false),
                Err(_) => false,
            }
        };
        let gate = CaptureGate::new(master);
        gate.set_source_enabled(source);
        gate
    })
}

/// 许可校验和事务提交持有同一捕获门，停采完成后旧代际不能提交。
pub fn commit_estimate(
    conn: &mut Connection,
    fact: &EstimateFact,
    permit: &CapturePermit,
) -> Result<bool, UsageError> {
    let is_hash = |value: &str| {
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    if ![
        &fact.host_key,
        &fact.thread_key,
        &fact.turn_key,
        &fact.item_key,
        &fact.reference_profile_id,
    ]
    .iter()
    .all(|key| is_hash(key))
    {
        return Err(UsageError::internal(
            "estimate identity or profile is not a stable digest".into(),
        ));
    }
    let state = permit
        .gate
        .0
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !state.master_enabled || !state.source_enabled || state.generation != permit.generation {
        return Ok(false);
    }
    let tx = conn.transaction().map_err(|e| {
        UsageError::new(
            "usage-db-failed",
            format!("estimate transaction failed: {e}"),
        )
    })?;
    let inserted = crate::database::dao::tps_estimate::insert(&tx, fact)?;
    tx.commit()
        .map_err(|e| UsageError::new("usage-db-failed", format!("estimate commit failed: {e}")))?;
    Ok(inserted)
}

pub const ENABLED_KEY: &str = "tokenizer-estimate:enabled";
#[doc(hidden)]
pub fn set_estimate_enabled_with_gate(
    conn: &Connection,
    enabled: bool,
    gate: &CaptureGate,
) -> Result<(), UsageError> {
    gate.save_source_with(enabled, || {
        super::store::save_state(conn, ENABLED_KEY, &serde_json::json!(enabled))
    })
}

/// 持久化单源开关；与总授权和事实提交共享同一代际门。
pub fn set_estimate_enabled(enabled: bool) -> Result<(), UsageError> {
    let gate = global_capture_gate();
    let conn = crate::database::connection::DB
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    set_estimate_enabled_with_gate(&conn, enabled, gate)
}

impl EstimateFact {
    /// 设置观察窗口，单位为单调时钟纳秒。
    pub fn set_duration_ns(&mut self, duration_ns: u64) {
        self.duration_ns = duration_ns;
    }
}
