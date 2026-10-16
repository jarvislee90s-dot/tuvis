//! Tailscale 通道的**运行态**（§C1）：CLI 探测/调用、快照、期望态与周期轮询守护、
//! serve 配置守卫、起停生命周期。**没有前台子进程可守**——运行态由轮询现算（见模块文档）。
//!
//! 与 [`super::wizard`]（供应链：下载与特权安装）和 [`super::reach`]（可达性校验）分开：
//! 本文件是常驻线程与 CLI 派生的所在，改动它的风险画像与"装软件"完全不同。
//!
//! ⚠️ **写路径验证位逐平台不同**（B-M5）：`funnel --bg` / `funnel reset` 的「立即返回」
//! 在 **Windows 已实测确认**（2026-10-07：退出码 0、0.1 秒返回），在 **macOS 仍是上游行为
//! 预期**（本机只跑过只读命令）——见 [`super::wizard::write_path_verified`]。

use std::path::PathBuf;

use super::reach::{current_dns_host, poll_reverify, set_reachability, Reachability};
use super::wizard::extract_approval_url;
use crate::remote::NoWindow;

/// `status --json` 的关键字段（只取需要的，不做全量建模）
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TsStatus {
    /// 实测取值：`Running`（已登录在线）/ `NeedsLogin` / `Stopped` 等
    pub backend_state: String,
    /// 待登录时的授权链接；已登录为空串。**兔维斯 把它做成按钮，不自己登录**
    pub auth_url: String,
    /// **带结尾点**（实测 `xxx.ts.net.`）——使用前必须归一
    pub dns_name: String,
    /// 不带结尾点的同值域名（实测 `CertDomains[0]`）
    pub cert_domains: Vec<String>,
}

/// 解析 `tailscale status --json`。**坏 JSON / 缺关键字段一律 Err**——
/// 引导要能如实报「读不到状态」，不得默认成 Running（那会把失败伪装成成功）。
pub(super) fn parse_status(json: &str) -> Result<TsStatus, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("状态非 JSON: {e}"))?;
    let backend_state = v
        .get("BackendState")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "状态缺少 BackendState".to_string())?
        .to_string();
    Ok(TsStatus {
        backend_state,
        auth_url: v
            .get("AuthURL")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        dns_name: v
            .get("Self")
            .and_then(|s| s.get("DNSName"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        cert_domains: v
            .get("CertDomains")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// Funnel 是否已伺服。实测：未开通时 `funnel status --json` 是 **`{}`**。
/// 解析不了按「未激活」处理（fail-closed——宁可说没开，也不谎称已开）。
pub(super) fn funnel_active(json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.as_object().map(|o| !o.is_empty()))
        .unwrap_or(false)
}

/// 后端是否**仍在初始化**（开机恢复窗口）。
///
/// **实测依据（2026-10-07 真机重启逐秒取证）**：T+58s Windows 服务已 Running
/// （Automatic），但 `BackendState=NoState`、`funnel status --json` **短暂为 `{}`**；
/// T+73s 后端转 Running 时 tailscaled 已把 Funnel 配置**逐字段原样恢复**（307 字节）。
/// ⇒ 这个窗口里的 `{}` **不是「配置丢失」**，是"还没加载完"。据此：
/// - [`map_status`] 不得把它报成故障（UI 语义 = 恢复中）；
/// - [`enable_funnel`] / [`heal_and_reverify`] 在判定配置归属与重开之前**必须先过本判据**。
///
/// 取值面：`NoState` 为实测形态；`Starting` 是上游语义里的同族初始化取值（一并按
/// 恢复中处理——它们都表示"后端还没给出结论"，都不构成"配置缺失"的证据）。
/// **不**把 `NeedsLogin` / `Stopped` 算进来：那两个是等用户动作 / 服务真停了，
/// 谎报成"正在恢复"会让人干等。
pub(super) fn backend_initializing(state: &str) -> bool {
    matches!(state, "NoState" | "Starting")
}

/// 后端阶段（由 `status --json` 现算；驱动快照与可达性语义）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackendPhase {
    /// `BackendState == "Running"`：后端就绪（**唯一允许判 Funnel 配置/重开的阶段**）
    Ready,
    /// 初始化中（`NoState` / `Starting`）：恢复窗口，不判配置、不重开
    Initializing,
    /// 其余（`NeedsLogin` / `Stopped` / …）：等用户动作或服务没起来，如实报
    Other,
}

/// 由状态读数定性后端阶段（纯函数，可测）
pub(super) fn backend_phase(st: &TsStatus) -> BackendPhase {
    if st.backend_state == "Running" {
        BackendPhase::Ready
    } else if backend_initializing(&st.backend_state) {
        BackendPhase::Initializing
    } else {
        BackendPhase::Other
    }
}

/// 由域名生成看板地址（**契约：恒为含 `/m` 的完整地址**）。
/// **复用 `tunnel::board_url` 做归一，不要重造**——它已实现
/// "去尾斜杠 + 补 `/m`" 且是幂等的，并有既有测试覆盖。
/// 本函数只多做一件事：**去掉实测存在的结尾点**（`Self.DNSName` 是 `xxx.ts.net.`）。
pub(super) fn board_url_from_dns_name(dns: &str) -> String {
    let host = dns.trim().trim_end_matches('.');
    if host.is_empty() {
        return String::new();
    }
    crate::remote::tunnel::board_url(&format!("https://{host}"))
}

// ============================================================
// CLI 探测与调用（进程/网络的唯一入口，全部经 run_cli 便于替换）
// ============================================================

/// CLI 候选路径表（**逐平台固定、只有完整路径**）。
///
/// **A5 纪律（2026-10-07 Windows 实测）**：`Get-Command tailscale` **为空**（CLI 不在
/// PATH）；卸载登记项 `InstallLocation=（空）`，而 `WindowsInstaller=1` +
/// `UninstallString=MsiExec.exe /X{...}` 表明是 **MSI 版**。故本函数：
/// - **不得**回落到 PATH 查找（实测找不到；且 PATH 命中还可能是冒名/旧版二进制）；
/// - **不得**读卸载登记的 `InstallLocation`（实测为空——注册表是可变输入，不是安装事实）；
/// - 只认标准安装位置的存在性。**探测不到 = 没装**，由调用方如实报「未检测到 Tailscale」。
///
/// ⚠️ **本表只是候选链的第一段**（2026-10-08 补）：它装的是「最常见/最快命中」的固定
/// 位置，**不再是唯一来源**。用户在 MSI 里自选路径（兔维斯 刻意不加 `/qn`，选择权本就该
/// 给用户）时这里必然落空，由 [`discovered_cli`]（Windows = 服务登记 `ImagePath`）兜底
/// ——**顺序与 fail-closed 契约见 [`find_cli_with`]**。本表自身仍是"逐平台固定完整路径"
/// （动态来源不进这张表，见其守卫测试）。
pub(super) fn cli_candidates(p: super::wizard::Platform) -> &'static [&'static str] {
    use super::wizard::Platform;
    match p {
        Platform::Mac => &["/Applications/Tailscale.app/Contents/MacOS/Tailscale"],
        Platform::Windows => &[r"C:\Program Files\Tailscale\tailscale.exe"],
        Platform::Other => &["/usr/bin/tailscale", "/usr/local/bin/tailscale"],
    }
}

/// CLI 路径探测（生产入口）：默认位置优先，落空才启用第二来源。契约见 [`find_cli_with`]。
/// **不做全盘搜索、不看 PATH**（依据见 [`cli_candidates`]）。
pub(super) fn find_cli() -> Option<PathBuf> {
    find_cli_with(
        cli_candidates(super::wizard::current_platform()),
        discovered_cli,
        |p| p.exists(),
    )
}

/// **候选链内核**（顺序即优先级；第二来源与存在性都注入，便于单测锁死顺序与 fail-closed）：
/// 1. **默认安装位置**（[`cli_candidates`]）——最快、覆盖绝大多数用户，**语义保持不变**；
/// 2. **第二来源**（[`discovered_cli`]；Windows = 服务登记）——**惰性**：默认位置一旦命中
///    就**连读都不读**（`discovered` 取 `FnOnce`，测试用 panic 替身把这一点锁死）；
/// 3. **链尾存在性门**：链上给出的任何路径都必须真实存在才返回；
/// 4. 全落空 → `None`（**fail-closed**：绝不返回一个猜的路径）。
///
/// **只增不改语义**（2026-10-08 用户裁决「治本」）：默认位置仍最优先，新增来源只在前面
/// 全部找不到时才用。
pub(super) fn find_cli_with(
    defaults: &[&str],
    discovered: impl FnOnce() -> Option<PathBuf>,
    exists: impl Fn(&std::path::Path) -> bool,
) -> Option<PathBuf> {
    defaults
        .iter()
        .map(PathBuf::from)
        .find(|p| exists(p.as_path()))
        .or_else(|| discovered().filter(|p| exists(p.as_path())))
}

// ------------------------------------------------------------
// 第二来源（Windows 服务登记）：**治本**——安装路径由用户选，兔维斯 不得假定它在哪
// ------------------------------------------------------------

/// 服务登记的注册表子键（`HKLM\` 下；末段 `Tailscale` = MSI 装出来的**服务名**——
/// `ServiceInstall.Name` / `ServiceControl.Name` 两者同名，实机侧另有
/// `sc.exe stop Tailscale` 佐证）。**为什么读注册表而不是 `sc qc Tailscale`**：
/// - `sc qc` 的字段标签（`BINARY_PATH_NAME`）**随系统显示语言本地化**——中文 Windows 上
///   根本不是这个标签，按标签解析会在**恰好出问题的那台中文机**上失配；注册表的**值名
///   `ImagePath` 是 ASCII、系统怎么本地化都不变**；
/// - `sc qc` 要派生子进程（多一个进程、多一份等待/失败面），注册表直读零进程；
/// - `winreg` **本仓已是 Windows 目标下的既有依赖**（`linker/detector.rs` 已用同一套读法），
///   零新增依赖。
///
/// **非管理员可读**：`HKLM\SYSTEM\CurrentControlSet\Services` 子树默认对 Users 授
/// `KEY_READ`，服务的默认安全描述符同样把 `SERVICE_QUERY_CONFIG` 交给已认证用户——
/// 与 `sc qc` 的可读性同源，只是不弹子进程。
///
/// ⚠️ **本机无法验证**（2026-10-08：本机为 macOS）：子键路径与值的**真实形态**须由
/// Windows 真机复核（见模块报告）。路径串逐字锁在测试
/// `service_image_path_registry_key_is_the_real_one` 里，改错即红。
#[cfg(any(windows, test))]
pub(super) const TS_SERVICE_REG_PATH: &str = r"SYSTEM\CurrentControlSet\Services\Tailscale";

/// 读服务登记的 `ImagePath`（**本模块唯一接触注册表的点**）。
///
/// 读不到一律 `None`——服务不存在（没装）/ 无权限 / 值不是字符串，三种情形一视同仁：
/// **不猜、不回落别的来源**（fail-closed：宁可如实报「未检测到 Tailscale」）。
///
/// **M6（2026-10-08 架构评审）：`REG_EXPAND_SZ` 必须展开 `%VAR%`。** 旧实现按字符串读、
/// **不展开**，于是 `ImagePath` 写成 `%ProgramFiles%\Tailscale\...` 的机器上这条第二来源
/// 会**静默失效**（fail-closed 成"未安装"——安全，但治不了本：用户明明装在默认位置却看到
/// 「未安装」）。现在读**原始值**拿 `vtype`，只有 `REG_EXPAND_SZ` 才展开（`REG_SZ` 里合法
/// 出现的 `%` 不得被改写），展开走纯函数 [`image_path_value`]。
#[cfg(windows)]
fn service_image_path() -> Option<String> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, REG_EXPAND_SZ, REG_SZ};
    use winreg::types::FromRegValue;
    use winreg::RegKey;

    let key = match RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(TS_SERVICE_REG_PATH) {
        Ok(k) => k,
        // 第二来源读不到不是故障（没装本来就该报没装）⇒ debug 级留痕即可，不打扰用户
        Err(e) => {
            log::debug!(
                "读取服务登记 ImagePath 失败（{TS_SERVICE_REG_PATH}）: {e}——第二来源按空处理"
            );
            return None;
        }
    };
    let raw = match key.get_raw_value("ImagePath") {
        Ok(raw) => raw,
        Err(e) => {
            log::debug!(
                "读取服务登记 ImagePath 失败（{TS_SERVICE_REG_PATH}）: {e}——第二来源按空处理"
            );
            return None;
        }
    };
    // 形态不对（REG_DWORD / REG_MULTI_SZ / …）：**不猜**——宁可如实报「未检测到」
    if raw.vtype != REG_SZ && raw.vtype != REG_EXPAND_SZ {
        log::debug!(
            "服务登记 ImagePath 的值类型不是字符串（{:?}）——第二来源按空处理",
            raw.vtype
        );
        return None;
    }
    // 形态对：解码（FromRegValue 认 REG_SZ / REG_EXPAND_SZ），按类型决定展开
    let s = String::from_reg_value(&raw).ok()?;
    Some(image_path_value(&s, raw.vtype == REG_EXPAND_SZ, |n| {
        std::env::var(n).ok()
    }))
}

