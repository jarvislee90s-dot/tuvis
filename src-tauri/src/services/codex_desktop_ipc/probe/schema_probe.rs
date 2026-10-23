//! 官方 schema 探针（#172）：跑随包 CLI 的
//! `app-server generate-json-schema --out <temp> --experimental`，读审批 decision 枚举。
//!
//! 为什么需要它：桌面私有 IPC 没有「问对面支持什么」的口，但随包 CLI 自带**官方
//! schema 生成器**（2026-10-21 实测 0.23s、40+ 文件）。它给出的是「这个 build 的
//! command/file 审批决策枚举长什么样」——比猜版本号可靠。
//!
//! 三条硬约束（设计 §4.3 / 施工单 T6）：
//! 1. **只读**：只跑 schema 生成，绝不发 IPC 帧、绝不触发审批；
//! 2. **有界**：3s 超时（挂死就 kill + 收尸），临时目录**用完即删**；
//! 3. **失败即 unknown**：命令缺失/失败、文件缺失、解析不出枚举——一律 `Unknown`
//!    （审批退回「到桌面处理」），绝不猜「应该支持」。
//!
//! 本模块**不**决定要不要开放审批（那是 T7 的门）：它只回答「schema 里写了什么」。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// schema 生成命令的固定参数（契约：顺序与开关都在这里，测试断言本表）。
pub const GENERATE_SCHEMA_SUBPATH: &str = "app-server";
pub const GENERATE_SCHEMA_ACTION: &str = "generate-json-schema";
/// experimental 开关（实测必须带，否则不出审批响应定义）。
pub const GENERATE_SCHEMA_EXPERIMENTAL: &str = "--experimental";
/// 命令超时（实测 0.23s；3s 是「上限」而非预期）。
pub const SCHEMA_TIMEOUT: Duration = Duration::from_secs(3);

/// 两个审批响应定义文件（command / file 审批的 decision 来源）。
pub const COMMAND_APPROVAL_FILE: &str = "CommandExecutionRequestApprovalResponse.json";
pub const FILE_APPROVAL_FILE: &str = "FileChangeRequestApprovalResponse.json";

/// 已知的审批 decision 词（用于定位枚举与判定「是否认得这个值」）。
/// `cancel` = 拒绝并**立即中断本轮**（与 `decline` 语义不同）。
pub const KNOWN_DECISIONS: &[&str] = &["accept", "acceptForSession", "decline", "cancel"];

/// schema 证据（成功时）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaEvidence {
    /// `文件名+大小` 的 sha256（归因与变更检测；不含内容——避免把官方文案抄进日志）。
    pub fingerprint: String,
    /// command 审批的 decision 枚举（保持 schema 里的顺序）。
    pub command_decisions: Vec<String>,
    /// file 审批的 decision 枚举。
    pub file_decisions: Vec<String>,
}

impl SchemaEvidence {
    /// 该 build 的 command 审批是否认这个词（T7 用它决定「允许/终止本轮」能不能给）。
    pub fn command_allows(&self, decision: &str) -> bool {
        self.command_decisions.iter().any(|d| d == decision)
    }

    /// 同上，file 审批。
    pub fn file_allows(&self, decision: &str) -> bool {
        self.file_decisions.iter().any(|d| d == decision)
    }
}

/// 探测结论。`Unknown` 一律导致「审批不开放」（到桌面处理）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaProbeOutcome {
    Known(SchemaEvidence),
    /// 明确原因码：`cli_not_found` / `spawn_failed` / `timeout` / `exit_failed` /
    /// `missing_definition` / `unparsable_decision`。
    Unknown {
        error_code: String,
    },
}

impl SchemaProbeOutcome {
    pub fn is_known(&self) -> bool {
        matches!(self, Self::Known(_))
    }

    pub fn error_code(&self) -> Option<&str> {
        match self {
            Self::Known(_) => None,
            Self::Unknown { error_code } => Some(error_code.as_str()),
        }
    }
}

/// 跑一次 schema 探针（阻塞实现：**调用方负责 `spawn_blocking`**）。
///
/// 顺序：定位随包 CLI → 临时目录 → 跑命令（3s 有界）→ 读两个定义文件 →
/// 折叠指纹 → **删临时目录**（成功/失败/超时都删）。
pub fn probe_schema_blocking(cli: &Path) -> SchemaProbeOutcome {
    with_temp_out_dir(|dir| {
        if let Err(outcome) = run_generate_schema(cli, dir) {
            return outcome;
        }
        collect_evidence(dir)
    })
}

