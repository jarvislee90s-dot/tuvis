// 外部通道（M4 T1）：cloudflared 获取层 + 隧道进程管理（Task 5）。
// 设计出处：docs/superpowers/specs/2026-09-16-m4-external-fullchain-design.md T1b/T1c/T1d

use crate::remote::NoWindow;
use std::path::{Path, PathBuf};

/// 通道三值（remote.channel 的值域；parse_channel 是唯一解析口）
pub const KEY_CHANNEL_VALUE_OFF: &str = "off";
pub const KEY_CHANNEL_VALUE_QUICK: &str = "quick";
pub const KEY_CHANNEL_VALUE_NAMED: &str = "named";

/// 通道设置解析（纯函数）：None/空串 → off（未配置视为关闭，老用户升级兼容）；
/// 仅认三个字面量，其余 None（写入侧 UI 限三选一，这里防御乱串）
pub fn parse_channel(v: Option<&str>) -> Option<&'static str> {
    match v.unwrap_or("") {
        "" | KEY_CHANNEL_VALUE_OFF => Some(KEY_CHANNEL_VALUE_OFF),
        KEY_CHANNEL_VALUE_QUICK => Some(KEY_CHANNEL_VALUE_QUICK),
        KEY_CHANNEL_VALUE_NAMED => Some(KEY_CHANNEL_VALUE_NAMED),
        _ => None,
    }
}

/// 平台二进制名（Windows 带 .exe 后缀）
pub fn bin_name() -> &'static str {
    if cfg!(windows) {
        "cloudflared.exe"
    } else {
        "cloudflared"
    }
}

/// cloudflared 固定放置路径（spec T1d：~/.tuvis/bin/，手动放置旁路即放这里）。
/// 数据目录构造沿代码库先例（manifest.rs：dirs::home_dir().unwrap_or_default().join(".tuvis")，
/// 无统一 helper——不新造，保持散用先例）
pub fn cloudflared_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".tuvis")
        .join("bin")
        .join(bin_name())
}

/// cloudflared 固定版本（用户实测版本）。**固定版本是 sha256 校验与镜像回退不引入
/// 供应链风险的前提**——latest 会随上游漂移，校验表与镜像缓存都跟不上；升级 =
/// 改本常量 + 逐项更新 expected_sha256 表（登记于 docs/superpowers/specs/2026-09-16-m4-external-fullchain-design.md §12.5）
pub const CLOUDFLARED_VERSION: &str = "2026.9.1";

/// 平台资产名（纯函数）：download_url_for 的 URL 末段、expected_sha256 查表键与
/// 失败文案共用同一标识——三处必须恒一致，抽出单一来源
pub fn asset_name() -> &'static str {
    if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "cloudflared-darwin-arm64.tgz"
        } else {
            "cloudflared-darwin-amd64.tgz"
        }
    } else if cfg!(windows) {
        "cloudflared-windows-amd64.exe"
    } else if cfg!(target_arch = "aarch64") {
        "cloudflared-linux-arm64"
    } else {
        "cloudflared-linux-amd64"
    }
}

/// 官方 SHA256 表（纯函数）：各资产**下载字节本体**的 sha256（GitHub API release
/// asset digest 口径）；macOS 的 .tgz 校验对象是**下载的压缩包本身**（解压前）。
/// **勿抄 release 正文「SHA256 Checksums」清单的 .tgz 条目**——上游把解包后内部
/// 二进制的哈希标在了 .tgz 名下（2026.9.1 实测：正文标 9a0b19f6…/1ea07ae7…，
/// tgz 本体实为 c27ab8fd…/ff0d3b51…，本地下载 + GitHub asset digest 双重验证），
/// 2026-09-17 版照抄导致 macOS 下载校验必挂（2026-10-13 更正，登记见 spec §12.5）。
/// 未知资产直接 Err（防御性：资产名写错立刻暴露，绝不静默免检）。
/// 升级流程：改 CLOUDFLARED_VERSION → 从新 release 的 GitHub API assets 取
/// digest 字段（**剥去 `sha256:` 前缀**再入表；或本地下载实测）→ 逐项替换
fn expected_sha256(asset: &str) -> Result<&'static str, String> {
    match asset {
        "cloudflared-darwin-amd64.tgz" => {
            Ok("ff0d3b51d5ff70eceef89d6b32145fee985018a2174596a5dbe405e2766e2ac4")
        }
        "cloudflared-darwin-arm64.tgz" => {
            Ok("c27ab8fd0aa489449e3d201eb02f957ef460a13b613662928b1b23394bf1bcfe")
        }
        "cloudflared-windows-amd64.exe" => {
            Ok("2837888cc0f5d58f15b6dc478376de90b4d3ba5241c7947455d1e0a0df429712")
        }
        "cloudflared-linux-amd64" => {
            Ok("03f1f25d1cc93b9ad6c60569d44060bc4f17ed97075760ed8cfca4b12dcd68cc")
        }
        "cloudflared-linux-arm64" => {
            Ok("3d97437c71848bd8df68041e12436b484a661d95073ea1937f01a845ce88faa3")
        }
        other => Err(format!(
            "未知 cloudflared 资产名: {other:?}（sha256 表未登记，拒绝下载）"
        )),
    }
}

/// 候选下载源（纯函数）：官方在首位，其后镜像（前缀 + 完整官方 URL 拼接）。
/// 镜像入表前置条件：开发期 `curl -sI` 实测存活（302/200），死镜像剔除——
/// 2026-09-17 实测 ghfast.top=200、gh-proxy.com=200，均在表
fn download_candidates(official: &str) -> Vec<String> {
    const MIRROR_PREFIXES: [&str; 2] = ["https://ghfast.top/", "https://gh-proxy.com/"];
    let mut out = vec![official.to_string()];
    out.extend(MIRROR_PREFIXES.map(|p| format!("{p}{official}")));
    out
}

/// sha256 比对单点（纯函数）：hex 小写比对——匹配 Ok(())，不符 Err。
/// 切片路径与流式路径共用它，保证两条路径**判据完全一致**（B-M6）
fn compare_sha256(actual_hex: &str, expected: &str) -> Result<(), String> {
    if actual_hex == expected.to_ascii_lowercase() {
        Ok(())
    } else {
        Err(format!(
            "sha256 校验不符（期望 {expected}，实际 {actual_hex}）"
        ))
    }
}

/// sha256 完整性校验（纯函数）：hex 小写比对——匹配 Ok(())，不符 Err（该候选弃用）。
/// pub(crate)：tailscale 向导（§C2）的安装包校验复用本单点，**不新写第二套校验**
pub(crate) fn verify_sha256(bytes: &[u8], expected: &str) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    compare_sha256(&hex, expected)
}