/// 注册表值 → 可交给 [`parse_service_image_path_with`] 的 `ImagePath` 串（**纯函数**，
/// 变量查找注入 ⇒ 任何宿主可测）：`REG_EXPAND_SZ` 按 `ExpandEnvironmentStringsW` 语义展开
/// `%VAR%`；其余形态**原样返回**（`REG_SZ` 里的 `%` 是路径的一部分，不是变量引用）。
///
/// "按类型决定"这条形态门是刻意的：无差别展开会把 `C:\100%\Tailscale\...` 这类合法路径
/// 改写成别的东西——治本的方式是认对类型，而不是"凡串皆展开"。
#[cfg(any(windows, test))]
pub(super) fn image_path_value(
    raw: &str,
    expand_sz: bool,
    lookup: impl Fn(&str) -> Option<String>,
) -> String {
    if expand_sz {
        expand_env_refs(raw, lookup)
    } else {
        raw.to_string()
    }
}

/// 展开 `%VAR%` 引用（Windows `REG_EXPAND_SZ` / `ExpandEnvironmentStringsW` 语义；
/// **纯函数**，单趟扫描、不递归展开替换结果）。
///
/// 边界（与上游语义对齐，逐条都有测试 `env_expansion_keeps_unknown_and_malformed_refs_verbatim`）：
/// - `%NAME%` 成对且 `NAME` 非空：命中就替换；**查不到则原样保留**（Windows 对未知变量也是
///   原样保留，不报错、不落空——这点很重要：fail-closed 的失败方式是"路径不存在"，不是
///   "路径被吞成空串"）；
/// - **没有配对的第二个 `%`**（如 `C:\50%\x.exe`）或**空名 `%%`**：原样保留，一个字符都不吞；
/// - 无 `%`：恒等（逐字复制，不做任何变量查找）。
///
/// 与 [`image_path_value`] 同一条 cfg 门（只服务 Windows 的第二来源与测试：非 Windows 的
/// 生产构建里它若留着就是死代码）。
#[cfg(any(windows, test))]
fn expand_env_refs(raw: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw; // 还没扫描的剩余片段
    while let Some(open) = rest.find('%') {
        // `%` 之前的普通片段原样带过
        out.push_str(&rest[..open]);
        // 落单的 `%`（后面再没有配对的）：整段原样保留到串尾
        let Some(close) = rest[open + 1..].find('%') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let name = &rest[open + 1..open + 1 + close];
        // 空名 `%%` 不得去查变量（`%NAME%` 才是一次引用）
        let value = if name.is_empty() { None } else { lookup(name) };
        match value {
            Some(val) => out.push_str(&val),
            // 未知变量 / 空名：整段原样保留（不吞字符、不报错）
            None => out.push_str(&rest[open..=open + 1 + close]),
        }
        rest = &rest[open + 2 + close..]; // 配对 `%` 之后
    }
    out.push_str(rest);
    out
}

/// 从服务登记的 `ImagePath` 原始值里解析出**服务可执行文件**的路径。
///
/// **纯函数**：不碰注册表、不碰文件系统——存在性判据 `exists` 由调用方注入。这样 Windows
/// 的解析语义能在**任何宿主**上用注入测试锁死（2026-10-08 本仓只有 macOS 可用，真机形态
/// 无从验证 ⇒ 注入测试是唯一可执行的证据）。
///
/// 形态规则（逐条对应 Windows 的 `CreateProcess` 解析语义）：
/// 1. **前后空白先 trim**（空格 / Tab / CRLF）——注册表值可能带尾随空白；
/// 2. **带引号**（`"C:\...\x.exe" <参数>`）：取**第一对引号内**的内容。这是唯一无歧义的
///    形态（路径里的空格不再是问题），且**零 FS 访问**；引号内为空或**只有开引号**
///    （残缺）→ `None`；
/// 3. **不带引号**：Windows 会把命令行在**每个空白处**切开、把逐个加长的前缀当候选程序名。
///    本实现取「**存在且最长**」的候选。与 Windows 的"由短到长取首个存在者"在常态下同解
///    （`C:\Program.exe` 这类劫持位通常不存在），但在**劫持位真存在**时本实现**不会**被
///    引到那个文件上：本函数的用途是定位 Tailscale 的安装目录，照搬由短到长就等于让一个
///    人为放进 `C:\Program.exe` 的文件来决定 兔维斯 去哪里找 CLI（unquoted service path
///    提权的同一机理）；退一步说，即便只剩下被劫持的短前缀存在，推出的
///    `<目录>\tailscale.exe` 仍要过链尾存在性门 ⇒ 依旧 fail-closed；
/// 4. **无存在者 / 形态残缺 → `None`**。**不猜**：宁可如实报「未检测到 Tailscale」，
///    也不返回一个猜的路径让调用方以莫名错误失败。
#[cfg(any(windows, test))]
pub(super) fn parse_service_image_path_with(
    raw: &str,
    exists: impl Fn(&str) -> bool,
) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    // ② 带引号：第一对引号内即 exe 路径
    if let Some(rest) = s.strip_prefix('"') {
        let (inner, _) = rest.split_once('"')?; // 残缺（无闭合引号）→ None
        let inner = inner.trim();
        return (!inner.is_empty()).then(|| inner.to_string());
    }
    // ③ 不带引号：由长到短取「存在」的候选前缀（空白即参数边界）
    let mut cand = s; // s 已 trim ⇒ 首轮候选 = 整串，且此后每轮都非空、无尾随空白
    loop {
        if exists(cand) {
            return Some(cand.to_string());
        }
        // 没有更短的空白边界了 ⇒ 候选已试尽，如实 None（不猜）
        cand = cand[..cand.rfind(char::is_whitespace)?].trim_end();
    }
}

/// 由服务本体的 exe 路径推出 **CLI 路径**（纯函数）。
///
/// 依据：Tailscale 的 Windows 服务跑的是 `tailscaled.exe`（守护进程），CLI 是**同一个安装
/// 目录**里的 `tailscale.exe`——MSI 的 File 表里两者同装到 `[INSTALLDIR]`。**本推导对上游
/// 变更免疫**：无论服务本体登记的是 `tailscaled.exe` 还是（有朝一日）`tailscale.exe`，
/// 「同目录 + `tailscale.exe`」都给出同一条路径。
///
/// **为什么不用 `std::path::Path`**：本函数处理的是 **Windows 形态的路径串**（反斜杠分隔），
/// 而 `Path` 的分隔符语义**跟随编译宿主**——在 macOS/Linux 上 `\` 不是分隔符，`parent()`
/// 会返回空串，纯函数测试根本跑不起来（本仓只有 macOS 可用）。故这里自己按 `\` / `/` 取
/// 最后一段分隔符；两种分隔符都认（在 Windows 上等价）。
///
/// 没有目录成分（裸文件名）→ `None`：无从知道装在哪，**不猜**（绝不返回一个相对路径，
/// 让调用方在当前工作目录里瞎找）。
#[cfg(any(windows, test))]
pub(super) fn cli_beside_service_exe(service_exe: &str) -> Option<String> {
    let s = service_exe.trim();
    let cut = s.rfind(['\\', '/'])?;
    let dir = &s[..=cut]; // 含结尾分隔符
    Some(format!("{dir}tailscale.exe"))
}

/// **第二来源**（默认位置之外的唯一来源；**惰性**——只在默认位置全部落空时才求值，
/// 见 [`find_cli_with`] 的顺序契约）。
///
/// 治本依据：Windows MSI 让用户自选安装路径（兔维斯 执行 `msiexec /i`，**刻意不加 `/qn`**），
/// 装到哪由用户定；而**服务登记必然指向真实安装位置**，与盘符 / 目录名 / 中文都无关。
///
/// 返回的是**候选**（**CLI 路径**此处不判存在：存在性由 [`find_cli_with`] 在链尾统一收口，
/// **CLI 那一次**才是门）；读不到登记 / 解析不出 exe / 推不出目录——任一步落空即 `None`（**不猜**）。
///
/// ⚠️ **链上其实有两次 `exists`，别把前一次当成门**（2026-10-08 收口轮限定语境）：上面第 ② 步
/// 的 [`parse_service_image_path_with`] 对**不带引号**的 `ImagePath` 会逐段加长地试候选，
/// 那里的 `exists`（生产注入 `Path::exists`）**确实会碰文件系统**——但它是**解析手段**
/// （用来切出参数边界、定位服务本体 exe），判的是"服务本体在哪"，**不是**"CLI 在不在"；
/// CLI 的存在性只在链尾那一道门判。（带引号形态零 FS 访问，见该函数的形态规则 ②。）
#[cfg(windows)]
pub(super) fn discovered_cli() -> Option<PathBuf> {
    let raw = service_image_path()?; // ① 读服务登记
    let exe = parse_service_image_path_with(&raw, |p| std::path::Path::new(p).exists())?; // ② 服务本体
    cli_beside_service_exe(&exe).map(PathBuf::from) // ③ 同目录 CLI
}

/// **非 Windows：没有第二来源。** macOS 的 CLI 在固定 `.app` 位置、Linux 走包管理器路径，
/// 不存在"用户自选安装路径"这个问题 ⇒ 恒 `None`，`find_cli` 的行为与改动前**逐字相同**。
/// 平台门控写在这一层，好让上面那些 Windows 专用的解析/推导在非 Windows 的生产构建里
/// **连编译都不参与**（它们标了 `#[cfg(any(windows, test))]`，只在 Windows 或测试构建存在）。
#[cfg(not(windows))]
pub(super) fn discovered_cli() -> Option<PathBuf> {
    None
}

