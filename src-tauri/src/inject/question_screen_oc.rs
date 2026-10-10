//! 题屏行块解析 → 统一快照 schema（2026-10-05 推广批 F3；2026-10-05 深夜扩
//! kimi 多选页——文件名沿用 oc，模块顶注释申报双工具）。
//! 与 claude 的 `question.rs::question_screen_snapshot` 同形（`QuestionScreenSnapshot`，
//! 前端对位逻辑零改动复用）。方言来源 = `dialect::own_answer_dialect("opencode")`
//! （勾选标记/入口标签单一真源）。
//!
//! 页判据：题页 footer 含 `enter toggle`（用户 2026-10-04 截图实测；Confirm 页是
//! `enter submit`、题页无 submit 锚——戊探A ④）→ 非题页返回 None（GET/回执按
//! 「读不到快照」保守降级，前端维持本地状态）。
//!
//! 字段口径：
//! - `checked` = 编号勾选行按屏上序（`Some(bool)`；opencode 全选项带勾选框）；
//! - `heading` = 首个编号行之前的非空行拼接（题干区，含页签行——前端做包含匹配，
//!   多余行不影响对位）；
//! - `free_text` / `free_text_present` = own answer 行内容（标签行 = 未填 →
//!   `present=true, text=None`；已保存 = 内容；无编号行形态异常时不误报）。

use crate::inject::dialect::own_answer_dialect;
use crate::inject::question::QuestionScreenSnapshot;

pub const OPENCODE_QUESTION_PAGE_ANCHOR: &str = "enter toggle";

fn strip(l: &str) -> &str {
    l.trim()
        .trim_start_matches(['\u{2503}', '\u{2502}', '\u{2192}', '\u{276f}'])
        .trim_start()
}

/// opencode **单选页快照**（2026-10-05 单选适配）：单选页无勾选框（编号行无
/// 标记），footer `enter submit` 与确认页同词形不能当页锚——结构判据：连续
/// 编号行（1..=n）≥2 且全部无勾选标记，末行 = own answer 行。
///
/// 产出：`checked = [null × 选项数]`（own 行不计；单选无勾选语义，前端同步
/// 链路原生支持 null 项）、`free_text = 末行内容（≠占位标签时，含用户残留/
/// 手动作答——正是「确认卡显示未作答」「无覆盖写入按钮」两个实机症状的
/// 同步通道）。
///
/// 误报面（会话正文里的编号列表）：由 GET 的 pending-question 门（仅挂卡会话
/// 拉快照）+ 前端 heading 对位（findQuestionByHeading）双重兜底。
fn opencode_single_select_snapshot(
    lines: &[String],
    d: &crate::inject::dialect::OwnAnswerDialect,
) -> Option<QuestionScreenSnapshot> {
    // **只看页切片**（2026-10-06 GET 断链定案）：transcript 里模型的「1. …
    // 2. …」编号说明文字先于真题页被扫进 numbered，真题页首行断号 → 整屏
    // 放弃——18:03-18:26 全部 GET「解析失败」的根因。从最后一个 `Questions`
    // 页首锚之后扫 = 只看活对话框区（heading 也随之变干净：tab 栏+题干）。
    let lines = crate::inject::question::opencode_question_page_slice(lines)?;
    let mut numbered: Vec<(u32, String)> = Vec::new();
    let mut heading_parts: Vec<&str> = Vec::new();
    for l in lines {
        let t = strip(l);
        if let Some((n, text, _)) = crate::inject::dialog::parse_option_line(t) {
            // 勾选标记在场 = 多选形态页（应由多选分支处理；这里出现 = 形态混杂，
            // 保守放弃）
            let lower = t.to_lowercase();
            if d.checked_markers.iter().any(|m| lower.contains(m))
                || d.unchecked_markers.iter().any(|m| lower.contains(m))
            {
                return None;
            }
            // 连续性：编号必须严格 1..=n（断号 = 会话正文里的编号列表，非题页）
            if n as usize != numbered.len() + 1 {
                return None;
            }
            numbered.push((n, text));
            continue;
        }
        if numbered.is_empty() && !t.is_empty() {
            heading_parts.push(t);
        }
    }
    if numbered.len() < 2 {
        return None; // 选项 + own 行至少 2 行
    }
    let (_, own_content) = numbered.last()?;
    // own 行若是当前答案，行尾带 " ✓"（2026-10-06 活体定案）——剥掉再入
    // free_text，卡面「已写入」不该显示对勾
    let own_clean = own_content.trim_end().trim_end_matches('✓').trim_end();
    let free_text = if own_clean.to_lowercase().contains(d.label) {
        None // 鲜态占位标签 = 未填
    } else {
        Some(own_clean.to_string()) // 用户残留/手动作答（本轮同步目标）
    };
    // checked：**逐行真值**（2026-10-06 活体定案：单选选中行尾 " ✓" →
    // Some(true)，未选行 Some(false)）——卡面勾选态/DigitKey 翻转判定的数据源
    let opt_count = numbered.len().saturating_sub(1);
    let checked: Vec<Option<bool>> = (1..=opt_count)
        .map(|n| crate::inject::question::opencode_option_checked_at(lines, n, d))
        .collect();
    Some(QuestionScreenSnapshot {
        // 长度 = 选项数（own 行不计）——前端 advance 归属校验按载荷
        // options.length 比长度，own 行混入恒错 1（01:31 断链教训）
        checked,
        free_text,
        free_text_present: true,
        heading: heading_parts.join(" "),
    })
}

