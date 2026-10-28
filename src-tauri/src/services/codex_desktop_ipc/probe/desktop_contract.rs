//! 当前官方运行包的**逐操作契约识别**（F01／T01）。
//!
//! 设计依据：spec §3.2（新版本如何继续可用）与执行计划 T01。
//!
//! **做什么**：从运行中的官方 Desktop bundle（macOS `.app`／Windows MSIX 包）定位
//! `app.asar`，只读解析其目录，按有版本的有限结构规则识别本阶段所需的方法表、
//! handler 形状与信封语义，产出逐操作 [`DesktopContractEvidence`]。
//!
//! **不做什么**（纪律）：
//! * 不执行包内 JS（没有 JS 引擎，也不为识别启动任何子进程）；
//! * 不发任何 IPC 写帧、不连接 Desktop（纯文件读取）；
//! * 不把"方法名存在"当"契约匹配"——每个操作要求各自的 handler 形状与语义证据；
//! * 不整包解压进内存：目录与目标 JS 分块读，包内容 SHA-256 流式计算。
//!
//! 当前规则覆盖：start-turn v2、submit-user-input v1、command/file decision v1、
//! interrupt v4 + expectedTurnId、异步 Question 信封（`questionItemId` 合成规则与
//! 单条 min(1) 解析）。本阶段不识别 steer（明确不接入答题路径）。

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::services::codex_desktop_ipc::capability::{
    CodexOperation, ContractMatch, ContractTarget, DesktopContractEvidence, CONTRACT_RULE_VERSION,
};
use crate::services::codex_desktop_ipc::probe::fingerprint::macos_cli_in_bundle;
use crate::services::codex_desktop_ipc::types::ProtocolError;

/// ASAR 目录头中"文件表"JSON 的长度上限（防御畸形包：真实包约 6 MB）。
const MAX_HEADER_JSON: u64 = 64 * 1024 * 1024;

/// 单个候选 JS 文件大小上限（契约只读 `.vite/build/*.js`；真实最大约 5 MB）。
const MAX_CANDIDATE_FILE: u64 = 48 * 1024 * 1024;

/// 候选 JS 文件数量上限（真实约 38 个；防止畸形目录把读取变成全包扫描）。
const MAX_CANDIDATE_FILES: usize = 128;

/// 契约识别的协议 JS 目录（Electron 主进程打包产物）。
const PROTOCOL_JS_DIR: &str = "/.vite/build/";

/// 官方包内 `app.asar` 的相对路径（按平台）。
pub fn asar_rel_path(platform: &str) -> Option<&'static str> {
    match platform.trim().to_ascii_lowercase().as_str() {
        "macos" => Some("Contents/Resources/app.asar"),
        "windows" => Some("app/resources/app.asar"),
        _ => None,
    }
}

/// 官方安装候选定位器（只返回**已按身份判据过滤**的 bundle 路径，排序去重）。
///
/// 生产实现扫本机运行进程；测试注入内存实现，**不真实扫描／连接本机 App**。
pub trait PackageLocator: Send + Sync {
    fn official_bundles(&self, platform: &str) -> Result<Vec<PathBuf>, ProtocolError>;
}

/// 生产定位器：运行中官方 Desktop 的 bundle（与 endpoint／指纹同一套身份判据）。
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessPackageLocator;

impl PackageLocator for ProcessPackageLocator {
    fn official_bundles(&self, platform: &str) -> Result<Vec<PathBuf>, ProtocolError> {
        match platform.trim().to_ascii_lowercase().as_str() {
            "macos" => {
                Ok(crate::services::codex_desktop_ipc::transport::unix::official_codex_bundles())
            }
            "windows" => Ok(
                crate::services::codex_desktop_ipc::transport::windows::official_desktop_bundles(),
            ),
            other => Err(ProtocolError::UnsupportedMethod(format!(
                "contract discovery unsupported platform: {other}"
            ))),
        }
    }
}

/// 解析当前契约目标：**只**接受官方运行身份给出的唯一安装。
///
/// 失败语义（全部关闭对应能力，不猜）：
/// * 无官方安装 → `Disconnected`；
/// * 多义（>1 个官方安装）→ `InvalidState`（不选"第一个"，也不按版本排序挑新）；
/// * 包内缺 `app.asar`／缺随包 CLI → `Disconnected`；
/// * `key` 与 `endpoint` 平台不一致 → `InvalidState`（防止跨平台证据串用）。
pub fn resolve_contract_target(
    endpoint: &crate::services::codex_desktop_ipc::transport::DesktopEndpoint,
    key: &crate::services::codex_desktop_ipc::probe::fingerprint::ProbeKey,
    epoch: u64,
) -> Result<ContractTarget, ProtocolError> {
    resolve_contract_target_with(&ProcessPackageLocator, endpoint, key, epoch)
}

/// 平台 + 探测键直达的解析入口（adapter 用：它只有 build/platform 事实，没有
/// socket 路径；本函数只依赖 `endpoint.platform`，与 [`resolve_contract_target`] 同源）。
pub fn resolve_contract_target_for(
    platform: &str,
    key: &crate::services::codex_desktop_ipc::probe::fingerprint::ProbeKey,
    epoch: u64,
) -> Result<ContractTarget, ProtocolError> {
    let endpoint = crate::services::codex_desktop_ipc::transport::DesktopEndpoint {
        platform: platform.to_string(),
        path: String::new(),
        build: key.build.clone(),
    };
    resolve_contract_target(&endpoint, key, epoch)
}

