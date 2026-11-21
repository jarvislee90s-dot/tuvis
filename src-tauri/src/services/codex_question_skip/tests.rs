use super::planner::plan_close;
use super::types::*;

// 合成样本；指纹为 Task1 v2 规范 tuple 的 JSON UTF-8 SHA-256。
pub(super) fn fixture() -> (GroupSelector, ThreadBindingProof, NativeCardSnapshot) {
    let group = GroupIdentity {
        thread_id: "thread-a".into(),
        source_turn_id: "turn-a".into(),
        source_item_id: "item-a".into(),
        content_generation: "generation-a".into(),
    };
    let question = QuestionFingerprint {
        title: "Synthetic question".into(),
        options: vec!["Alpha".into(), "Beta".into()],
    };
    let scope = WindowScope {
        pid: 42,
        started_at_ms: 100,
        hwnd: 123,
        package_fingerprint: "package-a".into(),
        view_path: vec![vec![1, 2], vec![3, 4]],
        scope_epoch: 7,
    };
    let selector = GroupSelector {
        group: group.clone(),
        source_complete: true,
        members: vec![question.clone()],
    };
    let proof = ThreadBindingProof {
        lease_id: "lease-a".into(),
        group,
        scope: scope.clone(),
        card_runtime_id: vec![10, 20],
        card_fingerprint: "d1311f56527c014c2a69100c0f7b033ddb72a2bb24d4190d338029ec74e682fd".into(),
        kind: BindingKind::ExplicitPairing,
        expires_at_ms: 1000,
    };
    let snapshot = NativeCardSnapshot {
        scope,
        complete: true,
        cards: vec![NativeCard {
            runtime_id: vec![10, 20],
            dismiss_runtime_id: vec![10, 20, 30],
            question,
            dismiss_is_direct_child: true,
            invoke_supported: true,
            native_question_count: Some(1),
        }],
    };
    (selector, proof, snapshot)
}

fn assert_refused(plan: ClosePlan) {
    assert!(
        matches!(plan, ClosePlan::Refuse(_)),
        "expected refusal, got {plan:?}"
    );
}

#[test]
fn valid_explicit_binding_invokes_the_observed_direct_dismiss_target() {
    let (selector, proof, snapshot) = fixture();
    assert_eq!(
        plan_close(&selector, &proof, &snapshot, 999),
        ClosePlan::Invoke(InvokeTarget {
            scope: snapshot.scope.clone(),
            card_runtime_id: snapshot.cards[0].runtime_id.clone(),
            dismiss_runtime_id: snapshot.cards[0].dismiss_runtime_id.clone(),
        })
    );
}

#[test]
fn explicit_pairing_selects_its_runtime_id_among_identical_text_cards() {
    let (selector, proof, mut snapshot) = fixture();
    let mut other = snapshot.cards[0].clone();
    other.runtime_id = vec![50];
    other.dismiss_runtime_id = vec![50, 60];
    snapshot.cards.insert(0, other);
    assert_eq!(
        plan_close(&selector, &proof, &snapshot, 999),
        ClosePlan::Invoke(InvokeTarget {
            scope: snapshot.scope.clone(),
            card_runtime_id: vec![10, 20],
            dismiss_runtime_id: vec![10, 20, 30],
        })
    );
}

#[test]
fn duplicate_physical_runtime_ids_are_ambiguous() {
    let (selector, proof, mut snapshot) = fixture();
    snapshot.cards.push(snapshot.cards[0].clone());
    assert_eq!(
        plan_close(&selector, &proof, &snapshot, 999),
        ClosePlan::Refuse(SkipReason::Ambiguous)
    );
}

#[test]
fn lease_is_expired_at_its_exact_boundary() {
    let (selector, proof, snapshot) = fixture();
    assert_eq!(
        plan_close(&selector, &proof, &snapshot, 1000),
        ClosePlan::Refuse(SkipReason::LeaseExpired)
    );
}

#[test]
fn incomplete_source_is_refused() {
    let (mut selector, proof, snapshot) = fixture();
    selector.source_complete = false;
    assert_eq!(
        plan_close(&selector, &proof, &snapshot, 999),
        ClosePlan::Refuse(SkipReason::SourceIncomplete)
    );
}

#[test]
fn multiple_source_members_are_unsupported_even_with_one_native_card() {
    let (mut selector, proof, snapshot) = fixture();
    selector.members.push(QuestionFingerprint {
        title: "Another question".into(),
        options: vec!["One".into()],
    });
    assert_eq!(
        plan_close(&selector, &proof, &snapshot, 999),
        ClosePlan::Refuse(SkipReason::UnsupportedGroup)
    );
}

#[test]
fn independent_context_can_verify_absence_only_without_a_bound_card() {
    let (selector, mut proof, mut snapshot) = fixture();
    proof.kind = BindingKind::IndependentContext;
    proof.card_runtime_id.clear();
    snapshot.cards.clear();
    assert_eq!(
        plan_close(&selector, &proof, &snapshot, 999),
        ClosePlan::NotPresentVerified
    );
}

#[test]
fn disappeared_explicitly_paired_card_is_not_verified_absent() {
    let (selector, proof, mut snapshot) = fixture();
    snapshot.cards.clear();
    assert_refused(plan_close(&selector, &proof, &snapshot, 999));
}

macro_rules! refusal_case {
    ($name:ident, |$selector:ident, $proof:ident, $snapshot:ident| $change:block) => {
        #[test]
        fn $name() {
            #[allow(unused_mut, unused_variables)]
            let (mut $selector, mut $proof, mut $snapshot) = fixture();
            $change
            assert_refused(plan_close(&$selector, &$proof, &$snapshot, 999));
        }
    };
}

