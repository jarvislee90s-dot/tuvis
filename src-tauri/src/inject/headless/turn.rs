// 共享「回合」件（Task 9 复审上提）：**通道无关**的单回合机械——执行缝、回执→审计词、
// 会话串行锁 / 取消靶子登记表、证据头。zcode（Task 8）与 codex（Task 9）共用；
// WB（Task 11）与 H11 三家（Task 13）落地时**依赖本模块**，不要再从 `zcode.rs` 取。
//
// **为什么独立成文件**（复审裁决）：这些件原先长在 `zcode.rs` 里，codex 落地后不得不
// `use zcode::{RunSeam, registry, TurnSlot, TurnObs}`——`zcode.rs` 实际上变成了无头层
// 的底座模块，WB 接入时会第三次复制这层耦合。上提后各通道**只依赖底座**
// （`mod.rs` / `receipt.rs` / `runner.rs` / `gate.rs` / `turn.rs`），通道之间不互相依赖。
//
// **零行为变化**：定义逐字搬迁（含既有文档与纪律说明），`zcode.rs` 以 `pub use` 转出
// 保持 Task 8 既有调用面（`zcode::registry()` / `zcode::TurnSlot` / `zcode::TurnObs` …）
// 不变——两套套件（zcode / codex）全绿即为证据。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::receipt::{Receipt, ReceiptStatus};
use super::runner::{CancelHandle, RunnerCfg};

/// 盒装 future 别名（执行缝/等待缝共用；避免 clippy::type_complexity）
pub type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;

/// 证据串行数上限（各类退出分类/锁判定的输入行数；子串匹配只看头部）
pub const EVIDENCE_HEAD_LINES: usize = 32;

// ============================================================
// CLI 发现 / Windows 垫片 spawn 形态（**通道无关**；Task 13 自 `codex.rs` 上提）
// ============================================================
//
// **为什么上提**（Task 13）：H11 三家 CLI（claude/kimi/opencode）与本机 codex 一样，
// 在 Windows 上常是 npm 垫片（`claude.cmd` / `opencode.cmd`；CreateProcess 不认批处理），
// 需要**同一套** PATH 扫描 + `cmd /c` 包装。留在 `codex.rs` 会让 `cli_three.rs` 变成
// 「通道依赖通道」——与 Task 9 复审上提 `RunSeam`/`registry` 同一条纪律。
// **零行为变化**：定义逐字搬迁 + 泛化（`codex_in_path` → [`cli_in_path`]），
// `codex.rs` 以 `pub use` 与薄委托保持既有调用面与既有测试不变。

/// PATH 分段（纯核）：分隔符按平台取（Windows `;` / 其余 `:`），空段与引号壳滤除
pub fn path_dirs(path_env: &str, os: &str) -> Vec<String> {
    let sep = if os == "windows" { ';' } else { ':' };
    path_env
        .split(sep)
        .map(|d| d.trim().trim_matches('"'))
        .filter(|d| !d.is_empty())
        .map(str::to_string)
        .collect()
}

/// PATH 扫描（纯核，**按工具名泛化**）：**按 PATH 顺序**在每段目录内试
/// `.exe` → `.cmd` → `.bat`（与 Windows 自身的解析顺序一致——PATH 顺序决定用哪个安装，
/// 目录内则真实可执行体优先：垫片要经 `cmd` 转一手，而 cmd 会重解析命令行）；POSIX 找裸名文件。
/// 找不到 → `None`（调用方如实拒绝，**绝不 spawn 不存在的程序**）
pub fn cli_in_path(tool: &str, path_env: &str, os: &str) -> Option<String> {
    let exts: &[&str] = if os == "windows" {
        &[".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    for dir in path_dirs(path_env, os) {
        for ext in exts {
            let cand = std::path::Path::new(&dir).join(format!("{tool}{ext}"));
            if cand.is_file() {
                return Some(cand.to_string_lossy().to_string());
            }
        }
    }
    None
}

/// 是否 Windows 批处理垫片（`.cmd`/`.bat`；CreateProcess 不认批处理）
pub fn shim_needs_cmd(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|e| e == "cmd" || e == "bat")
}

/// spawn 形态（Windows 垫片经 cmd 转一手；其余直 spawn）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnShape {
    pub program: String,
    pub prefix: Vec<String>,
}

impl SpawnShape {
    /// 是否**经 Windows 批处理垫片**转一手（`cmd /c <垫片>`）。
    ///
    /// 唯一判据 = [`spawn_shape`] 的构造产物（`program == "cmd"` 且前置 argv 首元素是 `/c`）：
    /// 别在别处另立「是不是垫片」的判据（两处判据会漂移，而漂移的方向是**漏拒**）。
    pub fn via_cmd_shim(&self) -> bool {
        self.program == "cmd"
            && self
                .prefix
                .first()
                .is_some_and(|a| a.eq_ignore_ascii_case("/c"))
    }
}

/// spawn 形态构造（唯一构造点）。**登记限制**：`cmd /c` 会重解析命令行——正文中的
/// `%VAR%` 会被展开（未定义变量原样保留）；这是 Windows npm 垫片的固有代价，
/// 故 [`cli_in_path`] 让 `.exe` 优先。**R3 起**该限制由 [`cmd_shim_body_refusal`]
/// 前移到**投递前拒绝**（含元字符的正文在垫片形态下不投递，见该函数）。
pub fn spawn_shape(exe: &str, os: &str) -> SpawnShape {
    if os == "windows" && shim_needs_cmd(exe) {
        SpawnShape {
            program: "cmd".to_string(),
            prefix: vec!["/c".to_string(), exe.to_string()],
        }
    } else {
        SpawnShape {
            program: exe.to_string(),
            prefix: Vec::new(),
        }
    }
}

// ============================================================
// cmd 垫片正文拒绝（P0 安全：Task 9 的「已知限制」升级为**投递前拒绝**）
// ============================================================

