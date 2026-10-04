// dsh 会话日志读取层：四代际并存（v0/v2/v3 存量 + v4 rc.2 现行——C0-① 实测；
// 取代际最大者 M0 F2），
// header 必读（目录名 ~XXXX 转义不可反解 id，评审漏项 #3；header 兼得子 Agent 过滤字段）

use serde::Deserialize;
use std::path::{Path, PathBuf};

use super::decode::decode_zstd_frames;

#[derive(Debug, Clone, Deserialize)]
pub struct DshHeader {
    pub id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(rename = "parentSession", default)]
    pub parent_session: Option<String>,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(rename = "delegationDepth", default)]
    pub delegation_depth: i64,
    #[serde(rename = "createdAt", default)]
    pub created_at: Option<i64>,
    #[serde(rename = "isSeeded", default)]
    pub is_seeded: Option<bool>,
    #[serde(default)]
    pub version: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DshEvent {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub seq: Option<i64>,
    #[serde(default)]
    pub time: Option<i64>,
    #[serde(default)]
    pub data: serde_json::Value,
}

/// 子 Agent 会话过滤（M0 F7：真实 home 33/76 是子 Agent，不出卡）
pub fn is_subagent(h: &DshHeader) -> bool {
    h.origin.as_deref() == Some("subagent") || h.delegation_depth > 0
}

/// 文件名 → 代际号（v0/v2/v3/v4 实测均在用）；不识别的文件名忽略
/// （代际门的读侧入口只认文件名，header.version 门在上层 mod.rs）
fn parse_generation(name: &str) -> Option<i64> {
    if name == "session.jsonl" || name == "session.jsonl.zstd" {
        return Some(0);
    }
    let rest = name.strip_prefix("session.v")?;
    let rest = rest
        .strip_suffix(".jsonl.zstd")
        .or_else(|| rest.strip_suffix(".jsonl"))?;
    rest.parse::<i64>().ok()
}

pub fn generation_logs(dir: &Path) -> Vec<(i64, PathBuf)> {
    let mut out: Vec<(i64, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            parse_generation(&name).map(|v| (v, e.path()))
        })
        .collect();
    out.sort_by_key(|(v, _)| *v);
    out
}

pub struct GenerationRead {
    pub version: i64,
    pub text: String,
    pub torn_frames: usize,
    /// 日志 mtime（毫秒）：v0 兜底判定的静默时长输入
    pub mtime_ms: i64,
}

