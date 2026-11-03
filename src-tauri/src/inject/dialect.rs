//! 问答交互方言表（设计 SSOT：docs/engineering/specs/2026-10-05-通用终端交互机制推广-design.md §F1）。
//! 差异=数据：新工具接入=填表+账本加行+探测定案，不新写阶段机。
//! 每条填制值必须能指到探测档案/用户实测（结论不超证据；铁律 8）。
//! claude 六机不进本表（用户裁决 2026-10-05：claude 不动，作为活体参照语义）。

/// 自由作答（own answer/Other/notes）链路的方言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnAnswerDialect {
    pub tool: &'static str,
    /// 入口行标签（小写；屏读定位判据）
    pub label: &'static str,
    /// 定位方式：↓ 逐行走位（高亮可见=false 时退化为盲走+弱校验）/ 数字直选 / 当前高亮（notes）
    pub locate: Locate,
    /// 勾选标记（小写匹配）；无勾选段 → 空表
    pub checked_markers: &'static [&'static str],
    pub unchecked_markers: &'static [&'static str],
    /// 编辑门：进编辑态前要按的键
    pub edit_gate: EditGate,
    /// 进编辑态后光标位置（清空动词的依据）
    pub cursor: EditCursor,
    /// 保存回车是否附带推进（opencode=false 仅保存，推进走 tab）
    pub save_advances: bool,
    /// **编辑态开启信号（footer 词形，小写匹配）**：2.x TUI 进编辑态时 footer
    /// 会从 `enter toggle` 变为这些词——是「编辑态已开启」的字符级判据。
    /// opencode 2.0.22 实测（2026-10-05 自建会话活体取证）：编辑态 footer =
    /// `⇆ tab  enter done  esc close`（`enter done`）；高亮在选项行 = `enter toggle`、
    /// 在 own answer 行 = `enter edit`——**footer 即高亮位的字符级信号**。
    /// **1.x 无此信号**（占位行形态），故空表 = 只认占位行（1.x 兼容）。
    pub edit_open_footer: &'static [&'static str],
    /// **高亮在 own answer 行的信号（footer 词形，小写匹配）**：2.0.22 实测高亮停在
    /// own 行时 footer = `… enter edit …`（同上活体取证）。走位闭环用：每步 ↓ 后
    /// 读屏，见此词形即提前停手（高亮已到位）；空表 = 无信号，维持盲走（1.x）。
    pub edit_highlight_footer: &'static [&'static str],
}

/// own answer 行的定位方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locate {
    /// ↓×(row-1) 走位（opencode；高亮不可见→弱校验）
    ArrowsToRow,
    /// 屏上编号数字直选（kimi Other）
    Digit,
    /// 不定位（codex notes：作用于当前高亮项）
    CurrentHighlight,
}

/// 编辑门：进编辑态前要按的键序列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditGate {
    /// 无门（定位即编辑）
    None,
    /// enter ×1 进编辑（未勾选鲜态）
    EnterOnce,
    /// 已勾选时 enter×2（首键=取消勾选，屏读复核后补键）
    EnterDoubleWhenChecked,
    /// tab 开备注（codex）
    Tab,
}

/// 进编辑态后的光标位置（覆盖写入的清空动词依据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditCursor {
    /// 光标在已存文本末尾（覆盖=backspace×len）
    End,
    /// 光标在行首（覆盖=right×len 推到行尾再回删——claude 形态，登记不实现）
    Start,
}

/// 按 tool 取方言；未接入（探测未定案）→ None（编排拒绝出手）。
pub fn own_answer_dialect(tool: &str) -> Option<OwnAnswerDialect> {
    match tool {
        "opencode" => Some(OwnAnswerDialect {
            tool: "opencode",
            label: "type your own answer",
            locate: Locate::ArrowsToRow,
            checked_markers: &["[v]", "[x]", "[✓]"],
            unchecked_markers: &["[ ]"],
            edit_gate: EditGate::EnterDoubleWhenChecked,
            cursor: EditCursor::End,
            save_advances: false,
            // 2026-10-05 自建会话活体取证（2.0.22）：编辑态 footer=`enter done`；
            // 无占位行（1.x 形态退役）——裸打字守卫改认 footer + 勾选翻转双信号；
            // 高亮在 own 行时 footer=`enter edit`——走位闭环信号
            edit_open_footer: &["enter done"],
            edit_highlight_footer: &["enter edit"],
        }),
        // kimi：多选形态存在但自由作答编排**未接入**（2026-10-05 探测定案
        // research/refs/phase2-消息注入/2026-10-05-kimi多选形态探测.md——K7 重进编辑
        // 与 K3 Enter 高亮落点未定案，链路无法安全闭合）；单选 Other 走既有
        // KimiFreeText（戊探B E-B8）不受影响。待定向探测（K7/K3）后回填。
        // codex：2026-10-05 实机复验定案（2026-10-05-codex弹窗键序复验.md——数字
        // 直选即交/Space 选中/Tab notes/多题推进全证实）。自由作答=Tab notes，语义
        // 登记如下；**执行器保持既有 CodexNotes**（已验证编排不换血，方言表登记语义
        // 供将来收编），故此处返回 None（generic 编排不接 codex）。
        // kimi 之外的第三种勾选行方言对照：codex 无勾选框（多选选中态走已答计数）。
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opencode_dialect_matches_user_verified_semantics() {
        let d = own_answer_dialect("opencode").expect("opencode 已定案");
        assert_eq!(d.label, "type your own answer");
        assert_eq!(d.locate, Locate::ArrowsToRow);
        assert_eq!(d.edit_gate, EditGate::EnterDoubleWhenChecked);
        assert_eq!(d.cursor, EditCursor::End);
        assert!(!d.save_advances, "保存回车不推进（推进走 tab）");
        assert!(d.checked_markers.contains(&"[v]"));
        assert!(d.unchecked_markers.contains(&"[ ]"));
        assert!(
            d.edit_open_footer.contains(&"enter done"),
            "2.0.22 编辑态 footer 词形（2026-10-05 自建会话活体取证）"
        );
    }

    #[test]
    fn unprobed_or_non_generic_tools_get_none() {
        assert!(
            own_answer_dialect("kimi").is_none(),
            "kimi 多选自由作答未定案（探测批 K）——generic 不接"
        );
        assert!(
            own_answer_dialect("codex").is_none(),
            "codex notes 语义已定案但执行器保持 CodexNotes（不换血）；方言表注释登记"
        );
        assert!(
            own_answer_dialect("claude").is_none(),
            "claude 六机不动，不进方言表"
        );
    }
}
