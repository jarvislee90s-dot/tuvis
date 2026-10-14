#![cfg(windows)]
//! Windows ConPTY 注入通道（M9R 执行层重写，M6R–M9R 批次 Task 3）：`AttachConsole`
//! → 打开 `CONIN$` → `WriteConsoleInput` 写键事件。两处 M6 探测实证仍有效（落地照做）：
//! 1. AttachConsole 后 `GetStdHandle` 返回旧控制台陈旧句柄（stdin 为管道时必败）
//!    ——必须以 `CONIN$` 显式打开；
//! 2. `INPUT_RECORD` 必须拍平为 blittable 布局（嵌套结构体在 FFI 封送报
//!    0x8007007A）——本文件自定 20 字节平铺结构指针直传，与 ConIn.ps1 的
//!    C# 平铺完全同构。
//!
//! ## M9R 执行纪律（本文件落实项，证据 `research/refs/phase2-消息注入/`）
//! - **P1-2 进程级互斥**：控制台附加态进程全局唯一，[`CONSOLE_OP`] 串行化
//!   「定位 → 附加 → 写入 → 复位」全程单临界区——公共入口 [`inject_text_spec`] /
//!   [`inject_key_spec`] 各取一次锁；[`resolve_target`] 与 [`inject_via`] 为
//!   **无锁内部版**（调用方已持锁），禁止嵌套取锁（std Mutex 不可重入，嵌套即死锁）；
//! - **P1-1 RAII 复位**：[`AttachGuard`] Drop → `FreeConsole` 无条件复位，任何
//!   错误/panic 早退路径都兜底——滞留附加列表的进程在宿主终端窗口关闭时会收到
//!   CTRL_CLOSE_EVENT，本应用不装 SetConsoleCtrlHandler，默认行为是**被系统终止**
//!   （用户关一个收过消息的终端 = 兔维斯 整个应用跟着退出，故复位不可省，计划明文）；
//! - **自适应节流（§8.1）**：按族规格（`super::families`，M6R 探测定案表）分流——
//!   快消费者 80 字符/块（160 事件单次写入）、块间 50ms；慢消费者/长文切背压，
//!   每块后排空目标输入缓冲占用至 ≤ DRAIN_TO(40) 再续写（15ms 轮询）；
//! - **P2-1 真总预算**：正文 / 提交回车 / 122 重试 / 背压轮询共享一个 deadline
//!   （`families::inject_budget_ms`，背压按斜率放宽），每轮循环顶部检查，耗尽即
//!   中文报错（可重试语义）；
//! - **部分写续写（M6R 纪律）**：`WriteConsoleInput` 实写 < 分片长 → 从偏移续写
//!   （续写不睡眠）；122（ERROR_INSUFFICIENT_BUFFER）现语义 = 目标读武装期缓冲
//!   暂满、可重试（M6「消费墙首读预算 13~43 字符」叙事已被 M6R 推翻）→ 100ms
//!   重试同块；错误码仅在 Err 时读取（F7：ok=true 时错误码为噪声）；
//! - **P2-2 键域校验**：域外键在取锁/附加之前快速失败并报错，**不回退文本+回车**；
//! - **Task 4 占用监控 + 判冻回执（§8.1「背压与异常」行 / M6R F1 / D8 裁决：
//!   不自动介入）**：[`paced_write`] 在块写入后 / 背压轮询处 / 122·Ok(0) 重试
//!   睡眠后采样目标输入缓冲占用——`pending>0` 且**相邻采样无下降**（逐样本
//!   口径，质量评审 C1 定案）持续 ≥ `families::OCC_ABNORMAL_MS`(5s) 判冻结
//!   （宿主选择模式：写入成功 TUI 读不到）→ 本轮注入立即 Err，报中文状态回执
//!   [`FREEZE_ERR`]（引导用户点一下终端窗口手动恢复后重试；旧「自动解冻」自愈
//!   方案已撤除——评审/台账已裁决留痕，OccWatch 保留作诊断与错误归因）；
//! - **Task 4 屏读层（CONOUT$）**：[`read_input_tail`] 读输入行尾部（直发滞留
//!   回查与 Task 5 插队草稿确认用），与注入共用 [`CONSOLE_OP`] 单临界区；
//! - **实机验证归属**：分块/预算/背压/续写/占用监控/判冻回执/屏读的实机行为验证
//!   归 Task 12 `#[ignore]`（本任务门禁只锁编译 + 单测 + 既有 FFI 契约测试）。

use std::thread::sleep;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use windows::core::Error as WinError;
use windows::core::HRESULT;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, GENERIC_READ, GENERIC_WRITE, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::Console::{
    AttachConsole, FreeConsole, GetConsoleScreenBufferInfo, GetNumberOfConsoleInputEvents,
    ReadConsoleOutputAttribute, ReadConsoleOutputCharacterW, WriteConsoleInputW,
    CONSOLE_SCREEN_BUFFER_INFO, COORD, INPUT_RECORD, KEY_EVENT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, VkKeyScanW, MAPVK_VK_TO_VSC};

use super::engine::{
    control_records, enter_records, text_records, vk_arrow_records, vt_seq_records, KeyLayout,
    KeyRecordSpec,
};
use super::families::{self, FamilySpec, TuiFamily};

/// 122 重试间隔（§8.1：122 走 100ms 重试同块，受 deadline 约束）
const RETRY_GAP: Duration = Duration::from_millis(100);
/// 背压轮询间隔（§8.1：排空目标输入缓冲的 15ms 轮询步距，受 deadline 约束）
const DRAIN_POLL_GAP: Duration = Duration::from_millis(15);
/// 半成功防重警示后缀（I1，正文判冻回执与回车块错误共用）：前缀内容已进输入行、
/// 提交未完成，盲目重试会重复正文——对齐 macOS tmux「文本已入 pane」先例。
const PARTIAL_WARN: &str = "；正文可能已写入输入行，重试将重复——建议先人工确认终端";
/// 判冻状态回执（§8.1/§8.3；D8 裁决：不自动介入——判冻即终止本轮注入，引导
/// 用户在电脑上点一下终端窗口手动恢复，点击即清除选择模式）
const FREEZE_ERR: &str = "目标终端疑似进入选择模式或暂停读取输入（输入积压）。请在电脑上点一下该终端窗口后重试（点击即清除选择模式），或稍后重发";

/// P1-2：进程级注入互斥。控制台「附加态」是进程全局唯一的资源，任何并发注入
/// （多会话同时 flush）都会互踩 attach/detach；此处串行化全部
/// 「定位 → 附加 → 写入 → 复位」临界区。锁纪律：公共入口各取一次锁；
/// `resolve_target` / `inject_via` 为无锁内部版，严禁嵌套取锁。
///
/// 最坏持锁时长 ≈ `families::inject_budget_ms`（背压按 45ms/字符斜率放宽，
/// 万字符最高 ≈460s）；等待方的 deadline 在**取锁之后**才起算——排队等待不计入
/// 自身预算，只会排队而不会误报超时。Task 4 落地：`read_input_tail`（屏读层）
/// 与注入共享此临界区（附加态的读侧同样互斥，杜绝读屏与写入互踩附加态/
/// 交错改读同一输入行）。
static CONSOLE_OP: Lazy<std::sync::Mutex<()>> = Lazy::new(|| std::sync::Mutex::new(()));

/// P1-1：附加态 RAII 守卫——构造前调用方已 `AttachConsole` 成功；Drop 时无条件
/// `FreeConsole` 复位。任何错误/panic 早退路径都经过 Drop（旧实现手工收尾，
/// `?` 早退/panic 路径会漏出滞留附加态 → CTRL_CLOSE_EVENT 连带终止风险）。
struct AttachGuard;
impl Drop for AttachGuard {
    fn drop(&mut self) {
        // SAFETY: FFI 调用；FreeConsole 幂等（未附加时也安全，M6 探测实证）
        unsafe {
            let _ = FreeConsole();
        }
    }
}

/// 注入结果统计（跨任务接口契约，Task 4/5 消费）。可见性为 `pub` 仅因
/// `super::e2e_support`（M9R Task 12 实机 E2E 测试支撑面）的 `pub use` re-export
/// 对 `pub(crate)` 项不合法（E0365）——非公开 API 承诺：生产消费方不变（engine /
/// confirm）， crate 内调用一律走 `Injector` 缝与旧薄壳。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InjectStats {
    /// 成功送达的正文字符数（`text.chars().count()`；Ok 即全量送达，不含提交回车）
    pub written: usize,
    /// 本次注入是否走背压节流（§8.1 自适应：慢消费者或长文）
    pub backpressure: bool,
}

