//! T02 / R02：真实移动端路由、设备门禁、实例登记及预览门；不启动 DSH 或读取真实配置。

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    body::{to_bytes, Body},
    http::{header, HeaderMap, Request, StatusCode},
    Router,
};
#[cfg(feature = "dsh-test-probes")]
use multi_agents_manager_lib::remote::content::dsh_media_parse_count_for_probe;
use multi_agents_manager_lib::{
    database::dao::archive::{query_archive_all_conn, register_sessions_conn},
    inject,
    remote::{
        codex_ipc::CodexControlHub,
        pairing::{persist_device, revoke_device, DeviceStore, NewDevice},
        pin::PinRateLimiter,
        server::{router_with_file_roots, CreateTaskHub, RemoteState, SseRegistry},
    },
    services::dsh_control::{
        self, auth,
        error::DshError,
        instances::{HostKind, InstanceRegistry},
        model::{
            CommandRequest, DshCommandRunner, DshEnvironment, DshListener, DshProcess,
            DshProcessSource, RunOutcome, TestUnobservableEnvironments,
        },
        production::{FsFileSource, SystemClock},
    },
    session::{AgentType, ProcessForm, Session, SessionStatus, SessionsResponse},
};
use serde_json::{json, Value};
use tokio::{sync::oneshot, time::timeout};
use tower::ServiceExt;

const DEVICE: &str = "dsh-media-fixture-device";
const COOKIE: &str = "mam_device=dsh-media-fixture-device";
const SESSION: &str = "session-media-original";
const SECOND_SESSION: &str = "session-media-distinct";
const REPORT: &str = "recorded report bytes";
const SUMMARY: &str = "recorded summary bytes";
const UNRECORDED: &str = "unrecorded file bytes";
const RESTRICTED: &str = "restricted fixture bytes";
const IMAGE_DECOY: &[u8] = b"benign bytes with the same name as the native image";

struct Offline;

impl DshProcessSource for Offline {
    fn list_processes(&self) -> Vec<DshProcess> {
        Vec::new()
    }

    fn listening_sockets(&self) -> Vec<DshListener> {
        Vec::new()
    }
}

impl DshCommandRunner for Offline {
    fn run(&self, _: &CommandRequest) -> Result<RunOutcome, DshError> {
        panic!("media GET must not spawn a host or run a command")
    }
}

struct Fixture {
    app: Router,
    store: DeviceStore,
    instance: String,
    other_instance: String,
    _tmp: tempfile::TempDir,
}

impl Fixture {
    fn new(live: bool) -> Self {
        Self::with_protected_roots_hook(live, || {})
    }