pub fn opencode_question_screen_snapshot(lines: &[String]) -> Option<QuestionScreenSnapshot> {
    let d = own_answer_dialect("opencode")?;
    if !lines
        .iter()
        .any(|l| l.to_lowercase().contains(OPENCODE_QUESTION_PAGE_ANCHOR))
    {
        // 多选页锚不在场 → 尝试**单选页**结构判据（2026-10-05 单选适配）：单选页
        // 无勾选框（编号行无标记），footer `enter submit` 与确认页同词形不能当
        // 页锚——改用**结构判据**：连续编号行（1..=n）≥2 且全部无勾选标记，末行
        // = own answer 行。确认页（Submit tab）无编号行 → None ✓。
        return opencode_single_select_snapshot(lines, &d);
    }
    let is_numbered_checked = |t: &str| -> Option<bool> {
        crate::inject::dialog::parse_option_line(t)?;
        let lower = t.to_lowercase();
        if d.checked_markers.iter().any(|m| lower.contains(m)) {
            Some(true)
        } else if d.unchecked_markers.iter().any(|m| lower.contains(m)) {
            Some(false)
        } else {
            None // 编号行但无勾选框（形态异常）——不计入
        }
    };
    let mut checked: Vec<Option<bool>> = Vec::new();
    let mut heading_parts: Vec<&str> = Vec::new();
    // 编号行内容（勾选标记 `]` 之后）——末行用于 own answer 保存态判定
    let mut last_row_text: Option<String> = None;
    for l in lines {
        let t = strip(l);
        if let Some(state) = is_numbered_checked(t) {
            checked.push(Some(state));
            last_row_text = Some(
                t.rfind(']')
                    .map(|i| t[i + 1..].trim_start().to_string())
                    .unwrap_or_default(),
            );
            continue;
        }
        if checked.is_empty() && !t.is_empty() {
            // 题干区：首个编号行之前的非空行
            heading_parts.push(t);
        }
    }
    if checked.is_empty() {
        return None; // 无选项行 = 不是可对位的题屏
    }
    // own answer 行（末编号行）：内容 == 标签 → 鲜态（present、无文本）；
    // 内容 != 标签 → 已保存（present + 屏上文本）
    let free_text_present = true;
    let saved = last_row_text.unwrap_or_default();
    let free_text = if saved.is_empty() || saved.to_lowercase().contains(d.label) {
        None
    } else {
        Some(saved)
    };
    Some(QuestionScreenSnapshot {
        checked,
        free_text,
        free_text_present,
        heading: heading_parts.join(" "),
    })
}

/// kimi **多选页快照**（2026-10-05 探测批 K 形态 + evidence/kimi-multi 实拍）：
/// 页锚 = footer 含 `tab switch`（kimi 多选页专属词形；编辑态 footer 亦含之，
/// 但编辑态行内容即输入中文字——快照如实回传，属可接受）。行语法（K2 定案）：
/// `[ ] label` / 已选 `[?] label`；Other 行**无编号**（`[ ] Other` 鲜态 /
/// `[?] Other: <文本>` 已存）。
///
/// 产出：`checked` = 逐选项 Some(已选)；`free_text` = Other 行文本（`Other: `
/// 之后；鲜态 None）；`free_text_present` = Other 行在场；`heading` = 首个
/// checkbox 行之前的非空行拼接。
///
/// 无 checkbox 行 → None（非 kimi 多选页：单选页/会话正文）。误报面 = GET
/// pending 门 + 前端 heading 对位双重兜底（同单选分支）。
/// kimi **Review 汇总页逐题摘要**（2026-10-07 新增，确认卡权威源切换）：
/// 形态（2.1.1 活体截图）：
///
/// ```text
/// Review your answer before submit
///  Q  <题干>
///  →  <答案>            （答案折行归并待实测样本）
///  Ready to submit your answers?
///  → [1] Submit
///    [2] Cancel
/// ```
///
/// 解析规则：`Q ` 前缀行开新题，其后第一个 `→ ` 行（非 `→ [N]` 确认区）为该题
/// 答案；`Ready to submit` 后的 Submit/Cancel 区不计入。未答题答案为空串。
/// 返回 (题干, 答案) 有序对；无 Q 行 → None（非 Review 页/解析不出）。
const REVIEW_ARROW: char = '→';

pub fn kimi_review_summary(lines: &[String]) -> Option<Vec<(String, String)>> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut in_review = false;
    for raw in lines {
        let t = raw.trim();
        if !in_review {
            if t.starts_with("Review your answer before submit") {
                in_review = true;
            }
            continue;
        }
        if t.starts_with("Ready to submit") {
            break; // Submit/Cancel 区及其后不计入
        }
        if t.is_empty() {
            continue;
        }
        if let Some(q) = t.strip_prefix("Q ") {
            pairs.push((q.trim().to_string(), String::new()));
            continue;
        }
        // → 答案行（非 → [N] 确认区）：首行或折行追加
        if let Some(ans) = t.strip_prefix(REVIEW_ARROW).map(str::trim) {
            if ans.starts_with('[') {
                continue; // [1] Submit / [2] Cancel 确认区
            }
            if let Some(last) = pairs.last_mut() {
                if last.1.is_empty() {
                    last.1 = ans.to_string();
                } else {
                    last.1.push(' ');
                    last.1.push_str(ans);
                }
            }
            continue;
        }
        // 折行归并（2026-10-07 17:03 实录形态）：无前缀非空行 = 上一项折行
        // ——Q 行后 → 题干折行（Q3「（单/选）」）；→ 行后 → 答案折行
        if let Some(last) = pairs.last_mut() {
            if last.1.is_empty() {
                last.0.push(' ');
                last.0.push_str(t);
            } else {
                last.1.push(' ');
                last.1.push_str(t);
            }
        }
    }
    (!pairs.is_empty()).then_some(pairs)
}

