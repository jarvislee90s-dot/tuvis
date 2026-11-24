//! DSH 原生契约与领域 DTO 契约测试（T01；计划 §2.2）。
//!
//! 三个 RED 用例：`response_rejects_foreign_rpc_id`（codec 身份闸）、
//! `dto_preserves_native_request_identity`（原生 ID 字符串不改写）、
//! `unknown_variant_is_not_action`（动作闭集）。夹具 =
//! `tests/fixtures/dsh-native/0.2.1-rc.3/`（来源/真伪/未测项见其 README.md；
//! 本测试不连接任何真实 DSH 实例，全部输入来自脱敏夹具/内存构造）。

use multi_agents_manager_lib::services::dsh_control::codec::{decode_response, parse_action};
use multi_agents_manager_lib::services::dsh_control::disabled_arc;
use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError, DSH_ERROR_REGISTRY};
use multi_agents_manager_lib::services::dsh_control::model::{
    ActionPayload, ActionReceipt, ActionRequest, ActionState, ActivationState, AuthorizationState,
    AuthorizedPeer, BindingChallenge, CatalogEntry, CatalogKind, CatalogResult, ClientIdentity,
    ConnectionState, CreateReceipt, CreateRequest, DeviceContext, EffectEvidence, EffectKind,
    EffectiveOperations, EvidenceSource, InstallationState, LocalHost, ManagementJob,
    ManagementKind, ManagementPhase, ManagementStatus, ModelCatalog, ModelSelection, NativeResult,
    Operation, OperationStatus, PendingInteraction, PendingKind, PendingQuestion,
    PendingQuestionOption, QueueItem, QueueItemState, RequestKey, SelectModelPayload, SendPayload,
    SessionSnapshot, Target, UploadKind, UploadReceipt,
};

/// 测试样本构造器：请求键（原生请求身份；ID 值与 waterfall 夹具同源）。
fn probe_key() -> RequestKey {
    RequestKey {
        host_id: "local".into(),
        epoch: 1,
        session_id: "session-b2f3dd10".into(),
        client_id: "client-probe-1".into(),
        event_id: "evt-17".into(),
        agent_id: "agent-probe-1".into(),
        revision: 3,
    }
}

/// 测试样本构造器：设备上下文（服务端从已配对 cookie 身份构造；测试直接注入）。
fn probe_device() -> DeviceContext {
    DeviceContext {
        device_id: "dev-1".into(),
        device_name: "测试设备".into(),
    }
}

// 帮助函数：读取契约夹具（缺夹具 = 环境错误，不算行为失败——夹具先行落盘）。
fn fixture(rel: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/dsh-native/0.2.1-rc.3")
        .join(rel);
    std::fs::read(&path).unwrap_or_else(|e| panic!("读取契约夹具 {path:?} 失败: {e}"))
}

/// RED-1：响应身份闸——rpcId 与请求不一致的外来帧必须拒绝（DSH_RPC_ID_MISMATCH），
/// 同帧按真实 rpcId 放行且身份/结果保持。样本 = Win 报告 §3.2 步骤 6 实跑回帧。
#[test]
fn response_rejects_foreign_rpc_id() {
    let frame = fixture("windows/rpc-session-list-ok.json");
    // 期望 "a1"，帧携带 rpcId="ok" → 外来帧，必须报错（规范断言 1）。
    let foreign = decode_response(&frame, "a1");
    assert!(foreign.is_err(), "rpcId 不一致的外来帧被放行：{foreign:?}");
    assert_eq!(
        foreign.unwrap_err().code,
        codes::RPC_ID_MISMATCH,
        "拒绝原因必须是身份不一致，而不是笼统坏帧"
    );

    // 同一帧按真实 rpcId 解码：放行 + 身份原样 + typed 成功。
    let decoded = decode_response(&frame, "ok").expect("同 rpcId 的真实帧必须放行");
    assert_eq!(decoded.rpc_id, "ok");
    assert!(matches!(decoded.result, NativeResult::Success { .. }));

    // 403 unpaired 失败信封（Mac 报告 §3.3 实测形状）→ typed 失败且码可读。
    let denied = decode_response(
        fixture("macos/rpc-unpaired-403.json").as_slice(),
        "invalid-request",
    )
    .expect("unpaired 信封是合法 server-response，必须能解码");
    match denied.result {
        NativeResult::Failure { error } => assert_eq!(error.code, "unpaired"),
        other => panic!("403 信封应解码为 typed 失败，得到 {other:?}"),
    }

    // 坏帧（非 JSON）→ DSH_FRAME_INVALID，不是 rpcId 错。
    assert_eq!(
        decode_response(b"not json", "a1").unwrap_err().code,
        codes::FRAME_INVALID
    );
}

