//! R01 PID/start 直接接口测试；只替换底层 OS 观测，不注入采样结果或可信身份。
//! process_environment 接口尚不存在，编译阻塞不算行为 RED；直接用例不再 ignore。
//! 仍未接线的活体测试只允许 env_clear 后创建的专属子 PID，不枚举用户环境。
//! 没有 PID 复用实机声明；同 PID 换 start 由原始观测序列确定性覆盖。
#![cfg(unix)]

#[path = "support/r01_environment_direct.rs"]
mod direct;
#[path = "support/r01_environment_driver.rs"]
mod driver;

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Mutex};

use direct::{assert_logs_redacted, assert_redacted, capture_logs, SECRET_KEY, SECRET_VALUE};
use driver::{fields, value, Driver, WAIT};
use multi_agents_manager_lib::services::dsh_control::process_environment::{
    sample_initial_environment, AllowlistedEnvironment, EnvironmentField,
    InitialEnvironmentObservation, ObservedProcessIdentity, ProcessEnvironmentReader,
    ReadCompleteness, ReadExtent, ReadFailure, SampleEnvironmentError,
};
use serde_json::{json, Value};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, UpdateKind};

const SYNTHETIC_PID: u32 = 424242;

fn identity(start: i64) -> ObservedProcessIdentity {
    ObservedProcessIdentity {
        pid: SYNTHETIC_PID,
        started_at_ms: start,
        executable: PathBuf::from("/synthetic/dsh"),
        native_generation: None,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Call {
    Identity(u32),
    ReadEnvironment(u32),
}

/// 仅返回低层原始观测；身份比较、完整性处理和字节解析全部交给生产函数。
/// 返回类型合同：ReadExtent{bytes_written, completeness}；Observation 提供
/// identity()/environment() 只读访问器；SampleEnvironmentError 的读取错误为
/// EnvironmentReadFailed(ReadFailure)，不携原始字节或自由文本诊断。
struct ScriptedReader {
    identities: Mutex<VecDeque<Option<ObservedProcessIdentity>>>,
    read: Mutex<Option<Result<ReadCompleteness, ReadFailure>>>,
    calls: Mutex<Vec<Call>>,
}

impl ScriptedReader {
    fn new(
        identities: Vec<Option<ObservedProcessIdentity>>,
        read: Result<ReadCompleteness, ReadFailure>,
    ) -> Self {
        Self {
            identities: Mutex::new(identities.into()),
            read: Mutex::new(Some(read)),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn sample(&self) -> Result<InitialEnvironmentObservation, SampleEnvironmentError> {
        let (actual, logs) = capture_logs(|| sample_initial_environment(self, &identity(1000)));
        assert_redacted(&format!("{actual:?}"));
        assert_logs_redacted(&logs);
        actual
    }

    fn assert_read_was_bracketed(&self) {
        assert_eq!(
            *self.calls.lock().unwrap(),
            [
                Call::Identity(SYNTHETIC_PID),
                Call::ReadEnvironment(SYNTHETIC_PID),
                Call::Identity(SYNTHETIC_PID),
            ]
        );
    }
}

impl ProcessEnvironmentReader for ScriptedReader {
    fn identity_now(&self, pid: u32) -> Result<Option<ObservedProcessIdentity>, ReadFailure> {
        assert_eq!(pid, SYNTHETIC_PID, "不得读取未指定 PID");
        self.calls.lock().unwrap().push(Call::Identity(pid));
        Ok(self
            .identities
            .lock()
            .unwrap()
            .pop_front()
            .expect("额外身份读取：测试仅提供本次前后两次原始观测"))
    }

    fn read_initial_environment_into(
        &self,
        pid: u32,
        output: &mut [u8],
    ) -> Result<ReadExtent, ReadFailure> {
        assert_eq!(pid, SYNTHETIC_PID, "不得读取未指定 PID");
        self.calls.lock().unwrap().push(Call::ReadEnvironment(pid));
        // 前置身份不匹配的用例不配置读取结果，误读会在此失败。
        let read = self
            .read
            .lock()
            .unwrap()
            .take()
            .expect("不允许此次环境读取");
        let bytes = format!("{SECRET_KEY}={SECRET_VALUE}\0DSH_HOME=/tmp/allowed\0");
        assert!(
            output.len() >= bytes.len(),
            "生产缓冲区不足以容纳小型合成夹具"
        );
        output[..bytes.len()].copy_from_slice(bytes.as_bytes());
        // 失败路径也已经写入部分字节，确保生产不能回收利用失败读取的内容。
        read.map(|completeness| ReadExtent {
            bytes_written: bytes.len(),
            completeness,
        })
    }
}

#[test]
fn changed_start_before_environment_read_prevents_any_environment_access() {
    let reader = ScriptedReader {
        identities: Mutex::new([Some(identity(2000))].into()),
        read: Mutex::new(None),
        calls: Mutex::new(Vec::new()),
    };
    assert!(matches!(
        reader.sample(),
        Err(SampleEnvironmentError::IdentityChanged)
    ));
    assert_eq!(
        *reader.calls.lock().unwrap(),
        [Call::Identity(SYNTHETIC_PID)]
    );
}

#[test]
fn reused_pid_with_changed_start_after_read_discards_the_environment() {
    let reader = ScriptedReader::new(
        vec![Some(identity(1000)), Some(identity(2000))],
        Ok(ReadCompleteness::Complete),
    );
    assert!(matches!(
        reader.sample(),
        Err(SampleEnvironmentError::IdentityChanged)
    ));
    reader.assert_read_was_bracketed();
}

#[test]
fn changed_pid_after_environment_read_discards_the_environment() {
    let mut changed = identity(1000);
    changed.pid = 424243;
    let reader = ScriptedReader::new(
        vec![Some(identity(1000)), Some(changed)],
        Ok(ReadCompleteness::Complete),
    );
    assert!(matches!(
        reader.sample(),
        Err(SampleEnvironmentError::IdentityChanged)
    ));
    reader.assert_read_was_bracketed();
}

#[test]
fn missing_identity_after_environment_read_discards_the_environment() {
    let reader = ScriptedReader::new(
        vec![Some(identity(1000)), None],
        Ok(ReadCompleteness::Complete),
    );
    assert!(matches!(
        reader.sample(),
        Err(SampleEnvironmentError::ProcessMissing)
    ));
    reader.assert_read_was_bracketed();
}

#[test]
fn permission_failure_is_not_an_unset_key() {
    let reader = ScriptedReader::new(
        vec![Some(identity(1000)), Some(identity(1000))],
        Err(ReadFailure::PermissionDenied),
    );
    assert!(matches!(
        reader.sample(),
        Err(SampleEnvironmentError::EnvironmentReadFailed(
            ReadFailure::PermissionDenied
        ))
    ));
    reader.assert_read_was_bracketed();
}

#[test]
fn io_failure_cannot_salvage_partial_allowed_values() {
    let reader = ScriptedReader::new(
        vec![Some(identity(1000)), Some(identity(1000))],
        Err(ReadFailure::Io),
    );
    assert!(matches!(
        reader.sample(),
        Err(SampleEnvironmentError::EnvironmentReadFailed(
            ReadFailure::Io
        ))
    ));
    reader.assert_read_was_bracketed();
}

#[test]
fn matching_raw_identities_return_only_the_parsed_allowlist_observation() {
    let reader = ScriptedReader::new(
        vec![Some(identity(1000)), Some(identity(1000))],
        Ok(ReadCompleteness::Complete),
    );
    let observation = reader.sample().expect("同代原始观测应可产生初始环境样本");
    assert_eq!(observation.identity(), &identity(1000));
    assert_eq!(
        observation.environment(),
        &AllowlistedEnvironment {
            dsh_home: EnvironmentField::Value("/tmp/allowed".to_owned()),
            home: EnvironmentField::Unset,
        }
    );
    reader.assert_read_was_bracketed();
}

// 以下真实 OS 适配器/停点仍未接线；保留隔离与泄漏断言，不算有效覆盖。
fn trace_ops(frame: &Value) -> Vec<&str> {
    frame["trace"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| call["op"].as_str().expect("低层调用必须带 op"))
        .collect()
}

struct OwnedProcess {
    child: Child,
    identity: Value,
    home: PathBuf,
    user_home: PathBuf,
    _dir: tempfile::TempDir,
}

impl OwnedProcess {
    fn spawn() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let user_home = dir.path().join("隔离用户 home");
        let home = dir.path().join("中文 自定义 home");
        std::fs::create_dir(&user_home).unwrap();
        std::fs::create_dir(&home).unwrap();
        let user_home = std::fs::canonicalize(user_home).unwrap();
        let home = std::fs::canonicalize(home).unwrap();
        // 固定脚本文本不插值，不启动 DSH、模型或用户 shell 配置；环境只含合成值。
        let mut child = Command::new("/bin/sh")
            .args(["-c", "printf 'ready\\n'; IFS= read -r line"])
            .env_clear()
            .env("HOME", &user_home)
            .env("DSH_HOME", &home)
            .env(SECRET_KEY, SECRET_VALUE)
            .current_dir(&user_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("隔离子进程启动失败：不能记为行为 RED");
        let stdout = child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let read = BufReader::new(stdout).read_line(&mut line);
            let _ = sender.send((read.is_ok(), line));
        });
        // 先建 RAII 守卫，任何前置条件断言失败都必须回收子进程。
        let mut owned = Self {
            child,
            identity: Value::Null,
            home,
            user_home,
            _dir: dir,
        };
        let (ok, ready) = receiver.recv_timeout(WAIT).expect("隔离子进程就绪超时");
        assert!(ok && ready == "ready\n", "隔离子进程就绪协议失败");
        let pid = Pid::from_u32(owned.child.id());
        let mut system = sysinfo::System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
        );
        // 仅获取专属子 PID 的身份；从不启用 environ 刷新，也不调用 All。
        let process = system.process(pid).expect("子 PID 身份不可读");
        let executable = process.exe().expect("子进程 exe 不可读");
        let start = process.start_time() as i64 * 1000;
        assert!(start > 0, "子进程 start 不可读");
        owned.identity = json!({
            "pid": owned.child.id(), "startedAtMs": start, "executable": executable,
        });
        owned
    }

    fn sample(&self, pause: Option<&str>) -> Driver {
        Driver::start(json!({
            "op": "sample_owned", "ownedPid": self.child.id(),
            "expectedIdentity": self.identity,
            "pauseAt": pause,
        }))
    }

    fn stop(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn assert_only_owned_reads(frame: &Value, pid: u32) {
    assert_eq!(
        trace_ops(frame),
        ["identity", "read_environment", "identity"]
    );
    for call in frame["trace"].as_array().unwrap() {
        assert_eq!(call["pid"], pid, "只允许访问本测试创建的子 PID");
        if call["op"] == "identity" {
            assert_eq!(call["fresh"], true, "前后身份复核必须绕过列表缓存");
        }
    }
}

#[test]
#[ignore = "未接线：真实 OS 读取器与专属子 PID；底层序列用例已直接调用生产采样接口"]
fn stable_owned_process_returns_allowed_fields_after_fresh_identity_checks() {
    let owned = OwnedProcess::spawn();
    let frame = owned.sample(None).finish();
    let mut expected = fields(
        value(owned.home.to_str().unwrap()),
        value(owned.user_home.to_str().unwrap()),
    );
    expected["identity"] = owned.identity.clone();
    assert_eq!(frame["result"], expected);
    assert_only_owned_reads(&frame, owned.child.id());
}

#[test]
#[ignore = "未接线：真实 OS 读取器前停点与专属子 PID；不得用合成序列冒充实机覆盖"]
fn owned_process_exit_before_environment_read_is_not_reported_as_unset() {
    let mut owned = OwnedProcess::spawn();
    let mut driver = owned.sample(Some("before_read"));
    driver.event("before_read");
    owned.stop();
    driver.resume();
    let frame = driver.finish();
    assert_eq!(
        frame["result"],
        json!({ "status": "stale", "reason": "process_missing" })
    );
    assert_only_owned_reads(&frame, owned.child.id());
}

#[test]
#[ignore = "未接线：真实 OS 读取器后停点与专属子 PID；不得用合成序列冒充实机覆盖"]
fn owned_process_exit_after_environment_read_discards_the_captured_fields() {
    let mut owned = OwnedProcess::spawn();
    let mut driver = owned.sample(Some("after_read"));
    driver.event("after_read");
    owned.stop();
    driver.resume();
    let frame = driver.finish();
    assert_eq!(
        frame["result"],
        json!({ "status": "stale", "reason": "process_missing" })
    );
    assert_only_owned_reads(&frame, owned.child.id());
}
