// OpenCode 原生动作存储（计划任务 5）：把动作账本 DAO 接到 [`DeviceStore`] 注入缝上。
//
// 一次原生写的完整次序（任务 7 按此实现，本层只保证持久化语义）：
//
//   reserve（事务：动作行 + 占用）→ 读能力/pending 复核 → claim（CAS：可能已发出）
//   → 【DB 锁外】唯一一次网络写 → finish（终态）
//
// 任何一步的 DB 访问都在 `DeviceStore::with` 的同步闭包里完成，闭包不返回连接引用，
// 网络绝不在持锁期间发生。本模块不含任何 HTTP 代码。
//
// 任务 7 在本文件追加派发核心（`NativeHub::execute/action/verify`），把上面的次序落成
// 可执行代码，并守住三条硬规则：
//
// * **一次派发，绝不重放**：`reserve` 同指纹即回既有动作；`claim` 之后无论崩溃、超时还是
//   丢响应都不会有第二次网络写——未知结果只由只读核对（`verify`）收敛。
// * **只有权威回读能证明的结论才落账**：`Confirmed` 只认精确原生 message id 出现在消息
//   列表；`Settled` 必须发生在读到终态之后；`NotAccepted` 必须带「整份未生效」证据词。
// * **零写出的拒绝路径**：斜杠命令、越界附件、空/超长正文、陈旧令牌、未放行能力、不归属
//   路由——全部在唯一一次网络写之前拒绝，并以墓碑（`finish(NotSent, …, Some(理由))`）
//   证明「从未派发」。
use std::path::Path;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::database::dao::opencode_actions::{self as dao, FinishDao, ReserveDao};
use crate::inject::normalize::{
    compose_injection_flagged, is_slash_message, message_signature_enabled_conn,
};
use crate::remote::api::MAX_SEND_CHARS;
use crate::remote::attachments::attachment_dir;
use crate::remote::pairing::DeviceStore;
use crate::session::{AgentType, ProcessForm, Session, SessionStatus};

use super::client::NativeWriteReply;
use super::forms::{same_form_context_for, validate_answers};
use super::projection::{
    read_form_detail, reverify_write_channel_identity, segment, DispatchSnapshot, NativeHub, Source,
};
use super::types::{
    codes, ActionState, ActionView, DeliveryKind, DraftAnswer, NativeActor, NativeCommand,
    NativeError, NativeOperation, NativeOperationId, NativeTarget, OpenCodeActionListEntry,
    PendingView, RuntimeKey,
};

/// 动作账本错误码（受控词表；HTTP/TS 映射在任务 8 引用本处常量，不另抄字面量）。
pub mod action_codes {
    /// 同 actionId 异载荷，或同一待答表单/审批已有活占用（HTTP 409）。
    pub const CONFLICT: &str = "OC_ACTION_CONFLICT";
    /// 动作不存在或不属于本设备（不做存在性预言机，HTTP 404）。
    pub const NOT_FOUND: &str = "OC_ACTION_NOT_FOUND";
    /// 非法状态转移（终态改写、未取得写出权就落账等，HTTP 409）。
    pub const STATE: &str = "OC_ACTION_STATE";
    /// 账本读写失败（不透出 SQL 文本）。
    pub const STORE_FAILED: &str = "OC_STORE_FAILED";
}

/// `NotAccepted` 的必要证据词：调用方已证明**整份请求未生效**（任务 0 金样规则）。
/// 没有这份证据时只能记 `Unconfirmed`，占用不释放。
pub const NOT_ACCEPTED_EVIDENCE: &str = dao::NOT_ACCEPTED_EVIDENCE;

/// 放弃理由词（R1 的 `ABANDON_*` 常量）：`finish(NotSent, …, Some(reason))` 的碑文。
///
/// 每个词都证明同一件事——**本动作在派发前被拒，从未发生任何网络写**。没有理由的放弃
/// 会被 DAO 拒绝（无碑不算放弃），行会永远既「可 claim」又「算未决」。
pub mod abandon_reasons {
    /// 绑定令牌或上下文令牌与当前投影不符（换代际/换会话/换目录/表单内容变化）。
    pub const BINDING_STALE: &str = "binding_stale";
    /// 契约未放行该操作（缺路径/形状/决定词/关联 id 能力）。
    pub const CAPABILITY: &str = "capability_missing";
    /// 本端不拥有该会话的原生写入入口（归属未选中或已交出）。
    pub const ROUTE_NOT_OWNED: &str = "route_not_owned";
    /// 待答表单/审批在派发前已定案（只说已定案，不声称哪端获胜）。
    pub const REQUEST_SETTLED: &str = "request_settled";
    /// 整组答案未通过提交期校验（不可答/缺必答/与当前字段不符）。
    pub const ANSWER_INVALID: &str = "answer_invalid";
    /// 附件不在受控落盘目录内（或无法归一/不是文件）。
    pub const ATTACHMENT_REFUSED: &str = "attachment_refused";
    /// 其它派发前复核失败（服务未登记、投影读取失败等）。
    pub const PRECHECK: &str = "precheck_failed";
    /// R2 服务身份核验失败（死 pid / 换代 / 冒名 info / 多 service 冲突 / 凭据缺失）：
    /// 证据链不成立，动作从未取得写出权。对外错误码仍携带具体受控码，词表只归类。
    pub const IDENTITY_UNVERIFIED: &str = "identity_unverified";
    /// **echo-challenge 归属写门拒绝**（H03）：探针回声挑战未在窗口内被锁定 TUI
    /// 以正确 pid 应答（或探针通道/服务不可用）——归属未证，零写出零预留。
    /// 对外错误码仍携带具体 `OC_GATE_*` 受控码，词表只归类。
    pub const BINDING_UNVERIFIED: &str = "binding_unverified";
    /// **代际已不可核对**（评审 Blocker 2）：服务换代后该行再也无法由权威回读收敛，
    /// 只能由用户在 `POST /opencode/route/legacy` 里显式 `force` 作废。它证明的是
    /// 「本端再也证明不了结果」，**不是**「未写出」——墓碑上写的正是这一点。
    pub const UNVERIFIABLE: &str = crate::database::dao::opencode_actions::UNVERIFIABLE_GENERATION;
    /// **窗口 CAS 拒绝**（G1/H1）：回声验证锁定的窗口身份/路由版本在队列变更或
    /// 派发事务复核时已不全等（同服务显式重选换了窗口）——新项不入队/原项不撤回/
    /// 恢复被拒，零写出。对外错误码仍携带 `OC_ROUTE_WINDOW_MISMATCH`，词表供
    /// 墓碑审计归类（拒绝原因留存，直到用户显式重选/重新确认）。
    pub const ROUTE_WINDOW_MISMATCH: &str = "route_window_mismatch";
    /// **统一端到端预算耗尽**（H5/I2/fix1）：动作链超出统一预算
    ///（`budget::ACTION_E2E_BUDGET_MS`，覆盖服务端动作链接收 → 正式回执全链）——
    /// 三层防御（claim 事务入口闸 / 写权后提交前复核 / 发包前复核）。前两层在授权
    /// 写出提交**之前**：确定零写出（写只发生在 claim 之后），墓碑 = 可核对的
    /// 「确定未发送」结论，**此后不得迟到取得派发权**。第三层（fix1）在 claim
    /// 提交完成后、原生请求开始前：跨截止 ⇒ 不发原生请求；claim 提交后账本边界
    /// 按设计不可判定（「可能已发出」已成账），落 Unknown 族 `unconfirmed`
    ///（reason=错误码本码，只按原 action ID 核对），绝不冒充「确定未发送」。
    /// 对外错误码仍携带 `OC_DISPATCH_BUDGET_EXHAUSTED`。
    pub const BUDGET_EXHAUSTED: &str = "budget_exhausted";
}

/// 结果理由词（落进 `ActionView.reason_code` 的受控词表；任务 9/10 按词映射文案）。
pub mod reason_words {
    /// 写被服务端拒绝（非 2xx）：未证明生效，也未证明未生效。
    pub const WRITE_REFUSED: &str = "write_refused";
    /// 回读找不到关联 id：既不能确认投递，也不能声称未生效。
    pub const ID_ABSENT: &str = "native_id_absent";
    /// 回读失败或自相矛盾：无法用权威回读定性。
    pub const READBACK_FAILED: &str = "readback_failed";
    /// 读到当前状态（空闲/目标 agent），但没有本次写回执，不能归因给本动作。
    pub const RESULT_UNPROVEN: &str = "result_unproven";
    /// 请求已定案（409/派发前复核）：只说已定案，不声称哪端获胜。
    pub const ALREADY_SETTLED: &str = "already_settled";
    /// 撤回前该项已被原生队列投递：撤回未生效，但执行也未被取消。
    pub const DELIVERED_BEFORE_RETRACT: &str = "delivered_before_retract";
    /// **插队（steer）已派发**：插队动作的唯一合法改动是把投递方式从 queue 改成
    /// steer（OpenCode 原生 steer 自带打断当前回合），派发的是原队列行的正文与
    /// 关联 id。回执带本词 = 「这条消息是插队投的，会打断当时正在跑的回合」的
    /// 可判定信号（本端不额外声称回合实际被停在哪一刻）。
    pub const STEERED_INTERRUPT: &str = "steered_interrupt";
}

/// [`NativeActionStore::reserve`] 的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveOutcome {
    /// 本次登记成功：动作处于 `not_sent`，尚不可派发
    New,
    /// 同 actionId 同指纹：既有动作（绝不第二次写；终态动作也在此原样返回）
    Existing(ActionView),
    /// 同 actionId 异载荷，或该未决请求已被别的动作占用
    Conflict,
}

/// 进程代次（每进程生成一次，`rand` 16 字节 hex）：`claim` 把它写进动作行，重启恢复
/// 只收敛**代次不同**的在飞行。不落盘到别处、不进 DTO、不参与日志。
fn process_epoch() -> &'static str {
    static EPOCH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    EPOCH.get_or_init(|| {
        let mut bytes = [0u8; 16];
        rand::fill(&mut bytes);
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    })
}

/// 原生动作账本（唯一写出权的持久事实源）。
///
/// 与 `NativeHub` 共用同一个 [`DeviceStore`]：一个进程构造一次——构造即崩溃恢复点
/// （上一次进程遗留的 `dispatching` → `unconfirmed`，绝不重发）。
pub struct NativeActionStore {
    store: DeviceStore,
    epoch: String,
}

impl NativeActionStore {
    /// 生产入口：用本进程代次构造。恢复只处理**上一次进程**留下的 `dispatching`
    /// （`dispatch_epoch` 不等于本代次，含 NULL），同进程内重复构造不会篡改在飞动作。
    /// 恢复失败只记录日志（随后 `claim` 自然不会给权，零派发）。
    pub fn new(store: DeviceStore) -> Self {
        Self::with_epoch(store, process_epoch())
    }

    /// 注入代次构造（正式 API，非 `cfg(test)`）：换一个代次即等价于「模拟一次进程重启」，
    /// 供集成测试与多进程场景使用；生产侧仍应走 [`Self::new`]。
    pub fn with_epoch(store: DeviceStore, epoch: &str) -> Self {
        let now = now_ms();
        store.with(|conn| {
            if let Err(e) = dao::recover_dispatching_conn(conn, epoch, now) {
                log::error!("opencode native action recovery failed: {e}");
            }
        });
        Self {
            store,
            epoch: epoch.to_string(),
        }
    }

    /// 登记动作（一个事务内完成「动作行 + 未决请求占用」）。
    ///
    /// 同 actionId 同指纹 → `Existing`（绝不第二次写）；同 actionId 异载荷 → `Conflict`；
    /// 同一待答表单/审批已有活占用 → `Conflict`（整事务回滚，零落地）。
    pub fn reserve(
        &self,
        device_id: &str,
        target: &NativeTarget,
        command: &NativeCommand,
    ) -> Result<ReserveOutcome, NativeError> {
        if command.session_id != target.session_id {
            return Err(NativeError::new(
                codes::OC_SESSION_MISMATCH,
                "native command session does not match the bound target",
            ));
        }
        let fingerprint = payload_sha256(command);
        let occupancy = occupancy_key_of(&target.session_id, &command.operation);
        let new = dao::NewAction {
            action_id: &command.action_id,
            device_id,
            target,
            operation: operation_word(&command.operation),
            request_key: request_key_of(&command.operation),
            payload_sha256: &fingerprint,
            occupancy_key: occupancy.as_deref(),
        };
        let now = now_ms();
        let outcome = self
            .store
            .with(|conn| dao::reserve_conn(conn, &new, now).map_err(store_error))?;
        Ok(match outcome {
            ReserveDao::New => ReserveOutcome::New,
            ReserveDao::Existing(row) => ReserveOutcome::Existing(view_of(&row)),
            ReserveDao::Hidden => {
                return Err(NativeError::new(
                    action_codes::NOT_FOUND,
                    "native action not found",
                ))
            }
            ReserveDao::Conflict => ReserveOutcome::Conflict,
        })
    }

    /// 取得写出权（`not_sent`/`queued` → `dispatching`，同事务复核暂停/FIFO 并清待发载荷）。
    ///
    /// 返回 `true` 之后才允许发生**唯一一次**网络写：`dispatching` 已落库（含本进程代次），
    /// 崩溃或超时都不会再有第二次派发机会。返回 `false` = 不给权（正在派发/已终态/
    /// 不存在/占用已放弃/**已被 `finish(NotSent)` 放弃**）。
    pub fn claim(&self, action_id: &str) -> Result<bool, NativeError> {
        let now = now_ms();
        let epoch = self.epoch.as_str();
        self.store
            .with(|conn| dao::claim_action_conn(conn, action_id, epoch, now).map_err(store_error))
    }

    /// 落账结论（同事务结算占用）。占用只在 `NotSent`（从未派发的放弃，且写入墓碑）与
    /// `NotAccepted`（必须带 [`NOT_ACCEPTED_EVIDENCE`]）时释放；其余结论保留占用。
    /// `Unconfirmed → Unconfirmed` 只允许**带理由**刷新（状态不变、占用不动，供只读核对
    /// 记录最新的 `readback_failed` 类理由）。
    ///
    /// `reason` 是「整份请求未生效」的唯一载体（计划固定了本签名）：传
    /// [`NOT_ACCEPTED_EVIDENCE`] 却没有权威回读证据，等于伪造终态；任务 7 只允许在
    /// 金样式 400 证明后使用。
    pub fn finish(
        &self,
        action_id: &str,
        state: ActionState,
        native_id: Option<&str>,
        reason: Option<&str>,
    ) -> Result<(), NativeError> {
        let now = now_ms();
        let outcome = self.store.with(|conn| {
            dao::finish_action_conn(conn, action_id, state, native_id, reason, now)
                .map_err(store_error)
        })?;
        match outcome {
            FinishDao::Applied => Ok(()),
            FinishDao::NotFound => Err(NativeError::new(
                action_codes::NOT_FOUND,
                "native action not found",
            )),
            FinishDao::Illegal => Err(NativeError::new(
                action_codes::STATE,
                "native action state transition is not allowed",
            )),
        }
    }

    /// 读动作（只给属主设备；别人的动作与不存在的动作同码，不给存在性预言机）。
    pub fn read(&self, device_id: &str, action_id: &str) -> Result<ActionView, NativeError> {
        self.store.with(|conn| {
            if let Some(row) = dao::get_action_conn(conn, action_id).map_err(store_error)? {
                if row.device_id == device_id {
                    return Ok(view_of(&row));
                }
            } else if let Some(row) = dao::rejection_conn(conn, action_id).map_err(store_error)? {
                if row.device_id == device_id {
                    return Ok(ActionView {
                        action_id: action_id.to_owned(),
                        state: ActionState::NotSent,
                        reason_code: Some(row.reason_code),
                        native_id: None,
                        runtime_dead: None,
                    });
                }
            }
            Err(NativeError::new(
                action_codes::NOT_FOUND,
                "native action not found",
            ))
        })
    }

    /// 读整行（只给属主设备）：`verify` 需要会话/目录/操作/原请求键来组织权威回读
    /// （账本不存正文与答案，回读只认这些元数据 + 派生命运的关联 id）。
    pub(crate) fn locate(
        &self,
        device_id: &str,
        action_id: &str,
    ) -> Result<dao::ActionRow, NativeError> {
        let row = self
            .store
            .with(|conn| dao::get_action_conn(conn, action_id).map_err(store_error))?;
        match row {
            Some(row) if row.device_id == device_id => Ok(row),
            _ => Err(NativeError::new(
                action_codes::NOT_FOUND,
                "native action not found",
            )),
        }
    }

    /// 本端是否拥有该会话的原生写入入口（路由 `active`；`draining` 不算）。
    pub fn owns_session(&self, session_id: &str) -> Result<bool, NativeError> {
        let route = self
            .store
            .with(|conn| dao::route_conn(conn, session_id).map_err(store_error))?;
        Ok(route.is_some_and(|route| route.route_state == dao::ROUTE_ACTIVE))
    }

    /// 认领会话的原生路由。返回 `true` 需要**同时**证明：旧 `inject_queue` 该会话已无
    /// 未决行、调用方断言 `legacy_quiescent`、且（换绑代际时）该会话没有未决 native 动作。
    /// 任一不能证明就返回 `false`：不迁移未知旧动作、不凭年龄猜测、不夺取别人的归属。
    pub fn try_claim_route(
        &self,
        target: &NativeTarget,
        legacy_quiescent: bool,
    ) -> Result<bool, NativeError> {
        self.try_claim_route_with(target, legacy_quiescent, false)
    }

    /// 认领的完整形态：`allow_reactivate` = 是否允许把 `draining` 拉回 `active`。
    ///
    /// * **隐式**入口（`GET /opencode/view` 的认领臂）恒传 `false`——退回旧路径是用户的
    ///   显式决定，一次刷新/换端不得静默撤销它（评审 R-2）；
    /// * **显式**入口（`POST /opencode/route/native`）传 `true`，语义是用户刚刚点了
    ///   「重新进入原生交互」。
    pub fn try_claim_route_with(
        &self,
        target: &NativeTarget,
        legacy_quiescent: bool,
        allow_reactivate: bool,
    ) -> Result<bool, NativeError> {
        let now = now_ms();
        self.store.with(|conn| {
            dao::claim_route_conn(conn, target, legacy_quiescent, now, allow_reactivate)
                .map_err(store_error)
        })
    }

    /// 路由状态词（`None` = 从未接入原生；`active` / `draining`）。
    pub fn route_state(&self, session_id: &str) -> Result<Option<String>, NativeError> {
        self.store
            .with(|conn| dao::route_state_conn(conn, session_id).map_err(store_error))
    }

    /// 该会话中「代际已不可核对」的动作条数（强制交回的预读；零副作用）。
    ///
    /// 只读：不写账本、不改状态。用于拒绝回执里的 `unverifiableActions` 与前端二次确认
    /// 文案——用户必须先知道「有多少条会被作废」。
    pub fn unverifiable_count(
        &self,
        session_id: &str,
        current: Option<&RuntimeKey>,
    ) -> Result<usize, NativeError> {
        Ok(self.unverifiable_ids(session_id, current)?.len())
    }

    /// 预读用：同 [`Self::unverifiable_ids`]，再排除「本进程真实在飞」的 id
    /// （force 实际也排除它们，两处必须同口径）。
    pub fn unverifiable_count_excluding(
        &self,
        session_id: &str,
        current: Option<&RuntimeKey>,
        in_flight: &[String],
    ) -> Result<usize, NativeError> {
        Ok(self
            .unverifiable_ids(session_id, current)?
            .into_iter()
            .filter(|id| !in_flight.iter().any(|active| active == id))
            .count())
    }

    /// 作废候选集：`Some(当前代际)` ⇒ 只取「存储代际 ≠ 当前代际」的未决行；
    /// `None` ⇒ 当前代际不可证，**该会话全部未决行都算**（评审 R-1）。SQL 只排除
    /// 本进程在飞的 `dispatching` 行（`dispatch_epoch` = 本代次）；真实在飞的
    /// future（状态可能已被 verify 改写）由调用方用 `in_flight` 二次排除（G7）。
    fn unverifiable_ids(
        &self,
        session_id: &str,
        current: Option<&RuntimeKey>,
    ) -> Result<Vec<String>, NativeError> {
        self.store.with(|conn| match current {
            Some(runtime) => {
                dao::unverifiable_for_session_conn(conn, session_id, runtime, &self.epoch)
                    .map_err(store_error)
            }
            None => dao::unresolved_ids_for_session_conn(conn, session_id, &self.epoch)
                .map_err(store_error),
        })
    }

    /// **显式作废**「代际已不可核对」的动作（评审 Blocker 2；只由用户在 route/legacy 里
    /// `force` 触发，绝不自动执行）。
    ///
    /// 对每条候选调既有 [`NativeActionStore::finish`]（`NotSent` + 专用碑文
    /// [`dao::UNVERIFIABLE_GENERATION`]）：同事务释放该动作的待答占用、写下「已作废」墓碑、
    /// 此后永不重派（`not_sent` + 有碑 = 不可 claim 的墓碑）。同代际的行与本进程在飞的行
    /// 不会进入候选集，因此仍可核对的行一条都不会被碰。
    ///
    /// 逐条独立事务；强制交回走的是原子形态 [`NativeActionStore::abandon_and_return`]。
    /// 返回实际作废条数。任何一条落账失败即报错（不吞错：账本写不进去就不能声称已作废）。
    pub fn abandon_unverifiable(
        &self,
        session_id: &str,
        current: Option<&RuntimeKey>,
    ) -> Result<usize, NativeError> {
        let ids = self.unverifiable_ids(session_id, current)?;
        for id in &ids {
            self.finish(
                id,
                ActionState::NotSent,
                None,
                Some(dao::UNVERIFIABLE_GENERATION),
            )?;
        }
        Ok(ids.len())
    }

    /// **强制交回旧路径**（G7）：作废候选集与路由降级在**同一个事务**里完成。
    ///
    /// 返回 `(是否已交回, 作废条数)`：`false` ⇒ 仍有剩余未决、交回不成立，**零墓碑且
    /// 路由不变**（调用方回 409）；`true` ⇒ 候选全部成为不可 claim 的墓碑，路由降级为
    /// `draining`。
    pub fn abandon_and_return(
        &self,
        session_id: &str,
        current: Option<&RuntimeKey>,
        in_flight: &[String],
    ) -> Result<(bool, usize), NativeError> {
        self.abandon_and_return_with(session_id, current, || in_flight.to_vec())
    }

    /// 同 [`Self::abandon_and_return`]，但**在持有 DB 连接的同一临界区内**取在飞快照：
    /// 调用方传 `NativeHub::active_in_flight`，保证「读在飞集合」与「事务内候选筛选」
    /// 之间没有可插入 claim 的窗口（G7 的 TOCTOU 加固）。
    ///
    /// `in_flight` **绝不可**再次进入 `DeviceStore::with`（同一把锁重入即死锁）；它只应
    /// 读取与账本无关的内存快照。
    pub(crate) fn abandon_and_return_with<F>(
        &self,
        session_id: &str,
        current: Option<&RuntimeKey>,
        in_flight: F,
    ) -> Result<(bool, usize), NativeError>
    where
        F: FnOnce() -> Vec<String>,
    {
        let now = now_ms();
        self.store.with(|conn| {
            let in_flight = in_flight();
            dao::abandon_and_return_conn(conn, session_id, current, &self.epoch, &in_flight, now)
                .map_err(store_error)
        })
    }

    /// 验证开关关闭时交出写入入口：仍有未决 native 动作则拒绝，路由行只降级不删除。
    pub fn disengage_route(&self, session_id: &str) -> Result<bool, NativeError> {
        let now = now_ms();
        self.store
            .with(|conn| dao::disengage_route_conn(conn, session_id, now).map_err(store_error))
    }
}

// ============================================================
// 任务 7：派发核心（execute / action / verify）
// ============================================================

/// 静态完整性校验的产物：send 的原始附件路径。
///
/// 附件「在不在受控目录」要等投影给出 `target.directory` 才能判定（见
/// [`verified_attachments`]），故静态期只收形态——绝对路径。
struct Intent {
    attachment_paths: Vec<String>,
}

/// 一次已验证附件：`file://` 绝对 URI + 文件名（任务 0：裸本机路径 400）。
struct Attachment {
    uri: String,
    name: String,
}

/// 派发计划：复核通过后，写阶段只按它拼装请求体（不回头再看命令）。
enum Plan {
    LocalOutbox,
    Send {
        text: String,
        files: Vec<Attachment>,
        delivery: DeliveryKind,
        /// 实际出网的关联 id（body.`id`）。普通发送/重绑 = `msg_<本动作 id>`；
        /// **插队** = 原队列行的 `msg_<targetActionId>`（Task C：派发的是既有那一行，
        /// 不新造 id）。写阶段与回读定性共用本字段，二者不可能漂移。
        client_message_id: String,
        /// 插队的目标队列行（`Some` = 本次派发是插队）：写成功后终态落在**那一行**
        /// 自己身上，本控制动作收敛为 `settled` + `steered_interrupt`。
        prioritize_target: Option<String>,
    },
    /// 插队（原生 PATCH）：把既有 inbox queue 项改成 steer，由上游在当前回合交付。
    Promote {
        item_id: String,
    },
    Answer {
        form_id: String,
        answer: Value,
    },
    Decide {
        request_id: String,
        decision: String,
        /// `always` 必须回读的已保存规则（action + resources）；once/reject 为 None。
        saved_rule: Option<(String, Vec<String>)>,
    },
    Retract {
        item_id: String,
    },
    Stop,
    SetAgent {
        agent: String,
    },
}

/// 一次回读定性（终态 + 原生 id + 理由词）。
struct Outcome {
    state: ActionState,
    native_id: Option<String>,
    reason: Option<String>,
}

impl Outcome {
    fn terminal(state: ActionState, native_id: Option<String>, reason: Option<&str>) -> Self {
        Self {
            state,
            native_id,
            reason: reason.map(str::to_string),
        }
    }

    fn unconfirmed(reason: Option<String>) -> Self {
        Self {
            state: ActionState::Unconfirmed,
            native_id: None,
            reason,
        }
    }
}

impl NativeHub {
    /// 从持久 active 路由恢复原会话寻址上下文，不创建归属也不更换服务代际。
    pub(crate) fn restore_active_target(
        &self,
        session_id: &str,
    ) -> Result<Option<(Session, NativeTarget)>, NativeError> {
        self.store().with(|conn| {
            let Some(route) = dao::route_conn(conn, session_id).map_err(store_error)? else {
                return Ok(None);
            };
            if route.route_state != dao::ROUTE_ACTIVE {
                return Ok(None);
            }
            let directory = std::path::PathBuf::from(&route.directory);
            if !directory.is_absolute()
                || directory.components().any(|part| {
                    matches!(
                        part,
                        std::path::Component::ParentDir | std::path::Component::CurDir
                    )
                })
            {
                return Err(action_invalid());
            }
            let target = NativeTarget {
                runtime: RuntimeKey {
                    service_id: route.service_id,
                    pid: u32::try_from(route.runtime_pid).map_err(|_| action_invalid())?,
                    started_at_ms: route.runtime_started_at_ms,
                },
                session_id: session_id.to_owned(),
                directory,
            };
            Ok(Some((session_card(session_id, &route.directory), target)))
        })
    }

    /// 在解析当前会话之前回读已有动作，避免目录或服务失效遮蔽未知结果。
    pub(crate) fn existing_command_outcome(
        &self,
        device_id: &str,
        command: &NativeCommand,
    ) -> Result<Option<ActionView>, NativeError> {
        let _ = intent_of(command)?;
        self.store().with(|conn| {
            if !crate::remote::pairing::device_valid(conn, device_id, now_ms()) {
                return Err(NativeError::new(
                    action_codes::NOT_FOUND,
                    "native action not found",
                ));
            }
            if let Some(row) =
                dao::get_action_conn(conn, &command.action_id).map_err(store_error)?
            {
                if row.device_id != device_id {
                    return Err(NativeError::new(
                        action_codes::NOT_FOUND,
                        "native action not found",
                    ));
                }
                if row.session_id != command.session_id
                    || row.payload_sha256 != payload_sha256(command)
                {
                    return Err(NativeError::new(
                        action_codes::CONFLICT,
                        "native action conflicts with a live one",
                    ));
                }
                return if row.state == ActionState::NotSent && row.reason_code.is_none() {
                    Ok(None)
                } else {
                    Ok(Some(view_of(&row)))
                };
            }
            if let Some(row) = dao::rejection_conn(conn, &command.action_id).map_err(store_error)? {
                if row.device_id != device_id {
                    return Err(NativeError::new(
                        action_codes::NOT_FOUND,
                        "native action not found",
                    ));
                }
                if row.session_id != command.session_id
                    || row.payload_sha256 != payload_sha256(command)
                {
                    return Err(NativeError::new(
                        action_codes::CONFLICT,
                        "native action conflicts with a live one",
                    ));
                }
                return Ok(Some(ActionView {
                    action_id: command.action_id.clone(),
                    state: ActionState::NotSent,
                    reason_code: Some(row.reason_code),
                    native_id: None,
                    runtime_dead: None,
                }));
            }
            Ok(None)
        })
    }

    /// 派发一个原生命令：**一次派发，绝不重放**。
    ///
    /// 次序（任务 7 的安全核心）：
    ///
    /// 1. **静态完整性校验**（无 IO、无账本、无网络）：动作 id 形态、关联 id、投递词、
    ///    决定词、斜杠命令、正文空/超长、附件路径形态。任一不成立 → 受控错误，零写出。
    /// 2. **当前投影**（网络，DB 锁外）：一次只读重建，给出绑定/能力/pending/inbox/带
    ///    原生 id 的消息记录。投影失败 ⇒ 零写出（拿不到当前事实就不写）。
    /// 3. **reserve**（账本事务）：同 actionId 同指纹 → 既有动作；该行若**从未进入过派发**
    ///    （`not_sent` 且无放弃理由——例如上次尝试在 claim 前被取消）则继续走派发路径，
    ///    `claim` 的 CAS 仍保证最多一次写出；墓碑/在飞/终态一律原样返回。
    ///    同一待答表单/审批已有活占用 → `OC_ACTION_CONFLICT`。
    /// 4. **派发前最终复核**（绑定/代际/能力/归属/当前请求/附件）：任一不成立 → 以
    ///    [`abandon_reasons`] 记墓碑后返回错误（该动作从未派发）。
    /// 5. **claim**（CAS）：把「可能已发出」持久化——此后无论崩溃、取消还是超时都不会有
    ///    第二次派发机会。
    /// 6. **组装一次 + 唯一一次网络写**（DB 锁外）：签名设置经既有单点现读，正文按
    ///    [`compose_injection_flagged`] 组装（重试查询不重新组装）；附件以 `file://`
    ///    绝对 URI 提交。
    /// 7. **回读 + finish**：只有**执行这次写的代际**的权威回读能证明的结论才落账。
    pub async fn execute(
        &self,
        actor: &NativeActor,
        session: &Session,
        command: NativeCommand,
    ) -> Result<ActionView, NativeError> {
        self.execute_pinned(actor, session, command, None).await
    }

    /// 动作链统一入口（H5 薄壳）：在服务端动作链接收点创建分段计时记录器
    /// （统一预算的计时起点），全链结束后落一行五段计时 + 结构化日志。
    /// 计时起点 = 本函数入口（动作许可等待**之前**，排队段覆盖许可等待）。
    pub(crate) async fn execute_pinned(
        &self,
        actor: &NativeActor,
        session: &Session,
        command: NativeCommand,
        expected: Option<&NativeTarget>,
    ) -> Result<ActionView, NativeError> {
        let marker = super::budget::ActionTiming::start(
            &command.action_id,
            &command.session_id,
            operation_word(&command.operation),
        );
        let mut timing = marker.begin();
        let result = self
            .execute_pinned_inner(&mut timing, actor, session, command, expected)
            .await;
        timing.finish(self);
        result
    }

    async fn execute_pinned_inner(
        &self,
        timing: &mut super::budget::ActionTiming,
        actor: &NativeActor,
        session: &Session,
        command: NativeCommand,
        expected: Option<&NativeTarget>,
    ) -> Result<ActionView, NativeError> {
        // 纯本地安全操作（暂停本地派发 / 撤回尚未 claim 的本地项）不需要服务身份；
        // 它们不产生原生写。恢复派发、重绑和原生写仍受归属门。
        let local_only = match &command.operation {
            NativeOperation::SetOutboxPaused(p) => p.paused,
            NativeOperation::RetractOutbox(_) => true,
            _ => false,
        };
        // **探针目标隔离**（I4）：写门探针会话是受保护对象——普通发送/附件/答题/
        // 审批/停止/切模式一律零动作零写拒绝。双识别：进程内探针注册表 + 卡片标题
        // 协议形态（跨进程残迹兜底；卡片标题漂移的残余边界如实记录在报告）。
        if !local_only
            && (self.is_probe_target(&command.session_id)
                || super::binding_gate::looks_like_probe_title(session.title.as_deref()))
        {
            return Err(NativeError::new(
                codes::OC_GATE_PROBE_TARGET,
                "the target session is the binding gate probe",
            ));
        }
        let delivery_epoch = self
            .outbox_runtime()
            .epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        let intent = intent_of(&command)?;
        // 同一 ID 的新请求等待在途请求定案，再按账本状态回复，避免并发拒绝后又写出。
        // H5 分段计时：许可等待计入「排队」段（计时起点在 execute_pinned 入口）。
        let permit_wait_started = std::time::Instant::now();
        let _action_permit = self.action_permit(&command.action_id).lock_owned().await;
        timing.add_queue_ms(permit_wait_started.elapsed().as_millis() as i64);
        // **H5 预算检查点 ①（入口）**：许可等待后已超统一预算 ⇒ 尽快落确定拒绝
        // （零动作行 ⇒ rejections 墓碑；确认零写出，意图可重试），不继续烧验证链。
        if !local_only && timing.exceeded(self) {
            timing.mark_budget_refused();
            return resolve_entry_failure(
                self,
                actor,
                session,
                &command,
                super::budget::budget_exhausted_error(),
            );
        }
        // 已可能写出的动作必须先按原属主、目标与意图回读账本。服务失效不应把
        // 已有 Unknown 误报成新请求的确定拒绝，也不能借此重放或窥探别人的动作。
        let existing = {
            let _db = timing.span(super::budget::Segment::Database);
            self.store().with(|conn| {
                if !crate::remote::pairing::device_valid(conn, &actor.device_id, now_ms()) {
                    return Err(NativeError::new(
                        action_codes::NOT_FOUND,
                        "native action not found",
                    ));
                }
                if let Some(rejection) =
                    dao::rejection_conn(conn, &command.action_id).map_err(store_error)?
                {
                    return Err(rejection_error(&rejection, actor, session, &command));
                }
                dao::get_action_conn(conn, &command.action_id).map_err(store_error)
            })?
        };
        if let Some(row) = existing {
            let same_directory = std::fs::canonicalize(&session.project_path)
                .ok()
                .is_some_and(|path| path == std::path::Path::new(&row.directory));
            if row.device_id != actor.device_id {
                return Err(NativeError::new(
                    action_codes::NOT_FOUND,
                    "native action not found",
                ));
            }
            if row.session_id != command.session_id
                || row.session_id != session.id
                || row.payload_sha256 != payload_sha256(&command)
            {
                return Err(NativeError::new(
                    action_codes::CONFLICT,
                    "native action conflicts with a live one",
                ));
            }
            // 已入队或已可能写出的动作仅回读原账本，不因目录失效误报零写。
            if row.state != ActionState::NotSent || row.reason_code.is_some() {
                // M3：既有未决行视图与只读核对同面携带行代际呈现面（终态/墓碑不装饰）。
                return self.with_runtime_dead(&actor.device_id, &command.action_id, view_of(&row));
            }
            if !same_directory {
                return Err(NativeError::new(
                    codes::OC_RESPONSE_INVALID,
                    "the original action directory cannot be verified",
                ));
            }
        }
        if local_only {
            return self
                .execute_local_outbox_pinned(actor, session, &command, delivery_epoch, expected)
                .await;
        }
        // **Task C（插队）· 预留前受控拒绝**：目标行不存在 / 非本设备 / 非本会话或
        // 本目录 / 已不在 `queued` / 冻结载荷缺席 ⇒ 直接拒绝，账本零新增、上游零写。
        // 放在 reserve 之前是「零写」的必要条件：一旦 reserve 落行，就再也说不出
        // 「这次请求什么都没留下」。
        self.precheck_prioritize(actor, session, &command)?;
        // 快照构建本身就是 R0 证据链的一次完整核验（登记 + 进程事实 + `/api/info`
        // pid/version/urls 交叉 + 会话 id/目录核对）；链不成立在此处失败，零动作行。
        let snapshot = {
            let _ownership = timing.span(super::budget::Segment::Ownership);
            match self.dispatch_snapshot_pinned(session, expected).await {
                Ok(snapshot) => snapshot,
                Err(error) => return resolve_entry_failure(self, actor, session, &command, error),
            }
        };
        // **H5 预算检查点 ②（reserve 前写权链入口）**：超预算 ⇒ 不再开探针轮
        //（echo 轮本身 10s 窗 + 单飞等待 20.25s），尽快落确定拒绝。
        if timing.exceeded(self) {
            timing.mark_budget_refused();
            return resolve_entry_failure(
                self,
                actor,
                session,
                &command,
                super::budget::budget_exhausted_error(),
            );
        }
        // **写权链 · 预留前复核**（H03 C1 接通）：三互证前置 + echo-challenge 归属
        // 证明（echo 门通过 = 生产归属证明成立）+ ACK 后终验（锁定 TUI 同代际 +
        // 服务通道 `/api/info` 互证，覆盖「应答后退出/换代仍取得派发权」的时序）。
        // 拒绝时零动作行（进 rejections 墓碑），与既有身份门同一条收敛路径。
        let flight_wait_before_gate = self.gate_flight_wait_total_ms();
        {
            let _ownership = timing.span(super::budget::Segment::Ownership);
            if let Err(error) = self
                .verify_write_authority_with(&snapshot.target, &snapshot.source_client)
                .await
            {
                return resolve_entry_failure(self, actor, session, &command, error);
            }
        }
        // H5 分段计时：本次门入口的单飞等待差分计入「排队」段。
        let waited = self.gate_flight_wait_total_ms();
        timing.add_flight_wait_ms(waited - flight_wait_before_gate);
        let store = self.action_store();
        let reserve_result = {
            let _db = timing.span(super::budget::Segment::Database);
            store.reserve(&actor.device_id, &snapshot.target, &command)
        };
        match reserve_result? {
            ReserveOutcome::Existing(view) => {
                // 取消窗口自愈：行还在 not_sent 且没有放弃理由 ⇒ 上次尝试从未 claim 过，
                // 本次继续派发（claim 的 CAS 保证最多一次写出）。墓碑（已拒绝）、在飞与
                // 终态都原样返回——绝不二次写。
                if view.state == ActionState::NotSent && view.reason_code.is_none() {
                    self.dispatch(
                        actor,
                        &snapshot,
                        command,
                        intent,
                        None,
                        delivery_epoch,
                        timing,
                    )
                    .await
                } else {
                    // M3：重复请求回执 = 「既有未决行视图」交给客户端，与只读核对同面
                    // 携带行代际呈现面（未决态才装饰；终态/墓碑原样）。
                    let action_id = view.action_id.clone();
                    self.with_runtime_dead(&actor.device_id, &action_id, view)
                }
            }
            ReserveOutcome::Conflict => Err(NativeError::new(
                action_codes::CONFLICT,
                "native action conflicts with a live one",
            )),
            ReserveOutcome::New => {
                self.dispatch(
                    actor,
                    &snapshot,
                    command,
                    intent,
                    None,
                    delivery_epoch,
                    timing,
                )
                .await
            }
        }
    }

    #[cfg(test)]
    async fn execute_local_outbox(
        &self,
        actor: &NativeActor,
        session: &Session,
        command: &NativeCommand,
        delivery_epoch: u64,
    ) -> Result<ActionView, NativeError> {
        self.execute_local_outbox_pinned(actor, session, command, delivery_epoch, None)
            .await
    }

    /// 本地队列控制只依赖已保存的路由与设备授权，不读取原生服务。
    async fn execute_local_outbox_pinned(
        &self,
        actor: &NativeActor,
        session: &Session,
        command: &NativeCommand,
        delivery_epoch: u64,
        expected: Option<&NativeTarget>,
    ) -> Result<ActionView, NativeError> {
        use std::sync::atomic::Ordering;
        let store = self.action_store();
        let target = self.store().with(|conn| {
            if !crate::remote::pairing::device_valid(conn, &actor.device_id, now_ms()) {
                return Err(NativeError::new(
                    action_codes::NOT_FOUND,
                    "native action not found",
                ));
            }
            let stored_path = std::path::PathBuf::from(&session.project_path);
            if !stored_path.is_absolute() || stored_path.components().any(|part| matches!(part, std::path::Component::ParentDir | std::path::Component::CurDir)) { return Err(action_invalid()); }
            // 本地暂停/撤回可对账本原目标离线执行；目录存在时仍核对其真实路径。
            let directory = std::fs::canonicalize(&stored_path).unwrap_or(stored_path);
            if session.id != command.session_id { return Err(action_invalid()); }
            let (service_id, runtime_pid, started_at_ms, stored_directory) = match &command.operation {
                NativeOperation::RetractOutbox(p) => {
                    let row = dao::get_action_conn(conn, &p.target_action_id).map_err(store_error)?.ok_or_else(action_invalid)?;
                    // 可撤回 = 排队项，或 D-N5-2 的孤儿（not_sent 且无放弃理由：claim 从未
                    // 发生 = 从未出网，撤回零网络写）；已带理由/已 claim 的行不可撤。
                    let retractable = row.state == ActionState::Queued
                        || (row.state == ActionState::NotSent && row.reason_code.is_none());
                    if row.device_id != actor.device_id || row.session_id != command.session_id || !retractable { return Err(action_invalid()); }
                    (row.service_id, row.runtime_pid, row.runtime_started_at_ms, row.directory)
                }
                _ => match dao::route_conn(conn, &command.session_id).map_err(store_error)? {
                    Some(row) if row.route_state == dao::ROUTE_ACTIVE =>
                        (row.service_id, row.runtime_pid, row.runtime_started_at_ms, row.directory),
                    None if matches!(&command.operation, NativeOperation::SetOutboxPaused(p) if p.paused) => {
                        let pinned = expected.ok_or_else(action_invalid)?;
                        if self.cached_target_for_binding(&command.session_id, &command.binding_token).as_ref() != Some(pinned) { return Err(action_invalid()); }
                        (pinned.runtime.service_id.clone(), i64::from(pinned.runtime.pid), pinned.runtime.started_at_ms, pinned.directory.to_string_lossy().into_owned())
                    }
                    _ => return Err(action_invalid()),
                }
            };
            if directory != std::path::Path::new(&stored_directory) { return Err(action_invalid()); }
            let pid = u32::try_from(runtime_pid).map_err(|_| action_invalid())?;
            Ok(NativeTarget {
                runtime: super::types::RuntimeKey { service_id, pid, started_at_ms },
                session_id: command.session_id.clone(), directory,
            })
        })?;
        if expected.is_some_and(|expected| expected != &target)
            || (matches!(&command.operation, NativeOperation::SetOutboxPaused(_))
                && !self.binding_matches(&target, &command.binding_token))
        {
            return Err(NativeError::new(
                codes::OC_BINDING_STALE,
                "the fixed native target changed",
            ));
        }
        match store.reserve(&actor.device_id, &target, command)? {
            ReserveOutcome::Existing(view)
                if view.state != ActionState::NotSent || view.reason_code.is_some() =>
            {
                return Ok(view)
            }
            ReserveOutcome::Conflict => {
                return Err(NativeError::new(
                    action_codes::CONFLICT,
                    "native action conflicts with a live one",
                ))
            }
            _ => {}
        }
        let local_result = self.store().with(|conn| {
            if !self.outbox_runtime().enabled.load(Ordering::SeqCst)
                || self.outbox_runtime().epoch.load(Ordering::SeqCst) != delivery_epoch
                || !crate::remote::pairing::device_valid(conn, &actor.device_id, now_ms())
            {
                return Err(action_invalid());
            }
            let tx = conn.unchecked_transaction().map_err(store_error)?;
            let target_valid = match &command.operation {
                NativeOperation::RetractOutbox(p) => dao::get_action_conn(&tx, &p.target_action_id)
                    .map_err(store_error)?
                    .is_some_and(|row| {
                        row.device_id == actor.device_id
                            && row_belongs_to_current(&row, &target)
                            // D-N5-2：排队项或无理由 not_sent 孤儿（从未出网）均可撤回。
                            && (row.state == ActionState::Queued
                                || (row.state == ActionState::NotSent
                                    && row.reason_code.is_none()))
                    }),
                _ => match dao::route_conn(&tx, &command.session_id).map_err(store_error)? {
                    Some(route) => route.route_state == dao::ROUTE_ACTIVE && route.same_runtime(&target),
                    None => matches!(&command.operation, NativeOperation::SetOutboxPaused(p) if p.paused)
                        && expected.is_some_and(|pinned| pinned == &target)
                        && self.cached_target_for_binding(&command.session_id, &command.binding_token).as_ref() == Some(&target),
                },
            };
            if !target_valid { return Err(action_invalid()); }
            if !dao::claim_action_in_transaction(&tx, &command.action_id, &store.epoch, now_ms())
                .map_err(store_error)?
            {
                return Ok(());
            }
            let applied = match &command.operation {
                NativeOperation::SetOutboxPaused(p) => {
                    crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                        &tx,
                        &command.session_id,
                        &target.directory.to_string_lossy(),
                        p.paused,
                        p.expected_revision,
                    )
                }
                NativeOperation::RetractOutbox(p) => {
                    crate::database::dao::opencode_outbox::retract_in_transaction(
                        &tx,
                        &actor.device_id,
                        &command.session_id,
                        &p.target_action_id,
                    )
                }
                _ => return Err(action_invalid()),
            }
            .map_err(store_error)?;
            let (state, reason) = if applied {
                (ActionState::Settled, Some("local_outbox_applied"))
            } else {
                (ActionState::NotAccepted, Some(NOT_ACCEPTED_EVIDENCE))
            };
            dao::apply_finish_conn(&tx, &command.action_id, state, None, reason, now_ms())
                .map_err(store_error)?;
            tx.commit().map_err(store_error)
        });
        if let Err(error) = local_result {
            // 事务存储错误已回滚 claim 与本地效果，保留未放弃动作供同意图重试。
            if error.code == action_codes::STORE_FAILED {
                return Err(error);
            }
            let current = store.read(&actor.device_id, &command.action_id)?;
            return Err(preclaim_failure(store, &current, &command.action_id, error));
        }
        store.read(&actor.device_id, &command.action_id)
    }

    fn rebind_payload(
        &self,
        command: &NativeCommand,
    ) -> Result<super::outbox::FrozenSend, NativeError> {
        use rusqlite::OptionalExtension;
        let NativeOperation::RebindOutbox(source) = &command.operation else {
            return Err(action_invalid());
        };
        let json=self.store().with(|conn| {
            let row=dao::get_action_conn(conn,&command.action_id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            // 本动作已 queued 时读自己的快照；首次重新确认只能读属主尚未 claim 的旧项。
            conn.query_row("SELECT o.command_json FROM opencode_outbox o JOIN opencode_native_actions a ON a.action_id=o.action_id AND a.session_id=o.session_id WHERE a.action_id IN (?1,?2) AND a.device_id=?3 AND a.session_id=?4 AND a.directory=?5 AND a.state='queued' AND o.command_json IS NOT NULL ORDER BY (a.action_id=?1) DESC LIMIT 1",rusqlite::params![command.action_id,source.target_action_id,row.device_id,command.session_id,row.directory],|r|r.get::<_,String>(0)).optional()
        }).map_err(store_error)?.ok_or_else(action_invalid)?;
        serde_json::from_str(&json).map_err(|_| action_invalid())
    }

    /// **插队的冻结载荷**（Task C）：只认目标行本身（绝不读本动作自己的行）。
    ///
    /// 与 [`Self::rebind_payload`] 的分工：重绑是「换一个 actionId 重新确认同一意图」，
    /// 因此可以读本源或自己的快照；插队是「派发**既有那一行**」，所以必须严格读
    /// `target_action_id` 这一行，且该行仍在 `queued`、冻结载荷在案。正文/附件/
    /// `clientMessageId` 全部照抄，调用方唯一允许的改动是把投递方式改成 steer。
    fn frozen_outbox_row(
        &self,
        command: &NativeCommand,
    ) -> Result<super::outbox::FrozenSend, NativeError> {
        use rusqlite::OptionalExtension;
        let NativeOperation::PrioritizeOutbox(source) = &command.operation else {
            return Err(action_invalid());
        };
        let json = self
            .store()
            .with(|conn| {
                conn.query_row(
                    "SELECT o.command_json FROM opencode_outbox o JOIN opencode_native_actions a \
                     ON a.action_id=o.action_id AND a.session_id=o.session_id \
                     WHERE a.action_id=?1 AND a.state='queued' AND o.command_json IS NOT NULL",
                    [&source.target_action_id],
                    |r| r.get::<_, String>(0),
                )
                .optional()
            })
            .map_err(store_error)?
            .ok_or_else(action_invalid)?;
        let frozen: super::outbox::FrozenSend =
            serde_json::from_str(&json).map_err(|_| action_invalid())?;
        // 冻结命令必须就是这一行（防御：载荷与行错位时不派发别人的正文）。
        if frozen.command.action_id != source.target_action_id {
            return Err(action_invalid());
        }
        Ok(frozen)
    }

    /// 插队的前置受控拒绝（**在 reserve 之前**调用）：不受理时账本零新增、上游零写。
    ///
    /// 判据 = 目标行存在、属本设备、属本会话与本目录、仍在 `queued`、冻结载荷在案。
    /// 任一不成立即返回受控错误：`not_found`（不存在/别人的行，不给存在性预言机）/
    /// `OC_ACTION_INVALID`（会话或目录不符）/ `OC_ACTION_CONFLICT`（已不在队列）。
    fn precheck_prioritize(
        &self,
        actor: &NativeActor,
        session: &Session,
        command: &NativeCommand,
    ) -> Result<(), NativeError> {
        let NativeOperation::PrioritizeOutbox(source) = &command.operation else {
            return Ok(());
        };
        let directory = std::fs::canonicalize(&session.project_path)
            .unwrap_or_else(|_| std::path::PathBuf::from(&session.project_path));
        let row = self.store().with(|conn| {
            dao::get_action_conn(conn, &source.target_action_id).map_err(store_error)
        })?;
        let Some(row) = row else {
            return Err(NativeError::new(
                action_codes::NOT_FOUND,
                "native action not found",
            ));
        };
        if row.device_id != actor.device_id {
            return Err(NativeError::new(
                action_codes::NOT_FOUND,
                "native action not found",
            ));
        }
        if row.session_id != command.session_id || directory != std::path::Path::new(&row.directory)
        {
            return Err(action_invalid());
        }
        if row.state != ActionState::Queued {
            return Err(NativeError::new(
                action_codes::CONFLICT,
                "native action conflicts with a live one",
            ));
        }
        let has_payload = self
            .store()
            .with(|conn| {
                conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM opencode_outbox WHERE action_id=?1 \
                     AND command_json IS NOT NULL)",
                    [&source.target_action_id],
                    |r| r.get::<_, bool>(0),
                )
            })
            .map_err(store_error)?;
        if !has_payload {
            return Err(action_invalid());
        }
        Ok(())
    }

    /// 消费已 reserve 的本地意图。不会通过重放 execute 取得第二个动作，也不改回 NotSent。
    pub async fn flush_outbox(&self, session: &Session) -> Result<(), NativeError> {
        self.flush_outbox_at_epoch(
            session,
            self.outbox_runtime()
                .epoch
                .load(std::sync::atomic::Ordering::SeqCst),
        )
        .await
    }

    pub(crate) async fn flush_outbox_at_epoch(
        &self,
        session: &Session,
        delivery_epoch: u64,
    ) -> Result<(), NativeError> {
        // R2：后台 flush 与直接派发用**同一**服务身份判定——每项的 dispatch 在 claim
        // 前核对；链不成立时快照失败，queued 项记原因原样保留，绝不重投。
        let directory = std::fs::canonicalize(&session.project_path)
            .unwrap_or_else(|_| std::path::PathBuf::from(&session.project_path));
        if !directory.is_absolute() {
            return Err(action_invalid());
        }
        let directory = directory.to_string_lossy().into_owned();
        for _ in 0..8 {
            let pending = self.store().with(|conn| {
                use rusqlite::OptionalExtension;
                // I1（统一队首规则 = 统一等待）：选取与最终派发共用同一队首定义——
                // (session,directory) 内 `state IN ('queued','dispatching')` 的最早
                // outbox 行，与 claim 的 FIFO 前序谓词同源（不再单设「跳过被拦队首」
                // 过滤器，消除「选取跳过/派发算前序」双规则缝）。队首带换窗拒绝原因
                // ⇒ 整队停止（下方臂），不越队首先发。
                conn.query_row("SELECT o.action_id,o.command_json,a.state,a.reason_code FROM opencode_outbox o JOIN opencode_native_actions a ON a.action_id=o.action_id AND a.session_id=o.session_id JOIN opencode_outbox_targets t ON t.session_id=a.session_id AND t.directory=a.directory WHERE o.session_id=?1 AND a.directory=?2 AND a.state IN ('queued','dispatching') AND t.paused=0 AND EXISTS (SELECT 1 FROM settings WHERE key='opencode.outbox_target_schema_version' AND value='1') ORDER BY o.seq LIMIT 1", rusqlite::params![session.id,directory], |r| Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?))).optional().map_err(store_error)
            })?;
            let Some((action_id, json, head_state, head_reason)) = pending else {
                return Ok(());
            };
            // I1 统一队首臂 ①：队首在飞（dispatching 是 claim 的前序谓词之一）——
            // 本循环不与其抢，等待其收敛后下一轮再评估。
            if head_state == "dispatching" {
                return Ok(());
            }
            // I1 统一队首臂 ②：queued 而载荷缺席是不可派发形态（实际不可达；claim
            // 同样把它算前序）——保守等待而非跳过，绝不越过队首。
            let Some(json) = json else {
                return Ok(());
            };
            // I1 统一队首臂 ③：队首带换窗拒绝原因 ⇒ 整队停止——零验权零投递，
            // 不重试不清除，直到用户显式处理（重选/重新确认 rebind_outbox 替换）。
            // 后续项同样等待（保序）；其余原因（瞬时读失败等）的队首保留既有重试语义。
            if head_reason.as_deref() == Some(codes::OC_ROUTE_WINDOW_MISMATCH) {
                return Ok(());
            }
            let row = self
                .store()
                .with(|conn| dao::get_action_conn(conn, &action_id).map_err(store_error))?
                .ok_or_else(action_invalid)?;
            let actor = self.store().with(|conn| {
                crate::remote::pairing::device_valid(conn, &row.device_id, now_ms()).then(|| {
                    NativeActor {
                        device_id: row.device_id.clone(),
                        device_name: crate::remote::pairing::device_name(conn, &row.device_id),
                        signature_enabled: message_signature_enabled_conn(conn),
                    }
                })
            });
            let Some(actor) = actor else {
                self.action_store().finish(
                    &action_id,
                    ActionState::NotSent,
                    None,
                    Some("device_revoked"),
                )?;
                continue;
            };
            let frozen: super::outbox::FrozenSend =
                serde_json::from_str(&json).map_err(|_| action_invalid())?;
            let mut command = frozen.command;
            if payload_sha256(&command) != row.payload_sha256 {
                return Err(action_invalid());
            }
            let snapshot = match self.dispatch_snapshot(session).await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    self.store().with(|conn| {
                        crate::database::dao::opencode_outbox::set_queued_reason_conn(
                            conn,
                            &action_id,
                            Some(&error.code),
                        )
                        .map_err(store_error)
                    })?;
                    return Err(error);
                }
            };
            if !row_belongs_to_current(&row, &snapshot.target) {
                self.store().with(|conn| {
                    crate::database::dao::opencode_outbox::set_queued_reason_conn(
                        conn,
                        &action_id,
                        Some(codes::OC_BINDING_STALE),
                    )
                    .map_err(store_error)
                })?;
                return Err(binding_stale());
            }
            // token 含进程随机锚。仅在账本中的同实例/同目录已核对后刷新内存副本，绝不换代重绑。
            command.binding_token = snapshot.binding_token.clone();
            let intent = intent_of(&command)?;
            // H5：后台 flush 的派发同样带五段计时与统一预算（claim 闸共用）。
            let mut timing = super::budget::ActionTiming::start(
                &action_id,
                &row.session_id,
                operation_word(&command.operation),
            )
            .begin();
            let result = self
                .dispatch(
                    &actor,
                    &snapshot,
                    command,
                    intent,
                    Some(frozen.attachment_paths),
                    delivery_epoch,
                    &mut timing,
                )
                .await;
            timing.finish(self);
            let result = result?;
            if result.state == ActionState::Queued {
                if let Some(reason) = result.reason_code {
                    return Err(NativeError::new(&reason, "queued action remains pending"));
                }
                return Ok(());
            }
        }
        Ok(())
    }

    /// 只读原工具：读动作视图（属主设备；别人的动作与不存在同码）。
    ///
    /// 出参为 `acknowledged`/`unconfirmed` 行附加 [`Self::with_runtime_dead`]
    /// 的行代际呈现面（M3）；其余状态原样返回（`runtime_dead = None`）。
    pub async fn action(
        &self,
        device_id: &str,
        action_id: &str,
    ) -> Result<ActionView, NativeError> {
        let view = self.action_store().read(device_id, action_id)?;
        self.with_runtime_dead(device_id, action_id, view)
    }

    /// J2（无本地锚发现）：本会话中属主设备需要核对的动作清单——动作行（含
    /// runtime_dead 呈现面）+ 前置拒绝（NotSent 视图，operation=unknown 占位）。
    /// 绑定态无关（旧绑定 409 下仍可用）；非属主设备同名会话查询=空（不泄露
    /// 存在性，与 [`Self::read`] 同口径）。
    pub async fn actions_for_session(
        &self,
        device_id: &str,
        session_id: &str,
    ) -> Result<Vec<OpenCodeActionListEntry>, NativeError> {
        let rows = self
            .store()
            .with(|conn| dao::actions_for_session_device(conn, device_id, session_id))
            .map_err(store_error)?;
        let mut entries = Vec::new();
        for row in &rows {
            let view = self.action_store().read(device_id, &row.action_id)?;
            if !matches!(
                view.state,
                ActionState::NotSent
                    | ActionState::Queued
                    | ActionState::Dispatching
                    | ActionState::Acknowledged
                    | ActionState::Unconfirmed
            ) || (view.state == ActionState::NotSent && view.reason_code.is_some())
            {
                continue;
            }
            let view = self.with_runtime_dead(device_id, &row.action_id, view)?;
            entries.push(OpenCodeActionListEntry {
                action_id: view.action_id,
                state: view.state,
                operation: row.operation.clone(),
                reason_code: view.reason_code,
                native_id: view.native_id,
                runtime_dead: view.runtime_dead,
            });
        }
        Ok(entries)
    }

    /// **原动作服务代际三态判定**（F12 项 2 + G3 收紧共用判定）：依据账本原服务身份
    /// （actions 行 service_id/runtime_pid/runtime_started_at）+ 三形态进程事实实测，
    /// 把 `acknowledged`/`unconfirmed` 行的代际归为三态：
    ///
    /// * [`OriginalGeneration::Alive`]：generation_fact 实测原进程仍以账本身份存活
    ///   （pid+lstart 全等）——登记切到新服务也不改判（原通道仍可核对，不可归档）；
    /// * [`OriginalGeneration::Ended`]：generation_fact 可证明「进程退出（机制明确
    ///   报告无此 pid）或 PID 已换代（pid 活但 lstart≠账本）」——只有这两种**可证明
    ///   形态**放行归档（G3：登记可读性不再是死亡佐证）；
    /// * [`OriginalGeneration::InsufficientEvidence`]：pid 不可解析，或事实通道读取
    ///   失败（fail-closed：读不到 ≠ 已死，绝不放行归档、不渲染出口）。
    ///
    /// [`Self::with_runtime_dead`]（出口渲染）与 [`Self::confirm_unknown`]（归档权威
    /// 闸）共用本判定，口径统一；两处对 Alive/InsufficientEvidence 的行为一致
    ///（不渲染出口、409 拒绝归档）。
    pub(crate) fn classify_original_generation(&self, row: &dao::ActionRow) -> OriginalGeneration {
        classify_original_generation_with(self.original_generation_fact(), row)
    }

    /// **行代际呈现面**（M3，P1-c；F12 项 2 改口径）：`acknowledged`/`unconfirmed`
    /// 视图附加 `runtime_dead`——按 [`Self::classify_original_generation`] 三态判定：
    /// GenerationEnded ⇒ `Some(true)`（渲染「确认终态未知」出口）；OriginalAlive ⇒
    /// `Some(false)`（仍可核对，呈现面收回、不渲染出口）；InsufficientEvidence ⇒
    /// `None`（未判定 ≠ 死亡：不显示出口、不允许确认，绝不猜方向）。
    ///
    /// 这是 [`Self::confirm_unknown`] 拒绝闸的**廉价只读镜像**（零网络读、零认领），
    /// 与权威闸共用同一判定（口径统一的 F12 项 2）；行定位失败（竞态删除等）按
    /// 「未判定」原样返回，绝不猜方向。
    fn with_runtime_dead(
        &self,
        device_id: &str,
        action_id: &str,
        view: ActionView,
    ) -> Result<ActionView, NativeError> {
        if !matches!(
            view.state,
            ActionState::Acknowledged | ActionState::Unconfirmed
        ) {
            return Ok(view);
        }
        let row = match self.action_store().locate(device_id, action_id) {
            Ok(row) => row,
            Err(err) if err.code == action_codes::NOT_FOUND => return Ok(view),
            Err(err) => return Err(err),
        };
        let runtime_dead = match self.classify_original_generation(&row) {
            OriginalGeneration::Ended => Some(true),
            OriginalGeneration::Alive => Some(false),
            OriginalGeneration::InsufficientEvidence => None,
        };
        Ok(ActionView {
            runtime_dead,
            ..view
        })
    }

    /// 只读核对：**绝不重发**。只有权威回读证明某事时才改写动作状态（R1）。
    ///
    /// 一次新鲜投影就是回读的全部依据（`records`/`inbox`/`pending` 正是为这一步而备），
    /// 且它必须属于**动作行记录的那个服务代际**——换代际后旧代际的空 pending/空 inbox
    /// 不能证明任何事（计划审查重点 #2：旧绑定失效）：
    ///
    /// * `send`：精确原生 id 出现在消息列表 → `Confirmed`；只出现在 inbox → `Acknowledged`；
    ///   两者都缺席 → 不改写（正文相同不是证据，缺席也不是「未生效」的证据）。
    /// * `answer`/`decide`：回读证明仍在 pending → **保持 `Unconfirmed`**（R1：瞬态
    ///   pending 不足以证明整份未生效，占用不释放）；已支持终态（表单 `answered`/
    ///   `cancelled`）或**可信**的审批缺席 → `Settled`（只说已定案，不声称哪端获胜）。
    ///   未知表单状态、归属不可证的审批缺席 → 保持 `Unconfirmed`（R3：不可信证据不定案）。
    /// * `retract`：仍在 inbox → **保持 `Unconfirmed`**；已被投递 → `Settled`；完整
    ///   回读下既不在 inbox 也不在消息列表 → `Confirmed`。
    /// * 取消窗口自愈：停在 `dispatching` 的行（派发 future 被丢弃）也由本路径收敛——
    ///   有证据按证据落账，无证据收敛为 `Unconfirmed`（绝不永远卡在飞）。
    /// * 行属于旧代际（旧锚 echo 不可达）：`send` 族行不再一律原样返回——G4 patch7
    ///   起，经 [`Self::verify_by_cross_generation_readback`] 复用 confirm_unknown 同款
    ///   跨代 read-back（当前可达服务 + 严格三元组），命中正式消息记录 ⇒ 收敛
    ///   `Confirmed`（零重投）；不命中/不可达 ⇒ 原样保持。
    /// * 回读失败 / 良性落账竞态 → 原样返回或返回最新视图，零伪造。
    ///
    /// 出参为 `acknowledged`/`unconfirmed` 行附加 [`Self::with_runtime_dead`]
    /// 的行代际呈现面（M3）：核对没收敛的死代际行，回执当场告诉前端出口在场。
    pub async fn verify(
        &self,
        device_id: &str,
        action_id: &str,
    ) -> Result<ActionView, NativeError> {
        let view = self.verify_inner(device_id, action_id).await?;
        self.with_runtime_dead(device_id, action_id, view)
    }

    /// [`Self::verify`] 的本体（呈现面装饰之前的既有语义，零改动）。
    async fn verify_inner(
        &self,
        device_id: &str,
        action_id: &str,
    ) -> Result<ActionView, NativeError> {
        let store = self.action_store();
        let current = store.read(device_id, action_id)?;
        if !matches!(
            current.state,
            ActionState::Dispatching | ActionState::Acknowledged | ActionState::Unconfirmed
        ) {
            return Ok(current);
        }
        let row = store.locate(device_id, action_id)?;
        // 只在「结果不明（含派发中被取消的 dispatching）」时回读；未派发与终态没有
        // 可证明的转移。
        if !matches!(
            row.state,
            ActionState::Dispatching | ActionState::Acknowledged | ActionState::Unconfirmed
        ) {
            return Ok(view_of(&row));
        }
        let session = session_card(&row.session_id, &row.directory);
        let Ok(pid) = u32::try_from(row.runtime_pid) else {
            return Ok(view_of(&row));
        };
        let target = NativeTarget {
            runtime: RuntimeKey {
                service_id: row.service_id.clone(),
                pid,
                started_at_ms: row.runtime_started_at_ms,
            },
            session_id: row.session_id.clone(),
            directory: std::path::PathBuf::from(&row.directory),
        };
        let epoch = self.observation_epoch();
        // 回读只使用账本原目标；不得发现并安装同 session 的其他来源。
        let snapshot = match self.dispatch_snapshot_pinned(&session, Some(&target)).await {
            Ok(snapshot) => snapshot,
            // G4 patch7（GC）：旧锚 echo 不可达（旧服务死 / 登记已换代）⇒ send 族行
            // 复用 confirm_unknown 同款跨代 read-back 收敛（零重投）；不命中/不可达
            // ⇒ 原样返回（不假收敛），与既有死代际语义一致。
            Err(_) => {
                return self
                    .verify_by_cross_generation_readback(device_id, &row)
                    .await
            }
        };
        let Some(source) = self.snapshot_source(&snapshot) else {
            return Ok(view_of(&row));
        };
        if source.descriptor.runtime != snapshot.target.runtime {
            // 换代际：当前投影自身都不是同一代际，无从证明。
            return Ok(view_of(&row));
        }
        if !row_belongs_to_current(&row, &snapshot.target) {
            // 行记录的是**已死的服务代际**：新代际的回读（含它的空 pending/空 inbox）
            // 不能证明旧写入的结果——原样返回，零改写。
            return Ok(view_of(&row));
        }
        let current = row.state;
        let outcome = match (row.operation.as_str(), row.request_key.as_deref()) {
            ("send" | "rebind_outbox", _) => verify_send(&snapshot, action_id, current),
            // 插队控制动作自己没有消息记录：它的证据是**原队列行那条消息**是否已入
            // 正式记录（`msg_<target>`）。命中 ⇒ 插队确实投出去了（`settled` +
            // `steered_interrupt`）；尚未命中 ⇒ 与 send 一致的 `dispatching` 收敛。
            ("prioritize_outbox", Some(target)) => {
                let native_id = client_message_id_of(target);
                if records_contain(&snapshot.records, &native_id) {
                    Some(Outcome::terminal(
                        ActionState::Settled,
                        None,
                        Some(reason_words::STEERED_INTERRUPT),
                    ))
                } else {
                    converge_if_dispatching(current, reason_words::ID_ABSENT)
                }
            }
            ("answer", Some(form_id)) => verify_answer(&source, &snapshot, form_id, current).await,
            ("decide", Some(request_id)) => verify_decide(&snapshot, request_id, current),
            ("retract", Some(item_id)) => verify_retract(&snapshot, item_id, current),
            ("stop_session", _) => verify_stop(&source, &snapshot.target, current).await,
            ("set_agent", Some(agent)) => {
                verify_set_agent(&source, &snapshot.target, agent, current).await
            }
            _ => None,
        };
        let Some(outcome) = outcome else {
            return Ok(view_of(&row));
        };
        // 终验（核对读取完成后）：用捕获的同一 client 重读 `/api/info` 核对服务身份——
        // 同端点换接收方时 PID/指针检查都可能仍成立，只有端点自报能分辨。
        // 核验失败 ⇒ 证据不可用，零改写保持未知。
        if reverify_write_channel_identity(&source).await.is_err() {
            return Ok(view_of(&row));
        }
        #[cfg(test)]
        self.run_verify_before_store_hook();
        let finished = self.store().with(|conn| {
            // 先取得数据库锁再读取进程事实，避免等待锁期间旧代际失效。
            if !self.snapshot_process_current(&snapshot) {
                return None;
            }
            self.with_current_snapshot(&snapshot, epoch, || {
                dao::finish_action_conn(
                    conn,
                    action_id,
                    outcome.state,
                    outcome.native_id.as_deref(),
                    outcome.reason.as_deref(),
                    now_ms(),
                )
                .map_err(store_error)
            })
        });
        match finished {
            None => store.read(device_id, action_id),
            Some(Ok(FinishDao::Applied | FinishDao::Illegal)) => store.read(device_id, action_id),
            Some(Ok(FinishDao::NotFound)) => Err(NativeError::new(
                action_codes::NOT_FOUND,
                "native action not found",
            )),
            Some(Err(error)) => Err(error),
        }
    }

    /// **显式确认终态未知**（patch3 D2，H13 证据 §10-D2）：死亡代际 `acknowledged`/
    /// `unconfirmed` 行的用户可达出口。审计 = 理由词 [`dao::USER_CONFIRMED_UNKNOWN`]
    /// + 属主设备校验（`locate` 只放行属主）+ 行 `updated_at`（确认时间）。
    ///
    /// F12 项 2（C6 三态代际判定）：提交时刻重跑 [`Self::classify_original_generation`]
    /// ——仍 **GenerationEnded** 才落账；OriginalAlive（原进程实测仍活）与
    /// InsufficientEvidence（登记缺失/读失败等证据不足形态）一律 409 拒绝（fail-closed，
    /// 判定漂移同样在此闸收紧）。落账前按**消息证据分流**（F12 项 2.4）：经当前可达
    /// 服务对原会话+规范目录 native read-back 查到 `msg_<actionId>` 正式消息记录 ⇒
    /// 以该记录为准记 **Confirmed**（落 native id，不损失定案）；读不回/服务不可达 ⇒
    /// `UnknownConfirmed`（保守，账本判定保持未知，绝不冒充 not_sent/任何定案）。
    /// 释放待答占用与解除未决屏障由 DAO 状态机同事务完成（见
    /// [`crate::database::dao::opencode_actions`] 的 `UnknownConfirmed` 臂）。
    pub async fn confirm_unknown(
        &self,
        device_id: &str,
        action_id: &str,
    ) -> Result<ActionView, NativeError> {
        let store = self.action_store();
        // 动作属主核对（locate 只放行属主设备；别人的动作与不存在同码）+ 状态核对
        // （归档入口只对 acknowledged/unconfirmed 开放，终态/墓碑原样返回）。
        let row = store.locate(device_id, action_id)?;
        if !matches!(
            row.state,
            ActionState::Acknowledged | ActionState::Unconfirmed
        ) {
            return Ok(view_of(&row));
        }
        // 代际闸（F12 项 2.3，提交时刻重跑判定）：仍 GenerationEnded 才落账。
        if self.classify_original_generation(&row) != OriginalGeneration::Ended {
            return Err(NativeError::new(
                action_codes::STATE,
                "该动作所属的服务代际未被证明死亡：请先用「只读核对」收敛结果",
            ));
        }
        // 证据分流（F12 项 2.4）：有正式消息证据 ⇒ confirmed；无 ⇒ unknown_confirmed。
        let session = session_card(&row.session_id, &row.directory);
        match self.read_back_original_message(&session, &row).await {
            Some(native_id) => {
                store.finish(
                    action_id,
                    ActionState::Confirmed,
                    Some(native_id.as_str()),
                    None,
                )?;
            }
            None => {
                store.finish(
                    action_id,
                    ActionState::UnknownConfirmed,
                    None,
                    Some(dao::USER_CONFIRMED_UNKNOWN),
                )?;
            }
        }
        store.read(device_id, action_id)
    }

    /// F12 项 2.4 的 read-back 原语：经**当前可达服务**（新代际）对原会话与规范目录
    /// 做只读投影，查找 `msg_<actionId>` 正式消息记录。证据三元组（原会话、规范目录、
    /// 消息 id 三者全等）任一不成立、服务不可达或读取失败 ⇒ `None`（保守：不伪造定案，
    /// 落 unknown_confirmed）。
    ///
    /// 零远端写（只读 GET），零认领、零安装。
    async fn read_back_original_message(
        &self,
        session: &Session,
        row: &dao::ActionRow,
    ) -> Option<String> {
        let native_id = client_message_id_of(&row.action_id);
        self.read_back_native_message(session, row, &native_id)
            .await
    }

    /// 同一回读原语的可指定 id 形态（插队的消息 id 是**原队列行**的，不是控制动作的）。
    async fn read_back_native_message(
        &self,
        session: &Session,
        row: &dao::ActionRow,
        native_id: &str,
    ) -> Option<String> {
        let Ok(snapshot) = self.dispatch_snapshot_pinned(session, None).await else {
            return None;
        };
        if snapshot.target.session_id != row.session_id
            || snapshot.target.directory.to_string_lossy() != row.directory
        {
            return None;
        }
        records_contain(&snapshot.records, native_id).then(|| native_id.to_string())
    }

    /// G4 patch7（GC）：核对路径的跨代 read-back 收敛。旧锚 echo 不可达（旧服务死 /
    /// 登记已换代）时，send 族行（`send` / `rebind_outbox`——只有这两族写会留下
    /// `msg_<actionId>` 正式消息记录）复用 [`Self::read_back_original_message`]：
    /// 经**当前可达服务**（当前登记 / 发现命中的活服务）做严格三元组匹配
    ///（原 session_id + 规范目录 + `msg_<actionId>`）。命中 ⇒ 正式消息记录在案是
    /// 「写入已落地」的积极证据（F12 项 2.4 同判据：记录跨换代持久在会话里，精确 id
    /// 只可能由本动作的原始派发写入）⇒ 该活行动作升级 `Confirmed`（落 native_id），
    /// 零重投（只读 GET）；不命中 / 服务不可达 ⇒ `None`，调用方保持原状（不假收敛、
    /// 不置终态）。
    ///
    /// 与 [`Self::confirm_unknown`] 的语义分工（共用回读、不共用状态迁移）：核对收敛
    /// = 活行动作的证据升级（不经过「确认终态未知」的归档态）；归档入口保持既有，
    /// 只对可证明死亡代际开放。落账不经进程快照闸（同 confirm_unknown：证据是会话
    /// 记录里的持久事实，非易逝服务状态），零远端写、零认领。
    /// 插队的缺口：控制动作若因外界中断停在 `dispatching`，跨代核对也能按原行消息
    /// 收敛（与 send 族同源，只读 GET、零重投）。`request_key` 就是目标行 id。
    async fn verify_by_cross_generation_readback(
        &self,
        device_id: &str,
        row: &dao::ActionRow,
    ) -> Result<ActionView, NativeError> {
        if !matches!(
            row.operation.as_str(),
            "send" | "rebind_outbox" | "prioritize_outbox"
        ) {
            return Ok(view_of(row));
        }
        let session = session_card(&row.session_id, &row.directory);
        // 插队的关联 id 是**原行**的；send/rebind 是自己的。
        let native_id = match (row.operation.as_str(), row.request_key.as_deref()) {
            ("prioritize_outbox", Some(target)) => {
                let witness = self.read_back_original_message(&session, row).await;
                // `read_back_original_message` 用 `msg_<本行 id>`，对插队不适用——
                // 直接按原行 id 做同一三元组回读。
                match witness {
                    Some(_) => None,
                    None => {
                        self.read_back_native_message(&session, row, &client_message_id_of(target))
                            .await
                    }
                }
            }
            _ => self.read_back_original_message(&session, row).await,
        };
        let Some(native_id) = native_id else {
            return Ok(view_of(row));
        };
        let (state, reason) = if row.operation == "prioritize_outbox" {
            (ActionState::Settled, Some(reason_words::STEERED_INTERRUPT))
        } else {
            (ActionState::Confirmed, None)
        };
        self.action_store()
            .finish(&row.action_id, state, Some(native_id.as_str()), reason)?;
        self.action_store().read(device_id, &row.action_id)
    }

    /// 选择原生路由（任务 8 屏障的切换侧）：持同会话 write permit 复核后**原子**登记归属。
    ///
    /// 次序（与旧写入口的 read permit 严格对立）：
    /// 1. **等旧 read permit 全部释放**（`write_owned().await`）——在飞旧写（入队 /
    ///    注入 / 结果记录）全部结束后才动路由，从根上消除「守卫检查后才切路由」的
    ///    竞态；切路由进行中新的旧写/ flush 也取不到 read permit（公平锁），只能让位；
    /// 2. 持 write permit 调任务 5 的 [`NativeActionStore::try_claim_route`]：同一
    ///    DB 事务内复核旧 `inject_queue` 未决行、调用方断言的 `legacy_quiescent`、
    ///    换绑代际时的未决 native 动作，然后原子登记 `active`。任一审不住 → `false`：
    ///    不迁移未知旧动作、不重复提交、不抢夺别人的归属。
    ///
    /// `legacy_quiescent` 恒传 `true` 的理由：本函数持 write permit 即已证明「没有
    /// 正在进行的旧写」（在飞者全部结束），而「旧待发行已排空」由 DAO 事务亲证
    /// （`count_pending_for_session_conn`）——调用方无法自证的其它猜测一概不传。
    ///
    /// 返回 `false` = 未进入 Native（旧 pending 未决 / 未决 native 动作）：调用方保持
    /// 旧路径，由用户先处理旧项。
    pub async fn select_native_route(&self, target: &NativeTarget) -> Result<bool, NativeError> {
        self.select_native_route_with(target, false).await
    }

    /// 认领的完整形态：`allow_reactivate=false`（隐式，视图认领臂）不会复活 `draining`；
    /// `true`（显式，`POST /opencode/route/native`）才允许用户重新进入（评审 R-2）。
    pub(crate) async fn select_native_route_with(
        &self,
        target: &NativeTarget,
        allow_reactivate: bool,
    ) -> Result<bool, NativeError> {
        // R2 服务身份门：认领路由与派发同一判定（多 service 保守闸 + 证据链）。
        self.verify_target_identity(target).await?;
        let _write = self.legacy_lock(&target.session_id).write_owned().await;
        self.action_store()
            .try_claim_route_with(target, true, allow_reactivate)
    }

    /// **交回旧路径**（任务 8 / 评审 I-1）：持同会话 write permit，把 `active` 降级为
    /// `draining`——此后旧写入口重新可用（`owns_session` 只认 `active`）。
    ///
    /// 语义与不变式（全部由既有 DAO 承担，本函数不新增规则）：
    /// 1. **未决 native 动作在场即拒绝**（`disengage_route_conn` 的第一道判断）：结果不明的
    ///    动作不能靠「切回旧路径」绕过，调用方拿到 `false` 必须如实回 409；
    /// 2. **不删行、不迁移**：路由行只降级（`draining`）等待重新启用；动作账本行原样保留，
    ///    旧 FIFO 里的遗留项也不会被搬动或重投；
    /// 3. **幂等**：从未接入 Native 的会话没有路由行 ⇒ `Ok(true)`（无事可做即成功），
    ///    重复调用同样成功（已经是 `draining` 时只是原地重写一行）。
    ///
    /// 返回 `true` = 该会话现在归旧路径；`false` = 有未决 native 动作，**零变更**。
    pub async fn disengage_route(&self, session_id: &str) -> Result<bool, NativeError> {
        let _write = self.legacy_lock(session_id).write_owned().await;
        settle_route(self.action_store(), session_id)
    }

    /// 条件交回：同会话路由写锁与数据库事务内都核对原目标。
    pub async fn return_route_for_target(
        &self,
        target: &NativeTarget,
        force: bool,
    ) -> Result<(bool, usize), NativeError> {
        self.return_route_for_target_confirmed(target, force, None, None)
            .await
    }

    pub(crate) fn force_candidates_for_target(
        &self,
        target: &NativeTarget,
    ) -> Result<(i64, Vec<String>), NativeError> {
        let current = self.current_runtime();
        let store = self.action_store();
        self.store().with(|conn| {
            let route = dao::route_conn(conn, &target.session_id)
                .map_err(store_error)?
                .filter(|route| route.same_runtime(target))
                .ok_or_else(|| {
                    NativeError::new(codes::OC_BINDING_STALE, "the selected native route changed")
                })?;
            let ids = dao::force_candidates_conn(
                conn,
                &target.session_id,
                current.as_ref(),
                &store.epoch,
                &self.active_in_flight(),
                Some(target),
            )
            .map_err(store_error)?;
            Ok((route.route_version, ids))
        })
    }

    pub(crate) async fn return_route_for_target_confirmed(
        &self,
        target: &NativeTarget,
        force: bool,
        expected_version: Option<i64>,
        confirmed_ids: Option<&[String]>,
    ) -> Result<(bool, usize), NativeError> {
        let route_before = self
            .store()
            .with(|conn| dao::route_conn(conn, &target.session_id))
            .map_err(store_error)?
            .filter(|route| route.same_runtime(target))
            .ok_or_else(|| {
                NativeError::new(codes::OC_BINDING_STALE, "the selected native route changed")
            })?;
        if expected_version.is_some_and(|version| version != route_before.route_version) {
            return Err(NativeError::new(
                codes::OC_BINDING_STALE,
                "the selected native route changed",
            ));
        }
        let _write = self.legacy_lock(&target.session_id).write_owned().await;
        let current = if force { self.current_runtime() } else { None };
        let store = self.action_store();
        let result = self.store().with(|conn| {
            if force {
                let in_flight = self.active_in_flight();
                dao::abandon_and_return_checked_conn(
                    conn,
                    &target.session_id,
                    current.as_ref(),
                    &store.epoch,
                    &in_flight,
                    now_ms(),
                    Some(target),
                    Some(&route_before),
                    confirmed_ids,
                )
            } else {
                dao::disengage_route_checked_conn(
                    conn,
                    &target.session_id,
                    now_ms(),
                    Some(target),
                    Some(&route_before),
                )
                .map(|returned| (returned, 0))
            }
        });
        result.map_err(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => {
                NativeError::new(codes::OC_BINDING_STALE, "the selected native route changed")
            }
            other => store_error(other),
        })
    }

    /// **强制交回旧路径**（评审 Blocker 2 + G7 原子性，显式运维决定）：候选集复核、
    /// 批量作废与路由降级在**同一个事务**里完成，要么全部成立、要么零变更。
    ///
    /// 语义边界：
    /// * 只作废**无法再核对**的行：有当前代际时是「存储代际 ≠ 当前代际 ∧ 未终态」；
    ///   当前代际不可证（服务被卸载 / 进程事实缺失）时是「全部未终态行」——此时没有任何
    ///   代际可比，`verify` 永远收敛不了，不作废就等于把会话永久锁死（评审 R-1）；
    /// * **本进程正在写的行例外**（`dispatching` ∧ `dispatch_epoch` = 本代次）：写可能
    ///   已在飞，作废会让「可能已写出」变成账本上的 `not_sent` 并释放占用，
    ///   由它自己收敛；
    /// * 作废后的行是 `not_sent` 墓碑（碑文 `unverifiable_generation`）：不再阻挡交回、
    ///   不可再 claim、不会重派；待答占用同事务释放；
    /// * 返回值 `(是否已归旧路径, 作废条数)`。若作废后仍有剩余未决动作（同代际行 /
    ///   本进程在飞行），交回不成立 ⇒ **整体回滚，零墓碑、路由不变**（HTTP 409）。
    pub async fn force_return_to_legacy(
        &self,
        session_id: &str,
    ) -> Result<(bool, usize), NativeError> {
        let _write = self.legacy_lock(session_id).write_owned().await;
        // 代际可证 ⇒ 只作废「换代前」的行；代际不可证（服务已卸载 / 进程事实读不出）
        // ⇒ 没有任何代际可比，该会话的每一条未终态行都无法再核对（评审 R-1）。
        let current = self.current_runtime();
        // 在飞集合必须在**同一 DB 事务内**读取（见 `abandon_and_return_with`）：
        // 事务外的快照会漏掉快照之后 claim 的写。
        self.action_store()
            .abandon_and_return_with(session_id, current.as_ref(), || self.active_in_flight())
    }

    /// reserve 之后的派发：最终复核 → claim → 组装一次 + 唯一一次写 → 回读 → finish。
    ///
    /// （R2 备注：原「后端桌面绑定门」已由统一服务身份门取代——普通启动不再要求桌面
    /// 显式连接参数；显式形态的 argv 证据保留在公开视图 `desktopBound` 字段里单列记录，
    /// 不作为写前提。`desktop_client_bound` 的 argv 校验逻辑与其单测保持不变。）
    // H5 计时参数随链传递：拆结构体会波及全部既有调用面（G1/H1 并发回归依赖
    // 逐参注入点），本层选择显式 allow。
    #[allow(clippy::too_many_arguments)]
    async fn dispatch(
        &self,
        actor: &NativeActor,
        snapshot: &DispatchSnapshot,
        command: NativeCommand,
        mut intent: Intent,
        frozen_paths: Option<Vec<String>>,
        delivery_epoch: u64,
        timing: &mut super::budget::ActionTiming,
    ) -> Result<ActionView, NativeError> {
        use std::sync::atomic::Ordering;
        let _serial = if matches!(
            command.operation,
            NativeOperation::Send(_)
                | NativeOperation::RebindOutbox(_)
                | NativeOperation::PrioritizeOutbox(_)
        ) {
            Some(
                self.outbox_runtime()
                    .session_lock(&command.session_id)
                    .lock_owned()
                    .await,
            )
        } else {
            None
        };
        let store = self.action_store();
        let current = {
            let _db = timing.span(super::budget::Segment::Database);
            store.read(&actor.device_id, &command.action_id)?
        };
        if !matches!(current.state, ActionState::NotSent | ActionState::Queued)
            || (current.state == ActionState::NotSent && current.reason_code.is_some())
            // I1（锁内重核）：后台可能在拒绝原因落账前已取走本项——拿到会话锁后
            // 重读队列态：带换窗拒绝原因的排队项是用户显式处理前的阻断态，任何
            // 后台派发路径都不得清掉原因再按当前（本可验证通过的）窗口投递。
            // 原视图原样返回（原因保留、零写出）；清除唯一合法路径 = 用户显式
            // 处理（rebind_outbox 替换撤回，DAO 侧同口径加固）。
            || (current.state == ActionState::Queued
                && current.reason_code.as_deref() == Some(codes::OC_ROUTE_WINDOW_MISMATCH))
        {
            return Ok(current);
        }
        let local_only = match &command.operation {
            NativeOperation::SetOutboxPaused(p) => p.paused,
            NativeOperation::RetractOutbox(_) => true,
            _ => false,
        };
        // 服务身份门正常在 `execute` 的 reserve 之前就拒绝；这里在 claim 之前再核一次
        // 三互证前置（R2）：覆盖「快照核验通过之后、写出之前」登记死 pid / 换代 /
        // 冒名重写登记的窗口。若真的在 reserve 之后才失效，必须走 `preclaim_failure`
        // 把本行定案（或给 queued 项记原因），否则会留下 `not_sent` 且无理由的未决行，
        // 客户端会按「确定零写出」清锚后永远看不到它。归属证明不在这里——同一次派发
        // 更靠后的写权链（echo-challenge + ACK 后终验）承担（H03 C1）。
        if !local_only {
            if let Err(err) = self.verify_target_preconditions(&snapshot.target).await {
                return resolve_dispatch_failure(store, &actor.device_id, &command.action_id, err);
            }
            // **探针目标隔离 · 派发防御**（I4）：后台队列恢复 / 重绑同查——探针会话
            // 绝不入队、绝不取得写出权（零新队列项）。
            if self.is_probe_target(&command.session_id) {
                return resolve_dispatch_failure(
                    store,
                    &actor.device_id,
                    &command.action_id,
                    NativeError::new(
                        codes::OC_GATE_PROBE_TARGET,
                        "the target session is the binding gate probe",
                    ),
                );
            }
        }
        let source = {
            let _ownership = timing.span(super::budget::Segment::Ownership);
            match self.current_source_for(&snapshot.target.session_id).await {
                Ok(source) => source,
                Err(err) => {
                    return resolve_dispatch_failure(
                        store,
                        &actor.device_id,
                        &command.action_id,
                        err,
                    )
                }
            }
        };
        if let Some(paths) = &frozen_paths {
            intent.attachment_paths = paths.clone();
        }
        let mut plan = {
            let _ownership = timing.span(super::budget::Segment::Ownership);
            match self.recheck(snapshot, &source, &command, intent).await {
                Ok(plan) => plan,
                Err(err) => {
                    return resolve_dispatch_failure(
                        store,
                        &actor.device_id,
                        &command.action_id,
                        err,
                    )
                }
            }
        };
        // C1：全部异步等待与操作特有读取结束之后、claim 之前，对**实际写通道**重读
        // `/api/info` 完整核对身份——锁窗口/读窗口内同端口接收方可能已被换成冒名服务，
        // 而登记与原 PID 事实仍然可读，仅靠重新发现（RuntimeKey 比对）拦不住。
        // G1 · 最终事务窗口 CAS 的期望值（回声验证产物）：None = 门未装配（既有
        // 测试逃逸语义，不加窗条件）；Some = (window_pid, window_boot, route_version)。
        let mut expected_window: Option<dao::ExpectedWindow> = None;
        let mut created_window = None;
        if !local_only {
            // **H5 预算检查点 ③（派发权前复核入口）**：超预算 ⇒ 不再开第二轮探针轮，
            // 尽快落确定拒绝（claim 前零写证明成立）。
            if timing.exceeded(self) {
                timing.mark_budget_refused();
                return resolve_dispatch_failure(
                    store,
                    &actor.device_id,
                    &command.action_id,
                    super::budget::budget_exhausted_error(),
                );
            }
            // **写权链 · 派发权前复核**（H03 C1 接通）：三互证前置 + echo-challenge
            // 归属证明 + ACK 后终验（锁定 TUI 同代际 + 对实际写通道重读 `/api/info`
            // 与代际锚互证——吸收原 C1 复核：同端点换接收方时只有端点自报能分辨）。
            // 位置在全部异步等待与操作特有读取结束之后、**队列冻结入队之前**：
            // 拒绝路径零新队列项（queued 项只在已在队时被记原因，绝不新增）。
            let flight_wait_before = self.gate_flight_wait_total_ms();
            let verified_window = {
                let _ownership = timing.span(super::budget::Segment::Ownership);
                match self
                    .verify_write_authority_with(&snapshot.target, &source.client)
                    .await
                {
                    Ok(window) => window,
                    Err(err) => {
                        return resolve_dispatch_failure(
                            store,
                            &actor.device_id,
                            &command.action_id,
                            err,
                        )
                    }
                }
            };
            // H5 分段计时：本轮门入口的单飞等待差分计入「排队」段。
            let waited = self.gate_flight_wait_total_ms();
            timing.add_flight_wait_ms(waited - flight_wait_before);
            // **G1 · 窗口身份与路由版本进最终事务**：把回声验证产物
            // (window_pid, window_boot) 与验证完成时刻的路由版本快照带进最终派发
            // 事务做 CAS——「回声验证 W1 后、取得派发权前同服务显式重选换到 W2」
            // 的旧请求不得再用 W1 授权写出。伴表读取失败在此处即阻断（fail-closed，
            // 不得因读失败放行——避免「换窗成功却漏升版本」的窗口）。
            if let Some(window) = &verified_window {
                let route_read = {
                    let _db = timing.span(super::budget::Segment::Database);
                    self.store()
                        .with(|conn| dao::route_conn(conn, &command.session_id))
                        .map_err(store_error)
                };
                match route_read {
                    Ok(Some(route)) => {
                        expected_window = Some(dao::ExpectedWindow {
                            window_pid: i64::from(window.pid),
                            window_boot: window.boot.clone(),
                            route_version: route.route_version,
                        });
                    }
                    // 路由行验证后消失：授权面已不在（交回/换代），零写拒绝。
                    Ok(None) => {
                        return resolve_dispatch_failure(
                            store,
                            &actor.device_id,
                            &command.action_id,
                            binding_stale(),
                        );
                    }
                    // 读取失败 ≠ 无绑定：fail-closed 阻断提交。
                    Err(err) => {
                        return resolve_dispatch_failure(
                            store,
                            &actor.device_id,
                            &command.action_id,
                            err,
                        )
                    }
                }
            }
            if command.action_id.starts_with("act_create_") {
                created_window = verified_window;
                if created_window.is_none() {
                    return resolve_dispatch_failure(
                        store,
                        &actor.device_id,
                        &command.action_id,
                        route_window_mismatch(),
                    );
                }
            }
            // G1 确定性并发回归注入点：回声验证完成后、取得派发权前（仅测试）。
            #[cfg(any(test, feature = "opencode-coordinator-tests"))]
            self.pass_dispatch_preclaim_hold().await;
        }
        // 每一次确认发送都先进入同一个 FIFO；暂停也适用于原先的即时发送入口。
        // 每一次确认发送都先进入同一个 FIFO；暂停也适用于原先的即时发送入口。
        if let Plan::Send {
            files, delivery, ..
        } = &mut plan
        {
            if *delivery != DeliveryKind::Queue {
                // steer 是直接投递，不进入本地 FIFO；暂停/撤回只约束 queue。
            } else {
                let (send, rebind_source, rebind_paths) = match &command.operation {
                    NativeOperation::Send(send) => (send.clone(), None, None),
                    NativeOperation::RebindOutbox(source) => {
                        let frozen = match self.rebind_payload(&command) {
                            Ok(frozen) => frozen,
                            Err(error) => {
                                return resolve_dispatch_failure(
                                    store,
                                    &actor.device_id,
                                    &command.action_id,
                                    error,
                                )
                            }
                        };
                        (
                            frozen.send,
                            Some(source.target_action_id.as_str()),
                            Some(frozen.attachment_paths),
                        )
                    }
                    _ => return Err(action_invalid()),
                };
                let paths = match frozen_paths.or(rebind_paths) {
                    Some(paths) => paths,
                    None => match freeze_attachments(&snapshot.target, &command.action_id, files) {
                        Ok(paths) => paths,
                        Err(err) => {
                            return resolve_dispatch_failure(
                                store,
                                &actor.device_id,
                                &command.action_id,
                                err,
                            )
                        }
                    },
                };
                *files = match verified_attachments(&snapshot.target, &paths) {
                    Ok(files) => files,
                    Err(err) => {
                        return resolve_dispatch_failure(
                            store,
                            &actor.device_id,
                            &command.action_id,
                            err,
                        )
                    }
                };
                // 入队存储失败与 claim 同路收敛（resolve_dispatch_failure）：H1 起
                // 入队事务自身带窗口 CAS，伴表读取失败（fail-closed）必须把拒绝
                // 原因落账（queued 项记原因 / not_sent 项走既有收敛），不得裸传。
                let enqueued = match self.store().with(|conn| {
                    if timing.exceeded(self) {
                        timing.mark_budget_refused();
                        return Err(super::budget::budget_exhausted_error());
                    }
                    let tx = conn.unchecked_transaction().map_err(store_error)?;
                    let enqueued =
                        crate::database::dao::opencode_outbox::enqueue_frozen_in_transaction(
                            &tx,
                            &command,
                            &paths,
                            &send,
                            rebind_source,
                            expected_window.as_ref(),
                        )
                        .map_err(store_error)?;
                    if timing.exceeded(self) {
                        timing.mark_budget_refused();
                        return Err(super::budget::budget_exhausted_error());
                    }
                    if enqueued {
                        tx.commit().map_err(store_error)?;
                    }
                    Ok(enqueued)
                }) {
                    Ok(enqueued) => enqueued,
                    Err(error) => {
                        return resolve_dispatch_failure(
                            store,
                            &actor.device_id,
                            &command.action_id,
                            error,
                        );
                    }
                };
                if !enqueued
                    && store.read(&actor.device_id, &command.action_id)?.state
                        != ActionState::Queued
                {
                    // H1（队列变更窗口 CAS）rows==0 二分：伴表/路由与验证产物**不全等**
                    // ⇒ 换窗拒绝（新项不入队 / 重绑原项不撤回不替换，拒绝原因经
                    // resolve 留存）；仍全等 ⇒ 非窗口原因（指纹不匹配 / 替换撤回失败
                    // 等既有语义），原样放弃。复核与入队 UPDATE 不同事务，但这里只做
                    // **错误归类**（写权已由入队事务的 CAS 拒绝，零写已定案），无授权面。
                    if let Some(expected) = &expected_window {
                        let drifted = !self
                            .store()
                            .with(|conn| {
                                dao::window_identity_matches_conn(
                                    conn,
                                    &command.session_id,
                                    expected,
                                )
                            })
                            .map_err(store_error)?;
                        if drifted {
                            return resolve_dispatch_failure(
                                store,
                                &actor.device_id,
                                &command.action_id,
                                route_window_mismatch(),
                            );
                        }
                    }
                    return Err(abandon(store, &command.action_id, action_invalid()));
                }
            }
        }
        if matches!(plan, Plan::LocalOutbox) {
            let result = self.store().with(|conn| {
                if !local_only && timing.exceeded(self) {
                    timing.mark_budget_refused();
                    return Err(super::budget::budget_exhausted_error());
                }
                if !self.outbox_runtime().enabled.load(Ordering::SeqCst)
                    || self.outbox_runtime().epoch.load(Ordering::SeqCst) != delivery_epoch
                    || !crate::remote::pairing::device_valid(conn, &actor.device_id, now_ms())
                {
                    return Err(action_invalid());
                }
                let tx = conn.unchecked_transaction().map_err(store_error)?;
                if !local_only
                    && !dao::route_conn(&tx, &command.session_id)
                        .map_err(store_error)?
                        .is_some_and(|route| {
                            route.route_state == dao::ROUTE_ACTIVE
                                && route.same_runtime(&snapshot.target)
                        })
                {
                    return Err(route_not_owned());
                }
                // H1（恢复派发窗口 CAS）：恢复派发（paused=false）不是本地豁免面——
                // 回声验证产物带进本事务做窗口 CAS；漂移 ⇒ rows==0 ⇒ 二分：不全等 ⇒
                // 恢复被拒（paused/队列态原样，零写，拒绝原因经 resolve 留存）；全等
                // ⇒ 输给并发派发者（既有语义：当前视图原样返回）。
                let claimed = match &expected_window {
                    Some(expected) => {
                        let claimed = dao::claim_action_with_window_in_transaction(
                            &tx,
                            &command.action_id,
                            &store.epoch,
                            now_ms(),
                            expected,
                        )
                        .map_err(store_error)?;
                        if !claimed
                            && !dao::window_identity_matches_conn(
                                &tx,
                                &command.session_id,
                                expected,
                            )
                            .map_err(store_error)?
                        {
                            return Err(route_window_mismatch());
                        }
                        claimed
                    }
                    None => dao::claim_action_in_transaction(
                        &tx,
                        &command.action_id,
                        &store.epoch,
                        now_ms(),
                    )
                    .map_err(store_error)?,
                };
                if !claimed {
                    return Ok(());
                }
                let applied = match &command.operation {
                    NativeOperation::SetOutboxPaused(p) => {
                        crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                            &tx,
                            &command.session_id,
                            &snapshot.target.directory.to_string_lossy(),
                            p.paused,
                            p.expected_revision,
                        )
                    }
                    NativeOperation::RetractOutbox(p) => {
                        crate::database::dao::opencode_outbox::retract_in_transaction(
                            &tx,
                            &actor.device_id,
                            &command.session_id,
                            &p.target_action_id,
                        )
                    }
                    _ => unreachable!(),
                }
                .map_err(store_error)?;
                let (state, reason) = if applied {
                    (ActionState::Settled, Some("local_outbox_applied"))
                } else {
                    (ActionState::NotAccepted, Some(NOT_ACCEPTED_EVIDENCE))
                };
                dao::apply_finish_conn(&tx, &command.action_id, state, None, reason, now_ms())
                    .map_err(store_error)?;
                if !local_only && timing.exceeded(self) {
                    timing.mark_budget_refused();
                    return Err(super::budget::budget_exhausted_error());
                }
                tx.commit().map_err(store_error)
            });
            if let Err(error) = result {
                if error.code == action_codes::STORE_FAILED {
                    return Err(error);
                }
                return resolve_dispatch_failure(
                    store,
                    &actor.device_id,
                    &command.action_id,
                    error,
                );
            }
            return store.read(&actor.device_id, &command.action_id);
        }
        if current.state == ActionState::Queued {
            self.store().with(|conn| {
                crate::database::dao::opencode_outbox::set_queued_reason_conn(
                    conn,
                    &command.action_id,
                    None,
                )
                .map_err(store_error)
            })?;
        }
        // 真实在飞标记**先于 claim**：force 在同一 DB 事务内读取该集合，若 claim 之后才
        // 登记，事务窗口内新 claim 的写会漏过排除（TOCTOU）。claim 失败时 Guard 立即
        // 摘除，只造成一次可忽略的保守拒绝。
        let _in_flight = self.begin_in_flight(&command.action_id);
        let claim_started = std::time::Instant::now();
        let claimed = self.store().with(|conn| {
            // **H5 预算检查点 ④（claim 事务内 · 权威闸）**：claim 是取得派发权的唯一
            // 入口——超预算后到达的派发在此拒绝授权写出（用户裁决：此后不得迟到取得
            // 派发权）。此刻零原生写（写只发生在 claim 提交之后），拒绝经既有
            // resolve_dispatch_failure 落 not_sent 墓碑 = 可核对的确定未发送结论。
            if timing.exceeded(self) {
                timing.mark_budget_refused();
                return Err(super::budget::budget_exhausted_error());
            }
            if !self.outbox_runtime().enabled.load(Ordering::SeqCst)
                || self.outbox_runtime().epoch.load(Ordering::SeqCst) != delivery_epoch
                || !crate::remote::pairing::device_valid(conn, &actor.device_id, now_ms())
            {
                return Ok(false);
            }
            let tx = conn.unchecked_transaction().map_err(store_error)?;
            let action_matches = dao::get_action_conn(&tx, &command.action_id)
                .map_err(store_error)?
                .is_some_and(|row| {
                    row.device_id == actor.device_id
                        && row_belongs_to_current(&row, &snapshot.target)
                });
            let route_matches = dao::route_conn(&tx, &command.session_id)
                .map_err(store_error)?
                .is_some_and(|route| {
                    route.route_state == dao::ROUTE_ACTIVE && route.same_runtime(&snapshot.target)
                });
            if !action_matches || !route_matches {
                return Err(binding_stale());
            }
            if let Some(window) = &created_window {
                let original = crate::database::dao::opencode_creates::first_sentence_target(
                    &tx,
                    &actor.device_id,
                    &command.action_id,
                )
                .map_err(store_error)?;
                let pinned = crate::database::dao::opencode_pinned_windows::pinned_window_conn(
                    &tx,
                    &command.session_id,
                )
                .map_err(store_error)?;
                if !original
                    .is_some_and(|record| record.matches_bound_window(&snapshot.target, window))
                    || !pinned.is_some_and(|pin| {
                        pin.window_pid == i64::from(window.pid)
                            && pin.window_started_at_ms == window.started_at_ms
                            && pin.window_boot == window.boot
                    })
                {
                    return Err(route_window_mismatch());
                }
            }
            let claimed = match &expected_window {
                // G1：带窗口 CAS 的 claim——「回声验证后、取得派权前」伴表窗口身份
                // 或路由版本漂移 ⇒ UPDATE 落空（rows==0）。伴表缺失/读取失败经 SQL
                // 错误原样传播（fail-closed，不得因读失败放行）。
                Some(expected) => {
                    let claimed = dao::claim_action_with_window_in_transaction(
                        &tx,
                        &command.action_id,
                        &store.epoch,
                        now_ms(),
                        expected,
                    )
                    .map_err(store_error)?;
                    if !claimed {
                        // rows==0 二分（H1 起与队列入队/恢复派发共用同一复核）：
                        // 伴表当前值与验证产物**不全等** ⇒ 换窗拒绝
                        //（OC_ROUTE_WINDOW_MISMATCH）；伴表读取 Err ⇒ store_error
                        // fail-closed；**全等** ⇒ 输给并发派发者（既有语义：返回当前
                        // 视图，绝不二次写）。注意复核也在同一事务内（与 UPDATE 同一
                        // 快照，不存在中间窗口）。
                        let unchanged =
                            dao::window_identity_matches_conn(&tx, &command.session_id, expected)
                                .map_err(store_error)?;
                        if !unchanged {
                            return Err(route_window_mismatch());
                        }
                    }
                    claimed
                }
                None => dao::claim_action_in_transaction(
                    &tx,
                    &command.action_id,
                    &store.epoch,
                    now_ms(),
                )
                .map_err(store_error)?,
            };
            // **Task C（插队）· 同一事务内提升目标队列行**：本控制动作取得写权的同时，
            // 把**既有那一行**从 `queued` 提升为 `dispatching`（清冻结载荷、修订 +1）。
            // 一次插队 = 一个 actionId = 最多一次派发：目标行此后既不可被 FIFO 选中
            // （`state` 不再是 queued），也不会被再次插队（`claim_prioritized_in_transaction`
            // 只认 queued）。目标已不可接管（并发赢者/已终态/暂停/别人的行）⇒ 整个事务
            // 回滚 = 零写：本控制动作自己也不留行（调用方按既有语义定案拒绝）。
            if claimed {
                if let Some(target) = prioritize_target_action_id(&command.operation) {
                    let promoted =
                        crate::database::dao::opencode_outbox::claim_prioritized_in_transaction(
                            &tx,
                            &actor.device_id,
                            &command.session_id,
                            &snapshot.target.directory.to_string_lossy(),
                            target,
                            &store.epoch,
                            now_ms(),
                        )
                        .map_err(store_error)?;
                    if !promoted {
                        // 回滚整个事务（含本动作自己的 claim）：目标已不在队列。
                        return Err(NativeError::new(
                            action_codes::CONFLICT,
                            "native action conflicts with a live one",
                        ));
                    }
                }
            }
            // **I2 仅测试 · 写权后持锁缝**：写权（claim UPDATE）已在握、提交未发生
            // ——真实链此窗口内坐着写锁/落盘等待面；测试经此闸把链确定性挂过统一
            // 预算截止（闭包 sync 不可 await，自旋等放行）。缝在复核 ⑤ 之前 = 忠实
            // 模拟「等待先发生、复核在提交前最后一刻判定」的真实次序。
            #[cfg(any(test, feature = "opencode-coordinator-tests"))]
            if let Some(hold) = self.claim_post_write_test_hold() {
                hold.pass_sync();
            }
            // **I2 · 预算检查点 ⑤（写权后复核 · 双层第二层）**：claim UPDATE 成功 =
            // 写权在握，但闸 ④ 到本提交之间坐着真实的等待面（外部写锁对峙、落盘
            // fsync 等）——等待可能把链拖过统一预算截止。提交前对**同一绝对截止**
            // 再核一次（同源：同一 `started` 单调起点 + 生产恒定预算常量 = 同一
            // deadline 值；测试注入缝活读仅用于确定性截断，见
            // `budget::ActionTiming::exceeded`）。超时 ⇒ 事务内 Err（tx drop = 回滚：
            // 零提交、授权面不存在），此后 resolve_dispatch_failure 落 not_sent 墓碑
            //（budget_exhausted）= 可核对的确定未发送结论；claim 之后才发生的原生写
            //（POST /interrupt）零发出——与闸 ④（取写权前）、发包前复核 ⑥（fix1，
            // 提交后·Unknown 族）构成三层防御：④ 快路径拒，⑤ 慢路径兜底零提交，
            // ⑥ 兜底零发包。
            if claimed && timing.exceeded(self) {
                timing.mark_budget_refused();
                return Err(super::budget::budget_exhausted_error());
            }
            tx.commit().map_err(store_error)?;
            Ok(claimed)
        });
        timing.add_ms(
            claim_started.elapsed().as_millis() as i64,
            super::budget::Segment::Database,
        );
        let claimed = match claimed {
            Ok(claimed) => claimed,
            Err(error) => {
                return resolve_dispatch_failure(store, &actor.device_id, &command.action_id, error)
            }
        };
        if !claimed {
            // 并发派发已取得写出权（或已成墓碑）：返回当前视图，绝不二次写。
            let _db = timing.span(super::budget::Segment::Database);
            return store.read(&actor.device_id, &command.action_id);
        }
        // **I2 fix1 仅测试 · 提交后持锁缝**：claim 已提交、原生请求未开始——真实链
        // 此窗口内坐着提交落盘/调度等待面；测试经此闸把链确定性挂过统一预算截止
        //（同步自旋，消费形态与 `ClaimPostWriteHold` 同纪律）。
        #[cfg(any(test, feature = "opencode-coordinator-tests"))]
        if let Some(hold) = self.claim_post_commit_test_hold() {
            hold.pass_sync();
        }
        // **I2 fix1 · 预算检查点 ⑥（发包前复核 · 三层第三层）**：claim 已提交 =
        // 授权面已成账本事实（「可能已发出」标记），但提交本身/提交后调度仍可能把
        // 链拖过统一预算截止——⑤ 之后到 write_native 之间是最后一段零包窗口。原生
        // 请求开始前对**同一绝对截止**再核（同源不重算，同 ④⑤）。跨截止 ⇒ 不开始
        // write_native（零包发出）。诚实分级（不冒充确定）：claim 提交后账本边界按
        // 设计不可判定——状态机明文「dispatching 之后不得回退成 not_sent」（重启恢复
        // 同口径：dispatching → unconfirmed 绝不回 not_sent），故本层落 **Unknown 族**
        // 结论：`Unconfirmed` + 碑文 `OC_DISPATCH_BUDGET_EXHAUSTED`，只按原 action ID
        // 核对（与丢回执同一既有语义，200 视图）；not_sent 墓碑分支属于 ④（取写权
        // 前）与 ⑤（提交前·事务内回滚）——前两层零改动。
        if timing.exceeded(self) {
            timing.mark_budget_refused();
            let _db = timing.span(super::budget::Segment::Database);
            store.finish(
                &command.action_id,
                ActionState::Unconfirmed,
                None,
                Some(super::budget::OC_DISPATCH_BUDGET_EXHAUSTED),
            )?;
            let view = store.read(&actor.device_id, &command.action_id)?;
            log::info!(
                "OC_DISPATCH_BUDGET_CUTOFF post-commit: action={} crossing budget before native write (zero packets), finalized unconfirmed",
                command.action_id
            );
            return Ok(view);
        }
        let reply = {
            let _dispatch = timing.span(super::budget::Segment::Dispatch);
            self.write_native(actor, &source, snapshot, &command, &plan)
                .await
        };
        self.settle(actor, &source, snapshot, &command, &plan, reply, timing)
            .await
    }

    /// 派发前最终复核：绑定 / 代际 / 能力 / 归属 / 当前请求 / 附件。
    ///
    /// 全部在 claim 之前：复核不过就记墓碑放弃，绝不把「可能已发出」写进账本。
    async fn recheck(
        &self,
        snapshot: &DispatchSnapshot,
        source: &Source,
        command: &NativeCommand,
        intent: Intent,
    ) -> Result<Plan, NativeError> {
        // 绑定令牌：命令携带的必须与当前投影逐字相等。
        if command.binding_token != snapshot.binding_token {
            return Err(binding_stale());
        }
        // 投影与写通道必须同一代际（同端口换 PID/id 即新代际，旧绑定失效）。
        if source.descriptor.runtime != snapshot.target.runtime {
            return Err(binding_stale());
        }
        if command.session_id != snapshot.target.session_id {
            return Err(NativeError::new(
                codes::OC_SESSION_MISMATCH,
                "native command session does not match the bound target",
            ));
        }
        // 纯本地安全操作只约束兔维斯尚未派发的项，不产生原生写，也不要求本端
        // 拥有原生写入口；恢复派发/重绑/原生写仍在下面按路由归属拒绝。
        match &command.operation {
            NativeOperation::SetOutboxPaused(p) if p.paused => return Ok(Plan::LocalOutbox),
            NativeOperation::RetractOutbox(_) => return Ok(Plan::LocalOutbox),
            _ => {}
        }
        // 归属：本端必须拥有该会话的原生写入入口（draining/未选中都不算）。
        if !self
            .action_store()
            .owns_session(&snapshot.target.session_id)?
        {
            return Err(route_not_owned());
        }
        match &command.operation {
            // 已在上方提前返回；保留防御臂避免 future 重排时漏编译。
            NativeOperation::SetOutboxPaused(_) | NativeOperation::RetractOutbox(_) => {
                Ok(Plan::LocalOutbox)
            }
            NativeOperation::StopSession(_) => {
                require_capability(snapshot, NativeOperationId::StopSession)?;
                Ok(Plan::Stop)
            }
            NativeOperation::SetAgent(payload) => {
                require_capability(snapshot, NativeOperationId::SetAgent)?;
                if !is_agent_id(&payload.agent) {
                    return Err(action_invalid());
                }
                // 只允许空闲切换：读会话权威 time.busy/time.idle；读不到或正在执行
                // 一律拒绝（零原生写），不在忙时并发切换。
                let session_path =
                    format!("/api/session/{}", segment(&snapshot.target.session_id)?);
                let state = source.client.read_json(&session_path, &[]).await?;
                let root = state.get("data").unwrap_or(&state);
                let busy = root.pointer("/time/busy").and_then(Value::as_i64);
                let idle = root.pointer("/time/idle").and_then(Value::as_i64);
                // 新会话只有 created/updated（没有 busy/idle）也是空闲；只有 busy
                // 存在且没有更晚的 idle 才算正在执行。
                if session_busy_from_times(busy, idle) {
                    return Err(session_busy());
                }
                // 目标必须来自本实例当前 primary 选项；不用显示名/任意字符串。
                let agents = source
                    .client
                    .read_json(
                        "/api/agent",
                        &[(
                            "location[directory]".to_string(),
                            snapshot.target.directory.to_string_lossy().into_owned(),
                        )],
                    )
                    .await?;
                let valid = agents
                    .get("data")
                    .and_then(Value::as_array)
                    .is_some_and(|items| {
                        items.iter().any(|item| {
                            item.get("id").and_then(Value::as_str) == Some(payload.agent.as_str())
                                && item.get("mode").and_then(Value::as_str) == Some("primary")
                                && item.get("hidden").and_then(Value::as_bool) != Some(true)
                        })
                    });
                if !valid {
                    return Err(command_unsupported());
                }
                Ok(Plan::SetAgent {
                    agent: payload.agent.clone(),
                })
            }
            NativeOperation::RebindOutbox(_) => {
                require_capability(snapshot, NativeOperationId::Send)?;
                let frozen = self.rebind_payload(command)?;
                Ok(Plan::Send {
                    text: frozen.send.text,
                    files: verified_attachments(&snapshot.target, &frozen.attachment_paths)?,
                    delivery: frozen.send.delivery,
                    client_message_id: client_message_id_of(&command.action_id),
                    prioritize_target: None,
                })
            }
            NativeOperation::PrioritizeOutbox(source) => {
                // 插队 = 「把既有队列行提前派发」：能力仍是 send（同一条 POST /prompt）。
                require_capability(snapshot, NativeOperationId::Send)?;
                let frozen = self.frozen_outbox_row(command)?;
                Ok(Plan::Send {
                    text: frozen.send.text,
                    // 附件用原行冻结的只读副本；不改、不重新冻结。
                    files: verified_attachments(&snapshot.target, &frozen.attachment_paths)?,
                    // Task C：唯一被允许的改动——投递方式 queue → steer。OpenCode 的
                    // steer 自带打断当前回合，因此**不**先调 stop_session。
                    delivery: DeliveryKind::Steer,
                    // 关联 id 必须是原行的：这条消息就是原队列行本身，不是新消息。
                    client_message_id: client_message_id_of(&source.target_action_id),
                    prioritize_target: Some(source.target_action_id.clone()),
                })
            }
            NativeOperation::Send(payload) => {
                require_capability(snapshot, NativeOperationId::Send)?;
                let files = verified_attachments(&snapshot.target, &intent.attachment_paths)?;
                Ok(Plan::Send {
                    text: payload.text.clone(),
                    files,
                    delivery: payload.delivery,
                    client_message_id: client_message_id_of(&command.action_id),
                    prioritize_target: None,
                })
            }
            NativeOperation::Answer(payload) => {
                // 缺 pending 读能力时，「列表里没有」不能当成「已定案」。
                require_capability(snapshot, NativeOperationId::AnswerGroup)?;
                require_capability(snapshot, NativeOperationId::ReadPending)?;
                let current = pending_form_token(snapshot, &payload.form_id)?;
                // 权威状态只来自 detail（列表项的 state 恒为 unknown）。
                let detail =
                    read_form_detail(&source.client, &snapshot.target, &payload.form_id).await?;
                match form_state_of(detail.status.as_deref()) {
                    FormState::Pending => {}
                    // 已支持终态：只说已定案（fail-closed，不再写出）。
                    FormState::Answered | FormState::Cancelled => return Err(request_settled()),
                    // 未知状态词 / 服务端没给状态：不放行，但不谎称「已定案」。
                    FormState::Unknown => {
                        return Err(NativeError::new(
                            codes::OC_CONTRACT_MISSING,
                            "native form detail carries no supported authoritative state",
                        ))
                    }
                }
                // R3：答案必须先过整组校验，且上下文必须绑定「这个表单 + 当前 binding」。
                if !same_form_context_for(
                    &detail.spec,
                    &snapshot.binding_token,
                    &payload.context_token,
                    &current,
                ) {
                    return Err(binding_stale());
                }
                let answer = validate_answers(&detail.spec, &payload.answers)?;
                Ok(Plan::Answer {
                    form_id: payload.form_id.clone(),
                    answer,
                })
            }
            NativeOperation::Decide(payload) => {
                // T7 未完成前不开放永久授权；always 必须在任何读 / 写之前拒绝。
                if payload.decision == "always" {
                    return Err(command_unsupported());
                }
                let capability = match payload.decision.as_str() {
                    "once" => NativeOperationId::PermissionOnce,
                    "reject" => NativeOperationId::PermissionReject,
                    _ => return Err(command_unsupported()),
                };
                require_capability(snapshot, capability)?;
                require_capability(snapshot, NativeOperationId::ReadPending)?;
                let current = pending_permission_token(snapshot, &payload.request_id)?;
                if payload.context_token != current {
                    return Err(binding_stale());
                }
                let saved_rule = if payload.decision == "always" {
                    let detail = source
                        .client
                        .read_json(
                            &format!(
                                "/api/session/{}/permission/{}",
                                segment(&snapshot.target.session_id)?,
                                segment(&payload.request_id)?
                            ),
                            &[],
                        )
                        .await?;
                    let data = detail.get("data").unwrap_or(&detail);
                    let action = data.get("action").and_then(Value::as_str);
                    let resources = data
                        .get("resources")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    match action {
                        Some(action) if !resources.is_empty() => {
                            Some((action.to_string(), resources))
                        }
                        _ => return Err(command_unsupported()),
                    }
                } else {
                    None
                };
                Ok(Plan::Decide {
                    request_id: payload.request_id.clone(),
                    decision: payload.decision.clone(),
                    saved_rule,
                })
            }
            NativeOperation::Retract(payload) => {
                require_capability(snapshot, NativeOperationId::Retract)?;
                Ok(Plan::Retract {
                    item_id: payload.item_id.clone(),
                })
            }
            NativeOperation::PromoteInbox(payload) => {
                require_capability(snapshot, NativeOperationId::PromoteInbox)?;
                Ok(Plan::Promote {
                    item_id: payload.item_id.clone(),
                })
            }
        }
    }

    /// 组装一次（仅新动作）+ 唯一一次网络写。DB 锁外。
    async fn write_native(
        &self,
        actor: &NativeActor,
        source: &Source,
        snapshot: &DispatchSnapshot,
        _command: &NativeCommand,
        plan: &Plan,
    ) -> Result<NativeWriteReply, NativeError> {
        let session = segment(&snapshot.target.session_id)?;
        match plan {
            Plan::LocalOutbox => Err(action_invalid()),
            Plan::Send {
                text,
                files,
                delivery,
                client_message_id,
                ..
            } => {
                // 签名设置经既有单点现读（`remote_message_signature`，缺省 off）。
                let signature = self.store().with(message_signature_enabled_conn);
                let wire = compose_injection_flagged(&actor.device_name, text, signature);
                let mut body = json!({
                    "id": client_message_id,
                    "text": wire,
                    "delivery": match delivery {
                        DeliveryKind::Queue => "queue",
                        DeliveryKind::Steer => "steer",
                    },
                });
                if !files.is_empty() {
                    body["files"] = Value::Array(
                        files
                            .iter()
                            .map(|file| json!({ "uri": file.uri, "name": file.name }))
                            .collect(),
                    );
                }
                source
                    .client
                    .write_once(&format!("/api/session/{session}/prompt"), &body)
                    .await
            }
            Plan::Answer { form_id, answer } => {
                let form = segment(form_id)?;
                source
                    .client
                    .write_once(
                        &format!("/api/session/{session}/form/{form}/reply"),
                        &json!({ "answer": answer }),
                    )
                    .await
            }
            Plan::Decide {
                request_id,
                decision,
                ..
            } => {
                let request = segment(request_id)?;
                source
                    .client
                    .write_once(
                        &format!("/api/session/{session}/permission/{request}/reply"),
                        &json!({ "decision": decision }),
                    )
                    .await
            }
            Plan::Retract { item_id } => {
                let item = segment(item_id)?;
                source
                    .client
                    .delete_once(&format!("/api/session/{session}/inbox/{item}"))
                    .await
            }
            // 插队 = 把**既有** inbox 项改投递为 steer（PATCH，只有 delivery 一个字段）。
            // 不重发正文：上游会把这同一项在当前回合交付并打断。
            Plan::Promote { item_id } => {
                let item = segment(item_id)?;
                source
                    .client
                    .patch_once(
                        &format!("/api/session/{session}/inbox/{item}"),
                        &json!({ "delivery": "steer" }),
                    )
                    .await
            }
            Plan::Stop => {
                source
                    .client
                    .write_once(&format!("/api/session/{session}/interrupt"), &json!({}))
                    .await
            }
            Plan::SetAgent { agent } => {
                source
                    .client
                    .write_once(
                        &format!("/api/session/{session}/agent"),
                        &json!({ "agent": agent }),
                    )
                    .await
            }
        }
    }

    /// 回读定性 + 落账终态（唯一收尾处）。
    ///
    /// 回读 = **写之后**的一次新鲜投影（`records`/`inbox`/`pending` 就是为这一步而备）；
    /// 回读定性 + 落账终态（唯一收尾处）。
    ///
    /// 回读 = **写之后**的一次新鲜投影（`records`/`inbox`/`pending` 就是为这一步而备），
    /// 且它必须与**执行这次写的代际**（`source`，即发出 POST 的那个 client 所属代际）
    /// 逐项相同：换代际后旧绑定失效，新代际的空 pending / 空 inbox / 空消息列表不能
    /// 证明旧写入的结果（计划审查重点 #2）——那种回读一律按失败处理，只记 `Unconfirmed`。
    /// 表单的权威状态仍走 detail 端点（列表不携带 `state.status`）。
    #[allow(clippy::too_many_arguments)]
    async fn settle(
        &self,
        actor: &NativeActor,
        source: &Source,
        snapshot: &DispatchSnapshot,
        command: &NativeCommand,
        plan: &Plan,
        reply: Result<NativeWriteReply, NativeError>,
        timing: &mut super::budget::ActionTiming,
    ) -> Result<ActionView, NativeError> {
        // 停止会话的应答本身就是服务端权威结果（`interrupted`），不做消息回读：
        // true = 收到时确有活动回合被停；false = 收到时已空闲（同样是确定结果）。
        if let Plan::SetAgent { agent } = plan {
            let outcome = {
                let _readback = timing.span(super::budget::Segment::Readback);
                match reply {
                    Ok(_) => {
                        let directory = snapshot.target.directory.to_string_lossy().into_owned();
                        let card = session_card(&snapshot.target.session_id, &directory);
                        match self.dispatch_snapshot(&card).await {
                            Ok(fresh) if fresh.target.runtime == source.descriptor.runtime => {
                                let path = format!(
                                    "/api/session/{}",
                                    segment(&snapshot.target.session_id)?
                                );
                                match source.client.read_json(&path, &[]).await {
                                    Ok(value) => {
                                        let root = value.get("data").unwrap_or(&value);
                                        if root.get("agent").and_then(Value::as_str)
                                            == Some(agent.as_str())
                                        {
                                            Outcome::terminal(
                                                ActionState::Settled,
                                                None,
                                                Some("agent_switched"),
                                            )
                                        } else {
                                            Outcome::unconfirmed(Some(
                                                reason_words::READBACK_FAILED.to_string(),
                                            ))
                                        }
                                    }
                                    Err(_) => Outcome::unconfirmed(Some(
                                        reason_words::READBACK_FAILED.to_string(),
                                    )),
                                }
                            }
                            _ => Outcome::unconfirmed(Some(
                                reason_words::READBACK_FAILED.to_string(),
                            )),
                        }
                    }
                    Err(err) => Outcome::unconfirmed(Some(err.code)),
                }
            };
            return self
                .finish_quietly(actor, outcome, &command.action_id, timing)
                .await;
        }
        if matches!(plan, Plan::Stop) {
            // H5 分段计时「回读」段：stop 的结果定性 = 应答体权威判定
            //（`interrupted` true/false/缺席），无额外投影回读——段耗时覆盖应答定性面。
            let outcome = {
                let _readback = timing.span(super::budget::Segment::Readback);
                match reply {
                    Ok(reply) => match reply
                        .body
                        .as_ref()
                        .and_then(|body| body.get("interrupted"))
                        .and_then(Value::as_bool)
                    {
                        Some(true) => {
                            Outcome::terminal(ActionState::Settled, None, Some("interrupted"))
                        }
                        Some(false) => {
                            Outcome::terminal(ActionState::Settled, None, Some("already_idle"))
                        }
                        None => {
                            Outcome::unconfirmed(Some(reason_words::READBACK_FAILED.to_string()))
                        }
                    },
                    Err(err) => Outcome::unconfirmed(Some(err.code)),
                }
            };
            return self
                .finish_quietly(actor, outcome, &command.action_id, timing)
                .await;
        }
        // **Task C（插队）**：本次派发的账本载体是**原队列行**，不是控制动作自己——
        // 「一次插队 = 一个 actionId = 最多一次派发」要求结果落在被派发的那一行上，
        // 否则原行会带着 `dispatching` 永远悬空。
        if let Plan::Send {
            prioritize_target: Some(target),
            client_message_id,
            ..
        } = plan
        {
            return self
                .settle_prioritized(
                    actor,
                    source,
                    snapshot,
                    command,
                    target,
                    client_message_id,
                    reply,
                    timing,
                )
                .await;
        }
        let outcome = match reply {
            // 写调用本身失败：服务端是否收到不可知 —— 只记未知，绝不重试。
            Err(err) => Outcome::unconfirmed(Some(err.code)),
            Ok(reply) => {
                let directory = snapshot.target.directory.to_string_lossy().into_owned();
                let card = session_card(&snapshot.target.session_id, &directory);
                let Ok(fresh) = self.dispatch_snapshot(&card).await else {
                    // 回读失败：无法用权威投影定性。
                    return self
                        .finish_quietly(
                            actor,
                            Outcome::unconfirmed(Some(reason_words::READBACK_FAILED.to_string())),
                            &command.action_id,
                            timing,
                        )
                        .await;
                };
                if fresh.target.runtime != source.descriptor.runtime {
                    // 回读来自**另一个代际**：它的空状态不是证据，绝不据此落 Confirmed/Settled。
                    Outcome::unconfirmed(Some(reason_words::READBACK_FAILED.to_string()))
                } else {
                    match plan {
                        Plan::LocalOutbox => {
                            Outcome::unconfirmed(Some(reason_words::READBACK_FAILED.to_string()))
                        }
                        Plan::Send {
                            client_message_id, ..
                        } => send_outcome(&fresh, client_message_id, reply),
                        Plan::Answer { form_id, .. } => {
                            answer_outcome(source, &fresh.target, form_id, reply).await
                        }
                        Plan::Decide { request_id, .. } => {
                            decide_outcome(&fresh, request_id, reply)
                        }
                        Plan::Retract { item_id } => retract_outcome(&fresh, item_id, reply),
                        // 插队的回读判据：该项**仍在队列**且 delivery 已变为 steer ⇒ 已受理；
                        // 若已不在队列，说明上游已消费（同样算交付成功，由正式记录佐证）。
                        Plan::Promote { item_id } => promote_outcome(&fresh, item_id, reply),
                        // Stop/SetAgent 已在上方用服务端应答或会话读回直接定案。
                        Plan::Stop | Plan::SetAgent { .. } => {
                            Outcome::unconfirmed(Some(reason_words::READBACK_FAILED.to_string()))
                        }
                    }
                }
            }
        };
        let outcome = if matches!(outcome.state, ActionState::Confirmed | ActionState::Settled) {
            if let Plan::Decide {
                decision,
                saved_rule: Some((action, resources)),
                ..
            } = plan
            {
                if decision == "always"
                    && !self
                        .saved_permission_rule_exists(source, &snapshot.target, action, resources)
                        .await
                {
                    Outcome::unconfirmed(Some(reason_words::READBACK_FAILED.to_string()))
                } else {
                    outcome
                }
            } else {
                outcome
            }
        } else {
            outcome
        };
        self.finish_quietly(actor, outcome, &command.action_id, timing)
            .await
    }

    /// **插队的收尾**（Task C）：一次上游写，两个账本行各归各位。
    ///
    /// * **原队列行**（`target`）= 这次写的真正载体：按与普通 send 完全相同的
    ///   回读定性（同一 `client_message_id`）落 `confirmed`/`acknowledged`/
    ///   `unconfirmed`。它在 claim 事务里已被提升为 `dispatching`，所以这次落账
    ///   走的是既有 `Dispatching → …` 迁移，不新增状态机臂。
    /// * **插队控制动作本身**（`command.action_id`）= `settled` +
    ///   [`reason_words::STEERED_INTERRUPT`]，两个分支都只落这一次终态：
    ///   原行得到确定结论 ⇒ `settled/steered_interrupt`；原行结论未知 ⇒ `settled`
    ///   也带同一词（插队的**派发**这一步已被受理，未知的是原行自己的投递结果，
    ///   它有自己的 `unconfirmed` 与核对出口）。本端不把「打断发生了」当结论——
    ///   steer 是原生语义，本端无从也不声称回合停在哪一刻。
    ///
    /// 零新增写入口：仍然只有 claim 之后那一次 `write_native`。
    #[allow(clippy::too_many_arguments)]
    async fn settle_prioritized(
        &self,
        actor: &NativeActor,
        source: &Source,
        snapshot: &DispatchSnapshot,
        command: &NativeCommand,
        target: &str,
        client_message_id: &str,
        reply: Result<NativeWriteReply, NativeError>,
        timing: &mut super::budget::ActionTiming,
    ) -> Result<ActionView, NativeError> {
        // 写调用本身失败：服务端是否收到不可知 —— 原行只记未知，绝不重试。
        let outcome = match reply {
            Err(err) => Outcome::unconfirmed(Some(err.code)),
            Ok(reply) => {
                let directory = snapshot.target.directory.to_string_lossy().into_owned();
                let card = session_card(&snapshot.target.session_id, &directory);
                let Ok(fresh) = self.dispatch_snapshot(&card).await else {
                    return self
                        .finish_prioritized_rows(
                            actor,
                            command.action_id.as_str(),
                            target,
                            Outcome::unconfirmed(Some(reason_words::READBACK_FAILED.to_string())),
                            timing,
                        )
                        .await;
                };
                if fresh.target.runtime != source.descriptor.runtime {
                    // 回读来自另一个代际：它的空状态不是证据。
                    Outcome::unconfirmed(Some(reason_words::READBACK_FAILED.to_string()))
                } else {
                    send_outcome(&fresh, client_message_id, reply)
                }
            }
        };
        self.finish_prioritized_rows(actor, command.action_id.as_str(), target, outcome, timing)
            .await
    }

    /// 插队的双行落账（同一 DB 临界区，顺序固定）：先原行（结论），后控制动作。
    ///
    /// 控制动作的结论**跟随原行的证据**（不冒充）：
    /// * 原行 `confirmed`/`acknowledged` ⇒ 插队确实被原生受理 ⇒ `settled` +
    ///   [`reason_words::STEERED_INTERRUPT`]（用户可判定的「会打断」信号）。
    /// * 原行仍是未知（`unconfirmed`）⇒ 控制动作同样 `unconfirmed` + 同一理由码：
    ///   只收到受理/根本没读到回执时，不得声称插队已生效。核对出口在原行上。
    async fn finish_prioritized_rows(
        &self,
        actor: &NativeActor,
        control_action_id: &str,
        target: &str,
        target_outcome: Outcome,
        timing: &mut super::budget::ActionTiming,
    ) -> Result<ActionView, NativeError> {
        let store = self.action_store();
        let control = if matches!(
            target_outcome.state,
            ActionState::Confirmed | ActionState::Acknowledged
        ) {
            Outcome::terminal(
                ActionState::Settled,
                None,
                Some(reason_words::STEERED_INTERRUPT),
            )
        } else {
            Outcome::unconfirmed(
                target_outcome
                    .reason
                    .clone()
                    .or_else(|| Some(reason_words::READBACK_FAILED.to_string())),
            )
        };
        {
            let _db = timing.span(super::budget::Segment::Database);
            store.finish(
                target,
                target_outcome.state,
                target_outcome.native_id.as_deref(),
                target_outcome.reason.as_deref(),
            )?;
            store.finish(
                control_action_id,
                control.state,
                control.native_id.as_deref(),
                control.reason.as_deref(),
            )?;
        }
        store.read(&actor.device_id, control_action_id)
    }

    /// `always` 的收尾证据：/api/permission/saved 必须出现同 action+resource 的规则。
    async fn saved_permission_rule_exists(
        &self,
        source: &Source,
        target: &super::types::NativeTarget,
        action: &str,
        resources: &[String],
    ) -> bool {
        let query = [(
            "location[directory]".to_string(),
            target.directory.to_string_lossy().into_owned(),
        )];
        let Ok(value) = source
            .client
            .read_json("/api/permission/saved", &query)
            .await
        else {
            return false;
        };
        value
            .get("data")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("action").and_then(Value::as_str) == Some(action)
                        && item
                            .get("resource")
                            .and_then(Value::as_str)
                            .is_some_and(|resource| resources.iter().any(|r| r == resource))
                })
            })
    }

    /// 落账终态并返回最新视图。
    async fn finish_quietly(
        &self,
        actor: &NativeActor,
        outcome: Outcome,
        action_id: &str,
        timing: &mut super::budget::ActionTiming,
    ) -> Result<ActionView, NativeError> {
        let store = self.action_store();
        let read = {
            // H5 分段计时：终态落账属「数据库等待」段。
            let _db = timing.span(super::budget::Segment::Database);
            store.finish(
                action_id,
                outcome.state,
                outcome.native_id.as_deref(),
                outcome.reason.as_deref(),
            )?;
            store.read(&actor.device_id, action_id)
        };
        read
    }
}

/// 派发前复核拒绝：记墓碑（证明本动作从未派发）后交出原错误。
fn abandon(store: &NativeActionStore, action_id: &str, err: NativeError) -> NativeError {
    let reason = abandon_reason(&err.code);
    if let Err(finish) = store.finish(action_id, ActionState::NotSent, None, Some(reason)) {
        log::error!("opencode native action abandon failed: {finish:?}");
    }
    err
}

/// 复核错误码 → 放弃理由词（受控词表，不在表内的落 `PRECHECK`）。
fn abandon_reason(code: &str) -> &'static str {
    match code {
        codes::OC_RUNTIME_UNVERIFIED
        | codes::OC_SERVICE_NOT_REGISTERED
        | codes::OC_SERVICE_REGISTRATION_INVALID
        | codes::OC_GENERATION_MISMATCH
        | codes::OC_VERSION_MISMATCH
        | codes::OC_INSTANCE_AMBIGUOUS => abandon_reasons::IDENTITY_UNVERIFIED,
        // H03 echo-challenge 写门：全部 `OC_GATE_*` 受控码共享同一碑文归类
        // （对外错误码保留具体原因，词表只证明「零写出零预留」）。
        codes::OC_GATE_NO_ACK
        | codes::OC_GATE_LATE_ACK
        | codes::OC_GATE_PID_MISMATCH
        | codes::OC_GATE_NONCE_MISMATCH
        | codes::OC_GATE_TITLE_INVALID
        | codes::OC_GATE_PROBE_UNWRITABLE
        | codes::OC_GATE_UNREACHABLE
        | codes::OC_GATE_BUSY
        | codes::OC_GATE_PID_UNVERIFIED => abandon_reasons::BINDING_UNVERIFIED,
        codes::OC_DISPATCH_BUDGET_EXHAUSTED => abandon_reasons::BUDGET_EXHAUSTED,
        codes::OC_BINDING_STALE => abandon_reasons::BINDING_STALE,
        codes::OC_CONTRACT_MISSING | codes::OC_CONTRACT_ID_UNSUPPORTED => {
            abandon_reasons::CAPABILITY
        }
        codes::OC_ROUTE_NOT_OWNED => abandon_reasons::ROUTE_NOT_OWNED,
        codes::OC_REQUEST_SETTLED => abandon_reasons::REQUEST_SETTLED,
        codes::OC_ANSWER_INCOMPLETE
        | codes::OC_ANSWER_INVALID
        | codes::OC_ANSWER_UNSUPPORTED
        | codes::OC_FORM_INVALID => abandon_reasons::ANSWER_INVALID,
        codes::OC_ATTACHMENT_REFUSED => abandon_reasons::ATTACHMENT_REFUSED,
        // H1：窗口 CAS 拒绝（换窗后旧请求零写）有专属碑文——拒绝原因留存可查询。
        codes::OC_ROUTE_WINDOW_MISMATCH => abandon_reasons::ROUTE_WINDOW_MISMATCH,
        _ => abandon_reasons::PRECHECK,
    }
}

/// 静态完整性校验（无 IO、无账本、无网络）：动作 id、关联 id、投递词、决定词、
/// 斜杠命令、正文空/超长、id 形态、附件路径形态。
fn intent_of(command: &NativeCommand) -> Result<Intent, NativeError> {
    if !is_action_id(&command.action_id) {
        return Err(action_invalid());
    }
    let mut attachment_paths = Vec::new();
    match &command.operation {
        NativeOperation::SetOutboxPaused(_) => {}
        NativeOperation::RetractOutbox(p) | NativeOperation::RebindOutbox(p) => {
            if !is_action_id(&p.target_action_id) {
                return Err(action_invalid());
            }
        }
        // 插队不携带正文：只有目标行 id（`deny_unknown_fields` 已在 DTO 层拒绝正文副本）。
        NativeOperation::PrioritizeOutbox(p) => {
            if !is_action_id(&p.target_action_id) {
                return Err(action_invalid());
            }
        }
        NativeOperation::StopSession(_) => {}
        NativeOperation::SetAgent(payload) => {
            if !is_agent_id(&payload.agent) {
                return Err(action_invalid());
            }
        }
        NativeOperation::Send(payload) => {
            // 关联 id 必须正是按 action_id 派生的 `msg_<action_id>`（Confirmed 的唯一依据）。
            if payload.client_message_id != client_message_id_of(&command.action_id) {
                return Err(action_invalid());
            }
            // 正文与附件至少一个非空；纯附件消息的编码/回读链路已由原生 prompt
            // files 支持，不再把空正文一律拒绝。
            if payload.text.trim().is_empty() && payload.attachment_paths.is_empty() {
                return Err(send_empty());
            }
            if payload.text.chars().count() > MAX_SEND_CHARS {
                return Err(send_too_long());
            }
            // 斜杠命令：零 prompt 写出，绝不把终端命令当正文发给模型。
            if is_slash_message(&payload.text) {
                return Err(command_unsupported());
            }
            for path in &payload.attachment_paths {
                // 只接受绝对路径（受控目录校验在拿到 target 后进行）。
                if !Path::new(path).is_absolute() {
                    return Err(attachment_refused());
                }
                attachment_paths.push(path.clone());
            }
        }
        NativeOperation::Answer(payload) => {
            segment(&payload.form_id)?;
        }
        NativeOperation::Decide(payload) => {
            segment(&payload.request_id)?;
            // T7 未完成前不开放永久授权；静态拒绝先于绑定/路由/账本。
            if payload.decision == "always" {
                return Err(command_unsupported());
            }
            // 只放行 once/reject；其他词不代猜。
            if !matches!(payload.decision.as_str(), "once" | "reject") {
                return Err(command_unsupported());
            }
        }
        NativeOperation::Retract(payload) | NativeOperation::PromoteInbox(payload) => {
            segment(&payload.item_id)?;
        }
    }
    Ok(Intent { attachment_paths })
}

/// 插队目标（`Some` = 本命令是 `prioritize_outbox`）。唯一读 `NativeOperation` 的
/// 便捷判据，claim 事务与 settle 共用，避免两处各自 pattern match 漂移。
fn prioritize_target_action_id(op: &NativeOperation) -> Option<&str> {
    match op {
        NativeOperation::PrioritizeOutbox(source) => Some(source.target_action_id.as_str()),
        _ => None,
    }
}

/// 后端按 action_id 派生的关联 id（合同固定形态：`msg_<action_id>`）。
fn client_message_id_of(action_id: &str) -> String {
    format!("msg_{action_id}")
}

/// 动作 id 形态：非空且只含 `[A-Za-z0-9_]`（关联 id 由它派生，形态由此保证）。
fn is_action_id(action_id: &str) -> bool {
    !action_id.is_empty()
        && action_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 逐操作能力门：缺键 = 未判定 = 不放行；关闭时用契约自带的受控原因码。
fn require_capability(
    snapshot: &DispatchSnapshot,
    operation: NativeOperationId,
) -> Result<(), NativeError> {
    let Some(capability) = snapshot.capabilities.get(&operation) else {
        return Err(NativeError::new(
            codes::OC_CONTRACT_MISSING,
            "native operation is not available in this contract",
        ));
    };
    if capability.allowed {
        Ok(())
    } else {
        Err(NativeError::new(
            capability
                .reason_code
                .as_deref()
                .unwrap_or(codes::OC_CONTRACT_MISSING),
            "native operation is not available in this contract",
        ))
    }
}

/// 当前 pending 列表里的表单上下文令牌；缺席 = 已定案（列表是「仍在 pending」的权威）。
fn pending_form_token(snapshot: &DispatchSnapshot, form_id: &str) -> Result<String, NativeError> {
    snapshot
        .pending
        .iter()
        .find_map(|item| match item {
            PendingView::Form {
                id, context_token, ..
            } if id == form_id => Some(context_token.clone()),
            _ => None,
        })
        .ok_or_else(request_settled)
}

/// 当前 pending 列表里的审批上下文令牌；缺席 = 已定案。
fn pending_permission_token(
    snapshot: &DispatchSnapshot,
    request_id: &str,
) -> Result<String, NativeError> {
    snapshot
        .pending
        .iter()
        .find_map(|item| match item {
            PendingView::Permission {
                id, context_token, ..
            } if id == request_id => Some(context_token.clone()),
            _ => None,
        })
        .ok_or_else(request_settled)
}

/// 原生通道单文件保守预算（5 MiB）：Mac OpenCode 2.0.22 接受 5 MiB，但 8–20 MiB
/// 会因上游 `Prompt.Base64` 返回 500。与上传端点自身的 20 MiB 上限相互独立。
const MAX_ATTACHMENT_FILE_BYTES: u64 = 5 * 1024 * 1024;
/// 单条消息原始附件字节合计上限（20 MiB，R4）：与上传侧单文件 20 MiB 相互独立。
/// 16 MiB 附件在读回里按 base64 膨胀约 22.4 MB，仍在 32 MiB 整包预算内。
const MAX_ATTACHMENT_TOTAL_BYTES: u64 = 20 * 1024 * 1024;
/// 单条消息附件文件数上限（R4）。
const MAX_ATTACHMENT_FILES: usize = 10;

/// 附件受控目录校验：canonicalize 后必须位于 `<cwd>/.tuvis-attachments/<session_id>/`
/// 内且确实是文件；提交形态是 `file://` 绝对 URI（任务 0：裸本机路径 400）。
///
/// 预算（在 claim/写之前判定，超限即 `OC_ATTACHMENT_REFUSED`、零写入）：
/// * 单个文件 ≤ [`MAX_ATTACHMENT_FILE_BYTES`]（5 MiB）；
/// * canonical 路径的原始字节合计 ≤ [`MAX_ATTACHMENT_TOTAL_BYTES`]（20 MiB）；
/// * 文件数 ≤ [`MAX_ATTACHMENT_FILES`]（10 个）。
///
/// URI 自身的百分号编码由 [`file_uri_of`] 负责（逐段编码，见下）。
fn verified_attachments(
    target: &NativeTarget,
    paths: &[String],
) -> Result<Vec<Attachment>, NativeError> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    // 文件数上限先判：不碰文件系统，也避免为超限列表做无谓的 canonicalize。
    if paths.len() > MAX_ATTACHMENT_FILES {
        return Err(attachment_refused());
    }
    // target.directory 已 realpath 归一；受控根目录同样归一（容忍其中的符号链接）。
    let root = attachment_dir(&target.directory, &target.session_id)
        .canonicalize()
        .map_err(|_| attachment_refused())?;
    let mut files = Vec::with_capacity(paths.len());
    let mut total_bytes = 0u64;
    for path in paths {
        let canonical = std::fs::canonicalize(path).map_err(|_| attachment_refused())?;
        if !canonical.starts_with(&root) {
            return Err(attachment_refused());
        }
        // 已归一的路径元数据既是「确实是文件」的证据，也是原始字节合计的来源。
        let metadata = canonical.metadata().map_err(|_| attachment_refused())?;
        if !metadata.is_file() {
            return Err(attachment_refused());
        }
        // 单文件预算先于合计判定：给出可区分的原因（手机据此展示明确文案）。
        if metadata.len() > MAX_ATTACHMENT_FILE_BYTES {
            return Err(attachment_file_too_large());
        }
        total_bytes = total_bytes
            .checked_add(metadata.len())
            .ok_or_else(attachment_refused)?;
        if total_bytes > MAX_ATTACHMENT_TOTAL_BYTES {
            return Err(attachment_refused());
        }
        let name = canonical
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(attachment_refused)?;
        files.push(Attachment {
            uri: file_uri_of(&canonical)?,
            name,
        });
    }
    Ok(files)
}

/// 复制确认帧附件；后续上传覆盖/清理不会改变已入队的内容。副本随项目附件目录保留。
fn freeze_attachments(
    target: &NativeTarget,
    action_id: &str,
    files: &[Attachment],
) -> Result<Vec<String>, NativeError> {
    use std::io::{Read, Write};
    let mut paths = Vec::new();
    for (index, file) in files.iter().enumerate() {
        let source = reqwest::Url::parse(&file.uri)
            .ok()
            .and_then(|u| u.to_file_path().ok())
            .ok_or_else(attachment_refused)?;
        let mut bytes = Vec::new();
        std::fs::File::open(source)
            .map_err(|_| attachment_refused())?
            .take(MAX_ATTACHMENT_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| attachment_refused())?;
        if bytes.len() as u64 > MAX_ATTACHMENT_FILE_BYTES {
            return Err(attachment_file_too_large());
        }
        let root = attachment_dir(&target.directory, &target.session_id)
            .canonicalize()
            .map_err(|_| attachment_refused())?;
        let mut dir = root.clone();
        for component in ["outbox", action_id, &index.to_string()] {
            dir.push(component);
            match std::fs::create_dir(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(attachment_refused()),
            }
            let metadata = std::fs::symlink_metadata(&dir).map_err(|_| attachment_refused())?;
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || !dir
                    .canonicalize()
                    .map_err(|_| attachment_refused())?
                    .starts_with(&root)
            {
                return Err(attachment_refused());
            }
        }
        let path = dir.join(&file.name);
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
        {
            Ok(mut output) => {
                output.write_all(&bytes).map_err(|_| attachment_refused())?;
                output.sync_all().map_err(|_| attachment_refused())?;
                let mut permissions = output
                    .metadata()
                    .map_err(|_| attachment_refused())?
                    .permissions();
                permissions.set_readonly(true);
                output
                    .set_permissions(permissions)
                    .map_err(|_| attachment_refused())?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if std::fs::symlink_metadata(&path)
                    .map_err(|_| attachment_refused())?
                    .file_type()
                    .is_symlink()
                {
                    return Err(attachment_refused());
                }
                if std::fs::read(&path).map_err(|_| attachment_refused())? != bytes {
                    return Err(attachment_refused());
                }
            }
            Err(_) => return Err(attachment_refused()),
        }
        paths.push(path.to_string_lossy().into_owned());
    }
    Ok(paths)
}

/// 按宿主平台转换 file URI：Windows 盘符、UNC、verbatim 路径和 Unix 原始字节
/// 由 URL 库处理，与投影的 to_file_path 成对；转换失败在派发前拒绝。
pub(crate) fn file_uri_of(path: &Path) -> Result<String, NativeError> {
    reqwest::Url::from_file_path(path)
        .map(|url| url.to_string())
        .map_err(|_| attachment_refused())
}

/// 原生消息记录里是否出现精确 id（跨页去重后的原始记录）。
fn records_contain(records: &[Value], id: &str) -> bool {
    records
        .iter()
        .any(|record| record.get("id").and_then(Value::as_str) == Some(id))
}

/// 从账本行重建会话卡（只用于重新绑定：卡片 id + 已归一目录）。
pub(super) fn session_card(session_id: &str, directory: &str) -> Session {
    Session {
        id: session_id.to_string(),
        agent_type: AgentType::OpenCode,
        project_name: String::new(),
        project_path: directory.to_string(),
        title: None,
        git_branch: None,
        github_url: None,
        status: SessionStatus::Idle,
        last_message: None,
        last_message_role: None,
        last_message_subagent_report: false,
        flap_from_subagent_activity: false,
        last_activity_at: String::new(),
        pid: 0,
        cpu_usage: 0.0,
        active_subagent_count: 0,
        form: ProcessForm::Cli,
        jump_supported: false,
        unread: false,
        unread_turned_green_at_ms: None,
    }
}

/// send 的回读定性（读自写之后的新鲜投影）：只有精确原生 message id 出现在消息列表
/// 才算 `Confirmed`。
///
/// 200 单独不是确认；inbox 出现该 id 只说明已入队（忙时先落 inbox，任务 0 已证）；
/// 正文相同更不是证据（同正文两次发送得到不同 id）。
fn send_outcome(fresh: &DispatchSnapshot, native_id: &str, reply: NativeWriteReply) -> Outcome {
    if !(200..300).contains(&reply.status) {
        return Outcome::unconfirmed(Some(reason_words::WRITE_REFUSED.to_string()));
    }
    if records_contain(&fresh.records, native_id) {
        return Outcome::terminal(ActionState::Confirmed, Some(native_id.to_string()), None);
    }
    if fresh.inbox.iter().any(|item| item.id == native_id) {
        return Outcome::terminal(ActionState::Acknowledged, Some(native_id.to_string()), None);
    }
    Outcome::unconfirmed(Some(reason_words::ID_ABSENT.to_string()))
}

/// 表单权威状态词的三态判定（R3）。
///
/// `state.status` 是开放字符串，本批只支持 `answered` / `cancelled` 两个终态。
/// `pending` 只证明尚未定案；任何未知词（含服务端将来新增的中间态）都不可当终态——
/// 否则「不是 pending 就算定案」会把未知状态误记成 Confirmed/Settled。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FormState {
    Pending,
    Answered,
    Cancelled,
    Unknown,
}

fn form_state_of(status: Option<&str>) -> FormState {
    match status {
        Some("pending") => FormState::Pending,
        Some("answered") => FormState::Answered,
        Some("cancelled") => FormState::Cancelled,
        _ => FormState::Unknown,
    }
}

/// answer 的回读定性（detail 是表单权威状态的唯一来源）。
///
/// * 2xx + `answered` ⇒ 我们的作答生效（竞答里恰好一个 204，409 归败者）；
/// * 2xx + `cancelled` ⇒ `Settled`（已定案，但不是本端作答成功，绝不声称 Confirmed）；
/// * 非 2xx + 已支持终态 ⇒ `Settled`（只说已定案，不声称哪端获胜）；
/// * 400 + 仍 pending ⇒ `NotAccepted`（任务 0 G2 金样：表单 400 之后 state 始终
///   pending、无部分生效 ⇒ 整份未生效，可释放占用）；
/// * 其余（pending / 未知状态词 / 读不到）⇒ `Unconfirmed`。
async fn answer_outcome(
    source: &Source,
    target: &NativeTarget,
    form_id: &str,
    reply: NativeWriteReply,
) -> Outcome {
    // 读不到 detail 与未知状态词同义：既不能确认，也不能声称未生效。
    let state = match read_form_detail(&source.client, target, form_id).await {
        Ok(detail) => form_state_of(detail.status.as_deref()),
        Err(_) => FormState::Unknown,
    };
    if (200..300).contains(&reply.status) {
        return match state {
            FormState::Answered => {
                Outcome::terminal(ActionState::Confirmed, Some(form_id.to_string()), None)
            }
            FormState::Cancelled => Outcome::terminal(
                ActionState::Settled,
                None,
                Some(reason_words::ALREADY_SETTLED),
            ),
            FormState::Pending | FormState::Unknown => {
                Outcome::unconfirmed(Some(reason_words::READBACK_FAILED.to_string()))
            }
        };
    }
    match state {
        FormState::Answered | FormState::Cancelled => Outcome::terminal(
            ActionState::Settled,
            None,
            Some(reason_words::ALREADY_SETTLED),
        ),
        FormState::Pending if reply.status == 400 => {
            Outcome::terminal(ActionState::NotAccepted, None, Some(NOT_ACCEPTED_EVIDENCE))
        }
        FormState::Pending => Outcome::unconfirmed(Some(reason_words::WRITE_REFUSED.to_string())),
        FormState::Unknown => Outcome::unconfirmed(Some(reason_words::READBACK_FAILED.to_string())),
    }
}

/// decide 的回读定性（读自写之后的新鲜投影）：2xx + 审批离开 pending ⇒ 放行/驳回生效；
/// 非 2xx + 已不在 pending ⇒ `Settled`。**「不在列表里」只有在审批列表完整可信时才成立**
/// （R3：任一条目归属不可证 ⇒ `permissions_trusted == false`，缺席不得当证据）。
/// 权限 400 没有「整份未生效」的金样证明 ⇒ 仍记 `Unconfirmed`（R1 只认证据）。
fn decide_outcome(fresh: &DispatchSnapshot, request_id: &str, reply: NativeWriteReply) -> Outcome {
    let present = fresh
        .pending
        .iter()
        .any(|item| matches!(item, PendingView::Permission { id, .. } if id == request_id));
    // 只有「确实不在列表里」**且**整份列表归属可证时，缺席才是「请求已消失」的证据。
    let gone = !present && fresh.permissions_trusted;
    if (200..300).contains(&reply.status) {
        return if gone {
            Outcome::terminal(ActionState::Confirmed, Some(request_id.to_string()), None)
        } else {
            Outcome::unconfirmed(Some(reason_words::READBACK_FAILED.to_string()))
        };
    }
    if gone {
        Outcome::terminal(
            ActionState::Settled,
            None,
            Some(reason_words::ALREADY_SETTLED),
        )
    } else if present {
        Outcome::unconfirmed(Some(reason_words::WRITE_REFUSED.to_string()))
    } else {
        Outcome::unconfirmed(Some(reason_words::READBACK_FAILED.to_string()))
    }
}

/// 插队（PATCH delivery=steer）的回读定性。
///
/// 可证的三种事实：
/// * 该项**仍在 inbox 且已带 steer** ⇒ 已受理（上游会在当前回合交付并打断）；
/// * 该项已**离开 inbox**、且正式记录里出现该 id ⇒ 已被消费（交付已发生）；
/// * 仍在 inbox 却仍是 queue ⇒ PATCH 没生效：明确未生效（`NotAccepted`），不是未知。
fn promote_outcome(fresh: &DispatchSnapshot, item_id: &str, _reply: NativeWriteReply) -> Outcome {
    if let Some(item) = fresh.inbox.iter().find(|item| item.id == item_id) {
        return match item.delivery {
            DeliveryKind::Steer => Outcome::terminal(
                ActionState::Acknowledged,
                Some(item_id.to_string()),
                Some(reason_words::STEERED_INTERRUPT),
            ),
            DeliveryKind::Queue => {
                Outcome::terminal(ActionState::NotAccepted, None, Some(NOT_ACCEPTED_EVIDENCE))
            }
        };
    }
    if records_contain(&fresh.records, item_id) {
        return Outcome::terminal(
            ActionState::Settled,
            Some(item_id.to_string()),
            Some(reason_words::DELIVERED_BEFORE_RETRACT),
        );
    }
    if fresh.complete {
        Outcome::terminal(ActionState::Confirmed, Some(item_id.to_string()), None)
    } else {
        Outcome::unconfirmed(Some(reason_words::READBACK_FAILED.to_string()))
    }
}

/// retract 的回读定性（读自写之后的新鲜投影）：**204 本身不是撤回证据**，分类只看回读。
///
/// * 仍在 inbox ⇒ 撤回未生效（对排队项的 DELETE 会移除它）⇒ `NotAccepted`；
/// * 已被投递（在消息列表）⇒ `Settled`（执行未被取消，只是队列先消费了它）；
/// * 完整回读下既不在 inbox 也不在消息列表 ⇒ 项已不在队列且从未投递 ⇒ `Confirmed`；
/// * 部分历史 ⇒ `Unconfirmed`（缺席不能作否定证据）。
fn retract_outcome(fresh: &DispatchSnapshot, item_id: &str, _reply: NativeWriteReply) -> Outcome {
    if fresh.inbox.iter().any(|item| item.id == item_id) {
        return Outcome::terminal(ActionState::NotAccepted, None, Some(NOT_ACCEPTED_EVIDENCE));
    }
    if records_contain(&fresh.records, item_id) {
        return Outcome::terminal(
            ActionState::Settled,
            None,
            Some(reason_words::DELIVERED_BEFORE_RETRACT),
        );
    }
    if fresh.complete {
        Outcome::terminal(ActionState::Confirmed, Some(item_id.to_string()), None)
    } else {
        Outcome::unconfirmed(Some(reason_words::READBACK_FAILED.to_string()))
    }
}

/// send 的核对（读自一次新鲜投影）：精确 id 在消息列表 ⇒ `Confirmed`；只在 inbox ⇒
/// `Acknowledged`；都缺席 ⇒ 不改写（正文相同不是证据，缺席也不是「未生效」的证据）。
/// 行停在 `dispatching`（派发被取消）且无证据时，收敛为 `Unconfirmed`——绝不卡在飞。
/// 停止的恢复核对：只读当前会话 `time.busy/time.idle`。当前已空闲 ⇒ 可收敛为
/// `settled/already_idle`（不声称是本次 HTTP 的 `interrupted` 标志）；仍忙/读不到 ⇒
/// 保持 Unknown，绝不重发。停到“服务收到时正在执行的回合”语义已由该状态覆盖。
async fn verify_stop(
    source: &Source,
    target: &super::types::NativeTarget,
    current: ActionState,
) -> Option<Outcome> {
    let path = format!("/api/session/{}", segment(&target.session_id).ok()?);
    let value = match source.client.read_json(&path, &[]).await {
        Ok(value) => value,
        Err(_) => {
            return keep_unconfirmed(current, reason_words::READBACK_FAILED);
        }
    };
    let root = value.get("data").unwrap_or(&value);
    let busy = root.pointer("/time/busy").and_then(Value::as_i64);
    let idle = root.pointer("/time/idle").and_then(Value::as_i64);
    // 当前空闲只说明“现在没有活动回合”，不能证明是本次丢失回执的停止造成；
    // 仍然保持 Unknown，绝不把已空闲当成本动作成功。
    if !session_busy_from_times(busy, idle) {
        keep_unconfirmed(current, reason_words::RESULT_UNPROVEN)
    } else {
        keep_unconfirmed(current, reason_words::READBACK_FAILED)
    }
}

/// 模式的恢复核对：目标 agent 已持久化在动作行 `request_key`。只有当前会话 agent
/// 精确等于该目标才可收敛；读到不同值/读失败都保持 Unknown（不把旧值或他人切换
/// 当成自己的成功）。
async fn verify_set_agent(
    source: &Source,
    target: &super::types::NativeTarget,
    agent: &str,
    current: ActionState,
) -> Option<Outcome> {
    let path = format!("/api/session/{}", segment(&target.session_id).ok()?);
    let value = match source.client.read_json(&path, &[]).await {
        Ok(value) => value,
        Err(_) => {
            return keep_unconfirmed(current, reason_words::READBACK_FAILED);
        }
    };
    let root = value.get("data").unwrap_or(&value);
    // 当前 agent 等于目标可能来自桌面或另一标签；没有本次写回执时不能归因。
    // 保持 Unknown，避免把别人的切换当成本动作成功。
    if root.get("agent").and_then(Value::as_str) == Some(agent) {
        keep_unconfirmed(current, reason_words::RESULT_UNPROVEN)
    } else {
        keep_unconfirmed(current, reason_words::READBACK_FAILED)
    }
}

fn verify_send(
    snapshot: &DispatchSnapshot,
    action_id: &str,
    current: ActionState,
) -> Option<Outcome> {
    let native_id = client_message_id_of(action_id);
    if records_contain(&snapshot.records, &native_id) {
        return Some(Outcome::terminal(
            ActionState::Confirmed,
            Some(native_id),
            None,
        ));
    }
    if snapshot.inbox.iter().any(|item| item.id == native_id)
        && current != ActionState::Acknowledged
    {
        return Some(Outcome::terminal(
            ActionState::Acknowledged,
            Some(native_id),
            None,
        ));
    }
    converge_if_dispatching(current, reason_words::ID_ABSENT)
}

/// answer 的核对：detail 读证明仍在 pending ⇒ **只证明尚未定案**，保持 `Unconfirmed`
/// （R1：不得把瞬态 pending 写成「整份未生效」）；`answered` / `cancelled` 是已支持终态
/// ⇒ `Settled`（不声称哪端获胜）；未知状态词与读不到状态同样**不改写**（R3：未知不是
/// 终态，`dispatching` 行只收敛为 `Unconfirmed`）。
async fn verify_answer(
    source: &Source,
    snapshot: &DispatchSnapshot,
    form_id: &str,
    current: ActionState,
) -> Option<Outcome> {
    let Ok(detail) = read_form_detail(&source.client, &snapshot.target, form_id).await else {
        return converge_if_dispatching(current, reason_words::READBACK_FAILED);
    };
    // detail 返回的另一张表单不能给本动作定案（与 form_result_pinned 同口径）：
    // 按「证据不可用」处理，保持未知，不释放未决约束。
    if detail.spec.id != form_id {
        return keep_unconfirmed(current, reason_words::READBACK_FAILED);
    }
    match form_state_of(detail.status.as_deref()) {
        FormState::Answered | FormState::Cancelled => Some(Outcome::terminal(
            ActionState::Settled,
            None,
            Some(reason_words::ALREADY_SETTLED),
        )),
        // pending 只证明尚未定案；未知词同样不能定案。两者都停在 Unknown。
        FormState::Pending | FormState::Unknown => {
            keep_unconfirmed(current, reason_words::READBACK_FAILED)
        }
    }
}

/// decide 的核对（读自一次新鲜投影）：仍在 pending ⇒ **保持 `Unconfirmed`**（尚未决定
/// 不是「未生效」）；已不在**且审批列表完整可信** ⇒ `Settled`；缺席但列表里有不可证
/// 归属项 ⇒ 保持 `Unconfirmed`（R3：不可证的缺席不得定案）。
fn verify_decide(
    snapshot: &DispatchSnapshot,
    request_id: &str,
    current: ActionState,
) -> Option<Outcome> {
    let present = snapshot
        .pending
        .iter()
        .any(|item| matches!(item, PendingView::Permission { id, .. } if id == request_id));
    if present || !snapshot.permissions_trusted {
        return keep_unconfirmed(current, reason_words::READBACK_FAILED);
    }
    Some(Outcome::terminal(
        ActionState::Settled,
        None,
        Some(reason_words::ALREADY_SETTLED),
    ))
}

/// retract 的核对（读自一次新鲜投影）：**仍在队列 ⇒ 保持 `Unconfirmed`**（撤回尚未
/// 生效，不是可证明的拒绝）；已投递 ⇒ `Settled`；完整回读下既不在 inbox 也不在消息
/// 列表 ⇒ 撤回生效。
fn verify_retract(
    snapshot: &DispatchSnapshot,
    item_id: &str,
    current: ActionState,
) -> Option<Outcome> {
    if snapshot.inbox.iter().any(|item| item.id == item_id) {
        return keep_unconfirmed(current, reason_words::READBACK_FAILED);
    }
    if records_contain(&snapshot.records, item_id) {
        return Some(Outcome::terminal(
            ActionState::Settled,
            None,
            Some(reason_words::DELIVERED_BEFORE_RETRACT),
        ));
    }
    if snapshot.complete {
        Some(Outcome::terminal(
            ActionState::Confirmed,
            Some(item_id.to_string()),
            None,
        ))
    } else {
        converge_if_dispatching(current, reason_words::READBACK_FAILED)
    }
}

/// 取消窗口自愈：停在 `dispatching` 的行（派发 future 被丢弃）在核对时若无正面证据，
/// 按 `Unconfirmed` 收敛——`dispatching` 与 `Unconfirmed` 同义「结果不明」，卡在飞
/// 会让行永远既不可派发又不可核对。
fn converge_if_dispatching(current: ActionState, reason: &str) -> Option<Outcome> {
    (current == ActionState::Dispatching).then(|| Outcome::unconfirmed(Some(reason.to_string())))
}

/// R1：核对只能把行**保持在** `Unconfirmed`——瞬态的 pending/inbox 证据不足以证明
/// 整份请求未生效，绝不落 `NotAccepted`（占用不释放）。`Dispatching` 收敛为
/// `Unconfirmed`；已经是 `Unconfirmed` 的行允许刷新为最新理由（状态不变）。
fn keep_unconfirmed(current: ActionState, reason: &str) -> Option<Outcome> {
    matches!(current, ActionState::Dispatching | ActionState::Unconfirmed)
        .then(|| Outcome::unconfirmed(Some(reason.to_string())))
}

/// 账本行记录的服务代际/目录与当前投影是否同一世代（旧代际的行不由新投影证明）。
///
/// 判据与 DAO 的 [`crate::database::dao::opencode_actions::RouteRow::same_runtime`]
/// 同源（service_id + pid + started_at_ms），再加会话 id 与已归一目录逐项相等。
/// F12 项 2：原动作服务代际三态（见 [`NativeHub::classify_original_generation`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OriginalGeneration {
    /// 原进程仍以账本身份存活（pid+lstart 全等）：仍可核对，不可归档、不渲染出口。
    Alive,
    /// 可证明进程退出（事实机制明确报告无此 pid）或 PID 已换代（pid 活但
    /// lstart≠账本）：可归档、渲染出口。
    Ended,
    /// 证据不足（pid 不可解析 / 事实通道读取失败——既不能证明在场也不能证明缺席）：
    /// fail-closed，不装饰、不允许归档。
    InsufficientEvidence,
}

/// 进程事实探测的三形态（G3/C6 判据收紧）：事实机制对「pid 是否在场、启动时刻」
/// 的**可区分**回答。两态缝（`Fn(u32) -> Option<i64>`）拿不到「明确缺席 vs 通道
/// 失败」的区分，本枚举是唯一的死亡证明载体。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessFactProbe {
    /// pid 在场且启动时刻可读（lstart）。
    Alive(i64),
    /// 事实机制健康且明确报告无此 pid（同一次刷新里健康探针 pid 在场）。
    NoSuchProcess,
    /// 事实通道不可用（健康探针自身也不在场 = 系统级读取失败；或在场但启动时刻
    /// 读不出）。**不构成任何死亡证明**：fail-closed。
    ReadFailure,
}

/// F12 项 2 + G3 收紧的判定本体（纯函数形态，便于注入实测缝做单元判定）。
///
/// 三形态判据（G3）：只有**可证明**的两形态落 `Ended`——事实机制明确报告无此 pid
///（[`ProcessFactProbe::NoSuchProcess`]）或 pid 在场但 lstart≠账本（PID 复用，
/// [`ProcessFactProbe::Alive`] 不等臂）；**读取失败**（[`ProcessFactProbe::ReadFailure`]
/// / pid 不可解析）一律 `InsufficientEvidence`——旧判据「fact None + 登记可读 ⇒
/// 采信退出」废除：fact None 只是「读不到」，登记通道健康不构成原进程退出的证明。
pub(crate) fn classify_original_generation_with(
    generation_fact: &dyn Fn(u32) -> ProcessFactProbe,
    row: &dao::ActionRow,
) -> OriginalGeneration {
    let Ok(pid) = u32::try_from(row.runtime_pid) else {
        return OriginalGeneration::InsufficientEvidence;
    };
    match generation_fact(pid) {
        // 实测：原进程仍以账本身份（pid+lstart）存活——登记切到别处也不改判。
        ProcessFactProbe::Alive(started) if started == row.runtime_started_at_ms => {
            OriginalGeneration::Alive
        }
        // PID 活但 lstart ≠ 账本 = PID 已复用（换代），可证明 ⇒ Ended。
        ProcessFactProbe::Alive(_) => OriginalGeneration::Ended,
        // 事实机制健康且明确报告无此 pid = 进程退出可证明 ⇒ Ended。
        ProcessFactProbe::NoSuchProcess => OriginalGeneration::Ended,
        // 通道不可用：既不能证明在场也不能证明缺席 ⇒ InsufficientEvidence
        //（不可归档、不装饰出口，绝不把「读不到」当「已死」）。
        ProcessFactProbe::ReadFailure => OriginalGeneration::InsufficientEvidence,
    }
}

fn row_belongs_to_current(row: &dao::ActionRow, target: &NativeTarget) -> bool {
    row.service_id == target.runtime.service_id
        && row.runtime_pid == i64::from(target.runtime.pid)
        && row.runtime_started_at_ms == target.runtime.started_at_ms
        && row.session_id == target.session_id
        && row.directory == target.directory.to_string_lossy()
}

/// agent id 的本地形态约束（与 OpenCode 的 ID 词表一致：字母数字、`-`、`_`、`.`）。
fn is_agent_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// 服务端 `time.busy/idle` 的忙判据：新会话两者都缺 = 空闲；有 busy 但没有更晚
/// 的 idle = 正在执行。`busy`/`idle` 是毫秒时间戳。
fn session_busy_from_times(busy: Option<i64>, idle: Option<i64>) -> bool {
    busy.is_some() && idle.is_none_or(|idle| busy.is_some_and(|busy| busy > idle))
}

fn session_busy() -> NativeError {
    NativeError::new(
        action_codes::CONFLICT,
        "the session is executing; switch agent only while idle",
    )
}

fn binding_stale() -> NativeError {
    NativeError::new(
        codes::OC_BINDING_STALE,
        "native command tokens do not match the current projection",
    )
}

fn command_unsupported() -> NativeError {
    NativeError::new(
        codes::OC_COMMAND_UNSUPPORTED,
        "command content is not supported in this batch",
    )
}

fn attachment_refused() -> NativeError {
    NativeError::new(
        codes::OC_ATTACHMENT_REFUSED,
        "attachment is outside the controlled directory or exceeds the per-message budget",
    )
}

/// 单文件超限：同码不同文案，手机可据此说明「单个文件 5 MiB」而不是笼统的预算超限。
fn attachment_file_too_large() -> NativeError {
    NativeError::new(
        codes::OC_ATTACHMENT_REFUSED,
        "attachment exceeds the native per-file budget (5 MiB)",
    )
}

fn send_empty() -> NativeError {
    NativeError::new(codes::OC_SEND_EMPTY, "send requires a non-empty body")
}

fn send_too_long() -> NativeError {
    NativeError::new(codes::OC_SEND_TOO_LONG, "send text exceeds the length cap")
}

fn action_invalid() -> NativeError {
    NativeError::new(
        codes::OC_ACTION_INVALID,
        "native action identity is not accepted",
    )
}

fn route_not_owned() -> NativeError {
    NativeError::new(
        codes::OC_ROUTE_NOT_OWNED,
        "this device does not own the native write entry",
    )
}

/// G1 换窗并发拒绝：回声验证锁定的窗口身份/路由版本与取得派发权时的伴表当前值
/// 不全等（同服务显式重选换了窗口）——旧请求不得再用旧窗口授权写出。零写出；
/// 待发项原样保留（既有 Unknown/待发语义承接，不自动重发）。
fn route_window_mismatch() -> NativeError {
    NativeError::new(
        codes::OC_ROUTE_WINDOW_MISMATCH,
        "the verified window changed before dispatch was granted",
    )
}

fn request_settled() -> NativeError {
    NativeError::new(
        codes::OC_REQUEST_SETTLED,
        "native request is already settled",
    )
}

fn view_of(row: &dao::ActionRow) -> ActionView {
    ActionView {
        action_id: row.action_id.clone(),
        state: row.state,
        reason_code: row.reason_code.clone(),
        native_id: row.native_id.clone(),
        runtime_dead: None,
    }
}

/// 交回的收尾判定（只服务非 force 的 [`NativeHub::disengage_route`]）：降级成功 ⇒
/// 已归旧路径；未变更时用既有归属读取区分「无路由行（幂等成功）」与「未决动作拒绝」
/// （`active` = 拒绝；`draining`/无行 = 已归旧路径）。
/// `force` 走 [`NativeActionStore::abandon_and_return`] 的单事务形态，不经过这里。
fn settle_route(store: &NativeActionStore, session_id: &str) -> Result<bool, NativeError> {
    if store.disengage_route(session_id)? {
        return Ok(true);
    }
    Ok(!store.owns_session(session_id)?)
}

fn rejection_error(
    row: &dao::RejectionRow,
    actor: &NativeActor,
    session: &Session,
    command: &NativeCommand,
) -> NativeError {
    let directory = std::fs::canonicalize(&session.project_path)
        .unwrap_or_else(|_| std::path::PathBuf::from(&session.project_path));
    if row.device_id != actor.device_id {
        NativeError::new(action_codes::NOT_FOUND, "native action not found")
    } else if row.session_id != command.session_id
        || session.id != command.session_id
        || directory != std::path::Path::new(&row.directory)
        || row.payload_sha256 != payload_sha256(command)
    {
        NativeError::new(
            action_codes::CONFLICT,
            "native action conflicts with a live one",
        )
    } else {
        NativeError::new(
            &row.reason_code,
            "native action was refused before dispatch",
        )
    }
}

/// 前置错误必须和 reserve 原子互斥；已有可能派发的动作返回其真实状态。
fn resolve_entry_failure(
    hub: &NativeHub,
    actor: &NativeActor,
    session: &Session,
    command: &NativeCommand,
    error: NativeError,
) -> Result<ActionView, NativeError> {
    let existing = hub.store().with(|conn| {
        let tx = conn.unchecked_transaction().map_err(store_error)?;
        if !crate::remote::pairing::device_valid(&tx, &actor.device_id, now_ms()) {
            return Err(NativeError::new(action_codes::NOT_FOUND, "native action not found"));
        }
        if let Some(rejection) = dao::rejection_conn(&tx, &command.action_id).map_err(store_error)? {
            return Err(rejection_error(&rejection, actor, session, command));
        }
        let directory = std::fs::canonicalize(&session.project_path)
            .unwrap_or_else(|_| std::path::PathBuf::from(&session.project_path));
        if session.id != command.session_id { return Err(action_invalid()); }
        let row = dao::get_action_conn(&tx, &command.action_id).map_err(store_error)?;
        let result = if let Some(row) = row {
            if row.device_id != actor.device_id {
                return Err(NativeError::new(action_codes::NOT_FOUND, "native action not found"));
            }
            if row.session_id != command.session_id || row.payload_sha256 != payload_sha256(command) {
                return Err(NativeError::new(action_codes::CONFLICT, "native action conflicts with a live one"));
            }
            if row.state == ActionState::NotSent && row.reason_code.is_none() {
                if directory != std::path::Path::new(&row.directory) {
                    return Err(NativeError::new(codes::OC_RESPONSE_INVALID, "the original action directory cannot be verified"));
                }
                if error.code != action_codes::STORE_FAILED {
                    let outcome = dao::apply_finish_conn(&tx, &command.action_id, ActionState::NotSent, None, Some(abandon_reason(&error.code)), now_ms()).map_err(store_error)?;
                    if !matches!(outcome, FinishDao::Applied) { return Err(action_invalid()); }
                }
                None
            } else { Some(view_of(&row)) }
        } else {
            tx.execute("INSERT INTO opencode_native_rejections(action_id,device_id,session_id,directory,payload_sha256,reason_code,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7)", rusqlite::params![command.action_id, actor.device_id, command.session_id, directory.to_string_lossy(), payload_sha256(command), error.code, now_ms()]).map_err(store_error)?;
            None
        };
        tx.commit().map_err(store_error)?;
        Ok(result)
    })?;
    match existing {
        Some(view) => Ok(view),
        None => Err(error),
    }
}

/// 按账本当前状态收敛派发前错误；不可用派发前的旧状态推断并发副本的结局。
fn resolve_dispatch_failure(
    store: &NativeActionStore,
    device_id: &str,
    action_id: &str,
    error: NativeError,
) -> Result<ActionView, NativeError> {
    let result = store.store.with(|conn| {
        let tx = conn.unchecked_transaction().map_err(store_error)?;
        let row = dao::get_action_conn(&tx, action_id).map_err(store_error)?
            .filter(|row| row.device_id == device_id)
            .ok_or_else(|| NativeError::new(action_codes::NOT_FOUND, "native action not found"))?;
        let view = match row.state {
            ActionState::Queued => {
                // I1（换窗拒绝不可被后台覆盖）：不得把换窗拒绝原因覆盖成其他码
                //（与 set_queued_reason_conn 同口径；本路径对换窗拒绝行不可达，
                // 锁内重核已先行阻断，此处为单一不变式的兜底收口）。
                let changed = tx.execute("UPDATE opencode_native_actions SET reason_code=?2,updated_at=?3 WHERE action_id=?1 AND state='queued' AND reason_code IS NOT ?2 AND (reason_code IS NULL OR reason_code<>?4)", rusqlite::params![action_id, &error.code, now_ms(), codes::OC_ROUTE_WINDOW_MISMATCH]).map_err(store_error)?;
                if changed > 0 {
                    tx.execute("UPDATE opencode_outbox_targets SET revision=revision+1 WHERE EXISTS (SELECT 1 FROM opencode_native_actions a WHERE a.action_id=?1 AND a.session_id=opencode_outbox_targets.session_id AND a.directory=opencode_outbox_targets.directory)", [action_id]).map_err(store_error)?;
                }
                Some(view_of(&dao::get_action_conn(&tx, action_id).map_err(store_error)?.ok_or_else(action_invalid)?))
            }
            ActionState::NotSent if row.reason_code.is_none() => {
                if error.code != action_codes::STORE_FAILED {
                    let result = dao::apply_finish_conn(&tx, action_id, ActionState::NotSent, None, Some(abandon_reason(&error.code)), now_ms()).map_err(store_error)?;
                    if !matches!(result, FinishDao::Applied) { return Err(action_invalid()); }
                }
                None
            }
            _ => Some(view_of(&row)),
        };
        tx.commit().map_err(store_error)?;
        Ok(view)
    })?;
    match result {
        Some(view) => Ok(view),
        None => Err(error),
    }
}

/// 初次确认前失败可证明未发送；持久 queued 的迟延复核失败保留意图，等待恢复或用户撤回。
fn preclaim_failure(
    store: &NativeActionStore,
    current: &ActionView,
    action_id: &str,
    error: NativeError,
) -> NativeError {
    if current.state == ActionState::Queued {
        if let Err(storage) = store.store.with(|conn| {
            crate::database::dao::opencode_outbox::set_queued_reason_conn(
                conn,
                action_id,
                Some(&error.code),
            )
            .map_err(store_error)
        }) {
            return storage;
        }
        error
    } else {
        abandon(store, action_id, error)
    }
}

fn store_error(e: rusqlite::Error) -> NativeError {
    log::error!("opencode native action store: {e}");
    NativeError::new(
        action_codes::STORE_FAILED,
        "native action store is unavailable",
    )
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// ============================================================
// 客户端原始意图指纹
// ============================================================

fn operation_word(op: &NativeOperation) -> &'static str {
    match op {
        NativeOperation::SetOutboxPaused(_) => "set_outbox_paused",
        NativeOperation::RetractOutbox(_) => "retract_outbox",
        NativeOperation::RebindOutbox(_) => "rebind_outbox",
        NativeOperation::PrioritizeOutbox(_) => "prioritize_outbox",
        NativeOperation::Send(_) => "send",
        NativeOperation::Answer(_) => "answer",
        NativeOperation::Decide(_) => "decide",
        NativeOperation::Retract(_) => "retract",
        NativeOperation::PromoteInbox(_) => "promote_inbox",
        NativeOperation::StopSession(_) => "stop_session",
        NativeOperation::SetAgent(_) => "set_agent",
    }
}

fn request_key_of(op: &NativeOperation) -> Option<&str> {
    match op {
        NativeOperation::SetOutboxPaused(_) => None,
        // 插队的「目标」也是账本上的既有队列行；指纹只含目标 id（无正文字段）。
        NativeOperation::RetractOutbox(p)
        | NativeOperation::RebindOutbox(p)
        | NativeOperation::PrioritizeOutbox(p) => Some(&p.target_action_id),
        NativeOperation::Send(_) => None,
        NativeOperation::PromoteInbox(p) => Some(p.item_id.as_str()),
        NativeOperation::Answer(p) => Some(p.form_id.as_str()),
        NativeOperation::Decide(p) => Some(p.request_id.as_str()),
        NativeOperation::Retract(p) => Some(p.item_id.as_str()),
        NativeOperation::StopSession(_) => None,
        // 目标 agent 是核对“是否已切到该值”的持久证据；不能只留布尔/无键。
        NativeOperation::SetAgent(payload) => Some(payload.agent.as_str()),
    }
}

/// 未决请求占用键：form/permission 才有（send/retract 不占用任何待答项）
fn occupancy_key_of(session_id: &str, op: &NativeOperation) -> Option<String> {
    match op {
        NativeOperation::Answer(p) => Some(format!("form:{session_id}:{}", p.form_id)),
        NativeOperation::Decide(p) => Some(format!("permission:{session_id}:{}", p.request_id)),
        _ => None,
    }
}

fn feed(h: &mut Sha256, bytes: &[u8]) {
    h.update((bytes.len() as u64).to_be_bytes());
    h.update(bytes);
}

fn feed_count(h: &mut Sha256, n: usize) {
    h.update((n as u64).to_be_bytes());
}

/// 客户端原始意图的指纹（同 actionId 的重复请求只有指纹逐字相同才算同一次派发）。
///
/// 只摘要意图字段，字段间做长度前缀（拼接歧义不可能）；`answers` 按 `BTreeMap` 键序
/// 摘要所以形状稳定。摘要值进库，原文（正文/答案）**不进库**。
pub fn payload_sha256(command: &NativeCommand) -> String {
    let mut h = Sha256::new();
    feed(&mut h, b"opencode-native-intent-v1");
    feed(&mut h, operation_word(&command.operation).as_bytes());
    match &command.operation {
        NativeOperation::SetOutboxPaused(p) => {
            h.update([u8::from(p.paused)]);
            h.update(p.expected_revision.to_be_bytes());
        }
        NativeOperation::RetractOutbox(p) | NativeOperation::RebindOutbox(p) => {
            feed(&mut h, p.target_action_id.as_bytes())
        }
        // 插队的意图 = 「把这一行提前派发」；正文不在载荷里，也不参与指纹。
        NativeOperation::PrioritizeOutbox(p) => feed(&mut h, p.target_action_id.as_bytes()),
        // 原生插队 = 「把这一既有 inbox 项改成 steer」；同样只含目标 id。
        NativeOperation::PromoteInbox(p) => feed(&mut h, p.item_id.as_bytes()),
        NativeOperation::Send(p) => {
            feed(&mut h, p.text.as_bytes());
            feed_count(&mut h, p.attachment_paths.len());
            for path in &p.attachment_paths {
                feed(&mut h, path.as_bytes());
            }
            h.update([match p.delivery {
                DeliveryKind::Queue => 0u8,
                DeliveryKind::Steer => 1u8,
            }]);
            feed(&mut h, p.client_message_id.as_bytes());
        }
        NativeOperation::Answer(p) => {
            feed(&mut h, p.form_id.as_bytes());
            feed(&mut h, p.context_token.as_bytes());
            feed_count(&mut h, p.answers.len());
            for (key, draft) in &p.answers {
                feed(&mut h, key.as_bytes());
                match draft {
                    DraftAnswer::Text(text) => {
                        feed(&mut h, b"text");
                        feed(&mut h, text.as_bytes());
                    }
                    DraftAnswer::Multi(values) => {
                        feed(&mut h, b"multi");
                        feed_count(&mut h, values.len());
                        for value in values {
                            feed(&mut h, value.as_bytes());
                        }
                    }
                    DraftAnswer::Skip => feed(&mut h, b"skip"),
                }
            }
        }
        NativeOperation::Decide(p) => {
            feed(&mut h, p.request_id.as_bytes());
            feed(&mut h, p.context_token.as_bytes());
            feed(&mut h, p.decision.as_bytes());
        }
        NativeOperation::Retract(p) => feed(&mut h, p.item_id.as_bytes()),
        NativeOperation::StopSession(_) => {}
        NativeOperation::SetAgent(p) => feed(&mut h, p.agent.as_bytes()),
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::dao::{inject_queue, opencode_actions as dao};
    use crate::remote::pairing::DeviceStore;
    use crate::services::opencode_native::types::{
        ActionState, AnswerPayload, Answers, DecidePayload, DeliveryKind, DiscoveryInput,
        DraftAnswer, FormAnswerValue, NativeActor, NativeCommand, NativeOperation, NativeTarget,
        PendingView, PublicNativeView, RetractPayload, RuntimeKey, SendPayload, SetAgentPayload,
        StopSessionPayload,
    };
    use crate::session::{AgentType, ProcessForm, Session, SessionStatus};
    use serde_json::{json, Value};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    #[test]
    fn fresh_session_without_busy_idle_is_idle_for_agent_switch() {
        assert!(!session_busy_from_times(None, None));
        assert!(session_busy_from_times(Some(10), None));
        assert!(!session_busy_from_times(Some(10), Some(11)));
        assert!(session_busy_from_times(Some(11), Some(10)));
    }
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Notify;

    // ==================== 任务 7：派发路径（execute / action / verify） ====================
    //
    // 假 peer 是手写回环 HTTP：所有断言（POST 次数、路径、请求体、回读次序）都以
    // **服务端真实收到的请求** 为准，不看客户端返回值自述。

    /// 派发专用会话 id（与既有 store 用例的 `ses_1` 隔离）。
    const SESSION_ID: &str = "ses_actions_fixture";
    const DEVICE_ID: &str = "dev-iphone";
    const DEVICE_NAME: &str = "iPhone";
    const FORM_ID: &str = "frm_actions_fixture";
    const PERMISSION_ID: &str = "per_actions_fixture";
    const ITEM_ID: &str = "msg_actions_queue_item";
    const SERVICE_ID: &str = "87bd7e45-27bf-490d-bced-58d79c108910";
    const PID: u32 = 60541;
    const STARTED_AT_MS: i64 = 1_793_289_345_000;
    /// 测试专用凭据：任何错误文案/日志里出现即失败。
    const PASSWORD: &str = "s3cret-credential-value";

    fn openapi_fixture() -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/opencode-native/openapi.contract.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("读取 openapi 夹具")
    }

    fn process_fact() -> Arc<dyn Fn(u32) -> Option<i64> + Send + Sync> {
        Arc::new(|pid: u32| {
            Some(if pid == PID {
                STARTED_AT_MS
            } else {
                STARTED_AT_MS + 60_000
            })
        })
    }

    fn card_for(session_id: &str, project: &std::path::Path) -> Session {
        Session {
            id: session_id.to_string(),
            agent_type: AgentType::OpenCode,
            project_name: "project".to_string(),
            project_path: project.to_string_lossy().into_owned(),
            title: Some("fixture".to_string()),
            git_branch: None,
            github_url: None,
            status: SessionStatus::Idle,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: "2026-10-30T00:00:00Z".to_string(),
            pid: 12345,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::Cli,
            jump_supported: true,
            unread: false,
            unread_turned_green_at_ms: None,
        }
    }

    fn actor() -> NativeActor {
        NativeActor {
            device_id: DEVICE_ID.to_string(),
            device_name: DEVICE_NAME.to_string(),
            signature_enabled: true,
        }
    }

    fn bound_target(project: &std::path::Path) -> NativeTarget {
        NativeTarget {
            runtime: RuntimeKey {
                service_id: SERVICE_ID.to_string(),
                pid: PID,
                started_at_ms: STARTED_AT_MS,
            },
            session_id: SESSION_ID.to_string(),
            directory: project.canonicalize().expect("归一项目目录"),
        }
    }

    /// 在受控目录里造一个指定逻辑大小的附件（稀疏文件：不实际占盘、不提交大文件）。
    fn controlled_attachment(project: &std::path::Path, name: &str, bytes: u64) -> String {
        let dir = project.join(".tuvis-attachments").join(SESSION_ID);
        std::fs::create_dir_all(&dir).expect("建受控附件目录");
        let path = dir.join(name);
        let file = std::fs::File::create(&path).expect("建附件");
        file.set_len(bytes).expect("设置附件逻辑大小");
        path.to_string_lossy().into_owned()
    }

    /// 任务 0 形态的字段：选项 + `custom:true` + `required:true`。
    fn text_field(key: &str, title: &str) -> Value {
        json!({
            "key": key,
            "title": title,
            "type": "string",
            "required": true,
            "options": [
                { "value": "red", "label": "red" },
                { "value": "blue", "label": "blue" }
            ],
            "custom": true,
        })
    }

    /// 现取当前绑定令牌（投影 token 含进程内随机锚，无法预计算）。
    async fn current_binding(rig: &Rig) -> String {
        rig.hub
            .view(&rig.card)
            .await
            .expect("首轮投影")
            .binding_token
    }

    fn send_cmd(action_id: &str, binding: &str, text: &str) -> NativeCommand {
        NativeCommand {
            action_id: action_id.to_string(),
            session_id: SESSION_ID.to_string(),
            binding_token: binding.to_string(),
            operation: NativeOperation::Send(SendPayload {
                text: text.to_string(),
                attachment_paths: Vec::new(),
                delivery: DeliveryKind::Queue,
                client_message_id: format!("msg_{action_id}"),
            }),
        }
    }

    fn send_cmd_with_attachments(
        action_id: &str,
        binding: &str,
        text: &str,
        paths: Vec<String>,
    ) -> NativeCommand {
        NativeCommand {
            action_id: action_id.to_string(),
            session_id: SESSION_ID.to_string(),
            binding_token: binding.to_string(),
            operation: NativeOperation::Send(SendPayload {
                text: text.to_string(),
                attachment_paths: paths,
                delivery: DeliveryKind::Queue,
                client_message_id: format!("msg_{action_id}"),
            }),
        }
    }

    fn answer_cmd(action_id: &str, binding: &str, token: &str, answers: Answers) -> NativeCommand {
        NativeCommand {
            action_id: action_id.to_string(),
            session_id: SESSION_ID.to_string(),
            binding_token: binding.to_string(),
            operation: NativeOperation::Answer(AnswerPayload {
                form_id: FORM_ID.to_string(),
                context_token: token.to_string(),
                answers,
            }),
        }
    }

    fn decide_cmd(action_id: &str, binding: &str, token: &str, decision: &str) -> NativeCommand {
        NativeCommand {
            action_id: action_id.to_string(),
            session_id: SESSION_ID.to_string(),
            binding_token: binding.to_string(),
            operation: NativeOperation::Decide(DecidePayload {
                request_id: PERMISSION_ID.to_string(),
                context_token: token.to_string(),
                decision: decision.to_string(),
            }),
        }
    }

    fn stop_cmd(action_id: &str, binding: &str) -> NativeCommand {
        NativeCommand {
            action_id: action_id.to_string(),
            session_id: SESSION_ID.to_string(),
            binding_token: binding.to_string(),
            operation: NativeOperation::StopSession(StopSessionPayload {}),
        }
    }

    fn set_agent_cmd(action_id: &str, binding: &str, agent: &str) -> NativeCommand {
        NativeCommand {
            action_id: action_id.to_string(),
            session_id: SESSION_ID.to_string(),
            binding_token: binding.to_string(),
            operation: NativeOperation::SetAgent(SetAgentPayload {
                agent: agent.to_string(),
            }),
        }
    }

    fn retract_cmd(action_id: &str, binding: &str) -> NativeCommand {
        NativeCommand {
            action_id: action_id.to_string(),
            session_id: SESSION_ID.to_string(),
            binding_token: binding.to_string(),
            operation: NativeOperation::Retract(RetractPayload {
                item_id: ITEM_ID.to_string(),
            }),
        }
    }

    fn q0_blue() -> Answers {
        let mut answers = Answers::new();
        answers.insert("q0".to_string(), DraftAnswer::Text("blue".to_string()));
        answers
    }

    /// 既有命令的绑定令牌由测试现取（投影 token 含进程内随机锚，无法预计算）。
    fn binding_and_form_token(view: &PublicNativeView) -> (String, String) {
        (view.binding_token.clone(), form_token_of(view))
    }

    /// 投影里第一个 pending 项的上下文令牌（夹具只放一项，取错形状立刻panic）。
    fn form_token_of(view: &PublicNativeView) -> String {
        match &view.pending[0] {
            PendingView::Form { context_token, .. } => context_token.clone(),
            other => panic!("期望表单分支，实际 {other:?}"),
        }
    }

    /// 投影里第一个 pending 审批项的上下文令牌。
    fn permission_token_of(view: &PublicNativeView) -> String {
        match &view.pending[0] {
            PendingView::Permission { context_token, .. } => context_token.clone(),
            other => panic!("期望审批分支，实际 {other:?}"),
        }
    }

    // ---------------- 假 peer ----------------

    /// 一次已解析请求（方法 + 目标 + JSON 体）：断言以线上真实请求为准。
    #[derive(Clone, Debug)]
    struct Recorded {
        method: String,
        target: String,
        body: Option<Value>,
    }

    impl Recorded {
        fn path(&self) -> &str {
            self.target.split('?').next().unwrap_or("")
        }
    }

    /// 表单世界项（列表形态 + 权威状态 + 官方答案 + reply 行为）。
    struct FormEntry {
        spec: Value,
        status: String,
        /// `state.answer` 原值（R5b 回读）：None = 服务端未给答案。
        answer: Option<Value>,
        detail_status: Option<u16>,
        reply: Reply,
        /// reply 之后让 detail 读失败（制造「无法读取终态」）。
        detail_fails_after_reply: bool,
    }

    struct PermissionEntry {
        spec: Value,
        reply: Reply,
    }

    /// 写端点的行为词（任务 0 金样：204 / 409 / 400 + 丢响应）。
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Reply {
        /// 204：生效（表单 → answered；审批 → 离开 pending 列表）。
        Applied,
        /// 204 但回读列表里**保留**该审批且归属字段缺失：归属不可证（R3 反例）。
        AppliedUnownedMissing,
        /// 204 但回读列表里**保留**该审批且归属字段是非字符串：归属不可证（R3 反例）。
        AppliedUnownedNonString,
        /// 204 但回读列表里该审批改挂到**已知其他会话**：过滤，但归属可证。
        AppliedOtherSession,
        /// 2xx 且把权威状态改成给出的词（含契约外的未知词，R3 反例）。
        AppliedWithStatus(&'static str),
        /// 409：已定案（不改状态）。
        Conflict,
        /// 400：拒绝（不改状态）。
        BadRequest,
        /// 竞答迟到：先按他端定案落态，再回 409。
        SettledThenConflict,
    }

    /// prompt 端点行为词。
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum PromptMode {
        /// 入库并回 200（回显调用方 id）。
        Ingested,
        /// 入库后断开连接（丢响应：服务端已收到，客户端看不到应答）。
        IngestedThenDropped,
        /// 入库但改用服务端自分的 id（同正文不同 id 反例）。
        IngestedWithForeignId,
        /// 不入库，409。
        Conflict,
        /// 不入库，400。
        BadRequest,
        /// 正常受理入原生队列，消息记录尚未出现。
        Queued,
        /// 只落 inbox、不入消息列表，然后断开连接（丢回执；回读只能证明「已入队」）。
        QueuedThenDropped,
    }

    struct World {
        session_id: String,
        directory: String,
        pid: u32,
        service_id: String,
        /// 换代际开关：写请求生效后立即换 service id + pid（模拟同端口服务重启）。
        switch_generation_on_write: bool,
        /// `/api/info` 的 pid 覆盖（R2 冒名反例）：Some = info 自报他者 pid，
        /// 登记文件滞留不变——同端口冒名服务的信息不一致形态。
        info_pid: Option<u32>,
        /// 第二会话（R2 桌面切换模型）：`(session_id, directory)`，只服务投影所需只读端点。
        second: Option<(String, String)>,
        openapi: String,
        messages: Vec<Value>,
        forms: Vec<FormEntry>,
        /// 会话级表单列表的状态码（None = 200）。
        forms_status: Option<u16>,
        /// 全局 `/api/form` 回退的状态码（None = 200 且回显绑定目录）。
        global_forms_status: Option<u16>,
        permissions: Vec<PermissionEntry>,
        inbox: Vec<Value>,
        prompt: PromptMode,
        delete_status: u16,
        agent: String,
        busy: Option<i64>,
        idle: Option<i64>,
        /// F12 写门探针会话槽位（跨换代保留；换代后门会新建，槽位覆盖即可）。
        probe: Option<WorldProbe>,
        /// H5：stop POST 记录请求后直接断开（丢回执：服务端已收到，客户端看不到应答）。
        drop_interrupt_reply: bool,
    }

    /// F12 探针会话槽位（World 假服务面）。
    struct WorldProbe {
        id: String,
        title: String,
        /// 自动 ack 的锁定 TUI pid。
        ack_pid: u32,
        auto_ack: bool,
    }

    impl World {
        fn new() -> Self {
            Self {
                session_id: SESSION_ID.to_string(),
                // directory 由 boot 设为夹具项目目录（绑定与全局回退都回显它）。
                directory: String::new(),
                pid: PID,
                service_id: SERVICE_ID.to_string(),
                switch_generation_on_write: false,
                info_pid: None,
                second: None,
                openapi: openapi_fixture(),
                messages: Vec::new(),
                forms: Vec::new(),
                forms_status: None,
                global_forms_status: None,
                permissions: Vec::new(),
                inbox: Vec::new(),
                prompt: PromptMode::Ingested,
                drop_interrupt_reply: false,
                delete_status: 204,
                agent: "build".to_string(),
                busy: None,
                idle: None,
                probe: None,
            }
        }
    }

    fn form_entry() -> FormEntry {
        FormEntry {
            spec: json!({
                "id": FORM_ID,
                "sessionID": SESSION_ID,
                "title": "Questions",
                "fields": [text_field("q0", "COLOR")],
            }),
            status: "pending".to_string(),
            answer: None,
            detail_status: None,
            reply: Reply::Applied,
            detail_fails_after_reply: false,
        }
    }

    fn permission_entry() -> PermissionEntry {
        PermissionEntry {
            spec: json!({
                "id": PERMISSION_ID,
                "sessionID": SESSION_ID,
                "action": "read",
                "resources": [".env"],
            }),
            reply: Reply::Applied,
        }
    }

    fn inbox_item(id: &str, text: &str) -> Value {
        json!({ "id": id, "type": "user", "payload": { "text": text }, "delivery": "queue" })
    }

    fn user_record(id: &str, text: &str) -> Value {
        json!({ "id": id, "type": "user", "time": { "created": 1_793_289_224_800i64 }, "text": text })
    }

    struct Shared {
        world: Mutex<World>,
        log: Mutex<Vec<Recorded>>,
        offline: AtomicBool,
        endpoint: String,
        port: u16,
        /// 登记文件路径：换代际时由假 peer 自己重写（模拟服务重启后的新登记）。
        state_file: std::path::PathBuf,
        /// 组合反例：把一次 form reply 的**应用与响应**都挂起，直到测试释放。
        hold_form_reply: Mutex<Option<FormReplyHold>>,
        hold_form_detail: Mutex<Option<FormReplyHold>>,
        hold_info: Mutex<Option<FormReplyHold>>,
    }

    struct FormReplyHold {
        entered: Arc<AtomicBool>,
        release: Arc<Notify>,
    }

    /// 换代核：换 service id + pid 并重写登记文件（同端口、新代际、旧绑定失效）。
    /// 进程启动时间由 `process_fact` 按 pid 推导，pid 变即 started_at 也变。
    fn bump_generation(shared: &Shared, world: &mut World) {
        world.pid += 9_000;
        world.service_id = format!("svc-restarted-{pid}", pid = world.pid);
        let raw = format!(
            r#"{{"id":"{id}","version":"2.0.22","url":"http://127.0.0.1:{port}","pid":{pid},"password":"{PASSWORD}"}}"#,
            id = world.service_id,
            port = shared.port,
            pid = world.pid,
        );
        if let Some(parent) = shared.state_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&shared.state_file, raw).expect("重写登记文件");
    }

    /// 写路径上的条件切换：仅当世界打开 `switch_generation_on_write` 时才换代（否则 no-op）。
    fn switch_generation(shared: &Shared, world: &mut World) {
        if world.switch_generation_on_write {
            bump_generation(shared, world);
        }
    }

    struct Peer {
        shared: Arc<Shared>,
        port: u16,
    }

    impl Peer {
        async fn start(world: World, state_file: &std::path::Path) -> Self {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind peer");
            let port = listener.local_addr().expect("local addr").port();
            let shared = Arc::new(Shared {
                world: Mutex::new(world),
                log: Mutex::new(Vec::new()),
                offline: AtomicBool::new(false),
                endpoint: format!("http://127.0.0.1:{port}"),
                port,
                state_file: state_file.to_path_buf(),
                hold_form_reply: Mutex::new(None),
                hold_form_detail: Mutex::new(None),
                hold_info: Mutex::new(None),
            });
            let accepted = Arc::clone(&shared);
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        break;
                    };
                    let shared = Arc::clone(&accepted);
                    tokio::spawn(async move { serve_connection(&mut stream, shared).await });
                }
            });
            Self { shared, port }
        }

        fn set(&self, edit: impl FnOnce(&mut World)) {
            edit(&mut self.shared.world.lock().unwrap());
        }

        /// 武装一次 form reply 挂起：返回 (已进入, 释放器)。测试在「已进入」后
        /// 才能断言在飞状态；释放后 peer 才应用状态并回响应。
        fn arm_form_reply_hold(&self) -> (Arc<AtomicBool>, Arc<Notify>) {
            let entered = Arc::new(AtomicBool::new(false));
            let release = Arc::new(Notify::new());
            *self.shared.hold_form_reply.lock().unwrap() = Some(FormReplyHold {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            });
            (entered, release)
        }

        /// 重启服务：换 service id + pid 并重写登记文件（同端口新代际，旧绑定失效）。
        fn restart(&self) {
            let mut world = self.shared.world.lock().unwrap();
            bump_generation(&self.shared, &mut world);
        }

        fn log(&self) -> Vec<Recorded> {
            self.shared.log.lock().unwrap().clone()
        }

        /// 命中某路径 + 方法的请求数。
        fn count(&self, method: &str, path: &str) -> usize {
            self.shared
                .log
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.method == method && request.path() == path)
                .count()
        }

        /// 非 GET 请求总数（写路径只允许有计划的那几次）。
        fn non_get(&self) -> usize {
            self.shared
                .log
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.method != "GET")
                .count()
        }

        fn total(&self) -> usize {
            self.shared.log.lock().unwrap().len()
        }

        fn prompt_posts(&self) -> usize {
            self.count("POST", &format!("/api/session/{SESSION_ID}/prompt"))
        }

        /// F12：开写门探针面（创建/读/写端点可用），`ack_pid` = 自动 ack 的锁定 TUI pid。
        fn arm_probe_w2(&self, ack_pid: u32) {
            self.shared.world.lock().unwrap().probe = Some(WorldProbe {
                id: "ses_probe_tuvis".to_string(),
                title: "probe:tuvis".to_string(),
                ack_pid,
                auto_ack: true,
            });
        }

        fn form_reply_posts(&self) -> usize {
            self.count(
                "POST",
                &format!("/api/session/{SESSION_ID}/form/{FORM_ID}/reply"),
            )
        }
    }

    async fn serve_connection(stream: &mut tokio::net::TcpStream, shared: Arc<Shared>) {
        loop {
            let Some(request) = read_request(stream).await else {
                return;
            };
            let path = request.path().to_string();
            let query = request
                .target
                .split_once('?')
                .map(|(_, query)| query.to_string())
                .unwrap_or_default();
            shared.log.lock().unwrap().push(request.clone());
            if shared.offline.load(Ordering::SeqCst) {
                return;
            }
            if request.method == "GET" && path == "/api/info" {
                let hold = shared.hold_info.lock().unwrap().take();
                if let Some(hold) = hold {
                    hold.entered.store(true, Ordering::SeqCst);
                    hold.release.notified().await;
                }
            }
            if request.method == "GET"
                && path == format!("/api/session/{SESSION_ID}/form/{FORM_ID}")
            {
                let hold = shared.hold_form_detail.lock().unwrap().take();
                if let Some(hold) = hold {
                    hold.entered.store(true, Ordering::SeqCst);
                    hold.release.notified().await;
                }
            }
            // 组合反例：form reply 在应用状态**之前**挂起，模拟「请求已到、原写仍在飞」。
            if request.method == "POST" && path.contains("/form/") && path.ends_with("/reply") {
                let hold = shared.hold_form_reply.lock().unwrap().take();
                if let Some(hold) = hold {
                    hold.entered.store(true, Ordering::SeqCst);
                    hold.release.notified().await;
                }
            }
            // None = 记录后直接断开（丢响应）。
            let Some((status, body)) =
                reply(&shared, &request.method, &path, &query, &request.body)
            else {
                return;
            };
            if write_raw(stream, status, &body).await.is_err() {
                return;
            }
        }
    }

    /// 路由一个请求。返回 `None` = 不发响应、直接断开（制造丢响应）。
    fn reply(
        shared: &Shared,
        method: &str,
        path: &str,
        query: &str,
        body: &Option<Value>,
    ) -> Option<(u16, String)> {
        let mut world = shared.world.lock().unwrap();
        let session = format!("/api/session/{}", world.session_id);
        // F12 写门探针回声面：POST /api/session 建探针；GET/PATCH 探针槽位
        // （auto_ack 时 challenge → 合法 ack，模拟插件读到 challenge 后的翻转）。
        if method == "POST" && path == "/api/session" {
            let creating_probe = body
                .as_ref()
                .and_then(|value| value.get("title"))
                .and_then(Value::as_str)
                == Some("probe:tuvis");
            if creating_probe {
                let slot = world.probe.get_or_insert_with(|| WorldProbe {
                    id: "ses_probe_tuvis".to_string(),
                    title: "probe:tuvis".to_string(),
                    ack_pid: 0,
                    auto_ack: true,
                });
                let id = slot.id.clone();
                return Some((200, json!({ "data": { "id": id } }).to_string()));
            }
        }
        if let Some(slot_id) = world.probe.as_ref().map(|slot| slot.id.clone()) {
            if path == format!("/api/session/{slot_id}") {
                if method == "GET" {
                    let title = world.probe.as_ref().expect("probe 在场").title.clone();
                    return Some((
                        200,
                        json!({ "data": { "id": slot_id, "title": title } }).to_string(),
                    ));
                }
                if method == "PATCH" {
                    let title = body
                        .as_ref()
                        .and_then(|value| value.get("title"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let slot = world.probe.as_mut().expect("probe 在场");
                    if slot.auto_ack {
                        if let crate::services::opencode_native::binding_gate::ProbeTitle::Challenge {
                            nonce,
                            ..
                        } = crate::services::opencode_native::binding_gate::parse_probe_title(&title)
                        {
                            let acktime = chrono::Utc::now().timestamp_millis();
                            slot.title = format!("ack:{nonce}:{}:{acktime}", slot.ack_pid);
                            return Some((204, String::new()));
                        }
                    }
                    slot.title = title;
                    return Some((204, String::new()));
                }
            }
        }
        if path == "/openapi.json" {
            return Some((200, world.openapi.clone()));
        }
        if path == "/api/info" {
            let body = json!({
                "version": "2.0.22",
                "pid": world.info_pid.unwrap_or(world.pid),
                "urls": [shared.endpoint.clone()],
            });
            return Some((200, body.to_string()));
        }
        // 第二会话（R2 桌面切换模型）：只服务投影所需只读端点；写端点一律落到 404，
        // 但请求仍进 log——「对 B 的写次数为零」用 log 断言。
        if let Some((second_id, second_directory)) = world.second.clone() {
            let prefix = format!("/api/session/{second_id}");
            if path == prefix {
                let body = json!({
                    "id": second_id,
                    "location": { "directory": second_directory },
                });
                return Some((200, body.to_string()));
            }
            if path == format!("{prefix}/message") {
                let body = json!({
                    "data": [],
                    "cursor": { "previous": Value::Null, "next": Value::Null },
                });
                return Some((200, body.to_string()));
            }
            for suffix in ["/form", "/inbox", "/permission"] {
                if path == format!("{prefix}{suffix}") {
                    return Some((200, json!({ "data": [] }).to_string()));
                }
            }
        }
        // 全局回退端点：location scope 回显**请求里**的目录（真实服务按请求作用域回显；
        // R2 第二会话的投影用不同目录打同一端点，必须能对上）。URL 编码的路径段还原。
        let scoped_directory = query
            .split('&')
            .find_map(|pair| {
                let (name, value) = pair.split_once('=')?;
                (name == "location[directory]")
                    .then(|| value.replace("%2F", "/").replace("%20", " "))
            })
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| world.directory.clone());
        if path == "/api/form" {
            return Some(match world.global_forms_status {
                Some(status) => (status, "{}".to_string()),
                None => (
                    200,
                    json!({ "location": { "directory": scoped_directory }, "data": [] })
                        .to_string(),
                ),
            });
        }
        if path == "/api/permission/request" {
            let body = json!({ "location": { "directory": scoped_directory }, "data": [] });
            return Some((200, body.to_string()));
        }
        if path == session {
            let body = json!({
                "id": world.session_id,
                "location": { "directory": world.directory },
                "agent": world.agent,
                "time": { "busy": world.busy, "idle": world.idle },
            });
            return Some((200, body.to_string()));
        }
        if path == format!("{session}/message") {
            let body = json!({
                "data": world.messages,
                "cursor": { "previous": Value::Null, "next": Value::Null },
            });
            return Some((200, body.to_string()));
        }
        if path == format!("{session}/form") {
            return Some(match world.forms_status {
                Some(status) => (status, "{}".to_string()),
                None => (
                    200,
                    json!({ "data": world.forms.iter().map(|form| form.spec.clone()).collect::<Vec<_>>() })
                        .to_string(),
                ),
            });
        }
        if let Some(form_id) = path
            .strip_prefix(&format!("{session}/form/"))
            .filter(|rest| !rest.ends_with("/reply"))
        {
            let Some(entry) = world
                .forms
                .iter()
                .find(|form| form.spec.get("id").and_then(Value::as_str) == Some(form_id))
            else {
                return Some((404, "{}".to_string()));
            };
            return Some(match entry.detail_status {
                Some(status) => (status, "{}".to_string()),
                None => {
                    let mut data = entry.spec.clone();
                    let mut state = json!({ "status": entry.status });
                    if let Some(answer) = &entry.answer {
                        state["answer"] = answer.clone();
                    }
                    data["state"] = state;
                    (200, json!({ "data": data }).to_string())
                }
            });
        }
        if path == format!("{session}/form/{FORM_ID}/reply") && method == "POST" {
            let outcome = handle_form_reply(&mut world);
            switch_generation(shared, &mut world);
            return Some(outcome);
        }
        if path == format!("{session}/permission") {
            let data: Vec<Value> = world
                .permissions
                .iter()
                .map(|permission| permission.spec.clone())
                .collect();
            return Some((200, json!({ "data": data }).to_string()));
        }
        if path == format!("{session}/permission/{PERMISSION_ID}/reply") && method == "POST" {
            let outcome = handle_permission_reply(&mut world);
            switch_generation(shared, &mut world);
            return Some(outcome);
        }
        if path == format!("{session}/inbox") {
            return Some((200, json!({ "data": world.inbox }).to_string()));
        }
        if method == "DELETE" {
            if let Some(item_id) = path.strip_prefix(&format!("{session}/inbox/")) {
                // 任务 0：DELETE 对排队项即移除；对未知/已投递 id 是 no-op（都回 204）。
                if (200..300).contains(&world.delete_status) {
                    world
                        .inbox
                        .retain(|item| item.get("id").and_then(Value::as_str) != Some(item_id));
                }
                let outcome = (world.delete_status, String::new());
                switch_generation(shared, &mut world);
                return Some(outcome);
            }
        }
        if method == "PATCH" {
            if let Some(item_id) = path.strip_prefix(&format!("{session}/inbox/")) {
                // 原生插队：把既有项的 delivery 改成 body.delivery（真实上游同样只改这一个字段）。
                let next = body
                    .as_ref()
                    .and_then(|b| b.get("delivery"))
                    .and_then(Value::as_str)
                    .unwrap_or("steer")
                    .to_string();
                for item in world.inbox.iter_mut() {
                    if item.get("id").and_then(Value::as_str) == Some(item_id) {
                        item["delivery"] = json!(next);
                    }
                }
                let outcome = (200, String::new());
                switch_generation(shared, &mut world);
                return Some(outcome);
            }
        }
        if path == format!("{session}/prompt") && method == "POST" {
            let outcome = handle_prompt(&mut world, body);
            switch_generation(shared, &mut world);
            return outcome;
        }
        // H5 停止端点（真实契约：`interrupted` = 收到停止时是否有活动回合）。
        if path == format!("{session}/interrupt") && method == "POST" {
            if world.drop_interrupt_reply {
                // 丢回执：请求已记录（写确已发出），响应被切断——客户端结果未知。
                return None;
            }
            let interrupted = world.busy.is_some();
            return Some((200, json!({ "interrupted": interrupted }).to_string()));
        }
        let _ = query;
        Some((404, "{}".to_string()))
    }

    fn ingest_message(world: &mut World, id: &str, text: &str) {
        world.messages.push(user_record(id, text));
    }

    fn handle_prompt(world: &mut World, body: &Option<Value>) -> Option<(u16, String)> {
        let text = body
            .as_ref()
            .and_then(|body| body.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match world.prompt {
            PromptMode::Ingested | PromptMode::IngestedThenDropped => {
                let id = body
                    .as_ref()
                    .and_then(|body| body.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("msg_unknown")
                    .to_string();
                ingest_message(world, &id, &text);
                if world.prompt == PromptMode::IngestedThenDropped {
                    return None;
                }
                let echo = json!({
                    "data": {
                        "id": id,
                        "sessionID": world.session_id,
                        "type": "user",
                        "payload": { "text": text },
                        "delivery": "queue",
                    }
                });
                Some((200, echo.to_string()))
            }
            PromptMode::IngestedWithForeignId => {
                let foreign = format!("msg_service_{}", world.messages.len());
                ingest_message(world, &foreign, &text);
                let echo = json!({
                    "data": {
                        "id": foreign,
                        "sessionID": world.session_id,
                        "type": "user",
                        "payload": { "text": text },
                        "delivery": "queue",
                    }
                });
                Some((200, echo.to_string()))
            }
            PromptMode::Conflict => {
                Some((409, json!({ "_tag": "PromptConflictError" }).to_string()))
            }
            PromptMode::BadRequest => {
                Some((400, json!({ "_tag": "InvalidRequestError" }).to_string()))
            }
            PromptMode::Queued | PromptMode::QueuedThenDropped => {
                // 只入队、不入消息列表：回读能证明「已受理」，但证明不了「已投递」。
                let id = body
                    .as_ref()
                    .and_then(|body| body.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("msg_unknown")
                    .to_string();
                world.inbox.push(inbox_item(&id, &text));
                if world.prompt == PromptMode::QueuedThenDropped {
                    None
                } else {
                    Some((200,json!({"data":{"id":id,"sessionID":world.session_id,"type":"user","payload":{"text":text},"delivery":"queue"}}).to_string()))
                }
            }
        }
    }

    fn handle_form_reply(world: &mut World) -> (u16, String) {
        let Some(entry) = world.forms.iter_mut().next() else {
            return (404, "{}".to_string());
        };
        match entry.reply {
            Reply::Applied => {
                entry.status = "answered".to_string();
                (204, String::new())
            }
            Reply::Conflict => {
                if entry.detail_fails_after_reply {
                    entry.detail_status = Some(500);
                }
                (
                    409,
                    json!({ "_tag": "FormAlreadySettledError", "id": FORM_ID }).to_string(),
                )
            }
            Reply::BadRequest => (
                400,
                json!({ "_tag": "FormInvalidAnswerError", "message": "Missing required form field: q1" })
                    .to_string(),
            ),
            Reply::SettledThenConflict => {
                entry.status = "answered".to_string();
                if entry.detail_fails_after_reply {
                    entry.detail_status = Some(500);
                }
                (
                    409,
                    json!({ "_tag": "FormAlreadySettledError", "id": FORM_ID }).to_string(),
                )
            }
            Reply::AppliedWithStatus(status) => {
                entry.status = status.to_string();
                (200, String::new())
            }
            Reply::AppliedUnownedMissing
            | Reply::AppliedUnownedNonString
            | Reply::AppliedOtherSession => unreachable!("approval-only reply words"),
        }
    }

    fn handle_permission_reply(world: &mut World) -> (u16, String) {
        let Some(entry) = world.permissions.iter().position(|_| true) else {
            return (404, "{}".to_string());
        };
        match world.permissions[entry].reply {
            Reply::Applied => {
                world.permissions.clear();
                (204, String::new())
            }
            Reply::AppliedUnownedMissing => {
                if let Some(object) = world.permissions[entry].spec.as_object_mut() {
                    object.remove("sessionID");
                }
                (204, String::new())
            }
            Reply::AppliedUnownedNonString => {
                world.permissions[entry].spec["sessionID"] = json!(42);
                (204, String::new())
            }
            Reply::AppliedOtherSession => {
                world.permissions[entry].spec["sessionID"] = json!("ses_other");
                (204, String::new())
            }
            Reply::Conflict | Reply::SettledThenConflict => {
                if world.permissions[entry].reply == Reply::SettledThenConflict {
                    world.permissions.clear();
                }
                (
                    409,
                    json!({ "_tag": "PermissionAlreadySettledError", "id": PERMISSION_ID })
                        .to_string(),
                )
            }
            Reply::BadRequest => (
                400,
                json!({ "_tag": "InvalidRequestError", "field": "decision" }).to_string(),
            ),
            Reply::AppliedWithStatus(_) => unreachable!("form-only reply word"),
        }
    }

    async fn write_raw(
        stream: &mut tokio::net::TcpStream,
        status: u16,
        body: &str,
    ) -> std::io::Result<()> {
        let raw = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(raw.as_bytes()).await?;
        stream.flush().await
    }

    fn head_end(buffer: &[u8]) -> Option<usize> {
        buffer.windows(4).position(|window| window == b"\r\n\r\n")
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<Recorded> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(end) = head_end(&buffer) {
                let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
                let mut lines = head.split("\r\n");
                let mut request_line = lines.next()?.split_whitespace();
                let method = request_line.next()?.to_string();
                let target = request_line.next()?.to_string();
                let mut declared = 0usize;
                for line in lines {
                    let (name, value) = line.split_once(':')?;
                    if name.trim().eq_ignore_ascii_case("content-length") {
                        declared = value.trim().parse().ok()?;
                    }
                }
                let mut body = buffer[end + 4..].to_vec();
                while body.len() < declared {
                    let read = stream.read(&mut chunk).await.ok()?;
                    if read == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..read]);
                }
                body.truncate(declared);
                let body = (!body.is_empty())
                    .then(|| serde_json::from_slice::<Value>(&body).ok())
                    .flatten();
                return Some(Recorded {
                    method,
                    target,
                    body,
                });
            }
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
    }

    struct Rig {
        _tmp: tempfile::TempDir,
        project: PathBuf,
        card: Session,
        peer: Peer,
        hub: Arc<NativeHub>,
    }

    async fn boot(world: World) -> Rig {
        boot_with_fact(world, process_fact()).await
    }

    async fn boot_with_fact(world: World, fact: TwoStateFact) -> Rig {
        // 两态注入的默认桥与生产注入构造一致：None ⇒ ReadFailure（保守，fail-closed）。
        let bridged = Arc::clone(&fact);
        let generation: Arc<dyn Fn(u32) -> super::ProcessFactProbe + Send + Sync> =
            Arc::new(move |pid: u32| match bridged(pid) {
                Some(started) => super::ProcessFactProbe::Alive(started),
                None => super::ProcessFactProbe::ReadFailure,
            });
        boot_with_facts(world, fact, generation).await
    }

    /// G3：显式双缝装配——两态缝喂发现/门，三形态缝喂代际判定与 patch5 豁免。
    /// 「明确无此 pid」等死亡证明形态只能经三形态缝表达（见 `switchable_facts`）。
    async fn boot_with_facts(
        mut world: World,
        fact: TwoStateFact,
        generation_fact: Arc<dyn Fn(u32) -> super::ProcessFactProbe + Send + Sync>,
    ) -> Rig {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).expect("建项目目录");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("建 home");
        let state_file = tmp.path().join("state/opencode/service.json");
        let card = card_for(SESSION_ID, &project);
        world.directory = project.to_string_lossy().into_owned();
        let peer = Peer::start(world, &state_file).await;
        write_registration(&state_file, peer.port);
        let hub = NativeHub::with_coordinator_facts(
            DeviceStore::memory(),
            DiscoveryInput { state_file, home },
            fact,
            generation_fact,
        );
        hub.store().with(|c| {
            crate::remote::pairing::persist_device(
                c,
                &crate::remote::pairing::NewDevice {
                    id: DEVICE_ID.into(),
                    name: DEVICE_NAME.into(),
                    ua: "fixture".into(),
                    origin_ip: "127.0.0.1".into(),
                    via: "lan".into(),
                    paired_at: now_ms(),
                },
            )
            .unwrap()
        });
        Rig {
            _tmp: tmp,
            project,
            card,
            peer,
            hub: Arc::new(hub),
        }
    }

    fn write_registration(state_file: &std::path::Path, port: u16) {
        if let Some(parent) = state_file.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let raw = format!(
            r#"{{"id":"{SERVICE_ID}","version":"2.0.22","url":"http://127.0.0.1:{port}","pid":{PID},"password":"{PASSWORD}"}}"#
        );
        std::fs::write(state_file, raw).unwrap();
    }

    /// 认领路由（原生写入入口）：否则任何派发都应被 OC_ROUTE_NOT_OWNED 挡下。
    fn claim_route(rig: &Rig) {
        assert!(
            rig.hub
                .action_store()
                .try_claim_route(&bound_target(&rig.project), true)
                .expect("认领路由"),
            "测试装配必须先拥有写入入口"
        );
    }

    /// 开启签名开关（设置 KV 单点）。
    fn set_signature(rig: &Rig, on: bool) {
        rig.hub.store().with(|conn| {
            crate::database::dao::settings::set_setting_conn(
                conn,
                "remote_message_signature",
                if on { "on" } else { "off" },
            )
        });
    }

    fn inject_queue_rows(rig: &Rig) -> i64 {
        rig.hub.store().with(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM inject_queue WHERE session_id = ?1",
                [SESSION_ID],
                |row| row.get(0),
            )
            .unwrap()
        })
    }

    fn ledger_rows(rig: &Rig) -> i64 {
        rig.hub.store().with(|conn| {
            conn.query_row("SELECT COUNT(*) FROM opencode_native_actions", [], |row| {
                row.get(0)
            })
            .unwrap()
        })
    }

    /// 对外字符串（状态/理由码）不得声称哪端获胜或执行被取消。
    fn assert_no_victory_or_cancel_claim(view: &ActionView) {
        let rendered = serde_json::to_string(view).expect("序列化 ActionView");
        for forbidden in ["won", "victory", "other side", "对方", "获胜", "cancel"] {
            assert!(
                !rendered.to_lowercase().contains(&forbidden.to_lowercase()),
                "对外字符串不得声称 {forbidden}：{rendered}"
            );
        }
    }

    async fn verify_does_not_finalize_when_process_changes_before_store_lock(decide: bool) {
        use std::sync::atomic::AtomicI64;
        let started = Arc::new(AtomicI64::new(STARTED_AT_MS));
        let fact = started.clone();
        let mut world = World::new();
        world.prompt = PromptMode::IngestedThenDropped;
        let rig = boot_with_fact(world, Arc::new(move |_| Some(fact.load(Ordering::SeqCst)))).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let result = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("verify_process_race", &binding, "test"),
            )
            .await
            .unwrap();
        assert_eq!(result.state, ActionState::Unconfirmed);
        if decide {
            rig.hub.store().with(|conn| conn.execute("UPDATE opencode_native_actions SET operation='decide', request_key='per-missing' WHERE action_id='verify_process_race'", []).unwrap());
        }
        let snapshot = rig.hub.dispatch_snapshot(&rig.card).await.unwrap();
        let source = rig.hub.snapshot_source(&snapshot).unwrap();
        let conn = match rig.hub.store() {
            DeviceStore::Owned(conn) => conn.clone(),
            _ => panic!("必须使用隔离内存库"),
        };
        let entered = Arc::new(Notify::new());
        let entered_hook = entered.clone();
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let (blocked, observed) = std::sync::mpsc::channel::<()>();
        let wait = Mutex::new(wait);
        let hook_conn = conn.clone();
        rig.hub.set_verify_before_store_hook(Arc::new(move || {
            entered_hook.notify_one();
            wait.lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            assert!(
                hook_conn.try_lock().is_err(),
                "核对落账时数据库锁必须被占用"
            );
            blocked.send(()).unwrap();
        }));
        let hub = rig.hub.clone();
        let task = tokio::spawn(async move { hub.verify(DEVICE_ID, "verify_process_race").await });
        tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
            .await
            .expect("核对必须读完并通过首次进程检查");
        let guard = conn.lock().unwrap();
        release.send(()).unwrap();
        observed
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("核对必须遇到锁竞争");
        started.store(STARTED_AT_MS + 1, Ordering::SeqCst);
        drop(guard);
        let verified = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let stored = rig
            .hub
            .action(DEVICE_ID, "verify_process_race")
            .await
            .unwrap();
        let after = rig
            .hub
            .snapshot_source(&snapshot)
            .expect("source 必须仍在册");
        assert!(
            Arc::ptr_eq(&source, &after),
            "仅进程代际变化，source Arc 必须保持不变"
        );
        assert_eq!(
            (verified.state, stored.state, rig.peer.prompt_posts()),
            (ActionState::Unconfirmed, ActionState::Unconfirmed, 1),
            "进程代际改变后旧回读不能确认或定案，也不能重发"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn verify_does_not_confirm_when_process_changes_before_store_lock() {
        verify_does_not_finalize_when_process_changes_before_store_lock(false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn verify_does_not_settle_when_process_changes_before_store_lock() {
        verify_does_not_finalize_when_process_changes_before_store_lock(true).await;
    }

    /// 丢响应后绝不重试：POST 恒为 1，未知结果只由只读核对收敛。
    #[tokio::test]
    async fn lost_prompt_response_is_not_retried() {
        let mut world = World::new();
        world.prompt = PromptMode::IngestedThenDropped;
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;

        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_lost", &binding, "第一条消息"),
            )
            .await
            .expect("丢响应不是错误，而是未知结果");
        assert_eq!(view.state, ActionState::Unconfirmed, "丢响应只能记未知");
        assert_eq!(rig.peer.prompt_posts(), 1, "只允许发生一次派发");

        // 只读核对：消息已按精确 id 入库 → Confirmed，且绝不再发一次。
        let verified = rig
            .hub
            .verify(DEVICE_ID, "act_lost")
            .await
            .expect("核对不得失败");
        assert_eq!(verified.state, ActionState::Confirmed);
        assert_eq!(verified.native_id.as_deref(), Some("msg_act_lost"));
        assert_eq!(rig.peer.prompt_posts(), 1, "核对不得重发");

        // 重复点击同一命令：一次派发，绝不重放。
        let again = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_lost", &binding, "第一条消息"),
            )
            .await
            .expect("重复点击返回既有动作");
        assert_eq!(again.state, ActionState::Confirmed);
        assert_eq!(rig.peer.prompt_posts(), 1, "重复点击不得二次派发");
        assert_eq!(ledger_rows(&rig), 1, "同一动作只有一行");
    }

    /// 登记移除但原服务仍在线：重复提交不重发，只读核对可确认精确消息。
    #[tokio::test]
    async fn unconfirmed_duplicate_survives_service_loss_without_redispatch() {
        let mut world = World::new();
        world.prompt = PromptMode::IngestedThenDropped;
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let command = send_cmd("act_offline_duplicate", &binding, "原意图");
        let original = rig
            .hub
            .execute(&actor(), &rig.card, command.clone())
            .await
            .unwrap();
        assert_eq!(original.state, ActionState::Unconfirmed);
        assert_eq!(rig.peer.non_get(), 1);

        std::fs::remove_file(&rig.peer.shared.state_file).expect("移除临时服务登记");

        let other_intent = send_cmd("act_offline_duplicate", &binding, "不同意图");
        assert!(rig
            .hub
            .execute(&actor(), &rig.card, other_intent)
            .await
            .is_err());
        let mut other_actor = actor();
        other_actor.device_id = "other-device".to_string();
        assert!(rig
            .hub
            .execute(&other_actor, &rig.card, command.clone())
            .await
            .is_err());
        assert!(rig
            .hub
            .action("other-device", "act_offline_duplicate")
            .await
            .is_err());
        let repeated = rig
            .hub
            .execute(&actor(), &rig.card, command.clone())
            .await
            .unwrap();
        // 核对前重复请求返回原未知动作（M3：既有行视图带行代际呈现面——登记移除但
        // 原进程经 process_fact 实测仍以账本身份存活（F12 项 2：OriginalAlive）⇒
        // Some(false)，呈现面收回；fresh 派发回执不携带，故逐字段比对承载语义字段）。
        assert_eq!(
            repeated.state, original.state,
            "核对前重复请求返回原未知动作"
        );
        assert_eq!(repeated.reason_code, original.reason_code);
        assert_eq!(repeated.native_id, original.native_id);
        assert_eq!(repeated.action_id, original.action_id);
        assert_eq!(
            repeated.runtime_dead,
            Some(false),
            "原进程实测仍活（pid+lstart 全等）⇒ OriginalAlive，不得宣告死亡"
        );
        assert_eq!(rig.peer.non_get(), 1, "重复请求不得增加写入");

        let before_verify = rig.peer.log().len();
        let verified = rig
            .hub
            .verify(DEVICE_ID, "act_offline_duplicate")
            .await
            .unwrap();
        assert_eq!(verified.state, ActionState::Confirmed);
        assert_eq!(
            verified.native_id.as_deref(),
            Some("msg_act_offline_duplicate")
        );
        let log = rig.peer.log();
        assert!(
            log[before_verify..]
                .iter()
                .any(|request| request.method == "GET"
                    && request.path() == format!("/api/session/{SESSION_ID}/message")),
            "核对必须读取权威消息列表"
        );
        assert!(
            log[before_verify..]
                .iter()
                .all(|request| request.method == "GET"),
            "核对只能只读"
        );
        assert_eq!(rig.peer.non_get(), 1, "核对不得增加写入");

        let repeated = rig.hub.execute(&actor(), &rig.card, command).await.unwrap();
        assert_eq!(repeated, verified, "确认后重复请求返回已确认动作");
        assert_eq!(rig.peer.non_get(), 1, "确认后重复请求不得重发");
        assert_eq!(ledger_rows(&rig), 1);
    }

    /// 原服务离线且进程事实不可用：只读核对和重复提交均保持未知。
    #[tokio::test]
    async fn unconfirmed_duplicate_stays_unknown_when_service_and_process_are_unavailable() {
        let available = Arc::new(AtomicBool::new(true));
        let fact_available = available.clone();
        let mut world = World::new();
        world.prompt = PromptMode::IngestedThenDropped;
        let rig = boot_with_fact(
            world,
            Arc::new(move |pid| {
                fact_available
                    .load(Ordering::SeqCst)
                    .then_some(if pid == PID {
                        STARTED_AT_MS
                    } else {
                        STARTED_AT_MS + 60_000
                    })
            }),
        )
        .await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let command = send_cmd("act_truly_offline", &binding, "原意图");
        let original = rig
            .hub
            .execute(&actor(), &rig.card, command.clone())
            .await
            .unwrap();
        assert_eq!(original.state, ActionState::Unconfirmed);
        assert_eq!(rig.peer.non_get(), 1);

        rig.peer.shared.offline.store(true, Ordering::SeqCst);
        available.store(false, Ordering::SeqCst);
        std::fs::remove_file(&rig.peer.shared.state_file).expect("移除临时服务登记");

        let verified = rig
            .hub
            .verify(DEVICE_ID, "act_truly_offline")
            .await
            .unwrap();
        // 核对零改写（M3：回执额外携带行代际呈现面，逐字段比对承载语义的字段）。
        assert_eq!(
            verified.state, original.state,
            "无法取得权威事实时必须保持未知"
        );
        assert_eq!(verified.reason_code, original.reason_code);
        assert_eq!(verified.native_id, original.native_id);
        assert_eq!(verified.action_id, original.action_id);
        assert_eq!(
            verified.runtime_dead,
            None,
            "登记移除 + 进程事实不可得 ⇒ InsufficientEvidence（F12 项 2 fail-closed）：未判定 ≠ 死亡，不渲染出口"
        );
        assert_eq!(rig.peer.non_get(), 1, "核对不得增加写入");
        let repeated = rig.hub.execute(&actor(), &rig.card, command).await.unwrap();
        // 离线时重复请求保持未知（M3：既有行视图带呈现面，与核对同面）
        assert_eq!(repeated.state, original.state, "离线时重复请求必须保持未知");
        assert_eq!(repeated.reason_code, original.reason_code);
        assert_eq!(repeated.runtime_dead, None, "证据不足不装饰（与核对同面）");
        assert_eq!(rig.peer.non_get(), 1, "离线时不得重发");
        assert_eq!(ledger_rows(&rig), 1);
    }

    /// 409 → 只有回读到终态才 Settled；读不到仍 Unconfirmed；任何对外字符串不声称哪端获胜。
    #[tokio::test]
    async fn settled_response_reads_answer_without_claiming_other_won() {
        // 相位一：409 + 回读可判定终态 → Settled。
        let mut world = World::new();
        let mut form = form_entry();
        form.reply = Reply::SettledThenConflict;
        world.forms = vec![form];
        let rig = boot(world).await;
        claim_route(&rig);
        let first = rig.hub.view(&rig.card).await.expect("首轮投影");
        let (binding, token) = binding_and_form_token(&first);

        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                answer_cmd("act_race", &binding, &token, q0_blue()),
            )
            .await
            .expect("409 是已知结果，不是错误");
        assert_eq!(view.state, ActionState::Settled, "只有回读定案后才 Settled");
        assert_eq!(view.reason_code.as_deref(), Some("already_settled"));
        assert_no_victory_or_cancel_claim(&view);
        assert_eq!(rig.peer.form_reply_posts(), 1, "只提交一次");

        // 409 之后必须真的回读过 detail（Settled 的证据来自回读，不是 409 本身）。
        let log = rig.peer.log();
        let reply_at = log
            .iter()
            .position(|request| request.method == "POST" && request.path().ends_with("/reply"))
            .expect("必须发生过 reply POST");
        assert!(
            log[reply_at..].iter().any(|request| request.method == "GET"
                && request.path() == format!("/api/session/{SESSION_ID}/form/{FORM_ID}")),
            "409 之后必须回读表单终态"
        );

        // 相位二：409 + 终态读不到 → Unconfirmed（不猜、不宣称）。
        let mut world = World::new();
        let mut form = form_entry();
        form.reply = Reply::Conflict;
        form.detail_fails_after_reply = true;
        world.forms = vec![form];
        let rig = boot(world).await;
        claim_route(&rig);
        let first = rig.hub.view(&rig.card).await.expect("首轮投影");
        let (binding, token) = binding_and_form_token(&first);

        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                answer_cmd("act_blind", &binding, &token, q0_blue()),
            )
            .await
            .expect("409 + 读不到终态仍是未知结果");
        assert_eq!(view.state, ActionState::Unconfirmed);
        assert_eq!(view.reason_code.as_deref(), Some("readback_failed"));
        assert_no_victory_or_cancel_claim(&view);
    }

    /// R5b：**无 `metadata.tool`** 的已答表单，按原 `session + formId` 回读官方状态与答案。
    ///
    /// 断言口径：结果来自 detail 的 `state.answer`（不是 pending 消失推断），且回读路径
    /// 不写动作账本。
    #[tokio::test]
    async fn form_result_reads_official_answer_without_tool_metadata() {
        let mut world = World::new();
        let mut entry = form_entry();
        // 表单外壳刻意不带 metadata/tool：R5b 的结果通道不得依赖旧 question 工具输出。
        entry.spec = json!({
            "id": FORM_ID,
            "sessionID": SESSION_ID,
            "title": "Questions",
            "fields": [text_field("q0", "COLOR")],
        });
        entry.status = "answered".to_string();
        entry.answer = Some(json!({
            "q0": "blue",
            "q1": ["a", "b"],
            "q2": { "nested": true },
        }));
        world.forms = vec![entry];
        let rig = boot(world).await;

        let result = rig
            .hub
            .form_result(&rig.card, FORM_ID)
            .await
            .expect("已答表单必须可回读");
        assert_eq!(result.form_id, FORM_ID);
        assert_eq!(result.session_id, SESSION_ID);
        assert_eq!(result.status, "answered");
        assert_eq!(result.answers["q0"], FormAnswerValue::Text("blue".into()));
        assert_eq!(
            result.answers["q1"],
            FormAnswerValue::Multi(vec!["a".to_string(), "b".to_string()])
        );
        assert!(
            matches!(result.answers["q2"], FormAnswerValue::Unknown(_)),
            "其它 JSON 形态不得降级成文本"
        );
        assert_eq!(ledger_rows(&rig), 0, "答案回读不得写动作账本");
    }

    /// R5b：cancelled 与 unknown 分开；两者都**不**携带官方答案。
    #[tokio::test]
    async fn form_result_cancelled_and_unknown_are_distinct_and_carry_no_answers() {
        for (status, expected) in [
            ("cancelled", "cancelled"),
            ("unexpected_status_word", "unknown"),
        ] {
            let mut world = World::new();
            let mut entry = form_entry();
            entry.status = status.to_string();
            entry.answer = Some(json!({ "q0": "blue" }));
            world.forms = vec![entry];
            let rig = boot(world).await;

            let result = rig
                .hub
                .form_result(&rig.card, FORM_ID)
                .await
                .expect("detail 可读");
            assert_eq!(result.status, expected);
            assert!(
                result.answers.is_empty(),
                "{status} 不得冒充已答结果：{:?}",
                result.answers
            );
        }
    }

    /// R5b：detail 读失败是错误（路由统一 404），不得变成 pending/answered 猜测。
    #[tokio::test]
    async fn form_result_read_failure_is_error_not_guess() {
        let mut world = World::new();
        let mut entry = form_entry();
        entry.status = "answered".to_string();
        entry.answer = Some(json!({ "q0": "blue" }));
        entry.detail_status = Some(500);
        world.forms = vec![entry];
        let rig = boot(world).await;

        let err = rig
            .hub
            .form_result(&rig.card, FORM_ID)
            .await
            .expect_err("读不到不是结果");
        assert_eq!(err.code, codes::OC_HTTP_STATUS);
        assert_eq!(ledger_rows(&rig), 0, "读失败同样零账本写入");
    }

    /// 同正文不同 id：正文相同不是证据，精确原生 id 缺席 → 永不 Confirmed。
    #[tokio::test]
    async fn same_text_without_native_id_does_not_confirm() {
        let mut world = World::new();
        world.prompt = PromptMode::IngestedWithForeignId;
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;

        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_dup", &binding, "DUPLICATE-TEXT"),
            )
            .await
            .expect("200 只是受理");
        assert_eq!(
            view.state,
            ActionState::Unconfirmed,
            "正文相同不得当成已确认"
        );
        assert_eq!(view.native_id, None, "没有精确原生 id 就没有 native_id");

        // 列表里确实有一条**正文相同**的消息：这正是「不能靠正文归因」的形态。
        let ingested = rig.peer.log();
        assert!(
            ingested.iter().any(|request| request.method == "GET"
                && request.path() == format!("/api/session/{SESSION_ID}/message")),
            "必须回读过消息列表"
        );

        // 核对之后仍然不得确认（也不得反向宣称未生效）。
        let verified = rig.hub.verify(DEVICE_ID, "act_dup").await.expect("核对");
        assert_eq!(verified.state, ActionState::Unconfirmed);
        assert_eq!(rig.peer.prompt_posts(), 1, "核对绝不重发");
    }

    /// pending 读失败 → 连发送都不得发生（最终复核拿不到当前投影，零写出）。
    #[tokio::test]
    async fn pending_read_failure_blocks_send() {
        let mut world = World::new();
        world.forms_status = Some(500);
        world.global_forms_status = Some(500);
        let rig = boot(world).await;
        claim_route(&rig);

        let err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_blocked", "bind-unset", "这条不该发出"),
            )
            .await
            .expect_err("pending 读失败不得放行任何写出");
        assert_eq!(err.code, codes::OC_HTTP_STATUS);
        assert_eq!(rig.peer.prompt_posts(), 0, "零 prompt 写出");
        assert_eq!(rig.peer.non_get(), 0, "任何写请求都不得发生");
        assert_eq!(ledger_rows(&rig), 0, "前置拒绝不得创建原生动作行");
        let rejected = rig
            .hub
            .action(DEVICE_ID, "act_blocked")
            .await
            .expect("原属主可读取持久拒绝");
        assert_eq!(rejected.action_id, "act_blocked");
        assert_eq!(rejected.state, ActionState::NotSent);
        assert_eq!(rejected.reason_code.as_deref(), Some(codes::OC_HTTP_STATUS));
        let verified = rig
            .hub
            .verify(DEVICE_ID, "act_blocked")
            .await
            .expect("原属主可核对持久拒绝");
        assert_eq!(verified, rejected);
        assert_eq!(ledger_rows(&rig), 0, "核对不得创建原生动作行");
        assert_eq!(rig.peer.non_get(), 0, "核对不得发起写请求");
    }

    /// 原生 queue 发送不产生第二个本地派发器：本地 FIFO 零落行，只有一次原生 POST。
    #[tokio::test]
    async fn send_queue_does_not_create_second_local_dispatcher() {
        let world = World::new();
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;

        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_queue", &binding, "排队发送"),
            )
            .await
            .expect("入库 + 回读命中即确认");
        assert_eq!(view.state, ActionState::Confirmed);

        assert_eq!(inject_queue_rows(&rig), 0, "原生发送不得落入本地待发 FIFO");
        assert_eq!(ledger_rows(&rig), 1, "只有一个动作实体，没有第二个派发器");
        assert_eq!(rig.peer.prompt_posts(), 1, "唯一一次原生派发");
        assert_eq!(rig.peer.non_get(), 1, "没有任何其它写请求");

        // 首批投递词只能是 queue。
        let posted = rig
            .peer
            .log()
            .into_iter()
            .find(|request| request.path() == format!("/api/session/{SESSION_ID}/prompt"))
            .expect("必须有 prompt POST");
        let body = posted.body.expect("prompt 必须有请求体");
        assert_eq!(body["delivery"], json!("queue"), "首批只产生 queue 投递");
        assert_eq!(
            body["id"],
            json!("msg_act_queue"),
            "关联 id 按 action_id 派生"
        );
    }

    /// 已投递项的 204 撤回：不是「执行被取消」，而是该项已被原生队列消费。
    #[tokio::test]
    async fn retract_consumed_item_is_not_reported_as_canceled_execution() {
        let mut world = World::new();
        // 任务 0 金样：投递后 DELETE 仍 204，但消息记录仍在（inbox 已空）。
        world.inbox = Vec::new();
        world.messages = vec![user_record(ITEM_ID, "QUEUE-THEN-DELETE")];
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;

        let view = rig
            .hub
            .execute(&actor(), &rig.card, retract_cmd("act_retract", &binding))
            .await
            .expect("204 不是撤回证据，但回读可以定性");
        assert_eq!(view.state, ActionState::Settled, "已投递 = 原请求已定案");
        assert_eq!(
            view.reason_code.as_deref(),
            Some("delivered_before_retract")
        );
        assert_no_victory_or_cancel_claim(&view);
        let deletes = rig.peer.count(
            "DELETE",
            &format!("/api/session/{SESSION_ID}/inbox/{ITEM_ID}"),
        );
        assert_eq!(deletes, 1, "只撤回一次");
    }

    /// 签名设置经既有单点读取，组装一次：精确字节 + 每动作仅一次 POST。
    #[tokio::test]
    async fn signature_setting_is_applied_once() {
        let world = World::new();
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        set_signature(&rig, true);

        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_signed", &binding, "你好"),
            )
            .await
            .expect("发送成功");
        assert_eq!(view.state, ActionState::Confirmed);
        assert_eq!(rig.peer.prompt_posts(), 1, "一次动作一次派发");

        let posted = rig
            .peer
            .log()
            .into_iter()
            .find(|request| request.path() == format!("/api/session/{SESSION_ID}/prompt"))
            .expect("必须有 prompt POST");
        let body = posted.body.expect("prompt 必须有请求体");
        assert_eq!(
            body["text"],
            json!("你好 [mobile iPhone]"),
            "签名开的组装字节必须与 compose_injection_flagged 逐字相同"
        );

        // 重复执行同一命令：不重新组装、不重新派发。
        let again = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_signed", &binding, "你好"),
            )
            .await
            .expect("重复点击走既有动作");
        assert_eq!(again.state, ActionState::Confirmed);
        assert_eq!(rig.peer.prompt_posts(), 1, "重试查询不得重新派发");

        // 关签名：同一设置单点读到 off → 裸注入。
        set_signature(&rig, false);
        let view = rig
            .hub
            .execute(&actor(), &rig.card, send_cmd("act_bare", &binding, "你好"))
            .await
            .expect("发送成功");
        assert_eq!(view.state, ActionState::Confirmed);
        assert_eq!(rig.peer.prompt_posts(), 2, "第二个动作才发生第二次派发");
        let posted = rig
            .peer
            .log()
            .into_iter()
            .filter(|request| request.path() == format!("/api/session/{SESSION_ID}/prompt"))
            .nth(1)
            .expect("第二次 prompt POST");
        assert_eq!(
            posted.body.expect("prompt 必须有请求体")["text"],
            json!("你好"),
            "签名关闭 = 裸注入"
        );
    }

    /// 斜杠命令零 prompt 写出：绝不把终端命令当正文发给模型。
    #[tokio::test]
    async fn slash_command_never_becomes_plain_prompt() {
        let world = World::new();
        let rig = boot(world).await;
        claim_route(&rig);

        for text in ["/plan 打开权限", "\u{1b}/permissions"] {
            let err = rig
                .hub
                .execute(
                    &actor(),
                    &rig.card,
                    send_cmd("act_slash", "bind-unset", text),
                )
                .await
                .expect_err("斜杠命令必须被拒");
            assert_eq!(err.code, codes::OC_COMMAND_UNSUPPORTED, "text={text:?}");
            assert!(!err.message.contains("/plan"), "错误文案不得回显正文");
        }
        assert_eq!(rig.peer.prompt_posts(), 0, "零 prompt 写出");
        assert_eq!(rig.peer.total(), 0, "静态复核即拒绝，连服务都不必打扰");
    }

    /// 受控目录之外的附件一律拒绝（零写出）。
    #[tokio::test]
    async fn attachment_outside_controlled_dir_is_refused() {
        let world = World::new();
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;

        // 项目目录里的普通文件：不在 <cwd>/.tuvis-attachments/<sid>/ 内。
        let stray = rig.project.join("stray.txt");
        std::fs::write(&stray, "x").unwrap();
        let err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd_with_attachments(
                    "act_stray",
                    &binding,
                    "看这个",
                    vec![stray.to_string_lossy().into_owned()],
                ),
            )
            .await
            .expect_err("受控目录之外的附件必须拒绝");
        assert_eq!(err.code, codes::OC_ATTACHMENT_REFUSED);

        // 目录内的符号链接逃逸：canonicalize 后同样在受控目录之外。
        let controlled = rig.project.join(".tuvis-attachments").join(SESSION_ID);
        std::fs::create_dir_all(&controlled).unwrap();
        let escape = controlled.join("escape.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&stray, &escape).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&stray, &escape).unwrap();
        let err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd_with_attachments(
                    "act_escape",
                    &binding,
                    "看这个",
                    vec![escape.to_string_lossy().into_owned()],
                ),
            )
            .await
            .expect_err("符号链接逃逸必须拒绝");
        assert_eq!(err.code, codes::OC_ATTACHMENT_REFUSED);

        assert_eq!(rig.peer.prompt_posts(), 0, "零 prompt 写出");
        assert_eq!(rig.peer.non_get(), 0, "任何写请求都不得发生");
    }

    /// 受控目录内的附件：以 file:// 绝对 URI 提交。
    #[tokio::test]
    async fn accepted_attachment_uses_file_uri() {
        let world = World::new();
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;

        let controlled = rig.project.join(".tuvis-attachments").join(SESSION_ID);
        std::fs::create_dir_all(&controlled).unwrap();
        let note = controlled.join("note.txt");
        std::fs::write(&note, "tuvis attachment probe\n").unwrap();
        let canonical = note.canonicalize().unwrap();

        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd_with_attachments(
                    "act_attach",
                    &binding,
                    "看这个附件",
                    vec![note.to_string_lossy().into_owned()],
                ),
            )
            .await
            .expect("受控目录内的附件可提交");
        assert_eq!(view.state, ActionState::Confirmed);

        let posted = rig
            .peer
            .log()
            .into_iter()
            .find(|request| request.path() == format!("/api/session/{SESSION_ID}/prompt"))
            .expect("必须有 prompt POST");
        let body = posted.body.expect("prompt 必须有请求体");
        let uri = reqwest::Url::parse(body["files"][0]["uri"].as_str().unwrap()).unwrap();
        assert_eq!(uri.scheme(), "file", "附件仍为 file:// 绝对 URI");
        let frozen = uri.to_file_path().unwrap();
        assert!(frozen.starts_with(
            attachment_dir(&rig.project, SESSION_ID)
                .canonicalize()
                .unwrap()
        ));
        assert_eq!(
            std::fs::read(frozen).unwrap(),
            std::fs::read(canonical).unwrap(),
            "发送冻结确认帧的内容"
        );
        assert_eq!(body["files"][0]["name"], json!("note.txt"));
        assert_eq!(body["text"], json!("看这个附件"), "正文与附件同发");
    }

    /// 原生插队 = PATCH 既有 inbox 项把 delivery 改成 steer：
    /// 只发一次 PATCH、**不发 prompt**（不重发正文），且回读判定为已受理。
    #[tokio::test]
    async fn promote_inbox_patches_delivery_without_resending_text() {
        let mut world = World::new();
        let item_id = "msg_queued_promote";
        world.inbox = vec![inbox_item(item_id, "排队的那条")];
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let command = NativeCommand {
            action_id: "act_promote_1".into(),
            session_id: SESSION_ID.into(),
            binding_token: binding,
            operation: NativeOperation::PromoteInbox(RetractPayload {
                item_id: item_id.into(),
            }),
        };
        let view = rig
            .hub
            .execute(&actor(), &rig.card, command)
            .await
            .expect("改投递必须放行");

        let log = rig.peer.log();
        let patches: Vec<_> = log
            .iter()
            .filter(|r| {
                r.method == "PATCH"
                    && r.path() == format!("/api/session/{SESSION_ID}/inbox/{item_id}")
            })
            .collect();
        assert_eq!(patches.len(), 1, "改投递恰好一次 PATCH");
        assert_eq!(
            patches[0].body.clone().expect("patch body")["delivery"],
            json!("steer")
        );
        // 关键：插队**不重发正文**——整轮没有 prompt POST。
        assert!(
            !log.iter()
                .any(|r| r.path() == format!("/api/session/{SESSION_ID}/prompt")),
            "改投递不得再发一次 prompt"
        );
        // 回读判定：仍在 inbox 且已 steer ⇒ 已受理
        assert_eq!(view.state, ActionState::Acknowledged);
        assert_eq!(
            view.reason_code.as_deref(),
            Some(reason_words::STEERED_INTERRUPT)
        );
    }

    /// steer 是直接投递：不进本地 FIFO，body.delivery 必须是 steer，且只写一次。
    #[tokio::test]
    async fn steer_delivery_is_direct_and_confirmed() {
        let world = World::new();
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let mut command = send_cmd("act_steer_direct", &binding, "补充一句");
        if let NativeOperation::Send(payload) = &mut command.operation {
            payload.delivery = DeliveryKind::Steer;
        }
        let view = rig
            .hub
            .execute(&actor(), &rig.card, command)
            .await
            .expect("steer 必须放行");
        assert_eq!(view.state, ActionState::Confirmed);
        let posted = rig
            .peer
            .log()
            .into_iter()
            .find(|request| request.path() == format!("/api/session/{SESSION_ID}/prompt"))
            .expect("必须有 prompt POST");
        assert_eq!(
            posted.body.expect("prompt body")["delivery"],
            json!("steer")
        );
        assert_eq!(rig.peer.prompt_posts(), 1, "steer 只允许一次派发");
    }

    /// 纯附件消息：正文为空但受控附件非空时必须放行，且 files 真实提交。
    #[tokio::test]
    async fn attachment_only_message_is_sent_without_body() {
        let world = World::new();
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let note = controlled_attachment(&rig.project, "only.txt", 8);
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd_with_attachments("act_attach_only", &binding, "   ", vec![note]),
            )
            .await
            .expect("纯附件消息必须允许");
        assert_eq!(view.state, ActionState::Confirmed);
        let posted = rig
            .peer
            .log()
            .into_iter()
            .find(|request| request.path() == format!("/api/session/{SESSION_ID}/prompt"))
            .expect("必须有 prompt POST");
        let body = posted.body.expect("prompt body");
        assert_eq!(body["files"].as_array().unwrap().len(), 1);
        assert!(
            body["text"]
                .as_str()
                .is_some_and(|text| text.trim().is_empty()),
            "纯附件正文应为空（或仅签名），实际 {:?}",
            body["text"]
        );

        // 空正文且无附件仍然拒绝。
        let err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_empty_no_file", &binding, "   "),
            )
            .await
            .expect_err("空正文无附件必须拒绝");
        assert_eq!(err.code, codes::OC_SEND_EMPTY);
    }

    /// R4：单条消息**原始**附件字节合计 ≤ 20 MiB，且单个文件 ≤ 5 MiB（原生通道
    /// 单文件预算）。四个 5 MiB 整文件 = 20 MiB 必须放行；四个 5 MiB + 1 字节合计
    /// 超限必须被拒——每个文件都在单文件预算内，只有「原始字节合计」能拒绝它。
    /// 两次判定都在 claim/写之前，拒绝时零原生写。
    #[tokio::test]
    async fn attachment_aggregate_over_budget_is_refused_before_write() {
        let world = World::new();
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;

        // 4 × 5 MiB = 20 MiB 整：边界不误伤合法大附件。
        let a = controlled_attachment(&rig.project, "a.bin", 5 * 1024 * 1024);
        let b = controlled_attachment(&rig.project, "b.bin", 5 * 1024 * 1024);
        let c = controlled_attachment(&rig.project, "c.bin", 5 * 1024 * 1024);
        let d = controlled_attachment(&rig.project, "d.bin", 5 * 1024 * 1024);
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd_with_attachments(
                    "act_at_budget",
                    &binding,
                    "看大附件",
                    vec![a.clone(), b.clone(), c.clone(), d.clone()],
                ),
            )
            .await
            .expect("20 MiB 整包是合法边界");
        assert_eq!(view.state, ActionState::Confirmed);
        assert_eq!(rig.peer.prompt_posts(), 1, "边界内附件必须真实派出一次");

        // 4 × 5 MiB + 1 字节：每个文件都 ≤ 5 MiB，只有「原始字节合计」能拒绝它。
        let tiny = controlled_attachment(&rig.project, "tiny.bin", 1);
        let err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd_with_attachments(
                    "act_aggregate_over",
                    &binding,
                    "看大附件",
                    vec![a, b, c, d, tiny],
                ),
            )
            .await
            .expect_err("每个文件都合法、合计超过 20 MiB 必须拒绝");
        assert_eq!(err.code, codes::OC_ATTACHMENT_REFUSED);
        assert_eq!(
            rig.peer.prompt_posts(),
            1,
            "合计超限拒绝必须零写入（不得把每个文件都合法的附件放过去）"
        );
        assert_eq!(rig.peer.non_get(), 1, "合计超限拒绝不得产生任何原生写请求");
    }

    /// 原生通道单文件预算 5 MiB：5 MiB 整放行并真实派发一次；5 MiB + 1 字节在
    /// claim/写之前拒绝（零原生写），动作落 `not_sent` 墓碑而不是 `unconfirmed`。
    #[tokio::test]
    async fn attachment_single_file_limit() {
        let world = World::new();
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;

        let exact = controlled_attachment(&rig.project, "exact.bin", 5 * 1024 * 1024);
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd_with_attachments("act_exact_5mib", &binding, "看附件", vec![exact]),
            )
            .await
            .expect("5 MiB 整是合法边界");
        assert_eq!(view.state, ActionState::Confirmed);
        assert_eq!(rig.peer.prompt_posts(), 1, "边界内附件必须真实派出一次");
        let posted = rig
            .peer
            .log()
            .into_iter()
            .find(|request| request.path() == format!("/api/session/{SESSION_ID}/prompt"))
            .expect("必须有 prompt POST");
        assert_eq!(
            posted.body.expect("prompt 必须有请求体")["files"]
                .as_array()
                .map(Vec::len),
            Some(1),
            "5 MiB 附件必须原样提交"
        );

        let over = controlled_attachment(&rig.project, "over.bin", 5 * 1024 * 1024 + 1);
        let err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd_with_attachments("act_over_5mib", &binding, "看附件", vec![over]),
            )
            .await
            .expect_err("单文件超过 5 MiB 必须拒绝");
        assert_eq!(err.code, codes::OC_ATTACHMENT_REFUSED);
        assert!(
            err.message.contains("per-file budget") && err.message.contains("5 MiB"),
            "单文件超限必须给出可区分的原因，实际：{}",
            err.message
        );
        assert_eq!(rig.peer.prompt_posts(), 1, "单文件超限拒绝必须零写入");
        assert_eq!(
            rig.peer.non_get(),
            1,
            "单文件超限拒绝不得产生任何原生写请求"
        );

        let tombstone = rig
            .hub
            .action_store()
            .read(DEVICE_ID, "act_over_5mib")
            .expect("拒绝后动作必须仍可在本机账本读取");
        assert_eq!(
            tombstone.state,
            ActionState::NotSent,
            "派发前拒绝必须落墓碑，不得是 unconfirmed"
        );
        assert_eq!(
            tombstone.reason_code.as_deref(),
            Some(abandon_reasons::ATTACHMENT_REFUSED)
        );
        assert_eq!(tombstone.native_id, None);
    }

    /// R4：单条消息附件文件数 ≤ 10；10 个放行，11 个在写之前拒绝（零写入）。
    #[tokio::test]
    async fn attachment_file_count_limit() {
        let world = World::new();
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;

        let eleven: Vec<String> = (0..11)
            .map(|index| controlled_attachment(&rig.project, &format!("f{index}.bin"), 1))
            .collect();
        let err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd_with_attachments("act_eleven", &binding, "看附件", eleven),
            )
            .await
            .expect_err("11 个附件必须拒绝");
        assert_eq!(err.code, codes::OC_ATTACHMENT_REFUSED);
        assert_eq!(rig.peer.prompt_posts(), 0, "超限拒绝必须零写入");
        assert_eq!(rig.peer.non_get(), 0, "超限拒绝不得产生任何原生写请求");

        let ten: Vec<String> = (0..10)
            .map(|index| controlled_attachment(&rig.project, &format!("ok{index}.bin"), 1))
            .collect();
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd_with_attachments("act_ten", &binding, "看附件", ten),
            )
            .await
            .expect("10 个附件是合法边界");
        assert_eq!(view.state, ActionState::Confirmed);
        assert_eq!(rig.peer.prompt_posts(), 1);
        let posted = rig
            .peer
            .log()
            .into_iter()
            .find(|request| request.path() == format!("/api/session/{SESSION_ID}/prompt"))
            .expect("必须有 prompt POST");
        let body = posted.body.expect("prompt 必须有请求体");
        assert_eq!(
            body["files"].as_array().map(Vec::len),
            Some(10),
            "10 个附件必须原样提交"
        );
    }

    #[test]
    fn native_attachment_uri_roundtrip_uses_platform_paths() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("中文 space # %.txt");
        std::fs::write(&path, b"same file").unwrap();
        let canonical = path.canonicalize().unwrap();
        let uri = file_uri_of(&canonical).unwrap();
        let decoded = reqwest::Url::parse(&uri).unwrap().to_file_path().unwrap();
        assert_eq!(decoded.canonicalize().unwrap(), canonical);
        assert!(file_uri_of(Path::new("relative.txt")).is_err());
        let command = send_cmd_with_attachments(
            "act_path",
            "binding",
            "附件",
            vec![canonical.to_string_lossy().into_owned()],
        );
        assert!(intent_of(&command).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_unc_and_verbatim_paths_use_standard_file_uris() {
        for (path, expected) in [
            (
                r"C:\project\中文 #.txt",
                "file:///C:/project/%E4%B8%AD%E6%96%87%20%23.txt",
            ),
            (r"\\server\share\a b.txt", "file://server/share/a%20b.txt"),
            (r"\\?\C:\project\a.txt", "file:///C:/project/a.txt"),
            (r"\\?\UNC\server\share\a.txt", "file://server/share/a.txt"),
        ] {
            assert!(intent_of(&send_cmd_with_attachments(
                "act_path",
                "binding",
                "附件",
                vec![path.to_string()]
            ))
            .is_ok());
            let uri = file_uri_of(Path::new(path)).unwrap();
            assert_eq!(uri, expected);
            let decoded = reqwest::Url::parse(&uri).unwrap().to_file_path().unwrap();
            assert_eq!(file_uri_of(&decoded).unwrap(), expected);
        }
    }

    /// `file://` URI 必须逐段百分号编码（评审 Blocker 3）：空格 / `#` / `?` / `%` /
    /// 中文都不能原样出现在 URI 里，且解码后必须**逐字回到磁盘路径**。
    #[cfg(unix)]
    #[test]
    fn file_uri_percent_encodes_special_characters() {
        let cases = [
            "/tmp/plain/file.txt",
            "/tmp/with space/file.txt",
            "/tmp/with#hash/file.txt",
            "/tmp/with?query/file.txt",
            "/tmp/100%/file.txt",
            "/tmp/中文目录/文件名 带#号?与%号.txt",
        ];
        for raw in cases {
            let uri = file_uri_of(Path::new(raw)).unwrap();
            assert!(
                uri.starts_with("file:///"),
                "必须是 file:// + 绝对路径：{uri}"
            );
            let body = &uri["file://".len()..];
            assert!(
                !body.contains([' ', '#', '?']),
                "空格/#/? 必须被编码（{raw} → {uri}）"
            );
            // 每个 '%' 后面必须紧跟两位十六进制（不许有裸 '%'）
            let bytes = body.as_bytes();
            for (i, byte) in bytes.iter().enumerate() {
                if *byte == b'%' {
                    let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).expect("十六进制");
                    assert!(
                        u8::from_str_radix(hex, 16).is_ok(),
                        "非法的百分号转义（{raw} → {uri}）"
                    );
                }
            }
            assert_eq!(
                decode_uri_path(&uri),
                raw,
                "解码必须逐字回到磁盘路径（{raw} → {uri}）"
            );
        }
    }

    /// 端到端：受控目录里带空格/#/?/中文的附件仍通过校验，提交出去的 URI 解码后是
    /// **磁盘上的同一个文件**（受控目录检查与编码互不干扰）。
    #[test]
    fn attachment_uri_round_trips_for_special_names() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path().join("project");
        let controlled = project.join(".tuvis-attachments").join(SESSION_ID);
        std::fs::create_dir_all(&controlled).expect("建受控目录");
        let name = if cfg!(windows) {
            "中文 报告#1%.txt"
        } else {
            "中文 报告#1?.txt"
        };
        let file = controlled.join(name);
        std::fs::write(&file, b"probe").expect("写附件");
        let canonical = file.canonicalize().expect("归一附件路径");

        let target = bound_target(&project);
        let files = verified_attachments(&target, &[file.to_string_lossy().into_owned()])
            .expect("受控目录内的特殊字符文件名可提交");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, name);
        let body = &files[0].uri["file://".len()..];
        assert!(
            !body.contains([' ', '#', '?']),
            "URI 不得含裸特殊字符：{}",
            files[0].uri
        );
        assert_eq!(
            reqwest::Url::parse(&files[0].uri)
                .unwrap()
                .to_file_path()
                .unwrap()
                .canonicalize()
                .unwrap(),
            canonical,
            "解码后必须是同一个磁盘文件"
        );
    }

    /// 测试侧解码器（只实现本模块产出的编码子集：`%XX` + 其余原样）。
    #[cfg(unix)]
    fn decode_uri_path(uri: &str) -> String {
        let body = uri.strip_prefix("file://").expect("file:// 前缀");
        let bytes = body.as_bytes();
        let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).expect("十六进制");
                out.push(u8::from_str_radix(hex, 16).expect("合法字节"));
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).expect("utf8 路径")
    }

    /// 审批（once/reject）：204 + pending 消失 ⇒ 生效；400 ⇒ 仍记 Unconfirmed
    /// （权限 400 没有「整份未生效」的金样证明）；409 + 已不在 pending ⇒ Settled。
    #[tokio::test]
    async fn permission_decision_confirms_only_after_pending_disappears() {
        // 相位一：once 放行，回读证明 pending 消失。
        let mut world = World::new();
        world.permissions = vec![permission_entry()];
        let rig = boot(world).await;
        claim_route(&rig);
        let first = rig.hub.view(&rig.card).await.expect("首轮投影");
        let binding = first.binding_token.clone();
        let token = permission_token_of(&first);

        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                decide_cmd("act_once", &binding, &token, "once"),
            )
            .await
            .expect("204 + pending 消失 = 生效");
        assert_eq!(view.state, ActionState::Confirmed);
        assert_eq!(view.native_id.as_deref(), Some(PERMISSION_ID));
        assert_eq!(
            rig.peer.count(
                "POST",
                &format!("/api/session/{SESSION_ID}/permission/{PERMISSION_ID}/reply")
            ),
            1,
            "只提交一次"
        );

        // 相位二：400 ⇒ 没有金样证明 ⇒ Unconfirmed（占用不放、不重发）。
        let mut world = World::new();
        let mut permission = permission_entry();
        permission.reply = Reply::BadRequest;
        world.permissions = vec![permission];
        let rig = boot(world).await;
        claim_route(&rig);
        let first = rig.hub.view(&rig.card).await.expect("首轮投影");
        let binding = first.binding_token.clone();
        let token = permission_token_of(&first);

        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                decide_cmd("act_bad", &binding, &token, "reject"),
            )
            .await
            .expect("400 不是错误，是未知结果");
        assert_eq!(view.state, ActionState::Unconfirmed);
        assert_eq!(view.reason_code.as_deref(), Some("write_refused"));

        // 相位三：409 + 已不在 pending ⇒ Settled（只说已定案）。
        let mut world = World::new();
        let mut permission = permission_entry();
        permission.reply = Reply::SettledThenConflict;
        world.permissions = vec![permission];
        let rig = boot(world).await;
        claim_route(&rig);
        let first = rig.hub.view(&rig.card).await.expect("首轮投影");
        let binding = first.binding_token.clone();
        let token = permission_token_of(&first);

        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                decide_cmd("act_late", &binding, &token, "reject"),
            )
            .await
            .expect("409 + 定案 = Settled");
        assert_eq!(view.state, ActionState::Settled);
        assert_no_victory_or_cancel_claim(&view);
    }

    /// R3：写后回读里同一 requestId 的归属不可证（`sessionID` 缺失 / 非字符串）⇒ 该缺席
    /// 不是「请求已消失」的证据：不得 Confirmed、核对不得 Settled，未决屏障继续挡住普通
    /// 交回、绝不重发；不可证项也不得被补成目标会话的待处理审批。
    /// 对照：只剩**已知其他会话**的项时归属仍可证，缺席照旧成立。
    #[tokio::test]
    async fn unowned_pending_cannot_settle_decision() {
        for reply in [Reply::AppliedUnownedMissing, Reply::AppliedUnownedNonString] {
            let mut world = World::new();
            let mut permission = permission_entry();
            permission.reply = reply;
            world.permissions = vec![permission];
            let rig = boot(world).await;
            claim_route(&rig);
            let first = rig.hub.view(&rig.card).await.expect("首轮投影");
            let (binding, token) = (first.binding_token.clone(), permission_token_of(&first));

            let view = rig
                .hub
                .execute(
                    &actor(),
                    &rig.card,
                    decide_cmd("act_unowned", &binding, &token, "once"),
                )
                .await
                .expect("204 是受理，不是错误");
            assert_eq!(
                view.state,
                ActionState::Unconfirmed,
                "归属不可证的缺席不得记放行生效"
            );
            assert_eq!(view.native_id, None, "没有可信证据就不得写原生 id");
            assert_eq!(
                view.reason_code.as_deref(),
                Some(reason_words::READBACK_FAILED)
            );

            let verified = rig
                .hub
                .verify(DEVICE_ID, "act_unowned")
                .await
                .expect("核对不得失败");
            assert_eq!(
                verified.state,
                ActionState::Unconfirmed,
                "归属不可证的缺席不得记已定案"
            );
            assert_eq!(verified.native_id, None);
            assert_eq!(
                verified.reason_code.as_deref(),
                Some(reason_words::READBACK_FAILED)
            );

            // 未决屏障保留：普通交回仍被拒绝、路由仍由本端持有、绝不重发。
            assert!(
                !rig.hub.disengage_route(SESSION_ID).await.expect("交回判定"),
                "未决动作在场时普通交回必须失败"
            );
            assert_eq!(
                rig.hub
                    .action_store()
                    .route_state(SESSION_ID)
                    .expect("路由状态")
                    .as_deref(),
                Some("active"),
                "交回失败不得改动路由"
            );
            assert_eq!(
                rig.peer.count(
                    "POST",
                    &format!("/api/session/{SESSION_ID}/permission/{PERMISSION_ID}/reply")
                ),
                1,
                "核对绝不重发"
            );

            // 展示面：不可证项也不得被补成目标会话的待处理项。
            let visible = rig.hub.view(&rig.card).await.expect("回读投影");
            assert!(
                visible
                    .pending
                    .iter()
                    .all(|item| !matches!(item, PendingView::Permission { .. })),
                "归属不可证的审批不得展示为目标会话的待处理项"
            );
        }

        // 对照：只剩已知其他会话的项 ⇒ 归属可证（不是目标会话），缺席仍可定案。
        let mut world = World::new();
        let mut permission = permission_entry();
        permission.reply = Reply::AppliedOtherSession;
        world.permissions = vec![permission];
        let rig = boot(world).await;
        claim_route(&rig);
        let first = rig.hub.view(&rig.card).await.expect("首轮投影");
        let (binding, token) = (first.binding_token.clone(), permission_token_of(&first));
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                decide_cmd("act_other_session", &binding, &token, "once"),
            )
            .await
            .expect("204 + 可证缺席 = 生效");
        assert_eq!(
            view.state,
            ActionState::Confirmed,
            "已知其他会话的项不得污染负面证据"
        );
        assert_eq!(view.native_id.as_deref(), Some(PERMISSION_ID));
    }

    /// R3：表单 detail 只认已支持终态 `answered` / `cancelled`；`pending` 与任何未知状态词
    /// 都不能定案（2xx 不得 Confirmed、核对不得 Settled）。`cancelled` 是终态，但不得冒充
    /// 本端作答成功（只算已定案）。
    #[tokio::test]
    async fn unknown_form_state_cannot_confirm_or_settle() {
        // 相位一：reply 200 + 未知状态 processing ⇒ 不得 Confirmed；核对仍是 Unknown。
        let mut world = World::new();
        let mut form = form_entry();
        form.reply = Reply::AppliedWithStatus("processing");
        world.forms = vec![form];
        let rig = boot(world).await;
        claim_route(&rig);
        let first = rig.hub.view(&rig.card).await.expect("首轮投影");
        let (binding, token) = binding_and_form_token(&first);

        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                answer_cmd("act_unknown", &binding, &token, q0_blue()),
            )
            .await
            .expect("200 + 未知状态 = 未知结果");
        assert_eq!(view.state, ActionState::Unconfirmed, "未知状态词不得假定案");
        assert_eq!(
            view.reason_code.as_deref(),
            Some(reason_words::READBACK_FAILED)
        );
        assert_eq!(rig.peer.form_reply_posts(), 1, "只提交一次");

        let verified = rig
            .hub
            .verify(DEVICE_ID, "act_unknown")
            .await
            .expect("核对不得失败");
        assert_eq!(
            verified.state,
            ActionState::Unconfirmed,
            "未知状态不得落 Settled"
        );
        assert_eq!(verified.native_id, None);

        // 正向：拿回已支持的 answered ⇒ 定案（不声称哪端获胜）。
        rig.peer
            .set(|world| world.forms[0].status = "answered".into());
        let settled = rig
            .hub
            .verify(DEVICE_ID, "act_unknown")
            .await
            .expect("核对不得失败");
        assert_eq!(settled.state, ActionState::Settled);
        assert_no_victory_or_cancel_claim(&settled);
        assert_eq!(rig.peer.form_reply_posts(), 1, "核对绝不重发");

        // 相位二：核对拿到 cancelled ⇒ 仍是已支持终态，但不是本端作答成功。
        let mut world = World::new();
        let mut form = form_entry();
        form.reply = Reply::Conflict;
        world.forms = vec![form];
        let rig = boot(world).await;
        claim_route(&rig);
        let first = rig.hub.view(&rig.card).await.expect("首轮投影");
        let (binding, token) = binding_and_form_token(&first);
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                answer_cmd("act_skip_late", &binding, &token, q0_blue()),
            )
            .await
            .expect("409 + pending = 未知结果");
        assert_eq!(view.state, ActionState::Unconfirmed);
        rig.peer
            .set(|world| world.forms[0].status = "cancelled".into());
        let settled = rig
            .hub
            .verify(DEVICE_ID, "act_skip_late")
            .await
            .expect("核对不得失败");
        assert_eq!(
            settled.state,
            ActionState::Settled,
            "cancelled 是已支持终态"
        );
        assert_no_victory_or_cancel_claim(&settled);
        assert_eq!(rig.peer.form_reply_posts(), 1, "核对绝不重发");

        // 相位三：reply 200 + cancelled ⇒ 已定案，但不是本端作答生效（不得 Confirmed）。
        let mut world = World::new();
        let mut form = form_entry();
        form.reply = Reply::AppliedWithStatus("cancelled");
        world.forms = vec![form];
        let rig = boot(world).await;
        claim_route(&rig);
        let first = rig.hub.view(&rig.card).await.expect("首轮投影");
        let (binding, token) = binding_and_form_token(&first);
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                answer_cmd("act_skip_ok", &binding, &token, q0_blue()),
            )
            .await
            .expect("200 是受理，不是错误");
        assert_eq!(
            view.state,
            ActionState::Settled,
            "cancelled 不得冒充本端作答成功"
        );
        assert_eq!(view.native_id, None, "Settled 不携带「本端写入的 id」");
        assert_no_victory_or_cancel_claim(&view);
    }

    /// R3（建议项）：丢回执但回读只在 inbox ⇒ `Unconfirmed` 凭原生 id 收敛为
    /// `Acknowledged`；不是 Confirmed、不释放未决、绝不重发。
    #[tokio::test]
    async fn lost_send_reply_found_only_in_inbox_becomes_acknowledged() {
        let mut world = World::new();
        world.prompt = PromptMode::QueuedThenDropped;
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;

        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_queued", &binding, "排队消息"),
            )
            .await
            .expect("丢响应不是错误，而是未知结果");
        assert_eq!(view.state, ActionState::Unconfirmed, "丢响应只能记未知");
        assert_eq!(rig.peer.prompt_posts(), 1, "只派发一次");

        let verified = rig
            .hub
            .verify(DEVICE_ID, "act_queued")
            .await
            .expect("核对不得失败");
        assert_eq!(
            verified.state,
            ActionState::Acknowledged,
            "只出现在 inbox = 已入队（不是已投递）"
        );
        assert_eq!(verified.native_id.as_deref(), Some("msg_act_queued"));
        assert_eq!(rig.peer.prompt_posts(), 1, "核对绝不重发");

        // 不释放未决：普通交回继续被拒绝。
        assert!(
            !rig.hub.disengage_route(SESSION_ID).await.expect("交回判定"),
            "Acknowledged 不是可交回的证据"
        );

        // 再次核对：不重复落账、不改写、不重发。
        let again = rig
            .hub
            .verify(DEVICE_ID, "act_queued")
            .await
            .expect("核对不得失败");
        assert_eq!(again.state, ActionState::Acknowledged);
        assert_eq!(rig.peer.prompt_posts(), 1);
    }

    /// 撤回被拒（DELETE 非 2xx）且项仍在队列 ⇒ NotAccepted（可证明未生效），绝不报成功。
    #[tokio::test]
    async fn refused_retract_keeps_the_item_queued() {
        let mut world = World::new();
        world.inbox = vec![inbox_item(ITEM_ID, "排队中")];
        world.delete_status = 500;
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;

        let view = rig
            .hub
            .execute(&actor(), &rig.card, retract_cmd("act_refused", &binding))
            .await
            .expect("撤回被拒是已知结果");
        assert_eq!(view.state, ActionState::NotAccepted);
        assert_eq!(
            view.reason_code.as_deref(),
            Some(NOT_ACCEPTED_EVIDENCE),
            "项仍在队列 = 撤回未生效（可证明）"
        );
        assert_eq!(
            rig.peer.count(
                "DELETE",
                &format!("/api/session/{SESSION_ID}/inbox/{ITEM_ID}")
            ),
            1,
            "只撤回一次"
        );
    }

    /// prompt 被服务端拒绝（409/400）：只记 Unconfirmed，绝不自动重发。
    #[tokio::test]
    async fn refused_prompt_stays_unconfirmed() {
        let mut world = World::new();
        world.prompt = PromptMode::Conflict;
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;

        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_conflict", &binding, "竞态发送"),
            )
            .await
            .expect("409 不是错误");
        assert_eq!(view.state, ActionState::Unconfirmed);
        assert_eq!(view.reason_code.as_deref(), Some("write_refused"));
        assert_eq!(rig.peer.prompt_posts(), 1);

        rig.peer.set(|world| world.prompt = PromptMode::BadRequest);
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_bad_request", &binding, "坏请求"),
            )
            .await
            .expect("400 不是错误");
        assert_eq!(view.state, ActionState::Unconfirmed);
        assert_eq!(view.reason_code.as_deref(), Some("write_refused"));
        assert_eq!(rig.peer.prompt_posts(), 2, "每个动作各一次，绝不重试");
    }

    /// I-1：写之后换代际（同端口服务重启），新代际的空 pending / 空 inbox / 空消息列表
    /// **不是**旧写入生效的证据——四种操作一律只记 `Unconfirmed`，绝不 Confirmed/Settled。
    #[tokio::test]
    async fn read_back_from_a_new_generation_is_not_evidence() {
        // decide：204 之后重启 ⇒ 新代际 pending 为空 ⇒ 不得记「放行生效」。
        let mut world = World::new();
        world.permissions = vec![permission_entry()];
        world.switch_generation_on_write = true;
        let rig = boot(world).await;
        claim_route(&rig);
        let first = rig.hub.view(&rig.card).await.expect("首轮投影");
        let (binding, token) = (first.binding_token.clone(), permission_token_of(&first));
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                decide_cmd("act_gen_decide", &binding, &token, "once"),
            )
            .await
            .expect("204 是受理，不是错误");
        assert_eq!(
            view.state,
            ActionState::Unconfirmed,
            "换代际后的空 pending 不得当成放行生效"
        );
        assert_eq!(view.reason_code.as_deref(), Some("readback_failed"));
        assert_eq!(
            rig.peer.count(
                "POST",
                &format!("/api/session/{SESSION_ID}/permission/{PERMISSION_ID}/reply")
            ),
            1,
            "换代际不改变「一次派发」"
        );

        // retract：DELETE 之后重启 ⇒ 新代际 inbox/消息都空 ⇒ 不得记「撤回生效」。
        let mut world = World::new();
        world.inbox = vec![inbox_item(ITEM_ID, "排队中")];
        world.switch_generation_on_write = true;
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                retract_cmd("act_gen_retract", &binding),
            )
            .await
            .expect("204 是受理，不是错误");
        assert_eq!(
            view.state,
            ActionState::Unconfirmed,
            "换代际后的缺席不得当成撤回生效（否定证据只在写代际成立）"
        );

        // answer：204 之后重启 ⇒ 新代际的 detail 状态不是「作答生效」的证据。
        let mut world = World::new();
        world.forms = vec![form_entry()];
        world.switch_generation_on_write = true;
        let rig = boot(world).await;
        claim_route(&rig);
        let first = rig.hub.view(&rig.card).await.expect("首轮投影");
        let (binding, token) = binding_and_form_token(&first);
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                answer_cmd("act_gen_answer", &binding, &token, q0_blue()),
            )
            .await
            .expect("204 是受理，不是错误");
        assert_eq!(
            view.state,
            ActionState::Unconfirmed,
            "换代际后的 detail 状态不得当成作答生效"
        );

        // send：入库之后重启 ⇒ 新代际的消息列表不是「已投递」的证据。
        let mut world = World::new();
        world.switch_generation_on_write = true;
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_gen_send", &binding, "换代际"),
            )
            .await
            .expect("200 是受理，不是错误");
        assert_eq!(
            view.state,
            ActionState::Unconfirmed,
            "换代际后的消息列表不得当成已投递"
        );
        assert_eq!(rig.peer.prompt_posts(), 1, "每个动作仍只派发一次");
    }

    /// 停止/模式丢回执后的只读核对：能观察当前值，但没有本次写回执时保持 Unknown，
    /// 不把“当前恰好空闲/等于目标”归因给本动作，核对绝不重发。
    #[tokio::test]
    async fn verify_recovers_stop_and_set_agent_without_resend() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let store = rig.hub.action_store();

        let stop = stop_cmd("act_stop_verify", &binding);
        assert!(matches!(
            store
                .reserve(DEVICE_ID, &bound_target(&rig.project), &stop)
                .expect("reserve stop"),
            ReserveOutcome::New
        ));
        assert!(store.claim("act_stop_verify").expect("claim stop"));
        rig.peer.set(|world| {
            world.busy = Some(100);
            world.idle = None;
        });
        let busy = rig.hub.verify(DEVICE_ID, "act_stop_verify").await.unwrap();
        assert_eq!(
            busy.state,
            ActionState::Unconfirmed,
            "仍在执行只能是 Unknown"
        );
        rig.peer.set(|world| {
            world.busy = None;
            world.idle = Some(200);
        });
        let idle = rig.hub.verify(DEVICE_ID, "act_stop_verify").await.unwrap();
        assert_eq!(idle.state, ActionState::Unconfirmed);
        assert_eq!(idle.reason_code.as_deref(), Some("result_unproven"));

        let set = set_agent_cmd("act_agent_verify", &binding, "plan");
        assert!(matches!(
            store
                .reserve(DEVICE_ID, &bound_target(&rig.project), &set)
                .expect("reserve agent"),
            ReserveOutcome::New
        ));
        assert!(store.claim("act_agent_verify").expect("claim agent"));
        rig.peer.set(|world| world.agent = "build".to_string());
        let unchanged = rig.hub.verify(DEVICE_ID, "act_agent_verify").await.unwrap();
        assert_eq!(unchanged.state, ActionState::Unconfirmed);
        rig.peer.set(|world| world.agent = "plan".to_string());
        let switched = rig.hub.verify(DEVICE_ID, "act_agent_verify").await.unwrap();
        assert_eq!(switched.state, ActionState::Unconfirmed);
        assert_eq!(switched.reason_code.as_deref(), Some("result_unproven"));
        assert_eq!(rig.peer.non_get(), 0, "恢复核对只读，绝不重发");
    }

    /// G4 patch7（GC RED b 正臂）：旧代际已受理行点「核对」不再死在旧锁——旧锚
    /// echo 不可达（旧服务死、登记已换代）时复用 confirm_unknown 同款跨代 read-back
    /// （当前可达服务 + 严格三元组 session/规范目录/`msg_<actionId>`）：正式消息记录
    /// 跨换代持久在场 ⇒ 活行动作升级 confirmed（nativeId 在案），全程零重投。
    /// （原 I-1「死代际核对一律原样返回」判据按 G4 patch7 收敛面迁移：缺席才保持
    /// 原状，见 [`verify_keeps_dead_generation_row_without_readback_evidence`]。）
    #[tokio::test]
    async fn verify_converges_dead_generation_row_via_cross_generation_readback() {
        let mut world = World::new();
        world.prompt = PromptMode::QueuedThenDropped;
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_dead", &binding, "旧代际的消息"),
            )
            .await
            .expect("丢响应是未知结果");
        assert_eq!(view.state, ActionState::Unconfirmed);
        assert_eq!(rig.peer.prompt_posts(), 1);

        // 旧代际在世时先按 inbox 证据收敛为已受理（F5 实录形态：acknowledged 行）。
        let acknowledged = rig
            .hub
            .verify(DEVICE_ID, "act_dead")
            .await
            .expect("核对不得失败");
        assert_eq!(acknowledged.state, ActionState::Acknowledged);

        // 旧服务死前确实把消息落进了会话记录（跨换代持久，新代际看得到它）；
        // 服务重启：同端口换 service id + pid，登记重写，旧锚 echo 不可达。
        rig.peer
            .set(|world| world.messages = vec![user_record("msg_act_dead", "旧代际的消息")]);
        rig.peer.restart();
        let verified = rig
            .hub
            .verify(DEVICE_ID, "act_dead")
            .await
            .expect("核对不得失败");
        assert_eq!(
            verified.state,
            ActionState::Confirmed,
            "跨代 read-back 命中正式消息记录 ⇒ 核对收敛 confirmed"
        );
        assert_eq!(
            verified.native_id.as_deref(),
            Some("msg_act_dead"),
            "收敛必须落精确 native message id"
        );
        assert_eq!(rig.peer.prompt_posts(), 1, "核对绝不重发（零重投）");
        let stored = rig.hub.action(DEVICE_ID, "act_dead").await.unwrap();
        assert_eq!(stored.state, ActionState::Confirmed, "账本已收敛");
        assert_eq!(stored.native_id.as_deref(), Some("msg_act_dead"));
    }

    /// G4 patch7（GC RED b 负臂，I-1 保守面保持）：无正式消息记录形态（只入 inbox、
    /// 零落地证据）的旧代际已受理行点「核对」——跨代 read-back 不命中 ⇒ 原样保持
    ///（不假收敛、不置终态）；「确认终态未知」仍是这类行的唯一用户出路。
    #[tokio::test]
    async fn verify_keeps_dead_generation_row_without_readback_evidence() {
        let mut world = World::new();
        world.prompt = PromptMode::QueuedThenDropped;
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_dead_no_ev", &binding, "无落地证据"),
            )
            .await
            .expect("丢响应是未知结果");
        assert_eq!(view.state, ActionState::Unconfirmed);
        let acknowledged = rig
            .hub
            .verify(DEVICE_ID, "act_dead_no_ev")
            .await
            .expect("核对不得失败");
        assert_eq!(acknowledged.state, ActionState::Acknowledged);

        // 服务重启（消息记录始终缺席：旧服务从没把它落进会话）。
        rig.peer.restart();
        let verified = rig
            .hub
            .verify(DEVICE_ID, "act_dead_no_ev")
            .await
            .expect("核对不得失败");
        assert_eq!(
            verified.state,
            ActionState::Acknowledged,
            "read-back 不命中 ⇒ 保持原状（已受理行不得被假收敛）"
        );
        assert_eq!(
            verified.native_id.as_deref(),
            Some("msg_act_dead_no_ev"),
            "已受理时落的原生 id 原样保留（不因核对失败丢失）"
        );
        assert_eq!(rig.peer.prompt_posts(), 1, "核对绝不重发");
        let stored = rig.hub.action(DEVICE_ID, "act_dead_no_ev").await.unwrap();
        assert_eq!(stored.state, ActionState::Acknowledged, "账本零改写");
    }

    /// patch3 D2（H13 证据 §10-D2）：死代际 acknowledged/unconfirmed 行的
    /// 「显式确认终态未知」出口。活代际拒绝（仍可只读核对）；死代际放行 →
    /// `unknown_confirmed` 归档态（审计词 + 原生 id 保留）；确认后**显式重选
    /// （换绑新代际）屏障解除**；重复确认幂等；零重发。
    #[tokio::test]
    async fn confirm_unknown_archives_a_dead_generation_row_and_lifts_the_reselect_barrier() {
        let restarted = Arc::new(AtomicBool::new(false));
        let mut world = World::new();
        // F12 项 2：Ingested 形态会在会话记录里留下正式消息（跨换代持久），归档时
        // read-back 会按证据分流升级 confirmed；本用例考**归档路径** ⇒ 用无记录形态。
        world.prompt = PromptMode::QueuedThenDropped;
        // G3 收紧迁移：死亡证明经三形态缝表达（restart 后 = NoSuchProcess）；
        // 两态缝的 None 只按读取失败处理，不再冒充死亡。
        let (fact, generation) = restart_aware_facts(Arc::clone(&restarted));
        let rig = boot_with_facts(world, fact, generation).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_unknown", &binding, "死代际确认终态未知"),
            )
            .await
            .expect("丢响应是未知结果");
        assert_eq!(view.state, ActionState::Unconfirmed);

        // 活代际：拒绝（只读核对仍然可用，结果不该由确认跳过）
        let refused = rig.hub.confirm_unknown(DEVICE_ID, "act_unknown").await;
        let Err(refused) = refused else {
            panic!("活代际必须拒绝确认");
        };
        assert_eq!(refused.code, action_codes::STATE);
        assert_eq!(
            rig.hub
                .action(DEVICE_ID, "act_unknown")
                .await
                .unwrap()
                .state,
            ActionState::Unconfirmed,
            "拒绝零改写"
        );

        // 拒绝期间换代表明未决屏障仍在：显式重入被拒（Ok(None)）
        rig.peer.restart();
        restarted.store(true, Ordering::SeqCst);
        assert!(
            rig.hub
                .reenter_native_route(&rig.card, Some(&binding))
                .await
                .expect("重入不得报错")
                .is_none(),
            "未决动作必须挡住显式重选"
        );

        // 确认：死代际行放行 → unknown_confirmed 归档态
        let confirmed = rig
            .hub
            .confirm_unknown(DEVICE_ID, "act_unknown")
            .await
            .expect("死代际确认放行");
        assert_eq!(confirmed.state, ActionState::UnknownConfirmed);
        assert_eq!(
            confirmed.reason_code.as_deref(),
            Some(dao::USER_CONFIRMED_UNKNOWN),
            "审计词必须在案"
        );
        assert_eq!(confirmed.native_id, None);

        // 重复确认：幂等原样返回（终态不可再变）
        let again = rig
            .hub
            .confirm_unknown(DEVICE_ID, "act_unknown")
            .await
            .expect("重复确认幂等");
        assert_eq!(again.state, ActionState::UnknownConfirmed);

        // 未决屏障解除：显式重选（换绑新代际）成功
        let rebound = rig
            .hub
            .reenter_native_route(&rig.card, Some(&binding))
            .await
            .expect("重入不得报错");
        assert!(rebound.is_some(), "确认后重选屏障解除");
    }

    /// M3（P1-c）：行代际呈现面（`runtime_dead`）——只读、零改写，只对
    /// `acknowledged`/`unconfirmed` 携带：活代际 `Some(false)`、代际不可证
    /// （进程事实死亡）`Some(true)`、其余状态缺席；呈现面放行的行与
    /// confirm-unknown 权威闸同向（同一 process_fact 判定惯例，两口径不冲突）。
    #[tokio::test]
    async fn runtime_dead_presentation_tracks_row_generation_fact() {
        let mut world = World::new();
        world.prompt = PromptMode::QueuedThenDropped;
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_rd", &binding, "死代际呈现面"),
            )
            .await
            .expect("丢响应是未知结果");
        assert_eq!(view.state, ActionState::Unconfirmed);
        assert_eq!(view.runtime_dead, None, "派发回执不携带呈现面");

        // 活代际 unconfirmed：可证明 = 当前活代际 ⇒ false（核对闸仍会拒绝确认）
        let read = rig.hub.action(DEVICE_ID, "act_rd").await.unwrap();
        assert_eq!(read.state, ActionState::Unconfirmed);
        assert_eq!(read.runtime_dead, Some(false));

        // 核对收敛 acknowledged（只入 inbox）：仍活代际 ⇒ false
        let verified = rig.hub.verify(DEVICE_ID, "act_rd").await.unwrap();
        assert_eq!(verified.state, ActionState::Acknowledged);
        assert_eq!(verified.native_id.as_deref(), Some("msg_act_rd"));
        assert_eq!(verified.runtime_dead, Some(false));

        // 已定案行（confirmed）不参与呈现面：缺席（未判定 ≠ 任何方向）
        rig.hub
            .action_store()
            .finish("act_rd", ActionState::Confirmed, Some("msg_act_rd"), None)
            .unwrap();
        let settled = rig.hub.action(DEVICE_ID, "act_rd").await.unwrap();
        assert_eq!(settled.state, ActionState::Confirmed);
        assert_eq!(settled.runtime_dead, None);

        // 重新造一条未决行（unconfirmed）后服务代际死亡（换代 + 原进程退出可证）：
        // 呈现面 true，且与 confirm-unknown 权威闸同向（死代际放行归档）。
        let restarted2 = Arc::new(AtomicBool::new(false));
        let mut world2 = World::new();
        // 无记录形态（归档路径；Ingested 会留下正式消息 → read-back 分流 confirmed）。
        world2.prompt = PromptMode::QueuedThenDropped;
        // G3 收紧迁移：restart 后的死亡经三形态缝表达为 NoSuchProcess。
        let (fact2, generation2) = restart_aware_facts(Arc::clone(&restarted2));
        let rig2 = boot_with_facts(world2, fact2, generation2).await;
        claim_route(&rig2);
        let binding2 = current_binding(&rig2).await;
        let view2 = rig2
            .hub
            .execute(
                &actor(),
                &rig2.card,
                send_cmd("act_rd2", &binding2, "死代际呈现面二"),
            )
            .await
            .expect("丢响应是未知结果");
        assert_eq!(view2.state, ActionState::Unconfirmed);
        assert_eq!(
            rig2.hub
                .action(DEVICE_ID, "act_rd2")
                .await
                .unwrap()
                .runtime_dead,
            Some(false),
            "活代际先证 false"
        );
        rig2.peer.restart();
        restarted2.store(true, Ordering::SeqCst);
        let dead = rig2.hub.action(DEVICE_ID, "act_rd2").await.unwrap();
        assert_eq!(dead.state, ActionState::Unconfirmed, "呈现面零改写");
        assert_eq!(
            dead.runtime_dead,
            Some(true),
            "进程事实死亡 ⇒ 不可证 ⇒ true"
        );
        let archived = rig2
            .hub
            .confirm_unknown(DEVICE_ID, "act_rd2")
            .await
            .expect("呈现面 true 的行权威闸同步放行");
        assert_eq!(archived.state, ActionState::UnknownConfirmed);
        assert_eq!(archived.runtime_dead, None, "归档态不参与呈现面");
    }

    /// patch3 D2：`unknown_confirmed` 行对只读核对与核对类收敛不可见（终态不可再变），
    /// 对作废候选集不可见（force 交回一条都不许再碰）。
    #[tokio::test]
    async fn confirm_unknown_is_final_and_invisible_to_force_and_verify() {
        let restarted = Arc::new(AtomicBool::new(false));
        let mut world = World::new();
        // 同上：归档路径考无记录形态（QueuedThenDropped），有证据的升级面由
        // archive_splits_on_native_message_evidence 专考。
        world.prompt = PromptMode::QueuedThenDropped;
        // G3 收紧迁移：restart 后的死亡经三形态缝表达为 NoSuchProcess。
        let (fact, generation) = restart_aware_facts(Arc::clone(&restarted));
        let rig = boot_with_facts(world, fact, generation).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        rig.hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_archived", &binding, "归档态不可再变"),
            )
            .await
            .expect("丢响应是未知结果");
        rig.peer.restart();
        restarted.store(true, Ordering::SeqCst);
        let confirmed = rig
            .hub
            .confirm_unknown(DEVICE_ID, "act_archived")
            .await
            .expect("死代际确认放行");
        assert_eq!(confirmed.state, ActionState::UnknownConfirmed);

        // verify：非未知状态原样返回（不收敛、不改写）
        let verified = rig
            .hub
            .verify(DEVICE_ID, "act_archived")
            .await
            .expect("核对不得失败");
        assert_eq!(verified.state, ActionState::UnknownConfirmed);
        assert_eq!(
            rig.peer.prompt_posts(),
            1,
            "核对绝不重发（仍是当初那一次派发）"
        );

        // force 作废候选：归档行不在候选集（unverifiable_count 为 0）
        assert_eq!(
            rig.hub
                .action_store()
                .unverifiable_count(&rig.card.id, None)
                .expect("作废候选预读不得失败"),
            0,
            "归档行不进入作废候选"
        );
    }

    /// I-2(a)：claim 之前被取消（行停在 `not_sent` 且无理由）⇒ 同一命令重试必须真正
    /// 派发恰好一次；终态后再重试则原样返回。
    #[tokio::test]
    async fn cancelled_before_claim_is_retried_by_the_same_command() {
        let world = World::new();
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let command = send_cmd("act_cancel", &binding, "取消后重试");

        // 模拟「reserve 之后、claim 之前」future 被丢弃：只留一行 not_sent。
        let store = rig.hub.action_store();
        assert!(matches!(
            store
                .reserve(DEVICE_ID, &bound_target(&rig.project), &command)
                .expect("reserve"),
            ReserveOutcome::New
        ));
        assert_eq!(
            rig.hub.action(DEVICE_ID, "act_cancel").await.unwrap().state,
            ActionState::NotSent,
            "行停在未派发"
        );
        assert_eq!(rig.peer.prompt_posts(), 0, "尚未派发");

        // 同一命令重试：必须继续走派发路径（claim 的 CAS 保证最多一次写出）。
        let view = rig
            .hub
            .execute(&actor(), &rig.card, command)
            .await
            .expect("未 claim 的行必须继续派发");
        assert_eq!(view.state, ActionState::Confirmed);
        assert_eq!(rig.peer.prompt_posts(), 1, "恰好一次派发");

        // 终态后重复点击：原样返回，绝不二次写。
        let again = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_cancel", &binding, "取消后重试"),
            )
            .await
            .expect("终态动作原样返回");
        assert_eq!(again.state, ActionState::Confirmed);
        assert_eq!(rig.peer.prompt_posts(), 1, "终态不得二次派发");
    }

    /// I-2(b)：claim 之后被取消（行停在 `dispatching`）⇒ `verify` 必须能收敛它：
    /// 有证据按证据落账，无证据收敛为 `Unconfirmed`——绝不永远卡在飞。
    #[tokio::test]
    async fn verify_converges_a_row_left_dispatching() {
        let world = World::new();
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;

        // 相位一：消息已在列表 ⇒ Confirmed（派发其实成功过，只是从未收尾）。
        let command = send_cmd("act_dangling", &binding, "派发中被取消");
        let store = rig.hub.action_store();
        assert!(matches!(
            store
                .reserve(DEVICE_ID, &bound_target(&rig.project), &command)
                .expect("reserve"),
            ReserveOutcome::New
        ));
        assert!(
            store.claim("act_dangling").expect("claim"),
            "claim 之后行停在 dispatching"
        );
        rig.peer
            .set(|world| world.messages = vec![user_record("msg_act_dangling", "派发中被取消")]);
        let verified = rig
            .hub
            .verify(DEVICE_ID, "act_dangling")
            .await
            .expect("核对不得失败");
        assert_eq!(verified.state, ActionState::Confirmed);
        assert_eq!(verified.native_id.as_deref(), Some("msg_act_dangling"));
        assert_eq!(rig.peer.prompt_posts(), 0, "核对绝不重发");

        // 相位二：无任何证据 ⇒ Unconfirmed（收敛，不卡在飞）。
        let command = send_cmd("act_dangling_2", &binding, "派发中被取消");
        assert!(matches!(
            store
                .reserve(DEVICE_ID, &bound_target(&rig.project), &command)
                .expect("reserve"),
            ReserveOutcome::New
        ));
        assert!(store.claim("act_dangling_2").expect("claim"));
        rig.peer.set(|world| world.messages = Vec::new());
        let verified = rig
            .hub
            .verify(DEVICE_ID, "act_dangling_2")
            .await
            .expect("核对不得失败");
        assert_eq!(
            verified.state,
            ActionState::Unconfirmed,
            "无证据的在飞行必须收敛为 Unconfirmed"
        );
        assert_eq!(verified.reason_code.as_deref(), Some("native_id_absent"));
    }

    /// M-4：重复点击 `verify`（良性落账竞态）返回最新视图，而不是 `OC_ACTION_STATE` 错误。
    #[tokio::test]
    async fn double_verify_returns_the_latest_view_instead_of_an_error() {
        let mut world = World::new();
        world.prompt = PromptMode::IngestedThenDropped;
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_double", &binding, "重复核对"),
            )
            .await
            .expect("丢响应是未知结果");
        assert_eq!(view.state, ActionState::Unconfirmed);

        // 第一次核对收敛为 Confirmed；再核对一次必须仍拿到视图（终态无可证明转移）。
        let first = rig
            .hub
            .verify(DEVICE_ID, "act_double")
            .await
            .expect("核对一");
        assert_eq!(first.state, ActionState::Confirmed);
        let second = rig
            .hub
            .verify(DEVICE_ID, "act_double")
            .await
            .expect("终态上的重复核对不得报错");
        assert_eq!(second.state, ActionState::Confirmed);
    }

    /// M-6：静态拒绝（超长/空正文/非法动作 id/关联 id 不符/steer 写入/非 once-reject 决定词）
    /// 全部零网络、零写出——连服务都不必打扰。
    #[tokio::test]
    async fn static_refusals_never_touch_the_service() {
        let world = World::new();
        let rig = boot(world).await;
        // 刻意不认领路由、不取绑定令牌：静态复核必须先于这一切。
        let binding = "bind-unset";

        let long = "x".repeat(crate::remote::api::MAX_SEND_CHARS + 1);
        let err = rig
            .hub
            .execute(&actor(), &rig.card, send_cmd("act_long", binding, &long))
            .await
            .expect_err("超长正文必须拒绝");
        assert_eq!(err.code, codes::OC_SEND_TOO_LONG);

        let err = rig
            .hub
            .execute(&actor(), &rig.card, send_cmd("act_empty", binding, "   "))
            .await
            .expect_err("空正文必须拒绝");
        assert_eq!(err.code, codes::OC_SEND_EMPTY);

        let err = rig
            .hub
            .execute(&actor(), &rig.card, send_cmd("act-bad-id", binding, "hi"))
            .await
            .expect_err("非法动作 id 必须拒绝");
        assert_eq!(err.code, codes::OC_ACTION_INVALID);

        let mut mismatched = send_cmd("act_mismatch", binding, "hi");
        if let NativeOperation::Send(payload) = &mut mismatched.operation {
            payload.client_message_id = "msg_someone_else".to_string();
        }
        let err = rig
            .hub
            .execute(&actor(), &rig.card, mismatched)
            .await
            .expect_err("关联 id 与 action_id 不符必须拒绝");
        assert_eq!(err.code, codes::OC_ACTION_INVALID);

        let err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                decide_cmd("act_never", binding, "token-unset", "never"),
            )
            .await
            .expect_err("服务端未列出的决定词必须拒绝");
        assert_eq!(err.code, codes::OC_COMMAND_UNSUPPORTED);

        let err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                decide_cmd("act_always_closed", binding, "token-unset", "always"),
            )
            .await
            .expect_err("T7 未完成前 always 永久授权必须关闭");
        assert_eq!(err.code, codes::OC_COMMAND_UNSUPPORTED);

        assert_eq!(rig.peer.total(), 0, "静态拒绝零网络：服务一次都没被打扰");
    }

    /// R1：表单仍在 pending 只证明「结果未知」。核对必须保持 `Unconfirmed`（绝不
    /// `NotAccepted`、绝不释放占用），直到权威回读给出定案词才收敛为 `Settled`。
    #[tokio::test]
    async fn unconfirmed_answer_stays_unconfirmed_while_the_form_is_pending() {
        let mut world = World::new();
        let mut form = form_entry();
        form.reply = Reply::Conflict;
        form.detail_fails_after_reply = true;
        world.forms = vec![form];
        let rig = boot(world).await;
        claim_route(&rig);
        let first = rig.hub.view(&rig.card).await.expect("首轮投影");
        let (binding, token) = binding_and_form_token(&first);

        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                answer_cmd("act_proof", &binding, &token, q0_blue()),
            )
            .await
            .expect("409 + 读不到终态 = 未知结果");
        assert_eq!(view.state, ActionState::Unconfirmed);
        assert_eq!(rig.peer.form_reply_posts(), 1, "只提交一次");

        // 证据不足时占用不放：同一表单的第二个动作必须冲突。
        let second = answer_cmd("act_proof_2", &binding, &token, q0_blue());
        let err = rig
            .hub
            .execute(&actor(), &rig.card, second)
            .await
            .expect_err("没有证据不得释放占用");
        assert_eq!(err.code, action_codes::CONFLICT);

        // 恢复 detail 可读（只读核对必须能在可读时证明某事）：表单仍在 pending。
        rig.peer.set(|world| world.forms[0].detail_status = None);
        let verified = rig
            .hub
            .verify(DEVICE_ID, "act_proof")
            .await
            .expect("核对不得失败");
        assert_eq!(
            verified.state,
            ActionState::Unconfirmed,
            "pending 只证明结果未知，不是「整份未生效」"
        );
        assert_eq!(
            verified.reason_code.as_deref(),
            Some(reason_words::READBACK_FAILED)
        );
        assert_eq!(verified.native_id, None);
        assert_eq!(rig.peer.form_reply_posts(), 1, "核对绝不重发");

        // 占用未释放：换新 actionId 仍必须冲突（未知结果不能当重试许可）。
        let third = answer_cmd("act_proof_3", &binding, &token, q0_blue());
        let err = rig
            .hub
            .execute(&actor(), &rig.card, third)
            .await
            .expect_err("核对不得释放占用");
        assert_eq!(err.code, action_codes::CONFLICT);

        // 定案后（权威回读给出终结词）才允许收敛；核对仍然绝不重发。
        rig.peer
            .set(|world| world.forms[0].status = "answered".into());
        let settled = rig
            .hub
            .verify(DEVICE_ID, "act_proof")
            .await
            .expect("核对不得失败");
        assert_eq!(settled.state, ActionState::Settled);
        assert_eq!(
            settled.reason_code.as_deref(),
            Some(reason_words::ALREADY_SETTLED)
        );
        assert_eq!(rig.peer.form_reply_posts(), 1, "核对绝不重发");
    }

    /// R1：审批仍在 pending 时核对只能保持 `Unconfirmed`——不把「尚未决定」写成
    /// 「整份未生效」，占用继续持有。
    #[tokio::test]
    async fn decide_pending_keeps_unconfirmed() {
        let mut world = World::new();
        let mut permission = permission_entry();
        permission.reply = Reply::Conflict;
        world.permissions = vec![permission];
        let rig = boot(world).await;
        claim_route(&rig);
        let first = rig.hub.view(&rig.card).await.expect("首轮投影");
        let binding = first.binding_token.clone();
        let token = permission_token_of(&first);

        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                decide_cmd("act_pending_decide", &binding, &token, "reject"),
            )
            .await
            .expect("409 + 仍在 pending = 未知结果");
        assert_eq!(view.state, ActionState::Unconfirmed);

        let verified = rig
            .hub
            .verify(DEVICE_ID, "act_pending_decide")
            .await
            .expect("核对不得失败");
        assert_eq!(
            verified.state,
            ActionState::Unconfirmed,
            "审批仍 pending：不得落 NotAccepted"
        );
        assert_eq!(
            verified.reason_code.as_deref(),
            Some(reason_words::READBACK_FAILED)
        );

        // 占用未释放：同一审批的第二个动作仍冲突。
        let err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                decide_cmd("act_pending_decide_2", &binding, &token, "reject"),
            )
            .await
            .expect_err("核对不得释放占用");
        assert_eq!(err.code, action_codes::CONFLICT);
        assert_eq!(
            rig.peer.count(
                "POST",
                &format!("/api/session/{SESSION_ID}/permission/{PERMISSION_ID}/reply")
            ),
            1,
            "核对绝不重发"
        );
    }

    /// R1：撤回后项仍在 inbox，只证明「撤回尚未生效」——核对必须保持 `Unconfirmed`，
    /// 不得把仍在队列的项写成 `NotAccepted`。
    #[tokio::test]
    async fn retract_in_inbox_keeps_unconfirmed() {
        let mut world = World::new();
        world.inbox = vec![inbox_item(ITEM_ID, "排队中")];
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;

        // 派发被中断的形态：行停在 `Unconfirmed`，而回读仍看得到该项在队列里。
        let store = rig.hub.action_store();
        assert!(matches!(
            store
                .reserve(
                    DEVICE_ID,
                    &bound_target(&rig.project),
                    &retract_cmd("act_retract_pending", &binding),
                )
                .expect("reserve"),
            ReserveOutcome::New
        ));
        assert!(store.claim("act_retract_pending").expect("claim"));
        store
            .finish(
                "act_retract_pending",
                ActionState::Unconfirmed,
                None,
                Some(reason_words::WRITE_REFUSED),
            )
            .expect("落未知结果");

        let verified = rig
            .hub
            .verify(DEVICE_ID, "act_retract_pending")
            .await
            .expect("核对不得失败");
        assert_eq!(
            verified.state,
            ActionState::Unconfirmed,
            "仍在 inbox 只证明撤回尚未生效，不得落 NotAccepted"
        );
        assert_eq!(
            verified.reason_code.as_deref(),
            Some(reason_words::READBACK_FAILED)
        );
        assert_eq!(
            rig.peer.count(
                "DELETE",
                &format!("/api/session/{SESSION_ID}/inbox/{ITEM_ID}")
            ),
            0,
            "核对绝不重发"
        );
    }

    fn target() -> NativeTarget {
        NativeTarget {
            runtime: runtime(4242),
            session_id: "ses_1".into(),
            directory: PathBuf::from("/tmp/oc/项目"),
        }
    }

    fn runtime(pid: u32) -> RuntimeKey {
        RuntimeKey {
            service_id: "svc-1".into(),
            pid,
            started_at_ms: 1_700_000_000_000,
        }
    }

    fn target_on(runtime: RuntimeKey) -> NativeTarget {
        NativeTarget {
            runtime,
            session_id: "ses_1".into(),
            directory: PathBuf::from("/tmp/oc/项目"),
        }
    }

    /// 任意会话 + 代际的目标（多会话作用域用例）
    fn target_of(session_id: &str, runtime: RuntimeKey) -> NativeTarget {
        NativeTarget {
            runtime,
            session_id: session_id.into(),
            directory: PathBuf::from("/tmp/oc/项目"),
        }
    }

    fn send(action_id: &str, text: &str) -> NativeCommand {
        NativeCommand {
            action_id: action_id.into(),
            session_id: "ses_1".into(),
            binding_token: "bt-1".into(),
            operation: NativeOperation::Send(SendPayload {
                text: text.into(),
                attachment_paths: Vec::new(),
                delivery: DeliveryKind::Queue,
                client_message_id: format!("msg_{action_id}"),
            }),
        }
    }

    /// 任意会话的发送命令
    fn send_in(session_id: &str, action_id: &str, text: &str) -> NativeCommand {
        NativeCommand {
            session_id: session_id.into(),
            ..send(action_id, text)
        }
    }

    fn answer(action_id: &str, form_id: &str) -> NativeCommand {
        let mut answers = Answers::new();
        answers.insert("q1".to_string(), DraftAnswer::Text("甲".into()));
        NativeCommand {
            action_id: action_id.into(),
            session_id: "ses_1".into(),
            binding_token: "bt-1".into(),
            operation: NativeOperation::Answer(AnswerPayload {
                form_id: form_id.into(),
                context_token: "ct-1".into(),
                answers,
            }),
        }
    }

    fn opened() -> (DeviceStore, NativeActionStore) {
        let device = DeviceStore::memory();
        let actions = NativeActionStore::new(device.clone());
        (device, actions)
    }

    fn action_rows(device: &DeviceStore) -> i64 {
        device.with(|c| {
            c.query_row("SELECT COUNT(*) FROM opencode_native_actions", [], |r| {
                r.get(0)
            })
            .unwrap()
        })
    }

    /// 把某表全部单元格拼成一段文本（隐私回归：账本里不得出现正文/答案原文）
    fn table_dump(device: &DeviceStore, table: &str) -> String {
        device.with(|c| {
            let mut stmt = c.prepare(&format!("SELECT * FROM {table}")).unwrap();
            let cols = stmt.column_count();
            let mut out = String::new();
            let mut rows = stmt.query([]).unwrap();
            while let Some(row) = rows.next().unwrap() {
                for i in 0..cols {
                    match row.get_ref(i).unwrap() {
                        rusqlite::types::ValueRef::Text(bytes) => {
                            out.push_str(&String::from_utf8_lossy(bytes));
                        }
                        rusqlite::types::ValueRef::Integer(value) => {
                            out.push_str(&value.to_string());
                        }
                        _ => {}
                    }
                    out.push('|');
                }
            }
            out
        })
    }

    fn route_row(device: &DeviceStore, session_id: &str) -> Option<dao::RouteRow> {
        device.with(|c| dao::route_conn(c, session_id).unwrap())
    }

    /// 同 actionId 同载荷 = 一次派发：重复 reserve 不产生第二条记录，claim 只成功一次
    #[test]
    fn same_action_same_payload_dispatches_once() {
        let (device, actions) = opened();
        let command = send("act-1", "你好");
        assert_eq!(
            actions.reserve("dev-1", &target(), &command).unwrap(),
            ReserveOutcome::New
        );
        assert_eq!(
            actions.reserve("dev-1", &target(), &command).unwrap(),
            ReserveOutcome::Existing(ActionView {
                action_id: "act-1".into(),
                state: ActionState::NotSent,
                reason_code: None,
                native_id: None,
                runtime_dead: None,
            }),
            "同指纹重复请求返回既有动作，绝不第二次写"
        );
        assert!(actions.claim("act-1").unwrap(), "首次 claim 取得写出权");
        assert!(!actions.claim("act-1").unwrap(), "同一动作只能写一次");
        assert_eq!(action_rows(&device), 1, "同一 actionId 只允许一行");
        // 重开库后重复请求仍不新增行、也不重新派发
        let reopened = NativeActionStore::new(device.clone());
        assert!(matches!(
            reopened.reserve("dev-1", &target(), &command).unwrap(),
            ReserveOutcome::Existing(_)
        ));
        assert!(!reopened.claim("act-1").unwrap());
        assert_eq!(action_rows(&device), 1);
    }

    /// R-1（终审残洞）：**当前代际不可证**（服务被卸载 / 进程事实读不出）时，强制交回
    /// 必须能作废该会话的**全部**未终态行。
    ///
    /// 为什么这是必需的：这种形态下没有任何代际可比，`verify` 也永远收敛不了；若仍要求
    /// 「同代际才算不可核对」，会话就永久卡在「原生写不了、旧路径 409、交回也被拒」。
    /// 作废只是把「本端再也证明不了结果」写进墓碑（不是声称未写出），占用同时释放。
    #[test]
    fn abandon_without_a_current_generation_releases_every_unresolved_action() {
        let (device, actions) = opened();
        // 一条未派发（not_sent）、一条已受理未确认（acknowledged）
        actions
            .reserve("dev-1", &target(), &send("act-1", "甲"))
            .unwrap();
        actions
            .reserve("dev-1", &target(), &send("act-2", "乙"))
            .unwrap();
        assert!(actions.claim("act-2").unwrap());
        actions
            .finish("act-2", ActionState::Acknowledged, Some("msg_act-2"), None)
            .unwrap();

        let session_id = target().session_id.clone();
        // 本进程**正在写**的行（dispatching + 本代次）：写请求可能已在飞，强制交回
        // 不得越权把它作废——否则「可能已写出」会变成账本上的 not_sent，占用被释放，
        // 同一表单可被二次作答。
        actions
            .reserve("dev-1", &target(), &send("act-3", "丙"))
            .unwrap();
        assert!(actions.claim("act-3").unwrap());
        // 代际不可证 ⇒ 不是 0，而是「每条**可作废**的未终态行都算」（R-1 的核心修正）
        assert_eq!(
            actions.unverifiable_count(&session_id, None).unwrap(),
            2,
            "没有当前代际时，全部可作废的未终态行都算；本进程在飞的那条除外"
        );
        assert_eq!(
            actions.abandon_unverifiable(&session_id, None).unwrap(),
            2,
            "强制交回只作废可作废的两条，不碰在飞行"
        );
        let in_flight = device.with(|c| dao::get_action_conn(c, "act-3").unwrap().unwrap());
        assert_eq!(
            in_flight.state,
            ActionState::Dispatching,
            "本进程在飞的行必须保持原状（由它自己收敛）"
        );

        for id in ["act-1", "act-2"] {
            let row = device.with(|c| dao::get_action_conn(c, id).unwrap().unwrap());
            assert_eq!(row.state, ActionState::NotSent, "{id} 落墓碑态");
            assert_eq!(
                row.reason_code.as_deref(),
                Some(abandon_reasons::UNVERIFIABLE),
                "{id} 必须写明「代际不可核对」，不是普通的未发送"
            );
            assert!(
                !actions.claim(id).unwrap(),
                "{id} 是墓碑，绝不能再次认领派发"
            );
        }
        // 占用真的释放了：作废行在 `opencode_native_claims` 里不再持有任何键
        let held = device.with(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM opencode_native_claims WHERE action_id IN ('act-1','act-2')",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        });
        assert_eq!(held, 0, "作废必须在同一事务里释放待答占用");
        // 幂等：再作废一次不产生第二遍效果
        assert_eq!(
            actions.abandon_unverifiable(&session_id, None).unwrap(),
            0,
            "已终态的行不会被重复作废"
        );
    }

    /// G7：`Some(当前代际)` 分支必须排除**本进程在飞**的 `dispatching` 行——即使该行的
    /// 存储代际已经过期（服务换代了，但这次写仍可能在本进程里飞），也不能被强制交回作废。
    #[test]
    fn dead_generation_dispatching_in_flight_is_not_abandoned() {
        let (device, actions) = opened();
        let live = runtime(4242);
        assert!(actions.try_claim_route(&target(), true).unwrap());
        let dead_target = target_on(runtime(9999));
        // 在飞行：存储代际是旧服务，但 dispatch_epoch = 本进程代次（claim 写入）。
        actions
            .reserve("dev-1", &dead_target, &send("act-inflight", "甲"))
            .unwrap();
        assert!(actions.claim("act-inflight").unwrap());
        // 对照：死代际的 `acknowledged` 行仍必须算「可作废」（排除只针对本进程在飞）。
        actions
            .reserve("dev-1", &dead_target, &send("act-ack", "乙"))
            .unwrap();
        assert!(actions.claim("act-ack").unwrap());
        actions
            .finish(
                "act-ack",
                ActionState::Acknowledged,
                Some("msg_act-ack"),
                None,
            )
            .unwrap();

        let session = target().session_id.clone();
        assert_eq!(
            actions.unverifiable_count(&session, Some(&live)).unwrap(),
            1,
            "本进程在飞的 dispatching 不算可作废；死代际的 acknowledged 算"
        );
        assert_eq!(
            actions.abandon_unverifiable(&session, Some(&live)).unwrap(),
            1,
            "只作废可作废的那一条"
        );
        let in_flight = device.with(|c| dao::get_action_conn(c, "act-inflight").unwrap().unwrap());
        assert_eq!(
            in_flight.state,
            ActionState::Dispatching,
            "本进程在飞的行必须保持原状（由它自己收敛）"
        );
        assert!(in_flight.reason_code.is_none(), "在飞行不得被写墓碑碑文");
    }

    /// G7：作废与交回是**同一个事务**——交回不成立（仍有剩余未决）或落账失败时，
    /// 一个墓碑都不许留下，路由也不许改变。
    #[test]
    fn abandon_and_return_is_atomic_on_failure() {
        let (device, actions) = opened();
        let live = runtime(4242);
        assert!(actions.try_claim_route(&target(), true).unwrap());
        let dead_target = target_on(runtime(9999));
        // 候选：死代际的未定案行
        actions
            .reserve("dev-1", &dead_target, &send("act-dead", "甲"))
            .unwrap();
        assert!(actions.claim("act-dead").unwrap());
        actions
            .finish(
                "act-dead",
                ActionState::Acknowledged,
                Some("msg_act-dead"),
                None,
            )
            .unwrap();
        // 非候选但会挡住交回：本进程在飞的 dispatching
        actions
            .reserve("dev-1", &dead_target, &send("act-inflight", "乙"))
            .unwrap();
        assert!(actions.claim("act-inflight").unwrap());

        let session = target().session_id.clone();
        // 剩余未决 ⇒ 交回不成立 ⇒ 整体回滚：零墓碑、路由不变。
        assert_eq!(
            actions
                .abandon_and_return(&session, Some(&live), &[])
                .unwrap(),
            (false, 0)
        );
        let dead = device.with(|c| dao::get_action_conn(c, "act-dead").unwrap().unwrap());
        assert_eq!(
            dead.state,
            ActionState::Acknowledged,
            "交回不成立时不得留下任何墓碑"
        );
        assert!(dead.reason_code.is_none(), "不得写碑文");
        assert_eq!(
            actions.route_state(&session).unwrap().as_deref(),
            Some(dao::ROUTE_ACTIVE),
            "交回不成立时路由不变"
        );

        // 移除在飞行后：一次事务完成「作废 + 交回」。
        device.with(|c| {
            c.execute(
                "DELETE FROM opencode_native_actions WHERE action_id = 'act-inflight'",
                [],
            )
            .unwrap()
        });
        assert_eq!(
            actions
                .abandon_and_return(&session, Some(&live), &[])
                .unwrap(),
            (true, 1)
        );
        let dead = device.with(|c| dao::get_action_conn(c, "act-dead").unwrap().unwrap());
        assert_eq!(dead.state, ActionState::NotSent);
        assert_eq!(
            dead.reason_code.as_deref(),
            Some(dao::UNVERIFIABLE_GENERATION)
        );
        assert!(!actions.claim("act-dead").unwrap(), "墓碑不可再派发");
        assert_eq!(
            actions.route_state(&session).unwrap().as_deref(),
            Some(dao::ROUTE_DRAINING)
        );

        // 注入失败：先让路由回到 `active`——这样「路由不变」的断言才不是空转（否则它
        // 本来就已是 draining）。墓碑落账之后的释放占用失败 ⇒ 整事务回滚：零墓碑、
        // 行原样保留、路由保持 active。
        assert!(
            actions.try_claim_route_with(&target(), true, true).unwrap(),
            "测试装配：把路由拉回 active"
        );
        assert_eq!(
            actions.route_state(&session).unwrap().as_deref(),
            Some(dao::ROUTE_ACTIVE),
            "注入前路由必须是 active"
        );
        actions
            .reserve("dev-1", &dead_target, &answer("act-atomic", "form-atomic"))
            .unwrap();
        assert!(actions.claim("act-atomic").unwrap());
        actions
            .finish(
                "act-atomic",
                ActionState::Acknowledged,
                Some("msg_act-atomic"),
                None,
            )
            .unwrap();
        device.with(|c| c.execute("DROP TABLE opencode_native_claims", []).unwrap());
        let err = actions
            .abandon_and_return(&session, Some(&live), &[])
            .unwrap_err();
        assert_eq!(err.code, action_codes::STORE_FAILED);
        let atomic = device.with(|c| dao::get_action_conn(c, "act-atomic").unwrap().unwrap());
        assert_eq!(
            atomic.state,
            ActionState::Acknowledged,
            "落账失败必须整事务回滚：不得留下墓碑"
        );
        assert!(atomic.reason_code.is_none(), "零墓碑");
        assert_eq!(
            actions.route_state(&session).unwrap().as_deref(),
            Some(dao::ROUTE_ACTIVE),
            "事务失败时路由必须保持注入前的 active"
        );
    }

    /// 同 actionId 异载荷 = 冲突，且既有行的指纹与状态都不被改写
    #[test]
    fn same_action_changed_payload_conflicts() {
        let (device, actions) = opened();
        assert_eq!(
            actions
                .reserve("dev-1", &target(), &send("act-1", "甲"))
                .unwrap(),
            ReserveOutcome::New
        );
        assert_eq!(
            actions
                .reserve("dev-1", &target(), &send("act-1", "乙"))
                .unwrap(),
            ReserveOutcome::Conflict,
            "同 actionId 换正文必须冲突"
        );
        // 换目标（同 id 换会话/目录/代际）同样冲突，不能串到别的会话
        let mut other_session = send("act-1", "甲");
        other_session.session_id = "ses_2".into();
        let mut other_target = target();
        other_target.session_id = "ses_2".into();
        assert_eq!(
            actions
                .reserve("dev-1", &other_target, &other_session)
                .unwrap(),
            ReserveOutcome::Conflict
        );
        // 原意图仍可幂等重放，指纹未变
        assert!(matches!(
            actions
                .reserve("dev-1", &target(), &send("act-1", "甲"))
                .unwrap(),
            ReserveOutcome::Existing(_)
        ));
        let stored = device.with(|c| dao::get_action_conn(c, "act-1").unwrap().unwrap());
        assert_eq!(stored.payload_sha256, payload_sha256(&send("act-1", "甲")));
        assert_eq!(stored.state, ActionState::NotSent);
        assert_eq!(action_rows(&device), 1);
    }

    /// 另一台设备既读不到别人的动作，也不能凭同 actionId 复用别人的写出权
    #[test]
    fn other_device_cannot_read_action() {
        let (device, actions) = opened();
        let command = send("act-1", "甲");
        actions.reserve("dev-1", &target(), &command).unwrap();
        assert_eq!(
            actions.read("dev-1", "act-1").unwrap().state,
            ActionState::NotSent
        );
        let err = actions.read("dev-2", "act-1").unwrap_err();
        assert_eq!(err.code, action_codes::NOT_FOUND);
        assert_eq!(
            actions.read("dev-1", "act-unknown").unwrap_err().code,
            action_codes::NOT_FOUND,
            "不存在的动作与别人的动作同码（不给存在性预言机）"
        );
        assert_eq!(
            actions
                .reserve("dev-2", &target(), &command)
                .unwrap_err()
                .code,
            action_codes::NOT_FOUND,
            "他设备不能接管别人的动作，也不能探测动作是否存在"
        );
        let stored = device.with(|conn| dao::get_action_conn(conn, "act-1").unwrap().unwrap());
        assert_eq!(stored.device_id, "dev-1", "属主不得改变");
        assert_eq!(stored.state, ActionState::NotSent, "不得取得写出权");
        assert_eq!(stored.payload_sha256, payload_sha256(&command));
        assert_eq!(action_rows(&device), 1, "不得创建额外动作行");
    }

    /// 重新打开数据库：遗留 dispatching 变 unconfirmed，且绝不再派发
    #[test]
    fn reopen_after_claim_keeps_unconfirmed() {
        let (device, actions) = opened();
        let command = send("act-1", "甲");
        actions.reserve("dev-1", &target(), &command).unwrap();
        assert!(actions.claim("act-1").unwrap());
        // 模拟崩溃后重启：同一库、**新进程代次**的实例
        let reopened = NativeActionStore::with_epoch(device.clone(), "epoch-restart");
        let view = reopened.read("dev-1", "act-1").unwrap();
        assert_eq!(view.state, ActionState::Unconfirmed);
        assert_eq!(view.reason_code.as_deref(), Some("process_restart"));
        assert!(!reopened.claim("act-1").unwrap(), "结果不明绝不重发");
        assert!(matches!(
            reopened.reserve("dev-1", &target(), &command).unwrap(),
            ReserveOutcome::Existing(ActionView {
                state: ActionState::Unconfirmed,
                ..
            })
        ));
        // 再开一次（再换代次）仍是 unconfirmed（幂等，不回退成可派发）
        let again = NativeActionStore::with_epoch(device.clone(), "epoch-restart-2");
        assert_eq!(
            again.read("dev-1", "act-1").unwrap().state,
            ActionState::Unconfirmed
        );
        assert_eq!(action_rows(&device), 1);
    }

    /// 同代次再构造（同进程内第二个 store / 第二个 hub）不得篡改在飞动作
    #[test]
    fn second_store_in_same_epoch_keeps_dispatching() {
        let (device, actions) = opened();
        actions
            .reserve("dev-1", &target(), &send("act-1", "甲"))
            .unwrap();
        assert!(actions.claim("act-1").unwrap());
        // 同代次：new() 复用进程代次
        let second = NativeActionStore::new(device.clone());
        let view = second.read("dev-1", "act-1").unwrap();
        assert_eq!(view.state, ActionState::Dispatching, "同代次不得改写在飞行");
        assert!(view.reason_code.is_none(), "不得伪造重启理由码");
        // 在飞动作仍可正常落账（结果由真正发起的那个实例给出）
        second
            .finish("act-1", ActionState::Acknowledged, None, None)
            .unwrap();
        assert_eq!(
            second.read("dev-1", "act-1").unwrap().state,
            ActionState::Acknowledged
        );
        assert_eq!(action_rows(&device), 1);
    }

    /// 换代次（真重启）才把在飞动作收敛成 unconfirmed，且此后再不可 claim
    #[test]
    fn new_epoch_rewrites_dispatching_and_never_reclaims() {
        let (device, actions) = opened();
        actions
            .reserve("dev-1", &target(), &send("act-1", "甲"))
            .unwrap();
        assert!(actions.claim("act-1").unwrap());
        let restarted = NativeActionStore::with_epoch(device.clone(), "epoch-after-crash");
        assert_eq!(
            restarted.read("dev-1", "act-1").unwrap().state,
            ActionState::Unconfirmed
        );
        assert!(
            !restarted.claim("act-1").unwrap(),
            "重启后绝不给第二次写出权"
        );
        // 同代次的第三次构造仍是 unconfirmed（不因再构造而回到可派发）
        let again = NativeActionStore::with_epoch(device.clone(), "epoch-after-crash");
        assert_eq!(
            again.read("dev-1", "act-1").unwrap().state,
            ActionState::Unconfirmed
        );
        assert!(!again.claim("act-1").unwrap());
    }

    /// 放弃 = 墓碑：不再可 claim、reserve 仍回既有动作、也不再阻挡路由交出
    #[test]
    fn abandoned_action_is_a_tombstone() {
        let (device, actions) = opened();
        assert!(actions.try_claim_route(&target(), true).unwrap());
        let command = send("act-1", "甲");
        actions.reserve("dev-1", &target(), &command).unwrap();
        assert!(actions.claim("act-1").unwrap());
        // 回滚到「放弃」只能发生在从未派发时：这里换一个从未 claim 的动作
        actions
            .reserve("dev-1", &target(), &send("act-2", "乙"))
            .unwrap();
        actions
            .finish("act-2", ActionState::NotSent, None, Some("user_abandoned"))
            .unwrap();
        assert!(
            !actions.claim("act-2").unwrap(),
            "已放弃的 send 动作不得再被派发（无占用行可依赖，靠墓碑）"
        );
        // reserve 回既有动作且能凭 reason_code 与「从未尝试」区分
        match actions
            .reserve("dev-1", &target(), &send("act-2", "乙"))
            .unwrap()
        {
            ReserveOutcome::Existing(view) => {
                assert_eq!(view.state, ActionState::NotSent);
                assert_eq!(view.reason_code.as_deref(), Some("user_abandoned"));
            }
            other => panic!("已登记动作不得被当成新动作: {other:?}"),
        }
        // 墓碑不再算未决：act-1 收尾后路由可以交出
        actions
            .finish("act-1", ActionState::Confirmed, Some("msg_act-1"), None)
            .unwrap();
        assert!(
            actions.disengage_route("ses_1").unwrap(),
            "墓碑不得永久阻挡关闭验证开关"
        );
        assert_eq!(route_row(&device, "ses_1").unwrap().route_state, "draining");
    }

    /// 未决判定必须按会话隔离：别的会话的未派发行/墓碑不得挡住本会话
    #[test]
    fn unresolved_actions_are_scoped_per_session() {
        let (device, actions) = opened();
        assert!(actions
            .try_claim_route(&target_of("ses_A", runtime(4242)), true)
            .unwrap());
        assert!(actions
            .try_claim_route(&target_of("ses_B", runtime(4242)), true)
            .unwrap());
        // B 有一个「已登记、未派发、未放弃」的行 = B 自己的未决
        actions
            .reserve(
                "dev-1",
                &target_of("ses_B", runtime(4242)),
                &send_in("ses_B", "act-b", "乙"),
            )
            .unwrap();
        // 方向一：B 的未决不得挡住 A（关闭开关 + 换代际认领都不受影响）
        assert!(
            actions.disengage_route("ses_A").unwrap(),
            "别的会话的未决行不得挡住本会话关闭开关"
        );
        // A 已 draining：**换代际的隐式认领同样不得复活它**（评审 R-2 的换代际分支）。
        // 「服务重启一次就把用户显式退回撤销」与「刷新一次就撤销」是同一种错误。
        assert!(
            !actions
                .try_claim_route(&target_of("ses_A", runtime(9999)), true)
                .unwrap(),
            "换代际（服务重启）时隐式认领也必须拒绝复活 draining"
        );
        assert_eq!(
            route_row(&device, "ses_A").unwrap().route_state,
            "draining",
            "被拒时零变更"
        );
        // 显式再进入才允许（与「别的会话未决不挡本会话」是两件事，一并断言）
        assert!(
            actions
                .try_claim_route_with(&target_of("ses_A", runtime(9999)), true, true)
                .unwrap(),
            "别的会话的未决行不得挡住本会话显式再进入（换代际）"
        );
        // 自己的未决仍挡住自己
        assert!(!actions.disengage_route("ses_B").unwrap());
        assert_eq!(route_row(&device, "ses_B").unwrap().route_state, "active");
        // 方向二：反过来（A 有未决、B 干净）
        actions
            .finish("act-b", ActionState::NotSent, None, Some("user_abandoned"))
            .unwrap();
        assert!(actions.disengage_route("ses_B").unwrap(), "墓碑不再算未决");
        assert!(actions
            .try_claim_route(&target_of("ses_A", runtime(4242)), true)
            .unwrap());
        // B 已 draining：隐式认领不得复活（评审 R-2），必须走显式再进入
        assert!(
            !actions
                .try_claim_route(&target_of("ses_B", runtime(4242)), true)
                .unwrap(),
            "draining 只能由显式再进入拉回 active"
        );
        assert!(actions
            .try_claim_route_with(&target_of("ses_B", runtime(4242)), true, true)
            .unwrap());
        actions
            .reserve(
                "dev-1",
                &target_of("ses_A", runtime(4242)),
                &send_in("ses_A", "act-a", "甲"),
            )
            .unwrap();
        assert!(
            !actions.disengage_route("ses_A").unwrap(),
            "自己的未决仍挡住自己"
        );
        assert!(
            actions.disengage_route("ses_B").unwrap(),
            "A 的未决不得挡住 B"
        );
    }

    /// 无理由的「放弃」被拒：不落碑，也就不会变成既不可派发又算未决的怪状态
    #[test]
    fn not_sent_without_reason_is_refused() {
        let (_device, actions) = opened();
        actions
            .reserve("dev-1", &target(), &send("act-1", "甲"))
            .unwrap();
        assert_eq!(
            actions
                .finish("act-1", ActionState::NotSent, None, None)
                .unwrap_err()
                .code,
            action_codes::STATE,
            "没有理由就不是墓碑，必须拒绝"
        );
        let view = actions.read("dev-1", "act-1").unwrap();
        assert_eq!(view.state, ActionState::NotSent);
        assert!(view.reason_code.is_none(), "被拒的调用不得写入任何理由码");
        assert!(actions.claim("act-1").unwrap(), "未落碑的动作仍可派发");
        // 带理由才落碑
        actions
            .reserve("dev-1", &target(), &send("act-2", "乙"))
            .unwrap();
        actions
            .finish("act-2", ActionState::NotSent, None, Some("user_abandoned"))
            .unwrap();
        assert_eq!(
            actions
                .read("dev-1", "act-2")
                .unwrap()
                .reason_code
                .as_deref(),
            Some("user_abandoned")
        );
        assert!(!actions.claim("act-2").unwrap(), "墓碑不可再派发");
    }

    /// 崩溃后 unconfirmed 的占用：只有带证据词才能收敛并释放，否则一律拒绝
    #[test]
    fn unconfirmed_releases_occupancy_only_with_evidence() {
        let (device, actions) = opened();
        actions
            .reserve("dev-1", &target(), &answer("act-a", "form-1"))
            .unwrap();
        assert!(actions.claim("act-a").unwrap());
        let crashed = NativeActionStore::with_epoch(device.clone(), "epoch-after-crash");
        assert_eq!(
            crashed.read("dev-1", "act-a").unwrap().state,
            ActionState::Unconfirmed
        );
        // 无证据：拒绝转移，占用仍在
        assert_eq!(
            crashed
                .finish("act-a", ActionState::NotAccepted, None, Some("http_400"))
                .unwrap_err()
                .code,
            action_codes::STATE
        );
        assert_eq!(
            crashed
                .reserve("dev-1", &target(), &answer("act-b", "form-1"))
                .unwrap(),
            ReserveOutcome::Conflict,
            "没有证据就不得释放占用"
        );
        // 带证据（权威回读证明整份未生效）：终态化并释放，新 actionId 可答同一表单
        crashed
            .finish(
                "act-a",
                ActionState::NotAccepted,
                None,
                Some(NOT_ACCEPTED_EVIDENCE),
            )
            .unwrap();
        assert_eq!(
            crashed
                .reserve("dev-1", &target(), &answer("act-b", "form-1"))
                .unwrap(),
            ReserveOutcome::New
        );
    }

    /// 旧 inject_queue 查询失败不得被当成「没有未决」：认领必须报错，不能 fail-open
    #[test]
    fn legacy_queue_read_failure_is_not_treated_as_quiescent() {
        let (device, actions) = opened();
        device.with(|c| c.execute("DROP TABLE inject_queue", []).unwrap());
        let err = actions.try_claim_route(&target(), true).unwrap_err();
        assert_eq!(err.code, action_codes::STORE_FAILED);
        assert!(
            route_row(&device, "ses_1").is_none(),
            "查不成就不能写路由行"
        );
        assert!(!actions.owns_session("ses_1").unwrap());
    }

    /// 同一待答表单恰好一个活占用：第二个请求冲突，放弃后可换新 actionId 重试
    #[test]
    fn one_pending_form_has_one_live_claim() {
        let (device, actions) = opened();
        assert_eq!(
            actions
                .reserve("dev-1", &target(), &answer("act-a", "form-1"))
                .unwrap(),
            ReserveOutcome::New
        );
        assert_eq!(
            actions
                .reserve("dev-2", &target(), &answer("act-b", "form-1"))
                .unwrap(),
            ReserveOutcome::Conflict,
            "同一待答表单只能有一个活占用（跨设备同样）"
        );
        assert_eq!(
            actions
                .reserve("dev-2", &target(), &answer("act-c", "form-2"))
                .unwrap(),
            ReserveOutcome::New,
            "不同表单互不阻塞"
        );
        // 从未派发即放弃：释放占用；旧动作仍是终态墓碑，用户重试必须新 actionId
        actions
            .finish("act-a", ActionState::NotSent, None, Some("form_changed"))
            .unwrap();
        assert!(
            !actions.claim("act-a").unwrap(),
            "已放弃的动作不得重新取得写出权（否则会与后来的动作双投）"
        );
        assert_eq!(
            actions
                .reserve("dev-2", &target(), &answer("act-d", "form-1"))
                .unwrap(),
            ReserveOutcome::New,
            "释放后新 actionId 可以认领同一表单"
        );
        assert_eq!(action_rows(&device), 3, "冲突请求不落库");
    }

    /// 账本只存指纹与结果元数据：正文、答案、绑定令牌原文绝不落库
    #[test]
    fn stores_only_fingerprints_not_bodies() {
        let (device, actions) = opened();
        let chat = send("act-1", "机密正文XYZ");
        actions.reserve("dev-1", &target(), &chat).unwrap();
        let mut answers = Answers::new();
        answers.insert("q1".to_string(), DraftAnswer::Text("答案机密ABC".into()));
        let form = NativeCommand {
            action_id: "act-2".into(),
            session_id: "ses_1".into(),
            binding_token: "bt-1".into(),
            operation: NativeOperation::Answer(AnswerPayload {
                form_id: "form-1".into(),
                context_token: "ct-1".into(),
                answers,
            }),
        };
        actions.reserve("dev-1", &target(), &form).unwrap();
        assert!(actions.claim("act-1").unwrap());
        let fingerprint = payload_sha256(&chat);
        assert_eq!(fingerprint.len(), 64, "指纹是 sha256 hex");
        assert!(fingerprint.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(fingerprint, payload_sha256(&form), "不同意图指纹不同");
        for table in [
            "opencode_native_actions",
            "opencode_native_claims",
            "opencode_native_routes",
        ] {
            let dump = table_dump(&device, table);
            assert!(!dump.contains("机密正文XYZ"), "{table} 不得存消息正文");
            assert!(!dump.contains("答案机密ABC"), "{table} 不得存表单答案");
            assert!(!dump.contains("bt-1"), "{table} 不得存绑定令牌");
        }
        let stored = device.with(|c| dao::get_action_conn(c, "act-1").unwrap().unwrap());
        assert_eq!(
            stored.payload_sha256, fingerprint,
            "落库的是客户端原始意图指纹"
        );
        assert!(stored.native_id.is_none() && stored.request_key.is_none());
    }

    /// 旧 inject_queue 还有未决写、或调用方无法确认静止 → 不得认领路由
    #[test]
    fn pending_or_uncertain_legacy_write_prevents_route_claim() {
        let (device, actions) = opened();
        let queued = device.with(|c| {
            inject_queue::enqueue_conn(c, "ses_1", "opencode", "dev-1", "iPhone", "旧消息", 1000)
        });
        assert!(
            !actions.try_claim_route(&target(), true).unwrap(),
            "旧待发未消除时不得认领（即使调用方自称静止）"
        );
        assert!(!actions.owns_session("ses_1").unwrap());
        device.with(|c| inject_queue::mark_sent_conn(c, queued, 1100));
        assert!(
            !actions.try_claim_route(&target(), false).unwrap(),
            "调用方不能证明旧动作静止时不得认领"
        );
        assert!(actions.try_claim_route(&target(), true).unwrap());
        assert!(actions.owns_session("ses_1").unwrap());
        assert!(
            actions.try_claim_route(&target(), true).unwrap(),
            "同代际重复认领幂等"
        );
        assert!(!actions.owns_session("ses_2").unwrap(), "别的会话没有归属");
        // 失败行（failed_reason 非空）不是未决：不再阻挡认领
        let (device2, actions2) = opened();
        device2.with(|c| {
            let id = inject_queue::enqueue_conn(
                c,
                "ses_1",
                "opencode",
                "dev-1",
                "iPhone",
                "旧消息",
                1000,
            );
            inject_queue::mark_failed_conn(c, id, "device_offline");
        });
        assert!(actions2.try_claim_route(&target(), true).unwrap());
    }

    /// NotAccepted 没有「整份未生效」证据：不记该状态、不释放占用
    #[test]
    fn not_accepted_without_evidence_does_not_release_occupancy() {
        let (_device, actions) = opened();
        actions
            .reserve("dev-1", &target(), &answer("act-a", "form-1"))
            .unwrap();
        assert!(actions.claim("act-a").unwrap());
        let err = actions
            .finish("act-a", ActionState::NotAccepted, None, Some("http_400"))
            .unwrap_err();
        assert_eq!(err.code, action_codes::STATE);
        assert_eq!(
            actions.read("dev-1", "act-a").unwrap().state,
            ActionState::Dispatching,
            "证据不足时状态不得改写成 not_accepted"
        );
        assert_eq!(
            actions
                .reserve("dev-2", &target(), &answer("act-b", "form-1"))
                .unwrap(),
            ReserveOutcome::Conflict,
            "占用仍被持有"
        );
        // 带证据才允许：not_accepted 是终态，且释放占用
        actions
            .finish(
                "act-a",
                ActionState::NotAccepted,
                None,
                Some(NOT_ACCEPTED_EVIDENCE),
            )
            .unwrap();
        assert_eq!(
            actions
                .reserve("dev-2", &target(), &answer("act-b", "form-1"))
                .unwrap(),
            ReserveOutcome::New
        );
    }

    /// 在飞登记必须引用计数：同一 actionId 的并发派发（reserve Existing 自愈）
    /// 可同时登记；claim 失败者的 Drop 不得删掉胜者仍在飞的登记（否则 force 又能
    /// 作废仍在执行的写——G7 的别名洞）。
    #[tokio::test]
    async fn duplicate_in_flight_guards_keep_registration_until_last_drop() {
        let rig = boot(World::new()).await;
        let first = rig.hub.begin_in_flight("act_alias");
        let second = rig.hub.begin_in_flight("act_alias");
        drop(first);
        assert_eq!(
            rig.hub.active_in_flight(),
            vec!["act_alias".to_string()],
            "一个 guard 释放后，另一个仍在飞，登记必须保留"
        );
        drop(second);
        assert!(
            rig.hub.active_in_flight().is_empty(),
            "最后一个 guard 释放才摘除"
        );
    }

    /// claim 之前的在飞登记也必须挡住 force：真实 future 活跃事实先于账本状态。
    /// 这里手工在 reserve 之后、claim 之前登记，验证「force 尊重 pre-claim 登记」；
    /// **不**声称覆盖 snapshot→txn 之间的 TOCTOU 交错（那由 begin→claim 的代码次序
    /// 与 force 在 DB 临界区内取快照共同保证，现有 seam 无法确定性插入 claim）。
    #[tokio::test]
    async fn force_respects_in_flight_registration_before_claim() {
        let mut world = World::new();
        world.forms = vec![form_entry()];
        let rig = boot(world).await;
        claim_route(&rig);
        let first = rig.hub.view(&rig.card).await.expect("首轮投影");
        let (binding, token) = binding_and_form_token(&first);
        let command = answer_cmd("act_before_claim", &binding, &token, q0_blue());
        let store = rig.hub.action_store();
        assert!(matches!(
            store
                .reserve(DEVICE_ID, &bound_target(&rig.project), &command)
                .expect("reserve"),
            ReserveOutcome::New
        ));

        let guard = rig.hub.begin_in_flight("act_before_claim");
        rig.peer.restart();
        let (returned, abandoned) = rig
            .hub
            .force_return_to_legacy(SESSION_ID)
            .await
            .expect("force 不得报错");
        assert!(
            !returned && abandoned == 0,
            "claim 之前的在飞登记必须让 force 拒绝：returned={returned} abandoned={abandoned}"
        );
        assert_eq!(
            rig.hub
                .action(DEVICE_ID, "act_before_claim")
                .await
                .expect("读动作")
                .state,
            ActionState::NotSent,
            "未 claim 的行保持 not_sent，不得被写成墓碑"
        );
        assert!(
            rig.hub
                .action(DEVICE_ID, "act_before_claim")
                .await
                .expect("读动作")
                .reason_code
                .is_none(),
            "在飞保护拒绝时不得落下作废碑文"
        );
        drop(guard);
        // 真正的未来结束后，死代际行才允许被显式作废（用户决定）。
        let (returned, abandoned) = rig
            .hub
            .force_return_to_legacy(SESSION_ID)
            .await
            .expect("force");
        assert!(
            returned && abandoned == 1,
            "在飞登记释放后，死代际行应可被显式作废"
        );
    }

    /// 组合反例（收口 §3）：原写仍在飞 → verify 把 Dispatching 收敛为 Unconfirmed
    /// → 服务换代 → force。**活跃写不能因展示状态被改写而被作废/交回**；
    /// 原写结束后再按实际结果收敛。
    #[tokio::test]
    async fn force_refuses_while_a_verified_write_is_still_in_flight() {
        let world = World::new();
        let mut world = world;
        world.forms = vec![form_entry()];
        let rig = boot(world).await;
        claim_route(&rig);
        let first = rig.hub.view(&rig.card).await.expect("首轮投影");
        let (binding, token) = binding_and_form_token(&first);
        let command = answer_cmd("act_inflight_force", &binding, &token, q0_blue());

        let (entered, release) = rig.peer.arm_form_reply_hold();
        let hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let task = tokio::spawn(async move { hub.execute(&actor(), &card, command).await });

        // 等 peer 真正收到写请求（挂起在应用状态之前）。
        for _ in 0..400 {
            if entered.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(entered.load(Ordering::SeqCst), "假 peer 必须收到在飞写");

        // 在飞期间只读核对：表单仍 pending ⇒ 收敛为 Unconfirmed（不是终态）。
        let verified = rig
            .hub
            .verify(DEVICE_ID, "act_inflight_force")
            .await
            .expect("在飞核对不得失败");
        assert_eq!(verified.state, ActionState::Unconfirmed);

        // 同一进程、服务换代（同端口新 id/pid）：该行看起来「代际已不可核对」。
        rig.peer.restart();
        assert_eq!(
            rig.hub
                .action_store()
                .route_state(SESSION_ID)
                .expect("路由"),
            Some("active".to_string()),
            "前置：路由仍归原生"
        );

        // 预读与 force 同口径：在飞行不得被宣称为「可作废」。
        assert_eq!(
            rig.hub.unverifiable_actions(SESSION_ID).expect("预读"),
            0,
            "409 回执不得把在飞行算作可作废"
        );

        // 关键断言：原写仍可能执行，force 不得作废它，也不得交回旧路径。
        let (returned, abandoned) = rig
            .hub
            .force_return_to_legacy(SESSION_ID)
            .await
            .expect("force 不得报错");
        assert!(
            !returned && abandoned == 0,
            "活跃写被作废/交回：returned={returned} abandoned={abandoned}"
        );
        assert_eq!(
            rig.hub
                .action(DEVICE_ID, "act_inflight_force")
                .await
                .expect("读动作")
                .state,
            ActionState::Unconfirmed,
            "在飞行不得被写成 not_sent 墓碑"
        );
        assert!(
            rig.hub
                .action_store()
                .owns_session(SESSION_ID)
                .expect("归属"),
            "交回不得发生"
        );

        // 释放原写：只允许一次真实写，结论按实际回读（换代后仍 Unknown）。
        release.notify_one();
        let finished = task.await.expect("派发任务").expect("派发不得失败");
        assert_eq!(finished.state, ActionState::Unconfirmed);
        assert_eq!(
            rig.peer.count(
                "POST",
                &format!("/api/session/{SESSION_ID}/form/{FORM_ID}/reply")
            ),
            1,
            "核实不得重发原写"
        );
        assert!(
            rig.hub
                .action_store()
                .owns_session(SESSION_ID)
                .expect("归属"),
            "释放后路由仍归原生（force 未交回）"
        );
    }

    /// 关闭验证开关：仍有未决 native 动作的归属不得被交出
    #[test]
    fn route_with_unresolved_actions_survives_switch_off() {
        let (device, actions) = opened();
        assert!(actions.try_claim_route(&target(), true).unwrap());
        actions
            .reserve("dev-1", &target(), &send("act-1", "甲"))
            .unwrap();
        assert!(actions.claim("act-1").unwrap());
        assert!(
            !actions.disengage_route("ses_1").unwrap(),
            "在飞动作未收尾时不得关闭归属"
        );
        assert!(actions.owns_session("ses_1").unwrap());
        assert!(route_row(&device, "ses_1").is_some(), "路由行绝不被删除");
        // 结果不明（unconfirmed）同样算未决
        actions
            .finish("act-1", ActionState::Unconfirmed, None, Some("timeout"))
            .unwrap();
        assert!(!actions.disengage_route("ses_1").unwrap());
        // 只读核对确认后才允许转入 draining（归属行保留，可再次启用）
        actions
            .finish("act-1", ActionState::Confirmed, Some("msg_act-1"), None)
            .unwrap();
        assert!(actions.disengage_route("ses_1").unwrap());
        assert!(
            !actions.owns_session("ses_1").unwrap(),
            "关闭后不再拥有写入入口"
        );
        assert_eq!(
            route_row(&device, "ses_1").unwrap().route_state,
            "draining",
            "归属只降级不删除"
        );
        assert!(
            !actions.try_claim_route(&target(), true).unwrap(),
            "隐式认领（GET 视图）不得复活 draining —— 否则一次刷新就撤销了用户的退回"
        );
        assert_eq!(
            route_row(&device, "ses_1").unwrap().route_state,
            "draining",
            "隐式认领被拒时零变更"
        );
        assert!(
            actions.try_claim_route_with(&target(), true, true).unwrap(),
            "显式重新进入（POST /opencode/route/native）可再次启用"
        );
        assert_eq!(route_row(&device, "ses_1").unwrap().route_state, "active");
    }

    /// 同端口换代际：旧代际还有未决动作时不得夺取路由，也不迁移旧动作
    #[test]
    fn route_claim_refuses_runtime_switch_with_unresolved_actions() {
        let (device, actions) = opened();
        assert!(actions.try_claim_route(&target(), true).unwrap());
        actions
            .reserve("dev-1", &target(), &send("act-1", "甲"))
            .unwrap();
        assert!(actions.claim("act-1").unwrap());
        let next = target_on(runtime(9999));
        assert!(
            !actions.try_claim_route(&next, true).unwrap(),
            "旧代际仍有未决动作：不得换绑路由"
        );
        let kept = route_row(&device, "ses_1").unwrap();
        assert_eq!(kept.runtime_pid, 4242, "路由行保持旧代际，不迁移");
        assert_eq!(
            actions.read("dev-1", "act-1").unwrap().state,
            ActionState::Dispatching,
            "不迁移未知旧动作"
        );
        actions
            .finish("act-1", ActionState::Confirmed, Some("msg_act-1"), None)
            .unwrap();
        assert!(actions.try_claim_route(&next, true).unwrap());
        assert_eq!(route_row(&device, "ses_1").unwrap().runtime_pid, 9999);
    }

    /// Confirmed 必须带原生消息 id（否则只能记 unconfirmed）
    #[test]
    fn confirmed_requires_native_id() {
        let (_device, actions) = opened();
        actions
            .reserve("dev-1", &target(), &send("act-1", "甲"))
            .unwrap();
        assert!(actions.claim("act-1").unwrap());
        let err = actions
            .finish("act-1", ActionState::Confirmed, None, None)
            .unwrap_err();
        assert_eq!(err.code, action_codes::STATE);
        assert_eq!(
            actions.read("dev-1", "act-1").unwrap().state,
            ActionState::Dispatching
        );
        actions
            .finish("act-1", ActionState::Confirmed, Some("msg_act-1"), None)
            .unwrap();
        let view = actions.read("dev-1", "act-1").unwrap();
        assert_eq!(view.state, ActionState::Confirmed);
        assert_eq!(view.native_id.as_deref(), Some("msg_act-1"));
    }

    /// 终态不可改写；未取得写出权不得落账
    #[test]
    fn terminal_state_is_not_rewritten() {
        let (_device, actions) = opened();
        actions
            .reserve("dev-1", &target(), &send("act-1", "甲"))
            .unwrap();
        assert_eq!(
            actions
                .finish("act-1", ActionState::Settled, None, Some("form_gone"))
                .unwrap_err()
                .code,
            action_codes::STATE,
            "未 claim 就落终态必须被拒"
        );
        assert!(actions.claim("act-1").unwrap());
        actions
            .finish("act-1", ActionState::Settled, None, Some("form_gone"))
            .unwrap();
        assert_eq!(
            actions
                .finish("act-1", ActionState::Confirmed, Some("msg_act-1"), None)
                .unwrap_err()
                .code,
            action_codes::STATE
        );
        assert_eq!(
            actions.read("dev-1", "act-1").unwrap().state,
            ActionState::Settled
        );
    }

    async fn queued_first_send_survives_claim_failure(trigger: &str, action_id: &str) {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let path = controlled_attachment(&rig.project, "claim-failure.txt", 0);
        std::fs::write(&path, b"frozen bytes").unwrap();
        rig.hub
            .store()
            .with(|conn| conn.execute_batch(trigger).unwrap());

        let result = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd_with_attachments(
                    action_id,
                    &binding,
                    "frozen message",
                    vec![path.clone()],
                ),
            )
            .await;
        let ledger = rig.hub.action(DEVICE_ID, action_id).await.unwrap();
        assert_eq!(
            ledger.state,
            ActionState::Queued,
            "入队后的失败不得销毁已确认意图"
        );
        let frozen = rig.hub.store().with(|conn| {
            let json: String = conn
                .query_row(
                    "SELECT command_json FROM opencode_outbox WHERE action_id=?1",
                    [action_id],
                    |row| row.get(0),
                )
                .unwrap();
            serde_json::from_str::<crate::services::opencode_native::outbox::FrozenSend>(&json)
                .unwrap()
        });
        assert_eq!(frozen.send.text, "frozen message");
        assert_eq!(frozen.attachment_paths.len(), 1);
        assert_ne!(frozen.attachment_paths[0], path);
        assert_eq!(
            std::fs::read(&frozen.attachment_paths[0]).unwrap(),
            b"frozen bytes"
        );
        assert_eq!(rig.peer.prompt_posts(), 0);
        assert_eq!(
            result.expect("已入队的意图必须返回真实账本状态").state,
            ActionState::Queued
        );
    }

    #[tokio::test]
    async fn first_enqueue_claim_transaction_error_returns_queued_with_frozen_payload() {
        queued_first_send_survives_claim_failure(
            "CREATE TRIGGER fail_send_claim BEFORE UPDATE OF state ON opencode_native_actions WHEN OLD.state='queued' AND NEW.state='dispatching' BEGIN SELECT RAISE(ABORT, 'claim failed'); END;",
            "queued_claim_error",
        ).await;
    }

    #[tokio::test]
    async fn first_enqueue_route_loss_returns_queued_with_frozen_payload() {
        queued_first_send_survives_claim_failure(
            "CREATE TRIGGER drain_after_enqueue AFTER INSERT ON opencode_outbox BEGIN UPDATE opencode_native_routes SET route_state='draining' WHERE session_id=NEW.session_id; END;",
            "queued_route_loss",
        ).await;
    }

    async fn wait_for_test_barrier(entered: &AtomicBool) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !entered.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("假服务屏障未到达");
    }

    async fn duplicate_answer_recheck_failure_returns_ledger_state(finish_first: bool) {
        let mut world = World::new();
        let mut form = form_entry();
        form.reply = Reply::Conflict;
        form.detail_fails_after_reply = true;
        world.forms.push(form);
        let rig = boot(world).await;
        claim_route(&rig);
        let view = rig.hub.view(&rig.card).await.unwrap();
        let (binding, token) = binding_and_form_token(&view);
        let command = answer_cmd("duplicate_recheck", &binding, &token, q0_blue());
        let snapshot = rig.hub.dispatch_snapshot(&rig.card).await.unwrap();
        assert!(matches!(
            rig.hub
                .action_store()
                .reserve(DEVICE_ID, &snapshot.target, &command)
                .unwrap(),
            ReserveOutcome::New
        ));

        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(Notify::new());
        *rig.peer.shared.hold_form_detail.lock().unwrap() = Some(FormReplyHold {
            entered: entered.clone(),
            release: release.clone(),
        });
        let hub = rig.hub.clone();
        let first_command = command.clone();
        let first = tokio::spawn(async move {
            let intent = intent_of(&first_command).unwrap();
            let mut timing = crate::services::opencode_native::budget::ActionTiming::start(
                first_command.action_id.as_str(),
                SESSION_ID,
                operation_word(&first_command.operation),
            )
            .begin();
            hub.dispatch(
                &actor(),
                &snapshot,
                first_command,
                intent,
                None,
                0,
                &mut timing,
            )
            .await
        });
        wait_for_test_barrier(&entered).await;

        let (write_entered, write_release) = rig.peer.arm_form_reply_hold();
        let hub = rig.hub.clone();
        let card = rig.card.clone();
        let mut second = Some(tokio::spawn(async move {
            hub.execute(&actor(), &card, command).await
        }));
        wait_for_test_barrier(&write_entered).await;
        if finish_first {
            write_release.notify_one();
            assert_eq!(
                second.take().unwrap().await.unwrap().unwrap().state,
                ActionState::Unconfirmed
            );
        }
        rig.peer.set(|world| world.forms.clear());
        release.notify_one();
        let result = first.await.unwrap();
        let ledger = rig
            .hub
            .action(DEVICE_ID, "duplicate_recheck")
            .await
            .unwrap();
        let expected = if finish_first {
            ActionState::Unconfirmed
        } else {
            ActionState::Dispatching
        };
        assert_eq!(ledger.state, expected);
        assert_eq!(rig.peer.form_reply_posts(), 1);
        if !finish_first {
            write_release.notify_one();
            let _ = second.take().unwrap().await;
        }
        assert_eq!(
            result
                .expect("并发复核失败不得掩盖已取得写权的账本状态")
                .state,
            expected
        );
    }

    #[tokio::test]
    async fn duplicate_answer_recheck_failure_returns_dispatching() {
        duplicate_answer_recheck_failure_returns_ledger_state(false).await;
    }

    #[tokio::test]
    async fn duplicate_answer_recheck_failure_returns_unconfirmed() {
        duplicate_answer_recheck_failure_returns_ledger_state(true).await;
    }

    fn arm_info_hold(rig: &Rig) -> (Arc<AtomicBool>, Arc<Notify>) {
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(Notify::new());
        *rig.peer.shared.hold_info.lock().unwrap() = Some(FormReplyHold {
            entered: entered.clone(),
            release: release.clone(),
        });
        (entered, release)
    }

    async fn entry_snapshot_failure_returns_concurrent_claim(preexisting: bool) {
        let mut world = World::new();
        let mut form = form_entry();
        form.reply = Reply::Conflict;
        form.detail_fails_after_reply = true;
        world.forms.push(form);
        let rig = boot(world).await;
        claim_route(&rig);
        let view = rig.hub.view(&rig.card).await.unwrap();
        let (binding, token) = binding_and_form_token(&view);
        let command = answer_cmd("entry_race", &binding, &token, q0_blue());
        let snapshot = rig.hub.dispatch_snapshot(&rig.card).await.unwrap();
        if preexisting {
            assert!(matches!(
                rig.hub
                    .action_store()
                    .reserve(DEVICE_ID, &snapshot.target, &command)
                    .unwrap(),
                ReserveOutcome::New
            ));
        }
        let (entered, release) = arm_info_hold(&rig);
        let hub = rig.hub.clone();
        let card = rig.card.clone();
        let first_command = command.clone();
        let first = tokio::spawn(async move { hub.execute(&actor(), &card, first_command).await });
        wait_for_test_barrier(&entered).await;
        if !preexisting {
            assert!(matches!(
                rig.hub
                    .action_store()
                    .reserve(DEVICE_ID, &snapshot.target, &command)
                    .unwrap(),
                ReserveOutcome::New
            ));
        }
        // 模拟独立派发者已进入内部派发阶段，不再从 execute 入口重复取得同 ID 锁。
        let (write_entered, write_release) = rig.peer.arm_form_reply_hold();
        let hub = rig.hub.clone();
        let other_command = command.clone();
        let second = tokio::spawn(async move {
            let intent = intent_of(&other_command).unwrap();
            let mut timing = crate::services::opencode_native::budget::ActionTiming::start(
                other_command.action_id.as_str(),
                SESSION_ID,
                operation_word(&other_command.operation),
            )
            .begin();
            hub.dispatch(
                &actor(),
                &snapshot,
                other_command,
                intent,
                None,
                0,
                &mut timing,
            )
            .await
        });
        wait_for_test_barrier(&write_entered).await;
        assert_eq!(
            rig.hub.action(DEVICE_ID, "entry_race").await.unwrap().state,
            ActionState::Dispatching
        );
        assert!(
            rig.hub.action("other-device", "entry_race").await.is_err(),
            "非属主不得读取动作"
        );
        rig.peer.set(|world| world.info_pid = Some(PID + 1));
        release.notify_one();
        let result = first.await.unwrap();
        rig.peer.set(|world| world.info_pid = None);
        write_release.notify_one();
        let dispatched = second.await.unwrap().unwrap();
        assert_eq!(
            dispatched.state,
            ActionState::Unconfirmed,
            "回读失败必须保留未知状态"
        );
        assert_eq!(rig.peer.form_reply_posts(), 1);
        assert_eq!(
            result.expect("入口复核失败须回读并发动作的真实状态").state,
            ActionState::Dispatching
        );
        std::fs::remove_file(&rig.peer.shared.state_file).unwrap();
        assert_eq!(
            rig.hub
                .execute(&actor(), &rig.card, command)
                .await
                .unwrap()
                .state,
            ActionState::Unconfirmed
        );
        assert_eq!(rig.peer.form_reply_posts(), 1, "未知状态重读不得重复写");
    }

    #[tokio::test]
    async fn entry_snapshot_failure_returns_claim_after_initial_absence() {
        entry_snapshot_failure_returns_concurrent_claim(false).await;
    }

    #[tokio::test]
    async fn entry_snapshot_failure_returns_claim_after_initial_not_sent() {
        entry_snapshot_failure_returns_concurrent_claim(true).await;
    }

    async fn entry_rejection_cannot_precede_same_action_write(preexisting: bool) {
        let mut world = World::new();
        world.forms.push(form_entry());
        let rig = boot(world).await;
        claim_route(&rig);
        let view = rig.hub.view(&rig.card).await.unwrap();
        let (binding, token) = binding_and_form_token(&view);
        let command = answer_cmd("entry_reverse_race", &binding, &token, q0_blue());
        if preexisting {
            assert!(matches!(
                rig.hub
                    .action_store()
                    .reserve(DEVICE_ID, &bound_target(&rig.project), &command)
                    .unwrap(),
                ReserveOutcome::New
            ));
        }

        // A 已读过初始账本，停在 snapshot 的 /api/info，尚未取得写权。
        let (entered, release) = arm_info_hold(&rig);
        let hub = rig.hub.clone();
        let card = rig.card.clone();
        let first_command = command.clone();
        let first = tokio::spawn(async move { hub.execute(&actor(), &card, first_command).await });
        wait_for_test_barrier(&entered).await;
        let initial = rig
            .hub
            .store()
            .with(|conn| dao::get_action_conn(conn, &command.action_id).unwrap());
        assert_eq!(
            initial.as_ref().map(|row| row.state),
            preexisting.then_some(ActionState::NotSent)
        );
        assert_eq!(rig.peer.form_reply_posts(), 0);

        // B 的执行已经启动；A 持锁时 B 应等待，而不是读到错误的服务身份后提前拒绝。
        rig.peer.set(|world| world.info_pid = Some(PID + 1));
        let hub = rig.hub.clone();
        let card = rig.card.clone();
        let started = Arc::new(Notify::new());
        let task_started = started.clone();
        let mut second = tokio::spawn(async move {
            task_started.notify_one();
            hub.execute(&actor(), &card, command).await
        });
        started.notified().await;
        let early = tokio::time::timeout(std::time::Duration::from_millis(500), &mut second).await;
        let writes_before_release = rig.peer.form_reply_posts();
        rig.peer.set(|world| world.info_pid = None);
        release.notify_one();
        let first_result = tokio::time::timeout(std::time::Duration::from_secs(10), first)
            .await
            .expect("A 未完成")
            .unwrap();
        let completed_early = early.is_ok();
        let second_result = match early {
            Ok(result) => result.unwrap(),
            Err(_) => tokio::time::timeout(std::time::Duration::from_secs(10), second)
                .await
                .expect("B 未完成")
                .unwrap(),
        };
        assert!(
            !completed_early,
            "A 尚未释放时 B 不得提前完成: {second_result:?}"
        );
        assert_eq!(writes_before_release, 0, "A 被阻塞时不得写出");
        let first_view = first_result.expect("A 应完成一次写入");
        let second_view = second_result.expect("B 应返回同一账本状态");
        assert_eq!(
            serde_json::to_value(first_view).unwrap(),
            serde_json::to_value(second_view).unwrap()
        );
        assert_eq!(rig.peer.form_reply_posts(), 1, "同一动作只写一次");
    }

    #[tokio::test]
    async fn entry_rejection_before_concurrent_claim_with_existing_not_sent() {
        entry_rejection_cannot_precede_same_action_write(true).await;
    }

    #[tokio::test]
    async fn entry_rejection_before_concurrent_reserve_with_initial_absence() {
        entry_rejection_cannot_precede_same_action_write(false).await;
    }

    async fn m6_owner_isolation_rig(rejected: bool) -> (Rig, NativeCommand) {
        let rig = boot(World::new()).await;
        rig.hub.store().with(|conn| {
            crate::remote::pairing::persist_device(
                conn,
                &crate::remote::pairing::NewDevice {
                    id: "dev-other".into(),
                    name: "other".into(),
                    ua: "other-device-fixture".into(),
                    origin_ip: "127.0.0.1".into(),
                    via: "lan".into(),
                    paired_at: now_ms(),
                },
            )
            .unwrap();
            assert!(crate::remote::pairing::device_valid(
                conn,
                DEVICE_ID,
                now_ms()
            ));
            assert!(crate::remote::pairing::device_valid(
                conn,
                "dev-other",
                now_ms()
            ));
        });
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let command = send_cmd("m6_owner_isolation", &binding, "original intent");
        if rejected {
            rig.peer.set(|world| world.info_pid = Some(PID + 1));
            rig.hub
                .execute(&actor(), &rig.card, command.clone())
                .await
                .expect_err("制造前置拒绝记录");
            rig.peer.set(|world| world.info_pid = None);
            rig.hub.store().with(|conn| {
                assert!(dao::rejection_conn(conn, &command.action_id)
                    .unwrap()
                    .is_some());
            });
        } else {
            assert!(matches!(
                rig.hub
                    .action_store()
                    .reserve(DEVICE_ID, &bound_target(&rig.project), &command)
                    .unwrap(),
                ReserveOutcome::New
            ));
        }
        (rig, command)
    }

    async fn m6_other_device_gets_not_found(rejected: bool, changed_intent: bool) {
        let (rig, mut command) = m6_owner_isolation_rig(rejected).await;
        if changed_intent {
            command = send_cmd(
                &command.action_id,
                &command.binding_token,
                "different intent",
            );
        }
        let other = NativeActor {
            device_id: "dev-other".into(),
            ..actor()
        };
        let writes_before = rig.peer.non_get();
        let result = rig.hub.execute(&other, &rig.card, command).await;
        assert_eq!(
            rig.peer.non_get(),
            writes_before,
            "其他设备不得触发原生写入"
        );
        assert_eq!(
            result.expect_err("其他设备不得取得原属主视图").code,
            action_codes::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn m6_other_device_existing_action_same_intent_is_not_found() {
        m6_other_device_gets_not_found(false, false).await;
    }

    #[tokio::test]
    async fn m6_other_device_existing_action_different_intent_is_not_found() {
        m6_other_device_gets_not_found(false, true).await;
    }

    #[tokio::test]
    async fn m6_other_device_rejected_action_same_intent_is_not_found() {
        m6_other_device_gets_not_found(true, false).await;
    }

    #[tokio::test]
    async fn m6_other_device_rejected_action_different_intent_is_not_found() {
        m6_other_device_gets_not_found(true, true).await;
    }

    async fn m6_owner_different_intent_conflicts(rejected: bool) {
        let (rig, command) = m6_owner_isolation_rig(rejected).await;
        let changed = send_cmd(
            &command.action_id,
            &command.binding_token,
            "different intent",
        );
        let writes_before = rig.peer.non_get();
        let result = rig.hub.execute(&actor(), &rig.card, changed).await;
        assert_eq!(
            rig.peer.non_get(),
            writes_before,
            "冲突请求不得触发原生写入"
        );
        assert_eq!(
            result.expect_err("属主的不同意图必须冲突").code,
            action_codes::CONFLICT
        );
    }

    #[tokio::test]
    async fn m6_owner_existing_action_different_intent_conflicts() {
        m6_owner_different_intent_conflicts(false).await;
    }

    #[tokio::test]
    async fn m6_owner_rejected_action_different_intent_conflicts() {
        m6_owner_different_intent_conflicts(true).await;
    }

    async fn rejected_entry_rig() -> (Rig, NativeCommand) {
        let mut rig = boot(World::new()).await;
        let db = rig._tmp.path().join("entry-rejection.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        crate::database::schema::init(&conn).unwrap();
        crate::database::migration::migrate(&conn).unwrap();
        let store = DeviceStore::Owned(Arc::new(Mutex::new(conn)));
        rig.hub = Arc::new(NativeHub::with_coordinator_sources(
            store,
            DiscoveryInput {
                state_file: rig.peer.shared.state_file.clone(),
                home: rig._tmp.path().join("home"),
            },
            process_fact(),
        ));
        rig.hub.store().with(|conn| {
            for id in [DEVICE_ID, "dev-other"] {
                crate::remote::pairing::persist_device(
                    conn,
                    &crate::remote::pairing::NewDevice {
                        id: id.into(),
                        name: id.into(),
                        ua: "fixture".into(),
                        origin_ip: "127.0.0.1".into(),
                        via: "lan".into(),
                        paired_at: now_ms(),
                    },
                )
                .unwrap();
            }
        });
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let path = controlled_attachment(&rig.project, "entry.txt", 4);
        let command =
            send_cmd_with_attachments("entry_rejected_id", &binding, "message", vec![path]);
        (rig, command)
    }

    fn assert_rejected_entry_has_no_effect(rig: &Rig) {
        let row = rig
            .hub
            .store()
            .with(|conn| dao::get_action_conn(conn, "entry_rejected_id").unwrap());
        assert!(row.is_none(), "前置拒绝的 ID 不得创建 native action");
        assert!(
            !attachment_dir(&rig.project, SESSION_ID)
                .join("outbox")
                .exists(),
            "前置拒绝不得冻结附件"
        );
        assert_eq!(rig.peer.prompt_posts(), 0, "前置拒绝的 ID 不得原生写出");
    }

    async fn reject_entry(rig: &Rig, command: &NativeCommand) {
        rig.peer.set(|world| world.info_pid = Some(PID + 1));
        let result = rig.hub.execute(&actor(), &rig.card, command.clone()).await;
        assert!(result.is_err(), "身份校验失败必须确定拒绝: {result:?}");
        assert_rejected_entry_has_no_effect(rig);
        rig.peer.set(|world| world.info_pid = None);
    }

    // 前置拒绝已经落到临时磁盘数据库；丢弃回执并重开连接模拟客户端丢响应和服务重启。
    async fn rejected_entry_read_after_reopen(verify: bool) {
        let (rig, command) = rejected_entry_rig().await;
        reject_entry(&rig, &command).await;
        let conn =
            rusqlite::Connection::open(rig._tmp.path().join("entry-rejection.sqlite")).unwrap();
        let reopened = NativeHub::with_coordinator_sources(
            DeviceStore::Owned(Arc::new(Mutex::new(conn))),
            DiscoveryInput {
                state_file: rig.peer.shared.state_file.clone(),
                home: rig._tmp.path().join("home"),
            },
            process_fact(),
        );
        let other = if verify {
            reopened.verify("dev-other", &command.action_id).await
        } else {
            reopened.action("dev-other", &command.action_id).await
        };
        assert_eq!(other.unwrap_err().code, action_codes::NOT_FOUND);
        let owner = if verify {
            reopened.verify(DEVICE_ID, &command.action_id).await
        } else {
            reopened.action(DEVICE_ID, &command.action_id).await
        };
        assert_rejected_entry_has_no_effect(&rig);
        let view = owner.expect("原属主必须能按原 actionId 查到已持久化的拒绝");
        assert_eq!(view.action_id, command.action_id);
        assert_eq!(view.state, ActionState::NotSent);
        assert!(view.reason_code.is_some(), "拒绝终态必须带明确理由");
    }

    #[tokio::test]
    async fn rejected_entry_read_returns_owner_terminal_after_reopen() {
        rejected_entry_read_after_reopen(false).await;
    }

    #[tokio::test]
    async fn rejected_entry_verify_returns_owner_terminal_after_reopen() {
        rejected_entry_read_after_reopen(true).await;
    }

    async fn duplicate_with_unavailable_or_changed_directory(state: ActionState, changed: bool) {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        rig.hub.store().with(|conn| {
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                conn,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                true,
                0,
            )
            .unwrap();
        });
        let command = send_cmd("directory_duplicate", &binding, "原意图");
        let queued = rig
            .hub
            .execute(&actor(), &rig.card, command.clone())
            .await
            .unwrap();
        assert_eq!(queued.state, ActionState::Queued);
        if state != ActionState::Queued {
            rig.hub.store().with(|conn| {
                let (revision, _) = crate::database::dao::opencode_outbox::target_state_conn(
                    conn,
                    SESSION_ID,
                    &rig.project.canonicalize().unwrap().to_string_lossy(),
                )
                .unwrap();
                crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                    conn,
                    SESSION_ID,
                    &rig.project.canonicalize().unwrap().to_string_lossy(),
                    false,
                    revision,
                )
                .unwrap();
            });
            assert!(rig.hub.action_store().claim(&command.action_id).unwrap());
            if state == ActionState::Unconfirmed {
                rig.hub
                    .action_store()
                    .finish(
                        &command.action_id,
                        ActionState::Unconfirmed,
                        None,
                        Some("readback_failed"),
                    )
                    .unwrap();
            }
        }
        let original = rig.hub.action(DEVICE_ID, &command.action_id).await.unwrap();
        assert_eq!(original.state, state);
        let mut card = rig.card.clone();
        let alternate = rig._tmp.path().join(if changed {
            "other-project"
        } else {
            "missing-project"
        });
        if changed {
            std::fs::create_dir_all(&alternate).unwrap();
        }
        card.project_path = alternate.to_string_lossy().into_owned();
        let result = rig.hub.execute(&actor(), &card, command.clone()).await;
        assert_eq!(rig.peer.prompt_posts(), 0, "重复请求不得再次派发");
        let returned = result.expect("既有动作必须保留原 actionId 的核对入口，不能误报确定拒绝");
        assert_eq!(returned, original);
    }

    macro_rules! duplicate_directory_test {
        ($name:ident, $state:expr, $changed:expr) => {
            #[tokio::test]
            async fn $name() {
                duplicate_with_unavailable_or_changed_directory($state, $changed).await;
            }
        };
    }

    duplicate_directory_test!(
        queued_duplicate_missing_directory_retains_action,
        ActionState::Queued,
        false
    );
    duplicate_directory_test!(
        queued_duplicate_changed_directory_retains_action,
        ActionState::Queued,
        true
    );
    duplicate_directory_test!(
        dispatching_duplicate_missing_directory_retains_action,
        ActionState::Dispatching,
        false
    );
    duplicate_directory_test!(
        dispatching_duplicate_changed_directory_retains_action,
        ActionState::Dispatching,
        true
    );
    duplicate_directory_test!(
        unconfirmed_duplicate_missing_directory_retains_action,
        ActionState::Unconfirmed,
        false
    );
    duplicate_directory_test!(
        unconfirmed_duplicate_changed_directory_retains_action,
        ActionState::Unconfirmed,
        true
    );

    #[tokio::test]
    async fn rejected_entry_blocks_waiter_and_later_same_id_after_service_recovers() {
        let (rig, command) = rejected_entry_rig().await;
        let (entered, release) = arm_info_hold(&rig);
        rig.peer.set(|world| world.info_pid = Some(PID + 1));
        let hub = rig.hub.clone();
        let card = rig.card.clone();
        let first_command = command.clone();
        let first = tokio::spawn(async move { hub.execute(&actor(), &card, first_command).await });
        wait_for_test_barrier(&entered).await;

        use std::future::Future;
        let (waiting_tx, waiting_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let hub = rig.hub.clone();
        let card = rig.card.clone();
        let second_command = command.clone();
        let second = tokio::spawn(async move {
            let actor = actor();
            let future = hub.execute(&actor, &card, second_command);
            tokio::pin!(future);
            std::future::poll_fn(|cx| match future.as_mut().poll(cx) {
                std::task::Poll::Pending => std::task::Poll::Ready(()),
                std::task::Poll::Ready(result) => panic!("B 在 A 定案前完成: {result:?}"),
            })
            .await;
            waiting_tx.send(()).unwrap();
            resume_rx.await.unwrap();
            future.await
        });
        waiting_rx.await.unwrap();
        release.notify_one();
        let first_result = first.await.unwrap();
        assert!(first_result.is_err(), "A 必须先确定拒绝: {first_result:?}");
        // B 的 future 已排队，但测试屏障在服务恢复前不再轮询它。
        rig.peer.set(|world| world.info_pid = None);
        resume_tx.send(()).unwrap();
        let second_result = second.await.unwrap();
        let third_result = rig.hub.execute(&actor(), &rig.card, command).await;
        assert_rejected_entry_has_no_effect(&rig);
        assert!(
            second_result.is_err(),
            "并发等待者不得重新执行: {second_result:?}"
        );
        assert!(
            third_result.is_err(),
            "后续同 ID 不得重新执行: {third_result:?}"
        );
    }

    #[tokio::test]
    async fn rejected_entry_blocks_later_same_id_after_service_recovers() {
        let (rig, command) = rejected_entry_rig().await;
        reject_entry(&rig, &command).await;
        let result = rig.hub.execute(&actor(), &rig.card, command).await;
        assert_rejected_entry_has_no_effect(&rig);
        assert!(result.is_err(), "后续同 ID 不得重新执行: {result:?}");
    }

    #[tokio::test]
    async fn rejected_entry_id_cannot_be_reused_by_another_device() {
        let (rig, command) = rejected_entry_rig().await;
        reject_entry(&rig, &command).await;
        let other = NativeActor {
            device_id: "dev-other".into(),
            ..actor()
        };
        let result = rig.hub.execute(&other, &rig.card, command).await;
        assert_rejected_entry_has_no_effect(&rig);
        assert!(result.is_err(), "其他设备不得取得原请求结果: {result:?}");
    }

    #[tokio::test]
    async fn rejected_entry_id_cannot_be_reused_for_another_intent() {
        let (rig, command) = rejected_entry_rig().await;
        reject_entry(&rig, &command).await;
        let changed = send_cmd(
            "entry_rejected_id",
            &command.binding_token,
            "different intent",
        );
        let result = rig.hub.execute(&actor(), &rig.card, changed).await;
        assert_rejected_entry_has_no_effect(&rig);
        assert!(result.is_err(), "不同意图不得取得原请求结果: {result:?}");
    }

    #[tokio::test]
    async fn rejected_entry_does_not_prevent_new_id_from_sending() {
        let (rig, command) = rejected_entry_rig().await;
        reject_entry(&rig, &command).await;
        let new_command = send_cmd("entry_new_id", &command.binding_token, "new message");
        let result = rig.hub.execute(&actor(), &rig.card, new_command).await;
        assert!(result.is_ok(), "新 ID 应能发送: {result:?}");
        assert_eq!(rig.peer.prompt_posts(), 1);
        assert!(rig
            .hub
            .store()
            .with(|conn| dao::get_action_conn(conn, "entry_rejected_id").unwrap())
            .is_none());
    }

    #[tokio::test]
    async fn rejected_entry_remains_blocked_after_reopening_persistent_database() {
        const CHILD: &str = "TUVIS_TEST_REJECTED_ENTRY_CHILD";
        if let Some(root) = std::env::var_os(CHILD) {
            let root = PathBuf::from(root);
            let conn = rusqlite::Connection::open(root.join("entry-rejection.sqlite")).unwrap();
            let hub = NativeHub::with_coordinator_sources(
                DeviceStore::Owned(Arc::new(Mutex::new(conn))),
                DiscoveryInput {
                    state_file: root.join("state/opencode/service.json"),
                    home: root.join("home"),
                },
                process_fact(),
            );
            let card = card_for(SESSION_ID, &root.join("project"));
            let binding = hub.view(&card).await.unwrap().binding_token;
            let path = root
                .join("project/.tuvis-attachments")
                .join(SESSION_ID)
                .join("entry.txt");
            let command = send_cmd_with_attachments(
                "entry_rejected_id",
                &binding,
                "message",
                vec![path.to_string_lossy().into_owned()],
            );
            let result = hub.execute(&actor(), &card, command).await;
            assert!(result.is_err(), "新进程中原 ID 不得执行: {result:?}");
            return;
        }
        let (rig, command) = rejected_entry_rig().await;
        reject_entry(&rig, &command).await;
        let root = rig._tmp.path().to_owned();
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("services::opencode_native::actions::tests::rejected_entry_remains_blocked_after_reopening_persistent_database")
                .arg("--nocapture")
                .env(CHILD, root)
                .output().unwrap()
        }).await.unwrap();
        assert_rejected_entry_has_no_effect(&rig);
        assert!(
            output.status.success(),
            "新进程测试失败: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[tokio::test]
    async fn rebind_source_retracted_after_recheck_does_not_leave_unexplained_action() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        rig.hub.store().with(|conn| {
            assert!(
                crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                    conn,
                    SESSION_ID,
                    &rig.project.canonicalize().unwrap().to_string_lossy(),
                    true,
                    0
                )
                .unwrap()
            );
        });
        rig.hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("rebind_source_race", &binding, "保留意图"),
            )
            .await
            .unwrap();
        let command: NativeCommand = serde_json::from_value(json!({
            "actionId":"rebind_race_lost_source", "sessionId":SESSION_ID, "bindingToken":binding,
            "operation":{"kind":"rebind_outbox","targetActionId":"rebind_source_race"}
        }))
        .unwrap();
        let snapshot = rig.hub.dispatch_snapshot(&rig.card).await.unwrap();
        assert!(matches!(
            rig.hub
                .action_store()
                .reserve(DEVICE_ID, &snapshot.target, &command)
                .unwrap(),
            ReserveOutcome::New
        ));
        let (entered, release) = arm_info_hold(&rig);
        let hub = rig.hub.clone();
        let first_command = command.clone();
        let task = tokio::spawn(async move {
            let intent = intent_of(&first_command).unwrap();
            let mut timing = crate::services::opencode_native::budget::ActionTiming::start(
                first_command.action_id.as_str(),
                SESSION_ID,
                operation_word(&first_command.operation),
            )
            .begin();
            hub.dispatch(
                &actor(),
                &snapshot,
                first_command,
                intent,
                None,
                0,
                &mut timing,
            )
            .await
        });
        wait_for_test_barrier(&entered).await;
        rig.hub.store().with(|conn| {
            assert!(crate::database::dao::opencode_outbox::retract_conn(
                conn,
                DEVICE_ID,
                SESSION_ID,
                "rebind_source_race"
            )
            .unwrap());
        });
        release.notify_one();
        let result = task.await.unwrap();
        let row = rig.hub.action(DEVICE_ID, &command.action_id).await.unwrap();
        assert!(result.is_err(), "撤回源后重绑须拒绝");
        assert_eq!(rig.peer.prompt_posts(), 0);
        assert!(
            row.state != ActionState::NotSent || row.reason_code.is_some(),
            "重绑失败不得留下无理由 NotSent: {row:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_resume_eligibility_failure_returns_completed_action() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        rig.hub.store().with(|conn| {
            assert!(
                crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                    conn,
                    SESSION_ID,
                    &rig.project.canonicalize().unwrap().to_string_lossy(),
                    true,
                    0
                )
                .unwrap()
            );
        });
        let command = resume_command("resume_same_id_race", &binding);
        let snapshot = rig.hub.dispatch_snapshot(&rig.card).await.unwrap();
        assert!(matches!(
            rig.hub
                .action_store()
                .reserve(DEVICE_ID, &snapshot.target, &command)
                .unwrap(),
            ReserveOutcome::New
        ));
        let (entered, release) = arm_info_hold(&rig);
        let hub = rig.hub.clone();
        let first_command = command.clone();
        let task = tokio::spawn(async move {
            let intent = intent_of(&first_command).unwrap();
            let mut timing = crate::services::opencode_native::budget::ActionTiming::start(
                first_command.action_id.as_str(),
                SESSION_ID,
                operation_word(&first_command.operation),
            )
            .begin();
            hub.dispatch(
                &actor(),
                &snapshot,
                first_command,
                intent,
                None,
                0,
                &mut timing,
            )
            .await
        });
        wait_for_test_barrier(&entered).await;
        let completed = rig
            .hub
            .execute(&actor(), &rig.card, command.clone())
            .await
            .unwrap();
        assert_eq!(completed.state, ActionState::Settled);
        rig.hub.store().with(|conn| {
            conn.execute(
                "UPDATE opencode_native_routes SET route_state='draining' WHERE session_id=?1",
                [SESSION_ID],
            )
            .unwrap();
        });
        release.notify_one();
        let result = task.await.unwrap();
        assert_eq!(
            rig.hub
                .action(DEVICE_ID, &command.action_id)
                .await
                .unwrap()
                .state,
            ActionState::Settled
        );
        assert_eq!(rig.peer.non_get(), 0);
        assert_eq!(
            result.expect("重复恢复的资格失败须返回已完成结果").state,
            ActionState::Settled
        );
    }

    #[tokio::test]
    async fn opencode_outbox_paused_send_is_persisted_without_native_post() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        rig.hub.store().with(|c| {
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                true,
                0,
            )
            .unwrap()
        });
        let reply = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("paused_send", &binding, "保存待发"),
            )
            .await
            .unwrap();
        assert_eq!(reply.state, ActionState::Queued);
        assert_eq!(rig.peer.prompt_posts(), 0);
        assert_eq!(inject_queue_rows(&rig), 0);
    }

    // ==== Task C：插队 = 派发既有队列行（OpenCode 原生 steer 自带打断） ====
    //
    // 用户裁决：不调 stop_session（steer 本身打断当前回合），不接受正文副本
    // （`text`/`content` 一律 400），派发的必须是队列里既有那一行（原 actionId、
    // 原 wire 正文、原 client_message_id）。一次插队 = 一个 actionId = 最多一次派发。

    /// 造一条 queued outbox 行（reserve + 冻结入队，尚不派发），并返回其冻结载荷。
    fn seed_queued_outbox_row(
        rig: &Rig,
        action_id: &str,
        text: &str,
    ) -> super::super::outbox::FrozenSend {
        let command = send_cmd(action_id, "binding", text);
        rig.hub
            .action_store()
            .reserve(DEVICE_ID, &bound_target(&rig.project), &command)
            .unwrap();
        let NativeOperation::Send(send) = &command.operation else {
            unreachable!();
        };
        let frozen_send = send.clone();
        rig.hub.store().with(|conn| {
            assert!(
                crate::database::dao::opencode_outbox::enqueue_frozen_conn(
                    conn,
                    &command,
                    &[],
                    send,
                    None,
                    None
                )
                .unwrap(),
                "前置：queued 行必须入队成功"
            );
        });
        super::super::outbox::FrozenSend {
            command,
            attachment_paths: Vec::new(),
            send: frozen_send,
        }
    }

    fn prioritize_cmd(action_id: &str, binding: &str, target_action_id: &str) -> NativeCommand {
        NativeCommand {
            action_id: action_id.to_string(),
            session_id: SESSION_ID.to_string(),
            binding_token: binding.to_string(),
            operation: NativeOperation::PrioritizeOutbox(
                crate::services::opencode_native::types::OutboxRetractPayload {
                    target_action_id: target_action_id.to_string(),
                },
            ),
        }
    }

    /// 插队派发**既有队列行**：上游只收到一次 prompt，正文与 clientMessageId 逐字
    /// 等于原冻结行，delivery 被改成 steer；原行不再被 FIFO 二次派发。
    #[tokio::test]
    async fn prioritize_outbox_dispatches_existing_row_once() {
        let world = World::new();
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        // 两行排队：插队行是**后**排的那条（越队必须成立）。
        let head = seed_queued_outbox_row(&rig, "act_prioritize_head", "排在前面");
        let target = seed_queued_outbox_row(&rig, "act_prioritize_target", "插队这一条");
        assert_eq!(rig.peer.prompt_posts(), 0, "前置：入队不派发");

        let receipt = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                prioritize_cmd("act_prioritize_now", &binding, "act_prioritize_target"),
            )
            .await
            .expect("插队必须放行");
        // 用户可见的「会打断」信号：控制动作 settled + steered_interrupt（不新增状态词）。
        assert_eq!(
            receipt.state,
            ActionState::Settled,
            "插队控制动作落 settled"
        );
        assert_eq!(
            receipt.reason_code.as_deref(),
            Some("steered_interrupt"),
            "回执必须携带可判定的打断信号"
        );
        // 重复提交同一插队 actionId：返回既有回执，绝不二次派发。
        let again = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                prioritize_cmd("act_prioritize_now", &binding, "act_prioritize_target"),
            )
            .await
            .expect("重复提交返回既有动作");
        assert_eq!(again.state, ActionState::Settled);
        assert_eq!(rig.peer.prompt_posts(), 1, "同一 actionId 绝不二次派发");
        // 插队动作本身 = 一次派发；被插队的原行不产生第二个动作。
        assert_eq!(rig.peer.prompt_posts(), 1, "插队只允许一次上游派发");

        let posted = rig
            .peer
            .log()
            .into_iter()
            .find(|request| request.path() == format!("/api/session/{SESSION_ID}/prompt"))
            .expect("必须有 prompt POST");
        let body = posted.body.expect("prompt 必须有请求体");
        assert_eq!(
            body["text"],
            json!("插队这一条"),
            "正文必须是原冻结正文（不是新生成/副本）"
        );
        assert_eq!(
            body["id"],
            json!("msg_act_prioritize_target"),
            "clientMessageId 必须是原行的（不是插队动作的新 id）"
        );
        assert_eq!(body["delivery"], json!("steer"), "唯一改动：queue → steer");
        assert_eq!(
            rig.hub
                .action(DEVICE_ID, "act_prioritize_target")
                .await
                .unwrap()
                .state,
            ActionState::Confirmed,
            "确认落在**原队列行**上（它不是第二条消息）"
        );
        // 原行载荷在 claim 时销毁 ⇒ FIFO 不会再派发它。
        assert!(
            h1_outbox_payload(&rig, "act_prioritize_target").is_none(),
            "插队行载荷已销毁"
        );
        // 后续后台循环：只剩队首那一行，且它照常派发一次（保序不受破坏）。
        rig.hub.store().with(|c| {
            let (revision, _) = crate::database::dao::opencode_outbox::target_state_conn(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
            )
            .unwrap();
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                false,
                revision,
            )
            .unwrap();
        });
        rig.hub.flush_outbox(&rig.card).await.unwrap();
        rig.hub.flush_outbox(&rig.card).await.unwrap();
        assert_eq!(
            rig.peer.prompt_posts(),
            2,
            "插队行绝不二次派发；队列剩下的队首各派发一次"
        );
        let bodies: Vec<String> = rig
            .peer
            .log()
            .into_iter()
            .filter(|request| request.path() == format!("/api/session/{SESSION_ID}/prompt"))
            .filter_map(|request| request.body)
            .map(|body| body["text"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(
            bodies,
            vec![target.send.text.clone(), head.send.text.clone(),],
            "插队先出，原队首随后；每条各一次"
        );
    }

    /// 插队**不接受正文副本**：请求体带 `text` 即反序列化失败（wire 层 400），
    /// 且绝不因为「带了正文」而改写任何队列行。
    #[test]
    fn prioritize_outbox_rejects_body_copy() {
        let raw = json!({
            "actionId": "act_prioritize_copy",
            "sessionId": SESSION_ID,
            "bindingToken": "binding",
            "operation": {
                "kind": "prioritize_outbox",
                "targetActionId": "act_queued_row",
                "text": "这是正文副本，必须拒绝"
            }
        });
        assert!(
            serde_json::from_value::<NativeCommand>(raw.clone()).is_err(),
            "带 text 的插队请求必须在反序列化层被拒（deny_unknown_fields）"
        );
        // 纯载荷形状（无正文副本）可行。
        let ok = json!({
            "actionId": "act_prioritize_copy",
            "sessionId": SESSION_ID,
            "bindingToken": "binding",
            "operation": {"kind": "prioritize_outbox", "targetActionId": "act_queued_row"}
        });
        let command: NativeCommand = serde_json::from_value(ok).expect("合法载荷");
        assert_eq!(
            operation_word(&command.operation),
            "prioritize_outbox",
            "wire 名固定为 prioritize_outbox"
        );
        assert!(matches!(
            command.operation,
            NativeOperation::PrioritizeOutbox(_)
        ));
        // 反例：`content` 同样是正文副本。
        let mut with_content = raw;
        with_content["operation"] = json!({
            "kind": "prioritize_outbox",
            "targetActionId": "act_queued_row",
            "content": "x"
        });
        assert!(
            serde_json::from_value::<NativeCommand>(with_content).is_err(),
            "带 content 的插队请求同样必须被拒"
        );
    }

    /// 目标不存在 / 已 `confirmed`：受控拒绝、上游零 prompt、账本零新增。
    #[tokio::test]
    async fn prioritize_outbox_unknown_or_not_queued_is_zero_write() {
        // ① 目标 actionId 不存在。
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let ledger_before = ledger_rows(&rig);
        let err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                prioritize_cmd("act_prioritize_missing", &binding, "act_never_existed"),
            )
            .await
            .expect_err("不存在的目标必须拒绝");
        assert_eq!(err.code, action_codes::NOT_FOUND, "不存在 = 404 语义");
        assert_eq!(rig.peer.prompt_posts(), 0, "零 prompt 写出");
        assert_eq!(ledger_rows(&rig), ledger_before, "账本零新增");

        // ② 目标已 confirmed（已派发过）：同样零写。
        let confirmed = send_cmd("act_prioritize_done", &binding, "已经发过的");
        let view = rig
            .hub
            .execute(&actor(), &rig.card, confirmed)
            .await
            .expect("正常发送先落成 confirmed");
        assert_eq!(view.state, ActionState::Confirmed);
        let posts_after_send = rig.peer.prompt_posts();
        let ledger_before = ledger_rows(&rig);
        let err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                prioritize_cmd("act_prioritize_done_again", &binding, "act_prioritize_done"),
            )
            .await
            .expect_err("已终态的行不可插队");
        assert_eq!(err.code, action_codes::CONFLICT, "不在队列 = 409 语义");
        assert_eq!(
            rig.peer.prompt_posts(),
            posts_after_send,
            "零额外 prompt 写出"
        );
        assert_eq!(ledger_rows(&rig), ledger_before, "账本零新增");

        // ③ 目标仍 queued 但属**别的设备**：按不存在处理（不做存在性预言机）。
        let other = seed_queued_outbox_row(&rig, "act_prioritize_foreign", "别人的行");
        rig.hub.store().with(|conn| {
            conn.execute(
                "UPDATE opencode_native_actions SET device_id='someone-else' WHERE action_id='act_prioritize_foreign'",
                [],
            )
            .unwrap();
        });
        let err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                prioritize_cmd(
                    "act_prioritize_foreign_now",
                    &binding,
                    "act_prioritize_foreign",
                ),
            )
            .await
            .expect_err("别人的行不可插队");
        assert_eq!(err.code, action_codes::NOT_FOUND);
        assert_eq!(
            rig.peer.prompt_posts(),
            posts_after_send,
            "别人的行零 prompt"
        );
        assert_eq!(
            h1_outbox_payload(&rig, "act_prioritize_foreign"),
            Some(serde_json::to_string(&other).unwrap()),
            "拒绝时别人的冻结载荷原样保留"
        );

        // ④ 目标仍 queued 但冻结载荷已被清空（不可派发形态）：零写拒绝。
        seed_queued_outbox_row(&rig, "act_prioritize_payload_less", "载荷已失");
        rig.hub.store().with(|conn| {
            conn.execute(
                "UPDATE opencode_outbox SET command_json=NULL WHERE action_id='act_prioritize_payload_less'",
                [],
            )
            .unwrap();
        });
        let ledger_before = ledger_rows(&rig);
        let err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                prioritize_cmd(
                    "act_prioritize_payload_less_now",
                    &binding,
                    "act_prioritize_payload_less",
                ),
            )
            .await
            .expect_err("无冻结载荷的 queued 行不可插队");
        assert_eq!(err.code, codes::OC_ACTION_INVALID);
        assert_eq!(
            rig.peer.prompt_posts(),
            posts_after_send,
            "不可派发形态零 prompt"
        );
        assert_eq!(ledger_rows(&rig), ledger_before, "账本零新增");
    }

    #[tokio::test]
    async fn opencode_outbox_resume_consumes_confirmed_intent_once() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        rig.hub.store().with(|c| {
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                true,
                0,
            )
            .unwrap()
        });
        rig.hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("resume_send", &binding, "稍后发送"),
            )
            .await
            .unwrap();
        rig.hub.store().with(|c| {
            let (revision, _) = crate::database::dao::opencode_outbox::target_state_conn(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
            )
            .unwrap();
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                false,
                revision,
            )
            .unwrap();
        });
        rig.hub.flush_outbox(&rig.card).await.unwrap();
        rig.hub.flush_outbox(&rig.card).await.unwrap();
        assert_eq!(rig.peer.prompt_posts(), 1);
        assert_eq!(
            rig.hub
                .action(DEVICE_ID, "resume_send")
                .await
                .unwrap()
                .state,
            ActionState::Confirmed
        );
    }

    #[tokio::test]
    async fn opencode_outbox_revoked_device_cannot_claim_direct_send() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        rig.hub
            .store()
            .with(|c| crate::remote::pairing::revoke_device(c, DEVICE_ID).unwrap());
        let _ = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("revoked_send", &binding, "不得派发"),
            )
            .await;
        assert_eq!(rig.peer.prompt_posts(), 0);
    }

    #[tokio::test]
    async fn opencode_outbox_remote_stop_blocks_new_claim() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        rig.hub.pause();
        let binding = current_binding(&rig).await;
        let _ = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("stopped_send", &binding, "冻结待发"),
            )
            .await;
        assert_eq!(rig.peer.prompt_posts(), 0);
    }

    #[tokio::test]
    async fn opencode_outbox_freezes_attachment_bytes_before_acknowledging_queue() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let path = controlled_attachment(&rig.project, "frozen.txt", 0);
        std::fs::write(&path, b"original").unwrap();
        rig.hub.store().with(|c| {
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                true,
                0,
            )
            .unwrap()
        });
        rig.hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd_with_attachments("frozen_send", &binding, "附件", vec![path.clone()]),
            )
            .await
            .unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        rig.hub.store().with(|c| {
            let (r, _) = crate::database::dao::opencode_outbox::target_state_conn(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
            )
            .unwrap();
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                false,
                r,
            )
            .unwrap();
        });
        rig.hub.flush_outbox(&rig.card).await.unwrap();
        let posted = rig
            .peer
            .log()
            .into_iter()
            .find(|r| r.method == "POST")
            .unwrap()
            .body
            .unwrap();
        let frozen = reqwest::Url::parse(posted["files"][0]["uri"].as_str().unwrap())
            .unwrap()
            .to_file_path()
            .unwrap();
        assert_eq!(std::fs::read(frozen).unwrap(), b"original");
    }

    #[tokio::test]
    async fn opencode_outbox_pause_command_is_local_idempotent_and_revision_checked() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let command:NativeCommand=serde_json::from_value(json!({"actionId":"pause_control","sessionId":SESSION_ID,"bindingToken":binding,"operation":{"kind":"set_outbox_paused","paused":true,"expectedRevision":0}})).unwrap();
        let reply = rig
            .hub
            .execute(&actor(), &rig.card, command.clone())
            .await
            .unwrap();
        assert_eq!(reply.state, ActionState::Settled);
        rig.hub.execute(&actor(), &rig.card, command).await.unwrap();
        assert_eq!(
            rig.hub.store().with(
                |c| crate::database::dao::opencode_outbox::target_state_conn(
                    c,
                    SESSION_ID,
                    &rig.project.canonicalize().unwrap().to_string_lossy()
                )
                .unwrap()
            ),
            (1, true)
        );
        assert_eq!(rig.peer.non_get(), 0);
    }

    fn resume_command(action_id: &str, binding: &str) -> NativeCommand {
        serde_json::from_value(json!({
            "actionId": action_id, "sessionId": SESSION_ID, "bindingToken": binding,
            "operation": {"kind": "set_outbox_paused", "paused": false, "expectedRevision": 1}
        }))
        .unwrap()
    }

    async fn resume_with_reserve_trigger(trigger: &str, expect_rejection: bool) {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        rig.hub.store().with(|conn| {
            assert!(
                crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                    conn,
                    SESSION_ID,
                    &rig.project.canonicalize().unwrap().to_string_lossy(),
                    true,
                    0
                )
                .unwrap()
            );
            conn.execute_batch(trigger).unwrap();
        });
        let result = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                resume_command("resume_interrupted", &binding),
            )
            .await;
        assert_eq!(rig.peer.non_get(), 0, "恢复控制不得发原生写");
        let state = rig.hub.store().with(|conn| {
            crate::database::dao::opencode_outbox::target_state_conn(
                conn,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
            )
            .unwrap()
        });
        assert_eq!(state, (1, true), "复核失败不得解除暂停");
        if expect_rejection {
            assert!(!matches!(result, Ok(view) if view.state == ActionState::Settled));
        }
        let row = rig
            .hub
            .store()
            .with(|conn| dao::get_action_conn(conn, "resume_interrupted").unwrap());
        assert!(
            row.is_none_or(|row| row.state != ActionState::NotSent || row.reason_code.is_some()),
            "reserve 后复核失败不得留下无理由 not_sent"
        );
    }

    #[tokio::test]
    async fn resume_dispatch_rejects_route_draining_after_reserve() {
        resume_with_reserve_trigger(
            "CREATE TRIGGER resume_drain AFTER INSERT ON opencode_native_actions BEGIN UPDATE opencode_native_routes SET route_state='draining' WHERE session_id=NEW.session_id; END;",
            true,
        )
        .await;
    }

    #[tokio::test]
    async fn resume_dispatch_rejects_route_generation_change_after_reserve() {
        resume_with_reserve_trigger(
            "CREATE TRIGGER resume_generation AFTER INSERT ON opencode_native_actions BEGIN UPDATE opencode_native_routes SET runtime_pid=runtime_pid+1 WHERE session_id=NEW.session_id; END;",
            true,
        )
        .await;
    }

    #[tokio::test]
    async fn resume_dispatch_revocation_after_reserve_leaves_no_unexplained_row() {
        resume_with_reserve_trigger(
            "CREATE TRIGGER resume_revoke AFTER INSERT ON opencode_native_actions BEGIN UPDATE remote_devices SET revoked=1 WHERE id=NEW.device_id; END;",
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn resume_dispatch_stale_epoch_after_reserve_leaves_no_unexplained_row() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        rig.hub.store().with(|conn| {
            assert!(
                crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                    conn,
                    SESSION_ID,
                    &rig.project.canonicalize().unwrap().to_string_lossy(),
                    true,
                    0
                )
                .unwrap()
            );
        });
        let snapshot = rig.hub.dispatch_snapshot(&rig.card).await.unwrap();
        let command = resume_command("resume_stale_epoch", &binding);
        let store = rig.hub.action_store();
        assert!(matches!(
            store
                .reserve(DEVICE_ID, &snapshot.target, &command)
                .unwrap(),
            ReserveOutcome::New
        ));
        let epoch = rig
            .hub
            .outbox_runtime()
            .epoch
            .fetch_add(1, Ordering::SeqCst);
        let intent = intent_of(&command).unwrap();
        let mut timing = crate::services::opencode_native::budget::ActionTiming::start(
            command.action_id.as_str(),
            SESSION_ID,
            operation_word(&command.operation),
        )
        .begin();
        let _ = rig
            .hub
            .dispatch(
                &actor(),
                &snapshot,
                command.clone(),
                intent,
                None,
                epoch,
                &mut timing,
            )
            .await;
        assert_eq!(rig.peer.non_get(), 0);
        assert_eq!(
            rig.hub.store().with(
                |conn| crate::database::dao::opencode_outbox::target_state_conn(
                    conn,
                    SESSION_ID,
                    &rig.project.canonicalize().unwrap().to_string_lossy()
                )
                .unwrap()
            ),
            (1, true)
        );
        let row = rig
            .hub
            .store()
            .with(|conn| dao::get_action_conn(conn, &command.action_id).unwrap());
        assert!(
            row.is_none_or(|row| row.state != ActionState::NotSent || row.reason_code.is_some()),
            "旧派发代次不得留下无理由 not_sent"
        );
    }

    #[tokio::test]
    async fn local_outbox_stale_binding_cannot_resume_offline() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let old_binding = current_binding(&rig).await;
        rig.hub.store().with(|conn| {
            assert!(
                crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                    conn,
                    SESSION_ID,
                    &rig.project.canonicalize().unwrap().to_string_lossy(),
                    true,
                    0
                )
                .unwrap()
            );
        });
        rig.peer.restart();
        let mut new_target = bound_target(&rig.project);
        new_target.runtime.pid += 9000;
        new_target.runtime.service_id = format!("svc-restarted-{}", new_target.runtime.pid);
        new_target.runtime.started_at_ms = STARTED_AT_MS + 60_000;
        assert!(rig
            .hub
            .action_store()
            .try_claim_route(&new_target, true)
            .unwrap());
        rig.peer.shared.offline.store(true, Ordering::SeqCst);
        let command: NativeCommand = serde_json::from_value(json!({
            "actionId": "old_binding_resume", "sessionId": SESSION_ID, "bindingToken": old_binding,
            "operation": {"kind": "set_outbox_paused", "paused": false, "expectedRevision": 1}
        }))
        .unwrap();
        let result = rig.hub.execute(&actor(), &rig.card, command).await;
        assert_eq!(rig.peer.non_get(), 0);
        let state = rig.hub.store().with(|conn| {
            crate::database::dao::opencode_outbox::target_state_conn(
                conn,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
            )
            .unwrap()
        });
        assert_eq!(state, (1, true), "旧绑定不得离线解除暂停");
        assert!(!matches!(result, Ok(view) if view.state == ActionState::Settled));
    }

    #[tokio::test]
    async fn local_outbox_revoked_device_cannot_read_duplicate_action() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let command: NativeCommand = serde_json::from_value(json!({
            "actionId": "revoked_duplicate_pause", "sessionId": SESSION_ID, "bindingToken": binding,
            "operation": {"kind": "set_outbox_paused", "paused": true, "expectedRevision": 0}
        }))
        .unwrap();
        assert_eq!(
            rig.hub
                .execute(&actor(), &rig.card, command.clone())
                .await
                .unwrap()
                .state,
            ActionState::Settled
        );
        rig.hub
            .store()
            .with(|conn| crate::remote::pairing::revoke_device(conn, DEVICE_ID).unwrap());
        let result = rig.hub.execute(&actor(), &rig.card, command).await;
        assert_eq!(rig.peer.non_get(), 0);
        assert!(result.is_err(), "设备撤销后不得读到重复动作: {result:?}");
    }

    async fn local_outbox_reserve_interruption_leaves_no_unexplained_row(trigger: &str) {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        rig.hub
            .store()
            .with(|conn| conn.execute_batch(trigger).unwrap());
        let command: NativeCommand = serde_json::from_value(json!({
            "actionId": "interrupted_local_pause", "sessionId": SESSION_ID, "bindingToken": binding,
            "operation": {"kind": "set_outbox_paused", "paused": true, "expectedRevision": 0}
        }))
        .unwrap();
        let result = rig.hub.execute(&actor(), &rig.card, command).await;
        assert!(result.is_err(), "预期最终复核拒绝");
        assert_eq!(rig.peer.non_get(), 0);
        assert_eq!(
            rig.hub.store().with(
                |conn| crate::database::dao::opencode_outbox::target_state_conn(
                    conn,
                    SESSION_ID,
                    &rig.project.canonicalize().unwrap().to_string_lossy()
                )
                .unwrap()
            ),
            (0, false)
        );
        let row = rig
            .hub
            .store()
            .with(|conn| dao::get_action_conn(conn, "interrupted_local_pause").unwrap());
        assert!(
            row.is_none_or(|row| row.state != ActionState::NotSent || row.reason_code.is_some()),
            "reserve 后失败不得遗留无理由 not_sent"
        );
    }

    #[tokio::test]
    async fn local_outbox_revocation_during_reserve_does_not_leave_unexplained_row() {
        local_outbox_reserve_interruption_leaves_no_unexplained_row(
            "CREATE TRIGGER revoke_during_reserve AFTER INSERT ON opencode_native_actions BEGIN UPDATE remote_devices SET revoked=1 WHERE id=NEW.device_id; END;"
        ).await;
    }

    #[tokio::test]
    async fn local_outbox_route_change_during_reserve_does_not_leave_unexplained_row() {
        local_outbox_reserve_interruption_leaves_no_unexplained_row(
            "CREATE TRIGGER change_route_during_reserve AFTER INSERT ON opencode_native_actions BEGIN UPDATE opencode_native_routes SET route_state='draining' WHERE session_id=NEW.session_id; END;"
        ).await;
    }

    #[tokio::test]
    async fn local_outbox_route_generation_change_during_reserve_does_not_leave_unexplained_row() {
        local_outbox_reserve_interruption_leaves_no_unexplained_row(
            "CREATE TRIGGER replace_route_during_reserve AFTER INSERT ON opencode_native_actions BEGIN UPDATE opencode_native_routes SET runtime_pid=runtime_pid+1 WHERE session_id=NEW.session_id; END;"
        ).await;
    }

    #[tokio::test]
    async fn local_outbox_stale_epoch_does_not_leave_unexplained_row() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let epoch = rig.hub.outbox_runtime().epoch.load(Ordering::SeqCst);
        rig.hub.pause();
        let command: NativeCommand = serde_json::from_value(json!({
            "actionId": "stale_epoch_local_pause", "sessionId": SESSION_ID, "bindingToken": binding,
            "operation": {"kind": "set_outbox_paused", "paused": true, "expectedRevision": 0}
        }))
        .unwrap();
        assert!(rig
            .hub
            .execute_local_outbox(&actor(), &rig.card, &command, epoch)
            .await
            .is_err());
        assert_eq!(rig.peer.non_get(), 0);
        let row = rig
            .hub
            .store()
            .with(|conn| dao::get_action_conn(conn, &command.action_id).unwrap());
        assert!(
            row.is_none_or(|row| row.state != ActionState::NotSent || row.reason_code.is_some()),
            "旧代次不得遗留无理由 not_sent"
        );
    }

    /// D-N5-2（孤儿待发项）：客户端在门轮被中止留下的 `not_sent` 且无理由行
    /// （reserve 已落、claim 从未发生 = **从未出网**）必须：
    /// ① 在待发区可见（owner=tuvis，正文不可得 ⇒ 摘要留空由前端如实标注）；
    /// ② 可撤回（本地安全操作，零网络写）；③ 撤回后脱离未决谓词（不再挡显式重选）。
    #[tokio::test]
    async fn orphan_not_sent_without_reason_is_visible_and_retractable_in_outbox() {
        let rig = boot(World::new()).await;
        let directory = rig.project.canonicalize().expect("归一项目目录");
        let directory_string = directory.to_string_lossy().into_owned();
        // 直接落一条孤儿行（N5 act_1458d026 的账本形态：无 outbox 行、无理由）。
        rig.hub.store().with(|conn| {
            conn.execute(
                "INSERT INTO opencode_native_actions (action_id,device_id,session_id,service_id,\
                 runtime_pid,runtime_started_at_ms,directory,operation,request_key,payload_sha256,\
                 occupancy_key,state,created_at,updated_at) \
                 VALUES ('act_orphan_n5',?1,?2,?3,?4,?5,?6,'send',NULL,'hash-orphan',NULL,\
                 'not_sent',1,1)",
                rusqlite::params![
                    DEVICE_ID,
                    SESSION_ID,
                    SERVICE_ID,
                    PID,
                    STARTED_AT_MS,
                    directory_string
                ],
            )
            .unwrap();
        });
        // 撤回前：未决谓词计入（挡重选的现状）。
        rig.hub.store().with(|conn| {
            assert_eq!(
                crate::database::dao::opencode_actions::unresolved_for_session_conn(
                    conn, SESSION_ID
                )
                .unwrap(),
                1,
                "孤儿计入未决谓词（现状语义保持）"
            );
        });
        // 可见：待发区呈现为本地项，正文摘要留空（不可得，不冒充原文）。
        let snapshot = rig.hub.outbox_view(&rig.card).expect("待发视图");
        let orphan = snapshot
            .items
            .iter()
            .find(|item| item.action_id == "act_orphan_n5")
            .expect("孤儿必须在待发区可见（D-N5-2）");
        assert_eq!(orphan.owner, "tuvis");
        assert_eq!(orphan.state, ActionState::NotSent);
        assert!(orphan.reason_code.is_none());
        assert!(orphan.text_summary.is_empty(), "正文不可得：摘要留空");
        // 撤回：本地安全操作（零服务写）。
        let binding = current_binding(&rig).await;
        let epoch = rig.hub.outbox_runtime().epoch.load(Ordering::SeqCst);
        let command = NativeCommand {
            action_id: "act_retract_orphan_n5".to_string(),
            session_id: SESSION_ID.to_string(),
            binding_token: binding,
            operation: NativeOperation::RetractOutbox(
                crate::services::opencode_native::types::OutboxRetractPayload {
                    target_action_id: "act_orphan_n5".to_string(),
                },
            ),
        };
        let view = rig
            .hub
            .execute_local_outbox(&actor(), &rig.card, &command, epoch)
            .await
            .expect("孤儿撤回必须放行（从未出网）");
        assert_eq!(view.state, ActionState::Settled, "撤回命令本身落账为已处理");
        // 撤回后：行带放弃理由 → 脱离未决谓词；待发区不再呈现。
        rig.hub.store().with(|conn| {
            let row = dao::get_action_conn(conn, "act_orphan_n5")
                .unwrap()
                .unwrap();
            assert_eq!(row.state, ActionState::NotSent);
            assert_eq!(
                row.reason_code.as_deref(),
                Some("outbox_retracted"),
                "撤回写入放弃理由"
            );
            assert_eq!(
                crate::database::dao::opencode_actions::unresolved_for_session_conn(
                    conn, SESSION_ID
                )
                .unwrap(),
                0,
                "撤回后不得再有未决行挡显式重选"
            );
        });
        let after = rig.hub.outbox_view(&rig.card).expect("待发视图");
        assert!(
            after
                .items
                .iter()
                .all(|item| item.action_id != "act_orphan_n5"),
            "已撤回孤儿退出待发区"
        );
        assert_eq!(rig.peer.non_get(), 0, "撤回零网络写");
    }

    /// D-N5-2 × D-N5-3 联动：孤儿行挡「换代际显式重选」的未决门，撤回后放行。
    #[tokio::test]
    async fn orphan_retract_releases_generation_change_claim_gate() {
        let rig = boot(World::new()).await;
        let directory = rig.project.canonicalize().expect("归一项目目录");
        let directory_string = directory.to_string_lossy().into_owned();
        claim_route(&rig);
        let live = bound_target(&rig.project);
        // 路由行改写为死亡代际 + 落一条孤儿 not_sent(NULL) 行。
        let dead = NativeTarget {
            runtime: RuntimeKey {
                service_id: "dead-generation".to_string(),
                pid: PID + 5_678,
                started_at_ms: STARTED_AT_MS - 7,
            },
            session_id: SESSION_ID.to_string(),
            directory: directory.clone(),
        };
        rig.hub.store().with(|conn| {
            assert!(
                dao::claim_route_conn(conn, &dead, true, 1, true).expect("换代登记死亡路由"),
                "登记死亡路由行"
            );
            conn.execute(
                "INSERT INTO opencode_native_actions (action_id,device_id,session_id,service_id,\
                 runtime_pid,runtime_started_at_ms,directory,operation,request_key,payload_sha256,\
                 occupancy_key,state,created_at,updated_at) \
                 VALUES ('act_orphan_gate',?1,?2,?3,?4,?5,?6,'send',NULL,'hash',NULL,'not_sent',1,1)",
                rusqlite::params![
                    DEVICE_ID,
                    SESSION_ID,
                    SERVICE_ID,
                    PID,
                    STARTED_AT_MS,
                    directory_string
                ],
            )
            .unwrap();
        });
        let route_before = rig
            .hub
            .store()
            .with(|conn| dao::route_conn(conn, SESSION_ID).unwrap());
        // 未决孤儿在场：换代认领被拒（既有语义，保持）。
        rig.hub.store().with(|conn| {
            let claimed =
                dao::claim_route_checked_conn(conn, &live, true, 2, true, Some(&route_before))
                    .expect("认领事务");
            assert!(!claimed, "未决孤儿必须挡换代认领（fail-closed 保持）");
        });
        // 撤回孤儿 → 未决门释放 → 换代认领放行。
        rig.hub.store().with(|conn| {
            assert!(
                crate::database::dao::opencode_outbox::retract_conn(
                    conn,
                    DEVICE_ID,
                    SESSION_ID,
                    "act_orphan_gate"
                )
                .expect("撤回孤儿"),
                "孤儿可撤回"
            );
            let claimed =
                dao::claim_route_checked_conn(conn, &live, true, 3, true, Some(&route_before))
                    .expect("认领事务");
            assert!(claimed, "撤回后换代认领放行（重选解锁）");
            let route = dao::route_conn(conn, SESSION_ID).unwrap().unwrap();
            assert_eq!(route.runtime_pid, i64::from(live.runtime.pid));
            assert_eq!(route.route_state, dao::ROUTE_ACTIVE);
        });
    }

    #[tokio::test]
    async fn opencode_offline_local_pause_checks_device_target_and_revision() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        rig.peer.shared.offline.store(true, Ordering::SeqCst);
        assert!(reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("{}/api/info", rig.peer.shared.endpoint))
            .send()
            .await
            .is_err());
        let pause = |id: &str, revision: u64| -> NativeCommand {
            serde_json::from_value(json!({
                "actionId": id, "sessionId": SESSION_ID, "bindingToken": binding,
                "operation": {"kind": "set_outbox_paused", "paused": true, "expectedRevision": revision}
            }))
            .unwrap()
        };
        let rejected = |result: Result<ActionView, NativeError>| !matches!(result, Ok(view) if view.state == ActionState::Settled);
        let mut stranger = actor();
        stranger.device_id = "unpaired-device".to_string();
        assert!(rejected(
            rig.hub
                .execute(&stranger, &rig.card, pause("offline_pause_unpaired", 0))
                .await
        ));
        let mut wrong_target = rig.card.clone();
        wrong_target.project_path = rig._tmp.path().to_string_lossy().into_owned();
        assert!(rejected(
            rig.hub
                .execute(&actor(), &wrong_target, pause("offline_pause_target", 0))
                .await
        ));
        assert!(rejected(
            rig.hub
                .execute(&actor(), &rig.card, pause("offline_pause_stale", 1))
                .await
        ));
        assert_eq!(
            rig.hub.store().with(
                |c| crate::database::dao::opencode_outbox::target_state_conn(
                    c,
                    SESSION_ID,
                    &rig.project.canonicalize().unwrap().to_string_lossy()
                )
                .unwrap()
            ),
            (0, false)
        );

        let result = rig
            .hub
            .execute(&actor(), &rig.card, pause("offline_pause_valid", 0))
            .await;
        assert_eq!(rig.peer.non_get(), 0, "本地暂停不得产生原生写");
        assert_eq!(
            result.expect("服务不可达时仍可暂停").state,
            ActionState::Settled
        );
        assert_eq!(
            rig.hub.store().with(
                |c| crate::database::dao::opencode_outbox::target_state_conn(
                    c,
                    SESSION_ID,
                    &rig.project.canonicalize().unwrap().to_string_lossy()
                )
                .unwrap()
            ),
            (1, true)
        );
    }

    #[tokio::test]
    async fn opencode_offline_local_retract_checks_owner_and_original_target() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        rig.hub.store().with(|c| {
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                true,
                0,
            )
            .unwrap();
            crate::remote::pairing::persist_device(
                c,
                &crate::remote::pairing::NewDevice {
                    id: "other-device".into(),
                    name: "other".into(),
                    ua: "fixture".into(),
                    origin_ip: "127.0.0.1".into(),
                    via: "lan".into(),
                    paired_at: now_ms(),
                },
            )
            .unwrap();
        });
        assert_eq!(
            rig.hub
                .execute(
                    &actor(),
                    &rig.card,
                    send_cmd("offline_queued", &binding, "本地待发")
                )
                .await
                .unwrap()
                .state,
            ActionState::Queued
        );
        let before = rig.hub.outbox_view(&rig.card).unwrap().revision;
        rig.peer.shared.offline.store(true, Ordering::SeqCst);
        assert!(reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("{}/api/info", rig.peer.shared.endpoint))
            .send()
            .await
            .is_err());
        let retract = |id: &str| -> NativeCommand {
            serde_json::from_value(json!({
                "actionId": id, "sessionId": SESSION_ID, "bindingToken": binding,
                "operation": {"kind": "retract_outbox", "targetActionId": "offline_queued"}
            }))
            .unwrap()
        };
        let rejected = |result: Result<ActionView, NativeError>| !matches!(result, Ok(view) if view.state == ActionState::Settled);
        let mut other = actor();
        other.device_id = "other-device".to_string();
        assert!(rejected(
            rig.hub
                .execute(&other, &rig.card, retract("offline_retract_other"))
                .await
        ));
        let mut wrong_target = rig.card.clone();
        wrong_target.project_path = rig._tmp.path().to_string_lossy().into_owned();
        assert!(rejected(
            rig.hub
                .execute(&actor(), &wrong_target, retract("offline_retract_target"))
                .await
        ));
        assert_eq!(
            rig.hub
                .action(DEVICE_ID, "offline_queued")
                .await
                .unwrap()
                .state,
            ActionState::Queued
        );
        assert_eq!(rig.hub.outbox_view(&rig.card).unwrap().revision, before);

        let result = rig
            .hub
            .execute(&actor(), &rig.card, retract("offline_retract_valid"))
            .await;
        assert_eq!(rig.peer.non_get(), 0, "本地撤回不得产生原生写");
        assert_eq!(
            result.expect("服务不可达时仍可撤回").state,
            ActionState::Settled
        );
        let item = rig.hub.action(DEVICE_ID, "offline_queued").await.unwrap();
        assert_eq!(item.state, ActionState::NotSent);
        assert_eq!(item.reason_code.as_deref(), Some("outbox_retracted"));
        assert_eq!(rig.hub.outbox_view(&rig.card).unwrap().revision, before + 1);
    }

    #[tokio::test]
    async fn queued_items_dispatch_without_view_subscribers() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        rig.hub.store().with(|c| {
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                true,
                0,
            )
            .unwrap()
        });
        rig.hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("background_send", &binding, "关闭详情仍发送"),
            )
            .await
            .unwrap();
        rig.hub.store().with(|c| {
            let (r, _) = crate::database::dao::opencode_outbox::target_state_conn(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
            )
            .unwrap();
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                false,
                r,
            )
            .unwrap();
        });
        let card = rig.card.clone();
        rig.hub
            .start_outbox(std::sync::Arc::new(move || vec![card.clone()]));
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if rig.peer.prompt_posts() == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("无订阅也须后台派发");
        rig.hub.pause();
        assert_eq!(rig.peer.prompt_posts(), 1);
    }

    #[tokio::test]
    async fn opencode_outbox_snapshot_is_readable_offline_without_revision_mutation() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        rig.hub.store().with(|c| {
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                true,
                0,
            )
            .unwrap()
        });
        rig.hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("offline_send", &binding, "离线可见"),
            )
            .await
            .unwrap();
        rig.hub.pause();
        let before = rig.peer.log().len();
        let first = rig.hub.outbox_view(&rig.card).unwrap();
        let second = rig.hub.outbox_view(&rig.card).unwrap();
        assert_eq!(first.revision, second.revision);
        assert_eq!(first.items[0].state, ActionState::Queued);
        assert_eq!(rig.peer.log().len(), before);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn opencode_outbox_snapshot_directory_cannot_escape_controlled_root() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let path = controlled_attachment(&rig.project, "safe.txt", 8);
        let outside = rig._tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(
            &outside,
            attachment_dir(&rig.project, SESSION_ID).join("outbox"),
        )
        .unwrap();
        let _ = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd_with_attachments("escape_snapshot", &binding, "附件", vec![path]),
            )
            .await;
        assert_eq!(
            std::fs::read_dir(outside).unwrap().count(),
            0,
            "不能先在控制根外写入再拒绝"
        );
        assert_eq!(rig.peer.prompt_posts(), 0);
    }

    async fn renamed_outbox_fixture() -> (Rig, String) {
        let mut rig = boot(World::new()).await;
        rig.card.project_path = rig
            .project
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        rig.hub.store().with(|c| {
            assert!(
                crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                    c,
                    SESSION_ID,
                    &rig.project.canonicalize().unwrap().to_string_lossy(),
                    true,
                    0,
                )
                .unwrap()
            );
        });
        assert_eq!(
            rig.hub
                .execute(
                    &actor(),
                    &rig.card,
                    send_cmd("renamed_source", &binding, "待发内容")
                )
                .await
                .unwrap()
                .state,
            ActionState::Queued
        );
        std::fs::rename(&rig.project, rig._tmp.path().join("renamed-project")).unwrap();
        rig.peer.shared.offline.store(true, Ordering::SeqCst);
        (rig, binding)
    }

    async fn replaced_project_file_fixture() -> (Rig, String) {
        let (rig, binding) = renamed_outbox_fixture().await;
        std::fs::write(&rig.project, b"not a directory").unwrap();
        rig.peer.shared.offline.store(false, Ordering::SeqCst);
        assert!(rig.project.is_file());
        (rig, binding)
    }

    #[tokio::test]
    async fn replaced_project_file_cannot_bind_online() {
        let (rig, _) = replaced_project_file_fixture().await;
        assert!(
            !rig.hub.can_bind(&rig.card).await,
            "普通文件不能作为项目目录绑定"
        );
        assert_eq!(rig.peer.non_get(), 0);
    }

    #[tokio::test]
    async fn replaced_project_file_cannot_resume_outbox_online() {
        let (rig, binding) = replaced_project_file_fixture().await;
        let revision = rig.hub.outbox_view(&rig.card).unwrap().revision;
        let resume = outbox_control(
            "file_resume",
            &binding,
            json!({"kind":"set_outbox_paused","paused":false,"expectedRevision":revision}),
        );
        let result = rig.hub.execute(&actor(), &rig.card, resume).await;
        assert!(result.is_err(), "普通文件不能恢复派发: {result:?}");
        assert_eq!(
            rig.hub
                .action(DEVICE_ID, "renamed_source")
                .await
                .unwrap()
                .state,
            ActionState::Queued
        );
        assert_eq!(rig.peer.non_get(), 0);
    }

    #[tokio::test]
    async fn replaced_project_file_cannot_rebind_outbox_online() {
        let (rig, binding) = replaced_project_file_fixture().await;
        let rebind = outbox_control(
            "file_rebind",
            &binding,
            json!({"kind":"rebind_outbox","targetActionId":"renamed_source"}),
        );
        let result = rig.hub.execute(&actor(), &rig.card, rebind).await;
        assert!(result.is_err(), "普通文件不能重绑: {result:?}");
        assert_eq!(
            rig.hub
                .action(DEVICE_ID, "renamed_source")
                .await
                .unwrap()
                .state,
            ActionState::Queued
        );
        assert_eq!(rig.peer.non_get(), 0);
    }

    #[tokio::test]
    async fn replaced_project_file_preserves_read_only_old_action() {
        let (rig, _) = replaced_project_file_fixture().await;
        let before = rig.peer.log().len();
        assert_eq!(
            rig.hub
                .action(DEVICE_ID, "renamed_source")
                .await
                .unwrap()
                .state,
            ActionState::Queued
        );
        assert_eq!(
            rig.hub.outbox_view(&rig.card).unwrap().items[0].action_id,
            "renamed_source"
        );
        assert_eq!(rig.peer.log().len(), before);
    }

    #[tokio::test]
    async fn replaced_project_file_allows_original_queue_pause() {
        let (rig, binding) = replaced_project_file_fixture().await;
        let revision = rig.hub.outbox_view(&rig.card).unwrap().revision;
        let pause = outbox_control(
            "file_pause",
            &binding,
            json!({"kind":"set_outbox_paused","paused":true,"expectedRevision":revision}),
        );
        let before = rig.peer.log().len();
        assert_eq!(
            rig.hub
                .execute(&actor(), &rig.card, pause)
                .await
                .unwrap()
                .state,
            ActionState::Settled
        );
        assert_eq!(rig.peer.log().len(), before);
    }

    #[tokio::test]
    async fn replaced_project_file_allows_original_queue_retraction() {
        let (rig, binding) = replaced_project_file_fixture().await;
        let retract = outbox_control(
            "file_retract",
            &binding,
            json!({"kind":"retract_outbox","targetActionId":"renamed_source"}),
        );
        let before = rig.peer.log().len();
        assert_eq!(
            rig.hub
                .execute(&actor(), &rig.card, retract)
                .await
                .unwrap()
                .state,
            ActionState::Settled
        );
        assert_eq!(
            rig.hub
                .action(DEVICE_ID, "renamed_source")
                .await
                .unwrap()
                .state,
            ActionState::NotSent
        );
        assert_eq!(rig.peer.log().len(), before);
    }

    fn outbox_control(id: &str, binding: &str, operation: Value) -> NativeCommand {
        serde_json::from_value(json!({"actionId":id,"sessionId":SESSION_ID,
            "bindingToken":binding,"operation":operation}))
        .unwrap()
    }

    #[tokio::test]
    async fn renamed_offline_outbox_is_readable_without_native_requests() {
        let (rig, _) = renamed_outbox_fixture().await;
        let before = rig.peer.log().len();
        let view = rig.hub.outbox_view(&rig.card).unwrap();
        assert_eq!(view.items.len(), 1);
        assert_eq!(view.items[0].action_id, "renamed_source");
        assert_eq!(rig.peer.log().len(), before);
    }

    #[tokio::test]
    async fn renamed_offline_binding_can_pause_without_native_requests() {
        let (rig, binding) = renamed_outbox_fixture().await;
        let revision = rig.hub.outbox_view(&rig.card).unwrap().revision;
        let cmd = outbox_control(
            "renamed_pause",
            &binding,
            json!({"kind":"set_outbox_paused","paused":true,"expectedRevision":revision}),
        );
        let before = rig.peer.log().len();
        let result = rig.hub.execute(&actor(), &rig.card, cmd).await;
        assert_eq!(rig.peer.log().len(), before);
        assert_eq!(
            result.expect("原绑定应能暂停本地队列").state,
            ActionState::Settled
        );
    }

    #[tokio::test]
    async fn renamed_offline_binding_can_retract_own_queued_item_without_native_requests() {
        let (rig, binding) = renamed_outbox_fixture().await;
        let cmd = outbox_control(
            "renamed_retract",
            &binding,
            json!({"kind":"retract_outbox","targetActionId":"renamed_source"}),
        );
        let before = rig.peer.log().len();
        let result = rig.hub.execute(&actor(), &rig.card, cmd).await;
        assert_eq!(rig.peer.log().len(), before);
        assert_eq!(
            result.expect("原绑定应能撤回本地待发").state,
            ActionState::Settled
        );
        assert_eq!(
            rig.hub
                .action(DEVICE_ID, "renamed_source")
                .await
                .unwrap()
                .state,
            ActionState::NotSent
        );
    }

    #[tokio::test]
    async fn renamed_offline_binding_cannot_resume_or_rebind() {
        let (rig, binding) = renamed_outbox_fixture().await;
        let revision = rig.hub.outbox_view(&rig.card).unwrap().revision;
        let resume = outbox_control(
            "renamed_resume",
            &binding,
            json!({"kind":"set_outbox_paused","paused":false,"expectedRevision":revision}),
        );
        let rebind = outbox_control(
            "renamed_rebind",
            &binding,
            json!({"kind":"rebind_outbox","targetActionId":"renamed_source"}),
        );
        assert!(rig.hub.execute(&actor(), &rig.card, resume).await.is_err());
        assert!(rig.hub.execute(&actor(), &rig.card, rebind).await.is_err());
        assert_eq!(
            rig.hub
                .action(DEVICE_ID, "renamed_source")
                .await
                .unwrap()
                .state,
            ActionState::Queued
        );
        assert_eq!(rig.peer.non_get(), 0);
    }

    #[tokio::test]
    async fn same_session_other_directory_cannot_rebind_plaintext_queue() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding_a = current_binding(&rig).await;
        rig.hub.store().with(|c| {
            assert!(
                crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                    c,
                    SESSION_ID,
                    &rig.project.canonicalize().unwrap().to_string_lossy(),
                    true,
                    0,
                )
                .unwrap()
            );
        });
        assert_eq!(
            rig.hub
                .execute(
                    &actor(),
                    &rig.card,
                    send_cmd("directory_a_source", &binding_a, "A private text")
                )
                .await
                .unwrap()
                .state,
            ActionState::Queued
        );
        let b = rig._tmp.path().join("project-b");
        std::fs::create_dir(&b).unwrap();
        let card_b = card_for(SESSION_ID, &b);
        rig.peer
            .set(|world| world.directory = b.to_string_lossy().into_owned());
        let binding_b = rig.hub.view(&card_b).await.unwrap().binding_token;
        assert_ne!(binding_a, binding_b);
        // 模拟 B 已经拥有同 session 的持久路由；A 的旧队列保留在账本中。
        rig.hub.store().with(|conn| {
            conn.execute(
                "UPDATE opencode_native_routes SET directory=?1 WHERE session_id=?2",
                rusqlite::params![b.canonicalize().unwrap().to_string_lossy(), SESSION_ID],
            )
            .unwrap();
        });
        assert!(rig.hub.outbox_view(&card_b).unwrap().items.is_empty());
        let cmd = outbox_control(
            "directory_b_rebind",
            &binding_b,
            json!({"kind":"rebind_outbox","targetActionId":"directory_a_source"}),
        );
        let result = rig.hub.execute(&actor(), &card_b, cmd).await;
        assert!(
            !matches!(result, Ok(ref view) if matches!(view.state, ActionState::Queued | ActionState::Confirmed | ActionState::Acknowledged))
        );
        assert_eq!(
            rig.hub
                .action(DEVICE_ID, "directory_a_source")
                .await
                .unwrap()
                .state,
            ActionState::Queued
        );
        assert_eq!(rig.hub.outbox_view(&rig.card).unwrap().items.len(), 1);
        assert_eq!(rig.peer.prompt_posts(), 0);
    }

    #[tokio::test]
    async fn unverified_ownership_cannot_resume_or_rebind_queued_item() {
        let rig = boot_verified(World::new()).await;
        claim_route_verified(&rig);
        let binding = current_binding_verified(&rig).await;
        let command = send_cmd("unverified_source", &binding, "保留待发");
        rig.hub
            .action_store()
            .reserve(DEVICE_ID, &bound_target(&rig.project), &command)
            .unwrap();
        let NativeOperation::Send(send) = &command.operation else {
            unreachable!()
        };
        rig.hub.store().with(|conn| {
            assert!(crate::database::dao::opencode_outbox::enqueue_frozen_conn(
                conn,
                &command,
                &[],
                send,
                None,
                None
            )
            .unwrap());
        });
        let revision = rig.hub.outbox_view(&rig.card).unwrap().revision;
        let resume = outbox_control(
            "unverified_resume",
            &binding,
            json!({"kind":"set_outbox_paused","paused":false,"expectedRevision":revision}),
        );
        let rebind = outbox_control(
            "unverified_rebind",
            &binding,
            json!({"kind":"rebind_outbox","targetActionId":"unverified_source"}),
        );
        assert_eq!(
            rig.hub
                .execute(&actor(), &rig.card, resume)
                .await
                .unwrap_err()
                .code,
            codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED
        );
        assert_eq!(
            rig.hub
                .execute(&actor(), &rig.card, rebind)
                .await
                .unwrap_err()
                .code,
            codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED
        );
        assert_eq!(
            rig.hub
                .action(DEVICE_ID, "unverified_source")
                .await
                .unwrap()
                .state,
            ActionState::Queued
        );
        assert_eq!(rig.peer.non_get(), 0);
    }

    #[tokio::test]
    async fn orphan_legacy_queue_is_not_automatically_dispatched() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        rig.hub.store().with(|conn| {
            conn.execute("INSERT INTO opencode_outbox(action_id,session_id,command_json,text_summary,attachment_count) VALUES ('orphan_legacy',?1,'legacy','legacy',0)", [SESSION_ID]).unwrap();
        });
        rig.hub.flush_outbox(&rig.card).await.unwrap();
        assert_eq!(rig.peer.non_get(), 0);
        rig.hub.store().with(|conn| {
            let payload: String = conn
                .query_row(
                    "SELECT command_json FROM opencode_outbox WHERE action_id='orphan_legacy'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(payload, "legacy");
        });
    }

    #[tokio::test]
    async fn opencode_outbox_explicit_rebind_replaces_only_unclaimed_owned_intent() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        rig.hub.store().with(|c| {
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                true,
                0,
            )
            .unwrap()
        });
        rig.hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("old_bound", &binding, "重新确认同一意图"),
            )
            .await
            .unwrap();
        // 持久目标代际过期，不能自动 flush。
        rig.hub.store().with(|c|c.execute("UPDATE opencode_native_actions SET runtime_pid=runtime_pid+1 WHERE action_id='old_bound'",[]).unwrap());
        let command:NativeCommand=serde_json::from_value(json!({"actionId":"rebound_send","sessionId":SESSION_ID,"bindingToken":binding,"operation":{"kind":"rebind_outbox","targetActionId":"old_bound"}})).unwrap();
        let result = rig
            .hub
            .execute(&actor(), &rig.card, command.clone())
            .await
            .unwrap();
        assert_eq!(result.state, ActionState::Queued);
        assert_eq!(
            rig.hub.action(DEVICE_ID, "old_bound").await.unwrap().state,
            ActionState::NotSent
        );
        rig.hub.execute(&actor(), &rig.card, command).await.unwrap();
        rig.hub.store().with(|c| {
            let (r, _) = crate::database::dao::opencode_outbox::target_state_conn(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
            )
            .unwrap();
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                false,
                r,
            )
            .unwrap();
        });
        rig.hub.flush_outbox(&rig.card).await.unwrap();
        assert_eq!(rig.peer.prompt_posts(), 1);
        assert_eq!(
            rig.hub
                .action(DEVICE_ID, "rebound_send")
                .await
                .unwrap()
                .state,
            ActionState::Confirmed
        );
    }

    #[tokio::test]
    async fn opencode_outbox_old_generation_stays_queued_with_rebind_reason() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        rig.hub.store().with(|c| {
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                true,
                0,
            )
            .unwrap()
        });
        rig.hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("old_generation", &binding, "待重新确认"),
            )
            .await
            .unwrap();
        rig.hub.store().with(|c|{c.execute("UPDATE opencode_native_actions SET runtime_pid=999 WHERE action_id='old_generation'",[]).unwrap();let(r,_)=crate::database::dao::opencode_outbox::target_state_conn(c, SESSION_ID, &rig.project.canonicalize().unwrap().to_string_lossy()).unwrap();crate::database::dao::opencode_outbox::set_paused_target_in_transaction(c, SESSION_ID, &rig.project.canonicalize().unwrap().to_string_lossy(),false,r).unwrap();});
        assert!(rig.hub.flush_outbox(&rig.card).await.is_err());
        let snapshot = rig.hub.outbox_view(&rig.card).unwrap();
        assert_eq!(snapshot.items[0].state, ActionState::Queued);
        assert_eq!(
            snapshot.items[0].reason_code.as_deref(),
            Some(codes::OC_BINDING_STALE)
        );
        assert_eq!(rig.peer.prompt_posts(), 0);
    }

    #[tokio::test]
    async fn opencode_outbox_stop_restart_rejects_old_dispatch_epoch() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        rig.hub.store().with(|c| {
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                true,
                0,
            )
            .unwrap()
        });
        rig.hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("epoch_send", &binding, "一次派发"),
            )
            .await
            .unwrap();
        rig.hub.store().with(|c| {
            let (r, _) = crate::database::dao::opencode_outbox::target_state_conn(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
            )
            .unwrap();
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                false,
                r,
            )
            .unwrap();
        });
        let old = rig
            .hub
            .outbox_runtime()
            .epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        rig.hub.pause();
        // 后台消费者会从持久队列恢复空看板上的会话，先阻挡它以隔离代际断言。
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let blocked = std::sync::Mutex::new(blocked);
        let consumer_entered = entered.clone();
        rig.hub.start_outbox(std::sync::Arc::new(move || {
            consumer_entered.notify_one();
            let _ = blocked.lock().unwrap().recv();
            Vec::new()
        }));
        tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified())
            .await
            .expect("后台消费者须到达屏障");
        rig.hub.flush_outbox_at_epoch(&rig.card, old).await.unwrap();
        assert_eq!(rig.peer.prompt_posts(), 0);
        rig.hub.flush_outbox(&rig.card).await.unwrap();
        assert_eq!(rig.peer.prompt_posts(), 1);
        rig.hub.pause();
        release.send(()).unwrap();
    }

    #[tokio::test]
    async fn opencode_outbox_control_failure_rolls_back_claim_and_effect() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        rig.hub.store().with(|c|c.execute_batch("CREATE TRIGGER fail_pause BEFORE UPDATE ON opencode_outbox_targets BEGIN SELECT RAISE(ABORT,'failure'); END; CREATE TRIGGER fail_pause_insert BEFORE INSERT ON opencode_outbox_targets BEGIN SELECT RAISE(ABORT,'failure'); END;").unwrap());
        let command:NativeCommand=serde_json::from_value(json!({"actionId":"atomic_pause","sessionId":SESSION_ID,"bindingToken":binding,"operation":{"kind":"set_outbox_paused","paused":true,"expectedRevision":0}})).unwrap();
        assert!(rig
            .hub
            .execute(&actor(), &rig.card, command.clone())
            .await
            .is_err());
        assert_eq!(
            rig.hub
                .action(DEVICE_ID, "atomic_pause")
                .await
                .unwrap()
                .state,
            ActionState::NotSent
        );
        rig.hub.store().with(|c| {
            c.execute_batch("DROP TRIGGER fail_pause; DROP TRIGGER fail_pause_insert;")
                .unwrap()
        });
        assert_eq!(
            rig.hub
                .execute(&actor(), &rig.card, command)
                .await
                .unwrap()
                .state,
            ActionState::Settled
        );
        assert_eq!(rig.peer.non_get(), 0);
    }

    #[tokio::test]
    async fn opencode_outbox_rebind_racing_dispatch_has_one_sender() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        rig.hub.store().with(|c| {
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                true,
                0,
            )
            .unwrap()
        });
        rig.hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("race_original", &binding, "一次派发"),
            )
            .await
            .unwrap();
        rig.hub.store().with(|c| {
            let (r, _) = crate::database::dao::opencode_outbox::target_state_conn(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
            )
            .unwrap();
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                false,
                r,
            )
            .unwrap();
        });
        let command:NativeCommand=serde_json::from_value(json!({"actionId":"race_rebind","sessionId":SESSION_ID,"bindingToken":binding,"operation":{"kind":"rebind_outbox","targetActionId":"race_original"}})).unwrap();
        let actor = actor();
        let _ = tokio::join!(
            rig.hub.flush_outbox(&rig.card),
            rig.hub.execute(&actor, &rig.card, command)
        );
        rig.hub.flush_outbox(&rig.card).await.unwrap();
        assert_eq!(rig.peer.prompt_posts(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn opencode_outbox_delayed_recheck_failure_retains_confirmed_intent() {
        let rig = boot(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let path = controlled_attachment(&rig.project, "retained.txt", 1);
        rig.hub.store().with(|c| {
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                true,
                0,
            )
            .unwrap()
        });
        rig.hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd_with_attachments("retained_send", &binding, "保存的意图", vec![path]),
            )
            .await
            .unwrap();
        let frozen: super::super::outbox::FrozenSend = rig.hub.store().with(|c| {
            let json: String = c
                .query_row(
                    "SELECT command_json FROM opencode_outbox WHERE action_id='retained_send'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            serde_json::from_str(&json).unwrap()
        });
        std::fs::remove_file(&frozen.attachment_paths[0]).unwrap();
        rig.hub.store().with(|c| {
            let (r, _) = crate::database::dao::opencode_outbox::target_state_conn(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
            )
            .unwrap();
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                false,
                r,
            )
            .unwrap();
        });
        assert!(rig.hub.flush_outbox(&rig.card).await.is_err());
        assert_eq!(
            rig.hub
                .action(DEVICE_ID, "retained_send")
                .await
                .unwrap()
                .state,
            ActionState::Queued
        );
        assert_eq!(rig.peer.prompt_posts(), 0);
    }

    #[tokio::test]
    async fn opencode_outbox_background_results_verify_without_second_prompt() {
        for (mode, expected) in [
            (PromptMode::IngestedThenDropped, ActionState::Unconfirmed),
            (PromptMode::Queued, ActionState::Acknowledged),
        ] {
            let mut world = World::new();
            world.prompt = mode;
            let rig = boot(world).await;
            claim_route(&rig);
            let binding = current_binding(&rig).await;
            rig.hub.store().with(|c| {
                crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                    c,
                    SESSION_ID,
                    &rig.project.canonicalize().unwrap().to_string_lossy(),
                    true,
                    0,
                )
                .unwrap()
            });
            let queued = rig
                .hub
                .execute(
                    &actor(),
                    &rig.card,
                    send_cmd("background_verify", &binding, "逐项核对"),
                )
                .await
                .unwrap();
            assert_eq!(queued.state, ActionState::Queued);
            rig.hub.store().with(|c| {
                let (r, _) = crate::database::dao::opencode_outbox::target_state_conn(
                    c,
                    SESSION_ID,
                    &rig.project.canonicalize().unwrap().to_string_lossy(),
                )
                .unwrap();
                crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                    c,
                    SESSION_ID,
                    &rig.project.canonicalize().unwrap().to_string_lossy(),
                    false,
                    r,
                )
                .unwrap();
            });
            rig.hub.flush_outbox(&rig.card).await.unwrap();
            assert_eq!(
                rig.hub.outbox_view(&rig.card).unwrap().items[0].state,
                expected
            );
            assert_eq!(rig.peer.prompt_posts(), 1);
            // 无界面订阅/重复后台循环不能重投未知或已受理项。
            rig.hub.flush_outbox(&rig.card).await.unwrap();
            let reads_before = rig.peer.total();
            assert_eq!(
                rig.hub
                    .verify("another-device", "background_verify")
                    .await
                    .unwrap_err()
                    .code,
                action_codes::NOT_FOUND
            );
            assert_eq!(rig.peer.total(), reads_before, "属主检查先于任何原生读取");
            if expected == ActionState::Acknowledged {
                assert_eq!(
                    rig.hub
                        .verify(DEVICE_ID, "background_verify")
                        .await
                        .unwrap()
                        .state,
                    ActionState::Acknowledged
                );
            }
            rig.peer.set(|world| {
                world.inbox.clear();
                world.messages = vec![user_record("msg_background_verify", "逐项核对")];
            });
            let verified = rig
                .hub
                .verify(DEVICE_ID, "background_verify")
                .await
                .unwrap();
            assert_eq!(verified.state, ActionState::Confirmed);
            assert_eq!(
                rig.hub.outbox_view(&rig.card).unwrap().items[0].state,
                ActionState::Confirmed
            );
            assert_eq!(rig.peer.prompt_posts(), 1, "核对不重发原消息");
            assert_eq!(rig.peer.non_get(), 1, "除原消息外没有任何原生写请求");
        }
    }

    // ==================== R2：统一普通启动与显式连接的写入核对 ====================
    //
    // `boot_verified` 走 `NativeHub::with_sources` 的生产判定路径，没有 TUI 归属
    // 证明源；`boot_r2(..., true)` 仅用于协调器测试。R0 服务身份的三路证据可控：
    //   1. 登记文件由测试写（死 pid / 换代变体由 `Peer::restart` 重写）；
    //   2. 进程事实由闭包给（`alive` 开关模拟 SIGKILL 形态：登记 pid 拿不到启动时间）；
    //   3. `/api/info` 由假 peer 回（`info_pid` 覆盖模拟同端口冒名的信息不一致）。
    // 生产路径用例不跳过归属门；协调器注入既不证明生产归属，也不证明身份复核。

    const SESSION_B_ID: &str = "ses_actions_fixture_b";
    /// 显式形态夹具的**客户端** TUI pid（argv 证据与环境凭据的持有者）。
    const EXPLICIT_CLIENT_PID: u32 = PID + 510;
    /// 显式形态夹具的**接收服务** pid（`/api/info` 自报；与客户端 PID 不同——
    /// R0 §4d：显式连接是「独立 TUI 客户端 → 独立服务」，两个进程）。
    const EXPLICIT_SERVICE_PID: u32 = PID + 777;

    /// 可控进程事实：登记 pid 仅在 `alive` 为真时可读启动时间（SIGKILL 后拿不到）。
    fn probe_fact(alive: Arc<AtomicBool>) -> Arc<dyn Fn(u32) -> Option<i64> + Send + Sync> {
        Arc::new(move |pid: u32| {
            if pid == PID {
                return alive.load(Ordering::SeqCst).then_some(STARTED_AT_MS);
            }
            Some(STARTED_AT_MS + 60_000)
        })
    }

    struct VerifiedRig {
        _tmp: tempfile::TempDir,
        project: PathBuf,
        /// 第二会话目录（桌面切换模型）：由 peer world 持有，rig 只留路径备断言。
        #[allow(dead_code)]
        project_b: PathBuf,
        card: Session,
        card_b: Session,
        peer: Peer,
        hub: Arc<NativeHub>,
        /// 服务进程存活开关（只影响登记 pid 的进程事实；换代后新 pid 恒可读）。
        alive: Arc<AtomicBool>,
        /// 多 service 冲突探针开关（确定性注入生产闸的**环境读取**，不改判定分支）。
        conflict: Arc<AtomicBool>,
    }

    /// R2 生产判定夹具：证据链全绿的普通启动（无显式启动参数、无桥、无桌面绑定进程）。
    async fn boot_verified(world: World) -> VerifiedRig {
        boot_r2(world, false, false).await
    }

    // 仅注入归属事实；不代表真实 TUI 的连接证据。
    async fn boot_with_test_ownership(world: World) -> VerifiedRig {
        boot_r2(world, false, true).await
    }

    // coordinator_only 绕过整个身份门，仅用于隔离协调器逻辑。
    async fn boot_r2(
        mut world: World,
        coordinator_only: bool,
        ownership_evidence: bool,
    ) -> VerifiedRig {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path().join("project");
        let project_b = tmp.path().join("project-b");
        std::fs::create_dir_all(&project).expect("建项目目录");
        std::fs::create_dir_all(&project_b).expect("建第二项目目录");
        let project = project.canonicalize().expect("归一项目目录");
        let project_b = project_b.canonicalize().expect("归一第二项目目录");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("建 home");
        let state_file = tmp.path().join("state/opencode/service.json");
        world.directory = project.to_string_lossy().into_owned();
        world.second = Some((
            SESSION_B_ID.to_string(),
            project_b.to_string_lossy().into_owned(),
        ));
        let card = card_for(SESSION_ID, &project);
        let card_b = card_for(SESSION_B_ID, &project_b);
        let peer = Peer::start(world, &state_file).await;
        write_registration(&state_file, peer.port);
        let alive = Arc::new(AtomicBool::new(true));
        let conflict = Arc::new(AtomicBool::new(false));
        let conflict_probe = Arc::clone(&conflict);
        let input = DiscoveryInput { state_file, home };
        let hub = if coordinator_only {
            NativeHub::with_coordinator_sources(
                DeviceStore::memory(),
                input,
                probe_fact(Arc::clone(&alive)),
            )
        } else {
            NativeHub::with_sources(
                DeviceStore::memory(),
                input,
                probe_fact(Arc::clone(&alive)),
                Some(
                    Arc::new(move |_dir: &std::path::Path| conflict_probe.load(Ordering::SeqCst))
                        as Arc<dyn Fn(&std::path::Path) -> bool + Send + Sync>,
                ),
                None,
            )
        };
        let hub = Arc::new(if ownership_evidence {
            hub.with_test_ownership_evidence()
        } else {
            hub
        });
        hub.store().with(|c| {
            crate::remote::pairing::persist_device(
                c,
                &crate::remote::pairing::NewDevice {
                    id: DEVICE_ID.into(),
                    name: DEVICE_NAME.into(),
                    ua: "fixture".into(),
                    origin_ip: "127.0.0.1".into(),
                    via: "lan".into(),
                    paired_at: now_ms(),
                },
            )
            .unwrap()
        });
        VerifiedRig {
            _tmp: tmp,
            project,
            project_b,
            card,
            card_b,
            peer,
            hub,
            alive,
            conflict,
        }
    }

    /// 认领路由（原生写入入口）的 R2 夹具变体。
    fn claim_route_verified(rig: &VerifiedRig) {
        assert!(
            rig.hub
                .action_store()
                .try_claim_route(&bound_target(&rig.project), true)
                .expect("认领路由"),
            "测试装配必须先拥有写入入口"
        );
    }

    /// 现取当前绑定令牌（R2 夹具变体）。
    async fn current_binding_verified(rig: &VerifiedRig) -> String {
        rig.hub
            .view(&rig.card)
            .await
            .expect("首轮投影")
            .binding_token
    }

    /// 动作账本行数（R2 夹具变体）。
    fn ledger_rows_verified(rig: &VerifiedRig) -> i64 {
        rig.hub.store().with(|conn| {
            conn.query_row("SELECT COUNT(*) FROM opencode_native_actions", [], |row| {
                row.get(0)
            })
            .unwrap()
        })
    }

    // 服务身份和共享数据库中的会话均可读，但夹具没有任何目标 TUI 连接证据。
    #[tokio::test]
    async fn ownership_unverified_direct_send_refuses_without_reserving_or_writing() {
        let rig = boot_verified(World::new()).await;
        claim_route_verified(&rig);
        let binding = current_binding_verified(&rig).await;
        let result = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("ownership_direct", &binding, "不能凭共享会话写入"),
            )
            .await;
        assert_eq!(rig.peer.non_get(), 0, "归属未证不得向服务写入");
        assert_eq!(ledger_rows_verified(&rig), 0, "拒写不得登记新动作");
        assert_eq!(
            result.unwrap_err().code,
            codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED
        );
    }

    #[tokio::test]
    async fn ownership_unverified_background_flush_preserves_queued_action() {
        let rig = boot_verified(World::new()).await;
        claim_route_verified(&rig);
        let binding = current_binding_verified(&rig).await;
        let command = send_cmd("ownership_queued", &binding, "保留已有待发");
        rig.hub
            .action_store()
            .reserve(DEVICE_ID, &bound_target(&rig.project), &command)
            .unwrap();
        let NativeOperation::Send(send) = &command.operation else {
            unreachable!();
        };
        rig.hub.store().with(|conn| {
            assert!(crate::database::dao::opencode_outbox::enqueue_frozen_conn(
                conn,
                &command,
                &[],
                send,
                None,
                None,
            )
            .unwrap());
        });
        let result = rig.hub.flush_outbox(&rig.card).await;
        assert_eq!(rig.peer.non_get(), 0, "归属未证不得向服务写入");
        let action = rig.hub.action(DEVICE_ID, "ownership_queued").await.unwrap();
        assert_eq!(action.state, ActionState::Queued, "已有动作必须保留待发");
        let outbox = rig.hub.outbox_view(&rig.card).unwrap();
        assert_eq!(outbox.items.len(), 1, "队列项必须保留");
        assert_eq!(
            result.unwrap_err().code,
            codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED
        );
    }

    /// 服务身份一致但缺少 TUI 连接证据：公开只读，不能认领或发送。
    #[tokio::test]
    async fn ordinary_tui_without_connection_evidence_is_read_only() {
        let rig = boot_verified(World::new()).await;
        let (view, claim) = rig.hub.view_and_claim(&rig.card, true).await.unwrap();
        assert!(!claim.owned);
        assert_eq!(
            claim.declined_reason,
            Some(codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED)
        );
        assert!(view.binding_required);
        assert_eq!(view.service_pid, Some(PID));
        // 公开能力反映当前归属状态下的实际可操作性。
        let send = view.capabilities.get(&NativeOperationId::Send).unwrap();
        assert!(!send.allowed);
        assert_eq!(
            send.reason_code.as_deref(),
            Some(codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED)
        );
        let err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_r2_plain", &view.binding_token, "普通启动直发"),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED);
        assert_eq!(ledger_rows_verified(&rig), 0);
        assert_eq!(rig.peer.non_get(), 0);
    }

    /// 手机已选 A（动作与待答都归属 A），桌面切到 B 后：动作不写 B，
    /// 动作 id 与待发归属原样，A 的待答令牌不漂移。
    #[tokio::test]
    async fn changed_window_selection_does_not_retarget_action() {
        let mut world = World::new();
        world.forms = vec![form_entry()];
        // 协调器隔离测试：注入绕过身份门，不证明生产归属或真实桌面切换。
        let rig = boot_r2(world, true, false).await;
        claim_route_verified(&rig);
        let binding = current_binding_verified(&rig).await;
        // 手机已选 A：暂停本地派发后确认发送只入队（归属 A 的本地待发）。
        rig.hub.store().with(|c| {
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                true,
                0,
            )
            .unwrap()
        });
        let queued = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_r2_stay", &binding, "发给 A"),
            )
            .await
            .expect("已验证目标在暂停期只入队");
        assert_eq!(queued.state, ActionState::Queued);
        assert_eq!(rig.peer.prompt_posts(), 0);
        // 桌面切到 B：B 的卡片投影并认领（hub 可见的「B 成为当前窗口」类比；
        // R0 已证 hub 观察不到桌面窗口本身，任何以「当前窗口」重定向目标的实现都会在此翻车）。
        let (_, claim_b) = rig
            .hub
            .view_and_claim(&rig.card_b, true)
            .await
            .expect("B 投影可认领");
        assert!(claim_b.owned);
        // A 的动作与待答原样：动作行仍是 A、仍是 queued；A 的待答令牌不漂移。
        let row = rig.hub.action(DEVICE_ID, "act_r2_stay").await.unwrap();
        assert_eq!(row.state, ActionState::Queued);
        let a_view = rig.hub.view(&rig.card).await.expect("A 投影");
        assert_eq!(a_view.pending.len(), 1, "A 的待答仍在 A");
        // 恢复 A 的队列派发：必须仍写 A，绝不写 B。
        rig.hub.store().with(|c| {
            let (revision, _) = crate::database::dao::opencode_outbox::target_state_conn(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
            )
            .unwrap();
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                false,
                revision,
            )
            .unwrap();
        });
        rig.hub
            .flush_outbox(&rig.card)
            .await
            .expect("A 队列恢复派发");
        assert_eq!(
            rig.peer
                .count("POST", &format!("/api/session/{SESSION_ID}/prompt")),
            1,
            "动作只写它 reserve 时的目标 A"
        );
        assert_eq!(
            rig.peer
                .count("POST", &format!("/api/session/{SESSION_B_ID}/prompt")),
            0,
            "桌面切到 B 后不得有任何写落到 B"
        );
    }

    /// 已有队列在服务进程死亡后仍保持待发，不依赖未证归属时的新入队。
    #[tokio::test]
    async fn dead_service_preserves_preexisting_queued_action() {
        let rig = boot_verified(World::new()).await;
        claim_route_verified(&rig);
        let binding = current_binding_verified(&rig).await;
        let command = send_cmd("r2_dead_queued", &binding, "已有待发");
        rig.hub
            .action_store()
            .reserve(DEVICE_ID, &bound_target(&rig.project), &command)
            .unwrap();
        let NativeOperation::Send(send) = &command.operation else {
            unreachable!();
        };
        rig.hub.store().with(|conn| {
            assert!(crate::database::dao::opencode_outbox::enqueue_frozen_conn(
                conn,
                &command,
                &[],
                send,
                None,
                None,
            )
            .unwrap());
        });
        rig.alive.store(false, Ordering::SeqCst);
        let err = rig.hub.flush_outbox(&rig.card).await.unwrap_err();
        assert_eq!(err.code, codes::OC_RUNTIME_UNVERIFIED);
        let action = rig.hub.action(DEVICE_ID, "r2_dead_queued").await.unwrap();
        assert_eq!(action.state, ActionState::Queued);
        assert_eq!(
            action.reason_code.as_deref(),
            Some(codes::OC_RUNTIME_UNVERIFIED)
        );
        assert_eq!(rig.hub.outbox_view(&rig.card).unwrap().items.len(), 1);
        assert_eq!(rig.peer.non_get(), 0);
    }

    /// 换代/死 pid 在三条路径上都拦住新写：首次直调、暂停队列恢复、
    /// 已 reserve 未 claim 的残留行重放；已可能写出的动作仍可只读核对，
    /// 重复请求不因绑定失效被误判「确定未写」。
    #[tokio::test]
    async fn old_generation_or_wrong_owner_blocks_before_claim() {
        // 相位一：首次直调。服务死亡（SIGKILL 滞留死 pid 登记）⇒ 拒绝走证据链的具体码，
        // 零动作行、零对端写。旧的一刀切门（OC_INSTANCE_OWNERSHIP_UNVERIFIED）不算数。
        let rig = boot_verified(World::new()).await;
        claim_route_verified(&rig);
        let binding = current_binding_verified(&rig).await;
        rig.alive.store(false, Ordering::SeqCst);
        let err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_r2_p1", &binding, "死 pid 直调"),
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.code,
            codes::OC_RUNTIME_UNVERIFIED,
            "首次直调必须由证据链（进程事实拿不到）拒绝，而不是一刀切归属门"
        );
        assert_eq!(ledger_rows_verified(&rig), 0, "未获准请求不得产生新动作行");
        assert_eq!(rig.peer.non_get(), 0, "对端写次数为零");

        // 相位二：注入测试归属事实，恢复派发前服务死亡 ⇒ claim 之前拒绝。
        // 注入不证明真实 TUI 归属；已排队项原样保留、零写出。
        let rig = boot_with_test_ownership(World::new()).await;
        claim_route_verified(&rig);
        let binding = current_binding_verified(&rig).await;
        rig.hub.store().with(|c| {
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                true,
                0,
            )
            .unwrap()
        });
        let queued = rig
            .hub
            .execute(&actor(), &rig.card, send_cmd("act_r2_p2", &binding, "入队"))
            .await
            .expect("已验证目标在暂停期只入队");
        assert_eq!(queued.state, ActionState::Queued);
        rig.alive.store(false, Ordering::SeqCst);
        rig.hub.store().with(|c| {
            let (revision, _) = crate::database::dao::opencode_outbox::target_state_conn(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
            )
            .unwrap();
            crate::database::dao::opencode_outbox::set_paused_target_in_transaction(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
                false,
                revision,
            )
            .unwrap();
        });
        let flush_err = rig.hub.flush_outbox(&rig.card).await.unwrap_err();
        assert_eq!(
            flush_err.code,
            codes::OC_RUNTIME_UNVERIFIED,
            "暂停队列恢复必须在 claim 之前由同一判定拒绝"
        );
        let row = rig.hub.action(DEVICE_ID, "act_r2_p2").await.unwrap();
        assert_eq!(row.state, ActionState::Queued, "已排队项原样保留");
        assert_eq!(
            row.reason_code.as_deref(),
            Some(codes::OC_RUNTIME_UNVERIFIED)
        );
        assert_eq!(rig.peer.prompt_posts(), 0, "对端写次数为零");

        // 相位三：reserve 与 claim 之间失效（真实交错）。占住同会话派发锁，让 execute
        // 完成初始核验与 reserve、停在 dispatch 的锁等待上；此刻服务死亡（SIGKILL），
        // 放行后 claim 前复核必须拦住——行已 reserve 但从未取得写出权，零写出。
        // （dispatch 含会话锁等待与操作特有读取等 await 点，锁窗口是可复现的交错缝。）
        let rig = boot_with_test_ownership(World::new()).await;
        claim_route_verified(&rig);
        let binding = current_binding_verified(&rig).await;
        let dispatch_lock = rig
            .hub
            .outbox_runtime()
            .session_lock(SESSION_ID)
            .lock_owned()
            .await;
        let hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let fixture_actor = actor();
        let task_binding = binding.clone();
        let task = tokio::spawn(async move {
            hub.execute(
                &fixture_actor,
                &card,
                send_cmd("act_r2_p3", &task_binding, "reserve 后失效"),
            )
            .await
        });
        let mut reserved = false;
        for _ in 0..400 {
            if rig.hub.action(DEVICE_ID, "act_r2_p3").await.is_ok() {
                reserved = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(reserved, "execute 必须先完成 reserve 再停在派发锁上");
        rig.alive.store(false, Ordering::SeqCst);
        drop(dispatch_lock);
        let err = task.await.unwrap().unwrap_err();
        assert_eq!(
            err.code,
            codes::OC_RUNTIME_UNVERIFIED,
            "claim 前复核必须拦住 reserve 之后的失效，不得走任何写路径"
        );
        let row = rig.hub.action(DEVICE_ID, "act_r2_p3").await.unwrap();
        assert_eq!(row.state, ActionState::NotSent, "动作从未取得写出权");
        assert_eq!(
            row.reason_code.as_deref(),
            Some(super::abandon_reasons::IDENTITY_UNVERIFIED),
            "墓碑记身份未证理由，不得留无理由未决行"
        );
        assert_eq!(rig.peer.prompt_posts(), 0, "对端写次数为零");
        // 已可能写出的动作（unconfirmed，可能已写出）在绑定失效后仍可只读核对：
        // 核对不改判（读不到证据就保持未知）、重复请求不得把它冲成「确定未写」。
        let may_write = send_cmd("act_r2_may_write", &binding, "可能已写出");
        rig.hub
            .action_store()
            .reserve(&actor().device_id, &bound_target(&rig.project), &may_write)
            .expect("装配未决动作");
        rig.hub.store().with(|c| {
            c.execute(
                "UPDATE opencode_native_actions SET state='unconfirmed', \
                 reason_code='readback_failed' WHERE action_id='act_r2_may_write'",
                [],
            )
            .unwrap();
        });
        let verified = rig.hub.verify(DEVICE_ID, "act_r2_may_write").await.unwrap();
        assert_eq!(
            verified.state,
            ActionState::Unconfirmed,
            "绑定失效后核对只读：不得改判成任何确定结论"
        );
        let repeated = rig
            .hub
            .execute(&actor(), &rig.card, may_write.clone())
            .await
            .expect("重复请求应返回原未知动作");
        assert_eq!(repeated, verified);
        let after = rig.hub.action(DEVICE_ID, "act_r2_may_write").await.unwrap();
        assert_eq!(
            after.state,
            ActionState::Unconfirmed,
            "重复请求不得因绑定失效被误判「确定未写」"
        );
        assert_eq!(rig.peer.prompt_posts(), 0, "对端写次数为零");
        assert_eq!(rig.peer.non_get(), 0);
    }

    /// 死 pid 拒写；服务换代恢复只读后仍需证明 TUI 当前连接。
    #[tokio::test]
    async fn dead_pid_registration_refuses_new_writes_until_regeneration() {
        let rig = boot_verified(World::new()).await;
        claim_route_verified(&rig);
        let binding = current_binding_verified(&rig).await;
        // SIGKILL：登记滞留死 pid（干净退出自删、SIGKILL 无清理机会）。换代窗口内
        // 读数必须按「服务不可用」处理：投影与写入都拒绝，不读旧登记硬连。
        rig.alive.store(false, Ordering::SeqCst);
        let view_err = rig.hub.view(&rig.card).await.unwrap_err();
        assert_eq!(view_err.code, codes::OC_RUNTIME_UNVERIFIED);
        let send_err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_r2_dead", &binding, "死 pid 拒写"),
            )
            .await
            .unwrap_err();
        assert_eq!(send_err.code, codes::OC_RUNTIME_UNVERIFIED);
        assert_eq!(ledger_rows_verified(&rig), 0);
        assert_eq!(rig.peer.non_get(), 0);
        // 自愈换代：TUI 约 11 秒拉起新服务（同端口同密码、新 id/pid），登记重写。
        rig.peer.restart();
        let (view, claim) = rig
            .hub
            .view_and_claim(&rig.card, true)
            .await
            .expect("新代际可只读投影");
        assert!(claim.owned, "原有路由归属保留");
        assert_eq!(
            claim.declined_reason,
            Some(codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED)
        );
        assert!(view.binding_required);
        assert_ne!(view.service_pid, Some(PID), "投影必须来自新代际");
        let err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_r2_healed", &view.binding_token, "新代际直发"),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED);
        assert_eq!(ledger_rows_verified(&rig), 0);
        assert_eq!(rig.peer.non_get(), 0);
    }

    /// 显式连接形态（R0 §4d）：无登记文件，证据 = argv `--server`+`--session` +
    /// 进程事实 + 进程环境 `OPENCODE_PASSWORD`；pid/urls 互证规则与普通形态相同。
    async fn boot_explicit(
        world: World,
        second_candidate: bool,
        foreign_urls: bool,
    ) -> VerifiedRig {
        boot_explicit_with_evidence(world, second_candidate, foreign_urls, false).await
    }

    async fn boot_explicit_with_evidence(
        mut world: World,
        second_candidate: bool,
        foreign_urls: bool,
        ownership_evidence: bool,
    ) -> VerifiedRig {
        use crate::services::opencode_native::projection::ExplicitEvidenceSources;
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path().join("project");
        let project_b = tmp.path().join("project-b");
        std::fs::create_dir_all(&project).expect("建项目目录");
        std::fs::create_dir_all(&project_b).expect("建第二项目目录");
        let project = project.canonicalize().expect("归一项目目录");
        let project_b = project_b.canonicalize().expect("归一第二项目目录");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("建 home");
        let state_file = tmp.path().join("state/opencode/service.json");
        world.directory = project.to_string_lossy().into_owned();
        world.second = Some((
            SESSION_B_ID.to_string(),
            project_b.to_string_lossy().into_owned(),
        ));
        // 接收服务 pid：`/api/info` 自报它（peer 世界与显式 info 缝共用同一取值）；
        // 登记文件不存在（该形态不写登记）；客户端 PID 另立（argv/env 的持有者）。
        world.pid = EXPLICIT_SERVICE_PID;
        let card = card_for(SESSION_ID, &project);
        let card_b = card_for(SESSION_B_ID, &project_b);
        let peer = Peer::start(world, &state_file).await;
        let endpoint = format!("http://127.0.0.1:{}", peer.port);
        let argv = vec![
            "/usr/local/bin/opencode".to_string(),
            "--server".to_string(),
            endpoint.clone(),
            "--session".to_string(),
            SESSION_ID.to_string(),
        ];
        let scan: crate::services::opencode_native::ExplicitScan =
            Arc::new(move |session: &str| {
                if session != SESSION_ID {
                    return Vec::new();
                }
                let mut matched = vec![(EXPLICIT_CLIENT_PID, argv.clone())];
                if second_candidate {
                    matched.push((
                        EXPLICIT_CLIENT_PID + 1,
                        vec![
                            "/usr/local/bin/opencode".to_string(),
                            "--server".to_string(),
                            format!("{endpoint}-alt"),
                            "--session".to_string(),
                            SESSION_ID.to_string(),
                        ],
                    ));
                }
                matched
            });
        let env: crate::services::opencode_native::ExplicitEnv = Arc::new(|pid: u32, key: &str| {
            (pid == EXPLICIT_CLIENT_PID && key == "OPENCODE_PASSWORD").then(|| PASSWORD.to_string())
        });
        // 服务身份缝：客户端 PID 不参与——pid 取「服务自报」的接收服务 pid；
        // foreign_urls 模拟「端点上的服务不是 argv 指认的那个」（urls 不含端点 origin）。
        let info_shared = Arc::clone(&peer.shared);
        let foreign = foreign_urls;
        let info: crate::services::opencode_native::ExplicitInfoFetch =
            Arc::new(move |endpoint, _password| {
                let world = info_shared.world.lock().unwrap();
                let pid = world.info_pid.unwrap_or(world.pid);
                Ok(json!({
                    "version": "2.0.22",
                    "pid": pid,
                    "urls": [if foreign {
                        "http://127.0.0.1:1".to_string()
                    } else {
                        endpoint.as_str().to_string()
                    }],
                }))
            });
        let alive = Arc::new(AtomicBool::new(true));
        let conflict = Arc::new(AtomicBool::new(false));
        let conflict_probe = Arc::clone(&conflict);
        let hub = NativeHub::with_sources(
            DeviceStore::memory(),
            DiscoveryInput { state_file, home },
            probe_fact(Arc::clone(&alive)),
            Some(
                Arc::new(move |_dir: &std::path::Path| conflict_probe.load(Ordering::SeqCst))
                    as Arc<dyn Fn(&std::path::Path) -> bool + Send + Sync>,
            ),
            Some(ExplicitEvidenceSources { scan, env, info }),
        );
        // 仅指定用例注入测试事实，不代表真实 TUI 的连接证据。
        let hub = Arc::new(if ownership_evidence {
            hub.with_test_ownership_evidence()
        } else {
            hub
        });
        hub.store().with(|c| {
            crate::remote::pairing::persist_device(
                c,
                &crate::remote::pairing::NewDevice {
                    id: DEVICE_ID.into(),
                    name: DEVICE_NAME.into(),
                    ua: "fixture".into(),
                    origin_ip: "127.0.0.1".into(),
                    via: "lan".into(),
                    paired_at: now_ms(),
                },
            )
            .unwrap()
        });
        VerifiedRig {
            _tmp: tmp,
            project,
            project_b,
            card,
            card_b,
            peer,
            hub,
            alive,
            conflict,
        }
    }

    /// argv 与服务身份不能证明目标 TUI 当前连接，显式形态仍只读。
    #[tokio::test]
    async fn explicit_connection_without_ownership_evidence_is_read_only() {
        let rig = boot_explicit(World::new(), false, false).await;
        assert_ne!(
            EXPLICIT_CLIENT_PID, EXPLICIT_SERVICE_PID,
            "夹具必须用不同的客户端/服务 PID（R0 §4d：两个进程）"
        );
        let (view, claim) = rig
            .hub
            .view_and_claim(&rig.card, true)
            .await
            .expect("显式形态只读投影");
        assert!(!claim.owned);
        assert_eq!(
            claim.declined_reason,
            Some(codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED)
        );
        assert!(view.binding_required);
        assert_eq!(
            view.service_pid,
            Some(EXPLICIT_SERVICE_PID),
            "服务 RuntimeKey 必须是 /api/info 自报的服务 pid，绝非 argv 客户端 pid"
        );
        let err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_r2_explicit", &view.binding_token, "显式连接直发"),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED);
        assert_eq!(ledger_rows_verified(&rig), 0);
        assert_eq!(rig.peer.non_get(), 0);
    }

    /// 显式形态错误实例负例：端点上的接收方不是 argv 指认的进程（info pid 不一致）
    /// ⇒ 与普通形态同规则按代际拒写；多个候选实例 ⇒ 证据不明保守拒绝。
    #[tokio::test]
    async fn explicit_connection_wrong_instance_is_refused() {
        // 负例一：端点上的服务自报 urls 不含 argv 指认端点（错误实例）⇒ 锚定阶段拒绝。
        let rig = boot_explicit(World::new(), false, true).await;
        let view_err = rig.hub.view(&rig.card).await.unwrap_err();
        assert_eq!(
            view_err.code,
            codes::OC_GENERATION_MISMATCH,
            "服务自报 urls 与 argv 端点不一致必须按代际拒读"
        );
        let err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_r2_exp_wrong", "stale-binding", "拒绝"),
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.code,
            codes::OC_GENERATION_MISMATCH,
            "显式形态错误实例必须由证据链拒绝"
        );
        assert_eq!(ledger_rows_verified(&rig), 0);
        assert_eq!(rig.peer.non_get(), 0);

        // 负例二：两个候选客户端实例 ⇒ 无法证明固定执行者。
        let rig2 = boot_explicit(World::new(), true, false).await;
        let ambiguous = rig2.hub.view(&rig2.card).await.unwrap_err();
        assert_eq!(ambiguous.code, codes::OC_INSTANCE_AMBIGUOUS);

        // 负例三：锚定后接收服务换代（同端点新服务 pid）⇒ 新发现重锚新代际，
        // 旧代际令牌的派发被代际/令牌复核拒绝：零写、墓碑保留。
        let rig3 = boot_explicit_with_evidence(World::new(), false, false, true).await;
        let (_, claim) = rig3
            .hub
            .view_and_claim(&rig3.card, true)
            .await
            .expect("首次锚定成功");
        assert!(claim.owned);
        assert_eq!(claim.declined_reason, None);
        let stale_binding = rig3.hub.view(&rig3.card).await.unwrap().binding_token;
        rig3.peer.set(|world| world.info_pid = Some(PID + 888));
        let err = rig3
            .hub
            .execute(
                &actor(),
                &rig3.card,
                send_cmd("act_r2_exp_gen", &stale_binding, "旧代际拒绝"),
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.code,
            codes::OC_BINDING_STALE,
            "接收服务换代后旧代际令牌必须被拒，绝不写向换代后的接收方"
        );
        assert_eq!(rig3.peer.prompt_posts(), 0, "对端写次数为零");
    }

    /// C1：初始核验与 reserve 完成之后、claim 之前，同端口接收方被换成冒名服务
    /// （登记与原 PID 事实仍可读）⇒ 必须对**实际写通道**重读 `/api/info` 证伪：
    /// 零 claim、零原生写、动作归属原样保留。
    #[tokio::test]
    async fn receiver_swap_after_verification_blocks_before_claim() {
        let rig = boot_with_test_ownership(World::new()).await;
        claim_route_verified(&rig);
        let binding = current_binding_verified(&rig).await;
        // 占住同会话派发锁：execute 完成快照核验与 reserve 后停在 dispatch 的锁等待上。
        let dispatch_lock = rig
            .hub
            .outbox_runtime()
            .session_lock(SESSION_ID)
            .lock_owned()
            .await;
        let hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let actor = actor();
        let task = tokio::spawn(async move {
            hub.execute(
                &actor,
                &card,
                send_cmd("act_r2_swap", &binding, "锁窗口冒名"),
            )
            .await
        });
        // 等 reserve 落行（execute 已越过初始核验，此刻停在派发锁上）。
        let mut reserved = false;
        for _ in 0..400 {
            if rig.hub.action(DEVICE_ID, "act_r2_swap").await.is_ok() {
                reserved = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(reserved, "execute 必须先完成 reserve 再停在派发锁上");
        // 锁窗口内同端口接收方被换成冒名服务（登记文件与进程事实都不变）。
        rig.peer.set(|world| world.info_pid = Some(PID + 777));
        drop(dispatch_lock);
        let err = task.await.unwrap().unwrap_err();
        assert_eq!(
            err.code,
            codes::OC_GENERATION_MISMATCH,
            "claim 前必须对实际写通道重读 /api/info，证伪冒名接收方"
        );
        let row = rig.hub.action(DEVICE_ID, "act_r2_swap").await.unwrap();
        assert_eq!(row.state, ActionState::NotSent, "零 claim：从未取得写出权");
        assert_eq!(
            row.reason_code.as_deref(),
            Some(super::abandon_reasons::IDENTITY_UNVERIFIED),
            "墓碑必须记身份未证理由，不得留无理由未决行（受控错误码随响应携带）"
        );
        assert_eq!(rig.peer.prompt_posts(), 0, "零原生写");
    }

    /// 目录/身份证据不明（同机多个可执行 service 在场）：多 service 保守闸在
    /// claim 之前拒绝新写（共享库反例的保守处置，随 R2 门保留）。
    #[tokio::test]
    async fn ambiguous_service_count_refuses_new_writes() {
        let rig = boot_verified(World::new()).await;
        claim_route_verified(&rig);
        let binding = current_binding_verified(&rig).await;
        // 生产判定分支不变，只把环境探针拨到「冲突在场」。
        rig.conflict.store(true, Ordering::SeqCst);
        let err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_r2_ambiguous", &binding, "证据不明拒写"),
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.code,
            codes::OC_INSTANCE_AMBIGUOUS,
            "多 service 在场必须由保守闸拒绝，不得自选执行者"
        );
        assert_eq!(ledger_rows_verified(&rig), 0, "未获准请求不得产生新动作行");
        assert_eq!(rig.peer.non_get(), 0, "对端写次数为零");
        // 冲突消失不能证明目标 TUI 的连接。
        rig.conflict.store(false, Ordering::SeqCst);
        let err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_r2_clear", &binding, "冲突解除仍未证"),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED);
        assert_eq!(ledger_rows_verified(&rig), 0);
        assert_eq!(rig.peer.non_get(), 0);
    }

    /// 同端口冒名：登记滞留、进程事实可读，但 `/api/info` 自报他者 pid
    /// ⇒ 信息与登记 id/pid 不一致，拒新写（端口与 TCP 连接都不是代际证据）。
    #[tokio::test]
    async fn imposter_port_info_mismatch_refuses_new_writes() {
        let rig = boot_verified(World::new()).await;
        claim_route_verified(&rig);
        // 冒名前取到的合法绑定令牌（真实时序：先验证后冒名漂移）。
        let binding = current_binding_verified(&rig).await;
        // 同端口冒名：登记滞留不变，`/api/info` 自报他者 pid。
        rig.peer.set(|world| world.info_pid = Some(PID + 4321));
        let view_err = rig.hub.view(&rig.card).await.unwrap_err();
        assert_eq!(
            view_err.code,
            codes::OC_GENERATION_MISMATCH,
            "info pid 与登记不一致必须按换代拒读"
        );
        let send_err = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_r2_imposter", &binding, "冒名端口拒写"),
            )
            .await
            .unwrap_err();
        assert_eq!(
            send_err.code,
            codes::OC_GENERATION_MISMATCH,
            "同端口冒名必须由证据链（info 与登记不一致）拒绝"
        );
        assert_eq!(ledger_rows_verified(&rig), 0, "未获准请求不得产生新动作行");
        assert_eq!(rig.peer.non_get(), 0, "对端写次数为零");
    }

    // ==== F12 项 1.5（RED d）：窗口记录缺失的后台派发 fail-closed ====

    /// F12 RED d 专用窗口身份（「另一扇打开同一会话的窗口」——旧代码会经新鲜回退
    /// 把它当成锁定窗口取得授权，新代码必须在解析处 fail-closed）。
    const TUI_W2_PID: u32 = 62100;
    const TUI_W2_STARTED_AT_MS: i64 = 1_793_291_000_000;

    /// 装配 echo 门 + 可变「当前新鲜 Opened」观察的 rig（F12 派发 fail-closed 用）。
    /// echo 门生产路径原样（admission → 探针回声 → ACK 终验）；探针面由假服务承担。
    async fn boot_gate_armed(mut world: World) -> Rig {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).expect("建项目目录");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("建 home");
        let state_file = tmp.path().join("state/opencode/service.json");
        let card = card_for(SESSION_ID, &project);
        world.directory = project.to_string_lossy().into_owned();
        let peer = Peer::start(world, &state_file).await;
        write_registration(&state_file, peer.port);
        let no_conflict: Arc<dyn Fn(&std::path::Path) -> bool + Send + Sync> =
            Arc::new(|_dir: &std::path::Path| false);
        let mut hub = NativeHub::with_sources(
            DeviceStore::memory(),
            DiscoveryInput {
                state_file: state_file.clone(),
                home: home.clone(),
            },
            process_fact(),
            Some(no_conflict),
            None,
        );
        // 新鲜观察：恒回「另一扇窗」W2（窗口记录缺失时旧代码经新鲜回退锁它）。
        let provider: crate::services::opencode_native::binding_gate::LockedWindowLookup =
            Arc::new(move |sid: &str| {
                (sid == SESSION_ID).then_some(crate::services::opencode_native::LockedWindow {
                    pid: TUI_W2_PID,
                    started_at_ms: TUI_W2_STARTED_AT_MS,
                    boot: None,
                })
            });
        let sha = Arc::new(
            crate::services::opencode_native::attribution_evidence::H02_ANCHOR_SHA256.to_string(),
        );
        let sha_for_tui: crate::services::opencode_native::attribution_evidence::TuiBinarySha = {
            let sha = Arc::clone(&sha);
            Arc::new(move |_pid: u32| Some((*sha).clone()))
        };
        let sha_for_service: crate::services::opencode_native::attribution_evidence::ServiceBinarySha = {
            let sha = Arc::clone(&sha);
            Arc::new(move |_pid: u32| Some((*sha).clone()))
        };
        let w2_pid = TUI_W2_PID;
        let w2_started = TUI_W2_STARTED_AT_MS;
        hub.arm_binding_gate_for_tests(
            provider,
            Arc::new(move |pid: u32| (pid == w2_pid).then_some(w2_started)),
            sha_for_tui,
            sha_for_service,
            5_000,
            10,
            home.clone(),
        );
        let hub = Arc::new(hub);
        hub.store().with(|c| {
            crate::remote::pairing::persist_device(
                c,
                &crate::remote::pairing::NewDevice {
                    id: DEVICE_ID.into(),
                    name: DEVICE_NAME.into(),
                    ua: "fixture".into(),
                    origin_ip: "127.0.0.1".into(),
                    via: "lan".into(),
                    paired_at: now_ms(),
                },
            )
            .unwrap()
        });
        // 探针面开在 W2（旧代码会真的对它发起 echo 并取得 ack → 完成投递）。
        peer.arm_probe_w2(TUI_W2_PID);
        Rig {
            _tmp: tmp,
            project,
            card,
            peer,
            hub,
        }
    }

    /// F12 RED d：路由 active + 窗口绑定记录缺失 ⇒ 后台派发不得经新鲜查询自行换窗
    /// ——锁定窗口解析对本会话 fail-closed（零探针 IO、零投递），queued 项记原因
    /// 原样保留；重选前零派发（M3/patch6 未知/待发出口承接）。
    #[tokio::test]
    async fn background_dispatch_without_window_record_fails_closed_before_any_echo() {
        let rig = boot_gate_armed(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        // 队列项：reserve + 冻结入 outbox（尚不派发）。
        let command = send_cmd("f12_no_record", &binding, "窗口记录缺失的待发项");
        rig.hub
            .action_store()
            .reserve(DEVICE_ID, &bound_target(&rig.project), &command)
            .unwrap();
        let NativeOperation::Send(send) = &command.operation else {
            unreachable!();
        };
        rig.hub.store().with(|conn| {
            assert!(crate::database::dao::opencode_outbox::enqueue_frozen_conn(
                conn,
                &command,
                &[],
                send,
                None,
                None,
            )
            .unwrap());
        });
        // 前置：本 rig 从未发生选择事件（内存注册表空），伴表也没有窗口记录
        //（「记录缺失」形态）。
        let rows = rig.hub.store().with(|c| {
            crate::database::dao::opencode_pinned_windows::ensure_table_conn(c)?;
            crate::database::dao::opencode_pinned_windows::pinned_window_conn(c, SESSION_ID)
        });
        assert!(rows.unwrap().is_none(), "前置：无窗口记录");
        let result = rig.hub.flush_outbox(&rig.card).await;
        // fail-closed：零投递、零探针请求（绝不经新鲜查询 echo 另一扇窗）。
        assert_eq!(
            rig.peer.prompt_posts(),
            0,
            "窗口记录缺失时重选前零派发（不得经新鲜回退换窗投递）"
        );
        assert_eq!(
            rig.peer.count("POST", "/api/session"),
            0,
            "零探针请求：fail-closed 必须发生在锁定窗口解析处"
        );
        let action = rig.hub.action(DEVICE_ID, "f12_no_record").await.unwrap();
        assert_eq!(action.state, ActionState::Queued, "待发项原样保留");
        assert!(
            action.reason_code.is_some(),
            "queued 项必须携带拒绝原因（未知/待发语义承接）: {result:?}"
        );
        let outbox = rig.hub.outbox_view(&rig.card).unwrap();
        assert_eq!(outbox.items.len(), 1, "队列恰一行");
    }

    // ==== G1：换窗后旧请求零写（最终派发事务窗口 CAS） ====

    const G1_TUI_PID: u32 = 62200;
    const G1_TUI_STARTED_AT_MS: i64 = 1_793_291_500_000;
    const G1_TUI_BOOT: &str = "boot-g1-fixture";
    const G1_TUI_ALT_PID: u32 = 62300;
    const G1_TUI_ALT_STARTED_AT_MS: i64 = 1_793_291_550_000;

    /// G1 rig：echo 门装配（H02 已证构建组合）+ 探针面开在 W1 + 派发闸。
    /// 视图认领把 W1 落进伴表绑定；派发链在「回声验证完成后、取得派发权前」停在
    /// 返回的闸上，测试在闸内制造确定性交错。
    async fn boot_g1(
        world: World,
    ) -> (
        Rig,
        Arc<crate::services::opencode_native::projection::DispatchPreclaimHold>,
    ) {
        boot_g1_in(world, tempfile::tempdir().expect("tempdir")).await
    }

    async fn boot_g1_in(
        mut world: World,
        tmp: tempfile::TempDir,
    ) -> (
        Rig,
        Arc<crate::services::opencode_native::projection::DispatchPreclaimHold>,
    ) {
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).expect("建项目目录");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("建 home");
        let state_file = tmp.path().join("state/opencode/service.json");
        let card = card_for(SESSION_ID, &project);
        world.directory = project.to_string_lossy().into_owned();
        let peer = Peer::start(world, &state_file).await;
        write_registration(&state_file, peer.port);
        let no_conflict: Arc<dyn Fn(&std::path::Path) -> bool + Send + Sync> =
            Arc::new(|_dir: &std::path::Path| false);
        let mut hub = NativeHub::with_sources(
            DeviceStore::memory(),
            DiscoveryInput {
                state_file: state_file.clone(),
                home: home.clone(),
            },
            process_fact(),
            Some(no_conflict),
            None,
        );
        // 派发闸必须在 Arc 化之前装配（同 arm_binding_gate 的 get_mut 约束）。
        let hold = hub.arm_dispatch_preclaim_hold_for_tests();
        // 锁定窗口：本会话恒回 W1（与视图认领将落库的伴表绑定同窗）。
        let provider: crate::services::opencode_native::binding_gate::LockedWindowLookup =
            Arc::new(move |sid: &str| {
                (sid == SESSION_ID).then_some(crate::services::opencode_native::LockedWindow {
                    pid: G1_TUI_PID,
                    started_at_ms: G1_TUI_STARTED_AT_MS,
                    boot: Some(G1_TUI_BOOT.to_string()),
                })
            });
        let sha = Arc::new(
            crate::services::opencode_native::attribution_evidence::H02_ANCHOR_SHA256.to_string(),
        );
        let sha_for_tui: crate::services::opencode_native::attribution_evidence::TuiBinarySha = {
            let sha = Arc::clone(&sha);
            Arc::new(move |_pid: u32| Some((*sha).clone()))
        };
        let sha_for_service: crate::services::opencode_native::attribution_evidence::ServiceBinarySha =
            {
                let sha = Arc::clone(&sha);
                Arc::new(move |_pid: u32| Some((*sha).clone()))
            };
        let alive_pid = G1_TUI_PID;
        let alive_started = G1_TUI_STARTED_AT_MS;
        hub.arm_binding_gate_for_tests(
            provider,
            Arc::new(move |pid: u32| (pid == alive_pid).then_some(alive_started)),
            sha_for_tui,
            sha_for_service,
            5_000,
            10,
            home,
        );
        let hub = Arc::new(hub);
        hub.store().with(|c| {
            crate::remote::pairing::persist_device(
                c,
                &crate::remote::pairing::NewDevice {
                    id: DEVICE_ID.into(),
                    name: DEVICE_NAME.into(),
                    ua: "fixture".into(),
                    origin_ip: "127.0.0.1".into(),
                    via: "lan".into(),
                    paired_at: now_ms(),
                },
            )
            .unwrap()
        });
        // 探针面开在 W1（echo 自动 ack）。
        peer.arm_probe_w2(G1_TUI_PID);
        (
            Rig {
                _tmp: tmp,
                project,
                card,
                peer,
                hub,
            },
            hold,
        )
    }

    /// G1 共用前置：视图认领（echo 一次 + 落 W1 绑定行）+ 队列项（reserve + 冻结
    /// 入 outbox，尚不派发）。返回 (binding, 当前路由版本, 伴表 W1 行)。
    async fn g1_seed_queued_action(
        rig: &Rig,
        action_id: &str,
    ) -> (
        String,
        i64,
        crate::database::dao::opencode_pinned_windows::PinnedWindowRow,
    ) {
        let (view, claim) = rig
            .hub
            .view_and_claim(&rig.card, true)
            .await
            .expect("建立路由");
        assert!(claim.owned);
        let (version, pin) = rig
            .hub
            .store()
            .with(|c| {
                crate::database::dao::opencode_pinned_windows::ensure_table_conn(c)?;
                let pin = crate::database::dao::opencode_pinned_windows::pinned_window_conn(
                    c, SESSION_ID,
                )
                .unwrap()
                .expect("伴表绑定在场（W1）");
                let route = dao::route_conn(c, SESSION_ID).unwrap().unwrap();
                Ok::<_, rusqlite::Error>((route.route_version, pin))
            })
            .unwrap();
        assert_eq!(pin.window_pid, i64::from(G1_TUI_PID), "前置：伴表锁定 W1");
        let command = send_cmd(action_id, &view.binding_token, "G1 待发项");
        rig.hub
            .action_store()
            .reserve(DEVICE_ID, &bound_target(&rig.project), &command)
            .unwrap();
        let NativeOperation::Send(send) = &command.operation else {
            unreachable!();
        };
        rig.hub.store().with(|conn| {
            assert!(crate::database::dao::opencode_outbox::enqueue_frozen_conn(
                conn,
                &command,
                &[],
                send,
                None,
                None
            )
            .unwrap());
        });
        (view.binding_token, version, pin)
    }

    #[tokio::test]
    async fn created_first_sentence_loses_window_authorization_before_claim() {
        use crate::database::dao::opencode_creates::{self, CreateRecord, CreateWindow};

        // 在工作区内建项目，避免 macOS 的 /private 临时目录被创建路径校验拒绝。
        let tmp = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let (rig, hold) = boot_g1_in(World::new(), tmp).await;
        let project = crate::inject::create_path::validate_existing(rig.project.to_str().unwrap())
            .expect("合法的创建目录");
        let service = rig.hub.discover_create_service(&rig.project).unwrap();
        let mut record = CreateRecord::new(
            DEVICE_ID,
            "0123456789abcdef0123456789abcdef".to_owned(),
            project,
            &service,
            Some("G1 待发项".to_owned()),
        );
        let (_, _, pin) = g1_seed_queued_action(&rig, &record.first_action_id).await;
        rig.hub.store().with(|conn| {
            assert!(opencode_creates::reserve(conn, DEVICE_ID, &record).unwrap());
            let before = record.clone();
            record.session_id = Some(SESSION_ID.to_owned());
            record.window_attempted = true;
            record.window = CreateWindow::Opened;
            record.confirmed_window = Some(crate::services::opencode_native::LockedWindow {
                pid: G1_TUI_PID,
                started_at_ms: G1_TUI_STARTED_AT_MS,
                boot: pin.window_boot,
            });
            assert!(opencode_creates::update(conn, DEVICE_ID, &before, &record).unwrap());
        });

        let hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let task = tokio::spawn(async move { hub.flush_outbox(&card).await });
        tokio::time::timeout(std::time::Duration::from_secs(10), hold.started.notified())
            .await
            .expect("首句须通过原窗回声验证并到达最终 claim 前的派发闸");
        let writes_before_claim = rig.peer.non_get();
        // 仅撤销创建记录的开窗授权，路由和伴表保持同窗。
        let before = record.clone();
        record.window = CreateWindow::Pending;
        record.confirmed_window = None;
        let updated = rig
            .hub
            .store()
            .with(|conn| opencode_creates::update(conn, DEVICE_ID, &before, &record));
        hold.release.notify_one();
        assert!(updated.unwrap(), "创建记录的原窗授权须已撤销");

        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .expect("派发应结束")
            .expect("派发任务未 panic");
        assert_eq!(rig.peer.prompt_posts(), 0, "首句不得写入上游: {result:?}");
        assert_eq!(rig.peer.non_get(), writes_before_claim, "放闸后零上游写");
        let err = result.expect_err("最终 claim 须拒绝已失去原窗授权的首句");
        assert_eq!(err.code, codes::OC_ROUTE_WINDOW_MISMATCH, "{err:?}");
        let action = rig
            .hub
            .action(DEVICE_ID, &record.first_action_id)
            .await
            .unwrap();
        assert_eq!(action.state, ActionState::Queued, "首句仍待发");
        assert_eq!(
            action.reason_code.as_deref(),
            Some(codes::OC_ROUTE_WINDOW_MISMATCH)
        );
    }

    /// G1 RED a（确定性并发回归）：回声验证完成后、取得派发权前，同服务显式重选
    /// 换窗（伴表行 pid/boot/version 变化）⇒ 派发零写拒绝：outbox 无新投递、无新
    /// 消息、待发项保留、拒绝码 OC_ROUTE_WINDOW_MISMATCH。
    #[tokio::test]
    async fn window_reselect_between_echo_and_claim_refuses_dispatch() {
        let (rig, hold) = boot_g1(World::new()).await;
        let (_binding, version_before, pin_before) = g1_seed_queued_action(&rig, "act_g1a").await;
        // 后台 flush：echo 对 W1 完成 → 停在派发闸（取得派发权之前）。
        let flush_hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let task = tokio::spawn(async move { flush_hub.flush_outbox(&card).await });
        tokio::time::timeout(std::time::Duration::from_secs(10), hold.started.notified())
            .await
            .expect("必须到达派发闸（回声验证已完成）");
        // 闸内：同服务显式重选换窗（W2）——路由版本 +1、伴表行换 pid/boot。
        let mut pinned_w2 = pin_before.clone();
        pinned_w2.window_pid = i64::from(G1_TUI_ALT_PID);
        pinned_w2.window_started_at_ms = G1_TUI_ALT_STARTED_AT_MS;
        pinned_w2.window_boot = None;
        let target = bound_target(&rig.project);
        rig.hub.store().with(|conn| {
            assert!(
                dao::claim_route_and_pin_checked_conn(
                    conn,
                    &target,
                    true,
                    now_ms(),
                    true,
                    None,
                    Some(&pinned_w2),
                )
                .unwrap(),
                "同代际显式重选换窗必须放行（未决项只挡换代际）"
            );
        });
        hold.release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        // 派发拒绝：flush 返回拒绝码；queued 项原样保留（不置终态、不清冻结载荷）。
        let err = result.expect_err("换窗后旧请求必须零写拒绝");
        assert_eq!(err.code, codes::OC_ROUTE_WINDOW_MISMATCH, "{err:?}");
        let action = rig.hub.action(DEVICE_ID, "act_g1a").await.unwrap();
        assert_eq!(action.state, ActionState::Queued, "待发项保留");
        assert_eq!(
            action.reason_code.as_deref(),
            Some(codes::OC_ROUTE_WINDOW_MISMATCH),
            "拒绝码必须落账（待发语义承接）"
        );
        // 零写：无新 prompt 投递；outbox 冻结载荷未被消费。
        assert_eq!(rig.peer.prompt_posts(), 0, "换窗后旧请求零原生写");
        use rusqlite::OptionalExtension;
        let frozen: Option<String> = rig
            .hub
            .store()
            .with(|conn| {
                conn.query_row(
                    "SELECT command_json FROM opencode_outbox WHERE action_id='act_g1a'",
                    [],
                    |r| r.get(0),
                )
                .optional()
            })
            .unwrap();
        assert!(frozen.is_some(), "outbox 行与冻结载荷原样保留");
        // 版本只被显式重选递增一次；伴表行 = W2（派发全程零写伴表/路由）。
        let (version_after, pin_after) = rig
            .hub
            .store()
            .with(|c| {
                let pin = crate::database::dao::opencode_pinned_windows::pinned_window_conn(
                    c, SESSION_ID,
                )
                .unwrap()
                .unwrap();
                let route = dao::route_conn(c, SESSION_ID).unwrap().unwrap();
                Ok::<_, rusqlite::Error>((route.route_version, pin))
            })
            .unwrap();
        assert_eq!(
            version_after,
            version_before + 1,
            "版本只被显式重选递增一次"
        );
        assert_eq!(pin_after.window_pid, i64::from(G1_TUI_ALT_PID));
    }

    /// G1 RED b（伴表读取失败 fail-closed）：回声验证后、取得派发权前伴表不可读
    /// ⇒ 提交被阻断（STORE_FAILED，不得因读失败放行），零写、待发保留。
    #[tokio::test]
    async fn companion_table_read_failure_blocks_dispatch_commit() {
        let (rig, hold) = boot_g1(World::new()).await;
        let (_binding, _version, _pin) = g1_seed_queued_action(&rig, "act_g1b").await;
        let flush_hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let task = tokio::spawn(async move { flush_hub.flush_outbox(&card).await });
        tokio::time::timeout(std::time::Duration::from_secs(10), hold.started.notified())
            .await
            .expect("必须到达派发闸");
        // 闸内：伴表不可读（整表移除——持久层未知 ≠ 无绑定）。
        rig.hub.store().with(|conn| {
            conn.execute("DROP TABLE opencode_native_pinned_windows", [])
                .unwrap();
        });
        hold.release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        let err = result.expect_err("伴表读取失败必须阻断提交");
        assert_eq!(err.code, action_codes::STORE_FAILED, "{err:?}");
        let action = rig.hub.action(DEVICE_ID, "act_g1b").await.unwrap();
        assert_eq!(action.state, ActionState::Queued, "待发项保留");
        assert_eq!(
            action.reason_code.as_deref(),
            Some(action_codes::STORE_FAILED),
            "fail-closed 拒绝必须落账"
        );
        assert_eq!(rig.peer.prompt_posts(), 0, "读取失败零原生写");
        assert_eq!(
            rig.hub.outbox_view(&rig.card).unwrap().items.len(),
            1,
            "队列恰一行（冻结载荷未消费）"
        );
    }

    /// G1 RED c（正常链回归）：回声与派发之间绑定未变 ⇒ 窗口 CAS 全等 ⇒ 照常派发
    /// （换窗守卫不误伤正常链）。
    #[tokio::test]
    async fn unchanged_window_between_echo_and_claim_dispatches_normally() {
        let (rig, hold) = boot_g1(World::new()).await;
        let (_binding, version_before, _pin) = g1_seed_queued_action(&rig, "act_g1c").await;
        let flush_hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let task = tokio::spawn(async move { flush_hub.flush_outbox(&card).await });
        tokio::time::timeout(std::time::Duration::from_secs(10), hold.started.notified())
            .await
            .expect("必须到达派发闸");
        // 闸内什么都不做：窗口/版本保持验证时的形态。
        hold.release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        result.expect("窗口未变必须照常派发");
        let action = rig.hub.action(DEVICE_ID, "act_g1c").await.unwrap();
        assert_eq!(action.state, ActionState::Confirmed, "正常链照常 confirmed");
        assert_eq!(rig.peer.prompt_posts(), 1, "恰一次原生投递");
        let version_after = rig.hub.store().with(|c| {
            dao::route_conn(c, SESSION_ID)
                .unwrap()
                .unwrap()
                .route_version
        });
        assert_eq!(version_after, version_before, "正常派发零版本变化");
    }

    // ==== H1（HA）：队列变更四路径窗口 CAS ====
    //
    // 四条路径共用同一 `ExpectedWindow` 判定（与 G1 最终 claim 同源，不建第二套）：
    // ①新入队（enqueue）②重绑替换（rebind_outbox）③恢复派发（resume / 后台 flush）
    // ④最终 claim（G1 已有，复核对齐）。拒绝语义差异化：新入队 = 零新行；重绑替换 =
    // 原项不撤回不替换；恢复派发 = 恢复被拒态不变；三者拒绝原因留存。后台循环对
    // 带窗口拒绝原因的项跳过（不重试不清除），直到用户显式处理。暂停/撤回是本地
    // 安全操作，不受 CAS 影响（h1g）。

    /// H1：读取 outbox 冻结载荷（内容不变断言用；NULL 载荷 = None）。
    fn h1_outbox_payload(rig: &Rig, action_id: &str) -> Option<String> {
        use rusqlite::OptionalExtension;
        rig.hub.store().with(|conn| {
            conn.query_row(
                "SELECT command_json FROM opencode_outbox WHERE action_id = ?1",
                [action_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .unwrap()
            .flatten()
        })
    }

    /// H1：读取 outbox 行数（零新行断言用）。
    fn h1_outbox_rows(rig: &Rig, action_id: &str) -> i64 {
        rig.hub.store().with(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM opencode_outbox WHERE action_id = ?1",
                [action_id],
                |r| r.get(0),
            )
            .unwrap()
        })
    }

    /// H1：读取目标级 (revision, paused)。
    fn h1_target_state(rig: &Rig) -> (u64, bool) {
        rig.hub.store().with(|c| {
            crate::database::dao::opencode_outbox::target_state_conn(
                c,
                SESSION_ID,
                &rig.project.canonicalize().unwrap().to_string_lossy(),
            )
            .unwrap()
        })
    }

    /// H1：同服务显式重选换窗（闸内确定性注入）：路由版本 +1、伴表行换窗口三元组。
    /// 返回换窗后的路由版本。
    fn h1_reselect_window(
        rig: &Rig,
        window_pid: u32,
        window_started_at_ms: i64,
        window_boot: Option<&str>,
    ) -> i64 {
        let pinned = crate::database::dao::opencode_pinned_windows::PinnedWindowRow {
            service_id: SERVICE_ID.into(),
            runtime_pid: i64::from(PID),
            runtime_started_at_ms: STARTED_AT_MS,
            directory: rig
                .project
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            window_pid: i64::from(window_pid),
            window_started_at_ms,
            window_boot: window_boot.map(str::to_string),
        };
        let target = bound_target(&rig.project);
        rig.hub.store().with(|conn| {
            assert!(
                dao::claim_route_and_pin_checked_conn(
                    conn,
                    &target,
                    true,
                    now_ms(),
                    true,
                    None,
                    Some(&pinned),
                )
                .unwrap(),
                "同代际显式重选换窗必须放行"
            );
        });
        rig.hub.store().with(|c| {
            dao::route_conn(c, SESSION_ID)
                .unwrap()
                .unwrap()
                .route_version
        })
    }

    /// H1 RED a（新入队路径）：回声验证完成后、入队事务提交前切到同服务另一窗口
    /// ⇒ 项不入队（outbox 零新行）、拒绝原因留存、零原生写。
    #[tokio::test]
    async fn h1_enqueue_refused_after_window_switch_leaves_zero_outbox_rows() {
        let (rig, hold) = boot_g1(World::new()).await;
        let (view, claim) = rig
            .hub
            .view_and_claim(&rig.card, true)
            .await
            .expect("建立路由");
        assert!(claim.owned);
        let command = send_cmd("act_h1a", &view.binding_token, "H1a 新入队");
        let hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let task = tokio::spawn(async move { hub.execute(&actor(), &card, command).await });
        tokio::time::timeout(std::time::Duration::from_secs(10), hold.started.notified())
            .await
            .expect("必须到达派发闸（回声验证已完成）");
        // 闸内：同服务显式重选换窗（W2）。
        h1_reselect_window(&rig, G1_TUI_ALT_PID, G1_TUI_ALT_STARTED_AT_MS, None);
        hold.release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        let err = result.expect_err("换窗后新项必须不入队（零写拒绝）");
        assert_eq!(err.code, codes::OC_ROUTE_WINDOW_MISMATCH, "{err:?}");
        assert_eq!(
            h1_outbox_rows(&rig, "act_h1a"),
            0,
            "入队 CAS 拒绝 ⇒ outbox 零新行"
        );
        let action = rig.hub.action(DEVICE_ID, "act_h1a").await.unwrap();
        assert_eq!(
            action.state,
            ActionState::NotSent,
            "项未入队：保持 not_sent"
        );
        assert_eq!(
            action.reason_code.as_deref(),
            Some("route_window_mismatch"),
            "拒绝原因留存（可查询）"
        );
        assert_eq!(rig.peer.prompt_posts(), 0, "零原生写");
    }

    /// H1 RED b（重绑替换路径）：回声验证后、入队事务提交前切窗 ⇒ 原项不撤回
    /// 不替换（冻结载荷原样）、替换项零新行、拒绝原因留存。
    #[tokio::test]
    async fn h1_rebind_refused_after_window_switch_keeps_original_intact() {
        let (rig, hold) = boot_g1(World::new()).await;
        let (binding, _version, _pin) = g1_seed_queued_action(&rig, "act_h1b").await;
        let command: NativeCommand = serde_json::from_value(json!({
            "actionId": "act_h1b_rebind", "sessionId": SESSION_ID, "bindingToken": binding,
            "operation": {"kind": "rebind_outbox", "targetActionId": "act_h1b"}
        }))
        .unwrap();
        let hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let task = tokio::spawn(async move { hub.execute(&actor(), &card, command).await });
        tokio::time::timeout(std::time::Duration::from_secs(10), hold.started.notified())
            .await
            .expect("必须到达派发闸");
        h1_reselect_window(&rig, G1_TUI_ALT_PID, G1_TUI_ALT_STARTED_AT_MS, None);
        hold.release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        let err = result.expect_err("换窗后重绑替换必须整体拒绝");
        assert_eq!(err.code, codes::OC_ROUTE_WINDOW_MISMATCH, "{err:?}");
        // 原项不撤回不替换：仍 queued、无原因、冻结载荷逐字保留。
        let original = rig.hub.action(DEVICE_ID, "act_h1b").await.unwrap();
        assert_eq!(original.state, ActionState::Queued, "原项不撤回：仍 queued");
        assert_eq!(original.reason_code, None, "原项零改动");
        assert!(
            h1_outbox_payload(&rig, "act_h1b").is_some(),
            "原项冻结载荷原样保留"
        );
        // 替换项零新行 + 拒绝原因留存。
        assert_eq!(
            h1_outbox_rows(&rig, "act_h1b_rebind"),
            0,
            "替换项不入队：零新行"
        );
        let rebind = rig.hub.action(DEVICE_ID, "act_h1b_rebind").await.unwrap();
        assert_eq!(rebind.state, ActionState::NotSent);
        assert_eq!(
            rebind.reason_code.as_deref(),
            Some("route_window_mismatch"),
            "拒绝原因留存（可查询）"
        );
        assert_eq!(rig.peer.prompt_posts(), 0, "零原生写");
    }

    /// H1 RED c（恢复派发路径）：回声验证后、恢复事务提交前切窗 ⇒ 恢复被拒
    /// （paused 保持 true）、队列态不变、零投递、拒绝原因留存。
    #[tokio::test]
    async fn h1_resume_refused_after_window_switch_keeps_paused_state() {
        let (rig, hold) = boot_g1(World::new()).await;
        let (binding, _version, _pin) = g1_seed_queued_action(&rig, "act_h1c").await;
        // 暂停目标（本地安全操作；CAS 豁免面见 h1g）。
        let pause: NativeCommand = serde_json::from_value(json!({
            "actionId": "act_h1c_pause", "sessionId": SESSION_ID, "bindingToken": binding,
            "operation": {"kind": "set_outbox_paused", "paused": true,
                          "expectedRevision": h1_target_state(&rig).0}
        }))
        .unwrap();
        let reply = rig.hub.execute(&actor(), &rig.card, pause).await.unwrap();
        assert_eq!(reply.state, ActionState::Settled);
        let (_, paused) = h1_target_state(&rig);
        assert!(paused, "前置：目标已暂停");
        assert!(
            h1_outbox_payload(&rig, "act_h1c").is_some(),
            "前置：排队项冻结载荷在场"
        );
        // 恢复派发（paused=false）——这是 CAS 面，不是本地豁免面。
        let resume: NativeCommand = serde_json::from_value(json!({
            "actionId": "act_h1c_resume", "sessionId": SESSION_ID, "bindingToken": binding,
            "operation": {"kind": "set_outbox_paused", "paused": false,
                          "expectedRevision": h1_target_state(&rig).0}
        }))
        .unwrap();
        let hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let task = tokio::spawn(async move { hub.execute(&actor(), &card, resume).await });
        tokio::time::timeout(std::time::Duration::from_secs(10), hold.started.notified())
            .await
            .expect("必须到达派发闸");
        h1_reselect_window(&rig, G1_TUI_ALT_PID, G1_TUI_ALT_STARTED_AT_MS, None);
        hold.release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        let err = result.expect_err("换窗后恢复派发必须被拒");
        assert_eq!(err.code, codes::OC_ROUTE_WINDOW_MISMATCH, "{err:?}");
        let (_, paused) = h1_target_state(&rig);
        assert!(paused, "暂停状态保持不变");
        let item = rig.hub.action(DEVICE_ID, "act_h1c").await.unwrap();
        assert_eq!(item.state, ActionState::Queued, "队列态不变");
        assert!(
            h1_outbox_payload(&rig, "act_h1c").is_some(),
            "队列冻结载荷不变"
        );
        let resume_action = rig.hub.action(DEVICE_ID, "act_h1c_resume").await.unwrap();
        assert_eq!(
            resume_action.reason_code.as_deref(),
            Some("route_window_mismatch"),
            "拒绝原因留存（可查询）"
        );
        assert_eq!(rig.peer.prompt_posts(), 0, "零投递");
    }

    #[tokio::test]
    async fn budget_exhausted_before_resume_storage_keeps_outbox_paused() {
        let (rig, hold) = boot_g1(World::new()).await;
        let (binding, _, _) = g1_seed_queued_action(&rig, "act_budget_resume_queued").await;
        let pause: NativeCommand = serde_json::from_value(json!({
            "actionId": "act_budget_pause", "sessionId": SESSION_ID, "bindingToken": binding,
            "operation": {"kind": "set_outbox_paused", "paused": true,
                          "expectedRevision": h1_target_state(&rig).0}
        }))
        .unwrap();
        assert_eq!(
            rig.hub
                .execute(&actor(), &rig.card, pause)
                .await
                .unwrap()
                .state,
            ActionState::Settled
        );
        let before = h1_target_state(&rig);
        assert!(before.1, "前置：目标已暂停");
        let resume: NativeCommand = serde_json::from_value(json!({
            "actionId": "act_budget_resume", "sessionId": SESSION_ID, "bindingToken": binding,
            "operation": {"kind": "set_outbox_paused", "paused": false,
                          "expectedRevision": before.0}
        }))
        .unwrap();
        let hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let task = tokio::spawn(async move { hub.execute(&actor(), &card, resume).await });
        tokio::time::timeout(std::time::Duration::from_secs(10), hold.started.notified())
            .await
            .expect("必须到达回声验证后、恢复存储前的派发闸");
        rig.hub.set_action_budget_ms_for_tests(1);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        hold.release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            h1_target_state(&rig),
            before,
            "超预算不得修改暂停状态或修订号: {result:?}"
        );
        assert_eq!(
            result.expect_err("超预算恢复派发必须拒绝").code,
            codes::OC_DISPATCH_BUDGET_EXHAUSTED
        );
    }

    #[tokio::test]
    async fn budget_exhausted_before_send_storage_leaves_no_outbox_row() {
        let (rig, hold) = boot_g1(World::new()).await;
        let (view, claim) = rig.hub.view_and_claim(&rig.card, true).await.unwrap();
        assert!(claim.owned);
        let action_id = "act_budget_enqueue";
        let command = send_cmd(action_id, &view.binding_token, "截止前未入队");
        let hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let task = tokio::spawn(async move { hub.execute(&actor(), &card, command).await });
        tokio::time::timeout(std::time::Duration::from_secs(10), hold.started.notified())
            .await
            .expect("必须到达回声验证后、入队存储前的派发闸");
        rig.hub.set_action_budget_ms_for_tests(1);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        hold.release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            h1_outbox_rows(&rig, action_id),
            0,
            "超预算不得入队: {result:?}"
        );
        assert_eq!(
            result.expect_err("超预算入队必须拒绝").code,
            codes::OC_DISPATCH_BUDGET_EXHAUSTED
        );
    }

    /// H1 RED d（后台循环跳过窗口拒绝项）：第一轮恢复派发遇切窗被拒（拒绝原因落
    /// 排队项）；显式重选回 W1 后 ≥2 轮后台派发循环：原项内容不变、暂停标志不变、
    /// 原生零投递、拒绝原因未被清除——后台绝不能清除拒绝原因后悄悄投递。
    #[tokio::test]
    async fn h1_background_loop_skips_window_rejected_item_until_explicit_handling() {
        let (rig, hold) = boot_g1(World::new()).await;
        let (_binding, _version, _pin) = g1_seed_queued_action(&rig, "act_h1d").await;
        // 第一轮：回声后切窗（W2）⇒ 恢复派发被拒、拒绝原因落排队项（GA 既有语义面）。
        let flush_hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let task = tokio::spawn(async move { flush_hub.flush_outbox(&card).await });
        tokio::time::timeout(std::time::Duration::from_secs(10), hold.started.notified())
            .await
            .expect("必须到达派发闸");
        h1_reselect_window(&rig, G1_TUI_ALT_PID, G1_TUI_ALT_STARTED_AT_MS, None);
        hold.release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        let err = result.expect_err("第一轮：换窗拒绝");
        assert_eq!(err.code, codes::OC_ROUTE_WINDOW_MISMATCH, "{err:?}");
        let rejected = rig.hub.action(DEVICE_ID, "act_h1d").await.unwrap();
        assert_eq!(rejected.state, ActionState::Queued, "前置：排队项保留");
        assert_eq!(
            rejected.reason_code.as_deref(),
            Some(codes::OC_ROUTE_WINDOW_MISMATCH),
            "前置：拒绝原因已留存"
        );
        let frozen_before = h1_outbox_payload(&rig, "act_h1d").expect("前置：冻结载荷在场");
        assert_eq!(rig.peer.prompt_posts(), 0, "前置：零投递");
        // 用户显式重选回 W1（版本再 +1）——窗口恢复与验证产物同窗，但版本已变。
        h1_reselect_window(&rig, G1_TUI_PID, G1_TUI_STARTED_AT_MS, Some(G1_TUI_BOOT));
        // ≥2 轮后台派发循环：带窗口拒绝原因的项必须跳过（不重试不清除）。
        rig.hub
            .flush_outbox(&rig.card)
            .await
            .expect("第二轮必须跳过而非投递");
        rig.hub
            .flush_outbox(&rig.card)
            .await
            .expect("第三轮仍须跳过");
        let after = rig.hub.action(DEVICE_ID, "act_h1d").await.unwrap();
        assert_eq!(after.state, ActionState::Queued, "原项不变");
        assert_eq!(
            after.reason_code.as_deref(),
            Some(codes::OC_ROUTE_WINDOW_MISMATCH),
            "拒绝原因未被清除"
        );
        assert_eq!(
            h1_outbox_payload(&rig, "act_h1d").as_deref(),
            Some(frozen_before.as_str()),
            "冻结载荷逐字节不变"
        );
        let (_, paused) = h1_target_state(&rig);
        assert!(!paused, "暂停标志保持不变（未暂停）");
        assert_eq!(rig.peer.prompt_posts(), 0, "原生零投递");
    }

    /// H1 RED e（显式重选+重新确认后只送达一次）：拒绝 → 窗口恢复 → 后台跳过
    /// → 用户 rebind_outbox 重新确认 ⇒ 消息只送达一次（零重投），旧项由显式
    /// 重选替换（撤回、载荷销毁）。
    #[tokio::test]
    async fn h1_explicit_rebind_after_rejection_delivers_exactly_once() {
        let (rig, hold) = boot_g1(World::new()).await;
        let (_binding, _version, _pin) = g1_seed_queued_action(&rig, "act_h1e").await;
        // 拒绝面：第一轮切窗被拒（拒绝原因落排队项）。
        let flush_hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let task = tokio::spawn(async move { flush_hub.flush_outbox(&card).await });
        tokio::time::timeout(std::time::Duration::from_secs(10), hold.started.notified())
            .await
            .expect("必须到达派发闸");
        h1_reselect_window(&rig, G1_TUI_ALT_PID, G1_TUI_ALT_STARTED_AT_MS, None);
        hold.release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            result.expect_err("第一轮：换窗拒绝").code,
            codes::OC_ROUTE_WINDOW_MISMATCH
        );
        // 窗口恢复回 W1 + 两轮后台循环跳过（不投递）。
        h1_reselect_window(&rig, G1_TUI_PID, G1_TUI_STARTED_AT_MS, Some(G1_TUI_BOOT));
        rig.hub.flush_outbox(&rig.card).await.expect("后台跳过");
        rig.hub.flush_outbox(&rig.card).await.expect("后台跳过");
        assert_eq!(rig.peer.prompt_posts(), 0, "后台循环零投递");
        // 用户显式重选+重新确认：rebind_outbox（新 actionId）。
        let binding = current_binding(&rig).await;
        let rebind: NativeCommand = serde_json::from_value(json!({
            "actionId": "act_h1e_rebind", "sessionId": SESSION_ID, "bindingToken": binding,
            "operation": {"kind": "rebind_outbox", "targetActionId": "act_h1e"}
        }))
        .unwrap();
        let reply = rig
            .hub
            .execute(&actor(), &rig.card, rebind)
            .await
            .expect("显式重新确认必须放行");
        assert_eq!(
            reply.state,
            ActionState::Confirmed,
            "重新确认后唯一一次投递"
        );
        assert_eq!(rig.peer.prompt_posts(), 1, "消息只送达一次");
        // 后续循环零重投。
        rig.hub.flush_outbox(&rig.card).await.expect("后续循环");
        rig.hub.flush_outbox(&rig.card).await.expect("后续循环");
        assert_eq!(rig.peer.prompt_posts(), 1, "零重投");
        // 旧项由显式重选替换（撤回、载荷销毁）。
        let original = rig.hub.action(DEVICE_ID, "act_h1e").await.unwrap();
        assert_eq!(original.state, ActionState::NotSent);
        assert_eq!(original.reason_code.as_deref(), Some("outbox_retracted"));
        assert!(
            h1_outbox_payload(&rig, "act_h1e").is_none(),
            "旧项载荷已销毁"
        );
    }

    /// H1 RED g（本地安全操作回归）：暂停与撤回不受窗口 CAS 影响——切窗制造
    /// 「CAS 必拒」漂移面后，暂停/撤回照常本地生效，零远端写。
    #[tokio::test]
    async fn h1_local_pause_and_retract_stay_outside_window_cas() {
        let (rig, _hold) = boot_g1(World::new()).await;
        let (binding, _version, _pin) = g1_seed_queued_action(&rig, "act_h1g").await;
        // 先切窗（制造漂移面；无派发在飞，无需闸）。
        h1_reselect_window(&rig, G1_TUI_ALT_PID, G1_TUI_ALT_STARTED_AT_MS, None);
        let writes_before = rig.peer.non_get();
        // 暂停：本地生效，不受 CAS 影响。
        let pause: NativeCommand = serde_json::from_value(json!({
            "actionId": "act_h1g_pause", "sessionId": SESSION_ID, "bindingToken": binding,
            "operation": {"kind": "set_outbox_paused", "paused": true,
                          "expectedRevision": h1_target_state(&rig).0}
        }))
        .unwrap();
        let reply = rig.hub.execute(&actor(), &rig.card, pause).await.unwrap();
        assert_eq!(reply.state, ActionState::Settled, "暂停必须本地生效");
        let (_, paused) = h1_target_state(&rig);
        assert!(paused);
        // 撤回：本地生效，零远端写。
        let retract: NativeCommand = serde_json::from_value(json!({
            "actionId": "act_h1g_retract", "sessionId": SESSION_ID, "bindingToken": binding,
            "operation": {"kind": "retract_outbox", "targetActionId": "act_h1g"}
        }))
        .unwrap();
        let reply = rig.hub.execute(&actor(), &rig.card, retract).await.unwrap();
        assert_eq!(reply.state, ActionState::Settled, "撤回必须本地生效");
        let item = rig.hub.action(DEVICE_ID, "act_h1g").await.unwrap();
        assert_eq!(item.state, ActionState::NotSent, "撤回后脱离队列");
        assert_eq!(item.reason_code.as_deref(), Some("outbox_retracted"));
        assert!(
            h1_outbox_payload(&rig, "act_h1g").is_none(),
            "撤回与载荷销毁同事务"
        );
        assert_eq!(rig.peer.non_get(), writes_before, "本地操作零远端写");
        assert_eq!(rig.peer.prompt_posts(), 0);
    }

    // ==== I1：队列换窗拒绝不可被后台覆盖（锁内重核 + 统一队首规则） ====
    //
    // 统一队首规则（I1 裁定 = 统一等待）：后台选取与最终派发共用同一队首定义——
    // (session,directory) 内 `state IN ('queued','dispatching')` 的最早 outbox 行
    //（与 claim 的 FIFO 前序谓词同源）。队首带换窗拒绝原因 ⇒ 后台循环整队停止
    //（零验权零投递），后续项同样等待，不越队首先发——FIFO 保序是队列的存在意义，
    // 后发先至会改变对话语义。用户显式处理（rebind_outbox 替换队首）后按序恢复。
    // 选取侧不再单设「跳过被拦队首」过滤器，消除「选取跳过/派发算前序」双规则缝。

    /// I1 共用前置：后台已取走待发项（pick 完成、快照在读）时，换窗拒绝经与生产
    /// 同一 DAO 写入器落账（模拟并发路径在「取项之后、锁内处理之前」写入的拒绝），
    /// 然后放行后台继续。返回 (flush 任务句柄, 冻结载荷原值)。
    async fn i1_rejection_lands_between_pick_and_lock(
        rig: &Rig,
        action_id: &str,
    ) -> (tokio::task::JoinHandle<Result<(), NativeError>>, String) {
        let frozen_before = h1_outbox_payload(rig, action_id).expect("前置：冻结载荷在场");
        let (entered, release) = arm_info_hold(rig);
        let flush_hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let task = tokio::spawn(async move { flush_hub.flush_outbox(&card).await });
        wait_for_test_barrier(&entered).await;
        let action_id = action_id.to_string();
        rig.hub
            .store()
            .with(move |conn| {
                crate::database::dao::opencode_outbox::set_queued_reason_conn(
                    conn,
                    &action_id,
                    Some(codes::OC_ROUTE_WINDOW_MISMATCH),
                )
            })
            .expect("换窗拒绝落账");
        release.notify_one();
        (task, frozen_before)
    }

    /// I1 RED b（锁内重核）：后台已取走待发项时换窗拒绝落账 ⇒ 拿到会话锁后不得
    /// 清因、也不得按「当前窗口本可验证通过」投递：原因保留、载荷逐字、零投递，
    /// 后台路径返回换窗拒绝码。
    #[tokio::test]
    async fn i1_lock_recheck_blocks_wipe_and_dispatch_of_window_rejected_item() {
        let (rig, hold) = boot_g1(World::new()).await;
        let (_binding, _version, _pin) = g1_seed_queued_action(&rig, "act_i1b").await;
        // 本测试不用派发闸（拒绝落在更早的取项/快照缝），预放行。
        hold.release.notify_one();
        let (task, frozen_before) = i1_rejection_lands_between_pick_and_lock(&rig, "act_i1b").await;
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        let err = result.expect_err("换窗拒绝后的后台派发必须被阻断");
        assert_eq!(err.code, codes::OC_ROUTE_WINDOW_MISMATCH, "{err:?}");
        let action = rig.hub.action(DEVICE_ID, "act_i1b").await.unwrap();
        assert_eq!(action.state, ActionState::Queued, "待发项保留");
        assert_eq!(
            action.reason_code.as_deref(),
            Some(codes::OC_ROUTE_WINDOW_MISMATCH),
            "锁内不得清除换窗拒绝原因"
        );
        assert_eq!(
            h1_outbox_payload(&rig, "act_i1b").as_deref(),
            Some(frozen_before.as_str()),
            "冻结载荷逐字保留"
        );
        assert_eq!(
            rig.peer.prompt_posts(),
            0,
            "零投递（窗口本可验证通过也不投）"
        );
    }

    /// I1 RED a（后台先取项 → 拒绝落账 → 后台继续）：原项保留（内容逐字）、后续
    /// 轮次零投递、拒绝原因不被清除；用户显式重绑后恢复投递恰一次（RED d 的既有
    /// 语义在同一闸内回归）。
    #[tokio::test]
    async fn i1_background_pick_race_keeps_rejection_until_explicit_rebind_delivers_once() {
        let (rig, hold) = boot_g1(World::new()).await;
        let (_binding, _version, _pin) = g1_seed_queued_action(&rig, "act_i1a").await;
        hold.release.notify_one();
        let (task, frozen_before) = i1_rejection_lands_between_pick_and_lock(&rig, "act_i1a").await;
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        let err = result.expect_err("后台继续不得覆盖换窗拒绝");
        assert_eq!(err.code, codes::OC_ROUTE_WINDOW_MISMATCH, "{err:?}");
        let action = rig.hub.action(DEVICE_ID, "act_i1a").await.unwrap();
        assert_eq!(action.state, ActionState::Queued, "原项保留");
        assert_eq!(
            action.reason_code.as_deref(),
            Some(codes::OC_ROUTE_WINDOW_MISMATCH),
            "拒绝原因不被后台清除"
        );
        assert_eq!(
            h1_outbox_payload(&rig, "act_i1a").as_deref(),
            Some(frozen_before.as_str()),
            "原项内容逐字"
        );
        assert_eq!(rig.peer.prompt_posts(), 0);
        // 后续轮次：零投递、原因仍在、载荷仍在。
        rig.hub.flush_outbox(&rig.card).await.expect("阻断轮次");
        rig.hub.flush_outbox(&rig.card).await.expect("阻断轮次");
        let after = rig.hub.action(DEVICE_ID, "act_i1a").await.unwrap();
        assert_eq!(after.state, ActionState::Queued);
        assert_eq!(
            after.reason_code.as_deref(),
            Some(codes::OC_ROUTE_WINDOW_MISMATCH)
        );
        assert_eq!(
            h1_outbox_payload(&rig, "act_i1a").as_deref(),
            Some(frozen_before.as_str())
        );
        assert_eq!(rig.peer.prompt_posts(), 0, "后续轮次零投递");
        // 用户显式处理（rebind_outbox 重新确认既有语义）：恢复投递恰一次。
        let binding = current_binding(&rig).await;
        let rebind: NativeCommand = serde_json::from_value(json!({
            "actionId": "act_i1a_rebind", "sessionId": SESSION_ID, "bindingToken": binding,
            "operation": {"kind": "rebind_outbox", "targetActionId": "act_i1a"}
        }))
        .unwrap();
        let reply = rig
            .hub
            .execute(&actor(), &rig.card, rebind)
            .await
            .expect("显式处理必须放行");
        assert_eq!(
            reply.state,
            ActionState::Confirmed,
            "重新确认后唯一一次投递"
        );
        assert_eq!(rig.peer.prompt_posts(), 1, "消息只送达一次");
        // 后续循环零重投；旧项被替换撤回、载荷销毁。
        rig.hub.flush_outbox(&rig.card).await.expect("后续循环");
        assert_eq!(rig.peer.prompt_posts(), 1, "零重投");
        let original = rig.hub.action(DEVICE_ID, "act_i1a").await.unwrap();
        assert_eq!(original.state, ActionState::NotSent);
        assert_eq!(original.reason_code.as_deref(), Some("outbox_retracted"));
        assert!(
            h1_outbox_payload(&rig, "act_i1a").is_none(),
            "旧项载荷已销毁"
        );
    }

    /// I1 RED c（统一队首规则）：被拦队首 ⇒ 后台整队停止（后续项零验权零投递，
    /// 不再反复验权发不出，也不再把换窗拒绝误记到后续项上）；用户显式重选+重绑
    /// 队首后队列按 seq 序恢复流动（既有重绑尾随语义：干净项先发、重确认替换
    /// 随后，恰各一次）——选取与派发同一谓词，行为由本测试钉住。
    #[tokio::test]
    async fn i1_blocked_head_stops_queue_without_reverification_and_recovery_keeps_fifo_order() {
        let (rig, hold) = boot_g1(World::new()).await;
        let (binding, _version, _pin) = g1_seed_queued_action(&rig, "act_i1c_a").await;
        // 后续项 B（同目标，seq 更大）：reserve + 冻结入队（尚不派发）。
        let command_b = send_cmd("act_i1c_b", &binding, "I1c 后续项");
        rig.hub
            .action_store()
            .reserve(DEVICE_ID, &bound_target(&rig.project), &command_b)
            .unwrap();
        let NativeOperation::Send(send_b) = &command_b.operation else {
            unreachable!()
        };
        rig.hub.store().with(|conn| {
            assert!(crate::database::dao::opencode_outbox::enqueue_frozen_conn(
                conn,
                &command_b,
                &[],
                send_b,
                None,
                None
            )
            .unwrap())
        });
        // 轮 1（GA 既有语义）：队首 A 派发途中闸内显式切窗 ⇒ 换窗拒绝落 A。
        let flush_hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let task = tokio::spawn(async move { flush_hub.flush_outbox(&card).await });
        tokio::time::timeout(std::time::Duration::from_secs(10), hold.started.notified())
            .await
            .expect("必须到达派发闸");
        h1_reselect_window(&rig, G1_TUI_ALT_PID, G1_TUI_ALT_STARTED_AT_MS, None);
        hold.release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            result.expect_err("轮 1：队首换窗拒绝").code,
            codes::OC_ROUTE_WINDOW_MISMATCH
        );
        let head = rig.hub.action(DEVICE_ID, "act_i1c_a").await.unwrap();
        assert_eq!(
            head.reason_code.as_deref(),
            Some(codes::OC_ROUTE_WINDOW_MISMATCH),
            "前置：队首被拦"
        );
        let requests_after_round1 = rig.peer.total();
        // 轮 2/3：统一队首 ⇒ 整队停止：后续项零验权（请求数零增）、零投递、不被记因。
        rig.hub
            .flush_outbox(&rig.card)
            .await
            .expect("统一队首：整队等待");
        rig.hub
            .flush_outbox(&rig.card)
            .await
            .expect("统一队首：整队等待");
        assert_eq!(
            rig.peer.total(),
            requests_after_round1,
            "被拦队首场景下后续项零验权（不再反复验权发不出）"
        );
        assert_eq!(rig.peer.prompt_posts(), 0);
        let tail = rig.hub.action(DEVICE_ID, "act_i1c_b").await.unwrap();
        assert_eq!(tail.state, ActionState::Queued, "后续项原样保留");
        assert_eq!(tail.reason_code, None, "后续项不被记因（等待的是队首）");
        assert!(
            h1_outbox_payload(&rig, "act_i1c_b").is_some(),
            "后续项载荷在场"
        );
        // 用户显式重选回 W1 + 重绑队首 ⇒ 阻断解除（既有 rebind_outbox 语义：替换项
        // 按新 seq 落队尾，排在已排队的干净项之后，不冒充已投递）。
        h1_reselect_window(&rig, G1_TUI_PID, G1_TUI_STARTED_AT_MS, Some(G1_TUI_BOOT));
        let binding_now = current_binding(&rig).await;
        let rebind: NativeCommand = serde_json::from_value(json!({
            "actionId": "act_i1c_a_rebind", "sessionId": SESSION_ID, "bindingToken": binding_now,
            "operation": {"kind": "rebind_outbox", "targetActionId": "act_i1c_a"}
        }))
        .unwrap();
        let reply = rig
            .hub
            .execute(&actor(), &rig.card, rebind)
            .await
            .expect("显式处理必须放行");
        assert_eq!(
            reply.state,
            ActionState::Queued,
            "替换项既有语义：落队尾等待（不越位已排队项，也不冒充已投递）"
        );
        // 原被拦项由显式重选替换撤回、载荷销毁（阻断态解除的唯一合法路径）。
        let original = rig.hub.action(DEVICE_ID, "act_i1c_a").await.unwrap();
        assert_eq!(original.state, ActionState::NotSent);
        assert_eq!(original.reason_code.as_deref(), Some("outbox_retracted"));
        assert!(h1_outbox_payload(&rig, "act_i1c_a").is_none());
        // 恢复轮次：队列按 seq 序重新流动——B 与重确认替换各恰投递一次。
        rig.hub.flush_outbox(&rig.card).await.expect("恢复轮次");
        assert_eq!(
            rig.peer.prompt_posts(),
            2,
            "后续项+队首替换各恰一次（零重投零丢失）"
        );
        let texts: Vec<String> = rig
            .peer
            .log()
            .iter()
            .filter(|request| {
                request.method == "POST"
                    && request.path() == format!("/api/session/{SESSION_ID}/prompt")
            })
            .filter_map(|request| {
                request
                    .body
                    .as_ref()
                    .and_then(|body| body["text"].as_str())
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(
            texts,
            vec!["I1c 后续项".to_string(), "G1 待发项".to_string()],
            "恢复按队列 seq 序（既有重绑尾随语义：干净项先发、重确认替换随后）"
        );
        // 恢复完毕：队列清空，后续循环零重投。
        rig.hub.flush_outbox(&rig.card).await.expect("收尾轮次");
        assert_eq!(rig.peer.prompt_posts(), 2, "零重投");
    }

    // ==== F12 项 2：C6 归档三态代际判定 ====

    /// 可切换进程事实：mode 0 = 正常（PID 活/换代 pid 活）；mode 1 = 全 None
    ///（瞬态读失败模拟）；mode 2 = PID 退出（None）、换代 pid 活；mode 3 = PID 换代
    ///（lstart 变）。
    type FactMode = std::sync::atomic::AtomicUsize;

    /// 两态进程事实缝（发现/门用）。
    type TwoStateFact = Arc<dyn Fn(u32) -> Option<i64> + Send + Sync>;
    /// G3 事实对：`(两态缝, 三形态缝)`——同一世界状态的两条缝。
    type FactPair = (
        TwoStateFact,
        Arc<dyn Fn(u32) -> super::ProcessFactProbe + Send + Sync>,
    );

    /// G3：两态 + 三形态事实对——同一 mode 同时驱动发现/门缝（两态）与代际判定缝
    ///（三形态），两条缝对同一世界状态保持一致。收紧后「fact None」在两态缝里只能
    /// 按读取失败处理（保守桥接），**明确死亡**必须经三形态缝表达：
    /// mode 2 的原进程退出在三形态缝里是 `NoSuchProcess`。
    fn switchable_facts(mode: Arc<FactMode>) -> FactPair {
        let two_state_mode = Arc::clone(&mode);
        let two_state: Arc<dyn Fn(u32) -> Option<i64> + Send + Sync> = Arc::new(move |pid: u32| {
            switchable_fact_answer(
                two_state_mode.load(std::sync::atomic::Ordering::SeqCst),
                pid,
            )
        });
        let generation: Arc<dyn Fn(u32) -> super::ProcessFactProbe + Send + Sync> =
            Arc::new(move |pid: u32| {
                match mode.load(std::sync::atomic::Ordering::SeqCst) {
                    0 => super::ProcessFactProbe::Alive(if pid == PID {
                        STARTED_AT_MS
                    } else {
                        STARTED_AT_MS + 60_000
                    }),
                    // 瞬态读失败：事实通道不可用（无死亡证明）。
                    1 => super::ProcessFactProbe::ReadFailure,
                    // 原进程退出：机制明确报告无此 pid（可证明死亡）。
                    2 => {
                        if pid == PID {
                            super::ProcessFactProbe::NoSuchProcess
                        } else {
                            super::ProcessFactProbe::Alive(STARTED_AT_MS + 60_000)
                        }
                    }
                    // PID 换代：pid 活但 lstart ≠ 账本。
                    _ => super::ProcessFactProbe::Alive(STARTED_AT_MS + 999),
                }
            });
        (two_state, generation)
    }

    /// 两态答案表（与 [`switchable_facts`] 的三形态答案一一对应）。
    fn switchable_fact_answer(mode_value: usize, pid: u32) -> Option<i64> {
        match mode_value {
            0 => Some(if pid == PID {
                STARTED_AT_MS
            } else {
                STARTED_AT_MS + 60_000
            }),
            // 瞬态读失败：连 PID 的事实都拿不到（显式服务可能仍活，但证据不可得）。
            1 => None,
            // 原进程退出：PID 事实 None；换代 pid 可测。
            2 => (pid != PID).then_some(STARTED_AT_MS + 60_000),
            // PID 换代：pid 活但 lstart ≠ 账本。
            _ => Some(STARTED_AT_MS + 999),
        }
    }

    fn switchable_fact(mode: Arc<FactMode>) -> Arc<dyn Fn(u32) -> Option<i64> + Send + Sync> {
        let (two_state, _generation) = switchable_facts(mode);
        two_state
    }

    /// G3：换代感知事实对——restart 后原 pid：两态缝 = None、三形态缝 =
    /// NoSuchProcess（可证明死亡）。供「boot + restart」形态的既有用例贴合 F12/G3
    /// 判定（默认事实桩对原 pid 恒回账本 lstart，会把换代误报成 OriginalAlive）。
    fn restart_aware_facts(restarted: Arc<AtomicBool>) -> FactPair {
        let two_state_restarted = Arc::clone(&restarted);
        let two_state: Arc<dyn Fn(u32) -> Option<i64> + Send + Sync> = Arc::new(move |pid: u32| {
            if pid == PID {
                (!two_state_restarted.load(Ordering::SeqCst)).then_some(STARTED_AT_MS)
            } else {
                Some(STARTED_AT_MS + 60_000)
            }
        });
        let generation: Arc<dyn Fn(u32) -> super::ProcessFactProbe + Send + Sync> =
            Arc::new(move |pid: u32| {
                if pid == PID {
                    if restarted.load(Ordering::SeqCst) {
                        super::ProcessFactProbe::NoSuchProcess
                    } else {
                        super::ProcessFactProbe::Alive(STARTED_AT_MS)
                    }
                } else {
                    super::ProcessFactProbe::Alive(STARTED_AT_MS + 60_000)
                }
            });
        (two_state, generation)
    }

    /// F12 RED f：无登记 + 显式服务仍活 + 短暂读失败（模拟超时）⇒ InsufficientEvidence
    /// ——出口不渲染（runtime_dead 缺席）、confirm_unknown 409（绝不把证据不足当「已死」
    /// 放行归档）。
    #[tokio::test]
    async fn insufficient_generation_evidence_neither_decorates_nor_archives() {
        let mode = Arc::new(FactMode::new(0));
        let mut world = World::new();
        world.prompt = PromptMode::QueuedThenDropped;
        let rig = boot_with_fact(world, switchable_fact(Arc::clone(&mode))).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_f12f", &binding, "证据不足形态"),
            )
            .await
            .expect("丢响应是未知结果");
        assert_eq!(view.state, ActionState::Unconfirmed);
        let verified = rig.hub.verify(DEVICE_ID, "act_f12f").await.unwrap();
        assert_eq!(verified.state, ActionState::Acknowledged);
        // 证据退化：事实通道瞬态读失败（None）+ 登记文件缺失。
        mode.store(1, std::sync::atomic::Ordering::SeqCst);
        let state_file = rig._tmp.path().join("state/opencode/service.json");
        std::fs::remove_file(&state_file).expect("移除登记文件（无登记形态）");
        let read = rig.hub.action(DEVICE_ID, "act_f12f").await.unwrap();
        assert_eq!(
            read.runtime_dead, None,
            "InsufficientEvidence 不装饰（出口不渲染）；不得把证据不足冒充死亡"
        );
        let error = rig
            .hub
            .confirm_unknown(DEVICE_ID, "act_f12f")
            .await
            .expect_err("证据不足时归档必须 409 拒绝");
        assert_eq!(error.code, action_codes::STATE, "{error:?}");
        let after = rig.hub.action(DEVICE_ID, "act_f12f").await.unwrap();
        assert_eq!(after.state, ActionState::Acknowledged, "拒绝必须零改写");
    }

    /// G3 RED d：原进程事实**读取失败**（无法区分「不存在/PID 复用/瞬态读不到」）
    /// 且**登记滞留可读** ⇒ InsufficientEvidence——出口不渲染、confirm 409、零归档。
    /// 旧判据「fact None + 登记可读 ⇒ 采信退出」在本用例翻车：fact None 只是
    /// 「读不到」，登记通道健康不构成原进程退出的证明（活桩仍在场）。
    #[tokio::test]
    async fn fact_read_failure_with_readable_registration_is_insufficient() {
        let mode = Arc::new(FactMode::new(0));
        let mut world = World::new();
        world.prompt = PromptMode::QueuedThenDropped;
        let rig = boot_with_fact(world, switchable_fact(Arc::clone(&mode))).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_g3d", &binding, "事实读取失败形态"),
            )
            .await
            .expect("丢响应是未知结果");
        assert_eq!(view.state, ActionState::Unconfirmed);
        let verified = rig.hub.verify(DEVICE_ID, "act_g3d").await.unwrap();
        assert_eq!(verified.state, ActionState::Acknowledged);
        // 证据退化：事实通道读取失败（None），**登记文件原样保留可读**
        //（与 F12 RED f 的差异正在于此：登记健康过去被当成死亡佐证）。
        mode.store(1, std::sync::atomic::Ordering::SeqCst);
        let state_file = rig._tmp.path().join("state/opencode/service.json");
        assert!(
            state_file.exists(),
            "前置：登记文件滞留可读（旧判据据此误判死亡）"
        );
        let read = rig.hub.action(DEVICE_ID, "act_g3d").await.unwrap();
        assert_eq!(
            read.runtime_dead, None,
            "读取失败 ≠ 已死：出口不渲染（未判定不得冒充死亡）"
        );
        let error = rig
            .hub
            .confirm_unknown(DEVICE_ID, "act_g3d")
            .await
            .expect_err("证据不足时归档必须 409 拒绝");
        assert_eq!(error.code, action_codes::STATE, "{error:?}");
        let after = rig.hub.action(DEVICE_ID, "act_g3d").await.unwrap();
        assert_eq!(after.state, ActionState::Acknowledged, "拒绝必须零改写");
    }

    /// H2 RED a：目标仍活（活桩在场 + 登记可读）+ 单 pid 读取失败 + 自身读取成功
    /// ⇒ ReadFailure/InsufficientEvidence **三面同闭**：UI 无归档入口
    ///（runtime_dead=None）、服务端拒归档（409 零改写）、死亡豁免拒绝（旧绑定
    /// 显式重选维持 `OC_BINDING_STALE`）。三消费面（with_runtime_dead /
    /// confirm_unknown / patch5 豁免）共用同一三形态判定——能读取自身进程或
    /// 服务登记，不能证明另一个 pid 已退出；任何一面对读取失败开口即回退放宽。
    #[tokio::test]
    async fn h2_read_failure_with_live_stub_keeps_all_three_death_faces_closed() {
        let mut world = World::new();
        world.prompt = PromptMode::QueuedThenDropped;
        // 活桩：两态缝（发现/门视角）对原 pid 恒回账本 lstart——目标真实在场。
        let stub: TwoStateFact = Arc::new(|pid: u32| (pid == PID).then_some(STARTED_AT_MS));
        // 三形态缝：自身读取成功（本进程事实健康），目标 pid 读取失败（单 pid 读不出）。
        let self_pid = std::process::id();
        let generation: Arc<dyn Fn(u32) -> super::ProcessFactProbe + Send + Sync> =
            Arc::new(move |pid: u32| {
                if pid == self_pid {
                    super::ProcessFactProbe::Alive(STARTED_AT_MS + 999_999)
                } else {
                    super::ProcessFactProbe::ReadFailure
                }
            });
        let rig = boot_with_facts(world, stub, generation).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_h2a", &binding, "单 pid 读失败"),
            )
            .await
            .expect("丢响应是未知结果");
        assert_eq!(view.state, ActionState::Unconfirmed);
        let verified = rig.hub.verify(DEVICE_ID, "act_h2a").await.unwrap();
        assert_eq!(verified.state, ActionState::Acknowledged);
        // 前置：目标活桩在场（peer 运行中）+ 登记文件滞留可读——两者都不构成
        // 死亡佐证，也不改变判定输入。
        let state_file = rig._tmp.path().join("state/opencode/service.json");
        assert!(state_file.exists(), "前置：登记文件可读（不构成死亡佐证）");
        // 面 1：UI 无归档入口（读取失败 ≠ 已死，出口不渲染）。
        let read = rig.hub.action(DEVICE_ID, "act_h2a").await.unwrap();
        assert_eq!(
            read.runtime_dead, None,
            "读取失败 ⇒ InsufficientEvidence：呈现面不渲染归档出口"
        );
        // 面 2：服务端拒归档（409 零改写）。
        let error = rig
            .hub
            .confirm_unknown(DEVICE_ID, "act_h2a")
            .await
            .expect_err("读取失败时归档必须 409 拒绝");
        assert_eq!(error.code, action_codes::STATE, "{error:?}");
        assert_eq!(
            rig.hub.action(DEVICE_ID, "act_h2a").await.unwrap().state,
            ActionState::Acknowledged,
            "拒绝必须零改写"
        );
        // 面 3：死亡豁免拒绝——不再匹配的旧绑定发起显式重选 ⇒ OC_BINDING_STALE
        //（patch5 豁免只认正向死亡证据，读取失败维持拒绝）。
        let reselect = rig
            .hub
            .reenter_native_route_versioned(&rig.card, Some("h2-foreign-binding"), None, None)
            .await;
        let refusal = reselect.expect_err("读取失败时死亡豁免必须拒绝");
        assert_eq!(
            refusal.code,
            codes::OC_BINDING_STALE,
            "死亡不可证明 ⇒ 显式重选维持 OC_BINDING_STALE：{refusal:?}"
        );
    }

    // ==== G3：三形态判据纯函数守卫（RED e/f 回归保持 + 收紧面） ====

    /// 判定输入行：账本原服务身份 = (PID, STARTED_AT_MS)。
    fn generation_row() -> dao::ActionRow {
        dao::ActionRow {
            action_id: "act_gen".into(),
            device_id: DEVICE_ID.into(),
            session_id: SESSION_ID.into(),
            service_id: SERVICE_ID.into(),
            runtime_pid: i64::from(PID),
            runtime_started_at_ms: STARTED_AT_MS,
            directory: "/proj".into(),
            operation: "send".into(),
            request_key: None,
            payload_sha256: "hash".into(),
            state: ActionState::Acknowledged,
            native_id: None,
            reason_code: None,
        }
    }

    /// RED e 守卫（收紧不丢明确形态）：事实机制明确报告无此 pid ⇒ Ended（可归档）。
    #[test]
    fn explicit_no_such_process_proves_ended() {
        let row = generation_row();
        let fact = |_pid: u32| super::ProcessFactProbe::NoSuchProcess;
        assert_eq!(
            classify_original_generation_with(&fact, &row),
            OriginalGeneration::Ended,
            "明确无此 pid = 退出可证明 ⇒ Ended"
        );
    }

    /// RED f 守卫（收紧不丢明确形态）：PID 复用（pid 活但 lstart≠账本）⇒ Ended。
    #[test]
    fn pid_reuse_proves_ended() {
        let row = generation_row();
        let fact = |_pid: u32| super::ProcessFactProbe::Alive(STARTED_AT_MS + 999);
        assert_eq!(
            classify_original_generation_with(&fact, &row),
            OriginalGeneration::Ended,
            "lstart≠账本 = PID 已换代 ⇒ Ended"
        );
    }

    /// 收紧面：读取失败 ⇒ InsufficientEvidence——登记/任何旁证不参与判定
    ///（旧判据「fact None + 登记可读 ⇒ 采信退出」废除）。
    #[test]
    fn read_failure_is_never_ended() {
        let row = generation_row();
        let fact = |_pid: u32| super::ProcessFactProbe::ReadFailure;
        assert_eq!(
            classify_original_generation_with(&fact, &row),
            OriginalGeneration::InsufficientEvidence,
            "读取失败不是死亡证明：fail-closed"
        );
    }

    /// Alive 相等 ⇒ Alive（登记切到别处也不改判，原通道仍可核对）。
    #[test]
    fn alive_with_ledger_identity_stays_alive() {
        let row = generation_row();
        let fact = |_pid: u32| super::ProcessFactProbe::Alive(STARTED_AT_MS);
        assert_eq!(
            classify_original_generation_with(&fact, &row),
            OriginalGeneration::Alive
        );
    }

    /// pid 不可解析 ⇒ InsufficientEvidence（负数 pid 不构成任何方向）。
    #[test]
    fn unparsable_pid_is_insufficient() {
        let mut row = generation_row();
        row.runtime_pid = -1;
        let fact = |_pid: u32| super::ProcessFactProbe::NoSuchProcess;
        assert_eq!(
            classify_original_generation_with(&fact, &row),
            OriginalGeneration::InsufficientEvidence
        );
    }

    /// F12 RED g：登记切到新服务但原进程仍活（pid+lstart 均在）⇒ OriginalAlive
    /// ——不可归档（409）、呈现面不渲染死亡出口。
    #[tokio::test]
    async fn registry_moved_but_original_process_alive_is_not_archivable() {
        let mut world = World::new();
        world.prompt = PromptMode::QueuedThenDropped;
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_f12g", &binding, "原进程仍活"),
            )
            .await
            .expect("丢响应是未知结果");
        assert_eq!(view.state, ActionState::Unconfirmed);
        let verified = rig.hub.verify(DEVICE_ID, "act_f12g").await.unwrap();
        assert_eq!(verified.state, ActionState::Acknowledged);
        // 登记切到新服务（换代重启），但原进程仍以账本身份存活（默认事实桩对 PID
        // 恒回账本 lstart）。
        rig.peer.restart();
        let read = rig.hub.action(DEVICE_ID, "act_f12g").await.unwrap();
        assert_eq!(
            read.runtime_dead,
            Some(false),
            "OriginalAlive：呈现面不渲染死亡出口（仍可核对的行不得宣告死亡）"
        );
        let error = rig
            .hub
            .confirm_unknown(DEVICE_ID, "act_f12g")
            .await
            .expect_err("原进程仍活时归档必须 409 拒绝");
        assert_eq!(error.code, action_codes::STATE, "{error:?}");
        let after = rig.hub.action(DEVICE_ID, "act_f12g").await.unwrap();
        assert_eq!(after.state, ActionState::Acknowledged, "拒绝必须零改写");
    }

    /// F12 h（回归守卫）：原进程退出或 PID 换代（lstart≠账本）⇒ GenerationEnded
    /// ——可归档（unknown_confirmed）+ 重选解锁成功。
    #[tokio::test]
    async fn ended_generation_is_archivable_then_reselect_succeeds() {
        let mode = Arc::new(FactMode::new(0));
        let mut world = World::new();
        world.prompt = PromptMode::QueuedThenDropped;
        // G3 收紧迁移：「原进程退出」的死亡证明经三形态缝表达为 NoSuchProcess
        //（变体 2 的 PID 换代走 Alive(lstart≠账本)，两态缝即可表达）。
        let (fact, generation) = switchable_facts(Arc::clone(&mode));
        let rig = boot_with_facts(world, fact, generation).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_f12h", &binding, "原进程退出"),
            )
            .await
            .expect("丢响应是未知结果");
        assert_eq!(view.state, ActionState::Unconfirmed);
        let verified = rig.hub.verify(DEVICE_ID, "act_f12h").await.unwrap();
        assert_eq!(verified.state, ActionState::Acknowledged);
        // 原进程退出（三形态缝明确报告无此 pid；登记滞留可读不再作为死亡佐证）
        // ⇒ GenerationEnded。
        mode.store(2, std::sync::atomic::Ordering::SeqCst);
        let read = rig.hub.action(DEVICE_ID, "act_f12h").await.unwrap();
        assert_eq!(read.runtime_dead, Some(true), "GenerationEnded 渲染出口");
        let archived = rig
            .hub
            .confirm_unknown(DEVICE_ID, "act_f12h")
            .await
            .expect("死亡代际放行归档");
        assert_eq!(archived.state, ActionState::UnknownConfirmed);
        // 归档解除未决屏障：换代重选（新代际目标）成功。
        let new_target = NativeTarget {
            runtime: RuntimeKey {
                service_id: "f12-next-generation".into(),
                pid: PID + 100,
                started_at_ms: STARTED_AT_MS + 120_000,
            },
            session_id: SESSION_ID.into(),
            directory: rig.project.canonicalize().unwrap(),
        };
        assert!(
            rig.hub
                .action_store()
                .try_claim_route(&new_target, true)
                .expect("重选判定"),
            "归档后重选必须解锁"
        );
        // PID 换代变体（lstart≠账本）同样 GenerationEnded（独立 rig，避免与上面的
        // 合成换代路由互相污染投影）。
        let mode2 = Arc::new(FactMode::new(0));
        let mut world2 = World::new();
        world2.prompt = PromptMode::QueuedThenDropped;
        let rig2 = boot_with_fact(world2, switchable_fact(Arc::clone(&mode2))).await;
        claim_route(&rig2);
        let binding2 = current_binding(&rig2).await;
        let view2 = rig2
            .hub
            .execute(
                &actor(),
                &rig2.card,
                send_cmd("act_f12h2", &binding2, "PID 换代"),
            )
            .await
            .expect("丢响应是未知结果");
        assert_eq!(view2.state, ActionState::Unconfirmed);
        let verified2 = rig2.hub.verify(DEVICE_ID, "act_f12h2").await.unwrap();
        assert_eq!(verified2.state, ActionState::Acknowledged);
        mode2.store(3, std::sync::atomic::Ordering::SeqCst);
        let read2 = rig2.hub.action(DEVICE_ID, "act_f12h2").await.unwrap();
        assert_eq!(read2.runtime_dead, Some(true), "PID 换代 ⇒ GenerationEnded");
        let archived2 = rig2
            .hub
            .confirm_unknown(DEVICE_ID, "act_f12h2")
            .await
            .expect("PID 换代放行归档");
        assert_eq!(archived2.state, ActionState::UnknownConfirmed);
    }

    /// F12 RED i：普通排队项与已受理插队项各走一遍归档→重选，全程零重投
    ///（outbox 恰一行 / 零行，prompt 零新投递）。
    #[tokio::test]
    async fn archive_then_reselect_never_redispatches_queue_or_steer_items() {
        // 变体 1：普通排队（queue 投递）已受理项（换代后原进程退出可证）。
        let mode = Arc::new(FactMode::new(0));
        let mut world = World::new();
        world.prompt = PromptMode::QueuedThenDropped;
        // G3 收紧迁移：mode 2 的死亡经三形态缝表达为 NoSuchProcess。
        let (fact, generation) = switchable_facts(Arc::clone(&mode));
        let rig = boot_with_facts(world, fact, generation).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_f12i1", &binding, "排队项"),
            )
            .await
            .expect("丢响应是未知结果");
        assert_eq!(view.state, ActionState::Unconfirmed);
        assert_eq!(rig.peer.prompt_posts(), 1, "恰一次原始投递");
        let verified = rig.hub.verify(DEVICE_ID, "act_f12i1").await.unwrap();
        assert_eq!(verified.state, ActionState::Acknowledged);
        rig.peer.restart();
        mode.store(2, std::sync::atomic::Ordering::SeqCst);
        let archived = rig
            .hub
            .confirm_unknown(DEVICE_ID, "act_f12i1")
            .await
            .expect("死亡代际放行归档");
        assert_eq!(archived.state, ActionState::UnknownConfirmed);
        let new_target = NativeTarget {
            runtime: rig.hub.current_runtime().expect("新代际可发现"),
            session_id: SESSION_ID.into(),
            directory: rig.project.canonicalize().unwrap(),
        };
        assert!(
            rig.hub
                .action_store()
                .try_claim_route(&new_target, true)
                .expect("重选判定"),
            "归档后重选解锁"
        );
        // 全程零重投：prompt 仍是原始那一次；outbox 恰一行（原行，载荷已清）。
        assert_eq!(rig.peer.prompt_posts(), 1, "归档→重选零新投递");
        let outbox = rig.hub.outbox_view(&rig.card).unwrap();
        assert_eq!(outbox.items.len(), 1, "outbox 恰一行");
        assert_eq!(
            rig.hub.action(DEVICE_ID, "act_f12i1").await.unwrap().state,
            ActionState::UnknownConfirmed,
            "归档行保持终态"
        );

        // 变体 2：已受理插队（steer）项（换代后原进程退出可证）。
        let mode2 = Arc::new(FactMode::new(0));
        let mut world2 = World::new();
        world2.prompt = PromptMode::QueuedThenDropped;
        // G3 收紧迁移：mode 2 的死亡经三形态缝表达为 NoSuchProcess。
        let (fact2, generation2) = switchable_facts(Arc::clone(&mode2));
        let rig2 = boot_with_facts(world2, fact2, generation2).await;
        claim_route(&rig2);
        let binding2 = current_binding(&rig2).await;
        let mut steer = send_cmd("act_f12i2", &binding2, "插队项");
        let NativeOperation::Send(send) = &mut steer.operation else {
            unreachable!();
        };
        send.delivery = DeliveryKind::Steer;
        let view2 = rig2
            .hub
            .execute(&actor(), &rig2.card, steer)
            .await
            .expect("丢响应是未知结果");
        assert_eq!(view2.state, ActionState::Unconfirmed);
        assert_eq!(rig2.peer.prompt_posts(), 1, "恰一次原始投递");
        let verified2 = rig2.hub.verify(DEVICE_ID, "act_f12i2").await.unwrap();
        assert_eq!(verified2.state, ActionState::Acknowledged);
        rig2.peer.restart();
        mode2.store(2, std::sync::atomic::Ordering::SeqCst);
        let archived2 = rig2
            .hub
            .confirm_unknown(DEVICE_ID, "act_f12i2")
            .await
            .expect("死亡代际放行归档");
        assert_eq!(archived2.state, ActionState::UnknownConfirmed);
        let new_target2 = NativeTarget {
            runtime: rig2.hub.current_runtime().expect("新代际可发现"),
            session_id: SESSION_ID.into(),
            directory: rig2.project.canonicalize().unwrap(),
        };
        assert!(
            rig2.hub
                .action_store()
                .try_claim_route(&new_target2, true)
                .expect("重选判定"),
            "归档后重选解锁"
        );
        assert_eq!(rig2.peer.prompt_posts(), 1, "归档→重选零新投递");
        let outbox2 = rig2.hub.outbox_view(&rig2.card).unwrap();
        assert_eq!(outbox2.items.len(), 0, "插队项不进本地 FIFO（outbox 零行）");
    }

    /// F12 RED j：归档证据分流——经当前可达服务对原会话+规范目录 read-back 查到
    /// `msg_<actionId>` 正式消息记录 ⇒ confirmed（落 native id）；查不到 ⇒
    /// unknown_confirmed（保守，现状）。
    #[tokio::test]
    async fn archive_splits_on_native_message_evidence() {
        // 变体 1：新代际会话记录里已有正式消息记录（消息实际已落会话）⇒ confirmed。
        let mode = Arc::new(FactMode::new(0));
        let mut world = World::new();
        world.prompt = PromptMode::QueuedThenDropped;
        // G3 收紧迁移：mode 2 的死亡经三形态缝表达为 NoSuchProcess。
        let (fact, generation) = switchable_facts(Arc::clone(&mode));
        let rig = boot_with_facts(world, fact, generation).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let view = rig
            .hub
            .execute(
                &actor(),
                &rig.card,
                send_cmd("act_f12j1", &binding, "已有落地证据"),
            )
            .await
            .expect("丢响应是未知结果");
        assert_eq!(view.state, ActionState::Unconfirmed);
        let verified = rig.hub.verify(DEVICE_ID, "act_f12j1").await.unwrap();
        assert_eq!(verified.state, ActionState::Acknowledged);
        rig.peer.restart();
        mode.store(2, std::sync::atomic::Ordering::SeqCst);
        // 消息实际已恰一次入会话（跨换代持久）；本用例不走「核对」入口（G4 patch7 起
        // verify 也消费同一 read-back 通道收敛 confirmed），专考归档入口的证据分流：
        // 同一证据，两路各自的状态迁移互不越界。
        rig.peer
            .set(|world| world.messages = vec![user_record("msg_act_f12j1", "已有落地证据")]);
        let archived = rig
            .hub
            .confirm_unknown(DEVICE_ID, "act_f12j1")
            .await
            .expect("归档入口放行");
        assert_eq!(
            archived.state,
            ActionState::Confirmed,
            "有正式消息证据 ⇒ confirmed（不得损失定案）"
        );
        assert_eq!(
            archived.native_id.as_deref(),
            Some("msg_act_f12j1"),
            "confirmed 必须落 native message id"
        );

        // 变体 2：无消息证据 ⇒ unknown_confirmed（保守，现状语义）。
        let mode2 = Arc::new(FactMode::new(0));
        let mut world2 = World::new();
        world2.prompt = PromptMode::QueuedThenDropped;
        // G3 收紧迁移：mode 2 的死亡经三形态缝表达为 NoSuchProcess。
        let (fact2, generation2) = switchable_facts(Arc::clone(&mode2));
        let rig2 = boot_with_facts(world2, fact2, generation2).await;
        claim_route(&rig2);
        let binding2 = current_binding(&rig2).await;
        let view2 = rig2
            .hub
            .execute(
                &actor(),
                &rig2.card,
                send_cmd("act_f12j2", &binding2, "无证据"),
            )
            .await
            .expect("丢响应是未知结果");
        assert_eq!(view2.state, ActionState::Unconfirmed);
        let verified2 = rig2.hub.verify(DEVICE_ID, "act_f12j2").await.unwrap();
        assert_eq!(verified2.state, ActionState::Acknowledged);
        rig2.peer.restart();
        mode2.store(2, std::sync::atomic::Ordering::SeqCst);
        let archived2 = rig2
            .hub
            .confirm_unknown(DEVICE_ID, "act_f12j2")
            .await
            .expect("归档入口放行");
        assert_eq!(
            archived2.state,
            ActionState::UnknownConfirmed,
            "无正式消息证据 ⇒ unknown_confirmed（保守）"
        );
        // native_id 可能来自此前只读核对收敛 Acknowledged 时落的原生 id（COALESCE
        // 保留），归档分流看的是**消息记录证据**：无记录 ⇒ 不升级 confirmed。
    }

    // ==== H5：停止统一端到端预算与五段分段计时（task HE） ====
    //
    // 预算语义（用户裁决逐条）：统一预算覆盖服务端动作链接收 → 正式回执全链；
    // 派发前超时 ⇒ 尽快落「确定未发送」可核对结论，**此后不得迟到取得派发权**
    // （claim 事务内预算复核）；派发后丢回执 ⇒ Unknown 只按原 action ID 核对
    //（既有语义，见 verify_recovers_stop_and_set_agent_without_resend）；
    // 404 只表示当前未读到（码面/文案不暗示失败终态）。

    /// 停止动作的假服务 POST /interrupt 命中数（零重发断言锚）。
    fn he_interrupt_posts(rig: &Rig) -> usize {
        rig.peer
            .count("POST", &format!("/api/session/{SESSION_ID}/interrupt"))
    }

    fn he_timing_row(
        rig: &Rig,
        action_id: &str,
    ) -> Option<crate::database::dao::opencode_action_timings::ActionTimingRow> {
        rig.hub.store().with(|conn| {
            crate::database::dao::opencode_action_timings::get_conn(conn, action_id).unwrap()
        })
    }

    /// H5 RED a：五段分段计时在场且结构化可查——每笔动作链尝试在
    /// `opencode_native_action_timings` 落一行（排队含单飞等待 / 归属核对 /
    /// 数据库等待 / 原生派发 / 回读 + total + budget_refused），重试覆盖不重复。
    #[tokio::test]
    async fn he_stop_dispatch_records_five_segment_timings() {
        let mut world = World::new();
        world.busy = Some(100);
        world.idle = None;
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let view = rig
            .hub
            .execute(&actor(), &rig.card, stop_cmd("act_he_timing", &binding))
            .await
            .expect("忙窗停止派发");
        assert_eq!(
            (view.state, view.reason_code.as_deref()),
            (ActionState::Settled, Some("interrupted")),
            "忙窗停止 = settled/interrupted"
        );
        assert_eq!(he_interrupt_posts(&rig), 1, "恰一次原生写");
        let timing = he_timing_row_wait(&rig, "act_he_timing")
            .await
            .expect("分段计时行必须落账");
        assert_eq!(timing.operation, "stop_session");
        assert_eq!(timing.session_id, SESSION_ID);
        assert!(timing.total_ms > 0, "全链总耗时必须为正: {timing:?}");
        assert!(
            timing.queue_ms >= 0
                && timing.ownership_ms >= 0
                && timing.database_ms >= 0
                && timing.dispatch_ms >= 0
                && timing.readback_ms >= 0,
            "五段必须全部在场（结构化）: {timing:?}"
        );
        // 毫秒分辨率下无争用本机回环链可快至个位毫秒——段级 >0 归因由
        // [`he_five_segment_timings_track_their_phase`] 用受控挂起确定性断言。
        let sum = timing.queue_ms
            + timing.ownership_ms
            + timing.database_ms
            + timing.dispatch_ms
            + timing.readback_ms;
        assert!(
            sum <= timing.total_ms,
            "分段之和不得超过全链总耗时（分段观测口径）: {timing:?}"
        );
        assert!(!timing.budget_refused, "正常链零预算拒绝: {timing:?}");
        // 同 ID 重复请求（去重回执）不产生第二行（主键覆盖语义）。
        rig.hub
            .execute(&actor(), &rig.card, stop_cmd("act_he_timing", &binding))
            .await
            .unwrap();
        let count: i64 = rig.hub.store().with(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM opencode_native_action_timings WHERE action_id='act_he_timing'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(count, 1, "重试覆盖同一行，绝不多行");
    }

    /// H5 RED a（段归因）：挂住快照的 `/api/info` 读取（归属核对段内的受控窗口）
    /// ⇒ 该窗口必须被计量进「归属核对」段，而「原生派发」段保持远小于它——
    /// 证明五段各自度量自己的相位（不是全链复制）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn he_five_segment_timings_track_their_phase() {
        let mut world = World::new();
        world.busy = Some(100);
        world.idle = None;
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let (entered, release) = arm_info_hold(&rig);
        let hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let task = tokio::spawn(async move {
            hub.execute(&actor(), &card, stop_cmd("act_he_phase", &binding))
                .await
        });
        wait_for_test_barrier(&entered).await;
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        release.notify_one();
        // JoinHandle 输出已是 Result：三层 unwrap = Elapsed / JoinError / NativeError。
        let view = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .expect("不挂起")
            .expect("任务不 panic")
            .expect("停止派发");
        assert_eq!(view.state, ActionState::Settled);
        let timing = he_timing_row_wait(&rig, "act_he_phase")
            .await
            .expect("计时行在场");
        assert!(
            timing.ownership_ms >= 50,
            "info 挂起窗口必须计入归属核对段: {timing:?}"
        );
        assert!(
            timing.dispatch_ms < timing.ownership_ms,
            "派发段必须远小于挂起的归属核对段（段相位独立）: {timing:?}"
        );
    }

    /// H5 RED c：派发前超时 ⇒ 确定未发送落账 + **此后不得迟到取得派发权**。
    /// 停在派发闸（取得派发权之前）→ 缩预算 → 放行 ⇒ claim 事务内预算复核拒绝
    /// 授权写出：零 interrupt 写出、not_sent 墓碑（碑文 budget_exhausted）、
    /// 墓碑后 claim 恒拒、计时行 budget_refused=1。
    #[tokio::test]
    async fn he_budget_exhausted_before_claim_blocks_late_dispatch_authority() {
        let (rig, hold) = boot_g1(World::new()).await;
        let (view, claim) = rig
            .hub
            .view_and_claim(&rig.card, true)
            .await
            .expect("建立路由");
        assert!(claim.owned);
        let binding = view.binding_token;
        let hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let task = tokio::spawn(async move {
            hub.execute(&actor(), &card, stop_cmd("act_he_budget", &binding))
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), hold.started.notified())
            .await
            .expect("必须到达派发闸（取得派发权之前）");
        // 闸内：统一预算缩到 1ms 并越过它 ⇒ 放行后到达的派发不得取得授权写出。
        rig.hub.set_action_budget_ms_for_tests(1);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        hold.release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .expect("预算拒绝必须尽快返回（不挂起）")
            .expect("任务不 panic");
        let err = result.expect_err("超预算派发必须受控拒绝");
        assert_eq!(
            err.code,
            crate::services::opencode_native::budget::OC_DISPATCH_BUDGET_EXHAUSTED,
            "{err:?}"
        );
        // 「确定未发送」可核对结论落账：not_sent + 碑文 budget_exhausted。
        let stored = rig.hub.action(DEVICE_ID, "act_he_budget").await.unwrap();
        assert_eq!(stored.state, ActionState::NotSent, "{stored:?}");
        assert_eq!(stored.reason_code.as_deref(), Some("budget_exhausted"));
        // 零迟到派发权：零 interrupt 写出；墓碑后 claim 恒拒（零授权写出）。
        assert_eq!(he_interrupt_posts(&rig), 0, "超预算派发零原生写");
        assert!(
            !rig.hub
                .action_store()
                .claim("act_he_budget")
                .expect("claim 读"),
            "确定未发送墓碑后不得再取得派发权"
        );
        // 计时观测位：budget_refused = 1（Ruling H2 测量面）。
        let timing = he_timing_row_wait(&rig, "act_he_budget")
            .await
            .expect("计时行必须落账");
        assert!(
            timing.budget_refused,
            "预算拒绝必须进计时观测位: {timing:?}"
        );
    }

    /// H5 语义回归 d：派发后丢回执 ⇒ Unknown 只按原 action ID 核对（既有语义），
    /// 全程恰一次原生写、零重发。
    #[tokio::test]
    async fn he_stop_lost_receipt_stays_unknown_converged_only_by_original_action_id() {
        let mut world = World::new();
        world.busy = Some(100);
        world.idle = None;
        let rig = boot(world).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        // 丢回执：stop POST 已发出但响应被切断（peer 记录请求后断开）。
        rig.peer.set(|world| world.drop_interrupt_reply = true);
        let view = rig
            .hub
            .execute(&actor(), &rig.card, stop_cmd("act_he_lost", &binding))
            .await
            .expect("丢回执是未知结果不是错误");
        assert_eq!(view.state, ActionState::Unconfirmed, "丢回执 = Unknown");
        assert_eq!(he_interrupt_posts(&rig), 1, "写恰一次");
        // 只按原 action ID 核对（零重发）：仍在忙 ⇒ 只能 Unknown（readback_failed）。
        let busy = rig.hub.verify(DEVICE_ID, "act_he_lost").await.unwrap();
        assert_eq!(
            busy.state,
            ActionState::Unconfirmed,
            "仍在执行只能是 Unknown"
        );
        assert_eq!(busy.reason_code.as_deref(), Some("readback_failed"));
        // 已空闲也不能把空闲当成本次停止成功（result_unproven，既有语义）。
        rig.peer.set(|world| {
            world.busy = None;
            world.idle = Some(200);
        });
        let idle = rig.hub.verify(DEVICE_ID, "act_he_lost").await.unwrap();
        assert_eq!(idle.state, ActionState::Unconfirmed);
        assert_eq!(idle.reason_code.as_deref(), Some("result_unproven"));
        assert_eq!(he_interrupt_posts(&rig), 1, "核对绝不重发");
        // 计时行照常落账（丢回执链也进测量面）。
        assert!(he_timing_row_wait(&rig, "act_he_lost").await.is_some());
    }

    // ==== I2：停止绝对截止（写权后复核 + 计时移出回执路径，task IB） ====
    //
    // 缺陷（用户复审原文）：最后一次预算检查发生在取得 SQLite 写权**前**（claim 闸
    // ④ 在事务入口）——此后若写锁等待跨过 45 秒，仍可能提交并发送 /interrupt；
    // 计时落账又在回执前同步写库。I2 加两层：⑤ 写权后复核（claim UPDATE 成功后、
    // 提交前再核同一绝对截止；事务内回退 = 零提交零发送）+ 计时落账异步化
    //（回执等待路径零计时写库）。
    //
    // 注入缝（GFix/HE 口径的外部锁争用）：对 rig 的 WAL 文件库开**第二个连接**
    // BEGIN IMMEDIATE + 一行写 ⇒ 外部连接持有 SQLite 写锁；hub 的 claim UPDATE
    // 在 busy_timeout 内等待。对峙解除用 ROLLBACK（不推进 WAL 头 ⇒ 读快照仍有效，
    // 等待方干净获得写锁；COMMIT 会制造 BUSY_SNAPSHOT 换错误路径，不是本场景）。

    /// IB 文件库 rig：与 `he_boot_file_rig` 同构（生产同路径 `open_and_init_once`
    /// 建 WAL 文件库），busy_timeout 可调（外部锁对峙必须小于它），并可装配
    /// 派发闸 / 计时落账闸 / 写权后持锁缝 / 提交后持锁缝（返回句柄；未装配 = None）。
    async fn ib_boot(
        mut world: World,
        busy_timeout: std::time::Duration,
        arm_dispatch_hold: bool,
        arm_timing_hold: bool,
        arm_claim_hold: bool,
        arm_post_commit_hold: bool,
    ) -> (
        Rig,
        std::path::PathBuf,
        Option<Arc<crate::services::opencode_native::projection::DispatchPreclaimHold>>,
        Option<Arc<crate::services::opencode_native::budget::TimingPersistHold>>,
        Option<Arc<crate::services::opencode_native::projection::ClaimPostWriteHold>>,
        Option<Arc<crate::services::opencode_native::projection::ClaimPostCommitHold>>,
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).expect("建项目目录");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("建 home");
        let state_file = tmp.path().join("state/opencode/service.json");
        let card = card_for(SESSION_ID, &project);
        world.directory = project.to_string_lossy().into_owned();
        let peer = Peer::start(world, &state_file).await;
        write_registration(&state_file, peer.port);
        let db_path = tmp.path().join("ib-i2.db");
        let conn = crate::database::connection::open_and_init_once(&db_path, true, busy_timeout)
            .map_err(|e| format!("IB 文件库初始化失败（open_and_init_once）: {e}"))
            .expect("IB 文件库初始化失败");
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .expect("读 journal_mode");
        assert_eq!(mode, "wal", "IB rig 库必须实际处于 WAL 模式");
        let fact = process_fact();
        let bridged = Arc::clone(&fact);
        let generation: Arc<dyn Fn(u32) -> super::ProcessFactProbe + Send + Sync> =
            Arc::new(move |pid: u32| match bridged(pid) {
                Some(started) => super::ProcessFactProbe::Alive(started),
                None => super::ProcessFactProbe::ReadFailure,
            });
        let mut hub = NativeHub::with_coordinator_facts(
            DeviceStore::Owned(std::sync::Arc::new(std::sync::Mutex::new(conn))),
            DiscoveryInput { state_file, home },
            fact,
            generation,
        );
        let dispatch_hold = arm_dispatch_hold.then(|| hub.arm_dispatch_preclaim_hold_for_tests());
        let timing_hold = arm_timing_hold.then(|| hub.arm_timing_persist_hold_for_tests());
        let claim_hold = arm_claim_hold.then(|| hub.arm_claim_post_write_hold_for_tests());
        let post_commit_hold =
            arm_post_commit_hold.then(|| hub.arm_claim_post_commit_hold_for_tests());
        let hub = Arc::new(hub);
        hub.store().with(|c| {
            crate::remote::pairing::persist_device(
                c,
                &crate::remote::pairing::NewDevice {
                    id: DEVICE_ID.into(),
                    name: "ib-fixture".into(),
                    ua: "fixture".into(),
                    origin_ip: "127.0.0.1".into(),
                    via: "lan".into(),
                    paired_at: now_ms(),
                },
            )
            .unwrap()
        });
        let rig = Rig {
            _tmp: tmp,
            project,
            card,
            peer,
            hub,
        };
        (
            rig,
            db_path,
            dispatch_hold,
            timing_hold,
            claim_hold,
            post_commit_hold,
        )
    }

    /// 外部写锁（IB 注入缝）：同文件第二连接 BEGIN IMMEDIATE + 一行写 ⇒ 持有
    /// WAL 写锁。返回连接（测试负责 ROLLBACK 释放——见小节注释）。
    fn ib_grab_external_write_lock(db_path: &std::path::Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(db_path).expect("外部连接打开");
        conn.busy_timeout(std::time::Duration::from_secs(2))
            .expect("外部连接 busy_timeout");
        conn.execute_batch("BEGIN IMMEDIATE")
            .expect("外部 BEGIN IMMEDIATE");
        conn.execute(
            "INSERT INTO opencode_native_action_timings\
             (action_id, session_id, operation, queue_ms, ownership_ms, database_ms,\
              dispatch_ms, readback_ms, total_ms, budget_refused, created_at)\
             VALUES ('ib-external-lock','','',0,0,0,0,0,0,0,0)",
            [],
        )
        .expect("外部连接持锁写");
        conn
    }

    /// 计时行确定性读取：落账异步化后行写不与回执同步完成——轮询等待（10ms 步进
    /// 的 await 让 spawn 的落账任务得到调度；5 秒上限）。
    async fn he_timing_row_wait(
        rig: &Rig,
        action_id: &str,
    ) -> Option<crate::database::dao::opencode_action_timings::ActionTimingRow> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(row) = he_timing_row(rig, action_id) {
                return Some(row);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// IB RED a：**写权在握后跨截止 ⇒ 提交前复核拒绝，零提交零发送 /interrupt**。
    /// 预算内进 claim 事务（闸 ④ 已过）、claim UPDATE 成功（写权在握）→「写权后
    /// 持锁缝」把链确定性挂过预算截止（同位置同语义复现 GFix 口径的写权后等待，
    /// 缝选择理由见 `ClaimPostWriteHold` 文档）→ 放行后提交前必须复核：拒绝授权
    ///（claim 回滚、not_sent 墓碑 budget_exhausted、零 interrupt 写出、墓碑后
    /// claim 恒拒、观测位 budget_refused）。`database_ms >= 1500` 同时证明走的是
    /// **闸 ④ 之后的写权后等待**（不是闸 ④ 入口拒绝）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ib_write_lock_wait_crossing_budget_rolls_back_claim_and_never_sends() {
        let mut world = World::new();
        world.busy = Some(100);
        world.idle = None;
        let (rig, _db_path, _dispatch, _timing, claim_hold, _post_commit) = ib_boot(
            world,
            std::time::Duration::from_secs(5),
            false,
            false,
            true,
            false,
        )
        .await;
        let (view, claim) = rig
            .hub
            .view_and_claim(&rig.card, true)
            .await
            .expect("建立路由");
        assert!(claim.owned);
        let binding = view.binding_token;
        // 预算 2s：闸 ④ 在挂闸前通过（亚秒级到达），闸内挂 3s 把链拖过截止。
        rig.hub.set_action_budget_ms_for_tests(2_000);
        let hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let task = tokio::spawn(async move {
            hub.execute(&actor(), &card, stop_cmd("act_ib_a", &binding))
                .await
        });
        let hold = claim_hold.expect("写权后持锁缝必须装配");
        tokio::time::timeout(std::time::Duration::from_secs(10), hold.started.notified())
            .await
            .expect("必须到达写权后持锁缝（claim UPDATE 已成功、提交未发生）");
        // 闸内挂 3s（真实时钟）：runtime 计时器可能被 poll 内自旋的 worker 饿死
        //（RED c 同源问题）——OS 线程钟 + 纯 waker 等待。
        let bell = Arc::new(Notify::new());
        let bell_clone = Arc::clone(&bell);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(3_000));
            bell_clone.notify_one();
        });
        bell.notified().await;
        // 断言前先放行：同步自旋在 poll 内，测试失败路径也必须放它退出。
        hold.release
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .expect("跨截止定案必须尽快返回（不挂起）")
            .expect("任务不 panic");
        let err = result.expect_err("写权在握后跨过统一预算 ⇒ 提交前复核拒绝，绝不派发");
        assert_eq!(
            err.code,
            crate::services::opencode_native::budget::OC_DISPATCH_BUDGET_EXHAUSTED,
            "{err:?}"
        );
        // 「确定未发送」可核对结论落账（not_sent 墓碑 + budget_exhausted 既有语义）。
        let stored = rig.hub.action(DEVICE_ID, "act_ib_a").await.unwrap();
        assert_eq!(stored.state, ActionState::NotSent, "{stored:?}");
        assert_eq!(stored.reason_code.as_deref(), Some("budget_exhausted"));
        // 零迟到停止：零 /interrupt 写出；墓碑后 claim 恒拒（此后不得迟到取得派发权）。
        assert_eq!(he_interrupt_posts(&rig), 0, "跨截止 ⇒ 零迟到原生写");
        assert!(
            !rig.hub.action_store().claim("act_ib_a").expect("claim 读"),
            "确定未发送墓碑后不得再取得派发权"
        );
        // 测量面：budget_refused 观测位 + database_ms 证明等待发生在闸 ④ 之后
        //（写权后复核在提交前拒绝；若闸 ④ 自身拒绝，database_ms 不会累积挂闸窗）。
        let timing = he_timing_row_wait(&rig, "act_ib_a")
            .await
            .expect("计时行必须落账");
        assert!(
            timing.budget_refused,
            "预算拒绝必须进计时观测位: {timing:?}"
        );
        assert!(
            timing.database_ms >= 1_500,
            "写权后挂闸等待必须计入数据库段（证明 ④ 已通过、复核拒绝）: {timing:?}"
        );
    }

    /// IB RED b：**写权后复核不误伤**——预算内通过（闸即挂即放）⇒ 照常派发：
    /// 恰一次 /interrupt、Settled、零预算拒绝（既有回归的显式变体）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ib_within_budget_write_lock_wait_still_dispatches_normally() {
        let mut world = World::new();
        world.busy = Some(100);
        world.idle = None;
        let (rig, _db_path, _dispatch, _timing, claim_hold, _post_commit) = ib_boot(
            world,
            std::time::Duration::from_secs(5),
            false,
            false,
            true,
            false,
        )
        .await;
        let (view, _claim) = rig
            .hub
            .view_and_claim(&rig.card, true)
            .await
            .expect("建立路由");
        let binding = view.binding_token;
        rig.hub.set_action_budget_ms_for_tests(2_000);
        let hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let task = tokio::spawn(async move {
            hub.execute(&actor(), &card, stop_cmd("act_ib_b", &binding))
                .await
        });
        let hold = claim_hold.expect("写权后持锁缝必须装配");
        tokio::time::timeout(std::time::Duration::from_secs(10), hold.started.notified())
            .await
            .expect("必须到达写权后持锁缝");
        // 即挂即放（预算内）：复核必须放行（不误伤合法迟到）。
        hold.release
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .expect("预算内派发必须完成（不挂起）")
            .expect("任务不 panic");
        let view = result.expect("预算内拿到写权 ⇒ 照常派发");
        assert_eq!(view.state, ActionState::Settled, "{view:?}");
        assert_eq!(he_interrupt_posts(&rig), 1, "恰一次原生写");
        let timing = he_timing_row_wait(&rig, "act_ib_b")
            .await
            .expect("计时行必须落账");
        assert!(!timing.budget_refused, "预算内零预算拒绝: {timing:?}");
    }

    /// IB 外部锁申报测试（ruling #4 注入缝的现代行为申报）：外部连接 IMMEDIATE
    /// 持锁压住 claim 窗口。实测（rusqlite bundled SQLite 3.54.1；系统 3.54.0 同证，
    /// WAL/delete 两模式同证）：**读事务内的写升级立即返回 SQLITE_BUSY，busy handler
    /// 不被调用**——外部锁在 claim UPDATE 上不再产生「跨截止的等待」，而是快速失败
    ///（STORE_FAILED：claim 事务回滚 = 零提交、零发送、行留可重试 not_sent）。
    /// 本测试锁定该安全性：外部锁争用下零 /interrupt 写出。注：claim 拒绝本身瞬时
    /// 完成，任务返回还需等链尾的**计时落账**（autocommit 写，会吃满 busy_timeout
    /// 后失败——测量绝不影响结果）；GREEN 后计时落账异步化，返回时间显著缩短。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ib_external_write_lock_contention_yields_zero_late_send() {
        let mut world = World::new();
        world.busy = Some(100);
        world.idle = None;
        let (rig, db_path, dispatch_hold, _timing, _claim_hold, _post_commit) = ib_boot(
            world,
            std::time::Duration::from_secs(4),
            true,
            false,
            false,
            false,
        )
        .await;
        let (view, _claim) = rig
            .hub
            .view_and_claim(&rig.card, true)
            .await
            .expect("建立路由");
        let binding = view.binding_token;
        let hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let task = tokio::spawn(async move {
            hub.execute(&actor(), &card, stop_cmd("act_ib_lock", &binding))
                .await
        });
        let hold = dispatch_hold.expect("派发闸必须装配");
        tokio::time::timeout(std::time::Duration::from_secs(10), hold.started.notified())
            .await
            .expect("必须到达派发闸");
        // 闸内抓外部写锁（IMMEDIATE + 一行写）；放行后 claim UPDATE 撞锁。
        let external = ib_grab_external_write_lock(&db_path);
        let t0 = std::time::Instant::now();
        hold.release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(15), task)
            .await
            .expect("定案必须有界返回（busy_timeout 兜底，不挂起）")
            .expect("任务不 panic");
        let elapsed = t0.elapsed();
        let err = result.expect_err("外部持锁下 claim UPDATE 必须结构化失败");
        assert_eq!(err.code, action_codes::STORE_FAILED, "{err:?}");
        assert_eq!(he_interrupt_posts(&rig), 0, "外部持锁 ⇒ 零迟到原生写");
        // 有界返回证明：外部锁全程未释放（ROLLBACK 在断言之后），任务仍已定案
        //（busy_timeout=4s 上界 + 余量；绝不跨锁无限等待）。
        assert!(
            elapsed < std::time::Duration::from_secs(8),
            "定案必须有界（≤ busy_timeout 链尾写兜底）: {elapsed:?}"
        );
        // 行未被 finalize 成任何终态（STORE_FAILED 不落墓碑，留可重试 not_sent）。
        let stored = rig.hub.action(DEVICE_ID, "act_ib_lock").await.unwrap();
        assert_eq!(stored.state, ActionState::NotSent, "{stored:?}");
        external.execute_batch("ROLLBACK").expect("释放外部写锁");
        drop(external);
    }

    /// IB RED c：**计时落账移出回执等待路径**——挂住计时写库（落账闸）⇒ 回执照常
    /// 返回（闸内 execute 必须已定案）；放行后计时行异步落账到位（数据完整性保持，
    /// 结构化日志行恒输出是丢失窗口兜底）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ib_receipt_return_is_not_blocked_by_timing_persist_write() {
        let mut world = World::new();
        world.busy = Some(100);
        world.idle = None;
        let (rig, _db_path, _dispatch, timing_hold, _claim_hold, _post_commit) = ib_boot(
            world,
            std::time::Duration::from_secs(5),
            false,
            true,
            false,
            false,
        )
        .await;
        let (view, _claim) = rig
            .hub
            .view_and_claim(&rig.card, true)
            .await
            .expect("建立路由");
        let binding = view.binding_token;
        let hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let task = tokio::spawn(async move {
            hub.execute(&actor(), &card, stop_cmd("act_ib_c", &binding))
                .await
        });
        let hold = timing_hold.expect("计时落账闸必须装配");
        tokio::time::timeout(std::time::Duration::from_secs(10), hold.started.notified())
            .await
            .expect("计时落账必须到达闸");
        // 闸仍挂着：回执必须能返回——计时写库不在回执等待路径上。
        // 判定钟不用 runtime 计时器：RED 形态的落账自旋卡住一个 runtime worker
        //（可能恰好持着 timer driver），runtime 计时器会被饿死；普通 OS 线程的
        // 真实时钟 + 纯 waker 的 select 不依赖 driver。
        let bell = Arc::new(Notify::new());
        let bell_clone = Arc::clone(&bell);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(2_000));
            bell_clone.notify_one();
        });
        let outcome: Option<Result<ActionView, NativeError>> = tokio::select! {
            r = task => Some(r.expect("任务不 panic")),
            _ = bell.notified() => None,
        };
        // 断言前先放行闸：RED 形态（同步消费）的落账在 poll 内自旋，测试失败路径
        // 也必须放它退出（runtime drop 会等当前 poll 结束）。
        hold.release
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            outcome.is_some(),
            "回执等待路径不得被计时写库同步阻塞（闸内 execute 必须已定案）"
        );
        let view = outcome.expect("回执必须已返回").expect("停止派发");
        assert_eq!(view.state, ActionState::Settled, "{view:?}");
        assert_eq!(he_interrupt_posts(&rig), 1, "恰一次原生写");
        // 放行闸 ⇒ 计时行异步落账到位（计时数据完整性保持）。
        let timing = he_timing_row_wait(&rig, "act_ib_c")
            .await
            .expect("放行后计时行必须异步落账到位");
        assert_eq!(timing.operation, "stop_session");
        assert!(!timing.budget_refused, "{timing:?}");
        assert!(
            timing.total_ms > 0 && timing.dispatch_ms >= 0,
            "五段结构照常: {timing:?}"
        );
    }

    /// IB fix1 RED：**claim 提交完成后跨截止 ⇒ 原生请求零发包**（发包前复核 ⑥，
    /// 三层第三层）。claim 已提交（授权面成账 =「可能已发出」标记）→「提交后持锁
    /// 缝」把链确定性挂过预算截止 → 放行后、write_native 之前必须复核：**不开始
    /// 原生请求**。诚实分级：claim 提交后账本边界按设计不可判定（状态机明文
    /// 「dispatching 之后不得回退成 not_sent」，重启恢复同口径）⇒ 落 Unknown 族
    /// `Unconfirmed`（reason=OC_DISPATCH_BUDGET_EXHAUSTED，只按原 action ID 核对，
    /// 与丢回执同一既有语义），绝不冒充「确定未发送」。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ib_post_commit_budget_crossing_never_sends_native_packet() {
        let mut world = World::new();
        world.busy = Some(100);
        world.idle = None;
        let (rig, _db_path, _dispatch, _timing, _claim_hold, post_commit) = ib_boot(
            world,
            std::time::Duration::from_secs(5),
            false,
            false,
            false,
            true,
        )
        .await;
        let (view, claim) = rig
            .hub
            .view_and_claim(&rig.card, true)
            .await
            .expect("建立路由");
        assert!(claim.owned);
        let binding = view.binding_token;
        // 预算 2s：闸在提交后即刻到达（亚秒级），闸内挂 3s 把链拖过截止。
        rig.hub.set_action_budget_ms_for_tests(2_000);
        let hub = Arc::clone(&rig.hub);
        let card = rig.card.clone();
        let task = tokio::spawn(async move {
            hub.execute(&actor(), &card, stop_cmd("act_ib_pc", &binding))
                .await
        });
        let hold = post_commit.expect("提交后持锁缝必须装配");
        tokio::time::timeout(std::time::Duration::from_secs(10), hold.started.notified())
            .await
            .expect("必须到达提交后持锁缝（claim 已提交、原生请求未开始）");
        // 闸内挂 3s（真实时钟：runtime 计时器可能被 poll 内自旋的 worker 饿死——
        // OS 线程钟 + 纯 waker 等待）。
        let bell = Arc::new(Notify::new());
        let bell_clone = Arc::clone(&bell);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(3_000));
            bell_clone.notify_one();
        });
        bell.notified().await;
        // 断言前先放行闸：同步自旋在 poll 内，测试失败路径也必须放它退出。
        hold.release
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .expect("跨截止定案必须尽快返回（不挂起）")
            .expect("任务不 panic");
        // 诚实分级：claim 已提交 ⇒ Unknown 族（Unconfirmed + 预算码），200 视图；
        // **零包判据**：原生请求恰好零次（假 peer 服务端为准）。
        let view = result.expect("受控 Unknown 结论必须返回");
        assert_eq!(view.state, ActionState::Unconfirmed, "{view:?}");
        assert_eq!(
            view.reason_code.as_deref(),
            Some("OC_DISPATCH_BUDGET_EXHAUSTED"),
            "{view:?}"
        );
        assert_eq!(
            he_interrupt_posts(&rig),
            0,
            "提交后跨截止 ⇒ /interrupt 零发包"
        );
        // 账本核对面：行 = Unconfirmed + 预算码（只按原 action ID 核对的既有语义）。
        let stored = rig.hub.action(DEVICE_ID, "act_ib_pc").await.unwrap();
        assert_eq!(stored.state, ActionState::Unconfirmed, "{stored:?}");
        assert_eq!(
            stored.reason_code.as_deref(),
            Some("OC_DISPATCH_BUDGET_EXHAUSTED")
        );
        // 不再可派发（占用/未决保护在）：后续核对只按原 action ID 收敛。
        assert!(
            !rig.hub.action_store().claim("act_ib_pc").expect("claim 读"),
            "Unconfirmed 行不得再取得派发权"
        );
        // 测量面：budget_refused 进观测位；total_ms 覆盖闸内等待（证明等待发生在
        // 提交后、发包前的链内，而非 ④/⑤ 任一闸）；派发段必须为零（零包佐证）。
        let timing = he_timing_row_wait(&rig, "act_ib_pc")
            .await
            .expect("计时行必须落账");
        assert!(
            timing.budget_refused,
            "预算拒绝必须进计时观测位: {timing:?}"
        );
        assert!(
            timing.total_ms >= 2_500,
            "提交后挂闸等待必须计入全链总耗时: {timing:?}"
        );
        assert_eq!(
            timing.dispatch_ms, 0,
            "原生派发段必须为零（零包发出的测量面佐证）: {timing:?}"
        );
    }

    // ==== H5 活体链测量（#[ignore]，`cargo test --lib he_measure_ -- --ignored --nocapture`
    // 显式运行；门禁套件不含）。五场景的进程级活体：真实 execute 动作链 + 真实回环
    // HTTP（假 peer）+ 真实 SQLite 文件库 + 真实分段计时落账。整机活体缺口（多服务
    // 保守闸 fail-closed 环境阻塞）在 task-HE-report 申报。====

    /// 文件库 rig：与 boot() 同一装配，但 DeviceStore 落真实 SQLite 文件
    ///（DB 段计时的真实锁与 fsync 面）。
    async fn he_boot_file_rig(world: World) -> (Rig, std::path::PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).expect("建项目目录");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("建 home");
        let state_file = tmp.path().join("state/opencode/service.json");
        let card = card_for(SESSION_ID, &project);
        let mut world = world;
        world.directory = project.to_string_lossy().into_owned();
        let peer = Peer::start(world, &state_file).await;
        write_registration(&state_file, peer.port);
        let db_path = tmp.path().join("he-measure.db");
        // HG 必办①（HR1b HE-1 裁定）：rig 建库必须走与生产同一条 `open_and_init_once`
        // ——裸 open+schema 绕过 `PRAGMA journal_mode=WAL`，此前 HE 报告「WAL 轮」列
        // 实为 delete 测量（列名失真）。busy_timeout=5s 与生产 init 同值。
        let conn = crate::database::connection::open_and_init_once(
            &db_path,
            true,
            std::time::Duration::from_secs(5),
        )
        .map_err(|e| {
            format!("按生产初始化路径打开测量文件库失败（open_and_init_once，含 WAL）: {e}")
        })
        .expect("测量文件库初始化失败");
        let rig_journal_mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .expect("读 journal_mode");
        assert_eq!(
            rig_journal_mode, "wal",
            "rig 库必须实际处于 WAL 模式（必办①真形态前提）"
        );
        // 与 boot() 同一装配面（coordinator facts；B1 归属证据面同源），仅库换文件形态。
        let fact = process_fact();
        let bridged = Arc::clone(&fact);
        let generation: Arc<dyn Fn(u32) -> super::ProcessFactProbe + Send + Sync> =
            Arc::new(move |pid: u32| match bridged(pid) {
                Some(started) => super::ProcessFactProbe::Alive(started),
                None => super::ProcessFactProbe::ReadFailure,
            });
        let hub = NativeHub::with_coordinator_facts(
            DeviceStore::Owned(std::sync::Arc::new(std::sync::Mutex::new(conn))),
            DiscoveryInput { state_file, home },
            fact,
            generation,
        );
        hub.store().with(|c| {
            crate::remote::pairing::persist_device(
                c,
                &crate::remote::pairing::NewDevice {
                    id: DEVICE_ID.into(),
                    name: DEVICE_NAME.into(),
                    ua: "he-measure".into(),
                    origin_ip: "127.0.0.1".into(),
                    via: "lan".into(),
                    paired_at: now_ms(),
                },
            )
            .unwrap()
        });
        let rig = Rig {
            _tmp: tmp,
            project,
            card,
            peer,
            hub: Arc::new(hub),
        };
        (rig, db_path)
    }

    async fn he_dump_timing(rig: &Rig, action_id: &str, tag: &str) {
        match he_timing_row_wait(rig, action_id).await {
            Some(t) => println!(
                "MEASURE [{tag}] {action_id} queue={} ownership={} database={} dispatch={} readback={} total={} budget_refused={}",
                t.queue_ms,
                t.ownership_ms,
                t.database_ms,
                t.dispatch_ms,
                t.readback_ms,
                t.total_ms,
                t.budget_refused
            ),
            None => println!("MEASURE [{tag}] {action_id} NO-TIMING-ROW"),
        }
    }

    /// M1+M2：空闲停止 / 忙窗停止（基线全链分段）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "H5 活体链测量：显式 --ignored 运行"]
    async fn he_measure_idle_and_busy_stop_chain() {
        let (rig, _db) = he_boot_file_rig(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let t0 = std::time::Instant::now();
        let view = rig
            .hub
            .execute(&actor(), &rig.card, stop_cmd("act_he_m1", &binding))
            .await
            .expect("空闲停止");
        println!(
            "MEASURE [m1_idle] wall={:.3}s state={:?} reason={:?} interrupt_posts={}",
            t0.elapsed().as_secs_f64(),
            view.state,
            view.reason_code,
            he_interrupt_posts(&rig)
        );
        he_dump_timing(&rig, "act_he_m1", "m1_idle").await;
        rig.peer.set(|world| {
            world.busy = Some(100);
            world.idle = None;
        });
        let t0 = std::time::Instant::now();
        let view = rig
            .hub
            .execute(&actor(), &rig.card, stop_cmd("act_he_m2", &binding))
            .await
            .expect("忙窗停止");
        println!(
            "MEASURE [m2_busy] wall={:.3}s state={:?} reason={:?} interrupt_posts={}",
            t0.elapsed().as_secs_f64(),
            view.state,
            view.reason_code,
            he_interrupt_posts(&rig)
        );
        he_dump_timing(&rig, "act_he_m2", "m2_busy").await;
        assert_eq!(he_interrupt_posts(&rig), 2, "两场景各恰一次原生写");
    }

    /// M3：并发 3 发停止（GFix 口径）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "H5 活体链测量：显式 --ignored 运行"]
    async fn he_measure_concurrent_three_stops_chain() {
        let (rig, _db) = he_boot_file_rig(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let t0 = std::time::Instant::now();
        let mut tasks = Vec::new();
        for i in 1..=3 {
            let hub = Arc::clone(&rig.hub);
            let card = rig.card.clone();
            let binding = binding.clone();
            tasks.push(tokio::spawn(async move {
                let t0 = std::time::Instant::now();
                let view = hub
                    .execute(
                        &actor(),
                        &card,
                        stop_cmd(&format!("act_he_m3_{i}"), &binding),
                    )
                    .await
                    .expect("并发停止");
                (i, t0.elapsed().as_secs_f64(), view.state, view.reason_code)
            }));
        }
        for task in tasks {
            let (i, wall, state, reason) = task.await.expect("任务");
            println!(
                "MEASURE [m3_concurrent] #{i} wall={wall:.3}s state={state:?} reason={reason:?}"
            );
            he_dump_timing(&rig, &format!("act_he_m3_{i}"), "m3_concurrent").await;
        }
        println!(
            "MEASURE [m3_concurrent] total_wall={:.3}s interrupt_posts={}",
            t0.elapsed().as_secs_f64(),
            he_interrupt_posts(&rig)
        );
        assert_eq!(
            he_interrupt_posts(&rig),
            3,
            "并发 3 发恰 3 次原生写（零重发）"
        );
    }

    /// M4：外部独占锁 40s 对峙（GFix 口径）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "H5 活体链测量：显式 --ignored 运行"]
    async fn he_measure_db_lock_40s_chain() {
        let (rig, db_path) = he_boot_file_rig(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        let lock_path = db_path.clone();
        let holder = std::thread::spawn(move || {
            let conn = rusqlite::Connection::open(&lock_path).expect("外部锁连接");
            conn.busy_timeout(std::time::Duration::from_millis(200))
                .ok();
            conn.execute_batch(
                "BEGIN EXCLUSIVE; INSERT OR REPLACE INTO settings (key,value) VALUES ('he_m4_probe','held');",
            )
            .expect("外部锁落位");
            println!("MEASURE [m4_dblock] lock-held 40s");
            std::thread::sleep(std::time::Duration::from_secs(40));
            let _ = conn.execute_batch("ROLLBACK");
            println!("MEASURE [m4_dblock] lock-released");
        });
        std::thread::sleep(std::time::Duration::from_millis(500));
        // HG 必办①（WAL 真形态）：外部 EXCLUSIVE 写者持锁窗内，**读者不被阻塞**——
        // WAL 库的新读连接立即读到快照（delete 模式下会被 SHARED→EXCLUSIVE 拉住）。
        let reader_probe = {
            let t = std::time::Instant::now();
            let rconn = rusqlite::Connection::open_with_flags(
                &db_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .expect("WAL 只读读者连接");
            let n: i64 = rconn
                .query_row("SELECT COUNT(*) FROM settings", [], |r| r.get(0))
                .expect("读者快照读");
            (t.elapsed(), n)
        };
        println!(
            "MEASURE [m4_dblock] reader_probe={:?} settings_rows={}（锁窗内读者零阻塞）",
            reader_probe.0, reader_probe.1
        );
        assert!(
            reader_probe.0 < std::time::Duration::from_secs(1),
            "WAL 真形态：锁窗内读者必须不被外部写者阻塞（实测 {:?}）",
            reader_probe.0
        );
        let t0 = std::time::Instant::now();
        let result = rig
            .hub
            .execute(&actor(), &rig.card, stop_cmd("act_he_m4", &binding))
            .await;
        let wall = t0.elapsed().as_secs_f64();
        holder.join().expect("持锁线程");
        match &result {
            Ok(view) => println!(
                "MEASURE [m4_dblock] wall={wall:.3}s state={:?} reason={:?}",
                view.state, view.reason_code
            ),
            Err(err) => println!(
                "MEASURE [m4_dblock] wall={wall:.3}s ERR code={} msg={}",
                err.code, err.message
            ),
        }
        he_dump_timing(&rig, "act_he_m4", "m4_dblock").await;
        // 账本核对（锁释放后）：本形态零动作行，GET 404
        //（「404 只表示当前未读到」语义的实测面）。
        let stored = rig.hub.action(DEVICE_ID, "act_he_m4").await;
        println!(
            "MEASURE [m4_dblock] ledger={:?}",
            stored
                .map(|v| format!("state={:?} reason={:?}", v.state, v.reason_code))
                .unwrap_or_else(|e| format!("404/{code}", code = e.code))
        );
        // WAL 真形态断言（HG 必办①，delete 形态对照见 task-HE-report §2）：
        // * 读者不再被外部写者阻塞 → 链条越过入口读（device_valid），失败点**移至首个写**
        //   ——错误码为写失败 OC_STORE_FAILED，不再是读失败折叠出的 OC_ACTION_NOT_FOUND；
        // * 失败墙钟 ≈ 1× busy_timeout（写段等待），不再是 delete 形态的 2×（业务读 +
        //   计时落账各烧一段）；墓碑写同被拒 → 账本零动作行零墓碑（404）。
        // * 计时行仍无法落账（计时账本与业务同库同锁 = 测量盲区不变，日志行兜底）。
        match &result {
            Err(err) => {
                assert_eq!(
                    err.code, "OC_STORE_FAILED",
                    "WAL 真形态：失败点应是首个写（OC_STORE_FAILED），而非读失败折叠的 NOT_FOUND"
                );
                assert!(
                    wall < 10.0,
                    "WAL 真形态：墙钟应 ≈1× busy_timeout（实测 {wall:.3}s，delete 形态 ≈2×=10.7s）"
                );
            }
            Ok(view) => panic!(
                "WAL 真形态：外部 EXCLUSIVE 独占窗内不应写出成功（state={:?} reason={:?}）",
                view.state, view.reason_code
            ),
        }
        assert_eq!(he_interrupt_posts(&rig), 0, "锁窗内零原生写");
    }

    /// M5：CPU 风暴饱和（8 路自旋，GFix 口径）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "H5 活体链测量：显式 --ignored 运行"]
    async fn he_measure_cpu_storm_chain() {
        let (rig, _db) = he_boot_file_rig(World::new()).await;
        claim_route(&rig);
        let binding = current_binding(&rig).await;
        rig.peer.set(|world| {
            world.busy = Some(100);
            world.idle = None;
        });
        let stop_flag = Arc::new(AtomicBool::new(false));
        let mut burners = Vec::new();
        for _ in 0..8 {
            let flag = Arc::clone(&stop_flag);
            burners.push(std::thread::spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    let mut x = 0u64;
                    for i in 0..100_000 {
                        x = x.wrapping_add(i);
                    }
                    std::hint::black_box(x);
                }
            }));
        }
        let t0 = std::time::Instant::now();
        let view = rig
            .hub
            .execute(&actor(), &rig.card, stop_cmd("act_he_m5", &binding))
            .await
            .expect("风暴中停止");
        let wall = t0.elapsed().as_secs_f64();
        stop_flag.store(true, Ordering::Relaxed);
        for b in burners {
            b.join().expect("风暴线程");
        }
        println!(
            "MEASURE [m5_storm] wall={wall:.3}s state={:?} reason={:?} interrupt_posts={}",
            view.state,
            view.reason_code,
            he_interrupt_posts(&rig)
        );
        he_dump_timing(&rig, "act_he_m5", "m5_storm").await;
        assert_eq!(he_interrupt_posts(&rig), 1, "风暴中恰一次原生写");
    }
}