/// `cmd` 会**重解析**的元字符。依据（Task 9 复审登记）：`cmd /c` 对整条命令行做自己的
/// 解析——`&` `|` 串联、`^` 转义、`<` `>` 重定向、`"` 的配对规则与 CRT 不同、
/// `%` 展开变量（未定义变量原样保留）；而 Rust 传参用的是 **CRT** 的 `\"` 转义风格，
/// **cmd 不认**⇒ 含这些字符的用户正文可能被**截断/改写/串联执行**。
pub const CMD_SHIM_METACHARS: &[char] = &['&', '|', '^', '<', '>', '"', '%'];

/// 被判元字符的**分段身份**（评审 I8 文案诚实化）：字符表与判定只有一条
/// （[`cmd_shim_refusal`]），但文案必须**点名实际命中的那一段**——项目路径命中时说
/// 「正文含 cmd 元字符」是**错报对象**（用户会去改正文，而真凶是项目路径，改多少遍都没用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdShimSubject {
    /// 用户正文（R3 正文面）
    Body,
    /// 项目路径（`-C` 的取值；评审 I8 项目路径面）
    ProjectPath,
    /// argv 里其余载荷分段（逐段判据的兜底面——不是正文也不是项目路径的那一段）
    ArgvSegment,
}

impl CmdShimSubject {
    /// 文案里的主语（**单一词表**：调用方不得自拼）
    pub fn label(self) -> &'static str {
        match self {
            CmdShimSubject::Body => "正文",
            CmdShimSubject::ProjectPath => "项目路径（-C 取值）",
            CmdShimSubject::ArgvSegment => "命令行分段",
        }
    }
}

/// **载荷分段 + 已解析 spawn 形态 ⇒ 放行 / 拒绝**（单一决策点；`Some(reason)` = 拒发）。
///
/// 只有「该分段进 argv」且「形态经 `cmd` 垫片」两条同时成立才判——故：
/// - **`.exe` 直装**（形态不是 `cmd /c`）⇒ 放行（CRT 引号规则与 CreateProcess 一致，无重解析面）；
/// - **正文走 stdin 的通道**（claude：`--input-format stream-json`）⇒ `in_argv = false`
///   ⇒ 放行（正文根本不经过命令行，垫片重解析面不存在——**别顺手把它也拒了**）。
///
/// `text` 必须是**用户可控的那一段原文**（不含我们追加的 `[mobile <花名>]` 签名——签名是
/// 我们自己拼的，不该由用户的判据代为背书）。判据是**纯函数**：零 I/O、零进程，故可单测；
/// 调用点的纪律见 `remote::api` 的分派点与 `codex::argv_shim_refusal`（拒绝 = `refused`，
/// 未起跑、零字节投递）。
pub fn cmd_shim_refusal(
    subject: CmdShimSubject,
    text: &str,
    shape: &SpawnShape,
    in_argv: bool,
) -> Option<String> {
    if !in_argv || !shape.via_cmd_shim() {
        return None;
    }
    let hit: Vec<String> = CMD_SHIM_METACHARS
        .iter()
        .filter(|c| text.contains(**c))
        .map(|c| c.to_string())
        .collect();
    if hit.is_empty() {
        return None;
    }
    // 文案风格同斜杠命令拒绝（spec H7 / remote::api）：**显式告知**为什么 + 怎么办，
    // 绝不静默改写载荷（改写了用户就不知道 CLI 实际收到了什么）
    Some(format!(
        "TURN_CMD_SHIM_UNSAFE: {subject}含 cmd 元字符（{hits}）：本机 CLI 是 npm 垫片（.cmd/.bat，经 `cmd /c` 启动），\
         cmd 会重解析整条命令行（串联/重定向/转义/变量展开，引号配对规则也与 CRT 不同）——\
         {subject}可能被截断、改写或串联执行。本条**未投递**（零字节投递）：请改用直装 CLI（.exe）\
         或去掉这些字符后重发",
        subject = subject.label(),
        hits = hit.join(" ")
    ))
}

/// 正文面的薄壳（等价于 [`cmd_shim_refusal`] 取 [`CmdShimSubject::Body`]）：既有调用点
/// （`remote::api` 的 codex/cli 正文门）语义与文案**逐字未变**，故不需要跟着改签名。
pub fn cmd_shim_body_refusal(
    text: &str,
    shape: &SpawnShape,
    body_travels_in_argv: bool,
) -> Option<String> {
    cmd_shim_refusal(CmdShimSubject::Body, text, shape, body_travels_in_argv)
}

// ============================================================
// 花名 / 设备名的 cmd 安全白名单（裁决 24b：R3 的**签名面**收口）
// ============================================================

/// 白名单中**除 Unicode 字母数字外**的允许字符（判据 = [`char::is_alphanumeric`] ∪ 本表）。
///
/// 只放命令行里常见、且**无 `cmd` 语义**的五个：空格（花名里的自然词界）、`-` `_` `.` `·`
/// （中英文名常见的分隔与点缀）。**其余一律不安全**——含 ASCII 标点 `& | ^ < > " % ! ( )`
/// 等（`cmd` 重解析面，依据同 [`CMD_SHIM_METACHARS`] / [`cmd_shim_body_refusal`]），
/// 也含一切符号与控制字符（emoji、换行、反斜杠）。
///
/// 为什么允许「空格」：花名里的空格不会构成 `cmd` 语义（`cmd /c` 下空格只分词，不改写
/// 命令行结构；CRT/cmd 的引号规则差异只对 `"` 生效），而「小明的 iPhone」这类花名很常见。
pub const DEVICE_NAME_EXTRA_ALLOWED: &[char] = &[' ', '-', '_', '.', '·'];

