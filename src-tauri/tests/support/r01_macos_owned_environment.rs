//! 只创建、保持并回收本测试专属 PID；不枚举进程，不读取父进程环境，不启动 DSH/模型。
//! stdout 握手替代固定 sleep；握手/退出有期限；任何断言失败均由 Drop kill + wait 回收。

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
#[cfg(feature = "dsh-test-probes")]
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::direct::{SECRET_KEY, SECRET_VALUE};

const WAIT: Duration = Duration::from_secs(5);

pub enum DshHome {
    Unset,
    Empty,
    Path,
}

pub struct OwnedEnvironmentProcess {
    child: Child,
    #[cfg(feature = "dsh-test-probes")]
    pub home: PathBuf,
    #[cfg(feature = "dsh-test-probes")]
    pub dsh_home: PathBuf,
    _directory: tempfile::TempDir,
}

impl OwnedEnvironmentProcess {
    pub fn spawn(dsh: DshHome) -> Self {
        // 显式指定系统临时根，连测试 support 也不调用 env::var/vars/home_dir/temp_dir。
        let directory = tempfile::Builder::new()
            .prefix("tuvis-r01-procargs2-")
            .tempdir_in("/private/tmp")
            .expect("无法创建隔离目录；属于 setup 阻塞");
        let home = directory.path().join("合成用户 home");
        let dsh_home = directory.path().join("合成 profile=a b");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&dsh_home).unwrap();
        let mut command = Command::new("/bin/sh");
        command
            .arg0("r01-owned-procargs2")
            .args([
                "-c",
                "printf 'r01-ready\\n'; IFS= read -r line",
                "HOME=/synthetic/argv-decoy",
                "",
                "DSH_HOME=/synthetic/argv-decoy",
                "",
            ])
            .env_clear()
            .env("HOME", &home)
            .env(SECRET_KEY, SECRET_VALUE)
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        match dsh {
            DshHome::Unset => {}
            DshHome::Empty => {
                command.env("DSH_HOME", "");
            }
            DshHome::Path => {
                command.env("DSH_HOME", &dsh_home);
            }
        }
        let child = command
            .spawn()
            .expect("专属子进程无法启动；属于 setup 阻塞");
        let mut owned = Self {
            child,
            #[cfg(feature = "dsh-test-probes")]
            home,
            #[cfg(feature = "dsh-test-probes")]
            dsh_home,
            _directory: directory,
        };
        let stdout = owned.child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout.take(128)).read_line(&mut line);
            let _ = sender.send(result.map(|_| line));
        });
        let line = receiver
            .recv_timeout(WAIT)
            .expect("专属子进程就绪超时；属于 setup 阻塞")
            .expect("专属子进程握手读取失败；属于 setup 阻塞");
        worker.join().expect("握手线程退出失败");
        assert!(line == "r01-ready\n", "握手错误；不回显子进程输出");
        assert!(
            owned.child.try_wait().unwrap().is_none(),
            "专属子进程在采样前退出；属于 setup 阻塞"
        );
        owned
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    #[cfg(feature = "dsh-test-probes")]
    pub fn expected_home(&self) -> &Path {
        &self.home
    }

    pub fn finish(mut self) {
        self.child
            .stdin
            .take()
            .unwrap()
            .write_all(b"finish\n")
            .expect("专属子进程输入管道已关闭");
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "专属子进程异常退出");
                return;
            }
            assert!(Instant::now() < deadline, "专属子进程未在期限内退出");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for OwnedEnvironmentProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
