//! Windows GUI 宿主「起子进程」的**统一出口**（2026-10-12 Task 15 复评 I3 新增；
//! 第二轮复评 J2 拆出两种模式）。
//!
//! # 两种模式（参数 [`ConsoleMode`]）
//!
//! - [`ConsoleMode::Detached`]：**派发即忘**（打开目录/文件、深链兜底、marker 辅助 exe）。
//!   子进程不需要控制台 → 三路 stdio 置 NUL（新开的、可继承的合法句柄，**不去碰父进程
//!   继承的句柄**）+ `CREATE_NO_WINDOW` 防闪黑框。
//! - [`ConsoleMode::OwnConsole`]：**子进程自带新控制台**（一键 resume 的 `wt` / conhost
//!   起窗）。**不变式：本分支一概不碰 stdio（保持 `Inherit`），也不动调用方的
//!   `creation_flags`** —— 这条路径要的是**新控制台自己的标准句柄**，不是 NUL：
//!   三路 `Stdio::null()` 会各自打开 `\\.\NUL` 拿到**非 NULL** 句柄 ⇒ std 的 `is_set`
//!   守卫为真 ⇒ 置上 `STARTF_USESTDHANDLES` ⇒ 窗内 `cmd /k` 的 stdin 指向 NUL 设备
//!   （非 TTY、随即 EOF），可能开出**死窗**；不碰 stdio 时（父进程是 GUI 无控制台 ⇒
//!   三路继承句柄为 NULL ⇒ 该标志不置）由系统按新控制台默认行为交付句柄。同理不得覆写
//!   `CREATE_NEW_CONSOLE`（要起的终端窗会被压掉）。
//!
//! # Detached 为什么置 NUL（现象与两条候选机制）
//!
//! 现象（真机，Release 版）：`windows_subsystem = "windows"` 的 GUI 进程没有控制台，
//! 从资源页打开目录/文件时报 `os error 6`（ERROR_INVALID_HANDLE）。
//!
//! 按 rustc 1.97.1 std 源码（CI/release 均用 `dtolnay/rust-toolchain@stable`）：
//!
//! - `Command::spawn()` 三路 stdio 默认 `Inherit`（`library/std/src/process.rs:1084`）；
//! - 继承的句柄不是直接转交：先 `GetStdHandle`，再 `DuplicateHandle(…, inherit=true)`
//!   复制一份可继承副本（`sys/process/windows.rs:605-618`）；
//! - 无控制台时 `GetStdHandle` 得 NULL ⇒ `get_handle` 返回 `Err(ERROR_INVALID_HANDLE)`
//!   （`sys/stdio/windows.rs:67-77`），由**调用方** `to_handle` 把它吞成 null 句柄
//!   （`sys/process/windows.rs:612-614`）；
//! - **仅当至少一个句柄非 null 才置 `STARTF_USESTDHANDLES`**
//!   （`sys/process/windows.rs:360-369` 的 `is_set` 守卫，注释明写「与 Win7/Win8+ 行为
//!   一致」）。
//!
//! 据此有**两条候选机制**都能产出 `os error 6`，**explorer 那次命中哪一条仍未解释**
//! （需真机句柄槽位取证，见底料档的判定性实验）：
//!
//! 1. **置了标志且其中带 NULL 项**：三路**全** NULL 时守卫为假、标志根本不置 ⇒ 按源码
//!    **不该报错**（故「全 NULL」这一态解释不了 explorer 的那次失败）；但**混态**——
//!    三路里至少一路是**有效非 NULL**、其余为 NULL——守卫为真 ⇒ 标志置上且**带 NULL 项**
//!    ⇒ 若 `CreateProcessW` 拒收 NULL 项，同样得到 `ERROR_INVALID_HANDLE`。**未在真机
//!    验证**（本仓库 `remote/tunnel.rs` / `remote/tailscale/status.rs` 在 release 版
//!    可用、且其 stdin 仍是继承来的 NULL，可作「NULL 项未必致命」的旁证，但两者都不是
//!    全 NULL/混态的完整对照）。
//! 2. **失效的非 NULL 值**：槽位里留着一个**已失效（非 NULL）**的句柄值——典型来源：由
//!    重定向过 stdio 的启动者拉起、启动者随后关闭了那些句柄——`DuplicateHandle` 当场
//!    失败，错误码正是 `ERROR_INVALID_HANDLE`（此时 `CreateProcessW` 可能根本没被调用）。
//!
//! 两条候选下「显式置 NUL」都是对的：候选 1 下无害（本就是 NULL 语义、且不再把 NULL 项
//! 交给 OS），候选 2 下彻底绕开 `DuplicateHandle` 这条面。
//!
//! **根因定案状态（如实申报，勿当已定案）**：真机只复现过 `commands/resource.rs` 的
//! explorer 一处；`inject/resume.rs` 的起窗（改动前同为「三路全继承 + `spawn()`」、完全
//! 同形）是否也曾失败**未在 Windows 真机验证**（macOS 无法执行 release GUI，本机也没有
//! Windows 主机 / wine）。故那处收敛是**同形加固**，不是「已验证的第二处缺陷」。定案
//! 实验与摘门条件见 `docs/superpowers/specs/2026-10-12-codex-app-form-止血门底料与摘门条件.md`。
//!
//! # 单点纪律
//!
//! 本函数是**唯一**出口：凡「起窗 / 拉起外部程序 / 派发辅助 exe」而**不等回执**的派发点
//! 都走这里，不要再各写一遍裸 `Command::new(..).spawn()` —— 漏一处就是一处 Release 独有
//! 的 `os error 6` 面（本纪律的由来：同形缺口当初只修了 `commands/resource.rs` 的 explorer
//! 一处，resume 起窗 / deep_link 的 cmd 兜底 / marker helper 均仍是裸 spawn）。
//!
//! 注：`CREATE_NO_WINDOW` 值与 `remote/mod.rs` 的 `NoWindow` 实现同源（均为该 Win32 常量）。
//! 两处各自域内单点、互不耦合：remote 那份服务其域内 CLI 探测（管道读输出），本模块服务
//! 起窗 / 派发（不等回执）。
#[cfg(windows)]
use std::process::{Child, Command, Stdio};

