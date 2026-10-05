// H11 三家 CLI 无头通道（Task 13 / C4）：claude 主体（长驻双向审批桥）+ kimi / opencode 薄适配。
//
// # 权威来源（本文件所有 wire 形状都有出处，勿凭印象改）
// - **claude argv 全集 + 恒带 `--permission-mode`（fail-closed）**：spec 附录 E-①
//   （AionCore `claude_conn.rs:154-269` / `adapter/claude.rs:1073-1126`，LIVE-PROBED）。
// - **`control_request` / `control_response` 形状 + allow 必带 `updatedInput` 原样回显 +
//   AskUserQuestion 的 answers 映射（键 = 题面全文，多选 = JSON 数组）+ 弃卡必须 deny**：
//   spec 附录 E-②（`claude_conn.rs:1474-1580`，2.1.178–2.1.227 实机标定）。
// - **turn 终点判据 = `stream_event{message_delta{stop_reason}}`**（勿等 `result` 帧）：
//   spec 附录 E-①（`--include-partial-messages` 的存在理由）。
// - **kimi / opencode 续接形态**：Task 13 实机探针（见 [`KIMI_ARGV_EVIDENCE`] /
//   [`OPENCODE_ARGV_EVIDENCE`] 的逐字记录）。
//
// # 本通道的两条纪律（与底座同规）
// 1. **测试绝不 spawn 真 CLI**：程序发现的生产出口 [`production_cli_spec`] 在 `cfg(test)`
//    恒 `None`（同 `zcode::production_roots` / `codex::production_exe`），端点用例断言的是
//    **如实的「CLI 不可达」失败**；wire 纯核用注入的帧串 / 剧本桩驱动，零真实进程、
//    零真实账号配额、零真实工具会话库写入。
// 2. **诚实**：审批送不达要说送不达；没有 `stop_reason` 就不能报成功；被拒的工具报「已拒绝」
//    而不是「失败」；未取证的东西在**读者会看的那一行**登记（不是只写在本文件头）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::receipt::{Receipt, ReceiptStatus, Stage};
use super::runner::RunnerCfg;
use super::turn::{BoxFuture, CancelFn, TurnSlot};

// ============================================================
// 程序名词表与生产发现（PATH 扫描 + Windows 垫片）
// ============================================================

/// claude 程序名（PATH 解析的真实目标见 [`production_cli_spec`] / [`SpawnShape`]）
pub const CLAUDE_PROGRAM: &str = "claude";
/// kimi 程序名
pub const KIMI_PROGRAM: &str = "kimi";
/// opencode 程序名
pub const OPENCODE_PROGRAM: &str = "opencode";

/// **claude turn 终点判据的取证记录**（`--include-partial-messages` 的存在理由）：
/// `stream_event{message_delta{stop_reason}}`，**不是** `result` 帧（附录 E-①）。
pub const CLAUDE_TURN_END_EVIDENCE: &str = "附录 E-①（AionCore adapter/claude.rs:1073-1107）";

/// **kimi 续接形态的实机取证记录**（Task 13 探针，2026-10-05，kimi 2.1.1，**1 次真实调用**）：
/// `kimi --session <id> --prompt hi --output-format stream-json`
/// → 出帧 `{"role":"assistant","content":"Hi! How can I help you today?"}` +
/// `{"role":"meta","type":"session.resume_hint","session_id":"session_5e82f154-…",…}`
/// （resume_hint 回带**同一个会话号** ⇒ 确为续接而非新建；会话号形态 = MAM 看板的
/// `session_<uuid>`）。**反例（同次探针里零模型调用的纯参数解析失败）**：plan/spec 的字面
/// 形态 `kimi -p -S <id> "<text>"` 在 2.1.1 **不可用**——`-p <prompt>` 会把紧跟的 `-S`
/// 吃成 prompt 值，`<id>` 因此落成位置参数被当作子命令，报 `unknown command 'session_…'`。
/// 故本通道落长旗标形态。
pub const KIMI_ARGV_EVIDENCE: &str =
    "Task 13 实机探针 2026-10-05（kimi 2.1.1，1 次真实调用）：--session/--prompt/--output-format stream-json";

/// **opencode 续接形态的实机取证记录**（Task 13 探针，2026-10-05，opencode v2.0.22，
/// **1 次真实调用**）：`opencode run --session ses_<id> --format json hi`
/// → 出帧 `{"type":"step_start",…,"sessionID":"ses_f71b120deffer3hV8kaMHyT5gD",…}`
/// （`sessionID` 原样回带 ⇒ `--session <id>` 确为续接、且 id 空间与 MAM 看板一致），
/// 随后 `{"type":"error","error":{"type":"provider.invalid-request","message":"…"}}`
/// ——**该次回合以 provider 错误收尾**（本机 opencode 默认模型 `deepseek-v4-flash:0731`
/// 已于 2026-09-25 退役、HTTP 410），故本次探针**只取证了续接形态与错误帧形状**，
/// 未取证成功回合的文本帧（成功帧形状取自本机 opencode 二进制内的 `D("text",…)` /
/// `D("step_finish",…)` 分派代码，见 [`opencode_argv`] 的登记）。
pub const OPENCODE_ARGV_EVIDENCE: &str =
    "Task 13 实机探针 2026-10-05（opencode v2.0.22，1 次真实调用——provider 410 错误回合）";

/// CLI 启动形态（program + 前置 argv：Windows 批处理垫片要经 `cmd /c`，见底座）
pub use super::turn::SpawnShape;

/// **生产程序发现**（PATH 扫描 + Windows 垫片包装）。
///
/// **`cfg(test)` 恒 `None`**（宪法级测试纪律，同 `zcode::production_roots` /
/// `codex::production_exe`）：本机真装了 claude / kimi / opencode，若单测也去扫真 PATH，
/// 端点用例就会**真的 spawn 一个真实回合**——消耗用户真实账号配额并写真实工具会话库。
/// 故测试构建一律「CLI 不可达」，端点用例断言的是**如实的不可达失败**；纯核
/// （[`super::turn::cli_in_path`] / [`super::turn::spawn_shape`]）用注入表覆盖。
pub fn production_cli_spec(tool: &str, os: &str) -> Option<SpawnShape> {
    #[cfg(test)]
    {
        let _ = (tool, os);
        // 测试构建：**默认恒 None**（下面 [`test_hooks`] 显式注入时才可达——端点用例要驱动
        // 「任务内收尾」路径时用一个**无害系统程序**当假 CLI，绝不 spawn 真工具 CLI）
        test_hooks::shape()
    }
    #[cfg(not(test))]
    {
        let path = std::env::var("PATH").unwrap_or_default();
        let exe = super::turn::cli_in_path(tool, &path, os)?;
        Some(super::turn::spawn_shape(&exe, os))
    }
}

/// **测试专用注入钩子**（仅 `cfg(test)`；生产构建里不存在）。
///
/// 用途（Task 13 复审 Important C 的取证需要）：端点用例要验证「审计与串行锁注销发生在
/// **detached 任务内**」——只有让 CLI 发现**可达**才会走到那条路径。注入的 program 必须是
/// **无害系统程序**（如 `powershell -NoProfile -Command Start-Sleep …`），
/// **绝不允许注入 claude/kimi/opencode 本体**（那是真实配额与真实会话库写入）。
///
/// **并行纪律**：钩子是进程级全局态，凡读写它的用例必须持 [`test_hooks::LOCK`] 串行执行
/// （与 codex/wb_acp 的 test_hooks 同款确定性方案），并在用例结束时 `clear()`。
#[cfg(test)]
pub mod test_hooks {
    use super::SpawnShape;
    use std::sync::Mutex;

    /// 钩子用例串行锁（tokio 互斥量：用例是 `#[tokio::test]`，守卫跨 await 持有）
    pub static LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    static SHAPE: std::sync::LazyLock<Mutex<Option<SpawnShape>>> =
        std::sync::LazyLock::new(|| Mutex::new(None));

    /// 注入（`None` = 恢复「CLI 不可达」的默认诚实态）
    pub fn set_shape(shape: Option<SpawnShape>) {
        *SHAPE.lock().unwrap_or_else(|e| e.into_inner()) = shape;
    }

    pub fn shape() -> Option<SpawnShape> {
        SHAPE.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 用例收尾守卫（panic 也清——全局态别留给后续用例）
    pub struct Clear;

    impl Drop for Clear {
        fn drop(&mut self) {
            set_shape(None);
        }
    }
}

// ============================================================
// claude：argv（**唯一构造点**）
// ============================================================

/// **claude 无头 argv（唯一构造点；`--permission-mode` 不可能缺席）**。
///
/// 序（附录 E-① transport 基集 + 权限档 + 会话旗标）：
/// `--print --input-format stream-json --output-format stream-json --verbose
///  --include-partial-messages --replay-user-messages --permission-prompt-tool stdio
///  --permission-mode <档> [--resume <id> | --session-id <id>]`
///
/// # 为什么 `--permission-mode` 不会被漏掉
/// **真正的保证是下面四条，不是函数里那句 `debug_assert!`**（它在 release 构建里被编译掉，
/// 只是开发期的绊线——Task 13 复审 Minor 4 的措辞更正）：
/// ① 权限档旗子由 [`super::PermissionSpec::claude_default`] **单点产生**（本函数不写字面量）；
/// ② 本函数是 claude argv 的**唯一构造点**（全仓没有第二处拼 claude argv）；
/// ③ 三条测试分别钉住 argv 全集、旗子单点、以及「任何权限档变体都必须产出权限旗子」；
/// ④ **[`ClaudeApprovalMode`] 没有 bypass 变体**（类型上就构造不出「更宽的档」）。
/// fail-closed 的代价说明：省略 `--permission-mode` 时 claude 无头默认 `bypassPermissions`
/// （附录 E-① LIVE-PROBED）——那不是「更宽松一点」，是审批面整体消失。
///
/// `resume`（在册会话续接）与 `fresh`（新会话 id）**互斥**（附录 E-① 实机：对已存在 id
/// 复用 `--session-id` 会硬报 `already in use`）；两者都为 `None` 时**不带会话旗标**
/// （claude 自铸 id）——本批不接线该形态，但形态完整留好。
pub fn claude_argv(_text: &str, resume: Option<&str>, fresh: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "--print".into(),
        "--input-format".into(),
        "stream-json".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--include-partial-messages".into(),
        "--replay-user-messages".into(),
    ];
    a.extend(
        super::PermissionSpec::claude_default()
            .flags()
            .into_iter()
            .map(str::to_string),
    );
    if let Some(id) = resume {
        a.push("--resume".into());
        a.push(id.to_string());
    } else if let Some(id) = fresh {
        a.push("--session-id".into());
        a.push(id.to_string());
    }
    debug_assert!(
        a.iter().any(|x| x == "--permission-mode"),
        "claude argv 必须恒带 --permission-mode（fail-closed；附录 E-①）：{a:?}"
    );
    a
}
/// claude stdin 的 user 帧（NDJSON 单行；`--input-format stream-json`）。
///
/// **取证**：Task 13 探针（2.1.287，1 次真实调用）逐字写入
/// `{"type":"user","message":{"role":"user","content":[{"type":"text","text":"…"}]},"parent_tool_use_id":null}`
/// 被接受，且 CLI 以 `--replay-user-messages` 原样回显（带 `isReplay:true` 与 CLI 自铸的
/// `uuid`）——该回显即「本轮消息已被受理」的证据帧（[`ClaudeEvent::PromptAccepted`]）。
pub fn claude_user_frame(text: &str) -> String {
    serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content": [{"type": "text", "text": text}]},
        "parent_tool_use_id": serde_json::Value::Null,
    })
    .to_string()
}

// ============================================================
// claude：控制面 wire（反向请求解析 / 正向应答构造）
// ============================================================

/// 反向控制请求 `control_request{subtype:"can_use_tool"}`（附录 E-②）。
///
/// `request_id` = **应答相关键**（≠ `tool_use_id`）：缺 `request_id` 的帧不可答 ——
/// 生产侧如实降级为「不可答」（[`ClaudeFrame::Unanswerable`]），**不 wedge 回合**。
#[derive(Debug, Clone, PartialEq)]
pub struct ControlRequest {
    pub request_id: String,
    pub tool_name: String,
    pub tool_use_id: String,
    /// 原样保留的入参（allow 的 `updatedInput` 必须原样回显它；问答答案也是往它上面加键）
    pub input: serde_json::Value,
}

impl ControlRequest {
    /// 是 AskUserQuestion（**走问答卡，不走审批卡**——附录 E-②：投影成独立事件）
    pub fn is_question(&self) -> bool {
        self.tool_name == "AskUserQuestion"
    }

    /// 入参**展示原文**（卡片主体）：Bash = 命令原文；其余工具 = 参数 JSON 文本。
    /// 截断口径沿用回执摘要单点（不另立一套长度）
    pub fn input_display(&self) -> String {
        let raw = match self.input.get("command").and_then(|v| v.as_str()) {
            Some(cmd) => cmd.to_string(),
            None => self.input.to_string(),
        };
        crate::inject::normalize::summarize(&raw, super::receipt::LAST_ASSISTANT_CHARS)
    }
}

/// 解析一行控制请求。**两种形态都收**（同一函数的两个入口，别在别处再写一份）：
/// ① 生产真帧 `{"type":"control_request","request_id":…,"request":{…}}`；
/// ② 裸 request 对象（附录 E-② 的 wire 规格直译形态，Task 13 测试与文档都用它）。
/// `subtype != "can_use_tool"` → `None`（其他 control subtype 一律不猜，见 [`demux_line`]）
pub fn parse_control_request(raw: &str) -> Option<ControlRequest> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let (request_id, req) = match v.get("request") {
        Some(inner) => (
            v.get("request_id")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            inner,
        ),
        None => (String::new(), &v),
    };
    if req.get("subtype").and_then(|s| s.as_str()) != Some("can_use_tool") {
        return None;
    }
    let tool_use_id = req
        .get("tool_use_id")
        .or_else(|| req.get("toolUseID"))
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();
    Some(ControlRequest {
        request_id,
        tool_name: req
            .get("tool_name")
            .or_else(|| req.get("toolName"))
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        tool_use_id,
        input: req.get("input").cloned().unwrap_or(serde_json::Value::Null),
    })
}

/// 审批 / 问答决策。
///
/// `Allow` / `Deny` = 工具审批两臂；**弃卡（dismiss）必须映射成 `Deny`**——附录 E-②：
/// claude 对「allow 但题未答全」是**静默丢弃该题**（不是重问），allow + 空答案 = 静默数据丢失。
/// `Answer` = 问答卡提交，**只能装已答全的 [`AnswerSet`]**（未答全在类型层面就构造不出来）。
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Allow,
    Deny,
    Answer(AnswerSet),
}

impl Decision {
    /// 决策 wire 词（**跨语言夹具 `tests/fixtures/headless_decision_words.json` 锁定**）：
    /// 移动端按钮 id / 应答端点入参 / 审计 result 列三处同源，勿另抄。
    pub const fn wire(&self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Deny => "deny",
            Decision::Answer(_) => "answer",
        }
    }

    /// 从 wire 词反解（应答端点入参 → 决策）。`answer` 需要答案载荷，故此处**不收**
    /// （端点对 `answer` 走 [`AnswerSet`] 构造路径）——未知词一律 `None`（不猜）
    pub fn from_wire(w: &str) -> Option<Decision> {
        match w {
            "allow" => Some(Decision::Allow),
            "deny" => Some(Decision::Deny),
            _ => None,
        }
    }
}

/// 审批卡的两枚决策选项（**单一来源**：移动端只渲染、应答端点只回带 id；词表由
/// [`Decision::wire`] 派生，不得另写一份字面量）
pub fn approval_options() -> Vec<(&'static str, &'static str)> {
    vec![
        (Decision::Allow.wire(), "允许"),
        (Decision::Deny.wire(), "拒绝"),
    ]
}

