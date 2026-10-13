//! 模式切换内核（批次丙 T6 起；批次丁 T4 扩为「二维结构 + 回读全开 + 两段式注入」）。
//!
//! # 要解决的问题
//!
//! 图1（无法退出 plan mode）+ 问题 9（**切换后不知道切到了哪**）。裁5 原文：
//! 「切换后不知道是什么模式 = 白做」——二维工具（codex/kimi）必须出**模式组 + 权限组**
//! 两组按钮，单轴工具（claude/opencode）出**切换钮 + 当前档回显**；**回读失败显示
//! 「请人工核对」，不假装成功**（红线 4 的同一口径）。
//!
//! # 统一模式枚举（对齐 happy 的 8 值，收敛为 兔维斯 5 值）
//!
//! happy 的 `PermissionMode = 'auto'|'default'|'acceptEdits'|'bypassPermissions'|
//! 'plan'|'read-only'|'safe-yolo'|'yolo'`（调研档案 §3）。兔维斯 收敛为 5 值：把
//! happy 的 auto/safe-yolo/yolo 三档合并为「完全信任」（对 TUI 侧而言都是「别再
//! 问我」），其余一一对应：
//!
//! | 兔维斯 统一档 | happy 对应 | 语义 |
//! |---|---|---|
//! | Plan | plan | 只读规划，不改文件 |
//! | Default | default | 每步询问（默认档） |
//! | AcceptEdits | acceptEdits | 自动接受文件编辑 |
//! | Bypass | bypassPermissions / auto / yolo | 完全信任，不再询问 |
//! | ReadOnly | read-only | 只读（不含规划语义） |
//!
//! # 二维结构（裁5 / §2.6 规格表逐格落地）
//!
//! codex 与 kimi 的「模式」与「权限」是**两个正交轴**（模式 = 计划/默认；权限 =
//! 要不要问你），故各自出两组按钮；claude 与 opencode 只有一个轴，出切换钮 + 回显。
//! 每组档位带**屏显标签**——**标签用工具自己的词**（§2.6「档位（屏显标签）」列），
//! 不是 兔维斯 通用名：kimi 权限组是「总是询问/按需询问/永不询问」，不能显示成 兔维斯 的
//! 「默认/接受编辑/完全信任」（用户看到的是终端上的词，对不上号等于没回显）。
//!
//! ## kimi 权限三档 → 兔维斯 5 值的映射（**判断依据，逐条写清**）
//!
//! kimi 官方三档原文（`kimi.exe` 内嵌 i18n 与 `/permission` 选择器，2.0.2 实测抽取）：
//!
//! | kimi 档 | 官方原文 | 语义 | 映射到 兔维斯 | 依据 |
//! |---|---|---|---|---|
//! | manual | `Always Ask` — `Auto-read only; everything else needs your approval first.` | 只自动读，其余逐一向你确认 | [`MamMode::Default`] | 与 兔维斯「默认：每步询问」同义（happy 的 `default` 同格） |
//! | yolo | `Ask When Needed` — `Routine edits and commands run automatically; risky actions, questions, and plans still ask.` | 常规改动/命令自动跑，高危与提问仍问 | [`MamMode::AcceptEdits`] | 兔维斯 5 值里**唯一**的「常规改动自动放行、高危仍问」档就是 AcceptEdits（happy 把 yolo/safe-yolo 也并进 Bypass，但那是 happy 的收敛口径；本仓 AcceptEdits 的语义「自动接受文件编辑」正是 kimi yolo 描述的前半句，且**不会**把「仍会问你」说成「完全信任」——映射到 Bypass 会说假话） |
//! | auto | `Never Ask` — `Never interrupts you; everything runs and is decided automatically.` | 完全不打断 | [`MamMode::Bypass`] | 与 兔维斯「完全信任：不再询问」逐字同义 |
//!
//! **不加枚举值的理由**：加第 6 值要动 `MamMode`（wire 词表、前端 `MAM_MODE_LABELS`、
//! happy 收敛表、GET/POST 契约）——这是一处**结构性改动**，而 kimi 的三档在 5 值里
//! 有语义等价的落点（见上表）。映射只影响**按钮背后的 wire 词**，**屏显标签仍用
//! kimi 的词**（§2.6），故用户看到的永远是终端上的说法，不存在术语失真。
//!
//! # 回读全开（批次丙台账「开放项 3」的收口）
//!
//! 批次丙 T6 只对 opencode 放开回读（其余三家「盲切 + 人工核对」）。T6 实机探测
//! （`research/refs/phase2-消息注入/2026-09-21-四家模式切换shift-tab实机探测.md`）
//! 证实**四家底栏都有可解析的档位文本**，故 T4 全部放开。**分族解析**（本模块的
//! 判断，理由见 [`parse_mode_from_screen`] 文档）：四家词表互斥性存疑（claude 屏上
//! 出现 `plan` 字样、codex 的项目路径可能含 `plan`），共用一个关键词匹配器必然互相
//! 误判；分族后每家的判据都能贴着**该家真机屏幕原文**写死。
//!
//! **缺席推断**（§2.6 表末「Default 靠缺席推断的口径处理」）：
//! - codex：底栏**只**在 Plan 档写 `Plan mode`，默认档没有任何模式字样 → 认出底栏
//!   且无 `plan mode` 即 Default；
//! - kimi：底栏只在 Plan 档有 `plan` 前缀 → 认出底栏且无前缀即 Default。
//!
//! 认不出底栏（屏读失败 / 形态漂移）→ `None` = **档未知** → 前端「请人工核对」，
//! 绝不把「没读到」说成 Default（红线 4）。
//!
//! # 两段式/三段式注入（§2.6 codex/kimi 权限组；丁T4 收尾按**实机取证**改写）
//!
//! `命令 → 菜单 → 屏读定位 → 闭环导航 →（codex Full Access：二次确认框）→ 成功回执核验`：
//!
//! - **第一段**投递开菜单命令：`/permissions`+tab（codex，2026-10-10 3787fa7d 用户
//!   实测指令：文本注入+150ms 后的动作与残留补发均为 tab）、`/permission`/`/yolo`/
//!   `/auto`+回车（kimi）——kimi 2.1.1 实测：键入文本只出**行内自动补全**，回车①
//!   执行命令后完整菜单才打开并停留（❯ 预选目标档），回车②由闭环导航确认；
//! - **第二段**屏读菜单定位目标档，**每发一个方向键就重新屏读复核**（高亮确实移到了
//!   相邻项）——见 [`navigate_until_highlighted`]；**只有**高亮确实落在目标档行时才发
//!   `enter`。依据 = 用户实机取证档案 §8 的方法论原文：「每按一次 ↓ 或者 ↑ 就重新
//!   屏读、确认高亮确实移到下一项」「**绝对不能默认**这个按键在什么位置……必须读取
//!   `/permissions` 之后实际哪个权限被选择了」；
//! - **第三段**（**仅** codex × Full Access，档案 §3 证实 1/2/3 档无此段）：二次确认框
//!   `Enable full access?` → 定位**唯一**含 `continue` 的肯定项 → 同一套闭环 → 提交；
//! - **成功回执核验**：屏读工具自己的成功回执行（codex **双代词形**：0.160.0
//!   `• Permission selection requested: …` ／ 0.154 旧锚 `Permissions updated to …`；
//!   kimi `Permission mode: …`）——见 [`permission_receipt_verified`]。
//!
//! 第二段与丁T3 对话框守卫的关系是本任务最需要想清楚的冲突——详见
//! [`crate::remote::api::session_mode_switch`] 的文档（那里是菜单路径唯一实现点）。
//!
//! # 边界（计划书 §2.5）
//!
//! 不做各工具像素级复刻；不做自定义主题。未实测的机制一律**不出手**（结论不得
//! 超过证据）。
//!
//! # 脆弱常量
//!
//! 注入节拍（分块/提交延迟）与屏读轮询窗（步长 + 各阶段总窗）集中在
//! [`super::timing`]（宪法 D20 / 计划 §2.9 的**单一事实源**；宪法横切 6），
//! 本模块只放**纯判定与纯构造** + 一条把窗用满的轮询内核
//! （[`poll_mode_readback`]）。
//!
//! # D20：回读必须**动态轮询**（本模块的合规面）
//!
//! 宪法 D20（`docs/MASTER-PLAN.md` §5.(c) 第 8 条）与计划 §2.9：切档后**不得**「输入完
//! 直接操作下一步」，必须轮询屏读至**读到判据本身**、有界、超时如实区分「未及确认」与
//! 「不符」。本模块的落点：
//! - [`poll_mode_readback`]——回读的轮询内核（命中即停；窗尽是**最后一次**判定）；
//! - [`crate::inject::timing`]——步长与总窗（含每条自裁值指向的 `#[ignore]` 实测项）；
//! - **例外**（D20(c)）：切档**前**的那次读取（`before`，用于推算步进组的应到档）与
//!   GET 的当前档都是**瞬时快照**——单次读、不轮询，**不要**给它们加窗。

use crate::inject::dialog::DialogOption;

/// 该工具是否支持**打断式插队**（批次丙 T9 定名；批次戊 E1 扩展为三家）。
///
/// 语义 = busy 运行中「立即发送」需要 **Esc 类控制键**参与（[`jump_sequence`]
/// 非 [`JumpSequence::QueueOnly`]）：
/// - **claude**：Esc 中断 → 队列整队取出开新回合（丁T9 语义链 + 戊探F 2.1.278 复现；
///   用户实测：撤回是功能非异常，防护见 confirm::claude_input_line_has_residue）；
/// - **codex**：打字→Tab 入队→Esc×1 直插（用户人工实测 2026-09-22 两轮终裁；
///   探测回退路径=草稿+Esc+手动 Enter，戊探F ×2）；
/// - **opencode**：Esc 打断+直插（用户人工实测 2026-09-22）。
///
/// **kimi 恒 false**：busy 直接投递=**排队制**（回合结束 50–86ms 自动开新回合，
/// 戊探F wire 对账 + 用户终裁），无需也不应注入 Esc（打断版插队 Esc 留作词典备选，
/// 产品不预填）。Ctrl+S 立即插队为**条件项**——产品引擎通道复验通过才上，未验前
/// 维持排队制（结论落台账）。
///
/// **不要与 [`mode_readback_supported`] 混**（两条不同的轴）：回读是「能不能看见
/// 当前档」，插队是「能不能打断运行中的回合」。
pub fn supports_interrupt(tool: &str) -> bool {
    !matches!(jump_sequence(tool), JumpSequence::QueueOnly)
}

/// 插队键序路由（批次戊 E1：busy 运行中「立即发送」的每家键序；唯一事实源=
/// 键序大词典清淤版 §6 busy 态行，与实机冲突停下上台账——裁20 用户实测优先）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpSequence {
    /// claude：**Esc×1 中断 → 屏读等回合停（含撤回窗口防护）→ 正文+回车**。
    /// 投递前检查输入行残留（疑似被撤回消息）→ 命中中止+回执（不自动清空）。
    EscInterruptThenText,
    /// codex：**打字（草稿，无提交回车）→ Tab 入队 → Esc×1 直插**（用户终裁首选；
    /// 探测回退=草稿+Esc+手动 Enter，戊探F ×2 实测，留 #[ignore] 实机面）。
    /// Esc 落在队列态=打断前台+直插；菜单在场时 Esc 被菜单吸收（戊探D）——菜单
    /// 在场属对话框守卫面，不在本路径处理。
    DraftTabThenEsc,
    /// opencode：**Esc×1 打断 → 正文+回车直插**。无忙态串判据（词典 §4 未记载
    /// opencode busy 屏串）→ 不做等回合停轮询（无判据可轮询，D20 不适用）；
    /// 「草稿 Esc 后去向」为未定面——实现不预填，`#[ignore]` 实机首测定案后回填
    /// 词典再动。**Ctrl+C 一律禁注**（裁19，键黑名单见 engine::forbidden_key_reason）。
    EscThenText,
    /// kimi：**不打断直接投递=排队制**（回合结束自动开新回合，不丢）；回执按
    /// 「已投递未确认」（消息尚未进入回合，不谎报 delivered——裁16 排队回执锁）。
    QueueOnly,
}

/// 工具 → 插队键序路由（小写 tool_id，口径同 [`family_for`]；未知工具保守归
/// [`JumpSequence::QueueOnly`]——无键序证据的工具不打控制键，直接投递由族规格
/// 兜底；路由层（remote::api）本就不对黑盒工具开放注入）。
pub fn jump_sequence(tool: &str) -> JumpSequence {
    match tool {
        "claude" => JumpSequence::EscInterruptThenText,
        "codex" => JumpSequence::DraftTabThenEsc,
        "opencode" => JumpSequence::EscThenText,
        // kimi 与未知工具：直接投递（kimi=排队制；未知=无证据不出手）
        _ => JumpSequence::QueueOnly,
    }
}

/// 兔维斯 统一模式档（5 值，对齐 happy 8 值收敛——见模块文档表）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MamMode {
    /// 只读规划（plan mode）
    Plan,
    /// 默认：每步询问
    Default,
    /// 自动接受文件编辑
    AcceptEdits,
    /// 完全信任：不再询问（happy 的 bypassPermissions/auto/yolo 收敛）
    Bypass,
    /// 只读（不含规划语义）
    ReadOnly,
}

impl MamMode {
    /// wire 字符串 ↔ 档（camelCase 请求体用小写词）
    ///
    /// **退役档永不可解析**（裁7）：codex 的 `untrusted`（0.149.0 起配置即拒启）与
    /// `on-failure`（deprecated）在这里**没有 wire 词**——任何客户端把这两个词发进
    /// POST 都会落到 `None` → 400。这是「UI 与代码不得作为可选档」在**输入面**的锁：
    /// 不是靠前端不渲染，而是后端根本不认（见 `mode_legacy_enum_is_not_selectable`）。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "plan" => Some(Self::Plan),
            "default" => Some(Self::Default),
            "acceptEdits" | "accept_edits" => Some(Self::AcceptEdits),
            "bypass" => Some(Self::Bypass),
            "readOnly" | "read_only" => Some(Self::ReadOnly),
            _ => None,
        }
    }

    /// wire 字符串（响应体与审计用）
    pub fn wire(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Bypass => "bypass",
            Self::ReadOnly => "readOnly",
        }
    }

    /// 中文显示名（**通用**档名——二维组的按钮标签用工具自己的词，见 [`ModeTier::label`]）
    pub fn label(self) -> &'static str {
        match self {
            Self::Plan => "计划",
            Self::Default => "默认",
            Self::AcceptEdits => "接受编辑",
            Self::Bypass => "完全信任",
            Self::ReadOnly => "只读",
        }
    }

    /// 全部档位（无顺序语义的集合；**环序**见 [`cycle_next`]）
    pub const ALL: [MamMode; 5] = [
        MamMode::Plan,
        MamMode::Default,
        MamMode::AcceptEdits,
        MamMode::Bypass,
        MamMode::ReadOnly,
    ];
}

/// 模式组标识（裁5 的二维结构：codex/kimi 两组正交；claude/opencode 单轴一组）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeGroupId {
    /// 模式组（计划 / 默认）——所有家都有
    Mode,
    /// 权限组（要不要问你）——只有 codex/kimi 有（claude 的接受编辑/信任档单轴内）
    Permission,
}

impl ModeGroupId {
    /// wire 词（GET 载荷与 POST 请求体）
    pub fn wire(self) -> &'static str {
        match self {
            Self::Mode => "mode",
            Self::Permission => "permission",
        }
    }

    /// wire 词 → 组（未知词 → None，调用方 400）
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "mode" => Some(Self::Mode),
            "permission" => Some(Self::Permission),
            _ => None,
        }
    }

    /// 组名（前端分组标题）
    pub fn label(self) -> &'static str {
        match self {
            Self::Mode => "模式",
            Self::Permission => "权限",
        }
    }
}

/// 单档：兔维斯 档 + **屏显标签**（工具自身词表）+ 可选性。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeTier {
    /// 兔维斯 统一档（wire 词见 [`MamMode::wire`]）
    pub mode: MamMode,
    /// 屏显标签（§2.6「档位（屏显标签）」列**逐字**；不等于 [`MamMode::label`] 时
    /// 以本字段为准——kimi 权限组即此例）
    pub label: &'static str,
    /// 是否可作为切换目标（false = 前端不渲染可点按钮，POST 也会被拒）
    pub selectable: bool,
    /// 不可选原因（`selectable=false` 时必填——**如实回执**，不静默禁用）
    pub reason: Option<&'static str>,
}

/// 官方已退役/弃用的旧档位（裁7）。
///
/// **刻意不放进 [`ModeGroupSpec::tiers`]**：tiers 是「可作为切换目标」的清单，而
/// legacy 档**不得作为可选档**——放进去就必须给它们一个 [`MamMode`] 才能表达，
/// 而 兔维斯 5 值里并没有对应值（发明一个值正是裁7 禁止的「作为可选档」）。故以独立
/// 清单承载，供 GET 载荷**如实展示**「这两个档官方已退役」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegacyTier {
    /// 屏上/官方文档里的旧档名原文
    pub label: &'static str,
    /// 退役依据（一句话，给用户看）
    pub note: &'static str,
}

/// 组的按钮布局（前端渲染分支；数据驱动——前端不硬编码「哪个工具哪组是 toggle」）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupLayout {
    /// 逐档按钮（每档一个可点钮）
    Tiers,
    /// 单钮 toggle（点击=向终端发一次循环键，目标档由前端按当前档翻转——
    /// codex 模式组的「计划 ⇄ 操作」= shift+tab，2026-09-23 用户实测裁决）
    Toggle,
    /// **单选面板**（2026-09-23 用户方案）：单钮「切换权限」→ 点开后由后端**读回
    /// 终端菜单的选项表**（编号 = 屏上实读值、文本 = 屏上原文）→ 用户点选哪项，
    /// 兔维斯 就敲哪个数字键。**兔维斯 不再猜「该敲哪个数」**——codex 权限组用此布局
    /// （档位编号随 Guardian 配置前移，硬编码「4→1」会错）。
    Picker,
}

impl GroupLayout {
    /// wire 词（GET 载荷 `groups[].layout`）
    pub fn wire(self) -> &'static str {
        match self {
            Self::Tiers => "tiers",
            Self::Toggle => "toggle",
            Self::Picker => "picker",
        }
    }
}

/// 单个组（模式组或权限组）的完整描述（纯静态表，可测）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeGroupSpec {
    /// 组标识
    pub id: ModeGroupId,
    /// 组名（前端分组标题）
    pub label: &'static str,
    /// 是否**步进**组（一步一档，目标档不参与按键构造——claude/opencode 的
    /// shift+tab 循环）。false = 逐档按钮（有直达机制）
    pub step: bool,
    /// 该组是否有屏读回读源（false → 前端只显示「请人工核对」，不假装知道当前档）
    pub readback: bool,
    /// 按钮布局（[`GroupLayout`]）
    pub layout: GroupLayout,
    /// 档位（**顺序即环序**——单轴组用 [`cycle_next`] 按此推进；二维组顺序即
    /// §2.6 表的档位列序）
    pub tiers: &'static [ModeTier],
    /// 已退役旧档（**不可选**，只作如实展示；见 [`LegacyTier`]）
    pub legacy: &'static [LegacyTier],
}

/// 工具的模式栏结构（裁5：二维 / 单轴 / 未实测）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeStructure {
    /// 两组正交（codex / kimi）
    TwoAxis {
        /// 模式组
        mode: ModeGroupSpec,
        /// 权限组
        permission: ModeGroupSpec,
    },
    /// 单轴（claude 四档环序 / opencode 两档）
    SingleAxis {
        /// 唯一轴（组标识恒为 [`ModeGroupId::Mode`]）
        axis: ModeGroupSpec,
    },
    /// 未实测工具（无结构 → 前端不渲染切换入口）
    Unsupported,
}

impl ModeStructure {
    /// 全部组（二维两家；单轴一组；未实测空）
    pub fn groups(&self) -> Vec<ModeGroupSpec> {
        match self {
            Self::TwoAxis { mode, permission } => vec![*mode, *permission],
            Self::SingleAxis { axis } => vec![*axis],
            Self::Unsupported => Vec::new(),
        }
    }

    /// 结构 wire 词（GET 载荷 `structure` 字段；前端据此选渲染分支）
    pub fn wire(&self) -> &'static str {
        match self {
            Self::TwoAxis { .. } => "twoAxis",
            Self::SingleAxis { .. } => "singleAxis",
            Self::Unsupported => "none",
        }
    }

    /// 取指定组（结构里没有该组 → None：POST 的 `group` 校验点）
    pub fn group(&self, id: ModeGroupId) -> Option<ModeGroupSpec> {
        self.groups().into_iter().find(|g| g.id == id)
    }
}

// ===== 档位表（§2.6 逐格；屏显标签 = 该格「档位（屏显标签）」列原文）=====

/// codex 模式组：操作 / 计划——**双向 shift+tab toggle**。
///
/// 2026-09-23 用户实机走查裁决（裁20「用户实测优先」）：shift+tab 在 计划/操作
/// 间双向循环可用，覆盖 §2.6 旧「仅 `/plan` 单向、退出无实测命令」的保守裁决
/// （台账「codex 模式切换改造」节登记）。两档 `selectable`，前端按
/// [`GroupLayout::Toggle`] 渲染单钮（目标档由前端按当前档翻转）。
const CODEX_MODE_TIERS: [ModeTier; 2] = [
    ModeTier {
        mode: MamMode::Default,
        label: "操作",
        selectable: true,
        reason: None,
    },
    ModeTier {
        mode: MamMode::Plan,
        label: "计划",
        selectable: true,
        reason: None,
    },
];

/// codex 权限组：只读 / 默认 / 自动审批 / 完全信任（2026-09-23 起四档）。
///
/// 屏显标签对应实测菜单项（`/permissions` 弹窗 `Update Model Permissions`，实机
/// 档位序 Read Only / Ask for approval / Approve for me / Full Access——用户
/// 2026-09-23 实机走查再次确认四项，且第 3 项按数字键可直达）：
/// 只读=`Read Only`、默认=`Ask for approval`、自动审批=`Approve for me`、
/// 完全信任=`Full Access`。旧「`Approve for me` 不作目标档」的保守裁决
/// （§2.6 三档）由用户实测推翻（裁20：数字 1/2/3 实测分别选中前三项）——
/// 台账「codex 模式切换改造」节登记。
const CODEX_PERMISSION_TIERS: [ModeTier; 4] = [
    ModeTier {
        mode: MamMode::ReadOnly,
        label: "只读",
        selectable: true,
        reason: None,
    },
    ModeTier {
        mode: MamMode::Default,
        label: "默认",
        selectable: true,
        reason: None,
    },
    ModeTier {
        mode: MamMode::AcceptEdits,
        label: "自动审批",
        selectable: true,
        reason: None,
    },
    ModeTier {
        mode: MamMode::Bypass,
        label: "完全信任",
        selectable: true,
        reason: None,
    },
];

/// codex 退役旧档（裁7：官方已退役/deprecated → UI 与代码不得作为可选档）
const CODEX_PERMISSION_LEGACY: [LegacyTier; 2] = [
    LegacyTier {
        label: "untrusted",
        note: "官方 0.149.0 起退役（配置即拒启）——不作为可选档",
    },
    LegacyTier {
        label: "on-failure",
        note: "官方已弃用（deprecated）——不作为可选档",
    },
];

/// kimi 模式组：默认 / 计划（`/plan off|on` 带参直达）
const KIMI_MODE_TIERS: [ModeTier; 2] = [
    ModeTier {
        mode: MamMode::Default,
        label: "默认",
        selectable: true,
        reason: None,
    },
    ModeTier {
        mode: MamMode::Plan,
        label: "计划",
        selectable: true,
        reason: None,
    },
];

/// kimi 权限组：总是询问 / 按需询问 / 永不询问（§2.6 屏显标签；映射依据见模块文档表）
const KIMI_PERMISSION_TIERS: [ModeTier; 3] = [
    ModeTier {
        mode: MamMode::Default,
        label: "总是询问",
        selectable: true,
        reason: None,
    },
    ModeTier {
        mode: MamMode::AcceptEdits,
        label: "按需询问",
        selectable: true,
        reason: None,
    },
    ModeTier {
        mode: MamMode::Bypass,
        label: "永不询问",
        selectable: true,
        reason: None,
    },
];

/// claude 单轴四档——**顺序 = 实测环序**（逐次 shift+tab 屏读：
/// `⏵⏵ accept edits on` → `⏸ plan mode on` → `⏵⏵ auto mode on` → `⏸ manual mode on`，
/// 探测档案 §1.2 + 复盘 `screen-cl1-cycA-st{1..4}.txt` 同序）。注意与
/// [`MamMode::ALL`] 的枚举序**不同**——本数组的顺序有环序语义，不可按 ALL 排列。
const CLAUDE_AXIS_TIERS: [ModeTier; 4] = [
    ModeTier {
        mode: MamMode::AcceptEdits,
        label: "接受编辑",
        selectable: true,
        reason: None,
    },
    ModeTier {
        mode: MamMode::Plan,
        label: "计划",
        selectable: true,
        reason: None,
    },
    ModeTier {
        mode: MamMode::Bypass,
        label: "完全信任",
        selectable: true,
        reason: None,
    },
    ModeTier {
        mode: MamMode::Default,
        label: "默认",
        selectable: true,
        reason: None,
    },
];

/// opencode 单轴两档：默认（原 `Build`，裁6 术语对齐）/ 计划
const OPENCODE_AXIS_TIERS: [ModeTier; 2] = [
    ModeTier {
        mode: MamMode::Default,
        label: "默认",
        selectable: true,
        reason: None,
    },
    ModeTier {
        mode: MamMode::Plan,
        label: "计划",
        selectable: true,
        reason: None,
    },
];

/// 工具 → 模式栏结构（纯函数，可测）。
///
/// **只对四家出手**（§2.6 表的四行）；其余工具 [`ModeStructure::Unsupported`]
/// ——前端不渲染任何切换入口（未验不出手）。
pub fn mode_structure(tool: &str) -> ModeStructure {
    match tool {
        "codex" => ModeStructure::TwoAxis {
            mode: ModeGroupSpec {
                id: ModeGroupId::Mode,
                label: ModeGroupId::Mode.label(),
                step: false,
                // 底栏 `Plan mode (shift+tab to cycle)`；缺席即 Default（§2.6 表末口径）
                readback: true,
                layout: GroupLayout::Toggle,
                tiers: &CODEX_MODE_TIERS,
                legacy: &[],
            },
            permission: ModeGroupSpec {
                id: ModeGroupId::Permission,
                label: ModeGroupId::Permission.label(),
                step: false,
                // 权限档**状态栏零显示**（0.160.0 复核定案）——旧「底栏只有模式文本
                // → 无回读源」在底栏面仍真，但回读源已换**事件行**（最新
                // `• Permission selection requested: <标签>`；滚出 → None → GET 侧
                // 回落「上次切换」记忆，spec §3.3 权限轴三级回落）
                readback: true,
                // **单选面板**（2026-09-23 用户方案）：兔维斯 读回终端菜单的选项表交给
                // 用户点，点哪项敲哪个数字键——不再由后端猜「该敲哪个数」（档位编号
                // 随 Guardian 配置前移，硬编码「4→1」会错）
                layout: GroupLayout::Picker,
                tiers: &CODEX_PERMISSION_TIERS,
                legacy: &CODEX_PERMISSION_LEGACY,
            },
        },
        "kimi" => ModeStructure::TwoAxis {
            mode: ModeGroupSpec {
                id: ModeGroupId::Mode,
                label: ModeGroupId::Mode.label(),
                step: false,
                // 底栏 `plan` 前缀；缺席即 Default（§2.6 表末口径）
                readback: true,
                layout: GroupLayout::Tiers,
                tiers: &KIMI_MODE_TIERS,
                legacy: &[],
            },
            permission: ModeGroupSpec {
                id: ModeGroupId::Permission,
                label: ModeGroupId::Permission.label(),
                step: false,
                // **2.1.1 起有底栏回读源**（2026-10-06 活体实测：非 manual 档底栏
                // 行首段为权限英文标签 `Always Ask`/`Ask When Needed`/`Never Ask`，
                // manual 不渲染标签=缺席推断 Default——夹具
                // kimi-211-bar-{manual,yolo,auto}-plan.txt 三态锁定）。旧实测
                // 「底栏只有模式文本」在 2.0.x 成立、已过期。解析 =
                // [`parse_kimi_permission_footer`]
                readback: true,
                layout: GroupLayout::Tiers,
                tiers: &KIMI_PERMISSION_TIERS,
                legacy: &[],
            },
        },
        "claude" => ModeStructure::SingleAxis {
            axis: ModeGroupSpec {
                id: ModeGroupId::Mode,
                label: ModeGroupId::Mode.label(),
                // shift+tab 一步一档：目标档不参与按键构造（§2.6「切换钮=shift+tab 一步+回读确认」）
                step: true,
                readback: true,
                layout: GroupLayout::Tiers,
                tiers: &CLAUDE_AXIS_TIERS,
                legacy: &[],
            },
        },
        "opencode" => ModeStructure::SingleAxis {
            axis: ModeGroupSpec {
                id: ModeGroupId::Mode,
                label: ModeGroupId::Mode.label(),
                step: true,
                readback: true,
                layout: GroupLayout::Tiers,
                tiers: &OPENCODE_AXIS_TIERS,
                legacy: &[],
            },
        },
        _ => ModeStructure::Unsupported,
    }
}

/// 单轴组的**环序推进**（当前档 → 下一步应到档）。当前档不在环内 / 非单轴组 → None。
///
/// claude 环序 `[AcceptEdits, Plan, Bypass, Default]` 是**实测**（探测档案 §1.2）；
/// **已知未验面**：claude 还有 `bypassPermissions` 档（官方 `--permission-mode` 旗标
/// 列有），某些配置下环序可能多一档——那样本函数的预测会与实测不符，端点侧按
/// `Mismatch` 如实回执「回读与预期不符」而不是假装成功（见 [`verify_mode_switch`]）。
pub fn cycle_next(tool: &str, current: MamMode) -> Option<MamMode> {
    let ModeStructure::SingleAxis { axis } = mode_structure(tool) else {
        return None;
    };
    let idx = axis.tiers.iter().position(|t| t.mode == current)?;
    let next = axis.tiers[(idx + 1) % axis.tiers.len()];
    Some(next.mode)
}

/// 模式切换机制（各工具一族；**顶部兼容字段**——真实机制按组描述在
/// [`ModeGroupSpec::step`]，本函数只服务 GET 载荷的 `switchKind` 旧字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeSwitchKind {
    /// shift+tab 循环（claude / opencode）：一次按键切一档
    ShiftTabCycle,
    /// 斜杠命令注入（codex / kimi）：`/plan`、`/permissions` 等
    SlashCommand,
    /// 无机制（未实测/不支持的工具）→ 端点拒绝，前端只显示当前档（若可屏读）
    Unsupported,
}

/// 会话工具 → 旧 `switchKind` 字段值（纯函数，可测）。
///
/// **T4 起 kimi 归 `SlashCommand`**：§2.6 给 kimi 的模式组是 `/plan on|off`（带参
/// 直达），不再是「盲切一档」；权限组三档都是两段式（命令只负责开菜单并预选：
/// `/permission`、`/yolo`、`/auto`，屏读确认高亮后才回车）。旧值
/// `ShiftTabCycle` 会让前端渲染「循环一步」钮（新结构下已由 `groups[].step` 驱动），
/// 故此处与事实对齐——本字段现在是**旧客户端的降级路径**，不是机制的唯一来源。
/// codex 保持 `SlashCommand`：其权限组仍是 `/permissions` 两段式（模式组 2026-09-23
/// 起虽改 shift+tab，但旧降级路径不做组级精细区分）。
pub fn switch_kind(tool: &str) -> ModeSwitchKind {
    match tool {
        "claude" | "opencode" => ModeSwitchKind::ShiftTabCycle,
        "codex" | "kimi" => ModeSwitchKind::SlashCommand,
        _ => ModeSwitchKind::Unsupported,
    }
}

/// 该工具是否支持**屏读回显当前模式**（T4：四家全开）。
///
/// 批次丙 T6 只放开 opencode（保守）；T6 实机探测已证**四家底栏都有可解析的档位
/// 文本**（opencode `Build`/`Plan`、claude `⏸ plan mode on` 等、codex `Plan mode`、
/// kimi `plan` 前缀）→ T4 按台账收口全开。「有回读源」不等于「一定读得到」：屏读
/// 失败 / 形态漂移仍返回 `None` → 前端「请人工核对」（红线 4）。
pub fn mode_readback_supported(tool: &str) -> bool {
    matches!(tool, "claude" | "opencode" | "codex" | "kimi")
}

/// 切换动作形态（按键 vs 文本 vs 两段式菜单）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeSwitchPlan {
    /// 单键（键域内名称，走 `inject_key_spec`）——一步一档的循环切换
    Key(&'static str),
    /// 单段文本（斜杠命令）——文本注入 + 回车提交
    Text(&'static str),
    /// **两段式**（§2.6 codex/kimi 权限组）：先投递 `open` 命令打开菜单，再屏读
    /// 定位 + 导航确认（第二段的实现点与守卫关系见 `remote::api::session_mode_switch`）
    Menu {
        /// 打开菜单的命令（`/permissions` / `/permission`）
        open: &'static str,
        /// 菜单里的目标档
        target: MamMode,
    },
}

impl ModeSwitchPlan {
    /// 本次注入是否**两段式菜单**（第一段开菜单、第二段屏读定位 + 导航确认）。
    ///
    /// 抽成方法而不是在端点写 `matches!(plan, ..Menu..)`：这个布尔决定**回执文案**
    /// （`remote::api::mode_verify_receipt` 的「屏读推算、未回读确认」限定，T4 复评
    /// I2）——判据若散在调用点，将来加变体（如第三种两段式）时回执会静默漏掉限定，
    /// 而那正是「不假装成功」要覆盖的态。放在内核里，**加变体时编译器会提醒改这里**
    /// （endpoint 用的就是本方法，不是自己的 matches）。
    pub fn is_two_stage(self) -> bool {
        matches!(self, Self::Menu { .. })
    }
}

/// 拒绝切换的原因（**如实回执**，不是静默失败）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchRefusal {
    /// 该工具的模式切换未实测（`Unsupported`）→ 端点 409 `no_mechanism`
    Unsupported,
    /// 该工具+组+档无实测机制，附**具体原因**（端点 409 `no_mechanism` + `reason`：
    /// 例如 codex 退出计划模式无命令、codex 权限组以外的档）
    NoMechanism(&'static str),
}

/// 模式切换的注入序列构造（纯函数，§2.6 逐格）。
///
/// 传 `group` 必须是**该工具结构里存在的组**（调用方先经 [`ModeStructure::group`] 校验）。
pub fn mode_switch_plan(
    tool: &str,
    group: ModeGroupId,
    target: MamMode,
) -> Result<ModeSwitchPlan, SwitchRefusal> {
    match (tool, group, target) {
        // shift+tab 一步（claude/opencode 单轴；目标档不参与按键构造）
        ("claude", ModeGroupId::Mode, _) | ("opencode", ModeGroupId::Mode, _) => {
            Ok(ModeSwitchPlan::Key("shift+tab"))
        }
        // codex 模式组：双向 shift+tab toggle（2026-09-23 用户实测裁决——裁20；
        // 目标档不参与按键构造，落点由屏读回读验证：`plan mode` 短语 / ` · ` 状态栏）
        ("codex", ModeGroupId::Mode, MamMode::Plan)
        | ("codex", ModeGroupId::Mode, MamMode::Default) => Ok(ModeSwitchPlan::Key("shift+tab")),
        // codex 权限组：`/permissions` 两段式（命令→菜单→**数字直达**——2026-09-23
        // 用户实测：菜单开着时按档位数字键直接选中并生效，1/2/3 一次直达、4 弹
        // 二阶段确认框再按 1；方向键闭环退役为 kimi 专用。台账登记）
        ("codex", ModeGroupId::Permission, MamMode::ReadOnly)
        | ("codex", ModeGroupId::Permission, MamMode::Default)
        | ("codex", ModeGroupId::Permission, MamMode::AcceptEdits)
        | ("codex", ModeGroupId::Permission, MamMode::Bypass) => Ok(ModeSwitchPlan::Menu {
            open: "/permissions",
            target,
        }),
        ("codex", ModeGroupId::Permission, _) => Err(SwitchRefusal::NoMechanism(
            "codex 权限菜单无该档（实测档位：只读 / Ask for approval / Approve for me / Full Access）",
        )),
        // kimi 模式组：`/plan on|off` 带参直达（TUI 内嵌实现：subcmd on/off/clear）
        ("kimi", ModeGroupId::Mode, MamMode::Plan) => Ok(ModeSwitchPlan::Text("/plan on")),
        ("kimi", ModeGroupId::Mode, MamMode::Default) => Ok(ModeSwitchPlan::Text("/plan off")),
        // kimi 权限组：三档**全部两段式**（2026-10-06 探针会话 2.1.1 定案）：命令
        // 文本+回车①开菜单（❯ 预选目标档、`← current` 恒标当前档、菜单停留等确认）
        // → 轮询等菜单画出 → **屏读确认高亮（❯/▶ 可 `strip_cursor_marker` 读）落在
        // 目标档才发回车②**（预选直通 = 0 方向键）→ 回执核验（`Permission mode:
        // <档>` 锚）→ verified 写档位记忆。三个命令的开菜单预选：`/permission`
        // 预选当前档、`/yolo` 预选「按需询问」、`/auto` 预选「永不询问」（2.1.1
        // 活体夹具 kimi-211-perm-menu-{yolo,auto}.txt）。历史备注：曾按「回车消费
        // picker」误诊改为纯文本首段（5744e45），2.1.1 探针证实纯文本态只有行内
        // 自动补全、菜单永不出现——已回退（本表即回退后形态）。
        ("kimi", ModeGroupId::Permission, MamMode::Default) => Ok(ModeSwitchPlan::Menu {
            open: "/permission",
            target: MamMode::Default,
        }),
        ("kimi", ModeGroupId::Permission, MamMode::AcceptEdits) => Ok(ModeSwitchPlan::Menu {
            open: "/yolo",
            target: MamMode::AcceptEdits,
        }),
        ("kimi", ModeGroupId::Permission, MamMode::Bypass) => Ok(ModeSwitchPlan::Menu {
            open: "/auto",
            target: MamMode::Bypass,
        }),
        ("kimi", ModeGroupId::Permission, _) => Err(SwitchRefusal::NoMechanism(
            "kimi 权限档无实测机制（实测三档：总是询问 / 按需询问 / 永不询问）",
        )),
        _ => Err(SwitchRefusal::Unsupported),
    }
}

/// **POST 的组推断**（纯函数，可测；旧客户端不带 `group` 字段时的兼容口径）。
///
/// 规则（三条，按序）：
/// 1. 请求带 `group` → 必须存在于该工具结构（否则 [`SwitchRefusal::Unsupported`]
///    ——端点映射 400/409）；
/// 2. 不带 `group`：目标档**只在一个组里** → 取该组；
/// 3. 两组都有（codex/kimi 的 `default` 即此例）→ **取可选的那一组**；两组都可选
///    → 取**模式组**。
///
/// 规则 3 的依据（逐条）：旧客户端（批次丙 T6 前端的调用面）在 codex 上只会发
/// `plan`（模式组）与 `bypass`（权限组）——两者都不歧义；在 kimi 上发的是
/// `default`（那是对 `ShiftTabCycle` 的「切一档」语义，实际效果 = 切plan 档），
/// 因此 `default` 归**模式组**才与旧行为同义。codex 的 `default` 在模式组里
/// **不可选**（无退出命令）而权限组里可选 → 落到权限组（= 旧实现 `/permissions`
/// 开面板的同义升级：现在是完整两段式）。两条都是「旧意图 → 最接近的可行动作」。
pub fn resolve_group(
    tool: &str,
    requested: Option<ModeGroupId>,
    target: MamMode,
) -> Result<ModeGroupId, SwitchRefusal> {
    let structure = mode_structure(tool);
    // 组必须先存在，且**目标档要在该组的 tiers 里**——否则「组对了但档不在组里」
    // 会被下游当作机制缺失，而调用方无法预判（判据只在这一处，不散两处）
    let in_group = |g: &ModeGroupSpec| g.tiers.iter().any(|t| t.mode == target);
    if let Some(id) = requested {
        let spec = structure.group(id).filter(in_group);
        return spec.map(|g| g.id).ok_or(SwitchRefusal::Unsupported);
    }
    let candidates: Vec<ModeGroupSpec> = structure.groups().into_iter().filter(in_group).collect();
    match candidates.as_slice() {
        [] => Err(SwitchRefusal::Unsupported),
        [only] => Ok(only.id),
        _ => {
            // 歧义：优先可取的那一组（档位在组内可选）；两组都可选 → 模式组（顺序即
            // groups() 的序：mode 在前）
            let selectable = candidates
                .iter()
                .find(|g| g.tiers.iter().any(|t| t.mode == target && t.selectable));
            Ok(selectable.map(|g| g.id).unwrap_or(candidates[0].id))
        }
    }
}

/// 模式/权限切换的**投递前拦截决策**（批次戊 E3② 守卫收窄的判据单点——
/// `remote::api::session_mode_switch` 消费，测试在此钉死判定表）。
///
/// # 判定表（裁17 / 守卫设计原则 §6.5：拦「破坏状态」，不拦「无效果」）
///
/// | 条件 | 判定 | 依据 |
/// |---|---|---|
/// | **对话框在场**（编号选项簇，注入前单次屏读快照——D20 例外条款） | [`SwitchBlock::Dialog`] | 既有控制类注入红线（丁T3 §2.7）：切档键被弹窗吞、Enter 误交默认答案 |
/// | **kimi × 问答待决**（消息尾部形态判据） | [`SwitchBlock::QuestionPending`] | **含回车的整条注入硬拒绝**（戊探D ×2：斜杠文本被问答 UI 吞、尾随 Enter 被解释为「选中高亮项」推进 Review=**污染待决态**——不是无效果，是破坏状态） |
/// | codex × 模式组 × Plan × 运行中 | [`SwitchBlock::CodexPlanBusy`] | codex 产品限制（`Plan mode unavailable right now.`），提前拦为给可读回执 |
/// | **其余一切 busy 态** | `None`（**放行**） | **裁17**：busy 中切权限/模式直接生效且不打断运行（codex `/permissions`+Full Access 二段确认照常弹、kimi 同——CX-1/2/K-2 三方实测，「busy 不可用」彻底证伪） |
///
/// codex 的问答待决**不在此拦**：codex 的 `request_user_input` 弹窗本身是**编号选项
/// 对话框**，落在「对话框在场」格（同一屏读快照即可判）；kimi 的问答框无编号簇
/// （屏读判不到），才需要消息尾部的第二判据。前端置灰 + 原因文案消费同一判定表
/// （spec §4 待决拦截硬约束）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchBlock {
    /// 对话框在场（既有红线）
    Dialog,
    /// kimi 问答待决（含回车整条硬拒绝）
    QuestionPending,
    /// codex /plan 运行中（产品限制）
    CodexPlanBusy,
}

/// 判定表本体（参数化全部条件，跨平台可测；`None` = 放行）
pub fn mode_switch_block(
    tool: &str,
    group: ModeGroupId,
    target: MamMode,
    is_running: bool,
    dialog_present: bool,
    question_pending: bool,
) -> Option<SwitchBlock> {
    if dialog_present {
        return Some(SwitchBlock::Dialog);
    }
    if tool == "kimi" && question_pending {
        return Some(SwitchBlock::QuestionPending);
    }
    if codex_plan_busy(tool, group, target, is_running) {
        return Some(SwitchBlock::CodexPlanBusy);
    }
    // busy（is_running）单独出现 → **放行**（裁17：busy 态不拦）
    None
}

/// **codex 模式组运行中不可用**（§2.6 表末「运行中不可用→如实回执」）——纯判据。
/// 判据来源：codex 0.155.1 二进制内嵌文案 `Plan mode unavailable right now.`
/// （`slash_dispatch` 分支，与 `/plan` 的 in-progress 门同源）——即 codex 自己就会
/// 拒；兔维斯 侧**在投递前**判，才能给用户一份**如实回执**而不是「已发送」后无变化。
///
/// 「运行中」的口径复用 [`crate::inject::queue::is_running`]（Processing / Thinking /
/// Compacting 三态——队列层既有单一判据，不另立一份）。2026-09-23 起拦截面扩为
/// **模式组任意档**（shift+tab 取代 /plan 后，运行中 shift+tab 的行为同样未实测——
/// 保守沿用旧拦截面，不盲扩到权限组；扩张决定在台账「codex 模式切换改造」节登记）。
pub fn codex_plan_busy(tool: &str, group: ModeGroupId, target: MamMode, is_running: bool) -> bool {
    let _ = target;
    tool == "codex" && group == ModeGroupId::Mode && is_running
}

/// 屏读文本 → 当前模式（**按工具分族解析**，纯函数可测）。
///
/// # 为什么分族（本模块的判断，理由）
///
/// 批次丙 T6 的实现是**工具无关**的关键词匹配（`accept edits`/`bypass`/`plan`/
/// `build` 谁先命中谁算）。T4 放开三家回读后这条路走不通：
/// - **误判是必然的，不是概率问题**：claude 屏上 `⏸ plan mode on` 与 codex 屏上
///   `Plan mode (shift+tab to cycle)` 都含 `plan`，而它们**不是同一档**（codex 的
///   `Plan mode` 是模式档、claude 的也是模式档，但 codex 底栏的 `Plan mode` 与
///   claude 底栏的 `plan mode on` 判据不同源）；更危险的是**正文**：claude 计划批准
///   框里就有 `No, stay in Plan mode`，工具无关匹配会把「对话框在场」读成「当前是
///   Plan 档」；
/// - **每家的缺席语义不同**（codex/kimi 的 Default 靠**缺席**推断，claude/opencode
///   靠**在场词**）——工具无关的匹配器无法表达「认出这个工具的底栏了，但上面没有
///   档位词」这层结论，只能返回 None，等于白放开；
/// - 分族后每家都能贴着自己的**真机屏幕原文**写死（本模块测试用的就是 T6 档案里的
///   逐字快照），可回归、可追责。
///
/// # 各家族判据（夹具 = `research/refs/phase2-消息注入/2026-09-21-四家模式切换shift+tab实机探测.md` 与
/// `%TEMP%\mam-probe-c3-20260921-150000\evidence\screen-*` 的**逐字原文**）
///
/// 一律**自底向上**扫描（底栏在屏幕最下方，取最靠下的一条命中——正文里引用模式词
/// 的段落总在底栏之上）：
///
/// | 工具 | 判据 | 实录 |
/// |---|---|---|
/// | claude | `accept edits on`→AcceptEdits；`plan mode on`→Plan；`auto mode on`→Bypass；`manual mode on`→Default | `⏵⏵ accept edits on (shift+tab to cycle) · ← for agents` / `⏸ plan mode on …` / `⏵⏵ auto mode on …` / `⏸ manual mode on · ? for shortuts ·←for agents` |
/// | opencode | 独立词 `build`→Default；独立词 `plan`→Plan | `Build   DeepSeek V4.1 Flash Ollama Cloud ·high` / `Plan   …   high` |
/// | kimi | 底栏（最后一条含 `thinking:` 的行）首词 `plan`→Plan；**认出底栏但无前缀→Default** | `plan  GLM-5.3-Flash thinking: high  C:\…\proj-kimi` / 默认态 `GLM-5.3-Flash thinking: high  C:\…` |
/// | codex | 行含独立短语 `plan mode`→Plan；行含 ` · `（状态栏分隔符）且无 `plan mode`→**Default（缺席推断）** | `glm-5.3-flash medium · ~\…\proj-codex  Plan mode (shift+tab to cycle)` / 默认档 `glm-5.3-flash medium · ~\…\proj-codex` |
///
/// # 保守面（如实申报）
///
/// - codex 的 Default 依赖 ` · ` 状态栏形态（实测两态都有）；若某版本改版式 → 返回
///   None（未知）而不是乱猜；
/// - kimi 的底栏识别依赖 `thinking:`（实测两台模型皆有；无思考能力的模型可能不印
///   这段 → 返回 None = 未知，方向保守）；
/// - **claude / codex** 的正文若出现与底栏**逐字相同**的短句且位于更下方，可能误判
///   ——两家的底栏判据是「含某子串」（`plan mode on` / `plan mode` + ` · `），
///   自底向上扫描只能缓解不能消除「正文里也有同样一行」的情形。实机由 `#[ignore]`
///   全量回读用例逐例核对（四个真机快照已在门禁内锁住；对话框遮挡时自然返回 None，
///   见 `claude_dialog_does_not_fake_plan_mode`）。
pub fn parse_mode_from_screen(tool: &str, lines: &[String]) -> Option<MamMode> {
    match tool {
        "claude" => parse_claude_footer(lines),
        "opencode" => parse_opencode_footer(lines),
        "kimi" => parse_kimi_footer(lines),
        // codex：**双源判读**（2026-10-10 用户实测「Plan 档读成 Default」修复）——
        // 全屏自底向上扫描会被**状态栏下方的快捷键行**（`← for agents · ? for
        // shortcuts`，含 ` · ` 分隔符、无模式词）先命中 → 缺席推断恒 Default，
        // Plan 档被读反。改走 [`codex_mode_current_from_screen`]（状态栏行窗 +
        // 事件行兜底，与切换核验同源判据——parse_codex_footer 保留为行窗内部实现）
        "codex" => codex_mode_current_from_screen(lines),
        // 未实测工具：不猜（旧实现会对任意工具跑关键词表——T4 起按族收敛）
        _ => None,
    }
}

/// claude 底栏（自底向上；四档词表 = 实测环序的四条原文）
fn parse_claude_footer(lines: &[String]) -> Option<MamMode> {
    for line in lines.iter().rev() {
        let lower = line.to_lowercase();
        if lower.contains("accept edits on") {
            return Some(MamMode::AcceptEdits);
        }
        if lower.contains("plan mode on") {
            return Some(MamMode::Plan);
        }
        if lower.contains("auto mode on") {
            // happy 的 auto 归 兔维斯 的 Bypass（调研档案 §3 的收敛表）
            return Some(MamMode::Bypass);
        }
        if lower.contains("manual mode on") {
            return Some(MamMode::Default);
        }
    }
    None
}

/// opencode 底栏（自底向上；`Build`=Default 是裁6 的术语来源，`Plan`=Plan）
fn parse_opencode_footer(lines: &[String]) -> Option<MamMode> {
    for line in lines.iter().rev() {
        let lower = line.to_lowercase();
        if contains_word(&lower, "plan") {
            return Some(MamMode::Plan);
        }
        if contains_word(&lower, "build") || contains_word(&lower, "default") {
            return Some(MamMode::Default);
        }
    }
    None
}

/// kimi 底栏行（**最后一条含 `thinking:` 的行**——2.1.1 活体仍稳定携带；认不出锚
/// 返回 None，宁可「未知」不猜）
fn kimi_footer_line(lines: &[String]) -> Option<&String> {
    lines
        .iter()
        .rev()
        .find(|l| l.to_lowercase().contains("thinking:"))
}

/// kimi 底栏 → **模式轴**（分词含独立词 `plan` → Plan，否则 Default 缺席推断）。
///
/// **2026-10-06 重写（2.1.1 实测过期修复）**：旧判据「行首词 == plan」按 2.0.x 底栏
/// 形态（`plan  <模型> thinking: …`，plan 恒首词）写死；2.1.1 起**非 manual 权限档
/// 时底栏以权限英文标签开头**（`Never Ask  plan  DeepSeek … thinking: …`），首词
/// 变成 `Never`/`Ask` → 旧判据恒误读 Default（用户实机「切计划报预期计划实际默认」
/// 的根因）。新判据改为**整行分词找独立词 `plan`**——新旧两种版式通吃：
/// - `plan  DeepSeek…`（manual / 2.0.x）→ Plan；
/// - `Never Ask  plan  DeepSeek…`（2.1.1 × 非 manual × plan ON）→ Plan；
/// - `Ask When Needed  DeepSeek…`（2.1.1 × 非 manual × plan OFF）→ Default；
/// - `DeepSeek…`（manual × plan OFF）→ Default。
fn parse_kimi_footer(lines: &[String]) -> Option<MamMode> {
    let footer = kimi_footer_line(lines)?;
    if footer
        .to_lowercase()
        .split_whitespace()
        .any(|w| w == "plan")
    {
        Some(MamMode::Plan)
    } else {
        // 缺席推断（§2.6 表末）：底栏在、没有独立词 plan → 默认档
        Some(MamMode::Default)
    }
}

/// kimi 底栏 → **权限轴**（2.1.1 实测：非 manual 档时底栏行首段为权限英文标签，
/// manual **不渲染标签** → 缺席推断 Default）。
///
/// 判据 = 底栏行**分词序列的前缀**恰为标签词（Title Case 转小写比对）：
/// - `ask when needed …` → AcceptEdits（三词，必须先于 always/never 判——同为 ask 开头）；
/// - `never ask …` → Bypass；
/// - `always ask …` → Default；
/// - 其余（`plan …` / `<模型>…`）→ Default（manual 缺席推断）。
///
/// 这让 kimi 权限组从「无回读源、verified 只能靠回执自证」升级为**底栏可回读**
/// （`readback: true`）：切档回执与 GET 当前档都有一手屏读证据（2.1.1 活体夹具
/// `kimi-211-bar-{manual,yolo,auto}-plan.txt` 三态锁定）。
fn parse_kimi_permission_footer(lines: &[String]) -> Option<MamMode> {
    let footer = kimi_footer_line(lines)?;
    let lower = footer.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    let starts_with =
        |label: &[&str]| words.len() >= label.len() && words.iter().zip(label).all(|(w, l)| w == l);
    if starts_with(&["ask", "when", "needed"]) {
        Some(MamMode::AcceptEdits)
    } else if starts_with(&["never", "ask"]) {
        Some(MamMode::Bypass)
    } else {
        // always ask（Default）与 manual（无标签）在此汇合——两者同档
        Some(MamMode::Default)
    }
}

/// 屏读文本 → **指定组的当前档**（模式轴 / 权限轴分发的单一入口）。
///
/// - 模式轴：四家各自底栏（[`parse_mode_from_screen`]）；
/// - 权限轴：kimi 走底栏（[`parse_kimi_permission_footer`]，2.1.1 起）；codex 走
///   **最新回执行**（[`codex_permission_current_from_screen`]，2026-10-09——状态栏
///   零显示、事件行滚出即 None → 调用方回落记忆，spec §3.3）；其余工具无两轴结构
///   → None。
pub fn parse_axis_from_screen(tool: &str, group: ModeGroupId, lines: &[String]) -> Option<MamMode> {
    match group {
        ModeGroupId::Mode => parse_mode_from_screen(tool, lines),
        ModeGroupId::Permission => match tool {
            "kimi" => parse_kimi_permission_footer(lines),
            // codex 权限轴回读（2026-10-09）：状态栏零显示——唯一屏读源 = 最新
            // 回执行（`• Permission selection requested: <标签>`）；滚出 → None
            // → 调用方回落记忆（api.rs recall_permission_tier，三级回落 spec §3.3）
            "codex" => codex_permission_current_from_screen(lines),
            _ => None,
        },
    }
}

/// codex 底栏（自底向上；`Plan mode` 短语 → Plan，` · ` 状态栏在场 → Default）
///
/// **与 claude 的区分点（分族的关键）**：claude 的底栏逐字是 `plan mode on`
/// （带 `on`），codex 的是裸 `Plan mode`。两者的 `(shift+tab to cycle)` 后缀相同，
/// 分隔符 ` · ` 也相同——只有这个 `on` 能分开它们。故本族的 Plan 判据是
/// 「含 `plan mode` 且**不含** `plan mode on`」。
fn parse_codex_footer(lines: &[String]) -> Option<MamMode> {
    for line in lines.iter().rev() {
        let lower = line.to_lowercase();
        // claude 的形态（`… mode on`）不是 codex 的判据 → 跳过
        if lower.contains("plan mode on") {
            continue;
        }
        // `Plan mode` 是**独立短语**（词边界：`Planner`/`planning` 不命中）
        if lower.contains("plan mode") {
            return Some(MamMode::Plan);
        }
        // 状态栏形态：`<模型> · <工作目录>`（默认档没有模式字样 → 缺席即 Default）。
        // 用**带空格的中点分隔符**做锚，避免正文里的裸 `·` 误命中
        if line.contains(" · ") {
            return Some(MamMode::Default);
        }
    }
    None
}

/// 行内是否存在**独立词** `word`（前后非 ASCII 字母——`Planner` 不算 `plan`）
fn contains_word(haystack: &str, word: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut from = 0usize;
    while let Some(i) = haystack[from..].find(word) {
        let start = from + i;
        let end = start + word.len();
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphabetic();
        let after_ok = end >= bytes.len() || !bytes[end].is_ascii_alphabetic();
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

/// codex **模式切换事件行**行形判据（2026-10-09 风险预测试批，底料 §1/§5）：
/// `• Model changed to <model> <effort> for <Plan|Default> mode.`
/// ——模型名/effort 是变量不参与匹配；判据锁**前缀 + 尾段**：
/// 前缀 `model changed to`、尾段 `for plan mode.` / `for default mode.`
/// （含句号）。**不进账本**（含变量，行形解析不 contains——spec §6-T2）。
/// 返回该行表意档（Plan/Default）；无变量中段校验（两锚之间非空即收）。
/// **事件行只是「最近一次切换」的记录，不是当前档的权威**——消费优先级见
/// [`codex_mode_current_from_screen`]（状态栏权威、事件行兜底）。
///
/// 判据选型（以底料 §1 逐字为准）：实测行恒带 `• ` 前缀与句号，但取
/// 「contains `model changed to` + 以 `for plan mode.` / `for default mode.`
/// 结尾（trim_end 后）」两锚交集——前缀 contains 已被句号尾段双重锁死误命中面
/// （正文引用该句的概率远低于缺 bullet 的重绘撕裂变体，底料 §5.1/§6 交叠形态）；
/// `starts_with("• model changed to ")` 会把撕裂帧里 bullet 被吞的变体整行漏掉。
/// 两锚取交集是「少漏 + 误命中面已被尾段封死」的平衡点。
pub(crate) fn parse_codex_mode_event_line(line: &str) -> Option<MamMode> {
    let lower = line.to_lowercase();
    if !lower.contains("model changed to") {
        return None;
    }
    let trimmed = lower.trim_end();
    if trimmed.ends_with("for plan mode.") {
        Some(MamMode::Plan)
    } else if trimmed.ends_with("for default mode.") {
        Some(MamMode::Default)
    } else {
        None
    }
}

/// codex 状态栏**行窗**行数（底料 §6 定案「状态栏可跨 1–2 行渲染 + composer 上方
/// 模型行」：mp-st-3 / mp-scroll-after 实测状态栏块恒占底部 3 行——模型行、提示行、
/// 撕裂残段行；事件行恒在 composer 区之上，不进此窗）。
const CODEX_STATUS_BAR_WINDOW_ROWS: usize = 3;

/// codex 模式轴**当前档双源判定**（spec §3.3 优先级表）：
/// 1. **状态栏在场即权威**——按 T4 的**行窗单源**（[`codex_state_bar_mode`]：先
///    剔除事件行、再取底部 [`CODEX_STATUS_BAR_WINDOW_ROWS`] 行交
///    [`parse_codex_footer`]）判读。行窗构造依据（底料 §6）：「状态栏可跨 1–2 行
///    渲染，解析按『行窗』不按『单行』取词」。终审 P2-4 判据一致化：本函数曾用
///    **裸** `parse_codex_footer`（窗不剔事件行）——事件行落进底 3 行窗时其
///    `for <M> mode.` 短语会抢在真状态栏之前被误读成「状态栏在场」（事件行是
///    「切换记录」不是状态栏；与 [`codex_switch_verified`] 的状态栏判据两路不一）；
/// 2. 状态栏行窗不可判（None——形态漂移/被滚出/无锚）→ **自底向上找最新事件行**
///    兜底（[`parse_codex_mode_event_line`]）。
///
/// 返回 None = 两源都不可判（调用方「未知」如实，不猜）。
pub(crate) fn codex_mode_current_from_screen(lines: &[String]) -> Option<MamMode> {
    if let Some(mode) = codex_state_bar_mode(lines) {
        return Some(mode);
    }
    lines
        .iter()
        .rev()
        .find_map(|l| parse_codex_mode_event_line(l))
}

/// codex **权限切换回执行**解析（2026-10-09 风险预测试批，底料 §1/§7）：
/// `• Permission selection requested: <菜单项标签去编号>`——标签 = 菜单项本名
/// 全形（档 2 = `Ask for approval (non-admin sandbox)`）。判据 = 行含账本锚
/// `permission selection requested`（[`crate::inject::anchor_ledger`] PERMISSION_MENU/
/// RECEIPT，0.160.0 词形）→ 冒号后标签 → 与 [`menu_target_label`] 四档
/// **前缀匹配**（回执行标签 starts_with 菜单短形标签——`ask for approval (non-admin
/// sandbox)` starts_with `ask for approval`；反向不行；描述列无交叉词——底料 §7
/// 无误配面实证）。
///
/// 两步判据（锚步 + 冒号步）各自如实：锚不命中 = 不是回执行（菜单行/标题/普通正文
/// → None）；锚命中但冒号后标签匹配不到任何档 = 认不出的档（→ None，**不猜**——
/// 上游新档位/文案变更时宁可「未知」也不能把新档误归旧档）。0.154 旧锚
/// `permissions updated to` 不进本判据（无冒号标签段，锚步即放行）——GET 的记忆
/// 回落与回执核验各自沿用旧通道，与本屏读面互不污染。
///
/// **多行并存取最新**（底料 §4：最新一条 = 当前档）——本函数单行判定，扫屏取最新
/// 在 [`codex_permission_current_from_screen`]。
///
/// # 重绘撕裂容忍（spec §4-附加）
///
/// 撕裂会产出「同一行连续两行」（读取侧瞬态）——本函数是**纯单行判定**，撕裂行
/// 与原行内容相同、解析结果一致；扫屏侧 `find_map` 自底向上取「最新一条有效行」，
/// 重复行不改变结论，**无需去重**（去重反而多一道假阴性面：撕裂行内容可能被交叠
/// 破坏——底料 §9-3「撕裂行不满足行形判据时跳过」，跳过即本函数返回 None，
/// 自然落到上一条有效行，正是想要的容忍形态）。
///
/// 返回 None 的情形：行不是回执行 / 标签匹配不到任何档（不猜）。
pub(crate) fn parse_codex_permission_event_line(line: &str) -> Option<MamMode> {
    // 锚步：行含账本锚才可能是回执行（小写比较，与账本全小写词形一致；P2-1
    // 单点常量 = 账本 RECEIPT 新行 text）
    let lower = line.to_lowercase();
    if !lower.contains(CODEX_PERMISSION_RECEIPT_ANCHOR) {
        return None;
    }
    // 冒号步：取冒号后标签段（`split_once` 取**首个**冒号——标签自身不含冒号，
    // 底料 §7 四档标签均无）
    let (_anchor_part, label_part) = lower.split_once(':')?;
    let label = label_part.trim();
    // 匹配步：与四个菜单短形标签做前缀匹配；**唯一命中**才 Some
    // （四档标签互不为前缀——read only / ask for approval / approve for me /
    // full access 首词即分家，唯一性是表性质；多命中是防御分支）
    let tiers = [
        (
            menu_target_label("codex", MamMode::ReadOnly),
            MamMode::ReadOnly,
        ),
        (
            menu_target_label("codex", MamMode::Default),
            MamMode::Default,
        ),
        (
            menu_target_label("codex", MamMode::AcceptEdits),
            MamMode::AcceptEdits,
        ),
        (menu_target_label("codex", MamMode::Bypass), MamMode::Bypass),
    ];
    let mut hit: Option<MamMode> = None;
    for (label_opt, mode) in tiers {
        let Some(short) = label_opt else {
            continue;
        };
        if label.starts_with(&short.to_lowercase()) {
            if hit.is_some() {
                return None; // 多命中（不可能但防御）：混入正文 → 不猜
            }
            hit = Some(mode);
        }
    }
    hit
}

/// codex 权限轴**当前档屏读判定**（spec §3.3）：权限档状态栏零显示（0.160.0
/// 定案，底料 §4-R2——约 30 行输出即滚出、屏上零权限痕迹）——唯一屏读源 =
/// 最新回执行。自底向上找第一条可解析的回执行（= 最新）。
///
/// None = 屏上无回执行（滚出/未切换过）→ 调用方回落记忆通道（api.rs
/// [`crate::remote::api::recall_permission_tier`]，既有）→ 都无 = 未知如实
/// （「屏读 → 记忆 → 未知」三级回落，spec §3.3 权限轴行）。
pub(crate) fn codex_permission_current_from_screen(lines: &[String]) -> Option<MamMode> {
    lines
        .iter()
        .rev()
        .find_map(|l| parse_codex_permission_event_line(l))
}

// ===== 切换闭环核验：内容集差分（2026-10-09 T4，spec §3.1/§3.2/§4-R5）=====

/// codex 模式/权限切换的**闭环核验**基线（动作前记录）：屏上全部事件行文本集合
/// （模式+权限两类）。动作后核验用「新于基线」= 内容集差分
/// （[`codex_switch_verified`]）——防两类误确认：旧行留存（spec §4-R1d/e）、
/// 视口还原揭示旧行（§4-R5）；系统自动切换（§4-R3b/c——plan 批准也打事件行）
/// 由「只认本动作时间窗内的新行」挡住（基线在此动作前 capture）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModeSwitchBaseline {
    /// 动作前屏上全部事件行文本（模式+权限两类；trim + lowercase 入集——
    /// 「新于基线」按该归一形态比较，撕裂瞬态不参与判据）
    pub event_lines: std::collections::HashSet<String>,
}

/// 记录切换动作前的**内容集基线**（[`ModeSwitchBaseline`]）：
/// `event_lines` = 屏上所有满足 [`parse_codex_mode_event_line`] 或
/// [`parse_codex_permission_event_line`] 的行（trim + lowercase 入集）。
pub(crate) fn capture_mode_switch_baseline(lines: &[String]) -> ModeSwitchBaseline {
    let mut event_lines = std::collections::HashSet::new();
    for l in lines {
        if parse_codex_mode_event_line(l).is_some()
            || parse_codex_permission_event_line(l).is_some()
        {
            event_lines.insert(l.trim().to_lowercase());
        }
    }
    ModeSwitchBaseline { event_lines }
}

/// 该行是否**新于基线**（内容集差分）：trim + lowercase 后不在基线事件行集合中。
///
/// 语义边界（spec §4-R5 如实申报）：同文案重复切换与视口还原**不可区分**——
/// 同文案旧行在基线集合里已登记，还原/重选都不构成新证据；该面判据退化为
/// 状态栏优先（模式轴状态栏可判；权限轴状态栏零显示 → 如实 unknown，不假 verified）。
///
/// 撕裂变体边界（登记不修，终审 P2-6）：中段损坏、前缀/尾段完好的撕裂行，其归一
/// 文本不在基线集 → 可判「新」——低概率假阳性边界（撕裂是读取侧瞬态、同帧通常
/// 伴随内容完好的原行已在基线集；且状态栏判据在模式轴先行兜底），如实登记不修。
///
/// `pub(crate)`：picker 的回执核验（api.rs pick 分支，T4-F4）用同一差分判据——
/// 「新于基线的回执行」才算本次成功的证据（判据单点，勿在调用侧重写）。
pub(crate) fn event_line_is_new(baseline: &ModeSwitchBaseline, line: &str) -> bool {
    !baseline.event_lines.contains(&line.trim().to_lowercase())
}

/// picker 链共用的**最新新回执行**（单点判据，终审 nit 提炼——api.rs 三处
/// 「parse + is_new」重复手拼收口于此）：自底向上找第一条「可解析为权限回执行
/// **且** 新于基线内容集」的行。调用方拿到行文本后再做目标档/标签的二次判定
/// （本函数不绑档——确认框 Yes 路 / 菜单段 Done 路的判定时机不同）。
/// None = 屏上没有新于基线的回执行（未生效/旧行留存/视口还原，R1c/R5）。
pub(crate) fn latest_new_permission_receipt(
    lines: &[String],
    baseline: &ModeSwitchBaseline,
) -> Option<String> {
    lines
        .iter()
        .rev()
        .find(|l| parse_codex_permission_event_line(l).is_some() && event_line_is_new(baseline, l))
        .cloned()
}

/// codex 模式/权限切换的**闭环核验**（单拍判据，纯函数；spec §3.1/§3.2/§4-R5）。
///
/// # 判据（单拍，任一命中即 verified；轮询形态由调用方逐拍驱动，窗尽 = 未生效）
/// - **模式轴**：状态栏翻到目标档（[`codex_mode_current_from_screen`] == expected）
///   ∨ 屏上出现**不在基线行集合**中的 `for <目标> mode.` 事件行；
/// - **权限轴**：屏上出现**不在基线行集合**中的 `• Permission selection requested:
///   <目标档标签>` 行（[`parse_codex_permission_event_line`] == expected）∨
///   旧锚 `permissions updated to <label>` 新于基线（0.154 兼容——label 用
///   [`menu_target_label`] 短形前缀匹配，`ask for approval` 覆盖全形回执行）。
///
/// 返回 true = 本拍有切换生效的新证据；false = 无证据（不区分「未生效」与
/// 「未重绘」——那是轮询窗的事，D20(b) 由调用方的窗尽回执区分）。
pub(crate) fn codex_switch_verified(
    lines: &[String],
    baseline: &ModeSwitchBaseline,
    group: ModeGroupId,
    expected: MamMode,
) -> bool {
    match group {
        ModeGroupId::Mode => {
            // 状态栏权威（spec §3.3「在场即权威」）：状态栏行窗判出目标档 → 直接
            // verified。行窗是底部 3 行的**独立单源**（[`codex_state_bar_mode`]），
            // 不吃 [`codex_mode_current_from_screen`] 的事件行兜底——兜底读出的档
            // 可能正是**旧行还原**的产物，必须走下面的「新于基线」核验，不能借道
            // 状态栏判据绕过差分（R5 的同一道闸）。
            if codex_state_bar_mode(lines) == Some(expected) {
                return true;
            }
            // 事件行路径：`for <目标> mode.` 行且**新于基线**（旧行留存/视口还原
            // 都不算——R1d/e、R5）
            lines.iter().rev().any(|l| {
                parse_codex_mode_event_line(l) == Some(expected) && event_line_is_new(baseline, l)
            })
        }
        ModeGroupId::Permission => {
            // 新锚：目标档回执行（新于基线）
            let new_receipt = lines.iter().rev().any(|l| {
                parse_codex_permission_event_line(l) == Some(expected)
                    && event_line_is_new(baseline, l)
            });
            if new_receipt {
                return true;
            }
            // 旧锚（0.154 兼容）：`permissions updated to <label>`（新于基线）——
            // label 用菜单短形前缀（回执行全形 starts_with 短形，同 §3.3 匹配纪律）
            let Some(label) = menu_target_label("codex", expected) else {
                return false;
            };
            let label_lower = label.to_lowercase();
            lines.iter().rev().any(|l| {
                let lower = l.trim().to_lowercase();
                lower.contains("permissions updated to")
                    && lower.contains(&label_lower)
                    && event_line_is_new(baseline, l)
            })
        }
    }
}

/// codex **状态栏行窗**的当前档（单源：只判状态栏，不吃事件行兜底）——
/// [`codex_mode_current_from_screen`] 与 [`codex_switch_verified`] 共用的拆分视图。
/// None = 状态栏行窗不可判（形态漂移/被滚出）。
///
/// 行窗构造前**剔除事件行**（模式+权限两类）：事件行是「切换记录」，不是状态栏——
/// 事件行尾段的 `for Plan mode.` 含 `plan mode` 短语，若混进行窗会被
/// [`parse_codex_footer`] 的短语判据误读成「状态栏在场」（核验路径的假 verified 面；
/// 小屏/短行集时事件行必然落进底 3 行窗）。真实底栏行不含 `model changed to`
/// 前缀、不解析为事件行，不受剔除影响。
fn codex_state_bar_mode(lines: &[String]) -> Option<MamMode> {
    let body: Vec<String> = lines
        .iter()
        .filter(|l| {
            parse_codex_mode_event_line(l).is_none()
                && parse_codex_permission_event_line(l).is_none()
        })
        .cloned()
        .collect();
    let start = body.len().saturating_sub(CODEX_STATUS_BAR_WINDOW_ROWS);
    let window = body[start..].join("\n");
    parse_codex_footer(&[window])
}

// ===== codex 模式组 Key 路闭环编排（2026-10-09 T4，spec §3.1；api.rs 装配、本处纯逻辑）=====

/// codex 模式组 shift+tab 切换**编排内核**的产物（api.rs 据此合成回执 wire 字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeToggleOutcome {
    /// 闭环核验结论（窗尽无变化 = false）
    pub verified: bool,
    /// 前读判定的当前档（None = 状态栏+事件行都不可判——**不拒绝**，判据缺 ≠ 档不符）
    pub pre_read: Option<MamMode>,
    /// 末拍屏读的当前档（回执 `observed` 字段——前端据此前读/后读不一致时自理卡面）
    pub observed: Option<MamMode>,
    /// 实际投递的键序列（审计与测试断言用；本编排恒为一次 shift+tab）
    pub sent_keys: Vec<String>,
}

/// codex 模式组切换（shift+tab）的**闭环编排内核**（spec §3.1 流程图的代码面；
/// 纯逻辑——屏读/发键/等待全部注入，与 [`run_codex_permission_stages`] 同款理由：
/// 段间控制流是判据的一部分，写在端点 `#[cfg(windows)]` 闭包里就只有实机能覆盖）。
///
/// # 编排（spec §3.1 逐段；2026-10-10 用户指令改版——**零投递闸移除**）
///
/// 1. **前读**（D20(c) 瞬时快照——单次读，不轮询）：`read_pre()` 读一屏——None
///    （读不到屏）→ `Err`（**零投递**；api.rs 把 Err 转成 failed 回执 + failed
///    审计——实现取比 spec §3.1 原稿「零审计」更保守的口径，终审 P2-5 如实对齐）。
///    Some → [`codex_mode_current_from_screen`] 判当前档——**前读只记录终端真值、
///    不再拦截**（2026-10-10 用户指令：点切换 = 无条件发 shift+tab；「卡面 current
///    过期 + 前读==目标零投递」正是实机日志 11:13-15 六次零投递的根因——卡面过期时
///    点按钮也必然动作）。判不出（None）照发——判据缺 ≠ 档不符。
/// 2. **核验预期**：= **终端前读真值的翻转**（前读 Plan → 预期 Default；前读
///    Default → 预期 Plan；前读不可判 → 取 `fallback_expected`——即前端 target
///    兜底）。翻转纯函数见 [`flip_codex_mode`]。
/// 3. **基线**：发键前 [`capture_mode_switch_baseline`]（内容集差分的「前」侧）。
/// 4. **发键**：`send_key("shift+tab")`（生产 2 记录形态零改动——api.rs 的
///    `locate_and_send_key_spec` 装配）；成功后 `settle()`。
/// 5. **核验**：`poll_verify()` 有界轮询（拍数上界 = `verify_rounds`——生产侧由
///    [`crate::inject::timing::poll_rounds`] 把 [`crate::inject::timing::
///    MODE_SWITCH_VERIFY_POLL_TOTAL_MS`] 按 [`crate::inject::timing::POLL_STEP_MS`]
///    换算成拍数注入，与 [`poll_mode_readback`] 的 `rounds` 同构，窗 = 步长 × 拍数，
///    D20(b) 的「有界」就体现在这两项上）——每拍 [`codex_switch_verified`]（Mode 组）：
///    - 命中 → `verified=true`，`observed` = 该拍 [`codex_mode_current_from_screen`]
///      （命中即停，D20(a)）；
///    - `Ok(None)` = 该拍读不到屏 → 停止轮询（保持最后读数，不空转）；
///    - `Err` = 读屏基础设施异常（键已投递，屏读瞬态不否定切换）→ 按「本轮无读数」
///      继续，**不向外传播**；
///    - 窗尽 → `verified=false`，`observed` = 最后一拍读数。
///
/// # 参数
///
/// `fallback_expected` = 前读不可判时的核验预期兜底（调用方传**前端 target**——
/// 卡面按当前档翻转出的落点；仅在前读判不出时参与，前读可判时以终端真值翻转为准）；
/// `verify_rounds` 核验窗拍数上界（`.max(1)` 纵深防御照抄
/// [`poll_mode_readback`]——0 拍 = 不读 = 无产物，不是有界轮询而是放弃；
/// `timing::poll_rounds` 已保证不下发 0）；`read_pre` 前读（单拍，D20(c) 瞬时快照
/// 语义）；`poll_verify` 核验拍（返回 `Some(lines)` = 该拍屏读原文，None = 该拍
/// 读不到屏；`Err` = 屏读基础设施异常——按无读数处理）；`settle` 拍间等待
/// （生产 = [`crate::inject::timing::POLL_STEP_MS`]）；`terminal` 发键与 settle 的
/// 终端缝（[`MenuTerminal`] 三件套——发键经 `terminal.send("shift+tab")`，
/// **生产 2 记录形态由 api.rs 的 send 装配决定**，内核只发键名）。
pub fn run_codex_mode_toggle_stages<Rd, Pv, W, T>(
    fallback_expected: MamMode,
    verify_rounds: u32,
    mut read_pre: Rd,
    mut poll_verify: Pv,
    mut settle: W,
    terminal: &mut T,
) -> Result<ModeToggleOutcome, String>
where
    Rd: FnMut() -> Option<Vec<String>>,
    Pv: FnMut() -> Result<Option<Vec<String>>, String>,
    W: FnMut(),
    T: MenuTerminal,
{
    // ===== 段 1：前读（D20(c) 瞬时快照——单次读，不轮询；只记录不再拦截）=====
    let Some(lines) = read_pre() else {
        return Err(
            "读不到屏——零投递（模式切换需要屏读确认当前档；请人工核对终端后重试）".to_string(),
        );
    };
    let pre_read = codex_mode_current_from_screen(&lines);
    // ===== 段 2：核验预期 = 前读真值翻转；前读不可判 → 前端 target 兜底 =====
    let expected = flip_codex_mode(pre_read).unwrap_or(fallback_expected);
    if pre_read.is_none() {
        log::debug!(
            "codex 模式切换：前读无法判定当前档 → 核验预期取前端 target 兜底（{expected:?}）"
        );
    } else {
        log::debug!(
            "codex 模式切换：前读 {pre_read:?} → 无条件发 shift+tab（核验预期 {expected:?}）"
        );
    }
    // ===== 段 3：基线（内容集差分的「前」侧）=====
    let baseline = capture_mode_switch_baseline(&lines);
    // ===== 段 4：发键（生产 2 记录形态在 api.rs 装配；这里只发键名）=====
    terminal.send("shift+tab")?;
    terminal.settle();
    let sent_keys = vec!["shift+tab".to_string()];
    // ===== 段 5：核验（基线差分轮询——变化即验，命中即停；拍数上界防挂死）=====
    let mut verified = false;
    let mut observed: Option<MamMode> = pre_read; // 一拍都没读到时以前读为最后读数
    for round in 0..verify_rounds.max(1) {
        match poll_verify() {
            Ok(Some(lines)) => {
                observed = codex_mode_current_from_screen(&lines);
                if codex_switch_verified(&lines, &baseline, ModeGroupId::Mode, expected) {
                    verified = true;
                    log::debug!(
                        "codex 模式切换：核验第 {}/{r} 拍命中预期档 {expected:?}（变化即验，命中即停）",
                        round + 1,
                        r = verify_rounds.max(1)
                    );
                    break;
                }
            }
            // 该拍读不到屏 → 停止（保持最后读数，不空转）
            Ok(None) => break,
            // 屏读基础设施异常：键已投递，屏读瞬态不否定切换 → 按无读数继续
            Err(e) => log::debug!(
                "codex 模式切换：核验第 {} 拍读屏异常（按无读数继续）：{e}",
                round + 1
            ),
        }
        // 末拍不再等（等下去也没有下一拍可读）
        if round + 1 < verify_rounds.max(1) {
            settle();
        }
    }
    if !verified {
        log::debug!("codex 模式切换：核验窗尽 → verified=false（末拍读数 {observed:?}）——如实回执");
    }
    Ok(ModeToggleOutcome {
        verified,
        pre_read,
        observed,
        sent_keys,
    })
}

/// codex 模式组 toggle 的**翻转**（纯函数；2026-10-10 用户指令「目标 = 前读翻转」
/// 的单点）：前读 Plan → 预期 Default；前读 Default → 预期 Plan；前读不可判（或
/// 不是模式组两档之一——防御）→ None，调用方取 `fallback_expected` 兜底。
fn flip_codex_mode(m: Option<MamMode>) -> Option<MamMode> {
    match m {
        Some(MamMode::Plan) => Some(MamMode::Default),
        Some(MamMode::Default) => Some(MamMode::Plan),
        _ => None,
    }
}

// ===== 回读确认（裁5「回读确认」/ 红线 4「不假装成功」）=====

/// 一次切档的**回读结论**（端点回执据此下发 `verified`/`hint`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeVerify {
    /// 回读命中目标档（`verified=true`）
    Confirmed,
    /// 回读成功但**不是**目标档——如实报出两边（不假装成功）
    Mismatch {
        /// 按机制推算的应到档
        expected: MamMode,
        /// 屏读到的实际档
        observed: MamMode,
    },
    /// 无法判定（无回读源 / 屏读失败 / 环序未知）——前端「请人工核对终端」
    Unverifiable,
}

/// 切完一轮后**应到的档**（纯函数）。
///
/// - 步进组（claude/opencode）：环序推算 [`cycle_next`]——**当前档未知时返回 None**
///   （不能凭空预测，那样只会造出假 Mismatch）；
/// - 直达组：目标档本身，但**仅当该组有回读源**（如 codex 权限组无底栏回读 ⇒ None，
///   端点回执如实说「该组无回读源，请人工核对」）。
pub fn expected_mode_after(
    tool: &str,
    group: ModeGroupId,
    target: MamMode,
    current: Option<MamMode>,
) -> Option<MamMode> {
    let spec = mode_structure(tool).group(group)?;
    if spec.step {
        return current.and_then(|c| cycle_next(tool, c));
    }
    if spec.readback {
        Some(target)
    } else {
        None
    }
}

/// 回读确认（纯函数）：`expected` 与屏读结果对账。
pub fn verify_mode_switch(expected: Option<MamMode>, observed: Option<MamMode>) -> ModeVerify {
    match (expected, observed) {
        (Some(e), Some(o)) if e == o => ModeVerify::Confirmed,
        (Some(e), Some(o)) => ModeVerify::Mismatch {
            expected: e,
            observed: o,
        },
        _ => ModeVerify::Unverifiable,
    }
}

/// **模式回读的轮询产物**（D20(a)(b) 的载体；端点回执据此下发 `verified`/`hint`/`current`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeReadbackOutcome {
    /// 窗尽（或提前命中）时**最后一次**判定——`Confirmed` 时是提前停止的那一拍
    pub verdict: ModeVerify,
    /// **最后一次屏读**解析出的档（回执 `current` 字段用；`None` = 那一拍读不到/认不出）
    ///
    /// 口径说明：刻意**只记最后一拍**——早拍的读数是更旧的一屏，把它当「当前档」
    /// 会谎报（红线 4 的同一道理：宁可 null，不给过期值）。
    pub observed: Option<MamMode>,
    /// 实际屏读次数（窗内拍数）
    pub reads: u32,
}

/// **模式回读的动态轮询**（宪法 D20(a)(b) / 计划 §2.9；用户实机观察①的修复点）。
///
/// # 为什么要轮询（旧实现错在哪）
///
/// 旧实现是「固定睡 150ms → **单次**读屏」（`remote::api` 的 `read_mode_from_screen_by_pid`，D20 起已删）：屏幕重绘未及
/// 就读，读到的还是**旧档** → `Mismatch` → 回执报「回读与预期不符」，但终端**其实已切**
/// （用户目视确认、刷新浏览器即一致）。那正是 **D20(a) 点名禁止的「用固定睡眠替代轮询」**。
///
/// # 判据（逐条，与 [`verify_mode_switch`] 的三态一一对应）
///
/// 每拍：屏读 → 解析当前档 → `verify_mode_switch(expected, observed)`：
/// - `Confirmed` → **立即停止**（D20(a)「命中即刻停止」，不浪费固定延迟）；
/// - `Mismatch` → **继续轮询**（屏上还是旧档**可能只是没重绘完**——观察①的形态）；
/// - `Unverifiable`（那一拍读不到屏/认不出形态）→ **继续轮询**至窗尽（同理）；
/// - **窗尽**用**最后一次**判定定结论：`Confirmed`/`Mismatch`/`Unverifiable` 如实下发
///   （D20(b)：超时**不得**统一成「失败」——「不符」与「未及确认」是两句不同的话，
///   见 `remote::api::mode_verify_receipt`）。
///
/// # 两个刻意的短路径（都不是「省掉轮询」，是不做无判据的空转）
///
/// 1. **`settle` 只在两拍之间调**：命中当拍不再等待（命中即刻停止）。
/// 2. **`expected == None` → 只读一拍**（该组无回读源 / 步进组当前档未知 → 应到档无从
///    推算）：`verify_mode_switch(None, 任意)` 恒为 `Unverifiable`，**判据不存在**——
///    转满窗也读不到判据本身。此时把窗睡满不是 D20 要的轮询，而是 D20(a) 禁止的固定
///    睡眠。故读**一拍**（供回执的 `current` 用，与注入前的快照同口径）即返回。
///    **判据只在这一处**：调用方把窗原样传进来即可，不必自己判「要不要轮询」。
///
/// # 终端 IO 走闭包（测试用脚本化屏序列驱动，不碰真 conhost）
///
/// `read` 返回一屏行集（`None` = 读不到屏）；`settle` 给 TUI 重绘留时间（生产 =
/// [`crate::inject::timing::POLL_STEP_MS`] 睡眠，测试 = 空操作/推进脚本）。**测试驱动
/// 方式见 `tests` 模块的 `run_readback_script`**。
///
/// # 参数
///
/// `rounds` = 窗内拍数（生产由 [`crate::inject::timing::poll_rounds`] 把总窗换算成
/// 步长 × 拍数——**窗 = 步长 × 轮数**，D20(b) 的「有界」就体现在这两项上）。
pub fn poll_mode_readback<Rd, Sl>(
    tool: &str,
    group: ModeGroupId,
    expected: Option<MamMode>,
    rounds: u32,
    mut read: Rd,
    mut settle: Sl,
) -> ModeReadbackOutcome
where
    Rd: FnMut() -> Option<Vec<String>>,
    Sl: FnMut(),
{
    // 见文档「两个刻意的短路径」②：无判据可读 → 只读一拍即返回
    let effective = if expected.is_some() { rounds.max(1) } else { 1 };
    let mut last = ModeReadbackOutcome {
        verdict: ModeVerify::Unverifiable,
        observed: None,
        reads: 0,
    };
    for i in 0..effective {
        let observed = read().and_then(|lines| parse_axis_from_screen(tool, group, &lines));
        let verdict = verify_mode_switch(expected, observed);
        last = ModeReadbackOutcome {
            verdict,
            observed,
            reads: i + 1,
        };
        if verdict == ModeVerify::Confirmed {
            log::debug!(
                "模式回读：第 {}/{effective} 拍命中目标档（命中即刻停止）",
                i + 1
            );
            break;
        }
        // 末拍不再等（等下去也没有下一拍可读）
        if i + 1 < effective {
            settle();
        }
    }
    if last.verdict != ModeVerify::Confirmed {
        log::debug!(
            "模式回读：窗尽（{effective} 拍）最后判定 = {:?}（reads={}）——如实回执",
            last.verdict,
            last.reads
        );
    }
    last
}

/// **有界轮询内核**（通用形态；2026-10-10 用户指令：picker Done 的回执核验从
/// 单拍读屏升级为与模式 toggle 同款有界轮询——本函数是两条链共用的轮询单点）。
///
/// 每拍：`read()` 读屏 → `judge()` 对该拍行集判定 → `Some(产物)` 即收
/// （命中即停，D20(a)）；`read()` 返回 None（该拍读不到屏）→ 停止轮询（保持最后
/// 读数，不空转——与 [`run_codex_mode_toggle_stages`] 核验段的 `Ok(None)` 同口径）；
/// 拍数上界 = `rounds`（`.max(1)` 纵深防御：0 拍 = 不读 = 放弃；生产由
/// [`crate::inject::timing::poll_rounds`] 把总窗按 [`crate::inject::timing::
/// POLL_STEP_MS`] 换算成拍数注入，`settle` = 拍间等待）。
///
/// 返回 `(首个命中产物, 最后一次读到的屏行集)`——后者供回执的 `screen_tail`
/// 消费（窗尽/命中两态都带；一拍都没读到 = 空集，调用方如实降级）。
pub(crate) fn poll_screen_until<Rd, W, T, J>(
    rounds: u32,
    mut read: Rd,
    mut settle: W,
    mut judge: J,
) -> (Option<T>, Vec<String>)
where
    Rd: FnMut() -> Option<Vec<String>>,
    W: FnMut(),
    J: FnMut(&[String]) -> Option<T>,
{
    let effective = rounds.max(1);
    let mut last_lines: Vec<String> = Vec::new();
    for i in 0..effective {
        match read() {
            Some(lines) => {
                last_lines = lines;
                if let Some(hit) = judge(&last_lines) {
                    return (Some(hit), last_lines);
                }
            }
            // 该拍读不到屏 → 停止（保持最后读数，不空转）
            None => break,
        }
        // 末拍不再等（等下去也没有下一拍可读）
        if i + 1 < effective {
            settle();
        }
    }
    (None, last_lines)
}

// ===== 权限菜单路径：定位、闭环导航、第三段与成功回执（§2.6 codex/kimi 权限组）=====

/// 权限菜单的**已知档位标签**（工具官方词，`/permissions` / `/permission` 弹窗原文）。
///
/// codex 序按**用户实机取证**（`2026-09-22-codex-kimi权限菜单与FullAccess三段式-用户实机
/// 取证.md` §1/§4 的逐字屏幕原文：`1. Read Only` / `2. Ask for approval (current)` /
/// `3. Approve for me` / `4. Full Access`）；kimi 按同档 §5 原文（`Always Ask` /
/// `Ask When Needed` / `Never Ask`）。
///
/// **本表是「哪些词算这一档」，不是「菜单长什么样」**：菜单行的匹配是**关键词包含**
/// （见 [`locate_menu_items`]），行首形态、编号、缩进、光标标记都不参与词表判据——
/// 两家实机形态不同（codex 编号 + `›`、kimi 无编号 + `❯`），但**同一个词表**即可覆盖。
///
/// **本表（E3① 起）只回答「哪些词算这一档」**——定位已改「标题/footer 锚窗+按家
/// 行形」（[`locate_menu_items`]），导航从**屏上实际高亮位**出发逐步复核
/// （[`navigate_until_highlighted`]），步进数与本表顺序无关。弹窗里缺失的项
/// （如 Guardian 关闭时的 `Approve for me`）因此天然不参与步进；一致性闸
/// （[`menu_items_coherent`]）的目标档全集仍按 [`menu_target_labels`]。
pub fn menu_labels(tool: &str) -> &'static [&'static str] {
    match tool {
        "codex" => &[
            "Read Only",
            "Ask for approval",
            "Approve for me",
            "Full Access",
        ],
        "kimi" => &["Always Ask", "Ask When Needed", "Never Ask"],
        _ => &[],
    }
}

/// 菜单里**该目标档**的标签（找不到 → None：该菜单里没有这一档，不出手）
pub fn menu_target_label(tool: &str, target: MamMode) -> Option<&'static str> {
    match (tool, target) {
        ("codex", MamMode::ReadOnly) => Some("Read Only"),
        ("codex", MamMode::Default) => Some("Ask for approval"),
        // 2026-09-23 起第 3 档（实机 `Approve for me`，数字键 3 直达）
        ("codex", MamMode::AcceptEdits) => Some("Approve for me"),
        ("codex", MamMode::Bypass) => Some("Full Access"),
        ("kimi", MamMode::Default) => Some("Always Ask"),
        ("kimi", MamMode::AcceptEdits) => Some("Ask When Needed"),
        ("kimi", MamMode::Bypass) => Some("Never Ask"),
        _ => None,
    }
}

/// kimi 档位行的**当前档后缀**（`← current`；与高亮 `❯` 双标记并存——
/// `← current` 不随光标移动，是回读当前档的锚，用户 K-1 实测）
pub(crate) const KIMI_CURRENT_SUFFIX: &str = "current";

/// **菜单定位**（纯函数，菜单路径的第一步）：屏读行集 → 编号化的菜单项表。
///
/// # 判据：**标题/footer 锚窗 + 按家行形解析**（批次戊 E3① 重写；**关键词簇判据退役**）
///
/// 旧判据「关键词包含 + 计数互斥」对**整屏**逐行扫档位词，N6 实证其失败形态：kimi
/// 描述行 `Never interrupts you; …` 含 `Never` 子串 → `Never Ask` 被计第 2 次 →
/// 一致性闸判「档位表不自洽」→ 闭环中止（「总是询问」永不可达——失败全在 兔维斯 解析，
/// TUI 导航无问题，用户 K-1 实测 ↓ 恰移一档）。戊探D 五锚实证（标题行 / footer 行 /
/// `← current` / `❯` / 两行组）全部字符层可解析且唯一 → 本函数改为：
///
/// 1. **锚窗**：先找**标题锚**（codex `update model permissions` / kimi
///    `select permission mode`），再找各自的 **footer 锚**；菜单行只在窗内取——
///    标题之上/窗外的正文（busy 混屏流式行、历史摘要的编号行）天然不可达
///    （戊探D：busy 开菜单时菜单块自身行序未被滚动正文污染）；
/// 2. **按家行形**：
///    - **codex**（项在标题↔footer 之间）：行 = `›? N. <档名标签>(current)? <描述>`，
///      经 [`crate::inject::dialog::parse_option_line`] 取编号行——**折行不产新项**
///      （折行是纯缩进描述续行，无 `N. ` 形态，天然不匹配；戊探D 间距 +1/+2 混合）；
///    - **kimi**（项在 footer 之后，两行组）：标签行 = 剥掉光标标记后**恰为**
///      `<档名> ← current` 或裸 `<档名>`（其余字符一律不收）——描述行（5 空格缩进、
///      归属上方标签行）**永不命中**：`Never interrupts you; …` 剥掉 `Never` 后还有
///      `interrupts…`，不满足「恰为」判据 → 正确归属为描述行（**N6 失败行的归宿**）。
/// 3. 高亮 = 行首光标标记（[`crate::inject::dialog::strip_cursor_marker`] 单点；
///    `← current` 是后缀标记、不影响高亮判定——双标记语义见常量注）。
///
/// `number` 按**屏上出现顺序** 1 起递增（内部序号，与屏上是否印数字无关）；
/// `label` 存 `规范标签|屏上原文`（前半供一致性闸与导航比对，后半供回执/日志）。
///
/// 返回 `None`（调用方据此**中止并如实回执**，绝不盲发方向键）：无标题锚（菜单未
/// 出现）/ 窗内合法行 < 2。本函数只做**原始定位**，输出**不可直接行动**（仍须过
/// [`menu_items_coherent`] 两不变式——锚窗已消灭 N6 的混入源，但「同档标签第二行」
/// 的纵深防御不变式保持）。**busy 态照常解析**（裁17：菜单在 busy 中照常打开，
/// 戊探D ×2 实测——守卫不拦 busy）。
pub fn locate_menu_items(lines: &[String], labels: &[&str]) -> Option<Vec<DialogOption>> {
    let lowered: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();
    let canon: Vec<(String, &str)> = labels.iter().map(|l| (l.to_lowercase(), *l)).collect();
    // 标题锚分家：codex 优先（其标题词与 kimi 不相交），再 kimi；都无 → 菜单未出现。
    // codex/kimi 的锚都经**账本**查（真源单点；见 [`crate::inject::anchor_ledger`]——
    // kimi 标题锚 `select permission mode` / footer 锚 `enter select · esc cancel` 同
    // 在账本 kimi 行；两 footer 前缀相同靠后缀分家，互不污染由账本测试
    // `codex_and_kimi_footers_do_not_cross_match` 锁定）
    if crate::inject::anchor_ledger::detect(
        &lowered,
        "codex",
        crate::inject::anchor_ledger::scenario::PERMISSION_MENU,
        crate::inject::anchor_ledger::slot::TITLE,
    )
    .is_some()
    {
        return locate_codex_menu(lines, &lowered, &canon);
    }
    let t = crate::inject::anchor_ledger::detect(
        &lowered,
        "kimi",
        crate::inject::anchor_ledger::scenario::PERMISSION_MENU,
        crate::inject::anchor_ledger::slot::TITLE,
    )?
    .line_index;
    locate_kimi_menu(lines, &lowered, t, &canon)
}

/// codex 菜单**锚窗**（标题锚行 → footer 锚行的行区间；两锚都在才成窗）。
/// [`locate_codex_menu`] 与 [`codex_permission_digit_probe`] 共用（同一判据单一实现）。
///
/// **两锚均经账本查**（[`crate::inject::anchor_ledger`]）：footer 自 0.156.1 起有
/// 两个变体（`press enter to confirm or esc to go back` / `enter select · esc back`），
/// 账本「认任意一句」——写死单句正是 2026-09-23 实机事故的根因。
fn codex_menu_window(lowered: &[String]) -> Option<(usize, usize)> {
    let title_idx = crate::inject::anchor_ledger::detect(
        lowered,
        "codex",
        crate::inject::anchor_ledger::scenario::PERMISSION_MENU,
        crate::inject::anchor_ledger::slot::TITLE,
    )?
    .line_index;
    let footer_idx = crate::inject::anchor_ledger::detect(
        &lowered[title_idx..],
        "codex",
        crate::inject::anchor_ledger::scenario::PERMISSION_MENU,
        crate::inject::anchor_ledger::slot::FOOTER,
    )?
    .line_index
        + title_idx;
    Some((title_idx, footer_idx))
}

/// codex 菜单定位：标题↔footer 窗内的**编号行**（折行不产新项）。
fn locate_codex_menu(
    lines: &[String],
    lowered: &[String],
    canon: &[(String, &str)],
) -> Option<Vec<DialogOption>> {
    let (title_idx, footer_idx) = codex_menu_window(lowered)?;
    let mut items: Vec<DialogOption> = Vec::new();
    for line in &lines[title_idx + 1..footer_idx] {
        // 编号行形态（`› 2. Ask for approval (current)  …`）；折行/空行/标题行自然落选
        let Some((_, raw_label, highlighted)) = crate::inject::dialog::parse_option_line(line)
        else {
            continue;
        };
        let raw_lower = raw_label.to_lowercase();
        if let Some((_, orig)) = canon.iter().find(|(lo, _)| raw_lower.starts_with(lo)) {
            items.push(DialogOption {
                number: items.len() as u32 + 1,
                label: format!("{orig}|{}", raw_label.trim()),
                highlighted,
            });
        }
    }
    if items.len() < 2 {
        return None;
    }
    Some(items)
}

/// codex 权限菜单的**选项清单**（单一实现）：窗内编号行 → 结构化选项表，**编号取
/// 屏上实读值**（不是自编序号）。
///
/// 这是 picker（移动端「终端菜单单选题」面板）与既有数字直达
/// （[`codex_permission_digit_probe`]）**共用**的一步——两者都从屏上读出「屏上编号 +
/// 屏上原文」，差别只在消费者：前者把整表交给用户点，后者按目标档挑一行取编号。
///
/// # 为什么编号必须取屏上实读值（而不是像 [`locate_codex_menu`] 那样按出现顺序自编）
///
/// Guardian 关闭时 `Approve for me` 会**缺席**、其后档位编号前移（实机已见
/// `(non-admin sandbox)` 这类形态变体）——用户看到的编号是屏上印的那个，兔维斯 敲的
/// 必须**同一个数**。自编序号只在「菜单整齐从 1 连续」时与屏上一致，一旦缺档就错位。
///
/// # 过滤（两道，都必要）
///
/// 1. **锚窗**：只在标题↔footer 之间取行——窗外的正文（busy 混屏流式行、历史摘要
///    编号行）天然不可达；
/// 2. **词表**：编号行文本必须**前缀命中** [`menu_labels`]（codex 四档）——滤掉
///    窗内可能混入的其它编号行（如说明段里的列表）。**前缀匹配**使 `(current)`、
///    `(non-admin sandbox)` 这类内联后缀天然不影响识别。
///
/// `label` 存**屏上原文**（picker 要展示给用户，且用户据此核对终端）——不裁剪、
/// 不改写。`None` = 锚窗不成立（菜单未出现/文案未入账）。
pub(crate) fn codex_menu_options(
    lines: &[String],
    lowered: &[String],
) -> Option<Vec<DialogOption>> {
    let (title_idx, footer_idx) = codex_menu_window(lowered)?;
    let canon: Vec<String> = menu_labels("codex")
        .iter()
        .map(|l| l.to_lowercase())
        .collect();
    let mut items: Vec<DialogOption> = Vec::new();
    for line in &lines[title_idx + 1..footer_idx] {
        let Some((number, raw_label, highlighted)) = crate::inject::dialog::parse_option_line(line)
        else {
            continue;
        };
        let raw_lower = raw_label.to_lowercase();
        if !canon.iter().any(|c| raw_lower.starts_with(c.as_str())) {
            continue; // 非档位行的编号行（正文/说明段列表）不入表
        }
        items.push(DialogOption {
            number, // 屏上实读编号
            // **只取档名部分**（剥掉右侧描述列）：codex 的菜单项是 `2. Ask for
            // approval (current)  <描述>` 单行形态（描述在同行右侧，实机 dump 已见），
            // 全行文本给用户看会被描述淹掉。截断点 = 词表匹配项之后的第一个
            // 「两空格以上」间隙（TUI 用它分栏；档名本身不含双空格）。
            label: menu_label_only(&raw_label, &canon),
            highlighted,
        });
    }
    if items.is_empty() {
        return None;
    }
    Some(items)
}

/// 菜单项行 → **只取档名部分**（剥掉右侧描述列）。
///
/// codex 的菜单项是 `2. Ask for approval (current)  <右侧描述>` 单行形态（2026-09-23
/// 实机 dump 确认：描述在同一行右侧，`(current)`/`(non-admin sandbox)` 是档名的内联
/// 后缀）。把整行给用户看会被描述淹没——picker 面板要的是**档名**。
///
/// 截断规则：词表命中项之后，遇到**两个及以上空格**即截断（TUI 用双空格分栏；档名
/// 本体不含双空格——`(current)`、`(non-admin sandbox)` 都是单空格分隔）。找不到分栏
/// 间隙时返回原文（宁可多给不可少给——用户核对时看到完整行也不算错）。
fn menu_label_only(raw_label: &str, canon: &[String]) -> String {
    let lower = raw_label.to_lowercase();
    let Some(hit) = canon.iter().find(|c| lower.starts_with(c.as_str())) else {
        return raw_label.trim().to_string();
    };
    let after = &raw_label[hit.len().min(raw_label.len())..];
    match after.find("  ") {
        Some(pos) => raw_label[..hit.len() + pos].trim().to_string(),
        None => raw_label.trim().to_string(),
    }
}
///
/// 判据 = 标题/footer 锚窗（[`codex_menu_window`]，与 [`locate_codex_menu`] 同窗）
/// 词表认档，**目标档标签恰命中一行** → 返回该行的**屏上编号**（如 `"3"`）。发该
/// 数字键即直选该档（kimi 菜单无屏上编号，仍走方向键闭环——数字直达是 codex 专属）。
///
/// # 与闭环判据的差别（放宽与收紧各一条，均有据）
///
/// - **不做全集闸**（[`menu_items_coherent`] 的目标档齐备不变式）：Guardian 关闭时
///   `Approve for me` 缺席、后续档编号前移——编号从**目标行自身**读出，缺席档不
///   影响目标行编号的正确性（这正是数字直达优于方向键的一处：无编号重排风险）。
///   非目标档的重复/异常不查：编号只取自目标行，别行的混入不改变目标行自身。
/// - **目标标签必须恰命中一行**（收紧）：0 行 = 菜单未画全（NotYet，可重试）；
///   \>1 行 = 屏上混入了含目标档名词的正文行（不变式 1 同源防线）→ Fatal 不猜。
/// - **屏上编号必须在 1..=9**：数字键域是单字符（B 族 `control_records`），
///   两位数意味着解析到了非菜单行 → Fatal。
///
/// 仅 Windows 执行侧（codex 菜单数字直达阶段机经屏读调用）消费——非 Windows
/// 编译下按 confirm.rs 先例条件化 allow。
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn codex_permission_digit_probe(lines: &[String], target: MamMode) -> PollStep<String> {
    let Some(target_label) = menu_target_label("codex", target) else {
        return PollStep::Fatal(format!(
            "codex 的权限菜单里没有「{}」档（不猜位置）",
            target.label()
        ));
    };
    let lowered: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();
    let Some(options) = codex_menu_options(lines, &lowered) else {
        return PollStep::NotYet(
            "codex 的权限菜单未出现或读不到档位表（不盲发数字键）".to_string(),
        );
    };
    let target_lower = target_label.to_lowercase();
    let hits: Vec<u32> = options
        .iter()
        .filter(|o| o.label.to_lowercase().starts_with(&target_lower))
        .map(|o| o.number)
        .collect();
    match hits.as_slice() {
        [d] if (1..=9).contains(d) => PollStep::Ready(d.to_string()),
        [d] => PollStep::Fatal(format!(
            "codex 权限菜单「{target_label}」的屏上编号（{d}）超出数字键域（1-9）——不猜，已中止"
        )),
        [] => PollStep::NotYet(format!(
            "codex 权限菜单窗内未见「{target_label}」行（菜单可能未画全）"
        )),
        _ => PollStep::Fatal(format!(
            "codex 权限菜单里「{target_label}」出现多行（屏上混入正文行）——不猜编号，已中止（不盲发数字键）"
        )),
    }
}

/// **残留清场 esc 后**等待锚消失的最大读屏拍数（每拍间隔 = `settle()`，生产为
/// SUBMIT_DELAY_MS=150ms ⇒ 兜底等待至多 ~750ms）。这就是「数字键等界面渲染出来
/// 再选」的兜底之一：清场不干净绝不开新菜单。
pub(crate) const RESIDUE_CLEAR_MAX_READS: usize = 5;

/// 屏上**是否存在 codex 权限 overlay**（菜单或 Full Access 确认框）——经**账本**
/// 判（标题槽位在屏即判；footer 可能被正文挤出可见窗，标题锚是存在的最低证据）。
///
/// 与 [`residual_overlay_present`] 的区别：本函数是**中性事实**（"屏上有 overlay"），
/// 后者是**该事实在清场语义下的名字**。picker 路径需要前者（overlay 在屏正是本次
/// 操作对象，不是「残留」），清场路径需要后者。
pub(crate) fn codex_overlay_present(lowered: &[String]) -> bool {
    crate::inject::anchor_ledger::detect(
        lowered,
        "codex",
        crate::inject::anchor_ledger::scenario::PERMISSION_MENU,
        crate::inject::anchor_ledger::slot::TITLE,
    )
    .is_some()
        || crate::inject::anchor_ledger::detect(
            lowered,
            "codex",
            crate::inject::anchor_ledger::scenario::FULL_ACCESS_CONFIRM,
            crate::inject::anchor_ledger::slot::TITLE,
        )
        .is_some()
}

/// 屏上 overlay 的**种类**（picker 路径用：菜单在场与确认框在场是两种不同面板）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexOverlay {
    /// 权限档位菜单（`Update Model Permissions`）
    PermissionMenu,
    /// Full Access 二次确认框（`Enable full access?`）
    ///
    /// **判据顺序**：确认框优先。0.160.0 实测（底料 §2 实证，mp-fa-confirm-open
    /// 同帧零菜单要素）：确认框**替换**菜单（非叠加）——确认框出现时菜单已不在屏，
    /// 本枚举的「确认框优先」顺序在 0.160.0 下两判据互斥、顺序无实际影响；判据顺序
    /// 保留以兼容旧版本可能的 overlay 叠加形态（见
    /// [`crate::inject::dialog::parse_dialog_clusters`] 的同源注记）。
    FullAccessConfirm,
}

/// 屏上 overlay 是哪种（都无 → `None`）。
pub(crate) fn codex_overlay_kind(lowered: &[String]) -> Option<CodexOverlay> {
    if crate::inject::anchor_ledger::detect(
        lowered,
        "codex",
        crate::inject::anchor_ledger::scenario::FULL_ACCESS_CONFIRM,
        crate::inject::anchor_ledger::slot::TITLE,
    )
    .is_some()
    {
        return Some(CodexOverlay::FullAccessConfirm);
    }
    if crate::inject::anchor_ledger::detect(
        lowered,
        "codex",
        crate::inject::anchor_ledger::scenario::PERMISSION_MENU,
        crate::inject::anchor_ledger::slot::TITLE,
    )
    .is_some()
    {
        return Some(CodexOverlay::PermissionMenu);
    }
    None
}

/// codex composer 的**占位文案**（空输入行显示的提示词，小写匹配）。屏读里占位
/// 与真实输入同形，只能按词表豁免——版本改词会导致「恒判残留」→ 守卫按 fail-safe
/// 中止（不误发，只是不可用），届时更新词表即可（台账「codex 模式切换改造」节登记）。
pub(crate) const CODEX_COMPOSER_PLACEHOLDERS: [&str; 1] = ["ask codex to do anything"];

/// 输入行清理的单次 backspace 上限（防呆：真实残留远小于此；超标按上限清）。
pub(crate) const COMPOSER_CLEAR_MAX_KEYS: usize = 64;

/// 提取 codex **composer 行的文本**（剥光标标记、trim；找不到 composer 行 → None）。
/// [`composer_residue`] 与段 2 窗尽兜底（判「/permissions 是否还留在输入行未执行」）
/// 共用此提取（单一实现）。
///
/// 判据（自底向上）：footer 行 = 最后一条含状态栏分隔符 ` · ` 的**非编号选项行**
/// （与 [`parse_codex_footer`] 同锚，composer 恒在底栏之上）；composer 行 = footer
/// 之上最近的一条**光标标记行**（[`crate::inject::dialog::strip_cursor_marker`]，
/// 标记集合覆盖 ›/❯/▶/>）。**实机取证（2026-09-23 探针
/// `codex_composer_codepoint_live_probe`）**：codex composer 前缀 = `›`(U+203A)+
/// 空格，与菜单高亮同字符——探针输出锁定，不再凭图像猜码点。
///
/// **0.156.1 起新增的排除条件（必须，否则判据被自身菜单污染）**：新菜单 footer
/// `enter select · esc back` **本身含 ` · `**，且它落在状态栏之上——若不排除，
/// 菜单开着时「最后一条含 ` · ` 的行」会落在菜单 footer 上；再往上找「最近的光标
/// 标记行」会**穿过整个菜单**回溯到对话流里的历史回显（如 `› D:/…/permissions`）
/// → [`composer_residue`] 把历史行当输入行 → 段 0.5 触发至多 64 次 backspace 后
/// 如实中止（功能不可用；更糟的是 backspace 会被**菜单**消费）。
///
/// 故两道排除：
/// 1. **overlay 在场 → 直接返回 `None`**（那一片区域属于菜单/确认框，不是 composer
///    ——判据在此时此地不适用，见 [`composer_residue`] 的「放行」口径）；
/// 2. **编号选项行不参与**（`parse_option_line` 命中即跳过）——底栏定位与候选行
///    两处都排除。真实状态栏形如 `… · C:\… · f2 to view`，不含 `N. ` 前缀。
pub(crate) fn codex_composer_text(lines: &[String]) -> Option<String> {
    let lowered: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();
    if codex_overlay_present(&lowered) {
        return None;
    }
    let footer_idx = lines.iter().rposition(|l| {
        l.contains(" · ") && crate::inject::dialog::parse_option_line(l).is_none()
    })?;
    for line in lines[..footer_idx].iter().rev() {
        // 候选 composer 行同样不得是编号选项行（菜单高亮行 `› 2. …` 也带光标标记）
        if crate::inject::dialog::parse_option_line(line).is_some() {
            continue;
        }
        let (rest, highlighted) = crate::inject::dialog::strip_cursor_marker(line);
        if !highlighted {
            continue; // 无光标标记前缀 → 不是 composer 行
        }
        return Some(rest.trim().to_string());
    }
    None
}

/// **输入行残留判定**（纯函数）——通用准则「**斜杠命令注入前，输入行必须纯净**」
/// （2026-09-23 用户指令）的 codex 判据。返回 `Some(可见字符数)` = composer 有残留
/// （须清理后再发命令，否则 `/permissions` 会与残留拼接成 `/permissions/permissions`
/// 之类的脏命令——用户实机走查事故现场）；`None` = 纯净或判据不可得。
///
/// 纯净 = composer 文本为空，或命中**占位词表**（空输入行显示提示词，屏读与真实
/// 输入同形只能按词豁免；版本改词会导致「恒判残留」→ 守卫按 fail-safe 中止不误发，
/// 届时更新词表即可）。**判据不可得 → `None`（放行）**：与守卫原则「判不清放行」
/// 同口径，主流程后续各段的屏读验证兜底。**已知边界**：历史回显（如已执行过的
/// `› /permissions`）位于对话流中部、不满足「footer 之上最近标记行」，天然不误判
/// （夹具 `codex-input-residue.txt` 锁定）。
pub(crate) fn composer_residue(lines: &[String]) -> Option<usize> {
    // **overlay 在场时判据不适用**（0.156.1 新 footer 含 ` · ` 暴露的形态）：菜单/
    // 确认框开着时，屏幕底部区域属于 overlay——那里没有 composer，且此时发 backspace
    // 会被**菜单**消费（不是清输入行，是改菜单选项，危害级）。故 overlay 在场一律
    // 返回「判据不可得」放行；调用方（[`codex_preflight`]）的顺序是**先清 overlay
    // 再查输入行**，保护不因此减弱（清场后重读的那一屏才做本判据）。
    let lowered: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();
    if codex_overlay_present(&lowered) {
        return None;
    }
    let text = codex_composer_text(lines)?;
    if text.is_empty() {
        return None;
    }
    let lower = text.to_lowercase();
    if CODEX_COMPOSER_PLACEHOLDERS
        .iter()
        .any(|p| lower.contains(p))
    {
        return None;
    }
    Some(text.chars().count())
}

/// codex **命令审批框在屏**判定（G3 账本锚；2026-10-10 诊断批）——标题槽位在屏
/// 即判（footer 可能被正文挤出可见窗，标题锚是存在的最低证据，与
/// [`codex_overlay_present`] 同口径）。消费方：[`codex_preflight`] 的 G1 闸
/// （审批框在场 → 直接 Err 零投递，文案点名）与数字直达失败回执的屏面摘要（G4）。
pub(crate) fn codex_approval_box_present(lowered: &[String]) -> bool {
    crate::inject::anchor_ledger::detect(
        lowered,
        "codex",
        crate::inject::anchor_ledger::scenario::APPROVE_COMMAND,
        crate::inject::anchor_ledger::slot::TITLE,
    )
    .is_some()
}

/// codex **composer 在场性**判定（G1，2026-10-10 诊断批）——idle/busy 主屏的
/// composer 光标行恒在，overlay（权限菜单/FA 确认框/审批框/codex 问题框）在场时
/// 该行被替换。实测判据（mpd 诊断批活体 dump，夹具 mode-perm-20261009/）：
///
/// - idle：`› Ask Codex to do anything`（codex-mp-idle-composer.txt）；
/// - busy：**占位行仍在**——busy 指示行是 `◦ Working (… esc to interrupt)`（`◦` =
///   U+25E6，非光标标记字符），其下 composer 占位行照旧
///   （codex-mp-busy-composer.txt）；
/// - overlay 在场：权限菜单（mpd-mpd-e4b-final）/审批框（codex-mp-approval-box.txt）
///   都把 composer 行替换掉（两分支判据全落空）。
///
/// 两分支：① 占位标记行（[`crate::inject::dialog::strip_cursor_marker`] 后含
/// **占位词表**）；② composer 行提取（[`codex_composer_text`]，覆盖「用户已打字、
/// 占位行被输入文本替换」形态——该函数对权限 overlay 在场按既有口径返回 `None`，
/// 恰与「不在场」同向）。两分支都落空 = **不在场**（fail-safe 取向：误判「不在场」
/// 的代价是多一次 esc/如实中止，误判「在场」的代价是命令打进 overlay——宁左）。
pub(crate) fn codex_composer_present(lines: &[String]) -> bool {
    for line in lines {
        let (rest, marked) = crate::inject::dialog::strip_cursor_marker(line);
        if !marked {
            continue;
        }
        let lower = rest.to_lowercase();
        if CODEX_COMPOSER_PLACEHOLDERS
            .iter()
            .any(|p| lower.contains(p))
        {
            return true;
        }
    }
    codex_composer_text(lines).is_some()
}

/// **残留 overlay 判定**（纯函数）：发 `/permissions` 前的一拍屏读里，是否已有
/// 上次遗留的**权限菜单或 Full Access 确认框**在屏（标题锚在屏即判——footer 锚
/// 可能被正文挤出可见窗，标题锚是存在的最低证据）。
///
/// 2026-09-23 用户实机走查的**事故根因**（两段）：
/// 1. 切换失败后菜单**留在屏上**，下一次点击注入的 `/permissions` 字符被 modal
///    菜单吞掉、回车**落在菜单上 = 确认当前高亮项**（用户看到 `Permissions
///    updated to Ask for approval` 与输入行堆积 `/permissions/permissions`）；
/// 2. （首轮修复漏掉的形态）**Full Access 确认框残留**不在菜单锚判据内——确认框
///    开着时 `/permissions` 被吞、enter **确认当前项**（`1. Yes, continue anyway`
///    = **意外启用完全信任**，用户点「只读」却得到 Full Access 的危害级误切）。
///
/// 故判据必须**同时覆盖两种 overlay**；检出 → 先 `esc` 清场再走正常流程。
///
/// 判据本体见 [`codex_overlay_present`]（账本查；本函数只是它在「残留」语义下的
/// 名字——同一事实两个语义，**不得**各写一遍扫描逻辑）。
pub(crate) fn residual_overlay_present(lines: &[String]) -> bool {
    let lowered: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();
    codex_overlay_present(&lowered)
}

/// kimi 菜单定位：footer 之后的**两行组**标签行（「恰为 `<档名> ← current` 或裸
/// `<档名>`」——描述行永不命中，N6 失败行 `Never interrupts you; …` 在此正确归属）。
/// 标题/footer 锚均经账本查（kimi 的菜单项在 footer 行**之后**——footer 紧贴标题
/// 下方，项列表在其下，与 codex 的「项在标题与 footer 之间」相反）。
fn locate_kimi_menu(
    lines: &[String],
    lowered: &[String],
    title_idx: usize,
    canon: &[(String, &str)],
) -> Option<Vec<DialogOption>> {
    let footer_idx = crate::inject::anchor_ledger::detect(
        &lowered[title_idx..],
        "kimi",
        crate::inject::anchor_ledger::scenario::PERMISSION_MENU,
        crate::inject::anchor_ledger::slot::FOOTER,
    )?
    .line_index
        + title_idx;
    let mut items: Vec<DialogOption> = Vec::new();
    for line in &lines[footer_idx + 1..] {
        let (rest, highlighted) = crate::inject::dialog::strip_cursor_marker(line);
        let trimmed = rest.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_lowercase();
        let Some(hit) = canon.iter().find(|(lo, _)| lower.starts_with(lo)) else {
            continue;
        };
        // 「恰为」判据：剥掉档名后，剩余只能是空（裸标签）或 `← current` 后缀——
        // 描述行剥掉前缀子串后仍有正文 → 落选（N6 的解）
        let remainder = lower[hit.0.len()..].trim();
        if !remainder.is_empty() && !remainder.contains(KIMI_CURRENT_SUFFIX) {
            continue;
        }
        items.push(DialogOption {
            number: items.len() as u32 + 1,
            label: format!("{}|{trimmed}", hit.1),
            highlighted,
        });
    }
    if items.len() < 2 {
        return None;
    }
    Some(items)
}

/// **菜单项表的一致性判据**（纯函数，可测；丁T4 收尾起是闭环首步的**必经闸**）——
/// [`locate_menu_items`] 的原始输出**必须过这道闸才能拿去导航**。
///
/// # 与关键词判据的分工（两道闸，缺一不可）
///
/// 关键词判据（[`locate_menu_items`]）挡的是「**一行里出现多个档位词**」（≥2 → 跳过）；
/// 本闸挡的是「**同一个档位词出现两次**」——即**同档标签的第二行**。二进制实证的两句
/// 警示语（`We strongly recommend selecting "Ask for approval" instead.` 等）**恰好各只
/// 命中一个标签**，靠不变式 1 拦下（见 `distractor_lines_are_handled_by_the_two_gates`）。
///
/// # 两条不变式（都由「菜单应含哪些档」这一**有据事实**导出，不依赖菜单块形态）
///
/// 1. **每个档位标签恰好出现一次**：权限菜单每一档只渲染一行（实机取证档 §1/§5 逐字
///    原文证实：codex 四行、kimi 三行，各行只含自己的档位词，描述在同行右列或下一行
///    但**不含档位词**），故同一标签出现两次 = 屏上混进了别的行（评审的反例：`/status`
///    回显 `Read Only (sandbox: read-only)` 与真菜单项 `Read Only` **重复**）。
/// 2. **目标档全集齐备**：该工具的三个用户可切档都得在场（§2.6 codex/kimi 各三档；
///    实机取证档 §1 记录的 codex 档位序含 `Approve for me`，但它只在 Guardian 开启时
///    出现——故**不算进**目标全集，把它算进会让 Guardian 关闭的正常菜单被判不完整而
///    永久无法切档）。缺档 = 菜单被对话框/正文挤掉一部分 → 位置不可信 → 拒绝。
///
/// # 残留风险（如实申报，不假装已消除）
///
/// 若同屏混入的行**恰好补齐了缺席的目标档**（菜单只画出 2 档、正文里正好有一行含第 3
/// 个目标标签）且不与真项重复，两条不变式都会通过。这需要两个巧合同时发生。
///
/// **闭环对本缺口的缓解（不声称消除）**：导航每步复核「位移恰好 1 行」，若混入行落在
/// 高亮与目标之间，终端的一步会跳出 ±1 → **中止**；只有混入行落在步进路径之外（如目标
/// 下方）时才蒙混——那种位置对编号平移同幅，对步进无影响。**本缺口仍留在台账**。
pub fn menu_items_coherent(items: &[DialogOption], tool: &str) -> bool {
    let seen: Vec<&str> = items
        .iter()
        .filter_map(|o| o.label.split_once('|').map(|(l, _)| l))
        .collect();
    if seen.len() != items.len() {
        return false; // 形态异常（label 未按 `标签|原文` 构造）
    }
    // 不变式 1：无重复
    for (i, a) in seen.iter().enumerate() {
        if seen[i + 1..].iter().any(|b| a.eq_ignore_ascii_case(b)) {
            log::debug!("权限菜单定位：标签重复（{a}）→ 屏上混入正文行，中止（不盲发方向键）");
            return false;
        }
    }
    // 不变式 2：目标档全集齐备
    let want = menu_target_labels(tool);
    if want.is_empty() {
        return false; // 未实测工具
    }
    let missing: Vec<&&str> = want
        .iter()
        .filter(|w| !seen.iter().any(|s| s.eq_ignore_ascii_case(w)))
        .collect();
    if !missing.is_empty() {
        log::debug!("权限菜单定位：目标档不全（缺 {missing:?}）→ 中止（不盲发方向键）");
        return false;
    }
    true
}

/// 工具 → **目标档标签全集**（用户可切的三档；codex/kimi 各三条，见 §2.6）。
///
/// **不含** codex 的 `Approve for me`：它只在 Guardian 开启时出现（实机取证档 §1 的
/// 菜单里它在场、§4 的另一次菜单里也在场，但 §2.6 未把它列为 兔维斯 的目标档），
/// 且算进完整集会误伤 Guardian 关闭时的正常菜单。
fn menu_target_labels(tool: &str) -> &'static [&'static str] {
    match tool {
        "codex" => &CODEX_TARGET_LABELS,
        "kimi" => &KIMI_TARGET_LABELS,
        _ => &[],
    }
}

/// codex 权限菜单的**目标档标签全集**（与 [`menu_target_label`] 的取值一一对应）
const CODEX_TARGET_LABELS: [&str; 3] = ["Read Only", "Ask for approval", "Full Access"];
/// kimi 权限菜单的目标档标签全集（§2.6 三档）
const KIMI_TARGET_LABELS: [&str; 3] = ["Always Ask", "Ask When Needed", "Never Ask"];

/// 闭环导航的**解析计划**：一次屏读行集 →（行表, 目标行标签）。
///
/// 两个调用点共用同一个闭环（[`navigate_until_highlighted`]），只有「怎么从屏上认行」
/// 不同：
/// - [`MenuNavPlan::PermissionTier`]：第二段的权限菜单——定位用**关键词包含 + 计数互斥**
///   （[`locate_menu_items`]）并过一致性闸（[`menu_items_coherent`]），目标 = 该档的官方标签；
/// - [`MenuNavPlan::ConfirmAffirmative`]：第三段的 Full Access 二次确认框——定位用
///   **编号对话框**解析器（[`crate::inject::dialog::parse_dialog_options`]），目标 =
///   **唯一**含 `continue` 的肯定项。
///
/// **确认框为什么不能用菜单定位器**：确认框的说明文里满是小写的 `full access`
/// （实机档案 §2 的两行说明都含），菜单定位器的档位词表会与说明文打架；而确认框本身
/// 是**编号 + 光标 `›`** 的标准对话框（档案 §2：「与批次丙 T5 解析器同形」），
/// `parse_dialog_options` 正是为编号对话框写的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuNavPlan<'a> {
    /// 第二段：权限菜单里的目标档
    PermissionTier {
        /// 工具名（词表与一致性闸的判据来源）
        tool: &'a str,
        /// 目标档
        target: MamMode,
    },
    /// 第三段：Full Access 确认框的肯定项（标签含 `keyword` 的**唯一**一项）
    ConfirmAffirmative {
        /// 工具名（错误文案用）
        tool: &'a str,
        /// 肯定项关键词（实机为 `continue`，见 [`FULL_ACCESS_AFFIRMATIVE_KEYWORD`]）
        keyword: &'a str,
    },
}

/// 一次屏读探测的**三态**结果（轮询与闭环共用同一套判据）。
///
/// 把「还没画出来」（可重试）与「形态异常」（立即中止）分开，是调用方语义的需要：
/// - 菜单路径：窗内没画出来 → 中止 + 如实回执；形态异常（档位表不自洽）→ 同样中止，
///   但**不必再等**（等下去也不会自洽）；
/// - 确认框路径：窗内没出现 → **不当作失败**（用户可能关过该警告），继续走回执核验；
///   形态异常（多个肯定项）→ 立即中止（零投递）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollStep<T> {
    /// 拿到可行动结果
    Ready(T),
    /// 本轮还看不到目标形态（**可重试**，`String` = 用户看得懂的原因）
    NotYet(String),
    /// 形态异常（**立即中止**，`String` = 用户看得懂的原因）
    Fatal(String),
}

impl MenuNavPlan<'_> {
    /// 阶段名（**用户看得懂**的中文中止原因要用它）
    pub fn stage(&self) -> &'static str {
        match self {
            Self::PermissionTier { .. } => "权限菜单",
            Self::ConfirmAffirmative { .. } => "Full Access 确认框",
        }
    }

    /// 轮询用的一次屏读探测（三态见 [`PollStep`]）。
    pub(crate) fn probe(&self, lines: &[String]) -> PollStep<(Vec<DialogOption>, String)> {
        match *self {
            Self::PermissionTier { tool, target } => {
                let Some(target_label) = menu_target_label(tool, target) else {
                    return PollStep::Fatal(format!(
                        "{tool} 的权限菜单里没有「{}」档（不猜位置）",
                        target.label()
                    ));
                };
                let Some(raw) = locate_menu_items(lines, menu_labels(tool)) else {
                    return PollStep::NotYet(format!(
                        "{tool} 的权限菜单未出现或读不到档位表（不盲发方向键）"
                    ));
                };
                if !menu_items_coherent(&raw, tool) {
                    return PollStep::Fatal(format!(
                        "{tool} 的权限菜单档位表不自洽（混入了正文行或被遮挡）——不猜位置，请在终端选择"
                    ));
                }
                // `规范标签|屏上原文` → 取规范标签（屏上原文仍留在 `locate_menu_items`
                // 的原始输出与日志里；闭环只需要可比较的规范标签）
                let items = raw
                    .into_iter()
                    .map(|o| DialogOption {
                        number: o.number,
                        label: o
                            .label
                            .split_once('|')
                            .map(|(l, _)| l.to_string())
                            .unwrap_or(o.label),
                        highlighted: o.highlighted,
                    })
                    .collect();
                PollStep::Ready((items, target_label.to_string()))
            }
            Self::ConfirmAffirmative { tool, keyword } => confirm_box_probe(lines, tool, keyword),
        }
    }

    /// 一次屏读 →（行表, 目标行标签）；**任何**未就绪或不合法都收敛为 `Err`
    /// （闭环的中途读屏用这个：导航途中目标形态消失 = 中止，而不是继续等）。
    pub(crate) fn snapshot(&self, lines: &[String]) -> Result<(Vec<DialogOption>, String), String> {
        match self.probe(lines) {
            PollStep::Ready(v) => Ok(v),
            PollStep::NotYet(e) | PollStep::Fatal(e) => Err(e),
        }
    }

    /// 每步复核用的**合法行标签**集（小写）——新高亮行的标签必须属于它。
    fn known_labels(&self, items: &[DialogOption]) -> Vec<String> {
        match self {
            Self::PermissionTier { tool, .. } => {
                menu_labels(tool).iter().map(|l| l.to_lowercase()).collect()
            }
            Self::ConfirmAffirmative { .. } => {
                items.iter().map(|o| o.label.to_lowercase()).collect()
            }
        }
    }
}

/// **Full Access 二次确认框**的一次屏读探测（第三段的判据单点）。
///
/// 逐**编号簇**找「恰好命中肯定项关键词一次」的那一簇（为什么不能取最长簇：见
/// [`crate::inject::dialog::parse_dialog_clusters`] 的文档——确认框出现时权限菜单可能
/// 仍在屏上，最长簇是菜单）。三种结果：
/// - 无任何簇含关键词 → `NotYet`（**不是错**：用户可能关过该警告，二进制有
///   `Continue and don't warn again.` 文案）；
/// - 同一簇里含关键词的选项 ≠ 1，或**多个簇**各含一个 → `Fatal`（不猜，零投递）；
/// - 恰好一个 → 可导航（行表 + 该行的屏上标签原文）。
pub(crate) fn confirm_box_probe(
    lines: &[String],
    tool: &str,
    keyword: &str,
) -> PollStep<(Vec<DialogOption>, String)> {
    let key = keyword.to_lowercase();
    let mut found: Option<(Vec<DialogOption>, String)> = None;
    for cluster in crate::inject::dialog::parse_dialog_clusters(lines) {
        if cluster.len() < 2 || cluster.len() > crate::inject::dialog::MAX_DIALOG_OPTIONS {
            continue; // 不成对话框的簇（单行 / 超数字键域）不可能是确认框
        }
        let hits: Vec<usize> = cluster
            .iter()
            .enumerate()
            .filter(|(_, o)| o.label.to_lowercase().contains(key.as_str()))
            .map(|(i, _)| i)
            .collect();
        let target = match hits.len() {
            0 => continue,
            1 => cluster[hits[0]].label.clone(),
            // 同一簇里多个肯定项：**不猜**（宁可中止也不误选「不再提醒」类变体）
            n => {
                return PollStep::Fatal(format!(
                    "{tool} Full Access 确认框里含「{keyword}」的选项有 {n} 个（应恰好 1 个）——零投递，请在终端确认"
                ));
            }
        };
        if found.is_some() {
            return PollStep::Fatal(format!(
                "{tool} 屏上有多个含「{keyword}」的选项簇，无法判定哪个是 Full Access 确认框——零投递，请在终端确认"
            ));
        }
        found = Some((cluster, target));
    }
    match found {
        Some(v) => PollStep::Ready(v),
        None => PollStep::NotYet(format!(
            "{tool} 未见 Full Access 二次确认框（屏上无含「{keyword}」的编号选项簇）"
        )),
    }
}

/// 取**唯一**高亮行（无高亮/多高亮 → Err）。与
/// [`crate::inject::dialog::navigation_anchors`] 的保守面同源，但错误文案带阶段名
/// （闭环的中止原因要能直接讲给用户听）。
fn unique_highlight(items: &[DialogOption], stage: &str) -> Result<usize, String> {
    let mut hl: Option<usize> = None;
    for (i, o) in items.iter().enumerate() {
        if o.highlighted {
            if hl.is_some() {
                return Err(format!(
                    "{stage}有多行高亮标记（形态异常）——不猜起点，已中止（不盲发方向键）"
                ));
            }
            hl = Some(i);
        }
    }
    hl.ok_or_else(|| format!("{stage}认不出当前高亮行——不猜起点，已中止（不盲发方向键）"))
}

/// **闭环导航**（纯逻辑，可测）：每发一个方向键就**重新屏读复核**，**只有**高亮确实落在
/// 目标行时才发 `enter`。
///
/// # 为什么要闭环（用户实机取证的**方法论要求**，原文见取证档案 §8）
///
/// > 每按一次 ↓ 或者 ↑ 就**重新屏读**、确认高亮确实移到下一项……你**绝对不能默认**
/// > 这个按键在什么位置，因为你并不知道 兔维斯 不在场的时候，这个敞开本身是什么权限，
/// > 所以你必须读取 `/permissions` 之后实际哪个权限被选择了，才能从此定位切换到正确权限上。
///
/// 旧实现（丁T4 首版的 `menu_navigation_sequence`，**本批删除**）是「一次算步进 + 盲发序列」：它先按
/// 屏上行序算 `[↑/↓ × k, enter]`，再把整串键一次投完——中途任何一步没生效（吞键、
/// 重绘竞态、菜单行数与我们算的不一致）都会让后续按键落在一个**错误的位置**，而最后
/// 那个 `enter` 是**盲提交**。闭环把「提交」变成一个**有条件的动作**：只有屏读确认
/// 高亮行就是目标档时才发回车。
///
/// # 逐步判据（任一不满足即中止，**不盲发回车**）
///
/// 1. 读屏 → 定位（`plan.snapshot`）+ 一致性闸 → 取**唯一**高亮行（无/多高亮 → 中止）；
/// 2. 高亮行标签 == 目标标签 → 发 `enter`，结束（**唯一的提交点**）；
/// 3. 否则按目标相对当前的方向发**一次** `↓` 或 `↑` → `settle()`（生产为
///    `SUBMIT_DELAY_MS` 睡眠，给 TUI 重绘）→ **重读屏**；
/// 4. 复核：新高亮行**必须存在**、标签**必须变了**（未变 → 「高亮未移动」，中止）、
///    位移**必须恰好 1 行**（多/少 → 屏上行序与我们的表不一致，中止）、
///    新标签**必须是合法行标签**（不是 → 中止）；
/// 5. 回到 2；方向键累计发送数超过「菜单项数 + 2」→ 中止（防死循环）。
///
/// **为什么位移必须恰好 1 行**（比 §8 的方法论更严一格，方向是保守）：菜单项行与行之间
/// 若混进了我们**没数进去**的行（正文行恰好命中/未命中的边界形态），终端的「下一项」
/// 与我们的「下一项」就不是同一行——一次 ↓ 会跳出 ±1，此时**立刻中止**，而不是继续按
/// 我们的表走。`enter` 只在标签比对命中时发出，因此即便表错，也不会提交到错的档上。
///
/// # 终端 IO 走 [`MenuTerminal`]（测试用脚本化屏序列驱动，不依赖真 conhost）
///
/// [`MenuTerminal::read`] 返回一屏行集（`None` = 读不到屏）；[`MenuTerminal::send`]
/// 投递一个按键；[`MenuTerminal::settle`] 给 TUI 重绘留时间（生产 = 睡眠，测试 = 空
/// 操作/推进脚本）。返回**实际发出的按键序列**（含末尾 `enter`）——测试据此断言
/// 「发过什么、没发什么」。既有按三闭包组织的调用方用 [`Closures`] 适配。
pub fn navigate_until_highlighted<T: MenuTerminal>(
    terminal: &mut T,
    plan: &MenuNavPlan<'_>,
) -> Result<Vec<String>, String> {
    let stage = plan.stage();
    let mut sent: Vec<String> = Vec::new();
    // 1. 首次屏读 + 定位 + 一致性闸 + 唯一高亮
    let first = terminal
        .read()
        .ok_or_else(|| format!("{stage}读不到屏幕——不盲发任何键（请人工核对终端）"))?;
    let (mut items, target_label) = plan.snapshot(&first)?;
    let known = plan.known_labels(&items);
    let mut cur = unique_highlight(&items, stage)?;
    // 迭代上限 = 菜单项数 + 2（方向键的累计发送数；正常收敛远小于它）
    let max_steps = items.len() + 2;
    let mut steps = 0usize;
    loop {
        // 2. 高亮已在目标行 → 这是**唯一**的提交点
        if items[cur].label.eq_ignore_ascii_case(&target_label) {
            terminal.send("enter")?;
            sent.push("enter".to_string());
            log::debug!(
                "闭环导航（{stage}）：高亮已落在「{target_label}」→ 提交（共 {} 键）",
                sent.len()
            );
            return Ok(sent);
        }
        let target_idx = items
            .iter()
            .position(|o| o.label.eq_ignore_ascii_case(&target_label))
            .ok_or_else(|| format!("{stage}上找不到目标行「{target_label}」（不猜位置）"))?;
        // 5. 迭代上限（每次方向键都计数，含「高亮不动」被检出前的那些）
        if steps >= max_steps {
            return Err(format!(
                "{stage}方向键已发 {steps} 次仍未落到「{target_label}」——超过上限（{}），已中止（未发回车）",
                max_steps
            ));
        }
        // 3. 按方向走一步（目标在下方 ↓、在上方 ↑；菜单是线性选择器，两个方向都不会越界：
        //    目标在前 ⇒ 当前必不在首行，目标在后 ⇒ 当前必不在末行）
        let key = if target_idx > cur { "down" } else { "up" };
        terminal.send(key)?;
        sent.push(key.to_string());
        steps += 1;
        terminal.settle();
        // 4. 每步复核：重读屏 → 重新定位 → 重新取高亮
        let next = terminal.read().ok_or_else(|| {
            format!(
                "{stage}步进后读不到屏幕（已发 {steps} 个方向键）——已中止，未发回车（不盲提交）"
            )
        })?;
        let (items2, target2) = plan.snapshot(&next)?;
        if !target2.eq_ignore_ascii_case(&target_label) {
            return Err(format!(
                "{stage}的候选行在步进后变了（目标行由「{target_label}」变为「{target2}」）——已中止，未发回车"
            ));
        }
        let cur2 = unique_highlight(&items2, stage)?;
        let new_label = items2[cur2].label.clone();
        if new_label.eq_ignore_ascii_case(&items[cur].label) {
            return Err(format!(
                "{stage}按一次 {key} 后高亮仍停在「{new_label}」——终端可能未响应或吞键，已中止（不盲发回车）"
            ));
        }
        let moved = cur2 as i64 - cur as i64;
        if moved != 1 && moved != -1 {
            return Err(format!(
                "{stage}按一次 {key} 后高亮位移了 {moved} 行（应为 1 行）——屏幕行序与我们的档位表不一致，已中止（未发回车）"
            ));
        }
        let lowered = new_label.to_lowercase();
        if !known.contains(&lowered) {
            return Err(format!(
                "{stage}按一次 {key} 后高亮移到了未知行「{new_label}」——已中止（未发回车）"
            ));
        }
        items = items2;
        cur = cur2;
    }
}

/// 第三段确认框的**肯定项关键词**（实机取证档案 §2 原文 `› 1. Yes, continue anyway`；
/// 二进制另有 `Continue and don't warn again.` 变体——同一关键词都覆盖）。
pub const FULL_ACCESS_AFFIRMATIVE_KEYWORD: &str = "continue";

/// 菜单路径各段的**编排结果**（见 [`run_menu_stages`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuStagesOutcome {
    /// 第二段实际发出的键序（含末尾 `enter`）
    pub menu_keys: Vec<String>,
    /// 第三段是否出现并走完（`false` = 确认框没出现——**不当作失败**）
    pub confirm_done: bool,
    /// 是否屏读到工具的成功回执行（`None` = 读不到屏，无法核验）
    pub receipt_seen: Option<bool>,
}

/// 菜单路径的**终端 IO 三件套**（读屏 / 发键 / 步进等待）——[`run_menu_stages`] 与
/// [`navigate_until_highlighted`] 共用的能力包。
///
/// 抽成 trait 而不是三个闭包参数：`run_menu_stages` 已经要 5 个参数（工具/组/档 +
/// 两个轮询），再加读屏/发键/等待就 8 个（clippy `too_many_arguments`），而这三者本就
/// 是**同一个「终端」概念的三面**——生产实现一次装好、闭环与编排共用，测试里换脚本化
/// 实现即可。**测试驱动方式见 `tests` 模块的 `run_stage_script`**。
pub trait MenuTerminal {
    /// 读一屏（`None` = 读不到屏）
    fn read(&mut self) -> Option<Vec<String>>;
    /// 发一个键（`Err` = 投递失败，立即中止）
    fn send(&mut self, key: &str) -> Result<(), String>;
    /// 按键后给 TUI 重绘留时间（生产 = 睡眠；测试 = 空操作或推进脚本）
    fn settle(&mut self);
}

/// 把三个独立闭包（读/发/等）适配成 [`MenuTerminal`]——生产侧与既有单测的便利入口。
///
/// **为什么要有它**：既有测试（`run_scripted_nav`）与端点侧都是按「三个闭包」组织的，
/// 若强行改成 trait 对象就要把每处调用点的可变捕获拆开重写。适配器一行到位，且
/// **零语义**（纯转发）。
pub struct Closures<R, S, W> {
    /// 读屏
    pub read: R,
    /// 发键
    pub send: S,
    /// 步进等待
    pub settle: W,
}

impl<R, S, W> MenuTerminal for Closures<R, S, W>
where
    R: FnMut() -> Option<Vec<String>>,
    S: FnMut(&str) -> Result<(), String>,
    W: FnMut(),
{
    fn read(&mut self) -> Option<Vec<String>> {
        (self.read)()
    }
    fn send(&mut self, key: &str) -> Result<(), String> {
        (self.send)(key)
    }
    fn settle(&mut self) {
        (self.settle)()
    }
}

/// **菜单路径的编排**（第二段 → 第三段 → 回执核验；纯逻辑，读屏/发键/等待全部注入）。
///
/// 抽到内核而不是写在端点里，理由与 [`MenuNavPlan`] 同源：**段与段之间的控制流是
/// 判据的一部分**（第三段只在 codex×Full Access 走、确认框缺席不算失败），写在
/// `#[cfg(windows)]` 的端点闭包里就**只有实机能覆盖**——本批已多次栽在「测试覆盖不到
/// 的实机路径」。此处终端 IO 是 [`MenuTerminal`] 参数，门禁里用脚本化屏序列就能把
/// **整条编排**（含分段与回执核验）钉住。
///
/// # 各段的语义（逐条与实机取证档对应）
///
/// 1. **第二段**：`poll` 循环用 [`MenuNavPlan::PermissionTier`] 探测，直到菜单画出
///    （`NotYet` 重试）/ 立即中止（`Fatal`）/ 窗尽（`Err`）→ [`navigate_until_highlighted`]；
/// 2. **第三段**：仅 [`needs_full_access_confirm`] 为真时走；确认框缺席 → `confirm_done
///    = false` 并**继续**（档案 §3：1/2/3 无此段；二进制 `Continue and don't warn again.`
///    说明用户可关掉该警告）；
/// 3. **回执核验**：`poll_receipt` 找 [`permission_receipt_verified`]，窗尽 →
///    `Some(false)`（**不是失败**，只影响回执里的 `verified`）。
///
/// # 参数
///
/// `poll(plan, required)`——`Ok(Some(screen))` = 目标形态已出现（附该屏），
/// `Ok(None)` = 窗尽未出现（`required=false` 时才会出现这种返回），`Err` = 形态异常
/// （立即中止）。`poll_receipt()`——`Ok(Some(screen))` = 屏上出现成功回执行，
/// `Ok(None)` = 窗尽未见。生产实现见 `remote::api::poll_menu_stage` / `poll_receipt`；
/// 测试用脚本化序列。
pub fn run_menu_stages<P, Q, T>(
    tool: &str,
    group: ModeGroupId,
    target: MamMode,
    mut poll: P,
    mut poll_receipt: Q,
    terminal: &mut T,
) -> Result<MenuStagesOutcome, String>
where
    P: FnMut(&MenuNavPlan<'_>, bool) -> Result<Option<Vec<String>>, String>,
    Q: FnMut() -> Result<Option<Vec<String>>, String>,
    T: MenuTerminal,
{
    // ===== 第二段：菜单（必须出现；不出现或形态异常都中止）=====
    let menu_plan = MenuNavPlan::PermissionTier { tool, target };
    // `false` = 「必须出现」（窗尽未出现 → Err 而非 Ok(None)）
    poll(&menu_plan, false)?.ok_or_else(|| {
        format!("{tool} 的权限菜单未出现或读不到档位表（不盲发方向键）；请人工核对终端")
    })?;
    let menu_keys = navigate_until_highlighted(terminal, &menu_plan)
        .map_err(|why| format!("{why}；请人工核对终端（命令已发送，档位未切）"))?;
    // ===== 第三段：Full Access 二次确认框（仅 codex × 权限组 × Full Access）=====
    let mut confirm_done = false;
    if needs_full_access_confirm(tool, group, target) {
        let confirm_plan = MenuNavPlan::ConfirmAffirmative {
            tool,
            keyword: FULL_ACCESS_AFFIRMATIVE_KEYWORD,
        };
        // `true` = 「可出现」（窗尽未出现 → Ok(None)，**不当作失败**）
        if poll(&confirm_plan, true)?.is_some() {
            navigate_until_highlighted(terminal, &confirm_plan)
                .map_err(|why| format!("{why}；请人工核对终端（Full Access 可能未生效）"))?;
            confirm_done = true;
        } else {
            log::debug!("模式菜单：未出现 Full Access 二次确认框（可能用户已关闭该警告）");
        }
    }
    // ===== 成功回执核验（所有档位）=====
    let receipt_seen = poll_receipt()?.is_some();
    Ok(MenuStagesOutcome {
        menu_keys,
        confirm_done,
        receipt_seen: Some(receipt_seen),
    })
}

/// G2 **hijack 对账**（2026-10-10 诊断批）——「/permissions+回车」落在审批框上时，
/// 回车 = 批准了待审批命令（真实副作用，诊断批 E1/E5 实证：curl 被执行 / 菜单无痕
/// 关闭），菜单不开 → 数字直达窗尽报「菜单未出现」+ composer 干净（补 enter 兜底
/// 不触发）。本对账在**窗尽且 composer 干净**的失败路径上读最后一拍屏：出现审批
/// 结果新行（`you approved` / `approved to always run`，小写 contains——词形取诊断
/// 批报告，审批框消失后打出的确认行）→ 错误文案**升级**为点名「误批准」，让用户
/// 立即到终端核对；否则维持原文案。
fn hijack_reconciled_error(base: String, last: Option<&Vec<String>>) -> String {
    let hijacked = last.is_some_and(|l| {
        l.iter().any(|x| {
            let lo = x.to_lowercase();
            lo.contains("you approved") || lo.contains("approved to always run")
        })
    });
    if hijacked {
        "权限切换未执行，且误批准了一条待审批命令（回车落在了审批框上）——请立即到终端核对刚才批准的命令！"
            .to_string()
    } else {
        base
    }
}

/// G4 **屏面摘要**（2026-10-10 诊断批，推论 5）——数字直达失败回执附三值摘要
/// （审批框在场？/权限菜单在场？/composer 在场？），下次诊断免猜。三值分别用
/// G3 账本锚 / [`codex_overlay_kind`] / G1 composer 判据，与闸同一实现。
///
/// **「菜单=不在 且 composer=在」附主屏注记**（2026-10-10 codex-perm-blind 活体
/// 取证批）：菜单判据在 codex 0.162.1（conhost+WT 双宿主、idle/busy/重开/选档
/// 六形态）活体全绿——该组合 = 读屏那一刻终端停在**主屏**（`/permissions` 未开出
/// 菜单：回合忙、命令未被执行、或读的不是用户看的那块屏），**不是判据失明**。
/// 注记点名这一结论与「核对会话卡对应窗口」的动作，免得下次再走「探针瞎了」的
/// 弯路（本次事故的直接教训）。
fn codex_screen_summary(last: Option<&Vec<String>>) -> String {
    let Some(l) = last else {
        return "（屏面摘要：读不到屏）".to_string();
    };
    let lowered: Vec<String> = l.iter().map(|x| x.to_lowercase()).collect();
    let in_out = |b: bool| if b { "在" } else { "不在" };
    let mut s = format!(
        "（屏面摘要：审批框={}/菜单={}/composer={}）",
        in_out(codex_approval_box_present(&lowered)),
        in_out(codex_overlay_kind(&lowered) == Some(CodexOverlay::PermissionMenu)),
        in_out(codex_composer_present(l)),
    );
    if codex_composer_present(l)
        && codex_overlay_kind(&lowered).is_none()
        && !codex_approval_box_present(&lowered)
    {
        s.push_str("〔读屏时终端停在主屏——/permissions 未开出菜单（回合忙或命令未被执行）；若终端屏上确有菜单，请核对会话卡对应的终端窗口是否为本会话〕");
    }
    s
}

/// **codex 权限组的数字直达编排**（2026-09-23 用户实测裁决；取代 [`run_menu_stages`]
/// 在 codex 上的职责，kimi 仍走闭环——kimi 菜单无屏上编号，数字键无意义）。
///
/// # 各段（每段都可独立中止）
///
/// 0. **残留防护**（事故根因，见 [`residual_overlay_present`]）：读一拍屏，已有上次
///    遗留的权限菜单/确认框 → 先 `esc` 关闭再开新菜单（条件等待锚消失）。读不到屏
///    → 跳过防护（后续段会如实失败，不因防护失败而额外报错）；
///
/// 0.5. **输入行纯净前置**（通用准则 2026-09-23，[`composer_residue`]）：composer
///      有残留 → backspace 逐字符清 + **闭环屏读验证**（清完仍不纯净 → 如实中止，
///      不盲发脏命令）；
///
/// 1. **开菜单**：`open_menu()`（生产 = `/permissions` 文本 + 回车）；
/// 2. **数字直达**：`poll_digit()` 轮询窗内读目标档**屏上编号**
///    （[`codex_permission_digit_probe`]）→ 发该数字键（**无回车**——实测数字键
///    一次直达，回车是旧闭环的提交键，这里发了反而可能误确认）；
/// 3. **二阶段确认**（仅 Full Access，[`needs_full_access_confirm`]）：`poll_confirm()`
///    等确认框簇 → 肯定项（唯一含 `continue` 项）的**屏上编号**直达（用户实测：
///    `Enable full access?` 按 `1` = Yes, continue anyway）；确认框缺席**不当作失败**
///    （用户可能关过该警告，与 [`run_menu_stages`] 同语义）；
/// 4. **回执核验**：`poll_receipt()` 找「切到目标档」的回执行——**双代词形**
///    （0.160.0 `Permission selection requested: <标签>` 优先，0.154 旧锚
///    `permissions updated to <label>` 兼容；P1-1 终审，判据单点
///    [`permission_receipt_verified`]）。
///
/// 抽进内核的理由与 [`run_menu_stages`] 同源：段间控制流（残留防护、确认框缺席
/// 不算失败）是判据的一部分，写在端点 `#[cfg(windows)]` 闭包里就只有实机能覆盖。
pub fn run_codex_permission_stages<O, P, Q, R, W, T>(
    target: MamMode,
    mut open_menu: O,
    mut poll_digit: P,
    mut poll_confirm: Q,
    mut poll_receipt: R,
    mut wait_floor: W,
    terminal: &mut T,
) -> Result<MenuStagesOutcome, String>
where
    O: FnMut() -> Result<(), String>,
    P: FnMut() -> Result<String, String>,
    Q: FnMut() -> Result<Option<Vec<DialogOption>>, String>,
    R: FnMut() -> Result<Option<Vec<String>>, String>,
    W: FnMut(),
    T: MenuTerminal,
{
    // ===== 段 0（G1 composer 在场性）+ 段 0·续（残留 overlay 清场）+ 段 0.5
    // （输入行纯净）：公共前置，见 [`codex_preflight`] =====
    //
    // 2026-09-23 用户实机走查复盘：残留**确认框**同样必须清（确认框开着时
    // `/permissions` 被吞、enter 会确认 `1. Yes, continue anyway` = 意外启用
    // 完全信任）；且 esc 后**必须条件等待锚消失**——esc 到 TUI 重绘完成有时间差，
    // 立刻开菜单仍可能撞上未消散的旧 overlay（「数字敲在旧对话框里」的根因）；
    // 固定睡不可靠（condition-based-waiting）：轮询读屏直到锚消失，窗尽如实中止。
    // 通用准则「斜杠命令注入前输入行必须纯净」同在此段（实测现场
    // `/permissions/permissions`）。
    let _ = codex_preflight(terminal)?;
    // ===== 段 1：开菜单 =====
    open_menu()?;
    // **硬性最短间隔**（用户指令 2026-09-23：相邻步骤 ≥0.5s，与轮询「并存取最大」
    // ——先硬等满 0.5s 再进入下一步的轮询窗；轮询 Ready 再快也不早于 0.5s）。
    // 见 [`crate::inject::timing::MODE_STEP_MIN_GAP_MS`]。
    wait_floor();
    // ===== 段 2：数字直达 =====
    let digit = match poll_digit() {
        Ok(d) => d,
        Err(why) => {
            // **窗尽兜底（2026-09-23 用户实测：菜单迟迟未出现）**：提交键落下时
            // codex TUI 可能正忙（如 MCP 报错刷屏/重绘），命令**没被提交**——
            // `/permissions` 还留在输入行。判据 = composer 文本含 `/permissions`
            // → **补发一次 tab**（提交键语义 2026-10-10 3787fa7d 定案：文本注入+
            // 150ms 后的动作与残留补发均为 tab；命令文本还在，不重复注入）再等
            // 一窗；composer 干净/读不到屏 → 无从补救，如实失败。
            //
            // 失败回执两道加固（2026-10-10 诊断批）：G2 hijack 对账（回车可能落
            // 在了审批框上 = 误批准，文案点名）+ G4 屏面摘要（三值快照，下次
            // 诊断免猜）。
            let last = terminal.read();
            let leftover = last
                .as_ref()
                .and_then(|l| codex_composer_text(l))
                .is_some_and(|t| t.contains("/permissions"));
            if !leftover {
                let msg = hijack_reconciled_error(why, last.as_ref());
                return Err(format!("{}{}", msg, codex_screen_summary(last.as_ref())));
            }
            log::info!("codex 权限切换：菜单窗尽且输入行仍有 /permissions（回车未生效）→ 补发一次 tab 再等一窗");
            terminal.send("tab")?;
            terminal.settle();
            poll_digit().map_err(|e2| {
                let last2 = terminal.read();
                format!(
                    "{}{}",
                    hijack_reconciled_error(e2, last2.as_ref()),
                    codex_screen_summary(last2.as_ref())
                )
            })?
        }
    };
    terminal.send(&digit)?;
    terminal.settle();
    let menu_keys = vec![digit];
    // ===== 段 3：Full Access 二阶段确认 =====
    let mut confirm_done = false;
    if needs_full_access_confirm("codex", ModeGroupId::Permission, target) {
        // 步骤②→③ 同样硬性 ≥0.5s（用户指令）
        wait_floor();
        if let Some(cluster) = poll_confirm()? {
            let affirmative = cluster.iter().find(|o| {
                o.label
                    .to_lowercase()
                    .contains(FULL_ACCESS_AFFIRMATIVE_KEYWORD)
            });
            let Some(o) = affirmative else {
                return Err(
                    "codex Full Access 确认框肯定项定位丢失——已中止，未发确认键；请人工核对终端（Full Access 可能未生效）"
                        .to_string(),
                );
            };
            terminal.send(&o.number.to_string())?;
            terminal.settle();
            confirm_done = true;
        } else {
            log::debug!("codex 权限切换：未出现 Full Access 二次确认框（可能用户已关闭该警告）");
        }
    }
    // ===== 段 4：成功回执核验 =====
    let receipt_seen = poll_receipt()?.is_some();
    Ok(MenuStagesOutcome {
        menu_keys,
        confirm_done,
        receipt_seen: Some(receipt_seen),
    })
}

/// **codex 斜杠命令注入前的公共前置**（composer 在场性 + 残留 overlay 清场 + 输入行
/// 纯净）——[`run_codex_permission_stages`]（旧自动路径）与 [`run_codex_menu_open`]
/// （picker 路径）**共用**，同一判据单一实现。
///
/// 段 0（G1，2026-10-10 诊断批）：composer 行**不在场**（= 有 overlay 占位）——
/// 审批框在屏（账本快筛命中）→ **直接 Err 零投递**（连 esc 都不发：esc 在审批框上
/// = 拒绝待审批命令，另一极副作用）；其余未知 overlay → `esc` + **条件等待 composer
/// 回归**；窗尽 → 如实中止（不盲发任何键）。
/// 段 0·续：屏上有残留 overlay（菜单/确认框）→ `esc` + **条件等待锚消失**（固定睡不
/// 可靠）；清不掉 → 如实中止（不盲发任何键）。
/// 段 0.5：输入行有残留 → `backspace` 逐字符清 + **闭环屏读复核**；清不净 → 如实中止。
///
/// 返回清场后最后读到的一屏（供调用方续用，省一次屏读）。
fn codex_preflight<T: MenuTerminal>(terminal: &mut T) -> Result<Option<Vec<String>>, String> {
    let mut latest = terminal.read();
    // ===== 段 0（G1，2026-10-10 诊断批）：composer 在场性通用闸 =====
    //
    // 诊断定案（mpd 证据批）：「/permissions+回车」落在**审批框在场**的终端上 →
    // 文本被 overlay 吞、**回车 = 批准了待审批命令**（真实副作用：curl 被执行）→
    // 菜单不开，数字直达报「菜单未出现」+ composer 干净（兜底不触发）。旧 preflight
    // 只认权限菜单/FA 确认框两种**有锚**残留——审批框/codex 问题框等 overlay 不可见。
    // 本闸改用**通用形态判据**（composer 行不在场 = 有 overlay 占位，
    // 见 [`codex_composer_present`]），不依赖逐 overlay 锚：
    // - **审批框在屏**（[`codex_approval_box_present`] 快筛命中）→ 直接 Err **零投递**
    //   （回车会误批准；esc 会拒绝——两种键都有副作用，处置权交用户，文案点名）；
    // - 其余未知 overlay → `esc` + 条件等待 composer 回归（RESIDUE_CLEAR_MAX_READS
    //   节奏，与残留清场同款）；回归 → 继续；窗尽 → Err 零投递。
    if let Some(lines) = latest.as_ref() {
        if !codex_composer_present(lines) {
            let lowered: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();
            if codex_approval_box_present(&lowered) {
                return Err(
                    "终端有待审批命令在场（命令审批框）——已中止，未发任何键；此时回车会误批准待审批命令、esc 会拒绝它——请先在终端处置，再重试权限切换"
                        .to_string(),
                );
            }
            // 锚漂移自报（评审 Minor：G1 未知 overlay 附 miss_report）——「权限菜单/
            // 确认框的账本锚为何没命中」直接给出候选词形与本机版本，锚漂移时定位
            // 从两轮走查缩到一次 grep（anchor_ledger::miss_report 的立项初衷）
            log::warn!(
                "codex：composer 行不在场（未知 overlay 占位）——权限菜单锚自报：{}；确认框锚自报：{} → esc 清场并等 composer 回归",
                crate::inject::anchor_ledger::miss_report(
                    "codex",
                    crate::inject::anchor_ledger::scenario::PERMISSION_MENU,
                    crate::inject::anchor_ledger::slot::TITLE,
                    crate::inject::approve::cached_cli_version("codex").as_deref(),
                ),
                crate::inject::anchor_ledger::miss_report(
                    "codex",
                    crate::inject::anchor_ledger::scenario::APPROVE_COMMAND,
                    crate::inject::anchor_ledger::slot::TITLE,
                    crate::inject::approve::cached_cli_version("codex").as_deref(),
                ),
            );
            terminal.send("esc")?;
            terminal.settle();
            let mut returned = false;
            for _ in 0..RESIDUE_CLEAR_MAX_READS {
                match terminal.read() {
                    Some(l) => {
                        let present = codex_composer_present(&l);
                        latest = Some(l);
                        if present {
                            returned = true;
                            break;
                        }
                        terminal.settle();
                    }
                    None => terminal.settle(),
                }
            }
            if !returned {
                return Err(
                    "终端有待处理交互（非权限菜单的对话框/审批框在场）——已中止，未发任何命令；请人工核对终端"
                        .to_string(),
                );
            }
            log::info!("codex：composer 行已回归 → 继续正常流程");
        }
    }
    if let Some(lines) = latest.as_ref() {
        if residual_overlay_present(lines) {
            log::debug!("codex：屏上已有残留 overlay（菜单/确认框）→ esc 清场");
            terminal.send("esc")?;
            terminal.settle();
            let mut cleared = false;
            for _ in 0..RESIDUE_CLEAR_MAX_READS {
                match terminal.read() {
                    Some(l) => {
                        let clean = !residual_overlay_present(&l);
                        latest = Some(l);
                        if clean {
                            cleared = true;
                            break;
                        }
                        terminal.settle();
                    }
                    None => terminal.settle(),
                }
            }
            if !cleared {
                return Err(
                    "codex 屏上残留的权限菜单/确认框按 esc 后仍未消失——不盲发任何键；请人工核对终端（手动按 esc 关闭后重试）"
                        .to_string(),
                );
            }
        }
    }
    if let Some(residue) = latest.as_ref().and_then(|l| composer_residue(l)) {
        log::debug!("codex：输入行残留 {residue} 字符 → backspace 清理后再发命令");
        for _ in 0..residue.min(COMPOSER_CLEAR_MAX_KEYS) {
            terminal.send("backspace")?;
        }
        terminal.settle();
        match terminal.read() {
            Some(ref l) if composer_residue(l).is_none() => {
                latest = Some(l.clone());
            }
            _ => {
                return Err(
                    "codex 输入行残留清理后仍不纯净（或读不到屏复核）——不盲发斜杠命令；请人工清空输入行后重试"
                        .to_string(),
                )
            }
        }
    }
    Ok(latest)
}

/// **picker 第一动作：打开菜单并读回选项清单**（移动端「终端菜单单选题」的数据源）。
///
/// 流程 = [`codex_preflight`] → `open_menu()`（`/permissions` + enter）→ `wait_floor()`
/// （与旧路径同一条 ≥0.5s 硬控，见 [`crate::inject::timing::MODE_STEP_MIN_GAP_MS`]）
/// → `read_options()` 轮询到菜单画全。
///
/// **与旧自动路径的关键差别**：本函数**不挑档、不敲数字**——它把屏上选项原样交给
/// 调用方（进而交给用户点）。「用户自己选」正是 2026-09-23 用户方案的要点：兔维斯 不再
/// 猜「该敲哪个数」，只负责读屏与投递用户点选的那个数。
pub fn run_codex_menu_open<T, O, P, W>(
    mut open_menu: O,
    mut read_options: P,
    mut wait_floor: W,
    terminal: &mut T,
) -> Result<Vec<DialogOption>, String>
where
    O: FnMut() -> Result<(), String>,
    P: FnMut() -> Result<Option<Vec<DialogOption>>, String>,
    W: FnMut(),
    T: MenuTerminal,
{
    codex_preflight(terminal)?;
    open_menu()?;
    wait_floor();
    match read_options()? {
        Some(opts) if !opts.is_empty() => Ok(opts),
        // 窗尽失败同样做 G2 hijack 对账（2026-10-10 诊断批 E5：菜单/审批框在场时
        // 第二次 /permissions+回车 = 字符被吞、回车选在高亮行——若落在审批框上，
        // 回执里的审批结果行在此点名）
        _ => {
            let last = terminal.read();
            Err(hijack_reconciled_error(
                "codex 的权限菜单未出现或读不到档位表——请人工核对终端（命令已发送，档位未切）"
                    .to_string(),
                last.as_ref(),
            ))
        }
    }
}

/// **picker 第二动作：按用户点选的屏上编号敲键**（无回车——实测数字键一次直达）。
///
/// # 硬前置（**不盲发**，本批的红线）
///
/// 敲键前**必须**屏读确认「菜单或确认框确实在屏」：
/// - 都不在屏 → `Err`（零投递：用户看到的可能是过期面板，敲进去会落到输入行）；
/// - 在屏 → 发该数字键；随后再读屏：
///   - **确认框在屏**（菜单阶段点了 Full Access）→ 交回调用方
///     （[`MenuPick::Confirm`]，二阶段由用户点）；
///   - 否则 → 回执核验（[`MenuPick::Done`]）。
///
/// # 确认框**阶段**（0.160.0 实测，T4-F4 修复；spec §3.2 Full Access 分叉）
///
/// 旧版把「确认框在屏」一律零投递拒回（二阶段第二跳死路）；现在**确认框在屏时的
/// 数字键按确认框阶段投递**：
/// - `number` ∈ {1, 2}（确认框只有 Yes/Cancel 两项，**越界零投递**——菜单编号
///   混进确认框是面板过期，敲 3/4 会落空或误触）；发键后确认框消失 →
///   [`MenuPick::Done`]（Yes 路径，回执核验交调用方）；确认框仍在 →
///   [`MenuPick::Confirm`]（键被吞/Cancel 回菜单读不到簇时如实交回用户）。
///
/// 本段**绝不做 esc 清场**（overlay 在屏正是本次操作对象，不是"残留"——这正是它
/// 不能走 [`codex_preflight`] 的原因）。
pub fn run_codex_menu_pick<T, R, Q, W>(
    number: u32,
    mut read_screen: R,
    mut poll_confirm: Q,
    mut wait_floor: W,
    terminal: &mut T,
) -> Result<MenuPick, String>
where
    R: FnMut() -> Option<Vec<String>>,
    Q: FnMut() -> Result<Option<Vec<DialogOption>>, String>,
    W: FnMut(),
    T: MenuTerminal,
{
    if !(1..=9).contains(&number) {
        return Err(format!(
            "屏上编号 {number} 超出数字键域（1-9）——零投递（面板数据可能过期，请重新读取）"
        ));
    }
    // 硬前置：屏上确有 overlay 才动手
    let Some(lines) = read_screen() else {
        return Err("读不到终端屏面——零投递（请人工核对终端；或点「重新读取」）".to_string());
    };
    let lowered: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();
    let Some(kind) = codex_overlay_kind(&lowered) else {
        return Err(
            "屏上没有权限菜单或确认框——零投递（面板数据可能已过期，请点「重新读取」）".to_string(),
        );
    };
    if kind == CodexOverlay::FullAccessConfirm {
        // 确认框阶段（T4-F4：旧版在此一律零投递——二阶段第二跳死路）：
        // 只认确认框选项域 1/2；越界 = 面板过期（菜单编号混进确认框）零投递
        if !matches!(number, 1 | 2) {
            return Err(format!(
                "Full Access 确认框只有 1（Yes）/ 2（Cancel）两项，屏上编号 {number} 越界——零投递（面板数据可能已过期，请重新读取）"
            ));
        }
        terminal.send(&number.to_string())?;
        terminal.settle();
        // 步骤硬性 ≥0.5s（与菜单阶段同一条）
        wait_floor();
        // 判走向：确认框消失 → 走 Done（Yes 生效，回执核验交调用方）；仍在 → 键被吞
        // （或 Cancel 回菜单但确认框探测窗读数滞后）——如实交回调用方
        let confirm_gone = match read_screen() {
            Some(l) => {
                let low: Vec<String> = l.iter().map(|x| x.to_lowercase()).collect();
                codex_overlay_kind(&low) != Some(CodexOverlay::FullAccessConfirm)
            }
            None => false, // 读不到屏复核 = 无法确认已生效 → 保守交回用户
        };
        if confirm_gone {
            return Ok(MenuPick::Done {
                screen: Vec::new(),
                from_confirm_box: true,
            });
        }
        let cluster = poll_confirm()?.unwrap_or_default();
        return Ok(MenuPick::Confirm(cluster));
    }
    terminal.send(&number.to_string())?;
    terminal.settle();
    // 步骤②→③ 硬性 ≥0.5s（与旧路径同一条）
    wait_floor();
    // 读屏判走向：确认框在屏 → 交用户点；否则做回执核验
    if let Some(cluster) = poll_confirm()? {
        return Ok(MenuPick::Confirm(cluster));
    }
    match read_screen() {
        Some(l) => {
            let low: Vec<String> = l.iter().map(|x| x.to_lowercase()).collect();
            if codex_overlay_kind(&low) == Some(CodexOverlay::FullAccessConfirm) {
                return Ok(MenuPick::Confirm(Vec::new()));
            }
            Ok(MenuPick::Done {
                screen: l,
                from_confirm_box: false,
            })
        }
        None => Ok(MenuPick::Done {
            screen: Vec::new(),
            from_confirm_box: false,
        }),
    }
}

/// [`run_codex_menu_pick`] 的走向。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuPick {
    /// 敲键后屏上出现二次确认框（Full Access）——选项表交用户点；空表 = 读不到选项
    /// （前端提示「请重新读取」）。确认框阶段发键后被吞时也走本走向（如实交回用户）。
    Confirm(Vec<DialogOption>),
    /// 已投递且无确认框；`screen` = 敲键后的屏（供回执核验）。确认框阶段发键且
    /// 确认框消失也走本走向（Yes 生效；`screen` 为空集——回执核验由调用方重读）。
    /// `from_confirm_box` = 本次键发在**确认框阶段**（api.rs 回执判向用：Yes/Cancel
    /// 语义不同——Yes 走核验回执，Cancel 可能只是「回菜单」而非完成切换）。
    Done {
        screen: Vec<String>,
        from_confirm_box: bool,
    },
}
///
/// 依据 = 实机取证档案 §3 + 用户 2026-09-23 实机走查：切 1/2/3 档**不出现**确认框，
/// 直接回 `• Permissions updated to …`；只有切到第 4 档（Full Access）才有
/// `Enable full access?`（按 1 = Yes, continue anyway 才生效）。
pub fn needs_full_access_confirm(tool: &str, group: ModeGroupId, target: MamMode) -> bool {
    tool == "codex" && group == ModeGroupId::Permission && target == MamMode::Bypass
}

/// codex **0.160.0 新锚词形**（回执改印 `• Permission selection requested: <标签>`
/// ——底料 §1 实打）。P1-1 终审：0.154 锚 `permissions updated to` 在 0.160.0
/// 恒缺席，回执核验若只认账本首行旧锚则恒 verified=false（段 4 回执自证全灭）。
/// 消费侧判据 = 双代词形（本常量 + 旧锚）。P2-1 起词形**单点**在账本
/// （[`crate::inject::anchor_ledger::CODEX_PERMISSION_RECEIPT_ANCHOR`]——账本
/// RECEIPT 新行 `text` 与本消费侧引用同一字符串，改词形只动账本一处），此处
/// 同名 re-export 供锚步 / 回执核验 / 绑定测试使用（不另立第二份词形真源）。
pub(crate) use crate::inject::anchor_ledger::CODEX_PERMISSION_RECEIPT_ANCHOR;

/// 工具的**旧代/现行**成功回执行锚（实机原文逐字；真源经账本，见
/// [`crate::inject::anchor_ledger`] 的 `slot::RECEIPT` 行）：
/// - codex：`• Permissions updated to Full Access`（档案 §3，1/2/3 与 4 档同形）
///   ——**0.154 旧代词形**，0.160.0 起改印新词形（见
///   [`CODEX_PERMISSION_RECEIPT_ANCHOR`]），旧行保留（旧版本终端兼容 + 账本
///   append-only）；
/// - kimi：`Permission mode: Always Ask`（档案 §5.1–5.3）——现行词形。
///
/// 返回 `candidates().first()`（账本序），未入账的工具 → `None`
/// （[`permission_receipt_verified`] 据此对旧词形不出手）。
fn permission_receipt_anchor(tool: &str) -> Option<&'static str> {
    crate::inject::anchor_ledger::candidates(
        tool,
        crate::inject::anchor_ledger::scenario::PERMISSION_MENU,
        crate::inject::anchor_ledger::slot::RECEIPT,
    )
    .first()
    .map(|r| r.text)
}

/// **成功回执核验**（纯函数，可测）：屏读行集里是否出现「成功切到**目标档**」的
/// 回执行。判据 = **双代词形**（P1-1 终审修复，spec §3.2）：
/// - **新词形（codex 0.160.0）**：行含 `permission selection requested` 且**冒号后
///   标签段**前缀匹配目标档标签（与 [`parse_codex_permission_event_line`] 的匹配
///   步同纪律——`ask for approval` 覆盖全形回执行）；
/// - **旧词形（codex 0.154 / kimi 现行）**：行含该工具账本锚
///   （[`permission_receipt_anchor`]）且含目标档标签（大小写不敏感）。
///
/// **为什么不能只找锚**：菜单本身与说明文都可能含档位词，而**别的档**的旧回执行会
/// 在屏上停留（用户先前切过档）——只认锚会把旧回执当成本次成功（正是「不假装成功」
/// 要防的谎报）。锚 + 目标档标签两者同在，才是「本次确实切到了目标档」的证据。
///
/// **为什么不能只认旧锚**（本函数曾经的回归面）：0.160.0 的回执不再印
/// `permissions updated to`——单锚判据在新版终端上恒 verified=false，菜单路径段 4
/// 的「工具自证」证据链全灭（`receipt_and_verdict` 的一票之力失效）。
///
/// 未实测工具 → 旧词形 `false`（不出手）；新词形 gate 在 codex（其它工具的屏面
/// 不出现该英文词形，gate 工具出身让「未实测不出手」契约保持可读）。**返回 false
/// 不等于失败**：回执可能被后续输出刷走，或用户此前关过该警告——调用方据此下发
/// 「请人工核对」，见 [`crate::remote::api::session_mode_switch`]。
///
/// **空标签必须短路**（本函数唯一的失败开保险）：`contains("")` 恒真，调用方若因任何
/// 原因传进空标签（如未实测工具的 `menu_target_label` 返回 `None` 后被 `unwrap_or("")`
/// 兜底），判据就退化成「只找锚」= **旧回执冒充本次成功**——正是上面那段要防的谎报。
/// 故此处显式短路（这条与 `menu_target_label` 的调用方兜底**成对**：任一侧改松都不会
/// 静默放行）。
pub fn permission_receipt_verified(tool: &str, lines: &[String], target_label: &str) -> bool {
    if target_label.trim().is_empty() {
        return false; // 见文档「空标签必须短路」
    }
    let want = target_label.to_lowercase();
    lines.iter().any(|l| {
        let lower = l.to_lowercase();
        // 新词形（codex 0.160.0）：锚在 + 冒号后标签段前缀匹配目标标签
        if tool == "codex"
            && lower.contains(CODEX_PERMISSION_RECEIPT_ANCHOR)
            && lower
                .split_once(':')
                .is_some_and(|(_, label)| label.trim().starts_with(want.as_str()))
        {
            return true;
        }
        // 旧词形（codex 0.154 / kimi 现行）：锚 + 目标标签同在
        permission_receipt_anchor(tool)
            .is_some_and(|anchor| lower.contains(anchor) && lower.contains(want.as_str()))
    })
}

/// **成功回执行**原文提取（picker 用）：屏上出现该工具的**任一代**回执锚即返回
/// **该行原文**（未出现 → `None`）。
///
/// # 与 [`permission_receipt_verified`] 的差别（为什么两个都要）
///
/// 后者要求「锚 + **目标档标签**」同在——因为它的场景里 兔维斯 自己知道目标档（旧自动
/// 路径按 wire 档敲）。picker 场景**目标档未知**：用户点的是**屏上编号**（1..N），
/// 后端不知道它对应哪个 wire 档（档位集合随 Guardian 配置变化，用户也可能点了本机
/// codex 新加的档）。故 picker 只能判「有没有成功回执行」，并把**原文**回给前端显示
/// （用户自己看得见切到了哪档）——这是「如实」，不是「假装知道」。
///
/// P1-1 终审：判据同样**双代词形**（0.154 锚在 0.160.0 恒缺席——单锚兼容通道在
/// 新版终端永不命中，picker 的「锚在屏」兜底失效）。
pub fn permission_receipt_seen(tool: &str, lines: &[String]) -> Option<String> {
    lines
        .iter()
        .find(|l| {
            let lower = l.to_lowercase();
            // 新词形（codex 0.160.0）：选择事件行即「有回执」——目标档未知场景，
            // 档名交前端原文显示
            (tool == "codex" && lower.contains(CODEX_PERMISSION_RECEIPT_ANCHOR))
                || permission_receipt_anchor(tool).is_some_and(|a| lower.contains(a))
        })
        .map(|l| l.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// **G1 起的最小「干净 idle 屏」**（2026-10-10 诊断批）：真实主屏的 composer
    /// 占位行恒在（mpd-mpd-e5-final 行 28 活体），G1 闸以「composer 行不在场」判
    /// overlay 占位——最小屏必须带上该行，否则闸把干净屏误判成 overlay（旧的单行
    /// footer 屏只配得上「判据不可得」，过不了在场性判）。
    fn clean_idle() -> Vec<String> {
        lines(&[
            "› Ask Codex to do anything",
            "  glm-5.3-flash medium · ~\\proj-codex",
        ])
    }

    // ==== E1 插队键序路由 ====

    /// **E1 四家插队键序路由锁**（词典 §6 busy 态行/裁16/裁19 的代码面）：
    /// claude=Esc 前置 / codex=草稿→Tab→Esc / opencode=Esc 打断直插 / kimi=排队制。
    /// [`supports_interrupt`] 与路由同源（QueueOnly 之外的均为「需要 Esc 类控制键」）。
    /// 还原动作（变异）：任一格改回 E1 前旧值（supports_interrupt 仅 claude）→ 先红。
    #[test]
    fn jump_sequence_routes_four_tools() {
        assert_eq!(
            jump_sequence("claude"),
            JumpSequence::EscInterruptThenText,
            "claude：Esc×1 中断→等回合停（含撤回防护）→正文"
        );
        assert_eq!(
            jump_sequence("codex"),
            JumpSequence::DraftTabThenEsc,
            "codex：打字→Tab 入队→Esc×1 直插（用户终裁首选）"
        );
        assert_eq!(
            jump_sequence("opencode"),
            JumpSequence::EscThenText,
            "opencode：Esc 打断+直插；Ctrl+C 禁注（裁19）"
        );
        assert_eq!(
            jump_sequence("kimi"),
            JumpSequence::QueueOnly,
            "kimi：busy 直接投递=排队制，不打控制键；Ctrl+S=条件项未复验不上"
        );
        // 未知工具保守归排队制（无键序证据不出手）
        assert_eq!(jump_sequence("workbuddy"), JumpSequence::QueueOnly);
        // supports_interrupt 与路由同源：三家 true（kimi 恒 false）
        assert!(supports_interrupt("claude"));
        assert!(supports_interrupt("codex"));
        assert!(supports_interrupt("opencode"));
        assert!(!supports_interrupt("kimi"));
        assert!(!supports_interrupt("workbuddy"));
    }

    // ==== 统一枚举 ====

    /// 统一枚举往返（wire ↔ 档）+ 别名词容忍
    #[test]
    fn mode_parse_roundtrip() {
        for m in MamMode::ALL {
            assert_eq!(MamMode::parse(m.wire()), Some(m), "wire 往返: {m:?}");
        }
        assert_eq!(MamMode::parse("accept_edits"), Some(MamMode::AcceptEdits));
        assert_eq!(MamMode::parse("read_only"), Some(MamMode::ReadOnly));
        assert_eq!(
            MamMode::parse("yolo"),
            None,
            "happy 的 yolo 在 兔维斯 收敛为 Bypass 单一档"
        );
        assert_eq!(MamMode::parse("bogus"), None);
    }

    /// **裁7 的输入面锁**：codex 退役旧档（untrusted / on-failure）**不可解析**——
    /// 不是「前端不渲染」的自律，而是后端的 POST 入口根本不认（→ 400）。
    /// 还原动作：给 `MamMode::parse` 加 `"untrusted"` 分支 → 本断言先红。
    #[test]
    fn mode_legacy_enum_is_not_selectable() {
        for legacy in ["untrusted", "on-failure", "onFailure", "untrusted_approval"] {
            assert_eq!(
                MamMode::parse(legacy),
                None,
                "退役档不得成为可选档（裁7）：{legacy}"
            );
        }
        // 且它们**不在**任何组的 tiers 里（结构面同样不得出现）
        for tool in ["codex", "kimi", "claude", "opencode"] {
            for g in mode_structure(tool).groups() {
                for t in g.tiers {
                    assert!(
                        !t.label.eq_ignore_ascii_case("untrusted")
                            && !t.label.eq_ignore_ascii_case("on-failure"),
                        "{tool} 的 {} 组 tiers 混入退役档 {}",
                        g.id.wire(),
                        t.label
                    );
                }
            }
        }
        // 只在 codex 权限组的 legacy 清单里如实登记
        let codex = mode_structure("codex");
        let perm = codex.group(ModeGroupId::Permission).unwrap();
        let names: Vec<&str> = perm.legacy.iter().map(|l| l.label).collect();
        assert_eq!(names, vec!["untrusted", "on-failure"]);
        assert!(
            perm.legacy.iter().all(|l| !l.note.is_empty()),
            "legacy 档必须带退役依据（如实申报）"
        );
        // 其余组不带 legacy
        assert!(codex.group(ModeGroupId::Mode).unwrap().legacy.is_empty());
        assert!(mode_structure("kimi")
            .groups()
            .iter()
            .all(|g| g.legacy.is_empty()));
    }

    /// 机制分族：claude/opencode 步进；codex/kimi 命令；其余不支持
    #[test]
    fn switch_kind_routing() {
        for t in ["claude", "opencode"] {
            assert_eq!(switch_kind(t), ModeSwitchKind::ShiftTabCycle, "{t}");
        }
        for t in ["codex", "kimi"] {
            assert_eq!(switch_kind(t), ModeSwitchKind::SlashCommand, "{t}");
        }
        for t in ["zcode", "dsh", "workbuddy", "openclaw", ""] {
            assert_eq!(
                switch_kind(t),
                ModeSwitchKind::Unsupported,
                "{t} 未实测 → 不出手"
            );
        }
    }

    /// 回读四家全开（批次丙台账开放项 3 的收口）；未实测工具仍 false
    #[test]
    fn readback_open_for_four_tools() {
        for t in ["claude", "opencode", "codex", "kimi"] {
            assert!(
                mode_readback_supported(t),
                "{t} 底栏有实测档位文本 → 放开回读"
            );
        }
        for t in ["zcode", "dsh", "workbuddy"] {
            assert!(!mode_readback_supported(t), "{t} 未实测 → 不回读");
        }
    }

    // ==== 结构表（§2.6 逐格）====

    /// §2.6 规格表逐格：结构 / 档位标签 / 顺序
    #[test]
    fn structure_matches_spec_table() {
        // codex：两组（模式组 操作/计划 = shift+tab toggle；权限组 只读/默认/完全信任）
        let codex = mode_structure("codex");
        assert_eq!(codex.wire(), "twoAxis");
        let m = codex.group(ModeGroupId::Mode).unwrap();
        assert_eq!(m.label, "模式");
        assert!(
            !m.step,
            "codex 模式组非步进：单钮 toggle，目标档由前端按 current 翻转"
        );
        assert_eq!(
            m.layout,
            GroupLayout::Toggle,
            "codex 模式组=单钮 toggle（2026-09-23 用户实测裁决）"
        );
        assert!(
            m.tiers.iter().all(|t| t.selectable),
            "操作/计划两档全部可选（shift+tab 双向）"
        );
        let labels: Vec<&str> = m.tiers.iter().map(|t| t.label).collect();
        assert_eq!(labels, vec!["操作", "计划"]);
        let p = codex.group(ModeGroupId::Permission).unwrap();
        assert_eq!(p.label, "权限");
        assert_eq!(
            p.layout,
            GroupLayout::Picker,
            "权限组是**单选面板**（2026-09-23 用户方案：兔维斯 读回终端菜单选项，用户点选哪项就敲哪个数字）\
             ——不再是逐档按钮（那需要前端硬编码「哪档对应哪个数字」，而档位编号随 Guardian 配置前移）"
        );
        let labels: Vec<&str> = p.tiers.iter().map(|t| t.label).collect();
        assert_eq!(labels, vec!["只读", "默认", "自动审批", "完全信任"]);
        assert_eq!(
            p.tiers.iter().map(|t| t.mode).collect::<Vec<_>>(),
            vec![
                MamMode::ReadOnly,
                MamMode::Default,
                MamMode::AcceptEdits,
                MamMode::Bypass
            ],
            "2026-09-23 起四档（第 3 档 = 实机 Approve for me，数字键 3 直达）"
        );

        // kimi：两组（模式组 默认/计划；权限组 总是询问/按需询问/永不询问）
        let kimi = mode_structure("kimi");
        assert_eq!(kimi.wire(), "twoAxis");
        let m = kimi.group(ModeGroupId::Mode).unwrap();
        assert_eq!(
            m.tiers.iter().map(|t| t.label).collect::<Vec<_>>(),
            vec!["默认", "计划"]
        );
        let p = kimi.group(ModeGroupId::Permission).unwrap();
        assert_eq!(
            p.tiers.iter().map(|t| t.label).collect::<Vec<_>>(),
            vec!["总是询问", "按需询问", "永不询问"],
            "§2.6 kimi 权限组屏显标签（工具自己的词，不是 兔维斯 通用名）"
        );
        // kimi 权限三档 → 兔维斯 映射（模块文档表的机器可验版本）
        assert_eq!(p.tiers[0].mode, MamMode::Default, "总是询问 = 每步询问");
        assert_eq!(
            p.tiers[1].mode,
            MamMode::AcceptEdits,
            "按需询问 = 常规改动/命令自动、高危仍问 → 兔维斯 唯一的「常规改动自动」档"
        );
        assert_eq!(p.tiers[2].mode, MamMode::Bypass, "永不询问 = 不再询问");
        assert!(
            p.tiers.iter().all(|t| t.selectable),
            "kimi 权限三档全部可选（总是询问走 /permission 两段式；其余两档有直达变体）"
        );

        // claude：单轴四档，**顺序 = 实测环序**（非 MamMode::ALL 的枚举序）
        let claude = mode_structure("claude");
        assert_eq!(claude.wire(), "singleAxis");
        let axis = claude.group(ModeGroupId::Mode).unwrap();
        assert!(axis.step, "shift+tab 一步一档");
        assert_eq!(
            axis.tiers.iter().map(|t| t.mode).collect::<Vec<_>>(),
            vec![
                MamMode::AcceptEdits,
                MamMode::Plan,
                MamMode::Bypass,
                MamMode::Default
            ],
            "实测环序 acceptEdits → plan → auto → manual"
        );

        // opencode：单轴两档（Build 已按裁6 改显「默认」）
        let oc = mode_structure("opencode");
        assert_eq!(oc.wire(), "singleAxis");
        assert_eq!(
            oc.group(ModeGroupId::Mode)
                .unwrap()
                .tiers
                .iter()
                .map(|t| t.label)
                .collect::<Vec<_>>(),
            vec!["默认", "计划"]
        );

        // 未实测工具：无结构
        assert_eq!(mode_structure("zcode").wire(), "none");
        assert!(mode_structure("zcode").groups().is_empty());
    }

    /// 二维家的**组存在性**：claude/opencode 没有权限组（单轴）；codex/kimi 有
    #[test]
    fn group_lookup_by_structure() {
        assert!(mode_structure("claude")
            .group(ModeGroupId::Permission)
            .is_none());
        assert!(mode_structure("opencode")
            .group(ModeGroupId::Permission)
            .is_none());
        assert!(mode_structure("codex")
            .group(ModeGroupId::Permission)
            .is_some());
        assert!(mode_structure("kimi")
            .group(ModeGroupId::Permission)
            .is_some());
    }

    /// 组 wire 词往返 + 标签
    #[test]
    fn group_id_wire_roundtrip() {
        for g in [ModeGroupId::Mode, ModeGroupId::Permission] {
            assert_eq!(ModeGroupId::parse(g.wire()), Some(g));
            assert!(!g.label().is_empty());
        }
        assert_eq!(ModeGroupId::parse("permissions"), None);
        assert_eq!(ModeGroupId::parse(""), None);
    }

    // ==== 切换序列（§2.6 切换机制列）====

    /// claude/opencode：一步 shift+tab（目标档不参与按键构造）
    #[test]
    fn plan_step_axis_is_one_shift_tab() {
        for t in ["claude", "opencode"] {
            for target in [MamMode::Plan, MamMode::Default, MamMode::AcceptEdits] {
                assert_eq!(
                    mode_switch_plan(t, ModeGroupId::Mode, target).unwrap(),
                    ModeSwitchPlan::Key("shift+tab"),
                    "{t} 循环一步"
                );
            }
        }
    }

    /// codex 模式组：**双向 shift+tab toggle**（2026-09-23 用户实机走查裁决——
    /// 裁20「用户实测优先」：shift+tab 在 计划/操作 间双向循环，实测可切。
    /// 覆盖 §2.6 旧「仅 `/plan` 单向、退出无实测命令」的保守裁决；词典更新在台账
    /// 「codex 模式切换改造」节登记）。
    #[test]
    fn codex_mode_group_is_shift_tab_toggle() {
        for target in [MamMode::Plan, MamMode::Default] {
            assert_eq!(
                mode_switch_plan("codex", ModeGroupId::Mode, target).unwrap(),
                ModeSwitchPlan::Key("shift+tab"),
                "codex 模式组双向 toggle（shift+tab），目标档不参与按键构造"
            );
        }
    }

    /// codex 权限组：**四档**都走 `/permissions` 两段式（2026-09-23 起含第 3 档
    /// 「自动审批」=`Approve for me`，用户实测数字直达）；非四档 → 拒绝
    #[test]
    fn codex_permission_is_two_stage_menu() {
        for (target, _label) in [
            (MamMode::ReadOnly, "Read Only"),
            (MamMode::Default, "Ask for approval"),
            (MamMode::AcceptEdits, "Approve for me"),
            (MamMode::Bypass, "Full Access"),
        ] {
            assert_eq!(
                mode_switch_plan("codex", ModeGroupId::Permission, target).unwrap(),
                ModeSwitchPlan::Menu {
                    open: "/permissions",
                    target
                }
            );
        }
        assert!(matches!(
            mode_switch_plan("codex", ModeGroupId::Permission, MamMode::Plan),
            Err(SwitchRefusal::NoMechanism(_))
        ));
    }

    /// kimi：模式组 `/plan on|off` 直达；权限组三档全部两段式（2026-10-06：命令只开
    /// 菜单并预选，屏读确认高亮后才回车——`/permission`/`/yolo`/`/auto` 各自开菜单）
    #[test]
    fn kimi_switch_mechanisms() {
        assert_eq!(
            mode_switch_plan("kimi", ModeGroupId::Mode, MamMode::Plan).unwrap(),
            ModeSwitchPlan::Text("/plan on")
        );
        assert_eq!(
            mode_switch_plan("kimi", ModeGroupId::Mode, MamMode::Default).unwrap(),
            ModeSwitchPlan::Text("/plan off")
        );
        assert_eq!(
            mode_switch_plan("kimi", ModeGroupId::Permission, MamMode::Default).unwrap(),
            ModeSwitchPlan::Menu {
                open: "/permission",
                target: MamMode::Default
            },
            "「总是询问」→ /permission 开菜单"
        );
        assert_eq!(
            mode_switch_plan("kimi", ModeGroupId::Permission, MamMode::AcceptEdits).unwrap(),
            ModeSwitchPlan::Menu {
                open: "/yolo",
                target: MamMode::AcceptEdits
            },
            "「按需询问」→ /yolo 开菜单并预选（不再盲发确认回车）"
        );
        assert_eq!(
            mode_switch_plan("kimi", ModeGroupId::Permission, MamMode::Bypass).unwrap(),
            ModeSwitchPlan::Menu {
                open: "/auto",
                target: MamMode::Bypass
            },
            "「永不询问」→ /auto 开菜单并预选（不再盲发确认回车）"
        );
    }

    /// **两段式判据**（T4 复评 I2 的单点）：只有 Menu 变体为真——回执的「屏读推算」
    /// 限定由它驱动（端点用本方法，不自己写 matches!）。
    /// 还原动作：把 `is_two_stage` 改成恒 true → `two_stage_receipt_*` 的成对锁先红。
    #[test]
    fn two_stage_flag_is_menu_only() {
        assert!(ModeSwitchPlan::Menu {
            open: "/permissions",
            target: MamMode::Bypass
        }
        .is_two_stage());
        assert!(!ModeSwitchPlan::Key("shift+tab").is_two_stage());
        assert!(!ModeSwitchPlan::Text("/plan on").is_two_stage());
    }

    /// 未实测工具 → Unsupported（端点 409 no_mechanism）
    #[test]
    fn unsupported_tool_refuses() {
        assert_eq!(
            mode_switch_plan("zcode", ModeGroupId::Mode, MamMode::Plan).unwrap_err(),
            SwitchRefusal::Unsupported
        );
        assert_eq!(
            mode_switch_plan("claude", ModeGroupId::Permission, MamMode::Plan).unwrap_err(),
            SwitchRefusal::Unsupported,
            "单轴工具没有权限组"
        );
    }

    /// 组推断（旧客户端不带 group）：唯一组 / 歧义取可选组 / 两组可选取模式组 / 无组
    #[test]
    fn group_inference_rules() {
        // 唯一组：claude 的 acceptEdits 只在单轴里
        assert_eq!(
            resolve_group("claude", None, MamMode::AcceptEdits),
            Ok(ModeGroupId::Mode)
        );
        // codex plan：只在模式组
        assert_eq!(
            resolve_group("codex", None, MamMode::Plan),
            Ok(ModeGroupId::Mode)
        );
        // codex bypass：只在权限组
        assert_eq!(
            resolve_group("codex", None, MamMode::Bypass),
            Ok(ModeGroupId::Permission)
        );
        // codex default：两组都有且**都可选**（2026-09-23 起 Default = shift+tab
        // toggle 档）→ 模式组（与 kimi default 同规则：两组可选取模式组）
        assert_eq!(
            resolve_group("codex", None, MamMode::Default),
            Ok(ModeGroupId::Mode),
            "codex Default 可选化后歧义消解与 kimi 同规：两组可选取模式组"
        );
        // kimi default：两组都有且**都可选** → 模式组（旧 shift+tab 语义 = 切计划档）
        assert_eq!(
            resolve_group("kimi", None, MamMode::Default),
            Ok(ModeGroupId::Mode)
        );
        // 显式 group 优先，且必须在结构里存在
        assert_eq!(
            resolve_group("kimi", Some(ModeGroupId::Permission), MamMode::Default),
            Ok(ModeGroupId::Permission)
        );
        assert_eq!(
            resolve_group("claude", Some(ModeGroupId::Permission), MamMode::Default),
            Err(SwitchRefusal::Unsupported),
            "单轴工具没有权限组 → 拒绝"
        );
        // 结构里没有的档 → 拒绝
        assert_eq!(
            resolve_group("kimi", Some(ModeGroupId::Mode), MamMode::ReadOnly),
            Err(SwitchRefusal::Unsupported)
        );
    }

    // ==== codex 运行中门（§2.6 表末）====

    /// codex 模式组运行中门：**模式组任意档**都拦（shift+tab 运行中行为未实测，
    /// 保守沿用 `/plan` 时代的拦截面——拦截范围扩张在台账「codex 模式切换改造」
    /// 节登记）；权限组不受此门（裁17 + CX-1/2 实测 busy 可切权限）。
    #[test]
    fn codex_plan_busy_predicate() {
        let busy = true;
        for target in [MamMode::Plan, MamMode::Default] {
            assert!(codex_plan_busy("codex", ModeGroupId::Mode, target, busy));
            assert!(!codex_plan_busy("codex", ModeGroupId::Mode, target, false));
        }
        for target in [
            MamMode::ReadOnly,
            MamMode::Default,
            MamMode::AcceptEdits,
            MamMode::Bypass,
        ] {
            assert!(!codex_plan_busy(
                "codex",
                ModeGroupId::Permission,
                target,
                busy
            ));
        }
        // 其他家不套用 codex 的门（未实测）
        for t in ["claude", "kimi", "opencode"] {
            assert!(!codex_plan_busy(t, ModeGroupId::Mode, MamMode::Plan, busy));
        }
    }

    // ==== 环序与回读确认 ====

    /// claude 环序（实测四档循环）+ opencode 双档循环
    #[test]
    fn cycle_next_follows_measured_ring() {
        assert_eq!(
            cycle_next("claude", MamMode::AcceptEdits),
            Some(MamMode::Plan)
        );
        assert_eq!(cycle_next("claude", MamMode::Plan), Some(MamMode::Bypass));
        assert_eq!(
            cycle_next("claude", MamMode::Bypass),
            Some(MamMode::Default)
        );
        assert_eq!(
            cycle_next("claude", MamMode::Default),
            Some(MamMode::AcceptEdits),
            // 依据 = **底栏 shift+tab 的环序实测**（T6 探测档案 §1.2：逐次 shift+tab
            // 得到 acceptEdits → plan → auto → manual，第四次后又回到 acceptEdits）。
            // 注意：这与 dialog::navigation_sequence 的「↓ 到尾部回卷」**不是同一条
            // 证据**——那是 claude **编号对话框**的行选择器行为（R1 实测），此处用的是
            // 模式栏的 shift+tab 循环。两条实测结论恰好一致（都回卷），但证据来源不同。
            "环尾回到环首（依据 = T6 底栏 shift+tab 环序实测，非对话框 ↓ 回卷）"
        );
        assert_eq!(
            cycle_next("opencode", MamMode::Default),
            Some(MamMode::Plan)
        );
        assert_eq!(
            cycle_next("opencode", MamMode::Plan),
            Some(MamMode::Default)
        );
        // 非单轴组 / 未实测工具 → None
        assert_eq!(cycle_next("codex", MamMode::Plan), None);
        assert_eq!(cycle_next("zcode", MamMode::Plan), None);
    }

    /// 期望档：步进组用环序推算（当前档未知 → None）；直达组只有**有回读源**时才可预测
    #[test]
    fn expected_mode_rules() {
        // claude 步进：从 AcceptEdits 推 Plan
        assert_eq!(
            expected_mode_after(
                "claude",
                ModeGroupId::Mode,
                MamMode::Default,
                Some(MamMode::AcceptEdits)
            ),
            Some(MamMode::Plan),
            "目标档对步进组不参与预测——预测来自环序"
        );
        assert_eq!(
            expected_mode_after("claude", ModeGroupId::Mode, MamMode::Default, None),
            None,
            "当前档未知 → 无法预测（不造假 Mismatch）"
        );
        // codex `/plan`：直达 + 有回读源 → 期望 Plan
        assert_eq!(
            expected_mode_after(
                "codex",
                ModeGroupId::Mode,
                MamMode::Plan,
                Some(MamMode::Default)
            ),
            Some(MamMode::Plan)
        );
        // codex 权限组 **2026-10-09 起有回读源**（最新回执行——状态栏零显示，
        // 事件行即唯一屏读面，spec §3.3）→ Some（旧值 None 已过期，kimi 2.1.1 同款先例）
        assert_eq!(
            expected_mode_after("codex", ModeGroupId::Permission, MamMode::Bypass, None),
            Some(MamMode::Bypass)
        );
        // kimi 权限组 **2.1.1 起有回读源**（底栏权限标签）→ Some（旧值 None 已过期）
        assert_eq!(
            expected_mode_after("kimi", ModeGroupId::Permission, MamMode::Bypass, None),
            Some(MamMode::Bypass)
        );
        // kimi 模式组有回读源
        assert_eq!(
            expected_mode_after(
                "kimi",
                ModeGroupId::Mode,
                MamMode::Default,
                Some(MamMode::Plan)
            ),
            Some(MamMode::Default)
        );
    }

    /// 回读对账三态（裁5「回读确认」）：命中 / 不符（如实报两边）/ 无法判定
    #[test]
    fn verify_mode_three_states() {
        assert_eq!(
            verify_mode_switch(Some(MamMode::Plan), Some(MamMode::Plan)),
            ModeVerify::Confirmed
        );
        assert_eq!(
            verify_mode_switch(Some(MamMode::Plan), Some(MamMode::Default)),
            ModeVerify::Mismatch {
                expected: MamMode::Plan,
                observed: MamMode::Default
            },
            "回读与预期不符必须如实报两边（不假装成功）"
        );
        assert_eq!(
            verify_mode_switch(None, Some(MamMode::Plan)),
            ModeVerify::Unverifiable,
            "无回读源/无法预测 → 不可判定"
        );
        assert_eq!(
            verify_mode_switch(Some(MamMode::Plan), None),
            ModeVerify::Unverifiable,
            "屏读失败 → 不可判定"
        );
    }

    // ==== D20：模式回读的**动态轮询**（脚本化屏序列驱动，不碰真 conhost）====

    /// **回读轮询的脚本化驱动器**：把一串「时间序列屏」喂给 [`poll_mode_readback`]，
    /// 收集（产物, 实际屏读次数, settle 次数）。
    ///
    /// 脚本语义（对齐生产轮询语义，同 `run_stage_script` 的做法）：`screens[i]` 是
    /// **第 i 拍读到的屏**；`None` 元素 = 那一拍读不到屏（模拟 AttachConsole 失败）；
    /// 序列耗尽后**重复最后一屏**（模拟「屏不再变」= 窗内一直停在旧档）。
    /// `settle` 空操作——测试不需要真等重绘（墙钟不前进，故 15 拍也是瞬时的）。
    fn run_readback_script(
        tool: &str,
        expected: Option<MamMode>,
        rounds: u32,
        screens: &[Option<&[String]>],
    ) -> (ModeReadbackOutcome, u32, u32) {
        run_readback_script_group(tool, ModeGroupId::Mode, expected, rounds, screens)
    }

    /// 按组版驱动器（模式轴测试走 [`run_readback_script`] 便捷包装；权限轴回读测试
    /// 直接用本函数——kimi 权限组 2.1.1 起有底栏回读源）
    fn run_readback_script_group(
        tool: &str,
        group: ModeGroupId,
        expected: Option<MamMode>,
        rounds: u32,
        screens: &[Option<&[String]>],
    ) -> (ModeReadbackOutcome, u32, u32) {
        use std::cell::Cell;
        let reads = Cell::new(0u32);
        let settles = Cell::new(0u32);
        let out = poll_mode_readback(
            tool,
            group,
            expected,
            rounds,
            || {
                let i = reads.get() as usize;
                reads.set(reads.get() + 1);
                let idx = i.min(screens.len().saturating_sub(1));
                screens
                    .get(idx)
                    .and_then(|s| s.as_ref())
                    .map(|s| s.to_vec())
            },
            || settles.set(settles.get() + 1),
        );
        (out, reads.get(), settles.get())
    }

    /// **用户观察①的形态（本任务的主回归锁）**：第一拍读到**旧档**、后续拍读到**目标档**
    /// → 最终必须是 `Confirmed`（而不是像旧实现那样报「回读与预期不符」）。
    ///
    /// 夹具用 claude 的真机底栏原文（T6 探测档案逐字）：`manual mode on`（默认档）→
    /// 下一拍 `plan mode on`（目标档）——正是「屏幕重绘未及」的两帧。
    ///
    /// 还原动作（变异①）：把 `poll_mode_readback` 的循环去掉、改成「读一次就返回」
    /// （等价于旧实现）→ 本测试先红（verdict 会是 Mismatch{Plan, Default}）。
    #[test]
    fn readback_poll_covers_observation_one_stale_frame() {
        let stale = lines(&["  ⏸ manual mode on · ? for shortuts ·←for agents"]);
        let fresh = lines(&["  ⏸ plan mode on (shift+tab to cycle) ·  for agents"]);
        let (out, reads, settles) = run_readback_script(
            "claude",
            Some(MamMode::Plan),
            15,
            &[Some(stale.as_slice()), Some(fresh.as_slice())],
        );
        assert_eq!(
            out.verdict,
            ModeVerify::Confirmed,
            "第一拍旧档、第二拍目标档 → 必须继续轮询并最终确认（观察①的修复形态）"
        );
        assert_eq!(
            out.observed,
            Some(MamMode::Plan),
            "最后一拍读到的就是目标档"
        );
        assert_eq!(reads, 2, "命中即刻停止：只读两拍");
        assert_eq!(settles, 1, "只在两拍之间等待一次（命中当拍不再等）");
    }

    /// **命中即刻停止**（D20(a)「不浪费固定延迟」）：首拍即读到目标档 → 只读一拍、
    /// **一次都不 settle**（否则就是「固定睡眠」换了个位置）。
    #[test]
    fn readback_poll_stops_on_first_hit_without_sleeping() {
        let fresh = lines(&["  ⏸ plan mode on (shift+tab to cycle)"]);
        let (out, reads, settles) =
            run_readback_script("claude", Some(MamMode::Plan), 15, &[Some(fresh.as_slice())]);
        assert_eq!(out.verdict, ModeVerify::Confirmed);
        assert_eq!(reads, 1, "首拍命中即停");
        assert_eq!(settles, 0, "命中当拍不等待（禁止用固定睡眠替代轮询）");
    }

    /// **窗尽如实区分三态**（D20(b)）：同样跑满窗，三种屏序列给出三个**不同**的结论——
    /// 「不符」（屏上明确是别的档，且**一直**是别的档）与「未及确认」（始终读不到屏）
    /// 绝不能塌成一个「失败」。
    ///
    /// 还原动作（变异②）：把 `ModeVerify::Mismatch` 与 `Unverifiable` 在回执层合并成
    /// 同一句话（`remote::api::mode_verify_receipt`）→ `remote::api` 的文案区分断言先红
    /// （本测试同时钉住内核侧的两种产物确实不同）。
    #[test]
    fn readback_poll_distinguishes_mismatch_from_unverifiable_at_window_end() {
        let stale = lines(&["  ⏸ manual mode on · ? for shortuts ·←for agents"]);
        // ① 窗内**一直**是旧档（真的没切成）→ Mismatch（如实报两边）
        let (m, reads, _) = run_readback_script(
            "claude",
            Some(MamMode::Plan),
            3,
            &[Some(stale.as_slice())], // 序列耗尽后重复最后一屏 = 屏不再变
        );
        assert_eq!(
            m.verdict,
            ModeVerify::Mismatch {
                expected: MamMode::Plan,
                observed: MamMode::Default,
            },
            "屏上明确是别的档 → 报「不符」并给出两边"
        );
        assert_eq!(
            m.observed,
            Some(MamMode::Default),
            "回执的 current = 实际读到的档"
        );
        assert_eq!(reads, 3, "窗尽才停（3 拍全读）");

        // ② 窗内**始终读不到屏** → Unverifiable（未及确认），**不是** Mismatch
        let (u, reads_u, _) =
            run_readback_script("claude", Some(MamMode::Plan), 3, &[None, None, None]);
        assert_eq!(
            u.verdict,
            ModeVerify::Unverifiable,
            "读不到屏 → 未及确认（不得与「不符」混为一谈）"
        );
        assert_eq!(
            u.observed, None,
            "没有读数 → current 必须为 null（不给过期值）"
        );
        assert_eq!(reads_u, 3, "读不到也要读满窗（可能只是没重绘完）");
        assert_ne!(m.verdict, u.verdict, "两种超时的结论必须不同（D20(b)）");

        // ③ **命中即停**：中间拍到目标档的那一拍就是终点——后续屏读（这里故意给
        //    `None`）**不再发生**，结论固定为 Confirmed（D20(a)「命中即刻停止」，
        //    不浪费固定延迟，也不「回头」重判）
        let fresh = lines(&["  ⏸ plan mode on (shift+tab to cycle)"]);
        let (f, reads_f, settles_f) = run_readback_script(
            "claude",
            Some(MamMode::Plan),
            3,
            &[Some(fresh.as_slice()), None],
        );
        assert_eq!(
            f.verdict,
            ModeVerify::Confirmed,
            "命中即停，不受后续屏序列影响"
        );
        assert_eq!(f.observed, Some(MamMode::Plan));
        assert_eq!(reads_f, 1, "命中当拍即停：第二拍根本不再读");
        assert_eq!(settles_f, 0, "命中当拍也不等待");
    }

    /// **无判据可读时不空转**（D20(a) 的判据是「读到判据本身」——判据不存在就没有
    /// 轮询可做）：`expected = None`（该组无回读源 / 步进组当前档未知）→ 本内核只读
    /// 一拍、**不睡**，产物恒为 `Unverifiable`（回执据此说「请人工核对」，不假装）。
    ///
    /// 这条同时钉住「把 1.5s 睡满」那种伪轮询：若实现改成无条件跑满窗，
    /// `settles` 会从 0 变成 14 → 先红。
    #[test]
    fn readback_poll_does_not_spin_without_a_criterion() {
        let fresh = lines(&["  ⏸ plan mode on (shift+tab to cycle)"]);
        let (out, reads, settles) =
            run_readback_script("claude", None, 15, &[Some(fresh.as_slice())]);
        assert_eq!(
            out.verdict,
            ModeVerify::Unverifiable,
            "无应到档可推算 → 不可判定（即便屏上读到了明确档位）"
        );
        assert_eq!(
            out.observed,
            Some(MamMode::Plan),
            "但仍带上读数供回执的 current 用"
        );
        assert_eq!(reads, 1, "只读一拍");
        assert_eq!(settles, 0, "不睡（判据不存在，转窗也不会读到判据本身）");
    }

    /// **拍数下界为 1**：调用方传入 `rounds = 0` → 仍读一拍（0 拍 = 不读 = 无产物，
    /// 那不是有界轮询而是放弃；`timing::poll_rounds` 已保证不下发 0，这里是纵深防御）。
    #[test]
    fn readback_poll_always_reads_at_least_once() {
        let (out, reads, _) = run_readback_script("claude", Some(MamMode::Plan), 0, &[None]);
        assert_eq!(out.reads, 1);
        assert_eq!(reads, 1);
        assert_eq!(out.verdict, ModeVerify::Unverifiable);
    }

    // ==== 屏读解析（分族；夹具 = 真机屏幕原文）====

    /// **codex 模式读反回归锁**（2026-10-10 用户实测：Plan 档被读成 Default）：
    /// 0.160.0+ 底栏拆两行——状态行（`… · 工作目录` + 行尾 `Plan mode`）之下还有
    /// **快捷键行**（`← for agents · ? for shortcuts · ⚠N warnings · f2 to view`，
    /// 含 ` · ` 分隔符、无模式词）。全屏自底向上扫描先命中快捷键行 → 缺席推断
    /// 恒 Default → Plan 档读反。判据单点 = [`codex_mode_current_from_screen`]
    /// （状态栏行窗+事件行剔除）；夹具 = mp-st-2/3 真机 dump（mode-perm-20261009）。
    #[test]
    fn parse_mode_codex_two_line_status_bar_plan_vs_default() {
        // Plan 态真机帧：状态行含行尾 Plan mode，其下是快捷键行
        assert_eq!(
            parse_mode_from_screen("codex", &mp_screen("codex-mp-plan-mode.txt")),
            Some(MamMode::Plan),
            "Plan 档不得被快捷键行的 ` · ` 读成 Default（用户实测读反回归）"
        );
        // Default 态真机帧：状态行无模式词
        assert_eq!(
            parse_mode_from_screen("codex", &mp_screen("codex-mp-default-mode.txt")),
            Some(MamMode::Default)
        );
    }

    /// claude：四档底栏逐字快照（T6 探测档案 §1.2 与 `screen-t6-claude-*` 原文）
    #[test]
    fn parse_claude_real_footers() {
        assert_eq!(
            parse_mode_from_screen(
                "claude",
                &lines(&["  ⏵⏵ accept edits on (shift+tab to cycle) · ← for agents"])
            ),
            Some(MamMode::AcceptEdits)
        );
        assert_eq!(
            parse_mode_from_screen(
                "claude",
                &lines(&["  ⏸ plan mode on (shift+tab to cycle) ·  for agents"])
            ),
            Some(MamMode::Plan)
        );
        assert_eq!(
            parse_mode_from_screen(
                "claude",
                &lines(&["  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents"])
            ),
            Some(MamMode::Bypass),
            "happy 的 auto 归 兔维斯 的 Bypass（调研档案 §3）"
        );
        assert_eq!(
            parse_mode_from_screen(
                "claude",
                &lines(&["  ⏸ manual mode on · ? for shortuts ·←for agents"])
            ),
            Some(MamMode::Default)
        );
        // 底栏在正文之下：自底向上取最靠下的命中
        assert_eq!(
            parse_mode_from_screen(
                "claude",
                &lines(&[
                    "  ⏸ plan mode on (shift+tab to cycle)", // 正文引用的旧状态
                    "  some body text",
                    "  ⏵⏵ accept edits on (shift+tab to cycle) · ← for agents",
                ])
            ),
            Some(MamMode::AcceptEdits),
        );
    }

    /// claude 的**计划批准框在场**时不得被读成「当前 Plan 档」——屏上是
    /// `No, stay in Plan mode`（无 `on`），且底栏已被对话框顶掉 → 未知（如实）
    #[test]
    fn claude_dialog_does_not_fake_plan_mode() {
        let screen = lines(&[
            " Claude has written up a plan and is ready to execute. Would you like to proceed?",
            "",
            " ❯ 1. Yes, and use auto mode",
            "   2. Yes, manually approve edits",
            "   3. No, stay in Plan mode",
            "      shift+tab to approve with this feedback",
        ]);
        assert_eq!(
            parse_mode_from_screen("claude", &screen),
            None,
            "对话框在场 = 底栏不可见 → 档未知（不得把选项文本当当前档）"
        );
    }

    /// opencode：`Build`/`Plan` 双档真机快照（5/5 校准的形态）
    #[test]
    fn parse_opencode_real_footers() {
        assert_eq!(
            parse_mode_from_screen(
                "opencode",
                &lines(&["  Build   DeepSeek V4.1 Flash Ollama Cloud ·high"])
            ),
            Some(MamMode::Default)
        );
        assert_eq!(
            parse_mode_from_screen(
                "opencode",
                &lines(&["  Plan   DeepSeek V4.1 Flash Ollama Cloud   high"])
            ),
            Some(MamMode::Plan)
        );
        // 模型名/路径不得误命中（`Planner`/`planning` 不是独立词）
        assert_eq!(
            parse_mode_from_screen("opencode", &lines(&["  Planner agent started"])),
            None
        );
        assert_eq!(
            parse_mode_from_screen("opencode", &lines(&["  planning the implementation"])),
            None
        );
    }

    /// kimi：`plan` 前缀 → Plan；**认出底栏但无前缀 → Default（缺席推断）**
    #[test]
    fn parse_kimi_real_footers() {
        assert_eq!(
            parse_mode_from_screen(
                "kimi",
                &lines(&[
                    " plan  GLM-5.3-Flash thinking: high  C:\\Users\\u\\AppData\\Local\\Temp\\proj-kimi",
                ])
            ),
            Some(MamMode::Plan)
        );
        assert_eq!(
            parse_mode_from_screen(
                "kimi",
                &lines(&[
                    " GLM-5.3-Flash thinking: high  C:\\Users\\u\\AppData\\Local\\Temp\\proj-kimi   ctrl+o expand",
                ])
            ),
            Some(MamMode::Default),
            "缺席推断：底栏在、无 plan 前缀 ⇒ 默认档（§2.6 表末）"
        );
        // 认不出底栏（无 `thinking:` 锚）→ 未知（不把没读到的屏说成 Default）
        assert_eq!(
            parse_mode_from_screen("kimi", &lines(&["  一些普通输出", "  > "])),
            None
        );
        // 计划文件路径里含 plan 字样但底栏无前缀：仍按底栏判 Default（路径不是判据）
        assert_eq!(
            parse_mode_from_screen(
                "kimi",
                &lines(&[
                    " C:/Users/u/.kimi-code/.../plans/ms-marvel.md",
                    " GLM-5.3-Flash thinking: high  C:\\proj-kimi",
                ])
            ),
            Some(MamMode::Default)
        );
    }

    /// codex：`Plan mode` 短语 → Plan；` · ` 状态栏在场无 Plan mode → Default（缺席推断）
    #[test]
    fn parse_codex_real_footers() {
        assert_eq!(
            parse_mode_from_screen(
                "codex",
                &lines(&[
                    "  glm-5.3-flash medium · ~\\AppData\\Local\\Temp\\proj-codex  Plan mode (shift+tab to cycle)",
                ])
            ),
            Some(MamMode::Plan)
        );
        assert_eq!(
            parse_mode_from_screen(
                "codex",
                &lines(&["  glm-5.3-flash medium · ~\\AppData\\Local\\Temp\\proj-codex"])
            ),
            Some(MamMode::Default),
            "缺席推断：状态栏在、无 Plan mode ⇒ 默认档"
        );
        // 底栏在正文之下：自底向上取最靠下的命中（实现期在 Plan，正文提过 default）
        assert_eq!(
            parse_mode_from_screen(
                "codex",
                &lines(&[
                    "  计划如下：",
                    "  1. 检查 main.py。",
                    "  glm-5.3-flash medium · ~\\proj-codex  Plan mode (shift+tab to cycle)",
                ])
            ),
            Some(MamMode::Plan)
        );
        // 认不出状态栏 → 未知
        assert_eq!(
            parse_mode_from_screen(
                "codex",
                &lines(&["  Press enter to confirm or esc to go back"])
            ),
            None
        );
        // `Planning` / 路径里的 plan 不算（只认 `plan mode` 短语与 ` · ` 状态栏）
        assert_eq!(
            parse_mode_from_screen("codex", &lines(&["  Planning next steps"])),
            None
        );
    }

    // ==== codex 模式事件行 × 模式轴双源判定（2026-10-09 风险预测试批 T2）====

    /// mode-perm-20261009 屏读夹具读取（e_stage2_screen 同款惯例；注意目录层级：
    /// e-stage2 在仓库根 `tests/fixtures/`，本目录在 `src-tauri/tests/fixtures/`）
    #[cfg(test)]
    fn mp_screen(name: &str) -> Vec<String> {
        mp_screen_in("mode-perm-20261009", name)
    }

    /// mode-perm 屏读夹具读取的**目录变体**（2026-10-10 起多批次夹具并存：
    /// `mode-perm-20261009` 诊断批 / `mode-perm-20261010` codex-perm-blind 取证批）
    #[cfg(test)]
    fn mp_screen_in(dir: &str, name: &str) -> Vec<String> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(dir)
            .join(name);
        let raw =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("读取夹具失败 {path:?}: {e}"));
        raw.trim_start_matches('\u{feff}')
            .lines()
            .filter(|l| !l.starts_with("# "))
            .map(|l| l.trim_end_matches('\r').to_string())
            .collect()
    }

    /// codex 模式事件行判据（2026-10-09 底料 §1 逐字；变量中段不参与匹配）
    #[test]
    fn codex_mode_event_line_parses_both_modes() {
        assert_eq!(
            parse_codex_mode_event_line(
                "• Model changed to deepseek-v4.1-flash medium for Plan mode."
            ),
            Some(MamMode::Plan)
        );
        assert_eq!(
            parse_codex_mode_event_line(
                "• Model changed to deepseek-v4.1-flash high for Default mode."
            ),
            Some(MamMode::Default)
        );
        // 变量无关：换模型名/effort 仍命中
        assert_eq!(
            parse_codex_mode_event_line("• Model changed to glm-5.3-flash max for Plan mode."),
            Some(MamMode::Plan)
        );
        // 负例：正文引用/半句/无尾段句号不命中
        assert_eq!(
            parse_codex_mode_event_line("Model changed to X for Plan mode"),
            None,
            "缺句号（尾段锁含句号）"
        );
        assert_eq!(
            parse_codex_mode_event_line("I asked for Plan mode."),
            None,
            "无前缀锚"
        );
        assert_eq!(parse_codex_mode_event_line(""), None);
    }

    /// codex 模式轴双源优先级（spec §3.3）：状态栏权威优先；事件行兜底；
    /// 两源皆无 → None
    #[test]
    fn codex_mode_current_state_bar_wins_over_event_lines() {
        // Plan 态夹具：状态栏 Plan mode 短语在场 + 事件行也在——状态栏可判即采信
        // （结果同为 Plan；优先级语义由滚出态测试反向锁定：事件行滚出后状态栏
        // 仍独立判档 Default，证明不是事件行在出答案）
        let plan = mp_screen("codex-mp-plan-mode.txt");
        assert_eq!(codex_mode_current_from_screen(&plan), Some(MamMode::Plan));
        // Default 态：状态栏 ` · ` 形态（无模式字样）→ Default（缺席推断）
        let default = mp_screen("codex-mp-default-mode.txt");
        assert_eq!(
            codex_mode_current_from_screen(&default),
            Some(MamMode::Default)
        );
    }

    #[test]
    fn codex_mode_current_event_line_fallback_when_state_bar_unreadable() {
        // 构造：状态栏被挤掉（删掉含 ` · ` 与 Plan mode 的行），只剩事件行
        let mut lines = mp_screen("codex-mp-plan-mode.txt");
        lines.retain(|l| {
            let lo = l.to_lowercase();
            !(lo.contains(" · ") || lo.contains("plan mode") && !lo.contains("model changed"))
        });
        assert_eq!(
            codex_mode_current_from_screen(&lines),
            Some(MamMode::Plan),
            "状态栏不可判 → 自底向上最新事件行兜底"
        );
        // 全空 → None
        assert_eq!(codex_mode_current_from_screen(&["".to_string()]), None);
    }

    #[test]
    fn codex_mode_current_scrolled_returns_none_or_default() {
        // 滚出态夹具（源 mp-scroll-after）：事件行滚出、状态栏 ` · ` 行仍在
        // → 状态栏权威照常，缺席推断 Default（夹具实测钉死，不留双态断言）
        let scrolled = mp_screen("codex-mp-scrolled.txt");
        assert_eq!(
            codex_mode_current_from_screen(&scrolled),
            Some(MamMode::Default),
            "滚出后状态栏仍可判（` · ` 在场）→ Default 缺席推断"
        );
    }

    /// 终审 P2-4：事件行落进底 3 行窗**不误判**——事件行（`for Plan mode.`）与真
    /// 状态栏同窗时，状态栏步骤不得把事件行短语误读成「状态栏在场」（旧裸窗把底
    /// 3 行拼成整串判 `plan mode` 短语，事件行的短语抢在真状态栏的 ` · ` 形态之前
    /// 命中 → 误判 Plan，状态栏 Default 权威被遮蔽）。与 [`codex_switch_verified`]
    /// 一致化后事件行先剔除 → 状态栏 ` · ` 形态判 Default。还原动作（变异）：把
    /// 状态栏步骤改回裸窗 → 本测试先红。
    #[test]
    fn codex_current_state_bar_window_ignores_event_lines() {
        let screen = lines(&[
            "  普通输出",
            "  glm-5.3-flash high · ~\\proj", // 真状态栏：Default
            "• Model changed to glm-5.3-flash max for Plan mode.", // 事件行（切换记录）
            "› Ask Codex to do anything",
        ]);
        assert_eq!(
            codex_mode_current_from_screen(&screen),
            Some(MamMode::Default),
            "窗内事件行不算状态栏——状态栏权威不被遮蔽"
        );
    }

    // ==== codex 权限回执行解析 × 权限轴屏读（2026-10-09 风险预测试批 T3）====

    /// codex 权限回执行解析（底料 §1/§7 逐字）：四档标签全形前缀匹配
    #[test]
    fn codex_permission_event_line_parses_all_tiers() {
        assert_eq!(
            parse_codex_permission_event_line("• Permission selection requested: Read Only"),
            Some(MamMode::ReadOnly)
        );
        assert_eq!(
            parse_codex_permission_event_line(
                "• Permission selection requested: Ask for approval (non-admin sandbox)"
            ),
            Some(MamMode::Default),
            "档 2 回执行打菜单全形标签（底料 §7）→ 前缀匹配到短形"
        );
        assert_eq!(
            parse_codex_permission_event_line("• Permission selection requested: Approve for me"),
            Some(MamMode::AcceptEdits)
        );
        assert_eq!(
            parse_codex_permission_event_line("• Permission selection requested: Full Access"),
            Some(MamMode::Bypass)
        );
        // 负例：菜单行/标题/认不出的档（不猜）
        assert_eq!(
            parse_codex_permission_event_line("1. Ask for approval (non-admin sandbox)"),
            None,
            "菜单行无账本锚 → 不命中"
        );
        assert_eq!(
            parse_codex_permission_event_line("Update Model Permissions"),
            None,
            "标题行无账本锚 → 不命中"
        );
        assert_eq!(
            parse_codex_permission_event_line("• Permission selection requested: Unknown Tier"),
            None,
            "锚命中但标签匹配不到任何档 → None（不猜）"
        );
        // 0.154 旧回执锚（无冒号标签段）：锚步放行、冒号步自然 None——不混入新判据
        assert_eq!(
            parse_codex_permission_event_line("• Permissions updated to Full Access"),
            None
        );
    }

    /// codex 权限轴屏读（最新回执行=当前档；撕裂容忍；滚出=None）
    #[test]
    fn codex_permission_current_latest_line_wins() {
        // 夹具 codex-mp-fa-done.txt：四行回执行并存（底料 §8），Full Access 最新 → Bypass
        let done = mp_screen("codex-mp-fa-done.txt");
        assert_eq!(
            codex_permission_current_from_screen(&done),
            Some(MamMode::Bypass)
        );
        // 夹具 codex-mp-receipt-readonly.txt：单行 Read Only 回执行 → ReadOnly
        let ro = mp_screen("codex-mp-receipt-readonly.txt");
        assert_eq!(
            codex_permission_current_from_screen(&ro),
            Some(MamMode::ReadOnly)
        );
        // 滚出态夹具（mp-scroll-after）：零回执行 → None（调用方回落记忆通道）
        let scrolled = mp_screen("codex-mp-scrolled.txt");
        assert_eq!(codex_permission_current_from_screen(&scrolled), None);
        // 多行并存：Read Only 回执行后追加 Full Access → 取最新（Full Access）
        let mut multi = ro.clone();
        multi.push("• Permission selection requested: Full Access".to_string());
        assert_eq!(
            codex_permission_current_from_screen(&multi),
            Some(MamMode::Bypass)
        );
        // 撕裂容忍：同一回执行连打两行 → 判定不变（重复行内容相同、解析结果一致，
        // find_map 自底向上取最新有效行——无需去重）
        multi.push("• Permission selection requested: Full Access".to_string());
        assert_eq!(
            codex_permission_current_from_screen(&multi),
            Some(MamMode::Bypass)
        );
    }

    // ==== 切换闭环核验：内容集差分（2026-10-09 T4，spec §3.1/§3.2/§4-R5）====

    /// **新事件行命中**（模式轴）：基线含旧行 `…for Plan mode.` → 动作后屏出现
    /// **新行** `…high for Default mode.`（不在基线集合）→ verified（R1d/e 锁：
    /// 旧行留存不作证据，新行才是）。
    #[test]
    fn switch_verified_by_new_event_line() {
        let baseline_screen = lines(&[
            "  普通输出",
            "• Model changed to deepseek-v4.1-flash medium for Plan mode.",
        ]);
        let baseline = capture_mode_switch_baseline(&baseline_screen);
        let after = lines(&[
            "  普通输出",
            "• Model changed to deepseek-v4.1-flash medium for Plan mode.",
            "• Model changed to deepseek-v4.1-flash high for Default mode.",
        ]);
        assert!(
            codex_switch_verified(&after, &baseline, ModeGroupId::Mode, MamMode::Default),
            "新于基线的 Default 事件行 = 切换生效"
        );
        // 反向：新 Default 行在屏、目标却是 Plan → 不 verified（目标不符）
        assert!(
            !codex_switch_verified(&after, &baseline, ModeGroupId::Mode, MamMode::Plan),
            "新行是 Default 不是 Plan——不得假 verified"
        );
    }

    /// **视口还原旧行不构成证据**（R5 核心锁）：基线含 `…for Default mode.` 旧行 →
    /// 动作后**同一行原样**还原（无新行）→ 不 verified。还原动作（变异）：把判据
    /// 退化成纯「行存在」→ 本测试先红（旧行留存误确认的形态）。
    #[test]
    fn switch_not_verified_by_stale_line() {
        let stale = "• Model changed to deepseek-v4.1-flash high for Default mode.";
        let baseline_screen = lines(&["  普通输出", stale]);
        let baseline = capture_mode_switch_baseline(&baseline_screen);
        let after = baseline_screen.clone(); // 视口还原 = 同一屏原样
        assert!(
            !codex_switch_verified(&after, &baseline, ModeGroupId::Mode, MamMode::Default),
            "旧行还原不是新证据（内容集差分）"
        );
    }

    /// **状态栏权威**：无新事件行、状态栏翻到目标档 → verified（spec §3.3
    /// 「状态栏在场即权威」；同文案重复切换无法与还原区分时判据退化到这一路）。
    #[test]
    fn switch_verified_by_state_bar_even_without_new_line() {
        // 基线 = Plan 态夹具（状态栏 Plan mode 短语 + 旧行 `for Plan mode.` 都在集合里）
        let baseline_screen = mp_screen("codex-mp-plan-mode.txt");
        let baseline = capture_mode_switch_baseline(&baseline_screen);
        // 动作后 = Default 态夹具（状态栏 ` · ` 形态 → Default 缺席推断；无新事件行）
        let after = mp_screen("codex-mp-default-mode.txt");
        assert!(
            codex_switch_verified(&after, &baseline, ModeGroupId::Mode, MamMode::Default),
            "状态栏翻到 Default = 切换生效（权威源优先）"
        );
    }

    /// **权限轴新回执行命中**：基线 = receipt-readonly 夹具（含 Read Only 行）→
    /// 动作后追加 `… requested: Approve for me` 新行 → AcceptEdits verified。
    #[test]
    fn permission_switch_verified_by_new_receipt_line() {
        let baseline = capture_mode_switch_baseline(&mp_screen("codex-mp-receipt-readonly.txt"));
        let mut after = mp_screen("codex-mp-receipt-readonly.txt");
        after.push("• Permission selection requested: Approve for me".to_string());
        assert!(
            codex_switch_verified(
                &after,
                &baseline,
                ModeGroupId::Permission,
                MamMode::AcceptEdits
            ),
            "新于基线的 Approve for me 回执行 = 切换生效"
        );
    }

    /// **权限轴视口还原旧行**（R5 同款）：基线含 Full Access 行 → 动作后同一行
    /// 原样还原（无新行）→ Bypass 不 verified。权限轴状态栏零显示 → 退化为
    /// 「无新行即不 verified」，如实 false，不假确认。
    #[test]
    fn permission_switch_not_verified_by_restored_line() {
        let restore = "• Permission selection requested: Full Access";
        let baseline_screen = lines(&["  普通输出", restore]);
        let baseline = capture_mode_switch_baseline(&baseline_screen);
        let after = baseline_screen.clone();
        assert!(
            !codex_switch_verified(&after, &baseline, ModeGroupId::Permission, MamMode::Bypass),
            "旧行还原不是新证据（权限轴零状态栏 → 无新行即不 verified）"
        );
    }

    /// **0.154 旧锚兼容**：`permissions updated to <label>` 行（新于基线）同样
    /// 构成权限轴证据（spec §3.2——旧锚新于基线 ∨ 新锚）。
    #[test]
    fn permission_switch_verified_by_legacy_anchor_line() {
        let baseline = capture_mode_switch_baseline(&lines(&["  普通输出"]));
        let after = lines(&["  普通输出", "• Permissions updated to Full Access"]);
        assert!(
            codex_switch_verified(&after, &baseline, ModeGroupId::Permission, MamMode::Bypass),
            "旧锚 permissions updated to Full Access（新于基线）= 0.154 兼容证据"
        );
    }

    // ==== codex 模式组 Key 路闭环编排（2026-10-09 T4，spec §3.1）====

    /// **前读已在「目标」档 → 仍必然发键**（2026-10-10 用户指令新语义锁，原
    /// `mode_toggle_already_on_target_zero_keys` 的反转型改写）：read 返回
    /// plan-mode 夹具、fallback_expected=Plan（卡面也以为已在 Plan——正是实机
    /// 「卡面过期」形态）→ **无零投递分支**，shift+tab 照发；核验预期 = 前读
    /// 翻转（Default）——首拍出现新 Default 事件行 → verified=true。
    /// 还原动作（变异）：把无条件发键改回「前读==预期即零投递」→ 本测试先红。
    #[test]
    fn mode_toggle_pre_read_target_still_sends_key() {
        let sent: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let plan = mp_screen("codex-mp-plan-mode.txt");
        // 动作后屏 = plan 夹具 + 一条**新** Default 事件行（内容集差分新证据）
        let mut after = plan.clone();
        after.push("• Model changed to glm-5.3-flash max for Default mode.".to_string());
        let verify_screens = std::cell::RefCell::new(vec![after.clone()]);
        let out = run_codex_mode_toggle_stages(
            MamMode::Plan, // fallback_expected = 前端 target（卡面停在 Plan）
            3,
            || Some(plan.clone()),
            || Ok(verify_screens.borrow_mut().pop()),
            || {},
            &mut Closures {
                read: || None,
                send: |k: &str| {
                    sent.borrow_mut().push(k.to_string());
                    Ok(())
                },
                settle: || {},
            },
        )
        .expect("前读可判 → 必然发键（无零投递分支）");
        assert!(out.verified, "翻转预期 Default 命中新事件行 → verified");
        assert_eq!(out.pre_read, Some(MamMode::Plan));
        assert_eq!(out.sent_keys, vec!["shift+tab".to_string()], "无条件发键");
        assert_eq!(sent.borrow().len(), 1, "恰好一次 shift+tab");
    }

    /// **前读不可判 → 核验预期取 fallback_expected**（前端 target 兜底的语义锁）：
    /// 前读返回空屏（状态栏+事件行都判不出）→ 继续发键且 `pre_read=None` 如实上报；
    /// 核验拍出现**新 Plan 事件行** → verified=true——证明预期确实落在了兜底档
    /// （Plan）上，而不是别处。
    #[test]
    fn mode_toggle_continues_when_pre_read_indeterminate() {
        let blank = lines(&["  普通输出", "  另一行"]);
        let sent: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let after = lines(&[
            "  普通输出",
            "  另一行",
            "• Model changed to glm-5.3-flash max for Plan mode.",
        ]);
        let verify_screens = std::cell::RefCell::new(vec![after.clone()]);
        let out = run_codex_mode_toggle_stages(
            MamMode::Plan, // fallback = 前端 target
            3,
            || Some(blank.clone()),
            || Ok(verify_screens.borrow_mut().pop()),
            || {},
            &mut Closures {
                read: || None,
                send: |k: &str| {
                    sent.borrow_mut().push(k.to_string());
                    Ok(())
                },
                settle: || {},
            },
        )
        .expect("前读不可判不是拒绝理由");
        assert_eq!(out.pre_read, None, "前读不可判 → None 如实");
        assert_eq!(
            sent.borrow().as_slice(),
            ["shift+tab".to_string()],
            "继续发键（无零投递分支）"
        );
        assert!(
            out.verified,
            "兜底预期 Plan 命中新 Plan 事件行 → 兜底确实驱动核验"
        );
    }

    /// **基线差分核验命中**：脚本化三屏——前读屏（Default 夹具）→ 发键 →
    /// 核验拍出现**新** Plan 事件行 → verified=true 且命中即停（读 1 拍）。
    /// 预期 = 前读翻转（Default→Plan），与 fallback（Plan）本例巧合同值。
    #[test]
    fn mode_toggle_verifies_by_baseline_diff() {
        let pre = mp_screen("codex-mp-default-mode.txt");
        let baseline = capture_mode_switch_baseline(&pre);
        // 动作后屏 = 前读屏 + 一条**新** Plan 事件行。注意：夹具里已有
        // `deepseek-…medium for Plan mode.` 旧行——内容集差分下同文案不算新，
        // 必须用变量段不同的行（parse 只认尾段 `for <目标> mode.`）才是新证据。
        let mut after = pre.clone();
        after.push("• Model changed to glm-5.3-flash max for Plan mode.".to_string());
        let verify_screens = std::cell::RefCell::new(vec![after.clone()]);
        let reads = std::cell::Cell::new(0u32);
        let out = run_codex_mode_toggle_stages(
            MamMode::Plan,
            3,
            || Some(pre.clone()),
            || {
                reads.set(reads.get() + 1);
                Ok(verify_screens.borrow_mut().pop())
            },
            || {},
            &mut Closures {
                read: || None,
                send: |k: &str| {
                    assert_eq!(k, "shift+tab", "发的是生产在役键");
                    Ok(())
                },
                settle: || {},
            },
        )
        .expect("编排应成功");
        assert!(out.verified, "新事件行命中 → verified");
        assert_eq!(out.pre_read, Some(MamMode::Default), "前读=Default");
        // 终审 P2-4 判据一致化后本断言随实态改写：合成屏 = Default 夹具（状态栏
        // 未翻转）+ 追加新 Plan 事件行——observed = 末拍屏读的当前档，**状态栏在场
        // 即权威**（spec §3.3；事件行只是切换记录）→ 如实取 Default；verified 仍由
        // 「新于基线」的事件行命中（两判据各司其职，P2-4 前旧窗判据会误取事件行的
        // Plan——正是被一致化掉的遮蔽面）。
        assert_eq!(
            out.observed,
            Some(MamMode::Default),
            "末拍屏读=状态栏权威档"
        );
        assert_eq!(out.sent_keys, vec!["shift+tab".to_string()]);
        assert_eq!(reads.get(), 1, "第 1 拍即命中（命中即停）");
        let _ = baseline; // 基线由编排内部 capture（此处只保证前读屏被差分消费）
    }

    /// **窗尽无变化 → verified=false**：核验拍恒返回同屏（无新行、状态栏不变），
    /// rounds 上界 = 3 拍 → 3 拍耗尽后如实 false（**上界的正确测试形态**：闭包
    /// 永不 None，靠 rounds 兜住——生产挂死回归锁）。
    #[test]
    fn mode_toggle_reports_no_change_on_window_exhaust() {
        let pre = mp_screen("codex-mp-default-mode.txt");
        let reads = std::cell::Cell::new(0u32);
        let out = run_codex_mode_toggle_stages(
            MamMode::Plan,
            3,
            || Some(pre.clone()),
            || {
                reads.set(reads.get() + 1);
                Ok(Some(pre.clone())) // 每拍同一屏：无新行、状态栏仍是 Default
            },
            || {},
            &mut Closures {
                read: || None,
                send: |k: &str| {
                    let _ = k;
                    Ok(())
                },
                settle: || {},
            },
        )
        .expect("编排应成功（窗尽不是 Err，是如实 false）");
        assert!(!out.verified, "窗尽无变化 → verified=false");
        assert_eq!(
            out.observed,
            Some(MamMode::Default),
            "末拍屏读=Default（未切走）"
        );
        assert_eq!(
            reads.get(),
            3,
            "拍数上界生效：恰读 3 拍后窗尽退出（不挂死）"
        );
    }

    /// **前读判定进 outcome**：前读屏判 Default、fallback=Plan（= 前读翻转后的
    /// 预期）→ `pre_read=Some(Default)`（api.rs 回执 hint 据此申报「前读=终端当前
    /// X」——spec §3.1 不代卡面纠偏）。
    #[test]
    fn mode_toggle_hint_carries_pre_read() {
        let pre = mp_screen("codex-mp-default-mode.txt");
        let out = run_codex_mode_toggle_stages(
            MamMode::Plan,
            3,
            || Some(pre.clone()),
            || Ok(Some(pre.clone())),
            || {},
            &mut Closures {
                read: || None,
                send: |k: &str| {
                    let _ = k;
                    Ok(())
                },
                settle: || {},
            },
        )
        .expect("编排应成功");
        assert_eq!(out.pre_read, Some(MamMode::Default));
        assert_eq!(out.observed, Some(MamMode::Default));
    }

    /// **有界轮询内核三态**（2026-10-10 picker Done 核验升级的回归锁）：
    /// ① 队首 N 拍旧态、后出回执行 → 第 N+1 拍命中即停（消费面 = picker Yes 路）；
    /// ② 窗尽无变化 → `(None, 最后读数)`（不挂死，靠 rounds 兜住）；
    /// ③ 中途读不到屏 → 停止（保持最后读数，不空转）。
    #[test]
    fn poll_screen_until_three_outcomes() {
        let old = lines(&["  普通输出"]);
        let hit = lines(&[
            "  普通输出",
            "• Permission selection requested: Full Access",
        ]);
        // ① 队首 2 拍旧态 → 第 3 拍命中（按队序消费，remove(0)）
        let seq = std::cell::RefCell::new(vec![
            Some(old.clone()),
            Some(old.clone()),
            Some(hit.clone()),
        ]);
        let settles = std::cell::Cell::new(0u32);
        let (out, last) = poll_screen_until(
            5,
            || {
                if seq.borrow().is_empty() {
                    None
                } else {
                    Some(seq.borrow_mut().remove(0).unwrap())
                }
            },
            || settles.set(settles.get() + 1),
            |ls| {
                ls.iter()
                    .find(|l| l.contains("Permission selection requested"))
                    .cloned()
            },
        );
        assert_eq!(
            out.as_deref(),
            Some("• Permission selection requested: Full Access")
        );
        assert_eq!(last, hit, "命中拍的行集 = 最后读数");
        assert_eq!(settles.get(), 2, "命中即停：只在两拍之间等待");
        // ② 窗尽无变化 → None + 最后读数
        let n = std::cell::Cell::new(0u32);
        let (out2, last2) = poll_screen_until(
            4,
            || {
                n.set(n.get() + 1);
                Some(old.clone())
            },
            || {},
            |_| None::<String>,
        );
        assert!(out2.is_none(), "窗尽无产物");
        assert_eq!(n.get(), 4, "拍数上界生效（不挂死）");
        assert_eq!(last2, old, "窗尽也带最后读数（screen_tail 消费面）");
        // ③ 第 1 拍读不到屏 → 停止（不轮到第 2 拍）
        let n3 = std::cell::Cell::new(0u32);
        let (out3, last3) = poll_screen_until(
            5,
            || {
                n3.set(n3.get() + 1);
                None
            },
            || {},
            |_| Some("x".to_string()),
        );
        assert!(out3.is_none() && last3.is_empty());
        assert_eq!(n3.get(), 1, "读不到屏即停（不空转）");
    }

    /// **分族的意义**（回归锁）：各家的判据只认自家的屏形态——两家的底栏都带
    /// `(shift+tab to cycle) · `，区别只在 `plan mode on`（claude）vs `Plan mode`（codex）。
    /// 还原动作：把 codex 族的「跳过 `plan mode on`」一行删掉 → 第一条断言先红
    /// （claude 的底栏会被 codex 解析器读成 Plan）。
    #[test]
    fn family_parsers_do_not_bleed_into_each_other() {
        let claude_plan_screen = lines(&["  ⏸ plan mode on (shift+tab to cycle) ·  for agents"]);
        assert_eq!(
            parse_mode_from_screen("claude", &claude_plan_screen),
            Some(MamMode::Plan)
        );
        assert_eq!(
            parse_mode_from_screen("codex", &claude_plan_screen),
            None,
            "claude 的底栏形态不是 codex 的判据（分族解析）"
        );
        let codex_screen =
            lines(&["  glm-5.3-flash medium · ~\\proj-codex  Plan mode (shift+tab to cycle)"]);
        assert_eq!(
            parse_mode_from_screen("codex", &codex_screen),
            Some(MamMode::Plan)
        );
        assert_eq!(
            parse_mode_from_screen("claude", &codex_screen),
            None,
            "codex 的底栏形态不是 claude 的判据"
        );
        // kimi 的底栏喂给 codex：`plan` 是首词（不是 `plan mode`），且 kimi 底栏无
        // ` · `（实测两态都没有）→ codex 族读不出结论
        let kimi_screen = lines(&[" plan  GLM-5.3-Flash thinking: high  C:\\proj-kimi"]);
        assert_eq!(
            parse_mode_from_screen("kimi", &kimi_screen),
            Some(MamMode::Plan)
        );
        assert_eq!(
            parse_mode_from_screen("codex", &kimi_screen),
            None,
            "kimi 的底栏形态不是 codex 的判据"
        );
        assert_eq!(
            parse_mode_from_screen("claude", &kimi_screen),
            None,
            "kimi 的底栏形态不是 claude 的判据"
        );
        // 未实测工具恒 None（旧实现会对任意工具跑词表）
        assert_eq!(parse_mode_from_screen("zcode", &claude_plan_screen), None);
        assert_eq!(parse_mode_from_screen("", &codex_screen), None);
    }

    /// **kimi 2.1.1 底栏双轴解析**（2026-10-06 活体夹具）：底栏以权限英文标签开头
    /// （非 manual 档），`plan` 不再是首词——旧「首词==plan」判据在此恒误读 Default
    /// （用户实机「切计划报预期计划实际默认」的根因）。模式轴 = 分词找独立词 plan；
    /// 权限轴 = 行首标签前缀（manual 不渲染标签 → Default 缺席推断）。
    #[test]
    fn kimi_211_bar_dual_axis_parsing() {
        let manual = e_stage2_screen("kimi-211-bar-manual-plan.txt");
        let yolo = e_stage2_screen("kimi-211-bar-yolo-plan.txt");
        let auto = e_stage2_screen("kimi-211-bar-auto-plan.txt");
        // 模式轴：三态底栏都含独立词 plan → 全 Plan（旧判据对 yolo/auto 态会误读 Default）
        for (name, screen) in [("manual", &manual), ("yolo", &yolo), ("auto", &auto)] {
            assert_eq!(
                parse_mode_from_screen("kimi", screen),
                Some(MamMode::Plan),
                "{name} × plan ON → 模式轴 Plan"
            );
        }
        // 权限轴：manual 缺席推断 Default；yolo → AcceptEdits；auto → Bypass
        assert_eq!(
            parse_axis_from_screen("kimi", ModeGroupId::Permission, &manual),
            Some(MamMode::Default),
            "manual 不渲染权限标签 → Default"
        );
        assert_eq!(
            parse_axis_from_screen("kimi", ModeGroupId::Permission, &yolo),
            Some(MamMode::AcceptEdits)
        );
        assert_eq!(
            parse_axis_from_screen("kimi", ModeGroupId::Permission, &auto),
            Some(MamMode::Bypass)
        );
        // 模式轴在无 plan 词的新版式下 → Default（2.1.1 × yolo × plan OFF 由分词判）
        let plan_off = lines(&["Ask When Needed  DeepSeek V4.1 Flash thinking: high  C:\\p"]);
        assert_eq!(
            parse_mode_from_screen("kimi", &plan_off),
            Some(MamMode::Default)
        );
        // codex 无权限轴回读源（其底栏仍只有模式文本）→ None
        assert_eq!(
            parse_axis_from_screen("codex", ModeGroupId::Permission, &yolo),
            None
        );
        // 无 thinking: 锚 → 两轴都 None（宁可未知不猜）
        let bare = lines(&["Ask When Needed DeepSeek V4.1 Flash"]);
        assert_eq!(parse_mode_from_screen("kimi", &bare), None);
        assert_eq!(
            parse_axis_from_screen("kimi", ModeGroupId::Permission, &bare),
            None
        );
    }

    /// **kimi 权限组底栏回读轮询**（readback:true 放开后的主回归锁）：切到 Never Ask，
    /// 第一拍屏上还是旧档（Ask When Needed）、第二拍底栏刷出 `Never Ask` → Confirmed
    /// （旧版无回读源时此场景恒 Unverifiable、档位记忆永不写入）。
    #[test]
    fn kimi_211_permission_readback_poll_confirms() {
        let stale = e_stage2_screen("kimi-211-bar-yolo-plan.txt");
        let fresh = e_stage2_screen("kimi-211-bar-auto-plan.txt");
        let (out, reads, _) = run_readback_script_group(
            "kimi",
            ModeGroupId::Permission,
            Some(MamMode::Bypass),
            15,
            &[Some(stale.as_slice()), Some(fresh.as_slice())],
        );
        assert_eq!(out.verdict, ModeVerify::Confirmed, "第二拍命中");
        assert_eq!(out.observed, Some(MamMode::Bypass));
        assert_eq!(reads, 2, "第一拍旧档继续轮询、第二拍命中即停");
    }

    // ==== 丁T4 收尾：权限菜单真机夹具（**逐字抄自用户实机取证档案**）====
    //
    // 全部夹具 = `research/refs/phase2-消息注入/2026-09-22-codex-kimi权限菜单与
    // FullAccess三段式-用户实机取证.md` 的屏幕原文（该档是**用户手工实机操作后转抄
    // 的逐字原文**，非 agent 探测脚本产物）。含 `›`(U+203A) / `❯`(U+276F) /
    // `←`(U+2190) / `↑↓·`(U+2191U+2193U+00B7) 等非 ASCII 字符与描述折行——
    // **本批的纪律要求：夹具必须含非 ASCII，必须来自真机**（T2/T4-C1 两次教训）。

    /// codex `/permissions` 菜单（**档案 §1 逐字**；新窗口默认态，高亮在第 2 项；
    /// 尾行 footer 锚 = 真机 dump 逐字——E3① 起菜单定位以标题↔footer 锚窗为前提）。
    fn codex_menu_real() -> Vec<String> {
        lines(&[
            "  Update Model Permissions",
            "",
            "  1. Read Only                   Codex can read files in the current workspace. Approval is required to edit files or",
            "                                 access the internet.",
            "› 2. Ask for approval (current)  Codex can read and edit files in the current workspace, and run commands. Approval is",
            "                                 required to access the internet or edit other files.",
            "  3. Approve for me              Only ask for actions detected as potentially unsafe.",
            "  4. Full Access                 Codex can edit files outside this workspace and access the internet without asking",
            "                                 for approval. Exercise caution when using.",
            "",
            "  Press enter to confirm or esc to go back",
        ])
    }

    /// kimi `/permission` 菜单（**档案 §5 逐字**；高亮在第 1 项，**每档后跟一行描述**）。
    fn kimi_menu_real() -> Vec<String> {
        lines(&[
            "  Select permission mode",
            "  ↑↓ navigate · Enter select · Esc cancel",
            "",
            "   ❯ Always Ask ← current",
            "     Auto-read only; everything else needs your approval first.",
            "     Ask When Needed",
            "     Routine edits and commands run automatically; risky actions, questions, and plans still ask.",
            "     Never Ask",
            "     Never interrupts you; everything is run and decided automatically.",
        ])
    }

    /// **定位判据 = 关键词包含 + 计数互斥**（core 修复）：codex 真机菜单 → **4 项、
    /// 顺序正确、第 2 项 highlighted**；标签是规范标签（与屏上编号/光标标记无关）。
    ///
    /// **还原动作（变异 1）**：把 `locate_menu_items` 改回旧的行首前缀匹配
    /// （`starts_with_ignore_case(text, label)`）→ 本测试**先红**（真机行首是
    /// `› 2. Ask for approval` / `1. Read Only`，标签不在行首 → 定位 0 项 → `None`）。
    /// 这正是实机取证档 §0 表第 1 行的实测结论「codex 定位判据失效（实测 0 项）」。
    #[test]
    fn locate_codex_menu_items_from_real_screen() {
        let screen = codex_menu_real();
        let items =
            locate_menu_items(&screen, menu_labels("codex")).expect("真机 codex 菜单必须解析");
        assert_eq!(
            items.len(),
            4,
            "真机菜单 4 项（编号 `1.`..`4.` 在行首、标签在其后；旧行首匹配在此得 0 项）"
        );
        let labels: Vec<String> = items
            .iter()
            .map(|o| o.label.split_once('|').unwrap().0.to_string())
            .collect();
        assert_eq!(
            labels,
            vec![
                "Read Only",
                "Ask for approval",
                "Approve for me",
                "Full Access"
            ],
            "顺序 = 屏上出现顺序（number 是内部序号，1 起）"
        );
        assert_eq!(
            items.iter().map(|o| o.number).collect::<Vec<_>>(),
            vec![1, 2, 3, 4],
            "内部编号按屏上顺序编（与屏上是否印数字无关）"
        );
        assert!(
            !items[0].highlighted
                && items[1].highlighted
                && !items[2].highlighted
                && !items[3].highlighted,
            "`›` 在第 2 项（Ask for approval (current)）→ 唯一高亮"
        );
        // **描述折行的续行不得成为独立项**：codex 把描述放在**档位行同行的右侧列**，
        // 折行后的续行是独立屏幕行、不含任何档位词（命中 0 个）→ 跳过。逐项断言
        // 「没有任何一项的屏上原文是纯续行」。
        assert!(
            items.iter().all(|o| {
                let text = o.label.split_once('|').map(|(_, t)| t).unwrap_or("");
                !text.starts_with("access the internet")
                    && !text.starts_with("required to access the internet")
                    && !text.starts_with("for approval. Exercise caution")
            }),
            "描述续行（不含档位词）不得被计成菜单项：{items:?}"
        );
        // 过一致性闸（定位输出的**行动前置条件**）
        assert!(menu_items_coherent(&items, "codex"));
    }

    /// **kimi 真机菜单 → 3 项、第 1 项 highlighted**：每档后跟一行描述（档位行**不连续**）
    /// ——任何「连续成簇 + 紧邻标题」的结构假设都会在这里误伤（实机取证档 §5 形态要点）。
    #[test]
    fn locate_kimi_menu_items_from_real_screen() {
        let screen = kimi_menu_real();
        let items =
            locate_menu_items(&screen, menu_labels("kimi")).expect("真机 kimi 菜单必须解析");
        assert_eq!(
            items.len(),
            3,
            "kimi 三档（描述行夹在档位行之间，不得被计进来）"
        );
        let labels: Vec<String> = items
            .iter()
            .map(|o| o.label.split_once('|').unwrap().0.to_string())
            .collect();
        assert_eq!(labels, vec!["Always Ask", "Ask When Needed", "Never Ask"]);
        assert!(
            items[0].highlighted && !items[1].highlighted && !items[2].highlighted,
            "`❯` 在第 1 项（Always Ask ← current）"
        );
        // `← current` 是**标签后缀**（不是独立列）：`Always Ask` 这一项仍然只命中自己
        assert!(
            items[0].label.contains("Always Ask ← current"),
            "屏上原文（含 `← current` 后缀）保留在 label 的后半段：{:?}",
            items[0].label
        );
        assert!(menu_items_coherent(&items, "kimi"));
    }

    /// **干扰行夹具（两条线）**：
    /// 1. **单档命中的警示语**（二进制实证：`We strongly recommend selecting "Ask for
    ///    approval" instead.` 在 `codex.exe` 内出现 1 次）→ 命中 1 个标签 → **会被计为
    ///    菜单项**，但它与真菜单项 `Ask for approval` **重复** → 由一致性闸的**不变式 1**
    ///    拒绝（这是「互斥规则拦多档、一致性闸拦同档第二行」的分工）；
    /// 2. **同时提到两档的散文** → 命中 ≥2 → **跳过**，不影响结果。
    #[test]
    fn distractor_lines_are_handled_by_the_two_gates() {
        // ① 单档警示语（窗内）：新行形判据下**不是 `N. ` 编号行** → 定位阶段直接排除，
        //    不再「计入 5 项后靠一致性闸兜底」（旧关键词判据的形态；E3① 起排除前移）
        let mut screen = codex_menu_real();
        let warning =
            "    We strongly recommend selecting \"Ask for approval\" instead.".to_string();
        let insert_at = screen.len() - 1; // footer 行之前（窗内）
        screen.insert(insert_at, warning.clone());
        let raw = locate_menu_items(&screen, menu_labels("codex")).expect("菜单照常解析");
        assert_eq!(
            raw.len(),
            4,
            "警示语行不是编号行 → 窗内也被行形判据排除（不再计入）"
        );
        assert!(menu_items_coherent(&raw, "codex"), "四真项自洽");
        //（导航行为不在此钉——脚本化单屏会触发「高亮不动」中止，与干扰行无关；
        // 闭环导航的专门测试见 closed_loop_* 族）

        // ② 同时提到两档的散文（窗内）：新行形判据下**不是 `N. ` 编号行** → 不进项表
        let mut screen2 = codex_menu_real();
        screen2.insert(
            screen2.len() - 1,
            "  Compare Read Only with Full Access before choosing.".to_string(),
        );
        let items = locate_menu_items(&screen2, menu_labels("codex")).expect("菜单照常解析");
        assert_eq!(
            items.len(),
            4,
            "两档散文行不是 `N. ` 编号行 → 窗内也不成项（行形判据天然排除）"
        );
        assert!(
            items.iter().all(|o| !o.label.contains("Compare")),
            "散文行不得成为菜单项：{items:?}"
        );

        // ③ **窗外排除锁**（E3① 锚窗语义）：同样的警示语/摘要行放在 footer **之后**
        // → 菜单窗外，定位器零吸收（旧关键词判据对整屏扫描会把它算进去）
        let mut screen3 = codex_menu_real();
        screen3.push(warning);
        screen3.push("  1. 构建工具:npm".to_string());
        let items3 = locate_menu_items(&screen3, menu_labels("codex")).expect("菜单照常解析");
        assert_eq!(
            items3.len(),
            4,
            "footer 之后的行一律不参与定位（锚窗纪律——busy 混屏不污染菜单块）"
        );
        assert!(menu_items_coherent(&items3, "codex"));
    }

    /// **编号不再是判据**（锁）：把同一份 codex 真机菜单的编号整体删掉（kimi 化——
    /// 只留缩进与光标标记）→ **仍定位 4 项**。
    ///
    /// **还原动作（变异 2）**：把定位判据改成「编号 + 标签」（如先
    /// `parse_option_line` 再在文本里找标签）→ 本测试先红（编号删掉后一项都认不出）。
    #[test]
    fn numbering_is_not_a_criterion() {
        // 逐行去掉 `N. ` 前缀（保留光标标记与缩进）——即 kimi 的形态
        let unnumbered: Vec<String> = codex_menu_real()
            .iter()
            .map(|l| {
                // 只处理形如 `[› ]  1. Read Only …` 的行；其余原样
                let (rest, hl) = crate::inject::dialog::strip_cursor_marker(l);
                let t = rest.trim_start();
                let stripped = match t.split_once(". ") {
                    Some((n, tail)) if n.chars().all(|c| c.is_ascii_digit()) && !n.is_empty() => {
                        tail.to_string()
                    }
                    _ => t.to_string(),
                };
                if stripped == t && !t.contains("Read Only") && !t.contains("Full Access") {
                    return l.clone(); // 非档位行（标题/描述）原样保留
                }
                format!("{}{}", if hl { "› " } else { "   " }, stripped)
            })
            .collect();
        // 夹具自检：编号确实没了（否则本测试证伪不了行形依赖）
        assert!(
            !unnumbered.iter().any(|l| {
                let t = l.trim_start_matches(['›', ' ']);
                t.starts_with("1. ") || t.starts_with("2. ")
            }),
            "夹具构造失败：编号没有被删干净：{unnumbered:?}"
        );
        // E3① 行形判据的两侧：codex 窗内**必须**是 `N. ` 编号行——删号后行形不匹配
        // → 如实返回 None（不定位）；kimi 侧则天然无编号（locate_kimi_menu_items_from_
        // real_screen 已锁）。编号对 codex 是**行形的一部分**，不是可选判据
        assert!(
            locate_menu_items(&unnumbered, menu_labels("codex")).is_none(),
            "codex 窗内无编号行形 → 不定位（如实返回，不猜）"
        );
    }

    // ==== E3② 守卫收窄（裁17）判定表锁 ====

    /// **busy 放行锁 ×2 + 待决拒绝锁 ×2**（判定表见 [`mode_switch_block`]）。
    ///
    /// 还原动作（变异）：在判定表加 `if is_running { return Some(..) }`（恢复旧
    /// 「busy 拦」）→ busy 放行两格先红；删 kimi 待决格 → 待决拒绝格先红。
    #[test]
    fn mode_switch_block_truth_table() {
        // busy 放行锁①：codex 权限组 × 运行中（Full Access 二段确认照常弹）→ 放行
        assert_eq!(
            mode_switch_block(
                "codex",
                ModeGroupId::Permission,
                MamMode::Bypass,
                true,
                false,
                false,
            ),
            None,
            "busy 切权限不拦（裁17：CX-1/2 实测直接生效不打断）"
        );
        // busy 放行锁②：kimi 权限组 × 运行中 → 放行
        assert_eq!(
            mode_switch_block(
                "kimi",
                ModeGroupId::Permission,
                MamMode::AcceptEdits,
                true,
                false,
                false,
            ),
            None,
            "busy 切权限不拦（裁17：K-2 实测运行中可切不打断）"
        );
        // 待决拒绝锁①：kimi × 问答待决 → 含回车整条硬拒绝（Enter 误选污染待决态）
        assert_eq!(
            mode_switch_block(
                "kimi",
                ModeGroupId::Permission,
                MamMode::AcceptEdits,
                false,
                false,
                true,
            ),
            Some(SwitchBlock::QuestionPending),
            "kimi 待决态 = 硬拒绝（戊探D ×2：Enter 误选推进 Review）"
        );
        // 待决拒绝锁②：kimi 待决态**空闲**同样拒绝（危害与 busy 无关）
        assert_eq!(
            mode_switch_block("kimi", ModeGroupId::Mode, MamMode::Plan, false, false, true,),
            Some(SwitchBlock::QuestionPending),
        );
        // codex 问答待决不落本格（其弹窗是编号对话框 → 落 Dialog 格）
        assert_ne!(
            mode_switch_block(
                "codex",
                ModeGroupId::Permission,
                MamMode::ReadOnly,
                false,
                true,
                false,
            ),
            None,
        );
        assert_eq!(
            mode_switch_block(
                "codex",
                ModeGroupId::Permission,
                MamMode::ReadOnly,
                false,
                false,
                true,
            ),
            None,
            "codex 待决依赖对话框判据（编号弹窗可屏读），不落 QuestionPending 格"
        );
        // codex /plan 运行中（产品限制格）
        assert_eq!(
            mode_switch_block(
                "codex",
                ModeGroupId::Mode,
                MamMode::Plan,
                true,
                false,
                false,
            ),
            Some(SwitchBlock::CodexPlanBusy),
        );
        // 对话框在场格优先级最高（守卫前置）
        assert_eq!(
            mode_switch_block(
                "kimi",
                ModeGroupId::Permission,
                MamMode::AcceptEdits,
                true,
                true,
                true,
            ),
            Some(SwitchBlock::Dialog),
        );
        // 空闲 + 无对话框 + 无待决 → 放行
        assert_eq!(
            mode_switch_block(
                "claude",
                ModeGroupId::Mode,
                MamMode::Plan,
                false,
                false,
                false,
            ),
            None,
        );
    }

    // ==== E3①：锚点分家定位 × 真机夹具（戊探D evidence 原件）====

    /// e-stage2 屏读夹具读取（confirm/dialog tests 同款；单一事实源=夹具文件本身）
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

    /// **codex 空闲菜单 × 真机夹具**：4 项、`(current)` 行高亮、上方历史摘要编号行
    /// （`1. 构建工具:npm`——旧关键词判据的最现实混入源，戊探D §2.4-5）零吸收。
    #[test]
    fn e3_codex_idle_menu_fixture_parses_without_pollution() {
        let screen = e_stage2_screen("codex-perm-menu-idle.txt");
        let items = locate_menu_items(&screen, menu_labels("codex")).expect("真机空闲菜单必须解析");
        assert_eq!(items.len(), 4);
        let labels: Vec<String> = items
            .iter()
            .map(|o| o.label.split_once('|').unwrap().0.to_string())
            .collect();
        assert_eq!(
            labels,
            vec![
                "Read Only",
                "Ask for approval",
                "Approve for me",
                "Full Access"
            ]
        );
        assert!(items[1].highlighted, "› 在第 2 项");
        assert!(
            items.iter().all(|o| !o.label.contains("构建工具")),
            "上方历史摘要的编号行（窗外）不得被吸收：{items:?}"
        );
        assert!(menu_items_coherent(&items, "codex"));
    }

    /// **codex busy 菜单 × 真机夹具**：流式正文与菜单块同屏（戊探D D③ busy 形态），
    /// 菜单块自身解析不受滚动正文污染（裁17：busy 态照常解析、守卫不拦）。
    #[test]
    fn e3_codex_busy_menu_fixture_not_polluted() {
        let screen = e_stage2_screen("codex-perm-menu-busy.txt");
        let items = locate_menu_items(&screen, menu_labels("codex")).expect("busy 菜单必须解析");
        assert_eq!(items.len(), 4);
        assert!(items[1].highlighted);
        assert!(menu_items_coherent(&items, "codex"));
    }

    /// **★ N6 收口锁（kimi「总是询问」可达）× 真机夹具**：`/permission` 菜单
    /// （戊探D kimi 段原件）——描述行 `Never interrupts you; …`（含 `Never` 子串，
    /// 旧关键词判据的精确触发行）被行形判据正确归属为描述行；三档项唯一 → 一致性闸
    /// 通过 → 目标「Always Ask」的探测 Ready（= 总是询问跨折行可达，N6 关闭）。
    ///
    /// 还原动作（变异）：把 [`locate_menu_items`] 换回旧「关键词 contains 整屏扫描」
    /// → `Never Ask` 重复命中 → 一致性闸拒绝 → probe 落 Fatal，本测试先红。
    #[test]
    fn e3_kimi_menu_fixture_n6_always_ask_reachable() {
        let screen = e_stage2_screen("kimi-perm-menu-permission.txt");
        let items =
            locate_menu_items(&screen, menu_labels("kimi")).expect("真机 kimi 菜单必须解析");
        assert_eq!(items.len(), 3, "三档项；两行组描述行不计入");
        let labels: Vec<String> = items
            .iter()
            .map(|o| o.label.split_once('|').unwrap().0.to_string())
            .collect();
        assert_eq!(labels, vec!["Always Ask", "Ask When Needed", "Never Ask"]);
        assert!(
            !items.iter().any(|o| o.label.contains("interrupts")),
            "N6 失败行 `Never interrupts you;…` 必须归属为描述行（不是菜单项）"
        );
        assert!(
            items[0].label.contains("← current"),
            "当前档后缀保留在屏上原文里"
        );
        assert!(items[0].highlighted, "❯ 在 Always Ask 行（夹具态的预选）");
        assert!(
            menu_items_coherent(&items, "kimi"),
            "三档唯一 → 自洽（N6 关闭）"
        );
        // 行动入口：目标=「总是询问」（Default）的探测必须 Ready（旧判据在此 Fatal）
        let plan = MenuNavPlan::PermissionTier {
            tool: "kimi",
            target: MamMode::Default,
        };
        match plan.probe(&screen) {
            PollStep::Ready((_, target)) => assert_eq!(target, "Always Ask"),
            other => panic!("总是询问必须可达（N6 关闭面）：{other:?}"),
        }
    }

    /// **kimi 预选差异 × 真机夹具**：`/yolo` 预选 `Ask When Needed`（❯ 随预选移动）、
    /// `/auto` 预选 `Never Ask`；`← current` 恒标 `Always Ask`（当前档不随光标动——
    /// 用户 K-1 实测的双标记语义）。
    #[test]
    fn e3_kimi_yolo_auto_fixtures_highlight_preselect() {
        for (name, tier_idx) in [
            ("kimi-perm-menu-yolo.txt", 1usize),
            ("kimi-perm-menu-auto.txt", 2),
        ] {
            let screen = e_stage2_screen(name);
            let items = locate_menu_items(&screen, menu_labels("kimi"))
                .unwrap_or_else(|| panic!("{name} 必须解析"));
            assert_eq!(items.len(), 3, "{name}");
            assert!(
                items[tier_idx].highlighted,
                "{name}：❯ 应在预选档（第 {} 项）",
                tier_idx + 1
            );
            assert!(
                !items[0].highlighted || tier_idx == 0,
                "{name}：❯ 不得停在 Always Ask（预选已移走）"
            );
            assert!(
                items[0].label.contains("← current"),
                "{name}：← current 恒标当前档 Always Ask"
            );
            assert!(menu_items_coherent(&items, "kimi"), "{name}");
        }
    }

    /// **无标题锚 → 不定位**（锚窗前提）：菜单行在屏但标题缺失（被刷走/遮挡）→ None，
    /// 不 guess（调用方如实中止）
    #[test]
    fn e3_no_title_anchor_no_locate() {
        let bare = lines(&[
            "   ❯ Always Ask ← current",
            "     Auto-read only.",
            "     Ask When Needed",
            "     Routine edits.",
            "     Never Ask",
            "     Never interrupts you.",
        ]);
        assert!(locate_menu_items(&bare, menu_labels("kimi")).is_none());
    }

    // ==== 丁T4 收尾：**闭环导航**（每步屏读复核；脚本化屏幕序列驱动）====

    /// **脚本化导航驱动器**：把一串「屏序列」喂给闭环，收集**实际发出的按键**。
    ///
    /// 构造方式（闭环的参数化设计正为此）：`read` 闭包每次调用返回脚本里的下一屏
    /// （用尽后重复最后一屏——模拟「按键没生效，屏没变」）；`send` 闭包记录按键；
    /// `settle` 空操作（测试不需要真等重绘）。返回 `(闭环结果, 实际发出的键序)`。
    fn run_scripted_nav(
        first: &[String],
        tool: &str,
        target: MamMode,
        following: &[&[String]],
    ) -> (Result<Vec<String>, String>, Vec<String>) {
        let mut queue: Vec<Vec<String>> = Vec::new();
        queue.push(first.to_vec());
        queue.extend(following.iter().map(|s| s.to_vec()));
        let mut idx = 0usize;
        let mut sent: Vec<String> = Vec::new();
        let plan = MenuNavPlan::PermissionTier { tool, target };
        let mut terminal = Closures {
            read: || {
                let v = queue[idx.min(queue.len() - 1)].clone();
                idx += 1;
                Some(v)
            },
            send: |k: &str| {
                sent.push(k.to_string());
                Ok(())
            },
            settle: || {},
        };
        let r = navigate_until_highlighted(&mut terminal, &plan);
        (r, sent)
    }

    /// 把 `codex_menu_real()` 的高亮**移到第 `hl` 项**（1 起）——构造脚本化屏幕序列用。
    /// 返回的屏**不是**真机原文的逐字复制（高亮位置是人为摆的），故只用于导航逻辑测试；
    /// 真机夹具见上面三条定位测试。
    fn codex_menu_with_highlight(hl: usize) -> Vec<String> {
        let mut screen = codex_menu_real();
        for (i, l) in screen.iter_mut().enumerate() {
            let (_, is_hl) = crate::inject::dialog::strip_cursor_marker(l);
            if !is_hl {
                continue;
            }
            // 把 `› ` 换成两个空格
            *l = l.replacen('\u{203a}', " ", 1);
            let _ = i;
        }
        // 在第 hl 项的档位行前加 `› `
        let mut count = 0usize;
        for l in screen.iter_mut() {
            let (rest, _) = crate::inject::dialog::strip_cursor_marker(l);
            let t = rest.trim();
            let is_tier = menu_labels("codex").iter().any(|lab| t.contains(lab));
            if !is_tier {
                continue;
            }
            count += 1;
            if count == hl {
                *l = format!("›{}", rest);
                break;
            }
        }
        screen
    }

    /// **闭环导航：目标在下方 k 步**——发 `[↓×k, enter]`，且**每步都复核过**。
    ///
    /// 脚本：第 2 屏 = 高亮在第 3 项、第 3 屏 = 第 4 项（每次 `↓` 都真的移动）。
    /// 断言键序 + 断言「回车是最后且仅有一个」——**唯一提交点**的执行证据。
    #[test]
    fn closed_loop_navigates_down_step_by_step() {
        let s1 = codex_menu_with_highlight(2); // 起点：Ask for approval（实机默认）
        let s2 = codex_menu_with_highlight(3);
        let s3 = codex_menu_with_highlight(4); // 目标：Full Access
        let (r, sent) = run_scripted_nav(&s1, "codex", MamMode::Bypass, &[&s2, &s3]);
        assert_eq!(r.unwrap(), vec!["down", "down", "enter"], "两步下发两个 ↓");
        assert_eq!(sent, vec!["down", "down", "enter"], "实际发出的键序");
        assert_eq!(
            sent.iter().filter(|k| *k == "enter").count(),
            1,
            "回车只发一次，且只在高亮确实落在目标行之后"
        );
    }

    /// **闭环导航：目标在上方**——发 `[↑×k, enter]`（不假设回卷；与审批路径的
    /// `navigation_sequence_directional` 同口径，但逐步复核）。
    #[test]
    fn closed_loop_navigates_up_step_by_step() {
        let s1 = codex_menu_with_highlight(3); // Approve for me
        let s2 = codex_menu_with_highlight(2);
        let s3 = codex_menu_with_highlight(1); // Read Only
        let (r, sent) = run_scripted_nav(&s1, "codex", MamMode::ReadOnly, &[&s2, &s3]);
        assert_eq!(r.unwrap(), vec!["up", "up", "enter"]);
        assert_eq!(sent, vec!["up", "up", "enter"]);
    }

    /// **高亮已在目标行**（实机常见：用户点的正是当前档）→ 零方向键、直接 `enter`。
    #[test]
    fn closed_loop_enters_immediately_when_already_on_target() {
        let s1 = codex_menu_with_highlight(2);
        let (r, sent) = run_scripted_nav(&s1, "codex", MamMode::Default, &[&s1]);
        assert_eq!(r.unwrap(), vec!["enter"]);
        assert_eq!(sent, vec!["enter"]);
    }

    /// **混入行在中间**（本任务点名的形态）：按一次 `↓` 后高亮**跳到我们表里的 +2 项**
    /// （模拟「表与实际屏不一致」——我们没数进去的行把两步并成一步）→ **中止且不发 enter**。
    ///
    /// 这是闭环的**核心安全价值**：旧实现（一次算步进 + 盲发）在这里会把剩下的键
    /// 全发完并回车——落到错档。
    #[test]
    fn closed_loop_aborts_when_one_step_jumps_two_rows() {
        let s1 = codex_menu_with_highlight(2);
        // 真实屏上多了一行我们没数进去的（比如某档描述折到了独立行 + 恰好命中档位词）
        // → 终端的一步 = 我们的两步
        let s2 = codex_menu_with_highlight(4);
        let (r, sent) = run_scripted_nav(&s1, "codex", MamMode::Bypass, &[&s2]);
        let err = r.as_ref().unwrap_err();
        assert!(
            err.contains("位移") && err.contains("2 行"),
            "位移 ≠ 1 必须点名（含阶段名与实测位移）：{err}"
        );
        assert!(
            !sent.contains(&"enter".to_string()),
            "**绝不盲提交**：中止时未发出回车（实际发出：{sent:?}）"
        );
    }

    /// **高亮不动 → 中止**（「终端可能未响应」）：脚本让 `↓` 后屏不变。
    #[test]
    fn closed_loop_aborts_when_highlight_does_not_move() {
        let s1 = codex_menu_with_highlight(2);
        let (r, sent) = run_scripted_nav(&s1, "codex", MamMode::Bypass, &[&s1]);
        let err = r.as_ref().unwrap_err();
        assert!(
            err.contains("高亮仍停在") && err.contains("终端可能未响应"),
            "要给用户看得懂的原因：{err}"
        );
        assert!(!sent.contains(&"enter".to_string()), "未发回车：{sent:?}");
    }

    /// 无高亮 / 多高亮 → 中止（**不猜起点**，与审批路径同口径，但文案带阶段名）
    #[test]
    fn closed_loop_refuses_without_unique_highlight() {
        // 无高亮
        let mut no_hl = codex_menu_real();
        for l in no_hl.iter_mut() {
            *l = l.replacen('\u{203a}', " ", 1);
        }
        let (r, sent) = run_scripted_nav(&no_hl, "codex", MamMode::Bypass, &[&no_hl]);
        assert!(
            r.as_ref().unwrap_err().contains("认不出当前高亮行"),
            "{r:?}"
        );
        assert!(sent.is_empty(), "零按键：{sent:?}");
        // 多高亮（E3① 起：dup 行插在**菜单窗内**——窗外行被锚窗排除，测不到多高亮）
        let mut multi = codex_menu_real();
        multi.insert(multi.len() - 1, "› 5. Read Only (dup)".to_string());
        let (r2, sent2) = run_scripted_nav(&multi, "codex", MamMode::Bypass, &[&multi]);
        assert!(r2.is_err(), "多高亮 → 中止：{r2:?}");
        assert!(sent2.is_empty(), "零按键：{sent2:?}");
    }

    /// **迭代上限**（防死循环）：脚本让每次 `↓` 都移动 1 行、但**永远到不了目标**
    /// （用一份人为的、目标档标签出现在屏上但高亮永远走不到它的行表：目标在第 4 项，
    /// 脚本循环 1→2→3→2→3…）。
    #[test]
    fn closed_loop_aborts_over_iteration_limit() {
        // 一屏只有三档（Read Only / Ask for approval / Approve for me）+ 目标 Full Access
        // **不在**任何行上 → `snapshot` 的「目标行不在表里」会先中止。
        // 为覆盖「上限」这条路径，改用「目标在表里但高亮永远在它下面来回」：
        // 用真机四档菜单，目标 = Approve for me（第 3 项），脚本让高亮在 1↔2 之间振荡。
        let s1 = codex_menu_with_highlight(1);
        let s2 = codex_menu_with_highlight(2);
        // 之后一直停在 2（每次 ↓ 都从 2 走到……不，2 之后是 3 —— 用 2 恒定制造「高亮不动」
        // 会走「不动」分支。真正制造「永远走不到」需两屏交替且方向来回——用 1→2→1→2…
        let (r, sent) = run_scripted_nav(
            &s1,
            "codex",
            MamMode::Plan, // codex 权限菜单没有 Plan 档（menu_target_label → None）
            &[&s2],
        );
        // 这一条覆盖的是「目标档标签在词表里、但不在屏上的档位表里」——codex 权限
        // 菜单没有 Plan 档（menu_target_label 返回 None）→ 首次即中止，零按键
        // （2026-09-23 起原用的 AcceptEdits 已成第 3 档「Approve for me」，不再适用）
        assert!(r.is_err(), "{r:?}");
        assert!(sent.is_empty(), "零按键：{sent:?}");

        // 上限路径：目标 = Full Access（第 4 项），脚本让高亮只在 1↔2 之间挪（始终
        // 朝目标方向下走一步后又「回退」——实际是模拟「屏上行序与我们表不一致到
        // 每步都位移 -1/+1 交替」）。构造：s1 高亮 1 → ↓ 后 s2 高亮 2 → ↓ 后 s3 高亮 1 …
        let s1b = codex_menu_with_highlight(1);
        let s2b = codex_menu_with_highlight(2);
        let s3b = codex_menu_with_highlight(1);
        let s4b = codex_menu_with_highlight(2);
        let s5b = codex_menu_with_highlight(1);
        let s6b = codex_menu_with_highlight(2);
        let s7b = codex_menu_with_highlight(1);
        let script: Vec<&[String]> =
            vec![&s2b, &s3b, &s4b, &s5b, &s6b, &s7b, &s3b, &s4b, &s5b, &s6b];
        let (r2, sent2) = run_scripted_nav(&s1b, "codex", MamMode::Bypass, &script);
        let err = r2.as_ref().unwrap_err();
        assert!(
            err.contains("超过上限"),
            "1↔2 振荡永远到不了第 4 项 → 上限中止（菜单 4 项 ⇒ 上限 6）：{err}"
        );
        assert!(
            sent2.len() <= 7,
            "方向键数不得超过上限+1（含触发上限检查的那一次）：{sent2:?}"
        );
        assert!(!sent2.contains(&"enter".to_string()), "未发回车：{sent2:?}");
    }

    /// 读不到屏（`None`）→ 中止，零按键（首屏与步进后各一格）
    #[test]
    fn closed_loop_aborts_when_screen_unreadable() {
        let plan = MenuNavPlan::PermissionTier {
            tool: "codex",
            target: MamMode::Bypass,
        };
        let mut sent: Vec<String> = Vec::new();
        let mut blind = Closures {
            read: || None,
            send: |k: &str| {
                sent.push(k.to_string());
                Ok(())
            },
            settle: || {},
        };
        let r = navigate_until_highlighted(&mut blind, &plan);
        assert!(r.as_ref().unwrap_err().contains("读不到屏幕"), "{r:?}");
        assert!(sent.is_empty());
        // 步进后读不到：首屏正常、第二次读屏 None
        let s1 = codex_menu_with_highlight(2);
        let mut idx = 0;
        let mut sent2: Vec<String> = Vec::new();
        let mut half_blind = Closures {
            read: || {
                idx += 1;
                if idx == 1 {
                    Some(s1.clone())
                } else {
                    None
                }
            },
            send: |k: &str| {
                sent2.push(k.to_string());
                Ok(())
            },
            settle: || {},
        };
        let r2 = navigate_until_highlighted(&mut half_blind, &plan);
        assert!(
            r2.as_ref().unwrap_err().contains("步进后读不到屏幕"),
            "{r2:?}"
        );
        assert_eq!(sent2, vec!["down"], "只发了那一步方向键，**没有回车**");
    }

    /// 未实测工具没有菜单词表 → 恒 Err（不出手，零按键）
    #[test]
    fn menu_labels_empty_for_untested_tools() {
        assert!(menu_labels("zcode").is_empty());
        assert_eq!(menu_target_label("zcode", MamMode::Bypass), None);
        let screen = lines(&["› Full Access", "   Read Only"]);
        let (r, sent) = run_scripted_nav(&screen, "zcode", MamMode::Bypass, &[&screen]);
        assert!(r.is_err(), "未实测工具 → 不出手：{r:?}");
        assert!(sent.is_empty(), "零按键");
    }

    /// 目标档不在屏上（kimi 菜单喂 codex 的目标）→ 中止（不猜位置），零按键
    #[test]
    fn closed_loop_refuses_target_absent_from_screen() {
        let screen = kimi_menu_real();
        let (r, sent) = run_scripted_nav(&screen, "kimi", MamMode::ReadOnly, &[&screen]);
        assert!(r.is_err(), "kimi 菜单没有只读档 → 中止：{r:?}");
        assert!(sent.is_empty());
    }

    // ==== 丁T4 收尾：第三段（Full Access 确认框）与成功回执核验 ====

    /// codex **Full Access 二次确认框**（**档案 §2 逐字**；高亮在第 1 项）。
    fn full_access_confirm_real() -> Vec<String> {
        lines(&[
            " Enable full access?",
            "  When Codex runs with full access, it can edit any file on your computer and run commands with network, without your",
            "  approval. Exercise caution when enabling full access. This significantly increases the risk of data loss, leaks, or",
            "  unexpected behavior.",
            "",
            "› 1. Yes, continue anyway  Apply full access for this session",
            "  2. Cancel                Go back without enabling full access",
        ])
    }

    /// **第三段定位**：肯定项 = **唯一**含 `continue` 的项；说明文里满是小写
    /// `full access` 也**不干扰**（这正是「不用菜单定位器」的理由——确认框用编号对话框
    /// 解析器）。
    #[test]
    fn full_access_confirm_locates_affirmative() {
        let screen = full_access_confirm_real();
        let plan = MenuNavPlan::ConfirmAffirmative {
            tool: "codex",
            keyword: FULL_ACCESS_AFFIRMATIVE_KEYWORD,
        };
        match plan.probe(&screen) {
            PollStep::Ready((items, target)) => {
                assert_eq!(items.len(), 2, "确认框是 2 项标准对话框");
                assert_eq!(
                    target,
                    "Yes, continue anyway  Apply full access for this session"
                );
                assert!(items[0].highlighted && !items[1].highlighted);
            }
            other => panic!("真机确认框必须解析出肯定项：{other:?}"),
        }
    }

    /// **真实形态的关键发现 + 为什么确认框不用 `parse_dialog_options`**。
    ///
    /// # 发现（本测试夹具 = 档案 §1/§2 逐字原文）：编号对话框解析器**吃不下** codex
    /// 权限菜单——菜单每档的描述折行是**独立的屏幕行**（非选项行），按
    /// [`crate::inject::dialog::parse_dialog_clusters`] 的规则「非选项行切断簇并重置期待
    /// 编号」，第 2/3/4 档的行既不是 1 也不是被期待的编号 → 全被丢弃，整屏只剩
    /// **一个 1 项簇** → `parse_dialog_options`（下界 ≥2）在真机菜单上返回 **None**。
    /// 这条本身就是「菜单定位必须用关键词包含判据」的**第二条**依据（第一条是实机
    /// 取证档 §0 表第 1 行的「行首匹配实测 0 项」）。
    ///
    /// # 为什么确认框仍不能用「取最长簇」
    ///
    /// 确认框与菜单同屏时（overlay），簇是 `[1,1,1,1,2]`——此刻「最长 = 确认框」只是
    /// **恰好**：一旦某版本把菜单渲染成不带折行的连续 4 行（4 项的簇），最长簇就会变成
    /// **菜单**，取最长即跳过确认框 → 永远切不了 Full Access。故确认框的定位判据是
    /// 「哪一簇里**恰好有一个**含肯定项关键词的选项」（[`confirm_box_probe`]）——
    /// 与簇长无关，两种渲染形态下都成立。
    #[test]
    fn full_access_confirm_beats_menu_overlay() {
        // ① 真机菜单单独在场：编号对话框解析器**返回 None**（每档被描述折行切成 1 项簇）
        let menu = codex_menu_real();
        assert!(
            crate::inject::dialog::parse_dialog_options(&menu).is_none(),
            "真机 codex 菜单的档位行被描述折行切成单行簇 → 编号解析器吃不下（本发现是\
             「菜单定位必须用关键词判据」的第二条依据）"
        );
        let clusters = crate::inject::dialog::parse_dialog_clusters(&menu);
        assert_eq!(
            clusters.len(),
            1,
            "描述折行切断簇后，只有第 1 行的 `1. Read Only` 能起簇；其后的 2/3/4 行既不是 \
             1 也不是期待的编号（期待值已被切断重置为 1）→ 被丢弃。整个真机菜单在编号解析器\
             眼里只剩 1 个 1 项簇 ⇒ `parse_dialog_options` 必然 None（这条锁住「菜单定位不能\
             复用编号解析器」）"
        );
        assert_eq!(clusters[0].len(), 1, "{clusters:?}");

        // ② overlay（菜单 + 确认框）：逐簇找**不依赖簇长**——命中确认框
        let mut screen = menu.clone();
        screen.extend(full_access_confirm_real());
        let plan = MenuNavPlan::ConfirmAffirmative {
            tool: "codex",
            keyword: FULL_ACCESS_AFFIRMATIVE_KEYWORD,
        };
        match plan.probe(&screen) {
            PollStep::Ready((items, target)) => {
                assert_eq!(
                    target,
                    "Yes, continue anyway  Apply full access for this session"
                );
                assert_eq!(items.len(), 2, "命中的是确认框那一簇（2 项）");
                assert!(items[0].highlighted);
            }
            other => panic!("逐簇找必须命中确认框：{other:?}"),
        }

        // ③ **反向形态**（本判据的存在理由）：菜单是**连续 4 项簇**（不带折行的版本）
        // → 逐簇找仍然命中确认框。**E2② 起 `parse_dialog_options` 也改「离底栏最近
        // 合格簇」**——overlay 形态下它同样选中确认框（2 项，活动对话框恒在底部），
        // 与旧「取最长」（选菜单 4 项）分道；`parse_dialog_clusters` 仍是逐簇关键词
        // 扫描与簇位置的内层视图（菜单路径消费的是它，不是 parse_dialog_options）
        let mut contiguous = lines(&[
            "  1. Read Only",
            "  2. Ask for approval (current)",
            "  3. Approve for me",
            "  4. Full Access",
        ]);
        contiguous.extend(full_access_confirm_real());
        assert_eq!(
            crate::inject::dialog::parse_dialog_clusters(&contiguous)
                .iter()
                .map(|c| c.len())
                .collect::<Vec<_>>(),
            vec![4, 2],
            "簇长 [4, 2]：逐簇关键词扫描的输入形态"
        );
        let bottom_most = crate::inject::dialog::parse_dialog_options(&contiguous).expect("有簇");
        assert_eq!(
            bottom_most.len(),
            2,
            "E2② 离底栏最近：overlay 下选中确认框（活动对话框恒在底部）"
        );
        assert!(
            bottom_most.iter().any(|o| o.label.contains("continue")),
            "底部簇就是确认框（含肯定项）"
        );
        match plan.probe(&contiguous) {
            PollStep::Ready((items, target)) => {
                assert_eq!(
                    target,
                    "Yes, continue anyway  Apply full access for this session"
                );
                assert_eq!(items.len(), 2);
            }
            other => panic!("逐簇找在 [4,2] 形态下仍必须命中确认框：{other:?}"),
        }
    }

    /// **肯定项不唯一 / 不存在 → 立即中止（零投递）**
    #[test]
    fn full_access_confirm_refuses_ambiguous_or_missing() {
        let plan = MenuNavPlan::ConfirmAffirmative {
            tool: "codex",
            keyword: FULL_ACCESS_AFFIRMATIVE_KEYWORD,
        };
        // ① 同一簇内两个含 `continue` 的项（二进制有 `Continue and don't warn again.`
        //    变体文案）→ Fatal（不猜，零投递）
        let two = lines(&[
            " Enable full access?",
            "› 1. Yes, continue anyway",
            "  2. Continue and don't warn again",
            "  3. Cancel",
        ]);
        match plan.probe(&two) {
            PollStep::Fatal(e) => assert!(e.contains("有 2 个"), "{e}"),
            other => panic!("两个肯定项必须 Fatal：{other:?}"),
        }
        // ② 没有任何含 `continue` 的项 → NotYet（**不是 Fatal**：可能用户关过该警告，
        //    调用方按「不当作失败」处理）
        let none = lines(&[" Enable full access?", "› 1. Yes", "  2. Cancel"]);
        match plan.probe(&none) {
            PollStep::NotYet(e) => assert!(e.contains("未见"), "{e}"),
            other => panic!("无肯定项是 NotYet（可继续走回执核验）：{other:?}"),
        }
        // ③ 两个**不同簇**各含一个肯定项 → Fatal（分不清哪个是确认框）
        let two_clusters = lines(&[
            "› 1. Yes, continue anyway",
            "  2. Cancel",
            "prose breaks the cluster",
            "› 1. Continue and don't warn again",
            "  2. Cancel",
        ]);
        match plan.probe(&two_clusters) {
            PollStep::Fatal(e) => assert!(e.contains("多个含"), "{e}"),
            other => panic!("两个簇必须 Fatal：{other:?}"),
        }
    }

    /// **第三段闭环**：从确认框的**实际高亮位**出发导航到肯定项 → `enter`。
    /// 脚本：确认框高亮在第 2 项（Cancel）→ `↑` 到第 1 项 → 提交。
    #[test]
    fn full_access_confirm_closed_loop_navigates_up() {
        let s1 = lines(&[
            " Enable full access?",
            "  1. Yes, continue anyway  Apply full access for this session",
            "› 2. Cancel                Go back without enabling full access",
        ]);
        let s2 = full_access_confirm_real(); // 高亮回到第 1 项（↑ 生效）
        let plan = MenuNavPlan::ConfirmAffirmative {
            tool: "codex",
            keyword: FULL_ACCESS_AFFIRMATIVE_KEYWORD,
        };
        let mut idx = 0usize;
        let mut sent: Vec<String> = Vec::new();
        let mut terminal = Closures {
            read: || {
                let v = if idx == 0 { s1.clone() } else { s2.clone() };
                idx += 1;
                Some(v)
            },
            send: |k: &str| {
                sent.push(k.to_string());
                Ok(())
            },
            settle: || {},
        };
        let r = navigate_until_highlighted(&mut terminal, &plan);
        assert_eq!(r.unwrap(), vec!["up", "enter"], "目标在上方 → ↑×1 + enter");
        assert_eq!(sent, vec!["up", "enter"]);
    }

    /// **成功回执核验**（所有档位）：锚 + 目标档标签**同时**命中才算成功。
    ///
    /// 实机原文（档案 §3 / §5.1–5.3）逐字入夹具，含 `•`(U+2022)。
    #[test]
    fn permission_receipt_requires_anchor_and_target_label() {
        // codex：1/2/3 与 4 档的回执（档案 §3 + §2 末尾）
        let codex_full = lines(&["• Permissions updated to Full Access"]);
        assert!(permission_receipt_verified(
            "codex",
            &codex_full,
            "Full Access"
        ));
        assert!(
            !permission_receipt_verified("codex", &codex_full, "Read Only"),
            "回执说的是 Full Access → 不得当成 Read Only 的成功证据（防旧回执冒充）"
        );
        let codex_ao = lines(&["• Permissions updated to Ask for approval"]);
        assert!(permission_receipt_verified(
            "codex",
            &codex_ao,
            "Ask for approval"
        ));
        // kimi（档案 §5.1–5.3）
        assert!(permission_receipt_verified(
            "kimi",
            &lines(&["Permission mode: Always Ask"]),
            "Always Ask"
        ));
        assert!(permission_receipt_verified(
            "kimi",
            &lines(&["Permission mode: Never Ask"]),
            "Never Ask"
        ));
        assert!(!permission_receipt_verified(
            "kimi",
            &lines(&["Permission mode: Never Ask"]),
            "Always Ask"
        ));
        // 只有锚没有档位词 / 只有档位词没有锚 → 都不算
        assert!(!permission_receipt_verified(
            "codex",
            &lines(&["Permissions updated to"]),
            "Full Access"
        ));
        assert!(!permission_receipt_verified(
            "codex",
            &lines(&["Full Access is dangerous"]),
            "Full Access"
        ));
        // 未实测工具 → 恒 false
        assert!(!permission_receipt_verified(
            "zcode",
            &codex_full,
            "Full Access"
        ));
        // **空标签必须短路**（否则 `contains("")` 恒真 → 退化成「只找锚」→ 旧回执冒充本次
        // 成功）。还原动作：删掉该短路 → 本断言先红。
        assert!(
            !permission_receipt_verified("codex", &codex_full, ""),
            "空标签不得让判据退化成「只找锚」"
        );
        assert!(
            !permission_receipt_verified("codex", &codex_full, "   "),
            "纯空白标签同义（trim 后为空）"
        );
    }

    /// P1-1 终审：回执核验**双代词形**——0.160.0 新词形
    /// （`• Permission selection requested: <标签>`）与 0.154 旧词形
    /// （`permissions updated to <label>`）各自命中；他档标签不命中（不假成功）。
    ///
    /// 回归史：单锚判据（账本首行 = 0.154 旧锚）在 0.160.0 恒 verified=false——
    /// 段 4 的工具自证一票失效。还原动作（变异）：删掉新词形分支 → 本测试先红。
    #[test]
    fn permission_receipt_verified_dual_word_forms() {
        // 新词形（0.160.0 实打，底料 §1）：目标标签 = 菜单短形，冒号后段**前缀**匹配
        // （全形回执行覆盖短形，同 parse_codex_permission_event_line 匹配纪律）
        assert!(permission_receipt_verified(
            "codex",
            &lines(&["• Permission selection requested: Full Access"]),
            "Full Access"
        ));
        assert!(
            permission_receipt_verified(
                "codex",
                &lines(&[
                    "  普通输出",
                    "• Permission selection requested: Ask for approval (non-admin sandbox)",
                ]),
                "Ask for approval"
            ),
            "全形回执行 starts_with 菜单短形标签"
        );
        // 旧词形（0.154）仍命中（旧版本终端兼容，append-only 旧行仍在账）
        assert!(permission_receipt_verified(
            "codex",
            &lines(&["• Permissions updated to Read Only"]),
            "Read Only"
        ));
        // 负例：新词形锚在、**他档**标签 → 不命中（防旧回执冒充本次成功）
        assert!(!permission_receipt_verified(
            "codex",
            &lines(&["• Permission selection requested: Read Only"]),
            "Full Access"
        ));
        // 负例：目标标签出现在锚**之前**（冒号前段）→ 冒号后段不含目标 → 不命中
        assert!(!permission_receipt_verified(
            "codex",
            &lines(&["Full Access (permission selection requested: Read Only)"]),
            "Full Access"
        ));
        // seen（picker 兜底通道）同走双词形：新词形行原文可取；0.154 旧行仍可取
        assert_eq!(
            permission_receipt_seen(
                "codex",
                &lines(&["• Permission selection requested: Full Access"])
            ),
            Some("• Permission selection requested: Full Access".to_string())
        );
        assert_eq!(
            permission_receipt_seen("codex", &lines(&["• Permissions updated to Full Access"])),
            Some("• Permissions updated to Full Access".to_string())
        );
        assert_eq!(
            permission_receipt_seen("codex", &lines(&["  普通输出"])),
            None,
            "无回执行 → None"
        );
        // kimi 不受双词形影响（现行词形 = 账本锚，判据不变）
        assert!(permission_receipt_verified(
            "kimi",
            &lines(&["Permission mode: Always Ask"]),
            "Always Ask"
        ));
        // 未实测工具：新词形 gate 在 codex → 恒 false（不出手契约不变）
        assert!(!permission_receipt_verified(
            "zcode",
            &lines(&["• Permission selection requested: Full Access"]),
            "Full Access"
        ));
    }

    /// **第三段/回执核验的接线**（纯函数层）：`needs_full_access_confirm` 只对
    /// **codex × 权限组 × Bypass** 为真（实机取证档案 §3：1/2/3 无第三段）。
    #[test]
    fn full_access_confirm_is_codex_permission_bypass_only() {
        assert!(needs_full_access_confirm(
            "codex",
            ModeGroupId::Permission,
            MamMode::Bypass
        ));
        // 同家其余档：无第三段
        assert!(!needs_full_access_confirm(
            "codex",
            ModeGroupId::Permission,
            MamMode::ReadOnly
        ));
        assert!(!needs_full_access_confirm(
            "codex",
            ModeGroupId::Permission,
            MamMode::Default
        ));
        // 同家**模式组**的 Bypass（结构上不存在，但判据本身要守住）
        assert!(!needs_full_access_confirm(
            "codex",
            ModeGroupId::Mode,
            MamMode::Bypass
        ));
        // 其余家：kimi 权限三档**均无**第三段（档案 §5.1–5.3）
        for t in ["kimi", "claude", "opencode", "zcode"] {
            assert!(
                !needs_full_access_confirm(t, ModeGroupId::Permission, MamMode::Bypass),
                "{t} 无 Full Access 二次确认框（未实测/无该档）"
            );
        }
    }

    // ==== 丁T4 收尾：非 ASCII 真机屏不得 panic（C1 根因防线，判据换新后重跑）====

    /// **C1 根因防线**（T4 复评 Critical）：旧判据用**字节切片**做前缀比较，在「前 N
    /// 字节跨非 ASCII 字符」时 panic，而**真机屏幕每一份都能凑出这种行**。
    ///
    /// 新判据（关键词包含 + `to_lowercase()`）**没有任何字节索引**，本测试锁住这一点：
    /// 夹具 = 批次丙 T6 探测档案的真机屏幕原文（含 `—`(U+2014，3 字节)、
    /// `╭─`(U+256D/U+2500)、中文）+ 丁T4 收尾的真机权限菜单（含 `›`/`❯`/`←`/`↑↓·`）。
    /// **断言的是「不 panic + 如实中止」**，不是「不编译」。
    ///
    /// **还原动作（变异 3）**：把 `locate_menu_items` 的 `to_lowercase()+contains` 改回
    /// 任何形式的字节切片比较（如 `text[..label.len()]`）→ 本测试先红（panic）。
    #[test]
    fn menu_locator_survives_real_non_ascii_screens() {
        // 真机屏幕原文（含三类非 ASCII；行号标注其出处，便于回溯）
        let real: Vec<&str> = vec![
            // screen-t6-claude-before.txt:14（`—` U+2014 落在第 8..11 字节——C1 的
            // 原始触发行；旧前缀比较 "Read Only" 的 9 字节切点正落在 `—` 内部）
            "  hello() —is complete and verified (python main.py printed hi).",
            // screen-codex-after-enter.txt:1（`╭─` 边框行；"Full Access" 的 11 字节切点
            // 落在第二个 `─` 内）
            "\u{feff}╭────────────────────────────────────────────────────────╮",
            // screen-codex-after-st1.txt:2（中文正文；"Read Only" 的切点落在「如」内）
            "  在 main.py 中新增一个 hello() 函数，返回 \"Hello, World!\"，不改动现有代码和其他文件。",
            // screen-kimi-after-enter-confirm.txt:23（`└─` 边框）
            "   └──────────────────────────────────────────────────────────────────────",
            // screen-codex-planmd.txt:15（`·` 分隔的状态栏）
            "  glm-5.3-flash medium · ~\\AppData\\Local\\Temp\\mam-probe-c3-20260921-150000\\proj-codex",
            // screen-kimi-after-st.txt:29（`plan` 前缀底栏——本行**应当**被解析，见 parse 测试）
            " plan  GLM-5.3-Flash thinking: high  C:\\Users\\bunny\\AppData\\Local\\Temp\\proj-kimi",
            // 纯 ASCII 短行（前缀比行长的边界格）
            "hi",
            "",
        ];
        let screen: Vec<String> = real.iter().map(|s| s.to_string()).collect();
        // ① 行动入口（闭环）：不得 panic，且**零按键**（这些屏上没有完整菜单）
        for (tool, target) in [
            ("codex", MamMode::ReadOnly),
            ("codex", MamMode::Default),
            ("codex", MamMode::Bypass),
            ("kimi", MamMode::Default),
            ("kimi", MamMode::AcceptEdits),
            ("kimi", MamMode::Bypass),
        ] {
            let (r, sent) = run_scripted_nav(&screen, tool, target, &[&screen]);
            assert!(
                r.is_err(),
                "真机非 ASCII 屏上没有完整菜单 → 必须如实中止（{tool}/{target:?}）：{r:?}"
            );
            assert!(sent.is_empty(), "中止 ⇒ 零按键：{sent:?}");
        }
        // ② 逐行驱动定位器 + 每家的确认框探测（全部标签 × 全部分行都不得 panic）
        for line in &screen {
            let one = screen_of_one(line);
            for tool in ["codex", "kimi"] {
                let _ = locate_menu_items(&one, menu_labels(tool));
                let _ = MenuNavPlan::ConfirmAffirmative {
                    tool,
                    keyword: FULL_ACCESS_AFFIRMATIVE_KEYWORD,
                }
                .probe(&one);
            }
        }
        // ③ 确认框的**真机非 ASCII 说明文**（含小写 full access）逐行喂探测器
        for line in full_access_confirm_real() {
            let _ = MenuNavPlan::ConfirmAffirmative {
                tool: "codex",
                keyword: FULL_ACCESS_AFFIRMATIVE_KEYWORD,
            }
            .probe(&screen_of_one(&line));
        }
    }

    fn screen_of_one(s: &str) -> Vec<String> {
        vec![s.to_string()]
    }

    /// **C1 语义回归**：关键词匹配必须**大小写不敏感**，且**含非 ASCII 词**也成立
    /// （当前词表是纯 ASCII，但判据不得假定这一点）。
    #[test]
    fn keyword_match_is_case_insensitive_and_unicode_safe() {
        // 大小写不敏感：真机原文是 Title Case，喂全大写也要命中（锚窗内行形不变）
        let padded = lines(&[
            "  UPDATE MODEL PERMISSIONS",
            "  1. READ ONLY",
            "› 2. ASK FOR APPROVAL (CURRENT)",
            "  4. FULL ACCESS",
            "  PRESS ENTER TO CONFIRM OR ESC TO GO BACK",
        ]);
        let items = locate_menu_items(&padded, menu_labels("codex")).unwrap();
        assert_eq!(items.len(), 3, "大小写不敏感（锚与行形全大写仍命中）");
        assert!(items[1].highlighted);
        let _ = padded;

        // 非 ASCII 标签同样命中（判据不含任何字节索引；kimi 锚窗 + kimi 行形，
        // 标签词表是参数——自定义词表走同一锚窗）
        let cn = lines(&[
            "  Select permission mode",
            "  ↑↓ navigate · Enter select · Esc cancel",
            "  只读 ← current",
            "❯ 默认",
            "  完全信任",
        ]);
        let items2 = locate_menu_items(&cn, &["只读", "默认", "完全信任"]).unwrap();
        assert_eq!(items2.len(), 3);
        assert!(items2[1].highlighted);
        // 窗内散文行（含两档词）不进项表（行形判据与字符集无关）
        let cn2 = lines(&[
            "  Select permission mode",
            "  ↑↓ navigate · Enter select · Esc cancel",
            "  只读",
            "  默认",
            "  完全信任",
            "  只读与完全信任的区别",
        ]);
        assert_eq!(
            locate_menu_items(&cn2, &["只读", "默认", "完全信任"])
                .unwrap()
                .len(),
            3
        );
    }

    // ==== 丁T4 复评 I1 的一致性闸（判据换新后重跑；两条不变式原样保留）====

    /// **评审反例的可执行锁**（T4 复评 I1，夹具**换成用户真机原文**）：同屏残留正文里
    /// 出现档位标签（评审给的真机形态：codex `/status` 回显 `Read Only (sandbox: read-only)`）。
    ///
    /// **E3① 后的形态**：该行不是 `N. ` 编号行 → 行形判据在**定位阶段**直接排除（4 项、
    /// 自洽、闭环可走）；「同档标签第二行」的一致性闸纵深防御由**编号重复行**锁
    /// （`menu_gate_refuses_duplicated_label_from_bystander_line`）。
    #[test]
    fn menu_gate_refuses_duplicated_label_from_bystander_line() {
        let mut screen = codex_menu_real();
        // 残留正文插在**高亮项与目标之间**（唯一会撑大错误步进的位置）——非编号行被排除
        screen.insert(4, "  Read Only (sandbox: read-only)".to_string());
        let raw = locate_menu_items(&screen, menu_labels("codex")).unwrap();
        assert_eq!(raw.len(), 4, "非编号行不进项表（行形判据前移排除）");
        assert!(menu_items_coherent(&raw, "codex"), "四真项自洽");
        let (r, _sent) = run_scripted_nav(&screen, "codex", MamMode::Bypass, &[&screen]);
        assert!(
            r.is_err(),
            "单屏脚本高亮不动 → 闭环中止（与干扰行无关）：{r:?}"
        );

        // **编号重复行**（窗内）：一致性闸不变式 1 的纵深防御仍生效——5 项、重复 → 拒绝
        let mut dup = codex_menu_real();
        dup.insert(4, "  5. Read Only (sandbox: read-only)".to_string());
        let raw_dup = locate_menu_items(&dup, menu_labels("codex")).unwrap();
        assert_eq!(raw_dup.len(), 5, "编号行如实计入");
        assert!(
            !menu_items_coherent(&raw_dup, "codex"),
            "标签重复 → 判不自洽（第二道闸的收口点）"
        );
        let (r2, sent2) = run_scripted_nav(&dup, "codex", MamMode::Bypass, &[&dup]);
        assert!(r2.is_err(), "混入编号重复行 → 必须中止：{r2:?}");
        assert!(
            sent2.is_empty(),
            "中止发生在首次定位 ⇒ **零按键**：{sent2:?}"
        );
    }

    /// 一致性闸的第二条不变式：**目标档全集齐备**——菜单被遮挡/只画出两档 → 中止。
    /// 同时锁住「`Approve for me` 不算进全集」（Guardian 关闭的正常菜单不得被判不完整）。
    #[test]
    fn menu_gate_requires_all_target_tiers() {
        // E3① 锚窗助手：行集包进 codex 标题↔footer（下同）
        let anchored = |rows: &[&str]| -> Vec<String> {
            let mut v = lines(&["  Update Model Permissions"]);
            v.extend(rows.iter().map(|s| s.to_string()));
            v.push("  Press enter to confirm or esc to go back".to_string());
            v
        };
        // 真机形态（含 Approve for me）→ 通过
        let full = anchored(&[
            "  1. Read Only",
            "› 2. Ask for approval (current)",
            "  3. Approve for me",
            "  4. Full Access",
        ]);
        assert!(menu_items_coherent(
            &locate_menu_items(&full, menu_labels("codex")).unwrap(),
            "codex"
        ));
        // Guardian 关闭（无 Approve for me）→ **仍通过**（它是可选档，不算进全集）
        let no_guardian = anchored(&[
            "  1. Read Only",
            "› 2. Ask for approval (current)",
            "  4. Full Access",
        ]);
        assert!(
            menu_items_coherent(
                &locate_menu_items(&no_guardian, menu_labels("codex")).unwrap(),
                "codex"
            ),
            "Approve for me 缺席不得判不完整（§2.6 未把它列为目标档）"
        );
        // 只画出两档（缺 Full Access）→ 中止
        let truncated = anchored(&["  1. Read Only", "› 2. Ask for approval (current)"]);
        let raw = locate_menu_items(&truncated, menu_labels("codex")).unwrap();
        assert!(!menu_items_coherent(&raw, "codex"), "目标档不全 → 不猜位置");
        let (r, sent) = run_scripted_nav(&truncated, "codex", MamMode::Default, &[&truncated]);
        assert!(r.is_err(), "不完整菜单 → 中止：{r:?}");
        assert!(sent.is_empty(), "零按键：{sent:?}");
        // kimi 三档同样要求
        let kimi_short = lines(&[
            "  Select permission mode",
            "  ↑↓ navigate · Enter select · Esc cancel",
            "   ❯ Always Ask ← current",
            "     Ask When Needed",
        ]);
        let raw_k = locate_menu_items(&kimi_short, menu_labels("kimi")).unwrap();
        assert!(
            !menu_items_coherent(&raw_k, "kimi"),
            "kimi 菜单缺永不询问 → 中止"
        );
    }

    /// 一致性闸的**残留风险**（E3① 更新：原「已知缺口」已被行形判据关闭）——
    /// 正文散文行（`Read Only (sandbox: read-only)`，非编号行）在**定位阶段**就被
    /// 排除，不再能「恰好补齐缺席的目标档」骗过两条不变式。本测试把缺口关闭钉成锁。
    #[test]
    fn menu_gate_known_gap_is_documented_not_hidden() {
        // 菜单只画出两档 + 正文恰好有一行含缺席的第三个目标标签（且不重复）
        let screen = lines(&[
            "  Update Model Permissions",
            "  1. Ask for approval",
            "› 2. Full Access",
            "  Read Only (sandbox: read-only)",
            "  Press enter to confirm or esc to go back",
        ]);
        let raw = locate_menu_items(&screen, menu_labels("codex")).unwrap();
        assert_eq!(
            raw.len(),
            2,
            "散文行被行形判据排除 → 只有 2 真项（旧关键词判据会计入 3 项）"
        );
        assert!(
            !menu_items_coherent(&raw, "codex"),
            "缺 Read Only 目标档 → 闸门拒绝（旧「已知缺口」经 E3① 行形判据关闭）"
        );
    }

    // ==== 丁T4 收尾：整条编排（第二段 → 第三段 → 回执核验）的脚本化驱动 ====

    /// 脚本化编排的返回：`(结果, 实际发出的键, 每次轮询的 (阶段名, 是否可选))`
    type StageScriptResult = (
        Result<MenuStagesOutcome, String>,
        Vec<String>,
        Vec<(String, bool)>,
    );

    /// **整条编排的脚本化驱动器**：读屏与发键全部脚本化，跑 [`run_menu_stages`]
    /// ——**不依赖真 conhost**（这是把编排抽进内核的全部意义）。
    ///
    /// # 脚本语义（**对齐生产轮询语义**，不是「每次读屏取下一屏」）
    ///
    /// `screens` 是终端内容的**时间序列**，`cursor` 指向「此刻屏上是什么」：
    /// - `send(key)`：记录按键（**不推进**——连续多键如 backspace 清行只重绘一次）；
    /// - `settle()`：**推进 cursor**（= 生产侧「按键后给 TUI 重绘留时间」，重绘后
    ///   屏面才变化）；
    /// - `read()`：返回**当前**屏（闭环的每次复核都读当前屏）；
    /// - `poll(plan, optional)`：**循环**读当前屏直到 `plan` 可行动——`Ready` → 返回；
    ///   `Fatal` → Err（立即中止）；`NotYet` → 推进 cursor 再试；**序列耗尽** = 生产侧
    ///   「轮询窗尽」→ `optional` 时 `Ok(None)`、否则 `Err`；
    /// - `poll_receipt()`：同循环，命中回执行 → `Ok(Some)`；耗尽 → `Ok(None)`。
    ///
    /// 借用纪律：`read`/`send` 闭包共同读写 `cursor` 与 `sent`，故用 `Cell`/`RefCell`
    /// 提供内部可变性（`MenuTerminal` 的 `&mut self` 只借一次，闭包内不再借用整体）。
    ///
    /// 返回 `(编排结果, 实际发出的键, 每次轮询的 (阶段名, 是否可选))`。
    fn run_stage_script(
        screens: Vec<Vec<String>>,
        tool: &str,
        group: ModeGroupId,
        target: MamMode,
    ) -> StageScriptResult {
        use crate::inject::mode::PollStep;
        use std::cell::{Cell, RefCell};
        let cursor = Cell::new(0usize);
        let cur = || screens[cursor.get().min(screens.len() - 1)].clone();
        /// 推进 cursor；已在末屏则返回 false（= 生产侧的「窗尽」）
        fn advance(cursor: &Cell<usize>, n: usize) -> bool {
            if cursor.get() + 1 < n {
                cursor.set(cursor.get() + 1);
                true
            } else {
                false
            }
        }
        let poll_calls: RefCell<Vec<(String, bool)>> = RefCell::new(Vec::new());
        let sent: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let n = screens.len();
        let outcome = run_menu_stages(
            tool,
            group,
            target,
            |plan, optional| {
                poll_calls
                    .borrow_mut()
                    .push((plan.stage().to_string(), optional));
                loop {
                    match plan.probe(&cur()) {
                        PollStep::Ready(_) => return Ok(Some(cur())),
                        PollStep::Fatal(why) => return Err(format!("{why}；请人工核对终端")),
                        PollStep::NotYet(_) => {
                            if !advance(&cursor, n) {
                                // 窗尽：菜单（optional=false）→ Err；确认框（true）→ None
                                if optional {
                                    return Ok(None);
                                }
                                return Err(format!(
                                    "{} 未出现或读不到（脚本窗尽）；请人工核对终端",
                                    plan.stage()
                                ));
                            }
                        }
                    }
                }
            },
            || {
                loop {
                    let labels = cur();
                    let label = menu_target_label(tool, target).unwrap_or("");
                    if permission_receipt_verified(tool, &labels, label) {
                        return Ok(Some(labels));
                    }
                    if !advance(&cursor, n) {
                        return Ok(None); // 窗尽未见到回执行（**不是失败**）
                    }
                }
            },
            &mut Closures {
                read: || Some(cur()),
                send: |k: &str| {
                    sent.borrow_mut().push(k.to_string());
                    Ok(())
                },
                settle: || {
                    advance(&cursor, n); // settle = 重绘窗口（脚本里推进到下一屏）
                },
            },
        );
        (outcome, sent.into_inner(), poll_calls.into_inner())
    }

    // ==== 2026-09-23 codex 数字直达：整条编排（残留防护 → 开菜单 → 数字直达 →
    //      二阶段确认 → 回执核验）的脚本化驱动 ====

    /// codex 数字直达编排的**脚本化驱动器**（[`run_codex_permission_stages`]）。
    ///
    /// 脚本语义与 [`run_stage_script`] 同款（cursor 指向当前屏、send/开菜单推进），
    /// 差异：`open_menu` 记录开菜单动作并推进 cursor（菜单不是「键」）；digit 与
    /// 确认框轮询分别走 [`codex_permission_digit_probe`] 与 [`confirm_box_probe`]。
    /// 返回 `(编排结果, 实际发出的键, 开菜单动作次数, 硬性间隔等待次数)`。
    fn run_codex_stage_script(
        screens: Vec<Vec<String>>,
        target: MamMode,
    ) -> (Result<MenuStagesOutcome, String>, Vec<String>, usize, usize) {
        use crate::inject::mode::PollStep;
        use std::cell::{Cell, RefCell};
        let cursor = Cell::new(0usize);
        let cur = || screens[cursor.get().min(screens.len() - 1)].clone();
        fn advance(cursor: &Cell<usize>, n: usize) -> bool {
            if cursor.get() + 1 < n {
                cursor.set(cursor.get() + 1);
                true
            } else {
                false
            }
        }
        let sent: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let opens: Cell<usize> = Cell::new(0);
        let waits: Cell<usize> = Cell::new(0);
        let n = screens.len();
        let outcome = run_codex_permission_stages(
            target,
            || {
                opens.set(opens.get() + 1);
                advance(&cursor, n); // 开菜单 → 菜单画出（脚本推进）
                Ok(())
            },
            || loop {
                match codex_permission_digit_probe(&cur(), target) {
                    PollStep::Ready(d) => return Ok(d),
                    PollStep::Fatal(why) => return Err(format!("{why}；请人工核对终端")),
                    PollStep::NotYet(_) => {
                        if !advance(&cursor, n) {
                            return Err(
                                "codex 的权限菜单未出现或读不到档位表（脚本窗尽）；请人工核对终端"
                                    .to_string(),
                            );
                        }
                    }
                }
            },
            || loop {
                match confirm_box_probe(&cur(), "codex", FULL_ACCESS_AFFIRMATIVE_KEYWORD) {
                    PollStep::Ready((c, _)) => return Ok(Some(c)),
                    PollStep::Fatal(why) => return Err(format!("{why}；请人工核对终端")),
                    PollStep::NotYet(_) => {
                        if !advance(&cursor, n) {
                            return Ok(None); // 窗尽：确认框缺席不当作失败
                        }
                    }
                }
            },
            || {
                loop {
                    let label = menu_target_label("codex", target).unwrap_or("");
                    if permission_receipt_verified("codex", &cur(), label) {
                        return Ok(Some(cur()));
                    }
                    if !advance(&cursor, n) {
                        return Ok(None); // 窗尽未见到回执行（不是失败）
                    }
                }
            },
            || {
                waits.set(waits.get() + 1); // 硬性间隔（生产=睡 MODE_STEP_MIN_GAP_MS）
            },
            &mut Closures {
                read: || Some(cur()),
                send: |k: &str| {
                    sent.borrow_mut().push(k.to_string());
                    Ok(())
                },
                settle: || {
                    advance(&cursor, n); // settle = 重绘窗口（脚本推进）
                },
            },
        );
        (outcome, sent.into_inner(), opens.get(), waits.get())
    }

    /// **场景①：1/2/3 档 —— 残留检查（无）→ 开菜单 → 数字直达 → 回执 → verified**
    ///
    /// 脚本（时间序）：① 干净屏（无残留菜单）→ ② 开菜单后菜单画出（目标=只读，
    /// 屏上编号 1）→ ③ 发数字后回执行出现。
    ///
    /// 断言：`menu_keys=["1"]`（**无回车**——数字直达不需要提交键）+ 零 esc +
    /// `receipt_seen=true` + 无确认段。菜单屏用**真机四项夹具**（用户 2026-09-23
    /// 走查截图转写，含 MCP 警告噪声行）。
    #[test]
    fn stage_flow_digit_tier_with_receipt() {
        let clean = clean_idle();
        let menu = e_stage2_screen("codex-perm-menu-4tier.txt");
        let receipt = lines(&["• Permissions updated to Read Only"]);
        let (r, sent, opens, waits) =
            run_codex_stage_script(vec![clean, menu, receipt], MamMode::ReadOnly);
        let out = r.expect("1/2/3 档：数字直达走完");
        assert_eq!(out.menu_keys, vec!["1"], "只读 = 屏上编号 1，直达");
        assert_eq!(sent, vec!["1"], "**无回车**（数字直达无提交键），无第三段");
        assert_eq!(opens, 1, "干净屏：无残留防护动作，开菜单恰好一次");
        assert_eq!(
            waits, 1,
            "步骤①开菜单 → 步骤②数字之间恰好一次硬性 ≥0.5s 间隔（用户指令）"
        );
        assert!(!out.confirm_done, "未走确认框");
        assert_eq!(
            out.receipt_seen,
            Some(true),
            "屏上有 `• Permissions updated to Read Only` → 回执核验通过"
        );
    }

    /// **场景②：Full Access —— 数字 4 → 确认框 → 肯定项编号 1 → 回执**
    ///
    /// 用户实测（2026-09-23）：「4 的需要二阶段确认，此时按 1」。确认框屏用
    /// **真机夹具**（`Enable full access?` + `1. Yes, continue anyway`）。
    #[test]
    fn stage_flow_digit_full_access_with_confirm() {
        let clean = clean_idle();
        let menu = e_stage2_screen("codex-perm-menu-4tier.txt");
        let confirm = e_stage2_screen("codex-full-access-confirm.txt");
        let receipt = lines(&["• Permissions updated to Full Access"]);
        let (r, sent, _, waits) =
            run_codex_stage_script(vec![clean, menu, confirm, receipt], MamMode::Bypass);
        let out = r.expect("Full Access 全链必须走通");
        assert_eq!(out.menu_keys, vec!["4"], "完全信任 = 屏上编号 4");
        assert!(out.confirm_done, "二阶段确认框已出现并走完");
        assert_eq!(
            sent,
            vec!["4", "1"],
            "数字 4 直达 + 确认框肯定项编号 1 直达"
        );
        assert_eq!(
            waits, 2,
            "两处硬性 ≥0.5s：步骤①→②（开菜单→数字）+ 步骤②→③（数字→确认框）"
        );
        assert_eq!(out.receipt_seen, Some(true));
    }

    /// **场景②之二：确认键 = 肯定项的屏上编号，与高亮位置无关**（数字直达与
    /// 闭环的本质差别——实测按 `1` 就是 Yes，不看点位）。
    #[test]
    fn stage_flow_digit_confirm_ignores_highlight() {
        let clean = clean_idle();
        let menu = e_stage2_screen("codex-perm-menu-4tier.txt");
        // 确认框高亮在 Cancel（第 2 项）——数字直达仍发肯定项编号 1
        let confirm_hl_cancel = lines(&[
            " Enable full access?",
            "  1. Yes, continue anyway  Apply full access for this session",
            "› 2. Cancel                Go back without enabling full access",
        ]);
        let receipt = lines(&["• Permissions updated to Full Access"]);
        let (r, sent, _, _) = run_codex_stage_script(
            vec![clean, menu, confirm_hl_cancel, receipt],
            MamMode::Bypass,
        );
        let out = r.expect("高亮在否定项也不影响编号直达");
        assert_eq!(
            sent,
            vec!["4", "1"],
            "确认键=肯定项屏上编号 1（与高亮无关）"
        );
        assert_eq!(out.receipt_seen, Some(true));
    }

    /// **场景③：肯定项不唯一 → 中止（Fatal），确认框阶段零投递**
    #[test]
    fn stage_flow_digit_confirm_ambiguous_aborts() {
        let clean = clean_idle();
        let menu = e_stage2_screen("codex-perm-menu-4tier.txt");
        // 同一簇内两个含 `continue` 的项（二进制有 `Continue and don't warn again.`
        // 变体文案）→ 分不清哪个是肯定项 → **不猜**
        let ambiguous = lines(&[
            " Enable full access?",
            "› 1. Yes, continue anyway",
            "  2. Continue and don't warn again",
            "  3. Cancel",
        ]);
        let (r, sent, _, _) = run_codex_stage_script(vec![clean, menu, ambiguous], MamMode::Bypass);
        let err = r.as_ref().unwrap_err();
        assert!(err.contains("有 2 个"), "肯定项不唯一必须点名：{err}");
        assert_eq!(
            sent,
            vec!["4"],
            "菜单数字已发（必要投递）；**确认框一步都没发**"
        );
    }

    /// **场景③之二：确认框在场但无肯定项** → 按「未出现」处理（**不当作失败**）→
    /// 继续回执核验；**确认框零投递**（宁可不切也不误点 `Cancel`）。
    #[test]
    fn stage_flow_digit_confirm_without_affirmative_is_not_failure() {
        let clean = clean_idle();
        let menu = e_stage2_screen("codex-perm-menu-4tier.txt");
        // 两个选项都不含 `continue`（某变体把 Yes 改写成别的词）
        let no_affirmative = lines(&[" Enable full access?", "› 1. Yes", "  2. Cancel"]);
        let (r, sent, _, _waits) =
            run_codex_stage_script(vec![clean, menu, no_affirmative], MamMode::Bypass);
        let out = r.expect("无肯定项 → 按「未出现」处理，**不中止全链**");
        assert!(!out.confirm_done, "确认框未走完");
        assert_eq!(sent, vec!["4"], "确认框零投递");
        assert_eq!(out.receipt_seen, Some(false), "也没见回执 → verified=false");
    }

    /// **场景④：残留菜单防护**——首屏已有上次遗留的菜单（标题锚在屏）→ 先 `esc`
    /// 关闭再开菜单。2026-09-23 事故根因：残留菜单吞字符、回车误确认当前项
    /// （用户实测现场 `Permissions updated to Ask for approval` + 输入行堆积）。
    #[test]
    fn stage_flow_digit_clears_residual_menu_first() {
        let residual = e_stage2_screen("codex-perm-menu-4tier.txt"); // 遗留菜单在屏
        let clean_after_esc = clean_idle();
        let menu = e_stage2_screen("codex-perm-menu-4tier.txt");
        let receipt = lines(&["• Permissions updated to Read Only"]);
        let (r, sent, opens, _waits) = run_codex_stage_script(
            vec![residual, clean_after_esc, menu, receipt],
            MamMode::ReadOnly,
        );
        let out = r.expect("残留防护后全链走通");
        assert_eq!(
            sent.first().map(|s| s.as_str()),
            Some("esc"),
            "**esc 先行**（关掉上次遗留的菜单再开新的）：{sent:?}"
        );
        assert_eq!(opens, 1, "开菜单恰好一次（防护后正常流程）");
        assert_eq!(out.menu_keys, vec!["1"]);
        assert_eq!(out.receipt_seen, Some(true));
    }

    /// **场景⑤：菜单窗尽（读不到目标编号）→ 中止，数字键零投递（不盲发）**
    #[test]
    fn stage_flow_digit_window_exhausted_aborts() {
        let clean = clean_idle();
        let blank = lines(&["  still loading..."]);
        let (r, sent, opens, _waits) = run_codex_stage_script(vec![clean, blank], MamMode::Default);
        let err = r.unwrap_err();
        assert!(
            err.contains("未出现或读不到档位表"),
            "中止文案要讲清「命令已发、档位未切」：{err}"
        );
        assert!(sent.is_empty(), "数字键零投递（不盲发数字键）：{sent:?}");
        assert_eq!(opens, 1, "开菜单已发（命令投递了，但档位键没发）");
    }

    // ==== 2026-09-23 账本接线：新 footer 变体（本次实机事故的回归锁）====

    /// **新 footer 变体（真机 dump 夹具）必须成窗**——本次实机事故的直接回归锁：
    /// codex 0.156.1 的 footer 是 `enter select · esc back`，旧实现写死单句
    /// `press enter to confirm or esc to go back` → 窗不成立 → 整条权限切换静默失效。
    /// 夹具来源 = 自建会话探针 dump（codex-cli 0.156.1）。
    #[test]
    fn menu_window_opens_with_new_footer_variant() {
        let screen = e_stage2_screen("codex-perm-menu-newfooter-escback.txt");
        let lowered: Vec<String> = screen.iter().map(|l| l.to_lowercase()).collect();
        let (title_idx, footer_idx) =
            codex_menu_window(&lowered).expect("新 footer 变体必须能成窗（本次事故的修复）");
        assert!(
            title_idx < footer_idx,
            "窗序必须正确：{title_idx} < {footer_idx}"
        );

        // 四档编号照常读到（数字直达在新形态下可用）
        for (target, digit) in [
            (MamMode::ReadOnly, "1"),
            (MamMode::Default, "2"),
            (MamMode::AcceptEdits, "3"),
            (MamMode::Bypass, "4"),
        ] {
            assert_eq!(
                codex_permission_digit_probe(&screen, target),
                PollStep::Ready(digit.to_string()),
                "{target:?} 在新 footer 形态下应读到屏上编号 {digit}"
            );
        }
    }

    /// **第 2 档带内联后缀 `(non-admin sandbox)`**（宽版布局夹具，用户截图转写）：
    /// `2. Ask for approval (non-admin sandbox) (current)` —— 前缀匹配使内联后缀
    /// 不影响认档；且 footer 之上的说明段（`The non-admin sandbox protects …`）
    /// **不得**进入菜单项集合（靠编号行形态天然过滤，本测锁死该不变式）。
    #[test]
    fn menu_options_tolerate_inline_suffix_and_explainer_block() {
        let screen = e_stage2_screen("codex-perm-menu-nonadmin-sandbox.txt");
        let lowered: Vec<String> = screen.iter().map(|l| l.to_lowercase()).collect();
        let opts = codex_menu_options(&screen, &lowered).expect("带内联后缀的菜单必须能解析");
        assert_eq!(opts.len(), 4, "恰好四档（说明段不得入表）：{opts:?}");
        assert_eq!(
            opts.iter().map(|o| o.number).collect::<Vec<_>>(),
            vec![1, 2, 3, 4],
            "编号取屏上实读值"
        );
        assert!(
            opts[1].label.contains("non-admin sandbox"),
            "屏上原文（含内联后缀）必须原样保留给用户核对：{:?}",
            opts[1].label
        );
        // 右侧描述列必须被剥掉（否则面板上每个选项都拖着一串英文说明）
        assert!(
            !opts[1].label.contains("Codex can read and edit"),
            "档名之外的右侧描述列应剥掉：{:?}",
            opts[1].label
        );
        assert!(
            !opts[0].label.contains("Codex can read files"),
            "档名之外的右侧描述列应剥掉：{:?}",
            opts[0].label
        );
        assert_eq!(opts[0].label, "Read Only", "纯档名");
        assert_eq!(opts[2].label, "Approve for me", "纯档名");
        assert_eq!(opts[3].label, "Full Access", "纯档名");
        assert!(opts[1].highlighted, "第 2 档是当前高亮项（›）");
        // 说明段的两行不得混入
        assert!(
            !opts
                .iter()
                .any(|o| o.label.contains("setup-default-sandbox")),
            "说明段不得入表：{opts:?}"
        );
        // 四档的屏上编号照常读到
        for (target, digit) in [
            (MamMode::ReadOnly, "1"),
            (MamMode::Default, "2"),
            (MamMode::AcceptEdits, "3"),
            (MamMode::Bypass, "4"),
        ] {
            assert_eq!(
                codex_permission_digit_probe(&screen, target),
                PollStep::Ready(digit.to_string()),
                "{target:?} 在带内联后缀形态下应读到 {digit}"
            );
        }
    }

    /// **composer 判据不被自身菜单污染**（0.156.1 新 footer 含 ` · ` 引入的连带陷阱）：
    /// 菜单开着时，`codex_composer_text` **不得**把高亮菜单项（`› 2. …`）认成 composer
    /// ——否则 `composer_residue` 恒报残留 → 段 0.5 backspace×64 后如实中止（功能不可用）。
    /// 反例（变异）：去掉 `parse_option_line` 排除条件 → 本测红。
    #[test]
    fn composer_text_not_polluted_by_own_menu() {
        let screen = e_stage2_screen("codex-perm-menu-newfooter-escback.txt");
        // 该夹具里菜单 footer 是最后一条含 ` · ` 的行，其上方紧邻高亮菜单项
        let text = codex_composer_text(&screen);
        // 判据不可得（None）也是**合格结果**——关键是不能把菜单项当 composer
        if let Some(t) = text {
            assert!(
                !t.starts_with("2. Ask for approval"),
                "菜单高亮项被误认成 composer（` · ` 陷阱）：{t:?}"
            );
            assert!(
                !t.contains("/permissions") && !t.contains("Read Only"),
                "菜单内容不得被当成输入行文本：{t:?}"
            );
        }
        // 更强的锁：残留判定不得因菜单在场而报「有残留」
        assert!(
            composer_residue(&screen).is_none(),
            "菜单在屏时不得判输入行有残留（否则段 0.5 会 backspace 清菜单）"
        );
    }

    /// **overlay 种类判定**：新 footer 夹具 → 权限菜单；确认框锚在屏 → 确认框优先
    /// （确认框弹出时菜单可能仍在屏，见 `parse_dialog_clusters` 同源注记）。
    #[test]
    fn overlay_kind_prefers_confirm_box() {
        let menu = e_stage2_screen("codex-perm-menu-newfooter-escback.txt");
        let lowered: Vec<String> = menu.iter().map(|l| l.to_lowercase()).collect();
        assert_eq!(
            codex_overlay_kind(&lowered),
            Some(CodexOverlay::PermissionMenu)
        );
        assert!(codex_overlay_present(&lowered));

        // 菜单 + 确认框同屏 → 判确认框
        let mut both = menu.clone();
        both.push("  Enable full access?".to_string());
        let lowered: Vec<String> = both.iter().map(|l| l.to_lowercase()).collect();
        assert_eq!(
            codex_overlay_kind(&lowered),
            Some(CodexOverlay::FullAccessConfirm),
            "确认框与菜单同屏时，用户面对的是确认框"
        );

        // 干净屏 → 无 overlay
        let clean: Vec<String> = lines(&["  普通输出", "• done"]);
        let lowered: Vec<String> = clean.iter().map(|l| l.to_lowercase()).collect();
        assert_eq!(codex_overlay_kind(&lowered), None);
        assert!(!codex_overlay_present(&lowered));
    }

    /// **picker 第二动作的硬前置**（不盲发）：屏上没有 overlay → 零投递。
    /// 这是「面板数据过期」情形（用户看到的菜单其实已关）——敲进去会落到输入行。
    #[test]
    fn pick_without_overlay_never_sends() {
        let sent: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let clean = lines(&["  普通输出", "• done"]);
        let r = run_codex_menu_pick(
            2,
            || Some(clean.clone()),
            || Ok(None),
            || {},
            &mut Closures {
                read: || Some(clean.clone()),
                send: |k: &str| {
                    sent.borrow_mut().push(k.to_string());
                    Ok(())
                },
                settle: || {},
            },
        );
        let err = r.unwrap_err();
        assert!(err.contains("零投递"), "{err}");
        assert!(sent.borrow().is_empty(), "零投递：{:?}", sent.borrow());
    }

    /// **picker：确认框阶段可投递**（T4-F4 改写；spec §3.2 Full Access 分叉）：
    /// 确认框在屏 + number=1 → 照发「1」（旧版零投递——二阶段第二跳死路），确认框
    /// 消失 → Done。还原动作（变异）：把确认框分支改回一律 Err → 本测试先红
    /// （sent 变空、Err 冒头）。
    #[test]
    fn pick_delivers_on_confirm_box_stage() {
        let sent: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let mut confirm_screen = e_stage2_screen("codex-perm-menu-newfooter-escback.txt");
        confirm_screen.push("  Enable full access?".to_string());
        // 发键后读屏：确认框已消失（Yes 生效）——用干净屏表示
        let after = lines(&["  普通输出", "• Permissions updated to Full Access"]);
        let stage = std::cell::Cell::new(0u32);
        let r = run_codex_menu_pick(
            1,
            || {
                if stage.get() == 0 {
                    stage.set(1);
                    Some(confirm_screen.clone())
                } else {
                    Some(after.clone())
                }
            },
            || Ok(None),
            || {},
            &mut Closures {
                read: || None,
                send: |k: &str| {
                    sent.borrow_mut().push(k.to_string());
                    Ok(())
                },
                settle: || {},
            },
        );
        match r {
            Ok(MenuPick::Done {
                from_confirm_box: true,
                ..
            }) => {}
            other => panic!("确认框阶段发 1 且确认框消失 → Done：{other:?}"),
        }
        assert_eq!(
            sent.borrow().as_slice(),
            ["1".to_string()],
            "确认框阶段照发数字键：{:?}",
            sent.borrow()
        );
    }

    /// **picker：确认框阶段越界零投递**（T4-F4 新增）：确认框只有 1/2 两项，
    /// number=3（菜单编号混进确认框 = 面板过期）→ Err 且零投递。
    #[test]
    fn pick_rejects_out_of_domain_on_confirm_box() {
        let sent: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let mut confirm_screen = e_stage2_screen("codex-perm-menu-newfooter-escback.txt");
        confirm_screen.push("  Enable full access?".to_string());
        let r = run_codex_menu_pick(
            3,
            || Some(confirm_screen.clone()),
            || Ok(None),
            || {},
            &mut Closures {
                read: || Some(confirm_screen.clone()),
                send: |k: &str| {
                    sent.borrow_mut().push(k.to_string());
                    Ok(())
                },
                settle: || {},
            },
        );
        let err = r.unwrap_err();
        assert!(err.contains("只有 1（Yes）/ 2（Cancel）"), "{err}");
        assert!(err.contains("零投递"), "{err}");
        assert!(sent.borrow().is_empty(), "越界零投递：{:?}", sent.borrow());
    }

    /// **picker：确认框阶段发 2（Cancel）后确认框仍在屏**（键被吞/回菜单读数滞后）
    /// → Confirm 如实交回用户（零投递不装成功）。
    #[test]
    fn pick_confirm_stage_still_on_screen_returns_confirm() {
        let sent: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let mut confirm_screen = e_stage2_screen("codex-perm-menu-newfooter-escback.txt");
        confirm_screen.push("  Enable full access?".to_string());
        let r = run_codex_menu_pick(
            2,
            || Some(confirm_screen.clone()),
            || Ok(None),
            || {},
            &mut Closures {
                read: || Some(confirm_screen.clone()),
                send: |k: &str| {
                    sent.borrow_mut().push(k.to_string());
                    Ok(())
                },
                settle: || {},
            },
        );
        match r {
            Ok(MenuPick::Confirm(_)) => {}
            other => panic!("确认框仍在屏 → Confirm 交回用户：{other:?}"),
        }
        assert_eq!(sent.borrow().as_slice(), ["2".to_string()]);
    }

    /// **picker：overlay 在屏 → 发该数字键（无回车）**，屏上出现确认框 → 返回 Confirm。
    #[test]
    fn pick_sends_digit_and_reports_confirm() {
        let sent: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let menu = e_stage2_screen("codex-perm-menu-newfooter-escback.txt");
        let cluster: std::cell::RefCell<Option<Vec<DialogOption>>> = std::cell::RefCell::new(None);
        let r = run_codex_menu_pick(
            4,
            || Some(menu.clone()),
            || {
                *cluster.borrow_mut() = Some(vec![DialogOption {
                    number: 1,
                    label: "Yes, continue anyway".to_string(),
                    highlighted: true,
                }]);
                Ok(cluster.borrow().clone())
            },
            || {},
            &mut Closures {
                read: || Some(menu.clone()),
                send: |k: &str| {
                    sent.borrow_mut().push(k.to_string());
                    Ok(())
                },
                settle: || {},
            },
        );
        match r {
            Ok(MenuPick::Confirm(opts)) => {
                assert_eq!(opts.len(), 1, "确认框选项交用户点");
            }
            other => panic!("应返回 Confirm：{other:?}"),
        }
        assert_eq!(sent.borrow().as_slice(), ["4"], "只发数字键、无回车");
    }

    /// **picker 第一动作（open）全链**：清场 → 开菜单 → 读回选项表（不挑档、不敲数字）。
    #[test]
    fn open_returns_options_without_picking() {
        let sent: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let screen = e_stage2_screen("codex-perm-menu-newfooter-escback.txt");
        let opens = std::cell::Cell::new(0usize);
        let r = run_codex_menu_open(
            || {
                opens.set(opens.get() + 1);
                Ok(())
            },
            || {
                let lowered: Vec<String> = screen.iter().map(|l| l.to_lowercase()).collect();
                Ok(codex_menu_options(&screen, &lowered))
            },
            || {},
            &mut Closures {
                // G1 起 preflight 要见到 composer 占位行（不在场 = overlay 占位 → esc）
                read: || Some(lines(&["› Ask Codex to do anything", "  普通输出"])),
                send: |k: &str| {
                    sent.borrow_mut().push(k.to_string());
                    Ok(())
                },
                settle: || {},
            },
        );
        let opts = r.expect("开菜单应成功并读回选项");
        assert_eq!(opts.len(), 4);
        assert_eq!(opens.get(), 1, "开菜单恰好一次");
        assert!(
            sent.borrow().is_empty(),
            "open 不敲任何数字键：{:?}",
            sent.borrow()
        );
    }

    // ==== 2026-09-23 codex 数字直达：判据级单测（真机夹具 + 变异锁）====

    /// **数字直达探测（真机四项夹具）**：四档各自读到**屏上编号** 1-4
    /// （含 `Approve for me` = 第 3 档——2026-09-23 起是可选目标档）。
    #[test]
    fn digit_probe_reads_screen_numbers_from_real_fixture() {
        let screen = e_stage2_screen("codex-perm-menu-4tier.txt");
        for (target, digit) in [
            (MamMode::ReadOnly, "1"),
            (MamMode::Default, "2"),
            (MamMode::AcceptEdits, "3"),
            (MamMode::Bypass, "4"),
        ] {
            assert_eq!(
                codex_permission_digit_probe(&screen, target),
                PollStep::Ready(digit.to_string()),
                "{target:?} 应读到屏上编号 {digit}"
            );
        }
    }

    /// **数字直达探测（Guardian 关的三档形态）**：`Approve for me` 缺席 →
    /// `Full Access` 的屏上编号前移为 3——编号从**目标行自身**读出，不做全集闸
    /// （变异自真机四项夹具：删第 3 档行 + 按真实渲染重编号，非手造屏）。
    #[test]
    fn digit_probe_survives_missing_approve_for_me_tier() {
        let mut screen = e_stage2_screen("codex-perm-menu-4tier.txt");
        screen.retain(|l| !l.contains("Approve for me"));
        for l in screen.iter_mut() {
            if l.contains("Full Access") {
                *l = l.replacen("4.", "3.", 1);
            }
        }
        assert_eq!(
            codex_permission_digit_probe(&screen, MamMode::Bypass),
            PollStep::Ready("3".to_string()),
            "Guardian 关时 Full Access 编号前移，编号直达照样读对"
        );
    }

    /// **数字直达探测的保守面**：目标标签在窗内出现多行（混入 `/status` 回显类
    /// 编号行）→ Fatal 不猜；无菜单 → NotYet（可重试）。
    #[test]
    fn digit_probe_refuses_ambiguous_or_absent_menu() {
        let mut screen = e_stage2_screen("codex-perm-menu-4tier.txt");
        // 在真菜单第 1 项行后插入一行编号行形态的混入行（窗内、含目标档名词）
        let pos = screen
            .iter()
            .position(|l| l.contains("1. Read Only"))
            .expect("夹具含第 1 项行")
            + 1;
        screen.insert(pos, "  6. Read Only (sandbox: read-only)".to_string());
        assert!(
            matches!(
                codex_permission_digit_probe(&screen, MamMode::ReadOnly),
                PollStep::Fatal(_)
            ),
            "目标标签两行 → Fatal（不猜编号）"
        );
        assert!(
            matches!(
                codex_permission_digit_probe(&lines(&["hello world"]), MamMode::ReadOnly),
                PollStep::NotYet(_)
            ),
            "无菜单 → NotYet（轮询可重试）"
        );
    }

    /// **残留 overlay 判定**：权限菜单**或** Full Access 确认框的标题锚在屏即判真
    /// （2026-09-23 二轮修复：确认框残留不在旧菜单锚判据内 → enter 误确认
    /// `1. Yes, continue anyway` = 意外启用完全信任的危害级误切）。
    #[test]
    fn residual_overlay_detection() {
        assert!(residual_overlay_present(&e_stage2_screen(
            "codex-perm-menu-4tier.txt"
        )));
        assert!(
            residual_overlay_present(&e_stage2_screen("codex-full-access-confirm.txt")),
            "确认框残留必须被清场判据覆盖"
        );
        assert!(!residual_overlay_present(&lines(&[
            "  glm-5.3-flash medium · ~\\proj-codex  Plan mode"
        ])));
    }

    /// **输入行残留判定（composer 判据）**——通用准则「斜杠命令注入前输入行必须
    /// 纯净」的判据锁。
    #[test]
    fn composer_residue_detection() {
        // 真机夹具：底部 composer = 占位文案（空输入行）→ 纯净
        let screen = e_stage2_screen("codex-input-residue.txt");
        assert_eq!(
            composer_residue(&screen),
            None,
            "占位 composer = 纯净（历史回显的 /permissions 在对话流中部，不得误判）"
        );
        // 变异（真机夹具为底）：占位行换成残留命令 → Some(字符数)
        let mut dirty = screen.clone();
        let pos = dirty
            .iter()
            .position(|l| l.contains("Ask Codex to do anything"))
            .expect("夹具含占位 composer 行");
        dirty[pos] = "\u{276f} /permissions/permissions".to_string();
        assert_eq!(
            composer_residue(&dirty),
            Some(24),
            "残留 24 字符 = backspace 清理键数"
        );
        // 无 footer（判据不可得）→ None（放行，守卫原则「判不清放行」）
        assert_eq!(composer_residue(&lines(&["some text"])), None);
        // 裸标记行（空 composer 另一形态）→ 纯净
        assert_eq!(
            composer_residue(&lines(&[
                "  glm-5.3-flash medium · ~\\proj-codex",
                "\u{276f}",
            ])),
            None
        );
    }

    /// **段 0.5：输入行残留清理全链**——composer 有残留 → backspace×N → 屏读验证
    /// 纯净 → 才开菜单（通用准则「斜杠命令注入前输入行必须纯净」）。
    #[test]
    fn stage_flow_digit_clears_composer_residue() {
        let mut dirty = e_stage2_screen("codex-input-residue.txt");
        let pos = dirty
            .iter()
            .position(|l| l.contains("Ask Codex to do anything"))
            .expect("夹具含占位 composer 行");
        dirty[pos] = "\u{276f} /permissions/permissions".to_string();
        let clean = e_stage2_screen("codex-input-residue.txt"); // 占位 = 已清干净
        let menu = e_stage2_screen("codex-perm-menu-4tier.txt");
        let receipt = lines(&["• Permissions updated to Read Only"]);
        let (r, sent, opens, _waits) =
            run_codex_stage_script(vec![dirty, clean, menu, receipt], MamMode::ReadOnly);
        let out = r.expect("输入行清干净后全链走通");
        let cleared: Vec<&String> = sent.iter().filter(|k| k.as_str() == "backspace").collect();
        assert_eq!(
            cleared.len(),
            24,
            "逐字符删除（残留 24 字符 backspace）：{sent:?}"
        );
        assert_eq!(
            sent.last().map(|s| s.as_str()),
            Some("1"),
            "清理后才是数字直达"
        );
        assert_eq!(opens, 1, "清干净才开菜单");
        assert_eq!(out.menu_keys, vec!["1"]);
        assert_eq!(out.receipt_seen, Some(true));
    }

    /// **段 2 窗尽兜底：/permissions 留在输入行未提交 → 补发一次 tab**（2026-09-23
    /// 用户实测开兜底；提交键语义 2026-10-10 3787fa7d 用户实测指令改为 **tab**：文本
    /// 注入+150ms 后的动作与残留补发均为 tab——codex TUI 忙时命令没被提交，菜单根本
    /// 没开；轮询窗尽后检查输入行，命令还在就补提交再等一窗；脚本：①干净屏
    /// ②命令未提交（/permissions 停在 composer）③补 tab 后菜单画出 ④回执。
    #[test]
    fn stage_flow_digit_reenter_when_command_not_submitted() {
        use std::cell::Cell;
        use std::cell::RefCell;
        // 场景脚本（按 digit 轮询的「窗」划分）：
        //   第一窗：窗尽 Err（菜单未出现）；窗尽后 read → composer 留着 /permissions
        //   补 tab → 第二窗：Ready("1")
        let digit_calls = Cell::new(0usize);
        let clean = clean_idle();
        let unsubmitted = lines(&[
            "\u{276f} /permissions",
            "  glm-5.3-flash medium · ~\\proj-codex",
        ]);
        let sent: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let outcome = run_codex_permission_stages(
            MamMode::ReadOnly,
            || Ok(()), // 开菜单（opens 断言不在本用例重点）
            || {
                if digit_calls.get() == 0 {
                    digit_calls.set(1);
                    Err("codex 的权限菜单未出现或读不到档位表（不盲发数字键）".to_string())
                } else {
                    Ok("1".to_string())
                }
            },
            || Ok(None),
            || Ok(None),
            || {}, // wait_floor
            &mut Closures {
                // 段 0 读干净屏；窗尽兜底读 → composer 留着 /permissions 的屏
                read: || {
                    if digit_calls.get() == 0 {
                        Some(clean.clone())
                    } else {
                        Some(unsubmitted.clone())
                    }
                },
                send: |k: &str| {
                    sent.borrow_mut().push(k.to_string());
                    Ok(())
                },
                settle: || {},
            },
        );
        let out = outcome.expect("补发 tab 后全链走通");
        assert_eq!(
            sent.borrow().first().map(|s| s.as_str()),
            Some("tab"),
            "补发的 tab 是第一个键（3787fa7d：残留补发键 enter→tab）"
        );
        assert_eq!(sent.borrow().last().map(|s| s.as_str()), Some("1"));
        assert_eq!(out.menu_keys, vec!["1"]);
        // 对照：composer 干净（无 /permissions）时窗尽 → 如实失败、不补 tab
        let read_hits = Cell::new(0usize);
        let sent2: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let digit_calls2 = Cell::new(0usize);
        let err = run_codex_permission_stages(
            MamMode::ReadOnly,
            || Ok(()),
            || {
                digit_calls2.set(digit_calls2.get() + 1);
                Err("窗尽".to_string())
            },
            || Ok(None),
            || Ok(None),
            || {},
            &mut Closures {
                read: || {
                    read_hits.set(read_hits.get() + 1);
                    Some(clean.clone())
                },
                send: |k: &str| {
                    sent2.borrow_mut().push(k.to_string());
                    Ok(())
                },
                settle: || {},
            },
        );
        assert!(err.is_err(), "无残留可补 → 如实失败");
        assert!(
            sent2.borrow().is_empty(),
            "composer 干净时窗尽零投递：{:?}",
            sent2.borrow()
        );
        assert_eq!(digit_calls2.get(), 1, "只轮询了一窗");
        let _ = read_hits.get();
    }

    /// **段 0.5：清不净 → 如实中止**（backspace 后屏上仍是残留 = 删除键未生效/
    /// 有异常——不盲发斜杠命令，零 /permissions 投递）。
    #[test]
    fn stage_flow_digit_unclearable_composer_aborts() {
        let mut dirty = e_stage2_screen("codex-input-residue.txt");
        let pos = dirty
            .iter()
            .position(|l| l.contains("Ask Codex to do anything"))
            .expect("夹具含占位 composer 行");
        dirty[pos] = "\u{276f} /permissions/permissions".to_string();
        // 单屏脚本：backspace 后屏面不变（清理未生效）
        let (r, sent, opens, _waits) = run_codex_stage_script(vec![dirty], MamMode::Default);
        let err = r.unwrap_err();
        assert!(err.contains("仍不纯净"), "清不净要如实中止：{err}");
        assert!(sent.iter().all(|k| k == "backspace"), "{sent:?}");
        assert_eq!(opens, 0, "纯净未验证前不得开菜单");
    }

    /// **实机取证探针（#[ignore]，零注入）**：codex composer 前缀码点定案工具。
    ///
    /// 背景：`composer_residue` 的判据=「footer 之上最近的光标标记行」（标记集合
    /// ›/❯/▶/>），但 **codex composer 前缀的真实码点无档案记录**——用户 2026-09-23
    /// 反馈输入行残留清理未生效，需要 composer 行的屏读原文+码点定判据。
    ///
    /// 跑法：codex TUI 正在运行、composer 里先随便打几个字符（如 `ab1`，不要提交）
    /// → `cargo test --lib codex_composer_codepoint -- --ignored --nocapture`
    /// → 输出=每个 codex 进程可见窗底部 14 行 + 每行前 4 字符的码点；贴回即定案。
    #[test]
    #[cfg(windows)]
    #[ignore = "实机取证：codex composer 前缀码点（前置=codex TUI 运行中 + composer 已打字）"]
    fn codex_composer_codepoint_live_probe() {
        use sysinfo::ProcessesToUpdate;
        let mut system = sysinfo::System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            sysinfo::ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::Always),
        );
        let pids: Vec<u32> = system
            .processes()
            .values()
            .filter(|p| {
                let name = p.name().to_string_lossy().to_lowercase();
                if name.contains("multi-agents-manager") || name.contains("tuvis") {
                    return false;
                }
                if name.contains("codex") {
                    return true;
                }
                // codex 经 npm 安装时主体是 `node.exe .../codex/bin/codex.js`——
                // 只按进程名找会漏（2026-09-23 实测：探针扫不到自建会话）。命令行
                // 里含 codex 包路径也算（与 adapter 的进程发现同口径）。
                let cmd = p
                    .cmd()
                    .iter()
                    .map(|c| c.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_lowercase();
                name.contains("node") && cmd.contains("codex")
            })
            .map(|p| p.pid().as_u32())
            .collect();
        eprintln!("发现 codex 进程：{pids:?}");
        if pids.is_empty() {
            eprintln!("未发现 codex 进程——请先启动 codex TUI 并在 composer 打几个字");
            return;
        }
        // 只读**自己 spawn 的**探针会话（红线：实机只碰自建进程）。默认不扫全机——
        // 用户会话的屏上含其对话内容，不应进入探针输出/夹具。
        let only: Option<u32> = std::env::var("MAM_PROBE_PID")
            .ok()
            .and_then(|s| s.parse().ok());
        let pids: Vec<u32> = match only {
            Some(p) => pids.into_iter().filter(|x| *x == p).collect(),
            None => {
                eprintln!("提示：未设 MAM_PROBE_PID，将 dump 全部 codex 进程（含用户会话）；建议只 dump 自建会话");
                pids
            }
        };
        for pid in pids {
            let Ok(lines) = crate::inject::windows_console::read_screen_window(pid) else {
                eprintln!("pid={pid} 屏读失败（无控制台/权限不足）");
                continue;
            };
            eprintln!(
                "==== pid={pid} 可见窗 {} 行；底部 14 行（前 4 字符码点）====",
                lines.len()
            );
            let start = lines.len().saturating_sub(14);
            for (i, l) in lines[start..].iter().enumerate() {
                let codes: Vec<String> = l
                    .chars()
                    .take(4)
                    .map(|c| format!("U+{:04X}", c as u32))
                    .collect();
                eprintln!("行{} {:?}  码点[{}]", start + i, l, codes.join(" "));
            }
            // 2026-09-23：菜单新形态取证——整屏逐行输出（`FIXTURE|` 前缀便于直接
            // 转存 tests/fixtures/e-stage2/*.txt；夹具红线禁止手造，故必须由本探针
            // 的实机 dump 原样落盘，人工只做"截取窗口"不做"改写文字"）
            eprintln!("---- FIXTURE| 起（pid={pid}，逐字原文，勿手改）----");
            for l in &lines {
                eprintln!("FIXTURE|{l}");
            }
            eprintln!("---- FIXTURE| 止 ----");
        }
    }

    /// **实机验证探针（#[ignore]）**：用**修复后的内核**读真实终端上的 codex 权限
    /// 菜单——本次事故的端到端复验（不是单测、不是夹具，是真的读一个开着的菜单）。
    ///
    /// 跑法：先开好 codex 权限菜单（注入 `/permissions` + 回车，或手动按）→
    /// `MAM_PROBE_PID=<pid> cargo test --lib codex_menu_live_kernel_probe -- --ignored --nocapture`
    ///
    /// 输出 = overlay 种类 + 读回的选项表（屏上编号/高亮/原文）+ composer 判据结论。
    /// **只碰 MAM_PROBE_PID 指定的进程**（红线：实机只碰自建会话）。
    #[test]
    #[cfg(windows)]
    #[ignore = "实机验证：codex 权限菜单读回（前置=菜单已打开 + MAM_PROBE_PID=<pid>）"]
    fn codex_menu_live_kernel_probe() {
        let Some(pid) = std::env::var("MAM_PROBE_PID")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            eprintln!("未设 MAM_PROBE_PID——跳过（红线：只碰自建会话进程）");
            return;
        };
        let Ok(lines) = crate::inject::windows_console::read_screen_window(pid) else {
            eprintln!("pid={pid} 屏读失败");
            return;
        };
        let lowered: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();
        eprintln!("---- pid={pid} 屏读 {} 行 ----", lines.len());
        eprintln!("overlay 种类：{:?}", codex_overlay_kind(&lowered));
        assert!(
            codex_overlay_present(&lowered),
            "屏上应有权限菜单（先开好菜单再跑本探针）"
        );
        let opts = codex_menu_options(&lines, &lowered).expect("修复后的内核必须能读回菜单选项");
        eprintln!("==== 读回的菜单选项（修复后内核，实机）====");
        for o in &opts {
            eprintln!(
                "  屏上编号 {} | 高亮={} | 文本={:?}",
                o.number, o.highlighted, o.label
            );
        }
        eprintln!("==== composer 判据（不得被菜单污染）====");
        eprintln!("codex_composer_text = {:?}", codex_composer_text(&lines));
        eprintln!("composer_residue    = {:?}", composer_residue(&lines));
        assert!(
            composer_residue(&lines).is_none(),
            "菜单在屏时不得判输入行有残留（否则段 0.5 会 backspace 清菜单）"
        );
        assert!(opts.len() >= 3, "真机菜单至少三档：{opts:?}");
        // 数字直达路径在新 footer 下同样可用（四档各自的屏上编号可读）
        for target in [
            MamMode::ReadOnly,
            MamMode::Default,
            MamMode::AcceptEdits,
            MamMode::Bypass,
        ] {
            match codex_permission_digit_probe(&lines, target) {
                PollStep::Ready(d) => eprintln!("  {target:?} → 屏上编号 {d} ✅"),
                other => panic!("{target:?} 数字直达应 Ready，实际 {other:?}"),
            }
        }
    }

    /// **codex 权限菜单逐环诊断探针（#[ignore]，零注入——「探针失明」的眼睛）**：
    /// 对 `MAM_PROBE_PID` 指定的 codex 进程跑生产同款 `read_screen_window`，把
    /// 权限菜单判据的**每一环**逐级打印（None/失败打印「到哪断了」）。
    ///
    /// 背景（2026-10-10 用户实机视频 vs 错误回执）：终端屏上 `Update Model
    /// Permissions` 菜单明明开着（视频帧可见标题+四项+footer），生产回执 G4 摘要
    /// 却报「菜单=不在/composer=在」——mpd 诊断批的 PS 采样器验证过菜单可读，但
    /// 那不是 Rust 生产代码路径。本探针把生产 Rust 判据链拆环取证，分歧点一眼
    /// 可见：
    ///
    /// 1. lowered 全屏（FIXTURE|行号|原文 dump，夹具红线的程序化提取源）；
    /// 2. `anchor_ledger::detect` PERMISSION_MENU/TITLE → 命中行号（未命中打印
    ///    疑似标题行，直接看词形漂移）；
    /// 3. PERMISSION_MENU/FOOTER **两个变体各自**逐行扫（账本 detect 认任意一句，
    ///    此处逐变体点名——footer 漂移时直接看到「哪句在、哪句不在」+ 码点 dump，
    ///    防中点类不可见字符漂移）；
    /// 4. `codex_menu_window` → (title_idx, footer_idx) 或 None；
    /// 5. `codex_overlay_kind` → 结果；
    /// 6. 生产定位路径：`locate_menu_items(lines, menu_labels("codex"))`（与
    ///    `MenuNavPlan::PermissionTier` 同款实参）→ 项数与各项 number/label/
    ///    highlighted + `menu_items_coherent` 结论 + `codex_permission_digit_probe`
    ///    端到端三态；
    /// 7. G1 `codex_composer_present` 判定（含两分支内部值）。
    ///
    /// 跑法：`MAM_PROBE_PID=<pid> cargo test --lib codex_permission_menu_live_probe -- --ignored --nocapture`
    /// 红线：只读零注入（不碰 CONIN$、不发键）；pid 由调用方显式给定（只碰自建会话）。
    #[test]
    #[cfg(windows)]
    #[ignore = "实机只读探针：codex 权限菜单逐环诊断（前置=MAM_PROBE_PID=<codex pid>，菜单开着跑）"]
    fn codex_permission_menu_live_probe() {
        use crate::inject::anchor_ledger::{scenario, slot};
        let Some(pid) = std::env::var("MAM_PROBE_PID")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            eprintln!("未设 MAM_PROBE_PID——跳过（红线：只碰显式指定的进程）");
            return;
        };
        let Ok(lines) = crate::inject::windows_console::read_screen_window(pid) else {
            eprintln!("pid={pid} 屏读失败（无控制台/权限不足）——断点：read_screen_window");
            return;
        };
        let lowered: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();
        eprintln!(
            "==== 环1 pid={pid} 可见窗 {} 行（FIXTURE|行号|原文）====",
            lines.len()
        );
        for (i, l) in lines.iter().enumerate() {
            eprintln!("FIXTURE|{i:02}|{l}");
        }
        // 疑似相关行的码点 dump（中点/光标标记类不可见字符漂移在此现形）
        eprintln!("---- 疑似行码点（含 permission/model/esc/select 的行）----");
        for (i, l) in lowered.iter().enumerate() {
            if l.contains("permission")
                || l.contains("model")
                || l.contains("esc")
                || l.contains("select")
            {
                let codes: Vec<String> = l
                    .chars()
                    .map(|c| {
                        if (c as u32) < 0x80 {
                            format!("{c}")
                        } else {
                            format!("U+{:04X}", c as u32)
                        }
                    })
                    .collect();
                eprintln!("行{i:02} {:?}", codes.join(""));
            }
        }
        // 环2：标题锚
        eprintln!("==== 环2 标题锚 detect（codex/PERMISSION_MENU/TITLE）====");
        match crate::inject::anchor_ledger::detect(
            &lowered,
            "codex",
            scenario::PERMISSION_MENU,
            slot::TITLE,
        ) {
            Some(hit) => eprintln!(
                "命中：行{} 文案「{}」（实测于 {}）",
                hit.line_index, hit.row.text, hit.row.observed_version
            ),
            None => {
                eprintln!("未命中 → overlay_kind 必 None、menu_window 必 None（断点=标题锚）");
                for (i, l) in lowered.iter().enumerate() {
                    if l.contains("permission") || l.contains("model") {
                        eprintln!("  疑似标题行 行{i}：{:?}", lines[i]);
                    }
                }
            }
        }
        // 环3：footer 两个变体各自逐行扫（与账本候选同词形）
        eprintln!("==== 环3 footer 锚两变体各自扫（codex/PERMISSION_MENU/FOOTER）====");
        const FOOTER_OLD: &str = "press enter to confirm or esc to go back";
        const FOOTER_NEW: &str = "enter select · esc back";
        for (name, text) in [("0.154.0 旧", FOOTER_OLD), ("0.156.1 新", FOOTER_NEW)] {
            let hits: Vec<usize> = lowered
                .iter()
                .enumerate()
                .filter(|(_, l)| l.contains(text))
                .map(|(i, _)| i)
                .collect();
            if hits.is_empty() {
                eprintln!("变体[{name}]「{text}」：未命中");
            } else {
                eprintln!("变体[{name}]「{text}」：命中行 {hits:?}");
            }
        }
        // 账本合口（认任意一句）的结论
        match crate::inject::anchor_ledger::detect(
            &lowered,
            "codex",
            scenario::PERMISSION_MENU,
            slot::FOOTER,
        ) {
            Some(hit) => eprintln!(
                "账本 FOOTER detect：命中行{} 文案「{}」（{}）",
                hit.line_index, hit.row.text, hit.row.observed_version
            ),
            None => eprintln!("账本 FOOTER detect：None（两变体都不在 → footer 锚断）"),
        }
        // 环4：锚窗
        eprintln!("==== 环4 codex_menu_window ====");
        match codex_menu_window(&lowered) {
            Some((t, f)) => eprintln!(
                "Some((title_idx={t}, footer_idx={f}))，窗内 {} 行",
                f - t - 1
            ),
            None => eprintln!("None（标题或 footer 锚不成窗——见环2/环3 哪个缺）"),
        }
        // 环5：overlay 种类
        eprintln!("==== 环5 codex_overlay_kind ====");
        eprintln!("{:?}", codex_overlay_kind(&lowered));
        // 环6：生产定位路径（MenuNavPlan::PermissionTier 同款实参）
        eprintln!("==== 环6 locate_menu_items（生产实参 menu_labels(codex)）====");
        match locate_menu_items(&lines, menu_labels("codex")) {
            Some(items) => {
                eprintln!("Some（{} 项）：", items.len());
                for o in &items {
                    eprintln!(
                        "  编号 {} | 高亮={} | {:?}",
                        o.number, o.highlighted, o.label
                    );
                }
                let coherent = menu_items_coherent(&items, "codex");
                eprintln!("menu_items_coherent = {coherent}");
            }
            None => eprintln!("None（无标题锚 或 窗内合法行 <2——见环2/环4）"),
        }
        eprintln!("==== 环6' codex_permission_digit_probe（ReadOnly 端到端三态）====");
        eprintln!(
            "{:?}",
            codex_permission_digit_probe(&lines, MamMode::ReadOnly)
        );
        // 环7：G1 composer 在场性（两分支内部值）
        eprintln!("==== 环7 codex_composer_present（G1）====");
        let placeholder_branch = lines.iter().any(|l| {
            let (rest, marked) = crate::inject::dialog::strip_cursor_marker(l);
            marked
                && CODEX_COMPOSER_PLACEHOLDERS
                    .iter()
                    .any(|p| rest.to_lowercase().contains(p))
        });
        let composer_text = codex_composer_text(&lines);
        eprintln!(
            "占位标记分支={placeholder_branch}；codex_composer_text={composer_text:?}；codex_composer_present={}",
            codex_composer_present(&lines)
        );
    }

    /// **kimi 只读屏读探针（#[ignore]）**：dump 指定 pid 的整屏原文，并跑一遍
    /// kimi 权限菜单定位器，输出「屏上有什么 / 现有判据认不认」——屏读故障诊断与
    /// 版本复验的活体取证入口（四闸门 Gate-1：判据结论前必有活体 dump）。
    ///
    /// 跑法：`MAM_PROBE_PID=<pid> cargo test --lib kimi_screen_live_dump -- --ignored --nocapture`
    ///
    /// **只读零注入**（不碰 CONIN$、不 Flush、不发键）；输出进探针控制台，
    /// 不落测试夹具（红线：用户会话屏面不进夹具）。
    #[test]
    #[cfg(windows)]
    #[ignore = "只读活体 dump（MAM_PROBE_PID=<pid>）"]
    fn kimi_screen_live_dump() {
        let Some(pid) = std::env::var("MAM_PROBE_PID")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            eprintln!("未设 MAM_PROBE_PID——跳过（只读探针不猜 pid）");
            return;
        };
        let Ok(lines) = crate::inject::windows_console::read_screen_window(pid) else {
            eprintln!("pid={pid} 屏读失败（无控制台/权限不足）");
            return;
        };
        eprintln!("==== pid={pid} 整屏 {} 行 ====", lines.len());
        for (i, l) in lines.iter().enumerate() {
            eprintln!("FIXTURE|{i:02}|{l}");
        }
        let items = locate_menu_items(&lines, menu_labels("kimi"));
        match items {
            Some(opts) => {
                eprintln!("==== kimi 权限菜单定位器：命中 {} 项 ====", opts.len());
                for o in &opts {
                    eprintln!(
                        "  编号 {} | 高亮={} | {:?}",
                        o.number, o.highlighted, o.label
                    );
                }
            }
            None => eprintln!("==== kimi 权限菜单定位器：未命中（标题/footer 锚或行形不匹配）===="),
        }
        // **属性层（粗体探测）**：视口各行属性压缩行程——高亮行（❯/▶ 前缀）与
        // 普通行的属性差 = 粗体/加亮是否属性级可读的判据
        match crate::inject::windows_console::read_screen_window_attrs(pid) {
            Ok(rows) => {
                eprintln!(
                    "==== 属性层 {} 行（行程压缩：起点x-长度:0x掩码）====",
                    rows.len()
                );
                for (i, row) in rows.iter().enumerate() {
                    let mut runs: Vec<String> = Vec::new();
                    let mut cur = row.first().copied().unwrap_or(0);
                    let mut start = 0usize;
                    for (x, a) in row.iter().enumerate() {
                        if *a != cur {
                            runs.push(format!("x{start}-{}:0x{cur:04X}", x - start));
                            cur = *a;
                            start = x;
                        }
                    }
                    runs.push(format!("x{start}-{}:0x{cur:04X}", row.len() - start));
                    eprintln!("ATTR {i:02} | {}", runs.join(" "));
                }
            }
            Err(e) => eprintln!("属性层读取失败：{e}"),
        }
    }

    /// **数字直达的输入行残留现场（真机夹具）**：用户走查截图里堆积的
    /// `/permissions/permissions` 与 `Plan mode` 底栏——同屏条件下 footer 解析
    /// 仍判 Plan（残留不干扰回读）、菜单判据不受历史回显影响。
    #[test]
    fn input_residue_fixture_keeps_footer_and_menu_verdicts() {
        let screen = e_stage2_screen("codex-input-residue.txt");
        assert_eq!(
            parse_mode_from_screen("codex", &screen),
            Some(MamMode::Plan),
            "底栏 `… · …  Plan mode` → 计划（残留不干扰回读）"
        );
        assert!(
            !residual_overlay_present(&screen),
            "历史回显的 `/permissions` 不算残留 overlay（判据是标题锚，不是命令词）"
        );
    }

    /// **确认框残留的清场（2026-09-23 二轮修复）**：屏上开着 `Enable full access?`
    /// （上一轮 Full Access 切换留下的确认框）→ esc 清场 → 等锚消失 → 才开菜单。
    /// 用户实测事故：确认框在场时判据漏判 → `/permissions` 被吞、enter 误确认。
    #[test]
    fn stage_flow_digit_clears_residual_confirm_box() {
        let residual = e_stage2_screen("codex-full-access-confirm.txt"); // 确认框在屏
        let clean_after_esc = clean_idle();
        let menu = e_stage2_screen("codex-perm-menu-4tier.txt");
        let receipt = lines(&["• Permissions updated to Read Only"]);
        let (r, sent, opens, _waits) = run_codex_stage_script(
            vec![residual, clean_after_esc, menu, receipt],
            MamMode::ReadOnly,
        );
        let out = r.expect("确认框残留清场后全链走通");
        assert_eq!(
            sent.first().map(|s| s.as_str()),
            Some("esc"),
            "**esc 先行**（清掉残留确认框）：{sent:?}"
        );
        assert_eq!(opens, 1, "清场完成才开菜单，恰好一次");
        assert_eq!(out.menu_keys, vec!["1"]);
        assert_eq!(out.receipt_seen, Some(true));
    }

    /// **清场失败（esc 无效）→ 如实中止，零后续投递**：单屏脚本（esc 后屏不变）。
    /// 2026-10-10 诊断批（G1）起，残留菜单的清场由 **G1 通用闸先行接管**（判据从
    /// 「账本锚消失」换成更通用的「composer 行回归」——菜单/确认框/审批框在屏时
    /// composer 行都被替换）→ 窗尽报「待处理交互」；段 0·续 的锚判据保留为纵深
    /// 防御，其自有失败文案由
    /// [`preflight_ledger_overlay_surviving_esc_aborts_with_anchor_message`] 锁定。
    #[test]
    fn stage_flow_digit_residue_wont_clear_aborts() {
        let residual = e_stage2_screen("codex-perm-menu-4tier.txt");
        let (r, sent, opens, _waits) = run_codex_stage_script(vec![residual], MamMode::Default);
        let err = r.unwrap_err();
        assert!(
            err.contains("待处理交互"),
            "G1 闸窗尽要如实中止（overlay 占位赶不走）：{err}"
        );
        assert_eq!(sent, vec!["esc"], "只发过清场 esc：{sent:?}");
        assert_eq!(opens, 0, "清场未完成不得开菜单（零 /permissions 投递）");
    }

    /// **段 0·续 的自有失败文案锁**（synthetic 形态）：composer 占位行与菜单标题锚
    /// **同屏**（真实屏两者互斥——overlay 在场时 composer 行被替换；此形态只在 TUI
    /// 渲染错序的间隙出现）→ G1 放行（composer 在场）、段 0·续 接管：esc 后锚
    /// 消不了 → 「仍未消失」如实中止。这条锁住「G1 先行不弱化残留清场守卫」。
    #[test]
    fn preflight_ledger_overlay_surviving_esc_aborts_with_anchor_message() {
        use std::cell::RefCell;
        let mut stuck = e_stage2_screen("codex-perm-menu-4tier.txt");
        stuck.push("› Ask Codex to do anything".to_string());
        let sent: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let err = codex_preflight(&mut Closures {
            read: || Some(stuck.clone()),
            send: |k: &str| {
                sent.borrow_mut().push(k.to_string());
                Ok(())
            },
            settle: || {},
        })
        .expect_err("锚消不了必须如实中止");
        assert!(err.contains("仍未消失"), "段 0·续 自有文案：{err}");
        assert_eq!(sent.borrow().as_slice(), ["esc"], "只发清场 esc");
    }

    // ==== 2026-10-10 诊断批加固四项：G1 composer 在场性闸 / G2 hijack 对账 /
    //      G3（账本行，测在 anchor_ledger）/ G4 屏面摘要 ====

    /// **G1 核心：审批框在屏（composer 不在）→ preflight Err 零投递**。
    ///
    /// 诊断定案（mpd-e7c-tr111 活体夹具）：「/permissions+回车」落在审批框在场
    /// 的终端上 = 回车**批准了待审批命令**（真实副作用：curl 被执行）。本闸在
    /// composer 不在场 + 审批框锚命中时**连 esc 都不发**（esc 在审批框上 = 拒绝
    /// 待审批命令，另一极副作用），处置权交用户。
    #[test]
    fn preflight_aborts_zero_inject_when_approval_box_on_screen() {
        use std::cell::RefCell;
        let screen = mp_screen("codex-mp-approval-box.txt");
        let sent: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let err = codex_preflight(&mut Closures {
            read: || Some(screen.clone()),
            send: |k: &str| {
                sent.borrow_mut().push(k.to_string());
                Ok(())
            },
            settle: || {},
        })
        .expect_err("审批框在场必须中止");
        assert!(
            err.contains("待审批命令"),
            "文案要点名审批框（回车会误批准）：{err}"
        );
        assert!(err.contains("请先在终端处置"), "处置权交用户：{err}");
        assert!(
            sent.borrow().is_empty(),
            "零投递（连 esc 都不发——esc 会拒绝待审批命令）：{:?}",
            sent.borrow()
        );
    }

    /// **G1 判据 × 活体夹具**：idle 主屏与 busy 帧（`◦ Working (… esc to interrupt)`
    /// 指示行 + composer 占位行同屏——`◦`=U+25E6 非光标标记）都判「composer 在场」
    /// → preflight 直通零键；审批框夹具判「不在场」（判据的负例对照）。
    #[test]
    fn composer_presence_judgment_on_live_fixtures() {
        let idle = mp_screen("codex-mp-idle-composer.txt");
        let busy = mp_screen("codex-mp-busy-composer.txt");
        let approval = mp_screen("codex-mp-approval-box.txt");
        assert!(
            codex_composer_present(&idle),
            "idle 主屏 composer 占位行恒在"
        );
        assert!(
            codex_composer_present(&busy),
            "busy 帧占位行仍在（busy 指示行是 ◦ 前缀，不冒充 composer 行）"
        );
        assert!(
            !codex_composer_present(&approval),
            "审批框在屏 = composer 行被替换（G1 的闸触发形态）"
        );
        // 用户已打字形态（占位行被输入文本替换）→ ② 分支（composer 行提取）判在场
        let typed = lines(&[
            "\u{276f} 帮我写个脚本",
            "  glm-5.3-flash medium · ~\\proj-codex",
        ]);
        assert!(codex_composer_present(&typed), "打字中的 composer 也算在场");
    }

    /// **G1 × idle/busy 活体屏全链直通**：preflight 后零键、流程可继续。
    #[test]
    fn preflight_passes_on_live_idle_and_busy_screens() {
        use std::cell::RefCell;
        for name in ["codex-mp-idle-composer.txt", "codex-mp-busy-composer.txt"] {
            let screen = mp_screen(name);
            let sent: RefCell<Vec<String>> = RefCell::new(Vec::new());
            let out = codex_preflight(&mut Closures {
                read: || Some(screen.clone()),
                send: |k: &str| {
                    sent.borrow_mut().push(k.to_string());
                    Ok(())
                },
                settle: || {},
            })
            .unwrap_or_else(|e| panic!("{name}: 干净主屏必须直通：{e}"));
            assert!(out.is_some(), "{name}: 返回清场后那屏");
            assert!(
                sent.borrow().is_empty(),
                "{name}: 干净屏零键：{:?}",
                sent.borrow()
            );
        }
    }

    /// **G1 × 未知 overlay：esc + 条件等待 composer 回归**——非审批框的 overlay
    /// （如 codex 问题框）在屏 → esc 一发、composer 回归后放行；esc 赶不走（单屏
    /// 脚本）→ 「待处理交互」如实中止（仍只发过那一发 esc）。
    #[test]
    fn preflight_escapes_unknown_overlay_then_waits_for_composer() {
        use std::cell::Cell;
        use std::cell::RefCell;
        let overlay = lines(&["  Confirm chunk?", "\u{203a} 1. Yes", "  2. No"]);
        let idle = clean_idle();
        // 成功路径：esc → 屏推进到 composer 回归
        let cursor = Cell::new(0usize);
        let screens = [overlay.clone(), idle];
        let sent: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let out = codex_preflight(&mut Closures {
            read: || {
                let s = &screens[cursor.get().min(screens.len() - 1)];
                Some(s.clone())
            },
            send: |k: &str| {
                sent.borrow_mut().push(k.to_string());
                Ok(())
            },
            settle: || cursor.set(cursor.get() + 1),
        })
        .expect("未知 overlay 被 esc 清掉后放行");
        assert_eq!(sent.borrow().as_slice(), ["esc"], "恰好一发 esc");
        assert!(out.is_some());
        // 失败路径：单屏（esc 后屏不变）→ 窗尽中止
        let sent2: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let err = codex_preflight(&mut Closures {
            read: || Some(overlay.clone()),
            send: |k: &str| {
                sent2.borrow_mut().push(k.to_string());
                Ok(())
            },
            settle: || {},
        })
        .expect_err("esc 赶不走的 overlay 必须中止");
        assert!(
            err.contains("待处理交互") && err.contains("未发任何命令"),
            "窗尽文案要点名待处理交互：{err}"
        );
        assert_eq!(sent2.borrow().as_slice(), ["esc"], "仍只发过那一发 esc");
    }

    /// **G2 hijack 对账**：数字直达窗尽 + composer 干净的失败路径上，最后一拍屏
    /// 出现审批结果行（`You approved codex to always run …`——回车落在了审批框上，
    /// 诊断批 E1 实证 curl 被执行）→ 错误文案升级点名「误批准」。
    #[test]
    fn digit_window_exhausted_with_approval_receipt_reports_hijack() {
        let hijacked = lines(&[
            "• You approved codex to always run curl -s https://example.com",
            "› Ask Codex to do anything",
            "  glm-5.3-flash medium · ~\\proj-codex",
        ]);
        let (r, sent, _opens, _waits) =
            run_codex_stage_script(vec![clean_idle(), hijacked], MamMode::ReadOnly);
        let err = r.expect_err("菜单没开成，且屏上有误批准回执");
        assert!(
            err.contains("误批准"),
            "窗尽 + 审批回执在场 → 文案必须升级点名误批准：{err}"
        );
        assert!(
            err.contains("权限切换未执行"),
            "同时如实说明切换没生效：{err}"
        );
        assert!(
            sent.iter()
                .all(|k| k != "1" && k != "2" && k != "3" && k != "4"),
            "数字键零投递（菜单从未出现）：{sent:?}"
        );
    }

    /// **G2 × picker open 路径**：read_options 窗尽 + 屏上有审批回执 → 同样点名。
    #[test]
    fn menu_open_failure_reconciles_hijack() {
        use std::cell::RefCell;
        let hijacked = lines(&[
            "• approved to always run curl -s https://example.com",
            "› Ask Codex to do anything",
            "  glm-5.3-flash medium · ~\\proj-codex",
        ]);
        let sent: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let err = run_codex_menu_open(
            || Ok(()),
            || Ok(None), // 菜单窗尽
            || {},
            &mut Closures {
                read: || Some(hijacked.clone()),
                send: |k: &str| {
                    sent.borrow_mut().push(k.to_string());
                    Ok(())
                },
                settle: || {},
            },
        )
        .expect_err("open 窗尽必须失败");
        assert!(err.contains("误批准"), "open 路径同样对账：{err}");
        assert!(
            sent.borrow().is_empty(),
            "open 不敲数字键：{:?}",
            sent.borrow()
        );
    }

    /// **G4 屏面摘要**：数字直达失败回执附三值快照（审批框/菜单/composer）——
    /// 审批框夹具在窗尽时的最后一拍屏 →「审批框=在/菜单=不在/composer=不在」；
    /// 读不到屏时如实写「读不到屏」。
    #[test]
    fn digit_failure_receipt_carries_screen_summary() {
        let approval = mp_screen("codex-mp-approval-box.txt");
        let (r, _sent, _opens, _waits) =
            run_codex_stage_script(vec![clean_idle(), approval], MamMode::ReadOnly);
        let err = r.expect_err("菜单窗尽（审批框占位）");
        assert!(
            err.contains("（屏面摘要：审批框=在/菜单=不在/composer=不在）"),
            "三值摘要要附在失败回执上：{err}"
        );
        // 读不到屏的摘要形态
        assert_eq!(codex_screen_summary(None), "（屏面摘要：读不到屏）");
    }

    // ==== 2026-10-10 codex-perm-blind 取证批：0.162.1 权限菜单活体夹具锁 ====
    //
    // 背景：用户实机视频里菜单开着、生产回执 G4 却报「菜单=不在/composer=在」，
    // 怀疑「PS 同构 ≠ Rust 同构」。本批活体取证（探针 codex_permission_menu_live_probe，
    // 证据 %USERPROFILE%\mam-probe-m6r\evidence\codex-perm-blind-20261010\）证明生产
    // Rust 判据链在 0.162.1 双宿主六形态**全绿**——判据不失明；夹具按红线由真机
    // dump 程序化提取（脚本 %TEMP%\extract-perm-fixtures-20261010.ps1），锁住 0.162.1
    // 的三个新屏形与一个主屏负例。

    /// **0.162.1 活体三形态 × 生产定位链**：conhost 新开菜单（窗内混 `Loading
    /// permissionapprovals…` 瞬态行）/ WT 描述折行 / busy 流式中开菜单（`(current)`
    /// 宽排版）——`locate_menu_items` 必须命中四项、一致性闸通过、数字直达四档
    /// 屏上编号齐备。**0.162.1 屏序 = 1:Ask for approval / 2:Approve for me /
    /// 3:Full Access / 4:Read Only**（与 0.154 的 1:Read Only 相反）——「编号取屏上
    /// 实读值」的语义由本锁钉死；菜单在屏时 composer 必判不在场。
    #[test]
    fn codex_permission_menu_0162_live_fixtures_locate_four_tiers() {
        for name in [
            "codex-mp-menu-fresh-loading.txt",
            "codex-mp-menu-wt-fold.txt",
            "codex-mp-menu-busy-overlay.txt",
        ] {
            let screen = mp_screen_in("mode-perm-20261010", name);
            let items = locate_menu_items(&screen, menu_labels("codex"))
                .unwrap_or_else(|| panic!("{name}：0.162.1 真机菜单必须解析"));
            assert_eq!(items.len(), 4, "{name}：四档齐备：{items:?}");
            assert!(menu_items_coherent(&items, "codex"), "{name}：四真项自洽");
            // 高亮恰一处（第 1 项 Ask for approval = 取证时的 current 档）
            assert!(items[0].highlighted, "{name}：高亮在第 1 项");
            assert_eq!(
                items.iter().filter(|o| o.highlighted).count(),
                1,
                "{name}：唯一高亮"
            );
            // 数字直达：四档各自的**屏上编号**（0.162.1 屏序，实测探针 Ready 同值）
            for (mode, want) in [
                (MamMode::Default, "1"),
                (MamMode::AcceptEdits, "2"),
                (MamMode::Bypass, "3"),
                (MamMode::ReadOnly, "4"),
            ] {
                match codex_permission_digit_probe(&screen, mode) {
                    PollStep::Ready(d) => {
                        assert_eq!(d, want, "{name}:{mode:?} 屏上编号取实读值")
                    }
                    other => panic!("{name}:{mode:?} 应 Ready，实际 {other:?}"),
                }
            }
            // 菜单占位 → composer 不在场（G1 与锚判据同向）
            assert!(
                !codex_composer_present(&screen),
                "{name}：菜单在屏时 composer 必不在场"
            );
        }
    }

    /// **Loading 瞬态行不产项**（0.162.1 新形态）：开菜单瞬间窗内混有
    /// `›    Loading permissionapprovals…`——带光标标记但非编号行，夹在第 3、4 项
    /// 之间。不得产生第 5 项、不得破坏一致性、不得多出高亮位（真机夹具锁定）。
    #[test]
    fn codex_permission_menu_loading_line_is_not_an_item() {
        let screen = mp_screen_in("mode-perm-20261010", "codex-mp-menu-fresh-loading.txt");
        assert!(
            screen
                .iter()
                .any(|l| l.contains("Loading permissionapprovals")),
            "夹具必须含 Loading 瞬态行（取证特征在位）"
        );
        let items = locate_menu_items(&screen, menu_labels("codex")).expect("菜单必须解析");
        assert_eq!(items.len(), 4, "Loading 行不得产项：{items:?}");
        assert!(
            items
                .iter()
                .all(|o| !o.label.to_lowercase().contains("loading")),
            "Loading 行不得入表：{items:?}"
        );
        assert_eq!(
            items.iter().filter(|o| o.highlighted).count(),
            1,
            "瞬态行的 `›` 不得多出高亮位"
        );
        assert!(menu_items_coherent(&items, "codex"));
    }

    /// **esc 后 idle 主屏 = 用户失败回执 G4「菜单=不在/composer=在」的同态**
    /// （0.162.1 真机）：零锚、定位 None、composer 在场——判据对该态的判定全部
    /// 正确（读屏那一刻菜单不在那块屏上）。G4 摘要对本态附**主屏注记**。
    #[test]
    fn codex_idle_after_esc_fixture_reads_as_main_screen() {
        let screen = mp_screen_in("mode-perm-20261010", "codex-mp-idle-after-esc.txt");
        let lowered: Vec<String> = screen.iter().map(|l| l.to_lowercase()).collect();
        assert_eq!(codex_overlay_kind(&lowered), None, "idle 主屏零锚");
        assert!(locate_menu_items(&screen, menu_labels("codex")).is_none());
        assert!(codex_composer_present(&screen), "composer 占位在场");
        let summary = codex_screen_summary(Some(&screen));
        assert!(
            summary.contains("主屏"),
            "「菜单=不在+composer=在」摘要要点名主屏态（非判据失明）：{summary}"
        );
        // 对照：审批框在场的摘要不附主屏注记（既有断言的精确文本不含注记）
        let approval = mp_screen("codex-mp-approval-box.txt");
        assert!(
            !codex_screen_summary(Some(&approval)).contains("主屏"),
            "审批框占位（composer=不在）不附主屏注记"
        );
    }

    /// **kimi 两段式**（无第三段）：菜单闭环 → enter → 回执 `Permission mode: Always Ask`。
    /// kimi 的每档描述行夹在档位行之间——这条同时锁住「kimi 形态下闭环仍从实际高亮位走」。
    #[test]
    fn stage_flow_kimi_menu_only_with_receipt() {
        let menu = kimi_menu_real(); // 高亮在 Always Ask（= 目标档 Default）
        let receipt = lines(&["Permission mode: Always Ask"]);
        let script = vec![menu, receipt];
        let (r, sent, polls) =
            run_stage_script(script, "kimi", ModeGroupId::Permission, MamMode::Default);
        let out = r.expect("kimi 总是询问（两段式）");
        assert_eq!(out.menu_keys, vec!["enter"], "高亮已在目标档 → 直接提交");
        assert_eq!(sent, vec!["enter"], "kimi 无第三段");
        assert_eq!(out.receipt_seen, Some(true), "kimi 回执锚 + 档位标签");
        assert_eq!(polls.len(), 1, "只有菜单一轮轮询");
        assert!(!out.confirm_done, "kimi 永不走确认框");
    }

    /// **kimi 向下走两步**（目标 = 永不询问，高亮在 Always Ask）→ `↓×2 + enter`，
    /// 每步都从当前屏复核（脚本三屏：高亮 1 → 2 → 3）。
    #[test]
    fn stage_flow_kimi_steps_down_to_target() {
        let s1 = kimi_menu_highlight(1);
        let s2 = kimi_menu_highlight(2);
        let s3 = kimi_menu_highlight(3); // Never Ask
        let receipt = lines(&["Permission mode: Never Ask"]);
        let script = vec![s1, s2, s3, receipt];
        let (r, sent, _) =
            run_stage_script(script, "kimi", ModeGroupId::Permission, MamMode::Bypass);
        let out = r.expect("kimi 三档两步");
        assert_eq!(out.menu_keys, vec!["down", "down", "enter"]);
        assert_eq!(sent, vec!["down", "down", "enter"]);
        assert_eq!(out.receipt_seen, Some(true));
    }
    /// kimi 菜单的高亮**人为摆到第 n 档**（同 [`codex_menu_with_highlight`]；只用于
    /// 导航逻辑测试，不是真机原文的逐字复制）。
    fn kimi_menu_highlight(hl: usize) -> Vec<String> {
        let mut screen = kimi_menu_real();
        for l in screen.iter_mut() {
            if crate::inject::dialog::strip_cursor_marker(l).1 {
                *l = l.replacen('\u{276f}', " ", 1);
            }
        }
        let mut count = 0usize;
        for l in screen.iter_mut() {
            let (rest, _) = crate::inject::dialog::strip_cursor_marker(l);
            let t = rest.trim();
            if !menu_labels("kimi").iter().any(|lab| t.contains(lab)) {
                continue;
            }
            count += 1;
            if count == hl {
                *l = format!("❯{}", rest);
                break;
            }
        }
        screen
    }

    /// **kimi 预选直通**（2026-10-06 权限组全两段式的关键收益）：`/yolo`/`/auto` 开菜单
    /// 后高亮（❯/▶）已在目标档（戊探D 夹具锁 `e3_kimi_yolo_auto_fixtures_highlight_preselect`）
    /// → 闭环 **0 方向键**直接回车，随后回执核验命中（`Permission mode: <档>` 锚）。
    #[test]
    fn stage_flow_kimi_preselected_goes_straight_to_enter() {
        // /yolo：预选第 2 档（Ask When Needed）
        let (r, sent, _) = run_stage_script(
            vec![
                kimi_menu_highlight(2),
                lines(&["Permission mode: Ask When Needed"]),
            ],
            "kimi",
            ModeGroupId::Permission,
            MamMode::AcceptEdits,
        );
        let out = r.expect("预选直通（yolo）");
        assert_eq!(
            out.menu_keys,
            vec!["enter"],
            "高亮已在目标档 → 不发任何方向键，唯一动作是回车"
        );
        assert_eq!(sent, vec!["enter"]);
        assert_eq!(out.receipt_seen, Some(true), "回执核验接上（锚+档标签）");
        // /auto：预选第 3 档（Never Ask）
        let (r, sent, _) = run_stage_script(
            vec![
                kimi_menu_highlight(3),
                lines(&["Permission mode: Never Ask"]),
            ],
            "kimi",
            ModeGroupId::Permission,
            MamMode::Bypass,
        );
        let out = r.expect("预选直通（auto）");
        assert_eq!(out.menu_keys, vec!["enter"]);
        assert_eq!(sent, vec!["enter"]);
        assert_eq!(out.receipt_seen, Some(true));
    }

    /// **菜单不出现（窗尽 `Ok(None)`）→ 整条编排 Err**（第二段是「必须出现」的门，
    /// 与确认框的「可出现」成对）
    #[test]
    fn stage_flow_menu_missing_aborts() {
        let sent: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let r = run_menu_stages(
            "codex",
            ModeGroupId::Permission,
            MamMode::Bypass,
            |_plan, optional| {
                // 第二段（optional=false）→ 窗尽返回 None；第三段（optional=true）不可能走到
                assert!(!optional, "菜单不出现就不该走到确认框轮询");
                Ok(None)
            },
            || Ok(None),
            &mut Closures {
                read: || Some(lines(&["  普通输出"])),
                send: |k: &str| {
                    sent.borrow_mut().push(k.to_string());
                    Ok(())
                },
                settle: || {},
            },
        );
        let err = r.as_ref().unwrap_err();
        assert!(
            err.contains("权限菜单未出现"),
            "菜单不出现必须如实说明（且不带任何键）：{err}"
        );
        assert!(sent.borrow().is_empty(), "零按键：{:?}", sent.borrow());
    }
}