/// RED-2：动作载荷里的原生请求身份（client/event/agent/revision）以原生字符串
/// 跨解析保持，不改写、不重生成（§2.2「这些 ID 以原生字符串保存，不由题干生成」）。
/// eventId="evt-17" 与 macos/waterfall-user-questions-request.json 夹具同源。
#[test]
fn dto_preserves_native_request_identity() {
    let wire = serde_json::json!({
        "type": "approval",
        "key": {
            "hostId": "local",
            "epoch": 1,
            "sessionId": "session-b2f3dd10",
            "clientId": "client-probe-1",
            "eventId": "evt-17",
            "agentId": "agent-probe-1",
            "revision": 3
        },
        "decision": "allowed-once"
    });
    let decoded = parse_action(&wire).expect("审批动作必须可解析");
    let approval = match decoded {
        ActionPayload::Approval(approval) => approval,
        other => panic!("期望 Approval 变体，得到 {other:?}"),
    };
    // 规范断言 2：原生请求身份逐字段保持。
    assert_eq!(approval.key.event_id, "evt-17");
    assert_eq!(approval.key.client_id, "client-probe-1");
    assert_eq!(approval.key.agent_id, "agent-probe-1");
    assert_eq!(approval.key.revision, 3);
    assert_eq!(approval.key.session_id, "session-b2f3dd10");
    assert_eq!(approval.decision, "allowed-once");

    // 反向：RequestKey 序列化回 wire 后原生 ID 仍是原字符串（camelCase 键名）。
    let rebuilt = ActionPayload::Approval(probe_key_approval());
    let round: serde_json::Value = serde_json::to_value(&rebuilt).expect("动作可序列化");
    assert_eq!(round["key"]["eventId"], serde_json::json!("evt-17"));
    assert_eq!(
        round["key"]["clientId"],
        serde_json::json!("client-probe-1")
    );
    assert_eq!(round["type"], serde_json::json!("approval"));
}

/// 测试样本构造器：Approval 载荷（身份 = probe_key）。
fn probe_key_approval() -> multi_agents_manager_lib::services::dsh_control::model::ApprovalPayload {
    multi_agents_manager_lib::services::dsh_control::model::ApprovalPayload {
        key: probe_key(),
        decision: "allowed-once".into(),
    }
}

/// RED-3：动作是**闭集**——type 不在白名单的 ns/method 一律拒绝（§2.2「不接收
/// 任意 ns/method」）；已知变体照常解析（证明不是无脑全拒）；已知变体携带
/// 未知字段同样拒绝（deny_unknown_fields 纪律）。
#[test]
fn unknown_variant_is_not_action() {
    // 规范断言 3：未知操作 → 错。
    let unknown = serde_json::json!({"type": "terminal.create", "args": {}});
    let unknown_result = parse_action(&unknown);
    assert!(
        unknown_result.is_err(),
        "白名单外操作被当成动作：{unknown_result:?}"
    );
    assert_eq!(unknown_result.unwrap_err().code, codes::ACTION_UNKNOWN);

    // 已知变体照常解析。
    let send = serde_json::json!({
        "type": "send",
        "text": "hi",
        "receiptIds": [],
        "queueOnly": false,
        "bypassPausedQueue": false
    });
    assert!(matches!(
        parse_action(&send).unwrap(),
        ActionPayload::Send(_)
    ));

    // 已知变体携带未声明字段 → 拒绝（单元变体与结构变体各验一例）。
    let unit_extra = serde_json::json!({"type": "pin", "text": "extra"});
    assert!(
        parse_action(&unit_extra).is_err(),
        "单元变体不得接受额外字段"
    );
    let struct_extra = serde_json::json!({
        "type": "queue_remove",
        "itemId": "q-1",
        "evil": true
    });
    assert_eq!(
        parse_action(&struct_extra).unwrap_err().code,
        codes::ACTION_INVALID,
        "已知变体的未知字段按载荷非法拒绝"
    );

    // 已知变体缺必填字段 → 拒绝（不是 unknown，是 invalid）。
    let missing = serde_json::json!({"type": "select_model", "provider": "p"});
    assert_eq!(
        parse_action(&missing).unwrap_err().code,
        codes::ACTION_INVALID
    );

    // 非对象载荷 → unknown。
    assert_eq!(
        parse_action(&serde_json::json!(42)).unwrap_err().code,
        codes::ACTION_UNKNOWN
    );
}