/// **正向应答构造**（附录 E-② 逐字形状）：
/// ```text
/// {"type":"control_response","response":{"subtype":"success","request_id":"<原样回显>",
///   "response":{"behavior":"allow"|"deny","updatedInput":{…},"toolUseID":"<PendingPerm.tool_use_id>"}}}
/// ```
/// - **allow 必带 `updatedInput`**（record 类型；stdio 通道 schema 必填）：缺省 → ZodError
///   拒绝整个 union → 已批准的工具**永远不跑**（附录 E-② LIVE-PINNED）。语义 = 原样回显
///   请求入参（问答卡则在其上加 `answers` 键）；
/// - deny **不带** `updatedInput`（附录 E-② deny 分支：`{behavior, message, toolUseID}`）。
pub fn build_control_response(req: &ControlRequest, d: &Decision) -> String {
    let inner = match d {
        Decision::Deny => serde_json::json!({
            "behavior": "deny",
            "message": "User rejected the request.",
            "toolUseID": req.tool_use_id,
        }),
        Decision::Allow => {
            let input = if req.input.is_object() {
                req.input.clone()
            } else {
                // 非 object 时防御性落 {}（附录 E-②），绝不把 null 塞进 updatedInput
                serde_json::json!({})
            };
            serde_json::json!({
                "behavior": "allow",
                "updatedInput": input,
                "toolUseID": req.tool_use_id,
            })
        }
        Decision::Answer(set) => {
            let mut input = if req.input.is_object() {
                req.input.clone()
            } else {
                serde_json::json!({})
            };
            input["answers"] = serde_json::from_str(&build_ask_answers(&set.0))
                .unwrap_or_else(|_| serde_json::json!({}));
            serde_json::json!({
                "behavior": "allow",
                "updatedInput": input,
                "toolUseID": req.tool_use_id,
            })
        }
    };
    serde_json::json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": req.request_id,
            "response": inner,
        }
    })
    .to_string()
}

// ============================================================
// 问答（AskUserQuestion）：题面 / 答案集 / answers 映射
// ============================================================

/// AskUserQuestion 的**一题**（附录 E-②/③：`{question, header?, options:[{label,description?}],
/// multiSelect?}`）。
///
/// 命名沿用 spec 的 wire 规格名 `Q`（Task 13 测试直译用它）。
#[derive(Debug, Clone, PartialEq)]
pub struct Q {
    /// 题面全文 = answers 的**键**（附录 E-②：键 = question 文本，不是 header、不是序号）
    pub text: String,
    pub header: Option<String>,
    pub multi: bool,
    /// 选项表 `[(label, description)]`（顺序原样保留——移动端按序渲染）
    pub options: Vec<(String, Option<String>)>,
    /// 已选标签（单选 = 至多一项；多选 = 若干项）
    pub selected: Vec<String>,
    /// Other 自由文本行（可为空）
    pub free: Option<String>,
}

impl Q {
    /// 单选（wire 直译形态：`Q::single(题面, 选项标签, 已选标签)`）
    pub fn single(text: &str, options: Vec<&str>, selected: &str) -> Self {
        Self {
            text: text.to_string(),
            header: None,
            multi: false,
            options: options.into_iter().map(|o| (o.to_string(), None)).collect(),
            selected: if selected.is_empty() {
                Vec::new()
            } else {
                vec![selected.to_string()]
            },
            free: None,
        }
    }

    /// 多选（wire 直译形态：`Q::multi(题面, 选项标签, 已选标签表)`）
    pub fn multi(text: &str, options: Vec<&str>, selected: Vec<&str>) -> Self {
        Self {
            text: text.to_string(),
            header: None,
            multi: true,
            options: options.into_iter().map(|o| (o.to_string(), None)).collect(),
            selected: selected.into_iter().map(str::to_string).collect(),
            free: None,
        }
    }

    /// Other 自由文本行（附录 E-③：有自由作答的题）
    pub fn with_free(mut self, text: &str) -> Self {
        self.free = Some(text.to_string());
        self
    }

    /// 自由文本有效值（trim 后非空才算答了）
    pub fn free_value(&self) -> Option<&str> {
        self.free
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// 本题是否已作答（**判据单点**：选项非空 或 自由文本非空）。未答全 → 不允许提交
    /// （防静默丢题——附录 E-②：claude 对未答题静默丢弃，不重问）
    pub fn answered(&self) -> bool {
        !self.selected.is_empty() || self.free_value().is_some()
    }

    /// 答案标签表（原样 + 自由文本行；多选与单选的 join 规则见 [`build_ask_answers`]）
    pub fn labels(&self) -> Vec<String> {
        let mut out = self.selected.clone();
        if let Some(f) = self.free_value() {
            out.push(f.to_string());
        }
        out
    }
}

/// 未答全的问答集（**拒绝提交的原因**：缺哪几题，逐题点名——不吞信息）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskIncomplete {
    pub missing: Vec<String>,
}

impl AskIncomplete {
    /// 供移动端 / 回执直接展示的中文原因（后端给文案、前端只渲染）
    pub fn reason(&self) -> String {
        format!(
            "问答未答全，不允许提交（防静默丢题）：仍有 {} 题未作答——{}",
            self.missing.len(),
            self.missing.join("；")
        )
    }
}

/// **已答全**的问答集：唯一构造点 [`AnswerSet::new`]（未答全 → `Err`）。
///
/// **为什么用类型而不是运行时检查**：附录 E-② 的失败模式是**静默丢题**（allow + 空答案
/// 会让 claude 丢掉整题且不重问）——那种错误一旦发生，用户与 MAM 都不会看到任何异常。
/// 把它做成「构造不出来」，[`Decision::Answer`] 就永远装不进未答全的集合（belt），
/// 移动端再禁一次提交按钮（braces）。
#[derive(Debug, Clone, PartialEq)]
pub struct AnswerSet(Vec<Q>);

impl AnswerSet {
    /// 构造（**全答才成立**；空题集也拒绝——没有题的「提交」是无意义动作）
    pub fn new(qs: Vec<Q>) -> Result<Self, AskIncomplete> {
        let missing: Vec<String> = qs
            .iter()
            .filter(|q| !q.answered())
            .map(|q| q.text.clone())
            .collect();
        if missing.is_empty() && !qs.is_empty() {
            Ok(AnswerSet(qs))
        } else {
            Err(AskIncomplete {
                missing: if missing.is_empty() {
                    vec!["（题集为空）".to_string()]
                } else {
                    missing
                },
            })
        }
    }

    pub fn questions(&self) -> &[Q] {
        &self.0
    }
}

/// answers 映射（附录 E-② 逐字规则）：**键 = 题面全文**；值 = 单选 → 标签字符串
/// （多标签以 `", "` 连接——claude 的 zod preprocess 同款），多选 → 标签 **JSON 数组**。
///
/// 返回 JSON 对象文本（调用点为 [`build_control_response`] 的 `Answer` 臂；
/// **提交前必须过 [`AnswerSet::new`]**——本函数只负责编码，不做完整性判定，
/// 完整性判定在类型层）。
pub fn build_ask_answers(qs: &[Q]) -> String {
    let mut map = serde_json::Map::new();
    for q in qs {
        let labels = q.labels();
        let v = if q.multi {
            serde_json::json!(labels)
        } else {
            serde_json::json!(labels.join(", "))
        };
        map.insert(q.text.clone(), v);
    }
    serde_json::Value::Object(map).to_string()
}

/// 上行答案被拒的原因（`reason` 供人读、`missing` 供机器判——端点原样透出两键）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerReject {
    pub reason: String,
    /// 未作答的题面（**只有「未答全」这一类拒绝才有**；题面不符/单选多答时为空表）
    pub missing: Vec<String>,
}

impl AnswerReject {
    fn other(reason: String) -> Self {
        Self {
            reason,
            missing: Vec::new(),
        }
    }
}

/// 从移动端上行答案构造 [`AnswerSet`]（**核侧权威**：多选判定取**登记待答项里的题集**，
/// 不信任上行标志——附录 E-② 要求多选编成 JSON 数组、单选编成字符串；若让上行决定，
/// 单选/多选会在 wire 上漂移，而 claude 的 zod 会静默吃掉形态不符的答案）。
///
/// `registered` = 登记待答项载荷里的 `questions` 数组（`PendingRequest::to_payload` 的产物）；
/// `entries` = `(题面, 标签表)`（移动端上行；自由文本行由移动端拼进标签表末尾）。
///
/// 三类拒绝（都返回**可直接展示的中文原因**）：
/// - 题面不在登记题集里 → 「陈旧页面或串话」；
/// - 单选题给了多个标签 → 如实拒绝（**不静默截断**，截断就是替用户做选择）；
/// - 有题未答（含**上行漏题**）→ 逐题点名（`missing` 非空，防静默丢题）。
pub fn answer_set_from_entries(
    registered: &[serde_json::Value],
    entries: &[(String, Vec<String>)],
) -> Result<AnswerSet, AnswerReject> {
    let mut qs: Vec<Q> = Vec::new();
    for (text, labels) in entries {
        let Some(reg) = registered
            .iter()
            .find(|q| q.get("question").and_then(|v| v.as_str()) == Some(text.as_str()))
        else {
            return Err(AnswerReject::other(format!(
                "答案里的题面不在本回合的题集里（陈旧页面或串话）：{text}"
            )));
        };
        let multi = reg
            .get("multiSelect")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !multi && labels.len() > 1 {
            return Err(AnswerReject::other(format!(
                "单选题只允许一个答案（收到 {} 个：{}）——请重新选择：{text}",
                labels.len(),
                labels.join(" / ")
            )));
        }
        let options: Vec<(String, Option<String>)> = reg
            .get("options")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|o| {
                        let l = o.get("label").and_then(|l| l.as_str())?.to_string();
                        Some((l, None))
                    })
                    .collect()
            })
            .unwrap_or_default();
        qs.push(Q {
            text: text.clone(),
            header: None,
            multi,
            options,
            selected: labels
                .iter()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect(),
            free: None,
        });
    }
    // 上行**漏题**也必须被拦（只校验「收到的答案是否答全」会漏掉整题缺失——那正是静默丢题）
    let missing: Vec<String> = registered
        .iter()
        .filter_map(|r| r.get("question").and_then(|v| v.as_str()))
        .filter(|t| !qs.iter().any(|q| q.text == *t && q.answered()))
        .map(str::to_string)
        .collect();
    if !missing.is_empty() {
        let reason = AskIncomplete {
            missing: missing.clone(),
        }
        .reason();
        return Err(AnswerReject { reason, missing });
    }
    AnswerSet::new(qs).map_err(|e| AnswerReject {
        reason: e.reason(),
        missing: e.missing,
    })
}

/// 从 `control_request.input.questions` 投影出题集（附录 E-②：三厂商收敛同形
/// `[{question, header?, options:[{label, description?}], multiSelect?}]`）。
/// 结构不符的题**跳过**（不猜、不造题）；一题都没有 → 空表（生产端如实记日志，
/// 卡片侧显示「没有可答题」而不是编一道题出来）
pub fn project_questions(input: &serde_json::Value) -> Vec<Q> {
    let arr = match input.get("questions").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        .filter_map(|q| {
            let text = q.get("question").and_then(|v| v.as_str())?.to_string();
            let options = q
                .get("options")
                .and_then(|v| v.as_array())
                .map(|opts| {
                    opts.iter()
                        .filter_map(|o| {
                            let label = o.get("label").and_then(|v| v.as_str())?.to_string();
                            let desc = o
                                .get("description")
                                .and_then(|v| v.as_str())
                                .map(str::to_string);
                            Some((label, desc))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(Q {
                text,
                header: q.get("header").and_then(|v| v.as_str()).map(str::to_string),
                multi: q
                    .get("multiSelect")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                options,
                selected: Vec::new(),
                free: None,
            })
        })
        .collect()
}

// ============================================================
// claude：stdout 双流解复用（事件流 / 控制面分离）
// ============================================================

/// 有语义的 claude 事件（**未知帧不猜**——附录 E-⑧ 逃逸舱纪律）
#[derive(Debug, Clone, PartialEq)]
pub enum ClaudeEvent {
    /// 我方 user 帧被 CLI 回显（`--replay-user-messages`；`isReplay:true`）= 消息已受理
    PromptAccepted,
    /// 一条 assistant 消息（取其中的 text 块；`usage` 在 assistant 帧上恒为 0，见 [`Self::TurnEnd`]）
    Assistant { text: String },
    /// 工具调用（诊断面：让审计/回执能说「本轮跑了哪些工具」）
    ToolUse { name: String },
    /// 工具结果（CLI 自执行完工具后回灌；`is_error` = 工具失败，不是回合失败）
    ToolResult { is_error: bool },
    /// **turn 终点候选**：`stream_event{message_delta{stop_reason, usage}}`。
    ///
    /// `terminal` = 该 stop_reason 是否**真的终结本回合**：`end_turn` / `max_tokens` /
    /// `stop_sequence` 是；`tool_use` / `pause_turn` / `null` **不是**（Task 13 探针实证：
    /// 2.1.287 先出 `stop_reason:"tool_use"`、CLI 执行工具、随后才出 `end_turn`——
    /// 把首个 stop_reason 当回合终点会把「工具回合」误判成结束）。
    /// `tokens` = 本次 API 调用的 `output_tokens`（assistant 帧上的 usage 实测恒 0，
    /// 故只有 message_delta 的 usage 可用；回合合计由累积器求和）。
    TurnEnd {
        stop_reason: Option<String>,
        tokens: Option<u64>,
        terminal: bool,
    },
    /// `result` 帧（**只作诊断**：附录 E-① 明令 turn 终点判据不等它——后台任务会把 result 拖住）
    Result { is_error: bool },
    /// 其他帧（system/status/stream_event 增量…）——一律忽略，不猜
    Other,
}

/// 解复用结论
#[derive(Debug, Clone, PartialEq)]
pub enum ClaudeFrame {
    /// 控制面：可答的审批/问答请求
    Control(ControlRequest),
    /// 控制面但**不可答**（缺 request_id：附录 E-② 明示这种帧不能答，只能如实降级）
    Unanswerable(ControlRequest),
    /// 事件流
    Event(ClaudeEvent),
    /// 非 JSON / 未知形状（诊断计数用）
    Noise,
}

/// **stdout 双流解复用**（本通道的核心分离点）：一行 → 控制面 or 事件流。
///
/// 分离**只按帧的 `type` 字段**（不按内容猜）：`control_request` → 控制面；
/// `stream_event` / `assistant` / `user` / `result` / `system` → 事件流；
/// 其余（含其他 control subtype：keep_alive / initialize / set_permission_mode …）
/// 一律 [`ClaudeEvent::Other`]（附录 E-②：那些 subtype 无 FSM 信号，全 opaque）。
pub fn demux_line(line: &str) -> ClaudeFrame {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return ClaudeFrame::Noise;
    };
    match v.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
        "control_request" => {
            let subtype = v
                .get("request")
                .and_then(|r| r.get("subtype"))
                .and_then(|s| s.as_str())
                .unwrap_or_default();
            if subtype != "can_use_tool" {
                return ClaudeFrame::Event(ClaudeEvent::Other);
            }
            match parse_control_request(line) {
                Some(req) if !req.request_id.is_empty() => ClaudeFrame::Control(req),
                Some(req) => ClaudeFrame::Unanswerable(req),
                None => ClaudeFrame::Event(ClaudeEvent::Other),
            }
        }
        "user" => {
            let is_replay = v.get("isReplay").and_then(|b| b.as_bool()).unwrap_or(false);
            let content = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            let is_tool_result = content
                .iter()
                .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"));
            if is_tool_result {
                let is_error = content
                    .iter()
                    .any(|b| b.get("is_error").and_then(|e| e.as_bool()) == Some(true));
                ClaudeFrame::Event(ClaudeEvent::ToolResult { is_error })
            } else if is_replay {
                ClaudeFrame::Event(ClaudeEvent::PromptAccepted)
            } else {
                ClaudeFrame::Event(ClaudeEvent::Other)
            }
        }
        "assistant" => {
            let content = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            if let Some(name) = content
                .iter()
                .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                .and_then(|b| b.get("name"))
                .and_then(|n| n.as_str())
            {
                return ClaudeFrame::Event(ClaudeEvent::ToolUse {
                    name: name.to_string(),
                });
            }
            let text = content
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            if text.trim().is_empty() {
                ClaudeFrame::Event(ClaudeEvent::Other)
            } else {
                ClaudeFrame::Event(ClaudeEvent::Assistant { text })
            }
        }
        "stream_event" => {
            let ev = v.get("event").cloned().unwrap_or(serde_json::Value::Null);
            match ev.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
                "message_delta" => {
                    let stop = ev
                        .get("delta")
                        .and_then(|d| d.get("stop_reason"))
                        .and_then(|s| s.as_str())
                        .map(str::to_string);
                    let tokens = ev
                        .get("usage")
                        .and_then(|u| u.get("output_tokens"))
                        .and_then(|t| t.as_u64());
                    let terminal = matches!(
                        stop.as_deref(),
                        Some("end_turn") | Some("max_tokens") | Some("stop_sequence")
                    );
                    ClaudeFrame::Event(ClaudeEvent::TurnEnd {
                        stop_reason: stop,
                        tokens,
                        terminal,
                    })
                }
                _ => ClaudeFrame::Event(ClaudeEvent::Other),
            }
        }
        "result" => {
            let is_error = v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);
            ClaudeFrame::Event(ClaudeEvent::Result { is_error })
        }
        _ => ClaudeFrame::Event(ClaudeEvent::Other),
    }
}

