//! OpenCode 服务插件桥（B1 可信初始绑定）。
//!
//! 设计边界（2026-11-02 决策）：
//! * 用户在 GUI 明确选择固定的 **service + session**；桥只证明“这个 session 由这个
//!   已登记的服务进程提供”，不跟随桌面前台、不要求每次导航都撤销授权。
//! * 服务内一次性安装的插件通过 Unix socket 连接本进程。兔维斯用内核 peer 事实
//!   （PID/UID/启动时间/可执行路径）核对连接确实来自登记文件里的 service PID，
//!   插件再用自己的 `ctx.session` 复核 session/directory；两侧一致才建立绑定。
//! * 插件回执不能阻止同进程恶意插件的先行副作用；本轮产品边界是可信宿主 + 明确
//!   安装的桥代码，不把“抵御 OpenCode 进程内任意恶意代码”加入基础收发的无限门槛。
//! * 断链/服务重启/插件重载后旧绑定失效；已 claim 的动作只读核对，不自动重放。
//!
//! 该模块只提供证明与确认，不直接执行会话写；写仍走现有 `NativeHub` 动作账本和
//! HTTP 一次派发，接收侧在 reserve/claim 前调用这里的 `confirm_session`。

use std::path::{Path, PathBuf};

use super::types::{NativeError, RuntimeKey};

#[cfg(unix)]
#[path = "bridge_unix.rs"]
mod unix;
#[cfg(unix)]
pub use unix::BridgeRuntime;

/// 桥的文件布局。生产用 `~/.tuvis/opencode-bridge`；测试显式注入临时根。
#[derive(Clone, Debug)]
pub struct BridgePaths {
    pub dir: PathBuf,
    pub socket: PathBuf,
    pub config: PathBuf,
    /// OpenCode `plugins` 里登记的目录（含 `index.js`）；loader 接受目录形式。
    pub plugin_dir: PathBuf,
    /// 目录内的实际入口文件。
    pub plugin_file: PathBuf,
}

impl BridgePaths {
    pub fn production(home: &Path) -> Self {
        let dir = home.join(".tuvis").join("opencode-bridge");
        Self::in_dir(dir)
    }

    pub fn in_dir(dir: PathBuf) -> Self {
        let plugin_dir = dir.join("plugin");
        Self {
            socket: dir.join("bridge.sock"),
            config: dir.join("bridge.json"),
            plugin_file: plugin_dir.join("index.js"),
            plugin_dir,
            dir,
        }
    }
}

/// 已由插件确认的固定目标。
#[derive(Clone, Debug)]
pub struct ConfirmedTarget {
    pub runtime: RuntimeKey,
    pub incarnation: String,
    pub location: PathBuf,
}

/// 归属证明接口。生产是 [`BridgeRuntime`]；测试可注入静态证明，但生产构造默认无证明。
pub trait OwnershipProof: Send + Sync {
    fn attested(&self) -> bool;
    fn runtime(&self) -> Option<RuntimeKey>;
    fn confirm<'a>(
        &'a self,
        session_id: &'a str,
        directory: &'a Path,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ConfirmedTarget, NativeError>> + Send + 'a>,
    >;
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeStatus {
    pub attested: bool,
    pub runtime: Option<RuntimeKey>,
    pub incarnation: Option<String>,
    pub location: Option<String>,
    pub peer_pid: Option<u32>,
    pub last_error: Option<String>,
}

/// Windows keeps its existing terminal compatibility path. The native bridge uses Unix peer identity.
#[cfg(not(unix))]
pub enum BridgeRuntime {}

#[cfg(not(unix))]
impl BridgeRuntime {
    pub async fn start(
        _paths: BridgePaths,
        _input: super::types::DiscoveryInput,
        _process_fact: std::sync::Arc<dyn Fn(u32) -> Option<i64> + Send + Sync>,
    ) -> Result<std::sync::Arc<Self>, String> {
        Err("OpenCode native bridge requires Unix peer verification".into())
    }

    pub async fn shutdown(&self) {
        match *self {}
    }

    pub fn is_stopped(&self) -> bool {
        match *self {}
    }

    pub async fn status(&self) -> BridgeStatus {
        match *self {}
    }
}