pub fn kimi_question_screen_snapshot(lines: &[String]) -> Option<QuestionScreenSnapshot> {
    // kimi 不在 OwnAnswerDialect（own answer 编排未接入）——标记直接内联。
    // **已选字形多候选**（账本吸收文案漂移的同一口径）：`[?]`=探测批 K（2.x），
    // `[✓]`=戊探B（2.0.2 空格/回车切勾实录），`[√]`=2.1.1 用户实机（2026-10-06
    // 截图）——三版本三字形，逐行任一命中即已选。未选恒 `[ ]`。
    const KIMI_CHECKED: &[&str] = &["[?]", "[✓]", "[√]"];
    const KIMI_UNCHECKED: &[&str] = &["[ ]"];
    let mut checked: Vec<Option<bool>> = Vec::new();
    let mut free_text: Option<String> = None;
    let mut other_seen = false;
    // **heading = `? ` 题干行**（2026-10-06 刷新对位修复）：旧形态「checkbox 行
    // 之前的非空行拼接」会把页头杂讯（`question` 标题、tab 栏 `优化方向 新增能力
    // …`、footer 片段）拼进 heading → 前端 findQuestionByHeading 对位必然失败 →
    // GET 刷新永远不知道终端停在第几题（用户实录）。2.1.1 活体定案：题干行形
    // `? <题干>`（单选/多选页同形，夹具 kimi-211-question-single.txt）——以它为
    // heading，剥掉 `? ` 前缀后与载荷 question 字段对位（前端 exact/partial 单一
    // 命中判据）。无 `? ` 行 → heading 空（对位自然放弃，保守不猜）。
    let mut heading: String = String::new();
    for l in lines {
        let t = strip(l);
        if heading.is_empty() && t.starts_with("? ") {
            heading = t[2..].trim().to_string();
        }
        let lower = t.to_lowercase();
        let boxed = KIMI_CHECKED
            .iter()
            .chain(KIMI_UNCHECKED.iter())
            .any(|m| lower.contains(m));
        if !boxed {
            continue;
        }
        // 内容 = 勾选标记之后的部分
        let content = KIMI_CHECKED
            .iter()
            .chain(KIMI_UNCHECKED.iter())
            .find_map(|m| lower.find(m).and_then(|i| t.get(i + m.len()..)))
            .map(|c| c.trim_start().to_string())
            .unwrap_or_default();
        let is_selected = KIMI_CHECKED.iter().any(|m| lower.contains(m));
        // Other 行（kimi 多选 Other 无编号）：内容以 `other` 开头
        if content.to_lowercase().starts_with("other") {
            other_seen = true;
            // `[?] Other: <文本>` → 冒号后为已存文本；`[ ] Other` → 鲜态
            free_text = content
                .split_once(':')
                .map(|(_, v)| v.trim().to_string())
                .filter(|v| !v.is_empty());
            continue;
        }
        checked.push(Some(is_selected));
    }
    if checked.is_empty() {
        // **单选页兜底**（2026-10-06 刷新对位修复）：无勾选框行可能是**单选题页**
        // （选项 = `[N] label` 编号行形，无勾选框）——此前返回 None → GET 无快照 →
        // 刷新永远回第 1 题。单选页出 heading-only 快照（checked 空 = 前端 heading
        // 对位后不写勾选态）+ Other 行文本（单选 Other 有编号，free_text 同面）。
        let arrow = char::from_u32(0x2192).unwrap();
        let chevron = char::from_u32(0x276F).unwrap();
        let has_bracket_row = lines.iter().any(|l| {
            let t = l.trim().trim_start_matches([arrow, chevron]).trim_start();
            t.starts_with('[')
                && t.find(']').is_some_and(|c| {
                    let n = &t[1..c];
                    n.len() == 1 && n.chars().next().is_some_and(|d| d.is_ascii_digit())
                })
        });
        if !has_bracket_row {
            return None;
        }
        let free_text = lines
            .iter()
            .rev()
            .filter_map(|l| {
                let t = l.trim().trim_start_matches([arrow, chevron]).trim_start();
                if !t.starts_with('[') {
                    return None;
                }
                let close = t.find(']')?;
                let after = t[close + 1..].trim();
                after
                    .to_lowercase()
                    .starts_with("other")
                    .then(|| after.split_once(':').map(|(_, v)| v.trim().to_string()))
                    .flatten()
                    .filter(|v| !v.is_empty())
            })
            .next();
        return Some(QuestionScreenSnapshot {
            checked: Vec::new(),
            free_text: free_text.clone(),
            free_text_present: free_text.is_some(),
            heading,
        });
    }
    Some(QuestionScreenSnapshot {
        checked,
        free_text,
        free_text_present: other_seen,
        heading,
    })
}

// ===== codex 0.160.0 问答面板快照解析器（2026-10-09 取证批 Task 4）=====
// 底料 `docs/superpowers/specs/2026-10-08-codex-160-question-屏读底料.md` §2；
// 夹具 = `question::live_fixtures::codex_*`（活体逐字取证，F 证据根
// `%USERPROFILE%\mam-probe-m6r\evidence\`）。屏形事实（取证定案）：
//
// ```text
//   Question 1/2 (2 unanswered)          ← 题号头：未答 `Question i/N (M unanswered)`
//   Which DB?                            ← 题干（可折行）
//                                         ← 空行（面板定形）
//   › 1. Postgres           使用 PostgreSQL 作为数据库。   ← ›=焦点行前缀
//     2. SQLite             使用 SQLite 作为数据库。
//   [  deepseek-... · ...  Plan mode      ← 状态栏行（活体变体，S2/S3）
//   tab to add notes | enter to submit answer | ←/→ to navigate questions | esc to interrupt
// ```
//
// - 归零词形：`Question 2/2`（计数段整个消失）= 全答完；
// - 摘要屏头 `• Questions 2/2 answered`（复数 Questions + answered）**不是面板**
//   → None（header 解析天然不匹配：strip_prefix("Question ") 后 `s 2/2...` 解析失败）；
// - `›` 前缀行 = 焦点行；无 `›` → focused=None 不猜；
// - 长 label 单行加宽不折行，desc 列浮动右移 → 按 ≥2 连续空格截断；
// - 滚回残留 → **last-pair-wins**：取最后一个「题号头 + 配对成立」的块。
//
// **配对判据（纯账本锚，2026-10-09 裁决收口）**：账本 footer 三槽位任一命中
// （`anchor_ledger::detect`，权威锚），唯一判据。footer 恒在场（活体定案）；
// 单题三段 footer 无 navigate 段，故三槽（Q_FOOTER_NAVIGATE / Q_FOOTER_SUBMIT_ANSWER
// / Q_FOOTER_SUBMIT_ALL）任一命中即配对成立。单独的题号头残留（无 footer 锚）
// 必须拒绝——防正文编号列表误报。曾经的「空行定形」兜底已撤（无谓的误报面扩大：
// 真机上 footer 恒在场，锚词配对即设计 §2.2 的「上下界锚」意图）。