// ============================================================
// 待答审批登记表（移动端 GET 取、POST 答）
// ============================================================

/// 待答项的种类（wire 词，跨语言夹具锁定）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    /// 工具审批卡（allow / deny 两钮）
    Approval,
    /// 问答卡（题集 + 提交）
    Question,
}

impl PendingKind {
    pub const fn wire(self) -> &'static str {
        match self {
            PendingKind::Approval => "approval",
            PendingKind::Question => "question",
        }
    }
}

/// 待答请求（**进程内内存态**：不落表、无 migration——审批是一次性动作，状态就是
/// 「回合正在等这一答」；回合终结/取消/超时/写失败即注销，见 [`PendingGuard`]）
#[derive(Debug)]
pub struct PendingRequest {
    pub request_id: String,
    pub kind: PendingKind,
    pub tool_name: String,
    pub input_display: String,
    pub questions: Vec<Q>,
    pub session_id: String,
    pub channel: &'static str,
    pub tier: &'static str,
    pub permission_mode: &'static str,
    pub requested_at: Instant,
    tx: tokio::sync::mpsc::UnboundedSender<Decision>,
}

impl PendingRequest {
    /// 移动端载荷（camelCase，与 `src/mobile/api.ts` 的 `HeadlessApprovalPending` 同形；
    /// `input` = 展示原文——**卡片必须能看到要执行的命令原文**，Task 15 M10 的取证点之一）
    pub fn to_payload(&self) -> serde_json::Value {
        let options: Vec<serde_json::Value> = approval_options()
            .into_iter()
            .map(|(id, label)| serde_json::json!({"id": id, "label": label}))
            .collect();
        let questions: Vec<serde_json::Value> = self
            .questions
            .iter()
            .map(|q| {
                serde_json::json!({
                    "question": q.text,
                    "header": q.header,
                    "multiSelect": q.multi,
                    "options": q.options.iter().map(|(l, d)| serde_json::json!({
                        "label": l, "description": d,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        serde_json::json!({
            "requestId": self.request_id,
            "kind": self.kind.wire(),
            "toolName": self.tool_name,
            "input": self.input_display,
            "sessionId": self.session_id,
            "channel": self.channel,
            "tier": self.tier,
            "permissionMode": self.permission_mode,
            "options": options,
            "questions": questions,
            "waitedMs": self.requested_at.elapsed().as_millis() as u64,
        })
    }
}

/// 待答登记表（进程级；**一会话至多一项**——claude 在等应答期间不再发新请求）
#[derive(Default)]
pub struct PendingRegistry {
    map: Mutex<HashMap<String, PendingRequest>>,
}

impl PendingRegistry {
    /// 登记。`Err(原样退回的请求)` = 该会话已有待答项——**如实拒绝、不覆盖**：覆盖会把
    /// 用户**正在看的那张卡**对应的等待方永远挂住（Task 13 复审 Important B：回合侧因此
    /// 必须保留前一项，而不是无条件改用新一项）。
    ///
    /// **为什么把请求退回去**（而不是只回 `None`）：退回物连着它自己的决策接收端，
    /// 回合可以把它**排进本地队列**，等前一项答完再登记上卡——既不丢请求，也不顶卡，
    /// 也不需要调用方重建一遍字段。**Box 只为压小 `Result`**（`PendingRequest` 体积不小，
    /// 按值当 Err 会让每次调用都背一个大对象——clippy::result_large_err 的正当理由）。
    pub fn register(&self, p: PendingRequest) -> Result<(), Box<PendingRequest>> {
        let mut g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if g.contains_key(&p.session_id) {
            return Err(Box::new(p));
        }
        g.insert(p.session_id.clone(), p);
        Ok(())
    }

    /// 注销（回合终结/取消/超时/写失败；重复注销 no-op）
    pub fn clear(&self, session_id: &str) {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }

    /// 当前待答项载荷（GET 端点用）
    pub fn payload_of(&self, session_id: &str) -> Option<serde_json::Value> {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .map(PendingRequest::to_payload)
    }

    /// 是否有待答项（取消/超时措辞与诊断面用）
    pub fn has(&self, session_id: &str) -> bool {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(session_id)
    }

    /// **应答投递**（POST 端点用；`Err` = 未送达的**诚实原因**）：
    /// - 无待答项 → 「没有待答的审批/问答请求」（可能已被取消/超时/回合已终结）；
    /// - `request_id` 不符 → 「请求标识不符」（陈旧页面/串号——不误答别人的请求）；
    /// - 送达 → 移除待答项并交给回合（回合随后写 `control_response` 到 stdin）。
    pub fn deliver(
        &self,
        session_id: &str,
        request_id: &str,
        decision: Decision,
    ) -> Result<(), String> {
        let taken = {
            let mut g = self.map.lock().unwrap_or_else(|e| e.into_inner());
            match g.get(session_id) {
                None => {
                    return Err(
                        "没有待答的审批/问答请求（回合可能已终结、被取消或已超时）".to_string()
                    )
                }
                Some(p) if p.request_id != request_id => {
                    return Err(
                        "请求标识不符（该审批请求已被新的请求替换或页面已陈旧）——请刷新后重试"
                            .to_string(),
                    )
                }
                Some(_) => g.remove(session_id).expect("上面已确认存在"),
            }
        };
        let wire = decision.wire().to_string();
        match taken.tx.send(decision) {
            Ok(()) => {
                log::info!(
                    "headless claude: control_response（审批应答 {wire}）已交给回合写 stdin（会话 {session_id}）"
                );
                Ok(())
            }
            // 回合侧接收端已 drop（取消/超时竞态）：如实报未送达，不谎报已答
            Err(_) => Err("回合已不再等待该应答（取消/超时竞态）——本次应答未送达".to_string()),
        }
    }
}

static PENDING: std::sync::LazyLock<PendingRegistry> =
    std::sync::LazyLock::new(PendingRegistry::default);

/// 进程级待答登记表（GET/POST 端点与回合共用同一份）
pub fn pending_registry() -> &'static PendingRegistry {
    &PENDING
}

/// **测试专用构造**（`cfg(test)`；`tx` 字段在生产是私有的——外部不得塞一个假发送端
/// 冒充「回合在等」）。返回（待答项，决策接收端）：端点用例装一个待答项，再从接收端
/// 断言决策真的投给了回合。
///
/// `tier` / `permission_mode` **不由调用方给**：取 [`super::PermissionSpec::claude_default`]
/// 的生产值（通道 wire 词与档位在生产是同一处单点产生的；让用例自由填会掩盖漂移）。
#[cfg(test)]
pub fn pending_for_test(
    request_id: &str,
    kind: PendingKind,
    tool_name: &str,
    input_display: &str,
    questions: Vec<Q>,
    session_id: &str,
    channel: &'static str,
) -> (
    PendingRequest,
    tokio::sync::mpsc::UnboundedReceiver<Decision>,
) {
    let spec = super::PermissionSpec::claude_default();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Decision>();
    (
        PendingRequest {
            request_id: request_id.to_string(),
            kind,
            tool_name: tool_name.to_string(),
            input_display: input_display.to_string(),
            questions,
            session_id: session_id.to_string(),
            channel,
            tier: spec.tier(),
            permission_mode: spec.permission_mode().unwrap_or("default"),
            requested_at: Instant::now(),
            tx,
        },
        rx,
    )
}

/// 待答项的**注销守卫**（回合终结/取消/超时/写失败/panic 展开都必然注销——
/// 否则移动端会永远看到一张答不了的卡）
struct PendingGuard {
    session_id: String,
}

impl PendingGuard {
    fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
        }
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        pending_registry().clear(&self.session_id);
    }
}

// ============================================================
// claude 长驻回合（裁决 8 特例：进程存活至 turn 结束）
// ============================================================

/// stdin 写口（异步：生产 = tokio `ChildStdin`；测试 = 记录桩）
pub type ClaudeWrite = dyn Fn(String) -> BoxFuture<Result<(), String>> + Send + Sync;

/// 子进程三管 + 退出通知（生产 = 真 spawn；测试 = 剧本桩，**零真实进程**）
pub struct ClaudeIo {
    pub pid: u32,
    pub write: Box<ClaudeWrite>,
    pub lines: tokio::sync::mpsc::UnboundedReceiver<String>,
    pub exit: tokio::sync::oneshot::Receiver<Option<i32>>,
    /// stderr 尾（诊断；生产 = 后台收集，测试 = 记录桩）
    pub stderr: Arc<Mutex<String>>,
}

/// 回合输入（与 IO 分离：IO 是进程，输入是语义）
pub struct ClaudeTurnInput {
    pub session_id: String,
    pub channel: &'static str,
    pub user_frame: String,
    pub timeout_ms: u64,
    pub tier: &'static str,
    pub permission_mode: &'static str,
    /// 取消观察口（生产 = `RunnerCfg::arm_cancel`；测试 = 手动 trigger）；
    /// `None` = 本回合不接取消（测试桩路径）
    pub cancel: Option<tokio::sync::oneshot::Receiver<()>>,
}

/// 回合结局（回执 + 审批计数 + 终结判据证据）
#[derive(Debug, Clone)]
pub struct ClaudeTurnOutcome {
    pub receipt: Receipt,
    /// 本轮收到的可答控制请求数
    pub controls: usize,
    /// 用户批准数
    pub allowed: usize,
    /// 用户拒绝数（**被拒不是失败**：回合继续，模型收到拒绝）
    pub denied: usize,
    /// 用户问答提交数
    pub answered: usize,
    /// 是否命中 turn 终点判据（`stream_event{message_delta{stop_reason=终结值}}`）
    pub turn_ended: bool,
    /// 观察到的 stop_reason（诊断面 / 诚实报告用）
    pub last_stop_reason: Option<String>,
}

/// 回合终结原因（回执措辞与审计都从这里取，避免两处各写一套）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndWhy {
    /// 命中 turn 终点判据（终结值 stop_reason）
    TurnEnd,
    /// stdout 关闭（进程死亡/管道断裂）**且未命中终点判据** → 绝不算成功
    LineClosed,
    /// 进程退出信标到达且未命中终点判据 → 同上
    Exit,
    /// 移动端取消
    Cancelled,
    /// watchdog 到点
    Timeout,
    /// `control_response` 写 stdin 失败（审批应答送不进去 → 回合无法继续，如实终止）
    WriteFailed,
}

/// 决策接收（`None` = 当前无待答项：**永不就绪**——由 `select!` 的 `if` 守卫关掉该分支）
async fn recv_decision(
    rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<Decision>>,
) -> Option<Decision> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending::<Option<Decision>>().await,
    }
}

/// 取消观察（`None` = 本回合不接取消：同样永不就绪）
async fn wait_cancel(rx: &mut Option<tokio::sync::oneshot::Receiver<()>>) {
    match rx {
        Some(r) => {
            let _ = r.await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// 帧累积（回执源：末条 assistant 文本 + 输出 token 合计 + 审批计数 + 诊断证据）
#[derive(Default)]
struct ClaudeAcc {
    last_assistant: Option<String>,
    tokens: u64,
    saw_tokens: bool,
    noise: usize,
    controls: usize,
    /// 因「前一项仍在等用户作答」而排进本地队列的控制请求数（Important B 的可观测面）
    deferred: usize,
    allowed: usize,
    denied: usize,
    answered: usize,
    last_stop_reason: Option<String>,
    prompt_accepted: bool,
    tool_names: Vec<String>,
    tool_results: usize,
    tool_errors: usize,
}

impl ClaudeAcc {
    /// 喂一帧；`Some(())` = **命中 turn 终点判据**（调用方立即收尾）
    fn apply(&mut self, ev: ClaudeEvent) -> Option<()> {
        match ev {
            ClaudeEvent::PromptAccepted => self.prompt_accepted = true,
            ClaudeEvent::Assistant { text } => {
                if !text.trim().is_empty() {
                    self.last_assistant = Some(text);
                }
            }
            ClaudeEvent::ToolUse { name } => self.tool_names.push(name),
            ClaudeEvent::ToolResult { is_error } => {
                self.tool_results += 1;
                if is_error {
                    self.tool_errors += 1;
                }
            }
            ClaudeEvent::TurnEnd {
                stop_reason,
                tokens,
                terminal,
            } => {
                if let Some(t) = tokens {
                    self.tokens += t;
                    self.saw_tokens = true;
                }
                if stop_reason.is_some() {
                    self.last_stop_reason = stop_reason;
                }
                if terminal {
                    return Some(());
                }
            }
            // result 帧只作诊断（附录 E-①：turn 终点判据**不等它**——后台任务会把它拖住）
            ClaudeEvent::Result { .. } | ClaudeEvent::Other => {}
        }
        None
    }

    /// 回合小结（回执 `reason` 的诊断面：工具/审批/证据帧计数——**不编因果**）
    fn digest(&self) -> String {
        let mut parts = vec![format!(
            "控制请求 {}（批准 {}/拒绝 {}/问答 {}）",
            self.controls, self.allowed, self.denied, self.answered
        )];
        if self.deferred > 0 {
            parts.push(format!(
                "其中 {} 项因前一项待答而排队上卡（未丢）",
                self.deferred
            ));
        }
        if !self.tool_names.is_empty() {
            parts.push(format!("工具调用 [{}]", self.tool_names.join(", ")));
        }
        if self.tool_results > 0 {
            parts.push(format!(
                "工具结果 {}{}",
                self.tool_results,
                if self.tool_errors > 0 {
                    format!("（其中 {} 个工具自身报错）", self.tool_errors)
                } else {
                    String::new()
                }
            ));
        }
        if !self.prompt_accepted {
            parts.push("未看到 user 帧回显（--replay-user-messages）".to_string());
        }
        parts.join("；")
    }
}

/// **claude 长驻回合**：进程存活至 turn 结束（裁决 8 的特例），stdout 双流解复用，
/// 控制面请求登记进 [`pending_registry`] 等移动端决策，决策回来即写 `control_response`
/// 到 stdin；turn 终点 = `stream_event{message_delta{stop_reason}}` 的**终结值**
/// （附录 E-①；`result` 帧不等）。
///
/// `kill(pid) -> String` = 整树终止缝（生产 = H4 的 `TreeGuard`；测试 = 记录桩）：
/// **turn 终结、取消、超时、写失败都终止进程**（本通道的进程只为一个 turn 而活；
/// 不 kill 就会留下一个没有 stdin 的孤儿 claude）。
pub async fn run_claude_turn(
    mut io: ClaudeIo,
    mut input: ClaudeTurnInput,
    kill: &(dyn Fn(u32) -> String + Send + Sync),
) -> ClaudeTurnOutcome {
    let started = Instant::now();
    let deadline = started + Duration::from_millis(input.timeout_ms);
    // 待答项注销守卫：终结/取消/超时/写失败/panic 展开都必然注销（卡不能永远挂着）
    let _pending_guard = PendingGuard::new(&input.session_id);
    let mut acc = ClaudeAcc::default();

    // ① user 帧（NDJSON 单行 + flush、不关流——附录 E-② 的同一条 stdin 规则）
    if let Err(e) = (io.write)(input.user_frame.clone()).await {
        let killed = kill(io.pid);
        return ClaudeTurnOutcome {
            receipt: Receipt::failed(
                Stage::ChannelError,
                &format!("写 stdin（user 帧）失败：{e}——回合未起跑；kill 进程树 = {killed}"),
            )
            .with_session(&input.session_id),
            controls: 0,
            allowed: 0,
            denied: 0,
            answered: 0,
            turn_ended: false,
            last_stop_reason: None,
        };
    }

    let mut cancel = input.cancel.take();
    let mut pending_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Decision>> = None;
    let mut pending_req: Option<ControlRequest> = None;
    // 冲突队列（Task 13 复审 Important B）：claude 在等应答期间**又**发来控制请求时，
    // 登记表如实拒绝（一会话一项 = 用户眼前只有一张卡），但**不丢**——连同它的原始请求
    // （应答要用 `tool_use_id`/`input`）与决策接收端排进本地 FIFO，等前一项答完立刻登记上卡
    // （用户接着看到第二张）。先前的实现把本地 pending_req/rx 无条件换成被拒的那一项：
    // 用户正在看的卡的 rx 被丢掉，于是那一答永远「未送达（取消/超时竞态）」、真请求挂到超时
    // ——**已修复**。
    let mut deferred: std::collections::VecDeque<(
        PendingRequest,
        ControlRequest,
        tokio::sync::mpsc::UnboundedReceiver<Decision>,
    )> = std::collections::VecDeque::new();
    let mut exit_code: Option<i32> = None;
    let mut write_fail: Option<String> = None;
    let why: EndWhy;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            why = EndWhy::Timeout;
            break;
        }
        tokio::select! {
            line = io.lines.recv() => {
                match line {
                    Some(l) => match demux_line(&l) {
                        ClaudeFrame::Control(req) => {
                            acc.controls += 1;
                            let kind = if req.is_question() { PendingKind::Question } else { PendingKind::Approval };
                            let questions = if req.is_question() { project_questions(&req.input) } else { Vec::new() };
                            if req.is_question() && questions.is_empty() {
                                // 问答题面为空：**不造题**——如实记日志，卡片侧显示「没有可答题」；
                                // 仍登记（否则用户看不到任何东西，回合静默等到超时）
                                log::warn!("headless claude: AskUserQuestion 的 questions 为空（会话 {}）", input.session_id);
                            }
                            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Decision>();
                            let p = PendingRequest {
                                request_id: req.request_id.clone(),
                                kind,
                                tool_name: req.tool_name.clone(),
                                input_display: req.input_display(),
                                questions,
                                session_id: input.session_id.clone(),
                                channel: input.channel,
                                tier: input.tier,
                                permission_mode: input.permission_mode,
                                requested_at: Instant::now(),
                                tx,
                            };
                            match pending_registry().register(p) {
                                Ok(()) => {
                                    // 上卡（若此刻已有前一项在等，这只可能是**前一项刚被答完**
                                    // 的同一轮里——正常路径下不会走到这里；见 Err 臂）
                                    pending_req = Some(req);
                                    pending_rx = Some(rx);
                                }
                                Err(back) => {
                                    // **前一项仍是用户眼前那张卡**：保留它的 req/rx 不动，
                                    // 本条排队等它答完再上卡（不丢请求、不顶卡）
                                    log::info!(
                                        "headless claude: 同会话已有待答项（用户正在作答）——本条控制请求（{}）排队等前一项答完再上卡（会话 {}）",
                                        back.request_id,
                                        input.session_id
                                    );
                                    deferred.push_back((*back, req, rx));
                                    acc.deferred += 1;
                                }
                            }
                        }
                        ClaudeFrame::Unanswerable(req) => {
                            // 缺 request_id 的帧**不可答**（附录 E-②）：如实记，不臆造 id、不 wedge
                            log::warn!(
                                "headless claude: 控制请求缺 request_id（工具 {}），不可答——回合继续",
                                req.tool_name
                            );
                        }
                        ClaudeFrame::Event(ev) => {
                            if acc.apply(ev).is_some() {
                                why = EndWhy::TurnEnd;
                                break;
                            }
                        }
                        ClaudeFrame::Noise => acc.noise += 1,
                    },
                    // stdout 关闭：进程可能已死（等一小会要个退出码，好写进回执证据）
                    None => {
                        if let Ok(code) = tokio::time::timeout(Duration::from_secs(1), &mut io.exit).await {
                            exit_code = code.ok().flatten();
                        }
                        why = EndWhy::LineClosed;
                        break;
                    }
                }
            }
            d = recv_decision(&mut pending_rx) => {
                if let Some(d) = d {
                    if let Some(req) = pending_req.take() {
                        match d {
                            Decision::Allow => acc.allowed += 1,
                            Decision::Deny => acc.denied += 1,
                            Decision::Answer(_) => acc.answered += 1,
                        }
                        let line = build_control_response(&req, &d);
                        // 待答项已消费：先注销（写失败也不留悬挂卡）
                        pending_registry().clear(&input.session_id);
                        if let Err(e) = (io.write)(line).await {
                            log::error!(
                                "headless claude: control_response 写 stdin 失败（会话 {}）：{e}",
                                input.session_id
                            );
                            write_fail = Some(e);
                            why = EndWhy::WriteFailed;
                            break;
                        }
                        // 生产可查的生命周期 marker（附录 E-② 同规）
                        log::info!(
                            "headless claude: control_response（审批应答 {}）written to stdin（会话 {}）",
                            d.wire(),
                            input.session_id
                        );
                        // **前一项答完 → 立刻把排队的下一项上卡**（用户接着看到第二张；
                        // 不这么做，排队的请求只能等到超时——那等于丢请求）
                        pending_rx = None;
                        if let Some((p, next_req, next_rx)) = deferred.pop_front() {
                            match pending_registry().register(p) {
                                Ok(()) => {
                                    pending_req = Some(next_req);
                                    pending_rx = Some(next_rx);
                                }
                                Err(back) => {
                                    // 理论不可达（刚 clear 过）：如实放回队首，不丢
                                    log::warn!(
                                        "headless claude: 排队项 {} 重新登记失败——放回队列",
                                        back.request_id
                                    );
                                    deferred.push_front((*back, next_req, next_rx));
                                }
                            }
                        }
                    } else {
                        pending_rx = None;
                    }
                }
            }
            _ = wait_cancel(&mut cancel), if cancel.is_some() => {
                why = EndWhy::Cancelled;
                break;
            }
            _ = tokio::time::sleep(remaining) => {
                why = EndWhy::Timeout;
                break;
            }
            code = &mut io.exit => {
                exit_code = code.ok().flatten();
                why = EndWhy::Exit;
                break;
            }
        }
    }

    // ② 收尾：任何形态都终止进程树（turn 结束就没有再留着的理由）
    let killed = kill(io.pid);
    let duration_ms = started.elapsed().as_millis() as u64;
    let waiting = pending_registry().has(&input.session_id);
    let stderr_tail = io
        .stderr
        .lock()
        .map(|s| s.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    let receipt = match why {
        EndWhy::TurnEnd => {
            let mut r = Receipt::ok(&input.session_id, duration_ms);
            r.last_assistant = acc
                .last_assistant
                .clone()
                .map(|t| super::turn::assistant_summary(&t));
            if acc.saw_tokens {
                r.tokens = Some(acc.tokens);
            }
            // 诚实口径：被拒**不是失败**，但必须在回执里说清「有 N 项被拒」
            let mut notes = vec![acc.digest()];
            if acc.denied > 0 {
                notes.push(format!(
                    "其中 {} 项工具/问答请求**已被用户拒绝**（deny）——模型收到的是拒绝，不是失败",
                    acc.denied
                ));
            }
            r.reason = Some(notes.join("；"));
            r
        }
        EndWhy::Cancelled => Receipt::cancelled(&format!(
            "已取消（移动端请求，先到者生效）；kill 进程树 = {killed}"
        ))
        .with_session(&input.session_id)
        .with_duration_ms(duration_ms),
        EndWhy::Timeout => Receipt::failed(
            Stage::Timeout,
            &format!(
                "watchdog {}s 到点{}；kill 进程树 = {killed}（可重试）",
                input.timeout_ms / 1000,
                if waiting {
                    "（回合当时**正在等待审批/问答应答**——移动端未在时限内作答）"
                } else {
                    ""
                }
            ),
        )
        .with_session(&input.session_id)
        .with_duration_ms(duration_ms),
        EndWhy::WriteFailed => Receipt::failed(
            Stage::ChannelError,
            &format!(
                "审批应答写 stdin 失败（{}）——回合无法继续，已终止；kill 进程树 = {killed}",
                write_fail.unwrap_or_else(|| "未知写错误".to_string())
            ),
        )
        .with_session(&input.session_id)
        .with_duration_ms(duration_ms),
        // **未命中终点判据 = 绝不报成功**（诚实纪律；附录 E-① 的判据没出现）
        EndWhy::LineClosed | EndWhy::Exit => {
            let ev = crate::inject::normalize::summarize(
                &super::turn::head_of(&stdout_digest(&acc, &stderr_tail)),
                crate::inject::normalize::AUDIT_SUMMARY_CHARS,
            );
            let how = if why == EndWhy::Exit {
                "进程退出"
            } else {
                "stdout 关闭"
            };
            Receipt::failed(
                Stage::ChannelError,
                &format!(
                    "{how}（exit={}）但**未命中 turn 终点判据**（未见到 \
                     stream_event{{message_delta{{stop_reason}}}} 的终结值——附录 E-①，勿等 result 帧）：\
                     本轮结果未知，请在会话内容中确认后再决定是否重发；证据：{ev}；kill 进程树 = {killed}",
                    exit_code.map_or("-".to_string(), |c| c.to_string())
                ),
            )
            .with_session(&input.session_id)
            .with_duration_ms(duration_ms)
        }
    };
    ClaudeTurnOutcome {
        receipt,
        controls: acc.controls,
        allowed: acc.allowed,
        denied: acc.denied,
        answered: acc.answered,
        turn_ended: why == EndWhy::TurnEnd,
        last_stop_reason: acc.last_stop_reason.clone(),
    }
}

/// 失败回执的证据串（帧统计 + 末条 assistant 摘要 + stderr 尾）——**不编因果**
fn stdout_digest(acc: &ClaudeAcc, stderr_tail: &str) -> Vec<String> {
    let mut v = vec![format!(
        "帧统计：{}",
        if acc.last_stop_reason.is_some() {
            format!(
                "最后 stop_reason={:?}（非终结值）",
                acc.last_stop_reason.as_deref().unwrap_or("-")
            )
        } else {
            "未见到任何 message_delta".to_string()
        }
    )];
    v.push(acc.digest());
    if let Some(t) = &acc.last_assistant {
        v.push(format!("末条 assistant 摘要={t}"));
    }
    if acc.noise > 0 {
        v.push(format!("无法解析的行 {} 条", acc.noise));
    }
    if !stderr_tail.trim().is_empty() {
        v.push(format!("stderr 尾={stderr_tail}"));
    }
    v
}

// ============================================================
// kimi / opencode：薄适配（argv + 回执解析）
// ============================================================

/// kimi argv（**取证形态**，见 [`KIMI_ARGV_EVIDENCE`]）：
/// `--session <id> --prompt <text> --output-format stream-json`
pub fn kimi_argv(text: &str, session_id: &str) -> Vec<String> {
    vec![
        "--session".into(),
        session_id.to_string(),
        "--prompt".into(),
        text.to_string(),
        "--output-format".into(),
        "stream-json".into(),
    ]
}

/// opencode argv（**取证形态**，见 [`OPENCODE_ARGV_EVIDENCE`]）：
/// `run --session <id> --format json <text>`
///
/// 帧词汇取自本机 opencode v2.0.22 二进制内的非交互渲染分派
/// （`D=(i,t,o)=>{if(e.format!=="json")return false; process.stdout.write(JSON.stringify({type:i,timestamp:t,sessionID:e.sessionID,...o}))}`，
/// 以及 `D("text",…{part})` / `D("step_finish",…{part:{type:"step-finish",reason,cost,tokens}})` /
/// `D("error",…{error})`）——**成功回合的文本帧未由 MAM 实机取证**（探针回合止于 provider 410），
/// 故解析按「帧 kind + part 载荷」防御式提取，并把取证等级登记在此。
pub fn opencode_argv(text: &str, session_id: &str) -> Vec<String> {
    vec![
        "run".into(),
        "--session".into(),
        session_id.to_string(),
        "--format".into(),
        "json".into(),
        text.to_string(),
    ]
}

/// 一家 CLI 的回执（末条 assistant + token；`error` = CLI/上游如实错误）
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CliReceipt {
    pub last_assistant: Option<String>,
    pub tokens: Option<u64>,
    /// CLI 自己报的错误（如 provider 410）——**如实透出**，不折算成「成功」
    pub error: Option<String>,
    /// 有语义的帧数（诊断：0 = 什么都没解析出来，回执据此如实报「无回执帧」）
    pub frames: usize,
}

/// kimi `--output-format stream-json` 回执解析（取证帧见 [`KIMI_ARGV_EVIDENCE`]）：
/// `{"role":"assistant","content":"…"}` → 末条回复；`{"role":"meta",…}` → 忽略。
/// **tokens 恒 `None`**：kimi 2.1.1 的 stream-json 输出**不含 usage 帧**（实测）——不猜。
pub fn parse_kimi_receipt(stdout: &[String]) -> CliReceipt {
    let mut out = CliReceipt::default();
    for line in stdout {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v.get("role").and_then(|r| r.as_str()).unwrap_or_default() {
            "assistant" => {
                if let Some(c) = v.get("content").and_then(|c| c.as_str()) {
                    if !c.trim().is_empty() {
                        out.last_assistant = Some(c.to_string());
                        out.frames += 1;
                    }
                }
            }
            "meta" => out.frames += 1,
            _ => {}
        }
    }
    out
}

/// opencode `--format json` 回执解析（帧词汇见 [`opencode_argv`] 的登记）。
///
/// 提取规则（**防御式**：未知帧不猜，只认能确证语义的字段）：
/// - `type:"text"` + `part.text` → 末条 assistant 文本；
/// - `type:"step_finish"` + `part.tokens.output` → 输出 token；
/// - `type:"error"` + `error.message` → 如实错误（**回合失败原因**，不是「成功」）。
pub fn parse_opencode_receipt(stdout: &[String]) -> CliReceipt {
    let mut out = CliReceipt::default();
    for line in stdout {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
            "text" => {
                if let Some(t) = v
                    .get("part")
                    .and_then(|p| p.get("text"))
                    .and_then(|t| t.as_str())
                {
                    if !t.trim().is_empty() {
                        out.last_assistant = Some(t.to_string());
                    }
                }
                out.frames += 1;
            }
            "step_finish" => {
                if let Some(n) = v
                    .get("part")
                    .and_then(|p| p.get("tokens"))
                    .and_then(|t| t.get("output"))
                    .and_then(|n| n.as_u64())
                {
                    out.tokens = Some(n);
                }
                out.frames += 1;
            }
            "error" => {
                let msg = v
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("CLI 报了未带 message 的错误帧");
                out.error = Some(msg.to_string());
                out.frames += 1;
            }
            "step_start" | "reasoning" | "tool" | "patch" | "file" => out.frames += 1,
            _ => {}
        }
    }
    out
}

/// kimi/opencode 的一次性回合执行（复用 Task 6 底座：超时/并发/取消/kill 树全归 runner）。
/// 返回（回执归一产物，各家解析产物）——回执解析由 [`parse_kimi_receipt`] /
/// [`parse_opencode_receipt`] 完成，收尾判定走 [`finish_oneshot`] 单一出口。
pub async fn run_cli_oneshot(
    mut cfg: RunnerCfg,
    session_id: &str,
    parse: &(dyn Fn(&[String]) -> CliReceipt + Send + Sync),
) -> (Receipt, CliReceipt) {
    let receipt = cfg.run().await;
    let stdout = cfg.captured_stdout();
    let parsed = parse(&stdout);
    let stderr = cfg.captured_stderr();
    let exit = cfg.last_exit_code();
    let final_receipt = finish_oneshot(receipt, &parsed, session_id, &stderr, exit, &stdout);
    (final_receipt, parsed)
}

/// 一次性回合的回执收尾（**诚实三态**，单一出口）。
///
/// # 铁律：机器已判的终态**不被改写**（Task 13 复审 Important A 修复）
/// 先前实现把 `Failed` 一律重写成 `failed(channel_error)`——watchdog 到点被杀（`timeout`）、
/// spawn 失败、崩溃等**机器判定的阶段码会丢**，且措辞会说出「**CLI 正常退出**但未拿到回执」
/// 这种与事实相反的话（被杀 ≠ 正常退出）。现在：
/// - `queued` / `cancelled` / `Failed(带 stage)` → **原样返回**（stage 与措辞都由机器给，
///   本函数只**追加**证据：CLI 错误帧原文 / stderr 尾——绝不改 stage、绝不改基调）；
/// - `Ok` → 才轮到「解析产物定成败」：CLI 错误帧 → `failed(channel_error)` + 原文；
///   有末条回复 → `ok`（末条 assistant + tokens via [`super::turn::ok_receipt_with_assistant`]）；
///   否则 → `failed(channel_error)`（此时「正常退出但无回执帧」才是真话）。
fn finish_oneshot(
    receipt: Receipt,
    parsed: &CliReceipt,
    session_id: &str,
    stderr: &str,
    exit: Option<i32>,
    stdout: &[String],
) -> Receipt {
    // 非自然退出终态（排队 / 取消）→ 原样
    if receipt.status == ReceiptStatus::Queued || receipt.status == ReceiptStatus::Cancelled {
        return receipt;
    }
    // **机器已判失败**（timeout / spawn / version_gate / crash / …）→ 保 stage 保措辞，只追加证据
    if receipt.status == ReceiptStatus::Failed {
        return append_evidence(receipt, parsed, stderr, exit);
    }
    // 以下只处理 Ok（runner 眼里进程自然退出 0）
    if let Some(err) = &parsed.error {
        return Receipt::failed(Stage::ChannelError, &format!("CLI 报错：{err}"))
            .with_session(session_id)
            .with_duration_ms(receipt.duration_ms);
    }
    if let Some(text) = &parsed.last_assistant {
        return super::turn::ok_receipt_with_assistant(
            session_id,
            text,
            parsed.tokens,
            receipt.duration_ms,
        );
    }
    let out = crate::inject::normalize::summarize(
        &super::turn::head_of(stdout),
        crate::inject::normalize::AUDIT_SUMMARY_CHARS,
    );
    let err =
        crate::inject::normalize::summarize(stderr, crate::inject::normalize::AUDIT_SUMMARY_CHARS);
    Receipt::failed(
        Stage::ChannelError,
        &format!(
            "CLI 正常退出但未拿到回执（exit={}）：stdout 无可解析回复帧={out}；stderr={err}",
            exit.map_or("-".to_string(), |c| c.to_string())
        ),
    )
    .with_session(session_id)
    .with_duration_ms(receipt.duration_ms)
}

/// 给**机器已判的失败回执**追加诊断证据（CLI 错误帧原文 / stderr 尾）——`stage` 与原因基调
/// **一字不动**（Task 13 复审 Important A：不得把 timeout 说成「正常退出」）。
fn append_evidence(
    mut receipt: Receipt,
    parsed: &CliReceipt,
    stderr: &str,
    exit: Option<i32>,
) -> Receipt {
    let mut extra: Vec<String> = Vec::new();
    if let Some(err) = &parsed.error {
        extra.push(format!("CLI 报错帧：{err}"));
    }
    if !stderr.trim().is_empty() {
        extra.push(format!(
            "stderr 尾={}",
            crate::inject::normalize::summarize(
                stderr,
                crate::inject::normalize::AUDIT_SUMMARY_CHARS
            )
        ));
    }
    // 退出码只在真拿到时追加（watchdog 杀掉的进程没有自然退出码——不编一个「-」出来）
    if let Some(code) = exit {
        extra.push(format!("exit={code}"));
    }
    if !extra.is_empty() {
        let base = receipt.reason.clone().unwrap_or_default();
        receipt.reason = Some(if base.is_empty() {
            extra.join("；")
        } else {
            format!("{base}；{}", extra.join("；"))
        });
    }
    receipt
}

/// **续接 cwd 门**（纯核，单一判据出口）：`None` = 可续接；`Some(原因)` = 如实拒绝。
///
/// 为什么三家都要过这道门（各有实测/文档依据）：
/// - **claude 按 cwd 键控磁盘上的会话**（附录 E-①：跨进程 resume 必须同 cwd 才找得到）；
/// - **opencode 的会话是 project 作用域**（`opencode session list` 只列当前项目；探针即
///   在会话所属目录跑通）；
/// - **kimi 的会话带 `workDir`**（`session_index.jsonl` 逐条记 workDir；探针也在会话
///   workDir 内跑通）——`--session <id>` 是否强依赖同 cwd 未取证，故**只要求路径可用**，
///   不额外假设（不给「必须同 cwd 才认」的过度断言）。
///
/// 空路径（未读卡的 DB-only 会话）或目录不在场（项目被删/移动）→ 拒绝：
/// **没有 cwd 的续接要么找不到会话、要么落到别的项目去**（后者最危险——静默写错会话）。
pub fn cwd_gate(project_path: &str) -> Option<&'static str> {
    let p = project_path.trim();
    if p.is_empty() {
        return Some(
            "会话工作目录未知（未读卡会话没有 cwd 记录）——无头续接需要同 cwd 才能定位到\
             原会话，本条未投递",
        );
    }
    if !std::path::Path::new(p).is_dir() {
        return Some(
            "会话工作目录不在场（项目已移动/删除？）——无头续接需要同 cwd 才能定位到原会话，\
             本条未投递",
        );
    }
    None
}

/// 一家 CLI 的通道占位件（把「会话串行锁 + 取消靶子」接到 [`super::turn::registry`]）；
/// 与 zcode/codex 同规（**不自建登记表**）
pub fn placeholder_slot(agent_type: &str, content: String, channel: &str) -> TurnSlot {
    TurnSlot::placeholder(agent_type, content).with_channel(channel)
}

// ============================================================
// 生产 spawn（**只被生产分派调用**；测试构建里 CLI 发现恒不可达 ⇒ 永不触达）
// ============================================================

/// claude 长驻进程的 spawn 形态（program 已按 [`SpawnShape`] 解好：Windows 垫片经 `cmd /c`）
pub struct ClaudeSpawn {
    pub shape: SpawnShape,
    pub argv: Vec<String>,
    pub cwd: String,
}

/// 生产长驻进程句柄（IO + H4 整树治理句柄）
pub struct ClaudeProcess {
    pub io: ClaudeIo,
    /// H4 进程树治理（Job Object / 进程组）——**复用底座**，不自建第二套 kill
    pub tree: super::runner::TreeGuard,
}

impl ClaudeProcess {
    /// 整树终止缝（交给 [`run_claude_turn`] 的 `kill`）
    pub fn kill_fn(&self) -> impl Fn(u32) -> String + Send + Sync + 'static {
        let tree = self.tree.clone();
        move |_pid: u32| tree.kill().describe()
    }
}

