//! Windows transport：Named Pipe `\\.\pipe\codex-ipc`（Tokio async ClientOptions，
//! Overlapped——不得同步单 handle 一读一写）。**Windows 原生测试必须在 Windows 真机
//! 运行**；非 Windows 平台只保留编译面（交叉 check 覆盖），不谎报运行证据。
//!
//! 身份：build 取运行中官方 Desktop 的 **MSIX 包版本**（包根 `AppxManifest.xml` 的
//! `Identity/@Version`；四段版本号与 Windows probe 记录的 `26.1013.3656.0` 同形）；
//! 来源缺失/多义 → `build=None` → profile 不命中 → A 写能力关闭（spec F8，不伪装已验证）。
//!
//! **2026-10-17 Windows 实机取证（验证批 F1）**：本文件旧版读 `ChatGPT.exe` 的
//! `VS_FIXEDFILEINFO`，实测拿到的是 **Chromium/Electron 版本** `155.0.8059.80`
//! （app 目录内含 `chrome.dll`(329 MB) 与 `155.0.8059.80.manifest`），与 app 包 build
//! `26.1013.4894.0` 不同源 ⇒ `profile_for` **永不命中**、A 写能力在任何真实 Desktop 上
//! 结构性关闭（连已验证 build 也打不开）。故改读 MSIX 包清单；旁证：`app\Codex.exe`
//! 与 CLI `codex.exe` 都**没有**版本资源，不能当来源。

use super::{DesktopEndpoint, EndpointResolver};
#[allow(unused_imports)]
use crate::services::codex_desktop_ipc::types::ProtocolError;

/// 官方 Desktop IPC 管道名（Windows probe 定案）。
pub const PIPE_NAME: &str = r"\\.\pipe\codex-ipc";

/// 官方 Desktop 进程名（Windows probe：ChatGPT.exe）。
#[cfg(windows)]
const DESKTOP_EXE: &str = "ChatGPT.exe";

pub struct WindowsEndpointResolver;

#[cfg(windows)]
impl EndpointResolver for WindowsEndpointResolver {
    fn resolve(&self) -> Result<DesktopEndpoint, ProtocolError> {
        Ok(DesktopEndpoint {
            platform: "windows".into(),
            path: PIPE_NAME.into(),
            build: desktop_build(),
        })
    }
}

#[cfg(not(windows))]
impl EndpointResolver for WindowsEndpointResolver {
    fn resolve(&self) -> Result<DesktopEndpoint, ProtocolError> {
        Err(ProtocolError::Disconnected(
            "named pipe transport is Windows-only".into(),
        ))
    }
}

/// 运行中官方 Desktop 的 **app 包版本**（多义 → None；缺失 → None）。
#[cfg(windows)]
fn desktop_build() -> Option<String> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
    );
    let mut versions: Vec<String> = Vec::new();
    for process in sys.processes().values() {
        let Some(exe) = process.exe().map(|p| p.to_path_buf()) else {
            continue;
        };
        if !exe
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case(DESKTOP_EXE))
        {
            continue;
        }
        if let Some(v) = package_version_for_exe(&exe) {
            versions.push(v);
        }
    }
    versions.sort();
    versions.dedup();
    if versions.len() == 1 {
        versions.pop()
    } else {
        None
    }
}

/// MSIX 包版本：从 exe 路径向上找包根（含 `AppxManifest.xml`），取 `Identity/@Version`。
///
/// 为什么不是 exe 版本资源：2026-10-17 Windows 实机实测 `ChatGPT.exe` 的
/// `VS_FIXEDFILEINFO` = `155.0.8059.80`（Chromium/Electron 版本），与 app 包 build
/// （`26.1013.x.y`）不同源；app 版本的真源 = 包清单 `Identity/@Version`
/// （非管理员亦可读，实测已取到 `26.1013.4894.0`）。
/// 读不到 / 形状不符 → `None`（调用方据此关能力，不伪装已验证）。
pub fn package_version_for_exe(exe: &std::path::Path) -> Option<String> {
    let mut dir = exe.parent()?;
    for _ in 0..3 {
        let manifest = dir.join("AppxManifest.xml");
        if manifest.is_file() {
            let text = std::fs::read_to_string(&manifest).ok()?;
            return identity_version(&text);
        }
        dir = dir.parent()?;
    }
    None
}

