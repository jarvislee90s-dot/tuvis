// 无头回执归一（H6）：**纯函数核**——零 I/O、零进程、零 DB。
//
// 三条纪律：
// 1. **前缀跳过**：stdout 里 JSON 之前可能有非 JSON 行（Mac 实测 zcode 会先吐
//    `ZCode Built-in missing/skipped (not-due)`）——从首个 `{` 起解析，逐候选重试；
// 2. **未知帧不猜**（附录 E-⑧）：JSON 但不含任何已知键 → **只计数不解释**；非 JSON
//    且非 JSON 的行 → 记噪音前缀行。两类都不得污染已累积字段；
// 3. **摘要口径单点**：`last_assistant` 截断复用 `inject::normalize::summarize`
//    （与 W5 审计摘要同一截断语义，不另造一份）。
use crate::inject::normalize;
use serde::{Deserialize, Serialize};

/// 末条 assistant 摘要截断长度（H6：回执不回传消息全文）
pub const LAST_ASSISTANT_CHARS: usize = 200;

/// 回执状态（spec H6：ok|queued|failed|cancelled）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptStatus {
    Ok,
    Queued,
    Failed,
    Cancelled,
}

/// 分阶段失败档（spec H6；WorkspaceBusy = zcode 工作区争用锁专档，Task 8 用）。
///
/// `Refused` 是 **Task 8 复审追补**的第八档：**投递前拒绝**——回合**未起跑、零字节投递**
/// 的拒绝（斜杠命令不具无头语义 / 会话串行锁占用 / 本平台无该通道形态）。它**不是**
/// `ChannelError`（那一档的语义是「通道跑过了但没拿到有效回执」）——混用会让移动端分诊
/// 文案说错话（把「没发出去」显示成「通道异常」）。线上为**增量**变体：老客户端读到
/// 未知档时的兜底由前端分诊表负责（见 `src/mobile/SessionDetail.tsx` 的未分类兜底）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Spawn,
    VersionGate,
    Timeout,
    Crash,
    ChannelError,
    Dialog,
    WorkspaceBusy,
    Refused,
}

/// 终止方（H4：取消与 watchdog **先到者生效，回执注明由谁终止**）。由
/// `(status, stage)` 归一推导——**不新增线上字段**（线上形状 = spec H6 的七键）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminator {
    /// 尚未起跑（排队中 / 版本门控拒发 / spawn 失败）
    NotStarted,
    /// 进程自然退出（含崩溃——崩溃也是进程自己退的）
    Exit,
    /// watchdog 到点终止
    Watchdog,
    /// 移动端主动取消
    Cancel,
}

/// 无头回执（spec H6 形状；`tokens`/`stage`/`reason` 缺席即不上线，不以 null 出现）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    pub status: ReceiptStatus,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_assistant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<Stage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Receipt {
    /// 成功终态（进程自然退出 0）
    pub fn ok(session_id: &str, duration_ms: u64) -> Self {
        Self::base(ReceiptStatus::Ok, session_id, duration_ms)
    }

    /// 失败终态 + 阶段（H6：分阶段失败回执）
    pub fn failed(stage: Stage, reason: &str) -> Self {
        let mut r = Self::base(ReceiptStatus::Failed, "", 0);
        r.stage = Some(stage);
        r.reason = Some(reason.to_string());
        r
    }

    /// 取消终态（移动端主动取消；stage 留空——取消不是失败阶段）
    pub fn cancelled(reason: &str) -> Self {
        let mut r = Self::base(ReceiptStatus::Cancelled, "", 0);
        r.reason = Some(reason.to_string());
        r
    }

    /// 并发名额已满终态（H4：**即时**回执——不阻塞、不静默丢，但**本条未执行**）。
    ///
    /// **文案诚实化（评审 I1）**：旧文案「全局队列第 {position} 位等待中」是假话——
    /// 生产上**没有真排队机制**（无名额即早退，释放后也不会自动补跑），位次恒等于
    /// 「在飞数 + 1」（`waiting` 只有测试面的阻塞取名额会加），故位次与「等待中」一并删除。
    /// 现在只描述**已发生的事实**：名额已满、本条未起跑、未投递，重发是用户的动作。
    pub fn concurrency_full(cap: usize) -> Self {
        let mut r = Self::base(ReceiptStatus::Queued, "", 0);
        r.reason = Some(format!(
            "无头并发已满（上限 {cap}）：本条未执行、未投递——请稍后重发"
        ));
        r
    }

    fn base(status: ReceiptStatus, session_id: &str, duration_ms: u64) -> Self {
        Self {
            status,
            session_id: session_id.to_string(),
            last_assistant: None,
            tokens: None,
            duration_ms,
            stage: None,
            reason: None,
        }
    }

    /// 补会话号（构造器不背会话号时由调用方回填）
    pub fn with_session(mut self, session_id: &str) -> Self {
        if self.session_id.is_empty() {
            self.session_id = session_id.to_string();
        }
        self
    }

    /// 补耗时（构造器只带阶段/原因时的回填口）
    pub fn with_duration_ms(mut self, duration_ms: u64) -> Self {
        self.duration_ms = duration_ms;
        self
    }

    /// 终止方（H4：回执注明由谁终止）
    pub fn terminator(&self) -> Terminator {
        match (self.status, self.stage) {
            (ReceiptStatus::Queued, _) => Terminator::NotStarted,
            (ReceiptStatus::Cancelled, _) => Terminator::Cancel,
            (ReceiptStatus::Failed, Some(Stage::Timeout)) => Terminator::Watchdog,
            // 未起跑三档：排队/版本门控拒发/投递前拒绝（Refused —— 零字节投递）
            (ReceiptStatus::Failed, Some(Stage::Spawn | Stage::VersionGate | Stage::Refused)) => {
                Terminator::NotStarted
            }
            _ => Terminator::Exit,
        }
    }
}