/// [`paced_write`] 结果（内部类型，Task 4）：`written` = 实写事件总数（含部分写
/// 续写累计）。判冻不经返回值表达——判冻即 Err 中文状态回执（D8 裁决：不自动
/// 介入；Task 12 E2E 只断言 backpressure，无解冻旗标消费面，字段随之撤除）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PacedOutcome {
    /// 成功写入的事件总数（含部分写续写累计）
    pub written: usize,
}

/// 平铺 KEY_EVENT `INPUT_RECORD`（blittable 20 字节）：EventType u16 + Reserved u16 +
/// bKeyDown i32(BOOL) + wRepeatCount u16 + wVirtualKeyCode u16 + wVirtualScanCode u16 +
/// UnicodeChar u16 + dwControlKeyState u32。
/// M6 教训：windows-rs 的嵌套 union 形态在 FFI 封送报 0x8007007A，必须平铺直传。
#[repr(C)]
struct FlatKeyRecord {
    event_type: u16,
    /// union 对齐填充位（保留 0）
    _reserved: u16,
    /// BOOL
    key_down: i32,
    repeat_count: u16,
    virtual_key_code: u16,
    virtual_scan_code: u16,
    unicode_char: u16,
    control_key_state: u32,
}
const _: () = assert!(std::mem::size_of::<FlatKeyRecord>() == 20);
const _: () = assert!(
    std::mem::size_of::<FlatKeyRecord>() == std::mem::size_of::<INPUT_RECORD>()
        && std::mem::align_of::<FlatKeyRecord>() == std::mem::align_of::<INPUT_RECORD>()
);

impl From<&KeyRecordSpec> for FlatKeyRecord {
    fn from(spec: &KeyRecordSpec) -> Self {
        Self {
            event_type: KEY_EVENT as u16, // 键事件类型 = 1
            _reserved: 0,
            key_down: i32::from(spec.down),
            repeat_count: 1,
            virtual_key_code: spec.vk,
            // M9R：扫描码随规格下发（VK 形态事件必带，B 族 crossterm 以 VK+scan 为准）
            virtual_scan_code: spec.scan,
            unicode_char: spec.ch,
            // T6：修饰位仅在 shift+tab 组合键时置位。该组合的唯一形态是
            // `ch == 0 ∧ vk == VK_TAB`（[`crate::inject::engine::shift_tab_records`]）；
            // 单字符/控制键/方向键记录一律 0，与既有行为逐字节一致。见 SHIFT_PRESSED
            control_key_state: if spec.ch == 0 && spec.vk == 0x09 {
                SHIFT_PRESSED
            } else {
                0
            },
        }
    }
}

/// SHIFT_PRESSED 修饰位（Win32 `dwControlKeyState` 常量；批次丙 T6 shift+tab 用）
const SHIFT_PRESSED: u32 = 0x0010;

/// 真实键位布局（M9R，Windows FFI，供调用点迁移；执行层大重写归 Task 3）。
/// 依据 M6R §8.1 定案：vk = VkKeyScanW(ch) & 0xFF，scan = MapVirtualKeyW(vk)。
pub(crate) struct WinKeyLayout;

impl KeyLayout for WinKeyLayout {
    /// 字符 → 虚拟键码：`'\r'` 直返 VK_RETURN（控制字符的 VkKeyScanW 语义不可靠，
    /// 回车 VK 形态三家统一）；非 ASCII → 0（纯字符流，与现役 ConIn.ps1 口径
    /// 一致）；ASCII → `VkKeyScanW`：返回 i16，-1（不可键入，如部分控制字符）
    /// → 0 走字符流，否则低字节为 VK（高字节为 shift/Ctrl/Alt 修饰位，M9R 事件
    /// 不带修饰态，丢弃）。
    fn vk_of(&self, c: char) -> u16 {
        if c == '\r' {
            return 0x0D; // VK_RETURN
        }
        if !c.is_ascii() {
            return 0;
        }
        // SAFETY: FFI 调用，无特殊前置条件
        let ret = unsafe { VkKeyScanW(c as u16) };
        if ret == -1 {
            0
        } else {
            (ret & 0xFF) as u16
        }
    }

    /// 虚拟键码 → 扫描码（`MAPVK_VK_TO_VSC`；无映射返回 0）。
    fn scan_of(&self, vk: u16) -> u16 {
        // SAFETY: FFI 调用，无特殊前置条件
        unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) as u16 }
    }
}

/// 附加到目标进程控制台：`FreeConsole`（幂等，未附加时也安全，M6 探测实证）
/// → `AttachConsole`。失败返回错误码（HRESULT 原值），由调用方决定是否回退祖先链。
fn attach(pid: u32) -> Result<(), u32> {
    let _ = unsafe { FreeConsole() };
    if let Err(e) = unsafe { AttachConsole(pid) } {
        log::warn!("AttachConsole(pid={pid}) 失败：0x{:08X}", e.code().0 as u32);
        return Err(e.code().0 as u32);
    }
    Ok(())
}

/// 打开目标控制台输入缓冲（`CONIN$`）。M6 修正：不能用 `GetStdHandle`
/// （附加后返回旧控制台陈旧句柄，stdin 为管道时必败）。
/// 需要读+写权（WriteConsoleInput 要求）；共享读写以兼容目标进程自身访问。
///
/// # SAFETY
/// FFI 调用：成功句柄由调用方负责 `CloseHandle` 收尾。
unsafe fn open_conin() -> Result<HANDLE, String> {
    CreateFileW(
        windows::core::w!("CONIN$"),
        GENERIC_READ.0 | GENERIC_WRITE.0,
        FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0),
        None,
        OPEN_EXISTING,
        FILE_FLAGS_AND_ATTRIBUTES(0),
        None,
    )
    .map_err(|e| format!("打开 CONIN$ 失败（0x{:08X}）", e.code().0 as u32))
}

/// 打开目标控制台屏幕缓冲（`CONOUT$`，读+写权，Task 4 屏读层）。读权供
/// `ReadConsoleOutputCharacterW`；共享读写以兼容目标进程自身访问。
///
/// # SAFETY
/// FFI 调用：成功句柄由调用方负责 `CloseHandle` 收尾；须处于目标控制台附加态。
unsafe fn open_conout() -> Result<HANDLE, String> {
    CreateFileW(
        windows::core::w!("CONOUT$"),
        GENERIC_READ.0 | GENERIC_WRITE.0,
        FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0),
        None,
        OPEN_EXISTING,
        FILE_FLAGS_AND_ATTRIBUTES(0),
        None,
    )
    .map_err(|e| format!("打开 CONOUT$ 失败（0x{:08X}）", e.code().0 as u32))
}

/// 单片写入：平铺结构切片按 `INPUT_RECORD` 同构布局指针直传（上方编译期断言锁定
/// 尺寸/对齐一致；windows-rs `WriteConsoleInputW` 内部对切片指针仅 transmute 直通，
/// 无嵌套封马 → 不踩 0x8007007A）。返回 windows Error 以便按错误码分流
/// （122 = 目标读武装期缓冲暂满，可重试）。**返回实写事件数**（M6R 纪律：
/// 实写 < 分片长时调用方从偏移续写）。
///
/// # SAFETY
/// FFI 调用：`handle` 须为有效 CONIN$ 句柄；切片生命周期覆盖调用全程。
unsafe fn write_chunk(handle: HANDLE, chunk: &[FlatKeyRecord]) -> Result<u32, WinError> {
    let records = std::slice::from_raw_parts(chunk.as_ptr() as *const INPUT_RECORD, chunk.len());
    let mut written = 0u32;
    WriteConsoleInputW(handle, records, &mut written).map(|_| written)
}

/// 122 判定：ERROR_INSUFFICIENT_BUFFER(122)（HRESULT 形态 0x8007007A）。
/// 仅在 `write_chunk` 返回 Err 时读错误码（M6R F7：ok=true 时错误码为噪声）。
fn is_insufficient_buffer(e: &WinError) -> bool {
    e.code() == HRESULT::from_win32(ERROR_INSUFFICIENT_BUFFER.0)
}

/// 查询目标输入缓冲占用（未消费事件数）。背压排空（Task 3）与占用监控判冻
/// （Task 4）共用同一查询；两者语义分离：`families::DRAIN_TO` 是背压回落阈值
/// （写停到占用 ≤40 续写，量「落到多少续写」），`families::OCC_ABNORMAL_MS` 是
/// 判冻窗口时长（pending>0 且逐样本无下降持续 ≥5s 才判异常，量「停多久」），
/// 互不复用。
fn query_pending(handle: HANDLE) -> Result<u32, String> {
    let mut pending = 0u32;
    // SAFETY: FFI 调用；handle 为本调用链刚打开的有效 CONIN$ 句柄
    unsafe { GetNumberOfConsoleInputEvents(handle, &mut pending) }.map_err(|e| {
        format!(
            "GetNumberOfConsoleInputEvents 失败（0x{:08X}）",
            e.code().0 as u32
        )
    })?;
    Ok(pending)
}