/// 验收①：§2.2 全家族 DTO 可序列化，且枚举 wire 词稳定（TS 镜像逐字对应）。
#[test]
fn all_dto_families_serialize_with_stable_wire_words() {
    // Operation 全 28 词：serde 词 == as_str（单一来源），总数锁定。
    assert_eq!(Operation::ALL.len(), 28);
    for op in Operation::ALL {
        assert_eq!(
            serde_json::to_value(op).unwrap(),
            serde_json::json!(op.as_str()),
            "{op:?} 的 serde 词必须与 as_str 一致"
        );
    }
    // ActionState 9 词 + Payload 单元变体 tag 词。
    assert_eq!(
        serde_json::to_value(ActionState::UnknownDelivery).unwrap(),
        serde_json::json!("unknown_delivery")
    );
    assert_eq!(
        serde_json::to_value(ActionState::Succeeded).unwrap(),
        serde_json::json!("succeeded")
    );
    for (payload, tag) in [
        (ActionPayload::CancelCurrent, "cancel_current"),
        (ActionPayload::Archive, "archive"),
        (ActionPayload::Unpin, "unpin"),
    ] {
        assert_eq!(
            serde_json::to_value(payload).unwrap()["type"],
            serde_json::json!(tag)
        );
    }
    // 结构变体 tag + camelCase 字段。
    let send = ActionPayload::Send(SendPayload {
        text: "hi".into(),
        receipt_ids: vec![],
        queue_only: false,
        bypass_paused_queue: false,
    });
    let v = serde_json::to_value(&send).unwrap();
    assert_eq!(v["type"], serde_json::json!("send"));
    assert_eq!(v["receiptIds"], serde_json::json!([]));
    assert_eq!(v["bypassPausedQueue"], serde_json::json!(false));
    let select = ActionPayload::SelectModel(SelectModelPayload {
        provider: "opencode-go".into(),
        model: "deepseek-v4.1-flash".into(),
        reasoning_effort: Some("max".into()),
    });
    let v = serde_json::to_value(&select).unwrap();
    assert_eq!(v["type"], serde_json::json!("select_model"));
    assert_eq!(v["reasoningEffort"], serde_json::json!("max"));
    // fork.at_seq 可缺省（原生 session/fork {atSeq?}，Mac 报告 §4）。
    let fork = serde_json::to_value(ActionPayload::Fork(
        multi_agents_manager_lib::services::dsh_control::model::ForkPayload { at_seq: None },
    ))
    .unwrap();
    assert_eq!(fork["type"], serde_json::json!("fork"));
    assert!(fork["atSeq"].is_null());

    // SessionSnapshot（含 PendingInteraction/QueueItem/EffectiveOperations/ModelSelection）。
    let snapshot = SessionSnapshot {
        target: Target {
            host_id: "local".into(),
            epoch: 1,
            session_id: "s-1".into(),
        },
        revision: 7,
        connection_state: ConnectionState::Connected,
        effective_operations: EffectiveOperations {
            revision: 1,
            operations: [(
                Operation::Send,
                OperationStatus {
                    implemented: true,
                    allowed: true,
                    verified: true,
                    requires_auto_adapt: false,
                    evidence: EvidenceSource::ObservedSuccess,
                    reason_code: None,
                },
            )]
            .into_iter()
            .collect(),
        },
        messages: Vec::new(),
        pending: vec![PendingInteraction {
            key: probe_key(),
            kind: PendingKind::Questions,
            allowed_decisions: vec!["answer".into()],
            question: Some("选苹果还是香蕉？".into()),
            questions: vec![PendingQuestion {
                id: "fruit_choice".into(),
                question: "选苹果还是香蕉？".into(),
                header: Some("水果选择".into()),
                options: vec![PendingQuestionOption {
                    label: "苹果".into(),
                    description: String::new(),
                }],
                multi_select: false,
            }],
            summary: None,
            resolved: false,
        }],
        queue: vec![QueueItem {
            item_id: "q-1".into(),
            action_id: Some("act-1".into()),
            text: "排队文本".into(),
            state: QueueItemState::NativePending,
        }],
        model: Some(ModelSelection {
            provider: "deepseek-official".into(),
            model: "deepseek-flash".into(),
            reasoning_effort: Some("high".into()),
        }),
    };
    let v = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(v["target"]["hostId"], serde_json::json!("local"));
    assert_eq!(v["connectionState"], serde_json::json!("connected"));
    assert_eq!(
        v["effectiveOperations"]["operations"]["send"]["evidence"],
        serde_json::json!("observed_success")
    );
    assert_eq!(
        v["pending"][0]["key"]["eventId"],
        serde_json::json!("evt-17")
    );
    assert_eq!(
        v["pending"][0]["allowedDecisions"][0],
        serde_json::json!("answer")
    );
    assert_eq!(
        v["pending"][0]["questions"][0]["multiSelect"],
        serde_json::json!(false)
    );
    assert_eq!(v["queue"][0]["itemId"], serde_json::json!("q-1"));
    assert_eq!(v["queue"][0]["state"], serde_json::json!("native_pending"));
    assert_eq!(v["model"]["reasoningEffort"], serde_json::json!("high"));

    // ActionRequest / ActionReceipt。
    let request = ActionRequest {
        target: Target {
            host_id: "local".into(),
            epoch: 1,
            session_id: "s-1".into(),
        },
        action_id: "act-1".into(),
        expected_revision: Some(7),
        payload: ActionPayload::Archive,
    };
    let v = serde_json::to_value(&request).unwrap();
    assert_eq!(v["actionId"], serde_json::json!("act-1"));
    assert_eq!(v["expectedRevision"], serde_json::json!(7));
    assert_eq!(v["payload"]["type"], serde_json::json!("archive"));

    let receipt = ActionReceipt {
        action_id: "act-1".into(),
        state: ActionState::Succeeded,
        native_id: Some("evt-1".into()),
        reason_code: None,
    };
    assert_eq!(
        serde_json::to_value(&receipt).unwrap()["actionId"],
        serde_json::json!("act-1")
    );

    // Catalog / Create / Upload / Effect / Management 家族。
    let catalog = CatalogResult::Models {
        catalog: ModelCatalog {
            default: Some(ModelSelection {
                provider: "deepseek-official".into(),
                model: "deepseek-flash".into(),
                reasoning_effort: None,
            }),
            routable_providers: vec!["official".into(), "account".into()],
            groups: Vec::new(),
        },
    };
    let v = serde_json::to_value(&catalog).unwrap();
    assert_eq!(v["kind"], serde_json::json!("models"));
    assert_eq!(
        v["catalog"]["routableProviders"][0],
        serde_json::json!("official")
    );
    let items = CatalogResult::Skills {
        items: vec![CatalogEntry {
            id: "brainstorming".into(),
            name: "brainstorming".into(),
            description: None,
        }],
    };
    assert_eq!(
        serde_json::to_value(&items).unwrap()["kind"],
        serde_json::json!("skills")
    );

    let create = CreateReceipt {
        action_id: "act-2".into(),
        target: Target {
            host_id: "local".into(),
            epoch: 1,
            session_id: "s-2".into(),
        },
        state: ActionState::Accepted,
        agent_preset: Some("standard".into()),
    };
    assert_eq!(
        serde_json::to_value(&create).unwrap()["agentPreset"],
        serde_json::json!("standard")
    );
    let create_req = CreateRequest {
        host_id: "local".into(),
        project_id: "proj-1".into(),
        action_id: "act-2".into(),
        initial_text: None,
    };
    assert_eq!(
        serde_json::to_value(&create_req).unwrap()["projectId"],
        serde_json::json!("proj-1")
    );

    let upload = UploadReceipt {
        target: Target {
            host_id: "local".into(),
            epoch: 1,
            session_id: "s-1".into(),
        },
        receipt_id: "rcpt-1".into(),
        name: "probe-sentinel.txt".into(),
        bytes: 33,
        kind: UploadKind::File,
    };
    let v = serde_json::to_value(&upload).unwrap();
    assert_eq!(v["receiptId"], serde_json::json!("rcpt-1"));
    assert_eq!(v["kind"], serde_json::json!("file"));

    let effect = EffectEvidence {
        operation: Operation::ApproveOnce,
        native_id: Some("evt-1".into()),
        epoch: 1,
        effect_kind: EffectKind::Decision,
    };
    assert_eq!(
        serde_json::to_value(&effect).unwrap()["effectKind"],
        serde_json::json!("decision")
    );

    let status = ManagementStatus {
        installation: InstallationState::Installed,
        activation: ActivationState::RestartRequired,
        authorization: AuthorizationState::Bound,
        operation: Some(ManagementJob {
            job_id: "job-1".into(),
            kind: ManagementKind::Install,
            phase: ManagementPhase::WaitingForDesktopExit,
            reason_code: None,
        }),
        desktop_version: Some("0.2.1-rc.3".into()),
        plugin_version: None,
        reason_code: None,
    };
    let v = serde_json::to_value(&status).unwrap();
    assert_eq!(v["installation"], serde_json::json!("installed"));
    assert_eq!(v["activation"], serde_json::json!("restart_required"));
    assert_eq!(
        v["operation"]["phase"],
        serde_json::json!("waiting_for_desktop_exit")
    );

    // 绑定补充类型（本机 IPC 面；公开键/路径非秘密，可序列化）。
    let client = ClientIdentity {
        public_key_spki: vec![1, 2, 3],
        executable: std::path::PathBuf::from("/app/DeepSeek Harness.exe"),
        pid: 37096,
        started_at_ms: 1_735_000_000_000,
    };
    assert_eq!(
        serde_json::to_value(&client).unwrap()["publicKeySpki"],
        serde_json::json!([1, 2, 3])
    );
    let challenge = BindingChallenge {
        binding_id: "bind-1".into(),
        display_code: "ABCD-1234".into(),
        expires_at_ms: 1_735_000_060_000,
    };
    assert_eq!(
        serde_json::to_value(&challenge).unwrap()["displayCode"],
        serde_json::json!("ABCD-1234")
    );
}

