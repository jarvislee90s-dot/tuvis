//! H08 header-only source 回归：只调用公开 collect_with，不访问私有缓存。
//! 每例独立进程、home 和缓存；归档脱敏夹具只读，边界样本仅写入临时目录。
//! 当前能力只包括受预算约束的头部发现与父子关系，状态统一为中性 Unknown。
//! 不用 Unknown 证明回合/生命周期、转写、token 或完整状态能力；不验证 facts 连续性。
//!
//! 路径故障先通过真实 source 的 header I/O 正控，再改变 mtime 并显式 arm，
//! 在 regular-file 校验之后、实际 header openat 之前替换，不能靠未命中探针假绿。
//! FIFO/监督正控 2s、普通 worker 10s、探针编译 20s；超时终止进程组并 wait 回收。
//! 输出用普通文件，避免继承 pipe 把清理再次拖住。计数只包含返回数据的真实读取。
//! 根目录以实体 inode 计量替换读取；无关会话变化用双 header 栅栏，不依赖枚举顺序。
//! 缓存失效不能抹掉重复 ID 的歧义；重读后变更在新快照返回前的真实 fstat 边界注入。
//! 此目标仅在 macOS 执行；Windows 非 Unix source 目前未实现，空列表不能解释为无子任务。
//! zstd 窗口研究以分配请求观测，需独立校准；8MiB 为待评审的 header 单次分配预算。
//! 探针构建/触发/校准失败属于测试环境错误，不当作 source 行为 RED。
#![cfg(target_os = "macos")]

use multi_agents_manager_lib::monitor::subagents::dsh::collect_with;
use multi_agents_manager_lib::monitor::subagents::SubagentView;
use serde_json::{json, Value};
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{symlink, FileTypeExt, MetadataExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

const PARENT: &str = "session-<parent-id>";
const CHILD: &str = "<child-id>";
const FOREIGN: &str = "session-<unrelated-parent-id>";
const OTHER: &str = "session-<others-id>";
const LARGE: usize = 512 * 1024;
const HEADER_BUDGET: u64 = 64 * 1024;
// 明确约束实际输入读取；不能仅限解压后输出而无界吞掉 skippable frame。
const HEADER_INPUT_LIMIT: u64 = HEADER_BUDGET + 1;
const CHILD_FIXTURE: &str =
    include_str!("fixtures/dsh-native/0.2.1-rc.5/macos/hybrid/web-subagent-child-sanitized.json");
const PARENT_FIXTURE: &str =
    include_str!("fixtures/dsh-native/0.2.1-rc.5/macos/hybrid/web-subagent-parent-sanitized.json");
const FORK_FIXTURE: &str =
    include_str!("fixtures/dsh-native/0.2.1-rc.5/macos/hybrid/web-fork-sanitized.json");

struct BoundedRun {
    output: Output,
    timed_out: bool,
    elapsed: Duration,
    pid: u32,
}

fn run_bounded(command: &mut Command, timeout: Duration) -> BoundedRun {
    let capture = tempfile::tempdir().unwrap();
    let stdout = capture.path().join("stdout");
    let stderr = capture.path().join("stderr");
    command
        .process_group(0)
        .stdout(Stdio::from(File::create(&stdout).unwrap()))
        .stderr(Stdio::from(File::create(&stderr).unwrap()));
    let started = Instant::now();
    let mut child = command.spawn().expect("无法启动测试进程；环境错误不算 RED");
    let pid = child.id();
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break (status, false);
        }
        if started.elapsed() >= timeout {
            let killed = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            assert!(
                killed == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
                "无法终止超时测试进程组"
            );
            break (child.wait().unwrap(), true);
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    BoundedRun {
        output: Output {
            status,
            stdout: fs::read(stdout).unwrap(),
            stderr: fs::read(stderr).unwrap(),
        },
        timed_out,
        elapsed: started.elapsed(),
        pid,
    }
}

struct ProbeLibrary {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn probe_library() -> &'static Path {
    static PROBE: OnceLock<ProbeLibrary> = OnceLock::new();
    &PROBE
        .get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("libh08_source_io.dylib");
            let mut compiler = Command::new("cc");
            compiler
                .args(["-dynamiclib", "-O2", "-Wall", "-Wextra", "-o"])
                .arg(&path)
                .arg(
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/support/dsh_source_io_probe.c"),
                );
            let build = run_bounded(&mut compiler, Duration::from_secs(20));
            assert!(!build.timed_out, "探针编译超时；环境错误不算 RED");
            assert!(
                build.output.status.success(),
                "探针构建失败，不算 RED: {}",
                String::from_utf8_lossy(&build.output.stderr)
            );
            ProbeLibrary { _dir: dir, path }
        })
        .path
}

fn isolated(case: &str) {
    let directory = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(directory.path()).unwrap();
    let target = root.join("dsh-home/sessions/project/child/session.v4.jsonl");
    let parent = root.join("dsh-home/sessions/project/parent/session.v4.jsonl");
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "source_io_worker",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("DYLD_INSERT_LIBRARIES", probe_library())
        .env("H08_IO_CASE", case)
        .env("H08_IO_ROOT", &root)
        .env("H08_IO_TRACE", root.join("read.trace"))
        .env("H08_IO_TARGET", &target)
        .env("H08_IO_ARM_FILE", root.join("probe.armed"))
        .env_remove("H08_IO_REPLACE_MODE")
        .env_remove("H08_IO_REPLACEMENT")
        .env_remove("H08_IO_DESTINATION")
        .env_remove("H08_IO_PARKED")
        .env_remove("H08_IO_PARENT_TARGET")
        .env_remove("H08_IO_SESSIONS_ROOT")
        .env_remove("H08_IO_ALLOCATION_TARGET");
    match case {
        "generation_during_header" => {
            command
                .env("H08_IO_REPLACE_MODE", "generation")
                .env("H08_IO_REPLACEMENT", root.join("replacement-stage.jsonl"))
                .env(
                    "H08_IO_DESTINATION",
                    target.with_file_name("session.v5.jsonl"),
                );
        }
        "same_path_after_header" => {
            command
                .env("H08_IO_REPLACE_MODE", "file_after_header")
                .env("H08_IO_REPLACEMENT", root.join("replacement-stage.jsonl"))
                .env("H08_IO_DESTINATION", &target);
        }
        "detail_single_snapshot_swap" => {
            command
                .env("H08_IO_REPLACE_MODE", "detail_after_read_replace")
                .env("H08_IO_REPLACEMENT", root.join("detail-stage.jsonl"));
        }
        "leaf_inplace_after_data" => {
            command
                .env("H08_IO_REPLACE_MODE", "inplace_after_data")
                .env("H08_IO_REPLACEMENT", root.join("inplace-stage.jsonl"));
        }
        "leaf_link_before_header" => {
            command
                .env("H08_IO_REPLACE_MODE", "link")
                .env("H08_IO_REPLACEMENT", root.join("outside/session.v4.jsonl"))
                .env("H08_IO_DESTINATION", &target);
        }
        "ancestor_before_component" | "ancestor_before_leaf" => {
            command
                .env(
                    "H08_IO_REPLACE_MODE",
                    if case == "ancestor_before_component" {
                        "ancestor"
                    } else {
                        "ancestor_leaf"
                    },
                )
                .env("H08_IO_REPLACEMENT", root.join("outside"))
                .env("H08_IO_DESTINATION", target.parent().unwrap())
                .env("H08_IO_PARKED", root.join("parked-child"));
        }
        "header_openat_positive" => {
            command.env("H08_IO_REPLACE_MODE", "observe_openat");
        }
        "fifo_before_header" => {
            command
                .env("H08_IO_REPLACE_MODE", "fifo")
                .env("H08_IO_DESTINATION", &target);
        }
        "parent_before_header_open" => {
            command
                .env("H08_IO_REPLACE_MODE", "parent_before_leaf")
                .env("H08_IO_PARENT_TARGET", &parent)
                .env("H08_IO_REPLACEMENT", root.join("parent-stage.jsonl"))
                .env(
                    "H08_IO_DESTINATION",
                    parent.with_file_name("session.v5.jsonl"),
                );
        }
        "parent_generation_during_headers" | "parent_chain_positive" => {
            command
                .env(
                    "H08_IO_REPLACE_MODE",
                    if case == "parent_chain_positive" {
                        "observe_parent_chain"
                    } else {
                        "parent_generation"
                    },
                )
                .env("H08_IO_PARENT_TARGET", &parent)
                .env("H08_IO_REPLACEMENT", root.join("parent-stage.jsonl"))
                .env(
                    "H08_IO_DESTINATION",
                    parent.with_file_name("session.v5.jsonl"),
                );
        }
        "parent_reread_generation" | "parent_reread_ctime" | "parent_reread_positive" => {
            command
                .env(
                    "H08_IO_REPLACE_MODE",
                    match case {
                        "parent_reread_generation" => "reread_parent_generation",
                        "parent_reread_ctime" => "reread_parent_ctime",
                        _ => "observe_parent_reread",
                    },
                )
                .env("H08_IO_PARENT_TARGET", &parent)
                .env("H08_IO_REPLACEMENT", root.join("reread-parent-stage.jsonl"))
                .env(
                    "H08_IO_DESTINATION",
                    parent.with_file_name("session.v5.jsonl"),
                );
        }
        "sessions_real_directory_swap" | "sessions_real_directory_positive" => {
            command
                .env(
                    "H08_IO_REPLACE_MODE",
                    if case == "sessions_real_directory_swap" {
                        "sessions_directory"
                    } else {
                        "observe_sessions_directory"
                    },
                )
                .env("H08_IO_SESSIONS_ROOT", root.join("dsh-home/sessions"))
                .env("H08_IO_REPLACEMENT", root.join("replacement-sessions"))
                .env("H08_IO_DESTINATION", root.join("dsh-home/sessions"))
                .env("H08_IO_PARKED", root.join("parked-sessions"));
        }
        "membership_swapped_before_publish" => {
            command
                .env("H08_IO_REPLACE_MODE", "membership_swap")
                .env("H08_IO_SESSIONS_ROOT", root.join("dsh-home/sessions"))
                .env("H08_IO_REPLACEMENT", root.join("swap-session"))
                .env(
                    "H08_IO_DESTINATION",
                    root.join("dsh-home/sessions/project/foreign"),
                )
                .env("H08_IO_PARKED", root.join("parked-foreign"));
        }
        "membership_added_before_publish" => {
            command
                .env("H08_IO_REPLACE_MODE", "membership_add")
                .env("H08_IO_SESSIONS_ROOT", root.join("dsh-home/sessions"))
                .env("H08_IO_REPLACEMENT", root.join("late-session"))
                .env(
                    "H08_IO_DESTINATION",
                    root.join("dsh-home/sessions/project/late"),
                );
        }
        "unrelated_body_append_during_scan" | "unrelated_body_append_before_scan" => {
            command
                .env(
                    "H08_IO_REPLACE_MODE",
                    if case == "unrelated_body_append_during_scan" {
                        "unrelated_append"
                    } else {
                        "observe_unrelated_append"
                    },
                )
                .env(
                    "H08_IO_PARENT_TARGET",
                    root.join("dsh-home/sessions/project/foreign/session.v4.jsonl"),
                );
        }
        "zstd_window_small" | "zstd_window_large" | "allocation_observer_positive" => {
            command.env(
                "H08_IO_ALLOCATION_TARGET",
                target.with_file_name("session.v4.jsonl.zstd"),
            );
        }
        _ => {}
    }
    let timeout = Duration::from_secs(
        if matches!(
            case,
            "fifo_before_header" | "preexisting_fifo" | "deadline_control"
        ) {
            2
        } else {
            10
        },
    );
    let run = run_bounded(&mut command, timeout);
    let output = run.output;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!(
        "case={case} timeout_ms={} elapsed_ms={} timed_out={} pid={}\n{stdout}",
        timeout.as_millis(),
        run.elapsed.as_millis(),
        run.timed_out,
        run.pid
    );
    if case == "deadline_control" {
        assert!(run.timed_out);
        assert!(stdout.contains("DELIBERATE_FIFO_READY"));
        assert_eq!(output.status.signal(), Some(libc::SIGKILL));
        assert!(run.elapsed < timeout + Duration::from_secs(1));
        return;
    }
    if case == "fifo_before_header" && run.timed_out {
        require_swap(&root);
        assert!(fs::symlink_metadata(&target).unwrap().file_type().is_fifo());
        assert_eq!(trace_values(&root, "OPENAT_LEAF_BEFORE", &target).len(), 1);
        assert_eq!(trace_values(&root, "FIFO_CREATED", &target).len(), 1);
        assert!(trace_values(&root, "OPENAT_RESULT", &target).is_empty());
        assert_eq!(output.status.signal(), Some(libc::SIGKILL));
        let gone = unsafe { libc::kill(run.pid as i32, 0) };
        assert!(gone < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH));
        println!("HEADER_FIFO swap=true writers=0 openat_returned=false worker_reaped=true");
    }
    assert!(
        !run.timed_out,
        "SOURCE_DEADLINE_EXCEEDED: {case} 超过 {} ms，已回收进程组\n{stdout}\n{stderr}",
        timeout.as_millis()
    );
    assert!(
        output.status.success(),
        "真实 header source 场景 {case} 失败\n{stdout}\n{stderr}"
    );
}

