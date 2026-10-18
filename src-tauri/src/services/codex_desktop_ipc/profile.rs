//! 平台 profile 与按方法版本门（探测定案：版本不匹配表现为 `no-client-found`，
//! 不能判定 owner 不存在；`thread-follower-*` 携带 envelope hostId 时按基值 +1）。
//!
//! profile 常量是**当前协议基线**：`false` = 该 build 尚未活体验证，不是协议不存在；
//! 只有 Task 10 真机闭环 PASS 才翻转对应 flag，测试可用注入 profile 测构造器。

use crate::services::codex_desktop_ipc::types::{CapabilitySet, ProtocolError};
use std::collections::BTreeMap;

/// 已验证 Desktop build 的能力/版本基线；未识别 build 返回 `None`（能力全关）。
#[derive(Clone)]
pub struct ProtocolProfile {
    pub platform: String,
    pub build: String,
    pub capabilities: CapabilitySet,
    pub method_versions: BTreeMap<String, u32>,
}

impl ProtocolProfile {
    /// 能力全关的诚实缺省（R3 定案）：未识别 build / 无 endpoint / 连接失败时使用。
    /// caps 全 false（A 写全关、投影 CompatOnly），但保留标准方法表——只读同步仍可。
    pub fn closed() -> Self {
        Self {
            platform: std::env::consts::OS.to_string(),
            build: String::new(),
            capabilities: CapabilitySet {
                send: false,
                state: false,
                command: false,
                permissions: false,
                blocking_input: false,
            },
            method_versions: method_table(),
        }
    }

    /// 按方法取版本。`envelope_host` 为 `Some` 时仅 `thread-follower-*` 基值 +1
    /// （本地 follower request envelope 不发 hostId，走基值——两种位置都要钉住）。
    pub fn version(&self, method: &str, envelope_host: Option<&str>) -> Result<u32, ProtocolError> {
        let base = *self
            .method_versions
            .get(method)
            .ok_or_else(|| ProtocolError::UnsupportedMethod(method.to_string()))?;
        if envelope_host.is_some() && method.starts_with("thread-follower-") {
            Ok(base + 1)
        } else {
            Ok(base)
        }
    }
}

/// 协议方法版本表（与平台无关；探测定案 + 计划「协议与工程值」）。
/// `load-complete-history` / command 变体 / Plan blocking input 在 Mac 未实测，
/// 版本常量按表登记但能力 flag 保持 false——未验证不启用。
fn method_table() -> BTreeMap<String, u32> {
    BTreeMap::from([
        ("thread-owner-discovery".to_string(), 1),
        ("thread-stream-following-changed".to_string(), 1),
        ("thread-stream-following-status-requested".to_string(), 1),
        ("thread-stream-state-changed".to_string(), 11),
        ("thread-follower-start-turn".to_string(), 2),
        ("thread-follower-load-complete-history".to_string(), 1),
        ("thread-follower-command-approval-decision".to_string(), 1),
        (
            "thread-follower-permissions-request-approval-response".to_string(),
            1,
        ),
        ("thread-follower-submit-user-input".to_string(), 1),
        // broadcast/信号面（接收为主）
        ("client-status-changed".to_string(), 0),
        ("ipc-connection-reset".to_string(), 1),
    ])
}

/// Windows 基线：command / blocking-input 已实测 PASS，permissions 未实测。
fn windows_profile(build: &str) -> ProtocolProfile {
    ProtocolProfile {
        platform: "windows".to_string(),
        build: build.to_string(),
        capabilities: CapabilitySet {
            send: true,
            state: true,
            command: true,
            permissions: false,
            blocking_input: true,
        },
        method_versions: method_table(),
    }
}

/// macOS 基线：permissions / start-turn / following 已实测 PASS；command 变体与
/// blocking input 未实测（NOT_TRIGGERED），flag 保持 false。
fn macos_profile(build: &str) -> ProtocolProfile {
    ProtocolProfile {
        platform: "macos".to_string(),
        build: build.to_string(),
        capabilities: CapabilitySet {
            send: true,
            state: true,
            command: false,
            permissions: true,
            blocking_input: false,
        },
        method_versions: method_table(),
    }
}

/// macOS `26.1013.40934`（2026-10-18 实机登记）：**只读方法 + start-turn v2 已受控
/// harness 实证**（initialize/discovery/following/snapshot/load-complete-history 由
/// 复审探针验证；start-turn 由实现侧探针实证——turn 成功创建 inProgress，23:58）。
/// **permissions/command/blocking_input 在该 build 未实测，flag 保持 false**——
/// 旧 build 的 PASS 不跨 build 迁移；审批/问答按证据逐项解锁。
fn macos_40934_profile(build: &str) -> ProtocolProfile {
    ProtocolProfile {
        platform: "macos".to_string(),
        build: build.to_string(),
        capabilities: CapabilitySet {
            send: true,
            state: true,
            command: false,
            permissions: false,
            blocking_input: false,
        },
        method_versions: method_table(),
    }
}