/// 验收②：秘密句柄 Debug 全脱敏；DshError 可 Display 为 `CODE: detail`（Tauri 边界形态）。
#[test]
fn secret_handle_redacts_debug_and_error_formats_as_coded_message() {
    let secret = b"owner-cookie-VALUE-DO-NOT-PRINT".to_vec();
    let peer = AuthorizedPeer::new("local", secret.clone());
    let rendered = format!("{peer:?}");
    assert!(
        rendered.contains("AuthorizedPeer"),
        "Debug 保留类型名：{rendered}"
    );
    assert!(
        !rendered.contains("owner-cookie-VALUE"),
        "Debug 泄露了秘密载荷：{rendered}"
    );
    // 秘密只在显式取用面（secret()）可见——transport 组请求头专用，不入 DTO/日志。
    assert_eq!(peer.secret(), secret.as_slice());
    // AuthorizedPeer 刻意**不实现 Serialize**（编译期边界：无 derive、无 impl，评审可查）。

    let err = DshError::new(codes::RPC_ID_MISMATCH, "响应 rpcId 与请求不一致");
    assert_eq!(
        err.to_message(),
        "DSH_RPC_ID_MISMATCH: 响应 rpcId 与请求不一致",
        "Tauri 边界统一「CODE: detail」形态（errcode.rs 约定）"
    );
}