/// 异步入口：定位随包 CLI（进程扫描，放到阻塞线程池）后跑探针。
pub async fn probe_schema() -> SchemaProbeOutcome {
    let cli = tokio::task::spawn_blocking(super::fingerprint::cli_executable)
        .await
        .ok()
        .flatten();
    let Some(cli) = cli else {
        return SchemaProbeOutcome::Unknown {
            error_code: "cli_not_found".into(),
        };
    };
    tokio::task::spawn_blocking(move || probe_schema_blocking(&cli))
        .await
        .unwrap_or_else(|_| SchemaProbeOutcome::Unknown {
            error_code: "probe_task_failed".into(),
        })
}

/// 跑 `app-server generate-json-schema --out <dir> --experimental`（3s 有界；超时 kill）。
fn run_generate_schema(cli: &Path, out: &Path) -> Result<(), SchemaProbeOutcome> {
    use wait_timeout::ChildExt;
    let mut child = std::process::Command::new(cli)
        .arg(GENERATE_SCHEMA_SUBPATH)
        .arg(GENERATE_SCHEMA_ACTION)
        .arg("--out")
        .arg(out)
        .arg(GENERATE_SCHEMA_EXPERIMENTAL)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|_| SchemaProbeOutcome::Unknown {
            error_code: "spawn_failed".into(),
        })?;
    match child.wait_timeout(SCHEMA_TIMEOUT) {
        Ok(Some(status)) => {
            if status.success() {
                Ok(())
            } else {
                Err(SchemaProbeOutcome::Unknown {
                    error_code: "exit_failed".into(),
                })
            }
        }
        Ok(None) => {
            // 超时：杀进程 + 收尸（不留挂死子进程）
            let _ = child.kill();
            let _ = child.wait();
            Err(SchemaProbeOutcome::Unknown {
                error_code: "timeout".into(),
            })
        }
        Err(_) => Err(SchemaProbeOutcome::Unknown {
            error_code: "spawn_failed".into(),
        }),
    }
}

/// 读两个定义文件 → 指纹 + decision 枚举。缺文件/解析不出 → `Unknown`（不猜）。
fn collect_evidence(dir: &Path) -> SchemaProbeOutcome {
    let command = match read_decision_enum(&dir.join(COMMAND_APPROVAL_FILE)) {
        Ok(v) => v,
        Err(code) => return SchemaProbeOutcome::Unknown { error_code: code },
    };
    let file = match read_decision_enum(&dir.join(FILE_APPROVAL_FILE)) {
        Ok(v) => v,
        Err(code) => return SchemaProbeOutcome::Unknown { error_code: code },
    };
    SchemaProbeOutcome::Known(SchemaEvidence {
        fingerprint: schema_fingerprint(dir),
        command_decisions: command,
        file_decisions: file,
    })
}

/// 从一份 JSON schema 里取审批 decision 枚举。
fn read_decision_enum(path: &Path) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(path).map_err(|_| "missing_definition".to_string())?;
    let json: Value = serde_json::from_str(&text).map_err(|_| "unparsable_decision".to_string())?;
    decision_enum(&json).ok_or_else(|| "unparsable_decision".to_string())
}

/// 找 decision 枚举。
///
/// **真实形态（2026-10-21 在 26.1013.4894.0 实测）**：`properties.decision` 不是直接
/// `enum`，而是 `{"$ref": "#/definitions/CommandExecutionApprovalDecision"}`，被引用的
/// 定义是 `oneOf: [ {enum:["accept"]}, {enum:["acceptForSession"]}, … ]`——每个词一个
/// 分支。所以必须：定位 `decision` → 解 `$ref` → 把 `oneOf`/`anyOf`/`allOf` 里的单词
/// 枚举**按序摊平**（旧实现只看 `decision.enum`，在真机上会只认出第一个词）。
///
/// 兜底顺序：`decision` 节点摊平 → 名字含 `Decision` 的定义摊平 → 「含 accept 的枚举」
/// → `None`（不猜）。
pub fn decision_enum(root: &Value) -> Option<Vec<String>> {
    if let Some(node) = find_key_recursive(root, "decision") {
        let words = collect_enum_words(root, node, 0);
        if !words.is_empty() {
            return Some(words);
        }
    }
    if let Some(words) = find_decision_definition(root) {
        return Some(words);
    }
    find_enum_containing(root, "accept")
}

