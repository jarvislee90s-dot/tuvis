//! Kimi 子 agent source（spec §4.3：活跃度判据 + 两点待实机校准）。
//! 子 agent = agents/<dir>/wire.jsonl 存在 ∧ dir != "main"；mtime 距 now < 90s
//! 视为活跃（与 opencode §4.2 同取值）。T2（观察台 §二.3）：90s 窗从「过滤出板」
//! 变为「status 分诊」——超窗转 Idle 在板，endTs = wire mtime。会话目录定位
//! **复用 kimi_parser 会话注册表**（session_index.jsonl + resolve_session_dir），
//! 不另写路径推导。v1 名称显示目录名（agent-N）；实机校准后换真名（§十 C1，
//! 校准只改本文件）。

use super::{
    active_within, json_time_to_iso, read_increment, sort_views, update_entry, CacheBox, IncrRead,
    IncrState, SubagentStatus, SubagentView, TokenUsage,
};
use crate::monitor::kimi_parser::{parse_session_index, resolve_session_dir, KimiDataRoot};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

/// SystemTime → ISO（kimi endTs 用；mtime 无完成事件语义，长静默转灰时锚在静默起点——
/// 与 90s 窗口径一致，如实申报）
fn systemtime_to_iso(t: SystemTime) -> Option<String> {
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| super::ms_to_iso(d.as_millis() as i64))
}

/// wire.jsonl 单行累计（纯函数）：usage.record 四桶 + 首行 metadata.created_at
pub(crate) fn accumulate_wire_line(line: &str, acc: &mut TokenUsage, created: &mut Option<String>) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    match v["type"].as_str() {
        Some("metadata") => {
            if created.is_none() {
                *created = json_time_to_iso(&v["created_at"]);
            }
        }
        Some("usage.record") => {
            // usage 对象在场则取之，否则事件平铺（双形态容忍；C1 校准后收严）
            let u = if v["usage"].is_object() {
                &v["usage"]
            } else {
                &v
            };
            acc.input += u["inputOther"].as_u64().unwrap_or(0);
            acc.output += u["output"].as_u64().unwrap_or(0);
            acc.cache_read += u["inputCacheRead"].as_u64().unwrap_or(0);
            acc.cache_creation += u["inputCacheCreation"].as_u64().unwrap_or(0);
        }
        _ => {}
    }
}

/// 会话级缓存条目（每 agent 目录一份增量游标）
#[derive(Default, Clone)]
struct KimiCache {
    agents: HashMap<String, AgentTrack>,
}
#[derive(Default, Clone)]
struct AgentTrack {
    incr: IncrState,
    tokens: TokenUsage,
    created: Option<String>,
}

pub fn collect(session_id: &str) -> Vec<SubagentView> {
    // 数据根判定复用 kimi_parser 单点（评审 P2-3：不内联第二份 env/home 逻辑）
    let Some(root) = crate::monitor::kimi_parser::kimi_data_root() else {
        return Vec::new();
    };
    collect_with(root, session_id, SystemTime::now())
}

pub(crate) fn collect_with(
    root: KimiDataRoot,
    session_id: &str,
    now: SystemTime,
) -> Vec<SubagentView> {
    // ① 注册表定位会话目录（零路径推导复制）
    let Some(dir) = parse_session_index(&root)
        .into_iter()
        .find(|e| e.session_id == session_id)
        .map(|e| resolve_session_dir(&root, &e.session_dir))
    else {
        return Vec::new();
    };
    // 信任边界（评审 P1-2，kimi_parser::stat_index_entry 同款，kimi_parser.rs:276-284）：
    // sessionDir 只允许落在 sessions 根之下——越界（含 ../ 逃逸、任意绝对路径）跳过，不误报
    if !dir.starts_with(&root.sessions) {
        return Vec::new();
    }
    // ② agents/ 扫描：dir != main ∧ wire.jsonl 存在——T2：去掉 active_within 门控
    //（v1「超窗隐藏」→「status 分诊」，mtime 留作 endTs 锚）
    let agents_dir = dir.join("agents");
    let mut candidates: Vec<(String, PathBuf, SystemTime)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&agents_dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name == "main" || !e.path().is_dir() {
                continue;
            }
            let wire = e.path().join("wire.jsonl");
            let Ok(meta) = std::fs::metadata(&wire) else {
                continue;
            };
            let Ok(mtime) = meta.modified() else {
                continue;
            };
            candidates.push((name, wire, mtime));
        }
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0)); // 注册序稳定
                                              // ③ 增量累计（缓存）
    update_entry("kimi", session_id, move |cur| {
        let mut st = cur
            .and_then(|b| b.downcast::<KimiCache>().ok())
            .map(|a| Arc::try_unwrap(a).unwrap_or_else(|a| (*a).clone()))
            .unwrap_or_default();
        let mut views = Vec::new();
        for (name, wire, mtime) in &candidates {
            let track = st.agents.entry(name.clone()).or_default();
            match read_increment(wire, &mut track.incr) {
                IncrRead::Unchanged => {}
                IncrRead::Lines { lines, reset } => {
                    if reset {
                        track.tokens = TokenUsage::default();
                        track.created = None;
                    }
                    for line in &lines {
                        accumulate_wire_line(line, &mut track.tokens, &mut track.created);
                    }
                }
            }
            // 活跃分诊（T2）：窗内 Running；超窗转 Idle 在板，endTs = wire mtime
            let active = active_within(*mtime, now);
            views.push(SubagentView {
                id: name.clone(),
                name: name.clone(), // v1：目录名（§4.3；C1 校准后换真名）
                description: None,
                spawn_ts: track.created.clone(),
                tokens: track.tokens,
                status: if active {
                    SubagentStatus::Running
                } else {
                    SubagentStatus::Idle
                },
                end_ts: if active {
                    None
                } else {
                    systemtime_to_iso(*mtime)
                },
            });
        }
        sort_views(&mut views);
        (Arc::new(st) as CacheBox, views)
    })
}