/// 占用监控判冻窗口（Task 4，§8.1「背压与异常」行）：**判冻条件 = pending>0
/// 且相邻两次采样无下降，连续 ≥ `families::OCC_ABNORMAL_MS`(5s)**——与**上一
/// 采样点**比（逐样本口径，质量评审 C1 定案；旧「历史最低水位基线」口径在背压
/// 锯齿稳态下基线 ratchet 到谷底、pending 永不跌破 → 例行误判冻结，已废弃）。
/// 任何逐样本下降（哪怕 1 事件）= 消费者在消费 → 重置窗口（背压 drain 的 15ms
/// 轮询逐拍下降 → 窗口永不满 5s，健康/慢消费者均不误报）；冻结/选择模式 pending
/// 恒定才累计到阈值触发；`pending == 0` → 完全重置。与 `families::DRAIN_TO`
/// （Task 3 既有背压回落阈值）语义分离，见 [`query_pending`] 注。
struct OccWatch {
    /// 上一采样点占用（逐样本比较基准；每拍更新为当前值）
    last: u32,
    /// pending>0 且无逐样本下降的连续起点（None = 窗口未开/已重置）
    since: Option<Instant>,
}

impl OccWatch {
    fn new() -> Self {
        Self {
            last: 0,
            since: None,
        }
    }

    /// 用既有查询结果推进判冻窗口（[`drain_to`] 轮询处复用同一 pending，
    /// 不重复查询）。判冻触发 → 直接 Err 中文状态回执（[`FREEZE_ERR`]，D8 裁决：
    /// 不自动介入——OccWatch 保留作诊断与错误归因，判冻回执即其产出）。
    fn observe(&mut self, pending: u32) -> Result<(), String> {
        if pending == 0 {
            // 完全重置：无积压（since 与 last 双清）
            self.last = 0;
            self.since = None;
            return Ok(());
        }
        let prev = self.last;
        self.last = pending; // last 恒为上一采样点（逐样本口径，见结构体 doc）
        match self.since {
            None => {
                self.since = Some(Instant::now());
            }
            Some(t0) => {
                if pending < prev {
                    // 逐样本下降 = 消费者存活（哪怕只降 1 事件）→ 重置窗口。
                    // 等待期内无注入 → pending 单调不增，两次采样间的下降量
                    // 即该间隔内的真实消费量——不存在「假下降」。
                    self.since = Some(Instant::now());
                } else if t0.elapsed().as_millis() as u64 >= families::OCC_ABNORMAL_MS {
                    // 判冻（§8.1）：pending>0 且逐样本无下降持续 ≥5s → 本轮注入
                    // 立即终止（D8 裁决：不自动介入）。I1/N1（Task 4 复检）：回执
                    // 在此保持**裸** FREEZE_ERR——防重警示（PARTIAL_WARN）由调用点
                    // 按路径拼接（正文有正文、键路径无正文），源头拼接会让回车路径
                    // 双重警示
                    return Err(FREEZE_ERR.to_string());
                }
            }
        }
        Ok(())
    }

    /// 采样一次占用并推进判冻窗口（块写入后 / 122·Ok(0) 重试睡眠后的独立查询点；
    /// 纯查询不睡眠，不改变既有块间节奏）。
    fn sample(&mut self, handle: HANDLE) -> Result<(), String> {
        let pending = query_pending(handle)?;
        self.observe(pending)
    }
}

/// 背压排空（§8.1）：轮询目标输入缓冲占用直至 ≤ `families::DRAIN_TO`(40) 再继续；
/// 15ms 步距（[`DRAIN_POLL_GAP`]），同样受 `deadline` 约束（P2-1 真总预算）。
/// Task 4：每次轮询顺带喂 [`OccWatch`] 判冻监控（复用同一次占用查询，无额外
/// 系统调用；判冻触发即 Err 中文状态回执终止本轮——回执语义，非节奏改变）。
fn drain_to(handle: HANDLE, deadline: Instant, watch: &mut OccWatch) -> Result<(), String> {
    loop {
        let pending = query_pending(handle)?;
        watch.observe(pending)?;
        if pending <= families::DRAIN_TO {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("注入超时（总预算耗尽），请重试".into());
        }
        sleep(DRAIN_POLL_GAP);
    }
}

/// 分片写入主循环（M9R §8.1 配方，取代旧 M6 消费墙保守实现）：
/// - 每块 `families::CHUNK_CHARS * 2` 事件（80 字符 = 160 事件单次写入单元）；
/// - **每轮循环顶部查 deadline**（P2-1 真总预算：写入/回车/122 重试/背压轮询共享）
///   耗尽 → `Err("注入超时（总预算耗尽），请重试")`；
/// - M6R 纪律：实写 < 分片长 → **从偏移续写**（续写不睡眠）；`Ok(0)` 不应发生
///   （防御），按 122 同款可重试处理；
/// - Err(122) → 100ms 重试同块（受 deadline）；其他错误 → 带错误码中文错误；
/// - 非背压：块间 `families::CHUNK_GAP_MS`(50ms)；背压：每块后 [`drain_to`]；
/// - Task 4 占用监控：每块写入后 / 背压轮询处（[`drain_to`] 内复用查询）/
///   122·Ok(0) 重试睡眠后采样输入缓冲占用（[`OccWatch`]）——判冻触发 →
///   Err 中文状态回执（[`FREEZE_ERR`]；D8 裁决：不自动介入）。采样为纯查询
///   不睡眠，不改变既有块间节奏。
///
/// 返回 [`PacedOutcome`]（实写事件总数）。
fn paced_write(
    handle: HANDLE,
    records: &[KeyRecordSpec],
    backpressure: bool,
    deadline: Instant,
) -> Result<PacedOutcome, String> {
    let chunk_events = families::CHUNK_CHARS * 2; // 80 字符 = 160 事件（§8.1）
    let mut sent = 0usize; // 已实写事件数（部分写续写的游标）
    let mut watch = OccWatch::new();
    while sent < records.len() {
        // P2-1：真总预算检查（写入/重试/轮询全走这里，一处收口）
        if Instant::now() >= deadline {
            return Err("注入超时（总预算耗尽），请重试".into());
        }
        let end = (sent + chunk_events).min(records.len());
        let chunk: Vec<FlatKeyRecord> =
            records[sent..end].iter().map(FlatKeyRecord::from).collect();
        match unsafe { write_chunk(handle, &chunk) } {
            Ok(written) if written as usize >= chunk.len() => {
                sent = end;
                if backpressure {
                    // 背压：每块后排空目标输入缓冲至阈值再继续（含末块，§8.1）——
                    // 轮询处同步喂判冻监控（复用查询）
                    drain_to(handle, deadline, &mut watch)?;
                } else {
                    if sent < records.len() {
                        // 非背压：块间短睡眠，给目标 TUI 留输入缓冲消费窗口
                        sleep(Duration::from_millis(families::CHUNK_GAP_MS));
                    }
                    // 每块写入后采样判冻（Task 4，§8.1；含末块，纯查询不睡眠）
                    watch.sample(handle)?;
                }
            }
            Ok(written) => {
                if written == 0 {
                    // Ok(0) 不应发生（防御）：按 122 同款 100ms 重试同块，睡后采样
                    sleep(RETRY_GAP);
                    watch.sample(handle)?;
                } else {
                    // M6R 纪律：实写 < 分片长，从偏移续写（续写不睡眠）
                    sent += written as usize;
                }
            }
            Err(e) if is_insufficient_buffer(&e) => {
                // 122 现语义（M6R 推翻消费墙）：目标读武装期缓冲暂满，可重试——
                // 100ms 后重试同块（§8.1）；deadline 由循环顶部统一把守；
                // 睡后采样判冻（Task 4：重试等待期同步监控消费进展）
                sleep(RETRY_GAP);
                watch.sample(handle)?;
            }
            Err(e) => {
                return Err(format!(
                    "WriteConsoleInput 失败（0x{:08X}）",
                    e.code().0 as u32
                ));
            }
        }
    }
    Ok(PacedOutcome { written: sent })
}