/// stderr 尾行数（诊断面；与 Task 6 的 8 行环缓冲同口径量级）
const CLAUDE_STDERR_TAIL_LINES: usize = 16;

/// **生产 spawn**（长驻：stdin 保持打开；stdout/stderr 逐行消费；退出经 oneshot 通知）。
///
/// # 三条纪律
/// ① **stdin 不关**（`--input-format stream-json` 的多回合/审批应答都走它；关流 = 提前收摊）；
/// ② 进程树治理走 H4 底座（[`super::runner::TreeGuard::adopt`] + 在飞登记表），
///    `kill_on_drop(true)` 兜底；
/// ③ 任何一步失败都**如实回 Err**（调用方如实拒绝，绝不假装回合已起跑）。
pub async fn spawn_claude(sp: &ClaudeSpawn) -> Result<ClaudeProcess, String> {
    use tokio::io::AsyncBufReadExt;
    let mut cmd = tokio::process::Command::new(&sp.shape.program);
    cmd.args(&sp.shape.prefix)
        .args(&sp.argv)
        .current_dir(&sp.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // POSIX：自成进程组 → 杀树可打整组（H4 的同一纪律）
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn 失败（{}）：{e}", sp.shape.program))?;
    let pid = child.id().unwrap_or(0);
    let tree = super::runner::TreeGuard::adopt(pid);
    super::runner::inflight_registry().register(pid, tree.clone());

    let Some(stdin) = child.stdin.take() else {
        let _ = tree.kill();
        return Err("claude stdin 管道不可用（spawn 配置异常）".to_string());
    };
    let stdin = Arc::new(tokio::sync::Mutex::new(stdin));
    let write: Box<ClaudeWrite> = Box::new(move |line: String| {
        let stdin = stdin.clone();
        Box::pin(async move {
            use tokio::io::AsyncWriteExt;
            let mut g = stdin.lock().await;
            g.write_all(line.as_bytes())
                .await
                .map_err(|e| format!("写 stdin 失败：{e}"))?;
            // NDJSON：单行 + 换行 + flush（附录 E-② 的同一条写入规则）
            g.write_all(b"\n")
                .await
                .map_err(|e| format!("写 stdin 换行失败：{e}"))?;
            g.flush()
                .await
                .map_err(|e| format!("flush stdin 失败：{e}"))
        })
    });

    let (lines_tx, lines_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if let Some(out) = child.stdout.take() {
        tokio::spawn(async move {
            let mut r = tokio::io::BufReader::new(out).lines();
            while let Ok(Some(line)) = r.next_line().await {
                if lines_tx.send(line).is_err() {
                    break; // 回合侧已收摊
                }
            }
        });
    }
    let stderr_tail = Arc::new(Mutex::new(String::new()));
    if let Some(err) = child.stderr.take() {
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut r = tokio::io::BufReader::new(err).lines();
            let mut buf: std::collections::VecDeque<String> = std::collections::VecDeque::new();
            while let Ok(Some(line)) = r.next_line().await {
                if buf.len() == CLAUDE_STDERR_TAIL_LINES {
                    buf.pop_front();
                }
                buf.push_back(line);
            }
            *tail.lock().unwrap_or_else(|e| e.into_inner()) =
                buf.into_iter().collect::<Vec<_>>().join("\n");
        });
    }
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<Option<i32>>();
    tokio::spawn(async move {
        let code = child.wait().await.ok().and_then(|s| s.code());
        let _ = exit_tx.send(code);
    });
    Ok(ClaudeProcess {
        io: ClaudeIo {
            pid,
            write,
            lines: lines_rx,
            exit: exit_rx,
            stderr: stderr_tail,
        },
        tree,
    })
}

