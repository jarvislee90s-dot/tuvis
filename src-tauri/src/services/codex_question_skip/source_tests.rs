use super::source::{
    binding_fence_for, build_source_snapshot, single_selector, SourceSnapshot, SourceStamp,
};
use super::types::SkipReason;
use crate::services::codex_desktop_ipc::async_question::question_item_id;
use serde_json::{json, Value};

fn stamp() -> SourceStamp {
    SourceStamp {
        owner_lease: Some("lease-a".into()),
        owner_epoch: 1,
        owner_generation: 2,
        mirror_build_generation: 3,
        source_seq: 7,
        stale: false,
        budget_refused: Some(false),
        truncated: Some(false),
        raw_complete: Some(true),
        display_local_seq: 4,
    }
}

fn question(title: &str) -> Value {
    json!({"title": title, "options": ["alpha", {"label": "beta"}]})
}

fn item(id: &str, questions: Value) -> Value {
    json!({"type": "agentMessage", "id": id, "questions": questions})
}

fn state(items: Vec<Value>) -> Value {
    json!({"turnHistory": {"history": {
        "isComplete": true,
        "islands": [{"entries": [{"key": "entry-other", "value": "ref-a"}]}],
        "entitiesByKey": {"ref-a": {"turnId": "turn-a", "items": items}}
    }}})
}

fn snapshot(value: &Value, stamp: &SourceStamp) -> SourceSnapshot {
    build_source_snapshot("thread-a", value, stamp).expect("complete canonical source")
}

fn assert_no_complete_group(value: &Value, stamp: &SourceStamp) {
    if let Ok(result) = build_source_snapshot("thread-a", value, stamp) {
        assert!(
            !result.groups.is_empty(),
            "invalid source must not disappear as empty history"
        );
        assert!(result.groups.iter().all(|g| !g.selector.source_complete));
    }
}

#[test]
fn binding_fence_preserves_canonical_selector_members_digest_and_revision() {
    let source = snapshot(
        &state(vec![item("item-a", json!([question("Full title")]))]),
        &stamp(),
    );
    let group = &source.groups[0];
    let fence = binding_fence_for(&source, &group.selector.group)
        .expect("healthy complete source signs a fence");
    assert_eq!(fence.selector, group.selector);
    assert_eq!(
        fence.question_keys,
        vec![group.members[0].question_key.clone()]
    );
    assert_eq!(
        fence.thread_members_digest,
        source.whole_thread_members_digest
    );
    assert_eq!(fence.revision, source.fence.source_seq);
    assert!(fence.healthy);
}

#[test]
fn binding_fence_rejects_missing_ambiguous_or_unhealthy_group() {
    let source = snapshot(
        &state(vec![item("item-a", json!([question("x")]))]),
        &stamp(),
    );
    let group = source.groups[0].selector.group.clone();
    let mut missing = group.clone();
    missing.source_item_id = "other".into();
    assert!(binding_fence_for(&source, &missing).is_err());
    let mut duplicate = source.clone();
    duplicate.groups.push(duplicate.groups[0].clone());
    assert!(binding_fence_for(&duplicate, &group).is_err());
    let mut unhealthy = source;
    unhealthy.fence.stale = true;
    assert!(binding_fence_for(&unhealthy, &group).is_err());
}

#[test]
fn binding_fence_requires_every_group_identity_field() {
    let source = snapshot(
        &state(vec![item("item-a", json!([question("x")]))]),
        &stamp(),
    );
    let original = source.groups[0].selector.group.clone();
    for field in 0..4 {
        let mut changed = original.clone();
        match field {
            0 => changed.thread_id.push('x'),
            1 => changed.source_turn_id.push('x'),
            2 => changed.source_item_id.push('x'),
            _ => changed.content_generation.push('x'),
        }
        assert!(
            binding_fence_for(&source, &changed).is_err(),
            "identity field {field}"
        );
    }
}

#[test]
fn binding_fence_rejects_multi_question_group() {
    let source = snapshot(
        &state(vec![item(
            "item-a",
            json!([question("first"), question("second")]),
        )]),
        &stamp(),
    );
    assert_eq!(
        binding_fence_for(&source, &source.groups[0].selector.group),
        Err(SkipReason::UnsupportedGroup)
    );
}

