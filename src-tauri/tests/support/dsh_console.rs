#![allow(dead_code)]

use axum::body::Body;
use axum::{
    http::{Request, StatusCode},
    Router,
};
use multi_agents_manager_lib::remote::console_capability::ConsoleCapability;
use multi_agents_manager_lib::remote::console_mount::ConsoleMount;
use std::path::Path;
use std::time::Duration;
use tokio::time::timeout;
use tower::ServiceExt;

pub(super) fn session_capability(
    device: &str,
    mount: &ConsoleMount,
    token: &str,
    instance: &str,
    epoch: u64,
    authority: &str,
) -> ConsoleCapability {
    ConsoleCapability {
        token: token.into(),
        device_id: device.into(),
        instance_id: instance.into(),
        authority: authority.into(),
        epoch,
        bootstrap: false,
        expires_at_ms: mount.now() + 60_000,
    }
}

pub(super) fn home_of(profile_dir: &Path) -> String {
    profile_dir
        .ancestors()
        .nth(2)
        .expect("profile 形如 <home>/profiles/desktop")
        .to_string_lossy()
        .into_owned()
}

pub(super) async fn get_console(wait: Duration, router: &Router, token: &str) -> StatusCode {
    timeout(
        wait,
        router.clone().oneshot(
            Request::builder()
                .uri(format!("/console/{token}/"))
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await
    .expect("控制台请求超时")
    .unwrap()
    .status()
}
