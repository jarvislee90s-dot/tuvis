//! H08 测试：解析基线、真实内容 source 行为，以及可执行的 HTTP 列表/详情契约。
//! 不连接宿主，不扫描真实 home，不修改归档夹具；磁盘输入仅写入每例独立 tempdir。
//!
//! 真实依据：rc.3 Desktop child/fork 与 rc.5 Web parent/child/fork 脱敏片段。
//! 嵌套、孤儿、缺失/未知 reason 是在真实结构上做的内存/临时文件派生边界，非实机证据。
//! 原有 15 个 log/status 解析测试只是基线，其通过不能作为 H08 行为 RED。
//!
//! content_source_behavior 直调已有 read_session_messages_with，测试读取、尾页、
//! 扩大 limit 加载更早内容、截断标记和零日志写入，不在测试中实现读取/分页逻辑。
//! source_contracts 使用独立内存 RemoteState 和真实 HTTP 业务处理器，不创建假 DSH source。
//! 列表工厂直连 monitor::subagents::dsh::collect_with_status（2026-11-18 销项批：
//! chip 需要回合终态证据），输入为隔离的 DSH 根。
//! 详情源尚未实现，保持未装配和 unsupported 的现有 RED；不自动验证生产注册变化。
//! 外父拒绝测试先要求正父可读，防止统一 unsupported 假通过；实例归属/增量预算待验。

use multi_agents_manager_lib::monitor::dsh::log::{
    is_subagent, parse_events, parse_header, DshEvent, DshHeader,
};
use multi_agents_manager_lib::monitor::dsh::status::{derive, LockState, StatusInput};
use multi_agents_manager_lib::session::SessionStatus;
use serde_json::{json, Value};

const DESKTOP_CHILD: &str =
    include_str!("fixtures/dsh-native/0.2.1-rc.3/macos/hybrid/desktop-child-sanitized.json");
const DESKTOP_FORK: &str =
    include_str!("fixtures/dsh-native/0.2.1-rc.3/macos/hybrid/desktop-fork-sanitized.json");
const WEB_PARENT: &str =
    include_str!("fixtures/dsh-native/0.2.1-rc.5/macos/hybrid/web-subagent-parent-sanitized.json");
const WEB_CHILD: &str =
    include_str!("fixtures/dsh-native/0.2.1-rc.5/macos/hybrid/web-subagent-child-sanitized.json");
const WEB_FORK: &str =
    include_str!("fixtures/dsh-native/0.2.1-rc.5/macos/hybrid/web-fork-sanitized.json");

fn fixture(raw: &str) -> Value {
    serde_json::from_str(raw).expect("已归档夹具必须是合法 JSON；坏夹具不算行为 RED")
}

fn header_value(raw: &str) -> Value {
    fixture(raw)
        .get("header")
        .expect("真实关联夹具必须保留 header")
        .clone()
}

fn decode_header(mut header: Value) -> DshHeader {
    // 归档外壳不是原生 JSONL。仅在内存中移除两个已知的时间脱敏占位符；
    // 不伪造毫秒值，不改写原生 id/parentSession/origin/delegationDepth。
    // 其他非法时间形状仍交给生产解析器拒绝，不用宽泛清洗掩盖回归。
    if matches!(
        header.get("createdAt").and_then(Value::as_str),
        Some("<redacted>" | "<epoch-ms>")
    ) {
        header.as_object_mut().unwrap().remove("createdAt");
    }
    let line = serde_json::to_string(&header).unwrap();
    parse_header(&line).expect("适配后的真实 header 必须由生产解析器读取")
}

fn decode_event(event: &Value) -> Vec<DshEvent> {
    parse_events(&serde_json::to_string(event).unwrap())
}

fn child_events_without_termination(child: &Value) -> Vec<DshEvent> {
    // 只投影已保留的原始事件；不把 observedEventTypes、jobStatusShape、
    // 取证说明或正文中的状态词伪装成真实状态事件。
    let mut events = Vec::new();
    for key in ["descriptor", "seededTask"] {
        events.extend(decode_event(&child[key]));
    }
    for event in child["toolCalls"].as_array().expect("子夹具工具事件数组") {
        events.extend(decode_event(event));
    }
    events
}

fn outcome(events: &[DshEvent]) -> multi_agents_manager_lib::monitor::dsh::status::StatusOutcome {
    derive(&StatusInput {
        events,
        lock: LockState::Unknown,
        silence_ms: None,
    })
}

// 防回归：若将“有 parentSession”当作子 Agent 充分条件，两个真实 fork 都会被误收。
#[test]
fn fork_is_not_a_subagent() {
    for (host, raw) in [("Desktop rc.3", DESKTOP_FORK), ("Web rc.5", WEB_FORK)] {
        let header = decode_header(header_value(raw));
        assert_eq!(
            header.parent_session.as_deref(),
            Some("session-<parent-id>")
        );
        assert!(
            !is_subagent(&header),
            "{host} 的真实 fork 不得进入子任务候选"
        );
    }
}

// 防回归：不能为排除 fork 而连真实子 Agent 一起过滤，或要求 child id 带 session-。
#[test]
fn real_subagent_is_recognized() {
    for (host, raw) in [("Desktop rc.3", DESKTOP_CHILD), ("Web rc.5", WEB_CHILD)] {
        let header = decode_header(header_value(raw));
        assert!(is_subagent(&header), "{host} 的真实子任务必须识别");
    }
}

#[test]
fn root_session_is_not_a_subagent() {
    let parent = decode_header(header_value(WEB_PARENT));
    assert!(!is_subagent(&parent), "真实主会话不得误识别为自己的子任务");
}