/// CLI 失败 → 错误文案（纯函数，可测）：stderr 非空用 stderr；**空串回落退出码 +
/// 命令上下文（修复轮 2 指针②）**——旧实现 `Err("")` 会让前端只见空 toast，无从排查。
/// 退出码取不到（被信号杀死等）如实写「未知」
pub(super) fn cli_error(args: &[&str], stderr: &str, code: Option<i32>) -> String {
    let s = stderr.trim();
    if !s.is_empty() {
        return s.to_string();
    }
    format!(
        "TS_CLI_FAIL_NO_STDERR: Tailscale CLI 失败（tailscale {}），退出码 {}，且无 stderr 输出",
        args.join(" "),
        code.map(|c| c.to_string()).unwrap_or_else(|| "未知".into())
    )
}

/// 只读命令（`status --json` / `get --json` / `funnel status --json`）的等待上限：
/// 给够——tailscaled 忙时慢一点是正常的，误杀只读查询会把「读不到状态」谎报成故障
pub(super) const CLI_TIMEOUT_READ: std::time::Duration = std::time::Duration::from_secs(20);

/// **写路径**（`funnel --bg` / `funnel reset` / `set --shields-up=false`）的等待上限：
/// **更短**（I-4）。A6 已实测这条 CLI 不加 `--bg` 时会打印横幅后**永久阻塞终端**；而
/// macOS 首开在设计中确有**一次浏览器批准**，`--bg` 在 macOS GUI standalone build 上
/// 是否立即返回**属未验证**（[`super::wizard::write_path_verified`]`(Mac) = false`）。
/// 若它等批准才返回：向导「开通」步的 spawn_blocking 线程会**永久挂住**（按钮永远转圈、
/// 无错误、无出路）。故写路径必须更早放弃，并把「超时」如实报出去（用户可重试），
/// **绝不静默挂着**——宁可报一次可重试的超时，也不留一个永不返回的调用。
pub(super) const CLI_TIMEOUT_WRITE: std::time::Duration = std::time::Duration::from_secs(10);

/// 该次 CLI 调用的等待上限（纯函数，可测）：写路径短窗、只读长窗。
/// 判据按命令语义而非「猜」：`funnel --bg` / `funnel reset` / `set` 三者会改动状态，
/// `funnel status` / `status` / `get` 只读。
pub(super) fn cli_timeout_for(args: &[&str]) -> std::time::Duration {
    match args {
        ["funnel", "reset", ..] | ["funnel", "--bg", ..] | ["set", ..] => CLI_TIMEOUT_WRITE,
        _ => CLI_TIMEOUT_READ,
    }
}

/// 有界等待子进程（I-4 的核心，可测）→ (退出态, stdout, stderr)。
///
/// **三条纪律**：
/// 1. **超时即 kill + 收尸**（`wait_timeout` + `kill` + `wait`），错误文案如实写「调用
///    Tailscale 超时（N 秒未返回，已终止该子进程）」——由调用方写进快照 error（不静默）；
/// 2. **wait 之前先把两条管道交给读线程**：若「先 wait 后读」，输出超过管道缓冲（~64KB）
///    的子进程会在我们 wait 时被写满而阻塞，从而**假性超时**（`approve.rs` 已如实登记该
///    边界）。`tailscale status --json` 在设备多的尾网里就可能超过 64KB，故本实现不接受
///    那个边界（测试 `run_cli_bounded_wait_kills_on_timeout_and_drains_large_output` 锁）；
/// 3. **超时分支绝不为「读完输出」而无限等待**（2026-10-07 Linux CI 抓获）：第 2 条的读
///    线程要等**管道 EOF**，而 EOF 只由「所有持有写端的进程都退出」触发。若直接子进程
///    fork 出了继承 stdout/stderr 的孙进程，kill 掉直接子进程后孙进程仍持有写端 ⇒ 读线程
///    要等孙进程自己退出，**有界等待被打回无界**（实测：Linux 上 `sh -c "sleep 30"` 的
///    200ms 超时窗耗了 30.0037s；macOS 的 `/bin/sh` 对单命令会 exec 故未暴露，但用
///    `sleep N & wait` 强制 fork 后 macOS 同样复现 20.011s）。故超时分支的收线程走
///    [`collect_bounded`]：只给有界宽限，拿不到就放弃（该分支输出本就丢弃）。
///
/// 为什么不用 `Command::output()`：它同样无超时（A6 实测这条 CLI 会永久阻塞）。
pub(super) fn wait_child_bounded(
    child: &mut std::process::Child,
    dur: std::time::Duration,
) -> Result<(std::process::ExitStatus, String, String), String> {
    use wait_timeout::ChildExt;
    // 先接管管道：读线程各自排空到 EOF
    let out = child.stdout.take().map(drain_pipe);
    let err = child.stderr.take().map(drain_pipe);
    // 正常退出：直接子进程已结束、自己的输出必然写完 ⇒ 无条件收，**不截断**大输出
    //
    // 【上条论断的失效条件（issue #126 登记，2026-10-14）——任一成立，本分支重回无界等待】
    //  1. `run_cli_with_timeout` 的 spawn 形态从「直接可执行文件」改为经 shell
    //     （`sh -c` / `cmd /c`）或改调可能 fork 的包装命令（脚本 / `sudo` / 代理进程）——
    //     孙进程继承管道写端时，EOF 只在全部写端持有者退出后才来（同上第 3 条纪律的机制）；
    //  2. 后果会放大：本路径在轮询线程上（`ensure_poller` 见旧线程未退出就不新起），
    //     一次悬挂 = 轮询永久失去自愈/重验。
    //  若将来确需给本分支加界：用「宽限内收满已知字节数即返回，否则继续无界等」的降级，
    //  **不要**直接套超时分支的 [`collect_bounded`]——500ms 宽限会截断本要解析的大输出。
    let collect = |rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>| {
        rx.map(|rx| rx.recv().unwrap_or_default())
            .unwrap_or_default()
    };
    match child.wait_timeout(dur) {
        Ok(Some(status)) => Ok((
            status,
            String::from_utf8_lossy(&collect(out)).to_string(),
            String::from_utf8_lossy(&collect(err)).to_string(),
        )),
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait(); // 收尸（防僵尸）
            let _ = collect_bounded(out);
            let _ = collect_bounded(err);
            Err(format!(
                "TS_CLI_TIMEOUT: 调用 Tailscale 超时（{} 秒未返回，已终止该子进程；该命令可能正在等待人工确认）",
                dur.as_secs()
            ))
        }
        Err(e) => Err(format!("TS_WAIT_FAIL: 等待 Tailscale 进程失败: {e}")),
    }
}

/// 超时分支给读线程的**有界宽限**（见 [`wait_child_bounded`] 第 3 条纪律）。
///
/// 直接子进程已被 kill + 收尸，正常情形下管道立刻 EOF，本宽限只是让收线程有机会退出、
/// 不泄漏 fd；一旦孙进程仍持有写端，绝不在此无限等待。
const DRAIN_GRACE_AFTER_KILL: std::time::Duration = std::time::Duration::from_millis(500);

/// 超时分支收输出：**只给有界宽限，拿不到就放弃**（输出本就随 Err 丢弃）。
///
/// 若孙进程仍持有管道写端，读线程会一直阻塞在 `read_to_end`；这里放弃后该线程被**游离**
/// （detach），直到持有写端的进程退出才自然结束——这是"有界等待"与"不泄漏线程"之间
/// 必须做的取舍：I-4 要求的是**等待有界**，不能为了收线程而把 UI 关键路径打回无界。
fn collect_bounded(rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>) -> Vec<u8> {
    rx.and_then(|rx| rx.recv_timeout(DRAIN_GRACE_AFTER_KILL).ok())
        .unwrap_or_default()
}

/// 管道排空线程（`wait_child_bounded` 用；见其注释的第 2 条纪律）。
///
/// 返回 `Receiver` 而非 `JoinHandle`：调用方在超时分支需要一个**有界宽限**（见
/// [`collect_bounded`]）——`JoinHandle::join` 没有超时版本，用它就会把无界等待引回来。
fn drain_pipe(mut p: impl std::io::Read + Send + 'static) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut p, &mut buf);
        let _ = tx.send(buf); // 只有读到 EOF 才送达；接收端已放弃则发送失败（忽略）
    });
    rx
}

/// 统一的 CLI 调用口。macOS 的 GUI 版兼作 CLI，**必须置 `TAILSCALE_BE_CLI=1`**
/// 才会走命令分发（实测）；Windows 是独立 CLI，无此需求。
/// **绝不 sudo**：本模块用到的命令都不需要管理员权限。
/// **I-4：等待有界**（[`cli_timeout_for`] 分档 + [`wait_child_bounded`] 超时 kill）——
/// 旧实现 `Command::output()` 无超时无 kill，一次挂住的 CLI 就能让轮询线程
/// （`ensure_poller` 见旧线程未退出就不新起）**永久失去自愈/重验**。
pub(super) fn run_cli(args: &[&str]) -> Result<String, String> {
    #[cfg(test)]
    if let Some(f) = super::RUN_CLI_OVERRIDE.lock().unwrap().as_ref() {
        return f(args); // 测试缝：注入 CLI 输出（零网络零进程），与 set_ts_snapshot 同纪律
    }
    run_cli_with_timeout(args, cli_timeout_for(args))
}

/// 显式超时版（测试直接调它验证有界等待；生产经 [`run_cli`] 按 [`cli_timeout_for`] 分档）
pub(super) fn run_cli_with_timeout(
    args: &[&str],
    dur: std::time::Duration,
) -> Result<String, String> {
    use std::process::Stdio;
    let bin = find_cli().ok_or_else(|| "未检测到 Tailscale（尚未安装）".to_string())?;
    let mut cmd = std::process::Command::new(bin);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    // 防闪窗（CREATE_NO_WINDOW）：本函数是轮询热路径（5–60s 一次），漏加就是连环黑窗
    cmd.no_window();
    #[cfg(target_os = "macos")]
    cmd.env("TAILSCALE_BE_CLI", "1");
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("TS_CLI_FAIL: 调用 Tailscale 失败: {e}"))?;
    let (status, stdout, stderr) = wait_child_bounded(&mut child, dur)?;
    if !status.success() {
        return Err(cli_error(args, &stderr, status.code()));
    }
    Ok(stdout)
}

// ============================================================
// 快照（复用 tunnel::ChannelStatus 这个通用形状 {running,url,error}）
// ============================================================

/// **为什么不把 tailscale 塞进 `tunnel::TunnelStatus`**（测绘报告曾建议扩那个结构）：
/// `TunnelStatus` 装的是**隧道**（有前台子进程、有 stderr 地址解析、有退避重启），
/// 而本通道**没有子进程**、状态来自 CLI 查询——塞进去会让一个语义明确的类型变杂物箱，
/// 并让 tailscale 的运行态写入必须绕经 `tunnel.rs`。这里**复用的是类型与测试缝形态**，
/// 不是那个容器。`channels_payload` 因此多收一个参数（已在其签名里写明）。
static TS_SNAPSHOT: once_cell::sync::Lazy<std::sync::Mutex<crate::remote::tunnel::ChannelStatus>> =
    once_cell::sync::Lazy::new(|| {
        std::sync::Mutex::new(crate::remote::tunnel::ChannelStatus::default())
    });