/// 取消句柄 → 登记表取消请求口（底座单点，通道不另写）
pub fn cancel_of(cfg: &RunnerCfg) -> CancelFn {
    super::turn::cancel_fn_of(cfg)
}

// ============================================================
// 测试（wire 纯核：mock stdin/stdout 帧；**零真实进程、零真实账号配额**）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inject::headless::{
        ClaudeApprovalMode, CodexApprovalPolicy, PermissionSpec, ZcodePermissionMode,
    };

    // ---------- 帧夹具（逐字取自 Task 13 探针 / 附录 E 的 wire 档案）----------

    /// assistant 帧（探针实证形状：`message.content[]` 里有 text 块）
    fn assistant_frame(text: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}],
                        "usage": {"input_tokens": 0, "output_tokens": 0}},
        })
        .to_string()
    }

    /// `stream_event{message_delta}` 帧（探针逐字形状；`usage.output_tokens` 是真值来源）
    fn message_delta_frame(stop: &str, tokens: u64) -> String {
        serde_json::json!({
            "type": "stream_event",
            "event": {"type": "message_delta", "delta": {"stop_reason": stop},
                      "usage": {"input_tokens": 15482, "output_tokens": tokens}},
            "session_id": "d35510ac-0cd6-43ea-8c64-9c76e2c20349",
        })
        .to_string()
    }

    /// 生产真帧的 control_request（附录 E-②：`request_id` 在顶层，请求体在 `request`）
    fn control_request_frame(request_id: &str) -> String {
        serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {"subtype": "can_use_tool", "tool_name": "Bash",
                        "input": {"command": "ls -la"}, "tool_use_id": "t1"},
        })
        .to_string()
    }

    /// 工具结果帧（CLI 自执行完工具后回灌我方 stdin 的那个 user 帧）
    fn tool_result_frame(is_error: bool) -> String {
        serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"tool_use_id": "t1", "type": "tool_result", "content": "probe-1", "is_error": is_error}]},
            "session_id": "s", "uuid": "u1",
        })
        .to_string()
    }

    /// 剧本桩：帧队列 + stdin 记录 + 退出通道（**零真实进程**）
    struct Rig {
        io: ClaudeIo,
        lines: tokio::sync::mpsc::UnboundedSender<String>,
        exit: Option<tokio::sync::oneshot::Sender<Option<i32>>>,
        written: Arc<Mutex<Vec<String>>>,
        kills: Arc<Mutex<Vec<u32>>>,
    }

    fn script_io(frames: &[String]) -> Rig {
        let (lines_tx, lines_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        for f in frames {
            let _ = lines_tx.send(f.clone());
        }
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<Option<i32>>();
        let written = Arc::new(Mutex::new(Vec::<String>::new()));
        let w = written.clone();
        let write: Box<ClaudeWrite> = Box::new(move |l: String| {
            let w = w.clone();
            Box::pin(async move {
                w.lock().unwrap_or_else(|e| e.into_inner()).push(l);
                Ok(())
            })
        });
        Rig {
            io: ClaudeIo {
                pid: 4242,
                write,
                lines: lines_rx,
                exit: exit_rx,
                stderr: Arc::new(Mutex::new(String::new())),
            },
            lines: lines_tx,
            exit: Some(exit_tx),
            written,
            kills: Arc::new(Mutex::new(Vec::<u32>::new())),
        }
    }

    /// 整树终止缝（测试桩：只记录 pid）
    fn kill_fn(kills: Arc<Mutex<Vec<u32>>>) -> impl Fn(u32) -> String + Send + Sync {
        move |pid: u32| {
            kills.lock().unwrap_or_else(|e| e.into_inner()).push(pid);
            "test-stub-tree-kill".to_string()
        }
    }

    fn turn_input(sid: &str, timeout_ms: u64) -> ClaudeTurnInput {
        ClaudeTurnInput {
            session_id: sid.to_string(),
            channel: crate::inject::routing::HeadlessKind::ClaudeP.wire_name(),
            user_frame: claude_user_frame("hi [mobile]"),
            timeout_ms,
            tier: PermissionSpec::claude_default().tier(),
            permission_mode: ClaudeApprovalMode::Stdio.permission_mode(),
            cancel: None,
        }
    }

    /// 等待答项出现（有界轮询；进程级登记表按会话号隔离，用例各用各的 id）
    async fn wait_pending(sid: &str) -> serde_json::Value {
        for _ in 0..400 {
            if let Some(p) = pending_registry().payload_of(sid) {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("待答项未在 2s 内出现（会话 {sid}）");
    }

    /// 等 stdin 已写满 n 行（有界轮询）——**把「回合已消费决策」变成可观测事件**：
    /// `select!` 的两臂同时就绪时选哪一条是随机的，故事件顺序必须由证据钉住，
    /// 不能靠「先 send 决策、后 send 终结帧」的时序假设（否则用例会随机红）
    async fn wait_written(written: &Arc<Mutex<Vec<String>>>, n: usize) {
        for _ in 0..400 {
            if written.lock().unwrap_or_else(|e| e.into_inner()).len() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("stdin 未在 2s 内写满 {n} 行");
    }

    // ===== Step 1（计划逐字）：附录 E ①② 的 wire 规格直译 =====

    #[test]
    fn claude_argv_full_set_e_permission_mode_always_present() {
        // 附录 E-①：恒带 --permission-mode（fail-closed：省略=bypassPermissions）
        let a = super::claude_argv("hi [mobile]", Some("sess_9"), None);
        for f in [
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--replay-user-messages",
            "--permission-prompt-tool",
            "stdio",
            "--permission-mode",
        ] {
            assert!(a.iter().any(|x| x.contains(f)), "缺 {f}");
        }
        assert!(a.iter().any(|x| x == "--resume")); // 与 --session-id 互斥（E-① 实机）
    }

    #[test]
    fn control_response_allow_must_echo_updated_input() {
        // 附录 E-②：allow 必带 updatedInput=原 input 回显；deny 不带；dismiss→deny
        let req = super::parse_control_request(
            r#"{"subtype":"can_use_tool","toolUseID":"t1","tool_name":"Bash","input":{"command":"ls"}}"#,
        )
        .unwrap();
        let allow = super::build_control_response(&req, &super::Decision::Allow);
        assert!(allow.contains("updatedInput") && allow.contains(r#""command":"ls""#));
        let deny = super::build_control_response(&req, &super::Decision::Deny);
        assert!(deny.contains(r#""behavior":"deny""#) && !deny.contains("updatedInput"));
    }

    #[test]
    fn ask_answers_keyed_by_question_text_multiselect_array() {
        // 附录 E-②/③：answers 键=题面文本；多选=数组；未答全 → 不允许提交（防静默丢题）
        let answers = super::build_ask_answers(&[
            super::Q::multi("选框架", vec!["a", "b"], vec!["a"]),
            super::Q::single("确认?", vec!["y", "n"], "y"),
        ]);
        assert!(answers.contains(r#""选框架":["a"]"#) && answers.contains(r#""确认?":"y""#));
    }

    // ===== argv / 权限档（fail-closed 的结构性保证）=====

    /// `--permission-mode` **不可能缺席**：① 权限旗子由 `PermissionSpec::claude_default`
    /// 单点产生（本测同时钉住该单点）；② `claude_argv` 是唯一构造点；③ 函数内 debug_assert。
    /// 反面：`bypassPermissions` 形态（省略 permission-mode 的默认档）在 argv 里不可能出现。
    #[test]
    fn claude_permission_flags_have_a_single_source_and_bypass_is_impossible() {
        let flags = PermissionSpec::claude_default().flags();
        assert_eq!(
            flags,
            vec![
                "--permission-prompt-tool",
                "stdio",
                "--permission-mode",
                "default"
            ],
            "claude 权限旗子必须由 PermissionSpec::ClaudeP 单点产生（Task 10 预留位接线）"
        );
        assert_eq!(PermissionSpec::claude_default().tier(), "stdio");
        assert_eq!(ClaudeApprovalMode::Stdio.wire(), "stdio");
        assert_eq!(ClaudeApprovalMode::Stdio.permission_mode(), "default");
        for resume in [Some("sess_9"), None] {
            let a = claude_argv("hi", resume, None);
            for f in &flags {
                assert!(a.iter().any(|x| x == f), "argv 缺权限旗子 {f}: {a:?}");
            }
            assert!(
                !a.iter().any(|x| x.contains("bypass")),
                "fail-closed：任何变体都不得出现 bypass 档: {a:?}"
            );
        }
    }

    /// **穷尽覆盖全部权限档变体**（Task 13 复审 Minor 4）：`PermissionSpec` 是封闭枚举——
    /// 本测按**无通配 match** 列出它的每一个变体，故将来**新增变体 → 本文件编译失败**，
    /// 强制作者回来显式决定「新变体与 claude 权限旗子的关系」。断言：
    /// ① claude 变体的旗子**恒含**权限旗子（`--permission-prompt-tool stdio` +
    ///    `--permission-mode default`，由 `ClaudeApprovalMode::flags` 单点产生）；
    /// ② 非 claude 变体**不得**夹带 `--permission-mode`（别把 claude 的旗子塞进别家）；
    /// ③ 任何变体都不得产出 bypass 档。
    ///
    /// 这才是「`--permission-mode` 不会被漏掉」的可执行保证——`claude_argv` 里的
    /// `debug_assert!` 在 release 构建里被编译掉，只是开发期绊线（措辞已更正）。
    #[test]
    fn every_permission_spec_variant_is_checked_for_the_claude_flags() {
        // **无通配 match**：新增变体即编译失败（这是本测的一半价值）
        fn is_claude_variant(s: PermissionSpec) -> bool {
            match s {
                PermissionSpec::Zcode(_) => false,
                PermissionSpec::CodexExec(_) => false,
                PermissionSpec::ClaudeP(_) => true,
                PermissionSpec::KimiDefault => false,
                PermissionSpec::OpencodeDefault => false,
            }
        }
        let all = [
            PermissionSpec::Zcode(ZcodePermissionMode::Yolo),
            PermissionSpec::CodexExec(CodexApprovalPolicy::OnRequest),
            PermissionSpec::CodexExec(CodexApprovalPolicy::Never),
            PermissionSpec::ClaudeP(ClaudeApprovalMode::Stdio),
            PermissionSpec::KimiDefault,
            PermissionSpec::OpencodeDefault,
        ];
        let mut seen = 0usize;
        for s in all {
            seen += 1;
            let flags = s.flags();
            assert!(
                !flags.iter().any(|f| f.contains("bypass")),
                "任何档都不得产出 bypass：{s:?} → {flags:?}"
            );
            if is_claude_variant(s) {
                assert_eq!(
                    flags,
                    vec![
                        "--permission-prompt-tool",
                        "stdio",
                        "--permission-mode",
                        "default"
                    ],
                    "claude 档的旗子必须是权限旗子全集（单点产生）"
                );
                assert_eq!(s.permission_mode(), Some("default"));
                // 唯一构造点消费它：argv 里必然出现（resume / fresh / 裸三形态都查）
                for (resume, fresh) in [(Some("s"), None), (None, Some("u")), (None, None)] {
                    let a = claude_argv("hi", resume, fresh);
                    for f in &flags {
                        assert!(a.iter().any(|x| x == f), "argv 缺 {f}: {a:?}");
                    }
                }
            } else {
                assert!(
                    !flags.contains(&"--permission-mode"),
                    "非 claude 档不得夹带 claude 的权限旗子：{s:?} → {flags:?}"
                );
                assert_eq!(s.permission_mode(), None, "非 claude 档没有权限模式词");
            }
        }
        assert_eq!(seen, 6, "穷尽表必须覆盖全部 6 个变体（新增变体请同步本表）");
    }

    /// resume 与 fresh **互斥**（附录 E-①：对已存在 id 复用 `--session-id` 会硬报
    /// `already in use`）；两者都不给时不带会话旗标（claude 自铸 id）
    #[test]
    fn claude_session_flags_are_mutually_exclusive() {
        let resume = claude_argv("hi", Some("sess_9"), None);
        assert!(resume.windows(2).any(|w| w == ["--resume", "sess_9"]));
        assert!(!resume.iter().any(|x| x == "--session-id"));
        let fresh = claude_argv("hi", None, Some("uuid-1"));
        assert!(fresh.windows(2).any(|w| w == ["--session-id", "uuid-1"]));
        assert!(!fresh.iter().any(|x| x == "--resume"));
        let bare = claude_argv("hi", None, None);
        assert!(!bare.iter().any(|x| x == "--resume" || x == "--session-id"));
        // fail-closed 对每一个变体都成立（含裸形态）
        assert!(bare.iter().any(|x| x == "--permission-mode"));
    }

    /// user 帧形态（探针逐字；NDJSON 单行、不关流）
    #[test]
    fn claude_user_frame_is_the_probed_stream_json_shape() {
        let f = claude_user_frame("hi [mobile]");
        assert!(!f.contains('\n'), "stdin 帧必须单行（NDJSON 纪律）");
        let v: serde_json::Value = serde_json::from_str(&f).unwrap();
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"][0]["type"], "text");
        assert_eq!(v["message"]["content"][0]["text"], "hi [mobile]");
    }

    // ===== 双流解复用 + turn 终点判据 =====

    /// 控制面与事件流**只按帧 type 分离**；其他 control subtype 一律不猜；
    /// 缺 `request_id` 的帧如实降级为不可答（附录 E-②：那种帧不能答）
    #[test]
    fn demux_separates_control_plane_from_event_stream() {
        match demux_line(&control_request_frame("req-1")) {
            ClaudeFrame::Control(r) => {
                assert_eq!(r.request_id, "req-1");
                assert_eq!(r.tool_name, "Bash");
                assert_eq!(r.tool_use_id, "t1");
                assert_eq!(r.input["command"], "ls -la");
                assert!(!r.is_question());
            }
            other => panic!("生产真帧必须解出可答控制请求: {other:?}"),
        }
        let no_id = r#"{"type":"control_request","request":{"subtype":"can_use_tool","tool_name":"Write","input":{},"tool_use_id":"t2"}}"#;
        assert!(matches!(demux_line(no_id), ClaudeFrame::Unanswerable(_)));
        // 其他 control subtype（keep_alive/initialize/set_permission_mode…）→ 不猜
        let other_sub =
            r#"{"type":"control_request","request_id":"c1","request":{"subtype":"keep_alive"}}"#;
        assert_eq!(
            demux_line(other_sub),
            ClaudeFrame::Event(ClaudeEvent::Other)
        );
        assert_eq!(demux_line("not json at all"), ClaudeFrame::Noise);
        assert_eq!(
            demux_line(&tool_result_frame(false)),
            ClaudeFrame::Event(ClaudeEvent::ToolResult { is_error: false })
        );
        assert_eq!(
            demux_line(r#"{"type":"user","message":{"role":"user","content":[]},"isReplay":true}"#),
            ClaudeFrame::Event(ClaudeEvent::PromptAccepted)
        );
        assert_eq!(
            demux_line(&assistant_frame("好了")),
            ClaudeFrame::Event(ClaudeEvent::Assistant {
                text: "好了".into()
            })
        );
    }

    /// **turn 终点判据**（附录 E-①）：`stream_event{message_delta{stop_reason}}` 的**终结值**
    /// 才终结回合——`tool_use` 是中途（Task 13 探针实证 2.1.287 先 tool_use 后 end_turn），
    /// `result` 帧**明确不是**终点判据（后台任务会把 result 拖住）。
    #[test]
    fn turn_end_criterion_rejects_tool_use_and_result_frames() {
        let f = demux_line(&message_delta_frame("tool_use", 79));
        match f {
            ClaudeFrame::Event(ClaudeEvent::TurnEnd {
                stop_reason,
                tokens,
                terminal,
            }) => {
                assert_eq!(stop_reason.as_deref(), Some("tool_use"));
                assert_eq!(tokens, Some(79));
                assert!(
                    !terminal,
                    "tool_use 不是回合终点（探针实证：之后还有 end_turn）"
                );
            }
            other => panic!("message_delta 必须解成 TurnEnd: {other:?}"),
        }
        for stop in ["end_turn", "max_tokens", "stop_sequence"] {
            match demux_line(&message_delta_frame(stop, 30)) {
                ClaudeFrame::Event(ClaudeEvent::TurnEnd { terminal, .. }) => {
                    assert!(terminal, "{stop} 必须终结回合")
                }
                other => panic!("{other:?}"),
            }
        }
        // result 帧：只作诊断，**不是** TurnEnd（附录 E-①）
        let result = r#"{"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","usage":{"output_tokens":109}}"#;
        assert_eq!(
            demux_line(result),
            ClaudeFrame::Event(ClaudeEvent::Result { is_error: false })
        );
    }

    // ===== 长驻回合（剧本桩：stdin/stdout 帧纯核）=====

    /// **审批往返全链（纯核）**：stdout 控制面请求 → 登记待答项（移动端可见的载荷）→
    /// 用户批准 → `control_response`（updatedInput 原样回显）写 stdin → 工具结果 →
    /// 终结值 `end_turn` 收尾 → 回执 ok（末条 assistant + tokens）+ 进程被终止 + 待答项注销
    #[tokio::test]
    async fn claude_turn_round_trips_an_approval_and_ends_on_terminal_stop_reason() {
        let sid = "sess_c3_allow";
        let rig = script_io(&[
            assistant_frame("先看看目录"),
            control_request_frame("req-1"),
        ]);
        let written = rig.written.clone();
        let kills = rig.kills.clone();
        let lines = rig.lines.clone();
        let input = turn_input(sid, 5_000);
        let kf = kill_fn(kills.clone());
        let io = rig.io;
        let driver = async {
            let p = wait_pending(sid).await;
            assert_eq!(p["kind"], "approval");
            assert_eq!(p["toolName"], "Bash");
            assert_eq!(p["requestId"], "req-1");
            assert_eq!(
                p["input"], "ls -la",
                "卡片必须带命令原文（Task 15 M10 取证点）"
            );
            assert_eq!(p["tier"], "stdio");
            assert_eq!(p["permissionMode"], "default");
            assert_eq!(p["channel"], "headless_claude_p");
            assert_eq!(
                p["options"],
                serde_json::json!([
                    {"id": "allow", "label": "允许"},
                    {"id": "deny", "label": "拒绝"},
                ]),
                "审批卡选项 id 必须来自 Decision::wire 单点"
            );
            assert!(p["questions"].as_array().unwrap().is_empty());
            pending_registry()
                .deliver(sid, "req-1", Decision::Allow)
                .expect("待答项在场时必须送达");
            // 等 control_response 真的写出（回合已消费决策）再推后续帧——见 wait_written
            wait_written(&written, 2).await;
            lines.send(tool_result_frame(false)).unwrap();
            lines.send(assistant_frame("已经列好了")).unwrap();
            lines.send(message_delta_frame("end_turn", 42)).unwrap();
        };
        let (out, ()) = tokio::join!(run_claude_turn(io, input, &kf), driver);

        assert_eq!(out.receipt.status, ReceiptStatus::Ok, "{:?}", out.receipt);
        assert_eq!(out.receipt.last_assistant.as_deref(), Some("已经列好了"));
        assert_eq!(
            out.receipt.tokens,
            Some(42),
            "tokens 取 message_delta.usage"
        );
        assert!(out.turn_ended && out.last_stop_reason.as_deref() == Some("end_turn"));
        assert_eq!(
            (out.controls, out.allowed, out.denied, out.answered),
            (1, 1, 0, 0)
        );
        let w = written.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(
            w.len(),
            2,
            "本回合只应写两行：user 帧 + control_response: {w:?}"
        );
        assert!(w[0].contains("hi [mobile]"));
        let resp: serde_json::Value = serde_json::from_str(&w[1]).unwrap();
        assert_eq!(resp["type"], "control_response");
        assert_eq!(resp["response"]["subtype"], "success");
        assert_eq!(resp["response"]["request_id"], "req-1");
        assert_eq!(resp["response"]["response"]["behavior"], "allow");
        assert_eq!(
            resp["response"]["response"]["updatedInput"],
            serde_json::json!({"command": "ls -la"}),
            "allow 必须原样回显 updatedInput（缺则工具永不执行）"
        );
        assert_eq!(resp["response"]["response"]["toolUseID"], "t1");
        assert!(
            kills
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&4242),
            "turn 结束后必须终止进程（本通道进程只为一次 turn 而活）"
        );
        assert!(!pending_registry().has(sid), "回合终结必须注销待答项");
    }

    /// **被拒 = 已拒绝，不是失败**（诚实口径）：deny 走 deny 分支（不带 updatedInput），
    /// 回合继续跑完，回执 ok 且 reason 如实写明「有 N 项被拒绝」
    #[tokio::test]
    async fn claude_turn_reports_denied_not_failed() {
        let sid = "sess_c3_deny";
        let rig = script_io(&[control_request_frame("req-2")]);
        let written = rig.written.clone();
        let kills = rig.kills.clone();
        let lines = rig.lines.clone();
        let input = turn_input(sid, 5_000);
        let kf = kill_fn(kills);
        let io = rig.io;
        let driver = async {
            wait_pending(sid).await;
            pending_registry()
                .deliver(sid, "req-2", Decision::Deny)
                .unwrap();
            wait_written(&written, 2).await; // 决策已被消费（deny 已写 stdin）
            lines
                .send(assistant_frame("好的，我不执行这条命令"))
                .unwrap();
            lines.send(message_delta_frame("end_turn", 7)).unwrap();
        };
        let (out, ()) = tokio::join!(run_claude_turn(io, input, &kf), driver);
        assert_eq!(out.receipt.status, ReceiptStatus::Ok);
        assert_eq!(out.receipt.stage, None, "被拒不是失败阶段");
        assert_eq!((out.allowed, out.denied), (0, 1));
        let reason = out.receipt.reason.clone().unwrap_or_default();
        assert!(
            reason.contains("拒绝"),
            "回执必须如实报「已拒绝」：{reason}"
        );
        let w = written.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let resp: serde_json::Value = serde_json::from_str(&w[1]).unwrap();
        assert_eq!(resp["response"]["response"]["behavior"], "deny");
        assert!(resp["response"]["response"].get("updatedInput").is_none());
    }

    /// 问答往返（纯核）：AskUserQuestion → 移动端载荷带题面/选项/多选标记 →
    /// 提交已答全集 → `updatedInput.answers`（键 = 题面）
    #[tokio::test]
    async fn claude_turn_round_trips_an_ask_user_question() {
        let sid = "sess_c3_ask";
        let ask_input = serde_json::json!({"questions": [
            {"question": "选框架", "header": "框架", "multiSelect": true,
             "options": [{"label": "a", "description": "A 方案"}, {"label": "b"}]},
            {"question": "确认?", "options": [{"label": "y"}, {"label": "n"}]}
        ]});
        let frame = serde_json::json!({
            "type": "control_request", "request_id": "req-3",
            "request": {"subtype": "can_use_tool", "tool_name": "AskUserQuestion",
                        "input": ask_input, "tool_use_id": "t3"},
        })
        .to_string();
        let rig = script_io(&[frame]);
        let written = rig.written.clone();
        let lines = rig.lines.clone();
        let input = turn_input(sid, 5_000);
        let kf = kill_fn(rig.kills.clone());
        let io = rig.io;
        let driver = async {
            let p = wait_pending(sid).await;
            assert_eq!(p["kind"], "question");
            assert_eq!(p["toolName"], "AskUserQuestion");
            let qs = p["questions"].as_array().unwrap();
            assert_eq!(qs.len(), 2);
            assert_eq!(qs[0]["question"], "选框架");
            assert_eq!(qs[0]["multiSelect"], true);
            assert_eq!(qs[0]["options"][0]["description"], "A 方案");
            // 未答全 → 构造不出 AnswerSet（防静默丢题：类型层拒绝）
            let incomplete = AnswerSet::new(vec![
                Q::multi("选框架", vec!["a", "b"], vec![]),
                Q::single("确认?", vec!["y", "n"], "y"),
            ]);
            assert!(incomplete.is_err());
            let set = AnswerSet::new(vec![
                Q::multi("选框架", vec!["a", "b"], vec!["a"]),
                Q::single("确认?", vec!["y", "n"], "y"),
            ])
            .unwrap();
            pending_registry()
                .deliver(sid, "req-3", Decision::Answer(set))
                .unwrap();
            wait_written(&written, 2).await; // 答案已写 stdin（updatedInput.answers）
            lines.send(assistant_frame("已按你的选择继续")).unwrap();
            lines.send(message_delta_frame("end_turn", 5)).unwrap();
        };
        let (out, ()) = tokio::join!(run_claude_turn(io, input, &kf), driver);
        assert_eq!(out.answered, 1);
        assert_eq!(out.receipt.status, ReceiptStatus::Ok);
        let w = written.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let resp: serde_json::Value = serde_json::from_str(&w[1]).unwrap();
        let ui = &resp["response"]["response"]["updatedInput"];
        assert_eq!(ui["answers"]["选框架"], serde_json::json!(["a"]));
        assert_eq!(ui["answers"]["确认?"], "y");
        assert!(
            ui["questions"].is_array(),
            "原 input 必须原样保留（只加 answers 键）"
        );
    }

    /// **无人应答审批 → watchdog 超时**：回执如实 timeout（不谎报成功）、待答项注销、进程终止
    #[tokio::test]
    async fn claude_turn_times_out_honestly_when_approval_is_never_answered() {
        let sid = "sess_c3_timeout";
        let rig = script_io(&[control_request_frame("req-4")]);
        let kills = rig.kills.clone();
        let _exit_keep = rig.exit; // 保住退出通道：本测只让 watchdog 到点
        let input = turn_input(sid, 200);
        let kf = kill_fn(kills.clone());
        let out = run_claude_turn(rig.io, input, &kf).await;
        assert_eq!(out.receipt.status, ReceiptStatus::Failed);
        assert_eq!(out.receipt.stage, Some(Stage::Timeout));
        assert!(!out.turn_ended);
        assert!(
            out.receipt
                .reason
                .clone()
                .unwrap_or_default()
                .contains("审批"),
            "超时原因必须点明「在等审批应答」：{:?}",
            out.receipt.reason
        );
        assert!(
            !pending_registry().has(sid),
            "超时必须注销待答项（卡不能永远挂着）"
        );
        assert!(kills
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&4242));
    }

    /// **没有终结值就不是成功**（诚实纪律）：stdout 关闭（哪怕 result 帧在场）时回执必须是
    /// `failed(channel_error)`，绝不因为「进程退 0」或「result 帧在场」就报 ok
    #[tokio::test]
    async fn claude_turn_without_terminal_stop_reason_is_never_reported_as_success() {
        let sid = "sess_c3_no_end";
        let result_frame = r#"{"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","usage":{"output_tokens":109}}"#;
        let rig = script_io(&[assistant_frame("半截话"), result_frame.to_string()]);
        drop(rig.lines); // stdout 关闭（进程死了/管道断了）
        let kills = rig.kills.clone();
        let input = turn_input(sid, 5_000);
        let kf = kill_fn(kills.clone());
        let out = run_claude_turn(rig.io, input, &kf).await;
        assert_eq!(out.receipt.status, ReceiptStatus::Failed);
        assert_eq!(out.receipt.stage, Some(Stage::ChannelError));
        assert!(!out.turn_ended);
        let reason = out.receipt.reason.clone().unwrap_or_default();
        assert!(
            reason.contains("stop_reason") || reason.contains("终点"),
            "失败原因必须点明「未命中 turn 终点判据」：{reason}"
        );
        assert!(kills
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&4242));
    }

    /// 取消（生产 = `RunnerCfg::arm_cancel` 的观察口）：回执 cancelled + 进程终止 + 待答项注销
    #[tokio::test]
    async fn claude_turn_cancel_is_reported_as_cancelled() {
        let sid = "sess_c3_cancel";
        let rig = script_io(&[control_request_frame("req-5")]);
        let kills = rig.kills.clone();
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let _exit_keep = rig.exit;
        let mut input = turn_input(sid, 5_000);
        input.cancel = Some(cancel_rx);
        let kf = kill_fn(kills.clone());
        let io = rig.io;
        let driver = async {
            wait_pending(sid).await;
            let _ = cancel_tx.send(());
        };
        let (out, ()) = tokio::join!(run_claude_turn(io, input, &kf), driver);
        assert_eq!(out.receipt.status, ReceiptStatus::Cancelled);
        assert!(!pending_registry().has(sid));
        assert!(kills
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&4242));
    }

    /// **Task 13 复审 Important B**：同一会话**并发两条**控制请求时，用户眼前那张卡
    /// （先登记的第一项）**不得被顶掉**——它的决收端必须还在（那一答要真的送达，而不是
    /// 被回一句「未送达（取消/超时竞态）」），后到的请求**排队**等前一项答完再上卡（不丢）。
    #[tokio::test]
    async fn concurrent_control_requests_never_clobber_the_visible_card() {
        let sid = "sess_c3_two_controls";
        let rig = script_io(&[
            control_request_frame("req-1"),
            control_request_frame("req-2"),
        ]);
        let written = rig.written.clone();
        let lines = rig.lines.clone();
        let input = turn_input(sid, 5_000);
        let kf = kill_fn(rig.kills.clone());
        let io = rig.io;
        let driver = async {
            // 第一张卡 = req-1
            let p = wait_pending(sid).await;
            assert_eq!(p["requestId"], "req-1");
            // 给 req-2 充分时间被处理：若实现是「顶掉」，此刻卡上会变成 req-2
            for _ in 0..20 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let still = pending_registry().payload_of(sid).expect("卡必须还在");
            assert_eq!(
                still["requestId"], "req-1",
                "用户眼前那张卡（先到项）不得被后来的请求顶掉"
            );
            // 答第一项：**必须送达**（旧实现这里会永远「未送达」——rx 被顶掉了）
            pending_registry()
                .deliver(sid, "req-1", Decision::Allow)
                .expect("先到项的决策必须能送达");
            wait_written(&written, 2).await;
            // 第二项随即上卡（不丢请求）
            let p2 = wait_pending(sid).await;
            assert_eq!(p2["requestId"], "req-2", "排队的请求必须接着上卡");
            pending_registry()
                .deliver(sid, "req-2", Decision::Deny)
                .expect("第二项的决策也必须能送达");
            wait_written(&written, 3).await;
            lines.send(assistant_frame("两项都处理了")).unwrap();
            lines.send(message_delta_frame("end_turn", 11)).unwrap();
        };
        let (out, ()) = tokio::join!(run_claude_turn(io, input, &kf), driver);
        assert_eq!(out.receipt.status, ReceiptStatus::Ok, "{:?}", out.receipt);
        assert_eq!(
            (out.controls, out.allowed, out.denied, out.answered),
            (2, 1, 1, 0)
        );
        assert!(
            out.receipt
                .reason
                .clone()
                .unwrap_or_default()
                .contains("排队上卡"),
            "回执必须如实报「有一项排队上卡」：{:?}",
            out.receipt.reason
        );
        let w = written.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(w.len(), 3, "user 帧 + 两条 control_response: {w:?}");
        let r1: serde_json::Value = serde_json::from_str(&w[1]).unwrap();
        assert_eq!(r1["response"]["request_id"], "req-1");
        assert_eq!(r1["response"]["response"]["behavior"], "allow");
        assert_eq!(
            r1["response"]["response"]["updatedInput"]["command"],
            "ls -la"
        );
        let r2: serde_json::Value = serde_json::from_str(&w[2]).unwrap();
        assert_eq!(r2["response"]["request_id"], "req-2");
        assert_eq!(r2["response"]["response"]["behavior"], "deny");
        assert!(!pending_registry().has(sid));
    }

    // ===== 问答完整性（belt & braces 的核侧）=====

    /// 未答全**构造不出**提交载荷（类型层拒绝）+ 原因逐题点名；自由文本行算作答；空题集拒绝
    #[test]
    fn answer_set_refuses_incomplete_and_names_the_missing_questions() {
        let ok = AnswerSet::new(vec![
            Q::multi("选框架", vec!["a", "b"], vec!["a"]),
            Q::single("确认?", vec!["y", "n"], "y"),
        ]);
        assert!(ok.is_ok());
        let err = AnswerSet::new(vec![
            Q::multi("选框架", vec!["a", "b"], vec![]),
            Q::single("确认?", vec!["y", "n"], "y"),
        ])
        .unwrap_err();
        assert_eq!(err.missing, vec!["选框架".to_string()]);
        assert!(err.reason().contains("未答全"));
        assert!(err.reason().contains("选框架"));
        // Other 自由文本行 = 已作答（两侧空白 trim 后非空）
        let free = Q::single("确认?", vec!["y", "n"], "").with_free(" 走自定义 ");
        assert!(free.answered() && AnswerSet::new(vec![free]).is_ok());
        // 空题集不是「答全了」
        let empty = AnswerSet::new(vec![]).unwrap_err();
        assert!(empty.reason().contains("未答全"));
        // 多选 + 自由文本 = 数组（标签 + 自由文本）
        let answers =
            build_ask_answers(&[Q::multi("选框架", vec!["a", "b"], vec!["a"]).with_free("c")]);
        assert!(answers.contains(r#""选框架":["a","c"]"#), "{answers}");
    }

    /// 题集投影（附录 E-② 的收敛同形）：结构不符的题**跳过**（不猜、不造题）
    #[test]
    fn question_projection_skips_malformed_entries() {
        let input = serde_json::json!({"questions": [
            {"question": "选框架", "multiSelect": true, "options": [{"label": "a"}]},
            {"header": "没有题面", "options": [{"label": "x"}]},
            {"question": "确认?", "options": [{"label": "y"}, {"bad": 1}]}
        ]});
        let qs = project_questions(&input);
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].text, "选框架");
        assert!(qs[0].multi);
        assert_eq!(qs[1].options, vec![("y".to_string(), None)]);
        assert!(project_questions(&serde_json::json!({})).is_empty());
    }

    /// **上行答案 → AnswerSet**（核侧权威）：多选判定取登记题集（**不信任上行**）——
    /// 多选题即使只勾一项也必须编成数组；单选给多标签 / 题面不在题集 / 上行漏题
    /// 三类都如实拒绝（逐题点名，防静默丢题）
    #[test]
    fn answer_set_from_entries_uses_the_registered_questions_as_authority() {
        let registered = serde_json::json!([
            {"question": "选框架", "multiSelect": true,
             "options": [{"label": "a"}, {"label": "b"}]},
            {"question": "确认?", "multiSelect": false, "options": [{"label": "y"}, {"label": "n"}]}
        ]);
        let reg = registered.as_array().unwrap();
        // 多选题只勾一项：仍编成**数组**（附录 E-②）
        let set = answer_set_from_entries(
            reg,
            &[
                ("选框架".to_string(), vec!["a".to_string()]),
                ("确认?".to_string(), vec!["y".to_string()]),
            ],
        )
        .expect("答全即可构造");
        let answers = build_ask_answers(set.questions());
        assert!(answers.contains(r#""选框架":["a"]"#), "{answers}");
        assert!(answers.contains(r#""确认?":"y""#), "{answers}");
        // 单选给两个标签 → 拒绝（不静默截断）
        let e = answer_set_from_entries(
            reg,
            &[
                ("选框架".to_string(), vec!["a".to_string()]),
                ("确认?".to_string(), vec!["y".to_string(), "n".to_string()]),
            ],
        )
        .unwrap_err();
        assert!(e.reason.contains("单选题只允许一个答案"), "{e:?}");
        assert!(
            e.missing.is_empty(),
            "非「未答全」类拒绝不带 missing：{e:?}"
        );
        // 题面不在登记题集 → 拒绝（陈旧页面/串话不猜）
        let e = answer_set_from_entries(reg, &[("没这道题".to_string(), vec!["x".to_string()])])
            .unwrap_err();
        assert!(e.reason.contains("不在本回合的题集里"), "{e:?}");
        // 上行漏题（第二题整条缺失）→ 拒绝并点名（missing 供端点透出）
        let e = answer_set_from_entries(reg, &[("选框架".to_string(), vec!["a".to_string()])])
            .unwrap_err();
        assert!(
            e.reason.contains("未答全") && e.reason.contains("确认?"),
            "{e:?}"
        );
        assert_eq!(e.missing, vec!["确认?".to_string()], "{e:?}");
        // 空标签（用户没选）→ 同样按未答全拒绝
        let e = answer_set_from_entries(
            reg,
            &[
                ("选框架".to_string(), vec![]),
                ("确认?".to_string(), vec!["y".to_string()]),
            ],
        )
        .unwrap_err();
        assert!(e.reason.contains("选框架"), "{e:?}");
    }

    // ===== kimi / opencode 薄适配 =====

    /// kimi argv + 回执（**探针逐字帧**）：末条 assistant；tokens 如实 None（无 usage 帧）
    #[test]
    fn kimi_argv_and_receipt_follow_the_probe() {
        assert_eq!(
            kimi_argv("hi", "session_5e82f154-aee4-462b-befa-60580d138b19"),
            vec![
                "--session",
                "session_5e82f154-aee4-462b-befa-60580d138b19",
                "--prompt",
                "hi",
                "--output-format",
                "stream-json",
            ]
        );
        let stdout = vec![
            r#"{"role":"meta","type":"system.version","version":"2.1.1"}"#.to_string(),
            r#"{"role":"assistant","content":"Hi! How can I help you today?"}"#.to_string(),
            r#"{"role":"meta","type":"session.resume_hint","session_id":"session_5e82f154-aee4-462b-befa-60580d138b19","command":"kimi -r session_x","content":"To resume this session: kimi -r session_x"}"#.to_string(),
        ];
        let r = parse_kimi_receipt(&stdout);
        assert_eq!(
            r.last_assistant.as_deref(),
            Some("Hi! How can I help you today?")
        );
        assert_eq!(
            r.tokens, None,
            "kimi 2.1.1 stream-json 无 usage 帧（实测）——不猜"
        );
        assert_eq!(r.error, None);
        assert_eq!(r.frames, 3);
        // 噪音行不产生回执（不猜）
        assert_eq!(
            parse_kimi_receipt(&["noise".to_string()]),
            CliReceipt::default()
        );
    }

    /// opencode argv + 回执：**探针逐字的错误回合**（provider 410）如实报错；
    /// 成功帧形状取自本机二进制分派代码（未实机取证——登记在 [`opencode_argv`] 文档）
    #[test]
    fn opencode_argv_and_receipt_follow_the_probe() {
        assert_eq!(
            opencode_argv("hi", "ses_f71b120deffer3hV8kaMHyT5gD"),
            vec![
                "run",
                "--session",
                "ses_f71b120deffer3hV8kaMHyT5gD",
                "--format",
                "json",
                "hi",
            ]
        );
        // 探针逐字（provider 410）：错误帧 → error 原文，绝不折算成成功
        let probe = vec![
            r#"{"type":"step_start","timestamp":1791168863214,"sessionID":"ses_f71b120deffer3hV8kaMHyT5gD","part":{"type":"step-start"}}"#.to_string(),
            r#"{"type":"error","timestamp":1791168863491,"sessionID":"ses_f71b120deffer3hV8kaMHyT5gD","error":{"type":"provider.invalid-request","message":"deepseek-v4-flash:0731 was retired at 2026-09-25 00:00:00 -0700 PDT","status":410}}"#.to_string(),
        ];
        let r = parse_opencode_receipt(&probe);
        assert!(r.error.clone().unwrap().contains("was retired"));
        assert_eq!(r.last_assistant, None);
        assert_eq!(r.frames, 2);
        // 成功帧（二进制分派词汇：D("text") / D("step_finish")）
        let ok = vec![
            r#"{"type":"text","timestamp":1,"sessionID":"ses_x","part":{"type":"text","text":"你好"}}"#.to_string(),
            r#"{"type":"step_finish","timestamp":2,"sessionID":"ses_x","part":{"type":"step-finish","reason":"stop","tokens":{"input":10,"output":7,"reasoning":0,"cache":{"read":0,"write":0}}}}"#.to_string(),
        ];
        let r = parse_opencode_receipt(&ok);
        assert_eq!(r.last_assistant.as_deref(), Some("你好"));
        assert_eq!(r.tokens, Some(7));
        assert_eq!(r.error, None);
    }

    /// 一次性回合回执收尾的**诚实三态**（单一出口）：CLI 报错 → 失败带原文；
    /// 有回复 → ok + 摘要 + tokens；exit 0 无回复帧 → 失败（不冒充成功）；
    /// 机器已判终态（queued/cancelled）**原样透传**，不覆盖
    #[test]
    fn oneshot_receipt_finish_is_honest_three_ways() {
        let err = CliReceipt {
            error: Some("boom".into()),
            ..Default::default()
        };
        let r = finish_oneshot(Receipt::ok("s", 100), &err, "s", "", Some(0), &[]);
        assert_eq!(r.status, ReceiptStatus::Failed);
        assert_eq!(r.stage, Some(Stage::ChannelError));
        assert!(r.reason.clone().unwrap().contains("boom"));
        assert_eq!(r.duration_ms, 100, "耗时沿用 runner 读数");

        let good = CliReceipt {
            last_assistant: Some("你好".into()),
            tokens: Some(7),
            error: None,
            frames: 2,
        };
        let r = finish_oneshot(Receipt::ok("s", 100), &good, "s", "", Some(0), &[]);
        assert_eq!(r.status, ReceiptStatus::Ok);
        assert_eq!(r.last_assistant.as_deref(), Some("你好"));
        assert_eq!(r.tokens, Some(7));

        let r = finish_oneshot(
            Receipt::ok("s", 100),
            &CliReceipt::default(),
            "s",
            "stderr 尾巴",
            Some(0),
            &["noise".into()],
        );
        assert_eq!(r.status, ReceiptStatus::Failed);
        assert_eq!(r.stage, Some(Stage::ChannelError));
        assert!(r.reason.clone().unwrap().contains("未拿到回执"));

        let q = finish_oneshot(
            Receipt::queued(2).with_session("s"),
            &good,
            "s",
            "",
            None,
            &[],
        );
        assert_eq!(q.status, ReceiptStatus::Queued);
        assert_eq!(q.queue_position(), Some(2));
    }

    /// **Task 13 复审 Important A**：watchdog 杀掉的回合必须**保住 `timeout` 阶段码与措辞**——
    /// 既不得降级成 `channel_error`，也不得说出「CLI 正常退出」（被杀 ≠ 正常退出）。
    /// 同规覆盖 spawn / crash 两类机器判定失败；且即便 CLI 在死前吐过错误帧，
    /// 也只**追加**证据、不改 stage（错误帧不能顶掉机器结论）。
    #[test]
    fn oneshot_never_rewrites_a_machine_judged_terminal_stage() {
        // ① watchdog 到点（runner 的原始回执逐字取自 runner.rs 的 TimedOut 臂）
        let killed = Receipt::failed(
            Stage::Timeout,
            "watchdog 600s 到点，已 kill 进程树 = job_object（可重试）",
        )
        .with_session("s")
        .with_duration_ms(600_000);
        let r = finish_oneshot(
            killed.clone(),
            &CliReceipt {
                last_assistant: Some("半截话".into()),
                tokens: Some(3),
                error: None,
                frames: 1,
            },
            "s",
            "stderr 尾",
            None, // 被杀进程没有自然退出码
            &["partial".into()],
        );
        assert_eq!(r.status, ReceiptStatus::Failed);
        assert_eq!(r.stage, Some(Stage::Timeout), "timeout 必须仍是 timeout");
        let reason = r.reason.clone().unwrap_or_default();
        assert!(reason.contains("watchdog 600s 到点"), "{reason}");
        assert!(
            !reason.contains("正常退出"),
            "被 watchdog 杀掉不得说成「正常退出」：{reason}"
        );
        assert!(reason.contains("stderr 尾"), "诊断证据须追加：{reason}");
        assert!(
            !reason.contains("exit="),
            "无自然退出码时不得编造 exit：{reason}"
        );
        assert_eq!(r.duration_ms, 600_000, "耗时沿用 runner 读数");
        assert_eq!(r.last_assistant, None, "失败回执不带「末条回复」");
        assert_eq!(r.tokens, None);

        // ② 即便 CLI 在死前吐过错误帧，也只追加证据、不顶掉机器 stage
        let with_err = finish_oneshot(
            killed,
            &CliReceipt {
                error: Some("provider 410".into()),
                ..Default::default()
            },
            "s",
            "",
            None,
            &[],
        );
        assert_eq!(with_err.stage, Some(Stage::Timeout));
        let reason = with_err.reason.clone().unwrap_or_default();
        assert!(
            reason.contains("watchdog") && reason.contains("provider 410"),
            "{reason}"
        );

        // ③ spawn / crash 同规
        for stage in [Stage::Spawn, Stage::Crash, Stage::WorkspaceBusy] {
            let r = finish_oneshot(
                Receipt::failed(stage, "机器判定原因").with_session("s"),
                &CliReceipt {
                    error: Some("boom".into()),
                    ..Default::default()
                },
                "s",
                "",
                Some(1),
                &[],
            );
            assert_eq!(r.stage, Some(stage), "{stage:?} 不得被改写");
            let reason = r.reason.clone().unwrap_or_default();
            assert!(reason.starts_with("机器判定原因"), "{reason}");
            assert!(
                reason.contains("boom") && reason.contains("exit=1"),
                "{reason}"
            );
        }
    }

    /// **测试构建绝不解析真 PATH**（宪法级测试纪律）：本机真装了三家 CLI，
    /// 若单测也去扫 PATH，端点用例就会真的 spawn 一个真实回合（真实账号配额 + 真实会话库）
    #[test]
    fn production_cli_discovery_is_never_reachable_from_tests() {
        for tool in [CLAUDE_PROGRAM, KIMI_PROGRAM, OPENCODE_PROGRAM] {
            assert!(
                production_cli_spec(tool, "windows").is_none(),
                "测试构建必须恒「CLI 不可达」（{tool}）"
            );
            assert!(production_cli_spec(tool, "macos").is_none());
        }
    }

    /// 续接 cwd 门：空路径 / 目录不在场 → 如实拒绝；真目录 → 放行（**不假设更多**）
    #[test]
    fn cwd_gate_refuses_unknown_or_missing_working_directories() {
        assert!(cwd_gate("   ").unwrap().contains("工作目录未知"));
        assert!(cwd_gate("E:/definitely-not-here-mam-c3").is_some());
        let tmp = std::env::temp_dir();
        assert!(
            cwd_gate(&tmp.to_string_lossy()).is_none(),
            "在场目录必须放行（tempdir: {tmp:?}）"
        );
    }

    /// **跨语言夹具锁**（`tests/fixtures/headless_decision_words.json` 是唯一名单）：
    /// 决策词 / 待答种类两侧各自对照同一夹具断言（与 `headless_stages.json` 同款做法）——
    /// 任一侧新增词而另一侧没跟上，必有一侧先红
    #[test]
    fn decision_words_are_pinned_by_the_cross_language_fixture() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("tests")
            .join("fixtures")
            .join("headless_decision_words.json");
        let raw = std::fs::read_to_string(&path)
            .expect("跨语言夹具必须存在（tests/fixtures/headless_decision_words.json）");
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let mut mine = vec![
            Decision::Allow.wire().to_string(),
            Decision::Deny.wire().to_string(),
            Decision::Answer(AnswerSet::new(vec![Q::single("q", vec!["y"], "y")]).unwrap())
                .wire()
                .to_string(),
        ];
        mine.sort();
        let mut want: Vec<String> = v["decisions"]
            .as_array()
            .expect("夹具必须有 decisions 数组")
            .iter()
            .map(|x| x.as_str().unwrap().to_string())
            .collect();
        want.sort();
        assert_eq!(mine, want, "决策词表必须与跨语言夹具逐项一致");

        let mut kinds = vec![
            PendingKind::Approval.wire().to_string(),
            PendingKind::Question.wire().to_string(),
        ];
        kinds.sort();
        let mut wk: Vec<String> = v["kinds"]
            .as_array()
            .expect("夹具必须有 kinds 数组")
            .iter()
            .map(|x| x.as_str().unwrap().to_string())
            .collect();
        wk.sort();
        assert_eq!(kinds, wk, "待答种类词表必须与跨语言夹具逐项一致");

        // 审批卡选项 id 只能来自决策词单点（前端按钮 id 与应答端点入参同源）
        let ids: Vec<&str> = approval_options().into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec![Decision::Allow.wire(), Decision::Deny.wire()]);
        assert_eq!(Decision::from_wire("allow"), Some(Decision::Allow));
        assert_eq!(Decision::from_wire("deny"), Some(Decision::Deny));
        assert_eq!(
            Decision::from_wire("answer"),
            None,
            "answer 需答案载荷，不从此口反解"
        );
        assert_eq!(Decision::from_wire("whatever"), None);
    }

    /// 待答登记表语义：同会话不覆盖 / 请求标识必须相符 / 送达即移除 / 空槽如实报未送达
    #[test]
    fn pending_registry_refuses_overwrite_and_mismatched_request_ids() {
        let sid = "sess_c3_registry";
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Decision>();
        let p = PendingRequest {
            request_id: "r1".into(),
            kind: PendingKind::Approval,
            tool_name: "Bash".into(),
            input_display: "ls".into(),
            questions: Vec::new(),
            session_id: sid.into(),
            channel: "headless_claude_p",
            tier: "stdio",
            permission_mode: "default",
            requested_at: Instant::now(),
            tx,
        };
        assert!(pending_registry().register(p).is_ok());
        let (tx2, _rx2) = tokio::sync::mpsc::unbounded_channel::<Decision>();
        let dup = PendingRequest {
            request_id: "r2".into(),
            kind: PendingKind::Approval,
            tool_name: "Write".into(),
            input_display: "{}".into(),
            questions: Vec::new(),
            session_id: sid.into(),
            channel: "headless_claude_p",
            tier: "stdio",
            permission_mode: "default",
            requested_at: Instant::now(),
            tx: tx2,
        };
        // 同会话第二次登记：**如实拒绝并把请求退回**（退回物连着它自己的决策接收端——
        // 回合侧据此排队而不是丢请求；Task 13 复审 Important B 的接口保证）
        let back = pending_registry()
            .register(dup)
            .expect_err("同会话不得覆盖待答项");
        assert_eq!(back.request_id, "r2", "退回的必须是被拒的那一项");
        assert_eq!(back.session_id, sid);
        let payload = pending_registry().payload_of(sid).unwrap();
        assert_eq!(payload["requestId"], "r1", "被拒的登记不得顶掉原待答项");
        assert_eq!(payload["input"], "ls");
        let e = pending_registry()
            .deliver(sid, "r9", Decision::Allow)
            .unwrap_err();
        assert!(e.contains("标识不符"), "{e}");
        assert!(pending_registry()
            .deliver(sid, "r1", Decision::Allow)
            .is_ok());
        assert_eq!(rx.try_recv().unwrap(), Decision::Allow, "决策必须交给回合");
        assert!(!pending_registry().has(sid), "送达即移除");
        let e = pending_registry()
            .deliver(sid, "r1", Decision::Allow)
            .unwrap_err();
        assert!(e.contains("没有待答"), "{e}");
    }

    /// 取消/超时的**未送达**语义（应答端点的诚实措辞来源）
    #[test]
    fn deliver_reports_undelivered_when_the_turn_stopped_waiting() {
        let sid = "sess_c3_dropped";
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Decision>();
        drop(rx); // 回合侧已不再等待（取消/超时竞态）
        let p = PendingRequest {
            request_id: "r1".into(),
            kind: PendingKind::Approval,
            tool_name: "Bash".into(),
            input_display: "ls".into(),
            questions: Vec::new(),
            session_id: sid.into(),
            channel: "headless_claude_p",
            tier: "stdio",
            permission_mode: "default",
            requested_at: Instant::now(),
            tx,
        };
        assert!(pending_registry().register(p).is_ok());
        let e = pending_registry()
            .deliver(sid, "r1", Decision::Deny)
            .unwrap_err();
        assert!(e.contains("未送达"), "如实报未送达，不谎报已答：{e}");
    }
}
