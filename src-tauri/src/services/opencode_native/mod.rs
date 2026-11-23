//! OpenCode 原生交互（macOS 首批）。
//!
//! 本模块只承载 OpenCode 默认后台 service 的原生读写：发现、传输、投影、动作与
//! 答题校验按计划逐任务拆分到独立文件。设计依据
//! `docs/engineering/plans/2026-10-29-OpenCode-macOS原生交互首批实施计划.md`。

#[cfg(all(feature = "opencode-coordinator-tests", not(debug_assertions)))]
compile_error!("opencode-coordinator-tests is forbidden in release builds");

pub mod actions;
pub mod attribution_evidence;
pub mod binding_gate;
pub mod bridge;
pub mod bridge_install;
pub mod budget;
pub mod client;
pub mod contract;
pub mod discovery;
pub mod forms;
pub mod outbox;
pub mod policy;
pub mod projection;
pub mod types;

pub use actions::{
    abandon_reasons, action_codes, reason_words, NativeActionStore, ReserveOutcome,
    NOT_ACCEPTED_EVIDENCE,
};
/// 写门锁定窗口身份：装门测试缝（`arm_binding_gate_for_tests`）的类型面。
#[cfg(any(test, feature = "opencode-coordinator-tests"))]
pub use binding_gate::{LockedWindow, LockedWindowLookup};
pub use bridge::{BridgePaths, BridgeRuntime, BridgeStatus, ConfirmedTarget, OwnershipProof};
pub use bridge_install::{BridgeInstall, BridgeInstallStatus};
pub use client::{NativeClient, NativeWriteReply};
pub use contract::{evaluate_contract, parse_form};
pub use discovery::{
    bind_session, discover, discover_explicit_client, explicit_service_identity, production_input,
    production_input_from, state_file_for,
};
pub use forms::{same_form_context, validate_answers};
pub use projection::{
    project_messages, ExplicitEnv, ExplicitEvidenceSources, ExplicitInfoFetch, ExplicitScan,
    NativeHub, RouteClaim, ROUTE_DECLINED_LEGACY_PENDING,
};
pub use types::*;