#[test]
fn bare_child_id_is_preserved() {
    let child = decode_header(header_value(WEB_CHILD));
    assert_eq!(
        child.id, "<child-id>",
        "不能自行给原生裸 UUID 补 session- 前缀"
    );
}

// 真实父子正例：期望取自跨文件关联表，不能用另一个转换器生成 expected。
#[test]
fn real_child_keeps_its_parent() {
    let child = decode_header(header_value(WEB_CHILD));
    assert_eq!(child.parent_session.as_deref(), Some("session-<parent-id>"));
}

#[test]
fn parent_catalog_preserves_the_bare_child_reference() {
    let parent = fixture(WEB_PARENT);
    let events = decode_event(&parent["delegationChain"]["subagentCatalog"]);
    assert_eq!(events.len(), 1, "真实 catalog 事件不能被解析器丢弃");
    assert_eq!(events[0].kind, "subagent/catalog");
    assert_eq!(events[0].data["childId"], json!("<child-id>"));
}

// 合成边界：只更改真实 child 结构的 id、parentSession 和 delegationDepth；
// 抓将孙节点父 ID 改写成根 ID、丢弃裸父 ID 的解析回归，不声称树查询已经实现。
#[test]
fn nested_children_keep_their_parent() {
    let child = decode_header(header_value(WEB_CHILD));
    let mut nested = header_value(WEB_CHILD);
    nested["id"] = json!("<grandchild-id>");
    nested["parentSession"] = json!("<child-id>");
    nested["delegationDepth"] = json!(2);
    let grandchild = decode_header(nested);

    assert_eq!(child.parent_session.as_deref(), Some("session-<parent-id>"));
    assert_eq!(grandchild.parent_session.as_deref(), Some("<child-id>"));
}

#[test]
fn nested_child_remains_a_subagent() {
    let mut nested = header_value(WEB_CHILD);
    nested["id"] = json!("<grandchild-id>");
    nested["parentSession"] = json!("<child-id>");
    nested["delegationDepth"] = json!(2);
    assert!(is_subagent(&decode_header(nested)));
}

// 只锁解析层“不捏造父 ID”；孤儿是否从最终树排除仍须 H08 source 层测试。
#[test]
fn orphan_header_does_not_invent_a_parent() {
    let mut orphan = header_value(WEB_CHILD);
    orphan.as_object_mut().unwrap().remove("parentSession");
    assert!(decode_header(orphan).parent_session.is_none());
}

// 真实 Desktop 片段只保留事件名称而没有结束载荷，不能据取证文字判 completed。
#[test]
fn missing_status_is_not_completed() {
    let header = header_value(DESKTOP_CHILD);
    let events = decode_event(&header);
    assert_eq!(events.len(), 1, "输入必须保留真实 session 事件");
    let out = outcome(&events);
    assert_eq!(out.status, SessionStatus::Idle, "无状态证据不能假完成");
    assert!(out.end_kind.is_none());
}

// 真实子侧已成功回报父会话，但缺少 turn/end 时，工具成功不能代替回合结束。
#[test]
fn successful_report_without_turn_end_is_not_completed() {
    let child = fixture(WEB_CHILD);
    let events = child_events_without_termination(&child);
    assert!(
        events.iter().any(|event| {
            event.kind == "tool/result"
                && event
                    .data
                    .pointer("/message/toolCallId")
                    .and_then(Value::as_str)
                    == Some("<callId-send>")
        }),
        "输入必须保留真实 send_message 成功回执，避免空输入假通过"
    );
    let out = outcome(&events);
    assert_eq!(out.status, SessionStatus::Idle);
    assert!(out.end_kind.is_none());
}

// 合成边界：只删真实 turn/end 的 reason；结束事件本身不构成 completed 证据。
#[test]
fn missing_end_reason_is_not_completed() {
    let mut child = fixture(WEB_CHILD);
    child["termination"]["data"]
        .as_object_mut()
        .unwrap()
        .remove("reason");
    let mut events = child_events_without_termination(&child);
    events.extend(decode_event(&child["termination"]));
    assert_eq!(outcome(&events).status, SessionStatus::Idle);
}

// 合成边界：未知原生 reason 必须保留诊断值，不能走“其余均成功”的默认分支。
#[test]
fn unknown_end_reason_is_not_completed() {
    let mut child = fixture(WEB_CHILD);
    child["termination"]["data"]["reason"]["kind"] = json!("future-unrecognized-status");
    let mut events = child_events_without_termination(&child);
    events.extend(decode_event(&child["termination"]));
    let out = outcome(&events);
    assert_eq!(out.status, SessionStatus::Idle);
    assert_eq!(out.end_kind.as_deref(), Some("future-unrecognized-status"));
}

// 同一真实终止帧的正例，防止解析器把所有 reason 一概丢弃而让未知反例假绿。
// 此处只证明回合 completed 被读取，不断言 continuable 子任务永久完成。
#[test]
fn known_turn_end_retains_its_reason() {
    let child = fixture(WEB_CHILD);
    let mut events = child_events_without_termination(&child);
    events.extend(decode_event(&child["termination"]));
    let out = outcome(&events);
    assert_eq!(out.end_kind.as_deref(), Some("completed"));
    assert_eq!(out.last_end_seq, Some(38));
}