pub(crate) fn ts_snapshot() -> crate::remote::tunnel::ChannelStatus {
    TS_SNAPSHOT.lock().unwrap().clone()
}

/// 快照写入口（形状与 `tunnel::set_snapshot` 同级同形，但**不是测试专用**——B-M9：
/// 注释不得宣称与代码不符的约束）。生产写入点**五处**，都在「本地宣称必须立刻收口」
/// 的纪律下：① [`stop_inner_outcome`]（清运行态）；② [`store_tick`]（轮询轮现算运行态）；
/// ③ [`record_deferred_open_failure`]（延迟补开通失败当轮上墙，I3）；④
/// [`retire_if_still_off`]（轮询线程退役清零，B-I3）；⑤ `mod.rs` 的启停失败路径
/// （`record_ts_failure`：把守卫拒绝/CLI 失败写进 `error` 供卡面展示，与开关路径同口径）。
/// 测试用它注入夹具，**用后必须还原默认值**（见 tests::teardown_globals）
pub(crate) fn set_ts_snapshot(f: impl FnOnce(&mut crate::remote::tunnel::ChannelStatus)) {
    f(&mut TS_SNAPSHOT.lock().unwrap());
}

/// 生命周期作废校验态（start / stop 两个入口共用，§C3 要求 7「校验态必须随生命周期
/// 失效」）：**旧的 `Verified` 不得跨生命周期存活**——`funnel reset`（stop 与自愈都发）
/// 恰恰是 §C3 实测事故里「记录未发布」的触发动作，校验态跨它存活，界面就会把一个刚被
/// 自己撤掉的地址报成可用，向导 verify 步还挂着「已完成」，用户没有任何再验提示。
///
/// 为什么选「直接置 Unverified」而不是「存 verified_at + 年龄上限」：年龄上限只是把
/// 误报窗口从「永久」缩到「N 秒」，**在 stop→start 之后依然会把旧地址报成可用**；
/// 而生命周期入口是**已知的失效时刻**（我们亲手改动了 Funnel 配置），没有理由留窗口。
/// 重开后的校验由轮询守护在首个轮询窗内自动发起（见 [`ensure_poller`]），用户不必手点。
pub(super) fn invalidate_reachability() {
    set_reachability(Reachability::Unverified);
    *LAST_REVERIFY.lock().unwrap() = None;
}

// ============================================================
// 通道生命周期（与 tunnel::{start_channel,stop_channel,stop_all} 对称）
// 守护模型 = 周期轮询：`funnel --bg` 立即返回、配置由常驻 tailscaled 持有，
// 没有子进程可守——运行态由轮询 status/funnel status 现算写进快照
// ============================================================

/// 轮询周期（秒）：Funnel 起停是低频动作，5s 足够卡面跟手
pub(super) const POLL_SECS: u64 = 5;

/// 轮询周期上限（秒，B-M3）：固定 5s × 2 次 CLI/轮 ≈ 24 次进程派生/分钟，且**持续失败
/// （未登录 / 后端常年忙）照打**。连续同结果时指数退避到本值，稳定态降到 1–2 次/分钟
pub(super) const POLL_MAX_SECS: u64 = 60;

/// 下一轮间隔（纯函数，可测——B-M3）：状态与上轮**逐字相同**（含连续失败：错误文案
/// 相同也算同结果）→ 间隔翻倍至 [`POLL_MAX_SECS`]；任何变化 → 回到 [`POLL_SECS`]
///（卡面跟手优先：起停 / 上下线必须尽快反映）。
/// 选「同结果退避」而非「只对失败退避」：稳定运行态同样不需要 5s 粒度
pub(super) fn next_poll_secs(current: u64, changed: bool) -> u64 {
    if changed {
        POLL_SECS
    } else {
        (current * 2).min(POLL_MAX_SECS)
    }
}

/// 快照逐字比较（B-M3 的退避判据；`ChannelStatus` 没实现 PartialEq，字段少就地比）
pub(super) fn same_snapshot(
    a: &crate::remote::tunnel::ChannelStatus,
    b: &crate::remote::tunnel::ChannelStatus,
) -> bool {
    a.running == b.running && a.url == b.url && a.error == b.error
}

/// 已 Verified 通道的重验周期（秒）：上游 Funnel/解析发布是 beta 形态，发布可撤——
/// 「先通后不通」必须能被发现并撤下地址（§C3 长期化，一等要求而非一次性）
pub(super) const REVERIFY_SECS: u64 = 60;

/// 重验到期判定（纯函数，可测）：从未重验过（None）或距上次 ≥[`REVERIFY_SECS`] 才到期
pub(super) fn reverify_due(last: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    match last {
        None => true,
        Some(t) => now.duration_since(t) >= std::time::Duration::from_secs(REVERIFY_SECS),
    }
}

/// 上一轮重验时刻（None = 从未重验，立即到期）。**为什么是全局而不是轮询线程的局部量**
/// （B-I2）：旧线程可能跨 stop→start 存活（[`ensure_poller`] 见到未退出的旧线程就不
/// 新起），局部量会把上一周期的时刻带进新周期——重开后要等满 60s 才校验，地址跟着
/// 不显示。生命周期入口经 [`invalidate_reachability`] 把它清零，保证重开后首个轮询窗
/// 即校验（用户开了开关就应当在数秒内看到地址或如实的「尚未生效」）
pub(super) static LAST_REVERIFY: once_cell::sync::Lazy<
    std::sync::Mutex<Option<std::time::Instant>>,
> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(None));

/// 本轮是否该重验（纯函数，可测——B-I2 的核心判据）：**只要通道在运行且窗口到期就
/// 校验，不分当前校验态**。旧口径（`reachability == Verified && due`）有两个实测后果：
/// ① 一旦 `reverify_with` 把它翻成 Failed，条件恒假 → **再无自动重试**，只能用户手点；
/// ② 每次 兔维斯 重启校验态复位 Unverified，而 `restore_enabled_tunnels` 只 refresh 不校验
/// → 载荷恒报「尚未生效」、地址恒不显示，与 §C1「设置页常驻固定地址」「重启后地址不变」
/// 直接冲突。故本判据**刻意不读校验态**——读它就是退回旧口径。
pub(super) fn should_reverify(running: bool, due: bool) -> bool {
    running && due
}

/// 期望态（Some(端口) = 用户开着）：轮询线程的开关信号——起停都发 CLI 命令，
/// 本态只记「用户要它在开」，运行态永远以轮询现算为准（不维护第二真值）
pub(super) static DESIRED: once_cell::sync::Lazy<std::sync::Mutex<Option<u16>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(None));

/// 轮询线程句柄（单例：已在轮询则不重复 spawn；期望态清空后线程自行退役）
static POLLER: once_cell::sync::Lazy<std::sync::Mutex<Option<std::thread::JoinHandle<()>>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(None));

/// serve 配置的归属分类（B-I5/B-M4 **判据的原话**——注释与判据必须一致，见
/// [`disable_funnel`]）。判据 = 逐条 handler 的 `Proxy` **精确**指向本机 兔维斯 端口：
/// - `Ours` = 每一条都是我们的 → 撤销安全；
/// - `Mixed` = 我们那路在 + 还有别人的条目 → **不拒绝**（加严会让公开暴露撤不掉，
///   见 disable_funnel 的权衡登记），但把将一并被清除的条目数如实交出去；
/// - `Foreign` = 一条我们的都没有（含「非空但读不懂」的未知形态）→ 拒绝 reset；
/// - `Absent` = 本来就没有配置（实测未开通 = `{}`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ServeOwnership {
    Absent,
    Ours,
    Mixed { others: usize },
    Foreign { others: usize },
}

impl ServeOwnership {
    /// 完全不含 兔维斯 那路 → 守卫拒绝（`foreign_serve_config` 的公开语义）
    pub(super) fn is_foreign(self) -> bool {
        matches!(self, Self::Foreign { .. })
    }

    /// 将随 `funnel reset` **一并被清除**的非 兔维斯 条目数（撤销回执/日志明示的数据源）
    pub(super) fn cleared_others(self) -> usize {
        match self {
            Self::Mixed { others } | Self::Foreign { others } => others,
            Self::Absent | Self::Ours => 0,
        }
    }
}

/// 单条 handler 是否就是 兔维斯 那一路（纯函数）：取 `Proxy` 的 host:port，**端口精确相等
/// 且 host 是回环**才算自己。旧判据用「整份 JSON 序列化后包含 `http://127.0.0.1:PORT`」
/// ——端口 ≤6553 时外来的 8080 会被端口 808 的 兔维斯 认成自己人（子串撞车，B-M4），
/// 于是 `funnel reset` 会清掉别人的配置。host 认回环的常见拼写（不把拼写当契约）
fn handler_is_ours(handler: &serde_json::Value, mam_port: u16) -> bool {
    let Some(proxy) = handler.get("Proxy").and_then(|p| p.as_str()) else {
        // 无 Proxy 的 handler（Text / FileServer 等形态）= 别人的条目
        return false;
    };
    let rest = proxy.split_once("://").map(|(_, r)| r).unwrap_or(proxy);
    let authority = rest.split('/').next().unwrap_or("");
    let Some((host, port)) = authority.rsplit_once(':') else {
        return false;
    };
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "[::1]"
        || host == "::1";
    loopback && port.parse::<u16>() == Ok(mam_port)
}

/// serve 配置里的一条 handler（**给人看的描述，B1 确认框的数据源**）。
/// **不进任何判定**——归属判定仍只认 [`handler_is_ours`]（与 §G3「展示字段与鉴权解耦」
/// 同纪律：描述文案不得参与安全逻辑）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ServeEntry {
    /// 是否就是 兔维斯 那一路（判据 = [`handler_is_ours`]，与归属分类同源）
    pub ours: bool,
    /// 人类可读描述（主机 + 路径 + 转发目标），例如
    /// `x.ts.net:443 /media → http://127.0.0.1:7000`
    pub label: String,
}

/// 一条 handler 的转发目标（描述用）：有 `Proxy` 用 Proxy；没有就用它的字段名
/// （FileServer / Text 等上游形态）；全无字段则如实写「未知目标」——**不猜**
fn handler_target(handler: &serde_json::Value) -> String {
    if let Some(p) = handler.get("Proxy").and_then(|p| p.as_str()) {
        return p.to_string();
    }
    let kinds: Vec<&str> = handler
        .as_object()
        .map(|o| o.keys().map(String::as_str).collect())
        .unwrap_or_default();
    if kinds.is_empty() {
        "（未知转发目标）".to_string()
    } else {
        kinds.join("+")
    }
}

/// serve 配置 → 逐条条目（**含归属**，B1）。
/// 解析不了 / 形态不认识时如实登记一条「读不懂」条目（归属 = 非我们那路，与
/// [`serve_ownership`] 的 fail-closed 同口径）——确认框既不漏报可识别条目，
/// 也不把读不懂的配置说成「没有条目」。
pub(super) fn serve_entries(funnel_json: &str, mam_port: u16) -> Vec<ServeEntry> {
    let trimmed = funnel_json.trim();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return if trimmed.is_empty() {
            Vec::new()
        } else {
            vec![ServeEntry {
                ours: false,
                label: "（配置形态无法识别——按非 兔维斯 条目处理）".to_string(),
            }]
        };
    };
    let mut out: Vec<ServeEntry> = Vec::new();
    // 实测形状：Web → { "<host>:443" → { "Handlers" → { "/" → { "Proxy": "…" } } } }
    if let Some(web) = v.get("Web").and_then(|w| w.as_object()) {
        for (host, entry) in web {
            let Some(handlers) = entry.get("Handlers").and_then(|h| h.as_object()) else {
                out.push(ServeEntry {
                    ours: false,
                    label: format!("{host}（该条目形态无法识别）"),
                });
                continue;
            };
            for (path, handler) in handlers {
                out.push(ServeEntry {
                    ours: handler_is_ours(handler, mam_port),
                    label: format!("{host} {path} → {}", handler_target(handler)),
                });
            }
        }
    }
    out
}

