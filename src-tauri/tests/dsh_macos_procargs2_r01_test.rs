//! R01 KERN_PROCARGS2 原始帧合同；不调用内核、不读取任何用户进程环境。
//!
//! 直接调用生产 parse_macos_procargs2，不使用替身解析器。
//! 合成输入固定公开 XNU xnu-12377.1.9 的 64 位路径对齐；不声明已验证 Darwin 27 布局。
//! 本轮只迁移测试夹具，不运行 Cargo；保留原 29 项行为断言。
//! buffer.len() 是本次 sysctl 输入容量，bytes_written 是成功回写的长度。
//! 即便以 NUL 结束，等于容量也不能证明完整；argv 后没有环境字节同样不能解释为 Unset。
//! 这些用例验证生产纯解码接缝；真实 sysctl 的接线另见 macOS 专属子 PID 测试。
#![cfg(all(target_os = "macos", target_pointer_width = "64"))]

#[path = "support/r01_environment_direct.rs"]
mod direct;
#[path = "support/r01_macos_procargs2_frame.rs"]
mod frame;

use direct::{assert_logs_redacted, assert_redacted, capture_logs, SECRET_KEY, SECRET_VALUE};
use frame::Frame;
use multi_agents_manager_lib::services::dsh_control::process_environment::{
    parse_macos_procargs2, AllowlistedEnvironment, EnvironmentField, ParseEnvironmentError,
    Procargs2Error,
};

fn fields(dsh_home: EnvironmentField, home: EnvironmentField) -> AllowlistedEnvironment {
    AllowlistedEnvironment { dsh_home, home }
}

fn value(text: &str) -> EnvironmentField {
    EnvironmentField::Value(text.to_owned())
}

fn ordinary_frame() -> Frame {
    Frame::new(
        b"/synthetic/dsh",
        &[b"dsh", b"web"],
        &[b"HOME=/synthetic/user", b"DSH_HOME=/synthetic/profile"],
    )
}

fn decode(buffer: &[u8], bytes_written: usize) -> Result<AllowlistedEnvironment, Procargs2Error> {
    let (actual, logs) = capture_logs(|| parse_macos_procargs2(buffer, bytes_written));
    // 检查真实生产返回值/错误的 Debug 和同步 log；不声称覆盖 DTO、stdout 或异步日志。
    assert_redacted(&format!("{actual:?}"));
    assert_logs_redacted(&logs);
    actual
}

fn decode_frame(frame: &Frame) -> Result<AllowlistedEnvironment, Procargs2Error> {
    decode(&frame.spare_buffer(), frame.bytes.len())
}

#[test]
fn native_endian_argc_separates_arguments_from_environment() {
    let frame = Frame::new(
        b"/synthetic/dsh",
        &[
            b"dsh",
            b"HOME=/synthetic/argv-home",
            b"DSH_HOME=/synthetic/argv-profile",
        ],
        &[b"HOME=/synthetic/user", b"DSH_HOME=/synthetic/profile"],
    );
    assert_eq!(
        decode_frame(&frame),
        Ok(fields(
            value("/synthetic/profile"),
            value("/synthetic/user")
        ))
    );
}

#[test]
fn an_empty_middle_argument_still_consumes_one_argc_slot() {
    let frame = Frame::new(
        b"/synthetic/dsh",
        &[b"dsh", b"", b"DSH_HOME=/synthetic/argv-decoy"],
        &[b"HOME=/synthetic/user"],
    );
    assert_eq!(
        decode_frame(&frame),
        Ok(fields(EnvironmentField::Unset, value("/synthetic/user")))
    );
}

#[test]
fn an_empty_final_argument_does_not_hide_the_first_environment_record() {
    let frame = Frame::new(
        b"/synthetic/dsh",
        &[b"dsh", b"", b""],
        &[b"DSH_HOME=/synthetic/profile"],
    );
    assert_eq!(
        decode_frame(&frame),
        Ok(fields(value("/synthetic/profile"), EnvironmentField::Unset))
    );
}

#[test]
fn executable_path_bytes_are_never_parsed_as_environment() {
    let frame = Frame::new(
        b"DSH_HOME=/synthetic/executable-decoy",
        &[b"dsh"],
        &[b"HOME=/synthetic/user"],
    );
    assert_eq!(
        decode_frame(&frame),
        Ok(fields(EnvironmentField::Unset, value("/synthetic/user")))
    );
}

