//! R01 复审：同 PID、同 start、同 exe 路径的真 exec，不得让采样采信上一映像的环境。
//! 专属子进程 env_clear；默认 reader 验证 Incomplete。dsh-test-probes 下使用
//! 合成环境帧和 FullFor 输入，进程身份与 exec 仍由专属子进程实测；不证明 OS 环境完整可见。
//! 本轮不运行 Cargo；回归尚未执行。
#![cfg(target_os = "macos")]

#[path = "support/r01_environment_direct.rs"]
mod direct;
#[path = "support/r01_same_path_exec_fixture.rs"]
mod fixture;
#[cfg(feature = "dsh-test-probes")]
#[path = "support/r01_owned_procargs2_source.rs"]
mod source;

#[cfg(feature = "dsh-test-probes")]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(feature = "dsh-test-probes")]
use std::sync::Mutex;

use direct::{assert_logs_redacted, assert_redacted, capture_logs};
use fixture::OwnedExecProcess;
#[cfg(feature = "dsh-test-probes")]
use fixture::{DSH_A, DSH_B, HOME_A, HOME_B};
#[cfg(feature = "dsh-test-probes")]
use multi_agents_manager_lib::services::dsh_control::process_environment::{
    parse_allowlisted_nul, AllowlistedEnvironment, EnvironmentField, ReadExtent, ReadFailure,
};
use multi_agents_manager_lib::services::dsh_control::process_environment::{
    sample_initial_environment, InitialEnvironmentObservation, MacOsProcessEnvironmentReader,
    ObservedProcessIdentity, ParseEnvironmentError, ProcessEnvironmentReader, ReadCompleteness,
    SampleEnvironmentError,
};

#[cfg(feature = "dsh-test-probes")]
fn expected_environment(home: &str, dsh_home: &str) -> AllowlistedEnvironment {
    AllowlistedEnvironment {
        dsh_home: EnvironmentField::Value(dsh_home.to_owned()),
        home: EnvironmentField::Value(home.to_owned()),
    }
}

fn identity(reader: &dyn ProcessEnvironmentReader, pid: u32) -> ObservedProcessIdentity {
    let (actual, logs) = capture_logs(|| reader.identity_now(pid));
    assert_redacted(&format!("{actual:?}"));
    assert_logs_redacted(&logs);
    let identity = actual
        .expect("专属 PID 身份读取失败")
        .expect("专属 PID 应仍存活");
    assert_eq!(identity.pid, pid, "只允许读取本测试专属 PID");
    identity
}

fn sample(
    reader: &dyn ProcessEnvironmentReader,
    expected: &ObservedProcessIdentity,
) -> Result<InitialEnvironmentObservation, SampleEnvironmentError> {
    let (actual, logs) = capture_logs(|| sample_initial_environment(reader, expected));
    assert_redacted(&format!("{actual:?}"));
    assert_logs_redacted(&logs);
    actual
}

fn assert_same_process_new_exec(before: &ObservedProcessIdentity, after: &ObservedProcessIdentity) {
    assert_eq!(after.pid, before.pid);
    assert_eq!(after.started_at_ms, before.started_at_ms);
    assert_eq!(after.started_at_ms % 1_000, 0);
    assert_eq!(after.executable, before.executable);
    let old = before
        .native_generation
        .as_ref()
        .expect("exec 前 native 代际可读");
    let new = after
        .native_generation
        .as_ref()
        .expect("exec 后 native 代际可读");
    assert_eq!(
        new.unique_id, old.unique_id,
        "同一进程的 unique_id 必须保持"
    );
    assert_ne!(
        new.pid_version, old.pid_version,
        "同路径 exec 必须改变 pid_version"
    );
}

fn assert_default_incomplete(
    reader: &MacOsProcessEnvironmentReader,
    expected: &ObservedProcessIdentity,
) {
    let mut output = [0xa5; 256];
    let extent = reader
        .read_initial_environment_into(expected.pid, &mut output)
        .unwrap();
    assert_eq!(extent.bytes_written, 0);
    assert_eq!(extent.completeness, ReadCompleteness::Incomplete);
    assert_eq!(
        sample(reader, expected),
        Err(SampleEnvironmentError::Parse(
            ParseEnvironmentError::ReadIncomplete
        ))
    );
}

/// 合成帧读取后、生产采样再次读取真实身份前，安排专属子进程执行真 exec。
#[cfg(feature = "dsh-test-probes")]
struct ExecAfterFirstRead {
    real: MacOsProcessEnvironmentReader,
    source: std::sync::Arc<source::OwnedSource>,
    child: Mutex<OwnedExecProcess>,
    reads: AtomicUsize,
    completed_execs: AtomicUsize,
}

#[cfg(feature = "dsh-test-probes")]
impl ProcessEnvironmentReader for ExecAfterFirstRead {
    fn identity_now(&self, pid: u32) -> Result<Option<ObservedProcessIdentity>, ReadFailure> {
        assert_eq!(pid, self.child.lock().unwrap().pid());
        self.real.identity_now(pid)
    }

