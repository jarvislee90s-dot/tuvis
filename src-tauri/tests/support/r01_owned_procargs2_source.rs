//! 仅供 dsh-test-probes：专属子 PID 的真实身份 + 夹具合成的 procargs2 帧。
//! FullFor 是测试输入，不证明 macOS 真能完整读取该子进程环境。
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use multi_agents_manager_lib::services::dsh_control::process_environment::{
    EnvironmentVisibility, MacOsProcargs2Source, MacOsProcessEnvironmentReader,
    ObservedProcessIdentity, Procargs2Observation, ProcessEnvironmentReader, ReadFailure,
};

pub struct OwnedSource {
    pid: u32,
    frame: Mutex<Vec<u8>>,
    pub reads: AtomicUsize,
}

impl OwnedSource {
    pub fn new(pid: u32, home: &str, dsh_home: Option<&str>) -> Self {
        Self {
            pid,
            frame: Mutex::new(frame(home, dsh_home)),
            reads: AtomicUsize::new(0),
        }
    }

    // 此支持模块由不同平台的测试各自编译，部分调用方不需要更改环境。
    #[allow(dead_code)]
    pub fn set_environment(&self, home: &str, dsh_home: Option<&str>) {
        *self.frame.lock().unwrap() = frame(home, dsh_home);
    }
}

fn frame(home: &str, dsh_home: Option<&str>) -> Vec<u8> {
    assert!(!home.as_bytes().contains(&0));
    assert!(!dsh_home.is_some_and(|s| s.as_bytes().contains(&0)));
    let mut bytes = 5i32.to_ne_bytes().to_vec();
    bytes.extend_from_slice(b"/synthetic/dsh\0");
    while !(bytes.len() - 4).is_multiple_of(8) {
        bytes.push(0);
    }
    bytes.extend_from_slice(
        b"synthetic-argv\0HOME=/synthetic/argv-decoy\0\0DSH_HOME=/synthetic/argv-decoy\0\0HOME=",
    );
    bytes.extend_from_slice(home.as_bytes());
    bytes.push(0);
    if let Some(value) = dsh_home {
        bytes.extend_from_slice(b"DSH_HOME=");
        bytes.extend_from_slice(value.as_bytes());
        bytes.push(0);
    }
    bytes.extend_from_slice(super::direct::SECRET_KEY.as_bytes());
    bytes.push(b'=');
    bytes.extend_from_slice(super::direct::SECRET_VALUE.as_bytes());
    bytes.push(0);
    bytes
}

impl MacOsProcargs2Source for OwnedSource {
    fn identity_now(&self, pid: u32) -> Result<Option<ObservedProcessIdentity>, ReadFailure> {
        assert_eq!(pid, self.pid, "只允许读取本测试专属子 PID");
        MacOsProcessEnvironmentReader::default().identity_now(pid)
    }

    fn read_procargs2_into(
        &self,
        pid: u32,
        output: &mut [u8],
    ) -> Result<Procargs2Observation, ReadFailure> {
        assert_eq!(pid, self.pid, "只允许读取本测试专属子 PID");
        let identity = self.identity_now(pid)?.ok_or(ReadFailure::Io)?;
        let frame = self.frame.lock().unwrap();
        assert!(frame.len() < output.len());
        output[..frame.len()].copy_from_slice(&frame);
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(Procargs2Observation {
            bytes_written: frame.len(),
            visibility: EnvironmentVisibility::FullFor(identity),
        })
    }
}