#[test]
fn consecutive_nuls_after_the_executable_are_alignment_before_argv() {
    // path 含 NUL 长 17，公开 XNU 64 位模型恰好补 7 个 NUL。
    let frame = Frame::new(
        b"/synthetic/dshxx",
        &[b"dsh", b"web"],
        &[b"DSH_HOME=/synthetic/profile"],
    );
    assert_eq!(
        decode_frame(&frame),
        Ok(fields(value("/synthetic/profile"), EnvironmentField::Unset))
    );
}

#[test]
fn nul_terminated_final_environment_record_needs_no_invented_double_nul_sentinel() {
    let frame = ordinary_frame();
    // Frame 最后一条环境记录只有自身的 C 字符串终止符；无额外 envp 指针/长度字段。
    assert_eq!(
        decode_frame(&frame),
        Ok(fields(
            value("/synthetic/profile"),
            value("/synthetic/user")
        ))
    );
}

#[test]
fn explicit_empty_allowed_value_survives_frame_decoding() {
    let frame = Frame::new(
        b"/synthetic/dsh",
        &[b"dsh"],
        &[b"DSH_HOME=", b"HOME=/synthetic/user"],
    );
    assert_eq!(
        decode_frame(&frame),
        Ok(fields(EnvironmentField::Empty, value("/synthetic/user")))
    );
}

#[test]
fn spaces_chinese_and_equals_in_environment_values_survive_frame_decoding() {
    let frame = Frame::new(
        b"/synthetic/dsh",
        &[b"dsh"],
        &["DSH_HOME=/synthetic/中文 home=a=b".as_bytes()],
    );
    assert_eq!(
        decode_frame(&frame),
        Ok(fields(
            value("/synthetic/中文 home=a=b"),
            EnvironmentField::Unset
        ))
    );
}

#[test]
fn a_complete_non_allowlisted_record_can_establish_allowed_keys_are_unset() {
    let frame = Frame::new(
        b"/synthetic/dsh",
        &[b"dsh"],
        &[b"NOT_DSH_HOME=/synthetic/decoy"],
    );
    assert_eq!(
        decode_frame(&frame),
        Ok(fields(EnvironmentField::Unset, EnvironmentField::Unset))
    );
}

#[test]
fn an_argc_header_cut_at_each_byte_is_incomplete() {
    let frame = ordinary_frame();
    let buffer = frame.spare_buffer();
    for written in 0..4 {
        assert_eq!(decode(&buffer, written), Err(Procargs2Error::Incomplete));
    }
}

#[test]
fn negative_argc_is_a_malformed_frame() {
    let mut frame = ordinary_frame();
    frame.set_argc(-1);
    assert_eq!(decode_frame(&frame), Err(Procargs2Error::MalformedFrame));
}

#[test]
fn zero_argc_cannot_establish_an_environment_boundary() {
    let mut frame = ordinary_frame();
    frame.set_argc(0);
    assert_eq!(decode_frame(&frame), Err(Procargs2Error::MalformedFrame));
}

#[test]
fn argc_larger_than_available_strings_is_incomplete() {
    let mut frame = ordinary_frame();
    frame.set_argc(i32::MAX);
    assert_eq!(decode_frame(&frame), Err(Procargs2Error::Incomplete));
}

#[test]
fn a_missing_executable_terminator_is_not_completed_from_buffer_slack() {
    let frame = ordinary_frame();
    let written = 4 + b"/synthetic/dsh".len();
    assert_eq!(
        decode(&frame.spare_buffer(), written),
        Err(Procargs2Error::Incomplete)
    );
}

#[test]
fn executable_alignment_without_argv_is_incomplete() {
    let frame = ordinary_frame();
    let written = frame.argv_start;
    assert_eq!(
        decode(&frame.spare_buffer(), written),
        Err(Procargs2Error::Incomplete)
    );
}

#[test]
fn a_missing_argument_terminator_is_not_completed_from_buffer_slack() {
    let frame = ordinary_frame();
    assert_eq!(
        decode(&frame.spare_buffer(), frame.argv_end - 1),
        Err(Procargs2Error::Incomplete)
    );
}

#[test]
fn successful_short_read_ending_at_argv_does_not_mean_environment_is_unset() {
    let frame = ordinary_frame();
    // 实际 buffer 中仍有环境数据；回写长度停在 argv 的 NUL，必须拒绝越界借用。
    assert_eq!(
        decode(&frame.spare_buffer(), frame.argv_end),
        Err(Procargs2Error::Incomplete)
    );
}

#[test]
fn a_frame_with_environment_physically_omitted_is_incomplete() {
    let frame = Frame::new(b"/synthetic/dsh", &[b"dsh", b"web"], &[]);
    assert_eq!(decode_frame(&frame), Err(Procargs2Error::Incomplete));
}

