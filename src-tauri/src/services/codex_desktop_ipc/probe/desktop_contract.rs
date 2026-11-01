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
//! 单条 min(1) 解析；标记绑定／schema／守卫按**引用关系**核验，压缩器改局部名不失配，
//! 见 [`async_answer_envelope_structure`]）。本阶段不识别 steer（明确不接入答题路径）。

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

/// 单操作的识别规则：版本表项 + 若干必须同时出现的结构字面 + 可选结构核验。
struct OperationRule {
    operation: CodexOperation,
    method: &'static str,
    base_version: u32,
    /// 必须逐字出现的片段：**只放协议常量与方法级形状**，不放压缩器产物。
    requires: &'static [&'static str],
    /// 字面之外的引用关系核验（标识符可任意一致改名）；`None` = 只看字面。
    structural: Option<fn(&str) -> bool>,
    request_rule: &'static str,
    response_rule: &'static str,
    semantic_rule: &'static str,
}

/// 异步答题信封的开/闭标记（协议常量）。`slice(34,-35)` 里的 34／35 就是二者长度：
/// Desktop 用它们裁掉标记取 JSON 正文，所以切片数字与标记绑在一起核验。
const ASYNC_OPEN_MARKER: &str = "<send_user_message_question_reply>";
const ASYNC_CLOSE_MARKER: &str = "</send_user_message_question_reply>";
/// `questionItemId` 合成的首元素（协议常量）。
const ASYNC_QUESTION_ID_TAG: &str = "request_user_input_async";

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
        structural: None,
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
        structural: None,
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
        structural: None,
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
        structural: None,
        request_rule: "conversationId+requestId+response",
        response_rule: "ok",
        semantic_rule: "blocking user-input v1",
    },
    OperationRule {
        operation: CodexOperation::AsyncAnswerIdle,
        method: "thread-follower-start-turn",
        base_version: 2,
        // 2026-10-28：Desktop 26.1023.51421 只把压缩器局部名改了（`uxe/dxe/r.P` →
        // `_Se/vSe/r.L`），旧规则把这些名字当契约而失配。这里只留协议常量；标记绑定、
        // zod 访问器、守卫接收者、handler 形参由 [`async_answer_envelope_structure`]
        // 按引用关系认。
        requires: &[
            "<send_user_message_question_reply>",
            "</send_user_message_question_reply>",
            "JSON.stringify([\"request_user_input_async\",",
            "questionItemId:",
            ".min(1)",
            "slice(34,-35)",
            "case\"thread-follower-start-turn\":",
        ],
        structural: Some(async_answer_envelope_structure),
        request_rule: "conversationId+turnStart{text=envelope[questionItemId,question,answer]}",
        response_rule: "result.result",
        semantic_rule: "questionItemId=JSON.stringify([\"request_user_input_async\",item,index])（形参按序;第二元素=map源(<Q>.questions??[])同接收者的原条目.id;第三元素=map index）;answers=min(1)数组∪恒等transform;三字段同一字符串构造（字符串专有组合子证人）;守卫if(!…||!…)失败即返回+try{let <局部>=直接解析（同接收者）",
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
        structural: None,
        request_rule: "conversationId+mode+expectedTurnId",
        response_rule: "interruptedTurnId+ok",
        semantic_rule: "interrupt v4 要求 expectedTurnId（不降级 v3）",
    },
];

/// 归一化：去空白、模板反引号统一成双引号；**相邻两个标识符字符之间的空白保留一个
/// 空格**（`var _Se=` / `return null` 的词法边界不能被抹掉，否则向前读绑定名会把
/// 关键字黏进来）。
///
/// 让匹配对打包器的压缩/美化都不敏感；结构化规则本身要求**形状**而非"关键词出现"。
fn normalize(js: &str) -> String {
    let mut out = String::with_capacity(js.len());
    let mut pending_space = false;
    for c in js.chars() {
        if c.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space {
            if is_ident_char(c) && out.chars().next_back().is_some_and(is_ident_char) {
                out.push(' ');
            }
            pending_space = false;
        }
        out.push(if c == '`' { '"' } else { c });
    }
    out
}

// ---------- 有限词法：标识符与引用关系（只服务结构核验，不是 JS 解析器） ----------

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$'
}

/// 从 `at` 起读一个标识符，返回 `(标识符, 结束偏移)`。
fn ident_at(hay: &str, at: usize) -> Option<(&str, usize)> {
    let mut chars = hay[at..].char_indices();
    let (_, first) = chars.next()?;
    if !is_ident_start(first) {
        return None;
    }
    let mut end = at + first.len_utf8();
    for (offset, c) in chars {
        if !is_ident_char(c) {
            break;
        }
        end = at + offset + c.len_utf8();
    }
    Some((&hay[at..end], end))
}

/// 从 `at` 起读一条成员链 `a.b.c`，返回 `(链文本, 结束偏移)`。
fn member_chain_at(hay: &str, at: usize) -> Option<(&str, usize)> {
    let (_, mut end) = ident_at(hay, at)?;
    while hay[end..].starts_with('.') {
        match ident_at(hay, end + 1) {
            Some((_, next)) => end = next,
            None => break,
        }
    }
    Some((&hay[at..end], end))
}

/// 从 `at` 起读 `a.b.c()`（成员链 + 空参调用），返回 `(调用文本, 结束偏移)`。
fn nullary_call_at(hay: &str, at: usize) -> Option<(&str, usize)> {
    let (_, end) = member_chain_at(hay, at)?;
    hay[end..]
        .starts_with("()")
        .then(|| (&hay[at..end + 2], end + 2))
}

/// 向前读 `end` 之前的标识符（`end` 处是标识符后的第一个字符），返回其起始偏移。
/// 起始处的词法边界由"再往前不是标识符字符"保证。
fn ident_before(hay: &str, end: usize) -> Option<usize> {
    let mut start = end;
    for (offset, c) in hay[..end].char_indices().rev() {
        if !is_ident_char(c) {
            break;
        }
        start = offset;
    }
    (start < end && hay[start..].chars().next().is_some_and(is_ident_start)).then_some(start)
}