/// 对 pid 完整注入一条写入闭包（M9R 重写，取代旧 records 直传版）。
/// **锁纪律：调用方须已持 [`CONSOLE_OP`] 锁**（本函数为无锁内部版，doc 即契约：
/// 公共入口各取一次锁、全程单临界区；本函数与 [`resolve_target`] 都不再取锁，
/// 杜绝 std Mutex 不可重入导致的嵌套死锁）。
/// 流程：resolve_target（锁内探测附加+祖先回退，每步失败即 FreeConsole 复位）
/// → attach(target) → [`AttachGuard`] → open_conin → 写入闭包 → CloseHandle
/// → guard Drop 复位。闭包在 guard 作用域内执行、不早退出；任何错误/panic
/// 路径由 guard Drop 兜底复位（P1-1，CTRL_CLOSE_EVENT 防连带终止）。
/// 泛型 `T`（Task 4 引入）：闭包可携带结果出临界区（现调用方 `T = ()`，泛型
/// 保留以不动注入链结构）。
fn inject_via<T>(pid: u32, write: impl FnOnce(HANDLE) -> Result<T, String>) -> Result<T, String> {
    let target = resolve_target(pid)?;
    attach(target).map_err(|code| format!("AttachConsole(pid={target}) 失败（0x{code:08X}）"))?;
    let _guard = AttachGuard;
    // SAFETY: FFI 调用；句柄生命周期收敛于本函数（下方无条件 CloseHandle）
    let handle = unsafe { open_conin() }?;
    let result = write(handle);
    // SAFETY: FFI 调用；handle 为本函数刚打开的内核句柄
    unsafe {
        let _ = CloseHandle(handle);
    }
    result
    // _guard 在此 Drop → FreeConsole 复位（无论成败；幂等，M6 探测实证）
}

/// CONOUT$ 屏读的临界区骨架（[`inject_via`] 的读侧同款）：锁 → [`resolve_target`]
/// → attach → [`AttachGuard`] → `open_conout` → 读闭包 → CloseHandle → Drop 复位。
/// [`read_input_tail`] 与 [`read_screen_window`] 共用——读侧同样独占附加态
/// （Task 4），两处各写一遍即锁纪律漂移面。
fn read_via<T>(pid: u32, read: impl FnOnce(HANDLE) -> Result<T, String>) -> Result<T, String> {
    let _lock = CONSOLE_OP.lock().unwrap_or_else(|e| e.into_inner());
    let target = resolve_target(pid)?;
    attach(target).map_err(|code| format!("AttachConsole(pid={target}) 失败（0x{code:08X}）"))?;
    let _guard = AttachGuard;
    // SAFETY: FFI 调用；句柄生命周期收敛于本函数（下方无条件 CloseHandle）
    let handle = unsafe { open_conout() }?;
    let result = read(handle);
    // SAFETY: FFI 调用；handle 为本函数刚打开的内核句柄
    unsafe {
        let _ = CloseHandle(handle);
    }
    result
    // _guard 在此 Drop → FreeConsole 复位；_lock 在此释放
}

/// 文本注入（「打字 + 回车」铁则，M9R spec 感知版）：正文按 [`text_records`]
/// 分流构造（ASCII 走 VK 形态、非 ASCII 走 vk=0 字符流）+ 尾部 VK 形态回车
/// 事件对（固定 [`families::SUBMIT_DELAY_MS`] 后单批提交）。自适应节流与真总
/// 预算由族规格驱动（§8.1 / P2-1）。
/// `text` 已是 compose 后单行（含字面 \n 两字符场景也按字符事件直发）。
/// 可见性 `pub` 仅服务 `super::e2e_support` 测试支撑面（E0365 缘由见
/// [`InjectStats`] 注）；crate 内生产调用一律经 `Injector` 缝（engine.rs）。
pub fn inject_text_spec(pid: u32, text: &str, spec: &FamilySpec) -> Result<InjectStats, String> {
    // P1-2：进程级串行（附加态全局唯一）；毒锁就地恢复（前次 panic 不放大为死锁）
    let _lock = CONSOLE_OP.lock().unwrap_or_else(|e| e.into_inner());
    // 域内路径：锁内 resolve_target（探测附加+祖先回退全在锁内，杜绝并发附加竞态）
    let chars = text.chars().count();
    let bp = families::use_backpressure(spec, chars);
    // P2-1：真总预算（背压按斜率放宽）；取锁后起算（预算度量注入而非等锁）
    let deadline = Instant::now() + Duration::from_millis(families::inject_budget_ms(spec, chars));
    let layout = WinKeyLayout;
    let body = text_records(text, &layout);
    let enter = enter_records(&layout);
    inject_via(pid, move |handle| {
        // N1：正文路径防重警示在调用点拼接（正文已部分写入输入行 → 任何正文错误
        // 皆带防重提示；判冻回执源头为裸 FREEZE_ERR，杜绝双重拼接）
        paced_write(handle, &body, bp, deadline).map_err(|e| format!("{e}{PARTIAL_WARN}"))?;
        // 提交回车：正文写完固定延迟后单批发（回车不进 chunk 计划，§8.1）
        sleep(Duration::from_millis(families::SUBMIT_DELAY_MS));
        paced_write(handle, &enter, false, deadline).map_err(|e| {
            // 半成功防重（对齐 macOS tmux「文本已入 pane」先例）：正文已进输入行、
            // 回车未提交，盲目重试会重复正文——追加人工确认提示（PARTIAL_WARN
            // 与正文调用点共用拼接，I1；判冻回执源头裸 FREEZE_ERR，N1）
            format!("{e}{PARTIAL_WARN}")
        })?;
        Ok(())
    })?;
    Ok(InjectStats {
        written: chars, // Ok = 全量送达；written 记正文字符数（不含提交回车）
        backpressure: bp,
    })
}

/// 草稿注入（批次戊 E1②）：[`inject_text_spec`] 去**尾部提交回车**版——正文进
/// composer 成草稿即止。唯一消费者 = codex 插队键序「打字→Tab 入队→Esc 直插」
/// （[`super::mode::JumpSequence::DraftTabThenEsc`]）的第一步：回车会把草稿当场
/// 提交，Tab 才是 codex 的「入队」键。分块/背压/预算与 [`inject_text_spec`] 全同。
pub fn inject_text_draft_spec(
    pid: u32,
    text: &str,
    spec: &FamilySpec,
) -> Result<InjectStats, String> {
    let _lock = CONSOLE_OP.lock().unwrap_or_else(|e| e.into_inner());
    let chars = text.chars().count();
    let bp = families::use_backpressure(spec, chars);
    let deadline = Instant::now() + Duration::from_millis(families::inject_budget_ms(spec, chars));
    let layout = WinKeyLayout;
    let body = text_records(text, &layout);
    inject_via(pid, move |handle| {
        paced_write(handle, &body, bp, deadline).map_err(|e| format!("{e}{PARTIAL_WARN}"))
    })?;
    Ok(InjectStats {
        written: chars,
        backpressure: bp,
    })
}

/// A 族方向键 → VT 序列映射表（本地常量，M6R §8.1 定案：ESC [ + A/B/C/D 字母流，
/// vk=0 字符形态整条单批原子写）。
/// 探针派生脆弱常量——版本复验清单见 `super::families`（宪法横切 6 集中落点），
/// 勿就地改值。
const ARROW_VT_SEQS: [(&str, &str); 4] = [
    ("up", "\x1b[A"),
    ("down", "\x1b[B"),
    ("right", "\x1b[C"),
    ("left", "\x1b[D"),
];

/// 键名 → 键事件序列（域判定纯构造，P2-2）：控制键/单字符字母数字走
/// [`control_records`]；方向键按族分支——A 族 [`vt_seq_records`]（VT 字符流）/
/// B 族 [`vk_arrow_records`]（VK+scan）。域外 → `None`。
fn key_records_for(key: &str, spec: &FamilySpec) -> Option<Vec<KeyRecordSpec>> {
    // 批次丙 T6：shift+tab 组合键（模式切换主键）——不在 control_records 的既有
    // 键域（那是个纯 VK 域，不带修饰位），单独分支；族无关（三家 A 族实测共性，
    // B 族同样以 VK+修饰位表达组合键）
    if key == "shift+tab" {
        return Some(super::engine::shift_tab_records(&WinKeyLayout));
    }
    if let Some(records) = control_records(key, &WinKeyLayout) {
        return Some(records);
    }
    let vt = ARROW_VT_SEQS
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, seq)| *seq);
    match (vt, spec.family) {
        (Some(seq), TuiFamily::RawVt) => Some(vt_seq_records(seq)),
        (Some(_), TuiFamily::Crossterm) => vk_arrow_records(key, &WinKeyLayout),
        (None, _) => None, // 域外
    }
}