/// sha256 完整性校验（**流式**，B-M6）：与 [`verify_sha256`] 同一比对口径，但不把整包读进
/// 内存——Tailscale 安装包（.pkg/.msi 数十 MB）在每次向导探测时都要校验一次，旧实现
/// `std::fs::read` 整读一次、download 步又整读一次（白占内存与 IO）。
/// 分块 64 KiB 读取；读取失败如实 Err（不可信，不得当成「校验通过」）
pub(crate) fn verify_sha256_file(path: &Path, expected: &str) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| format!("打开文件失败: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("读取文件失败: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    compare_sha256(&hex, expected)
}

/// 下载编排内核（纯编排，fetch_one 注入——生产实现才碰网络）：按候选序尝试
/// 下载 → sha256 校验 → 匹配才落盘；失败/不符的候选弃用换下一；全部失败聚合原因。
/// 本地写盘失败不换候选（磁盘问题换镜像无解）直接 Err
fn try_download(
    candidates: &[String],
    expected: &str,
    mut fetch_one: impl FnMut(&str) -> Result<Vec<u8>, String>,
    dest: &Path,
) -> Result<(), String> {
    let mut errs: Vec<String> = Vec::new();
    for url in candidates {
        match fetch_one(url) {
            Ok(bytes) => match verify_sha256(&bytes, expected) {
                Ok(()) => {
                    if let Some(dir) = dest.parent() {
                        std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败: {e}"))?;
                    }
                    return std::fs::write(dest, &bytes).map_err(|e| format!("写文件失败: {e}"));
                }
                Err(e) => errs.push(format!("{url} → {e}")),
            },
            Err(e) => errs.push(format!("{url} → {e}")),
        }
    }
    Err(format!("全部候选源失败: {}", errs.join("；")))
}

/// 终态获取失败文案（纯函数）：原因 + 官方下载页 + **完整绝对路径**指引——
/// 旧文案只给 `~/.tuvis/bin/` 相对写法，用户无法直接定位放置点
fn download_failure_message(reason: &str, asset: &str, dest: &Path) -> String {
    format!(
        "REMOTE_TUNNEL_DOWNLOAD_FAIL: cloudflared 自动获取失败: {reason}；可从 \
         https://github.com/cloudflare/cloudflared/releases 手动下载 {asset}，\
         放到 {} 后重试",
        dest.display()
    )
}

/// 下载源（纯函数）：官方 GitHub Releases **固定版本** + 固定资产名（latest 已弃用——
/// 固定版本是 sha256 校验与镜像回退不引入供应链风险的前提，见 CLOUDFLARED_VERSION）
pub fn download_url_for() -> Result<String, String> {
    Ok(format!(
        "https://github.com/cloudflare/cloudflared/releases/download/{CLOUDFLARED_VERSION}/{}",
        asset_name()
    ))
}

/// 二进制就绪判定（macOS 需可执行位；Windows 只查存在）
fn binary_ready(p: &Path) -> bool {
    if !p.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    true
}

/// 下载器注入缝形态（生产 download_to / 测试闭包共用；只收落盘 dest——候选循环 +
/// sha 校验的编排已收口进生产实现，测试闭包直接写假文件）。clippy type_complexity
/// 对函数参数的阈值更严，按其建议抽 type 定义——具体类型不变
pub type Downloader = Box<dyn FnOnce(&Path) -> Result<(), String>>;

/// 获取内核（可测）：已就绪 → 直接返回；否则调注入下载器（生产实现见 download_to，
/// 内部完成候选回退 + sha256 校验 + 落盘到 dest），下载完成后 macOS 从 .tgz 解压
/// （系统 tar）、赋可执行位；Windows 直接落位。
/// 平台分支用编译期 cfg 属性而非运行期 cfg!：分支体内 use 了 std::os::unix 专属
/// 条目，运行期分支在 Windows 目标上无法编译（对蓝本的最小编译适配，各平台行为不变）
pub fn ensure_with(bin: &Path, dl: Downloader) -> Result<PathBuf, String> {
    if binary_ready(bin) {
        return Ok(bin.to_path_buf());
    }
    let tmp = bin.with_extension("download");
    dl(&tmp)?;
    #[cfg(target_os = "macos")]
    {
        // .tgz 内含裸二进制 cloudflared：解到同目录再改名（tar 存在于 macOS/全部 CI）
        let dir = bin.parent().ok_or("cloudflared 路径无父目录")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("建 bin 目录失败: {e}"))?;
        let st = std::process::Command::new("tar")
            .args([
                "-xzf",
                tmp.to_str().unwrap_or_default(),
                "-C",
                dir.to_str().unwrap_or_default(),
            ])
            .status()
            .map_err(|e| format!("tar 启动失败: {e}"))?;
        if !st.success() {
            return Err("cloudflared 解压失败".into());
        }
        let _ = std::fs::remove_file(&tmp);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("赋执行位失败: {e}"))?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::rename(&tmp, bin).map_err(|e| format!("落位失败: {e}"))?;
    }
    if binary_ready(bin) {
        Ok(bin.to_path_buf())
    } else {
        Err("cloudflared 落位后校验失败".into())
    }
}

/// 通用下载器（单 URL → 落盘 dest；§C2 tailscale 安装包等复用）：沿用生产 HTTP
/// 客户端口径——reqwest rustls + system-proxy（读 Windows/macOS 系统代理）+
/// connect 15s / 总 600s 超时（大文件容忍慢网、杜绝永久挂起），错误口径与原
/// download_to 内联实现一致。**只管传输不管校验**——sha256 校验由调用方收口
/// （cloudflared 路径见 try_download，tailscale 路径见 run_step("download")）。
/// 仅生产路径调用，测试零网络红线不触碰本函数
pub(crate) fn download_url_to(url: &str, dest: &Path) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| format!("HTTP_CLIENT_BUILD_FAIL: HTTP 客户端构建失败: {e}"))?;
    let bytes = tauri::async_runtime::block_on(async {
        client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("下载失败: {e}"))?
            .error_for_status()
            .map_err(|e| format!("下载失败: {e}"))?
            .bytes()
            .await
            .map_err(|e| format!("下载中断: {e}"))
    })?;
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败: {e}"))?;
    }
    std::fs::write(dest, &bytes).map_err(|e| format!("写文件失败: {e}"))
}

/// 生产下载器（supervise 在 spawn_blocking 线程调用——同步外壳，单次 HTTP 才
/// block_on，阻塞线程无 runtime 限制）。官方 + 实测存活镜像的候选序尝试：
/// 每个候选经 [`download_url_to`] 取回字节（借 scratch 落点，读回即删——不用 dest
/// 本身，保证全部候选失败时 dest 不落半截数据）逐个 sha256 校验，通过才经
/// try_download 落盘 dest。仅生产路径调用，测试零网络红线不触碰本函数
pub fn download_to(dest: &Path) -> Result<(), String> {
    let official = download_url_for()?;
    let expected = expected_sha256(asset_name())?;
    let candidates = download_candidates(&official);
    try_download(
        &candidates,
        expected,
        |url| {
            let scratch = dest.with_extension("download");
            download_url_to(url, &scratch)?;
            let r = std::fs::read(&scratch).map_err(|e| format!("读回下载文件失败: {e}"));
            let _ = std::fs::remove_file(&scratch);
            r
        },
        dest,
    )
}

