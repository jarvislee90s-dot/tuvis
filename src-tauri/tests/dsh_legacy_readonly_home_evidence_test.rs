//! 旧登记不保存 dshHome；离线只读使用临时目录与注入的历史指纹探针。
//! 这里不接触生产环境、真实进程、真实授权账本或真实 DSH 实例。

use std::path::Path;
use std::sync::Arc;

use multi_agents_manager_lib::services::dsh_control::error::DshError;
use multi_agents_manager_lib::services::dsh_control::instances::{
    HostKind, InstanceIdentityProbe, InstanceRegistry, SelectedInstance,
};

struct TrustedHistoricalFingerprint;

impl InstanceIdentityProbe for TrustedHistoricalFingerprint {
    fn current_fingerprint(&self, instance: &SelectedInstance) -> Result<Option<String>, DshError> {
        Ok(Some(instance.identity_fingerprint.clone()))
    }
}

fn seed_history(home: &Path, session_id: &str) {
    let session = home.join("sessions/project").join(session_id);
    std::fs::create_dir_all(&session).unwrap();
    std::fs::write(
        session.join("session.jsonl"),
        format!("{{\"type\":\"session\",\"id\":\"{session_id}\"}}\n"),
    )
    .unwrap();
}

// 仅 `#[cfg(unix)]` 用例消费（无证据拒绝面依赖 Unix 符号链接夹具）；
// Windows 交叉目标下不门控会触发 dead_code -D warnings。
#[cfg(unix)]
fn offline_registry(path: &Path) -> InstanceRegistry {
    InstanceRegistry::open(path)
        .unwrap()
        .with_identity_probe(Arc::new(TrustedHistoricalFingerprint))
}

#[test]
fn legacy_direct_profile_keeps_trusted_offline_history_readable() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home-b");
    let profile = home.join("profiles/desktop");
    std::fs::create_dir_all(&profile).unwrap();
    seed_history(&home, "session-bbbbbbbb");
    let path = root.path().join("direct.json");
    use multi_agents_manager_lib::services::dsh_control::{auth, production};
    let env = production::production_env();
    let env = Arc::new(multi_agents_manager_lib::services::dsh_control::model::DshEnvironment {
        home: home.clone(),
        tuvis_home: root.path().join("tuvis"),
        dsh_state_dir: Some(root.path().join("state")),
        processes: Arc::clone(&env.processes),
        files: Arc::clone(&env.files),
        runner: Arc::clone(&env.runner),
        clock: Arc::clone(&env.clock),
        environments: Arc::new(multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments),
    });
    let registry = InstanceRegistry::open_with_env(&path, Some(Arc::clone(&env)))
        .unwrap()
        .with_identity_probe(Arc::new(TrustedHistoricalFingerprint));
    let key = b"fixture-spki";
    let instance = registry
        .register(HostKind::Desktop, &profile, &auth::key_digest(key))
        .unwrap();
    registry.select(&instance.instance_id).unwrap();
    assert_eq!(instance.dsh_home, None);
    let ledger = auth::instance_state_dir(&env, &instance.instance_id)
        .unwrap()
        .join("bindings.json");
    std::fs::create_dir_all(ledger.parent().unwrap()).unwrap();
    let row = serde_json::json!({"bindingId":"direct", "publicKeySpki":auth::b64url_encode(key), "revokedKeySpki":"", "executable":"/fixture/dsh", "pid":1, "startedAtMs":100, "profileDir":profile.to_string_lossy(), "createdAtMs":101, "state":"bound"});
    std::fs::write(
        ledger,
        serde_json::to_vec(&serde_json::json!({"bindings":[row]})).unwrap(),
    )
    .unwrap();
    drop(registry);

    let reopened = InstanceRegistry::open_with_env(&path, Some(env))
        .unwrap()
        .with_identity_probe(Arc::new(TrustedHistoricalFingerprint));
    assert_eq!(
        reopened
            .resolve_readonly_root(&instance.instance_id, "session-bbbbbbbb")
            .unwrap(),
        std::fs::canonicalize(home).unwrap(),
        "没有活体时，受信传统旧登记仍可读取自己的历史"
    );
}

#[cfg(unix)]
#[test]
fn legacy_direct_and_linked_profiles_without_evidence_are_rejected() {
    use multi_agents_manager_lib::services::dsh_control::error::codes;
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let direct_home = root.path().join("direct");
    let linked_home = root.path().join("linked");
    let direct = direct_home.join("profiles/desktop");
    let linked = linked_home.join("profiles/desktop");
    std::fs::create_dir_all(&direct).unwrap();
    std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
    symlink(&direct, &linked).unwrap();
    for (index, profile) in [direct, linked].iter().enumerate() {
        let registry = offline_registry(&root.path().join(format!("registry-{index}.json")));
        let instance = registry
            .register(HostKind::Desktop, profile, "trusted-key")
            .unwrap();
        registry.select(&instance.instance_id).unwrap();
        assert_eq!(
            registry
                .resolve_readonly_root(&instance.instance_id, "session-bbbbbbbb")
                .unwrap_err()
                .code,
            codes::SESSION_OWNERSHIP_UNPROVEN
        );
    }
}

#[cfg(unix)]
#[test]
fn legacy_link_and_direct_registration_lose_the_distinguishing_home_evidence() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let home_a = root.path().join("home-a");
    let home_b = root.path().join("home-b");
    let profile_a = home_a.join("profiles/desktop");
    let profile_b = home_b.join("profiles/desktop");
    std::fs::create_dir_all(home_a.join("profiles")).unwrap();
    std::fs::create_dir_all(&profile_b).unwrap();
    symlink(&profile_b, &profile_a).unwrap();
    seed_history(&home_a, "session-aaaaaaaa");
    seed_history(&home_b, "session-bbbbbbbb");

    let linked_path = root.path().join("linked.json");
    let direct_path = root.path().join("direct.json");
    let linked = offline_registry(&linked_path);
    let direct = offline_registry(&direct_path);
    let a_record = linked
        .register(HostKind::Desktop, &profile_a, "trusted-key")
        .unwrap();
    let b_record = direct
        .register(HostKind::Desktop, &profile_b, "trusted-key")
        .unwrap();
    linked.select(&a_record.instance_id).unwrap();
    direct.select(&b_record.instance_id).unwrap();

    // 两条登记保存在不同文件，但身份、指纹、profile 和选择均相同。
    // 登记本身不包含 A 的线索；实例绑定账本可能提供额外证据。
    assert_eq!(a_record, b_record);
    assert_eq!(a_record.dsh_home, None);
    assert_eq!(
        a_record.profile_dir,
        std::fs::canonicalize(&profile_b).unwrap()
    );
    let a_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&linked_path).unwrap()).unwrap();
    let b_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&direct_path).unwrap()).unwrap();
    assert_eq!(a_json, b_json);
    assert!(a_json["instances"][0].get("dshHome").is_none());
    assert_eq!(linked.selected().unwrap(), direct.selected().unwrap());
}