/// serve 配置分类（纯函数；`foreign_serve_config` 的判据内核）。
/// **fail-closed 收口**（修复轮 2 指针①的语义保留）：非空但解析不了 → `Foreign`；
/// 能解析但一条 handler 都没有（形态不认识）→ 同样 `Foreign`——绝不覆盖读不懂的配置。
/// **判据与 [`serve_entries`] 同源**（条目表 = 唯一事实源，计数由它派生，不会两处漂移）
pub(super) fn serve_ownership(funnel_json: &str, mam_port: u16) -> ServeOwnership {
    let trimmed = funnel_json.trim();
    if trimmed.is_empty() {
        return ServeOwnership::Absent; // 空串 = 没有配置可覆盖（守卫已 fail-closed）
    }
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() && !funnel_active(trimmed) {
        return ServeOwnership::Absent; // 可解析但空（实测 `{}`）= 本来就没有配置
    }
    let entries = serve_entries(trimmed, mam_port);
    let ours = entries.iter().filter(|e| e.ours).count();
    let others = entries.len() - ours;
    if ours == 0 {
        // 含「形态读不懂」：serve_entries 会给一条非 ours 条目（others ≥ 1）——
        // 比旧的 others:0 更如实（确实有一条我们读不懂的东西会被清掉）
        return ServeOwnership::Foreign { others };
    }
    if others == 0 {
        ServeOwnership::Ours
    } else {
        ServeOwnership::Mixed { others }
    }
}

/// 开通/撤销/自愈前的守卫：若**当前已有 serve/Funnel 配置**且**完全不含 兔维斯 那一路**，
/// 拒绝执行并如实报错——`funnel reset` 会清掉整份 serve 配置，静默清掉用户的
/// 自建配置是不可接受的副作用。
/// **判据的原话以 [`serve_ownership`] 为准**；本函数只回答「是否完全不含我们那路」。
/// ⚠️ **已知缺口（如实登记，勿在注释里宣称判据不给的保护）**：`Mixed`（我们那路 +
/// 用户自建条目）**放行** reset，**用户的条目会被一并清除**。这是有意的取舍：加严
/// 会让「公开暴露撤不掉」（守卫拒绝 → 兔维斯 的 Funnel 配置留在公网且 UI 无出路），
/// 失败模式比「多清一条用户条目」更糟（独立评审确认该权衡成立）。缓解 = 撤销回执与
/// audit 日志明示被清条目数（[`ServeOwnership::cleared_others`]）+ `disable_force`
/// 之外的手动出口（错误文案里给出终端命令）
pub(super) fn foreign_serve_config(funnel_json: &str, mam_port: u16) -> bool {
    serve_ownership(funnel_json, mam_port).is_foreign()
}

/// 轮询单轮的**状态映射**（纯函数，可测——周期循环本身零测试价值不测）：
/// (status 结果, funnel 结果) → 快照内容。fail-closed 链：读不到就如实报错，
/// 绝不把「未登录/没开 Funnel」伪装成运行，也不把他人配置当成自己的
pub(super) fn map_status(
    status: Result<TsStatus, String>,
    funnel: Result<String, String>,
    mam_port: u16,
) -> crate::remote::tunnel::ChannelStatus {
    let failed = |e: String| crate::remote::tunnel::ChannelStatus {
        running: false,
        url: None,
        error: Some(e),
    };
    let st = match status {
        Ok(s) => s,
        Err(e) => return failed(format!("TS_STATUS_READ_FAIL: 读取 Tailscale 状态失败: {e}")),
    };
    match backend_phase(&st) {
        // W-A（2026-10-07 真机重启实测）：后端仍在初始化 = **开机恢复窗口**，
        // 既不是故障也不是"未开通"——快照必须干净（running=false / error=None），
        // "恢复中"语义由可达性态承载（[`super::reach::Reachability::Recovering`]，
        // 载荷据此给 1–2 分钟口径）。旧实现写「Tailscale 未就绪（NoState）」，
        // 把每次开机的正常过程报成红色故障。
        BackendPhase::Initializing => return crate::remote::tunnel::ChannelStatus::default(),
        // 就绪 / 其余（NeedsLogin / Stopped）→ 原口径：如实说清是「待登录」还是「未就绪」
        BackendPhase::Ready | BackendPhase::Other => {}
    }
    if st.backend_state != "Running" {
        let hint = if st.auth_url.is_empty() {
            format!("Tailscale 未就绪（{}）", st.backend_state)
        } else {
            "Tailscale 待登录".to_string()
        };
        return failed(hint);
    }
    let funnel_json = match funnel {
        Ok(f) => f,
        Err(e) => return failed(format!("TS_FUNNEL_READ_FAIL: 读取 Funnel 状态失败: {e}")),
    };
    if !funnel_active(&funnel_json) {
        // 已登录但 Funnel 未开通：引导态而非故障态（error 留空，向导据此引导）
        return crate::remote::tunnel::ChannelStatus::default();
    }
    if foreign_serve_config(&funnel_json, mam_port) {
        return failed("Funnel 已被其他 serve 配置占用（非 兔维斯）".into());
    }
    // 机器域名：DNSName 优先（带结尾点，board_url_from_dns_name 剥），
    // 回落 CertDomains[0]（不带点的同值域名）
    let dns = if st.dns_name.is_empty() {
        st.cert_domains.first().cloned().unwrap_or_default()
    } else {
        st.dns_name
    };
    if dns.is_empty() {
        return failed("已开通 Funnel 但读不到机器域名".into());
    }
    crate::remote::tunnel::ChannelStatus {
        running: true,
        url: Some(board_url_from_dns_name(&dns)),
        error: None,
    }
}

/// 一轮 CLI 读数 + 由它现算的快照（**读与写分开**：延迟补开通的判据要用同一轮读数，
/// 不能为了判据再派生一遍进程——B-M11 的"一次读数多处复用"纪律）。
pub(super) struct TsTick {
    pub(super) status: Result<TsStatus, String>,
    pub(super) funnel: Result<String, String>,
    pub(super) snapshot: crate::remote::tunnel::ChannelStatus,
}

/// 现算一轮读数（**不写任何全局态**）
pub(super) fn read_tick(port: u16) -> TsTick {
    let status = run_cli(&["status", "--json"]).and_then(|j| parse_status(&j));
    let funnel = run_cli(&["funnel", "status", "--json"]);
    let snapshot = map_status(status.clone(), funnel.clone(), port);
    TsTick {
        status,
        funnel,
        snapshot,
    }
}

/// 延迟补开通失败的**留痕位（sticky）**：`map_status` 每轮重算会把快照 error 覆盖掉，
/// 失败若只写在快照里就只闪一个轮询窗——用户看到的是"没反应"（B-M2 已确立的口径：
/// 失败必须上墙）。故单独留位，由 [`store_tick`] 合并进快照；配置确认回来时清位。
static DEFERRED_OPEN_ERROR: once_cell::sync::Lazy<std::sync::Mutex<Option<String>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(None));

/// 读出留痕（测试与载荷断言用）
pub(super) fn deferred_open_error() -> Option<String> {
    DEFERRED_OPEN_ERROR.lock().unwrap().clone()
}

/// 写/清留痕（清 = 传 None）
pub(super) fn set_deferred_open_error(e: Option<String>) {
    *DEFERRED_OPEN_ERROR.lock().unwrap() = e;
}

/// 记一次延迟补开通失败：**同一轮就写进快照**（用户立刻看到）+ **留痕**（后续每轮
/// `map_status` 重算会把快照 error 清掉，留痕负责把它顶回来，直到配置确认回来/停机）。
///
/// **I3（2026-10-07 评审）**：写快照同样要过 `DESIRED` 复查（与 [`store_tick`] 同源纪律）
/// ——「补开通失败」是**开着这条通道**的失败；用户已把通道关掉时还把它写进快照，卡面
/// 就会对一个刚关掉的通道报这条错（且 `DESIRED=None` 期间无 `store_tick` 纠正）。
/// 留痕照记（下次开启时 `start_channel_inner` 会翻页清零），只是不写给已关闭通道。
pub(super) fn record_deferred_open_failure(port: u16, msg: String) {
    set_deferred_open_error(Some(msg.clone()));
    let desired = DESIRED.lock().unwrap();
    if *desired == Some(port) {
        set_ts_snapshot(|s| {
            s.running = false;
            s.error = Some(msg);
        });
    }
}

/// 写快照（**期望态复查与写入在同一临界区**，B-I3：先查后写若分两次取锁，会给停机路径
/// 留下「查到 Some → stop 清空 → 本函数把运行态写回」的窗口，卡面在停机后继续宣称运行，
/// 最长一个轮询窗）。锁序：DESIRED → TS_SNAPSHOT（两把锁从无反向嵌套）。
///
/// 顺带做两件与"后端阶段"有关、必须与快照同步的事：
/// ① 把可达性置/清「恢复中」（[`super::reach`]）——它是"恢复窗口"语义的对外载体；
/// ② 合并延迟补开通的失败留痕（见 [`DEFERRED_OPEN_ERROR`]）。
/// **I3（2026-10-07 评审）**：①与②都必须**在 `DESIRED` 复查之内**（见下），
/// 与快照同一临界区——否则「读到 Some → 用户关掉通道 → 本函数把恢复中/失败写回」
/// 会把一个刚被关掉的通道渲染成「恢复中（无需任何操作）」或「补开通失败」并滞留
/// （`DESIRED=None` 期间再无 `store_tick` 可纠正，退役分支也不碰可达性）。
pub(super) fn store_tick(port: u16, tick: &TsTick) {
    let mut snap = tick.snapshot.clone();
    // 配置确认在（快照宣称运行）→ 延迟补开通这一页翻过去了，留痕清位
    if snap.running {
        set_deferred_open_error(None);
    }
    if snap.error.is_none() {
        snap.error = deferred_open_error();
    }
    let desired = DESIRED.lock().unwrap();
    if *desired == Some(port) {
        // **后端阶段 → 可达性语义（恢复窗口 ↔ 正常流程）**：与快照同临界区（B-I3 同源纪律）。
        // 判据只有「本通道仍开着」才写——关掉的通道的可达性由 stop_inner_outcome 的
        // invalidate_reachability 独家负责，本函数不得抢写。
        let now = std::time::Instant::now();
        match &tick.status {
            Ok(s) if backend_phase(s) == BackendPhase::Initializing => {
                // **M1（2026-10-07 评审 Minor）**：返回值 = 已超「后端持续未就绪」上界时的
                // **如实原因**——同一轮写进快照 error，不再无界地说「通常 1–2 分钟」
                if let Some(reason) = super::reach::note_backend_initializing(&s.backend_state, now)
                {
                    snap.error = Some(reason);
                }
            }
            Ok(s) if backend_phase(s) == BackendPhase::Ready => {
                super::reach::note_backend_ready(now)
            }
            _ => {}
        }
        set_ts_snapshot(|s| *s = snap);
    }
}

