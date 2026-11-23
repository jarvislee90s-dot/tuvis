use super::*;
use crate::database::dao::opencode_actions as dao;

async fn established() -> (Fixture, Peer, Arc<NativeHub>, NativeTarget, String) {
    let fixture = fixture();
    let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
    let hub = Arc::new(hub(&fixture, peer.port).with_test_ownership_evidence());
    let (view, claim) = hub.view_and_claim(&fixture.card, true).await.expect("建立 A 路由");
    assert!(claim.owned);
    let route = hub.inner.store.with(|c| dao::route_conn(c, SESSION_ID).unwrap().unwrap());
    let target = NativeTarget {
        runtime: RuntimeKey { service_id: route.service_id, pid: route.runtime_pid as u32, started_at_ms: route.runtime_started_at_ms },
        session_id: SESSION_ID.into(), directory: route.directory.into(),
    };
    assert!(hub.binding_matches(&target, &view.binding_token), "请求先验证 A token");
    (fixture, peer, hub, target, view.binding_token)
}

fn seed_action(hub: &NativeHub, id: &str, target: &NativeTarget, state: &str) {
    hub.inner.store.with(|c| {
        c.execute("INSERT INTO opencode_native_actions (action_id,device_id,session_id,service_id,runtime_pid,runtime_started_at_ms,directory,operation,request_key,payload_sha256,occupancy_key,state,created_at,updated_at) VALUES (?1,'device',?2,?3,?4,?5,?6,'answer',?1,'hash',?1,?7,1,1)", rusqlite::params![id, SESSION_ID, target.runtime.service_id, target.runtime.pid, target.runtime.started_at_ms, target.directory.to_string_lossy(), state]).unwrap();
        c.execute("INSERT INTO opencode_native_claims (occupancy_key,action_id,session_id,created_at,updated_at) VALUES (?1,?1,?2,1,1)", rusqlite::params![id, SESSION_ID]).unwrap();
    });
}