/// 解析 `<Identity … Version="x.y.z.w" …>` 的版本串（纯字符串，无 XML 依赖）。
/// 形状校验：≥3 段、每段非空且全数字——防把别的属性误当版本。
fn identity_version(text: &str) -> Option<String> {
    let ident = text.find("<Identity")?;
    let rest = &text[ident..];
    let tag = match rest.find('>') {
        Some(end) => &rest[..end],
        None => rest,
    };
    let key = "Version=\"";
    let start = tag.find(key)? + key.len();
    let tail = &tag[start..];
    let end = tail.find('"')?;
    let ver = &tail[..end];
    let parts: Vec<&str> = ver.split('.').collect();
    let shaped = parts.len() >= 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    if shaped {
        Some(ver.to_string())
    } else {
        None
    }
}

/// 连接 named pipe，超时见 [`super::CONNECT_TIMEOUT`]；ERROR_PIPE_BUSY 按官方
/// 模式等待重试。
#[cfg(windows)]
pub async fn connect(endpoint: &DesktopEndpoint) -> Result<Box<dyn super::IpcIo>, ProtocolError> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let path = endpoint.path.clone();
    let open = async move {
        // 命名管道实例忙时轮询重试（ERROR_PIPE_BUSY = Win32 SDK 常量 231，
        // windows-rs `Win32::Foundation::ERROR_PIPE_BUSY`；禁手写数字防漂移）
        #[cfg(windows)]
        {
            use windows::Win32::Foundation::ERROR_PIPE_BUSY;
            for _ in 0..50 {
                match ClientOptions::new().open(&path) {
                    Ok(client) => return Ok(client),
                    Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        #[cfg(not(windows))]
        {
            let _ = &path;
        }
        Err(std::io::Error::other("pipe busy retry budget exhausted"))
    };
    match tokio::time::timeout(super::CONNECT_TIMEOUT, open).await {
        Ok(Ok(client)) => Ok(Box::new(client)),
        Ok(Err(e)) => Err(ProtocolError::Disconnected(format!(
            "connect to {} failed: {e}",
            endpoint.path
        ))),
        Err(_) => Err(ProtocolError::Timeout(format!(
            "connect to {} timed out ({}s)",
            endpoint.path,
            super::CONNECT_TIMEOUT.as_secs()
        ))),
    }
}

#[cfg(not(windows))]
pub async fn connect(endpoint: &DesktopEndpoint) -> Result<Box<dyn super::IpcIo>, ProtocolError> {
    let _ = endpoint;
    Err(ProtocolError::Disconnected(
        "named pipe transport is Windows-only".into(),
    ))
}

#[cfg(test)]
mod tests {
    // 真实假服务端 named pipe 往返测试只在 Windows 原生运行（CI windows job）；
    // macOS/Linux 上该测试不存在也不谎报执行。
    #[cfg(windows)]
    use super::DesktopEndpoint;
    use super::*;

    /// F1 回归（2026-10-17 Windows 验证批）：包清单 `Identity/@Version` 解析。
    /// 形状取自实机 `AppxManifest.xml`（该机器包版本 `26.1013.4894.0`）。
    #[test]
    fn identity_version_parses_real_manifest_shape() {
        let text = r#"<?xml version="1.0" encoding="utf-8"?>
<Package xmlns:mp="http://schemas.microsoft.com/appx/2014/phone/manifest">
  <Identity Name="OpenAI.Codex" ProcessorArchitecture="x64" Version="26.1013.4894.0" Publisher="CN=50BDFD77-8903-4850-9FFE-6E8522F64D5B" />
  <Properties><DisplayName>Codex</DisplayName></Properties>
</Package>"#;
        assert_eq!(identity_version(text).as_deref(), Some("26.1013.4894.0"));
        // 属性顺序不同、标签自闭合形态都要取到（不假设 Version 紧跟 Identity 名）
        assert_eq!(
            identity_version(r#"<Identity Publisher="CN=x" Version="26.1013.3656.0" />"#)
                .as_deref(),
            Some("26.1013.3656.0")
        );
    }

    /// 形状守卫：缺 Identity / 非四段数字 / 段数不足 → None（宁可关能力，不伪装已验证）。
    #[test]
    fn identity_version_rejects_wrong_shape_or_missing() {
        assert!(identity_version("<Package/>").is_none());
        assert!(identity_version(r#"<Identity Name="x"/>"#).is_none());
        assert!(identity_version(r#"<Identity Name="x" Version="abc"/>"#).is_none());
        assert!(identity_version(r#"<Identity Name="x" Version="1.0"/>"#).is_none());
        assert!(identity_version(r#"<Identity Name="x" Version="1.0.0"/>"#).is_some());
    }

    /// 包根定位：exe 在 `<pkg>\app\ChatGPT.exe`，清单在 `<pkg>\AppxManifest.xml`
    /// （两级上溯）；无清单 → None。
    #[test]
    fn package_version_walks_up_to_manifest() {
        let root = std::env::temp_dir().join(format!("tuvis-pkgver-{}", std::process::id()));
        let app = root.join("app");
        std::fs::create_dir_all(&app).expect("mkdir");
        let exe = app.join("ChatGPT.exe");
        std::fs::write(&exe, b"stub").expect("write exe");
        std::fs::write(
            root.join("AppxManifest.xml"),
            r#"<Identity Name="OpenAI.Codex" Version="26.1013.3656.0" />"#,
        )
        .expect("write manifest");
        assert_eq!(
            package_version_for_exe(&exe).as_deref(),
            Some("26.1013.3656.0")
        );
        assert!(package_version_for_exe(std::path::Path::new("C:/nonexistent/x.exe")).is_none());
        // 收尾：逐个删（不用递归删除）
        let _ = std::fs::remove_file(root.join("AppxManifest.xml"));
        let _ = std::fs::remove_file(&exe);
        let _ = std::fs::remove_dir(&app);
        let _ = std::fs::remove_dir(&root);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn connects_to_fake_pipe_server_and_echoes() -> Result<(), Box<dyn std::error::Error>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::windows::named_pipe::ServerOptions;

        // 随机专属 pipe 名：绝不占用生产 `\\.\pipe\codex-ipc`（Desktop 在场时也安全）
        let test_pipe = format!(
            "\\\\.\\pipe\\codex-ipc-test-{}",
            crate::services::codex_desktop_ipc::types::new_uuid()
        );
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&test_pipe)?;
        // endpoint 注入测试 pipe 名（不经生产 resolver——resolver 只解析真实 endpoint）
        let ep = DesktopEndpoint {
            platform: "windows".into(),
            path: test_pipe.clone(),
            build: None,
        };

        let sv = tokio::spawn(async move {
            server.connect().await?;
            let (mut r, mut w) = tokio::io::split(server);
            let mut buf = vec![0u8; 64];
            let n = r.read(&mut buf).await?;
            w.write_all(&buf[..n]).await?;
            Ok::<(), std::io::Error>(())
        });

        let mut io = super::connect(&ep).await?;
        io.write_all(b"ping").await?;
        let mut buf = vec![0u8; 64];
        let n = io.read(&mut buf).await?;
        assert_eq!(&buf[..n], b"ping");
        sv.await??;
        Ok(())
    }

    /// F3（2026-10-17 复审）：忙态第二客户端 → ERROR_PIPE_BUSY 重试 → 服务端
    /// accept 后生产 `connect()` 成功（探针 P2 的仓内自动化形态）。
    #[cfg(windows)]
    #[tokio::test]
    async fn busy_pipe_retries_then_connects() -> Result<(), Box<dyn std::error::Error>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::windows::named_pipe::ServerOptions;
        let test_pipe = format!(
            "\\\\.\\pipe\\codex-ipc-test-{}",
            crate::services::codex_desktop_ipc::types::new_uuid()
        );
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&test_pipe)?;
        let ep = DesktopEndpoint {
            platform: "windows".into(),
            path: test_pipe.clone(),
            build: None,
        };
        // 客户端先发起（实例忙 → BUSY 重试）；200ms 后服务端 accept
        let sv = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            server.connect().await?;
            let (mut r, mut w) = tokio::io::split(server);
            let mut buf = vec![0u8; 16];
            let n = r.read(&mut buf).await?;
            w.write_all(&buf[..n]).await?;
            Ok::<(), std::io::Error>(())
        });
        let mut io = tokio::time::timeout(std::time::Duration::from_secs(10), super::connect(&ep))
            .await
            .expect("busy 重试必须限时成功")
            .expect("connect after busy");
        io.write_all(b"busy").await?;
        let mut buf = vec![0u8; 16];
        let n = io.read(&mut buf).await?;
        assert_eq!(&buf[..n], b"busy");
        sv.await??;
        Ok(())
    }

    /// F3：持续忙 → 重试预算耗尽诚实失败（绝不误判成功；探针 P3 形态）。
    ///
    /// Win32 语义勘正（2026-10-19，复审确定性红修复）：`CreateNamedPipe` 后实例即处于
    /// listening 态，客户端**无需**服务端 `ConnectNamedPipe` 就能连上——旧构造
    /// 「永不 accept 的服务端」根本不忙，客户端直接连成功（该测试因此恒红）。
    /// 真 BUSY 构造：单实例服务端的唯一实例被客户端 A 占住、服务端从不
    /// `DisconnectNamedPipe` → 客户端 B 每次 open 都得 `ERROR_PIPE_BUSY` →
    /// 50×50ms 重试预算耗尽。耗时断言贴预算下限（~2.5s）；错误形态：内层预算先
    /// 耗尽 → `Disconnected("…budget exhausted")`，计时抖动令外层 3s 先到 →
    /// `Timeout`（probe P3 实测 3.005s 即外层先到的形态）——两者都是预算耗尽的
    /// 诚实失败，由计时器抖动决定哪层先报。
    #[cfg(windows)]
    #[tokio::test]
    async fn sustained_busy_times_out_honestly() -> Result<(), Box<dyn std::error::Error>> {
        use tokio::net::windows::named_pipe::ServerOptions;
        let test_pipe = format!(
            "\\\\.\\pipe\\codex-ipc-test-{}",
            crate::services::codex_desktop_ipc::types::new_uuid()
        );
        // 单实例服务端：唯一实例被占后，后续 open 全部 BUSY
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&test_pipe)?;
        let ep = DesktopEndpoint {
            platform: "windows".into(),
            path: test_pipe.clone(),
            build: None,
        };
        // 客户端 A 占住唯一实例（持有到测试结束，服务端从不 DisconnectNamedPipe）
        let _occupant = super::connect(&ep).await?;
        // 客户端 B：真 BUSY → 重试预算耗尽
        let started = std::time::Instant::now();
        let r = super::connect(&ep).await;
        let elapsed = started.elapsed();
        match &r {
            Err(ProtocolError::Timeout(m)) => assert!(m.contains("timed out"), "{m}"),
            Err(ProtocolError::Disconnected(m)) => {
                assert!(
                    m.contains("budget exhausted"),
                    "诚实失败必须是预算耗尽，实际 {m}"
                )
            }
            Err(e) => panic!("持续忙的诚实失败只能是预算耗尽/超时，实际 {e}"),
            Ok(_) => panic!("持续忙绝不许误判成功"),
        }
        assert!(
            elapsed >= std::time::Duration::from_millis(2400),
            "必须耗尽 50×50ms 重试预算（下限 ~2.5s，实际 {elapsed:?}）"
        );
        drop(_occupant);
        drop(server);
        Ok(())
    }

    /// F3：读挂起不阻塞写（全双工；探针 P4 形态）——服务端延迟读，客户端写
    /// 仍即时完成。
    #[cfg(windows)]
    #[tokio::test]
    async fn read_blocked_does_not_block_write() -> Result<(), Box<dyn std::error::Error>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::windows::named_pipe::ServerOptions;
        let test_pipe = format!(
            "\\\\.\\pipe\\codex-ipc-test-{}",
            crate::services::codex_desktop_ipc::types::new_uuid()
        );
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&test_pipe)?;
        let ep = DesktopEndpoint {
            platform: "windows".into(),
            path: test_pipe,
            build: None,
        };
        // 服务端 connect 后先睡 300ms 再读（读挂起窗口）
        let sv = tokio::spawn(async move {
            server.connect().await?;
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let (mut r, mut w) = tokio::io::split(server);
            let mut buf = vec![0u8; 4096];
            let n = r.read(&mut buf).await?;
            w.write_all(&buf[..n]).await?;
            Ok::<(), std::io::Error>(())
        });
        let io = super::connect(&ep).await?;
        // split 读写半：写在独立任务完成（读挂起窗口内，不互相阻塞）
        let (mut r_half, mut w_half) = tokio::io::split(io);
        let write = tokio::spawn(async move {
            w_half.write_all(&[0u8; 4096]).await?;
            Ok::<(), std::io::Error>(())
        });
        tokio::time::timeout(std::time::Duration::from_millis(250), write)
            .await
            .expect("写必须在读挂起窗口内完成（全双工）")
            .expect("write task join")
            .expect("write io");
        let mut buf = vec![0u8; 4096];
        let n = r_half.read(&mut buf).await?;
        assert_eq!(n, 4096);
        sv.await??;
        Ok(())
    }
}