/// 单帧归一读取结果（只认已知键；三键全缺 = 未知帧）
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FrameFacts {
    pub session_id: Option<String>,
    pub assistant_text: Option<String>,
    pub tokens: Option<u64>,
}

/// 读取一帧的已知字段（`None` = 未知帧——**不猜**，由调用方计数）
pub fn read_frame(v: &serde_json::Value) -> Option<FrameFacts> {
    let obj = v.as_object()?;
    let session_id = ["sessionId", "session_id"]
        .iter()
        .find_map(|k| obj.get(*k).and_then(|x| x.as_str()).map(str::to_string));
    let assistant_text = ["response", "result", "text", "lastAssistant", "content"]
        .iter()
        .find_map(|k| obj.get(*k).and_then(|x| x.as_str()).map(str::to_string))
        .filter(|s| !s.trim().is_empty());
    let tokens = ["tokens", "outputTokens", "output_tokens"]
        .iter()
        .find_map(|k| obj.get(*k).and_then(serde_json::Value::as_u64))
        .or_else(|| {
            obj.get("usage")
                .and_then(|u| u.get("output_tokens").or_else(|| u.get("total_tokens")))
                .and_then(serde_json::Value::as_u64)
        });
    if session_id.is_none() && assistant_text.is_none() && tokens.is_none() {
        return None;
    }
    Some(FrameFacts {
        session_id,
        assistant_text,
        tokens,
    })
}

/// 跳过非 JSON 前缀行，取**首个完整 JSON 对象**（Mac 实测污染行的对策）。
/// 返回 `None` = 整段无可用对象帧（调用方按 channel_error 处理，不猜）。
pub fn parse_json_skipping_prefix(raw: &str) -> Option<serde_json::Value> {
    let mut from = 0usize;
    while let Some(rel) = raw[from..].find('{') {
        let start = from + rel;
        // 流式反序列化：允许对象后跟任意尾串（"}\ntail" 也算成功）
        let mut it =
            serde_json::Deserializer::from_str(&raw[start..]).into_iter::<serde_json::Value>();
        if let Some(Ok(v)) = it.next() {
            if v.is_object() {
                return Some(v);
            }
        }
        from = start + 1;
        if from >= raw.len() {
            break;
        }
    }
    None
}

/// 增量帧累积器（runner 边读 stdout 边喂；未知帧/噪音分账计数）
#[derive(Debug, Clone, Default)]
pub struct FrameAccumulator {
    pub session_id: Option<String>,
    pub last_assistant: Option<String>,
    pub tokens: Option<u64>,
    /// 含已知键的对象帧数
    pub json_frames: usize,
    /// 无 JSON 的噪音前缀行数（Mac 实测的 `ZCode Built-in …` 一类）
    pub noise_lines: usize,
    /// JSON 但无已知键的帧数（**只计数不解释**——附录 E-⑧ 未知帧不猜）
    pub unknown_frames: usize,
}

