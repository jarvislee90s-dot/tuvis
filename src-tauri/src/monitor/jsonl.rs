// JSONL 会话文件读取公共件 — 尾部读取、cwd 提取、文件枚举
// 各工具 JSONL 解析器（claude/codex/kimi）共用；不包含任何工具协议判定逻辑

use crate::session::model::JsonlMessage;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// 既有尾部窗基准（512KB）：轮询路径解析器的恒定字节预算，语义不变
const TAIL_BYTES: u64 = 512 * 1024;

/// 读取 JSONL 文件尾部最多 max_lines 行（按文件顺序返回）。
/// 超过 512KB 的文件从尾部定位并跳过首条截断行，与既有解析器行为逐字节一致；
/// 调用方按 .iter().rev() 即得"最新在前"的遍历序
pub(crate) fn read_recent_lines(path: &Path, max_lines: usize) -> Vec<String> {
    read_recent_lines_with_budget(path, max_lines, TAIL_BYTES).0
}

/// 尾部读取的**可失败**版本：文件缺失/不可读/行读取失败都返回 Err。
/// 补偿用它区分“空文件/非完成态”与“读失败”，读失败时保留观测重试。
pub(crate) fn read_recent_lines_result(
    path: &Path,
    max_lines: usize,
) -> std::io::Result<Vec<String>> {
    read_tail_lines(path, max_lines, TAIL_BYTES).map(|(lines, _)| lines)
}

/// 带字节预算的尾部读取（Bug 1，M3 验收）：返回 (行集, 头部是否被截断)。
/// 内容层按需单会话读取以 `512KB×⌈limit/200⌉` 放大窗，据此向移动端发 truncated
/// 信号（「加载更早消息」可达性）；轮询路径解析器仍走 read_recent_lines（512KB
/// 恒定窗），预算契约不受影响。文件缺失/不可读 → (空集, false)
pub(crate) fn read_recent_lines_with_budget(
    path: &Path,
    max_lines: usize,
    max_bytes: u64,
) -> (Vec<String>, bool) {
    read_tail_lines(path, max_lines, max_bytes).unwrap_or_default()
}

/// 字节级尾部读取：截断点可能落在多字节 UTF-8 字符中间，**不能用
/// `read_line(&mut String)`**（会因无效 UTF-8 拒绝整个合法文件）。
/// 先用 `read_until(b'\n')` 按字节丢弃截断残行，再把完整行做 lossy 解码。
fn read_tail_lines(
    path: &Path,
    max_lines: usize,
    max_bytes: u64,
) -> std::io::Result<(Vec<String>, bool)> {
    let file = File::open(path)?;
    let file_size = file.metadata()?.len();
    let truncated = file_size > max_bytes;
    let mut reader = BufReader::new(file);
    if truncated {
        reader.seek(SeekFrom::End(-(max_bytes as i64)))?;
        let mut discard = Vec::new();
        reader.read_until(b'\n', &mut discard)?;
    }
    let mut lines = Vec::new();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&buf);
        lines.push(text.trim_end_matches(['\n', '\r']).to_string());
    }
    let start = lines.len().saturating_sub(max_lines);
    Ok((lines[start..].to_vec(), truncated))
}

/// 读取 JSONL 文件头部最多 max_lines 行（按文件顺序返回）。
/// 「取首条」场景专用（issue #35-6）：长会话的首条 user 消息不在尾部窗口内，
/// 标题降级需从文件头读取；文件缺失/不可读 → 空集
pub(crate) fn read_first_lines(path: &Path, max_lines: usize) -> Vec<String> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    BufReader::new(file)
        .lines()
        .take(max_lines)
        .map_while(Result::ok)
        .collect()
}

/// 从 JSONL 文件头部提取首个有效 cwd（Claude 协议消息携带 cwd 字段）
pub(crate) fn extract_cwd_from_jsonl(jsonl_path: &Path) -> Option<String> {
    let file = File::open(jsonl_path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().take(20).flatten() {
        if let Ok(msg) = serde_json::from_str::<JsonlMessage>(&line) {
            if let Some(cwd) = msg.cwd {
                if super::project::is_valid_cwd(&cwd) {
                    return Some(cwd);
                }
            }
        }
    }
    None
}

/// Claude 子 agent 会话文件命名：agent-*.jsonl
pub(crate) fn is_subagent_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|name| name.starts_with("agent-") && name.ends_with(".jsonl"))
        .unwrap_or(false)
}