// 合成边界：缺少事件水位时，不能把不可归因的结束帧当作已完成事实。
#[test]
fn unsequenced_turn_end_is_not_completed() {
    let mut child = fixture(WEB_CHILD);
    child["termination"].as_object_mut().unwrap().remove("seq");
    let mut events = child_events_without_termination(&child);
    events.extend(decode_event(&child["termination"]));
    let out = outcome(&events);
    assert_eq!(out.status, SessionStatus::Idle);
    assert!(out.last_end_seq.is_none());
}

/// source 契约的输入准备：只投影真实事件，不做关系筛选、状态推导或分页。
/// 提供完整临时 home，供后续 DSH source 工厂消费；不经过生产全局 STATE。
pub mod source_contracts {
    use super::*;
    use app::remote::server::{CreateTaskHub, RemoteState, SseRegistry};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use http_body_util::BodyExt;
    use multi_agents_manager_lib as app;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    pub const PARENT_ID: &str = "session-<parent-id>";
    pub const CHILD_ID: &str = "<child-id>";
    pub const GRANDCHILD_ID: &str = "<grandchild-id>";
    pub const UNKNOWN_CHILD_ID: &str = "<unknown-child-id>";

    pub struct FixtureHome {
        dir: tempfile::TempDir,
    }

    impl Default for FixtureHome {
        fn default() -> Self {
            Self::new()
        }
    }

    impl FixtureHome {
        pub fn new() -> Self {
            let home = Self {
                dir: tempfile::tempdir().unwrap(),
            };
            let parent = fixture(WEB_PARENT);
            let chain = &parent["delegationChain"];
            let parent_events = [
                "toolCall",
                "subagentCatalog",
                "toolResult",
                "childReportArrival",
                "childReportUserMessage",
            ]
            .map(|key| chain[key].clone());
            home.write_log("parent", parent["header"].clone(), &parent_events);

            let child = fixture(WEB_CHILD);
            let mut child_events = vec![child["descriptor"].clone(), child["seededTask"].clone()];
            child_events.extend(child["toolCalls"].as_array().unwrap().iter().cloned());
            child_events.push(child["termination"].clone());
            home.write_log("child", child["header"].clone(), &child_events);
            home.write_log("fork", header_value(WEB_FORK), &[]);

            // 以下都是合成边界：真实结构上只改身份/父 ID/深度，不冒充实机嵌套。
            let mut nested = child["header"].clone();
            nested["id"] = json!(GRANDCHILD_ID);
            nested["parentSession"] = json!(CHILD_ID);
            nested["delegationDepth"] = json!(2);
            home.write_log("nested", nested, &[]);

            // Desktop 片段没有原始结束载荷；不根据 observedEventTypes 猜状态。
            let mut unknown = header_value(DESKTOP_CHILD);
            unknown["id"] = json!(UNKNOWN_CHILD_ID);
            home.write_log("unknown", unknown, &[]);

            let mut orphan = child["header"].clone();
            orphan["id"] = json!("<orphan-id>");
            orphan["parentSession"] = json!("session-<missing-parent-id>");
            home.write_log("orphan", orphan, &[]);

            // 外父真实存在于同一个临时 home；拒绝不能靠“父会话不存在”假通过。
            let mut foreign = parent["header"].clone();
            foreign["id"] = json!("session-<unrelated-parent-id>");
            home.write_log("foreign-parent", foreign, &[]);
            home
        }

        pub fn path(&self) -> &Path {
            self.dir.path()
        }

        fn write_log(&self, directory: &str, mut header: Value, events: &[Value]) {
            if matches!(
                header.get("createdAt").and_then(Value::as_str),
                Some("<redacted>" | "<epoch-ms>")
            ) {
                header.as_object_mut().unwrap().remove("createdAt");
            }
            let directory = self
                .path()
                .join(".dsh/sessions/h08-fixture")
                .join(directory);
            std::fs::create_dir_all(&directory).unwrap();
            let mut text = serde_json::to_string(&header).unwrap();
            text.push('\n');
            for event in events {
                text.push_str(&serde_json::to_string(event).unwrap());
                text.push('\n');
            }
            std::fs::write(directory.join("session.v4.jsonl"), text).unwrap();
        }

