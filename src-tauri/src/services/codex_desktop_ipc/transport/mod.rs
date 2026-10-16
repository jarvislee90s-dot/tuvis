//! 本机 endpoint 解析 / 平台身份 / 连接（platform-specific 实现见 `unix` / `windows`）。
//!
//! 安全纪律（spec S1）：endpoint 解析只读本机文件系统与运行进程，不接受任何 HTTP
//! 输入；连接前校验 socket 归属与权限（拒绝非同 uid、非 socket、宽权限），**不更改
//! 用户权限**。身份纪律：Desktop build 从运行中的官方进程 bundle 读取；来源缺失或
//! 多义 → `build=None` → 对应 profile 不命中 → A 写能力关闭（不能伪装成已验证版本）。
//!
//! 两个平台模块**全平台编译**（交叉 check 覆盖对面语法），非本机平台的方法返回
//! 诚实错误——不假装可用。

use super::types::ProtocolError;
use tokio::io::{AsyncRead, AsyncWrite};

/// 全双工 IPC 流抽象（Windows Named Pipe / Unix socket 的公共底座）。
pub trait IpcIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IpcIo for T {}

pub type BoxIo = Box<dyn IpcIo>;

/// 本机 endpoint：解析产物，路径与平台来自本机事实，**不接收网络输入**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEndpoint {
    /// "macos" | "windows"（与 profile 平台键一致）
    pub platform: String,
    /// Unix socket 绝对路径 / Windows named pipe 名
    pub path: String,
    /// 运行中官方 Desktop 的 bundle/exe 版本；`None` = 来源缺失或多义（能力全关）
    pub build: Option<String>,
}

/// endpoint 解析器。生产用各平台默认实现；测试注入路径/家目录。
pub trait EndpointResolver: Send + Sync {
    fn resolve(&self) -> Result<DesktopEndpoint, ProtocolError>;
}

/// 连接超时（计划「超时/恢复」：connect 3s）。
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// 按平台分发连接。
pub async fn connect(endpoint: &DesktopEndpoint) -> Result<BoxIo, ProtocolError> {
    match endpoint.platform.as_str() {
        "macos" => unix::connect(endpoint).await,
        "windows" => windows::connect(endpoint).await,
        other => Err(ProtocolError::Disconnected(format!(
            "unknown transport platform: {other}"
        ))),
    }
}

pub mod unix;
pub mod windows;