#[test]
fn complete_single_question_produces_exact_selector_and_key() {
    let result = snapshot(
        &state(vec![item("item-a", json!([question("Full title")]))]),
        &stamp(),
    );
    assert_eq!(result.groups.len(), 1);
    let group = &result.groups[0];
    assert_eq!(group.declared_count, 1);
    assert_eq!(group.members.len(), 1);
    assert_eq!(group.members[0].index, 0);
    assert_eq!(
        group.members[0].question_key,
        "[\"request_user_input_async\",\"item-a\",0]"
    );
    let selector = single_selector(group).unwrap();
    assert!(selector.source_complete);
    assert_eq!(selector.group.thread_id, "thread-a");
    assert_eq!(selector.group.source_turn_id, "turn-a");
    assert_eq!(selector.group.source_item_id, "item-a");
    assert_eq!(selector.members[0].title, "Full title");
    assert_eq!(selector.members[0].options, vec!["alpha", "beta"]);
    assert_eq!(selector.group.content_generation.len(), 64);
    assert!(selector
        .group
        .content_generation
        .bytes()
        .all(|b| b.is_ascii_hexdigit()));
}

#[test]
fn answered_member_remains_in_multi_question_group() {
    let entries = json!([{"questionItemId":"[\"request_user_input_async\",\"item-a\",0]", "question":"first", "answer":"alpha"}]);
    let answer = format!(
        "<send_user_message_question_reply>\n{entries}\n</send_user_message_question_reply>"
    );
    let value = state(vec![
        item("item-a", json!([question("first"), question("second")])),
        json!({"type":"userMessage", "id":"answer", "content":[{"type":"text", "text":answer}]}),
    ]);
    let result = snapshot(&value, &stamp());
    assert_eq!(result.groups[0].declared_count, 2);
    assert_eq!(result.groups[0].members.len(), 2);
    assert_eq!(
        single_selector(&result.groups[0]),
        Err(SkipReason::UnsupportedGroup)
    );
}

#[test]
fn malformed_second_question_cannot_become_singleton() {
    assert_no_complete_group(
        &state(vec![item("item-a", json!([question("first"), 7]))]),
        &stamp(),
    );
}

#[test]
fn malformed_option_cannot_be_silently_dropped() {
    assert_no_complete_group(
        &state(vec![item(
            "item-a",
            json!([{"title":"x", "options":["ok", 7]}]),
        )]),
        &stamp(),
    );
}

#[test]
fn missing_item_identity_cannot_produce_complete_group() {
    assert_no_complete_group(
        &state(vec![
            json!({"type":"agentMessage", "questions":[question("x")]}),
        ]),
        &stamp(),
    );
}

#[test]
fn duplicate_question_keys_cannot_produce_complete_groups() {
    assert_no_complete_group(
        &state(vec![
            item("same", json!([question("x")])),
            item("same", json!([question("y")])),
        ]),
        &stamp(),
    );
}

#[test]
fn dangling_canonical_reference_cannot_produce_complete_group() {
    let mut value = state(vec![item("item-a", json!([question("x")]))]);
    value["turnHistory"]["history"]["islands"][0]["entries"]
        .as_array_mut()
        .unwrap()
        .push(json!({"value":"missing"}));
    assert_no_complete_group(&value, &stamp());
}

#[test]
fn incomplete_history_and_unhealthy_stamp_cannot_produce_complete_group() {
    let value = state(vec![item("item-a", json!([question("x")]))]);
    let mut truncated = stamp();
    truncated.raw_complete = Some(false);
    assert_no_complete_group(&value, &truncated);
    truncated.raw_complete = None;
    assert_no_complete_group(&value, &truncated);
    let mut budget = stamp();
    budget.budget_refused = Some(true);
    assert_no_complete_group(&value, &budget);
    budget.budget_refused = None;
    assert_no_complete_group(&value, &budget);
    let mut clipped = stamp();
    clipped.truncated = Some(true);
    assert_no_complete_group(&value, &clipped);
    clipped.truncated = None;
    assert_no_complete_group(&value, &clipped);
    let mut stale = stamp();
    stale.stale = true;
    assert_no_complete_group(&value, &stale);
    let mut no_lease = stamp();
    no_lease.owner_lease = None;
    assert_no_complete_group(&value, &no_lease);
    let mut paged = value;
    paged["turnHistory"]["history"]["entitiesByKey"]["ref-a"]["itemsPagination"] =
        json!({"evicted":true});
    assert_no_complete_group(&paged, &stamp());
}