fn fixture_header(raw: &str) -> Value {
    let mut header = serde_json::from_str::<Value>(raw).unwrap()["header"].clone();
    if matches!(
        header["createdAt"].as_str(),
        Some("<epoch-ms>" | "<redacted>")
    ) {
        header.as_object_mut().unwrap().remove("createdAt");
    }
    header
}

fn log_text(header: &Value, events: &[Value], payload_bytes: usize) -> String {
    let mut text = serde_json::to_string(header).unwrap() + "\n";
    for event in events {
        text.push_str(&(serde_json::to_string(event).unwrap() + "\n"));
    }
    if payload_bytes > 0 {
        text.push_str(
            &(serde_json::to_string(
                &json!({"type":"probe/padding","data":{"text":"x".repeat(payload_bytes)}}),
            )
            .unwrap()
                + "\n"),
        );
    }
    text
}

struct SourceFixture {
    root: PathBuf,
    home: PathBuf,
    log: PathBuf,
}

impl SourceFixture {
    fn new(root: PathBuf) -> Self {
        let home = root.join("dsh-home");
        let log = home.join("sessions/project/child/session.v4.jsonl");
        let fixture = Self { root, home, log };
        fixture.write("parent", 4, &fixture_header(PARENT_FIXTURE), &[], 0);
        let mut foreign = fixture_header(PARENT_FIXTURE);
        foreign["id"] = json!(FOREIGN);
        fixture.write("foreign", 4, &foreign, &[], 0);
        fixture
    }
    fn write(
        &self,
        name: &str,
        generation: u32,
        header: &Value,
        events: &[Value],
        payload_bytes: usize,
    ) -> PathBuf {
        let dir = self.home.join("sessions/project").join(name);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("session.v{generation}.jsonl"));
        fs::write(&path, log_text(header, events, payload_bytes)).unwrap();
        path
    }
    fn child(&self, events: &[Value], payload_bytes: usize) {
        self.write(
            "child",
            4,
            &fixture_header(CHILD_FIXTURE),
            events,
            payload_bytes,
        );
    }
    fn collect(&self, parent: &str) -> Vec<SubagentView> {
        collect_with(&self.home, parent)
    }
    fn set_modified(&self, value: SystemTime) {
        File::options()
            .write(true)
            .open(&self.log)
            .unwrap()
            .set_times(FileTimes::new().set_modified(value))
            .unwrap();
    }
    fn warm_and_arm_header(&self) -> Reads {
        assert_neutral(&self.collect(PARENT));
        let before = require_header_io(&self.root, &self.log, "header-positive-before-arm");
        let modified = fs::metadata(&self.log).unwrap().modified().unwrap();
        self.set_modified(modified - Duration::from_secs(1));
        fs::write(self.root.join("probe.armed"), b"armed").unwrap();
        before
    }
    fn outside(&self) -> PathBuf {
        let path = self.root.join("outside/session.v4.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut header = fixture_header(CHILD_FIXTURE);
        header["id"] = json!("<outside-child-id>");
        header["parentSession"] = json!(FOREIGN);
        fs::write(&path, log_text(&header, &[], LARGE)).unwrap();
        path
    }
}

#[derive(Default, Clone, Copy, Debug)]
struct Reads {
    calls: u64,
    bytes: u64,
}

fn read_count(root: &Path, accept: impl Fn(&Path) -> bool) -> Reads {
    let mut count = Reads::default();
    for line in fs::read_to_string(root.join("read.trace"))
        .unwrap_or_default()
        .lines()
    {
        let fields: Vec<_> = line.splitn(3, '\t').collect();
        if fields.len() == 3 && fields[0] == "READ" && accept(Path::new(fields[2])) {
            count.calls += 1;
            count.bytes += fields[1].parse::<u64>().unwrap();
        }
    }
    count
}
fn reads(root: &Path, path: &Path) -> Reads {
    read_count(root, |p| p == path)
}
fn reads_under(root: &Path, directory: &Path) -> Reads {
    read_count(root, |p| p.starts_with(directory))
}
fn observed(root: &Path, path: &Path, phase: &str) -> Reads {
    let count = reads(root, path);
    println!(
        "HEADER_IO phase={phase} data_reads={} bytes={} file={}",
        count.calls,
        count.bytes,
        path.display()
    );
    count
}
fn require_header_io(root: &Path, path: &Path, phase: &str) -> Reads {
    let count = observed(root, path, phase);
    assert!(
        count.calls > 0 && count.bytes > 0,
        "探针必须捕获真实 header 读取；未命中不算通过"
    );
    assert!(
        count.bytes <= HEADER_INPUT_LIMIT,
        "header 首次实际读取必须有界: {count:?}"
    );
    count
}
fn inode_read_count(root: &Path, path: &Path, inode: u64) -> Reads {
    let identity = format!("{inode}:{}", path.display());
    let mut count = Reads::default();
    for line in fs::read_to_string(root.join("read.trace"))
        .unwrap_or_default()
        .lines()
    {
        let fields: Vec<_> = line.splitn(3, '\t').collect();
        if fields.len() == 3 && fields[0] == "READ_ID" && fields[2] == identity {
            count.calls += 1;
            count.bytes += fields[1].parse::<u64>().unwrap();
        }
    }
    println!(
        "HEADER_INODE inode={inode} reads={} bytes={}",
        count.calls, count.bytes
    );
    count
}
fn inode_reads_after(root: &Path, marker: &str, path: &Path, inode: u64) -> Reads {
    let trace = fs::read_to_string(root.join("read.trace")).unwrap();
    let identity = format!("{inode}:{}", path.display());
    let mut reached = false;
    let mut count = Reads::default();
    for line in trace.lines() {
        let fields: Vec<_> = line.splitn(3, '\t').collect();
        if fields.len() != 3 {
            continue;
        }
        if fields[0] == marker && Path::new(fields[2]) == path {
            reached = true;
            continue;
        }
        if reached && fields[0] == "READ_ID" && fields[2] == identity {
            count.calls += 1;
            count.bytes += fields[1].parse::<u64>().unwrap();
        }
    }
    assert!(reached, "必须实际命中指定快照/变更标记");
    println!(
        "HEADER_IO_AFTER marker={marker} inode={inode} reads={} bytes={}",
        count.calls, count.bytes
    );
    count
}

