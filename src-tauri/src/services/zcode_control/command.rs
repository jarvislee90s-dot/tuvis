//! ZCode 官方桥的写命令派发（生产计划 T3 · Slice B）。
//!
//! 本文件是官方协议**唯一**的会话写出口：HTTP/前端只能提交 [`ControlAction`]，
//! 信封、clientId、commandId、正文签名都在这里生成，手机无法传入任意 wire。
//!
//! # 单次语义（E3 证据：`SendOnceLoop` @ 固定 SHA）
//!
//! - 一个动作在派发前先经 [`ActionStore::reserve`] 落 `reserved` 行（Slice A；
//!   回执一律映射为保守 `unknown`）：同 actionId 同载荷 → 直接返回既有回执，
//!   **零帧**；载荷不同 → `action_payload_conflict`；
//! - 只有 `New` 才构造**恰一个** `sendConversationCommandV4` 帧；无重发计时器、
//!   无第二次 sendText、无 CLI 降级路径；
//! - ACK 窗 20s（[`ACK_TIMEOUT`]）：该窗口**原样交给端口**，由桥的 RPC 待答条目
//!   按同一期限收口并清账（`not_ready`）；本层**不**再外套超时（外层取消会绕过
//!   桥的 pending 清理）。窗内无应答 → `unknown`（写没写出未知），绝不自动重发；
//!   只有桥**确证零写**的绑定/编码拒绝（`stale_generation` / `no_session` /
//!   `payload_too_large`）才给 `send_failed`（`not_ready` 同时覆盖「就绪闸拒写」
//!   与「写后等应答超窗」，不可区分 → 保守 `unknown`）；
//! - 已 `accepted` 不被无关活动/迟到的失败证据降级（`ActionStore::record_result`
//!   的恰好一次 CAS）。
//!
//! # 正文纪律
//!
//! [`compose_official_text`] 是本路线**唯一**正文出口：保留真实换行/中文/emoji
//! （绝不走终端注入的 `normalize_newlines`，那会把换行变字面 `\n` 两个字符）；
//! 签名开关单点复用 [`message_signature_enabled_conn`]（设置缺省 off），`/` 开头
//! 不加后缀。审计/日志/错误只含类别与计数，不摘正文。
//!
//! # 问答与 snooze（T4.1 Slice B2 + Fix round 1）
//!
//! `Answer` 走「只读幂等预检 → 宿主授权 → 原值映射 → **写前才预留** →（必要时
//! snooze）→ 恰一帧 `resolveInteraction`」：授权/映射失败零帧、零账本行、零审计。
//! `autoResolution` 在场的卡必须先收到 snooze 的 accepted/duplicate/**noop**
//! ACK（noop = 已暂停的幂等回放）**且**卡仍在宿主集合，否则零 resolve。
//! ACK accepted/duplicate 只算 `accepted`（本地消费，**不等于**宿主撤卡），只有
//! ACK 后 10s 内观察到**精确目标**的 pending 代数前进且该 id 消失才升级
//! `confirmed`。`Snooze` 独立动作只暂停计时，不消费卡（结果无论如何都释放本地
//! 在途）。在途闸只在确证零写（send_failed / snooze 失败）、resolve
//! rejected/noop、或宿主撤卡帧（`PendingBook` 整体替换时缺席）时释放；写后
//! unknown 保留闸（绝不自动重发）。答案正文只穿过 wire 与内存，账本只存指纹。

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{Map, Value};

use crate::inject::normalize::message_signature_enabled_conn;
use crate::remote::pairing::DeviceStore;
use crate::services::codex_desktop_ipc::types::new_uuid;

use super::action::{action_params, audit_summary, payload_hash, ActionStore, Reservation};
use super::bridge::{BridgeManager, AGENT_CHANNEL};
use super::pending::build_user_answer;
use super::types::{
    codes, ActionContext, ActionReceipt, ActionState, BoundTarget, CommandPort, ControlAction,
    PendingPort, ZResult, ZcodeError,
};

/// 官方发送方法名（`zcodeAgent.ts:937`、探针 `SendOnceLoop`）。
pub const AGENT_SEND_COMMAND_METHOD: &str = "sendConversationCommandV4";

/// 信封 `type`（普通文本；`command.ts:95-135` 的 `sendText` 只必带 `text`）。
pub const SEND_TEXT_TYPE: &str = "sendText";

/// 信封 `type`（问答应答；探针 `INTERACTION_COMMAND_TYPES.resolve`，
/// `command.ts:221-234`：payload `{interactionId, answer}`）。
pub const RESOLVE_INTERACTION_TYPE: &str = "resolveInteraction";

/// ACK 等待窗（20s，与生产计划一致）：原样交给端口（桥的 RPC 待答条目按它收口
/// 并清账），窗内无应答按保守 `unknown`，绝不重发。
pub const ACK_TIMEOUT: Duration = Duration::from_secs(20);

/// 应答 ACK 后等待宿主撤卡的观察窗（10s，探针 `INTERACTION_TERMINAL_WAIT_MS`）：
/// 只有宿主 pending 代数前进且该 id 消失才升级 `confirmed`，否则停在 `accepted`。
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(10);

/// 单条正文的 UTF-8 字节预算。
///
/// 推导（最坏物理路径）：正文进入消息 JSON 后最坏按 `\u00XX` 转义膨胀 6×
/// （控制字符密集），整条消息随后被 [`super::codec::encode_frame`] Base64 进
/// `dataBase64` 再按 1 MiB 物理信封计量，Base64 再膨胀 4/3 → 合计最坏 8×。
/// 因此预算 = (`MAX_PHYSICAL_FRAME_BYTES` − 8 KiB 固定开销) / 8：**通过本预算的
/// 正文必然能通过编码**（不会出现「预检放行、编码期 `payload_too_large`」）。
/// 超预算在**写帧之前**以 `payload_too_large` 拒绝（零帧零行）。
pub const MAX_TEXT_BYTES: usize =
    (super::codec::MAX_PHYSICAL_FRAME_BYTES.saturating_sub(8_192)) / 8;