impl FrameAccumulator {
    /// 喂一行 stdout（幂等累积：会话号/耗时外字段取**最新非空**）
    pub fn push_line(&mut self, line: &str) {
        let t = line.trim();
        if t.is_empty() {
            return; // 空行既非噪音也非帧
        }
        match parse_json_skipping_prefix(t) {
            Some(v) => match read_frame(&v) {
                Some(f) => {
                    self.json_frames += 1;
                    if f.session_id.is_some() {
                        self.session_id = f.session_id;
                    }
                    if f.assistant_text.is_some() {
                        self.last_assistant = f.assistant_text;
                    }
                    if f.tokens.is_some() {
                        self.tokens = f.tokens;
                    }
                }
                None => self.unknown_frames += 1,
            },
            // 行内无 `{`：是合法 JSON（数组/标量）→ 未知帧；完全不是 JSON → 噪音行
            None => {
                if serde_json::from_str::<serde_json::Value>(t).is_ok() {
                    self.unknown_frames += 1;
                } else {
                    self.noise_lines += 1;
                }
            }
        }
    }

    /// 归一回执：末条 assistant 按 [`LAST_ASSISTANT_CHARS`] 截断（W5 同口径）
    pub fn receipt(
        &self,
        fallback_session_id: &str,
        status: ReceiptStatus,
        stage: Option<Stage>,
        duration_ms: u64,
    ) -> Receipt {
        let mut r = Receipt::base(status, fallback_session_id, duration_ms);
        r.session_id = self
            .session_id
            .clone()
            .unwrap_or_else(|| fallback_session_id.to_string());
        r.last_assistant = self
            .last_assistant
            .as_deref()
            .map(|t| normalize::summarize(t, LAST_ASSISTANT_CHARS));
        r.tokens = self.tokens;
        r.stage = stage;
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 计划书 Step 1 原例（Mac 实测）：JSON 前有 "ZCode Built-in missing" 等非 JSON
    /// 行——跳过前缀取首个 '{' 起。
    #[test]
    fn parses_json_after_noise_prefix_lines() {
        let raw = "ZCode Built-in skipped (not-due)\n{\"sessionId\":\"s1\",\"response\":\"hi\"}";
        let v = parse_json_skipping_prefix(raw).unwrap();
        assert_eq!(v["sessionId"], "s1");
    }

    /// 计划书 Step 1 原例：回执归一——失败回执必须带 status=Failed 与阶段。
    #[test]
    fn receipt_normalization_maps_stages() {
        let r = Receipt::failed(Stage::Timeout, "watchdog 600s");
        assert_eq!(r.status, ReceiptStatus::Failed);
        assert!(matches!(r.stage, Some(Stage::Timeout)));
    }

    /// last_assistant 截断 200 字（多字节安全：按字符不按字节）
    #[test]
    fn last_assistant_truncated_to_200_chars() {
        let long = "答".repeat(260);
        let mut acc = FrameAccumulator::default();
        acc.push_line(&format!("{{\"response\":\"{long}\"}}"));
        let r = acc.receipt("s1", ReceiptStatus::Ok, None, 12);
        let last = r.last_assistant.expect("末条 assistant 摘要必须带出");
        assert_eq!(
            last.chars().count(),
            LAST_ASSISTANT_CHARS + 1,
            "200 字 + 省略号"
        );
        assert!(last.starts_with(&"答".repeat(200)));
    }

    /// 未知帧不猜（附录 E-⑧）：JSON 但不含已知键 → 计数不作解释；非 JSON 前缀行
    /// 单独计噪音；两类都不得污染已累积的字段。
    #[test]
    fn unknown_frames_fall_through_counted_not_interpreted() {
        let mut acc = FrameAccumulator::default();
        acc.push_line("ZCode Built-in skipped (not-due)"); // 噪音前缀行
        acc.push_line("{\"whatever\":123}"); // 未知帧（JSON 无已知键）
        acc.push_line("[1,2,3]"); // 非对象 JSON → 未知帧
        acc.push_line("42"); // 裸标量 → 未知帧
        acc.push_line("{\"sessionId\":\"s1\",\"tokens\":7}"); // 已知帧
        assert_eq!(acc.noise_lines, 1);
        assert_eq!(acc.unknown_frames, 3, "未知帧只计数不解释");
        assert_eq!(acc.json_frames, 1, "只认含已知键的对象帧");
        assert_eq!(acc.session_id.as_deref(), Some("s1"));
        assert_eq!(acc.tokens, Some(7));
        assert_eq!(acc.last_assistant, None, "未知帧不得被猜成 assistant 文本");
    }

    /// Failed / Cancelled 归一 + 终止方（H4：回执须注明由谁终止）
    #[test]
    fn terminal_states_and_terminator_are_normalized() {
        let crash = Receipt::failed(Stage::Crash, "exit=3");
        assert_eq!(crash.terminator(), Terminator::Exit);
        let timeout = Receipt::failed(Stage::Timeout, "watchdog 600s 到点");
        assert_eq!(timeout.terminator(), Terminator::Watchdog);
        let cancelled = Receipt::cancelled("用户取消（移动端）");
        assert_eq!(cancelled.status, ReceiptStatus::Cancelled);
        assert_eq!(cancelled.stage, None, "取消不是失败阶段——stage 留空");
        assert_eq!(cancelled.terminator(), Terminator::Cancel);
        let ok = Receipt::ok("s1", 1234);
        assert_eq!(ok.status, ReceiptStatus::Ok);
        assert_eq!(ok.terminator(), Terminator::Exit);
        // 并发名额已满终态（H4）：未起跑；文案只陈述已发生的事实
        let full = Receipt::concurrency_full(2);
        assert_eq!(full.status, ReceiptStatus::Queued);
        assert_eq!(full.terminator(), Terminator::NotStarted);
        assert_eq!(
            full.reason.as_deref(),
            Some("无头并发已满（上限 2）：本条未执行、未投递——请稍后重发")
        );
    }

    /// 评审 I1（假队列回执诚实化）：满额回执**不得**再声称「第 N 位等待中」——
    /// 生产没有真排队（无名额即早退，释放后也不补跑），位次恒假。
    /// 撤掉修复（回到 `queued(position)` + `queue_position()`）本测必红。
    #[test]
    fn concurrency_full_receipt_never_promises_a_queue_position() {
        let r = Receipt::concurrency_full(2);
        let reason = r.reason.as_deref().unwrap_or("");
        for lie in ["位等待中", "第 3 位", "第 2 位", "全局队列"] {
            assert!(
                !reason.contains(lie),
                "满额回执不得含排队位次话术「{lie}」: {reason}"
            );
        }
        assert!(
            reason.contains("未执行"),
            "必须如实说明本条未执行: {reason}"
        );
        assert!(reason.contains("未投递"), "必须如实说明零投递: {reason}");
        assert!(reason.contains('2'), "必须带出实际并发上限: {reason}");
        // 位次是**不上线的内部字段**，随假话术一并删除——线上不得出现任何位次键
        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("queuedAt").is_none(), "满额回执不得上线条目位次: {v}");
        assert_eq!(
            v["status"],
            serde_json::json!("queued"),
            "状态词仍为「未起跑」档（驱动层负责转译为 refused）"
        );
    }