#[test]
fn null_and_absent_questions_and_user_message_are_not_sources() {
    let value = state(vec![
        item("null", Value::Null),
        json!({"type":"agentMessage", "id":"absent"}),
        json!({"type":"userMessage", "id":"fake", "questions":[question("fake")]}),
    ]);
    assert!(snapshot(&value, &stamp()).groups.is_empty());
}

#[test]
fn appended_item_changes_whole_thread_digest_but_not_existing_group_generation() {
    let before = state(vec![item("item-a", json!([question("first")]))]);
    let after = state(vec![
        item("item-a", json!([question("first")])),
        item("item-b", json!([question("second")])),
    ]);
    let a = snapshot(&before, &stamp());
    let b = snapshot(&after, &stamp());
    assert_eq!(b.groups.len(), 2);
    assert_ne!(a.whole_thread_members_digest, b.whole_thread_members_digest);
    assert_eq!(
        a.groups[0].selector.group.content_generation,
        b.groups[0].selector.group.content_generation
    );
    assert!(single_selector(&b.groups[0]).is_ok());
}

#[test]
fn owner_and_mirror_rebuild_revoke_fence_without_changing_content_generation() {
    let mut value = state(vec![item("item-a", json!([question("x")]))]);
    value["contextToken"] = json!("old");
    let a = snapshot(&value, &stamp());
    value["contextToken"] = json!("new");
    let mut changed = stamp();
    changed.owner_lease = Some("lease-b".into());
    changed.owner_epoch += 1;
    changed.owner_generation += 1;
    changed.mirror_build_generation += 1;
    let b = snapshot(&value, &changed);
    assert_eq!(
        a.groups[0].selector.group.content_generation,
        b.groups[0].selector.group.content_generation
    );
    assert_ne!(a.fence, b.fence);
}

#[test]
fn display_only_sequence_does_not_change_source_semantics() {
    let value = state(vec![item("item-a", json!([question("x")]))]);
    let a = snapshot(&value, &stamp());
    let mut changed = stamp();
    changed.display_local_seq += 1;
    assert_eq!(a, snapshot(&value, &changed));
}

#[test]
fn title_ordered_options_and_question_key_each_change_content_generation() {
    let original = snapshot(
        &state(vec![item("item-a", json!([question("x")]))]),
        &stamp(),
    );
    let generation = &original.groups[0].selector.group.content_generation;
    for changed in [
        item("item-a", json!([question("different")])),
        item(
            "item-a",
            json!([{"title":"x", "options":["beta", "alpha"]}]),
        ),
        item("item-b", json!([question("x")])),
    ] {
        let next = snapshot(&state(vec![changed]), &stamp());
        assert_ne!(
            generation,
            &next.groups[0].selector.group.content_generation
        );
    }
}

#[test]
fn referenced_summary_turn_rejects_entire_source() {
    let mut value = state(vec![item("item-a", json!([question("x")]))]);
    value["turnHistory"]["history"]["islands"][0]["entries"]
        .as_array_mut()
        .unwrap()
        .push(json!({"value":"ref-b"}));
    value["turnHistory"]["history"]["entitiesByKey"]["ref-b"] =
        json!({"turnId":"turn-b", "itemsView":"summary", "items":[]});
    assert_eq!(
        build_source_snapshot("thread-a", &value, &stamp()),
        Err(SkipReason::SourceIncomplete)
    );
}

#[test]
fn non_object_pagination_rejects_entire_source() {
    for invalid in [json!(7), json!([])] {
        let mut value = state(vec![item("item-a", json!([question("x")]))]);
        value["turnHistory"]["history"]["entitiesByKey"]["ref-a"]["itemsPagination"] =
            invalid.clone();
        assert_eq!(
            build_source_snapshot("thread-a", &value, &stamp()),
            Err(SkipReason::SourceIncomplete),
            "pagination: {invalid}"
        );
    }
}

