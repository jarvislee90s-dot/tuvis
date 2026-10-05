//! H10 zcode 无头**新建**（Task 12）：手机端在指定项目里无头建一个 zcode 会话并把首句
//! 注入进去，回执新 `sess_id` + 可见性分层提示。
//!
//! # 与 H7（Task 8）的关系：**同一通道的另一种命令形态**
//! 新建 = [`super::zcode::build_create_argv`]（`resume = None`，argv 里**没有** `--resume`）
//! + 同一套回合机械（并发名额/看门狗/取消/kill 树/争用锁重试全在
//! [`super::zcode::run_turn`] —— 本模块**不重写**）。
//!
//! # 新建特有的三件事（本模块的全部职责）
//! 1. **候选列表纯核**：`recentProjects`（`~/.zcode/v2/setting.json`，**只读**，主源）
//!    ∪ 看板快照项目，按存在性过滤、按工作区口径去重、如实标注信任档；
//! 2. **手填路径校验纯核**：路径合法 + 盘符在场 + **黑名单同源文件预览黑名单**
//!    （[`crate::remote::files`] 的 SENSITIVE 口径，不另造第二份）+ **绝不递归建目录**
//!    （叶节点缺席而父目录不在场 → 如实拒绝并点名原因）；
//! 3. **新会话号的确认**（**诚实红线：确认不到就说确认不到，绝不编造 sess_id**）：
//!    stdout JSON 帧给出 `sessionId` 为一等来源；否则**只认会话库**——创建前不在册、
//!    创建后出现在该项目下的**合规新会话**（`sess_` + UUID）才算数：0 个 = 未确认、
//!    1 个 = 确认、≥2 个 = 归属不可判定（如实报歧义，不挑一个冒充）。回执的
//!    `lastAssistant`/`tokens` 经 [`super::zcode::StoreProbe`] 从会话库取（**Task 8 同源**），
//!    stdout 只作完成信号。
//!
//! # 提示面（两端定案）
//! - **可见性分层**：项目在 APP 信任表内 ⇒「重启 ZCode 应用后可见」；否则「仅 MAM 可见」
//!   ——映射复用 Task 7 单点 [`crate::inject::routing::zcode_visibility`]（经
//!   [`super::zcode::visibility_of`]），**不另造第二份**；
//! - **黄字信号**：同项目已有在册 zcode 会话 ⇒ 提示（**不拦截**）——沿用配对不确定门
//!   的同款「诚实提示、不阻断」取向（同项目多开时卡片↔会话的对应关系本就不确定）。
//!
//! # 测试纪律（宪法级）
//! 单测**绝不** spawn 真 zcode、绝不读真实 `~/.zcode`：安装发现走 `cfg(test)` 空表
//! （[`super::zcode::production_roots`]）、真实 `~/.zcode` 只经注入缝（tempdir 夹具或
//! 脚本桩）、目录创建走记录桩（不碰真实文件系统）。
use std::path::{Path, PathBuf};

use super::receipt::{Receipt, ReceiptStatus, Stage};
use super::runner::RunnerCfg;
use super::turn::RunSeam;
use super::zcode::{self, TurnDeps, ZcodeInvocation};
use crate::inject::routing::Visibility;
use crate::monitor::zcode_parser::is_valid_session_id;
use crate::session::{AgentType, Session, SessionStatus};

/// 首句默认（spec H10：探针 `hi`——会话物化条件 PZ 复核）。端点与移动端都读它（单一来源）。
pub const DEFAULT_FIRST_TEXT: &str = "hi";

// ============================================================
// 候选列表（纯核）
// ============================================================

/// 候选来源（**主源 = APP 信任表**；看板快照是补充）
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateSource {
    /// `~/.zcode/v2/setting.json` 的 `recentProjects`（APP 已信任/最近打开的工作区）
    Trusted,
    /// 看板快照里的项目（MAM 已看见的会话所属目录）
    Board,
}

/// 候选项目（`trusted = false` 的候选**务必如实标注**：未信任目录的新会话 APP 永不收录）。
/// **本结构不 derive Serialize**：线上 JSON 由端点组装（每条另带 `note` 逐字可见性文案），
/// 本结构只是纯核产物——避免出现「看着能序列化、线上形状其实更宽」的假面
/// （`CandidateSource` 仍 derive Serialize：端点的 `source` 词就取自它）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCandidate {
    /// 路径**原样保留**来源形态（信任表里的写法 / 看板卡片里的写法）
    pub path: String,
    pub source: CandidateSource,
    /// 是否在 APP 信任表内（false ⇒ 新会话仅 MAM 可见）
    pub trusted: bool,
}

/// 候选列表纯核：`trusted`（**主源**，保序）∪ `board`（补充），
/// ① **准入过滤**（`admissible` 由调用方注入：生产 = 路径在场 ∧ **不在同源黑名单内**
/// ——列表不提供创建路径必然拒绝的候选，复审 Minor 5）；② 按工作区口径去重
/// （[`zcode::same_workspace`]，信任表条目胜出）。
pub fn candidates(
    trusted: &[String],
    board: &[String],
    admissible: &dyn Fn(&str) -> bool,
    os: &str,
) -> Vec<ProjectCandidate> {
    let mut out: Vec<ProjectCandidate> = Vec::new();
    for (list, source) in [
        (trusted, CandidateSource::Trusted),
        (board, CandidateSource::Board),
    ] {
        for path in list {
            if path.trim().is_empty() || !admissible(path) {
                continue; // 缺席/被黑名单拒的目录不提供选择（选了也建不出来）
            }
            if out.iter().any(|c| zcode::same_workspace(&c.path, path, os)) {
                continue; // 同工作区去重（信任表主源胜出——先入者即主源）
            }
            out.push(ProjectCandidate {
                path: path.clone(),
                source,
                // 信任档按**同一份信任判据**如实标注（不是按来源猜）
                trusted: trusted.iter().any(|t| zcode::same_workspace(t, path, os)),
            });
        }
    }
    out
}

// ============================================================
// 手填路径校验（纯核）
// ============================================================

/// 手填路径的拒绝原因（**每条都点名原因**——拒绝臂必须说清为什么，不返回含糊的失败）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManualPathReject {
    /// 空 / 全空白
    Empty,
    /// 不是完整绝对路径（相对路径、`C:proj` 这类盘符相对路径、POSIX 上的相对路径）
    NotAbsolute,
    /// 盘符/卷根不在场（Windows `Z:\` 不存在、UNC 共享根不可达）
    DriveMissing,
    /// 命中敏感目录黑名单（**同源文件预览黑名单**：`remote::files` 的 SENSITIVE 口径）
    Sensitive,
    /// 路径已存在但不是目录（如指向一个文件）
    NotDirectory,
    /// 叶节点缺席且**父目录也不在场** —— 需要递归建目录，本功能**绝不做**（如实拒绝）
    NeedsRecursiveCreation,
    /// 叶节点缺席且父路径在场但不是目录（如 `…/file.txt/x`）
    ParentNotDirectory,
}

impl ManualPathReject {
    /// 用户可读原因（后端给文案，移动端只渲染——与 H3/H7 同款单一措辞出口）
    pub fn reason(&self) -> String {
        match self {
            ManualPathReject::Empty => "项目路径不能为空".to_string(),
            ManualPathReject::NotAbsolute => {
                "项目路径必须是完整绝对路径（如 D:\\projects\\demo 或 /Users/me/demo）".to_string()
            }
            ManualPathReject::DriveMissing => {
                "项目路径所在盘符/卷根不在场（盘符不存在或未挂载）".to_string()
            }
            ManualPathReject::Sensitive => {
                "项目路径命中敏感目录黑名单（与文件预览同一口径：凭据/会话数据目录不可选）"
                    .to_string()
            }
            ManualPathReject::NotDirectory => "项目路径已存在但不是目录".to_string(),
            ManualPathReject::NeedsRecursiveCreation => {
                "项目路径的父目录不存在——本功能不递归创建目录，请先在磁盘上建好上级目录（或改选已存在的目录）"
                    .to_string()
            }
            ManualPathReject::ParentNotDirectory => {
                "项目路径的上级不是目录（父路径被同名文件占位）".to_string()
            }
        }
    }
}

/// 校验结论：**该目录当前该怎么落地**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathPlan {
    /// 目录已在场（直接用）
    Existing,
    /// 叶节点缺席但**父目录在场** → 单层创建该叶节点（**绝不递归**）
    CreateHere,
}

/// 文件系统观测缝（纯核与真实 IO 的**唯一**接触面）。
///
/// `sensitive` 是黑名单判据——生产接线**必须**指向 `remote::files` 的文件预览口径
/// （[`production_probe`]），**不得**在本模块另写一份目录名表。
///
/// `Send + Sync` 是硬要求：端点 handler 会把观测缝跨 await 持有（axum 的 `Handler`
/// 要求 future 为 `Send`），缺了它 handler 直接编译不过。
pub struct FsProbe<'a> {
    pub exists: Box<dyn Fn(&str) -> bool + Send + Sync + 'a>,
    pub is_dir: Box<dyn Fn(&str) -> bool + Send + Sync + 'a>,
    pub sensitive: Box<dyn Fn(&str) -> bool + Send + Sync + 'a>,
}

/// 生产观测缝：`exists`/`is_dir` 走 `std::fs`，`sensitive` **直接调** [`crate::remote::files`]
/// （同一份 `SENSITIVE_DIRS` / 豁免表 / fail-closed 分支——单一真源）。
/// `home` = 主目录基准（None = 读不到 → fail-closed 全段匹配，与文件预览同分支）。
pub fn production_probe(home: Option<&Path>, windows: bool) -> FsProbe<'static> {
    let home: Option<PathBuf> = home.map(Path::to_path_buf);
    FsProbe {
        exists: Box::new(|p: &str| Path::new(p).exists()),
        is_dir: Box::new(|p: &str| Path::new(p).is_dir()),
        // **单一真源**：文件预览的黑名单口径（含豁免表与 fail-closed 分支）
        sensitive: Box::new(move |p: &str| {
            crate::remote::files::project_path_rejected(Path::new(p), home.as_deref(), windows)
        }),
    }
}

