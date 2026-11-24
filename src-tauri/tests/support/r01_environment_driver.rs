//! R01 测试接缝合同；这里不实现环境解析，也不调用全量进程发现。
//!
//! 待提供 `TUVIS_R01_ENV_TEST_DRIVER` 指向的绝对可执行文件：逐行 JSON stdin/stdout，
//! 请求带 `protocol: 1`，只调用真实生产解析器/采样编排，不能另写参考实现。
//! `parse` 使用注入的 read{status,bytes}；完整性必须由读取层声明，不能仅看末尾 NUL。
//! `resolve_initial_home` 还接收目标 cwd、已独立取得的 userHome；产物只是初始环境证据。
//! `sample` 的 identityReads 是低层身份源的确定性序列，不是预置编排结果。
//! `sample_owned` 必须只读取 ownedPid，身份读取绕过列表 TTL；before_read/after_read
//! 两个停点输出同名 event，收到 {event:"continue"} 才继续，便于有界地退出子进程。
//! 最终帧 {event:"result",result,observations:{dto,logs,errors},trace}：observations
//! 必须来自真实 DTO 投影、日志捕获和错误转换，不能返回占位空集合冒充覆盖。
//! trace 是实际低层调用顺序；环境读取事件命名 read_environment，身份为 identity。
//! 任一原始环境 canary 出现在 stdout、stderr、DTO、日志或错误中均失败。
//! 本接缝尚不存在，因此调用方均显式 ignore；解除 ignore 前须实现并验收该驱动。

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub const WAIT: Duration = Duration::from_secs(5);
pub const SECRET_KEY: &str = "R01_PRIVATE_CANARY";
pub const SECRET_VALUE: &str = "synthetic-r01-secret-never-export-7c29";
const OUTPUT_LIMIT: u64 = 256 * 1024;

pub struct Driver {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    stderr: Receiver<Vec<u8>>,
}

impl Driver {
    pub fn start(mut request: Value) -> Self {
        let executable = std::env::var_os("TUVIS_R01_ENV_TEST_DRIVER")
            .expect("接线阻塞：须提供生产 R01 测试驱动，不可记为行为 RED");
        assert!(
            Path::new(&executable).is_absolute(),
            "测试驱动必须为绝对路径"
        );
        let mut child = Command::new(executable)
            .env_clear()
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("接线阻塞：生产测试驱动无法启动");
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (send, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout.take(OUTPUT_LIMIT)).lines() {
                let Ok(line) = line else { break };
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        let (stderr_sender, stderr_receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let read = stderr.take(OUTPUT_LIMIT).read_to_end(&mut bytes);
            if read.is_ok() {
                let _ = stderr_sender.send(bytes);
            }
        });
        let stdin = child.stdin.take();
        let mut driver = Self {
            child,
            stdin,
            lines,
            stderr: stderr_receiver,
        };
        request["protocol"] = json!(1);
        driver.send(&request);
        driver
    }

    fn send(&mut self, request: &Value) {
        let input = self.stdin.as_mut().unwrap();
        serde_json::to_writer(&mut *input, request).expect("测试请求编码失败");
        input.write_all(b"\n").unwrap();
        input.flush().unwrap();
    }

    pub fn event(&self, expected: &str) -> Value {
        let line = self
            .lines
            .recv_timeout(WAIT)
            .expect("驱动超时或提前退出：不是预期行为 RED");
        assert_redacted(line.as_bytes());
        let frame: Value = serde_json::from_str(&line)
            .unwrap_or_else(|_| panic!("驱动输出非 JSON：禁止打印原文以免扩大泄漏"));
        assert_frame_redacted(&frame);
        assert_eq!(frame["event"], expected, "驱动停点顺序错误");
        frame
    }

    /// 继续驱动到下一个停点。部分用例只走到某个断点即断言，故不总是调用。
    #[allow(dead_code)]
    pub fn resume(&mut self) {
        self.send(&json!({ "event": "continue" }));
    }

    pub fn finish(mut self) -> Value {
        let frame = self.event("result");
        let observations = frame["observations"]
            .as_object()
            .expect("必须捕获生产 DTO/日志/错误观察面");
        for field in ["dto", "logs", "errors"] {
            assert!(observations.contains_key(field), "缺少观察面 {field}");
        }
        assert!(frame["trace"].is_array(), "必须提供实际调用轨迹");
        self.stdin.take();
        let deadline = Instant::now() + WAIT;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "驱动未在有界时间内退出");
            std::thread::sleep(Duration::from_millis(5));
        };
        let stderr = self
            .stderr
            .recv_timeout(WAIT)
            .expect("驱动 stderr 未关闭或读取失败");
        assert!(
            stderr.len() < OUTPUT_LIMIT as usize,
            "驱动 stderr 超出捕获预算"
        );
        assert_redacted(&stderr);
        match self.lines.recv_timeout(WAIT) {
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => panic!("驱动 stdout 未关闭"),
            Ok(line) => {
                assert_redacted(line.as_bytes());
                panic!("result 之后不得继续输出未消费帧");
            }
        }
        assert!(status.success(), "驱动执行失败：不属于预期行为 RED");
        frame
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn assert_redacted(bytes: &[u8]) {
    for canary in [SECRET_KEY, SECRET_VALUE] {
        assert!(
            !bytes
                .windows(canary.len())
                .any(|window| window == canary.as_bytes()),
            "原始环境 canary 泄漏；禁止把实际内容放进断言错误"
        );
    }
}

fn assert_frame_redacted(frame: &Value) {
    // 解码后再次检查，避免 JSON Unicode 转义绕过原始 stdout 字节检查。
    assert_redacted(&serde_json::to_vec(frame).unwrap());
    match frame {
        Value::Array(items) => {
            // 原始环境也可能被错误序列化为字节数组，而非字符串。
            let bytes: Option<Vec<u8>> = items
                .iter()
                .map(|item| item.as_u64().and_then(|n| u8::try_from(n).ok()))
                .collect();
            if let Some(bytes) = bytes {
                assert_redacted(&bytes);
            }
            for item in items {
                assert_frame_redacted(item);
            }
        }
        Value::Object(fields) => {
            for value in fields.values() {
                assert_frame_redacted(value);
            }
        }
        _ => {}
    }
}

#[allow(dead_code)] // 供未在本文件使用的场景组合复用（夹具公共 helper）
pub fn complete(bytes: impl AsRef<[u8]>) -> Value {
    json!({ "status": "complete", "bytes": bytes.as_ref() })
}

#[allow(dead_code)] // 供未在本文件使用的场景组合复用（夹具公共 helper）
pub fn parse(read: Value) -> Value {
    Driver::start(json!({ "op": "parse", "read": read })).finish()
}

pub fn fields(dsh_home: Value, home: Value) -> Value {
    json!({ "status": "complete", "fields": { "DSH_HOME": dsh_home, "HOME": home } })
}

#[allow(dead_code)] // 供未在本文件使用的场景组合复用（夹具公共 helper）
pub fn unset() -> Value {
    json!({ "state": "unset" })
}

pub fn value(text: &str) -> Value {
    json!({ "state": "value", "value": text })
}
