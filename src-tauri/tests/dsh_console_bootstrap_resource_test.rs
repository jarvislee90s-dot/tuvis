use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::OriginalUri;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;
use multi_agents_manager_lib::remote::console_capability::ConsoleCapability;
use multi_agents_manager_lib::remote::console_mount::{self, ConsoleMount};
use multi_agents_manager_lib::services::dsh_control::transport::PeerGate;
use tower::ServiceExt;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const OTHER_TOKEN: &str = "fedcba9876543210fedcba9876543210";
const SCRIPT: &str = "plugins/??fixture-a/client.js,fixture-b/client.js&rev=0123456789ab";
const JAVASCRIPT: &str = "void true;";
const JS_MIME: &str = "text/javascript; charset=utf-8";

struct Fixture {
    app: Router,
    script_hits: Arc<AtomicUsize>,
    upstream: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.upstream.abort();
    }
}

impl Fixture {
    async fn new(declaration: String, script_mime: &'static str) -> Self {
        let script_hits = Arc::new(AtomicUsize::new(0));
        let hits = Arc::clone(&script_hits);
        let upstream = Router::new()
            .route(
                "/",
                get(move || async move { ([(header::CONTENT_TYPE, "text/html")], declaration) }),
            )
            .route(
                "/plugins/",
                any(move |OriginalUri(uri): OriginalUri| async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let resource = uri
                        .path_and_query()
                        .and_then(|target| target.as_str().strip_prefix('/'));
                    if resource != Some(SCRIPT) {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    ([(header::CONTENT_TYPE, script_mime)], JAVASCRIPT).into_response()
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let upstream = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });

        let mount =
            Arc::new(ConsoleMount::new().with_device_check(|device| device == "fixture-device"));
        mount
            .install_browser_credential(
                "fixture-instance",
                1,
                &authority,
                "browser=synthetic".into(),
                PeerGate::new(),
            )
            .unwrap();
        for token in [TOKEN, OTHER_TOKEN] {
            mount
                .register(ConsoleCapability {
                    token: token.into(),
                    device_id: "fixture-device".into(),
                    instance_id: "fixture-instance".into(),
                    authority: authority.clone(),
                    epoch: 1,
                    bootstrap: false,
                    expires_at_ms: mount.now() + 60_000,
                })
                .unwrap();
        }
        Self {
            app: console_mount::router(mount),
            script_hits,
            upstream,
        }
    }

    async fn with_script_tag(script_mime: &'static str) -> Self {
        Self::new(
            format!(r#"<!doctype html><script src="{SCRIPT}"></script>"#),
            script_mime,
        )
        .await
    }

    async fn request(&self, method: Method, token: &str, resource: &str) -> Response {
        let request = Request::builder()
            .method(method)
            .uri(format!("/console/{token}/{resource}"))
            .body(Body::empty())
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.app.clone().oneshot(request))
            .await
            .unwrap()
            .unwrap()
    }

    async fn load_home(&self) {
        let home = self.request(Method::GET, TOKEN, "").await;
        assert_eq!(home.status(), StatusCode::OK, "控制台首页必须先加载成功");
        assert_eq!(home.headers()[header::CONTENT_TYPE], "text/html");
        let html = to_bytes(home.into_body(), 1024).await.unwrap();
        assert!(std::str::from_utf8(&html).unwrap().contains(SCRIPT));
    }

    async fn assert_script_delivered(&self) {
        let script = self.request(Method::GET, TOKEN, SCRIPT).await;
        assert_eq!(script.status(), StatusCode::OK);
        assert_eq!(script.headers()[header::CONTENT_TYPE], JS_MIME);
        let javascript = to_bytes(script.into_body(), 1024).await.unwrap();
        assert_eq!(javascript.as_ref(), JAVASCRIPT.as_bytes());
        assert_eq!(self.script_hits.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn same_capability_loads_the_combo_script_declared_by_its_html_homepage() {
    let fixture = Fixture::with_script_tag(JS_MIME).await;
    fixture.load_home().await;
    fixture.assert_script_delivered().await;
}

#[tokio::test]
async fn combo_request_before_homepage_still_reaches_upstream() {
    let fixture = Fixture::with_script_tag(JS_MIME).await;
    fixture.assert_script_delivered().await;
}

#[tokio::test]
async fn upstream_retains_control_of_resource_mime() {
    let fixture = Fixture::with_script_tag("text/plain").await;
    let response = fixture.request(Method::GET, TOKEN, SCRIPT).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "text/plain");
    assert_eq!(fixture.script_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn preload_as_script_declares_the_combo_for_the_same_capability() {
    let fixture = Fixture::new(
        format!(r#"<!doctype html><link rel="preload" as="script" href="{SCRIPT}">"#),
        JS_MIME,
    )
    .await;
    fixture.load_home().await;
    fixture.assert_script_delivered().await;
}
