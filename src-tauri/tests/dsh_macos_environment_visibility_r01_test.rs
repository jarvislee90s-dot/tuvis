//! R01 可见性回归：sysctl 成功、留有容量、argv 后还有记录，均不能证明环境未被省略。
//! 仅替换底层原始读源；MacOsProcessEnvironmentReader 与 sample_initial_environment 均须为生产代码。
//!
//! 注入接口仅存在于默认关闭的 dsh-test-probes 测试支持构建；普通默认构建不编译本文件。
//! 在 src-tauri 目录显式执行：
//! cargo test --features dsh-test-probes --test dsh_macos_environment_visibility_r01_test
//!
//! 下述 raw + visibility 接缝仅用于该 feature 下的独立测试：
//! - MacOsProcargs2Source: Send + Sync，identity_now(pid) 使用现有身份返回类型；
//!   read_procargs2_into(pid, output) -> Result<Procargs2Observation, ReadFailure>；
//! - Procargs2Observation { bytes_written: usize, visibility: EnvironmentVisibility }；
//! - EnvironmentVisibility: Clone，取 Unknown 或 FullFor(ObservedProcessIdentity)；
//! - MacOsProcessEnvironmentReader::with_source(Arc<dyn MacOsProcargs2Source>) -> Self。
//!
//! FullFor 只是此底层编排测试的合成输入，绝不是已经取得的 OS 可见性证明或产品可信状态。
//! 生产当前没有已证来源；默认必须 Unknown，禁止把测试 FullFor 注入产品或据此关闭 R01。
//! 未来若提供生产完整可见性来源，必须由已核实版本的 OS 读取源独立证明：本次读没有 omit_env_vars，
//! 并绑定指定 PID / native unique_id + pid_version 及本次读取的权限上下文；无法证明则 Unknown。
//! 记录内容、尾部 pfz/executable_path、自报标记、成功状态、NUL 或返回长度都不能生成该证明。
//! 传输完整性与可见性是两个条件；FullFor 不能挽救截断或无效 extent。
//! 本轮不跑 Cargo，无真实权限依赖，也不采集任何用户进程环境。
#![cfg(all(
    target_os = "macos",
    target_pointer_width = "64",
    feature = "dsh-test-probes"
))]

#[path = "support/r01_environment_direct.rs"]
mod direct;
#[path = "support/r01_procargs2_visibility_frames.rs"]
mod frames;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use direct::{assert_logs_redacted, assert_redacted, capture_logs};
use multi_agents_manager_lib::services::dsh_control::process_environment::{
    parse_allowlisted_nul, sample_initial_environment, AllowlistedEnvironment, EnvironmentField,
    EnvironmentVisibility, MacOsProcargs2Source, MacOsProcessEnvironmentReader, NativeGeneration,
    ObservedProcessIdentity, Procargs2Observation, ProcessEnvironmentReader, ReadCompleteness,
    ReadFailure,
};

const PID: u32 = 424242;

fn identity() -> ObservedProcessIdentity {
    ObservedProcessIdentity {
        pid: PID,
        started_at_ms: 123_000,
        executable: PathBuf::from("/synthetic/dsh"),
        native_generation: Some(NativeGeneration {
            unique_id: 93,
            pid_version: 7,
        }),
    }
}

/// 只提供原始字节、原始身份和独立的模拟证据；不返回白名单、Complete、Unset 或采样结果。
struct RawSource {
    bytes: Vec<u8>,
    visibility: EnvironmentVisibility,
    fill_capacity: bool,
    reads: AtomicUsize,
}

impl MacOsProcargs2Source for RawSource {
    fn identity_now(&self, pid: u32) -> Result<Option<ObservedProcessIdentity>, ReadFailure> {
        assert_eq!(pid, PID, "不得读取合成目标以外的 PID");
        Ok(Some(identity()))
    }

    fn read_procargs2_into(
        &self,
        pid: u32,
        output: &mut [u8],
    ) -> Result<Procargs2Observation, ReadFailure> {
        assert_eq!(pid, PID, "不得读取合成目标以外的 PID");
        assert!(
            output.len() > self.bytes.len(),
            "低层缓冲区须容纳小型原始夹具并留余量"
        );
        self.reads.fetch_add(1, Ordering::SeqCst);
        output.fill(if self.fill_capacity { 0 } else { 0xa5 });
        output[..self.bytes.len()].copy_from_slice(&self.bytes);
        Ok(Procargs2Observation {
            bytes_written: if self.fill_capacity {
                output.len()
            } else {
                self.bytes.len()
            },
            visibility: self.visibility.clone(),
        })
    }
}

fn reader(
    bytes: Vec<u8>,
    visibility: EnvironmentVisibility,
    fill_capacity: bool,
) -> (MacOsProcessEnvironmentReader, Arc<RawSource>) {
    let source = Arc::new(RawSource {
        bytes,
        visibility,
        fill_capacity,
        reads: AtomicUsize::new(0),
    });
    let reader = MacOsProcessEnvironmentReader::with_source(source.clone());
    (reader, source)
}

fn assert_unusable(reader: &dyn ProcessEnvironmentReader, source: &RawSource) {
    let mut output = [0xa5; 4096];
    let (read, logs) = capture_logs(|| reader.read_initial_environment_into(PID, &mut output));
    assert_redacted(&format!("{read:?}"));
    assert_logs_redacted(&logs);
    if let Ok(extent) = read {
        assert!(extent.bytes_written <= output.len());
        assert_eq!(
            extent.completeness,
            ReadCompleteness::Incomplete,
            "可见性未知或证据不适用，不能输出 Complete"
        );
    }
    let (sample, logs) = capture_logs(|| sample_initial_environment(reader, &identity()));
    assert_redacted(&format!("{sample:?}"));
    assert_logs_redacted(&logs);
    assert!(sample.is_err(), "不得把被省略/未知的白名单字段采信为 Unset");
    assert!(
        source.reads.load(Ordering::SeqCst) > 0,
        "必须实际经过底层原始读取接缝"
    );
}