#[test]
fn non_boolean_pagination_fields_reject_entire_source() {
    for field in ["evicted", "isLoadingOlder", "hasLoadedOldest"] {
        for invalid in [json!(null), json!(7), json!("false")] {
            let mut value = state(vec![item("item-a", json!([question("x")]))]);
            value["turnHistory"]["history"]["entitiesByKey"]["ref-a"]["itemsPagination"] =
                json!({field: invalid.clone()});
            assert_eq!(
                build_source_snapshot("thread-a", &value, &stamp()),
                Err(SkipReason::SourceIncomplete),
                "{field}: {invalid}"
            );
        }
    }
}

#[test]
fn non_string_items_view_rejects_entire_source() {
    for invalid in [json!(null), json!(7), json!([])] {
        let mut value = state(vec![item("item-a", json!([question("x")]))]);
        value["turnHistory"]["history"]["entitiesByKey"]["ref-a"]["itemsView"] = invalid.clone();
        assert_eq!(
            build_source_snapshot("thread-a", &value, &stamp()),
            Err(SkipReason::SourceIncomplete),
            "itemsView: {invalid}"
        );
    }
}

#[test]
fn omitted_pagination_and_view_are_accepted() {
    let value = state(vec![item("item-a", json!([question("x")]))]);
    assert_eq!(snapshot(&value, &stamp()).groups.len(), 1);
}

#[test]
fn omitted_pagination_flags_are_accepted() {
    let mut value = state(vec![item("item-a", json!([question("x")]))]);
    value["turnHistory"]["history"]["entitiesByKey"]["ref-a"]["itemsPagination"] = json!({});
    assert_eq!(snapshot(&value, &stamp()).groups.len(), 1);
}

#[test]
fn item_without_string_type_rejects_entire_source() {
    for bad_item in [
        json!({"id":"item-b", "questions":[question("bad")]}),
        json!({"type":null, "id":"item-b", "questions":[question("bad")]}),
        json!({"type":7, "id":"item-b", "questions":[question("bad")]}),
    ] {
        let value = state(vec![
            item("item-a", json!([question("x")])),
            bad_item.clone(),
        ]);
        assert_eq!(
            build_source_snapshot("thread-a", &value, &stamp()),
            Err(SkipReason::SourceIncomplete),
            "item: {bad_item}"
        );
    }
}

#[test]
fn user_message_questions_are_ignored_even_when_malformed() {
    let value = state(vec![
        item("item-a", json!([question("x")])),
        json!({"type":"userMessage", "id":"item-b", "questions":7}),
    ]);
    assert_eq!(snapshot(&value, &stamp()).groups.len(), 1);
}

#[test]
fn generation_distinguishes_option_and_question_boundaries() {
    let k1 = question_item_id("item-a", 1);
    let single = state(vec![item(
        "item-a",
        json!([{"title":"A", "options":[k1, "\u{1}", ""]}]),
    )]);
    let multiple = state(vec![item(
        "item-a",
        json!([
            {"title":"A", "options":[]}, {"title":"\u{0}", "options":[]}
        ]),
    )]);
    let a = snapshot(&single, &stamp());
    let b = snapshot(&multiple, &stamp());
    assert_ne!(
        a.groups[0].selector.group.content_generation,
        b.groups[0].selector.group.content_generation
    );
}

#[test]
fn whole_thread_digest_distinguishes_option_and_item_boundaries() {
    let kb = question_item_id("item-b", 0);
    let single = state(vec![item(
        "item-a",
        json!([{
            "title":"A", "options":["turn-a", "item-b", kb, "B"]
        }]),
    )]);
    let multiple = state(vec![
        item("item-a", json!([{"title":"A", "options":[]}])),
        item("item-b", json!([{"title":"B", "options":[]}])),
    ]);
    let a = snapshot(&single, &stamp());
    let b = snapshot(&multiple, &stamp());
    assert_ne!(a.whole_thread_members_digest, b.whole_thread_members_digest);
}