    fn with_protected_roots_hook(live: bool, hook: impl Fn() + Send + Sync + 'static) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let project = root.join("project");
        let restricted = project.join("restricted");
        fs::create_dir_all(&restricted).unwrap();
        fs::write(project.join("report.txt"), REPORT).unwrap();
        fs::write(project.join("summary.txt"), SUMMARY).unwrap();
        fs::write(project.join("unrecorded.txt"), UNRECORDED).unwrap();
        fs::write(restricted.join("note.txt"), RESTRICTED).unwrap();
        fs::write(project.join("native.png"), IMAGE_DECOY).unwrap();
        let oversized = fs::File::create(project.join("oversized.txt")).unwrap();
        oversized.set_len(500 * 1024 + 1).unwrap();
        drop(oversized);

        let home = root.join("dsh-home");
        let other_home = root.join("other-dsh-home");
        write_transcript(
            &home,
            &project,
            SESSION,
            &[
                "report.txt",
                "summary.txt",
                "restricted/note.txt",
                "missing.txt",
                "oversized.txt",
            ],
            true,
        );
        write_transcript(&home, &project, SECOND_SESSION, &["unrecorded.txt"], false);
        write_transcript(&other_home, &project, SESSION, &["unrecorded.txt"], false);

        let env = Arc::new(DshEnvironment {
            home: root.join("user-home"),
            tuvis_home: root.join("tuvis"),
            dsh_state_dir: Some(root.join("dsh-state")),
            processes: Arc::new(Offline),
            files: Arc::new(FsFileSource),
            runner: Arc::new(Offline),
            clock: Arc::new(SystemClock),
            environments: Arc::new(TestUnobservableEnvironments),
        });
        fs::create_dir_all(&env.home).unwrap();
        let registry =
            InstanceRegistry::open_with_env(root.join("instances.json"), Some(Arc::clone(&env)))
                .unwrap();
        let instance = register_retired(&registry, &env, &home);
        let other_instance = register_retired(&registry, &env, &other_home);
        registry.select(&instance).unwrap();
        let registry =
            InstanceRegistry::open_with_env(root.join("instances.json"), Some(env.clone()))
                .unwrap();
        let hub = dsh_control::disabled_arc();
        hub.set_readonly_root_resolver(Arc::new(move |id, sid| {
            registry.resolve_readonly_root(id, sid)
        }));
        let sessions = vec![
            session(SESSION, &instance, &project),
            session(SECOND_SESSION, &instance, &project),
        ];
        let store = DeviceStore::memory();
        store.with(|db| {
            persist_device(
                db,
                &NewDevice {
                    id: DEVICE.into(),
                    name: "isolated media fixture".into(),
                    ua: "test browser".into(),
                    origin_ip: "127.0.0.1".into(),
                    via: "lan".into(),
                    paired_at: chrono::Utc::now().timestamp_millis(),
                },
            )
            .unwrap();
            assert_eq!(register_sessions_conn(db, &sessions), sessions.len());
        });
        let snapshot = move || SessionsResponse {
            sessions: if live { sessions.clone() } else { Vec::new() },
            total_count: if live { sessions.len() } else { 0 },
            waiting_count: 0,
        };
        let archive_store = store.clone();
        let state = Arc::new(RemoteState {
            opencode_native: std::sync::Arc::new(
                multi_agents_manager_lib::services::opencode_native::NativeHub::new(
                    multi_agents_manager_lib::remote::pairing::DeviceStore::memory(),
                ),
            ),
            session_source: Box::new(snapshot.clone()),
            fresh_session_source: Box::new(snapshot),
            pairing_counter: Box::new(Vec::new),
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: Arc::new(|_| 0),
            board_hidden_unhide: Arc::new(|_| 0),
            unread_mark_read: Arc::new(|_, _, _| ()),
            session_close: Arc::new(|_| panic!("read-only fixture")),
            store: store.clone(),
            host_source: Box::new(|| Value::Null),
            message_source: Box::new(|_, _, _| Err("use the registered DSH transcript".into())),
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            watcher_tx: tokio::sync::broadcast::channel(4).0,
            sse_registry: Arc::new(SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            pin_limiter: Mutex::new(PinRateLimiter::new()),
            global_pin_limiter: Mutex::new(PinRateLimiter::global()),
            pin_source: Box::new(|| None),
            now_source: Box::new(|| 0),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            injector: Arc::new(inject::engine::RealInjector),
            resume_spawner: Arc::new(|_| panic!("read-only fixture")),
            archive_source: Box::new(move || archive_store.with(query_archive_all_conn)),
            archive_delete: Arc::new(|_| 0),
            confirm_probe: Arc::new(|_, _, _| false),
            dialog_probe: Arc::new(|_, _| None),
            screen_probe: Arc::new(|_, _| None),
            capability_table: inject::capability::new_table(),
            create_hub: Arc::new(CreateTaskHub::stub()),
            home_source: Box::new(move || Some(env.home.to_string_lossy().into_owned())),
            ui_config_source: Box::new(|| None),
            subagent_source: Default::default(),
            subagent_message_source: Default::default(),
            codex_control: CodexControlHub::disabled_arc(),
            dsh_control: hub,
            dsh_console_mount: None,
            dsh_console_upstream_source: None,
            dsh_auto_adapt_source: Arc::new(|| false),
            desktop_language: Default::default(),
        });
        let app = router_with_file_roots(
            state,
            Arc::new(move || {
                hook();
                Ok(vec![restricted.clone()])
            }),
        );
        Self {
            app,
            store,
            instance,
            other_instance,
            _tmp: tmp,
        }
    }

    fn dsh_home(&self) -> PathBuf {
        self._tmp.path().join("dsh-home")
    }

    #[cfg(feature = "dsh-test-probes")]
    fn media_parse_count(&self) -> usize {
        dsh_media_parse_count_for_probe(&self.dsh_home().canonicalize().unwrap(), SESSION)
    }

    fn session_dir(&self, sid: &str) -> PathBuf {
        self.dsh_home().join("sessions/project").join(sid)
    }

    fn transcript_path(&self, filename: &str) -> PathBuf {
        self.session_dir(SESSION).join(filename)
    }

    #[cfg(unix)]
    fn write_project_file(&self, name: &str, contents: &str) {
        fs::write(self._tmp.path().join("project").join(name), contents).unwrap();
    }

    async fn get(&self, uri: &str, cookie: Option<&str>) -> Reply {
        let mut request = Request::builder().uri(uri);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let (parts, body) = response.into_parts();
        Reply {
            status: parts.status,
            headers: parts.headers,
            body: to_bytes(body, 6 * 1024 * 1024).await.unwrap().to_vec(),
        }
    }

    async fn reference(&self, sid: &str, name: &str) -> String {
        let response = self.get(
            &format!("/m/api/v1/session-messages?agent_type=dsh&session_id={sid}&instanceId={}&history=1", self.instance),
            Some(COOKIE),
        ).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "registered transcript control: {response:?}"
        );
        let body = response.json();
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|message| message["media"].as_array())
            .flatten()
            .find(|media| media["name"] == name)
            .and_then(|media| media["refId"].as_str())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| panic!("transcript control: {name} has no reference in {body}"))
            .to_owned()
    }

    async fn preview(&self, path: &str) -> Reply {
        self.get(
            &format!(
                "/m/api/v1/file?session_id={SESSION}&instanceId={}&path={path}",
                self.instance
            ),
            Some(COOKIE),
        )
        .await
    }

    async fn media(&self, reference: &str) -> Reply {
        self.get(&media_uri(&self.instance, SESSION, reference), Some(COOKIE))
            .await
    }
}