        pub fn snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
            fn visit(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
                for entry in std::fs::read_dir(dir).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        visit(root, &path, files);
                    } else {
                        files.insert(
                            path.strip_prefix(root).unwrap().to_path_buf(),
                            std::fs::read(path).unwrap(),
                        );
                    }
                }
            }
            let mut files = BTreeMap::new();
            visit(self.path(), self.path(), &mut files);
            files
        }
    }

    struct NoInjector;

    impl app::inject::engine::Injector for NoInjector {
        fn locate_and_inject(&self, _: u32, _: &str) -> Result<(), String> {
            panic!("H08 只读测试不得注入终端")
        }
        fn locate_and_send_key_inner(&self, _: u32, _: &str) -> Result<(), String> {
            panic!("H08 只读测试不得发送按键")
        }
    }

    fn empty_sessions() -> app::session::SessionsResponse {
        app::session::SessionsResponse {
            sessions: vec![],
            total_count: 0,
            waiting_count: 0,
        }
    }

    fn fixture_state(home: &FixtureHome) -> Arc<RemoteState> {
        let store = app::remote::pairing::DeviceStore::memory();
        let content_home = home.path().to_path_buf();
        let list_home = home.path().join(".dsh");
        let detail_home = home.path().to_path_buf();
        let display_home = home.path().to_string_lossy().to_string();
        Arc::new(RemoteState {
            opencode_native: std::sync::Arc::new(
                multi_agents_manager_lib::services::opencode_native::NativeHub::new(
                    multi_agents_manager_lib::remote::pairing::DeviceStore::memory(),
                ),
            ),
            session_source: Box::new(empty_sessions),
            fresh_session_source: Box::new(empty_sessions),
            pairing_counter: Box::new(Vec::new),
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: Arc::new(|_| panic!("不得隐藏会话")),
            board_hidden_unhide: Arc::new(|_| panic!("不得恢复会话")),
            unread_mark_read: Arc::new(|_, _, _| panic!("不得修改未读状态")),
            session_close: Arc::new(|_| panic!("不得关闭进程")),
            codex_control: Arc::new(
                app::remote::codex_ipc::CodexControlHub::disabled_with_store(store.clone()),
            ),
            store,
            host_source: Box::new(|| json!({})),
            message_source: Box::new(move |agent, sid, limit| {
                app::remote::content::read_session_messages_with(&content_home, agent, sid, limit)
            }),
            path_source: Box::new(|_, _, _| panic!("不读取文件路径源")),
            watcher_tx: tokio::sync::broadcast::channel(8).0,
            sse_registry: Arc::new(SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            pin_limiter: Mutex::new(app::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: Mutex::new(app::remote::pin::PinRateLimiter::global()),
            pin_source: Box::new(|| None),
            now_source: Box::new(|| 0),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            injector: Arc::new(NoInjector),
            resume_spawner: Arc::new(|_| panic!("不得启动终端")),
            archive_source: Box::new(Vec::new),
            archive_delete: Arc::new(|_| panic!("不得删除归档")),
            confirm_probe: Arc::new(|_, _, _| panic!("不得读取真实终端")),
            dialog_probe: Arc::new(|_, _| panic!("不得读取真实对话框")),
            screen_probe: Arc::new(|_, _| panic!("不得读取真实屏幕")),
            capability_table: app::inject::capability::new_table(),
            create_hub: Arc::new(CreateTaskHub::stub()),
            home_source: Box::new(move || Some(display_home.clone())),
            ui_config_source: Box::new(|| None),
            // 列表直连真实 DSH source；传入隔离的 DSH 根，不复制关系或状态算法。
            subagent_source: [(
                "dsh",
                Box::new(move |_: &str, parent: &str, _i: Option<&str>| {
                    app::monitor::subagents::dsh::collect_with_status(&list_home, parent)
                }) as Box<app::remote::server::SubagentSourceFn>,
            )]
            .into_iter()
            .collect(),
            // H08 详情直连真实读取器；传入同一隔离 home，不复制父子核验逻辑。
            // skip 游标（2026-11-18 销项批）随缝签名透传到真实读取面。
            subagent_message_source: [(
                "dsh",
                Box::new(
                    move |parent: &str,
                          child: &str,
                          limit: usize,
                          skip: usize,
                          _i: Option<&str>| {
                        app::remote::content::read_dsh_subagent_messages_with(
                            &detail_home,
                            Some(detail_home.join(".dsh").to_string_lossy().as_ref()),
                            parent,
                            child,
                            limit,
                            skip,
                        )
                    },
                ) as Box<app::remote::server::SubagentMessageSourceFn>,
            )]
            .into_iter()
            .collect(),
            dsh_control: app::services::dsh_control::disabled_arc(),
            dsh_console_mount: None,
            dsh_console_upstream_source: None,
            dsh_auto_adapt_source: Arc::new(|| false),
            desktop_language: Default::default(),
        })
    }

    struct HttpFixture {
        home: FixtureHome,
        router: Router,
    }

    impl HttpFixture {
        fn new() -> Self {
            let home = FixtureHome::new();
            // 进程内 HTTP：只挂真实业务处理器，不启动端口、全局 STATE、gate 或扫描器。
            // 此处验证 source 缺失与业务行为，不承担生产注册一致性或配对鉴权验收。
            let router = Router::new()
                .route(
                    "/m/api/v1/session-subagents",
                    get(app::remote::api::session_subagents),
                )
                .route(
                    "/m/api/v1/session-subagent-messages",
                    get(app::remote::api::session_subagent_messages),
                )
                .with_state(fixture_state(&home));
            Self { home, router }
        }

        async fn get(&self, path: &str, params: &[(&str, &str)]) -> (StatusCode, Value) {
            // Url 仅用于编码 query；不创建网络客户端或发起网络请求。
            let mut url = reqwest::Url::parse("http://h08.invalid").unwrap();
            url.set_path(path);
            url.query_pairs_mut().extend_pairs(params.iter().copied());
            let uri = format!("{}?{}", url.path(), url.query().unwrap());
            let response = self
                .router
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let body = if bytes.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&bytes).unwrap()
            };
            (status, body)
        }

        async fn children(&self, parent: &str) -> Vec<Value> {
            let (status, body) = self
                .get(
                    "/m/api/v1/session-subagents",
                    &[("agent_type", "dsh"), ("session_id", parent)],
                )
                .await;
            assert_eq!(status, StatusCode::OK);
            body["subagents"]
                .as_array()
                .expect("真实列表处理器必须返回 subagents 数组")
                .clone()
        }

        async fn detail(&self, parent: &str, limit: &str) -> (StatusCode, Value) {
            self.detail_of(parent, CHILD_ID, limit, None).await
        }

        /// 通用详情请求：subagent_id 与 `before` 游标（A06 销项批：子详情续页
        /// 接通后端点必须把游标透传到子 Agent 自己的读取面）。
        async fn detail_of(
            &self,
            parent: &str,
            child: &str,
            limit: &str,
            before: Option<&str>,
        ) -> (StatusCode, Value) {
            let mut params = vec![
                ("agent_type", "dsh"),
                ("session_id", parent),
                ("subagent_id", child),
                ("limit", limit),
            ];
            if let Some(cursor) = before {
                params.push(("before", cursor));
            }
            self.get("/m/api/v1/session-subagent-messages", &params)
                .await
        }

        async fn supported_detail(&self, limit: &str) -> Value {
            let (status, body) = self.detail(PARENT_ID, limit).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["supported"], json!(true), "H08_DSH_SOURCE_MISSING: 真实子日志已在临时 home，详情处理器却未装配 DSH source；响应={body}");
            body
        }
    }

    #[tokio::test]
    async fn dsh_child_details_have_a_source() {
        let http = HttpFixture::new();
        // 正控制：同一临时日志可由真实通用 reader 读出 5 条，避免夹具/路径错误假 RED。
        let visible =
            app::remote::content::read_session_messages_with(http.home.path(), "dsh", CHILD_ID, 20)
                .unwrap();
        assert_eq!(visible.messages.len(), 5);
        http.supported_detail("2").await;
    }

    #[tokio::test]
    async fn fork_is_excluded_from_dsh_list() {
        let http = HttpFixture::new();
        let rows = http.children(PARENT_ID).await;
        assert!(
            rows.iter().any(|row| row["id"] == CHILD_ID),
            "H08_DSH_SOURCE_MISSING: 空列表不能证明 fork 被排除；响应={rows:?}"
        );
        assert!(!rows.iter().any(|row| row["id"] == "session-<fork-id>"));
    }

    #[tokio::test]
    async fn child_belongs_to_its_direct_parent() {
        let http = HttpFixture::new();
        let rows = http.children(PARENT_ID).await;
        assert!(
            rows.iter().any(|row| row["id"] == CHILD_ID),
            "H08_DSH_SOURCE_MISSING: 正父必须能查到真实子任务"
        );
        assert!(!http
            .children("session-<unrelated-parent-id>")
            .await
            .iter()
            .any(|row| row["id"] == CHILD_ID));
    }

    #[tokio::test]
    async fn nested_children_keep_their_parent() {
        let http = HttpFixture::new();
        let rows = http.children(PARENT_ID).await;
        assert!(
            rows.iter().any(|row| row["id"] == CHILD_ID),
            "H08_DSH_SOURCE_MISSING: 根父必须先能查到直属子任务"
        );
        assert!(!rows.iter().any(|row| row["id"] == GRANDCHILD_ID));
        let nested = http.children(CHILD_ID).await;
        let ids: Vec<_> = nested
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, [GRANDCHILD_ID], "孙任务应保留裸 child ID 为直属父");
    }

    #[tokio::test]
    async fn orphan_is_not_guessed_into_root() {
        let http = HttpFixture::new();
        let rows = http.children(PARENT_ID).await;
        assert!(
            rows.iter().any(|row| row["id"] == CHILD_ID),
            "空列表不能证明孤儿被正确排除"
        );
        assert!(!rows.iter().any(|row| row["id"] == "<orphan-id>"));
    }

    /// A06 补证（2026-11-15 真实 home 取证）：402 个真实 Desktop 子会话中 400 个
    /// 在 header 下一行携带 `subagent/descriptor`（data.label = 人类可读任务名，
    /// provider 恒 "spawn"、mode 恒 "continuable"）。chip 数据面的 name 应取该
    /// label；id/父子关联面保持裸 child id 不变。
    #[tokio::test]
    async fn child_chip_name_comes_from_descriptor_label() {
        let http = HttpFixture::new();
        let rows = http.children(PARENT_ID).await;
        let row = rows
            .iter()
            .find(|row| row["id"] == CHILD_ID)
            .expect("正父必须能查到真实子任务");
        assert_eq!(
            row["name"],
            json!("<task-label>"),
            "chip name 应取子日志 descriptor.label（真实形态 400/402 在案）: {row}"
        );
        assert_eq!(
            row["id"],
            json!(CHILD_ID),
            "id 不变：父子关联面仍用裸 child id"
        );
    }

    /// A06 补证：真实「活跃→结束」转写形态（turn/start 无 turn/end → 追加
    /// turn/end completed）跨两次 collect 的行为锁定——按 2026-11-06 登记裁决，
    /// DSH chip 无可信存活证明不发明 running：开回合保持 unknown + endTs 空；
    /// 2026-11-18 销项批取证升级（本机 Desktop home 435/435 子代理日志携带
    /// turn/end 终态帧——闭回合是宿主写下的确定事实）：追加 turn/end 后改为
    /// idle + endTs=事件时间（终态锚，不是「永久完成」——continuable 子任务
    /// 仍在全量名单，只是退出活跃面）；descriptor label 与 id 全程稳定。
    #[tokio::test]
    async fn child_transition_active_to_finished_never_invents_a_state() {
        use multi_agents_manager_lib as app;
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join(".dsh");
        let parent = fixture(WEB_PARENT);
        home_write_log(
            &root,
            "parent",
            &parent["header"].clone(),
            &[parent["delegationChain"]["toolCall"].clone()],
        );
        let child = fixture(WEB_CHILD);
        // 活跃态：descriptor + 种子任务 + 开回合（turn/start 无 turn/end）——
        // 事件词表与主面板 running 黄金样本（sample4/sample8）同一套。
        let mut active_events = vec![
            child["descriptor"].clone(),
            child["seededTask"].clone(),
            json!({"type":"turn/start","seq":24,"time":1793799630000u64,"data":{"turn":1}}),
        ];
        home_write_log(&root, "child", &child["header"].clone(), &active_events);
        let rows = app::monitor::subagents::dsh::collect_with_status(&root, PARENT_ID);
        let row = rows
            .iter()
            .find(|r| r.id == CHILD_ID)
            .expect("活跃子任务在板");
        assert_eq!(
            row.name, "<task-label>",
            "活跃态 chip name 已取 descriptor.label"
        );
        assert_eq!(
            row.status,
            app::monitor::subagents::SubagentStatus::Unknown,
            "开回合无存活证明 → 不发明 running（2026-11-06 登记裁决维持）"
        );
        assert!(row.end_ts.is_none(), "运行/未知态不得伪造终态锚");
        // 结束态：追加 turn/end completed（真实 reason.kind，主面板同词表）→
        // 闭回合可证终态：idle + endTs = turn/end 事件时间（2026-11-18 取证）。
        active_events.push(
            json!({"type":"turn/end","seq":30,"time":1793799700000u64,"data":{"turn":1,"reason":{"kind":"completed"}}}),
        );
        home_write_log(&root, "child", &child["header"].clone(), &active_events);
        let rows = app::monitor::subagents::dsh::collect_with_status(&root, PARENT_ID);
        let row = rows
            .iter()
            .find(|r| r.id == CHILD_ID)
            .expect("结束子任务仍在板（全量名单不隐没）");
        assert_eq!(
            row.status,
            app::monitor::subagents::SubagentStatus::Idle,
            "闭回合 = 可证终态（435/435 真实日志取证），不假报也不滞留 Unknown"
        );
        assert_eq!(
            row.end_ts
                .as_deref()
                .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                .map(|t| t.timestamp_millis()),
            Some(1793799700000),
            "endTs = turn/end 事件时间（终态锚，前端冻结时长用）"
        );
        assert_eq!(row.name, "<task-label>", "label 跨转写增长保持稳定");
        assert_eq!(row.id, CHILD_ID, "id 全程稳定");
    }

    /// 临时 home 写日志（与 FixtureHome::write_log 同结构：header + 事件行，
    /// 未压缩 v4；供转写增长用例重写同一文件）。
    fn home_write_log(root: &Path, directory: &str, header: &Value, events: &[Value]) {
        let mut header = header.clone();
        if matches!(
            header.get("createdAt").and_then(Value::as_str),
            Some("<redacted>" | "<epoch-ms>")
        ) {
            header.as_object_mut().unwrap().remove("createdAt");
        }
        let dir = root.join("sessions/h08-transition").join(directory);
        std::fs::create_dir_all(&dir).unwrap();
        let mut text = serde_json::to_string(&header).unwrap();
        text.push('\n');
        for event in events {
            text.push_str(&serde_json::to_string(event).unwrap());
            text.push('\n');
        }
        std::fs::write(dir.join("session.v4.jsonl"), text).unwrap();
    }

    async fn assert_neutral_unknown(http: &HttpFixture, id: &str) {
        let rows = http.children(PARENT_ID).await;
        let row = rows
            .iter()
            .find(|row| row["id"] == id)
            .expect("H08_DSH_SOURCE_MISSING: 未知状态子任务不能消失");
        assert!(
            row["status"] == "unknown" || row["statusKnown"] == false,
            "未知状态须中性申报: {row}"
        );
        assert!(row["endTs"].is_null(), "未知状态不能伪造永久终止时刻");
    }

    #[tokio::test]
    async fn missing_status_stays_unknown() {
        assert_neutral_unknown(&HttpFixture::new(), UNKNOWN_CHILD_ID).await;
    }

    /// 2026-11-18 销项批（取证升级）：真实转写携带 turn/end 终态帧（435/435
    /// 真实子代理日志）→ 闭回合 = 可证终态：idle + endTs=事件时间。语义是
    /// 「最后已知回合已结束」（退出活跃面、时长冻结），**不是**「永久完成」——
    /// continuable 子任务仍在全量名单（2026-11-06「不发明永久完成」的关注点
    /// 由「终态 ≠ 名单移除」继续保证，本用例同时锁定两点）。
    #[tokio::test]
    async fn closed_turn_reports_terminal_idle_with_anchor() {
        let http = HttpFixture::new();
        let rows = http.children(PARENT_ID).await;
        let row = rows
            .iter()
            .find(|row| row["id"] == CHILD_ID)
            .expect("H08_DSH_SOURCE_MISSING: 真实子任务必须在板");
        assert_eq!(
            row["status"], "idle",
            "WEB_CHILD 真实夹具带 turn/end completed → idle（终态可证）: {row}"
        );
        let end_ts = row["endTs"]
            .as_str()
            .expect("终态必须携带 endTs 锚（前端冻结时长）");
        let ms = chrono::DateTime::parse_from_rfc3339(end_ts)
            .expect("endTs 必须是 ISO8601")
            .timestamp_millis();
        assert_eq!(
            ms, 1793799713415,
            "endTs = 真实夹具 turn/end 事件时间（seq 38）"
        );
        // 终态 ≠ 名单移除：全量名单照常在板（continuable 不被「完成」隐没）。
        assert!(rows.iter().any(|r| r["id"] == UNKNOWN_CHILD_ID));
    }

    #[tokio::test]
    async fn foreign_parent_cannot_read_child() {
        let http = HttpFixture::new();
        // 正控制必须先通过；统一 unsupported 不能充当“外父拒绝”。
        let own = http.supported_detail("20").await;
        assert_eq!(own["messages"].as_array().unwrap().len(), 5);
        let (status, body) = http.detail("session-<unrelated-parent-id>", "20").await;
        assert!(
            matches!(status, StatusCode::NOT_FOUND | StatusCode::FORBIDDEN),
            "外父读取必须拒绝，不能返回成功/unsupported: {status} {body}"
        );
    }

    #[tokio::test]
    async fn child_detail_supports_expanding_the_page() {
        let http = HttpFixture::new();
        let recent = http.supported_detail("2").await;
        let expanded = http.supported_detail("20").await;
        assert_eq!(recent["messages"].as_array().unwrap().len(), 2);
        assert_eq!(expanded["messages"].as_array().unwrap().len(), 5);
        assert_eq!(expanded["messages"][0]["role"], "user");
        assert_eq!(recent["messages"][0]["toolName"], "send_message");
    }

    #[tokio::test]
    async fn child_detail_reports_earlier_messages() {
        let http = HttpFixture::new();
        let recent = http.supported_detail("2").await;
        assert_eq!(recent["messages"].as_array().unwrap().len(), 2);
        assert_eq!(recent["truncated"], true);
        let expanded = http.supported_detail("20").await;
        assert_eq!(expanded["truncated"], false);
    }

    /// A06 销项（2026-11-18）：子详情续页接通。真实 Desktop home 里根会话映射
    /// 消息最高 12877 条、子任务映射最高 951 条（尚未触发旧 1000 上限），但
    /// 上限只是时间问题——端点必须把 `before` 游标透传到**子 Agent 自己的
    /// 读取面**（与根会话读取面同一 0958dc93 口径：游标 = 起始索引语义，
    /// olderBefore 键仅真实存在续页游标时在场）。本用例走 HTTP + 真实读取链
    /// （同 fixture_state 直连 read_dsh_subagent_messages_with），1001 条消息
    /// 夹具：首页吃满 1000、次页按游标取余，倒页序拼接必须还原完整文件序
    /// （无重复、无跳号）。旧代码无游标 → `olderBefore` 键缺席即 RED。
    #[tokio::test]
    async fn child_detail_pages_beyond_thousand_messages_without_gap_or_dup() {
        let http = HttpFixture::new();
        write_big_child_log(http.home.path(), 1001);
        const BIG_CHILD: &str = "<big-child-id>";
        // 首页：无 before = 尾窗首页，limit 请求 1000（clamp 上限）吃满
        let (status, first) = http.detail_of(PARENT_ID, BIG_CHILD, "1000", None).await;
        assert_eq!(status, StatusCode::OK, "首页必须可读: {first}");
        assert_eq!(first["supported"], json!(true));
        let msgs = first["messages"].as_array().expect("messages 数组");
        assert_eq!(msgs.len(), 1000, "首页必须吃满单页预算 1000");
        assert_eq!(
            first["truncated"],
            json!(true),
            "1001 条取尾 1000 必须报截断"
        );
        let cursor = first["olderBefore"]
            .as_i64()
            .expect("truncated 首页必须携带 olderBefore 游标（旧代码键缺席 = RED）");
        assert_eq!(
            cursor, 1,
            "游标 = 本页最旧消息的起始索引（1001 条取尾 1000 → 起始索引 1）"
        );
        // 次页：按游标取余（最旧 1 条），truncated=false 且键缺席（到头即停）
        let (status, second) = http
            .detail_of(PARENT_ID, BIG_CHILD, "1000", Some("1"))
            .await;
        assert_eq!(status, StatusCode::OK, "续页必须可读: {second}");
        let rest = second["messages"].as_array().expect("messages 数组");
        assert_eq!(rest.len(), 1, "游标 1 之前的余量恰 1 条");
        assert_eq!(second["truncated"], json!(false));
        assert!(
            second.get("olderBefore").is_none(),
            "最旧页不得出现 olderBefore 键（null 也算在场）: {second}"
        );
        // 倒页序拼接还原完整文件序：无重复、无跳号
        let mut all: Vec<String> = vec![rest[0]["content"].as_str().unwrap().to_string()];
        all.extend(
            msgs.iter()
                .map(|m| m["content"].as_str().unwrap().to_string()),
        );
        let expect: Vec<String> = (0..1001).map(|i| format!("big-{i}")).collect();
        assert_eq!(all, expect, "拼接必须与写入序逐条一致（无重复无跳号）");
    }

    /// 子会话归属不符 → 拒绝：孙任务的直属父是子任务，不是根父——游标批
    /// 改动不得削弱既有父子核验（read_verified_content 的 parent_session ==
    /// parent_id 门）。正控制：同一孙任务在直属父下可读（嵌套链合法）。
    #[tokio::test]
    async fn grandchild_is_rejected_under_root_parent() {
        let http = HttpFixture::new();
        // 正控制：孙任务直属父 = CHILD_ID，可读
        let (status, body) = http.detail_of(CHILD_ID, GRANDCHILD_ID, "20", None).await;
        assert_eq!(status, StatusCode::OK, "直属父读孙任务必须放行: {body}");
        assert_eq!(body["supported"], json!(true));
        // 反例：根父读孙任务 → 拒绝（不能成功、不能 unsupported 空态假通过）
        let (status, body) = http.detail_of(PARENT_ID, GRANDCHILD_ID, "20", None).await;
        assert!(
            matches!(status, StatusCode::NOT_FOUND | StatusCode::FORBIDDEN),
            "子会话归属不符必须拒绝: {status} {body}"
        );
    }

    /// 游标与归属核验的交叉面：携带 `before` 的请求同样走完整父子核验——
    /// 外父 + 游标的组合不得因续页路径而放行（核验在游标消费之前）。
    #[tokio::test]
    async fn cursor_requests_still_require_parent_ownership() {
        let http = HttpFixture::new();
        let (status, body) = http
            .detail_of("session-<unrelated-parent-id>", CHILD_ID, "2", Some("1"))
            .await;
        assert!(
            matches!(status, StatusCode::NOT_FOUND | StatusCode::FORBIDDEN),
            "外父带游标读取必须拒绝: {status} {body}"
        );
        // 正控制：正父带同一游标可读（排除「游标参数本身导致拒绝」的假通过）
        let (status, body) = http.detail_of(PARENT_ID, CHILD_ID, "2", Some("1")).await;
        assert_eq!(status, StatusCode::OK, "正父带游标必须照常可读: {body}");
    }

    /// 大消息量子日志写入（HTTP 级 >1000 续页夹具）：header 用真实 WEB_CHILD
    /// 形态（只换 id），正文 = n 条真人 user/message（source.kind=user → 全部
    /// 映射为可见消息），写入未压缩 v4（与 FixtureHome::write_log 同结构）。
    fn write_big_child_log(home: &Path, n: usize) {
        let mut header = header_value(WEB_CHILD);
        header["id"] = json!("<big-child-id>");
        if matches!(
            header.get("createdAt").and_then(Value::as_str),
            Some("<redacted>" | "<epoch-ms>")
        ) {
            header.as_object_mut().unwrap().remove("createdAt");
        }
        let dir = home.join(".dsh/sessions/h08-fixture").join("big-child");
        std::fs::create_dir_all(&dir).unwrap();
        let mut text = serde_json::to_string(&header).unwrap();
        text.push('\n');
        for i in 0..n {
            text.push_str(
                &serde_json::to_string(&json!({
                    "type": "user/message",
                    "seq": i,
                    "time": 1793799630000u64 + i as u64,
                    "data": {
                        "content": [{"type": "text", "text": format!("big-{i}")}],
                        "source": {"kind": "user"}
                    }
                }))
                .unwrap(),
            );
            text.push('\n');
        }
        std::fs::write(dir.join("session.v4.jsonl"), text).unwrap();
    }

    #[tokio::test]
    async fn child_detail_is_read_only() {
        let http = HttpFixture::new();
        let before = http.home.snapshot();
        let page = http.supported_detail("2").await;
        assert_eq!(page["messages"].as_array().unwrap().len(), 2);
        assert_eq!(
            http.home.snapshot(),
            before,
            "只读详情不能改写日志或创建账本/游标"
        );
    }
}