#[test]
fn increasing_argc_cannot_turn_consumed_environment_into_a_complete_empty_environment() {
    let mut frame = Frame::new(b"/synthetic/dsh", &[b"dsh"], &[b"HOME=/synthetic/user"]);
    frame.set_argc(2);
    assert_eq!(decode_frame(&frame), Err(Procargs2Error::Incomplete));
}

#[test]
fn a_missing_final_environment_nul_is_not_completed_from_buffer_slack() {
    let frame = ordinary_frame();
    assert_eq!(
        decode(&frame.spare_buffer(), frame.bytes.len() - 1),
        Err(Procargs2Error::Environment(
            ParseEnvironmentError::MissingTerminator
        ))
    );
}

#[test]
fn successful_return_length_equal_to_capacity_is_incomplete_even_with_a_final_nul() {
    let frame = ordinary_frame();
    // 等于容量不能凭 syscall success 或末尾 NUL 证明没有被截掉后续环境项。
    assert_eq!(
        decode(&frame.bytes, frame.bytes.len()),
        Err(Procargs2Error::Incomplete)
    );
}

#[test]
fn successful_return_length_equal_to_capacity_at_argv_boundary_is_incomplete() {
    let frame = ordinary_frame();
    assert_eq!(
        decode(&frame.bytes[..frame.argv_end], frame.argv_end),
        Err(Procargs2Error::Incomplete)
    );
}

#[test]
fn a_reported_length_larger_than_the_supplied_capacity_is_invalid() {
    let frame = ordinary_frame();
    assert_eq!(
        decode(&frame.bytes, frame.bytes.len() + 1),
        Err(Procargs2Error::InvalidExtent)
    );
}

#[test]
fn allowed_looking_bytes_outside_the_returned_extent_are_not_environment() {
    let frame = Frame::new(b"/synthetic/dsh", &[b"dsh"], &[b"HOME=/synthetic/user"]);
    let mut buffer = frame.bytes.clone();
    buffer.extend_from_slice(b"DSH_HOME=/synthetic/slack-decoy\0");
    assert_eq!(
        decode(&buffer, frame.bytes.len()),
        Ok(fields(EnvironmentField::Unset, value("/synthetic/user")))
    );
}

#[test]
fn duplicate_allowed_keys_in_environment_are_not_silently_collapsed() {
    let frame = Frame::new(
        b"/synthetic/dsh",
        &[b"dsh"],
        &[b"DSH_HOME=/synthetic/a", b"DSH_HOME=/synthetic/b"],
    );
    assert_eq!(
        decode_frame(&frame),
        Err(Procargs2Error::Environment(
            ParseEnvironmentError::DuplicateKey
        ))
    );
}

#[test]
fn non_utf8_argv_does_not_prevent_decoding_valid_allowed_environment() {
    let frame = Frame::new(
        b"/synthetic/\xff",
        &[b"dsh", b"\xff\xfe"],
        &[b"DSH_HOME=/synthetic/profile"],
    );
    assert_eq!(
        decode_frame(&frame),
        Ok(fields(value("/synthetic/profile"), EnvironmentField::Unset))
    );
}

#[test]
fn invalid_utf8_in_an_allowed_value_is_rejected_without_lossy_conversion() {
    let frame = Frame::new(b"/synthetic/dsh", &[b"dsh"], &[b"DSH_HOME=/synthetic/\xff"]);
    assert_eq!(
        decode_frame(&frame),
        Err(Procargs2Error::Environment(
            ParseEnvironmentError::InvalidEncoding
        ))
    );
}

#[test]
fn non_allowlisted_raw_bytes_are_absent_from_success_results_and_logs() {
    let secret = format!("{SECRET_KEY}={SECRET_VALUE}");
    let frame = Frame::new(
        b"/synthetic/dsh",
        &[b"dsh", secret.as_bytes()],
        &[
            secret.as_bytes(),
            b"PRIVATE_BINARY=\xff\xfe",
            b"DSH_HOME=/synthetic/profile",
        ],
    );
    assert_eq!(
        decode_frame(&frame),
        Ok(fields(value("/synthetic/profile"), EnvironmentField::Unset))
    );
}

#[test]
fn non_allowlisted_raw_bytes_are_absent_from_error_results_and_logs() {
    let secret = format!("{SECRET_KEY}={SECRET_VALUE}");
    let frame = Frame::new(
        b"/synthetic/dsh",
        &[b"dsh"],
        &[secret.as_bytes(), b"DSH_HOME=\xff"],
    );
    assert_eq!(
        decode_frame(&frame),
        Err(Procargs2Error::Environment(
            ParseEnvironmentError::InvalidEncoding
        ))
    );
}