fn trace_values(root: &Path, kind: &str, path: &Path) -> Vec<i64> {
    fs::read_to_string(root.join("read.trace"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.splitn(3, '\t').collect();
            (fields.len() == 3 && fields[0] == kind && Path::new(fields[2]) == path)
                .then(|| fields[1].parse().unwrap())
        })
        .collect()
}
fn require_swap(root: &Path) {
    let trace = fs::read_to_string(root.join("read.trace")).unwrap();
    assert_eq!(
        trace
            .lines()
            .filter(|line| line.starts_with("SWAP\t1\t"))
            .count(),
        1,
        "必须真实执行一次替换；未命中不是安全证据"
    );
    assert!(!trace.contains("SWAP_ERROR"));
    println!("HEADER_SWAP confirmed=true");
}
struct OpenatObservation {
    flags: i64,
    result: i64,
    parent_before: i64,
    parent_after: i64,
    file_inode: Option<i64>,
}
fn require_header_openat(f: &SourceFixture, path: &Path, ancestor: bool) -> OpenatObservation {
    let trace = fs::read_to_string(f.root.join("read.trace")).unwrap();
    let mut opened = None;
    let mut read = false;
    let mut verified = false;
    let mut before_boundary = false;
    for line in trace.lines() {
        let fields: Vec<_> = line.splitn(3, '\t').collect();
        if fields.len() != 3 {
            continue;
        }
        if Path::new(fields[2]) == f.log.as_path() {
            match fields[0] {
                "HEADER_OPENAT_OK" => {
                    opened = fields[1].parse::<i64>().ok();
                    read = false;
                }
                "HEADER_DATA" if opened == fields[1].parse::<i64>().ok() => read = true,
                "HEADER_PRECHECK" if read && opened == fields[1].parse::<i64>().ok() => {
                    verified = true
                }
                _ => {}
            }
        }
        let kind = if ancestor {
            "OPENAT_ANCESTOR_BEFORE"
        } else {
            "OPENAT_LEAF_BEFORE"
        };
        if fields[0] == kind && Path::new(fields[2]) == path {
            before_boundary = verified;
            break;
        }
    }
    assert!(before_boundary, "栅栏前必须真实命中同一 regular inode 的 openat → 数据读取 → fstat 快照；未命中不算安全证据");
    let kind = if ancestor {
        "OPENAT_ANCESTOR_BEFORE"
    } else {
        "OPENAT_LEAF_BEFORE"
    };
    let flags = trace_values(&f.root, kind, path);
    let result = trace_values(&f.root, "OPENAT_RESULT", path);
    let before = trace_values(&f.root, "OPENAT_PARENT_BEFORE", path);
    let after = trace_values(&f.root, "OPENAT_PARENT_AFTER", path);
    assert_eq!(flags.len(), 1, "必须命中一次真实 header openat 边界");
    assert_eq!(result.len(), 1);
    assert_eq!(before.len(), 1);
    assert_eq!(after.len(), 1);
    assert_ne!(flags[0] & i64::from(libc::O_NOFOLLOW), 0);
    if ancestor {
        assert_ne!(flags[0] & i64::from(libc::O_DIRECTORY), 0);
    } else {
        assert_eq!(flags[0] & i64::from(libc::O_DIRECTORY), 0);
    }
    let inode = trace_values(&f.root, "OPENAT_FILE_INODE", path)
        .first()
        .copied();
    println!("HEADER_OPENAT ancestor={ancestor} flags={} result={} errno={:?} parent_inode={}->{} file_inode={inode:?}", flags[0], result[0], trace_values(&f.root, "OPENAT_ERRNO", path), before[0], after[0]);
    OpenatObservation {
        flags: flags[0],
        result: result[0],
        parent_before: before[0],
        parent_after: after[0],
        file_inode: inode,
    }
}
fn contains(rows: &[SubagentView], id: &str) -> bool {
    rows.iter().any(|row| row.id == id)
}
fn assert_neutral(rows: &[SubagentView]) {
    let row = rows
        .iter()
        .find(|row| row.id == CHILD)
        .expect("真实子任务头必须可见");
    assert_eq!(serde_json::to_value(row).unwrap()["status"], "unknown");
    assert!(row.end_ts.is_none(), "header 源不得伪造终止事实");
}
fn create_fifo(path: &Path) {
    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
}

fn failed_header_replacement(f: &SourceFixture, preserve_stamp: bool) {
    let valid = log_text(&fixture_header(CHILD_FIXTURE), &[], 0);
    let mut invalid = vec![b' '; valid.len()];
    invalid[0] = b'!';
    *invalid.last_mut().unwrap() = b'\n';
    fs::create_dir_all(f.log.parent().unwrap()).unwrap();
    fs::write(&f.log, invalid).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(3600));
    let old = fs::metadata(&f.log).unwrap();
    assert!(f.collect(PARENT).is_empty());
    require_header_io(&f.root, &f.log, "failed-header-positive");
    let stage = f.root.join("valid-header-stage.jsonl");
    fs::write(&stage, valid).unwrap();
    File::options()
        .write(true)
        .open(&stage)
        .unwrap()
        .set_times(FileTimes::new().set_modified(
            old.modified().unwrap()
                + if preserve_stamp {
                    Duration::ZERO
                } else {
                    Duration::from_secs(1)
                },
        ))
        .unwrap();
    fs::rename(stage, &f.log).unwrap();
    let new = fs::metadata(&f.log).unwrap();
    assert_ne!(old.ino(), new.ino());
    assert_eq!(old.len(), new.len());
    if preserve_stamp {
        assert_eq!(old.modified().unwrap(), new.modified().unwrap());
    }
    let mut rows = f.collect(PARENT);
    if !contains(&rows, CHILD) {
        rows = f.collect(PARENT);
    }
    assert_neutral(&rows);
    let count = inode_read_count(&f.root, &f.log, new.ino());
    assert!(count.bytes > 0 && count.bytes <= HEADER_INPUT_LIMIT);
}

fn rewrite_header_binding(f: &SourceFixture, same_inode: bool, grows: bool) {
    f.child(&[], LARGE);
    f.set_modified(SystemTime::now() - Duration::from_secs(3600));
    assert_neutral(&f.collect(PARENT));
    let before = require_header_io(&f.root, &f.log, "binding-before");
    let old = fs::metadata(&f.log).unwrap();
    let new_parent = if grows { FOREIGN } else { OTHER };
    if !grows {
        assert_eq!(PARENT.len(), OTHER.len());
        let mut parent = fixture_header(PARENT_FIXTURE);
        parent["id"] = json!(OTHER);
        f.write("other-parent", 4, &parent, &[], 0);
    }
    let mut header = fixture_header(CHILD_FIXTURE);
    header["parentSession"] = json!(new_parent);
    let replacement = log_text(&header, &[], LARGE + if grows { 128 } else { 0 });
    if same_inode {
        fs::write(&f.log, replacement).unwrap();
        f.set_modified(old.modified().unwrap());
    } else {
        let stage = f.root.join("replacement-stage.jsonl");
        fs::write(&stage, replacement).unwrap();
        File::options()
            .write(true)
            .open(&stage)
            .unwrap()
            .set_times(FileTimes::new().set_modified(old.modified().unwrap()))
            .unwrap();
        fs::rename(stage, &f.log).unwrap();
    }
    let new = fs::metadata(&f.log).unwrap();
    assert_eq!(new.ino() == old.ino(), same_inode);
    assert_eq!(new.modified().unwrap(), old.modified().unwrap());
    if grows {
        assert!(new.len() > old.len());
    } else {
        assert_eq!(new.len(), old.len());
    }
    assert!(!contains(&f.collect(PARENT), CHILD));
    let mut rows = f.collect(new_parent);
    if !contains(&rows, CHILD) {
        rows = f.collect(new_parent);
    }
    assert_neutral(&rows);
    assert!(!contains(&f.collect(PARENT), CHILD), "不能继续挂旧父");
    let after = observed(&f.root, &f.log, "binding-after");
    assert!(after.bytes > before.bytes && after.bytes - before.bytes <= HEADER_INPUT_LIMIT);
}

fn compressed_header_case(f: &SourceFixture, skippable_bytes: usize) {
    f.child(&[], 0);
    assert_neutral(&f.collect(PARENT));
    require_header_io(&f.root, &f.log, "plain-before-compressed-positive");
    fs::remove_file(&f.log).unwrap();
    let header = log_text(&fixture_header(CHILD_FIXTURE), &[], 0);
    let frame = zstd::stream::encode_all(header.as_bytes(), 1).unwrap();
    let mut input = Vec::new();
    if skippable_bytes > 0 {
        input.extend_from_slice(&0x184D2A50u32.to_le_bytes());
        input.extend_from_slice(&(skippable_bytes as u32).to_le_bytes());
        input.extend(std::iter::repeat_n(0xa5, skippable_bytes));
    }
    input.extend(frame);
    let compressed = f.log.with_file_name("session.v4.jsonl.zstd");
    fs::write(&compressed, &input).unwrap();
    let rows = f.collect(PARENT);
    let count = observed(&f.root, &compressed, "compressed-header-actual-input");
    println!(
        "ZSTD_HEADER skippable_bytes={skippable_bytes} input_bytes={} actual_read_bytes={}",
        input.len(),
        count.bytes
    );
    if skippable_bytes < HEADER_BUDGET as usize {
        assert_neutral(&rows);
        assert!(
            count.bytes > 0,
            "正常 zstd/skippable 正控必须真正读取压缩输入"
        );
    }
    assert!(
        count.bytes <= HEADER_INPUT_LIMIT,
        "解压后 header 输出预算不能代替压缩输入预算: {count:?}"
    );
}

// 另一个真实目录树；使用不同 inode 和长度，避免旧缓存偶然命中掩盖根替换。
fn alternate_sessions(sessions: &Path) -> Vec<(PathBuf, u64)> {
    let mut entries = Vec::new();
    for (name, raw) in [("parent", PARENT_FIXTURE), ("child", CHILD_FIXTURE)] {
        let relative = PathBuf::from(format!("project/{name}/session.v4.jsonl"));
        let path = sessions.join(&relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut header = fixture_header(raw);
        header["probeDirectory"] = json!("different-real-directory");
        fs::write(
            &path,
            log_text(&header, &[], if name == "child" { LARGE + 128 } else { 0 }),
        )
        .unwrap();
        entries.push((relative, fs::metadata(&path).unwrap().ino()));
    }
    entries
}

fn raw_window_frame(header: &[u8], window_log: u8) -> Vec<u8> {
    assert!((10..=25).contains(&window_log) && header.len() <= 1024);
    let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0, (window_log - 10) << 3];
    let block = ((header.len() as u32) << 3).to_le_bytes(); // 非末尾 raw block
    frame.extend_from_slice(&block[..3]);
    frame.extend_from_slice(header);
    frame.extend_from_slice(&[1, 0, 0]); // 末尾空 raw block；完整合法 frame
    frame
}

