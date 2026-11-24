#![cfg(unix)]
//! 监听探测回归：只运行临时伪 lsof，不读取真实监听表或继承敏感环境。
//! 外层独立进程与进程组看门狗防止测试挂死，并清理继承 stdout 的后代。
//! Windows 的 netstat 需要等价的独立进程/Job Object 测试，不能由此 Unix 测试替代。

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use multi_agents_manager_lib::services::dsh_control::model::DshProcessSource;
use multi_agents_manager_lib::services::dsh_control::production::SysinfoProcessSource;
use wait_timeout::ChildExt;

const WATCHDOG: Duration = Duration::from_secs(25);
const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
const EXPECTED_BOUND: Duration = Duration::from_secs(4);
const WORKER_ENV: &str = "TUVIS_TEST_LISTENER_WORKER";

fn script(dir: &Path, contents: &str) {
    let path = dir.join("lsof");
    fs::write(&path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

// 每个用例启动独立测试进程；只向它传临时 PATH 和测试参数，不改全局环境。
// worker 与伪 lsof 同处独立进程组，超时后整体终止并回收 worker。
fn run_worker(dir: &Path, scenario: &str) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "listener_worker", "--nocapture"])
        .env_clear()
        .env("PATH", dir)
        .env(WORKER_ENV, scenario)
        .env("TUVIS_TEST_PID_FILE", dir.join("pid"))
        .env("TUVIS_TEST_DESCENDANTS", dir.join("descendants"))
        .env("TUVIS_TEST_OUTPUT", dir.join("output"))
        .env("TUVIS_TEST_EXIT_MARKER", dir.join("exited"))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()
        .unwrap();
    let result = child.wait_timeout(WATCHDOG);
    // 无论 worker 成功、断言失败或超时，都清理它启动的整组后代。
    unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
    // 探测实现可能给子进程创建新进程组，因此也单独清理夹具记录的后代。
    if let Ok(pids) = fs::read_to_string(dir.join("descendants")) {
        for pid in pids
            .lines()
            .filter_map(|line| line.parse::<libc::pid_t>().ok())
        {
            if pid > 1 {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    match result.unwrap() {
        Some(status) => assert!(status.success(), "listener worker failed: {status}"),
        None => {
            child.wait().unwrap();
            panic!("listener worker exceeded {WATCHDOG:?}");
        }
    }
}

#[test]
fn blocked_probe_is_terminated_and_reaped_within_bound() {
    let dir = tempfile::tempdir().unwrap();
    script(
        dir.path(),
        "#!/bin/sh\nprintf '%s' \"$$\" > \"$TUVIS_TEST_PID_FILE\"\nwhile :; do :; done\n",
    );
    run_worker(dir.path(), "blocked");
}

#[test]
fn output_larger_than_pipe_capacity_does_not_deadlock() {
    let dir = tempfile::tempdir().unwrap();
    // 两条管道各写入超过 1 MiB，再输出一条可解析记录，证明完整消费。
    script(
        dir.path(),
        "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 20000 ]; do\n  printf '%080d\\n' 0\n  printf '%080d\\n' 0 >&2\n  i=$((i + 1))\ndone\nprintf 'node 123 test 1 IPv4 0 0 TCP 127.0.0.1:5214\\n'\n",
    );
    run_worker(dir.path(), "large-output");
}

fn output_with_length(length: usize, second_listener: bool) -> Vec<u8> {
    let first = b"node 123 test 1 IPv4 0 0 TCP 127.0.0.1:5214\n";
    let second = b"node 123 test 2 IPv4 0 0 TCP 127.0.0.1:5215\n";
    let mut output = first.to_vec();
    let padding_end = if second_listener {
        length - second.len()
    } else {
        length
    };
    output.resize(padding_end - 1, b'x');
    output.push(b'\n');
    if second_listener {
        output.extend_from_slice(second);
    }
    output
}

#[test]
fn oversized_output_is_not_accepted_as_a_complete_listener_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("output"),
        output_with_length(OUTPUT_LIMIT + 100, true),
    )
    .unwrap();
    // cat 被提前关闭的管道打断时仍成功退出，不能借退出状态掩盖截断问题。
    script(dir.path(), "#!/bin/sh\n/bin/cat \"$TUVIS_TEST_OUTPUT\"\nprintf done > \"$TUVIS_TEST_EXIT_MARKER\"\nexit 0\n");
    run_worker(dir.path(), "oversized");
}