#[derive(Debug)]
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|err| panic!("expected JSON: {err}; {self:?}"))
    }

    fn assert_rejected(&self) {
        assert!(
            self.status.is_client_error(),
            "expected explicit refusal: {self:?}"
        );
        let body = String::from_utf8_lossy(&self.body);
        for forbidden in [
            REPORT,
            SUMMARY,
            UNRECORDED,
            RESTRICTED,
            std::str::from_utf8(IMAGE_DECOY).unwrap(),
        ] {
            assert!(
                !body.contains(forbidden),
                "unexpected file contents: {self:?}"
            );
        }
    }
}

fn media_uri(instance: &str, sid: &str, reference: &str) -> String {
    format!("/m/api/v1/dsh/media?instanceId={instance}&sessionId={sid}&refId={reference}")
}

fn session(sid: &str, instance: &str, project: &Path) -> Session {
    Session {
        id: sid.into(),
        agent_type: AgentType::Dsh,
        project_name: "media project".into(),
        project_path: project.to_string_lossy().into_owned(),
        title: None,
        git_branch: None,
        github_url: None,
        status: SessionStatus::Idle,
        last_message: None,
        last_message_role: None,
        last_message_subagent_report: false,
        flap_from_subagent_activity: false,
        last_activity_at: "2026-11-21T00:00:00Z".into(),
        pid: 1,
        cpu_usage: 0.0,
        active_subagent_count: 0,
        form: ProcessForm::App,
        jump_supported: false,
        unread: false,
        unread_turned_green_at_ms: None,
        dsh_instance_id: Some(instance.into()),
        dsh_subagent: None,
    }
}

fn register_retired(registry: &InstanceRegistry, env: &DshEnvironment, home: &Path) -> String {
    let profile = home.join("profiles/desktop");
    fs::create_dir_all(&profile).unwrap();
    let key = format!("fixture origin for {}", home.display());
    registry
        .register_candidate_with_home(HostKind::Desktop, &profile, Some(home))
        .unwrap();
    let instance = registry
        .register(
            HostKind::Desktop,
            &profile,
            &auth::key_digest(key.as_bytes()),
        )
        .unwrap();
    let state = auth::instance_state_dir(env, &instance.instance_id).unwrap();
    fs::create_dir_all(&state).unwrap();
    fs::write(
        state.join("bindings.json"),
        json!({"bindings":[{
            "bindingId":"fixture", "publicKeySpki":"",
            "revokedKeySpki":auth::b64url_encode(key.as_bytes()),
            "executable":"/fixture/dsh", "pid":1, "startedAtMs":100,
            "profileDir":profile, "createdAtMs":101, "state":"revoked"
        }]})
        .to_string(),
    )
    .unwrap();
    registry
        .retire_identity_fingerprint(&instance.instance_id)
        .unwrap();
    instance.instance_id
}