// ============================================================
// 隧道进程管理（T1b/T1c）：spawn → stderr 解析(quick) → 守护重启 → 快照
// ============================================================

use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// 单通道运行态（M5 A5 双通道化）：running 由 snapshot() 现算（句柄存活，不落全局——
/// 陈旧标志不可能撒谎）；url / error 由 supervise 摄取点写入，url 恒为 board_url
/// 归一后的看板完整地址
#[derive(Clone, Default, serde::Serialize)]
pub struct ChannelStatus {
    pub running: bool,
    pub url: Option<String>,
    pub error: Option<String>,
}

/// 双通道聚合快照（remote_status / gate 域名源 / 托盘消费）。
/// **错误语义选型：每通道独立 error（而非聚合列表）**——gate fail-closed 只需
/// 「任一通道有错」的并集判定，但设置页按通道渲染错误卡片（A6）、via 域名需按通道
/// 归集，聚合列表反而迫使每个消费方再按通道拆分。error ⇒ 该通道无存活 cloudflared，
/// 消费方按「错误通道不宣称」口径过滤地址与域名
#[derive(Clone, Default, serde::Serialize)]
pub struct TunnelStatus {
    pub quick: ChannelStatus,
    pub named: ChannelStatus,
}
static SNAPSHOT: Lazy<Mutex<TunnelStatus>> = Lazy::new(Mutex::default);

pub fn snapshot() -> TunnelStatus {
    // SNAPSHOT 短锁克隆后即放，再取 TUNNELS 短锁现算 running——两把锁从不嵌套持有，
    // 各自独立短临界区（与 mod.rs SERVER_HANDLE/registry 同纪律）
    let mut st = SNAPSHOT.lock().unwrap().clone();
    let slots = TUNNELS.lock().unwrap();
    st.quick.running = slot_live(&slots.quick);
    st.named.running = slot_live(&slots.named);
    st
}
/// pub(crate) 供 remote/mod.rs 的「快照 → 域名适配器」单测注入通道/错误态
/// （评审 Minor 6；全局态仅测试触碰，用后还原默认值）
pub(crate) fn set_snapshot(f: impl FnOnce(&mut TunnelStatus)) {
    f(&mut SNAPSHOT.lock().unwrap());
}

/// 句柄存活判定（与 mod.rs handle_is_live 同判据）：句柄存在且 supervisor 任务未结束。
/// 任务自行退出（spawn 失败 / 3 次退避放弃）留下**已完成**的陈旧句柄 = 不存活
fn slot_live(h: &Option<TunnelHandle>) -> bool {
    h.as_ref()
        .map(|t| !t.supervisor.inner().is_finished())
        .unwrap_or(false)
}

/// 运行句柄：stop 信号 + supervisor 任务句柄（子进程由 supervisor 全权持有，
/// stop 经标志位传导——supervisor 内 kill_on_drop 保证 abort 时子进程必死）
struct TunnelHandle {
    stop: Arc<AtomicBool>,
    supervisor: tauri::async_runtime::JoinHandle<()>,
}

/// 每通道双槽位（M5 A5 安全关键）：quick / named 可同开，句柄互不共享——
/// stop_channel 只 take 对应槽位，另一通道句柄不受扰（专测锁定）。
/// 槽位按通道分立后，旧「单槽 TUNNEL + mode 三值」模型随 remote.channel 一起退役
#[derive(Default)]
struct TunnelSlots {
    quick: Option<TunnelHandle>,
    named: Option<TunnelHandle>,
}
static TUNNELS: Lazy<Mutex<TunnelSlots>> = Lazy::new(Mutex::default);

/// 按通道写快照（supervise 摄取点 / stop 复位的单点入口）：mode 只认 quick/named
/// 两字面量（调用方来自 ChannelKind / parse_channel 值域，此处再过滤乱串防御）
fn set_channel_snapshot(mode: &str, f: impl FnOnce(&mut ChannelStatus)) {
    let quick = mode == KEY_CHANNEL_VALUE_QUICK;
    set_snapshot(move |s| {
        if quick {
            f(&mut s.quick)
        } else {
            f(&mut s.named)
        }
    });
}

/// 隧道地址归一为看板地址（纯函数，平台无关的纯字符串处理）：去尾部 `/`（可多个）
/// → 追加 `/m`；已以 `/m` 结尾则幂等（不重复追加，不产生 `/m/m`）。
/// 快照 url 契约：摄取点（supervise 的 stderr 解析）统一过本函数，url 恒为
/// 看板完整地址——消费方（地址表 / issue_token / 托盘复制 / toast）直接使用，
/// 不再各自拼接（根路径 / 会 404，服务端只伺服 /m 前缀）。
/// 输入假定为裸域名（无路径）；若未来解析器返回带路径 URL 需重新评估
pub fn board_url(base: &str) -> String {
    let t = base.trim_end_matches('/');
    if t.ends_with("/m") {
        t.to_string()
    } else {
        format!("{t}/m")
    }
}

/// stderr 摄取点解析+归一（纯函数）：quick=trycloudflare 行 / named=自有子域行
/// → 裸域名 → board_url 归一为看板完整地址。原「parse_quick_url/parse_named_url
/// → board_url」两步收口为单点，锁定「快照 url 恒为看板地址」契约的执行点——
/// stderr 任务只调本函数，不得绕开归一直取裸域名
pub fn parsed_board_url(line: &str, quick: bool) -> Option<String> {
    let parsed = if quick {
        parse_quick_url(line)
    } else {
        parse_named_url(line)
    };
    parsed.map(|u| board_url(&u))
}