/// 单键注入（M9R spec 感知版）：键域内（enter/esc/tab/backspace/space/单字符字母数字/
/// 方向键/shift+tab）→ 按族形态单批写入；域外 → **快速失败报错，不回退文本+回车**
/// （P2-2 闭环）。域校验在取锁/附加之前（假 pid 也不触发任何控制台附加）。可见性
/// `pub` 仅服务 `super::e2e_support` 测试支撑面（缘由同 [`InjectStats`] 注）。
pub fn inject_key_spec(pid: u32, key: &str, spec: &FamilySpec) -> Result<(), String> {
    // 键黑名单（批次戊 E1⑤，裁19）：先于域校验——ctrl+c 无论域内域外一律拒绝
    // （opencode 按下即退出应用；单点拒绝防日后域扩展回归）。
    // 【#79 上提后本处是**纵深第二道**】：第一道已在 Injector 引擎入口
    // （engine.rs `locate_and_send_key` 默认实现，macOS 三通道同受检）；黑名单
    // 本体单点 = engine::forbidden_key_reason。此处保留防本文件被绕过直调。
    if let Some(reason) = super::engine::forbidden_key_reason(key) {
        return Err(reason);
    }
    // P2-2：域校验先行——必须在取锁/附加之前快速失败（不触任何控制台 API）
    let Some(records) = key_records_for(key, spec) else {
        return Err(format!(
            "不支持的按键：{key}（域：enter/esc/tab/backspace/space/shift+tab/单字符字母数字/方向键）"
        ));
    };
    let _lock = CONSOLE_OP.lock().unwrap_or_else(|e| e.into_inner());
    // 键事件 ≤ 8 条（方向键 VT 序列），无需放宽——固定基础预算（§8.1）
    let deadline = Instant::now() + Duration::from_millis(families::BASE_BUDGET_MS);
    inject_via(pid, move |handle| {
        paced_write(handle, &records, false, deadline)?;
        Ok(())
    })
}

/// 批量退格（2026-10-04 计划反馈通道）：`count` 个 VK_BACK 成对事件按小步进批量
/// 写入。退格是**纯删除键、无逐键重绘耦合**——不等 SUBMIT_DELAY（探测定案
/// 2026-10-04 §S4：claude 2.1.287 composer 上批量 backspace×30 全清实证，日志
/// `evidence/plan-feedback/probe-type-pf-20261004-165110.log`）。分块/背压/预算
/// 沿用长文本口径（事件对数 ≈ 字符数）。crate 内生产可见，唯一消费者 =
/// `remote::api` 的计划反馈编排。
pub(crate) fn inject_backspaces_spec(
    pid: u32,
    count: usize,
    spec: &FamilySpec,
) -> Result<(), String> {
    if count == 0 {
        return Ok(());
    }
    let scan = WinKeyLayout.scan_of(0x08u16); // VK_BACK
    let records: Vec<KeyRecordSpec> = (0..count)
        .flat_map(|_| super::engine::key_pair(0x08u16, scan, 0x08u16))
        .collect();
    let _lock = CONSOLE_OP.lock().unwrap_or_else(|e| e.into_inner());
    let chars = count; // 事件对数 ≈ 字符数（预算/背压同长文口径）
    let bp = families::use_backpressure(spec, chars);
    let deadline = Instant::now() + Duration::from_millis(families::inject_budget_ms(spec, chars));
    inject_via(pid, move |handle| {
        paced_write(handle, &records, bp, deadline)?;
        Ok(())
    })
}

/// 输入行尾部屏读（Task 4，M6R/§8.1：注入后读屏可判定「文本滞留输入行」）。
/// 契约签名 `read_input_tail(pid, n)`：CONOUT$ + `ReadConsoleOutputCharacterW`
/// 从光标处**同行向前**读 `min(n, cursor_x+1)` 个 UTF-16 unit，
/// `String::from_utf16_lossy` 返回。**不 trim**——截尾/匹配语义归调用方
/// （直发滞留回查与 Task 5 插队草稿确认用）。已知界限（Task 5 消费方需知）：
/// 「同行向前」以屏幕缓冲**视觉行**为准——输入行逻辑折行时只能读到光标所在的
/// 最后一视觉行，读不到上一视觉行的行首部分。
///
/// 锁纪律：全程与注入共用 [`CONSOLE_OP`] 单临界区（毒锁恢复同既有）——见
/// [`read_via`]（读侧临界区骨架）。
pub(crate) fn read_input_tail(pid: u32, n: usize) -> Result<String, String> {
    read_via(pid, |handle| read_tail_chars(handle, n))
}

/// 屏读实现体（[`read_input_tail`] 已开 CONOUT$ 句柄）：`GetConsoleScreenBufferInfo`
/// 取光标位置 → 同行向前读 `min(n, cursor_x+1)` 个 unit。
fn read_tail_chars(handle: HANDLE, n: usize) -> Result<String, String> {
    // M2：n==0 早退（避免空切片/零宽读取走 FFI 的歧义路径，语义恒空串）
    if n == 0 {
        return Ok(String::new());
    }
    let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
    // SAFETY: FFI 调用；info 为本函数栈上缓冲
    unsafe { GetConsoleScreenBufferInfo(handle, &mut info) }.map_err(|e| {
        format!(
            "GetConsoleScreenBufferInfo 失败（0x{:08X}）",
            e.code().0 as u32
        )
    })?;
    let cursor = info.dwCursorPosition;
    // 同行向前：起点 X = cursor.X - (want-1)，want 封顶 cursor.X+1（到行首）；
    // 再封顶 i16::MAX 防御（cursor.X 理论上界，缓冲宽度实际远小于此）
    let want = n.min(cursor.X.max(0) as usize + 1).min(i16::MAX as usize);
    let mut buf = vec![0u16; want];
    let mut read = 0u32;
    let start = COORD {
        X: cursor.X - (want as i16 - 1),
        Y: cursor.Y,
    };
    // SAFETY: FFI 调用；buf 长度即读取上限（windows-rs 绑定以切片长度为 nLength）
    unsafe { ReadConsoleOutputCharacterW(handle, &mut buf, start, &mut read) }.map_err(|e| {
        format!(
            "ReadConsoleOutputCharacterW 失败（0x{:08X}）",
            e.code().0 as u32
        )
    })?;
    buf.truncate(read as usize);
    Ok(String::from_utf16_lossy(&buf)) // 不 trim（契约：截尾/匹配语义归调用方）
}

/// 屏幕**可见窗口**读取（批次丙 T5：N 选项审批对话框屏读解析的数据源）。
///
/// 与 [`read_input_tail`] 的差异：后者只读光标所在的**同行**（输入行草稿判定用），
/// 本函数读**整个可见窗口区**（`srWindow` 的每一行）——审批/计划批准对话框是
/// 多行块（题干 + `1. xxx` / `2. xxx` 选项列表 + 提示行），只看输入行读不到。
///
/// 返回：按行拆分的字符串（行序 = 屏幕从上到下，**已 trim 行尾**——屏幕行宽用
/// 空格补齐，保留会让解析器把空行当内容；行内前导空白保留，缩进是对话框的层级
/// 信息）。行数上限 = 窗口高度（`srWindow` 高度，典型 ≤ 50）。
///
/// 已知界限（消费方需知）：① 只读**可见窗口**，滚出窗口的历史行读不到（对话框
/// 必然在可见区，可接受）；② 每行读 `srWindow.Right - srWindow.Left + 1` 个 unit
/// （窗口宽度），超宽内容被窗口裁掉（TUI 按窗口宽排版，实际不裁）；③ 与
/// [`read_input_tail`] 同款锁纪律（[`CONSOLE_OP`] 单临界区 + 附加态复位）。
///
/// 失败语义：任何 FFI 失败 → Err（调用方按「屏读失败」降级——T5 红线 3/4：屏读
/// 失败必须降级为二元卡 + 人工核对提示，不猜）。
///
/// 锁纪律：全程与注入共用 [`CONSOLE_OP`] 单临界区（毒锁恢复同既有）——见
/// [`read_via`]（读侧临界区骨架）。
pub(crate) fn read_screen_window(pid: u32) -> Result<Vec<String>, String> {
    read_via(pid, read_window_lines)
}