/// 路径**可用形态**归一（校验/创建/端点回执共用一口径；**不折叠大小写**——这是要拿去
/// 建目录/传给 `--cwd` 的字面路径，折叠会改掉用户磁盘上的真实大小写）：
/// 去首尾空白 + 分隔符统一 `/` + 去尾分隔符（卷根 `E:/`、`/` 除外，去了就成相对形态）。
fn norm_project_path(path: &str, windows: bool) -> String {
    let t = path.trim().replace('\\', "/");
    let trimmed = t.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    if windows && trimmed.len() == 2 && trimmed.ends_with(':') {
        return format!("{trimmed}/");
    }
    trimmed.to_string()
}

/// 绝对路径的**卷根**（Windows：`E:/`、UNC `//host/share`；POSIX：`/`）。
/// `None` = 不是完整绝对路径（相对路径、`C:proj` 这类盘符相对形式）。
fn volume_root(norm: &str, windows: bool) -> Option<String> {
    if !windows {
        return norm.starts_with('/').then(|| "/".to_string());
    }
    let b = norm.as_bytes();
    if norm.starts_with("//") {
        let mut parts = norm.split('/').filter(|s| !s.is_empty());
        let host = parts.next()?;
        let share = parts.next()?;
        return Some(format!("//{host}/{share}"));
    }
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/' {
        return Some(format!("{}:/", (b[0] as char).to_ascii_uppercase()));
    }
    None
}

/// 父目录（归一形态；`E:/new` 的父是 `E:/`——盘符根不能退化成 `E:`）
fn parent_of(norm: &str) -> Option<String> {
    let (head, _) = norm.rsplit_once('/')?;
    if head.is_empty() {
        Some("/".to_string())
    } else if head.len() == 2 && head.ends_with(':') {
        Some(format!("{head}/"))
    } else {
        Some(head.to_string())
    }
}

/// 手填路径校验纯核（判定顺序即理由顺序，勿随意调换——黑名单先于存在性：
/// 敏感目录即使不存在也不可选，避免「先建出来再拦」）：
/// 空 → 绝对性 → 盘符在场 → **黑名单** → 已在场（须是目录）→ 叶节点缺席时父目录在场性。
pub fn validate_manual_path(
    path: &str,
    windows: bool,
    probe: &FsProbe<'_>,
) -> Result<PathPlan, ManualPathReject> {
    if path.trim().is_empty() {
        return Err(ManualPathReject::Empty);
    }
    let p = norm_project_path(path, windows);
    let Some(vol) = volume_root(&p, windows) else {
        return Err(ManualPathReject::NotAbsolute);
    };
    if !(probe.exists)(&vol) {
        return Err(ManualPathReject::DriveMissing);
    }
    if (probe.sensitive)(&p) {
        return Err(ManualPathReject::Sensitive);
    }
    if (probe.exists)(&p) {
        return if (probe.is_dir)(&p) {
            Ok(PathPlan::Existing)
        } else {
            Err(ManualPathReject::NotDirectory)
        };
    }
    // 叶节点缺席：**父目录必须在场且是目录** —— 否则就要递归建目录，本功能绝不做
    let Some(parent) = parent_of(&p) else {
        return Err(ManualPathReject::NotAbsolute);
    };
    if !(probe.exists)(&parent) {
        return Err(ManualPathReject::NeedsRecursiveCreation);
    }
    if !(probe.is_dir)(&parent) {
        return Err(ManualPathReject::ParentNotDirectory);
    }
    Ok(PathPlan::CreateHere)
}

// ============================================================
// 新会话确认（纯核；诚实红线）
// ============================================================

/// 会话库在册会话（**只读**读缝产物：id + 目录 + 建行时刻）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSession {
    pub id: String,
    pub directory: String,
    /// `session.time_created`（毫秒；真机表列，2026-10-05 只读 PRAGMA 核实）。
    /// `None` = 该列缺失/类型漂移（升级改表）⇒ 判据退回 id 基线（**无时间证据**，
    /// 不因此废掉整条发现链）。
    pub created_at: Option<i64>,
}

/// 新建发现结论（**不猜**）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewSessionVerdict {
    /// 恰好一个创建前不在册的新会话 —— 确认
    Single(String),
    /// 没有新会话（老会话被更新**不算**新建）
    None,
    /// 出现多个新会话 —— 归属不可判定，如实报数量（绝不挑一个冒充）
    Ambiguous(usize),
}

/// 创建前不在册、创建后落在**该项目**下的新会话（纯核）。
///
/// 准入三道（复审 Important 2 后加固）：
/// 1. 合规 id（[`crate::monitor::zcode_parser::is_valid_session_id`]：`sess_` + UUID ——
///    子代理/脏数据天然被拒）；
/// 2. 目录属该项目（工作区口径 [`zcode::same_workspace`]）；
/// 3. **建行时刻 ≥ `since_ms`（回合起点）** —— 主证据：创建于本回合之前的会话
///    **结构上不可能**被判成「新建」，与基线读得成不成功无关（基线读失败一次再也
///    骗不出假确认）。`created_at` 缺失（老库/改表）时该道放行，退由「不在基线内」承担。
///
/// `since_ms` = 回合起跑前取的墙钟毫秒。
pub fn new_session_of(
    project: &str,
    before: &[StoredSession],
    now: &[StoredSession],
    os: &str,
    since_ms: i64,
) -> NewSessionVerdict {
    let fresh: Vec<&StoredSession> = now
        .iter()
        .filter(|s| {
            is_valid_session_id(&s.id)
                && zcode::same_workspace(&s.directory, project, os)
                && !before.iter().any(|b| b.id == s.id)
                && s.created_at.map(|t| t >= since_ms).unwrap_or(true)
        })
        .collect();
    match fresh.len() {
        0 => NewSessionVerdict::None,
        1 => NewSessionVerdict::Single(fresh[0].id.clone()),
        n => NewSessionVerdict::Ambiguous(n),
    }
}

// ============================================================
// 提示面（黄字信号 + 可见性分层）
// ============================================================

/// 该项目里**在册**的 zcode 会话数（黄字信号的判据；`Finished` 除外——已结束的会话
/// 不构成「同项目已有活跃会话」）。工具/工作区口径分别复用
/// [`crate::session::AgentType::ZCode`] 与 [`zcode::same_workspace`]。
pub fn active_zcode_in_project(project: &str, sessions: &[Session], os: &str) -> usize {
    sessions
        .iter()
        .filter(|s| {
            s.agent_type == AgentType::ZCode
                && s.status != SessionStatus::Finished
                && zcode::same_workspace(&s.project_path, project, os)
        })
        .count()
}

/// 新建回执的**提示面**（纯核；文案单点，端点与移动端只渲染不另编）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateHints {
    pub visibility: Visibility,
    /// 可见性文案（逐字来自 [`Visibility::note`]）
    pub visibility_note: String,
    /// 黄字信号（`None` = 同项目无在册 zcode 会话）——**不拦截**，只提示
    pub warning: Option<String>,
}

/// 提示面组装：可见性档（Task 7 单点映射）+ 同项目在册 zcode 会话的黄字信号。
pub fn create_hints(
    project: &str,
    visibility: Visibility,
    board: &[Session],
    os: &str,
) -> CreateHints {
    let n = active_zcode_in_project(project, board, os);
    CreateHints {
        visibility,
        visibility_note: visibility.note().to_string(),
        warning: (n > 0).then(|| {
            format!(
                "同一项目已有 {n} 个在册 zcode 会话（同项目多开）——新建会话可能与既有卡片混淆，\
                 且 ZCode 应用正在该项目活动时无头回合会撞工作区争用锁；本提示**不拦截**本次创建"
            )
        }),
    }
}

// ============================================================
// 新建回合编排
// ============================================================

/// 单层目录创建缝（生产 = `std::fs::create_dir`——**非** `create_dir_all`；测试 = 记录桩，
/// 零真实文件系统接触）。类型别名只为收敛 clippy::type_complexity，语义即字面。
pub type EnsureDirFn = dyn Fn(&str) -> Result<(), String> + Send + Sync;

/// 会话库**在册会话**读缝（只读；生产 = [`crate::monitor::zcode_parser::stored_sessions_home`]）。
///
/// **返回 `Option`**（复审 Important 2）：「库不可读」（`None`）必须与「该项目没有会话」
/// （`Some(空表)`）区分开——判据的输入若把两者混为一谈，一次瞬时读失败就能把在册旧会话
/// 说成本轮新建。
pub type StoredSessionsFn = dyn Fn() -> Option<Vec<StoredSession>> + Send + Sync;

/// 新建回合的依赖：**回合机械复用 Task 8 的 [`zcode::TurnDeps`]**（探活/等待/库读/
/// 争用锁重试节奏单源），本结构只补两件创建特有的缝。
pub struct CreateDeps {
    /// 平台（路径口径与命令形态分叉的唯一输入）
    pub os: &'static str,
    /// Task 8 回合机械（生产经 [`CreateDeps::production`] 装配）
    pub turn: TurnDeps,
    /// 会话库**在册会话 (id, 目录)** 读缝（只读；创建基线对比用）
    pub stored_sessions: Box<StoredSessionsFn>,
    /// **单层**目录创建缝（叶节点缺席而父目录在场时；绝不递归——见 [`PathPlan::CreateHere`]）
    pub ensure_dir: Box<EnsureDirFn>,
}

impl CreateDeps {
    /// 生产依赖（回合机械 = Task 8 的 [`zcode::TurnDeps::production`]；库读 = 只读会话库；
    /// 建目录 = `std::fs::create_dir` —— **单层，绝不递归**）
    pub fn production(os: &'static str) -> Self {
        let mut turn = TurnDeps::production(os);
        // 新建路径的「按会话号读库」**必然落空**（回合起跑时还没有会话号，锁键不是会话号）
        // ——Task 8 的库确认轮询在这里只会白等 2s，故收成 1 次。新会话的发现轮询由
        // [`run_create`] 按**同一组常量口径**（`STORE_CONFIRM_ATTEMPTS` / 注入的间隔）
        // 另行驱动，不另立第二套节奏。
        turn.store_confirm_attempts = 1;
        Self {
            os,
            turn,
            stored_sessions: Box::new(|| {
                // **Option 原样透传**（复审 Important 2）：库不可读 ⇒ None ⇒ 调用方放弃
                // 发现，**绝不**降级成「空基线」（那会把旧会话误报成本轮新建）
                crate::monitor::zcode_parser::stored_sessions_home(&dirs::home_dir()?).map(|rows| {
                    rows.into_iter()
                        .map(|(id, directory, created_at)| StoredSession {
                            id,
                            directory,
                            created_at,
                        })
                        .collect()
                })
            }),
            // `create_dir`（**非** `create_dir_all`）：父目录缺席时它自己就会失败——
            // 「绝不递归建目录」由 API 选择在结构上保证，不靠调用纪律
            ensure_dir: Box::new(|p: &str| std::fs::create_dir(p).map_err(|e| e.to_string())),
        }
    }
}