/// 精确 build 匹配（不是版本下限——升级后的 build 未经验证必须重新门控）。
pub fn profile_for(platform: &str, build: &str) -> Option<ProtocolProfile> {
    match (platform, build) {
        ("windows", "26.1013.3656.0") => Some(windows_profile(build)),
        ("macos", "26.1013.31256") => Some(macos_profile(build)),
        ("macos", "26.1013.40934") => Some(macos_40934_profile(build)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::codex_desktop_ipc::types::PendingRequestId;

    #[test]
    fn closed_profile_has_zero_caps_but_standard_method_table() {
        let c = ProtocolProfile::closed();
        assert!(!c.capabilities.send, "closed：A 写全关");
        assert!(!c.capabilities.state);
        assert!(!c.capabilities.command);
        assert!(!c.capabilities.permissions);
        assert!(!c.capabilities.blocking_input);
        // 只读同步仍可：标准方法表在场
        assert_eq!(c.version("thread-owner-discovery", None).unwrap(), 1);
        assert_eq!(c.version("thread-stream-state-changed", None).unwrap(), 11);
        assert_eq!(
            c.version("thread-follower-load-complete-history", None)
                .unwrap(),
            1
        );
    }

    #[test]
    fn unverified_or_unknown_builds_are_none() {
        assert!(profile_for("macos", "26.1013.31257").is_none());
        assert!(profile_for("macos", "26.1013.40935").is_none());
        // 40934 精确命中：send/state 开（harness 实证），审批/问答关（未实测）
        let p = profile_for("macos", "26.1013.40934").expect("registered");
        assert!(p.capabilities.send && p.capabilities.state);
        assert!(!p.capabilities.permissions);
        assert!(!p.capabilities.command);
        assert!(!p.capabilities.blocking_input);
        assert!(profile_for("windows", "26.1013.3655.0").is_none());
        assert!(profile_for("linux", "26.1013.31256").is_none());
        assert!(profile_for("macos", "").is_none());
        // 已验证 build 精确命中
        assert!(profile_for("macos", "26.1013.31256").is_some());
        assert!(profile_for("windows", "26.1013.3656.0").is_some());
    }

    #[test]
    fn version_gate_base_and_envelope_host_plus_one() {
        let p = profile_for("macos", "26.1013.31256").unwrap();
        // 本地 follower request：envelope 不发 hostId → 基值
        assert_eq!(p.version("thread-follower-start-turn", None).unwrap(), 2);
        // 构造器显式携带 envelope hostId：仅 follower 方法按基值 +1
        assert_eq!(
            p.version("thread-follower-start-turn", Some("local"))
                .unwrap(),
            3
        );
        // 非 follower 方法不受 envelope hostId 影响
        assert_eq!(p.version("thread-stream-state-changed", None).unwrap(), 11);
        assert_eq!(
            p.version("thread-stream-state-changed", Some("local"))
                .unwrap(),
            11
        );
        assert_eq!(p.version("client-status-changed", None).unwrap(), 0);
        // 未登记方法拒绝，不盲发
        assert_eq!(
            p.version("thread-follower-interrupt-turn", None),
            Err(ProtocolError::UnsupportedMethod(
                "thread-follower-interrupt-turn".to_string()
            ))
        );
    }

    #[test]
    fn profiles_keep_unverified_methods_disabled() {
        let mac = profile_for("macos", "26.1013.31256").unwrap();
        assert_eq!(mac.platform, "macos");
        assert!(mac.capabilities.send);
        assert!(mac.capabilities.state);
        assert!(mac.capabilities.permissions);
        assert!(!mac.capabilities.command);
        assert!(!mac.capabilities.blocking_input);

        let win = profile_for("windows", "26.1013.3656.0").unwrap();
        assert!(win.capabilities.send);
        assert!(win.capabilities.state);
        assert!(win.capabilities.command);
        assert!(win.capabilities.blocking_input);
        assert!(!win.capabilities.permissions);
    }

    #[test]
    fn request_id_key_shapes() {
        assert_eq!(
            PendingRequestId::from_key("n:9007199254740993")
                .unwrap()
                .to_key(),
            "n:9007199254740993"
        );
        assert_ne!(
            PendingRequestId::from_key("n:12").unwrap(),
            PendingRequestId::from_key("s:12").unwrap()
        );
    }
}