/// 回报提醒打标锚（#151 全工具化）：agents/<dir>/wire.jsonl 的 mtime 毫秒列表
/// （最后写时刻 ≈ 回报写完）。stat-only 不进增量缓存——打标由「绿会话」闸住
/// 成本。为什么不复用 collect 的 endTs：kimi 的 Idle 分诊有 90s 活动窗滞后，
/// 「刚完成 10s」的子代理视图里还是 Running（endTs=None），恰是打标最需要的
/// 时刻——取原始活动毫秒，不经视图分诊。
pub fn wire_activity_anchors(session_id: &str) -> Vec<i64> {
    // 数据根判定复用 kimi_parser 单点（同 collect）
    let Some(root) = crate::monitor::kimi_parser::kimi_data_root() else {
        return Vec::new();
    };
    wire_activity_anchors_with(root, session_id)
}

/// 可注入核心（root 注入；测试 tempdir）
pub(crate) fn wire_activity_anchors_with(root: KimiDataRoot, session_id: &str) -> Vec<i64> {
    let Some(dir) = parse_session_index(&root)
        .into_iter()
        .find(|e| e.session_id == session_id)
        .map(|e| resolve_session_dir(&root, &e.session_dir))
    else {
        return Vec::new();
    };
    // 信任边界同 collect_with：越界（含 ../ 逃逸）跳过
    if !dir.starts_with(&root.sessions) {
        return Vec::new();
    }
    let agents_dir = dir.join("agents");
    let mut anchors = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&agents_dir) {
        for e in entries.flatten() {
            if e.file_name() == "main" || !e.path().is_dir() {
                continue;
            }
            let wire = e.path().join("wire.jsonl");
            if let Some(ms) = std::fs::metadata(&wire)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            {
                anchors.push(ms.as_millis() as i64);
            }
        }
    }
    anchors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::kimi_parser::{resolve_data_root, KimiDataRoot};
    use crate::monitor::subagents::ms_to_iso;

    /// 夹具：.kimi-code 根 + session_index.jsonl + sessions/<wd>/<sid>/agents/<dir>/wire.jsonl
    fn kimi_fixture(sid: &str, agents: &[(&str, &str)]) -> (tempfile::TempDir, KimiDataRoot) {
        let td = tempfile::tempdir().unwrap();
        let home = td.path().join("khome");
        let sess = home.join("sessions").join("wd-1").join(sid);
        for (dir, wire) in agents {
            let d = sess.join("agents").join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("wire.jsonl"), wire).unwrap();
        }
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("session_index.jsonl"),
            format!(
                r#"{{"sessionId":"{sid}","sessionDir":"sessions/wd-1/{sid}","workDir":"E:/demo"}}"#
            ),
        )
        .unwrap();
        let root = resolve_data_root(Some(home.to_str().unwrap()), td.path()).unwrap();
        (td, root)
    }
    fn wire(body: &str) -> String {
        format!("{{\"type\":\"metadata\",\"created_at\":1760000000000}}\n{body}")
    }
    fn usage(inp: u64, out: u64, cr: u64, cc: u64) -> String {
        // usage.record 四桶 camelCase（spec §2.3 实盘形态）
        format!("{{\"type\":\"usage.record\",\"time\":1760000001000,\"usage\":{{\"inputOther\":{inp},\"output\":{out},\"inputCacheRead\":{cr},\"inputCacheCreation\":{cc}}}}}\n")
    }

    /// main 排除 + wire 缺失跳过 + mtime 活跃 + usage 累计 + spawnTs（created_at 毫秒）
    #[test]
    fn kimi_source_scans_agents_dir() {
        super::super::reset_cache_for_tests();
        let (_td, root) = kimi_fixture(
            "ks-1",
            &[
                ("main", &wire(&usage(9, 9, 9, 9))), // main 排除（§4.3）
                ("agent-0", &wire(&usage(100, 200, 300, 400))),
                ("agent-1", ""),   // 空 wire 也登记（存在即候选）
                ("empty-dir", ""), // 无 wire.jsonl → 跳过（构造后删 wire）
            ],
        );
        std::fs::remove_file(
            root.sessions
                .join("wd-1")
                .join("ks-1")
                .join("agents")
                .join("empty-dir")
                .join("wire.jsonl"),
        )
        .unwrap();
        // now = wire mtime + 10s（活跃）
        let mtime = std::fs::metadata(
            root.sessions
                .join("wd-1")
                .join("ks-1")
                .join("agents")
                .join("agent-0")
                .join("wire.jsonl"),
        )
        .unwrap()
        .modified()
        .unwrap();
        let v = collect_with(
            root.clone(),
            "ks-1",
            mtime + std::time::Duration::from_secs(10),
        );
        let ids: Vec<&str> = v.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            ids,
            ["agent-0", "agent-1"],
            "main 排除、无 wire 跳过、目录名即名称（§4.3 v1）"
        );
        assert_eq!(
            v[0].tokens,
            TokenUsage {
                input: 100,
                cache_read: 300,
                cache_creation: 400,
                output: 200
            }
        );
        assert_eq!(v[0].spawn_ts, ms_to_iso(1_760_000_000_000));
        assert_eq!(v[0].status, SubagentStatus::Running, "mtime 在窗内 → 活跃");
        assert_eq!(v[0].end_ts, None, "运行中 endTs 恒空");
        // mtime 超 90s → 不再隐藏，转 Idle 冻结在板（观察台 §二.3 全量名单）
        let v = collect_with(
            root.clone(),
            "ks-1",
            mtime + std::time::Duration::from_secs(91),
        );
        assert_eq!(
            v.len(),
            2,
            "超窗不隐藏：agent-0/agent-1 均留原位（agent-1 同为有 wire 的候选）"
        );
        assert!(v.iter().all(|s| s.status == SubagentStatus::Idle));
        assert_eq!(
            v[0].end_ts.as_deref(),
            Some(systemtime_to_iso(mtime).unwrap().as_str()),
            "endTs = wire mtime（长静默锚在静默起点，§8.4 口径如实申报）"
        );
        super::super::reset_cache_for_tests();
    }

    /// 增量：第二轮只累计追加的 usage.record；重建（缓存清空）结果一致
    #[test]
    fn kimi_source_incremental_and_rebuild() {
        super::super::reset_cache_for_tests();
        let (_td, root) = kimi_fixture("ks-2", &[("agent-0", &wire(&usage(1, 2, 3, 4)))]);
        let wire_path = root
            .sessions
            .join("wd-1")
            .join("ks-2")
            .join("agents")
            .join("agent-0")
            .join("wire.jsonl");
        let mtime = std::fs::metadata(&wire_path).unwrap().modified().unwrap();
        let now = || mtime + std::time::Duration::from_secs(5);
        assert_eq!(
            collect_with(root.clone(), "ks-2", now())[0].tokens.total(),
            10
        );
        // 追加一条 usage.record（mtime 变新）
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&wire_path)
            .unwrap();
        f.write_all(usage(10, 20, 30, 40).as_bytes()).unwrap();
        drop(f);
        let mtime2 = std::fs::metadata(&wire_path).unwrap().modified().unwrap();
        assert_eq!(
            collect_with(
                root.clone(),
                "ks-2",
                mtime2 + std::time::Duration::from_secs(5)
            )[0]
            .tokens
            .total(),
            110,
            "增量只加新增（1+2+3+4 + 10+20+30+40）"
        );
        // 重启重建
        super::super::reset_cache_for_tests();
        assert_eq!(
            collect_with(
                root.clone(),
                "ks-2",
                mtime2 + std::time::Duration::from_secs(5)
            )[0]
            .tokens
            .total(),
            110
        );
        super::super::reset_cache_for_tests();
    }

    /// P1-2：sessionDir 越界（../../ 逃逸 / 任意绝对路径）→ 空态（信任边界与
    /// kimi_parser::stat_index_entry 同款——防索引行指向 sessions 根之外）
    #[test]
    fn kimi_source_rejects_out_of_root_session_dir() {
        super::super::reset_cache_for_tests();
        let td = tempfile::tempdir().unwrap();
        let home = td.path().join("khome");
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        // 两行坏索引：相对逃逸 + 绝对路径；均不得出数据
        std::fs::write(
            home.join("session_index.jsonl"),
            concat!(
                r#"{"sessionId":"bad1","sessionDir":"../../escape","workDir":"E:/x"}"#,
                "\n",
                r#"{"sessionId":"bad2","sessionDir":"C:/Windows/Temp","workDir":"E:/x"}"#,
                "\n",
            ),
        )
        .unwrap();
        let root = resolve_data_root(Some(home.to_str().unwrap()), td.path()).unwrap();
        assert!(collect_with(root.clone(), "bad1", std::time::SystemTime::now()).is_empty());
        assert!(collect_with(root, "bad2", std::time::SystemTime::now()).is_empty());
        super::super::reset_cache_for_tests();
    }

    /// 会话不在索引 → 空态（复用注册表，不另写路径推导）
    #[test]
    fn kimi_source_unknown_session_empty() {
        super::super::reset_cache_for_tests();
        let (_td, root) = kimi_fixture("ks-3", &[("agent-0", &wire(&usage(1, 1, 1, 1)))]);
        assert!(collect_with(root, "nope", std::time::SystemTime::now()).is_empty());
        super::super::reset_cache_for_tests();
    }

    // ==== #151：回报打标锚 wire_activity_anchors_with（stat-only，不进增量缓存）====

    /// 锚收集：agent wire 的 mtime 毫秒；main 排除、无 wire 跳过、不在索引空态
    #[test]
    fn wire_activity_anchors_collect_mtime_ms() {
        let (_td, root) = kimi_fixture(
            "ks-anch",
            &[
                ("main", "{}\n"),     // main 排除
                ("agent-0", "{}\n"),  // 收
                ("agent-1", "{}\n"),  // 收
                ("empty-dir", ""),    // 无 wire.jsonl → 跳过（构造后删 wire）
            ],
        );
        std::fs::remove_file(
            root.sessions
                .join("wd-1")
                .join("ks-anch")
                .join("agents")
                .join("empty-dir")
                .join("wire.jsonl"),
        )
        .unwrap();
        // 精确设 mtime：锚的可控值（回报窗判定在 report_anchor_near_last，已单测边界）
        let t0 = filetime::FileTime::from_unix_time(1_760_000_000, 123_000_000);
        let t1 = filetime::FileTime::from_unix_time(1_760_000_050, 0);
        std::fs::write(
            root.sessions.join("wd-1").join("ks-anch").join("agents").join("agent-0").join("wire.jsonl"),
            "{}\n",
        )
        .unwrap();
        filetime::set_file_mtime(
            root.sessions.join("wd-1").join("ks-anch").join("agents").join("agent-0").join("wire.jsonl"),
            t0,
        )
        .unwrap();
        filetime::set_file_mtime(
            root.sessions.join("wd-1").join("ks-anch").join("agents").join("agent-1").join("wire.jsonl"),
            t1,
        )
        .unwrap();
        let mut anchors = wire_activity_anchors_with(root.clone(), "ks-anch");
        anchors.sort_unstable();
        assert_eq!(
            anchors,
            [1_760_000_000_123, 1_760_000_050_000],
            "收集各 agent wire mtime 毫秒（main 排除、无 wire 跳过）"
        );
        // 不在索引 / 越界 sessionDir → 空态（信任边界同 collect_with）
        assert!(wire_activity_anchors_with(root.clone(), "nope").is_empty());
    }

    /// 越界 sessionDir（../../ 逃逸）→ 空锚（P1-2 信任边界同样适用于锚收集）
    #[test]
    fn wire_activity_anchors_reject_out_of_root_session_dir() {
        let td = tempfile::tempdir().unwrap();
        let home = td.path().join("khome");
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        std::fs::write(
            home.join("session_index.jsonl"),
            concat!(
                r#"{"sessionId":"bad1","sessionDir":"../../escape","workDir":"E:/x"}"#,
                "\n",
            ),
        )
        .unwrap();
        let root = resolve_data_root(Some(home.to_str().unwrap()), td.path()).unwrap();
        assert!(wire_activity_anchors_with(root, "bad1").is_empty());
    }
}