type RouteSnapshot = (Option<dao::RouteRow>, (i64, i64), Vec<(String, String, Option<String>, i64)>, Vec<(String, String, i64)>);
fn snapshot(hub: &NativeHub) -> RouteSnapshot {
    hub.inner.store.with(|c| {
        let route = dao::route_conn(c, SESSION_ID).unwrap();
        let versions = c.query_row("SELECT claimed_at,updated_at FROM opencode_native_routes WHERE session_id=?1", [SESSION_ID], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        let mut actions = c.prepare("SELECT action_id,state,reason_code,updated_at FROM opencode_native_actions ORDER BY action_id").unwrap();
        let actions = actions.query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap().map(Result::unwrap).collect();
        let mut claims = c.prepare("SELECT occupancy_key,action_id,updated_at FROM opencode_native_claims ORDER BY occupancy_key").unwrap();
        let claims = claims.query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap().map(Result::unwrap).collect();
        (route, versions, actions, claims)
    })
}

fn switch_a_b_a(hub: &NativeHub, target: &NativeTarget) {
    let mut b = target.clone();
    b.runtime.service_id = "isolated-service-b".into();
    b.runtime.pid += 1;
    hub.inner.store.with(|c| {
        assert!(dao::claim_route_conn(c, &b, true, 100, true).unwrap());
        assert!(dao::claim_route_conn(c, target, true, 200, true).unwrap());
    });
}

async fn wait_for_writer(lock: &Arc<tokio::sync::RwLock<()>>) {
    tokio::time::timeout(WAIT, async {
        while lock.clone().try_read_owned().is_ok() { tokio::task::yield_now().await; }
    }).await.expect("旧请求必须排队等待同会话写锁");
}

async fn legacy_aba(force: bool) {
    let (fixture, _peer, hub, target, _token) = established().await;
    let lock = hub.legacy_lock(SESSION_ID);
    let permit = lock.clone().read_owned().await;
    let old_hub = hub.clone();
    let old_target = target.clone();
    let task = tokio::spawn(async move { old_hub.return_route_for_target(&old_target, force).await });
    wait_for_writer(&lock).await;
    switch_a_b_a(&hub, &target);
    // force 情况下，旧代际未决动作与占用只能在新路由建立后加入。
    seed_action(&hub, "aba-action", &target, if force { "unconfirmed" } else { "confirmed" });
    if force { std::fs::remove_file(&fixture.state_file).unwrap(); }
    let before = snapshot(&hub);
    drop(permit);
    let result = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    let after = snapshot(&hub);
    assert_eq!(after, before, "旧交回不得改动新路由、动作或占用；结果 {result:?}");
    assert!(!matches!(result, Ok((true, _))), "旧交回必须拒绝");
}

#[tokio::test]
async fn stale_legacy_return_cannot_change_route_after_same_runtime_aba() { legacy_aba(false).await; }

#[tokio::test]
async fn stale_force_return_cannot_abandon_actions_after_same_runtime_aba() { legacy_aba(true).await; }

#[tokio::test]
async fn stale_reselection_cannot_change_route_after_same_runtime_aba() {
    let (fixture, _peer, hub, target, token) = established().await;
    let lock = hub.legacy_lock(SESSION_ID);
    let permit = lock.clone().read_owned().await;
    let old_hub = hub.clone();
    let card = fixture.card.clone();
    let task = tokio::spawn(async move { old_hub.reenter_native_route(&card, Some(&token)).await });
    wait_for_writer(&lock).await;
    switch_a_b_a(&hub, &target);
    seed_action(&hub, "reselect-action", &target, "confirmed");
    let before = snapshot(&hub);
    drop(permit);
    let result = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    assert_eq!(snapshot(&hub), before, "旧重选不得改动路由、动作或占用");
    assert!(result.is_err(), "旧重选必须拒绝：{result:?}");
}

#[tokio::test]
async fn confirmed_candidate_is_not_replaced_by_another_service_before_native_selection() {
    let (fixture, a, hub, _target, token_a) = established().await;
    let dir = fixture.project.canonicalize().unwrap();
    let b = Peer::start(Script::new(&dir)).await;
    let c = Peer::start(Script::new(&dir)).await;
    let register = |service: &str, peer: &Peer| {
        std::fs::write(&fixture.state_file, format!(
            r#"{{"id":"{service}","version":"2.0.22","url":"http://127.0.0.1:{}","pid":{PID},"password":"{PASSWORD}"}}"#, peer.port
        )).unwrap();
    };
    register("candidate-b", &b);
    let device_id = "device";
    let preview = hub.preview_candidate(&fixture.card, device_id).await.expect("只读预览 B");
    assert_eq!(preview.target.runtime.service_id, "candidate-b");
    assert_ne!(preview.binding_token, token_a);
    let before = snapshot(&hub);
    let version = before.0.as_ref().unwrap().route_version;
    register("candidate-c", &c);
    let result = hub.reenter_native_route_versioned(&fixture.card, Some(&token_a), Some(version), Some((&preview.preview_token, device_id))).await;
    let after = snapshot(&hub);
    assert_eq!(c.total(), 0, "候选已变化，不能读取或写入 C");
    assert_eq!((a.non_get(), b.non_get(), c.non_get()), (0, 0, 0), "只读选择过程不得向任何服务写入");
    assert_eq!(after, before, "确认 B 后候选变成 C 不得更改旧路由：{result:?}");
    assert!(result.is_err(), "确认 B 后不得选择 C：{result:?}");
}

#[tokio::test]
async fn waiting_selection_rejects_same_runtime_replacement_client() {
    let (fixture, a, hub, _target, token) = established().await;
    let dir = fixture.project.canonicalize().unwrap();
    let replacement = Peer::start(Script::new(&dir)).await;
    let preview = hub.inner.refresh_raw(&fixture.card).await.unwrap();
    let source = preview.source_client.clone();
    let before = snapshot(&hub);
    let version = before.0.as_ref().unwrap().route_version;
    let lock = hub.legacy_lock(SESSION_ID);
    let permit = lock.clone().read_owned().await;
    let h = hub.clone();
    let card = fixture.card.clone();
    let task = tokio::spawn(async move { h.reenter_native_route_versioned(&card, Some(&token), Some(version), None).await });
    wait_for_writer(&lock).await;
    write_registration(&fixture.state_file, replacement.port, PID);
    let next = hub.inner.refresh_raw(&fixture.card).await.expect("同 runtime 的替代 client 可读取");
    assert_eq!(next.target.runtime, preview.target.runtime);
    assert!(!Arc::ptr_eq(&source, &next.source_client), "必须实际替换 client 实例");
    drop(permit);
    let result = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    assert_eq!((a.non_get(), replacement.non_get()), (0, 0));
    assert_eq!(snapshot(&hub), before, "等待时替换来源不得改动路由：{result:?}");
    assert!(result.is_err(), "旧 client 的选择必须拒绝：{result:?}");
}

#[tokio::test]
async fn waiting_selection_rejects_candidate_invalidated_by_401() {
    let (fixture, peer, hub, target, token) = established().await;
    let source = hub.inner.state_lock().source.as_ref().unwrap().clone();
    seed_action(&hub, "candidate-401-action", &target, "confirmed");
    let before = snapshot(&hub);
    let saved_snapshot = hub.inner.state_lock().sessions[SESSION_ID].snapshot.as_ref().unwrap().clone();
    let version = before.0.as_ref().unwrap().route_version;
    let lock = hub.legacy_lock(SESSION_ID);
    let permit = lock.clone().read_owned().await;
    let h = hub.clone();
    let card = fixture.card.clone();
    let task = tokio::spawn(async move { h.reenter_native_route_versioned(&card, Some(&token), Some(version), None).await });
    wait_for_writer(&lock).await;
    peer.set(|script| script.info_status = 401);
    assert!(source.client.read_json(INFO_PATH, &[]).await.is_err());
    assert!(source.client.is_invalidated());
    drop(permit);
    let result = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    assert!(result.is_err(), "失效候选不得认领：{result:?}");
    assert_eq!(snapshot(&hub), before);
    assert!(Arc::ptr_eq(hub.inner.state_lock().sessions[SESSION_ID].snapshot.as_ref().unwrap(), &saved_snapshot));
    assert_eq!(peer.non_get(), 0);
}

#[tokio::test]
async fn invalidated_installed_predecessor_does_not_block_switch_to_valid_candidate() {
    let (fixture, a, hub, target, token) = established().await;
    let source_a = hub.inner.state_lock().source.as_ref().unwrap().clone();
    source_a.client.invalidate();
    assert!(hub.inner.state_lock().source.as_ref().is_some_and(|source| Arc::ptr_eq(source, &source_a)));
    let b = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
    b.set(|script| script.pid = PID + 1);
    write_registration(&fixture.state_file, b.port, PID + 1);
    let preview = hub.preview_candidate(&fixture.card, "device").await.expect("预览 B");
    let before = snapshot(&hub);
    let version = before.0.as_ref().unwrap().route_version;
    let result = hub.reenter_native_route_versioned(&fixture.card, Some(&token), Some(version), Some((&preview.preview_token, "device"))).await.expect("失效前驱不得阻断切换").expect("B 已认领");
    assert_ne!(result.0, token);
    let route = snapshot(&hub).0.unwrap();
    assert_ne!(route.runtime_pid, target.runtime.pid as i64);
    assert_eq!(route.runtime_pid, (PID + 1) as i64);
    let installed = hub.inner.state_lock().source.as_ref().unwrap().clone();
    assert!(!Arc::ptr_eq(&installed, &source_a));
    assert_eq!(installed.descriptor.runtime.pid, PID + 1);
    assert!(!installed.client.is_invalidated());
    assert_eq!((a.non_get(), b.non_get()), (0, 0));
}

#[tokio::test]
async fn waiting_selection_is_rejected_after_pause() {
    let (fixture, peer, hub, _target, token) = established().await;
    let before = snapshot(&hub);
    let version = before.0.as_ref().unwrap().route_version;
    let lock = hub.legacy_lock(SESSION_ID);
    let permit = lock.clone().read_owned().await;
    let h = hub.clone();
    let card = fixture.card.clone();
    let task = tokio::spawn(async move { h.reenter_native_route_versioned(&card, Some(&token), Some(version), None).await });
    wait_for_writer(&lock).await;
    hub.pause();
    drop(permit);
    let result = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    assert_eq!(peer.non_get(), 0);
    assert_eq!(snapshot(&hub), before, "暂停后旧选择不得更改路由：{result:?}");
    assert!(result.is_err(), "暂停后在途选择必须拒绝：{result:?}");
}

#[tokio::test]
async fn force_confirmation_does_not_expand_when_candidates_change() {
    let (fixture, _peer, hub, target, _token) = established().await;
    std::fs::remove_file(&fixture.state_file).unwrap();
    seed_action(&hub, "confirmed-candidate", &target, "unconfirmed");
    assert_eq!(hub.unverifiable_actions(SESSION_ID).unwrap(), 1, "用户确认时仅一项");
    seed_action(&hub, "new-candidate", &target, "unconfirmed");
    let before = snapshot(&hub);
    let result = hub.return_route_for_target(&target, true).await;
    let after = snapshot(&hub);
    let new_action = |s: &RouteSnapshot| {
        (s.2.iter().find(|a| a.0 == "new-candidate").cloned(), s.3.iter().find(|c| c.0 == "new-candidate").cloned())
    };
    assert_eq!(new_action(&after), new_action(&before), "候选变化后不得作废新增动作或释放其占用：{result:?}");
    if !matches!(result, Ok((true, _))) {
        assert_eq!(after, before, "拒绝时路由、动作和占用必须零副作用");
    } else {
        assert!(matches!(result, Ok((true, n)) if n <= 1), "不得扩大作废数量：{result:?}");
    }
}

/// D-N5-3 专用 hub：锁定 pid（≠PID）的死亡经**三形态缝**明确表达
///（NoSuchProcess——G3 收紧后两态缝的 None 只按读取失败处理，不再冒充死亡证明）；
/// PID 本身仍可证存活。模拟「候选应用重启后 secret 消亡，进程事实可辨死亡」。
fn hub_with_dead_lock_fact(fixture: &Fixture, port: u16) -> NativeHub {
    write_registration(&fixture.state_file, port, PID);
    let generation: GenerationFact = Arc::new(|pid: u32| {
        if pid == PID {
            crate::services::opencode_native::actions::ProcessFactProbe::Alive(STARTED_AT_MS)
        } else {
            crate::services::opencode_native::actions::ProcessFactProbe::NoSuchProcess
        }
    });
    NativeHub::with_coordinator_facts(
        DeviceStore::memory(),
        DiscoveryInput {
            state_file: fixture.state_file.clone(),
            home: fixture.home.clone(),
        },
        Arc::new(|pid: u32| (pid == PID).then_some(STARTED_AT_MS)),
        generation,
    )
    .with_test_ownership_evidence()
}

/// D-N5-3（RED→GREEN）：路由行锁定**可证明死亡**的代际 + 携带不再匹配的旧绑定
/// （候选重启后旧 token 不可再生）→ 显式重选（候选预览 + 确认）必须放行，
/// 原子换代登记新绑定；死亡不可证（同 pid 存活 / 事实读取失败）时维持
/// OC_BINDING_STALE（G3：读取失败不再豁免，收紧方向）。
#[tokio::test]
async fn dead_generation_route_lets_explicit_reselect_register_new_binding() {
    let fixture = fixture();
    let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
    let hub = Arc::new(hub_with_dead_lock_fact(&fixture, peer.port));
    let (view, claim) = hub.view_and_claim(&fixture.card, true).await.expect("建立存活路由");
    assert!(claim.owned);
    // 把路由行改写成「死亡代际」：锁定 pid 的进程事实返回 None（pid 消失）。
    let dead = NativeTarget {
        runtime: RuntimeKey {
            service_id: "dead-generation".into(),
            pid: PID + 4_321,
            started_at_ms: STARTED_AT_MS - 1,
        },
        session_id: SESSION_ID.into(),
        directory: fixture.project.clone(),
    };
    hub.inner.store.with(|c| {
        assert!(dao::claim_route_conn(c, &dead, true, 300, true).unwrap(), "换代登记死亡路由");
    });
    let version = snapshot(&hub).0.as_ref().unwrap().route_version;
    // 旧 token（存活代际派生）对死亡路由行必然不再匹配——与候选重启后旧 token
    // 不可再生的 N5 拓扑同形。
    let preview = hub.preview_candidate(&fixture.card, "device").await.expect("只读预览存活候选");
    let result = hub
        .reenter_native_route_versioned(
            &fixture.card,
            Some(&view.binding_token),
            Some(version),
            Some((&preview.preview_token, "device")),
        )
        .await
        .expect("死亡代际的显式重选必须放行")
        .expect("新绑定已登记");
    // 夹具内新旧 token 同源（同一 hub 进程），断言核心 = 路由行确实从死亡代际
    // 原子换代到存活代际；token 与存活目标绑定一致。
    let route = snapshot(&hub).0.unwrap();
    assert_eq!(route.runtime_pid, i64::from(PID), "路由行换代到存活代际");
    assert_eq!(route.runtime_started_at_ms, STARTED_AT_MS);
    assert_eq!(route.route_state, dao::ROUTE_ACTIVE);
    let live = NativeTarget {
        runtime: RuntimeKey {
            service_id: route.service_id.clone(),
            pid: PID,
            started_at_ms: STARTED_AT_MS,
        },
        session_id: SESSION_ID.into(),
        directory: std::path::PathBuf::from(&route.directory),
    };
    assert!(hub.binding_matches(&live, &result.0), "返回的是存活代际绑定");
    assert_eq!(peer.non_get(), 0, "选择过程零服务写入");
}

/// D-N5-3 反例：路由行锁定的代际**存活**（同 pid 同 lstart 可证）但绑定不匹配 →
/// 维持 OC_BINDING_STALE 拒绝，路由零变更（fail-closed 不放宽）。
#[tokio::test]
async fn alive_generation_route_keeps_refusing_reselect_with_foreign_binding() {
    let (fixture, _peer, hub, target, _token) = established().await;
    // 换代登记一个「存活」路由行：锁定 pid=PID+1，桩事实返回 Some(+60s) = 可证存活。
    let alive_foreign = NativeTarget {
        runtime: RuntimeKey {
            service_id: "alive-foreign-generation".into(),
            pid: target.runtime.pid + 1,
            started_at_ms: target.runtime.started_at_ms + 60_000,
        },
        session_id: SESSION_ID.into(),
        directory: fixture.project.clone(),
    };
    hub.inner.store.with(|c| {
        assert!(dao::claim_route_conn(c, &alive_foreign, true, 300, true).unwrap());
    });
    let before = snapshot(&hub);
    let version = before.0.as_ref().unwrap().route_version;
    let preview = hub.preview_candidate(&fixture.card, "device").await.expect("只读预览");
    let result = hub
        .reenter_native_route_versioned(
            &fixture.card,
            Some(&_token),
            Some(version),
            Some((&preview.preview_token, "device")),
        )
        .await;
    let error = result.expect_err("存活代际 + 异主绑定必须维持拒绝");
    assert_eq!(error.code, codes::OC_BINDING_STALE, "{error:?}");
    assert_eq!(snapshot(&hub), before, "拒绝必须零变更");
}

#[cfg(test)]
#[cfg(target_os = "macos")]
mod task1_binding_regressions {
    use super::*;

    #[tokio::test]
    async fn same_tui_generation_with_new_plugin_boot_rejects_old_pin_without_writes() {
        use crate::services::opencode_native::{attribution_evidence::H02_ANCHOR_SHA256, LockedWindow};

        const NEW_BOOT: &str = "boot-n1-restarted-plugin";
        const ECHO_WINDOW_MS: i64 = 2_000;
        let fixture = fixture();
        let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
        peer.arm_probe(TUI_PID);
        let mut hub = hub(&fixture, peer.port);
        let reloaded = Arc::new(AtomicBool::new(false));
        let observation = Arc::clone(&reloaded);
        hub.arm_binding_gate_for_tests(
            Arc::new(move |sid| {
                let boot = if observation.load(Ordering::SeqCst) {
                    NEW_BOOT
                } else {
                    TUI_BOOT
                };
                (sid == SESSION_ID).then(|| LockedWindow {
                    pid: TUI_PID,
                    started_at_ms: TUI_STARTED_AT_MS,
                    boot: Some(boot.to_owned()),
                })
            }),
            Arc::new(|pid| (pid == TUI_PID).then_some(TUI_STARTED_AT_MS)),
            Arc::new(|_| Some(H02_ANCHOR_SHA256.to_owned())),
            Arc::new(|_| Some(H02_ANCHOR_SHA256.to_owned())),
            ECHO_WINDOW_MS,
            10,
            fixture.home.clone(),
        );
        let target = pinned_target(&fixture);
        let (_, claim) = hub
            .view_and_claim(&fixture.card, true)
            .await
            .expect("绑定原插件代际");
        assert!(claim.owned);
        // 正向控制：同一路由、窗口和服务在插件重载前确实拥有写权。
        hub.verify_target_identity(&target)
            .await
            .expect("原插件能够应答回声");
        let pin = || {
            hub.inner.store.with(|conn| {
                crate::database::dao::opencode_pinned_windows::pinned_window_conn(conn, SESSION_ID)
                    .unwrap()
                    .expect("固定窗口必须在场")
            })
        };
        let before_pin = pin();
        assert_eq!(before_pin.window_boot.as_deref(), Some(TUI_BOOT));
        let before_route = snapshot(&hub);
        let before_writes = peer.non_get();
        reloaded.store(true, Ordering::SeqCst);
        // 新插件不消费 expectBoot 指向旧插件的控制文件，旧实现会耗尽回声窗口。
        peer.set(|script| script.probe.as_mut().unwrap().auto_ack = false);
        let started = tokio::time::Instant::now();
        let error = tokio::time::timeout(WAIT, hub.verify_target_identity(&target))
            .await
            .expect("旧绑定必须及时返回")
            .expect_err("插件重载后旧绑定不得通过写权核验");
        assert_eq!(
            error.code,
            codes::OC_BINDING_STALE,
            "同 PID/启动时刻仅 boot 改变：{error:?}；新增服务写入 {} 次",
            peer.non_get() - before_writes,
        );
        assert!(
            started.elapsed() < Duration::from_millis(ECHO_WINDOW_MS as u64),
            "已知旧绑定不应等待完整回声窗口",
        );
        assert_eq!(peer.non_get(), before_writes, "不得创建探针或改写探针标题");
        assert_eq!(pin(), before_pin, "不得自动换绑");
        assert_eq!(snapshot(&hub), before_route, "路由、动作和占用零变更");
    }

    #[tokio::test]
    async fn explicit_reselect_does_not_commit_old_pin_after_previewed_window_leaves_session() {
        use crate::database::dao::opencode_reselect::{self, Receipt};
        use crate::services::opencode_native::{attribution_evidence::H02_ANCHOR_SHA256, LockedWindow};

        const OTHER_SESSION: &str = "ses-preview-switched-to-b";
        const DEVICE: &str = "device";
        const REQUEST: &str = "preview-then-navigate";
        let fixture = fixture();
        let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
        peer.arm_probe(TUI_PID);
        let mut hub = hub(&fixture, peer.port);
        let switched = Arc::new(AtomicBool::new(false));
        let observation = Arc::clone(&switched);
        let fresh: crate::services::opencode_native::binding_gate::LockedWindowLookup =
            Arc::new(move |sid| {
                let opened = if observation.load(Ordering::SeqCst) {
                    OTHER_SESSION
                } else {
                    SESSION_ID
                };
                (sid == opened).then(|| LockedWindow {
                    pid: TUI_PID,
                    started_at_ms: TUI_STARTED_AT_MS,
                    boot: Some(TUI_BOOT.to_owned()),
                })
            });
        hub.arm_binding_gate_for_tests(
            Arc::clone(&fresh),
            Arc::new(|pid| (pid == TUI_PID).then_some(TUI_STARTED_AT_MS)),
            Arc::new(|_| Some(H02_ANCHOR_SHA256.to_owned())),
            Arc::new(|_| Some(H02_ANCHOR_SHA256.to_owned())),
            2_000,
            10,
            fixture.home.clone(),
        );
        let (view, claim) = hub
            .view_and_claim(&fixture.card, true)
            .await
            .expect("窗口打开 A 时固定路由");
        assert!(claim.owned);
        let opened_a = fresh(SESSION_ID).expect("预览前窗口打开 A");
        let preview = hub
            .preview_candidate(&fixture.card, DEVICE)
            .await
            .expect("显式预览 A");
        let before = snapshot(&hub);
        let version = before.0.as_ref().unwrap().route_version;
        assert!(hub.inner.store.with(|conn| {
            opencode_reselect::reserve(conn, DEVICE, SESSION_ID, REQUEST).unwrap()
        }));

        switched.store(true, Ordering::SeqCst);
        assert!(fresh(SESSION_ID).is_none(), "切换后没有新鲜 A 窗口");
        assert_eq!(fresh(OTHER_SESSION), Some(opened_a), "同一窗口改为打开 B");
        // 保持自动回声：服务 SSE 仍可见不等于这扇窗口仍打开 A。
        let result = tokio::time::timeout(
            WAIT,
            hub.reenter_native_route_recorded(
                &fixture.card,
                Some(&view.binding_token),
                Some(version),
                Some((&preview.preview_token, DEVICE)),
                Some((DEVICE, REQUEST)),
            ),
        )
        .await
        .expect("重选必须返回");
        let receipt = hub.inner.store.with(|conn| {
            opencode_reselect::get(conn, DEVICE, SESSION_ID, REQUEST).unwrap()
        });
        if let Some(Receipt::Committed(committed)) = receipt {
            panic!(
                "A 已不可见，不得用旧窗口落已提交回执：pid={}，route_version={}，重选结果={result:?}",
                committed.window.pid, committed.route_version,
            );
        }
        assert!(!matches!(result, Ok(Some(_))), "不得报告重选成功：{result:?}");
        assert_eq!(snapshot(&hub), before, "拒绝不得更改旧路由");
    }
}

// ==== F12 项 1：固定窗口与路由同事务原子提交 ====

/// F12 RED a：显式重选的窗口绑定写入失败 ⇒ 整个认领事务回滚。
///
/// 原子性 = 路由 CAS 与 pinned_windows 行在同一事务：伴表写入被触发器 ABORT 时，
/// 重选必须整体失败、路由行（含 route_version 与 updated_at）零变更——绝不允许
/// 「路由已换代、绑定记录缺失」的半提交态。
#[tokio::test]
async fn pinned_window_write_failure_rolls_back_route_claim_atomically() {
    let fixture = fixture();
    let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
    let mut raw_hub = hub(&fixture, peer.port).with_test_ownership_evidence();
    raw_hub.set_locked_window_provider_for_tests(Arc::new(|sid: &str| {
        (sid == SESSION_ID).then(|| crate::services::opencode_native::LockedWindow {
            pid: TUI_PID,
            started_at_ms: TUI_STARTED_AT_MS,
            boot: Some(TUI_BOOT.to_string()),
        })
    }));
    let hub = Arc::new(raw_hub);
    let (view, claim) = hub.view_and_claim(&fixture.card, true).await.expect("建立路由");
    assert!(claim.owned);
    // 前置：首次认领的选择事件已把窗口身份写入伴表。
    let bound = hub
        .inner
        .store
        .with(|c| {
            crate::database::dao::opencode_pinned_windows::ensure_table_conn(c)?;
            crate::database::dao::opencode_pinned_windows::pinned_window_conn(c, SESSION_ID)
        })
        .unwrap()
        .expect("首次认领必须把窗口身份写入伴表");
    assert_eq!(bound.window_pid, i64::from(TUI_PID));
    // 之后任何伴表写入都失败（触发器 ABORT）→ 重选必须整体失败且路由零变更。
    hub.inner.store.with(|c| {
        c.execute_batch(
            "CREATE TRIGGER pin_fail BEFORE INSERT ON opencode_native_pinned_windows \
             BEGIN SELECT RAISE(ABORT, 'pin-fail'); END;",
        )
    })
    .unwrap();
    let before = snapshot(&hub);
    let version = before.0.as_ref().unwrap().route_version;
    let card = fixture.card.clone();
    let result = hub
        .reenter_native_route_versioned(&card, Some(&view.binding_token), Some(version), None)
        .await;
    assert!(
        result.is_err(),
        "绑定写库失败时重选必须整体失败（不得半提交）：{result:?}"
    );
    assert_eq!(
        snapshot(&hub),
        before,
        "路由行（含 route_version/updated_at）必须随绑定写库失败回滚"
    );
}

/// F12 RED b：提交写入的绑定 = **回声验证通过的窗口身份**（M2 同轮单一身份产物），
/// 不是提交时刻的重新取样。回声结束后（最后一次通道终验处挂起）、提交前把新鲜
/// 观察切到另一扇窗口：伴表必须仍是回声验证的那个窗口，另一窗不入绑定。
#[tokio::test]
#[cfg_attr(not(target_os = "macos"), ignore = "requires macOS OpenCode ownership or process semantics")]
async fn reselect_commits_the_echo_verified_window_identity() {
    let fixture = fixture();
    let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
    let mut raw_hub = hub(&fixture, peer.port).with_test_ownership_evidence();
    peer.arm_probe(TUI_PID);
    // 新鲜观察 provider：翻转前回 W1（两次 verify 的 echo 门锁定），翻转后回 W2
    //（模拟「回声结束后观察切到另一窗口」）。
    let flipped = Arc::new(AtomicBool::new(false));
    let flipped_for_provider = Arc::clone(&flipped);
    let provider: crate::services::opencode_native::binding_gate::LockedWindowLookup =
        Arc::new(move |sid: &str| {
            if sid != SESSION_ID {
                return None;
            }
            if flipped_for_provider.load(Ordering::SeqCst) {
                Some(crate::services::opencode_native::LockedWindow {
                    pid: TUI_ALT_PID,
                    started_at_ms: TUI_ALT_STARTED_AT_MS,
                    boot: None,
                })
            } else {
                Some(crate::services::opencode_native::LockedWindow {
                    pid: TUI_PID,
                    started_at_ms: TUI_STARTED_AT_MS,
                    boot: Some(TUI_BOOT.to_string()),
                })
            }
        });
    // 装配 echo 门（H02 已证构建组合；锁定窗口查询 = 上述 provider）。
    let sha = Arc::new(crate::services::opencode_native::attribution_evidence::H02_ANCHOR_SHA256.to_string());
    let sha_for_tui: crate::services::opencode_native::attribution_evidence::TuiBinarySha = {
        let sha = Arc::clone(&sha);
        Arc::new(move |_pid: u32| Some((*sha).clone()))
    };
    let sha_for_service: crate::services::opencode_native::attribution_evidence::ServiceBinarySha = {
        let sha = Arc::clone(&sha);
        Arc::new(move |_pid: u32| Some((*sha).clone()))
    };
    let alive_pid = TUI_PID;
    let alive_started = TUI_STARTED_AT_MS;
    raw_hub.arm_binding_gate_for_tests(
        provider,
        Arc::new(move |pid: u32| (pid == alive_pid).then_some(alive_started)),
        sha_for_tui,
        sha_for_service,
        5_000,
        10,
        fixture.home.clone(),
    );
    let hub = Arc::new(raw_hub);
    let (view, claim) = hub.view_and_claim(&fixture.card, true).await.expect("建立路由");
    assert!(claim.owned);
    let before = snapshot(&hub);
    let version = before.0.as_ref().unwrap().route_version;
    // 重选在后台跑；在最后一次通道终验（reverify 的 /api/info）挂起处翻转观察。
    let hold = peer.arm_info_hold_after(4);
    let h = Arc::clone(&hub);
    let card = fixture.card.clone();
    let token = view.binding_token.clone();
    let task =
        tokio::spawn(async move { h.reenter_native_route_versioned(&card, Some(&token), Some(version), None).await });
    tokio::time::timeout(WAIT, hold.started.notified())
        .await
        .expect("必须到达最后一次通道终验的挂起闸");
    flipped.store(true, Ordering::SeqCst);
    hold.release.notify_one();
    tokio::time::timeout(WAIT, task)
        .await
        .unwrap()
        .unwrap()
        .expect("回声通过的重选必须成功")
        .expect("新绑定已登记");
    let row = hub
        .inner
        .store
        .with(|c| {
            crate::database::dao::opencode_pinned_windows::ensure_table_conn(c)?;
            crate::database::dao::opencode_pinned_windows::pinned_window_conn(c, SESSION_ID)
        })
        .unwrap()
        .expect("重选必须写入伴表");
    assert_eq!(
        row.window_pid, i64::from(TUI_PID),
        "提交绑定 = 回声验证的窗口（W1），观察翻转后的另一窗（W2）不得入绑定"
    );
    assert_eq!(row.window_boot.as_deref(), Some(TUI_BOOT));
}

/// F12 RED e：route_version 的窗口语义——换窗重选递增、同窗重选不变、
/// 页面刷新（view 认领同代际活跃路由）零版本变化。
#[tokio::test]
async fn reselect_route_version_tracks_window_identity_change() {
    let fixture = fixture();
    let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
    let mut raw_hub = hub(&fixture, peer.port).with_test_ownership_evidence();
    // 新鲜观察：由外部开关控制当前窗口（W1 ⇄ W2）。
    let pid = Arc::new(AtomicUsize::new(TUI_PID as usize));
    let pid_for_provider = Arc::clone(&pid);
    raw_hub.set_locked_window_provider_for_tests(Arc::new(move |sid: &str| {
        (sid == SESSION_ID).then(|| {
            let (window_pid, started) = if pid_for_provider.load(Ordering::SeqCst) == TUI_PID as usize {
                (TUI_PID, TUI_STARTED_AT_MS)
            } else {
                (TUI_ALT_PID, TUI_ALT_STARTED_AT_MS)
            };
            crate::services::opencode_native::LockedWindow {
                pid: window_pid,
                started_at_ms: started,
                boot: Some(TUI_BOOT.to_string()),
            }
        })
    }));
    let hub = Arc::new(raw_hub);
    let version_of = || {
        snapshot(&hub)
            .0
            .as_ref()
            .unwrap()
            .route_version
    };
    let (_, claim) = hub.view_and_claim(&fixture.card, true).await.expect("首次认领");
    assert!(claim.owned);
    let version = version_of();
    let token = {
        let route = snapshot(&hub).0.unwrap();
        let target = NativeTarget {
            runtime: RuntimeKey {
                service_id: route.service_id.clone(),
                pid: route.runtime_pid as u32,
                started_at_ms: route.runtime_started_at_ms,
            },
            session_id: SESSION_ID.into(),
            directory: route.directory.clone().into(),
        };
        hub.inner.binding_token(&target)
    };
    let card = fixture.card.clone();
    // 同窗重选（W1 → W1）：版本不变。
    let _ = hub
        .reenter_native_route_versioned(&card, Some(&token), Some(version), None)
        .await
        .expect("同窗重选成功")
        .expect("绑定在场");
    assert_eq!(
        version_of(),
        version,
        "同窗重选不得递增 route_version"
    );
    // 页面刷新（view 认领同代际活跃路由）：零版本变化。
    let _ = hub.view_and_claim(&fixture.card, true).await.expect("刷新");
    assert_eq!(version_of(), version, "页面刷新零版本变化");
    // 换窗重选（W2）：递增一次。
    pid.store(TUI_ALT_PID as usize, Ordering::SeqCst);
    let _ = hub
        .reenter_native_route_versioned(&card, Some(&token), Some(version), None)
        .await
        .expect("换窗重选成功")
        .expect("绑定在场");
    assert_eq!(
        version_of(),
        version + 1,
        "换窗重选必须递增 route_version（同代际换窗也是路由身份变更）"
    );
}
