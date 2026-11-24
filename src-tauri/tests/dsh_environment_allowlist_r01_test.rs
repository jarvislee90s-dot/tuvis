//! R01 初始环境白名单的直接接口测试：仅使用合成字节，不读取用户进程环境。
//!
//! process_environment 生产接口尚不存在；编译阻塞不属于行为 RED。
//! 纯解析用例不再 ignore、不经过 driver；home/真实 DTO 消费者仍明确未接线。
//! 重复白名单键按保守拒绝；初始环境结果不代表有效 profile 或可信宿主。

#[path = "support/r01_environment_direct.rs"]
mod direct;
#[path = "support/r01_environment_driver.rs"]
mod driver;

use direct::{assert_logs_redacted, assert_redacted, capture_logs, SECRET_KEY, SECRET_VALUE};
use driver::{complete, Driver};
use multi_agents_manager_lib::services::dsh_control::process_environment::{
    parse_allowlisted_nul, AllowlistedEnvironment, EnvironmentField, ParseEnvironmentError,
    ReadCompleteness,
};
use serde_json::{json, Value};

fn fields(dsh_home: EnvironmentField, home: EnvironmentField) -> AllowlistedEnvironment {
    AllowlistedEnvironment { dsh_home, home }
}

fn value(text: &str) -> EnvironmentField {
    EnvironmentField::Value(text.to_owned())
}

macro_rules! parse_case {
    ($name:ident, $bytes:expr, $completeness:expr, $expected:expr) => {
        #[test]
        fn $name() {
            let (actual, logs) = capture_logs(|| parse_allowlisted_nul($bytes, $completeness));
            assert_redacted(&format!("{actual:?}"));
            assert_logs_redacted(&logs);
            assert_eq!(actual, $expected);
        }
    };
}

parse_case!(
    nul_records_preserve_spaces_and_chinese_in_allowed_values,
    "DSH_HOME=/tmp/中文 自定义 home\0HOME=/tmp/用户 home\0".as_bytes(),
    ReadCompleteness::Complete,
    Ok(fields(
        value("/tmp/中文 自定义 home"),
        value("/tmp/用户 home")
    ))
);
parse_case!(
    equals_after_the_first_separator_belong_to_the_value,
    b"DSH_HOME=/tmp/a=b=c\0",
    ReadCompleteness::Complete,
    Ok(fields(value("/tmp/a=b=c"), EnvironmentField::Unset))
);
parse_case!(
    whitespace_does_not_create_an_additional_environment_record,
    b"DSH_HOME=/tmp/a HOME=/tmp/not-another-key\0",
    ReadCompleteness::Complete,
    Ok(fields(
        value("/tmp/a HOME=/tmp/not-another-key"),
        EnvironmentField::Unset
    ))
);
parse_case!(
    newline_inside_a_value_is_preserved,
    b"DSH_HOME=/tmp/line\nbreak\0",
    ReadCompleteness::Complete,
    Ok(fields(value("/tmp/line\nbreak"), EnvironmentField::Unset))
);
parse_case!(
    surrounding_spaces_in_a_value_are_not_trimmed,
    b"DSH_HOME= /tmp/home \0",
    ReadCompleteness::Complete,
    Ok(fields(value(" /tmp/home "), EnvironmentField::Unset))
);
parse_case!(
    similarly_named_keys_do_not_match_the_allowlist,
    b"NOT_DSH_HOME=/tmp/a\0DSH_HOME_SUFFIX=/tmp/b\0dsh_home=/tmp/c\0",
    ReadCompleteness::Complete,
    Ok(fields(EnvironmentField::Unset, EnvironmentField::Unset))
);
parse_case!(
    explicit_empty_dsh_home_is_distinct_from_unset,
    b"DSH_HOME=\0",
    ReadCompleteness::Complete,
    Ok(fields(EnvironmentField::Empty, EnvironmentField::Unset))
);
parse_case!(
    complete_environment_without_dsh_home_reports_unset,
    b"HOME=/tmp/user\0",
    ReadCompleteness::Complete,
    Ok(fields(EnvironmentField::Unset, value("/tmp/user")))
);
parse_case!(
    empty_but_complete_environment_reports_unset,
    b"",
    ReadCompleteness::Complete,
    Ok(fields(EnvironmentField::Unset, EnvironmentField::Unset))
);
parse_case!(
    conflicting_duplicate_allowed_keys_are_rejected,
    b"DSH_HOME=/tmp/first\0DSH_HOME=/tmp/second\0",
    ReadCompleteness::Complete,
    Err(ParseEnvironmentError::DuplicateKey)
);
parse_case!(
    identical_duplicate_allowed_keys_are_not_silently_collapsed,
    b"DSH_HOME=/tmp/same\0DSH_HOME=/tmp/same\0",
    ReadCompleteness::Complete,
    Err(ParseEnvironmentError::DuplicateKey)
);
parse_case!(
    an_empty_duplicate_cannot_override_an_allowed_value,
    b"DSH_HOME=/tmp/first\0DSH_HOME=\0",
    ReadCompleteness::Complete,
    Err(ParseEnvironmentError::DuplicateKey)
);
parse_case!(
    an_unterminated_record_is_incomplete_rather_than_unset,
    b"HOME=/tmp/user\0DSH_HOME=/tmp/partial",
    ReadCompleteness::Complete,
    Err(ParseEnvironmentError::MissingTerminator)
);
parse_case!(
    reader_truncation_is_incomplete_even_at_a_nul_boundary,
    b"HOME=/tmp/user\0",
    ReadCompleteness::Incomplete,
    Err(ParseEnvironmentError::ReadIncomplete)
);
parse_case!(
    malformed_allowed_record_is_not_an_unset_key,
    b"DSH_HOME\0",
    ReadCompleteness::Complete,
    Err(ParseEnvironmentError::MalformedRecord)
);
parse_case!(
    invalid_utf8_in_an_allowed_value_is_not_lossily_replaced,
    b"DSH_HOME=/tmp/\xff\0",
    ReadCompleteness::Complete,
    Err(ParseEnvironmentError::InvalidEncoding)
);
parse_case!(
    invalid_utf8_outside_the_allowlist_does_not_corrupt_allowed_values,
    b"PRIVATE_BINARY=\xff\xfe\0DSH_HOME=/tmp/ok\0",
    ReadCompleteness::Complete,
    Ok(fields(value("/tmp/ok"), EnvironmentField::Unset))
);

