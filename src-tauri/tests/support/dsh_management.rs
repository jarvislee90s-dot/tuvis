#![allow(dead_code)]

use multi_agents_manager_lib::services::dsh_control::{
    install::DshPluginManager,
    model::{DshProcess, ManagementPhase, RunOutcome},
};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub(super) fn desktop_host_process(
    pid: u32,
    install_dir: &str,
    profile_token: &str,
    started_at_ms: i64,
) -> DshProcess {
    DshProcess {
        pid,
        exe: PathBuf::from(format!(r"{install_dir}\DeepSeek Harness.exe")),
        cmdline: vec![
            format!(r"{install_dir}\DeepSeek Harness.exe"),
            "--expose-internals".into(),
            format!(
                r"{install_dir}\resources\app.asar\dsh\node_modules\@deepseek-ai\dsh-desktop-host\lib\index.js"
            ),
            format!(r"{install_dir}\resources\app.asar\dsh"),
            profile_token.into(),
            format!(r"{install_dir}\resources\runtime\primary-runtime"),
        ],
        started_at_ms,
    }
}

pub(super) fn invalid_profile_name(name: &str) -> Option<String> {
    if name.trim().is_empty() {
        return Some("profile name must not be empty".into());
    }
    if name.contains('/') || name.contains('\\') {
        return Some(format!(
            "dsh: Error: invalid profile name {name:?}: profile names must not contain path separators"
        ));
    }
    if name == "." || name == ".." || name == "node_modules" {
        return Some(format!("dsh: Error: invalid profile name {name:?}"));
    }
    None
}

pub(super) async fn wait_phase(
    mgr: &DshPluginManager,
    id: &str,
    want: &[ManagementPhase],
) -> multi_agents_manager_lib::services::dsh_control::model::ManagementJob {
    use multi_agents_manager_lib::services::dsh_control::model::ManagementJob;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let job: ManagementJob = mgr.job(id).expect("作业可查");
        if want.contains(&job.phase) {
            return job;
        }
        assert!(
            Instant::now() < deadline,
            "等待 {want:?} 超时，最后作业 {job:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub(super) fn outcome(
    exit_code: i32,
    stdout: impl Into<String>,
    stderr: impl Into<String>,
) -> RunOutcome {
    RunOutcome {
        exit_code,
        stdout: stdout.into(),
        stderr: stderr.into(),
    }
}