fn write_transcript(home: &Path, cwd: &Path, sid: &str, paths: &[&str], image: bool) {
    let dir = home.join("sessions/project").join(sid);
    fs::create_dir_all(&dir).unwrap();
    let mut lines = vec![transcript_header(cwd, sid)];
    lines.extend(present_events(1, "present-fixture", paths));
    if image {
        lines.push(json!({"type":"user/message", "seq":3, "time":1003,
            "data":{"id":"native-image-event", "role":"user", "source":{"kind":"user"}, "content":[{
                "type":"image", "attachment":{"attachmentId":"native-image-id", "bytes":256,
                "height":16, "width":16, "mediaType":"image/png", "name":"native.png"}
            }]}
        }));
    }
    let frame = zstd::stream::encode_all(jsonl(&lines).as_bytes(), 3).unwrap();
    fs::write(dir.join("session.v4.jsonl.zstd"), frame).unwrap();
}

fn transcript_header(cwd: &Path, sid: &str) -> Value {
    json!({"type":"session", "version":4, "id":sid, "cwd":cwd, "createdAt":1000, "isSeeded":false})
}

fn present_events(seq: u64, call_id: &str, paths: &[&str]) -> [Value; 2] {
    [
        json!({"type":"tool/call", "seq":seq, "time":1000 + seq,
            "data":{"name":"present", "callId":call_id, "arguments":"{}"}}),
        json!({"type":"deliverables/presented", "seq":seq + 1, "time":1001 + seq,
            "data":{"callId":call_id, "files":paths.iter().map(|path| json!({"path":path})).collect::<Vec<_>>()}}),
    ]
}