    fn read_initial_environment_into(
        &self,
        pid: u32,
        output: &mut [u8],
    ) -> Result<ReadExtent, ReadFailure> {
        assert_eq!(pid, self.child.lock().unwrap().pid());
        let actual = self.real.read_initial_environment_into(pid, output);
        if self.reads.fetch_add(1, Ordering::SeqCst) == 0 {
            let extent = actual
                .as_ref()
                .expect("必须先读到注入的 A 帧，不能用读取失败冒充拒绝旧代");
            assert!(extent.bytes_written <= output.len());
            assert_eq!(extent.completeness, ReadCompleteness::Complete);
            let captured =
                parse_allowlisted_nul(&output[..extent.bytes_written], extent.completeness)
                    .expect("注入的 A 帧应可解析");
            assert_eq!(captured, expected_environment(HOME_A, DSH_A));
            self.child.lock().unwrap().exec_same_path();
            self.source.set_environment(HOME_B, Some(DSH_B));
            self.completed_execs.fetch_add(1, Ordering::SeqCst);
        }
        actual
    }
}

#[test]
fn a_stable_owned_image_can_still_produce_its_current_initial_environment() {
    // 正控：拒绝所有采样不能让同路径 exec 回归假通过。
    let mut child = OwnedExecProcess::spawn();
    let reader = MacOsProcessEnvironmentReader::default();
    let expected = identity(&reader, child.pid());
    assert_default_incomplete(&reader, &expected);
    #[cfg(feature = "dsh-test-probes")]
    {
        let source =
            std::sync::Arc::new(source::OwnedSource::new(child.pid(), HOME_A, Some(DSH_A)));
        let injected = MacOsProcessEnvironmentReader::with_source(source.clone());
        let observation = sample(&injected, &expected).expect("注入完整证据的稳定 A 应可采样");
        assert_eq!(
            observation.environment(),
            &expected_environment(HOME_A, DSH_A)
        );
        assert!(source.reads.load(Ordering::SeqCst) > 0);
    }
    child.finish();
}

#[test]
fn default_reader_stays_incomplete_across_same_path_exec() {
    let mut child = OwnedExecProcess::spawn();
    let reader = MacOsProcessEnvironmentReader::default();
    let before = identity(&reader, child.pid());
    assert_default_incomplete(&reader, &before);
    child.exec_same_path();
    let after = identity(&reader, child.pid());
    assert_same_process_new_exec(&before, &after);
    assert_default_incomplete(&reader, &after);
    child.finish();
}

#[cfg(feature = "dsh-test-probes")]
#[test]
fn same_pid_start_and_path_exec_after_read_cannot_publish_the_old_image_environment() {
    let child = OwnedExecProcess::spawn();
    let pid = child.pid();
    let source = std::sync::Arc::new(source::OwnedSource::new(pid, HOME_A, Some(DSH_A)));
    let real = MacOsProcessEnvironmentReader::with_source(source.clone());
    let before = identity(&real, pid);
    let positive = sample(&real, &before).expect("竞态前注入完整 A 证据必须可读");
    assert_eq!(positive.environment(), &expected_environment(HOME_A, DSH_A));

    let reader = ExecAfterFirstRead {
        real,
        source,
        child: Mutex::new(child),
        reads: AtomicUsize::new(0),
        completed_execs: AtomicUsize::new(0),
    };
    let raced = sample(&reader, &before);
    assert_eq!(
        reader.completed_execs.load(Ordering::SeqCst),
        1,
        "必须完成一次真正的同路径 exec"
    );
    let after = identity(&reader.real, pid);
    // 真 exec 前后同 PID、同秒、同路径；native 代际区分同一进程的两次映像。
    assert_same_process_new_exec(&before, &after);
    // 第二个正控：真 exec 完成后切换为 B 的合成帧，稳定 B 可重新采样。
    let current = sample(&reader.real, &after).expect("exec 完成后稳定 B 映像必须可重新采样");
    assert_eq!(current.environment(), &expected_environment(HOME_B, DSH_B));

    // 允许明确拒绝，或核验后重读成功；两条合法路径都不能发布先前捕获的 A 环境/Unset。
    match raced {
        Err(SampleEnvironmentError::IdentityChanged) => {}
        Ok(observation) => {
            assert_eq!(
                observation.environment(),
                &expected_environment(HOME_B, DSH_B),
                "真 exec 已完成，竞态采样只能拒绝或返回重新核验的 B 环境"
            );
            assert_eq!(observation.identity(), &after);
        }
        Err(other) => panic!("必须因身份换代拒绝，而非其他错误：{other:?}"),
    }
    reader.child.lock().unwrap().finish();
}

#[test]
#[ignore = "仅供本文件专属子进程 --exact 调用；不是产品覆盖用例"]
fn r01_owned_same_path_exec_fixture() {
    fixture::run_fixture();
}