#[cfg(test)]
mod content_source_behavior {
    use super::source_contracts::{FixtureHome, CHILD_ID};
    use multi_agents_manager_lib::remote::content::{read_session_messages_with, MessageSourceFn};

    // 只捕获临时 home，再转发到真实统一内容 source；不忽略参数、不实现分页/状态逻辑。
    fn source(home: &FixtureHome) -> Box<MessageSourceFn> {
        let home_path = home.path().to_path_buf();
        Box::new(move |agent, sid, limit| read_session_messages_with(&home_path, agent, sid, limit))
    }

    #[test]
    fn child_detail_returns_the_requested_tail_page() {
        let home = FixtureHome::new();
        let source = source(&home);
        let page = source("dsh", CHILD_ID, 2).expect("临时真实子日志必须可读");
        assert_eq!(page.messages.len(), 2);
        assert_eq!(page.messages[0].kind, "tool-call");
        assert_eq!(page.messages[0].tool_name.as_deref(), Some("send_message"));
        assert_eq!(page.messages[1].kind, "tool-result");
    }

    #[test]
    fn child_detail_expands_to_include_the_seeded_task() {
        let home = FixtureHome::new();
        let source = source(&home);
        let page = source("dsh", CHILD_ID, 20).unwrap();
        assert_eq!(page.messages.len(), 5);
        assert_eq!(page.messages[0].role, "user");
        assert_eq!(page.messages[0].content, "<任务简报多行文本：角色、工作目录、目标目录 .tuvis-attachments/session-<parent-id>/、只读约束、汇报要求>");
        assert_eq!(page.messages[1].tool_name.as_deref(), Some("glob"));
    }

    // H08 行为目标：当前 DSH 内容源裁掉更早消息但 truncated 恒 false。
    // 这是可运行的真实行为断言；只能在编译通过并看到该断言失败后登记为 RED。
    #[test]
    fn child_detail_reports_earlier_messages_when_limited() {
        let home = FixtureHome::new();
        let source = source(&home);
        let page = source("dsh", CHILD_ID, 2).unwrap();
        assert_eq!(page.messages.len(), 2);
        assert!(
            page.truncated,
            "真实子日志共 5 条可见消息，尾页取 2 条时必须提示更早消息"
        );
    }

    #[test]
    fn child_detail_full_page_has_no_earlier_messages() {
        let home = FixtureHome::new();
        let page = source(&home)("dsh", CHILD_ID, 20).unwrap();
        assert_eq!(page.messages.len(), 5);
        assert!(!page.truncated);
    }

    #[test]
    fn reading_child_detail_does_not_change_the_fixture_home() {
        let home = FixtureHome::new();
        let before = home.snapshot();
        let page = source(&home)("dsh", CHILD_ID, 2).unwrap();
        assert_eq!(page.messages.len(), 2);
        assert_eq!(home.snapshot(), before, "只读详情不得改写日志或新增文件");
    }
}