fn jsonl(lines: &[Value]) -> String {
    lines
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

fn write_plain_transcript(path: &Path, cwd: &Path, names: &[&str]) {
    write_plain_transcript_for_session(path, cwd, SESSION, names);
}

fn write_plain_transcript_for_session(path: &Path, cwd: &Path, sid: &str, names: &[&str]) {
    let mut lines = vec![transcript_header(cwd, sid)];
    lines.extend(present_events(1, "present-fixture", names));
    fs::write(path, jsonl(&lines)).unwrap();
}

#[tokio::test]
async fn recorded_present_file_uses_the_same_preview_as_the_file_route() {
    let fixture = Fixture::new(true);
    let preview = fixture.preview("report.txt").await;
    assert_eq!(
        preview.status,
        StatusCode::OK,
        "ordinary preview control: {preview:?}"
    );
    assert_eq!(preview.json()["content"], REPORT);
    let reference = fixture.reference(SESSION, "report.txt").await;
    let media = fixture.media(&reference).await;
    assert_eq!(
        media.status,
        StatusCode::OK,
        "recorded reference must reach the media router: {media:?}"
    );
    assert_eq!(media.json(), preview.json());
    assert_eq!(media.headers[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn recorded_present_file_remains_readable_from_off_history() {
    let fixture = Fixture::new(false);
    let reference = fixture.reference(SESSION, "report.txt").await;
    let media = fixture.media(&reference).await;
    assert_eq!(
        media.status,
        StatusCode::OK,
        "archived reference with no live session: {media:?}"
    );
    assert_eq!(media.json()["content"], REPORT);
}

#[tokio::test]
async fn recorded_present_file_remains_previewable_with_a_torn_zstd_tail() {
    let fixture = Fixture::new(true);
    let reference = fixture.reference(SESSION, "report.txt").await;
    let transcript = fixture.transcript_path("session.v4.jsonl.zstd");
    let mut tail = zstd::stream::encode_all(
        &b"\n{\"type\":\"user/message\",\"seq\":4,\"time\":1004}\n"[..],
        3,
    )
    .unwrap();
    tail.truncate(tail.len() - 5);
    // 模拟追加写入在最后一帧中断：既有完整帧保留，不改写其内容。
    fs::OpenOptions::new()
        .append(true)
        .open(transcript)
        .unwrap()
        .write_all(&tail)
        .unwrap();

    let media = fixture.media(&reference).await;
    assert_eq!(
        media.status,
        StatusCode::OK,
        "a torn tail must not hide the report in a complete frame: {media:?}"
    );
    assert_eq!(media.json()["content"], REPORT);
}

#[cfg(all(unix, feature = "dsh-test-probes"))]
#[tokio::test]
async fn distinct_sequential_media_refs_share_one_transcript_parse() {
    let fixture = Fixture::new(true);
    let report = fixture.reference(SESSION, "report.txt").await;
    let summary = fixture.reference(SESSION, "summary.txt").await;
    assert_ne!(report, summary);
    assert_eq!(fixture.media_parse_count(), 0);

    for (reference, contents) in [(&report, REPORT), (&summary, SUMMARY)] {
        let media = fixture.media(reference).await;
        assert_eq!(media.status, StatusCode::OK, "{media:?}");
        assert_eq!(media.json()["content"], contents);
    }
    assert_eq!(fixture.media_parse_count(), 1);
}

#[cfg(all(unix, feature = "dsh-test-probes"))]
#[tokio::test]
async fn distinct_concurrent_cold_media_refs_share_one_transcript_parse() {
    let fixture = Fixture::new(true);
    let report = fixture.reference(SESSION, "report.txt").await;
    let summary = fixture.reference(SESSION, "summary.txt").await;
    assert_ne!(report, summary);
    assert_eq!(fixture.media_parse_count(), 0);

    let (report, summary) = tokio::join!(fixture.media(&report), fixture.media(&summary));
    for (media, contents) in [(report, REPORT), (summary, SUMMARY)] {
        assert_eq!(media.status, StatusCode::OK, "{media:?}");
        assert_eq!(media.json()["content"], contents);
    }
    assert_eq!(fixture.media_parse_count(), 1);
}

#[tokio::test]
async fn appending_a_complete_frame_makes_new_references_resolvable() {
    let fixture = Fixture::new(true);
    let original = fixture.reference(SESSION, "report.txt").await;
    assert_eq!(fixture.media(&original).await.status, StatusCode::OK);

    let events = jsonl(&present_events(4, "present-appended", &["unrecorded.txt"]));
    let frame = zstd::stream::encode_all(format!("\n{events}\n").as_bytes(), 3).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(fixture.transcript_path("session.v4.jsonl.zstd"))
        .unwrap()
        .write_all(&frame)
        .unwrap();

    #[cfg(feature = "dsh-test-probes")]
    {
        // 在重新拉正文前读取旧引用：追加必须让媒体查询自行刷新索引。
        let before = fixture.media_parse_count();
        assert_eq!(fixture.media(&original).await.status, StatusCode::OK);
        assert_eq!(fixture.media_parse_count(), before + 1);
    }
    let appended = fixture.reference(SESSION, "unrecorded.txt").await;
    assert_ne!(appended, original);
    let media = fixture.media(&appended).await;
    assert_eq!(
        media.status,
        StatusCode::OK,
        "appended reference: {media:?}"
    );
    assert_eq!(media.json()["content"], UNRECORDED);
}

#[cfg(unix)]
#[tokio::test]
async fn atomic_transcript_replacement_invalidates_cached_references_at_same_size_and_mtime() {
    let fixture = Fixture::new(true);
    let cwd = fixture._tmp.path().canonicalize().unwrap().join("project");
    let transcript = fixture.transcript_path("session.v5.jsonl");
    write_plain_transcript(&transcript, &cwd, &["report.txt"]);
    let original = fixture.reference(SESSION, "report.txt").await;
    assert_eq!(fixture.media(&original).await.status, StatusCode::OK);

    let replaced = fixture.transcript_path("replacement.tmp");
    fixture.write_project_file("update.txt", UNRECORDED);
    write_plain_transcript(&replaced, &cwd, &["update.txt"]);
    replace_preserving_size_and_mtime(&transcript, &replaced);

    let stale = fixture.media(&original).await;
    stale.assert_rejected();
    assert_eq!(stale.status, StatusCode::NOT_FOUND);
    let fresh = fixture.reference(SESSION, "update.txt").await;
    let media = fixture.media(&fresh).await;
    assert_eq!(
        media.status,
        StatusCode::OK,
        "replacement reference: {media:?}"
    );
    assert_eq!(media.json()["content"], UNRECORDED);
}

#[cfg(unix)]
fn replace_preserving_size_and_mtime(transcript: &Path, replacement: &Path) {
    use std::os::unix::fs::MetadataExt;

    // 同一路径、长度、mtime，只更换文件本身；不依赖时钟精度或 sleep。
    let prior = fs::metadata(transcript).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(replacement)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(prior.modified().unwrap()))
        .unwrap();
    fs::rename(replacement, transcript).unwrap();
    let current = fs::metadata(transcript).unwrap();
    assert_eq!(current.len(), prior.len());
    assert_eq!(current.modified().unwrap(), prior.modified().unwrap());
    assert_ne!(current.ino(), prior.ino());
}

#[cfg(unix)]
#[tokio::test]
async fn replaced_sibling_transcript_with_duplicate_sid_invalidates_media_ownership() {
    let fixture = Fixture::new(true);
    let cwd = fixture._tmp.path().canonicalize().unwrap().join("project");
    let sibling_dir = fixture.session_dir(SECOND_SESSION);
    let sibling = sibling_dir.join("session.v5.jsonl");
    write_plain_transcript_for_session(&sibling, &cwd, SECOND_SESSION, &["unrecorded.txt"]);

    let reference = fixture.reference(SESSION, "report.txt").await;
    // 真实归属核验必须枚举同根的两份头；预热时另一份头属于不同会话。
    let control = fixture.media(&reference).await;
    assert_eq!(control.status, StatusCode::OK, "unique owner: {control:?}");
    assert_eq!(control.json()["content"], REPORT);

    let replacement = sibling_dir.join("replacement.tmp");
    write_plain_transcript_for_session(&replacement, &cwd, SESSION, &["unrecorded.txt"]);
    replace_preserving_size_and_mtime(&sibling, &replacement);

    let ambiguous = fixture.media(&reference).await;
    ambiguous.assert_rejected();
    assert_eq!(ambiguous.status, StatusCode::FORBIDDEN);

    // 去掉歧义后，原会话从未修改过的引用仍能读取。
    fs::remove_file(sibling).unwrap();
    let recovered = fixture.media(&reference).await;
    assert_eq!(
        recovered.status,
        StatusCode::OK,
        "unique again: {recovered:?}"
    );
    assert_eq!(recovered.json()["content"], REPORT);
}

#[tokio::test]
async fn highest_generation_supersedes_the_previous_reference_set() {
    let fixture = Fixture::new(true);
    let original = fixture.reference(SESSION, "report.txt").await;
    assert_eq!(fixture.media(&original).await.status, StatusCode::OK);

    let cwd = fixture._tmp.path().canonicalize().unwrap().join("project");
    write_plain_transcript(
        &fixture.transcript_path("session.v5.jsonl"),
        &cwd,
        &["unrecorded.txt"],
    );
    assert!(fixture.transcript_path("session.v4.jsonl.zstd").is_file());

    let stale = fixture.media(&original).await;
    stale.assert_rejected();
    assert_eq!(stale.status, StatusCode::NOT_FOUND);
    let fresh = fixture.reference(SESSION, "unrecorded.txt").await;
    let media = fixture.media(&fresh).await;
    assert_eq!(
        media.status,
        StatusCode::OK,
        "new-generation reference: {media:?}"
    );
    assert_eq!(media.json()["content"], UNRECORDED);
}

#[tokio::test]
async fn media_reference_index_rejects_one_over_the_reference_budget() {
    let fixture = Fixture::new(true);
    let cwd = fixture._tmp.path().canonicalize().unwrap().join("project");
    let transcript = fixture.transcript_path("session.v5.jsonl");
    let placeholders: Vec<_> = (1..4096).map(|i| format!("placeholder-{i}.txt")).collect();
    let mut files = vec!["report.txt"];
    files.extend(placeholders.iter().map(String::as_str));
    write_plain_transcript(&transcript, &cwd, &files);
    let reference = fixture.reference(SESSION, "report.txt").await;
    let control = fixture.media(&reference).await;
    assert_eq!(
        control.status,
        StatusCode::OK,
        "4096 references: {control:?}"
    );
    assert_eq!(control.json()["content"], REPORT);

    // 第一项仍在原位。超出预算时不能悄悄截断索引并继续返回其内容。
    files.push("over-budget.txt");
    write_plain_transcript(&transcript, &cwd, &files);
    let media = fixture.media(&reference).await;
    media.assert_rejected();
    assert_eq!(media.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn recorded_present_file_in_registered_root_is_readable_after_d22() {
    let fixture = Fixture::new(true);
    let preview = fixture.preview("restricted/note.txt").await;
    // D22：普通预览及转录附件遵循同一目录准入规则。
    assert_eq!(preview.status, StatusCode::OK);
    assert_eq!(preview.json()["content"], RESTRICTED);
    let reference = fixture.reference(SESSION, "note.txt").await;
    let media = fixture.media(&reference).await;
    assert_eq!(media.status, StatusCode::OK);
    assert_eq!(media.json()["content"], RESTRICTED);
}

#[tokio::test]
async fn recorded_present_file_reports_a_missing_file() {
    let fixture = Fixture::new(true);
    let reference = fixture.reference(SESSION, "missing.txt").await;
    let media = fixture.media(&reference).await;
    media.assert_rejected();
    assert_eq!(media.json()["error"], "not_found");
}

#[tokio::test]
async fn recorded_present_file_respects_the_common_preview_budget() {
    let fixture = Fixture::new(true);
    let reference = fixture.reference(SESSION, "oversized.txt").await;
    let media = fixture.media(&reference).await;
    media.assert_rejected();
    assert_eq!(media.json()["error"], "too_large");
}

#[tokio::test]
async fn native_image_is_unavailable_without_a_proven_upstream() {
    let fixture = Fixture::new(true);
    let reference = fixture.reference(SESSION, "native.png").await;
    let media = fixture.media(&reference).await;
    media.assert_rejected();
    let error = media.json();
    assert!(
        error["error"].as_str().is_some_and(|code| !code.is_empty()),
        "missing native-media reason: {media:?}"
    );
}

#[tokio::test]
async fn forged_reference_is_not_resolved() {
    let fixture = Fixture::new(true);
    let reference = fixture.reference(SESSION, "report.txt").await;
    let last = reference.chars().last().unwrap();
    let forged = format!(
        "{}{}",
        &reference[..reference.len() - last.len_utf8()],
        if last == '0' { '1' } else { '0' }
    );
    fixture.media(&forged).await.assert_rejected();
}

#[tokio::test]
async fn reference_from_another_session_is_not_resolved() {
    let fixture = Fixture::new(true);
    let reference = fixture.reference(SECOND_SESSION, "unrecorded.txt").await;
    let original = fixture
        .get(
            &media_uri(&fixture.instance, SECOND_SESSION, &reference),
            Some(COOKIE),
        )
        .await;
    assert_eq!(
        original.status,
        StatusCode::OK,
        "request control in owning session: {original:?}"
    );
    assert_eq!(original.json()["content"], UNRECORDED);
    fixture.media(&reference).await.assert_rejected();
}

#[tokio::test]
async fn same_session_id_in_another_registered_instance_is_rejected() {
    let fixture = Fixture::new(false);
    let reference = fixture.reference(SESSION, "report.txt").await;
    let media = fixture
        .get(
            &media_uri(&fixture.other_instance, SESSION, &reference),
            Some(COOKIE),
        )
        .await;
    media.assert_rejected();
    assert_eq!(media.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn missing_or_invalid_device_is_rejected() {
    let fixture = Fixture::new(true);
    let reference = fixture.reference(SESSION, "report.txt").await;
    let uri = media_uri(&fixture.instance, SESSION, &reference);
    for cookie in [None, Some("mam_device=not-a-paired-device")] {
        let media = fixture.get(&uri, cookie).await;
        media.assert_rejected();
        assert_eq!(media.status, StatusCode::FORBIDDEN);
    }
}

#[tokio::test]
async fn revocation_invalidates_the_previously_accepted_cookie() {
    let fixture = Fixture::new(true);
    let reference = fixture.reference(SESSION, "report.txt").await;
    assert_eq!(fixture.media(&reference).await.status, StatusCode::OK);
    fixture.store.with(|db| revoke_device(db, DEVICE).unwrap());
    let media = fixture.media(&reference).await;
    media.assert_rejected();
    assert_eq!(media.status, StatusCode::FORBIDDEN);
}

type MediaPause = Arc<Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>>;

fn fixture_with_media_pause() -> (Fixture, MediaPause) {
    let latch = MediaPause::default();
    let fixture = Fixture::with_protected_roots_hook(true, {
        let latch = Arc::clone(&latch);
        move || {
            let pending = latch.lock().unwrap().take();
            if let Some((entered, resume)) = pending {
                // 已通过初次设备鉴权与引用解析；暂停在线程读取文件之前。
                entered.send(()).expect("test must observe the roots hook");
                resume.blocking_recv().expect("test must release the read");
            }
        }
    });
    (fixture, latch)
}

async fn media_with_paused_mutation(
    fixture: &Fixture,
    latch: &MediaPause,
    reference: &str,
    mutate: impl FnOnce(),
) -> Reply {
    let (entered, at_roots) = oneshot::channel();
    let (release, resume) = oneshot::channel();
    *latch.lock().unwrap() = Some((entered, resume));

    timeout(Duration::from_secs(15), async {
        let request = fixture.media(reference);
        tokio::pin!(request);
        tokio::select! {
            entered = at_roots => entered.expect("roots hook must signal the in-flight read"),
            response = &mut request => panic!("request completed before reaching roots: {response:?}"),
        }
        mutate();
        release.send(()).expect("in-flight read must await release");
        request.await
    })
    .await
    .expect("in-flight request did not finish")
}

#[tokio::test]
async fn revocation_during_media_read_withholds_file_contents() {
    let (fixture, latch) = fixture_with_media_pause();
    let reference = fixture.reference(SESSION, "report.txt").await;
    let control = fixture.media(&reference).await;
    assert_eq!(
        control.status,
        StatusCode::OK,
        "paired control: {control:?}"
    );
    assert_eq!(control.json()["content"], REPORT);

    let media = media_with_paused_mutation(&fixture, &latch, &reference, || {
        fixture.store.with(|db| revoke_device(db, DEVICE).unwrap());
    })
    .await;
    media.assert_rejected();
    assert_eq!(media.status, StatusCode::FORBIDDEN);
    assert_eq!(media.json(), json!({"error":"forbidden"}));
}

#[tokio::test]
async fn generation_change_during_media_read_withholds_old_file_contents() {
    let (fixture, latch) = fixture_with_media_pause();
    let reference = fixture.reference(SESSION, "report.txt").await;
    let control = fixture.media(&reference).await;
    assert_eq!(
        control.status,
        StatusCode::OK,
        "old generation: {control:?}"
    );
    assert_eq!(control.json()["content"], REPORT);

    let media = media_with_paused_mutation(&fixture, &latch, &reference, || {
        write_plain_transcript(
            &fixture.transcript_path("session.v5.jsonl"),
            &fixture._tmp.path().canonicalize().unwrap().join("project"),
            &["unrecorded.txt"],
        );
    })
    .await;
    media.assert_rejected();

    let fresh = fixture.reference(SESSION, "unrecorded.txt").await;
    let control = fixture.media(&fresh).await;
    assert_eq!(
        control.status,
        StatusCode::OK,
        "new generation: {control:?}"
    );
    assert_eq!(control.json()["content"], UNRECORDED);
}

#[tokio::test]
async fn a_client_path_cannot_substitute_for_a_reference() {
    let fixture = Fixture::new(true);
    let preview = fixture.preview("unrecorded.txt").await;
    assert_eq!(preview.status, StatusCode::OK);
    assert_eq!(preview.json()["content"], UNRECORDED);
    let uri = format!(
        "/m/api/v1/dsh/media?instanceId={}&sessionId={SESSION}&path=unrecorded.txt",
        fixture.instance
    );
    let media = fixture.get(&uri, Some(COOKIE)).await;
    media.assert_rejected();
    assert_eq!(media.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_client_path_cannot_override_the_recorded_file() {
    let fixture = Fixture::new(true);
    let reference = fixture.reference(SESSION, "report.txt").await;
    let uri = format!(
        "{}&path=unrecorded.txt",
        media_uri(&fixture.instance, SESSION, &reference)
    );
    let media = fixture.get(&uri, Some(COOKIE)).await;
    if media.status == StatusCode::OK {
        assert_eq!(media.json()["content"], REPORT);
    } else {
        media.assert_rejected();
        assert_eq!(media.status, StatusCode::BAD_REQUEST);
    }
}