/// 新会话确认来源（诊断面；移动端据此说清「怎么确认的」）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewSessionConfirmation {
    /// stdout JSON 帧的 `sessionId`（Task 8 帧口径）
    StdoutFrame(String),
    /// 会话库发现的新会话（**唯一权威判据**）
    Store(String),
    /// 未确认（原因在 `receipt.reason` —— **绝不编造 sess_id**）
    Unconfirmed,
}

/// 新建回合结局
#[derive(Debug, Clone)]
pub struct CreateOutcome {
    /// 回执：**`session_id` 只在确认到新会话时非空**；未确认即空串（绝不把锁键/
    /// 项目路径冒充会话号），状态/阶段/原因如实沿用 Task 8 回执件
    pub receipt: Receipt,
    pub confirmation: NewSessionConfirmation,
    /// 回合尝试次数（争用锁重试计数，Task 8 同口径）
    pub attempts: usize,
    /// 争用锁重试用尽（如实的「工作区忙」终态）
    pub busy_final: bool,
}

/// 新建一个 zcode 会话（**唯一**编排入口）。流程：
/// ① 叶节点缺席则**单层**建目录（失败即如实拒绝，零字节投递）→ ② 记项目在册会话基线
/// （会话库只读；**`None` = 基线不可得 ⇒ 放弃发现、如实未确认**）→ ③ 走 Task 8 的
/// [`zcode::run_turn`]（同并发名额/看门狗/取消/争用锁重试；会话号位传**锁键**——新建时尚无
/// 会话号）→ ④ 确认新会话号（stdout 帧优先，其次会话库发现；发现以「`time_created` ≥ 回合
/// 起点」为主证据）→ ⑤ 组装回执（确认来源 + 会话号 + 库里的 `lastAssistant`/`tokens`）。
///
/// `lock_key` 取 [`zcode::create_lock_key`]（项目级串行：同项目两次新建不许重叠——
/// 两个无头进程同时在该工作区开会话会互相争用）。
pub async fn run_create(
    inv: &ZcodeInvocation,
    project: &str,
    plan: PathPlan,
    lock_key: &str,
    make_runner: &(dyn Fn(&ZcodeInvocation) -> RunnerCfg + Send + Sync),
    deps: &CreateDeps,
    run: &RunSeam,
) -> CreateOutcome {
    // ① 叶节点缺席 → **单层**创建（父目录不在场时 `create_dir` 自身就会失败，如实拒绝：
    //    回合未起跑、零字节投递 ⇒ `refused` 档，不冒充 `spawn` 失败）
    if plan == PathPlan::CreateHere {
        if let Err(e) = (deps.ensure_dir)(project) {
            return CreateOutcome {
                receipt: Receipt::failed(
                    Stage::Refused,
                    &format!(
                        "项目目录创建失败（{project}）：{e}——本功能不递归创建目录，\
                         请先在磁盘上建好上级目录或改选已存在的目录"
                    ),
                ),
                confirmation: NewSessionConfirmation::Unconfirmed,
                attempts: 0,
                busy_final: false,
            };
        }
    }
    // ② 基线（**回合前**读一次库）：本项目在册会话——新会话的判据基线（必须早于起跑）。
    //    `None` = 库不可读 ⇒ **判据不可得**（复审 Important 2）：放弃发现、如实未确认，
    //    绝不把 None 当空基线用。
    let before = (deps.stored_sessions)();
    // 回合起点墙钟（**紧接基线之后、起跑之前**取）：`time_created >= 起点` 是「本轮新建」
    // 的主证据——创建于本回合之前的会话结构上不可能被判成新建（与基线读得成不成功无关）。
    let turn_started_ms = chrono::Utc::now().timestamp_millis();
    // ③ 回合：走 Task 8 的机械（并发名额/看门狗/取消/争用锁重试单源）。会话号位传**锁键**
    //    ——创建时尚无会话号；`resume = None` 使该值不进命令行，只作串行锁与回执兜底键。
    let turn = zcode::run_turn(inv, lock_key, project, make_runner, &deps.turn, run).await;
    // ④ 新会话号确认（**stdout 帧优先，其次会话库发现；都没有 = 如实未确认**）
    let frame_id = (turn.receipt_source == zcode::ReceiptSource::StdoutJson)
        .then(|| turn.receipt.session_id.clone())
        .filter(|id| !id.is_empty() && id != lock_key);
    if let Some(id) = frame_id {
        if is_valid_session_id(&id) {
            let receipt = fill_missing_from_store(turn.receipt, &id, deps);
            return CreateOutcome {
                receipt,
                confirmation: NewSessionConfirmation::StdoutFrame(id),
                attempts: turn.attempts,
                busy_final: turn.busy_final,
            };
        }
        // 帧给了「会话号」但不合规（上板只认 `sess_` + UUID）：**不当成确认**，
        // 如实说明形态不合规（不把可疑串冒充 sess_id）
        return CreateOutcome {
            receipt: unconfirmed_receipt(
                turn.receipt,
                &format!("stdout 帧给出的会话号形态不合规（{id}）——未确认新会话"),
            ),
            confirmation: NewSessionConfirmation::Unconfirmed,
            attempts: turn.attempts,
            busy_final: turn.busy_final,
        };
    }
    // 基线不可得 ⇒ 判据不可得（复审 Important 2）：**跳过发现**、如实未确认（不猜）
    let Some(before) = before else {
        return CreateOutcome {
            receipt: unconfirmed_receipt(turn.receipt, BASELINE_UNAVAILABLE_NOTE),
            confirmation: NewSessionConfirmation::Unconfirmed,
            attempts: turn.attempts,
            busy_final: turn.busy_final,
        };
    };
    // 会话库发现（**有界**轮询：懒落库/WAL 可见性可能滞后；上限复用 Task 8 常量口径）。
    // 轮询**只在「什么都没发现」时继续**：一旦看见新会话（1 个 = 确认；≥2 个 = 归属
    // 不可判定）就停下——再等只会让歧义更歧义，不会变得可判定。
    let mut verdict = NewSessionVerdict::None;
    for attempt in 0..zcode::STORE_CONFIRM_ATTEMPTS.max(1) {
        if attempt > 0 {
            (deps.turn.wait)(deps.turn.store_confirm_interval).await;
        }
        let Some(now) = (deps.stored_sessions)() else {
            // 轮询期间库变得不可读 ⇒ 判据不再成立（不把「读不到」当「没有新会话」）
            verdict = NewSessionVerdict::None;
            break;
        };
        verdict = new_session_of(project, &before, &now, deps.os, turn_started_ms);
        if !matches!(verdict, NewSessionVerdict::None) {
            break;
        }
    }
    match verdict {
        NewSessionVerdict::Single(id) => {
            let snapshot = (deps.turn.store_probe)(&id);
            let receipt = confirmed_receipt(turn.receipt, &id, snapshot);
            CreateOutcome {
                receipt,
                confirmation: NewSessionConfirmation::Store(id),
                attempts: turn.attempts,
                busy_final: turn.busy_final,
            }
        }
        other => CreateOutcome {
            receipt: unconfirmed_receipt(turn.receipt, &discovery_note(&other)),
            confirmation: NewSessionConfirmation::Unconfirmed,
            attempts: turn.attempts,
            busy_final: turn.busy_final,
        },
    }
}

/// 库确认成功时的回执组装（**只有「回合本身正常结束」才升为 Ok**）：
/// - 回合 Ok / 或「exit 0 但按会话号读库读不到」（`channel_error` + `Unconfirmed`）⇒
///   用库里的 `lastAssistant`/`tokens` 组装 Ok（Task 8 的截断口径单点
///   [`super::turn::ok_receipt_with_assistant`]）；
/// - 回合**真的失败**（超时/崩溃/取消/工作区忙/投递前拒绝）⇒ **保留失败终态**，只把
///   已确认的新会话号填上，并在原因里如实注明「会话已建、但回合失败」——绝不因为
///   库里有新会话就把失败抹成成功（首句可能没送达）。
///
/// `durationMs` 沿用回合墙钟（Task 8 口径）；确认轮询的额外耗时**不计入**（那不属于回合）。
fn confirmed_receipt(
    turn_receipt: Receipt,
    id: &str,
    snapshot: Option<zcode::StoreSnapshot>,
) -> Receipt {
    let text = snapshot
        .as_ref()
        .and_then(|s| s.last_assistant.clone())
        .or_else(|| turn_receipt.last_assistant.clone());
    let tokens = snapshot
        .as_ref()
        .and_then(|s| s.tokens)
        .or(turn_receipt.tokens);
    // 「回合本身正常结束」= 回执 Ok，或 **exit 0 但按会话号没能确认**（channel_error ——
    // 那正是本模块要接手确认的形态）
    let normal_exit = turn_receipt.status == ReceiptStatus::Ok
        || (turn_receipt.status == ReceiptStatus::Failed
            && turn_receipt.stage == Some(Stage::ChannelError));
    if normal_exit {
        let mut r = match text {
            Some(t) => {
                super::turn::ok_receipt_with_assistant(id, &t, tokens, turn_receipt.duration_ms)
            }
            // 会话已确认（库里确有新会话）但尚无可见的回复内容 —— 如实给 Ok 但不编摘要
            None => {
                let mut r = Receipt::ok(id, turn_receipt.duration_ms);
                r.tokens = tokens;
                r
            }
        };
        r.reason = None;
        return r;
    }
    // 失败终态保留：填已确认的会话号 + 如实注明
    let base = turn_receipt
        .reason
        .clone()
        .unwrap_or_else(|| "回合失败".to_string());
    let mut r = turn_receipt;
    r.session_id = id.to_string();
    r.last_assistant = r
        .last_assistant
        .or_else(|| text.map(|t| super::turn::assistant_summary(&t)));
    r.tokens = tokens;
    r.reason = Some(format!(
        "{base}｜会话库已确认新会话 {id}（项目里已建出该会话，但本轮未正常结束——首句是否送达请在会话内容中核实）"
    ));
    r
}