/// 危险字符的**展示**形态：控制字符打转义（别把裸换行/ESC 打进给用户看的文案），其余原样
fn device_name_char_label(c: char) -> String {
    if c.is_control() || c == '\u{7f}' {
        format!("\\u{{{:X}}}", c as u32)
    } else {
        c.to_string()
    }
}

/// **花名（设备名）白名单判定 —— 本判据的唯一决策点**（`Some(reason)` = 不安全，须拒）。
///
/// # 为什么是白名单，不是黑名单（裁决 24b；R3 签名面收口）
/// R3 只判了**用户正文**（[`cmd_shim_body_refusal`]），而 ` [mobile <花名>]` 这个签名是
/// **我们自己拼上去的**——它同样会进无头 CLI 的 argv（`codex queue --message` /
/// `kimi --prompt` / `opencode run <位置参数>` / `zcode.cjs --prompt`），Windows 上这些 CLI
/// 常是 npm 垫片（`.cmd` ⇒ `cmd /c`）⇒ **签名也会被 `cmd` 重解析**。黑名单永远补不全
/// （`cmd` 的解析面随上下文变化、还有转义与引号配对规则差异），白名单把「能进命令行的字符」
/// 钉死成可枚举的一类。
///
/// # 判据（字符类，不是字节）
/// [`char::is_alphanumeric`] ∪ [`DEVICE_NAME_EXTRA_ALLOWED`]——`is_alphanumeric` 覆盖全部
/// Unicode 字母/数字，故**中文花名天然通过**（「小明的手机」）；emoji、ASCII 标点、控制
/// 字符一律不安全。
///
/// # 有意从严（裁决 25；2026-10-05 用户裁决——**勿改为按威胁面放宽**）
/// 白名单是**故意收窄**的：自定义花名用**常规字符**（字母/数字/空格/`-` `_` `.` `·`）即可，
/// **emoji 与未列举符号一律拒**——**即便**某个符号在 `cmd` 下未必真能被解析成动作。
/// 用户理由：要在 `cmd` 威胁面**之上留余量**，不做「逐个字符论证无害」的减法。
/// 故**不要**因为「这个字符看起来没有 `cmd` 语义」而放宽本判据：本判据是**字符类白名单**，
/// 不是威胁面清单（改判据前先读本条）。
///
/// # 调用点（**本轮只有一处**；勿在文档里写成三处）
/// ① **签名组装单点** [`crate::inject::normalize::compose_injection`]：**拼接之前**判，
///    不安全即 `Err(reason)`，调用方按 `refused` 档 fail closed（零字节投递）。
///
/// **未接线的两处（如实登记，属 #117 续作）**：配对登记点
/// （`remote::api::persist_and_cookie`）与设备改名内核（`remote::rename_device_core`）
/// 当前**没有**调用本判据——危险花名仍可登记/改名成功，直到首次投递时由 ① 拒绝（零投递，
/// 不会误投，但用户要到发送时才知道）。这是本轮「不扩 main 既有配对/改名行为」的取舍；
/// 若续作要补「注册即拒」，需要在两处接同一判据并补 HTTP 400 错误码。
///
/// **不做 migration**：存量已登记的危险花名由 ① 在**投递时**兜住。
pub fn device_name_refusal(name: &str) -> Option<String> {
    let mut hit: Vec<char> = Vec::new();
    for c in name.chars() {
        if c.is_alphanumeric() || DEVICE_NAME_EXTRA_ALLOWED.contains(&c) {
            continue;
        }
        if !hit.contains(&c) {
            hit.push(c);
        }
    }
    if hit.is_empty() {
        return None;
    }
    // 文案风格同 [`cmd_shim_body_refusal`]：**显式告知**是哪个字符 + 为什么 + 怎么办，
    // 绝不静默改写花名（改写了用户就不知道 CLI 实际收到的是什么签名）
    let shown: Vec<String> = hit
        .iter()
        .take(3)
        .map(|c| device_name_char_label(*c))
        .collect();
    let more = if hit.len() > 3 {
        format!(" (+{})", hit.len())
    } else {
        String::new()
    };
    Some(format!(
        "TURN_DEVICE_NAME_UNSAFE: 设备花名含命令行不安全字符（{}{more}）：花名会作为 `[mobile <花名>]` 签名拼进无头 \
         CLI 的命令行，而 Windows 上这些 CLI 常是 npm 垫片（.cmd/.bat 经 `cmd /c` 启动）——\
         cmd 会重解析整条命令行（串联/重定向/转义/变量展开，引号配对规则也与 CRT 不同），\
         签名可能被截断、改写或串联执行。白名单 = **Unicode 字母数字 + 空格 `-` `_` `.` `·`**；\
         本条**未投递**（零字节投递）：请在 MAM 设备花名册把该设备改名（去掉这些字符）后重发",
        shown.join(" ")
    ))
}