// 拟议的 header 解码单次分配请求上限；以 allocator 请求计量，不依赖 RSS 触页。
const ZSTD_HEADER_ALLOCATION_LIMIT: i64 = 8 * 1024 * 1024;
fn window_allocation_case(f: &SourceFixture, window_log: u8) {
    f.child(&[], 0);
    assert_neutral(&f.collect(PARENT));
    require_header_io(&f.root, &f.log, "plain-window-positive");
    fs::remove_file(&f.log).unwrap();
    let target = f.log.with_file_name("session.v4.jsonl.zstd");
    let frame = raw_window_frame(
        log_text(&fixture_header(CHILD_FIXTURE), &[], 0).as_bytes(),
        window_log,
    );
    fs::write(&target, &frame).unwrap();
    let rows = f.collect(PARENT);
    require_header_io(&f.root, &target, "window-frame-input");
    assert!(
        trace_values(&f.root, "ZSTD_ALLOCATION_SCOPE", &target).contains(&1),
        "必须实际读取目标压缩输入并启动观测"
    );
    let requests = trace_values(&f.root, "ZSTD_ALLOCATION_REQUEST", &target);
    let maximum = requests.iter().copied().max().unwrap_or(0);
    println!("ZSTD_WINDOW window_log={window_log} input_bytes={} requests={requests:?} max_requested={maximum} proposed_limit={ZSTD_HEADER_ALLOCATION_LIMIT}", frame.len());
    if window_log == 10 {
        assert_neutral(&rows);
    }
    assert!(
        maximum <= ZSTD_HEADER_ALLOCATION_LIMIT,
        "小 header 输入不能触发按大窗口分配；大窗口可保守拒绝而非先分配"
    );
}

fn duplicate_cache_case(f: &SourceFixture, case: &str) {
    f.child(&[], 0);
    let duplicate = f.write(
        "physical-duplicate",
        4,
        &fixture_header(CHILD_FIXTURE),
        &[],
        0,
    );
    assert!(
        !contains(&f.collect(PARENT), CHILD),
        "两个真实实体同 ID 时必须保持歧义"
    );
    let first = require_header_io(&f.root, &f.log, "duplicate-first-cached-header");
    let second = require_header_io(&f.root, &duplicate, "duplicate-second-cached-header");
    if case == "duplicate_unchanged_cache" {
        assert!(!contains(&f.collect(PARENT), CHILD));
        assert_eq!(reads(&f.root, &f.log).bytes, first.bytes);
        assert_eq!(reads(&f.root, &duplicate).bytes, second.bytes);
        return;
    }
    if case == "duplicate_removed_positive" {
        fs::remove_file(&duplicate).unwrap();
        assert!(!duplicate.exists());
        assert_neutral(&f.collect(PARENT));
        return;
    }
    let before = fs::metadata(&duplicate).unwrap();
    let stage = f.root.join("same-id-duplicate-stage.jsonl");
    // 相同脱敏原生头：ID、父 ID、长度和 mtime 均不变，实体明确换 inode。
    fs::write(&stage, log_text(&fixture_header(CHILD_FIXTURE), &[], 0)).unwrap();
    File::options()
        .write(true)
        .open(&stage)
        .unwrap()
        .set_times(FileTimes::new().set_modified(before.modified().unwrap()))
        .unwrap();
    fs::rename(stage, &duplicate).unwrap();
    let after = fs::metadata(&duplicate).unwrap();
    assert_ne!(before.ino(), after.ino());
    assert_eq!(before.len(), after.len());
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    let rows = f.collect(PARENT); // 不能先重试，把错误唯一性的那一拍吞掉。
    let new_io = inode_read_count(&f.root, &duplicate, after.ino());
    println!("DUPLICATE_CACHE_REPLACEMENT old_inode={} new_inode={} same_id={CHILD} same_size=true same_mtime=true replacement_read_bytes={} first_result_contains_child={}", before.ino(), after.ino(), new_io.bytes, contains(&rows, CHILD));
    assert!(
        !contains(&rows, CHILD),
        "缓存失效时跳过仍存在的同 ID 实体，不能将另一条误判唯一并发布"
    );
    assert!(
        !contains(&f.collect(PARENT), CHILD),
        "重读后也必须保持真实重复 ID 的歧义"
    );
}

fn parent_reread_case(f: &SourceFixture, case: &str) {
    f.child(&[], LARGE);
    let parent = f.home.join("sessions/project/parent/session.v4.jsonl");
    let original = fs::metadata(&parent).unwrap();
    let append_bytes = b"{\"type\":\"probe/body\"}\n";
    let mut replacement = fixture_header(PARENT_FIXTURE);
    assert_eq!(PARENT.len(), OTHER.len());
    replacement["id"] = json!(OTHER);
    // 与 native Prime 追加后的父文件等长，ctime 场景仅原地写入并恢复 mtime。
    let staged = log_text(&replacement, &[json!({"type":"probe/body"})], 0);
    assert_eq!(
        staged.len() as u64,
        original.len() + append_bytes.len() as u64
    );
    let stage = f.root.join("reread-parent-stage.jsonl");
    fs::write(&stage, staged).unwrap();
    let staged_inode = fs::metadata(&stage).unwrap().ino();
    let rows = f.collect(PARENT);
    require_header_io(&f.root, &f.log, "reread-child-initial-positive");
    require_header_io(&f.root, &parent, "reread-parent-source-input");
    assert_eq!(trace_values(&f.root, "REREAD_PRIME_APPEND", &parent), [1]);
    assert_eq!(
        trace_values(&f.root, "REREAD_PRIME_BYTES", &parent),
        [append_bytes.len() as i64]
    );
    assert!(
        !trace_values(&f.root, "REREAD_PARENT_DATA", &parent).is_empty(),
        "必须真的重读父头，不能把首读替换当作重读回归"
    );
    assert!(
        trace_values(&f.root, "PARENT_OPENAT_OK", &parent).len() >= 2,
        "父头首读和重读必须来自真实 openat"
    );
    assert!(
        trace_values(&f.root, "PARENT_HEADER_DATA", &parent).len() >= 2,
        "必须有两次真实父头数据读取"
    );
    assert!(
        trace_values(&f.root, "PARENT_SNAPSHOT", &parent).len() >= 2,
        "两次读取后均需真实 fstat 快照"
    );
    assert_eq!(
        trace_values(&f.root, "REREAD_SNAPSHOT_BARRIER", &parent),
        [1]
    );
    let trace = fs::read_to_string(f.root.join("read.trace")).unwrap();
    assert!(
        !trace.contains("MUTATION_ERROR"),
        "探针变更失败不算 source RED: {trace}"
    );
    println!("PARENT_REREAD case={case} real_reread=true late_boundary=true first_result_contains_child={}", contains(&rows, CHILD));
    if case == "parent_reread_positive" {
        assert_eq!(trace_values(&f.root, "REREAD_NO_MUTATION", &parent), [1]);
        assert!(trace_values(&f.root, "REREAD_MUTATION_APPLIED", &parent).is_empty());
        assert_eq!(
            fs::metadata(&parent).unwrap().len(),
            original.len() + append_bytes.len() as u64
        );
        assert_neutral(&rows);
        return;
    }
    assert_eq!(
        trace_values(&f.root, "REREAD_MUTATION_APPLIED", &parent),
        [1]
    );
    let changed = if case == "parent_reread_generation" {
        require_swap(&f.root);
        let changed = parent.with_file_name("session.v5.jsonl");
        assert_eq!(fs::metadata(&changed).unwrap().ino(), staged_inode);
        assert_eq!(
            inode_read_count(&f.root, &changed, staged_inode).bytes,
            0,
            "父链换代后的替换实体不得读取"
        );
        changed
    } else {
        let metadata = fs::metadata(&parent).unwrap();
        assert_eq!(
            inode_reads_after(&f.root, "REREAD_MUTATION_APPLIED", &parent, metadata.ino()).bytes,
            0,
            "同 inode 替换后的内容不得再次读取"
        );
        assert_eq!(
            trace_values(&f.root, "REREAD_REFERENCE_INODE", &parent),
            [metadata.ino() as i64]
        );
        assert_eq!(
            trace_values(&f.root, "REREAD_REFERENCE_SIZE", &parent),
            [metadata.len() as i64]
        );
        let modified = metadata
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        assert_eq!(
            trace_values(&f.root, "REREAD_REFERENCE_MTIME_SEC", &parent),
            [modified.as_secs() as i64]
        );
        assert_eq!(
            trace_values(&f.root, "REREAD_REFERENCE_MTIME_NSEC", &parent),
            [i64::from(modified.subsec_nanos())]
        );
        let reference_ctime = (
            trace_values(&f.root, "REREAD_REFERENCE_CTIME_SEC", &parent)[0],
            trace_values(&f.root, "REREAD_REFERENCE_CTIME_NSEC", &parent)[0],
        );
        assert_ne!(
            (metadata.ctime(), metadata.ctime_nsec()),
            reference_ctime,
            "必须真改 ctime，而不是只声明发生重写"
        );
        println!("REREAD_CTIME inode={} same_size=true same_mtime=true old_ctime={reference_ctime:?} new_ctime={:?}", metadata.ino(), (metadata.ctime(), metadata.ctime_nsec()));
        parent.clone()
    };
    // 输入/输出计数已捕获；这里额外读测试文件仅验证探针确实换了父身份。
    let actual = fs::read_to_string(&changed).unwrap();
    let actual_header: Value = serde_json::from_str(actual.lines().next().unwrap()).unwrap();
    assert_eq!(actual_header["id"], OTHER);
    assert!(
        !contains(&rows, CHILD),
        "重读取得的新快照之后父链已失效，返回前不能仅核对子节点而发布"
    );
}

