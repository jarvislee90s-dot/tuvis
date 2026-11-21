use super::helper::*;

#[test]
fn walker_distinguishes_empty_leaf_from_com_failure() {
    assert_eq!(
        classify_walker_step(false, false),
        WalkerStep::EndOfSiblings
    );
    assert_eq!(classify_walker_step(false, true), WalkerStep::Advanced);
    assert_eq!(classify_walker_step(true, false), WalkerStep::ComFailed);
}

#[test]
fn enumeration_limits_reject_truncation_and_exhausted_budgets() {
    assert!(audit_siblings(64).is_ok());
    assert!(audit_siblings(65).is_err());
    assert!(audit_depth(32).is_ok());
    assert!(audit_depth(33).is_err());
    assert!(audit_budget(1).is_ok());
    assert!(audit_budget(0).is_err());
}

fn identity() -> IdentityBaseline {
    IdentityBaseline {
        host: ProcessIdentityFact {
            pid: 1,
            creation_filetime: 100_001,
            package_full_name: Some("pkg".into()),
            image_path: "host.exe".into(),
        },
        renderer: Some(ProcessIdentityFact {
            pid: 2,
            creation_filetime: 200_001,
            package_full_name: Some("pkg".into()),
            image_path: "renderer.exe".into(),
        }),
        window_rid: vec![1, 2, 3],
    }
}

#[test]
fn rejected_identity_drift_preserves_baseline_and_poisons_each_attempt() {
    use super::binding::SharedObservation;
    use std::time::{Duration, Instant};
    let original = identity();
    let mut changed = original.clone();
    changed.renderer.as_mut().unwrap().creation_filetime += 1;
    assert_eq!(
        original.renderer.as_ref().unwrap().creation_filetime / 10_000,
        changed.renderer.as_ref().unwrap().creation_filetime / 10_000
    );
    let mut slot = Some(original.clone());
    let shared = SharedObservation::new();
    for _ in 0..2 {
        let result = update_identity_baseline(
            &mut slot,
            &changed,
            Instant::now() + Duration::from_secs(1),
            |_, _| panic!("drift must not be accepted as a new baseline"),
            || {
                shared.mark_poisoned().unwrap();
            },
        );
        assert!(result.is_err());
        assert_eq!(slot, Some(original.clone()));
        assert!(shared.is_poisoned().unwrap());
    }
}

#[test]
fn new_observation_can_establish_a_fresh_proven_identity() {
    use super::binding::SharedObservation;
    use std::time::{Duration, Instant};
    let mut changed = identity();
    changed.renderer.as_mut().unwrap().creation_filetime += 1;
    let mut fresh_slot = None;
    let shared = SharedObservation::new();
    let result = update_identity_baseline(
        &mut fresh_slot,
        &changed,
        Instant::now() + Duration::from_secs(1),
        |current, _| Ok(current.clone()),
        || {
            shared.mark_poisoned().unwrap();
        },
    );
    assert!(result.is_ok());
    assert_eq!(fresh_slot, Some(changed));
    assert!(!shared.is_poisoned().unwrap());
}

#[test]
fn identity_revalidation_rejects_each_changed_process_or_window_fact() {
    let baseline = identity();
    assert!(baseline.revalidate(&baseline).is_ok());
    for field in 0..9 {
        let mut live = baseline.clone();
        match field {
            0 => live.host.creation_filetime += 1,
            1 => live.host.pid += 1,
            2 => live.host.package_full_name = None,
            3 => live.host.image_path.push('x'),
            4 => live.renderer.as_mut().unwrap().creation_filetime += 1,
            5 => live.renderer.as_mut().unwrap().pid += 1,
            6 => live.renderer.as_mut().unwrap().image_path.push('x'),
            7 => live.renderer = None,
            _ => live.window_rid.push(4),
        }
        assert!(
            baseline.revalidate(&live).is_err(),
            "identity field {field}"
        );
    }
}

#[test]
fn valid_identity_chain_accepts_package_paths_and_matching_generations() {
    let mut baseline = identity();
    baseline.host.image_path = r"C:\Apps\Pkg\host.exe".into();
    baseline.renderer.as_mut().unwrap().image_path = r"C:\Apps\Pkg\renderer.exe".into();
    let child = baseline.renderer.as_ref().unwrap();
    let hop = ParentHop {
        child_pid: child.pid,
        child_filetime: child.creation_filetime,
        parent_pid: baseline.host.pid,
        parent_filetime: baseline.host.creation_filetime,
    };
    assert!(validate_identity_chain(r"C:\Apps\Pkg", &baseline.host, Some(child), &[hop]).is_ok());
}

#[test]
fn cached_array_exact_i4_tag_calls_reader() {
    let called = std::cell::Cell::new(false);
    let value = with_cached_i4_array(0x2003, || {
        called.set(true);
        Ok(42)
    });
    assert!(called.get());
    assert_eq!(value, Ok(42));
}

#[test]
fn cached_array_invalid_tags_never_call_reader() {
    for vt in [0x6003, 0x2008, 0x0003, 0x0000] {
        let called = std::cell::Cell::new(false);
        let value = with_cached_i4_array(vt, || {
            called.set(true);
            Ok(42)
        });
        assert!(value.is_err());
        assert!(!called.get(), "invalid tag {vt:x} must not read union");
    }
}

#[test]
fn runtime_id_requires_nonempty_cached_identity() {
    assert_eq!(
        decode_cached_runtime_id(&RuntimeIdSource::Cached(vec![1, 2])).unwrap(),
        vec![1, 2]
    );
    assert!(decode_cached_runtime_id(&RuntimeIdSource::Cached(vec![])).is_err());
    assert!(decode_cached_runtime_id(&RuntimeIdSource::LiveOnly).is_err());
}

#[test]
fn subscription_coverage_requires_each_category() {
    let all = [
        SubscriptionKind::StructureChanged,
        SubscriptionKind::PropertyChanged,
        SubscriptionKind::SelectionInvalidated,
        SubscriptionKind::SelectionItemEvent,
    ];
    assert!(audit_subscription_coverage(&all).is_ok());
    for missing in 0..all.len() {
        let remaining: Vec<_> = all
            .iter()
            .enumerate()
            .filter_map(|(i, kind)| (i != missing).then_some(*kind))
            .collect();
        assert!(audit_subscription_coverage(&remaining).is_err());
    }
}

#[test]
fn partial_registration_rollback_is_reverse_order() {
    let mut manifest = SubscriptionManifest::new();
    let first = SubscriptionEntry {
        kind: SubscriptionKind::StructureChanged,
        target_rid: vec![1],
    };
    let second = SubscriptionEntry {
        kind: SubscriptionKind::PropertyChanged,
        target_rid: vec![2],
    };
    manifest.record_success(first.clone());
    manifest.record_success(second.clone());
    assert_eq!(manifest.rollback_plan(), vec![second, first]);
    manifest.clear();
    assert!(manifest.committed().is_empty());
}

#[test]
fn safe_array_length_rejects_overflow_ranges_without_allocation() {
    assert_eq!(checked_array_len(-3, 2, 6), Ok(6));
    assert_eq!(checked_array_len(7, 7, 1), Ok(1));
    assert!(checked_array_len(2, 1, 10).is_err());
    assert!(checked_array_len(i32::MIN, i32::MAX, 64).is_err());
    assert!(checked_array_len(0, 64, 64).is_err());
}
