//! Windows transport：Named Pipe `\\.\pipe\codex-ipc`（Tokio async ClientOptions，
//! Overlapped——不得同步单 handle 一读一写）。**Windows 原生测试必须在 Windows 真机
//! 运行**；非 Windows 平台只保留编译面（交叉 check 覆盖），不谎报运行证据。
//!
//! 身份：build 从运行中的 ChatGPT.exe 版本资源读取（Win32 VerQueryValue 固定信息块，
//! 四段版本号与 Windows probe 记录的 `26.1013.3656.0` 同形）；来源缺失/多义 →
//! `build=None` → profile 不命中 → A 写能力关闭（spec F8，不伪装已验证）。

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

/// 运行中官方 Desktop 的 exe 版本（多义 → None；缺失 → None）。
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
        if let Some(v) = exe_file_version(&exe) {
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

/// 读 exe 版本资源固定信息块（VS_FIXEDFILEINFO 四段版本号）。
#[cfg(windows)]
fn exe_file_version(exe: &std::path::Path) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
    };
    unsafe {
        let wide: Vec<u16> = exe
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let filename = PCWSTR::from_raw(wide.as_ptr());
        let mut handle = 0u32;
        let size = GetFileVersionInfoSizeW(filename, Some(&mut handle));
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        if GetFileVersionInfoW(filename, Some(handle), size, data.as_mut_ptr().cast()).is_err() {
            return None;
        }
        let mut info_ptr: *mut VS_FIXEDFILEINFO = std::ptr::null_mut();
        let mut len = 0u32;
        let root: Vec<u16> = "\\".encode_utf16().chain(std::iter::once(0)).collect();
        if !VerQueryValueW(
            data.as_ptr().cast(),
            PCWSTR::from_raw(root.as_ptr()),
            std::ptr::addr_of_mut!(info_ptr).cast::<std::ffi::c_void>()
                as *mut *mut std::ffi::c_void,
            &mut len,
        )
        .as_bool()
        {
            return None;
        }
        if info_ptr.is_null() || len == 0 {
            return None;
        }
        let info = &*info_ptr;
        let ms = info.dwFileVersionMS;
        let ls = info.dwFileVersionLS;
        Some(format!(
            "{}.{}.{}.{}",
            ms >> 16,
            ms & 0xffff,
            ls >> 16,
            ls & 0xffff
        ))
    }
}

/// 连接 named pipe，超时见 [`super::CONNECT_TIMEOUT`]；ERROR_PIPE_BUSY 按官方
/// 模式等待重试。
#[cfg(windows)]
pub async fn connect(endpoint: &DesktopEndpoint) -> Result<Box<dyn super::IpcIo>, ProtocolError> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let path = endpoint.path.clone();
    let open = async move {
        // 命名管道实例忙时轮询重试（ERROR_PIPE_BUSY = 2231 / 0x8B7）
        for _ in 0..50 {
            match ClientOptions::new().open(&path) {
                Ok(client) => return Ok(client),
                Err(e) if e.raw_os_error() == Some(2231) => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => return Err(e),
            }
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
    use super::{EndpointResolver, WindowsEndpointResolver, PIPE_NAME};

    #[cfg(windows)]
    #[tokio::test]
    async fn connects_to_fake_pipe_server_and_echoes() -> Result<(), Box<dyn std::error::Error>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::windows::named_pipe::ServerOptions;

        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(PIPE_NAME)?;
        let resolver = WindowsEndpointResolver;
        let ep = resolver.resolve()?;
        assert_eq!(ep.path, PIPE_NAME);

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
}
