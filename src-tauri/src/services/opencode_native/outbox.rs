//! 本地待发的生命周期与同会话派发许可。暂停本地不会停止原生已受理项。
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};

use super::NativeHub;

pub(crate) struct OutboxRuntime {
    pub enabled: AtomicBool,
    pub epoch: AtomicU64,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    task: Mutex<Option<(u64, tokio::task::JoinHandle<()>)>>,
}

impl Default for OutboxRuntime {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(true),
            epoch: AtomicU64::new(0),
            locks: Mutex::new(HashMap::new()),
            task: Mutex::new(None),
        }
    }
}

impl OutboxRuntime {
    pub fn session_lock(&self, session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(session_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}

impl NativeHub {
    pub(crate) fn stop_outbox(&self) {
        // 与最后一次 claim 共用连接互斥：先停止则不能取得写权，已 claim 则继续唯一一次落账。
        self.store().with(|_| {
            self.outbox_runtime().enabled.store(false, Ordering::SeqCst);
            self.outbox_runtime().epoch.fetch_add(1, Ordering::SeqCst);
        });
    }
}

/// 已确认发送的冻结快照。原始命令保留指纹，附件使用专属只读副本。
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct FrozenSend {
    pub command: super::types::NativeCommand,
    pub attachment_paths: Vec<String>,
    pub send: super::types::SendPayload,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxView {
    pub action_id: String,
    pub owner: &'static str,
    pub state: super::types::ActionState,
    pub text_summary: String,
    pub attachment_count: u64,
    pub reason_code: Option<String>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxSnapshot {
    pub session_id: String,
    pub revision: u64,
    pub paused: bool,
    pub items: Vec<OutboxView>,
}

impl NativeHub {
    pub fn outbox_view(
        &self,
        session: &crate::session::Session,
    ) -> Result<OutboxSnapshot, super::types::NativeError> {
        let directory = std::fs::canonicalize(&session.project_path)
            .unwrap_or_else(|_| std::path::PathBuf::from(&session.project_path));
        if !directory.is_absolute() {
            return Err(super::types::NativeError::new(
                super::actions::action_codes::STORE_FAILED,
                "outbox target unavailable",
            ));
        }
        self.outbox_view_for_target(&session.id, &directory.to_string_lossy())
    }

    pub(crate) fn outbox_view_for_target(
        &self,
        session_id: &str,
        directory: &str,
    ) -> Result<OutboxSnapshot, super::types::NativeError> {
        let _ = self.action_store();
        self.store().with(|conn| {
            let (revision,paused)=crate::database::dao::opencode_outbox::target_state_conn(conn,session_id,directory)?;
            let mut statement=conn.prepare("SELECT o.action_id,o.text_summary,o.attachment_count FROM opencode_outbox o JOIN opencode_native_actions a ON a.action_id=o.action_id AND a.session_id=o.session_id WHERE o.session_id=?1 AND a.directory=?2 ORDER BY o.seq")?;
            let rows=statement.query_map(rusqlite::params![session_id,directory],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?)))?;
            let mut items=Vec::new();
            for row in rows {
                let (action_id,text_summary,attachment_count)=row?;
                if let Some(action)=crate::database::dao::opencode_actions::get_action_conn(conn,&action_id)? {
                    let local=matches!(action.state,super::types::ActionState::Queued|super::types::ActionState::NotSent);
                    items.push(OutboxView {action_id,owner:if local {"tuvis"} else {"native"},state:action.state,text_summary,attachment_count:attachment_count as u64,reason_code:action.reason_code});
                }
            }
            // D-N5-2（孤儿待发项）：客户端在门轮被中止（10s 超时/页面离场）会在账本留下
            // `not_sent` 且无放弃理由的行——reserve 已落、claim 从未发生、**从未出网**，
            // 但没有 outbox 行（载荷未冻结），既不可见也不可撤回，还计入未决谓词挡重选。
            // 这里把它们作为本地项（owner=tuvis）呈现：正文不可得（账本只存指纹）⇒
            // text_summary 留空，由前端以受控文案如实标注「正文未知」，不冒充原文。
            // 撤回出口见 `execute_local_outbox_pinned` / `retract_in_transaction`。
            let mut orphans=conn.prepare("SELECT a.action_id FROM opencode_native_actions a WHERE a.session_id=?1 AND a.directory=?2 AND a.state='not_sent' AND a.reason_code IS NULL AND NOT EXISTS (SELECT 1 FROM opencode_outbox o WHERE o.action_id=a.action_id) ORDER BY a.created_at ASC, a.action_id ASC")?;
            let orphan_rows=orphans.query_map(rusqlite::params![session_id,directory],|r|r.get::<_,String>(0))?;
            for row in orphan_rows {
                let action_id=row?;
                if let Some(action)=crate::database::dao::opencode_actions::get_action_conn(conn,&action_id)? {
                    items.push(OutboxView {action_id,owner:"tuvis",state:action.state,text_summary:String::new(),attachment_count:0,reason_code:action.reason_code});
                }
            }
            Ok::<_,rusqlite::Error>(OutboxSnapshot {session_id:session_id.to_owned(),revision,paused,items})
        }).map_err(|_|super::types::NativeError::new(super::actions::action_codes::STORE_FAILED,"outbox unavailable"))
    }

    /// 远程生命周期中的唯一后台消费者；不依赖 watch/详情页订阅。
    pub fn start_outbox(
        self: &Arc<Self>,
        sessions: Arc<dyn Fn() -> Vec<crate::session::Session> + Send + Sync>,
    ) {
        let _ = self.action_store();
        let runtime = self.outbox_runtime();
        let mut slot = runtime.task.lock().unwrap();
        let epoch = self.store().with(|_| {
            runtime.enabled.store(true, Ordering::SeqCst);
            runtime.epoch.load(Ordering::SeqCst)
        });
        if slot
            .as_ref()
            .is_some_and(|(existing, task)| *existing == epoch && !task.is_finished())
        {
            return;
        }
        let weak = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                interval.tick().await;
                let Some(hub) = weak.upgrade() else {
                    return;
                };
                if !hub.outbox_runtime().enabled.load(Ordering::SeqCst)
                    || hub.outbox_runtime().epoch.load(Ordering::SeqCst) != epoch
                {
                    return;
                }
                // H1（后台不能清除拒绝原因后悄悄投递）：带窗口 CAS 拒绝原因的排队项
                // 不计入 pending、不唤醒循环——跳过语义与 flush 逐项查询同源。
                let pending=hub.store().with(|conn| conn.query_row("SELECT EXISTS(SELECT 1 FROM opencode_outbox o JOIN opencode_native_actions a ON a.action_id=o.action_id AND a.session_id=o.session_id JOIN opencode_outbox_targets t ON t.session_id=a.session_id AND t.directory=a.directory WHERE a.state='queued' AND t.paused=0 AND (a.reason_code IS NULL OR a.reason_code<>?1) AND o.command_json IS NOT NULL AND EXISTS (SELECT 1 FROM settings WHERE key='opencode.outbox_target_schema_version' AND value='1'))",[crate::services::opencode_native::types::codes::OC_ROUTE_WINDOW_MISMATCH],|r|r.get::<_,bool>(0)).unwrap_or(false));
                if !pending {
                    continue;
                }
                let source = sessions.clone();
                let mut cards = tokio::task::spawn_blocking(move || source())
                    .await
                    .unwrap_or_default();
                // 持久待发项的原目标不随看板下卡而消失；逐会话用最早排队行的
                // 目录恢复寻址，flush 仍逐项核验原服务代际和设备，不以此授权写入。
                let queued = hub.store().with(|conn| {
                    let mut stmt = conn.prepare("SELECT a.session_id,a.directory FROM opencode_outbox o JOIN opencode_native_actions a ON a.action_id=o.action_id AND a.session_id=o.session_id JOIN opencode_outbox_targets t ON t.session_id=a.session_id AND t.directory=a.directory WHERE a.state='queued' AND t.paused=0 AND (a.reason_code IS NULL OR a.reason_code<>?1) AND o.command_json IS NOT NULL AND EXISTS (SELECT 1 FROM settings WHERE key='opencode.outbox_target_schema_version' AND value='1') ORDER BY o.seq")?;
                    let rows = stmt.query_map([crate::services::opencode_native::types::codes::OC_ROUTE_WINDOW_MISMATCH], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
                    rows.collect::<Result<Vec<_>, _>>()
                });
                if let Ok(queued) = queued {
                    let queued_ids: std::collections::HashSet<_> =
                        queued.iter().map(|(id, _)| id.clone()).collect();
                    cards.retain(|card| {
                        card.agent_type != crate::session::AgentType::OpenCode
                            || !queued_ids.contains(&card.id)
                    });
                    let mut seen = std::collections::HashSet::new();
                    for (id, directory) in queued {
                        if seen.insert((id.clone(), directory.clone())) {
                            cards.push(super::actions::session_card(&id, &directory));
                        }
                    }
                }
                for session in cards
                    .iter()
                    .filter(|s| s.agent_type == crate::session::AgentType::OpenCode)
                {
                    if hub.outbox_runtime().epoch.load(Ordering::SeqCst) != epoch {
                        return;
                    }
                    if let Err(error) = hub.flush_outbox_at_epoch(session, epoch).await {
                        log::debug!("OpenCode outbox retained: {}", error.code);
                    }
                }
            }
        });
        // 旧代任务若在唯一写途中，让它自然落账；epoch 闸禁止它开始下一次写。
        *slot = Some((epoch, task));
    }
}