#[test]
fn non_allowlisted_bytes_are_absent_from_parser_result_and_actual_logs() {
    let bytes = format!("{SECRET_KEY}={SECRET_VALUE}\0DSH_HOME=/tmp/allowed\0");
    let (actual, logs) =
        capture_logs(|| parse_allowlisted_nul(bytes.as_bytes(), ReadCompleteness::Complete));
    assert_redacted(&format!("{actual:?}"));
    assert_logs_redacted(&logs);
    assert_eq!(
        actual,
        Ok(fields(value("/tmp/allowed"), EnvironmentField::Unset))
    );
}

#[test]
#[ignore = "未接线：真实 DTO 投影及输出消费者；直接解析和日志断言不替代此面"]
fn non_allowlisted_environment_never_appears_in_any_observable_output() {
    let bytes = format!("{SECRET_KEY}={SECRET_VALUE}\0DSH_HOME=/tmp/allowed\0");
    let actual = driver::parse(complete(bytes));
    assert_eq!(
        actual["result"],
        driver::fields(driver::value("/tmp/allowed"), driver::unset())
    );
}

#[test]
#[ignore = "未接线：真实 DTO/错误转换消费者；底层读取失败另有直接接口用例"]
fn a_failed_read_does_not_echo_raw_environment_in_error_details() {
    let actual = driver::parse(json!({
        "status": "failed", "kind": "io",
        "diagnostic": format!("{SECRET_KEY}={SECRET_VALUE}"),
        "bytes": format!("{SECRET_KEY}={SECRET_VALUE}\0").as_bytes(),
    }));
    assert_eq!(
        actual["result"],
        json!({ "status": "failed", "reason": "read_failed" })
    );
}

fn resolve(read: Value, cwd: Value, user_home: &std::path::Path) -> Value {
    Driver::start(json!({
        "op": "resolve_initial_home", "read": read,
        "cwd": cwd, "userHome": user_home,
    }))
    .finish()["result"]
        .clone()
}

fn resolved(home: &std::path::Path) -> Value {
    json!({
        "status": "resolved_initial", "homeRoot": home,
        "evidence": "initial_environment", "verified": false,
    })
}

#[test]
#[ignore = "未接线：生产初始 home 消费者；不属于纯字节解析"]
fn unset_dsh_home_uses_the_independently_supplied_target_user_home() {
    let dir = tempfile::tempdir().unwrap();
    let expected = dir.path().join(".dsh");
    std::fs::create_dir(&expected).unwrap();
    let expected = std::fs::canonicalize(expected).unwrap();
    assert_eq!(
        resolve(complete(b""), Value::Null, dir.path()),
        resolved(&expected)
    );
}

#[test]
#[ignore = "未接线：生产初始 home 消费者；不属于纯字节解析"]
fn explicitly_empty_dsh_home_does_not_fall_back_to_default_home() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".dsh")).unwrap();
    assert_eq!(
        resolve(complete(b"DSH_HOME=\0"), Value::Null, dir.path()),
        json!({ "status": "unresolved", "reason": "empty_home" })
    );
}

#[test]
#[ignore = "未接线：读取失败到 home 回退的真实消费者链路"]
fn failed_environment_read_does_not_fall_back_to_an_existing_default_home() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".dsh")).unwrap();
    assert_eq!(
        resolve(
            json!({ "status": "failed", "kind": "permission_denied" }),
            Value::Null,
            dir.path()
        ),
        json!({ "status": "failed", "reason": "read_failed" })
    );
}

#[test]
#[ignore = "未接线：生产初始 home 消费者；不属于纯字节解析"]
fn relative_dsh_home_resolves_against_target_cwd_instead_of_test_or_tuvis_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("目标 cwd");
    let home = cwd.join("中文 home");
    std::fs::create_dir_all(&home).unwrap();
    let expected = std::fs::canonicalize(home).unwrap();
    assert_eq!(
        resolve(complete("DSH_HOME=./中文 home\0"), json!(cwd), dir.path()),
        resolved(&expected)
    );
}

#[test]
#[ignore = "未接线：生产初始 home 消费者；不属于纯字节解析"]
fn relative_dsh_home_without_target_cwd_remains_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        resolve(complete(b"DSH_HOME=./relative\0"), Value::Null, dir.path()),
        json!({ "status": "unresolved", "reason": "missing_cwd" })
    );
}

#[cfg(unix)]
#[test]
#[ignore = "未接线：生产初始 home 消费者；不属于纯字节解析"]
fn symlink_home_is_canonicalized_without_becoming_verified_profile_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let actual = dir.path().join("真实 中文 home");
    let alias = dir.path().join("home alias");
    std::fs::create_dir(&actual).unwrap();
    std::os::unix::fs::symlink(&actual, &alias).unwrap();
    let read = complete(format!("DSH_HOME={}\0", alias.display()));
    let expected = std::fs::canonicalize(actual).unwrap();
    assert_eq!(resolve(read, Value::Null, dir.path()), resolved(&expected));
}
