//! ZCode 官方桥的写命令派发（生产计划 T3 · Slice B）。
//!
//! 本文件是官方协议**唯一**的会话写出口：HTTP/前端只能提交 [`ControlAction`]，
//! 信封、clientId、commandId、正文签名都在这里生成，手机无法传入任意 wire。
//!
//! # 单次语义（E3 证据：`SendOnceLoop` @ 固定 SHA）
//!
//! - 一个动作在派发前先经 [`ActionStore::reserve`] 落保守 `unknown` 行（Slice A）：
//!   同 actionId 同载荷 → 直接返回既有回执，**零帧**；载荷不同 → `action_payload_conflict`；
//! - 只有 `New` 才构造**恰一个** `sendConversationCommandV4` 帧；无重发计时器、
//!   无第二次 sendText、无 CLI 降级路径；
//! - ACK 窗 20s（[`ACK_TIMEOUT`]）：窗内无应答 → `unknown`（写没写出未知），
//!   绝不自动重发；只有桥**确证零写**的绑定/编码拒绝（`stale_generation` /
//!   `no_session` / `payload_too_large`）才给 `send_failed`（`not_ready` 同时覆盖
//!   「就绪闸拒写」与「写后等应答超窗」，不可区分 → 保守 `unknown`）；
//! - 已 `accepted` 不被无关活动/迟到的失败证据降级（`ActionStore::record_result` 守卫）。
//!
//! # 正文纪律
//!
//! [`compose_official_text`] 是本路线**唯一**正文出口：保留真实换行/中文/emoji
//! （绝不走终端注入的 `normalize_newlines`，那会把换行变字面 `\n` 两个字符）；
//! 签名开关单点复用 [`message_signature_enabled_conn`]（设置缺省 off），`/` 开头
//! 不加后缀。审计/日志/错误只含类别与计数，不摘正文。

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{Map, Value};

use crate::inject::normalize::message_signature_enabled_conn;
use crate::remote::pairing::DeviceStore;
use crate::services::codex_desktop_ipc::types::new_uuid;

use super::action::{action_params, payload_hash, ActionStore, Reservation};
use super::bridge::{BridgeManager, AGENT_CHANNEL};
use super::types::{
    codes, ActionContext, ActionReceipt, ActionState, BoundTarget, CommandPort, ControlAction,
    ZResult, ZcodeError,
};

/// 官方发送方法名（`zcodeAgent.ts:937`、探针 `SendOnceLoop`）。
pub const AGENT_SEND_COMMAND_METHOD: &str = "sendConversationCommandV4";

/// 信封 `type`（普通文本；`command.ts:95-135` 的 `sendText` 只必带 `text`）。
pub const SEND_TEXT_TYPE: &str = "sendText";

/// ACK 等待窗（20s，与生产计划一致）：窗内无应答按保守 `unknown`，绝不重发。
pub const ACK_TIMEOUT: Duration = Duration::from_secs(20);

/// 单条正文的 UTF-8 字节预算。
///
/// 推导：官方物理信封上限 `MAX_PHYSICAL_FRAME_BYTES` = 1 MiB（[`super::codec`]）；
/// 信封 JSON 最坏转义膨胀 6×（每个字节写成 `\u00XX`），再预留 8 KiB 给
/// header/方法名/workspace 路径/时间戳等固定开销，余量除以 6 即正文预算。
/// 超预算在**写帧之前**以 `payload_too_large` 拒绝（零帧零行）。
pub const MAX_TEXT_BYTES: usize =
    (super::codec::MAX_PHYSICAL_FRAME_BYTES.saturating_sub(8_192)) / 6;

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
}

impl CommandSender {
    /// 生产构造：ACK 窗取 [`ACK_TIMEOUT`]。
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
        }
    }

    /// 执行一次控制动作。
    ///
    /// 本批只实现 `Send`；其余 `ControlAction` 变体一律 `invalid_action` 且零帧零行
    /// （T4–T6 逐个接入，绝不伪造成功）。
    pub async fn execute(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        action: &ControlAction,
    ) -> ZResult<ActionReceipt> {
        let ControlAction::Send { text, .. } = action else {
            return Err(invalid_action());
        };
        if action.session_id() != target.session_id {
            return Err(invalid_action());
        }
        if action.generation() != target.generation {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }

        // 正文出口（含空白/预算校验）：校验失败零帧零行，不去碰账本。
        let signature_on = self.store.with(message_signature_enabled_conn);
        let composed = compose_official_text(&ctx.device_name, text, signature_on)?;

        // 幂等指纹只取原始动作参数（不含签名产物）：签名开关变化不改变同一
        // actionId 的语义身份，也绝不因此产生第二次写出。
        let hash = payload_hash(
            super::action::T3_ACTION_KIND,
            &target.session_id,
            target.generation,
            &action_params(action),
        );
        match self.actions.reserve(ctx, target, &hash)? {
            Reservation::Existing(receipt) => return Ok(receipt),
            Reservation::New => {}
        }

        let receipt = self.dispatch_send(ctx, target, composed).await;
        if let Err(error) = self.actions.record_result(&receipt) {
            // 已写出后账本更新失败：不得对手机谎报失败；该行保持保守 unknown，
            // 同 actionId 重放只会拿到 unknown 且不重发。
            log::error!("zcode_actions: receipt not persisted: {error:?}");
        }
        Ok(receipt)
    }

    /// 构造信封并**恰一次**调用发送端口（无重发、无第二次 sendText、无 CLI 降级）。
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
        let (state, reason_code) = match tokio::time::timeout(
            self.ack_timeout,
            self.bridge.call(
                target,
                AGENT_CHANNEL,
                AGENT_SEND_COMMAND_METHOD,
                vec![envelope],
            ),
        )
        .await
        {
            // 超窗：写没写出/受理与否未知，绝不重发。
            Err(_) => (ActionState::Unknown, Some(codes::UNKNOWN.to_owned())),
            Ok(Err(error)) => classify_port_error(&error),
            Ok(Ok(ack)) => classify_ack(&ack),
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
            BridgeManager::call(self, target, channel, method, vec![Value::Object(params)]).await
        })
    }
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
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
        /// 永不应答：模拟写后断网/ACK 丢失（只能靠调用方超窗收口）。
        Pending,
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
    }

    impl CommandPort for FakeBridge {
        fn call<'a>(
            &'a self,
            target: &'a BoundTarget,
            channel: &'a str,
            method: &'a str,
            args: Vec<Value>,
        ) -> BoxFuture<'a, ZResult<Value>> {
            Box::pin(async move {
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
                let reply = self.reply.lock().unwrap().clone();
                match reply {
                    Reply::Body(body) => Ok(body),
                    Reply::Error(code) => Err(ZcodeError::of(code)),
                    Reply::Pending => std::future::pending().await,
                }
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
        let fake = FakeBridge::new(Reply::Pending);
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
        let before = FakeBridge::new(Reply::Pending);
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

    /// 非 send 变体：invalid_action 且零帧零行（T4–T6 才接入，不伪造成功）。
    #[tokio::test]
    async fn non_send_actions_are_invalid_and_zero_wire() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        let sender = sender(&fake, &store);
        let actions = [
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
        for (index, action) in actions.iter().enumerate() {
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
}
