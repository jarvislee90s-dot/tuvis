//! Windows「派发即忘」子进程的**统一出口**（本轮新增，2026-10-12 Task 15 复评 I3）。
//!
//! # 为什么必须走这里
//!
//! 现象（真机，Release 版）：`windows_subsystem = "windows"` 的 GUI 进程**没有控制台**，
//! 从资源页打开目录/文件时报 `os error 6`（ERROR_INVALID_HANDLE）。修法 = 显式
//! `Stdio::null()` 三个 std 句柄，**不去碰父进程的继承句柄**。
//!
//! 机制（按 rustc 1.97.1 std 源码取证——CI/release 均用 `dtolnay/rust-toolchain@stable`）：
//! - `Command::spawn()` 三路 stdio 默认 `Inherit`（`library/std/src/process.rs:1084`）；
//! - 继承的句柄不是直接转交，而是先 `GetStdHandle` 再 `DuplicateHandle(…, inherit=true)`
//!   复制一份可继承副本（`sys/process/windows.rs:605`）；无控制台时槽位里可能是 NULL，
//!   也可能是**已失效的非 NULL 值**（典型来源：由重定向过 stdio 的启动者拉起，启动者
//!   随后关闭了那些句柄）；
//! - NULL 会被 std 放过且不置 `STARTF_USESTDHANDLES`（`sys/process/windows.rs:360-369`
//!   的 `is_set` 守卫，注释明写「与 Win7/Win8+ 行为一致」），即**单纯 NULL 不会报错**；
//!   真正炸的是**失效的非 NULL 值**——`DuplicateHandle` 当场失败，错误码正是
//!   `ERROR_INVALID_HANDLE` = **os error 6**（此时 `CreateProcessW` 可能根本没被调用）。
//!
//! **根因定案状态（如实申报，勿当已定案）**：真机只复现过 `resource.rs` 的 explorer
//! 一处；同一机制下 `inject/resume.rs` 的起窗（修改前同为「三路全继承 + `spawn()`」、
//! 完全同形）**按源码也应失败**，但本轮**未在 Windows 真机验证**（macOS 无法执行
//! release GUI，本机也没有 Windows 主机/wine）。故本模块对 resume / deep_link /
//! marker helper 的收敛是**同形加固**，不是「已验证的第二处缺陷」。定案实验与摘门
//! 条件见 `docs/superpowers/specs/2026-10-12-codex-app-form-止血门底料与摘门条件.md`。
//!
//! # 单点纪律
//!
//! 本函数是**唯一**出口：凡「起窗 / 拉起外部程序 / 派发辅助 exe」而**不等回执**的
//! 派发点都走这里，不要再各写一遍裸 `Command::new(..).spawn()` —— 漏一处就是一处
//! Release 独有的 `os error 6` 面（本纪律的由来：同形缺口当初只修了
//! `commands/resource.rs` 的 explorer 一处，resume 起窗 / deep_link 的 cmd 兜底 /
//! marker helper 均仍是裸 spawn）。
//!
//! `needs_console`：子进程**本该有自己的可见/新建控制台**（Windows 一键 resume 的
//! `cmd /k` 与 `wt` 起窗）时置 `true` —— 此时**不**补 `CREATE_NO_WINDOW`，否则会与
//! 调用方的 `CREATE_NEW_CONSOLE` 起窗语义冲突（要起的终端窗被压掉，比黑框严重得多）；
//! 置 `false` 时补 `CREATE_NO_WINDOW`（0x0800_0000）：派发控制台程序（`cmd /C start`
//! 兜底、marker 辅助 exe）时不闪黑框。两种取值都置 null stdio。
//!
//! 注：`CREATE_NO_WINDOW` 值与 `remote/mod.rs` 的 `NoWindow` 实现同源（均为该 Win32
//! 常量）。两处各自域内单点、互不耦合：remote 那份服务其域内 CLI 探测（管道读输出），
//! 本模块服务 fire-and-forget 派发（不等回执）。
//!
//! 注：`CREATE_NO_WINDOW` 值与 `remote/mod.rs` 的 `NoWindow` 实现同源（均为该 Win32
//! 常量）。两处各自域内单点、互不耦合：remote 那份服务其域内 CLI 探测（管道读输出），
//! 本模块服务 fire-and-forget 派发（不等回执）。
#[cfg(windows)]
use std::process::{Child, Command, Stdio};

/// `CREATE_NO_WINDOW`：控制台程序不建控制台窗口（防闪黑框）。仅 Windows 有意义。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 派发 fire-and-forget 子进程（Windows）。失败契约与 `Command::spawn()` 同形
/// （调用方按既有文案映射）。
#[cfg(windows)]
pub(crate) fn spawn_detached(cmd: &mut Command, needs_console: bool) -> std::io::Result<Child> {
    use std::os::windows::process::CommandExt;
    // ① 不去碰父进程继承的 std 句柄：置 NUL（新开的、可继承的合法句柄）——
    //    继承面（含 std 侧的 DuplicateHandle）整个绕开，机制见模块文档
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // ② 防闪窗（仅在子进程不需要自己的控制台时；见模块文档的 needs_console）
    if !needs_console {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn()
}
