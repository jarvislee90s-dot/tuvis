//! 发送预检的实例切换错误必须与真实发送保持相同优先级。
//!
//! 前置：请求实例 A，所选宿主探针返回 B；B 的 Send 契约匹配，但自动适配关闭。
//! 使用真实 DshControlHub 服务入口、独立内存账本和 TempDir 包束；仅隔离外部探针。
//! 不调用 support::setup、生产装配、全局数据库、真实进程扫描或原生发送。
//! Cargo 冻结期间只落测试；解除冻结后执行本测试目标验证 RED/GREEN。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use multi_agents_manager_lib::services::dsh_control::actions::{NativeDispatch, NativeSendOutcome};
use multi_agents_manager_lib::services::dsh_control::contract::{
    ContractReadReport, CONTRACT_RULE_VERSION,
};
use multi_agents_manager_lib::services::dsh_control::error::DshError;
use multi_agents_manager_lib::services::dsh_control::model::{
    ContractEvidence, DeviceContext, LocalHost, Operation, OperationContractMatch, SendPayload,
    Target,
};
use multi_agents_manager_lib::services::dsh_control::{DshControlHub, DshDb};

struct NoNativeSend;

impl NativeDispatch for NoNativeSend {
    fn send_message<'a>(
        &'a self,
        _: &'a Target,
        _: &'a SendPayload,
        _: &'a str,
    ) -> BoxFuture<'a, Result<NativeSendOutcome, DshError>> {
        Box::pin(async { panic!("实例切换预检拒绝不得触达原生发送") })
    }
}

#[tokio::test]
async fn preflight_returns_instance_switched_before_selected_hosts_closed_send_gate() {
    const REQUESTED_INSTANCE: &str = "instance-a";
    const SELECTED_INSTANCE: &str = "instance-b";
    const SESSION: &str = "session-aaaaaaaa";

    // 包束保持到测试结束，能力门前后 stat 都落在本用例的真实临时文件上。
    let root = tempfile::tempdir().expect("isolated bundle root");
    let contents = root.path().join("Dsh.app/Contents");
    std::fs::create_dir_all(contents.join("MacOS")).unwrap();
    std::fs::create_dir_all(contents.join("Resources")).unwrap();
    let executable = contents.join("MacOS/dsh");
    std::fs::write(&executable, b"isolated executable").unwrap();
    std::fs::write(contents.join("Resources/app.asar"), b"isolated package").unwrap();
    let profile_dir = root.path().join("B/profiles/desktop");
    std::fs::create_dir_all(&profile_dir).unwrap();

    let conn = rusqlite::Connection::open_in_memory().expect("isolated ledger");
    multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    let hub = DshControlHub::with_contract_read(
        DshDb::Owned(Arc::new(Mutex::new(conn))),
        Arc::new(NoNativeSend),
        Arc::new(move || {
            Ok(LocalHost {
                host_id: SELECTED_INSTANCE.into(),
                platform: "macos".into(),
                profile_dir: profile_dir.clone(),
                executable: executable.clone(),
                pid: 202,
                started_at_ms: 2_000,
                epoch: 2,
                authority: "127.0.0.1:1".into(),
                package_sha256: String::new(),
            })
        }),
        Arc::new(|host| {
            assert_eq!(host.host_id, SELECTED_INSTANCE);
            Ok(ContractReadReport {
                evidence: ContractEvidence {
                    package_sha256: "isolated-selected-b-package".into(),
                    rule_version: CONTRACT_RULE_VERSION,
                    matches: BTreeMap::from([(
                        Operation::Send,
                        OperationContractMatch {
                            method: "session/prompt".into(),
                            structural_rule: "isolated-structural-match".into(),
                            semantic_rule: "isolated-semantic-match".into(),
                        },
                    )]),
                },
                inconclusive: Default::default(),
                unresolved: Default::default(),
                budget_limited: false,
            })
        }),
        Some(Arc::new(|_, _| panic!("实例切换应先于会话归属解析拒绝"))),
    );
    hub.set_auto_adapt_source(Arc::new(|| false));
    hub.open_write_gate();

    // 用真实能力门自证前置，避免包文件缺失等无关故障遮蔽优先级回归。
    let gate = hub.send_gate_status(false).await.expect("B gate status");
    assert!(!gate.allowed, "所选 B 的发送能力门必须关闭");
    assert_eq!(
        gate.reason_code.as_deref(),
        Some("operation_auto_adapt_off")
    );

    // 空账本 + 全新 action_id：真实发送必须走新写准入，而非幂等重放。
    let send_error = hub
        .dispatch_session_send_for_instance(
            DeviceContext {
                device_id: "isolated-device".into(),
                device_name: "Isolated device".into(),
            },
            SESSION,
            "fresh-instance-switch-action",
            SendPayload {
                text: "instance A message".into(),
                receipt_ids: vec![],
                queue_only: false,
                bypass_paused_queue: false,
            },
            false,
            Some(REQUESTED_INSTANCE),
        )
        .await
        .expect_err("真实发送应先报告 A/B 实例不一致");
    assert_eq!(send_error.code, "DSH_INSTANCE_SWITCHED");

    // 回归断言：若先返回 B 的关闭门状态，会得到 Ok(denied)，在这里失败。
    let preflight_error = hub
        .send_session_gate_status(false, SESSION, Some(REQUESTED_INSTANCE))
        .await
        .expect_err("预检必须与真实发送一致，优先报告实例切换，而非返回 B 的关闭门状态");
    assert_eq!(preflight_error.code, "DSH_INSTANCE_SWITCHED");
}