/// 枚举目录内最近修改的会话 JSONL 文件（排除子 agent 文件，按 mtime 倒序）
pub(crate) fn get_recent_jsonl_files(project_dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = fs::read_dir(project_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let p = e.path();
            p.extension().map(|ext| ext == "jsonl").unwrap_or(false) && !is_subagent_file(&p)
        })
        .filter_map(|e| {
            let path = e.path();
            let modified = e.metadata().and_then(|m| m.modified()).ok()?;
            Some((path, modified))
        })
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.1));
    files.into_iter().map(|(p, _)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug 1 修复（M3 验收）：带字节预算的尾部读取必须报告「头部被截断」。
    /// 512KB 窗切进 600KB 大行 → truncated=true、头部小行缺席；放大窗后全文件
    /// 可读（truncated=false）；read_recent_lines 委托语义不变（512KB 恒定窗）
    #[test]
    fn read_recent_lines_with_budget_reports_head_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fat.jsonl");
        let big = "x".repeat(600 * 1024);
        std::fs::write(&path, format!("head\n{big}\ntail\n")).unwrap();
        // 512KB 窗：seek 落在大行内 → 大行与头部小行均不可见，仅尾行存活
        let (lines, truncated) = read_recent_lines_with_budget(&path, 100, 512 * 1024);
        assert!(truncated, "超窗文件必须报告头部截断");
        assert_eq!(lines, vec!["tail".to_string()]);
        // 1MB 窗：全文件进窗，头部行可见
        let (lines, truncated) = read_recent_lines_with_budget(&path, 100, 1024 * 1024);
        assert!(!truncated, "未超窗不得报告截断");
        assert_eq!(lines, vec!["head".to_string(), big, "tail".to_string()]);
        // 未超窗的小文件：truncated 恒 false
        let small = dir.path().join("small.jsonl");
        std::fs::write(&small, "a\nb\n").unwrap();
        let (lines, truncated) = read_recent_lines_with_budget(&small, 10, 512 * 1024);
        assert!(!truncated);
        assert_eq!(lines, vec!["a".to_string(), "b".to_string()]);
        // 既有委托：read_recent_lines 仍是 512KB 恒定窗（轮询路径解析器零语义变化）
        assert_eq!(read_recent_lines(&path, 100), vec!["tail".to_string()]);
    }

    /// issue #35-6：头部读取按文件顺序返回，行数上限生效，缺失文件 → 空集
    #[test]
    fn read_first_lines_returns_head_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        std::fs::write(&path, "l1\nl2\nl3\nl4\n").unwrap();
        assert_eq!(
            read_first_lines(&path, 2),
            vec!["l1".to_string(), "l2".to_string()]
        );
        // 上限宽于文件行数 → 全量返回
        assert_eq!(read_first_lines(&path, 10).len(), 4);
        // 文件缺失 → 空集（不 panic）
        assert!(read_first_lines(&dir.path().join("missing.jsonl"), 5).is_empty());
    }

    /// 512KB 尾部窗口起点落在多字节 UTF-8 字符中间时，合法长 JSONL 仍必须可读。
    #[test]
    fn tail_reader_tolerates_utf8_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("session.jsonl");
        let head = "{\"type\":\"reasoning\",\"content\":\"";
        let tail_line =
            "{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"完成\"}]}\n";
        let mut chosen = None;
        for pad in 0..3 {
            let mut content = String::new();
            content.push_str(head);
            content.push_str(&"中".repeat(220_000));
            content.push_str(&"a".repeat(pad));
            content.push_str("\"}\n");
            let line1_end = content.len();
            content.push_str(tail_line);
            let start = content.len() - TAIL_BYTES as usize;
            if start < line1_end && content.as_bytes()[start] & 0xC0 == 0x80 {
                chosen = Some(content);
                break;
            }
        }
        let content = chosen.expect("应能构造出 UTF-8 字符中间的截断点");
        fs::write(&p, content).unwrap();

        let lines = read_recent_lines_result(&p, 500).unwrap();
        assert!(
            lines.iter().any(|l| l.contains("完成")),
            "截断点落在 UTF-8 中间时仍应读出最后的完成行: {lines:?}"
        );
    }
}