/// codex 问答面板快照（设计 §3.1 wire 契约，camelCase）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexQuestionSnapshot {
    /// 当前题号（0 起）
    pub question_idx: usize,
    /// 总题数
    pub question_total: usize,
    /// 未答数（计数段消失 = 0，底料定案 #2）
    pub unanswered: usize,
    /// 当前题是否末题（i == n）
    pub is_last: bool,
    /// 题干（剥前导空白；折行简单拼接）
    pub heading: String,
    /// 选项 label（剥 (Recommended) 尾缀与描述列）
    pub options: Vec<String>,
    /// 焦点选项（0 起；`›` 行读不到 = None，不猜）
    pub focused: Option<usize>,
}

/// 焦点标记 `›`（U+203A）——codex 面板焦点行前缀。注意与 claude 的 `❯`
/// （U+276F）不同，不能复用 `strip` 的剥前缀表。
const CODEX_FOCUS_MARK: char = '\u{203A}';

/// 解析题号头：`Question {i}/{n}` | `Question {i}/{n} ({k} unanswered)`
/// → `Some((i, n, Some(k) | None))`。
///
/// - 归零词形 `Question 2/2`（计数段整个消失）→ k = None；
/// - 摘要屏头 `Questions 3/3 answered`（复数 + answered）天然不匹配：
///   strip_prefix("Question ") 后剩 `s 3/3 answered`，`split_once(' ')` 得
///   ratio = "s" 解析失败 → None；
/// - i/n 为 0 或 i > n → None（防御）。
///
/// `pub(crate)`：`question::live_probe_tests::codex_question_live_probe` 的
/// None 成因诊断复用生产词形（不重造判据）。
pub(crate) fn parse_codex_question_header(line: &str) -> Option<(usize, usize, Option<usize>)> {
    let rest = line.trim().strip_prefix("Question ")?;
    // ratio = "i/n"（计数段若有，由首个空格分开）
    let (ratio, counter) = match rest.split_once(' ') {
        Some((r, c)) => (r, Some(c)),
        None => (rest, None),
    };
    let (i, n) = ratio.split_once('/')?;
    let i: usize = i.parse().ok()?;
    let n: usize = n.parse().ok()?;
    if i == 0 || n == 0 || i > n {
        return None;
    }
    // 计数段 `(k unanswered)`；归零词形 k=None → 0（调用方 unwrap_or(0)）
    let k = counter.and_then(|c| {
        let c = c.trim().strip_prefix('(')?.strip_suffix(')')?;
        let k = c.trim().strip_suffix("unanswered")?.trim().parse().ok()?;
        Some(k)
    });
    Some((i, n, k))
}

/// 解析选项行：`{空白}(› )?{N}. label{≥2 空格}描述` → `Some((N, label, focused))`。
///
/// - 行首 2 空格缩进 + `›`（如 `  › 1. Postgres`）：trim_start 后判 `›`；
/// - label 剥 `(Recommended)` 尾缀（取证未见，设计 §2.1 有——防御性保留）与
///   描述列（连续 ≥2 空格截断；长 label 单行加宽时 desc 列浮动右移，同判据）；
/// - 编号单数字也多数字均可（防御性）。
fn parse_codex_option_row(line: &str) -> Option<(usize, String, bool)> {
    let t = line.trim_start();
    let focused = t.starts_with(CODEX_FOCUS_MARK);
    // `›` 是多字节字符（U+203A，3 字节）——按 len_utf8 切，不得按字节 1 切
    let t = if focused {
        t[CODEX_FOCUS_MARK.len_utf8()..].trim_start()
    } else {
        t
    };
    // 编号 + `. `
    let dot = t.find('.')?;
    let n: usize = t[..dot].trim().parse().ok()?;
    if n == 0 {
        return None;
    }
    let mut label = t[dot + 1..].trim_start();
    // 剥 (Recommended) 尾缀（先剥描述列后剥尾缀均可；尾缀也可能被描述列截断
    // 逻辑带走——顺序：先截描述列，再剥尾缀，最后 trim_end）
    if let Some(pos) = label.find("  ") {
        label = &label[..pos];
    }
    let label = label
        .trim_end()
        .strip_suffix("(Recommended)")
        .map(str::trim_end)
        .unwrap_or_else(|| label.trim_end());
    if label.is_empty() {
        return None;
    }
    Some((n, label.to_string(), focused))
}

