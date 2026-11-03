//! 估算速度独立数据集：仅使用临时测试账本。
mod support;
use multi_agents_manager_lib::services::tps::v2::{model::SpeedDashboardQuery, query::dashboard};
use rusqlite::{params, Connection};
use serde_json::json;

const NOW: i64 = 1_700_000_000_000;
fn insert(c: &Connection, item: &str, profile: &str, tokens: i64, duration_ms: i64, end: i64) {
    c.execute("INSERT INTO tps_body_estimate(host_key,thread_key,turn_key,item_key,reference_profile_id,model,channel,model_source,reference_tokens,duration_ns,ended_at_ms,run_id,source_build,short_window,small_sample) VALUES ('h','s','t',?1,?2,'gpt-test',NULL,'request',?3,?4,?5,'r','b',0,0)",params![item,profile,tokens,duration_ms*1_000_000,end]).unwrap();
}
fn query() -> SpeedDashboardQuery {
    serde_json::from_value(json!({"range":"1h","selection":null,"metricMode":"tokenizer-estimate","toolId":null,"search":"","sort":"tool"})).expect("third mode wire contract")
}
#[test]
fn estimate_weighted_sums_and_boundary_buckets() {
    let c = support::open_ledger_db("estimate-weighted");
    insert(&c, "a", "p1", 100, 4000, NOW - 1000);
    insert(&c, "b", "p1", 300, 6000, NOW);
    insert(&c, "excluded", "p1", 900, 1000, NOW - 3_600_000);
    let result = serde_json::to_value(dashboard(&c, &query(), NOW).unwrap()).unwrap();
    let series = &result["series"][0];
    assert_eq!(series["seriesKey"]["referenceProfileId"], "p1");
    assert_eq!(series["metrics"]["tokenizerEstimate"]["value"], 40.0);
    assert_eq!(series["metrics"]["tokenizerEstimate"]["sampleCount"], 2);
    assert_eq!(series["metrics"]["bodyObservedMs"].as_f64(), Some(10000.0));
    assert_eq!(
        series["metrics"]["stream"]["value"],
        serde_json::Value::Null
    );
    assert_eq!(
        series["metrics"]["response"]["value"],
        serde_json::Value::Null
    );
    let bucket_n: i64 = result["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            b["metrics"]["tokenizerEstimate"]["sampleCount"]
                .as_i64()
                .unwrap()
        })
        .sum();
    assert_eq!(bucket_n, 2);
}
#[test]
fn estimate_profiles_remain_separate_and_explicit_missing_selection_is_kept() {
    let c = support::open_ledger_db("estimate-profiles");
    insert(&c, "a", "p1", 100, 4000, NOW - 2);
    insert(&c, "b", "p2", 300, 6000, NOW - 1);
    let mut q = query();
    let result = serde_json::to_value(dashboard(&c, &q, NOW).unwrap()).unwrap();
    assert_eq!(result["series"].as_array().unwrap().len(), 2);
    q.selection = Some(serde_json::from_value(json!({"tool":"codex","model":"missing","channel":null,"method":"ipc-body-reference","referenceProfileId":"p1"})).unwrap());
    let missing = serde_json::to_value(dashboard(&c, &q, NOW).unwrap()).unwrap();
    assert_eq!(missing["selectedSeries"]["model"], "missing");
    assert!(missing["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .all(|b| b["metrics"]["tokenizerEstimate"]["value"].is_null()));
}
#[test]
fn estimate_capability_and_source_states_are_independent() {
    let c = support::open_ledger_db("estimate-source-state");
    let result = serde_json::to_value(dashboard(&c, &query(), NOW).unwrap()).unwrap();
    let codex = result["sourceStates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source"] == "codex")
        .unwrap();
    assert_eq!(codex["capabilities"]["response"], true);
    assert_eq!(
        codex["capabilities"]["tokenizerEstimate"],
        cfg!(target_os = "macos")
    );
    assert_eq!(
        codex["estimateState"]["supported"],
        cfg!(target_os = "macos")
    );
    assert_eq!(codex["estimateState"]["enabled"], true);
    assert_eq!(
        codex["estimateState"]["lastRecordAt"],
        serde_json::Value::Null
    );
    c.execute("INSERT INTO tps_state(key,value_json,updated_at) VALUES('tokenizer-estimate:enabled','false',0)", []).unwrap();
    insert(&c, "old", "p1", 100, 4000, NOW - 1);
    let stopped = serde_json::to_value(dashboard(&c, &query(), NOW).unwrap()).unwrap();
    let codex = stopped["sourceStates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source"] == "codex")
        .unwrap();
    assert_eq!(codex["estimateState"]["enabled"], false);
    assert_eq!(codex["estimateState"]["lastRecordAt"], NOW - 1);
    assert_eq!(stopped["series"].as_array().unwrap().len(), 1);
}
#[test]
fn native_and_estimate_facts_never_mix_and_short_counts_survive() {
    let c = support::open_ledger_db("estimate-isolation");
    insert(&c, "a", "p1", 10, 134, NOW - 100);
    c.execute(
        "UPDATE tps_body_estimate SET short_window=1, small_sample=1 WHERE item_key='a'",
        [],
    )
    .unwrap();
    insert(&c, "b", "p1", 100, 500, NOW - 1);
    c.execute("INSERT INTO tps_observation(source,host_instance,stable_call_key,started_at_ms,ended_at_ms,generation_ms,response_elapsed_ms,output_tokens,output_scope,model,purpose,outcome,method) VALUES('codex','h','native',?1,?2,1000,2000,948,'providerReported','gpt-test','mainTask','success','log-boundary-approx')",params![NOW-2000,NOW-1]).unwrap();
    let estimate = serde_json::to_value(dashboard(&c, &query(), NOW).unwrap()).unwrap();
    let m = &estimate["series"][0]["metrics"];
    assert_eq!(m["tokenizerEstimate"]["outputTokens"], 110);
    assert_eq!(m["shortWindowCount"], 1);
    assert_eq!(m["smallSampleCount"], 1);
    assert_eq!(m["outputScope"], "visibleBody");
    let mut native = query();
    native.metric_mode = serde_json::from_value(json!("response")).unwrap();
    let native = serde_json::to_value(dashboard(&c, &native, NOW).unwrap()).unwrap();
    assert_eq!(native["series"].as_array().unwrap().len(), 1);
    assert_eq!(
        native["series"][0]["metrics"]["response"]["outputTokens"],
        948
    );
    assert_eq!(
        native["series"][0]["metrics"]["tokenizerEstimate"]["sampleCount"],
        0
    );
    assert!(native["series"][0]["seriesKey"]["referenceProfileId"].is_null());
}
#[test]
fn old_series_key_deserializes_without_profile() {
    use multi_agents_manager_lib::services::tps::model::SeriesKey;
    let key: SeriesKey = serde_json::from_value(
        json!({"tool":"codex","model":"gpt","channel":null,"method":"log-boundary-approx"}),
    )
    .unwrap();
    assert_eq!(key.reference_profile_id, None);
}
#[test]
fn submillisecond_duration_remains_positive_in_public_rate() {
    let c = support::open_ledger_db("estimate-submillisecond");
    insert(&c, "tiny", "p1", 1, 1, NOW);
    c.execute(
        "UPDATE tps_body_estimate SET duration_ns=500000 WHERE item_key='tiny'",
        [],
    )
    .unwrap();
    let result = serde_json::to_value(dashboard(&c, &query(), NOW).unwrap()).unwrap();
    let m = &result["series"][0]["metrics"];
    assert_eq!(m["tokenizerEstimate"]["durationMs"].as_f64(), Some(0.5));
    assert_eq!(m["tokenizerEstimate"]["value"].as_f64(), Some(2000.0));
}
#[test]
fn legacy_native_series_identity_does_not_change() {
    use multi_agents_manager_lib::services::tps::model::SeriesKey;
    let key: SeriesKey = serde_json::from_value(
        json!({"tool":"codex","model":"gpt","channel":null,"method":"log-boundary-approx"}),
    )
    .unwrap();
    assert_eq!(
        key.id(),
        r#"{"tool":"codex","model":"gpt","channel":null,"method":"log-boundary-approx"}"#
    );
}

#[test]
fn single_body_demo_uses_last_growth_not_late_confirmation() {
    // 这是合成演示值，不是实机测量值。确认时刻设为增长后 682 ms。
    let c = support::open_ledger_db("estimate-late-confirmation");
    let last_growth = NOW - NOW % 300_000 - 1;
    let confirmed_at = last_growth + 682;
    insert(&c, "demo", "p1", 100, 4000, last_growth);
    let result = serde_json::to_value(dashboard(&c, &query(), confirmed_at).unwrap()).unwrap();
    let m = &result["series"][0]["metrics"];
    assert_eq!(m["tokenizerEstimate"]["value"].as_f64(), Some(25.0));
    assert_eq!(m["tokenizerEstimate"]["durationMs"].as_f64(), Some(4000.0));
    let bucket = result["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["metrics"]["tokenizerEstimate"]["sampleCount"] == 1)
        .unwrap();
    assert!(
        bucket["startMs"].as_i64().unwrap() < last_growth
            && bucket["endMs"].as_i64().unwrap() >= last_growth
    );
    assert!(bucket["endMs"].as_i64().unwrap() < confirmed_at);
}
