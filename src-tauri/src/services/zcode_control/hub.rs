//! 单实例 ZCode 控制 Hub（生产计划 T7.1 Slice A）。
//!
//! # 职责
//! - **唯一**的进程内控制面：桌面与两台配对手机共享同一个 [`ZcodeHub`] 与同一条物理
//!   relay/bridge（不按调用方建第二条 socket）；
//! - **代际权威**：配置保存/删除先在 Hub 推进 generation，再断开 relay 并作废投影/动作；
//!   **换/删链接保留账号 JWT**（真正忘记授权走 `forget_authorization`），旧代际的迟到
//!   连接/动作一律 `stale_generation`，且**零**后端执行；
//! - **能力闸**：`execute` 在绑定/派发之前按本 Hub 的 [`CapabilityView`] 拒绝未开闸动作
//!   （默认全关 = `needs_evidence`），手机无法自报能力；
//! - **事件面**：`subscribe` 广播 [`ControlEvent`]（连接/会话/动作），事件体只含 Tuvis
//!   DTO，**绝无** JWT/token/官方原文。
//!
//! 构造即装配生产链（AuthService → RelayClient → BridgeManager → CommandSender）并启动
//! 一条常驻 relay 事件泵：`Data` 逐帧进桥，`State` 更新连接态、`conflict`（KICKED）
//! 推进控制代际并作废旧桥/投影；`Data` 溢出（`RelayLag`）显式作废旧投影并发布
//! `ready=false` 的降级事件，下一次 GET 重新 bind/现读（绝不返回旧缓存）。
//! 构造**不**自动 OAuth、**不**自动连接。
//! HTTP/IPC 接线在后续切片完成；本文件只交付可测的 Hub 核心。
//! DB 读写失败由调用方处置：`config_changed` 只在保存/删除**成功后**被调用，
//! 失败保存不得断开旧连接（本函数不读写入结果、不自行落库）。

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, Mutex as AsyncMutex, Notify};

use super::auth::{AuthService, AuthStatus, OAuthProvider, OpenAuthorize};
use super::auto_connect::{capture_auto_intent, write_auto_intent, AutoIntentClaim};
use super::bridge::{BridgeManager, DataOutcome};
use super::capability::{
    confirm_interrupt, confirm_pending, confirm_selection, confirm_send, resolve_capabilities,
    Capability, CapabilityKey, CapabilityRecord, CapabilityState, ConfirmationBaseline,
    ConfirmationContext, ConfirmationObservation, ConfirmationOutcome, ConfirmationTarget,
    InterruptObservation, SelectionObservation, SendObservation,
};
use super::command::CommandSender;
use super::config::{get_config, ConfigState};
use super::credential_store::OsCredentialStore;
use super::relay::{DeviceRoute, RelayClient, RelayEvent, RelayLag};
use super::types::{
    codes, ActionContext, ActionReceipt, ActionState, BoundTarget, CapabilitySource,
    CapabilityView, ConnectionState, ConnectionView, ControlAction, ControlEvent, ControlEventKind,
    HostBuildProbe, HostEvidence, SessionView, TargetKey, ZResult, ZcodeError,
};
use crate::database::dao::zcode_capability::{
    validate_evidence_token, CapabilityLedger, DbCapabilityLedger,
};
use crate::remote::pairing::DeviceStore;

#[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
use super::evidence::{EvidencePermit, EvidencePermitStore};

/// Hub 广播缓冲（事件只做通知，缺口由消费方重 GET 兜底）。
const EVENT_CAPACITY: usize = 64;
/// relay→Hub 事件泵队列（有界：State 满时 relay 侧**无超时**等待消费端，Data 满时
/// 置位 lag 并触发投影失效，消费方绝不把旧缓存当权威）。
const RELAY_EVENT_CAPACITY: usize = 64;
/// 已发布会话视图的跟踪上限（有界：**最后登记**的最多 32 个；超限淘汰最久未登记
/// 的会话（LRU），不随会话数无界增长。Data 溢出按此表逐会话降级）。
const TRACKED_SESSION_CAP: usize = 32;

/// 把静态矩阵包装成现算源（测试/固定夹具；生产源来自 capability resolver）。
fn static_policy_source(policy: CapabilityView) -> CapabilitySource {
    Arc::new(move || policy.clone())
}

/// schema 指纹占位（T4 裁决）：T3 尚未提供真实 schema 哈希；key 失效由
/// `app_build` + `protocol_version` 承担，本占位只保证同 build 内 key 稳定
/// （账本 confirmed/degraded 跨重连存活的前提）。恒为固定标识——正文/凭据
/// 绝不进入 fingerprint。
const SCHEMA_FINGERPRINT_PLACEHOLDER: &str = "v4";

/// probe epoch 占位（T4 裁决）：本任务恒 0。失效面已由 invalidate 清空接口形状
/// 证据 + build/协议变化换 key 覆盖；epoch 推进属后续探测复位任务。
const PROBE_EPOCH: u32 = 0;

/// 从宿主证据拼能力 key（spec R2 五元组）。platform 取本机 OS（`macos`/`windows`
/// 与 shipped 表词表一致）；build/协议缺席按空串/0 处理（未知形态：shipped 不
/// 命中、账本失配、仅接口候选可判——resolver 侧按缺证据保守关闭）。
fn capability_key(evidence: &HostEvidence) -> CapabilityKey {
    CapabilityKey::new(
        std::env::consts::OS,
        evidence.app_build.as_deref().unwrap_or(""),
        evidence.protocol_version.unwrap_or(0),
        SCHEMA_FINGERPRINT_PLACEHOLDER,
        PROBE_EPOCH,
    )
}

/// 首次真实动作确认窗（spec R4：5s 有界；到点未确认 → inconclusive，绝不重发）。
const CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(5);

/// 确认落账的判据词表（全部是结构化判据标识，满足 T2 写入硬闸；
/// 正文/题目/选项/答案/JWT/链接绝不进入账本）。
mod confirmation_words {
    /// send 确认：revision 前进 + 新用户消息可见（id 变化代表可见）。
    pub(crate) const SEND_CONFIRMED: &str = "revision_advanced+new_user_message";
    /// model/thought 确认：新快照选择三元组精确相等。
    pub(crate) const SELECTION_CONFIRMED: &str = "selection_matched";
    /// pending 卡确认（泵观察判据路径）：目标卡撤下 + 会话 revision 前进。
    pub(crate) const PENDING_CONFIRMED: &str = "pending_cleared+revision_advanced";
    /// pending 卡确认（回执因果终态路径）：宿主已撤卡确证（CommandSender 的
    /// wait_host_cleared 观察）；会话 revision 前进由泵判据负责，这里绝不冒写。
    pub(crate) const PENDING_CLEARED: &str = "pending_cleared";
    /// pending 卡到点未决且最后观察里目标卡仍在场。
    pub(crate) const PENDING_STILL_PRESENT: &str = "pending_still_present";
    /// interrupt 确认（泵观察判据路径）：目标守卫消失 + 会话 revision 前进。
    pub(crate) const INTERRUPT_CONFIRMED: &str = "guard_cleared+revision_advanced";
    /// interrupt 确认（回执因果终态路径）：宿主停止终态确证目标守卫消失
    /// （CommandSender 的 wait_stop_cleared 观察）；会话 revision 前进由泵判据
    /// 负责，这里绝不冒写。
    pub(crate) const INTERRUPT_CLEARED: &str = "guard_cleared";
    /// interrupt 到点未决且最后观察里目标守卫仍在场。
    pub(crate) const INTERRUPT_STILL_ACTIVE: &str = "guard_still_active";
    /// 确认窗到点未取得证据（不重发，保持候选）。
    pub(crate) const TIMEOUT: &str = "timeout_5s";
    /// 动作回执显式失败（host rejected/noop）。
    pub(crate) const EXPLICIT_FAILURE: &str = "explicit_failure";
    /// 回执保守未知（写后 ACK 超窗等——结果未知按 inconclusive）。
    pub(crate) const UNKNOWN_RECEIPT: &str = "unknown_receipt";
    /// 显式失败但回执原因码不是判据标识形状时的兜底冻结码。
    pub(crate) const ACTION_FAILED: &str = "action_failed";
}

/// 首次真实动作确认的 pending 登记表（spec R4；Hub 内唯一实例）。
///
/// 生命周期：动作提交前登记（baseline 已捕获）→ 事件泵观察新投影调
/// [`Self::observe_session`]，或动作回执直接给出终态调 [`Self::settle_receipt`]；
/// 到点未决由每条登记的计时器任务走 [`Self::sweep_expired`] 写 inconclusive。
/// 纪律：**绝不重发**动作；旧代际/别的 session 的结果丢弃不落账；ACK-only 绝不升级。
pub(crate) struct ConfirmationTracker {
    /// `(session_id, capability) → 登记上下文`。同槽位已有 pending 时忽略再登记
    /// （首次确认未决期间的后续真实使用不另开确认窗，首个 baseline 已足够）。
    pending: Mutex<BTreeMap<(String, Capability), ConfirmationContext>>,
    /// T2 能力账本（`None` = 未装配账本：确认流程整体停用，登记面跳过）。
    ledger: Option<Arc<dyn CapabilityLedger>>,
}

impl ConfirmationTracker {
    fn new(ledger: Option<Arc<dyn CapabilityLedger>>) -> Self {
        Self {
            pending: Mutex::new(BTreeMap::new()),
            ledger,
        }
    }

    /// 本会话是否有未决确认（事件泵观察面的快速路径）。
    fn has_pending(&self, session_id: &str) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .any(|(session, _)| session == session_id)
    }

    /// 该能力是否还需要首次确认（账本无 Confirmed/Degraded/Unsupported 判决；
    /// Inconclusive/InterfaceMatched/无记录都算候选）。未装配账本 → false。
    fn needs_confirmation(&self, key: &CapabilityKey, capability: Capability) -> bool {
        let Some(ledger) = self.ledger.as_ref() else {
            return false;
        };
        !ledger.get(key).into_iter().any(|record| {
            record.capability == capability
                && matches!(
                    record.state,
                    CapabilityState::Confirmed
                        | CapabilityState::Degraded
                        | CapabilityState::Unsupported
                )
        })
    }

    /// 登记一条确认（同槽位已有 pending → **忽略并保留首个 baseline**，返回 false）。
    ///
    /// 不能用 `BTreeMap::insert` 直写：它对已存在键是**覆盖**并返回 `Some(old)`——
    /// 「更晚 deadline、新 baseline」的 ctx 会顶掉旧登记，而返回 false 又不补计时器，
    /// 新期限从此无人负责（会话安静时 pending 无限滞留，有界窗承诺被打破）。
    /// 判空短路后同槽位二次登记是纯 no-op：旧 ctx（原 deadline、原计时器）原样保留。
    fn register(&self, ctx: ConfirmationContext) -> bool {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let slot = (ctx.session_id.clone(), ctx.capability);
        if pending.contains_key(&slot) {
            return false;
        }
        pending.insert(slot, ctx);
        true
    }

    /// 事件泵观察：一条新投影（`generation` 是发布该视图的 Hub 代际；`fingerprint`
    /// 是观察时刻的会话日志头指纹，可缺）。两段式：锁内只做判定与摘除，锁外落账。
    fn observe_session(&self, view: &SessionView, generation: u64, fingerprint: Option<String>) {
        let now = Instant::now();
        let decisions = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let session_keys: Vec<(String, Capability)> = pending
                .keys()
                .filter(|(session, _)| session == &view.session_id)
                .cloned()
                .collect();
            let mut decisions = Vec::new();
            for slot in session_keys {
                // 先摘除再判定：confirmed/超时/旧代际直接离场，未决的重新放回。
                let Some(ctx) = pending.remove(&slot) else {
                    continue;
                };
                // 旧代际：确认结果丢弃（spec R4），不落账、只摘除。
                if generation != ctx.generation {
                    continue;
                }
                // 到点未决：inconclusive（有界窗，绝不重发）。
                if now >= ctx.deadline {
                    // Pending/Interrupt 目标：最后观察里目标卡/守卫仍在场 → 判据词
                    // 如实记「卡未撤 / 守卫未清」；其余形态沿用一般超窗词。
                    let record = match &ctx.target {
                        ConfirmationTarget::Pending { request_key }
                            if view
                                .pending
                                .iter()
                                .any(|card| &card.interaction_id == request_key) =>
                        {
                            inconclusive_record_with(
                                slot.1,
                                confirmation_words::PENDING_STILL_PRESENT,
                            )
                        }
                        ConfirmationTarget::Interrupt { guard_id }
                            if view.control.as_ref().is_some_and(|control| {
                                control.active_works.iter().any(|work| {
                                    work.foreground_execution_id.as_deref() == Some(guard_id)
                                })
                            }) =>
                        {
                            inconclusive_record_with(
                                slot.1,
                                confirmation_words::INTERRUPT_STILL_ACTIVE,
                            )
                        }
                        _ => inconclusive_record(slot.1),
                    };
                    decisions.push((ctx, record));
                    continue;
                }
                let outcome = match &ctx.target {
                    ConfirmationTarget::UserMessage { .. } => {
                        // 观察证据缺一半（revision 或指纹未取得）→ 不判定，等下一帧。
                        match (view.revision, fingerprint.as_ref()) {
                            (Some(revision), Some(id)) => {
                                let obs = SendObservation {
                                    generation,
                                    revision,
                                    last_user_message_id: id.clone(),
                                };
                                confirm_send(&ctx, &obs)
                            }
                            _ => ConfirmationOutcome::Inconclusive,
                        }
                    }
                    ConfirmationTarget::Selection { .. } => match view.selection.as_ref() {
                        Some(selection) => {
                            let obs = SelectionObservation {
                                generation,
                                revision: view.revision.unwrap_or(0),
                                provider: selection.provider.clone(),
                                model: selection.model.clone(),
                                thought: selection.thought.clone().unwrap_or_default(),
                            };
                            confirm_selection(&ctx, &obs)
                        }
                        // 快照 config 未到：不判定，等下一帧。
                        None => ConfirmationOutcome::Inconclusive,
                    },
                    ConfirmationTarget::Pending { .. } => {
                        // 观察视图带的是桥投影代际：判定输入统一盖本 Hub 代际（与
                        // send/selection 观察的 generation 实参同源），否则判据的
                        // generation 闸永不咬合。
                        let mut after = view.clone();
                        after.generation = generation;
                        confirm_pending(&ctx, &after)
                    }
                    ConfirmationTarget::Interrupt { .. } => {
                        // 等效 control 快照：守卫 id 集合只在宿主明确宣告 control
                        // （activeWorks 是数组）时才算证据；未宣告 → 没有任何
                        // 「守卫消失」证据，等下一帧。generation 实参 = 本 Hub 代际
                        // （与 send/selection 同源）。
                        let active_works = view.control.as_ref().map(|control| {
                            control
                                .active_works
                                .iter()
                                .filter_map(|work| work.foreground_execution_id.clone())
                                .collect::<Vec<_>>()
                        });
                        let obs = InterruptObservation {
                            generation,
                            revision: view.revision,
                            active_works,
                        };
                        confirm_interrupt(&ctx, &obs)
                    }
                };
                if outcome == ConfirmationOutcome::Confirmed {
                    let evidence = confirmed_evidence(&ctx.target);
                    decisions.push((ctx, confirmed_record(slot.1, evidence)));
                } else {
                    // Inconclusive：保持 pending（等后续帧或计时器）。
                    pending.insert(slot, ctx);
                }
            }
            decisions
        };
        for (ctx, record) in decisions {
            self.write(&ctx, record);
        }
    }

    /// 动作回执终态收口（派发返回后立即判定）。
    ///
    /// 只结算**本次 action 涉及的能力槽位**（`capabilities` 由登记点随 action 传入）：
    /// 同会话其他能力（如仍在 ACK 窗内的 send）的未决确认绝不由本回执代答——selection
    /// 的回执既不能替 send 写 confirmed（没有新用户消息证据），也不能把 send 一并降级。
    ///
    /// `current_generation` 是结算时刻的 Hub 当前代际：登记代际已过期（派发在途发生
    /// 配置变更/断开/冲突）→ 全部摘除且**零落账**（spec R4「旧代际确认结果丢弃」
    /// 覆盖**所有**写路径，回执收口不是帧观察的例外）。
    fn settle_receipt(
        &self,
        session_id: &str,
        capabilities: &[Capability],
        receipt: &ActionReceipt,
        current_generation: u64,
    ) {
        let decisions: Vec<(ConfirmationContext, CapabilityRecord)> = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // 本 action 的槽位 = 同会话 ∩ 本次动作的能力集。
            let mut slots: Vec<(String, Capability)> = pending
                .keys()
                .filter(|(session, capability)| {
                    session == session_id && capabilities.contains(capability)
                })
                .cloned()
                .collect();
            // 旧代际登记：摘除且零落账（本回执不构成新代际证据）。
            let mut stale: Vec<(String, Capability)> = Vec::new();
            slots.retain(|slot| {
                let live = pending
                    .get(slot)
                    .is_some_and(|ctx| ctx.generation == current_generation);
                if !live {
                    stale.push(slot.clone());
                }
                live
            });
            for slot in stale {
                pending.remove(&slot);
            }
            match receipt.state {
                // ACK-only：保持 pending，交给事件泵/计时器（绝不因此升级）。
                ActionState::Accepted => Vec::new(),
                // 因果终态（如 selection 的 wait_selection 命中）→ confirmed。
                ActionState::Confirmed => slots
                    .iter()
                    .filter_map(|slot| pending.remove(slot).map(|ctx| (ctx, slot.1)))
                    .map(|(ctx, capability)| {
                        let evidence = confirmed_receipt_evidence(&ctx.target);
                        (ctx, confirmed_record(capability, evidence))
                    })
                    .collect(),
                // 宿主显式失败 → degraded，只影响本 key 本能力（冻结原因码）。
                ActionState::Rejected | ActionState::Noop => {
                    let reason = sanitized_reason(receipt);
                    slots
                        .iter()
                        .filter_map(|slot| pending.remove(slot).map(|ctx| (ctx, slot.1)))
                        .map(|(ctx, capability)| {
                            let mut record = CapabilityRecord::degraded(capability, reason.clone());
                            record.evidence = Some(confirmation_words::EXPLICIT_FAILURE.to_owned());
                            (ctx, record)
                        })
                        .collect()
                }
                // 结果未知（写后 ACK 超窗等）→ inconclusive（保持候选）。
                ActionState::Unknown => slots
                    .iter()
                    .filter_map(|slot| pending.remove(slot).map(|ctx| (ctx, slot.1)))
                    .map(|(ctx, capability)| {
                        (
                            ctx,
                            inconclusive_record_with(
                                capability,
                                confirmation_words::UNKNOWN_RECEIPT,
                            ),
                        )
                    })
                    .collect(),
                // 确证零写（桥写前拒绝）：本次尝试零证据 → 只摘除，不落账。
                ActionState::SendFailed => {
                    for slot in &slots {
                        pending.remove(slot);
                    }
                    Vec::new()
                }
            }
        };
        for (ctx, record) in decisions {
            self.write(&ctx, record);
        }
    }

    /// 派发失败（Err，无回执）：只摘除**本次 action 能力集**内的 pending，不落账。
    ///
    /// CAS 陈旧（selection stale）与端口错误都不构成能力证据——前者只需刷新重试，
    /// 后者可能是瞬时连通性问题；降级只留给回执显式失败。
    fn drop_pending(&self, session_id: &str, capabilities: &[Capability]) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let slots: Vec<(String, Capability)> = pending
            .keys()
            .filter(|(session, capability)| {
                session == session_id && capabilities.contains(capability)
            })
            .cloned()
            .collect();
        for slot in slots {
            pending.remove(&slot);
        }
    }

    /// ACK 后重置确认窗期限（spec R4：「ACK 后等新投影……5s 有界」）。
    ///
    /// 只重置**本会话本代际**的 pending；返回每条新期限（调用方逐条补计时器任务，
    /// 旧计时器照常醒来但只会看到更晚的期限，sweep 自然空转）。
    fn rearm(&self, session_id: &str, generation: u64, timeout: Duration) -> Vec<Instant> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        let mut deadlines = Vec::new();
        for ((session, _), ctx) in pending.iter_mut() {
            if session == session_id && ctx.generation == generation {
                ctx.deadline = now.checked_add(timeout).unwrap_or_else(Instant::now);
                deadlines.push(ctx.deadline);
            }
        }
        deadlines
    }

    /// 计时器回调：摘除并落账全部到点未决的登记（inconclusive）。
    ///
    /// `current_generation` 是醒来时刻的 Hub 当前代际：登记代际已过期 → 只摘除
    /// **零落账**（确认结果随代际一起作废；没有事件帧帮它丢弃的旧代际 pending
    /// 由此收口，绝不过期滞留、也绝不写旧判决）。
    fn sweep_expired(&self, now: Instant, current_generation: u64) {
        let decisions: Vec<(ConfirmationContext, CapabilityRecord)> = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let expired: Vec<(String, Capability)> = pending
                .iter()
                .filter(|(_, ctx)| now >= ctx.deadline)
                .map(|(slot, _)| slot.clone())
                .collect();
            let mut decisions = Vec::new();
            for slot in expired {
                let Some(ctx) = pending.remove(&slot) else {
                    continue;
                };
                // 旧代际：到点只摘除零落账（确认结果随代际一起作废）。
                if ctx.generation != current_generation {
                    continue;
                }
                decisions.push((ctx, inconclusive_record(slot.1)));
            }
            decisions
        };
        for (ctx, record) in decisions {
            self.write(&ctx, record);
        }
    }

    /// 落账（T2 账本 upsert；失败只 warn 不假报，spec R6）。日志只带能力名与
    /// 状态判据词，绝不带指纹/正文/凭据。
    fn write(&self, ctx: &ConfirmationContext, record: CapabilityRecord) {
        let Some(ledger) = self.ledger.as_ref() else {
            return;
        };
        if ledger.put(&ctx.key, &record).is_err() {
            log::warn!(
                "ZCode 能力确认落账失败（capability={} state={}），本结果不落账",
                ctx.capability.as_str(),
                state_word(record.state)
            );
        }
    }
}

/// confirmed 行（evidence 按调用方给的判据词落库；词由确认目标推导）。
fn confirmed_record(capability: Capability, evidence: &str) -> CapabilityRecord {
    let mut record = CapabilityRecord::confirmed(capability);
    record.evidence = Some(evidence.to_owned());
    record
}

/// 泵观察判据路径的 confirmed 判据词（按确认目标推导）。
fn confirmed_evidence(target: &ConfirmationTarget) -> &'static str {
    match target {
        ConfirmationTarget::UserMessage { .. } => confirmation_words::SEND_CONFIRMED,
        ConfirmationTarget::Selection { .. } => confirmation_words::SELECTION_CONFIRMED,
        ConfirmationTarget::Pending { .. } => confirmation_words::PENDING_CONFIRMED,
        ConfirmationTarget::Interrupt { .. } => confirmation_words::INTERRUPT_CONFIRMED,
    }
}

/// 回执因果终态路径的 confirmed 判据词：pending 类能力在这里只确证「宿主已撤卡」
/// （CommandSender 的 wait_host_cleared 观察），会话 revision 前进由泵判据负责
/// ——判据词如实只写撤卡，绝不冒写未验证的判据。interrupt 同理：回执终态只写
/// 停止终态直接确证的守卫消失（`wait_stop_cleared` 的会话水位基线是派发在途的
/// 写前复核值，不是登记时刻 baseline，不冒写 revision 前进）。
fn confirmed_receipt_evidence(target: &ConfirmationTarget) -> &'static str {
    match target {
        ConfirmationTarget::Pending { .. } => confirmation_words::PENDING_CLEARED,
        ConfirmationTarget::Interrupt { .. } => confirmation_words::INTERRUPT_CLEARED,
        _ => confirmed_evidence(target),
    }
}

/// inconclusive 行（默认判据词 = 确认窗到点）。
fn inconclusive_record(capability: Capability) -> CapabilityRecord {
    inconclusive_record_with(capability, confirmation_words::TIMEOUT)
}

fn inconclusive_record_with(capability: Capability, evidence: &str) -> CapabilityRecord {
    CapabilityRecord {
        capability,
        state: CapabilityState::Inconclusive,
        reason_code: None,
        evidence: Some(evidence.to_owned()),
    }
}

/// 回执原因码净化：只放行判据标识形状（与 T2 写入硬闸同一校验），否则用兜底码。
fn sanitized_reason(receipt: &ActionReceipt) -> String {
    receipt
        .reason_code
        .as_deref()
        .filter(|code| validate_evidence_token(code).is_ok())
        .map(str::to_owned)
        .unwrap_or_else(|| confirmation_words::ACTION_FAILED.to_owned())
}

/// 日志用的状态词（state 列冻结词表，与 dao 的落库词一致）。
fn state_word(state: CapabilityState) -> &'static str {
    match state {
        CapabilityState::Unsupported => "unsupported",
        CapabilityState::InterfaceMatched => "interface_matched",
        CapabilityState::Confirmed => "confirmed",
        CapabilityState::Degraded => "degraded",
        CapabilityState::Inconclusive => "inconclusive",
    }
}

/// Hub 依赖的窄后端端口（测试注入 fake；生产 [`PartsBackend`]）。
///
/// - `reset` 是 `config_changed` 的专用动作：断开 relay、按新代际作废投影；
///   **不再清账号 JWT**（换/删链接保留授权），与用户显式 `disconnect` 语义不同，
///   因此单列而不复用 `disconnect`；真正忘记授权走 `forget_authorization`。
/// - `restore_authorization`／`auth_status`／`forget_authorization` 是凭据生命周期口：
///   默认实现 = 未授权（旧 fake 零改动），生产 [`PartsBackend`] 委托 `AuthService`，
///   只暴露 [`AuthStatus`]，绝不暴露 JWT。
/// - `bind_session`／`session`／`execute` 的 `BoundTarget` 由 Hub 在当前代际内现绑，
///   后端不缓存目标，也不接受调用方自报的 workspace。
pub trait HubBackend: Send + Sync {
    /// 走一次官方授权（打开系统浏览器 → 回环回调 → JWT 只入内存）。
    fn authorize<'a>(
        &'a self,
        provider: OAuthProvider,
        open: OpenAuthorize,
    ) -> BoxFuture<'a, ZResult<()>>;

    /// 建立唯一物理连接（幂等复用见 `RelayClient::connect`）。
    fn connect<'a>(&'a self, route: DeviceRoute) -> BoxFuture<'a, ZResult<ConnectionView>>;

    /// 显式断开（保留 JWT）。
    fn disconnect<'a>(&'a self) -> BoxFuture<'a, ()>;

    /// 清投影并按代际作废（配置保存/删除成功后的唯一清理口；**保留账号 JWT**）。
    fn reset<'a>(&'a self, generation: u64) -> BoxFuture<'a, ()>;

    /// 启动/显式重试时恢复已保存授权（默认：未授权；生产委托 `AuthService::restore`）。
    ///
    /// 实现不得自动打开浏览器，也不得返回 JWT——只回 [`AuthStatus`]。
    fn restore_authorization<'a>(&'a self) -> BoxFuture<'a, AuthStatus> {
        Box::pin(async { self.auth_status() })
    }

    /// 只读授权状态快照（默认未授权；旧 fake 零改动，生产委托 `AuthService::status`）。
    fn auth_status(&self) -> AuthStatus {
        AuthStatus::default()
    }

    /// 忘记授权：清内存并删除持久记录（默认无操作；生产委托 `AuthService::forget`）。
    ///
    /// 删除失败必须如实体现在返回的 [`AuthStatus::problem`]（如 `DeleteFailed`），
    /// 且不得让旧记录复活。
    fn forget_authorization<'a>(&'a self) -> BoxFuture<'a, AuthStatus> {
        Box::pin(async { self.auth_status() })
    }

    /// 按 Tuvis sessionId 精确绑定当前代际的官方会话。
    fn bind_session<'a>(&'a self, key: &'a TargetKey) -> BoxFuture<'a, ZResult<BoundTarget>>;

    /// 读取精确目标的只读投影。
    fn session<'a>(&'a self, target: &'a BoundTarget) -> BoxFuture<'a, ZResult<SessionView>>;

    /// 派发一次写动作（目标已由 Hub 绑定并校验代际）。
    fn execute<'a>(
        &'a self,
        ctx: &'a ActionContext,
        target: &'a BoundTarget,
        action: &'a ControlAction,
    ) -> BoxFuture<'a, ZResult<ActionReceipt>>;

    /// 只读查询持久账本里的既有回执（**先于**连接/能力闸与目标绑定）。
    ///
    /// 语义与派发路径的幂等命中一致：同 actionId 同 device/session/generation/kind/
    /// payload_hash → 既有回执；任一不同 → `action_payload_conflict`；无行 → `None`。
    /// 实现必须是**纯读**：零出站帧、零新行、零审计。
    ///
    /// 默认 `Ok(None)`：没有持久账本的后端与既有测试 fake 零改动；生产
    /// [`PartsBackend`] 委托 `CommandSender::existing_receipt`。
    fn existing_receipt<'a>(
        &'a self,
        ctx: &'a ActionContext,
        action: &'a ControlAction,
    ) -> BoxFuture<'a, ZResult<Option<ActionReceipt>>> {
        let _ = (ctx, action);
        Box::pin(async { Ok(None) })
    }

    /// 本动作在**宿主卡语义**下需要的全部能力名（只读；不消费许可）。
    ///
    /// 生产 [`PartsBackend`] 委托 `CommandSender::required_capabilities`（Answer/
    /// Snooze 会读取精确目标上的宿主 pending 卡题型）。默认实现只覆盖静态动作；
    /// `Answer`/`Snooze` 无法在没有宿主卡时推导，一律 `not_ready`——绝不按客户端
    /// 自报的选中值数量猜测，也绝不因缺少能力证据而放行。
    fn required_capabilities<'a>(
        &'a self,
        target: &'a BoundTarget,
        action: &'a ControlAction,
    ) -> BoxFuture<'a, ZResult<Vec<&'static str>>> {
        let _ = target;
        Box::pin(async move {
            match action {
                ControlAction::Send { .. } => Ok(vec!["send"]),
                ControlAction::Stop { .. } => Ok(vec!["interrupt"]),
                ControlAction::Permission { .. } => Ok(vec!["approval_reply"]),
                ControlAction::Selection { .. } => Ok(vec!["model", "thought_level"]),
                ControlAction::Answer { .. } | ControlAction::Snooze { .. } => {
                    Err(ZcodeError::of(codes::NOT_READY))
                }
            }
        })
    }

    /// 带**验收许可**的派发（T8.2 debug 证据构建专用；普通构建永远不会传入非空
    /// `permits`）。默认实现忽略许可并退回 [`Self::execute`]，保持既有 fake 零改动。
    ///
    /// `permits` 只含 Hub 已按会话/代际/能力校验并消费过的能力名；实现方只允许用
    /// 它放宽**能力闸**一项，目标/权限 option kind/`fullAccess`/pending/stop 等守卫
    /// 必须原样执行。
    fn execute_with_permits<'a>(
        &'a self,
        ctx: &'a ActionContext,
        target: &'a BoundTarget,
        action: &'a ControlAction,
        permits: &'a [&'a str],
    ) -> BoxFuture<'a, ZResult<ActionReceipt>> {
        let _ = permits;
        self.execute(ctx, target, action)
    }

    /// 本后端的证据能力矩阵（生产默认全关）。
    fn policy(&self) -> CapabilityView;

    /// 动作提交路径的确认 baseline（T5/T6 首次真实动作确认；send/model/thought/
    /// 问答/审批登记）。
    ///
    /// 返回 `None` = 后端不提供 baseline（本次动作跳过确认登记）。生产
    /// [`PartsBackend`] 用桥的**只读快照**（revision/当前选择/日志头指纹，零网络）
    /// 与 `policy_source` 同一拼法的 capability key 现算；默认 `None` 让既有
    /// fake 零改动。实现必须是纯读：零出站帧、零重订阅。
    // `private_interfaces`：`HubBackend` 的标称 pub 是 with_parts 测试构造的历史
    // 面；baseline 类型按 spec 纯核纪律是 crate 内类型（实际可用面即本 crate）。
    #[allow(private_interfaces)]
    fn confirmation_baseline<'a>(
        &'a self,
        session_id: &'a str,
    ) -> BoxFuture<'a, Option<ConfirmationBaseline>> {
        let _ = session_id;
        Box::pin(async { None })
    }
}

/// 事件泵所需的桥窄口（生产 = [`BridgeManager`]，测试注入 fake）。
///
/// 只含 `on_data`/`invalidate` 两个入口：泵因此拿不到 relay 生命周期或配置入口。
/// `on_data` 返回 [`DataOutcome`]（本帧是否吸收 + **发生变化的会话视图**，桥侧前后
/// 快照对比）：即使本帧报错，降级/恢复视图也必须带出来，泵据此发布 `Session` 事件，
/// 而不是只让桥内投影悄悄前进。
pub(crate) trait BridgePump: Send + Sync {
    fn on_data<'a>(&'a self, payload: Value) -> BoxFuture<'a, DataOutcome>;
    /// 作废旧桥/投影（实现内部推进自己的投影代际，不接受外部代际，避免代际空间混用）。
    fn invalidate<'a>(&'a self) -> BoxFuture<'a, ()>;

    /// T5 确认观察：精确会话的日志头指纹（send 确认的「新用户消息可见」面）。
    /// 默认 `None`（无指纹证据 → send 确认保守不升级）；测试 fake 零改动。
    fn log_fingerprint(&self, session_id: &str) -> Option<String> {
        let _ = session_id;
        None
    }
}

/// 事件泵装配口：relay 事件接收端 + 泵桥 + 可选 Data 溢出观察口（测试可缺省）。
type PumpParts = (
    mpsc::Receiver<RelayEvent>,
    Arc<dyn BridgePump>,
    Option<RelayLag>,
);

/// 生产泵口：与 [`PartsBackend`] 共用同一个投影代际计数器。
struct ProjectionBridge {
    bridge: Arc<BridgeManager>,
    generation: Arc<AtomicU64>,
}

impl BridgePump for ProjectionBridge {
    fn on_data<'a>(&'a self, payload: Value) -> BoxFuture<'a, DataOutcome> {
        Box::pin(async move { BridgeManager::on_data_outcome(&self.bridge, payload).await })
    }

    fn invalidate<'a>(&'a self) -> BoxFuture<'a, ()> {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move { BridgeManager::invalidate(&self.bridge, generation).await })
    }

    fn log_fingerprint(&self, session_id: &str) -> Option<String> {
        self.bridge.log_fingerprint(session_id)
    }
}

/// 生产后端：一条 relay + 一个 bridge + 一个命令派发器（进程内单实例）。
pub struct PartsBackend {
    auth: Arc<AuthService>,
    relay: Arc<RelayClient>,
    bridge: Arc<BridgeManager>,
    sender: Arc<CommandSender>,
    /// 能力策略的**唯一**现算源：宿主证据 + DB 能力账本 → capability resolver。
    /// 会话矩阵（Hub）与执行闸（CommandSender）共用同一实例，禁止各存一份快照。
    policy_source: CapabilitySource,
    /// T2 能力账本实例（resolver 与 Hub 首次确认落账共用同一份，不建第二个库）。
    ledger: Arc<dyn CapabilityLedger>,
    /// 投影代际：`reset`/重新连接/冲突时单调推进，保证 `BridgeManager::invalidate`
    /// 一定生效（事件泵与后端共用同一个计数器）。
    projection_generation: Arc<AtomicU64>,
}

impl PartsBackend {
    /// 组装生产链（不触发 OAuth、不自动连接），返回 relay 事件接收端与 Data 溢出观察口
    /// 供 Hub 事件泵消费。
    ///
    /// 生产 `AuthService` **显式注入** `OsCredentialStore`（设计 §4.2）：构造本身不访问
    /// 系统保险箱，只把同一个 `DeviceStore` 交给凭据适配口；恢复只在显式
    /// `restore_authorization`/自动连接尝试时发生。
    fn production(store: DeviceStore) -> (Self, mpsc::Receiver<RelayEvent>, RelayLag) {
        let auth = Arc::new(
            AuthService::new(reqwest::Client::new())
                .with_credential_store(Arc::new(OsCredentialStore::new(store.clone()))),
        );
        Self::assemble_parts(store, auth, None, None)
    }

    /// 测试专用生产装配：复用测试里**真实**授权完成的 [`AuthService`]（同一 JWT 不
    /// 二次 OAuth），只把 relay 端点换成本机回环假官方（[`RelayClient::with_endpoint`]）。
    /// Auth/Relay/Bridge/CommandSender 全部是生产实现，端点之外零替身。
    #[cfg(test)]
    pub(crate) fn production_test(
        store: DeviceStore,
        auth: Arc<AuthService>,
        ws_origin: String,
    ) -> (Self, mpsc::Receiver<RelayEvent>, RelayLag) {
        Self::assemble_parts(store, auth, Some(ws_origin), None)
    }

    /// 测试专用生产装配（T9 未知 build e2e）：同 [`Self::production_test`]，另注入
    /// 宿主 build 探针——真实探针读本机安装 APP 的 plist，跨机器/跨时间不可复现，
    /// 「未知 build」用例必须确定性注入才能钉住 resolver 的接口候选路径。
    #[cfg(test)]
    pub(crate) fn production_test_with_host_build(
        store: DeviceStore,
        auth: Arc<AuthService>,
        ws_origin: String,
        host_build: HostBuildProbe,
    ) -> (Self, mpsc::Receiver<RelayEvent>, RelayLag) {
        Self::assemble_parts(store, auth, Some(ws_origin), Some(host_build))
    }

    /// 生产装配的唯一实现：`ws_origin = None` 恒走固定官方端点
    /// （[`RelayClient::new`]）；回环端点只在 `#[cfg(test)]` 下可注入。
    /// `host_build` 只在测试目标下生效（见 [`bridge_with_host_build`]）。
    fn assemble_parts(
        store: DeviceStore,
        auth: Arc<AuthService>,
        ws_origin: Option<String>,
        host_build: Option<HostBuildProbe>,
    ) -> (Self, mpsc::Receiver<RelayEvent>, RelayLag) {
        // 泵由 Hub 持有：State 溢出由 relay 无超时等待消费端（终态不丢），Data 溢出只
        // 置位 lag，由事件泵作废旧投影并让下一次 GET 重新 bind/现读（绝不阻塞 relay actor）。
        let (events, events_rx) = mpsc::channel(RELAY_EVENT_CAPACITY);
        let relay = Arc::new(relay_for(auth.clone(), events, ws_origin));
        let lag = relay.lag_watcher();
        let bridge = Arc::new(bridge_with_host_build(
            BridgeManager::new(relay.clone()),
            host_build,
        ));
        // 单一策略源（T4）：宿主证据（build/协议 → 能力 key）+ 接口形状 + DB 能力
        // 账本 → capability resolver；解析路径异常由 resolver 兜底回退
        // `production_unverified()` 全关（spec §6.6），账本读失败退化为
        // shipped + 接口候选（不阻塞连接）。
        // sender 与会话 DTO 共用同一 source：证据变化后两边同时翻转。
        // 账本实例挂在后端上（T5 首次确认落账与 resolver 共用同一份）。
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let policy_source: CapabilitySource = {
            let bridge = bridge.clone();
            let ledger = ledger.clone();
            Arc::new(move || {
                let evidence = bridge.host_evidence();
                resolve_capabilities(
                    &capability_key(&evidence),
                    &evidence.interface_evidence(),
                    ledger.as_ref(),
                )
            })
        };
        let sender = Arc::new(
            CommandSender::new(bridge.clone(), store)
                .with_capability_source(policy_source.clone())
                .with_pending(bridge.clone())
                .with_stop(bridge.clone())
                .with_selection(bridge.clone()),
        );
        (
            Self {
                auth,
                relay,
                bridge,
                sender,
                policy_source,
                ledger,
                projection_generation: Arc::new(AtomicU64::new(0)),
            },
            events_rx,
            lag,
        )
    }

    /// Hub 与会话能力矩阵使用的同一策略源（clone 只复制 `Arc`，不是矩阵快照）。
    /// `pub(crate)`：T9 e2e（wire_e2e）用生产同源 resolver 装配 Hub 时取同一实例。
    pub(crate) fn policy_source(&self) -> CapabilitySource {
        self.policy_source.clone()
    }

    /// T2 能力账本实例（Hub 首次确认落账与 resolver 共用同一份）。
    /// `pub(crate)`：T9 e2e（wire_e2e）装配 Hub 时把同一账本接进确认流程。
    pub(crate) fn capability_ledger(&self) -> Arc<dyn CapabilityLedger> {
        self.ledger.clone()
    }

    /// 事件泵的桥端（与后端共用同一个 `BridgeManager` 与投影代际）。
    pub(crate) fn pump_bridge(&self) -> Arc<dyn BridgePump> {
        Arc::new(ProjectionBridge {
            bridge: self.bridge.clone(),
            generation: self.projection_generation.clone(),
        })
    }

    fn advance_projection(&self) -> u64 {
        self.projection_generation.fetch_add(1, Ordering::SeqCst) + 1
    }
}

/// 装配形态 → relay 构造：生产恒固定官方端点；只有 `#[cfg(test)]` 且显式给出来源时
/// 才走回环端点注入（生产参数恒 `None`）。
fn relay_for(
    auth: Arc<AuthService>,
    events: mpsc::Sender<RelayEvent>,
    ws_origin: Option<String>,
) -> RelayClient {
    match ws_origin {
        #[cfg(test)]
        Some(origin) => RelayClient::with_endpoint(auth, events, origin),
        _ => RelayClient::new(auth, events),
    }
}

/// 装配形态 → 宿主 build 探针：只在测试目标下把探针注入桥（`with_host_build` 是
/// `#[cfg(test)]` 缝）；生产目标原样返回默认探针（读本机安装 APP，注入参数恒 `None`）。
#[cfg(test)]
fn bridge_with_host_build(
    bridge: BridgeManager,
    host_build: Option<HostBuildProbe>,
) -> BridgeManager {
    match host_build {
        Some(probe) => bridge.with_host_build(probe),
        None => bridge,
    }
}

#[cfg(not(test))]
fn bridge_with_host_build(
    bridge: BridgeManager,
    host_build: Option<HostBuildProbe>,
) -> BridgeManager {
    let _ = host_build;
    bridge
}

impl HubBackend for PartsBackend {
    fn authorize<'a>(
        &'a self,
        provider: OAuthProvider,
        open: OpenAuthorize,
    ) -> BoxFuture<'a, ZResult<()>> {
        Box::pin(async move { self.auth.authorize(provider, open).await })
    }

    fn connect<'a>(&'a self, route: DeviceRoute) -> BoxFuture<'a, ZResult<ConnectionView>> {
        Box::pin(async move {
            let view = self.relay.connect(route).await?;
            // 新连接：旧桥/投影一律作废，绑定只在当前投影代际内有效。
            self.bridge.invalidate(self.advance_projection()).await;
            Ok(view)
        })
    }

    fn disconnect<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.relay.disconnect().await;
            self.bridge.invalidate(self.advance_projection()).await;
        })
    }

    fn reset<'a>(&'a self, _generation: u64) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            // 换/删链接保留账号 JWT：只断 relay、作废旧桥/投影，绝不清 AuthService 内存，
            // 也不删除持久记录（真正忘记授权走 `forget_authorization`）。
            // 迟到 token 结果仍由 AuthService 自己的 credential epoch 作废。
            //
            // 本实现**不**用代际参数做 owner 校验（保持既有语义）：清理归属由 Hub 层
            // 负责——`config_changed` 在 `disconnect_lock` 内复核 `CloseGuard` 仍是当前
            // 窗口后才调用本方法，陈旧 owner 绝不触达这里。
            self.relay.disconnect().await;
            self.bridge.invalidate(self.advance_projection()).await;
        })
    }

    fn restore_authorization<'a>(&'a self) -> BoxFuture<'a, AuthStatus> {
        Box::pin(async move { self.auth.restore().await })
    }

    fn auth_status(&self) -> AuthStatus {
        self.auth.status()
    }

    fn forget_authorization<'a>(&'a self) -> BoxFuture<'a, AuthStatus> {
        Box::pin(async move { self.auth.forget().await })
    }

    fn bind_session<'a>(&'a self, key: &'a TargetKey) -> BoxFuture<'a, ZResult<BoundTarget>> {
        Box::pin(async move { self.bridge.bind(key).await })
    }

    fn session<'a>(&'a self, target: &'a BoundTarget) -> BoxFuture<'a, ZResult<SessionView>> {
        Box::pin(async move { self.bridge.session(target).await })
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a ActionContext,
        target: &'a BoundTarget,
        action: &'a ControlAction,
    ) -> BoxFuture<'a, ZResult<ActionReceipt>> {
        Box::pin(async move { self.sender.execute(ctx, target, action).await })
    }

    fn existing_receipt<'a>(
        &'a self,
        ctx: &'a ActionContext,
        action: &'a ControlAction,
    ) -> BoxFuture<'a, ZResult<Option<ActionReceipt>>> {
        Box::pin(async move { self.sender.existing_receipt(ctx, action).await })
    }

    fn required_capabilities<'a>(
        &'a self,
        target: &'a BoundTarget,
        action: &'a ControlAction,
    ) -> BoxFuture<'a, ZResult<Vec<&'static str>>> {
        Box::pin(async move { self.sender.required_capabilities(target, action).await })
    }

    fn execute_with_permits<'a>(
        &'a self,
        ctx: &'a ActionContext,
        target: &'a BoundTarget,
        action: &'a ControlAction,
        permits: &'a [&'a str],
    ) -> BoxFuture<'a, ZResult<ActionReceipt>> {
        Box::pin(async move {
            self.sender
                .execute_with_permits(ctx, target, action, permits)
                .await
        })
    }

    fn policy(&self) -> CapabilityView {
        // 现算（不是构造时快照）：运行时证据变化立即反映到 Hub 闸门与手机矩阵。
        (self.policy_source)()
    }

    // 同 trait 侧：baseline 类型是 crate 内类型（见 trait 方法注）。
    #[allow(private_interfaces)]
    fn confirmation_baseline<'a>(
        &'a self,
        session_id: &'a str,
    ) -> BoxFuture<'a, Option<ConfirmationBaseline>> {
        Box::pin(async move {
            // capability key 与 policy_source 同一拼法（同证据稳定）；
            // 会话侧 baseline 来自桥的只读快照（零网络、零重订阅）。
            let observation = self.bridge.confirmation_observation(session_id);
            Some(ConfirmationBaseline {
                key: capability_key(&self.bridge.host_evidence()),
                observation: observation.unwrap_or(ConfirmationObservation {
                    revision: None,
                    provider: None,
                    log_fingerprint: None,
                    active_works: None,
                }),
            })
        })
    }
}

fn lock_inner(inner: &Mutex<Inner>) -> std::sync::MutexGuard<'_, Inner> {
    inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 「已配置 + 是否已授权」对应的展示态（配置变更第一/末段的唯一映射口径）。
fn configured_state(configured: bool, authorized: bool) -> ConnectionState {
    if !configured {
        ConnectionState::NeedsConfig
    } else if authorized {
        ConnectionState::Offline
    } else {
        ConnectionState::NeedsAuth
    }
}

/// 后端 `bind_session` 只产出桥投影代际；公开面一律用 Hub 代际：
/// 原值收进 `bridge_generation`（桥内 call/pending/stop/selection 对拍），
/// `generation` 覆写为 Hub 代际（CommandSender 校验/落账/回执用公开代际）。
fn public_target(mut target: BoundTarget, generation: u64) -> BoundTarget {
    let bridge_generation = target.bridge_generation.unwrap_or(target.generation);
    target.bridge_generation = Some(bridge_generation);
    target.generation = generation;
    target
}

/// 把一条有效验收许可覆盖到会话能力矩阵（**仅 debug 证据构建**）。
///
/// 只把该能力一项置 `enabled=true` / `reasonCode=None`；其余能力保持基线全关。
/// 未知能力名不做任何修改（返回 `false`）。本函数不编入普通/发布构建。
#[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
fn overlay_permit(view: &mut CapabilityView, capability: &str) -> bool {
    let flag = match capability {
        "send" => &mut view.send,
        "single_select" => &mut view.single_select,
        "multi_select" => &mut view.multi_select,
        "free_text" => &mut view.free_text,
        "interrupt" => &mut view.interrupt,
        "approval_reply" => &mut view.approval_reply,
        "model" => &mut view.model,
        "thought_level" => &mut view.thought_level,
        _ => return false,
    };
    flag.enabled = true;
    flag.reason_code = None;
    true
}

/// 能力名 → 策略矩阵里的单项开关（未知名字返回 `None`，一律按能力关闭处理）。
fn capability_flag<'a>(
    policy: &'a CapabilityView,
    capability: &str,
) -> Option<&'a super::types::CapabilityFlag> {
    Some(match capability {
        "send" => &policy.send,
        "single_select" => &policy.single_select,
        "multi_select" => &policy.multi_select,
        "free_text" => &policy.free_text,
        "interrupt" => &policy.interrupt,
        "approval_reply" => &policy.approval_reply,
        "model" => &policy.model,
        "thought_level" => &policy.thought_level,
        _ => return None,
    })
}

/// 宿主卡题型能力名 → 问答确认能力（`required_capabilities` 词表的确认侧逆映射）。
///
/// `approval_reply` **不在**本映射内（spec §4.1：审批确认只来自 Permission 动作）
/// ——Answer 动作指向 permission 卡是客户端形状错误（build_user_answer 会拒绝），
/// 其失败不构成审批能力证据。send/interrupt/model/thought_level 同样不在 pending
/// 确认面上，返回 `None`。
fn answer_capability(name: &str) -> Option<Capability> {
    match name {
        "single_select" => Some(Capability::SingleSelect),
        "multi_select" => Some(Capability::MultiSelect),
        "free_text" => Some(Capability::FreeText),
        _ => None,
    }
}

/// 广播一条控制事件（先取号再发送；调用方不得持内锁）。
///
/// `device_id` 只留在进程内供 SSE 过滤同会话动作（`ControlEvent` 序列化时跳过），
/// 连接/会话事件的调用方一律传 `None`。
fn emit_event(
    inner: &Mutex<Inner>,
    events: &broadcast::Sender<ControlEvent>,
    kind: ControlEventKind,
    generation: u64,
    session_id: Option<String>,
    device_id: Option<String>,
    data: Value,
) {
    let seq = {
        let mut state = lock_inner(inner);
        state.seq = state.seq.wrapping_add(1);
        state.seq
    };
    let _ = events.send(ControlEvent {
        seq,
        generation,
        session_id,
        kind,
        data,
        device_id,
    });
}

/// 广播一条连接事件。
fn emit_connection(
    inner: &Mutex<Inner>,
    events: &broadcast::Sender<ControlEvent>,
    view: &ConnectionView,
) {
    emit_event(
        inner,
        events,
        ControlEventKind::Connection,
        view.generation,
        None,
        None,
        serde_json::to_value(view).unwrap_or(Value::Null),
    );
}

/// 广播一条会话投影事件（本会话；设备维度为 None）。
fn emit_session_event(
    inner: &Mutex<Inner>,
    events: &broadcast::Sender<ControlEvent>,
    view: &SessionView,
) {
    emit_event(
        inner,
        events,
        ControlEventKind::Session,
        view.generation,
        Some(view.session_id.clone()),
        None,
        serde_json::to_value(view).unwrap_or(Value::Null),
    );
}

/// [`apply_relay_state`] 应用一帧 relay 状态后，调用方需要执行的外部副作用。
///
/// 把副作用从状态机里分离：泵只按这里的标志执行通知，绝不把“是否重连”的判断
/// 塞回 relay 层。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct RelayStateEffects {
    /// 本次**进入** KICKED 冲突（调用方持久暂停自动连接并取消待重试轮次）。
    entered_conflict: bool,
    /// 本次是 `paired → offline` 的**真实掉线**（调用方唤醒自动连接开新轮）。
    ///
    /// 只认这一个状态对：worker 自身连接失败走的是 `connecting → offline`（或
    /// `needs_auth → offline`），绝不触发自动重连预算重置。
    dropped_from_paired: bool,
}

/// 事件泵的 State 处理：锁内改状态/代际，锁外作废旧桥投影并广播。
///
/// - `conflict`（KICKED）是终态：推进 Hub 控制代际（旧动作立即 `stale_generation`），
///   作废桥/投影后广播；`entered_conflict` 表示**本次进入**冲突（调用方据此持久暂停
///   自动连接，绝不继续自动重连）；
/// - 真实掉线（覆盖前 `paired`、新状态 `offline`）单独标记 `dropped_from_paired`：
///   调用方唤醒自动连接开新轮；worker 自身失败（覆盖前 `connecting` 等）不标记；
/// - `needs_auth` 同时清 `authorized`（JWT 已失效）；
/// - Hub 处于 `needs_config`/`needs_auth` 时只接受 `needs_auth`/`conflict`：
///   配置变更后的旧连接离线/握手事件不得把 Hub 拉回 `offline`/`connecting`；
/// - 关闭窗口（`closing` 置位）内一律忽略：旧 attempt 的 relay 状态不得回写
///   `begin_disconnect` 已同步落定的 Offline；
/// - `generation <= relay_floor` 的迟到状态一律丢弃。
async fn apply_relay_state(
    inner: &Mutex<Inner>,
    events: &broadcast::Sender<ControlEvent>,
    bridge: &dyn BridgePump,
    view: ConnectionView,
) -> RelayStateEffects {
    let (out, effects, stale) = {
        let mut state = lock_inner(inner);
        // 关闭窗口：`begin_disconnect` 已在锁内同步落定 Offline。旧 attempt 的迟到
        // relay 状态（Connecting/Paired/Offline 乃至 Conflict/NeedsAuth）一律忽略，
        // 关闭态在窗口内是唯一权威；关闭结束后的新连接自会带来新 relay 代际。
        if state.closing.is_some() {
            return RelayStateEffects::default();
        }
        if view.generation <= state.relay_floor {
            return RelayStateEffects::default();
        }
        state.relay_generation = state.relay_generation.max(view.generation);
        let conflict = view.state == ConnectionState::Conflict;
        let entering_conflict = conflict && state.state != ConnectionState::Conflict;
        if entering_conflict {
            // 只在进入冲突时推进一次（重复冲突事件不再推代际）。
            state.generation = state.generation.wrapping_add(1);
        } else if matches!(
            state.state,
            ConnectionState::NeedsConfig | ConnectionState::NeedsAuth
        ) && view.state != ConnectionState::NeedsAuth
        {
            return RelayStateEffects::default();
        }
        // 覆盖前判定真实掉线：只认 paired → offline。worker 自身失败的 relay 状态是
        // connecting → offline（或 needs_auth → offline），不会触发自动重连。
        let effects = RelayStateEffects {
            entered_conflict: entering_conflict,
            dropped_from_paired: state.state == ConnectionState::Paired
                && view.state == ConnectionState::Offline,
        };
        state.state = view.state;
        state.reason_code = view.reason_code;
        if view.state != ConnectionState::Connecting {
            // 非 connecting 的落定 = 本 attempt 的新状态：`connect` 据它判断迟到的
            // 握手结果是否已被事件泵取代（新状态胜出，绝不回写假 paired）。
            state.state_epoch = state.state_epoch.wrapping_add(1);
        }
        if view.device_label.is_some() {
            state.device_label = view.device_label;
        }
        if view.state == ConnectionState::NeedsAuth {
            state.authorized = false;
        }
        let out = state.view();
        let stale = if out.state == ConnectionState::Paired {
            Vec::new()
        } else {
            state.stale_sessions(out.generation)
        };
        (out, effects, stale)
    };
    if effects.entered_conflict {
        bridge.invalidate().await;
    }
    emit_connection(inner, events, &out);
    for stale in &stale {
        emit_session_event(inner, events, stale);
    }
    effects
}

/// 事件泵的 Data 溢出处理：旧投影**显式失效**，绝不把旧缓存继续当权威。
///
/// relay 侧对 Data 只置位 [`RelayLag`] + 唤醒（不阻塞 actor），因此泵在此承担「失效」
/// 语义：先作废桥内投影（下一次 GET 必然重新 bind/现读），再在 paired 时把**每个**
/// 已发布过的跟踪会话降级为 `ready=false`、能力回到基线全关，并**逐会话**在锁外广播
/// `Session` 事件——手机（可能不止一台、不止一个会话）据它丢弃卡面，下一次 GET 重新
/// 取数。已全部降级时不再重复发事件；**不**为 `last_session` 单独补发（生产上每次
/// 成功 GET 都会登记跟踪表，它只是表内普通一员）。不伪造任何数据内容。
async fn apply_relay_lag(
    inner: &Mutex<Inner>,
    events: &broadcast::Sender<ControlEvent>,
    bridge: &dyn BridgePump,
) {
    // 先作废桥投影：旧缓存不得在失效窗口内被继续读取。
    bridge.invalidate().await;
    let stale = {
        let mut state = lock_inner(inner);
        if state.state != ConnectionState::Paired {
            return;
        }
        // 只降级仍 ready 的跟踪会话：全部已降级 = 无变化 → 零事件（重复 lag 不重发）。
        state.downgrade_tracked_sessions()
    };
    for view in &stale {
        emit_session_event(inner, events, view);
    }
}

/// 事件泵的 Data 会话变化：锁内校验代际/配对并落 `last_session` 与跟踪表，锁外广播。
///
/// - `hub_generation` 是本帧进入桥之前的 Hub 代际：桥处理在途若发生换代（配置变更/
///   断开/冲突），旧代际的投影绝不得贴上新代际，一律丢弃；
/// - `view.generation` 是**桥投影代际**，仅作诊断（不参与对外比较）：对外事件恒用
///   Hub 代际；
/// - 能力矩阵：桥投影不携带 Hub 能力（基线全关，证据层由 Hub 在 GET 时现算）。只有
///   `last_session` 恰好是同一会话时才沿用它的矩阵（GET 时现算的权威值），否则回到
///   基线全关——绝不把别的会话或桥投影的能力扣到本会话上（不伪造开闸）。
///
/// 调用方不得持内锁调用：本函数自己取锁，且广播在锁外进行。
fn apply_relay_session(
    inner: &Mutex<Inner>,
    events: &broadcast::Sender<ControlEvent>,
    hub_generation: u64,
    mut view: SessionView,
) {
    let view = {
        let mut state = lock_inner(inner);
        if state.generation != hub_generation || state.state != ConnectionState::Paired {
            return;
        }
        view.generation = state.generation;
        view.capabilities = if view.ready {
            state
                .last_session
                .as_ref()
                .filter(|last| last.session_id == view.session_id)
                .map(|last| last.capabilities.clone())
                .unwrap_or_default()
        } else {
            // 降级视图（坏帧/解码/序号错误）：绝不沿用 last_session 里的旧开闸矩阵，
            // 否则手机在 ready=false 的投影上仍显示可点的控制面。
            CapabilityView::default()
        };
        state.last_session = Some(view.clone());
        // 本会话也可能被别的手机 GET 过：变化同样登记，lag 才能逐会话降级。
        state.track_session(&view);
        view
    };
    emit_session_event(inner, events, &view);
}

/// Hub 内部状态（全部字段由同一把 std 锁保护，锁外不做网络 await）。
struct Inner {
    /// 控制代际：配置保存/删除与显式断开推进；动作/目标必须匹配它。
    generation: u64,
    /// 本机是否保存了格式合法的官方链接（读自 settings）。
    configured: bool,
    /// 本进程是否持有账号 JWT（只在内存；重启即失效）。
    authorized: bool,
    /// 关闭窗口的持有 epoch（`None` = 无关闭在途）：`begin_disconnect` 在锁内置为
    /// 本次代际；对应的 [`CloseGuard`] Drop 时**仅当仍匹配本 epoch** 才清除。
    /// 置位期间 `connect`/relay 状态回写一律拒绝（关闭已同步落定 Offline）。
    closing: Option<u64>,
    state: ConnectionState,
    reason_code: Option<String>,
    device_label: Option<String>,
    /// 事件序号（单调递增，发现缺口用）。
    seq: u64,
    /// 事件泵落定非 `connecting` 状态的纪元：`connect` 用它识别「握手期间已被
    /// 更新的状态取代」（新状态胜出）。
    state_epoch: u64,
    /// 已应用到 Hub 的 relay 事件代际（relay 自己的 generation，不是控制代际）。
    relay_generation: u64,
    /// 事件泵地板：`generation <= relay_floor` 的 relay 状态一律忽略（配置变更/断开
    /// 之后迟到的旧代际状态不得回写新状态）。
    relay_floor: u64,
    /// 最近一次经 `session()` 读取的投影：用于能力矩阵复用与 legacy 兜底；
    /// 逐会话降级/通知以 `tracked_sessions` 为准，本字段不再是唯一广播对象。
    last_session: Option<SessionView>,
    /// 已发布过的会话视图（按 sessionId 稳定排序、有界）：Data 溢出时**逐会话**
    /// 降级并各广播一条 `Session` 事件，不能只通知最后一个会话。
    tracked_sessions: BTreeMap<String, SessionView>,
    /// 跟踪表的**最后登记序**（LRU）：back = 最近登记/再次 GET，front = 最久未登记；
    /// 超限时按它淘汰（与 `tracked_sessions` 的 key 集合同步维护）。
    tracked_order: VecDeque<String>,
}

impl Inner {
    fn view(&self) -> ConnectionView {
        ConnectionView {
            state: self.state,
            generation: self.generation,
            reason_code: self.reason_code.clone(),
            device_label: self.device_label.clone(),
        }
    }

    /// 连接离开 paired：把**所有就绪的**跟踪会话如实降级为不新鲜（当前代际、
    /// `ready=false`、能力回基线全关），返回按 sessionId 稳定排序的降级视图（调用方
    /// 在锁外逐条广播），随后清空跟踪表——离开 paired 后旧桥的任何会话视图都不得在
    /// 重连后复活（`begin_disconnect`/`config_changed`/冲突与掉线都经由本入口）。
    ///
    /// 关键：relay **掉线不换代**（代际不变）时也必须逐会话通知；只发 connection 帧
    /// 时，停在别的会话的手机不一定重拉。`last_session` 只是兜底（它未进跟踪表且
    /// 仍 ready 的 legacy 状态），不是唯一广播对象。
    fn stale_sessions(&mut self, generation: u64) -> Vec<SessionView> {
        let mut changed = Vec::new();
        for view in self.tracked_sessions.values_mut() {
            if !view.ready {
                continue;
            }
            view.generation = generation;
            view.ready = false;
            // 投影已不新鲜：能力矩阵同步回到基线全关，绝不把旧代际的许可带进新代际事件。
            view.capabilities = CapabilityView::default();
            changed.push(view.clone());
        }
        if let Some(last) = self.last_session.as_mut() {
            let was_ready = last.ready;
            last.generation = generation;
            last.ready = false;
            last.capabilities = CapabilityView::default();
            let represented = changed
                .iter()
                .any(|view| view.session_id == last.session_id);
            // legacy/防御兜底：last_session 未跟踪且仍 ready（正常生产不可达，GET 恒会
            // 登记跟踪表）时补一条；已降级/已在变化集合内的绝不重复发。
            if was_ready && !represented {
                changed.push(last.clone());
            }
        }
        self.tracked_sessions.clear();
        self.tracked_order.clear();
        changed
    }

    /// 登记（有界，按**最后登记序** = LRU）已发布会话视图：每次成功 GET / Data 变化
    /// 都刷新同一条并把它移到登记序队尾；新会话超限时淘汰最久未登记（队首）的那个。
    fn track_session(&mut self, view: &SessionView) {
        match self
            .tracked_order
            .iter()
            .position(|id| *id == view.session_id)
        {
            // 已登记：同一会话再次 GET/变化 = 最近一次登记（移到队尾，不扩容）。
            Some(index) => {
                self.tracked_order.remove(index);
            }
            // 新会话：超限先淘汰最久未登记（队首）的会话，再入队尾。
            None => {
                if self.tracked_order.len() >= TRACKED_SESSION_CAP {
                    if let Some(oldest) = self.tracked_order.pop_front() {
                        self.tracked_sessions.remove(&oldest);
                    }
                }
            }
        }
        self.tracked_order.push_back(view.session_id.clone());
        self.tracked_sessions
            .insert(view.session_id.clone(), view.clone());
    }

    /// Data 溢出/投影失效：把所有**就绪的**跟踪会话降级为不新鲜（当前 Hub 代际、
    /// 能力回基线全关），写回跟踪表并同步 `last_session`，返回按 sessionId 稳定
    /// 排序的变化视图（调用方在锁外逐条广播）。
    fn downgrade_tracked_sessions(&mut self) -> Vec<SessionView> {
        let generation = self.generation;
        let mut changed = Vec::new();
        for view in self.tracked_sessions.values_mut() {
            if !view.ready {
                continue;
            }
            view.generation = generation;
            view.ready = false;
            // 投影已不新鲜：能力矩阵同步回到基线全关，绝不把旧代际的许可带进新事件。
            view.capabilities = CapabilityView::default();
            changed.push(view.clone());
        }
        if let Some(last) = self.last_session.as_mut() {
            last.generation = generation;
            last.ready = false;
            last.capabilities = CapabilityView::default();
        }
        changed
    }
}

/// 关闭窗口的 RAII 凭据：由 [`ZcodeHub::begin_disconnect`] **同步**创建并随返回值
/// 交出，不含任何 await 点。
///
/// 清理（[`ZcodeHub::finish_disconnect`]）消费它：正常结束即 Drop 清位；若清理
/// future 被取消 / panic / 从未被 poll，凭据随 future 一起 Drop——只要 `closing`
/// 仍是本次 epoch 就清位，绝不把 Hub 永久锁在「拒绝连接」态。重复 `begin` 会写入
/// 新 epoch，因此旧凭据 Drop 时不会误清新窗口。
#[must_use = "CloseGuard 必须交给 finish_disconnect，或作为取消语义被显式丢弃"]
pub(crate) struct CloseGuard {
    inner: Arc<Mutex<Inner>>,
    epoch: u64,
}

impl CloseGuard {
    /// 本凭据是否仍是当前关闭窗口（锁内查 `closing == Some(self.epoch)`）。
    ///
    /// 只有 current 的清理任务才拥有清理权：更新的 `begin_disconnect` 一旦接管，
    /// 旧凭据既不得清位，也不得再发 `backend.disconnect()`。
    fn is_current(&self) -> bool {
        lock_inner(&self.inner).closing == Some(self.epoch)
    }
}

impl Drop for CloseGuard {
    fn drop(&mut self) {
        let mut inner = lock_inner(&self.inner);
        if inner.closing == Some(self.epoch) {
            inner.closing = None;
        }
    }
}

/// 测试专用：KICKED 延迟写的两段闸（生产不编译）。
///
/// 事件到达时泵在 blocking 池上异步落库；真实竞态窗口（事件已捕获代际、用户写
/// 已落库、KICKED 写才拿到锁）无法用固定 sleep 稳定复现。这里让测试在**落库前**
/// 放行、在**落库后**收到完成信号，从而确定性验证「旧代 KICKED 写被跳过」。
#[cfg(test)]
#[derive(Default)]
struct KickedIntentProbe {
    /// 非空时：延迟写先等这里放行（测试取走后由 `recv` 挂住）。
    release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    /// 非空时：延迟写落库/跳过之后向这里回报完成。
    done: Mutex<Option<std::sync::mpsc::Sender<()>>>,
}

#[cfg(test)]
impl KickedIntentProbe {
    fn wait(&self) {
        let release = self.release.lock().unwrap().take();
        if let Some(release) = release {
            let _ = release.recv();
        }
    }

    fn finish(&self) {
        let done = self.done.lock().unwrap().take();
        if let Some(done) = done {
            let _ = done.send(());
        }
    }
}

/// 单实例 ZCode 控制 Hub（桌面与手机共享）。
pub struct ZcodeHub {
    store: DeviceStore,
    backend: Arc<dyn HubBackend>,
    /// 与 [`PartsBackend`] 共用的能力策略现算源：会话矩阵与执行闸同源。
    policy_source: CapabilitySource,
    events: broadcast::Sender<ControlEvent>,
    inner: Arc<Mutex<Inner>>,
    /// 连接单飞：并发 `connect` 只让一个真正开始新连接，其余复用当前视图。
    connect_lock: AsyncMutex<()>,
    /// 清理单飞：重叠关闭的 `finish_disconnect` 串行执行（后者不得越过前者的清理
    /// 窗口；陈旧 owner 在窗口外一律零副作用）。`connect` 绝不取本锁。
    disconnect_lock: AsyncMutex<()>,
    /// 抢回单飞（T8）：并发 `takeover_requested` 只让一个真正执行「断开→重连」，
    /// 其余等它收尾后按已落定的新状态走幂等/前置检查，绝不叠加第二轮流。
    takeover_lock: AsyncMutex<()>,
    /// 自动连接单 worker 的启动幂等标记（`start_auto_connect` 只成功一次）。
    auto_started: AtomicBool,
    /// 启动恢复是否已经尝试过（失败也算）：自动连接 worker 不再第二次读原生存储。
    ///
    /// 显式 `connect_requested`/用户点击重试不受此限，可直接调
    /// [`Self::restore_authorization`] 再次恢复。
    auto_restore_attempted: AtomicBool,
    /// 自动连接唤醒（单 worker 消费；多次触发合并为逐次尝试）。
    auto_notify: Arc<Notify>,
    /// `forget_authorization` 后的内存暂停（授权成功/显式连接恢复）。
    ///
    /// 持久意图另见 settings 的 `remote.zcode_auto_connect`；本标记只封住忘记期间
    /// 「旧 restore 抢在新删除之前复活内存凭据」的窗口，不跨进程。`Arc` 让 relay
    /// 事件泵在 KICKED 时也能同步置位并唤醒 worker，立即取消待重试轮次。
    auto_paused: Arc<AtomicBool>,
    /// 退出清理已开始（P2-5 退出残留）：置位后自动连接 worker **单调停机**——在途尝试
    /// 作废、退避轮取消、清位后也不再自行重连。只由退出路径置位，进程内不撤销
    /// （退出/升级安装都会终结进程）；远程停服/显式断开绝不置位。
    shutting_down: AtomicBool,
    /// 自动连接意图的写入代际（P2-6，单调递增）：用户意图写与 KICKED 延迟写共用。
    ///
    /// 用户写在 DB 锁内预占新代（此后恒为最新）；KICKED 在事件到达时预占一代，
    /// 延迟的 blocking 写携带该代。落库前在 DB 锁内复核代际，被更新的用户意图取代
    /// 时跳过——迟到的 KICKED 写绝不覆盖用户后来的 true/false。
    auto_intent_generation: Arc<AtomicU64>,
    /// 测试观察口：KICKED 延迟写的两段闸（见 [`KickedIntentProbe`]）。
    #[cfg(test)]
    kicked_intent_probe: Arc<KickedIntentProbe>,
    /// 测试观察口：`config_changed` 在 begin 之后、取清理串行锁之前的闸（P2-3 回归：
    /// 让更新的配置变更先完整完成并重连，再放行陈旧 owner）。
    #[cfg(test)]
    config_pre_reset_gate: Mutex<Option<Arc<Notify>>>,
    /// 测试观察口：自动连接 worker 已执行的尝试次数（证明「等 worker 跑完」而不是
    /// 靠固定 sleep；不影响生产行为）。
    #[cfg(test)]
    auto_attempts: AtomicU64,
    /// T8.2 debug 证据构建的单次验收许可册（进程内存；不落 settings/DB）。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    evidence: EvidencePermitStore,
    /// 首次真实动作确认的登记表（spec R4；send/model/thought 接线在 execute/事件泵）。
    confirmations: Arc<ConfirmationTracker>,
    /// 确认窗时长（生产 [`CONFIRMATION_TIMEOUT`]；测试注入短窗）。
    confirmation_timeout: Duration,
}

impl ZcodeHub {
    /// 生产构造：固定官方端点，不自动 OAuth、不自动连接；策略默认全关（`needs_evidence`）。
    ///
    /// 构造即启动一条 relay 事件泵（`Weak` 持有 Hub 状态：Hub 释放或事件通道关闭即退出），
    /// 因此无任何自动授权/自动连接副作用。必须在 Tokio runtime 内构造
    /// （`BridgeManager` 会 spawn 出站 actor）。
    pub fn new(store: DeviceStore) -> Self {
        let (parts, relay_events, lag) = PartsBackend::production(store.clone());
        // 生产唯一来源：宿主证据 + DB 能力账本 → capability resolver（异常回退全关）。
        let policy_source = parts.policy_source();
        // 确认落账与 resolver 用**同一个**账本实例（同一 DeviceStore，不建第二个库）。
        let ledger = parts.capability_ledger();
        let bridge = parts.pump_bridge();
        let backend: Arc<dyn HubBackend> = Arc::new(parts);
        Self::assemble(
            store,
            backend,
            policy_source,
            Some((relay_events, bridge, Some(lag))),
            Some(ledger),
            CONFIRMATION_TIMEOUT,
        )
    }

    /// 测试专用：注入 fake 后端与能力矩阵（不注册任何设置/IPC 入口）。
    ///
    /// 不设 `cfg(test)` 门：本仓库集成测试（`tests/`）以独立 crate 编译库，看不到
    /// 单元测试配置；生产装配只经 [`Self::new`]，本入口不向设置/IPC 暴露。
    #[doc(hidden)]
    pub fn with_parts(
        store: DeviceStore,
        backend: Arc<dyn HubBackend>,
        policy: CapabilityView,
    ) -> Self {
        Self::assemble(
            store,
            backend,
            static_policy_source(policy),
            None,
            None,
            CONFIRMATION_TIMEOUT,
        )
    }

    /// 测试专用：注入**动态**策略源，验证会话矩阵与执行闸来自同一来源。
    #[doc(hidden)]
    pub fn with_parts_and_policy_source(
        store: DeviceStore,
        backend: Arc<dyn HubBackend>,
        policy_source: CapabilitySource,
    ) -> Self {
        Self::assemble(
            store,
            backend,
            policy_source,
            None,
            None,
            CONFIRMATION_TIMEOUT,
        )
    }

    /// 事件泵测试构造（fake relay 通道 + fake bridge；零网络；无 lag 观察口）。
    #[cfg(test)]
    fn with_pump(
        store: DeviceStore,
        backend: Arc<dyn HubBackend>,
        policy: CapabilityView,
        events: mpsc::Receiver<RelayEvent>,
        bridge: Arc<dyn BridgePump>,
    ) -> Self {
        Self::assemble(
            store,
            backend,
            static_policy_source(policy),
            Some((events, bridge, None)),
            None,
            CONFIRMATION_TIMEOUT,
        )
    }

    /// 事件泵测试构造 + Data 溢出观察口（验证 lag → 投影失效接线）。
    #[cfg(test)]
    pub(crate) fn with_pump_and_lag(
        store: DeviceStore,
        backend: Arc<dyn HubBackend>,
        policy: CapabilityView,
        events: mpsc::Receiver<RelayEvent>,
        bridge: Arc<dyn BridgePump>,
        lag: RelayLag,
    ) -> Self {
        Self::assemble(
            store,
            backend,
            static_policy_source(policy),
            Some((events, bridge, Some(lag))),
            None,
            CONFIRMATION_TIMEOUT,
        )
    }

    /// 事件泵测试构造 + 能力确认账本注入（T5：首次真实动作确认的落账接线）。
    ///
    /// `confirmation_timeout` 可注入短窗（生产 [`CONFIRMATION_TIMEOUT`] 5s），
    /// 让超时 → inconclusive 的用例不等真实 5 秒。
    #[cfg(test)]
    fn with_pump_and_confirmation(
        store: DeviceStore,
        backend: Arc<dyn HubBackend>,
        policy: CapabilityView,
        events: mpsc::Receiver<RelayEvent>,
        bridge: Arc<dyn BridgePump>,
        ledger: Arc<dyn CapabilityLedger>,
        confirmation_timeout: Duration,
    ) -> Self {
        Self::assemble(
            store,
            backend,
            static_policy_source(policy),
            Some((events, bridge, None)),
            Some(ledger),
            confirmation_timeout,
        )
    }

    /// 事件泵测试构造 + 生产同源策略源与确认账本（T9 未知 build e2e）。
    ///
    /// 与生产 [`Self::new`] 的差别只在回环端点注入与探针来源：策略矩阵来自
    /// `PartsBackend` 装配的 **capability resolver**（shipped + DB 账本 + 接口形状），
    /// 确认落账与 resolver 共用同一份 DB 账本——e2e 据此证明「未知 build 接口候选
    /// 开放 + 首次真实动作确认落账」走的是生产解析路径，不是注入的静态矩阵。
    #[cfg(test)]
    pub(crate) fn with_pump_policy_source_and_confirmation(
        store: DeviceStore,
        backend: Arc<dyn HubBackend>,
        policy_source: CapabilitySource,
        events: mpsc::Receiver<RelayEvent>,
        bridge: Arc<dyn BridgePump>,
        lag: RelayLag,
        ledger: Arc<dyn CapabilityLedger>,
    ) -> Self {
        Self::assemble(
            store,
            backend,
            policy_source,
            Some((events, bridge, Some(lag))),
            Some(ledger),
            CONFIRMATION_TIMEOUT,
        )
    }

    fn assemble(
        store: DeviceStore,
        backend: Arc<dyn HubBackend>,
        policy_source: CapabilitySource,
        pump: Option<PumpParts>,
        ledger: Option<Arc<dyn CapabilityLedger>>,
        confirmation_timeout: Duration,
    ) -> Self {
        let confirmations = Arc::new(ConfirmationTracker::new(ledger));
        let hub = Self {
            store,
            backend,
            policy_source,
            events: broadcast::channel(EVENT_CAPACITY).0,
            inner: Arc::new(Mutex::new(Inner {
                generation: 0,
                configured: false,
                authorized: false,
                closing: None,
                state: ConnectionState::NeedsConfig,
                reason_code: None,
                device_label: None,
                seq: 0,
                state_epoch: 0,
                relay_generation: 0,
                relay_floor: 0,
                last_session: None,
                tracked_sessions: BTreeMap::new(),
                tracked_order: VecDeque::new(),
            })),
            connect_lock: AsyncMutex::new(()),
            disconnect_lock: AsyncMutex::new(()),
            takeover_lock: AsyncMutex::new(()),
            auto_started: AtomicBool::new(false),
            auto_restore_attempted: AtomicBool::new(false),
            auto_notify: Arc::new(Notify::new()),
            auto_paused: Arc::new(AtomicBool::new(false)),
            shutting_down: AtomicBool::new(false),
            auto_intent_generation: Arc::new(AtomicU64::new(0)),
            #[cfg(test)]
            kicked_intent_probe: Arc::new(KickedIntentProbe::default()),
            #[cfg(test)]
            config_pre_reset_gate: Mutex::new(None),
            #[cfg(test)]
            auto_attempts: AtomicU64::new(0),
            #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
            evidence: EvidencePermitStore::new(),
            confirmations,
            confirmation_timeout,
        };
        let configured = hub.read_configured();
        {
            let mut inner = hub.lock();
            inner.configured = configured;
            inner.state = if configured {
                ConnectionState::NeedsAuth
            } else {
                ConnectionState::NeedsConfig
            };
        }
        if let Some((events, bridge, lag)) = pump {
            hub.spawn_pump(events, bridge, lag);
        }
        hub
    }

    /// 常驻事件泵：`Data` 进桥并发布本帧的会话变化，`State` 更新连接态并按需作废
    /// 旧桥/投影；`RelayLag`（Data 溢出）**显式**作废旧投影并让下一次 GET 重新现读。
    ///
    /// 只持有 Hub 状态的 `Weak`：Hub 释放或 relay 通道关闭（sender 全丢）即退出；
    /// 单帧坏数据/桥错误只记一次降级，绝不拆泵（T2.2 M-10）。
    ///
    /// 每轮先 `lag.take()`：Data 溢出（relay 侧只置位 + 唤醒，不阻塞 actor）后，旧投影
    /// 已不可信——先作废桥/投影并发布降级事件，再继续消费队列，绝不让 GET 读到旧缓存。
    fn spawn_pump(
        &self,
        mut events: mpsc::Receiver<RelayEvent>,
        bridge: Arc<dyn BridgePump>,
        lag: Option<RelayLag>,
    ) {
        let inner = Arc::downgrade(&self.inner);
        let out = self.events.clone();
        // KICKED 进入时要持久化「暂停自动连接」：给泵一份与 Hub 同一库的 DeviceStore。
        let store = self.store.clone();
        // 同一条冲突事件还要**立即**取消自动连接的待重试轮次（不等 1s/3s/10s 定时器）：
        // 暂停标记 + 唤醒都走与 worker 共享的句柄。
        let auto_paused = self.auto_paused.clone();
        let auto_notify = self.auto_notify.clone();
        // KICKED 的持久暂停同样走意图代际（P2-6）：事件到达即预占一代，延迟写携带它。
        let auto_intent_generation = self.auto_intent_generation.clone();
        // T5/T6 首次确认：事件泵把新投影喂给登记表（send/model/thought 与
        // pending 卡式能力的观察点）。
        let confirmations = self.confirmations.clone();
        #[cfg(test)]
        let kicked_intent_probe = self.kicked_intent_probe.clone();
        tokio::spawn(async move {
            loop {
                if let Some(lag) = &lag {
                    if lag.take() {
                        let Some(inner) = inner.upgrade() else { break };
                        apply_relay_lag(&inner, &out, bridge.as_ref()).await;
                    }
                }
                let event = match &lag {
                    Some(lag) => tokio::select! {
                        event = events.recv() => event,
                        _ = lag.changed() => continue,
                    },
                    None => events.recv().await,
                };
                let Some(event) = event else { break };
                let Some(inner) = inner.upgrade() else { break };
                match event {
                    RelayEvent::Data(payload) => {
                        // 本帧进入桥之前的 Hub 代际：桥处理在途若发生换代，视图作废。
                        let hub_generation = lock_inner(&inner).generation;
                        let outcome = bridge.on_data(payload).await;
                        // 先发布本帧造成的变化（含错误帧把会话标成不新鲜的降级视图）；
                        // 代际/配对闸在 apply_relay_session 内复核。
                        for view in outcome.changed {
                            // T5 确认观察：本会话有未决确认才取日志头指纹并克隆视图
                            // （快速路径零克隆、零桥查询）。
                            let observed = confirmations.has_pending(&view.session_id).then(|| {
                                let fingerprint = bridge.log_fingerprint(&view.session_id);
                                (view.clone(), fingerprint)
                            });
                            apply_relay_session(&inner, &out, hub_generation, view);
                            if let Some((view, fingerprint)) = observed {
                                confirmations.observe_session(&view, hub_generation, fingerprint);
                            }
                        }
                        // 错误只代表本帧未吸收：不改变连接终态、不拆泵（ACK 已尽力发出）。
                        // 降级视图已在上一步发布，下一次 GET 走重新 bind/现读自愈。
                        let _ = outcome.result;
                    }
                    RelayEvent::State(view) => {
                        // P2-6：KICKED 的意图代际在**事件到达时**（应用状态之前）预占，
                        // 此后用户的任何意图写都拿到更高代、必然胜出；不能等
                        // `apply_relay_state` 的 await（桥失效）之后再取，否则这期间
                        // 落到 DB 的用户写会被误判为「更旧」。
                        let entering_conflict = view.state == ConnectionState::Conflict
                            && lock_inner(&inner).state != ConnectionState::Conflict;
                        let conflict_claim =
                            entering_conflict.then(|| capture_auto_intent(&auto_intent_generation));
                        let effects = apply_relay_state(&inner, &out, bridge.as_ref(), view).await;
                        if effects.entered_conflict {
                            // KICKED：先同步暂停并唤醒自动连接（取消待重试轮次），再持久化
                            // 「暂停自动连接」（设计 §5.2）。checked 写；失败不声称成功，
                            // 也不阻塞事件泵（阻塞型 DB 调用交给 blocking 池）。
                            //
                            // P2-6：事件到达即**预占意图代际**，延迟写携带该代。用户随后
                            // 的显式连接/断开会预占更高代，本写落库时因代际过期被跳过——
                            // 迟到的 KICKED 绝不覆盖更新的用户意图。
                            auto_paused.store(true, Ordering::SeqCst);
                            auto_notify.notify_one();
                            let store = store.clone();
                            let generation = auto_intent_generation.clone();
                            let claim = conflict_claim
                                .unwrap_or_else(|| capture_auto_intent(&auto_intent_generation));
                            #[cfg(test)]
                            let probe = kicked_intent_probe.clone();
                            tokio::task::spawn_blocking(move || {
                                #[cfg(test)]
                                probe.wait();
                                let outcome = write_auto_intent(&store, &generation, claim, false);
                                #[cfg(test)]
                                probe.finish();
                                match outcome {
                                    Ok(true) => {}
                                    // 只报失联事实：不打印 JWT/链接等任何凭据或目标。
                                    Ok(false) => log::warn!(
                                        "ZCode KICKED：自动暂停意图已被更新的用户意图取代，跳过写入"
                                    ),
                                    Err(_) => {
                                        log::warn!("ZCode KICKED：持久化自动暂停意图失败")
                                    }
                                }
                            });
                        }
                        if effects.dropped_from_paired {
                            // 真实掉线（paired → offline）= 设计 §5.2 的“下一次真实掉线
                            // 开始新一轮”：只唤醒同一 worker（配对后预算已重置），不设
                            // 暂停、不写 auto=false；worker 自身失败不会走到这里。
                            auto_notify.notify_one();
                        }
                    }
                }
            }
        });
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        lock_inner(&self.inner)
    }

    /// 从 settings 读取配置形态：只有 `Saved` 才算已配置（坏值/空白 = 未配置）。
    fn read_configured(&self) -> bool {
        self.store
            .with(|conn| get_config(conn).config_state == ConfigState::Saved)
    }

    /// 广播一条连接事件（持有内锁时不可调用）。
    fn emit_connection(&self, view: &ConnectionView) {
        emit_connection(&self.inner, &self.events, view);
    }

    /// 广播一条会话投影事件（持有内锁时不可调用）。
    fn emit_session(&self, view: &SessionView) {
        emit_session_event(&self.inner, &self.events, view);
    }

    /// 落定连接状态并广播：先连接事件，再在离开 paired 时广播一次投影降级
    /// （`ready=false`）——手机据 Session 事件得知旧投影已不可用。
    ///
    /// 返回 `(视图, attempt 纪元)`：纪元与状态在同一把锁内取得，`connect` 据它在
    /// 提交握手结果前判断事件泵是否已落定更新的状态。
    fn settle(
        &self,
        state: ConnectionState,
        reason_code: Option<String>,
        device_label: Option<String>,
    ) -> (ConnectionView, u64) {
        let (view, stale, attempt) = {
            let mut inner = self.lock();
            inner.state = state;
            inner.reason_code = reason_code;
            inner.device_label = device_label;
            let view = inner.view();
            let stale = if state == ConnectionState::Paired {
                Vec::new()
            } else {
                inner.stale_sessions(view.generation)
            };
            (view, stale, inner.state_epoch)
        };
        self.emit_connection(&view);
        for stale in &stale {
            self.emit_session(stale);
        }
        (view, attempt)
    }

    /// 握手前落定 `connecting`：**同一把锁**内先复核「代际未变且不在关闭窗口」，
    /// 不满足则**不写任何状态**、直接返回 stale/offline。
    ///
    /// 用于封死 `connect` 的 closing 检查与 `settle` 之间的窗口：`begin_disconnect`
    /// 若抢先落定 Offline，这里的复核会让本次握手尝试直接作废，绝不把刚落的
    /// Offline（或 config_changed 的 needs_auth）覆盖成 `connecting`。
    fn settle_if_current(
        &self,
        generation: u64,
        device_label: Option<String>,
    ) -> ZResult<(ConnectionView, u64)> {
        let (view, stale, attempt) = {
            let mut inner = self.lock();
            if inner.generation != generation {
                return Err(ZcodeError::of(codes::STALE_GENERATION));
            }
            if inner.closing.is_some() {
                return Err(ZcodeError::of(codes::OFFLINE));
            }
            inner.state = ConnectionState::Connecting;
            inner.reason_code = None;
            inner.device_label = device_label;
            let view = inner.view();
            let stale = inner.stale_sessions(view.generation);
            (view, stale, inner.state_epoch)
        };
        self.emit_connection(&view);
        for stale in &stale {
            self.emit_session(stale);
        }
        Ok((view, attempt))
    }

    fn state_of(error: &ZcodeError) -> ConnectionState {
        match error.code.as_str() {
            codes::NEEDS_CONFIG => ConnectionState::NeedsConfig,
            codes::NEEDS_AUTH => ConnectionState::NeedsAuth,
            codes::OFFLINE => ConnectionState::Offline,
            codes::CONFLICT => ConnectionState::Conflict,
            _ => ConnectionState::Unknown,
        }
    }

    fn state_error(state: ConnectionState) -> ZcodeError {
        let code = match state {
            ConnectionState::NeedsConfig => codes::NEEDS_CONFIG,
            ConnectionState::NeedsAuth => codes::NEEDS_AUTH,
            ConnectionState::Connecting => codes::NOT_READY,
            ConnectionState::Offline => codes::OFFLINE,
            ConnectionState::Conflict => codes::CONFLICT,
            // `Paired` 不会走到这里；未知形态按 unknown 保守收口。
            ConnectionState::Paired | ConnectionState::Unknown => codes::UNKNOWN,
        };
        ZcodeError::of(code)
    }

    /// 当前只读连接视图。
    pub fn connection(&self) -> ConnectionView {
        self.lock().view()
    }

    /// 订阅控制事件（可多订阅者：桌面 + 手机共享同一 Hub）。
    pub fn subscribe(&self) -> broadcast::Receiver<ControlEvent> {
        self.events.subscribe()
    }

    /// 本 Hub 当前能力矩阵（现算；与执行闸同一个 source）。
    pub fn policy(&self) -> CapabilityView {
        self.current_policy()
    }

    /// 当前策略的唯一读取点：会话矩阵与 [`Self::check_policy`] 共用。
    ///
    /// 生产 source = capability resolver（宿主证据 + DB 能力账本 + 接口形状证据，
    /// 异常回退 `production_unverified`）；测试可注入动态开关。
    fn current_policy(&self) -> CapabilityView {
        (self.policy_source)()
    }

    /// 下发给手机的会话能力矩阵：当前策略 + 本会话/本代际的有效验收许可。
    ///
    /// 基线**就是** [`Self::current_policy`]（与 `execute` 的能力闸同一来源），不再
    /// 另读一份「未知宿主」硬编码策略；证据构建下再叠加本会话/本代际**尚未消费且
    /// 未过期**的许可（每项只放宽一次动作）。不跨会话、不跨代际、不缓存：许可被
    /// 消费/过期/换代后，下一次 `session()` GET 立即回到当前策略。普通构建没有许可
    /// 册，行为恒为当前策略（生产默认全关）。
    fn session_capabilities(&self, session_id: &str, generation: u64) -> CapabilityView {
        #[allow(unused_mut)]
        let mut view = self.current_policy();
        #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
        {
            for capability in self.evidence.armed_capabilities(session_id, generation) {
                overlay_permit(&mut view, &capability);
            }
        }
        #[cfg(not(all(feature = "zcode-bridge-evidence", debug_assertions)))]
        {
            let _ = (session_id, generation);
        }
        view
    }

    /// T8.2 **仅 debug 证据构建**：桌面发放一条单次验收许可（唯一发放入口）。
    ///
    /// 只登记进程内存（不写 settings/DB/日志）；空会话、未知能力名一律
    /// `invalid_action`，非当前代际一律 `stale_generation`（发放了也永远不会命中）。
    /// 手机侧没有对应 HTTP 入口，只能经桌面 IPC 调用本方法。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    pub fn arm_evidence(
        &self,
        session_id: &str,
        generation: u64,
        capability: &str,
    ) -> ZResult<EvidencePermit> {
        let capability = capability.trim();
        if session_id.trim().is_empty()
            || capability.is_empty()
            || !super::evidence::is_known_capability(capability)
        {
            return Err(ZcodeError::of(codes::INVALID_ACTION));
        }
        if generation != self.lock().generation {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        Ok(self.evidence.arm(session_id, generation, capability))
    }

    /// 走官方授权。成功后落 `offline`、持久化自动连接意图并唤醒自动连接 worker
    /// （worker 未启动/条件不满足时仍需显式 [`Self::connect_requested`]）。
    ///
    /// 进入时若已有旧连接/在途握手（`paired`/`connecting`）：先在锁内推进代际、置
    /// `needs_auth`、作废旧投影，再锁外断开旧 relay，最后才走 OAuth；旧目标绝不跨进
    /// 新授权代际。初次授权（`needs_auth`/`offline`）不额外断开。
    ///
    /// 本方法**不**自动打开第二次浏览器，也不在授权结果里携带 JWT；自动连接只走
    /// 既有 [`Self::connect`]。
    pub async fn authorize(
        &self,
        provider: OAuthProvider,
        open: OpenAuthorize,
    ) -> ZResult<ConnectionView> {
        let (generation, disconnected) = {
            let mut inner = self.lock();
            if !inner.configured {
                drop(inner);
                self.publish(ConnectionState::NeedsConfig, None);
                return Err(ZcodeError::of(codes::NEEDS_CONFIG));
            }
            let mut disconnected = None;
            if matches!(
                inner.state,
                ConnectionState::Paired | ConnectionState::Connecting
            ) {
                inner.generation = inner.generation.wrapping_add(1);
                inner.authorized = false;
                inner.state = ConnectionState::NeedsAuth;
                inner.reason_code = None;
                inner.device_label = None;
                // 旧连接已应用的 relay 状态不得回写新授权代际。
                inner.relay_floor = inner.relay_generation;
                let view = inner.view();
                let stale = inner.stale_sessions(view.generation);
                disconnected = Some((view, stale));
            }
            (inner.generation, disconnected)
        };
        if let Some((view, stale)) = disconnected {
            self.emit_connection(&view);
            for stale in &stale {
                self.emit_session(stale);
            }
            // 取消在途握手/关闭旧 WS；初次授权（needs_auth/offline）不多调一次。
            self.backend.disconnect().await;
        }
        if let Err(error) = self.backend.authorize(provider, open).await {
            return Err(self.apply_failure(generation, error));
        }
        let mut inner = self.lock();
        // 授权在途时配置已变更/已断开：本次 JWT 结果作废，绝不复活旧目标。
        if inner.generation != generation {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        inner.authorized = true;
        inner.state = ConnectionState::Offline;
        inner.reason_code = None;
        inner.device_label = None;
        // 断开旧连接期间可能又应用了 relay 状态：接受新授权前把 floor 抬到已应用代际，
        // 旧连接的任何迟到状态都不得回写新授权代际。
        inner.relay_floor = inner.relay_generation;
        let view = inner.view();
        let stale = inner.stale_sessions(view.generation);
        drop(inner);
        self.emit_connection(&view);
        for stale in &stale {
            self.emit_session(stale);
        }
        // 授权成功 = 用户连接意图（设计 §3.3/§5.1）。标记写失败不改变本次授权结果，
        // 但绝不假报已保存：唤醒后 worker 仍按现查的持久意图决定是否连接。
        let _ = write_auto_intent(
            &self.store,
            &self.auto_intent_generation,
            AutoIntentClaim::Latest,
            true,
        );
        self.resume_auto_connect();
        self.wake_auto_connect();
        Ok(view)
    }

    /// 恢复已保存授权（启动/显式重试入口）：只经后端读系统存储，**绝不**打开浏览器。
    ///
    /// `AuthService` 是权威；本方法把结果对齐 Hub 的 `authorized` 缓存与展示状态
    /// （已配置且已授权 = `offline`，可继续连接；未授权 = `needs_auth`）。
    ///
    /// 首次调用登记「启动恢复已尝试」（失败也登记）：自动连接 worker 之后只读内存快照，
    /// 不再第二次读原生存储；显式 `connect_requested` 仍可直接调用本方法重试。
    pub async fn restore_authorization(&self) -> AuthStatus {
        self.auto_restore_attempted.store(true, Ordering::SeqCst);
        let status = self.backend.restore_authorization().await;
        self.sync_authorization(&status);
        status
    }

    /// 只读授权状态（后端权威；不含 JWT，不触发存储访问）。
    pub fn auth_status(&self) -> AuthStatus {
        self.backend.auth_status()
    }

    /// 用户显式连接：先持久化自动连接意图（checked，失败即 `storage_failed`，绝不
    /// 假报已保存），必要时重试一次授权恢复（**不自动 OAuth**），成功后走原
    /// [`Self::connect`]。
    pub async fn connect_requested(&self) -> ZResult<ConnectionView> {
        write_auto_intent(
            &self.store,
            &self.auto_intent_generation,
            AutoIntentClaim::Latest,
            true,
        )?;
        self.resume_auto_connect();
        // 未保存链接：交给 connect 按既有 `needs_config` 语义处置（不读保险箱）。
        if !self.read_configured() {
            return self.connect().await;
        }
        let status = self.backend.auth_status();
        if status.authorized {
            self.sync_authorization(&status);
        } else {
            let restored = self.restore_authorization().await;
            if !restored.authorized {
                // 恢复失败/无凭据：保持 needs_auth，绝不在此路径打开浏览器。
                return Err(ZcodeError::of(codes::NEEDS_AUTH));
            }
        }
        let result = self.connect().await;
        if matches!(&result, Err(error) if error.is(codes::OFFLINE)) {
            // 显式连接遇到网络离线 = 新的用户触发（设计 §5.2：显式连接才开新轮）：
            // 取消 worker 可能在途的旧退避并从首档 1s 重开预算。成功/幂等不额外唤醒，
            // 保证一次点击最多只增加一次 backend connect。
            self.wake_auto_connect();
        }
        result
    }

    /// 用户显式断开：先 checked 持久化 `auto=false`（失败即 `storage_failed`，**不得**
    /// 悄悄断开），再走原 [`Self::disconnect`]（保留 JWT，可再次连接）。
    ///
    /// 同时置内存暂停并唤醒 worker：正在退避的旧轮次立即作废（H1），不会在用户手动
    /// 断开后自行补连；显式连接/授权成功会解除暂停。
    pub async fn disconnect_requested(&self) -> ZResult<ConnectionView> {
        write_auto_intent(
            &self.store,
            &self.auto_intent_generation,
            AutoIntentClaim::Latest,
            false,
        )?;
        self.pause_auto_connect();
        Ok(self.disconnect().await)
    }

    /// 手机「断开并抢回」（T8，spec §6.5）：冲突态（官方页面把 Tuvis 踢成 conflict）
    /// 的一次性自救——显式断开后立刻按显式连接路径重连（复用既有 JWT，auto 意图置
    /// true），**不**触发 OAuth。
    ///
    /// 检查顺序：
    /// 1. 已配对/握手中 → 幂等返回当前视图（零断开零重连）；
    /// 2. 未配置 → `needs_config`；
    /// 3. 未授权 → `needs_auth`（零 OAuth：绝不调 `authorize`/开浏览器）；
    /// 4. 其余（含 conflict 终态）→ 复用既有断开编排（[`Self::disconnect`]，含两段
    ///    `backend.disconnect` 的清理窗口与 `connect_lock` 串行）→
    ///    [`Self::connect_requested`]（JWT 复用）。
    ///
    /// 结果仍 conflict 时**原样返回**该视图（`Ok`，不循环不重试）：再连一次也只会
    /// 再次被踢，调用方 UI 提示用户先关闭官方页面。并发抢回由 `takeover_lock` 单飞：
    /// 第二个调用等第一个收尾后按新状态走幂等/前置检查，绝不叠加第二轮流。
    pub async fn takeover_requested(&self) -> ZResult<ConnectionView> {
        let _single = self.takeover_lock.lock().await;
        let (state, configured, authorized) = {
            let inner = self.lock();
            (inner.state, inner.configured, inner.authorized)
        };
        // 幂等：同一 Hub 的第二个调用方（另一台手机/桌面）复用当前连接。
        if matches!(state, ConnectionState::Paired | ConnectionState::Connecting) {
            return Ok(self.connection());
        }
        if !configured {
            self.publish(ConnectionState::NeedsConfig, None);
            return Err(ZcodeError::of(codes::NEEDS_CONFIG));
        }
        // 未授权：零 OAuth——不调 authorize、不开浏览器，如实回冻结码。
        if !authorized {
            self.publish(ConnectionState::NeedsAuth, None);
            return Err(ZcodeError::of(codes::NEEDS_AUTH));
        }
        // 冲突终态先显式断开（推进代际、清理在途握手与竞态窗口连接），再按显式
        // 连接路径重连；重连结果（含仍 conflict）原样透出，绝不二次尝试。
        self.disconnect().await;
        self.connect_requested().await
    }

    /// 忘记授权：暂停自动连接、推进代际并断开 relay、清 Hub 授权缓存，再让后端清内存
    /// 并删除持久记录；**保留设备链接**。
    ///
    /// 删除失败如实回 `storage_failed`（后续 `auth_status().problem` 可见具体原因），
    /// 但已清状态**绝不回滚/不恢复**；自动连接意图同时落 `false`，写失败同样如实回报。
    pub async fn forget_authorization(&self) -> ZResult<ConnectionView> {
        // 1) 暂停本轮自动连接（内存闸封住忘记与删除之间的 restore 窗口），并持久化。
        self.pause_auto_connect();
        let auto_persisted = write_auto_intent(
            &self.store,
            &self.auto_intent_generation,
            AutoIntentClaim::Latest,
            false,
        )
        .is_ok();
        // 2) 推进控制代际、进入关闭窗口（关闭窗口阻止并发新连接）。
        let (closed, guard) = self.begin_disconnect();
        let closed_generation = closed.generation;
        self.lock().authorized = false;
        // 3) **先静默 relay**（持同一关闭凭据）：取消在途握手、等在途 connect 收尾、
        //    再关掉竞态窗口内建立的连接。旧顺序先删身份后断 relay，会让在途握手在
        //    （可能很慢/无超时的）删除期间完成并留下一条 live 的旧 JWT 连接——Hub 对
        //    stale 结果不调 `backend.disconnect`，那条连接就会一直活着。
        let current = self.quiesce_disconnect(&guard).await;
        // 4) 再清后端内存/取消旧 epoch/禁用恢复并删除持久记录（删除失败如实上报）。
        //    窗口若已被更新者接管（current=false），删除义务仍须完成（与旧语义一致），
        //    但清位与末态只属于仍 current 的本窗口。
        let status = self.backend.forget_authorization().await;
        drop(guard);
        // 5) 链接保留（不碰 config）；展示落「需要重新授权 / 需要配置」。
        //
        // 删除可能耗时：期间若更新的配置变更/断开/冲突已接管代际，更新者的状态优先，
        // 本步骤绝不覆盖；删除失败仍如实回错（已清/不恢复语义不变）。
        let configured = self.read_configured();
        let (view, stale, changed, _superseded) = {
            let mut inner = self.lock();
            if inner.generation != closed_generation {
                (inner.view(), Vec::new(), false, true)
            } else {
                inner.configured = configured;
                inner.authorized = false;
                let before = inner.state;
                inner.state = if configured {
                    ConnectionState::NeedsAuth
                } else {
                    ConnectionState::NeedsConfig
                };
                inner.reason_code = None;
                inner.device_label = None;
                // 断开触发的旧 relay 状态不得把「需要授权」改写回 offline。
                inner.relay_floor = inner.relay_generation;
                let view = inner.view();
                let stale = inner.stale_sessions(view.generation);
                let changed = before != view.state;
                (view, stale, changed, false)
            }
        };
        if changed {
            self.emit_connection(&view);
        }
        for stale in &stale {
            self.emit_session(stale);
        }
        if current {
            // 本窗口已清位且末态已落：补唤醒（理由同 `finish_disconnect` 的 P2-5）。
            // 只唤醒不越权：auto=false/暂停/无授权时 worker 现查后即停。
            self.wake_auto_connect();
        }
        if status.problem.is_some() || !auto_persisted {
            return Err(ZcodeError::of(codes::STORAGE_FAILED));
        }
        // `_superseded` 时 view 已是更新者的当前视图，本函数不覆盖它。
        Ok(view)
    }

    /// 用后端授权事实对齐 Hub 缓存与不涉及连接的展示状态。
    ///
    /// 只改 `needs_auth`/`needs_config`/`offline` 三档：已配对/握手中/冲突时不凭
    /// 一次恢复结果改写连接态（连接事实由 relay 与显式断开掌握）。
    fn sync_authorization(&self, status: &AuthStatus) {
        let (view, changed) = {
            let mut inner = self.lock();
            inner.authorized = status.authorized;
            let before = (
                inner.state,
                inner.reason_code.clone(),
                inner.device_label.clone(),
            );
            if !inner.configured {
                inner.state = ConnectionState::NeedsConfig;
                inner.reason_code = None;
                inner.device_label = None;
            } else if status.authorized {
                if matches!(
                    inner.state,
                    ConnectionState::NeedsAuth | ConnectionState::NeedsConfig
                ) {
                    inner.state = ConnectionState::Offline;
                    inner.reason_code = None;
                    inner.device_label = None;
                }
            } else if !matches!(
                inner.state,
                ConnectionState::Paired | ConnectionState::Connecting | ConnectionState::Conflict
            ) {
                inner.state = ConnectionState::NeedsAuth;
                inner.reason_code = None;
                inner.device_label = None;
            }
            let changed = before
                != (
                    inner.state,
                    inner.reason_code.clone(),
                    inner.device_label.clone(),
                );
            (inner.view(), changed)
        };
        if changed {
            self.emit_connection(&view);
        }
    }

    // ------------------------------------------------------------------
    // 自动连接编排的内部访问口（`auto_connect.rs` 是同域兄弟模块）
    // ------------------------------------------------------------------

    pub(crate) fn auto_connect_store(&self) -> &DeviceStore {
        &self.store
    }

    /// 自动连接 worker 的「启动恢复是否已尝试」观察口（P2-7：只读一次原生存储）。
    pub(crate) fn auto_restore_attempted(&self) -> bool {
        self.auto_restore_attempted.load(Ordering::SeqCst)
    }

    /// 测试观察口：worker 已执行到 `try_auto_connect` 的次数（同步信号，不用 sleep）。
    #[cfg(test)]
    pub(crate) fn auto_attempt_count(&self) -> u64 {
        self.auto_attempts.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn record_auto_attempt(&self) {
        self.auto_attempts.fetch_add(1, Ordering::SeqCst);
    }

    /// 测试观察口：当前意图代际（KICKED 预占/用户写都会推进它）。
    #[cfg(test)]
    fn auto_intent_generation(&self) -> u64 {
        self.auto_intent_generation.load(Ordering::SeqCst)
    }

    /// 测试观察口：给下一次 KICKED 延迟写装两段闸，返回 `(放行, 完成)` 两端。
    ///
    /// 只影响测试构建；生产泵不编译探针字段，也没有任何等待。
    #[cfg(test)]
    fn install_kicked_intent_probe(
        &self,
    ) -> (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>) {
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        *self.kicked_intent_probe.release.lock().unwrap() = Some(release_rx);
        *self.kicked_intent_probe.done.lock().unwrap() = Some(done_tx);
        (release_tx, done_rx)
    }

    /// 测试观察口：给下一次 `config_changed` 装「begin 后、取清理锁前」的闸。
    #[cfg(test)]
    fn install_config_pre_reset_gate(&self) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        *self.config_pre_reset_gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    /// 测试观察口：`config_changed` 是否已停在 pre-reset 闸前。
    #[cfg(test)]
    fn config_waiting_before_reset(&self) -> bool {
        self.config_pre_reset_gate.lock().unwrap().is_none()
    }

    /// 测试观察口：KICKED 延迟写是否已到闸前（取走放行端 = 已进入 blocking 任务）。
    #[cfg(test)]
    fn kicked_write_at_gate(&self) -> bool {
        self.kicked_intent_probe.release.lock().unwrap().is_none()
    }

    pub(crate) fn auto_connect_paused(&self) -> bool {
        self.auto_paused.load(Ordering::SeqCst)
    }

    /// 暂停本轮自动连接（忘记授权 / 手动断开 / KICKED）：同步置位并唤醒 worker，
    /// 让正在退避的旧轮次立即作废，而不是等 1s/3s/10s 定时器到点。
    pub(crate) fn pause_auto_connect(&self) {
        self.auto_paused.store(true, Ordering::SeqCst);
        self.wake_auto_connect();
    }

    /// 退出清理第一步（**仅退出路径**）：标记进程正在退出并同步取消自动连接的在途/
    /// 退避轮次。
    ///
    /// 只置内存标记（`shutting_down` 单调、不撤销）+ 内存暂停 + 唤醒；**不**写持久
    /// auto 意图（退出不改用户偏好）。worker 在入口与每次 await 后都检查该标记：
    /// 即便关闭窗口随后清位，既有轮次也绝不自行重连。
    pub(crate) fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.pause_auto_connect();
    }

    /// 退出清理是否已开始（worker 的单调停机闸）。
    pub(crate) fn shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    pub(crate) fn resume_auto_connect(&self) {
        self.auto_paused.store(false, Ordering::SeqCst);
    }

    /// 自动连接轮次绑定的控制代际：换/删链接、显式断开、KICKED 推进代际后，旧的
    /// 定时器/在途尝试一律作废，绝不向旧目标发起连接（等待结束后由 worker 现查）。
    pub(crate) fn auto_connect_generation(&self) -> u64 {
        self.lock().generation
    }

    /// 单 worker 启动幂等：第一次调用返回 true，后续调用 false。
    pub(crate) fn auto_worker_begin(&self) -> bool {
        !self.auto_started.swap(true, Ordering::SeqCst)
    }

    pub(crate) fn auto_notify_handle(&self) -> Arc<Notify> {
        self.auto_notify.clone()
    }

    /// 自动连接的运行时前置：不在关闭窗口、未配对、未握手中、非冲突终态。
    pub(crate) fn auto_connect_runtime_ready(&self) -> bool {
        let inner = self.lock();
        inner.closing.is_none()
            && !matches!(
                inner.state,
                ConnectionState::Paired | ConnectionState::Connecting | ConnectionState::Conflict
            )
    }

    /// 测试专用：把 Hub 标记为「已授权」，复用测试里真实 [`AuthService`] 已取得的
    /// JWT，**不**再走一次 OAuth（不重复授权、不等待回调）。
    ///
    /// 状态落点**逐字段镜像** [`Self::authorize`] 成功分支（`authorized=true`、
    /// `offline`、清原因与设备标签、广播连接事件并按需降级旧投影），只是不触达
    /// 后端、不发网络。随后 [`Self::connect`] 仍走真实 relay token + WebSocket 握手。
    #[cfg(test)]
    pub(crate) fn mark_authorized_for_test(&self) -> ConnectionView {
        let (view, stale) = {
            let mut inner = self.lock();
            inner.authorized = true;
            inner.state = ConnectionState::Offline;
            inner.reason_code = None;
            inner.device_label = None;
            let view = inner.view();
            let stale = inner.stale_sessions(view.generation);
            (view, stale)
        };
        self.emit_connection(&view);
        for stale in &stale {
            self.emit_session(stale);
        }
        view
    }

    /// 建立唯一物理连接（已配对/连接中幂等；`conflict` 为终态，不自动重试）。
    pub async fn connect(&self) -> ZResult<ConnectionView> {
        let _single = self.connect_lock.lock().await;
        // 断开清理窗口（`begin_disconnect` 已置 Offline、`finish_disconnect` 尚未收尾）：
        // 立即回 Offline 错误且**不**调 `backend.connect`——否则在清理最后一次
        // `backend.disconnect` 之后仍可能新开一条连接，把已断开的 Hub 留在 paired。
        if self.lock().closing.is_some() {
            return Err(ZcodeError::of(codes::OFFLINE));
        }
        let (state, configured, authorized, generation) = {
            let inner = self.lock();
            (
                inner.state,
                inner.configured,
                inner.authorized,
                inner.generation,
            )
        };
        match state {
            // 幂等：同一 Hub 的第二个调用方（另一台手机/桌面）复用同一条物理连接。
            ConnectionState::Paired | ConnectionState::Connecting => {
                return Ok(self.connection());
            }
            // KICKED = 冲突终态：绝不自动抢回，只有显式断开/配置变更才复位。
            ConnectionState::Conflict => return Err(ZcodeError::of(codes::CONFLICT)),
            _ => {}
        }
        if !configured {
            self.publish(ConnectionState::NeedsConfig, None);
            return Err(ZcodeError::of(codes::NEEDS_CONFIG));
        }
        if !authorized {
            self.publish(ConnectionState::NeedsAuth, None);
            return Err(ZcodeError::of(codes::NEEDS_AUTH));
        }
        let (saved, url) = self.store.with(|conn| {
            let config = get_config(conn);
            (config.config_state == ConfigState::Saved, config.url)
        });
        let Some(url) = url.filter(|_| saved) else {
            self.publish(ConnectionState::NeedsConfig, None);
            return Err(ZcodeError::of(codes::NEEDS_CONFIG));
        };
        let route = match DeviceRoute::from_saved_url(&url) {
            Ok(route) => route,
            Err(error) => {
                self.publish(ConnectionState::NeedsConfig, Some(error.code.clone()));
                return Err(error);
            }
        };
        let label = route.label.clone();
        // 握手中：离开 paired，已读投影同步降级广播；attempt 纪元与状态同锁取得。
        // 关闭/换代复核也在这把锁内（settle_if_current）：begin_disconnect 若恰好
        // 抢在 closing 检查与这一步之间，绝不覆盖它刚落的 Offline，且零后端连接。
        let (_, attempt) = match self.settle_if_current(generation, label.clone()) {
            Ok(pair) => pair,
            Err(error) => return Err(error),
        };
        // settle 之后、真正触达后端之前再复核一次（只读，不改状态）：关闭/换代恰好
        // 插在 settle 与这里时，状态已由 begin_disconnect/config_changed 自己落定，
        // 本次尝试直接作废且不调 backend.connect。
        {
            let inner = self.lock();
            if inner.generation != generation {
                return Err(ZcodeError::of(codes::STALE_GENERATION));
            }
            if inner.closing.is_some() {
                return Err(ZcodeError::of(codes::OFFLINE));
            }
        }
        match self.backend.connect(route).await {
            Err(error) => Err(self.apply_failure(generation, error)),
            Ok(view) => {
                let mut inner = self.lock();
                // 迟到结果（配置变更/断开已推进代际）一律丢弃，不得回退状态。
                if inner.generation != generation {
                    return Err(ZcodeError::of(codes::STALE_GENERATION));
                }
                // 事件泵在本 attempt 期间已落定更新的非 connecting 状态（如握手后
                // 立即掉线）：新状态胜出，绝不把迟到的握手成功写成假 paired。
                if inner.state_epoch != attempt {
                    let current = inner.view();
                    drop(inner);
                    return match current.state {
                        ConnectionState::Paired => Ok(current),
                        state => Err(Self::state_error(state)),
                    };
                }
                inner.state = view.state;
                inner.reason_code = view.reason_code.clone();
                inner.device_label = view.device_label.clone().or(label);
                let out = inner.view();
                let stale = if out.state == ConnectionState::Paired {
                    Vec::new()
                } else {
                    inner.stale_sessions(out.generation)
                };
                drop(inner);
                self.emit_connection(&out);
                for stale in &stale {
                    self.emit_session(stale);
                }
                if out.state == ConnectionState::Conflict {
                    // KICKED 也可能作为本次 connect 的结果直接落定：与事件泵同规——
                    // 这是**事件性**写入，不是用户意图，先预占一代再写 false；若期间
                    // 用户已写入更新的意图（更高代），本写跳过，绝不抢占。普通掉线/
                    // 显式断开绝不动用户意图。
                    let claim = capture_auto_intent(&self.auto_intent_generation);
                    match write_auto_intent(&self.store, &self.auto_intent_generation, claim, false)
                    {
                        Ok(true) => {}
                        Ok(false) => {
                            log::warn!("ZCode KICKED：自动暂停意图已被更新的用户意图取代，跳过写入")
                        }
                        // 只报失联事实：不打印 JWT/链接等任何凭据或目标。
                        Err(_) => log::warn!("ZCode KICKED：持久化自动暂停意图失败"),
                    }
                }
                Ok(out)
            }
        }
    }

    /// 断开第一阶段（同步、不持 `connect_lock`、不 await）：推进代际、置 Offline、
    /// 清原因/设备标签、抬高 relay 地板并广播连接与投影降级事件；同时把关闭窗口
    /// 置为本次 epoch。返回 `(视图, 关闭凭据)`。
    ///
    /// 关闭凭据 [`CloseGuard`] 在**本函数内**（锁内）创建并随返回值交出：调用方
    /// 立即持有取消兜底——即便后续清理 future 被取消/从未 poll/panic，凭据 Drop 也
    /// 会清位。
    ///
    /// 远端关停先调它：手机/桌面立刻看到「已断开」，不必等在途握手的网络超时；
    /// 昂贵的清理交给 [`Self::finish_disconnect`] 在后台完成。
    pub(crate) fn begin_disconnect(&self) -> (ConnectionView, CloseGuard) {
        let (view, stale, epoch) = {
            let mut inner = self.lock();
            inner.generation = inner.generation.wrapping_add(1);
            inner.state = ConnectionState::Offline;
            inner.reason_code = None;
            inner.device_label = None;
            // 与 config_changed 同规：关闭前的旧 relay 状态（已应用代际）不得回写；
            // 关闭窗口内仍在途的旧 attempt 由 `apply_relay_state` 的 closing 闸拦下。
            inner.relay_floor = inner.relay_generation;
            let epoch = inner.generation;
            inner.closing = Some(epoch);
            let view = inner.view();
            let stale = inner.stale_sessions(view.generation);
            (view, stale, epoch)
        };
        self.emit_connection(&view);
        for stale in &stale {
            self.emit_session(stale);
        }
        (
            view,
            CloseGuard {
                inner: self.inner.clone(),
                epoch,
            },
        )
    }

    /// 断开第二阶段（清理，调用方不可用超时取消它）：**按 epoch 归属**串行执行：
    /// 取 `disconnect_lock` → 复核凭据仍是当前窗口 → 断后端（取消在途 relay
    /// connect）→ 取 `connect_lock`（等在途 connect 收尾）→ 再次复核 → 再断一次后端
    /// （关掉竞态窗口内新建立的连接）→ Drop 凭据清位。
    ///
    /// 两道 epoch 复核缺一不可：
    /// - 进入时已过期 → 更新的关闭窗口已接管，本清理什么都不做；
    /// - 等到 `connect_lock` 期间被更新窗口取代 → 不得再发第二次 `backend.disconnect`
    ///   （否则会关掉新窗口清位后建立的新连接）。
    ///
    /// `disconnect_lock` 只在本函数内取得，`connect` 永不取它——锁序只有
    /// `disconnect_lock → connect_lock`，无反转。
    ///
    /// `closing` 在整个清理期间为置位（见 [`Self::connect`] 与
    /// [`apply_relay_state`]）：即便新 connect 在这两次 `backend.disconnect` 之间
    /// 拿到锁，也不会真的开连接；旧 attempt 的 relay 状态也不会回写 Offline。
    ///
    /// # 活性前提（不缩短清理、不撤回清理）
    ///
    /// 本函数会**等待** `backend.disconnect()` 终止：relay 的终态 `State` 在事件队列
    /// 满时无超时地等待消费端（`relay.rs` 的 #8 有意取舍），所以
    /// `RelayClient::disconnect` 可能长时间不返回；remote 侧的 5s 关停等待
    /// （`ZCODE_HUB_SHUTDOWN_WAIT`）只是**等待上界**，超时不会 abort 清理任务
    ///（任务继续在后台收尾）。因此「关闭后能否及时重连」以
    /// `backend.disconnect()` 最终终止为前提。
    ///
    /// 这里**不得**加「超时就撤回清理」的兜底：撤回后旧任务仍可能在下一次
    /// `begin_disconnect`/新连接之后发出 `backend.disconnect()`，重新引入「旧清理
    /// 关掉新连接」的竞态。epoch 归属检查只能阻止**已被接管**的 owner 继续清理，
    /// 不能让已经发出的 disconnect 撤回。
    ///
    /// 关闭清理的第一段（`finish_disconnect` 与 `forget_authorization` 复用）：
    /// 取 `disconnect_lock` → 复核凭据 → `backend.disconnect`（取消在途握手）→
    /// 取 `connect_lock`（等在途 connect 收尾）→ 再次复核 → 再 `backend.disconnect`
    /// （关掉竞态窗口内建立的连接）。
    ///
    /// 返回 `true` = 凭据仍是当前窗口（relay 已静默、无在途握手）；`false` = 已被
    /// 更新的关闭窗口接管（本段碰到 epoch 复核即停止，调用方不得清位/做本窗口收尾）。
    ///
    /// 锁序只有 `disconnect_lock → connect_lock`，`connect` 永不取 `disconnect_lock`。
    async fn quiesce_disconnect(&self, guard: &CloseGuard) -> bool {
        // 清理单飞：重叠关闭的清理按到达顺序串行，后一个不得越过前一个的窗口。
        let _cleanup = self.disconnect_lock.lock().await;
        // 更新的 begin_disconnect 已接管：陈旧 owner 不得再发任何清理动作。
        if !guard.is_current() {
            return false;
        }
        // 先取消在途握手：relay connect 等网络的 await 会就此收尾、释放 connect_lock。
        self.backend.disconnect().await;
        // 等在途 connect 收尾（此后再无持锁的旧代际握手）。
        let _single = self.connect_lock.lock().await;
        // 等 connect_lock 期间可能又发生了更新的关闭（本凭据已过期）：不得再发第二次
        // disconnect（否则会关掉新窗口清位后建立的新连接），只按 epoch 兜底 Drop。
        if !guard.is_current() {
            return false;
        }
        // 关掉竞态窗口内可能已建立的连接（本 Hub 期间绝不留下 paired）。
        self.backend.disconnect().await;
        true
    }

    /// 本 future 被取消/panic 时，`guard` 随栈帧 Drop，按 epoch 兜底清位。
    pub(crate) async fn finish_disconnect(&self, guard: CloseGuard) {
        self.finish_disconnect_with(guard, true).await;
    }

    /// 静默收尾（**退出/进程终结专用**）：清位规则与 [`Self::finish_disconnect`] 相同，
    /// 但**不唤醒**自动连接——退出只释放资源，绝不因补唤醒而用仍在内存的 JWT /
    /// 仍有效的 auto 意图新开一条 relay（「退出只释放、不自动重连」）。
    ///
    /// 远程停服/显式断开仍必须用会唤醒的 [`Self::finish_disconnect`]（P2-5：
    /// 关闭窗口内被消费的唤醒不得丢）。
    pub(crate) async fn finish_disconnect_silent(&self, guard: CloseGuard) {
        self.finish_disconnect_with(guard, false).await;
    }

    /// 关闭清理的公共收尾：静默段 → 按 epoch 清位 → 可选唤醒。
    async fn finish_disconnect_with(&self, guard: CloseGuard, wake: bool) {
        // 静默段与忘授权的顺序解耦（P2-4 复用同一实现）。
        if !self.quiesce_disconnect(&guard).await {
            drop(guard);
            return;
        }
        // 静默完成后再清位：新 connect 只可能在清理完全结束后才被允许。
        drop(guard);
        if wake {
            // P2-5：清位后补一次唤醒。关闭窗口内到达的唤醒只能让 worker 看到
            // Offline/closing 并以 Stopped 消费掉；不补这一次，「快关快开」就丢了唤醒。
            // 只唤醒不越权：worker 现查 auto 意图/remote.enabled/链接，用户断开
            // （auto=false）仍不会自动连接。
            self.wake_auto_connect();
        }
    }

    /// 用户显式断开：推进代际、保留 JWT，可再次连接。
    ///
    /// 与配置变更同规：代际推进与「不可控」状态在**同一把锁**内落定，
    /// `backend.disconnect` 在途期间不得再读到旧配对/旧投影。
    pub async fn disconnect(&self) -> ConnectionView {
        let (view, guard) = self.begin_disconnect();
        self.finish_disconnect(guard).await;
        view
    }

    /// 配置保存/删除成功后的唯一入口：**原子**推进代际并退出配对，再断 relay、作废投影。
    ///
    /// **保留账号 JWT**：`reset` 不再清凭据（换/删链接不重新 OAuth）；是否仍授权在
    /// reset 后由 `backend.auth_status()` 权威落定（Hub 的 `authorized` 只是缓存）。
    /// 调用方只在 DB 保存**成功后**调用；保存失败保持原连接，不由本函数处置。
    /// 代际推进与「不可控」状态在**同一把锁**内落定：`reset` 在途的窗口里
    /// `connection()`/`session()` 绝不会看到「新代际 + 旧配对/旧投影/旧标签」。
    ///
    /// **清理窗口**（P2-3）：代际推进的同时置 `closing`，`reset`/`auth_status` 全程
    /// 持 [`CloseGuard`]；窗口内 `connect`/自动连接一律被拒，旧清理不得覆盖新代际的
    /// Offline/NeedsConfig，也不得让新目标被提前连上。清理结束后 drop 凭据清位，
    /// 再唤醒自动连接（关闭窗口内被消费的唤醒不得丢）。
    pub async fn config_changed(&self) -> ConnectionView {
        // 新配置已由调用方落库；先读形态（本地 KV，非网络、不持锁）。
        let configured = self.read_configured();
        let (view, guard) = self.begin_config_change(configured);
        #[cfg(test)]
        {
            let gate = self.config_pre_reset_gate.lock().unwrap().take();
            if let Some(gate) = gate {
                gate.notified().await;
            }
        }
        // 清理串行 + owner 校验（终审 P2-3 残留）：`reset` 是清理动作，必须与断开清理
        // 共用 `disconnect_lock`，且只有**仍是当前窗口**的 owner 才能执行——否则 B 已
        // 完成并重连后，陈旧的 A 仍可能 reset 断掉 B 的连接/投影。
        //
        // 本函数不取 `connect_lock`、不调用 `finish_disconnect`：锁序只有
        // `disconnect_lock`（config 不产生 `disconnect_lock → connect_lock` 嵌套），
        // 与 finish 的 `disconnect_lock → connect_lock` 无反转、无死锁。
        {
            let _cleanup = self.disconnect_lock.lock().await;
            if guard.is_current() {
                // 仍是 owner 才断 relay、作废旧投影（reset 不清 JWT）。
                self.backend.reset(view.generation).await;
            }
        }
        // 非 owner 也读一次权威状态（只读、不触网），供下面的代际复核/诊断；
        // 末态只在仍是本代际时落库，绝不覆盖更新的配置变更。
        let status = self.backend.auth_status();
        let (out, applied) = {
            let mut inner = self.lock();
            // 期间又发生了更新的配置变更/断开：以更新者的状态为准。
            if inner.generation != view.generation {
                (inner.view(), false)
            } else {
                inner.configured = configured;
                inner.authorized = status.authorized;
                inner.state = configured_state(configured, status.authorized);
                inner.reason_code = None;
                inner.device_label = None;
                inner.relay_floor = inner.relay_generation;
                let out = inner.view();
                drop(inner);
                if out != view {
                    self.emit_connection(&out);
                }
                (out, true)
            }
        };
        // 只看仍是本窗口的凭据清位：更新的配置变更已接管时绝不清它的新窗口。
        drop(guard);
        if applied {
            // 清位后补一次唤醒：清理窗口内 worker 只能看到 Offline/closing，唤醒许可
            // 已被本轮消费；这里不补，快关快开的唤醒就丢了（gate 现查 intent/remote）。
            // 被更新的配置变更取代（applied=false）时不补：新 owner 自己会唤醒。
            self.wake_auto_connect();
        }
        out
    }

    /// 配置变更第一阶段（同步、锁内）：推进代际、置 `closing`、落新形态并降级旧投影，
    /// 返回 `(视图, 关闭凭据)`。
    ///
    /// `closing` 与 `begin_disconnect` 同规：凭据 Drop 时**仅当仍是本 epoch** 才清位，
    /// 因此更新的配置变更接管后，旧清理既不清新窗口、也不覆盖新代际状态。
    fn begin_config_change(&self, configured: bool) -> (ConnectionView, CloseGuard) {
        let (view, stale, epoch) = {
            let mut inner = self.lock();
            inner.generation = inner.generation.wrapping_add(1);
            let generation = inner.generation;
            inner.configured = configured;
            // 不无条件清 authorized：reset 只断连接、不清账号 JWT。
            inner.state = configured_state(configured, inner.authorized);
            inner.reason_code = None;
            inner.device_label = None;
            // reset 之前到达的旧 relay 状态（含 reset 触发的 offline）一律丢弃。
            inner.relay_floor = inner.relay_generation;
            // 清理窗口：reset/auth_status 在途期间拒绝新连接与旧 relay 状态回写。
            inner.closing = Some(generation);
            let view = inner.view();
            let stale = inner.stale_sessions(generation);
            (view, stale, generation)
        };
        self.emit_connection(&view);
        for stale in &stale {
            self.emit_session(stale);
        }
        (
            view,
            CloseGuard {
                inner: self.inner.clone(),
                epoch,
            },
        )
    }

    /// 读取精确目标的会话投影（只接受当前代际内可精确绑定的 Tuvis sessionId）。
    ///
    /// 公开代际唯一：投影的 `generation` 恒为 Hub 代际（bridge 投影代际内部使用，
    /// 不外泄）；读取成功即广播一条本会话的 [`ControlEventKind::Session`]。
    pub async fn session(&self, key: &TargetKey) -> ZResult<SessionView> {
        let generation = self.control_generation()?;
        let target = self.backend.bind_session(key).await?;
        if self.lock().generation != generation {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        // 公开代际 = Hub 代际；桥投影代际收进目标内部字段（桥内对拍用）。
        let target = public_target(target, generation);
        let mut view = self.backend.session(&target).await?;
        // 读在途时代际已变：旧投影不得贴上新代际。
        if self.lock().generation != generation {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        view.generation = generation;
        // 能力矩阵：与 `execute` 闸同一现算源（生产 = 桥的宿主证据）叠加本会话/
        // 本代际仍有效的验收许可（仅 debug 证据构建）。每次 GET 现算：策略或许可
        // 变化（消费/过期/换代）在下一次 `session()` 立即可见——手机按该字段硬门。
        view.capabilities = self.session_capabilities(&view.session_id, generation);
        // 不新鲜投影不得携带开闸矩阵：与事件路径 `apply_relay_session` 同规（坏帧/
        // 解码/序号错误后 GET 仍可能读到 ready=false 的投影；执行闸另按策略现算）。
        if !view.ready {
            view.capabilities = CapabilityView::default();
        }
        {
            let mut inner = self.lock();
            // 现读在途时代际已变（断开/配置变更）：旧投影绝不贴上新代际，也绝不
            // 登记为可降级视图（否则旧桥会话会在重连后复活）。
            if inner.generation != generation {
                return Err(ZcodeError::of(codes::STALE_GENERATION));
            }
            // 同代掉线（relay offline，代际不变）同样不得登记：在途 GET 的结果不能
            // 把 ready=true 写回一个已离开 paired 的 Hub（按当前连接态报错）。
            if inner.state != ConnectionState::Paired {
                return Err(Self::state_error(inner.state));
            }
            inner.last_session = Some(view.clone());
            // 每个成功 GET 的会话都进跟踪表：Data 溢出时逐会话降级（不止最后一个）。
            inner.track_session(&view);
        }
        self.emit_session(&view);
        Ok(view)
    }

    /// 执行一次控制动作：代际 + 持久幂等 → 连接门 + 能力闸 + 目标绑定通过才派发。
    pub async fn execute(
        &self,
        ctx: ActionContext,
        action: ControlAction,
    ) -> ZResult<ActionReceipt> {
        // 代际判定先于一切：旧代际动作即便当前未就绪也必须回 `stale_generation`
        // （调用方据此丢弃本地卡面，而不是误以为「重连后还能重试」），也绝不
        // 从账本取回旧代际回执。
        let generation = self.lock().generation;
        if action.generation() != generation {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        // 持久幂等查询紧随代际判定、**先于**连接门/能力闸/bind：断开、未配对或
        // 能力关闭后重试同一 actionId，仍取回既有 accepted/confirmed/unknown 回执；
        // 冲突（同 id 换载荷/跨设备）也在此直接上抛，而不是误报 needs_auth/offline。
        // 重放命中不 emit 新的 Action 事件（否则手机会看到重复回执）。
        if let Some(existing) = self.backend.existing_receipt(&ctx, &action).await? {
            return Ok(existing);
        }
        self.control_generation()?;
        // 能力闸分三段：
        // - 静态动作（Send/Stop/Permission/Selection）在 bind 前按策略/许可检查；
        // - Answer/Snooze 在 bind 前先做**廉价否决**：四项问答/审批能力全关且无
        //   任何匹配许可 → needs_evidence（零 bind、零 I/O）；
        // - 否则绑定公开目标，再读宿主 pending 卡的题型推导**全部**所需能力
        //   （客户端 selectedValues 数量不参与判定），逐项过闸后才消费许可。
        let host_aware = matches!(
            &action,
            ControlAction::Answer { .. } | ControlAction::Snooze { .. }
        );
        let mut permits = if host_aware {
            self.check_answer_precheck(&action)?;
            Vec::new()
        } else {
            self.check_policy(&action)?
        };
        let key = TargetKey {
            session_id: action.session_id().to_owned(),
        };
        let target = self.backend.bind_session(&key).await?;
        // 绑定在途时配置变更/断开：动作归属旧代际，零 execute。
        if self.lock().generation != generation {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        // CommandSender 只认公开代际（校验/落账/回执）；桥投影代际留在目标内部。
        let target = public_target(target, generation);
        // 宿主卡题型推导出的能力名（Answer/Snooze；Permission 是静态闸不在此列）。
        // 除过闸外，它还是问答确认登记的能力集来源（multi/free 只在形状存在时候选）。
        let mut host_required: Vec<&'static str> = Vec::new();
        if host_aware {
            let required = self.backend.required_capabilities(&target, &action).await?;
            // 读宿主卡期间可能发生换代：旧代际动作零执行（与 `session` 同纪律）。
            if self.lock().generation != generation {
                return Err(ZcodeError::of(codes::STALE_GENERATION));
            }
            permits = self.check_required_policy(&action, &required)?;
            host_required = required;
        }
        // R4 首次真实动作确认：闸门已全过、派发未发生——此刻登记 baseline
        // （send/model/thought/问答作答/审批应答；已判决的能力与未提供 baseline 的
        // 后端跳过）。能力集随 action 固定，收口/摘除只结算同一集合（selection 的
        // 回执绝不代答同会话仍在 ACK 窗内的 send）。
        let confirmation_capabilities: Vec<Capability> = match &action {
            ControlAction::Send { .. } => vec![Capability::Send],
            ControlAction::Selection { .. } => vec![Capability::Model, Capability::ThoughtLevel],
            // 问答作答：候选 = 宿主卡题型推导（客户端所选数量/freeText 内容不参与）。
            ControlAction::Answer { .. } => host_required
                .iter()
                .filter_map(|name| answer_capability(name))
                .collect(),
            // 审批应答：只确认「回答既有 permission 卡」（spec §4.1）——登记只发生
            // 在用户对既有卡的动作上，不监听「发送触发审批卡」/elicitation/新卡。
            ControlAction::Permission { .. } => vec![Capability::ApprovalReply],
            // 打断：interrupt 首次真实使用（spec R4：目标 guard 从 activeWorks 消失
            // + 停止终态）。登记面在 `register_confirmations` 里再按「初始 control
            // 已宣告且目标守卫在场」收窄——无守卫可等待时不登记、不触发。
            ControlAction::Stop { .. } => vec![Capability::Interrupt],
            // Snooze 只暂停自动结束计时，不是作答/审批：零登记。
            _ => Vec::new(),
        };
        if !confirmation_capabilities.is_empty() {
            self.register_confirmations(&action, &confirmation_capabilities, generation)
                .await;
        }
        let dispatch = self
            .backend
            .execute_with_permits(&ctx, &target, &action, &permits)
            .await;
        match dispatch {
            Ok(mut receipt) => {
                // 回执终态先收口确认（Confirmed/Rejected/Unknown 直接落账，
                // ACK-only 保持 pending）；spec R4 的 5s 窗从 ACK 起算 → 重置期限。
                // 结算用**当前**代际：派发在途若发生换代，旧代际登记摘除且零落账。
                let current_generation = self.lock().generation;
                self.confirmations.settle_receipt(
                    action.session_id(),
                    &confirmation_capabilities,
                    &receipt,
                    current_generation,
                );
                if receipt.state == ActionState::Accepted {
                    for deadline in self.confirmations.rearm(
                        action.session_id(),
                        generation,
                        self.confirmation_timeout,
                    ) {
                        self.spawn_confirmation_timer(deadline);
                    }
                }
                // 公开代际唯一：回执盖本次校验过的 Hub 代际（CommandSender 用公开代际
                // 校验/落账，桥投影代际只在目标内部）。
                // 若执行在途发生了换代，回执仍是旧代际——客户端据 connection 发现并重取。
                receipt.generation = generation;
                self.emit_action(&receipt, &ctx.device_id);
                Ok(receipt)
            }
            Err(error) => {
                // 派发失败（CAS 陈旧/端口错误）不构成能力证据：只摘除本动作能力集
                // 内的 pending，绝不据此降级（降级只留给回执显式失败）。
                self.confirmations
                    .drop_pending(action.session_id(), &confirmation_capabilities);
                Err(error)
            }
        }
    }

    /// R4 首次真实动作确认的登记面：捕获 baseline 并为每个仍候选的能力建立确认窗。
    ///
    /// 候选判定 = 账本无 Confirmed/Degraded/Unsupported 判决（Inconclusive/
    /// InterfaceMatched/无记录都重新确认）；后端不提供 baseline 或未装配账本时
    /// 整体跳过。确认窗的期限在 ACK 后重置（spec：「ACK 后等新投影……5s 有界」）。
    async fn register_confirmations(
        &self,
        action: &ControlAction,
        capabilities: &[Capability],
        generation: u64,
    ) {
        let Some(baseline) = self
            .backend
            .confirmation_baseline(action.session_id())
            .await
        else {
            return;
        };
        for capability in capabilities {
            if !self
                .confirmations
                .needs_confirmation(&baseline.key, *capability)
            {
                continue;
            }
            let target = match (action, capability) {
                (ControlAction::Send { .. }, Capability::Send) => {
                    // 登记时刻的日志头指纹 = 「最后用户消息」基线（id 形状，非正文）。
                    ConfirmationTarget::UserMessage {
                        baseline_id: baseline
                            .observation
                            .log_fingerprint
                            .clone()
                            .unwrap_or_default(),
                    }
                }
                (
                    ControlAction::Selection { model, thought, .. },
                    Capability::Model | Capability::ThoughtLevel,
                ) => ConfirmationTarget::Selection {
                    // provider 不随模型切换变化：取登记时刻快照值（缺席 = 空串，
                    // 三项精确相等判据自然不可能确认，保守不升级）。
                    provider: baseline.observation.provider.clone().unwrap_or_default(),
                    model: model.clone(),
                    thought: thought.clone(),
                },
                // 问答作答：目标 = 动作指向的宿主卡 requestKey（本实现取
                // `PendingView.interaction_id`；只存 id 形状，绝不存题目/选项/答案）。
                (
                    ControlAction::Answer { interaction_id, .. },
                    Capability::SingleSelect | Capability::MultiSelect | Capability::FreeText,
                ) => ConfirmationTarget::Pending {
                    request_key: interaction_id.clone(),
                },
                // 审批应答：同上，且登记只发生在用户对**既有** permission 卡的动作
                // （spec §4.1）——发送触发的新审批卡不在任何登记面上。
                (ControlAction::Permission { interaction_id, .. }, Capability::ApprovalReply) => {
                    ConfirmationTarget::Pending {
                        request_key: interaction_id.clone(),
                    }
                }
                // 打断：目标守卫 = 动作自带的 `expectedForegroundExecutionId`（只存
                // id 形状）。初始 control 未宣告（None）或目标守卫已不在 activeWorks
                // → 没有可等待的守卫，**不登记**（无确认窗、不触发、不确认）。
                (
                    ControlAction::Stop {
                        expected_foreground_execution_id,
                        ..
                    },
                    Capability::Interrupt,
                ) => {
                    let guard_present = baseline
                        .observation
                        .active_works
                        .as_ref()
                        .is_some_and(|guards| guards.contains(expected_foreground_execution_id));
                    if !guard_present {
                        continue;
                    }
                    ConfirmationTarget::Interrupt {
                        guard_id: expected_foreground_execution_id.clone(),
                    }
                }
                _ => continue,
            };
            let ctx = ConfirmationContext {
                capability: *capability,
                key: baseline.key.clone(),
                session_id: action.session_id().to_owned(),
                generation,
                baseline_revision: baseline.observation.revision.unwrap_or(0),
                target,
                deadline: Instant::now()
                    .checked_add(self.confirmation_timeout)
                    .unwrap_or_else(Instant::now),
            };
            // 同槽位已有 pending → 忽略再登记（首个确认窗未决期间不开新窗）。
            if self.confirmations.register(ctx.clone()) {
                self.spawn_confirmation_timer(ctx.deadline);
            }
        }
    }

    /// 一条有界确认计时器：到点未决 → inconclusive（绝不重发动作）。
    ///
    /// 醒来时现读 Hub 当前代际交给 sweep：登记代际若已过期（配置变更/断开/冲突），
    /// 到点只摘除零落账。`Weak` 持 Hub 状态——Hub 释放后计时器直接退出。
    fn spawn_confirmation_timer(&self, deadline: Instant) {
        let tracker = self.confirmations.clone();
        let inner = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            let now = Instant::now();
            if deadline > now {
                tokio::time::sleep(deadline - now).await;
            }
            if let Some(inner) = inner.upgrade() {
                let current_generation = lock_inner(&inner).generation;
                tracker.sweep_expired(Instant::now(), current_generation);
            }
        });
    }

    /// 广播一条动作回执事件；设备 id 只留在进程内供 SSE 同会话过滤。
    fn emit_action(&self, receipt: &ActionReceipt, device_id: &str) {
        emit_event(
            &self.inner,
            &self.events,
            ControlEventKind::Action,
            receipt.generation,
            Some(receipt.session_id.clone()),
            Some(device_id.to_owned()),
            serde_json::to_value(receipt).unwrap_or(Value::Null),
        );
    }

    /// 当前代际的「可控制」门（未配置→needs_config，未授权→needs_auth，未配对→状态码）。
    fn control_generation(&self) -> ZResult<u64> {
        let inner = self.lock();
        if !inner.configured {
            return Err(ZcodeError::of(codes::NEEDS_CONFIG));
        }
        if !inner.authorized {
            return Err(ZcodeError::of(codes::NEEDS_AUTH));
        }
        if inner.state != ConnectionState::Paired {
            return Err(Self::state_error(inner.state));
        }
        Ok(inner.generation)
    }

    /// Answer/Snooze 绑定前的**廉价否决**（零 bind、零 I/O、不消费许可）。
    ///
    /// 当前策略的四项问答/审批能力（`single_select`/`multi_select`/`free_text`/
    /// `approval_reply`）全关，且（证据构建）没有任何匹配本会话/代际/本题的可用
    /// 许可 → 直接 `needs_evidence`。任一能力开闸或存在可用许可才继续 bind，由宿主
    /// pending 卡推导精确能力集合后再逐项过闸。
    ///
    /// Snooze **不是**免闸辅助操作：它同样必须过宿主题型能力闸。生产政策全关时
    /// 服务端直接拒绝（零 bind/零执行），手机端按钮由同一 `SessionView.capabilities`
    /// 矩阵禁用，不会发出请求。
    fn check_answer_precheck(&self, action: &ControlAction) -> ZResult<()> {
        let policy = self.current_policy();
        if policy.single_select.enabled
            || policy.multi_select.enabled
            || policy.free_text.enabled
            || policy.approval_reply.enabled
        {
            return Ok(());
        }
        #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
        {
            let interaction_id = match action {
                ControlAction::Answer { interaction_id, .. }
                | ControlAction::Snooze { interaction_id, .. } => interaction_id.as_str(),
                _ => return Err(ZcodeError::of(codes::INVALID_ACTION)),
            };
            for capability in [
                "single_select",
                "multi_select",
                "free_text",
                "approval_reply",
            ] {
                if self.evidence.available_answer(
                    action.session_id(),
                    action.generation(),
                    capability,
                    interaction_id,
                ) {
                    return Ok(());
                }
            }
        }
        #[cfg(not(all(feature = "zcode-bridge-evidence", debug_assertions)))]
        {
            let _ = action;
        }
        Err(ZcodeError::of(codes::NEEDS_EVIDENCE))
    }

    /// 静态动作能力闸（bind 前）：Send/Stop/Permission/Selection。
    ///
    /// T8.2 证据构建：能力被策略关闭时，若存在**匹配本次会话/代际/能力**的有效许可，
    /// 恰好放行这一次动作并原子消费（answer/permission 走 snooze 绑题消费）；返回
    /// 被许可放宽的能力名，供 `CommandSender` 的同级闸放行同一动作。普通构建没有
    /// 许可册，能力关闭即拒绝，行为与旧版完全一致。
    ///
    /// Answer/Snooze 不在此列：它们必须在目标绑定后按宿主卡题型推导全部能力，
    /// 见 [`Self::check_required_policy`]。
    fn check_policy(&self, action: &ControlAction) -> ZResult<Vec<&'static str>> {
        // 与 `session_capabilities` 同一现算源：手机矩阵与执行闸不允许不同步。
        let policy = self.current_policy();
        let mut required: Vec<(&'static str, &super::types::CapabilityFlag, Option<&str>)> =
            Vec::new();
        match action {
            ControlAction::Send { .. } => required.push(("send", &policy.send, None)),
            ControlAction::Permission { interaction_id, .. } => required.push((
                "approval_reply",
                &policy.approval_reply,
                Some(interaction_id),
            )),
            ControlAction::Stop { .. } => required.push(("interrupt", &policy.interrupt, None)),
            ControlAction::Selection { .. } => {
                required.push(("model", &policy.model, None));
                required.push(("thought_level", &policy.thought_level, None));
            }
            // Answer/Snooze 的所需能力只能来自宿主 pending 卡：必须先绑定目标再走
            // `check_required_policy`。走到这里说明调用顺序错误，保守零执行。
            ControlAction::Answer { .. } | ControlAction::Snooze { .. } => {
                return Err(ZcodeError::of(codes::INVALID_ACTION));
            }
        }
        #[cfg_attr(
            not(all(feature = "zcode-bridge-evidence", debug_assertions)),
            allow(unused_mut)
        )]
        let mut permitted: Vec<&'static str> = Vec::new();
        #[cfg_attr(
            not(all(feature = "zcode-bridge-evidence", debug_assertions)),
            allow(unused_variables)
        )]
        for (capability, flag, interaction_id) in required {
            if flag.enabled {
                continue;
            }
            #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
            {
                // 许可只放宽「能力闸」一项：会话/代际/能力/绑题必须全部匹配；
                // 已 snooze 绑题的许可只能由该题的最终 answer/permission 消费。
                let matched = match interaction_id {
                    Some(interaction_id) => self.evidence.consume_answer(
                        action.session_id(),
                        action.generation(),
                        capability,
                        interaction_id,
                    ),
                    None => {
                        self.evidence
                            .consume(action.session_id(), action.generation(), capability)
                    }
                };
                if matched {
                    permitted.push(capability);
                    continue;
                }
            }
            let code = flag
                .reason_code
                .clone()
                .unwrap_or_else(|| codes::NEEDS_EVIDENCE.to_owned());
            return Err(ZcodeError { code });
        }
        Ok(permitted)
    }

    /// Answer/Snooze 的宿主题型能力闸：逐项检查**全部**所需能力（策略开启，或
    /// 本会话/本代际/本题存在可消费的验收许可），全部通过后才消费许可。
    ///
    /// 能力名由 [`HubBackend::required_capabilities`] 读取宿主 pending 卡推导；
    /// 客户端 `selectedValues` 的数量/freeText 内容**不**参与判定（否则多选卡只
    /// 传一个值即可借单选许可放行，混合题也会漏检）。Answer 采用 all-or-nothing：
    /// 先只读检查全部项，再逐项原子消费；任一项缺失时零消费、零执行。
    fn check_required_policy(
        &self,
        action: &ControlAction,
        required: &[&'static str],
    ) -> ZResult<Vec<&'static str>> {
        let policy = self.current_policy();
        let session_id = action.session_id();
        let generation = action.generation();
        let interaction_id = match action {
            ControlAction::Answer { interaction_id, .. }
            | ControlAction::Snooze { interaction_id, .. } => interaction_id.as_str(),
            _ => return Err(ZcodeError::of(codes::INVALID_ACTION)),
        };
        #[cfg_attr(
            not(all(feature = "zcode-bridge-evidence", debug_assertions)),
            allow(unused_mut)
        )]
        let mut permit_backed: Vec<&'static str> = Vec::new();
        for capability in required {
            let Some(flag) = capability_flag(&policy, capability) else {
                return Err(ZcodeError::of(codes::NEEDS_EVIDENCE));
            };
            if flag.enabled {
                continue;
            }
            #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
            {
                // 只读预检：所有能力都确认可用后才消费，避免「第一项已消费、
                // 第二项缺失」的部分放行。
                if self.evidence.available_answer(
                    session_id,
                    generation,
                    capability,
                    interaction_id,
                ) {
                    permit_backed.push(*capability);
                    continue;
                }
            }
            let code = flag
                .reason_code
                .clone()
                .unwrap_or_else(|| codes::NEEDS_EVIDENCE.to_owned());
            return Err(ZcodeError { code });
        }
        // 全量检查通过后才动许可：Answer 消费；Snooze 只绑题不消费。
        #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
        {
            match action {
                ControlAction::Answer { .. } => {
                    for capability in &permit_backed {
                        if !self.evidence.consume_answer(
                            session_id,
                            generation,
                            capability,
                            interaction_id,
                        ) {
                            // 并发调用抢走其中一项：整体拒绝，绝不放行半套。
                            return Err(ZcodeError::of(codes::NEEDS_EVIDENCE));
                        }
                    }
                }
                // 首次 snooze 只把本题 required 的答案/审批许可绑到该题，不消费，
                // 绝不绑定 send/model 等非答案许可。
                ControlAction::Snooze { .. } => {
                    let bound = permit_backed.is_empty()
                        || self.evidence.snooze_interaction_limited(
                            session_id,
                            generation,
                            interaction_id,
                            &permit_backed,
                        );
                    if !bound {
                        return Err(ZcodeError::of(codes::NEEDS_EVIDENCE));
                    }
                }
                _ => {}
            }
        }
        #[cfg(not(all(feature = "zcode-bridge-evidence", debug_assertions)))]
        {
            let _ = (session_id, generation, interaction_id);
        }
        Ok(permit_backed)
    }

    /// 失败收口：代际未变才写状态并广播；旧代际失败一律 `stale_generation`。
    fn apply_failure(&self, generation: u64, error: ZcodeError) -> ZcodeError {
        let (view, stale) = {
            let mut inner = self.lock();
            if inner.generation != generation {
                return ZcodeError::of(codes::STALE_GENERATION);
            }
            let state = Self::state_of(&error);
            if state == ConnectionState::NeedsAuth {
                inner.authorized = false;
            }
            inner.state = state;
            inner.reason_code = Some(error.code.clone());
            inner.device_label = None;
            let view = inner.view();
            let stale = inner.stale_sessions(view.generation);
            (view, stale)
        };
        self.emit_connection(&view);
        for stale in &stale {
            self.emit_session(stale);
        }
        error
    }

    /// 写状态并广播一条连接事件（离开 paired 时附带投影降级事件）。
    fn publish(&self, state: ConnectionState, reason_code: Option<String>) -> ConnectionView {
        self.settle(state, reason_code, None).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::dao::settings::{try_get_setting_conn, try_set_setting_conn};
    use crate::database::dao::zcode_capability::{CapabilityLedger, DbCapabilityLedger};
    use crate::services::zcode_control::auth::{AuthProblem, AuthService, AuthStatus};
    use crate::services::zcode_control::auto_connect::AUTO_CONNECT_KEY;
    use crate::services::zcode_control::capability::{
        Capability, CapabilityKey, CapabilityRecord, CapabilityState, ConfirmationBaseline,
        ConfirmationObservation,
    };
    use crate::services::zcode_control::config::set_config;
    use crate::services::zcode_control::credential_store::{
        MemoryCredentialStore, StoredCredential, SCHEMA_VERSION,
    };
    use crate::services::zcode_control::types::{
        ActionState, ActiveWork, AnswerEntry, CapabilityFlag, ControlView, PendingKind,
        PendingView, SelectionView,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::time::Duration;
    use tokio::sync::Notify;

    const URL: &str = "https://zcode.z.ai/remote/v4?sid=synthetic-hub-0001&name=SyntheticHub";
    const URL_WITH_MID: &str = "https://zcode.z.ai/remote/v4?sid=synthetic-hub-0001&mid=synthetic-mid-0002&name=SyntheticHub";
    const URL_NEXT: &str = "https://zcode.z.ai/remote/v4?sid=synthetic-hub-0002&name=SyntheticHub2";

    #[derive(Default)]
    struct FakeBackend {
        authorize_calls: AtomicUsize,
        connect_calls: AtomicUsize,
        disconnect_calls: AtomicUsize,
        /// T5 确认观察：`confirmation_baseline` 的替身应答（`None` = 后端不提供
        /// baseline，登记跳过——既有用例零改动）。
        confirmation_baseline: Mutex<Option<ConfirmationBaseline>>,
        /// T5 确认观察：下一次 `execute` 的替身回执（`None` = 默认 Accepted）。
        execute_receipt: Mutex<Option<ZResult<ActionReceipt>>>,
        /// 一次性 `execute` 闸：武装后下一次 execute 在替身内等待放行（登记已完成、
        /// 回执未返回的「派发在途」窗口测试用）。
        execute_gate: Mutex<Option<Arc<Notify>>>,
        /// 一次性 `disconnect()` 闸：第一次调用等待放行，后续调用立即返回
        /// （关闭清理重叠窗口测试用）。
        disconnect_gate: Mutex<Option<Arc<Notify>>>,
        reset_calls: AtomicUsize,
        bind_calls: AtomicUsize,
        execute_calls: AtomicUsize,
        /// 在途 connect 的闸（迟到结果测试用）。
        gate: Mutex<Option<Arc<Notify>>>,
        /// 在途 `session()` 的闸（GET 在途掉线测试用）。
        session_gate: Mutex<Option<Arc<Notify>>>,
        /// `session()` 进入次数（配合闸判断在途）。
        session_calls: AtomicUsize,
        /// 下一次 `session()` 返回的投影是否不新鲜（默认 false = ready）。
        session_unready: AtomicBool,
        /// 在途 reset 的闸（配置变更窗口测试用）。
        reset_gate: Mutex<Option<Arc<Notify>>>,
        /// reset 是否模拟「断掉 relay 连接」（默认 false 保持既有用例；P2-3 回归开 true
        /// 后，陈旧 config_changed 若仍执行 reset 会把新连接的 live 断掉）。
        reset_drops_live: AtomicBool,
        connect_result: Mutex<Option<ZResult<ConnectionView>>>,
        /// 持久幂等查询的替身结果（`None` = 无既有回执，即默认语义）。
        existing_result: Mutex<Option<ZResult<Option<ActionReceipt>>>>,
        /// 持久幂等查询被调用次数（证明查询先于连接/许可闸）。
        existing_calls: AtomicUsize,
        bind_generation: AtomicU64,
        /// 每次 connect 收到的目标（`sid` + `mid`），用于证明只从保存链接解析。
        routes: Mutex<Vec<(String, Option<String>)>>,
        /// 每次 reset 收到的目标代际。
        reset_generations: Mutex<Vec<u64>>,
        /// 每次 execute 收到的目标（证明 Hub 已把公开/桥两套代际分开盖好）。
        executed_targets: Mutex<Vec<BoundTarget>>,
        /// `required_capabilities` 的替身应答（`None` = 默认 `["single_select"]`）。
        required_result: Mutex<Option<ZResult<Vec<&'static str>>>>,
        /// `required_capabilities` 被调用次数（证明 Hub 在绑定后读取宿主题型）。
        required_calls: AtomicUsize,
        /// `restore_authorization` 的替身应答（默认未授权，须由用例显式设定）。
        restored_auth: Mutex<AuthStatus>,
        /// 当前授权状态（authorize/restore/forget 会更新；config_changed 读取它）。
        auth_state: Mutex<AuthStatus>,
        /// `restore_authorization` 被调用次数（证明启动恢复只做一次）。
        restore_calls: AtomicUsize,
        /// `forget_authorization` 被调用次数与替身应答。
        forget_calls: AtomicUsize,
        forget_result: Mutex<Option<AuthStatus>>,
        /// `forget_authorization` 的一次性闸：第一次调用等待放行（替身删除/清内存在途）。
        forget_gate: Mutex<Option<Arc<Notify>>>,
        /// 替身是否持有账号 JWT（authorize/restore 置位、forget 清除）。
        jwt: AtomicBool,
        /// 替身 relay 是否有一条 live 连接（成功配对置位、disconnect 清除）。
        ///
        /// P2-4 的关键观察位：旧代在途握手若在删除期间完成，它会把这里置 true；
        /// Hub 对 stale 结果不调 `backend.disconnect`，于是旧 JWT 连接会一直活着。
        live: AtomicBool,
    }

    impl FakeBackend {
        fn paired_view() -> ConnectionView {
            ConnectionView {
                state: ConnectionState::Paired,
                generation: 1,
                reason_code: None,
                device_label: Some("fake-device".into()),
            }
        }

        fn set_connect(&self, result: ZResult<ConnectionView>) {
            self.connect_result.lock().unwrap().replace(result);
        }

        /// 设定 `restore_authorization` 的替身应答（模拟系统保险箱里的持久身份）。
        ///
        /// **默认未授权**：需要验证恢复路径的用例必须显式设定，绝不用默认 true 掩盖
        /// 漏接线。
        fn set_restored_auth(&self, status: AuthStatus) {
            *self.restored_auth.lock().unwrap() = status;
        }

        /// 设定 `forget_authorization` 的替身应答（如 `DeleteFailed`）。
        fn set_forget_result(&self, status: AuthStatus) {
            self.forget_result.lock().unwrap().replace(status);
        }

        /// 替身当前是否持有账号 JWT（忘记后必须为 false）。
        fn holds_jwt(&self) -> bool {
            self.jwt.load(Ordering::SeqCst)
        }

        /// 替身 relay 是否仍有 live 连接（删除期间必须为 false）。
        fn is_live(&self) -> bool {
            self.live.load(Ordering::SeqCst)
        }

        /// 设定 `required_capabilities` 的应答（缺省 = `["single_select"]`，与既有
        /// Answer/Snooze 用例的合成卡兼容）。
        fn set_required(&self, result: ZResult<Vec<&'static str>>) {
            self.required_result.lock().unwrap().replace(result);
        }

        /// 设定持久幂等查询的应答（`Some(Err(..))` = 冲突；缺省 = 无既有回执）。
        fn set_existing(&self, result: ZResult<Option<ActionReceipt>>) {
            self.existing_result.lock().unwrap().replace(result);
        }

        /// 设定 `confirmation_baseline` 的替身应答（T5 确认观察登记面）。
        fn set_baseline(&self, baseline: ConfirmationBaseline) {
            self.confirmation_baseline.lock().unwrap().replace(baseline);
        }

        /// 设定下一次 `execute` 的替身回执状态（T5：Confirmed/Rejected/Unknown）。
        fn set_execute_receipt(&self, state: ActionState, reason: Option<String>) {
            let receipt = ActionReceipt {
                action_id: "synthetic-receipt".into(),
                session_id: "sess_synthetic0000".into(),
                generation: 0,
                state,
                reason_code: reason,
            };
            self.execute_receipt.lock().unwrap().replace(Ok(receipt));
        }

        /// 武装一次性 `execute` 闸：下一次 execute 在替身内等待放行。返回放行把手
        /// （回执收口测试用它制造「登记已完成、派发在途」的换代窗口）。
        fn arm_execute_gate(&self) -> Arc<Notify> {
            let gate = Arc::new(Notify::new());
            self.execute_gate.lock().unwrap().replace(gate.clone());
            gate
        }
    }

    impl HubBackend for FakeBackend {
        fn authorize<'a>(
            &'a self,
            provider: OAuthProvider,
            _open: OpenAuthorize,
        ) -> BoxFuture<'a, ZResult<()>> {
            self.authorize_calls.fetch_add(1, Ordering::SeqCst);
            // 记录授权路径的真实状态：授权成功后 `auth_status` 必须已授权，
            // 这样 config_changed 才能证明「保留账号、不重新 OAuth」。
            *self.auth_state.lock().unwrap() = AuthStatus {
                authorized: true,
                remembered: false,
                restoring: false,
                provider: Some(provider),
                problem: None,
            };
            self.jwt.store(true, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }

        fn restore_authorization<'a>(&'a self) -> BoxFuture<'a, AuthStatus> {
            self.restore_calls.fetch_add(1, Ordering::SeqCst);
            // 与 AuthService 同语义：已有内存授权直接返回，不读持久记录。
            let current = {
                let mut state = self.auth_state.lock().unwrap();
                if state.authorized {
                    state.clone()
                } else {
                    let restored = self.restored_auth.lock().unwrap().clone();
                    *state = restored.clone();
                    restored
                }
            };
            if current.authorized {
                self.jwt.store(true, Ordering::SeqCst);
            }
            Box::pin(async move { current })
        }

        fn auth_status(&self) -> AuthStatus {
            self.auth_state.lock().unwrap().clone()
        }

        fn forget_authorization<'a>(&'a self) -> BoxFuture<'a, AuthStatus> {
            self.forget_calls.fetch_add(1, Ordering::SeqCst);
            let gate = self.forget_gate.lock().unwrap().take();
            let status = self
                .forget_result
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_default();
            Box::pin(async move {
                // 删除/清内存在途：闸放行前 AuthService 仍持有旧 JWT（正是要封的窗口）。
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                *self.auth_state.lock().unwrap() = status.clone();
                // 持久记录已删除 = 之后的 restore 不再返回已授权（替身的最简建模）。
                *self.restored_auth.lock().unwrap() = status.clone();
                self.jwt.store(false, Ordering::SeqCst);
                status
            })
        }

        fn connect<'a>(&'a self, route: DeviceRoute) -> BoxFuture<'a, ZResult<ConnectionView>> {
            self.connect_calls.fetch_add(1, Ordering::SeqCst);
            self.routes.lock().unwrap().push((route.sid, route.mid));
            Box::pin(async move {
                let gate = self.gate.lock().unwrap().clone();
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                let result = self
                    .connect_result
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| Ok(Self::paired_view()));
                // 替身 relay：握手成功 = 一条 live 连接（关闭清理必须把它关掉）。
                if matches!(&result, Ok(view) if view.state == ConnectionState::Paired) {
                    self.live.store(true, Ordering::SeqCst);
                }
                result
            })
        }

        fn disconnect<'a>(&'a self) -> BoxFuture<'a, ()> {
            self.disconnect_calls.fetch_add(1, Ordering::SeqCst);
            // 一次性：只有第一次调用取走闸并等待（后续立即返回）。
            let gate = self.disconnect_gate.lock().unwrap().take();
            Box::pin(async move {
                // 替身 relay：断开即无 live 连接。
                self.live.store(false, Ordering::SeqCst);
                if let Some(gate) = gate {
                    gate.notified().await;
                }
            })
        }

        fn reset<'a>(&'a self, generation: u64) -> BoxFuture<'a, ()> {
            self.reset_calls.fetch_add(1, Ordering::SeqCst);
            self.reset_generations.lock().unwrap().push(generation);
            let drops_live = self.reset_drops_live.load(Ordering::SeqCst);
            Box::pin(async move {
                // 进入 reset = relay 断开/投影作废：开了副作用开关就断掉 live 连接。
                if drops_live {
                    self.live.store(false, Ordering::SeqCst);
                }
                let gate = self.reset_gate.lock().unwrap().clone();
                if let Some(gate) = gate {
                    gate.notified().await;
                }
            })
        }

        fn bind_session<'a>(&'a self, key: &'a TargetKey) -> BoxFuture<'a, ZResult<BoundTarget>> {
            self.bind_calls.fetch_add(1, Ordering::SeqCst);
            let session_id = key.session_id.clone();
            let generation = self.bind_generation.load(Ordering::SeqCst);
            Box::pin(async move {
                Ok(BoundTarget {
                    session_id,
                    workspace_path: "/synthetic/workspace".into(),
                    workspace_identity: None,
                    workspace_key: "/synthetic/workspace".into(),
                    generation,
                    bridge_generation: None,
                })
            })
        }

        fn session<'a>(&'a self, target: &'a BoundTarget) -> BoxFuture<'a, ZResult<SessionView>> {
            self.session_calls.fetch_add(1, Ordering::SeqCst);
            let ready = !self.session_unready.load(Ordering::SeqCst);
            Box::pin(async move {
                let gate = self.session_gate.lock().unwrap().clone();
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                Ok(SessionView {
                    session_id: target.session_id.clone(),
                    generation: target.generation,
                    revision: Some(1),
                    ready,
                    pending: Vec::new(),
                    control: None,
                    selection: None,
                    capabilities: CapabilityView::default(),
                })
            })
        }

        fn execute<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            _action: &'a ControlAction,
        ) -> BoxFuture<'a, ZResult<ActionReceipt>> {
            self.execute_calls.fetch_add(1, Ordering::SeqCst);
            self.executed_targets.lock().unwrap().push(target.clone());
            let receipt = self.execute_receipt.lock().unwrap().clone();
            let gate = self.execute_gate.lock().unwrap().take();
            let receipt = receipt.unwrap_or_else(|| {
                Ok(ActionReceipt {
                    action_id: ctx.action_id.clone(),
                    session_id: target.session_id.clone(),
                    // 生产回执盖的是投影代际（≠ Hub 代际）：Hub 必须覆写为公开代际。
                    generation: target.generation,
                    state: ActionState::Accepted,
                    reason_code: None,
                })
            });
            Box::pin(async move {
                // 一次性闸：模拟「登记已完成、派发在途」的窗口（闸在计数之后）。
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                receipt
            })
        }

        fn existing_receipt<'a>(
            &'a self,
            _ctx: &'a ActionContext,
            _action: &'a ControlAction,
        ) -> BoxFuture<'a, ZResult<Option<ActionReceipt>>> {
            self.existing_calls.fetch_add(1, Ordering::SeqCst);
            let result = self
                .existing_result
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(Ok(None));
            Box::pin(async move { result })
        }

        fn required_capabilities<'a>(
            &'a self,
            _target: &'a BoundTarget,
            _action: &'a ControlAction,
        ) -> BoxFuture<'a, ZResult<Vec<&'static str>>> {
            self.required_calls.fetch_add(1, Ordering::SeqCst);
            let result = self
                .required_result
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| Ok(vec!["single_select"]));
            Box::pin(async move { result })
        }

        fn policy(&self) -> CapabilityView {
            CapabilityView::default()
        }

        fn confirmation_baseline<'a>(
            &'a self,
            _session_id: &'a str,
        ) -> BoxFuture<'a, Option<ConfirmationBaseline>> {
            let baseline = self.confirmation_baseline.lock().unwrap().clone();
            Box::pin(async move { baseline })
        }
    }

    fn send_policy() -> CapabilityView {
        CapabilityView {
            send: CapabilityFlag {
                enabled: true,
                reason_code: None,
            },
            ..CapabilityView::default()
        }
    }

    /// 测试用：按名称开启策略里的能力项（其余保持默认关闭 + 冻结原因码）。
    fn open_capability(view: &mut CapabilityView, name: &str) {
        let flag = match name {
            "send" => &mut view.send,
            "single_select" => &mut view.single_select,
            "multi_select" => &mut view.multi_select,
            "free_text" => &mut view.free_text,
            "interrupt" => &mut view.interrupt,
            "approval_reply" => &mut view.approval_reply,
            "model" => &mut view.model,
            "thought_level" => &mut view.thought_level,
            other => panic!("unknown capability {other}"),
        };
        flag.enabled = true;
        flag.reason_code = None;
    }

    fn policy_with(names: &[&str]) -> CapabilityView {
        let mut view = CapabilityView::default();
        for name in names {
            open_capability(&mut view, name);
        }
        view
    }

    /// 可动态翻转的策略源 + 假后端（不依赖 static `with_parts` 的单份快照）。
    fn hub_with_policy(
        fake: Arc<FakeBackend>,
        policy: Arc<Mutex<CapabilityView>>,
    ) -> Arc<ZcodeHub> {
        let source: CapabilitySource = {
            let policy = policy.clone();
            Arc::new(move || policy.lock().unwrap().clone())
        };
        Arc::new(ZcodeHub::with_parts_and_policy_source(
            stored(URL),
            fake,
            source,
        ))
    }

    /// 证据构建测试：默认策略（全关）+ 指定宿主所需能力的已配对 Hub。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    async fn closed_evidence_hub(
        fake: Arc<FakeBackend>,
        required: Vec<&'static str>,
    ) -> (Arc<ZcodeHub>, u64) {
        fake.set_required(Ok(required));
        let hub = hub_with_policy(fake, Arc::new(Mutex::new(CapabilityView::default())));
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let generation = hub.connection().generation;
        (hub, generation)
    }

    fn stored(url: &str) -> DeviceStore {
        let store = DeviceStore::memory();
        store.with(|conn| set_config(conn, url).unwrap());
        store
    }

    /// T4 装配裁决钉住：capability key 对同证据稳定（账本跨重连存活的前提），
    /// build/协议变化即换 key；schema 指纹是占位常量（不含正文/凭据），probe
    /// epoch 本任务恒 0。
    #[test]
    fn capability_key_is_stable_per_evidence_and_content_free() {
        let evidence = HostEvidence {
            app_build: Some("3.16.2.8769".to_owned()),
            protocol_version: Some(3),
            ..Default::default()
        };
        assert_eq!(capability_key(&evidence), capability_key(&evidence));
        // build / 协议变化 → 不同 key（失效单元语义）。
        let mut upgraded = evidence.clone();
        upgraded.app_build = Some("3.16.2.9999".to_owned());
        assert_ne!(capability_key(&upgraded), capability_key(&evidence));
        let mut pre_hello = evidence.clone();
        pre_hello.protocol_version = None;
        assert_ne!(capability_key(&pre_hello), capability_key(&evidence));
        // 缺证据按未知处理（空 build / 协议 0），五元组其余字段为固定占位。
        let unknown = capability_key(&HostEvidence::default());
        assert_ne!(unknown, capability_key(&evidence));
        assert_eq!(unknown.platform, std::env::consts::OS);
        assert_eq!(unknown.app_build, "");
        assert_eq!(unknown.protocol_version, 0);
        assert_eq!(unknown.schema_fingerprint, "v4");
        assert_eq!(unknown.probe_epoch, 0);
    }

    /// 一个已授权的持久身份（恢复路径的替身；不携带 JWT）。
    fn authorized_auth() -> AuthStatus {
        AuthStatus {
            authorized: true,
            remembered: true,
            restoring: false,
            provider: Some(OAuthProvider::Bigmodel),
            problem: None,
        }
    }

    fn set_remote_enabled(store: &DeviceStore, enabled: bool) {
        store.with(|conn| {
            try_set_setting_conn(
                conn,
                crate::remote::KEY_ENABLED,
                if enabled { "true" } else { "false" },
            )
            .unwrap()
        });
    }

    /// 读自动连接持久意图的原始值（`None` = 缺键 = 默认 true）。
    fn stored_auto(store: &DeviceStore) -> Option<String> {
        store.with(|conn| try_get_setting_conn(conn, AUTO_CONNECT_KEY).unwrap())
    }

    /// 让 settings 读写失败（模拟 DB 故障）：Hub 已构造后再破坏，可验证 checked 写。
    fn break_settings(store: &DeviceStore) {
        store.with(|conn| conn.execute("DROP TABLE settings", []).unwrap());
    }

    /// 等到目标连接态（既有事件订阅 + 1s 超时；只供 fake 测试）。
    async fn wait_for_connection(hub: &ZcodeHub, state: ConnectionState) {
        if hub.connection().state == state {
            return;
        }
        let mut events = hub.subscribe();
        let expected = serde_json::to_value(state).unwrap();
        let wait = async {
            loop {
                if hub.connection().state == state {
                    return;
                }
                match events.recv().await {
                    Ok(event)
                        if event.kind == ControlEventKind::Connection
                            && event.data["state"] == expected =>
                    {
                        if hub.connection().state == state {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => panic!("事件通道关闭"),
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(1), wait)
            .await
            .expect("1s 内必须到达目标连接态");
    }

    /// 等 worker 至少执行到第 `at_least` 次尝试（同步信号；负向断言据此确认
    /// 「worker 已跑过」而不是靠固定 sleep 猜时序）。
    async fn wait_for_auto_attempt(hub: &ZcodeHub, at_least: u64) {
        for _ in 0..200 {
            if hub.auto_attempt_count() >= at_least {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "自动连接 worker 未在预期内尝试：{} < {}",
            hub.auto_attempt_count(),
            at_least
        );
    }

    /// 等 checked 写入真正落库（KICKED 的持久暂停走 blocking 池）。
    async fn wait_for_stored_auto(store: &DeviceStore, expected: &str) {
        for _ in 0..200 {
            if stored_auto(store).as_deref() == Some(expected) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "自动连接意图未落库为 {expected}，实际 {:?}",
            stored_auto(store)
        );
    }

    /// 让已 spawn 的任务/事件泵跑起来（只做短让步，不引入真实等待）。
    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    fn noop_open() -> OpenAuthorize {
        Arc::new(|_| Ok(()))
    }

    fn ctx(action_id: &str) -> ActionContext {
        ActionContext {
            action_id: action_id.into(),
            device_id: "synthetic-device".into(),
            device_name: "Synthetic".into(),
        }
    }

    fn send_action(generation: u64) -> ControlAction {
        ControlAction::Send {
            session_id: "sess_synthetic0000".into(),
            generation,
            text: "synthetic text".into(),
        }
    }

    fn answer_action(
        generation: u64,
        interaction_id: &str,
        selected_values: Vec<&str>,
        free_text: Option<&str>,
    ) -> ControlAction {
        ControlAction::Answer {
            session_id: "sess_synthetic0000".into(),
            generation,
            interaction_id: interaction_id.into(),
            answers: vec![AnswerEntry {
                question_text: "synthetic host question".into(),
                selected_values: selected_values.into_iter().map(str::to_owned).collect(),
                free_text: free_text.map(str::to_owned),
            }],
        }
    }

    fn snooze_action(generation: u64, interaction_id: &str) -> ControlAction {
        ControlAction::Snooze {
            session_id: "sess_synthetic0000".into(),
            generation,
            interaction_id: interaction_id.into(),
        }
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        for _ in 0..1_000 {
            if condition() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("条件在让步循环内未成立");
    }

    async fn paired_hub(fake: Arc<FakeBackend>) -> Arc<ZcodeHub> {
        let hub = Arc::new(ZcodeHub::with_parts(
            stored(URL),
            fake.clone(),
            send_policy(),
        ));
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        hub
    }

    /// 配置变更：先推进代际，旧回调（在途 connect）与旧动作全部作废且零后端执行。
    #[tokio::test]
    async fn config_change_invalidates_callback_and_pending() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        let old_generation = hub.connection().generation;
        let mut events = hub.subscribe();

        // 旧代际的在途连接：被闸停住，等配置变更后才返回。
        hub.disconnect().await;
        let gate = Arc::new(Notify::new());
        fake.gate.lock().unwrap().replace(gate.clone());
        let late = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.connect().await })
        };
        wait_until(|| fake.connect_calls.load(Ordering::SeqCst) >= 2).await;

        let view = hub.config_changed().await;
        gate.notify_one();
        let late_result = late.await.expect("spawn 不应 panic");
        assert!(matches!(late_result, Err(err) if err.is(codes::STALE_GENERATION)));

        let new_generation = view.generation;
        assert!(new_generation > old_generation);
        assert_eq!(
            view.state,
            ConnectionState::Offline,
            "换链接保留账号 JWT（只需要重连，不需重新授权）"
        );
        assert_eq!(
            hub.connection().state,
            ConnectionState::Offline,
            "旧连接已断，但授权保留"
        );

        // 旧代际动作：stale_generation 且零 bind/零 execute。
        let err = hub
            .execute(ctx("synthetic-action-1"), send_action(old_generation))
            .await
            .unwrap_err();
        assert!(err.is(codes::STALE_GENERATION));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 0);

        // 至少广播一条新代际的连接事件，且事件体不含凭据字段。
        let mut saw_connection = false;
        while let Ok(event) = events.try_recv() {
            if event.kind == ControlEventKind::Connection && event.generation == new_generation {
                saw_connection = true;
                let wire = serde_json::to_string(&event).unwrap();
                assert!(!wire.contains("jwt") && !wire.contains("token"));
            }
        }
        assert!(saw_connection);
    }

    /// 桌面 + 两台手机共享同一 Hub/后端：并发 connect 只开一条物理连接。
    #[tokio::test]
    async fn one_hub_shared_between_desktop_and_mobile() {
        let fake = Arc::new(FakeBackend::default());
        let hub = Arc::new(ZcodeHub::with_parts(
            stored(URL),
            fake.clone(),
            send_policy(),
        ));
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        let mut mobile_a = hub.subscribe();
        let mut mobile_b = hub.subscribe();

        let desktop = hub.clone();
        let first_mobile = hub.clone();
        let (first, second) = tokio::join!(desktop.connect(), first_mobile.connect());
        let first = first.unwrap();
        let second = second.unwrap();
        assert!(first.is_paired());
        assert!(second.is_paired());
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 1);
        assert_eq!(hub.connection(), first, "桌面读取同一视图，不另开连接");

        let mut notified = 0;
        for receiver in [&mut mobile_a, &mut mobile_b] {
            while let Ok(event) = receiver.try_recv() {
                if event.kind == ControlEventKind::Connection
                    && event.data["state"] == serde_json::json!("paired")
                {
                    notified += 1;
                    break;
                }
            }
        }
        assert_eq!(notified, 2, "两个订阅者都收到同一代际的连接事件");
    }

    /// 未保存链接 = needs_config（零授权/零连接）；已保存但无 JWT = needs_auth。
    #[tokio::test]
    async fn missing_config_is_needs_config_saved_is_needs_auth() {
        let fake = Arc::new(FakeBackend::default());
        let hub = ZcodeHub::with_parts(DeviceStore::memory(), fake.clone(), send_policy());
        assert_eq!(hub.connection().state, ConnectionState::NeedsConfig);
        assert!(hub.connect().await.unwrap_err().is(codes::NEEDS_CONFIG));
        assert!(hub
            .authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap_err()
            .is(codes::NEEDS_CONFIG));
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 0);

        let saved = ZcodeHub::with_parts(stored(URL), fake.clone(), send_policy());
        assert_eq!(saved.connection().state, ConnectionState::NeedsAuth);
        assert!(saved.connect().await.unwrap_err().is(codes::NEEDS_AUTH));
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 0);
    }

    /// 显式断开保留 JWT：状态 offline，但无需重新授权即可重连。
    #[tokio::test]
    async fn disconnect_keeps_jwt_but_offline() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;

        let view = hub.disconnect().await;
        assert_eq!(view.state, ConnectionState::Offline);
        assert_eq!(view.reason_code, None);
        assert_eq!(hub.connection().state, ConnectionState::Offline);
        assert_eq!(
            fake.disconnect_calls.load(Ordering::SeqCst),
            2,
            "断开 = 先取消在途 + 锁后再关一次（竞态窗口）"
        );

        let reconnected = hub.connect().await.unwrap();
        assert!(reconnected.is_paired());
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 1, "不重新授权");
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 2);
    }

    /// 握手在途时断开：Offline/代际必须**立即**落定（不得先等 `connect_lock`），
    /// 在途 connect 被作废（stale_generation），后端断开至少调用一次。
    #[tokio::test]
    async fn disconnect_invalidates_inflight_connect_without_waiting_for_lock() {
        let fake = Arc::new(FakeBackend::default());
        let hub = Arc::new(ZcodeHub::with_parts(
            stored(URL),
            fake.clone(),
            send_policy(),
        ));
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        let gate = Arc::new(Notify::new());
        fake.gate.lock().unwrap().replace(gate.clone());

        let in_flight = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.connect().await })
        };
        wait_until(|| fake.connect_calls.load(Ordering::SeqCst) == 1).await;

        let disconnecting = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.disconnect().await })
        };

        // 旧实现先等 connect_lock（被在途握手占着）→ 这里会一直停在 connecting。
        wait_until(|| hub.connection().state == ConnectionState::Offline).await;
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            1,
            "断开不得触发新的后端连接"
        );

        gate.notify_one();
        let late = in_flight.await.expect("spawn 不应 panic");
        assert!(
            matches!(&late, Err(err) if err.is(codes::STALE_GENERATION)),
            "在途 connect 必须作废，实际 {late:?}"
        );

        let view = disconnecting.await.expect("spawn 不应 panic");
        assert_eq!(view.state, ConnectionState::Offline);
        assert!(
            fake.disconnect_calls.load(Ordering::SeqCst) >= 1,
            "清理必须调用后端断开"
        );
        assert_eq!(hub.connection().state, ConnectionState::Offline);

        // 不粘死：清理结束后显式重连仍可达（用户断开保留 JWT，不重授权）。
        fake.gate.lock().unwrap().take();
        assert!(hub.connect().await.expect("显式重连应成功").is_paired());
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 1);
    }

    /// 断开清理窗口内的新 connect：`closing` 一置位就立即回 Offline 错误且不触达
    /// `backend.connect`（覆盖「begin 已落定、finish 尚未收尾」的竞态窗口）；
    /// 清理完成后 `closing` 复位，显式重连仍可达。
    #[tokio::test]
    async fn connect_during_disconnect_cleanup_is_rejected_without_backend_call() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        let connects_before = fake.connect_calls.load(Ordering::SeqCst);

        // begin = 远端关停的同步前半段：此时清理尚未开始（closing = Some(epoch)）。
        let (view, guard) = hub.begin_disconnect();
        assert_eq!(view.state, ConnectionState::Offline);

        let err = hub.connect().await.unwrap_err();
        assert!(
            err.is(codes::OFFLINE),
            "清理窗口内必须立刻回 offline 错误，实际 {err:?}"
        );
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            connects_before,
            "清理窗口内不得真的开新连接"
        );
        assert_eq!(hub.connection().state, ConnectionState::Offline);

        hub.finish_disconnect(guard).await;
        assert_eq!(
            fake.disconnect_calls.load(Ordering::SeqCst),
            2,
            "finish = 先取消在途 + 锁后再关一次"
        );
        let reconnected = hub.connect().await.expect("清理完成后显式重连应成功");
        assert!(reconnected.is_paired());
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            connects_before + 1,
            "closing 必须已复位"
        );
    }

    /// 清理 future 被取消（guard 直接 Drop）也要清位：`closing` 绝不永久锁死 connect。
    /// 凭据在 `begin_disconnect` 内同步创建，所以「从未 poll 就丢弃 future」同样覆盖。
    #[tokio::test]
    async fn dropped_close_guard_reopens_connect() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        let connects_before = fake.connect_calls.load(Ordering::SeqCst);

        let (_view, guard) = hub.begin_disconnect();
        // 窗口内：拒绝新连接（零 backend.connect）。
        assert!(hub.connect().await.unwrap_err().is(codes::OFFLINE));
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), connects_before);

        // 模拟清理 future 被取消/从未被调度：凭据随 future 析构而 Drop。
        drop(guard);
        let reconnected = hub.connect().await.expect("closing 必须已清位");
        assert!(reconnected.is_paired());
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            connects_before + 1,
            "Drop 后必须允许显式重连"
        );
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 1, "不重授权");
    }

    /// 重复 begin 会写新 epoch：旧 guard Drop 不得误清后来者的关闭窗口。
    #[tokio::test]
    async fn stale_close_guard_cannot_clear_newer_closing() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        let connects_before = fake.connect_calls.load(Ordering::SeqCst);

        let (_first, first_guard) = hub.begin_disconnect();
        let (_second, second_guard) = hub.begin_disconnect();
        // 旧 epoch 的凭据先 Drop：不得把新窗口清掉。
        drop(first_guard);
        let err = hub.connect().await.unwrap_err();
        assert!(err.is(codes::OFFLINE), "新关闭窗口必须仍生效，实际 {err:?}");
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), connects_before);

        // 新 epoch 的凭据 Drop 才清位。
        drop(second_guard);
        assert!(hub
            .connect()
            .await
            .expect("新窗口关闭后应可重连")
            .is_paired());
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            connects_before + 1
        );
    }

    /// 重叠关闭的清理必须按 epoch 归属串行：后一个关闭完成后，较早的清理任务不得再
    /// `backend.disconnect()` 把新连接关掉；陈旧凭据在新连接之后 finish 是零副作用。
    #[tokio::test]
    async fn old_cleanup_cannot_disconnect_a_newer_connection() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        assert_eq!(hub.connection().state, ConnectionState::Paired);

        // 一次性闸：只有 A 的第一次 disconnect 会等待。
        let gate = Arc::new(Notify::new());
        fake.disconnect_gate.lock().unwrap().replace(gate.clone());

        // A：begin + 异步清理，卡在第一次 disconnect。
        let (_view_a, guard_a) = hub.begin_disconnect();
        let task_a = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.finish_disconnect(guard_a).await })
        };
        wait_until(|| fake.disconnect_calls.load(Ordering::SeqCst) == 1).await;

        // B：新的关闭窗口接管（后一个），其清理必须排在 A 的清理窗口之后。
        let (_view_b, guard_b) = hub.begin_disconnect();
        let task_b = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.finish_disconnect(guard_b).await })
        };
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(
            !task_b.is_finished(),
            "后一个清理不得越过前一个仍在进行的清理窗口"
        );
        assert_eq!(
            fake.disconnect_calls.load(Ordering::SeqCst),
            1,
            "后一个清理在窗口外不得发 disconnect"
        );

        // 关闭窗口内不得建立新连接（closing 仍置位）。
        let blocked = hub.connect().await.expect_err("关闭窗口内不得建立新连接");
        assert!(blocked.is(codes::OFFLINE));

        // 放行 A 的第一次 disconnect：A 因 epoch 过期收尾，B 完成两次清理并清位。
        gate.notify_one();
        task_a.await.expect("A 清理任务不得 panic");
        task_b.await.expect("B 清理任务不得 panic");
        assert_eq!(
            fake.disconnect_calls.load(Ordering::SeqCst),
            3,
            "A 一次 + B 两次；陈旧 A 不得再发第二次（旧实现为 4）"
        );

        // 清理完全结束后允许新连接；旧实现里 A 的迟到第二次 disconnect 会杀掉它。
        let view = hub.connect().await.expect("新连接必须成功");
        assert_eq!(view.state, ConnectionState::Paired);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            fake.disconnect_calls.load(Ordering::SeqCst),
            3,
            "陈旧清理不得在重连后再发 disconnect"
        );
        assert_eq!(
            hub.connection().state,
            ConnectionState::Paired,
            "新连接不得被旧清理关掉"
        );

        // 陈旧凭据：在新连接之后 finish 是 no-op（is_current == false，零额外 disconnect）。
        let calls_before = fake.disconnect_calls.load(Ordering::SeqCst);
        let stale_guard = CloseGuard {
            inner: hub.inner.clone(),
            epoch: u64::MAX,
        };
        assert!(!stale_guard.is_current(), "陈旧凭据必须被识别为非当前窗口");
        hub.finish_disconnect(stale_guard).await;
        assert_eq!(
            fake.disconnect_calls.load(Ordering::SeqCst),
            calls_before,
            "陈旧凭据不得再 disconnect"
        );
        assert_eq!(hub.connection().state, ConnectionState::Paired);
        assert!(hub.lock().closing.is_none(), "重连后不得残留关闭窗口");
    }

    /// 真实可达的 stale owner（不是手工 epoch）：A `begin_disconnect` 后**不 poll**，
    /// B 完整清理并清位，重连成功后，再 finish A 的真实凭据必须是零副作用 no-op。
    #[tokio::test]
    async fn stale_real_guard_after_newer_close_and_reconnect_is_noop() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        assert_eq!(hub.connection().state, ConnectionState::Paired);

        let (_view_a, guard_a) = hub.begin_disconnect();
        let (_view_b, guard_b) = hub.begin_disconnect();
        hub.finish_disconnect(guard_b).await;
        assert!(hub.lock().closing.is_none(), "B 清理后关闭窗口必须清位");
        let calls_after_b = fake.disconnect_calls.load(Ordering::SeqCst);
        assert_eq!(calls_after_b, 2, "B 只有自己的两次 disconnect");

        let view = hub.connect().await.expect("B 完成后必须可以重连");
        assert_eq!(view.state, ConnectionState::Paired);

        // A 的真实凭据（从未被 poll）：重连后 finish 必须是 no-op。
        assert!(!guard_a.is_current(), "A 的关闭窗口已被 B 取代");
        hub.finish_disconnect(guard_a).await;
        assert_eq!(
            fake.disconnect_calls.load(Ordering::SeqCst),
            calls_after_b,
            "陈旧 A 不得再发 backend.disconnect"
        );
        assert_eq!(hub.connection().state, ConnectionState::Paired);
        assert!(hub.lock().closing.is_none());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            fake.disconnect_calls.load(Ordering::SeqCst),
            calls_after_b,
            "稳定窗后也不得出现迟到清理"
        );
        assert_eq!(hub.connection().state, ConnectionState::Paired);
    }

    /// 关闭窗口内旧 attempt 的 relay 状态不得回写：begin 之后事件泵即使收到更高
    /// relay 代际的 Paired/Connecting，Hub 也必须保持 begin 落定的 Offline。
    #[tokio::test]
    async fn relay_state_cannot_overwrite_closing_offline() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, pump) = pumped(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        let (_view, guard) = hub.begin_disconnect();

        // 旧 attempt 的迟到状态（relay 代际远高于地板 0）：绝不能把 Hub 拉回 paired。
        events
            .send(RelayEvent::State(relay_view(ConnectionState::Paired, 99)))
            .await
            .unwrap();
        events
            .send(RelayEvent::State(relay_view(
                ConnectionState::Connecting,
                100,
            )))
            .await
            .unwrap();
        // 同一 mpsc FIFO 上的哨兵帧：它进了桥即证明前面两条 State 已被泵处理。
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 1})))
            .await
            .unwrap();
        wait_until(|| pump.data.lock().unwrap().len() == 1).await;
        assert_eq!(
            hub.connection().state,
            ConnectionState::Offline,
            "关闭窗口内 relay 状态不得回写"
        );

        drop(guard);
    }

    /// 旧代际动作：拒绝且零后端执行；同代际动作才派发。
    #[tokio::test]
    async fn stale_generation_action_is_rejected_zero_execute() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        let generation = hub.connection().generation;

        let err = hub
            .execute(ctx("synthetic-action-2"), send_action(generation + 7))
            .await
            .unwrap_err();
        assert!(err.is(codes::STALE_GENERATION));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 0);

        let receipt = hub
            .execute(ctx("synthetic-action-3"), send_action(generation))
            .await
            .unwrap();
        assert_eq!(receipt.state, ActionState::Accepted);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
    }

    /// 未授权/断开（needs_auth）时重试同一 actionId：持久账本查询先于连接/许可闸，
    /// 直接回既有 accepted 回执——零 bind、零 execute，也不重复广播 Action 事件。
    #[tokio::test]
    async fn replay_returns_stored_receipt_before_needs_auth() {
        let fake = Arc::new(FakeBackend::default());
        let hub = ZcodeHub::with_parts(stored(URL), fake.clone(), send_policy());
        assert_eq!(
            hub.connection().state,
            ConnectionState::NeedsAuth,
            "前置：已保存链接但未授权"
        );
        let generation = hub.connection().generation;
        let stored_receipt = ActionReceipt {
            action_id: "synthetic-replay-accepted".into(),
            session_id: "sess_synthetic0000".into(),
            generation,
            state: ActionState::Accepted,
            reason_code: None,
        };
        fake.set_existing(Ok(Some(stored_receipt.clone())));
        let mut events = hub.subscribe();

        let got = hub
            .execute(ctx("synthetic-replay-accepted"), send_action(generation))
            .await
            .expect("既有回执重放不得被 needs_auth 拦下");
        assert_eq!(got, stored_receipt, "必须原样返回既定终态回执");
        assert_eq!(
            fake.existing_calls.load(Ordering::SeqCst),
            1,
            "必须先查持久账本"
        );
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 0, "重放零绑定");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0, "重放零派发");
        // 重放不得广播新的 Action 事件（否则手机会看到重复回执）。
        while let Ok(event) = events.try_recv() {
            assert_ne!(
                event.kind,
                ControlEventKind::Action,
                "重放不得 emit 新 Action 事件"
            );
        }
    }

    /// 账本判定载荷冲突：错误必须在连接闸之前上抛（同 id 换载荷不是「未授权」），
    /// 且零 bind/零 execute。
    #[tokio::test]
    async fn replay_conflict_is_returned_before_needs_auth() {
        let fake = Arc::new(FakeBackend::default());
        let hub = ZcodeHub::with_parts(stored(URL), fake.clone(), send_policy());
        let generation = hub.connection().generation;
        fake.set_existing(Err(ZcodeError::of(codes::ACTION_PAYLOAD_CONFLICT)));

        let err = hub
            .execute(ctx("synthetic-replay-conflict"), send_action(generation))
            .await
            .unwrap_err();
        assert!(
            err.is(codes::ACTION_PAYLOAD_CONFLICT),
            "必须先回 action_payload_conflict 而不是 needs_auth: {err:?}"
        );
        assert_eq!(fake.existing_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
    }

    /// 代际判定仍先于持久查询：旧代际动作即使账本有回执也不得取回（零查询）。
    #[tokio::test]
    async fn stale_generation_is_rejected_before_existing_receipt() {
        let fake = Arc::new(FakeBackend::default());
        let hub = ZcodeHub::with_parts(stored(URL), fake.clone(), send_policy());
        let generation = hub.connection().generation;
        fake.set_existing(Ok(Some(ActionReceipt {
            action_id: "synthetic-replay-stale".into(),
            session_id: "sess_synthetic0000".into(),
            generation,
            state: ActionState::Accepted,
            reason_code: None,
        })));

        let err = hub
            .execute(ctx("synthetic-replay-stale"), send_action(generation + 1))
            .await
            .unwrap_err();
        assert!(err.is(codes::STALE_GENERATION));
        assert_eq!(
            fake.existing_calls.load(Ordering::SeqCst),
            0,
            "旧代际不得查询/返回账本回执"
        );
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
    }

    /// 公开代际唯一：SessionView/ActionReceipt 只暴露 Hub 代际（投影代际不外泄）。
    #[tokio::test]
    async fn public_generation_is_hub_generation_after_connect() {
        let fake = Arc::new(FakeBackend::default());
        // 生产 connect 会让 bridge 推进自己的投影代际；这里刻意造出分叉。
        fake.bind_generation.store(41, Ordering::SeqCst);
        let hub = paired_hub(fake.clone()).await;
        let generation = hub.connection().generation;

        let view = hub
            .session(&TargetKey {
                session_id: "sess_synthetic0000".into(),
            })
            .await
            .unwrap();
        assert_eq!(view.generation, generation, "会话视图只暴露 Hub 代际");

        let receipt = hub
            .execute(ctx("synthetic-action-public"), send_action(generation))
            .await
            .unwrap();
        assert_eq!(receipt.generation, generation, "动作回执只暴露 Hub 代际");
        // 交到后端的精确目标：公开代际 = Hub 代际，桥投影代际 41 收进内部字段。
        let executed = fake.executed_targets.lock().unwrap().last().cloned();
        let executed = executed.expect("fake 必须收到一次 execute 目标");
        assert_eq!(executed.generation, generation, "目标公开代际 = Hub 代际");
        assert_eq!(
            executed.bridge_generation,
            Some(41),
            "桥投影代际必须原样收进 bridge_generation"
        );
    }

    /// 握手成功与 relay 掉线交错：事件泵先落定的离线态必须胜出，绝不回写假 paired。
    #[tokio::test]
    async fn late_connect_success_cannot_overwrite_offline() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, _pump) = pumped(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        let gate = Arc::new(Notify::new());
        fake.gate.lock().unwrap().replace(gate.clone());
        let late = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.connect().await })
        };
        wait_until(|| fake.connect_calls.load(Ordering::SeqCst) == 1).await;

        // relay 在同一 relay 代际先报 paired 再掉线：泵把 Hub 落定 offline。
        events
            .send(RelayEvent::State(relay_view(ConnectionState::Paired, 1)))
            .await
            .unwrap();
        events
            .send(RelayEvent::State(relay_view(ConnectionState::Offline, 1)))
            .await
            .unwrap();
        wait_until(|| hub.connection().state == ConnectionState::Offline).await;

        gate.notify_one();
        let result = late.await.expect("spawn 不应 panic");
        assert!(
            matches!(&result, Err(error) if error.is(codes::OFFLINE)),
            "迟到的握手成功必须被更新的离线态收口，实际 {result:?}"
        );
        assert_eq!(hub.connection().state, ConnectionState::Offline);
        assert!(!hub.connection().is_paired(), "绝不粘在假 paired");

        // 不粘死：下一次显式 connect 仍会真的再开一次后端连接。
        fake.gate.lock().unwrap().take();
        hub.connect().await.expect("显式重连仍应可达");
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 2);
    }

    /// 真并发双 connect：只有一个真正开始后端连接，另一个排队后复用同一配对。
    #[tokio::test]
    async fn concurrent_connect_opens_one_backend_connect() {
        let fake = Arc::new(FakeBackend::default());
        let hub = Arc::new(ZcodeHub::with_parts(
            stored(URL),
            fake.clone(),
            send_policy(),
        ));
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        let gate = Arc::new(Notify::new());
        fake.gate.lock().unwrap().replace(gate.clone());

        let first = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.connect().await })
        };
        wait_until(|| fake.connect_calls.load(Ordering::SeqCst) == 1).await;
        // 第二个调用在 connect_lock 上排队（不会进入后端 connect）。
        let second = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.connect().await })
        };
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            1,
            "在途期间双 connect 只允许一次后端连接"
        );

        gate.notify_one();
        assert!(first.await.unwrap().unwrap().is_paired());
        assert!(second.await.unwrap().unwrap().is_paired());
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            1,
            "后到的调用必须复用同一物理连接"
        );
    }

    /// 配置变更后：旧代际动作零执行；重连后的会话/回执与连接视图同一代际。
    #[tokio::test]
    async fn old_generation_action_rejected_after_config_change() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        let old_generation = hub.connection().generation;

        let view = hub.config_changed().await;
        let new_generation = view.generation;
        assert!(new_generation > old_generation);
        assert_eq!(view.state, ConnectionState::Offline, "配置变更保留账号 JWT");

        let err = hub
            .execute(ctx("synthetic-action-old"), send_action(old_generation))
            .await
            .unwrap_err();
        assert!(err.is(codes::STALE_GENERATION));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 0);

        // 重新授权/连接：动作、会话、回执必须共用 Hub 新代际（不是投影代际）。
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        let session = hub
            .session(&TargetKey {
                session_id: "sess_synthetic0000".into(),
            })
            .await
            .unwrap();
        assert_eq!(session.generation, new_generation);
        let receipt = hub
            .execute(ctx("synthetic-action-new"), send_action(new_generation))
            .await
            .unwrap();
        assert_eq!(receipt.generation, new_generation);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
    }

    /// 成功绑定会话后广播 Session 事件（本会话、无设备维度、无凭据）。
    #[tokio::test]
    async fn session_emits_session_event() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake).await;
        let generation = hub.connection().generation;
        let mut events = hub.subscribe();

        hub.session(&TargetKey {
            session_id: "sess_synthetic0000".into(),
        })
        .await
        .unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .expect("必须广播 session 事件")
            .expect("广播通道未关闭");
        assert_eq!(event.kind, ControlEventKind::Session);
        assert_eq!(event.session_id.as_deref(), Some("sess_synthetic0000"));
        assert_eq!(event.generation, generation);
        assert_eq!(event.device_id, None, "会话事件无设备维度");
        assert_eq!(event.data["sessionId"], "sess_synthetic0000");
        assert_eq!(event.data["ready"], true);
        let wire = serde_json::to_string(&event).unwrap();
        assert!(!wire.contains("jwt") && !wire.contains("token"));
    }

    /// 成功动作后广播 Action 事件：带发起设备（仅进程内，序列化不可见）。
    #[tokio::test]
    async fn execute_emits_action_event_with_device_id() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake).await;
        let generation = hub.connection().generation;
        let mut events = hub.subscribe();

        hub.execute(ctx("synthetic-action-event"), send_action(generation))
            .await
            .unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .expect("必须广播 action 事件")
            .expect("广播通道未关闭");
        assert_eq!(event.kind, ControlEventKind::Action);
        assert_eq!(event.session_id.as_deref(), Some("sess_synthetic0000"));
        assert_eq!(event.generation, generation);
        assert_eq!(event.device_id.as_deref(), Some("synthetic-device"));
        assert_eq!(event.data["actionId"], "synthetic-action-event");
        assert_eq!(event.data["state"], "accepted");
        let wire = serde_json::to_string(&event).unwrap();
        assert!(!wire.contains("deviceId"), "设备 id 绝不序列化: {wire}");
        assert!(
            !wire.contains("synthetic-device"),
            "设备 id 绝不序列化: {wire}"
        );
    }

    /// 配置变更窗口：新代际绝不与旧投影/旧标签同框（reset 在途也如此）。
    #[tokio::test]
    async fn config_change_never_exposes_new_generation_with_old_projection() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        let key = TargetKey {
            session_id: "sess_synthetic0000".into(),
        };
        // 先读一次投影（旧链接下可读），让 Hub 持有可降级的会话视图。
        hub.session(&key).await.unwrap();
        let old = hub.connection();
        assert!(old.is_paired());

        let gate = Arc::new(Notify::new());
        fake.reset_gate.lock().unwrap().replace(gate.clone());
        let mut events = hub.subscribe();
        let changing = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.config_changed().await })
        };
        wait_until(|| fake.reset_calls.load(Ordering::SeqCst) == 1).await;

        // reset 尚在途：连接面必须是「新代际 + 未连接（账号仍保留）」，绝不复用旧配对/旧标签。
        let during = hub.connection();
        assert!(during.generation > old.generation);
        assert_eq!(during.state, ConnectionState::Offline);
        assert_eq!(during.device_label, None);
        // 旧投影不得以新代际出现：控制门直接拒绝（未连接），零新绑定。
        let err = hub.session(&key).await.unwrap_err();
        assert!(err.is(codes::OFFLINE));
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 1);

        // 已广播帧同样不得出现「新代际 + paired / ready=true」。
        while let Ok(event) = events.try_recv() {
            if event.generation != during.generation {
                continue;
            }
            match event.kind {
                ControlEventKind::Connection => {
                    assert_ne!(event.data["state"], serde_json::json!("paired"))
                }
                ControlEventKind::Session => {
                    assert_eq!(event.data["ready"], false, "旧投影只能降级广播")
                }
                ControlEventKind::Action => {}
            }
        }

        gate.notify_one();
        let done = changing.await.unwrap();
        assert_eq!(done.state, ConnectionState::Offline);
        assert_eq!(done.generation, during.generation);
    }

    /// P2-3：`config_changed` 的 `reset`/`auth_status` 清理期间必须处于关闭窗口——
    /// 显式连接与自动连接都不得提前连上新目标；清理结束后清位、可重连。
    #[tokio::test]
    async fn config_change_cleanup_window_blocks_connect_and_auto() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        let store = stored(URL);
        set_remote_enabled(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store.clone(),
            fake.clone(),
            send_policy(),
        ));
        hub.start_auto_connect();
        wait_for_connection(&hub, ConnectionState::Paired).await;
        let paired = hub.connection();
        let connects_before = fake.connect_calls.load(Ordering::SeqCst);

        let gate = Arc::new(Notify::new());
        fake.reset_gate.lock().unwrap().replace(gate.clone());
        let changing = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.config_changed().await })
        };
        wait_until(|| fake.reset_calls.load(Ordering::SeqCst) == 1).await;

        // reset 在途：关闭窗口内 connect 必须立即 offline 且零后端连接。
        assert_eq!(hub.connection().state, ConnectionState::Offline);
        let err = hub.connect().await.unwrap_err();
        assert!(
            err.is(codes::OFFLINE),
            "清理窗口内必须 offline，实际 {err:?}"
        );
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            connects_before,
            "清理窗口内不得提前连新目标"
        );

        // auto 唤醒同样被关闭窗口拒绝（worker 确实跑过一次，但不连接）。
        let attempts = hub.auto_attempt_count();
        hub.wake_auto_connect();
        wait_for_auto_attempt(&hub, attempts + 1).await;
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            connects_before,
            "清理窗口内自动连接不得连新目标"
        );
        assert_eq!(hub.connection().state, ConnectionState::Offline);

        // 放行 reset：清位后必须可重连（旧清理不得把新代际留在 Offline）。
        gate.notify_one();
        let changed = changing.await.expect("spawn 不应 panic");
        assert_eq!(changed.generation, paired.generation + 1);
        assert_eq!(changed.state, ConnectionState::Offline);
        assert!(hub.lock().closing.is_none(), "reset 结束后关闭窗口必须清位");
        wait_for_connection(&hub, ConnectionState::Paired).await;
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            connects_before + 1,
            "清理结束后只允许一次新连接"
        );
    }

    /// P2-3：重叠的配置变更——旧窗口的清理结束时不得清掉新窗口的 `closing`，
    /// 也不得把新代际的展示状态覆盖回去。
    #[tokio::test]
    async fn stale_config_change_guard_cannot_clear_newer_closing() {
        let fake = Arc::new(FakeBackend::default());
        // 已配对：配置变更必须断旧连接；JWT 保留（fake 已授权）。
        let hub = paired_hub(fake.clone()).await;
        let paired = hub.connection();
        let connects_before = fake.connect_calls.load(Ordering::SeqCst);

        let gate = Arc::new(Notify::new());
        fake.reset_gate.lock().unwrap().replace(gate.clone());
        let first = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.config_changed().await })
        };
        wait_until(|| fake.reset_calls.load(Ordering::SeqCst) == 1).await;
        let after_first = hub.connection();
        assert!(after_first.generation > paired.generation);

        // 第二次配置变更接管：begin 立即推进代际/写入更高的关闭窗口，但它的 reset
        // 必须**等 A 的清理窗口结束**（同一把 disconnect_lock 串行）。
        let second = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.config_changed().await })
        };
        wait_until(|| {
            let view = hub.connection();
            view.generation > after_first.generation && view.state == ConnectionState::Offline
        })
        .await;
        let newer = hub.connection();
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            fake.reset_calls.load(Ordering::SeqCst),
            1,
            "B 的 reset 必须等 A 的清理窗口结束（清理串行）"
        );

        // 新窗口生效：connect 被拒绝且零后端连接。
        let err = hub.connect().await.unwrap_err();
        assert!(err.is(codes::OFFLINE), "新关闭窗口必须生效，实际 {err:?}");
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), connects_before);

        // 放行较早的清理：它（曾是 owner）的末态因代际已变被跳过，且不得清 B 的窗口。
        gate.notify_one();
        let first_view = first.await.expect("spawn 不应 panic");
        assert_eq!(
            first_view.generation, newer.generation,
            "旧清理不得覆盖新代际状态"
        );
        assert_eq!(
            hub.lock().closing,
            Some(newer.generation),
            "旧 guard 不得清掉新窗口"
        );

        // A 释放清理锁后 B 才进入 reset（同一闸），此时仍拒绝连接。
        wait_until(|| fake.reset_calls.load(Ordering::SeqCst) == 2).await;
        assert!(hub.connect().await.unwrap_err().is(codes::OFFLINE));

        // 放行较新的清理：它清位后显式重连可达（B 的连接胜出）。
        gate.notify_one();
        let second_view = second.await.expect("spawn 不应 panic");
        assert_eq!(second_view.generation, newer.generation);
        assert!(hub.lock().closing.is_none(), "新窗口结束后必须清位");
        let reconnected = hub.connect().await.expect("清理结束后必须可重连");
        assert!(reconnected.is_paired());
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            connects_before + 1
        );
    }

    /// P2-3 残留（终审）：`config_changed` 的 `reset` 必须与断开清理串行且校验 owner。
    /// 「B 先完整完成并重连、A 后执行」时，陈旧 A 绝不能再 reset 断掉 B 的连接/投影。
    #[tokio::test]
    async fn stale_config_change_cannot_reset_newer_connection() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        assert_eq!(hub.connection().state, ConnectionState::Paired);
        assert!(fake.is_live(), "前置：已配对 = relay live");
        let resets_before = fake.reset_calls.load(Ordering::SeqCst);
        assert_eq!(resets_before, 0);

        // reset 的断链副作用：只要陈旧 A 还执行 reset，就会把 B 的 live 断掉（RED）。
        fake.reset_drops_live.store(true, Ordering::SeqCst);

        // A：begin 之后、取清理串行锁之前停住。
        let gate = hub.install_config_pre_reset_gate();
        let a = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.config_changed().await })
        };
        wait_until(|| hub.config_waiting_before_reset()).await;
        assert_eq!(
            fake.reset_calls.load(Ordering::SeqCst),
            resets_before,
            "A 停住时不得已执行 reset"
        );

        // B：完整执行（reset/auth_status/清位）并显式重连——B 是当前 owner。
        let b = hub.config_changed().await;
        assert_eq!(b.state, ConnectionState::Offline);
        assert_eq!(
            fake.reset_calls.load(Ordering::SeqCst),
            resets_before + 1,
            "B（owner）必须执行一次 reset"
        );
        assert!(!fake.is_live(), "B 的 reset 必须断掉旧链");
        assert!(hub.lock().closing.is_none(), "B 结束后关闭窗口清位");
        let reconnected = hub.connect().await.expect("B 之后必须可重连");
        assert!(reconnected.is_paired());
        assert!(fake.is_live(), "B 的新连接必须 live");
        let connects_after_b = fake.connect_calls.load(Ordering::SeqCst);

        // 放行 A：陈旧 owner 不得再 reset / 断 B 的连接，也不得覆盖 B 的末态。
        gate.notify_one();
        let a_view = a.await.expect("spawn 不应 panic");
        assert_eq!(
            a_view.generation, b.generation,
            "陈旧 A 不得覆盖 B 的代际/末态"
        );
        assert_eq!(
            fake.reset_calls.load(Ordering::SeqCst),
            resets_before + 1,
            "陈旧 A 不得再执行 reset"
        );
        assert!(fake.is_live(), "陈旧 reset 不得断掉 B 的新连接");
        assert_eq!(hub.connection().state, ConnectionState::Paired);
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            connects_after_b,
            "陈旧 A 不得追加连接"
        );
        // 投影仍有效：B 的连接可正常读取会话。
        let session = hub
            .session(&TargetKey {
                session_id: "sess_synthetic0000".into(),
            })
            .await
            .expect("B 的新连接投影必须仍有效");
        assert!(session.ready);
    }

    /// KICKED（conflict）是终态：不自动重连，只有显式断开/配置变更才复位。
    #[tokio::test]
    async fn kicked_backend_state_is_conflict_terminal() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_connect(Err(ZcodeError::of(codes::CONFLICT)));
        let hub = ZcodeHub::with_parts(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();

        assert!(hub.connect().await.unwrap_err().is(codes::CONFLICT));
        assert_eq!(hub.connection().state, ConnectionState::Conflict);
        assert!(hub.connect().await.unwrap_err().is(codes::CONFLICT));
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 1, "绝不自抢重连");

        let view = hub.disconnect().await;
        assert_eq!(view.state, ConnectionState::Offline);
    }

    /// T8 夹具：conflict 视图（reasonCode = 冻结码 conflict，无设备标签）。
    fn conflict_view() -> ConnectionView {
        ConnectionView {
            state: ConnectionState::Conflict,
            generation: 1,
            reason_code: Some(codes::CONFLICT.into()),
            device_label: None,
        }
    }

    /// 手机「断开并抢回」（spec §6.5）：conflict → 显式断开 → 显式连接各**一次**，
    /// 成功落 paired。绝不循环：抢回后再收到 conflict 也原样返回，不做第二次尝试。
    #[tokio::test]
    async fn takeover_conflict_disconnects_then_connects_once() {
        let fake = Arc::new(FakeBackend::default());
        fake.jwt.store(true, Ordering::SeqCst);
        fake.auth_state.lock().unwrap().authorized = true;
        let hub = ZcodeHub::with_parts(stored(URL), fake.clone(), send_policy());
        hub.mark_authorized_for_test();
        fake.set_connect(Ok(conflict_view()));
        let first = hub.connect().await.unwrap();
        assert_eq!(first.state, ConnectionState::Conflict);

        fake.set_connect(Ok(FakeBackend::paired_view()));
        let view = hub.takeover_requested().await.unwrap();
        assert!(view.is_paired());
        // 一次断开（quiesce 两段 backend.disconnect）+ 一次重连（初连 + 抢回）。
        assert_eq!(fake.disconnect_calls.load(Ordering::SeqCst), 2);
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 2);
    }

    /// 未授权的抢回：`needs_auth` 且**零 OAuth**——不调 authorize、不开浏览器。
    #[tokio::test]
    async fn takeover_requires_auth_without_oauth() {
        let fake = Arc::new(FakeBackend::default());
        let hub = ZcodeHub::with_parts(stored(URL), fake.clone(), send_policy());
        let err = hub.takeover_requested().await.unwrap_err();
        assert!(err.is(codes::NEEDS_AUTH));
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            0,
            "未授权不得发起任何连接"
        );
    }

    /// 已配对/握手中时抢回是幂等的：直接返回当前视图，零断开零重连。
    #[tokio::test]
    async fn takeover_paired_is_idempotent() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        let view = hub.takeover_requested().await.unwrap();
        assert!(view.is_paired());
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 1, "不重连");
        assert_eq!(
            fake.disconnect_calls.load(Ordering::SeqCst),
            0,
            "已配对不得断开"
        );
    }

    /// 未开闸能力：needs_evidence 且零后端执行（手机不可自报能力）。
    #[tokio::test]
    async fn closed_policy_blocks_execute_zero_backend() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        let generation = hub.connection().generation;
        // paired_hub 用 send_policy（仅 send 开闸）；问答默认关闭。
        let answer = ControlAction::Answer {
            session_id: "sess_synthetic0000".into(),
            generation,
            interaction_id: "synthetic-interaction".into(),
            answers: vec![AnswerEntry {
                question_text: "q".into(),
                selected_values: vec!["a".into()],
                free_text: None,
            }],
        };
        let err = hub
            .execute(ctx("synthetic-action-4"), answer)
            .await
            .unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            fake.bind_calls.load(Ordering::SeqCst),
            0,
            "四项问答/审批能力全关时必须 bind 前否决（零 I/O）"
        );
        assert_eq!(fake.required_calls.load(Ordering::SeqCst), 0);
    }

    /// Answer 全关时在 bind 前否决：不绑定、不读宿主卡、不执行（默认策略）。
    #[tokio::test]
    async fn answer_all_capabilities_closed_rejected_before_bind() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_required(Ok(vec!["multi_select"]));
        let hub = hub_with_policy(
            fake.clone(),
            Arc::new(Mutex::new(CapabilityView::default())),
        );
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let generation = hub.connection().generation;

        // 客户端只传一个值；宿主卡是多选题。默认策略四项全关。
        let err = hub
            .execute(
                ctx("closed-answer-1"),
                answer_action(generation, "pi-closed-answer", vec!["one"], None),
            )
            .await
            .unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE));
        assert_eq!(
            fake.bind_calls.load(Ordering::SeqCst),
            0,
            "全关时必须 bind 前拒绝（旧实现会先 bind 再拒）"
        );
        assert_eq!(fake.required_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
    }

    /// Snooze 不是免闸辅助操作：默认策略（四项问答/审批全关）下必须在 bind 前
    /// 拒绝，零绑定、零执行；与手机端同一能力矩阵的禁用一致。
    #[tokio::test]
    async fn snooze_all_capabilities_closed_rejected_before_bind() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_required(Ok(vec!["single_select"]));
        let hub = hub_with_policy(
            fake.clone(),
            Arc::new(Mutex::new(CapabilityView::default())),
        );
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let generation = hub.connection().generation;

        let err = hub
            .execute(
                ctx("closed-snooze-1"),
                snooze_action(generation, "pi-closed-snooze"),
            )
            .await
            .unwrap_err();
        assert!(
            err.is(codes::NEEDS_EVIDENCE),
            "Snooze 不得绕开能力闸：全关时服务端必须拒绝"
        );
        assert_eq!(
            fake.bind_calls.load(Ordering::SeqCst),
            0,
            "Snooze 全关时也必须 bind 前拒绝（零 I/O）"
        );
        assert_eq!(fake.required_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
    }

    /// Snooze 只绑本次 required 的答案许可：`send` 许可保持未绑，仍可被普通
    /// `consume` 使用；绑定的 `single_select` 留给该题最终 Answer 消费。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn snooze_binds_only_required_capability_not_other_permits() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, generation) = closed_evidence_hub(fake.clone(), vec!["single_select"]).await;
        hub.arm_evidence(EVIDENCE_SESSION, generation, "single_select")
            .unwrap();
        hub.arm_evidence(EVIDENCE_SESSION, generation, "send")
            .unwrap();

        hub.execute(
            ctx("snooze-limit-1"),
            snooze_action(generation, "ix-limit-1"),
        )
        .await
        .expect("single_select 许可应允许 snooze");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);

        // 旧的全量绑定会把 send 一并绑到该题 → 普通 consume 失败。
        let sent = hub
            .execute(ctx("snooze-limit-send"), send_action(generation))
            .await
            .expect("send 许可不得被 snooze 绑定");
        assert_eq!(sent.state, ActionState::Accepted);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 2);

        // snooze 不消费：同题最终 Answer 仍能用该许可。
        let answer = hub
            .execute(
                ctx("snooze-limit-answer"),
                answer_action(generation, "ix-limit-1", vec!["one"], None),
            )
            .await
            .expect("snooze 绑定的题必须能最终消费许可");
        assert_eq!(answer.state, ActionState::Accepted);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 3);
    }

    /// 只有 `send` 许可、宿主 required 为 `single_select`：Snooze 拒绝，零 bind、
    /// 零执行，且 `send` 许可完全未被碰（仍可正常发送）。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn snooze_ignores_send_permit_and_makes_no_binding() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, generation) = closed_evidence_hub(fake.clone(), vec!["single_select"]).await;
        hub.arm_evidence(EVIDENCE_SESSION, generation, "send")
            .unwrap();

        let err = hub
            .execute(
                ctx("snooze-send-only-1"),
                snooze_action(generation, "ix-send-only"),
            )
            .await
            .unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE), "send 许可不得放宽问答能力");
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);

        let sent = hub
            .execute(ctx("snooze-send-only-send"), send_action(generation))
            .await
            .expect("send 许可必须原样可用");
        assert_eq!(sent.state, ActionState::Accepted);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
    }

    /// 许可已绑 ix-1：ix-2 的 snooze 拒绝且不改绑；ix-1 仍可最终作答。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn snooze_other_interaction_rejected_without_stealing_binding() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, generation) = closed_evidence_hub(fake.clone(), vec!["single_select"]).await;
        hub.arm_evidence(EVIDENCE_SESSION, generation, "single_select")
            .unwrap();

        hub.execute(ctx("snooze-bind-1"), snooze_action(generation, "ix-1"))
            .await
            .expect("首次 snooze 应绑定 ix-1");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 1);

        let err = hub
            .execute(ctx("snooze-steal-2"), snooze_action(generation, "ix-2"))
            .await
            .unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE), "其他题不得借用/抢走已绑许可");
        assert_eq!(
            fake.bind_calls.load(Ordering::SeqCst),
            1,
            "能力闸拒绝必须发生在 bind 之前"
        );
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);

        // ix-1 的绑定未被改写：最终 answer 仍能消费。
        let answer = hub
            .execute(
                ctx("snooze-bind-answer"),
                answer_action(generation, "ix-1", vec!["one"], None),
            )
            .await
            .expect("ix-1 的绑定必须原样保留");
        assert_eq!(answer.state, ActionState::Accepted);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 2);
    }

    /// 两项 required 只 arm 一项：Snooze 整单拒绝且**不得部分绑定**；补齐后
    /// 换一题仍可成功（证明第一次失败没有把 single_select 绑到 ix-1）。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn snooze_multi_required_missing_permit_leaves_no_partial_binding() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, generation) =
            closed_evidence_hub(fake.clone(), vec!["single_select", "free_text"]).await;
        hub.arm_evidence(EVIDENCE_SESSION, generation, "single_select")
            .unwrap();

        let err = hub
            .execute(ctx("snooze-partial-1"), snooze_action(generation, "ix-1"))
            .await
            .unwrap_err();
        assert!(
            err.is(codes::NEEDS_EVIDENCE),
            "缺 free_text 许可必须整单拒绝"
        );
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);

        hub.arm_evidence(EVIDENCE_SESSION, generation, "free_text")
            .unwrap();
        hub.execute(ctx("snooze-partial-2"), snooze_action(generation, "ix-2"))
            .await
            .expect("失败调用不得留下部分绑定；补齐后换题应成功");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
    }

    /// 权限卡 snooze：只绑 `approval_reply` 且不消费，最终 Permission 仍可用。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn snooze_permission_binds_approval_reply_without_consuming() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, generation) = closed_evidence_hub(fake.clone(), vec!["approval_reply"]).await;
        hub.arm_evidence(EVIDENCE_SESSION, generation, "approval_reply")
            .unwrap();

        hub.execute(ctx("snooze-perm-1"), snooze_action(generation, "ix-perm"))
            .await
            .expect("approval_reply 许可应允许 snooze 权限卡");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);

        let permission = ControlAction::Permission {
            session_id: "sess_synthetic0000".into(),
            generation,
            interaction_id: "ix-perm".into(),
            option_id: "allowOnce".into(),
        };
        let receipt = hub
            .execute(ctx("snooze-perm-resolve"), permission)
            .await
            .expect("snooze 不消费许可，最终审批仍应可用");
        assert_eq!(receipt.state, ActionState::Accepted);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 2);
    }

    /// 问答所需能力来自**宿主卡题型**：多选卡即便客户端只传 1 个值，也必须
    /// `multi_select` 开闸/许可；客户端值数量绝不参与推断。
    #[tokio::test]
    async fn answer_capability_comes_from_host_card_not_client_count() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_required(Ok(vec!["multi_select"]));
        let policy = Arc::new(Mutex::new(policy_with(&["single_select"])));
        let hub = hub_with_policy(fake.clone(), policy.clone());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let generation = hub.connection().generation;

        // 宿主卡是多选题；客户端只勾了一个值。单选取向的开闸不得放行。
        let err = hub
            .execute(
                ctx("host-multi-1"),
                answer_action(generation, "pi-host-multi", vec!["only-one"], None),
            )
            .await
            .unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            fake.required_calls.load(Ordering::SeqCst),
            1,
            "必须在绑定后读取宿主卡题型，而不是看客户端值数量"
        );
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 1);

        // 打开宿主卡真正要求的 multi_select（single_select 保持开着）→ 放行。
        open_capability(&mut policy.lock().unwrap(), "multi_select");
        let receipt = hub
            .execute(
                ctx("host-multi-2"),
                answer_action(generation, "pi-host-multi", vec!["only-one"], None),
            )
            .await
            .expect("宿主多选卡在 multi_select 开闸后必须放行");
        assert_eq!(receipt.state, ActionState::Accepted);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fake.required_calls.load(Ordering::SeqCst), 2);
    }

    /// 混合题（单选 + 自由文本）逐项检查**全部**宿主所需能力，缺任一即拒。
    #[tokio::test]
    async fn mixed_question_requires_all_host_capabilities() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_required(Ok(vec!["single_select", "free_text"]));
        let policy = Arc::new(Mutex::new(policy_with(&["single_select"])));
        let hub = hub_with_policy(fake.clone(), policy.clone());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let generation = hub.connection().generation;

        // 只开了单选：混合题缺 free_text，零执行。
        let err = hub
            .execute(
                ctx("mixed-1"),
                answer_action(generation, "pi-host-mixed", vec!["one"], None),
            )
            .await
            .unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.required_calls.load(Ordering::SeqCst), 1);

        // 两项都开 → 放行。
        open_capability(&mut policy.lock().unwrap(), "free_text");
        let receipt = hub
            .execute(
                ctx("mixed-2"),
                answer_action(generation, "pi-host-mixed", vec!["one"], None),
            )
            .await
            .expect("全部所需能力开闸后必须放行");
        assert_eq!(receipt.state, ActionState::Accepted);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
    }

    /// Snooze 同样按宿主卡题型过能力闸：多选卡缺 `multi_select` 时零执行。
    #[tokio::test]
    async fn snooze_capability_comes_from_host_card() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_required(Ok(vec!["multi_select"]));
        let policy = Arc::new(Mutex::new(policy_with(&["single_select"])));
        let hub = hub_with_policy(fake.clone(), policy.clone());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let generation = hub.connection().generation;

        let err = hub
            .execute(
                ctx("snooze-host-1"),
                snooze_action(generation, "pi-host-snooze"),
            )
            .await
            .unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.required_calls.load(Ordering::SeqCst), 1);

        open_capability(&mut policy.lock().unwrap(), "multi_select");
        hub.execute(
            ctx("snooze-host-2"),
            snooze_action(generation, "pi-host-snooze"),
        )
        .await
        .expect("host multi_select 开闸后 snooze 应放行");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
    }

    /// 证据构建：多选卡的所需能力是 `multi_select`；`single_select` 许可绝不能
    /// 放行多选卡（许可只放宽它自己那项能力），多选许可才放行。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn host_multi_select_requires_multi_select_permit() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_required(Ok(vec!["multi_select"]));
        let policy = Arc::new(Mutex::new(CapabilityView::default()));
        let hub = hub_with_policy(fake.clone(), policy);
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let generation = hub.connection().generation;
        let key = TargetKey {
            session_id: EVIDENCE_SESSION.into(),
        };

        hub.arm_evidence(EVIDENCE_SESSION, generation, "single_select")
            .unwrap();
        let err = hub
            .execute(
                ctx("permit-multi-1"),
                answer_action(generation, "pi-permit-multi", vec!["one"], None),
            )
            .await
            .unwrap_err();
        assert!(
            err.is(codes::NEEDS_EVIDENCE),
            "single_select 许可不得放行多选卡"
        );
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);

        // 失败的能力闸不得消费任何许可：单选许可仍可用于它自己的能力。
        let armed = hub.session(&key).await.expect("会话投影应可读");
        assert!(armed.capabilities.single_select.enabled);
        assert!(!armed.capabilities.multi_select.enabled);

        // 发放真正的多选许可：放行，且只消费 multi_select。
        hub.arm_evidence(EVIDENCE_SESSION, generation, "multi_select")
            .unwrap();
        hub.execute(
            ctx("permit-multi-2"),
            answer_action(generation, "pi-permit-multi", vec!["one"], None),
        )
        .await
        .expect("multi_select 许可应放行多选卡");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
        let after = hub.session(&key).await.expect("会话投影应可读");
        assert!(
            after.capabilities.single_select.enabled,
            "未参与本次检查的许可不得被消费"
        );
        assert!(
            !after.capabilities.multi_select.enabled,
            "多选许可恰消费一次"
        );
    }

    /// 证据构建：混合题缺任一所需许可时，**任何**许可都不得被提前消费
    /// （all-or-nothing）；补齐后才一次性消费并放行。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn mixed_question_permit_check_is_all_or_nothing() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_required(Ok(vec!["single_select", "free_text"]));
        let policy = Arc::new(Mutex::new(CapabilityView::default()));
        let hub = hub_with_policy(fake.clone(), policy);
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let generation = hub.connection().generation;
        let key = TargetKey {
            session_id: EVIDENCE_SESSION.into(),
        };

        hub.arm_evidence(EVIDENCE_SESSION, generation, "single_select")
            .unwrap();
        let err = hub
            .execute(
                ctx("permit-mixed-1"),
                answer_action(generation, "pi-permit-mixed", vec!["one"], None),
            )
            .await
            .unwrap_err();
        assert!(
            err.is(codes::NEEDS_EVIDENCE),
            "缺 free_text 许可必须整单拒绝"
        );
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);

        let armed = hub.session(&key).await.expect("会话投影应可读");
        assert!(
            armed.capabilities.single_select.enabled,
            "全量检查失败时 single_select 许可必须原样保留（不得部分消费）"
        );

        hub.arm_evidence(EVIDENCE_SESSION, generation, "free_text")
            .unwrap();
        hub.execute(
            ctx("permit-mixed-2"),
            answer_action(generation, "pi-permit-mixed", vec!["one"], None),
        )
        .await
        .expect("两项许可齐全后必须放行");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
        let after = hub.session(&key).await.expect("会话投影应可读");
        assert!(!after.capabilities.single_select.enabled);
        assert!(!after.capabilities.free_text.enabled);
    }

    /// 会话能力矩阵的逐项断言：未取得产品级实机证据时八项必须全关且带冻结原因码。
    fn assert_capabilities_closed(view: &CapabilityView) {
        for (name, flag) in [
            ("send", &view.send),
            ("single_select", &view.single_select),
            ("multi_select", &view.multi_select),
            ("free_text", &view.free_text),
            ("interrupt", &view.interrupt),
            ("approval_reply", &view.approval_reply),
            ("model", &view.model),
            ("thought_level", &view.thought_level),
        ] {
            assert!(!flag.enabled, "{name} 未经 live 验收不得开闸");
            assert_eq!(
                flag.reason_code.as_deref(),
                Some(codes::NEEDS_EVIDENCE),
                "{name} 必须带冻结原因码"
            );
        }
    }

    /// 默认策略（生产未知宿主 / 静态全关夹具）：会话能力矩阵全关。
    ///
    /// 会话 DTO 的基线就是执行闸的同一 `current_policy()`：默认 source 全关时，
    /// 手机矩阵不得出现任何开闸项。
    #[tokio::test]
    async fn session_capabilities_are_closed_by_default() {
        let fake = Arc::new(FakeBackend::default());
        let hub = Arc::new(ZcodeHub::with_parts(
            stored(URL),
            fake.clone(),
            CapabilityView::default(),
        ));
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let view = hub
            .session(&TargetKey {
                session_id: "sess_synthetic0000".into(),
            })
            .await
            .unwrap();
        assert!(view.ready, "只读投影本身必须就绪");
        assert_capabilities_closed(&view.capabilities);
    }

    /// 不新鲜投影的 GET 同样不得携带开闸矩阵：与事件路径
    /// `apply_relay_session` 同规（坏帧/解码/序号错误后 GET 读到的 ready=false）。
    ///
    /// 策略 source 全开（send_policy）时，ready=true 的 GET 必须按策略开闸；
    /// ready=false 的 GET 必须回基线全关，且写回的 `last_session` 同样全关。
    #[tokio::test]
    async fn session_get_closes_capabilities_when_projection_not_ready() {
        let fake = Arc::new(FakeBackend::default());
        let hub = Arc::new(ZcodeHub::with_parts(
            stored(URL),
            fake.clone(),
            send_policy(),
        ));
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let key = TargetKey {
            session_id: "sess_not_ready_get".into(),
        };

        // fake 的投影不新鲜：GET 仍成功返回投影，但能力矩阵必须全关。
        fake.session_unready.store(true, Ordering::SeqCst);
        let stale = hub.session(&key).await.expect("GET 仍成功返回投影");
        assert!(!stale.ready, "fake 已把投影标成不新鲜");
        assert_eq!(
            stale.capabilities,
            CapabilityView::default(),
            "不新鲜投影不得携带开闸矩阵（GET 与事件路径同规）"
        );
        {
            let inner = hub.lock();
            let last = inner
                .last_session
                .as_ref()
                .expect("GET 已播种 last_session");
            assert!(!last.ready);
            assert_eq!(
                last.capabilities,
                CapabilityView::default(),
                "写回 last_session 的能力矩阵也必须全关"
            );
        }

        // 投影恢复就绪：同一策略源下 GET 必须按策略开闸（不是被上一轮全关固化）。
        fake.session_unready.store(false, Ordering::SeqCst);
        let ready = hub.session(&key).await.expect("恢复后 GET");
        assert!(ready.ready);
        assert_eq!(
            ready.capabilities,
            send_policy(),
            "ready 投影必须按当前策略现算能力矩阵"
        );
    }

    /// 同一动态策略源同时喂会话矩阵与执行闸：翻转开/关，两者同步变化。
    #[tokio::test]
    async fn session_matrix_and_execute_gate_share_one_policy_source() {
        use std::sync::atomic::AtomicBool;

        let open = Arc::new(AtomicBool::new(false));
        let flag = open.clone();
        let source: CapabilitySource = Arc::new(move || {
            if flag.load(Ordering::SeqCst) {
                send_policy()
            } else {
                CapabilityView::default()
            }
        });
        let fake = Arc::new(FakeBackend::default());
        let hub = Arc::new(ZcodeHub::with_parts_and_policy_source(
            stored(URL),
            fake.clone(),
            source,
        ));
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let generation = hub.connection().generation;
        let key = TargetKey {
            session_id: "sess_synthetic0000".into(),
        };

        // ① 全关：矩阵关 + 闸拒绝（零绑定零执行）。
        let closed = hub.session(&key).await.expect("会话投影应可读");
        assert_capabilities_closed(&closed.capabilities);
        let refused = hub
            .execute(ctx("policy-source-closed-1"), send_action(generation))
            .await
            .unwrap_err();
        assert!(refused.is(codes::NEEDS_EVIDENCE));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);

        // ② 翻转开：会话矩阵与 execute 闸同时放行。
        open.store(true, Ordering::SeqCst);
        let opened = hub.session(&key).await.expect("会话投影应可读");
        assert!(opened.capabilities.send.enabled);
        assert_eq!(opened.capabilities.send.reason_code, None);
        let receipt = hub
            .execute(ctx("policy-source-open"), send_action(generation))
            .await
            .expect("同一 source 开闸后 execute 必须放行");
        assert_eq!(receipt.state, ActionState::Accepted);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);

        // ③ 翻转回全关：矩阵与闸同时收回，下一次动作零执行。
        open.store(false, Ordering::SeqCst);
        let closed_again = hub.session(&key).await.expect("会话投影应可读");
        assert_capabilities_closed(&closed_again.capabilities);
        let refused = hub
            .execute(ctx("policy-source-closed-2"), send_action(generation))
            .await
            .unwrap_err();
        assert!(refused.is(codes::NEEDS_EVIDENCE));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
    }

    // ---------------------------------------------------------------------
    // T8.2 debug 证据许可：桌面发放 → 手机恰好一次动作 → 闸门重新关闭。
    // 普通构建（无 feature）不编入这些用例。
    // ---------------------------------------------------------------------

    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    async fn paired_hub_closed(fake: Arc<FakeBackend>) -> Arc<ZcodeHub> {
        let hub = Arc::new(ZcodeHub::with_parts(
            stored(URL),
            fake.clone(),
            CapabilityView::default(),
        ));
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        hub
    }

    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    const EVIDENCE_SESSION: &str = "sess_synthetic0000";

    /// 许可只开自己那一项能力、只开本会话；一次动作消费后下一次 GET 立即全关。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn armed_permit_enables_only_that_capability_for_one_session() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub_closed(fake.clone()).await;
        let generation = hub.connection().generation;
        let key = TargetKey {
            session_id: EVIDENCE_SESSION.into(),
        };

        hub.arm_evidence(EVIDENCE_SESSION, generation, "send")
            .expect("当前代际应可发放许可");

        // 发放后下一次 GET：只有被许可的那一项开闸，其余仍全关。
        let armed = hub.session(&key).await.expect("会话投影应可读");
        assert!(armed.capabilities.send.enabled, "已发放许可必须反映到 GET");
        assert_eq!(armed.capabilities.send.reason_code, None);
        for (name, flag) in [
            ("single_select", &armed.capabilities.single_select),
            ("multi_select", &armed.capabilities.multi_select),
            ("free_text", &armed.capabilities.free_text),
            ("interrupt", &armed.capabilities.interrupt),
            ("approval_reply", &armed.capabilities.approval_reply),
            ("model", &armed.capabilities.model),
            ("thought_level", &armed.capabilities.thought_level),
        ] {
            assert!(!flag.enabled, "{name} 未被许可，不得开闸");
        }

        // 恰一次动作消费许可；下一次 GET 回到全关（手机硬门随之关闭）。
        hub.execute(ctx("evidence-view-1"), send_action(generation))
            .await
            .expect("有效许可必须放行一次动作");
        let consumed = hub.session(&key).await.expect("会话投影应可读");
        assert_capabilities_closed(&consumed.capabilities);
    }

    /// 许可不跨会话、不跨代际：其他会话同代际与同会话新代际的 GET 都必须全关。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn permit_does_not_leak_to_other_session_or_generation() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub_closed(fake.clone()).await;
        let generation = hub.connection().generation;
        let key = TargetKey {
            session_id: EVIDENCE_SESSION.into(),
        };
        hub.arm_evidence(EVIDENCE_SESSION, generation, "send")
            .unwrap();

        // 前提：本会话当前代际确实能看到许可。
        let armed = hub.session(&key).await.unwrap();
        assert!(armed.capabilities.send.enabled);

        // 其他会话：同一代际也不得借用。
        let other = hub
            .session(&TargetKey {
                session_id: "sess_synthetic_other".into(),
            })
            .await
            .unwrap();
        assert_capabilities_closed(&other.capabilities);

        // 换代（显式断开推进 Hub 代际）后重连：旧许可不得在新代际的 GET 上复活。
        hub.disconnect().await;
        hub.connect().await.expect("重连应成功");
        let new_generation = hub.connection().generation;
        assert!(new_generation > generation, "断开必须推进代际");
        let after = hub.session(&key).await.unwrap();
        assert_eq!(after.generation, new_generation);
        assert_capabilities_closed(&after.capabilities);
    }

    /// 一次许可恰放行一次动作；第二个动作、其他会话、旧代际都回到原闸。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn evidence_permit_allows_one_action_then_closes() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub_closed(fake.clone()).await;
        let generation = hub.connection().generation;

        // 桌面发放（当前代际 + 已知能力名）。
        hub.arm_evidence(EVIDENCE_SESSION, generation, "send")
            .expect("当前代际应可发放许可");

        // 第一次动作：许可匹配 → 放行。
        let first = hub
            .execute(ctx("evidence-1"), send_action(generation))
            .await
            .expect("有效许可必须放行一次动作");
        assert_eq!(first.state, ActionState::Accepted);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);

        // 第二次动作（新 actionId）：许可已消费 → 回到 needs_evidence 且零 execute。
        let second = hub
            .execute(ctx("evidence-2"), send_action(generation))
            .await
            .unwrap_err();
        assert!(second.is(codes::NEEDS_EVIDENCE), "许可是单次的");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);

        // 目标守卫不变：许可只属于发放时的会话，其他会话不得借用。
        hub.arm_evidence(EVIDENCE_SESSION, generation, "send")
            .unwrap();
        let other = ControlAction::Send {
            session_id: "sess_synthetic_other".into(),
            generation,
            text: "synthetic other".into(),
        };
        let err = hub.execute(ctx("evidence-3"), other).await.unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);

        // 旧代际动作即便许可在场也先被代际门拒绝。
        hub.arm_evidence(EVIDENCE_SESSION, generation, "send")
            .unwrap();
        let stale = hub
            .execute(ctx("evidence-4"), send_action(generation + 1))
            .await
            .unwrap_err();
        assert!(stale.is(codes::STALE_GENERATION));

        // 原会话的许可未被其他会话/旧代际消耗：仍可放行一次。
        hub.execute(ctx("evidence-5"), send_action(generation))
            .await
            .expect("原会话许可应仍有效");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 2);
    }

    /// snooze 只把许可绑到该 interactionId（不消费）；其他题不得借用，最终 answer 消费。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn snooze_binds_permit_without_consuming() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub_closed(fake.clone()).await;
        let generation = hub.connection().generation;

        hub.arm_evidence(EVIDENCE_SESSION, generation, "single_select")
            .unwrap();

        let snooze = |interaction: &str| ControlAction::Snooze {
            session_id: EVIDENCE_SESSION.into(),
            generation,
            interaction_id: interaction.into(),
        };
        let answer = |interaction: &str| ControlAction::Answer {
            session_id: EVIDENCE_SESSION.into(),
            generation,
            interaction_id: interaction.into(),
            answers: vec![AnswerEntry {
                question_text: "synthetic question".into(),
                selected_values: vec!["synthetic option".into()],
                free_text: None,
            }],
        };

        // 首次 snooze 绑题（不消费）；重复同题幂等。
        hub.execute(ctx("snooze-1"), snooze("ix-1")).await.unwrap();
        hub.execute(ctx("snooze-2"), snooze("ix-1")).await.unwrap();
        // 许可已绑定 ix-1：其他题的 snooze 不得改绑/借用 → 能力闸拒绝（零执行）。
        let err = hub
            .execute(ctx("snooze-3"), snooze("ix-2"))
            .await
            .unwrap_err();
        assert!(
            err.is(codes::NEEDS_EVIDENCE),
            "许可绑定到其他题时 snooze 必须拒绝，绝不得借用"
        );

        // 其他题的最终 answer 不能借用许可。
        let err = hub
            .execute(ctx("answer-other"), answer("ix-2"))
            .await
            .unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE), "其他交互不得借用许可");
        assert_eq!(
            fake.execute_calls.load(Ordering::SeqCst),
            2,
            "被拒绝的 snooze/answer 零 execute（前两次 snooze 各自一次）"
        );

        // 绑定题的 answer 原子消费许可。
        hub.execute(ctx("answer-1"), answer("ix-1"))
            .await
            .expect("snooze 绑定的题必须能消费许可");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 3);

        // 消费后回到闸门关闭。
        let err = hub
            .execute(ctx("answer-2"), answer("ix-1"))
            .await
            .unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 3);
    }

    /// 事件泵的 fake 桥：记录 `on_data`/`invalidate`，按队列注入每帧的
    /// [`DataOutcome`]（错误 + 降级视图 / 成功 + 变化 / 空 = `Ok` 无变化）。
    #[derive(Default)]
    struct FakePump {
        data: Mutex<Vec<Value>>,
        invalidations: AtomicUsize,
        /// 每帧结果（取走一次；空 = `Ok` + 无变化）。
        outcomes: Mutex<VecDeque<DataOutcome>>,
        /// T5 确认观察：按会话注入的日志头指纹（缺席 = `None`，与 trait 默认一致）。
        fingerprints: Mutex<BTreeMap<String, String>>,
    }

    impl FakePump {
        /// 排入下一帧的桥结果（错误与变化同时携带，模拟坏帧已降级投影）。
        fn push_outcome(&self, result: ZResult<()>, changed: Vec<SessionView>) {
            self.outcomes
                .lock()
                .unwrap()
                .push_back(DataOutcome { result, changed });
        }

        /// 注入某会话的日志头指纹（T5 send 确认观察面）。
        fn set_fingerprint(&self, session_id: &str, fingerprint: &str) {
            self.fingerprints
                .lock()
                .unwrap()
                .insert(session_id.to_owned(), fingerprint.to_owned());
        }
    }

    impl BridgePump for FakePump {
        fn on_data<'a>(&'a self, payload: Value) -> BoxFuture<'a, DataOutcome> {
            self.data.lock().unwrap().push(payload);
            let outcome = self
                .outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(DataOutcome {
                    result: Ok(()),
                    changed: Vec::new(),
                });
            Box::pin(async move { outcome })
        }

        fn invalidate<'a>(&'a self) -> BoxFuture<'a, ()> {
            self.invalidations.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }

        fn log_fingerprint(&self, session_id: &str) -> Option<String> {
            self.fingerprints.lock().unwrap().get(session_id).cloned()
        }
    }

    /// 等到下一条 `Session` 事件（跳过连接/动作事件；广播通道会缓冲未读事件）。
    async fn next_session_event(receiver: &mut broadcast::Receiver<ControlEvent>) -> ControlEvent {
        loop {
            match receiver.recv().await {
                Ok(event) if event.kind == ControlEventKind::Session => return event,
                Ok(_) => continue,
                Err(error) => panic!("事件通道关闭：{error}"),
            }
        }
    }

    fn relay_view(state: ConnectionState, generation: u64) -> ConnectionView {
        ConnectionView {
            state,
            generation,
            reason_code: None,
            device_label: Some("synthetic-relay".into()),
        }
    }

    /// 带事件泵的 Hub（fake relay 通道 + fake 桥，零网络）。
    fn pumped(
        store: DeviceStore,
        backend: Arc<FakeBackend>,
        policy: CapabilityView,
    ) -> (Arc<ZcodeHub>, mpsc::Sender<RelayEvent>, Arc<FakePump>) {
        let (events, receiver) = mpsc::channel(8);
        let pump = Arc::new(FakePump::default());
        let hub = Arc::new(ZcodeHub::with_pump(
            store,
            backend,
            policy,
            receiver,
            pump.clone(),
        ));
        (hub, events, pump)
    }

    /// 带事件泵 + Data 溢出观察口（`RelayLag`）的 Hub，用于验证溢出失效接线。
    fn pumped_with_lag(
        store: DeviceStore,
        backend: Arc<FakeBackend>,
        policy: CapabilityView,
    ) -> (
        Arc<ZcodeHub>,
        mpsc::Sender<RelayEvent>,
        Arc<FakePump>,
        RelayLag,
    ) {
        let (events, receiver) = mpsc::channel(8);
        let pump = Arc::new(FakePump::default());
        let lag = RelayLag::new();
        let hub = Arc::new(ZcodeHub::with_pump_and_lag(
            store,
            backend,
            policy,
            receiver,
            pump.clone(),
            lag.clone(),
        ));
        (hub, events, pump, lag)
    }

    /// 触发一次 relay Data 溢出（置位 + 补一帧唤醒泵），等到**本次**桥投影已作废。
    async fn trigger_relay_lag(events: &mpsc::Sender<RelayEvent>, pump: &FakePump, lag: &RelayLag) {
        let before = pump.invalidations.load(Ordering::SeqCst);
        lag.set();
        events
            .send(RelayEvent::Data(serde_json::json!({"nudge": 1})))
            .await
            .unwrap();
        wait_until(|| pump.invalidations.load(Ordering::SeqCst) > before).await;
    }

    /// relay `Data` 逐帧进桥；单帧错误只丢该帧，泵继续。
    #[tokio::test]
    async fn relay_data_is_pumped_into_bridge() {
        let fake = Arc::new(FakeBackend::default());
        let (_hub, events, pump) = pumped(stored(URL), fake, send_policy());

        pump.push_outcome(Err(ZcodeError::offline()), Vec::new());
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 1})))
            .await
            .unwrap();
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 2})))
            .await
            .unwrap();
        wait_until(|| pump.data.lock().unwrap().len() == 2).await;

        let seen = pump.data.lock().unwrap().clone();
        assert_eq!(seen[0]["seq"], 1);
        assert_eq!(seen[1]["seq"], 2, "首帧错误后泵必须继续");
    }

    /// Data 帧的会话变化必须由 Hub 发布为 `Session` 事件（不手工写 SSE）：
    /// 代际由 Hub 覆写、能力矩阵只在同会话时沿用（否则回到基线全关），
    /// 无变化的 Data 不重复发事件。
    #[tokio::test]
    async fn relay_data_publishes_changed_session_event() {
        const SYNCED: &str = "sess_synthetic_pump_change";
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, pump) = pumped(stored(URL), fake, send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        let generation = hub.connection().generation;
        let mut subscriber = hub.subscribe();

        // 合成变化视图：桥投影代际 ≠ Hub 代际、带一张 pending 卡与一条 control，
        // 能力矩阵故意全开（桥投影不携带 Hub 能力，发布时绝不得沿用）。
        let synthetic =
            |revision: u64, view_generation: u64, capabilities: CapabilityView| SessionView {
                session_id: SYNCED.into(),
                generation: view_generation,
                revision: Some(revision),
                ready: true,
                pending: vec![PendingView {
                    interaction_id: "pi-synthetic-pump-change".into(),
                    kind: PendingKind::Permission,
                    anchor_row_id: Some(3),
                    tool_call_id: Some("call_synthetic_pump_change".into()),
                    created_at: Some(1_700_000_000_000),
                    summary: Some("synthetic: pump change".into()),
                    tool_name: Some("Write".into()),
                    schema_interaction: None,
                    sensitive: false,
                    auto_resolution: false,
                    auto_resolution_state: None,
                    questions: Vec::new(),
                    options: Vec::new(),
                }],
                control: Some(ControlView {
                    active_works: vec![ActiveWork {
                        kind: "primaryTurn".into(),
                        foreground_execution_id: Some("E-synthetic-pump-change".into()),
                    }],
                }),
                selection: None,
                capabilities,
            };
        // ① 未 GET 过的会话：能力矩阵回基线全关，代际/会话/卡面全部按视图落事件。
        pump.push_outcome(Ok(()), vec![synthetic(7, generation + 9, send_policy())]);
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 1})))
            .await
            .unwrap();
        let event = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            next_session_event(&mut subscriber),
        )
        .await
        .expect("Data 的会话变化必须发布 Session 事件");
        assert_eq!(event.session_id.as_deref(), Some(SYNCED));
        assert_eq!(event.generation, generation, "Hub 代际覆写桥投影代际");
        assert_eq!(event.device_id, None, "会话事件无设备维度");
        assert_eq!(event.data["sessionId"], SYNCED);
        assert_eq!(event.data["generation"], generation);
        assert_eq!(event.data["revision"], 7);
        assert_eq!(event.data["ready"], true);
        assert_eq!(
            event.data["pending"][0]["interactionId"],
            "pi-synthetic-pump-change"
        );
        assert_eq!(
            event.data["control"]["activeWorks"][0]["foregroundExecutionId"],
            "E-synthetic-pump-change"
        );
        assert_eq!(
            event.data["capabilities"],
            serde_json::to_value(CapabilityView::default()).unwrap(),
            "未知会话的能力矩阵必须回到基线全关（不伪造开闸）"
        );

        // ② 同一会话的后续变化：沿用 Hub 已持有的能力矩阵（GET 时现算的权威值）。
        //    公开 GET 在普通构建只会给全关矩阵，这里直接播种上一条已发布视图。
        {
            let mut inner = hub.lock();
            let mut authoritative = inner.last_session.clone().expect("已发布会话视图");
            authoritative.capabilities = send_policy();
            inner.last_session = Some(authoritative);
        }
        pump.push_outcome(
            Ok(()),
            vec![synthetic(8, generation + 9, CapabilityView::default())],
        );
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 2})))
            .await
            .unwrap();
        let event = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            next_session_event(&mut subscriber),
        )
        .await
        .expect("同会话的 Data 变化同样必须发布");
        assert_eq!(event.data["revision"], 8);
        assert_eq!(
            event.data["capabilities"]["send"]["enabled"], true,
            "同会话必须沿用 Hub 的能力矩阵"
        );

        // ③ 无变化的 Data：泵继续吸收，但绝不新增 Session 事件。
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 3})))
            .await
            .unwrap();
        wait_until(|| pump.data.lock().unwrap().len() == 3).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        while let Ok(event) = subscriber.try_recv() {
            assert_ne!(
                event.kind,
                ControlEventKind::Session,
                "无变化的 Data 不得发布 Session 事件"
            );
        }
    }

    /// 桥帧错误（坏 CRC/解码/序号）也必须把降级视图发给手机：Hub 泵先应用本帧的
    /// 全部视图变化（即使 `result=Err`），再丢弃错误，连接保持 paired。
    ///
    /// 降级视图的能力矩阵必须回基线全关（绝不沿用 `last_session` 的旧开闸矩阵）；
    /// 随后 ready=true 的帧发布恢复事件，GET/重绑路径仍可用。
    #[tokio::test]
    async fn relay_data_error_publishes_degraded_session_and_recovers() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, pump) = pumped(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        assert_eq!(hub.connection().state, ConnectionState::Paired);
        let generation = hub.connection().generation;

        let key = TargetKey {
            session_id: "sess_bridge_error".into(),
        };
        let primed = hub.session(&key).await.expect("首次 GET");
        assert!(primed.ready);
        assert_eq!(primed.capabilities, send_policy(), "GET 现算出开闸矩阵");
        let mut subscriber = hub.subscribe();

        // 桥错误 + 已降级的视图（能力故意带旧开闸矩阵：Hub 必须清回基线）。
        let degraded = SessionView {
            session_id: key.session_id.clone(),
            generation: generation + 9,
            revision: Some(2),
            ready: false,
            pending: Vec::new(),
            control: None,
            selection: None,
            capabilities: send_policy(),
        };
        pump.push_outcome(
            Err(ZcodeError::of(codes::UNSUPPORTED_PROTOCOL)),
            vec![degraded],
        );
        events
            .send(RelayEvent::Data(serde_json::json!({"bad": 1})))
            .await
            .unwrap();

        let event = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            next_session_event(&mut subscriber),
        )
        .await
        .expect("错误帧的降级视图必须发布 Session 事件");
        assert_eq!(event.session_id.as_deref(), Some(key.session_id.as_str()));
        assert_eq!(event.generation, generation, "对外恒用 Hub 代际");
        assert_eq!(event.data["ready"], false);
        assert_eq!(
            event.data["capabilities"],
            serde_json::to_value(CapabilityView::default()).unwrap(),
            "降级视图不得沿用 last_session 的旧开闸矩阵"
        );
        assert_eq!(
            hub.connection().state,
            ConnectionState::Paired,
            "桥帧错误不得改变连接态"
        );
        {
            let inner = hub.lock();
            let last = inner
                .last_session
                .as_ref()
                .expect("降级后仍有 last_session");
            assert!(!last.ready);
            assert_eq!(last.capabilities, CapabilityView::default());
        }

        // 恢复帧：ready=true 同样发布；GET/重绑仍可用（能力按当前策略现算）。
        let recovered = SessionView {
            session_id: key.session_id.clone(),
            generation: generation + 9,
            revision: Some(3),
            ready: true,
            pending: Vec::new(),
            control: None,
            selection: None,
            capabilities: CapabilityView::default(),
        };
        pump.push_outcome(Ok(()), vec![recovered]);
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 2})))
            .await
            .unwrap();
        let event = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            next_session_event(&mut subscriber),
        )
        .await
        .expect("恢复帧必须发布 Session 事件");
        assert_eq!(event.session_id.as_deref(), Some(key.session_id.as_str()));
        assert_eq!(event.data["ready"], true);
        assert_eq!(event.generation, generation);

        let rebound = hub.session(&key).await.expect("GET 仍可用");
        assert!(rebound.ready);
        assert_eq!(rebound.generation, generation);
        assert_eq!(
            rebound.capabilities,
            send_policy(),
            "GET 重新现算能力矩阵（不沿用失效缓存）"
        );
    }

    /// 连续两个错误帧都必须被处理、泵绝不退出；空变化的错误不发布事件，
    /// 随后正常帧仍能发布视图。
    #[tokio::test]
    async fn relay_data_errors_do_not_stop_pump() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, pump) = pumped(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        let generation = hub.connection().generation;
        let mut subscriber = hub.subscribe();

        pump.push_outcome(Err(ZcodeError::of(codes::UNSUPPORTED_PROTOCOL)), Vec::new());
        pump.push_outcome(Err(ZcodeError::of(codes::UNSUPPORTED_PROTOCOL)), Vec::new());
        for seq in 1..=2 {
            events
                .send(RelayEvent::Data(serde_json::json!({"seq": seq})))
                .await
                .unwrap();
        }
        wait_until(|| pump.data.lock().unwrap().len() == 2).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        while let Ok(event) = subscriber.try_recv() {
            assert_ne!(
                event.kind,
                ControlEventKind::Session,
                "空变化的错误帧不得发布 Session 事件"
            );
        }
        assert_eq!(
            hub.connection().state,
            ConnectionState::Paired,
            "错误帧不得改变连接态"
        );

        // 第三帧正常：视图仍能发布 → 泵没有因连续错误退出。
        let alive = SessionView {
            session_id: "sess_pump_alive".into(),
            generation: generation + 9,
            revision: Some(1),
            ready: true,
            pending: Vec::new(),
            control: None,
            selection: None,
            capabilities: CapabilityView::default(),
        };
        pump.push_outcome(Ok(()), vec![alive]);
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 3})))
            .await
            .unwrap();
        let event = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            next_session_event(&mut subscriber),
        )
        .await
        .expect("连续错误后泵仍须处理后续帧");
        assert_eq!(event.session_id.as_deref(), Some("sess_pump_alive"));
        assert_eq!(event.data["ready"], true);
    }

    /// Data 溢出必须显式让投影失效：invalidate 一次 + 发布 ready=false/能力全关。
    ///
    /// 注意：`hub.session()` 本来每次都会 `bind_session` + `session`，故本用例不断言
    /// 「GET 重新 bind」（那没有判别力）；旧桥帧不得复活投影由 bridge 层用例
    /// `invalidate_drops_old_bridge_frames_and_rebind_uses_fresh_bridge` 覆盖。
    #[tokio::test]
    async fn relay_lag_invalidates_projection_and_publishes_stale_session() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, pump, lag) = pumped_with_lag(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        assert_eq!(hub.connection().state, ConnectionState::Paired);

        let key = TargetKey {
            session_id: "sess_lag_stale".into(),
        };
        // 真实 GET 播种（跟踪表 + last_session 同时登记）：能力矩阵由 GET 现算
        //（当前策略 = send_policy，send 开），lag 必须清回基线全关。
        let primed = hub.session(&key).await.expect("首次 GET");
        assert!(primed.ready);
        assert_eq!(primed.capabilities, send_policy());
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 1);
        assert_eq!(hub.lock().tracked_sessions.len(), 1, "GET 必须登记跟踪表");
        let generation = hub.connection().generation;
        let mut subscriber = hub.subscribe();

        // 触发 relay 侧 Data 溢出信号。溢出发生时队列本就积压着待处理事件，这里补一帧
        // 唤醒泵；泵会在下一轮先处理 lag 标记（本用例只断言失效结果，不断言两者顺序）。
        lag.set();
        events
            .send(RelayEvent::Data(serde_json::json!({"nudge": 1})))
            .await
            .unwrap();
        wait_until(|| pump.data.lock().unwrap().len() == 1).await;
        wait_until(|| pump.invalidations.load(Ordering::SeqCst) == 1).await;
        assert_eq!(
            pump.invalidations.load(Ordering::SeqCst),
            1,
            "lag 必须作废一次桥投影"
        );

        let downgraded = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            next_session_event(&mut subscriber),
        )
        .await
        .expect("lag 必须发布投影降级 Session 事件");
        assert_eq!(
            downgraded.session_id.as_deref(),
            Some(key.session_id.as_str())
        );
        assert_eq!(downgraded.generation, generation);
        assert_eq!(
            downgraded.data["ready"], false,
            "Data 溢出后旧投影必须显式失效"
        );
        assert_eq!(
            downgraded.data["capabilities"],
            serde_json::to_value(CapabilityView::default()).unwrap(),
            "失效投影的能力矩阵必须回到基线全关（不得沿用旧缓存）"
        );

        // 缓存中的 last_session 已被就地降级（不是只在事件里装样子）。
        assert!(
            !hub.lock()
                .last_session
                .as_ref()
                .expect("降级后仍有 last_session")
                .ready,
            "last_session 必须写回 ready=false"
        );
    }

    /// Data 溢出必须让**每个**已跟踪会话失效（不是只通知最后一个），且 GET 能自愈；
    /// 离开 paired 时清空跟踪表，旧桥会话不得在重连后复活。
    #[tokio::test]
    async fn relay_lag_downgrades_every_tracked_session_and_get_recovers() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, pump, lag) = pumped_with_lag(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        assert_eq!(hub.connection().state, ConnectionState::Paired);
        let generation = hub.connection().generation;

        let key_a = TargetKey {
            session_id: "sess_lag_a".into(),
        };
        let key_b = TargetKey {
            session_id: "sess_lag_b".into(),
        };
        // 两个会话各自 GET：都必须是 ready，并各自登记进跟踪表。
        assert!(hub.session(&key_a).await.expect("A 首次 GET").ready);
        assert!(hub.session(&key_b).await.expect("B 首次 GET").ready);
        {
            let mut inner = hub.lock();
            assert_eq!(
                inner.tracked_sessions.len(),
                2,
                "每次成功 GET 都必须登记会话"
            );
            // 能力伪造成全开：lag 必须把**每个**会话的能力清回基线（不得沿用旧许可）。
            for view in inner.tracked_sessions.values_mut() {
                view.capabilities = send_policy();
            }
        }
        let mut subscriber = hub.subscribe();

        trigger_relay_lag(&events, &pump, &lag).await;

        let mut downgraded = Vec::new();
        for _ in 0..2 {
            downgraded.push(
                tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    next_session_event(&mut subscriber),
                )
                .await
                .expect("lag 必须为每个已跟踪会话各发布一条 Session 事件"),
            );
        }
        let order: Vec<&str> = downgraded
            .iter()
            .map(|event| {
                event
                    .session_id
                    .as_deref()
                    .expect("会话事件必须带 sessionId")
            })
            .collect();
        assert_eq!(
            order,
            vec![key_a.session_id.as_str(), key_b.session_id.as_str()],
            "会话降级事件必须按 sessionId 稳定排序"
        );
        for event in &downgraded {
            assert_eq!(event.generation, generation, "事件代际 = 当前 Hub 代际");
            assert_eq!(event.data["generation"], generation);
            assert_eq!(event.data["ready"], false, "每个已跟踪会话都必须显式失效");
            assert_eq!(
                event.data["capabilities"],
                serde_json::to_value(CapabilityView::default()).unwrap(),
                "失效投影的能力矩阵必须回到基线全关"
            );
        }
        {
            let inner = hub.lock();
            assert!(
                inner.tracked_sessions.values().all(|view| !view.ready),
                "跟踪表必须就地降级（不是只在事件里装样子）"
            );
            assert!(
                !inner
                    .last_session
                    .as_ref()
                    .expect("降级后仍有 last_session")
                    .ready,
                "last_session 必须同步降级"
            );
        }
        // 每个会话恰好一条：不得重复发布降级事件（手机按它一次性重拉）。
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        while let Ok(event) = subscriber.try_recv() {
            assert_ne!(
                event.kind,
                ControlEventKind::Session,
                "降级不得重复发布（每个会话恰好一条）"
            );
        }

        // GET 自愈：重新现读，回到 ready 且能力按当前策略现算（不沿用失效缓存）。
        let recovered_a = hub.session(&key_a).await.expect("lag 后 A 重新 GET");
        assert!(recovered_a.ready, "GET 必须自愈回 ready");
        assert_eq!(recovered_a.generation, generation);
        assert_eq!(recovered_a.capabilities, send_policy());
        assert!(hub.session(&key_b).await.expect("lag 后 B 重新 GET").ready);
        assert!(
            hub.lock().tracked_sessions.values().all(|view| view.ready),
            "GET 恢复后跟踪表不得残留失效视图"
        );

        // 离开 paired（断开）：跟踪表清空，旧桥会话不得在重连后复活。
        hub.disconnect().await;
        assert!(hub.lock().tracked_sessions.is_empty(), "断开必须清空跟踪表");
    }

    /// relay 掉线**不换代**：离开 paired 时同样逐会话广播 ready=false（不只是
    /// last_session 一条，也不只是 connection 帧），并清空跟踪表。
    #[tokio::test]
    async fn relay_offline_notifies_every_tracked_session() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, _pump) = pumped(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        assert_eq!(hub.connection().state, ConnectionState::Paired);
        let generation = hub.connection().generation;
        let key_a = TargetKey {
            session_id: "sess_offline_a".into(),
        };
        let key_b = TargetKey {
            session_id: "sess_offline_b".into(),
        };
        assert!(hub.session(&key_a).await.expect("A GET").ready);
        assert!(hub.session(&key_b).await.expect("B GET").ready);
        let mut subscriber = hub.subscribe();

        // relay 掉线（同代）：控制代际不变，只离开 paired。
        events
            .send(RelayEvent::State(relay_view(ConnectionState::Offline, 7)))
            .await
            .unwrap();
        wait_until(|| hub.connection().state == ConnectionState::Offline).await;
        assert_eq!(
            hub.connection().generation,
            generation,
            "relay 掉线不得推进控制代际"
        );

        let mut downgraded = Vec::new();
        for _ in 0..2 {
            downgraded.push(
                tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    next_session_event(&mut subscriber),
                )
                .await
                .expect("掉线必须为每个已跟踪会话广播降级事件"),
            );
        }
        let order: Vec<&str> = downgraded
            .iter()
            .map(|event| event.session_id.as_deref().expect("会话事件必带 sessionId"))
            .collect();
        assert_eq!(
            order,
            vec![key_a.session_id.as_str(), key_b.session_id.as_str()],
            "逐会话降级必须按 sessionId 稳定排序"
        );
        for event in &downgraded {
            assert_eq!(
                event.generation, generation,
                "掉线不换代：沿用当前 Hub 代际"
            );
            assert_eq!(event.data["ready"], false);
            assert_eq!(
                event.data["capabilities"],
                serde_json::to_value(CapabilityView::default()).unwrap()
            );
        }
        {
            let inner = hub.lock();
            assert!(
                inner.tracked_sessions.is_empty(),
                "离开 paired 必须清空跟踪表"
            );
            assert!(inner.tracked_order.is_empty(), "登记序也必须同步清空");
        }
        // 无第三条（每个会话恰好一条）。
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        while let Ok(event) = subscriber.try_recv() {
            assert_ne!(event.kind, ControlEventKind::Session, "不得重复广播");
        }
    }

    /// 同代掉线（不换代）时在途 GET 绝不能把 ready=true 登记回 Hub：末段锁内要求
    /// 仍是 paired，否则按当前连接态报错且零登记（跟踪表 / last_session 都不写）。
    #[tokio::test]
    async fn session_get_in_flight_offline_is_rejected_without_registration() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, _pump) = pumped(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        let generation = hub.connection().generation;

        let gate = Arc::new(Notify::new());
        fake.session_gate.lock().unwrap().replace(gate.clone());
        let key = TargetKey {
            session_id: "sess_inflight_offline".into(),
        };
        let for_task = hub.clone();
        let handle = tokio::spawn(async move { for_task.session(&key).await });
        wait_until(|| fake.session_calls.load(Ordering::SeqCst) == 1).await;

        // relay 同代掉线：控制代际不变，GET 仍在途。
        events
            .send(RelayEvent::State(relay_view(ConnectionState::Offline, 7)))
            .await
            .unwrap();
        wait_until(|| hub.connection().state == ConnectionState::Offline).await;
        assert_eq!(hub.connection().generation, generation);

        gate.notify_one();
        let error = handle.await.expect("GET 任务不得 panic").unwrap_err();
        assert!(
            error.is(codes::OFFLINE),
            "掉线后完成 GET 必须报当前连接态，而不是登记 ready=true"
        );
        let inner = hub.lock();
        assert!(inner.tracked_sessions.is_empty(), "掉线后不得登记跟踪视图");
        assert!(inner.tracked_order.is_empty(), "掉线后不得登记跟踪序");
        assert!(inner.last_session.is_none(), "不得写回 last_session");
    }

    /// 登记 cap+1 个会话：故意让**登记序与字典序相反**（先登记的 `z_first` 字典序
    /// 最大），用于证明淘汰按最后登记序（LRU）而不是按 key 排序。
    async fn overfill_tracked_sessions(hub: &ZcodeHub) {
        let first = TargetKey {
            session_id: "z_first".into(),
        };
        assert!(hub.session(&first).await.expect("首次 GET 应成功").ready);
        for index in 0..TRACKED_SESSION_CAP {
            let key = TargetKey {
                session_id: format!("a_{index:03}"),
            };
            assert!(hub.session(&key).await.expect("GET 应成功").ready);
        }
    }

    /// 跟踪表有界且按**最后登记序**淘汰（LRU）：最久未登记的会话先出局，与字典序无关；
    /// 已登记会话再次 GET = 最近登记（移到队尾，不扩容、不淘汰）。
    #[tokio::test]
    async fn tracked_sessions_evict_least_recently_tracked() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, _events, _pump, _lag) = pumped_with_lag(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();

        overfill_tracked_sessions(&hub).await;

        let newest = format!("a_{:03}", TRACKED_SESSION_CAP - 1);
        {
            let inner = hub.lock();
            assert_eq!(
                inner.tracked_sessions.len(),
                TRACKED_SESSION_CAP,
                "跟踪表不得超过上限"
            );
            assert_eq!(
                inner.tracked_order.len(),
                TRACKED_SESSION_CAP,
                "登记序与跟踪表长度必须一致"
            );
            assert!(
                !inner.tracked_sessions.contains_key("z_first"),
                "超限必须淘汰最久未登记的会话（LRU），而不是字典序最小的 key"
            );
            assert!(
                inner.tracked_sessions.contains_key("a_000"),
                "字典序最小的会话不是淘汰对象"
            );
            assert_eq!(
                inner.tracked_order.front().map(String::as_str),
                Some("a_000"),
                "队首 = 当前最久未登记的会话"
            );
            assert_eq!(
                inner.tracked_order.back().map(String::as_str),
                Some(newest.as_str()),
                "队尾 = 最近登记的会话"
            );
        }

        // 已登记会话再次 GET = 最近登记：只改登记序，不扩容、不淘汰。
        let refreshed = TargetKey {
            session_id: "a_000".into(),
        };
        assert!(
            hub.session(&refreshed)
                .await
                .expect("重复 GET 应成功")
                .ready
        );
        let inner = hub.lock();
        assert_eq!(
            inner.tracked_sessions.len(),
            TRACKED_SESSION_CAP,
            "重复登记不得扩容"
        );
        assert_eq!(
            inner.tracked_order.back().map(String::as_str),
            Some("a_000"),
            "再次 GET 必须把会话移到登记序队尾"
        );
        assert_eq!(
            inner.tracked_order.front().map(String::as_str),
            Some("a_001"),
            "次久未登记的会话成为新队首"
        );
    }

    /// 第二次 Data 溢出：跟踪表非空但已全部降级 → 不得重复发 Session 事件（防刷）。
    #[tokio::test]
    async fn second_relay_lag_does_not_republish_downgraded_sessions() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, pump, lag) = pumped_with_lag(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        for session_id in ["sess_lag_a", "sess_lag_b"] {
            let key = TargetKey {
                session_id: session_id.into(),
            };
            assert!(hub.session(&key).await.expect("GET 应成功").ready);
        }
        let mut subscriber = hub.subscribe();

        trigger_relay_lag(&events, &pump, &lag).await;
        for _ in 0..2 {
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                next_session_event(&mut subscriber),
            )
            .await
            .expect("首轮 lag 必须逐会话降级");
        }

        // 第二次溢出：桥投影仍作废，但全部会话已 `ready=false` → 零 Session 事件。
        trigger_relay_lag(&events, &pump, &lag).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        while let Ok(event) = subscriber.try_recv() {
            assert_ne!(
                event.kind,
                ControlEventKind::Session,
                "已全部降级时不得重复广播（防刷新风暴）"
            );
        }
    }

    /// 有界代价（不是假阳性）：被 LRU 淘汰的会话不再收到 lag 降级事件；它的下一次
    /// GET 会重新登记并现读——跟踪表只服务最后登记的 32 个会话。
    #[tokio::test]
    async fn evicted_session_is_not_notified_by_lag() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, pump, lag) = pumped_with_lag(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        overfill_tracked_sessions(&hub).await;
        let mut subscriber = hub.subscribe();

        trigger_relay_lag(&events, &pump, &lag).await;

        // 恰好跟踪表内每个会话一条，被淘汰的 `z_first` 不在其中。
        for _ in 0..TRACKED_SESSION_CAP {
            let event = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                next_session_event(&mut subscriber),
            )
            .await
            .expect("每个跟踪会话各一条降级事件");
            assert_ne!(
                event.session_id.as_deref(),
                Some("z_first"),
                "被 LRU 淘汰的会话不得收到 lag 事件（有界代价，下一次 GET 会重新登记）"
            );
        }
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        while let Ok(event) = subscriber.try_recv() {
            assert_ne!(
                event.kind,
                ControlEventKind::Session,
                "只发跟踪表内的会话（不得补发第 33 条）"
            );
        }
    }

    /// relay `KICKED` → Hub `conflict` 终态：推进代际、作废桥/投影、旧动作零执行。
    #[tokio::test]
    async fn relay_kicked_state_is_conflict_and_invalidates_old_generation() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, pump) = pumped(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        let old_generation = hub.connection().generation;
        let mut subscriber = hub.subscribe();

        events
            .send(RelayEvent::State(relay_view(ConnectionState::Conflict, 7)))
            .await
            .unwrap();
        wait_until(|| hub.connection().state == ConnectionState::Conflict).await;

        let new_generation = hub.connection().generation;
        assert!(new_generation > old_generation);
        assert_eq!(
            pump.invalidations.load(Ordering::SeqCst),
            1,
            "冲突必须作废旧桥/投影（一次）"
        );

        let err = hub
            .execute(ctx("synthetic-action-kicked"), send_action(old_generation))
            .await
            .unwrap_err();
        assert!(err.is(codes::STALE_GENERATION));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 0);

        let mut saw_conflict = false;
        while let Ok(event) = subscriber.try_recv() {
            if event.kind == ControlEventKind::Connection
                && event.generation == new_generation
                && event.data["state"] == serde_json::json!("conflict")
            {
                saw_conflict = true;
            }
        }
        assert!(saw_conflict, "Hub 必须广播新代际的 conflict 连接事件");
    }

    /// 构造（生产与 fake）都不得自动 OAuth/自动连接；生产策略默认全关。
    #[tokio::test]
    async fn hub_construction_does_not_authorize_or_connect() {
        let fake = Arc::new(FakeBackend::default());
        let hub = ZcodeHub::with_parts(stored(URL), fake.clone(), send_policy());
        assert_eq!(hub.connection().state, ConnectionState::NeedsAuth);
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.disconnect_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.reset_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);

        let unconfigured = ZcodeHub::with_parts(DeviceStore::memory(), fake, send_policy());
        assert_eq!(
            unconfigured.connection().state,
            ConnectionState::NeedsConfig
        );

        // 生产链构造：只装配零件与空转事件泵，不取 token、不开 WS、不广播。
        let production = ZcodeHub::new(stored(URL));
        assert_eq!(production.connection().state, ConnectionState::NeedsAuth);
        assert_eq!(production.policy(), CapabilityView::default());
        assert!(production.subscribe().try_recv().is_err());
    }

    /// `connect` 只从保存链接解析目标（sid + 可选 mid，不猜测），并使用 Hub 新代际。
    #[tokio::test]
    async fn connect_uses_saved_route_and_new_generation() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, _events, _pump) = pumped(stored(URL_WITH_MID), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        let first = hub.connect().await.unwrap();
        assert!(first.is_paired());
        assert_eq!(first.generation, hub.connection().generation);
        {
            let routes = fake.routes.lock().unwrap();
            let route = routes.last().expect("必须收到连接目标");
            assert_eq!(route.0, "synthetic-hub-0001");
            assert_eq!(route.1.as_deref(), Some("synthetic-mid-0002"));
        }

        // 配置变更推进新代际：重连结果用新代际，不回传后端自己的旧 generation。
        let old_generation = first.generation;
        hub.config_changed().await;
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        let second = hub.connect().await.unwrap();
        assert!(second.generation > old_generation);
        assert_eq!(second.generation, hub.connection().generation);

        // 无 mid 的保存链接：原样缺失（不合成、不猜测）。
        let (plain, _plain_events, _plain_pump) = pumped(stored(URL), fake.clone(), send_policy());
        plain
            .authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        plain.connect().await.unwrap();
        let routes = fake.routes.lock().unwrap();
        assert_eq!(routes.last().expect("必须收到连接目标").1, None);
    }

    /// 配置保存/删除（换/删链接）：推进代际 → `reset` 只断 relay/投影，**保留账号 JWT**；
    /// 旧动作与旧 relay 状态不得复活，且无需重新 OAuth 即可连新目标。
    #[tokio::test]
    async fn config_changed_keeps_account_and_drops_projection() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, _pump) = pumped(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        let old_generation = hub.connection().generation;
        // 先让 Hub 应用一次旧 relay 状态（抬高 relay_generation，作为 floor 基准）。
        events
            .send(RelayEvent::State(relay_view(ConnectionState::Offline, 5)))
            .await
            .unwrap();
        wait_until(|| hub.connection().state == ConnectionState::Offline).await;

        let view = hub.config_changed().await;
        assert_eq!(
            view.state,
            ConnectionState::Offline,
            "换链接保留账号 JWT（不得退回 needs_auth）"
        );
        assert!(view.generation > old_generation);
        assert_eq!(
            *fake.reset_generations.lock().unwrap(),
            vec![view.generation],
            "reset 必须拿到新代际（断 relay/作废投影）"
        );
        assert_eq!(fake.reset_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fake.authorize_calls.load(Ordering::SeqCst),
            1,
            "换链接不得重新 OAuth"
        );
        assert!(hub.auth_status().authorized, "账号授权必须保留");

        // 已应用代际之前的旧 relay 状态不得复活（floor 拦下）。
        events
            .send(RelayEvent::State(relay_view(ConnectionState::Paired, 5)))
            .await
            .unwrap();
        settle().await;
        assert_eq!(hub.connection().state, ConnectionState::Offline);

        // 旧代际动作：零执行。
        let err = hub
            .execute(ctx("synthetic-action-reset"), send_action(old_generation))
            .await
            .unwrap_err();
        assert!(err.is(codes::STALE_GENERATION));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);

        // 保留的账号 JWT 直接可用：无需重新授权即可连新目标。
        let reconnected = hub.connect().await.unwrap();
        assert!(reconnected.is_paired());
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 2);
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 1);
    }

    /// 启动：幂等 start 只起一个 worker，恢复一次授权并连接一次；零 authorize/execute。
    #[tokio::test]
    async fn startup_restores_and_connects_once_without_authorize() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        let store = stored(URL);
        set_remote_enabled(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store.clone(),
            fake.clone(),
            send_policy(),
        ));

        hub.start_auto_connect();
        hub.start_auto_connect();
        wait_for_connection(&hub, ConnectionState::Paired).await;

        assert_eq!(fake.restore_calls.load(Ordering::SeqCst), 1, "只恢复一次");
        assert_eq!(
            fake.authorize_calls.load(Ordering::SeqCst),
            0,
            "复用持久授权，绝不自动 OAuth"
        );
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            1,
            "两个 start 只产生一次连接"
        );
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
        assert!(hub.auth_status().authorized);
        assert_eq!(
            stored_auto(&store),
            None,
            "启动恢复不得擅自改写用户意图（缺键即默认 true）"
        );
    }

    /// 授权成功即用户连接意图：持久 true 并唤醒自动连接，不需要第二次点击。
    #[tokio::test]
    async fn authorize_success_auto_connects_without_second_click() {
        let fake = Arc::new(FakeBackend::default());
        let store = stored(URL);
        set_remote_enabled(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store.clone(),
            fake.clone(),
            send_policy(),
        ));
        hub.start_auto_connect();
        // 首次尝试：没有持久授权 → 停（不打开浏览器）。
        wait_until(|| fake.restore_calls.load(Ordering::SeqCst) == 1).await;
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 0);

        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        wait_for_connection(&hub, ConnectionState::Paired).await;

        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            stored_auto(&store).as_deref(),
            Some("true"),
            "授权成功必须持久化连接意图"
        );
    }

    /// 远程总开关关闭：worker 不得连接，也不得打开浏览器/读取保险箱。
    #[tokio::test]
    async fn remote_disabled_never_connects_or_opens_browser() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        let store = stored(URL);
        set_remote_enabled(&store, false);
        let hub = Arc::new(ZcodeHub::with_parts(store, fake.clone(), send_policy()));

        hub.start_auto_connect();
        hub.wake_auto_connect();
        // 同步信号：等 worker 把初始尝试 + 本次唤醒都跑完，再断言没有越权动作。
        wait_for_auto_attempt(&hub, 2).await;

        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            fake.restore_calls.load(Ordering::SeqCst),
            0,
            "总开关关闭时前置就停，连存储恢复都不做"
        );
    }

    /// 用户显式断开 → 持久 false；运行时断开（退出/停服/远程关闭）不改用户意图。
    #[tokio::test]
    async fn requested_disconnect_is_persistent_but_runtime_close_is_not() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        let store = stored(URL);
        set_remote_enabled(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store.clone(),
            fake.clone(),
            send_policy(),
        ));

        assert!(hub.connect_requested().await.unwrap().is_paired());
        assert_eq!(stored_auto(&store).as_deref(), Some("true"));
        assert_eq!(fake.restore_calls.load(Ordering::SeqCst), 1);

        // 运行时断开：保留 JWT、不写用户意图。
        hub.disconnect().await;
        assert_eq!(hub.connection().state, ConnectionState::Offline);
        assert_eq!(stored_auto(&store).as_deref(), Some("true"));

        // 再次显式连接不重复恢复（内存已授权）。
        assert!(hub.connect_requested().await.unwrap().is_paired());
        assert_eq!(fake.restore_calls.load(Ordering::SeqCst), 1);

        // 用户主动断开：先 checked 落 false，再断开。
        let view = hub.disconnect_requested().await.unwrap();
        assert_eq!(view.state, ConnectionState::Offline);
        assert_eq!(stored_auto(&store).as_deref(), Some("false"));

        // 暂停后 worker 不得重连。
        let connects = fake.connect_calls.load(Ordering::SeqCst);
        hub.start_auto_connect();
        wait_for_auto_attempt(&hub, 1).await;
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), connects);
    }

    /// `connect_requested`/`disconnect_requested` 的 checked 写失败必须如实回错且零副作用。
    #[tokio::test]
    async fn requested_writes_report_storage_failure_without_side_effect() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        let store = stored(URL);
        let hub = ZcodeHub::with_parts(store.clone(), fake.clone(), send_policy());
        hub.connect_requested().await.unwrap();
        assert!(hub.connection().is_paired());
        let connects = fake.connect_calls.load(Ordering::SeqCst);

        break_settings(&store);
        let err = hub.disconnect_requested().await.unwrap_err();
        assert!(err.is(codes::STORAGE_FAILED));
        assert_eq!(
            fake.disconnect_calls.load(Ordering::SeqCst),
            0,
            "写失败不得悄悄断开"
        );
        assert!(hub.connection().is_paired());

        let err = hub.connect_requested().await.unwrap_err();
        assert!(err.is(codes::STORAGE_FAILED));
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), connects);
        assert!(hub.connection().is_paired());
    }

    /// 换链接只断旧目标、保留账号；显式忘记才清凭据并删除记录（链接保留）。
    #[tokio::test]
    async fn route_change_preserves_account_forget_does_not() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        let store = stored(URL);
        set_remote_enabled(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store.clone(),
            fake.clone(),
            send_policy(),
        ));
        hub.start_auto_connect();
        wait_for_connection(&hub, ConnectionState::Paired).await;
        assert_eq!(fake.forget_calls.load(Ordering::SeqCst), 0);

        // 换链接：旧连接/投影作废，账号 JWT 保留（零 forget、零新 OAuth）。
        store.with(|conn| set_config(conn, URL_NEXT).unwrap());
        let changed = hub.config_changed().await;
        assert_eq!(changed.state, ConnectionState::Offline);
        assert_eq!(fake.forget_calls.load(Ordering::SeqCst), 0);
        assert!(hub.auth_status().authorized);
        assert_eq!(fake.reset_calls.load(Ordering::SeqCst), 1);

        // 自动 worker 改连新目标；仍不重新授权。
        hub.wake_auto_connect();
        wait_for_connection(&hub, ConnectionState::Paired).await;
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 2);
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 0);
        {
            let routes = fake.routes.lock().unwrap();
            assert_eq!(
                routes.last().expect("必须收到新连接目标").0,
                "synthetic-hub-0002"
            );
        }

        // 显式忘记：清授权 + 删除持久记录；设备链接保留。
        let view = hub.forget_authorization().await.unwrap();
        assert_eq!(view.state, ConnectionState::NeedsAuth);
        assert_eq!(fake.forget_calls.load(Ordering::SeqCst), 1);
        assert!(!hub.auth_status().authorized);
        assert_eq!(stored_auto(&store).as_deref(), Some("false"));
        assert_eq!(
            store.with(|conn| get_config(conn).config_state),
            ConfigState::Saved,
            "忘记授权必须保留设备链接"
        );
        assert_eq!(
            store.with(|conn| get_config(conn).url),
            Some(URL_NEXT.to_owned())
        );

        // 忘记后即使被唤醒也不得自动重连。
        let connects = fake.connect_calls.load(Ordering::SeqCst);
        let attempts = hub.auto_attempt_count();
        hub.wake_auto_connect();
        wait_for_auto_attempt(&hub, attempts + 1).await;
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), connects);
        assert_eq!(hub.connection().state, ConnectionState::NeedsAuth);
    }

    /// 忘记时删除失败：如实回 storage_failed（problem 可见），但状态保持已清且不恢复。
    #[tokio::test]
    async fn forget_delete_failure_reports_storage_failed_but_stays_cleared() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        fake.set_forget_result(AuthStatus {
            authorized: false,
            problem: Some(AuthProblem::DeleteFailed),
            ..AuthStatus::default()
        });
        let store = stored(URL);
        set_remote_enabled(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store.clone(),
            fake.clone(),
            send_policy(),
        ));
        hub.start_auto_connect();
        wait_for_connection(&hub, ConnectionState::Paired).await;

        let err = hub.forget_authorization().await.unwrap_err();
        assert!(err.is(codes::STORAGE_FAILED));
        assert_eq!(fake.forget_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            hub.auth_status().problem,
            Some(AuthProblem::DeleteFailed),
            "删除失败必须如实可见"
        );
        assert!(!hub.auth_status().authorized);
        assert_eq!(hub.connection().state, ConnectionState::NeedsAuth);
        assert_eq!(stored_auto(&store).as_deref(), Some("false"));

        let connects = fake.connect_calls.load(Ordering::SeqCst);
        let attempts = hub.auto_attempt_count();
        hub.wake_auto_connect();
        wait_for_auto_attempt(&hub, attempts + 1).await;
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), connects);
    }

    /// P2-4：忘授权的关闭窗口必须覆盖**身份清 + 持久删除 + relay 清理**整段——
    /// 删除在途时 `connect_requested` 不得用旧 JWT 建连；放行后 needs_auth、JWT 清、
    /// 记录删，显式连接不再拿旧凭据。
    #[tokio::test]
    async fn forget_holds_closing_through_identity_delete() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        let store = stored(URL);
        set_remote_enabled(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store.clone(),
            fake.clone(),
            send_policy(),
        ));
        hub.start_auto_connect();
        wait_for_connection(&hub, ConnectionState::Paired).await;
        assert!(fake.holds_jwt(), "前置：替身持有账号 JWT");
        let connects_before = fake.connect_calls.load(Ordering::SeqCst);

        // 删除闸：替身把「清内存 + 删除」整体挂在闸后（比生产更保守——真实
        // `AuthService::forget` 先同步清内存、再走慢删除；生产竞态另由
        // `forget_quiesces_inflight_connect_before_deleting_identity` 与
        // `production_forget_keeps_closing_across_slow_delete` 覆盖）。
        let gate = Arc::new(Notify::new());
        fake.forget_gate.lock().unwrap().replace(gate.clone());
        let forgetting = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.forget_authorization().await })
        };
        wait_until(|| fake.forget_calls.load(Ordering::SeqCst) == 1).await;

        // 身份缓存已清、自动连接已暂停，但删除在途：关闭窗口必须仍拒绝连接。
        assert!(
            fake.holds_jwt(),
            "删除在途时替身仍持有旧 JWT（正是要封的窗口）"
        );
        assert!(hub.auto_connect_paused(), "忘记必须先置内存暂停");
        let err = hub.connect_requested().await.unwrap_err();
        assert!(
            err.is(codes::OFFLINE),
            "删除在途必须立即 offline（不得用旧 JWT 建连），实际 {err:?}"
        );
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            connects_before,
            "关闭窗口内不得用旧 JWT 建连"
        );

        gate.notify_one();
        let view = forgetting
            .await
            .expect("spawn 不应 panic")
            .expect("忘记必须成功");
        assert_eq!(view.state, ConnectionState::NeedsAuth);
        assert_eq!(fake.forget_calls.load(Ordering::SeqCst), 1, "不得重复删除");
        assert!(!fake.holds_jwt(), "JWT 必须已清");
        assert!(!hub.auth_status().authorized);
        assert!(
            !hub.auth_status().remembered,
            "持久记录已删除：替身恢复不再返回已授权"
        );
        assert!(hub.lock().closing.is_none(), "忘记结束后关闭窗口必须清位");

        // guard 释放后显式连接不拿旧 JWT：needs_auth 且零后端连接。
        let err = hub.connect_requested().await.unwrap_err();
        assert!(err.is(codes::NEEDS_AUTH), "实际 {err:?}");
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            connects_before,
            "忘记后显式连接不得触达后端连接"
        );
    }

    /// P2-4（复审 Important）：静默 relay（取消在途握手/等 connect_lock/再断一次）必须
    /// **先于**删除身份。旧顺序先删后断：在途握手可在慢删除期间完成并留下一条 live 的
    /// 旧 JWT 连接，而 Hub 对 stale 结果不调 `backend.disconnect`。
    #[tokio::test]
    async fn forget_quiesces_inflight_connect_before_deleting_identity() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        let store = stored(URL);
        set_remote_enabled(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store.clone(),
            fake.clone(),
            send_policy(),
        ));
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        assert!(fake.holds_jwt(), "前置：替身持有账号 JWT");

        // 在途握手：卡在 fake.connect 内（持 connect_lock）。
        let connect_gate = Arc::new(Notify::new());
        fake.gate.lock().unwrap().replace(connect_gate.clone());
        let inflight = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.connect().await })
        };
        wait_until(|| fake.connect_calls.load(Ordering::SeqCst) == 1).await;

        // 删除闸：让「静默完成、删除在途」的窗口可确定观察。
        let forget_gate = Arc::new(Notify::new());
        fake.forget_gate
            .lock()
            .unwrap()
            .replace(forget_gate.clone());
        let forgetting = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.forget_authorization().await })
        };
        wait_until(|| hub.connection().state == ConnectionState::Offline).await;

        // 关键顺序：删除尚未开始就必须已发出 relay 静默（第一断）；旧实现此时
        // 仍卡在 backend.forget 内、disconnect_calls 为 0（本等待即超时 → RED）。
        wait_until(|| fake.disconnect_calls.load(Ordering::SeqCst) >= 1).await;
        assert!(
            fake.forget_calls.load(Ordering::SeqCst) == 0,
            "静默必须先于删除身份"
        );

        // 放行在途握手：Hub 结果作废（stale），绝不把状态拉回 paired。
        connect_gate.notify_one();
        let stale = inflight.await.expect("spawn 不应 panic");
        assert!(
            matches!(&stale, Err(err) if err.is(codes::STALE_GENERATION)),
            "在途握手必须作废，实际 {stale:?}"
        );
        assert_eq!(hub.connection().state, ConnectionState::Offline);

        // 删除在途：第二次静默已关掉竞态窗口内建立的连接 ⇒ 没有 live 的旧 JWT 连接。
        wait_until(|| fake.forget_calls.load(Ordering::SeqCst) == 1).await;
        assert!(
            fake.disconnect_calls.load(Ordering::SeqCst) >= 2,
            "删除前必须完成两次 relay 静默"
        );
        assert!(!fake.is_live(), "删除期间不得留下 live 的旧 JWT 连接");
        assert!(fake.holds_jwt(), "删除在途：替身仍持旧 JWT（窗口前提）");
        assert_eq!(hub.connection().state, ConnectionState::Offline);
        // 删除在途仍拒绝新连接且零后端 connect。
        let connects_before = fake.connect_calls.load(Ordering::SeqCst);
        assert!(hub.connect().await.unwrap_err().is(codes::OFFLINE));
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), connects_before);

        // 放行删除：末态 needs_auth、JWT/记录清、关闭窗口清位。
        forget_gate.notify_one();
        let view = forgetting
            .await
            .expect("spawn 不应 panic")
            .expect("忘记必须成功");
        assert_eq!(view.state, ConnectionState::NeedsAuth);
        assert_eq!(fake.forget_calls.load(Ordering::SeqCst), 1);
        assert!(!fake.holds_jwt(), "JWT 必须已清");
        assert!(!fake.is_live(), "忘记结束后不得留有 live 连接");
        assert!(hub.lock().closing.is_none(), "忘记结束后必须清位");
        assert!(hub
            .connect_requested()
            .await
            .unwrap_err()
            .is(codes::NEEDS_AUTH));
    }

    /// P2-4 生产语义（真实 AuthService + 内存保险箱）：生产 `AuthService::forget` 先
    /// **同步**清内存 JWT、再在慢路径删除持久记录；关闭窗口必须覆盖整段慢删除——
    /// 期间 connect 被拒且零 relay 连接，删除后持久记录消失、显式连接不再拿旧 JWT。
    ///
    /// 这条用例避免把测试替身「把清内存也挂在闸后」的保守契约当成生产 RED：真实实现
    /// 下清内存不会被删除拖慢，真正要封的是「慢删除期间的连接窗口」。
    #[tokio::test]
    async fn production_forget_keeps_closing_across_slow_delete() {
        // exp=2100 的合成 JWT：只验证本地 exp 解析，不涉及签名校验。
        const FUTURE_JWT: &str = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJzdWIiOiJzeW50aGV0aWMtYWNjb3VudCIsImV4cCI6NDEwMjQ0NDgwMH0.synthetic-signature";

        let store = stored(URL);
        set_remote_enabled(&store, true);
        let credentials = Arc::new(MemoryCredentialStore::new());
        credentials.seed(StoredCredential {
            schema_version: SCHEMA_VERSION,
            provider: OAuthProvider::Bigmodel,
            jwt: FUTURE_JWT.to_owned(),
        });
        let auth = Arc::new(
            AuthService::new(reqwest::Client::new()).with_credential_store(credentials.clone()),
        );
        let (parts, relay_events, lag) =
            PartsBackend::production_test(store.clone(), auth.clone(), "ws://127.0.0.1:9".into());
        let bridge = parts.pump_bridge();
        let backend: Arc<dyn HubBackend> = Arc::new(parts);
        let hub = Arc::new(ZcodeHub::with_pump_and_lag(
            store.clone(),
            backend,
            send_policy(),
            relay_events,
            bridge,
            lag,
        ));
        assert!(hub.restore_authorization().await.authorized);
        assert_eq!(hub.connection().state, ConnectionState::Offline);

        // 删除闸：停在 AuthService 的慢路径删除上（内存 JWT 此时已同步清掉）。
        let gate = credentials.gate_delete();
        let forgetting = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.forget_authorization().await })
        };
        gate.wait_entered().await;
        assert!(
            hub.lock().closing.is_some(),
            "慢删除在途：关闭窗口必须仍置位"
        );
        assert!(
            !hub.auth_status().authorized,
            "真实 forget 先同步清内存 JWT（不随慢删除推迟）"
        );
        let err = hub.connect().await.unwrap_err();
        assert!(
            err.is(codes::OFFLINE),
            "慢删除在途 connect 必须 offline，实际 {err:?}"
        );
        assert_eq!(hub.connection().state, ConnectionState::Offline);
        assert_eq!(credentials.delete_count(), 1, "删除已进入慢路径");

        gate.release();
        let view = forgetting
            .await
            .expect("spawn 不应 panic")
            .expect("忘记必须成功");
        assert_eq!(view.state, ConnectionState::NeedsAuth);
        assert_eq!(credentials.delete_count(), 1, "不得重复删除");
        assert!(credentials.stored().is_none(), "持久记录必须已删除");
        assert!(hub.lock().closing.is_none(), "忘记结束后必须清位");
        // 显式连接：真实 AuthService 已无内存/持久 JWT ⇒ needs_auth，零新 WS。
        let err = hub.connect_requested().await.unwrap_err();
        assert!(err.is(codes::NEEDS_AUTH), "实际 {err:?}");
    }

    /// P2-5：关闭窗口内被消费的自动连接唤醒不得丢——`finish_disconnect` 清位后必须
    /// 再唤醒一次；用户断开（auto=false）语义不变。
    #[tokio::test]
    async fn finish_disconnect_wakes_auto_after_fast_reopen() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        let store = stored(URL);
        set_remote_enabled(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store.clone(),
            fake.clone(),
            send_policy(),
        ));
        hub.start_auto_connect();
        wait_for_connection(&hub, ConnectionState::Paired).await;
        let connects_before = fake.connect_calls.load(Ordering::SeqCst);

        // 快关：关闭窗口内唤醒 worker——它只能看到 closing，唤醒许可被本轮消费。
        let (_view, guard) = hub.begin_disconnect();
        let attempts = hub.auto_attempt_count();
        hub.wake_auto_connect();
        wait_for_auto_attempt(&hub, attempts + 1).await;
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            connects_before,
            "关闭窗口内不得连接"
        );

        // 快开：finish 清位后必须补一次唤醒，否则上面那次唤醒就丢了。
        hub.finish_disconnect(guard).await;
        wait_until(|| fake.connect_calls.load(Ordering::SeqCst) > connects_before).await;
        wait_for_connection(&hub, ConnectionState::Paired).await;
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            connects_before + 1
        );
    }

    /// P2-5 反例（退出路径）：`finish_disconnect_silent` 清位后不得唤醒自动连接；
    /// 条件本身仍允许连接（显式唤醒立刻重连），证明差异只在「是否补唤醒」。
    #[tokio::test]
    async fn silent_finish_disconnect_does_not_wake_auto() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        let store = stored(URL);
        set_remote_enabled(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store.clone(),
            fake.clone(),
            send_policy(),
        ));
        hub.start_auto_connect();
        wait_for_connection(&hub, ConnectionState::Paired).await;
        let connects_before = fake.connect_calls.load(Ordering::SeqCst);
        let attempts = hub.auto_attempt_count();

        let (_view, guard) = hub.begin_disconnect();
        hub.finish_disconnect_silent(guard).await;
        settle().await;
        assert_eq!(hub.connection().state, ConnectionState::Offline);
        assert_eq!(
            hub.auto_attempt_count(),
            attempts,
            "静默收尾不得唤醒自动连接 worker"
        );
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            connects_before,
            "退出静默断开后不得自动重连"
        );

        // 反证：显式唤醒仍会重连（上面不连是静默，不是条件不允许）。
        hub.wake_auto_connect();
        wait_until(|| fake.connect_calls.load(Ordering::SeqCst) > connects_before).await;
        wait_for_connection(&hub, ConnectionState::Paired).await;
    }

    /// P2-5：finish 的补唤醒只做一次现查判定——用户明确断开（auto=false）仍不连接。
    #[tokio::test]
    async fn finish_disconnect_wake_keeps_manual_disconnect_intent() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        let store = stored(URL);
        set_remote_enabled(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store.clone(),
            fake.clone(),
            send_policy(),
        ));
        hub.start_auto_connect();
        wait_for_connection(&hub, ConnectionState::Paired).await;
        let connects_before = fake.connect_calls.load(Ordering::SeqCst);

        let view = hub.disconnect_requested().await.unwrap();
        assert_eq!(view.state, ConnectionState::Offline);
        assert_eq!(stored_auto(&store).as_deref(), Some("false"));
        let attempts = hub.auto_attempt_count();
        hub.wake_auto_connect();
        wait_for_auto_attempt(&hub, attempts + 1).await;
        settle().await;
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            connects_before,
            "用户断开 auto=false：finish 的补唤醒不得自动重连"
        );
        assert_eq!(hub.connection().state, ConnectionState::Offline);
    }

    /// P2-6：事件路径——KICKED 的延迟写携带事件到达时的旧代际，用户之后的显式
    /// 连接（true，更高代）绝不能被它覆盖。
    #[tokio::test]
    async fn kicked_late_write_cannot_override_newer_user_intent() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        let store = stored(URL);
        set_remote_enabled(&store, true);
        let (hub, events, _pump) = pumped(store.clone(), fake.clone(), send_policy());
        hub.start_auto_connect();
        wait_for_connection(&hub, ConnectionState::Paired).await;

        // KICKED 落库闸：事件到达即预占代际，真正的写被挂在 blocking 池上。
        let (release, done) = hub.install_kicked_intent_probe();
        events
            .send(RelayEvent::State(relay_view(ConnectionState::Conflict, 7)))
            .await
            .unwrap();
        wait_until(|| hub.connection().state == ConnectionState::Conflict).await;
        // 等延迟写真正到闸前（此后它的落库顺序完全由测试决定）。
        wait_until(|| hub.kicked_write_at_gate()).await;
        let captured = hub.auto_intent_generation();
        assert!(captured > 0, "KICKED 事件到达必须预占意图代际");

        // 用户在事件之后显式连接：意图 true，代际更高。
        let err = hub.connect_requested().await.unwrap_err();
        assert!(err.is(codes::CONFLICT));
        assert!(
            hub.auto_intent_generation() > captured,
            "用户写必须预占更高代"
        );
        assert_eq!(
            stored_auto(&store).as_deref(),
            Some("true"),
            "用户显式连接必须落 true"
        );

        // 放行迟到的 KICKED 写：代际已过期，必须跳过而不是覆盖。
        release.send(()).unwrap();
        done.recv_timeout(Duration::from_secs(2))
            .expect("KICKED 延迟写任务必须完成");
        assert_eq!(
            stored_auto(&store).as_deref(),
            Some("true"),
            "迟到的 KICKED 写不得覆盖用户更新的意图"
        );
    }

    /// KICKED（relay conflict）进入时持久 auto=false 并停止本轮，唤醒也不重连。
    #[tokio::test]
    async fn kicked_pauses_auto_connect_and_persists_false() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        let store = stored(URL);
        set_remote_enabled(&store, true);
        let (hub, events, _pump) = pumped(store.clone(), fake.clone(), send_policy());
        hub.start_auto_connect();
        wait_for_connection(&hub, ConnectionState::Paired).await;

        events
            .send(RelayEvent::State(relay_view(ConnectionState::Conflict, 7)))
            .await
            .unwrap();
        wait_until(|| hub.connection().state == ConnectionState::Conflict).await;
        wait_for_stored_auto(&store, "false").await;

        let connects = fake.connect_calls.load(Ordering::SeqCst);
        let attempts = hub.auto_attempt_count();
        hub.wake_auto_connect();
        wait_for_auto_attempt(&hub, attempts + 1).await;
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            connects,
            "KICKED 后不得自动抢回连接"
        );
        assert_eq!(hub.connection().state, ConnectionState::Conflict);
    }

    /// 已配对后再次授权：先断旧连接并推进代际，旧 relay 事件不得回写 paired。
    #[tokio::test]
    async fn authorize_after_paired_disconnects_old_connection_and_advances_generation() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, _pump) = pumped(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        let paired = hub.connection();
        assert!(paired.is_paired());
        // 抬高已应用的 relay 代际，模拟旧连接留下的可迟到状态。
        events
            .send(RelayEvent::State(relay_view(ConnectionState::Paired, 9)))
            .await
            .unwrap();
        settle().await;
        assert_eq!(hub.connection().state, ConnectionState::Paired);
        let disconnects = fake.disconnect_calls.load(Ordering::SeqCst);

        let view = hub
            .authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        assert_eq!(view.state, ConnectionState::Offline);
        assert!(view.generation > paired.generation, "重新授权必须推进代际");
        assert!(
            fake.disconnect_calls.load(Ordering::SeqCst) > disconnects,
            "已配对时重新授权必须先断开旧连接"
        );

        // 旧代际 relay 事件（<= relay_floor）不得把 Hub 拉回 paired。
        events
            .send(RelayEvent::State(relay_view(ConnectionState::Paired, 9)))
            .await
            .unwrap();
        settle().await;
        assert_eq!(hub.connection().state, ConnectionState::Offline);
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 2);
    }

    /// connect 直接返回 conflict（KICKED 在握手中落定）同样持久 auto=false。
    #[tokio::test]
    async fn connect_conflict_result_persists_auto_false() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        fake.set_connect(Ok(ConnectionView {
            state: ConnectionState::Conflict,
            generation: 7,
            reason_code: None,
            device_label: None,
        }));
        let store = stored(URL);
        set_remote_enabled(&store, true);
        let hub = ZcodeHub::with_parts(store.clone(), fake.clone(), send_policy());

        let view = hub.connect_requested().await.unwrap();
        assert_eq!(view.state, ConnectionState::Conflict);
        assert_eq!(
            stored_auto(&store).as_deref(),
            Some("false"),
            "KICKED 直接作为 connect 结果时也必须持久暂停"
        );
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 0);
    }

    /// 生产装配（真实 AuthService + 内存凭据存储）：换/删链接的 `reset` 保留账号授权。
    ///
    /// 这是旧 `reset` 调 `auth.clear()` 的**可证伪**覆盖：把 clear 加回去，本用例立即失败。
    #[tokio::test]
    async fn production_reset_preserves_restored_account_authorization() {
        // exp=2100 的合成 JWT：只验证本地 exp 解析，不涉及签名校验。
        const FUTURE_JWT: &str = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJzdWIiOiJzeW50aGV0aWMtYWNjb3VudCIsImV4cCI6NDEwMjQ0NDgwMH0.synthetic-signature";

        let store = stored(URL);
        let credentials = Arc::new(MemoryCredentialStore::new());
        credentials.seed(StoredCredential {
            schema_version: SCHEMA_VERSION,
            provider: OAuthProvider::Bigmodel,
            jwt: FUTURE_JWT.to_owned(),
        });
        let auth = Arc::new(
            AuthService::new(reqwest::Client::new()).with_credential_store(credentials.clone()),
        );
        assert!(auth.restore().await.authorized, "前置：持久身份可恢复");

        let (parts, relay_events, lag) =
            PartsBackend::production_test(store.clone(), auth.clone(), "ws://127.0.0.1:9".into());
        let bridge = parts.pump_bridge();
        let backend: Arc<dyn HubBackend> = Arc::new(parts);
        let hub = ZcodeHub::with_pump_and_lag(
            store.clone(),
            backend,
            send_policy(),
            relay_events,
            bridge,
            lag,
        );
        assert!(hub.restore_authorization().await.authorized);
        assert_eq!(hub.connection().state, ConnectionState::Offline);

        // 换链接：reset 只断 relay/投影，不得清 AuthService 内存或删除持久记录。
        let changed = hub.config_changed().await;
        assert_eq!(changed.state, ConnectionState::Offline);
        assert!(
            auth.status().authorized,
            "reset 不得清 AuthService 内存授权"
        );
        assert!(hub.auth_status().authorized, "Hub 缓存同样保留");
        assert_eq!(credentials.delete_count(), 0, "换链接不得删除持久记录");
        assert_eq!(credentials.load_count(), 1, "reset 不读存储");

        // 删链接：回到 needs_config，但账号授权仍保留（不需要重新 OAuth）。
        store.with(|conn| set_config(conn, "").unwrap());
        let removed = hub.config_changed().await;
        assert_eq!(removed.state, ConnectionState::NeedsConfig);
        assert!(auth.status().authorized, "删链接不得清账号");
        assert!(hub.auth_status().authorized);
        assert!(
            hub.connect().await.unwrap_err().is(codes::NEEDS_CONFIG),
            "删链接后连接被配置门拒绝，而不是误报 needs_auth"
        );
        assert_eq!(credentials.delete_count(), 0);
    }

    // ====================================================================
    // T5：首次真实动作确认（spec R4）——send / model / thought 接线
    // ====================================================================

    /// 确认测试的能力 key（与 fake baseline 注入值一致；key 变化即另一份账本）。
    fn confirmation_key() -> CapabilityKey {
        CapabilityKey::new("macos", "9.9.9.9999", 3, "shape", 1)
    }

    /// send 确认测试的 baseline：revision 10、日志头指纹 "u0"（与纯判据用例同约定）。
    fn send_baseline() -> ConfirmationBaseline {
        ConfirmationBaseline {
            key: confirmation_key(),
            observation: ConfirmationObservation {
                revision: Some(10),
                provider: None,
                log_fingerprint: Some("u0".to_owned()),
                active_works: None,
            },
        }
    }

    /// selection 确认测试的 baseline：revision 10、当前 provider。
    fn selection_baseline() -> ConfirmationBaseline {
        ConfirmationBaseline {
            key: confirmation_key(),
            observation: ConfirmationObservation {
                revision: Some(10),
                provider: Some("synthetic-provider".to_owned()),
                log_fingerprint: None,
                active_works: None,
            },
        }
    }

    fn selection_action(generation: u64, model: &str, thought: &str) -> ControlAction {
        ControlAction::Selection {
            session_id: "sess_synthetic0000".into(),
            generation,
            model: model.into(),
            thought: thought.into(),
        }
    }

    /// 合成一条 ready 会话变化视图（确认观察的输入面）。
    fn changed_view(session_id: &str, revision: u64) -> SessionView {
        SessionView {
            session_id: session_id.to_owned(),
            generation: 0,
            revision: Some(revision),
            ready: true,
            pending: Vec::new(),
            control: None,
            selection: None,
            capabilities: CapabilityView::default(),
        }
    }

    /// 带确认账本与短确认窗（50ms）的已配对 Hub（fake relay 通道 + fake 桥）。
    async fn confirmation_hub(
        fake: Arc<FakeBackend>,
        ledger: Arc<dyn CapabilityLedger>,
        policy: CapabilityView,
    ) -> (Arc<ZcodeHub>, mpsc::Sender<RelayEvent>, Arc<FakePump>) {
        confirmation_hub_with_timeout(fake, ledger, policy, Duration::from_millis(50)).await
    }

    /// 同上，确认窗可注入（登记竞态敏感的用例用长窗靠观察路径确认，不赌计时器）。
    async fn confirmation_hub_with_timeout(
        fake: Arc<FakeBackend>,
        ledger: Arc<dyn CapabilityLedger>,
        policy: CapabilityView,
        confirmation_timeout: Duration,
    ) -> (Arc<ZcodeHub>, mpsc::Sender<RelayEvent>, Arc<FakePump>) {
        let (events, receiver) = mpsc::channel(8);
        let pump = Arc::new(FakePump::default());
        let hub = Arc::new(ZcodeHub::with_pump_and_confirmation(
            stored(URL),
            fake,
            policy,
            receiver,
            pump.clone(),
            ledger,
            confirmation_timeout,
        ));
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        (hub, events, pump)
    }

    /// 带睡眠的轮询等待（确认计时器是真实 50ms 定时器，yield 循环等不到它）。
    async fn wait_until_with_sleep(mut condition: impl FnMut() -> bool) {
        for _ in 0..500 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("条件在 2.5s 睡眠轮询内未成立");
    }

    /// ACK-only 不升级：send 被 accepted 后，确认窗内没有新投影证据 → 账本
    /// 不写 confirmed；短窗到期由计时器写 inconclusive（保持候选，不重发）。
    #[tokio::test]
    async fn ack_only_send_times_out_inconclusive_without_resend() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(send_baseline());
        let (hub, events, pump) =
            confirmation_hub(fake.clone(), ledger.clone(), send_policy()).await;
        let generation = hub.connection().generation;

        let receipt = hub
            .execute(ctx("send-ack-1"), send_action(generation))
            .await
            .expect("send 应被受理");
        assert_eq!(receipt.state, ActionState::Accepted);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);

        // ACK-only：账本必须仍然为空（绝不因受理升级）。
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            ledger.get(&confirmation_key()).is_empty(),
            "ACK-only 绝不写 confirmed"
        );

        // 同形观察（revision/指纹都未前进）也不确认；随后短窗到期 → inconclusive。
        pump.set_fingerprint("sess_synthetic0000", "u0");
        pump.push_outcome(Ok(()), vec![changed_view("sess_synthetic0000", 10)]);
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 1})))
            .await
            .unwrap();
        wait_until_with_sleep(|| !ledger.get(&confirmation_key()).is_empty()).await;

        let rows = ledger.get(&confirmation_key());
        assert_eq!(rows.len(), 1, "同一能力只落一行");
        assert_eq!(rows[0].capability, Capability::Send);
        assert_eq!(rows[0].state, CapabilityState::Inconclusive);
        assert_eq!(rows[0].evidence.as_deref(), Some("timeout_5s"));
        assert_eq!(rows[0].reason_code, None);
        // 结构性不重发：确认窗内只派发过一次动作。
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
    }

    /// 新用户消息可见（指纹变化）+ revision 前进 → 事件泵观察确认并落账 confirmed。
    #[tokio::test]
    async fn first_send_confirms_via_pump_observation() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(send_baseline());
        let (hub, events, pump) =
            confirmation_hub(fake.clone(), ledger.clone(), send_policy()).await;
        let generation = hub.connection().generation;

        let receipt = hub
            .execute(ctx("send-confirm-1"), send_action(generation))
            .await
            .expect("send 应被受理");
        assert_eq!(receipt.state, ActionState::Accepted);

        // 新投影：revision 前进 + 日志头指纹变化（新用户消息可见）。
        pump.set_fingerprint("sess_synthetic0000", "u1");
        pump.push_outcome(Ok(()), vec![changed_view("sess_synthetic0000", 11)]);
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 2})))
            .await
            .unwrap();

        wait_until_with_sleep(|| {
            ledger
                .get(&confirmation_key())
                .first()
                .is_some_and(|row| row.state == CapabilityState::Confirmed)
        })
        .await;

        let rows = ledger.get(&confirmation_key());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].capability, Capability::Send);
        assert_eq!(rows[0].state, CapabilityState::Confirmed);
        // 判据词经真实 T2 硬闸落库（DbCapabilityLedger 的受限校验在场）。
        assert_eq!(
            rows[0].evidence.as_deref(),
            Some("revision_advanced+new_user_message")
        );
        assert_eq!(rows[0].reason_code, None);
    }

    /// 旧代际的确认结果丢弃（帧观察路径）：登记后代际推进（配置变更），随后哪怕
    /// 新代际投影视图带着完全符合判据的证据，也不得写入账本。
    ///
    /// 长确认窗（2s）保证登记计时器在本测试内**不会**触发——帧路径的丢弃由观察
    /// 自证；计时器路径由测试末尾的直调 `sweep_expired`（同计时器醒来语义：现读
    /// 当前代际）确定性地覆盖，不依赖帧/计时器的到达时序。
    #[tokio::test]
    async fn stale_generation_confirmation_is_discarded_without_ledger_write() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(send_baseline());
        let (hub, events, pump) = confirmation_hub_with_timeout(
            fake.clone(),
            ledger.clone(),
            send_policy(),
            Duration::from_secs(2),
        )
        .await;
        let generation = hub.connection().generation;

        hub.execute(ctx("send-stale-1"), send_action(generation))
            .await
            .expect("send 应被受理");

        // 配置变更推进 Hub 代际：旧代际 pending 的确认结果一律丢弃。
        hub.config_changed().await;
        let current_generation = hub.connection().generation;
        assert_ne!(current_generation, generation, "前置：换代已发生");

        // 连续喂**满足判据**的新投影（revision 前进 + 指纹变化）：帧观察路径的
        // 代际闸必须逐帧丢弃。
        for seq in 3..8 {
            pump.set_fingerprint("sess_synthetic0000", "u1");
            pump.push_outcome(Ok(()), vec![changed_view("sess_synthetic0000", 11)]);
            events
                .send(RelayEvent::Data(serde_json::json!({"seq": seq})))
                .await
                .unwrap();
        }
        // 先证明帧确实都进了桥（处理面已发生），负断言才有意义。
        wait_until(|| pump.data.lock().unwrap().len() >= 5).await;

        // 计时器路径同语义覆盖：到点 sweep 用当前代际对拍（与计时器任务醒来时
        // 的实参一致），旧代际登记只摘除零落账。
        hub.confirmations
            .sweep_expired(Instant::now(), current_generation);
        assert!(
            ledger.get(&confirmation_key()).is_empty(),
            "旧代际确认结果丢弃，不落账（帧观察与计时器两条路径）"
        );
    }

    /// 旧代际的到点收口不落账（计时器路径，端到端）：登记后代际推进、**不喂任何
    /// 帧**，登记计时器在确认窗到点醒来（现读 Hub 当前代际交给 sweep）——旧代际
    /// 登记只摘除，绝不写出 inconclusive 判决。
    ///
    /// 确认窗 2s + 睡 2.3s：唯一时序假设是「配置变更在 2s 内完成」（fake 全内存，
    /// 实际毫秒级），不涉及帧/计时器赛跑。
    #[tokio::test]
    async fn stale_generation_sweep_discards_without_ledger_write() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(send_baseline());
        let (hub, _events, _pump) = confirmation_hub_with_timeout(
            fake.clone(),
            ledger.clone(),
            send_policy(),
            Duration::from_secs(2),
        )
        .await;
        let generation = hub.connection().generation;

        hub.execute(ctx("send-stale-sweep"), send_action(generation))
            .await
            .expect("send 应被受理");
        hub.config_changed().await;
        assert_ne!(hub.connection().generation, generation, "前置：换代已发生");

        // 不喂任何帧：真实计时器在 2s 到点醒来，当前代际 ≠ 登记代际。
        tokio::time::sleep(Duration::from_millis(2_300)).await;
        assert!(
            ledger.get(&confirmation_key()).is_empty(),
            "旧代际登记到点只摘除，不落 inconclusive"
        );
    }

    /// 旧代际的**回执收口**不落账：登记（gen G）→ 派发在途（execute 闸）→ 配置
    /// 变更推进代际 → 回执 Rejected 返回——收口按当前代际对拍，旧代际登记摘除且
    /// 零落账（degraded 也不得写；回执收口不是帧观察的例外）。
    ///
    /// 长确认窗（2s）保证闸放行前登记计时器不会先行触发（否则 sweep 抢先把旧
    /// pending 摘掉，收口路径反而测不到）。
    #[tokio::test]
    async fn stale_generation_receipt_settlement_discards_without_ledger_write() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(selection_baseline());
        fake.set_execute_receipt(ActionState::Rejected, None);
        let gate = fake.arm_execute_gate();
        let (hub, _events, _pump) = confirmation_hub_with_timeout(
            fake.clone(),
            ledger.clone(),
            policy_with(&["model", "thought_level"]),
            Duration::from_secs(2),
        )
        .await;
        let generation = hub.connection().generation;

        let task = tokio::spawn({
            let hub = hub.clone();
            async move {
                hub.execute(
                    ctx("selection-stale-settle"),
                    selection_action(generation, "GLM-5.3-Flash", "low"),
                )
                .await
            }
        });
        // execute_calls ≥ 1 ⟹ 登记已完成（登记严格先于派发，替身计数在闸之前）。
        wait_until(|| fake.execute_calls.load(Ordering::SeqCst) >= 1).await;
        hub.config_changed().await;
        gate.notify_one();
        let receipt = task.await.unwrap().expect("回执返回 Ok");
        assert_eq!(receipt.state, ActionState::Rejected);

        // 收口用当前代际对拍：旧代际登记摘除零落账（登记计时器 2s 内保持沉默，
        // 账本为空即证明收口路径既没写 degraded、也没有把 pending 留给计时器）。
        assert!(
            ledger.get(&confirmation_key()).is_empty(),
            "旧代际回执收口不得落账（degraded 也不得写）"
        );
    }

    /// 同槽位二次登记是纯 no-op：保留**首个** baseline 与原计时器——新投影按首个
    /// baseline 确认落账（若第二次登记覆盖了首登记，rev 11 < 新 baseline 12 永不
    /// 确认，且替换后的新期限无计时器负责，pending 无限滞留）。
    #[tokio::test]
    async fn duplicate_registration_keeps_first_baseline_and_timer_coverage() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(send_baseline());
        // 长窗：不赌计时器，让确认走观察路径（bug 形态下也无计时器可救）。
        let (hub, events, pump) = confirmation_hub_with_timeout(
            fake.clone(),
            ledger.clone(),
            send_policy(),
            Duration::from_secs(2),
        )
        .await;
        let generation = hub.connection().generation;

        hub.execute(ctx("send-dup-1"), send_action(generation))
            .await
            .expect("send 应被受理");

        // 确认窗未决期间的第二次真实使用：baseline 已漂移（rev 12 / u9）。
        fake.set_baseline(ConfirmationBaseline {
            key: confirmation_key(),
            observation: ConfirmationObservation {
                revision: Some(12),
                provider: None,
                log_fingerprint: Some("u9".to_owned()),
                active_works: None,
            },
        });
        hub.execute(ctx("send-dup-2"), send_action(generation))
            .await
            .expect("send 应被受理");

        // 新投影 rev 11 / u1：对首个 baseline（rev 10 / u0）构成确认。
        pump.set_fingerprint("sess_synthetic0000", "u1");
        pump.push_outcome(Ok(()), vec![changed_view("sess_synthetic0000", 11)]);
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 7})))
            .await
            .unwrap();

        wait_until_with_sleep(|| {
            ledger
                .get(&confirmation_key())
                .first()
                .is_some_and(|row| row.state == CapabilityState::Confirmed)
        })
        .await;

        let rows = ledger.get(&confirmation_key());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].capability, Capability::Send);
        assert_eq!(rows[0].state, CapabilityState::Confirmed);
        assert_eq!(
            rows[0].evidence.as_deref(),
            Some("revision_advanced+new_user_message")
        );
    }

    /// selection 回执显式失败不代答共存的 send pending：model/thought_level 各自
    /// degraded，send 的未决确认保持独立并由**自己的**计时器结算（session 全量
    /// 结算会把 send 一并 degraded——回执只属于产生它的 action 的能力集）。
    #[tokio::test]
    async fn selection_receipt_does_not_touch_pending_send_on_reject() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(ConfirmationBaseline {
            key: confirmation_key(),
            observation: ConfirmationObservation {
                revision: Some(10),
                provider: Some("synthetic-provider".to_owned()),
                log_fingerprint: Some("u0".to_owned()),
                active_works: None,
            },
        });
        let (hub, _events, _pump) = confirmation_hub(
            fake.clone(),
            ledger.clone(),
            policy_with(&["send", "model", "thought_level"]),
        )
        .await;
        let generation = hub.connection().generation;

        // send 先行：ACK-only，send pending 未决。
        hub.execute(ctx("send-mixed-1"), send_action(generation))
            .await
            .expect("send 应被受理");

        // selection 回执显式失败：只结算 model/thought_level。
        fake.set_execute_receipt(ActionState::Rejected, None);
        let receipt = hub
            .execute(
                ctx("selection-mixed-1"),
                selection_action(generation, "GLM-5.3-Flash", "low"),
            )
            .await
            .expect("回执带回显，execute 返回 Ok");
        assert_eq!(receipt.state, ActionState::Rejected);

        let rows = ledger.get(&confirmation_key());
        assert_eq!(rows.len(), 2, "此刻只有 selection 两行");
        for row in &rows {
            assert!(matches!(
                row.capability,
                Capability::Model | Capability::ThoughtLevel
            ));
            assert_eq!(row.state, CapabilityState::Degraded);
            assert_eq!(row.evidence.as_deref(), Some("explicit_failure"));
        }

        // send 未被代答：它自己的确认窗到期后自己结算。
        wait_until_with_sleep(|| ledger.get(&confirmation_key()).len() == 3).await;
        let rows = ledger.get(&confirmation_key());
        let send_row = rows
            .iter()
            .find(|row| row.capability == Capability::Send)
            .expect("send pending 应保留并由自己的计时器结算");
        assert_eq!(send_row.state, CapabilityState::Inconclusive);
        assert_eq!(send_row.evidence.as_deref(), Some("timeout_5s"));
    }

    /// selection 回执因果终态（confirmed）不代答共存的 send pending：model/thought
    /// 各自 confirmed；**没有新用户消息证据的 send 绝不因此写 confirmed**（否则
    /// 实质违反「ACK-only 不升级」），它由自己的计时器结算为 inconclusive。
    #[tokio::test]
    async fn selection_receipt_does_not_touch_pending_send_on_confirm() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(ConfirmationBaseline {
            key: confirmation_key(),
            observation: ConfirmationObservation {
                revision: Some(10),
                provider: Some("synthetic-provider".to_owned()),
                log_fingerprint: Some("u0".to_owned()),
                active_works: None,
            },
        });
        let (hub, _events, _pump) = confirmation_hub(
            fake.clone(),
            ledger.clone(),
            policy_with(&["send", "model", "thought_level"]),
        )
        .await;
        let generation = hub.connection().generation;

        hub.execute(ctx("send-mixed-2"), send_action(generation))
            .await
            .expect("send 应被受理");

        // selection 回执因果终态。
        fake.set_execute_receipt(ActionState::Confirmed, None);
        let receipt = hub
            .execute(
                ctx("selection-mixed-2"),
                selection_action(generation, "GLM-5.3-Flash", "low"),
            )
            .await
            .expect("confirmed 回执返回 Ok");
        assert_eq!(receipt.state, ActionState::Confirmed);

        let rows = ledger.get(&confirmation_key());
        assert_eq!(rows.len(), 2, "send 绝不随 selection 回执升级");
        for row in &rows {
            assert!(matches!(
                row.capability,
                Capability::Model | Capability::ThoughtLevel
            ));
            assert_eq!(row.state, CapabilityState::Confirmed);
            assert_eq!(row.evidence.as_deref(), Some("selection_matched"));
        }

        // send pending 完好：自己的窗到期 → 自己的 inconclusive。
        wait_until_with_sleep(|| ledger.get(&confirmation_key()).len() == 3).await;
        let rows = ledger.get(&confirmation_key());
        let send_row = rows
            .iter()
            .find(|row| row.capability == Capability::Send)
            .expect("send pending 应保留并由自己的计时器结算");
        assert_eq!(send_row.state, CapabilityState::Inconclusive);
        assert_eq!(send_row.evidence.as_deref(), Some("timeout_5s"));
    }

    /// 动作回执显式失败 → 该 key 的该能力 degraded（冻结原因码），只影响它自己。
    #[tokio::test]
    async fn explicit_receipt_failure_degrades_only_that_capability() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(selection_baseline());
        fake.set_execute_receipt(ActionState::Rejected, None);
        let (hub, _events, _pump) = confirmation_hub(
            fake.clone(),
            ledger.clone(),
            policy_with(&["model", "thought_level"]),
        )
        .await;
        let generation = hub.connection().generation;

        let receipt = hub
            .execute(
                ctx("selection-reject-1"),
                selection_action(generation, "GLM-5.3-Flash", "high"),
            )
            .await
            .expect("回执带回显，execute 返回 Ok");
        assert_eq!(receipt.state, ActionState::Rejected);

        let rows = ledger.get(&confirmation_key());
        assert_eq!(rows.len(), 2, "只有 model/thought_level 两行");
        for row in &rows {
            assert_eq!(row.state, CapabilityState::Degraded);
            assert_eq!(row.evidence.as_deref(), Some("explicit_failure"));
            assert!(
                matches!(row.reason_code.as_deref(), Some("action_failed") | Some(_)),
                "原因码是判据标识形状"
            );
        }
        assert!(
            rows.iter()
                .all(|row| matches!(row.capability, Capability::Model | Capability::ThoughtLevel)),
            "degraded 只影响本动作涉及的能力，不波及 send 等"
        );
    }

    /// 回执已是因果终态（CommandSender 的 wait_selection 命中）→ 直接落账 confirmed。
    #[tokio::test]
    async fn confirmed_receipt_writes_selection_ledger() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(selection_baseline());
        fake.set_execute_receipt(ActionState::Confirmed, None);
        let (hub, _events, _pump) = confirmation_hub(
            fake.clone(),
            ledger.clone(),
            policy_with(&["model", "thought_level"]),
        )
        .await;
        let generation = hub.connection().generation;

        let receipt = hub
            .execute(
                ctx("selection-confirm-1"),
                selection_action(generation, "GLM-5.3-Flash", "low"),
            )
            .await
            .expect("confirmed 回执返回 Ok");
        assert_eq!(receipt.state, ActionState::Confirmed);

        let rows = ledger.get(&confirmation_key());
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row.state, CapabilityState::Confirmed);
            assert_eq!(row.evidence.as_deref(), Some("selection_matched"));
        }
    }

    /// ACK-only 的 selection：事件泵观察带错误模型的新快照 → 不确认（保持候选），
    /// 直到短窗到期写 inconclusive；正确模型的新快照则确认落账。
    #[tokio::test]
    async fn pump_observation_confirms_selection_only_on_exact_match() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(selection_baseline());
        let (hub, events, pump) = confirmation_hub(
            fake.clone(),
            ledger.clone(),
            policy_with(&["model", "thought_level"]),
        )
        .await;
        let generation = hub.connection().generation;

        hub.execute(
            ctx("selection-pump-1"),
            selection_action(generation, "GLM-5.3-Flash", "low"),
        )
        .await
        .expect("selection 应被受理");

        // 错误模型的新快照（ACK-only 形态）：不确认。
        let mut wrong = changed_view("sess_synthetic0000", 11);
        wrong.selection = Some(SelectionView {
            provider: "synthetic-provider".into(),
            model: "GLM-5.3".into(),
            thought: Some("low".into()),
            models: Vec::new(),
        });
        pump.push_outcome(Ok(()), vec![wrong]);
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 4})))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            ledger.get(&confirmation_key()).is_empty(),
            "wrong model 不确认（保持候选）"
        );

        // 正确模型的新快照：model/thought_level 各落一行 confirmed。
        let mut right = changed_view("sess_synthetic0000", 12);
        right.selection = Some(SelectionView {
            provider: "synthetic-provider".into(),
            model: "GLM-5.3-Flash".into(),
            thought: Some("low".into()),
            models: Vec::new(),
        });
        pump.push_outcome(Ok(()), vec![right]);
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 5})))
            .await
            .unwrap();
        wait_until_with_sleep(|| ledger.get(&confirmation_key()).len() == 2).await;
        for row in ledger.get(&confirmation_key()) {
            assert_eq!(row.state, CapabilityState::Confirmed);
            assert_eq!(row.evidence.as_deref(), Some("selection_matched"));
        }
    }

    /// 已 confirmed 的能力不再重复确认：登记面跳过，后续确认性观察不得改写账本。
    #[tokio::test]
    async fn already_confirmed_capability_is_not_reconfirmed() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        ledger
            .put(
                &confirmation_key(),
                &CapabilityRecord::confirmed(Capability::Send),
            )
            .unwrap();
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(send_baseline());
        let (hub, events, pump) =
            confirmation_hub(fake.clone(), ledger.clone(), send_policy()).await;
        let generation = hub.connection().generation;

        hub.execute(ctx("send-again-1"), send_action(generation))
            .await
            .expect("send 应被受理");
        pump.set_fingerprint("sess_synthetic0000", "u1");
        pump.push_outcome(Ok(()), vec![changed_view("sess_synthetic0000", 11)]);
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 6})))
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(120)).await;
        let rows = ledger.get(&confirmation_key());
        assert_eq!(rows.len(), 1, "不产生新行");
        assert_eq!(rows[0].state, CapabilityState::Confirmed);
        assert_eq!(
            rows[0].evidence, None,
            "预置行未被确认流程改写（登记面已跳过）"
        );
    }

    // ====================================================================
    // T6：首次真实作答/审批确认（spec R4/§4.1）——pending 卡式能力接线
    // ====================================================================

    /// Pending 确认测试的 baseline：revision 10（与纯判据用例同约定）。
    fn pending_baseline() -> ConfirmationBaseline {
        ConfirmationBaseline {
            key: confirmation_key(),
            observation: ConfirmationObservation {
                revision: Some(10),
                provider: None,
                log_fingerprint: None,
                active_works: None,
            },
        }
    }

    /// 审批应答测试动作（既有 permission 卡的 optionId；option 原值不进账本）。
    fn permission_action(generation: u64, interaction_id: &str, option_id: &str) -> ControlAction {
        ControlAction::Permission {
            session_id: "sess_synthetic0000".into(),
            generation,
            interaction_id: interaction_id.into(),
            option_id: option_id.into(),
        }
    }

    /// 合成一张最小宿主 pending 卡（观察面只保留 id 形状；题目/选项不入测试断言）。
    fn pending_card(interaction_id: &str) -> PendingView {
        PendingView {
            interaction_id: interaction_id.to_owned(),
            kind: PendingKind::UserInput,
            anchor_row_id: None,
            tool_call_id: None,
            created_at: None,
            summary: None,
            tool_name: None,
            schema_interaction: None,
            sensitive: false,
            auto_resolution: false,
            auto_resolution_state: None,
            questions: Vec::new(),
            options: Vec::new(),
        }
    }

    /// 带宿主卡集合的变化视图（其余字段与 `changed_view` 同形）。
    fn changed_view_with_pending(
        session_id: &str,
        revision: u64,
        cards: Vec<PendingView>,
    ) -> SessionView {
        let mut view = changed_view(session_id, revision);
        view.pending = cards;
        view
    }

    /// 问答作答首次使用：ACK 后泵观察到目标卡撤下 + revision 前进 → confirmed
    /// （判据词 `pending_cleared+revision_advanced`；确认窗内绝不重发动作）。
    ///
    /// 长确认窗（2s）：不赌计时器，让确认走观察路径（T5 同款纪律）。
    #[tokio::test]
    async fn answer_confirmation_confirms_via_pump_when_target_cleared() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(pending_baseline());
        let (hub, events, pump) = confirmation_hub_with_timeout(
            fake.clone(),
            ledger.clone(),
            policy_with(&["single_select"]),
            Duration::from_secs(2),
        )
        .await;
        let generation = hub.connection().generation;

        let receipt = hub
            .execute(
                ctx("answer-confirm-1"),
                answer_action(generation, "ix-synthetic-1", vec!["A"], None),
            )
            .await
            .expect("answer 应被受理");
        assert_eq!(receipt.state, ActionState::Accepted);

        // 新投影：revision 前进 + 目标卡已从宿主集合撤下。
        pump.push_outcome(
            Ok(()),
            vec![changed_view_with_pending("sess_synthetic0000", 11, vec![])],
        );
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 1})))
            .await
            .unwrap();

        wait_until_with_sleep(|| {
            ledger
                .get(&confirmation_key())
                .first()
                .is_some_and(|row| row.state == CapabilityState::Confirmed)
        })
        .await;

        let rows = ledger.get(&confirmation_key());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].capability, Capability::SingleSelect);
        assert_eq!(rows[0].state, CapabilityState::Confirmed);
        assert_eq!(
            rows[0].evidence.as_deref(),
            Some("pending_cleared+revision_advanced")
        );
        assert_eq!(rows[0].reason_code, None);
        // 结构性不重发：确认窗内只派发过一次动作。
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
    }

    /// 目标卡仍在场（哪怕 revision 前进、旁边的卡撤下了）：绝不确认——保持候选，
    /// 等后续帧或到点结算（不重发）。
    ///
    /// 长确认窗（2s）：观察路径在窗内判定，判定结果由判据本身决定（目标卡在场 →
    /// 永不 confirmed），账本为空即证明观察没有升级；到点词形由 tracker 级用例
    /// 确定性钉住，这里不赌帧/计时器到达时序。
    #[tokio::test]
    async fn answer_confirmation_keeps_candidate_while_target_card_present() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(pending_baseline());
        let (hub, events, pump) = confirmation_hub_with_timeout(
            fake.clone(),
            ledger.clone(),
            policy_with(&["single_select"]),
            Duration::from_secs(2),
        )
        .await;
        let generation = hub.connection().generation;

        hub.execute(
            ctx("answer-present-1"),
            answer_action(generation, "ix-synthetic-1", vec!["A"], None),
        )
        .await
        .expect("answer 应被受理");

        // 新投影：revision 前进，但目标卡仍挂在宿主集合里（旁边卡撤下不算数）。
        pump.push_outcome(
            Ok(()),
            vec![changed_view_with_pending(
                "sess_synthetic0000",
                11,
                vec![
                    pending_card("ix-synthetic-1"),
                    pending_card("ix-synthetic-2"),
                ],
            )],
        );
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 2})))
            .await
            .unwrap();
        // 先证明帧确实进了桥（观察已发生），负断言才有意义。
        wait_until(|| !pump.data.lock().unwrap().is_empty()).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            ledger.get(&confirmation_key()).is_empty(),
            "目标卡在场绝不确认（保持候选，零落账）"
        );
        // 结构性不重发：确认窗内只派发过一次动作。
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
    }

    /// multi/free 只在宿主卡形状存在时候选：题型推导只报 single_select 时，策略里
    /// 开着的多选/自由作答绝不登记、绝不确认（闸归 resolver，这里钉登记面）。
    #[tokio::test]
    async fn answer_confirmation_registers_only_shape_derived_capabilities() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(pending_baseline());
        let (hub, events, pump) = confirmation_hub_with_timeout(
            fake.clone(),
            ledger.clone(),
            policy_with(&["single_select", "multi_select", "free_text"]),
            Duration::from_secs(2),
        )
        .await;
        let generation = hub.connection().generation;

        hub.execute(
            ctx("answer-shape-1"),
            answer_action(generation, "ix-synthetic-1", vec!["A"], None),
        )
        .await
        .expect("answer 应被受理");

        pump.push_outcome(
            Ok(()),
            vec![changed_view_with_pending("sess_synthetic0000", 11, vec![])],
        );
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 3})))
            .await
            .unwrap();

        wait_until_with_sleep(|| !ledger.get(&confirmation_key()).is_empty()).await;
        let rows = ledger.get(&confirmation_key());
        assert_eq!(rows.len(), 1, "只有题型推导出的 single_select 登记确认");
        assert_eq!(rows[0].capability, Capability::SingleSelect);
        assert_eq!(rows[0].state, CapabilityState::Confirmed);
        assert!(
            rows.iter().all(|row| {
                !matches!(
                    row.capability,
                    Capability::MultiSelect | Capability::FreeText
                )
            }),
            "形状缺席的 multi/free 绝不登记"
        );
    }

    /// 混合题型（单选+多选+自由作答）一次作答同时确认三项：同卡同 requestKey，
    /// 一个观察把同动作能力集的三个槽位一起结算。
    #[tokio::test]
    async fn answer_confirmation_settles_all_shape_derived_capabilities() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(pending_baseline());
        fake.set_required(Ok(vec!["single_select", "multi_select", "free_text"]));
        let (hub, events, pump) = confirmation_hub_with_timeout(
            fake.clone(),
            ledger.clone(),
            policy_with(&["single_select", "multi_select", "free_text"]),
            Duration::from_secs(2),
        )
        .await;
        let generation = hub.connection().generation;

        hub.execute(
            ctx("answer-shape-2"),
            answer_action(generation, "ix-synthetic-1", vec!["A"], None),
        )
        .await
        .expect("answer 应被受理");

        pump.push_outcome(
            Ok(()),
            vec![changed_view_with_pending("sess_synthetic0000", 11, vec![])],
        );
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 4})))
            .await
            .unwrap();

        wait_until_with_sleep(|| ledger.get(&confirmation_key()).len() == 3).await;
        for row in ledger.get(&confirmation_key()) {
            assert!(matches!(
                row.capability,
                Capability::SingleSelect | Capability::MultiSelect | Capability::FreeText
            ));
            assert_eq!(row.state, CapabilityState::Confirmed);
            assert_eq!(
                row.evidence.as_deref(),
                Some("pending_cleared+revision_advanced")
            );
        }
    }

    /// 审批应答首次使用（既有 permission 卡）：泵观察到目标卡撤下 + revision 前进
    /// → approval_reply confirmed（spec §4.1：只确认「回答既有卡」）。
    #[tokio::test]
    async fn permission_confirmation_confirms_via_pump_when_target_cleared() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(pending_baseline());
        let (hub, events, pump) = confirmation_hub_with_timeout(
            fake.clone(),
            ledger.clone(),
            policy_with(&["approval_reply"]),
            Duration::from_secs(2),
        )
        .await;
        let generation = hub.connection().generation;

        let receipt = hub
            .execute(
                ctx("permission-confirm-1"),
                permission_action(generation, "ix-synthetic-1", "allowOnce"),
            )
            .await
            .expect("permission 应被受理");
        assert_eq!(receipt.state, ActionState::Accepted);

        pump.push_outcome(
            Ok(()),
            vec![changed_view_with_pending("sess_synthetic0000", 11, vec![])],
        );
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 5})))
            .await
            .unwrap();

        wait_until_with_sleep(|| {
            ledger
                .get(&confirmation_key())
                .first()
                .is_some_and(|row| row.state == CapabilityState::Confirmed)
        })
        .await;
        let rows = ledger.get(&confirmation_key());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].capability, Capability::ApprovalReply);
        assert_eq!(rows[0].state, CapabilityState::Confirmed);
        assert_eq!(
            rows[0].evidence.as_deref(),
            Some("pending_cleared+revision_advanced")
        );
    }

    /// 审批回执显式失败 → approval_reply degraded（explicit_failure），只影响它自己。
    #[tokio::test]
    async fn permission_receipt_failure_degrades_approval_only() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(pending_baseline());
        fake.set_execute_receipt(ActionState::Rejected, None);
        let (hub, _events, _pump) = confirmation_hub(
            fake.clone(),
            ledger.clone(),
            policy_with(&["approval_reply"]),
        )
        .await;
        let generation = hub.connection().generation;

        let receipt = hub
            .execute(
                ctx("permission-reject-1"),
                permission_action(generation, "ix-synthetic-1", "deny"),
            )
            .await
            .expect("回执带回显，execute 返回 Ok");
        assert_eq!(receipt.state, ActionState::Rejected);

        let rows = ledger.get(&confirmation_key());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].capability, Capability::ApprovalReply);
        assert_eq!(rows[0].state, CapabilityState::Degraded);
        assert_eq!(rows[0].evidence.as_deref(), Some("explicit_failure"));
    }

    /// 审批能力只确认「回答既有 permission 卡」：发送动作只登记 send——即便随后的
    /// 投影 revision 前进（且投影里没有任何卡），approval_reply 也绝不因此写行
    /// （不把「发送触发审批卡」当能力，不监听 elicitation/新卡）。
    #[tokio::test]
    async fn send_action_does_not_register_approval_for_new_permission_card() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(send_baseline());
        let (hub, events, pump) = confirmation_hub_with_timeout(
            fake.clone(),
            ledger.clone(),
            policy_with(&["send", "approval_reply"]),
            Duration::from_secs(2),
        )
        .await;
        let generation = hub.connection().generation;

        let receipt = hub
            .execute(ctx("send-no-approval-1"), send_action(generation))
            .await
            .expect("send 应被受理");
        assert_eq!(receipt.state, ActionState::Accepted);

        // 新投影：revision 前进 + 新用户消息可见（send 的确认判据全满足）。
        pump.set_fingerprint("sess_synthetic0000", "u1");
        pump.push_outcome(
            Ok(()),
            vec![changed_view_with_pending("sess_synthetic0000", 11, vec![])],
        );
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 6})))
            .await
            .unwrap();

        wait_until_with_sleep(|| {
            ledger
                .get(&confirmation_key())
                .first()
                .is_some_and(|row| row.state == CapabilityState::Confirmed)
        })
        .await;
        let rows = ledger.get(&confirmation_key());
        assert_eq!(rows.len(), 1, "只落 send 一行");
        assert_eq!(rows[0].capability, Capability::Send);
        assert!(
            rows.iter()
                .all(|row| row.capability != Capability::ApprovalReply),
            "approval_reply 不随发送动作登记/确认"
        );
    }

    /// 到点未决的 Pending 登记：最后观察里目标卡仍在场 → 判据词如实记
    /// `pending_still_present`；卡已撤的到点登记沿用一般超窗词（直调 tracker，
    /// 与计时器醒来的 sweep 语义一致——到点即 inconclusive，绝不重判）。
    #[tokio::test]
    async fn expired_pending_observation_reports_card_still_present() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(pending_baseline());
        let (hub, _events, _pump) = confirmation_hub(
            fake.clone(),
            ledger.clone(),
            policy_with(&["single_select"]),
        )
        .await;
        let generation = hub.connection().generation;

        // 直调登记面：两条已到点的 Pending 登记（同会话不同能力槽位）。
        for (capability, request_key) in [
            (Capability::SingleSelect, "ix-synthetic-1"),
            (Capability::MultiSelect, "ix-synthetic-2"),
        ] {
            assert!(hub.confirmations.register(ConfirmationContext {
                capability,
                key: confirmation_key(),
                session_id: "sess_synthetic0000".to_owned(),
                generation,
                baseline_revision: 10,
                target: ConfirmationTarget::Pending {
                    request_key: request_key.to_owned(),
                },
                deadline: Instant::now(),
            }));
        }

        // 观察帧：revision 前进，卡 ix-1 仍在场、卡 ix-2 已撤（但登记均已到点，
        // 到点分支先于判据——证据词按最后观察如实区分）。
        hub.confirmations.observe_session(
            &changed_view_with_pending(
                "sess_synthetic0000",
                11,
                vec![pending_card("ix-synthetic-1")],
            ),
            generation,
            None,
        );

        let rows = ledger.get(&confirmation_key());
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row.state, CapabilityState::Inconclusive);
            let expected = match row.capability {
                Capability::SingleSelect => "pending_still_present",
                Capability::MultiSelect => "timeout_5s",
                other => panic!("unexpected capability {other:?}"),
            };
            assert_eq!(row.evidence.as_deref(), Some(expected));
        }
    }

    // ====================================================================
    // T7：首次真实打断确认（spec R4）——interrupt 接线
    // ====================================================================

    /// interrupt 确认测试的 baseline：revision 10 + 初始 control 守卫集合
    /// （`None` = control 未宣告；只存 id 形状，回合内容不入账本）。
    fn interrupt_baseline(active_works: Option<Vec<String>>) -> ConfirmationBaseline {
        ConfirmationBaseline {
            key: confirmation_key(),
            observation: ConfirmationObservation {
                revision: Some(10),
                provider: None,
                log_fingerprint: None,
                active_works,
            },
        }
    }

    /// 打断测试动作：守卫 id = 动作自带的 `expectedForegroundExecutionId`
    /// （id 形状；绝不进日志/账本/断言正文）。
    fn stop_action(generation: u64, guard_id: &str) -> ControlAction {
        ControlAction::Stop {
            session_id: "sess_synthetic0000".into(),
            generation,
            expected_foreground_execution_id: guard_id.into(),
        }
    }

    /// 带宿主 control 守卫集合的变化视图（其余字段与 `changed_view` 同形）。
    fn changed_view_with_control(
        session_id: &str,
        revision: u64,
        guards: Vec<&str>,
    ) -> SessionView {
        let mut view = changed_view(session_id, revision);
        view.control = Some(ControlView {
            active_works: guards
                .into_iter()
                .map(|guard_id| ActiveWork {
                    kind: "primaryTurn".to_owned(),
                    foreground_execution_id: Some(guard_id.to_owned()),
                })
                .collect(),
        });
        view
    }

    /// 打断首次使用：ACK 后泵观察到目标守卫从 activeWorks 消失 + revision 前进
    /// → confirmed（判据词 `guard_cleared+revision_advanced`；确认窗内绝不重发）。
    ///
    /// 长确认窗（2s）：不赌计时器，让确认走观察路径（T5/T6 同款纪律）。
    #[tokio::test]
    async fn stop_confirmation_confirms_via_pump_when_guard_cleared() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(interrupt_baseline(Some(vec!["work-1".to_owned()])));
        let (hub, events, pump) = confirmation_hub_with_timeout(
            fake.clone(),
            ledger.clone(),
            policy_with(&["interrupt"]),
            Duration::from_secs(2),
        )
        .await;
        let generation = hub.connection().generation;

        let receipt = hub
            .execute(ctx("stop-confirm-1"), stop_action(generation, "work-1"))
            .await
            .expect("stop 应被受理");
        assert_eq!(receipt.state, ActionState::Accepted);

        // 新投影：revision 前进 + 目标守卫已从 activeWorks 消失（显式空数组）。
        pump.push_outcome(
            Ok(()),
            vec![changed_view_with_control("sess_synthetic0000", 11, vec![])],
        );
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 1})))
            .await
            .unwrap();

        wait_until_with_sleep(|| {
            ledger
                .get(&confirmation_key())
                .first()
                .is_some_and(|row| row.state == CapabilityState::Confirmed)
        })
        .await;

        let rows = ledger.get(&confirmation_key());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].capability, Capability::Interrupt);
        assert_eq!(rows[0].state, CapabilityState::Confirmed);
        // 判据词经真实 T2 硬闸落库（DbCapabilityLedger 的受限校验在场）。
        assert_eq!(
            rows[0].evidence.as_deref(),
            Some("guard_cleared+revision_advanced")
        );
        assert_eq!(rows[0].reason_code, None);
        // 结构性不重发：确认窗内只派发过一次动作。
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
    }

    /// 初始 control 未宣告 / 目标守卫不在场 → 无守卫可等待：不登记（无确认窗），
    /// 随后哪怕投影 revision 前进 + 守卫消失（判据形态全满足）也绝不确认。
    #[tokio::test]
    async fn stop_without_target_guard_in_initial_control_does_not_register() {
        for (case, active_works) in [
            ("control 未宣告", None),
            ("目标守卫不在场", Some(vec!["work-other".to_owned()])),
        ] {
            let store = stored(URL);
            let ledger: Arc<dyn CapabilityLedger> =
                Arc::new(DbCapabilityLedger::new(store.clone()));
            let fake = Arc::new(FakeBackend::default());
            fake.set_baseline(interrupt_baseline(active_works));
            let (hub, events, pump) = confirmation_hub_with_timeout(
                fake.clone(),
                ledger.clone(),
                policy_with(&["interrupt"]),
                Duration::from_secs(2),
            )
            .await;
            let generation = hub.connection().generation;

            let receipt = hub
                .execute(ctx("stop-no-guard-1"), stop_action(generation, "work-1"))
                .await
                .expect("stop 应被受理");
            assert_eq!(receipt.state, ActionState::Accepted);

            // 即便随后的投影满足判据形态（revision 前进 + 显式空 activeWorks），
            // 没登记过就没有确认窗：绝不确认。
            pump.push_outcome(
                Ok(()),
                vec![changed_view_with_control("sess_synthetic0000", 11, vec![])],
            );
            events
                .send(RelayEvent::Data(serde_json::json!({"seq": 2})))
                .await
                .unwrap();
            // 先证明帧确实进了桥（观察已发生），负断言才有意义。
            wait_until(|| !pump.data.lock().unwrap().is_empty()).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(
                ledger.get(&confirmation_key()).is_empty(),
                "{case}：无守卫不登记，绝不确认"
            );
            // 结构性不重发：只派发过一次动作。
            assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
        }
    }

    /// 目标守卫仍在场（哪怕 revision 前进、别的守卫消失了）：绝不确认——不把
    /// 其它回合的停止当目标打断成功；保持候选，等后续帧或到点结算（不重发）。
    #[tokio::test]
    async fn stop_confirmation_keeps_candidate_while_target_guard_still_active() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(interrupt_baseline(Some(vec![
            "work-1".to_owned(),
            "work-2".to_owned(),
        ])));
        let (hub, events, pump) = confirmation_hub_with_timeout(
            fake.clone(),
            ledger.clone(),
            policy_with(&["interrupt"]),
            Duration::from_secs(2),
        )
        .await;
        let generation = hub.connection().generation;

        hub.execute(ctx("stop-present-1"), stop_action(generation, "work-1"))
            .await
            .expect("stop 应被受理");

        // 新投影：revision 前进，别的守卫（work-2）消失，但目标守卫（work-1）仍在。
        pump.push_outcome(
            Ok(()),
            vec![changed_view_with_control(
                "sess_synthetic0000",
                11,
                vec!["work-1"],
            )],
        );
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 3})))
            .await
            .unwrap();
        // 先证明帧确实进了桥（观察已发生），负断言才有意义。
        wait_until(|| !pump.data.lock().unwrap().is_empty()).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            ledger.get(&confirmation_key()).is_empty(),
            "目标守卫在场绝不确认（保持候选，零落账）"
        );
        // 结构性不重发：确认窗内只派发过一次动作。
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
    }

    /// 回执因果终态（CommandSender 的 wait_stop_cleared 确证停止终态后升级
    /// confirmed）→ interrupt 直接 confirmed，判据词如实只写守卫消失
    /// （`guard_cleared`；会话 revision 前进由泵判据负责，绝不冒写）。
    #[tokio::test]
    async fn stop_receipt_terminal_confirms_interrupt() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(interrupt_baseline(Some(vec!["work-1".to_owned()])));
        fake.set_execute_receipt(ActionState::Confirmed, None);
        let (hub, _events, _pump) =
            confirmation_hub(fake.clone(), ledger.clone(), policy_with(&["interrupt"])).await;
        let generation = hub.connection().generation;

        let receipt = hub
            .execute(ctx("stop-terminal-1"), stop_action(generation, "work-1"))
            .await
            .expect("回执带回显，execute 返回 Ok");
        assert_eq!(receipt.state, ActionState::Confirmed);

        let rows = ledger.get(&confirmation_key());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].capability, Capability::Interrupt);
        assert_eq!(rows[0].state, CapabilityState::Confirmed);
        assert_eq!(rows[0].evidence.as_deref(), Some("guard_cleared"));
        assert_eq!(rows[0].reason_code, None);
    }

    /// 回执结果未知（写后 ACK 超窗）→ inconclusive（保持候选，不重发）。
    #[tokio::test]
    async fn stop_receipt_unknown_settles_inconclusive_without_resend() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(interrupt_baseline(Some(vec!["work-1".to_owned()])));
        fake.set_execute_receipt(ActionState::Unknown, None);
        let (hub, _events, _pump) =
            confirmation_hub(fake.clone(), ledger.clone(), policy_with(&["interrupt"])).await;
        let generation = hub.connection().generation;

        let receipt = hub
            .execute(ctx("stop-unknown-1"), stop_action(generation, "work-1"))
            .await
            .expect("回执带回显，execute 返回 Ok");
        assert_eq!(receipt.state, ActionState::Unknown);

        let rows = ledger.get(&confirmation_key());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].capability, Capability::Interrupt);
        assert_eq!(rows[0].state, CapabilityState::Inconclusive);
        assert_eq!(rows[0].evidence.as_deref(), Some("unknown_receipt"));
        // 结构性不重发：只派发过一次动作。
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
    }

    /// ACK-only 不升级：stop 被 accepted 后确认窗内无新投影证据 → 账本不写
    /// confirmed；短窗到期由计时器写 inconclusive（保持候选，绝不重发）。
    #[tokio::test]
    async fn ack_only_stop_times_out_inconclusive_without_resend() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(interrupt_baseline(Some(vec!["work-1".to_owned()])));
        let (hub, _events, _pump) =
            confirmation_hub(fake.clone(), ledger.clone(), policy_with(&["interrupt"])).await;
        let generation = hub.connection().generation;

        let receipt = hub
            .execute(ctx("stop-ack-1"), stop_action(generation, "work-1"))
            .await
            .expect("stop 应被受理");
        assert_eq!(receipt.state, ActionState::Accepted);

        // 不喂任何帧：确认窗到点 → inconclusive。
        wait_until_with_sleep(|| !ledger.get(&confirmation_key()).is_empty()).await;
        let rows = ledger.get(&confirmation_key());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].capability, Capability::Interrupt);
        assert_eq!(rows[0].state, CapabilityState::Inconclusive);
        assert_eq!(rows[0].evidence.as_deref(), Some("timeout_5s"));
        // 结构性不重发：确认窗内只派发过一次动作。
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
    }

    /// 到点未决的 Interrupt 登记：最后观察里目标守卫仍在场 → 判据词如实记
    /// `guard_still_active`；守卫已不在的到点登记沿用一般超窗词（直调 tracker，
    /// 与计时器醒来的 sweep 语义一致——到点即 inconclusive，绝不重判）。
    #[tokio::test]
    async fn expired_interrupt_observation_reports_guard_still_active() {
        let store = stored(URL);
        let ledger: Arc<dyn CapabilityLedger> = Arc::new(DbCapabilityLedger::new(store.clone()));
        let fake = Arc::new(FakeBackend::default());
        fake.set_baseline(interrupt_baseline(Some(vec!["work-1".to_owned()])));
        let (hub, _events, _pump) =
            confirmation_hub(fake.clone(), ledger.clone(), policy_with(&["interrupt"])).await;
        let generation = hub.connection().generation;

        // 到点登记 #1：观察帧里目标守卫仍在场。
        assert!(hub.confirmations.register(ConfirmationContext {
            capability: Capability::Interrupt,
            key: confirmation_key(),
            session_id: "sess_synthetic0000".to_owned(),
            generation,
            baseline_revision: 10,
            target: ConfirmationTarget::Interrupt {
                guard_id: "work-1".to_owned(),
            },
            deadline: Instant::now(),
        }));
        hub.confirmations.observe_session(
            &changed_view_with_control("sess_synthetic0000", 11, vec!["work-1"]),
            generation,
            None,
        );
        let rows = ledger.get(&confirmation_key());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, CapabilityState::Inconclusive);
        assert_eq!(rows[0].evidence.as_deref(), Some("guard_still_active"));

        // 到点登记 #2（同槽位已被摘除，可再登记）：观察帧里目标守卫已不在。
        assert!(hub.confirmations.register(ConfirmationContext {
            capability: Capability::Interrupt,
            key: confirmation_key(),
            session_id: "sess_synthetic0000".to_owned(),
            generation,
            baseline_revision: 11,
            target: ConfirmationTarget::Interrupt {
                guard_id: "work-1".to_owned(),
            },
            deadline: Instant::now(),
        }));
        hub.confirmations.observe_session(
            &changed_view_with_control("sess_synthetic0000", 12, vec!["work-9"]),
            generation,
            None,
        );
        let rows = ledger.get(&confirmation_key());
        assert_eq!(rows.len(), 1, "同 key 同能力 upsert 覆盖");
        assert_eq!(rows[0].state, CapabilityState::Inconclusive);
        assert_eq!(rows[0].evidence.as_deref(), Some("timeout_5s"));
    }
}