/// 验收④：错误码登记表 ↔ zh/en locale 双键齐备（结构性自证，门禁脚本之外的锁）。
#[test]
fn error_registry_has_zh_and_en_locale_keys() {
    let read_locale = |name: &str| -> serde_json::Value {
        serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../src/i18n/locales/")
                    .join(name),
            )
            .unwrap_or_else(|e| panic!("读 {name} 失败: {e}")),
        )
        .expect("locale 必须是合法 JSON")
    };
    let zh = read_locale("zh.json");
    let en = read_locale("en.json");
    assert!(!DSH_ERROR_REGISTRY.is_empty());
    for entry in DSH_ERROR_REGISTRY {
        let (code, description) = entry.split_once(':').expect("登记串必有码前缀");
        let key = camel_case(code);
        let zh_text = zh["errors"][&key]
            .as_str()
            .unwrap_or_else(|| panic!("zh.json 缺 errors.{key}"));
        let en_text = en["errors"][&key]
            .as_str()
            .unwrap_or_else(|| panic!("en.json 缺 errors.{key}"));
        assert!(!zh_text.trim().is_empty() && !en_text.trim().is_empty());
        // zh 文案 = 登记表说明（单源）；en 为对应英文句（键位对齐由 i18n 门禁守）。
        assert_eq!(
            zh_text,
            description.trim(),
            "zh 文案须与登记表说明一致（{code}）"
        );
    }
}

