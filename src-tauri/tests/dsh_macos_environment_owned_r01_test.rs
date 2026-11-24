//! R01 macOS 真机测试定义：仅向生产 reader 传入本测试 env_clear 后创建的专属子 PID。
//!
//! 默认读取器必须保守返回 Incomplete。原有完整读取正控在 dsh-test-probes 下
//! 注入合成 procargs2 帧和 FullFor 证据；这不证明真实 OS 完整读取能力。
//! 只使用本测试专属 PID；本轮不运行 Cargo。
#![cfg(target_os = "macos")]

#[path = "support/r01_environment_direct.rs"]
mod direct;
#[path = "support/r01_macos_owned_environment.rs"]
mod owned;
#[cfg(feature = "dsh-test-probes")]
#[path = "support/r01_owned_procargs2_source.rs"]
mod source;

use direct::{assert_logs_redacted, assert_redacted, capture_logs};
use multi_agents_manager_lib::services::dsh_control::process_environment::{
    sample_initial_environment, MacOsProcessEnvironmentReader, ParseEnvironmentError,
    ProcessEnvironmentReader, ReadCompleteness, SampleEnvironmentError,
};
use owned::{DshHome, OwnedEnvironmentProcess};

fn assert_default_incomplete(child: &OwnedEnvironmentProcess) {
    let reader = MacOsProcessEnvironmentReader::default();
    let pid = child.pid();
    let (identity, logs) = capture_logs(|| reader.identity_now(pid));
    assert_redacted(&format!("{identity:?}"));
    assert_logs_redacted(&logs);
    let identity = identity
        .expect("生产身份读取失败；不能以忽略或 fallback 掩盖")
        .expect("已握手且未退出的专属子 PID 应存在");
    assert_eq!(identity.pid, pid, "身份必须来自指定专属 PID");
    let mut bytes = [0xa5; 256];
    let extent = reader
        .read_initial_environment_into(pid, &mut bytes)
        .unwrap();
    assert_eq!(extent.completeness, ReadCompleteness::Incomplete);
    assert_eq!(extent.bytes_written, 0);
    let (actual, logs) = capture_logs(|| sample_initial_environment(&reader, &identity));
    assert_redacted(&format!("{actual:?}"));
    assert_logs_redacted(&logs);
    assert_eq!(
        actual,
        Err(SampleEnvironmentError::Parse(
            ParseEnvironmentError::ReadIncomplete
        ))
    );
}

#[cfg(feature = "dsh-test-probes")]
fn sample_owned(
    child: &OwnedEnvironmentProcess,
    dsh_home: Option<&str>,
) -> multi_agents_manager_lib::services::dsh_control::process_environment::InitialEnvironmentObservation{
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    let source = Arc::new(source::OwnedSource::new(
        child.pid(),
        child.expected_home().to_str().unwrap(),
        dsh_home,
    ));
    let reader = MacOsProcessEnvironmentReader::with_source(source.clone());
    let identity = reader.identity_now(child.pid()).unwrap().unwrap();
    let (actual, logs) = capture_logs(|| sample_initial_environment(&reader, &identity));
    assert_redacted(&format!("{actual:?}"));
    assert_logs_redacted(&logs);
    let observation = actual.expect("注入完整证据时专属 PID 应可采样");
    assert_eq!(observation.identity(), &identity);
    assert!(source.reads.load(Ordering::SeqCst) > 0);
    observation
}

#[test]
fn owned_env_clear_child_preserves_its_initial_custom_home_without_exporting_private_records() {
    let child = OwnedEnvironmentProcess::spawn(DshHome::Path);
    assert_default_incomplete(&child);
    #[cfg(feature = "dsh-test-probes")]
    {
        use multi_agents_manager_lib::services::dsh_control::process_environment::{
            AllowlistedEnvironment, EnvironmentField,
        };
        let observation = sample_owned(&child, Some(child.dsh_home.to_str().unwrap()));
        assert_eq!(
            observation.environment(),
            &AllowlistedEnvironment {
                dsh_home: EnvironmentField::Value(child.dsh_home.to_str().unwrap().to_owned()),
                home: EnvironmentField::Value(child.expected_home().to_str().unwrap().to_owned()),
            }
        );
    }
    // feature 正控使用合成帧；日志和返回值核对合成 private canary 不外泄。
    child.finish();
}

#[test]
fn owned_env_clear_child_without_dsh_home_reports_unset_instead_of_inheriting_a_parent_value() {
    let child = OwnedEnvironmentProcess::spawn(DshHome::Unset);
    assert_default_incomplete(&child);
    #[cfg(feature = "dsh-test-probes")]
    {
        use multi_agents_manager_lib::services::dsh_control::process_environment::{
            AllowlistedEnvironment, EnvironmentField,
        };
        let observation = sample_owned(&child, None);
        assert_eq!(
            observation.environment(),
            &AllowlistedEnvironment {
                dsh_home: EnvironmentField::Unset,
                home: EnvironmentField::Value(child.expected_home().to_str().unwrap().to_owned()),
            }
        );
    }
    child.finish();
}

#[test]
fn owned_env_clear_child_with_an_explicitly_empty_dsh_home_reports_empty() {
    let child = OwnedEnvironmentProcess::spawn(DshHome::Empty);
    assert_default_incomplete(&child);
    #[cfg(feature = "dsh-test-probes")]
    {
        use multi_agents_manager_lib::services::dsh_control::process_environment::{
            AllowlistedEnvironment, EnvironmentField,
        };
        let observation = sample_owned(&child, Some(""));
        assert_eq!(
            observation.environment(),
            &AllowlistedEnvironment {
                dsh_home: EnvironmentField::Empty,
                home: EnvironmentField::Value(child.expected_home().to_str().unwrap().to_owned()),
            }
        );
    }
    child.finish();
}