/// 帧已点名会话号时**补齐**库里的 `lastAssistant`/`tokens`（帧没有才补——帧有一等优先）。
fn fill_missing_from_store(mut r: Receipt, id: &str, deps: &CreateDeps) -> Receipt {
    if r.last_assistant.is_none() {
        if let Some(s) = (deps.turn.store_probe)(id) {
            r.last_assistant = s
                .last_assistant
                .as_deref()
                .map(super::turn::assistant_summary);
            if r.tokens.is_none() {
                r.tokens = s.tokens;
            }
        }
    }
    r.session_id = id.to_string();
    r
}

/// 未确认臂：**会话号一律清空**——**无论它看着多合法**。
///
/// 为什么无条件（Task 12 复审 Important 1）：未确认的来路不止「锁键兜底」一种，最尖的一种是
/// **CLI 先打了含 `sessionId` 的 JSON 帧、随后崩溃/被信号杀死**——`runner::finish_exit` 会把
/// 帧里的会话号放进 Crash 回执（`ReceiptSource::NotApplicable`），而本模块的帧路径只认
/// `StdoutJson`，于是走到未确认臂。此时若按「合法就保留」的旧判据，就会以
/// `confirmation:"none"` 发出**非空 `sessionId`**：线契约自相矛盾，审计行的会话号列也不再是
/// 「未确认」的意思。契约必须是「Unconfirmed ⇒ `sessionId` 为空」——**在源头（回执）与
/// 出口（[`crate::remote::api::create_envelope`]）两层都成立**。
fn unconfirmed_receipt(mut r: Receipt, note: &str) -> Receipt {
    r.session_id = String::new();
    let base = r
        .reason
        .clone()
        .unwrap_or_else(|| "新建回合未确认新会话".to_string());
    r.reason = Some(format!("{base}｜{note}"));
    r
}

/// 基线不可得时的如实文案（复审 Important 2：判据缺一半 ⇒ 放弃发现，不猜）
const BASELINE_UNAVAILABLE_NOTE: &str =
    "会话库基线不可读（判定「是否新建」的前置证据缺失）——未确认新会话\
     （未确认即不报会话号；请在上板会话列表中核实，勿盲目重发：重发会再建一个会话）";