/// 向前读 `end` 之前的成员链 `a.b.c`，返回其起始偏移。
fn member_chain_before(hay: &str, end: usize) -> Option<usize> {
    let mut start = end;
    for (offset, c) in hay[..end].char_indices().rev() {
        if !(is_ident_char(c) || c == '.') {
            break;
        }
        start = offset;
    }
    (start < end).then_some(start)
}

/// 从 `from` 起最多 `len` 字节的窗口（退到字符边界）。
fn window(hay: &str, from: usize, len: usize) -> &str {
    let mut end = (from + len).min(hay.len());
    while !hay.is_char_boundary(end) {
        end -= 1;
    }
    &hay[from..end]
}

/// 所有形如 `IDENT=<literal>` 的绑定名（`literal` 已含引号；IDENT 前必须是词法边界）。
fn bindings_of_literal<'a>(hay: &'a str, literal: &str) -> Vec<&'a str> {
    let needle = format!("={literal}");
    let mut out: Vec<&str> = hay
        .match_indices(&needle)
        .filter_map(|(eq, _)| ident_before(hay, eq).map(|start| &hay[start..eq]))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// zod 字符串 schema **专有**的组合子（数组／数字／对象 schema 上不存在）。用来证明三字段
/// 共用的构造链确实是字符串构造，而不写死它的压缩名：同一条链在包内别处被这些组合子
/// 修饰（`r.L().trim(`），改名时链与证人一起变。
const STRING_ONLY_COMBINATORS: &[&str] = &[
    "trim",
    "regex",
    "url",
    "email",
    "uuid",
    "startsWith",
    "endsWith",
    "includes",
    "datetime",
    "toLowerCase",
    "toUpperCase",
];