fn detail_event(text: &str) -> Value {
    json!({"type":"user/message","seq":8,"data":{"role":"user","content":[{"type":"text","text":text}],"source":{"kind":"user"}}})
}

fn detail(
    f: &SourceFixture,
    parent: &str,
    child: &str,
) -> Result<multi_agents_manager_lib::remote::content::MessagesPage, String> {
    let root = f.home.to_str().expect("隔离临时路径应为 UTF-8");
    multi_agents_manager_lib::remote::content::read_dsh_subagent_messages_with(
        &f.root,
        Some(root),
        parent,
        child,
        20,
        0,
    )
}

fn assert_trusted_detail(f: &SourceFixture) {
    let page = detail(f, PARENT, CHILD).expect("真实父子正控必须可读取");
    assert!(page
        .messages
        .iter()
        .any(|message| message.content == "trusted-child-original"));
    assert!(!page
        .messages
        .iter()
        .any(|message| message.content == "foreign-child-replacement"));
}

fn detail_relationship_case(f: &SourceFixture, case: &str) {
    f.child(&[detail_event("trusted-child-original")], 0);
    assert_trusted_detail(f);
    if case == "detail_parent_replaced" {
        let parent = f.home.join("sessions/project/parent/session.v4.jsonl");
        let old = fs::metadata(&parent).unwrap();
        let mut changed = fixture_header(PARENT_FIXTURE);
        assert_eq!(PARENT.len(), OTHER.len());
        changed["id"] = json!(OTHER);
        let text = log_text(&changed, &[], 0);
        assert_eq!(text.len() as u64, old.len());
        fs::write(&parent, text).unwrap();
        File::options()
            .write(true)
            .open(&parent)
            .unwrap()
            .set_times(FileTimes::new().set_modified(old.modified().unwrap()))
            .unwrap();
        let current = fs::metadata(&parent).unwrap();
        assert_eq!(old.ino(), current.ino());
        assert_eq!(old.len(), current.len());
        assert_eq!(old.modified().unwrap(), current.modified().unwrap());
        assert_ne!(
            (old.ctime(), old.ctime_nsec()),
            (current.ctime(), current.ctime_nsec())
        );
    } else if case == "detail_orphan_parent" {
        let mut parent = fixture_header(PARENT_FIXTURE);
        parent["origin"] = json!("subagent");
        parent["parentSession"] = json!("session-<missing-ancestor-id>");
        parent["delegationDepth"] = json!(1);
        f.write("parent", 4, &parent, &[], 0);
    } else if case == "detail_duplicate_parent" {
        f.write(
            "duplicate-parent",
            4,
            &fixture_header(PARENT_FIXTURE),
            &[],
            0,
        );
    } else {
        f.write(
            "duplicate-child",
            4,
            &fixture_header(CHILD_FIXTURE),
            &[detail_event("duplicate")],
            0,
        );
    }
    let result = detail(f, PARENT, CHILD);
    assert!(
        result.is_err(),
        "{case}: 父身份/父链或子身份不可唯一证明时必须拒绝，实际返回: {result:?}"
    );
}