#[test]
fn output_at_the_limit_is_accepted_when_complete() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("output"),
        output_with_length(OUTPUT_LIMIT, false),
    )
    .unwrap();
    script(dir.path(), "#!/bin/sh\n/bin/cat \"$TUVIS_TEST_OUTPUT\"\nprintf done > \"$TUVIS_TEST_EXIT_MARKER\"\nexit 0\n");
    run_worker(dir.path(), "at-limit");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn current_thread_count() -> usize {
    #[cfg(target_os = "linux")]
    {
        fs::read_dir("/proc/self/task").unwrap().count()
    }
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_taskinfo>();
        let read = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDTASKINFO,
                0,
                (&mut info as *mut libc::proc_taskinfo).cast(),
                size as i32,
            )
        };
        assert_eq!(
            read, size as i32,
            "proc_pidinfo must return task information"
        );
        info.pti_threadnum as usize
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn inherited_stdout_does_not_leave_a_reader_thread_per_probe() {
    let dir = tempfile::tempdir().unwrap();
    // 后代记录 PID 后持有 stdout；直接子进程确认后代已启动再成功退出。
    script(dir.path(), "#!/bin/sh\n/bin/sh -c 'printf \"%s\\n\" \"$$\" >> \"$TUVIS_TEST_DESCENDANTS\"; printf %s \"$$\" > \"$TUVIS_TEST_PID_FILE\"; exec /bin/sleep 30' &\ni=0\nwhile [ ! -s \"$TUVIS_TEST_PID_FILE\" ]; do\n  i=$((i + 1)); [ \"$i\" -lt 100000 ] || exit 1\ndone\nexit 0\n");
    run_worker(dir.path(), "inherited-stdout");
}

#[test]
fn listener_worker() {
    let Ok(scenario) = std::env::var(WORKER_ENV) else {
        return;
    };
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if scenario == "inherited-stdout" {
        let baseline = current_thread_count();
        let pid_file = std::env::var_os("TUVIS_TEST_PID_FILE").unwrap();
        let source = SysinfoProcessSource::new();
        for _ in 0..4 {
            let _ = fs::remove_file(&pid_file);
            let started = Instant::now();
            let listeners = source.listening_sockets_fresh();
            assert!(
                started.elapsed() < EXPECTED_BOUND,
                "probe must return within its bound"
            );
            assert!(
                fs::metadata(&pid_file).is_ok(),
                "fixture descendant did not start"
            );
            assert!(
                listeners.is_empty(),
                "incomplete output must not become a snapshot"
            );
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while current_thread_count() > baseline + 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let remaining = current_thread_count();
        assert!(
            remaining <= baseline + 1,
            "reader threads accumulate across probes: baseline={baseline}, after={remaining}"
        );
        return;
    }
    let started = Instant::now();
    let listeners = SysinfoProcessSource::new().listening_sockets_fresh();
    assert!(
        started.elapsed() < EXPECTED_BOUND,
        "probe exceeded {EXPECTED_BOUND:?}"
    );
    match scenario.as_str() {
        "blocked" => {
            assert!(listeners.is_empty(), "timeout must return no listeners");
            let pid: libc::pid_t =
                fs::read_to_string(std::env::var_os("TUVIS_TEST_PID_FILE").unwrap())
                    .unwrap()
                    .parse()
                    .unwrap();
            // kill(pid, 0) 也能发现僵尸；waitpid 的 ECHILD 进一步确认已被收尸。
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                -1,
                "probe is still alive or a zombie"
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
            let mut status = 0;
            assert_eq!(
                unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }
        "large-output" => {
            assert!(
                listeners
                    .iter()
                    .any(|entry| entry.owner_pid == 123 && entry.authority == "127.0.0.1:5214"),
                "expected record after large output"
            );
        }
        "oversized" | "at-limit" => {
            assert!(
                Path::new(&std::env::var_os("TUVIS_TEST_EXIT_MARKER").unwrap()).exists(),
                "fake lsof did not reach successful exit"
            );
            if scenario == "oversized" {
                assert!(
                    listeners.is_empty(),
                    "truncated output must not publish the first listener: {listeners:?}"
                );
            } else {
                assert_eq!(
                    listeners.len(),
                    1,
                    "complete output at the limit must be accepted"
                );
                assert_eq!(listeners[0].owner_pid, 123);
                assert_eq!(listeners[0].authority, "127.0.0.1:5214");
            }
        }
        _ => panic!("unknown worker scenario"),
    }
}
