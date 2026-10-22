//! macOS transport：Unix domain socket（`$CODEX_HOME/ipc/ipc.sock`，默认
//! `~/.codex/ipc/ipc.sock`；旧 tmpdir 路径仅作回退）。探测定案：socket 由 Desktop
//! 主进程每次启动重建，目录 0700、socket 0600、同 uid；宽权限/异 uid/非 socket 一律
//! 拒绝连接，且**不代用户改权限**。非 macOS 平台编译面返回诚实错误。

use super::{DesktopEndpoint, EndpointResolver};
#[allow(unused_imports)]
use crate::services::codex_desktop_ipc::types::ProtocolError;
use std::path::{Path, PathBuf};

/// 官方 Desktop bundle id（Mac 探针 OBSERVED：com.openai.codex）。
pub const CODEX_BUNDLE_ID: &str = "com.openai.codex";

/// 官方 Desktop bundle 的 `Contents/Info.plist` 文本（缺文件/读不了 → `None`）。
///
/// 已知边界：仅支持 **XML plist**（官方现发 XML）。若未来改发二进制 plist
/// （`bplist00`），子串搜索会失配 → 身份判定如实为 false → 降级为缺 build/指纹，
/// 不会误判成官方。
fn read_info_plist(bundle: &Path) -> Option<String> {
    std::fs::read_to_string(bundle.join("Contents").join("Info.plist")).ok()
}

/// 官方 Codex Desktop bundle 的身份判据：`Info.plist` 的 `CFBundleIdentifier` 精确
/// 等于 [`CODEX_BUNDLE_ID`]。**不按显示名猜**——官方安装也可能叫 `ChatGPT.app`
/// （PR #178 真机验收实测）；helper 的嵌套 bundle id 不符 → 不认。
pub fn is_official_codex_bundle(bundle: &Path) -> bool {
    read_info_plist(bundle)
        .and_then(|text| plist_value(&text, "CFBundleIdentifier"))
        .as_deref()
        == Some(CODEX_BUNDLE_ID)
}

/// 进程 exe 路径 → 所属 `.app` bundle（**只做路径形态截取**，不读文件/不验身份）：
/// 取首个 `.app/` 前缀——嵌套在外层包内的 helper `.app` 会归并到外层主 bundle。
/// endpoint resolver（[`platform::desktop_build`]）与探测指纹（fingerprint 的随包
/// CLI 定位）共用本截取 + [`is_official_codex_bundle`] 身份判据，不许各自再抄一份
/// （会漂移）。调用方应先对截取结果**排序去重**再验身份（每进程一次截取、每唯一
/// bundle 一次 Info.plist 读）。
pub fn app_bundle_prefix(exe: &str) -> Option<PathBuf> {
    let end = exe.find(".app/")?;
    Some(PathBuf::from(&exe[..end + 4]))
}