/// quick stderr 地址解析（纯函数）：行内 https://*.trycloudflare.com 才算。
/// 两层防御（2026-09-20 Mac 实测：临时隧道重试期 stderr 打出含注册端点的 JSON 行，
/// 旧实现把 `https://api.trycloudflare.com/tunnel":` 当地址写入快照并弹「公网地址
/// 已获取」，用户拿到的 URL 无法访问——与 named 侧穿透事故同型的解析缺陷）：
/// ① host 按 URL 字符集（字母数字/点/横杠）连续截取——天然免疫 JSON 引号/冒号/
///    逗号黏连与路径（cloudflared 把 URL 作为 JSON key/value 嵌在标点之间）；
/// ② 只认 `*.trycloudflare.com` 后缀并**排除注册端点 `api.trycloudflare.com`**
///    （获取临时地址的 API 本身不是看板地址）。
pub fn parse_quick_url(line: &str) -> Option<String> {
    let s = line.find("https://")?;
    let e = line.find(".trycloudflare.com")?;
    if e <= s {
        return None;
    }
    const SCHEME: &str = "https://";
    let host: String = line[s + SCHEME.len()..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
        .collect();
    if host.is_empty() || host == "api.trycloudflare.com" || !host.ends_with(".trycloudflare.com") {
        return None;
    }
    Some(format!("https://{host}"))
}

/// 守护退避（纯函数）：连续失败 n 次后的下次重启等待；None = 放弃
pub fn backoff_ms(consecutive_failures: u32) -> Option<u64> {
    match consecutive_failures {
        1 => Some(1_000),
        2 => Some(2_000),
        3 => Some(4_000),
        _ => None,
    }
}

/// 按通道启动隧道（M5 A5，幂等：该通道句柄存活直接返回；**已完成**陈旧句柄视为
/// 不存在重 spawn——自愈判据与 mod.rs start_server_core 同口径）。named 缺 Token
/// 不 spawn，错误写该通道快照供设置页展示。失败不返回 Err（隧道失败不阻断远程
/// 主体——spec T1d），运行态可见性全在快照。调用方值域：mod.rs 的 ChannelKind /
/// restore_enabled_tunnels（通道开关与总开关是隧道启停唯一入口）
pub fn start_channel(mode: &str, port: u16) {
    let is_quick = mode == KEY_CHANNEL_VALUE_QUICK;
    let is_named = mode == KEY_CHANNEL_VALUE_NAMED;
    if !is_quick && !is_named {
        return; // 防御：未知通道名无槽位可起（值来自 ChannelKind，此处兜底乱串）
    }
    if is_named
        && crate::database::dao::settings::get_setting(crate::remote::KEY_TUNNEL_TOKEN)
            .map(|t| t.trim().is_empty())
            .unwrap_or(true)
    {
        set_channel_snapshot(mode, |c| {
            c.error = Some("REMOTE_NO_TUNNEL_TOKEN: 自有域名缺少 Tunnel Token".into())
        });
        return;
    }
    let mut slots = TUNNELS.lock().unwrap();
    let slot = if is_quick {
        &mut slots.quick
    } else {
        &mut slots.named
    };
    if slot_live(slot) {
        return; // 运行中幂等（重复开关 / 总开关与通道开关重入均短路）
    }
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let m = mode.to_string();
    let supervisor = tauri::async_runtime::spawn(async move {
        supervise(m, port, stop2).await;
    });
    *slot = Some(TunnelHandle { stop, supervisor });
}

/// 守护主循环（每通道独立运行一份）：确保二进制 → spawn → 监听 stderr(quick 解析地址)
/// → wait → 退避重启。quick / named 各自 spawn 互不干扰，全部快照写入经
/// set_channel_snapshot 落到**本通道**槽位，绝不触碰另一通道
async fn supervise(mode: String, port: u16, stop: Arc<AtomicBool>) {
    set_channel_snapshot(&mode, |c| {
        c.url = None;
        c.error = None;
    });
    // 获取二进制（可能触发下载——秒到分钟级）。ensure_with 的下载器是**同步闭包**，
    // 生产实现 download_to 内部对单次 HTTP 做 block_on：必须在 spawn_blocking 线程里
    // 调用——supervise 本身是 async 任务，async 上下文内禁止 block_on（会死锁/panic；
    // blocking 线程池无此限制）
    let bin_path = cloudflared_path();
    // bin_path 被 move 进 spawn_blocking 闭包——显示串先另存（错误文案需完整绝对路径）
    let bin_display = bin_path.display().to_string();
    let bin =
        match tokio::task::spawn_blocking(move || ensure_with(&bin_path, Box::new(download_to)))
            .await
            .unwrap_or_else(|e| Err(format!("REMOTE_TUNNEL_TASK_PANIC: 获取任务异常: {e}")))
        {
            Ok(p) => p,
            Err(e) => {
                set_channel_snapshot(&mode, |c| {
                    c.error = Some(download_failure_message(
                        &e,
                        asset_name(),
                        Path::new(&bin_display),
                    ))
                });
                return;
            }
        };
    let mut failures: u32 = 0;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let started_at = std::time::Instant::now();
        let mut cmd = tokio::process::Command::new(&bin);
        cmd.arg("tunnel").arg("--no-autoupdate").kill_on_drop(true); // supervisor 被 abort 时子进程必死（无孤儿）
                                                                     // 防闪窗：cloudflared 是控制台程序且**常驻运行**（隧道活多久黑窗挂多久）
        cmd.no_window();
        if mode == KEY_CHANNEL_VALUE_QUICK {
            cmd.arg("--url").arg(format!("http://127.0.0.1:{port}"));
        } else {
            let token =
                crate::database::dao::settings::get_setting(crate::remote::KEY_TUNNEL_TOKEN)
                    .unwrap_or_default();
            cmd.arg("run").arg("--token").arg(token);
        }
        cmd.stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                set_channel_snapshot(&mode, |c| {
                    c.error = Some(format!(
                        "REMOTE_TUNNEL_START_FAIL: cloudflared 启动失败: {e}"
                    ))
                });
                return;
            }
        };
        // stderr 读取任务（地址解析：quick=trycloudflare 行；named=自有子域行）
        let stderr = child.stderr.take();
        let mode_for_stderr = mode.clone();
        let url_sink = Arc::new(Mutex::new(Option::<String>::None));
        let url_sink2 = url_sink.clone();
        if let Some(err) = stderr {
            tauri::async_runtime::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let quick = mode_for_stderr == KEY_CHANNEL_VALUE_QUICK;
                let mut lines = BufReader::new(err).lines();
                // clippy whilelet_loop 适配（蓝本 loop/match 同语义：Ok(None)/Err 均终止）
                while let Ok(Some(line)) = lines.next_line().await {
                    // 归一在摄取点（parsed_board_url 内部完成解析 + board_url 两步）：
                    // 快照 url 恒为看板完整地址——url_sink / set_snapshot / emit_ui 消费的
                    // 全是归一后的值，下游（地址表 / issue_token / 托盘 / toast）不再各自拼 /m
                    if let Some(u) = parsed_board_url(&line, quick) {
                        let fresh = {
                            let mut g = url_sink2.lock().unwrap();
                            let fresh = g.as_ref() != Some(&u);
                            *g = Some(u.clone());
                            fresh
                        };
                        if fresh {
                            // 竞态窗口取舍（评审 Minor 3，payload 侧收口）：本写入任务
                            // 不持有句柄，stop_channel 的快照复位与在途行写入存在毫秒级
                            // 竞态——产物 url=Some/running=false/error=None 的陈旧快照会
                            // 留到下次 start_channel。读取侧（channels_payload）以
                            // running 为门把已停通道的 address 收口为 null，窗口无害化；
                            // 此处不另做停止标志检查（多一份需同步的标志拷贝，两处口径
                            // 易漂移）
                            set_channel_snapshot(&mode_for_stderr, |c| c.url = Some(u.clone()));
                            // M5 P2-c：自动记忆最近一次解析成功的自有域名地址（KV 持久，
                            // 隧道关着/未解析时设置页与豁免名单仍可用）。仅 named 记忆
                            // （quick 地址每次必变，记忆无意义）
                            if mode_for_stderr == KEY_CHANNEL_VALUE_NAMED {
                                crate::database::dao::settings::set_setting(
                                    crate::remote::KEY_NAMED_ADDR_LAST,
                                    &u,
                                );
                            }
                            // spec T1c：地址变化桌面通知（含首次拿到地址；channel 字段
                            // 供前端区分双通道，A6 消费）
                            crate::remote::events::emit_ui(
                                "remote-tunnel-address",
                                serde_json::json!({ "url": u, "channel": mode_for_stderr }),
                            );
                        }
                    }
                }
            });
        }
        // 等子进程退出（或被 stop 方经 kill_on_drop/标志位终止）
        let status = child.wait().await;
        if stop.load(Ordering::Relaxed) {
            break; // 主动停止不算失败
        }
        // 稳定运行 ≥60s 后的退出视为「新失败」重置计数——否则数周内三次偶发闪断
        // 就会累计到永久放弃（backoff 给的 3 次是**连续**失败语义，spec T1b）
        if started_at.elapsed() >= std::time::Duration::from_secs(60) {
            failures = 0;
        }
        failures += 1;
        match backoff_ms(failures) {
            Some(ms) => {
                log::warn!("cloudflared 退出({status:?})，{ms}ms 后第 {failures} 次重启");
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            }
            None => {
                set_channel_snapshot(&mode, |c| {
                    c.error =
                        Some("REMOTE_TUNNEL_GAVE_UP: cloudflared 连续失败 3 次，已停止守护".into())
                });
                // spec T1b：放弃时桌面通知报错（Task 8 的 useRemoteEvents 监听）
                crate::remote::events::emit_ui(
                    "remote-tunnel-error",
                    serde_json::json!({"error": "REMOTE_TUNNEL_GAVE_UP: cloudflared 连续失败 3 次，已停止守护", "channel": mode}),
                );
                return;
            }
        }
    }
}