/// 契约识别失败的机器码（诊断只记类别/阶段，不带路径与正文）。
pub fn contract_error_code(error: &ProtocolError) -> &'static str {
    match error {
        ProtocolError::Disconnected(_) => "contract_package_unavailable",
        ProtocolError::InvalidFrame(_) => "contract_package_malformed",
        ProtocolError::StaleContext(_) => "contract_package_changed",
        ProtocolError::InvalidState(_) => "contract_target_ambiguous",
        ProtocolError::UnsupportedMethod(_) => "contract_platform_unsupported",
        ProtocolError::Timeout(_) => "contract_read_timeout",
        ProtocolError::Capacity(_) => "contract_read_over_budget",
        ProtocolError::InvalidId(_) => "contract_invalid_target",
    }
}

/// 可注入定位器的解析入口（测试用）。
pub fn resolve_contract_target_with(
    locator: &dyn PackageLocator,
    endpoint: &crate::services::codex_desktop_ipc::transport::DesktopEndpoint,
    key: &crate::services::codex_desktop_ipc::probe::fingerprint::ProbeKey,
    epoch: u64,
) -> Result<ContractTarget, ProtocolError> {
    let platform = endpoint.platform.trim().to_ascii_lowercase();
    if key.platform != platform {
        return Err(ProtocolError::InvalidState(format!(
            "probe key platform {} does not match endpoint platform {platform}",
            key.platform
        )));
    }
    let rel = asar_rel_path(&platform).ok_or_else(|| {
        ProtocolError::UnsupportedMethod(format!(
            "contract discovery unsupported platform: {platform}"
        ))
    })?;
    let mut bundles = locator.official_bundles(&platform)?;
    bundles.sort();
    bundles.dedup();
    match bundles.len() {
        0 => Err(ProtocolError::Disconnected(
            "no running official Codex Desktop installation".into(),
        )),
        1 => {
            let bundle = &bundles[0];
            let package_path = bundle.join(rel);
            if !package_path.is_file() {
                return Err(ProtocolError::Disconnected(format!(
                    "official package not found: {}",
                    package_path.display()
                )));
            }
            let cli_path = cli_path_in_bundle(&platform, bundle).ok_or_else(|| {
                ProtocolError::Disconnected(format!(
                    "bundled CLI not found in {}",
                    bundle.display()
                ))
            })?;
            Ok(ContractTarget {
                probe_key: key.clone(),
                connection_epoch: epoch,
                package_path,
                cli_path,
            })
        }
        n => Err(ProtocolError::InvalidState(format!(
            "ambiguous official Codex Desktop installation: {n} candidates"
        ))),
    }
}

/// 平台内的随包 CLI 路径（只做存在性判定；不执行）。
fn cli_path_in_bundle(platform: &str, bundle: &Path) -> Option<PathBuf> {
    match platform {
        "macos" => macos_cli_in_bundle(bundle),
        "windows" => {
            let cli = bundle
                .join("app")
                .join("resources")
                .join("codex-cli")
                .join("bin")
                .join("codex.exe");
            cli.is_file().then_some(cli)
        }
        _ => None,
    }
}

/// 只读观察钩子：测试用（在哈希完成后注入"包被改写"的定序），生产恒为 [`NoopObserver`]。
///
/// 不是公开 API 的一部分：`read_desktop_contract` 永远走 `NoopObserver`；
/// 只有 crate 内测试使用带 observer 的入口，避免随机 sleep 猜竞争。
pub trait ContractReadObserver: Send + Sync {
    fn after_hash(&self, _package: &Path) {}
}

/// 空观察者。
pub struct NoopObserver;

impl ContractReadObserver for NoopObserver {}

/// 读取当前包的契约证据（阻塞只读；由 `ProductionContractReader` 放进
/// `spawn_blocking`）。
pub fn read_desktop_contract(
    target: &ContractTarget,
) -> Result<DesktopContractEvidence, ProtocolError> {
    read_desktop_contract_observed(target, &NoopObserver)
}

/// 带观察钩子的读取入口（测试专用）。
pub fn read_desktop_contract_observed(
    target: &ContractTarget,
    observer: &dyn ContractReadObserver,
) -> Result<DesktopContractEvidence, ProtocolError> {
    let before = package_stamp(&target.package_path)?;
    let (sha256, candidates) = read_package(&target.package_path, observer)?;
    let after = package_stamp(&target.package_path)?;
    if before != after {
        // 读期间包被替换/改写：本份证据不可归因到任何确定内容 → 拒绝安装。
        return Err(ProtocolError::StaleContext(
            "official package changed while reading contract".into(),
        ));
    }
    if candidates.is_empty() {
        return Err(ProtocolError::InvalidState(
            "official package has no protocol bundle under .vite/build".into(),
        ));
    }
    let matches = match_operations(&candidates);
    Ok(DesktopContractEvidence {
        probe_key: target.target_key(),
        package_sha256: sha256,
        rule_version: CONTRACT_RULE_VERSION,
        matches,
        package_size: Some(after.len),
        package_mtime_ms: after.modified.and_then(mtime_ms),
    })
}

