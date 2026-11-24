//! 复审 Important-1 回归（Task 4 单开关 R2 门）：选定实例但**无开关记录**
//! （`remote-switch.json` 尚未物化）时，公开授权入口必须拒绝（`DSH_SWITCH_OFF`）
//! ——`gate_authorization` 的 None 分支必须在生产命令壳生效，不得在壳层放行成
//! 绕过窗；生命周期门在该窗放行（落到真实域结果，绝不出现门码）。
//!
//! 第九轮 P1-1 追加：**关闭未收敛窗**（close_pending——等待卸载 / 关闭侧
//! Failed(Revoke)）的生产壳层穿线——`lifecycle_gate_for_selected` 三个直调命令
//! （install/repair/uninstall）必须拒绝（`DSH_SWITCH_ACTIVE`）；真 Off 记录的
//! 直调清理口保留放行（既有语义不回退）。
//!
//! 经**生产命令壳**驱动：独立测试二进制 = 独立进程 = 生产单例（env/registry）
//! 只被本二进制消费；`support::setup()` 把 `HOME`/`TUVIS_HOME` 重定向到一次性
//! 目录（Box::leak 保活），绝不触真实 `~/.tuvis` / `~/.dsh`。本文件的多个场景
//! 收在同一个 `#[tokio::test]` 内顺序执行（生产 registry 的选定实例是进程级
//! 单例，并行用例会互相改写选定面）。

mod support;

use multi_agents_manager_lib::commands::dsh::{
    dsh_binding_arm_intent, dsh_binding_start, dsh_plugin_install, dsh_plugin_repair,
    dsh_plugin_uninstall, dsh_switch_enable, dsh_switch_get, dsh_switch_repair, DshInstanceRequest,
};
use multi_agents_manager_lib::services::dsh_control::instances::HostKind;
use multi_agents_manager_lib::services::dsh_control::model::{ManagementKind, ManagementPhase};
use multi_agents_manager_lib::services::dsh_control::switch::{SwitchPhase, SwitchStep};
use multi_agents_manager_lib::services::dsh_control::{auth, production};