fn assert_complete(
    reader: &dyn ProcessEnvironmentReader,
    source: &RawSource,
    expected: AllowlistedEnvironment,
) {
    let mut output = [0xa5; 4096];
    let (read, logs) = capture_logs(|| reader.read_initial_environment_into(PID, &mut output));
    assert_redacted(&format!("{read:?}"));
    assert_logs_redacted(&logs);
    let extent = read.expect("适用的合成完整可见性证据应允许正常读取");
    assert!(extent.bytes_written <= output.len());
    assert_eq!(extent.completeness, ReadCompleteness::Complete);
    assert_eq!(
        parse_allowlisted_nul(&output[..extent.bytes_written], extent.completeness),
        Ok(expected.clone())
    );
    let (sample, logs) = capture_logs(|| sample_initial_environment(reader, &identity()));
    assert_redacted(&format!("{sample:?}"));
    assert_logs_redacted(&logs);
    assert_eq!(
        sample.expect("稳定同代正控应能采样").environment(),
        &expected
    );
    assert!(source.reads.load(Ordering::SeqCst) > 0);
}

#[test]
fn omitted_environment_after_empty_argv_zero_cannot_make_hidden_dsh_home_unset() {
    // 返回帧只保留真实环境的第一条 HOME，后面的 DSH_HOME 在内核 omit 分支被省略。
    let (reader, source) = reader(
        frames::omitted_after_first_home(),
        EnvironmentVisibility::Unknown,
        false,
    );
    assert_unusable(&reader, &source);
}

#[test]
fn an_apple_looking_or_self_declared_tail_record_is_not_visibility_evidence() {
    for record in [
        b"pfz=0x1000".as_slice(),
        b"executable_path=/synthetic/dsh".as_slice(),
        b"R01_ENVIRONMENT_VISIBLE=complete".as_slice(),
    ] {
        let (reader, source) = reader(
            frames::omitted_after_first_record(record),
            EnvironmentVisibility::Unknown,
            false,
        );
        assert_unusable(&reader, &source);
    }
}

#[test]
fn even_a_structurally_full_frame_needs_independent_visibility_evidence() {
    // 与下方 home-only 正控使用完全相同的字节；差异只有独立的 source 可见性输入。
    let (reader, source) = reader(
        frames::fully_visible(true),
        EnvironmentVisibility::Unknown,
        false,
    );
    assert_unusable(&reader, &source);
}

#[test]
fn both_allowed_values_in_a_frame_do_not_upgrade_unknown_visibility_to_complete() {
    let (reader, source) = reader(
        frames::fully_visible(false),
        EnvironmentVisibility::Unknown,
        false,
    );
    assert_unusable(&reader, &source);
}

#[test]
fn visibility_evidence_from_another_exec_generation_is_unusable() {
    let mut previous = identity();
    previous.native_generation.as_mut().unwrap().pid_version = 6;
    let (reader, source) = reader(
        frames::fully_visible(true),
        EnvironmentVisibility::FullFor(previous),
        false,
    );
    assert_unusable(&reader, &source);
}

#[test]
fn visibility_evidence_for_another_pid_is_unusable() {
    let mut other = identity();
    other.pid = PID + 1;
    let (reader, source) = reader(
        frames::fully_visible(true),
        EnvironmentVisibility::FullFor(other),
        false,
    );
    assert_unusable(&reader, &source);
}

#[test]
fn visibility_evidence_from_another_unique_id_is_unusable() {
    let mut other = identity();
    other.native_generation.as_mut().unwrap().unique_id = 94;
    let (reader, source) = reader(
        frames::fully_visible(true),
        EnvironmentVisibility::FullFor(other),
        false,
    );
    assert_unusable(&reader, &source);
}

#[test]
fn visibility_evidence_without_a_native_generation_cannot_prove_the_current_image() {
    let mut unbound = identity();
    unbound.native_generation = None;
    let (reader, source) = reader(
        frames::fully_visible(true),
        EnvironmentVisibility::FullFor(unbound),
        false,
    );
    assert_unusable(&reader, &source);
}

#[test]
fn full_visibility_does_not_override_a_successful_read_that_fills_capacity() {
    let (reader, source) = reader(
        frames::fully_visible(true),
        EnvironmentVisibility::FullFor(identity()),
        true,
    );
    assert_unusable(&reader, &source);
}

#[test]
fn independently_full_visibility_allows_genuinely_absent_dsh_home_to_be_unset() {
    // 仅是底层编排的合成正控，不是某个 OS 权限来源已得到验证。
    let (reader, source) = reader(
        frames::fully_visible(true),
        EnvironmentVisibility::FullFor(identity()),
        false,
    );
    assert_complete(
        &reader,
        &source,
        AllowlistedEnvironment {
            dsh_home: EnvironmentField::Unset,
            home: EnvironmentField::Value(frames::HOME.to_owned()),
        },
    );
}

#[test]
fn independently_full_visibility_preserves_both_present_allowed_values() {
    let (reader, source) = reader(
        frames::fully_visible(false),
        EnvironmentVisibility::FullFor(identity()),
        false,
    );
    assert_complete(
        &reader,
        &source,
        AllowlistedEnvironment {
            dsh_home: EnvironmentField::Value(frames::DSH_HOME.to_owned()),
            home: EnvironmentField::Value(frames::HOME.to_owned()),
        },
    );
}