/// H14：流式计算官方包内容 SHA-256（缓存复用前的**内容**核对；stat 可被伪造，
/// 只有内容哈希能证明"同一份包"）。只读、无解包。
pub fn hash_package(path: &std::path::Path) -> Result<String, ProtocolError> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path).map_err(|e| {
        ProtocolError::Disconnected(format!(
            "official package unreadable {}: {e}",
            path.display()
        ))
    })?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        use std::io::Read;
        let n = file.read(&mut buf).map_err(|e| {
            ProtocolError::Disconnected(format!("official package read failed: {e}"))
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// 包的"同一性戳"：长度 + 修改时间（读前读后比较；变化即 StaleContext）。
/// 把系统时间折叠为毫秒（供持久缓存核对当前包内容；失败 → None = 未记录）。
fn mtime_ms(time: SystemTime) -> Option<i64> {
    time.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PackageStamp {
    len: u64,
    modified: Option<SystemTime>,
}

fn package_stamp(path: &Path) -> Result<PackageStamp, ProtocolError> {
    let meta = std::fs::metadata(path).map_err(|e| {
        ProtocolError::Disconnected(format!(
            "official package unreadable {}: {e}",
            path.display()
        ))
    })?;
    if !meta.is_file() {
        return Err(ProtocolError::Disconnected(format!(
            "official package is not a file: {}",
            path.display()
        )));
    }
    Ok(PackageStamp {
        len: meta.len(),
        modified: meta.modified().ok(),
    })
}

/// 流式哈希整包并抽取候选协议 JS（每次调用只读一遍包）。
fn read_package(
    path: &Path,
    observer: &dyn ContractReadObserver,
) -> Result<(String, Vec<(String, String)>), ProtocolError> {
    let mut file = std::fs::File::open(path).map_err(|e| {
        ProtocolError::Disconnected(format!(
            "official package unreadable {}: {e}",
            path.display()
        ))
    })?;
    let len = file
        .metadata()
        .map_err(|e| ProtocolError::Disconnected(format!("official package stat failed: {e}")))?
        .len();

    // 1) 目录头：16 字节定长前缀（[4][header_size][…][json_size]）。
    let mut prefix = [0u8; 16];
    file.read_exact(&mut prefix).map_err(|e| {
        ProtocolError::InvalidFrame(format!("package too short for ASAR header: {e}"))
    })?;
    let prefix_kind = u32::from_le_bytes(prefix[0..4].try_into().unwrap());
    let header_size = u32::from_le_bytes(prefix[4..8].try_into().unwrap()) as u64;
    let json_size = u32::from_le_bytes(prefix[12..16].try_into().unwrap()) as u64;
    if prefix_kind != 4 {
        return Err(ProtocolError::InvalidFrame(format!(
            "unexpected ASAR header prefix: {prefix_kind}"
        )));
    }
    let json_start = 16u64;
    let data_start = 8u64
        .checked_add(header_size)
        .ok_or_else(|| ProtocolError::InvalidFrame("ASAR header size overflow".into()))?;
    if header_size < 8 || json_size == 0 || json_size > MAX_HEADER_JSON {
        return Err(ProtocolError::InvalidFrame(format!(
            "ASAR header sizes out of range: header={header_size} json={json_size}"
        )));
    }
    if json_start + json_size > data_start || data_start > len {
        return Err(ProtocolError::InvalidFrame(format!(
            "ASAR header overruns package: json_end={} data_start={data_start} file_len={len}",
            json_start + json_size
        )));
    }

    let mut header_json = vec![0u8; json_size as usize];
    file.read_exact(&mut header_json)
        .map_err(|e| ProtocolError::InvalidFrame(format!("ASAR header truncated: {e}")))?;
    let directory: serde_json::Value = serde_json::from_slice(&header_json)
        .map_err(|e| ProtocolError::InvalidFrame(format!("ASAR directory is not JSON: {e}")))?;
    let entries = collect_entries(&directory);

    // 2) 逐文件流式读候选（顺序读，失败只报错不重试）。
    let mut candidates: Vec<(String, String)> = Vec::new();
    let mut entries: Vec<AsarEntry> = entries
        .into_iter()
        .filter(|e| {
            !e.unpacked
                && e.name.starts_with(PROTOCOL_JS_DIR)
                && e.name.ends_with(".js")
                && e.size <= MAX_CANDIDATE_FILE
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries.truncate(MAX_CANDIDATE_FILES);
    for entry in &entries {
        let start = data_start.checked_add(entry.offset).ok_or_else(|| {
            ProtocolError::InvalidFrame(format!("ASAR entry offset overflow: {}", entry.name))
        })?;
        let end = start.checked_add(entry.size).ok_or_else(|| {
            ProtocolError::InvalidFrame(format!("ASAR entry size overflow: {}", entry.name))
        })?;
        if end > len {
            return Err(ProtocolError::InvalidFrame(format!(
                "ASAR entry out of bounds: {} [{start}..{end}) file_len={len}",
                entry.name
            )));
        }
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(start))
            .map_err(|e| ProtocolError::InvalidFrame(format!("ASAR seek failed: {e}")))?;
        let mut body = vec![0u8; entry.size as usize];
        file.read_exact(&mut body)
            .map_err(|e| ProtocolError::InvalidFrame(format!("ASAR entry truncated: {e}")))?;
        let text = String::from_utf8_lossy(&body).into_owned();
        candidates.push((entry.name.clone(), text));
    }

    // 3) 整包 SHA-256（独立一遍流式读；不动候选内容）。
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))
        .map_err(|e| ProtocolError::Disconnected(format!("package seek failed: {e}")))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| ProtocolError::Disconnected(format!("package read failed: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let sha256: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    observer.after_hash(path);
    Ok((sha256, candidates))
}

/// ASAR 目录里的一个普通文件项。
#[derive(Debug, Clone, PartialEq, Eq)]
struct AsarEntry {
    /// 以 `/` 开头的包内绝对路径（与真实 asar 目录同形）。
    name: String,
    size: u64,
    offset: u64,
    unpacked: bool,
}

fn collect_entries(directory: &serde_json::Value) -> Vec<AsarEntry> {
    fn walk(node: &serde_json::Value, prefix: &str, out: &mut Vec<AsarEntry>) {
        let Some(files) = node.get("files").and_then(|v| v.as_object()) else {
            return;
        };
        for (name, meta) in files {
            let path = format!("{prefix}/{name}");
            if meta.get("files").is_some() {
                walk(meta, &path, out);
                continue;
            }
            let size = meta.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
            // 真实 asar 的 offset 是**字符串**（如 "0"）；兼容数字形态。
            let offset = meta
                .get("offset")
                .and_then(|v| {
                    v.as_str()
                        .and_then(|s| s.parse::<u64>().ok())
                        .or_else(|| v.as_u64())
                })
                .unwrap_or(0);
            let unpacked = meta
                .get("unpacked")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            out.push(AsarEntry {
                name: path,
                size,
                offset,
                unpacked,
            });
        }
    }
    let mut out = Vec::new();
    walk(directory, "", &mut out);
    out
}

// ============================================================
// 结构匹配器（有限词法，按操作独立）
// ============================================================

/// 单操作的识别规则：版本表项 + 若干必须同时出现的结构字面。
struct OperationRule {
    operation: CodexOperation,
    method: &'static str,
    base_version: u32,
    requires: &'static [&'static str],
    request_rule: &'static str,
    response_rule: &'static str,
    semantic_rule: &'static str,
}

/// 当前规则版本下的全部识别规则。
///
/// **刻意不含** `steer`（本阶段明确不接入答题路径）、`request_cancel`（无已核验专用
/// RPC）与 mode／permissions 组（F09—F11 不新增写能力）。
const RULES: &[OperationRule] = &[
    OperationRule {
        operation: CodexOperation::Send,
        method: "thread-follower-start-turn",
        base_version: 2,
        requires: &[
            "case\"thread-follower-start-turn\":",
            "startTurn(t.params.conversationId,t.params.turnStart)",
            "{method:t.method,result:{result:",
        ],
        request_rule: "conversationId+turnStart",
        response_rule: "result.result",
        semantic_rule: "start-turn v2 承载普通消息",
    },
    OperationRule {
        operation: CodexOperation::CommandApproval,
        method: "thread-follower-command-approval-decision",
        base_version: 1,
        requires: &[
            "case\"thread-follower-command-approval-decision\":",
            "replyWithCommandExecutionApprovalDecision(t.params.conversationId,t.params.requestId,t.params.decision)",
            "{method:t.method,result:{ok:!0}}",
        ],
        request_rule: "conversationId+requestId+decision",
        response_rule: "ok",
        semantic_rule: "command decision v1",
    },
    OperationRule {
        operation: CodexOperation::FileChangeApproval,
        method: "thread-follower-file-approval-decision",
        base_version: 1,
        requires: &[
            "case\"thread-follower-file-approval-decision\":",
            "replyWithFileChangeApprovalDecision(t.params.conversationId,t.params.requestId,t.params.decision)",
            "{method:t.method,result:{ok:!0}}",
        ],
        request_rule: "conversationId+requestId+decision",
        response_rule: "ok",
        semantic_rule: "file decision v1",
    },
    OperationRule {
        operation: CodexOperation::BlockingInput,
        method: "thread-follower-submit-user-input",
        base_version: 1,
        requires: &[
            "case\"thread-follower-submit-user-input\":",
            "replyWithUserInputResponse(t.params.conversationId,t.params.requestId,t.params.response)",
            "{method:t.method,result:{ok:!0}}",
        ],
        request_rule: "conversationId+requestId+response",
        response_rule: "ok",
        semantic_rule: "blocking user-input v1",
    },
    OperationRule {
        operation: CodexOperation::AsyncAnswerIdle,
        method: "thread-follower-start-turn",
        base_version: 2,
        requires: &[
            "<send_user_message_question_reply>",
            "</send_user_message_question_reply>",
            "JSON.stringify([\"request_user_input_async\",",
            "questionItemId:",
            "question:r.P(),answer:r.P()",
            ".min(1)",
            "startsWith(uxe)",
            "endsWith(dxe)",
            "slice(34,-35)",
            "startTurn(t.params.conversationId,t.params.turnStart)",
        ],
        request_rule: "conversationId+turnStart{text=envelope[questionItemId,question,answer]}",
        response_rule: "result.result",
        semantic_rule: "questionItemId=JSON.stringify([\"request_user_input_async\",itemId,index]);answers=min(1)",
    },
    OperationRule {
        operation: CodexOperation::Interrupt,
        method: "thread-follower-interrupt-turn",
        base_version: 4,
        requires: &[
            "case\"thread-follower-interrupt-turn\":",
            "interruptConversation(t.params.conversationId,t.params.mode,t.params.expectedTurnId)",
            "expectedTurnId",
            "interruptedTurnId:",
        ],
        request_rule: "conversationId+mode+expectedTurnId",
        response_rule: "interruptedTurnId+ok",
        semantic_rule: "interrupt v4 要求 expectedTurnId（不降级 v3）",
    },
];

/// 归一化：去全部空白、模板反引号统一成双引号。
///
/// 让匹配对打包器的压缩/美化都不敏感；结构化规则本身要求**形状**而非"关键词出现"。
fn normalize(js: &str) -> String {
    js.chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| if c == '`' { '"' } else { c })
        .collect()
}

/// 方法版本表里的值（要求唯一；出现 `"method":` 但值是其它形态的忽略）。
fn table_version(hay: &str, method: &str) -> Option<u32> {
    let needle = format!("\"{method}\":");
    let mut values: Vec<u32> = Vec::new();
    for (idx, _) in hay.match_indices(&needle) {
        let rest = &hay[idx + needle.len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            continue;
        }
        let followed_by_table_sep = rest[digits.len()..]
            .chars()
            .next()
            .is_some_and(|c| c == ',' || c == '}');
        if !followed_by_table_sep {
            continue;
        }
        if let Ok(value) = digits.parse::<u32>() {
            values.push(value);
        }
    }
    values.sort_unstable();
    values.dedup();
    match values.as_slice() {
        [only] => Some(*only),
        _ => None,
    }
}

fn match_operations(candidates: &[(String, String)]) -> BTreeMap<CodexOperation, ContractMatch> {
    let hay = candidates
        .iter()
        .map(|(_, content)| normalize(content))
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut matches = BTreeMap::new();
    for rule in RULES {
        let Some(version) = table_version(&hay, rule.method) else {
            continue;
        };
        if version != rule.base_version {
            continue;
        }
        if !rule.requires.iter().all(|needle| hay.contains(needle)) {
            continue;
        }
        matches.insert(
            rule.operation,
            ContractMatch {
                method: rule.method.to_string(),
                base_version: version,
                request_rule: rule.request_rule.to_string(),
                response_rule: rule.response_rule.to_string(),
                semantic_rule: rule.semantic_rule.to_string(),
            },
        );
    }
    matches
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::codex_desktop_ipc::probe::fingerprint::ProbeKey;
    use crate::services::codex_desktop_ipc::transport::DesktopEndpoint;

    const VERSION_TABLE: &str = include_str!(
        "../../../../tests/fixtures/codex-desktop-ipc/observed/20261026-macos/contract-fragments/version-table.js"
    );
    const START_TURN_HANDLER: &str = include_str!(
        "../../../../tests/fixtures/codex-desktop-ipc/observed/20261026-macos/contract-fragments/start-turn-handler.js"
    );
    const DECISION_HANDLERS: &str = include_str!(
        "../../../../tests/fixtures/codex-desktop-ipc/observed/20261026-macos/contract-fragments/decision-and-user-input-handlers.js"
    );
    const INTERRUPT_HANDLER: &str = include_str!(
        "../../../../tests/fixtures/codex-desktop-ipc/observed/20261026-macos/contract-fragments/interrupt-handler.js"
    );
    const ASYNC_ENVELOPE: &str = include_str!(
        "../../../../tests/fixtures/codex-desktop-ipc/observed/20261026-macos/contract-fragments/async-envelope.js"
    );
    const START_TURN_REQUEST: &str = include_str!(
        "../../../../tests/fixtures/codex-desktop-ipc/observed/20261026-macos/contract-fragments/start-turn-request.js"
    );

    fn main_fixture_js() -> String {
        format!(
            "{START_TURN_HANDLER}\n{DECISION_HANDLERS}\n{INTERRUPT_HANDLER}\n{ASYNC_ENVELOPE}\n{START_TURN_REQUEST}\n"
        )
    }

    /// 在给定 asar 里写入 `files`（包内绝对路径 → 内容），返回写入字节数。
    fn write_asar(path: &Path, files: &[(&str, &str)]) {
        let mut root = serde_json::json!({ "files": {} });
        let mut payload: Vec<u8> = Vec::new();
        let mut offset: u64 = 0;
        for (name, content) in files {
            insert_entry(&mut root, name, content.len() as u64, offset);
            offset += content.len() as u64;
            payload.extend_from_slice(content.as_bytes());
        }
        let json = serde_json::to_vec(&root).expect("asar directory json");
        let header_size = 8 + json.len() as u64;
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&(header_size as u32).to_le_bytes());
        out.extend_from_slice(&((json.len() as u32) + 6).to_le_bytes());
        out.extend_from_slice(&(json.len() as u32).to_le_bytes());
        out.extend_from_slice(&json);
        assert_eq!(out.len() as u64, 16 + json.len() as u64);
        out.extend_from_slice(&payload);
        std::fs::write(path, out).expect("write asar");
    }

    fn insert_entry(root: &mut serde_json::Value, name: &str, size: u64, offset: u64) {
        let parts: Vec<&str> = name.trim_start_matches('/').split('/').collect();
        let mut node = root;
        for (idx, part) in parts.iter().enumerate() {
            if idx + 1 == parts.len() {
                let files = node
                    .get_mut("files")
                    .and_then(|v| v.as_object_mut())
                    .expect("files object");
                files.insert(
                    (*part).to_string(),
                    serde_json::json!({"size": size, "offset": offset.to_string()}),
                );
                return;
            }
            let files = node
                .get_mut("files")
                .and_then(|v| v.as_object_mut())
                .expect("files object");
            node = files
                .entry((*part).to_string())
                .or_insert_with(|| serde_json::json!({"files": {}}));
        }
    }

    fn target_for(package: &Path, build: &str) -> ContractTarget {
        ContractTarget {
            probe_key: ProbeKey::new("macos", Some(build), Some("0.164.0-alpha.11.1")),
            connection_epoch: 7,
            package_path: package.to_path_buf(),
            cli_path: PathBuf::from("/tmp/placeholder/codex"),
        }
    }

    fn read(package: &Path, build: &str) -> Result<DesktopContractEvidence, ProtocolError> {
        read_desktop_contract(&target_for(package, build))
    }

    fn fixture_package(dir: &Path) -> PathBuf {
        let package = dir.join("app.asar");
        write_asar(
            &package,
            &[
                ("/.vite/build/main-fixture.js", &main_fixture_js()),
                ("/.vite/build/src-fixture.js", VERSION_TABLE),
            ],
        );
        package
    }

    /// 四个核心具名用例之一：同契约、不同 build 仍识别（不能靠 build 白名单）。
    #[test]
    fn f08p_t01_same_contract_new_build_matches() {
        let dir = tempfile::tempdir().unwrap();
        let package = fixture_package(dir.path());
        let old_build = read(&package, "26.1021.31345").expect("old build evidence");
        let new_build = read(&package, "26.1101.99999").expect("new build evidence");

        assert!(new_build
            .matches
            .contains_key(&CodexOperation::AsyncAnswerIdle));
        assert!(new_build
            .matches
            .contains_key(&CodexOperation::BlockingInput));
        assert!(new_build.matches.contains_key(&CodexOperation::Interrupt));
        assert_eq!(
            new_build.matches[&CodexOperation::Interrupt].base_version,
            4
        );
        assert_eq!(new_build.matches[&CodexOperation::Send].base_version, 2);
        for op in [
            CodexOperation::Send,
            CodexOperation::CommandApproval,
            CodexOperation::FileChangeApproval,
            CodexOperation::BlockingInput,
            CodexOperation::AsyncAnswerIdle,
            CodexOperation::Interrupt,
        ] {
            assert!(old_build.matched(op), "missing operation {op:?}");
        }
        // 未接入 / 无专用 RPC 的操作不因同包出现而放行。
        assert!(!old_build.matched(CodexOperation::RequestCancel));
        assert!(!old_build.matched(CodexOperation::PermissionsApproval));
        assert!(!old_build.matched(CodexOperation::ModeRead));
        // 同包内容 → 同一内容指纹；build 只影响复用键。
        assert_eq!(old_build.package_sha256, new_build.package_sha256);
        assert_eq!(new_build.rule_version, CONTRACT_RULE_VERSION);
        assert_ne!(old_build.probe_key, new_build.probe_key);
    }

    /// 四个核心具名用例之二：只有方法关键词 / 语义漂移都不放行。
    #[test]
    fn f08p_t01_keyword_only_or_changed_shape_denied() {
        let dir = tempfile::tempdir().unwrap();
        let keyword_only = dir.path().join("keyword.asar");
        let keyword_js =
            "var t={\"thread-follower-start-turn\":2,\"thread-follower-interrupt-turn\":4,\
             \"thread-follower-command-approval-decision\":1,\
             \"thread-follower-file-approval-decision\":1,\
             \"thread-follower-submit-user-input\":1}};\n\
             var names=[\"thread-follower-start-turn\",\"thread-follower-interrupt-turn\",\
             \"thread-follower-submit-user-input\",\"request_user_input_async\"];\n";
        write_asar(
            &keyword_only,
            &[("/.vite/build/main-keyword.js", keyword_js)],
        );
        let keyword_evidence = read(&keyword_only, "26.1021.31345").expect("keyword evidence");
        assert!(!keyword_evidence
            .matches
            .contains_key(&CodexOperation::AsyncAnswerIdle));
        assert!(
            keyword_evidence.matches.is_empty(),
            "method names alone must not match: {:?}",
            keyword_evidence.matches.keys().collect::<Vec<_>>()
        );

        let changed = dir.path().join("changed.asar");
        let drifted =
            main_fixture_js().replace("replyWithUserInputResponse", "submitUserInputResponseV2");
        write_asar(
            &changed,
            &[
                ("/.vite/build/main-fixture.js", &drifted),
                ("/.vite/build/src-fixture.js", VERSION_TABLE),
            ],
        );
        let changed_evidence = read(&changed, "26.1021.31345").expect("changed evidence");
        assert!(!changed_evidence.matched(CodexOperation::BlockingInput));
        assert!(changed_evidence.matched(CodexOperation::Send));
        assert!(changed_evidence.matched(CodexOperation::AsyncAnswerIdle));
    }

    /// 四个核心具名用例之三：同尺寸、内容变化 → 内容指纹必须变化。
    #[test]
    fn f08p_t01_same_size_changed_content_changes_hash() {
        let dir = tempfile::tempdir().unwrap();
        let package = fixture_package(dir.path());
        let len_before = std::fs::metadata(&package).unwrap().len();
        let a = read(&package, "26.1021.31345").expect("first read");

        // 同尺寸改一个字节（把 `questionItemId` 改写成 `questionItemIe`）。
        let mut bytes = std::fs::read(&package).unwrap();
        let needle = b"questionItemId";
        let at = bytes
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("marker present");
        bytes[at + needle.len() - 1] = b'e';
        std::fs::write(&package, &bytes).unwrap();
        let len_after = std::fs::metadata(&package).unwrap().len();
        assert_eq!(len_before, len_after);

        let b = read(&package, "26.1021.31345").expect("second read");
        assert_ne!(a.package_sha256, b.package_sha256);
    }

    /// 四个核心具名用例之四：安装多义 → 不产出目标（不挑一个凑合）。
    #[test]
    fn f08p_t01_ambiguous_package_denied() {
        let dir = tempfile::tempdir().unwrap();
        let bundle_one = make_bundle(dir.path(), "One");
        let bundle_two = make_bundle(dir.path(), "Two");
        let locator = FakeLocator(vec![bundle_one, bundle_two]);
        let endpoint = DesktopEndpoint {
            platform: "macos".into(),
            path: "/tmp/ipc.sock".into(),
            build: Some("26.1021.31345".into()),
        };
        let key = ProbeKey::new("macos", Some("26.1021.31345"), Some("0.164.0-alpha.11.1"));
        let err = resolve_contract_target_with(&locator, &endpoint, &key, 3)
            .expect_err("ambiguous installation must not resolve");
        assert!(
            matches!(err, ProtocolError::InvalidState(ref m) if m.contains("ambiguous")),
            "unexpected: {err:?}"
        );

        // 唯一安装 → 目标取自官方 bundle（路径来自本机身份，不来自 HTTP）。
        let locator = FakeLocator(vec![make_bundle(dir.path(), "Solo")]);
        let target = resolve_contract_target_with(&locator, &endpoint, &key, 9)
            .expect("single installation resolves");
        assert_eq!(target.connection_epoch, 9);
        assert_eq!(target.probe_key, key);
        assert!(target.package_path.ends_with("Contents/Resources/app.asar"));
        assert!(target.cli_path.exists());

        // 无安装 → 诚实失败（不返回"空证据"冒充已识别）。
        let empty = FakeLocator(Vec::new());
        assert!(resolve_contract_target_with(&empty, &endpoint, &key, 1).is_err());

        // 平台不一致 → 拒绝（防跨平台证据串用）。
        let wrong_platform = ProbeKey::new("windows", Some("26.1021.31345"), None);
        assert!(resolve_contract_target_with(
            &FakeLocator(vec![make_bundle(dir.path(), "Solo2")]),
            &endpoint,
            &wrong_platform,
            1
        )
        .is_err());
    }

    struct FakeLocator(Vec<PathBuf>);

    impl PackageLocator for FakeLocator {
        fn official_bundles(&self, _platform: &str) -> Result<Vec<PathBuf>, ProtocolError> {
            Ok(self.0.clone())
        }
    }

    fn make_bundle(root: &Path, name: &str) -> PathBuf {
        let bundle = root.join(format!("{name}.app"));
        let resources = bundle.join("Contents").join("Resources");
        std::fs::create_dir_all(resources.join("codex-cli").join("bin")).unwrap();
        write_asar(
            &resources.join("app.asar"),
            &[
                ("/.vite/build/main-fixture.js", &main_fixture_js()),
                ("/.vite/build/src-fixture.js", VERSION_TABLE),
            ],
        );
        std::fs::write(
            resources.join("codex-cli").join("bin").join("codex"),
            b"#!/bin/sh\n",
        )
        .unwrap();
        bundle
    }

    /// ASAR 长度/越界拒绝（畸形包不得被解释成"已识别"）。
    #[test]
    fn f08p_t01_asar_invalid_length_or_bounds_rejected() {
        let dir = tempfile::tempdir().unwrap();

        let truncated = dir.path().join("truncated.asar");
        std::fs::write(&truncated, [4u8, 0, 0, 0]).unwrap();
        assert!(read(&truncated, "b").is_err());

        let overrun = dir.path().join("overrun.asar");
        let mut bytes = std::fs::read(fixture_package(dir.path())).unwrap();
        let declared_json = (bytes.len() as u32) + 4096;
        bytes[12..16].copy_from_slice(&declared_json.to_le_bytes());
        std::fs::write(&overrun, &bytes).unwrap();
        assert!(read(&overrun, "b").is_err());

        let entry_oob = dir.path().join("entry-oob.asar");
        let mut bytes = std::fs::read(fixture_package(dir.path())).unwrap();
        // 截掉尾部一个字节：目录仍声明原长度 → 条目越界。
        bytes.pop();
        std::fs::write(&entry_oob, &bytes).unwrap();
        let err = read(&entry_oob, "b").expect_err("entry beyond file must be rejected");
        assert!(
            matches!(err, ProtocolError::InvalidFrame(ref m) if m.contains("out of bounds")),
            "unexpected: {err:?}"
        );
    }

    /// 读取期间包变化 → 拒绝（不把两次读到的混合内容当同一份证据）。
    #[test]
    fn f08p_t01_package_changed_during_read_rejected() {
        struct AppendingObserver {
            ran: std::sync::atomic::AtomicBool,
        }
        impl ContractReadObserver for AppendingObserver {
            fn after_hash(&self, package: &Path) {
                use std::io::Write;
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(package)
                    .expect("open for append");
                file.write_all(b"x").expect("append byte");
                self.ran.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let package = fixture_package(dir.path());
        let observer = AppendingObserver {
            ran: std::sync::atomic::AtomicBool::new(false),
        };
        let err = read_desktop_contract_observed(&target_for(&package, "b"), &observer)
            .expect_err("changed package must be rejected");
        assert!(observer.ran.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            matches!(err, ProtocolError::StaleContext(_)),
            "unexpected: {err:?}"
        );
    }

    /// interrupt 必须 v4 + expectedTurnId：版本降级或语义漂移都关闭。
    #[test]
    fn f08p_t01_interrupt_requires_v4_and_expected_turn_id() {
        let dir = tempfile::tempdir().unwrap();
        let downgraded = dir.path().join("v3.asar");
        let table_v3 = VERSION_TABLE.replace(
            "\"thread-follower-interrupt-turn\":4",
            "\"thread-follower-interrupt-turn\":3",
        );
        write_asar(
            &downgraded,
            &[
                ("/.vite/build/main-fixture.js", &main_fixture_js()),
                ("/.vite/build/src-fixture.js", &table_v3),
            ],
        );
        let evidence = read(&downgraded, "b").expect("v3 evidence");
        assert!(!evidence.matched(CodexOperation::Interrupt));
        assert!(evidence.matched(CodexOperation::Send));

        let untargeted = dir.path().join("untargeted.asar");
        let no_expected = main_fixture_js().replace(
            "t.params.mode,t.params.expectedTurnId",
            "t.params.mode,t.params.turnId",
        );
        write_asar(
            &untargeted,
            &[
                ("/.vite/build/main-fixture.js", &no_expected),
                ("/.vite/build/src-fixture.js", VERSION_TABLE),
            ],
        );
        let evidence = read(&untargeted, "b").expect("untargeted evidence");
        assert!(!evidence.matched(CodexOperation::Interrupt));
    }

    /// 识别过程不执行包内 JS（没有 JS 引擎；不产生任何副作用）。
    #[test]
    fn f08p_t01_never_executes_package_javascript() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("executed.marker");
        let package = dir.path().join("payload.asar");
        let side_effect = format!(
            "require(\"fs\").writeFileSync({:?}, \"executed\");\n",
            marker.display().to_string()
        );
        write_asar(
            &package,
            &[
                ("/.vite/build/main-fixture.js", &main_fixture_js()),
                ("/.vite/build/src-fixture.js", VERSION_TABLE),
                ("/.vite/build/malicious.js", &side_effect),
            ],
        );
        let evidence = read(&package, "b").expect("evidence");
        assert!(evidence.matched(CodexOperation::Send));
        assert!(!marker.exists(), "package JS must never be executed");
    }

    /// 本机真实安装包冒烟（默认 ignore）：T01 交付时手工跑一次，证明有限结构规则
    /// 对**当前实际安装**成立；不作为常规门禁（CI 无官方 Desktop 安装）。
    #[test]
    #[ignore = "需要本机安装官方 Codex Desktop；手工取证用"]
    fn f08p_t01_local_installed_package_smoke() {
        let candidates = [
            "/Applications/ChatGPT.app/Contents/Resources/app.asar",
            "/Applications/Codex.app/Contents/Resources/app.asar",
        ];
        let Some(package) = candidates
            .iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
        else {
            eprintln!("no local official package; nothing to verify");
            return;
        };
        let evidence = read(&package, "local").expect("real package evidence");
        eprintln!(
            "package sha256={} matches={:?}",
            evidence.package_sha256,
            evidence
                .matches
                .iter()
                .map(|(op, m)| (*op, m.base_version))
                .collect::<Vec<_>>()
        );
        for op in [
            CodexOperation::Send,
            CodexOperation::CommandApproval,
            CodexOperation::FileChangeApproval,
            CodexOperation::BlockingInput,
            CodexOperation::AsyncAnswerIdle,
            CodexOperation::Interrupt,
        ] {
            assert!(evidence.matched(op), "missing operation {op:?}");
        }
    }

    /// 证据结构与"证据来自当前包"的最低不变量（不携带会话正文）。
    #[test]
    fn f08p_t01_evidence_carries_structure_not_session_text() {
        let dir = tempfile::tempdir().unwrap();
        let package = fixture_package(dir.path());
        let evidence = read(&package, "b").expect("evidence");
        assert_eq!(evidence.package_sha256.len(), 64);
        for (operation, matched) in &evidence.matches {
            assert!(!matched.method.is_empty(), "{operation:?} method");
            assert!(
                matched.request_rule.contains("conversationId")
                    || matched.request_rule.contains("envelope"),
                "{operation:?} request rule: {}",
                matched.request_rule
            );
            assert!(!matched.semantic_rule.is_empty());
        }
    }
}
