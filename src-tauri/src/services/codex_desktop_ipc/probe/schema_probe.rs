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

/// 找 decision 枚举：先按 `"decision"` 键定位（schema 的常规形态），找不到再退一步
/// 找「含 `accept` 的字符串数组枚举」。两条都不可得 → `None`（不猜）。
pub fn decision_enum(root: &Value) -> Option<Vec<String>> {
    if let Some(found) = find_decision_key(root) {
        return Some(found);
    }
    find_enum_containing(root, "accept")
}

/// 递归找 `"decision"` → `{ "enum": [...] }`（也兼容直接 `"decision": [ ... ]`）。
fn find_decision_key(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::Object(map) => {
            if let Some(dec) = map.get("decision") {
                if let Some(list) = enum_strings(dec) {
                    return Some(list);
                }
            }
            for child in map.values() {
                if let Some(found) = find_decision_key(child) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(find_decision_key),
        _ => None,
    }
}

fn enum_strings(value: &Value) -> Option<Vec<String>> {
    let arr = match value {
        Value::Array(items) => items,
        Value::Object(map) => map.get("enum")?.as_array()?,
        _ => return None,
    };
    let out: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    (!out.is_empty()).then_some(out)
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

    /// 固定样例：官方 schema 的常规形态（`properties.decision.enum`）。
    const COMMAND_SCHEMA: &str = r#"{
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "CommandExecutionRequestApprovalResponse",
        "type": "object",
        "properties": {
            "decision": {
                "type": "string",
                "enum": ["accept", "acceptForSession", "decline", "cancel"]
            }
        },
        "required": ["decision"]
    }"#;

    /// file 审批样例：枚举挂在 `$defs` 下（另一种真实形态）。
    const FILE_SCHEMA: &str = r##"{
        "title": "FileChangeRequestApprovalResponse",
        "type": "object",
        "$defs": {
            "Decision": { "type": "string", "enum": ["accept", "cancel", "decline"] }
        },
        "properties": { "decision": { "$ref": "#/$defs/Decision" } }
    }"##;

    fn write_sample(dir: &Path, command: &str, file: &str) {
        std::fs::write(dir.join(COMMAND_APPROVAL_FILE), command).unwrap();
        std::fs::write(dir.join(FILE_APPROVAL_FILE), file).unwrap();
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