// 新进程隔离生产缓存；入口只由父测试在装好探针和临时 home 后调用。
#[test]
#[ignore = "由父测试启动，不能独立当作覆盖跳过"]
fn source_io_worker() {
    let root = PathBuf::from(std::env::var_os("H08_IO_ROOT").expect("缺少隔离 home"));
    let case = std::env::var("H08_IO_CASE").unwrap();
    let f = SourceFixture::new(root);
    match case.as_str() {
        "deadline_control" => {
            let fifo = f.root.join("deliberate-block.fifo");
            create_fifo(&fifo);
            println!("DELIBERATE_FIFO_READY");
            std::io::stdout().flush().unwrap();
            let _blocked = File::open(fifo).unwrap();
            panic!("无写端 FIFO 不应自行完成打开");
        }
        "detail_single_snapshot_swap" => {
            f.child(&[detail_event("trusted-child-original")], 0);
            let original_inode = fs::metadata(&f.log).unwrap().ino();
            let mut replacement = fixture_header(CHILD_FIXTURE);
            replacement["parentSession"] = json!(FOREIGN);
            let stage = f.root.join("detail-stage.jsonl");
            fs::write(
                &stage,
                log_text(
                    &replacement,
                    &[detail_event("foreign-child-replacement")],
                    0,
                ),
            )
            .unwrap();
            let replacement_inode = fs::metadata(&stage).unwrap().ino();
            assert_ne!(original_inode, replacement_inode);
            let result = detail(&f, PARENT, CHILD);
            require_swap(&f.root);
            assert_eq!(
                trace_values(&f.root, "DETAIL_FIRST_READ_INODE", &f.log),
                [original_inode as i64],
                "替换前必须真实读取已授权子实体"
            );
            assert_eq!(fs::metadata(&f.log).unwrap().ino(), replacement_inode);
            assert_eq!(
                inode_read_count(&f.root, &f.log, replacement_inode).bytes,
                0,
                "身份核验后不能再次读另一实体的正文"
            );
            if let Ok(page) = result {
                assert!(
                    page.messages
                        .iter()
                        .any(|message| message.content == "trusted-child-original"),
                    "成功返回必须包含原实体正文"
                );
                assert!(
                    !page
                        .messages
                        .iter()
                        .any(|message| message.content == "foreign-child-replacement"),
                    "不得返回替换后外父实体正文"
                );
            }
        }
        "detail_snapshot_positive" => {
            f.child(&[detail_event("trusted-child-original")], 0);
            assert_trusted_detail(&f);
            require_header_io(&f.root, &f.log, "detail-original-positive");
        }
        "detail_parent_replaced"
        | "detail_orphan_parent"
        | "detail_duplicate_parent"
        | "detail_duplicate_child" => detail_relationship_case(&f, &case),
        "fork_child" => {
            fs::remove_dir_all(f.home.join("sessions/project/parent")).unwrap();
            f.write("fork", 4, &fixture_header(FORK_FIXTURE), &[], 0);
            let mut header = fixture_header(CHILD_FIXTURE);
            header["parentSession"] = json!("session-<fork-id>");
            f.write("child", 4, &header, &[], 0);
            assert_neutral(&f.collect("session-<fork-id>"));
            require_header_io(&f.root, &f.log, "fork-child-header");
        }
        "unknown_open" | "unknown_completed" | "unknown_kernel_lock" => {
            let event = if case == "unknown_completed" {
                serde_json::from_str::<Value>(CHILD_FIXTURE).unwrap()["termination"].clone()
            } else {
                json!({"type":"turn/start","seq":100,"data":{"turn":1}})
            };
            f.child(&[event], LARGE);
            let lease = if case == "unknown_kernel_lock" {
                let file = File::create(f.log.parent().unwrap().join("session.lock")).unwrap();
                file.try_lock().unwrap();
                Some(file)
            } else {
                None
            };
            assert_neutral(&f.collect(PARENT));
            require_header_io(&f.root, &f.log, "neutral-header-projection-only");
            println!("STATUS_SCOPE header_only=true lifecycle_verified=false");
            drop(lease);
        }
        "bounded_header" | "unchanged_header" | "body_growth" => {
            f.child(&[], LARGE);
            assert_neutral(&f.collect(PARENT));
            let before = require_header_io(&f.root, &f.log, "initial-bounded-header");
            if case == "unchanged_header" {
                assert_neutral(&f.collect(PARENT));
                assert_eq!(reads(&f.root, &f.log).bytes, before.bytes);
            } else if case == "body_growth" {
                OpenOptions::new()
                    .append(true)
                    .open(&f.log)
                    .unwrap()
                    .write_all(b"{\"type\":\"probe/body\"}\n")
                    .unwrap();
                assert_neutral(&f.collect(PARENT));
                let after = observed(&f.root, &f.log, "body-change-header-revalidation");
                assert!(
                    after.bytes > before.bytes && after.bytes - before.bytes <= HEADER_INPUT_LIMIT
                );
            }
        }
        "aged_changed_header" => {
            f.child(&[], 0);
            assert_neutral(&f.collect(PARENT));
            require_header_io(&f.root, &f.log, "fresh-header-before-age");
            let mut header = fixture_header(CHILD_FIXTURE);
            header["parentSession"] = json!(FOREIGN);
            f.write("child", 4, &header, &[], 0);
            f.set_modified(SystemTime::now() - Duration::from_secs(48 * 3600));
            assert!(!contains(&f.collect(PARENT), CHILD));
        }
        "cold_aged_header" => {
            // 冷历史（超 24h 且无缓存）必须仍然可见：归属判定要求全目录 ID 唯一，
            // 跳过任何一个候选都无法排出重复 ID，旧历史下的子任务会永久消失。
            // 边界是**有界**：补读必须是预算内的 header 读，且只付一次。
            f.child(&[], LARGE);
            f.set_modified(SystemTime::now() - Duration::from_secs(48 * 3600));
            let rows = f.collect(PARENT);
            assert_neutral(&rows);
            // 冷文件只读 header 预算内的一小段，不是整文件。
            let cold = require_header_io(&f.root, &f.log, "cold-aged-header-filled");
            assert!(
                cold.bytes <= HEADER_INPUT_LIMIT,
                "冷文件补读必须有界: {cold:?}"
            );
            // 第二次调用命中缓存，不再产生新的数据读取（不重复付代价）。
            let before = reads(&f.root, &f.log);
            assert_neutral(&f.collect(PARENT));
            assert_eq!(
                reads(&f.root, &f.log).bytes,
                before.bytes,
                "冷文件补读必须进缓存，第二次调用不得重复读取"
            );
        }
        "fresh_history_does_not_consume_cold_budget" => {
            // 关键不变量：补读上限**只对超窗文件计数**。
            //
            // 要让这条断言真正敏感，冷文件必须**排在枚举末尾之后才出现**——
            // 否则它在额度耗尽前就被读到，用例无论计数对错都会绿（这正是
            // 上一版的缺陷）。做法：先写 600 个新鲜会话，再写冷会话，并把
            // 冷会话命名成排序靠后的目录（`zzz-cold`）。
            f.child(&[], 0);
            for index in 0..600 {
                let name = format!("aaa-fresh-{index:03}");
                let mut header = fixture_header(PARENT_FIXTURE);
                header["id"] = json!(format!("<fresh-{index}>"));
                header["parentSession"] = Value::Null;
                f.write(&name, 4, &header, &[], 0);
            }
            // 冷会话：命名排序在所有新鲜目录之后，枚举到它时已消耗 >512 个候选。
            let mut cold = fixture_header(PARENT_FIXTURE);
            cold["id"] = json!("<cold-tail>");
            cold["parentSession"] = Value::Null;
            let path = f.write("zzz-cold", 4, &cold, &[], 0);
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(
                    FileTimes::new()
                        .set_modified(SystemTime::now() - Duration::from_secs(48 * 3600)),
                )
                .unwrap();
            // 若计数错误地对新鲜文件递增，额度在读到 zzz-cold 之前就已耗尽，
            // 整轮返回「覆盖不足」→ 子任务不可见 → 断言失败。
            assert_neutral(&f.collect(PARENT));
        }
        "cold_history_over_budget_fails_closed" => {
            // 补读有上限：超过上限必须仍按「覆盖不足」拒绝，不能变成无界扫描。
            // 这里造 COLD_FILL_LIMIT+1 个冷会话（每个一个日志），远超单次预算。
            f.child(&[], 0);
            let cold = SystemTime::now() - Duration::from_secs(48 * 3600);
            for index in 0..513 {
                let name = format!("cold-{index:03}");
                let mut header = fixture_header(PARENT_FIXTURE);
                header["id"] = json!(format!("<cold-{index}>"));
                header["parentSession"] = Value::Null;
                let path = f.write(&name, 4, &header, &[], 0);
                File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_times(FileTimes::new().set_modified(cold))
                    .unwrap();
            }
            // 子任务本身新鲜，但目录里有超量冷候选 → 无法证明唯一性 → 不出卡。
            assert!(
                !contains(&f.collect(PARENT), CHILD),
                "超过补读上限时必须 fail-closed，不得发布未经完整核验的归属"
            );
        }
        "directory_alias" | "distinct_duplicate_id" => {
            f.child(&[], 0);
            if case == "directory_alias" {
                symlink(
                    f.log.parent().unwrap(),
                    f.home.join("sessions/project/child-alias"),
                )
                .unwrap();
                let rows = f.collect(PARENT);
                assert_eq!(rows.iter().filter(|row| row.id == CHILD).count(), 1);
            } else {
                f.write(
                    "physical-duplicate",
                    4,
                    &fixture_header(CHILD_FIXTURE),
                    &[],
                    0,
                );
                assert!(!contains(&f.collect(PARENT), CHILD));
                require_header_io(
                    &f.root,
                    &f.home
                        .join("sessions/project/physical-duplicate/session.v4.jsonl"),
                    "physical-duplicate-positive",
                );
            }
            require_header_io(&f.root, &f.log, "canonical-identity-header");
        }
        "duplicate_same_stamp_inode"
        | "duplicate_unchanged_cache"
        | "duplicate_removed_positive" => duplicate_cache_case(&f, &case),
        "parent_reread_generation" | "parent_reread_ctime" | "parent_reread_positive" => {
            parent_reread_case(&f, &case)
        }
        "new_inode_same_stamp" => rewrite_header_binding(&f, false, false),
        "same_inode_equal_header" => rewrite_header_binding(&f, true, false),
        "same_inode_grown_header" => rewrite_header_binding(&f, true, true),
        "failed_header_same_stamp" => failed_header_replacement(&f, true),
        "failed_header_new_stamp" => failed_header_replacement(&f, false),
        "failed_header_cached" => {
            // 负缓存契约：坏头首次读取有界、不会出卡；同一实体快照再次扫描零新增读取。
            // 变化后的 inode/指纹恢复由 failed_header_replacement 的独立正控验证。
            fs::create_dir_all(f.log.parent().unwrap()).unwrap();
            let mut header = fixture_header(CHILD_FIXTURE);
            header["probe"] = json!("x".repeat(2 * HEADER_BUDGET as usize));
            fs::write(&f.log, serde_json::to_vec(&header).unwrap()).unwrap();
            assert!(f.collect(PARENT).is_empty());
            let before = require_header_io(&f.root, &f.log, "over-budget-header-failure");
            assert!(f.collect(PARENT).is_empty());
            assert_eq!(reads(&f.root, &f.log).bytes, before.bytes);
        }
        "generation_between_calls" => {
            f.child(&[], LARGE);
            assert_neutral(&f.collect(PARENT));
            require_header_io(&f.root, &f.log, "old-generation-positive");
            let mut header = fixture_header(CHILD_FIXTURE);
            header["id"] = json!("<replacement-child-id>");
            header["parentSession"] = json!(FOREIGN);
            let next = f.write("child", 5, &header, &[], LARGE);
            assert!(!contains(&f.collect(PARENT), CHILD));
            assert!(contains(&f.collect(FOREIGN), "<replacement-child-id>"));
            require_header_io(&f.root, &next, "new-generation-header");
        }
        "generation_during_header" | "same_path_after_header" => {
            f.child(&[], LARGE);
            let mut header = fixture_header(CHILD_FIXTURE);
            header["parentSession"] = json!(FOREIGN);
            fs::write(
                f.root.join("replacement-stage.jsonl"),
                log_text(&header, &[], LARGE),
            )
            .unwrap();
            let replacement_inode = fs::metadata(f.root.join("replacement-stage.jsonl"))
                .unwrap()
                .ino();
            let before = f.warm_and_arm_header();
            let rows = f.collect(PARENT);
            require_swap(&f.root);
            require_header_openat(&f, &f.log, false);
            assert!(
                !contains(&rows, CHILD),
                "返回前已替换的头/代际不得发布旧父关联"
            );
            let replacement_path = if case == "generation_during_header" {
                f.log.with_file_name("session.v5.jsonl")
            } else {
                f.log.clone()
            };
            assert_eq!(
                fs::metadata(&replacement_path).unwrap().ino(),
                replacement_inode
            );
            assert_eq!(
                inode_read_count(&f.root, &replacement_path, replacement_inode).bytes,
                0,
                "真实 header openat 前替换的实体必须零字节读取"
            );
            // 原实体允许通过已验证句柄有界读取；其字节不能归入替换 inode。
            assert!(
                reads(&f.root, &f.log).bytes.saturating_sub(before.bytes) <= HEADER_INPUT_LIMIT
            );
        }
        "leaf_link_before_header"
        | "fifo_before_header"
        | "ancestor_before_component"
        | "ancestor_before_leaf"
        | "header_openat_positive" => {
            f.child(&[], LARGE);
            let original_inode = fs::metadata(&f.log).unwrap().ino();
            let outside = f.outside();
            let before = f.warm_and_arm_header();
            let rows = f.collect(PARENT);
            let ancestor = case == "ancestor_before_component";
            let path = if ancestor {
                f.log.parent().unwrap()
            } else {
                &f.log
            };
            let boundary = require_header_openat(&f, path, ancestor);
            if case == "header_openat_positive" {
                assert_neutral(&rows);
                assert!(boundary.result >= 0);
                assert_eq!(boundary.file_inode, Some(original_inode as i64));
                assert_eq!(boundary.parent_before, boundary.parent_after);
                let after = observed(&f.root, &f.log, "real-header-openat-positive");
                assert!(
                    after.bytes > before.bytes && after.bytes - before.bytes <= HEADER_INPUT_LIMIT
                );
                assert!(!fs::read_to_string(f.root.join("read.trace"))
                    .unwrap()
                    .contains("SWAP"));
            } else {
                require_swap(&f.root);
                assert!(!contains(&rows, CHILD));
                // 若仍依赖第二次正文打开，真实 header 已多读一遍；此断言会抓住假命中。
                assert_eq!(
                    reads(&f.root, &f.log).bytes,
                    before.bytes,
                    "武装后必须在 header 数据读取前命中，不能依赖已移除的 facts 打开"
                );
                if case == "ancestor_before_leaf" {
                    assert!(boundary.result >= 0);
                    assert_eq!(boundary.parent_before, boundary.parent_after);
                    assert_eq!(boundary.file_inode, Some(original_inode as i64));
                    require_header_io(
                        &f.root,
                        &f.root.join("parked-child/session.v4.jsonl"),
                        "anchored-original-header",
                    );
                } else if case == "fifo_before_header" {
                    assert_ne!(boundary.flags & i64::from(libc::O_NONBLOCK), 0);
                    assert!(fs::symlink_metadata(&f.log).unwrap().file_type().is_fifo());
                    let inode = fs::symlink_metadata(&f.log).unwrap().ino();
                    assert_eq!(inode_read_count(&f.root, &f.log, inode).bytes, 0);
                } else {
                    assert!(boundary.result < 0);
                }
            }
            let count = observed(&f.root, &outside, "outside-header-bytes");
            assert_eq!(count.bytes, 0);
        }
        "link_between_calls" => {
            f.child(&[], LARGE);
            assert_neutral(&f.collect(PARENT));
            require_header_io(&f.root, &f.log, "header-before-static-link");
            let outside = f.outside();
            fs::remove_file(&f.log).unwrap();
            symlink(&outside, &f.log).unwrap();
            assert!(!contains(&f.collect(PARENT), CHILD));
            assert_eq!(reads(&f.root, &outside).bytes, 0);
        }
        "preexisting_fifo" => {
            fs::create_dir_all(f.log.parent().unwrap()).unwrap();
            create_fifo(&f.log);
            assert!(f.collect(PARENT).is_empty());
            require_header_io(
                &f.root,
                &f.home.join("sessions/project/parent/session.v4.jsonl"),
                "regular-parent-before-fifo-rejection",
            );
            assert_eq!(reads(&f.root, &f.log).bytes, 0);
        }
        "sessions_root_link" | "sessions_root_direct" => {
            f.child(&[], LARGE);
            assert_neutral(&f.collect(PARENT));
            require_header_io(&f.root, &f.log, "authorized-root-header-positive");
            let outside = f.root.join("outside-sessions");
            if case == "sessions_root_link" {
                fs::rename(f.home.join("sessions"), &outside).unwrap();
                symlink(&outside, f.home.join("sessions")).unwrap();
                let rows = f.collect(PARENT);
                let count = reads_under(&f.root, &outside);
                println!(
                    "SESSIONS_ROOT_SWAP is_symlink=true outside_reads={} outside_bytes={}",
                    count.calls, count.bytes
                );
                assert_eq!(
                    count.bytes, 0,
                    "授权 home 的 sessions 根链接不得授权读取根外内容"
                );
                assert!(!contains(&rows, CHILD));
            } else {
                fs::create_dir_all(&outside).unwrap();
                fs::write(
                    outside.join("trap.jsonl"),
                    log_text(&fixture_header(CHILD_FIXTURE), &[], LARGE),
                )
                .unwrap();
                assert!(!fs::symlink_metadata(f.home.join("sessions"))
                    .unwrap()
                    .file_type()
                    .is_symlink());
                assert_neutral(&f.collect(PARENT));
                assert_eq!(reads_under(&f.root, &outside).bytes, 0);
            }
        }
        "home_link_before_call" | "home_direct_positive" => {
            f.child(&[], LARGE);
            assert_neutral(&f.collect(PARENT));
            require_header_io(&f.root, &f.log, "authorized-home-baseline");
            let authorized_inode = fs::symlink_metadata(&f.home).unwrap().ino();
            let outside_home = f.root.join("replacement-home");
            alternate_sessions(&outside_home.join("sessions"));
            if case == "home_link_before_call" {
                fs::rename(&f.home, f.root.join("parked-home")).unwrap();
                symlink(&outside_home, &f.home).unwrap();
                assert!(fs::symlink_metadata(&f.home)
                    .unwrap()
                    .file_type()
                    .is_symlink());
                assert_ne!(fs::metadata(&f.home).unwrap().ino(), authorized_inode);
            }
            let rows = f.collect(PARENT);
            let count = reads_under(&f.root, &outside_home);
            println!("HOME_IDENTITY case={case} authorized_inode={authorized_inode} outside_reads={} outside_bytes={}", count.calls, count.bytes);
            assert_eq!(
                count.bytes, 0,
                "调用前 home 被替换为链接，不能将 canonicalize 后的另一目录当成原授权 home"
            );
            if case == "home_direct_positive" {
                assert_neutral(&rows);
            }
            // 允许实现保守返回空，或继续使用已锚定的原授权快照；禁止读取另一 home。
        }
        "membership_swapped_before_publish" => {
            f.child(&[], LARGE);
            assert_neutral(&f.collect(PARENT));
            require_header_io(&f.root, &f.log, "membership-swap-baseline-header");
            // 末验枚举前把**非目标**会话（foreign）换成同名的新实体，
            // 新实体携带与子任务相同的 ID → 名称集合完全不变，只有实体身份变了。
            let mut swapped = fixture_header(CHILD_FIXTURE);
            swapped["parentSession"] = json!(FOREIGN);
            let stage = f.root.join("swap-session");
            fs::create_dir_all(&stage).unwrap();
            fs::write(
                stage.join("session.v4.jsonl"),
                log_text(&swapped, &[], LARGE),
            )
            .unwrap();
            fs::write(f.root.join("probe.armed"), b"armed").unwrap();
            let rows = f.collect(PARENT);
            assert_eq!(
                trace_values(
                    &f.root,
                    "MEMBERSHIP_MUTATION_APPLIED",
                    &f.home.join("sessions/project/foreign")
                ),
                [1],
                "必须在末验枚举前真实发生同名换实体"
            );
            assert!(
                !contains(&rows, CHILD),
                "同名换实体改变了唯一性前提，必须拒绝发布旧结论"
            );
        }
        "membership_added_before_publish" => {
            f.child(&[], LARGE);
            // 正控：基线必须真实出卡（子任务可见），否则「拒绝」无意义。
            assert_neutral(&f.collect(PARENT));
            require_header_io(&f.root, &f.log, "membership-baseline-header");
            // 扫描后新增一个完整候选会话（携带**重复的子任务 ID** 且父为 FOREIGN），
            // 它不在逐行重开范围内；集合末验必须发现并拒绝发布。
            let mut late = fixture_header(CHILD_FIXTURE);
            late["parentSession"] = json!(FOREIGN);
            let stage = f.root.join("late-session");
            fs::create_dir_all(&stage).unwrap();
            fs::write(stage.join("session.v4.jsonl"), log_text(&late, &[], LARGE)).unwrap();
            fs::write(f.root.join("probe.armed"), b"armed").unwrap();
            let rows = f.collect(PARENT);
            assert_eq!(
                trace_values(
                    &f.root,
                    "MEMBERSHIP_MUTATION_APPLIED",
                    &f.home.join("sessions/project/late")
                ),
                [1],
                "必须在末验枚举前真实新增成员；未命中不算安全证据"
            );
            assert!(
                !contains(&rows, CHILD),
                "扫描后出现的候选未纳入本轮判定，必须拒绝发布而不是沿用旧集合"
            );
        }
        "sessions_real_directory_swap" | "sessions_real_directory_positive" => {
            f.child(&[], LARGE);
            let sessions = f.home.join("sessions");
            let original_inode = fs::symlink_metadata(&sessions).unwrap().ino();
            let stage = f.root.join("replacement-sessions");
            let alternate = alternate_sessions(&stage);
            f.warm_and_arm_header();
            let rows = f.collect(PARENT);
            assert_eq!(
                trace_values(&f.root, "SESSIONS_ROOT_VALIDATED_INODE", &sessions),
                [original_inode as i64]
            );
            let entrances = trace_values(&f.root, "SESSIONS_ROOT_IO_BEFORE", &sessions);
            assert_eq!(entrances.len(), 1, "必须在已校验根的真实目录遍历入口命中");
            assert!(matches!(entrances[0], 1 | 2));
            assert_eq!(
                trace_values(&f.root, "SESSIONS_ROOT_IO_RESULT", &sessions),
                [1]
            );
            let mut bytes = 0;
            for (relative, inode) in &alternate {
                bytes += inode_read_count(&f.root, &sessions.join(relative), *inode).bytes;
            }
            println!("REAL_ROOT_REPLACEMENT case={case} old_inode={original_inode} current_inode={} replacement_inode_bytes={bytes}", fs::symlink_metadata(&sessions).unwrap().ino());
            if case == "sessions_real_directory_swap" {
                require_swap(&f.root);
                assert!(!fs::symlink_metadata(&sessions)
                    .unwrap()
                    .file_type()
                    .is_symlink());
                assert_ne!(fs::metadata(&sessions).unwrap().ino(), original_inode);
                assert_eq!(
                    fs::metadata(f.root.join("parked-sessions")).unwrap().ino(),
                    original_inode
                );
                assert_eq!(
                    bytes, 0,
                    "根路径字符串没变、节点仍是目录，也不能扫描校验后换入的另一实体"
                );
                // 不强制丢弃通过原目录 FD 锚定的一致快照，安全边界是新实体零读取。
            } else {
                assert_eq!(fs::metadata(&sessions).unwrap().ino(), original_inode);
                assert_neutral(&rows);
                assert_eq!(bytes, 0);
                assert_eq!(reads_under(&f.root, &stage).bytes, 0);
                assert!(!fs::read_to_string(f.root.join("read.trace"))
                    .unwrap()
                    .contains("SWAP"));
            }
        }
        "unrelated_body_append_during_scan" | "unrelated_body_append_before_scan" => {
            f.child(&[], LARGE);
            let peer = f.home.join("sessions/project/foreign/session.v4.jsonl");
            let before = fs::metadata(&peer).unwrap();
            let append = b"{\"type\":\"probe/body\"}\n";
            if case == "unrelated_body_append_before_scan" {
                OpenOptions::new()
                    .append(true)
                    .open(&peer)
                    .unwrap()
                    .write_all(append)
                    .unwrap();
            }
            let rows = f.collect(PARENT);
            require_header_io(&f.root, &f.log, "requested-child-snapshot");
            require_header_io(&f.root, &peer, "unrelated-header-snapshot");
            assert_eq!(
                trace_values(&f.root, "UNRELATED_APPEND_BARRIER", &peer),
                [1],
                "目标子头与无关会话头都读完后必须真实命中"
            );
            let after = fs::metadata(&peer).unwrap();
            assert_eq!(after.ino(), before.ino());
            assert_eq!(after.len() - before.len(), append.len() as u64);
            if case == "unrelated_body_append_during_scan" {
                assert_eq!(trace_values(&f.root, "APPEND_APPLIED", &peer), [1]);
                assert_eq!(
                    trace_values(&f.root, "APPEND_BYTES", &peer),
                    [append.len() as i64]
                );
            }
            println!("UNRELATED_APPEND case={case} peer_id={FOREIGN} header_unchanged=true appended_bytes={} returned_child={}", append.len(), contains(&rows, CHILD));
            assert_neutral(&rows);
        }
        "zstd_window_small" => window_allocation_case(&f, 10),
        "zstd_window_large" => window_allocation_case(&f, 25),
        "allocation_observer_positive" => {
            let target = f.log.with_file_name("session.v4.jsonl.zstd");
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(&target, b"allocation-observer-calibration").unwrap();
            let input = fs::read(&target).unwrap();
            std::hint::black_box(&input);
            let mut allocation = Vec::<u8>::with_capacity(16 * 1024 * 1024);
            allocation.push(7);
            std::hint::black_box(&allocation);
            drop(allocation);
            File::open(&target).unwrap().metadata().unwrap(); // 仅校准采样范围关闭，使用真实 fstat。
            let scopes = trace_values(&f.root, "ZSTD_ALLOCATION_SCOPE", &target);
            let requests = trace_values(&f.root, "ZSTD_ALLOCATION_REQUEST", &target);
            println!("ALLOCATION_CALIBRATION scopes={scopes:?} requests={requests:?}");
            assert!(scopes.contains(&1) && scopes.contains(&0));
            assert!(
                requests.iter().any(|bytes| *bytes >= 16 * 1024 * 1024),
                "探针必须实际捕获本 Rust worker 的已知分配，不能把未命中算作内存预算通过"
            );
        }
        "zstd_regular" => compressed_header_case(&f, 0),
        "zstd_small_skippable" => compressed_header_case(&f, 128),
        "zstd_large_skippable" => compressed_header_case(&f, LARGE),
        "parent_before_header_open" => {
            f.child(&[], LARGE);
            let parent = f.home.join("sessions/project/parent/session.v4.jsonl");
            let mut replacement = fixture_header(PARENT_FIXTURE);
            replacement["id"] = json!(OTHER);
            let stage = f.root.join("parent-stage.jsonl");
            fs::write(&stage, log_text(&replacement, &[], 0)).unwrap();
            let replacement_inode = fs::metadata(&stage).unwrap().ino();
            f.warm_and_arm_header();
            // 两头都改变指纹，确保任一枚举顺序下第二个头均要真实重读。
            let modified = fs::metadata(&parent).unwrap().modified().unwrap();
            File::options()
                .write(true)
                .open(&parent)
                .unwrap()
                .set_times(FileTimes::new().set_modified(modified - Duration::from_secs(1)))
                .unwrap();
            let rows = f.collect(PARENT);
            require_swap(&f.root);
            let trace = fs::read_to_string(f.root.join("read.trace")).unwrap();
            let events: Vec<Vec<&str>> = trace
                .lines()
                .map(|line| line.splitn(3, '\t').collect())
                .filter(|fields: &Vec<&str>| fields.len() == 3)
                .collect();
            let barriers: Vec<_> = events
                .iter()
                .enumerate()
                .filter(|(_, fields)| fields[0] == "PARENT_PREOPEN_BARRIER")
                .collect();
            assert_eq!(
                barriers.len(),
                1,
                "父链替换必须真实发生在第二个 header openat 前"
            );
            let (index, barrier) = barriers[0];
            assert_ne!(
                barrier[1].parse::<i64>().unwrap() & i64::from(libc::O_NOFOLLOW),
                0
            );
            let other_snapshot = if Path::new(barrier[2]) == f.log.as_path() {
                "ARMED_PARENT_SNAPSHOT"
            } else {
                assert_eq!(Path::new(barrier[2]), parent.as_path());
                "ARMED_CHILD_SNAPSHOT"
            };
            assert!(
                events[..index]
                    .iter()
                    .any(|fields| fields[0] == other_snapshot),
                "替换前必须已有另一头真实读取后的 fstat 快照"
            );
            let changed = parent.with_file_name("session.v5.jsonl");
            assert_eq!(fs::metadata(&changed).unwrap().ino(), replacement_inode);
            assert_eq!(
                inode_read_count(&f.root, &changed, replacement_inode).bytes,
                0,
                "父链替换实体必须零字节读取"
            );
            assert!(!contains(&rows, CHILD), "父链代际变更后不得发布失效关联");
        }
        "parent_generation_during_headers" | "parent_chain_positive" => {
            f.child(&[], LARGE);
            let parent = f.home.join("sessions/project/parent/session.v4.jsonl");
            let mut replacement = fixture_header(PARENT_FIXTURE);
            replacement["id"] = json!("session-<replaced-parent-id>");
            let stage = f.root.join("parent-stage.jsonl");
            fs::write(&stage, log_text(&replacement, &[], 0)).unwrap();
            let staged_inode = fs::metadata(&stage).unwrap().ino();
            let rows = f.collect(PARENT);
            require_header_io(&f.root, &f.log, "child-chain-header-positive");
            require_header_io(&f.root, &parent, "parent-chain-header-positive");
            assert_eq!(
                trace_values(&f.root, "PARENT_CHAIN_BARRIER", &parent),
                [1],
                "必须在父子真实 header 都已读取后命中，不依赖目录枚举顺序"
            );
            if case == "parent_chain_positive" {
                assert_neutral(&rows);
                assert!(!fs::read_to_string(f.root.join("read.trace"))
                    .unwrap()
                    .contains("SWAP"));
            } else {
                require_swap(&f.root);
                let changed = parent.with_file_name("session.v5.jsonl");
                assert_eq!(fs::metadata(&changed).unwrap().ino(), staged_inode);
                assert_eq!(
                    inode_read_count(&f.root, &changed, staged_inode).bytes,
                    0,
                    "父链替换实体不得读取"
                );
                assert!(
                    !contains(&rows, CHILD),
                    "父链最新代际已换原生身份，返回前必须放弃旧父链下的子节点"
                );
            }
        }
        other => panic!("未知 header 场景 {other}"),
    }
}