/// 递归找某个键（任意深度；`properties.decision` 与扁平 `decision` 都能命中）。
fn find_key_recursive<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => {
            if let Some(found) = map.get(key) {
                return Some(found);
            }
            map.values().find_map(|c| find_key_recursive(c, key))
        }
        Value::Array(items) => items.iter().find_map(|c| find_key_recursive(c, key)),
        _ => None,
    }
}

/// 解同文档内的 `$ref`（`#/definitions/X` 或 `#/$defs/X`；`~0`/`~1` 反转义）。
fn resolve_ref<'a>(root: &'a Value, node: &'a Value) -> Option<&'a Value> {
    let reference = node.get("$ref")?.as_str()?;
    let path = reference.strip_prefix("#/")?;
    let mut current = root;
    for raw in path.split('/') {
        let seg = raw.replace("~1", "/").replace("~0", "~");
        current = current.get(&seg)?;
    }
    Some(current)
}

/// 摊平节点里的枚举词：`enum` 直接收；`oneOf`/`anyOf`/`allOf` 逐支递归（含 `$ref`）。
/// 深度上限防病态 schema（循环引用在 JSON 里不可能，但嵌套可以很深）。
fn collect_enum_words(root: &Value, node: &Value, depth: usize) -> Vec<String> {
    if depth > 8 {
        return Vec::new();
    }
    if let Some(target) = resolve_ref(root, node) {
        return collect_enum_words(root, target, depth + 1);
    }
    if let Some(list) = node.get("enum").and_then(|e| e.as_array()) {
        return list
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
    }
    let mut words: Vec<String> = Vec::new();
    for combinator in ["oneOf", "anyOf", "allOf"] {
        if let Some(branches) = node.get(combinator).and_then(|b| b.as_array()) {
            for branch in branches {
                for word in collect_enum_words(root, branch, depth + 1) {
                    if !words.contains(&word) {
                        words.push(word);
                    }
                }
            }
        }
    }
    words
}

/// 兜底：找名字含 `Decision`（且能摊出枚举）的定义——真实 schema 里决策定义就叫
/// `CommandExecutionApprovalDecision` / `FileChangeApprovalDecision`。
fn find_decision_definition(root: &Value) -> Option<Vec<String>> {
    for container in ["definitions", "$defs"] {
        let Some(defs) = root.get(container).and_then(|d| d.as_object()) else {
            continue;
        };
        for (name, node) in defs {
            if !name.contains("Decision") {
                continue;
            }
            let words = collect_enum_words(root, node, 0);
            if !words.is_empty() {
                return Some(words);
            }
        }
    }
    None
}

/// 兜底：任意 `enum` 数组里含指定词。
fn find_enum_containing(value: &Value, needle: &str) -> Option<Vec<String>> {
    match value {
        Value::Object(map) => {
            if let Some(Value::Array(items)) = map.get("enum") {
                let list: Vec<String> = items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect();
                if list.iter().any(|s| s == needle) {
                    return Some(list);
                }
            }
            map.values().find_map(|c| find_enum_containing(c, needle))
        }
        Value::Array(items) => items.iter().find_map(|c| find_enum_containing(c, needle)),
        _ => None,
    }
}

/// `文件名+大小` 的 sha256（按相对路径排序后拼接；不含文件内容）。
///
/// 为什么是「名字+大小」而不是内容哈希：设计要的是**schema 版本指纹**——内容哈希会
/// 把官方文档改写也算进去，而名字+大小足以区分「文件集合变了 / 单个文件换了大小」。
/// 指纹只做归因，不参与兼容判定（判定看 decision 枚举本身）。
pub fn schema_fingerprint(root: &Path) -> String {
    let mut entries = Vec::new();
    collect_entries(root, root, &mut entries);
    entries.sort();
    let mut hasher = Sha256::new();
    for (name, size) in entries {
        hasher.update(name.as_bytes());
        hasher.update(b":");
        hasher.update(size.to_string().as_bytes());
        hasher.update(b"\n");
    }
    // sha2 0.11 的 Output 泛型不实现 LowerHex：手工转十六进制（不引入新依赖）
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn collect_entries(root: &Path, dir: &Path, out: &mut Vec<(String, u64)>) {
    let Ok(iter) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in iter.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_entries(root, &path, out);
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let name = path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        out.push((name, meta.len()));
    }
}