/// camelCase(码)：`DSH_RPC_ID_MISMATCH` → `dshRpcIdMismatch`（errcode.rs 门禁同款规则）。
fn camel_case(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    let mut upper_next = false;
    for ch in code.chars() {
        if ch == '_' {
            upper_next = true;
        } else if upper_next {
            out.extend(ch.to_uppercase());
            upper_next = false;
        } else {
            out.extend(ch.to_lowercase());
        }
    }
    out
}

/// §2.5 边界：**disabled** hub 的每个入口都显式 Unsupported（未启用装配不冒充
/// 已支持）；production hub 自 T09 起已接线动作账本，见
/// `production_hub_dispatches_into_the_action_ledger`（dsh_store_test）。
#[tokio::test]
async fn disabled_hub_returns_unsupported_for_every_entry() {
    for hub in [disabled_arc()] {
        let err = hub
            .dispatch(
                probe_device(),
                ActionRequest {
                    target: Target {
                        host_id: "local".into(),
                        epoch: 1,
                        session_id: "s-1".into(),
                    },
                    action_id: "act-1".into(),
                    expected_revision: None,
                    payload: ActionPayload::Archive,
                },
            )
            .await
            .expect_err("禁用 hub 必须拒绝写");
        // 未装配 → UNSUPPORTED（不冒充「已接入但写门关」）。写门判定在装配判定
        // 之后（复审 m1：顺序反了会把「未装配」误报成「恢复失败」）。
        assert_eq!(err.code, codes::UNSUPPORTED, "实际：{err:?}");

        let snapshot_err = hub
            .snapshot("s-1")
            .await
            .expect_err("占位 snapshot 必须拒绝");
        assert_eq!(snapshot_err.code, codes::UNSUPPORTED);

        let create_err = hub
            .create(
                probe_device(),
                CreateRequest {
                    host_id: "local".into(),
                    project_id: "proj-1".into(),
                    action_id: "act-2".into(),
                    initial_text: None,
                },
            )
            .await
            .expect_err("占位 create 必须拒绝");
        assert_eq!(create_err.code, codes::UNSUPPORTED);
    }
}

/// CatalogKind/CatalogResult 闭集：未知 kind 在 serde 层拒绝。
#[test]
fn catalog_result_rejects_unknown_kind() {
    let unknown: Result<CatalogResult, _> =
        serde_json::from_value(serde_json::json!({"kind": "terminal", "items": []}));
    assert!(unknown.is_err(), "未知目录 kind 被当成目录");

    let kind: Result<CatalogKind, _> = serde_json::from_value(serde_json::json!("sessions"));
    assert!(matches!(kind.unwrap(), CatalogKind::Sessions));
}