/// codex 问答面板快照解析主入口：**last-pair-wins**——从后往前找题号头，其下方
/// **至屏尾**的窗内账本三 footer 槽位（Q_FOOTER_NAVIGATE / Q_FOOTER_SUBMIT_ANSWER
/// / Q_FOOTER_SUBMIT_ALL）任一命中即配对成功并从该题号头向下解析（唯一判据，见
/// 模块注释「配对判据」）。单独的题号头残留（无 footer 锚）不算。
///
/// 配对窗扩至屏尾（评审 P2-5）：原 ~12 行固定窗会把「长选项/折行题干 + footer
/// 被挤出窗」的面板误判为「无 footer 配对」→ 回退错配到更早的残留旧面板——
/// 屏高固定而面板行数浮动，固定窗无证据支撑；屏尾就是活体 footer 所在的唯一
/// 有界承诺。
///
/// 非面板（普通屏 / 摘要屏 / 确认屏无题号头）→ None，调用方按「读不到快照」
/// 保守降级。
pub fn codex_question_screen_snapshot(lines: &[String]) -> Option<CodexQuestionSnapshot> {
    let lowered: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();
    let ledger_slot_hit = |start: usize, end: usize| -> bool {
        let window = &lowered[start..end.min(lowered.len())];
        [
            crate::inject::anchor_ledger::slot::Q_FOOTER_NAVIGATE,
            crate::inject::anchor_ledger::slot::Q_FOOTER_SUBMIT_ANSWER,
            crate::inject::anchor_ledger::slot::Q_FOOTER_SUBMIT_ALL,
        ]
        .iter()
        .any(|slot| {
            crate::inject::anchor_ledger::detect(
                window,
                "codex",
                crate::inject::anchor_ledger::scenario::QUESTION,
                slot,
            )
            .is_some()
        })
    };
    // last-pair-wins：从后往前扫题号头，首个配对成立者即当前面板
    for (hi, line) in lines.iter().enumerate().rev() {
        let (idx, total, unanswered) = match parse_codex_question_header(line) {
            Some(h) => h,
            None => continue,
        };
        // 配对窗 = 题号头下方**至屏尾**（评审 P2-5：防长选项/折行题干把 footer
        // 挤出固定窗、防窗尾裁切后回退错配旧面板）
        if !ledger_slot_hit(hi + 1, lines.len()) {
            continue; // 单独题号头残留（无 footer 锚）——不算
        }
        // 从题号头向下解析面板内容
        let mut heading_parts: Vec<String> = Vec::new();
        let mut options: Vec<String> = Vec::new();
        let mut focused: Option<usize> = None;
        let mut expect_next = 1usize; // 编号连续性：1..=m，跳变即断
        for l in lines[hi + 1..].iter() {
            let t = l.trim();
            if let Some((n, label, is_focus)) = parse_codex_option_row(t) {
                if n != expect_next {
                    break; // 编号跳变 = 面板选项区结束/形态异常
                }
                if is_focus {
                    focused = Some(n - 1);
                }
                options.push(label);
                expect_next += 1;
                continue;
            }
            if options.is_empty() {
                // 题干区：header 与选项区之间的非空行（跳过含 unanswered 的
                // 行防计数头折行重复；空行不进 heading）
                if t.is_empty() {
                    continue;
                }
                if t.contains("unanswered") {
                    continue;
                }
                heading_parts.push(t.to_string());
                continue;
            }
            // 选项区已开始：空行 / 状态栏行 / footer / 其他行 = 块终点
            break;
        }
        if options.is_empty() {
            continue; // 无选项行（形态异常）——继续向前找更早的配对块
        }
        return Some(CodexQuestionSnapshot {
            question_idx: idx - 1,
            question_total: total,
            unanswered: unanswered.unwrap_or(0),
            is_last: idx == total,
            heading: heading_parts.join(" "),
            options,
            focused,
        });
    }
    None
}

/// 选项行文本判据（与 [`parse_codex_option_row`] 的编号解析**同源**）：
/// 到首个 `.` 之前的串全 ASCII 数字且非空 = `N. label` 选项行。
///
/// **2026-10-11 评审 Important 3 修正**：旧判据「首字符是数字即跳过」把数字开头的
/// 备注文本（如「3 点建议」）误当选项行——notes 行系统性失明，清空路径还会假报
/// 成功（读不到 note = 无字）。同源判据下「3 点建议」的 `3` 后无 `.`，不再误判。
fn looks_like_option_text(text: &str) -> bool {
    let Some(dot) = text.find('.') else {
        return false;
    };
    let head = &text[..dot];
    !head.is_empty() && head.bytes().all(|b| b.is_ascii_digit())
}

/// codex 面板 **notes 行文本**解析（2026-10-10 用户指令：「note 位置但凡有输入，
/// 一定要显示在远端页面上」——GET/回执消费）：
/// 题号头之后、首个 `› ` 前缀**非选项**行（选项行 `› N. label` 带编号点，已排除）。
/// `› Add notes` 占位 = 空备注 → `None`；composer 提示行（面板外）按已知文案排除。
///
/// 只认**题号头之下**的首个命中——面板之下的滚回区/composer 草稿不误收。
pub fn codex_notes_row_text(lines: &[String]) -> Option<String> {
    // 定位最后一个题号头（面板顶；与 codex_question_screen_snapshot 的
    // last-pair-wins 同向——只认最新面板）
    let header = lines
        .iter()
        .rposition(|l| parse_codex_question_header(l).is_some())?;
    for l in lines[header + 1..].iter() {
        let t = l.trim_start();
        let Some(rest) = t.strip_prefix(CODEX_FOCUS_MARK) else {
            continue;
        };
        let text = rest.trim();
        if text.is_empty() {
            continue;
        }
        // 选项行（`› N. …`）跳过——同源判据（数字串 + `.`），数字开头的备注不误伤
        if looks_like_option_text(text) {
            continue;
        }
        let lower = text.to_lowercase();
        if lower.starts_with("add notes") {
            return None; // 占位词 = 空备注（如实 None）
        }
        if lower.starts_with("ask codex") {
            continue; // composer 提示行（面板外已知文案）
        }
        return Some(text.to_string());
    }
    None
}