pub fn read_best_generation(dir: &Path) -> Option<GenerationRead> {
    let (version, path) = generation_logs(dir).pop()?;
    let bytes = std::fs::read(&path).ok()?;
    let mtime_ms = std::fs::metadata(&path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    let (text, torn_frames) = if path.extension().map(|e| e == "zstd").unwrap_or(false) {
        let d = decode_zstd_frames(&bytes).ok()?;
        (d.text, d.torn_frames)
    } else {
        (String::from_utf8_lossy(&bytes).to_string(), 0)
    };
    Some(GenerationRead {
        version,
        text,
        torn_frames,
        mtime_ms,
    })
}

pub fn parse_header(text: &str) -> Option<DshHeader> {
    serde_json::from_str(text.lines().next()?).ok()
}

pub fn parse_events(text: &str) -> Vec<DshEvent> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/dsh")
            .join(name);
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("读取夹具失败 {p:?}: {e}"))
    }

    #[test]
    fn golden_header_and_subagent_filter() {
        // 黄金样本 sample1：header 含 id/cwd；非子 Agent
        let text = fixture("sample1-completed.sanitized.jsonl");
        let h = parse_header(&text).expect("header 可解析");
        assert!(h.id.starts_with("session-"));
        assert!(!h.cwd.as_deref().unwrap_or("").is_empty());
        assert!(!is_subagent(&h));
        let events = parse_events(&text);
        assert_eq!(events.len(), 21); // M0 实测 21 事件
    }

    #[test]
    fn subagent_detected_by_origin_and_depth() {
        let mut h = DshHeader {
            id: "x".into(),
            cwd: None,
            parent_session: None,
            origin: None,
            delegation_depth: 0,
            created_at: None,
            is_seeded: None,
            version: None,
        };
        assert!(!is_subagent(&h));
        h.origin = Some("subagent".into());
        assert!(is_subagent(&h));
        h.origin = None;
        h.delegation_depth = 1;
        assert!(is_subagent(&h));
    }

    #[test]
    fn picks_max_generation() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        // 未压缩 v0（旧存量）
        std::fs::write(
            d.join("session.jsonl"),
            "{\"type\":\"session\",\"version\":0,\"id\":\"a\"}\n",
        )
        .unwrap();
        // 压缩 v3（新）—— 用解码器测试同款编码
        let frame = zstd::stream::encode_all(
            b"{\"type\":\"session\",\"version\":3,\"id\":\"a\"}\n".as_slice(),
            3,
        )
        .unwrap();
        std::fs::write(d.join("session.v3.jsonl.zstd"), &frame).unwrap();
        let read = read_best_generation(d).expect("应读到");
        assert_eq!(read.version, 3);
        let h = parse_header(&read.text).unwrap();
        assert_eq!(h.version, Some(3));
    }

    #[test]
    fn empty_dir_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_best_generation(dir.path()).is_none());
        assert!(generation_logs(dir.path()).is_empty());
    }

    /// 行为：v4 文件名解析出代际 4、多帧 zstd 全解、v4 header 与全部事件行可解析
    /// （含 v4 独有 `agent/inbox/spliced` 帧）。
    /// 溯源：此断言**修复前即通过**——它是「读侧底座（代际门/解码/schema）无罪」的
    /// 证据，故障真根因在上层 header.version 白名单（见 mod.rs is_known_generation）
    #[test]
    fn v4_filename_parses_and_multiframe_log_decodes() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        // rc.2 实测 header 键集：type/version/id/createdAt/cwd/isSeeded/delegationDepth/agentPreset
        let header = "{\"type\":\"session\",\"version\":4,\"id\":\"session-v4\",\"createdAt\":1000,\"cwd\":\"/tmp/p\",\"isSeeded\":false,\"delegationDepth\":0,\"agentPreset\":\"default\"}";
        // 多帧拼接（对齐真实文件 76 帧形态：header 帧 + 追加写入的事件帧）
        let mut frame = zstd::stream::encode_all(format!("{header}\n").as_bytes(), 3).unwrap();
        frame.extend_from_slice(
            &zstd::stream::encode_all(
                "{\"type\":\"turn/start\",\"seq\":1,\"data\":{}}\n{\"type\":\"agent/inbox/spliced\",\"seq\":2,\"data\":{}}\n"
                    .as_bytes(),
                3,
            )
            .unwrap(),
        );
        std::fs::write(d.join("session.v4.jsonl.zstd"), &frame).unwrap();

        // ① 代际门：文件名 → 4
        let gens = generation_logs(d);
        assert_eq!(gens.len(), 1);
        assert_eq!(gens[0].0, 4, "v4 文件名须解析为代际 4");
        // ③ 多帧解码 + ② header/事件解析
        let read = read_best_generation(d).expect("v4 应可读");
        assert_eq!(read.version, 4);
        assert_eq!(read.torn_frames, 0, "v4 帧形态无需容错丢弃");
        let h = parse_header(&read.text).expect("v4 header 可解析");
        assert_eq!(h.id, "session-v4");
        assert_eq!(h.version, Some(4));
        assert_eq!(h.delegation_depth, 0);
        assert!(!is_subagent(&h));
        // header 行本身也是一行合法 JSON，parse_events 一并收下（kind="session"，
        // 各消费层不匹配即落空——preview/status 均按已知 kind 匹配）
        let events = parse_events(&read.text);
        assert_eq!(events.len(), 3, "header 行 + 2 事件行均可解析");
        assert!(
            events.iter().any(|e| e.kind == "agent/inbox/spliced"),
            "v4 独有帧可解析（是否消费由上层决定：未知帧不猜 ⇒ 静默落空）"
        );
        assert!(events.iter().any(|e| e.kind == "turn/start"));
    }
}