    /// token 用量：有则取、无则 None（不猜 0）
    #[test]
    fn tokens_extracted_when_present_and_none_when_absent() {
        let mut acc = FrameAccumulator::default();
        assert_eq!(acc.receipt("s1", ReceiptStatus::Ok, None, 1).tokens, None);
        acc.push_line("{\"response\":\"hi\"}");
        assert_eq!(acc.receipt("s1", ReceiptStatus::Ok, None, 1).tokens, None);
        acc.push_line("{\"usage\":{\"output_tokens\":11},\"response\":\"done\"}");
        let r = acc.receipt("s1", ReceiptStatus::Ok, None, 1);
        assert_eq!(r.tokens, Some(11));
        assert_eq!(
            r.last_assistant.as_deref(),
            Some("done"),
            "末条 assistant 取最新"
        );
    }

    /// 线上形状钉死（移动端契约 = spec H6）：camelCase + 可选键缺席即不出现在 JSON 里
    #[test]
    fn receipt_wire_shape_is_pinned() {
        let full = Receipt::failed(Stage::Timeout, "watchdog 600s");
        let v = serde_json::to_value(&full).unwrap();
        assert_eq!(v["status"], serde_json::json!("failed"));
        assert_eq!(v["stage"], serde_json::json!("timeout"));
        assert_eq!(v["reason"], serde_json::json!("watchdog 600s"));
        assert!(v.get("sessionId").is_some(), "键名必须 camelCase: {v}");
        assert!(v.get("durationMs").is_some());
        // 截断摘要在线上也叫 lastAssistant（值经 W5 口径截断）
        let mut acc = FrameAccumulator::default();
        acc.push_line("{\"sessionId\":\"s1\",\"response\":\"hi\"}");
        let hit = acc.receipt("fallback", ReceiptStatus::Ok, None, 7);
        let hv = serde_json::to_value(&hit).unwrap();
        assert_eq!(hv["lastAssistant"], serde_json::json!("hi"));
        assert_eq!(
            hv["sessionId"],
            serde_json::json!("s1"),
            "帧里的会话号覆盖回填值"
        );
        assert_eq!(hv["durationMs"], serde_json::json!(7));
        let sparse = Receipt::ok("s1", 5);
        let sv = serde_json::to_value(&sparse).unwrap();
        assert_eq!(sv["status"], serde_json::json!("ok"));
        for k in ["tokens", "stage", "reason", "lastAssistant"] {
            assert!(sv.get(k).is_none(), "缺席的可选键不得以 null 出现: {sv}");
        }
    }