/// 可见窗口逐行读取实现体（[`read_screen_window`] 已开 CONOUT$ 句柄）。
fn read_window_lines(handle: HANDLE) -> Result<Vec<String>, String> {
    let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
    // SAFETY: FFI 调用；info 为本函数栈上缓冲
    unsafe { GetConsoleScreenBufferInfo(handle, &mut info) }.map_err(|e| {
        format!(
            "GetConsoleScreenBufferInfo 失败（0x{:08X}）",
            e.code().0 as u32
        )
    })?;
    let win = info.srWindow;
    let width = (win.Right - win.Left + 1).max(0) as usize;
    let height = (win.Bottom - win.Top + 1).max(0) as usize;
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }
    let mut lines = Vec::with_capacity(height);
    let mut buf = vec![0u16; width];
    for row in 0..height {
        let mut read = 0u32;
        let start = COORD {
            X: win.Left,
            Y: win.Top + row as i16,
        };
        // SAFETY: FFI 调用；buf 长度即读取上限（windows-rs 绑定以切片长度为 nLength）
        unsafe { ReadConsoleOutputCharacterW(handle, &mut buf, start, &mut read) }.map_err(
            |e| {
                format!(
                    "ReadConsoleOutputCharacterW 失败（0x{:08X}）",
                    e.code().0 as u32
                )
            },
        )?;
        let line = String::from_utf16_lossy(&buf[..read as usize]);
        lines.push(line.trim_end().to_string());
    }
    Ok(lines)
}

/// **字符属性级读取**（2026-10-06 粗体探针专用，#[ignore] 实机消费）：与
/// [`read_screen_window`] 同一视口定位，每行返回属性数组（`FOREGROUND_INTENSITY`
/// 等位——粗体/高亮在属性层，字符层不可见）。仅探针消费，生产不依赖。
#[cfg(windows)]
#[allow(dead_code)] // 粗体探针通道（2026-10-06 定案：属性层可读）——生产消费下批
pub(crate) fn read_screen_window_attrs(pid: u32) -> Result<Vec<Vec<u16>>, String> {
    read_via(pid, |handle| {
        let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
        // SAFETY: FFI 调用；info 为本函数栈上缓冲
        unsafe { GetConsoleScreenBufferInfo(handle, &mut info) }.map_err(|e| {
            format!(
                "GetConsoleScreenBufferInfo 失败（0x{:08X}）",
                e.code().0 as u32
            )
        })?;
        let win = info.srWindow;
        let width = (win.Right - win.Left + 1).max(0) as usize;
        let height = (win.Bottom - win.Top + 1).max(0) as usize;
        let mut rows = Vec::with_capacity(height);
        for row in 0..height {
            let mut buf = vec![0u16; width];
            let mut read = 0u32;
            let start = COORD {
                X: win.Left,
                Y: win.Top + row as i16,
            };
            // SAFETY: FFI 调用；buf 长度即读取上限
            unsafe { ReadConsoleOutputAttribute(handle, &mut buf, start, &mut read) }.map_err(
                |e| {
                    format!(
                        "ReadConsoleOutputAttribute 失败（0x{:08X}）",
                        e.code().0 as u32
                    )
                },
            )?;
            buf.truncate(read as usize);
            rows.push(buf);
        }
        Ok(rows)
    })
}

/// 插队确认排空判定专用（Task 5 / A1 插队语义）：轮询目标输入缓冲占用直至
/// ≤ [`families::DRAIN_TO`]（15ms 步距）或超时——达标 `Ok(true)`、超时
/// `Ok(false)`（调用方报「投递超时」）、基础设施失败 `Err` 上抛（调用方
/// best-effort 以「写入成功」为准处理）。与注入共用 [`CONSOLE_OP`] 串行
/// （附加态互斥；锁在此取一次，CONIN$ 句柄的临界区骨架复用 [`inject_via`]——
/// 与注入同一纪律：锁 → resolve → attach → guard → CONIN$ 查询 → CloseHandle →
/// guard Drop 复位 → 解锁）。
pub(crate) fn wait_input_drained(pid: u32, timeout_ms: u64) -> Result<bool, String> {
    let _lock = CONSOLE_OP.lock().unwrap_or_else(|e| e.into_inner());
    inject_via(pid, |handle| {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            match query_pending(handle) {
                Ok(pending) => {
                    if pending <= families::DRAIN_TO {
                        return Ok(true);
                    }
                    if Instant::now() >= deadline {
                        return Ok(false);
                    }
                    sleep(DRAIN_POLL_GAP);
                }
                Err(e) => return Err(e),
            }
        }
    })
}

/// 旧薄壳（保留供 trait 默认路径与既有调用编译）：无族回退 [`families::FALLBACK_SPEC`]
/// + 委托 [`inject_text_spec`]（回退常量已归口 families.rs——脆弱常量集中落点）。
pub(crate) fn locate_and_inject(pid: u32, text: &str) -> Result<(), String> {
    inject_text_spec(pid, text, &families::FALLBACK_SPEC).map(|_| ())
}

/// 旧薄壳（单键）：无族回退 [`families::FALLBACK_SPEC`] + 委托 [`inject_key_spec`]。
pub(crate) fn locate_and_send_key(pid: u32, key: &str) -> Result<(), String> {
    inject_key_spec(pid, key, &families::FALLBACK_SPEC)
}