/// 极简 XML Info.plist 取值（无新依赖）：`<key>K</key>` 之后首个 `<string>` 的值；
/// 空串视为缺失。
pub fn plist_value(text: &str, key: &str) -> Option<String> {
    let needle = format!("<key>{key}</key>");
    let pos = text.find(&needle)?;
    let rest = &text[pos + needle.len()..];
    let open = rest.find("<string>")? + "<string>".len();
    let close = rest[open..].find("</string>")? + open;
    let v = rest[open..close].trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

// 当前有效用户 id：免 libc 依赖的 extern 声明（仅 unix 编译）。
#[cfg(unix)]
extern "C" {
    fn geteuid() -> u32;
}

#[cfg(unix)]
fn current_uid() -> u32 {
    unsafe { geteuid() }
}

/// 可注入的解析器：`codex_home=None` 时用 `$CODEX_HOME`，缺省 `~/.codex`；
/// `legacy_tmpdir=None` 时用系统 tmpdir（测试注入自己的 tempdir，不触碰真实路径）。
pub struct UnixEndpointResolver {
    pub codex_home: Option<PathBuf>,
    #[cfg(unix)]
    pub legacy_tmpdir: Option<PathBuf>,
}

#[cfg(unix)]
impl EndpointResolver for UnixEndpointResolver {
    fn resolve(&self) -> Result<DesktopEndpoint, ProtocolError> {
        let home = platform::default_codex_home(self.codex_home.as_deref());
        let tmpdir = self
            .legacy_tmpdir
            .clone()
            .unwrap_or_else(std::env::temp_dir);
        let candidates = platform::candidate_paths(&home, &tmpdir);
        let mut last_err =
            ProtocolError::Disconnected("IPC endpoint not found (Desktop not running?)".into());
        for path in candidates {
            match platform::validate_socket(&path) {
                Ok(()) => {
                    return Ok(DesktopEndpoint {
                        platform: "macos".into(),
                        path: path.to_string_lossy().into_owned(),
                        // 身份与 endpoint 归属绑定：读不到官方 build → 能力全关
                        build: platform::desktop_build(),
                    });
                }
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }
}

#[cfg(not(unix))]
impl EndpointResolver for UnixEndpointResolver {
    fn resolve(&self) -> Result<DesktopEndpoint, ProtocolError> {
        Err(ProtocolError::Disconnected(
            "unix socket transport is macOS-only".into(),
        ))
    }
}

/// 连接 Unix socket，超时见 [`super::CONNECT_TIMEOUT`]。
#[cfg(unix)]
pub async fn connect(endpoint: &DesktopEndpoint) -> Result<Box<dyn super::IpcIo>, ProtocolError> {
    use tokio::net::UnixStream;
    let path = std::path::PathBuf::from(&endpoint.path);
    let connect_fut = UnixStream::connect(&path);
    match tokio::time::timeout(super::CONNECT_TIMEOUT, connect_fut).await {
        Ok(Ok(stream)) => Ok(Box::new(stream)),
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

#[cfg(not(unix))]
pub async fn connect(endpoint: &DesktopEndpoint) -> Result<Box<dyn super::IpcIo>, ProtocolError> {
    let _ = endpoint;
    Err(ProtocolError::Disconnected(
        "unix socket transport is macOS-only".into(),
    ))
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::path::{Path, PathBuf};

    /// 默认 CODEX_HOME（`$CODEX_HOME` → `~/.codex`）。
    pub fn default_codex_home(explicit: Option<&Path>) -> PathBuf {
        if let Some(p) = explicit {
            return p.to_path_buf();
        }
        if let Ok(env_home) = std::env::var("CODEX_HOME") {
            if !env_home.trim().is_empty() {
                return PathBuf::from(env_home);
            }
        }
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(".codex")
    }

    /// 主路径：`<home>/ipc/ipc.sock`；回退：`<tmpdir>/codex-ipc/ipc-<uid>.sock`。
    /// 回退仅在主路径**确实不存在**时检查（主路径在场但不合规 → 如实拒绝，
    /// 不静默改连旧路径）；`tmpdir` 可注入（测试不触碰真实机器路径）。
    pub fn candidate_paths(home: &Path, tmpdir: &Path) -> Vec<PathBuf> {
        let primary = home.join("ipc").join("ipc.sock");
        if primary.exists() {
            return vec![primary];
        }
        let uid = current_uid();
        vec![tmpdir.join("codex-ipc").join(format!("ipc-{uid}.sock"))]
    }

    /// socket 安全校验：必须是 socket、同 euid、目录与 socket 都不得 group/other
    /// 可写（0600/0700 为期望形态；**只拒绝，不修改**）。
    pub fn validate_socket(path: &Path) -> Result<(), ProtocolError> {
        use std::os::unix::fs::FileTypeExt;
        use std::os::unix::fs::PermissionsExt;
        let sock_meta = std::fs::metadata(path).map_err(|e| {
            ProtocolError::Disconnected(format!("IPC endpoint unreachable {}: {e}", path.display()))
        })?;
        if !sock_meta.file_type().is_socket() {
            return Err(ProtocolError::Disconnected(format!(
                "IPC endpoint is not a socket: {}",
                path.display()
            )));
        }
        let mode = sock_meta.permissions().mode();
        if mode & 0o022 != 0 {
            return Err(ProtocolError::Disconnected(format!(
                "IPC socket permissions too wide ({:o}): refusing, not fixing permissions",
                mode & 0o777
            )));
        }
        let uid = current_uid();
        use std::os::unix::fs::MetadataExt;
        if sock_meta.uid() != uid {
            return Err(ProtocolError::Disconnected(
                "IPC socket not owned by current user: refusing".into(),
            ));
        }
        if let Some(dir) = path.parent() {
            let dir_meta = std::fs::metadata(dir).map_err(|e| {
                ProtocolError::Disconnected(format!("IPC directory unreachable: {e}"))
            })?;
            let dir_mode = dir_meta.permissions().mode();
            if dir_mode & 0o022 != 0 {
                return Err(ProtocolError::Disconnected(format!(
                    "IPC directory permissions too wide ({:o}): refusing",
                    dir_mode & 0o777
                )));
            }
        }
        Ok(())
    }

    /// 读取运行中官方 Desktop 的 bundle 版本：扫进程 exe 路径含 `.app/Contents/MacOS/`
    /// 的候选，读其 `Info.plist` 的 CFBundleIdentifier + CFBundleShortVersionString，
    /// 以 bundle id 定身份；多义（多个不同版本）→ None。
    pub fn desktop_build() -> Option<String> {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
        let mut sys = System::new();
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
        );
        // 先截取 .app 前缀并按安装路径去重，再对唯一 bundle 读 Info.plist
        // （每唯一安装一次读盘，非每进程一次）。
        let mut bundles: Vec<PathBuf> = sys
            .processes()
            .values()
            .filter_map(|p| p.exe())
            .filter_map(|exe| super::app_bundle_prefix(&exe.to_string_lossy()))
            .collect();
        bundles.sort();
        bundles.dedup();
        let mut versions: Vec<String> = bundles
            .iter()
            .filter_map(|bundle| bundle_version(bundle))
            .collect();
        versions.sort();
        versions.dedup();
        if versions.len() == 1 {
            versions.pop()
        } else {
            None // 0 个 = 来源缺失；>1 个 = 多义——一律关闭
        }
    }

    /// bundle 版本（`CFBundleShortVersionString`）——身份判据与
    /// [`super::official_codex_bundle`] 同源（bundle id 精确匹配）；版本缺失 → `None`。
    fn bundle_version(bundle: &Path) -> Option<String> {
        let text = super::read_info_plist(bundle)?;
        if super::plist_value(&text, "CFBundleIdentifier")?.as_str() != CODEX_BUNDLE_ID {
            return None;
        }
        super::plist_value(&text, "CFBundleShortVersionString")
    }
}

#[cfg(unix)]
pub use platform::{candidate_paths, default_codex_home, desktop_build, validate_socket};

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::path::Path;

    #[cfg(unix)]
    fn make_resolver(home: &Path, tmpdir: &Path) -> UnixEndpointResolver {
        UnixEndpointResolver {
            codex_home: Some(home.to_path_buf()),
            legacy_tmpdir: Some(tmpdir.to_path_buf()),
        }
    }

    #[cfg(unix)]
    fn bind_sock(dir: &Path, name: &str) -> std::os::unix::net::UnixListener {
        std::os::unix::net::UnixListener::bind(dir.join(name)).unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolves_codex_home_socket_then_legacy_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let tmpdir = tmp.path().join("tmproot");
        std::fs::create_dir_all(&tmpdir).unwrap();
        let home = tmp.path().join("codex-home");
        std::fs::create_dir_all(home.join("ipc")).unwrap();
        let _listener = bind_sock(&home.join("ipc"), "ipc.sock");
        let r = make_resolver(&home, &tmpdir).resolve().unwrap();
        assert_eq!(r.platform, "macos");
        assert_eq!(r.path, home.join("ipc").join("ipc.sock").to_string_lossy());

        // 主路径不存在 → 回退**注入 tmpdir** 下的旧路径（绝不触碰真实机器路径）
        let legacy_dir = tmpdir.join("codex-ipc");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let uid = current_uid();
        let legacy = legacy_dir.join(format!("ipc-{uid}.sock"));
        let _l2 = std::os::unix::net::UnixListener::bind(&legacy).unwrap();
        let home2 = tmp.path().join("empty-home");
        std::fs::create_dir_all(&home2).unwrap();
        let r2 = make_resolver(&home2, &tmpdir).resolve().unwrap();
        assert_eq!(r2.path, legacy.to_string_lossy());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_non_socket_wrong_perms_and_missing_endpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let tmpdir = tmp.path().join("tmproot-empty");
        std::fs::create_dir_all(&tmpdir).unwrap();
        let home = tmp.path().join("codex-home");
        let ipc = home.join("ipc");
        std::fs::create_dir_all(&ipc).unwrap();

        // 非 socket 普通文件：在场即判，不回退旧路径
        std::fs::write(ipc.join("ipc.sock"), b"not a socket").unwrap();
        assert!(make_resolver(&home, &tmpdir).resolve().is_err());

        // 宽权限 socket（0666）——只拒绝，不改权限
        std::fs::remove_file(ipc.join("ipc.sock")).unwrap();
        let _l = bind_sock(&ipc, "ipc.sock");
        let sock_path = ipc.join("ipc.sock");
        std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(make_resolver(&home, &tmpdir).resolve().is_err());
        assert_eq!(
            std::fs::metadata(&sock_path).unwrap().permissions().mode() & 0o777,
            0o666,
            "校验失败后不得修改用户权限"
        );
        std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        // 宽权限目录（0777）
        let wide_home = tmp.path().join("wide-home");
        std::fs::create_dir_all(wide_home.join("ipc")).unwrap();
        let _l3 = bind_sock(&wide_home.join("ipc"), "ipc.sock");
        std::fs::set_permissions(
            wide_home.join("ipc"),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        assert!(make_resolver(&wide_home, &tmpdir).resolve().is_err());

        // endpoint 缺失（主路径与注入 tmpdir 旧路径都不存在）
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(make_resolver(&empty, &tmpdir).resolve().is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn connect_roundtrip_within_timeout() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let tmp = tempfile::tempdir().unwrap();
        let ipc = tmp.path().join("ipc");
        std::fs::create_dir_all(&ipc).unwrap();
        let listener = tokio::net::UnixListener::bind(ipc.join("ipc.sock")).unwrap();
        let ep = DesktopEndpoint {
            platform: "macos".into(),
            path: ipc.join("ipc.sock").to_string_lossy().into_owned(),
            build: Some("26.1013.31256".into()),
        };
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let mut sock = sock;
            let mut buf = vec![0u8; 64];
            let n = sock.read(&mut buf).await.unwrap();
            sock.write_all(&buf[..n]).await.unwrap();
        });
        let mut io = connect(&ep).await.unwrap();
        io.write_all(b"ping").await.unwrap();
        let mut buf = vec![0u8; 64];
        let n = io.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"ping");
        server.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn duplex_does_not_block_write() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // 同一连接：reader 阻塞等待期间，split 出的 writer 仍能即时写出完整帧
        // （connection actor 的 reader/writer 分离正是建立在该语义上）
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("duplex.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (mut sr, mut sw) = tokio::io::split(sock);
            // 服务端只在收到一帧后才回（期间客户端 reader 一直在等）
            let mut buf = vec![0u8; 64];
            let n = sr.read(&mut buf).await.unwrap();
            sw.write_all(&buf[..n]).await.unwrap();
        });
        let ep = DesktopEndpoint {
            platform: "macos".into(),
            path: path.to_string_lossy().into_owned(),
            build: None,
        };
        let io = connect(&ep).await.unwrap();
        let (mut rh, mut wh) = tokio::io::split(io);
        // reader 阻塞等回包（服务端收到一帧后才回——期间 writer 必须不被阻塞）
        let read_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 64];
            let n = rh.read(&mut buf).await.unwrap();
            buf.truncate(n);
            buf
        });
        let started = std::time::Instant::now();
        wh.write_all(b"payload").await.unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "writer 被 reader 阻塞"
        );
        let echoed = read_task.await.unwrap();
        assert_eq!(echoed, b"payload");
        server.await.unwrap();
    }
}