/// LocalHost 是本机 DTO（路径/PID 只在本机可见，不进远程路由——§2.2 纪律由
/// 后续路由任务遵守）；此处只锁其序列化形状与字段名（值为合成探针值）。
#[test]
fn local_host_serializes_camel_case() {
    let host = LocalHost {
        host_id: "local".into(),
        platform: "macos".into(),
        profile_dir: std::path::PathBuf::from("/Users/probe/.dsh/profiles/desktop"),
        executable: std::path::PathBuf::from("/Applications/DeepSeek Harness.app"),
        pid: 5011,
        started_at_ms: 1_735_000_000_000,
        epoch: 1,
        authority: "127.0.0.1:63026".into(),
        package_sha256: "0000000000000000000000000000000000000000000000000000000000000000".into(),
    };
    let v = serde_json::to_value(&host).unwrap();
    assert_eq!(v["hostId"], serde_json::json!("local"));
    assert_eq!(
        v["profileDir"],
        serde_json::json!("/Users/probe/.dsh/profiles/desktop")
    );
    assert_eq!(
        v["packageSha256"],
        serde_json::json!("0000000000000000000000000000000000000000000000000000000000000000")
    );
}

/// DshError 构造必须走登记表（debug 构建硬失败未登记码，UsageError 同款纪律）。
#[test]
#[should_panic(expected = "未登记进 DSH_ERROR_REGISTRY")]
fn unregistered_error_code_is_rejected_in_debug() {
    let _ = DshError::new("DSH_NOT_A_REGISTERED_CODE", "未登记码必须硬失败");
}

// ============================================================
// K7 回归（2026-10-29 修复批 M-5）：CredentialFrame 的 Debug 必须脱敏
// ============================================================
//
// 缺陷：`CredentialFrame { binding_id, cookie }` 自动 derive 了 Debug，而 `cookie`
// 是 owner Cookie 明文——任何 `{:?}` 落点（日志、错误上下文、调试打印、测试失败
// 信息）都会把长效凭据打出去。`RedeemedConnect` / `AuthorizedPeer` 已经有手工
// 脱敏纪律，凭据帧是同一条纪律的漏网字段。
//
// 契约：手工实现 Debug（不 derive），cookie 一律输出 `<redacted:len=N>`；
// `binding_id` 是非秘密元数据，可以显示（撤权/排障需要它）。

/// M-5：`{:?}` 既不含 cookie 明文、也不暴露「cookie」这个字段名所承载的值。
#[test]
fn credential_frame_debug_redacts_the_owner_cookie() {
    use multi_agents_manager_lib::services::dsh_control::auth::CredentialFrame;

    let cookie = "dsh-auth-abc123=owner-cookie-VALUE-DO-NOT-PRINT";
    let frame = CredentialFrame {
        binding_id: "binding-xyz".to_string(),
        cookie: cookie.to_string(),
        browser_cookie: None,
    };
    let rendered = format!("{frame:?}");

    assert!(
        rendered.contains("CredentialFrame"),
        "Debug 应保留类型名（可诊断）：{rendered}"
    );
    assert!(
        !rendered.contains("owner-cookie-VALUE"),
        "Debug 泄露了 owner Cookie 明文：{rendered}"
    );
    // 整段 cookie（含名字）都不得出现——名字部分同样是凭据材料（官方 cookie 名
    // 派生自 authority 摘要，属可关联标识）。
    assert!(
        !rendered.contains("dsh-auth-abc123"),
        "Debug 泄露了 cookie 的 name=value 形态：{rendered}"
    );
    assert!(
        rendered.contains(&format!("<redacted:len={}>", cookie.len())),
        "Debug 必须以长度占位替代秘密（长度是脱敏谱系的既有口径）：{rendered}"
    );
    // binding_id 是非秘密元数据（撤权/排障要它），可以显示。
    assert!(
        rendered.contains("binding-xyz"),
        "binding_id 是非秘密元数据，应可显示：{rendered}"
    );
}
