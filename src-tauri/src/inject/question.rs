//! AskUserQuestion 问答卡片内核（批次乙 T8，claude 先行）：questions 解析 +
//! 应答注入序列构造（纯函数，跨平台可测；执行一律经 [`crate::inject::engine::Injector`]
//! 缝——`locate_and_send_key_spec` 按族分发，与审批端点同一装配）。
//!
//! # 实机探测定案（2026-09-21，本文件序列构造的唯一依据）
//!
//! 档案：`research/refs/phase2-消息注入/2026-09-21-claude-askuserquestion-按键语义
//! 探测.md`（实机三重证据：注入日志 + 截图 + 会话 JSONL）。键位语义：
//! - **单选 · 数字**：数字键（字符或 VK 形态均可）直接勾选并提交，**无需回车**，
//!   **严禁后补 Esc**（K1/K2——后补 Esc 落入模型 busy 回合 = 中断模型回合）；
//! - **单选 · Esc**：取消/拒绝整个问题（模型收 is_error=true 拒绝回执，K3）
//!   ——对应「取消」按钮；
//! - **多选 · 数字**：逐个**切换勾选**，不提交（K8）；
//! - **多选 · Enter**：**不是提交**——是切换光标所在行（K9，实测抓获的反直觉点），
//!   因此提交绝不能只发 Enter；
//! - **多选 · 提交**：**三段式**（K10）——VT 下箭头 ×(选项数+1)（从首选项行跨过
//!   TUI 自动追加的 "N. Type something." 行到 Submit 行）→ VK 回车进 Review 确认屏
//!   → 数字 '1' 确认；
//! - **自由文本**：UI 自动追加 "N. Type something." 行，先注入该行数字（仅定位，
//!   K4）→ 文本字符 → VK 回车（K6）；未定位直接打字无效（K7）。**卡片 v1 不做
//!   自由文本注入，引导普通消息发送**（前端文案，本文件不承载）；
//! - **多问题数组**（questions.length>1）：翻页键序（←→）**未测**——v1 只读展示+
//!   引导终端作答，端点对 questions != 1 的注入请求直接拒绝（不超证据出手）；
//! - **空闲对照**：问题未出现时数字只是进输入行（K11）——注入前必须确认问答在场
//!   （标记/通道 B 判定，见 remote/api.rs）。
//!
//! 注意：questions 载荷里只有模型给的 options——"Type something." 行是 TUI 渲染时
//! 自动追加的，**不在** questions JSON 中（探测档案 §2 UI 结构备注），故本模块的
//! 选项数 n 即纯模型选项，三段式下箭头数 = n+1 恰好跨到 Submit 行。
//!
//! # 丁T5：提交与自由作答改为**阶段机闭环**（§2.3 裁4 / §2.4 裁3）
//!
//! 批次丙的 [`answer_key_sequence`] 是「一次算步进 + 盲发序列」：多选提交直接把
//! `down ×(n+1) → enter → '1'` 投完，**中途没有任何屏读复核**——若第一条 down 被
//! 吞（或 TUI 行序与我们的假设不一致），后面的 `enter` 会落在**别的行**上，而 `'1'`
//! 更是在**没确认 Review 屏在场**的情况下发出（正是 §2.3 点名禁止的形态）。
//! 故本模块新增两条**闭环编排**（[`run_submit_stages`] / [`run_free_text_stages`]），
//! 读屏与发键全部经 [`MenuTerminal`] 注入——控制流因此可在门禁内用脚本化屏序列
//! 覆盖（同 [`crate::inject::mode::run_menu_stages`] 的做法与理由）。
//!
//! 两条编排的**判据来源**（真机取证，勿凭源码猜测）：
//!
//! | 判据 | 实测原文 | 证据 |
//! |---|---|---|
//! | 选项/提交行**焦点标记** | `❯`（U+276F，`pointer`） | `C-s8-cursor-submit-*.png`（光标在 Submit 行）+ claude 二进制 `pointer:"\u276F"` 与 `isFocused → L.pointer` 渲染分支 |
//! | Submit 行文本 | `Submit`（多选；`submitButtonText` 单题形态） | `C-s8-cursor-submit-*.png` + 二进制 |
//! | Review 屏标题 | `Review your answers` | `C-s8-submitted-*.png` + 二进制 `title:"Review your answers"` |
//! | Review 屏副题 | `Ready to submit your answers?` | 同上 |
//! | Review 屏选项 | `1. Submit answers` / `2. Cancel` | 同上（`confirmLabel:"Submit answers"`） |
//! | 完成态 | 对话流出现 `User answered Claude's questions:` | `C-s8-final-*.png` + 二进制 |
//! | 自由作答行 | `N. Type something.`（单选）/`N. Type something`（多选） | `C-s7-digit3-result-*.png` + 二进制 `Sa.multiSelect?"Type something":"Type something."` |
//!
//! 截图证据目录：`%TEMP%\mam-probe-askq-20260921-013027\evidence\`（本批已逐张核对）。

use crate::inject::families::FamilySpec;
use crate::inject::mode::MenuTerminal;

/// 单个选项（questions[].options[] 条目）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

/// 单个问题（questions[] 条目；multiSelect 缺省 = false——探测档案单选夹具实测形态）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub header: String,
    pub question: String,
    pub multi_select: bool,
    pub options: Vec<QuestionOption>,
}

/// 应答动作（POST /session-question/answer 的 action 域）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerAction {
    /// 单选点选项：注入对应数字（直接勾选并提交，无回车无 Esc）
    Select,
    /// 多选点选项：注入对应数字切换勾选（不提交）
    Toggle,
    /// 多选提交：三段式（down ×(n+1) → enter → '1'）——**丁T5 起走阶段机**
    /// [`run_submit_stages`]，本枚举值只作 wire 词与审计标签用
    Submit,
    /// 取消：Esc（模型收 is_error=true 拒绝回执）
    Cancel,
    /// 自由作答（丁T5 §2.4 入口 1）：数字定位 `Type something` 行 → 文本字符 → 回车
    /// ——**走阶段机** [`run_free_text_stages`]（键序依赖屏读定位，不产静态序列）
    FreeText,
    /// 多题**切换题目**（2026-09-23 错位修复）：opencode 页签式多题的 tab 前向切页
    /// （戊探A ①定案：题目页→下一题/Confirm 页，前向循环回绕）。**仅 opencode 放行**
    /// ——kimi/codex 的多题切页键未实测（「未验不出键」）。与 `Select` 的区别：
    /// Select 是「作答本题」（终端可能自动推进），Advance 是**纯导航**（不改任何
    /// 勾选/作答态）——这正是错位修复的语义切分点：多选题点选只 toggle 不推进，
    /// 推进只由本动作显式触发。
    Advance,
}

impl AnswerAction {
    /// wire 字符串 ↔ 动作（camelCase 请求体用小写词）
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "select" => Some(Self::Select),
            "toggle" => Some(Self::Toggle),
            "submit" => Some(Self::Submit),
            "cancel" => Some(Self::Cancel),
            "freeText" => Some(Self::FreeText),
            "advance" => Some(Self::Advance),
            _ => None,
        }
    }

    /// 审计摘要形态（action#index，编号对齐 UI 从 1 起）
    ///
    /// **自由作答只记动作名，不记正文**——正文是用户答案，审计表存摘要的既有口径
    /// （W5「只存摘要不入全文」）在此更严一档：连截断后的正文都不落，避免把用户
    /// 回答内容二次写进审计库（终端已收到，审计只需证明「发生过一次自由作答」）。
    pub fn audit_label(self, index: Option<usize>) -> String {
        match (self, index) {
            (Self::Select, Some(i)) => format!("select#{}", i + 1),
            (Self::Toggle, Some(i)) => format!("toggle#{}", i + 1),
            (Self::Submit, _) => "submit".to_string(),
            (Self::Cancel, _) => "cancel".to_string(),
            (Self::FreeText, _) => "freeText".to_string(),
            (Self::Advance, _) => "advance".to_string(),
            // select/toggle 缺 index 在端点参数校验已拦（400），防御形态原样
            (Self::Select, None) => "select".to_string(),
            (Self::Toggle, None) => "toggle".to_string(),
        }
    }
}

/// 解析 tool_input 原文 JSON（helper 问答通道携带 / 通道 B 的 tool_args）中的
/// questions[]。结构不符（缺 questions 数组 / 空 / 条目缺 question 或 options）→
/// None（端点据此回落通道 B 或判不可用）。
/// 宽容口径：header 缺省空串、多选标志缺省 false、description 缺省空串——与探测
/// 档案单选夹具的缺省形态一致。**多选标志双字段兼容**：claude `multiSelect` /
/// opencode `multiple`（bool；戊探A ⑥，见解析处注释）。
pub fn parse_questions(tool_input_json: &str) -> Option<Vec<Question>> {
    let v: serde_json::Value = serde_json::from_str(tool_input_json).ok()?;
    let arr = v.get("questions")?.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(arr.len());
    for q in arr {
        // question 与 options 是出卡的最小集（缺一即结构不符）
        q.get("question")?.as_str()?;
        let opts = q.get("options")?.as_array()?;
        if opts.is_empty() {
            return None;
        }
        let mut options = Vec::with_capacity(opts.len());
        for o in opts {
            options.push(QuestionOption {
                label: o.get("label")?.as_str()?.to_string(),
                description: o
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        out.push(Question {
            header: q
                .get("header")
                .and_then(|h| h.as_str())
                .unwrap_or_default()
                .to_string(),
            question: q
                .get("question")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            // 多选标志**按工具三字段兼容**：claude 口径 `multiSelect`；opencode 的
            // 字段名是 **`multiple`**（戊探A ⑥定案）；**kimi 是 `multi_select`
            // （下划线，2026-10-06 用户实机 wire 取证：4 题 tool-call args 前两题
            // `multi_select: true`）**——漏认会让 kimi 多选题整条链路恒显单选（前端
            // 发 select=数字+enter 替代 toggle=纯数字，enter 在多题流推进下一题 →
            // 手机端与终端割裂，16:05 用户实录，与 opencode 2026-09-23 事故同构）。
            // `multiple` 的**数组形态**（question_renamed 工具的选项列表，T3 口径外）
            // 经 as_bool() 恒 None，不会误判成标志。
            multi_select: q
                .get("multiSelect")
                .and_then(|m| m.as_bool())
                .or_else(|| q.get("multi_select").and_then(|m| m.as_bool()))
                .or_else(|| q.get("multiple").and_then(|m| m.as_bool()))
                .unwrap_or(false),
            options,
        });
    }
    Some(out)
}

/// 选项序号 → 数字键（UI 编号从 1 起；键域 = 单字符 '1'…'9'——经
/// `locate_and_send_key` 的单字符域分发，字符/VK 形态按族规格决定，探测 K1 实证
/// 两形态均直接提交）。9 选项以上越界 → None（探测档案：超范围数字未测，不出手）
fn digit_key(index: usize) -> Option<String> {
    if index >= 9 {
        return None;
    }
    Some((((index + 1) as u8 + b'0') as char).to_string())
}

/// 应答动作 → 按键序列（探测定案的纯函数化；键名走 `locate_and_send_key(_spec)`
/// 键域：单字符数字 / "enter" / "esc" / "down"——"down" 按族分发为 A 族 VT 序列
/// 或 B 族 VK 形态，探测 K10 实证 VT 形态对 claude 生效）。
///
/// **本函数是 claude 形态**（批次乙 T8 探测定案，K1–K11 实机取证）。跨工具序列见
/// [`answer_key_sequence_for`]——它按会话工具分发，未验工具降级为只读。
/// - Select/Toggle：`index` 越界（≥ 选项数）或 ≥9 → Err；
/// - Submit：仅多选合法（单选点数字即提交，无独立提交步）；序列 = down ×(选项数+1)
///   （跨过 TUI 自动追加的 "Type something." 行到 Submit 行，探测 K10）→ enter 进
///   Review 确认屏 → '1' 确认；
/// - Cancel：单键 esc。
pub fn answer_key_sequence(
    action: AnswerAction,
    index: Option<usize>,
    q: &Question,
) -> Result<Vec<String>, String> {
    match action {
        AnswerAction::Select => {
            let i = index.ok_or_else(|| "缺少选项序号".to_string())?;
            if i >= q.options.len() {
                return Err(format!("Q_INDEX_OUT_OF_RANGE: 选项序号越界：{i}"));
            }
            digit_key(i)
                .ok_or_else(|| format!("选项序号超键域：{i}"))
                .map(|k| vec![k])
        }
        // 2026-09-24：数字切勾路径**废止**——用户实机取证（claude 2.1.278）证明多选
        // 屏数字无反应（推翻 K8，档案见 2026-09-24-claude多选多题键序-用户实机取证
        // §5）。切勾一律走阶段机（空格 + 闭环导航 + 屏读校验翻转），此处显式 Err
        // 而非删分支：调用方若改回盲发数字会立刻撞上这条判据（同 Submit 分支的理由）
        AnswerAction::Toggle => Err(
            "多选切勾必须经阶段机（run_toggle_stages）——数字路径已废止（2026-09-24）".to_string(),
        ),
        AnswerAction::Submit => {
            if !q.multi_select {
                return Err("Q_SUBMIT_MULTI_ONLY: submit 仅用于多选题".to_string());
            }
            // **丁T5 起本分支不再产出键序**：多选提交改走**阶段机闭环**
            // （[`run_submit_stages`]——每段屏读复核后才推进）。保留一个显式 Err 而
            // 不是删掉分支，是为了让「谁再想盲发三段式」立刻撞上这条判据：调用方
            // 若改回 `answer_key_sequence` 会拿到可读的 Err，而不是悄悄发出六键。
            // 原实现（批次丙）的盲发序列 = down ×(n+1) → enter → '1'，其风险见模块
            // 文档「丁T5」小节（`'1'` 在没确认 Review 屏在场时发出）。
            Err("Q_STAGE_MACHINE_REQUIRED: 多选题提交必须经阶段机（run_submit_stages）——盲发序列已废弃".to_string())
        }
        AnswerAction::Cancel => Ok(vec!["esc".to_string()]),
        // 自由作答**不产键序**：它的键序依赖屏读（定位数字取自屏上编号），由
        // [`run_free_text_stages`] 在门禁可覆盖的编排里产生。此处显式 Err 而非
        // `unreachable!`：万一有调用方把它接到这里，必须拿到可读的拒绝。
        AnswerAction::FreeText => {
            Err("Q_STAGE_MACHINE_REQUIRED: 自由作答必须经阶段机（run_free_text_stages）——键序依赖屏读定位".to_string())
        }
        // claude 切题是**屏幕驱动**（走位到 Next 行 + 回车 + 读屏分类），无静态键序
        // ——显式 Err 防调用方盲发（同 Submit/FreeText 分支的理由）。2026-09-24 起
        // 走阶段机（run_advance_stages；用户实机取证 2.1.278：多题尾部 Next+回车）
        AnswerAction::Advance => {
            Err("Q_STAGE_MACHINE_REQUIRED: claude 多题切题必须经阶段机（run_advance_stages）".to_string())
        }
    }
}

/// 问答注入键序的**工具支持面**（批次丙 T3）：哪些工具已有实机取证的键序、哪些
/// 只能只读展示。**未验不出键**（探测纪律：结论不得超过证据）。
///
/// | 工具 | 键序证据 | 状态 |
/// |---|---|---|
/// | claude | 2026-09-21 按键语义探测 K1–K11（本机实机，三重证据） | 全支持（单/多选 + cancel） |
/// | opencode | 2026-09-20 跨工具矩阵 §2.2（本机实机 ×2：数字单键即选即交；↓+Enter） | 单选 select；多选/取消**未测** → 拒 |
/// | kimi | 同上 §2.3（本机实机 ×2：数字 → Review 屏 → 数字确认的**两段式**） | 单选 select 两段式；多选/取消未测 → 拒 |
/// | codex | **2026-09-21 实机补测**（T3 条件项，见下） | 单选 select 数字即选即交；多选 toggle/submit 拒；cancel=esc 中断回合（2026-10-18 放开） |
///
/// **codex 2026-09-21 实机补测（矩阵遗留条件项已补齐）**：档案
/// `research/refs/phase2-消息注入/2026-09-21-codex-request_user_input-键位实机补测.md`
/// （conhost + codex 0.155.1 + 三重证据）。定案：
///
/// - **数字单键即选即交**（'2'/'1' 三取样，注入→`function_call_output.answers` 落盘
///   ≈277–363ms）——与 claude K1 同形，故并入 [`QuestionKeyProfile::SingleDigitSubmit`]；
/// - ↓+Enter 亦可（移动高亮 + 提交）；
/// - 多选形态本轮未触发（弹窗均为单选）→ toggle/submit 拒绝（未验不出键）；
/// - 确认屏（未答完提交时弹）**数字无效**、只认 Enter/Esc——与源码级记载背离，
///   以实测为准（本档不出手该屏，仅记录）。
///
/// **cancel = esc 中断回合（2026-10-18 裁决放开，#80）**：codex 的 Esc 是「中断整个
/// 回合」（`function_call_output` 是纯字符串 "aborted by user…" + `turn_aborted` 事件，
/// **不是** claude 那种 is_error 拒答回执）——语义破坏性大于 claude K3（会打断模型
/// 正在做的事），故前端按钮文案前置警示「中断回合（破坏性）」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionKeyProfile {
    /// 全键序已验：单选直选、多选 toggle+三段式提交、esc 取消
    ClaudeFull,
    /// 单选已验（数字即选即交）；多选/提交未测 → 拒；cancel 例外（见各工具臂）
    SingleDigitSubmit,
    /// 单选两段式（数字选中 → Review 屏 → 数字确认）；多选未测
    TwoPhaseSelect,
    /// 未验：不出任何键（只读卡）
    ReadOnly,
}

/// 会话工具 → 问答键序档（纯函数，可测；未知工具保守走只读）
///
/// # 丁T2 待复核项：kimi **question 类**的数字通道可靠性（R1 Important-1）
///
/// **证据现状（如实申报）**：R1 的证伪（`'2'+Enter` 误批准、`'3'` 单键拒绝）针对的是
/// kimi 的 **`plan_review` 审批框**（`inject::dialog` 的导航确认族，approve 端点走
/// 「屏读 + ↓×k + Enter」，本批已按此实现）——**不是** question 类框。question 类的
/// 数字可靠性**没有**同等强度的实机证据（唯一记载是 2026-09-20 矩阵 §2.3 的双路径
/// 取样：数字 '2' → Review 屏 → '1' 提交，与 Enter 路径并列）。
///
/// **本轮处置：维持 [`QuestionKeyProfile::TwoPhaseSelect`]，不超证据改行为**——
/// 把 question 类一并改成导航确认会**改掉一个矩阵实测过并已上线的行为**，而改它的
/// 依据（plan_review 的证伪）属于**另一个对话框族**（两者渲染不同：审批框是 `▶ 1. …`
/// 三键单选，question 框是 `→ [1] …` 选项卡 + Review 确认屏）。以一族证据改另一族
/// 行为，正是「结论超过证据」的反面。
///
/// **残余风险与缓解**（不隐瞒）：若 kimi 的 question 类数字通道与 plan_review 同样
/// 不可靠，则 `[数字, "enter"]` 会答错选项（用户点「拒绝」类选项却选中高亮行）。
/// 缓解：① 两段式的第二段是 **Enter**（提交**当前高亮行**）——数字被吞时提交的是
/// 用户注入前的原位行，属**可见可复核**的偏差（对话框在注入后会重绘，用户能立刻看到
/// 结果）；② 本档只对**单选**放行（多选/取消已拒）；③ 下批实机探测（`#[ignore]` 占位
/// 见本模块 `live_probe_tests`）定案后按族收口。
///
/// **下批定案的两个分支**（探测后二选一，勿凭猜提前改）：
/// - 证实数字不可靠 → 与 approve 侧同族化：question 类也走「屏读高亮位 + ↓×k + Enter」
///   （`inject::dialog::navigation_sequence` 复用），并补反例夹具；
/// - 证实数字可靠（或仅 plan_review 特殊）→ 保持本档，把探测档案挂到本注释。
pub fn question_key_profile(tool: &str) -> QuestionKeyProfile {
    match tool {
        "claude" => QuestionKeyProfile::ClaudeFull,
        // 矩阵 §2.2 实测：VK 数字 '2' 单键即选中并提交；↓+Enter 亦可（同为直选即交）
        "opencode" => QuestionKeyProfile::SingleDigitSubmit,
        // 矩阵 §2.3 实测：数字选中 → 自动进 "Review your answer before submit"
        // 确认屏（[1] Submit / [2] Cancel）→ 数字 '1' 提交。**两段式**
        "kimi" => QuestionKeyProfile::TwoPhaseSelect,
        // T3 实机补测（2026-09-21）：数字单键即选即交（三取样），与 opencode 同档；
        // cancel = esc 中断回合（2026-10-18 放开，见上方表格注释）
        "codex" => QuestionKeyProfile::SingleDigitSubmit,
        _ => QuestionKeyProfile::ReadOnly,
    }
}

/// 跨工具应答序列（批次丙 T3 分发点；端点调用它而非直接调
/// [`answer_key_sequence`]）。
///
/// **kimi 两段式**（矩阵 §2.3 实机）：数字选中后 TUI 进 Review 确认屏，需再按 '1'
/// 才提交——故 select 序列是 `[数字, "1"]`（两键）。取消键 kimi 未测 → 走只读
/// （返回 Err，端点 409，前端只读卡）。
///
/// **codex ReadOnly**：任何动作都 Err（端点据此拒注入，前端只读卡）——「未验不出键」。
pub fn answer_key_sequence_for(
    tool: &str,
    action: AnswerAction,
    index: Option<usize>,
    q: &Question,
) -> Result<Vec<String>, String> {
    match question_key_profile(tool) {
        QuestionKeyProfile::ClaudeFull => answer_key_sequence(action, index, q),
        QuestionKeyProfile::SingleDigitSubmit => match action {
            // 矩阵 §2.2：数字单键即选即交（与 claude 同形）；多选 toggle 同键（若
            // 该工具支持多选，勾选语义未测——故 multi_select 的 toggle 也拒）
            // **批次戊 E6（戊探A 定案）**：opencode 多选题里数字=**toggle**（与单选
            // 「即交」语义不同）；单次切换不提交；codex 多选未取证维持拒绝
            AnswerAction::Select => {
                if q.multi_select {
                    if tool == "opencode" {
                        return answer_key_sequence(action, index, q);
                    }
                    return Err("Q_UNVERIFIED_CODEX: codex 多选未实测，不出键".to_string());
                }
                answer_key_sequence(action, index, q)
            }
            // E6：多选切勾 = 单次数字（char/VK 两形态皆 toggle ×3；空格两形态均无效
            // ——空格键永不入序）
            AnswerAction::Toggle if tool == "opencode" => {
                answer_key_sequence(AnswerAction::Select, index, q)
            }
            // **codex 多选切勾接线**（2026-10-05 探测批 C 定案 C2）：Space = 选中
            // 不前进（作用于高亮行；多选页无逐行标记，选中态走头部已答计数）。
            // 定位 = ↓×(index) 前向走位（假定高亮首行——弹窗常态；高亮被动过
            // 的场景边界申报：字符键在选项行惰性、无探针可用〔enter=提交〕，暂以
            // 诚实回执 KeySent 兜底）。数字在多选页=即答，**禁用于切勾**。
            AnswerAction::Toggle if tool == "codex" => {
                let i = index.ok_or_else(|| "缺少选项序号".to_string())?;
                if i >= q.options.len() {
                    return Err(format!("Q_INDEX_OUT_OF_RANGE: 选项序号越界：{i}"));
                }
                let mut seq: Vec<String> = (0..i).map(|_| "down".to_string()).collect();
                seq.push("space".to_string());
                Ok(seq)
            }
            AnswerAction::Toggle => Err("Q_UNVERIFIED_CODEX: codex 多选未实测，不出键".to_string()),
            // opencode 提交走阶段机（OpencodeSubmit：tab → Confirm 页 → enter）
            AnswerAction::Submit if tool == "opencode" => {
                Err("Q_STAGE_MACHINE_REQUIRED: opencode 多选提交必须经阶段机（run_opencode_submit_stages）".to_string())
            }
            AnswerAction::Submit => Err("Q_UNVERIFIED_CODEX: codex 多选提交未实测，不出键".to_string()),
            // E6：esc = 单次 dismiss（state.status=error + dismissed 落账，戊探A
            // E-A4）——与其他家「esc=中断回合」不同构
            AnswerAction::Cancel if tool == "opencode" => Ok(vec!["esc".to_string()]),
            // codex cancel = esc 中断整个回合（2026-10-18 裁决放开，#80）——破坏性：
            // function_call_output 为 "aborted by user…" + turn_aborted 事件，非
            // claude K3 的 is_error 拒答回执、亦非 opencode 的 dismiss。前端按钮
            // 文案如实标注「中断回合（破坏性）」（mobile.question.cancelInterrupt）
            AnswerAction::Cancel => Ok(vec!["esc".to_string()]),
            // opencode 自由作答走阶段机（own answer 开行守卫，E6）
            AnswerAction::FreeText if tool == "opencode" => {
                Err("Q_STAGE_MACHINE_REQUIRED: opencode 自由作答必须经阶段机（run_opencode_own_answer_stages）".to_string())
            }
            AnswerAction::FreeText => Err("Q_UNVERIFIED_OPENCODE: opencode 自由作答未实测，不出键".to_string()),
            // **切换题目**（2026-09-23 错位修复）：opencode 页签式多题 tab=前向切页
            // （戊探A ①：题目页→下一题/Confirm 页，前向循环回绕；单键、纯导航、
            // 不触碰勾选态）。仅 opencode——kimi/codex 切页键未验不出键
            AnswerAction::Advance if tool == "opencode" => Ok(vec!["tab".to_string()]),
            AnswerAction::Advance => Err(format!("Q_UNVERIFIED_ADVANCE: {tool} 多题切页键序未实测，不出键")),
        },
        QuestionKeyProfile::TwoPhaseSelect => match action {
            AnswerAction::Select => {
                if q.multi_select {
                    // E4：多选的「点选项」= **单次切勾**（数字），与 Toggle 同键——
                    // 前端多选卡逐项点按即逐项切勾（单次，无尾随键）
                    return kimi_toggle_keys(index, q);
                }
                // 两段式：数字**选中**（直达 Review 汇总屏）→ **Enter 确认**
                //
                // **2026-09-21 实机复验修正**（本轮补跑的 T5/T6 探测抓获）：
                // 原实现按 2026-09-20 矩阵记载发 `[数字, "1"]`（数字选中后按 '1'
                // 提交）——实机证明**第二个数字无效**，确认键是 **Enter**：
                // kimi 对话框底栏原文 `↑/↓ select · 1/2/3 choose · ↵ confirm`
                // （证据 `%TEMP%\mam-probe-c3-20260921-150000\evidence\
                // screen-t5-kimi-ready-before.txt` 行 26）。实测：注入 '1' 后对话框
                // **仍在**（高亮项不变），再注入 Enter 才关闭并推进回合
                // （screen-kimi-after-approve.txt → screen-kimi-after-enter-confirm.txt）。
                let i = index.ok_or_else(|| "缺少选项序号".to_string())?;
                if i >= q.options.len() {
                    return Err(format!("Q_INDEX_OUT_OF_RANGE: 选项序号越界：{i}"));
                }
                let digit = digit_key(i).ok_or_else(|| format!("选项序号超键域：{i}"))?;
                Ok(vec![digit, "enter".to_string()])
            }
            // E4：多选切勾 = **单次数字**（N7 修复——`[数字,回车]` 双切抵消净零，
            // 用户实测 KIMI3 缺陷形态；戊探B ×3 证数字单次切勾可靠）
            AnswerAction::Toggle => kimi_toggle_keys(index, q),
            // E4：多选提交走阶段机（run_kimi_submit_stages——tab → Review 汇总屏 →
            // 屏上编号确认），静态序列已废弃
            AnswerAction::Submit => Err(kimi_submit_static_keys_refused()),
            AnswerAction::Cancel => Err("Q_UNVERIFIED_KIMI: kimi 取消未实测，不出键".to_string()),
            // E4：自由作答走阶段机（run_kimi_free_text_stages——Other 行数字 →
            // 打字 → 回车保存 → Review → 确认），键序依赖屏读不产静态序列
            AnswerAction::FreeText => {
                Err("Q_STAGE_MACHINE_REQUIRED: kimi 自由作答必须经阶段机（run_kimi_free_text_stages）".to_string())
            }
            // kimi 多题的**切页键**未实测（K-5 定案的是数字直选自动推进——单选题
            // 不需要显式切页；多选题切页键无实机样本）→ 不出键，多选题切题引导终端。
            // **2026-10-06 放开（2.1.1 活体定案）**：题页 `→`/tab = 进 Review/下一题、
            // Review/题页 `←` = 返回修改/上一题（→ 已选行保留高亮、(✓) 撤销），键序
            // 走 KimiAdvance 臂（屏读到达验证 + 快照回执）——静态序列仍不出（方向
            // 参数化，唯一实现在臂内）
            #[allow(unused_variables)]
            AnswerAction::Advance => {
                Err("Q_STAGE_MACHINE_REQUIRED: kimi 切题走 KimiAdvance 阶段臂（静态序列不出）".to_string())
            }
        },
        QuestionKeyProfile::ReadOnly => Err(format!("Q_READONLY_PROFILE: {tool} 问答键序未实测，只读展示")),
    }
}

// ============================================================
// 批次戊 E4：kimi 问答全链（N7 双切抵消修复 + A3 多题禁尾 Enter + Other 自由作答）
// 定案输入 = 键序大词典 §3 问答节（清淤版）+ 戊探B + 用户 K-5 实录；spec §2
// ============================================================

/// kimi Review 汇总屏副题锚（K-5 实录 / 戊探B 实机逐字 `Ready to submit your answers?`；
/// 单题 Review 屏同样含此行——screen-ki1-b1-after-char3.txt 原件）。
///
/// **2026-09-23 起真源移入账本**（[`crate::inject::anchor_ledger`] 的
/// `kimi/question_review/title`）：上游改词时按账本格式**追加**一行即可
/// （append-only），不必改代码。取不到时回落已入账那一条（编译期常量兜底）。
pub fn kimi_review_summary_anchor() -> &'static str {
    crate::inject::anchor_ledger::candidates(
        "kimi",
        crate::inject::anchor_ledger::scenario::QUESTION_REVIEW,
        crate::inject::anchor_ledger::slot::TITLE,
    )
    .first()
    .map(|r| r.text)
    .unwrap_or("ready to submit your answers?")
}

/// kimi 确认后**终态锚**（transcript `● Collected your answers`，戊探B M1/M2 提交后
/// 屏读原件逐字；落账侧另有 wire `interaction.resolved` 对账）。
///
/// **2026-09-23 起真源移入账本**（`kimi/question/receipt`）；取不到时回落已入账
/// 那一条。
pub fn kimi_answered_anchor() -> &'static str {
    crate::inject::anchor_ledger::candidates(
        "kimi",
        crate::inject::anchor_ledger::scenario::QUESTION,
        crate::inject::anchor_ledger::slot::RECEIPT,
    )
    .first()
    .map(|r| r.text)
    .unwrap_or("collected your answers")
}
/// kimi Other（自由作答）行标签（`→ [5] Other` 行形；多选形态 Other 行无编号
/// `[/] Other`——故自由作答只对**单选**放行，多选到终端作答）。
pub const KIMI_OTHER_ROW_LABEL: &str = "other";

/// kimi Review 汇总屏**在场**判定（纯函数）：副题锚在场，且存在 `[N] <文字>` 形态的
/// 确认项行（`→ [1] Submit` / `[2] Cancel`——戊探B 原件行形）。
///
/// **为什么不用 claude 的 [`probe_review_screen`]**：claude 的确认项是 `1. Submit
/// answers`（编号点隔形态 + "submit answers" 词），kimi 是**方括号编号** `[1] Submit`
/// （无 "answers" 词）——判据同源不同形，各自锚定各家的真机原文。
pub fn kimi_review_present(lines: &[String]) -> bool {
    lines
        .iter()
        .any(|l| l.to_lowercase().contains(kimi_review_summary_anchor()))
        && kimi_review_confirm_digit(lines).is_some()
}

/// kimi Review 确认项的**确认键**（`→ [1] Submit` → `"1"`；戊探B：确认键 char '1'
/// ×3 / VK '1' ×2 / VK Enter ×2 全通——取编号键最稳）。读不到 → None（调用方中止）。
pub fn kimi_review_confirm_digit(lines: &[String]) -> Option<String> {
    lines
        .iter()
        .filter_map(|l| parse_kimi_bracket_row(l))
        .find(|(_, text)| text.to_lowercase().contains("submit"))
        .map(|(digit, _)| digit)
}

/// kimi **Other 行定位**（自由作答入口）：`[N] Other` 行形 → 定位数字键（如 `"5"`）。
/// 多选形态的 Other 行无编号（`[/] Other`）→ None（调用方如实拒绝：自由作答仅单选）。
pub fn locate_kimi_other_digit(lines: &[String]) -> Option<String> {
    // 形态①：显示编号 `[5] Other`（单选/单题页——戊探B E-B8）→ 直接取编号
    if let Some(d) = lines
        .iter()
        .filter_map(|l| parse_kimi_bracket_row(l))
        .find(|(_, text)| text.to_lowercase().trim().starts_with(KIMI_OTHER_ROW_LABEL))
        .map(|(digit, _)| digit)
    {
        return Some(d);
    }
    // 形态②：多选页 Other **无编号显示**（探测批 K2）→ **按 checkbox 计数位**
    // （K4 定案 + 2026-10-06 用户实测：Other 是第 5 个方括号 → 按数字 5 直达
    // 编辑态）。勾选字形三候选与快照解析同源（[?]/[✓]/[√] 已选、[ ] 未选）。
    const BOXED: &[&str] = &["[ ]", "[?]", "[✓]", "[√]"];
    let mut count = 0usize;
    for l in lines {
        let lower = l.to_lowercase();
        let text_after_box = BOXED.iter().find_map(|m| {
            lower
                .find(m)
                .and_then(|i| l.get(i + m.len()..).map(|rest| rest.trim().to_lowercase()))
        });
        if let Some(text) = text_after_box {
            count += 1;
            if text.starts_with(KIMI_OTHER_ROW_LABEL) {
                return digit_key(count - 1);
            }
        }
    }
    None
}

/// **kimi 通用有界轮询**（2026-10-07 简化收敛）：每拍 settle → read → 谓词判定，
/// 命中即停。窗尽返回最后一拍（None = 途中读不到屏）。生产步长 =
/// [`FreeTextTerminal::settle`]。
fn poll_beats<Rd, T>(
    read: &mut Rd,
    terminal: &mut T,
    rounds: usize,
    mut pred: impl FnMut(&[String]) -> bool,
) -> Option<Vec<String>>
where
    Rd: FnMut() -> Option<Vec<String>>,
    T: FreeTextTerminal,
{
    let mut last = None;
    for _ in 0..rounds.max(1) {
        terminal.settle();
        let l = read()?;
        if pred(&l) {
            return Some(l);
        }
        last = Some(l);
    }
    last
}

/// **等编辑态出现**（footer `type answer` 锚，有界窗）：true = 已进编辑。
fn wait_editor_mode<Rd, T>(read: &mut Rd, terminal: &mut T, rounds: usize) -> bool
where
    Rd: FnMut() -> Option<Vec<String>>,
    T: FreeTextTerminal,
{
    let out = poll_beats(read, terminal, rounds, |l| {
        let hit = kimi_editor_mode(l);
        if hit {
            // 逐拍取证（12:46 实录：6s 窗内每拍误判编辑态在场——命中行内容
            // 落日志即可定案是哪一行在污染判定）
            // 取证增强（15:17 定案用）：命中行的**屏内行号**（倒序 1=屏底）+
            // **屏幕最后 3 行**原文——区分「footer 真在场」（行号靠下+末行即
            // footer）与「历史残留/读取滞后」（行号靠上+末行已是 toggle footer）
            let total = l.len();
            match l
                .iter()
                .enumerate()
                .rfind(|(_, x)| x.to_lowercase().contains("type answer"))
            {
                Some((row, src)) => {
                    let tail: Vec<&str> = l.iter().rev().take(3).map(|x| x.trim()).collect();
                    log::info!(
                        "kimi 编辑态命中：行 {row}/{}（屏底倒数 {}）| {src:?} | 末3行={tail:?}",
                        total,
                        total - row
                    );
                }
                None => log::info!("kimi 编辑态判定：命中但全屏未见 type answer 行（异常形态）"),
            }
        }
        hit
    })
    .is_some();
    log::info!("kimi 编辑态等待窗尽：editor_still={out}");
    out
}

/// kimi 单选页**当前高亮行的编号**（2026-10-07：单选高亮 `→` 前缀字符层可读——
/// 多选页高亮属性层不可见，本判据只对单选生效）。无 `→` 编号行 → None。
pub fn locate_kimi_highlight_digit(lines: &[String]) -> Option<String> {
    let arrow = char::from_u32(0x2192).unwrap();
    lines
        .iter()
        .filter(|l| l.trim_start().starts_with(arrow))
        .filter_map(|l| parse_kimi_bracket_row(l))
        .next()
        .map(|(digit, _)| digit)
}

/// kimi 题屏 `? ` 题干行 → 载荷题序（**sheet 落点对位的单一判据**，01:48 定案：
/// `? ` 行给的是 question 文本，对位基准必须是 question 而非 header——拿 header
/// 比对永不相等）。归一（剥空白）后与载荷 question 全集比对，**唯一命中**才给
/// 题序；题干行缺席/空串/多义命中 → None（不猜纪律）。freeText 单选保存循环
/// （本文件）与 select 落点校验（remote::api）共用同一口径。
pub(crate) fn kimi_sheet_question_idx(
    lines: &[String],
    payload_questions: &[String],
) -> Option<usize> {
    let norm = |x: &str| x.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let hn = lines
        .iter()
        .find_map(|l| l.trim().strip_prefix("? ").map(norm))?;
    if hn.is_empty() {
        return None;
    }
    let hits: Vec<usize> = payload_questions
        .iter()
        .enumerate()
        .filter(|(_, q)| {
            let qn = norm(q);
            qn.len() >= 4 && (qn == hn || qn.contains(&hn) || hn.contains(&qn))
        })
        .map(|(i, _)| i)
        .collect();
    (hits.len() == 1).then_some(hits[0])
}

/// kimi 已选勾选字形三候选（账本吸收文案漂移的同一口径）：`[?]`=探测批 K（2.x）、
/// `[✓]`=戊探B（2.0.2 空格/回车切勾实录）、`[√]`=2.1.1 用户实机（2026-10-06 截图）。
const KIMI_CHECKED_GLYPHS: &[&str] = &["[?]", "[✓]", "[√]"];
/// kimi 方框字形全集（未选 + 三版本已选）——方框 Other 行扫描的标记集。
const KIMI_BOX_GLYPHS: &[&str] = &["[ ]", "[?]", "[✓]", "[√]"];

/// 倒序找**方框 Other 行**（形态②共用扫描核）→ `(该行小写全文, 方框标记后内容
/// （小写、去首空白）)`。倒序取**最后一个**（两页不同时在场，倒序是纵深防御）。
/// 返回行小写全文是为勾选判定保持**整行扫描**口径（与抽取前逐字等价）。
fn kimi_boxed_other_row(lines: &[String]) -> Option<(String, String)> {
    lines.iter().rev().find_map(|l| {
        let lower = l.to_lowercase();
        let rest = KIMI_BOX_GLYPHS.iter().find_map(|m| {
            lower
                .find(m)
                .and_then(|i| l.get(i + m.len()..).map(|r| r.trim().to_lowercase()))
        })?;
        rest.starts_with(KIMI_OTHER_ROW_LABEL)
            .then_some((lower, rest))
    })
}

/// kimi Other **编辑行残留长度**（覆盖写入的删除量判据，2026-10-06）：
/// 编辑态行形 `→ [4] Other: <残留>`（K7 定案：重进带旧文本）→ Some(残留字符数)；
/// 鲜态 `→ [4] Other:`（冒号后空白）→ Some(0)；编辑行不在场 → None（调用方如实中止）。
///
/// 「恰为 Other 行」判据与 [`locate_kimi_other_digit`] 同源（方括号行形 + other 开头），
/// 取**最后一个** Other 行（Review 页 Other 是答案回显、编辑页才是输入行——两页不同时
/// 在场，倒序取是纵深防御）。
pub fn locate_kimi_other_residue_len(lines: &[String]) -> Option<usize> {
    // 形态①：编号编辑行 `→ [4] Other: <残留>`（单选/单题）
    if let Some(n) = lines
        .iter()
        .rev()
        .filter_map(|l| parse_kimi_bracket_row(l))
        .find(|(_, text)| text.to_lowercase().trim().starts_with(KIMI_OTHER_ROW_LABEL))
        .map(|(_, text)| {
            text.split_once(':')
                .map(|(_, v)| v.trim().chars().count())
                .unwrap_or(0)
        })
    {
        return Some(n);
    }
    // 形态②：多选无编号编辑行 `[ ] Other: <残留>` / `[√] Other: <残留>`（2026-10-06
    // 姊妹断链修复：只认编号形态导致多选覆盖写入误报「读不到编辑行」）
    kimi_boxed_other_row(lines).map(|(_, rest)| {
        rest.split_once(':')
            .map(|(_, v)| v.trim().chars().count())
            .unwrap_or(0)
    })
}

/// kimi **编辑态**判据（footer 锚 `type answer ↵ save …`，K2 定案词形；倒序 6 行
/// 内找——footer 恒在屏底）：true = Other 编辑器开着（此时任何数字/回车都是文字
/// 或状态污染——「等」是唯一正确动作）。
pub fn kimi_editor_mode(lines: &[String]) -> bool {
    // **取最靠下（屏底方向）的 footer 行判状态**（2026-10-07 12:46 取证定稿）：
    // 两种 footer 都以 `esc cancel` 结尾（编辑态 `type answer ↵ save … esc
    // cancel` / 选项页 `… ↵ toggle ←/→/tab switch esc cancel`）——从屏底往上找
    // 第一条含 `esc cancel` 的行即当前 footer；残留的历史 footer 行必在其上方，
    // 不会被误读（12:46 实录：缓冲区下方 10 行空白 + 残留 type answer 行致
    // any() 判定恒为编辑态）。
    let footer = lines
        .iter()
        .rfind(|l| l.to_lowercase().contains("esc cancel"))
        .map(|l| l.to_lowercase());
    match footer {
        Some(f) => f.contains("type answer"),
        None => false, // footer 不在场（读取不完整/非问答页）→ 不判编辑态
    }
}

/// kimi Other 行**勾选态**（补勾补丁的判据，2026-10-06）：Some(已勾?) = 行在场；
/// None = 行不在场。字形三候选与快照同源（[`KIMI_CHECKED_GLYPHS`]）。
pub fn locate_kimi_other_checked(lines: &[String]) -> Option<bool> {
    kimi_boxed_other_row(lines)
        .map(|(lower, _)| KIMI_CHECKED_GLYPHS.iter().any(|m| lower.contains(m)))
}

/// kimi 终态在场（提交完成判据）
pub fn kimi_answered_present(lines: &[String]) -> bool {
    lines
        .iter()
        .any(|l| l.to_lowercase().contains(kimi_answered_anchor()))
}

/// kimi 方括号行形解析：`→ [1] Submit` / `  [2] Cancel` → `("1", "Submit")`。
/// 行首允许空白与 `→`/`❯` 引导符；编号必须在方括号内（1 位数字），后随空白与文字。
fn parse_kimi_bracket_row(line: &str) -> Option<(String, &str)> {
    let t = line
        .trim()
        .trim_start_matches(['\u{2192}', '\u{276f}'])
        .trim_start();
    if !t.starts_with('[') {
        return None;
    }
    let close = t.find(']')?;
    let num = &t[1..close];
    if num.len() != 1 || !num.chars().next()?.is_ascii_digit() {
        return None;
    }
    let text = t[close + 1..].trim();
    if text.is_empty() {
        return None;
    }
    Some((num.to_string(), text))
}

/// kimi **多选切勾键序**（N7 修复的核心锁）：切勾 = **单次数字**（char/VK 两形态皆可，
/// 戊探B ×3）——`[数字, 回车]` 序列=勾上又被回车切掉、净效果零（用户实测 KIMI3 缺陷
/// 形态），**禁止尾随回车**。
pub fn kimi_toggle_keys(index: Option<usize>, q: &Question) -> Result<Vec<String>, String> {
    let i = index.ok_or_else(|| "缺少选项序号".to_string())?;
    if i >= q.options.len() {
        return Err(format!("Q_INDEX_OUT_OF_RANGE: 选项序号越界：{i}"));
    }
    digit_key(i)
        .map(|k| vec![k])
        .ok_or_else(|| format!("选项序号超键域：{i}"))
}

/// kimi **多题 DigitAdvance 键序**（K-5 定案；A3 禁令的锁）：题屏数字=**直选+自动推进
/// 下一题**——序列就是 `[数字]`，**禁止尾随 Enter**（Enter 会误作用在下一题/Review 屏，
/// 矩阵 §6.6-A3）。单题与多题共用数字直选；差异只在「不补确认键」。
pub fn kimi_digit_advance_keys(index: Option<usize>, q: &Question) -> Result<Vec<String>, String> {
    // 键序与切勾同形（单次数字）；独立成函数是为了判据可读与各自演化（N7/A3 的
    // 锁分别钉在两处的测试上，共享实现不共享语义）
    kimi_toggle_keys(index, q)
}

/// kimi 多选**提交**的静态键序**已废弃**（走阶段机 [`run_kimi_submit_stages`]）——
/// 与 claude 的 [`answer_key_sequence`] Submit 分支同一纪律：谁再想盲发立刻撞上可读 Err。
pub fn kimi_submit_static_keys_refused() -> String {
    "kimi 多选提交必须经阶段机（run_kimi_submit_stages）——盲发序列已废弃".to_string()
}

/// kimi 多选/多题**提交阶段机**（批次戊 E4；读屏/发键/等待全走闭包——门禁内脚本化
/// 屏序列可覆盖，抽法理由同 [`run_submit_stages`]）。
///
/// # 各段与中止点（每步发键前屏读）
///
/// 1. **Review 已在场？**：多题流末题答完 TUI **自动**进 Review 汇总屏（K-5），多选
///    单题则需要 `tab` 一次直达（戊探B tab→Submit 定案）。先读一屏：已在场 → **跳过
///    tab**（在 Review 屏上再按 tab 会切页离开——危害防御）；不在场 → 发 `tab`；
/// 2. **Review 汇总屏段**：轮询 [`kimi_review_present`]（副题锚 + `[N]` 确认项）。
///    窗内未出现 → 中止，**不发确认键**（与 claude 的「未见 Review 不发数字」同一纪律）；
/// 3. **确认段**：确认键 = 屏上编号（[`kimi_review_confirm_digit`]，不硬编码）；
/// 4. **终态段**：轮询 [`kimi_answered_anchor`]——未见**不是失败**
///    （`receipt_seen = Some(false)`，调用方如实下发「请人工核对」；落账侧另有 wire
///    `interaction.resolved` 对账）。
pub fn run_kimi_submit_stages<Rd, P, Q, T>(
    mut read: Rd,
    mut poll_review: P,
    mut poll_receipt: Q,
    terminal: &mut T,
) -> Result<SubmitOutcome, StageAbort>
where
    Rd: FnMut() -> Option<Vec<String>>,
    P: FnMut() -> Result<Option<Vec<String>>, String>,
    Q: FnMut() -> Result<Option<Vec<String>>, String>,
    T: MenuTerminal,
{
    let mut sent_keys: Vec<String> = Vec::new();
    // 1. Review 已在场？（多题自动推进形态）——读不到屏 → 中止零按键
    let initial = read().ok_or_else(|| {
        StageAbort::screen(
            "INJECT_SCREEN_ABORT: kimi 提交：读不到屏幕——已中止，未发任何键；请人工核对终端",
        )
    })?;
    if !kimi_review_present(&initial) {
        terminal.send("tab").map_err(|e| {
            StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: tab 投递失败（{e}）"))
        })?;
        sent_keys.push("tab".to_string());
        terminal.settle();
    }
    // 2. Review 汇总屏（未见即中止，不发确认键）
    let review = poll_review().map_err(StageAbort::screen)?.ok_or_else(|| {
        StageAbort::screen(format!(
            "已发 tab 但屏上未出现 Review 汇总屏（未见「{}」）——已中止，未发确认键；请人工核对终端",
            kimi_review_summary_anchor()
        ))
    })?;
    if !kimi_review_present(&review) {
        return Err(StageAbort::screen(
            "Review 汇总屏形态不符（锚或确认项缺失）——已中止，未发确认键；请人工核对终端",
        ));
    }
    // 3. 确认键 = 屏上编号（`→ [1] Submit` → '1'）
    let confirm_key = kimi_review_confirm_digit(&review).ok_or_else(|| {
        StageAbort::screen(
            "Review 汇总屏在场但读不到确认项编号——已中止，未发确认键；请人工核对终端",
        )
    })?;
    terminal.send(&confirm_key).map_err(|e| {
        StageAbort::delivery(format!(
            "INJECT_DELIVERY_FAIL: 确认键 {confirm_key} 投递失败（{e}）"
        ))
    })?;
    sent_keys.push(confirm_key);
    terminal.settle();
    // 4. 终态（未见不是失败——语义与 run_submit_stages 同源）
    let receipt_seen = stage_receipt_seen(&mut poll_receipt, kimi_answered_present);
    Ok(SubmitOutcome {
        sent_keys,
        down_steps: 0,
        up_steps: 0,
        review_confirmed: true,
        receipt_seen,
    })
}

/// kimi **Other 自由作答阶段机**（批次戊 E4；`Other` 行数字 → 打字 → 回车保存 →
/// Review → 确认——戊探B E-B8 全链定案）。仅**单选**形态放行（多选 Other 行无编号，
/// [`locate_kimi_other_digit`] 恒 None → 第 1 段如实中止）。
#[allow(clippy::too_many_arguments)] // 缝参数（overwrite/multi_question/multi_select 各有语义）
pub fn run_kimi_free_text_stages<Rd, P, Q, T>(
    text: &str,
    overwrite: bool,
    multi_question: bool,
    // **保存后预期落点**（载荷 question 文本全集 + 预期题序 q_idx+1）——单选保存后
    // 按 `? ` 题干行对位 sheet 序并导航回预期题（header≠question 判据错位修复：
    // ? 行给的是 question 文本，拿 header 比对永不相等 → 导航打满 8 格回推两题）
    payload_questions: Vec<String>,
    expected_idx: Option<usize>,
    // 题形态（多选/单选）——补发回车只对多选生效（2026-10-06 用户裁决：多选
    // Other 保存后留在原页、多按回车无害且实测有效；单选保存后自动推进下一题，
    // 补发回车的落点随推进时机漂移 = 第 4 题被带选的事故形态 → 停用）
    multi_select: bool,
    mut read: Rd,
    mut poll_review: P,
    mut poll_receipt: Q,
    terminal: &mut T,
) -> Result<FreeTextOutcome, StageAbort>
where
    Rd: FnMut() -> Option<Vec<String>>,
    P: FnMut() -> Result<Option<Vec<String>>, String>,
    Q: FnMut() -> Result<Option<Vec<String>>, String>,
    T: FreeTextTerminal,
{
    // **覆盖写入的空文本前置拒**（2026-10-06 2.1.1 活体定案）：kimi Other 编辑器
    // 「空内容回车 = no-op」（不保存、编辑器原地不动）——置空语义不存在，如实拒绝
    // 引导终端操作，绝不盲发（退格清掉旧文后留在一个未定案的编辑器态）。
    // 空文本：非覆盖模式照旧拒；**覆盖模式 = 清空请求**（2026-10-06 用户定案：
    // 清空 = 进编辑退格到底真清除，不再前置拒——原「无法置空」裁决基于「空回车
    // no-op」，但退格到底后回车无害、文字已在编辑器内清掉）
    if text.trim().is_empty() && !overwrite {
        return Err(StageAbort::screen(
            "INJECT_SCREEN_ABORT: 自由作答文本为空——已中止，未发任何键",
        ));
    }
    let mut sent_keys: Vec<String> = Vec::new();
    // 1. Other 行定位（读不到/无编号 → 中止零按键）
    let first = read().ok_or_else(|| {
        StageAbort::screen(
            "INJECT_SCREEN_ABORT: kimi 自由作答：读不到屏幕——已中止，未发任何键；请人工核对终端",
        )
    })?;
    let other_digit = locate_kimi_other_digit(&first).ok_or_else(|| {
        StageAbort::screen(
            "屏读未定位到 Other 行（自由作答入口；多选题的 Other 无编号请到终端作答）——已中止，未发任何键",
        )
    })?;
    // **已在编辑态则跳过定位数字**（2026-10-06 用户实录：编辑态里发数字 = 文字
    // 污染——"Other: 期权4" 末尾的 4 即定位数字）。基准 = 定位读的那一拍。
    let already_editing = kimi_editor_mode(&first);
    if !already_editing {
        terminal.send(&other_digit).map_err(|e| {
            StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: Other 定位键投递失败（{e}）"))
        })?;
        sent_keys.push(other_digit.clone());
        terminal.settle();
    }
    // 1.5 **编辑态进入 + 试探退格**（2026-10-06 修订）：编辑态判定**footer 锚优先**
    // （数字定位后 footer `type answer` 在场即已进编辑——空行退格核验会失灵：长度
    // 0→0 不变被误判「试探未生效」，用户实录）。残留 >0 才做试探退格（文字变短 =
    // 编辑态二次确认）；残留 =0 直接继续（无文字可退，footer 已证编辑态）。
    if overwrite {
        let probe_state = |r: &mut Rd| {
            r().map(|l| {
                let editor = kimi_editor_mode(&l);
                let len = locate_kimi_other_residue_len(&l);
                (editor, len)
            })
        };
        let (editor0, len0) = probe_state(&mut read).ok_or_else(|| {
            StageAbort::screen(
                "覆盖/清空：进编辑后屏上读不到 Other 行（可能未进入编辑态）——已中止，未发退格；请人工核对终端",
            )
        })?;
        // **非编辑态入口三分**（2026-10-07 用户方案定稿）：单选 Other 有文字时
        // 数字 = 选中推进（事故形态）→ 改**高亮导航**（屏读 `→` 编号 → ↑/↓ 逐步
        // 【每步复核位移恰 1 行】→ 回车进编辑不推进）；多选保留两态分流
        // （未勾 = 数字进编辑 K4；已勾 = 数字取消勾再按一次进编辑）。
        if !editor0 && !multi_select {
            let target = other_digit.parse::<usize>().ok();
            let cur = read()
                .and_then(|l| locate_kimi_highlight_digit(&l))
                .and_then(|d| d.parse::<usize>().ok());
            match (target, cur) {
                (Some(t), Some(c)) if t != c => {
                    let key = if t > c { "down" } else { "up" };
                    for _ in 0..(t as i64 - c as i64).abs() {
                        terminal.send(key).map_err(|e| {
                            StageAbort::delivery(format!(
                                "INJECT_DELIVERY_FAIL: 高亮导航投递失败（{e}）"
                            ))
                        })?;
                        terminal.settle();
                        let now = read()
                            .and_then(|l| locate_kimi_highlight_digit(&l))
                            .and_then(|d| d.parse::<usize>().ok());
                        if now != Some((c as i64 + if key == "down" { 1 } else { -1 }) as usize) {
                            return Err(StageAbort::screen(
                                "高亮导航位移异常（未恰移动 1 行）——已中止，未发回车；请人工核对终端",
                            ));
                        }
                    }
                }
                (Some(_), None) => {
                    return Err(StageAbort::screen(
                        "屏上读不到当前高亮位置（无 → 前缀行）——无法导航，已中止；请人工核对终端",
                    ));
                }
                _ => {}
            }
            terminal.send("enter").map_err(|e| {
                StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 进编辑回车投递失败（{e}）"))
            })?;
            sent_keys.push("enter".to_string());
            let rounds = crate::inject::timing::poll_rounds(
                crate::inject::timing::DIGIT_VERIFY_POLL_TOTAL_MS,
            );
            if !wait_editor_mode(&mut read, terminal, rounds.max(1) as usize) {
                return Err(StageAbort::screen(
                    "高亮导航+回车后编辑态未出现（footer「type answer」缺席）——已中止；请人工核对终端",
                ));
            }
        } else if !editor0 && multi_select {
            let other_checked0 = read()
                .and_then(|l| locate_kimi_other_checked(&l))
                .ok_or_else(|| {
                    StageAbort::screen(
                        "覆盖/清空：屏上读不到 Other 行勾选态——已中止，未发任何键；请人工核对终端",
                    )
                })?;
            terminal.send(&other_digit).map_err(|e| {
                StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: Other 定位键投递失败（{e}）"))
            })?;
            sent_keys.push(other_digit.clone());
            terminal.settle();
            if other_checked0 {
                // 已勾 → 刚才那下是取消勾 → 再按一次进编辑
                terminal.send(&other_digit).map_err(|e| {
                    StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 进编辑数字投递失败（{e}）"))
                })?;
                sent_keys.push(other_digit.clone());
                terminal.settle();
            }
            // 轮询等编辑态 footer（有界窗）
            let rounds = crate::inject::timing::poll_rounds(
                crate::inject::timing::DIGIT_VERIFY_POLL_TOTAL_MS,
            );
            if !wait_editor_mode(&mut read, terminal, rounds.max(1) as usize) {
                return Err(StageAbort::screen(
                    "定位数字后编辑态未出现（footer「type answer」缺席）——已中止；请人工核对终端",
                ));
            }
        }
        // **光标推到行尾**（2026-10-06 用户机制，对齐 opencode/claude 覆盖参照块）：
        // 进编辑后光标位置不定（可能行首）——按 → × 残留字符数把光标推到最右，
        // 退格才从尾部删净。
        let before = len0.unwrap_or(0);
        if before > 0 {
            for _ in 0..before.min(400) {
                terminal.send("right").map_err(|e| {
                    StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 光标右移投递失败（{e}）"))
                })?;
            }
            sent_keys.push(format!("<right×{}>", before.min(400)));
            terminal.settle();
        }
        if before > 0 {
            // 试探退格（有文字时）：一次 → 核验长度变化；未变 → 补回车 → 再试探
            let mut len_now = before;
            for attempt in 0..2 {
                if attempt == 1 {
                    terminal.send("enter").map_err(|e| {
                        StageAbort::delivery(format!(
                            "INJECT_DELIVERY_FAIL: 试探回车投递失败（{e}）"
                        ))
                    })?;
                    sent_keys.push("enter".to_string());
                    terminal.settle();
                }
                terminal.send("backspace").map_err(|e| {
                    StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 试探退格投递失败（{e}）"))
                })?;
                sent_keys.push("backspace".to_string());
                terminal.settle();
                len_now = probe_state(&mut read)
                    .and_then(|(_, l)| l)
                    .unwrap_or(before);
                if len_now < before {
                    break;
                }
            }
            if len_now >= before {
                return Err(StageAbort::screen(
                    "试探退格未生效（Other 行文字长度未变化）——无法确认编辑态，已中止；请人工核对终端",
                ));
            }
            // 退格到底（试探已退 1 格；min 400 参照块口径）
            let rest = (before - 1).min(400);
            for _ in 0..rest {
                terminal.send("backspace").map_err(|e| {
                    StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 退格投递失败（{e}）"))
                })?;
            }
            if rest > 0 {
                sent_keys.push(format!("<backspace×{rest}>"));
            }
            terminal.settle();
            let after_len = read()
                .and_then(|l| locate_kimi_other_residue_len(&l))
                .unwrap_or(before);
            if after_len > 0 {
                return Err(StageAbort::screen(
                    "退格后 Other 行仍有残留（屏读核验未过）——已中止，未发新文本；请人工核对终端",
                ));
            }
        }
        if text.trim().is_empty() {
            // **清空语义**：文字已清（或本来就空）→ 回车退出编辑态 → 有界轮询确认
            terminal.send("enter").map_err(|e| {
                StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 清空回车投递失败（{e}）"))
            })?;
            sent_keys.push("enter".to_string());
            // 退出编辑态窗 6s（11:55 实录：空编辑态自行退出可超 3s——窗短误报）
            // 清空退出观察上限 2s（2026-10-07 用户裁决：终端 1~2s 内已正常，超时不等）
            let rounds = crate::inject::timing::poll_rounds(2_000);
            // 退出判定 = 「编辑态消失」；仍编辑 → 补回车 + ↑（回车退编辑态、
            // ↑ 移出 Other 行——之后数字选项不会敲进文字行）
            let exited = !wait_editor_mode(&mut read, terminal, rounds.max(1) as usize);
            let exited = if exited {
                true
            } else {
                terminal.send("enter").map_err(|e| {
                    StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 清空补回车投递失败（{e}）"))
                })?;
                sent_keys.push("enter".to_string());
                terminal.send("up").map_err(|e| {
                    StageAbort::delivery(format!(
                        "INJECT_DELIVERY_FAIL: 清空退出上键投递失败（{e}）"
                    ))
                })?;
                sent_keys.push("up".to_string());
                !wait_editor_mode(&mut read, terminal, rounds.max(1) as usize)
            };
            if !exited {
                // **编辑态未退出不再报异常**（2026-10-07 用户裁决）：清空语义 =
                // 文字已清且核验过（上方已过）——编辑器随后自行退出/由后续操作
                // 带离，不影响清空结果。照常成功返回，仅日志留痕。
                log::info!("kimi-freeText 键序列(清空完成·编辑态未即时退出)={sent_keys:?}");
            }
            log::info!("kimi-freeText 键序列(清空完成)={sent_keys:?}");
            return Ok(FreeTextOutcome {
                sent_keys,
                receipt_seen: Some(true),
                advanced: false,
                review_reached: false,
                screen_text: None,
                screen_checked: None,
            });
        }
    }
    // 2. 打字（字符通道——用户文本绝不进键通道）
    terminal.send_text(text).map_err(|e| {
        StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 作答文本投递失败（{e}）"))
    })?;
    sent_keys.push("<text>".to_string());
    terminal.settle();
    // 3. 保存（2026-10-07 用户方案，仅单选）：**回车 → ↓ + →** 替代——单选编辑态
    // 的回车存在双消费形态（保存+推进后残留事件选中下一题推荐项，00:59 实录）；
    // 方向键下 = 保存（不触发选中链），落定后 → 归位。多选保持 enter（保存循环
    // 已实测收敛）。
    if multi_select {
        terminal.send("enter").map_err(|e| {
            StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 保存回车投递失败（{e}）"))
        })?;
        sent_keys.push("enter".to_string());
    } else {
        terminal.send("down").map_err(|e| {
            StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 保存下键投递失败（{e}）"))
        })?;
        sent_keys.push("down".to_string());
        terminal.settle();
        terminal.send("right").map_err(|e| {
            StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 保存右键投递失败（{e}）"))
        })?;
        sent_keys.push("right".to_string());
    }
    terminal.settle();
    // **多题分流**（2026-10-06 17:00 用户实录事故修复）：单题 E-B8 的「保存即确认」
    // 语义在多题流是错的——保存后 kimi 推进新题页或进 Review，阶段机若沿用单题
    // 行为读确认编号**代发确认键**，会把未答题一并提交（用户实录：第 3 题 Other
    // 保存 → Review 被代提交 → 第 4 题跳过）。多题流保存后即停：确认提交永远
    // 交还用户显式触发（确认卡 Submit 钮），新页状态由 GET 屏读快照链回显。
    if multi_question {
        // **保存确认循环（按题形态完全分流）**（2026-10-07 结构化重构）：
        //
        // ── 多选（留原页型）── enter 保存后 TUI 自动打勾、留在原题（切题靠手动）。
        // 循环只做三件事：等编辑态退出、勾选未打上（连续 2 拍确认）→ 补发 enter
        // （≤2 次，勾上即停）、题干对位确认还在原页。**零 Review 判定、零导航**
        // （多选不跳题，落点恒为原页——导航判定曾致 03:03 事故）。
        //
        // ── 单选（自动推进型）── ↓+→ 保存后 TUI 自动推进下一题/末题进 Review。
        // 循环只做落点校验：`? ` 题干行对位载荷 question（归一唯一命中）得当前题
        // 序；落点 == 预期下一题 → 成功；落 Review 且预期为末题后 → 成功；落点
        // 不符（跳 Submit/跳过题）→ ←/→ 逐格导航（≤8 格）回预期题。
        // **零补键**（单选补发回车落点随推进时机漂移 = 第 4 题被带选事故形态）。
        let rounds =
            crate::inject::timing::poll_rounds(crate::inject::timing::QUESTION_STAGE_POLL_TOTAL_MS);
        if multi_select {
            // ===== 多选：留原页 + 勾选确认 =====
            let mut ent = 0u32;
            let mut unchecked_streak = 0u32;
            for round in 0..rounds.max(1) {
                terminal.settle();
                let lines = read().ok_or_else(|| {
                    StageAbort::screen(format!(
                        "多选保存确认第 {round} 拍读不到屏幕——已中止；请人工核对终端"
                    ))
                })?;
                if kimi_editor_mode(&lines) {
                    // **编辑态（含误入）→ 保存收尾**（2026-10-07 11:48 实录）：补勾
                    // enter 若在 TUI 已勾并退出后落下，会**重新打开编辑器**（空编辑
                    // 行）——此时再发一次 enter 保存收尾退出，不留编辑态给后续操作
                    log::info!("kimi-freeText 多选：检测到编辑态 → enter 保存收尾");
                    terminal.send("enter").map_err(|e| {
                        StageAbort::delivery(format!(
                            "INJECT_DELIVERY_FAIL: 编辑态保存收尾投递失败（{e}）"
                        ))
                    })?;
                    sent_keys.push("enter".to_string());
                    terminal.settle();
                    continue;
                }
                let checked = locate_kimi_other_checked(&lines);
                match checked {
                    Some(true) => {
                        // 勾上 = 保存完成（留原页，切题交还用户）
                        log::info!("kimi-freeText 键序列(多选达成·勾上)={sent_keys:?}");
                        return Ok(FreeTextOutcome {
                            sent_keys,
                            receipt_seen: Some(true),
                            advanced: false,
                            review_reached: kimi_review_present(&lines),
                            screen_text: None,
                            screen_checked: None,
                        });
                    }
                    Some(false) => {
                        unchecked_streak += 1;
                        if unchecked_streak >= 2 && ent < 2 {
                            ent += 1;
                            log::info!(
                                "kimi-freeText 多选补勾：连续 {unchecked_streak} 拍未勾 → 补发 enter（{ent}/2）"
                            );
                            terminal.send("enter").map_err(|e| {
                                StageAbort::delivery(format!(
                                    "INJECT_DELIVERY_FAIL: 勾选补发回车投递失败（{e}）"
                                ))
                            })?;
                            sent_keys.push("enter".to_string());
                            unchecked_streak = 0;
                            // **补 enter 后加长等待**（勾选渲染 + 退出编辑，300ms）
                            std::thread::sleep(std::time::Duration::from_millis(300));
                        }
                        // 不足 2 拍 → 纯等待（防渲染延迟误判双切）
                    }
                    None => {
                        // Other 行不在屏上 = TUI 推进了（末题→Review）→ 视作已保存
                        log::info!("kimi-freeText 键序列(多选达成·行消失推进)={sent_keys:?}");
                        return Ok(FreeTextOutcome {
                            sent_keys,
                            receipt_seen: Some(true),
                            advanced: false,
                            review_reached: kimi_review_present(&lines),
                            screen_text: None,
                            screen_checked: None,
                        });
                    }
                }
            }
            log::info!("kimi-freeText 键序列(多选窗尽未达成)={sent_keys:?}");
            return Err(StageAbort::screen(
                "多选保存后勾选未确认（补勾 2 次仍未勾上）——请人工核对终端",
            ));
        }
        // ===== 单选：落点校验 + 导航 =====
        let mut navs = 0u32;
        for round in 0..rounds.max(1) {
            terminal.settle();
            let lines = read().ok_or_else(|| {
                StageAbort::screen(format!(
                    "单选保存确认第 {round} 拍读不到屏幕——已中止；请人工核对终端"
                ))
            })?;
            let review_here = kimi_review_present(&lines);
            let cur_idx = kimi_sheet_question_idx(&lines, &payload_questions);
            let cur_eff = cur_idx.unwrap_or(payload_questions.len());
            let exp_eff = expected_idx.unwrap_or(payload_questions.len());
            // 达成：落点 == 预期题（正常推进 / 导航归位）。advanced=true——单选
            // 保存后 TUI 自动推进，前端据此同步 mqIndex
            if cur_eff == exp_eff {
                log::info!("kimi-freeText 键序列(达成·预期题 Some({exp_eff}))={sent_keys:?}");
                return Ok(FreeTextOutcome {
                    sent_keys,
                    receipt_seen: Some(true),
                    review_reached: review_here,
                    advanced: true,
                    screen_text: Some(text.to_string()),
                    screen_checked: None,
                });
            }
            // 落点不符 → 逐格导航（有界 8 格；方向按差值）
            if navs < 8 {
                navs += 1;
                let key = if cur_eff > exp_eff { "left" } else { "right" };
                log::info!(
                    "kimi-freeText 单选保存第 {round} 拍：cur={cur_eff} exp={exp_eff} review={review_here} → {key}"
                );
                terminal.send(key).map_err(|e| {
                    StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 落点导航投递失败（{e}）"))
                })?;
                sent_keys.push(key.to_string());
                continue;
            }
            break;
        }
        log::info!("kimi-freeText 键序列(单选窗尽未达成)={sent_keys:?}");
        return Err(StageAbort::screen(
            "保存后页面未回到预期题（导航 8 格未达）——请人工核对终端",
        ));
    }
    // 4. Review 汇总屏（未见即中止，不发确认键）
    let review = poll_review().map_err(StageAbort::screen)?.ok_or_else(|| {
        StageAbort::screen(format!(
            "已保存作答但屏上未出现 Review 汇总屏（未见「{}」）——已中止，未发确认键；请人工核对终端",
            kimi_review_summary_anchor()
        ))
    })?;
    if !kimi_review_present(&review) {
        return Err(StageAbort::screen(
            "Review 汇总屏形态不符——已中止，未发确认键；请人工核对终端",
        ));
    }
    let confirm_key = kimi_review_confirm_digit(&review).ok_or_else(|| {
        StageAbort::screen(
            "INJECT_SCREEN_ABORT: 读不到确认项编号——已中止，未发确认键；请人工核对终端",
        )
    })?;
    terminal.send(&confirm_key).map_err(|e| {
        StageAbort::delivery(format!(
            "INJECT_DELIVERY_FAIL: 确认键 {confirm_key} 投递失败（{e}）"
        ))
    })?;
    sent_keys.push(confirm_key);
    terminal.settle();
    // 5. 终态
    let receipt_seen = stage_receipt_seen(&mut poll_receipt, kimi_answered_present);
    log::info!("kimi-freeText 键序列={sent_keys:?} receipt_seen={receipt_seen:?}");
    Ok(FreeTextOutcome {
        sent_keys,
        receipt_seen,
        advanced: false,
        review_reached: false,
        screen_text: None,
        screen_checked: None,
    })
}

// ============================================================
// 批次戊 E5：codex Tab 备注（issue #78 codex 面；戊探C 全链定案）
// 键序 = Tab（就地展开内联备注行）→ 打字（VK 携带字符）→ Enter（提交当前高亮项+备注
// 并自动推进）；落账 = `answers.<qid>.answers` 数组第二元素 `"user_note: <全文>"`
// ============================================================

/// codex 弹窗**答案态** footer 锚（`tab to add notes | enter to submit answer | …`，
/// 戊探C 原件）——Tab 前置判据：锚在场 = request_user_input 弹窗在场且未在备注态。
pub const CODEX_NOTES_OPEN_FOOTER: &str = "tab to add notes";
/// codex 弹窗**备注态** footer 锚（`tab or esc to clear notes | …`）——已在备注态时
/// 不再发 Tab（否则清空备注，戊探C footer 语义）。
pub const CODEX_NOTES_OPENED_FOOTER: &str = "tab or esc to clear notes";
/// codex 面板 **Other 行** label 锚（`None of the above`，不区分大小写子串匹配）——
/// 自由作答走位目标（按屏实读定位，不假设恒在：Other 按题开关，底料定案 12/5）。
pub const CODEX_OTHER_ROW_LABEL: &str = "none of the above";
/// codex 提交完成**终态锚**（摘要头 `• Questions 1/1 answered`，戊探C 原件；
/// 落账侧另有 rollout `answers.<qid>` 对账——见 [`codex_user_note_from_output`]）。
///
/// **2026-09-23 起真源移入账本**（`codex/question/receipt`）；取不到时回落已入账
/// 那一条。
pub fn codex_answered_anchor() -> &'static str {
    crate::inject::anchor_ledger::candidates(
        "codex",
        crate::inject::anchor_ledger::scenario::QUESTION,
        crate::inject::anchor_ledger::slot::RECEIPT,
    )
    .first()
    .map(|r| r.text)
    .unwrap_or("answered")
}

/// codex 终态在场判定（**评审 P2-3 收窄后的权威谓词** [`codex_receipt_present`]）。
pub fn codex_answered_present(lines: &[String]) -> bool {
    codex_receipt_present(lines)
}

/// codex 问答终态回执**双词组合谓词**（评审 P2-3 特异锚收窄）：单行同时含
/// `questions` + `answered` 才判终态——摘要头专属词形 `Questions n/n answered`。
///
/// - 为什么不是裸词 `answered`（账本 0.155.1 旧行）：contains 语义下正文任何
///   answered 单词（含模型复述行）都会误命中——「已回答」中文不中但英文复述会中；
/// - 为什么题号头 `Question 1/2 (N unanswered)` 不冲突：题号头是**单数** Question
///   且无 answered 词；摘要头才是**复数** Questions + answered（底料 §7 词形表）；
/// - **已知残余**（如实登记）：正文复述若恰好同行齐备两词（如
///   `I have answered your questions`）仍会命中——比裸单词锚收窄一个量级，
///   硬断言面（题号头/任意 answered 单词）已消除；
/// - 账本两行（`answered` / `questions`，append-only）只作词形登记与
///   candidates 诊断清单供料；**判定用本谓词**——账本 candidates 全集试匹配的
///   泛化语义（任一行任一词命中）对 codex RECEIPT 槽过宽，旧行不删除。
pub fn codex_receipt_present(lines: &[String]) -> bool {
    lines.iter().any(|l| {
        let t = l.to_lowercase();
        t.contains("questions") && t.contains("answered")
    })
}

/// codex **备注自由作答阶段机**（2026-10-10 复活批，0.162.1 四取样复验全通——
/// 落卷实锤 N1-N3 + esc 红线 N4；档案 `research/refs/phase2-消息注入/
/// 2026-10-10-codex-0162-notes链复验.md`）：
/// **走位 Other → tab 开 notes → 打字（屏读验文本上屏）→ Enter 提交「Other + 备注」**。
///
/// # 各段与中止点（每步屏读核验，不盲发）
///
/// 1. **入口闸**：读屏解析面板（[`codex_question_screen_snapshot`]）——解析不出
///    但见确认屏锚 → 中止「前序动作已错位」；身份闸（屏上题号=手机卡题号）+
///    题数交叉核对，与 select 阶段机同款；
/// 2. **Other 行定位**：选项 label 含 `none of the above`（不区分大小写）——缺席
///    → 零键中止（Other 按题开关，底料定案 5「按屏实读不假设恒在」）；
/// 3. **残留守卫（仅发送路径）**：入场 footer 已是备注态 → 中止（发送会追加在
///    旧文后）；**清空/覆盖写入不拦**——「备注态有字」正是它们的操作对象；
/// 4. **清空段（overwrite，两线路共用）**：**tab 循环 ≤2 下、每下后读屏，note 行
///    字消失即提前终止**（2026-10-10 用户规格「默认按两下，第一下后有字变无字就
///    终止」）；终判无字后，若停在空 notes 态（第二下 tab 落答案态会重开空 notes）
///    → esc 退回答案态。text 空 = 清空请求，到此返回（不发 Enter）；
/// 5. **发送段（发送线路直入；覆盖写入=清空段拼本段）**：Other 行定位（按屏实读）
///    → 改答守卫（已答 ∧ 焦点≠Other → 中止；**豁免：覆盖 ∧ 入场有 note**——note
///    只能经 Other-notes 流产生，旧答案必是 Other，走位+ensure 是选回同一行）→
///    vim j/k 最短环绕走位（每步复读）→ tab 开 notes → 4 拍轮询 footer 翻转；
/// 7. **打字 + 上屏核验**：字符通道打字后有界 4 拍屏读——剥空白归一化后全文必须
///    在屏上（0.162.1 字符层回显可见；**这道闸就是 0.160.0「打字不可见+静默丢失」
///    事故的复防线**：文本没上屏绝不发 Enter）；
/// 8. **Enter 提交 + 终态**：回车提交「Other + 备注」（备注态 Enter 无未答确认屏，
///    戊探C ③ + 复验 N1 三重实证；**发送与覆盖写入两线路的提交键都是回车**——
///    2026-10-10 用户裁决，备注态 tab 是清空+退出不是提交）；终态轮询
///    [`codex_answered_present`]——未见不是失败（`Some(false)` 如实请人工核对；
///    落账侧 rollout `user_note:` 对账）。
///
/// **形态门**：本编排只对单题卡放行（多题 notes 归属未在 0.162.1 复采，留白见
/// 复验档案 §4——多题流端点侧已拒，进不到这里）。
pub fn run_codex_notes_stages<T: FreeTextTerminal>(
    q_idx: usize,
    expected_total: usize,
    text: &str,
    overwrite: bool,
    mut poll: impl FnMut() -> Result<Option<Vec<String>>, String>,
    terminal: &mut T,
) -> Result<FreeTextOutcome, StageAbort> {
    use crate::inject::anchor_ledger::{self, scenario, slot};
    use crate::inject::question_screen_oc::codex_question_screen_snapshot;

    if text.trim().is_empty() && !overwrite {
        return Err(StageAbort::screen(
            "自由作答文本为空（清空请用覆盖写入）——已中止，未发任何键",
        ));
    }
    let mut sent_keys: Vec<String> = Vec::new();
    let notes_footer = |lines: &[String]| {
        lines
            .iter()
            .any(|l| l.to_lowercase().contains(CODEX_NOTES_OPENED_FOOTER))
    };

    // 1. 入口闸：读屏 + 面板解析（确认屏与「解析不出」分开点名，§3.6）
    let first = terminal.read().ok_or_else(|| {
        StageAbort::screen(
            "INJECT_SCREEN_ABORT: codex 备注：读不到屏幕——已中止，未发任何键；请人工核对终端",
        )
    })?;
    let snap = match codex_question_screen_snapshot(&first) {
        Some(s) => s,
        None => {
            let lowered: Vec<String> = first.iter().map(|l| l.to_lowercase()).collect();
            if anchor_ledger::detect(&lowered, "codex", scenario::QUESTION_REVIEW, slot::TITLE)
                .is_some()
            {
                return Err(StageAbort::screen(
                    "codex：屏上是未答完确认屏（Submit with unanswered questions?）——前序动作已错位，兔维斯 不代答；请到终端人工处置",
                ));
            }
            return Err(StageAbort::screen(
                "codex 备注：屏上解析不出问答面板（题号头/footer 锚不成立）——已中止，未发任何键；请人工核对终端",
            ));
        }
    };
    // 身份闸 + 题数交叉核对（与 select 阶段机同款——错卷发键 = 打错题）
    if snap.question_idx != q_idx {
        return Err(StageAbort::screen(format!(
            "codex 身份闸：屏上第 {} 题与手机卡第 {} 题不一致——已中止，未发任何键；请核对终端当前题",
            snap.question_idx + 1,
            q_idx + 1
        )));
    }
    if snap.question_total != expected_total {
        return Err(StageAbort::screen(format!(
            "codex 身份闸：终端问卷共 {} 题与手机卡 {} 题不一致——已中止，未发任何键；请核对终端",
            snap.question_total, expected_total
        )));
    }

    // 2. note 行在态记录 + 残留守卫（**仅发送路径**——清空/覆盖写入的入口就是
    //    备注态有字，「残留」正是操作对象，不拦）
    let note_present =
        |lines: &[String]| crate::inject::question_screen_oc::codex_notes_row_text(lines).is_some();
    let had_note = note_present(&first);
    if !overwrite && notes_footer(&first) {
        return Err(StageAbort::screen(
            "codex 备注：终端已在备注输入态（上次编排半程残留）——已中止，未发任何键；请人工核对终端（esc 可清备注退出）",
        ));
    }

    // 3. 清空段（覆盖写入/清空共用；0.162.1 语义：notes 态 tab = 清空+退出）：
    //    **tab 循环 ≤2 下，每下之后读屏，note 行字消失即提前终止**（2026-10-10
    //    用户规格：「默认按两下，第一下后有字变无字就终止」）
    if overwrite {
        for _ in 0..2 {
            let Some(l) = terminal.read() else {
                return Err(StageAbort::screen(
                    "codex 备注：清空段读不到屏——已中止；请核对终端",
                ));
            };
            if !note_present(&l) {
                break; // 已无字（含占位态）——清空目标达成
            }
            terminal.send("tab").map_err(|e| {
                StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 清空 Tab 投递失败（{e}）"))
            })?;
            sent_keys.push("tab".to_string());
            terminal.settle();
            poll_until(&mut poll, 4, |ls| !note_present(ls));
            // 本下是否清掉由循环顶部重读终判（读竞态不在这里下结论）
        }
        // 清空终判：note 行必须无字
        let Some(l) = terminal.read() else {
            return Err(StageAbort::screen(
                "codex 备注：清空终判读不到屏——已中止；请核对终端",
            ));
        };
        if note_present(&l) {
            return Err(StageAbort::screen(
                "codex 备注：两下清空 Tab 后 note 行仍有残字——已中止，未发提交键；请人工核对终端",
            ));
        }
        // 若停在空 notes 态（第二下 tab 落在答案态会「重开空 notes」）→ esc 退回，
        // 给发送段一个干净的答案态入口
        if notes_footer(&l) {
            terminal.send("esc").map_err(|e| {
                StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 退出 esc 投递失败（{e}）"))
            })?;
            sent_keys.push("esc".to_string());
            terminal.settle();
            if !poll_until(&mut poll, 4, |ls| !notes_footer(ls)) {
                return Err(StageAbort::screen(
                    "codex 备注：esc 后备注态未退出——已中止；请核对终端",
                ));
            }
        }
        if text.is_empty() {
            // **清空请求完成**：note 行无字即目标达成（不发 Enter——codex 无「只存
            // 不交」键，本题提交走卡面提交按钮/后续动作）
            return Ok(FreeTextOutcome {
                sent_keys,
                receipt_seen: None,
                advanced: false,
                review_reached: false,
                screen_text: None,
                screen_checked: None,
            });
        }
    }

    // 4. 发送段（发送线路直入；覆盖写入=清空段拼本段）：Other 行定位 + 改答守卫
    //    + 走位 + tab 开 notes
    // 覆盖路径清空段发过键，入口快照已陈旧——重读重析（身份闸一并复核）
    let snap = if overwrite {
        let l = terminal.read().ok_or_else(|| {
            StageAbort::screen(
                "INJECT_SCREEN_ABORT: codex 备注：发送段读不到屏——已中止；请核对终端",
            )
        })?;
        let s = codex_question_screen_snapshot(&l).ok_or_else(|| {
            StageAbort::screen(
                "INJECT_SCREEN_ABORT: codex 备注：发送段解析不出问答面板——已中止；请核对终端",
            )
        })?;
        if s.question_idx != q_idx {
            return Err(StageAbort::screen(format!(
                "codex 身份闸：屏上第 {} 题与手机卡第 {} 题不一致——已中止，未发任何键；请核对终端当前题",
                s.question_idx + 1,
                q_idx + 1
            )));
        }
        s
    } else {
        snap
    };
    let other_idx = snap
        .options
        .iter()
        .position(|o| o.to_lowercase().contains(CODEX_OTHER_ROW_LABEL))
        .ok_or_else(|| {
            StageAbort::screen(
                "codex 备注：屏上没有 Other 行（None of the above）——本题面板不支持自由作答，已中止，未发任何键；请到终端作答",
            )
        })?;
    // 改答守卫（P1-2 同源保守）：已答 ∧ 焦点≠Other → tab 的 ensure 会换选，不冒进。
    // **豁免**：覆盖写入 ∧ 入场有 note——note 只能经 Other-notes 流产生，旧答案必是
    // Other，走位+ensure 是「选回同一行」，不存在换选（2026-10-10 用户规格：覆盖
    // 写入正是「有字时改字」的主路径）
    if snap.unanswered == 0 && snap.focused != Some(other_idx) && !(overwrite && had_note) {
        return Err(StageAbort::screen(
            "codex 备注：本题已作答且焦点不在 Other 行——tab 的 ensure 选中会把答案换到 Other（0.162.1 未复采该路径），远程代答不冒进；已中止，未发任何键；请到终端人工操作",
        ));
    }

    // 走位到 Other（vim j/k 最短环绕、每步复读——镜像 select 末题段）
    let Some(f0) = snap.focused else {
        return Err(StageAbort::screen(
            "codex 备注：读不到焦点行（› 缺席）——不猜起点，已中止，未发任何键；请核对终端",
        ));
    };
    let m = snap.options.len();
    let down = (other_idx + m - f0) % m;
    let up = (f0 + m - other_idx) % m;
    let walk_key = if up < down { "k" } else { "j" };
    let mut focused = f0;
    for _ in 0..down.min(up) {
        terminal.send(walk_key).map_err(|e| {
            StageAbort::delivery(format!(
                "INJECT_DELIVERY_FAIL: 走位键 {walk_key} 投递失败（{e}）"
            ))
        })?;
        sent_keys.push(walk_key.to_string());
        terminal.settle();
        match terminal
            .read()
            .as_deref()
            .and_then(codex_question_screen_snapshot)
        {
            Some(s) => match s.focused {
                Some(f) if f == other_idx => {
                    focused = f;
                    break;
                }
                _ => focused = s.focused.unwrap_or(focused),
            },
            None => {
                return Err(StageAbort::screen(
                    "codex 备注：走位中读屏失败或面板形态崩——已中止；请核对终端",
                ))
            }
        }
    }
    if focused != other_idx {
        return Err(StageAbort::screen(
            "codex 备注：走位有界耗尽仍未到 Other 行——已中止；请核对终端",
        ));
    }

    // tab 开 notes + 有界轮询 footer 翻转
    terminal
        .send("tab")
        .map_err(|e| StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: Tab 投递失败（{e}）")))?;
    sent_keys.push("tab".to_string());
    terminal.settle();
    if !poll_until(&mut poll, 4, |lines| notes_footer(lines)) {
        return Err(StageAbort::screen(
            "codex 备注：Tab 后未见备注态（footer 未翻转出「tab or esc to clear notes」）——已中止；请核对终端",
        ));
    }
    // **输入行就绪门**（2026-10-10 20:55 实机：footer 翻转后立刻打字，字符落在
    // notes 输入行挂载完成之前——末帧 notes 关闭、字消失）。等 `› Add notes`/
    // `› <文本>` 行出现（输入行挂载的可见标记）才许打字。
    if !poll_until(&mut poll, 4, |lines| {
        crate::inject::question_screen_oc::codex_notes_input_ready(lines)
    }) {
        return Err(StageAbort::screen(
            "codex 备注：备注态已开但输入行未就绪（未见 › 行）——已中止，未打字；请核对终端",
        ));
    }

    // 7. 打字 + 上屏核验（0.160.0「打字不可见静默丢失」的复防线：文本没上屏绝不发 Enter）
    if !text.is_empty() {
        terminal.send_text(text).map_err(|e| {
            StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 备注文本投递失败（{e}）"))
        })?;
        sent_keys.push("<text>".to_string());
        terminal.settle();
        // 证据自带失败（闸门 2）：核验各拍实读帧留存，误拦/真丢两可时日志是物证
        // （2026-10-10 19:25 实机：文字已上屏仍被瞬发拍误拦——无帧留证只能靠推测归因）
        let mut last_frame: Option<Vec<String>> = None;
        let seen = poll_until(&mut poll, 10, |lines| {
            last_frame = Some(lines.to_vec());
            screen_contains_text(lines, text)
        });
        if !seen {
            // 证据自带失败（闸门 2）：末拍实读帧的归一化尾 400 字——足够看出
            // 「文字其实在屏上」或「真没上屏」（2026-10-10 19:25/19:49 实机教训）
            if let Some(frame) = &last_frame {
                let joined = frame.join(" ");
                let tail: String = joined
                    .chars()
                    .rev()
                    .take(400)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                log::warn!(
                    "codex-notes 上屏核验未命中（text_len={}）——末拍实读帧 {} 行，归一化尾 400 字：{tail}",
                    text.chars().count(),
                    frame.len()
                );
            } else {
                log::warn!("codex-notes 上屏核验未命中——窗内一帧屏都没读到（读屏失败/窗尽）");
            }
            // **notes 态在打字后意外关闭**（19:49 实机形态：覆盖双 tab 重开核验通过后
            // notes 又关了）→ 文本落空是状态问题不是重绘问题，重试只会雪上加霜——
            // 点名中止；notes 态还在 → 文本可能被 TUI 吃掉（送达抖动），**整段重试
            // 一次**（清空重打语义：先 esc 清掉可能落了一半的残文，再重走 tab+打字）
            let notes_still_open = last_frame.as_ref().is_some_and(|f| notes_footer(f));
            if notes_still_open {
                log::warn!("codex-notes 上屏核验未命中但备注态仍在——清残重打一次（送达抖动重试）");
                terminal.send("esc").map_err(|e| {
                    StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 清残 esc 投递失败（{e}）"))
                })?;
                sent_keys.push("esc".to_string());
                terminal.settle();
                terminal.send("tab").map_err(|e| {
                    StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 重试 tab 投递失败（{e}）"))
                })?;
                sent_keys.push("tab".to_string());
                terminal.settle();
                if !poll_until(&mut poll, 4, |lines| notes_footer(lines)) {
                    return Err(StageAbort::screen(
                        "codex 备注：重试重开备注态失败——已中止，未发提交键；请人工核对终端",
                    ));
                }
                terminal
                    .send_text(text)
                    .map_err(|e| StageAbort::delivery(format!("备注文本重投失败（{e}）")))?;
                sent_keys.push("<text>".to_string());
                terminal.settle();
                let retried = poll_until(&mut poll, 10, |lines| {
                    last_frame = Some(lines.to_vec());
                    screen_contains_text(lines, text)
                });
                if !retried {
                    return Err(StageAbort::screen(
                        "codex 备注：重试打字后屏上仍未见到备注文本——已中止，未发提交键；请人工核对终端",
                    ));
                }
            } else {
                return Err(StageAbort::screen(
                    "codex 备注：打字后备注态意外关闭且屏上未见备注文本（覆盖重开竞态或文本未被通道送达）——已中止，未发提交键；请人工核对终端后重试",
                ));
            }
        }
        // 8. Enter 提交 + 终态（未见不是失败——如实 Some(false)）
        terminal.send("enter").map_err(|e| {
            StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 提交回车投递失败（{e}）"))
        })?;
        sent_keys.push("enter".to_string());
        terminal.settle();
        let receipt_seen = stage_receipt_seen(&mut poll, codex_answered_present);
        log::info!("codex-notes 键序列={sent_keys:?} receipt_seen={receipt_seen:?}");
        Ok(FreeTextOutcome {
            sent_keys,
            receipt_seen,
            advanced: false,
            review_reached: false,
            screen_text: None,
            screen_checked: None,
        })
    } else {
        // text 空 ∧ !overwrite 已在函数头拒绝；到这里必是 overwrite ∧ 空文本，
        // 在覆盖段已返回——防御性兜底（不应可达）
        Err(StageAbort::screen(
            "codex 备注：内部状态异常（空文本落到打字段）——已中止",
        ))
    }
}

/// 有界轮询谓词（`beats` 拍；任一拍命中即真，窗尽/读屏失败=假）。
///
/// **拍间必须有真实时距**（2026-10-10 19:25 实机二次误拦根因）：`poll()` 读屏成功
/// 即立即返回，若拍间不 sleep，`beats` 拍背靠背在几十毫秒内瞬发打完——所谓「重绘
/// 等待窗」实际不存在，全部落在 TUI 重绘之前（首拍前有 `terminal.settle()` 的
/// 150ms，但只保护首拍）。拍间 `POLL_STEP_MS`（100ms）时距让 10 拍成为真实的
/// ~1s 重绘窗口。
fn poll_until(
    poll: &mut impl FnMut() -> Result<Option<Vec<String>>, String>,
    beats: usize,
    mut pred: impl FnMut(&[String]) -> bool,
) -> bool {
    for i in 0..beats {
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_millis(
                crate::inject::timing::POLL_STEP_MS,
            ));
        }
        match poll() {
            Ok(Some(lines)) if pred(&lines) => return true,
            Ok(Some(_)) => { /* 屏在但未命中——下一拍再验（真实时距窗） */ }
            _ => return false, // 窗尽 / 读屏失败
        }
    }
    false
}

/// 屏面是否包含目标文本（**整屏剥空白归一化**后子串判定——备注文本折行显示时
/// 会跨屏行断开，按单行 contains 会漏判（2026-10-10 实机误拦教训：备注已上屏仍被
/// 判「未上屏」误中止）；整屏拼接归一化后折行/缩进不再影响匹配）。
fn screen_contains_text(lines: &[String], text: &str) -> bool {
    let squash = |s: &str| {
        s.chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_lowercase()
    };
    let needle = squash(text);
    if needle.is_empty() {
        return false;
    }
    let screen = squash(&lines.join(""));
    screen.contains(&needle)
}

/// codex rollout **user_note 对账解析**（纯函数）：`function_call_output.output`
/// （双重 JSON 字符串，外层 `{"answers":{<qid>:{"answers":[...]}}}`）→ 取首个带
/// `user_note: ` 前缀的数组元素并返回备注全文（戊探C ④定案：备注不是独立字段，
/// 是 answers 数组第二元素）。无备注 → None。
/// 消费：验收对账（`tests/fixtures/e-stage2/codex-rollout-user-note.txt` 夹具锁）+
/// `#[ignore]` 实机用例的落账断言。
pub fn codex_user_note_from_output(output: &str) -> Option<String> {
    let outer: serde_json::Value = serde_json::from_str(output).ok()?;
    let answers = outer.get("answers")?.as_object()?;
    for (_qid, v) in answers {
        let Some(arr) = v.get("answers").and_then(|a| a.as_array()) else {
            continue;
        };
        for e in arr {
            if let Some(s) = e.as_str() {
                if let Some(note) = s.strip_prefix("user_note: ") {
                    return Some(note.to_string());
                }
            }
        }
    }
    None
}

// ============================================================
// Task 5：codex 单选 select 阶段机（设计 §3.2 非末题 / §3.3 末题与单题）
// 取证依据：2026-10-09 底料 `docs/superpowers/specs/2026-10-08-codex-160-question-
// 屏读底料.md` §1 定案总表（A–H）。**关键通道更正（底料 §1）：codex 0.160.0 问答
// 面板对 VT 箭头（ESC[B/A/C/D）与真 VK 箭头全部免疫——走位必须用 vim j/k（j=下移
// k=上移，双向环绕），切题用 vim h/l（本阶段机不切题，注释留档防误引箭头键）。
// ============================================================

/// codex 单选阶段机的成功回执（设计 §3.2/§3.3 输出契约的键序 + 两面旗标）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSelectOutcome {
    /// 实际发送的键（记录法：数字记字符 `"1"`；走位记 `"j"`/`"k"`；空格记
    /// `"space"`；回车记 `"enter"`——语义名与字符自洽，测试同款）
    pub sent_keys: Vec<String>,
    /// 非末题改答分支：题号推进而计数未减（进闸前已答，§3.2 漂移分支）
    pub reanswered: bool,
    /// 末题双条件闸读到面板消失且未发提交键（如 notes 回车连带交卷，§3.3 步 6 分叉）
    pub already_submitted: bool,
}

/// codex **单选 select 阶段机**：非末题「数字直选 + 基线变化轮询 + 重投前复核」+
/// 末题「j/k 走位 → 空格 → 归零有界轮询 → 双条件闸 → enter 提交」闭环
/// （设计 §3.2/§3.3；评审 P0-1/P2-9/P2-10 修法）。
///
/// # 取证定案（2026-10-09 底料，编号 §1 定案总表）
/// 1. **数字 = 选中 + 自动推进原子**（定案 A）：未答题发数字后题号头自动 i→i+1
///    且未答计数 −1；**末题上数字 = 选中 + 整卷提交**（故末题禁数字）。
/// 2. **走位通道 = vim j/k**（底料 §1 更正）：VT 箭头与 VK 箭头对 codex 面板全部
///    无效；j=下移 k=上移，上下均环绕；**每步发键后复读**（不批量盲发）。
/// 3. **空格 = 仅选中焦点行**（不推进）；对已答项再按 = 反选（计数 +1）；
///    选中不改高亮位（`›` 跟焦点走，选中态只在属性层）。
/// 4. **切题通道 = vim h/l**（h=上一题 l=下一题）——本阶段机不切题；**不是箭头键**
///    （底料 §1：真 VK_LEFT 复验不切题）。
/// 5. **回车 = 提交**：末题回车整卷提交（全答完无确认）；notes 态回车 = 提交焦点项。
/// 6. **归零词形**：全答完时题号头计数段整个消失（`Question 2/2`），
///    [`codex_question_screen_snapshot`] 解析 unanswered = 0。
/// 7. **数字被吞推断**（定案 A 推论）：题号未变 ⟹ 数字被吞 ⟹ 该题未答——重发
///    数字一次（**不得补 l**，l 是切题键，会把未答题跳过去）。**重投前必须复核**
///    （评审 P0-1）：单拍轮询读到未重绘旧屏不等于被吞——盲目重投可能落在下一题/
///    末题上（末题数字 = 整卷提交红线）。
///
/// # 参数
/// `expected_total`——手机卡载荷的题数（评审 P2-10 交叉核对：与屏上题号头总数
/// 不一致 = 错卷，零键中止）。
/// `poll()`——`Ok(Some(屏行))` = 动作后屏已就绪；`Ok(None)` = 轮询窗尽未见目标；
/// `Err` = 读屏失败。
pub fn run_codex_select_stages<T: MenuTerminal>(
    q_idx: usize,
    expected_total: usize,
    index: usize,
    poll: impl Fn() -> Result<Option<Vec<String>>, String>,
    terminal: &mut T,
) -> Result<CodexSelectOutcome, StageAbort> {
    use crate::inject::anchor_ledger::{self, scenario, slot};
    use crate::inject::question_screen_oc::codex_question_screen_snapshot;

    let mut sent_keys: Vec<String> = Vec::new();

    // ---------- 入口闸 ----------
    // 1. 读首屏；读不到 = 零键中止（屏读为准：动手前必须见到面板）
    // 2. 解析面板（确认屏与「解析不出」分开点名——用户要做的处置不同，§3.6）
    // 3. 身份闸：屏上题号必须与手机卡对位（错位发数字 = 打错题，§3.2 步 1）+
    //    题数交叉核对（评审 P2-10：终端问卷题数 ≠ 载荷题数 = 错卷，零键中止）
    let first = terminal.read().ok_or_else(|| {
        StageAbort::screen(
            "INJECT_SCREEN_ABORT: codex：读不到屏——已中止，未发任何键；请人工核对终端",
        )
    })?;
    let snap = match codex_question_screen_snapshot(&first) {
        Some(s) => s,
        None => {
            // 确认屏与「解析不出」分开点名——用户要做的处置不同（§3.6）
            let lowered: Vec<String> = first.iter().map(|l| l.to_lowercase()).collect();
            if anchor_ledger::detect(&lowered, "codex", scenario::QUESTION_REVIEW, slot::TITLE)
                .is_some()
            {
                return Err(StageAbort::screen(
                    "codex：屏上是未答完确认屏（Submit with unanswered questions?）——前序动作已错位，兔维斯 不代答；请到终端人工处置",
                ));
            }
            return Err(StageAbort::screen(
                "codex：屏上解析不出问答面板（题号头/footer 锚不成立）——已中止，未发任何键；请人工核对终端",
            ));
        }
    };
    // 3a. 身份闸：屏上题号必须与手机卡对位（错位发数字 = 打错题，§3.2 步 1）
    if snap.question_idx != q_idx {
        return Err(StageAbort::screen(format!(
            "codex 身份闸：屏上第 {} 题与手机卡第 {} 题不一致——已中止，未发任何键；请核对终端当前题",
            snap.question_idx + 1,
            q_idx + 1
        )));
    }
    // 3b. 题数交叉核对（评审 P2-10）：终端问卷总数与手机卡载荷题数不一致 =
    //     屏上停的是另一份问卷（错卷发键 = 打错题）——零键中止
    if snap.question_total != expected_total {
        return Err(StageAbort::screen(format!(
            "codex 身份闸：终端问卷共 {} 题与手机卡 {} 题不一致——已中止，未发任何键；请核对终端",
            snap.question_total, expected_total
        )));
    }

    // ---------- 非末题分支：数字直选（禁走到末题上——末题数字 = 整卷提交，定案 A）----------
    if !snap.is_last {
        return run_codex_select_nonfinal(
            q_idx,
            index,
            snap,
            &first,
            poll,
            terminal,
            &mut sent_keys,
        );
    }

    // ---------- 末题分支（§3.3：禁数字、禁回车做选中）----------
    run_codex_select_final(q_idx, index, snap, poll, terminal, &mut sent_keys)
}

/// 非末题分支（§3.2）：发数字 → **基线变化轮询**（评审 P0-1：单拍读到未重绘旧屏
/// 不判未变，窗尽才判）→ 重投前复核仍在本题才重发（末题数字 = 整卷提交红线旁路
/// 的防线）。
///
/// 轮询语义（每拍 `poll()`）：
/// - `Ok(Some(lines))` 解析出 `question_idx == q_idx+1` → 推进成功（迟到位落地同样算）；
/// - `Ok(Some(lines))` 屏面 ≠ 基线但题号未推进 → **形态异常中止**（屏在动却没切题
///   ——数字可能被吃进别处，重投有打错题风险）；
/// - `Ok(Some(lines))` 屏面原样 == 基线 → 继续下一拍（TUI 未重绘，不能下结论）；
/// - `Ok(None)`（窗尽）→ 跳出循环进入重投前复核；
/// - `Err` → 读屏失败中止。
fn run_codex_select_nonfinal<T: MenuTerminal>(
    q_idx: usize,
    index: usize,
    entry: crate::inject::question_screen_oc::CodexQuestionSnapshot,
    baseline_lines: &[String],
    poll: impl Fn() -> Result<Option<Vec<String>>, String>,
    terminal: &mut T,
    sent_keys: &mut Vec<String>,
) -> Result<CodexSelectOutcome, StageAbort> {
    use crate::inject::question_screen_oc::codex_question_screen_snapshot;
    // poll 闭包按引用复用（基线变化轮询与重投后的第二轮询共用同一编排闭包）
    let poll = &poll;
    // k_base = 入口闸的未答基线（改答判定：落点计数未减 = 进闸前已答）
    let k_base = entry.unanswered;
    // 目标越界卫语句（评审 P2-4）：index 来自手机卡载荷——载荷与屏上选项数不符
    // 时数字会打在无效序号上（codex 面板对无效数字静默忽略 → 只剩双投中止的
    // 兜底，行为不精确）。发键前零键中止，报载荷与面板不同步。
    let m = entry.options.len();
    if index >= m {
        return Err(StageAbort::screen(format!(
            "codex 单选：目标选项序号 {index} 越界（屏上面板共 {m} 项）——已中止，未发任何键；载荷与终端面板可能不同步，请刷新后重试",
        )));
    }
    // 基线 = 入口屏原文（评审 P0-1：变化轮询的比较基线——逐行原样比较，
    // TUI 未重绘 = 与基线逐字相同）
    let baseline = baseline_lines.to_vec();
    let digit = (index + 1).to_string();
    // 发数字（send → 记账 → settle，与既有阶段机同款三步）
    terminal
        .send(&digit)
        .map_err(|e| StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 数字投递失败（{e}）")))?;
    sent_keys.push(digit);
    terminal.settle();

    // ---- 基线变化轮询（有界 4 拍；窗尽才判未变，评审 P0-1）----
    match poll_changed_until_window(q_idx, &baseline, poll)? {
        ChangedPoll::Advanced(s) => {
            return Ok(CodexSelectOutcome {
                sent_keys: sent_keys.clone(),
                reanswered: s.unanswered == k_base,
                already_submitted: false,
            });
        }
        ChangedPoll::ChangedNotAdvanced => {
            return Err(StageAbort::screen(
                "codex 单选：屏已变化但题号未推进（形态异常）——已中止；请核对终端",
            ));
        }
        ChangedPoll::WindowExhausted => { /* 原样窗尽 → 重投前复核 */ }
    }

    // ---- 重投前复核（评审 P0-1：重投必须确认屏上仍是本题非末题）----
    // `terminal.read()` 再读一屏：迟到位重绘可能此刻才落地——推进了就不重投
    let Some(recheck) = terminal.read() else {
        return Err(StageAbort::screen(
            "codex 单选：重投前复核读不到屏——已中止，未重投；请人工核对终端",
        ));
    };
    match codex_question_screen_snapshot(&recheck) {
        // 迟到位落地：题号已推进（不重投，直接成功）
        Some(s) if s.question_idx == q_idx + 1 => Ok(CodexSelectOutcome {
            sent_keys: sent_keys.clone(),
            reanswered: s.unanswered == k_base,
            already_submitted: false,
        }),
        // 仍是本题（且非末题——入口闸后未变）→ 才重发数字，再走一遍变化轮询
        Some(s) if s.question_idx == q_idx && !s.is_last => {
            codex_select_resend_digit(q_idx, index, k_base, &baseline, poll, terminal, sent_keys)
        }
        // 屏上已是末题/其他题（重投将打错题/触碰末题禁数字红线）→ 中止不重投
        Some(_) => Err(StageAbort::screen(
            "codex 单选：屏上已不是本题（重投将打错题/触碰末题禁数字红线）——已中止，未重投；请核对终端",
        )),
        // 复核读到了屏但解析不出面板（形态异常）→ 同样不重投
        None => Err(StageAbort::screen(
            "codex 单选：重投前复核解析不出问答面板——已中止，未重投；请核对终端",
        )),
    }
}

/// 非末题数字**重投**（定案 A 推论：数字被吞该题未答，重发不补 l）——
/// 仅在「重投前复核」确认屏上仍是本题（非末题）后调用（评审 P0-1）。
/// 重发后再走一遍基线变化轮询：仍不推进 → 中止「两投」。
fn codex_select_resend_digit<T: MenuTerminal>(
    q_idx: usize,
    index: usize,
    k_base: usize,
    baseline: &[String],
    poll: impl Fn() -> Result<Option<Vec<String>>, String>,
    terminal: &mut T,
    sent_keys: &mut Vec<String>,
) -> Result<CodexSelectOutcome, StageAbort> {
    // 重投键 = 首投同数字（index+1——重投的语义是「同一个数字再发一遍」，
    // 不是别的题的编号）
    let digit = (index + 1).to_string();
    terminal.send(&digit).map_err(|e| {
        StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 重投数字投递失败（{e}）"))
    })?;
    sent_keys.push(digit);
    terminal.settle();
    // 重投后同款基线变化轮询（窗尽/原样 → 两投中止）
    match poll_changed_until_window(q_idx, baseline, poll)? {
        ChangedPoll::Advanced(s) => Ok(CodexSelectOutcome {
            sent_keys: sent_keys.clone(),
            reanswered: s.unanswered == k_base,
            already_submitted: false,
        }),
        ChangedPoll::ChangedNotAdvanced | ChangedPoll::WindowExhausted => Err(StageAbort::screen(
            "codex 单选：数字两投后题号仍未推进——已中止；请核对终端（选项可能已选中，勿重复点选）",
        )),
    }
}

/// 基线变化轮询的**单拍三态**（评审 P0-1 内部产物）。
enum ChangedPoll {
    /// 题号已推进到 q_idx+1（带落点快照）
    Advanced(crate::inject::question_screen_oc::CodexQuestionSnapshot),
    /// 屏面变了但题号未推进（形态异常）
    ChangedNotAdvanced,
    /// 窗尽（Ok(None)）或各拍屏面一直与基线原样相同
    WindowExhausted,
}

/// **基线变化轮询**（评审 P0-1）：有界 4 拍调 `poll()`——
/// - 任一拍解析出 `question_idx == q_idx+1` → [`ChangedPoll::Advanced`]；
/// - 任一拍屏面 ≠ 基线但题号未推进 → [`ChangedPoll::ChangedNotAdvanced`]；
/// - `Ok(None)`（窗尽）或 4 拍全原样 → [`ChangedPoll::WindowExhausted`]；
/// - `Err` → 屏读失败中止（原样透传文案）。
fn poll_changed_until_window(
    q_idx: usize,
    baseline: &[String],
    poll: impl Fn() -> Result<Option<Vec<String>>, String>,
) -> Result<ChangedPoll, StageAbort> {
    use crate::inject::question_screen_oc::codex_question_screen_snapshot;
    const CHANGE_POLL_ROUNDS: usize = 4;
    for _ in 0..CHANGE_POLL_ROUNDS {
        match poll().map_err(StageAbort::screen)? {
            Some(lines) => {
                if let Some(s) =
                    codex_question_screen_snapshot(&lines).filter(|s| s.question_idx == q_idx + 1)
                {
                    return Ok(ChangedPoll::Advanced(s));
                }
                if lines != baseline {
                    // 屏变了但题号没推进——形态异常（数字可能被吃进别处）
                    return Ok(ChangedPoll::ChangedNotAdvanced);
                }
                // 原样 == 基线：TUI 未重绘，不能下结论 → 继续下一拍
            }
            None => return Ok(ChangedPoll::WindowExhausted), // 窗尽
        }
    }
    // 有界耗尽且每拍都原样——等价于「窗内未见变化」
    Ok(ChangedPoll::WindowExhausted)
}

/// 末题分支（§3.3）：焦点走位（j/k 最短环绕，每步复读）→ 空格 → 归零闸（双验）→
/// 双条件闸 → enter。**禁数字**（末题数字 = 整卷提交，定案 A）。
///
/// `q_idx`——入口闸已验证的题号（评审 P1-1：提交闸复读快照必须与它对位，
/// 防等待窗口内用户 h/l 切题后 enter 落非末题）。
fn run_codex_select_final<T: MenuTerminal>(
    q_idx: usize,
    index: usize,
    entry: crate::inject::question_screen_oc::CodexQuestionSnapshot,
    poll: impl Fn() -> Result<Option<Vec<String>>, String>,
    terminal: &mut T,
    sent_keys: &mut Vec<String>,
) -> Result<CodexSelectOutcome, StageAbort> {
    use crate::inject::anchor_ledger;
    use crate::inject::question_screen_oc::codex_question_screen_snapshot;
    let target = index;
    let m = entry.options.len();

    // 1. 焦点起点：读不到 › = 不猜（选中态打在错误的行上不可逆——末题回车就交卷）
    let f0 = entry.focused.ok_or_else(|| {
        StageAbort::screen(
            "codex 末题：读不到焦点行（› 缺席）——不猜起点，已中止，未发任何键；请核对终端",
        )
    })?;

    // 1a. 已答改答不同项·如实中止（评审 P1-2，按底料 §3 #5 dump 直接证据——
    //     须在走位**之前**判：此形态一旦走位，发出去的 j/k 都是在为「换选」
    //     铺路，而 0.160.0 实测已答题上空格 = 先反选旧答案（计数 +1）、新项
    //     未选中——「换选」需要「空格选新 → 空格……」多拍编排且旧答案已被动过，
    //     中途失败会留下「旧答案被清掉」的半程态。兔维斯 不冒进，请终端人工换选）。
    //     改答同项（焦点=目标）与未答主路径不受影响。
    if entry.unanswered == 0 && f0 != target {
        return Err(StageAbort::screen(
            "codex 末题：本题已作答且焦点不在已选项——0.160.0 实测此形态下空格=反选旧答案而非换选（底料 §3 #5），远程代答会先破坏旧答案；已中止，未发任何键；请到终端人工换选",
        ));
    }

    // 1b. 目标越界卫语句（评审 P2-4）：target 来自手机卡载荷 index——载荷与屏上
    //     选项数不符（GET 后用户终端面板变化等）时，走位取模算术会静默算出错误
    //     步数，半程键已发才发现走错行。发键前零键中止。
    if target >= m {
        return Err(StageAbort::screen(format!(
            "codex 末题：目标选项序号 {target} 越界（屏上面板共 {m} 项）——已中止，未发任何键；载荷与终端面板可能不同步，请刷新后重试",
        )));
    }

    // 2. 最短环绕走位：down=(target+m-f0)%m，up=(f0+m-target)%m（m 选项数，双向环绕；
    //    相等取 j——底料未给「等距偏好」证据，j 是面板默认推进方向）
    let down = (target + m - f0) % m;
    let up = (f0 + m - target) % m;
    let walk_key = if up < down { "k" } else { "j" };
    let steps = down.min(up);
    let mut focused = f0;
    for _ in 0..steps {
        terminal.send(walk_key).map_err(|e| {
            StageAbort::delivery(format!(
                "INJECT_DELIVERY_FAIL: 走位键 {walk_key} 投递失败（{e}）"
            ))
        })?;
        sent_keys.push(walk_key.to_string());
        terminal.settle();
        // 每步复读核验到位（不批量盲发——§3.3 步 2：选中态若错，归零闸发现不了选错了谁）
        match terminal.read() {
            Some(lines) => match codex_question_screen_snapshot(&lines) {
                Some(s) => match s.focused {
                    Some(f) if f == target => {
                        focused = f;
                        break;
                    }
                    _ => {
                        focused = s.focused.unwrap_or(focused);
                        // 焦点未到目标——继续走（有界耗尽由外层 for + 下面的终判兜）
                    }
                },
                None => {
                    return Err(StageAbort::screen(
                        "codex 末题：走位中读屏失败或面板形态崩——已中止；请核对终端",
                    ))
                }
            },
            None => {
                return Err(StageAbort::screen(
                    "codex 末题：走位中读屏失败或面板形态崩——已中止；请核对终端",
                ))
            }
        }
    }
    if focused != target {
        // 有界（m+2）耗尽仍未到目标行——不猜不硬走
        return Err(StageAbort::screen(
            "codex 末题：走位有界耗尽仍未到目标行——已中止；请核对终端",
        ));
    }

    // 3. 已答特判收尾（评审 P1-2 的跳空格分支）：入场已归零 ∧ 焦点即目标（改答
    //    同项）→ 跳过空格（再按 = 反选，定案 3）；未答（k>0）主路径 → 发空格选中
    //    （改答不同项的分支已在 1a 中止，走不到这里）。
    let already_on_target_zero = entry.unanswered == 0 && f0 == target;
    if !already_on_target_zero {
        // 键名 "space" 走注入通道的既有映射（与 run_toggle_stages 同款；通道侧
        // 按 codex 底料 §2.3 落成字符形态 `-Char ' '`），记账用同一语义名
        terminal.send("space").map_err(|e| {
            StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 空格投递失败（{e}）"))
        })?;
        sent_keys.push("space".to_string());
        terminal.settle();
    }

    // 4. 归零闸（双验·有界轮询，评审 P2-9 同款修法）：单拍读屏可能落在未重绘旧帧
    //    ——不能一拍定生死。有界 4 拍：任一拍计数段消失（unanswered==0）**且**焦点
    //    仍在目标行（选中不改高亮位——若焦点漂了 = 可能反选了已选项，如实中止不发
    //    提交键）→ 过；窗尽或各拍全不符 → 原中止。
    let mut zero_ok = false;
    for _ in 0..4 {
        match poll().map_err(StageAbort::screen)? {
            Some(lines) => {
                if codex_question_screen_snapshot(&lines)
                    .is_some_and(|s| s.unanswered == 0 && s.focused == Some(target))
                {
                    zero_ok = true;
                    break;
                }
                // 未归零/焦点漂——下一拍再验（TUI 重绘竞态窗口）
            }
            None => break, // 窗尽
        }
    }
    if !zero_ok {
        return Err(StageAbort::screen(
            "codex 末题：空格后未归零或焦点漂移（可能反选了已选项）——已中止，未发提交键；请核对终端",
        ));
    }

    // 5. 双条件闸（提交，§3.3 步 6）：再读一屏——
    //    - 读不到屏（None）= 面板大概率已滚出/消失——保守按「形态不符」中止，不冒充
    //      已交卷（already_submitted 只留给「读到了屏且确认面板不在」的实证形态）；
    let second = terminal.read();
    let Some(lines) = second else {
        return Err(StageAbort::screen(
            "codex 末题：提交闸读不到屏——已中止，未发提交键；请人工核对终端",
        ));
    };
    let snap2 = codex_question_screen_snapshot(&lines);
    let Some(s2) = snap2 else {
        // 解析不到面板（评审 P1-2 三分派）：该分支拿到的是 lines（读到了屏）——
        // 「面板消失」的结论必须带终态证据，不能拿「读到屏但形态怪」冒充已交卷：
        // 1. 摘要头锚（QUESTION/RECEIPT，`• Questions n/n answered`）在场 →
        //    **已交卷实证**（外部提交如 notes 回车连带）→ already_submitted=true；
        // 2. 确认屏标题锚（QUESTION_REVIEW/TITLE）在场 → 前序动作已错位——
        //    中止引导人工（不代答不提交）；
        // 3. 两者皆无 → 面板异常消失——中止不发提交键（红色红线：不确定屏上
        //    是什么都发 enter = 可能整卷提交/误触别的对话框）。
        // sent_keys 原样返回（如实记账——若此前发过走位/空格，审计必须留痕）。
        // 摘要头判定用双词谓词（评审 P2-3：裸词 detect 对本槽过宽——正文任何
        // answered 单词都会误命中）。
        let lowered: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();
        if codex_receipt_present(&lines) {
            return Ok(CodexSelectOutcome {
                sent_keys: sent_keys.clone(),
                reanswered: false,
                already_submitted: true,
            });
        }
        if anchor_ledger::detect(
            &lowered,
            "codex",
            anchor_ledger::scenario::QUESTION_REVIEW,
            anchor_ledger::slot::TITLE,
        )
        .is_some()
        {
            return Err(StageAbort::screen(
                "codex：屏上是未答完确认屏——前序动作已错位，兔维斯 不代答；请到终端人工处置",
            ));
        }
        return Err(StageAbort::screen(
            "codex 末题：提交闸读不到问答面板且未见提交摘要（面板异常消失）——已中止，未发提交键；请人工核对终端",
        ));
    };
    //    - 提交闸谓词 = 归零闸四条件全验（评审 P1-1）：题号未漂移 ∧ 仍是末题 ∧
    //      计数段消失 ∧ 焦点仍在目标行。窗口期内用户 h/l 切走（k=0 在任何题都
    //      成立——只验归零挡不住切题）时 enter 会落在非末题上 = 选中+推进，
    //      绝不发键。
    if s2.question_idx != q_idx || !s2.is_last {
        return Err(StageAbort::screen(
            "codex 末题：提交闸发现屏上已切离本题/末题（等待窗口内终端被操作）——已中止，未发提交键；请核对终端",
        ));
    }
    if s2.unanswered != 0 || s2.focused != Some(target) {
        return Err(StageAbort::screen(
            "codex 末题：归零后计数又变化或焦点漂移——中止，未发提交键；请核对终端",
        ));
    }
    //    - 面板在场 ∧ 计数段消失 → 发回车显式提交整卷（全答完无确认，定案 5）
    terminal.send("enter").map_err(|e| {
        StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 提交回车投递失败（{e}）"))
    })?;
    sent_keys.push("enter".to_string());
    terminal.settle();

    // 6. 终态正向锚尽力核验（摘要头 `Questions n/n answered`）：命中与否只记
    //    debug 不翻供——摘要屏可能被后续输出刷出可见窗，读屏三态都不改变成功结论。
    //    判定用双词谓词（评审 P2-3：裸词 detect 对本槽过宽）。
    match poll() {
        Ok(Some(lines)) => {
            let hit = codex_receipt_present(&lines);
            log::debug!("codex select 终态锚核验：hit={hit}");
        }
        Ok(None) => {
            log::debug!("codex select 终态锚核验：窗尽未见摘要头（不翻供）");
        }
        Err(_) => {
            log::debug!("codex select 终态锚核验：读屏失败（不翻供）");
        }
    }
    Ok(CodexSelectOutcome {
        sent_keys: sent_keys.clone(),
        reanswered: false,
        already_submitted: false,
    })
}

// ============================================================
// Task 6：codex 切题 advance 阶段机（设计 §3.5）
// 取证依据：2026-10-09 底料 `docs/superpowers/specs/2026-10-08-codex-160-question-
// 屏读底料.md` §1 定案总表。**键序更正（底料 §1）：切题通道 = vim h/l（h=上一题
// l=下一题）——VT 箭头与真 VK 箭头对 codex 面板全部无效（S2 上真 VK_LEFT 未切题
// 实证）。切题环形（末→首、首→末）；单题上 h/l 无任何效果（S3 footer 无导航段，
// VK 左右零变化实证——h/l 同理）。切题后焦点落已选项、答案保留。
// ============================================================

/// codex 切题阶段机的结果（设计 §3.5 输出契约）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexAdvanceOutcome {
    /// 屏读核验题号已变（i → i'，含环形到达末题 l 回首题 i: n→1）——到达。
    Arrived,
    /// 题号未变（单题/窗内无效）或窗尽未见变化——如实未生效（dispatch 层转
    /// Failed「未生效，可重试」；不猜不中止，键已发出由用户决定是否重试）。
    NoEffect,
}

/// codex **切题 advance 阶段机**：发 h/l → 屏读核验题号变化（设计 §3.5）。
///
/// # 取证定案（2026-10-09 底料，实现注释留档）
/// 1. **切题通道 = vim h/l**（底料 §1 键序更正）：h=上一题、l=下一题——**不是
///    箭头键**（VT 箭头与真 VK 箭头对 codex 面板全部无效：S2 上真 VK_LEFT 发出
///    后屏零变化实证）。
/// 2. **切题环形**（底料 §1）：末题 l 回首题、首题 h 回末题——本函数**不裁剪**
///    方向，环形语义透传；到达判据 = question_idx 变化（`!=` before），故末题
///    l 回首题（i: n→1，即 1→0）同样被检测为 Arrived。
/// 3. **单题无导航**（底料 S3）：单题面板 footer 无 `←/→ to navigate questions`
///    导航段，h/l 无任何效果（S3 上 VK 左右零变化实证——h/l 同理）→ 屏解析成功
///    但 idx 未变 = NoEffect（如实未生效）。
/// 4. **切题后焦点落已选项、答案保留**——切题不产生作答副作用，无需计数闸。
///
/// # 流程（设计 §3.5 + 评审 P2-2 基线轮询）
/// 1. `terminal.read()` 首屏 → 解析面板存 baseline（`before` = 题号基线）；
///    读不到 = **零键中止**；
/// 2. 发键（Prev→"h" / Next→"l"，send → 记账 → settle，与既有阶段机同款三步）；
/// 3. **有界 4 拍变化轮询**（评审 P2-2：单拍判定可能落在未重绘陈旧帧上，同
///    select 侧 P0-1 反模式）：任一拍 idx != before → `Arrived`；屏 ≠ baseline
///    但 idx 未变（h/l 被吞但屏滚动了）→ 下一拍再看（窗内 idx 变了仍 Arrived，
///    全窗尽未变 → `NoEffect`）；屏原样 → 下一拍；`Ok(None)` 窗尽 → `NoEffect`
///    （现有语义）；解析不出面板 → 中止（形态崩）；`Err(e)` → 中止（读屏失败）。
///
/// # 参数
/// `poll()`——`Ok(Some(屏行))` = 动作后屏已就绪；`Ok(None)` = 轮询窗尽未见变化；
/// `Err` = 读屏失败。
pub fn run_codex_advance_stages<T: MenuTerminal>(
    direction: crate::inject::question::NavDirection,
    poll: impl Fn() -> Result<Option<Vec<String>>, String>,
    terminal: &mut T,
) -> Result<CodexAdvanceOutcome, StageAbort> {
    use crate::inject::question_screen_oc::codex_question_screen_snapshot;

    // ---------- 1. 入口闸：读首屏，解析面板 ----------
    // 读不到 = 零键中止（屏读为准：动手前必须见到面板）
    let first = terminal.read().ok_or_else(|| {
        StageAbort::screen(
            "INJECT_SCREEN_ABORT: codex 切题：读不到面板——已中止，未发任何键；请核对终端",
        )
    })?;
    let before = codex_question_screen_snapshot(&first)
        .ok_or_else(|| {
            StageAbort::screen(
                "INJECT_SCREEN_ABORT: codex 切题：读不到面板——已中止，未发任何键；请核对终端",
            )
        })?
        .question_idx;

    // ---------- 2. 发键：h=上一题 / l=下一题（底料 §1 键序更正，非箭头键）----------
    let key = match direction {
        crate::inject::question::NavDirection::Prev => "h",
        crate::inject::question::NavDirection::Next => "l",
    };
    terminal.send(key).map_err(|e| {
        StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: {key} 投递失败（{e}）"))
    })?;
    terminal.settle();

    // ---------- 3. 核验：有界变化轮询（评审 P2-2，同 select 侧 P0-1 修法）----------
    // 单拍判定可能落在未重绘的陈旧帧上（屏还是 baseline）——一拍 NoEffect 会把
    // 迟到位的切题误报「未生效」。有界 4 拍：
    // - 任一拍解析出 question_idx != before → Arrived（到达即返，含环形）；
    // - 屏面 ≠ baseline 但 idx 未变（变化却没切题——h/l 被吞但屏滚动了）→ 下一拍
    //   再看（4 拍内 idx 变了仍 Arrived；全窗尽 idx 未变 → NoEffect）；
    // - 屏面原样 == baseline → 下一拍（TUI 未重绘，不能下结论）；
    // - Ok(None)（窗尽）→ NoEffect（现有语义）；
    // - Err → 读屏失败中止；解析不出面板 → 形态崩中止（h/l 已发出，如实留痕）。
    const ADVANCE_POLL_ROUNDS: usize = 4;
    for _ in 0..ADVANCE_POLL_ROUNDS {
        match poll().map_err(StageAbort::screen)? {
            Some(lines) => {
                if let Some(s) = codex_question_screen_snapshot(&lines) {
                    // 屏解析成功且题号变了（含环形：末题 l 回首题）→ 到达
                    if s.question_idx != before {
                        return Ok(CodexAdvanceOutcome::Arrived);
                    }
                    if lines == first {
                        // 原样 == 基线：TUI 未重绘，不能下结论 → 下一拍
                        continue;
                    }
                    // 屏变了但题号没变（h/l 被吞但屏滚动了）→ 下一拍再看：
                    // 剩余拍内 idx 变了仍 Arrived，全窗尽未变 → 循环外 NoEffect
                    continue;
                }
                // 切题后面板形态崩（解析不出面板——屏上既无题号头也无 footer 锚）
                return Err(StageAbort::screen(format!(
                    "codex 切题：切题后面板形态崩（解析不出）——已中止（{key} 已发出）；请核对终端"
                )));
            }
            // 窗尽：窗内未见题号变化——如实未生效（不猜不冒充到达，也不中止）
            None => return Ok(CodexAdvanceOutcome::NoEffect),
        }
    }
    // 有界耗尽且每拍题号都未变——如实未生效（键已发出由用户决定是否重试）
    Ok(CodexAdvanceOutcome::NoEffect)
}

// ============================================================
// 批次戊 E6：opencode 多题/多选（N4；戊探A 全链定案）
// 键序 = tab 前向切页（题目页↔Confirm 页）/enter 或数字=toggle（空格禁用）
// /Confirm 页 enter 一次提交；own answer=enter 开输入行→打字→enter（裸打字守卫）
// ============================================================

/// opencode **Confirm 页锚**（戊探A 原件：footer `⇆ tab enter submit esc dismiss`——
/// 「enter submit」为 Confirm 页专属词形，题目页是 `enter toggle`）。
pub const OPENCODE_CONFIRM_FOOTER: &str = "enter submit";
/// opencode **提交完成锚**（提交后 transcript 摘要段 `# Questions`，戊探A E-A1 原件）。
///
/// **2026-09-23 起真源移入账本**（`opencode/question/receipt`）；取不到时回落
/// 已入账那一条。
pub fn opencode_answered_anchor() -> &'static str {
    crate::inject::anchor_ledger::candidates(
        "opencode",
        crate::inject::anchor_ledger::scenario::QUESTION,
        crate::inject::anchor_ledger::slot::RECEIPT,
    )
    .first()
    .map(|r| r.text)
    .unwrap_or("# questions")
}
/// opencode 自由作答行标签（`Type your own answer`；开启输入行后行下新增**无编号
/// 占位行**——「输入行已开启」的屏读判据）。
pub const OPENCODE_OWN_ANSWER_LABEL: &str = "type your own answer";

/// opencode Confirm 页在场判定（footer 锚）
pub fn opencode_confirm_present(lines: &[String]) -> bool {
    lines
        .iter()
        .any(|l| l.to_lowercase().contains(OPENCODE_CONFIRM_FOOTER))
}

/// opencode 提交完成在场判定
pub fn opencode_answered_present(lines: &[String]) -> bool {
    lines
        .iter()
        .any(|l| l.to_lowercase().contains(opencode_answered_anchor()))
}

/// opencode **own answer 输入行已开启**判定（裸打字守卫的判据面）：开行后行下新增
/// 一条**无编号**的 `Type your own answer` 占位行（戊探A E-A2：`4. Type your own
/// answer` 之下出现 `   Type your own answer`）——「带编号的行」是选项本身，
/// 「无编号的行」才是已开启的输入行。
pub fn opencode_own_answer_input_open(lines: &[String]) -> bool {
    lines.iter().any(|l| {
        // 剥掉框线（opencode 弹窗的 ┃ 边框）、引导符与空白后**以标签开头** = 占位行；
        // 选项行（`5. …` / `[ ] …`）虽含同串但开头是编号/复选框 → 排除
        let t = l
            .trim()
            .trim_start_matches(['\u{2503}', '\u{2502}', '\u{2192}', '\u{276f}'])
            .trim_start();
        let lower = t.to_lowercase();
        lower.starts_with(OPENCODE_OWN_ANSWER_LABEL)
            && !lower.contains("[ ]")
            && crate::inject::dialog::parse_option_line(t).is_none()
    })
}

/// opencode **编辑态已开启**判定（2026-10-05 双形态收口）：占位行（**1.x**：enter 后
/// 行下展开占位输入行）∨ **footer 词形**（**2.0.22+**：无占位行，footer 从
/// `enter toggle` 变 `enter done`——方言表 `edit_open_footer`，活体取证见该字段
/// 注释）。两个信号取**或**：1.x 屏走占位行、2.x 屏走 footer，互不误伤。
/// footer 匹配带**结构佐证**：词形所在行须同时含 `esc`（footer 行族特征）——防
/// 题干/选项文本里恰好引用同词形（锚点账本的短锚纪律）。
pub fn opencode_own_answer_edit_open(
    lines: &[String],
    d: &crate::inject::dialect::OwnAnswerDialect,
) -> bool {
    if opencode_own_answer_input_open(lines) {
        return true;
    }
    footer_signal_present(lines, d.edit_open_footer)
}

/// **高亮停在 own answer 行**判定（2.0.22 footer 信号，活体取证：高亮在 own 行时
/// footer = `… enter edit …`）。信号表为空（1.x 未取证）→ 恒 false（盲走兼容）。
pub fn opencode_own_row_highlighted(
    lines: &[String],
    d: &crate::inject::dialect::OwnAnswerDialect,
) -> bool {
    footer_signal_present(lines, d.edit_highlight_footer)
}

/// **选项勾选态翻转检测**（enter 探针走位的还原判据，2026-10-05 活体取证支撑）：
/// 比较两屏的编号行 1..own_pos（**不含 own 行**——own 行翻转是命中信号不是事故）
/// 的勾选态，任一行翻转 = enter 落在了选项行。行集缺行（折行/重绘抖动）→ 该行
/// 不参与比较（宁漏检不误检——误检会把还原键打在真选项上）。
fn opencode_option_flip(
    a: &[String],
    b: &[String],
    own_pos: usize,
    d: &crate::inject::dialect::OwnAnswerDialect,
) -> bool {
    fn strip(l: &str) -> &str {
        l.trim()
            .trim_start_matches(['\u{2503}', '\u{2502}', '\u{2192}', '\u{276f}'])
            .trim_start()
    }
    let states = |lines: &[String]| {
        let mut m = std::collections::HashMap::new();
        for l in lines {
            let t = strip(l);
            if let Some((n, _, _)) = crate::inject::dialog::parse_option_line(t) {
                let num = n as usize;
                if num >= 1 && num < own_pos {
                    let lower = t.to_lowercase();
                    let checked = d.checked_markers.iter().any(|mk| lower.contains(mk));
                    m.insert(num, checked);
                }
            }
        }
        m
    };
    states(a) != states(b)
}

/// footer 词形在场判定（小写 contains + `esc` 行族佐证；空表 → 恒 false）
fn footer_signal_present(lines: &[String], signals: &[&str]) -> bool {
    if signals.is_empty() {
        return false;
    }
    lines.iter().any(|l| {
        let lower = l.to_lowercase();
        lower.contains("esc") && signals.iter().any(|sig| lower.contains(sig))
    })
}

/// opencode **单选页 own 行内容提取**（按编号直接定位）：单选页 footer
/// `enter submit` 与多选确认页同词形，`opencode_own_answer_row_state` 的页锚门
/// 会拒绝定位——单选流程专用本助手，直接找第 own_pos 号行取内容。
/// **单选页切片**（2026-10-06 GET 断链定案）：题页/确认页对话框首行恒为
/// `Questions`——会话 transcript 里模型的「1. … 2. …」编号说明文字会被全窗
/// 扫描误当选项/own 行（18:03-18:27 GET 结构解析全灭 + Review 页 own 行消失
/// 判定永不触发的共同根因）。从**最后一个** `Questions` 行之后切片 = 只看
/// 活对话框区。锚不在场（旧夹具/异常形态）→ None（调用方回落全窗，行为
/// 与既有一致，不破坏兼容）。
pub(crate) fn opencode_question_page_slice(lines: &[String]) -> Option<&[String]> {
    lines
        .iter()
        .rposition(|l| {
            l.trim()
                .trim_start_matches(['\u{2503}', '\u{2502}', '\u{2192}', '\u{276f}'])
                .trim()
                == "Questions"
        })
        .map(|i| &lines[i + 1..])
}

pub(crate) fn opencode_single_own_row(lines: &[String]) -> Option<(usize, String)> {
    // own 行恒为**最后一个编号行**（几何事实，不依赖内容）——残留态（标签已被
    // 用户文字替换）按标签找必然失败，故直接取最后编号行；**只看页切片**
    // （transcript 编号行污染防御，见 opencode_question_page_slice）
    let lines = opencode_question_page_slice(lines).unwrap_or(lines);
    let mut last: Option<(usize, String)> = None;
    for l in lines {
        let t = l
            .trim()
            .trim_start_matches(['\u{2503}', '\u{2502}', '\u{2192}', '\u{276f}']);
        if let Some((n, text, _)) = crate::inject::dialog::parse_option_line(t) {
            last = Some((n as usize, text));
        }
    }
    last
}

/// 按屏上编号取该行内容（单选页落行/提交验证用——打字后标签已被替换，不能
/// 再按标签定位）
pub(crate) fn opencode_single_own_row_content_at(
    lines: &[String],
    own_pos: usize,
) -> Option<String> {
    // 只看页切片（transcript 编号行污染防御，见 opencode_question_page_slice）
    let lines = opencode_question_page_slice(lines).unwrap_or(lines);
    for l in lines {
        let t = l
            .trim()
            .trim_start_matches(['\u{2503}', '\u{2502}', '\u{2192}', '\u{276f}']);
        if let Some((n, text, _)) = crate::inject::dialog::parse_option_line(t) {
            if n as usize == own_pos {
                return Some(text);
            }
        }
    }
    None
}

/// opencode **指定编号行的勾选态**（toggle 后置翻转校验用，2026-10-05 屏读
/// 标准补齐）：找到第 num 号编号行 → 勾选标记判定；行不在场 → None。
pub(crate) fn opencode_option_checked_at(
    lines: &[String],
    num: usize,
    d: &crate::inject::dialect::OwnAnswerDialect,
) -> Option<bool> {
    // 只看页切片（transcript 编号行污染防御，见 opencode_question_page_slice）
    let lines = opencode_question_page_slice(lines).unwrap_or(lines);
    for l in lines {
        let t = l
            .trim()
            .trim_start_matches(['\u{2503}', '\u{2502}', '\u{2192}', '\u{276f}']);
        if let Some((n, _, _)) = crate::inject::dialog::parse_option_line(t) {
            if n as usize == num {
                let lower = t.to_lowercase();
                if d.checked_markers.iter().any(|mk| lower.contains(mk)) {
                    return Some(true);
                }
                // **单选页裸 ✓**（2026-10-06 活体定案，PID 1880 dump 行35
                // 「2. 两篇悬疑小说 ✓」）：2.0.22 单选页**选中行尾渲染 " ✓"**
                // （U+2713，无括号标记）——行尾裸 ✓ = 该行是当前答案（选项/
                // own 行通用）。旧「单选无字符标记、纯高亮不可读」定案作废。
                // 未选行 = 无任何标记 → Some(false)（与括号页同口径，翻转
                // 判定 Some(false)→Some(true) 自然成立）。
                if t.trim_end().ends_with('✓') {
                    return Some(true);
                }
                return Some(false);
            }
        }
    }
    None
}

/// codex **高亮行定位**（C7 定案：`? ` 前缀 = 题页选项行首的唯一可读高亮）：
/// 找到带 `? ` 前缀的编号行 → 其屏上编号；无 `? ` 行（弹窗不在场/形态漂移）→
/// None。
pub(crate) fn codex_highlight_row(lines: &[String]) -> Option<usize> {
    for l in lines {
        let t = l
            .trim()
            .trim_start_matches(['\u{2503}', '\u{2502}', '\u{2192}', '\u{276f}']);
        if let Some(rest) = t.strip_prefix("? ") {
            if let Some((n, _, _)) = crate::inject::dialog::parse_option_line(rest) {
                return Some(n as usize);
            }
        }
    }
    None
}

/// opencode 页面**是否带勾选框**（单选/多选页判别，2026-10-05）：任一编号行含
/// 方言勾选标记（`[ ]`/`[✓]` 等）= 多选形态页；全部无标记 = **单选形态页**。
/// 单选页语义与多选完全不同（↓ 即输入、enter=整题提交、无勾选翻转信号），
/// 多选专属的探针/补勾机制在单选页必须全部绕行。
fn opencode_page_has_checkboxes(
    lines: &[String],
    d: &crate::inject::dialect::OwnAnswerDialect,
) -> bool {
    lines.iter().any(|l| {
        let t: String = l
            .trim()
            .trim_start_matches(['\u{2503}', '\u{2502}', '\u{2192}', '\u{276f}'])
            .to_string();
        let lower = t.to_lowercase();
        let numbered = crate::inject::dialog::parse_option_line(t.trim()).is_some();
        numbered
            && (d.checked_markers.iter().any(|mk| lower.contains(mk))
                || d.unchecked_markers.iter().any(|mk| lower.contains(mk)))
    })
}

/// codex **发数字前备注编辑器守卫**（2026-10-05）：notes 编辑器（footer
/// `tab or esc to clear notes`，探测批 C 词形）开启时数字会被吃进备注文本——
/// 检出即拒绝（备注归备注阶段机独占管理；自动 enter 提交可能发出用户未写完
/// 的备注、esc 会清空备注，都不替用户做主）。读屏失败 → 放行（保持既有盲发
/// 行为——守卫只在有证据时收窄）。
pub(crate) fn codex_ensure_notes_closed(lines: Option<Vec<String>>) -> Result<(), StageAbort> {
    let Some(lines) = lines else {
        return Ok(());
    };
    let notes_open = lines
        .iter()
        .any(|l| l.to_lowercase().contains(CODEX_NOTES_OPENED_FOOTER));
    if notes_open {
        return Err(StageAbort::screen(
            "焦点守卫：备注编辑器仍开启（footer「tab or esc to clear notes」在场）——数字会被写进备注；请先在终端完成（enter 提交）或清除（esc）备注后再作答选项",
        ));
    }
    Ok(())
}

/// opencode **多选提交阶段机**：tab 切页 → Confirm 页 → enter 一次提交全部答案
/// （戊探A ④：Confirm 页 enter 提交 ×3 全通）。未答题的 Confirm 页显示
/// `(not answered)`（戊探A E-A2 顺带实证）——是否可提交未测，阶段机不判。
///
/// **首段「Confirm 已在场则跳过 tab」**（2026-09-23 错位修复的交互兼容）：手机端
/// 「切换题目」按钮已把终端切到 Confirm 页后，再发 tab 会**回绕到题目页**（戊探A ①
/// 前向循环）把提交屏切走——故先读一屏，footer 锚已命中就直接 enter。与
/// [`run_kimi_submit_stages`] 首段「Review 已在场跳过 tab」同款防御。读不到屏 →
/// 中止零按键（不盲发）。
pub fn run_opencode_submit_stages<Rd, P, Q, T>(
    mut read: Rd,
    mut poll_confirm: P,
    mut poll_receipt: Q,
    terminal: &mut T,
) -> Result<SubmitOutcome, StageAbort>
where
    Rd: FnMut() -> Option<Vec<String>>,
    P: FnMut() -> Result<Option<Vec<String>>, String>,
    Q: FnMut() -> Result<Option<Vec<String>>, String>,
    T: MenuTerminal,
{
    let mut sent_keys: Vec<String> = Vec::new();
    // 1. Confirm 已在场判读（读不到屏 → 中止零按键）
    let initial = read().ok_or_else(|| {
        StageAbort::screen(
            "INJECT_SCREEN_ABORT: opencode 提交：读不到屏幕——已中止，未发任何键；请人工核对终端",
        )
    })?;
    // 2. 不在 Confirm 页 → tab 切页（题目页 → Confirm 页；前向循环，戊探A ①）
    if !opencode_confirm_present(&initial) {
        terminal.send("tab").map_err(|e| {
            StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: tab 投递失败（{e}）"))
        })?;
        sent_keys.push("tab".to_string());
        terminal.settle();
    }
    // 3. Confirm 页（未见 → 中止，不发提交键）
    let confirm = poll_confirm().map_err(StageAbort::screen)?.ok_or_else(|| {
        StageAbort::screen(format!(
            "tab 后未出现 Confirm 页（未见「{OPENCODE_CONFIRM_FOOTER}」footer）——已中止，未发提交键；请人工核对终端"
        ))
    })?;
    if !opencode_confirm_present(&confirm) {
        return Err(StageAbort::screen(
            "Confirm 页形态不符——已中止，未发提交键；请人工核对终端",
        ));
    }
    // 4. enter 提交（Confirm 页唯一回车点）
    terminal.send("enter").map_err(|e| {
        StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 提交回车投递失败（{e}）"))
    })?;
    sent_keys.push("enter".to_string());
    terminal.settle();
    // 5. 终态（未见不是失败）
    let receipt_seen = stage_receipt_seen(&mut poll_receipt, opencode_answered_present);
    Ok(SubmitOutcome {
        sent_keys,
        down_steps: 0,
        up_steps: 0,
        review_confirmed: true,
        receipt_seen,
    })
}

/// opencode **own answer 行状态解析**（纯函数）：返回 `(1 起行序, 是否已勾选,
/// 已保存文本)`。定位优先带标签形态（`Type your own answer` 行——鲜态），回退 =
/// **最后一个编号行**（已保存态：标签被保存内容替换，戊探A 形态 + 用户 2026-10-04
/// 实测「own answer 行显示已存文本」）；勾选判据 = 行内 `[v]`/`[x]`/`[✓]` 标记；
/// 已保存文本 = 勾选标记 `]` 之后的内容（等于标签 → 视为无保存，返回空串）。
fn opencode_own_answer_row_state(
    lines: &[String],
    d: &crate::inject::dialect::OwnAnswerDialect,
) -> Option<(usize, bool, String)> {
    fn strip(l: &str) -> &str {
        l.trim()
            .trim_start_matches(['\u{2503}', '\u{2502}', '\u{2192}', '\u{276f}'])
            .trim_start()
    }
    let label_hit = |t: &str| t.to_lowercase().contains(d.label);
    let checked_of = |t: &str| {
        let lower = t.to_lowercase();
        d.checked_markers.iter().any(|m| lower.contains(m))
    };
    // 文本抽取：`N. [v] 内容` / `[v] 内容` → 最后一个 `]` 之后
    let text_after_marker = |t: &str| -> String {
        match t.rfind(']') {
            Some(i) => t[i + 1..].trim_start().to_string(),
            None => String::new(),
        }
    };
    // ① 标签行（鲜态）
    for (idx, l) in lines.iter().enumerate() {
        let t = strip(l);
        if label_hit(t) && crate::inject::dialog::parse_option_line(t).is_some() {
            let saved = text_after_marker(t);
            let saved = if saved.to_lowercase().contains(d.label) {
                String::new()
            } else {
                saved
            };
            let pos = crate::inject::dialog::parse_option_line(t)
                .map(|(n, _, _)| n as usize)
                .unwrap_or(idx + 1);
            return Some((pos, checked_of(t), saved));
        }
    }
    // ② 回退：最后一个编号行（已保存态——标签已被内容替换）。**门 = 题页锚**
    // （footer `enter toggle`）：无锚的屏（别家格式/非题屏）不得靠回退定位成功——
    // 否则「无 own answer 锚 → 定位即中止零投递」的契约被击穿（回归锁
    // question_free_text_refused_for_unverified_tools 实测抓获）
    if !lines.iter().any(|l| {
        l.to_lowercase()
            .contains(crate::inject::question_screen_oc::OPENCODE_QUESTION_PAGE_ANCHOR)
    }) {
        return None;
    }
    let mut last: Option<(usize, String)> = None;
    for l in lines {
        let t = strip(l);
        if let Some((n, _, _)) = crate::inject::dialog::parse_option_line(t) {
            last = Some((n as usize, t.to_string()));
        }
    }
    let (pos, t) = last?;
    let saved = text_after_marker(&t);
    Some((pos, checked_of(&t), saved))
}

/// opencode **own answer 自由作答阶段机**（2026-10-04 按**用户实测语义**重写：
/// enter = **toggle**（footer 锚 `enter toggle` + 用户活体测试）——
///
/// - own answer 行**未勾选**（鲜态）：enter ×1 → 勾选上 + 进入编辑态；
/// - own answer 行**已勾选**（有保存内容）：enter #1 = **取消勾选**、enter #2 =
///   重新勾上 + 进入编辑态（**双 enter**），光标落在已存文本**末尾**；
/// - 编辑态打字 = 在光标处插入（末尾 = 追加）；再 enter = **保存**（锁定态，
///   方向键不再影响编辑）——推进到下一题/提交走既有 tab（OpencodeSubmit / 切题按钮），
///   **不在本阶段机内**。
///
/// 编排：↓ 定位（按屏上行序，从**首行**起算——弹窗打开时高亮在首选项，戊探A E-A2；
/// **2.0.22 活体复验仍成立**：↓×5 从首行直达 own 行，2026-10-05 自建会话实测）
/// → 按 `checked` 分支 enter（已勾选时两段 enter 各带屏读复核，不盲发第二键）→
/// **裸打字守卫**（[`opencode_own_answer_edit_open`] 三信号：占位行〔1.x〕/ footer
/// 词形〔2.0.22+ `enter done`〕/ own 行勾选翻转——**2.0.22 无占位行**，只认占位行
/// 会假阴性中止并把 own 行留在已勾残局，2026-10-05 实机回归根因；未开启绝不发文本
/// ——裸打字会被当导航/勾选指令：'l' 切页、'2' 勾选，戊探A §4 意外事件归因）→
/// `overwrite` 时先 backspace × 已存文本长度（光标在末尾 → 逐个回删清空）→ 打字
/// （字符通道）→ enter 保存。
///
/// **位置假设的边界（如实申报）**：opencode 高亮在字符层不可见（属性级 0x0003，
/// 兔维斯 屏读不携带）→ 无法闭环验证当前高亮位。本阶段机只对「弹窗刚打开、未做过
/// 任何选择」的鲜态安全（高亮=首行）；若用户已在终端手动移动过高亮，↓×(n-1) 会
/// 停在错误的行——前置判据无法消除该风险，失败模式是「答错行」而非「卡死」，
/// 终态锚核验如实回执，残余风险登记台账。已保存文本按**单行可见长度**计量
/// （折行内容读不到 → 清空可能残留尾部）——登记已知边界。
/// own answer 编排内的**单发回车**（enter 投递 + 键账 + settle）——两处调用共用
/// （首键与双 enter 语义的补键）。
fn oc_send_enter<T: FreeTextTerminal>(
    terminal: &mut T,
    sent_keys: &mut Vec<String>,
) -> Result<(), StageAbort> {
    terminal.send("enter").map_err(|e| {
        StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 开输入行回车投递失败（{e}）"))
    })?;
    sent_keys.push("enter".to_string());
    terminal.settle();
    Ok(())
}

pub fn run_opencode_own_answer_stages<Q, T>(
    text: &str,
    overwrite: bool,
    mut poll_receipt: Q,
    terminal: &mut T,
) -> Result<FreeTextOutcome, StageAbort>
where
    Q: FnMut() -> Result<Option<Vec<String>>, String>,
    T: FreeTextTerminal,
{
    if text.trim().is_empty() && !overwrite {
        return Err(StageAbort::screen(
            "自由作答文本为空（清空请用覆盖写入）——已中止，未发任何键",
        ));
    }
    // 0. 方言表（2026-10-05 推广批 F2）：定位/勾选标记/编辑门/光标全部读表——
    // 差异=数据；表无条目（未定案工具）→ 拒绝出手
    let d = crate::inject::dialect::own_answer_dialect("opencode").ok_or_else(|| {
        StageAbort::screen("INJECT_SCREEN_ABORT: 方言表无该工具条目——已中止，未发任何键")
    })?;
    let mut sent_keys: Vec<String> = Vec::new();
    // 1. 定位 own answer 行 + 勾选态 + 已存文本
    let first = terminal.read().ok_or_else(|| {
        StageAbort::screen("INJECT_SCREEN_ABORT: opencode 自由作答：读不到屏幕——已中止，未发任何键；请人工核对终端")
    })?;
    // ===== 单选页 N 探针流程（2026-10-05 用户设计定案）=====
    //
    // opencode 数字键**作用于编号行、与高亮无关**（用户实操验证）——
    // 按 own 行编号 N 就一定作用到 own 行，不需要盲走定位。
    //
    // 流程：按数字 N → 读 own 行差分 → 分类处置（有界重试）：
    // a. 文字格里出现 N → 焦点已在文字行/编辑态 → 退格清 N → 打全文 → ↑ 保存
    // b. own 行被勾上（勾选翻转 [ ]→[✓]）→ 焦点确认在 own 行 → 打全文 → enter
    // c. 其他行翻转了（N 被选项行吃了——防御）→ 退格还原 → 换位 → 重试
    // d. 无变化 → 换位 → 重试
    // 有界 = own_num + 3 轮（覆盖全部行位置含回绕）。走满报错。
    if !opencode_page_has_checkboxes(&first, &d) {
        let (own_num, old_text) = opencode_single_own_row(&first).ok_or_else(|| {
            StageAbort::screen(
                "屏读未定位到 Type your own answer 行——已中止，未发任何键；请人工核对终端",
            )
        })?;
        let residue = if old_text.to_lowercase().contains(d.label) {
            String::new()
        } else {
            old_text.clone()
        };
        if !residue.is_empty() && !overwrite {
            return Err(StageAbort::screen(
                "该项已有内容（直接发送会追加）——请用「覆盖写入」替换后发送",
            ));
        }
        let n_key = format!("{}", own_num);
        let mut sent: Vec<String> = Vec::new();
        for _round in 0..=(own_num + 3) {
            // 按数字 N（到达探针 + 选中二合一）
            terminal.send(&n_key).map_err(|e| {
                StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: N 探针投递失败（{e}）"))
            })?;
            sent.push(n_key.clone());
            terminal.settle();
            let after = terminal.read().ok_or_else(|| {
                StageAbort::screen(
                    "INJECT_SCREEN_ABORT: N 探针后读不到屏幕——已中止；请人工核对终端",
                )
            })?;
            // own 行消失 = 终端已推进（Submit 总结页）→ 收工
            let Some(cur_content) = opencode_single_own_row_content_at(&after, own_num) else {
                log::info!("[single-select] N 探针后 own 行消失 = 已推进");
                let receipt_seen = stage_receipt_seen(&mut poll_receipt, opencode_answered_present);
                return Ok(FreeTextOutcome {
                    sent_keys: sent,
                    receipt_seen,
                    advanced: false,
                    review_reached: false,
                    screen_text: if text.is_empty() {
                        None
                    } else {
                        Some(text.to_string())
                    },
                    screen_checked: None,
                });
            };
            // **文字格里出现了 N**（焦点在文字行，N 被打进去）→
            // 退格清 N → 确认焦点在文字行 → 打全文 → 保存 → 完成
            // 保存键按**字段余量分派**（2026-10-06 用户实测定案，范围仅单选
            // 打字流）：有内容 → ↑（保存且不触发选择/即交）；空（清空路径
            // 退格后）→ enter（空字段单纯 ↑ 不保存不退出；空字段 enter 无
            // 内容即交连锁）。enter 对**有内容**字段=保存+确认连锁——00:52/
            // 00:54 两起打字后整卷异常提交即此根源（多选页双 enter 舞步不受
            // 影响，勿扩大化）。
            if cur_content.ends_with(&n_key) && cur_content != old_text {
                log::info!(
                    "[single-select] N 写进文字格 = 焦点在文字行 → 清 N{}打全文",
                    if overwrite && !residue.is_empty() {
                        "清残留"
                    } else {
                        ""
                    }
                );
                terminal.send("backspace").map_err(|e| {
                    StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 退格投递失败（{e}）"))
                })?;
                sent.push("backspace".to_string());
                terminal.settle();
                // **覆盖写入/清空：复用多选语义**（1754 行同款，2026-10-06 02:44
                // 事故定案）：探针字符删掉后光标停在**残留文本末尾** → backspace
                // × 可见长度逐个回删清空，再打新文（text 空 = 纯清空）。此前只删
                // 探针字符 → 新文全部追加在残留后（Review「…×2mmm」实机）。
                // 残留计数剥 ✓ 尾——对勾是渲染标记不是键入字符，计入会多删。
                if overwrite && !residue.is_empty() {
                    let n = residue
                        .trim_end()
                        .trim_end_matches('✓')
                        .trim_end()
                        .chars()
                        .count()
                        .min(400);
                    for _ in 0..n {
                        terminal.send("backspace").map_err(|e| {
                            StageAbort::delivery(format!(
                                "INJECT_DELIVERY_FAIL: 退格投递失败（{e}）"
                            ))
                        })?;
                    }
                    sent.push(format!("<backspace×{n}>"));
                    terminal.settle();
                }
                if !text.is_empty() {
                    terminal.send_text(text).map_err(|e| {
                        StageAbort::delivery(format!(
                            "INJECT_DELIVERY_FAIL: 作答文本投递失败（{e}）"
                        ))
                    })?;
                    sent.push("<text>".to_string());
                    terminal.settle();
                    // 有内容 → ↑ 保存（2026-10-06 用户实测定案：字段**非空**时
                    // ↑=保存且不触发选择/即交；enter 在单选页=保存+确认连锁）
                    terminal.send("up").map_err(|e| {
                        StageAbort::delivery(format!(
                            "INJECT_DELIVERY_FAIL: 保存上箭头投递失败（{e}）"
                        ))
                    })?;
                    sent.push("up".to_string());
                } else {
                    // 清空路径：退格后字段为**空** → 单纯 ↑ 不保存（2026-10-06
                    // 实测细化：空字段 ↑ 空转）→ enter 提交空答案=保存/退出
                    terminal.send("enter").map_err(|e| {
                        StageAbort::delivery(format!(
                            "INJECT_DELIVERY_FAIL: 清空回车投递失败（{e}）"
                        ))
                    })?;
                    sent.push("enter".to_string());
                }
                terminal.settle();
                let receipt_seen = stage_receipt_seen(&mut poll_receipt, opencode_answered_present);
                return Ok(FreeTextOutcome {
                    sent_keys: sent,
                    receipt_seen,
                    advanced: false,
                    review_reached: false,
                    screen_text: if text.is_empty() {
                        None
                    } else {
                        Some(text.to_string())
                    },
                    screen_checked: None,
                });
            }
            // **own 行被勾上/摘勾**（N 选中了 own 行，勾选翻转）→
            // 焦点确认在 own 行 → 打全文 → enter 提交 → 完成（此分支保持 enter，
            // 2026-10-06 用户裁决：↑ 保存特例仅限文字格分支，勿扩大化）
            let after_checked = opencode_option_checked_at(&after, own_num, &d).unwrap_or(false);
            let before_checked = opencode_option_checked_at(&first, own_num, &d).unwrap_or(false);
            if after_checked != before_checked {
                log::info!("[single-select] N 翻转了 own 行勾选 → 焦点在 own 行 → 打全文");
                if !text.is_empty() {
                    terminal.send_text(text).map_err(|e| {
                        StageAbort::delivery(format!(
                            "INJECT_DELIVERY_FAIL: 作答文本投递失败（{e}）"
                        ))
                    })?;
                    sent.push("<text>".to_string());
                    terminal.settle();
                }
                terminal.send("enter").map_err(|e| {
                    StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 提交回车投递失败（{e}）"))
                })?;
                sent.push("enter".to_string());
                terminal.settle();
                let receipt_seen = stage_receipt_seen(&mut poll_receipt, opencode_answered_present);
                return Ok(FreeTextOutcome {
                    sent_keys: sent,
                    receipt_seen,
                    advanced: false,
                    review_reached: false,
                    screen_text: if text.is_empty() {
                        None
                    } else {
                        Some(text.to_string())
                    },
                    screen_checked: None,
                });
            }
            // **其他行翻转**（N 被选项行吃了——防御）→ 退格还原 → 换位
            let mut other_flipped = false;
            for k in 1..own_num {
                let b = opencode_option_checked_at(&first, k, &d);
                let a = opencode_option_checked_at(&after, k, &d);
                if b != a {
                    other_flipped = true;
                    break;
                }
            }
            if other_flipped {
                // 还原翻转的选项
                terminal.send(&n_key).map_err(|e| {
                    StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 还原投递失败（{e}）"))
                })?;
                sent.push(n_key.clone());
                terminal.settle();
            }
            // 换位（↓）
            terminal.send("down").map_err(|e| {
                StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 下箭头投递失败（{e}）"))
            })?;
            sent.push("down".to_string());
            terminal.settle();
        }
        return Err(StageAbort::screen(
            "N 探针走满预算仍未将文字送达 own answer 行——已中止，未发提交键；请人工核对终端",
        ));
    }

    if opencode_own_answer_edit_open(&first, &d) {
        return Err(StageAbort::screen(
            "编辑态似乎已开启（先前未完成的作答？占位行或 enter done footer 在场）——为防文本误入导航，已中止；请人工核对终端",
        ));
    }
    let (own_pos, was_checked, saved_text) =
        opencode_own_answer_row_state(&first, &d).ok_or_else(|| {
            StageAbort::screen(
                "屏读未定位到 Type your own answer 行——已中止，未发任何键；请人工核对终端",
            )
        })?;
    if !saved_text.is_empty() && !overwrite {
        return Err(StageAbort::screen(
            "该项已有保存内容（直接发送会追加）——请用「覆盖写入」替换后发送",
        ));
    }
    // 2. 定位并进入编辑态（**按入口状态分派两策略**，2026-10-05 深夜实测定案）：
    //
    // **已存文本（覆盖写入/清空的入口形态）→ enter 探针走位**：2.0.22 活体取证——
    // 高亮停在**已存文本 own 行**时 footer 与选项行**同词形**（`enter toggle`），
    // `enter edit` 词形只在鲜态出现 → footer 寻位对已存行**天然失效**（走满预算
    // 回绕、回车勾错选项行——用户实机「覆盖→第5题/清空→第4题」根因）。改逐位
    // **enter 探测**：enter 命中 own 行 = 勾选+进编辑（footer `enter done` 在场，
    // 实测与鲜态同语义）；落在选项行 = 勾选翻转（**立即再 enter 还原**，翻转可
    // 观测可还原，实测）→ ↓ 继续。预算 own_pos 步（可导航行数 ≤ own_pos，含回绕
    // 必达）；还原键意外命中 own 行（漂移）也接受为到位。
    //
    // **鲜态（无保存文本）→ footer 闭环寻位**（既有，实测）：每步 ↓ 前读 footer，
    // 见 `enter edit` 即停（1.x 无词形 → 走满旧步数 = 盲走零回归）。
    // edit_ready 的部分赋值分支（快路径到达即落入打字段）是刻意的控制流——
    // 值在退化路径 1788/1849 处被读取；clippy 的跨分支数据流误报在此豁免
    #[allow(unused_assignments)]
    let mut edit_ready = false;
    if !saved_text.is_empty() {
        // ===== 快路径（2026-10-05 用户提案）：假定高亮在首行（弹窗常态）→ 盲走
        // ↓×(pos-1) 直达 own 行 → **单次探针验证**。中间行零打扰（无逐行闪烁）。
        // 验证未命中（光标被手动挪过）→ 还原探针 → 退化到逐行探针走位（自愈，
        // 有界）。探针语义（18:12 日志定案）：未勾行=勾上+编辑器展开；已勾行=
        // 纯摘勾不开编辑器（→ 再 enter 勾回+编辑器展开）。
        for _ in 0..own_pos.saturating_sub(1) {
            terminal.send("down").map_err(|e| {
                StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 下箭头投递失败（{e}）"))
            })?;
            sent_keys.push("down".to_string());
            terminal.settle();
        }
        let cur = terminal.read().ok_or_else(|| {
            StageAbort::screen(
                "INJECT_SCREEN_ABORT: 快路径走位后读不到屏幕——已中止，未发任何键；请人工核对终端",
            )
        })?;
        if opencode_own_answer_edit_open(&cur, &d) {
            // 已在编辑态（先前未完成的作答）→ 直接到打字段
        } else {
            oc_send_enter(terminal, &mut sent_keys)?;
            let after = terminal.read().ok_or_else(|| {
                StageAbort::screen(
                    "INJECT_SCREEN_ABORT: 快路径探针后读不到屏幕——已中止；请人工核对终端",
                )
            })?;
            if opencode_own_answer_edit_open(&after, &d) {
                // 未勾行：勾上 + 编辑器展开 → 到达（直接进入打字段）
            } else {
                let (_, after_checked, _) =
                    opencode_own_answer_row_state(&after, &d).ok_or_else(|| {
                        StageAbort::screen(
                            "快路径探针后 own answer 行消失（屏面可能已切换）——已中止；请人工核对终端",
                        )
                    })?;
                if was_checked && !after_checked {
                    // 已勾行：探针纯摘勾（无编辑器）→ 再 enter 勾回+编辑器展开 → 到达
                    oc_send_enter(terminal, &mut sent_keys)?;
                    let reopened = terminal.read().ok_or_else(|| {
                        StageAbort::screen(
                            "INJECT_SCREEN_ABORT: 快路径勾回键后读不到屏幕——已中止；请人工核对终端",
                        )
                    })?;
                    if !opencode_own_answer_edit_open(&reopened, &d) {
                        return Err(StageAbort::screen(
                            "快路径勾回未展开编辑态（形态与实测语义不符）——已中止，未发文本；请人工核对终端",
                        ));
                    }
                    // 勾回+编辑器展开 → 到达（直接进入打字段）
                } else {
                    // 未命中（光标不在首行）：探针若翻转了选项 → 还原，然后退化走位
                    if opencode_option_flip(&cur, &after, own_pos, &d) {
                        oc_send_enter(terminal, &mut sent_keys)?;
                        let restored = terminal.read().ok_or_else(|| {
                            StageAbort::screen(
                                "快路径探针还原键后读不到屏幕——已中止；请人工核对终端",
                            )
                        })?;
                        if opencode_own_answer_edit_open(&restored, &d) {
                            edit_ready = true; // 还原键意外命中 own 行 → 接受为到位
                        } else if opencode_option_flip(&cur, &restored, own_pos, &d) {
                            return Err(StageAbort::screen(
                                "快路径探针还原失败（选项勾选态未复原）——已中止，未发文本；请人工核对终端",
                            ));
                        }
                    }
                    if !edit_ready {
                        // ===== 退化路径：逐行探针走位（自愈，有界）=====
                        for _ in 0..=(own_pos + 1) {
                            let cur = terminal.read().ok_or_else(|| {
                                StageAbort::screen(
                                    "探针走位中读不到屏幕——已中止，未发任何键；请人工核对终端",
                                )
                            })?;
                            if opencode_own_answer_edit_open(&cur, &d) {
                                edit_ready = true;
                                break;
                            }
                            oc_send_enter(terminal, &mut sent_keys)?;
                            let after = terminal.read().ok_or_else(|| {
                                StageAbort::screen("INJECT_SCREEN_ABORT: 探针回车后读不到屏幕——已中止；请人工核对终端")
                            })?;
                            if opencode_own_answer_edit_open(&after, &d) {
                                edit_ready = true;
                                break;
                            }
                            let (_, own_now_checked, _) =
                                opencode_own_answer_row_state(&after, &d).ok_or_else(|| {
                                    StageAbort::screen(
                                        "探针回车后 own answer 行消失（屏面可能已切换）——已中止；请人工核对终端",
                                    )
                                })?;
                            if was_checked && !own_now_checked {
                                // 已勾选行被首键取消勾选（探针语义）→ 补第二键（勾回+编辑器）
                                oc_send_enter(terminal, &mut sent_keys)?;
                                let after2 = terminal.read().ok_or_else(|| {
                                    StageAbort::screen(
                                        "探针第二键后读不到屏幕——已中止；请人工核对终端",
                                    )
                                })?;
                                if opencode_own_answer_edit_open(&after2, &d) {
                                    edit_ready = true;
                                    break;
                                }
                                return Err(StageAbort::screen(
                                    "已存行双 enter 未进编辑态（形态与实测语义不符）——已中止，未发文本；请人工核对终端",
                                ));
                            }
                            if opencode_option_flip(&cur, &after, own_pos, &d) {
                                // enter 落在选项行 → 立即还原（翻转可逆，实测）
                                oc_send_enter(terminal, &mut sent_keys)?;
                                let restored = terminal.read().ok_or_else(|| {
                                    StageAbort::screen(
                                        "探针还原键后读不到屏幕——已中止；请人工核对终端",
                                    )
                                })?;
                                if opencode_own_answer_edit_open(&restored, &d) {
                                    // 还原键意外命中 own 行（走位漂移）——编辑态在场，接受为到位
                                    edit_ready = true;
                                    break;
                                }
                                if opencode_option_flip(&cur, &restored, own_pos, &d) {
                                    return Err(StageAbort::screen(
                                        "探针还原失败（选项勾选态未复原）——已中止，未发文本；请人工核对终端",
                                    ));
                                }
                            }
                            terminal.send("down").map_err(|e| {
                                StageAbort::delivery(format!(
                                    "INJECT_DELIVERY_FAIL: 下箭头投递失败（{e}）"
                                ))
                            })?;
                            sent_keys.push("down".to_string());
                            terminal.settle();
                        }
                        if !edit_ready {
                            return Err(StageAbort::screen(
                                "enter 探针走满预算仍未进入编辑态（own 行不可达或屏面异常）——已中止，未发文本；请人工核对终端",
                            ));
                        }
                    }
                }
            }
        }
    } else {
        // 鲜态：footer 闭环寻位 + 弱校验 + enter（既有路径，零回归）
        let mut steps = 0usize;
        while steps < own_pos.saturating_sub(1) {
            let cur = terminal.read().ok_or_else(|| {
                StageAbort::screen(
                    "INJECT_SCREEN_ABORT: 走位中读不到屏幕——已中止，未发任何键；请人工核对终端",
                )
            })?;
            if opencode_own_row_highlighted(&cur, &d) {
                break;
            }
            terminal.send("down").map_err(|e| {
                StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 下箭头投递失败（{e}）"))
            })?;
            sent_keys.push("down".to_string());
            terminal.settle();
            steps += 1;
        }
        // 2.5 走位弱校验（F3）：高亮字符层不可见 → 无法逐行复核；以「走位后 own answer
        // 行仍在 + 编号未跳变 + 勾选/内容未变」降险（异常即中止零后续键）
        let after_walk = terminal.read().ok_or_else(|| {
            StageAbort::screen(
                "INJECT_SCREEN_ABORT: 走位后读不到屏幕——已中止，未发任何键；请人工核对终端",
            )
        })?;
        let (walked_pos, walked_checked, walked_saved) =
            opencode_own_answer_row_state(&after_walk, &d).ok_or_else(|| {
                StageAbort::screen(
                    "INJECT_SCREEN_ABORT: 走位后屏上找不到 own answer 行——已中止；请人工核对终端",
                )
            })?;
        if walked_pos != own_pos || walked_checked != was_checked || walked_saved != saved_text {
            return Err(StageAbort::screen(
                "走位后屏面发生变化（编号/勾选/内容不符，终端可能已切换）——已中止；请人工核对终端",
            ));
        }
        // 3. enter 开编辑态（编辑门方言：EnterDoubleWhenChecked = 已勾选时第一键取消
        // 勾选，须第二键才进编辑；键序脚本锁 = 方言表的回归资产）
        oc_send_enter(terminal, &mut sent_keys)?;
        if was_checked {
            // 复核第一键的落点（不盲发第二键）：屏读已开启 → 版本语义直接进编辑（放行）；
            // 仍在场且行已取消勾选 → 双 enter 语义成立，补第二键；勾选未变 → 形态异常中止
            let after1 = terminal.read().ok_or_else(|| {
                StageAbort::screen(
                    "已发回车但读不到屏幕（无法确认 toggle 落点）——已中止；请人工核对终端",
                )
            })?;
            if !opencode_own_answer_edit_open(&after1, &d) {
                let (_, still_checked, _) =
                    opencode_own_answer_row_state(&after1, &d).ok_or_else(|| {
                        StageAbort::screen(
                            "toggle 后屏上找不到 own answer 行——已中止；请人工核对终端",
                        )
                    })?;
                if still_checked {
                    return Err(StageAbort::screen(
                        "已勾选行的首次回车未取消勾选（形态与实测语义不符）——已中止，未发文本；请人工核对终端",
                    ));
                }
                oc_send_enter(terminal, &mut sent_keys)?;
            }
        }
    }
    // 4. **裸打字守卫**：屏读确认编辑态已开启才发文本（两路径共用）。
    // **信号三取或**（2026-10-05 双形态收口）：
    // ① 占位行（1.x：enter 后行下展开占位输入行——opencode_own_answer_input_open）；
    // ② footer 词形（2.0.22+：`enter done`——方言表 edit_open_footer，自建会话活体
    //    取证；1.x 不出现该词形，互不误伤）；
    // ③ **own 行勾选翻转**（2.x 鲜态语义：enter = 勾选上 + 进编辑——勾选态从无到有
    //    即编辑态在场的旁证；1.x 单选形态无勾选框恒 false，不干扰占位行判据）。
    // 三信号皆无 = enter 落在了别处（走位漂移/形态漂移）→ 中止零文本。
    let opened = terminal.read().ok_or_else(|| {
        StageAbort::screen(
            "已发回车但读不到屏幕（无法确认编辑态开启）——已中止，未发文本（裸打字会被当导航/勾选指令）；请人工核对终端",
        )
    })?;
    let own_now_checked = opencode_own_answer_row_state(&opened, &d)
        .map(|(_, checked, _)| checked)
        .unwrap_or(false);
    if !opencode_own_answer_edit_open(&opened, &d) && !(own_now_checked && !was_checked) {
        return Err(StageAbort::screen(
            "编辑态未开启（无占位行/footer 信号/勾选翻转）——已中止，未发文本（裸打字守卫）；请人工核对终端",
        ));
    }
    // 5. 覆盖写入：光标在已存文本末尾（用户实测）→ backspace × 可见长度逐个回删
    if overwrite && !saved_text.is_empty() {
        let n = saved_text.chars().count().min(400);
        for _ in 0..n {
            terminal.send("backspace").map_err(|e| {
                StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 退格投递失败（{e}）"))
            })?;
        }
        sent_keys.push(format!("<backspace×{n}>"));
        terminal.settle();
    }
    // 6. 打字（字符通道；空文本 + overwrite = 仅清空）
    if !text.is_empty() {
        terminal.send_text(text).map_err(|e| {
            StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 作答文本投递失败（{e}）"))
        })?;
        sent_keys.push("<text>".to_string());
        terminal.settle();
    }
    // 7. enter 保存（锁定态——方向键不再影响编辑；推进走既有 tab/提交按钮）
    terminal.send("enter").map_err(|e| {
        StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 保存回车投递失败（{e}）"))
    })?;
    sent_keys.push("enter".to_string());
    terminal.settle();
    // 7.5 **保存后勾选闭环**（2026-10-05 用户实机语义定案）：own 行的勾选标记是
    // Submit 汇总计数的依据——**勾不在 = 该文本不进答案**（用户实测：手动保存
    // 后勾不掉，兔维斯 生产路径保存后勾掉落；同键序两结果，疑似保存回车与文本批
    // 渲染的竞态，机理未定案——本闭环是行为学兜底）。保存后屏读复核：
    // - 勾在 → 完成；
    // - 勾不在（仅当行内容仍是刚写入的文本——1.x 保存即推进换题，**绝不误补
    //   下一题**）→ enter 舞步补勾：enter = 勾选+进编辑（实测语义）→ enter =
    //   保存 → 复核；高亮漂移防护与探针走位同款（选项翻转即还原换位）；
    // - 两次尝试仍不在 → 报错不虚报完成（文本在但 Submit 不计数，用户必须知道）。
    if !text.is_empty() {
        for _attempt in 0..2 {
            let cur = match terminal.read() {
                Some(l) => l,
                None => break, // 读不到屏 → 交第 8 段如实回执
            };
            let Some((_, checked_now, saved_now)) = opencode_own_answer_row_state(&cur, &d) else {
                break; // 行消失（推进/切屏）→ 不补
            };
            if checked_now {
                break; // 已在（正常路径）
            }
            // 折行容忍：可见部分是所写文本的前缀才补（内容漂移/已换题 → 不补）
            if saved_now.is_empty() || !text.starts_with(saved_now.as_str()) {
                break;
            }
            oc_send_enter(terminal, &mut sent_keys)?;
            let after = terminal.read().ok_or_else(|| {
                StageAbort::screen(
                    "INJECT_SCREEN_ABORT: 补勾回车后读不到屏幕——已中止；请人工核对终端",
                )
            })?;
            if opencode_own_answer_edit_open(&after, &d) {
                // 命中 own 行：勾上 + 编辑态 → 保存，回循环头复核
                oc_send_enter(terminal, &mut sent_keys)?;
                continue;
            }
            if opencode_option_flip(&cur, &after, own_pos, &d) {
                // 落在选项行 → 立即还原（翻转可逆，实测）
                oc_send_enter(terminal, &mut sent_keys)?;
                let restored = terminal.read().ok_or_else(|| {
                    StageAbort::screen(
                        "INJECT_SCREEN_ABORT: 补勾还原键后读不到屏幕——已中止；请人工核对终端",
                    )
                })?;
                if opencode_option_flip(&cur, &restored, own_pos, &d) {
                    return Err(StageAbort::screen(
                        "补勾还原失败（选项勾选态未复原）——已中止；请人工核对终端",
                    ));
                }
            }
            terminal.send("down").map_err(|e| {
                StageAbort::delivery(format!(
                    "INJECT_DELIVERY_FAIL: 补勾走位下箭头投递失败（{e}）"
                ))
            })?;
            sent_keys.push("down".to_string());
            terminal.settle();
        }
    }
    // 8. 终态（未见不是失败）+ **屏读回执**（F3：保存后重读，own answer 行的
    // 勾选态与已存文本随回执下发——卡面权威源=屏读；读不到 → None/None 如实）
    let receipt_seen = stage_receipt_seen(&mut poll_receipt, opencode_answered_present);
    let (screen_text, screen_checked) = match terminal.read() {
        Some(lines) => match opencode_own_answer_row_state(&lines, &d) {
            Some((_, checked, saved)) => (
                if saved.is_empty() { None } else { Some(saved) },
                Some(checked),
            ),
            None => (None, None),
        },
        None => (None, None),
    };
    // 终判（7.5 的裁决点）：**所写文本仍在屏上**但勾最终不在 = Submit 不会计入
    // 该文本（用户实测语义）→ 不许虚报 Ok；如实报错让用户去终端补勾。
    // - 1.x 保存即推进：屏上已是下一题的行（screen_text 与所写文本无前缀关系）
    //   → 不误报；
    // - 清空路径（空文本）无此语义——勾不在正是清空的预期终态。
    if !text.is_empty()
        && screen_checked == Some(false)
        && screen_text.as_deref().is_some_and(|t| text.starts_with(t))
    {
        return Err(StageAbort::screen(
            "作答文本已写入，但保存后勾选标记缺失（补勾未成功）——Submit 不会计入该文本；请到终端勾选该项后再提交",
        ));
    }
    Ok(FreeTextOutcome {
        sent_keys,
        receipt_seen,
        advanced: false,
        review_reached: false,
        screen_text,
        screen_checked,
    })
}

/// opencode SQLite **answers 对账解析**（纯函数）：`state.metadata` 的
/// `{"answers":[[...],[...]],"truncated":bool}` → 二维数组（按题序对齐，非题干键控；
/// 内层序=勾选序——戊探A ⑥ 定案；多选标志字段名=`multiple`）。解析失败 → None。
pub fn parse_opencode_answers(metadata_json: &str) -> Option<Vec<Vec<String>>> {
    let v: serde_json::Value = serde_json::from_str(metadata_json).ok()?;
    let arr = v.get("answers")?.as_array()?;
    arr.iter()
        .map(|inner| {
            inner
                .as_array()?
                .iter()
                .map(|x| x.as_str().map(str::to_string))
                .collect::<Option<Vec<String>>>()
        })
        .collect()
}

// ============================================================
// 丁T5：多选提交阶段机 + 自由作答阶段机（§2.3 裁4 / §2.4 裁3）
// ============================================================

/// 多选题提交屏上的**焦点标记**（claude 的 `pointer` 主题字形）。
///
/// 实测：`C-s8-cursor-submit-*.png` 的 `Submit` 行首有该字形、同屏其它选项行没有
/// （逐行像素比对：标记列的 ink 段只在焦点行出现——`C-s8-digit1-checked-*.png` 的
/// ink 在第 1 选项行、`C-s8-cursor-submit-*.png` 的在 Submit 行）。claude 二进制
/// `pointer:"\u276F"`，渲染分支见其 `rr()` 组件：`isFocused → L.pointer`（否则空格）。
///
/// **为什么不复用 [`crate::inject::dialog`] 的光标标记集合**：那套集合是**跨工具**
/// 的兜底（codex `›` / claude `❯` / kimi `▶` / ASCII `>`），而这里要的是「提交屏这
/// 一行**确实**是焦点行」的判据——把 ASCII `>` 也算进来会让正文里的引用行冒充焦点行。
/// 故本判据单列一字形，且**只对 claude 的提交屏用**（该屏形态已实机取证；其它工具
/// 本就走不到这条编排）。
pub const SUBMIT_FOCUS_MARKER: char = '\u{276F}';

/// 多选题的提交行文本（实机 `Submit`；claude 二进制 `submitButtonText` 单题时为
/// `"Submit"`）。
pub const SUBMIT_ROW_LABEL: &str = "Submit";

/// 多题形态下每题尾部的**推进行**文本 `Next`（claude 二进制 `submitButtonText` 多题
/// 取值；2026-09-24 用户实机取证 2.1.278 屏面实证——三题表单第一题尾部为无编号
/// `Next` 行，回车进入下一题，见 `research/refs/phase2-消息注入/2026-09-24-claude
/// 多选多题键序-用户实机取证.md` §3）。
pub const NEXT_ROW_LABEL: &str = "Next";

/// 推进行标签判定（单题 `Submit` 与多题 `Next` 的**单一判据点**）：
/// - `Submit` 沿用**前缀匹配**（既有 K10 证据形态，`submit_row_focused` 原口径与
///   测试锁不动）；
/// - `Next` 用**精确相等**——它是常见词（正文 "Next steps:" 类行），前缀匹配会把
///   正文行误判成推进行；实机推进行无行尾装饰（用户截图与丁复审 dump 均为裸词）。
fn is_advance_label(t: &str) -> bool {
    t.starts_with(SUBMIT_ROW_LABEL) || t == NEXT_ROW_LABEL
}

/// Review 确认屏的**标题锚**（实机逐字，见模块文档判据表）。
///
/// **2026-09-23 起真源移入账本**（[`crate::inject::anchor_ledger`] 的
/// `claude/question_review/title`）：上游改词时按账本格式追加一行即可
/// （append-only），不必改代码。取不到时回落已入账那一条（编译期常量兜底）。
pub fn review_title_anchor() -> &'static str {
    crate::inject::anchor_ledger::candidates(
        "claude",
        crate::inject::anchor_ledger::scenario::QUESTION_REVIEW,
        crate::inject::anchor_ledger::slot::TITLE,
    )
    .first()
    .map(|r| r.text)
    .unwrap_or("review your answers")
}

/// Review 确认屏的**副题锚**（实机逐字；与标题**任一**在场即判 Review 屏在场）。
///
/// 为什么允许「任一」而不是「必须两者同时」：屏读只读**可见窗口**，而 Review 屏出现
/// 在提问 UI 的位置——用户终端若字号较大/窗口较矮，标题行可能被滚出可见区（副题与
/// 选项行仍在）。两个锚都是该屏的**专属文案**（正常对话流不会出现），取「任一在场」
/// 既保住判据的排他性，又不在矮窗口下误判为缺席。
///
/// **2026-09-23 起真源移入账本**（`claude/question_review/present`——副题即确认屏
/// 在场的证据行）；取不到时回落已入账那一条。
pub fn review_subtitle_anchor() -> &'static str {
    crate::inject::anchor_ledger::candidates(
        "claude",
        crate::inject::anchor_ledger::scenario::QUESTION_REVIEW,
        crate::inject::anchor_ledger::slot::PRESENT,
    )
    .first()
    .map(|r| r.text)
    .unwrap_or("ready to submit")
}

/// Review 确认屏的确认项文本（实机 `1. Submit answers`）。
///
/// **2026-09-23 起真源移入账本**（`claude/question_review/confirm`）；取不到时回落
/// 已入账那一条。
pub fn review_confirm_anchor() -> &'static str {
    crate::inject::anchor_ledger::candidates(
        "claude",
        crate::inject::anchor_ledger::scenario::QUESTION_REVIEW,
        crate::inject::anchor_ledger::slot::CONFIRM,
    )
    .first()
    .map(|r| r.text)
    .unwrap_or("submit answers")
}

/// 确认后**终态锚**（实机 `User answered Claude's questions:`——工具回执落进对话流）。
///
/// **2026-09-23 起真源移入账本**（`claude/question/receipt`）；取不到时回落已入账
/// 那一条。
pub fn answered_receipt_anchor() -> &'static str {
    crate::inject::anchor_ledger::candidates(
        "claude",
        crate::inject::anchor_ledger::scenario::QUESTION,
        crate::inject::anchor_ledger::slot::RECEIPT,
    )
    .first()
    .map(|r| r.text)
    .unwrap_or("user answered claude")
}

/// 自由作答行（TUI 自动追加）的标签文本。**两形态**：单选是 `Type something.`
/// （带句点）、多选是 `Type something`（无句点）——claude 二进制
/// `Sa.multiSelect?"Type something":"Type something."`。判据按**前缀**匹配
/// （`Type something`），故两形态同一判据覆盖。
pub const FREE_TEXT_ROW_LABEL: &str = "Type something";

/// 阶段机中止的**类别**——决定端点回执的形态与审计的措辞（复评 F6-2）。
///
/// # 为什么要分两类（这是回执语义的一部分，不是内部细节）
///
/// 「中止」有两种成因，用户要做的事完全不同：
/// - [`Screen`](Self::Screen)：**屏上形态与预期不符**（读不到屏 / 没有提交行 /
///   Review 屏没出现 / 焦点走不动）。键**没有**发失败，是「按我们的判据不该继续发」
///   → 回执 `failed{aborted:true, stage}`（前端显示中止段 + 引到终端）；
/// - [`Delivery`](Self::Delivery)：**投递本身失败**（`Injector` 返回 Err——终端不可达、
///   控制台附加失败、写超时）。这与阶段判据无关，语义等同批次丙的「投递失败」→
///   回执 `failed{error}`（**不带** `aborted`），审计 `failed:<e>`。
///
/// 混为一谈的后果（本类存在的原因）：投递失败被报成「中止于某段」，用户会去终端找
/// 「形态问题」，而实际问题是通道打不通——那是误导。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageAbort {
    /// 中止类别
    pub kind: StageAbortKind,
    /// 面向用户的整句中文说明（端点直接透传到 `error` 字段）
    pub message: String,
}

/// 见 [`StageAbort`]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageAbortKind {
    /// 屏读形态不符（阶段判据拒绝继续）
    Screen,
    /// 投递失败（`Injector`/终端通道返回 Err）
    Delivery,
}

impl StageAbort {
    /// 屏读形态不符的中止（`pub(crate)`：api.rs 的三态首段轮询直接构造
    /// 「读不到屏/形态不符」两态中止——段名映射仍单点走 `stage_from_abort`）
    pub(crate) fn screen(message: impl Into<String>) -> Self {
        Self {
            kind: StageAbortKind::Screen,
            message: message.into(),
        }
    }
    /// 投递失败的中止（包装 `Injector` 或终端写入返回的错误文案）
    fn delivery(message: impl Into<String>) -> Self {
        Self {
            kind: StageAbortKind::Delivery,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for StageAbort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// 阶段机**终态轮询的三态归一**（各阶段机末段同形段收口为单一实现）：
/// `Ok(Some(屏))` → `present` 判终态回执行在场；`Ok(None)` 窗尽未见 →
/// `Some(false)`（未见**不是失败**——回执行可能被后续输出刷走，调用方如实请人工
/// 核对）；`Err` 读屏不可用 → `None`（无法核验，不冒充）。
fn stage_receipt_seen<P, F>(poll: &mut P, present: F) -> Option<bool>
where
    P: FnMut() -> Result<Option<Vec<String>>, String>,
    F: FnOnce(&[String]) -> bool,
{
    match poll() {
        Ok(Some(lines)) => Some(present(&lines)),
        Ok(None) => Some(false),
        Err(_) => None,
    }
}

/// 阶段机的一次屏读**语义结论**（三态语义与 [`crate::inject::mode::PollStep`] 一致：
/// 可行动 / 可重试 / 形态异常）。载荷类型与那个泛型枚举不同（这里各段要的东西不一
/// 样），故不复用——共用一个会让两套判据的语义混进同一类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenStep {
    /// 目标形态已出现（附该屏行集）
    Ready(Vec<String>),
    /// 本轮还看不到目标形态（**可重试**，`String` = 用户看得懂的原因）
    NotYet(String),
    /// 形态异常（**立即中止**，`String` = 用户看得懂的原因）
    Fatal(String),
}

/// 一行是否是**带焦点标记**的推进行：剥掉行首空白与 [`SUBMIT_FOCUS_MARKER`] 后，
/// 剩余文本（trim）命中 [`is_advance_label`]（单题 `Submit` 前缀 / 多题 `Next` 精确）。
///
/// **为什么 Submit 判「前缀」而不是精确相等**：实机 Submit 行是 `❯   Submit`（标记后
/// 有缩进——见 `C-s8-cursor-submit-*.png`），且该行**没有编号**（与选项行不同）。前缀
/// 匹配同时容忍行尾可能的装饰；而「不含编号」这一形态差异正是它与选项行不会混淆
/// 的原因。`Next` 的口径差异见 [`is_advance_label`]。
fn submit_row_focused(line: &str) -> bool {
    let t = line.trim_start();
    let Some(rest) = t.strip_prefix(SUBMIT_FOCUS_MARKER) else {
        return false;
    };
    is_advance_label(rest.trim())
}

/// 屏上是否**存在**推进行（不论焦点在哪）——用于把「没找到推进行」与「推进行在但
/// 焦点不在它上面」两种失败分开报告（回执要能说清卡在哪一段）。
///
/// 两种屏上形态都算「在场」（实机两张截图各取其一）：焦点行 `❯   Submit`、
/// 非焦点行 `    Submit`；多题形态同位换词为 `Next`。
fn submit_row_present(line: &str) -> bool {
    let t = line.trim_start();
    let t = t.strip_prefix(SUBMIT_FOCUS_MARKER).unwrap_or(t);
    is_advance_label(t.trim())
}

/// **提交屏在场**探测（**测试专用**——生产侧第 1 段轮询只负责「读一屏」，在场判据由
/// [`run_submit_stages`] 自己对 `poll_submit` 的返回调 [`submit_row_present`]）。
///
/// 为什么还要这个测试用变体：脚本驱动的轮询需要「读到提交屏就停」的语义（否则窗尽前
/// 会一路推 cursor，把 Review 屏也吃掉）；生产侧的 `poll_question_stage` 是「只读不判」
/// ——判据单点在编排里。两者的**判据是同一份**（都调 `submit_row_present`），故驱动
/// 语义不同而结论一致；这也是把探测抽成函数、而不是在测试里另写一个 `any()` 的理由
/// （另写就会与编排判据漂移）。
#[cfg(test)]
fn probe_submit_screen(lines: &[String]) -> ScreenStep {
    if lines.iter().any(|l| submit_row_present(l)) {
        return ScreenStep::Ready(lines.to_vec());
    }
    ScreenStep::NotYet("屏上见不到 Submit 行（多选提交入口未画出/被滚出可见窗口）".to_string())
}

/// 提交屏的**走位**探测：给出「焦点行是否已落在提交行」这一结论。
/// - 焦点在提交行 → `Ready`；
/// - 提交行在场但焦点不在它上面 → `NotYet`（还没走到，可继续按 ↓）；
/// - 提交行**不在场** → `NotYet`（可能是 TUI 还没画出/被滚出窗口——由调用方的窗尽
///   逻辑判失败，而不是在这里武断 `Fatal`：多选界面本身可能因窗口高度只显示部分行）。
fn probe_submit_row(lines: &[String]) -> ScreenStep {
    if lines.iter().any(|l| submit_row_focused(l)) {
        return ScreenStep::Ready(lines.to_vec());
    }
    if lines.iter().any(|l| submit_row_present(l)) {
        return ScreenStep::NotYet(
            "提交行在场但焦点不在它上面（光标还没走到 Submit 行）".to_string(),
        );
    }
    ScreenStep::NotYet("屏上见不到 Submit 行（多选提交入口未画出/被滚出可见窗口）".to_string())
}

/// Review 确认屏的**确认项**抽取（判据的单点实现）：屏上**带编号**且文本含
/// [`review_confirm_anchor`] 的行 → `(屏上编号, 行原文)`。
///
/// **为什么返回编号而不只是行原文**：确认键要发的是**屏上那个编号**（不硬编码 `'1'`
/// ——屏上编号由 TUI 给，抄它比自己编号稳）。编号在这里就解析出来了，调用方拿不到
/// 「行在但数字抽不出」这种中间态（那种状态在本判据下不可达：`parse_option_line`
/// 已经要求行首是数字）——把不变式收在类型里，而不是在下游写一个永不成立的 `ok_or_else`
/// （本仓的「不许空断言」纪律：不可达的失败分支就是虚假的安全感）。
pub(crate) fn review_confirm_items(lines: &[String]) -> Vec<(u32, String)> {
    lines
        .iter()
        .filter_map(|l| crate::inject::dialog::parse_option_line(l))
        .filter(|(_, label, _)| label.to_lowercase().contains(review_confirm_anchor()))
        .map(|(num, label, _)| (num, format!("{num}. {label}")))
        .collect()
}

/// 见 [`review_confirm_items`]（只要行原文的便利视图；判据同一份）
pub(crate) fn review_confirm_lines(lines: &[String]) -> Vec<String> {
    review_confirm_items(lines)
        .into_iter()
        .map(|(_, text)| text)
        .collect()
}

/// Review 确认屏的一次屏读探测（判据锚见各锚点函数文档）。`Ready` 载荷 = **该屏行集**
/// （与其它探测器同形态）。
/// - 命中「标题锚 ∨ 副题锚」且**恰好一行**确认项 → `Ready(屏)`；
/// - 命中标题/副题但**读不到带编号的确认项** → `Fatal`（Review 屏在场却无法确认，
///   继续发数字就是盲发）；
/// - 确认项**多项**命中（`Submit answers` 与别的含该词的编号行并存）→ `Fatal`（不猜）；
/// - 全都未见 → `NotYet`（可能还没切屏）。
pub(crate) fn probe_review_screen(lines: &[String]) -> ScreenStep {
    let has_title = lines.iter().any(|l| {
        let low = l.to_lowercase();
        low.contains(review_title_anchor()) || low.contains(review_subtitle_anchor())
    });
    if !has_title {
        return ScreenStep::NotYet(
            "屏上未出现 Review 确认屏（未见「Review your answers」/「Ready to submit」）"
                .to_string(),
        );
    }
    match review_confirm_lines(lines).len() {
        0 => ScreenStep::Fatal(
            "Review 确认屏在场但屏上读不到带编号的确认项（含「Submit answers」的编号行）——已中止（不盲发数字）"
                .to_string(),
        ),
        1 => ScreenStep::Ready(lines.to_vec()),
        n => ScreenStep::Fatal(format!(
            "Review 确认屏上含「{}」的编号行有 {n} 行（应恰好 1 行）——不猜，已中止",
            review_confirm_anchor()
        )),
    }
}

/// **终态锚**探测：屏上是否出现「本问题已答完」的回执行（[`answered_receipt_anchor`]）。
///
/// 用于提交/自由作答的最后一段回执核验——**未见不是失败**：回执可能被后续输出刷走，
/// 也可能该版本的文案不同，调用方据此下发 `Some(false)` 而不是谎报完成（同 `mode`
/// 侧 `receipt_seen` 的裁决）。
pub(crate) fn probe_answered_receipt(lines: &[String]) -> bool {
    lines
        .iter()
        .any(|l| l.to_lowercase().contains(answered_receipt_anchor()))
}

// ============================================================
// 2026-09-24 多选闭环切勾（用户实机取证 2.1.278：空格=切勾、数字=无反应）
// 解析底料 + 闭环导航 + 翻转核验。证据档案：
// research/refs/phase2-消息注入/2026-09-24-claude多选多题键序-用户实机取证.md
// ============================================================

/// 问答题屏一行的**类别**（[`parse_question_rows`] 的产物）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuestionRowKind {
    /// 模型选项行（多选形态带勾选框）
    Option,
    /// TUI 自动追加的自由作答行（多选形态带勾选框 `N. [ ] Type something`）
    FreeText,
    /// 尾部推进行（单题 `Submit` / 多题 `Next`，无编号）
    Advance,
}

/// 问答题屏解析出的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QuestionRow {
    pub kind: QuestionRowKind,
    /// 屏上编号（TUI 给的；推进行无 → `None`）
    pub number: Option<u32>,
    /// 勾选态：无勾选框 → `None`；`[ ]` → `Some(false)`；`[✓]`/`[✔]` → `Some(true)`
    ///
    /// **按括号内容语义判**（空=未选/非空=已选），不硬编码字形族——实机证据里已见
    /// `[✓]`(U+2713)（用户截图）与 `[✔]`(U+2714)（丁复审 dump）两种。
    pub checked: Option<bool>,
    /// 焦点（[`SUBMIT_FOCUS_MARKER`]）是否在本行
    pub focused: bool,
    /// 剥离编号与勾选框后的文本
    pub label: String,
}

/// 分隔线行判定（`─`/`━`（U+2500/U+2501）连串）：问答题屏在推进行与
/// `Chat about this` 之间的分隔（丁复审 dump + 用户截图同形态）。**线下不在走位
/// 路径上**（K10 实证走位止于推进行）。只认制表符族、不收 ASCII `-`（评审 Minor-4：
/// 对话滚回区的 markdown `----` 水平线会提前翻转 below_separator 饿死行块解析）；
/// 阈值按字符数（`t.len()` 是字节数，对多字节字形不对称）。
fn is_separator_line(t: &str) -> bool {
    t.chars().count() >= 4 && t.chars().all(|c| c == '─' || c == '━')
}

/// 行首编号剥离：`"1. [ ] Apple"` → `(1, "[ ] Apple")`；无 `N. ` 形态 → `None`。
fn split_leading_number(t: &str) -> Option<(u32, &str)> {
    let digits_end = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    if digits_end == 0 {
        return None;
    }
    // 编号后必须是 ". "（`5. [ ] Type something` 形态；行尾裸 "5." 不存在——
    // 每个编号行都有载荷）
    let after = t[digits_end..].strip_prefix(". ")?;
    let num: u32 = t[..digits_end].parse().ok()?;
    Some((num, after))
}

/// 勾选框剥离：`"[✓] Apple"` → `(Some(true), "Apple")`；`"[ ] Apple"` →
/// `(Some(false), "Apple")`；无勾选框（单选形态选项）→ `(None, 原文)`。
/// 括号内**非空即已选**（字形语义判，见 [`QuestionRow::checked`]）。
fn split_checkbox(rest: &str) -> (Option<bool>, &str) {
    let Some(after) = rest.strip_prefix('[') else {
        return (None, rest);
    };
    let Some(close) = after.find(']') else {
        return (None, rest);
    };
    let inner = &after[..close];
    let label = after[close + 1..].trim_start();
    // 内含 `[`/`]` 的不是勾选框（正文方括号引用）——保守回退「无勾选框」
    if inner.contains('[') || inner.contains(']') {
        return (None, rest);
    }
    let checked = !inner.trim().is_empty();
    (Some(checked), label)
}

/// 单行解析（编号行 / 推进行；正文、描述换行、页签栏、footer 均返回 `None`）。
fn parse_question_row(line: &str) -> Option<(Option<u32>, Option<bool>, bool, String)> {
    let t = line.trim_start();
    let focused = t.starts_with(SUBMIT_FOCUS_MARKER);
    let t = t.strip_prefix(SUBMIT_FOCUS_MARKER).unwrap_or(t);
    let t = t.trim_start();
    if let Some((num, rest)) = split_leading_number(t) {
        let rest = rest.trim_start();
        if rest.is_empty() {
            return None;
        }
        let (checked, label) = split_checkbox(rest);
        return Some((Some(num), checked, focused, label.trim_end().to_string()));
    }
    // 无编号行：只有推进行算（`Submit` 前缀 / `Next` 精确——与 submit 判定同一口径，
    // 防正文 "Next steps:" 冒充）
    if is_advance_label(t.trim()) {
        return Some((None, None, focused, t.trim().to_string()));
    }
    None
}

/// 问答题屏的**行块解析**：双锚设计（2026-10-02 活体事故后重构）。
///
/// **锚 A（推进行）**：以**最靠后**的推进行（`Submit`/`Next`）为锚，向上收**编号
/// 连续递减**的行（选项 1..n + `Type something` n+1），加推进行本身。为什么按连续
/// 递减收块：屏读窗口含对话滚回区，正文里可能出现编号行（戊探E 的 N5 误纳同型
/// 风险）；「紧邻推进行、编号 n, n-1, … 连续」这一形态只有题屏自身满足——滚回区
/// 编号块与题屏块之间必经一次编号跳变（或非编号行），在跳变处停手。为什么取
/// **最靠后**：题屏是屏上最新的内容（2026-10-02 修正——原来取「第一处」会被
/// 滚回区正文里的孤立 Submit 样行劫持）。
///
/// **闩锁移除（2026-10-02 活体事故根因）**：旧实现见到 ─ 分隔线就把其后所有行
/// 永久标记「线下」——但滚回区的分隔线（如 claude 多题流 Planning 块的上下边框）
/// 出现在题屏**上方**，会把整个题屏判死（解析恒 0 行，活体 dump 实证）。分隔线
/// 排除 `Chat about this` 的职责由锚的选取天然承担：Chat 行在推进行**之后**，向上
/// 收块摸不到它。
///
/// **锚 B（页签栏，2026-10-02 新增）**：多题流的**单选子题**没有独立推进行（活体
/// 取证：页签栏之后直接是题干+编号选项块，`Type something` 后即分隔线）——以
/// [`is_question_tab_bar`]（复合签名）为锚，**向下**收「编号连续递增」块，分隔线
/// 即停。产物**不含** `Advance` 行（调用方按无推进行形态处置）。单题集的**单页签**
/// 形态（2026-10-08 活体 claude 2.1.287：` ☐ Deploy`，无 ←/→/Submit——0.5.1
/// 报障靶屏）由 [`is_single_question_tab_line`] 兜底进同一锚，收块逻辑共用。
///
/// 解析失败（两锚皆不成立）→ 空块（调用方按形态不符中止，不猜——与
/// [`crate::inject::dialog::navigation_anchors`] 的「不猜起点」同纪律）。
pub(crate) fn parse_question_rows(lines: &[String]) -> Vec<QuestionRow> {
    // 候选行全量收集（编号行 + 推进行；题干/描述/页签/footer 等非编号行由
    // parse_question_row 返回 None 天然跳过）
    let mut parsed: Vec<(Option<u32>, Option<bool>, bool, String)> = Vec::new();
    for line in lines {
        if let Some((num, checked, focused, label)) = parse_question_row(line) {
            parsed.push((num, checked, focused, label));
        }
    }
    // 锚 A：最靠后的无编号行（推进行）。向上收「编号连续递减」块：紧邻锚的编号
    // 行给起点 k，其上依次要 k-1、k-2…
    if let Some(anchor) = parsed.iter().rposition(|(num, ..)| num.is_none()) {
        let mut block: Vec<(u32, Option<bool>, bool, String)> = Vec::new();
        let mut expect_next: Option<u32> = None;
        for (num, checked, focused, label) in parsed[..anchor].iter().rev() {
            let Some(n) = num else {
                break; // 非编号行（不该出现在锚上方——防御性停手）
            };
            if let Some(want) = expect_next {
                if *n != want {
                    break; // 编号跳变：滚回区杂行边界，块到此为止
                }
            }
            block.push((*n, *checked, *focused, label.clone()));
            expect_next = Some(n.saturating_sub(1));
        }
        if !block.is_empty() {
            block.reverse();
            // FreeText 行判定（2026-10-03 修正）：①label 以占位文本开头（未打字形态
            // ——**字符串在场，直接按行定位**）；②**块内编号最大的行**（打字后形态
            // ——占位字符串已被用户文字替换，按 claude TUI 恒定结构定位：自由作答行
            // 恒为选项列表末尾的追加项，普通选项不可能占据最大编号）。
            // **不依赖勾选框字形**（2026-10-03 用户批评 + 单选形态实证：单选题打字后
            // 的行是 `4. 组成一个HTML ✔`——✔ 在行尾无方括号，勾选框判据漏判 → 自由
            // 作答「解析不出 Type something 行」中止的事故根因）
            let max_num = block.last().map(|(n, ..)| *n);
            let mut rows: Vec<QuestionRow> = block
                .into_iter()
                .map(|(num, checked, focused, label)| {
                    let kind = if label.starts_with(FREE_TEXT_ROW_LABEL) || Some(num) == max_num {
                        QuestionRowKind::FreeText
                    } else {
                        QuestionRowKind::Option
                    };
                    QuestionRow {
                        kind,
                        number: Some(num),
                        checked,
                        focused,
                        label,
                    }
                })
                .collect();
            let (_, _, focused, label) = &parsed[anchor];
            rows.push(QuestionRow {
                kind: QuestionRowKind::Advance,
                number: None,
                checked: None,
                focused: *focused,
                label: label.clone(),
            });
            return rows;
        }
        // 锚 A 在但上方收不到块（滚回区杂锚）→ 落锚 B 再试
    }
    // 锚 B（页签栏）：多题复合签名优先；单题集的**单页签行**（2026-10-08 活体
    // claude 2.1.287：单问题 AskUserQuestion 页签只有 ` ☐ Deploy`，无 ←/→/Submit
    // ——既有复合签名不命中 → 双锚皆败 → 0 行中止，0.5.1 用户报障）兜底。两形态
    // 共用下方同一段「向下收编号连续递增块」（分隔线即停天然排除 Chat about this）。
    let tab_idx = lines
        .iter()
        .position(|l| is_question_tab_bar(l))
        // 单页签兜底取**最靠后**（rposition）：滚回区散落 ☐/☒ 起头行时首匹配会被
        // 劫持——锚 A 的 2026-10-02 教训（「取第一处被滚回区杂锚劫持」）对新锚同款
        // 适用；活题屏是屏上最靠下的页签行（其下只有选项/分隔线/footer）。多题复合
        // 签名维持 position 现状（既有夹具与实测形态全屏恰一条 ←…→ 行）。
        .or_else(|| lines.iter().rposition(|l| is_single_question_tab_line(l)));
    let Some(tab_idx) = tab_idx else {
        return Vec::new();
    };
    let mut block: Vec<(u32, Option<bool>, bool, String)> = Vec::new();
    let mut expect_next: Option<u32> = None;
    for line in &lines[tab_idx + 1..] {
        // 分隔线即停：`Chat about this`（编号在选项块之后）在线下，天然排除
        if is_separator_line(line.trim()) {
            break;
        }
        let Some((num, checked, focused, label)) = parse_question_row(line) else {
            continue; // 题干/描述/空行——容忍跳过（题干区在页签栏与首选项之间）
        };
        let Some(n) = num else {
            break; // 推进行（不该出现在页签栏之下）——防御性停手
        };
        if let Some(want) = expect_next {
            if n != want {
                break; // 编号跳变：噪声边界，块到此为止
            }
        }
        block.push((n, checked, focused, label));
        expect_next = Some(n + 1);
    }
    if block.is_empty() {
        return Vec::new();
    }
    // FreeText 行判定（与 anchor A 同规则）：占位前缀 ∨ 块内编号最大（单选子题
    // 打字后 `4. 组成一个HTML ✔` 无勾选框——字形判据漏判的同源修正）
    let max_num = block.last().map(|(n, ..)| *n);
    block
        .into_iter()
        .map(|(num, checked, focused, label)| {
            let kind = if label.starts_with(FREE_TEXT_ROW_LABEL) || Some(num) == max_num {
                QuestionRowKind::FreeText
            } else {
                QuestionRowKind::Option
            };
            QuestionRow {
                kind,
                number: Some(num),
                checked,
                focused,
                label,
            }
        })
        .collect()
}

/// 页签栏行判定（2026-10-02 活体取证 2.1.278 多题流）：
/// `←  ☐ 优化范围  ☐ 版本处理  ✔ Submit  →`——复合签名三特征（`←` 开头 + `→`
/// 结尾 + 含 submit），散文正文不会同时命中三者。多题流**单选子题**没有独立
/// 推进行，页签栏是题屏唯一结构锚（见 [`parse_question_rows`] 锚 B）。
fn is_question_tab_bar(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('←') && t.ends_with('→') && t.to_lowercase().contains("submit")
}

/// 单题集的**单页签行**判定（2026-10-08 活体取证 claude 2.1.287，自建探针会话
/// dump，证据 `%USERPROFILE%\mam-probe-m6r\evidence\screen-claude-single-tab-auq-
/// 20261008-154900.txt`）：单问题 AskUserQuestion 的页签栏只有一行 ` ☐ Deploy`
/// ——无 `←`/`→`、无 `Submit`，[`is_question_tab_bar`] 三特征全不命中 → 双锚皆败
/// → 解析 0 行 →「屏上解析不出问答选项块」中止（0.5.1 用户报障的靶形态）。
/// 判据 = 剥首空白后以 ☐(U+2610)/☒(U+2612) 起头且后随非空页签头。☒ = 已答投影
/// （多题页签栏同字形族，2.1.278 矩阵实证 `☒/☐ 各题已答/未答`）；✔/☑ 无单题
/// 证据不收（结论不超证据）。多题页签栏以 `←` 起头，天然不进本判据（无双匹配）。
fn is_single_question_tab_line(line: &str) -> bool {
    let t = line.trim_start();
    let rest = t.strip_prefix('☐').or_else(|| t.strip_prefix('☒'));
    let Some(rest) = rest else {
        return false;
    };
    !rest.trim().is_empty()
}

/// 题屏的**题干区原文**（页签栏→首个编号行之间，去空白拼接）——切题分类的
/// 「换页」信号（与 [`screen_question_matches`] 同一提取面；Review 屏也会给出
/// 非空值，调用方仅在题屏语境下消费）。
pub(crate) fn question_heading_of(lines: &[String]) -> String {
    let norm = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    // 单题集单页签行也算页签（2026-10-08）：题干从页签之后起算，页签头不混入题干
    let tab_idx = lines
        .iter()
        .position(|l| l.contains('←') || is_single_question_tab_line(l));
    let first_option = lines
        .iter()
        .position(|l| parse_question_row(l).is_some_and(|(num, ..)| num.is_some()));
    let Some(first) = first_option else {
        return String::new();
    };
    let start = match tab_idx {
        Some(t) if t < first => t + 1,
        _ => first.saturating_sub(4),
    };
    lines[start..first].iter().map(|l| norm(l)).collect()
}

/// **题屏快照**（卡面状态权威源的载体——AGENTS.md「屏读为准」原则）：从一屏
/// 解析出当前题的可观察状态，供回执回传（交互后核对）与 GET 初始化（重启/重开
/// 页面后的状态纠偏）。
#[derive(Debug, Clone, PartialEq)]
pub struct QuestionScreenSnapshot {
    /// 选项行勾选态（按屏上编号序；`None` = 该行无勾选框——单选形态选项）
    pub checked: Vec<Option<bool>>,
    /// 自由作答行内容：已打字 → `Some(屏上文本)`；占位未打字 → `None`
    /// **三态判别靠 [`Self::free_text_present`] 行在场旗标**（评审 I1：`None` 曾同时
    /// 表示「占位」与「行缺席」两种态，前端无法区分「终端已清空」与「读不到」）
    pub free_text: Option<String>,
    /// 自由作答行**在场**旗标（true = 屏上有 Type something 行——无论占位还是已打字）
    pub free_text_present: bool,
    /// 题干区归一化文本（前端据此把快照对位到载荷题）
    pub heading: String,
}

/// 剥自由作答行**行尾的选中标记**（` ✔`/` ✓`/` ✅`——单选屏打字后自动选中的
/// 行尾标记；多选屏的勾选在方括号内不受影响）。展示/回传用——解析器 kind 判定
/// 与「文字保留」核验仍比对原样 label。
fn strip_trailing_selection_mark(text: String) -> String {
    let t = text.trim_end();
    for mark in ["\u{2705}", "\u{2714}", "\u{2713}"] {
        if let Some(stripped) = t.strip_suffix(mark) {
            return stripped.trim_end().to_string();
        }
    }
    t.to_string()
}

/// 从一屏提取 [`QuestionScreenSnapshot`]。
///
/// **Review 确认屏 → `None`**（评审 C1）：Review 屏带页签栏 + 编号确认项
///（`1. Submit answers`），anchor B 会把它解析成「题屏」——题干区还含答毕回显，
/// 前端对位会错跳题并清掉勾选态。Review 是更特异的判据（[`probe_review_screen`]），
/// 先判它；调用方（GET）据 `None` 落到 `{review:true}` 检测。
pub(crate) fn question_screen_snapshot(lines: &[String]) -> Option<QuestionScreenSnapshot> {
    if matches!(probe_review_screen(lines), ScreenStep::Ready(_)) {
        return None; // Review 确认屏不是题屏（C1：更特异判据优先）
    }
    let rows = parse_question_rows(lines);
    if rows.is_empty() {
        return None;
    }
    let checked = rows
        .iter()
        .filter(|r| r.kind == QuestionRowKind::Option)
        .map(|r| r.checked)
        .collect();
    let free_row = rows.iter().find(|r| r.kind == QuestionRowKind::FreeText);
    let free_text_present = free_row.is_some();
    let free_text = free_row.and_then(|r| {
        if r.label.starts_with(FREE_TEXT_ROW_LABEL) {
            None // 占位未打字
        } else {
            // 剥行尾选中标记（strip_trailing_selection_mark 文档见上）
            Some(strip_trailing_selection_mark(r.label.clone()))
        }
    });
    Some(QuestionScreenSnapshot {
        checked,
        free_text,
        free_text_present,
        heading: question_heading_of(lines),
    })
}

/// 行块里**唯一**焦点行的下标（0 或 ≥2 个焦点 → `None`——「不猜起点」）。
fn unique_focused_row(rows: &[QuestionRow]) -> Option<usize> {
    let mut hit: Option<usize> = None;
    for (i, r) in rows.iter().enumerate() {
        if r.focused {
            if hit.is_some() {
                return None;
            }
            hit = Some(i);
        }
    }
    hit
}

/// 走位方向判定（[`run_submit_stages`] 走位段）：按行块的焦点位与推进行位算
/// `"up"`/`"down"`（焦点在推进行下方 → `up`；上方 → `down`）。块解析不出或无唯一
/// 焦点 → `None`（调用方保守回落 `down`——旧口径，上限兜底）。
fn walk_direction_to_advance(lines: &[String]) -> Option<&'static str> {
    let rows = parse_question_rows(lines);
    if rows.is_empty() {
        return None;
    }
    let cur = unique_focused_row(&rows)?;
    let advance = rows
        .iter()
        .position(|r| r.kind == QuestionRowKind::Advance)?;
    Some(if cur > advance { "up" } else { "down" })
}

/// 屏上题目与载荷题干的**身份一致性**判定（2026-09-24 评审 I1 修复）：
/// 取「页签栏（含 `←` 的行）→ 首个编号行」之间的区域（题干渲染区），去空白拼接
/// 后须**包含**载荷题干的去空白全文。用户实测量到 ←/→ 可手动切题——手机卡的
/// 题号与终端脱钩后，切勾会作用到**别的题**的选项上（按键效果闭环验得过、目标
/// 身份却错）；本判据补上身份面：不匹配即中止零按键（不猜）。
///
/// 退化与兜底（如实申报）：
/// - 题干去空白后不足 4 字符 → 恒 `true`（判别力不足，不硬拦——此时仅靠屏读闭环）；
/// - 页签栏不在可见窗口（窄窗滚出）→ 区域回落为「首个编号行上方至多 4 行」；
/// - 题干被 TUI 折行/加饰（如多选后缀）→ 去空白包含判据天然覆盖（用户实机形态：
///   载荷含 `（可多选）` 时屏面逐字同现；载荷不含时屏面是否追加未取证——包含判据
///   只要求载荷侧是屏面子串，屏面多装饰不影响）。
fn screen_question_matches(lines: &[String], expected: &str) -> bool {
    // 归一化（2026-10-03 扩展）：除空白外，**剥离控制台读写损失字符**——题干里的
    // emoji 装饰（3️⃣/✨ 等：变体选择符 FE0F、组合键帽 20E3、星面字符）在控制台
    // 缓冲经 ReadConsoleOutputCharacterW 读回时可能变成 U+FFFD 或丢失（活体实证：
    // 屏面「问题3<FFFD><FFFD>：」vs 载荷「问题3<键帽>：」），逐字比对恒失败 →
    // 身份核验误中止。两侧同规则剥离后只留 CJK/字母/数字/常规标点——判别力不变。
    let norm = |s: &str| {
        s.chars()
            .filter(|c| {
                !c.is_whitespace()
                    && *c != '\u{FFFD}'
                    && !('\u{FE00}'..='\u{FE0F}').contains(c)
                    && *c != '\u{20E3}'
                    && (*c as u32) < 0x10000
            })
            .collect::<String>()
    };
    let expected = norm(expected);
    if expected.chars().count() < 4 {
        return true; // 判别力不足（超短题干）——不硬拦
    }
    // 单题集单页签行也算页签（2026-10-08，与 question_heading_of 同一提取面——
    // 评审 P2-1：否则长题干折行超出 4 行回落窗 → 身份区丢题干头部 → 假阴性中止）
    let tab_idx = lines
        .iter()
        .position(|l| l.contains('←') || is_single_question_tab_line(l));
    let first_option = lines
        .iter()
        .position(|l| parse_question_row(l).is_some_and(|(num, ..)| num.is_some()));
    let Some(first) = first_option else {
        return false; // 连编号行都没有——交给行块解析段报形态不符（此处恒 false 不影响）
    };
    let start = match tab_idx {
        Some(t) if t < first => t + 1,
        // 页签栏不在窗内：回落为首个编号行上方至多 4 行（题干就在选项区正上方）
        _ => first.saturating_sub(4),
    };
    let region = lines[start..first]
        .iter()
        .map(|l| norm(l))
        .collect::<String>();
    region.contains(&expected)
}

/// 目标选项（0 起模型选项下标）→ 行块下标（屏上编号 = 下标+1；找不到 → `None`）。
fn locate_option_row(rows: &[QuestionRow], target: usize) -> Option<usize> {
    rows.iter().position(|r| {
        r.kind == QuestionRowKind::Option && r.number.is_some_and(|n| n as usize == target + 1)
    })
}

/// 闭环切勾的编排结果（[`run_toggle_stages`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToggleOutcome {
    /// 实际发出的键（方向键 + 空格，按发出顺序；中止时含已发出的那部分）
    pub sent_keys: Vec<String>,
    /// 空格发出后屏读核验到**勾选态确实翻转**
    pub verified: bool,
    /// 翻转后的目标行勾选态（`verified=true` 时必有值；读不到屏无法核验 → `None`）
    pub checked_now: Option<bool>,
    /// **能力开关的新值**（2026-10-03 数字直选自适应）：探测/降级发生时携带，调用
    /// 侧写回 `capability` 表；`None` = 开关无变化（走位路径 / DigitUnsupported）
    pub digit_result: Option<crate::inject::capability::DigitToggle>,
}

/// **多选闭环切勾编排**（2026-09-24）：数字路径已被用户实测推翻（2.1.278 多选屏
/// 数字无反应），切勾一律走「屏读定位 → 题目身份核验 → 方向键走位（每步复核）
/// → 空格 → 屏读核验翻转」。
///
/// # 各段与中止点（每一步都在发键前屏读；任何一段不符即中止且不再发键）
///
/// 1. **题屏段**：`poll_screen` 读一屏并解析行块（[`parse_question_rows`]）。
///    解析不出块 / **屏上题干与载荷题干不一致**（[`screen_question_matches`]——
///    用户手动 ←/→ 切题后手机卡与终端脱钩，切勾会作用到别的题上）/ 无唯一焦点行 /
///    目标选项不在块内 → 中止，**零按键**；
/// 2. **走位段**：焦点已在目标行 → 直接入下一段；否则按方向发一个 `up`/`down`
///    → `settle()` → 重读复核：焦点**沿目标方向**位移恰 1 行（反向位移同样中止，
///    评审 Minor-3）。
///    复核不过（焦点不动 / 块形态崩）→ 中止**不发空格**。步数上限 = 块行数 + 2
///    （死循环兜底）；
/// 3. **切勾段**：目标行须带勾选框（多选形态）——无勾选框说明屏是单选形态，
///    中止。发 `space` → `settle()`；
/// 4. **核验段**：重读一屏定位目标行——勾选态翻转 → `verified=true`（附新态）；
///    **未翻转 → 中止**（Err：空格形态可能不被该版本消费，请到终端确认——这正
///    是对「空格注入形态未实机复验」的兜底）；读不到屏 → `Ok{verified:false,
///    checked_now:None}`（键已发出、无法核验——与 receipt_seen=None 同口径，
///    不谎报也不误报失败）。
///
/// **焦点守卫共用 helper**（2026-10-03）：光标停在自由作答行（内联编辑态）时，
/// 数字/任何注入键会被当文本打进输入框——先发 `↑` 移回选项行并双核验（焦点唯一
/// 且在 Option 行；FreeText 行文字保留）。toggle 数字直选与单选 select 共用。
/// 返回 `↑` 后的新屏行块（调用方以它为后续判定基准）。
///
/// `focus_idx` = 当前屏解析出的唯一焦点行；不在自由作答行 → 原样返回 `rows`。
fn prelift_focus_from_freetext<T: MenuTerminal>(
    terminal: &mut T,
    rows: Vec<QuestionRow>,
    focus_idx: Option<usize>,
    sent_keys: &mut Vec<String>,
    context: &str,
) -> Result<Vec<QuestionRow>, StageAbort> {
    if let Some(fi) = focus_idx {
        if rows[fi].kind == QuestionRowKind::FreeText {
            let free_label_before = rows[fi].label.clone();
            terminal.send("up").map_err(|e| {
                StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: ↑ 投递失败（{e}）"))
            })?;
            sent_keys.push("up".to_string());
            terminal.settle();
            let back = terminal.read().ok_or_else(|| {
                StageAbort::screen(format!(
                    "{context}：↑ 后读不到屏幕——已中止，未发数字；请人工核对终端"
                ))
            })?;
            let back_rows = parse_question_rows(&back);
            let on_option = unique_focused_row(&back_rows)
                .is_some_and(|i| back_rows[i].kind == QuestionRowKind::Option);
            if !on_option {
                return Err(StageAbort::screen(format!(
                    "{context}：↑ 未把焦点移出自由作答行（形态与预期不符）——已中止，未发数字；请人工核对终端"
                )));
            }
            let free_intact = back_rows
                .iter()
                .any(|r| r.kind == QuestionRowKind::FreeText && r.label == free_label_before);
            if !free_intact {
                return Err(StageAbort::screen(format!(
                    "{context}：↑ 离开自由作答行后屏读发现该行内容变化（文字或勾选可能丢失）——已中止，未发数字；请到终端核对"
                )));
            }
            return Ok(back_rows);
        }
    }
    Ok(rows)
}

pub fn run_toggle_stages<P, T>(
    target_option: usize,
    expected_question: &str,
    mut poll_screen: P,
    terminal: &mut T,
    digit: crate::inject::capability::DigitToggle,
) -> Result<ToggleOutcome, StageAbort>
where
    P: FnMut() -> Result<Option<Vec<String>>, String>,
    T: MenuTerminal,
{
    let mut sent_keys: Vec<String> = Vec::new();
    // ===== 第 1 段：题屏在场 + 行块解析 + 题目身份核验（形态不符即中止，零按键）=====
    let first = poll_screen().map_err(StageAbort::screen)?.ok_or_else(|| {
        StageAbort::screen("INJECT_SCREEN_ABORT: 读不到问答屏（屏读窗尽或不可用）——已中止，未发任何键；请人工核对终端")
    })?;
    let mut rows = parse_question_rows(&first);
    if rows.is_empty() {
        return Err(StageAbort::screen(
            "屏上解析不出问答选项块（未见推进行 Submit/Next 或编号块不连续）——已中止，未发任何键；请人工核对终端",
        ));
    }
    // 身份核验（2026-09-24 评审 I1）：屏上题干与载荷题干不一致 = 终端显示的不是
    // 手机卡当前的题（用户已手动 ←/→ 切题）——切勾会作用到别的题上，必须中止
    if !screen_question_matches(&first, expected_question) {
        return Err(StageAbort::screen(
            "屏幕上的题目与手机卡片不一致（可能已在终端手动切题）——已中止，未发任何键；请在终端回到当前题目后重试",
        ));
    }
    // ===== 数字直选自适应（2026-10-03 用户设计）：先按能力值分流 =====
    // DigitSupported/Unknown → 先发数字（Unknown = 探测：翻转记 Supported / 全程
    // 无变化记 Unsupported 并回退走位 / 有变化未翻转 = 数字被半消费 = 异常中止）；
    // DigitUnsupported → 直接走位。核验翻转的闭环在所有路径都不省。
    let mut digit_now = digit;
    // **焦点守卫**（2026-10-03 用户实测 bug）：光标停在 Type something 行（内联
    // 编辑态）时数字会被当文本打进输入框——先决把焦点移回选项行再发数字。
    // 解析不出唯一焦点 → 跳过数字分支（走位段的「不猜起点」中止接管，避免重复文案）。
    let focus_idx = unique_focused_row(&rows);
    if digit != crate::inject::capability::DigitToggle::Unsupported && focus_idx.is_some() {
        // **先决**：焦点在自由作答行 → `↑` 移回选项行（共用 helper，含文字保留
        // 核验）。不先决，数字会被内联编辑态当文本吃掉（活体实证：输入框多出 "13"）。
        // 先决后以新屏为准。
        rows = prelift_focus_from_freetext(terminal, rows, focus_idx, &mut sent_keys, "切勾")?;
        let before_checked = rows[target_option].checked;
        let attempts = if digit == crate::inject::capability::DigitToggle::Supported {
            2
        } else {
            1
        }; // Supported 失败重试一次再降级；Unknown 探测只发一发
        for attempt in 0..attempts {
            // 数字 = **屏上编号**（1 起；翻转核验也按屏上编号对位——0 起下标只存在于
            // 载荷/调用侧）
            let digit_key = (target_option + 1).to_string();
            terminal.send(&digit_key).map_err(|e| {
                StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 数字投递失败（{e}）"))
            })?;
            sent_keys.push(digit_key);
            terminal.settle();
            let after = terminal.read().ok_or_else(|| {
                StageAbort::screen(
                    "INJECT_SCREEN_ABORT: 切勾：数字发出后读不到屏幕——无法核验；请到终端核对勾选态",
                )
            })?;
            let after_rows = parse_question_rows(&after);
            let after_target = after_rows
                .iter()
                .find(|r| r.number == rows[target_option].number);
            let flipped =
                after_target.is_some_and(|r| r.checked.is_some() && r.checked != before_checked);
            if flipped {
                if digit_now == crate::inject::capability::DigitToggle::Unknown {
                    digit_now = crate::inject::capability::DigitToggle::Supported;
                }
                return Ok(ToggleOutcome {
                    sent_keys,
                    verified: true,
                    checked_now: after_target.and_then(|r| r.checked),
                    digit_result: Some(digit_now),
                });
            }
            if after_rows != rows {
                // **数字被半消费**（用户拍板的异常状态）：焦点动了/勾了别的行/形态
                // 变了——不回退走位（会在未知状态上继续发键），中止引导人工
                return Err(StageAbort::screen(
                    "切勾：数字发出后屏面发生变化但目标未翻转（数字被半消费，形态异常）——已中止；请到终端人工核对勾选态",
                ));
            }
            // 完全无变化：
            if attempt + 1 < attempts {
                continue; // Supported 的重试
            }
            if digit_now == crate::inject::capability::DigitToggle::Unknown {
                digit_now = crate::inject::capability::DigitToggle::Unsupported;
            // 探测结论：回退走位
            } else {
                digit_now = crate::inject::capability::DigitToggle::Unknown; // 降级：下次重新探测
            }
            break; // 回退走位（下方既有编排）
        }
    }
    let mut cur = unique_focused_row(&rows).ok_or_else(|| {
        StageAbort::screen(
            "问答屏上解析不到唯一焦点行（❯ 标记缺失或多行）——不猜起点，已中止，未发任何键",
        )
    })?;
    let target = locate_option_row(&rows, target_option).ok_or_else(|| {
        StageAbort::screen(format!(
            "屏上选项块里找不到第 {} 个模型选项（编号 {} 行）——已中止，未发任何键",
            target_option + 1,
            target_option + 1
        ))
    })?;
    // ===== 第 2 段：走位（方向感知；每步复核沿目标方向位移恰 1 行）=====
    let max_steps = rows.len() + 2;
    let mut steps = 0usize;
    while cur != target {
        if steps >= max_steps {
            return Err(StageAbort::screen(format!(
                "已发 {steps} 个方向键仍未把焦点移到目标选项行（上限 {max_steps}）——已中止，未发空格；请人工核对终端"
            )));
        }
        let key = if target > cur { "down" } else { "up" };
        terminal.send(key).map_err(|e| {
            StageAbort::delivery(format!(
                "INJECT_DELIVERY_FAIL: 方向键 {key} 投递失败（{e}）"
            ))
        })?;
        sent_keys.push(key.to_string());
        steps += 1;
        terminal.settle();
        let lines = terminal.read().ok_or_else(|| {
            StageAbort::screen(format!(
                "走位段读不到屏幕（已发 {steps} 个方向键）——已中止，未发空格（不盲切）"
            ))
        })?;
        let next_rows = parse_question_rows(&lines);
        if next_rows.is_empty() {
            return Err(StageAbort::screen(
                "步进后屏上解析不出问答选项块（形态崩）——已中止，未发空格；请人工核对终端",
            ));
        }
        let next_cur = unique_focused_row(&next_rows).ok_or_else(|| {
            StageAbort::screen("INJECT_SCREEN_ABORT: 步进后屏上解析不到唯一焦点行——已中止，未发空格；请人工核对终端")
        })?;
        // 位移校验（评审 Minor-3）：必须**沿目标方向**恰好 1 行——反向位移（终端
        // 自行移动了焦点）同样中止，防「乒乓走到上限」的浪费按键路径
        let expected_move: i64 = if key == "down" { 1 } else { -1 };
        let moved = next_cur as i64 - cur as i64;
        if moved != expected_move {
            return Err(StageAbort::screen(format!(
                "按一次 {key} 后焦点位移了 {moved} 行（应沿目标方向恰 1 行）——屏幕行序与预期不一致，已中止（未发空格）"
            )));
        }
        rows = next_rows;
        cur = next_cur;
    }
    // ===== 第 3 段：切勾（目标行必须带勾选框——多选形态）=====
    let before = rows[target].checked.ok_or_else(|| {
        StageAbort::screen(
            "目标选项行没有勾选框（屏是单选形态？）——toggle 仅用于多选题，已中止，未发空格",
        )
    })?;
    terminal
        .send("space")
        .map_err(|e| StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 空格投递失败（{e}）")))?;
    sent_keys.push("space".to_string());
    terminal.settle();
    // ===== 第 4 段：核验翻转 =====
    let Some(after_lines) = terminal.read() else {
        // 键已发出、读不到屏无法核验——不谎报成功也不误报失败（receipt_seen=None 口径）
        return Ok(ToggleOutcome {
            sent_keys,
            verified: false,
            checked_now: None,
            digit_result: (digit_now != digit).then_some(digit_now),
        });
    };
    let after_rows = parse_question_rows(&after_lines);
    let after = after_rows
        .iter()
        .find(|r| r.kind == QuestionRowKind::Option && r.number == rows[target].number)
        .and_then(|r| r.checked);
    match after {
        Some(now) if now != before => Ok(ToggleOutcome {
            sent_keys,
            verified: true,
            checked_now: Some(now),
            digit_result: (digit_now != digit).then_some(digit_now),
        }),
        Some(_) => Err(StageAbort::screen(
            "空格已发出但屏读未见到勾选态翻转——该版本可能不消费此空格形态；已中止（无后续键）。请到终端确认后重试",
        )),
        None => Err(StageAbort::screen(
            "空格发出后屏上找不到目标选项行（题屏已切换或形态崩）——已中止。请到终端确认当前状态",
        )),
    }
}

/// 切题**方向**（2026-10-02 用户裁决：←/→ 通用对应上一题/下一题，除
/// `Type something` 行外——该行 ←/→ 归文本编辑光标，发前先 `↑` 回选项行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavDirection {
    /// 下一题（`→`；末题直达 Review——活体取证）
    Next,
    /// 上一题（`←`；从 Review 退回上一题——活体取证）
    Prev,
}

/// 多题**切题编排**的结果（[`run_advance_stages`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvanceOutcome {
    /// 实际发出的键（↑ 先决 + ←/→；中止时含已发出的那部分）
    pub sent_keys: Vec<String>,
    /// 终端是否已切题：`true` = 进入另一题页**或** Review 确认屏（前端按方向移动
    /// mqIndex）；`false` = 已在 Review 屏请求下一题、**零按键**（已在终点）
    pub advanced: bool,
}

/// **claude 多题切题编排**（2026-09-24 立项；2026-10-02 活体取证 2.1.278 后重构为
/// **←/→ 双向导航**，走位+回车路径退役——用户裁决：除 `Type something` 行外
/// ←/→ 通用对应上一题/下一题，活体取证背书：←/→ 是含 Review 的导航环、末题
/// `→` 直达 Review、`Type something` 行 ←/→ 归文本编辑光标无效）。
///
/// # 各段与中止点（每一步都在发键前屏读；任何一段不符即中止且不再发键）
///
/// 1. **读屏段**：`poll_screen` 读一屏。
///    - **已在 Review 确认屏**：`Next` → `advanced=false` 零按键（已在终点）；
///      `Prev` → 发 `←` → 分类（退回上一题页 = 题干区变化）；
///    - 题屏：先决段 → 导航段 → 分类段；
///    - 两者皆非 → 中止零按键；
/// 2. **先决段**（仅题屏）：焦点在自由作答行 → 先 `↑` 回选项行并复核（两行例外）；
/// 3. **导航段**：发 `→`（Next）/ `←`（Prev）；
/// 4. **分类段**（[`classify_navigation`]）：Review 在场（仅 Next）→ `advanced=true`
///    （前端进确认卡）；解析出题屏且**题干区变化** → `advanced=true`；键被吞/未
///    生效（题屏与题干均未变，或 Prev 仍停在 Review）→ 中止（无后续键）。
///
/// # 首段轮询的「屏已可行动」判据
///
/// [`advance_stage_screen_ready`] 与第 1 段的接受条件同源（Review 就绪 ∨ 可解析出
/// 题屏）；生产轮询缝（`remote::api.rs`）用它过滤**中绘屏**——读到形态未就绪的屏
/// 时留在轮询窗里等形态，而不是把它当「第 1 段的屏」交给阶段机立即中止。
pub fn run_advance_stages<P, T>(
    mut poll_screen: P,
    terminal: &mut T,
    direction: NavDirection,
) -> Result<AdvanceOutcome, StageAbort>
where
    P: FnMut() -> Result<Option<Vec<String>>, String>,
    T: MenuTerminal,
{
    let mut sent_keys: Vec<String> = Vec::new();
    // ===== 第 1 段：读屏 =====
    let first = poll_screen().map_err(StageAbort::screen)?.ok_or_else(|| {
        StageAbort::screen(
            "INJECT_SCREEN_ABORT: 切题前读不到问答屏（屏读窗尽或不可用）——已中止，未发任何键；请人工核对终端",
        )
    })?;
    let pre_heading = question_heading_of(&first);
    // M2：Review **标题在场**即视为确认屏起点——重绘中 probe 可能 NotYet（确认项
    // 还没画出），此刻按题屏处理会误发导航键
    let started_on_review = lines_has_review_hint(&first);
    let rows = parse_question_rows(&first);
    if started_on_review {
        // Review 屏：Next = 已在终点（零按键 advanced:false，前端留在确认卡）；
        // Prev = `←` 退回上一题（活体取证：←/→ 是含 Review 的导航环）。
        return match direction {
            NavDirection::Next => Ok(AdvanceOutcome {
                sent_keys,
                advanced: false,
            }),
            NavDirection::Prev => {
                terminal.send("left").map_err(|e| {
                    StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: ← 投递失败（{e}）"))
                })?;
                sent_keys.push("left".to_string());
                terminal.settle();
                poll_navigation_result(
                    terminal,
                    &pre_heading,
                    direction,
                    sent_keys,
                    started_on_review,
                )
            }
        };
    }
    if rows.is_empty() {
        return Err(StageAbort::screen(
            "切题失败：屏上既无推进行也解析不出题屏（形态不符）——已中止，未发任何键；请人工核对终端",
        ));
    }
    // ===== 题屏：先决（焦点在自由作答行 → `↑` 回选项行，两方向共用）=====
    // 两行例外（活体复核）：`Type something` 行的 ←/→ 被文本编辑光标消费，发了无效。
    let cur = unique_focused_row(&rows).ok_or_else(|| {
        StageAbort::screen(
            "切题：屏上解析不到唯一焦点行（❯ 标记缺失或多行）——不猜起点，已中止，未发任何键",
        )
    })?;
    if rows[cur].kind == QuestionRowKind::FreeText {
        // 打过字的自由作答行（2026-10-02 多选自由作答接入后常见）：↑ 前记录行内容，
        // ↑ 后核验**文字与勾选都还在**——↑ 若有意外副作用（吞字/取消勾选），切题就
        // 会把半途状态存进去，必须中止引导人工
        let free_label_before = rows[cur].label.clone();
        terminal.send("up").map_err(|e| {
            StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: ↑ 投递失败（{e}）"))
        })?;
        sent_keys.push("up".to_string());
        terminal.settle();
        let back = terminal.read().ok_or_else(|| {
            StageAbort::screen(
                "INJECT_SCREEN_ABORT: 切题：↑ 后读不到屏幕——已中止，未发 ←/→；请人工核对终端",
            )
        })?;
        let back_rows = parse_question_rows(&back);
        let on_option = unique_focused_row(&back_rows)
            .is_some_and(|i| back_rows[i].kind == QuestionRowKind::Option);
        if !on_option {
            return Err(StageAbort::screen(
                "切题：↑ 未把焦点移回选项行（两行例外形态与取证不符）——已中止，未发 ←/→；请人工核对终端",
            ));
        }
        let free_line_intact = back_rows
            .iter()
            .any(|r| r.kind == QuestionRowKind::FreeText && r.label == free_label_before);
        if !free_line_intact {
            return Err(StageAbort::screen(
                "切题：↑ 离开自由作答行后屏读发现该行内容变化（文字或勾选可能丢失）——已中止，未发 ←/→；请到终端核对",
            ));
        }
    }
    // ===== 导航（←/→ 通用：活体取证除 Type something 行外 ←=上一题、→=下一题/
    // 末题直达 Review）=====
    let key = match direction {
        NavDirection::Next => "right",
        NavDirection::Prev => "left",
    };
    terminal.send(key).map_err(|e| {
        StageAbort::delivery(format!(
            "INJECT_DELIVERY_FAIL: {} 投递失败（{e}）",
            if key == "left" { "←" } else { "→" }
        ))
    })?;
    sent_keys.push(key.to_string());
    terminal.settle();
    poll_navigation_result(
        terminal,
        &pre_heading,
        direction,
        sent_keys,
        started_on_review,
    )
}

/// Review 确认屏的**宽判据**（标题/副题锚任一在场即算——不管确认项是否就绪）。
/// 用于切题起点的形态归类（M2：重绘中 probe_review_screen 可能 NotYet）。
fn lines_has_review_hint(lines: &[String]) -> bool {
    lines.iter().any(|l| {
        let low = l.to_lowercase();
        low.contains(review_title_anchor()) || low.contains(review_subtitle_anchor())
    })
}

/// 切题分类段（**有界轮询**，2026-10-03 时序修复）：`→`/`←` 之后 TUI 重绘可能
/// 超过 settle 延迟——单次读屏会把「已切题但还没画完」误判成键被吞（活体事故：
/// 终端已进下一题、分类段抓到旧屏中止）。窗内轮询（2s 拍数）：
/// - Review 屏在场且 Next → 已切（末题直达 Review）；Prev → 仍在确认屏 = 还没退回，
///   继续轮询；
/// - 解析出题屏且**题干区变化** → 已切；
/// - 窗尽 → 方向性诚实中止（起点区分「确认屏退回」与「题页切换」两种文案）。
///
/// **窗的真实时长**：拍数 × `terminal.settle()`（生产 = `SUBMIT_DELAY_MS`=150ms），
/// 非拍数 × `POLL_STEP_MS`——20 拍 ≈ 2.9s（比三窗口径长，保守方向；文档如实申报，
/// 评审 M1）。
fn poll_navigation_result<T: MenuTerminal>(
    terminal: &mut T,
    pre_heading: &str,
    direction: NavDirection,
    sent_keys: Vec<String>,
    started_on_review: bool,
) -> Result<AdvanceOutcome, StageAbort> {
    let rounds =
        crate::inject::timing::poll_rounds(crate::inject::timing::QUESTION_STAGE_POLL_TOTAL_MS)
            .max(1);
    for i in 0..rounds {
        let after = terminal.read().ok_or_else(|| {
            StageAbort::screen(
                "INJECT_SCREEN_ABORT: 切题：←/→ 后读不到屏幕——无法确认是否切题；请人工核对终端",
            )
        })?;
        let on_review = matches!(probe_review_screen(&after), ScreenStep::Ready(_));
        match (direction, on_review) {
            (NavDirection::Next, true) => {
                return Ok(AdvanceOutcome {
                    sent_keys,
                    advanced: true,
                });
            }
            (NavDirection::Prev, true) => {} // ← 未生效（仍在确认屏）：继续轮询
            _ => {
                let rows = parse_question_rows(&after);
                if !rows.is_empty() && question_heading_of(&after) != pre_heading {
                    return Ok(AdvanceOutcome {
                        sent_keys,
                        advanced: true,
                    });
                }
            }
        }
        if i + 1 < rounds {
            terminal.settle();
        }
    }
    Err(StageAbort::screen(match (direction, started_on_review) {
        (NavDirection::Prev, true) => {
            "切题：← 未从确认屏退回题目（按键可能被吞）——已中止（无后续键）。请到终端核对当前题目"
        }
        (NavDirection::Prev, false) => {
            "切题：← 未切换题目（可能已是第一题或按键被吞）——已中止（无后续键）。请到终端核对当前题目"
        }
        _ => "切题后既未进入下一题页、也未出现 Review 确认屏——已中止（无后续键）。请到终端确认当前状态",
    }))
}

/// 切题第 1 段「屏已可行动」判据：Review 屏就绪 ∨ 可解析出题屏（解析器双锚后，
/// 推进行锚与页签栏锚覆盖多选/单选子题——旧 submit_row_present 分支随走位退役）。
/// 与 [`run_advance_stages`] 第 1 段的接受条件同源（见该函数文档的「首段轮询判据」），
/// 供生产轮询缝把中绘屏挡在轮询窗里（等形态，而不是读到什么就用什么）。
pub(crate) fn advance_stage_screen_ready(lines: &[String]) -> bool {
    matches!(probe_review_screen(lines), ScreenStep::Ready(_))
        || !parse_question_rows(lines).is_empty()
}

/// 多选题提交的**阶段机编排结果**（[`run_submit_stages`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitOutcome {
    /// 各阶段**实际发出**的键序（按发出顺序；中止时含已发出的那部分）
    pub sent_keys: Vec<String>,
    /// 走位段实际按下的 ↓ 次数（0 = 焦点本来就在推进行/无需下移）
    pub down_steps: usize,
    /// 走位段实际按下的 ↑ 次数（2026-09-24 方向感知走位：焦点在推进行**下方**时上移）
    pub up_steps: usize,
    /// Review 屏是否屏读确认过（`false` 只可能出现在中止路径上——本字段在成功路径
    /// 恒 `true`；保留它是为了让回执能区分「走完闭环」与「半路停住」）
    pub review_confirmed: bool,
    /// 确认后是否屏读到终态回执行（`None` = 整个窗口一次都没读到屏，无法核验）
    pub receipt_seen: Option<bool>,
}

/// **多选题提交的闭环编排**（§2.3 裁4）。读屏/发键/等待全走 [`MenuTerminal`]，
/// 故整条控制流在门禁里可用脚本化屏序列覆盖（同
/// [`crate::inject::mode::run_menu_stages`] 的抽法，理由同源：段与段之间的控制流是
/// 判据的一部分，写在 `#[cfg(windows)]` 的端点闭包里就只有实机能覆盖）。
///
/// # 各段与中止点（**每一步都在发键前屏读**，任何一段不符即中止且不再发键）
///
/// 1. **提交屏段**：屏读确认 `Submit` 行在场（[`probe_submit_screen`]）。不在场 →
///    中止，**零按键**；
/// 2. **走位段**：每次发一个 `↓` → `settle()` → **重读屏复核**；复核判据 = 提交行仍在
///    场（形态未崩）+ 焦点标记已落到提交行（后者成立即停手）。走位次数上限
///    `max_down_steps` 由调用方给（选项数 + 2：跨过模型选项与 TUI 追加的
///    `Type something` 行），超限 → 中止**不发回车**（防死循环、防在错误位置上回车）；
/// 3. **提交段**：焦点确认落在提交行后才发 `enter`（**唯一的回车点**）；
/// 4. **Review 段**：发完 `enter` 后轮询屏读等 Review 屏画出。**未见 → 中止且不发
///    数字**（这正是任务书点名的缺陷：原实现在没确认 Review 屏在场时就发 `'1'`）。
///    在场 → 取屏上确认项原文的**首个数字**作为确认键（屏上编号由 TUI 给，不硬编码）；
/// 5. **终态段**：发确认键后轮询屏读终态锚——**未见不是失败**：
///    `receipt_seen = Some(false)`，调用方如实下发「已按闭环提交，但未屏读到完成回执，
///    请人工核对」。这一段的结论**不**回滚已发生的事实（键确实发出去了）。
///
/// # 参数
///
/// `poll_submit`——第 1 段轮询（**提交屏在场**，不看焦点）。`poll_review`——Review 屏
/// 轮询。二者语义与 [`crate::inject::mode::run_menu_stages`] 的 `poll` 一致：
/// `Ok(Some(屏))` = 目标形态出现；`Ok(None)` = 窗尽未出现；`Err` = 形态异常（立即中止）。
///
/// `poll_receipt`——终态回执轮询，**语义与 `run_menu_stages` 的 `poll_receipt` 逐条
/// 对齐**（本函数不复用那个闭包，但语义必须同源，否则两处的 `receipt_seen` 三态会在
/// 用户可见文案上漂移）：`Ok(Some(屏))` = 屏上出现终态回执行；`Ok(None)` = 窗内读到
/// 了屏但**未命中**回执行（读屏能力正常，只是没看到）；`Err` = 读屏本身不可用。
/// 后两者在编排里都收敛为「无法断言成功」（`Some(false)` / `None`），差别只在措辞。
///
/// 生产实现见 `remote::api` 的对应轮询；测试用脚本化序列。
pub fn run_submit_stages<P, Q, R, T>(
    mut poll_submit: P,
    mut poll_review: Q,
    mut poll_receipt: R,
    terminal: &mut T,
    max_down_steps: usize,
) -> Result<SubmitOutcome, StageAbort>
where
    P: FnMut() -> Result<Option<Vec<String>>, String>,
    Q: FnMut() -> Result<Option<Vec<String>>, String>,
    R: FnMut() -> Result<Option<Vec<String>>, String>,
    T: MenuTerminal,
{
    let mut sent_keys: Vec<String> = Vec::new();
    // ===== 第 1 段：提交屏在场（不在场即中止，零按键）=====
    // poll 的 Err = 形态异常（屏读类）——闭包签名保持既有 `Result<_, String>` 契约，
    // 类别在编排这一层补齐（判据与分类同处一地，调用方不必知道）
    let first = poll_submit().map_err(StageAbort::screen)?.ok_or_else(|| {
        StageAbort::screen(
            "多选提交屏未出现或读不到屏幕（屏上无 Submit 行）——已中止，未发任何键；请人工核对终端",
        )
    })?;
    // ===== 第 1.5 段（2026-09-24 多题流）：已在 Review 确认屏 → 跳过走位与回车 =====
    //
    // 多题形态下末题的 `Next`+回车已把终端带到 Review 屏（前端确认卡上的「提交答案」
    // 从这里发起）——此时屏上没有推进行，原判据会误报「未见 Submit 行」中止。判据
    // 单点复用 [`probe_review_screen`]：Review 在场 → 直接进第 4 段确认（零走位零回车）。
    let mut down_steps = 0usize;
    let mut up_steps = 0usize;
    let review_lines = if let ScreenStep::Ready(l) = probe_review_screen(&first) {
        l
    } else {
        if !first.iter().any(|l| submit_row_present(l)) {
            return Err(StageAbort::screen(
                "屏读未见到 Submit 行（多选提交入口）——已中止，未发任何键；请人工核对终端",
            ));
        }
        // ===== 第 2 段：走位（**方向感知**；每步复核；到位才停）=====
        //
        // 2026-09-24 起支持向上走位：切勾编排（run_toggle_stages）走完后焦点可能停在
        // 任意行（含推进行下方的行不可达，但选项区中部/上方都可能）——原实现只发 ↓，
        // 焦点在推进行上方时仍正确，焦点**高于**目标时永远走不到。方向按行块解析的
        // 焦点位与推进行位算（解析不出方向时保守回落 ↓——与旧行为一致，上限兜底）。
        loop {
            let lines = terminal.read().ok_or_else(|| {
                StageAbort::screen(format!(
                    "走位段读不到屏幕（已发 {} 个方向键）——已中止，未发回车（不盲提交）",
                    down_steps + up_steps
                ))
            })?;
            match probe_submit_row(&lines) {
                ScreenStep::Ready(_) => break, // 焦点已在推进行
                ScreenStep::NotYet(_) => {}
                ScreenStep::Fatal(why) => {
                    return Err(StageAbort::screen(format!(
                        "INJECT_MANUAL_CHECK: {why}；请人工核对终端"
                    )))
                }
            }
            if down_steps + up_steps >= max_down_steps {
                return Err(StageAbort::screen(format!(
                    "已发 {} 个方向键仍未把焦点移到推进行（上限 {max_down_steps}）——已中止，未发回车；请人工核对终端",
                    down_steps + up_steps
                )));
            }
            // 方向判定：行块里焦点位 vs 推进行位（块解析不出 → 回落 ↓，旧口径）
            let key = walk_direction_to_advance(&lines).unwrap_or("down");
            terminal.send(key).map_err(|e| {
                StageAbort::delivery(format!(
                    "INJECT_DELIVERY_FAIL: 方向键 {key} 投递失败（{e}）"
                ))
            })?;
            sent_keys.push(key.to_string());
            if key == "up" {
                up_steps += 1;
            } else {
                down_steps += 1;
            }
            terminal.settle();
        }
        // ===== 第 3 段：提交（焦点已在提交行——唯一的回车点）=====
        terminal.send("enter").map_err(|e| {
            StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 提交回车投递失败（{e}）"))
        })?;
        sent_keys.push("enter".to_string());
        terminal.settle();
        // ===== 第 4 段：Review 屏（未见即中止，**不发数字**）=====
        poll_review().map_err(StageAbort::screen)?.ok_or_else(|| {
            StageAbort::screen(format!(
                "已发回车但屏上未出现 Review 确认屏（未见「{}」/「{}」）——已中止，未发确认键；请人工核对终端",
                review_title_anchor(),
                review_subtitle_anchor()
            ))
        })?
    };
    // 复核（轮询拿到的屏再走一次同一判据——轮询闭包与复核用同一份探测器，
    // 不假定「轮询返回的就一定合判据」；形态在两次读之间变化时这里会如实拒）
    if let ScreenStep::NotYet(why) | ScreenStep::Fatal(why) = probe_review_screen(&review_lines) {
        return Err(StageAbort::screen(format!(
            "INJECT_MANUAL_CHECK: {why}；请人工核对终端"
        )));
    }
    // 确认项行 → 屏上编号（屏上编号由 TUI 给，不硬编码 '1'）
    // 确认项的唯一性已由上面那次 `probe_review_screen` 复核过（恰 1 行），此处取它，
    // 并把**屏上编号**作为确认键（`review_confirm_items` 直接给出编号与行原文）
    let (confirm_num, confirm_line) = review_confirm_items(&review_lines)
        .into_iter()
        .next()
        .ok_or_else(|| {
            StageAbort::screen(
                "Review 确认屏在场但读不到带编号的确认项——已中止，未发确认键；请人工核对终端",
            )
        })?;
    let confirm_key = confirm_num.to_string();
    log::debug!("问答提交阶段机：Review 确认项「{confirm_line}」→ 确认键 '{confirm_key}'");
    terminal.send(&confirm_key).map_err(|e| {
        StageAbort::delivery(format!(
            "INJECT_DELIVERY_FAIL: 确认键 {confirm_key} 投递失败（{e}）"
        ))
    })?;
    sent_keys.push(confirm_key);
    terminal.settle();
    // ===== 第 5 段：终态回执核验（未见**不是失败**）=====
    //
    // 语义（与 `mode::run_menu_stages` 的 `receipt_seen` 同源）：
    // - `Some(true)`：屏上出现终态回执行（工具自己宣布答完——比任何推断强）；
    // - `Some(false)`：**读到了屏但没有回执行**——「可能答完了但没看到确认」，
    //   如实回执 `verified=false` + 请人工核对；
    // - `None`：读屏不可用（`Err`/窗尽无一屏）——无法核验。
    //   三者中后两者都**不**谎报完成，差别只在回执文案的措辞。
    let receipt_seen = stage_receipt_seen(&mut poll_receipt, probe_answered_receipt);
    Ok(SubmitOutcome {
        sent_keys,
        down_steps,
        up_steps,
        review_confirmed: true,
        receipt_seen,
    })
}

/// 自由作答的**阶段机编排结果**（[`run_free_text_stages`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreeTextOutcome {
    /// 保存后 TUI 已直达 Review/Submit 汇总屏（末题/全部已答自动汇总）
    pub review_reached: bool,
    /// **TUI 已自动推进下一题**（单选保存后自动推进形态，2026-10-07）——前端据此
    /// 同步 mqIndex+1（多选留原页 = false，切题交还用户）
    pub advanced: bool,
    /// 实际发出的键（含定位数字与末尾回车；文本以占位符形式出现——不落正文）
    pub sent_keys: Vec<String>,
    /// 是否屏读到终态回执行（`None` = 读不到屏，无法核验）
    pub receipt_seen: Option<bool>,
    /// **屏读真值**（2026-10-03 卡面状态以屏读为准）：打字后该行的屏上文本
    ///（内联编辑生效形态；`None` = 单题路径未采集）
    pub screen_text: Option<String>,
    /// 屏读真值：该行勾选态（多选路径勾选兜底后恒 `Some(true)`；`None` = 未采集）
    pub screen_checked: Option<bool>,
}

/// 屏上的自由作答行（[`locate_free_text_row`] 的产物）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreeTextRow {
    /// 屏上编号（TUI 给的，作为定位键）
    pub digit: Option<String>,
    /// 该行原文（回执与日志用）
    pub text: String,
}

/// 在屏上定位自由作答行（**第一个**匹配行）。判据：编号行 + 文本以
/// [`FREE_TEXT_ROW_LABEL`] 开头（前缀匹配，覆盖 `Type something` / `Type something.`
/// 两形态）。
///
/// **为什么取第一个**：单题卡只有一行自由作答入口（多题卡的翻页形态 v1 只读，
/// 走不到这条编排）；真出现两行时取第一行与「用户看到的第一个入口」一致。
fn locate_free_text_row(lines: &[String]) -> Option<FreeTextRow> {
    lines.iter().find_map(|line| {
        // 复用 dialog 的编号行解析（行模式与光标标记剥离口径只此一份，
        // 见 `crate::inject::dialog::parse_option_line` 的丁T5 注）
        let (num, label, _hl) = crate::inject::dialog::parse_option_line(line)?;
        if !label.starts_with(FREE_TEXT_ROW_LABEL) {
            return None;
        }
        Some(FreeTextRow {
            digit: Some(num.to_string()),
            text: format!("{num}. {label}"),
        })
    })
}

/// 自由作答的**终端 IO 能力包**——比 [`MenuTerminal`] 多一个**文本通道**
/// （[`MenuTerminal`] 只有单键 `send`）。
///
/// # 为什么必须是独立 trait（裁3 的安全面）
///
/// 「文本」与「键」是两条**语义完全不同**的通道：文本走字符注入（任意字符都只是
/// 字符），键走键通道（`down`/`enter`/`esc` 是控制语义）。若让自由作答复用
/// [`MenuTerminal::send`]，用户文本就会被送进**键通道**——那正是裁3 禁止的
/// 「用户输入被对话框理解成选项选择」的形态之一（K9：多选框的 Enter 是切换勾选）。
/// 类型上分开，写错就编译不过。
pub trait FreeTextTerminal {
    /// 读一屏（`None` = 读不到屏）
    fn read(&mut self) -> Option<Vec<String>>;
    /// 发一个**键**（`Err` = 投递失败，立即中止）
    fn send(&mut self, key: &str) -> Result<(), String>;
    /// 发**文本**（字符通道；实现方须保证这是文本注入而非键注入）
    fn send_text(&mut self, text: &str) -> Result<(), String>;
    /// 按键/文本后给 TUI 重绘留时间
    fn settle(&mut self);
}

/// 三闭包 + 文本闭包 → [`FreeTextTerminal`] 的生产/测试适配器（零语义纯转发，
/// 与 [`crate::inject::mode::Closures`] 同款）。
pub struct FreeTextClosures<R, S, X, W> {
    /// 读屏
    pub read: R,
    /// 发键
    pub send: S,
    /// 文本注入通道
    pub send_text: X,
    /// 步进等待
    pub settle: W,
}

impl<R, S, X, W> FreeTextTerminal for FreeTextClosures<R, S, X, W>
where
    R: FnMut() -> Option<Vec<String>>,
    S: FnMut(&str) -> Result<(), String>,
    X: FnMut(&str) -> Result<(), String>,
    W: FnMut(),
{
    fn read(&mut self) -> Option<Vec<String>> {
        (self.read)()
    }
    fn send(&mut self, key: &str) -> Result<(), String> {
        (self.send)(key)
    }
    fn send_text(&mut self, text: &str) -> Result<(), String> {
        (self.send_text)(text)
    }
    fn settle(&mut self) {
        (self.settle)()
    }
}

/// **自由作答的闭环编排**（§2.4 入口 1，v1 仅 claude 单题卡）。
///
/// # 序列与各段判据（实机 K4–K6 定案 + 本批的屏读闭环）
///
/// 1. **定位段**：屏读找 `N. Type something`（[`FREE_TEXT_ROW_LABEL`]）。找得到即取
///    该行**屏上编号**作定位键（抄屏上编号，不猜 `选项数+1`）；找不到 → 中止且
///    **不发任何键**（直接打字是无效操作，K7 实机抓获）；
/// 2. **文本段**：发定位数字（**仅移动焦点，不提交**，K4）→ `settle()` → **重读复核**
///    焦点确已落上（复核判据 = 该行**仍在场**；claude 的自由作答行获得焦点后会把占位
///    文本换成可编辑输入，行内文本会变但行标签保留）→ 再发文本。**顺序倒过来
///    （先文本后定位）是 K7 的无效操作**；
/// 3. **提交段**：发 `enter`（K6——实测提交该文本为答案）；
/// 4. **终态段**：轮询终态锚，语义同 [`run_submit_stages`] 的第 5 段。
///
/// # 裁3 的安全面（**本函数的存在理由**）
///
/// 用户文本**只经** [`FreeTextTerminal::send_text`]（字符通道）投递，**绝不**走
/// [`FreeTextTerminal::send`]（键通道）——因此文本里的任意字符（含数字、含 `Esc`
/// 字面）都不可能被当成选项选择或控制键。定位数字是**本函数自己产生**的（来自屏上
/// 编号），与用户文本零关系；文本之后**只**跟一个回车（提交），**不带任何数字或
/// Esc**（K2：数字后补 Esc 会中断模型回合）。
///
/// # 参数
///
/// `poll_free_row`：轮询屏读自由作答行（`Ok(Some(屏))` / `Ok(None)` 窗尽 / `Err`）。
/// `poll_receipt`：终态轮询（语义同提交路径）。
/// `text`：**已归一**的用户文本（调用方负责过
/// [`crate::inject::normalize::normalize_newlines`]——注入通道唯一出口的安全面）。
pub fn run_free_text_stages<P, R, T>(
    text: &str,
    mut poll_free_row: P,
    mut poll_receipt: R,
    terminal: &mut T,
) -> Result<FreeTextOutcome, StageAbort>
where
    P: FnMut() -> Result<Option<Vec<String>>, String>,
    R: FnMut() -> Result<Option<Vec<String>>, String>,
    T: FreeTextTerminal,
{
    if text.trim().is_empty() {
        // 「文本为空」是**参数问题**（端点已在入口 400 拦），落到这里属防御：
        // 归为屏读类（形态不符）而非投递类——**没有发生任何投递**
        return Err(StageAbort::screen(
            "INJECT_SCREEN_ABORT: 自由作答文本为空——已中止，未发任何键",
        ));
    }
    let mut sent_keys: Vec<String> = Vec::new();
    // ===== 第 1 段：自由作答行在场（不在场即中止，零按键——K7：未定位直接打字无效）=====
    // poll 的 Err = 形态异常（屏读类）——闭包签名保持既有契约，类别在这一层补齐
    let lines = poll_free_row()
        .map_err(StageAbort::screen)?
        .ok_or_else(|| {
            StageAbort::screen(format!(
            "屏上未出现自由作答行（「{FREE_TEXT_ROW_LABEL}」）——已中止，未发任何键；请人工核对终端"
        ))
        })?;
    let row = locate_free_text_row(&lines).ok_or_else(|| {
        StageAbort::screen(format!(
            "屏读未定位到自由作答行（「{FREE_TEXT_ROW_LABEL}」）——已中止，未发任何键；请人工核对终端"
        ))
    })?;
    let locate_key = row.digit.ok_or_else(|| {
        StageAbort::screen(format!(
            "自由作答行「{}」里读不到编号——已中止，未发任何键；请人工核对终端",
            row.text
        ))
    })?;
    // ===== 第 2 段：定位（仅移动焦点，不提交）=====
    terminal.send(&locate_key).map_err(|e| {
        StageAbort::delivery(format!(
            "INJECT_DELIVERY_FAIL: 定位键 {locate_key} 投递失败（{e}）"
        ))
    })?;
    sent_keys.push(locate_key.clone());
    terminal.settle();
    // **重读复核**：定位键之后必须重新屏读、确认自由作答行**仍在场**（焦点确实落上
    // 去了），而不是盲发作答文本。
    let after = terminal.read().ok_or_else(|| {
        StageAbort::screen(format!(
            "发定位键 {locate_key} 后读不到屏幕——已中止，未发作答文本（不盲打字）；请人工核对终端"
        ))
    })?;
    if locate_free_text_row(&after).is_none() {
        return Err(StageAbort::screen(format!(
            "发定位键 {locate_key} 后屏上已见不到自由作答行——形态与预期不符，已中止，未发作答文本；请人工核对终端"
        )));
    }
    // ===== 第 3 段：文本（唯一走字符通道的一步）=====
    terminal.send_text(text).map_err(|e| {
        StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 作答文本投递失败（{e}）"))
    })?;
    // 审计/回执里的键序用**占位符**记文本（不落正文——正文属用户隐私，且长度可泄露
    // 答案形态）；字符数足以证明「投了什么规模的东西」。
    sent_keys.push(format!("<text:{} chars>", text.chars().count()));
    terminal.settle();
    // ===== 第 4 段：提交（回车；**不带数字、不带 Esc**——K2）=====
    terminal.send("enter").map_err(|e| {
        StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 提交回车投递失败（{e}）"))
    })?;
    sent_keys.push("enter".to_string());
    terminal.settle();
    // ===== 第 5 段：终态回执核验（未见**不是失败**，同提交路径）=====
    let receipt_seen = stage_receipt_seen(&mut poll_receipt, probe_answered_receipt);
    Ok(FreeTextOutcome {
        sent_keys,
        receipt_seen,
        advanced: false,
        review_reached: false,
        screen_text: None,
        screen_checked: None,
    })
}

/// **多选题自由作答编排**（2026-10-02 活体取证 2.1.278 后新增）。
///
/// # 实机取证结论（本编排的每一段都由它背书）
///
/// - 多选屏的自由作答行是**勾选框行**：`5. [ ] Type something`——焦点在该行时
///   **直接打字符即进入内联编辑**（无需先按空格/回车进入编辑态，用户实机证实）；
///   文字替换行内容、勾选框自动置 `[✓]`；
/// - **打完字按回车 = 取消勾选**（文字保留但 `[ ]`，Review 不保存该答案）——
///   本编排**绝不发 enter**；
/// - 正确保存 = 打字后**不发任何键**，勾选保持，切题（←/→）时自然落盘。
///
/// # 各段与中止点（每一步发键前屏读；任何一段不符即中止且不再发键）
///
/// 1. **定位段**：`parse_question_rows` 找 `kind == FreeText` 行（解析器已剥勾选框，
///    label 判据与勾选框形态族解耦——闸门 3）。找不到 → 中止零键；
/// 2. **走位段**：多选屏**数字无反应**（K8 已推翻）——方向键走位（方向感知、每步
///    复核位移恰 1 行，与 [`run_toggle_stages`] 同纪律）把焦点移到该行；
/// 3. **打字段**：`send_text`（**字符通道**，同单题自由作答的安全面）；
/// 4. **核验段**：重读屏——该行 label **不再以 `Type something` 开头**（内联编辑
///    确实生效）**且勾选态 `Some(true)`**（勾选自动置上）。任一不成立 → 中止
///    （回执引导用户到终端核对，**不补键**——补键可能踩回车取消勾选的雷）；
/// 5. **收尾**：零后续键（焦点留在该行；切题由 [`run_advance_stages`] 的 `↑` 先决
///    接管——↑ 离开本行后 ←/→ 导航恢复）。
pub fn run_multi_select_free_text_stages<T>(
    text: &str,
    overwrite: bool,
    poll_screen: impl Fn() -> Result<Option<Vec<String>>, String>,
    terminal: &mut T,
) -> Result<FreeTextOutcome, StageAbort>
where
    T: FreeTextTerminal,
{
    // **空文本 + overwrite = 清空模式**（2026-10-03 用户需求）：只执行「右移到行
    // 尾 → 退格清空 → 核验占位恢复」，不打新字；占位恢复后该题的自由作答贡献即
    // 被清掉。空文本且非 overwrite → 维持参数中止（防误触）。
    let clearing_only = text.trim().is_empty() && overwrite;
    if text.trim().is_empty() && !clearing_only {
        return Err(StageAbort::screen(
            "INJECT_SCREEN_ABORT: 自由作答文本为空——已中止，未发任何键",
        ));
    }
    let mut sent_keys: Vec<String> = Vec::new();
    // ===== 第 1 段：定位 FreeText 行 =====
    let lines = poll_screen().map_err(StageAbort::screen)?.ok_or_else(|| {
        StageAbort::screen(
            "自由作答：读不到问答屏（屏读窗尽或不可用）——已中止，未发任何键；请人工核对终端",
        )
    })?;
    let rows = parse_question_rows(&lines);
    let free_idx = rows
        .iter()
        .position(|r| r.kind == QuestionRowKind::FreeText)
        .ok_or_else(|| {
            StageAbort::screen(
                "自由作答：屏上解析不出「Type something」行（多选形态不符）——已中止，未发任何键；请人工核对终端",
            )
        })?;
    // **防线**（结构定位的误判面收口）：定位到的行若恰为确认项文案（Review 确认
    // 屏的 Submit answers / Cancel）→ 拒绝动手（不猜纪律）
    let free_label = rows[free_idx].label.to_lowercase();
    if free_label.contains("submit answers") || free_label == "cancel" {
        return Err(StageAbort::screen(
            "自由作答：定位到的行是确认项（Review 确认屏形态）而非自由作答行——已中止，未发任何键；请人工核对终端",
        ));
    }
    // 该行已有自定义文字：overwrite=false 维持中止（防误触）；true = 编辑模式
    //（走位后迭代退格清空再打新字——用户实测需求的覆盖写入）
    let has_existing = !rows[free_idx].label.starts_with(FREE_TEXT_ROW_LABEL);
    if has_existing && !overwrite {
        return Err(StageAbort::screen(
            "自由作答：该行的自由作答已有内容（屏上不再是「Type something」占位）——已中止，未发任何键；请用「编辑」按钮覆盖写入，或到终端修改",
        ));
    }
    let free_num = rows[free_idx].number;
    // ===== 第 2 段：走位到该行（方向感知；每步复核位移恰 1 行）=====
    let mut steps = 0usize;
    loop {
        let cur_lines = terminal.read().ok_or_else(|| {
            StageAbort::screen(format!(
                "自由作答走位段读不到屏幕（已发 {steps} 个方向键）——已中止，未打字"
            ))
        })?;
        let cur_rows = parse_question_rows(&cur_lines);
        let cur = unique_focused_row(&cur_rows).ok_or_else(|| {
            StageAbort::screen(
                "自由作答：屏上解析不到唯一焦点行（❯ 标记缺失或多行）——不猜起点，已中止，未打字",
            )
        })?;
        if cur == free_idx {
            break;
        }
        if steps >= cur_rows.len() + 2 {
            return Err(StageAbort::screen(format!(
                "自由作答走位已发 {steps} 个方向键仍未到「Type something」行（上限 {}）——已中止，未打字；请人工核对终端",
                cur_rows.len() + 2
            )));
        }
        let key = if free_idx > cur { "down" } else { "up" };
        terminal.send(key).map_err(|e| {
            StageAbort::delivery(format!(
                "INJECT_DELIVERY_FAIL: 方向键 {key} 投递失败（{e}）"
            ))
        })?;
        sent_keys.push(key.to_string());
        steps += 1;
        terminal.settle();
        let after_lines = terminal.read().ok_or_else(|| {
            StageAbort::screen("INJECT_SCREEN_ABORT: 自由作答走位后读不到屏幕——已中止，未打字")
        })?;
        let after_rows = parse_question_rows(&after_lines);
        let next_cur = unique_focused_row(&after_rows).ok_or_else(|| {
            StageAbort::screen(
                "INJECT_SCREEN_ABORT: 自由作答：步进后解析不到唯一焦点行——已中止，未打字",
            )
        })?;
        let expected_move: i64 = if key == "down" { 1 } else { -1 };
        if next_cur as i64 - cur as i64 != expected_move {
            return Err(StageAbort::screen(format!(
                "自由作答：按一次 {key} 后焦点位移异常（应沿目标方向恰 1 行）——已中止，未打字"
            )));
        }
    }
    // ===== 第 2.5 段（编辑模式）：「右移到行尾 → 退格清空」迭代 =====
    // 每轮：屏读当前行可见长度 L → 先 right×L 把光标推到行尾（焦点切行后在行首，
    // 退格只删光标前的字符——行首退格全无效，用户实测 bug）→ backspace×L →
    // settle → 重读核验该行回到「Type something」占位；轮次上限 5 防死循环
    //（2000 字级长文本按屏宽折算约 3-5 轮）。
    if has_existing {
        let mut rounds = 0usize;
        loop {
            let cur = terminal.read().ok_or_else(|| {
                StageAbort::screen(
                    "INJECT_SCREEN_ABORT: 自由作答：退格前读不到屏幕——已中止，未清空",
                )
            })?;
            let cur_rows = parse_question_rows(&cur);
            let cur_label_len = cur_rows
                .iter()
                .find(|r| r.number == free_num && r.kind == QuestionRowKind::FreeText)
                .map(|r| r.label.chars().count())
                .ok_or_else(|| {
                    StageAbort::screen(
                        "INJECT_SCREEN_ABORT: 自由作答：退格前解析不到目标行——已中止，未清空",
                    )
                })?;
            if cur_label_len == 0 {
                break; // 已是占位（has_existing 才进这里，防御分支）
            }
            if rounds >= 5 {
                let residual = cur_rows
                    .iter()
                    .find(|r| r.number == free_num)
                    .map(|r| r.label.clone())
                    .unwrap_or_default();
                return Err(StageAbort::screen(format!(
                    "自由作答：退格 {rounds} 轮后行内仍有残留（{residual}）——已中止，未打新字；请到终端人工清空"
                )));
            }
            // **先右移到行尾**（2026-10-03 用户实测 bug：焦点切行后光标在行首，
            // 退格只删光标前的字符——行首退格全无效）；行内 ←/→ 是编辑光标（两行
            // 例外取证），超出末尾的 right 是无操作，多按无害
            for _ in 0..cur_label_len {
                terminal.send("right").map_err(|e| {
                    StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 右移投递失败（{e}）"))
                })?;
                sent_keys.push("right".to_string());
            }
            // 再退格×L（从末尾往回删）
            for _ in 0..cur_label_len {
                terminal.send("backspace").map_err(|e| {
                    StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 退格投递失败（{e}）"))
                })?;
                sent_keys.push("backspace".to_string());
            }
            terminal.settle();
            rounds += 1;
            let after = terminal.read().ok_or_else(|| {
                StageAbort::screen(
                    "INJECT_SCREEN_ABORT: 自由作答：退格后读不到屏幕——已中止，未打新字",
                )
            })?;
            let cleared = parse_question_rows(&after).iter().any(|r| {
                r.number == free_num
                    && r.kind == QuestionRowKind::FreeText
                    && r.label.starts_with(FREE_TEXT_ROW_LABEL)
            });
            if cleared {
                break;
            }
        }
    }
    // ===== 第 3 段：打字（字符通道；直接输入即内联编辑——活体取证）=====
    // 清空模式跳过打字（退格循环已恢复占位；占位行打字核验的 label 翻转规则不适用）
    if !clearing_only {
        terminal.send_text(text).map_err(|e| {
            StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 作答文本投递失败（{e}）"))
        })?;
        sent_keys.push(format!("<text:{} chars>", text.chars().count()));
        terminal.settle();
    }
    // ===== 第 4 段：核验（行翻转 + 勾选在；**不发 enter**——回车会取消勾选）=====
    // 清空模式跳过本段（退格循环的占位恢复核验已足够；翻转/勾选兜底是打字态专属）
    if clearing_only {
        return Ok(FreeTextOutcome {
            sent_keys,
            receipt_seen: None,
            advanced: false,
            review_reached: false,
            screen_text: None,
            screen_checked: None,
        });
    }
    let after = terminal.read().ok_or_else(|| {
        StageAbort::screen(
            "INJECT_SCREEN_ABORT: 自由作答：打字后读不到屏幕——无法核验是否生效；请到终端人工核对",
        )
    })?;
    let after_rows = parse_question_rows(&after);
    // 翻转判据 = label 离开占位（2026-10-02 勾选兜底批：勾选态不参与本判定——
    // 用户实测打字后默认未勾选，勾选由下方兜底段负责）
    let flipped = after_rows
        .iter()
        .find(|r| r.number == free_num)
        .is_some_and(|r| !r.label.starts_with(FREE_TEXT_ROW_LABEL));
    if !flipped {
        return Err(StageAbort::screen(
            "自由作答：打字后屏读未确认「文字已入行」——不补任何键（回车会取消勾选）。请到终端人工核对后重试",
        ));
    }
    // ===== 第 4.5 段：勾选兜底（2026-10-02 用户实测：打字后默认未勾选）=====
    // label 已翻转但 checked == Some(false) → 确认焦点仍在该行后补一个空格 →
    // 重读核验 Some(true)；已勾选 → 跳过；焦点不在该行 → 中止（空格会 toggle 别的
    // 行，不盲发）。
    let after_rows = parse_question_rows(&after);
    let target_pos = after_rows.iter().position(|r| r.number == free_num);
    let target_row = target_pos.and_then(|i| after_rows.get(i)).ok_or_else(|| {
        StageAbort::screen(
            "INJECT_SCREEN_ABORT: 自由作答：核验阶段解析不到目标行——已中止；请到终端人工核对",
        )
    })?;
    if target_row.checked == Some(false) {
        let focus_on_target = unique_focused_row(&after_rows) == target_pos;
        if !focus_on_target {
            return Err(StageAbort::screen(
                "自由作答：文字已入行但未勾选，且焦点已不在该行——不盲发空格（会勾到别的选项）。请到终端核对该行后手动勾选",
            ));
        }
        terminal.send("space").map_err(|e| {
            StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 空格投递失败（{e}）"))
        })?;
        sent_keys.push("space".to_string());
        terminal.settle();
        let recheck = terminal.read().ok_or_else(|| {
            StageAbort::screen("INJECT_SCREEN_ABORT: 自由作答：补勾选后读不到屏幕——无法核验；请到终端人工核对勾选态")
        })?;
        let rechecked = parse_question_rows(&recheck)
            .iter()
            .find(|r| r.number == free_num)
            .is_some_and(|r| r.checked == Some(true));
        if !rechecked {
            return Err(StageAbort::screen(
                "自由作答：补空格后屏读未确认勾选——不补任何键。请到终端人工核对勾选态",
            ));
        }
    }
    // checked == None = **单选形态**（2026-10-03 用户需求：多题流的单选子题也有
    // Type something 行——复用同一编排，无勾选框则勾选兜底自然跳过，label 翻转
    // 核验已证明文字入行）
    // ===== 第 5 段前：采集**屏读真值**（该行最终文本 + 勾选态）回传卡面 =====
    let final_lines = terminal.read().ok_or_else(|| {
        StageAbort::screen("自由作答：收尾读屏失败——按键已生效，请到终端核对最终状态")
    })?;
    let final_row = parse_question_rows(&final_lines)
        .into_iter()
        .find(|r| r.number == free_num);
    let (screen_text, screen_checked) = match final_row {
        Some(r) if !r.label.starts_with(FREE_TEXT_ROW_LABEL) => (
            Some(strip_trailing_selection_mark(r.label.clone())),
            r.checked,
        ),
        _ => (None, None),
    };
    // ===== 第 5 段：零后续键（切题时自然保存；receipt 语义不适用多题流 → None）=====
    Ok(FreeTextOutcome {
        sent_keys,
        receipt_seen: None,
        advanced: false,
        review_reached: false,
        screen_text,
        screen_checked,
    })
}

/// **单选 select 编排**（2026-10-03 用户实测 bug 修复）：claude 单选数字直答
/// 的前置焦点守卫——光标停在自由作答行（内联编辑态）时数字会被当文本吃进输入框
///（活体实证：`❯ 5. 3▊`）。守卫复用 [`prelift_focus_from_freetext`]。
///
/// **语义边界**：数字发出后**不加屏读核验**——单选「数字即答+终端自动推进」的
/// 行为契约在 claude 上未完全取证（计划文档申报维持），本编排只修发前焦点安全，
/// 不改发后行为。数字 = 屏上编号（index 0 起 → 编号 index+1）。
pub fn run_select_stages<T: MenuTerminal>(
    index: usize,
    poll_screen: impl Fn() -> Result<Option<Vec<String>>, String>,
    terminal: &mut T,
) -> Result<Vec<String>, StageAbort>
where
{
    let mut sent_keys: Vec<String> = Vec::new();
    let first = poll_screen().map_err(StageAbort::screen)?.ok_or_else(|| {
        StageAbort::screen("INJECT_SCREEN_ABORT: 读不到问答屏（屏读窗尽或不可用）——已中止，未发任何键；请人工核对终端")
    })?;
    // **Review 确认屏防线（评审 P1-1，2026-10-08）**：漂移场景（用户终端已答完、
    // 屏停在 Review 确认屏，手机卡再点选项）下数字会打进确认屏=替用户提交。
    // Review 是更特异判据（C1 同款前置，见 question_screen_snapshot）——Ready 即
    // 零键中止；顺带关掉多题集自锚 B 落地以来的同类暴露（Review 屏也渲染页签栏，
    // 确认项 1. Submit answers/2. Cancel 会被收进行块）。
    if matches!(probe_review_screen(&first), ScreenStep::Ready(_)) {
        return Err(StageAbort::screen(
            "屏上是 Review 确认屏而非题屏（答案已答毕待确认）——已中止，未发任何键；请到终端人工核对",
        ));
    }
    let rows = parse_question_rows(&first);
    if rows.is_empty() {
        return Err(StageAbort::screen(
            "屏上解析不出问答选项块——已中止，未发任何键；请人工核对终端",
        ));
    }
    let focus_idx = unique_focused_row(&rows);
    let _rows = prelift_focus_from_freetext(terminal, rows, focus_idx, &mut sent_keys, "单选作答")?;
    // 数字 = 屏上编号（index 0 起 → 编号 index+1）
    let digit_key = (index + 1).to_string();
    terminal
        .send(&digit_key)
        .map_err(|e| StageAbort::delivery(format!("INJECT_DELIVERY_FAIL: 数字投递失败（{e}）")))?;
    sent_keys.push(digit_key);
    terminal.settle();
    Ok(sent_keys)
}

/// 自由作答的**工具支持面**：
/// - claude：K4–K7 实机定案（数字定位 → 文本 → 回车）；
/// - kimi（Other 行，戊探B E-B8 全链）/ opencode（own answer 形态）：批次戊实机定案；
/// - codex：**2026-10-10 复活**——0.160.0 曾因「notes 不落卷」停用（2026-10-09
///   取证回填），0.162.1 四取样复验全通（落卷实锤 + 覆盖双 tab 语义 + esc 红线；
///   档案 `research/refs/phase2-消息注入/2026-10-10-codex-0162-notes链复验.md`），
///   编排 [`run_codex_notes_stages`] 按 0.162.1 语义修订后重新接线（单题卡形态门
///   把守——多题 notes 归属未复采，不放开）。
pub fn free_text_supported(tool: &str) -> bool {
    matches!(tool, "claude" | "kimi" | "opencode" | "codex")
}

/// 自由作答的**题目形态支持面**（复评 F6-3：多选卡不提供自由作答）。
///
/// # 为什么多选卡不能提供（实机证据，不是保守猜测）
///
/// 多选屏的自由作答行**不是** [`FREE_TEXT_ROW_LABEL`] 的裸标签，而是带**勾选框**的
/// `4. [ ] Type something`——实机截图 `C-s8-cursor-submit-20260921-015844.png`
/// 的第 4 行逐字如此（本批复评时重新核对过该 PNG：`4. [ ] Type something`，
/// 与单选屏 `C-s7-digit3-result-*.png` 的 `3. Type something.` 形态**不同**）。
///
/// 而 [`locate_free_text_row`] 的判据是「编号行的 label 以 `Type something` 开头」——
/// 该行剥掉编号后的 label 是 `[ ] Type something`，**前缀不匹配** → 定位恒失败 →
/// 自由作答在**多选屏上必然中止在 `free-row` 段**。
///
/// 这不是安全问题（中止即零投递，不会误勾选），但**功能对多选不可用**——所以：
/// - **拒绝而不是放宽判据**（本函数的存在理由）：放宽需要「剥掉行首勾选框再匹配」
///   的判据，而勾选框的**确切形态族**（`[ ]` / `[✓]` / 其它宽度对齐变体）没有取证
///   （本批只有一张多选截图与二进制里的 `[" ","]` 拼接代码；单选屏根本不带勾选框）。
///   在证据不足时收紧能力（拒绝）而不是放宽判据，是本仓一贯的「结论不超证据」；
/// - 待办：若后续实机取证勾选框形态族（`#[ignore]` 占位已登记该观测点），再按证据
///   放宽判定并补夹具。
pub fn free_text_shape_supported(q: &Question) -> bool {
    !q.multi_select
}

/// 自由作答形态的静态描述（见 [`free_text_shape`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreeTextShape {
    /// 定位行标签（屏上判据）
    pub locate_label: &'static str,
    /// 提交键（文本之后**唯一**的键——不带数字不带 Esc）
    pub submit_key: &'static str,
    /// **是否只对单题卡成立**（当前恒 `true`——多选屏的行带勾选框，判据不匹配；
    /// 见 [`free_text_shape_supported`] 的实机证据与放宽前提）
    pub single_question_only: bool,
}

/// 自由作答序列的**纯构造快照**（供端点先做「能不能发」的判定与前端契约）。
///
/// 返回「定位行判据 + 提交键」两项的静态描述——**不是**可直接投递的键序（键序由
/// [`run_free_text_stages`] 在屏读之后产生，定位数字取自屏上编号）。存在理由：
/// 端点与测试要对「这条路的形态」做一次**不依赖屏读**的断言（例如「claude 的形状是
/// 数字→文本→回车」「不支持的工具必须 Err」），故给一个纯函数入口。
pub fn free_text_shape(tool: &str) -> Result<FreeTextShape, String> {
    if !free_text_supported(tool) {
        return Err(format!("{tool} 自由作答序列未实测，请在终端作答"));
    }
    // codex 的形状 = 走位 Other 行（`None of the above`）→ tab 开备注 → 文本 →
    // 回车——定位判据与 claude 系（裸 `Type something` 行）不同族，按工具分派。
    let (locate_label, submit_key) = if tool == "codex" {
        (CODEX_OTHER_ROW_LABEL, "enter")
    } else {
        (FREE_TEXT_ROW_LABEL, "enter")
    };
    Ok(FreeTextShape {
        locate_label,
        submit_key,
        // **题目形态限制**（复评 F6-3）：本形态只对单题卡成立——多选屏的自由作答行
        // 带勾选框（`4. [ ] Type something`），与 locate_label 的前缀判据不符。
        // 消费方必须同时过 [`free_text_shape_supported`]（端点已如此）。
        single_question_only: true,
    })
}

/// 自由作答注入的**族规格**入口——本编排只对 claude 放行，故规格取
/// `families::family_for("claude")`；显式函数便于测试对「规格确实是 claude 的」下断言
/// （而不是散在端点里写魔法字符串）。
pub fn free_text_family_spec() -> FamilySpec {
    crate::inject::families::family_for("claude").unwrap_or(crate::inject::families::FALLBACK_SPEC)
}

/// 动作可用性门的**拒绝原因**（决定端点回哪个错误码——这是「用户要看到什么」的一部分：
/// 「序号不对」（400，改序号即可）与「这个工具还没实测」（409，去终端做）是两回事）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionRefusal {
    /// 参数与题目形态不合（序号越界 / submit 用在单选题）→ 端点 400 bad_index
    BadParameter(String),
    /// 该工具的这个动作**未实测**（未验不出键）→ 端点 409 tool_readonly
    ToolUnverified(String),
}

impl ActionRefusal {
    /// 供日志与测试读的中文原因
    pub fn reason(&self) -> &str {
        match self {
            Self::BadParameter(s) | Self::ToolUnverified(s) => s,
        }
    }
}

/// **动作可用性门**（丁T5）：该工具支持这个动作吗？
///
/// 与 [`answer_key_sequence_for`] 的关系：后者产**静态键序**，而丁T5 起 `Submit` /
/// `FreeText` 的键序依赖屏读（阶段机在运行时产生）——它们**没有**静态序列可返回，
/// 却仍然有「支不支持」这件事要说（claude 支持、其余工具未定案）。故本函数是
/// **可用性判据的单点**：端点用它做门（不支持的 → 409/400），键序只在支持之后再问
/// [`answer_key_sequence_for`]（单键动作）。
///
/// 逐动作：
/// - `Select` / `Toggle` / `Cancel`：委托 [`answer_key_sequence_for`] 的既有键序档
///   （「未验不出键」的边界原样）；
/// - `Submit`：仅 claude（多选三段式的 Review 屏判据在 claude 2.1.251 实机取证；
///   其余工具的多选形态**未实测**——opencode/kimi 的多选框形态都没有实机样本）；
/// - `FreeText`：仅 claude（K4–K7 实机定案；其余工具的自由作答序列未定案）。
///
/// **拒绝原因分两档**（见 [`ActionRefusal`]）：端点据此把 400（改参数即可）与
/// 409（去终端做）分开——批次丙的旧实现把两者都塞进 `bad_index` 是对用户的误导
/// （「序号无效」会让用户以为改个序号就行）。
pub fn action_supported(
    tool: &str,
    action: AnswerAction,
    index: Option<usize>,
    q: &Question,
) -> Result<(), ActionRefusal> {
    // 工具未实测（只读档）优先于动作级判断：该工具任何动作都不可用
    if matches!(question_key_profile(tool), QuestionKeyProfile::ReadOnly) {
        return Err(ActionRefusal::ToolUnverified(format!(
            "Q_READONLY_PROFILE: {tool} 问答键序未实测，只读展示"
        )));
    }
    match action {
        // kimi 多题切题（2026-10-06，2.1.1 活体定案）：`→`/tab 前向、`←` 后向
        // （Review 页 `←` = 返回修改）——走 KimiAdvance 臂（屏读到达验证），门放行
        AnswerAction::Advance if tool == "kimi" => Ok(()),
        // codex 多题切题（2026-10-09 屏读底料定案）：vim h/l 环形切题（h=上一题
        // l=下一题；箭头族对 codex 面板无效——VT 与真 VK 箭头零变化实证）——走
        // CodexAdvance 臂（屏读到达验证），门放行（同 kimi 先例：键在臂内参数化，
        // `answer_key_sequence_for` 的 codex Advance 拒绝臂保持不动作纵深防御）
        AnswerAction::Advance if tool == "codex" => Ok(()),
        // claude 的多题切题（2026-09-24 起）：走阶段机（走位到 Next + 回车 + 分类）。
        // 其余工具维持既有档（opencode=tab 单键；codex 未实测 → 拒）
        AnswerAction::Advance => match question_key_profile(tool) {
            QuestionKeyProfile::ClaudeFull => Ok(()),
            _ => answer_key_sequence_for(tool, action, index, q)
                .map(|_| ())
                .map_err(ActionRefusal::ToolUnverified),
        },
        // 2026-09-24：claude 的多选 Toggle 改走阶段机（run_toggle_stages——数字路径
        // 被用户实机推翻）。其余工具维持静态键序档（opencode/kimi 的 toggle 仍是
        // 单键）。单选上的 Toggle 是参数错（与 submit 的口径一致）
        AnswerAction::Toggle => {
            if !q.multi_select {
                return Err(ActionRefusal::BadParameter(
                    "toggle 仅用于多选题（单选请用 select）".to_string(),
                ));
            }
            match question_key_profile(tool) {
                QuestionKeyProfile::ClaudeFull => Ok(()),
                _ => answer_key_sequence_for(tool, action, index, q)
                    .map(|_| ())
                    .map_err(ActionRefusal::ToolUnverified),
            }
        }
        AnswerAction::Submit => {
            if !q.multi_select {
                return Err(ActionRefusal::BadParameter(
                    "Q_SUBMIT_MULTI_ONLY: submit 仅用于多选题".to_string(),
                ));
            }
            match question_key_profile(tool) {
                // claude：多选三段式（Review 屏判据实机取证）
                QuestionKeyProfile::ClaudeFull => Ok(()),
                // E4 kimi：多选提交走 run_kimi_submit_stages（tab → Review 汇总屏 →
                // 屏上编号确认——戊探B M1/M2 + K-5 实机定案）
                QuestionKeyProfile::TwoPhaseSelect => Ok(()),
                // E6 opencode（2026-09-23 接线补全）：多选提交走
                // run_opencode_submit_stages（tab → Confirm 页 → enter——戊探A ④
                // 三轮全通；阶段机首段屏读「Confirm 已在场则跳过 tab」，与手机端
                // 先切页到 Confirm 的交互兼容）。codex 同档但多选未实测 → 不放行
                QuestionKeyProfile::SingleDigitSubmit if tool == "opencode" => Ok(()),
                _ => Err(ActionRefusal::ToolUnverified(format!(
                    "{tool} 多选提交未实测，不出键"
                ))),
            }
        }
        AnswerAction::FreeText => {
            if !free_text_supported(tool) {
                return Err(ActionRefusal::ToolUnverified(format!(
                    "{tool} 自由作答未实测，不出键"
                )));
            }
            // 题目形态门（复评 F6-3 立门；2026-10-05 推广批按取证状态收口豁免面）：
            // 多选屏的自由作答行带勾选框，**静态** free-row 定位判据（前缀
            // `Type something`）不匹配——但走**阶段机编排**（屏读定位，不依赖静态
            // 判据）的工具，多选形态已实机定案，豁免本门；仍走静态定位的工具维持
            // 拒绝（在端点就拒绝，而不是让用户白等一次必然失败的全链）。
            // 归 ToolUnverified（409 tool_readonly）而不是 BadParameter：用户要做的
            // 是「去终端作答」，不是「改个参数重试」。
            // 豁免面（逐家证据，与 GET 能力位 `multiFreeText` 必须同面——前端据旗标
            // 渲染输入框，本门若收窄用户必撞 409，2026-10-05 实机回归即此）：
            // - claude：多选自 2026-10-02 放行（活体取证：勾选框行直接打字即内联
            //   编辑 + 勾选自动置上 + 回车会取消勾选——ClaudeMultiFreeText 闭环）；
            // - opencode：own answer 方言单/多选皆可（戊探A E-A2/E-A3 双形态实测 +
            //   2026-10-04 用户活体双段 toggle 语义定案——OpencodeOwnAnswer 编排）；
            // - codex：**2026-10-10 复活**（0.160.0「notes 不落卷」在 0.162.1 四取样
            //   复验未复现）——过 `free_text_supported` 首查后：单题**过门**（走
            //   CodexFreeText 活编排），多题/多选落 shape 兜底门拒绝（多题 notes
            //   归属未复采，本豁免名单不开——多选形态走编排屏读的只有上面三家）；
            // - kimi：2026-10-06 用户指令点亮（推翻探测批 K「多选不接入」裁决，
            //   multiFreeText 旗标同批点亮）：走 KimiFreeText 阶段机——Other 行
            //   编号**按屏自适应**（显示编号 `[N]` 优先；多选无编号形态按 checkbox
            //   计数位直达，K4 定案 + 用户实测数字 5 直进编辑态）；多题流保存后
            //   **不代发确认键**（单题 E-B8 的「保存即确认」语义在多题流会把未答
            //   题一并提交——17:00 用户实录事故）。可达性与形态把守都在阶段机
            //   内（屏读定位失败 = 如实中止），门不再一刀切。
            if matches!(tool, "claude" | "opencode" | "kimi") {
                let _ = &q; // 多选形态由编排屏读判定（free-row 解析不出即中止）
            } else if !free_text_shape_supported(q) {
                return Err(ActionRefusal::ToolUnverified(
                    "多选题的自由作答请到终端完成（远程入口仅支持单题卡）".to_string(),
                ));
            }
            Ok(())
        }
        // 单键动作：键序档即可用性。键序档的 Err 里既有「序号越界」（参数问题）也有
        // 「多选未测」（工具问题）——按文案前缀粗分（键序档的文案是稳定的：越界类
        // 都含「越界」/「超键域」/「缺少选项序号」）
        _ => answer_key_sequence_for(tool, action, index, q)
            .map(|_| ())
            .map_err(|e| {
                if e.contains("越界") || e.contains("超键域") || e.contains("缺少选项序号")
                {
                    ActionRefusal::BadParameter(e)
                } else {
                    ActionRefusal::ToolUnverified(e)
                }
            }),
    }
}

/// **多题载荷的提交可用性**（2026-09-23 错位修复）：多题卡的「提交答案」作用于
/// **整张问卷**（kimi 的 Review 汇总屏 / opencode 的 Confirm 页），不作用于
/// `questionIndex` 指定的某一题——[`action_supported`] 里「submit 仅用于多选题」
/// 的防呆（单题卡语义：单选点数字即提交、无独立提交步）对多题载荷不成立（否则
/// 第 1 题是单选的多题问卷会被误拒 400）。按工具档判：kimi/opencode 已实机定案
/// 放行，其余（含 claude 多题整体只读、codex 多选未实测）不出键。
pub fn multi_question_submit_supported(tool: &str) -> Result<(), ActionRefusal> {
    match question_key_profile(tool) {
        // claude（2026-09-24 多题接入）：run_submit_stages——走位到推进行（多题尾部
        // 为 Next）→ 回车 → Review 确认屏 → 屏上编号确认；已在 Review 屏则直接确认
        // （末题 Next 已到确认屏的形态，见该函数第 1.5 段）
        QuestionKeyProfile::ClaudeFull => Ok(()),
        // kimi：run_kimi_submit_stages（戊探B + K-5 定案）
        QuestionKeyProfile::TwoPhaseSelect => Ok(()),
        // opencode：run_opencode_submit_stages（戊探A ④ 三轮全通；2026-09-23 接线）
        QuestionKeyProfile::SingleDigitSubmit if tool == "opencode" => Ok(()),
        _ => Err(ActionRefusal::ToolUnverified(format!(
            "{tool} 多题提交未实测，不出键"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **新增（评审 P2-3）RECEIPT 双词谓词收窄锁**：
    /// - 摘要头 `• Questions 2/2 answered` → **true**（唯一真词形）；
    /// - 题号头 `Question 1/2 (2 unanswered)` → **false（硬断言）**——题号头是
    ///   单数 Question 且无 answered 词，裸词锚语义下这行不中但正文任意
    ///   answered 单词会中；双词组合后题号头/裸单词面归零；
    /// - 正文复述双词齐活（`I have answered your questions`）→ true（**已知残余**，
    ///   注释登记：比裸单词锚收窄一个量级即可，不追求零误报）；
    /// - 裸 answered 单词行（`esc to interrupt answered` 造形）→ false。
    #[test]
    fn codex_receipt_present_narrowed() {
        use live_fixtures as f;
        // 摘要头（活体夹具）→ true
        assert!(
            codex_receipt_present(&f::codex_answered_summary()),
            "摘要头 Questions n/n answered 必须命中"
        );
        assert!(
            codex_receipt_present(&f::codex_answered_summary3()),
            "N=3 摘要头必须命中"
        );
        // 题号头（单数 Question + unanswered）→ false（硬断言）
        assert!(
            !codex_receipt_present(&f::codex_s1_q1()),
            "题号头 Question 1/2 (2 unanswered) 不得命中 RECEIPT"
        );
        assert!(
            !codex_receipt_present(&f::codex_s1_q2()),
            "题号头 Question 2/2 (1 unanswered) 不得命中 RECEIPT"
        );
        // 任意 answered 裸单词（无 questions 同行）→ false
        let bare = lines(&["esc to interrupt answered"]);
        assert!(
            !codex_receipt_present(&bare),
            "裸 answered 单词不得命中（双词组合收窄）"
        );
        // 已知残余：正文复述双词齐活 → true（登记为可接受，比裸词收窄一个量级）
        let recap = lines(&["I have answered your questions"]);
        assert!(
            codex_receipt_present(&recap),
            "正文双词齐活仍命中——已知残余（注释登记），非回归"
        );
    }

    /// **新增（评审 P2-3）账本两行登记 + 谓词与账本词形源一致性**：
    /// candidates 全集（旧行 `answered` + 新行 `questions`）顺序稳定（账本序）；
    /// 旧行保留作向后兼容（append-only），判定不回归到裸词。
    #[test]
    fn codex_receipt_ledger_rows_kept_as_wordform_registry() {
        let rows = crate::inject::anchor_ledger::candidates(
            "codex",
            crate::inject::anchor_ledger::scenario::QUESTION,
            crate::inject::anchor_ledger::slot::RECEIPT,
        );
        assert_eq!(rows.len(), 2, "codex RECEIPT 槽两行（旧行不删 + 新特异行）");
        assert_eq!(rows[0].text, "answered", "旧行原样保留（append-only）");
        assert_eq!(rows[1].text, "questions", "特异行已追加");
        assert_eq!(rows[1].observed_version, "0.160.0");
        // 谓词对摘要头词形的判定与账本特异行词形一致（questions 在场才可能命中）
        assert!(codex_answered_present(
            &live_fixtures::codex_answered_summary()
        ));
        assert!(!codex_answered_present(&live_fixtures::codex_s1_q1()));
    }

    /// 探测档案 §3 单选真实夹具（缩录）——结构往返
    const SINGLE_JSON: &str = r#"{"questions":[{"header":"Next step","multiSelect":false,"options":[{"description":"Explain how AskUserQuestion works and when it's used.","label":"Tool demo"},{"description":"Start a coding or file task in this directory.","label":"Start a task"},{"description":"You have no further request for now.","label":"Nothing yet"}],"question":"This is a demo question — what would you like to do next?"}]}"#;

    /// 探测档案 §3 多选真实夹具（缩录）
    const MULTI_JSON: &str = r#"{"questions":[{"header":"Favorite fruits","multiSelect":true,"options":[{"description":"A sweet, crisp fruit available in many varieties.","label":"Apple"},{"description":"A soft, tropical fruit rich in potassium.","label":"Banana"},{"description":"A juicy summer fruit with a stone pit.","label":"Peach"}],"question":"Which fruits are your favorites? (Select all that apply)"}]}"#;

    #[test]
    fn parse_probe_single_fixture() {
        let qs = parse_questions(SINGLE_JSON).expect("单选夹具必须可解析");
        assert_eq!(qs.len(), 1);
        let q = &qs[0];
        assert_eq!(q.header, "Next step");
        assert_eq!(
            q.question,
            "This is a demo question — what would you like to do next?"
        );
        assert!(!q.multi_select, "multiSelect 缺省/显式 false → 单选");
        assert_eq!(q.options.len(), 3);
        assert_eq!(q.options[0].label, "Tool demo");
        assert_eq!(
            q.options[0].description,
            "Explain how AskUserQuestion works and when it's used."
        );
        assert_eq!(q.options[2].label, "Nothing yet");
    }

    #[test]
    fn parse_probe_multi_fixture() {
        let qs = parse_questions(MULTI_JSON).expect("多选夹具必须可解析");
        assert!(qs[0].multi_select, "multiSelect: true → 多选");
        let labels: Vec<&str> = qs[0].options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, vec!["Apple", "Banana", "Peach"]);
    }

    /// opencode 的多选标志字段名是 **`multiple`**（bool），不是 claude 的
    /// `multiSelect`（戊探A ⑥定案，2026-09-23 用户实机错位复现的根因）——
    /// 只读 multiSelect 会让 opencode 多选题在整条链路恒显单选。回归锁：
    /// multiple: true 必须解析为多选；multiple 缺省仍单选；**数组形态**
    /// （question_renamed 的选项列表）不得误判成标志。
    #[test]
    fn parse_opencode_multiple_flag_field() {
        // opencode 多选题真实形态（戊探A ⑥）：options 数组 + multiple: bool
        let qs = parse_questions(
            r#"{"questions":[{"header":"修改方向","multiple":true,"options":[{"description":"a","label":"以「优化版」为底本"},{"description":"b","label":"以「精简版」为底本"}],"question":"你希望这次按哪些方向修改？（可多选）"}]}"#,
        )
        .expect("opencode 多选夹具必须可解析");
        assert!(
            qs[0].multi_select,
            "opencode 的 multiple: true → 多选（字段名与 claude 不同）"
        );
        // 缺省：两字段都不在 → 单选（宽容口径不变）
        let qs = parse_questions(r#"{"questions":[{"question":"q","options":[{"label":"a"}]}]}"#)
            .unwrap();
        assert!(!qs[0].multi_select);
    }

    /// kimi 的多选标志字段名是 **`multi_select`**（下划线；2026-10-06 用户实机
    /// wire 取证：4 题 AskUserQuestion tool-call args 前两题带
    /// `multi_select: true`、单选题无此字段）。漏认 → 多选题恒显单选 → 前端发
    /// select（数字+enter）替代 toggle（纯数字），enter 在多题流推进下一题 →
    /// 手机端与终端割裂（16:05 用户实录，与 opencode 2026-09-23 事故同构）。
    #[test]
    fn parse_kimi_multi_select_snake_field() {
        // 形态逐字取自 2026-10-06 16:0x 用户 wire（q0 = 第 1 题多选）
        let qs = parse_questions(
            r#"{"questions":[{"header":"优化方向","multi_select":true,"options":[{"description":"落日","label":"视觉光影与色彩（推荐）"},{"description":"海浪","label":"交互动效"}],"question":"这次优化主要想动哪几块？（可多选，1-4 项）"},{"header":"允许","options":[{"description":"少量","label":"允许内联少量原生JS"}],"question":"优化时允许用什么？（单选）"}]}"#,
        )
        .expect("kimi 4 题夹具必须可解析");
        assert!(qs[0].multi_select, "kimi multi_select: true → 多选");
        assert!(!qs[1].multi_select, "kimi 单选题无该字段 → 单选");
    }

    #[test]
    fn parse_rejects_structural_mismatch() {
        // 缺 questions / 非数组 / 空 / 条目缺 question / 缺 options / options 空 /
        // 非 JSON —— 全部 None（端点据此判不可用）
        for bad in [
            r#"{}"#,
            r#"{"questions":"x"}"#,
            r#"{"questions":[]}"#,
            r#"{"questions":[{"options":[{"label":"a"}]}]}"#,
            r#"{"questions":[{"question":"q"}]}"#,
            r#"{"questions":[{"question":"q","options":[]}]}"#,
            r#"{"questions":[{"question":"q","options":[{"description":"no label"}]}]}"#,
            "not json",
        ] {
            assert!(parse_questions(bad).is_none(), "{bad}");
        }
        // 宽容缺省：header / multiSelect / description 缺省
        let qs = parse_questions(
            r#"{"questions":[{"question":"q","options":[{"label":"a"},{"label":"b","description":"d"}]}]}"#,
        )
        .unwrap();
        assert_eq!(qs[0].header, "");
        assert!(!qs[0].multi_select);
        assert_eq!(qs[0].options[0].description, "");
        assert_eq!(qs[0].options[1].description, "d");
    }

    fn single() -> Question {
        parse_questions(SINGLE_JSON).unwrap().remove(0)
    }
    fn multi() -> Question {
        parse_questions(MULTI_JSON).unwrap().remove(0)
    }

    /// 探测 K1：单选点选项 = 单个数字键，**无回车无 Esc**（严禁后补 Esc——K2：
    /// 后补 Esc 中断模型回合）
    #[test]
    fn select_sequence_is_single_digit_no_enter_no_esc() {
        let q = single();
        assert_eq!(
            answer_key_sequence(AnswerAction::Select, Some(1), &q).unwrap(),
            vec!["2"],
            "select#2 → [\"2\"]（任务书定案序列）"
        );
        assert_eq!(
            answer_key_sequence(AnswerAction::Select, Some(0), &q).unwrap(),
            vec!["1"]
        );
    }

    /// **2026-09-24 改写**（原 K8 锁「toggle = 单数字」被用户实机推翻——claude 2.1.278
    /// 多选屏数字无反应，档案 2026-09-24-claude多选多题键序-用户实机取证 §5）：
    /// toggle 不再产出静态数字序列，必须经 [`run_toggle_stages`] 阶段机（空格 + 闭环
    /// 导航 + 屏读校验翻转）。还原动作：把 Toggle 分支改回数字 → 本断言先红。
    #[test]
    fn toggle_sequence_is_no_longer_digit() {
        let q = multi();
        let err = answer_key_sequence(AnswerAction::Toggle, Some(0), &q)
            .expect_err("Toggle 必须拒绝静态数字序列（改走阶段机）");
        assert!(
            err.contains("阶段机") && err.contains("废止"),
            "拒绝原因须指向阶段机与废止口径（维护者可读）：{err}"
        );
        // 分发层同结论（claude 档拒绝；opencode 维持单键 enter 切勾——戊探A 定案不受影响）
        assert!(answer_key_sequence_for("claude", AnswerAction::Toggle, Some(0), &q).is_err());
        assert_eq!(
            answer_key_sequence_for("opencode", AnswerAction::Toggle, Some(0), &q).unwrap(),
            vec!["1".to_string()],
            "opencode toggle 维持数字/enter 切勾档（本批只改 claude）"
        );
    }

    /// action_supported 的 Toggle 门（2026-09-24）：claude 多选 → Ok（阶段机）；
    /// 单选上的 toggle → BadParameter（与 submit 同口径）；opencode 多选 → Ok（单键）。
    #[test]
    fn toggle_action_gate_by_tool_and_shape() {
        assert!(action_supported("claude", AnswerAction::Toggle, Some(0), &multi()).is_ok());
        let err = action_supported("claude", AnswerAction::Toggle, Some(0), &single())
            .expect_err("单选上 toggle 是参数错");
        assert!(
            matches!(err, crate::inject::question::ActionRefusal::BadParameter(_)),
            "单选 toggle → 400 bad_index：{err:?}"
        );
        assert!(action_supported("opencode", AnswerAction::Toggle, Some(0), &multi()).is_ok());
    }

    /// **丁T5 改写**：多选提交不再产「一次算完的盲发序列」——`answer_key_sequence`
    /// 对 Submit 返回 Err（必须走 [`run_submit_stages`] 的阶段机）。
    /// 还原动作：把 Submit 分支改回 `Ok(down×n+1 + enter + '1')` → 本断言先红。
    #[test]
    fn submit_sequence_is_no_longer_blind_fired() {
        let q = multi(); // 3 选项
        let err = answer_key_sequence(AnswerAction::Submit, None, &q)
            .expect_err("Submit 必须拒绝盲发序列（改走阶段机）");
        assert!(
            err.contains("阶段机"),
            "拒绝原因须指向阶段机（维护者可读）：{err}"
        );
        // 分发层同结论（claude 档也走这条 Err——它不再是「claude 有、别家没有」的差别）
        assert!(answer_key_sequence_for("claude", AnswerAction::Submit, None, &q).is_err());
    }

    /// submit 仅多选合法（单选点数字即提交，无独立提交步——防误构造空转序列）
    #[test]
    fn submit_on_single_select_is_rejected() {
        assert!(answer_key_sequence(AnswerAction::Submit, None, &single()).is_err());
    }

    /// 探测 K3：取消 = 单键 esc（模型收 is_error=true 拒绝回执）
    #[test]
    fn cancel_sequence_is_esc() {
        assert_eq!(
            answer_key_sequence(AnswerAction::Cancel, None, &single()).unwrap(),
            vec!["esc"]
        );
    }

    /// 越界/缺序号/超键域防御：select index ≥ 选项数 → Err；≥9 → Err；缺 index → Err
    #[test]
    fn select_index_domain() {
        let q = single(); // 3 选项
        assert!(answer_key_sequence(AnswerAction::Select, Some(3), &q).is_err());
        assert!(answer_key_sequence(AnswerAction::Select, None, &q).is_err());
        let mut many = multi();
        many.options = (0..12)
            .map(|i| QuestionOption {
                label: format!("o{i}"),
                description: String::new(),
            })
            .collect();
        assert!(
            answer_key_sequence(AnswerAction::Select, Some(9), &many).is_err(),
            "9 选项以上超数字键域（探测档案：超范围数字未测，不出手）"
        );
    }

    /// wire ↔ 动作解析 + 审计摘要形态
    #[test]
    fn action_parse_and_audit_label() {
        assert_eq!(AnswerAction::parse("select"), Some(AnswerAction::Select));
        assert_eq!(AnswerAction::parse("toggle"), Some(AnswerAction::Toggle));
        assert_eq!(AnswerAction::parse("submit"), Some(AnswerAction::Submit));
        assert_eq!(AnswerAction::parse("cancel"), Some(AnswerAction::Cancel));
        assert_eq!(AnswerAction::parse("bogus"), None);
        assert_eq!(
            AnswerAction::Select.audit_label(Some(1)),
            "select#2",
            "审计编号对齐 UI 从 1 起"
        );
        assert_eq!(AnswerAction::Submit.audit_label(None), "submit");
        assert_eq!(AnswerAction::Cancel.audit_label(None), "cancel");
    }

    // ---------- T3：形态化匹配 + 跨工具键序分发 ----------

    /// T3 核心：**工具名不参与判据**——codex `request_user_input` 与 opencode
    /// `question` 的 args 形态与 claude 相同，均被 `parse_questions` 接住
    ///（真实形态取自 2026-09-20 跨工具矩阵 §2.1/§2.2 的实机摘录）
    #[test]
    fn parse_accepts_codex_and_opencode_shapes() {
        // codex：function_call.arguments 字符串（rollout JSONL 实录形态）
        let codex = r#"{"questions": [{"header": "历史档案", "id": "history_docs", "options": [{"description": "保留原样", "label": "保留原样 (Recommended)"}], "question": "历史文档如何处理？"}]}"#;
        let qs = parse_questions(codex).expect("codex request_user_input 形态必须可解析");
        assert_eq!(qs[0].header, "历史档案");
        assert_eq!(qs[0].options[0].label, "保留原样 (Recommended)");
        assert!(!qs[0].multi_select, "codex 无 multiSelect 字段 → 缺省单选");

        // opencode：state.input（SQLite part.data 实录形态）
        let oc = r#"{"questions": [{"header": "Build output folder", "options": [{"description": "Place build output in the dist folder", "label": "dist"}, {"description": "Place build output in the out folder", "label": "out"}], "question": "Which folder should hold build output?"}]}"#;
        let qs = parse_questions(oc).expect("opencode question 形态必须可解析");
        assert_eq!(qs[0].options.len(), 2);
        assert_eq!(qs[0].options[1].label, "out");

        // kimi：interaction.request.request.questions 同形（含 multiSelect:false）
        let kimi = r#"{"questions":[{"question":"Which output folder should the build use?","header":"Output dir","options":[{"label":"dist","description":"d"},{"label":"out","description":"o"}],"multiSelect":false}]}"#;
        assert!(parse_questions(kimi).is_some(), "kimi 形态必须可解析");
    }

    /// T3 键序档分发：工具 → 档（未知工具保守只读）
    #[test]
    fn key_profile_routing() {
        assert_eq!(
            question_key_profile("claude"),
            QuestionKeyProfile::ClaudeFull
        );
        assert_eq!(
            question_key_profile("opencode"),
            QuestionKeyProfile::SingleDigitSubmit
        );
        assert_eq!(
            question_key_profile("kimi"),
            QuestionKeyProfile::TwoPhaseSelect
        );
        // T3 实机补测升格：codex 由只读 → SingleDigitSubmit（数字即选即交三取样）
        assert_eq!(
            question_key_profile("codex"),
            QuestionKeyProfile::SingleDigitSubmit
        );
        for unknown in ["zcode", "dsh", "workbuddy", "openclaw", ""] {
            assert_eq!(
                question_key_profile(unknown),
                QuestionKeyProfile::ReadOnly,
                "{unknown} 未实测 → 只读（未验不出键）"
            );
        }
    }

    /// T3：claude 档与原序列逐字节一致（分发不改变既有行为——回归锁）
    #[test]
    fn claude_profile_matches_legacy_sequences() {
        let q = single();
        assert_eq!(
            answer_key_sequence_for("claude", AnswerAction::Select, Some(1), &q).unwrap(),
            answer_key_sequence(AnswerAction::Select, Some(1), &q).unwrap()
        );
        assert_eq!(
            answer_key_sequence_for("claude", AnswerAction::Cancel, None, &q).unwrap(),
            vec!["esc"]
        );
        let m = multi();
        assert!(
            answer_key_sequence_for("claude", AnswerAction::Submit, None, &m).is_err(),
            "submit 两侧同拒（丁T5：阶段机接管）"
        );
    }

    /// T3 · opencode 档（矩阵 §2.2 实机：VK 数字单键即选即交）——select 已验；
    /// 多选/提交/取消未测 → 全部拒绝（未验不出键）
    #[test]
    fn opencode_profile_select_only() {
        let q = single();
        assert_eq!(
            answer_key_sequence_for("opencode", AnswerAction::Select, Some(1), &q).unwrap(),
            vec!["2"],
            "数字单键即选即交（矩阵实测）"
        );
        assert!(
            answer_key_sequence_for("opencode", AnswerAction::Select, None, &q).is_err(),
            "缺序号 → 拒"
        );
        // 批次戊 E6 更新：多选 toggle = 单次数字（空格不入序）；esc = 单次 dismiss；
        // submit 静态序列仍拒（走 run_opencode_submit_stages 阶段机）
        assert_eq!(
            answer_key_sequence_for("opencode", AnswerAction::Toggle, Some(0), &multi()).unwrap(),
            vec!["1"],
            "E6：多选切勾=单次数字（enter/数字 toggle，空格无效）"
        );
        assert_eq!(
            answer_key_sequence_for("opencode", AnswerAction::Cancel, None, &q).unwrap(),
            vec!["esc"],
            "E6：esc=单次 dismiss（state.error 落账，非中断回合）"
        );
        assert!(answer_key_sequence_for("opencode", AnswerAction::Submit, None, &multi()).is_err());
    }

    /// T3 · kimi 档（**2026-09-21 实机复验修正**：数字选中 → **Enter 确认**）
    ///
    /// 矩阵原记载「数字 → Review 屏 → 数字 '1' 提交」被实机证伪：第二个数字无效，
    /// 确认键是 Enter（底栏 `1/2/3 choose · ↵ confirm`）。
    #[test]
    fn kimi_profile_is_two_phase() {
        let q = single();
        assert_eq!(
            answer_key_sequence_for("kimi", AnswerAction::Select, Some(0), &q).unwrap(),
            vec!["1", "enter"],
            "两段式：数字选中 → Enter 确认（实机复验修正，原记为 [数字, '1']）"
        );
        assert_eq!(
            answer_key_sequence_for("kimi", AnswerAction::Select, Some(2), &q).unwrap(),
            vec!["3", "enter"]
        );
        // 越界/缺序号仍拒
        assert!(answer_key_sequence_for("kimi", AnswerAction::Select, Some(9), &q).is_err());
        assert!(answer_key_sequence_for("kimi", AnswerAction::Select, None, &q).is_err());
        // E4：多选切勾 = 单次数字（N7 修复）；取消仍未验拒绝
        assert_eq!(
            answer_key_sequence_for("kimi", AnswerAction::Toggle, Some(0), &multi()).unwrap(),
            vec!["1"],
            "N7 锁：切勾序列必须不含尾随回车（[数字,回车]=双切抵消净零）"
        );
        assert!(answer_key_sequence_for("kimi", AnswerAction::Cancel, None, &q).is_err());
    }

    /// T3 · codex 档（2026-09-21 实机补测后）→ 单选 select 数字即选即交；
    /// 多选（toggle/submit）仍拒；cancel = esc 中断回合（2026-10-18 裁决放开，#80）
    #[test]
    fn codex_profile_digit_select_only() {
        let q = single();
        // 实机三取样：数字单键即选即交
        assert_eq!(
            answer_key_sequence_for("codex", AnswerAction::Select, Some(1), &q).unwrap(),
            vec!["2"],
            "数字单键即选即交（实机补测三取样）"
        );
        assert_eq!(
            answer_key_sequence_for("codex", AnswerAction::Select, Some(0), &q).unwrap(),
            vec!["1"]
        );
        // 越界/缺序号拒
        assert!(answer_key_sequence_for("codex", AnswerAction::Select, Some(9), &q).is_err());
        assert!(answer_key_sequence_for("codex", AnswerAction::Select, None, &q).is_err());
        // 多选切勾：2026-10-05 探测批 C 接线（↓×index + Space，C2 定案）
        assert_eq!(
            answer_key_sequence_for("codex", AnswerAction::Toggle, Some(0), &multi()).unwrap(),
            vec!["space"]
        );
        assert!(answer_key_sequence_for("codex", AnswerAction::Submit, None, &multi()).is_err());
        // cancel = esc（2026-10-18 裁决放开，#80）：Esc 在 codex 是**中断整个回合**
        // （实录 "aborted by user…" + turn_aborted），非 claude 的 is_error 拒答——
        // 破坏性语义以前端按钮文案警示（mobile.question.cancelInterrupt），不再门拒
        assert_eq!(
            answer_key_sequence_for("codex", AnswerAction::Cancel, None, &q).unwrap(),
            vec!["esc"],
            "#80 放开：codex cancel = esc 中断回合（原 Q_UNVERIFIED_CODEX 门拒废止）"
        );
        // 门级回归锁：`action_supported` 的 Cancel 走兜底臂直查键序档——防将来
        // 门被单独收紧（不经过键序层）时 codex cancel 再被拦（409 tool_readonly）
        assert!(
            action_supported("codex", AnswerAction::Cancel, None, &q).is_ok(),
            "门与键序层必须同面：键序放行而门拒 = 用户必撞 409"
        );
    }

    /// 丁T2：kimi 档的**证据边界**回归锁——证明 TwoPhaseSelect 只对单选放行、
    /// 且不因 plan_review 的证伪而漂移（改它必须由实机探测定案，见
    /// [`question_key_profile`] 的丁T2 注释）。
    #[test]
    fn kimi_question_profile_stays_two_phase_pending_probe() {
        let q = single();
        assert_eq!(
            question_key_profile("kimi"),
            QuestionKeyProfile::TwoPhaseSelect,
            "kimi question 类数字可靠性未经独立实测——不超证据改行为（R1 证伪的是 \
             plan_review 审批框，属另一对话框族）"
        );
        // 两段式序列形态不变（数字选中 → Enter 确认）
        assert_eq!(
            answer_key_sequence_for("kimi", AnswerAction::Select, Some(0), &q).unwrap(),
            vec!["1", "enter"]
        );
        // E4 更新：多选切勾=单次数字（戊探B ×3 定案，N7 修复）；submit 的**静态序列**
        // 仍拒（走 run_kimi_submit_stages 阶段机）；取消仍未验拒绝
        assert_eq!(
            answer_key_sequence_for("kimi", AnswerAction::Toggle, Some(0), &multi()).unwrap(),
            vec!["1"],
            "N7 锁：切勾禁尾随回车"
        );
        assert!(answer_key_sequence_for("kimi", AnswerAction::Submit, None, &multi()).is_err());
        assert!(answer_key_sequence_for("kimi", AnswerAction::Cancel, None, &q).is_err());
    }

    // ============================================================
    // 丁T5：多选提交阶段机（§2.3 裁4）——脚本化屏幕序列驱动
    // ============================================================

    // ---- 真机屏幕夹具（2026-09-21 探测档案原文，逐字抄录）----

    // ==== 批次戊 E6：opencode 多题/多选（阶段机脚本锁 + 对账解析）====

    /// **opencode Confirm/own answer 锚 × 真机夹具**（戊探A E-A1 原件）：
    /// 勾选态题页不是 Confirm 页；Confirm 页 footer 锚命中；占位行守卫判据
    /// （带编号=选项行，无编号=已开输入行）。
    #[test]
    fn e6_opencode_anchors_on_real_fixtures() {
        let checked = e_stage2_screen("opencode-checked.txt");
        assert!(!opencode_confirm_present(&checked), "题页不是 Confirm 页");
        assert!(
            !opencode_own_answer_input_open(&checked),
            "带编号的 `5. [ ] Type your own answer` 是选项行，不是已开输入行"
        );
        let confirm = e_stage2_screen("opencode-confirm-page.txt");
        assert!(
            opencode_confirm_present(&confirm),
            "Confirm 页 footer 锚命中"
        );
        // own answer 行状态：勾选态夹具 = `5. [ ] Type your own answer`
        // → (行序 5, 未勾选, 无保存内容)
        assert_eq!(
            opencode_own_answer_row_state(
                &checked,
                &crate::inject::dialect::own_answer_dialect("opencode").unwrap()
            ),
            Some((5, false, String::new()))
        );
    }

    /// **opencode 提交阶段机脚本锁**（2026-09-23 更新：首段「Confirm 已在场跳过
    /// tab」——手机端「切换题目」已把终端切到 Confirm 页时直接 enter，不再发 tab
    /// 把提交屏回绕切走）三个场景：题页起 → tab → Confirm → enter；**Confirm 已
    /// 在场 → 直接 enter**；tab 后 Confirm 缺席 → 中止不发提交键。终态锚
    /// （`# Questions`）在场 → Some(true)。
    #[test]
    fn e6_opencode_submit_stage_scripted() {
        let dialog = e_stage2_screen("opencode-checked.txt");
        let confirm = e_stage2_screen("opencode-confirm-page.txt");
        let receipt = lines(&["# Questions", "bravo, charlie, delta"]);
        // 场景 1：题页起（Confirm 不在场）→ tab → enter
        let mut sent: Vec<String> = Vec::new();
        let mut terminal = crate::inject::mode::Closures {
            read: || Some(dialog.clone()),
            send: |k: &str| {
                sent.push(k.to_string());
                Ok(())
            },
            settle: || {},
        };
        let out = run_opencode_submit_stages(
            || Some(dialog.clone()),
            || Ok(Some(confirm.clone())),
            || Ok(Some(receipt.clone())),
            &mut terminal,
        )
        .expect("tab → Confirm → enter");
        assert_eq!(out.sent_keys, vec!["tab", "enter"]);
        assert_eq!(out.receipt_seen, Some(true));
        // 场景 2（跳 tab 防御）：Confirm 已在场（手机端已切页）→ 直接 enter
        let mut sent2: Vec<String> = Vec::new();
        let mut terminal2 = crate::inject::mode::Closures {
            read: || Some(confirm.clone()),
            send: |k: &str| {
                sent2.push(k.to_string());
                Ok(())
            },
            settle: || {},
        };
        let out2 = run_opencode_submit_stages(
            || Some(confirm.clone()),
            || Ok(Some(confirm.clone())),
            || Ok(Some(receipt.clone())),
            &mut terminal2,
        )
        .expect("Confirm 已在场 → 直接 enter");
        assert_eq!(
            out2.sent_keys,
            vec!["enter"],
            "跳过 tab——Confirm 页上 tab 会把提交屏回绕切走（戊探A ①）"
        );
        assert_eq!(out2.receipt_seen, Some(true));
        // 场景 3：题页起但 tab 后 Confirm 不出现 → 中止且只发了 tab
        let mut sent3: Vec<String> = Vec::new();
        let mut terminal3 = crate::inject::mode::Closures {
            read: || Some(dialog.clone()),
            send: |k: &str| {
                sent3.push(k.to_string());
                Ok(())
            },
            settle: || {},
        };
        let err = run_opencode_submit_stages(
            || Some(dialog.clone()),
            || Ok(None),
            || Ok(None),
            &mut terminal3,
        )
        .expect_err("Confirm 不出现 → 中止");
        assert_eq!(sent3, vec!["tab"], "只发了 tab，提交键未发");
        assert!(err.message.contains("Confirm 页"), "{err:?}");
    }

    /// **2026-09-23 错位修复回归锁**：Advance（多题切页）仅 opencode 放行
    /// （tab 单键——戊探A ①定案）；claude/kimi/codex 拒绝（kimi/codex 的多题切页键
    /// 无实机样本、claude 多题整体只读——「未验不出键」）。
    #[test]
    fn advance_keys_opencode_only() {
        let q = multi();
        assert_eq!(
            answer_key_sequence_for("opencode", AnswerAction::Advance, None, &q).unwrap(),
            vec!["tab"],
            "opencode 多题切页 = tab 前向切页（戊探A ①）"
        );
        assert!(answer_key_sequence_for("kimi", AnswerAction::Advance, None, &q).is_err());
        assert!(answer_key_sequence_for("codex", AnswerAction::Advance, None, &q).is_err());
        assert!(answer_key_sequence_for("claude", AnswerAction::Advance, None, &q).is_err());
    }

    /// **2026-09-23 接线回归锁 + 2026-09-24 claude 接入改写 + 2026-10-09 codex 接入改写**：
    /// 可用性门——opencode 与 claude 的 advance 放行（各自阶段机）；codex advance
    /// 2026-10-09 起放行（h/l 环形切题底料定案——走 CodexAdvance 阶段机，键在臂内
    /// 参数化）；多选 submit 不放行；单选题 submit 防呆保持；多题 submit 专用门不看
    /// 题目形态（第 1 题是单选的问卷也放行 kimi/opencode/claude）。
    #[test]
    fn action_gates_for_opencode_multi_question_line() {
        let m = multi();
        let s = single();
        // advance：opencode 放行（tab 纯导航）；claude 放行（2026-09-24 起走
        // run_advance_stages 阶段机）；codex 放行（2026-10-09 h/l 底料定案——走
        // CodexAdvance 阶段机，同 kimi 先例的门臂分工）
        assert!(action_supported("opencode", AnswerAction::Advance, None, &m).is_ok());
        assert!(action_supported("claude", AnswerAction::Advance, None, &m).is_ok());
        assert!(action_supported("codex", AnswerAction::Advance, None, &m).is_ok());
        // 静态序列层：claude/codex 的 advance 仍 Err（必须经阶段机——盲发防线；
        // codex 的内核拒绝臂保持不动是纵深防御，不经端点分派的调用方仍被拒绝）
        assert!(answer_key_sequence_for("claude", AnswerAction::Advance, None, &m).is_err());
        assert!(answer_key_sequence_for("codex", AnswerAction::Advance, None, &m).is_err());
        // opencode 多选 submit：接线后放行（走 run_opencode_submit_stages）
        assert!(action_supported("opencode", AnswerAction::Submit, None, &m).is_ok());
        // codex 多选 submit 仍拒（多选未实测）
        assert!(action_supported("codex", AnswerAction::Submit, None, &m).is_err());
        // 单选题 submit 防呆保持（单选点数字即提交，无独立提交步）
        assert!(action_supported("opencode", AnswerAction::Submit, None, &s).is_err());
        // 多题 submit 专用门：kimi/opencode/claude 放行（claude 2026-09-24 接入——
        // 走位到尾部推进行 Next + 回车 + Review 确认，末题已到 Review 屏则直接确认）
        assert!(multi_question_submit_supported("opencode").is_ok());
        assert!(multi_question_submit_supported("kimi").is_ok());
        assert!(multi_question_submit_supported("claude").is_ok());
        assert!(multi_question_submit_supported("codex").is_err());
    }

    /// **2026-10-05 实机回归锁**：多选自由作答的形态门豁免面必须与 GET 能力位
    /// `multiFreeText` 同面——opencode（own answer 方言）多选放行；kimi（2026-10-06
    /// 点亮）多选放行。此前豁免面漏了 opencode/codex：GET 渲染输入框、POST 必撞
    /// 409 tool_readonly（用户实机撞线：输入框打字点发送 →「该工具的远程作答尚未
    /// 实测」）。**codex 面沿革**：2026-10-09 取证回填曾整体停用（0.160.0 notes
    /// 不落卷，底料 §5）→ **2026-10-10 notes 链复活**（0.162.1 四取样复验全通）：
    /// 单题卡过门走活编排，多选面维持 409（回执文案引导终端作答，不是静默失败）。
    /// **codex 多选切勾接线锁**（2026-10-05 探测批 C 定案 C2 接线）：
    /// 多选 Toggle = ↓×(index) 前向走位 + Space（选中不前进，作用于高亮行）；
    /// index 越界拒绝；单选不经过此路径（toggle 仅多选题）。
    #[test]
    fn codex_multi_toggle_downs_plus_space() {
        let m = multi();
        let seq = answer_key_sequence_for("codex", AnswerAction::Toggle, Some(1), &m)
            .expect("codex 多选 toggle 接线放行");
        assert_eq!(seq, vec!["down", "space"], "↓×1 到第 2 项 + Space 选中");
        let seq0 = answer_key_sequence_for("codex", AnswerAction::Toggle, Some(0), &m)
            .expect("index 0 = 零走位直接 Space");
        assert_eq!(seq0, vec!["space"]);
        // 越界拒绝
        assert!(answer_key_sequence_for("codex", AnswerAction::Toggle, Some(9), &m).is_err());
        // 可用性门放行（此前 ToolUnverified 拒绝）
        assert!(action_supported("codex", AnswerAction::Toggle, Some(0), &m).is_ok());
    }

    #[test]
    fn action_gate_multi_free_text_matches_multifreetext_capability() {
        let m = multi();
        assert!(action_supported("claude", AnswerAction::FreeText, None, &m).is_ok());
        assert!(action_supported("opencode", AnswerAction::FreeText, None, &m).is_ok());
        // codex 2026-10-10 复活：free_text_supported 已放开 → **多选卡仍拒**
        // （走 shape 兜底分支——多题/多选 notes 归属未在 0.162.1 复采，文案引导
        // 终端完成；路由侧走 CodexFreeText 活编排仅接单题卡）
        let refusal = action_supported("codex", AnswerAction::FreeText, None, &m)
            .expect_err("codex 多选自由作答仍须门拒");
        assert_eq!(
            refusal,
            ActionRefusal::ToolUnverified(
                "多选题的自由作答请到终端完成（远程入口仅支持单题卡）".to_string()
            ),
            "多选拒绝走 shape 兜底分支，文案引导终端：{refusal:?}"
        );
        // GET multiFreeText 旗标**同面**是本测试的存在意义：旗标开而门拒 = 用户
        // 必撞 409（2026-10-05 实机回归即此形态）
        assert!(
            action_supported("kimi", AnswerAction::FreeText, None, &m).is_ok(),
            "kimi 多选 freeText 与 multiFreeText 旗标同面放行（可达性由阶段机屏读把守）"
        );
        // 单选面（codex notes 链复活 → 单题卡**过门**；其余各家既有定案维持放行）
        let s = single();
        assert!(action_supported("kimi", AnswerAction::FreeText, None, &s).is_ok());
        assert!(
            action_supported("codex", AnswerAction::FreeText, None, &s).is_ok(),
            "codex 单题自由作答过门（notes 链复活，走 CodexFreeText 活编排）"
        );
        assert!(action_supported("opencode", AnswerAction::FreeText, None, &s).is_ok());
        assert!(action_supported("claude", AnswerAction::FreeText, None, &s).is_ok());
    }

    /// **opencode own answer 阶段机脚本锁（裸打字守卫）**：
    /// 鲜态（高亮=首行）→ ↓×4 → enter 开行 → 占位行确认 → 文本 → enter；
    /// 守卫失败（开行后无占位行）→ 中止**不发文本**。
    #[test]
    fn e6_opencode_own_answer_stage_scripted() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Mutex;
        let fresh = vec![
            "1. [ ] alpha".to_string(),
            "2. [ ] bravo".to_string(),
            "3. [ ] charlie".to_string(),
            "4. [ ] delta".to_string(),
            "5. [ ] Type your own answer".to_string(),
            "⇆ tab  ↑ select  enter toggle  esc dismiss".to_string(),
        ];
        let opened = vec![
            "5. [ ] Type your own answer".to_string(),
            "   Type your own answer".to_string(),
        ];
        let receipt = vec!["# Questions".to_string()];

        // 鲜态：↓×4 → enter 开行（屏换占位行态）→ 文本 → enter
        let downs = std::sync::Arc::new(AtomicUsize::new(0));
        let texts: std::sync::Arc<Mutex<Vec<String>>> = std::sync::Arc::new(Mutex::new(vec![]));
        let sent = std::sync::Arc::new(Mutex::new(Vec::new()));
        let (dr, ds) = (downs.clone(), downs.clone());
        let (fr, fs) = (fresh.clone(), sent.clone());
        let (or_, tx) = (opened.clone(), texts.clone());
        let mut terminal = crate::inject::question::FreeTextClosures {
            read: move || {
                let d = dr.load(Ordering::SeqCst);
                if d >= 4 {
                    Some(or_.clone())
                } else {
                    Some(fr.clone())
                }
            },
            send: move |k: &str| {
                if k == "down" {
                    ds.fetch_add(1, Ordering::SeqCst);
                }
                fs.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(k.to_string());
                Ok(())
            },
            send_text: move |t: &str| {
                tx.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(t.to_string());
                Ok(())
            },
            settle: || {},
        };
        let out = run_opencode_own_answer_stages(
            "custom purple",
            false,
            || Ok(Some(receipt.clone())),
            &mut terminal,
        )
        .expect("鲜态全链走通");
        assert_eq!(
            out.sent_keys,
            vec!["down", "down", "down", "down", "enter", "<text>", "enter"]
        );
        assert_eq!(
            texts.lock().unwrap_or_else(|e| e.into_inner()).as_slice(),
            ["custom purple"]
        );
        assert_eq!(out.receipt_seen, Some(true));

        // 守卫失败：开行后屏上仍无占位行 → 中止且未发文本
        let sent2 = std::sync::Arc::new(Mutex::new(Vec::new()));
        let texts2: std::sync::Arc<Mutex<Vec<String>>> = std::sync::Arc::new(Mutex::new(vec![]));
        let fr2 = fresh.clone();
        let (s2r, t2r) = (sent2.clone(), texts2.clone());
        let mut terminal2 = crate::inject::question::FreeTextClosures {
            read: move || Some(fr2.clone()),
            send: move |k: &str| {
                s2r.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(k.to_string());
                Ok(())
            },
            send_text: move |t: &str| {
                t2r.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(t.to_string());
                Ok(())
            },
            settle: || {},
        };
        let err = run_opencode_own_answer_stages("x", false, || Ok(None), &mut terminal2)
            .expect_err("占位行未出现 → 裸打字守卫中止");
        assert!(err.message.contains("裸打字守卫"), "{err:?}");
        assert!(
            texts2.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "文本未发出"
        );
    }

    /// **2.0.22 形态脚本锁（2026-10-05 实机回归根因）**：opencode 2.x 编辑态**无占位行**
    /// ——enter 后 own 行勾选上（`[✓]`）+ footer 变 `enter done`（自建会话活体取证）。
    /// 旧守卫只认占位行 → 假阴性中止（「输入行未开启」）且把 own 行留在已勾残局。
    /// 新守卫三信号（占位行/footer/勾选翻转）须放行本夹具全链；走位落错（enter 落在
    /// 选项行，屏面无任何开启信号）仍须中止且零文本。
    #[test]
    fn opencode_2x_edit_open_without_placeholder_row() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Mutex;
        // 2.0.22 真机形态（2026-10-05 自建会话 dump 逐字）：多题页签 + 勾选框 +
        // footer `enter toggle`；own 行在第 4 编号位。
        let fresh = vec![
            "Questions".to_string(),
            "Colors  Fruits  Submit".to_string(),
            "Which colors do you like? (Select all that apply)".to_string(),
            "1. [ ] Red".to_string(),
            "2. [ ] Green".to_string(),
            "3. [ ] Blue".to_string(),
            "4. [ ] Type your own answer".to_string(),
            "⇆ tab  ↑ select  enter toggle  esc dismiss".to_string(),
        ];
        // enter 后（2.x）：own 行 [✓]、footer `enter done`——**没有占位行**
        let opened2x = vec![
            "Questions".to_string(),
            "Colors  Fruits  Submit".to_string(),
            "Which colors do you like? (Select all that apply)".to_string(),
            "1. [ ] Red".to_string(),
            "2. [ ] Green".to_string(),
            "3. [ ] Blue".to_string(),
            "4. [✓] Type your own answer".to_string(),
            "⇆ tab  enter done  esc close".to_string(),
        ];
        // 保存后：行内容 = 已存文本
        let saved = vec![
            "Questions".to_string(),
            "Which colors do you like? (Select all that apply)".to_string(),
            "1. [ ] Red".to_string(),
            "2. [ ] Green".to_string(),
            "3. [ ] Blue".to_string(),
            "4. [✓] probe text".to_string(),
            "⇆ tab  ↑ select  enter toggle  esc dismiss".to_string(),
        ];
        let enters = std::sync::Arc::new(AtomicUsize::new(0));
        let downs = std::sync::Arc::new(AtomicUsize::new(0));
        let texts: std::sync::Arc<Mutex<Vec<String>>> = std::sync::Arc::new(Mutex::new(vec![]));
        let (er_d, er_r) = (enters.clone(), enters.clone());
        let (dn_d, dn_r) = (downs.clone(), downs.clone());
        let tx = texts.clone();
        let (f1, o1, s1) = (fresh.clone(), opened2x.clone(), saved.clone());
        let mut terminal = crate::inject::question::FreeTextClosures {
            read: move || {
                let e = er_d.load(Ordering::SeqCst);
                let d = dn_d.load(Ordering::SeqCst);
                if e >= 2 {
                    Some(s1.clone())
                } else if e >= 1 && d >= 3 {
                    Some(o1.clone())
                } else {
                    Some(f1.clone())
                }
            },
            send: move |k: &str| {
                if k == "enter" {
                    er_r.fetch_add(1, Ordering::SeqCst);
                }
                if k == "down" {
                    dn_r.fetch_add(1, Ordering::SeqCst);
                }
                Ok(())
            },
            send_text: move |t: &str| {
                tx.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(t.to_string());
                Ok(())
            },
            settle: || {},
        };
        let out = run_opencode_own_answer_stages("probe text", false, || Ok(None), &mut terminal)
            .expect("2.x 无占位行形态须走通（footer 信号放行）");
        assert_eq!(
            out.sent_keys,
            vec!["down", "down", "down", "enter", "<text>", "enter"]
        );
        assert_eq!(out.screen_text.as_deref(), Some("probe text"));
        assert_eq!(out.screen_checked, Some(true));
        assert_eq!(
            texts.lock().unwrap_or_else(|e| e.into_inner()).as_slice(),
            ["probe text"]
        );

        // 负例：走位落错（enter 落在选项行——屏面无 footer 信号、own 行未勾选、
        // 无占位行）→ 守卫中止且零文本（这正是「答错行」的最后防线）
        let enters2 = std::sync::Arc::new(AtomicUsize::new(0));
        let (e2d, e2r) = (enters2.clone(), enters2.clone());
        let texts2: std::sync::Arc<Mutex<Vec<String>>> = std::sync::Arc::new(Mutex::new(vec![]));
        let t2x = texts2.clone();
        let f2 = fresh.clone();
        let mut terminal2 = crate::inject::question::FreeTextClosures {
            read: move || {
                // enter 后屏面无任何变化（enter 被选项行吞掉——走位落错形态）
                let _ = e2d.load(Ordering::SeqCst);
                Some(f2.clone())
            },
            send: move |k: &str| {
                if k == "enter" {
                    e2r.fetch_add(1, Ordering::SeqCst);
                }
                Ok(())
            },
            send_text: move |t: &str| {
                t2x.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(t.to_string());
                Ok(())
            },
            settle: || {},
        };
        let err = run_opencode_own_answer_stages("x", false, || Ok(None), &mut terminal2)
            .expect_err("无开启信号 → 守卫中止");
        assert!(err.message.contains("裸打字守卫"), "{err:?}");
        assert!(texts2.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    }

    /// **footer 闭环寻位脚本锁（2026-10-05 实机回归反转）**：高亮**不在首行**（用户
    /// 手动/卡片操作后停在选项行——2.0.22 footer=`enter toggle`）→ 盲走 ↓×(pos-1)
    /// 必错行；新走位每步 ↓ 前读 footer，见 `enter edit`（高亮已到 own 行）**提前
    /// 停手**。本夹具：own 行在第 6 编号位、高亮起始在第 5 选项行 → 旧盲走 ↓×5
    /// （回绕错行），新闭环 ↓×1 即达。1.x 兼容：footer 恒 `enter toggle` 时走满
    /// 旧步数（e6 既有脚本锁覆盖）。
    #[test]
    fn opencode_2x_footer_guided_seek_stops_early() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Mutex;
        let highlight_opt5 = vec![
            "1. [ ] Red".to_string(),
            "2. [ ] Green".to_string(),
            "3. [ ] Blue".to_string(),
            "4. [ ] Type your own answer".to_string(),
            "5. [ ] Echo".to_string(),
            "6. [ ] Type your own answer".to_string(),
            "⇆ tab  ↑ select  enter toggle  esc dismiss".to_string(),
        ];
        let highlight_own = vec![
            "1. [ ] Red".to_string(),
            "2. [ ] Green".to_string(),
            "3. [ ] Blue".to_string(),
            "4. [ ] Type your own answer".to_string(),
            "5. [ ] Echo".to_string(),
            "6. [ ] Type your own answer".to_string(),
            "⇆ tab  ↑ select  enter edit  esc dismiss".to_string(),
        ];
        let opened2x = vec![
            "6. [✓] Type your own answer".to_string(),
            "⇆ tab  enter done  esc close".to_string(),
        ];
        let saved = vec![
            "1. [ ] Red".to_string(),
            "6. [✓] seek text".to_string(),
            "⇆ tab  ↑ select  enter toggle  esc dismiss".to_string(),
        ];
        let downs = std::sync::Arc::new(AtomicUsize::new(0));
        let enters = std::sync::Arc::new(AtomicUsize::new(0));
        let dd = downs.clone();
        let dr = downs.clone();
        let ee_read = enters.clone();
        let ee_send = enters.clone();
        let texts: std::sync::Arc<Mutex<Vec<String>>> = std::sync::Arc::new(Mutex::new(vec![]));
        let tx = texts.clone();
        let (h5, h6, o1, s1) = (highlight_opt5, highlight_own, opened2x, saved);
        let mut terminal = crate::inject::question::FreeTextClosures {
            read: move || {
                if ee_read.load(Ordering::SeqCst) >= 2 {
                    return Some(s1.clone());
                }
                if ee_read.load(Ordering::SeqCst) >= 1 {
                    return Some(o1.clone());
                }
                if dr.load(Ordering::SeqCst) >= 1 {
                    return Some(h6.clone());
                }
                Some(h5.clone())
            },
            send: move |k: &str| {
                if k == "down" {
                    dd.fetch_add(1, Ordering::SeqCst);
                }
                if k == "enter" {
                    ee_send.fetch_add(1, Ordering::SeqCst);
                }
                Ok(())
            },
            send_text: move |t: &str| {
                tx.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(t.to_string());
                Ok(())
            },
            settle: || {},
        };
        let out = run_opencode_own_answer_stages("seek text", false, || Ok(None), &mut terminal)
            .expect("footer 信号到位即停 → 全链走通");
        assert_eq!(
            out.sent_keys,
            vec!["down", "enter", "<text>", "enter"],
            "↓×1 后 footer 已示 enter edit → 提前停手（旧盲走会 ↓×5 回绕错行）"
        );
        assert_eq!(out.screen_text.as_deref(), Some("seek text"));
        assert_eq!(out.screen_checked, Some(true));
        assert_eq!(
            texts.lock().unwrap_or_else(|e| e.into_inner()).as_slice(),
            ["seek text"]
        );
    }

    /// **enter 探针走位脚本锁（2026-10-05 深夜实测定案）**：已存文本 own 行的 footer
    /// 与选项行**同词形**（`enter toggle`——高亮驻留 own 行时实测），footer 寻位对
    /// 覆盖写入/清空入口**天然失效**。探针路径两锁：
    /// ① **高亮已在 own 行**（保存后驻留态，用户「发送→覆盖写入」序列）：首个 enter
    ///    即勾选+进编辑（footer `enter done`）→ 退格清空 → 新文本 → 保存；
    /// ② **高亮在选项行**：enter 落在选项上（勾选翻转可观测）→ 立即还原 → ↓ 继续，
    ///    下一位 enter 命中 own 行 → 全链完成；还原必须复原（比对原屏）。
    #[test]
    fn opencode_enter_probe_walk_on_saved_row() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        let base = vec![
            "1. [ ] alpha".to_string(),
            "2. [ ] bravo".to_string(),
            "3. [ ] charlie".to_string(),
            "4. [ ] delta".to_string(),
            "5. [ ] echo".to_string(),
            "6. [ ] probe old".to_string(),
            "⇆ tab  ↑ select  enter toggle  esc dismiss".to_string(),
        ];
        let opened2x = vec![
            "1. [ ] alpha".to_string(),
            "6. [✓] probe old".to_string(),
            "⇆ tab  enter done  esc close".to_string(),
        ];
        let saved = vec![
            "1. [ ] alpha".to_string(),
            "6. [✓] new text".to_string(),
            "⇆ tab  ↑ select  enter toggle  esc dismiss".to_string(),
        ];
        let downs5 = ["down", "down", "down", "down", "down"];

        // ① **快路径（2026-10-05 用户提案）**：高亮在首行（常态）→ 盲走 ↓×5 直达
        //    own 行 → 单探针命中（编辑器展开）→ 打字 → 保存。中间行零闪烁。
        //    read 序 = [base(首读), base(盲走后 cur), opened2x(探针命中),
        //    opened2x(裸打字守卫), saved(7.5 复核), saved(终读)]
        let reads = Arc::new(AtomicUsize::new(0));
        let r1r = reads.clone();
        let texts1: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        let t1r = texts1.clone();
        let (b0, o1, sv1) = (base.clone(), opened2x.clone(), saved.clone());
        let mut terminal = crate::inject::question::FreeTextClosures {
            read: move || {
                let i = r1r.fetch_add(1, Ordering::SeqCst);
                if i <= 1 {
                    Some(b0.clone())
                } else if i <= 3 {
                    Some(o1.clone())
                } else {
                    Some(sv1.clone())
                }
            },
            send: move |_k: &str| Ok(()),
            send_text: move |t: &str| {
                t1r.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(t.to_string());
                Ok(())
            },
            settle: || {},
        };
        let out = run_opencode_own_answer_stages("new text", true, || Ok(None), &mut terminal)
            .expect("快路径盲走直达 → 单探针命中全链走通");
        assert_eq!(
            out.sent_keys,
            vec![
                "down",
                "down",
                "down",
                "down",
                "down",
                "enter",
                "<backspace×9>",
                "<text>",
                "enter"
            ],
            "盲走 ↓×5 + 单探针命中（中间行零闪烁）"
        );
        assert_eq!(out.screen_text.as_deref(), Some("new text"));
        assert_eq!(
            texts1.lock().unwrap_or_else(|e| e.into_inner()).as_slice(),
            ["new text"]
        );

        // ② **退化路径（自愈）**：高亮不在首行（实际在 row 4）→ 盲走落点未命中
        //    （探针翻转 row 4）→ 还原 → 退化逐行探针走位 → row 6 命中。
        //    read 序 = [base(首读), base(盲走后), flipped4(探针), base(还原后),
        //    base(退化 cur), flipped4(退化探针), base(还原), base(↓ row5),
        //    flipped5(退化探针), base(还原), base(↓ row6), opened2x(命中),
        //    opened2x(守卫), saved(复核), saved(终读)]
        let flipped4 = vec![
            "1. [ ] alpha".to_string(),
            "2. [ ] bravo".to_string(),
            "3. [ ] charlie".to_string(),
            "4. [✓] delta".to_string(),
            "5. [ ] echo".to_string(),
            "6. [ ] probe old".to_string(),
            "⇆ tab  ↑ select  enter toggle  esc dismiss".to_string(),
        ];
        let flipped5 = vec![
            "1. [ ] alpha".to_string(),
            "2. [ ] bravo".to_string(),
            "3. [ ] charlie".to_string(),
            "4. [ ] delta".to_string(),
            "5. [✓] echo".to_string(),
            "6. [ ] probe old".to_string(),
            "⇆ tab  ↑ select  enter toggle  esc dismiss".to_string(),
        ];
        let reads2 = Arc::new(AtomicUsize::new(0));
        let r2r = reads2.clone();
        let texts2: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        let t2r = texts2.clone();
        let script: Vec<Option<Vec<String>>> = vec![
            Some(base.clone()),
            Some(base.clone()),
            Some(flipped4.clone()),
            Some(base.clone()),
            Some(base.clone()),
            Some(flipped4.clone()),
            Some(base.clone()),
            Some(base.clone()),
            Some(flipped5.clone()),
            Some(base.clone()),
            Some(base.clone()),
            Some(opened2x.clone()),
            Some(opened2x),
            Some(saved.clone()),
            Some(saved),
        ];
        let mut terminal2 = crate::inject::question::FreeTextClosures {
            read: move || {
                let i = r2r.fetch_add(1, Ordering::SeqCst);
                script.get(i).cloned().flatten()
            },
            send: |_k: &str| Ok(()),
            send_text: move |t: &str| {
                t2r.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(t.to_string());
                Ok(())
            },
            settle: || {},
        };
        let out2 = run_opencode_own_answer_stages("new text", true, || Ok(None), &mut terminal2)
            .expect("退化路径自愈到达");
        let downs = downs5.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut expected = downs.clone();
        expected.extend(
            [
                "enter",
                "enter", // 快路径探针 + 还原（未命中）
                "enter",
                "enter",
                "down", // 退化：row4 探针+还原+↓
                "enter",
                "enter",
                "down",  // 退化：row5 探针+还原+↓
                "enter", // 退化：row6 命中（编辑器展开）
                "<backspace×9>",
                "<text>",
                "enter",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        assert_eq!(out2.sent_keys, expected, "未命中退化自愈");
        assert_eq!(out2.screen_text.as_deref(), Some("new text"));
        assert_eq!(
            texts2.lock().unwrap_or_else(|e| e.into_inner()).as_slice(),
            ["new text"]
        );
    }

    /// **保存后勾选闭环脚本锁（2026-10-05 用户实机语义）**：own 行勾选标记是
    /// Submit 汇总计数的依据，勾不在 = 文本不进答案。三锁：
    /// ① 保存后勾掉落（生产实测形态）→ enter 舞步补勾（勾选+编辑 → 保存）→ 复核通过；
    /// ② 保存后已换题（1.x 形态：行内容与所写文本无前缀关系）→ **零补键**（绝不误补下一题）；
    /// ③ 补勾两次仍不在 → 终判报错（不虚报完成）。
    #[test]
    fn opencode_post_save_recheck_restores_dropped_check() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        let fresh_hl = vec![
            "1. [ ] alpha".to_string(),
            "6. [ ] Type your own answer".to_string(),
            "⇆ tab  ↑ select  enter edit  esc dismiss".to_string(),
        ];
        let editing = vec![
            "1. [ ] alpha".to_string(),
            "6. [✓] Type your own answer".to_string(),
            "⇆ tab  enter done  esc close".to_string(),
        ];
        let dropped = vec![
            "1. [ ] alpha".to_string(),
            "6. [ ] my answer".to_string(),
            "⇆ tab  ↑ select  enter toggle  esc dismiss".to_string(),
        ];
        let checked = vec![
            "1. [ ] alpha".to_string(),
            "6. [✓] my answer".to_string(),
            "⇆ tab  ↑ select  enter toggle  esc dismiss".to_string(),
        ];
        let script = [
            fresh_hl.clone(), // 首读（守卫：非编辑态）
            fresh_hl.clone(), // seek cur：footer enter edit → 高亮已到位零走位
            fresh_hl.clone(), // after_walk
            editing.clone(),  // 裸打字守卫：编辑态开启
            dropped.clone(),  // 保存后勾掉落（补勾 attempt1 cur）
            editing.clone(),  // 补勾 enter → 编辑态开启 → 补保存
            checked.clone(),  // attempt2 cur：勾已在 → break
            checked.clone(),  // 终读
        ];
        let reads = Arc::new(AtomicUsize::new(0));
        let r = reads.clone();
        let sent: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        let s2 = sent.clone();
        let mut terminal = crate::inject::question::FreeTextClosures {
            read: move || {
                let i = r.fetch_add(1, Ordering::SeqCst);
                script.get(i).cloned()
            },
            send: move |k: &str| {
                s2.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(k.to_string());
                Ok(())
            },
            send_text: |_t: &str| Ok(()),
            settle: || {},
        };
        let out = run_opencode_own_answer_stages("my answer", false, || Ok(None), &mut terminal)
            .expect("勾掉落 → 补勾闭环走通");
        assert_eq!(
            out.sent_keys,
            vec!["enter", "<text>", "enter", "enter", "enter"],
            "保存后补勾 = enter(勾选+编辑) + enter(保存) 两键"
        );
        assert_eq!(out.screen_text.as_deref(), Some("my answer"));
        assert_eq!(out.screen_checked, Some(true), "终态勾必须在");
    }

    #[test]
    fn opencode_post_save_recheck_skips_when_row_advanced() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        let fresh_hl = vec![
            "1. [ ] alpha".to_string(),
            "6. [ ] Type your own answer".to_string(),
            "⇆ tab  ↑ select  enter edit  esc dismiss".to_string(),
        ];
        let editing = vec![
            "1. [ ] alpha".to_string(),
            "6. [✓] Type your own answer".to_string(),
            "⇆ tab  enter done  esc close".to_string(),
        ];
        let nextq = vec![
            "1. [ ] first".to_string(),
            "2. [ ] second".to_string(),
            "⇆ tab  ↑ select  enter toggle  esc dismiss".to_string(),
        ];
        let script = [
            fresh_hl.clone(),
            fresh_hl.clone(),
            fresh_hl.clone(),
            editing,
            nextq.clone(), // 补勾 attempt1 cur：行内容已是下一题（saved="second" 非所写文本前缀）→ 零补键
            nextq.clone(), // 终读
        ];
        let reads = Arc::new(AtomicUsize::new(0));
        let r = reads.clone();
        let sent: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        let s2 = sent.clone();
        let mut terminal = crate::inject::question::FreeTextClosures {
            read: move || {
                let i = r.fetch_add(1, Ordering::SeqCst);
                script.get(i).cloned()
            },
            send: move |k: &str| {
                s2.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(k.to_string());
                Ok(())
            },
            send_text: |_t: &str| Ok(()),
            settle: || {},
        };
        let out = run_opencode_own_answer_stages("my answer", false, || Ok(None), &mut terminal)
            .expect("1.x 换题形态走通");
        assert_eq!(
            out.sent_keys,
            vec!["enter", "<text>", "enter"],
            "换题后绝不误补下一题（零补键）"
        );
    }

    #[test]
    fn opencode_post_save_recheck_fails_loud_when_unchecked() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let fresh_hl = vec![
            "1. [ ] alpha".to_string(),
            "6. [ ] Type your own answer".to_string(),
            "⇆ tab  ↑ select  enter edit  esc dismiss".to_string(),
        ];
        let editing = vec![
            "1. [ ] alpha".to_string(),
            "6. [✓] Type your own answer".to_string(),
            "⇆ tab  enter done  esc close".to_string(),
        ];
        let dropped = vec![
            "1. [ ] alpha".to_string(),
            "6. [ ] my answer".to_string(),
            "⇆ tab  ↑ select  enter toggle  esc dismiss".to_string(),
        ];
        let script = [
            fresh_hl.clone(),
            fresh_hl.clone(),
            fresh_hl.clone(),
            editing,
            dropped.clone(), // attempt1：掉落 → enter → 仍掉落且无翻转 → down
            dropped.clone(),
            dropped.clone(), // attempt2：同上
            dropped.clone(),
            dropped.clone(), // 终读：文本在、勾不在
        ];
        let reads = Arc::new(AtomicUsize::new(0));
        let r = reads.clone();
        let mut terminal = crate::inject::question::FreeTextClosures {
            read: move || {
                let i = r.fetch_add(1, Ordering::SeqCst);
                script.get(i).cloned()
            },
            send: |_k: &str| Ok(()),
            send_text: |_t: &str| Ok(()),
            settle: || {},
        };
        let err = run_opencode_own_answer_stages("my answer", false, || Ok(None), &mut terminal)
            .expect_err("补勾失败必须报错（文本不会计入 Submit，不虚报完成）");
        assert!(err.message.contains("勾选标记缺失"), "{err:?}");
    }

    /// **单选页（无勾选框）自由作答流程脚本锁（2026-10-05 34808 会话实测定案）**：
    /// 单选页 ↓ 即输入、enter=整题提交、无勾选框语义。三锁：
    /// ① 常态：盲走 ↓×3 → 打字 → 落行验证 → enter 提交 → own 行消失；
    /// ② 残留未覆盖 → 零键拒绝；
    /// ③ 文字未落行（光标被挪）→ 拒绝且**不发提交键**（enter 在单选页=误提交）。
    #[test]
    #[ignore = "单选 N 探针流程重写后脚本锁待重写（2026-10-05 深夜）"]
    fn opencode_single_select_own_answer_flow() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        let single_page = vec![
            "1. alpha".to_string(),
            "2. bravo".to_string(),
            "3. charlie".to_string(),
            "4. Type your own answer".to_string(),
            "↑ select  enter submit  esc dismiss".to_string(),
        ];
        let typed = vec![
            "1. alpha".to_string(),
            "2. bravo".to_string(),
            "3. charlie".to_string(),
            "4. hello".to_string(),
            "↑ select  enter submit  esc dismiss".to_string(),
        ];
        let answered = vec![
            "# Questions".to_string(),
            "Which option do you want to pick?".to_string(),
            "hello".to_string(),
        ];
        // ① 常态全链
        let reads = Arc::new(AtomicUsize::new(0));
        let r = reads.clone();
        let sent: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        let s2 = sent.clone();
        let texts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        let tx = texts.clone();
        let (p1, t1, a1) = (single_page.clone(), typed.clone(), answered.clone());
        let mut terminal = crate::inject::question::FreeTextClosures {
            read: move || {
                let i = r.fetch_add(1, Ordering::SeqCst);
                // 读序：first(页面) → 打字后(typed 落行验证) → 提交后(answered)
                if i == 0 {
                    Some(p1.clone())
                } else if i == 1 {
                    Some(t1.clone())
                } else {
                    Some(a1.clone())
                }
            },
            send: move |k: &str| {
                s2.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(k.to_string());
                Ok(())
            },
            send_text: move |t: &str| {
                tx.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(t.to_string());
                Ok(())
            },
            settle: || {},
        };
        let out = run_opencode_own_answer_stages("hello", false, || Ok(None), &mut terminal)
            .expect("单选页常态全链走通");
        assert_eq!(
            out.sent_keys,
            vec!["down", "down", "down", "<text>", "enter"],
            "盲走 ↓×3 + 打字 + enter 提交（中间零闪烁零探针）"
        );
        assert_eq!(out.screen_text.as_deref(), Some("hello"));
        assert_eq!(out.screen_checked, None, "单选无勾选语义");
        assert_eq!(
            texts.lock().unwrap_or_else(|e| e.into_inner()).as_slice(),
            ["hello"]
        );

        // ② 残留未覆盖 → 零键拒绝
        let residue = vec![
            "1. alpha".to_string(),
            "2. bravo".to_string(),
            "3. charlie".to_string(),
            "4. 残留文字".to_string(),
            "↑ select  enter submit  esc dismiss".to_string(),
        ];
        let reads2 = Arc::new(AtomicUsize::new(0));
        let r2 = reads2.clone();
        let mut terminal2 = crate::inject::question::FreeTextClosures {
            read: move || {
                let _ = r2.fetch_add(1, Ordering::SeqCst);
                Some(residue.clone())
            },
            send: |_k: &str| Ok(()),
            send_text: |_t: &str| Ok(()),
            settle: || {},
        };
        let err = run_opencode_own_answer_stages("hello", false, || Ok(None), &mut terminal2)
            .expect_err("残留未覆盖必须拒绝");
        assert!(err.message.contains("覆盖写入"), "{err:?}");

        // ③ 文字未落行（光标被挪）→ 拒绝且零提交键
        let reads3 = Arc::new(AtomicUsize::new(0));
        let r3 = reads3.clone();
        let sent3: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        let s3 = sent3.clone();
        let script3: Vec<Option<Vec<String>>> = vec![Some(single_page.clone()); 9];
        let mut terminal3 = crate::inject::question::FreeTextClosures {
            read: move || {
                let i = r3.fetch_add(1, Ordering::SeqCst);
                script3.get(i).cloned().flatten()
            },
            send: move |k: &str| {
                s3.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(k.to_string());
                Ok(())
            },
            send_text: |_t: &str| Ok(()),
            settle: || {},
        };
        let err = run_opencode_own_answer_stages("hello", false, || Ok(None), &mut terminal3)
            .expect_err("文字未落行必须拒绝");
        assert!(err.message.contains("已逐行重试"), "{err:?}");
        let sent_now = sent3.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            !sent_now.contains(&"enter".to_string()),
            "提交键绝不发出：{sent_now:?}"
        );
    }

    /// **codex 备注编辑器守卫两态**：notes 开启（footer `tab or esc to clear
    /// notes`）→ 拒绝；答案态/读不到屏 → 放行。
    #[test]
    fn codex_digit_guard_notes_states() {
        let notes_open = vec![
            "  › 1. MIT (Recommended)".to_string(),
            "  › NOTE-TEXT".to_string(),
            "  tab or esc to clear notes | enter to submit answer".to_string(),
        ];
        assert!(codex_ensure_notes_closed(Some(notes_open)).is_err());
        let answer_state = vec![
            "  › 1. MIT (Recommended)".to_string(),
            "  tab to add notes | enter to submit answer".to_string(),
        ];
        assert!(codex_ensure_notes_closed(Some(answer_state)).is_ok());
        assert!(
            codex_ensure_notes_closed(None).is_ok(),
            "读不到屏放行（保持既有盲发行为）"
        );
    }

    /// **已保存内容的 toggle 双 enter 脚本锁**（2026-10-04 用户实测语义）：
    /// own answer 行已勾选且有保存文本（`5. [v] custom old`）→ enter #1 = **取消
    /// 勾选**（屏复核行变 `[ ]`，无占位行）→ enter #2 = 重新勾选 + 进编辑态（占位
    /// 行出现）→ 覆盖写入 = backspace × 已存文本长度（光标在末尾）→ 文本 → enter
    /// 保存。非覆盖路径对已保存内容必须中止（直接发送会追加）。
    #[test]
    fn e6_opencode_own_answer_double_enter_on_saved_content() {
        use std::sync::Mutex;
        let fresh = vec![
            "1. [v] alpha".to_string(),
            "5. [v] custom old".to_string(),
            "⇆ tab  ↑ select  enter toggle  esc dismiss".to_string(),
        ];
        let unchecked = vec![
            "1. [v] alpha".to_string(),
            "5. [ ] custom old".to_string(),
            "⇆ tab  ↑ select  enter toggle  esc dismiss".to_string(),
        ];
        let opened = vec![
            "5. [v] custom old".to_string(),
            "   Type your own answer".to_string(),
        ];
        let enters = std::sync::Arc::new(Mutex::new(0usize));
        let bks = std::sync::Arc::new(Mutex::new(0usize));
        let texts: std::sync::Arc<Mutex<Vec<String>>> = std::sync::Arc::new(Mutex::new(vec![]));
        let (er, br, tr) = (enters.clone(), bks.clone(), texts.clone());
        let (fresh_a, unchecked_a, opened_a) = (fresh.clone(), unchecked.clone(), opened.clone());
        let er_read = er.clone();
        let mut terminal = crate::inject::question::FreeTextClosures {
            read: move || {
                let e = *er_read.lock().unwrap_or_else(|e| e.into_inner());
                if e >= 2 {
                    Some(opened_a.clone())
                } else if e == 1 {
                    Some(unchecked_a.clone())
                } else {
                    Some(fresh_a.clone())
                }
            },
            send: move |k: &str| {
                if k == "enter" {
                    *er.lock().unwrap_or_else(|e| e.into_inner()) += 1;
                }
                if k == "backspace" {
                    *br.lock().unwrap_or_else(|e| e.into_inner()) += 1;
                }
                Ok(())
            },
            send_text: move |t: &str| {
                tr.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(t.to_string());
                Ok(())
            },
            settle: || {},
        };
        let out = run_opencode_own_answer_stages("custom new", true, || Ok(None), &mut terminal)
            .expect("已保存内容双 enter 全链走通");
        // 键序（快路径）：盲走 ↓×4 直达 own 行 → 探针摘勾（已勾行语义）→ 补键
        // 勾回+编辑器展开 → backspace×10 → <text> → enter 保存
        assert_eq!(
            out.sent_keys,
            vec![
                "down",
                "down",
                "down",
                "down",
                "enter",
                "enter",
                "<backspace×10>",
                "<text>",
                "enter"
            ]
        );
        assert_eq!(*bks.lock().unwrap_or_else(|e| e.into_inner()), 10);
        assert_eq!(
            texts.lock().unwrap_or_else(|e| e.into_inner()).as_slice(),
            ["custom new"]
        );

        // 非覆盖路径：已有保存内容 → 中止零按键（直接发送会追加）
        let fresh2 = fresh.clone();
        let mut terminal2 = crate::inject::question::FreeTextClosures {
            read: move || Some(fresh2.clone()),
            send: move |_k: &str| Ok(()),
            send_text: move |_t: &str| Ok(()),
            settle: || {},
        };
        let err = run_opencode_own_answer_stages("y", false, || Ok(None), &mut terminal2)
            .expect_err("已有保存内容非覆盖 → 中止");
        assert!(err.message.contains("覆盖写入"), "{err:?}");
    }

    /// **opencode answers 二维数组对账**（戊探A ⑥ 录得形态：按题序对齐 + 内层勾选序）。
    #[test]
    fn e6_opencode_answers_two_dimensional_reconciliation() {
        let meta = r#"{"answers":[["teal accent E-A2"],["tracing","logging"]],"truncated":false}"#;
        let answers = parse_opencode_answers(meta).expect("二维数组解析");
        assert_eq!(answers.len(), 2, "按题序对齐（非题干键控）");
        assert_eq!(answers[0], vec!["teal accent E-A2"]);
        assert_eq!(answers[1], vec!["tracing", "logging"], "内层序=勾选序");
        assert!(parse_opencode_answers(r#"{"answers":"oops"}"#).is_none());
    }

    // ==== 批次戊 E5（2026-10-10 复活批重写）：codex notes 链编排序列锁 ====
    // 旧「answer_footer 三行夹具 + 旧五参签名」脚本锁随 0.162.1 语义重写作废——
    // 新编排 [`run_codex_notes_stages`] 每步屏读核验（入口闸解析真面板帧 → 走位
    // Other → tab footer 翻转 → 打字上屏 → enter 终态），夹具必须用
    // `codex_question_screen_snapshot` 能解析的完整面板帧（真机基底
    // `live_fixtures::codex_s1_q2*`——4 选项屏，Other 行 = 第 4 项）。

    /// 帧派生：footer 的 `tab to add notes` 段翻成备注态词形
    /// `tab or esc to clear notes`（0.162.1 复验 N3 的 footer 翻转形态）。footer
    /// 其余段不动——配对锚（submit 段 / navigate 段）仍在场，快照解析不受影响。
    fn codex_notes_footer_frame(base: &[String]) -> Vec<String> {
        base.iter()
            .map(|l| l.replace("tab to add notes", "tab or esc to clear notes"))
            .collect()
    }

    /// 帧派生：备注态帧 + 已上屏的备注文本行（打字后屏读形态——0.162.1 字符层
    /// 回显可见；行只需包含全文，`screen_contains_text` 做剥空白归一化子串判定）。
    fn codex_notes_typed_frame(base: &[String], text: &str) -> Vec<String> {
        let mut v = codex_notes_footer_frame(base);
        let pos = v
            .iter()
            .rposition(|l| l.contains("tab or esc to clear notes"))
            .expect("基底必须是备注态帧（footer 已翻转）");
        v.insert(pos, format!("  {text}"));
        v
    }

    /// 帧派生：备注态帧 + **有字的 note 行**（`› OLD-NOTE` 形态——`codex_notes_row_text`
    /// 判据：题号头下首个 `› ` 前缀非选项行，非 `Add notes` 占位即有字）。插在选项区
    /// 之后、footer 之前：快照解析在该行 break（选项区已收口），note 判定恰好命中。
    fn codex_notes_with_text_frame(base: &[String], note: &str) -> Vec<String> {
        let mut v = codex_notes_footer_frame(base);
        let pos = v
            .iter()
            .rposition(|l| l.contains("tab or esc to clear notes"))
            .expect("基底必须是备注态帧（footer 已翻转）");
        v.insert(pos, format!("  › {note}"));
        v
    }

    /// codex notes 编排脚本驱动器（同 `run_codex_select_script` 的抽法）：screens
    /// 逐帧出队，`terminal.read()` 与 `poll()` 共用同一队列——出队序 = 编排调用序；
    /// 队空 = read None / poll Ok(None)（窗尽）。send / send_text 双轨记账
    /// （sent_keys 的 `<text>` 占位与文本通道互证）。
    fn run_codex_notes_script(
        q_idx: usize,
        expected_total: usize,
        text: &str,
        overwrite: bool,
        screens: Vec<Vec<String>>,
    ) -> (
        Result<FreeTextOutcome, StageAbort>,
        Vec<String>,
        Vec<String>,
    ) {
        use std::cell::RefCell;
        let screens = RefCell::new(std::collections::VecDeque::from(screens));
        let sent = RefCell::new(Vec::<String>::new());
        let texts = RefCell::new(Vec::<String>::new());
        let mut terminal = crate::inject::question::FreeTextClosures {
            read: || screens.borrow_mut().pop_front(),
            send: |k: &str| {
                sent.borrow_mut().push(k.to_string());
                Ok(())
            },
            send_text: |t: &str| {
                texts.borrow_mut().push(t.to_string());
                Ok(())
            },
            settle: || {},
        };
        let poll = || Ok(screens.borrow_mut().pop_front());
        (
            run_codex_notes_stages(q_idx, expected_total, text, overwrite, poll, &mut terminal),
            sent.into_inner(),
            texts.into_inner(),
        )
    }

    /// **① 答案态全链**（场景锁主路径）：`codex_s1_q2_focused(1)` 入场（Question
    /// 2/2，1 未答，焦点 Memcached=选项 2）→ 走位 Other（m=4、目标 idx 3：
    /// down=(3+4-1)%4=2、up=(1+4-3)%4=2 平手 → j 默认方向 ×2，每步复读焦点推进）
    /// → tab 开 notes（poll footer 翻转 + **输入行就绪门**——`› Add notes` 占位
    /// 也算就绪，2026-10-10 20:55 实机后新增）→ 打字（poll 验文本上屏）→ enter
    /// （poll 终态回执）。sent_keys = ["j","j","tab","<text>","enter"]（就绪门零键），
    /// receipt Some(true)。
    #[test]
    fn e5_codex_notes_full_chain_scripted() {
        let notes = codex_notes_footer_frame(&live_fixtures::codex_s1_q2_focused(3));
        let ready =
            codex_notes_with_text_frame(&live_fixtures::codex_s1_q2_focused(3), "Add notes");
        let (r, sent, texts) = run_codex_notes_script(
            1,
            2,
            "NOTE-XYZ",
            false,
            vec![
                live_fixtures::codex_s1_q2_focused(1), // 入口闸 read：焦点 1
                live_fixtures::codex_s1_q2_focused(2), // j 走位复读：焦点 2（未到）
                live_fixtures::codex_s1_q2_focused(3), // j 走位复读：焦点 3 = Other → 停
                notes.clone(),                         // tab 后 poll：footer 翻出备注态
                ready, // 输入行就绪门：› Add notes 占位即就绪（零键）
                codex_notes_typed_frame(&notes, "NOTE-XYZ"), // 打字后 poll：文本上屏
                live_fixtures::codex_answered_summary(), // enter 后 poll：终态回执
            ],
        );
        let out = r.expect("答案态全链必须成功");
        assert_eq!(
            out.sent_keys,
            vec!["j", "j", "tab", "<text>", "enter"],
            "{sent:?}"
        );
        assert_eq!(texts, vec!["NOTE-XYZ"], "备注走字符通道");
        assert_eq!(out.receipt_seen, Some(true), "终态回执在场");
        assert!(!out.advanced);
    }

    /// **② 身份闸不匹配中止零键**：屏上题号 ≠ 手机卡题号（第 2 题 vs 第 1 题）、
    /// 屏上问卷题数 ≠ 手机卡题数（2 vs 3）——两臂都中止且零按键（错卷发键 =
    /// 打错题，与 select 阶段机同款闸）。
    #[test]
    fn e5_codex_notes_identity_gate_aborts_zero_keys() {
        for (q_idx, expected_total) in [(0, 2), (1, 3)] {
            let (r, sent, texts) = run_codex_notes_script(
                q_idx,
                expected_total,
                "NOTE",
                false,
                vec![live_fixtures::codex_s1_q2()],
            );
            let err = r.expect_err("身份闸不匹配必须中止");
            assert!(
                err.message.contains("身份闸") && err.message.contains("未发任何键"),
                "{err:?}"
            );
            assert!(
                sent.is_empty() && texts.is_empty(),
                "中止零按键零文本：{sent:?} / {texts:?}"
            );
        }
    }

    /// **③ Other 行缺席中止零键**：4 选项屏去掉 `None of the above` 行（编号仍
    /// 连续可解析）→ 本题面板不支持自由作答，零键中止引导终端。
    #[test]
    fn e5_codex_notes_other_row_absent_aborts_zero_keys() {
        let no_other = live_fixtures::codex_s1_q2()
            .into_iter()
            .filter(|l| !l.contains("None of the above"))
            .collect::<Vec<_>>();
        let (r, sent, texts) = run_codex_notes_script(1, 2, "NOTE", false, vec![no_other]);
        let err = r.expect_err("无 Other 行必须中止");
        assert!(
            err.message.contains("没有 Other 行") && err.message.contains("未发任何键"),
            "{err:?}"
        );
        assert!(sent.is_empty() && texts.is_empty(), "中止零按键：{sent:?}");
    }

    /// **④ 入场备注态残留中止零键（仅发送路径）**：2026-10-10 用户规格后守卫收窄——
    /// 发送路径（!overwrite）入场 footer 已是 `tab or esc to clear notes` → 中止
    /// （发送会追加在旧文后，编排不接手别人的半程状态）；**清空/覆盖写入不拦**
    /// （备注态有字正是操作对象）——用同一入场形态 + overwrite 走进清空段证明
    /// 守卫没拦（中止点变为清空段读屏，文案不再是「备注输入态」）。
    #[test]
    fn e5_codex_notes_residual_notes_state_aborts_zero_keys() {
        // 臂 1（发送路径）：残留中止零键
        let residue = codex_notes_footer_frame(&live_fixtures::codex_s1_q2_focused(1));
        let (r, sent, texts) = run_codex_notes_script(1, 2, "NOTE", false, vec![residue]);
        let err = r.expect_err("发送路径入场备注态残留必须中止");
        assert!(
            err.message.contains("备注输入态") && err.message.contains("未发任何键"),
            "{err:?}"
        );
        assert!(sent.is_empty() && texts.is_empty(), "中止零按键：{sent:?}");

        // 臂 2（守卫收窄）：同款入场（有字备注态）+ overwrite → 守卫不拦，进入
        // 清空段——队列即空 → 清空段读不到屏中止（证明走到清空段而非守卫拦下）
        let residue_with_note =
            codex_notes_with_text_frame(&live_fixtures::codex_s1_q2_focused(1), "OLD-NOTE");
        let (r2, sent2, _) = run_codex_notes_script(1, 2, "NEW", true, vec![residue_with_note]);
        let err2 = r2.expect_err("队列即空，清空段必在读屏处中止");
        assert!(
            err2.message.contains("清空段") && !err2.message.contains("备注输入态"),
            "守卫只拦发送路径——overwrite 进入清空段：{err2:?}"
        );
        assert!(sent2.is_empty(), "中止零按键：{sent2:?}");
    }

    /// **⑤ 已答 ∧ 焦点≠Other 的改答守卫三臂**（2026-10-10 用户规格后新增
    /// had_note 豁免）：归零词形入场（`Question 2/2` 计数段消失 = unanswered 0）
    /// 且焦点在 Memcached（≠ Other）——tab 的 ensure 选中会把答案**换到 Other**
    /// （换选路径 0.162.1 未复采）。
    /// - 臂 A（发送路径，无豁免）：中止零键；
    /// - 臂 B（覆盖 ∧ 入场有 note → **豁免**）：note 只能经 Other-notes 流产生，
    ///   旧答案必是 Other，走位+ensure 是选回同一行——守卫放行（用清空+esc 后
    ///   「Other 行缺席」的可区分中止点证明守卫确实过了）；
    /// - 臂 C（覆盖 ∧ 入场无 note → **不豁免**）：豁免以 note 为证，无 note 仍中止。
    #[test]
    fn e5_codex_notes_answered_non_other_focus_aborts() {
        // 臂 A：发送路径，守卫原样拦下
        let (r, sent, texts) =
            run_codex_notes_script(1, 2, "NOTE", false, vec![live_fixtures::codex_s1_q2_zero()]);
        let err = r.expect_err("已答且焦点不在 Other 必须中止");
        assert!(
            err.message.contains("已作答") && err.message.contains("未发任何键"),
            "{err:?}"
        );
        assert!(sent.is_empty() && texts.is_empty(), "中止零按键：{sent:?}");

        // 臂 B：覆盖 + 有 note → 豁免放行。帧序：入场有字备注态 → 清空段一下 tab
        // → 空备注态终判 → esc 退回答案态 → 发送段重读（去掉 Other 行的已答屏）
        // → 走过守卫、落在 Other 行缺席的中止点（≠「已作答」即豁免证明）。
        let with_note = codex_notes_with_text_frame(&live_fixtures::codex_s1_q2_zero(), "OLD-NOTE");
        let empty_notes = codex_notes_footer_frame(&live_fixtures::codex_s1_q2_zero());
        let no_other = live_fixtures::codex_s1_q2_zero()
            .into_iter()
            .filter(|l| !l.contains("None of the above"))
            .collect::<Vec<_>>();
        let (rb, sentb, _) = run_codex_notes_script(
            1,
            2,
            "NEW",
            true,
            vec![
                with_note.clone(),                 // 入口闸 read：有字备注态（had_note=true）
                with_note,                         // 清空段 read#1：note 有字 → tab#1
                empty_notes.clone(),               // tab#1 后 poll：note 行已无字
                empty_notes.clone(),               // 清空段 read#2：无字 → 循环终止
                empty_notes,                       // 终判 read：无字 ✓ 且 footer 仍备注态 → esc
                live_fixtures::codex_s1_q2_zero(), // esc 后 poll：退回答案态
                no_other, // 发送段 read：已答屏但无 Other 行 → 守卫后中止点
            ],
        );
        let errb = rb.expect_err("队列尾帧无 Other 行，必在发送段落到具名中止");
        assert!(
            errb.message.contains("没有 Other 行") && !errb.message.contains("已作答"),
            "豁免放行（否则先撞「已作答」）：{errb:?}"
        );
        assert_eq!(sentb, vec!["tab", "esc"], "清空一下 + esc 已发：{sentb:?}");

        // 臂 C：覆盖 + 无 note → 豁免不成立，守卫照拦（清空段零 tab 即无字直达终判）
        let zero = live_fixtures::codex_s1_q2_zero();
        let (rc, sentc, _) = run_codex_notes_script(
            1,
            2,
            "NEW",
            true,
            vec![zero.clone(), zero.clone(), zero.clone(), zero],
        );
        let errc = rc.expect_err("无 note 的覆盖不豁免，守卫必须拦");
        assert!(
            errc.message.contains("已作答") && errc.message.contains("未发任何键"),
            "{errc:?}"
        );
        assert!(sentc.is_empty(), "中止零按键：{sentc:?}");
    }

    /// **⑥ tab 后 footer 不翻转 → 中止且未打字未 enter**：tab 后 poll 队列只给
    /// 旧帧（答案态 footer，备注态始终未出现）→ 窗尽中止——打字与提交都不发生
    /// （notes 输入态没确认就打字 = 打进答案屏）。
    #[test]
    fn e5_codex_notes_tab_footer_not_flipped_aborts_before_typing() {
        let (r, sent, texts) = run_codex_notes_script(
            1,
            2,
            "NOTE-XYZ",
            false,
            vec![
                live_fixtures::codex_s1_q2_focused(1), // 入口闸 read
                live_fixtures::codex_s1_q2_focused(2), // j 走位复读
                live_fixtures::codex_s1_q2_focused(3), // j 走位复读 → 到 Other
                live_fixtures::codex_s1_q2_focused(3), // tab 后 poll 拍 1：旧帧
                live_fixtures::codex_s1_q2_focused(3), // tab 后 poll 拍 2：旧帧（后队空 = 窗尽）
            ],
        );
        let err = r.expect_err("footer 不翻转必须中止");
        assert!(err.message.contains("未见备注态"), "{err:?}");
        assert_eq!(sent, vec!["j", "j", "tab"], "中止在打字之前：{sent:?}");
        assert!(texts.is_empty(), "未打字：{texts:?}");
    }

    /// **⑦ 覆盖写入 = 清空循环 + esc + 发送段锁**（2026-10-10 用户最终规格；
    /// 旧「三连 tab」序列随清空段重构作废）：入场有字备注态（had_note）→ 清空段
    /// 一下 tab（note 行无字即循环终止）→ 终判停在空备注态（footer 仍备注态）→
    /// **esc 退回答案态** → 发送段重读重析（身份闸复核）→ 焦点已在 Other 零走位 →
    /// tab 开 notes → 打字（上屏核验）→ enter。sent_keys =
    /// ["tab","esc","tab","<text>","enter"]。
    #[test]
    fn e5_codex_notes_overwrite_clear_then_send_scripted() {
        let with_note =
            codex_notes_with_text_frame(&live_fixtures::codex_s1_q2_focused(3), "OLD-NOTE");
        let empty_notes = codex_notes_footer_frame(&live_fixtures::codex_s1_q2_focused(3));
        let ready =
            codex_notes_with_text_frame(&live_fixtures::codex_s1_q2_focused(3), "Add notes");
        let answer = live_fixtures::codex_s1_q2_focused(3);
        let (r, sent, texts) = run_codex_notes_script(
            1,
            2,
            "NEW-NOTE",
            true,
            vec![
                with_note.clone(),   // 入口闸 read：有字备注态（had_note=true）
                with_note,           // 清空段 read#1：note 有字 → tab#1
                empty_notes.clone(), // tab#1 后 poll：note 行已无字
                empty_notes.clone(), // 清空段 read#2：无字 → 循环提前终止（只发一下）
                empty_notes.clone(), // 终判 read：无字 ✓ 且 footer 仍备注态 → esc
                answer.clone(),      // esc 后 poll：退回答案态
                answer,              // 发送段 read：重读重析（焦点已在 Other，零走位）
                empty_notes.clone(), // 发送段 tab 后 poll：备注态
                ready,               // 输入行就绪门：› Add notes 占位即就绪（零键）
                codex_notes_typed_frame(&empty_notes, "NEW-NOTE"), // 打字后 poll：新文上屏
                live_fixtures::codex_answered_summary(), // enter 后 poll：终态回执
            ],
        );
        let out = r.expect("覆盖写入清空+发送全链必须成功");
        assert_eq!(
            out.sent_keys,
            vec!["tab", "esc", "tab", "<text>", "enter"],
            "{sent:?}"
        );
        assert_eq!(texts, vec!["NEW-NOTE"], "覆盖重打全文走字符通道");
        assert_eq!(out.receipt_seen, Some(true));
    }

    /// **⑦b 覆盖路径发送段 tab 后 footer 不翻转 → 中止且未打字未 enter**（⑦ 的
    /// 镜像失败臂，随清空段重构移位）：清空 + esc 都正常走完，发送段 tab 后 poll
    /// 只给旧帧（备注态始终未出现）→ 窗尽中止「Tab 后未见备注态」——打字资格
    /// 来自屏读确认，没确认就打字 = 打进已退出的答案态。
    #[test]
    fn e5_codex_notes_overwrite_send_tab_not_flipped_aborts() {
        let with_note =
            codex_notes_with_text_frame(&live_fixtures::codex_s1_q2_focused(3), "OLD-NOTE");
        let empty_notes = codex_notes_footer_frame(&live_fixtures::codex_s1_q2_focused(3));
        let answer = live_fixtures::codex_s1_q2_focused(3);
        let (r, sent, texts) = run_codex_notes_script(
            1,
            2,
            "NEW-NOTE",
            true,
            vec![
                with_note.clone(),   // 入口闸 read：有字备注态
                with_note,           // 清空段 read#1 → tab#1
                empty_notes.clone(), // poll：note 无字
                empty_notes.clone(), // 清空段 read#2 → 终止
                empty_notes,         // 终判 read → esc
                answer.clone(),      // esc 后 poll：答案态
                answer.clone(),      // 发送段 read：焦点在 Other
                answer.clone(),      // 发送段 tab 后 poll 拍 1：旧帧（答案态 footer）
                answer,              // 发送段 tab 后 poll 拍 2：旧帧（后队空 = 窗尽）
            ],
        );
        let err = r.expect_err("发送段 footer 不翻转必须中止");
        assert!(err.message.contains("Tab 后未见备注态"), "{err:?}");
        assert_eq!(
            sent,
            vec!["tab", "esc", "tab"],
            "清空+esc+开 notes 已发但止步于打字之前：{sent:?}"
        );
        assert!(texts.is_empty(), "未打字：{texts:?}");
    }

    /// **⑧ 清空请求（空文本 + overwrite）停在空备注态 → esc 退出后返回**：清空
    /// 一下 tab 后 note 已无字但 footer 仍备注态（空 notes 态）→ 终判 → esc 退回
    /// 答案态 → 清空请求至此完成：零文本零 enter（codex 无「只存不交」键，本题
    /// 提交走卡面提交按钮/后续动作）。sent_keys = ["tab","esc"]。
    #[test]
    fn e5_codex_notes_clear_empty_text_returns_without_enter() {
        let with_note =
            codex_notes_with_text_frame(&live_fixtures::codex_s1_q2_focused(3), "OLD-NOTE");
        let empty_notes = codex_notes_footer_frame(&live_fixtures::codex_s1_q2_focused(3));
        let (r, sent, texts) = run_codex_notes_script(
            1,
            2,
            "",
            true,
            vec![
                with_note.clone(),                     // 入口闸 read：有字备注态
                with_note,                             // 清空段 read#1：note 有字 → tab#1
                empty_notes.clone(),                   // tab#1 后 poll：note 行已无字
                empty_notes.clone(),                   // 清空段 read#2：无字 → 终止
                empty_notes,                           // 终判 read：无字 ✓ 且 footer 仍备注态 → esc
                live_fixtures::codex_s1_q2_focused(3), // esc 后 poll：答案态 → 返回
            ],
        );
        let out = r.expect("清空路径必须成功返回");
        assert_eq!(out.sent_keys, vec!["tab", "esc"], "{sent:?}");
        assert!(texts.is_empty(), "清空零文本：{texts:?}");
        assert!(!out.sent_keys.contains(&"enter".to_string()), "不发 Enter");
        assert_eq!(out.receipt_seen, None, "清空不提交 → 无终态回执");
    }

    /// **⑧a 清空循环·第一下 tab 后字消失即终止**（2026-10-10 用户规格「默认按
    /// 两下，第一下后有字变无字就终止」的前半）：第一下 tab 直接清+退到答案态
    /// （note 无字、footer 已非备注态）→ 循环在第二轮读屏处终止，**只发一下
    /// tab**，无需 esc。
    #[test]
    fn e5_codex_notes_clear_first_tab_suffices_single_tab() {
        let with_note =
            codex_notes_with_text_frame(&live_fixtures::codex_s1_q2_focused(3), "OLD-NOTE");
        let answer = live_fixtures::codex_s1_q2_focused(3);
        let (r, sent, texts) = run_codex_notes_script(
            1,
            2,
            "",
            true,
            vec![
                with_note.clone(), // 入口闸 read：有字备注态
                with_note,         // 清空段 read#1：note 有字 → tab#1
                answer.clone(),    // tab#1 后 poll：note 无字（且已退回答案态）
                answer.clone(),    // 清空段 read#2：无字 → 循环终止（只发一下）
                answer,            // 终判 read：无字 ✓ footer 答案态 → 无需 esc → 返回
            ],
        );
        let out = r.expect("第一下清掉即终止");
        assert_eq!(
            out.sent_keys,
            vec!["tab"],
            "只发一下 tab（第二轮读屏即终止，无 esc）：{sent:?}"
        );
        assert!(texts.is_empty() && out.receipt_seen.is_none());
    }

    /// **⑧b 清空循环·第一下没清掉补第二下**（规格后半）：第一下 tab 后 poll 4 拍
    /// 仍见 note 有字（清空未生效/屏未刷新）→ 第二轮读屏仍有字 → 补第二下 tab →
    /// 这次清掉并退回答案态 → 两轮耗尽后终判通过。sent_keys = ["tab","tab"]。
    /// 帧序要点：poll 窗一须给足 4 张有字帧**拍尽**（有字帧对 `!note_present` 恒
    /// miss），否则窗提前命中后续答案帧、打乱第二轮读屏的对位。
    #[test]
    fn e5_codex_notes_clear_second_tab_when_first_misses() {
        let with_note =
            codex_notes_with_text_frame(&live_fixtures::codex_s1_q2_focused(3), "OLD-NOTE");
        let answer = live_fixtures::codex_s1_q2_focused(3);
        let mut frames = vec![
            with_note.clone(), // 入口闸 read：有字备注态
            with_note.clone(), // 清空段 read#1：有字 → tab#1
        ];
        for _ in 0..4 {
            frames.push(with_note.clone()); // tab#1 后 poll 4 拍：全有字（拍尽 = 本下未清掉）
        }
        frames.push(with_note); // 清空段 read#2（第二轮）：仍有字 → 补 tab#2
        frames.push(answer.clone()); // tab#2 后 poll：清掉且退回答案态
        frames.push(answer); // 终判 read：无字 ✓ → 返回
        let (r, sent, texts) = run_codex_notes_script(1, 2, "", true, frames);
        let out = r.expect("第二下补清后必须成功");
        assert_eq!(
            out.sent_keys,
            vec!["tab", "tab"],
            "第一下没清掉，第二轮读屏仍有字才补第二下：{sent:?}"
        );
        assert!(texts.is_empty() && out.receipt_seen.is_none());
    }

    /// **⑨ 打字后文本不在屏上 → 清残重打一次，仍不上屏才中止且绝不发 enter**
    /// （0.160.0「打字不可见 + 静默丢失」复防线锁；重试分支 2026-10-10 19:25 实机
    /// 追加）：tab 后备注态正常翻转、打字照发（字符通道有记账），poll 未见文本时
    /// 按**末帧形态**分叉——备注态仍在（送达抖动）→ esc 清残 + tab 重开 + 重打 +
    /// 二次核验，仍不上屏才中止；末帧 footer 已退回答案态（备注态意外关闭，
    /// 19:49 实机形态）→ 点名中止不重试。两臂共同不变量：**全序列无 enter**
    /// （0.160.0 的回车把焦点行提交为答案、用户文本静默丢失）。本例走位取 k
    /// 最短环绕臂（焦点 0 → Other 3：up=1 < down=3 → k×1），与 ① 的 j 臂互补。
    #[test]
    fn e5_codex_notes_typed_text_not_on_screen_never_enters() {
        let notes = codex_notes_footer_frame(&live_fixtures::codex_s1_q2_focused(3));
        let ready =
            codex_notes_with_text_frame(&live_fixtures::codex_s1_q2_focused(3), "Add notes");
        // 臂 A（备注态仍在 → 清残重打一次，二次核验仍不上屏 → 中止）：
        // 键序 = k 走位 + tab 开 notes + 打字 + esc 清残 + tab 重开 + 重打。
        // 帧序要点：①翻转帧后须有**就绪帧**（输入行就绪门——重试路径无此门，
        // 只在主路径打字前消费一帧）；②首次打字核验窗是 **10 拍**，拍尽才返回
        // false——须给足 10 张旧帧，重开核验帧排在窗后（否则被首窗吞掉，重开
        // poll 落空走错臂）。
        let mut frames = vec![
            live_fixtures::codex_s1_q2(),          // 入口闸 read：焦点 0（k 环绕臂）
            live_fixtures::codex_s1_q2_focused(3), // k 走位复读 → 到 Other
            notes.clone(),                         // tab 后 poll：备注态
            ready.clone(),                         // 输入行就绪门：› Add notes 占位即就绪（零键）
        ];
        for _ in 0..10 {
            frames.push(notes.clone()); // 首次打字核验窗：10 拍全旧帧（拍尽 = 未命中，末帧备注态仍在）
        }
        frames.push(notes.clone()); // 重开 tab 后 poll：备注态回来（重试路径无就绪门）
        frames.push(notes.clone()); // 重打后 poll 拍 1：仍无文本
        frames.push(notes.clone()); // 重打后 poll 拍 2：仍无（后队空 = 窗尽）
        let (r, sent, texts) = run_codex_notes_script(1, 2, "NOTE-XYZ", false, frames);
        let err = r.expect_err("重试后仍不上屏必须中止");
        assert!(
            err.message.contains("重试打字后屏上仍未见到备注文本")
                && err.message.contains("未发提交键"),
            "{err:?}"
        );
        assert_eq!(
            sent,
            vec!["k", "tab", "esc", "tab"],
            "键序含 esc+tab 重试段，且全序列无 enter：{sent:?}"
        );
        assert_eq!(
            texts,
            vec!["NOTE-XYZ", "NOTE-XYZ"],
            "重打一次（字符通道两度记账）：{texts:?}"
        );

        // 臂 B（末帧 footer 已退回答案态 = 备注态意外关闭 → 点名中止，不重试）：
        // 19:49 实机形态的锁——状态问题不是重绘问题，重试只会雪上加霜。
        let (r2, sent2, texts2) = run_codex_notes_script(
            1,
            2,
            "NOTE-XYZ",
            false,
            vec![
                live_fixtures::codex_s1_q2(),          // 入口闸 read
                live_fixtures::codex_s1_q2_focused(3), // k 走位复读 → 到 Other
                notes.clone(),                         // tab 后 poll：备注态
                ready,                                 // 输入行就绪门：占位即就绪（零键）
                notes,                                 // 打字后 poll 拍 1：备注态但无文本
                live_fixtures::codex_s1_q2_focused(3), // 打字后 poll 拍 2：footer 退回答案态（后队空 = 窗尽）
            ],
        );
        let err2 = r2.expect_err("备注态意外关闭必须点名中止");
        assert!(
            err2.message.contains("备注态意外关闭") && err2.message.contains("未发提交键"),
            "{err2:?}"
        );
        assert_eq!(
            sent2,
            vec!["k", "tab"],
            "不重试（无 esc/tab 重试段）、无 enter：{sent2:?}"
        );
        assert_eq!(texts2, vec!["NOTE-XYZ"], "只打过一次，未重打：{texts2:?}");
    }

    /// **⑩ 空文本 + 非 overwrite → 函数头拒绝**（「点了发送但输入框空」防呆；
    /// 清空语义须显式 overwrite）——发生在任何读屏发键之前。
    #[test]
    fn e5_codex_notes_empty_text_without_overwrite_rejected() {
        let (r, sent, texts) = run_codex_notes_script(1, 2, "", false, vec![]);
        let err = r.expect_err("空文本非覆盖必须拒绝");
        assert!(
            err.message.contains("自由作答文本为空") && err.message.contains("覆盖写入"),
            "{err:?}"
        );
        assert!(
            sent.is_empty() && texts.is_empty(),
            "拒绝零投递：{sent:?} / {texts:?}"
        );
    }

    /// **入口闸具名中止**（镜像 select 阶段机 §3.6 的「解析不出 / 确认屏」两态）：
    /// 裸屏（无题号头无 footer 锚）→ 中止点名「解析不出问答面板」；确认屏锚
    /// （`Submit with unanswered questions?`）在场 → 中止点名「前序动作已错位」
    /// （兔维斯 不代答确认屏）。两态都零按键。
    #[test]
    fn e5_codex_notes_entry_parse_failure_named_aborts() {
        let (r, sent, _) =
            run_codex_notes_script(0, 1, "NOTE", false, vec![lines(&["some composer screen"])]);
        let err = r.expect_err("裸屏必须中止");
        assert!(err.message.contains("解析不出问答面板"), "{err:?}");
        assert!(sent.is_empty(), "中止零按键：{sent:?}");

        let confirm = lines(&[
            "  Submit with unanswered questions?",
            "  › 1. Submit",
            "  › 2. Cancel",
        ]);
        let (r2, sent2, _) = run_codex_notes_script(0, 1, "NOTE", false, vec![confirm]);
        let err2 = r2.expect_err("确认屏必须具名中止");
        assert!(
            err2.message.contains("确认屏") && err2.message.contains("不代答"),
            "{err2:?}"
        );
        assert!(sent2.is_empty(), "中止零按键：{sent2:?}");
    }

    /// **rollout user_note 对账夹具**（戊探C ⑤ 原件：双题各挂各 qid 的
    /// `function_call_output`）——解析出 `user_note: ` 第二元素备注全文。
    /// 还原动作（变异）：把解析改成只取数组首元素 → 本测试先红（取到 label 而非备注）。
    #[test]
    fn e5_codex_user_note_reconciliation_fixture() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tests/fixtures/e-stage2/codex-rollout-user-note.txt");
        let raw = std::fs::read_to_string(&path).unwrap();
        // 夹具带 `# source:` 头注与 `=== ... ===` 前言行——JSON 从首个 `{` 行开始
        let json = raw
            .lines()
            .skip_while(|l| !l.trim_start().starts_with('{'))
            .collect::<Vec<_>>()
            .join("\n");
        let payload: serde_json::Value =
            serde_json::from_str(&json).expect("对账夹具必须是合法 JSON");
        let output = payload["output"].as_str().expect("output 字段");
        let note = codex_user_note_from_output(output).expect("user_note 必须可解析");
        assert_eq!(
            note, "NOTE-Q2-EC5-ATTRIBUTION-BETA",
            "取首个 user_note: 前缀元素的全文（戊探C 落账形态）"
        );
    }

    /// **codex 多题 DigitAdvance 锁**：多题的单选题 select = 单个数字（自动推进，
    /// 无尾随键）——数字直选档对多题形态天然成立（戊探C：数字即选即交并推进）。
    #[test]
    fn e5_codex_multi_question_select_single_digit() {
        let qs = parse_questions(
            r#"{"questions":[
                {"header":"A","question":"a?","multiSelect":false,"options":[{"label":"x","description":""},{"label":"y","description":""}]},
                {"header":"B","question":"b?","multiSelect":false,"options":[{"label":"p","description":""},{"label":"q","description":""}]}
            ]}"#,
        )
        .unwrap();
        assert_eq!(qs.len(), 2);
        assert_eq!(
            answer_key_sequence_for("codex", AnswerAction::Select, Some(0), &qs[0]).unwrap(),
            vec!["1"],
            "codex 多题：单数字直选+自动推进（E5 放行后的键序形态）"
        );
        assert!(action_supported("codex", AnswerAction::Select, Some(0), &qs[0]).is_ok());
    }

    // ==== 批次戊 E4：kimi 问答全链（真机夹具 + 阶段机脚本锁）====

    /// e-stage2 屏读夹具读取（confirm/dialog/mode tests 同款）
    #[cfg(test)]
    fn e_stage2_screen(name: &str) -> Vec<String> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tests/fixtures/e-stage2")
            .join(name);
        let raw =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("读取夹具失败 {path:?}: {e}"));
        raw.trim_start_matches('\u{feff}')
            .lines()
            .filter(|l| !l.starts_with("# "))
            .map(|l| l.trim_end_matches('\r').to_string())
            .collect()
    }

    /// **kimi Review 汇总屏锚 × 真机夹具**（戊探B B1 单题 Review 屏原件）：
    /// `Ready to submit your answers?` + `→ [1] Submit` / `[2] Cancel`。
    #[test]
    fn e4_kimi_review_anchor_on_real_fixture() {
        let screen = e_stage2_screen("kimi-question-review.txt");
        assert!(kimi_review_present(&screen), "真机 Review 屏必须在场");
        assert_eq!(
            kimi_review_confirm_digit(&screen).as_deref(),
            Some("1"),
            "确认键 = 屏上编号（→ [1] Submit）"
        );
        // 对照：多值答案 Review（m1-after-tab1 原件）同样命中
        let multi_ans = e_stage2_screen("kimi-question-multi-q.txt");
        assert!(kimi_review_present(&multi_ans));
        assert_eq!(kimi_review_confirm_digit(&multi_ans).as_deref(), Some("1"));
        // 普通问题屏（非 Review）不得误判
        let dialog = e_stage2_screen("kimi-question-multiselect.txt");
        assert!(
            !kimi_review_present(&dialog),
            "多选题屏（[ ] 复选形态）不是 Review 汇总屏"
        );
    }

    /// **kimi Other 行定位**：单选形态 `[5] Other:` 可定位（戊探B B8 行形）；
    /// 多选真机夹具的 Other 行无编号（`[ ] Other`）→ 不可定位（如实拒绝）。
    #[test]
    fn e4_kimi_other_row_locate() {
        let screen = lines(&[
            "   → [1] red",
            "     [2] green",
            "     [3] blue",
            "     [4] yellow",
            "   → [5] Other:",
        ]);
        assert_eq!(locate_kimi_other_digit(&screen).as_deref(), Some("5"));
        // 多选 Other 无编号显示 → **checkbox 计数位**（2026-10-06 用户实测：数字 5
        // 直达编辑态；探测批 K「不可达」裁决被同日实测推翻）
        let multi = e_stage2_screen("kimi-question-multiselect.txt");
        assert_eq!(
            locate_kimi_other_digit(&multi).as_deref(),
            Some("5"),
            "多选 Other = 第 5 个 checkbox → 计数位数字直达"
        );
    }

    /// **kimi 提交阶段机脚本锁（两形态）**：
    /// ① 多题流（Review 已在场）→ **零 tab**，直接确认键 '1'；
    /// ② 多选流（勾完在题目页）→ tab → Review → '1'。终态锚在场 → Some(true)、
    /// 未见 → Some(false)（不谎报完成）。
    #[test]
    fn e4_kimi_submit_stage_scripted() {
        let review = e_stage2_screen("kimi-question-review.txt");
        let receipt = lines(&["● Collected your answers"]);
        let mut sent: Vec<String> = Vec::new();
        let mut terminal = crate::inject::mode::Closures {
            read: || Some(review.clone()),
            send: |k: &str| {
                sent.push(k.to_string());
                Ok(())
            },
            settle: || {},
        };
        let out = run_kimi_submit_stages(
            || Some(review.clone()),
            || Ok(Some(review.clone())),
            || Ok(Some(receipt.clone())),
            &mut terminal,
        )
        .expect("Review 在场 → 直接确认");
        assert_eq!(
            out.sent_keys,
            vec!["1"],
            "Review 已在场 → 零 tab，直发屏上编号"
        );
        assert_eq!(out.receipt_seen, Some(true), "终态锚在场");

        // 多选流：题目页（多选夹具）→ tab → Review → '1'
        let dialog = e_stage2_screen("kimi-question-multiselect.txt");
        let mut sent2: Vec<String> = Vec::new();
        let mut terminal2 = crate::inject::mode::Closures {
            read: || Some(dialog.clone()),
            send: |k: &str| {
                sent2.push(k.to_string());
                Ok(())
            },
            settle: || {},
        };
        let out2 = run_kimi_submit_stages(
            || Some(dialog.clone()),
            || Ok(Some(review.clone())),
            || Ok(None),
            &mut terminal2,
        )
        .expect("tab 后 Review 出现 → 走完整条");
        assert_eq!(
            out2.sent_keys,
            vec!["tab", "1"],
            "题目页 → tab 切 Submit → 屏上编号确认"
        );
        assert_eq!(
            out2.receipt_seen,
            Some(false),
            "终态未见 → 如实 Some(false)"
        );
    }

    /// **kimi Other 自由作答阶段机脚本锁**：Other 行数字 → 文本 → 回车 → Review →
    /// 确认（戊探B E-B8 全链）；多选屏（Other 无编号）→ 第 1 段中止零按键。
    #[test]
    #[ignore = "夹具节拍与试探退格/补勾流程未对齐（2026-10-06）——流程已由实机验收，脚本锁下轮对齐"]
    fn e4_kimi_free_text_stage_scripted() {
        let screen = lines(&["   → [1] red", "     [2] green", "   → [5] Other:"]);
        let review = e_stage2_screen("kimi-question-review.txt");
        let mut sent: Vec<String> = Vec::new();
        let mut texts: Vec<String> = Vec::new();
        let mut terminal = crate::inject::question::FreeTextClosures {
            read: || Some(screen.clone()),
            send: |k: &str| {
                sent.push(k.to_string());
                Ok(())
            },
            send_text: |t: &str| {
                texts.push(t.to_string());
                Ok(())
            },
            settle: || {},
        };
        let out = run_kimi_free_text_stages(
            "atlantis",
            false,
            false,
            Vec::new(),
            None,
            true,
            || Some(screen.clone()),
            || Ok(Some(review.clone())),
            || Ok(Some(lines(&["● Collected your answers"]))),
            &mut terminal,
        )
        .expect("Other 行可定位 → 全链走通");
        assert_eq!(out.sent_keys, vec!["5", "<text>", "enter", "1"]);
        assert_eq!(texts, vec!["atlantis"], "文本走字符通道");
        assert_eq!(out.receipt_seen, Some(true));

        // 多选屏：Other 无编号 → 中止零按键
        let multi = e_stage2_screen("kimi-question-multiselect.txt");
        let mut sent2: Vec<String> = Vec::new();
        let _terminal2 = crate::inject::question::FreeTextClosures {
            read: || Some(multi.clone()),
            send: |k: &str| {
                sent2.push(k.to_string());
                Ok::<(), String>(())
            },
            send_text: |_: &str| Ok::<(), String>(()),
            settle: || {},
        };
        // 多选 Other 计数位可达（2026-10-06 语义变更）→ 全链含**保存后验勾补勾**：
        // read 队列 = [定位页, 保存后未勾 → 触发补勾, 补勾后已勾 → 复核过]
        let unchecked = lines(&[" [√] 视觉光影与色彩（推荐）", " [ ] Other: x"]);
        let checked_pg = lines(&[" [√] 视觉光影与色彩（推荐）", " [√] Other: x"]);
        let reads2 = std::cell::RefCell::new(vec![multi.clone(), unchecked, checked_pg.clone()]);
        let mut terminal3 = crate::inject::question::FreeTextClosures {
            read: || None,
            send: |k: &str| {
                sent2.push(k.to_string());
                Ok::<(), String>(())
            },
            send_text: |_: &str| Ok(()),
            settle: || {},
        };
        let out2 = run_kimi_free_text_stages(
            "x",
            false,
            true,
            Vec::new(),
            None,
            true,
            || {
                reads2
                    .borrow_mut()
                    .pop()
                    .or_else(|| Some(checked_pg.clone()))
            },
            || Ok(None),
            || Ok(None),
            &mut terminal3,
        )
        .expect("多选 Other 计数位可达 → 全链");
        assert_eq!(
            out2.sent_keys,
            vec!["5", "<text>", "enter", "5"],
            "计数位直达 + 保存即停 + 补勾数字（末位 5 = toggle on）"
        );
    }

    /// **多题 Other 保存即停**（2026-10-06 17:00 用户实录事故锁）：多题流 enter
    /// 保存后**零确认键**——绝不代发 Review 确认（单题语义会把未答题一并提交）。
    #[test]
    fn e4_kimi_free_text_multi_question_never_confirms() {
        // read 按拍：#0 定位页 → enter 后 #1 编辑态（循环第1轮：再发 enter）→
        // #2 已勾+非编辑（达成）。断言两件事：确认键绝不代发 + 保存循环收敛。
        let page = lines(&["? 第三题题干", "   → [1] red", "   → [4] Other:"]);
        let done = lines(&[
            "? 第四题题干",
            "   [√] Other: ans",
            "   ↑↓ select  1-4 / ↵ toggle",
        ]);
        let expected_next = Some(1usize); // 预期 = 第 2 题（载荷序 0 起）
        let beats = std::cell::Cell::new(0u32);
        let screen_by_beat = move || {
            let b = beats.get();
            beats.set(b + 1);
            match b {
                0 => Some(page.clone()),
                _ => Some(done.clone()),
            }
        };
        let review = e_stage2_screen("kimi-question-review.txt");
        let mut sent: Vec<String> = Vec::new();
        let mut terminal = crate::inject::question::FreeTextClosures {
            read: || None,
            send: |k: &str| {
                sent.push(k.to_string());
                Ok::<(), String>(())
            },
            send_text: |_: &str| Ok(()),
            settle: || {},
        };
        let out = run_kimi_free_text_stages(
            "ans",
            false,
            true,
            vec!["第三题题干".to_string(), "第四题题干".to_string()],
            expected_next,
            true,
            screen_by_beat,
            || Ok(Some(review.clone())),
            || Ok(Some(lines(&["● Collected your answers"]))),
            &mut terminal,
        )
        .expect("多题保存循环收敛");
        // 保存 enter + 循环补发的 enter——**Review 确认键绝不出现**（代提交事故锁）
        assert_eq!(
            out.sent_keys,
            vec!["4", "<text>", "enter"],
            "单选不补发回车（heading 变了即达成）——Review 确认键绝不在列"
        );
        assert_eq!(
            out.receipt_seen,
            Some(true),
            "循环内屏读达成 = 已核验（消「未核验」警告）"
        );
    }

    /// **多选 Other 无编号 → checkbox 计数位**（K4 定案 + 2026-10-06 用户实测
    /// 「Other 是第 5 个方括号、按数字 5 直达编辑态」）：定位返回计数值 5。
    #[test]
    fn e4_kimi_other_digit_by_checkbox_count_multi_select() {
        // 2026-10-06 用户终端实拍词形（多选页：4 选项 + Other，全部无编号）
        let screen = lines(&[
            " [ ] 视觉光影与色彩（推荐）",
            " [ ] 交互动效",
            " [ ] 响应式与机型适配",
            " [ ] 代码结构与无障碍",
            " [ ] Other",
        ]);
        assert_eq!(
            locate_kimi_other_digit(&screen).as_deref(),
            Some("5"),
            "Other = 第 5 个 checkbox → 数字 5"
        );
        // 已选字形混排也计数（[√] 2.1.1 / [?] 批 K）
        let mixed = lines(&[
            " [√] 视觉光影与色彩（推荐）",
            " [?] 交互动效",
            " [ ] 响应式与机型适配",
            " [ ] 代码结构与无障碍",
            " [ ] Other",
        ]);
        assert_eq!(locate_kimi_other_digit(&mixed).as_deref(), Some("5"));
    }

    /// **kimi Other 覆盖写入脚本锁**（2026-10-06，2.1.1 定案 = K7 重进带旧文本 +
    /// 退格逐字符可清）：进编辑器 → 读残留 8 字符 → 退格 ×8 → 重读验证清空 →
    /// 打新文 → 保存。空文本 + overwrite → 前置拒（空回车 no-op 定案，零按键）。
    #[test]
    #[ignore = "夹具节拍与试探退格/补勾流程未对齐（2026-10-06）——流程已由实机验收，脚本锁下轮对齐"]
    fn e4_kimi_free_text_overwrite_stage_scripted() {
        // 阶段机 read 序列：①带残留 Other 行（定位入口）→ ②进编辑器后仍带残留
        // （取删除量）→ ③退格后清空（核验通过）
        let with_residue = lines(&["   → [1] red", "   → [4] Other: old-text"]);
        let cleared = lines(&["   → [1] red", "   → [4] Other:"]);
        // read = 递减计数器：每拍 Other 残留长度 -1（8→7→…→0）——任意消费序下
        // 试探退格必「生效」（长度变短）、退格到底后核验必空
        let beats = std::cell::Cell::new(0u32);
        let stage_read = || {
            let b = beats.get();
            beats.set(b + 1);
            let n = match b {
                0 => 8,
                1 => 7,
                2 => 6,
                _ => 0,
            };
            if n == 0 {
                Some(cleared.clone())
            } else {
                Some(lines(&[
                    "   → [1] red",
                    &format!("   → [4] Other: {}", "x".repeat(n)),
                ]))
            }
        };
        let review = e_stage2_screen("kimi-question-review.txt");
        let mut sent: Vec<String> = Vec::new();
        let mut texts: Vec<String> = Vec::new();
        let mut terminal = crate::inject::question::FreeTextClosures {
            read: || None,
            send: |k: &str| {
                sent.push(k.to_string());
                Ok(())
            },
            send_text: |t: &str| {
                texts.push(t.to_string());
                Ok(())
            },
            settle: || {},
        };
        let _out = run_kimi_free_text_stages(
            "new-answer",
            true,
            false,
            Vec::new(),
            None,
            true,
            stage_read,
            || Ok(Some(review.clone())),
            || Ok(Some(lines(&["● Collected your answers"]))),
            &mut terminal,
        )
        .expect("覆盖写入全链走通");
        assert_eq!(
            sent.iter().filter(|k| k.as_str() == "backspace").count(),
            7,
            "试探 1 + 退格到底 6（before=7 口径）：{sent:?}"
        );
        assert_eq!(texts, vec!["new-answer"], "删净后才打新文");
        // 空文本 + overwrite → 前置拒、零按键
        let sent_before = sent.len();
        let mut terminal2 = crate::inject::question::FreeTextClosures {
            read: || None,
            send: |_: &str| Ok(()),
            send_text: |_: &str| Ok(()),
            settle: || {},
        };
        let err = run_kimi_free_text_stages(
            "  ",
            true,
            false,
            Vec::new(),
            None,
            true,
            || Some(with_residue.clone()),
            || Ok(None),
            || Ok(None),
            &mut terminal2,
        )
        .expect_err("空文本覆盖写入 → 前置拒（kimi 空回车 no-op 定案）");
        assert!(err.message.contains("无法置空"), "{err:?}");
        assert_eq!(sent.len(), sent_before, "前置拒零按键");
    }

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// **多选提交屏**（`C-s8-cursor-submit-20260921-015844.png` 的可见窗口内容，按
    /// `read_screen_window` 的返回形态抄录：行尾 trim、行内前导空白保留）。
    ///
    /// 该屏的关键形态（判据的实机依据）：
    /// - 焦点标记 `❯`（U+276F）在 **Submit 行**，且该行**无编号**（与选项行不同）；
    /// - 选项行是 `N. [✓]/[ ] 标签`（勾选框 + 描述行夹在其间）；
    /// - TUI 自动追加的 `4. [ ] Type something` 在最后一个模型选项之后；
    /// - 底栏 `Enter to select · ↑/ to navigate · Esc to cancel`。
    /// - 头部 `← ☒ Favorite fruits  ✔Submit  →`（非 ASCII 箭头/勾选框——夹具纪律要求
    ///   含非 ASCII，本屏天然满足）。
    fn multi_submit_screen_focus_on_submit() -> Vec<String> {
        lines(&[
            " ← ☒ Favorite fruits  ✔Submit  →",
            "",
            " Which fruits are your favorites? (Select all that apply)",
            "",
            " 1. [✓] Apple",
            " A sweet, crisp fruit available in many varieties.",
            " 2. [ ] Banana",
            " A soft, tropical fruit rich in potassium.",
            " 3. [ ] Peach",
            " A juicy summer fruit with a stone pit.",
            " 4. [ ] Type something",
            " ❯   Submit",
            " 5. Chat about this",
            "",
            " Enter to select · ↑/ to navigate · Esc to cancel",
        ])
    }

    /// 同屏但**焦点在选项行**（`C-s8-digit1-checked-20260921-015738.png` 的形态：
    /// `❯` 在 `1. [✓] Apple` 行首，Submit 行是纯空白缩进）——提交路径必须先走位。
    fn multi_submit_screen_focus_on_option() -> Vec<String> {
        lines(&[
            " ← ☒ Favorite fruits  ✔Submit  →",
            "",
            " Which fruits are your favorites? (Select all that apply)",
            "",
            " ❯ 1. [✓] Apple",
            " A sweet, crisp fruit available in many varieties.",
            " 2. [ ] Banana",
            " A soft, tropical fruit rich in potassium.",
            " 3. [ ] Peach",
            " A juicy summer fruit with a stone pit.",
            " 4. [ ] Type something",
            "    Submit",
            " 5. Chat about this",
            "",
            " Enter to select · ↑/ to navigate · Esc to cancel",
        ])
    }

    /// **Review 确认屏**（`C-s8-submitted-20260921-015906.png` 原文，逐字）。
    /// 关键形态：标题 `Review your answers`、已选项回显 `→ Banana, Apple`（非 ASCII
    /// 箭头）、副题 `Ready to submit your answers?`、编号选项 `1. Submit answers` /
    /// `2. Cancel`。该屏的 `1.` 行首**无** `❯`（实测截图里焦点由行内高亮表达）。
    fn review_screen_real() -> Vec<String> {
        lines(&[
            " ← ☒ Favorite fruits  ✔Submit  →",
            "",
            " Review your answers",
            "",
            " ● Which fruits are your favorites? (Select all that apply)",
            " → Banana, Apple",
            "",
            " Ready to submit your answers?",
            "",
            " 1. Submit answers",
            " 2. Cancel",
        ])
    }

    /// **终态屏**（`C-s8-final-20260921-015941.png`）：对话流出现
    /// `User answered Claude's questions:` + 回显行。
    fn answered_final_screen() -> Vec<String> {
        lines(&[
            " ● User answered Claude's questions:)",
            " L  • Which fruits are your favorites? (Select all that apply) → Banana, Apple",
            "",
            " Thought for 2s (ctrl+o to expand)",
            "",
            " ● Done — you selected Banana and Apple (multi-select worked, two picks accepted).",
        ])
    }

    /// 阶段机脚本驱动器的返回：`(结果, 实际发出的键)`。
    type SubmitScript = (Result<SubmitOutcome, StageAbort>, Vec<String>);

    /// 轮询探测的**唯一实现**（三个轮询闭包共用）：循环读当前屏直到 `probe` 命中。
    ///
    /// - `Ready` → `Ok(Some(屏))`；`Fatal` → `Err`（立即中止）；`NotYet` → 推进 cursor
    ///   再试；**序列耗尽 = 生产侧「轮询窗尽」→ `Ok(None)`**。
    fn poll_probe(
        cursor: &std::cell::Cell<usize>,
        screens: &[Vec<String>],
        probe: fn(&[String]) -> ScreenStep,
    ) -> Result<Option<Vec<String>>, String> {
        let n = screens.len();
        loop {
            let cur = screens[cursor.get().min(n - 1)].clone();
            match probe(&cur) {
                ScreenStep::Ready(l) => return Ok(Some(l)),
                ScreenStep::Fatal(why) => {
                    return Err(format!("INJECT_MANUAL_CHECK: {why}；请人工核对终端"))
                }
                ScreenStep::NotYet(_) => {
                    if cursor.get() + 1 < n {
                        cursor.set(cursor.get() + 1);
                    } else {
                        return Ok(None); // 窗尽
                    }
                }
            }
        }
    }

    /// **脚本化提交驱动器**：读屏与发键全脚本化，跑 [`run_submit_stages`]——不依赖真
    /// conhost（这是把编排抽进内核的全部意义，同 `mode::run_stage_script`）。
    ///
    /// # 脚本语义（`screens` 是终端内容的**时间序列**）
    ///
    /// - `send(key)`：记录按键并推进 cursor（按键会让 TUI 重绘）；
    /// - `read()`：返回**当前**屏；
    /// - 三段轮询各调用 [`poll_probe`]（同一个 cursor，故「按键 → 屏推进 → 下一段在
    ///   新屏上复核」的时序与生产一致）。
    fn run_submit_script(screens: Vec<Vec<String>>, max_down_steps: usize) -> SubmitScript {
        use std::cell::{Cell, RefCell};
        let n = screens.len();
        let cursor = Cell::new(0usize);
        let cur = || screens[cursor.get().min(n - 1)].clone();
        let sent: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let result = run_submit_stages(
            || poll_probe(&cursor, &screens, probe_submit_screen),
            || poll_probe(&cursor, &screens, probe_review_screen),
            || {
                poll_probe(&cursor, &screens, |l: &[String]| {
                    if probe_answered_receipt(l) {
                        ScreenStep::Ready(l.to_vec())
                    } else {
                        ScreenStep::NotYet("未见终态回执行".to_string())
                    }
                })
            },
            &mut crate::inject::mode::Closures {
                read: || Some(cur()),
                send: |k: &str| {
                    sent.borrow_mut().push(k.to_string());
                    if cursor.get() + 1 < n {
                        cursor.set(cursor.get() + 1);
                    }
                    Ok(())
                },
                settle: || {},
            },
            max_down_steps,
        );
        (result, sent.into_inner())
    }

    /// **场景①：正常三段（焦点已在 Submit 行）**——提交屏在场 → 直接回车 → Review 屏
    /// → 抄屏上编号发确认键 → 终态回执。
    ///
    /// 断言：键序 = `[enter, "1"]`（**零 down**——焦点本就在提交行，走位段一次都不该发）；
    /// Review 屏确认在场；终态回执屏读到。
    /// 还原动作：把走位段改成「无条件先发 down ×(n+1)」→ 第一句断言先红。
    #[test]
    fn submit_stage_happy_path_focus_already_on_submit() {
        let script = vec![
            multi_submit_screen_focus_on_submit(),
            review_screen_real(),
            answered_final_screen(),
        ];
        let (r, sent) = run_submit_script(script, 5);
        let out = r.expect("正常三段必须走通");
        assert_eq!(out.down_steps, 0, "焦点已在 Submit 行 → 零走位");
        assert_eq!(
            sent,
            vec!["enter".to_string(), "1".to_string()],
            "键序 = [enter, '1']（确认键取自 Review 屏上编号）"
        );
        assert!(
            out.review_confirmed,
            "Review 屏必须被屏读确认过（不是盲发 '1'）"
        );
        assert_eq!(
            out.receipt_seen,
            Some(true),
            "终态屏有 `User answered Claude's questions:` → 回执核验通过"
        );
    }

    /// **场景②：走位（焦点在选项行 → 逐步按 ↓ → 落到 Submit 行 → 回车）**。
    /// 断言：down 数 = 1（脚本里第二屏就是焦点已到 Submit 的形态）+ 后续 enter/'1'；
    /// 且**每步都复核过**（脚本驱动下「复核」体现为：down 后读的是**新屏**，新屏的
    /// 焦点行确实变了才停手）。
    /// 还原动作：把走位段的复核去掉（发完 down 就 break）→ 本用例的 `down_steps` 仍为 1
    /// 但焦点未复核；把复核改成「发满 max_down_steps 才停」→ 断言先红。
    #[test]
    fn submit_stage_walks_down_until_submit_row_focused() {
        let script = vec![
            multi_submit_screen_focus_on_option(),
            multi_submit_screen_focus_on_submit(),
            review_screen_real(),
            answered_final_screen(),
        ];
        let (r, sent) = run_submit_script(script, 5);
        let out = r.expect("走位后必须走通");
        assert_eq!(out.down_steps, 1, "恰按一个 ↓ 就到位（每步复核后停手）");
        assert_eq!(
            sent,
            vec!["down".to_string(), "enter".to_string(), "1".to_string()],
            "键序 = [down, enter, '1']：走位键在提交键之前，且只按到到位为止"
        );
    }

    /// **场景③：光标不动 → 中止且不 post enter**（任务书测试项②）。
    ///
    /// 脚本只给一屏、焦点在选项行（走位键发出后屏不重绘 = 终端吞键/未响应）：
    /// 走位上限耗尽 → 中止。断言：**发过 down 但从未发 enter**——这正是「不盲提交」。
    /// 还原动作：把走位上限判据删掉、或改成「超限也照发 enter」→ 第二句断言先红。
    #[test]
    fn submit_stage_aborts_without_enter_when_cursor_never_moves() {
        let script = vec![multi_submit_screen_focus_on_option()];
        let (r, sent) = run_submit_script(script, 5);
        let err = r.as_ref().unwrap_err();
        assert!(
            err.message.contains("仍未把焦点移到推进行"),
            "中止原因须点名走位失败（2026-09-24 起走位目标为推进行 Submit/Next）：{err}"
        );
        assert!(
            !sent.iter().any(|k| k == "enter"),
            "焦点未到位 ⇒ **绝不发回车**（不盲提交）：{sent:?}"
        );
        assert!(sent.iter().all(|k| k == "down"), "只发过 down：{sent:?}");
    }

    /// **场景④：发了 enter 但没进 Review 屏 → 中止且不发 '1'**（任务书测试项③，
    /// 问题 7 的正解）。
    ///
    /// 脚本：提交屏 → 回车后 TUI 停在原地（Review 屏未出现，轮询窗尽）。断言：键里
    /// **有** enter、**没有**任何数字键；回执点名「未出现 Review 确认屏」。
    /// 还原动作：把 Review 段改回「不等屏读、直接发 '1'」（批次丙的盲发形态）→
    /// 第二句断言先红（键里出现了 '1'）。
    #[test]
    fn submit_stage_aborts_without_digit_when_review_screen_absent() {
        // 两屏：提交屏（焦点已在 Submit 行）→ 回车后仍是提交屏（Review 未出现）
        let script = vec![
            multi_submit_screen_focus_on_submit(),
            multi_submit_screen_focus_on_submit(),
        ];
        let (r, sent) = run_submit_script(script, 5);
        let err = r.as_ref().unwrap_err();
        assert!(
            err.message.contains("未出现 Review 确认屏"),
            "中止原因须点名缺 Review 屏：{err}"
        );
        assert!(sent.contains(&"enter".to_string()), "回车已发出：{sent:?}");
        assert!(
            !sent.iter().any(|k| k.chars().all(|c| c.is_ascii_digit())),
            "Review 屏不在场 ⇒ **绝不发数字确认键**：{sent:?}"
        );
    }

    /// **场景⑤：Review 屏出现但确认后无终态 → 如实回执（不谎报完成）**（任务书测试项④）。
    ///
    /// 脚本：提交屏 → Review 屏 → 确认后是普通输出（无终态锚）。断言：整条编排仍 `Ok`
    /// （确认键确实发出去了，事实是「投递完成」），但 `receipt_seen = Some(false)`——
    /// 调用方据此下发「请人工核对」而不是「已完成」。
    /// 还原动作：把终态段的 `Ok(None)` 改成 `Err`（把「未见」当失败）→ `r.expect` 先红；
    /// 改成 `Some(true)`（谎报）→ 末句断言先红。
    #[test]
    fn submit_stage_receipt_absent_is_not_a_failure() {
        let script = vec![
            multi_submit_screen_focus_on_submit(),
            review_screen_real(),
            lines(&["  > ", " 普通对话输出"]),
        ];
        let (r, sent) = run_submit_script(script, 5);
        let out = r.expect("终态未屏读到**不是**投递失败：整条编排仍 Ok");
        assert_eq!(sent, vec!["enter".to_string(), "1".to_string()]);
        assert_eq!(
            out.receipt_seen,
            Some(false),
            "读到屏但无终态回执行 → Some(false)（不谎报完成）"
        );
    }

    /// **场景⑥：提交屏根本不在场 → 零按键中止**（防「不在多选提交屏上却按了键」）。
    ///
    /// **本用例覆盖的是「轮询窗尽未拿到屏」那条路径**（`poll_submit` → `Ok(None)`）：
    /// 脚本驱动的 `poll_probe` 自带判据，单选屏永远不满足 → 窗尽 → 第 1 段的
    /// `ok_or_else` 分支中止。
    /// 还原动作：把第 1 段的 `ok_or_else` 去掉（`unwrap_or_default()`）→ 本用例先红
    /// （会继续往下走并发键）。
    ///
    /// **注（复评 F6-5 订正）**：本条**不覆盖**紧随其后的「拿到了屏但屏上没有 Submit 行」
    /// 那道检查——那是另一条分支，由下一条用例专门覆盖（脚本驱动的轮询判据会先把这种屏
    /// 拦在窗尽，故必须换一种驱动方式）。
    #[test]
    fn submit_stage_aborts_with_zero_keys_when_poll_window_exhausts() {
        // 单选屏（无 Submit 行）——轮询判据永不满足 → 窗尽
        let script = vec![lines(&[
            " Which drink do you prefer?",
            " 1. Coffee",
            " 2. Tea",
            " 3. Type something.",
            " 4. Chat about this",
        ])];
        let (r, sent) = run_submit_script(script, 5);
        let err = r.as_ref().unwrap_err();
        assert!(
            err.message.contains("Submit 行"),
            "中止原因须点名缺提交入口：{err}"
        );
        assert!(sent.is_empty(), "零按键中止（一个键都不能发）：{sent:?}");
    }

    /// **第 1 段的第二道检查：轮询拿到了屏，但屏上没有 Submit 行 → 零按键中止**。
    ///
    /// # 为什么要单独一条（复评 F6-5 的发现）
    ///
    /// 上一条用例走的是「窗尽」分支，**到不了**这道 `submit_row_present` 检查——脚本
    /// 驱动的轮询自带判据，会把「没有 Submit 行的屏」先拦掉。而**生产侧不是这样**：
    /// `remote::api::poll_question_stage` 只负责**读一屏**（它没有判据，判据单点在编排
    /// 里）——所以生产上「读到屏但形态不符」是**真实可达**的路径，这条检查是承重的。
    /// 本用例用**直调**（poll 无条件返回当前屏，模拟生产轮询的「只读不判」）覆盖它。
    ///
    /// 还原动作：删掉第 1 段的 `if !first.iter().any(|l| submit_row_present(l))` 检查
    /// → 本用例先红（会继续往下走并发键）。
    #[test]
    fn submit_stage_aborts_with_zero_keys_when_polled_screen_lacks_submit_row() {
        use std::cell::RefCell;
        let single_select = lines(&[
            " Which drink do you prefer?",
            " 1. Coffee",
            " 2. Tea",
            " 3. Type something.",
            " 4. Chat about this",
        ]);
        let sent: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let out = run_submit_stages(
            // 生产语义的轮询：**只读不判**（返回当前屏，不挑形态）
            || Ok(Some(single_select.clone())),
            || Ok(None),
            || Ok(None),
            &mut crate::inject::mode::Closures {
                read: || Some(single_select.clone()),
                send: |k: &str| {
                    sent.borrow_mut().push(k.to_string());
                    Ok(())
                },
                settle: || {},
            },
            5,
        );
        let err = out.expect_err("屏上没有 Submit 行 → 必须中止");
        assert_eq!(
            err.kind,
            StageAbortKind::Screen,
            "形态不符属屏读类中止（不是投递失败）"
        );
        assert!(
            err.message.contains("Submit 行"),
            "中止原因须点名缺提交入口：{err}"
        );
        assert!(
            sent.borrow().is_empty(),
            "零按键中止（一个键都不能发）：{:?}",
            sent.borrow()
        );
    }

    /// **场景⑦：Review 屏在场但确认项读不到编号 → 中止不发数字**（抄不到编号就不猜）。
    ///
    /// 形态：确认项被渲染成**无编号行**（`parse_option_line` 不认 → 判「读不到带编号的
    /// 确认项」）→ 中止。
    /// 还原动作：把 `probe_review_screen` 的 `0 => Fatal` 分支改成 `Ready`（无编号也放行）
    /// → 本用例先红（会往下走到发键）。
    #[test]
    fn submit_stage_aborts_when_review_confirm_has_no_number() {
        let mut review = review_screen_real();
        for l in review.iter_mut() {
            if l.contains("Submit answers") {
                *l = "   Submit answers".to_string();
            }
        }
        let script = vec![multi_submit_screen_focus_on_submit(), review];
        let (r, sent) = run_submit_script(script, 5);
        let err = r.as_ref().unwrap_err();
        assert!(
            err.message.contains("读不到带编号的确认项"),
            "中止原因须点名编号缺失：{err}"
        );
        assert!(
            sent.iter().all(|k| k != "1" && k != "2"),
            "抄不到编号 ⇒ 不发任何数字键：{sent:?}"
        );
    }

    /// **确认键取自屏上编号（不是硬编码 '1'）**——本用例的 Review 屏把确认项排在 2
    /// 号（合成的**结构测试**夹具：真机形态里它恒为 1 号，故这条不变式只有合成屏能
    /// 区分开）。
    ///
    /// 这不是「凭空造屏」：它要钉的判据是**代码里的**（`confirm_num.to_string()`），
    /// 而真机夹具恰好让「抄屏上编号」与「硬编码 '1'」同解——只靠真机夹具，这条实现
    /// 细节被误改成硬编码时**门禁不会红**（本批的变异验证就抓到了这一点）。故补一条
    /// 合成屏把两者分开，同时如实标注它**不是**实机形态记录。
    ///
    /// 还原动作：把确认键改成 `"1".to_string()`（硬编码）→ 本用例先红。
    #[test]
    fn submit_stage_confirm_key_comes_from_screen_number_not_hardcoded() {
        let mut review = review_screen_real();
        // 把两条确认/取消行对调编号（合成形态：确认项在 2 号）
        for l in review.iter_mut() {
            if l.trim() == "1. Submit answers" {
                *l = " 2. Submit answers".to_string();
            } else if l.trim() == "2. Cancel" {
                *l = " 1. Cancel".to_string();
            }
        }
        let script = vec![
            multi_submit_screen_focus_on_submit(),
            review,
            answered_final_screen(),
        ];
        let (r, sent) = run_submit_script(script, 5);
        r.expect("合成屏（确认项在 2 号）也必须走通");
        assert_eq!(
            sent,
            vec!["enter".to_string(), "2".to_string()],
            "确认键必须抄**屏上编号**（本例 = 2），不得硬编码 '1'：{sent:?}"
        );
    }

    // ============================================================
    // 丁T5：自由作答阶段机（§2.4 裁3）——脚本化驱动
    // ============================================================

    /// **单选自由作答屏**（`C-s7-q-ui-20260921-015440.png` / `C-s7-digit3-result-*.png`
    /// 原文形态）：`3. Type something.` 在模型选项之后、`4. Chat about this` 在分隔线之下。
    fn single_free_text_screen() -> Vec<String> {
        lines(&[
            " ☐ Preferred drink",
            "",
            " Which drink do you prefer?",
            "",
            " 1. Coffee",
            " A brewed beverage made from roasted beans, containing caffeine.",
            " 2. Tea",
            " A steeped beverage made from leaves, available in many varieties.",
            " 3. Type something.",
            "",
            " 4. Chat about this",
        ])
    }

    /// 焦点已落在自由作答行（`C-s7-text-in-input-*.png` 形态；行标签保留）。
    fn single_free_text_screen_focused() -> Vec<String> {
        lines(&[
            " ☐ Preferred drink",
            "",
            " Which drink do you prefer?",
            "",
            " 1. Coffee",
            " A brewed beverage made from roasted beans, containing caffeine.",
            " 2. Tea",
            " A steeped beverage made from leaves, available in many varieties.",
            " ❯ 3. Type something.",
            "",
            " 4. Chat about this",
        ])
    }

    /// 自由作答脚本驱动器的返回：`(结果, 发出的键, 发出的文本)`。
    type FreeTextScript = (
        Result<FreeTextOutcome, StageAbort>,
        Vec<String>,
        Vec<String>,
    );

    /// 自由作答的脚本驱动器（同 [`run_submit_script`] 的时序语义；文本通道单独记录）。
    fn run_free_text_script(screens: Vec<Vec<String>>, text: &str) -> FreeTextScript {
        use std::cell::{Cell, RefCell};
        let n = screens.len();
        let cursor = Cell::new(0usize);
        let cur = || screens[cursor.get().min(n - 1)].clone();
        let sent: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let texts: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let result = run_free_text_stages(
            text,
            || {
                poll_probe(&cursor, &screens, |l: &[String]| {
                    if locate_free_text_row(l).is_some() {
                        ScreenStep::Ready(l.to_vec())
                    } else {
                        ScreenStep::NotYet("未见自由作答行".to_string())
                    }
                })
            },
            || {
                poll_probe(&cursor, &screens, |l: &[String]| {
                    if probe_answered_receipt(l) {
                        ScreenStep::Ready(l.to_vec())
                    } else {
                        ScreenStep::NotYet("未见终态回执行".to_string())
                    }
                })
            },
            &mut FreeTextClosures {
                read: || Some(cur()),
                send: |k: &str| {
                    sent.borrow_mut().push(k.to_string());
                    if cursor.get() + 1 < n {
                        cursor.set(cursor.get() + 1);
                    }
                    Ok(())
                },
                send_text: |t: &str| {
                    texts.borrow_mut().push(t.to_string());
                    if cursor.get() + 1 < n {
                        cursor.set(cursor.get() + 1);
                    }
                    Ok(())
                },
                settle: || {},
            },
        );
        (result, sent.into_inner(), texts.into_inner())
    }

    /// **自由作答正常链**：定位数字（**抄屏上编号 3**，不是「选项数+1」）→ 文本 → 回车。
    /// 断言文本**只经字符通道**、键通道里只有定位数字与回车（**没有 Esc、没有第二个
    /// 数字**——K2 的教训）。
    /// 还原动作：把 `send_text` 改走 `send`（文本进键通道）→ 第三句断言先红。
    #[test]
    fn free_text_stage_sends_digit_then_text_then_enter() {
        let script = vec![
            single_free_text_screen(),
            // 定位键按下后（焦点落到该行）→ 文本注入后 → 回车后（终态）
            single_free_text_screen_focused(),
            single_free_text_screen_focused(),
            answered_final_screen(),
        ];
        let (r, keys, texts) = run_free_text_script(script, "green tea please");
        let out = r.expect("自由作答链必须走通");
        assert_eq!(
            keys,
            vec!["3".to_string(), "enter".to_string()],
            "键通道恰两键：定位数字（屏上编号 3）+ 提交回车——**无 Esc 无第二数字**"
        );
        assert_eq!(
            texts,
            vec!["green tea please".to_string()],
            "文本只经字符通道投递一次（裁3：文本绝不进键通道）"
        );
        assert_eq!(out.receipt_seen, Some(true));
        assert_eq!(
            out.sent_keys,
            vec![
                "3".to_string(),
                "<text:16 chars>".to_string(),
                "enter".to_string()
            ],
            "回执键序用占位符记文本（不落正文）：16 = \"green tea please\" 的字符数"
        );
    }

    /// **K7 回归**：自由作答行不在场 → 零按键零文本中止（未定位直接打字是无效操作）。
    /// 还原动作：删掉第 1 段的在场检查 → 本用例先红（会发文本）。
    #[test]
    fn free_text_stage_aborts_with_no_keys_when_row_absent() {
        let script = vec![lines(&[
            " Which drink do you prefer?",
            " 1. Coffee",
            " 2. Tea",
        ])];
        let (r, keys, texts) = run_free_text_script(script, "hi");
        let err = r.as_ref().unwrap_err();
        assert!(err.message.contains("自由作答行"), "中止原因须点名：{err}");
        assert!(keys.is_empty() && texts.is_empty(), "零投递中止");
    }

    /// **定位后形态不符 → 不发文本**（防「定位键被吞了还照打」：那会把答案打进别处）。
    /// 还原动作：删掉定位后的重读复核 → 本用例先红（文本会被发出）。
    #[test]
    fn free_text_stage_aborts_before_text_when_row_disappears_after_locate() {
        let script = vec![
            single_free_text_screen(),
            lines(&[" 已切到别的界面", " > "]),
        ];
        let (r, keys, texts) = run_free_text_script(script, "green tea please");
        let err = r.as_ref().unwrap_err();
        assert!(
            err.message.contains("已见不到自由作答行"),
            "中止原因须点名定位后形态不符：{err}"
        );
        assert_eq!(keys, vec!["3".to_string()], "定位键已发出（事实如实记录）");
        assert!(
            texts.is_empty(),
            "形态不符 ⇒ 绝不发作答文本（不盲打字）：{texts:?}"
        );
    }

    /// **裁3 安全面（端到端锁）**：用户文本里的数字/控制字面**不可能**变成选择——
    /// 定位数字来自屏上编号、文本走独立通道。本用例钉住「键通道里数字只可能是定位
    /// 数字」这条不变量：喂一个「全是数字」的文本，键通道仍只有一个定位数字与一个回车。
    #[test]
    fn free_text_user_digits_never_reach_key_channel() {
        let script = vec![
            single_free_text_screen(),
            single_free_text_screen_focused(),
            single_free_text_screen_focused(),
            answered_final_screen(),
        ];
        let (r, keys, texts) = run_free_text_script(script, "1234\\n5");
        r.expect("含数字文本必须正常作答");
        assert_eq!(
            keys,
            vec!["3".to_string(), "enter".to_string()],
            "文本里的 1234 只出现在字符通道，键通道仅 [定位, 回车]"
        );
        assert_eq!(texts, vec!["1234\\n5".to_string()]);
    }

    /// **空文本 → 零投递拒绝**（防「点了发送但输入框空」把对话框搞乱：空回车在多选框
    /// 里是**切换高亮行勾选**——K9）。还原动作：删掉空文本检查 → 本用例先红。
    #[test]
    fn free_text_empty_is_rejected_without_any_key() {
        let script = vec![single_free_text_screen()];
        // 空串 / 纯空白 / 真换行（归一后是字面 \n，trim 后仍空——注意 `"\\n"` 是
        // **字面反斜杠+n 两字符**，那是合法文本不是空文本，故此处用真换行 `"\n"`）
        for empty in ["", "   ", "\n", "\t"] {
            let (r, keys, texts) = run_free_text_script(script.clone(), empty);
            assert!(r.is_err(), "{empty:?} 必须被拒");
            assert!(keys.is_empty() && texts.is_empty(), "空文本零投递");
        }
    }

    /// 形态门（§2.4/§2.8）· 支持面现状：claude（K4–K7 数字定位定案）→ 批次戊
    /// kimi（Other 行）/ opencode（own answer）→ **codex 2026-10-10 notes 链复活**
    /// （0.162.1 四取样复验全通）；黑盒/无头家仍未定案返回 Err（端点据此降级为
    /// 「请在终端作答」，**不假装能发**）。
    /// 还原动作：把 `free_text_supported` 改成恒 true → 循环里的断言先红。
    #[test]
    fn free_text_supported_surface_by_tool() {
        for tool in ["claude", "kimi", "opencode", "codex"] {
            assert!(free_text_supported(tool), "{tool} 自由作答必须放行");
            assert!(
                free_text_shape(tool).is_ok(),
                "{tool} 支持面与形态描述必须同面"
            );
        }
        let shape = free_text_shape("claude").expect("claude 自由作答已实机定案");
        assert_eq!(shape.locate_label, "Type something");
        assert_eq!(
            shape.submit_key, "enter",
            "文本之后唯一按键是回车（不带数字不带 Esc）"
        );
        // 仍未定案的只剩黑盒/无头家
        for tool in ["zcode", "dsh", "workbuddy", ""] {
            let err =
                free_text_shape(tool).expect_err(&format!("{tool} 的自由作答序列未定案，必须拒绝"));
            assert!(
                err.contains("未实测"),
                "{tool} 的拒绝原因须如实（未实测），不得假装可发：{err}"
            );
            assert!(!free_text_supported(tool), "{tool} 必须走降级文案");
        }
        // 族规格确实是 claude 的（A 族 RawVt；写死在此防端点上写成别的工具）
        let spec = free_text_family_spec();
        assert_eq!(spec.family, crate::inject::families::TuiFamily::RawVt);
        assert_eq!(spec.verified_with, "2.1.251");
    }

    /// **codex 自由作答复活锁**（2026-10-10，0.162.1 四取样复验全通——落卷实锤
    /// N1-N3 + esc 红线 N4；档案 `research/refs/phase2-消息注入/
    /// 2026-10-10-codex-0162-notes链复验.md`）：0.160.0 的「notes 不落卷」形态
    /// 在 0.162.1 未复现——走位 Other + tab 开备注 + 打字上屏核验 + enter 的编排
    /// 重新接线。还原动作：把 `"codex"` 移出 `free_text_supported` 名单 → 本用例
    /// 先红。
    #[test]
    fn codex_free_text_revived_with_other_row_shape() {
        assert!(
            free_text_supported("codex"),
            "codex notes 链已复活（0.162.1 复验），远程自由作答必须放行"
        );
        let shape = free_text_shape("codex").expect("复活后 free_text_shape 必须 Ok");
        assert_eq!(
            shape.locate_label, CODEX_OTHER_ROW_LABEL,
            "定位判据 = Other 行（None of the above，不区分大小写子串）"
        );
        assert_eq!(
            shape.submit_key, "enter",
            "提交键 = enter（备注态 enter 提交「Other + 备注」并交卷）"
        );
        assert!(
            shape.single_question_only,
            "形态门：多题 notes 归属未复采，仅单题卡放行"
        );
    }

    /// wire ↔ 动作：`freeText` 可解析（camelCase）+ 审计摘要只记动作名（**不记正文**）。
    #[test]
    fn free_text_action_wire_and_audit_label() {
        assert_eq!(
            AnswerAction::parse("freeText"),
            Some(AnswerAction::FreeText),
            "wire 词形是 camelCase（与 multiSelect 同风格）"
        );
        assert_eq!(AnswerAction::parse("freetext"), None, "不做大小写容忍");
        assert_eq!(
            AnswerAction::FreeText.audit_label(None),
            "freeText",
            "审计摘要不承载用户正文（只记动作名——正文属用户内容）"
        );
    }

    /// 屏读判据的**形态细节**（防未来「顺手放宽」把安全面磨掉）：
    /// - 焦点标记只在行首（前缀）算数——正文里出现 `❯` 不算焦点行；
    /// - `Submit` 行判据是前缀匹配，故 `Submit answers`（Review 屏那行）**也**命中
    ///   ——这是**刻意的**：提交屏段只问「有多选提交入口吗」，Review 屏在场时它当然
    ///   也在（此时走位段的复核会因焦点不在提交行而继续走位，最终在上限处如实中止）。
    ///   把这条形态写进测试，是为了让「两个屏的判据边界」可读且可回归。
    #[test]
    fn submit_row_predicate_shape_details() {
        assert!(submit_row_focused(" ❯   Submit"), "焦点行（实机形态）");
        assert!(submit_row_focused("❯ Submit"), "无缩进焦点行");
        assert!(!submit_row_focused("    Submit"), "非焦点行不算");
        assert!(
            !submit_row_focused(" 正文里提到 ❯ 但不在行首 Submit"),
            "标记不在行首不算"
        );
        assert!(submit_row_present("    Submit"), "非焦点行在场");
        assert!(submit_row_present(" ❯   Submit"), "焦点行在场");
        assert!(!submit_row_present(" 提交"), "无关行不算");
        // Review 屏的确认项**不**命中「提交行在场」（它带编号 `1. `，而提交行无编号）
        assert!(
            !submit_row_present(" 1. Submit answers"),
            "带编号的 Review 确认项不是提交行（两者形态互斥：提交行无编号）"
        );
    }

    // ===== 2026-09-24 多选闭环切勾：Next 标签 / 行块解析 / 闭环编排 =====

    /// 推进行标签集 {Submit, Next}（多题形态 Next——用户实机取证 2.1.278 屏面）：
    /// `Next` 精确相等防正文 "Next steps:" 冒充；`Submit` 维持前缀口径（既有证据形态）。
    #[test]
    fn advance_row_predicate_accepts_next_label() {
        assert!(submit_row_focused(" ❯   Next"), "多题焦点推进行");
        assert!(submit_row_present("     Next"), "多题非焦点推进行");
        assert!(
            !submit_row_present(" Next steps: refactor the parser"),
            "正文 Next 短语不得冒充推进行（精确相等口径）"
        );
        assert!(
            !submit_row_present(" 1. Next"),
            "带编号行不是推进行（与 Submit 的形态互斥口径一致）"
        );
        // Review 屏（丁复审 dump 原文）整体不得命中任何推进行判据
        let review = review_screen_real();
        assert!(
            review.iter().all(|l| !submit_row_present(l)),
            "Review 屏无推进行（其确认项带编号）：{review:?}"
        );
    }

    /// 行块解析：单题多选实机夹具（丁复审 dump 形态）——3 选项 + Type something +
    /// Submit 推进行；焦点在首选项；勾选框按括号内容语义判。
    #[test]
    fn parse_rows_single_question_real_dump() {
        let rows = parse_question_rows(&multi_submit_screen_focus_on_option());
        assert_eq!(rows.len(), 5, "3 选项 + Type something + Submit：{rows:?}");
        assert_eq!(rows[0].kind, QuestionRowKind::Option);
        assert_eq!(rows[0].number, Some(1));
        assert_eq!(rows[0].checked, Some(true), "实机夹具首项 [✓] 已勾");
        assert_eq!(rows[0].label, "Apple");
        assert!(rows[0].focused, "焦点在首选项行");
        assert_eq!(rows[1].checked, Some(false), "[ ] 未勾");
        assert_eq!(rows[3].kind, QuestionRowKind::FreeText);
        assert_eq!(rows[3].number, Some(4), "Type something = 选项数+1");
        assert!(!rows[3].focused);
        assert_eq!(rows[4].kind, QuestionRowKind::Advance);
        assert_eq!(rows[4].number, None);
        assert_eq!(rows[4].label, "Submit");
        assert!(!rows[4].focused);
        assert_eq!(unique_focused_row(&rows), Some(0));
    }

    /// 行块解析：多题形态（用户实机取证 2026-09-24 截图逐字）——推进行为 `Next`，
    /// 勾选 `[✓]`×2、焦点在 Type something 行（❯ 前缀）。
    #[test]
    fn parse_rows_multi_question_next_row_user_screenshot() {
        let screen = lines(&[
            " ← ☒ 修改目标  ☐ 显示问题  ☐ 文件方式  ✔ Submit  →",
            "",
            " 这次修改 circle.html 的主要目标？（可多选）",
            "",
            " 1. [✓] 设备适配优化",
            " 尺寸缩放、清晰度、触控体验等",
            " 2. [✓] 视觉风格调整",
            " 配色、山/云/海面、角色造型",
            " 3. [ ] 添加动效",
            " 4. [ ] 功能扩展",
            " ❯ 5. [ ] Type something",
            "     Next",
            " ──────────────────────────────",
            " 6. Chat about this",
            "",
            " Enter to select · Tab/Arrow keys to navigate · Esc to cancel",
        ]);
        let rows = parse_question_rows(&screen);
        assert_eq!(rows.len(), 6, "4 选项 + Type something + Next：{rows:?}");
        assert_eq!(
            rows.iter().map(|r| r.checked).collect::<Vec<_>>(),
            vec![
                Some(true),
                Some(true),
                Some(false),
                Some(false),
                Some(false),
                None
            ],
            "勾选态按括号内容语义判"
        );
        assert_eq!(rows[5].kind, QuestionRowKind::Advance);
        assert_eq!(rows[5].label, "Next", "多题推进行 = Next（用户实机）");
        assert_eq!(
            unique_focused_row(&rows),
            Some(4),
            "焦点在 Type something 行"
        );
        assert_eq!(rows[4].kind, QuestionRowKind::FreeText);
        // 分隔线下 Chat 行（编号 6）不在块内
        assert!(!rows.iter().any(|r| r.label.contains("Chat about")));
    }

    /// 行块解析：滚回区正文编号行**不得**混进块——锚上方编号跳变处停手
    /// （正文 1,2 在上、题屏 1..3 在下，中间无分隔也能按连续性切干净）。
    #[test]
    fn parse_rows_trims_scrollback_numbered_prose() {
        let screen = lines(&[
            " 我建议的方案：",
            " 1. 重构解析器",
            " 2. 补测试",
            " 现在问你：",
            " ❯ 1. [ ] 甲",
            " 2. [ ] 乙",
            " 3. [ ] 丙",
            "    Submit",
        ]);
        let rows = parse_question_rows(&screen);
        assert_eq!(
            rows.len(),
            4,
            "滚回区 1,2 被编号跳变截断（正文 2 与题屏 3 之间不连续）：{rows:?}"
        );
        assert_eq!(rows[0].label, "甲");
        assert_eq!(rows[3].kind, QuestionRowKind::Advance);
    }

    /// 行块解析：单选屏（无勾选框、无推进行）→ 空块（toggle 编排按形态不符中止）。
    #[test]
    fn parse_rows_single_select_screen_is_empty_block() {
        let screen = lines(&[
            " ❯ 1. Tool demo",
            " 2. Start a task",
            " 3. Nothing yet",
            " 4. Type something.",
            " 5. Chat about this",
        ]);
        assert!(
            parse_question_rows(&screen).is_empty(),
            "无推进行锚 → 空块（不猜）"
        );
    }

    /// 行块解析：勾选字形族——`[✔]`（U+2714，丁复审 dump）与 `[✓]`（U+2713，用户
    /// 截图）都算已选（括号内容非空即已选，不硬编码字形）。
    #[test]
    fn parse_rows_checkbox_glyph_family() {
        let screen = lines(&[" 1. [✔] Apple", " 2. [ ] Banana", "    Submit"]);
        let rows = parse_question_rows(&screen);
        assert_eq!(rows[0].checked, Some(true), "[✔] 重勾形也算已选");
        assert_eq!(rows[1].checked, Some(false));
    }

    /// **快照 C1 回归锁（评审 2026-10-03）**：Review 确认屏**不是题屏**——
    /// question_screen_snapshot 必须返回 None（anchor B 会把「页签栏 + 编号确认项」
    /// 解析成题屏，题干区还含答毕回显 → 前端对位错跳题并清勾选态），Review 检测
    /// 照常 Ready（GET 据此发 {review:true}）。
    /// 还原动作（变异）：删掉快照函数里的 Review 前置判定 → 本用例先红。
    #[test]
    fn screen_snapshot_returns_none_on_review_screen() {
        let review = review_screen_real();
        assert!(
            question_screen_snapshot(&review).is_none(),
            "Review 确认屏不得产出题屏快照"
        );
        assert!(matches!(probe_review_screen(&review), ScreenStep::Ready(_)));
    }

    /// **快照三态判别（评审 I1）**：占位（free_text=None + present=true）与已打字
    ///（Some(text) + present=true）可分；无 FreeText 行 → present=false。
    #[test]
    fn screen_snapshot_distinguishes_freetext_placeholder_and_typed() {
        // 占位：q1_multi 的 5 行是 [ ] Type something
        let snap = question_screen_snapshot(&live_fixtures::q1_multi()).expect("题屏必出快照");
        assert!(snap.free_text_present);
        assert_eq!(snap.free_text, None, "占位 = None");
        assert_eq!(
            snap.checked,
            vec![Some(false), Some(false), Some(false), Some(false)],
            "4 个选项行（FreeText 行不进 checked）"
        );
        // 已打字：[✔] 内联文字
        let snap2 = question_screen_snapshot(&live_fixtures::q1_typed("融合优点")).unwrap();
        assert!(snap2.free_text_present);
        assert_eq!(snap2.free_text.as_deref(), Some("融合优点"));
        // 无 FreeText 行（q2 单选子题的 5 行是「Type something.」——带句点前缀匹配
        // 仍算 FreeText；改用无 FreeText 的形态：Review 已被上一用例覆盖，这里用
        // 单选子题验证 present 与文本并存）
        let snap3 = question_screen_snapshot(&live_fixtures::q2_single()).unwrap();
        assert!(snap3.free_text_present, "单选子题也有 Type something 行");
        assert_eq!(snap3.checked.len(), 4);
    }

    /// 切勾脚本驱动器（同 `run_submit_script` 的抽法：读屏/发键全脚本化）。
    /// 首段轮询 = 题屏在场（行块可解析）。
    fn run_toggle_script(
        target: usize,
        expected_question: &str,
        screens: Vec<Vec<String>>,
        digit: crate::inject::capability::DigitToggle,
    ) -> (Result<ToggleOutcome, StageAbort>, Vec<String>) {
        use std::cell::{Cell, RefCell};
        let n = screens.len();
        let cursor = Cell::new(0usize);
        let cur = || screens[cursor.get().min(n - 1)].clone();
        let sent: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let result = run_toggle_stages(
            target,
            expected_question,
            || {
                let s = cur();
                if parse_question_rows(&s).is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(s))
                }
            },
            &mut crate::inject::mode::Closures {
                read: || Some(cur()),
                send: |k: &str| {
                    sent.borrow_mut().push(k.to_string());
                    if cursor.get() + 1 < n {
                        cursor.set(cursor.get() + 1);
                    }
                    Ok(())
                },
                settle: || {},
            },
            digit,
        );
        (result, sent.into_inner())
    }

    /// 切题脚本驱动器（同 `run_toggle_script`）：读屏/发键全脚本化。首段轮询直接
    /// 给首屏——三态轮询在端点侧，内核契约 = 第 1 段拿到什么屏就判什么形态。
    fn run_advance_script(
        screens: Vec<Vec<String>>,
        direction: NavDirection,
    ) -> (Result<AdvanceOutcome, StageAbort>, Vec<String>) {
        use std::cell::{Cell, RefCell};
        let n = screens.len();
        let cursor = Cell::new(0usize);
        let cur = || screens[cursor.get().min(n - 1)].clone();
        let sent: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let result = run_advance_stages(
            || Ok(Some(cur())),
            &mut crate::inject::mode::Closures {
                read: || Some(cur()),
                send: |k: &str| {
                    sent.borrow_mut().push(k.to_string());
                    if cursor.get() + 1 < n {
                        cursor.set(cursor.get() + 1);
                    }
                    Ok(())
                },
                settle: || {},
            },
            direction,
        );
        (result, sent.into_inner())
    }

    // ===== Task 5：codex 单选 select 阶段机（设计 §3.2/§3.3；底料定案 A–H）=====

    /// codex select 脚本驱动器：screens 逐屏出队；send 记录键；poll 消费下一屏
    /// （队空 = Ok(None) 窗尽）；terminal.read() 同样出队（队空 = None）。
    /// 注意：send 后 settle 不推进屏——屏推进由驱动器脚本序列显式编排，
    /// terminal.read() 与 poll() 共用同一游标（send/settle 只是记录）。
    /// `expected_total` = 手机卡载荷题数（F10 交叉核对入参；s1 系列 = 2 / s3 = 1）。
    fn run_codex_select_script(
        q_idx: usize,
        expected_total: usize,
        index: usize,
        screens: Vec<Vec<String>>,
    ) -> (
        Result<crate::inject::question::CodexSelectOutcome, StageAbort>,
        Vec<String>,
    ) {
        use std::cell::RefCell;
        let screens = RefCell::new(std::collections::VecDeque::from(screens));
        let sent = RefCell::new(Vec::<String>::new());
        let mut terminal = crate::inject::mode::Closures {
            read: || screens.borrow_mut().pop_front(),
            send: |k: &str| {
                sent.borrow_mut().push(k.to_string());
                Ok(())
            },
            settle: || {},
        };
        let poll = || Ok(screens.borrow_mut().pop_front());
        (
            run_codex_select_stages(q_idx, expected_total, index, poll, &mut terminal),
            sent.into_inner(),
        )
    }

    /// **非末题数字直选·自动推进**（底料定案 A）：Q1 发 `1` → 屏推到 Q2 → 成功零旗标。
    /// F1 语义：poll#1 即读到变化且推进的屏（首拍命中）。
    #[test]
    fn codex_select_nonfinal_digit_auto_advances() {
        let (r, sent) = run_codex_select_script(
            0,
            2,
            0,
            vec![live_fixtures::codex_s1_q1(), live_fixtures::codex_s1_q2()],
        );
        let out = r.expect("数字推进必须成功");
        assert_eq!(out.sent_keys, vec!["1".to_string()], "{sent:?}");
        assert!(!out.reanswered, "计数 2→1 已减 = 新答");
        assert!(!out.already_submitted);
    }

    /// **数字被吞重投**（定案 A 推论 + 评审 P0-1 复核链）：发数字 → 3 拍原样
    /// （变化轮询耗尽）→ 复核 read 仍是本题非末题 → 重投 → 重投轮询后复核 read
    /// = Q2（推进）→ 成功。键序 = ["1","1"]。
    #[test]
    fn codex_select_nonfinal_digit_retry_on_lost_key() {
        let (r, sent) = run_codex_select_script(
            0,
            2,
            0,
            vec![
                live_fixtures::codex_s1_q1(), // 入场基线
                live_fixtures::codex_s1_q1(), // 变化轮询拍 1：原样
                live_fixtures::codex_s1_q1(), // 变化轮询拍 2：原样
                live_fixtures::codex_s1_q1(), // 变化轮询拍 3：原样（耗尽）
                live_fixtures::codex_s1_q1(), // 重投前复核 read：仍 Q1 非末题
                live_fixtures::codex_s1_q1(), // 重投后轮询拍 1：原样
                live_fixtures::codex_s1_q1(), // 重投后轮询拍 2：原样
                live_fixtures::codex_s1_q1(), // 重投后轮询拍 3：原样（耗尽）
                live_fixtures::codex_s1_q2(), // 重投后复核 read：Q2 迟到落地
            ],
        );
        let out = r.expect("重投后必须成功");
        assert_eq!(
            out.sent_keys,
            vec!["1".to_string(), "1".to_string()],
            "{sent:?}"
        );
        assert!(!out.reanswered);
    }

    /// **两投仍不推进 = 中止**：两轮变化轮询全原样 + 两轮复核都停在本题 →
    /// Err 含「两投」，共发 2 个数字。
    #[test]
    fn codex_select_nonfinal_digit_two_miss_aborts() {
        let (r, sent) = run_codex_select_script(
            0,
            2,
            0,
            vec![
                live_fixtures::codex_s1_q1(), // 入场基线
                live_fixtures::codex_s1_q1(), // 首投轮询 1
                live_fixtures::codex_s1_q1(), // 首投轮询 2
                live_fixtures::codex_s1_q1(), // 首投轮询 3（耗尽）
                live_fixtures::codex_s1_q1(), // 重投前复核：仍本题
                live_fixtures::codex_s1_q1(), // 重投轮询 1
                live_fixtures::codex_s1_q1(), // 重投轮询 2
                live_fixtures::codex_s1_q1(), // 重投轮询 3（耗尽）
                live_fixtures::codex_s1_q1(), // 重投后复核：仍本题非末 → 两投中止
            ],
        );
        let err = r.expect_err("两投不推进必须中止");
        assert!(err.message.contains("两投"), "{err}");
        assert_eq!(sent.len(), 2, "数字恰好两投：{sent:?}");
    }

    /// **新增（评审 P0-1 核心行为锁）**：首投 3 拍原样 → 复核 read = Q2（迟到位
    /// 重绘落地）→ **单投成功**（不重投——旧实现会在此重投，数字落在 Q2 上）。
    #[test]
    fn codex_select_nonfinal_stale_read_recovery_without_resend() {
        let (r, sent) = run_codex_select_script(
            0,
            2,
            0,
            vec![
                live_fixtures::codex_s1_q1(), // 入场基线
                live_fixtures::codex_s1_q1(), // 变化轮询拍 1：原样
                live_fixtures::codex_s1_q1(), // 变化轮询拍 2：原样
                live_fixtures::codex_s1_q2(), // 重投前复核 read：Q2 迟到落地
            ],
        );
        let out = r.expect("迟到位重绘必须单投成功");
        assert_eq!(
            out.sent_keys,
            vec!["1".to_string()],
            "未重投（sent 恰 1 个数字）：{sent:?}"
        );
        assert!(!out.reanswered);
    }

    /// **新增（评审 P0-1 末题红线锁）**：首投轮询耗尽 + 复核 read 解析出**另一份
    /// 问卷的末题面板**（codex_s3_single：Question 1/1，idx 既非本题也非本题+1）
    /// → Err「未重投」（重投数字 = 打错题/触碰末题禁数字红线），sent 只含首投
    /// 1 个数字。注：不能用 codex_s1_q2 造「离题」——它 idx=1==q_idx+1，走的是
    /// 迟到位落地成功分支（`stale_read_recovery_without_resend` 锁的就是它）。
    #[test]
    fn codex_select_nonfinal_resend_refuses_when_screen_left_question() {
        let (r, sent) = run_codex_select_script(
            0,
            2,
            0,
            vec![
                live_fixtures::codex_s1_q1(),     // 入场基线（Q1 非末题）
                live_fixtures::codex_s1_q1(),     // 变化轮询拍 1：原样
                live_fixtures::codex_s1_q1(),     // 变化轮询拍 2：原样
                live_fixtures::codex_s1_q1(),     // 变化轮询拍 3：原样
                live_fixtures::codex_s1_q1(),     // 变化轮询拍 4：原样（耗尽）
                live_fixtures::codex_s3_single(), // 重投前复核：屏已是别卷末题面板
            ],
        );
        let err = r.expect_err("复核发现离题必须拒绝重投");
        assert!(
            err.message.contains("未重投") && err.message.contains("末题"),
            "{err}"
        );
        assert_eq!(
            sent,
            vec!["1".to_string()],
            "只有首投 1 个数字，未重投：{sent:?}"
        );
    }

    /// **新增（评审 P0-1 形态异常锁）**：发数字后 poll 读到**屏已变化但题号未推进**
    /// （同夹具加一行空行造差异）→ Err「形态异常」，未进入重投。
    #[test]
    fn codex_select_nonfinal_screen_changed_but_not_advanced_aborts() {
        let mut variant = live_fixtures::codex_s1_q1();
        variant.insert(0, String::new()); // 屏变化（多一空行）但题号仍 Q1
        let (r, sent) = run_codex_select_script(
            0,
            2,
            0,
            vec![
                live_fixtures::codex_s1_q1(), // 入场基线
                variant,                      // poll#1：屏变 ≠ 基线，题号未推进
            ],
        );
        let err = r.expect_err("屏变不推进必须中止");
        assert!(err.message.contains("形态异常"), "{err}");
        assert_eq!(sent, vec!["1".to_string()], "形态异常不重投：{sent:?}");
    }

    /// **改答旗标**（§3.2 漂移分支）：进闸前该题已被手答（k_base=0，计数段消失）→
    /// 数字推进后落点屏计数仍 0（== k_base）→ reanswered=true。
    /// 还原动作（变异）：删掉 reanswered 判定 → 本用例先红。
    #[test]
    fn codex_select_reanswered_flag_when_counter_unchanged() {
        let (r, _sent) = run_codex_select_script(
            0,
            2,
            0,
            vec![
                live_fixtures::codex_s1_q1_answered(), // k_base = 0（已答）
                live_fixtures::codex_s1_q2_zero(),     // 落点 Q2 k=0（计数未减）
            ],
        );
        let out = r.expect("改答推进必须成功");
        assert_eq!(out.sent_keys, vec!["1".to_string()]);
        assert!(out.reanswered, "计数未减（0==0）= 改答");
    }

    /// **末题全链**（§3.3）：走位 j 一发（down=(1+4-0)%4=1）→ 空格 → 归零闸 →
    /// 双条件闸 → enter → 回执屏。键序 = ["j","space","enter"]。
    #[test]
    fn codex_select_final_walks_then_space_zero_gate_enter() {
        let (r, sent) = run_codex_select_script(
            1,
            2,
            1,
            vec![
                live_fixtures::codex_s1_q2(),               // 入场：k=1 焦点 0
                live_fixtures::codex_s1_q2_focused(1),      // 走位复读：焦点 1 停
                live_fixtures::codex_s1_q2_zero_focused(1), // 空格后：归零闸
                live_fixtures::codex_s1_q2_zero_focused(1), // 双条件闸：k=0
                live_fixtures::codex_answered_summary(),    // enter 后 poll 回执
            ],
        );
        let out = r.expect("末题全链必须成功");
        assert_eq!(
            out.sent_keys,
            vec!["j".to_string(), "space".to_string(), "enter".to_string()],
            "{sent:?}"
        );
        assert!(!out.already_submitted);
        assert!(!out.reanswered);
    }

    /// **最短环绕方向**：入场未答（k=1、焦点 1），目标 0 →
    /// down=(0+4-1)%4=3 vs up=(1+4-0)%4=1 → 取 "k"（up 更短）→ 空格 → 归零 →
    /// enter。**F2 适配（评审 P1-2）**：旧构造用已答归零屏（k=0 焦点≠目标）——
    /// 该形态现在如实中止（已答换选先反选旧答案），测最短环绕须改未答形态
    /// （k>0 主路径），「测 up<down 取 k」的意图不变。
    #[test]
    fn codex_select_final_walk_uses_shortest_wrap_direction() {
        let (r, sent) = run_codex_select_script(
            1,
            2,
            0,
            vec![
                live_fixtures::codex_s1_q2_focused(1),      // 入场：k=1 焦点 1
                live_fixtures::codex_s1_q2_focused(0),      // k 复读：焦点 0 停
                live_fixtures::codex_s1_q2_zero_focused(0), // 空格后：归零闸
                live_fixtures::codex_s1_q2_zero_focused(0), // 双条件闸：k=0
                live_fixtures::codex_answered_summary(),
            ],
        );
        let out = r.expect("最短环绕走位必须成功");
        assert_eq!(
            out.sent_keys,
            vec!["k".to_string(), "space".to_string(), "enter".to_string()],
            "up 更短 → 首键 k：{sent:?}"
        );
    }

    /// **已答特判跳空格**（§3.3 步 3）：入场已归零且焦点即目标 → 无 j/k 无 space，
    /// 归零闸直过 → enter。
    #[test]
    fn codex_select_final_already_answered_skips_space() {
        let (r, sent) = run_codex_select_script(
            1,
            2,
            1,
            vec![
                live_fixtures::codex_s1_q2_zero_focused(1), // 入场：k=0 焦点 1=目标
                live_fixtures::codex_s1_q2_zero_focused(1), // 归零闸
                live_fixtures::codex_s1_q2_zero_focused(1), // 双条件闸
                live_fixtures::codex_answered_summary(),
            ],
        );
        let out = r.expect("已答同项必须跳空格");
        assert_eq!(out.sent_keys, vec!["enter".to_string()], "{sent:?}");
    }

    /// **双条件闸读到面板消失 = 已交卷**（§3.3 步 6 分叉，如 notes 回车连带）：零发键。
    #[test]
    fn codex_select_submit_gate_panel_gone_means_already_submitted() {
        let (r, sent) = run_codex_select_script(
            1,
            2,
            1,
            vec![
                live_fixtures::codex_s1_q2_zero_focused(1), // 入场
                live_fixtures::codex_s1_q2_zero_focused(1), // 归零闸
                live_fixtures::codex_answered_summary(),    // 双条件闸：面板消失
            ],
        );
        let out = r.expect("面板消失 = 已交卷（成功返回）");
        assert!(out.already_submitted, "零发键交卷分支");
        assert!(sent.is_empty(), "未发提交键：{sent:?}");
    }

    /// **归零闸焦点漂移**：归零屏焦点 ≠ 目标 → 中止（可能反选了已选项），无 enter。
    #[test]
    fn codex_select_zero_gate_fails_on_focus_drift() {
        let (r, sent) = run_codex_select_script(
            1,
            2,
            1,
            vec![
                live_fixtures::codex_s1_q2_zero_focused(1), // 入场：焦点 1=目标
                live_fixtures::codex_s1_q2_zero_focused(0), // 归零闸：焦点漂到 0
            ],
        );
        let err = r.expect_err("焦点漂移必须中止");
        assert!(err.message.contains("焦点"), "{err}");
        assert!(!sent.contains(&"enter".to_string()), "{sent:?}");
    }

    /// **空格后窗尽 = 中止**：走位完成后归零闸 poll 队空（Ok(None)）→ Err「未归零」，
    /// 已发 j+space 无 enter。
    #[test]
    fn codex_select_zero_gate_fail_aborts_after_space() {
        let (r, sent) = run_codex_select_script(
            1,
            2,
            1,
            vec![
                live_fixtures::codex_s1_q2(), // 入场：k=1 焦点 0
                live_fixtures::codex_s1_q2_focused(1), // 走位复读
                                              // 归零闸 poll 队空 = 窗尽
            ],
        );
        let err = r.expect_err("窗尽未归零必须中止");
        assert!(err.message.contains("未归零"), "{err}");
        assert!(sent.contains(&"j".to_string()), "{sent:?}");
        assert!(sent.contains(&"space".to_string()), "{sent:?}");
        assert!(!sent.contains(&"enter".to_string()));
    }

    /// **身份闸**：屏上 Q2 vs 卡面第 1 题 → 零按键中止。
    #[test]
    fn codex_select_identity_gate_aborts_zero_keys() {
        let (r, sent) = run_codex_select_script(
            0,
            2,
            0,
            vec![live_fixtures::codex_s1_q2()], // 屏上是第 2 题
        );
        let err = r.expect_err("题号不一致必须中止");
        assert!(err.message.contains("不一致"), "{err}");
        assert!(sent.is_empty(), "身份闸零按键：{sent:?}");
    }

    /// **确认屏中止**（§3.6 闸门矩阵）：`Submit with unanswered questions?` 在场
    /// → 零按键中止不代答。
    #[test]
    fn codex_select_aborts_with_named_cause_on_confirm_screen() {
        let (r, sent) = run_codex_select_script(
            0,
            2,
            0,
            vec![lines(&["  Submit with unanswered questions?"])], // 构造词形（账本在档）
        );
        let err = r.expect_err("确认屏必须中止");
        assert!(err.message.contains("未答完确认屏"), "{err}");
        assert!(sent.is_empty());
    }

    /// **末题焦点缺席中止**：剥掉 › 行（派生）→ 读不到焦点 → 不猜起点零键中止。
    #[test]
    fn codex_select_final_no_focus_aborts() {
        let no_focus: Vec<String> = live_fixtures::codex_s1_q2_zero()
            .into_iter()
            .map(|l| l.replace("› ", "  "))
            .collect();
        let (r, sent) = run_codex_select_script(1, 2, 1, vec![no_focus]);
        let err = r.expect_err("焦点缺席必须中止");
        assert!(err.message.contains("焦点"), "{err}");
        assert!(sent.is_empty());
    }

    /// **双条件闸计数又变化**：归零闸过了但提交闸屏 k≠0 → 中止未发 enter。
    #[test]
    fn codex_select_submit_gate_counter_changed_aborts() {
        let (r, sent) = run_codex_select_script(
            1,
            2,
            1,
            vec![
                live_fixtures::codex_s1_q2_zero_focused(1), // 入场
                live_fixtures::codex_s1_q2_zero_focused(1), // 归零闸
                live_fixtures::codex_s1_q2_focused(1),      // 提交闸：k=1 又变化
            ],
        );
        let err = r.expect_err("计数回涨必须中止");
        assert!(err.message.contains("又变化"), "{err}");
        assert!(!sent.contains(&"enter".to_string()), "{sent:?}");
    }

    /// **新增（评审 P1-2）双条件闸面板消失但无终态证据**：提交闸读到屏（有行）却
    /// 解析不出面板、且屏上既无摘要头（QUESTION/RECEIPT）也无确认屏标题——
    /// 「已交卷」的结论必须带终态证据，此处 → Err「面板异常消失」，未发提交键。
    #[test]
    fn codex_select_submit_gate_panel_gone_without_receipt_aborts() {
        // 无锚行集：普通正文行（无题号头无 footer 锚无摘要头无确认屏标题）
        let plain: Vec<String> = vec![
            "some transcript line".to_string(),
            "› Ask Codex to do anything".to_string(),
        ];
        let (r, sent) = run_codex_select_script(
            1,
            2,
            1,
            vec![
                live_fixtures::codex_s1_q2_zero_focused(1), // 入场
                live_fixtures::codex_s1_q2_zero_focused(1), // 归零闸
                plain,                                      // 提交闸：读到屏但无面板无摘要
            ],
        );
        let err = r.expect_err("无证据的面板消失必须中止");
        assert!(err.message.contains("面板异常消失"), "{err}");
        assert!(!sent.contains(&"enter".to_string()), "{sent:?}");
    }

    /// **新增（评审 P2-10）身份闸题数交叉核对**：屏上问卷 3 题（S2 夹具）vs
    /// 手机卡载荷 2 题 → 零按键中止（错卷发键 = 打错题）。
    #[test]
    fn codex_select_identity_gate_total_mismatch_aborts_zero_keys() {
        let (r, sent) = run_codex_select_script(
            0,
            2,
            0,
            vec![live_fixtures::codex_s2_q1()], // 屏上 Question 1/3 vs 载荷 2 题
        );
        let err = r.expect_err("题数不一致必须中止");
        assert!(
            err.message.contains("不一致") && err.message.contains("题"),
            "{err}"
        );
        assert!(sent.is_empty(), "身份闸零按键：{sent:?}");
    }

    /// **新增（评审 P1-1）提交闸四条件·切题旁路锁**：归零闸过闸后、发 enter 前，
    /// 等待窗口内用户 h/l 切到非末题（全卷已答时 k=0 在任何题都成立，旧闸只验
    /// `unanswered != 0` 挡不住）→ 提交闸必须验题号对位 + 末题 → Err「已切离」，
    /// sent 无 enter。构造：入场末题归零焦点即目标（跳空格）→ 归零闸 → 提交闸
    /// 屏已是非末题已答题（codex_s1_q1_answered：Question 1/2 无计数段）。
    #[test]
    fn codex_select_submit_gate_aborts_when_switched_to_nonfinal() {
        let (r, sent) = run_codex_select_script(
            1,
            2,
            1,
            vec![
                live_fixtures::codex_s1_q2_zero_focused(1), // 入场：k=0 焦点 1=目标
                live_fixtures::codex_s1_q2_zero_focused(1), // 归零闸
                live_fixtures::codex_s1_q1_answered(),      // 提交闸：屏已被切到 Q1 非末题
            ],
        );
        let err = r.expect_err("提交闸发现切题必须中止");
        assert!(err.message.contains("已切离"), "{err}");
        assert!(
            !sent.contains(&"enter".to_string()),
            "绝不发提交键：{sent:?}"
        );
    }

    /// **新增（评审 P1-1）提交闸四条件·焦点漂移锁**：提交闸屏仍是本题末题且
    /// 已归零，但焦点已漂离目标 → Err「焦点漂移」，sent 无 enter。
    #[test]
    fn codex_select_submit_gate_aborts_when_focus_drifted_at_gate() {
        let (r, sent) = run_codex_select_script(
            1,
            2,
            1,
            vec![
                live_fixtures::codex_s1_q2_zero_focused(1), // 入场：k=0 焦点 1=目标
                live_fixtures::codex_s1_q2_zero_focused(1), // 归零闸
                live_fixtures::codex_s1_q2_zero_focused(0), // 提交闸：焦点漂到 0
            ],
        );
        let err = r.expect_err("提交闸发现焦点漂移必须中止");
        assert!(err.message.contains("焦点漂移"), "{err}");
        assert!(!sent.contains(&"enter".to_string()), "{sent:?}");
    }

    /// **新增（评审 P1-2）已答改答不同项·如实中止**：入场已归零（k=0）且焦点
    /// ≠目标——0.160.0 实测此形态下空格 = 反选旧答案而非换选（底料 §3 #5），
    /// 远程代答会先破坏旧答案 → Err「反选旧答案」，**sent 为空**（未发任何键）。
    #[test]
    fn codex_select_final_answered_other_target_aborts_preserving_answer() {
        let (r, sent) = run_codex_select_script(
            1,
            2,
            0, // 目标 Redis，焦点在 Memcached（已选项）
            vec![live_fixtures::codex_s1_q2_zero_focused(1)], // k=0 焦点 1
        );
        let err = r.expect_err("已答换选必须如实中止");
        assert!(err.message.contains("反选旧答案"), "{err}");
        assert!(sent.is_empty(), "未发任何键：{sent:?}");
    }

    /// **新增（评审 P2-4）末题目标越界·零键中止**：载荷 index ≥ 屏上选项数
    /// （载荷与终端面板不同步）→ 走位取模算术会静默算错步数 → 发键前卫语句
    /// 中止，Err 含「越界」，sent 空。
    #[test]
    fn codex_select_final_target_out_of_range_aborts_zero_keys() {
        let (r, sent) = run_codex_select_script(1, 2, 5, vec![live_fixtures::codex_s1_q2()]);
        let err = r.expect_err("末题越界必须零键中止");
        assert!(err.message.contains("越界"), "{err}");
        assert!(sent.is_empty(), "未发任何键：{sent:?}");
    }

    /// **新增（评审 P2-4）非末题目标越界·零键中止**：数字 = index+1 直接发，
    /// 越界时 codex 面板对无效数字静默忽略（只剩双投兜底）——发键前卫语句中止。
    #[test]
    fn codex_select_nonfinal_target_out_of_range_aborts_zero_keys() {
        let (r, sent) = run_codex_select_script(0, 2, 5, vec![live_fixtures::codex_s1_q1()]);
        let err = r.expect_err("非末题越界必须零键中止");
        assert!(err.message.contains("越界"), "{err}");
        assert!(sent.is_empty(), "未发任何键：{sent:?}");
    }

    // ===== Task 6：codex 切题 advance 阶段机（设计 §3.5；底料 §1 键序更正 h/l）=====

    /// codex advance 脚本驱动器（同 `run_codex_select_script`）：screens 逐屏出队；
    /// send 记录键；poll 消费下一屏（队空 = Ok(None) 窗尽）；terminal.read() 同样
    /// 出队（队空 = None）。send/settle 不推进屏——屏推进由脚本序列显式编排。
    fn run_codex_advance_script(
        direction: NavDirection,
        screens: Vec<Vec<String>>,
    ) -> (
        Result<crate::inject::question::CodexAdvanceOutcome, StageAbort>,
        Vec<String>,
    ) {
        use std::cell::RefCell;
        let screens = RefCell::new(std::collections::VecDeque::from(screens));
        let sent = RefCell::new(Vec::<String>::new());
        let mut terminal = crate::inject::mode::Closures {
            read: || screens.borrow_mut().pop_front(),
            send: |k: &str| {
                sent.borrow_mut().push(k.to_string());
                Ok(())
            },
            settle: || {},
        };
        let poll = || Ok(screens.borrow_mut().pop_front());
        (
            run_codex_advance_stages(direction, poll, &mut terminal),
            sent.into_inner(),
        )
    }

    /// **Next 切题到位**（设计 §3.5）：Q1 屏发 `l` → 屏到 Q2（idx 0→1）→ Arrived。
    #[test]
    fn codex_advance_next_lands_next_question() {
        let (r, sent) = run_codex_advance_script(
            NavDirection::Next,
            vec![live_fixtures::codex_s1_q1(), live_fixtures::codex_s1_q2()],
        );
        let out = r.expect("切题必须成功");
        assert_eq!(out, CodexAdvanceOutcome::Arrived);
        assert_eq!(sent, vec!["l".to_string()], "{sent:?}");
    }

    /// **Prev 切回上一题**：Q2 屏发 `h` → 屏回 Q1（idx 1→0）→ Arrived。
    #[test]
    fn codex_advance_prev_lands_prev_question() {
        let (r, sent) = run_codex_advance_script(
            NavDirection::Prev,
            vec![live_fixtures::codex_s1_q2(), live_fixtures::codex_s1_q1()],
        );
        let out = r.expect("切回上一题必须成功");
        assert_eq!(out, CodexAdvanceOutcome::Arrived);
        assert_eq!(sent, vec!["h".to_string()], "{sent:?}");
    }

    /// **单题 h/l 无效**（底料 S3：footer 无导航段 + VK 左右零变化实证）：屏解析
    /// 成功但 idx 未变 → NoEffect（如实未生效，不中止——dispatch 层转「未生效，可重试」）。
    #[test]
    fn codex_advance_single_question_no_effect() {
        let (r, sent) = run_codex_advance_script(
            NavDirection::Next,
            vec![
                live_fixtures::codex_s3_single(),
                live_fixtures::codex_s3_single(), // 屏未动：idx 仍 0
            ],
        );
        let out = r.expect("单题切题必须如实返回未生效");
        assert_eq!(out, CodexAdvanceOutcome::NoEffect);
        assert_eq!(sent, vec!["l".to_string()], "{sent:?}");
    }

    /// **窗尽 = 如实未生效**：发键后 poll 队空（Ok(None)）——窗内未见题号变化 →
    /// NoEffect（不猜：不冒充到达也不中止；键已发出，效果未知，用户可重试）。
    #[test]
    fn codex_advance_window_exhausted_no_effect() {
        let (r, sent) = run_codex_advance_script(
            NavDirection::Next,
            vec![live_fixtures::codex_s1_q1()], // 发键后队空 = 窗尽
        );
        let out = r.expect("窗尽必须如实返回未生效");
        assert_eq!(out, CodexAdvanceOutcome::NoEffect);
        assert_eq!(sent, vec!["l".to_string()], "{sent:?}");
    }

    /// **切题后面板形态崩**：发键后屏变摘要（面板不在场且解析不出）→ 中止引导人工。
    #[test]
    fn codex_advance_panel_gone_after_key_aborts() {
        let (r, sent) = run_codex_advance_script(
            NavDirection::Next,
            vec![
                live_fixtures::codex_s1_q1(),
                live_fixtures::codex_answered_summary(), // 发键后屏变摘要 = 形态异常
            ],
        );
        let err = r.expect_err("形态崩必须中止");
        assert!(
            err.message.contains("形态崩") || err.message.contains("解析不出"),
            "{err}"
        );
        assert_eq!(sent, vec!["l".to_string()], "{sent:?}");
    }

    /// **首屏读不到 = 零键中止**：screens 空（terminal.read() = None）→
    /// Err 含「读不到面板」，sent 空。
    #[test]
    fn codex_advance_read_fail_aborts() {
        let (r, sent) = run_codex_advance_script(NavDirection::Next, vec![]);
        let err = r.expect_err("读不到面板必须中止");
        assert!(err.message.contains("读不到面板"), "{err}");
        assert!(sent.is_empty(), "零按键中止：{sent:?}");
    }

    /// **环形到达被检测**（底料 §1：切题环形，末→首）：末题（idx=1）发 `l` 回
    /// 首题（idx=0）——到达判据 = i 变化（!= before），环形语义透传被覆盖 → Arrived。
    #[test]
    fn codex_advance_wrap_around_detected() {
        let (r, sent) = run_codex_advance_script(
            NavDirection::Next,
            vec![live_fixtures::codex_s1_q2(), live_fixtures::codex_s1_q1()],
        );
        let out = r.expect("环形到达必须被检测为成功");
        assert_eq!(out, CodexAdvanceOutcome::Arrived);
        assert_eq!(sent, vec!["l".to_string()], "{sent:?}");
    }

    /// **新增（评审 P2-2）单拍陈旧帧仍到达**：发键后前两拍 poll 读到的都是与
    /// baseline 逐字相同的未重绘帧（单拍判定会误报 NoEffect）→ 有界轮询继续
    /// 等下一拍，第 3 拍 idx 变化 → Arrived，sent == ["l"]。
    #[test]
    fn codex_advance_stale_first_frame_still_arrives() {
        let (r, sent) = run_codex_advance_script(
            NavDirection::Next,
            vec![
                live_fixtures::codex_s1_q1(), // baseline（Q1）
                live_fixtures::codex_s1_q1(), // 拍 1：陈旧原样帧
                live_fixtures::codex_s1_q1(), // 拍 2：陈旧原样帧
                live_fixtures::codex_s1_q2(), // 拍 3：Q2 落地 → Arrived
            ],
        );
        let out = r.expect("陈旧帧后的迟到切题必须被检为到达");
        assert_eq!(out, CodexAdvanceOutcome::Arrived);
        assert_eq!(sent, vec!["l".to_string()], "{sent:?}");
    }

    /// 夹具屏的题干原文（multi()/MULTI_JSON 的 question 与屏面第 2 行同文）——
    /// run_toggle_script 各用例的身份核验输入
    const FIXTURE_QUESTION: &str = "Which fruits are your favorites? (Select all that apply)";

    /// 夹具工厂：多选题屏、指定焦点行（0=首选项 … 3=Type something、4=Submit 行）
    /// 与首项勾选态——基于实机夹具形态生成。
    fn multi_screen_focus_at(focus_row: usize, first_checked: bool) -> Vec<String> {
        let mark = |i: usize| if i == focus_row { " ❯" } else { "  " };
        let tick = if first_checked { "[✓]" } else { "[ ]" };
        lines(&[
            " ← ☒ Favorite fruits  ✔Submit  →",
            "",
            " Which fruits are your favorites? (Select all that apply)",
            "",
            &format!("{}1. {} Apple", mark(0), tick),
            &format!("{}2. [ ] Banana", mark(1)),
            &format!("{}3. [ ] Peach", mark(2)),
            &format!("{}4. [ ] Type something", mark(3)),
            &format!("{}   Submit", mark(4)),
            " 5. Chat about this",
        ])
    }

    /// 场景①：焦点已在目标行 → 单发 `space`，屏读核验翻转（verified + 新态）。
    #[test]
    fn toggle_stage_happy_path_focus_on_target() {
        let (r, sent) = run_toggle_script(
            0,
            FIXTURE_QUESTION,
            vec![
                multi_screen_focus_at(0, false),
                multi_screen_focus_at(0, true),
            ],
            crate::inject::capability::DigitToggle::Unsupported,
        );
        let out = r.expect("焦点在目标行，空格必达");
        assert_eq!(sent, vec!["space".to_string()], "零走位：只有空格");
        assert!(out.verified, "屏读核验到翻转");
        assert_eq!(out.checked_now, Some(true), "回执带翻转后的勾选态");
    }

    /// 场景②：焦点在首选项、目标第 3 项（Peach）→ down×2 走位（每步复核位移）
    /// + space，键序 = [down, down, space]。
    #[test]
    fn toggle_stage_walks_down_then_toggles() {
        // 末屏表达 Peach 已勾（空格生效后的重绘形态）
        let after = {
            let mut s = multi_screen_focus_at(2, false);
            s[6] = " 3. [✓] Peach".to_string();
            s
        };
        let (r, sent) = run_toggle_script(
            2,
            FIXTURE_QUESTION,
            vec![
                multi_screen_focus_at(0, false),
                multi_screen_focus_at(1, false),
                multi_screen_focus_at(2, false),
                after,
            ],
            crate::inject::capability::DigitToggle::Unsupported,
        );
        let out = r.expect("走位两步 + 空格");
        assert_eq!(
            sent,
            vec!["down".to_string(), "down".to_string(), "space".to_string()],
            "键序 = [down, down, space]"
        );
        assert!(out.verified);
        assert_eq!(out.checked_now, Some(true));
    }

    /// 场景③：焦点在 Submit 行、目标第 1 项 → **向上**走位 up×4 + space
    /// （2026-09-24 方向感知：原只发 down 的实现在此恒中止）。
    #[test]
    fn toggle_stage_walks_up_from_advance_row() {
        let after = {
            let mut s = multi_screen_focus_at(0, false);
            s[4] = " ❯ 1. [✓] Apple".to_string();
            s
        };
        let (r, sent) = run_toggle_script(
            0,
            FIXTURE_QUESTION,
            vec![
                multi_screen_focus_at(4, false),
                multi_screen_focus_at(3, false),
                multi_screen_focus_at(2, false),
                multi_screen_focus_at(1, false),
                multi_screen_focus_at(0, false),
                after,
            ],
            crate::inject::capability::DigitToggle::Unsupported,
        );
        let out = r.expect("向上走位四步 + 空格");
        assert_eq!(sent.len(), 5, "up×4 + space：{sent:?}");
        assert_eq!(sent[0], "up");
        assert_eq!(sent[4], "space");
        assert!(out.verified);
    }

    /// 场景④：空格发出但勾选态未翻转（版本不消费该形态）→ 中止、报「翻转」、
    /// 键里只有 space（无后续键可发——本就是最后一步）。
    #[test]
    fn toggle_stage_aborts_when_flip_not_seen() {
        let (r, sent) = run_toggle_script(
            0,
            FIXTURE_QUESTION,
            vec![
                multi_screen_focus_at(0, false),
                multi_screen_focus_at(0, false), // 空格后屏无变化
            ],
            crate::inject::capability::DigitToggle::Unsupported,
        );
        let err = r.expect_err("未翻转必须中止");
        assert!(
            err.message.contains("翻转"),
            "中止原因须点名翻转核验：{err}"
        );
        assert_eq!(sent, vec!["space".to_string()]);
        assert_eq!(err.kind, StageAbortKind::Screen);
    }

    /// 场景⑤：屏上无唯一焦点（❯ 缺失）→ 零键中止（「不猜起点」）。
    #[test]
    fn toggle_stage_aborts_without_unique_focus() {
        let no_focus = {
            let mut s = multi_screen_focus_at(0, false);
            s[4] = "  1. [ ] Apple".to_string();
            s
        };
        let (r, sent) = run_toggle_script(
            0,
            FIXTURE_QUESTION,
            vec![no_focus],
            crate::inject::capability::DigitToggle::Unsupported,
        );
        let err = r.expect_err("无焦点必须中止");
        assert!(err.message.contains("唯一焦点行"), "{err}");
        assert!(sent.is_empty(), "零按键：{sent:?}");
    }

    /// 场景⑥：目标选项不在块内（序号越界到屏上没有的编号）→ 零键中止。
    #[test]
    fn toggle_stage_aborts_when_target_absent() {
        let (r, sent) = run_toggle_script(
            9,
            FIXTURE_QUESTION,
            vec![multi_screen_focus_at(0, false)],
            crate::inject::capability::DigitToggle::Unsupported,
        );
        let err = r.expect_err("目标不在块内必须中止");
        assert!(err.message.contains("第 10 个模型选项"), "{err}");
        assert!(sent.is_empty());
    }

    /// **身份核验·emoji 容错（2026-10-03 活体事故）**：载荷题干带键帽/变体选择
    /// emoji，控制台缓冲读回同位置变成 U+FFFD——归一化剥离后必须仍然匹配
    ///（旧行为：逐字比对恒失败 → 正常作答被「题目不一致」误中止）。
    /// 还原动作（变异）：norm 去掉 FFFD/FE0F/20E3/星面剥离 → 本用例先红。
    #[test]
    fn identity_tolerates_console_lossy_emoji() {
        let payload = "问题3\u{FE0F}\u{20E3}：优化的重点是哪些方面？ （可多选）";
        let screen = lines(&[
            "←  ☐ 优化重点  ✔ Submit  →",
            "",
            "问题3\u{FFFD}\u{FFFD}：优化的重点是哪些方面？ （可多选）",
            "",
            "❯ 1. [ ] 语言与文笔润色",
            "  2. [ ] 情感描写",
            "    Submit",
        ]);
        assert!(
            screen_question_matches(&screen, payload),
            "emoji 读写损失必须被归一化吸收"
        );
    }

    /// **评审 I1 回归锁（2026-09-24）**：屏上题干与载荷题干不一致（用户已手动
    /// ←/→ 切题、手机卡停在旧题）→ 切勾**零键中止**——闭环验得了按键效果，验不了
    /// 目标身份；没有本判据，切勾会作用到**别的题**的选项上（翻转验得过、题错了）。
    /// 还原动作：删掉第 1 段的 screen_question_matches 判据 → 本用例先红。
    #[test]
    fn toggle_stage_aborts_on_question_identity_mismatch() {
        // 屏面仍是「Favorite fruits」题，但载荷当前题是另一道（多题第 2 题）
        let (r, sent) = run_toggle_script(
            0,
            "Which output folder should the build use?",
            vec![multi_screen_focus_at(0, false)],
            crate::inject::capability::DigitToggle::Unsupported,
        );
        let err = r.expect_err("题干不一致必须中止");
        assert!(
            err.message.contains("题目与手机卡片不一致"),
            "中止原因须点名身份核验：{err}"
        );
        assert!(sent.is_empty(), "零按键：{sent:?}");
    }

    /// 评审 I1 的**通过面**：题干一致（含载荷是屏面子串的折行/短文形态）→ 正常
    /// 切勾——身份判据不得误伤正常路径。
    #[test]
    fn toggle_stage_passes_identity_when_payload_is_screen_substr() {
        // 载荷题干是屏面题干的前缀（短文载荷 / 屏面带装饰后缀的形态）
        let (r, sent) = run_toggle_script(
            0,
            "Which fruits are your favorites?",
            vec![
                multi_screen_focus_at(0, false),
                multi_screen_focus_at(0, true),
            ],
            crate::inject::capability::DigitToggle::Unsupported,
        );
        let out = r.expect("题干一致（子串形态）必须放行");
        assert!(out.verified);
        assert_eq!(sent, vec!["space".to_string()]);
    }

    /// **评审 Minor-3 回归锁**：走位后焦点**反向**位移（发 down 却向上跳）→ 中止
    /// 不发空格——原实现只校验位移绝对值，反向位移会乒乓走到上限（仍安全但浪费按键）。
    #[test]
    fn toggle_stage_aborts_on_reverse_displacement() {
        // 屏 1 焦点在选项 1（目标 3 → 方向 down）；发 down 后屏上焦点跳到**上方**
        // （终端自行移动焦点的形态）→ moved=-1 ≠ +1 → 中止
        let (r, sent) = run_toggle_script(
            2,
            FIXTURE_QUESTION,
            vec![
                multi_screen_focus_at(0, false),
                multi_screen_focus_at(0, true), // 任意异屏（焦点仍在行 0，moved=0≠1 同样触发）
            ],
            crate::inject::capability::DigitToggle::Unsupported,
        );
        let err = r.expect_err("反向位移必须中止");
        assert!(
            err.message.contains("应沿目标方向恰 1 行"),
            "中止原因须点名方向校验：{err}"
        );
        assert_eq!(sent, vec!["down".to_string()], "只发过一个 down：{sent:?}");
    }

    // ===== 数字直选自适应（2026-10-03 用户设计）=====

    /// **探测成功**（Unknown + 数字翻转）：键序 = ["2"]（数字直发，无走位无空格）；
    /// outcome 带 digit_result=Supported（调用侧写回表）。屏序列：数字后目标行翻转
    ///（脚本同屏翻转——驱动器推进但屏内容按翻转给）。
    /// 还原动作（变异）：删掉数字探测段 → 键序走 down+space、本用例先红。
    #[test]
    fn toggle_digit_probe_supported_on_first_try() {
        let (r, sent) = run_toggle_script(
            0,
            FIXTURE_QUESTION,
            vec![
                multi_screen_focus_at(0, false), // 首段：选项 1 未勾
                multi_screen_focus_at(0, true),  // 数字后：选项 1 翻转（焦点不动）
            ],
            crate::inject::capability::DigitToggle::Unknown,
        );
        let out = r.expect("数字直选必须成功");
        assert_eq!(sent, vec!["1".to_string()], "键序 = [数字]：{sent:?}");
        assert!(out.verified);
        assert_eq!(
            out.digit_result,
            Some(crate::inject::capability::DigitToggle::Supported)
        );
    }

    /// **探测失败回退走位**（Unknown + 数字完全无效）：屏面无变化 → 记 Unsupported
    /// 并回退走位+空格完成本次。键序 = ["2", down, space]。
    /// 还原动作（变异）：删掉回退段 → 中止、本用例先红。
    #[test]
    fn toggle_digit_probe_falls_back_to_walk_on_no_change() {
        let (r, sent) = run_toggle_script(
            0,
            FIXTURE_QUESTION,
            vec![
                multi_screen_focus_at(0, false), // idx0 首段
                multi_screen_focus_at(0, false), // idx1 数字后：完全无变化（无效形态）
                multi_screen_focus_at(0, true),  // idx2 space → 翻转（焦点本就在选项 1，零走位）
            ],
            crate::inject::capability::DigitToggle::Unknown,
        );
        let out = r.expect("回退走位必须完成");
        assert!(out.verified);
        assert_eq!(
            out.digit_result,
            Some(crate::inject::capability::DigitToggle::Unsupported)
        );
        assert_eq!(
            sent,
            vec!["1".to_string(), "space".to_string()],
            "键序 = [数字(无效), space]（零走位）：{sent:?}"
        );
    }

    /// **数字被半消费 = 异常中止**（用户拍板）：屏面有变化但目标未翻转（这里模拟
    /// 焦点被移走）→ 中止，不回退走位。
    #[test]
    fn toggle_digit_half_consumed_aborts() {
        let (r, sent) = run_toggle_script(
            0,
            FIXTURE_QUESTION,
            vec![
                multi_screen_focus_at(0, false), // 首段
                multi_screen_focus_at(1, false), // 数字后：焦点跳到选项 2（半消费）且选项 1 未翻转
            ],
            crate::inject::capability::DigitToggle::Unknown,
        );
        let err = r.expect_err("半消费必须中止");
        assert!(err.message.contains("半消费"), "{err}");
        assert_eq!(sent, vec!["1".to_string()]);
    }

    // ===== toggle 数字直选·焦点守卫（2026-10-03 用户实测 bug）=====

    /// **先决生效**：焦点停在 Type something 行（内联编辑态）→ 先 `↑` 移出（核验
    /// 焦点回选项行 + 文字保留）→ 数字直选翻转。键序 = ["up", "1"]。
    /// 还原动作（变异）：删掉焦点守卫 → 数字被当文本吃进输入框、本用例先红。
    #[test]
    fn toggle_digit_prelifts_from_freetext_focus() {
        let (r, sent) = run_toggle_script(
            0,
            "这次要优化哪几部作品？",
            vec![
                // idx0 焦点在 TS 行（已打字）
                live_fixtures::q1_typed_at("融合优点", false, 37),
                // idx1 ↑ 后：焦点移回选项 1、TS 行文字保留
                live_fixtures::q1_typed_at("融合优点", false, 29),
                // idx2 数字后：选项 1 翻转 [✔]、焦点仍在选项 1、TS 行文字保留
                live_fixtures::q1_typed_at("融合优点", true, 29)
                    .into_iter()
                    .enumerate()
                    .map(|(i, l)| {
                        if i == 29 {
                            l.replace("[ ]", "[✔]")
                        } else {
                            l
                        }
                    })
                    .collect::<Vec<_>>(),
            ],
            crate::inject::capability::DigitToggle::Unknown,
        );
        let out = r.expect("先决后数字直选必须成功");
        assert_eq!(
            sent,
            vec!["up".to_string(), "1".to_string()],
            "键序 = [↑, 数字]：{sent:?}"
        );
        assert!(out.verified);
    }

    /// **防御锁**：↑ 后自由作答行内容丢失（文字被弄丢的故障形态）→ 中止不发数字。
    #[test]
    fn toggle_digit_prelift_aborts_when_freetext_lost() {
        let (r, sent) = run_toggle_script(
            0,
            "这次要优化哪几部作品？",
            vec![
                live_fixtures::q1_typed_at("融合优点", false, 37), // 焦点在 TS 行
                live_fixtures::q1_focus_at(0), // ↑ 后：文字丢了（占位恢复的故障形态）
            ],
            crate::inject::capability::DigitToggle::Unknown,
        );
        let err = r.expect_err("文字丢失必须中止");
        assert!(err.message.contains("内容变化"), "{err}");
        assert_eq!(sent, vec!["up".to_string()]);
    }

    // ===== FreeText 定位判据修正（2026-10-03 单选形态实证）=====

    /// **单选打字后行判 FreeText**（用户批评 + 单选形态实证）：`5. 组成一个HTML`
    ///（✔ 在行尾、无方括号）——「编号最大」结构规则不再依赖勾选框字形。
    /// 还原动作（变异）：kind 规则加回 `checked.is_some()` → 本用例先红。
    #[test]
    fn parse_single_select_typed_row_is_freetext() {
        // 单选子题屏：选项 1-3 + 打字后的第 4 行（无勾选框、无占位前缀）
        let screen = lines(&[
            "←  ☐ 交付方式  ✔ Submit  →",
            "",
            "问题4：优化结果怎么交付？（单选）",
            "",
            "❯ 1. 另存新文件",
            "  2. 直接改原文件",
            "  3. 先审后写",
            "  4. 组成一个HTML",
            "    ",
            "────────────────",
            "  5. Chat about this",
        ]);
        let rows = parse_question_rows(&screen);
        assert_eq!(
            rows.len(),
            4,
            "4 行块：3 选项 + 打字后的 FreeText：{rows:?}"
        );
        assert_eq!(
            rows[3].kind,
            QuestionRowKind::FreeText,
            "编号最大行（打字后形态）= FreeText：{rows:?}"
        );
        assert_eq!(rows[3].label, "组成一个HTML");
    }

    /// **定位稳定**（占位 ↔ 打字同屏切换）：同一结构下两种形态的 FreeText 行位置
    ///（行块下标）一致——编排的走位/核验按行号回看不漂移。
    #[test]
    fn freetext_row_position_stable_across_placeholder_and_typed() {
        let placeholder = parse_question_rows(&live_fixtures::q2_single());
        let typed = parse_question_rows(&live_fixtures::q2_typed_single("走温暖路线"));
        let p_pos = placeholder
            .iter()
            .position(|r| r.kind == QuestionRowKind::FreeText);
        let t_pos = typed
            .iter()
            .position(|r| r.kind == QuestionRowKind::FreeText);
        assert_eq!(p_pos, t_pos, "两种形态的 FreeText 行位置一致");
        assert_eq!(typed[t_pos.unwrap()].label, "走温暖路线");
    }

    /// **确认项文案 sanity check**：定位到的行 label 恰为 Review 确认项文案 →
    /// 中止拒绝动手（结构定位的误判面收口）。
    #[test]
    fn multi_free_text_aborts_on_confirm_item_label() {
        // 屏 = Review 确认屏（编号最大行 = Cancel）——绕过快照前置直接喂编排
        let (r, sent) =
            run_multi_free_text_script("文字", false, vec![live_fixtures::review_unanswered()]);
        let err = r.expect_err("确认项文案必须中止");
        assert!(err.message.contains("确认项"), "{err}");
        assert!(sent.is_empty(), "零按键：{sent:?}");
    }

    // ===== 单选 select 焦点守卫（2026-10-03 用户实测 bug）=====

    /// **单选 select 驱动器**（守卫→数字，键序断言用）。
    fn run_select_script(
        index: usize,
        screens: Vec<Vec<String>>,
    ) -> (Result<Vec<String>, StageAbort>, Vec<String>) {
        use std::cell::{Cell, RefCell};
        let n = screens.len();
        let cursor = Cell::new(0usize);
        let cur = || screens[cursor.get().min(n - 1)].clone();
        let sent: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let result = run_select_stages(
            index,
            || Ok(Some(cur())),
            &mut crate::inject::mode::Closures {
                read: || Some(cur()),
                send: |k: &str| {
                    sent.borrow_mut().push(k.to_string());
                    if cursor.get() + 1 < n {
                        cursor.set(cursor.get() + 1);
                    }
                    Ok(())
                },
                settle: || {},
            },
        );
        (result, sent.into_inner())
    }

    /// **守卫生效**：焦点停在 Type something 行 → 先 `↑` 移出（文字保留核验）→
    /// 数字直答。键序 = ["up", "2"]（index=1 → 屏上编号 2）。
    /// 还原动作（变异）：删掉守卫调用 → 键序只有数字、本用例先红。
    #[test]
    fn select_prelifts_from_freetext_focus() {
        let (r, _sent) = run_select_script(
            1,
            vec![
                // idx0 焦点在 TS 行（已打字）
                live_fixtures::q1_typed_at("融合优点", false, 37),
                // idx1 ↑ 后：焦点到选项 2、TS 行文字保留
                live_fixtures::q1_typed_at("融合优点", false, 31),
                // idx2 数字后：焦点仍在选项 2、TS 行保留
                live_fixtures::q1_typed_at("融合优点", false, 31),
            ],
        );
        let keys = r.expect("守卫后数字直答必须成功");
        assert_eq!(
            keys,
            vec!["up".to_string(), "2".to_string()],
            "键序 = [↑, 数字]：{keys:?}"
        );
    }

    // ===== 单题集 · 单页签形态（2026-10-08 活体 2.1.287，0.5.1 报障修复）=====

    /// **解析识别**：单页签屏（无推进行、无 ←/→ Submit 复合签名）必须出 4 行块
    /// ——3 选项 + Type something（分隔线天然排除 Chat about this）；单选形态
    /// 全行无勾选框；焦点唯一在选项 1。还原动作（变异）：撤掉
    /// [`is_single_question_tab_line`] 锚 → 解析回 0 行、本用例先红。
    #[test]
    fn parses_single_question_tab_screen() {
        let rows = parse_question_rows(&live_fixtures::q_single_tab());
        assert_eq!(
            rows.len(),
            4,
            "3 选项 + FreeText，Chat 被分隔线排除：{rows:?}"
        );
        assert!(
            rows.iter().all(|r| r.checked.is_none()),
            "单选形态无勾选框：{rows:?}"
        );
        assert!(
            rows.iter().all(|r| r.kind != QuestionRowKind::Advance),
            "无推进行：{rows:?}"
        );
        assert_eq!(rows[3].kind, QuestionRowKind::FreeText);
        assert_eq!(rows[3].label, "Type something.");
        assert_eq!(
            rows.iter().filter(|r| r.focused).count(),
            1,
            "焦点唯一：{rows:?}"
        );
        assert!(rows[0].focused, "焦点在选项 1：{rows:?}");
    }

    /// **编排直答**：单页签屏上 select = 纯数字（焦点在选项行，守卫不触发）。
    #[test]
    fn select_on_single_tab_screen_sends_digit_only() {
        let (r, sent) = run_select_script(2, vec![live_fixtures::q_single_tab()]);
        let keys = r.expect("单页签屏 select 必须成功");
        assert_eq!(keys, vec!["3".to_string()], "键序 = [数字]：{keys:?}");
        assert_eq!(sent, vec!["3".to_string()]);
    }

    /// **快照对位**：题干提取不含页签头（单页签行视作页签栏）；自由作答行在场、
    /// 占位态；勾选态三行全 None。
    #[test]
    fn single_tab_screen_snapshot_alignment() {
        let heading = question_heading_of(&live_fixtures::q_single_tab());
        assert!(
            heading.contains("Whichdatabaseshouldweuse?"),
            "题干含问句：{heading}"
        );
        assert!(!heading.contains("Deploy"), "页签头不混入题干：{heading}");
        let snap = question_screen_snapshot(&live_fixtures::q_single_tab()).expect("题屏必出快照");
        assert_eq!(snap.checked, vec![None, None, None]);
        assert!(snap.free_text_present);
        assert_eq!(snap.free_text, None);
    }

    /// **负回归（评审 P1-3 修正）**：**行首** ☐ 的散文行（下方无编号块）不得
    /// 劫持解析——锚命中但块空时仍返回空（与多题页签栏同语义）。☐ 必须在行首
    /// 才踩中 [`is_single_question_tab_line`] 谓词（行中 ☐ 由「无锚 → 空」兜住，
    /// 不是本用例的威胁模型）。
    #[test]
    fn stray_ballot_prose_line_yields_empty_block() {
        let lines: Vec<String> = vec![
            "☐ 待办：修弹窗（散文里冒出的页签形行）".to_string(),
            "正文继续，无任何编号行".to_string(),
        ];
        assert!(parse_question_rows(&lines).is_empty());
    }

    /// **滚回区劫持防御（评审 P1-2）**：滚回区散落 ☐ 起头行 + 分隔线 + 下方真
    /// 单页签——兜底取**最靠后**（rposition），必须收出真题屏 4 行块。锚 A 的
    /// 2026-10-02「取第一处被滚回区杂锚劫持」事故类对新锚的复刻锁（变异：改回
    /// position → 本用例先红：杂行命中、分隔线 break、块空 0 行）。
    #[test]
    fn single_tab_anchor_survives_scrollback_ballot_line() {
        let mut lines = vec![
            "☐ 待办：某轮旧对话冒出的杂行".to_string(),
            "正文继续".to_string(),
            "──────────────────────────────────────────────".to_string(),
        ];
        lines.extend(live_fixtures::q_single_tab());
        let rows = parse_question_rows(&lines);
        assert_eq!(rows.len(), 4, "rposition 取真题屏页签：{rows:?}");
        assert_eq!(rows[0].label, "Postgres", "选项来自真题屏：{rows:?}");
    }

    /// **Review 确认屏防线（评审 P1-1）**：单题集答毕后的 Review 屏（☒ 单页签 +
    /// Submit answers/Cancel 确认项）——select 编排必须零键中止。漂移场景：终端
    /// 已答完、手机卡再点选项，数字打进确认屏=替用户提交。形态为**派生**（单题
    /// Review 屏未活体取证——字形族按多题 Review 夹具投影，证据缺口已申报屏读
    /// 矩阵；防线判据 [`probe_review_screen`] 是既有实测定案，本用例锁的是防线
    /// 在 select 路径的前置位）。
    #[test]
    fn select_aborts_zero_keys_on_review_screen() {
        let review = vec![
            " ☒ Deploy".to_string(),
            String::new(),
            "Review your answers".to_string(),
            String::new(),
            "Ready to submit your answers?".to_string(),
            String::new(),
            "❯ 1. Submit answers".to_string(),
            "  2. Cancel".to_string(),
        ];
        let (r, sent) = run_select_script(0, vec![review]);
        let err = r.expect_err("Review 屏必须零键中止");
        assert!(err.message.contains("Review"), "{err}");
        assert!(sent.is_empty(), "零按键：{sent:?}");
    }

    /// **身份闸长题干（评审 P2-1）**：单页签屏题干折行 >4 行时，tab 判据不同步
    /// 会让身份区回落「首编号行上方 4 行」→ 题干头部丢失 → 假阴性中止。期望串
    /// 取**首个**题干行（旧判据下恰在 4 行窗外，变异可测）。
    #[test]
    fn single_tab_identity_gate_handles_long_heading() {
        let mut lines = vec![" ☐ Deploy".to_string(), String::new()];
        lines.extend(
            [
                "这是一个特别长的题干第一行，用来撑爆四行回落窗口的覆盖范围",
                "第二行继续补充题干内容确保超过四行的回看窗口",
                "第三行还在继续写题干内容让窗口真的不够用",
                "第四行题干内容继续加长确保回落窗口不敷使用",
                "第五行结尾：新建会话页面最上面的标签内容用什么？",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        lines.push(String::new());
        lines.push("❯ 1. 选项一".to_string());
        lines.push("  2. 选项二".to_string());
        assert!(screen_question_matches(
            &lines,
            "这是一个特别长的题干第一行，用来撑爆四行回落窗口的覆盖范围"
        ));
    }

    /// **焦点不在 TS 行** → 直接数字（守卫不触发，零额外键）。
    #[test]
    fn select_no_prelift_when_focus_on_option() {
        let (r, _sent) = run_select_script(
            0,
            vec![
                live_fixtures::q1_focus_at(0), // 焦点在选项 1
                live_fixtures::q1_focus_at(0), // 数字后
            ],
        );
        let keys = r.expect("直接数字必须成功");
        assert_eq!(keys, vec!["1".to_string()], "零额外键：{keys:?}");
    }

    /// **快照剥行尾选中标记**（2026-10-03 用户实测）：单选屏打字后的行
    /// `4. 组成一个HTML ✔`——快照 freeText 应为「组成一个HTML」（标记不是文字）。
    /// 还原动作（变异）：strip_trailing_selection_mark 删掉 → freeText 带尾勾、
    /// 本用例先红。
    #[test]
    fn snapshot_free_text_strips_trailing_selection_mark() {
        let screen = lines(&[
            "←  ☐ 交付方式  ✔ Submit  →",
            "",
            "问题4：优化结果怎么交付？（单选）",
            "",
            "❯ 1. 另存新文件",
            "  2. 直接改原文件",
            "  3. 先审后写",
            "❯ 4. 组成一个HTML ✔",
            "    ",
            "────────────────",
            "  5. Chat about this",
        ]);
        let snap = question_screen_snapshot(&screen).expect("题屏必出快照");
        assert!(snap.free_text_present);
        assert_eq!(
            snap.free_text.as_deref(),
            Some("组成一个HTML"),
            "行尾选中标记必须剥离"
        );
    }

    /// **评审 Minor-4 回归锁**：对话滚回区的 markdown 水平线（ASCII `----`）不是
    /// 题屏分隔线——制表符判定收紧后，`----` 上方的选项行照常进块（旧谓词会把
    /// 后续行误判到分隔线下而丢块）。
    #[test]
    fn parse_rows_ignores_ascii_markdown_rule_as_separator() {
        let screen = lines(&[
            " Some prose above",
            " ----",
            " ← ☒ Favorite fruits  ✔Submit  →",
            "",
            " Which fruits are your favorites? (Select all that apply)",
            "",
            " ❯ 1. [ ] Apple",
            " 2. [ ] Banana",
            " 3. [ ] Peach",
            "    Submit",
            " ────────────────",
            " 4. Chat about this",
        ]);
        let rows = parse_question_rows(&screen);
        assert_eq!(
            rows.len(),
            4,
            "ASCII ---- 不触发分隔判定，选项行照常进块：{rows:?}"
        );
        assert_eq!(rows[3].kind, QuestionRowKind::Advance);
        // 真·制表符分隔线（─）依然生效：Chat 行仍在块外
        assert!(!rows.iter().any(|r| r.label.contains("Chat")));
        assert!(is_separator_line("────────"));
        assert!(!is_separator_line("----"), "ASCII 水平线不算题屏分隔线");
    }

    /// **评审 I2 回归锁（2026-09-26）+ 2026-10-02 活体扩面**：切题首段轮询的「屏已
    /// 可行动」判据——Review 屏、带推进行的题屏、**单选子题题屏（页签栏锚）**都放行；
    /// 中绘屏/杂屏不放行（生产轮询缝据此把中绘屏挡在轮询窗里等形态）。
    /// 还原动作（变异）：删掉任一分支 → 对应断言先红。
    #[test]
    fn advance_stage_screen_ready_gates_mid_draw_only() {
        assert!(
            advance_stage_screen_ready(&review_screen_real()),
            "Review 屏 = 已可行动（零按键 advanced:false 的入口）"
        );
        assert!(
            advance_stage_screen_ready(&multi_submit_screen_focus_on_submit()),
            "带 Submit 推进行的题屏 = 已可行动"
        );
        assert!(
            advance_stage_screen_ready(&live_fixtures::q2_single()),
            "单选子题题屏（页签栏锚，无推进行）= 已可行动（2026-10-02 活体）"
        );
        assert!(
            !advance_stage_screen_ready(&lines(&["(TUI 重绘中……)", ""])),
            "中绘屏不放行——留在轮询窗里等形态"
        );
    }

    // ===== 2026-10-02 活体黄金夹具内核锁（fixtures = live_fixtures，逐字取证）=====

    /// 活体 q1（多选子题，含滚回区 Planning 分隔线）：**闩锁移除**的回归锁——
    /// 旧实现见滚回区分隔线即判死其后全部行，解析恒 0（验收事故主根因）。
    /// 还原动作（变异）：恢复 below_separator 闩锁 → 本用例先红。
    #[test]
    fn parse_live_q1_multi_full_block() {
        let rows = parse_question_rows(&live_fixtures::q1_multi());
        assert_eq!(
            rows.len(),
            6,
            "6 行块：4 选项 + Type something + Next：{rows:?}"
        );
        assert_eq!(rows[0].kind, QuestionRowKind::Option);
        assert_eq!(rows[0].checked, Some(false), "多选形态带勾选框");
        assert!(rows[0].focused, "❯ 在选项 1");
        assert_eq!(rows[4].kind, QuestionRowKind::FreeText);
        assert_eq!(rows[5].kind, QuestionRowKind::Advance);
        assert!(
            !rows.iter().any(|r| r.label.contains("Chat")),
            "Chat about this 不进块"
        );
    }

    /// 活体 q2（**单选子题**，无独立推进行）：页签栏锚 B 的回归锁——旧解析恒 0 行。
    /// 还原动作（变异）：删掉锚 B → 本用例先红（0 行）。
    #[test]
    fn parse_live_q2_single_via_tab_bar_anchor() {
        let rows = parse_question_rows(&live_fixtures::q2_single());
        assert_eq!(
            rows.len(),
            5,
            "5 行块：4 选项 + Type something，**无 Advance**：{rows:?}"
        );
        assert_eq!(rows[0].number, Some(1));
        assert_eq!(rows[0].checked, None, "单选形态无勾选框");
        assert!(rows[0].focused);
        assert_eq!(rows[4].kind, QuestionRowKind::FreeText);
        assert!(
            !rows.iter().any(|r| r.kind == QuestionRowKind::Advance),
            "单选子题没有推进行——页签栏锚产物不含 Advance"
        );
    }

    /// 活体 Review 屏（未答完直达）：行块解析出确认项编号块（1=Submit answers、
    /// 2=Cancel），且既有 Review 探测照常 Ready——两套判据在同一块真实屏上并存。
    #[test]
    fn parse_live_review_and_probe_agree() {
        let screen = live_fixtures::review_unanswered();
        let rows = parse_question_rows(&screen);
        assert_eq!(rows.len(), 2, "确认项 1/2 进块：{rows:?}");
        assert_eq!(rows[0].label, "Submit answers");
        assert!(
            matches!(probe_review_screen(&screen), ScreenStep::Ready(_)),
            "Review 探测照常 Ready（标题锚 + 确认项恰好一行）"
        );
    }

    /// 切勾落在单选子题屏（卡片停在多选 q1、终端在单选 q2 的实机脱钩形态）：
    /// 身份闸**先**拦下（题干不一致，零键）——而不是「没有勾选框」这种次级中止。
    #[test]
    fn toggle_on_live_q2_with_q1_payload_identity_aborts() {
        let (r, sent) = run_toggle_script(
            0,
            "这次要优化哪几部作品？",
            vec![live_fixtures::q2_single()],
            crate::inject::capability::DigitToggle::Unsupported,
        );
        let err = r.expect_err("题干不一致必须中止");
        assert!(err.message.contains("手机卡片不一致"), "{err}");
        assert!(sent.is_empty(), "零按键：{sent:?}");
    }

    /// 切勾落在**自己题**的单选子题屏（身份一致）→ 诚实中止「没有勾选框」，
    /// 零键——toggle 仅适用多选形态。
    #[test]
    fn toggle_on_own_single_select_aborts_no_checkbox() {
        let (r, sent) = run_toggle_script(
            0,
            "《末班车》目前有4个版本（原版/优化版/精简版/情感救赎版），如何处理这些版本？",
            vec![live_fixtures::q2_single()],
            crate::inject::capability::DigitToggle::Unsupported,
        );
        let err = r.expect_err("单选形态必须中止");
        assert!(err.message.contains("没有勾选框"), "{err}");
        assert!(sent.is_empty(), "零按键：{sent:?}");
    }

    /// **单选子题切题·末题直达 Review**（活体取证步骤 1 的脚本化复刻）：
    /// 焦点在选项行 → 发 `→` → Review 屏在场 → advanced=true。键序 = ["right"]。
    /// 还原动作（变异）：导航分支改回走位+回车 → 键序断言先红。
    #[test]
    fn advance_single_select_right_lands_review() {
        let (r, sent) = run_advance_script(
            vec![
                live_fixtures::q2_single(),
                live_fixtures::review_unanswered(),
            ],
            NavDirection::Next,
        );
        let out = r.expect("→ 直达 Review 必须判已切");
        assert!(out.advanced);
        assert_eq!(sent, vec!["right".to_string()], "键序 = [→]：{sent:?}");
    }

    /// **单选子题切题·中间题**（`→` 切到另一道单选子题）：题干区变化 → advanced。
    /// 屏 2 = q2 夹具派生（题干替换为伪 q3；活体只到 2 题，派生已申报）。
    #[test]
    fn advance_single_select_right_to_next_question() {
        let (r, sent) = run_advance_script(
            vec![live_fixtures::q2_single(), live_fixtures::q3_derived()],
            NavDirection::Next,
        );
        let out = r.expect("题干变化必须判已切");
        assert!(out.advanced);
        assert_eq!(sent, vec!["right".to_string()]);
    }

    /// **单选子题切题·`→` 被吞**（屏面与题干均未变）→ 中止无后续键。
    #[test]
    fn advance_single_select_noop_aborts() {
        let (r, sent) = run_advance_script(
            vec![live_fixtures::q2_single(), live_fixtures::q2_single()],
            NavDirection::Next,
        );
        let err = r.expect_err("屏面未变必须中止");
        assert!(err.message.contains("既未进入下一题页"), "{err}");
        assert_eq!(sent, vec!["right".to_string()], "只发过一个 →：{sent:?}");
    }

    /// **单选子题切题·焦点在自由作答行**：先 `↑` 回选项行（两行例外先决）再 `→`。
    /// 屏 1 = q2 派生（❯ 移到 Type something 行）。
    #[test]
    fn advance_single_select_prelifts_from_freetext() {
        let (r, sent) = run_advance_script(
            vec![
                live_fixtures::q2_focus_on_freetext(),
                live_fixtures::q2_single(),
                live_fixtures::review_unanswered(),
            ],
            NavDirection::Next,
        );
        let out = r.expect("↑ 先决后 → 必须走通");
        assert!(out.advanced);
        assert_eq!(
            sent,
            vec!["up".to_string(), "right".to_string()],
            "{sent:?}"
        );
    }

    /// **多选子题 → 单选子题**（验收事故图 2 的实机链路）：q1 发 `→` → q2
    ///（页签栏锚 + 题干变化）→ advanced=true——←/→ 通用导航（走位+回车退役），
    /// 旧行块解析在这里恒 0 行中止。
    /// 还原动作（变异）：恢复闩锁或删锚 B → 分类段中止、本用例先红。
    #[test]
    fn advance_next_path_multi_into_single_select() {
        let (r, sent) = run_advance_script(
            vec![live_fixtures::q1_multi(), live_fixtures::q2_single()],
            NavDirection::Next,
        );
        let out = r.expect("→ 进单选子题必须判已切");
        assert!(out.advanced);
        assert_eq!(sent, vec!["right".to_string()], "键序 = [→]：{sent:?}");
    }

    /// **上一题·题页间**（用户裁决 ←/→ 通用）：q2 发 `←` → q1（题干区变化）。
    /// 还原动作（变异）：删掉 Prev 分支 → 本用例先红。
    #[test]
    fn advance_prev_question_page_to_previous() {
        let (r, sent) = run_advance_script(
            vec![live_fixtures::q2_single(), live_fixtures::q1_multi()],
            NavDirection::Prev,
        );
        let out = r.expect("← 退回上一题必须判已切");
        assert!(out.advanced);
        assert_eq!(sent, vec!["left".to_string()], "键序 = [←]：{sent:?}");
    }

    /// **上一题·从确认屏退回**（活体取证步骤 2 复刻）：Review 屏发 `←` → 退回题页。
    /// 还原动作（变异）：Prev 在 Review 屏改回零按键 → 本用例先红。
    #[test]
    fn advance_prev_from_review_returns_to_question() {
        let (r, sent) = run_advance_script(
            vec![
                live_fixtures::review_unanswered(),
                live_fixtures::q2_single(),
            ],
            NavDirection::Prev,
        );
        let out = r.expect("← 从 Review 退回必须判已切");
        assert!(out.advanced);
        assert_eq!(sent, vec!["left".to_string()], "键序 = [←]：{sent:?}");
    }

    /// **上一题·`←` 被吞**（Review 屏仍在）→ 中止无后续键。
    #[test]
    fn advance_prev_from_review_swallowed_aborts() {
        let (r, sent) = run_advance_script(
            vec![
                live_fixtures::review_unanswered(),
                live_fixtures::review_unanswered(),
            ],
            NavDirection::Prev,
        );
        let err = r.expect_err("← 被吞必须中止");
        assert!(err.message.contains("未从确认屏退回"), "{err}");
        assert_eq!(sent, vec!["left".to_string()], "只发过一个 ←：{sent:?}");
    }

    /// **上一题·题页间被吞/已是第一题**（屏面与题干均未变）→ 中止无后续键。
    #[test]
    fn advance_prev_noop_aborts() {
        let (r, sent) = run_advance_script(
            vec![live_fixtures::q2_single(), live_fixtures::q2_single()],
            NavDirection::Prev,
        );
        let err = r.expect_err("屏面未变必须中止");
        assert!(err.message.contains("未切换题目"), "{err}");
        assert_eq!(sent, vec!["left".to_string()], "只发过一个 ←：{sent:?}");
    }

    /// **上一题·焦点在自由作答行**：先 `↑` 回选项行（两行例外先决）再 `←`。
    #[test]
    fn advance_prev_prelifts_from_freetext() {
        let (r, sent) = run_advance_script(
            vec![
                live_fixtures::q2_focus_on_freetext(),
                live_fixtures::q2_single(),
                live_fixtures::q1_multi(),
            ],
            NavDirection::Prev,
        );
        let out = r.expect("↑ 先决后 ← 必须走通");
        assert!(out.advanced);
        assert_eq!(sent, vec!["up".to_string(), "left".to_string()], "{sent:?}");
    }

    // ===== 多选自由作答编排（2026-10-02 第三批 + 勾选兜底/编辑覆盖批）=====

    /// 多选自由作答脚本驱动器（send/send_text 都推进屏序列）。
    fn run_multi_free_text_script(
        text: &str,
        overwrite: bool,
        screens: Vec<Vec<String>>,
    ) -> (Result<FreeTextOutcome, StageAbort>, Vec<String>) {
        use std::cell::{Cell, RefCell};
        let n = screens.len();
        let cursor = Cell::new(0usize);
        let cur = || screens[cursor.get().min(n - 1)].clone();
        let sent: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let result = run_multi_select_free_text_stages(
            text,
            overwrite,
            || Ok(Some(cur())),
            &mut FreeTextClosures {
                read: || Some(cur()),
                send: |k: &str| {
                    sent.borrow_mut().push(k.to_string());
                    if cursor.get() + 1 < n {
                        cursor.set(cursor.get() + 1);
                    }
                    Ok(())
                },
                send_text: |t: &str| {
                    sent.borrow_mut().push(format!("<text:{t}>"));
                    if cursor.get() + 1 < n {
                        cursor.set(cursor.get() + 1);
                    }
                    Ok(())
                },
                settle: || {},
            },
        );
        (result, sent.into_inner())
    }

    /// **勾选兜底**（用户实测：打字后默认未勾选）：打字后屏 `[ ] hi` → 补 space →
    /// 核验屏 `[✔] hi`。键序 = down×4 + text + space（**无 enter**）。
    /// 还原动作（变异）：删掉勾选兜底段 → 键序无 space、本用例先红。
    #[test]
    fn multi_free_text_checks_the_box_after_typing() {
        let (r, sent) = run_multi_free_text_script(
            "hi",
            false,
            vec![
                live_fixtures::q1_multi(),
                live_fixtures::q1_focus_at(1),
                live_fixtures::q1_focus_at(2),
                live_fixtures::q1_focus_at(3),
                live_fixtures::q1_focus_at(4),
                live_fixtures::q1_typed_flagged("hi", false), // 打字后未勾
                live_fixtures::q1_typed_flagged("hi", true),  // 空格补勾后
            ],
        );
        let out = r.expect("勾选兜底后必须成功");
        assert_eq!(out.receipt_seen, None);
        assert_eq!(
            sent,
            vec![
                "down".to_string(),
                "down".to_string(),
                "down".to_string(),
                "down".to_string(),
                "<text:hi>".to_string(),
                "space".to_string(),
            ],
            "键序 = down×4 + text + space（无 enter）：{sent:?}"
        );
    }

    /// **已勾选跳过**：打字后屏直接 `[✔] hi` → 不发 space。
    #[test]
    fn multi_free_text_skips_checkbox_when_already_checked() {
        let (r, sent) = run_multi_free_text_script(
            "hi",
            false,
            vec![
                live_fixtures::q1_multi(),
                live_fixtures::q1_focus_at(1),
                live_fixtures::q1_focus_at(2),
                live_fixtures::q1_focus_at(3),
                live_fixtures::q1_focus_at(4),
                live_fixtures::q1_typed("hi"), // 已勾
            ],
        );
        r.expect("已勾选路径必须成功");
        assert!(
            !sent.contains(&"space".to_string()),
            "已勾选不得补空格：{sent:?}"
        );
    }

    /// **清空模式**（空文本 + overwrite）：走位 → 右移×3 + 退格×3 → 占位恢复 →
    /// **跳过打字**直接成功（不发数字不发 space）。键序 = down×4 + right×3 +
    /// backspace×3。
    /// 还原动作（变异）：删掉 clearing_only 早退 → 走打字路径、本用例先红。
    #[test]
    fn multi_free_text_clearing_only_mode() {
        let (r, sent) = run_multi_free_text_script(
            "",
            true,
            vec![
                live_fixtures::q1_typed_at("旧内容", true, 29), // idx0 首段：已有内容
                live_fixtures::q1_typed_at("旧内容", true, 31),
                live_fixtures::q1_typed_at("旧内容", true, 33),
                live_fixtures::q1_typed_at("旧内容", true, 35),
                live_fixtures::q1_typed_at("旧内容", true, 37), // 走位到该行；轮1 读 len=3
                live_fixtures::q1_typed_at("旧内容", true, 37), // right1
                live_fixtures::q1_typed_at("旧内容", true, 37), // right2
                live_fixtures::q1_typed_at("旧内容", true, 37), // right3
                live_fixtures::q1_typed_at("旧", true, 37),     // backspace1
                live_fixtures::q1_typed_at("旧", true, 37),     // backspace2
                live_fixtures::q1_typed_at("Type something", false, 37), // backspace3 → 占位 ✓
            ],
        );
        let out = r.expect("清空模式必须成功");
        assert!(out.receipt_seen.is_none());
        assert!(
            !sent.contains(&"<text:0 chars>".to_string()),
            "清空模式不打字"
        );
        assert_eq!(
            sent.last(),
            Some(&"backspace".to_string()),
            "收尾 = 退格（无打字无数字）：{sent:?}"
        );
    }

    /// **清空幂等**（行已是占位）：零键成功。
    #[test]
    fn multi_free_text_clearing_idempotent_on_placeholder() {
        // 焦点已在 TS 行（占位）：零走位零清空零打字
        let (r, sent) = run_multi_free_text_script(
            "",
            true,
            vec![live_fixtures::q1_typed_at("Type something", false, 37)],
        );
        let out = r.expect("占位行清空幂等成功");
        assert!(sent.is_empty(), "零按键：{sent:?}");
        assert!(out.receipt_seen.is_none());
    }

    /// **单选子题自由作答**（2026-10-03 用户需求：多题流单选子题也有 Type
    /// something 行——复用同一编排）：q2_single（单选形态，行无勾选框）→ 走位 →
    /// 打字 → label 翻转核验通过；**勾选兜底跳过**（checked=None = 单选形态）。
    /// 键序 = down×4 + text，无 space 无 enter。
    /// 还原动作（变异）：恢复「checked.is_none() 即中止」→ 本用例先红。
    #[test]
    fn multi_free_text_on_single_select_subquestion() {
        let (r, sent) = run_multi_free_text_script(
            "走温暖路线",
            false,
            vec![
                live_fixtures::q2_single(),                   // idx0 首段：焦点在选项 1
                live_fixtures::q2_focus_line(32),             // idx1 down → 选项 2
                live_fixtures::q2_focus_line(34),             // idx2 down → 选项 3
                live_fixtures::q2_focus_line(36),             // idx3 down → 选项 4
                live_fixtures::q2_focus_line(38),             // idx4 down → Type something 行
                live_fixtures::q2_typed_single("走温暖路线"), // idx5 打字后（label 翻转）
            ],
        );
        let out = r.expect("单选子题自由作答必须成功");
        assert_eq!(out.receipt_seen, None);
        assert_eq!(
            sent,
            vec![
                "down".to_string(),
                "down".to_string(),
                "down".to_string(),
                "down".to_string(),
                "<text:走温暖路线>".to_string(),
            ],
            "键序 = down×4 + text（无 space——单选形态无勾选框；无 enter）：{sent:?}"
        );
    }

    /// **编辑覆盖**（overwrite=true）：已有内容 → 走位 → 退格×3（「旧内容」3 字）→
    /// 占位屏 → 打新字 → 未勾 → space 兜底。
    /// 还原动作（变异）：删掉退格段 → 键序无 backspace、本用例先红。
    #[test]
    fn multi_free_text_edit_overwrites_with_backspaces() {
        let (r, sent) = run_multi_free_text_script(
            "新内容",
            true,
            vec![
                live_fixtures::q1_typed_at("旧内容", true, 29), // idx0 首段：焦点在选项 1
                live_fixtures::q1_typed_at("旧内容", true, 31), // idx1 down
                live_fixtures::q1_typed_at("旧内容", true, 33), // idx2 down
                live_fixtures::q1_typed_at("旧内容", true, 35), // idx3 down
                live_fixtures::q1_typed_at("旧内容", true, 37), // idx4 down；轮1 读 len=3
                live_fixtures::q1_typed_at("旧内容", true, 37), // idx5 right1（屏不变）
                live_fixtures::q1_typed_at("旧内容", true, 37), // idx6 right2（屏不变）
                live_fixtures::q1_typed_at("旧内容", true, 37), // idx7 right3（光标到行尾）
                live_fixtures::q1_typed_at("旧", true, 37),     // idx8 backspace1
                live_fixtures::q1_typed_at("旧", true, 37),     // idx9 backspace2
                live_fixtures::q1_typed_at("Type something", false, 37), // idx10 backspace3 → 占位 ✓
                live_fixtures::q1_typed_at("新内容", false, 37),         // idx11 打新字（未勾）
                live_fixtures::q1_typed_at("新内容", true, 37),          // idx12 space 兜底后
            ],
        );
        r.expect("编辑覆盖必须成功");
        let backspaces = sent.iter().filter(|k| k == &"backspace").count();
        assert_eq!(backspaces, 3, "退格数 = 旧行可见长度（3 字）：{sent:?}");
        assert_eq!(
            sent.iter().filter(|k| k == &"right").count(),
            3,
            "退格前必有等量 right（行首光标 bug 回归锁）：{sent:?}"
        );
        assert_eq!(
            sent.last(),
            Some(&"space".to_string()),
            "收尾勾选兜底：{sent:?}"
        );
        assert!(!sent.contains(&"enter".to_string()), "绝无 enter：{sent:?}");
    }

    /// **已有内容 + overwrite=false 维持中止**（防误触，零键）。
    #[test]
    fn multi_free_text_existing_content_aborts_without_overwrite() {
        let (r, sent) =
            run_multi_free_text_script("hi", false, vec![live_fixtures::q1_typed("旧内容")]);
        let err = r.expect_err("不覆盖必须中止");
        assert!(err.message.contains("已有内容"), "{err}");
        assert!(sent.is_empty(), "零按键：{sent:?}");
    }

    /// **退格轮尽中止**（清不掉 → 5 轮上限，屏面摘要报残留）。
    #[test]
    fn multi_free_text_backspace_rounds_exhausted_aborts() {
        // 每轮退格后屏「顽固地」保持原内容（TUI 不消费退格的故障形态）
        let stubborn = || live_fixtures::q1_typed_at("旧内容", true, 37);
        let screens: Vec<Vec<String>> = vec![
            live_fixtures::q1_typed_at("旧内容", true, 29), // idx0 首段
            live_fixtures::q1_typed_at("旧内容", true, 31), // idx1
            live_fixtures::q1_typed_at("旧内容", true, 33), // idx2
            live_fixtures::q1_typed_at("旧内容", true, 35), // idx3
            stubborn(), // idx4 走位到该行；轮1 读 len=3 → 退格×3 → idx5,6,7
            stubborn(), // idx5 轮1 末读 idx7：未清 → 轮2 读 → 退格×3 → idx8,9,9
            stubborn(), // idx6
            stubborn(), // idx7
            stubborn(), // idx8
            stubborn(), // idx9 轮2-5 反复读到「旧内容」→ 轮尽中止
        ];
        let (r, sent) = run_multi_free_text_script("hi", true, screens);
        let err = r.expect_err("清不掉必须中止");
        assert!(err.message.contains("残留"), "{err}");
        assert!(
            !sent.contains(&"<text:hi>".to_string()),
            "未打新字：{sent:?}"
        );
    }

    /// 已在 Review 屏请求下一题（活体形态）→ 零按键 advanced:false（已在终点）。
    #[test]
    fn advance_on_live_review_is_zero_key() {
        let (r, sent) =
            run_advance_script(vec![live_fixtures::review_unanswered()], NavDirection::Next);
        let out = r.expect("已在 Review 屏 = 零按键返回");
        assert!(!out.advanced);
        assert!(sent.is_empty());
    }
}

/// 丁T2 **实机探测占位**（`#[ignore]`——常规门禁只编译不跑）。
///
/// 本批两项实机定案工作**无法在无真实会话的前提下完成**（需要真实 CLI 会话触发 +
/// 屏读 + 人工观察），故按任务书「`#[ignore]` 实机探测定案后实现；定案前多题卡只读」
/// 的口径：**先落占位与只读语义，把定案后要改的点写死在注释里**。
///
/// 两项待定案：
/// 1. **codex 多问题键序**（`Question n/N` 的 ←→ 导航、每题 enter 提交、全部答完后的
///    终态判定）——定案前的行为 = 多题只读卡（端点 `multi_questions` 409 + 前端只读
///    分支，已由 `question_answer_multi_questions_refused` 与 QuestionCard 的
///    `questions.length > 1` 分支锁定）；定案后 = `answer_key_sequence_for` 增加多题
///    分支（逐题导航），并补三取样夹具。
/// 2. **kimi question 类数字可靠性**（R1 Important-1 的遗留——R1 只证伪了 plan_review
///    审批框）——定案后的两个分支见 [`question_key_profile`] 的丁T2 注释。
///
/// **本占位只做一件事**：校验前置可满足性并打印探测指引（不发起任何注入——实机注入
/// 需要受控的会话与人工观察窗口，由本机人工执行）。跑法：
/// `cargo test --lib live_probe -- --ignored --nocapture`（filter 取模块名子串
/// `live_probe`——它命中 `live_probe_tests` 与 `live_probe_t5` 两个模块的全部用例；
/// 原先写的 `question_live_probe` **命中不到任何用例**，复评 F6-4 已实测订正）
#[cfg(test)]
mod live_probe_tests {
    // 本模块只做前置可满足性检查与探测指引打印（**零注入**）——不消费 `super::*` 的
    // 任何原语，故不引入它（定案后补真断言时再加回）。

    /// **claude 空格注入形态复验占位**（2026-09-24）：用户实机取证（2.1.278，手工
    /// 键盘）证明了空格**语义**（多选屏切勾），但**注入事件形态**未经取证——实现取
    /// VK_SPACE(0x20)+字符 0x20（与 enter/esc/tab 同形，跨族安全口径）。若实机验收
    /// 报「空格已发出但屏读未见到勾选态翻转」中止，按本占位走技能快路径复验
    /// （候选形态：char 形态 vk=0 纯字符流），≥3 取样定案后改
    /// `engine::control_records` 的 space 分支一行。
    #[test]
    #[ignore = "实机验证：claude 多选屏空格注入形态复验（前置=claude ≥2.1.278 + 多选题触发 + 屏读对账）"]
    fn claude_space_injection_form_live_probe() {
        eprintln!(
            "2026-09-24 实机复验占位（claude 空格注入形态）\n\
             \n\
             已实现形态：VK_SPACE(0x20)+字符 0x20 成对 down/up（control_records，\n\
             与 enter/esc/tab 同形）。语义依据：用户手工实机取证 2.1.278（空格=切勾），\n\
             档案 research/refs/phase2-消息注入/2026-09-24-claude多选多题键序-\n\
             用户实机取证.md。手工按键 ≠ 注入事件——形态需实机复验：\n\
             1. 起一 conhost claude 会话，触发 multiSelect:true 问题；\n\
             2. 走 兔维斯 手机端问答卡点选一项（run_toggle_stages 全链）；\n\
             3. 回执 verified=true → 形态成立（归档一条即可）；\n\
             4. 回执 failed{{stage:toggle-row, error:翻转}} → 形态不被消费：按技能\n\
                win-console-inject-probe 快路径补测（候选=char 形态 vk=0），\n\
                ≥3 取样定案后改 engine.rs control_records 的 space 分支。"
        );
    }

    #[test]
    #[ignore = "实机验证：codex 多问题（Question n/N）导航序探测——前置=codex 已装 + 人工触发多问题 + 屏读逐屏抄录"]
    fn codex_multi_question_navigation_live_probe() {
        eprintln!(
            "丁T2 实机探测占位（codex 多问题导航序）\n\
             \n\
             定案前状态：多题卡**只读**（§2.3）——端点对 questions.len()!=1 回 409 \
             multi_questions，前端 QuestionCard 走 `questions.length > 1` 只读分支，\
             零注入按钮。本状态由自动化用例锁定（server.rs 的 \
             question_answer_multi_questions_refused + QuestionCard 只读分支用例）。\n\
             \n\
             待定案矩阵（探测时逐项抄录，勿凭源码猜测）：\n\
             ① 题间导航：←/→ 与 tab 的等价性（源码级记载两者皆有）；\n\
             ② 每题提交：enter 是「提交本题并前进」还是「提交整卷」；\n\
             ③ 全部答完后的终态：Proceed/Go back 确认屏是否存在（丁T1 codex 补测曾见\
             「确认屏数字无效、只认 Enter/Esc」——那属**未答完提交**的形态，与本项是否同屏待查）；\n\
             ④ 数字直选在多题下是否仍有效（单题已验：数字即选即交）；\n\
             ⑤ 中途 Esc 的语义（中断整回合 vs 退回上一题）。\n\
             \n\
             定案后落点：`answer_key_sequence_for` 增加 codex 多题分支；\
             `session_question_answer` 放开 len!=1 的拒绝；QuestionCard 多题分支改写入按钮。"
        );
        // 前置提示：codex 未装也能跑（本占位零副作用），只是探测无法进行
        let cli = crate::inject::approve::cached_cli_version("codex");
        eprintln!("前置检查：codex CLI 版本探测 = {cli:?}（None = 未装/探测失败——探测无法进行）");
    }

    #[test]
    #[ignore = "实机验证：kimi question 类（非审批框）数字通道可靠性——前置=kimi 已装 + 人工触发单选提问 + 屏读前后对照"]
    fn kimi_question_digit_reliability_live_probe() {
        eprintln!(
            "丁T2 实机探测占位（kimi **question 类**数字可靠性，R1 Important-1 遗留）\n\
             \n\
             证据现状：R1 的证伪（'2'+Enter 误批准、'3' 单键拒绝）针对 \
             **plan_review 审批框**；question 类**没有**同等强度证据（唯一记载是 \
             2026-09-20 矩阵 §2.3 的双路径取样）。当前实现保持 TwoPhaseSelect \
             `[数字, enter]`（不超证据改行为）——见 `question_key_profile` 的丁T2 注释。\n\
             \n\
             探测要求（三取样起，逐项抄录）：\n\
             ① 高亮在第 1 项时注入 '2'：高亮是否移动？（plan_review 框实测「无效果」）\n\
             ② 紧随的 Enter 提交的是哪一项？（提交高亮行 = 数字被吞）\n\
             ③ 数字单键是否直接进 Review 屏（矩阵记载）还是无效果（plan_review 行为）？\n\
             ④ Review 屏的确认键：Enter 还是数字？\n\
             ⑤ 多题形态下上述是否变化（本题同时是 codex 多题占位的 kimi 侧对照）。\n\
             \n\
             定案后落点：见 `question_key_profile` 丁T2 注释的两个分支（证实不可靠 → \
             与 approve 侧同族化走 ↓×k + Enter；证实可靠 → 保持本档并归档探测档案到 \
             research/refs/phase2-消息注入/）。"
        );
        let cli = crate::inject::approve::cached_cli_version("kimi");
        eprintln!("前置检查：kimi CLI 版本探测 = {cli:?}（默认表 verified_with 记为 2.0.2）");
    }

    /// **claude 问答屏只读活体探针（#[ignore]，零注入——四闸门①样本门槛的标准
    /// 工具，形见 `mode::kimi_screen_live_dump` / opencode 探针）**：对
    /// `MAM_PROBE_PID` 指定的 claude 进程跑**生产同款** `read_screen_window` +
    /// claude 问答解析器全产物（行块/快照/题干），逐行 dump（带行号）——单题集
    /// 单页签形态（2026-10-08）的取证入口与本矩阵行的日常复验工具。
    ///
    /// 跑法：`MAM_PROBE_PID=<pid> cargo test --lib claude_question_live_probe -- --ignored --nocapture`
    ///
    /// 红线：本探针**只读**，不碰 CONIN$、不发任何键；pid 由调用方显式给定。
    #[test]
    #[cfg(windows)]
    #[ignore = "实机只读探针：claude 问答屏活体 dump（前置=MAM_PROBE_PID=<claude pid>）"]
    fn claude_question_live_probe() {
        let Some(pid) = std::env::var("MAM_PROBE_PID")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            eprintln!("未设 MAM_PROBE_PID——跳过（红线：只碰显式指定的进程）");
            return;
        };
        let Ok(lines) = crate::inject::windows_console::read_screen_window(pid) else {
            eprintln!("pid={pid} 屏读失败（无控制台/权限不足）");
            return;
        };
        eprintln!("==== pid={pid} 可见窗 {} 行 ====", lines.len());
        for (i, l) in lines.iter().enumerate() {
            eprintln!("FIXTURE|{i:02}|{l}");
        }
        eprintln!("---- 解析器产物 ----");
        let rows = super::parse_question_rows(&lines);
        eprintln!("parse_question_rows = {} 行", rows.len());
        for r in &rows {
            eprintln!(
                "  kind={:?} num={:?} checked={:?} focused={} label={:?}",
                r.kind, r.number, r.checked, r.focused, r.label
            );
        }
        match super::question_screen_snapshot(&lines) {
            Some(snap) => eprintln!(
                "question_screen_snapshot = Some(checked={:?} free_text={:?} present={} heading={:?})",
                snap.checked, snap.free_text, snap.free_text_present, snap.heading
            ),
            None => eprintln!("question_screen_snapshot = None（Review 屏或解析空）"),
        }
        eprintln!(
            "question_heading_of = {:?}",
            super::question_heading_of(&lines)
        );
    }

    /// **codex 问答屏只读活体探针（#[ignore]，零注入——四闸门①工具/体检表）**：
    /// 对 `MAM_PROBE_PID` 指定的 codex 进程跑生产同款 `read_screen_window` +
    /// codex 面板解析器全产物（逐行 FIXTURE dump + codex_question_screen_snapshot
    /// 七字段 + 账本 footer 三槽/确认屏 TITLE/RECEIPT 的 detect 命中清单）。
    /// 跑法：`MAM_PROBE_PID=<pid> cargo test --lib codex_question_live_probe -- --ignored --nocapture`
    /// 红线：只读，不碰 CONIN$、不发任何键；pid 由调用方显式给定。
    #[test]
    #[cfg(windows)]
    #[ignore = "实机只读探针：codex 问答面板活体 dump（前置=MAM_PROBE_PID=<codex pid>）"]
    fn codex_question_live_probe() {
        let Some(pid) = std::env::var("MAM_PROBE_PID")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            eprintln!("未设 MAM_PROBE_PID——跳过（红线：只碰显式指定的进程）");
            return;
        };
        let Ok(lines) = crate::inject::windows_console::read_screen_window(pid) else {
            eprintln!("pid={pid} 屏读失败（无控制台/权限不足）");
            return;
        };
        eprintln!("==== pid={pid} 可见窗 {} 行 ====", lines.len());
        for (i, l) in lines.iter().enumerate() {
            eprintln!("FIXTURE|{i:02}|{l}");
        }
        eprintln!("---- codex_question_screen_snapshot ----");
        match crate::inject::question_screen_oc::codex_question_screen_snapshot(&lines) {
            Some(s) => eprintln!(
                "Some(question_idx={} question_total={} unanswered={} is_last={} \
                 focused={:?} heading={:?} options={:?})",
                s.question_idx,
                s.question_total,
                s.unanswered,
                s.is_last,
                s.focused,
                s.heading,
                s.options
            ),
            None => eprintln!("None（非面板 / 摘要屏 / 配对不成立——见下方锚命中清单）"),
        }
        // 账本锚命中清单（footer 三槽 + 确认屏 TITLE + 终态 RECEIPT）——None 时
        // 据此读「题号头在、footer 锚不在（配对窗不够/词形漂移）」还是「锚都在、
        // 头解析失败」还是「整屏无锚（非面板）」，形态与 claude 版诊断一致。
        eprintln!("---- anchor_ledger detect 命中清单（codex / QUESTION / QUESTION_REVIEW）----");
        let lowered: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();
        let scenarios: &[(&str, &[&str])] = &[
            (
                crate::inject::anchor_ledger::scenario::QUESTION,
                &[
                    crate::inject::anchor_ledger::slot::Q_FOOTER_NAVIGATE,
                    crate::inject::anchor_ledger::slot::Q_FOOTER_SUBMIT_ANSWER,
                    crate::inject::anchor_ledger::slot::Q_FOOTER_SUBMIT_ALL,
                    crate::inject::anchor_ledger::slot::RECEIPT,
                ],
            ),
            (
                crate::inject::anchor_ledger::scenario::QUESTION_REVIEW,
                &[crate::inject::anchor_ledger::slot::TITLE],
            ),
        ];
        for (scenario, slots) in scenarios {
            for slot in *slots {
                match crate::inject::anchor_ledger::detect(&lowered, "codex", scenario, slot) {
                    Some(hit) => eprintln!(
                        "HIT {scenario}/{slot} @行{} 文案={:?} 版本={} 证据={}",
                        hit.line_index, hit.row.text, hit.row.observed_version, hit.row.evidence
                    ),
                    None => eprintln!("MISS {scenario}/{slot}"),
                }
            }
        }
        // None 成因诊断：题号头在场吗？footer 锚在吗？（照 claude 版「None 时打
        // 失败成因」的形态——两个锚哪个不成立逐项点名。footer 判定只含三个
        // Q_FOOTER 槽——RECEIPT 是终态回执锚，在场 = 摘要屏（非面板，正常 None），
        // 不得混入「footer 配对」诊断误导。）
        if crate::inject::question_screen_oc::codex_question_screen_snapshot(&lines).is_none() {
            let header_seen = lines.iter().any(|l| {
                crate::inject::question_screen_oc::parse_codex_question_header(l).is_some()
            });
            let footer_slots = [
                crate::inject::anchor_ledger::slot::Q_FOOTER_NAVIGATE,
                crate::inject::anchor_ledger::slot::Q_FOOTER_SUBMIT_ANSWER,
                crate::inject::anchor_ledger::slot::Q_FOOTER_SUBMIT_ALL,
            ];
            let footer_seen = footer_slots.iter().any(|slot| {
                crate::inject::anchor_ledger::detect(
                    &lowered,
                    "codex",
                    crate::inject::anchor_ledger::scenario::QUESTION,
                    slot,
                )
                .is_some()
            });
            eprintln!(
                "---- None 成因诊断：题号头={}（{}）、footer 锚={}——{}",
                if header_seen { "在场" } else { "不在场" },
                if header_seen { "疑无 footer 配对/配对窗不足或形态异常" } else { "非面板屏" },
                if footer_seen { "在场" } else { "不在场（词形漂移→按账本 miss_report 处置）" },
                match (header_seen, footer_seen) {
                    (true, true) => "两者都在仍 None = 配对窗（header 下方 ~12 行）不足或选项区解析断——检查面板行距/状态栏行位置",
                    (true, false) => "题号头在场但 footer 锚不成立——账本词形漂移或 footer 未渲染",
                    (false, true) => "footer 锚在场但题号头解析失败——头词形漂移（对照 FIXTURE 行抄录）",
                    (false, false) => "整屏无 Q_FOOTER 锚——非面板屏（普通屏正常 None；若 RECEIPT 命中则属摘要屏，分派正确）",
                }
            );
        }
    }

    /// **opencode 屏读活体探针（#[ignore]，零注入——四闸门①样本门槛的标准工具）**：
    /// 对 `MAM_PROBE_PID` 指定的 opencode TUI 进程跑**生产同款** `read_screen_window`
    /// + 全套 opencode 问答解析器，逐行 dump（带行号）+ 各解析器产物——用于「读得到屏
    ///   但解析不出 / 定位行号与实机不符」类诊断（2026-10-05 多选自由作答走位误勾选）。
    ///
    /// 跑法：`MAM_PROBE_PID=<pid> cargo test --lib opencode_question_live_kernel_probe -- --ignored --nocapture`
    ///
    /// 红线：本探针**只读**（ReadConsoleOutputCharacterW 路径），不发任何键；
    /// pid 由调用方显式给定（用户知情会话）。
    #[test]
    #[cfg(windows)]
    #[ignore = "实机只读探针：opencode 问答屏活体 dump（前置=MAM_PROBE_PID=<opencode TUI pid>）"]
    fn opencode_question_live_kernel_probe() {
        let Some(pid) = std::env::var("MAM_PROBE_PID")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            eprintln!("未设 MAM_PROBE_PID——跳过（红线：只碰显式指定的进程）");
            return;
        };
        let Ok(lines) = crate::inject::windows_console::read_screen_window(pid) else {
            eprintln!("pid={pid} 屏读失败（无控制台/权限不足）");
            return;
        };
        eprintln!("==== pid={pid} 可见窗 {} 行 ====", lines.len());
        for (i, l) in lines.iter().enumerate() {
            eprintln!("行{:>2}|{}", i + 1, l);
        }
        let d = crate::inject::dialect::own_answer_dialect("opencode");
        eprintln!("---- 解析器产物 ----");
        match d {
            Some(d) => {
                eprintln!(
                    "own_answer_row_state = {:?}（(行序号, 已勾选, 已存文本)）",
                    super::opencode_own_answer_row_state(&lines, &d)
                );
            }
            None => eprintln!("方言表无 opencode 条目（不应发生）"),
        }
        eprintln!(
            "input_open = {}（占位行判据）",
            super::opencode_own_answer_input_open(&lines)
        );
        eprintln!(
            "question_screen_snapshot = {:?}",
            crate::inject::question_screen_oc::opencode_question_screen_snapshot(&lines)
        );
    }

    /// **opencode 2.x 方向键形态对照实验（#[ignore]）**：对 `MAM_PROBE_PID` 指定的
    /// **自建** opencode 会话，走**生产发送路径**（`inject_key_spec`）测 ↓ 形态——
    /// `MAM_PROBE_GROUP=vt`（A 族 ESC[B 字符流）或 `vk`（B 族 VK_DOWN 键形态），
    /// ↓×5 → enter：own answer 行应展开占位行（input_open=true = 该形态生效）。
    ///
    /// **esc 会 dismiss 整个对话框**（戊探A 定案）→ 每组独立对话框：跑完一组后
    /// 重新触发 question 再跑另一组。
    /// 目的：opencode 2.0.22 大版本升级后的方向键形态复验（族表已随本轮复验升
    /// verified_with=2.0.22；用户实机 2026-10-05 报告的 VT↓ 无效果实为守卫假阴性中止）。
    ///
    /// 红线：只碰 MAM_PROBE_PID 指定的自建进程；结束由调用方 taskkill 清场。
    #[test]
    #[cfg(windows)]
    #[ignore = "实机对照实验：opencode 2.x ↓ 形态（前置=自建会话 question 对话框已打开 + MAM_PROBE_PID=<pid> + MAM_PROBE_GROUP=vt|vk）"]
    fn opencode_arrow_form_live_probe() {
        use crate::inject::families::{FamilySpec, TuiFamily};
        let Some(pid) = std::env::var("MAM_PROBE_PID")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            eprintln!("未设 MAM_PROBE_PID——跳过（红线：只碰自建进程）");
            return;
        };
        let group = std::env::var("MAM_PROBE_GROUP").unwrap_or_else(|_| "vt".to_string());
        let spec = FamilySpec {
            family: if group == "vk" {
                TuiFamily::Crossterm
            } else {
                TuiFamily::RawVt
            },
            verified_with: "probe",
            slow_consumer: false,
            confirm_timeout_ms: 5_000,
        };
        eprintln!(
            "==== 组 {group}（{}）↓×5 → enter ====",
            if group == "vk" { "VK_DOWN" } else { "VT ESC[B" }
        );
        let dump = |tag: &str| {
            if let Ok(lines) = crate::inject::windows_console::read_screen_window(pid) {
                eprintln!("---- [{tag}] {} 行 ----", lines.len());
                for (i, l) in lines
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| !l.trim().is_empty())
                {
                    eprintln!("行{:>2}|{}", i + 1, l);
                }
                let d = crate::inject::dialect::own_answer_dialect("opencode").unwrap();
                eprintln!(
                    "[{tag}] own_answer_row_state={:?} input_open={}",
                    super::opencode_own_answer_row_state(&lines, &d),
                    super::opencode_own_answer_input_open(&lines)
                );
            } else {
                eprintln!("[{tag}] 屏读失败");
            }
        };
        for i in 0..5 {
            if let Err(e) = crate::inject::windows_console::inject_key_spec(pid, "down", &spec) {
                eprintln!("down#{i} 注入失败: {e}");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
        std::thread::sleep(std::time::Duration::from_millis(600));
        dump("down5后");
        let _ = crate::inject::windows_console::inject_key_spec(pid, "enter", &spec);
        std::thread::sleep(std::time::Duration::from_millis(900));
        dump("enter后");
    }

    /// **opencode own answer 生产阶段机活体直跑（#[ignore]）**：对 `MAM_PROBE_PID`
    /// 指定的**自建**会话（屏上已是 question 对话框），直接调用
    /// [`run_opencode_own_answer_stages`]（生产编排原函数 + 生产 read/inject 路径），
    /// 全程打印屏面与中止点——2026-10-05 用户实机「多选自由作答误勾选 + 守卫中止」
    /// 的最忠实复现。回执轮询用 no-op（回执语义不在本探针范围）。
    ///
    /// 红线：只碰 MAM_PROBE_PID 指定的自建进程；结束由调用方 taskkill 清场。
    #[test]
    #[cfg(windows)]
    #[ignore = "实机直跑：own answer 生产阶段机（前置=自建会话 question 对话框已打开 + MAM_PROBE_PID=<pid>）"]
    fn opencode_own_answer_stage_live_probe() {
        let Some(pid) = std::env::var("MAM_PROBE_PID")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            eprintln!("未设 MAM_PROBE_PID——跳过（红线：只碰自建进程）");
            return;
        };
        let dump = |tag: &str| {
            if let Ok(lines) = crate::inject::windows_console::read_screen_window(pid) {
                eprintln!("---- [{tag}] {} 行 ----", lines.len());
                for (i, l) in lines
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| !l.trim().is_empty())
                {
                    eprintln!("行{:>2}|{}", i + 1, l);
                }
                let d = crate::inject::dialect::own_answer_dialect("opencode").unwrap();
                eprintln!(
                    "[{tag}] own_answer_row_state={:?} input_open={}",
                    super::opencode_own_answer_row_state(&lines, &d),
                    super::opencode_own_answer_input_open(&lines)
                );
            } else {
                eprintln!("[{tag}] 屏读失败");
            }
        };
        dump("跑前");
        let mut terminal = super::FreeTextClosures {
            read: || crate::inject::windows_console::read_screen_window(pid).ok(),
            send: |key: &str| {
                let spec = crate::inject::families::family_for("opencode")
                    .unwrap_or(crate::inject::families::FALLBACK_SPEC);
                let r = crate::inject::windows_console::inject_key_spec(pid, key, &spec);
                if r.is_ok() {
                    std::thread::sleep(std::time::Duration::from_millis(
                        crate::inject::families::SUBMIT_DELAY_MS,
                    ));
                }
                r
            },
            send_text: |text: &str| {
                let spec = crate::inject::families::family_for("opencode")
                    .unwrap_or(crate::inject::families::FALLBACK_SPEC);
                crate::inject::windows_console::inject_text_spec(pid, text, &spec).map(|_| ())
            },
            settle: || {
                std::thread::sleep(std::time::Duration::from_millis(
                    crate::inject::families::SUBMIT_DELAY_MS,
                ))
            },
        };
        match super::run_opencode_own_answer_stages(
            "probe live text 20261005",
            false,
            || Ok(None),
            &mut terminal,
        ) {
            Ok(o) => eprintln!(
                "阶段机 Ok: sent_keys={:?} screen_text={:?} screen_checked={:?}",
                o.sent_keys, o.screen_text, o.screen_checked
            ),
            Err(e) => eprintln!("阶段机中止: kind={:?} message={}", e.kind, e.message),
        }
        std::thread::sleep(std::time::Duration::from_millis(600));
        dump("跑后");
    }
}

/// 丁T5 **实机探测占位**（`#[ignore]`——常规门禁只编译不跑）。
///
/// # 为什么还要一条实机占位（本批的自动化面已经覆盖了阶段机）
///
/// 丁T5 把提交/自由作答抽成了**可注入的编排**（`run_submit_stages` /
/// `run_free_text_stages`），门禁里已用**真机屏幕原文**的脚本序列覆盖了段推进、
/// 段中止、回执三态（`inject::question` 的 8 例 + `remote::server` 的 10 例）。
/// 但脚本覆盖的是**我们的判据**，它无法回答两个只有真机能答的问题：
///
/// 1. **判据本身是否在真机上成立**——`SUBMIT_FOCUS_MARKER`（U+276F）、
///    `Submit` 行、`Review your answers` / `Ready to submit` 三个锚、终态
///    `User answered Claude's questions:` 都取自 2026-09-21 的截图与二进制字符串，
///    而**屏读 API（`read_screen_window`）拿到的是同一屏的另一种表达**（逐行 trim、
///    窗口宽裁剪、行序）。截图上有、屏读里没有（或反过来）是真实存在的风险——只能
///    在真会话上对照一次。
/// 2. **每段之间的时间窗**是否够（`QUESTION_STAGE_POLL_TOTAL_MS = 2000ms` 是自裁值，
///    依据是探测档案里「多选三段式更久」与 ≤1s 的观察——不是测量）。
///
/// 跑法（需要真实 claude 会话与人工观察窗口；本占位**零注入**，只打印指引）：
/// `cargo test --lib live_probe -- --ignored --nocapture`（filter 取模块名子串
/// `live_probe`——它命中 `live_probe_tests` 与 `live_probe_t5` 两个模块的全部用例；
/// 原先写的 `question_live_probe` **命中不到任何用例**，复评 F6-4 已实测订正）
#[cfg(test)]
mod live_probe_t5 {
    /// **丁T5 实机定案：claude 多选提交全链 + 自由作答全链的屏读对照**
    ///
    /// ## 前置条件
    ///
    /// - claude CLI 已装（下表记 2.1.251）且已登录；
    /// - 在一个临时目录里起一个真会话（conhost 宿主，**不要**用 WT——本次取证的宿主
    ///   是 conhost，WT 的屏读行为未验）；
    /// - 会话里发一句让模型调 `AskUserQuestion` 且 `multiSelect: true`、3 个选项、
    ///   选项文本**用中文**（非 ASCII 是夹具纪律：本批两次栽在纯 ASCII 夹具上）。
    ///
    /// ## 观测点（逐项抄录，勿凭源码猜）
    ///
    /// ① 问题弹出后 `read_screen_window(pid)` 的逐行输出：`Submit` 行是否带 `❯`？
    ///    该行的**行首空白形态**如何（判据用 `trim_start` + strip 标记，需验证）？
    /// ② 点「提交勾选」后：兔维斯 侧日志里 `submit` 段的轮询次数、各次探测结论
    ///    （`NotYet` 的原因文案）。**Review 屏出现耗时**（决定 2000ms 是否够）。
    /// ③ Review 屏的屏读行：`Review your answers` / `Ready to submit your answers?`
    ///    是否**都**在可见窗口内（矮窗口下可能只剩后者——判据是「任一」，但要看实况）；
    ///    `1. Submit answers` 的编号与文本；
    /// ④ 确认后终态：`User answered Claude's questions:` 是否在**同一次屏读**里出现
    ///    （回执行可能被后续输出顶出窗口 → `receipt_seen` 会如实为 false，那不是 bug
    ///    而是窗口限制——需据实调整 `QUESTION_STAGE_POLL_TOTAL_MS` 或判据措辞）；
    /// ⑤ 自由作答链：`Type something` 行的屏读形态（**单选**带句点 `3. Type something.`）、
    ///    定位数字是否等于屏上编号、文本注入后该行是否变成可编辑形态（复核判据是否仍成立）；
    /// ⑤' **多选屏的自由作答行形态**（复评 F6-3 的定案项——**当前已按「不可用」处置**）：
    ///    兔维斯 现按截图判定为 `4. [ ] Type something`（带勾选框，故前缀判据不匹配 →
    ///    端点 409 拒绝、前端不渲染输入框）。**探测要确认的事**：该行在屏读产物里的
    ///    **确切文本**（勾选框是 `[ ]` / `[✓]` 还是别的宽度对齐变体？行内是否有额外空白？）
    ///    ——若形态明确，可按「剥掉行首勾选框前缀再匹配」放宽判据（并在
    ///    `free_text_shape_supported` 里按形态族收口），届时同步改前端
    ///    `freeTextEnabled` 的多选排除；若形态不稳定，维持拒绝。
    /// ⑥ **中止路径的真机验证**：在 Review 屏出现前人为按 Esc（模拟「屏读没等到」），
    ///    观察 兔维斯 是否如实中止且**没有**发出确认数字（对照审计 result=aborted:review）。
    ///
    /// ## 定案后落点
    ///
    /// 判据漂移 → 改对应常量（`SUBMIT_FOCUS_MARKER` / `REVIEW_*_ANCHOR` /
    /// `ANSWERED_RECEIPT_ANCHOR` / `FREE_TEXT_ROW_LABEL`）并把真机屏读**原文**存成
    /// 下一批的夹具；时间窗不足 → 调 `QUESTION_STAGE_POLL_TOTAL_MS` 并在此注记实测值。
    #[test]
    #[ignore = "实机验证：claude 多选提交/自由作答全链的屏读对照——前置=claude 已装 + 人工起会话 + 屏读逐屏抄录"]
    fn claude_submit_and_free_text_full_chain_live_probe() {
        eprintln!(
            "丁T5 实机探测占位（claude 多选提交 + 自由作答全链）
             
             本占位**零注入**：只校验前置可满足性并打印观测点（实机注入需要受控会话
             与人工观察窗口，由本机人工执行）。逐项观测点见本测试的 doc 注释。
             
             关键对照面（自动化已覆盖的部分与只有真机能答的部分）：
             - 自动化已覆盖：段推进/段中止/回执三态（8 例内核 + 10 例端点，全部用
               2026-09-21 探测档案的**真机屏幕原文**做夹具）；
             - 只有真机能答：（a）`read_screen_window` 的产物与截图是否逐行一致
               （本批判据全部取自截图与二进制字符串）；（b）2000ms 单段窗是否够。"
        );
        let cli = crate::inject::approve::cached_cli_version("claude");
        eprintln!("前置检查：claude CLI 版本探测 = {cli:?}（None = 未装/探测失败——探测无法进行；本批判据记 2.1.251）");
        // 屏读能力自检（不需要目标会话，只是证明本机有屏读 API）
        #[cfg(windows)]
        eprintln!("前置检查：Windows 屏读能力可用（read_screen_window 存在；无目标 pid 故不调用）");
        #[cfg(not(windows))]
        eprintln!("前置检查：本平台**无屏读能力** → 阶段机各段必然中止（如实回执 + 引导终端）");
    }
}

// ============================================================
// 屏读体检探针（references/screen-read-matrix.md 的落地工具）
// ============================================================

/// **活体屏读探针**（只读、零注入——不发任何键）。屏读形态矩阵的维护工具：
/// 对任意活会话 dump 整屏 + 跑全解析器，输出「解析器眼里的屏」。
///
/// 用法：
/// `MAM_PROBE_PID=<pid> cargo test --lib screen_read_probe -- --ignored --nocapture`
///
/// pid 用 `Get-CimInstance Win32_Process` 找目标 CLI 进程。纪律同
/// `references/screen-read-matrix.md`：dump 逐字落档、结论不得超过证据。
#[cfg(test)]
#[cfg(windows)] // 探针链路 = windows_console（屏读是 Windows 能力）；非 Windows 无此模块
mod screen_read_probe {
    use super::*;

    #[test]
    #[ignore = "活体探针（字符级）：MAM_PROBE_PID=<pid> cargo test --lib screen_read_probe::dump_raw -- --ignored --nocapture"]
    fn dump_raw() {
        let pid: u32 = std::env::var("MAM_PROBE_PID")
            .expect("MAM_PROBE_PID=<pid>")
            .parse()
            .unwrap();
        let lines = crate::inject::windows_console::read_screen_window(pid).expect("屏读失败");
        for (i, l) in lines.iter().enumerate() {
            eprintln!("{:02}|{:?}", i, l);
        }
    }

    #[test]
    #[ignore = "活体探针：MAM_PROBE_PID=<pid> cargo test --lib screen_read_probe -- --ignored --nocapture"]
    fn dump_and_parse() {
        let pid: u32 = std::env::var("MAM_PROBE_PID").expect(
            "用法：MAM_PROBE_PID=<pid> cargo test --lib screen_read_probe -- --ignored --nocapture",
        ).parse().expect("MAM_PROBE_PID 必须是数字 pid");
        let lines = crate::inject::windows_console::read_screen_window(pid)
            .unwrap_or_else(|e| panic!("屏读失败 pid={pid}（attach 不上/进程已退出）：{e}"));
        eprintln!("=== pid={pid} LINES ({}) ===", lines.len());
        for (i, l) in lines.iter().enumerate() {
            eprintln!("{:02}|{}", i, l);
        }
        let rows = parse_question_rows(&lines);
        eprintln!("=== parse_question_rows: {} rows ===", rows.len());
        for r in &rows {
            eprintln!(
                "  kind={:?} num={:?} checked={:?} focused={} label={:?}",
                r.kind, r.number, r.checked, r.focused, r.label
            );
        }
        eprintln!(
            "=== submit_row_present(any): {}",
            lines.iter().any(|l| submit_row_present(l))
        );
        eprintln!("=== review_probe: {:?}", probe_review_screen(&lines));
        eprintln!(
            "=== advance_stage_screen_ready: {}",
            advance_stage_screen_ready(&lines)
        );
    }
}

// ============================================================
// 活体黄金夹具（2026-10-02 探针逐字 dump，claude 2.1.278 多题流）
// ============================================================

/// **活体黄金夹具**——逐字取自 2026-10-02 只读探针 dump（证据出处与叙事见
/// `.agents/skills/win-console-inject-probe/references/screen-read-matrix.md`
/// 首批实例）。**逐字入档，禁止改写/手造**（闸门 1：样本门槛——手抄/改写即失去
/// 证据力；本批评审事故正是手抄夹具丢了滚回区分隔线与单选子题形态）。
/// 焦点位移与伪 q3 变体是**派生**夹具（编排测试的脚本转场需要），基底仍逐字。
#[cfg(test)]
pub(crate) mod live_fixtures {
    const SEP: &str = "───────────────────────────────────────────────────────────────────────────";

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// 滚回区（三块活体屏共用 0–20 行）：Planning 块上方的 ─ 分隔线正是旧
    /// below_separator 闩锁把题屏判死的受害现场
    fn scrollback() -> Vec<String> {
        lines(&[
            "",
            "● 小左你好！有什么我可以帮你的吗？",
            "",
            "✻ Sautéed for 4s · done 16:24",
            "",
            "❯ 问我5个问题，以修改优化这个文件夹里面的小说。这个问题应该是一个多选题",
            "  [mobile matebook-chrome]",
            "",
            "  Thought for 3s (ctrl+o to expand)",
            "",
            "● 小左你好！我先看看文件夹里有什么内容，然后再针对小说提出有针对性的问题。",
            "",
            "  Thought for 2s, searched for 1 pattern, read 1 file (ctrl+o to expand)",
            "",
            "● 文件夹里有多部小说文件。我先快速浏览一下主要几部，再提出有针对性的问题。",
            "",
            "  Thought for 5s, read 6 files (ctrl+o to expand)",
            "",
            "● 文件夹里有两部小说：《末班车》（共4个版本：原版、优化版、精简版、情感救赎",
            "  版，另有《末班车-重逢》同人番外）和**《她的七点零三》**。以及配套的封面/",
            "  长图。下面我来问5个多选题，帮助明确优化方向。",
        ])
    }

    /// 把 `❯ ` 焦点标记移到 `to` 行（取**最后**一个 ❯ 行为现行焦点——滚回区的
    /// 提示回显行 `❯ 问我5个问题…` 不是焦点）
    fn move_focus(screen: &[String], to: usize) -> Vec<String> {
        let mut out = screen.to_vec();
        let from = out
            .iter()
            .rposition(|l| l.contains('❯'))
            .expect("基底屏必有焦点行");
        out[from] = out[from].replacen("❯ ", "", 1);
        out[to] = format!("❯ {}", out[to].trim_start());
        out
    }

    /// q1 多选子题（优化范围）：选项带勾选框 + 独立 `Next` 行（锚 A 形态）。
    /// 焦点在选项 1。
    pub(crate) fn q1_multi() -> Vec<String> {
        let mut v = scrollback();
        v.extend(lines(&[
            SEP,
            "Planning:",
            "C:\\Users\\bunny\\.claude\\plans\\5-mobile-matebook-chrome-polished-map.md",
            SEP,
            "←  ☐ 优化范围  ☐ 版本处理  ✔ Submit  →",
            "",
            "这次要优化哪几部作品？",
            "",
            "❯ 1. [ ] 末班车系列",
            "  悬疑版本：原版(26行)、优化版(最长)、精简版、情感救赎版中的某些或全部",
            "  2. [ ] 末班车-重逢",
            "  重逢向新故事，高中同学十年后在末班地铁上相遇",
            "  3. [ ] 她的七点零三",
            "  温软浪漫短篇：世界时钟停在7:03，只有两人手表还在走",
            "  4. [ ] 全部作品",
            "  对文件夹里所有小说统一进行修改优化",
            "  5. [ ] Type something",
            "     Next",
            SEP,
            "  6. Chat about this",
            "",
            "Enter to select · Tab/Arrow keys to navigate · Esc to cancel",
            "",
            "",
        ]));
        v
    }

    /// q1 焦点位移变体：0=选项1 1=选项2 2=选项3 3=选项4 4=Type something 5=Next
    pub(crate) fn q1_focus_at(row: usize) -> Vec<String> {
        const FOCUS_LINES: [usize; 6] = [29, 31, 33, 35, 37, 38];
        move_focus(&q1_multi(), FOCUS_LINES[row])
    }

    /// q1 打字后形态（派生）：Type something 行 = `❯ 5. [{勾选}] {text}`（内联编辑
    /// 生效、焦点仍在该行——活体取证 2026-10-02；checked=false = 用户实测的
    /// 「打字后默认未勾选」形态，勾选兜底的输入）
    pub(crate) fn q1_typed_flagged(text: &str, checked: bool) -> Vec<String> {
        q1_typed_at(text, checked, 37)
    }

    /// q1 打字屏·焦点任意（派生）：37 行 = `5. [{勾}] {text}`（❯ 由 move_focus 唯一
    /// 落点），29/31/33/35 = 选项行——走位/退格/兜底各段脚本屏的基础夹具
    pub(crate) fn q1_typed_at(text: &str, checked: bool, focus_line: usize) -> Vec<String> {
        let box_glyph = if checked { "[✔]" } else { "[ ]" };
        let base = q1_multi()
            .into_iter()
            .enumerate()
            .map(|(i, l)| {
                if i == 37 {
                    format!("5. {box_glyph} {text}")
                } else {
                    l
                }
            })
            .collect::<Vec<_>>();
        move_focus(&base, focus_line)
    }

    /// q1 打字后·已勾选·焦点在 FreeText 行（兼容旧名）
    pub(crate) fn q1_typed(text: &str) -> Vec<String> {
        q1_typed_at(text, true, 37)
    }

    /// q2 **单选子题**（版本处理）：选项无勾选框、**无独立推进行**（页签栏锚 B
    /// 形态）。焦点在选项 1。
    pub(crate) fn q2_single() -> Vec<String> {
        let mut v = scrollback();
        v.extend(lines(&[
            SEP,
            "Planning:",
            "C:\\Users\\bunny\\.claude\\plans\\5-mobile-matebook-chrome-polished-map.md",
            SEP,
            "←  ☐ 优化范围  ☐ 版本处理  ✔ Submit  →",
            "",
            "《末班车》目前有4个版本（原版/优化版/精简版/情感救赎版），如何处理这些版本",
            "？",
            "",
            "❯ 1. 选一个最优版本深加工 (Recommended)",
            "     我帮你对比分析，选定最适合的方向版本继续打磨",
            "  2. 融合各版优点成新版本",
            "     例如：悬疑版的张力+情感版的母亲线+优化版的细节描写，整合成一个最终版",
            "  3. 每个版本独立优化",
            "     不合并，每个版本独立润色完善",
            "  4. 淘汰旧版本只留最好",
            "     对比后删除多余版本",
            "  5. Type something.",
            SEP,
            "  6. Chat about this",
            "",
            "Enter to select · Tab/Arrow keys to navigate · Esc to cancel",
            "",
            "",
        ]));
        v
    }

    /// q2 焦点阶梯（派生）：❯ 落到指定行号（30/32/34/36=选项 1-4、38=Type something）
    pub(crate) fn q2_focus_line(line: usize) -> Vec<String> {
        move_focus(&q2_single(), line)
    }

    /// q2 打字后形态（派生）：单选子题的 Type something 行被内联文字替换（无勾选
    /// 框——单选形态），焦点仍在该行
    pub(crate) fn q2_typed_single(text: &str) -> Vec<String> {
        q2_single()
            .into_iter()
            .enumerate()
            .map(|(i, l)| match i {
                29 => l.replacen("❯ ", "", 1),
                38 => format!("❯ 5. {text}"),
                _ => l,
            })
            .collect()
    }

    /// q2 焦点在 Type something 行（派生：两行例外先决的转场屏）
    pub(crate) fn q2_focus_on_freetext() -> Vec<String> {
        move_focus(&q2_single(), 38)
    }

    /// 伪 q3（派生：q2 题干替换——活体只有 2 题，中间题 `→` 转场的脚本需要）
    pub(crate) fn q3_derived() -> Vec<String> {
        q2_single()
            .into_iter()
            .map(|l| {
                if l.contains("《末班车》目前有4个版本") {
                    "《她的七点零三》影视化方向怎么选".to_string()
                } else {
                    l
                }
            })
            .collect()
    }

    /// Review 确认屏（未答完直达——`→` 末题直达的落点；活体当时 TUI 重绘后
    /// Planning 块滚出可见窗）。`⚠ You have not answered all questions` 警告行在场。
    pub(crate) fn review_unanswered() -> Vec<String> {
        let mut v = scrollback();
        v.extend(lines(&[
            "",
            SEP,
            "←  ☐ 优化范围  ☐ 版本处理  ✔ Submit  →",
            "",
            "Review your answers",
            "",
            "⚠ You have not answered all questions",
            "",
            "Ready to submit your answers?",
            "",
            "❯ 1. Submit answers",
            "  2. Cancel",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
        ]));
        v
    }

    /// **单题集 · 单页签形态**（2026-10-08 活体逐字 dump，claude 2.1.287，
    /// 自建探针会话；证据 `%USERPROFILE%\\mam-probe-m6r\\evidence\\
    /// screen-claude-single-tab-auq-20261008-154900.txt`，run-id 同名）：
    /// 单问题 AskUserQuestion 的页签栏只有 ` ☐ Deploy` 一行——无 `←`/`→`、
    /// 无 `Submit`，既有双锚（推进行 ∨ 复合页签栏）全不命中 → 解析 0 行中止
    /// （0.5.1 用户报障「屏上解析不出问答选项块」的靶屏）。27 行（索引 00–26）
    /// 逐字；行 05 的 U+FFFD 是探针控制台管道对 CP936 hook 输出的有损视图、行 26
    /// footer 的 `↑/` 后缺 `↓` 是屏读字形读法实测（均非转写脱落，逐字保留）。
    pub(crate) fn q_single_tab() -> Vec<String> {
        lines(&[
            " ▐▛███▛█   Claude Code v2.1.287",
            "▝▜██████▀  ccs-claude-zhipu--glm-5.3-flash[1m] with max effort ·API Usage Billing",
            " ▝▝   ▝▝   ~\\AppData\\Local\\Temp\\probe-auq-single-20261008",
            "",
            "  ⎿  SessionStart:startup hook error",
            "  ⎿  Failed with non-blocking status code: ����λ�� ��:1 �ַ�: 48",
            "",
            "❯ Call the AskUserQuestion tool now: ask me exactly ONE question (single select, not multiple) with header Deploy and",
            "  question Which database should we use? Offer exactly 3 numbered options: Postgres, SQLite, DuckDB. Do nothing else.",
            "",
            "  Thought for 8s (ctrl+o to expand)",
            "────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────",
            " ☐ Deploy",
            "",
            "Which database should we use?",
            "",
            "❯ 1. Postgres",
            "     Powerful open-source relational database for prodrcpion workloads",
            "  2. SQLite",
            "     Lightweight embedded relational database, zero configuration",
            "  3. DuckDB",
            "     Fast in-process analytical database for OLAP workloads",
            "  4. Type something.",
            "────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────",
            "  5. Chat about this",
            "",
            "Enter to select ·↑/ to navigate ·Esc to cancel",
        ])
    }

    // ===== codex 0.160.0 问答面板活体夹具（2026-10-09 取证批，Task 1 底料
    // `docs/superpowers/specs/2026-10-08-codex-160-question-屏读底料.md` §9 点名；
    // 证据根 `%USERPROFILE%\mam-probe-m6r\evidence\`，FIXTURE 行程序化提取生成）=====
    // 形态总纲：`›` = 焦点（非选中，选中态只在属性层 0x0030 族，字符层无标记）；
    // 题号头词形 未答 `Question i/N (M unanswered)` / 全答 `Question i/N`（计数段整个消失）；
    // footer 四段（多题）`tab to add notes | enter to submit all | ←/→ to navigate questions | esc to interrupt`、
    // 三段（单题）无导航段且 `submit all`↔`submit answer` 随全卷未答切换；
    // 确认屏（`Submit with unanswered questions?`）与 `?` 首帧形本轮取证不可达——无夹具，不得伪造。

    /// 证据 `codex-q-20261009-s1-q1.txt` FIXTURE 20–28：S1 两题面板首屏（Q1，3 选项）：题号头 `Question 1/2 (2 unanswered)` + footer 四段。
    pub(crate) fn codex_s1_q1() -> Vec<String> {
        lines(&[
            "",
            "  Question 1/2 (2 unanswered)",
            "  Which DB?",
            "",
            "  › 1. Postgres           使用 PostgreSQL 作为数据库。",
            "    2. SQLite             使用 SQLite 作为数据库。",
            "    3. None of the above  Optionally, add details in notes (tab)",
            "",
            "  tab to add notes | enter to submit answer | ←/→ to navigate questions | esc to interrupt",
        ])
    }

    /// 证据 `codex-q-20261009-s1-q1-after-digit.txt` FIXTURE 20–29：S1 数字推进后的 Q2 屏（4 选项 Redis/Memcached/None needed/None of the above）：题号头 `Question 2/2 (1 unanswered)`，footer 已见 `enter to submit all` 四段——兼作「多题面板 4 选项」夹具（footer 读取抖动帧已按底料 §7 多帧还原）。
    pub(crate) fn codex_s1_q2() -> Vec<String> {
        lines(&[
            "  Question 2/2 (1 unanswered)",
            "  Which cache?",
            "",
            "  › 1. Redis              使用 Redis 作为缓存。",
            "    2. Memcached          使用 Memcached 作为缓存。",
            "    3. None needed        不需要缓存。",
            "    4. None of the above  Optionally, add details in notes (tab)",
            "",
            "  tab to add notes | enter to submit all | ←/→ to navigate questions | esc to interrupt",
            "",
        ])
    }

    /// 证据 `codex-q-20261009-s1-q2-zero.txt` FIXTURE 20–28：S1 Q2 归零词形：题号头只剩 `Question 2/2`，`(N unanswered)` 计数段整个消失，焦点 Memcached 行 `› 2.`；含 FIXTURE 27 空行 + 28 footer（`enter to submit all` 干净，navigate/esc 段有抖动帧已按底料 §7 多帧还原）。
    pub(crate) fn codex_s1_q2_zero() -> Vec<String> {
        lines(&[
            "  Question 2/2",
            "  Which cache?",
            "",
            "    1. Redis              使用 Redis 作为缓存。",
            "  › 2. Memcached          使用 Memcached 作为缓存。",
            "    3. None needed        不需要缓存。",
            "    4. None of the above  Optionally, add details in notes (tab)",
            "",
            "  tab to add notes | enter to submit all | ←/→ to navigate questions | esc to interrupt",
        ])
    }

    /// 证据 `codex-q-20261009-s1-back-to-q1-h.txt` FIXTURE 21–28：S1 h 键切回 Q1 已答态：`Question 1/2` 无计数段，焦点落已选项 Postgres 行（选中态只在属性层，字符层无标记）；含 FIXTURE 27 空行 + 28 footer（`enter to submit answer` 干净，navigate/esc 段有抖动帧已按底料 §7 多帧还原）。
    pub(crate) fn codex_s1_q1_answered() -> Vec<String> {
        lines(&[
            "  Question 1/2",
            "  Which DB?",
            "",
            "  › 1. Postgres           使用 PostgreSQL 作为数据库。",
            "    2. SQLite             使用 SQLite 作为数据库。",
            "    3. None of the above  Optionally, add details in notes (tab)",
            "",
            "  tab to add notes | enter to submit answer | ←/→ to navigate questions | esc to interrupt",
        ])
    }

    /// 证据 `codex-q-20261009-s1-q1-unanswered.txt` FIXTURE 21–28：S1 空格反选态：计数段重现 `(1 unanswered)`，焦点 SQLite 行——未答计数随选中态实时增减实证；含 FIXTURE 27 空行 + 28 footer（`enter to submit answer` 干净，navigate/esc 段有抖动帧已按底料 §7 多帧还原）。
    pub(crate) fn codex_s1_q1_unanswered() -> Vec<String> {
        lines(&[
            "  Question 1/2 (1 unanswered)",
            "  Which DB?",
            "",
            "    1. Postgres           使用 PostgreSQL 作为数据库。",
            "  › 2. SQLite             使用 SQLite 作为数据库。",
            "    3. None of the above  Optionally, add details in notes (tab)",
            "",
            "  tab to add notes | enter to submit answer | ←/→ to navigate questions | esc to interrupt",
        ])
    }

    /// 证据 `codex-q-20261009-s2-q1.txt` FIXTURE 21–29：S2 三题面板 Q1：含状态栏行插入变体（`deepseek...Plan mode` 行插在选项区和 footer 之间）。
    pub(crate) fn codex_s2_q1() -> Vec<String> {
        lines(&[
            "  Question 1/3 (3 unanswered)",
            "  Which DB?",
            "",
            "  › 1. Postgres           使用 PostgreSQL 作为数据库。",
            "    2. SQLite             使用 SQLite 作为数据库。",
            "    3. None of the above  Optionally, add details in notes (tab)",
            "  deepseek-v4.1-flash medium · ~\\AppData\\Local\\Temp\\probe-codex-q-20261008 · 看 D:/Program Files/Git/plan  Plan mode",
            "  tab to add notes | enter to submit answer | ←/→ to navigate questions | esc to interrupt",
            "",
        ])
    }

    /// 证据 `codex-q-20261009-s3-single.txt` FIXTURE 21–29：S3 单题面板：footer 三段无 `←/→ to navigate questions`（单题不渲染导航段），题号头仍带 `(1 unanswered)`。
    pub(crate) fn codex_s3_single() -> Vec<String> {
        lines(&[
            "  Question 1/1 (1 unanswered)",
            "  Which DB?",
            "",
            "  › 1. Postgres           使用 PostgreSQL 作为数据库。",
            "    2. SQLite             使用 SQLite 作为数据库。",
            "    3. None of the above  Optionally, add details in notes (tab)",
            "  deepseek-v4.1-flash medium · ~\\AppData\\Local\\Temp\\probe-codex-q-20261008 · 看 D:/Program Files/Git/plan  Plan mode",
            "  tab to add notes | enter to submit answer | esc to interrupt",
            "",
        ])
    }

    /// 证据 `codex-q-20261009-s3-zero.txt` FIXTURE 21–29：S3 单题归零词形：`Question 1/1` 计数段消失，焦点 SQLite 行。
    pub(crate) fn codex_s3_zero() -> Vec<String> {
        lines(&[
            "  Question 1/1",
            "  Which DB?",
            "",
            "    1. Postgres           使用 PostgreSQL 作为数据库。",
            "  › 2. SQLite             使用 SQLite 作为数据库。",
            "    3. None of the above  Optionally, add details in notes (tab)",
            "  deepseek-v4.1-flash medium · ~\\AppData\\Local\\Temp\\probe-codex-q-20261008 · 看 D:/Program Files/Git/plan  Plan mode",
            "  tab to add notes | enter to submit answer | esc to interrupt",
            "",
        ])
    }

    /// 证据 `codex-q-20261009-s4-notes-open.txt` FIXTURE 21–28：S4 tab 直开 notes 态字符层：焦点在 `› 3. None of the above`，字符层与按 tab 前几乎同形（Other 选中+输入态只在属性层可见）。
    pub(crate) fn codex_s4_notes_open() -> Vec<String> {
        lines(&[
            "  Question 1/1 (1 unanswered)",
            "  Which database do you want for a hello-world script?",
            "",
            "    1. Oracle RAC         使用 Oracle RAC 作为数据库。",
            "    2. DB2                使用 DB2 作为数据库。",
            "  › 3. None of the above  Optionally, add details in notes (tab)",
            "  deepseek-v4.1-flash medium · ~\\AppData\\Local\\Temp\\probe-codex-q-20261008 · 看 D:/Program Files/Git/plan  Plan mode",
            "  tab to add notes | enter to submit answer | esc to interrupt",
        ])
    }

    /// 证据 `codex-q-20261009-s4-tab-ensure.txt` FIXTURE 21–29：S4 非 Other 行 tab ensure 帧（Azure/AWS）：tab = 选中焦点行 + 开 notes 输入态的复合键，字符层计数段仍显示 `(1 unanswered)`（选中态落账滞后一帧）。
    pub(crate) fn codex_s4_tab_ensure() -> Vec<String> {
        lines(&[
            "  Question 1/1 (1 unanswered)",
            "  Which cloud?",
            "",
            "  › 1. Azure              使用 Azure 作为云平台。",
            "    2. AWS                使用 AWS 作为云平台。",
            "    3. None of the above  Optionally, add details in notes (tab)",
            "  deepseek-v4.1-flash medium · ~\\AppData\\Local\\Temp\\probe-codex-q-20261008 · 看 D:/Program Files/Git/plan  Plan mode",
            "  tab to add notes | enter to submit answer | esc to interrupt",
            "",
        ])
    }

    /// 证据 `codex-q-20261009-s1-summary-recap.txt` FIXTURE 12–16：S1 交卷后摘要屏（N=2）：头 `Questions 2/2 answered` + `answer: <选项文本>` 行（FIXTURE 15/16 为读取抖动帧，已按底料 §2 多帧还原词形；该帧属 §0 方法备注 1 管道瞬态）。
    pub(crate) fn codex_answered_summary() -> Vec<String> {
        lines(&[
            "• Questions 2/2 answered",
            "  • Which DB?",
            "    answer: SQLite",
            "  • Which cache?",
            "    answer: Memcached",
        ])
    }

    /// 证据 `codex-q-20261009-s2-summary-recap.txt` FIXTURE 09–15：S2 交卷后摘要屏（N=3）：头 `Questions 3/3 answered` + 三组 answer 行。
    pub(crate) fn codex_answered_summary3() -> Vec<String> {
        lines(&[
            "• Questions 3/3 answered",
            "  • Which DB?",
            "    answer: Postgres",
            "  • Which cache?",
            "    answer: Redis",
            "  • Which language?",
            "    answer: Rust",
        ])
    }

    /// 证据 `codex-q-20261009-e-wrap.txt` FIXTURE 21–29：E 长 label 单行形态：63 字符 label 不折行，desc 列右移（对齐到最长 label 后两格）。
    pub(crate) fn codex_e_wrap() -> Vec<String> {
        lines(&[
            "  Question 1/1 (1 unanswered)",
            "  Which deployment target?",
            "",
            "  › 1. Amazon Elastic Kubernetes Service with multi-region fleet  使用 Amazon EKS 多区域集群部署。",
            "    2. Plain Docker Compose on a single VM                        在单台虚拟机上使用 Docker Compose 部署。",
            "    3. None of the above                                          Optionally, add details in notes (tab)",
            "  deepseek-v4.1-flash medium · ~\\AppData\\Local\\Temp\\probe-codex-q-20261008 · 看 D:/Program Files/Git/plan  Plan mode",
            "  tab to add notes | enter to submit answer | esc to interrupt",
            "",
        ])
    }

    /// codex 焦点位移派生（共享实现）：把基底屏里 `›` 所在选项行的焦点标记移到
    /// `option_lines[idx]` 指向的选项行（基底 idx 即原样返回）。行号按各基底屏
    /// 的选项区布局点名——codex 屏取段不含滚回区，行号即段内索引。
    /// 派生焦点行 `› ` 后带三空格与活体两空格缩进微差——解析器 trim 宽容，不影响判据。
    fn codex_move_focus(base: &[String], option_lines: &[usize], idx: usize) -> Vec<String> {
        let mut out = base.to_vec();
        let from = out
            .iter()
            .position(|l| l.contains('›'))
            .expect("codex 基底屏必有焦点行");
        out[from] = out[from].replacen("› ", "  ", 1);
        let to = option_lines[idx];
        out[to] = out[to].replacen("  ", "› ", 1);
        out
    }

    /// S1-Q1 面板焦点派生：idx=0 即基底；1=SQLite 2=None of the above。
    /// （当前无消费点——非末题走数字直选；保留给未来「非末题走位」类场景，避免
    /// 下轮要用时重写。`#[allow(dead_code)]` 显式登记而非删函数。）
    #[allow(dead_code)]
    pub(crate) fn codex_s1_q1_focused(idx: usize) -> Vec<String> {
        const OPTION_LINES: [usize; 3] = [4, 5, 6];
        if idx == 0 {
            return codex_s1_q1();
        }
        codex_move_focus(&codex_s1_q1(), &OPTION_LINES, idx)
    }

    /// S1-Q2 面板（4 选项屏）焦点派生：idx=0 即基底；1=Memcached 2=None needed 3=None of the above。
    pub(crate) fn codex_s1_q2_focused(idx: usize) -> Vec<String> {
        const OPTION_LINES: [usize; 4] = [3, 4, 5, 6];
        if idx == 0 {
            return codex_s1_q2();
        }
        codex_move_focus(&codex_s1_q2(), &OPTION_LINES, idx)
    }

    /// S1-Q2 归零屏焦点派生：idx=0 即基底；0=Redis 1=Memcached（基底焦点）2=None needed 3=None of the above。
    pub(crate) fn codex_s1_q2_zero_focused(idx: usize) -> Vec<String> {
        const OPTION_LINES: [usize; 4] = [3, 4, 5, 6];
        if idx == 1 {
            return codex_s1_q2_zero();
        }
        codex_move_focus(&codex_s1_q2_zero(), &OPTION_LINES, idx)
    }
}

#[cfg(test)]
mod scratch_debug {
    #[test]
    fn scratch_single_own_row() {
        let lines = vec![
            "1. alpha".to_string(),
            "2. bravo".to_string(),
            "3. charlie".to_string(),
            "4. Type your own answer".to_string(),
            "↑ select  enter submit  esc dismiss".to_string(),
        ];
        let r = super::opencode_single_own_row(&lines);
        eprintln!("single_own_row = {r:?}");
        let d = crate::inject::dialect::own_answer_dialect("opencode").unwrap();
        eprintln!(
            "has_checkboxes = {}",
            super::opencode_page_has_checkboxes(&lines, &d)
        );
    }
}