/// codex 面板 **notes 输入行就绪**判定（2026-10-10 实机 20:55 误拦教训：footer 翻
/// 转后立刻打字，字符落在输入行挂载完成之前——末帧 notes 态关闭、字消失）。就绪 =
/// 题号头之后存在 `› ` 前缀非选项行（**占位 `Add notes` 或已输入文本都算**——
/// 占位行本身就是输入行挂载的可见标记）。
pub fn codex_notes_input_ready(lines: &[String]) -> bool {
    let Some(header) = lines
        .iter()
        .rposition(|l| parse_codex_question_header(l).is_some())
    else {
        return false;
    };
    for l in &lines[header + 1..] {
        let t = l.trim_start();
        let Some(rest) = t.strip_prefix(CODEX_FOCUS_MARK) else {
            continue;
        };
        let text = rest.trim();
        if text.is_empty() {
            continue;
        }
        if looks_like_option_text(text) {
            continue; // 选项行（同源判据——数字开头的备注不误伤）
        }
        if text.to_lowercase().starts_with("ask codex") {
            continue; // composer 提示行（面板外）
        }
        return true; // 占位或文本——输入行已挂载
    }
    false
}

#[cfg(test)]
mod tests {

    /// **数字开头备注不误判为选项行**（2026-10-11 评审 Important 3）：旧判据「首字符
    /// 是数字即跳过」把「3 点建议」当 `3. …` 选项行——notes 行失明、清空路径假报
    /// 成功。同源判据（数字串 + `.`）下必须照常读出。还原动作：把
    /// `looks_like_option_text` 退回首字符判据 → 本用例先红。
    #[test]
    fn codex_notes_row_text_allows_digit_prefixed_note() {
        let frame = vec![
            "  Question 1/1 (1 unanswered)".to_string(),
            "  你偏好哪种协作方式?".to_string(),
            "  › 1. 直接开干 (Recommended)  我直接动手实现".to_string(),
            "    2. None of the above  Optionally, add details in notes (tab)".to_string(),
            "  › 3 点建议：先讨论再动手".to_string(),
            "  tab or esc to clear notes | enter to submit answer".to_string(),
        ];
        assert_eq!(
            codex_notes_row_text(&frame).as_deref(),
            Some("3 点建议：先讨论再动手"),
            "数字开头的备注必须照常读出（不是选项行）"
        );
        assert!(codex_notes_input_ready(&frame));
        // 对照：真选项行（`3. label`）仍被跳过——扫到的是下一行占位 → None
        let option_like = vec![
            "  Question 1/1 (1 unanswered)".to_string(),
            "  你偏好哪种协作方式?".to_string(),
            "  › 1. 直接开干".to_string(),
            "  › 3. 真选项行".to_string(),
            "  › Add notes".to_string(),
            "  tab or esc to clear notes | enter to submit answer".to_string(),
        ];
        assert_eq!(codex_notes_row_text(&option_like), None, "真选项行仍须跳过");
    }

    use super::*;

    fn multi_page() -> Vec<String> {
        vec![
            "OC | 优化重点: 你想加的动效".to_string(),
            "你希望这次优化重点放在哪些方面？（可多选）".to_string(),
            "".to_string(),
            "1. [ ] 画面美感与细节".to_string(),
            "2. [ ] 交互动效".to_string(),
            "3. [v] 性能与兼容性".to_string(),
            "4. [ ] 无障碍与可用性".to_string(),
            "5. [ ] 代码结构整洁".to_string(),
            "6. [ ] Type your own answer".to_string(),
            "⇆ tab  ↑↓ select  enter toggle  esc dismiss".to_string(),
        ]
    }

    #[test]
    fn oc_snapshot_parses_multi_select_page() {
        let snap = opencode_question_screen_snapshot(&multi_page()).expect("题页必须出快照");
        assert_eq!(
            snap.checked,
            vec![
                Some(false),
                Some(false),
                Some(true),
                Some(false),
                Some(false),
                Some(false)
            ]
        );
        assert!(snap.heading.contains("你希望这次优化重点放在哪些方面"));
        // own answer 鲜态：present=true、text=None
        assert!(snap.free_text_present);
        assert_eq!(snap.free_text, None);
    }

    #[test]
    fn oc_snapshot_saved_content_surface() {
        let mut saved = multi_page();
        saved[8] = "6. [v] 出场的人物需要增加一些".to_string();
        let snap = opencode_question_screen_snapshot(&saved).expect("已保存题页必须出快照");
        assert_eq!(snap.checked[5], Some(true));
        assert_eq!(snap.free_text.as_deref(), Some("出场的人物需要增加一些"));
    }

    #[test]
    fn oc_snapshot_parses_single_select_page() {
        // 2026-10-05 单选页实拍形态（34808 会话 + 用户截图）：无勾选框、
        // own 行内容为手写残留
        let single = vec![
            "Questions".to_string(),
            "使用场景".to_string(),
            "这个页面的主要使用场景是？（单选）".to_string(),
            "1. 手机浏览器独立打开".to_string(),
            "2. 手机+电脑都要好看".to_string(),
            "3. 嵌入到其他网页里".to_string(),
            "4. 能否融合上述2篇的".to_string(),
            "⇆ tab  ↑↓ select  enter confirm  esc dismiss".to_string(),
        ];
        let snap = opencode_question_screen_snapshot(&single).expect("单选页必须出快照");
        assert_eq!(
            snap.checked,
            vec![Some(false), Some(false), Some(false)],
            "单选未选行 = Some(false)（无任何标记）；own 行不计——长度必须等于\
             选项数，否则 advance 回执快照被前端归属校验整体拒收（2026-10-06 修复）"
        );
        assert_eq!(
            snap.free_text.as_deref(),
            Some("能否融合上述2篇的"),
            "残留必须同步（覆盖写入按钮的数据源）"
        );
        assert!(snap.free_text_present);
        assert!(snap.heading.contains("这个页面的主要使用场景是"));
    }