/// 轮询一轮：status + funnel status 现算 → **期望态仍在才写快照**（[`read_tick`] +
/// [`store_tick`] 的单轮组合；起停入口也用它"起完立即现算一轮"）。
/// （`pub(super)`：I-4 的「超时 → 快照 error 上墙」用例要压这条链本身，而不是它的下游替代品）
pub(super) fn refresh_once(port: u16) {
    let tick = read_tick(port);
    store_tick(port, &tick);
}

/// 「期望态已清空」分支的退役内核（B-I3）：返回 true = 本线程现在可以退役。
/// **调用方必须已持 `DESIRED` 锁**——复查与清零要在同一临界区内完成。
///
/// 为什么必须在同一把锁内复查（µs 级窗口，后果永久）：线程读到 `None` 后若被抢占，
/// 主线程趁隙 `start_channel_inner`（置 `DESIRED=Some` → [`refresh_once`] 写好运行快照
/// → [`ensure_poller`] 见本线程 `!is_finished()` 故**不新起**），本线程恢复后无条件
/// 清零快照 → 通道永远显示未运行、**无自愈路径**（只有再切一次开关才恢复）。
///
/// 为什么只清「没有 error 的快照」：清零若落在调用方（`mod.rs` 开关路径）写入守卫
/// 拒绝 / CLI 失败之后，就会把诊断擦成「尚未生效」——用户看到的不是「外来配置占用」
/// 而是"没反应"。带 error 的快照是写给用户看的，本线程无权擦。
pub(super) fn retire_if_still_off(desired: &mut Option<u16>) -> bool {
    if desired.is_some() {
        return false; // 期间已被重新开启：本轮不清零，交回循环继续守护
    }
    set_ts_snapshot(|s| {
        if s.error.is_none() {
            *s = crate::remote::tunnel::ChannelStatus::default();
        }
    });
    true
}

/// 轮询线程的**单轮状态**（线程局部：随线程退役消失，不跨生命周期残留；B-M3 退避状态）。
#[derive(Clone)]
pub(super) struct PollerState {
    /// 下一轮睡多久（同结果翻倍至 [`POLL_MAX_SECS`]）
    pub interval: u64,
    /// 上一轮快照（逐字比较判「有没有变化」）
    pub last: Option<crate::remote::tunnel::ChannelStatus>,
    /// **连续**观测到后端 `Running` 的轮数（延迟补开通判据要用，见 [`deferred_open_due`]）
    pub ready_streak: u32,
    /// 本次「配置缺失期」是否已试过补开通（失败不无限重试）
    pub deferred_attempted: bool,
    /// 上一轮的后端相位（**M2**：相位跃迁本身是「变化」信号，见 [`poller_tick`]）
    pub last_phase: Option<BackendPhase>,
}

impl Default for PollerState {
    fn default() -> Self {
        Self {
            interval: POLL_SECS,
            last: None,
            ready_streak: 0,
            deferred_attempted: false,
            last_phase: None,
        }
    }
}

/// 延迟补开通：**后端连续就绪**轮数门槛。为什么不是"第一眼 Running 就算"——
/// 实测 T+73s 才是配置逐字段回来的时刻，第一眼 Running 时 `funnel status --json`
/// 仍可能是 `{}`（后端刚起、serve 配置尚未加载）。多等一轮就把"还没加载完"和
/// "真的没有"分开了——这正是报告要求的"确认"而非"看一眼"。
///
/// **M2（2026-10-07 评审 Minor）代价订正**：原注释写"多等一轮（首个轮询窗 5s 量级）"，
/// 与代码不符——恢复窗口内快照恒为初始化的干净口径（`default`），与"配置真缺失"的
/// 快照**逐字相同** ⇒ `changed=false` ⇒ 轮询间隔按 5→10→20→40→60 退避，于是"多等一轮"
/// 的真实代价是**退避后的那一轮**（就绪后最坏 ≈2 个窗、约 2 分钟）。现在
/// [`poller_tick`] 把**后端相位跃迁**也算作变化（就绪立即回到 [`POLL_SECS`]），
/// 于是这里的"多等一轮"才真正是 5s 量级。
pub(super) const DEFERRED_READY_STREAK: u32 = 2;

/// **延迟补开通判据**（W-A 的收尾半步，纯判据 + 记账，动作在 [`poller_tick`]）：
/// 只在**同时**满足下列条件时才返回 true——
/// ① 后端读数成功且 `BackendState == "Running"`：**恢复窗口（NoState/Starting）内一律不判**；
/// ② 连续 ≥ [`DEFERRED_READY_STREAK`] 轮就绪（"确认"，不是"看一眼"）；
/// ③ Funnel 读数成功且归属**确认缺失**（`Absent`）：读不到 / 读不懂（`Foreign`）都不判，
///    fail-closed——宁可不开，也不覆盖读不懂的配置；
/// ④ 快照本身没有 error（有错先把真原因给用户看，不叠加动作）；
/// ⑤ 本次缺失期还没试过（失败不无限重试）。
///
/// 记账：`ready_streak` 随每轮就绪自增、任何非就绪轮归零；`deferred_attempted` 在
/// 配置确认回来（不缺失）时复位，于是"下一次真的缺失"仍会被补开通。
pub(super) fn deferred_open_due(st: &mut PollerState, port: u16, tick: &TsTick) -> bool {
    let ready = matches!(&tick.status, Ok(s) if backend_phase(s) == BackendPhase::Ready);
    st.ready_streak = if ready { st.ready_streak + 1 } else { 0 };
    let absent =
        matches!(&tick.funnel, Ok(j) if serve_ownership(j, port) == ServeOwnership::Absent);
    if !absent {
        st.deferred_attempted = false; // 配置回来了（或读不懂）→ 这一页翻过去
        return false;
    }
    if !ready || st.ready_streak < DEFERRED_READY_STREAK || st.deferred_attempted {
        return false;
    }
    if tick.snapshot.error.is_some() {
        return false; // 快照已经带错：先让用户看到真原因
    }
    st.deferred_attempted = true;
    true
}

/// 轮询线程的**单轮**（线程循环与 60s 重验用例**共用同一实现**——M-6）。返回 true = 本线程
/// 现在可以退役。
///
/// **为什么抽成函数（M-6 评审判定）**：旧用例（`sixty_second_reverify_cycle_...`）**自己
/// 重写了一遍循环模型**——直接调 `should_reverify` / `reverify_due` / `reverify_with`，
/// 于是把 `should_reverify` 的调用从线程体里删掉，用例**照样绿**（它压的是自己那份判据）。
/// 判据与动作现在只此一处，用例驱动本函数：删调用必红。
///
/// `now` 由调用方给（线程传 `Instant::now()`，用例传虚拟时钟）；`reverify` 是重验出口
/// （生产 = `poll_reverify`：测试构建下没有假探针就返回 None，守住零网络红线）。
pub(super) fn poller_tick(
    st: &mut PollerState,
    now: std::time::Instant,
    reverify: &mut dyn FnMut(&str) -> Option<Reachability>,
) -> bool {
    let mut changed = true; // 退役分支也视为「变化」→ 重置周期
    let desired = *DESIRED.lock().unwrap();
    match desired {
        Some(port) => {
            let tick = read_tick(port); // 一轮读数喂给快照与延迟补开通判据（B-M11）
            store_tick(port, &tick);
            let snap = ts_snapshot();
            // W-A 收尾半步：恢复窗口内不判不写；后端**确认**就绪且配置**确认**缺失才补开通
            if deferred_open_due(st, port, &tick) {
                match enable_funnel(port) {
                    Ok(FunnelOutcome::Opened(_)) => {
                        set_deferred_open_error(None);
                        crate::remote::events::audit(
                            "tailscale_funnel_reopened",
                            "reason=config_absent_after_backend_ready",
                        );
                        refresh_once(port); // 立即现算，不等下一轮
                    }
                    // 期间后端又缩回初始化 → 不记账（`deferred_open_due` 已置位，故显式复位），
                    // 下一轮就绪时重试
                    Ok(FunnelOutcome::Deferred { .. }) => {
                        st.deferred_attempted = false;
                    }
                    Err(e) => {
                        // 失败必须上墙且留痕（sticky——否则只闪一个轮询窗，用户看到"没反应"）
                        record_deferred_open_failure(
                            port,
                            format!("TS_DEFERRED_OPEN_FAIL: 开机后自动补开通 Funnel 失败: {e}"),
                        );
                        log::warn!("tailscale 延迟补开通失败: {e}");
                    }
                }
            }
            // **M2（2026-10-07 评审 Minor）**：退避判据除了「快照逐字变了」，还要看
            // **后端相位跃迁**——恢复窗口内快照恒为 `default`（与"配置真缺失"逐字相同），
            // 只看快照会让间隔一路退避到 60s，而补开通要先"连续 2 轮就绪"，于是最坏在
            // 就绪后 ~2 个窗（~2 分钟）才发生（与注释里"多等一轮 5s"不符）。相位变了
            // （NoState/Starting → Running 或反向）就重置退避，就绪复评立刻按 5s 走。
            let phase = tick.status.as_ref().ok().map(backend_phase);
            let phase_changed = st.last_phase != phase;
            st.last_phase = phase;
            let snap_changed = st
                .last
                .as_ref()
                .map(|p| !same_snapshot(p, &snap))
                .unwrap_or(true);
            changed = snap_changed || phase_changed;
            st.last = Some(snap.clone());
            let due = reverify_due(*LAST_REVERIFY.lock().unwrap(), now);
            if should_reverify(snap.running, due) {
                *LAST_REVERIFY.lock().unwrap() = Some(now);
                if let Some(host) = current_dns_host() {
                    let _ = reverify(&host);
                }
            }
        }
        None => {
            // 退役判定必须在**同一把 DESIRED 锁内**复查后再清零（B-I3，见
            // retire_if_still_off）：期间被重新开启则交回循环继续守护
            let mut d = DESIRED.lock().unwrap();
            if retire_if_still_off(&mut d) {
                return true; // 真的还关着：清零后线程退役（下次 start_channel 重新 spawn）
            }
            drop(d);
        }
    }
    st.interval = next_poll_secs(st.interval, changed);
    false
}

/// 轮询线程（单例）：睡醒 → 单轮（[`poller_tick`]）；期望态清空 → 清零快照后退役。
/// §C3 长期化（B-I2 修正口径）：**只要通道在运行且重验窗口到期就重验**（[`should_reverify`]，
/// 不分当前校验态）——翻 Failed 后仍会按窗口自动重试，重启后首个窗口即把地址点亮；
/// 事件只在「从可用跌落」时发（[`super::reach::failure_event_reason`]，防刷屏）。
/// 校验是 blocking reqwest，但本线程是专职 std::thread（命名 `mam-tailscale-poller`，
/// 便于采样定位），不在异步上下文，阻塞无碍
fn ensure_poller() {
    let mut h = POLLER.lock().unwrap();
    if h.as_ref().map(|t| !t.is_finished()).unwrap_or(false) {
        return; // 已在轮询
    }
    let spawned = std::thread::Builder::new()
        .name("mam-tailscale-poller".into())
        .spawn(move || {
            let mut st = PollerState::default();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(st.interval));
                // 测试构建下无假探针 → None（跳过，零网络红线）；生产恒 Some
                if poller_tick(&mut st, std::time::Instant::now(), &mut |h| {
                    poll_reverify(h)
                }) {
                    return;
                }
            }
        });
    match spawned {
        Ok(t) => *h = Some(t),
        // 线程创建失败不 panic：起停仍由 CLI + 各入口的 refresh_once 现算，通道可用但
        // 失去自愈/重验——如实记日志（不假装守护仍在）
        Err(e) => log::error!("tailscale 轮询线程创建失败（通道仍可用，但失去自动重验）: {e}"),
    }
}