#[tokio::test]
async fn no_record_and_close_pending_windows_gate_public_ipc() {
    support::setup();
    std::env::remove_var("DSH_HOME");
    std::env::remove_var("TUVIS_DSH_STATE_DIR");
    let home = std::env::var("HOME").unwrap();
    let env = production::production_env();
    let state_dir = production::checked_state_dir(&env).unwrap();
    assert!(env.tuvis_home.starts_with(&home), "TUVIS_HOME 未隔离");
    assert!(state_dir.starts_with(&home), "dsh 状态目录未隔离");
    // 登记并选择一个实例，但不生成任何开关记录（不查 dsh_switch_get、不开启）。
    let registry = production::production_registry().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    // 后台卸载作业可能在断言后仍引用目录；泄漏 TempDir 由进程退出回收。
    std::mem::forget(temp);
    let home_dir = root.join("dsh-home");
    let physical = home_dir.join("profiles/desktop");
    std::fs::create_dir_all(&physical).unwrap();
    std::fs::write(physical.join("package.json"), b"{}").unwrap();
    let instance = registry
        .register_candidate_with_home(HostKind::Desktop, &physical, Some(&home_dir))
        .unwrap();
    registry.select(&instance.instance_id).unwrap();

    // 授权激活入口：记录缺席 = 未开启 → DSH_SWITCH_OFF（不得绕过开关激活远控）。
    let err = dsh_binding_start().await.unwrap_err();
    assert!(
        err.starts_with("DSH_SWITCH_OFF"),
        "binding_start 必须被开关门拒绝: {err}"
    );
    let err = dsh_binding_arm_intent(DshInstanceRequest {
        instance_id: instance.instance_id.clone(),
    })
    .await
    .unwrap_err();
    assert!(
        err.starts_with("DSH_SWITCH_OFF"),
        "arm_intent 必须被开关门拒绝: {err}"
    );

    // 生命周期入口不受该窗影响：门放行 → 落到真实域结果，绝不出现门码。
    match dsh_plugin_uninstall().await {
        Ok(_) => {}
        Err(e) => assert!(
            !e.starts_with("DSH_SWITCH_ACTIVE"),
            "生命周期门不得在无记录窗拒绝: {e}"
        ),
    }

    // ===== 第九轮 P1-1：关闭未收敛窗（close_pending）的生产壳层穿线 =====
    // 为选定实例落一份 Failed(Revoke) 开关记录（关闭撤权失败的真实盘面形态），
    // 三个直调生命周期命令必须全拒（直调修复曾把 Failed(Revoke) 洗成可推进态）。
    let record_dir = auth::instance_state_dir(&production::production_env(), &instance.instance_id)
        .expect("实例状态目录解析");
    std::fs::create_dir_all(&record_dir).unwrap();
    let record_path = record_dir.join("remote-switch.json");
    std::fs::write(
        &record_path,
        serde_json::json!({
            "enabled": false,
            "phase": "failed",
            "failedStep": "revoke",
            "generation": 1,
        })
        .to_string(),
    )
    .unwrap();
    let err = dsh_plugin_install().await.unwrap_err();
    assert!(
        err.starts_with("DSH_SWITCH_ACTIVE"),
        "close_pending 态直调 install 必须被生命周期门拒绝: {err}"
    );
    let err = dsh_plugin_repair().await.unwrap_err();
    assert!(
        err.starts_with("DSH_SWITCH_ACTIVE"),
        "close_pending 态直调 repair 必须被生命周期门拒绝: {err}"
    );
    let err = dsh_plugin_uninstall().await.unwrap_err();
    assert!(
        err.starts_with("DSH_SWITCH_ACTIVE"),
        "close_pending 态直调 uninstall 必须被生命周期门拒绝: {err}"
    );

    // 无记录窗也可能启动卸载；用独立实例隔离 F09 的终态回执。
    {
        // F09-A/F09-D：旧版开关记录引用已失败的卸载作业，不能把它显示为排队中。
        let home_dir = root.join("f09-dsh-home");
        let physical = home_dir.join("profiles/desktop");
        std::fs::create_dir_all(&physical).unwrap();
        std::fs::write(physical.join("package.json"), b"{}").unwrap();
        let instance = registry
            .register_candidate_with_home(HostKind::Desktop, &physical, Some(&home_dir))
            .unwrap();
        registry.select(&instance.instance_id).unwrap();
        let record_dir = auth::instance_state_dir(&env, &instance.instance_id).unwrap();
        std::fs::create_dir_all(&record_dir).unwrap();
        let record_path = record_dir.join("remote-switch.json");

        let instance_dir = state_dir.join("instances").join(&instance.instance_id);
        assert_eq!(record_dir, instance_dir);
        let job_id = "legacy-failed-uninstall";
        std::fs::write(
            instance_dir.join("management.json"),
            serde_json::json!({
                "jobs": [{
                    "jobId": job_id,
                    "kind": "uninstall",
                    "phase": "failed",
                    "reasonCode": "DSH_CLI_FAILED",
                    "updatedAtMs": 1,
                }],
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            &record_path,
            serde_json::json!({
                "enabled": false,
                "phase": "failed",
                "failedStep": "uninstall",
                "reasonCode": "DSH_CLI_FAILED",
                "generation": 7,
                "jobId": job_id,
                "jobKind": "uninstall",
            })
            .to_string(),
        )
        .unwrap();

        let request = || DshInstanceRequest {
            instance_id: instance.instance_id.clone(),
        };
        let before = std::fs::read_to_string(&record_path).unwrap();
        let view = dsh_switch_get(Some(request())).await.unwrap();
        let after = std::fs::read_to_string(&record_path).unwrap();
        eprintln!(
            "{} before GET: {before}\nafter GET: {after}",
            record_path.display()
        );
        assert_eq!(view.phase, SwitchPhase::Failed);
        assert_eq!(view.failed_step, Some(SwitchStep::Uninstall));
        assert!(!view.enabled);
        let job = view.job.expect("必须返回账本里的卸载作业");
        assert_eq!(job.phase, ManagementPhase::Failed);
        assert_ne!(job.phase, ManagementPhase::Queued);
        assert_eq!(job.kind, ManagementKind::Uninstall);
        assert_eq!(job.job_id, job_id);
        assert_eq!(job.reason_code.as_deref(), Some("DSH_CLI_FAILED"));

        let err = dsh_switch_enable(request()).await.unwrap_err();
        assert!(err.starts_with("DSH_SWITCH_REVOKE_PENDING"), "{err}");
        let err = dsh_switch_repair(request()).await.unwrap_err();
        assert!(err.starts_with("DSH_SWITCH_ACTIVE"), "{err}");
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
        assert_eq!(persisted["phase"], "failed");
        assert_eq!(persisted["jobId"], job_id);
        assert_eq!(persisted["generation"], 7);
    }
    registry.select(&instance.instance_id).unwrap();

    // 真 Off 记录：直调清理口保留放行（既有语义不回退；落到真实域结果，不出现门码）。
    std::fs::write(
        &record_path,
        serde_json::json!({
            "enabled": false,
            "phase": "off",
            "generation": 2,
        })
        .to_string(),
    )
    .unwrap();
    match dsh_plugin_uninstall().await {
        Ok(_) => {}
        Err(e) => assert!(
            !e.starts_with("DSH_SWITCH_ACTIVE"),
            "生命周期门不得在真 Off 窗拒绝: {e}"
        ),
    }
}