    #[test]
    fn oc_snapshot_single_select_fresh_placeholder_is_none_text() {
        let fresh = vec![
            "Questions".to_string(),
            "1. alpha".to_string(),
            "2. bravo".to_string(),
            "3. charlie".to_string(),
            "4. Type your own answer".to_string(),
            "↑ select  enter submit  esc dismiss".to_string(),
        ];
        let snap = opencode_question_screen_snapshot(&fresh).expect("鲜态单选页必须出快照");
        assert!(snap.free_text_present);
        assert_eq!(snap.free_text, None, "占位标签 = 未填");
        // 3 选项 + own 行：checked 只含选项行（own 行不计，2026-10-06 修复）；
        // 未选行 = Some(false)（裸 ✓ 定案后与括号页同口径）
        assert_eq!(snap.checked, vec![Some(false), Some(false), Some(false)]);
    }

    #[test]
    fn oc_snapshot_single_select_checkmark_row_is_true_and_own_tail_stripped() {
        // 2026-10-06 活体定案（PID 1880 dump 行35/行41）：单选选中行尾渲染
        // " ✓"（U+2713，无括号）；own 行是当前答案时同样带 ✓
        let marked = vec![
            "Questions".to_string(),
            "你说的「上述2篇」具体指什么？".to_string(),
            "1. 两张 SVG 插画".to_string(),
            "2. 两篇悬疑小说 ✓".to_string(),
            "3. 画面里的两/三组角色".to_string(),
            "4. 其他（我来说明）".to_string(),
            "5. 出场的人物需要增加一些 ✓".to_string(),
            "⇆ tab  ↑↓ select  enter confirm  esc dismiss".to_string(),
        ];
        let snap = opencode_question_screen_snapshot(&marked).expect("✓ 页必须出快照");
        assert_eq!(
            snap.checked,
            vec![Some(false), Some(true), Some(false), Some(false)],
            "行尾裸 ✓ = 选中（DigitKey 翻转判定 Some(false)→Some(true) 的数据源）"
        );
        // own 行 ✓ 尾剥掉——卡面「已写入」不显示对勾
        assert_eq!(snap.free_text.as_deref(), Some("出场的人物需要增加一些"));
    }

    #[test]
    fn oc_snapshot_single_select_none_on_broken_numbering() {
        // 会话正文里的编号列表（断号）不得误判为单选题页
        let list = vec![
            "1. 第一点".to_string(),
            "3. 第三点".to_string(),
            "4. 补充".to_string(),
        ];
        assert_eq!(opencode_question_screen_snapshot(&list), None);
    }

    #[test]
    fn kimi_snapshot_parses_multi_page_with_other_residue() {
        // 2026-10-05 探测批 K 实拍形态（evidence/kimi-multi/probe-scr-km 原样词形）
        let kimi_page = vec![
            " Shui guo    Submit".to_string(),
            "".to_string(),
            " ? ni xi huan na xie shui guo?".to_string(),
            "".to_string(),
            "  [ ] A apple".to_string(),
            "        Xuan xiang A: apple".to_string(),
            "  [?] B banana".to_string(),
            "  [ ] C cherry".to_string(),
            "  [?] Other: 有个想法".to_string(),
            "".to_string(),
            "  ↑↓ select  1-4 / ? toggle  ←/→/tab switch  esc cancel".to_string(),
        ];
        let snap = kimi_question_screen_snapshot(&kimi_page).expect("kimi 多选页必须出快照");
        assert_eq!(
            snap.checked,
            vec![Some(false), Some(true), Some(false)],
            "逐选项勾选态（Other 行不计入 checked）"
        );
        assert_eq!(
            snap.free_text.as_deref(),
            Some("有个想法"),
            "Other 残留同步"
        );
        assert!(snap.free_text_present);
        // heading = `? ` 题干行（2026-10-06 刷新对位修复：不再拼 tab 栏杂讯——
        // 前端 findQuestionByHeading 拿它与载荷 question 字段 exact/partial 对位）
        assert!(
            snap.heading.contains("ni xi huan na xie shui guo"),
            "heading 应为题干行剥 ? 前缀：{:?}",
            snap.heading
        );
        assert!(!snap.heading.contains("Submit"), "不含 tab 栏杂讯");
    }

    #[test]
    fn kimi_snapshot_fresh_other_is_none_text() {
        let fresh = vec![
            "  [ ] A apple".to_string(),
            "  [ ] B banana".to_string(),
            "  [ ] Other".to_string(),
            "  ↑↓ select  1-4 / ? toggle  ←/→/tab switch  esc cancel".to_string(),
        ];
        let snap = kimi_question_screen_snapshot(&fresh).expect("鲜态必须出快照");
        assert_eq!(snap.free_text, None, "鲜态 Other = 未填");
        assert!(snap.free_text_present);
    }

    #[test]
    fn kimi_snapshot_none_without_other_row() {
        // 无 Other 行（模型未给 Other 选项）→ 仍出快照（free_text_present=false）
        let no_other = vec![
            "  [ ] A apple".to_string(),
            "  [ ] B banana".to_string(),
            "  ↑↓ select  1-4 / ? toggle  ←/→/tab switch  esc cancel".to_string(),
        ];
        let snap = kimi_question_screen_snapshot(&no_other).expect("有选项行应出快照");
        assert_eq!(snap.checked, vec![Some(false), Some(false)]);
        assert!(!snap.free_text_present);
        // 无 checkbox 行（普通正文）→ None
        assert_eq!(
            kimi_question_screen_snapshot(&["1. 普通".to_string()]),
            None
        );
    }

    #[test]
    fn oc_snapshot_none_on_confirm_page() {
        let confirm = vec![
            "Questions".to_string(),
            "⇆ tab  enter submit  esc dismiss".to_string(),
        ];
        assert_eq!(opencode_question_screen_snapshot(&confirm), None);
    }