/// 生成一次命令 id：rand 128-bit UUIDv4（复用既有 `new_uuid`，不新增依赖）。
pub fn new_command_id() -> String {
    new_uuid()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn invalid_action() -> ZcodeError {
    ZcodeError::of(codes::INVALID_ACTION)
}

/// 组装官方发送正文（本路线**唯一**正文出口）。
///
/// - 纯空白（含只有换行/制表符）→ `invalid_action`（零帧零行）；
/// - 正文原样保留真实换行/中文/emoji，**不做**终端 `normalize_newlines`；
/// - `signature_on` 且正文不以 `/` 开头 → 追加 ` [mobile <设备花名>]`（花名内的
///   换行折叠为空格，避免伪造消息行；花名为空则不追加）；
/// - 组装后超 [`MAX_TEXT_BYTES`] → `payload_too_large`。
pub fn compose_official_text(device_name: &str, text: &str, signature_on: bool) -> ZResult<String> {
    if text.trim().is_empty() {
        return Err(invalid_action());
    }
    let mut composed = String::with_capacity(text.len() + device_name.len() + 12);
    composed.push_str(text);
    if signature_on && !text.starts_with('/') {
        let folded: String = device_name
            .chars()
            .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
            .collect();
        let name = folded.trim();
        if !name.is_empty() {
            composed.push_str(" [mobile ");
            composed.push_str(name);
            composed.push(']');
        }
    }
    if composed.len() > MAX_TEXT_BYTES {
        return Err(ZcodeError::of(codes::PAYLOAD_TOO_LARGE));
    }
    Ok(composed)
}

/// ACK 分类（官方 `CommandAck`：`status` ∈ accepted/duplicate/rejected/noop）。
///
/// 只认显式状态：无法解释的应答既不当受理也不谎称拒绝，按保守 `unknown`。
fn classify_ack(ack: &Value) -> (ActionState, Option<String>) {
    match ack.get("status").and_then(Value::as_str) {
        Some("accepted") | Some("duplicate") => (ActionState::Accepted, None),
        Some("noop") => (ActionState::Noop, None),
        Some("rejected") => (ActionState::Rejected, None),
        _ => (ActionState::Unknown, Some(codes::UNKNOWN.to_owned())),
    }
}

/// 端口错误分类。
///
/// 只有桥**保证零写**的失败（写前绑定/编码拒绝）才是 `send_failed`；
/// `offline`/`not_ready`/`unknown` 可能发生在写帧之后（断网、ACK 超窗、宿主
/// PromiseError），一律按保守 `unknown`——绝不让手机误以为「确定没发出」而重发。
fn classify_port_error(error: &ZcodeError) -> (ActionState, Option<String>) {
    let state = match error.code.as_str() {
        codes::STALE_GENERATION | codes::NO_SESSION | codes::PAYLOAD_TOO_LARGE => {
            ActionState::SendFailed
        }
        _ => ActionState::Unknown,
    };
    (state, Some(error.code.clone()))
}

/// 一次写动作的派发器（生产：`BridgeManager` + 全局库；测试：fake 端口 + 内存库）。
pub struct CommandSender {
    bridge: Arc<dyn CommandPort>,
    actions: ActionStore,
    store: DeviceStore,
    ack_timeout: Duration,
    /// pending 授权 / snooze / 宿主终态端口：`Answer`/`Snooze` 必需，生产接线在 T7.1。
    pending: Option<Arc<dyn PendingPort>>,
    /// 宿主撤卡观察窗（测试注入短窗；生产 [`CONFIRM_TIMEOUT`]）。
    confirm_timeout: Duration,
}

impl CommandSender {
    /// 生产构造：ACK 窗取 [`ACK_TIMEOUT`]；尚未接 pending 端口（T7.1 用
    /// [`Self::with_pending`] 装配，未装配时 Answer/Snooze 一律 `not_ready`）。
    pub fn new(bridge: Arc<dyn CommandPort>, store: DeviceStore) -> Self {
        Self::with_timeout(bridge, store, ACK_TIMEOUT)
    }

    /// 注入 ACK 窗（测试专用：20s 用例不真等 20s）。
    pub(crate) fn with_timeout(
        bridge: Arc<dyn CommandPort>,
        store: DeviceStore,
        ack_timeout: Duration,
    ) -> Self {
        Self {
            bridge,
            actions: ActionStore::new(store.clone()),
            store,
            ack_timeout,
            pending: None,
            confirm_timeout: CONFIRM_TIMEOUT,
        }
    }

    /// 装配 pending 授权 / snooze / 宿主终态端口（生产 = `BridgeManager`，T7.1 接线）。
    pub fn with_pending(mut self, pending: Arc<dyn PendingPort>) -> Self {
        self.pending = Some(pending);
        self
    }

    /// 注入宿主撤卡观察窗（测试专用：10s 用例不真等 10s）。
    #[cfg(test)]
    pub(crate) fn with_confirm_timeout(mut self, confirm_timeout: Duration) -> Self {
        self.confirm_timeout = confirm_timeout;
        self
    }

    /// 执行一次控制动作。
    ///
    /// 已接入 `Send`/`Answer`/`Snooze`；其余 `ControlAction` 变体一律 `invalid_action`
    /// 且零帧零行（T4.2–T6 逐个接入，绝不伪造成功）。Answer/Snooze 在未装配 pending
    /// 端口时一律 `not_ready`（零帧零行，生产接线在 T7.1）。
    pub async fn execute(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        action: &ControlAction,
    ) -> ZResult<ActionReceipt> {
        self.check_action(ctx, target, action)?;
        match action {
            ControlAction::Send { .. } => self.execute_send(ctx, target, action).await,
            ControlAction::Answer { .. } => self.execute_answer(ctx, target, action).await,
            ControlAction::Snooze { .. } => self.execute_snooze(ctx, target, action).await,
            _ => Err(invalid_action()),
        }
    }

    /// 动作身份校验（所有变体共用）：空 actionId 会成为全局幂等键，必须拒绝。
    fn check_action(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        action: &ControlAction,
    ) -> ZResult<()> {
        if ctx.action_id.trim().is_empty() {
            // 空 actionId 会成为全局幂等键（第一个空 id 动作占位，后续一律冲突）。
            return Err(invalid_action());
        }
        if action.session_id() != target.session_id {
            return Err(invalid_action());
        }
        if action.generation() != target.generation {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        Ok(())
    }

    /// 普通文本发送。
    async fn execute_send(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        action: &ControlAction,
    ) -> ZResult<ActionReceipt> {
        let ControlAction::Send { text, .. } = action else {
            return Err(invalid_action());
        };
        // 正文出口（含空白/预算校验）：校验失败零帧零行，不去碰账本。
        let signature_on = self.store.with(message_signature_enabled_conn);
        let composed = compose_official_text(&ctx.device_name, text, signature_on)?;

        // 幂等指纹只取原始动作参数（不含签名产物）：签名开关变化不改变同一
        // actionId 的语义身份，也绝不因此产生第二次写出。
        let params = action_params(action);
        let hash = payload_hash(
            action.kind(),
            &target.session_id,
            target.generation,
            &params,
        )?;
        // 审计摘要只含类别 + 字数（answer/permission 为答案/选项数），不含正文。
        let summary = audit_summary(action.kind(), &params);
        match self
            .actions
            .reserve_with_kind(ctx, target, action.kind(), &hash, &summary)?
        {
            Reservation::Existing(receipt) => return Ok(receipt),
            Reservation::New => {}
        }

        let receipt = self.dispatch_send(ctx, target, composed).await;
        self.record(&receipt);
        Ok(receipt)
    }

    /// 预留动作（幂等指纹 = kind + 会话 + 代际 + 全部参数，只落指纹不落正文）。
    ///
    /// 同 actionId 同载荷 → 既有回执（调用方直接返回，零帧）；不同载荷/跨设备 → 冲突。
    fn reserve_action(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        action: &ControlAction,
    ) -> ZResult<Option<ActionReceipt>> {
        let params = action_params(action);
        let hash = payload_hash(
            action.kind(),
            &target.session_id,
            target.generation,
            &params,
        )?;
        let summary = audit_summary(action.kind(), &params);
        match self
            .actions
            .reserve_with_kind(ctx, target, action.kind(), &hash, &summary)?
        {
            Reservation::Existing(receipt) => Ok(Some(receipt)),
            Reservation::New => Ok(None),
        }
    }

    /// 只读查询既有回执（不写行/审计）：本地校验与授权之前先判幂等重放。
    fn lookup_action(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        action: &ControlAction,
    ) -> ZResult<Option<ActionReceipt>> {
        let params = action_params(action);
        let hash = payload_hash(
            action.kind(),
            &target.session_id,
            target.generation,
            &params,
        )?;
        self.actions
            .existing_receipt(ctx, target, action.kind(), &hash)
    }

    /// 问答应答：只读幂等预检 → 宿主授权 → 原值映射 → **写前才预留** →
    /// （必要时 snooze）→ 恰一帧 resolve。
    ///
    /// 授权/映射失败发生在预留之前：零帧、零账本行、零审计，actionId 不被占死。
    async fn execute_answer(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        action: &ControlAction,
    ) -> ZResult<ActionReceipt> {
        let ControlAction::Answer {
            interaction_id,
            answers,
            ..
        } = action
        else {
            return Err(invalid_action());
        };
        let Some(pending) = self.pending.as_ref() else {
            // 未装配 pending 端口：不能证明卡在宿主在场，零帧零行。
            return Err(ZcodeError::of(codes::NOT_READY));
        };
        // 幂等重放（同 actionId 同载荷）：只回既有回执，零新行、零帧。
        if let Some(existing) = self.lookup_action(ctx, target, action)? {
            return Ok(existing);
        }
        // 宿主观察基线（精确目标）：终态要求「该目标代数前进且该 id 不在其集合」。
        let baseline = pending.host_pending_revision(target);
        let card = pending.authorize_pending(target, interaction_id).await?;
        // 原值映射（含 schema 冻结闸/选项集合/重名题干）：失败零帧零行、释放本地在途。
        let answer = match build_user_answer(&card, answers) {
            Ok(answer) => answer,
            Err(error) => {
                pending.clear_local_inflight(target, interaction_id);
                return Err(error);
            }
        };
        // 写即将发生：此时才预留（审计的 started 只属于真正会离机的动作）。
        match self.reserve_action(ctx, target, action) {
            Ok(None) => {}
            Ok(Some(existing)) => {
                pending.clear_local_inflight(target, interaction_id);
                return Ok(existing);
            }
            Err(error) => {
                pending.clear_local_inflight(target, interaction_id);
                return Err(error);
            }
        }
        // autoResolution 在场：先 snooze（恰一帧）且必须 Ok（accepted/duplicate/
        // noop=已暂停）且卡仍在宿主集合，否则零 resolve。
        if card.auto_resolution {
            if let Err(error) = pending
                .snooze(target, interaction_id, &ctx.device_id, self.ack_timeout)
                .await
            {
                // resolve 从未写出（可证明零写）：释放在途允许重试；snooze 本身
                // 幂等（重复/迟到 noop 属已暂停），不会造成双暂停副作用。
                pending.clear_local_inflight(target, interaction_id);
                let (state, reason_code) = classify_port_error(&error);
                let receipt = self.make_receipt(ctx, target, state, reason_code);
                self.record(&receipt);
                return Ok(receipt);
            }
            if !pending.host_has_pending(target, interaction_id) {
                // snooze 已写出但宿主集合已无此卡：不猜是否受理，零 resolve。
                pending.clear_local_inflight(target, interaction_id);
                let receipt = self.make_receipt(
                    ctx,
                    target,
                    ActionState::Unknown,
                    Some(codes::STALE_PENDING.to_owned()),
                );
                self.record(&receipt);
                return Ok(receipt);
            }
        }

        let mut receipt = self
            .dispatch_resolve(ctx, target, interaction_id, answer)
            .await;
        if receipt.state != ActionState::Accepted
            && !pending.host_has_pending(target, interaction_id)
        {
            receipt.state = ActionState::Unknown;
            receipt.reason_code = Some(codes::STALE_PENDING.to_owned());
        }
        if receipt.state == ActionState::Accepted {
            // 本地消费 ≠ 宿主撤卡：先收口在途，再等宿主自己的整体替换。
            pending.mark_resolved_local(target, interaction_id);
            self.record(&receipt);
            if pending
                .wait_host_cleared(target, interaction_id, baseline, self.confirm_timeout)
                .await
            {
                // 只有「代数前进 + 卡消失」才是因果终态；超时停在 accepted。
                receipt.state = ActionState::Confirmed;
                receipt.reason_code = None;
            }
        } else if receipt.state == ActionState::SendFailed {
            // 确证零写：释放本地在途，用户可重试同一张卡。
            pending.clear_local_inflight(target, interaction_id);
        } else if matches!(receipt.state, ActionState::Rejected | ActionState::Noop) {
            // 宿主显式拒绝/无需处理且卡仍在：本次应答可证明未生效，放开重试；
            // 写后 unknown（含 ACK 超窗）保持闸（防双目重复作答），只由宿主
            // 后续撤卡帧或换代释放。
            pending.clear_local_inflight(target, interaction_id);
        }
        self.record(&receipt);
        Ok(receipt)
    }

    /// 暂停问答卡自动结束计时（独立动作）：只读幂等预检 → 授权 → 写前预留 →
    /// 恰一帧 snooze。不消费卡，结果无论如何都释放本地在途。
    async fn execute_snooze(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        action: &ControlAction,
    ) -> ZResult<ActionReceipt> {
        let ControlAction::Snooze { interaction_id, .. } = action else {
            return Err(invalid_action());
        };
        let Some(pending) = self.pending.as_ref() else {
            return Err(ZcodeError::of(codes::NOT_READY));
        };
        if let Some(existing) = self.lookup_action(ctx, target, action)? {
            return Ok(existing);
        }
        pending.authorize_pending(target, interaction_id).await?;
        match self.reserve_action(ctx, target, action) {
            Ok(None) => {}
            Ok(Some(existing)) => {
                pending.clear_local_inflight(target, interaction_id);
                return Ok(existing);
            }
            Err(error) => {
                pending.clear_local_inflight(target, interaction_id);
                return Err(error);
            }
        }
        let result = pending
            .snooze(target, interaction_id, &ctx.device_id, self.ack_timeout)
            .await;
        // snooze 不消费卡：用户之后仍要作答，必须放开本地在途闸。
        pending.clear_local_inflight(target, interaction_id);
        match result {
            Ok(()) => {
                let receipt = self.make_receipt(ctx, target, ActionState::Accepted, None);
                self.record(&receipt);
                Ok(receipt)
            }
            Err(error) => {
                // 冻结映射：rejected → invalid_action、超窗 → unknown（noop 已是
                // 幂等成功）；两类都按错误上抛（不是成功回执），账本按可证明性
                // 保守收口。
                let (state, reason_code) = classify_port_error(&error);
                let receipt = self.make_receipt(ctx, target, state, reason_code);
                self.record(&receipt);
                Err(error)
            }
        }
    }

    /// 构造并**恰一次**调用 resolveInteraction 端口（channel/method/信封与探针一致）。
    async fn dispatch_resolve(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        interaction_id: &str,
        answer: Value,
    ) -> ActionReceipt {
        let envelope = serde_json::json!({
            "commandId": new_command_id(),
            // clientId 只来自可信调用方上下文（配对门/本机 DB）。
            "clientId": ctx.device_id,
            "sessionId": target.session_id,
            "type": RESOLVE_INTERACTION_TYPE,
            "payload": { "interactionId": interaction_id, "answer": answer },
            "issuedAt": now_ms(),
        });
        let (state, reason_code) = match self
            .bridge
            .call(
                target,
                AGENT_CHANNEL,
                AGENT_SEND_COMMAND_METHOD,
                vec![envelope],
                self.ack_timeout,
            )
            .await
        {
            Err(error) => classify_port_error(&error),
            Ok(ack) => classify_ack(&ack),
        };
        self.make_receipt(ctx, target, state, reason_code)
    }

    /// 组装回执（actionId/session/generation 只来自可信上下文）。
    fn make_receipt(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        state: ActionState,
        reason_code: Option<String>,
    ) -> ActionReceipt {
        ActionReceipt {
            action_id: ctx.action_id.clone(),
            session_id: target.session_id.clone(),
            generation: target.generation,
            state,
            reason_code,
        }
    }

    /// 落账回执；失败时只记错误码（不得对手机谎报失败）。
    fn record(&self, receipt: &ActionReceipt) {
        if let Err(error) = self.actions.record_result(receipt) {
            log::error!("zcode_actions: receipt not persisted: {error:?}");
        }
    }

    /// 构造信封并**恰一次**调用发送端口（无重发、无第二次 sendText、无 CLI 降级）。
    ///
    /// 超时由端口拥有：`ack_timeout` 原样传入，桥按它给 RPC 待答条目设期限并在超窗时
    /// 自行清账；这里**不**外套 `tokio::time::timeout`（取消 future 会绕过桥的
    /// `unregister_rpc`，留下永不回收的 pending waiter）。
    async fn dispatch_send(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        text: String,
    ) -> ActionReceipt {
        let envelope = serde_json::json!({
            "commandId": new_command_id(),
            // clientId 只来自可信调用方上下文（配对门/本机 DB），绝不取手机任意字段。
            "clientId": ctx.device_id,
            "sessionId": target.session_id,
            "type": SEND_TEXT_TYPE,
            "payload": { "text": text },
            "issuedAt": now_ms(),
        });
        let (state, reason_code) = match self
            .bridge
            .call(
                target,
                AGENT_CHANNEL,
                AGENT_SEND_COMMAND_METHOD,
                vec![envelope],
                self.ack_timeout,
            )
            .await
        {
            Err(error) => classify_port_error(&error),
            Ok(ack) => classify_ack(&ack),
        };
        ActionReceipt {
            action_id: ctx.action_id.clone(),
            session_id: target.session_id.clone(),
            generation: target.generation,
            state,
            reason_code,
        }
    }
}

// ============================================================
// 生产端口：BridgeManager
// ============================================================

impl CommandPort for BridgeManager {
    /// 在绑定会话的桥上发一次 RPC。
    ///
    /// 调用方传入 `args = [envelope]`；本适配层按 E3 合同把它包成官方 params
    /// `{workspacePath, workspaceIdentity?, envelope}`（探针 `SendOnceLoop` 同形），
    /// workspace 归属只来自 [`BoundTarget`]（精确 listing 产物），不接受手机自报。
    fn call<'a>(
        &'a self,
        target: &'a BoundTarget,
        channel: &'a str,
        method: &'a str,
        args: Vec<Value>,
        timeout: Duration,
    ) -> BoxFuture<'a, ZResult<Value>> {
        Box::pin(async move {
            let mut args = args.into_iter();
            let envelope = args
                .next()
                .ok_or_else(|| ZcodeError::of(codes::INVALID_ACTION))?;
            if args.next().is_some() {
                // 信封之外的多余参数一律拒绝（HTTP 不得注入任意 wire 字段）。
                return Err(ZcodeError::of(codes::INVALID_ACTION));
            }
            let mut params = Map::new();
            params.insert(
                "workspacePath".to_owned(),
                Value::from(target.workspace_path.clone()),
            );
            if let Some(identity) = &target.workspace_identity {
                params.insert(
                    "workspaceIdentity".to_owned(),
                    Value::from(identity.clone()),
                );
            }
            params.insert("envelope".to_owned(), envelope);
            // ACK 窗原样交给桥的 RPC 待答条目：生产 20s 由桥自己的期限收口并清账。
            BridgeManager::call_with_timeout(
                self,
                target,
                channel,
                method,
                vec![Value::Object(params)],
                timeout,
            )
            .await
        })
    }
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::sync::Semaphore;

    use crate::database::dao::settings;
    use crate::services::zcode_control::types::AnswerEntry;

    const SESSION: &str = "sess_0123456789abcdef0123456789abcdef";
    const GEN: u64 = 7;
    const DEVICE: &str = "phone-1";

    fn target() -> BoundTarget {
        BoundTarget {
            session_id: SESSION.to_string(),
            workspace_path: "/tmp/ws".to_string(),
            workspace_identity: Some("ws-identity".to_string()),
            workspace_key: "ws-identity".to_string(),
            generation: GEN,
        }
    }

    fn ctx(action_id: &str) -> ActionContext {
        ActionContext {
            action_id: action_id.to_string(),
            device_id: DEVICE.to_string(),
            device_name: "iPhone".to_string(),
        }
    }

    fn send_action(text: &str) -> ControlAction {
        ControlAction::Send {
            session_id: SESSION.to_string(),
            generation: GEN,
            text: text.to_string(),
        }
    }

    fn accepted_body() -> Value {
        serde_json::json!({ "status": "accepted" })
    }

    /// 一次出站帧记录（测试观测：channel/method/args 原样）。
    #[derive(Debug, Clone)]
    struct Frame {
        target: BoundTarget,
        channel: String,
        method: String,
        args: Vec<Value>,
    }

    /// 脚本化应答。
    #[derive(Debug, Clone)]
    enum Reply {
        Body(Value),
        Error(&'static str),
        /// 桥自己的 RPC 待答窗到期（写后无应答/ACK 丢失）：按真实桥口径在窗到期时
        /// 返回 `not_ready`（并自行清账），调用方只能靠这个期限收口。
        AckLost,
        /// 在应答窗的 `num/den` 比例后返回成功（模拟 15s/20s 的宿主 accepted）。
        BodyAfter {
            body: Value,
            num: u32,
            den: u32,
        },
        /// 在应答窗的 `num/den` 比例后返回端口错误（模拟桥内期限先到的竞态）。
        ErrorAfter {
            code: &'static str,
            num: u32,
            den: u32,
        },
    }

    /// 在途 future 的取消探针：只有 future 被 **drop**（而非正常返回）才会置位。
    struct CancelProbe<'a> {
        flag: &'a AtomicBool,
        armed: bool,
    }

    impl<'a> CancelProbe<'a> {
        fn new(flag: &'a AtomicBool) -> Self {
            Self { flag, armed: true }
        }

        fn disarm(mut self) {
            self.armed = false;
        }
    }

    impl Drop for CancelProbe<'_> {
        fn drop(&mut self) {
            if self.armed {
                self.flag.store(true, Ordering::SeqCst);
            }
        }
    }

    struct FakeBridge {
        frames: Mutex<Vec<Frame>>,
        reply: Mutex<Reply>,
        wire: AtomicUsize,
        /// 每记录一帧放行一个 permit（测试等待「帧已写出」）。
        recorded: Semaphore,
        /// 门控模式：记录后等 release permit 才应答。
        release: Semaphore,
        gated: bool,
        /// 每次调用收到的应答窗（断言「窗口原样交给端口」）。
        observed_timeouts: Mutex<Vec<Duration>>,
        /// 在途 future 被调用方取消过（生产上等价于桥 pending 泄漏）。
        cancelled: AtomicBool,
    }

    impl FakeBridge {
        fn new(reply: Reply) -> Arc<Self> {
            Arc::new(Self {
                frames: Mutex::new(Vec::new()),
                reply: Mutex::new(reply),
                wire: AtomicUsize::new(0),
                recorded: Semaphore::new(0),
                release: Semaphore::new(0),
                gated: false,
                observed_timeouts: Mutex::new(Vec::new()),
                cancelled: AtomicBool::new(false),
            })
        }

        fn gated() -> Arc<Self> {
            Arc::new(Self {
                frames: Mutex::new(Vec::new()),
                reply: Mutex::new(Reply::Body(accepted_body())),
                wire: AtomicUsize::new(0),
                recorded: Semaphore::new(0),
                release: Semaphore::new(0),
                gated: true,
                observed_timeouts: Mutex::new(Vec::new()),
                cancelled: AtomicBool::new(false),
            })
        }

        fn set_reply(&self, reply: Reply) {
            *self.reply.lock().unwrap() = reply;
        }

        fn wire(&self) -> usize {
            self.wire.load(Ordering::SeqCst)
        }

        fn frames(&self) -> Vec<Frame> {
            self.frames.lock().unwrap().clone()
        }

        fn envelope(&self, index: usize) -> Value {
            self.frames()[index].args[0].clone()
        }

        fn observed_timeouts(&self) -> Vec<Duration> {
            self.observed_timeouts.lock().unwrap().clone()
        }

        fn cancelled(&self) -> bool {
            self.cancelled.load(Ordering::SeqCst)
        }
    }

    impl CommandPort for FakeBridge {
        fn call<'a>(
            &'a self,
            target: &'a BoundTarget,
            channel: &'a str,
            method: &'a str,
            args: Vec<Value>,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<Value>> {
            Box::pin(async move {
                self.observed_timeouts.lock().unwrap().push(timeout);
                {
                    self.frames.lock().unwrap().push(Frame {
                        target: target.clone(),
                        channel: channel.to_owned(),
                        method: method.to_owned(),
                        args,
                    });
                    self.wire.fetch_add(1, Ordering::SeqCst);
                    self.recorded.add_permits(1);
                }
                if self.gated {
                    let _ = self.release.acquire().await;
                }
                let probe = CancelProbe::new(&self.cancelled);
                let reply = self.reply.lock().unwrap().clone();
                let scale = |num: u32, den: u32| timeout.mul_f64(f64::from(num) / f64::from(den));
                let outcome = match reply {
                    Reply::Body(body) => Ok(body),
                    Reply::Error(code) => Err(ZcodeError::of(code)),
                    Reply::AckLost => {
                        tokio::time::sleep(timeout).await;
                        Err(ZcodeError::of(codes::NOT_READY))
                    }
                    Reply::BodyAfter { body, num, den } => {
                        tokio::time::sleep(scale(num, den)).await;
                        Ok(body)
                    }
                    Reply::ErrorAfter { code, num, den } => {
                        tokio::time::sleep(scale(num, den)).await;
                        Err(ZcodeError::of(code))
                    }
                };
                probe.disarm();
                outcome
            })
        }
    }

    fn sender(fake: &Arc<FakeBridge>, store: &DeviceStore) -> CommandSender {
        CommandSender::with_timeout(fake.clone(), store.clone(), Duration::from_millis(20))
    }

    fn row_count(store: &DeviceStore) -> i64 {
        store.with(|c| {
            c.query_row("SELECT COUNT(*) FROM zcode_actions", [], |r| r.get(0))
                .unwrap()
        })
    }

    fn audits(store: &DeviceStore) -> Vec<crate::database::dao::write_audit::AuditRow> {
        store.with(|c| crate::database::dao::write_audit::recent_conn(c, 100))
    }

    /// 同 actionId 同载荷：第二次只返回既有回执，线上恰一帧。
    #[tokio::test]
    async fn same_action_id_same_payload_returns_receipt_without_resend() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        let sender = sender(&fake, &store);
        let (ctx, action) = (ctx("a-1"), send_action("你好\n第二行"));

        let first = sender.execute(&ctx, &target(), &action).await.unwrap();
        let second = sender.execute(&ctx, &target(), &action).await.unwrap();

        let wire_send_text_count = fake.wire();
        assert_eq!(wire_send_text_count, 1);
        assert_eq!(second.action_id, first.action_id);
        assert_eq!(first.state, ActionState::Accepted);
        assert_eq!(second.state, ActionState::Accepted);
        assert_eq!(second.session_id, SESSION);
        assert_eq!(second.generation, GEN);
        assert_eq!(row_count(&store), 1);
    }

    /// 同 actionId 换正文：action_payload_conflict，零第二帧、零新行。
    #[tokio::test]
    async fn action_id_reused_with_different_body_is_conflict() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        let sender = sender(&fake, &store);
        let ctx = ctx("a-1");
        sender
            .execute(&ctx, &target(), &send_action("第一段"))
            .await
            .unwrap();

        let reused_id_error = sender
            .execute(&ctx, &target(), &send_action("第二段"))
            .await
            .unwrap_err();

        assert_eq!(reused_id_error.code, "action_payload_conflict");
        assert_eq!(fake.wire(), 1, "冲突不得产生第二帧");
        assert_eq!(row_count(&store), 1);
    }

    /// 写后 ACK 丢失：保守 unknown，重放同 id 零帧。
    #[tokio::test]
    async fn dropped_ack_after_write_is_unknown_and_never_resent() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::AckLost);
        let sender = sender(&fake, &store);
        let (ctx, action) = (ctx("a-1"), send_action("写后断网"));

        let lost_ack = sender.execute(&ctx, &target(), &action).await.unwrap();
        let before_retry = fake.wire();
        let retry = sender.execute(&ctx, &target(), &action).await.unwrap();

        assert_eq!(lost_ack.state, ActionState::Unknown);
        assert_eq!(retry.state, ActionState::Unknown);
        let retry_wire_count = fake.wire() - before_retry;
        assert_eq!(retry_wire_count, 0);
        assert_eq!(fake.wire(), 1, "全生命周期只允许一帧");
    }

    /// 审计表不可写：预留整体回滚，零命令帧。
    #[tokio::test]
    async fn audit_storage_failure_is_zero_write() {
        let store = DeviceStore::memory();
        store.with(|c| c.execute_batch("DROP TABLE write_audit;").unwrap());
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        let sender = sender(&fake, &store);

        let err = sender
            .execute(&ctx("a-1"), &target(), &send_action("审计失败"))
            .await
            .unwrap_err();

        assert_eq!(err.code, "storage_failed");
        let commands_after_db_failure = fake.wire();
        assert_eq!(commands_after_db_failure, 0);
        assert_eq!(row_count(&store), 0, "审计失败不得留下账本行");
    }

    /// 正文出口：真实换行/中文/emoji 原样保留；签名只在开关打开时追加；`/` 开头不加。
    #[test]
    fn official_text_preserves_newlines_and_optional_device_signature() {
        assert_eq!(
            compose_official_text("phone", "第一行\n第二行", false).unwrap(),
            "第一行\n第二行"
        );
        assert_eq!(
            compose_official_text("phone", "第一行\n第二行", true).unwrap(),
            "第一行\n第二行 [mobile phone]"
        );
        assert_eq!(
            compose_official_text("手机📱", "表情🌏\n中文", true).unwrap(),
            "表情🌏\n中文 [mobile 手机📱]"
        );
        // CRLF 也不归一（终端 normalize 会把换行变字面 \n 两个字符，这里绝不走那条路）
        assert_eq!(
            compose_official_text("phone", "a\r\nb", false).unwrap(),
            "a\r\nb"
        );
        // 行首斜杠：不加签名（斜杠命令的任何附加文本都会使其失效）
        assert_eq!(
            compose_official_text("phone", "/compact", true).unwrap(),
            "/compact"
        );
        // 纯空白：invalid_action（零帧零行）
        assert_eq!(
            compose_official_text("phone", " \n\t ", true)
                .unwrap_err()
                .code,
            "invalid_action"
        );
        // 超预算：payload_too_large（写帧之前拒绝）
        let huge = "a".repeat(MAX_TEXT_BYTES + 1);
        assert_eq!(
            compose_official_text("phone", &huge, false)
                .unwrap_err()
                .code,
            "payload_too_large"
        );
        // 签名花名内换行折叠为空格（不得凭空多出一行）
        assert_eq!(
            compose_official_text("i\nPhone", "hi", true).unwrap(),
            "hi [mobile i Phone]"
        );
    }

    /// 已受理不因无关活动/多余 ACK 字段被升级为 confirmed。
    #[tokio::test]
    async fn accepted_is_not_confirmed_by_unrelated_activity() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Body(serde_json::json!({
            "status": "accepted",
            "activeWorks": [{ "kind": "chat", "foregroundExecutionId": "exec-9" }],
            "sessionActivity": 3,
        })));
        let sender = sender(&fake, &store);
        let (ctx, action) = (ctx("a-1"), send_action("正常一条"));

        let first = sender.execute(&ctx, &target(), &action).await.unwrap();
        let unrelated_activity_receipt = sender.execute(&ctx, &target(), &action).await.unwrap();

        assert_eq!(first.state, ActionState::Accepted);
        assert_eq!(unrelated_activity_receipt.state, ActionState::Accepted);
        assert_ne!(first.state, ActionState::Confirmed);
        assert_eq!(fake.wire(), 1);
    }

    /// 并发同 id：只派发一次，后到者拿既有保守回执。
    #[tokio::test]
    async fn concurrent_same_id_calls_dispatch_once() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::gated();
        let sender = Arc::new(CommandSender::with_timeout(
            fake.clone(),
            store.clone(),
            Duration::from_secs(5),
        ));
        let (ctx, action) = (ctx("a-1"), send_action("并发"));
        let first = tokio::spawn({
            let sender = sender.clone();
            let (ctx, action, target) = (ctx.clone(), action.clone(), target());
            async move { sender.execute(&ctx, &target, &action).await }
        });
        // 等第一帧真正写出（账本行已插入，派发在途）
        let _written = tokio::time::timeout(Duration::from_secs(2), fake.recorded.acquire())
            .await
            .expect("第一帧未写出")
            .expect("semaphore");
        let second = tokio::spawn({
            let sender = sender.clone();
            let (ctx, action, target) = (ctx.clone(), action.clone(), target());
            async move { sender.execute(&ctx, &target, &action).await }
        });
        let second = tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .expect("并发第二次调用不得阻塞")
            .unwrap()
            .unwrap();
        assert_eq!(second.state, ActionState::Unknown, "后到者只得保守回执");
        assert_eq!(fake.wire(), 1, "并发同 id 只允许一次派发");

        fake.release.add_permits(1);
        let first = tokio::time::timeout(Duration::from_secs(2), first)
            .await
            .expect("首次派发超时")
            .unwrap()
            .unwrap();
        assert_eq!(first.state, ActionState::Accepted);
        assert_eq!(fake.wire(), 1);
    }

    /// 重启恢复：新进程实例重放同 id 只返回 unknown，零帧。
    #[tokio::test]
    async fn restart_recovery_returns_unknown_without_send() {
        let store = DeviceStore::memory();
        let before = FakeBridge::new(Reply::AckLost);
        let before_sender = sender(&before, &store);
        let (ctx, action) = (ctx("a-1"), send_action("重启前"));
        let crashed = before_sender
            .execute(&ctx, &target(), &action)
            .await
            .unwrap();
        assert_eq!(crashed.state, ActionState::Unknown);
        assert_eq!(before.wire(), 1);

        // 重启后的进程视图：同一持久库 + 新桥
        let after = FakeBridge::new(Reply::Body(accepted_body()));
        let after_sender = sender(&after, &store);
        let recovered = after_sender
            .execute(&ctx, &target(), &action)
            .await
            .unwrap();

        assert_eq!(recovered.state, ActionState::Unknown);
        assert_eq!(after.wire(), 0, "重启后绝不重发");
        assert_eq!(row_count(&store), 1);
    }

    /// 跨设备同 id：冲突且零帧，不泄漏旧回执。
    #[tokio::test]
    async fn cross_device_same_id_is_conflict_without_wire() {
        let store = DeviceStore::memory();
        let owner = FakeBridge::new(Reply::Body(accepted_body()));
        sender(&owner, &store)
            .execute(&ctx("a-1"), &target(), &send_action("主人的消息"))
            .await
            .unwrap();
        assert_eq!(owner.wire(), 1);

        let intruder = FakeBridge::new(Reply::Body(accepted_body()));
        let intruder_ctx = ActionContext {
            action_id: "a-1".to_string(),
            device_id: "phone-2".to_string(),
            device_name: "Pixel".to_string(),
        };
        let err = sender(&intruder, &store)
            .execute(&intruder_ctx, &target(), &send_action("主人的消息"))
            .await
            .unwrap_err();
        assert_eq!(err.code, "action_payload_conflict");
        assert_eq!(intruder.wire(), 0, "跨设备同 id 不得写出");
    }

    /// 纯空白/超预算：写帧之前拒绝，零帧零行。
    #[tokio::test]
    async fn whitespace_only_and_over_budget_send_zero_wire() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        let sender = sender(&fake, &store);

        let blank = sender
            .execute(&ctx("a-1"), &target(), &send_action("   \n\t  "))
            .await
            .unwrap_err();
        assert_eq!(blank.code, "invalid_action");
        let huge = "a".repeat(MAX_TEXT_BYTES + 1);
        let too_large = sender
            .execute(&ctx("a-2"), &target(), &send_action(&huge))
            .await
            .unwrap_err();
        assert_eq!(too_large.code, "payload_too_large");
        assert_eq!(fake.wire(), 0);
        assert_eq!(row_count(&store), 0);
    }

    /// 签名开关读取单点：设置 on 才追加 ` [mobile <花名>]`，off 裸发；`/` 开头不加。
    #[tokio::test]
    async fn signature_setting_controls_official_suffix() {
        let store = DeviceStore::memory();
        store.with(|c| settings::set_setting_conn(c, "remote_message_signature", "on"));
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        let sender = sender(&fake, &store);

        sender
            .execute(&ctx("a-1"), &target(), &send_action("带签名"))
            .await
            .unwrap();
        sender
            .execute(&ctx("a-2"), &target(), &send_action("/compact"))
            .await
            .unwrap();
        assert_eq!(
            fake.envelope(0)["payload"]["text"],
            serde_json::json!("带签名 [mobile iPhone]")
        );
        assert_eq!(
            fake.envelope(1)["payload"]["text"],
            serde_json::json!("/compact")
        );

        // 关闭后裸发（缺省 off 语义由 message_signature_enabled_conn 保证）
        store.with(|c| settings::set_setting_conn(c, "remote_message_signature", "off"));
        sender
            .execute(&ctx("a-3"), &target(), &send_action("裸发"))
            .await
            .unwrap();
        assert_eq!(
            fake.envelope(2)["payload"]["text"],
            serde_json::json!("裸发")
        );
    }

    /// 行首斜杠保持原样且内容逐字不变（不在本批承诺新增 slash 能力）。
    #[tokio::test]
    async fn leading_slash_message_keeps_no_signature() {
        let store = DeviceStore::memory();
        store.with(|c| settings::set_setting_conn(c, "remote_message_signature", "on"));
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        sender(&fake, &store)
            .execute(&ctx("a-1"), &target(), &send_action("/permissions\nnext"))
            .await
            .unwrap();
        assert_eq!(
            fake.envelope(0)["payload"]["text"],
            serde_json::json!("/permissions\nnext")
        );
    }

    /// ACK 显式拒绝/noop 如实分类，且不回传宿主原因原文。
    #[tokio::test]
    async fn ack_rejected_and_noop_are_distinct_states() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Body(serde_json::json!({
            "status": "rejected",
            "reasonCode": "host-secret-reason",
        })));
        let sender = sender(&fake, &store);

        let rejected = sender
            .execute(&ctx("a-1"), &target(), &send_action("被拒"))
            .await
            .unwrap();
        assert_eq!(rejected.state, ActionState::Rejected);
        assert!(rejected.reason_code.is_none(), "宿主原因原文不得下发/落账");

        fake.set_reply(Reply::Body(serde_json::json!({ "status": "noop" })));
        let noop = sender
            .execute(&ctx("a-2"), &target(), &send_action("无需处理"))
            .await
            .unwrap();
        assert_eq!(noop.state, ActionState::Noop);
        assert_ne!(noop.state, ActionState::Accepted, "noop 不是成功");
        assert_eq!(fake.wire(), 2);
    }

    /// 写后失败/断网按 unknown（绝不 send_failed）；写前绑定失败才是 send_failed。
    #[tokio::test]
    async fn write_error_maps_to_unknown_but_bind_error_is_send_failed() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Error(codes::UNKNOWN));
        let sender = sender(&fake, &store);

        for (index, code) in [codes::UNKNOWN, codes::OFFLINE, codes::NOT_READY]
            .into_iter()
            .enumerate()
        {
            fake.set_reply(Reply::Error(code));
            let receipt = sender
                .execute(
                    &ctx(&format!("a-{index}")),
                    &target(),
                    &send_action("可能已写出"),
                )
                .await
                .unwrap();
            assert_eq!(
                receipt.state,
                ActionState::Unknown,
                "{code} 不得判成确证未写出"
            );
            assert_eq!(receipt.reason_code.as_deref(), Some(code));
        }

        // 桥保证零写的绑定代际/会话失败 → send_failed（用户可换 actionId 重试）
        fake.set_reply(Reply::Error(codes::STALE_GENERATION));
        let failed = sender
            .execute(&ctx("a-stale"), &target(), &send_action("零写"))
            .await
            .unwrap();
        assert_eq!(failed.state, ActionState::SendFailed);
        assert_eq!(failed.reason_code.as_deref(), Some(codes::STALE_GENERATION));
    }

    /// 未接入变体：invalid_action 且零帧零行；Answer/Snooze 已接入命令层，但未装配
    /// pending 端口时一律 not_ready（零帧零行，生产接线在 T7.1）。
    #[tokio::test]
    async fn non_send_actions_are_invalid_and_zero_wire() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        let sender = sender(&fake, &store);
        let unsupported = [
            ControlAction::Permission {
                session_id: SESSION.to_string(),
                generation: GEN,
                interaction_id: "ix-1".to_string(),
                option_id: "allowOnce".to_string(),
            },
            ControlAction::Stop {
                session_id: SESSION.to_string(),
                generation: GEN,
                expected_foreground_execution_id: "exec-1".to_string(),
            },
            ControlAction::Selection {
                session_id: SESSION.to_string(),
                generation: GEN,
                model: "model-a".to_string(),
                thought: "high".to_string(),
            },
        ];
        for (index, action) in unsupported.iter().enumerate() {
            let err = sender
                .execute(&ctx(&format!("a-{index}")), &target(), action)
                .await
                .unwrap_err();
            assert_eq!(
                err.code,
                "invalid_action",
                "{} 未接入必须拒绝",
                action.kind()
            );
        }
        let needs_port = [
            ControlAction::Snooze {
                session_id: SESSION.to_string(),
                generation: GEN,
                interaction_id: "ix-1".to_string(),
            },
            ControlAction::Answer {
                session_id: SESSION.to_string(),
                generation: GEN,
                interaction_id: "ix-1".to_string(),
                answers: vec![AnswerEntry {
                    question_text: "题".to_string(),
                    selected_values: vec!["A".to_string()],
                    free_text: None,
                }],
            },
        ];
        for (index, action) in needs_port.iter().enumerate() {
            let err = sender
                .execute(&ctx(&format!("a-p{index}")), &target(), action)
                .await
                .unwrap_err();
            assert_eq!(
                err.code,
                "not_ready",
                "{} 未装配 pending 端口必须拒绝",
                action.kind()
            );
        }
        assert_eq!(fake.wire(), 0);
        assert_eq!(row_count(&store), 0);
    }

    /// 动作自报的会话/代际与绑定目标不一致：零帧零行。
    #[tokio::test]
    async fn mismatched_target_or_generation_is_zero_wire() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        let sender = sender(&fake, &store);

        let stale = ControlAction::Send {
            session_id: SESSION.to_string(),
            generation: GEN + 1,
            text: "旧代际".to_string(),
        };
        assert_eq!(
            sender
                .execute(&ctx("a-1"), &target(), &stale)
                .await
                .unwrap_err()
                .code,
            "stale_generation"
        );

        let other_session = ControlAction::Send {
            session_id: "sess_other".to_string(),
            generation: GEN,
            text: "别的会话".to_string(),
        };
        assert_eq!(
            sender
                .execute(&ctx("a-2"), &target(), &other_session)
                .await
                .unwrap_err()
                .code,
            "invalid_action"
        );
        assert_eq!(fake.wire(), 0);
        assert_eq!(row_count(&store), 0);
    }

    /// 信封逐字段对齐 E3 证据：channel/method/字段面/可信 clientId/epoch ms。
    #[tokio::test]
    async fn envelope_matches_probe_wire_shape() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        sender(&fake, &store)
            .execute(&ctx("a-1"), &target(), &send_action("信封检查"))
            .await
            .unwrap();

        let frames = fake.frames();
        assert_eq!(frames.len(), 1);
        let frame = &frames[0];
        assert_eq!(frame.channel, "zcode-agent");
        assert_eq!(frame.method, "sendConversationCommandV4");
        assert_eq!(frame.target.session_id, SESSION);
        assert_eq!(frame.args.len(), 1, "args 恰一个 envelope");

        let envelope = frame.args[0].as_object().expect("envelope 必须是对象");
        let mut keys: Vec<&str> = envelope.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "clientId",
                "commandId",
                "issuedAt",
                "payload",
                "sessionId",
                "type"
            ]
        );
        assert_eq!(envelope["clientId"], serde_json::json!(DEVICE));
        assert_eq!(envelope["sessionId"], serde_json::json!(SESSION));
        assert_eq!(envelope["type"], serde_json::json!("sendText"));
        let payload = envelope["payload"].as_object().expect("payload");
        assert_eq!(payload.len(), 1, "sendText 只带 text（无 mode/model/CAS）");
        assert_eq!(payload["text"], serde_json::json!("信封检查"));
        let issued_at = envelope["issuedAt"]
            .as_i64()
            .expect("issuedAt 必须是整数 epoch ms");
        assert!(issued_at >= 1_700_000_000_000, "issuedAt 必须是毫秒");

        let command_id = envelope["commandId"].as_str().expect("commandId");
        assert_eq!(command_id.len(), 36, "commandId 必须是 UUID");
        assert_eq!(command_id.as_bytes()[14], b'4', "UUIDv4 版本位");
        assert!(
            matches!(command_id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
            "UUIDv4 变体位"
        );
        assert_ne!(new_command_id(), new_command_id(), "commandId 必须随机");
    }

    /// ACK 窗必须是 20s，而且必须**原样交给端口**（桥的 RPC 待答条目按它收口）：
    /// 15s 的宿主 accepted 仍在窗内 → Accepted；21s 才到的应答已超窗 → Unknown。
    #[tokio::test]
    async fn send_ack_window_is_twenty_seconds() {
        // 生产窗：20s（brief 写死；桥内 rpc 窗不再是有效上限）。
        assert_eq!(ACK_TIMEOUT, Duration::from_secs(20));

        let store = DeviceStore::memory();
        let production = FakeBridge::new(Reply::AckLost);
        let sender = CommandSender::new(production.clone(), store.clone());
        assert_eq!(sender.ack_timeout, ACK_TIMEOUT, "生产构造必须用 20s ACK 窗");

        // 用注入窗把 20s 时序压到毫秒：fake 按「自身窗的 15/20、21/20」应答，
        // 比例语义与真实的 15s/21s 完全一致。
        let fast = FakeBridge::new(Reply::BodyAfter {
            body: accepted_body(),
            num: 15,
            den: 20,
        });
        let sender =
            CommandSender::with_timeout(fast.clone(), store.clone(), Duration::from_millis(40));
        let within = sender
            .execute(&ctx("a-15"), &target(), &send_action("十五秒的 ACK"))
            .await
            .unwrap();
        assert_eq!(
            within.state,
            ActionState::Accepted,
            "窗内（15/20）的宿主 accepted 必须判已受理"
        );
        assert_eq!(
            fast.observed_timeouts(),
            vec![Duration::from_millis(40)],
            "ACK 窗必须原样交给端口（不得被桥内 rpc 窗抢先收口）"
        );

        let late = FakeBridge::new(Reply::ErrorAfter {
            code: codes::NOT_READY,
            num: 21,
            den: 20,
        });
        let sender = CommandSender::with_timeout(late.clone(), store, Duration::from_millis(40));
        let beyond = sender
            .execute(&ctx("a-21"), &target(), &send_action("二十一秒的 ACK"))
            .await
            .unwrap();
        assert_eq!(
            beyond.state,
            ActionState::Unknown,
            "超出 ACK 窗的应答（桥先收口）只能是保守 unknown"
        );
        assert_eq!(late.wire(), 1, "超窗也不得重发");
    }

    /// 超窗收口由**端口自己的期限**完成：调用方绝不取消在途 future。
    ///
    /// 取消（外层 `tokio::time::timeout` 早于端口期限触发）会绕过桥的
    /// `unregister_rpc` 清理，在 pending 里留下永不回收的 waiter；本用例用
    /// drop 探针锁死「future 正常返回而非被 drop」。
    #[tokio::test]
    async fn timeout_cleans_bridge_pending() {
        let store = DeviceStore::memory();
        // 端口在自身窗的 2 倍处才收口：若调用方还外套 20ms 超时，future 会在
        // 半途被 drop（cancelled 置位），正好复现 pending 泄漏形态。
        let fake = FakeBridge::new(Reply::ErrorAfter {
            code: codes::NOT_READY,
            num: 2,
            den: 1,
        });
        let sender = CommandSender::with_timeout(fake.clone(), store, Duration::from_millis(20));

        let receipt = sender
            .execute(&ctx("a-1"), &target(), &send_action("等端口收口"))
            .await
            .unwrap();

        assert_eq!(receipt.state, ActionState::Unknown);
        assert!(
            !fake.cancelled(),
            "调用方不得取消端口 future：桥必须自行 unregister pending 条目"
        );
        assert_eq!(fake.wire(), 1);
    }

    /// 起始审计与终态审计都带「类别 + 字数」，但绝不摘正文。
    #[tokio::test]
    async fn audit_start_and_terminal_include_count_without_body() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        let sender = sender(&fake, &store);
        sender
            .execute(&ctx("a-1"), &target(), &send_action("你好世界"))
            .await
            .unwrap();

        let rows = audits(&store);
        assert_eq!(rows.len(), 2, "起始 + 终态各一条审计");
        // recent_conn 最新在前：终态 accepted，随后是 started。
        assert_eq!(rows[0].result, "accepted");
        assert_eq!(rows[1].result, "started");
        for row in &rows {
            assert_eq!(row.channel, "zcode_official");
            assert_eq!(row.agent_type, "zcode");
            assert_eq!(row.action, "zcode_send");
            assert_eq!(row.summary, "send chars=4", "摘要只记类别与字数");
            assert!(!row.summary.contains("你好"), "摘要绝不摘正文");
        }
    }

    /// 预算按物理帧最坏膨胀推导（JSON `\u00XX` 6× × Base64 4/3）：
    /// 预算内的控制字符密集正文必须真的能通过编码，否则预检形同虚设。
    #[test]
    fn byte_budget_survives_worst_case_base64_encoding() {
        let boundary = "\u{1}".repeat(MAX_TEXT_BYTES);
        let composed =
            compose_official_text("phone", &boundary, false).expect("预算内必须可以组合");
        let message = serde_json::to_vec(&serde_json::json!({
            "type": SEND_TEXT_TYPE,
            "payload": { "text": composed },
        }))
        .expect("message json");
        super::super::codec::encode_frame(
            &super::super::codec::RpcIdentity::new("bridge-1"),
            1,
            1,
            &message,
        )
        .expect("预算内的最坏转义正文必须可以通过物理帧编码（1 MiB 含 Base64）");
    }

    // ============================================================
    // T4.1 Slice B2：问答应答 / snooze / 宿主终态（tests::answer）
    // ============================================================

    mod answer {
        use super::*;
        use std::collections::BTreeSet;
        use std::time::Instant;

        use serde_json::json;

        use crate::services::zcode_control::types::{
            PendingKind, PendingView, QuestionOption, QuestionView,
        };

        /// 宿主 pending 侧的测试替身（与出站 `FakeBridge` 分开：授权/snooze/终态
        /// 是独立端口；生产两者都是 `BridgeManager`）。
        struct FakePending {
            inner: Mutex<PendingState>,
        }

        struct PendingState {
            target: BoundTarget,
            card: PendingView,
            /// 本地仍在册（authorize 可过）：与宿主集合独立。
            local_present: bool,
            /// 宿主最近一帧的卡集合。
            host_ids: BTreeSet<String>,
            host_revision: u64,
            inflight: BTreeSet<String>,
            consumed: BTreeSet<String>,
            snooze_result: Result<(), &'static str>,
            snooze_calls: usize,
            /// 每次 snooze 收到的 clientId（断言与 resolve/send 同源）。
            snooze_clients: Vec<String>,
            /// `wait_host_cleared` 首次轮询时模拟宿主撤卡（推进代数 + 移出集合）。
            clear_on_wait: bool,
        }

        impl FakePending {
            fn new(target: BoundTarget, card: PendingView) -> Arc<Self> {
                let interaction_id = card.interaction_id.clone();
                Arc::new(Self {
                    inner: Mutex::new(PendingState {
                        target,
                        card,
                        local_present: true,
                        host_ids: [interaction_id].into_iter().collect(),
                        host_revision: 1,
                        inflight: BTreeSet::new(),
                        consumed: BTreeSet::new(),
                        snooze_result: Ok(()),
                        snooze_calls: 0,
                        snooze_clients: Vec::new(),
                        clear_on_wait: false,
                    }),
                })
            }

            fn set_snooze_error(&self, code: &'static str) {
                self.inner.lock().unwrap().snooze_result = Err(code);
            }

            fn set_snooze_ok(&self) {
                self.inner.lock().unwrap().snooze_result = Ok(());
            }

            fn clear_host_card(&self) {
                let mut state = self.inner.lock().unwrap();
                let interaction_id = state.card.interaction_id.clone();
                state.host_ids.remove(&interaction_id);
                state.host_revision += 1;
            }

            /// 宿主帧整体替换为「不含该卡」（镜像 `PendingBook::replace_pendings`：
            /// 撤卡帧同时释放写后未知的在途闸，本地展示同步撤卡）。
            fn host_frame_without_card(&self) {
                let mut state = self.inner.lock().unwrap();
                let interaction_id = state.card.interaction_id.clone();
                state.host_ids.remove(&interaction_id);
                state.host_revision += 1;
                state.inflight.remove(&interaction_id);
                state.local_present = false;
            }

            /// 宿主后续帧重新列出该 id（同一卡回到展示面）。
            fn host_frame_relists_card(&self) {
                let mut state = self.inner.lock().unwrap();
                let interaction_id = state.card.interaction_id.clone();
                state.host_ids.insert(interaction_id);
                state.host_revision += 1;
                state.local_present = true;
            }

            fn clear_on_wait(&self) {
                self.inner.lock().unwrap().clear_on_wait = true;
            }

            fn snooze_calls(&self) -> usize {
                self.inner.lock().unwrap().snooze_calls
            }

            fn snooze_clients(&self) -> Vec<String> {
                self.inner.lock().unwrap().snooze_clients.clone()
            }

            fn inflight(&self, interaction_id: &str) -> bool {
                self.inner.lock().unwrap().inflight.contains(interaction_id)
            }

            fn consumed(&self, interaction_id: &str) -> bool {
                self.inner.lock().unwrap().consumed.contains(interaction_id)
            }
        }

        impl PendingPort for FakePending {
            fn authorize_pending<'a>(
                &'a self,
                target: &'a BoundTarget,
                interaction_id: &'a str,
            ) -> BoxFuture<'a, ZResult<PendingView>> {
                Box::pin(async move {
                    let mut state = self.inner.lock().unwrap();
                    if *target != state.target {
                        return Err(ZcodeError::of(codes::STALE_PENDING));
                    }
                    if state.consumed.contains(interaction_id)
                        || state.inflight.contains(interaction_id)
                    {
                        return Err(ZcodeError::of(codes::STALE_PENDING));
                    }
                    if !state.local_present || state.card.interaction_id != interaction_id {
                        return Err(ZcodeError::of(codes::STALE_PENDING));
                    }
                    state.inflight.insert(interaction_id.to_owned());
                    Ok(state.card.clone())
                })
            }

            fn snooze<'a>(
                &'a self,
                target: &'a BoundTarget,
                _interaction_id: &'a str,
                client_id: &'a str,
                _timeout: Duration,
            ) -> BoxFuture<'a, ZResult<()>> {
                Box::pin(async move {
                    let mut state = self.inner.lock().unwrap();
                    if *target != state.target {
                        return Err(ZcodeError::of(codes::STALE_PENDING));
                    }
                    state.snooze_calls += 1;
                    state.snooze_clients.push(client_id.to_owned());
                    state.snooze_result.map_err(ZcodeError::of)
                })
            }

            fn host_pending_revision(&self, target: &BoundTarget) -> u64 {
                let state = self.inner.lock().unwrap();
                if *target != state.target {
                    return 0;
                }
                state.host_revision
            }

            fn host_has_pending(&self, target: &BoundTarget, interaction_id: &str) -> bool {
                let state = self.inner.lock().unwrap();
                *target == state.target && state.host_ids.contains(interaction_id)
            }

            fn wait_host_cleared<'a>(
                &'a self,
                target: &'a BoundTarget,
                interaction_id: &'a str,
                baseline_revision: u64,
                timeout: Duration,
            ) -> BoxFuture<'a, bool> {
                Box::pin(async move {
                    let deadline = Instant::now()
                        .checked_add(timeout)
                        .unwrap_or_else(Instant::now);
                    loop {
                        {
                            let mut state = self.inner.lock().unwrap();
                            if *target != state.target {
                                return false;
                            }
                            if state.clear_on_wait && state.host_revision == baseline_revision {
                                state.clear_on_wait = false;
                                state.host_ids.remove(interaction_id);
                                state.host_revision += 1;
                            }
                            if state.host_revision > baseline_revision
                                && !state.host_ids.contains(interaction_id)
                            {
                                return true;
                            }
                        }
                        let now = Instant::now();
                        if now >= deadline {
                            return false;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
            }

            fn mark_resolved_local(&self, target: &BoundTarget, interaction_id: &str) {
                let mut state = self.inner.lock().unwrap();
                if *target != state.target {
                    return;
                }
                state.inflight.remove(interaction_id);
                state.consumed.insert(interaction_id.to_owned());
                state.local_present = false;
            }

            fn clear_local_inflight(&self, target: &BoundTarget, interaction_id: &str) {
                let mut state = self.inner.lock().unwrap();
                if *target != state.target {
                    return;
                }
                state.inflight.remove(interaction_id);
            }
        }

        fn question(
            question_text: &str,
            values: &[&str],
            multi_select: bool,
            free_text_allowed: bool,
        ) -> QuestionView {
            QuestionView {
                header: "H".to_owned(),
                question_text: question_text.to_owned(),
                options: values
                    .iter()
                    .map(|value| QuestionOption {
                        value: (*value).to_owned(),
                        label: (*value).to_owned(),
                        description: None,
                        preview: None,
                    })
                    .collect(),
                multi_select,
                free_text_allowed,
            }
        }

        fn card(
            interaction_id: &str,
            auto_resolution: bool,
            questions: Vec<QuestionView>,
        ) -> PendingView {
            PendingView {
                interaction_id: interaction_id.to_owned(),
                kind: PendingKind::UserInput,
                anchor_row_id: Some(7),
                tool_call_id: Some("call_synthetic_0002".to_owned()),
                created_at: Some(1_700_000_000_000),
                summary: Some("synthetic: pick answers".to_owned()),
                tool_name: Some("AskUserQuestion".to_owned()),
                schema_interaction: None,
                sensitive: false,
                auto_resolution,
                auto_resolution_state: auto_resolution.then(|| "visibleCountdown".to_owned()),
                questions,
                options: Vec::new(),
            }
        }

        fn single_card(interaction_id: &str) -> PendingView {
            card(
                interaction_id,
                false,
                vec![question("原题？", &["A", "B"], false, false)],
            )
        }

        fn fake_pending(card: PendingView) -> Arc<FakePending> {
            FakePending::new(target(), card)
        }

        fn answer_entry(question_text: &str, values: &[&str]) -> AnswerEntry {
            AnswerEntry {
                question_text: question_text.to_owned(),
                selected_values: values.iter().map(|value| (*value).to_owned()).collect(),
                free_text: None,
            }
        }

        fn free_text_entry(question_text: &str, text: &str) -> AnswerEntry {
            AnswerEntry {
                question_text: question_text.to_owned(),
                selected_values: Vec::new(),
                free_text: Some(text.to_owned()),
            }
        }

        fn answer_action(interaction_id: &str, answers: Vec<AnswerEntry>) -> ControlAction {
            answer_action_for(SESSION, GEN, interaction_id, answers)
        }

        fn answer_action_for(
            session_id: &str,
            generation: u64,
            interaction_id: &str,
            answers: Vec<AnswerEntry>,
        ) -> ControlAction {
            ControlAction::Answer {
                session_id: session_id.to_owned(),
                generation,
                interaction_id: interaction_id.to_owned(),
                answers,
            }
        }

        fn answer_sender(
            fake: &Arc<FakeBridge>,
            pending: &Arc<FakePending>,
            store: &DeviceStore,
        ) -> CommandSender {
            CommandSender::with_timeout(fake.clone(), store.clone(), Duration::from_millis(20))
                .with_pending(pending.clone())
                .with_confirm_timeout(Duration::from_millis(40))
        }

        fn resolve_frames(fake: &FakeBridge) -> Vec<Frame> {
            fake.frames()
                .into_iter()
                .filter(|frame| {
                    frame
                        .args
                        .first()
                        .map(|envelope| envelope["type"] == json!("resolveInteraction"))
                        .unwrap_or(false)
                })
                .collect()
        }

        fn stored_state(store: &DeviceStore, action_id: &str) -> Option<String> {
            store.with(|conn| {
                conn.query_row(
                    "SELECT state FROM zcode_actions WHERE action_id = ?1",
                    [action_id],
                    |row| row.get(0),
                )
                .ok()
            })
        }

        /// 账本 + 审计的全列 dump（断言敏感正文从不落库）。
        fn db_dump(store: &DeviceStore) -> String {
            store.with(|conn| {
                let mut dump = String::new();
                for table in ["zcode_actions", "write_audit"] {
                    let mut statement = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
                    let columns = statement.column_count();
                    let mut rows = statement.query([]).unwrap();
                    while let Some(row) = rows.next().unwrap() {
                        for index in 0..columns {
                            let value: rusqlite::types::Value = row.get(index).unwrap();
                            dump.push_str(&format!("{value:?}|"));
                        }
                    }
                }
                dump
            })
        }

        /// 已消费/不在册的卡：零帧，绝不复用另一张卡。
        #[tokio::test]
        async fn stale_or_consumed_pending_never_sends() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);
            let action = answer_action("ix-1", vec![answer_entry("原题？", &["A"])]);

            let first = sender
                .execute(&ctx("a-1"), &target(), &action)
                .await
                .unwrap();
            assert_eq!(first.state, ActionState::Accepted);
            assert_eq!(resolve_frames(&fake).len(), 1);

            // 同一张卡第二次提交（新 actionId）：本地已消费 → stale_pending，零新帧。
            let err = sender
                .execute(&ctx("a-2"), &target(), &action)
                .await
                .unwrap_err();
            assert_eq!(err.code, "stale_pending");
            assert_eq!(resolve_frames(&fake).len(), 1, "已消费卡不得再次 resolve");

            // 不存在的卡：同样零帧（不得顺手消费别的卡）。
            let missing = answer_action("ix-missing", vec![answer_entry("原题？", &["A"])]);
            let err = sender
                .execute(&ctx("a-3"), &target(), &missing)
                .await
                .unwrap_err();
            assert_eq!(err.code, "stale_pending");
            assert_eq!(resolve_frames(&fake).len(), 1);
        }

        /// 本地消费 ≠ 宿主撤卡：ACK-only 时宿主集合仍列着卡。
        #[tokio::test]
        async fn local_consume_is_not_host_cleared() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);
            let receipt = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();

            assert_eq!(receipt.state, ActionState::Accepted);
            assert!(pending.consumed("ix-1"), "ACK 受理后本地必须消费");
            assert!(
                pending.host_has_pending(&target(), "ix-1"),
                "本地消费不得改动宿主集合"
            );
            // 线上恰一帧 resolve（不依赖测试替身自己的轮询循环）。
            assert_eq!(resolve_frames(&fake).len(), 1);
            assert_eq!(stored_state(&store, "a-1").as_deref(), Some("accepted"));
        }

        /// 敏感卡：答案只上 wire，账本只有指纹，审计只有计数。
        #[tokio::test]
        async fn sensitive_question_never_persists_draft() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let mut sensitive = card(
                "ix-secret",
                false,
                vec![question("口令？", &[], false, true)],
            );
            sensitive.sensitive = true;
            let pending = fake_pending(sensitive);
            let sender = answer_sender(&fake, &pending, &store);
            let receipt = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action(
                        "ix-secret",
                        vec![free_text_entry("口令？", "机密-草稿-ABC")],
                    ),
                )
                .await
                .unwrap();
            assert_eq!(receipt.state, ActionState::Accepted);

            // wire 上必须有答案（送达宿主），本地库不得有任何正文/题干痕迹。
            let envelope = fake.envelope(0);
            assert_eq!(
                envelope["payload"]["answer"]["content"]["answers"]["口令？"],
                json!("机密-草稿-ABC")
            );
            let dump = db_dump(&store);
            for forbidden in ["机密", "草稿", "ABC", "口令？"] {
                assert!(
                    !dump.contains(forbidden),
                    "敏感正文/题干不得落库: {forbidden} in {dump}"
                );
            }
            let hash: String = store.with(|conn| {
                conn.query_row(
                    "SELECT payload_hash FROM zcode_actions WHERE action_id = 'a-1'",
                    [],
                    |row| row.get(0),
                )
                .unwrap()
            });
            assert_eq!(hash.len(), 64, "账本只存指纹");
            assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
            let summary: String = store.with(|conn| {
                conn.query_row(
                    "SELECT summary FROM zcode_actions WHERE action_id = 'a-1'",
                    [],
                    |row| row.get(0),
                )
                .unwrap()
            });
            assert_eq!(summary, "answer answers=1 selected=0");
            assert!(
                audits(&store)
                    .iter()
                    .all(|row| row.summary == "answer answers=1 selected=0"),
                "审计摘要只含类别与计数"
            );
        }

        /// 计划审批（plan_approval）不是问答：看见 Approve 选项也不得代批。
        #[tokio::test]
        async fn plan_approval_is_not_a_question_auto_approval() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let mut plan = card(
                "ix-plan",
                true,
                vec![question(
                    "Approve plan?",
                    &["Approve", "Reject"],
                    false,
                    false,
                )],
            );
            plan.schema_interaction = Some("plan_approval".to_owned());
            let pending = fake_pending(plan);
            let sender = answer_sender(&fake, &pending, &store);

            let err = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-plan", vec![answer_entry("Approve plan?", &["Approve"])]),
                )
                .await
                .unwrap_err();
            assert_eq!(err.code, "needs_evidence", "计划审批绝不能按问答代批");
            assert_eq!(fake.wire(), 0, "未支持 schema 必须零 resolve 帧");
            assert_eq!(pending.snooze_calls(), 0, "未支持 schema 也不得先 snooze");
            assert!(!pending.consumed("ix-plan"), "零 value 不得消费卡");
            assert_eq!(stored_state(&store, "a-1"), None, "本地拒绝不得留下账本行");
            assert!(audits(&store).is_empty(), "本地拒绝不得写 started 审计");
        }

        /// snooze 未证实（失败或卡已不在宿主集合）：零 resolve。
        #[tokio::test]
        async fn snooze_unconfirmed_zero_resolve() {
            let store = DeviceStore::memory();

            // ① snooze 失败（超窗/unknown）：零 resolve，回执保守 unknown。
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(card(
                "ix-1",
                true,
                vec![question("原题？", &["A"], false, false)],
            ));
            pending.set_snooze_error(codes::UNKNOWN);
            let sender = answer_sender(&fake, &pending, &store);
            let receipt = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();
            assert_eq!(receipt.state, ActionState::Unknown);
            assert_eq!(pending.snooze_calls(), 1, "snooze 恰一帧");
            assert_eq!(resolve_frames(&fake).len(), 0, "snooze 未证实不得 resolve");

            // ② snooze 受理但卡不在宿主集合：仍零 resolve。
            let fake2 = FakeBridge::new(Reply::Body(accepted_body()));
            let pending2 = fake_pending(card(
                "ix-2",
                true,
                vec![question("原题？", &["A"], false, false)],
            ));
            pending2.clear_host_card();
            let sender2 = answer_sender(&fake2, &pending2, &store);
            let receipt2 = sender2
                .execute(
                    &ctx("a-2"),
                    &target(),
                    &answer_action("ix-2", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();
            assert_eq!(receipt2.state, ActionState::Unknown);
            assert_eq!(pending2.snooze_calls(), 1);
            assert_eq!(resolve_frames(&fake2).len(), 0);
        }

        /// snooze 受理且卡仍在本帧宿主集合：恰一帧 resolve，形状对齐探针。
        #[tokio::test]
        async fn snooze_accepted_then_resolve_single_frame() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(card(
                "ix-1",
                true,
                vec![question("原题？", &["A", "B"], false, false)],
            ));
            let sender = answer_sender(&fake, &pending, &store);
            let receipt = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();

            assert_eq!(receipt.state, ActionState::Accepted);
            assert_eq!(
                pending.snooze_calls(),
                1,
                "autoResolution 在场恰一次 snooze"
            );
            let frames = resolve_frames(&fake);
            assert_eq!(frames.len(), 1, "恰一帧 resolveInteraction");
            let frame = &frames[0];
            assert_eq!(frame.channel, "zcode-agent");
            assert_eq!(frame.method, "sendConversationCommandV4");
            let envelope = &frame.args[0];
            assert_eq!(envelope["type"], json!("resolveInteraction"));
            assert_eq!(envelope["payload"]["interactionId"], json!("ix-1"));
            assert_eq!(
                envelope["payload"]["answer"],
                json!({
                    "action": "accept",
                    "content": {
                        "answers": { "原题？": "A" },
                        "answer_0": "A",
                        "answer": "A"
                    }
                }),
                "resolve 载荷必须是宿主原值形状"
            );
            assert_eq!(envelope["clientId"], json!(DEVICE));
            assert_eq!(envelope["sessionId"], json!(SESSION));
            assert!(envelope["issuedAt"].as_i64().unwrap() >= 1_700_000_000_000);
            assert_eq!(
                fake.observed_timeouts(),
                vec![Duration::from_millis(20)],
                "ACK 窗原样交给端口"
            );
        }

        /// ACK 只算 accepted；宿主撤卡（代数前进 + id 消失）才升级 confirmed。
        #[tokio::test]
        async fn ack_only_is_accepted_host_clear_is_confirmed() {
            let store = DeviceStore::memory();

            // ① ACK 受理但宿主没撤卡：只报 accepted（绝不伪造 confirmed）。
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);
            let ack_only = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();
            assert_eq!(ack_only.state, ActionState::Accepted);
            assert!(pending.host_has_pending(&target(), "ix-1"));
            assert_eq!(stored_state(&store, "a-1").as_deref(), Some("accepted"));

            // ② ACK 后宿主撤卡（代数前进 + id 消失）才是 confirmed。
            let fake2 = FakeBridge::new(Reply::Body(accepted_body()));
            let pending2 = fake_pending(single_card("ix-1"));
            pending2.clear_on_wait();
            let sender2 = answer_sender(&fake2, &pending2, &store);
            let confirmed = sender2
                .execute(
                    &ctx("a-2"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();
            assert_eq!(confirmed.state, ActionState::Confirmed);
            assert!(!pending2.host_has_pending(&target(), "ix-1"));
            assert_eq!(stored_state(&store, "a-2").as_deref(), Some("confirmed"));
        }

        /// 宿主卡在 ACK 前已消失且没有 accepted ACK：只能 unknown，不得消费。
        #[tokio::test]
        async fn host_clear_before_ack_is_unknown() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(json!({ "status": "noop" })));
            let pending = fake_pending(single_card("ix-1"));
            // 宿主已撤卡（本地投影尚未跟上）。
            pending.clear_host_card();
            let sender = answer_sender(&fake, &pending, &store);

            let receipt = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();
            assert_eq!(
                receipt.state,
                ActionState::Unknown,
                "无 accepted ACK 且宿主已撤卡只能 unknown"
            );
            assert_eq!(receipt.reason_code.as_deref(), Some("stale_pending"));
            assert_eq!(resolve_frames(&fake).len(), 1, "帧已写出，只收口为 unknown");
            assert!(!pending.consumed("ix-1"), "未受理不得本地消费");
        }

        /// 错会话/错代际：零帧且不消费这张卡（正确目标随后仍可作答）。
        #[tokio::test]
        async fn wrong_session_or_anchor_does_not_consume_other_card() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);
            let answers = vec![answer_entry("原题？", &["A"])];

            let other = BoundTarget {
                session_id: "sess_ffffffffffffffffffffffffffffffff".to_owned(),
                ..target()
            };
            let err = sender
                .execute(
                    &ctx("a-1"),
                    &other,
                    &answer_action_for(&other.session_id, GEN, "ix-1", answers.clone()),
                )
                .await
                .unwrap_err();
            assert_eq!(err.code, "stale_pending");
            assert_eq!(fake.wire(), 0);

            let newer = BoundTarget {
                generation: GEN + 1,
                ..target()
            };
            let err = sender
                .execute(
                    &ctx("a-2"),
                    &newer,
                    &answer_action_for(SESSION, GEN + 1, "ix-1", answers.clone()),
                )
                .await
                .unwrap_err();
            assert_eq!(err.code, "stale_pending");
            assert_eq!(fake.wire(), 0);

            // 正确目标仍可作答：错目标尝试没有消费/在途锁死这张卡。
            let receipt = sender
                .execute(&ctx("a-3"), &target(), &answer_action("ix-1", answers))
                .await
                .unwrap();
            assert_eq!(receipt.state, ActionState::Accepted);
            assert_eq!(resolve_frames(&fake).len(), 1);
        }

        /// 多题答案合并成**一次** resolve；多选回传宿主 value 数组。
        #[tokio::test]
        async fn multi_question_resolves_once() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let questions = vec![
                question("颜色？", &["Red", "Blue"], false, false),
                question("尺寸？", &["S", "M"], true, false),
            ];
            let pending = fake_pending(card("ix-1", false, questions));
            let sender = answer_sender(&fake, &pending, &store);
            let answers = vec![
                answer_entry("颜色？", &["Red"]),
                answer_entry("尺寸？", &["S", "M"]),
            ];
            let receipt = sender
                .execute(&ctx("a-1"), &target(), &answer_action("ix-1", answers))
                .await
                .unwrap();
            assert_eq!(receipt.state, ActionState::Accepted);
            let frames = resolve_frames(&fake);
            assert_eq!(frames.len(), 1, "多题合并为恰一次 resolve");
            let content = &frames[0].args[0]["payload"]["answer"]["content"];
            assert_eq!(content["answers"]["颜色？"], json!("Red"));
            assert_eq!(content["answers"]["尺寸？"], json!("S, M"));
            assert_eq!(content["answer_0"], json!("Red"));
            assert_eq!(content["answer_1"], json!(["S", "M"]));
            assert!(content.get("answer").is_none(), "多题不带单题兼容字段");
        }

        /// 同 actionId 同载荷重放：零第二帧，只回既有回执。
        #[tokio::test]
        async fn answer_reuses_action_id_idempotently() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);
            let action = answer_action("ix-1", vec![answer_entry("原题？", &["A"])]);

            let first = sender
                .execute(&ctx("a-1"), &target(), &action)
                .await
                .unwrap();
            let second = sender
                .execute(&ctx("a-1"), &target(), &action)
                .await
                .unwrap();

            assert_eq!(first.state, ActionState::Accepted);
            assert_eq!(second, first, "同 actionId 同载荷必须返回既有回执");
            assert_eq!(resolve_frames(&fake).len(), 1, "重放零第二帧");
            assert_eq!(row_count(&store), 1);
        }

        // ============================================================
        // Fix round 1：snooze 幂等 noop / 在途闸释放 / 预留顺序 / 失败面
        // ============================================================

        /// snooze 的 `noop`（宿主「已暂停」）是幂等成功：卡仍在宿主集合 → 照常
        /// 恰一帧 resolve（重复 snooze 不得把卡锁死）。
        #[tokio::test]
        async fn snooze_noop_is_already_snoozed_then_resolves() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            // FakePending 的 `Ok` 即端口语义「accepted/duplicate/noop 均已暂停」。
            let pending = fake_pending(card(
                "ix-1",
                true,
                vec![question("原题？", &["A"], false, false)],
            ));
            let sender = answer_sender(&fake, &pending, &store);

            let receipt = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();

            assert_eq!(receipt.state, ActionState::Accepted);
            assert_eq!(pending.snooze_calls(), 1, "snooze 恰一帧");
            assert_eq!(
                pending.snooze_clients(),
                vec![DEVICE.to_owned()],
                "snooze 的 clientId 必须与 resolve 同源（可信设备 id）"
            );
            assert_eq!(resolve_frames(&fake).len(), 1, "noop 后仍必须 resolve");
            assert_eq!(fake.envelope(0)["clientId"], json!(DEVICE));
        }

        /// snooze 被宿主显式拒绝：硬失败，零 resolve，释放在途（可重试）。
        #[tokio::test]
        async fn snooze_rejected_zero_resolve() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(card(
                "ix-1",
                true,
                vec![question("原题？", &["A"], false, false)],
            ));
            pending.set_snooze_error(codes::INVALID_ACTION);
            let sender = answer_sender(&fake, &pending, &store);

            let receipt = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();
            assert_eq!(receipt.state, ActionState::Unknown);
            assert_eq!(receipt.reason_code.as_deref(), Some(codes::INVALID_ACTION));
            assert_eq!(pending.snooze_calls(), 1, "snooze 恰一帧");
            assert_eq!(resolve_frames(&fake).len(), 0, "rejected 必须零 resolve");
            assert!(
                !pending.inflight("ix-1"),
                "resolve 未写出（可证明零写）必须释放在途"
            );

            // 释放后可重试：宿主下次接受 snooze → 恰一帧 resolve。
            pending.set_snooze_ok();
            let retry = sender
                .execute(
                    &ctx("a-2"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();
            assert_eq!(retry.state, ActionState::Accepted);
            assert_eq!(resolve_frames(&fake).len(), 1);
        }

        /// resolve ACK `rejected` 且卡仍在宿主集合：释放本地在途，用户可换新
        /// actionId 重试（宿主 alreadyResolved 幂等是最后防线）。
        #[tokio::test]
        async fn rejected_resolve_releases_inflight() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(json!({"status": "rejected"})));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);

            let rejected = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();
            assert_eq!(rejected.state, ActionState::Rejected);
            assert!(!pending.inflight("ix-1"), "rejected 必须释放在途闸");
            assert_eq!(resolve_frames(&fake).len(), 1);

            fake.set_reply(Reply::Body(accepted_body()));
            let retry = sender
                .execute(
                    &ctx("a-2"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();
            assert_eq!(retry.state, ActionState::Accepted, "新 actionId 可重试");
            assert_eq!(resolve_frames(&fake).len(), 2, "恰一帧/次，无自动重发");
            assert_eq!(row_count(&store), 2);
        }

        /// 写后 unknown 保留在途闸（不自动重试）；只有宿主撤卡帧才释放。
        #[tokio::test]
        async fn unknown_keeps_gate_until_host_frame() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Error(codes::NOT_READY));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);
            let answers = vec![answer_entry("原题？", &["A"])];

            let unknown = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", answers.clone()),
                )
                .await
                .unwrap();
            assert_eq!(unknown.state, ActionState::Unknown);
            assert!(pending.inflight("ix-1"), "写后未知不得自动放开在途闸");
            assert_eq!(resolve_frames(&fake).len(), 1);

            // 宿主帧仍列该卡：闸保持，重试 stale 且零新帧。
            let blocked = sender
                .execute(
                    &ctx("a-2"),
                    &target(),
                    &answer_action("ix-1", answers.clone()),
                )
                .await
                .unwrap_err();
            assert_eq!(blocked.code, "stale_pending");
            assert_eq!(resolve_frames(&fake).len(), 1, "未知结果绝不自动重发");

            // 宿主帧撤卡（册整体替换缺席）→ 闸释放、展示同步撤下。
            pending.host_frame_without_card();
            assert!(!pending.inflight("ix-1"));
            let hidden = sender
                .execute(
                    &ctx("a-3"),
                    &target(),
                    &answer_action("ix-1", answers.clone()),
                )
                .await
                .unwrap_err();
            assert_eq!(hidden.code, "stale_pending", "撤卡后本地展示不得复活");

            // 宿主后续帧重新列出该 id：无需人工清闸即可重新作答。
            pending.host_frame_relists_card();
            fake.set_reply(Reply::Body(accepted_body()));
            let retried = sender
                .execute(&ctx("a-4"), &target(), &answer_action("ix-1", answers))
                .await
                .unwrap();
            assert_eq!(retried.state, ActionState::Accepted);
            assert_eq!(resolve_frames(&fake).len(), 2);
        }

        /// 本地答案校验失败：零 resolve、零账本行、零审计（预留晚于校验）。
        #[tokio::test]
        async fn invalid_answer_leaves_no_reservation() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);

            let error = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["Z"])]),
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, "invalid_answer");
            assert_eq!(resolve_frames(&fake).len(), 0);
            assert_eq!(row_count(&store), 0, "本地拒绝不得留下 reserved 行");
            assert!(audits(&store).is_empty(), "本地拒绝不得写 started 审计");
            assert!(!pending.inflight("ix-1"), "校验失败必须释放在途");
        }

        /// 授权失败（卡已撤/锚过期）：零 resolve、零账本行、零审计。
        #[tokio::test]
        async fn stale_pending_leaves_no_reservation() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);

            let error = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-missing", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, "stale_pending");
            assert_eq!(resolve_frames(&fake).len(), 0);
            assert_eq!(row_count(&store), 0, "授权失败不得留下 reserved 行");
            assert!(audits(&store).is_empty(), "授权失败不得写 started 审计");
        }

        /// resolve 确证零写（桥绑定拒绝）：send_failed 释放在途并允许新 actionId 重试。
        #[tokio::test]
        async fn resolve_zero_write_releases_inflight() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Error(codes::STALE_GENERATION));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);
            let answers = vec![answer_entry("原题？", &["A"])];

            let failed = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", answers.clone()),
                )
                .await
                .unwrap();
            assert_eq!(failed.state, ActionState::SendFailed);
            assert!(!pending.inflight("ix-1"), "确证零写必须释放在途闸");

            fake.set_reply(Reply::Body(accepted_body()));
            let retry = sender
                .execute(&ctx("a-2"), &target(), &answer_action("ix-1", answers))
                .await
                .unwrap();
            assert_eq!(retry.state, ActionState::Accepted);
            assert_eq!(resolve_frames(&fake).len(), 2, "两次各恰一帧");
        }

        /// 独立 Snooze 动作：accepted / rejected / timeout 的收口与零 resolve。
        #[tokio::test]
        async fn standalone_snooze_execute_outcomes() {
            let snooze = ControlAction::Snooze {
                session_id: SESSION.to_string(),
                generation: GEN,
                interaction_id: "ix-1".to_string(),
            };

            // accepted：受理回执，账本 accepted，不消费卡且在途已释放。
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);
            let receipt = sender
                .execute(&ctx("a-1"), &target(), &snooze)
                .await
                .unwrap();
            assert_eq!(receipt.state, ActionState::Accepted);
            assert_eq!(pending.snooze_calls(), 1);
            assert_eq!(pending.snooze_clients(), vec![DEVICE.to_owned()]);
            assert!(!pending.inflight("ix-1"), "snooze 不消费卡，必须释放在途");
            assert!(pending.host_has_pending(&target(), "ix-1"));
            assert_eq!(resolve_frames(&fake).len(), 0);
            assert_eq!(stored_state(&store, "a-1").as_deref(), Some("accepted"));

            // rejected：错误原样上抛，账本保守 unknown，零 resolve。
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(single_card("ix-1"));
            pending.set_snooze_error(codes::INVALID_ACTION);
            let sender = answer_sender(&fake, &pending, &store);
            let error = sender
                .execute(&ctx("a-1"), &target(), &snooze)
                .await
                .unwrap_err();
            assert_eq!(error.code, "invalid_action");
            assert_eq!(stored_state(&store, "a-1").as_deref(), Some("unknown"));
            assert!(!pending.inflight("ix-1"));

            // 超窗 unknown：错误上抛，账本 unknown，绝不重发。
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(single_card("ix-1"));
            pending.set_snooze_error(codes::UNKNOWN);
            let sender = answer_sender(&fake, &pending, &store);
            let error = sender
                .execute(&ctx("a-1"), &target(), &snooze)
                .await
                .unwrap_err();
            assert_eq!(error.code, "unknown");
            assert_eq!(pending.snooze_calls(), 1);
            assert_eq!(stored_state(&store, "a-1").as_deref(), Some("unknown"));
        }
    }
}