/// named 地址解析（纯函数），两形态（2026-09-17 穿透事故的治根项）：
/// ① banner 形态：stderr 中自有子域的 https:// 行（cloudflared 直连凭证模式的
///    启动横幅打印其 hostname）；
/// ② token 模式（远程托管）形态：`"hostname":"<host>"` 配置行——token 启动
///    **不打印** https:// 横幅，M4 只认①导致快照 url 恒 None（设置页无地址显示，
///    且在 M5 豁免语义下演化为穿透，见 mod.rs 豁免名单内核注释）。
/// cloudflare.com / cfargotunnel.com（Registered tunnel connection 行内嵌的
/// 隧道域）一律排除。解析不到留 None（via 装饰标注侧自动回落 lan），设置页提示
/// 「地址以 Cloudflare 面板为准」
pub fn parse_named_url(line: &str) -> Option<String> {
    if let Some(pos) = line.find("https://") {
        // 只取 origin（scheme 后到第一个 `/` 为止）——**剥掉一切路径**：cloudflared
        // 的错误日志行会携带带路径的完整 URL（实测：…/m/api/v1/events 被当成地址，
        // 经 board_url 归一后变成 …/events/m 的脏值）。看板地址 = origin + /m
        let url = line[pos..].split_whitespace().next()?;
        let host = url.trim_start_matches("https://").split('/').next()?;
        if let Some(h) = named_host_ok(host) {
            return Some(format!("https://{h}"));
        }
    }
    // token 模式：配置 JSON 的 ingress hostname（无 scheme；一行可能含多条 ingress，
    // 取第一条合法者）
    const KEY: &str = "\"hostname\":\"";
    let mut from = 0;
    while let Some(rel) = line[from..].find(KEY) {
        let start = from + rel + KEY.len();
        let rest = &line[start..];
        let host = &rest[..rest.find('"').unwrap_or(rest.len())];
        if let Some(h) = named_host_ok(host) {
            return Some(format!("https://{h}"));
        }
        from = start;
    }
    None
}

/// named 域名合法性（两形态共用）：带点（排除裸词），排除 cloudflare 自有域
/// （面板域 docs.cloudflare.com / 隧道域 *.cfargotunnel.com）
fn named_host_ok(host: &str) -> Option<&str> {
    let h = host.trim();
    let bad = h.contains("cloudflare.com") || h.contains("cfargotunnel.com") || !h.contains('.');
    (!bad).then_some(h)
}

/// 停止单通道（M5 A5，幂等；**安全关键**）：只 take 对应槽位——另一通道句柄不受扰
/// （专测锁定「quick 存活时 stop_channel("named") 不动 quick」）。停止 = 置 stop 标志，
/// abort supervisor（kill_on_drop 传播杀子进程，无孤儿），随后该通道快照复位——
/// running 由 snapshot() 现算：句柄取走即 false，无陈旧标志可写
pub fn stop_channel(mode: &str) {
    let is_quick = mode == KEY_CHANNEL_VALUE_QUICK;
    if !is_quick && mode != KEY_CHANNEL_VALUE_NAMED {
        return; // 防御：未知通道名无槽位可停
    }
    let h = {
        let mut slots = TUNNELS.lock().unwrap();
        if is_quick {
            slots.quick.take()
        } else {
            slots.named.take()
        }
    };
    if let Some(h) = h {
        h.stop.store(true, Ordering::Relaxed);
        h.supervisor.abort(); // kill 由 abort 传播（子进程 kill_on_drop 已设——无孤儿）
    }
    set_channel_snapshot(mode, |c| {
        c.url = None;
        c.error = None;
    });
}

/// 双通道全停（总开关关闭 / 应用退出 / serve 退出联动）：逐一停止单通道，
/// 槽位独立，语义与 stop_channel 完全一致
pub fn stop_all() {
    stop_channel(KEY_CHANNEL_VALUE_QUICK);
    stop_channel(KEY_CHANNEL_VALUE_NAMED);
}