/// 开通 Funnel 的结果（**两态而非「Err 表示没开成」**）：开机恢复窗口里"没开"不是错误，
/// 而是"还没轮到我们判断"——用 Err 表达会让 UI 把每次重启的正常过程报成故障。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum FunnelOutcome {
    /// 已开通（批准链接若有：首开时上游**可能**要求一次浏览器批准）
    Opened(Option<String>),
    /// 后端尚未 `Running`（开机恢复窗口）：**未判配置、未写任何东西**，等轮询复评
    Deferred { backend_state: String },
}

/// 开通 Funnel（模块原语，`start_channel` 与向导 funnel/enable 步共用单点）：
/// **⓪ 后端门（W-A，2026-10-07 真机重启实测）**：先读 `status --json`，
///    `BackendState != "Running"` → 返回 [`FunnelOutcome::Deferred`]，**到此为止**。
///    为什么这一门必须在守卫之前：T+58s 实测服务已 Running 但 `BackendState=NoState`、
///    `funnel status --json` **短暂为 `{}`**——先判配置就会把这个 `{}` 读成「配置丢失」
///    （守卫放行）并重开 `funnel --bg`；而 T+73s 实测 tailscaled 自己就把配置逐字段
///    恢复了。更糟的是紧接着的 `set --shields-up=false` 回读在初始化期必然失败，
///    UI 会收到「无法确认已关闭」的**假故障**（真机现象见 W-A 报告）。
/// ① 守卫：已有 serve/Funnel 配置且不是 兔维斯 的 → 拒绝（`funnel --bg` 会覆盖/混入
///    用户自建配置——绝不静默覆盖）
/// ② shields-up 关（Funnel 前置；幂等——实测 `get --json` 里键名精确为 `shields-up`）
/// ③ `funnel --bg <port>`（**A6：`--bg` 是必需的**，2026-10-07 Windows 实测——不加
///    `--bg` 时 `tailscale funnel <port>` 打印横幅 + `Press Ctrl+C to exit.` 后
///    **一直阻塞终端**，而 兔维斯 是 GUI 应用、绝不能让任何调用线程挂住；带 `--bg` 时
///    退出码 0、0.1 秒返回、无批准链接。**macOS 侧仍是预期**——本机只跑过只读命令，
///    见 [`super::wizard::write_path_verified`]）
/// 返回批准链接：首次开通时上游**可能**要求一次浏览器批准（实测两平台各执一端：
/// macOS 首开确有一次，Windows 1.102.4 零批准）——**链接出现才递出去，不出现不是错误、
/// 更不得等它**（`funnel --bg` 返回成功即算开通，下一步是等公网解析，见 §C3）；
/// 无待批链接（幂等重开/已批准/上游不要求）返回 None。Err 时 DESIRED/快照不动
///（生命周期收口由调用方负责——start_channel 与 run_step 各自处理）
pub(super) fn enable_funnel(port: u16) -> Result<FunnelOutcome, String> {
    // ⓪ 后端门（读不到状态 = Err，fail-closed：绝不拿"读不到"当"可以开"）
    let st = run_cli(&["status", "--json"]).and_then(|j| parse_status(&j))?;
    if backend_phase(&st) != BackendPhase::Ready {
        return Ok(FunnelOutcome::Deferred {
            backend_state: st.backend_state,
        });
    }
    // B-I4：守卫读数失败一律 fail-closed——旧实现 unwrap_or_default() 把「读不到」
    // 当成「没有配置」放行，CLI 一时失败就会覆盖用户自建配置
    let before = run_cli(&["funnel", "status", "--json"])
        .map_err(|e| funnel_status_unreadable("开通", &e))?;
    if let ServeOwnership::Foreign { others } = serve_ownership(&before, port) {
        return Err(foreign_serve_error("开通", others));
    }
    disable_shields_up()?;
    // **`--bg` 必须在**（A6 实测：不加会阻塞终端直到 Ctrl+C）。锁见
    // tests::every_funnel_enable_call_passes_bg_or_it_would_block_the_terminal
    run_cli(&["funnel", "--bg", &port.to_string()])?;
    // 开通动作已完成（`--bg` 返回成功即算开通）——下一步是等公网解析（§C3 的 verify
    // 轮询），**不在这里等批准**：批准链接可能存在也可能不存在（Windows 实测不存在）。
    // 有链接就交回前端给用户点，没有就直接返回 None（不是错误）。
    Ok(FunnelOutcome::Opened(extract_approval_url(
        &run_cli(&["funnel", "status"]).unwrap_or_default(),
    )))
}

/// shields-up 关（模块私有原语）：`tailscale set --shields-up=false`，幂等。
/// 偏好项键名**已实测为 `shields-up`**（`get --json`）；**写入侧尚未实测**
/// （探测提示词第 6 步），故写完必须回读确认——读不回/仍为开就如实报错，
/// 绝不把「写没写进去不知道」伪装成成功
pub(super) fn disable_shields_up() -> Result<(), String> {
    run_cli(&["set", "--shields-up=false"])?;
    let after = run_cli(&["get", "--json"])?;
    let v: serde_json::Value =
        serde_json::from_str(&after).map_err(|e| format!("回读偏好失败: {e}"))?;
    match v.get("shields-up").and_then(|x| x.as_bool()) {
        Some(false) => Ok(()),
        Some(true) => Err("「阻止传入连接」仍为开启——写入未生效".into()),
        None => Err("回读不到 shields-up 字段，无法确认已关闭".into()),
    }
}

/// 撤销 Funnel（模块私有原语）：`funnel reset` **清整份 serve 配置**——
/// 这也是本模块守卫存在的唯一理由。
///
/// **判据的原话（B-I5 ③：注释不得宣称判据不给的保护）**：守卫只拒绝
/// `ServeOwnership::Foreign`，即「整份配置里**一条**指向本机 兔维斯 端口的 handler 都没有」
/// （含非空但读不懂的未知形态）。它**不保证**「配置里只有我们那一条」——
/// `Mixed`（我们那路 + 用户自建条目）**照常放行**，**用户的条目会随这次 reset 一并
/// 被清除**。
///
/// **为什么明知会误清仍不加严（失败模式比较，独立评审确认权衡成立）**：加严 = 叠加形态下
/// 守卫拒绝 reset → 兔维斯 自己开通的 Funnel 配置留在公网、地址继续可达，而 UI 没有任何
/// 出路（用户只能去终端手敲命令）——「公开暴露撤不掉」比「多清一条用户自建路径」更糟。
///
/// **缺口如何向用户呈现**：① **撤销前**——前端先调 `disable_preview` 拿到逐条预览
/// （`serve_entries` 的条目表 + `foreign`），只要「有非 兔维斯 条目会被清」或「普通撤销会被
/// 守卫拒绝」就弹确认框逐条列出，用户点头才走 `disable_force`（B1 已接线）；
/// ② **撤销后**——回执带上**实际被清除的条目**（[`ServeResetReport::cleared_entries`]，
/// 与条数同源），卡面逐条回看「我的 /media 那次是被谁清的」（M-1 已接线），并同步写
/// audit 日志；③ 拒绝路径的错误文案给出条目数与两条手动出口（终端 `tailscale funnel
/// reset` / 向导 `disable_force`）。
///
/// **M-2 注释纠偏（2026-10-07 评审）**：本段此前写「卡面逐条呈现待前端接线（本批不改
/// 前端）」——B1 起前端已接线（确认框 + 卡面回看），注释与代码不符即缺陷，故改写为现状。
pub(super) fn disable_funnel() -> Result<(), String> {
    run_cli(&["funnel", "reset"]).map(|_| ())?;
    // W-B 档位记账：本函数是 `funnel reset` 的**唯一调用点**（撤销 / 自愈 / 重开三条路
    // 都经它），故"本进程重开过"是个可证事实 → 之后的"记录尚未发布"按**重新开通档**
    // 给时长（实测 30–49 秒），而不是首开那句 5 分钟。
    // **I1（2026-10-07 评审）**：这两句此前与代码不符——自愈自己发过一条 reset 绕过本
    // 函数（不记账，且吞掉失败）。现已收口，并由源码锁
    // `funnel_reset_has_exactly_one_call_site` 把"唯一调用点"从注释变成可执行约束。
    super::reach::note_funnel_reset();
    Ok(())
}

/// 启动生命周期单点（start_channel 与向导 funnel/enable 步共用——修复轮 2 指针③：
/// 向导接线收敛到与 start_channel 一致的生命周期，不再「直调原语不置期望态」）：
/// ① 守卫+开通（enable_funnel 内含）② 记期望态 + 立即现算一轮 + 起轮询。
/// 返回开通结果（批准链接若有）供向导递给用户。
///
/// **W-A（2026-10-07 真机重启实测）**：后端仍在初始化时 `enable_funnel` 返回
/// [`FunnelOutcome::Deferred`]——此时**不判配置、不写任何东西**，但生命周期照走：
/// 记期望态 → 现算（快照 = 恢复中，不是故障）→ 起轮询。轮询在"后端**确认**就绪且配置
/// **确认**缺失"时才补开通（[`deferred_open_due`]），于是：配置已被 tailscaled 恢复
/// （实测形态）= 一次写都不发就自动回到运行；真丢了 = 窗口结束后自动补上。
pub(super) fn start_channel_inner(port: u16) -> Result<FunnelOutcome, String> {
    // §C3 要求 7（B-I1）：先把旧校验态作废——重开 Funnel 是**已知的地址失效时刻**，
    // 旧的 Verified 不得存活（失败路径同样保持作废：宁可报「尚未验证」）
    invalidate_reachability();
    // 延迟补开通的失败留痕随新一次开启翻页（同 stop 侧口径：留痕只属于它那次缺失期）
    set_deferred_open_error(None);
    let outcome = enable_funnel(port)?;
    *DESIRED.lock().unwrap() = Some(port);
    refresh_once(port); // 起完立即现算一轮（不等第一个轮询窗）
    ensure_poller();
    Ok(outcome)
}

/// 启动 Tailscale 通道（remote_toggle_channel / 启动恢复的入口）：
/// 守卫（已有外来 serve 配置 → 拒绝，绝不覆盖）→ 开通 → 记期望态 + 起轮询。
/// 返回 Err 时快照不动（调用方把错误写快照展示——与 tunnel「失败不阻断」同口径）
pub(crate) fn start_channel(port: u16) -> Result<(), String> {
    start_channel_inner(port).map(|_| ())
}

/// 外来 serve 配置守卫的特征文案（开通/撤销两向单点，防两处口径漂移——修复轮 1 起
/// stop 路径与 start_channel 同款）
pub(super) fn foreign_serve_error(action: &str, others: usize) -> String {
    // 条目数如实报出（B-I5 ②）：连"会清掉几条"都说不出来，用户就没有知情可言
    let detail = if others > 0 {
        format!("（{others} 条非 兔维斯 条目）")
    } else {
        String::new()
    };
    format!(
        "TS_FOREIGN_SERVE: 检测到非 兔维斯 的 Tailscale serve/Funnel 配置{detail}，为避免覆盖已中止{action}；\
         如确认可放弃该配置，请在终端执行 `tailscale funnel reset` 后重试，\
         或在向导中确认后执行强制撤销（向导步 disable_force）"
    )
}