/// 证据头（生产/测试共用）：前 [`EVIDENCE_HEAD_LINES`] 行拼接（判定为子串匹配）
pub fn head_of(lines: &[String]) -> String {
    lines
        .iter()
        .take(EVIDENCE_HEAD_LINES)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

/// 一次**尝试**的观测（执行缝的产物；回执 + 原始流供退出分类/回执归一消费）
#[derive(Debug, Clone)]
pub struct TurnObs {
    pub receipt: Receipt,
    pub stdout: Vec<String>,
    pub stderr: String,
    pub exit: Option<i32>,
}

/// 执行缝（生产 = `RunnerCfg::run().await` 真 spawn；测试 = Task 6 的 `run_once` 脚本缝）
pub type RunSeam = dyn Fn(RunnerCfg) -> BoxFuture<TurnObs> + Send + Sync;

/// 生产执行缝（真 spawn：`RunnerCfg::run().await`；超时/取消/kill 树/在飞登记全归
/// [`super::runner`]）。断言用脚本缝见各通道测试模块（Task 6 的 `run_once`）。
pub fn production_run_seam() -> Box<RunSeam> {
    Box::new(|mut cfg: RunnerCfg| {
        Box::pin(async move {
            let receipt = cfg.run().await;
            TurnObs {
                receipt,
                stdout: cfg.captured_stdout(),
                stderr: cfg.captured_stderr(),
                exit: cfg.last_exit_code(),
            }
        })
    })
}

/// 回执 → 审计 result 词（H6 口径：终态；失败带阶段码。阶段词经 serde **单一来源**
/// 取 [`super::receipt::Stage`] 的 wire 名，不另抄一份词表）
pub fn receipt_result_word(r: &Receipt) -> String {
    match r.status {
        ReceiptStatus::Ok => "ok".to_string(),
        ReceiptStatus::Queued => "queued".to_string(),
        ReceiptStatus::Cancelled => "cancelled".to_string(),
        ReceiptStatus::Failed => match r.stage {
            Some(st) => {
                let word = serde_json::to_value(st)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_else(|| "unknown".to_string());
                format!("failed({word})")
            }
            None => "failed".to_string(),
        },
    }
}

/// 末条 assistant 的**摘要口径单点**（截断长度 = [`super::receipt::LAST_ASSISTANT_CHARS`]、
/// 截断语义 = [`crate::inject::normalize::summarize`]，与 W5 审计摘要同源）。
///
/// **为什么单独成函数**（Task 12 复审 Minor 3）：[`ok_receipt_with_assistant`] 与
/// 「回执已在手、只补摘要」的调用点（`zcode_create::fill_missing_from_store` /
/// `confirmed_receipt` 的失败臂）必须走同一条截断——各自内联
/// `summarize(.., LAST_ASSISTANT_CHARS)` 就是同一口径的两份写法，改一处漏一处不会编译报错。
pub fn assistant_summary(text: &str) -> String {
    crate::inject::normalize::summarize(text, super::receipt::LAST_ASSISTANT_CHARS)
}

/// 库/通道确认成功时的 **Ok 回执**（`lastAssistant` 摘要 + tokens）。
///
/// **为什么在底座**（Task 12 复审式上提）：`zcode.rs` 原有的私有 `ok_receipt_from_store`
/// 是**通道无关**的收尾件（截断口径 = [`super::receipt::LAST_ASSISTANT_CHARS`] +
/// [`crate::inject::normalize::summarize`] 单点），而 H10 新建路径（`zcode_create.rs`）
/// 从会话库确认新会话时要用同一条口径——留在 `zcode.rs` 就会从通道模块外借私有件，
/// 或被迫抄第二份截断。定义上提到此，`zcode.rs` 的既有调用改为委托（**零行为变化**）。
pub fn ok_receipt_with_assistant(
    session_id: &str,
    text: &str,
    tokens: Option<u64>,
    duration_ms: u64,
) -> Receipt {
    let mut r = Receipt::ok(session_id, duration_ms);
    r.last_assistant = Some(assistant_summary(text));
    r.tokens = tokens;
    r
}

// ============================================================
// 会话串行锁 + 取消靶子（MAM 自己的；zcode 无头不拒绝并发，codex 复用同一表）
// ============================================================

/// 取消请求口（Task 6 的 [`CancelHandle`] 包一层，便于测试注入「送达/未送达」两形态）
pub type CancelFn = Arc<dyn Fn() -> bool + Send + Sync>;

/// 在飞回合槽位（取消靶子 + 回合起跑时刻 + 审计上下文）。
///
/// **不含设备身份**：回合审计行的设备取**发起发送的**设备（端点持有），取消审计行的设备
/// 取**按下取消的**设备（取消端点持有）——槽位只保留两者都要用的回合自身信息。
pub struct TurnSlot {
    pub cancel: CancelFn,
    /// 取消靶子**是否武装过**（[`TurnRegistry::arm`] 置 true）。
    ///
    /// 取消端点据此分辨两种都返回 `false` 但**语义不同**的形态（Task 11 复审 Important 3）：
    /// - 未武装 + 通道本批未接线（H9 WB ACP）⇒「本通道未接线取消，**回合仍在运行**」；
    /// - 未武装 + 已接线通道（zcode/codex 的版本门控/预检窗口）⇒「尚未进入可取消阶段，
    ///   **回合仍在运行**，请稍后重试」；
    /// - 已武装 ⇒ 回合已终结/已被取消（先到者生效）。
    pub cancel_wired: bool,
    pub started: std::time::Instant,
    /// 本回合的注入正文（取消审计行与回合审计行同源）
    pub content: String,
    /// 会话工具 id（回合/取消两行审计的 agent_type 列）
    pub agent_type: String,
    /// 本回合的**无头通道 wire 名**（取消审计行 channel 列的**单点来源**）。
    ///
    /// **为什么存在**（Task 11 复审 Important 2）：取消端点原先把 channel 写死
    /// `headless_zcode`，而 codex 回合**真的武装取消靶子** ⇒ 送达的 codex 取消被记成
    /// `headless_zcode`（审计/历史把取消归错通道）。通道名由各通道占位时声明
    /// （[`TurnSlot::with_channel`]），端点从槽位取，**不得再写常量**。
    /// 空串 = 未声明（只可能来自测试桩或将来新增通道漏声明——端点按
    /// `headless_unattributed` 如实落账并 warn，不冒充任何既有通道）。
    pub channel: String,
}

impl TurnSlot {
    /// 占位槽（取消靶子待 [`TurnRegistry::arm`] 逐尝试更新）
    pub fn placeholder(agent_type: &str, content: String) -> Self {
        Self {
            cancel: Arc::new(|| false),
            cancel_wired: false,
            started: std::time::Instant::now(),
            content,
            agent_type: agent_type.to_string(),
            channel: String::new(),
        }
    }

    /// 声明本回合的无头通道（取消审计行 channel 列的单点来源；生产三条分派臂都必须声明）
    pub fn with_channel(mut self, channel: &str) -> Self {
        self.channel = channel.to_string();
        self
    }
}

/// 取消端点**一次取全**的槽位快照（内容/工具/通道/靶子是否武装/已跑时长）。
///
/// **为什么一次取全**（Task 11 复审 Important 2/3）：取消行要写通道名、措辞要分辨
/// 「未接线 / 未武装 / 已终结」——分两次查表既有竞态窗口，也逼调用方另抄一份判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelSnapshot {
    pub content: String,
    pub agent_type: String,
    pub channel: String,
    pub cancel_wired: bool,
    pub elapsed_ms: u64,
}