refusal_case!(native_count_unknown_is_refused, |s, p, n| {
    n.cards[0].native_question_count = None;
});
refusal_case!(native_count_greater_than_one_is_refused, |s, p, n| {
    n.cards[0].native_question_count = Some(2);
});
refusal_case!(native_count_zero_is_refused, |s, p, n| {
    n.cards[0].native_question_count = Some(0);
});
refusal_case!(incomplete_snapshot_is_refused, |s, p, n| {
    n.complete = false;
});
refusal_case!(
    incomplete_empty_snapshot_cannot_verify_absence,
    |s, p, n| {
        p.kind = BindingKind::IndependentContext;
        p.card_runtime_id.clear();
        n.complete = false;
        n.cards.clear();
    }
);
refusal_case!(non_direct_dismiss_is_refused, |s, p, n| {
    n.cards[0].dismiss_is_direct_child = false;
});
refusal_case!(dismiss_without_invoke_is_refused, |s, p, n| {
    n.cards[0].invoke_supported = false;
});
refusal_case!(changed_native_title_is_refused, |s, p, n| {
    n.cards[0].question.title = "Changed".into();
});
refusal_case!(changed_native_option_order_is_refused, |s, p, n| {
    n.cards[0].question.options.reverse();
});
refusal_case!(changed_source_options_are_refused, |s, p, n| {
    s.members[0].options.reverse();
});
refusal_case!(wrong_proof_fingerprint_is_refused, |s, p, n| {
    p.card_fingerprint = "wrong".into();
});
refusal_case!(
    historical_card_with_other_runtime_id_is_refused,
    |s, p, n| {
        n.cards[0].runtime_id = vec![88];
    }
);
refusal_case!(unbound_same_text_is_not_thread_proof, |s, p, n| {
    p.card_runtime_id.clear();
    p.kind = BindingKind::ExplicitPairing;
});
refusal_case!(different_thread_proof_is_refused, |s, p, n| {
    p.group.thread_id = "thread-b".into();
});
refusal_case!(different_turn_proof_is_refused, |s, p, n| {
    p.group.source_turn_id = "turn-b".into();
});
refusal_case!(different_item_proof_is_refused, |s, p, n| {
    p.group.source_item_id = "item-b".into();
});
refusal_case!(different_generation_proof_is_refused, |s, p, n| {
    p.group.content_generation = "generation-b".into();
});
refusal_case!(old_group_cannot_select_new_group_card, |s, p, n| {
    n.cards[0].runtime_id = vec![90];
    n.cards[0].question.title = "Other group".into();
});
refusal_case!(changed_pid_is_refused, |s, p, n| {
    n.scope.pid += 1;
});
refusal_case!(changed_start_time_is_refused, |s, p, n| {
    n.scope.started_at_ms += 1;
});
refusal_case!(changed_hwnd_is_refused, |s, p, n| {
    n.scope.hwnd += 1;
});
refusal_case!(changed_package_is_refused, |s, p, n| {
    n.scope.package_fingerprint = "package-b".into();
});
refusal_case!(changed_view_path_is_refused, |s, p, n| {
    n.scope.view_path[0].push(99);
});
refusal_case!(changed_scope_epoch_is_refused, |s, p, n| {
    n.scope.scope_epoch += 1;
});
refusal_case!(
    bound_card_absence_under_independent_context_is_unresolved,
    |s, p, n| {
        p.kind = BindingKind::IndependentContext;
        n.cards.clear();
    }
);

#[test]
fn native_count_none_has_specific_reason() {
    let (s, p, mut n) = fixture();
    n.cards[0].native_question_count = None;
    assert_eq!(
        plan_close(&s, &p, &n, 999),
        ClosePlan::Refuse(SkipReason::NativeCountUnknown)
    );
}

#[test]
fn native_counts_zero_and_two_are_unsupported() {
    for count in [0, 2] {
        let (s, p, mut n) = fixture();
        n.cards[0].native_question_count = Some(count);
        assert_eq!(
            plan_close(&s, &p, &n, 999),
            ClosePlan::Refuse(SkipReason::UnsupportedGroup)
        );
    }
}

#[test]
fn empty_lease_requires_binding() {
    let (s, mut p, n) = fixture();
    p.lease_id.clear();
    assert_eq!(
        plan_close(&s, &p, &n, 999),
        ClosePlan::Refuse(SkipReason::BindingRequired)
    );
}

#[test]
fn zero_source_members_are_unsupported() {
    let (mut s, p, n) = fixture();
    s.members.clear();
    assert_eq!(
        plan_close(&s, &p, &n, 999),
        ClosePlan::Refuse(SkipReason::UnsupportedGroup)
    );
}

#[test]
fn empty_or_card_identical_dismiss_is_unavailable() {
    for dismiss in [vec![], vec![10, 20]] {
        let (s, p, mut n) = fixture();
        n.cards[0].dismiss_runtime_id = dismiss;
        assert_eq!(
            plan_close(&s, &p, &n, 999),
            ClosePlan::Refuse(SkipReason::InvokeUnavailable)
        );
    }
}

#[test]
fn independent_context_with_unbound_present_card_is_ambiguous() {
    let (s, mut p, n) = fixture();
    p.kind = BindingKind::IndependentContext;
    p.card_runtime_id.clear();
    assert_eq!(
        plan_close(&s, &p, &n, 999),
        ClosePlan::Refuse(SkipReason::Ambiguous)
    );
}