    /// 阶段串钉死（移动端按 stage 分诊文案）：八档全量在册。
    /// **跨语言锁**：同一份名单另存 `tests/fixtures/headless_stages.json`，前端分诊表
    /// （`src/mobile/SessionDetail.tsx`）与 Rust 侧各自对照它断言——任一侧新增变体而另一侧
    /// 没跟上，必有一侧先红（夹具是唯一名单来源，避免两处硬编码漂移）。
    #[test]
    fn stage_wire_names_are_pinned() {
        let pairs = [
            (Stage::Spawn, "spawn"),
            (Stage::VersionGate, "version_gate"),
            (Stage::Timeout, "timeout"),
            (Stage::Crash, "crash"),
            (Stage::ChannelError, "channel_error"),
            (Stage::Dialog, "dialog"),
            (Stage::WorkspaceBusy, "workspace_busy"),
            (Stage::Refused, "refused"),
        ];
        for (st, name) in pairs {
            assert_eq!(serde_json::to_value(st).unwrap(), serde_json::json!(name));
        }
        // 跨语言夹具锁：枚举变体集合 == 夹具名单（新增变体必须同步夹具与前端分诊表）
        let raw = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("tests")
                .join("fixtures")
                .join("headless_stages.json"),
        )
        .expect("跨语言夹具必须存在（tests/fixtures/headless_stages.json）");
        let fixture: serde_json::Value = serde_json::from_str(&raw).expect("夹具必须是合法 JSON");
        let mut want: Vec<String> = fixture["stages"]
            .as_array()
            .expect("夹具须有 stages 数组")
            .iter()
            .map(|v| v.as_str().expect("stages 元素须为字符串").to_string())
            .collect();
        let mut got: Vec<String> = pairs
            .iter()
            .map(|(st, _)| {
                serde_json::to_value(st)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        want.sort();
        got.sort();
        assert_eq!(
            got, want,
            "Stage 变体集合与跨语言夹具不一致（前端分诊表按同一夹具断言）"
        );
    }

    /// 前缀跳过：无 JSON 行 → None（调用方据此判 channel_error，不猜）
    #[test]
    fn prefix_skip_returns_none_without_json() {
        assert!(parse_json_skipping_prefix("").is_none());
        assert!(parse_json_skipping_prefix("ZCode Built-in missing\nskipped (not-due)").is_none());
        // JSON 前的噪音行含 '}' 也不行：只有 '{' 起才算帧
        assert!(parse_json_skipping_prefix("{oops}\nnot json").is_none());
        // 多帧时取首个完整对象（跨行的对象也能截出）
        let v = parse_json_skipping_prefix("noise\n{\n \"sessionId\": \"s2\"\n}\ntail").unwrap();
        assert_eq!(v["sessionId"], "s2");
    }
}