/// 进程级在飞登记表（会话串行锁的唯一判据出口）
#[derive(Default)]
pub struct TurnRegistry {
    map: Mutex<HashMap<String, TurnSlot>>,
}

/// 在飞槽位的 **RAII 守卫**（评审 I14）：[`TurnRegistry::begin_guarded`] 占位成功即返回它，
/// **Drop 自动注销槽位**。
///
/// # 为什么需要
/// `begin`/`end` 手工配对时代，每个 early-return 臂、每个 detached 任务、每次 panic 展开
/// 都要记得 `end`——漏一处就是该会话**永久**「已有在飞回合」拒绝，直到 MAM 重启
/// （`TurnRegistry` 无 TTL）。把配对收敛到类型上：占位者拿到守卫，守卫 Drop 即注销。
///
/// # 与旧 `end` 逐条等价的不变量
/// - **refuse 路径不碰锁**：`begin_guarded` 失败**不返回守卫** ⇒ 调用方**无从**注销别人的
///   槽位（旧 API 下「按会话号误删下一发槽位」的风险是真实存在过的——见 `zcode.rs` 的
///   U1 记载）；本类型把那条竞态变成**不可表达**；
/// - **单主**：只有守卫持有者能注销，且守卫**随任务移动**（detached 任务收尾 = 任务内
///   Drop）——不再有「任何人按会话号全局注销」的口子；
/// - **可显式早释放**：[`TurnGuard::release`]（与旧 `end` 同一时刻生效；用于「先释放锁、
///   再写审计行」这类既有顺序），语义 = 消费守卫、Drop 不再重复注销。
#[must_use = "守卫 Drop 即注销槽位；拿到就丢等于刚占位又立刻让出会话串行锁"]
pub struct TurnGuard<'a> {
    session_id: String,
    registry: &'a TurnRegistry,
    armed: bool,
}

impl TurnGuard<'_> {
    /// 显式早释放（等价于旧 `end`）：消费守卫、立刻注销；Drop 不再重复注销。
    pub fn release(mut self) {
        self.armed = false;
        self.registry.retire(&self.session_id);
    }

    /// 本守卫占的会话号（诊断/测试可观测面）
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.registry.retire(&self.session_id);
        }
    }
}

impl TurnRegistry {
    /// 占位内核（`false` = 该会话已有在飞回合）。**私有**：对外只有守卫入口
    /// [`Self::begin_guarded`] 与测试面 shim [`Self::begin`]——手工配对面不出口到生产。
    fn insert(&self, session_id: &str, slot: TurnSlot) -> bool {
        let mut g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if g.contains_key(session_id) {
            return false;
        }
        g.insert(session_id.to_string(), slot);
        true
    }

    /// 占位并返回 **RAII 守卫**（评审 I14；**生产唯一入口**）：`None` = 该会话已有在飞回合
    /// ——调用方必须**如实拒绝**（不排队、不覆盖）。拿不到守卫就没有注销能力——这正是
    /// 「refuse 路径不碰锁」的类型级保证。
    pub fn begin_guarded(&self, session_id: &str, slot: TurnSlot) -> Option<TurnGuard<'_>> {
        if !self.insert(session_id, slot) {
            return None;
        }
        Some(TurnGuard {
            session_id: session_id.to_string(),
            registry: self,
            armed: true,
        })
    }

    /// 手工占位（**仅测试面 shim**，评审 I14）：返回裸 `bool`，注销靠 [`Self::end`] 手工配对
    /// ——生产**已零调用点**（七条分派臂全走 [`Self::begin_guarded`]），故按「生产构建零调用」
    /// 收窄到 `#[cfg(test)]`：与 [`super::runner::GlobalSem::acquire`] 的 R11 同款处置，
    /// **把手工配对的面从生产 API 里拿掉**，而不是留个口子等后人用出新漏点。
    /// 用例要测「占位被拒」这类注册表自身语义时用它；要测注销语义请用守卫。
    #[cfg(test)]
    pub fn begin(&self, session_id: &str, slot: TurnSlot) -> bool {
        self.insert(session_id, slot)
    }

    /// 更新取消靶子（每尝试一次；无槽位时 no-op——测试直驱各通道 `run_turn` 不建槽）。
    /// **同时置 [`TurnSlot::cancel_wired`]**：取消端点据此分辨「未武装」与「已终结」
    /// （两者都返回 `false`，但措辞必须不同——Task 11 复审 Important 3）
    pub fn arm(&self, session_id: &str, cancel: CancelFn) {
        let mut g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = g.get_mut(session_id) {
            slot.cancel = cancel;
            slot.cancel_wired = true;
        }
    }

    /// 更新槽位声明的通道（与 [`Self::arm`] 同型的更新口；Task 6b review M-3）：
    /// 改道（exec 计划 → 单写者锁拒 → 改投 queue）落定进入消费等待时同步槽位——
    /// 取消端点的分叉判据读槽位通道词，不同步则改道腿拿不到「queue 滞留」语义。
    /// 无槽位时 no-op（同 [`Self::arm`]——测试直驱内核不建槽）
    pub fn update_channel(&self, session_id: &str, channel: &str) {
        let mut g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = g.get_mut(session_id) {
            slot.channel = channel.to_string();
        }
    }

    /// 回合终结：注销槽位（**内部**：单主的实现面——只有 [`TurnGuard`] 调得动）
    fn retire(&self, session_id: &str) {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }

    /// 手工注销（**仅测试面 shim**）：与 [`Self::begin`] 成对，理由同它（生产零调用点，
    /// 收窄到 `#[cfg(test)]`）。
    #[cfg(test)]
    pub fn end(&self, session_id: &str) {
        self.retire(session_id)
    }

    pub fn in_flight(&self, session_id: &str) -> bool {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(session_id)
    }

    /// 请求取消：`None` = 无在飞回合；`Some(false)` = 有回合但取消未送达（已终结/已取消
    /// ——先到者生效）；`Some(true)` = 送达。**不注销槽位**（终结由回合自身 [`Self::end`]
    /// 负责，取消只是请求）。
    pub fn request_cancel(&self, session_id: &str) -> Option<bool> {
        let cancel = {
            let g = self.map.lock().unwrap_or_else(|e| e.into_inner());
            g.get(session_id).map(|s| s.cancel.clone())
        };
        cancel.map(|f| f())
    }

    /// 取消端点**一次取全**的快照（内容/工具/通道/靶子是否武装/已跑时长）——
    /// 取消审计行的 channel 列与措辞判定都从这里取，**不再写常量、不再分两次查表**
    pub fn cancel_snapshot(&self, session_id: &str) -> Option<CancelSnapshot> {
        let g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        g.get(session_id).map(|s| CancelSnapshot {
            content: s.content.clone(),
            agent_type: s.agent_type.clone(),
            channel: s.channel.clone(),
            cancel_wired: s.cancel_wired,
            elapsed_ms: s.started.elapsed().as_millis() as u64,
        })
    }

    /// 三元素槽位快照（诊断/测试面；实现单点 = [`Self::cancel_snapshot`]，避免两份判定）
    pub fn slot_snapshot(&self, session_id: &str) -> Option<(String, String, u64)> {
        self.cancel_snapshot(session_id)
            .map(|s| (s.content, s.agent_type, s.elapsed_ms))
    }
}