macro_rules! source_cases {
    ($($name:ident => $case:literal),* $(,)?) => { $(#[test] fn $name() { isolated($case); })* };
}
source_cases! {
    detail_identity_and_returned_content_share_one_snapshot => "detail_single_snapshot_swap",
    detail_original_parent_and_child_can_be_read => "detail_snapshot_positive",
    detail_rejects_equal_length_parent_identity_rewrite => "detail_parent_replaced",
    detail_rejects_orphaned_parent_chain => "detail_orphan_parent",
    detail_rejects_duplicate_parent_identity => "detail_duplicate_parent",
    detail_rejects_duplicate_child_identity => "detail_duplicate_child",
    fork_child_survives_missing_fork_provenance_ancestor => "fork_child",
    open_turn_remains_neutral_unknown_in_header_source => "unknown_open",
    completed_turn_does_not_claim_completed_status => "unknown_completed",
    kernel_lock_does_not_claim_lifecycle_capability => "unknown_kernel_lock",
    large_body_requires_only_bounded_header_input => "bounded_header",
    unchanged_header_performs_no_additional_data_reads => "unchanged_header",
    body_growth_only_revalidates_bounded_header => "body_growth",
    aged_changed_header_does_not_reuse_stale_parent => "aged_changed_header",
    cold_aged_header_is_filled_once_within_budget => "cold_aged_header",
    cold_history_beyond_the_fill_budget_fails_closed => "cold_history_over_budget_fails_closed",
    fresh_history_does_not_consume_the_cold_fill_budget => "fresh_history_does_not_consume_cold_budget",
    canonical_directory_alias_keeps_one_real_child => "directory_alias",
    distinct_physical_duplicate_ids_are_not_guessed => "distinct_duplicate_id",
    cached_duplicate_same_stamp_new_inode_is_not_mistaken_as_unique => "duplicate_same_stamp_inode",
    unchanged_duplicate_cache_remains_ambiguous_without_new_reads => "duplicate_unchanged_cache",
    actually_removed_duplicate_restores_real_uniqueness => "duplicate_removed_positive",
    parent_generation_changed_after_real_reread_is_not_published => "parent_reread_generation",
    parent_same_inode_same_stamp_ctime_change_after_reread_is_not_published => "parent_reread_ctime",
    benign_parent_body_append_is_really_reread_and_keeps_child => "parent_reread_positive",
    new_inode_same_stamp_invalidates_cached_header => "new_inode_same_stamp",
    same_inode_equal_header_rewrite_restored_mtime_changes_parent => "same_inode_equal_header",
    same_inode_grown_header_rewrite_changes_parent => "same_inode_grown_header",
    failed_header_same_stamp_valid_inode_recovers => "failed_header_same_stamp",
    failed_header_changed_stamp_valid_inode_recovers => "failed_header_new_stamp",
    unchanged_over_budget_header_reuses_failure_cache => "failed_header_cached",
    generation_replacement_between_calls_updates_header_identity => "generation_between_calls",
    generation_added_before_header_open_reads_no_replacement => "generation_during_header",
    same_path_replacement_before_header_open_reads_no_replacement => "same_path_after_header",
    leaf_link_replaced_before_header_openat_is_rejected => "leaf_link_before_header",
    ancestor_replaced_before_header_directory_openat_is_rejected => "ancestor_before_component",
    ancestor_replaced_after_dirfd_open_keeps_header_anchored => "ancestor_before_leaf",
    unmodified_header_openat_really_reads_bounded_header => "header_openat_positive",
    fifo_replaced_before_header_openat_is_rejected_within_deadline => "fifo_before_header",
    link_replacement_between_calls_reads_no_outside_header => "link_between_calls",
    preexisting_fifo_is_rejected_without_reading => "preexisting_fifo",
    sessions_root_symlink_reads_no_outside_bytes => "sessions_root_link",
    direct_authorized_sessions_root_reads_own_headers => "sessions_root_direct",
    valid_zstd_header_has_bounded_compressed_input => "zstd_regular",
    small_skippable_frame_preserves_valid_zstd_header => "zstd_small_skippable",
    large_skippable_frame_cannot_bypass_header_input_budget => "zstd_large_skippable",
    parent_replacement_before_second_header_open_reads_no_replacement => "parent_before_header_open",
    parent_generation_replacement_does_not_publish_invalid_chain => "parent_generation_during_headers",
    unchanged_parent_chain_has_real_header_io => "parent_chain_positive",
    authorized_home_replaced_by_link_reads_no_other_home => "home_link_before_call",
    unchanged_authorized_home_keeps_real_header_positive => "home_direct_positive",
    sessions_real_directory_replacement_after_check_reads_no_new_entity => "sessions_real_directory_swap",
    checked_sessions_directory_without_replacement_keeps_child => "sessions_real_directory_positive",
    late_directory_member_appearing_after_scan_blocks_publication => "membership_added_before_publish",
    same_name_entity_replacement_before_final_check_blocks_publication => "membership_swapped_before_publish",
    unrelated_body_append_during_scan_keeps_requested_child => "unrelated_body_append_during_scan",
    unrelated_body_append_before_scan_keeps_requested_child => "unrelated_body_append_before_scan",
    normal_zstd_window_has_bounded_header_allocation => "zstd_window_small",
    large_zstd_window_does_not_allocate_unbounded_header_state => "zstd_window_large",
    allocation_observer_captures_real_worker_request => "allocation_observer_positive",
    timeout_supervisor_reaps_deliberately_blocked_process_group => "deadline_control",
}