/// 守卫读数失败的特征文案（B-I4，fail-closed 单点）：**读不到 Funnel 状态就绝不执行
/// `funnel --bg`（开通）或 `funnel reset`（撤销/自愈）**。旧实现用 `unwrap_or_default()`
/// 把「读不到」当成「没有配置」放行——CLI 一时失败（后端忙 / 版本差异）就会覆盖用户
/// 自建配置，与本模块自己的 fail-closed 纪律（`map_status` / 步骤判据表都
/// 「读不到就报错」）自相矛盾
pub(super) fn funnel_status_unreadable(action: &str, e: &str) -> String {
    format!("TS_FUNNEL_STATUS_UNREADABLE: 读不到 Funnel 状态，拒绝{action}（无法确认是否会覆盖他人的 serve 配置）: {e}")
}

/// 停止内核（start/stop 对称的唯一实现，stop_channel / stop_all / run_step("disable")
/// 共用）：**本地宣称先收口**（DESIRED 清空 + 快照复位——无论后续 CLI 结果如何，兔维斯
/// 绝不继续宣称运行）→ 守卫（配置已是外来 → 拒绝 `funnel reset`——它清整份 serve
/// 配置，静默清掉用户自建配置不可接受；错误口径与 start_channel 同款）→ 非外来才撤。
/// port 来源：显式 hint（向导步参数）优先，回落 DESIRED（取走即清）；两者皆无 =
/// 兔维斯 从未开过本通道，无事可做直接 Ok（停机路径依赖这条零 CLI 早退，不悬挂）。
/// CLI 失败（含未安装）在收口之后如实 Err 上抛——调用方（开关闭包）写快照展示。
/// `force` = 用户已显式确认的强制撤销（B-M4 出口，见 run_step("disable_force")）：
/// **只跳过归属守卫**（配置完全不含 兔维斯 那路也照撤——否则端口被改后 兔维斯 自己的旧配置
/// 会变"外来"，开通与撤销双双被拒、Funnel 留在公网且 UI 无出路），
/// 仍要求守卫读数成功（读不到配置就不动，fail-closed 不因 force 放宽）。
///
/// **返回 `Option`（B1 补账 · 总开关关闭路径的留痕判据）**：`None` = 本就没开（端口 hint
/// 与期望态皆无，**什么都没拆**）；`Some(报告)` = 确实发出了 `funnel reset` 并成功返回。
/// 停机路径的审计必须区分这两者（只有真拆掉才能记「已撤销」，见 [`stop_all_with`]），
/// 而向导步/开关路径总有端口 hint、从不落 `None` 分支——它们经薄壳 [`stop_inner`] 取报告。
pub(super) fn stop_inner_outcome(
    port_hint: Option<u16>,
    force: bool,
) -> Result<Option<ServeResetReport>, String> {
    // 本地宣称收口（期望态清空 + 快照复位 + 校验态作废）必须在**同一临界区**完成
    // （B-I3）：分开做会给在途轮询轮留下「读到 Some → 被抢占 → 这里清空 → 轮询把运行态
    // 写回」的窗口，卡面在停机后继续宣称运行最长一个轮询窗
    let desired = {
        let mut d = DESIRED.lock().unwrap();
        let taken = d.take();
        set_ts_snapshot(|s| *s = crate::remote::tunnel::ChannelStatus::default());
        // §C3 要求 7（B-I1）：本地宣称收口的同时作废校验态——下面可能发出 `funnel reset`
        //（实测事故里「记录未发布」的触发动作），旧的 Verified 绝不能再被当作有效
        invalidate_reachability();
        // 延迟补开通的失败留痕也随停机翻页：通道已停，上一轮的补开通失败不该在下次
        // 开启时"复活"成当前故障（它是上一次缺失期的事实，不是这一次的）
        set_deferred_open_error(None);
        taken
    };
    let Some(port) = port_hint.or(desired) else {
        return Ok(None); // 本就没开：没有「我们那份」可撤（也没有「已撤销」可留痕）
    };
    // 读数失败 → 拒绝（B-I4）；这一步 force 也不放宽：读不到就不动配置
    let funnel_json = run_cli(&["funnel", "status", "--json"])
        .map_err(|e| funnel_status_unreadable("撤销", &e))?;
    let ownership = serve_ownership(&funnel_json, port);
    if ownership.is_foreign() && !force {
        return Err(foreign_serve_error("撤销", ownership.cleared_others()));
    }
    // M-1：把**实际被一并清除的条目**随回执交出去（不只是条数）——卡面「N 条」与条目列表
    // 由此**同源**（都取自这次撤销的条目表），不再出现「确认框列预览条目、toast 报撤销时
    // 条数」两处对不上。判据与 ownership 同源单点：`serve_ownership` 内部就是从同一张
    // `serve_entries` 表派生的计数，故两者恒等（debug 自检兜住漂移）。
    let cleared_entries: Vec<ServeEntry> = serve_entries(&funnel_json, port)
        .into_iter()
        .filter(|e| !e.ours)
        .collect();
    debug_assert_eq!(
        cleared_entries.len(),
        ownership.cleared_others(),
        "条目表与条数必须同源（serve_entries 单点派生）"
    );
    let report = ServeResetReport {
        cleared_others: ownership.cleared_others(),
        forced: force && ownership.is_foreign(),
        cleared_entries,
    };
    disable_funnel()?;
    if report.cleared_others > 0 {
        // B-I5 ②：`funnel reset` 会把这 N 条一起清掉——如实告知（日志/审计），
        // 不静默（回执另随 run_step("disable") 交回调用方）
        log::warn!(
            "tailscale 撤销：另有 {} 条非 兔维斯 serve 条目随 funnel reset 一并清除{}",
            report.cleared_others,
            if report.forced {
                "（强制撤销）"
            } else {
                ""
            }
        );
        crate::remote::events::audit(
            "tailscale_serve_reset_cleared_extras",
            &format!("count={} forced={}", report.cleared_others, report.forced),
        );
    }
    Ok(Some(report))
}

/// 既有签名的薄壳（向导步注入点 `disable` / `disable_force` 与开关路径 `stop_channel`
/// 消费）：这两条路**总有端口 hint**（向导步参数 / `run_step` 透传），从不落
/// [`stop_inner_outcome`] 的 `None` 分支——无事可做按空报告收口，行为与旧实现逐字一致。
pub(super) fn stop_inner(port_hint: Option<u16>, force: bool) -> Result<ServeResetReport, String> {
    stop_inner_outcome(port_hint, force).map(|r| r.unwrap_or_default())
}

/// 撤销回执（B-I5 ②/ B-M4 / M-1）：把「随 `funnel reset` 一并被清除的非 兔维斯 条目」如实
/// 交回调用方——**条数与条目表同源**（都取自同一次 `serve_entries`），叠加形态下这是用户
/// 唯一能看到 "你的 /media 也没了" 的数据源。前端 B1 已接线：确认框用 disable_preview
/// 逐条列出将清除的条目，撤销后用本回执的 `cleared_entries` 逐条呈现**实际**被清者
/// （两条路的条目来源都是后端条目表，不靠前端猜测拼装）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct ServeResetReport {
    /// 随本次 `funnel reset` 一并被清除的非 兔维斯 条目数
    pub cleared_others: usize,
    /// 是否走了强制撤销（跳过了归属守卫）
    pub forced: bool,
    /// **实际被一并清除的非 兔维斯 条目**（与 `cleared_others` 同源，恒等）
    pub cleared_entries: Vec<ServeEntry>,
}

/// 停止 Tailscale 通道（开关 off 的入口）：本地宣称收口 + 守卫后撤配置。
/// Err = 守卫拒绝（外来配置在先不撤）或 CLI 失败——收口已完成，Err 仅供调用方
/// 把原因写快照展示。被一并清除的非 兔维斯 条目走日志/审计——**本入口（开关关闭路径）没有
/// 回执消费方**，条目级的卡面回看走向导步 `disable` / `disable_force` 的回执
/// （[`ServeResetReport::cleared_entries`]，M-1/B1 接线）。
pub(crate) fn stop_channel() -> Result<(), String> {
    stop_inner(None, false).map(|_| ())
}

/// 全停（单通道模块；总开关关闭 / serve 失败退出 / 应用退出三处停机路径共用）：
/// 失败仅告警不阻断——停机路径不允许因通道清理失败而中断（本地宣称已先收口，
/// 见 stop_inner）。留痕内核见 [`stop_all_with`]
pub(crate) fn stop_all() {
    stop_all_with(crate::remote::events::audit);
}

/// 全停内核（可测：审计出口注入——与 `wizard::disable_step_with` 同一「可测内核 +
/// 依赖注入」纪律）。**本函数是 tailscale 拆卸的单点**：总开关关闭
/// （`remote_toggle(false)` → `stop_server_explicit_close` → `stop_server`）、serve 失败
/// 退出、应用退出三处停机路径都经它。
///
/// **为什么必须留痕（B1 补账 · 第二条：补掉最后一处不对称）**：审计的唯一用途是事后回答
/// 「这个 Funnel 是什么时候关的、关没关」。撤销此前只有向导步留痕（上一批），而**关总开关**
/// （`remote_toggle(false)`）恰是用户最可能用来撤掉公网暴露的动作——此前只发 UI 事件
/// `remote-changed`、不留任何审计，等于审计在最主要的路径上失效。挂在拆卸单点上而非某一
/// 调用方：三处停机路径都真的拆了 Funnel，谁拆谁留痕，不会再出现新的不对称。
///
/// **只在真的拆掉时发（失败不撒谎）**：`Ok(None)`（本通道原本就没开 = 没有「已撤销」可言）
/// 与 `Err`（守卫拒绝 / CLI 失败，配置还在公网）**一条都不发**——假审计会让事后追溯得出
/// 「公网暴露已撤下」的错误结论。动作名与字段和开通路径（`remote_toggle_channel`）及向导
/// 撤销步**同源**（`channel_toggled` + 空格分隔 `key=value`）：一把 `channel=tailscale` 的
/// grep 就能拉出完整开/关序列。**`forced` 不在本行**：本路径 `force` 恒为 false（从不强制
/// 撤销），写一个恒假常量是硬编噪声而非事实——是否强制只有向导步那条路有。连带清掉的非
/// 兔维斯 条目数照记（`cleared_others`，与向导行同字段同口径）；`>0` 时 [`stop_inner_outcome`]
/// 另发一条副作用审计 `tailscale_serve_reset_cleared_extras`，两条各司其职（一条记撤销动作
/// 本身、一条记副作用），互不替代。
pub(super) fn stop_all_with(audit: impl FnOnce(&str, &str)) {
    match stop_inner_outcome(None, false) {
        Ok(Some(report)) => audit(
            "channel_toggled",
            &format!(
                "channel=tailscale on=false cleared_others={}",
                report.cleared_others
            ),
        ),
        // 本就没开：没有「已撤销」可言（早退零 CLI，见 stop_inner_outcome）
        Ok(None) => {}
        Err(e) => {
            log::warn!("tailscale 通道全停有残留（守卫拒绝或 CLI 失败，本地宣称已收口）: {e}")
        }
    }
}