/// PID 策略（M6 裁定）：先试 pid 本体（会话 CLI 原生进程 claude.exe/codex.exe/kimi.exe）；
/// `AttachConsole` 失败 → 祖先链（近→远）第一个附加成功者；全部失败 → 中文错误。
/// M9R 锁纪律：**无锁内部版**——假定调用方已持 [`CONSOLE_OP`] 锁（公共入口
/// 各取一次锁、全程单临界区；resolve 与注入不得各自加锁嵌套）。探测性附加/
/// 祖先回退每一步失败都 FreeConsole 复位，任何路径不滞留附加态。
fn resolve_target(pid: u32) -> Result<u32, String> {
    let mut tried: Vec<String> = Vec::new();
    // 探测性附加：命中即复位（inject_via 会重新附加）——任何路径都不滞留附加态
    if let Err(code) = attach(pid) {
        tried.push(format!("pid={pid} 0x{code:08X}"));
    } else {
        let _ = unsafe { FreeConsole() };
        return Ok(pid);
    }
    let system = sysinfo::System::new_all();
    for ancestor in crate::window::win32::collect_ancestor_pids(&system, pid) {
        if ancestor == pid {
            continue; // 本体已试过
        }
        if let Err(code) = attach(ancestor) {
            tried.push(format!("ancestor={ancestor} 0x{code:08X}"));
        } else {
            let _ = unsafe { FreeConsole() };
            return Ok(ancestor);
        }
    }
    Err(format!(
        "附加目标控制台失败（pid={pid}，尝试：{}）",
        tried.join("；")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **CJK 文本注入活体探针**（2026-10-10 notes 复活批，实机诊断工具）：
    /// 对 `MAM_PROBE_PID` 指定的 codex 进程用**生产路径** `inject_text_spec` 打一段
    /// 中文（vk=0 字符流形态）+ 尾部回车——配合屏读 dump 验证三件事：①WriteConsoleInput
    /// 层实写数（PS 探针 ConIn.ps1 在 CJK 上封送炸 0x8007007A 写 0，Rust 平铺布局
    /// 是另一条路，须单独实证）；②crossterm 对 vk=0 中文事件收不收；③整链落卷。
    /// 跑法：`MAM_PROBE_PID=<codex pid> cargo test --lib cjk_text_inject_live_probe
    /// -- --ignored --nocapture`（前置：目标面板已开 notes 态——本探针会打字+回车提交）。
    #[ignore = "实机 CJK 注入探针：会向目标面板打字并回车提交（前置=MAM_PROBE_PID=<codex pid>）"]
    #[test]
    fn cjk_text_inject_live_probe() {
        let Some(pid) = std::env::var("MAM_PROBE_PID")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            eprintln!("未设 MAM_PROBE_PID——跳过（红线：只碰显式指定的进程）");
            return;
        };
        let spec = families::family_for("codex").unwrap();
        let stats = inject_text_spec(pid, "中文备注八字实测", &spec)
            .unwrap_or_else(|e| panic!("CJK 注入失败（生产路径）：{e}"));
        println!("CJK inject stats: {stats:?}——请配合 codex_question_live_probe dump 屏面核验");
    }

    /// P2-2 键域校验单测：域外键报错（不回退文本+回车），且**域校验必须在
    /// attach 之前快速失败**——假 pid 424242 不触发任何控制台附加（无真控制台
    /// 也可稳定跑，常规 cargo test 门禁内执行）。
    #[test]
    fn unknown_key_rejected() {
        let err =
            inject_key_spec(424242, "bad!", &families::family_for("codex").unwrap()).unwrap_err();
        assert!(err.contains("不支持的按键"));
    }

    /// E1⑤ Ctrl+C 黑名单（裁19）集成锁：`inject_key_spec("ctrl+c")` 在**域校验与
    /// 附加之前**拒绝（假 pid 不触控制台），报错含「禁注」与 opencode 危害点名。
    /// 还原动作（变异）：把 inject_key_spec 的黑名单前置删掉（等价日后域扩展时
    /// ctrl+c 漏网）→ 本测试先红（报错变成域文案或写入成功）。
    #[test]
    fn ctrl_c_rejected_before_domain_check() {
        let err = inject_key_spec(424242, "ctrl+c", &families::family_for("opencode").unwrap())
            .unwrap_err();
        assert!(err.contains("禁注"), "黑名单文案：{err}");
        assert!(
            err.contains("opencode"),
            "须点名 opencode 退出应用的危害：{err}"
        );
    }

    /// 族分派单测（Minor 4）：同一方向键名按族规格分流——A 族（claude，RawVt）
    /// 走 VT 字符流（vk=0 整条单批原子写），B 族（codex，Crossterm）走 VK+scan
    /// 键形态（VK_UP=0x26）。
    #[test]
    fn key_records_for_family_dispatch() {
        let claude = families::family_for("claude").unwrap();
        let up_vt = key_records_for("up", &claude).unwrap();
        assert!(up_vt.iter().all(|k| k.vk == 0)); // VT 字符流形态
        let codex = families::family_for("codex").unwrap();
        let up_vk = key_records_for("up", &codex).unwrap();
        assert_eq!(up_vk[0].vk, 0x26); // VK_UP 键形态
    }

    /// backspace 键契约（2026-09-23 斜杠命令纯净准则的执行原语）：VK_BACK=0x08
    /// 且 ch 携带 0x08（crossterm 解析 ueChar=0x08 → KeyCode::Backspace）、成对
    /// down/up、无修饰位。消费方 = codex 权限路编排段 0.5 的输入行清理。
    #[test]
    fn backspace_key_record_contract() {
        let codex = families::family_for("codex").unwrap();
        let recs = key_records_for("backspace", &codex).expect("backspace 在键域内");
        assert_eq!(recs.len(), 2, "成对 down/up");
        assert!(recs[0].down && !recs[1].down);
        assert!(
            recs.iter().all(|r| r.vk == 0x08 && r.ch == 0x08),
            "VK_BACK 且字符同码"
        );
        let flat: Vec<FlatKeyRecord> = recs.iter().map(Into::into).collect();
        assert!(
            flat.iter().all(|f| f.control_key_state == 0),
            "backspace 不带修饰位（SHIFT_PRESSED 仅 shift+tab）"
        );
    }

    /// shift+tab 组合键契约（2026-09-23 codex 模式组 toggle 改造的覆盖缺口补锁）：
    /// 键名分派命中 `shift_tab_records`（族无关特例）→ VK_TAB+ch=0 的成对 down/up，
    /// 且 `FlatKeyRecord` 转换携带 SHIFT_PRESSED 修饰位——B 族 crossterm 以
    /// `dwControlKeyState` 表达 Shift（戊探C 实机证据），丢修饰位 = 终端收到裸 Tab
    /// （自动补全而不是切模式）。还原动作：删 `From<&KeyRecordSpec>` 的修饰位置位
    /// 或 `key_records_for` 的特例分支 → 任一断言先红。
    #[test]
    fn shift_tab_records_carry_shift_modifier() {
        let codex = families::family_for("codex").unwrap();
        let recs = key_records_for("shift+tab", &codex).expect("shift+tab 在键域内");
        assert_eq!(recs.len(), 2, "成对 down/up");
        assert!(recs[0].down && !recs[1].down);
        assert!(
            recs.iter().all(|r| r.vk == 0x09 && r.ch == 0),
            "VK_TAB 无字符"
        );
        let flat: Vec<FlatKeyRecord> = recs.iter().map(Into::into).collect();
        assert!(
            flat.iter().all(|f| f.control_key_state == SHIFT_PRESSED),
            "两条记录都带 SHIFT_PRESSED"
        );
        assert_eq!(flat[0].virtual_key_code, 0x09);
        assert!(flat.iter().all(|f| f.unicode_char == 0));
    }

    /// WinKeyLayout 真 FFI 契约单测（无需目标控制台，常规 cargo test 可跑）：
    /// 不硬编码键位（规避键盘布局差异误报）——vk_of('a') 与 `VkKeyScanW('a')`
    /// 低字节直接对拍；另钉三条铁律：'\r' → VK_RETURN 契约、非 ASCII → 0
    /// 字符流口径、扫描码派生非 0。
    #[test]
    fn win_key_layout_ffi_contract() {
        let layout = WinKeyLayout;
        // 字符键：vk_of 与 VkKeyScanW 低字节对拍（非硬编码 0x41，布局无关）
        let vk_scan_a = unsafe { VkKeyScanW('a' as u16) };
        assert_ne!(vk_scan_a, -1); // 'a' 在真实键盘布局必可键入
        assert_eq!(layout.vk_of('a'), (vk_scan_a & 0xFF) as u16);
        // KeyLayout trait 契约：'\r' 必返 VK_RETURN(0x0D)
        assert_eq!(layout.vk_of('\r'), 0x0D);
        // 非 ASCII 一律 0（纯字符流口径，与 ConIn.ps1 一致）
        assert_eq!(layout.vk_of('我'), 0);
        // 扫描码派生：VK_RETURN 对应扫描码非 0
        assert!(layout.scan_of(0x0D) > 0);
    }

    /// Task 12 真 FFI 一跳集成测试（#[ignore]：需要弹真实 conhost 窗口，不在常规门禁跑）。
    /// 运行：`cargo test --lib inject::windows_console -- --ignored --nocapture`
    ///
    /// 拓扑口径（M6 实证 + 本机对照实验 2026-09-18）：测试进程直生 cmd（CREATE_NEW_CONSOLE）
    /// 的 AttachConsole 返回 0x80070005，而 conhost.exe cmd /k（PowerShell Start-Process
    /// 起手）拓扑下 attach cmd.exe 稳定成功——故本测试 shell 出 PowerShell 建立已证拓扑。
    /// 地面真值：向该 cmd 注入 `echo FFI-HOP-OK > hop.txt`，文件落盘 = 送达。
    #[test]
    #[ignore = "需要真实 conhost 窗口与外部进程，实机验证时以 --ignored 运行"]
    fn ffi_hop_injects_into_fresh_console_cmd() {
        use super::locate_and_inject;
        use std::process::Command;

        let work = std::env::temp_dir().join(format!("mam-ffi-hop-{}", std::process::id()));
        std::fs::create_dir_all(&work).unwrap();
        let hop = work.join("hop.txt");
        let _ = std::fs::remove_file(&hop);

        // 已证拓扑：conhost.exe cmd /k（挂在 work 目录）
        let script = format!(
            "Start-Process conhost.exe -ArgumentList 'cmd.exe','/k' -WorkingDirectory '{}'",
            work.display()
        );
        let st = Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .status()
            .expect("启动 powershell 失败");
        assert!(st.success(), "Start-Process 失败");
        std::thread::sleep(std::time::Duration::from_millis(3000));

        // 目标 pid：最新起的裸 cmd.exe（CommandLine 恰为 "cmd.exe"，排除其它业务进程）。
        // 查询落盘为 .ps1 执行——规避 Rust→PowerShell 的多层引号转义
        let finder = work.parent().unwrap().join("mam-ffi-hop-find.ps1");
        std::fs::write(
            &finder,
            // CommandLine 实为 "cmd.exe /k"（本测试独有形态，Exclude 其它业务进程）
            "(Get-CimInstance Win32_Process -Filter \"Name='cmd.exe'\" | Where-Object { $_.CommandLine -match 'cmd\\.exe /k$' } | Sort-Object CreationDate -Descending | Select-Object -First 1).ProcessId",
        )
        .unwrap();
        let find_pid = Command::new("powershell")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                &finder.display().to_string(),
            ])
            .output()
            .expect("枚举 cmd pid 失败");
        let out = String::from_utf8_lossy(&find_pid.stdout).trim().to_string();
        let _ = std::fs::remove_file(&finder);
        println!("ffi-hop: target cmd pid={out}");
        let pid: u32 = out.parse().expect("cmd pid 解析失败");

        // 等 cmd 提示符就绪（读武装）
        std::thread::sleep(std::time::Duration::from_millis(1500));

        // 真 FFI 一跳：打字 + 回车（echo 重定向落盘）
        let result = locate_and_inject(pid, "echo FFI-HOP-OK > hop.txt");
        println!("ffi-hop: inject result={result:?}");

        // 等 cmd 执行落盘
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let content = std::fs::read_to_string(&hop).ok();
        println!("ffi-hop: hop.txt={content:?}");

        // 收尾：关掉探测窗口（按 pid 杀 cmd 与其 conhost 宿主）
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();

        assert!(result.is_ok(), "注入应成功：{result:?}");
        assert!(
            content.map(|c| c.contains("FFI-HOP-OK")).unwrap_or(false),
            "hop.txt 应含 FFI-HOP-OK（地面真值）"
        );
    }

    // ===== Task 4 实机探测（占用监控 / 判冻回执 / 屏读层）=====
    // 两条测试均涉及真实控制台子进程（弹真实 conhost 窗口），按批次全局约束
    // 一律 #[ignore]——常规门禁只编译；实机真跑归 Task 12。测试只碰 temp 目录
    // 探测会话，结束 taskkill 清场，零接触真实 ~/.tuvis。

    /// 探测会话起手（复用 [`ffi_hop_injects_into_fresh_console_cmd`] 已证拓扑与
    /// pid 查找法，temp 目录）：PowerShell Start-Process conhost.exe cmd /k →
    /// 查找最新起的裸 cmd.exe（CommandLine 恰为 "cmd.exe /k"，本仓探测独有
    /// 形态，Exclude 其它业务进程）。pid 查询落盘 .ps1 执行——规避
    /// Rust→PowerShell 的多层引号转义（ffi_hop 实证做法）。
    fn spawn_probe_cmd(work: &std::path::Path, tag: &str) -> u32 {
        use std::process::Command;
        let script = format!(
            "Start-Process conhost.exe -ArgumentList 'cmd.exe','/k' -WorkingDirectory '{}'",
            work.display()
        );
        let st = Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .status()
            .expect("启动 powershell 失败");
        assert!(st.success(), "Start-Process 失败");
        std::thread::sleep(Duration::from_millis(3000));

        let finder = work.parent().unwrap().join(format!("mam-{tag}-find.ps1"));
        std::fs::write(
            &finder,
            "(Get-CimInstance Win32_Process -Filter \"Name='cmd.exe'\" | Where-Object { $_.CommandLine -match 'cmd\\.exe /k$' } | Sort-Object CreationDate -Descending | Select-Object -First 1).ProcessId",
        )
        .unwrap();
        let out = Command::new("powershell")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                &finder.display().to_string(),
            ])
            .output()
            .expect("枚举 cmd pid 失败");
        let _ = std::fs::remove_file(&finder);
        let pid_text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        println!("{tag}: target cmd pid={pid_text}");
        pid_text.parse().expect("cmd pid 解析失败")
    }

    /// 清场：按 pid 连带杀 cmd 与其 conhost 宿主（ffi_hop 同款）。
    fn kill_probe(pid: u32) {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
    }

    /// Task 4 判冻回执探测（#[ignore]：需要真实 conhost 子进程，实机真跑归
    /// Task 12）。运行：`cargo test --lib inject::windows_console -- --ignored --nocapture`
    ///
    /// 流程（D8 裁决：不自动介入，判冻只报中文状态回执）：conhost cmd /k 拓扑 →
    /// 内部通道快速连写大批事件制造占用（背压模式块后排空，cmd 消费不及时即
    /// pending>0）→ 独立硬断言：健康消费者（conhost cmd 会正常消费）连写 4000
    /// 字符必须 `outcome.is_ok()`（C1 回归钉）——真冻结态依赖宿主选择
    /// 模式（写入成功 TUI 读不到），本探测无法确定性合成，真值验证归
    /// Task 12（§8.1 / §8.3）。
    #[test]
    #[ignore = "需要真实 conhost 窗口与外部进程，实机验证时以 --ignored 运行"]
    fn occ_stuck_reports_freeze_receipt_probe() {
        let work = std::env::temp_dir().join(format!("mam-occ-probe-{}", std::process::id()));
        std::fs::create_dir_all(&work).unwrap();
        let pid = spawn_probe_cmd(&work, "occ");
        // 等 cmd 提示符就绪（读武装）
        std::thread::sleep(Duration::from_millis(1500));

        // 单线程测试直调内部通道（无并发，锁纪律不适用）：
        // resolve → attach → guard → open_conin → 背压连写 4000 字符制造占用
        let target = resolve_target(pid).expect("resolve 目标失败");
        attach(target).expect("attach 目标失败");
        let guard = AttachGuard;
        // SAFETY: 测试 FFI；句柄在下方无条件 CloseHandle
        let handle = unsafe { open_conin() }.expect("打开 CONIN$ 失败");
        let text = "x".repeat(4000);
        let records = text_records(&text, &WinKeyLayout);
        let outcome = paced_write(
            handle,
            &records,
            true,
            Instant::now() + Duration::from_secs(120),
        );
        println!("occ-probe: paced_write={outcome:?}");
        let after = query_pending(handle).unwrap_or(0);
        println!("occ-probe: pending after={after}");

        // SAFETY: FFI 收尾
        unsafe {
            let _ = CloseHandle(handle);
        }
        drop(guard); // FreeConsole 复位后再清场
        kill_probe(pid);

        // 独立硬断言：健康消费者（conhost cmd 会正常消费）连写 4000 字符必须 Ok
        // （逐样本下降口径下背压锯齿稳态窗口持续重置，不会误判冻结）。C1 回归钉：
        // 若健康消费者出现『选择模式』/『输入积压』Err = C1 回归（判冻口径破坏），
        // 本断言按设计转红。
        assert!(
            outcome.is_ok(),
            "健康消费者不应被误判冻结（『选择模式』/『输入积压』Err = C1 回归）：after={after} paced_write={outcome:?}"
        );
    }

    /// Task 4 屏读层探测（#[ignore]：需要真实 conhost 子进程，实机真跑归
    /// Task 12）。同拓扑 → 不带回车直发文本（tests 模块可直接调私有
    /// [`paced_write`] / [`text_records`]：attach→guard→open_conin→paced_write
    /// 不发 enter）→ [`read_input_tail`]\(pid, 16) 断言返回串含 "tail123"
    /// （屏读滞留输入行 = Task 5 确认层的确切用例）→ 补发 enter 清行 →
    /// taskkill 清场。
    #[test]
    #[ignore = "需要真实 conhost 窗口与外部进程，实机验证时以 --ignored 运行"]
    fn read_input_tail_returns_recent_chars() {
        let work = std::env::temp_dir().join(format!("mam-tail-probe-{}", std::process::id()));
        std::fs::create_dir_all(&work).unwrap();
        let pid = spawn_probe_cmd(&work, "tail");
        // 等 cmd 提示符就绪（读武装）
        std::thread::sleep(Duration::from_millis(1500));

        // 阶段①：直发文本，不发 enter（输入行滞留 = 屏读对象）
        {
            let target = resolve_target(pid).expect("resolve 目标失败");
            attach(target).expect("attach 目标失败");
            let guard = AttachGuard;
            // SAFETY: 测试 FFI；句柄在下方无条件 CloseHandle
            let handle = unsafe { open_conin() }.expect("打开 CONIN$ 失败");
            let records = text_records("echo tail123", &WinKeyLayout);
            let outcome = paced_write(
                handle,
                &records,
                false,
                Instant::now() + Duration::from_secs(10),
            )
            .expect("直发文本失败");
            assert_eq!(outcome.written, records.len(), "全文应一次写完");
            // SAFETY: FFI 收尾
            unsafe {
                let _ = CloseHandle(handle);
            }
            drop(guard); // FreeConsole 复位——read_input_tail 需全新附加（单临界区互斥）
        }

        // 命中回显渲染余量
        std::thread::sleep(Duration::from_millis(500));

        // 阶段②：屏读（契约签名：自取 CONSOLE_OP 锁、自附加、CONOUT$ 读尾部）
        let tail = read_input_tail(pid, 16).expect("read_input_tail 失败");
        println!("tail-probe: read_input_tail(16)={tail:?}");
        assert!(
            tail.contains("tail123"),
            "输入行尾部屏读应含 tail123：{tail:?}"
        );

        // 阶段③：补发 enter 清行（提交滞留行，防探测残留挂输入行）
        {
            let target = resolve_target(pid).expect("resolve 目标失败(清行)");
            attach(target).expect("attach 目标失败(清行)");
            let guard = AttachGuard;
            // SAFETY: 测试 FFI；句柄在下方无条件 CloseHandle
            let handle = unsafe { open_conin() }.expect("打开 CONIN$ 失败(清行)");
            let enter = enter_records(&WinKeyLayout);
            paced_write(
                handle,
                &enter,
                false,
                Instant::now() + Duration::from_secs(10),
            )
            .expect("补发 enter 失败");
            // SAFETY: FFI 收尾
            unsafe {
                let _ = CloseHandle(handle);
            }
            drop(guard);
        }

        kill_probe(pid);
    }
}
