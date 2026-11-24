//! 独立同路径 exec 夹具：只操作本测试 env_clear 后创建的 PID，不枚举进程或读取用户环境。
//! 子进程重用同一测试二进制中的 ignored 入口；阶段由受控 argv 的 --skip 标记传递。
//! exec 前后 HOME/DSH_HOME 都由这里显式提供；无 shell、无外部编译器、无磁盘配置读取。

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

pub const ENTRYPOINT: &str = "r01_owned_same_path_exec_fixture";
pub const HOME_A: &str = "/synthetic/r01-exec/用户 A";
pub const DSH_A: &str = "/synthetic/r01-exec/profile A";
pub const HOME_B: &str = "/synthetic/r01-exec/用户 B";
pub const DSH_B: &str = "/synthetic/r01-exec/profile B";
const WAIT: Duration = Duration::from_secs(5);
const OUTPUT_BUDGET: u64 = 16 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    A,
    B,
}

impl Phase {
    fn argument(self) -> &'static str {
        match self {
            Self::A => "r01-owned-exec-phase-a",
            Self::B => "r01-owned-exec-phase-b",
        }
    }

    fn ready(self) -> &'static str {
        match self {
            Self::A => "R01_OWNED_EXEC_A_READY",
            Self::B => "R01_OWNED_EXEC_B_READY",
        }
    }
}

fn command(phase: Phase) -> Command {
    // current_exe 只取得本测试二进制路径；不从用户环境寻找可执行文件。
    let mut command = Command::new(std::env::current_exe().expect("无法定位当前测试二进制"));
    command
        .args([
            "--exact",
            ENTRYPOINT,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
            "--skip",
            phase.argument(),
        ])
        .env_clear()
        .env("HOME", if phase == Phase::A { HOME_A } else { HOME_B })
        .env("DSH_HOME", if phase == Phase::A { DSH_A } else { DSH_B })
        .current_dir("/private/tmp");
    command
}

/// 仅由独立测试文件的 ignored fixture 入口调用；普通测试运行不会进入这里。
pub fn run_fixture() {
    let arguments: Vec<_> = std::env::args_os().collect();
    let has_marker = |phase: Phase| {
        arguments
            .windows(2)
            .any(|pair| pair[0] == "--skip" && pair[1] == phase.argument())
    };
    assert!(
        has_marker(Phase::A) != has_marker(Phase::B),
        "夹具必须由父测试携带唯一阶段标记启动"
    );
    let phase = if has_marker(Phase::A) {
        Phase::A
    } else {
        Phase::B
    };
    let mut stdout = std::io::stdout();
    // 前置换行使握手不会粘在 libtest 的 test-name 前缀上。
    writeln!(stdout, "\n{}", phase.ready()).unwrap();
    stdout.flush().unwrap();
    let stdin = std::io::stdin();
    let mut input = BufReader::new(stdin.lock().take(128));
    // 每个分支都终结进程（exec 替换映像 / return），因此这是一次读取而非循环。
    {
        let mut line = String::new();
        assert!(
            input.read_line(&mut line).unwrap() > 0,
            "父测试提前关闭输入"
        );
        match line.as_str() {
            "exec\n" if phase == Phase::A => {
                // 真正替换当前进程映像，而非 spawn 新 PID 或用 set_var 改内存环境。
                let error = command(Phase::B)
                    .stdin(Stdio::inherit())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit())
                    .exec();
                panic!("同路径 exec 失败：{error}");
            }
            "finish\n" => {}
            _ => panic!("夹具收到非预期命令；不回显输入"),
        }
    }
}

pub struct OwnedExecProcess {
    child: Child,
    lines: Receiver<Result<String, std::io::Error>>,
    phase: Phase,
}

impl OwnedExecProcess {
    pub fn spawn() -> Self {
        let mut child = command(Phase::A)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("隔离夹具启动失败；属于 setup 阻塞");
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout.take(OUTPUT_BUDGET)).lines() {
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        // 握手断言之前就取得 RAII 所有权，失败时也会 kill + wait。
        let owned = Self {
            child,
            lines,
            phase: Phase::A,
        };
        owned.await_ready(Phase::A);
        owned
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    fn send(&mut self, bytes: &[u8]) {
        let input = self.child.stdin.as_mut().unwrap();
        input.write_all(bytes).expect("夹具输入管道已关闭");
        input.flush().unwrap();
    }

    fn await_ready(&self, phase: Phase) {
        let deadline = Instant::now() + WAIT;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("夹具握手超时；属于 setup 阻塞");
            let line = self
                .lines
                .recv_timeout(remaining)
                .expect("夹具握手超时或进程退出；属于 setup 阻塞")
                .expect("夹具 stdout 读取失败；属于 setup 阻塞");
            if line == phase.ready() {
                return;
            }
            // 只忽略 libtest 的包装输出，不把原文放入日志或失败详情。
        }
    }

    pub fn exec_same_path(&mut self) {
        assert!(self.phase == Phase::A, "只允许一次 A→B 真 exec");
        self.send(b"exec\n");
        self.await_ready(Phase::B);
        self.phase = Phase::B;
        assert!(self.child.try_wait().unwrap().is_none(), "B 阶段已提前退出");
    }

    pub fn finish(&mut self) {
        self.send(b"finish\n");
        self.child.stdin.take();
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "夹具未正常退出");
                return;
            }
            assert!(Instant::now() < deadline, "夹具退出超时");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for OwnedExecProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