/// 隧道全局态（SNAPSHOT / TUNNELS）测试互斥锁（M5 A5）：tunnel.rs 与 remote/mod.rs
/// 的快照/槽位相关测试共享——默认多线程 test 运行下两文件用例并发触碰同一全局会互踩，
/// 持锁串行化 + 各用例用后还原默认值。锁中毒直接取内（某用例 panic 后其余用例照跑，
/// 不连锁红）
#[cfg(test)]
pub(crate) mod test_sync {
    use once_cell::sync::Lazy;
    pub static TUNNEL_GLOBALS: Lazy<std::sync::Mutex<()>> = Lazy::new(|| std::sync::Mutex::new(()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 还原全局默认（每个触碰 TUNNELS/SNAPSHOT 的用例结束前必须调用——与 mod.rs
    /// 快照测试「用后即还」同一纪律）。注入的假句柄随还原 abort 终结，不留悬挂任务
    fn reset_globals() {
        let mut slots = TUNNELS.lock().unwrap();
        if let Some(h) = slots.quick.take() {
            h.supervisor.abort();
        }
        if let Some(h) = slots.named.take() {
            h.supervisor.abort();
        }
        drop(slots);
        set_snapshot(|s| *s = TunnelStatus::default());
    }

    /// 假句柄（可观测终结，A4 同款技巧）：supervisor = pending 任务持 tx——任务被
    /// abort 后 tx 随 future drop，rx 端 Disconnected；stop 标志位真实可断言
    fn fake_handle() -> (TunnelHandle, std::sync::mpsc::Receiver<()>) {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let supervisor = tauri::async_runtime::spawn(async move {
            let _tx = tx; // 随被取消的任务 drop → rx 端可观测
            std::future::pending::<()>().await;
        });
        (
            TunnelHandle {
                stop: Arc::new(AtomicBool::new(false)),
                supervisor,
            },
            rx,
        )
    }

    /// 自旋等 rx 断连（上限 5s）——证明持有 tx 的 supervisor 任务已被 abort 终结
    fn assert_disconnected(rx: &std::sync::mpsc::Receiver<()>, what: &str) {
        let mut waited = 0u32;
        loop {
            match rx.try_recv() {
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    assert!(waited < 5000, "{what} 5s 内未终结");
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    waited += 1;
                }
                Ok(v) => panic!("{what} 不应产出值，实际 {v:?}"),
            }
        }
    }

    /// M5 A5 安全关键专测：quick 存活时 stop_channel("named") 不动 quick——
    /// named supervisor 被终结（rx 断连）、quick 的 stop 标志未置位且 rx 在有界窗口内
    /// 不断连（变异锚点：还原为单槽 stop() 全停时，quick 的断连断言必红）
    #[test]
    fn stop_channel_targets_only_its_own_slot() {
        let _g = test_sync::TUNNEL_GLOBALS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (qh, qrx) = fake_handle();
        let (nh, nrx) = fake_handle();
        {
            let mut slots = TUNNELS.lock().unwrap();
            slots.quick = Some(qh);
            slots.named = Some(nh);
        }
        stop_channel(KEY_CHANNEL_VALUE_NAMED);
        assert_disconnected(&nrx, "named supervisor");
        {
            let slots = TUNNELS.lock().unwrap();
            assert!(
                !slots.quick.as_ref().unwrap().stop.load(Ordering::Relaxed),
                "quick 的 stop 标志不得被 named 的停止置位"
            );
            assert!(slots.named.is_none(), "named 槽位必须已清空");
        }
        // 有界窗口内 quick 未被终结（100ms 静默窗口；abort 送达是毫秒级，
        // 窗口内 Empty 即任务存活——启发式断言，配合标志位双证）。必须发生在
        // 清理 abort 之前，否则测的是自己刚杀掉的句柄
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            matches!(qrx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)),
            "quick supervisor 必须仍存活（named 的停止不得波及）"
        );
        reset_globals();
    }

    /// stop_channel 幂等：空槽位重复停止 no-op 不 panic；停止后槽位清空、
    /// 该通道快照复位（url/error 清掉，running 由句柄现算）
    #[test]
    fn stop_channel_is_idempotent_and_resets_snapshot() {
        let _g = test_sync::TUNNEL_GLOBALS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 空槽位连停两次：不 panic（幂等）
        stop_channel(KEY_CHANNEL_VALUE_QUICK);
        stop_channel(KEY_CHANNEL_VALUE_QUICK);
        // 注入后再停：句柄终结 + 槽位清空 + 快照复位
        let (qh, qrx) = fake_handle();
        TUNNELS.lock().unwrap().quick = Some(qh);
        set_snapshot(|s| {
            s.quick.url = Some("https://q.trycloudflare.com/m".into());
            s.quick.error = Some("旧错误".into());
        });
        stop_channel(KEY_CHANNEL_VALUE_QUICK);
        assert_disconnected(&qrx, "quick supervisor");
        assert!(TUNNELS.lock().unwrap().quick.is_none(), "槽位必须清空");
        let st = snapshot();
        assert_eq!(st.quick.url, None, "停止后 url 复位");
        assert_eq!(st.quick.error, None, "停止后 error 复位");
        assert!(!st.quick.running, "句柄取走后 running 现算为 false");
        // 未知通道名：防御性 no-op（不 panic、不触碰任何槽位）
        stop_channel("bogus");
        reset_globals();
    }

    /// stop_all 全停：两通道句柄全部终结、槽位清空（总开关关闭 / 应用退出路径的语义）
    #[test]
    fn stop_all_stops_both_channels() {
        let _g = test_sync::TUNNEL_GLOBALS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (qh, qrx) = fake_handle();
        let (nh, nrx) = fake_handle();
        {
            let mut slots = TUNNELS.lock().unwrap();
            slots.quick = Some(qh);
            slots.named = Some(nh);
        }
        stop_all();
        assert_disconnected(&qrx, "quick supervisor");
        assert_disconnected(&nrx, "named supervisor");
        {
            let slots = TUNNELS.lock().unwrap();
            assert!(slots.quick.is_none() && slots.named.is_none());
        }
        reset_globals();
    }

    /// 双通道同开快照聚合：两通道 url/error 各自独立呈现；running 随句柄存活现算
    /// （句柄取走立即 false）。两通道同开 = A5 用户裁决的核心场景
    #[test]
    fn snapshot_aggregates_both_channels_and_running_follows_slots() {
        let _g = test_sync::TUNNEL_GLOBALS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (qh, _qrx) = fake_handle();
        TUNNELS.lock().unwrap().quick = Some(qh);
        set_snapshot(|s| {
            s.quick.url = Some("https://q-test.trycloudflare.com/m".into());
            s.named.url = Some("https://mam.example.com/m".into());
            s.named.error = Some("cloudflared 启动失败".into());
        });
        let st = snapshot();
        assert!(st.quick.running, "quick 句柄存活 → running 真");
        assert_eq!(
            st.quick.url.as_deref(),
            Some("https://q-test.trycloudflare.com/m")
        );
        assert_eq!(st.quick.error, None);
        assert!(!st.named.running, "named 无句柄 → running 假（现算不撒谎）");
        assert_eq!(st.named.url.as_deref(), Some("https://mam.example.com/m"));
        assert_eq!(st.named.error.as_deref(), Some("cloudflared 启动失败"));
        reset_globals();
    }

    #[test]
    fn channel_parse_only_three_literals() {
        assert_eq!(parse_channel(None), Some("off"));
        assert_eq!(parse_channel(Some("")), Some("off"));
        assert_eq!(parse_channel(Some("quick")), Some("quick"));
        assert_eq!(parse_channel(Some("named")), Some("named"));
        assert_eq!(parse_channel(Some("QUICK")), None); // 大小写敏感拒绝
        assert_eq!(parse_channel(Some("tls")), None);
    }

    #[test]
    fn download_url_matches_platform() {
        // 纯函数按 cfg 分支断言本机平台（跨平台 CI 各自命中各自分支）
        let url = download_url_for().unwrap();
        // 固定版本段（pinned）：latest 是 sha 校验与镜像回退的供应链风险源，禁用
        assert!(url.contains(&format!("/releases/download/{CLOUDFLARED_VERSION}/")));
        assert!(!url.contains("latest"));
        if cfg!(target_os = "macos") {
            assert!(url.contains("cloudflared-darwin-"));
            assert!(url.ends_with(".tgz"));
        } else if cfg!(windows) {
            assert!(url.ends_with("cloudflared-windows-amd64.exe"));
        }
        assert!(url.starts_with("https://github.com/cloudflare/cloudflared/releases/"));
        // URL 末段恒为资产名——expected_sha256 查表与失败文案共用同一标识
        assert_eq!(url.rsplit('/').next(), Some(asset_name()));
    }

    #[test]
    fn ensure_skips_download_when_binary_exists() {
        // 已存在（含可执行位）→ 不调下载器直接 Ok
        let dir = tempfile::tempdir().unwrap();
        // cloudflared_path 读 HOME——本测试改为直接驱动 ensure_cloudflared 的
        // exists 判定内核：ensure_with(bin_path, downloader)
        let bin = dir.path().join(bin_name());
        #[cfg(unix)]
        std::fs::write(&bin, b"fake").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        #[cfg(windows)]
        std::fs::write(&bin, b"fake").unwrap();
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c2 = called.clone();
        let r = ensure_with(
            &bin,
            Box::new(move |_dest| {
                c2.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }),
        );
        assert!(r.is_ok());
        assert!(!called.load(std::sync::atomic::Ordering::Relaxed));
    }

    // ==== Task 6：下载器加固（固定版本 + sha256 + 镜像回退 + 失败指引）====
    // 零网络红线：fetch 闭包全部为内存假实现，不触真实 HTTP

    /// 公认 SHA256 测试向量：sha256("hello")
    const HELLO_SHA: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn download_candidates_official_first_then_alive_mirrors() {
        let official =
            "https://github.com/cloudflare/cloudflared/releases/download/2026.9.1/cloudflared-windows-amd64.exe";
        let c = download_candidates(official);
        assert_eq!(c[0], official, "官方源恒首位");
        // 镜像 = 前缀 + 完整官方 URL；两者 2026-09-17 开发期 curl -sI 实测均 200 存活
        assert_eq!(c[1], format!("https://ghfast.top/{official}"));
        assert_eq!(c[2], format!("https://gh-proxy.com/{official}"));
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn expected_sha256_table_matches_release_asset_bytes() {
        // 值 = 2026.9.1 各资产**下载字节本体**的 sha256（GitHub API asset digest，
        // 本地下载实测一致；升级 = 改常量 + 更新本表）。.tgz 两行**不是** release
        // 正文清单值——上游把内部二进制哈希标在 .tgz 名下（见 expected_sha256 注释）
        assert_eq!(
            expected_sha256("cloudflared-darwin-amd64.tgz").unwrap(),
            "ff0d3b51d5ff70eceef89d6b32145fee985018a2174596a5dbe405e2766e2ac4"
        );
        assert_eq!(
            expected_sha256("cloudflared-darwin-arm64.tgz").unwrap(),
            "c27ab8fd0aa489449e3d201eb02f957ef460a13b613662928b1b23394bf1bcfe"
        );
        assert_eq!(
            expected_sha256("cloudflared-windows-amd64.exe").unwrap(),
            "2837888cc0f5d58f15b6dc478376de90b4d3ba5241c7947455d1e0a0df429712"
        );
        assert_eq!(
            expected_sha256("cloudflared-linux-amd64").unwrap(),
            "03f1f25d1cc93b9ad6c60569d44060bc4f17ed97075760ed8cfca4b12dcd68cc"
        );
        assert_eq!(
            expected_sha256("cloudflared-linux-arm64").unwrap(),
            "3d97437c71848bd8df68041e12436b484a661d95073ea1937f01a845ce88faa3"
        );
    }

    #[test]
    fn expected_sha256_rejects_unknown_asset() {
        // 防御性：资产名写错立刻暴露，绝不静默免检
        assert!(expected_sha256("cloudflared-freebsd-amd64").is_err());
        assert!(expected_sha256("").is_err());
    }

    #[test]
    fn verify_sha256_accepts_correct_and_rejects_wrong_hex() {
        // 正确 hex 通过
        assert_eq!(verify_sha256(b"hello", HELLO_SHA), Ok(()));
        // 错误 hex（另一公认向量的值）拒绝
        assert!(verify_sha256(
            b"hello",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        )
        .is_err());
        // 乱串期望值拒绝
        assert!(verify_sha256(b"hello", "not-a-hash").is_err());
    }

    #[test]
    fn try_download_stops_at_first_valid_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("bin").join("cloudflared");
        let mut called: Vec<String> = vec![];
        let r = try_download(
            &["https://a/1".to_string(), "https://b/2".to_string()],
            HELLO_SHA,
            |url| {
                called.push(url.to_string());
                Ok(b"hello".to_vec())
            },
            &dest,
        );
        assert!(r.is_ok());
        // 首候选成功即停：后续 fetch 不被调
        assert_eq!(called, vec!["https://a/1".to_string()]);
        // 校验通过才落盘（含父目录自动创建）
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
    }

    #[test]
    fn try_download_falls_back_on_fetch_error_and_sha_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("cloudflared");
        let mut called: Vec<String> = vec![];
        let r = try_download(
            &[
                "https://a/1".to_string(),
                "https://b/2".to_string(),
                "https://c/3".to_string(),
            ],
            HELLO_SHA,
            |url| {
                called.push(url.to_string());
                match url {
                    "https://a/1" => Err("连接超时".into()), // 网络失败 → 换下一
                    "https://b/2" => Ok(b"corrupted".to_vec()), // sha 不符 → 弃用换下一
                    _ => Ok(b"hello".to_vec()),              // 首个有效候选
                }
            },
            &dest,
        );
        assert!(r.is_ok());
        assert_eq!(
            called,
            vec![
                "https://a/1".to_string(),
                "https://b/2".to_string(),
                "https://c/3".to_string()
            ],
            "失败候选依序弃用回落，不得提前终止"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
    }

    #[test]
    fn try_download_errs_and_writes_nothing_when_all_fail() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("cloudflared");
        let r = try_download(
            &["https://a/1".to_string(), "https://b/2".to_string()],
            HELLO_SHA,
            |url| Err(format!("源不可达: {url}")),
            &dest,
        );
        assert!(r.is_err());
        let msg = r.unwrap_err();
        assert!(
            msg.contains("https://a/1") && msg.contains("https://b/2"),
            "全部失败须聚合各候选原因: {msg}"
        );
        assert!(!dest.exists(), "全失败不得落盘");
    }

    #[test]
    fn download_failure_message_has_absolute_path_and_official_page() {
        let dir = tempfile::tempdir().unwrap();
        let abs = dir.path().join("bin").join(bin_name()); // 真实绝对路径
        let msg = download_failure_message("全部候选源失败", asset_name(), &abs);
        assert!(msg.contains("https://github.com/cloudflare/cloudflared/releases"));
        assert!(
            msg.contains(&abs.display().to_string()),
            "必须含完整绝对路径（不得只有 ~/.tuvis/bin/ 相对写法）: {msg}"
        );
        assert!(msg.contains(asset_name()));
    }

    #[test]
    fn parsed_board_url_normalizes_quick_and_named_lines() {
        // quick 形态 → 看板完整地址（「快照 url 恒为看板地址」契约的执行点）
        assert_eq!(
            parsed_board_url(
                r#"2026-09-16T00:00:00Z INF |  https://example-words-here.trycloudflare.com"#,
                true
            ),
            Some("https://example-words-here.trycloudflare.com/m".to_string())
        );
        // named 形态（自有子域行）→ 同样归一
        assert_eq!(
            parsed_board_url("INF hostname=https://mam.example.asia", false),
            Some("https://mam.example.asia/m".to_string())
        );
        // 已含 /m 的输入幂等（不产生 /m/m）
        assert_eq!(
            parsed_board_url("INF hostname=https://mam.example.asia/m", false),
            Some("https://mam.example.asia/m".to_string())
        );
        // 解析不到的行 → None
        assert_eq!(
            parsed_board_url("INF Registered tunnel connection", true),
            None
        );
        // quick 模式下 cloudflare.com 域行不误报
        assert_eq!(
            parsed_board_url("see https://docs.cloudflare.com/", true),
            None
        );
    }

    /// token（远程托管）模式解析（2026-09-17 穿透事故治根项）：无 https:// 横幅，
    /// 配置 JSON 内嵌 ingress hostname——旧实现只认横幅 → 快照 url 恒 None
    #[test]
    fn parse_named_url_token_mode_hostname_line() {
        let line = r#"2026-09-17T00:00:00Z INF Updated to new configuration {"config":{"ingress":[{"hostname":"mam-win.bondtoolbox.asia","service":"http://localhost:9420"},{"service":"http_status:404"}]}}"#;
        assert_eq!(
            parse_named_url(line),
            Some("https://mam-win.bondtoolbox.asia".to_string())
        );
        assert_eq!(
            parsed_board_url(line, false),
            Some("https://mam-win.bondtoolbox.asia/m".to_string())
        );
        // Registered tunnel connection 行内嵌的隧道域（cfargotunnel.com）不得误报
        assert_eq!(
            parse_named_url(
                r#"INF Registered tunnel connection connIndex=0 hostname=abc123.cfargotunnel.com"#
            ),
            None
        );
        // 面板文档域不误报
        assert_eq!(
            parse_named_url(
                r#"INF config {"ingress":[{"hostname":"docs.cloudflare.com","service":"http_status:404"}]}"#
            ),
            None
        );
        // 多条 ingress：跳过不合法者，取第一条合法域名
        let multi = r#"INF Updated to new configuration {"config":{"ingress":[{"hostname":"a.cfargotunnel.com"},{"hostname":"mam-two.example.org","service":"http://localhost:9420"}]}}"#;
        assert_eq!(
            parse_named_url(multi),
            Some("https://mam-two.example.org".to_string())
        );
    }

    // ==== Task 5：隧道进程管理（T1b/T1c）纯函数测试 ====
    // 零网络、零 ~/.tuvis 红线：以下全部为纯函数断言，不触 DAO / 进程 / 文件系统

    #[test]
    fn quick_url_parses_only_trycloudflare_host() {
        let line = r#"2026-09-16T00:00:00Z INF |  https://example-words-here.trycloudflare.com"#;
        assert_eq!(
            parse_quick_url(line).as_deref(),
            Some("https://example-words-here.trycloudflare.com")
        );
        // 普通日志行/其他域名不误报
        assert_eq!(parse_quick_url("INF Registered tunnel connection"), None);
        assert_eq!(parse_quick_url("see https://docs.cloudflare.com/"), None);
    }

    #[test]
    fn quick_url_excludes_api_endpoint_and_strips_punctuation() {
        // 2026-09-20 Mac 实测回归锁：临时隧道重试期 stderr 的 JSON 形态行——
        // 注册端点 api.trycloudflare.com 不是看板地址，旧实现产出
        // `https://api.trycloudflare.com/tunnel":/m` 脏地址并误弹「已获取」
        assert_eq!(
            parse_quick_url(r#"INF "https://api.trycloudflare.com/tunnel": failed"#),
            None
        );
        assert_eq!(
            parse_quick_url("INF POST https://api.trycloudflare.com/tunnel retrying"),
            None
        );
        // JSON value 形态（URL 嵌在引号逗号之间）→ 剥标点后正常解析
        assert_eq!(
            parse_quick_url(
                r#"INF {"url":"https://some-random-words.trycloudflare.com","line":1}"#
            )
            .as_deref(),
            Some("https://some-random-words.trycloudflare.com")
        );
        // 带路径的行剥成 origin（quick 看板地址恒为裸域名，与 parse_named_url 同口径）
        assert_eq!(
            parse_quick_url("INF https://real-words.trycloudflare.com/api/v1/x"),
            Some("https://real-words.trycloudflare.com".to_string())
        );
    }

    // ==== 裸域名 404 修复：board_url 归一（纯字符串，平台无关） ====

    #[test]
    fn board_url_appends_m_idempotent_and_trims_slashes() {
        // 裸域名（quick/named stderr 解析形态）→ 补 /m
        assert_eq!(
            board_url("https://example-words-here.trycloudflare.com"),
            "https://example-words-here.trycloudflare.com/m"
        );
        assert_eq!(
            board_url("https://mam.example.asia"),
            "https://mam.example.asia/m"
        );
        // 已以 /m 结尾 → 幂等（不得产生 /m/m）
        assert_eq!(
            board_url("https://mam.example.asia/m"),
            "https://mam.example.asia/m"
        );
        // 尾部 / 去重（可多个），含 /m/ 形态
        assert_eq!(
            board_url("https://mam.example.asia/"),
            "https://mam.example.asia/m"
        );
        assert_eq!(
            board_url("https://mam.example.asia///"),
            "https://mam.example.asia/m"
        );
        assert_eq!(
            board_url("https://mam.example.asia/m/"),
            "https://mam.example.asia/m"
        );
        // 退化输入不 panic：空串 → 仍返回含 /m 的形态
        assert_eq!(board_url(""), "/m");
    }

    #[test]
    fn backoff_is_1s_2s_4s_then_give_up() {
        assert_eq!(backoff_ms(1), Some(1000));
        assert_eq!(backoff_ms(2), Some(2000));
        assert_eq!(backoff_ms(3), Some(4000));
        assert_eq!(backoff_ms(4), None); // 连续失败 3 次后停止（spec T1b）
    }
}