/// 守卫与解析必须是**同一条引用链**，且只接受已观测的分支形态：
/// `if(!R.startsWith(O)||!R.endsWith(C))return null;`——负向检查失败即返回（两版观测
/// 包都只有这一形态；不保留肯定形分支）。解析必须紧跟已观测的直接语法
/// `;try{let <局部名>=E.safeParse(JSON.parse(R.slice(34,-35)))`：守卫到解析之间**只
/// 允许这一段**（局部名可一致改名），不出现任何其他语句——嵌套/箭头函数、改道返回、
/// 未调用包装全部无处容身。返回所有这样的信封 schema 名 `E`。
fn guarded_parse_schemas<'a>(hay: &'a str, opens: &[&str], closes: &[&str]) -> Vec<&'a str> {
    let slice = format!(
        ".slice({},-{})))",
        ASYNC_OPEN_MARKER.len(),
        ASYNC_CLOSE_MARKER.len()
    );
    let mut out = Vec::new();
    for open in opens {
        let starts = format!(".startsWith({open})");
        for (dot, _) in hay.match_indices(&starts) {
            let Some(r_start) = ident_before(hay, dot) else {
                continue;
            };
            // 已观测形态：守卫以 `if(!` 开头——负向检查，失败即返回。
            if !hay[..r_start].ends_with("if(!") {
                continue;
            }
            let receiver = &hay[r_start..dot];
            let after = dot + starts.len();
            let Some(guard_end) = closes.iter().find_map(|close| {
                let tail = format!("||!{receiver}.endsWith({close}))return null;");
                hay[after..]
                    .starts_with(&tail)
                    .then_some(after + tail.len())
            }) else {
                continue;
            };
            let scope = window(hay, guard_end, 256);
            let parse = format!(".safeParse(JSON.parse({receiver}{slice}");
            for (pos, _) in scope.match_indices(&parse) {
                let Some(e_start) = ident_before(scope, pos) else {
                    continue;
                };
                if observed_parse_gap(&scope[..e_start]) {
                    out.push(&scope[e_start..pos]);
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// 守卫结束到解析之间必须是已观测的直接解析语句开头 `try{let <局部名>=`（守卫尾的
/// `return null;` 已随守卫一并钉死）——局部名可一致改名；其它任何间隔
/// （`let dead=r=>…`、`function dead(…)`、额外 `return`、直接裸调用等）都是未观测
/// 形态，拒绝。
fn observed_parse_gap(gap: &str) -> bool {
    let Some(rest) = gap.strip_prefix("try{let ") else {
        return false;
    };
    let Some((_, end)) = ident_at(rest, 0) else {
        return false;
    };
    rest[end..].starts_with('=') && rest[end + 1..].is_empty()
}

/// 条目 schema：`I=<callee>({questionItemId:T,question:T,answer:T})`，三字段的空参
/// 类型构造 `T` 必须**完全相同**。返回所有 `(I, T)`；`T` 是否为字符串构造由
/// [`string_schema_witness`] 另证。
fn item_schema_bindings(hay: &str) -> Vec<(&str, &str)> {
    const HEAD: &str = "{questionItemId:";
    let mut out = Vec::new();
    for (brace, _) in hay.match_indices(HEAD) {
        let Some((ty, after_first)) = nullary_call_at(hay, brace + HEAD.len()) else {
            continue;
        };
        let tail = format!(",question:{ty},answer:{ty}}}");
        if !hay[after_first..].starts_with(&tail) {
            continue;
        }
        // `(` ← callee 链 ← `=` ← 绑定名
        if !hay[..brace].ends_with('(') {
            continue;
        }
        let Some(callee) = member_chain_before(hay, brace - 1) else {
            continue;
        };
        if !hay[..callee].ends_with('=') {
            continue;
        }
        if let Some(start) = ident_before(hay, callee - 1) {
            out.push((&hay[start..callee - 1], ty));
        }
    }
    out
}

/// 字符串类型证人：构造链 `T`（如 `r.L()`）在包内别处紧跟字符串专有组合子（`r.L().trim(`）。
/// 没有证人 → 字段类型不可证 → 不匹配（不猜）。
fn string_schema_witness(hay: &str, ty: &str) -> bool {
    STRING_ONLY_COMBINATORS.iter().any(|combinator| {
        let needle = format!("{ty}.{combinator}(");
        hay.match_indices(&needle)
            .any(|(idx, _)| idx == 0 || !hay[..idx].chars().next_back().is_some_and(is_ident_char))
    })
}

/// 信封 schema：`E=<callee>([<callee>(I).min(1),I.transform(p=>[p])])`——数组至少一条，
/// 或单条经**恒等包装**成唯一元素数组（回调 `p=>[p]`／`(p)=>[p]`，且是数组最后一个成员）。
/// 返回所有 `E`。
fn envelope_schema_bindings<'a>(hay: &'a str, item: &str) -> Vec<&'a str> {
    let needle = format!("({item}).min(1),{item}.transform(");
    let mut out = Vec::new();
    for (paren, _) in hay.match_indices(&needle) {
        if !identity_wrap_at(hay, paren + needle.len()) {
            continue;
        }
        // `(` ← 内层 callee 链 ← `[` ← `(` ← 外层 callee 链 ← `=` ← 绑定名
        let Some(inner) = member_chain_before(hay, paren) else {
            continue;
        };
        if !hay[..inner].ends_with("([") {
            continue;
        }
        let Some(outer) = member_chain_before(hay, inner - 2) else {
            continue;
        };
        if !hay[..outer].ends_with('=') {
            continue;
        }
        if let Some(start) = ident_before(hay, outer - 1) {
            out.push(&hay[start..outer - 1]);
        }
    }
    out
}

/// `p=>[p])])` 或 `(p)=>[p])])`：transform 回调把入参包成唯一元素数组，并结束信封数组。
fn identity_wrap_at(hay: &str, at: usize) -> bool {
    let callback = if hay[at..].starts_with('(') {
        ident_at(hay, at + 1)
            .filter(|(_, end)| hay[*end..].starts_with(')'))
            .map(|(param, end)| (param, end + 1))
    } else {
        ident_at(hay, at)
    };
    callback.is_some_and(|(param, end)| hay[end..].starts_with(&format!("=>[{param}])])")))
}

/// 题目标识合成的**参数关系**（不止看 tag 与两个变量共现）：
/// * 定义：`function F(P1,P2){return JSON.stringify(["request_user_input_async",P1,P2])`——
///   数组第二／三元素必须按序就是两个形参，且 `P1≠P2`；
/// * 调用：`.map((X,N)=>F(ARG,N))`——第三元素来自 map 回调的 index 形参 `N`，第二元素来自
///   条目侧表达式（≠N、不派生于 N）。
fn question_id_synthesis_bound(hay: &str) -> bool {
    const RET: &str = "{return ";
    let needle = format!("JSON.stringify([\"{ASYNC_QUESTION_ID_TAG}\",");
    hay.match_indices(&needle).any(|(idx, _)| {
        let Some((p1, e1)) = ident_at(hay, idx + needle.len()) else {
            return false;
        };
        if !hay[e1..].starts_with(',') {
            return false;
        }
        let Some((p2, e2)) = ident_at(hay, e1 + 1) else {
            return false;
        };
        if p1 == p2 || !hay[e2..].starts_with("])") {
            return false;
        }
        if !hay[..idx].ends_with(RET) {
            return false;
        }
        let ret = idx - RET.len();
        let params = format!("({p1},{p2})");
        if !hay[..ret].ends_with(&params) {
            return false;
        }
        let params_at = ret - params.len();
        let Some(f_start) = ident_before(hay, params_at) else {
            return false;
        };
        if !hay[..f_start].ends_with("function ") {
            return false;
        }
        callsite_passes_index(hay, &hay[f_start..params_at])
    })
}

/// `.map((X,N)=>F(ARG,N))`：第二实参正是 map 回调的 index 形参 `N`；第一实参必须是
/// **本 `.map` 的源集合所属条目**的 `.id`——只支持已观测的 map 源形态
/// `(<Q>.questions??[]).map(...)`，`Q` 与 `.id` 的接收者做**完整成员链比较**（形参名
/// 无关）。`ge.questions` 含 `e.questions` 子串不算；窗口内别处出现的 `.questions`
/// 也不算来源。`.type`／`.turnId`、map 子项自己的 `.id`、无关对象的 `.id` 一律拒绝。
fn callsite_passes_index(hay: &str, f: &str) -> bool {
    const HEAD: &str = ".map((";
    hay.match_indices(HEAD).any(|(idx, _)| {
        let Some((_, e_x)) = ident_at(hay, idx + HEAD.len()) else {
            return false;
        };
        if !hay[e_x..].starts_with(',') {
            return false;
        }
        let Some((index, e_n)) = ident_at(hay, e_x + 1) else {
            return false;
        };
        let call = format!(")=>{f}(");
        if !hay[e_n..].starts_with(&call) {
            return false;
        }
        let Some((arg, e_arg)) = member_chain_at(hay, e_n + call.len()) else {
            return false;
        };
        if arg == index || arg.starts_with(&format!("{index}.")) {
            return false;
        }
        if !hay[e_arg..].starts_with(&format!(",{index}))")) {
            return false;
        }
        // 第一实参 = 原条目 `.id`，且 map 源就是 `(<该条目>.questions??[])`（完整相等）。
        let Some(item) = arg.strip_suffix(".id") else {
            return false;
        };
        if item.is_empty() || !item.chars().next().is_some_and(is_ident_start) {
            return false;
        }
        window_before(hay, idx, 64).ends_with(&format!("({item}.questions??[])"))
    })
}

/// `end` 之前最多 `len` 字节的窗口（退到字符边界）。
fn window_before(hay: &str, end: usize, len: usize) -> &str {
    let mut start = end.saturating_sub(len);
    while !hay.is_char_boundary(start) {
        start -= 1;
    }
    &hay[start..end]
}

/// 承载 handler：`startTurn(<p>.params.conversationId,<p>.params.turnStart)`，形参名无关。
fn start_turn_carrier_present(hay: &str) -> bool {
    const HEAD: &str = "startTurn(";
    hay.match_indices(HEAD).any(|(idx, _)| {
        ident_at(hay, idx + HEAD.len()).is_some_and(|(param, end)| {
            hay[end..].starts_with(&format!(".params.conversationId,{param}.params.turnStart)"))
        })
    })
}

/// 异步答题信封的结构核验（标识符可任意一致改名；结构或语义一变即拒绝）：
///
/// 1. 开/闭标记各绑定到某个标识符；只接受已观测的守卫分支 `if(!R.startsWith(O)||
///    !R.endsWith(C))return null;`，其后只允许已观测的直接解析语句
///    `try{let <局部名>=E.safeParse(JSON.parse(R.slice(34,-35)))`（同接收者；无任何
///    其他间隔——嵌套/箭头函数、改道返回无处容身）；
/// 2. 条目 schema 三字段同一空参构造，且该构造链有字符串专有组合子证人；
/// 3. 信封 schema 由该条目 schema 组成（`min(1)` 数组 ∪ 恒等 transform `p=>[p]`），
///    并且正是第 1 条解析链用的 schema；
/// 4. `questionItemId` 由生成函数两个形参按序合成，调用点第二元素是 map 源
///    `(<Q>.questions??[])` 同接收者原条目的 `.id`、第三元素是 map index；
/// 5. 承载 handler 仍是 `startTurn(conversationId, turnStart)`。
///
/// 证据覆盖范围：以上全是有限词法上的引用关系，不是 JS 求值。数组／联合构造只核"接受
/// 条目 schema 且 `.min(1)`"与两分支外形，同形的 tuple 等角色无法词法区分；字段顺序、
/// 守卫写法（非观测形态）、生成函数改箭头或内联、调用点不走 `.map((x,i)=>…)` 都会被
/// 拒绝而非放行，需复探后再登记。字符串证人是"有限规则下的佐证"，不承诺完整符号解析。
fn async_answer_envelope_structure(hay: &str) -> bool {
    let open = format!("\"{ASYNC_OPEN_MARKER}\"");
    let close = format!("\"{ASYNC_CLOSE_MARKER}\"");
    let opens = bindings_of_literal(hay, &open);
    let closes = bindings_of_literal(hay, &close);
    let parsers = guarded_parse_schemas(hay, &opens, &closes);
    if parsers.is_empty() {
        return false;
    }
    let envelope_ok = item_schema_bindings(hay)
        .into_iter()
        .filter(|(_, ty)| string_schema_witness(hay, ty))
        .any(|(item, _)| {
            envelope_schema_bindings(hay, item)
                .iter()
                .any(|envelope| parsers.contains(envelope))
        });
    envelope_ok && question_id_synthesis_bound(hay) && start_turn_carrier_present(hay)
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
        if rule.structural.is_some_and(|check| !check(&hay)) {
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

    /// 26.1021 包已不可得：题目标识调用点与字符串 schema 证人按 26.1023 观测形态合成
    /// （见 `synthetic/20261026-macos-completions/README.md`），只用于旧夹具规则回归。
    const SYNTHETIC_CALLSITE_20261026: &str = include_str!(
        "../../../../tests/fixtures/codex-desktop-ipc/synthetic/20261026-macos-completions/question-id-callsite.synthetic.js"
    );
    const SYNTHETIC_STRING_WITNESS_20261026: &str = include_str!(
        "../../../../tests/fixtures/codex-desktop-ipc/synthetic/20261026-macos-completions/string-schema-witness.synthetic.js"
    );

    fn main_fixture_js() -> String {
        format!(
            "{START_TURN_HANDLER}\n{DECISION_HANDLERS}\n{INTERRUPT_HANDLER}\n{ASYNC_ENVELOPE}\n{SYNTHETIC_CALLSITE_20261026}\n{SYNTHETIC_STRING_WITNESS_20261026}\n{START_TURN_REQUEST}\n"
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

    // ===== 2026-10-28 异步答题结构识别（重命名容忍）=====
    //
    // 现场：Desktop 26.1023.51421 只把压缩器局部名改了（uxe/dxe/r.P → _Se/vSe/r.L），
    // 协议常量、字段名、题目标识合成、min(1)、slice(34,-35)、守卫引用关系全部未变，
    // 旧规则却因为把局部名当契约而失配。下列用例只走 `read` 黑盒：一致重命名必须仍
    // 匹配，真正的结构/语义漂移必须拒绝，且每个拒绝用例都用 CommandApproval 作对照
    // （证明包被正常读取，只是本项被拒）。

    const VERSION_TABLE_20261028: &str = include_str!(
        "../../../../tests/fixtures/codex-desktop-ipc/observed/20261028-macos/contract-fragments/version-table.js"
    );
    const START_TURN_HANDLER_20261028: &str = include_str!(
        "../../../../tests/fixtures/codex-desktop-ipc/observed/20261028-macos/contract-fragments/start-turn-handler.js"
    );
    const DECISION_HANDLERS_20261028: &str = include_str!(
        "../../../../tests/fixtures/codex-desktop-ipc/observed/20261028-macos/contract-fragments/decision-and-user-input-handlers.js"
    );
    const INTERRUPT_HANDLER_20261028: &str = include_str!(
        "../../../../tests/fixtures/codex-desktop-ipc/observed/20261028-macos/contract-fragments/interrupt-handler.js"
    );
    const ASYNC_ENVELOPE_20261028: &str = include_str!(
        "../../../../tests/fixtures/codex-desktop-ipc/observed/20261028-macos/contract-fragments/async-envelope.js"
    );
    const START_TURN_REQUEST_20261028: &str = include_str!(
        "../../../../tests/fixtures/codex-desktop-ipc/observed/20261028-macos/contract-fragments/start-turn-request.js"
    );
    /// 题目标识生成函数的调用点：`map((t,n)=>bSe(e.id,n))`——第三元素来自 index 形参。
    const QUESTION_ID_CALLSITE_20261028: &str = include_str!(
        "../../../../tests/fixtures/codex-desktop-ipc/observed/20261028-macos/contract-fragments/question-id-callsite.js"
    );
    /// 字符串 schema 证人：三字段共用的构造 `r.L()` 在包内别处被字符串专有组合子修饰。
    const STRING_WITNESS_20261028: &str = include_str!(
        "../../../../tests/fixtures/codex-desktop-ipc/observed/20261028-macos/contract-fragments/string-schema-witness.js"
    );

    /// 20261028 主进程片段拼接（可替换 start-turn handler 与异步信封做单点突变；
    /// 调用点与证人取观测原文）。
    fn main_js_20261028(handler: &str, envelope: &str) -> String {
        main_js_20261028_with(
            handler,
            envelope,
            QUESTION_ID_CALLSITE_20261028,
            STRING_WITNESS_20261028,
        )
    }

    fn main_js_20261028_with(
        handler: &str,
        envelope: &str,
        callsite: &str,
        witness: &str,
    ) -> String {
        format!(
            "{handler}\n{DECISION_HANDLERS_20261028}\n{INTERRUPT_HANDLER_20261028}\n{envelope}\n{callsite}\n{witness}\n{START_TURN_REQUEST_20261028}\n"
        )
    }

    fn evidence_20261028(
        dir: &Path,
        name: &str,
        main_js: &str,
        table_js: &str,
    ) -> DesktopContractEvidence {
        let package = dir.join(name);
        write_asar(
            &package,
            &[
                ("/.vite/build/main-fixture.js", main_js),
                ("/.vite/build/src-fixture.js", table_js),
            ],
        );
        read(&package, "26.1023.51421").expect("20261028 evidence")
    }

    /// 单点突变：锚点必须命中，否则用例是没改到东西的假阳性。
    fn mutate(source: &str, from: &str, to: &str) -> String {
        assert!(source.contains(from), "mutation anchor missing: {from}");
        source.replace(from, to)
    }

    fn assert_async_rejected_but_package_read(evidence: &DesktopContractEvidence, what: &str) {
        assert!(
            !evidence.matched(CodexOperation::AsyncAnswerIdle),
            "{what}: 真漂移必须拒绝 AsyncAnswerIdle"
        );
        assert!(
            evidence.matched(CodexOperation::CommandApproval),
            "{what}: 包应被正常读取（对照项 CommandApproval 仍匹配）"
        );
    }

    /// 当前安装包（26.1023.51421）的片段必须识别出全部六项，异步答题仍走 start-turn v2。
    #[test]
    fn async_contract_current_build_26_1023_51421_matches() {
        let dir = tempfile::tempdir().unwrap();
        let evidence = evidence_20261028(
            dir.path(),
            "current.asar",
            &main_js_20261028(START_TURN_HANDLER_20261028, ASYNC_ENVELOPE_20261028),
            VERSION_TABLE_20261028,
        );
        for op in [
            CodexOperation::Send,
            CodexOperation::CommandApproval,
            CodexOperation::FileChangeApproval,
            CodexOperation::BlockingInput,
            CodexOperation::AsyncAnswerIdle,
            CodexOperation::Interrupt,
        ] {
            assert!(evidence.matched(op), "26.1023.51421 missing {op:?}");
        }
        assert_eq!(
            evidence.matches[&CodexOperation::AsyncAnswerIdle].base_version,
            2
        );
    }

    /// 压缩器把所有局部名再改一轮（含 zod 访问器、守卫接收者、handler 形参、生成函数名、
    /// 证人里的同一构造链）→ 仍匹配。不断言 Send：它的字面规则不在本次范围，形参改名后可能失配。
    #[test]
    fn async_contract_consistent_identifier_renames_still_match() {
        let renames: &[(&str, &str)] = &[
            ("_Se", "openTag"),
            ("vSe", "closeTag"),
            (",ey=", ",itemSchema="),
            ("(ey)", "(itemSchema)"),
            ("ey.transform", "itemSchema.transform"),
            (",ySe=", ",envelopeSchema="),
            ("ySe.safeParse", "envelopeSchema.safeParse"),
            ("r.M(", "zod.object("),
            ("r.L()", "zod.str()"),
            ("r.B(", "zod.union("),
            ("r.v(", "zod.array("),
            ("bSe", "makeQuestionItemId"),
            ("xSe", "parseReplyEnvelope"),
            ("r=(n===-1", "body=(n===-1"),
            ("!r.startsWith", "!body.startsWith"),
            ("!r.endsWith", "!body.endsWith"),
            ("JSON.parse(r.slice", "JSON.parse(body.slice"),
        ];
        let mut envelope = ASYNC_ENVELOPE_20261028.to_string();
        for (from, to) in renames {
            envelope = mutate(&envelope, from, to);
        }
        assert_ne!(envelope, ASYNC_ENVELOPE_20261028);
        // 调用点与证人跟着同一轮改名（生成函数名、对象/字符串构造链、map 形参）。
        let callsite = mutate(
            &mutate(QUESTION_ID_CALLSITE_20261028, "bSe", "makeQuestionItemId"),
            "(t,n)=>",
            "(item,idx)=>",
        )
        .replace("e.id,n)", "e.id,idx)");
        assert!(callsite.contains("makeQuestionItemId(e.id,idx)"));
        let witness = mutate(
            &mutate(STRING_WITNESS_20261028, "r.M(", "zod.object("),
            "r.L()",
            "zod.str()",
        );
        let handler = mutate(
            &mutate(START_TURN_HANDLER_20261028, "t.params.", "req.params."),
            "t.method",
            "req.method",
        );
        let dir = tempfile::tempdir().unwrap();
        let evidence = evidence_20261028(
            dir.path(),
            "renamed.asar",
            &main_js_20261028_with(&handler, &envelope, &callsite, &witness),
            VERSION_TABLE_20261028,
        );
        assert!(
            evidence.matched(CodexOperation::AsyncAnswerIdle),
            "一致重命名不得导致失配: {:?}",
            evidence.matches.keys().collect::<Vec<_>>()
        );
        assert!(evidence.matched(CodexOperation::CommandApproval));
    }

    #[test]
    fn async_contract_rejects_field_type_drift() {
        let dir = tempfile::tempdir().unwrap();
        let envelope = mutate(ASYNC_ENVELOPE_20261028, "answer:r.L()", "answer:r.N()");
        let evidence = evidence_20261028(
            dir.path(),
            "a.asar",
            &main_js_20261028(START_TURN_HANDLER_20261028, &envelope),
            VERSION_TABLE_20261028,
        );
        assert_async_rejected_but_package_read(&evidence, "字段类型不一致");
    }

    #[test]
    fn async_contract_rejects_field_name_drift() {
        let dir = tempfile::tempdir().unwrap();
        let envelope = mutate(ASYNC_ENVELOPE_20261028, "answer:r.L()", "reply:r.L()");
        let evidence = evidence_20261028(
            dir.path(),
            "b.asar",
            &main_js_20261028(START_TURN_HANDLER_20261028, &envelope),
            VERSION_TABLE_20261028,
        );
        assert_async_rejected_but_package_read(&evidence, "字段名改变");
    }

    #[test]
    fn async_contract_rejects_envelope_marker_drift() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            ASYNC_ENVELOPE_20261028
                .matches("send_user_message_question_reply")
                .count(),
            2
        );
        let envelope = ASYNC_ENVELOPE_20261028.replace(
            "send_user_message_question_reply",
            "send_user_message_question_reply_v2",
        );
        let evidence = evidence_20261028(
            dir.path(),
            "c.asar",
            &main_js_20261028(START_TURN_HANDLER_20261028, &envelope),
            VERSION_TABLE_20261028,
        );
        assert_async_rejected_but_package_read(&evidence, "信封标记改变");
    }

    #[test]
    fn async_contract_rejects_start_turn_version_drift() {
        let dir = tempfile::tempdir().unwrap();
        let table = mutate(
            VERSION_TABLE_20261028,
            "\"thread-follower-start-turn\":2",
            "\"thread-follower-start-turn\":3",
        );
        let evidence = evidence_20261028(
            dir.path(),
            "d.asar",
            &main_js_20261028(START_TURN_HANDLER_20261028, ASYNC_ENVELOPE_20261028),
            &table,
        );
        assert_async_rejected_but_package_read(&evidence, "start-turn 方法版本改变");
        assert!(!evidence.matched(CodexOperation::Send));
    }

    #[test]
    fn async_contract_rejects_min_count_drift() {
        let dir = tempfile::tempdir().unwrap();
        let envelope = mutate(ASYNC_ENVELOPE_20261028, ".min(1)", ".min(0)");
        let evidence = evidence_20261028(
            dir.path(),
            "e.asar",
            &main_js_20261028(START_TURN_HANDLER_20261028, &envelope),
            VERSION_TABLE_20261028,
        );
        assert_async_rejected_but_package_read(&evidence, "min(1) 改变");
    }

    #[test]
    fn async_contract_rejects_question_id_tag_drift() {
        let dir = tempfile::tempdir().unwrap();
        let envelope = mutate(
            ASYNC_ENVELOPE_20261028,
            "request_user_input_async",
            "request_user_input_async_v2",
        );
        let evidence = evidence_20261028(
            dir.path(),
            "f.asar",
            &main_js_20261028(START_TURN_HANDLER_20261028, &envelope),
            VERSION_TABLE_20261028,
        );
        assert_async_rejected_but_package_read(&evidence, "题目标识前缀改变");
    }

    /// 守卫引用的不是绑定了开标记的变量（引用关系断裂）→ 拒绝，即使 `startsWith(` 仍在。
    #[test]
    fn async_contract_rejects_unbound_guard_reference() {
        let dir = tempfile::tempdir().unwrap();
        let envelope = mutate(
            ASYNC_ENVELOPE_20261028,
            "startsWith(_Se)",
            "startsWith(zSe)",
        );
        let evidence = evidence_20261028(
            dir.path(),
            "g.asar",
            &main_js_20261028(START_TURN_HANDLER_20261028, &envelope),
            VERSION_TABLE_20261028,
        );
        assert_async_rejected_but_package_read(&evidence, "守卫引用未绑定标记");
    }

    #[test]
    fn async_contract_rejects_slice_drift() {
        let dir = tempfile::tempdir().unwrap();
        let envelope = mutate(ASYNC_ENVELOPE_20261028, "slice(34,-35)", "slice(35,-36)");
        let evidence = evidence_20261028(
            dir.path(),
            "h.asar",
            &main_js_20261028(START_TURN_HANDLER_20261028, &envelope),
            VERSION_TABLE_20261028,
        );
        assert_async_rejected_but_package_read(&evidence, "载荷切片改变");
    }

    #[test]
    fn async_contract_rejects_carrier_param_drift() {
        let dir = tempfile::tempdir().unwrap();
        let handler = mutate(
            START_TURN_HANDLER_20261028,
            "t.params.turnStart",
            "t.params.input",
        );
        let evidence = evidence_20261028(
            dir.path(),
            "i.asar",
            &main_js_20261028(&handler, ASYNC_ENVELOPE_20261028),
            VERSION_TABLE_20261028,
        );
        assert_async_rejected_but_package_read(&evidence, "start-turn 承载参数改变");
    }

    /// 解析用的 schema 不是绑定了 `min(1)`/`transform` 的信封 schema → 拒绝。
    #[test]
    fn async_contract_rejects_envelope_schema_decoupled() {
        let dir = tempfile::tempdir().unwrap();
        let envelope = mutate(ASYNC_ENVELOPE_20261028, "ySe.safeParse", "qSe.safeParse");
        let evidence = evidence_20261028(
            dir.path(),
            "j.asar",
            &main_js_20261028(START_TURN_HANDLER_20261028, &envelope),
            VERSION_TABLE_20261028,
        );
        assert_async_rejected_but_package_read(&evidence, "信封 schema 与解析脱钩");
    }

    // ===== 2026-10-28 复审收口（S01 守卫/解析同链 · S02 题目标识参数关系 · S03 schema 语义）=====

    fn envelope_evidence(dir: &Path, name: &str, envelope: &str) -> DesktopContractEvidence {
        evidence_20261028(
            dir,
            name,
            &main_js_20261028(START_TURN_HANDLER_20261028, envelope),
            VERSION_TABLE_20261028,
        )
    }

    /// S01：守卫前半检查 `r`，后半却检查 `other` → 被验证的不是同一输入，拒绝。
    #[test]
    fn async_contract_rejects_guard_receiver_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let envelope = mutate(
            ASYNC_ENVELOPE_20261028,
            "!r.endsWith(vSe)",
            "!other.endsWith(vSe)",
        );
        let evidence = envelope_evidence(dir.path(), "s01a.asar", &envelope);
        assert_async_rejected_but_package_read(&evidence, "守卫两半接收者不同");
    }

    /// S01：守卫检查 `r`，解析却裁切 `other` → 被解析的不是被验证的内容，拒绝。
    #[test]
    fn async_contract_rejects_parse_input_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let envelope = mutate(
            ASYNC_ENVELOPE_20261028,
            "JSON.parse(r.slice(34,-35))",
            "JSON.parse(other.slice(34,-35))",
        );
        let evidence = envelope_evidence(dir.path(), "s01b.asar", &envelope);
        assert_async_rejected_but_package_read(&evidence, "解析输入与守卫输入不同");
    }

    /// S01：守卫在一个函数里结束，解析搬进另一个同名形参的函数 → 不是同一作用域，拒绝。
    #[test]
    fn async_contract_rejects_guard_and_parse_in_different_functions() {
        let dir = tempfile::tempdir().unwrap();
        let envelope = mutate(
            ASYNC_ENVELOPE_20261028,
            "return null;try{let e=ySe.safeParse(JSON.parse(r.slice(34,-35)));return e.success?e.data:null}catch{return null}}",
            "return null}function qSe(r){try{let e=ySe.safeParse(JSON.parse(r.slice(34,-35)));return e.success?e.data:null}catch{return null}}",
        );
        let evidence = envelope_evidence(dir.path(), "s01c.asar", &envelope);
        assert_async_rejected_but_package_read(&evidence, "守卫与解析跨函数拼接");
    }

    /// S02：生成函数 `bSe(e,t)` 的数组第二/三元素交换 → questionItemId 不再是 [tag,item,index]，拒绝。
    #[test]
    fn async_contract_rejects_question_id_swapped_params() {
        let dir = tempfile::tempdir().unwrap();
        let envelope = mutate(
            ASYNC_ENVELOPE_20261028,
            "`request_user_input_async`,e,t]",
            "`request_user_input_async`,t,e]",
        );
        let evidence = envelope_evidence(dir.path(), "s02a.asar", &envelope);
        assert_async_rejected_but_package_read(&evidence, "题目标识参数交换");
    }

    /// S02：同一形参重复出现 → index 丢失，拒绝。
    #[test]
    fn async_contract_rejects_question_id_duplicated_param() {
        let dir = tempfile::tempdir().unwrap();
        let envelope = mutate(
            ASYNC_ENVELOPE_20261028,
            "`request_user_input_async`,e,t]",
            "`request_user_input_async`,e,e]",
        );
        let evidence = envelope_evidence(dir.path(), "s02b.asar", &envelope);
        assert_async_rejected_but_package_read(&evidence, "题目标识参数重复");
    }

    /// S02：数组元素换成非形参变量 → 拒绝。
    #[test]
    fn async_contract_rejects_question_id_foreign_variable() {
        let dir = tempfile::tempdir().unwrap();
        let envelope = mutate(
            ASYNC_ENVELOPE_20261028,
            "`request_user_input_async`,e,t]",
            "`request_user_input_async`,e,x]",
        );
        let evidence = envelope_evidence(dir.path(), "s02c.asar", &envelope);
        assert_async_rejected_but_package_read(&evidence, "题目标识元素来自外来变量");
    }

    /// S02：调用点把 item 与 index 调换（`bSe(n,e.id)`）→ 第三元素不再是 map index，拒绝。
    #[test]
    fn async_contract_rejects_question_id_callsite_swapped() {
        let dir = tempfile::tempdir().unwrap();
        let callsite = mutate(QUESTION_ID_CALLSITE_20261028, "bSe(e.id,n)", "bSe(n,e.id)");
        let evidence = evidence_20261028(
            dir.path(),
            "s02d.asar",
            &main_js_20261028_with(
                START_TURN_HANDLER_20261028,
                ASYNC_ENVELOPE_20261028,
                &callsite,
                STRING_WITNESS_20261028,
            ),
            VERSION_TABLE_20261028,
        );
        assert_async_rejected_but_package_read(&evidence, "调用点 item/index 调换");
    }

    /// S02：没有任何调用点把 index 形参传到第三位 → 无法确认第三元素来源，拒绝。
    #[test]
    fn async_contract_rejects_missing_question_id_callsite() {
        let dir = tempfile::tempdir().unwrap();
        let evidence = evidence_20261028(
            dir.path(),
            "s02e.asar",
            &main_js_20261028_with(
                START_TURN_HANDLER_20261028,
                ASYNC_ENVELOPE_20261028,
                "",
                STRING_WITNESS_20261028,
            ),
            VERSION_TABLE_20261028,
        );
        assert_async_rejected_but_package_read(&evidence, "缺少题目标识调用点");
    }

    /// S03：三字段**统一**换成非字符串构造（彼此仍一致）→ 没有字符串证人，拒绝。
    #[test]
    fn async_contract_rejects_uniform_non_string_fields() {
        let dir = tempfile::tempdir().unwrap();
        let envelope = mutate(
            ASYNC_ENVELOPE_20261028,
            "questionItemId:r.L(),question:r.L(),answer:r.L()",
            "questionItemId:z.number(),question:z.number(),answer:z.number()",
        );
        let evidence = envelope_evidence(dir.path(), "s03a.asar", &envelope);
        assert_async_rejected_but_package_read(&evidence, "三字段统一改为非字符串构造");
    }

    /// S03：包内没有该构造链的字符串专有组合子用法（证人缺失）→ 字段类型不可证，拒绝。
    #[test]
    fn async_contract_rejects_missing_string_witness() {
        let dir = tempfile::tempdir().unwrap();
        let evidence = evidence_20261028(
            dir.path(),
            "s03e.asar",
            &main_js_20261028_with(
                START_TURN_HANDLER_20261028,
                ASYNC_ENVELOPE_20261028,
                QUESTION_ID_CALLSITE_20261028,
                "",
            ),
            VERSION_TABLE_20261028,
        );
        assert_async_rejected_but_package_read(&evidence, "缺少字符串 schema 证人");
    }

    /// S03：单条 transform 不再把原条目包成唯一元素数组（丢弃 / 不包 / 包别的变量）→ 拒绝。
    #[test]
    fn async_contract_rejects_transform_not_wrapping_item() {
        for (idx, (to, what)) in [
            ("ey.transform(e=>[])", "transform 丢弃条目"),
            ("ey.transform(e=>e)", "transform 不包数组"),
            ("ey.transform(e=>[t])", "transform 包了别的变量"),
        ]
        .into_iter()
        .enumerate()
        {
            let dir = tempfile::tempdir().unwrap();
            let envelope = mutate(ASYNC_ENVELOPE_20261028, "ey.transform(e=>[e])", to);
            let evidence = envelope_evidence(dir.path(), &format!("s03t{idx}.asar"), &envelope);
            assert_async_rejected_but_package_read(&evidence, what);
        }
    }

    // ===== 2026-10-29 复审残留（S01 守卫分支作用/内层函数 · S02 itemId 来源未绑定）=====

    /// S01：肯定形守卫 `if(R.startsWith(O)&&R.endsWith(C))return null;` ——合法信封被
    /// 直接拒绝。已观测的两版包都只有"负向检查失败即返回"形态，肯定形必须拒绝。
    #[test]
    fn async_contract_rejects_positive_guard_form() {
        let dir = tempfile::tempdir().unwrap();
        let envelope = mutate(
            ASYNC_ENVELOPE_20261028,
            "if(!r.startsWith(_Se)||!r.endsWith(vSe))return null;",
            "if(r.startsWith(_Se)&&r.endsWith(vSe))return null;",
        );
        let evidence = envelope_evidence(dir.path(), "r01a.asar", &envelope);
        assert_async_rejected_but_package_read(&evidence, "肯定形守卫（合法信封直接返回失败）");
    }

    /// S01：真正的解析藏进守卫后声明、从不调用的内层函数，外层直接返回失败 → 拒绝。
    #[test]
    fn async_contract_rejects_dead_inner_function_parse() {
        let dir = tempfile::tempdir().unwrap();
        let envelope = mutate(
            ASYNC_ENVELOPE_20261028,
            ";try{let e=ySe.safeParse(JSON.parse(r.slice(34,-35)));return e.success?e.data:null}catch{return null}}",
            ";function dead(r){try{let e=ySe.safeParse(JSON.parse(r.slice(34,-35)));return e.success?e.data:null}catch{return null}}return null}}",
        );
        let evidence = envelope_evidence(dir.path(), "r01b.asar", &envelope);
        assert_async_rejected_but_package_read(&evidence, "解析藏在未调用的内层函数");
    }

    /// S02：第一实参换成原条目的 `.type` / `.turnId` → itemId 不再来自原条目 ID，拒绝。
    #[test]
    fn async_contract_rejects_question_id_source_not_id() {
        for (idx, (to, what)) in [
            ("bSe(e.type,n)", "第一实参是原条目 .type"),
            ("bSe(e.turnId,n)", "第一实参是原条目 .turnId"),
        ]
        .into_iter()
        .enumerate()
        {
            let dir = tempfile::tempdir().unwrap();
            let callsite = mutate(QUESTION_ID_CALLSITE_20261028, "bSe(e.id,n)", to);
            let evidence = evidence_20261028(
                dir.path(),
                &format!("r02a{idx}.asar"),
                &main_js_20261028_with(
                    START_TURN_HANDLER_20261028,
                    ASYNC_ENVELOPE_20261028,
                    &callsite,
                    STRING_WITNESS_20261028,
                ),
                VERSION_TABLE_20261028,
            );
            assert_async_rejected_but_package_read(&evidence, what);
        }
    }

    /// S02：第一实参换成 map 子项（题目自身）的 `.id` 或无关对象的 `.id` → 拒绝
    ///（`.id` 形态成立，但接收者不是提供 `questions` 集合的原条目）。
    #[test]
    fn async_contract_rejects_question_id_source_foreign_id() {
        for (idx, (to, what)) in [
            ("bSe(t.id,n)", "第一实参是 map 子项自己的 .id"),
            ("bSe(g.id,n)", "第一实参是无关对象的 .id"),
        ]
        .into_iter()
        .enumerate()
        {
            let dir = tempfile::tempdir().unwrap();
            let callsite = mutate(QUESTION_ID_CALLSITE_20261028, "bSe(e.id,n)", to);
            let evidence = evidence_20261028(
                dir.path(),
                &format!("r02b{idx}.asar"),
                &main_js_20261028_with(
                    START_TURN_HANDLER_20261028,
                    ASYNC_ENVELOPE_20261028,
                    &callsite,
                    STRING_WITNESS_20261028,
                ),
                VERSION_TABLE_20261028,
            );
            assert_async_rejected_but_package_read(&evidence, what);
        }
    }

    // ===== 2026-10-29 合并审查残留（S01 表达式箭头 · S02 map 源接收者子串误判）=====

    /// S01：解析藏进未调用的表达式箭头 `let dead=r=>schema.safeParse(...)`，入口直接
    /// 返回失败 → 拒绝（守卫到解析之间只允许已观测的直接解析语法）。
    #[test]
    fn async_contract_rejects_expression_arrow_parse() {
        let dir = tempfile::tempdir().unwrap();
        let envelope = mutate(
            ASYNC_ENVELOPE_20261028,
            ";try{let e=ySe.safeParse(JSON.parse(r.slice(34,-35)));return e.success?e.data:null}catch{return null}}",
            ";let dead=r=>ySe.safeParse(JSON.parse(r.slice(34,-35)));return null}}",
        );
        let evidence = envelope_evidence(dir.path(), "r03a.asar", &envelope);
        assert_async_rejected_but_package_read(&evidence, "解析藏在未调用的表达式箭头内");
    }

    /// S02：map 源接收者与 `.id` 接收者不是同一条成员链 → 拒绝：
    /// ① `(ge.questions??[]).map((t,n)=>bSe(e.id,n))`——`ge.questions` 含 `e.questions`
    ///    子串，但遍历的是 ge 的题、用的是 e 的 ID；
    /// ② 窗口内先出现 `e.questions`、实际 map 源是别的集合——附近出现不等于来源。
    #[test]
    fn async_contract_rejects_questions_source_receiver_mismatch() {
        let variants = [
            (
                "(e.questions??[]).map(",
                "(ge.questions??[]).map(",
                "map 源接收者 ge 与 .id 接收者 e 不同",
            ),
            (
                "(e.questions??[]).map(",
                "(e.questions??[]).length>0&&(g.items??[]).map(",
                "窗口内出现过 e.questions 但实际 map 源是别的集合",
            ),
        ];
        for (idx, (from, to, what)) in variants.into_iter().enumerate() {
            let dir = tempfile::tempdir().unwrap();
            let callsite = mutate(QUESTION_ID_CALLSITE_20261028, from, to);
            let evidence = evidence_20261028(
                dir.path(),
                &format!("r03b{idx}.asar"),
                &main_js_20261028_with(
                    START_TURN_HANDLER_20261028,
                    ASYNC_ENVELOPE_20261028,
                    &callsite,
                    STRING_WITNESS_20261028,
                ),
                VERSION_TABLE_20261028,
            );
            assert_async_rejected_but_package_read(&evidence, what);
        }
    }
}