/// 临时输出目录：`f` 结束时**无条件**删掉（成功/失败/panic 展开都删）。
fn with_temp_out_dir<R>(f: impl FnOnce(&Path) -> R) -> R {
    let dir = std::env::temp_dir().join(format!(
        "tuvis-codex-schema-{}-{}",
        std::process::id(),
        super::super::types::new_uuid()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let _cleanup = TempDirGuard(dir.clone());
    f(&dir)
}

/// 目录清理守卫（Drop 时递归删；测试也直接用它验证「用完即删」）。
struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// command 审批样例：**真实 26.1013.4894.0 形态**（`properties.decision` 是
    /// `$ref`，被引用的定义是 `oneOf` 单词语义分支）。
    const COMMAND_SCHEMA: &str = r##"{
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "CommandExecutionRequestApprovalResponse",
        "type": "object",
        "definitions": {
            "CommandExecutionApprovalDecision": {
                "oneOf": [
                    { "description": "User approved the command.", "enum": ["accept"], "type": "string" },
                    { "description": "User approved for the session.", "enum": ["acceptForSession"], "type": "string" },
                    { "description": "User denied the command. The agent will continue the turn.", "enum": ["decline"], "type": "string" },
                    { "description": "User denied the command. The turn will also be immediately interrupted.", "enum": ["cancel"], "type": "string" }
                ]
            }
        },
        "properties": { "decision": { "$ref": "#/definitions/CommandExecutionApprovalDecision" } },
        "required": ["decision"]
    }"##;

    /// file 审批样例：同构（`$defs` + `$ref` + `oneOf`；真实 schema 用 `definitions`，
    /// 这里兼容 `$defs` 拼写）。
    const FILE_SCHEMA: &str = r##"{
        "title": "FileChangeRequestApprovalResponse",
        "type": "object",
        "$defs": {
            "FileChangeApprovalDecision": {
                "oneOf": [
                    { "enum": ["accept"], "type": "string" },
                    { "enum": ["cancel"], "type": "string" },
                    { "enum": ["decline"], "type": "string" }
                ]
            }
        },
        "properties": { "decision": { "$ref": "#/$defs/FileChangeApprovalDecision" } }
    }"##;

    fn write_sample(dir: &Path, command: &str, file: &str) {
        std::fs::write(dir.join(COMMAND_APPROVAL_FILE), command).unwrap();
        std::fs::write(dir.join(FILE_APPROVAL_FILE), file).unwrap();
    }

    /// **真机金样回归**（2026-10-21，Codex 26.1013.4894.0）：`properties.decision` 是
    /// `$ref` → `definitions.*Decision.oneOf[4]`，每个词一个分支。旧实现（只看
    /// `decision.enum`）在这里只会认出第一个词，本用例钉死「四个词按序摊平」。
    #[test]
    fn parses_real_4894_approval_schema_fixtures() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("codex-desktop-ipc")
            .join("schema");
        let evidence = match collect_evidence(&dir) {
            SchemaProbeOutcome::Known(e) => e,
            other => panic!("真机 schema 夹具必须解析成功，got {other:?}"),
        };
        assert_eq!(
            evidence.command_decisions,
            vec!["accept", "acceptForSession", "decline", "cancel"],
            "command 审批：$ref → oneOf 四支必须按序摊平"
        );
        assert_eq!(
            evidence.file_decisions,
            vec!["accept", "acceptForSession", "decline", "cancel"],
            "file 审批：同构"
        );
        // 只断言「schema 里有什么」；「本轮开哪几个词」是 T7 的策略（accept + cancel），
        // 不在这里表达——`*_allows` 的语义是「schema 认识这个词」。
        assert!(evidence.command_allows("accept") && evidence.command_allows("cancel"));
        assert!(
            evidence.file_allows("decline"),
            "schema 里有 decline（策略层另判是否开放）"
        );
    }

    /// 固定样例解析：两个文件的 decision 枚举都要认出来。
    #[test]
    fn parses_fixed_schema_samples() {
        let tmp = tempfile::tempdir().unwrap();
        write_sample(tmp.path(), COMMAND_SCHEMA, FILE_SCHEMA);
        let evidence = match collect_evidence(tmp.path()) {
            SchemaProbeOutcome::Known(e) => e,
            other => panic!("固定样例必须解析成功，got {other:?}"),
        };
        assert_eq!(
            evidence.command_decisions,
            vec!["accept", "acceptForSession", "decline", "cancel"]
        );
        assert_eq!(evidence.file_decisions, vec!["accept", "cancel", "decline"]);
        assert!(evidence.command_allows("accept"));
        assert!(evidence.command_allows("cancel"));
        assert!(!evidence.command_allows("approve"), "没写的词一律不认");
        assert!(evidence.file_allows("decline"));
        assert!(!evidence.fingerprint.is_empty());
    }

    /// 枚举形态兜底：没有 `decision` 键时按「含 accept 的 enum」找。
    #[test]
    fn falls_back_to_enum_containing_accept() {
        let root: Value = serde_json::from_str(
            r#"{"oneOf":[{"enum":["no","yes"]},{"enum":["accept","cancel"]}]}"#,
        )
        .unwrap();
        assert_eq!(
            decision_enum(&root),
            Some(vec!["accept".to_string(), "cancel".to_string()])
        );
        // 完全认不出 → None（不猜）
        let junk: Value = serde_json::from_str(r#"{"type":"object"}"#).unwrap();
        assert_eq!(decision_enum(&junk), None);
    }

    /// 缺定义文件 / JSON 非法 → `Unknown` 且带原因码（审批不开放）。
    #[test]
    fn missing_or_broken_definition_degrades_to_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        // 缺 file 定义
        std::fs::write(tmp.path().join(COMMAND_APPROVAL_FILE), COMMAND_SCHEMA).unwrap();
        let outcome = collect_evidence(tmp.path());
        assert_eq!(
            outcome,
            SchemaProbeOutcome::Unknown {
                error_code: "missing_definition".into()
            }
        );
        // file 定义非法 JSON
        std::fs::write(tmp.path().join(FILE_APPROVAL_FILE), "{not json").unwrap();
        assert_eq!(
            collect_evidence(tmp.path()),
            SchemaProbeOutcome::Unknown {
                error_code: "unparsable_decision".into()
            }
        );
        // JSON 合法但没有 decision 枚举
        std::fs::write(tmp.path().join(FILE_APPROVAL_FILE), r#"{"type":"object"}"#).unwrap();
        assert_eq!(
            collect_evidence(tmp.path()),
            SchemaProbeOutcome::Unknown {
                error_code: "unparsable_decision".into()
            }
        );
    }

    /// 命令失败降级：CLI 路径不存在 → `spawn_failed`（不 panic、不猜）。
    #[test]
    fn spawn_failure_degrades_to_unknown() {
        with_temp_out_dir(|dir| {
            let missing = dir.join("no-such-codex");
            let outcome = probe_schema_blocking(&missing);
            assert_eq!(
                outcome,
                SchemaProbeOutcome::Unknown {
                    error_code: "spawn_failed".into()
                }
            );
        });
    }

    /// 临时目录**用完即删**（成功/失败都删；这里用守卫直接钉住清理行为）。
    #[test]
    fn temp_out_dir_is_removed_after_use() {
        let captured = with_temp_out_dir(|dir| {
            std::fs::write(dir.join("x.json"), "{}").unwrap();
            assert!(dir.is_dir(), "闭包内目录必须存在");
            dir.to_path_buf()
        });
        assert!(
            !captured.exists(),
            "闭包退出后临时目录必须被删除（不留 schema 残留）"
        );
    }

    /// 指纹只对「文件集合 + 大小」敏感：内容改写不改名不改大小 → 指纹不变
    /// （归因用，不参与兼容判定）。
    #[test]
    fn fingerprint_tracks_names_and_sizes_only() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        write_sample(a.path(), COMMAND_SCHEMA, FILE_SCHEMA);
        write_sample(b.path(), COMMAND_SCHEMA, FILE_SCHEMA);
        assert_eq!(schema_fingerprint(a.path()), schema_fingerprint(b.path()));

        // 同大小的内容变化：指纹不变（这是设计要的口径）
        let same_len_other = COMMAND_SCHEMA.replace("decline", "DECLINE");
        assert_eq!(same_len_other.len(), COMMAND_SCHEMA.len());
        write_sample(b.path(), &same_len_other, FILE_SCHEMA);
        assert_eq!(schema_fingerprint(a.path()), schema_fingerprint(b.path()));

        // 大小一变 → 指纹变
        write_sample(b.path(), &format!("{COMMAND_SCHEMA} "), FILE_SCHEMA);
        assert_ne!(schema_fingerprint(a.path()), schema_fingerprint(b.path()));
    }
}