    /// codex 0.160.0 问答面板快照解析器（2026-10-09 取证批 Task 4，底料
    /// `docs/superpowers/specs/2026-10-08-codex-160-question-屏读底料.md` §2）。
    /// 夹具 = `question::live_fixtures::codex_*`（活体逐字取证，不得改夹具）。
    mod codex_snapshot_tests {
        use super::*;
        use crate::inject::question::live_fixtures;

        #[test]
        fn parses_s1_q1_fixture() {
            let s = codex_question_screen_snapshot(&live_fixtures::codex_s1_q1())
                .expect("S1-Q1 面板必解析");
            assert_eq!(s.question_idx, 0);
            assert_eq!(s.question_total, 2);
            assert_eq!(s.unanswered, 2);
            assert!(!s.is_last);
            assert_eq!(s.focused, Some(0));
            assert_eq!(s.options, vec!["Postgres", "SQLite", "None of the above"]);
            assert_eq!(s.heading, "Which DB?");
        }

        #[test]
        fn parses_s1_q2_four_options_with_all_footer() {
            let s = codex_question_screen_snapshot(&live_fixtures::codex_s1_q2()).unwrap();
            assert_eq!(s.question_idx, 1);
            assert!(s.is_last);
            assert_eq!(s.unanswered, 1);
            assert_eq!(
                s.options,
                vec!["Redis", "Memcached", "None needed", "None of the above"]
            );
        }

        #[test]
        fn parses_zero_counter_wording() {
            let s = codex_question_screen_snapshot(&live_fixtures::codex_s1_q2_zero()).unwrap();
            assert_eq!(s.unanswered, 0);
            assert!(s.is_last);
            assert_eq!(s.focused, Some(1)); // › 2. Memcached
        }

        #[test]
        fn parses_answered_q1_no_counter() {
            let s = codex_question_screen_snapshot(&live_fixtures::codex_s1_q1_answered()).unwrap();
            assert_eq!(s.unanswered, 0);
            assert!(!s.is_last);
            assert_eq!(s.focused, Some(0));
        }

        #[test]
        fn parses_unanswered_reselect_state() {
            let s =
                codex_question_screen_snapshot(&live_fixtures::codex_s1_q1_unanswered()).unwrap();
            assert_eq!(s.unanswered, 1);
            assert_eq!(s.focused, Some(1));
        }

        #[test]
        fn parses_s2_with_status_bar_variant() {
            // 状态栏行插在选项区和 footer 之间（活体变体）——仍须解析
            let s = codex_question_screen_snapshot(&live_fixtures::codex_s2_q1()).unwrap();
            assert_eq!(s.question_total, 3);
            assert_eq!(s.options.len(), 3);
        }

        #[test]
        fn parses_s3_single_three_segment_footer() {
            let s = codex_question_screen_snapshot(&live_fixtures::codex_s3_single()).unwrap();
            assert_eq!(s.question_total, 1);
            assert!(s.is_last);
            assert_eq!(s.unanswered, 1);
        }

        #[test]
        fn parses_s3_zero() {
            let s = codex_question_screen_snapshot(&live_fixtures::codex_s3_zero()).unwrap();
            assert_eq!(s.unanswered, 0);
        }

        #[test]
        fn parses_long_label_single_line() {
            let s = codex_question_screen_snapshot(&live_fixtures::codex_e_wrap()).unwrap();
            assert_eq!(
                s.options[0],
                "Amazon Elastic Kubernetes Service with multi-region fleet"
            );
        }

        #[test]
        fn answered_summary_is_not_panel() {
            assert!(
                codex_question_screen_snapshot(&live_fixtures::codex_answered_summary()).is_none()
            );
            assert!(
                codex_question_screen_snapshot(&live_fixtures::codex_answered_summary3()).is_none()
            );
        }

        #[test]
        fn notes_open_still_parses_as_panel() {
            // notes 态字符层与面板同形（题号头+选项区+footer 都在）——可解析
            let s = codex_question_screen_snapshot(&live_fixtures::codex_s4_notes_open()).unwrap();
            assert_eq!(s.focused, Some(2));
        }

        #[test]
        fn plain_screen_is_not_panel() {
            // 无题号头无 footer 的普通屏 → None
            let lines: Vec<String> = vec!["› Ask Codex to do anything".to_string(), "".to_string()];
            assert!(codex_question_screen_snapshot(&lines).is_none());
        }

        #[test]
        fn scrolled_back_residual_takes_last_pair() {
            // 构造：旧面板（残留题号头+footer）+ 新面板——取最后一对（新面板）
            let mut lines = live_fixtures::codex_s1_q1();
            lines.push("".to_string());
            lines.extend(live_fixtures::codex_s3_single().iter().cloned());
            let s = codex_question_screen_snapshot(&lines).unwrap();
            assert_eq!(s.question_total, 1); // 落在新面板（S3 单题）
            assert_eq!(s.heading, "Which DB?");
            assert_eq!(s.unanswered, 1);
        }

        #[test]
        fn header_without_footer_pair_is_not_panel() {
            // 题号头在场但下方无 footer 配对 → None（防正文编号列表误报）
            let lines: Vec<String> = vec![
                "  Question 1/2 (2 unanswered)".to_string(),
                "  Which DB?".to_string(),
                "  › 1. Postgres".to_string(),
                "    2. SQLite".to_string(),
            ];
            assert!(codex_question_screen_snapshot(&lines).is_none());
        }

        #[test]
        fn parses_s4_tab_ensure_fixture() {
            // S4 tab ensure 帧（Azure/AWS）：字符层仍是面板定形
            let s = codex_question_screen_snapshot(&live_fixtures::codex_s4_tab_ensure()).unwrap();
            assert_eq!(s.question_total, 1);
            assert_eq!(s.options, vec!["Azure", "AWS", "None of the above"]);
            assert_eq!(s.focused, Some(0));
        }
    }
}