/// `CREATE_NO_WINDOW`：控制台程序不建控制台窗口（防闪黑框）。仅 Windows 有意义。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 子进程的**控制台归属**（决定 stdio 与 creation flags 两条完全不同的处理；第二轮复评
/// J2：此前「一律置 NUL 只用布尔控制 NO_WINDOW」把一个未验证的行为变更带进了本来就该有
/// 控制台的 resume 起窗路径）。见模块文档的两条不变式。
#[cfg(windows)]
#[derive(Clone, Copy)]
pub(crate) enum ConsoleMode {
    /// **派发即忘**：子进程不需要控制台（打开目录/文件、深链兜底、辅助 exe）
    /// —— 三路 stdio 置 NUL + `CREATE_NO_WINDOW`。
    Detached,
    /// **子进程自带新控制台**（一键 resume 的 `wt` / conhost 起窗）
    /// —— **一概不碰 stdio 与 creation_flags**。
    OwnConsole,
}

/// 派发子进程（Windows）。失败契约与 `Command::spawn()` 同形（调用方按既有文案映射）。
#[cfg(windows)]
pub(crate) fn spawn_detached(cmd: &mut Command, mode: ConsoleMode) -> std::io::Result<Child> {
    use std::os::windows::process::CommandExt;
    match mode {
        ConsoleMode::Detached => {
            // ① 不去碰父进程继承的 std 句柄：置 NUL（新开的、可继承的合法句柄）——
            //    继承面（含 std 侧的 DuplicateHandle）整个绕开，机制见模块文档
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // ② 防闪窗（子进程不需要自己的控制台）
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        // 不变式（勿改）：OwnConsole 不动 stdio、不动 creation_flags —— 该路径要的是
        // 新控制台自己的标准句柄；置 NUL 会让窗内 cmd /k 的三路句柄指向 NUL 设备
        //（stdin 非 TTY、随即 EOF）可能开出死窗，而覆写 flags 会压掉 CREATE_NEW_CONSOLE
        // 的起窗语义。理由与证据口径见模块文档。
        ConsoleMode::OwnConsole => {}
    }
    cmd.spawn()
}