/// 发现结论 → 如实文案（未确认也要说清为什么）
fn discovery_note(v: &NewSessionVerdict) -> String {
    match v {
        NewSessionVerdict::None => "会话库中该项目未出现新会话——未确认新会话（未确认即不报会话号；\
             请在上板会话列表中核实，勿盲目重发：重发会再建一个会话）"
            .to_string(),
        NewSessionVerdict::Ambiguous(n) => format!(
            "会话库中该项目出现了 {n} 个新会话——归属不可判定，未确认新会话\
             （请人工核实，勿盲目重发：重发会再建一个会话）"
        ),
        NewSessionVerdict::Single(id) => format!("会话库已确认新会话 {id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inject::headless::zcode::{StoreProbe, StoreSnapshot, ZcodeSpec};
    use crate::session::{AgentType, ProcessForm};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    // ===== 夹具 =====

    fn sess(id: &str, tool: AgentType, status: SessionStatus, path: &str) -> Session {
        Session {
            id: id.into(),
            agent_type: tool,
            project_name: "proj".into(),
            project_path: path.into(),
            title: None,
            git_branch: None,
            github_url: None,
            status,
            last_message: None,
            last_message_role: None,
            last_activity_at: "2026-10-05T00:00:00Z".into(),
            pid: 0,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::App,
            jump_supported: true,
            unread: false,
        }
    }

    fn stored(id: &str, dir: &str) -> StoredSession {
        stored_at(id, dir, None)
    }

    fn stored_at(id: &str, dir: &str, created_at: Option<i64>) -> StoredSession {
        StoredSession {
            id: id.into(),
            directory: dir.into(),
            created_at,
        }
    }

    /// 「本轮新建」的时间证据：略晚于当前时刻（`run_create` 在起跑前才取回合起点，
    /// 故夹具必须晚于测试体构造夹具的那一刻 ⇒ 取未来 1 分钟，确定性且无需时钟缝）
    fn created_now() -> Option<i64> {
        Some(chrono::Utc::now().timestamp_millis() + 60_000)
    }

    /// 「回合前就在册」的时间证据（早于回合起点 ⇒ 结构上不可能是本轮新建）
    fn created_before() -> Option<i64> {
        Some(chrono::Utc::now().timestamp_millis() - 60_000)
    }

    const UUID_A: &str = "sess_11111111-1111-1111-1111-111111111111";
    const UUID_B: &str = "sess_22222222-2222-2222-2222-222222222222";
    const UUID_C: &str = "sess_33333333-3333-3333-3333-333333333333";
    /// 判定用的「回合起点」：夹具里凡 created_at ≥ 它 = 本轮建的行
    const TURN_START: i64 = 1_000_000;

    fn snap(seq: i64, id: &str, text: &str, tokens: u64) -> StoreSnapshot {
        StoreSnapshot {
            last_seq: seq,
            last_assistant_id: Some(id.into()),
            last_assistant: Some(text.into()),
            tokens: Some(tokens),
        }
    }

    // ===== 候选列表 =====

    /// 候选列表 = 信任表（主源，保序）∪ 看板快照项目：存在性过滤 + 工作区口径去重 +
    /// 信任档如实标注；macOS 不做大小写折叠（与 `same_workspace` 同口径）
    #[test]
    fn candidates_union_prefers_trusted_and_filters_missing() {
        let trusted = vec!["E:\\proj_a".to_string(), "E:\\ghost".to_string()];
        let board = vec![
            "E:/proj_a".to_string(),
            "E:/proj_b".to_string(),
            "E:/ghost2".to_string(),
        ];
        let exists = |p: &str| !p.contains("ghost");
        let out = candidates(&trusted, &board, &exists, "windows");
        assert_eq!(
            out,
            vec![
                ProjectCandidate {
                    path: "E:\\proj_a".into(),
                    source: CandidateSource::Trusted,
                    trusted: true,
                },
                ProjectCandidate {
                    path: "E:/proj_b".into(),
                    source: CandidateSource::Board,
                    trusted: false,
                },
            ],
            "信任表主源在前、去重、缺席过滤、信任档标注"
        );
        // 大小写口径随平台：macOS 不折叠 → 两条各留（不是同一个目录）
        let mac = candidates(
            &["/tmp/P".to_string()],
            &["/tmp/p".to_string()],
            &|_| true,
            "macos",
        );
        assert_eq!(mac.len(), 2, "macOS 大小写敏感（与 same_workspace 同口径）");
        // 空表 → 空候选
        assert!(candidates(&[], &[], &|_| true, "windows").is_empty());
    }

    // ===== 手填路径校验 =====

    /// 手填校验判定表（逐格点因）：合法在册 / 缺席单层创建 / 盘符不在场 / 敏感命中 /
    /// 需递归创建 / 父路径被文件占位 / 非绝对 / 空
    #[test]
    fn manual_path_validation_table() {
        let exists = |p: &str| {
            matches!(
                p,
                "E:/" | "C:/" | "E:/proj" | "E:/proj/sub" | "E:/proj/file.txt"
            )
        };
        let is_dir = |p: &str| matches!(p, "E:/" | "C:/" | "E:/proj" | "E:/proj/sub");
        let sensitive = |p: &str| p.contains("/.ssh");
        let probe = FsProbe {
            exists: Box::new(exists),
            is_dir: Box::new(is_dir),
            sensitive: Box::new(sensitive),
        };
        let check = |p: &str| validate_manual_path(p, true, &probe);

        // 合法：在册目录 / 子目录 / 尾分隔符归一
        assert_eq!(check("E:/proj"), Ok(PathPlan::Existing));
        assert_eq!(check("E:\\proj\\"), Ok(PathPlan::Existing), "尾分隔符归一");
        assert_eq!(check("E:/proj/sub"), Ok(PathPlan::Existing));
        // 缺席但父目录在场 → 单层创建（**不递归**）
        assert_eq!(check("E:/proj/new"), Ok(PathPlan::CreateHere));
        // 缺席且父目录也不在场 → 需递归创建 ⇒ 拒绝并点名
        assert_eq!(
            check("E:/proj/new/deep"),
            Err(ManualPathReject::NeedsRecursiveCreation)
        );
        assert_eq!(
            check("E:/proj/new/deep/x"),
            Err(ManualPathReject::NeedsRecursiveCreation)
        );
        // 父路径被文件占位
        assert_eq!(
            check("E:/proj/file.txt/x"),
            Err(ManualPathReject::ParentNotDirectory)
        );
        // 已存在但不是目录
        assert_eq!(
            check("E:/proj/file.txt"),
            Err(ManualPathReject::NotDirectory)
        );
        // 盘符不在场（Z: 未挂载）
        assert_eq!(check("Z:/proj"), Err(ManualPathReject::DriveMissing));
        // 敏感黑名单（判据注入；生产指向 files 单源）：**即使不存在也先拦**
        assert_eq!(check("E:/.ssh/proj"), Err(ManualPathReject::Sensitive));
        // 非绝对（相对路径 / 盘符相对路径）
        assert_eq!(check("proj"), Err(ManualPathReject::NotAbsolute));
        assert_eq!(check("C:proj"), Err(ManualPathReject::NotAbsolute));
        // 空
        assert_eq!(check(""), Err(ManualPathReject::Empty));
        assert_eq!(check("   "), Err(ManualPathReject::Empty));

        // 每条拒绝原因都必须点名（拒绝臂不许含糊）
        for (rej, needle) in [
            (ManualPathReject::Empty, "不能为空"),
            (ManualPathReject::NotAbsolute, "绝对路径"),
            (ManualPathReject::DriveMissing, "盘符"),
            (ManualPathReject::Sensitive, "敏感目录黑名单"),
            (ManualPathReject::NotDirectory, "不是目录"),
            (ManualPathReject::NeedsRecursiveCreation, "不递归创建"),
            (ManualPathReject::ParentNotDirectory, "上级不是目录"),
        ] {
            let r = rej.reason();
            assert!(r.contains(needle), "{rej:?} 原因必须点名 {needle}: {r}");
        }

        // POSIX 分支（同一纯核，平台语义注入）
        let posix = FsProbe {
            exists: Box::new(|p: &str| p == "/" || p == "/tmp/proj"),
            is_dir: Box::new(|p: &str| p == "/" || p == "/tmp/proj"),
            sensitive: Box::new(|_| false),
        };
        assert_eq!(
            validate_manual_path("/tmp/proj", false, &posix),
            Ok(PathPlan::Existing)
        );
        assert_eq!(
            validate_manual_path("relative/x", false, &posix),
            Err(ManualPathReject::NotAbsolute)
        );
        assert_eq!(
            validate_manual_path("/tmp/proj/new", false, &posix),
            Ok(PathPlan::CreateHere)
        );
    }

    /// 黑名单**同源**：生产观测缝直接用 `remote::files` 的文件预览口径（同一份
    /// SENSITIVE_DIRS + 豁免表 + fail-closed 分支）——tempdir 主目录夹具，零真实主目录接触
    #[test]
    fn manual_path_blacklist_is_the_file_preview_single_source() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(home.join(".claude").join("plans")).unwrap();
        let windows = cfg!(windows);
        let probe = production_probe(Some(home), windows);

        let ssh_proj = home.join(".ssh").join("proj");
        assert_eq!(
            validate_manual_path(&ssh_proj.to_string_lossy(), windows, &probe),
            Err(ManualPathReject::Sensitive),
            "凭据目录（与文件预览同一份黑名单）"
        );
        // 豁免产物子树（.claude/plans）与文件预览同口径放行
        let plans_proj = home.join(".claude").join("plans").join("proj");
        assert_eq!(
            validate_manual_path(&plans_proj.to_string_lossy(), windows, &probe),
            Ok(PathPlan::CreateHere),
            "豁免产物子树（.claude/plans）：与文件预览同口径"
        );
        // 主目录基准不可用 → fail-closed 全段匹配（凭据目录照拦）
        let no_home = production_probe(None, windows);
        assert_eq!(
            validate_manual_path(&ssh_proj.to_string_lossy(), windows, &no_home),
            Err(ManualPathReject::Sensitive),
            "基准缺失不得让黑名单整段失效（fail-closed，同 read_file_safe）"
        );
    }

    /// **复审 Minor 1**：指向敏感目录的**链接**必须照拦——字面路径看着人畜无害，
    /// 但真实落点在黑名单里（与文件预览同结论：预览 canonicalize 后拒，本项目路径
    /// 选择也必须拒）。链接形态平台分叉：Windows 用 junction（`mklink /J`，无需管理员），
    /// Unix 用 symlink；环境不支持链接时**如实跳过**（并在输出里说明），不假装通过。
    #[test]
    fn manual_path_blacklist_resolves_links_into_sensitive_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let ssh = home.join(".ssh");
        std::fs::create_dir_all(ssh.join("proj")).unwrap();
        let windows = cfg!(windows);
        let probe = production_probe(Some(home), windows);

        // ① 链接本身在非敏感处、指向敏感目录 ⇒ 拒绝（解链接后落点命中黑名单）
        let link = home.join("innocent_link");
        if !try_link_dir(&ssh, &link) {
            eprintln!("跳过：本环境无法创建目录链接（{link:?}）——链接绕过面未在本机验证");
            return;
        }
        assert_eq!(
            validate_manual_path(&link.to_string_lossy(), windows, &probe),
            Err(ManualPathReject::Sensitive),
            "指向敏感目录的链接必须照拦（与文件预览同结论）"
        );
        // ② 叶节点缺席、**父级**是上述链接 ⇒ 同样拒绝（父级解链接）
        let under_link = link.join("new_project");
        assert_eq!(
            validate_manual_path(&under_link.to_string_lossy(), windows, &probe),
            Err(ManualPathReject::Sensitive),
            "父级是链接时也必须解链接后再判（叶节点尚不存在）"
        );
        // ③ 反向对照：链接指向**普通**目录 ⇒ 放行（解链接不误伤）
        let plain = home.join("plain_dir");
        std::fs::create_dir_all(&plain).unwrap();
        let ok_link = home.join("ok_link");
        if try_link_dir(&plain, &ok_link) {
            assert_eq!(
                validate_manual_path(&ok_link.to_string_lossy(), windows, &probe),
                Ok(PathPlan::Existing),
                "指向普通目录的链接照常可用（不误伤）"
            );
        }
    }

    /// 建目录链接（Windows = junction / Unix = symlink）；不支持则返回 false（用例跳过）
    #[cfg(windows)]
    fn try_link_dir(target: &Path, link: &Path) -> bool {
        std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[cfg(not(windows))]
    fn try_link_dir(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    // ===== 新会话确认 =====

    /// 新会话确认纯核：只认**合规**且落在该项目下的**新** id；老会话被更新不算新建；
    /// 多个新会话 = 归属不可判定（如实 Ambiguous，不挑一个冒充）；**创建于回合之前的
    /// 会话即使不在基线内也不算新建**（`time_created` 主证据 —— 复审 Important 2）
    #[test]
    fn new_session_discovery_is_single_and_never_guessed() {
        let old = stored(UUID_A, "E:/proj");
        let fresh = stored_at(UUID_B, "E:/proj", Some(TURN_START + 5));
        let before = vec![old.clone()];
        assert_eq!(
            new_session_of(
                "E:/proj",
                &before,
                &[old.clone(), fresh.clone()],
                "windows",
                TURN_START
            ),
            NewSessionVerdict::Single(UUID_B.into())
        );
        assert_eq!(
            new_session_of(
                "E:/proj",
                &before,
                std::slice::from_ref(&old),
                "windows",
                TURN_START
            ),
            NewSessionVerdict::None,
            "老会话被更新不算新建"
        );
        let f2 = stored_at(UUID_C, "E:/proj", Some(TURN_START + 6));
        assert_eq!(
            new_session_of(
                "E:/proj",
                &before,
                &[old.clone(), fresh.clone(), f2],
                "windows",
                TURN_START
            ),
            NewSessionVerdict::Ambiguous(2),
            "多个新会话 → 不猜"
        );
        // 不合规 id（子代理会话/脏数据）不算新会话
        let junk = stored(
            "sess_subagent_agent_44444444-4444-4444-4444-444444444444",
            "E:/proj",
        );
        assert_eq!(
            new_session_of(
                "E:/proj",
                &before,
                &[old.clone(), junk],
                "windows",
                TURN_START
            ),
            NewSessionVerdict::None
        );
        // 别家目录的会话不算
        let other = stored("sess_55555555-5555-5555-5555-555555555555", "E:/other");
        assert_eq!(
            new_session_of(
                "E:/proj",
                &before,
                &[old.clone(), other],
                "windows",
                TURN_START
            ),
            NewSessionVerdict::None
        );
        // 空基线（该项目此前无会话）→ 首个**本轮建**的在册会话即新
        assert_eq!(
            new_session_of(
                "E:/proj",
                &[],
                std::slice::from_ref(&fresh),
                "windows",
                TURN_START
            ),
            NewSessionVerdict::Single(UUID_B.into())
        );
        // 子目录同属该工作区（同 same_workspace 口径）
        let sub = stored_at(UUID_C, "E:/proj/sub", Some(TURN_START + 7));
        assert_eq!(
            new_session_of(
                "E:/proj",
                &before,
                &[old.clone(), sub],
                "windows",
                TURN_START
            ),
            NewSessionVerdict::Single(UUID_C.into())
        );
        // **结构防线**（复审 Important 2）：空基线 + 只有一个**回合前建的**旧会话 ⇒
        // 判 None（旧会话绝不因「不在基线内」被说成新建——基线读失败也骗不出假确认）
        let pre = stored_at(UUID_C, "E:/proj", Some(TURN_START - 1));
        assert_eq!(
            new_session_of(
                "E:/proj",
                &[],
                std::slice::from_ref(&pre),
                "windows",
                TURN_START
            ),
            NewSessionVerdict::None,
            "time_created < 回合起点 ⇒ 不是本轮新建（哪怕它不在基线里）"
        );
        // 无时间证据（列缺失/老库）→ 该道放行，仍由「不在基线内」承担
        let no_time = stored(UUID_C, "E:/proj");
        assert_eq!(
            new_session_of(
                "E:/proj",
                &[],
                std::slice::from_ref(&no_time),
                "windows",
                TURN_START
            ),
            NewSessionVerdict::Single(UUID_C.into()),
            "无时间证据 ⇒ 退回 id 基线判据（不废掉发现链）"
        );
        // 时刻相等算本轮（≥，与 CLI 落库毫秒粒度对齐）
        let at_start = stored_at(UUID_C, "E:/proj", Some(TURN_START));
        assert_eq!(
            new_session_of(
                "E:/proj",
                &[],
                std::slice::from_ref(&at_start),
                "windows",
                TURN_START
            ),
            NewSessionVerdict::Single(UUID_C.into())
        );
    }

    // ===== 提示面 =====

    /// 可见性分层：信任 → 重启可见；未信任/读不到信任表 → 仅 MAM 可见（保守，不谎报）
    #[test]
    fn visibility_hint_layers_by_trust() {
        let dir = tempfile::tempdir().unwrap();
        let v2 = dir.path().join(".zcode").join("v2");
        std::fs::create_dir_all(&v2).unwrap();
        std::fs::write(
            v2.join("setting.json"),
            "{\"recentProjects\":[\"E:\\\\proj\"]}",
        )
        .unwrap();
        let home = dir.path().to_path_buf();

        let trusted = create_hints(
            "E:/proj",
            zcode::visibility_of("E:/proj", Some(&home), "windows"),
            &[],
            "windows",
        );
        assert_eq!(trusted.visibility, Visibility::AfterRestart);
        assert_eq!(
            trusted.visibility_note,
            "已信任工作区：重启 ZCode 应用后可见"
        );

        let untrusted = create_hints(
            "E:/other",
            zcode::visibility_of("E:/other", Some(&home), "windows"),
            &[],
            "windows",
        );
        assert_eq!(untrusted.visibility, Visibility::MamOnly);
        assert_eq!(untrusted.visibility_note, "未信任工作区：仅 MAM 可见");

        let no_home = create_hints(
            "E:/proj",
            zcode::visibility_of("E:/proj", None, "windows"),
            &[],
            "windows",
        );
        assert_eq!(
            no_home.visibility_note, "未信任工作区：仅 MAM 可见",
            "读不到信任表 = 保守判未信任（同 H7）"
        );
    }

    /// 黄字信号：同项目**在册** zcode 会话才计数（子目录同工作区；已结束不算；别家工具
    /// 不算；分隔符边界严判）；信号只提示不拦截，别的项目不误报
    #[test]
    fn yellow_signal_counts_same_project_zcode_sessions_only() {
        let board = vec![
            sess(
                "sess_p1",
                AgentType::ZCode,
                SessionStatus::Processing,
                "E:/proj",
            ),
            sess(
                "sess_p2",
                AgentType::ZCode,
                SessionStatus::Idle,
                "E:/proj/sub",
            ),
            sess(
                "sess_p3",
                AgentType::ZCode,
                SessionStatus::Finished,
                "E:/proj",
            ),
            sess(
                "sess_p4",
                AgentType::Claude,
                SessionStatus::Waiting,
                "E:/proj",
            ),
            sess(
                "sess_p5",
                AgentType::ZCode,
                SessionStatus::Waiting,
                "E:/proj2",
            ),
        ];
        assert_eq!(active_zcode_in_project("E:/proj", &board, "windows"), 2);
        assert_eq!(active_zcode_in_project("E:/proj2", &board, "windows"), 1);
        assert_eq!(active_zcode_in_project("E:/nope", &board, "windows"), 0);

        let hinted = create_hints("E:/proj", Visibility::MamOnly, &board, "windows");
        let warn = hinted.warning.unwrap_or_default();
        assert!(
            warn.contains('2') && warn.contains("不拦截"),
            "黄字信号必须给出数量并申明不拦截：{warn}"
        );
        assert!(
            create_hints("E:/nope", Visibility::MamOnly, &board, "windows")
                .warning
                .is_none(),
            "干净项目不得凭空报警"
        );
    }

    // ===== 编排（脚本缝，零真实进程/零真实库）=====

    /// 库读缝脚本：按序吐出快照序列（空 → None = 读不到），并记调用次数
    fn store_queue(items: Vec<Option<StoreSnapshot>>) -> (Box<StoreProbe>, Arc<Mutex<usize>>) {
        let q = Arc::new(Mutex::new(items));
        let calls = Arc::new(Mutex::new(0usize));
        let calls2 = calls.clone();
        let probe: Box<StoreProbe> = Box::new(move |_sid: &str| {
            *calls2.lock().unwrap_or_else(|e| e.into_inner()) += 1;
            let mut q = q.lock().unwrap_or_else(|e| e.into_inner());
            if q.is_empty() {
                None
            } else {
                q.remove(0)
            }
        });
        (probe, calls)
    }

    /// 只在**点名 id** 时命中（其余 = 读不到）：Task 8 的锁键探针在新建路径上必然落空，
    /// 本桩把「新建会话的快照」与「按会话号的快照」分开，避免测试互相串味
    fn keyed_store(map: Vec<(&'static str, StoreSnapshot)>) -> Box<StoreProbe> {
        Box::new(move |sid: &str| {
            map.iter()
                .find(|(id, _)| *id == sid)
                .map(|(_, s)| s.clone())
        })
    }

    /// 库内在册会话脚本（按序吐出；脚本耗尽 → `None` = **库不可读**——与「空表 = 该项目
    /// 没有会话」严格区分：复审 Important 2 的判据正是靠这个区分成立）
    fn stored_seq(
        items: Vec<Option<Vec<StoredSession>>>,
    ) -> (Box<StoredSessionsFn>, Arc<Mutex<usize>>) {
        let q = Arc::new(Mutex::new(items));
        let calls = Arc::new(Mutex::new(0usize));
        let calls2 = calls.clone();
        (
            Box::new(move || {
                *calls2.lock().unwrap_or_else(|e| e.into_inner()) += 1;
                let mut q = q.lock().unwrap_or_else(|e| e.into_inner());
                if q.is_empty() {
                    None
                } else {
                    q.remove(0)
                }
            }),
            calls,
        )
    }

    fn test_deps(
        stored: Box<StoredSessionsFn>,
        store_probe: Box<StoreProbe>,
        ensure: Box<EnsureDirFn>,
        active: bool,
        waits: Arc<Mutex<Vec<u64>>>,
        attempts: usize,
    ) -> CreateDeps {
        CreateDeps {
            os: "windows",
            turn: TurnDeps {
                max_busy_retries: zcode::BUSY_MAX_RETRIES,
                retry_spacing: Duration::from_millis(zcode::BUSY_RETRY_SPACING_MS),
                workspace_active: Box::new(move |_| active),
                wait: Box::new(move |d: Duration| {
                    let waits = waits.clone();
                    Box::pin(async move {
                        waits
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(d.as_millis() as u64);
                    })
                }),
                store_probe,
                store_confirm_attempts: attempts,
                store_confirm_interval: Duration::from_millis(zcode::STORE_CONFIRM_INTERVAL_MS),
            },
            stored_sessions: stored,
            ensure_dir: ensure,
        }
    }

    /// 脚本执行缝：退出 0 + 指定 stdout（第 1 次起即成功；busy 变体由 `busy_times` 给）
    fn scripted_seam(busy_times: usize, stdout: Vec<String>) -> Box<RunSeam> {
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        Box::new(move |cfg: RunnerCfg| {
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut cfg = cfg;
            if n < busy_times {
                cfg = cfg.stdout_lines(vec![format!("Error: {}", zcode::WORKSPACE_BUSY_MARKER)]);
            } else {
                cfg = cfg.stdout_lines(stdout.clone());
            }
            Box::pin(async move {
                let receipt = cfg.run_once(|_| {});
                super::super::turn::TurnObs {
                    receipt,
                    stdout: cfg.captured_stdout(),
                    stderr: cfg.captured_stderr(),
                    exit: cfg.last_exit_code(),
                }
            })
        })
    }

    fn make_runner(inv: &ZcodeInvocation) -> RunnerCfg {
        RunnerCfg::for_test()
            .session_id("create|e:/proj")
            .args(inv.argv.clone())
    }

    /// 真机形态：stdout 无 JSON（`--resume` 回合实证形态，新建回合按同口径兜底）→
    /// 新会话号**只认会话库**：创建前不在册的新会话出现 + 其库内 assistant 回复 ⇒ 确认
    #[tokio::test]
    async fn run_create_confirms_the_new_session_from_the_store() {
        let inv = zcode::build_create_argv(&ZcodeSpec::win("D:/ZCode"), "hi", "E:/proj", None);
        let old = stored(UUID_A, "E:/proj");
        let fresh = stored_at(UUID_B, "E:/proj", created_now());
        let (stored_fn, calls) = stored_seq(vec![
            Some(vec![old.clone()]),
            Some(vec![old.clone(), fresh.clone()]),
        ]);
        let (probe, _) = store_queue(vec![]);
        let waits = Arc::new(Mutex::new(Vec::new()));
        let deps = test_deps(
            stored_fn,
            keyed_store(vec![(UUID_B, snap(9, "m2", "首句已答", 12))]),
            Box::new(|_| Ok(())),
            false,
            waits.clone(),
            1,
        );
        let seam = scripted_seam(0, vec!["ZCode Built-in skipped (not-due)".to_string()]);
        let out = run_create(
            &inv,
            "E:/proj",
            PathPlan::Existing,
            &zcode::create_lock_key("E:/proj", "windows"),
            &make_runner,
            &deps,
            &*seam,
        )
        .await;
        assert_eq!(
            out.confirmation,
            NewSessionConfirmation::Store(UUID_B.into()),
            "{:?}",
            out.receipt
        );
        assert_eq!(out.receipt.status, ReceiptStatus::Ok);
        assert_eq!(
            out.receipt.session_id, UUID_B,
            "回执会话号 = 发现到的新会话"
        );
        assert_eq!(out.receipt.last_assistant.as_deref(), Some("首句已答"));
        assert_eq!(out.receipt.tokens, Some(12), "tokens 取自库（不编数字）");
        assert_eq!(out.attempts, 1);
        assert_eq!(*calls.lock().unwrap(), 2, "基线 1 次 + 回合后 1 次");
        drop(probe);
    }

    /// 库写入滞后：前两次还没看到新会话、第三次看到 → **有界轮询**等到它（不无限等）
    #[tokio::test]
    async fn run_create_polls_the_store_until_the_new_session_lands() {
        let inv = zcode::build_create_argv(&ZcodeSpec::win("D:/ZCode"), "hi", "E:/proj", None);
        let old = stored(UUID_A, "E:/proj");
        let fresh = stored_at(UUID_B, "E:/proj", created_now());
        let (stored_fn, calls) = stored_seq(vec![
            Some(vec![old.clone()]),
            Some(vec![old.clone()]),
            Some(vec![old.clone()]),
            Some(vec![old.clone(), fresh.clone()]),
        ]);
        let waits = Arc::new(Mutex::new(Vec::new()));
        let deps = test_deps(
            stored_fn,
            keyed_store(vec![(UUID_B, snap(9, "m2", "迟到但到了", 3))]),
            Box::new(|_| Ok(())),
            false,
            waits.clone(),
            3,
        );
        let seam = scripted_seam(0, vec!["ZCode Built-in skipped (not-due)".to_string()]);
        let out = run_create(
            &inv,
            "E:/proj",
            PathPlan::Existing,
            &zcode::create_lock_key("E:/proj", "windows"),
            &make_runner,
            &deps,
            &*seam,
        )
        .await;
        assert_eq!(
            out.confirmation,
            NewSessionConfirmation::Store(UUID_B.into())
        );
        assert_eq!(out.receipt.last_assistant.as_deref(), Some("迟到但到了"));
        assert_eq!(*calls.lock().unwrap(), 4, "基线 1 + 轮询 3 次（有界）");
    }

    /// stdout JSON 帧（`sessionId`）是**一等来源**：命中即确认，且不再做库发现轮询
    #[tokio::test]
    async fn run_create_prefers_the_stdout_frame_session_id() {
        let inv = zcode::build_create_argv(&ZcodeSpec::win("D:/ZCode"), "hi", "E:/proj", None);
        let (stored_fn, calls) = stored_seq(vec![Some(vec![])]);
        let waits = Arc::new(Mutex::new(Vec::new()));
        let deps = test_deps(
            stored_fn,
            keyed_store(vec![]),
            Box::new(|_| Ok(())),
            false,
            waits.clone(),
            3,
        );
        let seam = scripted_seam(
            0,
            vec![
                "ZCode Built-in skipped (not-due)".to_string(),
                format!("{{\"sessionId\":\"{UUID_B}\",\"response\":\"pong\",\"tokens\":7}}"),
            ],
        );
        let out = run_create(
            &inv,
            "E:/proj",
            PathPlan::Existing,
            &zcode::create_lock_key("E:/proj", "windows"),
            &make_runner,
            &deps,
            &*seam,
        )
        .await;
        assert_eq!(
            out.confirmation,
            NewSessionConfirmation::StdoutFrame(UUID_B.into())
        );
        assert_eq!(out.receipt.status, ReceiptStatus::Ok, "{:?}", out.receipt);
        assert_eq!(out.receipt.session_id, UUID_B);
        assert_eq!(out.receipt.last_assistant.as_deref(), Some("pong"));
        assert_eq!(*calls.lock().unwrap(), 1, "帧已点名会话号 → 不做库发现轮询");
    }

    /// **诚实红线**：确认不到新会话 → 如实未确认；**绝不把锁键/项目路径冒充 sess_id**，
    /// 也绝不把「老会话被更新」说成新建
    #[tokio::test]
    async fn run_create_never_fabricates_a_session_id() {
        let inv = zcode::build_create_argv(&ZcodeSpec::win("D:/ZCode"), "hi", "E:/proj", None);
        let old = stored(UUID_A, "E:/proj");
        let (stored_fn, _) = stored_seq(vec![Some(vec![old.clone()]), Some(vec![old.clone()])]);
        let waits = Arc::new(Mutex::new(Vec::new()));
        let deps = test_deps(
            stored_fn,
            keyed_store(vec![]),
            Box::new(|_| Ok(())),
            false,
            waits,
            1,
        );
        let seam = scripted_seam(0, vec!["ZCode Built-in skipped (not-due)".to_string()]);
        let lock_key = zcode::create_lock_key("E:/proj", "windows");
        let out = run_create(
            &inv,
            "E:/proj",
            PathPlan::Existing,
            &lock_key,
            &make_runner,
            &deps,
            &*seam,
        )
        .await;
        assert_eq!(out.confirmation, NewSessionConfirmation::Unconfirmed);
        assert_eq!(out.receipt.status, ReceiptStatus::Failed);
        assert_eq!(out.receipt.stage, Some(Stage::ChannelError));
        assert_eq!(
            out.receipt.session_id, "",
            "未确认 ⇒ 会话号必须为空串（锁键/项目路径不得冒充 sess_id）"
        );
        let reason = out.receipt.reason.clone().unwrap_or_default();
        assert!(
            reason.contains("未确认新会话"),
            "原因必须点名「未确认新会话」：{reason}"
        );
        assert!(
            !reason.contains("create|"),
            "原因里不得泄露内部锁键：{reason}"
        );
    }

    /// 出现多个新会话 → 归属不可判定：如实报歧义，绝不挑一个冒充（同一诚实红线）
    #[tokio::test]
    async fn run_create_reports_ambiguity_instead_of_guessing() {
        let inv = zcode::build_create_argv(&ZcodeSpec::win("D:/ZCode"), "hi", "E:/proj", None);
        let old = stored(UUID_A, "E:/proj");
        let fresh = stored_at(UUID_B, "E:/proj", created_now());
        let f2 = stored_at(UUID_C, "E:/proj", created_now());
        let (stored_fn, _) = stored_seq(vec![
            Some(vec![old.clone()]),
            Some(vec![old.clone(), fresh, f2]),
        ]);
        let deps = test_deps(
            stored_fn,
            keyed_store(vec![]),
            Box::new(|_| Ok(())),
            false,
            Arc::new(Mutex::new(Vec::new())),
            1,
        );
        let seam = scripted_seam(0, vec!["ZCode Built-in skipped (not-due)".to_string()]);
        let out = run_create(
            &inv,
            "E:/proj",
            PathPlan::Existing,
            &zcode::create_lock_key("E:/proj", "windows"),
            &make_runner,
            &deps,
            &*seam,
        )
        .await;
        assert_eq!(out.confirmation, NewSessionConfirmation::Unconfirmed);
        assert_eq!(out.receipt.session_id, "");
        let reason = out.receipt.reason.clone().unwrap_or_default();
        assert!(
            reason.contains("2 个新会话"),
            "必须如实报歧义数量：{reason}"
        );
    }

    /// 叶节点缺席（父目录在场）→ **单层**建目录（`create_dir` 语义，绝不递归）；
    /// 已在场则不建；建目录失败 → 投递前拒绝（零字节投递，原因点名）
    #[tokio::test]
    async fn run_create_creates_only_the_missing_leaf_dir() {
        let inv = zcode::build_create_argv(&ZcodeSpec::win("D:/ZCode"), "hi", "E:/proj/new", None);
        let made = Arc::new(Mutex::new(Vec::<String>::new()));
        let made2 = made.clone();
        let (stored_fn, _) = stored_seq(vec![Some(vec![])]);
        let deps = test_deps(
            stored_fn,
            keyed_store(vec![]),
            Box::new(move |p: &str| {
                made2
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(p.to_string());
                Ok(())
            }),
            false,
            Arc::new(Mutex::new(Vec::new())),
            1,
        );
        let seam = scripted_seam(0, vec!["ZCode Built-in skipped (not-due)".to_string()]);
        let _ = run_create(
            &inv,
            "E:/proj/new",
            PathPlan::CreateHere,
            &zcode::create_lock_key("E:/proj/new", "windows"),
            &make_runner,
            &deps,
            &*seam,
        )
        .await;
        assert_eq!(
            made.lock().unwrap().clone(),
            vec!["E:/proj/new".to_string()],
            "缺席叶节点建一次（且是归一化后的路径）"
        );

        // 已在场（Existing）→ 不建目录
        let made3 = Arc::new(Mutex::new(Vec::<String>::new()));
        let made4 = made3.clone();
        let (stored_fn2, _) = stored_seq(vec![Some(vec![])]);
        let deps2 = test_deps(
            stored_fn2,
            keyed_store(vec![]),
            Box::new(move |p: &str| {
                made4
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(p.to_string());
                Ok(())
            }),
            false,
            Arc::new(Mutex::new(Vec::new())),
            1,
        );
        let _ = run_create(
            &inv,
            "E:/proj",
            PathPlan::Existing,
            &zcode::create_lock_key("E:/proj", "windows"),
            &make_runner,
            &deps2,
            &*seam,
        )
        .await;
        assert!(made3.lock().unwrap().is_empty(), "已在场不建目录");

        // 建目录失败 → 投递前拒绝（零字节投递）
        let (stored_fn3, calls) = stored_seq(vec![Some(vec![])]);
        let deps3 = test_deps(
            stored_fn3,
            keyed_store(vec![]),
            Box::new(|_: &str| Err("磁盘只读".to_string())),
            false,
            Arc::new(Mutex::new(Vec::new())),
            1,
        );
        let out = run_create(
            &inv,
            "E:/proj/new",
            PathPlan::CreateHere,
            &zcode::create_lock_key("E:/proj/new", "windows"),
            &make_runner,
            &deps3,
            &*seam,
        )
        .await;
        assert_eq!(out.receipt.status, ReceiptStatus::Failed);
        assert_eq!(out.receipt.stage, Some(Stage::Refused));
        assert_eq!(out.receipt.session_id, "");
        assert!(
            out.receipt.reason.unwrap_or_default().contains("磁盘只读"),
            "原因必须带底层详情"
        );
        assert_eq!(
            *calls.lock().unwrap(),
            0,
            "建目录失败在回合起跑前拒绝（不读库、不 spawn）"
        );
    }

    /// **失败终态不许被抹成成功**：回合超时（看门狗终止）但库里确实建出了新会话 ⇒
    /// 如实保留 `timeout` 失败 + 填上**已确认**的会话号 + 注明「会话已建但本轮未正常结束」
    #[tokio::test]
    async fn run_create_keeps_failure_status_even_if_the_session_landed() {
        let inv = zcode::build_create_argv(&ZcodeSpec::win("D:/ZCode"), "hi", "E:/proj", None);
        let old = stored(UUID_A, "E:/proj");
        let fresh = stored_at(UUID_B, "E:/proj", created_now());
        let (stored_fn, _) = stored_seq(vec![
            Some(vec![old.clone()]),
            Some(vec![old.clone(), fresh]),
        ]);
        let deps = test_deps(
            stored_fn,
            keyed_store(vec![(UUID_B, snap(9, "m2", "答复", 4))]),
            Box::new(|_| Ok(())),
            false,
            Arc::new(Mutex::new(Vec::new())),
            1,
        );
        // 超时执行缝：watchdog 1ms + 回合体睡 50ms → runner 回执 Timeout
        let seam: Box<RunSeam> = Box::new(|cfg: RunnerCfg| {
            Box::pin(async move {
                let mut cfg = cfg.timeout_ms(1);
                let receipt = cfg.run_once(|_| std::thread::sleep(Duration::from_millis(50)));
                super::super::turn::TurnObs {
                    receipt,
                    stdout: cfg.captured_stdout(),
                    stderr: cfg.captured_stderr(),
                    exit: cfg.last_exit_code(),
                }
            })
        });
        let out = run_create(
            &inv,
            "E:/proj",
            PathPlan::Existing,
            &zcode::create_lock_key("E:/proj", "windows"),
            &make_runner,
            &deps,
            &*seam,
        )
        .await;
        assert_eq!(
            out.confirmation,
            NewSessionConfirmation::Store(UUID_B.into())
        );
        assert_eq!(
            out.receipt.status,
            ReceiptStatus::Failed,
            "回合失败不得因库里有新会话就升成 Ok（首句可能没送达）: {:?}",
            out.receipt
        );
        assert_eq!(out.receipt.stage, Some(Stage::Timeout));
        assert_eq!(out.receipt.session_id, UUID_B, "已确认的会话号必须如实给出");
        let reason = out.receipt.reason.clone().unwrap_or_default();
        assert!(
            reason.contains(UUID_B) && reason.contains("未正常结束"),
            "必须注明「会话已建但本轮未正常结束」：{reason}"
        );
    }

    /// **复审 Important 1**：CLI 先打出含 `sessionId` 的 JSON 帧、随后**崩溃/被信号杀死**
    /// ⇒ runner 把帧里的会话号放进 Crash 回执（`ReceiptSource::NotApplicable`），本模块的
    /// 帧路径只认 `StdoutJson` ⇒ 走未确认臂。此时回执**必须清空会话号**（未确认即空串），
    /// 否则线上就出现「`confirmation:"none"` + 非空 `sessionId`」的自相矛盾，
    /// 审计行的会话号列也不再等于「未确认」。
    #[tokio::test]
    async fn run_create_never_keeps_a_frame_id_when_the_turn_crashed() {
        let inv = zcode::build_create_argv(&ZcodeSpec::win("D:/ZCode"), "hi", "E:/proj", None);
        let old = stored(UUID_A, "E:/proj");
        // 库里**没有**新会话（崩溃形态：会话可能没落库）
        let (stored_fn, _) = stored_seq(vec![
            Some(vec![old.clone()]),
            Some(vec![old.clone()]),
            Some(vec![old.clone()]),
            Some(vec![old.clone()]),
        ]);
        let deps = test_deps(
            stored_fn,
            keyed_store(vec![]),
            Box::new(|_| Ok(())),
            false,
            Arc::new(Mutex::new(Vec::new())),
            3,
        );
        // 帧先出（合法会话号 + 半句回复），随后退出码 1（Crash）
        let seam: Box<RunSeam> = Box::new(|cfg: RunnerCfg| {
            let mut cfg = cfg
                .exit_code(1)
                .stderr_tail("boom")
                .stdout_lines(vec![format!(
                    "{{\"sessionId\":\"{UUID_B}\",\"response\":\"半句话\"}}"
                )]);
            Box::pin(async move {
                let receipt = cfg.run_once(|_| {});
                super::super::turn::TurnObs {
                    receipt,
                    stdout: cfg.captured_stdout(),
                    stderr: cfg.captured_stderr(),
                    exit: cfg.last_exit_code(),
                }
            })
        });
        let out = run_create(
            &inv,
            "E:/proj",
            PathPlan::Existing,
            &zcode::create_lock_key("E:/proj", "windows"),
            &make_runner,
            &deps,
            &*seam,
        )
        .await;
        assert_eq!(out.confirmation, NewSessionConfirmation::Unconfirmed);
        assert_eq!(out.receipt.stage, Some(Stage::Crash), "{:?}", out.receipt);
        assert_eq!(
            out.receipt.session_id, "",
            "崩溃回合里打过的帧号不得冒充「已确认会话」（未确认 ⇒ 空串）"
        );
        let reason = out.receipt.reason.clone().unwrap_or_default();
        assert!(
            reason.contains("未确认新会话") && reason.contains("boom"),
            "原因须同时保留崩溃证据与未确认结论：{reason}"
        );
        // 帧里的摘要如实保留（只清会话号，不抹掉别的证据）
        assert_eq!(out.receipt.last_assistant.as_deref(), Some("半句话"));
    }

    /// **复审 Important 2 的两副面孔**：
    /// ① 基线读**不可得**（`None`）⇒ 放弃发现、如实未确认，**绝不**把后来读成功的
    ///    「项目里唯一那个旧会话」报成新建；
    /// ② 基线里缺行（窗口截断/读得不全）但该行**创建于回合之前** ⇒ 同样不是本轮新建
    ///    （`time_created` 主证据的结构防线）。
    #[tokio::test]
    async fn run_create_never_reports_a_pre_existing_session_from_a_failed_baseline() {
        let inv = zcode::build_create_argv(&ZcodeSpec::win("D:/ZCode"), "hi", "E:/proj", None);
        let seam = scripted_seam(0, vec!["ZCode Built-in skipped (not-due)".to_string()]);
        let old_proj = stored_at(UUID_A, "E:/proj", created_before());
        // ① 基线 = None（库不可读），但脚本里**已经备好**下一次读成功会吐出的旧会话——
        //    如果实现把它当空基线用，就会出现「旧会话 = 新建」的假确认
        let (stored_fn, calls) = stored_seq(vec![None, Some(vec![old_proj.clone()])]);
        let deps = test_deps(
            stored_fn,
            keyed_store(vec![]),
            Box::new(|_| Ok(())),
            false,
            Arc::new(Mutex::new(Vec::new())),
            1,
        );
        let out = run_create(
            &inv,
            "E:/proj",
            PathPlan::Existing,
            &zcode::create_lock_key("E:/proj", "windows"),
            &make_runner,
            &deps,
            &*seam,
        )
        .await;
        assert_eq!(
            out.confirmation,
            NewSessionConfirmation::Unconfirmed,
            "基线不可得 ⇒ 不猜（{:?}）",
            out.receipt
        );
        assert_eq!(out.receipt.session_id, "");
        assert_eq!(
            *calls.lock().unwrap(),
            1,
            "基线不可得 ⇒ 发现被放弃（不再轮询读库，也就没有机会把旧会话说成新建）"
        );
        let reason = out.receipt.reason.clone().unwrap_or_default();
        assert!(reason.contains("基线不可读"), "原因须点名基线：{reason}");

        // ② 基线可读但**缺行**（空表）+ 唯一的行创建于回合之前 ⇒ 判 None（不是新建）
        let (stored_fn2, _) = stored_seq(vec![Some(vec![]), Some(vec![old_proj.clone()])]);
        let deps2 = test_deps(
            stored_fn2,
            keyed_store(vec![]),
            Box::new(|_| Ok(())),
            false,
            Arc::new(Mutex::new(Vec::new())),
            1,
        );
        let out2 = run_create(
            &inv,
            "E:/proj",
            PathPlan::Existing,
            &zcode::create_lock_key("E:/proj", "windows"),
            &make_runner,
            &deps2,
            &*seam,
        )
        .await;
        assert_eq!(
            out2.confirmation,
            NewSessionConfirmation::Unconfirmed,
            "回合前建的会话不在基线里也**不是**本轮新建（time_created 防线）：{:?}",
            out2.receipt
        );
        assert_eq!(out2.receipt.session_id, "");

        // ③ 轮询期间库**变得不可读**（基线可读 + 首轮读出 None）⇒ 立即停轮询、如实未确认
        //    ——不把「读不到」当「没有新会话」继续空等，也不据此下任何结论
        let (stored_fn3, calls3) = stored_seq(vec![Some(vec![]), None, Some(vec![])]);
        let deps3 = test_deps(
            stored_fn3,
            keyed_store(vec![]),
            Box::new(|_| Ok(())),
            false,
            Arc::new(Mutex::new(Vec::new())),
            3,
        );
        let out3 = run_create(
            &inv,
            "E:/proj",
            PathPlan::Existing,
            &zcode::create_lock_key("E:/proj", "windows"),
            &make_runner,
            &deps3,
            &*seam,
        )
        .await;
        assert_eq!(out3.confirmation, NewSessionConfirmation::Unconfirmed);
        assert_eq!(
            *calls3.lock().unwrap(),
            2,
            "首轮读出不可读即停（不再空等后两轮）"
        );
    }

    /// 争用锁（工作区忙）语义**沿用 Task 8 单源**：重试用尽 → 如实的 `workspace_busy`
    /// 失败回执；新建路径同样不冒充成功、不给会话号
    #[tokio::test]
    async fn run_create_reports_workspace_busy_honestly() {
        let inv = zcode::build_create_argv(&ZcodeSpec::win("D:/ZCode"), "hi", "E:/proj", None);
        // 基线 + 三次「读得到但没有新会话」：让两段节奏各自走满（争用退避 2 次 + 库确认 2 次）
        let (stored_fn, _) =
            stored_seq(vec![Some(vec![]), Some(vec![]), Some(vec![]), Some(vec![])]);
        let waits = Arc::new(Mutex::new(Vec::new()));
        let deps = test_deps(
            stored_fn,
            keyed_store(vec![]),
            Box::new(|_| Ok(())),
            true,
            waits.clone(),
            1,
        );
        let seam = scripted_seam(usize::MAX, vec![]);
        let out = run_create(
            &inv,
            "E:/proj",
            PathPlan::Existing,
            &zcode::create_lock_key("E:/proj", "windows"),
            &make_runner,
            &deps,
            &*seam,
        )
        .await;
        assert!(out.busy_final);
        assert_eq!(out.attempts, zcode::BUSY_MAX_RETRIES + 1);
        assert_eq!(out.receipt.stage, Some(Stage::WorkspaceBusy));
        assert_eq!(out.receipt.session_id, "");
        assert_eq!(
            waits.lock().unwrap().clone(),
            vec![
                // 争用锁退避（Task 8 单源：5s × 重试上限 2）
                zcode::BUSY_RETRY_SPACING_MS,
                zcode::BUSY_RETRY_SPACING_MS,
                // 回合后的库发现轮询（有界：3 次读 → 2 次间隔，1s）
                zcode::STORE_CONFIRM_INTERVAL_MS,
                zcode::STORE_CONFIRM_INTERVAL_MS,
            ],
            "两段节奏各自单源（争用 5s / 库确认 1s）"
        );
    }
}