static REGISTRY: std::sync::LazyLock<TurnRegistry> =
    std::sync::LazyLock::new(TurnRegistry::default);

/// 进程级登记表句柄（**所有无头通道共用一份**：取消端点只认它）
pub fn registry() -> &'static TurnRegistry {
    &REGISTRY
}

/// Task 6 的取消句柄 → 本表的取消请求口（各通道 `run_turn` 逐尝试武装用）
pub fn cancel_fn_of(cfg: &RunnerCfg) -> CancelFn {
    let handle: CancelHandle = cfg.cancel_handle();
    Arc::new(move || handle.cancel())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 登记表语义（Task 8 既有行为的搬迁回归）：占位互斥 / 取消先到者生效 / 槽位快照
    #[test]
    fn registry_is_exclusive_and_cancel_is_first_wins() {
        let sid = "sess_turn_registry_probe";
        assert!(registry().begin(sid, TurnSlot::placeholder("codex", "hi".into())));
        assert!(
            !registry().begin(sid, TurnSlot::placeholder("codex", "hi".into())),
            "同会话第二次占位必须失败（不排队、不覆盖）"
        );
        assert!(registry().in_flight(sid));
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f2 = fired.clone();
        registry().arm(
            sid,
            Arc::new(move || {
                f2.store(true, std::sync::atomic::Ordering::SeqCst);
                true
            }),
        );
        assert_eq!(
            registry().request_cancel(sid),
            Some(true),
            "在飞回合取消送达"
        );
        assert!(fired.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            registry().slot_snapshot(sid).map(|(c, t, _)| (c, t)),
            Some(("hi".to_string(), "codex".to_string())),
            "槽位快照供取消审计行取正文/工具"
        );
        registry().end(sid);
        assert!(!registry().in_flight(sid));
        assert_eq!(
            registry().request_cancel(sid),
            None,
            "无在飞回合 → None（取消端点据此不落审计行）"
        );
    }

    /// 审计 result 词（回执 → 词表）与证据头口径
    #[test]
    fn audit_words_and_evidence_head_are_pinned() {
        assert_eq!(receipt_result_word(&Receipt::ok("s", 1)), "ok");
        assert_eq!(
            receipt_result_word(&Receipt::failed(super::super::receipt::Stage::Crash, "x")),
            "failed(crash)"
        );
        let lines: Vec<String> = (0..40).map(|i| format!("l{i}")).collect();
        let head = head_of(&lines);
        assert_eq!(head.lines().count(), EVIDENCE_HEAD_LINES);
        assert!(head.starts_with("l0") && head.ends_with("l31"));
    }

    // ===== 串行锁守卫（评审 I14）=====

    /// **守卫在 panic 展开时也必须注销槽位**（评审 I14 的核心承诺）：手工 `begin`/`end`
    /// 配对的年代，分派臂里任何一处 panic 都会把槽位永久留在表里——该会话此后每次无头发送
    /// 都被判「已有在飞回合」直到 MAM 重启（`TurnRegistry` 无 TTL）。Drop 守卫把这条路径
    /// 收进类型。
    ///
    /// 变异验证：把 `TurnGuard` 的 `Drop` 体删掉 → 本用例第二条断言红。
    #[test]
    fn guard_releases_the_slot_on_panic_unwind() {
        let reg = TurnRegistry::default();
        let sid = "sess_i14_panic";
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = reg
                .begin_guarded(sid, TurnSlot::placeholder("zcode", "hi".into()))
                .expect("空闲会话必须占到槽位");
            assert!(reg.in_flight(sid), "守卫存活期间槽位在册");
            panic!("模拟分派臂 panic（结果未知）");
        }));
        assert!(panicked.is_err(), "夹具前提：闭包确实 panic 了");
        assert!(
            !reg.in_flight(sid),
            "panic 展开必须注销槽位（否则该会话永久「已有在飞回合」）"
        );
        let again = reg
            .begin_guarded(sid, TurnSlot::placeholder("zcode", "hi".into()))
            .expect("槽位真释放——同会话可重新占位（不是残留死锁）");
        drop(again);
    }

    /// **早退（early-return）也必须注销**：分派臂里「占位后某步失败 ⇒ return 拒绝」的路径
    /// 最多——守卫随作用域收尾 Drop，不需要每条 return 记得写 `end`。
    ///
    /// 变异验证：把守卫换成 `std::mem::forget`（等价于漏 `end`）→ 本用例第二条断言红。
    #[test]
    fn guard_releases_the_slot_on_early_return() {
        /// 模拟一条「占位后失败即 return」的分派臂
        fn dispatch_arm(reg: &TurnRegistry, sid: &str, fail: bool) -> Result<(), &'static str> {
            let _g = reg
                .begin_guarded(sid, TurnSlot::placeholder("zcode", "hi".into()))
                .ok_or("已有在飞回合")?;
            if fail {
                return Err("安装形态不可达"); // 早退：不写任何 end
            }
            Ok(())
        }
        let reg = TurnRegistry::default();
        let sid = "sess_i14_early";
        assert_eq!(dispatch_arm(&reg, sid, true), Err("安装形态不可达"));
        assert!(!reg.in_flight(sid), "早退必须注销槽位（守卫随作用域 Drop）");
        assert_eq!(dispatch_arm(&reg, sid, false), Ok(()));
        assert!(!reg.in_flight(sid), "正常返回同样注销");
    }

    /// **拒占路径不碰锁**（评审 I14 点名必须保持的既有语义）：`begin_guarded` 失败返回
    /// `None` ⇒ 调用方**拿不到守卫** ⇒ 结构上无从注销**别人**的槽位。
    ///
    /// 变异验证：把失败臂改成「先 `retire` 再返回 None」（模拟旧代码里按会话号误删）→
    /// 本用例第三条断言红。
    #[test]
    fn guarded_begin_refusal_never_touches_the_occupied_slot() {
        let reg = TurnRegistry::default();
        let sid = "sess_i14_refuse";
        let first = reg
            .begin_guarded(sid, TurnSlot::placeholder("zcode", "first".into()))
            .expect("第一次占位");
        assert!(
            reg.begin_guarded(sid, TurnSlot::placeholder("zcode", "second".into()))
                .is_none(),
            "同会话第二次占位必须如实拒绝（不排队、不覆盖）"
        );
        assert!(reg.in_flight(sid), "拒绝不得顺手注销在飞的那一发");
        assert_eq!(
            reg.slot_snapshot(sid).map(|(c, _, _)| c),
            Some("first".to_string()),
            "在飞内容不得被覆盖"
        );
        drop(first);
        assert!(!reg.in_flight(sid));
    }

    // ===== R3：cmd 垫片正文拒绝（票面三条 + stdin 例外面）=====

    /// 票面第一条：**垫片 + 元字符 ⇒ 拒绝**（逐个元字符都拒；`%` 是 Task 9 点名的那个）。
    #[test]
    fn cmd_shim_body_with_metachars_is_refused() {
        let shim = spawn_shape(r"C:\Users\u\AppData\Roaming\npm\codex.cmd", "windows");
        assert!(shim.via_cmd_shim(), "`.cmd` 在 Windows 上必须判为垫片形态");
        for c in CMD_SHIM_METACHARS {
            let text = format!("列出目录 {c} 后面");
            let reason = cmd_shim_body_refusal(&text, &shim, true)
                .unwrap_or_else(|| panic!("元字符 {c:?} 在垫片路径上必须拒发"));
            assert!(
                reason.contains(&c.to_string()) && reason.contains("cmd"),
                "拒绝文案必须点名是 cmd 重解析 + 具体元字符（显式告知）: {reason}"
            );
            assert!(
                reason.contains("未投递"),
                "必须说清零字节投递（拒绝 ≠ 尝试后失败）: {reason}"
            );
        }
        // 票据点名的 `%VAR%` 形态（变量展开面）
        assert!(cmd_shim_body_refusal("echo %PATH%", &shim, true).is_some());
        // `.bat` 同判（两种批处理后缀都是 cmd 垫片）
        let bat = spawn_shape(r"C:\npm\codex.bat", "windows");
        assert!(cmd_shim_body_refusal("a & b", &bat, true).is_some());
    }

    /// **评审 I8 追补：文案必须点名实际命中的那一段**——判定同一条，主语按分段身份取
    /// （`CmdShimSubject::label` 单一词表）。项目路径命中时说「正文含…」是**错报对象**：
    /// 用户会去改正文，而真凶在项目路径上，改多少遍都没用。
    ///
    /// 变异验证：让 `cmd_shim_refusal` 忽略 `subject` 恒用「正文」→ 本用例第二条断言红。
    #[test]
    fn cmd_shim_refusal_names_the_actual_offending_segment() {
        let shim = spawn_shape(r"C:\npm\codex.cmd", "windows");
        let body =
            cmd_shim_refusal(CmdShimSubject::Body, "a & b", &shim, true).expect("正文命中必拒");
        assert!(
            body.starts_with("TURN_CMD_SHIM_UNSAFE: 正文含 cmd 元字符"),
            "{body}"
        );
        let path = cmd_shim_refusal(CmdShimSubject::ProjectPath, "E:/A&B", &shim, true)
            .expect("项目路径命中必拒");
        assert!(
            path.starts_with("TURN_CMD_SHIM_UNSAFE: 项目路径（-C 取值）含 cmd 元字符"),
            "{path}"
        );
        let seg = cmd_shim_refusal(CmdShimSubject::ArgvSegment, "hi %PATH%", &shim, true)
            .expect("argv 分段命中必拒");
        assert!(
            seg.starts_with("TURN_CMD_SHIM_UNSAFE: 命令行分段含 cmd 元字符"),
            "{seg}"
        );
        // 三个主语各自只出现自己那一段（词表单点，不串味）
        for (reason, mine, others) in [
            (&body, "正文", ["项目路径", "命令行分段"]),
            (&path, "项目路径", ["正文含", "命令行分段"]),
            (&seg, "命令行分段", ["正文含", "项目路径"]),
        ] {
            assert!(reason.contains(mine), "{reason}");
            for o in others {
                assert!(!reason.contains(o), "不得串到别段（{o}）: {reason}");
            }
        }
        // 判定门与主语无关：形态不成立/不进 argv ⇒ 三个主语都放行
        for subject in [
            CmdShimSubject::Body,
            CmdShimSubject::ProjectPath,
            CmdShimSubject::ArgvSegment,
        ] {
            assert_eq!(cmd_shim_refusal(subject, "a & b", &shim, false), None);
            let direct = spawn_shape("C:/bin/codex.exe", "windows");
            assert_eq!(cmd_shim_refusal(subject, "a & b", &direct, true), None);
        }
    }

    /// 票面第二条：**垫片 + 普通正文 ⇒ 放行**（判据不得扩大成「垫片一律拒」）。
    #[test]
    fn cmd_shim_body_with_ordinary_text_is_allowed() {
        let shim = spawn_shape(r"C:\npm\codex.cmd", "windows");
        for text in [
            "hi",
            "帮我看看这个函数：fn main() {}",
            "解释一下 ls -la 的输出",
            "多行\n正文\n也可以",
        ] {
            assert_eq!(
                cmd_shim_body_refusal(text, &shim, true),
                None,
                "普通正文必须放行: {text:?}"
            );
        }
    }

    /// 票面第三条：**`.exe` 直装 + 元字符 ⇒ 放行**（直 spawn 无 cmd 重解析面）。
    #[test]
    fn exe_direct_body_with_metachars_is_allowed() {
        let direct = spawn_shape(r"C:\bin\codex.exe", "windows");
        assert!(!direct.via_cmd_shim());
        assert_eq!(
            cmd_shim_body_refusal("a & b %PATH% \"x\"", &direct, true),
            None
        );
        // 非 Windows 上 `.cmd` 也不经 cmd（无 cmd 可转）⇒ 形态直 spawn ⇒ 放行
        let posix = spawn_shape("codex.cmd", "macos");
        assert_eq!(cmd_shim_body_refusal("a & b", &posix, true), None);
    }

    /// **例外面**：正文**走 stdin** 的通道（claude）即使形态是垫片也放行——
    /// 正文根本不进命令行，垫片重解析面不存在（票面：claude 不受影响，别顺手拒了它）。
    #[test]
    fn stdin_body_with_metachars_is_allowed_even_on_a_shim() {
        let shim = spawn_shape(r"C:\npm\claude.cmd", "windows");
        assert!(shim.via_cmd_shim(), "前提：claude 在本机也是垫片形态");
        assert_eq!(cmd_shim_body_refusal("a & b %PATH%", &shim, false), None);
    }

    // ===== R12-S2 / 裁决 24b：花名 cmd 安全白名单（签名面收口）=====

    /// 票面：危险花名**逐个字符**都拒（`&` / `%` / `"` 三个点名 + 其余 ASCII 标点），
    /// 且文案**点名具体字符** + 说清「零字节投递」+ 给出路（改名）。
    #[test]
    fn device_name_with_cmd_metachars_is_refused() {
        for c in [
            '&', '%', '"', '|', '^', '<', '>', '!', '(', ')', '\\', '\'', '`', '$', ';',
        ] {
            let name = format!("小明{c}的手机");
            let reason = device_name_refusal(&name)
                .unwrap_or_else(|| panic!("危险花名 {name:?} 必须拒（{c:?} 在 cmd 里有语义）"));
            assert!(
                reason.contains(&device_name_char_label(c)) && reason.contains("cmd"),
                "拒绝文案必须点名具体字符 + 说清 cmd 重解析面: {reason}"
            );
            assert!(
                reason.contains("未投递"),
                "必须说清零字节投递（拒绝 ≠ 尝试后失败）: {reason}"
            );
            assert!(
                reason.contains("改名"),
                "必须给出路（花名是注册期的值，改一次名即可）: {reason}"
            );
        }
        // 控制字符：换行会被归一成字面 `\n`（两个字符），但反斜杠本身就不是白名单字符；
        // 裸换行/ESC/DEL 同拒，且**展示形态打转义**（不把裸控制字符打进用户文案）
        let nl = device_name_refusal("iPhone\n15").expect("裸换行花名必须拒");
        assert!(nl.contains("\\u{A}"), "控制字符必须打转义展示: {nl}");
        assert!(device_name_refusal("a\u{7f}b").is_some(), "DEL 同拒");
        // emoji（非字母数字、非白名单标点）同拒
        assert!(
            device_name_refusal("小明的手机📱").is_some(),
            "emoji 不在白名单"
        );
    }

    /// 票面：**中文/正常花名必须放行**（白名单是字符类判据，不是 ASCII 白名单）——
    /// 「中文花名要能过」是本判据最容易误伤的一面，单独钉住。
    #[test]
    fn ordinary_chinese_and_english_device_names_are_allowed() {
        for name in [
            "小明的手机",
            "测试设备",
            "iPhone",
            "iPad Pro",
            "Pixel 9",
            "mate-60_pro",
            "设备·二号",
            "新设备",
            "Galaxy S24 Ultra 1",
        ] {
            assert_eq!(
                device_name_refusal(name),
                None,
                "正常花名必须放行（白名单 = Unicode 字母数字 + 空格 - _ . ·）: {name:?}"
            );
        }
        // 空花名不在本判据职责内（注册点另行回落默认名，见 persist_and_cookie）
        assert_eq!(device_name_refusal(""), None);
    }
}
