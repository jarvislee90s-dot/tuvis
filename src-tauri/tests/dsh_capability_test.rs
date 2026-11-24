//! T08：逐操作契约识别、证据与未知 build 兼容（contract + capability）行为测试。
//!
//! 输入层次（如实申报，仿夹具 README 纪律）：
//! * **规则锚点**：逐条锚定本机官方 DSH `0.2.1-rc.3` 安装包的生成描述符
//!   （`app.asar` 内 `@deepseek-ai/dsh-api-session-controller/lib/typert.host.js`、
//!   `dsh-user-questions/lib/typert.host.js`、`dsh-api-workspace-controller`、
//!   `dsh-commands`、`dsh-client-file-upload/lib/types/{protocol,http-route}.js`、
//!   `dsh-user-approval/lib/types/index.js`、`dsh-api-remotes/lib/types/remote-events.js`、
//!   `dsh-api-gateway/lib/types/stream-protocol.js`），2026-10-30 由本任务实读核验。
//! * **本文件的包夹具**：**synthetic**——上述真实描述符行/片段的最小拼装，用于驱动
//!   匹配器；不是官方新版实测，不得作为「该版本已实测」引用。
//! * 夹具按**方法分块**组织（`'ns/method': { ... }`），并在兄弟方法块里保留诱饵
//!   片段（`accepted:true`、`archivedSessionIds`、`pinnedSessionIds`、`hasMore`），
//!   使「整文件拼接匹配」的假阳性可被 B-1 用例捕获。
//! * `package_sha256`：真实包级 SHA-256 的人工登记值仍属夹具 README「未测项」，
//!   本文件的包指纹由 `contract::read_report_with` 对**合成 asar 字节**实算
//!   （synthetic 占位性质，但"同内容同值 / 改字节必变"是真实哈希行为）。
//!
//! 全部文件访问经注入式 [`ContractFileSource`]，不触真实 app 路径；不连接任何
//! DSH 实例、不发写帧（验收⑥）。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use multi_agents_manager_lib::services::dsh_control::capability::{
    classify_native_failure, evaluate, reason, AnchoredOperations, FailureClass, OperationContext,
};
use multi_agents_manager_lib::services::dsh_control::contract::{
    package_path, read_report_with, read_with, rule_audit, ContractBudget, ContractFileReader,
    ContractFileSource, ContractReadReport, CONTRACT_RULE_VERSION,
};
use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::model::{
    ContractEvidence, EffectiveOperations, EvidenceSource, LocalHost, Operation,
};

// ============================================================
// 注入假体
// ============================================================

/// 本机宿主（Mac 形态；asar 路径由 contract 自 executable 推导）。
fn host() -> LocalHost {
    LocalHost {
        host_id: "local".into(),
        platform: "macos".into(),
        profile_dir: PathBuf::from("/Users/probe/.dsh/profiles/desktop"),
        executable: PathBuf::from(
            "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness",
        ),
        pid: 4242,
        started_at_ms: 1_700_000_000_000,
        epoch: 7,
        authority: "127.0.0.1:5214".into(),
        // identity 的**描述符级**指纹（与内容指纹不同层）；契约证据以整包内容为准。
        package_sha256: "d37a0417fbe3cad1b115337839071a494ef857157e0b2b91777d2217886bd333".into(),
    }
}

/// 假文件源：内存 asar 字节 + 调用记录（验收⑥：只允许只读调用）。
struct FakePackage {
    bytes: Vec<u8>,
    /// 每次读的 (offset, len)。
    reads: Mutex<Vec<(u64, u64)>>,
    /// 在指定 offset 处注入一次 IO 失败（P2-3）。
    fail_at: Option<u64>,
    /// 打开次数（P2-2：生产路径只应打开一次）。
    opens: Mutex<usize>,
}

impl FakePackage {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            reads: Mutex::new(Vec::new()),
            fail_at: None,
            opens: Mutex::new(0),
        }
    }

    fn failing(bytes: Vec<u8>, fail_at: u64) -> Self {
        Self {
            fail_at: Some(fail_at),
            ..Self::new(bytes)
        }
    }

    fn reads(&self) -> Vec<(u64, u64)> {
        self.reads.lock().unwrap().clone()
    }

    fn opens(&self) -> usize {
        *self.opens.lock().unwrap()
    }

    fn bytes_read(&self) -> u64 {
        self.reads().iter().map(|(_, len)| *len).sum()
    }
}

/// 假句柄：借用父结构的内存字节与调用记录。
struct FakeReader<'a> {
    package: &'a FakePackage,
}

impl ContractFileReader for FakeReader<'_> {
    fn len(&self) -> u64 {
        self.package.bytes.len() as u64
    }

    fn read_range(&mut self, offset: u64, len: u64) -> Result<Vec<u8>, DshError> {
        self.package.reads.lock().unwrap().push((offset, len));
        if self.package.fail_at == Some(offset) {
            return Err(DshError::new(codes::STATE_IO, "injected read failure"));
        }
        let start = usize::try_from(offset)
            .map_err(|_| DshError::new(codes::STATE_IO, "offset overflow"))?;
        let end = start
            .checked_add(usize::try_from(len).unwrap_or(usize::MAX))
            .filter(|end| *end <= self.package.bytes.len())
            .ok_or_else(|| DshError::new(codes::STATE_IO, "range beyond package"))?;
        Ok(self.package.bytes[start..end].to_vec())
    }
}

impl ContractFileSource for FakePackage {
    fn open(&self, path: &Path) -> Result<Box<dyn ContractFileReader + '_>, DshError> {
        if !path.to_string_lossy().ends_with("app.asar") {
            return Err(DshError::new(
                codes::STATE_IO,
                format!("unexpected package path: {path:?}"),
            ));
        }
        *self.opens.lock().unwrap() += 1;
        Ok(Box::new(FakeReader { package: self }))
    }
}

// ============================================================
// 合成 asar（synthetic；asar 目录格式真实）
// ============================================================

/// 内存 asar：16 字节前缀 + JSON 目录 + 顺序数据区。
fn asar(files: &[(&str, &str)]) -> Vec<u8> {
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
    out.extend_from_slice(&payload);
    out
}

fn insert_entry(root: &mut serde_json::Value, name: &str, size: u64, offset: u64) {
    let parts: Vec<&str> = name.trim_start_matches('/').split('/').collect();
    let mut node = root;
    for (idx, part) in parts.iter().enumerate() {
        let files = node
            .get_mut("files")
            .and_then(|v| v.as_object_mut())
            .expect("files object");
        if idx + 1 == parts.len() {
            files.insert(
                (*part).to_string(),
                serde_json::json!({"size": size, "offset": offset.to_string()}),
            );
            return;
        }
        node = files
            .entry((*part).to_string())
            .or_insert_with(|| serde_json::json!({"files": {}}));
    }
}

// ============================================================
// 合成包内容（synthetic；按方法分块 + 兄弟方法诱饵）
// ============================================================
//
// 每块 = 本机 0.2.1-rc.3 生成描述符里**真实行/片段**的最小拼装（真实结构词、
// 合成取值）。匹配器归一化（去空白、反引号→双引号）后按块内形状串比对。
//
// 诱饵（B-1 的检测面）：`session/cancel` 与 `session/prompt` 同含 accepted:true；
// `session/unarchiveSession` 与 archive 同含 archivedSessionIds；`unpinSession` 与
// `pinSession` 同含 pinnedSessionIds；`session/list` 与 `session/search` 同含 hasMore。

const SESSION_JS: &str = r#"
const $remote = createTypertRemoteInfo({
  methods: {
    'session/list': {
      service: 'sessionController',
      parameters: [{ name: '_request', source: 'json' }],
      result: { jsonSchema: {"type":"object","properties":{"items":{"type":"array","items":{}},"hasMore":{"type":"boolean"}}} },
    },
    'session/page': {
      result: { jsonSchema: {"type":"object","properties":{"items":{"type":"array","items":{}}}} },
    },
    'session/follow': {
      mode: 'stream',
      result: { jsonSchema: {"oneOf":[{"properties":{"type":{"type":"string","const":"snapshot"}}}]} },
      projections: { imageLimits: {"maxImageBytes":{"type":"number"}} },
    },
    'session/prompt': {
      parameters: [{ name: 'request', mode: 'strict', jsonSchema: {"type":"object","properties":{"requestId":{"type":"string"},"sessionId":{"type":"string"},"mode":{"type":"string","enum":["queue","steer"]},"content":{"type":"array","items":{"oneOf":[{"properties":{"type":{"type":"string","const":"text"},"text":{"type":"string"}},"required":["type","text"]},{"properties":{"type":{"type":"string","const":"image"},"mediaType":{"type":"string","enum":["image/png","image/jpeg","image/webp","image/gif"]},"data":{"type":"string"}}},{"properties":{"type":{"type":"string","const":"file"},"receiptId":{"type":"string"}}}]}}},"required":["requestId","sessionId","mode","content"]}}],
      result: {"type":"object","properties":{"accepted":{"type":"boolean","const":true}},"required":["accepted"]},
    },
    'session/modelCatalog': {
      result: { typeSymbol: '@deepseek-ai/dsh-api-session-controller/types#ModelCatalog' },
      jsonSchema: {"type":"object","properties":{"groups":{"items":{"properties":{"reasoning":{"properties":{"efforts":{"type":"array","items":{}},"defaultEffort":{"type":"string"},"reasoningEffort":{"type":"string"}}}}}}}},
    },
    'session/selectModel': {
      description: 'Select one Session-local model after explicitly resuming the Session; save the default in the background.',
      jsonSchema: {"type":"object","properties":{"sessionId":{"type":"string"},"provider":{"type":"string"},"model":{"type":"string"},"reasoningEffort":{"type":"string"}},"required":["sessionId","provider","model"],"additionalProperties":true},
      strictCodec: {"type":"object","properties":{"sessionId":{"type":"string"},"provider":{"type":"string"},"model":{"type":"string"},"reasoningEffort":{"type":"string"}},"required":["sessionId","provider","model"],"additionalProperties":false},
      result: {"type":"object","properties":{"selected":{"type":"object","properties":{"provider":{"type":"string"},"model":{"type":"string"},"reasoningEffort":{"type":"string"}}}}},
    },
    'session/cancel': {
      description: 'Cancel one active Agent turn without dropping its pending inbox.',
      jsonSchema: {"type":"object","properties":{"sessionId":{"type":"string"}},"required":["sessionId"]},
      result: {"type":"object","properties":{"accepted":{"type":"boolean","const":true}}},
    },
    'session/updateQueue': {
      description: 'Mutate one still-pending queue occurrence, resuming a cold Agent first.',
      jsonSchema: {"type":"object","properties":{"sessionId":{"type":"string"},"itemId":{"type":"string"}},"action":{"oneOf":[{"type":"object","properties":{"kind":{"type":"string","const":"edit"}},"required":["kind"]},{"type":"object","properties":{"kind":{"type":"string","const":"remove"}},"required":["kind"]},{"type":"object","properties":{"kind":{"type":"string","const":"steer"}},"required":["kind"]}]}},
    },
    'session/create': {
      result: { description: 'the created or reused Session identity and its preset when configured.', jsonSchema: {"type":"object","properties":{"sessionId":{"type":"string"},"agentPreset":{"type":"string","description":"Preset the Session runs, when the deployment composes a preset roster."}}} },
      jsonSchema: {"type":"object","properties":{"workspaceId":{"type":"string"},"cwd":{"type":"string"},"sessionId":{"type":"string"},"agentPreset":{"type":"string"}}},
    },
    'session/rename': {
      result: { description: 'the accepted title and durable event sequence.', jsonSchema: {"type":"object","properties":{"title":{"type":"string"},"seq":{"type":"number"}}} },
      jsonSchema: {"type":"object","properties":{"sessionId":{"type":"string"},"title":{"type":"string"}},"required":["sessionId","title"],"additionalProperties":true},
    },
    'session/fork': {
      description: 'Fork one cold-readable exact event prefix into a new Session.',
      jsonSchema: {"type":"object","properties":{"sessionId":{"type":"string"},"atSeq":{"type":"number"},"allowMigration":{"type":"boolean"}}},
    },
    'session/search': {
      description: 'Search visible Session content without resuming an Agent; hasMore describes the bounded result before that filtering.',
      jsonSchema: {"type":"object","properties":{"query":{"type":"string"}}},
      result: {"type":"object","properties":{"hasMore":{"type":"boolean"}}},
    },
    'skills/list': {
      jsonSchema: {"type":"object","properties":{"skills":{"type":"array","items":{"properties":{"modelInvocable":{"type":"boolean"}}}}}},
    },
  },
});
"#;

const QUESTIONS_JS: &str = r#"
const $remote = createTypertRemoteInfo({
  methods: {
    'userQuestions/answer': {
      description: 'Complete structured answer batch, one item per question of the call.',
      parameters: [
        { name: 'agent' },
        { name: 'callId' },
        { name: 'answer', jsonSchema: {"type":"object","properties":{"answers":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string","description":"The answered question id."},"selected":{"type":"array","items":{"type":"string"},"description":"Selected option labels. May accompany custom text for a multi-select question."},"custom":{"type":"string","description":"Optional free-text \"Other\" answer."}},"required":["id","selected"]}},"required":["answers"]}} },
      ],
      result: {"type":"boolean"},
    },
  },
});
const schemas = [
  {"name":"AskUserQuestionIntent","declaration":"export type AskUserQuestionIntent = { kind: 'plan-review'; approve: string; callId?: ToolCallId; };"},
  {"name":"AskUserQuestionItem","declaration":"export interface AskUserQuestionItem {\n    id: string;\n    question: string;\n    header?: string;\n    multiSelect?: boolean;\n    intent?: AskUserQuestionIntent;\n}"},
  {"name":"AskUserQuestionOption","declaration":"export interface AskUserQuestionOption {\n    label: string;\n}"},
];
"#;

const WORKSPACE_JS: &str = r#"
const $remote = createTypertRemoteInfo({
  methods: {
    'workspace/archiveSession': { description: 'Hide one known Session from Workspace grouping surfaces.', result: {"type":"object","properties":{"archivedSessionIds":{"type":"array","items":{"type":"string"}}}} },
    'workspace/unarchiveSession': { description: 'Restore one archived Session to Workspace grouping surfaces.', result: {"type":"object","properties":{"archivedSessionIds":{"type":"array","items":{"type":"string"}}}} },
    'workspace/pinSession': { description: 'Surface one known unarchived Session ahead of unpinned Sessions.', result: {"type":"object","properties":{"pinnedSessionIds":{"type":"array","items":{"type":"string"}}}} },
    'workspace/unpinSession': { description: "Remove one Session pin without changing its saved order.", result: {"type":"object","properties":{"pinnedSessionIds":{"type":"array","items":{"type":"string"}}}} },
  },
});
"#;

const COMMANDS_JS: &str = r#"
const $remote = createTypertRemoteInfo({
  methods: {
    'commands/execute': {
      scope: { context: 'agent', wire: 'agentId' },
      parameters: [
        { name: 'line', description: 'complete slash-command line.' },
        { name: 'submittedAttachments', jsonSchema: {"type":"array","items":{"oneOf":[{"properties":{"type":{"type":"string","const":"file"},"receiptId":{"type":"string"}}}]}} },
      ],
    },
    'commands/list': {
      scope: { context: 'agent', wire: 'agentId' },
      result: { description: 'name-sorted descriptors after scoped shadowing.', jsonSchema: {"type":"array","items":{"type":"object","properties":{"definitionId":{"type":"string"},"name":{"type":"string"}}}} },
    },
  },
});
"#;

const UPLOAD_PROTOCOL_JS: &str =
    "export const FILE_UPLOAD_PATH = '/api/session/uploadFileBinary';\n";

const UPLOAD_ROUTE_JS: &str = r#"
export async function handleFileUploadHttp(service, request) {
  const mediaType = request.headers.get('content-type')?.split(';', 1)[0]?.trim().toLowerCase();
  if (mediaType !== 'application/octet-stream') return new Response(null, { status: 415 });
  const value = await service.uploadStream({ sessionId: 'session-synthetic', data: [] });
  return value;
}
"#;

const APPROVAL_TYPES_JS: &str =
    "const OUTCOMES = ['allowed-once', 'rejected', 'cancelled', 'unavailable'];\n";

const REMOTE_EVENTS_JS: &str = r#"
export const API_REMOTE_FORWARDED_EVENTS = [
  { event: 'approval/request', mode: 'waterfall' },
  { event: 'user-questions/request', mode: 'waterfall' },
];
"#;

const STREAM_PROTOCOL_JS: &str = r#"
if (!isRecord(value) || !exactKeys(value, ['clientId', 'eventId', 'outcome']) || !isRecord(value.outcome)) {
  throw new Error('api gateway: invalid Remote event result');
}
const result = {
  outcome: Object.hasOwn(outcome, 'value') ? { kind: 'result', value: outcome.value } : { kind: 'result' },
};
"#;

const SESSION_PATH: &str =
    "/dsh/node_modules/@deepseek-ai/dsh-api-session-controller/lib/typert.host.js";
const QUESTIONS_PATH: &str = "/dsh/node_modules/@deepseek-ai/dsh-user-questions/lib/typert.host.js";
const WORKSPACE_PATH: &str =
    "/dsh/node_modules/@deepseek-ai/dsh-api-workspace-controller/lib/typert.host.js";
const COMMANDS_PATH: &str = "/dsh/node_modules/@deepseek-ai/dsh-commands/lib/typert.host.js";
const UPLOAD_PROTOCOL_PATH: &str =
    "/dsh/node_modules/@deepseek-ai/dsh-client-file-upload/lib/types/protocol.js";
const UPLOAD_ROUTE_PATH: &str =
    "/dsh/node_modules/@deepseek-ai/dsh-client-file-upload/lib/types/http-route.js";
const APPROVAL_TYPES_PATH: &str =
    "/dsh/node_modules/@deepseek-ai/dsh-user-approval/lib/types/index.js";
const REMOTE_EVENTS_PATH: &str =
    "/dsh/node_modules/@deepseek-ai/dsh-api-remotes/lib/types/remote-events.js";
const STREAM_PROTOCOL_PATH: &str =
    "/dsh/node_modules/@deepseek-ai/dsh-api-gateway/lib/types/stream-protocol.js";

/// 全部候选文件（synthetic 包）。
fn full_files() -> Vec<(&'static str, &'static str)> {
    vec![
        (SESSION_PATH, SESSION_JS),
        (QUESTIONS_PATH, QUESTIONS_JS),
        (WORKSPACE_PATH, WORKSPACE_JS),
        (COMMANDS_PATH, COMMANDS_JS),
        (UPLOAD_PROTOCOL_PATH, UPLOAD_PROTOCOL_JS),
        (UPLOAD_ROUTE_PATH, UPLOAD_ROUTE_JS),
        (APPROVAL_TYPES_PATH, APPROVAL_TYPES_JS),
        (REMOTE_EVENTS_PATH, REMOTE_EVENTS_JS),
        (STREAM_PROTOCOL_PATH, STREAM_PROTOCOL_JS),
    ]
}

/// 替换某个来源文件内容（其余保持原样）。
fn with_replaced(
    source_path: &str,
    replacement: &'static str,
) -> Vec<(&'static str, &'static str)> {
    full_files()
        .into_iter()
        .map(|(path, content)| {
            if path == source_path {
                (path, replacement)
            } else {
                (path, content)
            }
        })
        .collect()
}

fn full_package() -> Vec<u8> {
    asar(&full_files())
}

/// 多选形状漂移包（`multiSelect?: boolean;` → `multiSelect?: number;`）。
fn multi_answer_drift_package() -> Vec<u8> {
    let drifted: &'static str = Box::leak(
        QUESTIONS_JS
            .replace("multiSelect?: boolean;", "multiSelect?: number;")
            .into_boxed_str(),
    );
    asar(&with_replaced(QUESTIONS_PATH, drifted))
}

/// 只含只读方法契约的包（读成功不得解锁任何写操作）。
fn read_only_package() -> Vec<u8> {
    const READ_ONLY_JS: &str = r#"
const $remote = createTypertRemoteInfo({
  methods: {
    'session/list': {
      parameters: [{ name: '_request', source: 'json' }],
      result: { jsonSchema: {"type":"object","properties":{"items":{"type":"array","items":{}}}} },
    },
    'session/follow': {
      mode: 'stream',
      result: { jsonSchema: {"oneOf":[{"properties":{"type":{"type":"string","const":"snapshot"}}}]} },
    },
  },
});
"#;
    asar(&[(SESSION_PATH, READ_ONLY_JS)])
}

/// 只有方法名、没有结构/语义的包（方法名本身不得算匹配）。
fn keyword_only_package() -> Vec<u8> {
    const KEYWORDS: &str = r#"
const names = ['session/list','session/prompt','session/selectModel','session/modelCatalog',
  'session/cancel','session/updateQueue','session/create','session/rename','session/fork',
  'session/search','userQuestions/answer','workspace/archiveSession','workspace/unarchiveSession',
  'workspace/pinSession','workspace/unpinSession','commands/list','commands/execute','skills/list'];
const more = ['approval/request','allowed-once','rejected','multiSelect','maxImageBytes'];
"#;
    asar(&[(SESSION_PATH, KEYWORDS)])
}

// ============================================================
// 期望值来源：生产入口返回
// ============================================================

fn read_ok(files: &[(&str, &str)]) -> ContractEvidence {
    read_with(
        &host(),
        CONTRACT_RULE_VERSION,
        &FakePackage::new(asar(files)),
    )
    .expect("synthetic package must be identifiable")
}

fn read_ok_package(bytes: Vec<u8>) -> ContractEvidence {
    read_with(&host(), CONTRACT_RULE_VERSION, &FakePackage::new(bytes))
        .expect("synthetic package must be identifiable")
}

/// 走**生产语义**的报告入口（P1-1：能力求值只允许经 `read_report_with`）。
fn report_ok(files: &[(&str, &str)]) -> ContractReadReport {
    read_report_with(
        &host(),
        CONTRACT_RULE_VERSION,
        &FakePackage::new(asar(files)),
        &ContractBudget::DEFAULT,
    )
    .expect("synthetic package must be identifiable")
}

fn all_implemented() -> BTreeSet<Operation> {
    Operation::ALL.into_iter().collect()
}

/// 从契约报告出发的完整事实上下文（`anchor_to` 是 P1-2 的正确路径）。
fn context_for(report: &ContractReadReport) -> OperationContext {
    OperationContext {
        target_fresh: true,
        auto_adapt_enabled: true,
        read_evidence: false,
        implemented: all_implemented(),
        current_package_sha256: String::new(),
        contract_rule_version: 0,
        shipped: None,
        observed: None,
        contract: None,
        contract_inconclusive: BTreeSet::new(),
        contract_error: None,
        drifted: BTreeSet::new(),
        auth_invalid: false,
    }
    .anchor_to(report)
    .expect("当前代报告必须可锚定")
}

fn context_from_evidence(evidence: ContractEvidence) -> OperationContext {
    context_for(&ContractReadReport {
        evidence,
        inconclusive: BTreeSet::new(),
        unresolved: BTreeSet::new(),
        budget_limited: false,
    })
}

fn status_reason(ops: &EffectiveOperations, operation: Operation) -> Option<String> {
    ops.operations
        .get(&operation)
        .and_then(|status| status.reason_code.clone())
}

// ============================================================
// 复审修复轮 2：方法块边界与片段归属（P1-A / P2-A / P2-D）
// ============================================================

/// 追加一段「TYPERT 尾区」副本（真实包在 `export const TYPERT` 后有同名语义副本）。
///
/// 这些副本属于**模型区**，不是方法 descriptor；块必须以配对 `}` 收口，不能吞进来。
fn with_tail_copies(
    files: Vec<(&'static str, &'static str)>,
    source: &str,
    tail: &'static str,
) -> Vec<(&'static str, &'static str)> {
    files
        .into_iter()
        .map(|(path, content)| {
            if path == source {
                (
                    path,
                    Box::leak(format!("{content}{tail}").into_boxed_str()) as &'static str,
                )
            } else {
                (path, content)
            }
        })
        .collect()
}

/// P1-A（commands/list）：只漂移 descriptor 内语义片段，尾部副本保持原样。
/// 块若吞入尾区（或结束于文件尾），尾副本会替 descriptor 命中 —— 必须判不匹配。
#[test]
fn commands_list_descriptor_drift_is_not_masked_by_tail_copy() {
    const TAIL: &str = r#"
export const TYPERT = {
  model: { commands: { description: 'name-sorted descriptors after scoped shadowing.' } },
};
"#;
    // descriptor 内漂移：语义句改名（尾部副本保留原名）。
    let drifted: &'static str = Box::leak(
        COMMANDS_JS
            .replacen(
                "name-sorted descriptors after scoped shadowing.",
                "descriptors ordered lexicographically by name.",
                1,
            )
            .into_boxed_str(),
    );
    let files = with_tail_copies(with_replaced(COMMANDS_PATH, drifted), COMMANDS_PATH, TAIL);
    let evidence = read_ok(&files);
    assert!(
        !evidence.matches.contains_key(&Operation::CommandsList),
        "descriptor 内语义漂移后，尾区同名副本不得替 CommandsList 命中"
    );
    assert!(
        evidence.matches.contains_key(&Operation::CommandExecute),
        "同文件另一方法不受影响"
    );
}

/// P1-A（skills/list）：尾块必须止于 descriptor 的配对 `}`，不得吞入后续模型区。
#[test]
fn skills_list_descriptor_drift_is_not_masked_by_tail() {
    const TAIL: &str = r#"
export const TYPERT = {
  model: { skills: { shape: {"skills":{"type":"array","items":{"properties":{"modelInvocable":{"type":"boolean"}}}}} } },
};
"#;
    // descriptor 内漂移：把结果里的 modelInvocable 形状改掉。
    let drifted: &'static str = Box::leak(
        SESSION_JS
            .replacen(
                r#""skills":{"type":"array","items":{"properties":{"modelInvocable":{"type":"boolean"}}}}"#,
                r#""skills":{"type":"array","items":{"properties":{"modelInvocable":{"type":"string"}}}}"#,
                1,
            )
            .into_boxed_str(),
    );
    let files = with_tail_copies(with_replaced(SESSION_PATH, drifted), SESSION_PATH, TAIL);
    let evidence = read_ok(&files);
    assert!(
        !evidence.matches.contains_key(&Operation::SkillsList),
        "skills/list descriptor 漂移后，尾区形状副本不得替它命中"
    );
    assert!(
        evidence.matches.contains_key(&Operation::Send),
        "会话域其它方法不受影响"
    );
}

/// P1-A（workspace/unpinSession）：文件内最后一个方法，块尾最容易被尾区吞掉。
#[test]
fn unpin_descriptor_drift_is_not_masked_by_tail() {
    const TAIL: &str = r#"
export const TYPERT = {
  model: { workspace: { pin: {"pinnedSessionIds":{"type":"array","items":{"type":"string"}}} } },
};
"#;
    let drifted: &'static str = Box::leak(
        WORKSPACE_JS
            .replacen(
                r#"'workspace/unpinSession': { description: "Remove one Session pin without changing its saved order.", result: {"type":"object","properties":{"pinnedSessionIds":{"type":"array","items":{"type":"string"}}}} }"#,
                r#"'workspace/unpinSession': { description: "Remove one Session pin without changing its saved order.", result: {"type":"object","properties":{"pinnedSessionIds":{"type":"string"}}} }"#,
                1,
            )
            .into_boxed_str(),
    );
    assert!(
        !drifted.contains("pinnedSessionIds\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}}} },\n    'workspace/unpinSession'")
            && drifted.contains("pinnedSessionIds\":{\"type\":\"string\"}}"),
        "前提：unpin 的 pinnedSessionIds 已从 array 漂移为 string"
    );
    let files = with_tail_copies(with_replaced(WORKSPACE_PATH, drifted), WORKSPACE_PATH, TAIL);
    let evidence = read_ok(&files);
    assert!(
        !evidence.matches.contains_key(&Operation::Unpin),
        "unpin descriptor 漂移后，尾区数组副本不得替它命中"
    );
    assert!(
        evidence.matches.contains_key(&Operation::Pin),
        "Pin 不受影响"
    );
}

/// P2-A：多窗口规则的片段必须**按窗口归属**——第一窗口漂移不能被第二窗口的同名
/// 片段掩盖（`selectModel` 内 3 处 reasoningEffort，`modelCatalog` 另有独立副本）。
#[test]
fn reasoning_select_fragment_must_belong_to_its_window() {
    // 只漂移 **selectModel** 窗口内的 reasoningEffort（modelCatalog 窗口保持原样作诱惑）。
    // 真实包 selectModel descriptor 内有 3 处同名片段，故按窗口边界整体替换。
    let select_start = SESSION_JS
        .find("'session/selectModel': {")
        .expect("fixture block");
    let select_end = SESSION_JS[select_start..]
        .find("'session/cancel': {")
        .map(|offset| select_start + offset)
        .expect("next block");
    let select_block = &SESSION_JS[select_start..select_end];
    let drifted_block = select_block.replace(
        r#""reasoningEffort":{"type":"string"}"#,
        r#""reasoningEffort":{"type":"number"}"#,
    );
    assert_eq!(
        select_block
            .matches(r#""reasoningEffort":{"type":"string"}"#)
            .count(),
        3,
        "前提：selectModel 内有 3 处该片段（与真实包同型）"
    );
    assert_eq!(
        drifted_block
            .matches(r#""reasoningEffort":{"type":"string"}"#)
            .count(),
        0,
        "前提：selectModel 窗口内已全部漂移"
    );
    let drifted_js = format!(
        "{}{}{}",
        &SESSION_JS[..select_start],
        drifted_block,
        &SESSION_JS[select_end..]
    );
    // modelCatalog 窗口的同名副本仍在（正是这条负例要排除的诱惑）。
    assert!(drifted_js
        .contains(r#""defaultEffort":{"type":"string"},"reasoningEffort":{"type":"string"}"#));
    let drifted: &'static str = Box::leak(drifted_js.into_boxed_str());
    let evidence = read_ok_package(asar(&with_replaced(SESSION_PATH, drifted)));
    assert!(
        !evidence.matches.contains_key(&Operation::ReasoningSelect),
        "selectModel 窗口漂移后，modelCatalog 窗口的同名片段不得替它命中"
    );
    // 依赖 modelCatalog 窗口的其它操作仍在（MultiAnswer 不依赖它；这里用 Read 对照）。
    assert!(evidence.matches.contains_key(&Operation::Read));
}

/// N-C：短读 reader（不按请求长度给字节）不得让 header 解析越界 panic，
/// 必须按畸形头返回 `DSH_FRAME_INVALID`。
#[test]
fn truncated_prefix_from_short_reader_is_rejected() {
    struct ShortReader(Vec<u8>);
    impl ContractFileSource for ShortReader {
        fn open(&self, _path: &Path) -> Result<Box<dyn ContractFileReader + '_>, DshError> {
            struct R<'a>(&'a [u8]);
            impl ContractFileReader for R<'_> {
                fn len(&self) -> u64 {
                    self.0.len() as u64
                }
                // 永远只给 8 字节（少于 16 字节前缀）
                fn read_range(&mut self, offset: u64, len: u64) -> Result<Vec<u8>, DshError> {
                    let start = offset as usize;
                    let end = (start + (len as usize)).min(self.0.len()).min(start + 8);
                    Ok(self.0[start..end].to_vec())
                }
            }
            Ok(Box::new(R(&self.0)))
        }
    }
    let mut bytes = full_package();
    bytes.extend_from_slice(&[0u8; 8]);
    let err = read_report_with(
        &host(),
        CONTRACT_RULE_VERSION,
        &ShortReader(bytes),
        &ContractBudget::DEFAULT,
    )
    .expect_err("短读必须按畸形头拒绝，而不是 panic");
    assert_eq!(err.code, codes::FRAME_INVALID);
    assert!(err.detail.contains("truncated"), "{}", err.detail);
}

/// P2-D：`hasMore` 必须是**结果形状**片段，散文里的 `'hasMore' describes …` 不算。
#[test]
fn session_search_result_drift_is_not_masked_by_prose() {
    const TAIL: &str = r#"
export const TYPERT = {
  model: { search: { doc: "search; 'hasMore' describes the bounded result." } },
};
"#;
    // 漂移结果区形状（description 里的散文保持原样 —— 这正是原先的假阳性来源）。
    let drifted: &'static str = Box::leak(
        SESSION_JS
            .replacen(
                r#"result: {"type":"object","properties":{"hasMore":{"type":"boolean"}}},"#,
                r#"result: {"type":"object","properties":{"hasMore":{"type":"string"}}},"#,
                1,
            )
            .into_boxed_str(),
    );
    let files = with_tail_copies(with_replaced(SESSION_PATH, drifted), SESSION_PATH, TAIL);
    let evidence = read_ok(&files);
    assert!(
        !evidence.matches.contains_key(&Operation::SessionSearch),
        "结果形状漂移后，描述里的 'hasMore' 散文与尾区副本都不得替 SessionSearch 命中"
    );
}

// ============================================================
// 复审修复轮 2：anchor 生命周期（Bl-1 / P2-B / P2-C）
// ============================================================

// C-3（T09 接线前）：异代报告在 anchor_to 门口被整体拒绝——该语义由
// stale_generation_report_cannot_be_anchored（更强：含 drifted 保留断言）承接；
// 求值侧代际比对仍是第二道锁，由 rule_generation_change_does_not_reuse_old_evidence
// 锁定。（轻量复审 Minor-4：旧用例 anchor_to_does_not_upgrade_stale_evidence 与
// 其逐行重复，已删除。）

/// P2-B：`anchor_to` 是"重识别成功"的推荐入口，必须清掉旧漂移结论。
#[test]
fn anchor_to_clears_drift() {
    let report = report_ok(&full_files());
    let mut ctx = context_for(&report);
    ctx.record_native_failure(Operation::Send, "gateway/arguments-invalid");
    assert!(!evaluate(&ctx).allowed(Operation::Send), "漂移后应被撤");

    let reanchored = ctx.anchor_to(&report).expect("当前代报告必须可锚定");
    assert!(reanchored.drifted.is_empty(), "重识别后必须清空漂移结论");
    assert!(
        evaluate(&reanchored).allowed(Operation::Send),
        "重识别并清漂移后应恢复（契约未变）"
    );
}

/// P2-B：认证失效的复位入口必须存在（否则只能重建上下文）。
#[test]
fn auth_invalid_has_explicit_reset() {
    let report = report_ok(&full_files());
    let mut ctx = context_for(&report);
    ctx.record_native_failure(Operation::Send, "unpaired");
    assert!(!evaluate(&ctx).allowed(Operation::Send));

    ctx.clear_auth_invalid();
    assert!(!ctx.auth_invalid);
    assert!(evaluate(&ctx).allowed(Operation::Send));
}

/// P2-C：预算受限报告（指纹为空）**保留**调用方已有的非空当前指纹，
/// 不得连带把 shipped/observed 证据撤掉。
#[test]
fn budget_limited_report_keeps_shipped_evidence() {
    let report = report_ok(&full_files());
    let content_sha = report.evidence.package_sha256.clone();
    assert!(!content_sha.is_empty());

    // 调用方先有 shipped 事实（锚定当前内容指纹）。
    let mut ctx = context_for(&report);
    ctx.shipped = Some(AnchoredOperations {
        package_sha256: content_sha.clone(),
        operations: [Operation::Send].into_iter().collect(),
    });

    // 随后一次预算受限的重识别：指纹为空、无匹配。
    let budget_limited = ContractReadReport {
        evidence: ContractEvidence {
            package_sha256: String::new(),
            rule_version: CONTRACT_RULE_VERSION,
            matches: BTreeMap::new(),
        },
        inconclusive: Operation::ALL.into_iter().collect(),
        unresolved: BTreeSet::new(),
        budget_limited: true,
    };
    // 复审轮 3 Major-1 追加：同一份预算受限报告也不得洗掉漂移结论。
    let mut drifted_ctx = context_for(&report);
    drifted_ctx.record_native_failure(Operation::MultiAnswer, "gateway/arguments-invalid");
    let drifted_ctx = drifted_ctx
        .anchor_to(&budget_limited)
        .expect("报告为当前代际");
    assert!(
        drifted_ctx.drifted.contains(&Operation::MultiAnswer),
        "预算受限报告未重识别任何操作，不得清除漂移结论（Major-1）"
    );

    let ctx = ctx.anchor_to(&budget_limited).expect("报告为当前代际");
    assert_eq!(
        ctx.current_package_sha256, content_sha,
        "预算受限不得清空已知的当前内容指纹"
    );
    let ops = evaluate(&ctx);
    assert!(
        ops.allowed(Operation::Send),
        "shipped 证据按许可保留，不得因预算受限误撤"
    );
    assert_eq!(
        ops.operations.get(&Operation::Send).unwrap().evidence,
        EvidenceSource::Shipped
    );
}

/// 复审修复轮 3 Major-1：预算受限报告**什么都没重识别**，不得把已漂移的操作
/// 复活（fail-open）。`anchor_to` 只在报告确实给出该操作的新识别结论时才解除
/// 其漂移；预算受限（matches 空）时一个都不清。
#[test]
fn budget_limited_anchor_does_not_revive_drifted_operations() {
    let report = report_ok(&full_files());
    let mut ctx = context_for(&report);
    ctx.record_native_failure(Operation::Send, "gateway/arguments-invalid");
    assert!(
        !evaluate(&ctx).allowed(Operation::Send),
        "前提：漂移已撤回 Send"
    );

    // 预算受限报告：目录头超限（指纹空、无任何匹配、全部 inconclusive）。
    let budget_limited = ContractReadReport {
        evidence: ContractEvidence {
            package_sha256: String::new(),
            rule_version: CONTRACT_RULE_VERSION,
            matches: BTreeMap::new(),
        },
        inconclusive: Operation::ALL.into_iter().collect(),
        unresolved: BTreeSet::new(),
        budget_limited: true,
    };
    let ctx = ctx.anchor_to(&budget_limited).expect("报告为当前代际");
    assert!(
        ctx.drifted.contains(&Operation::Send),
        "预算受限报告未重识别任何操作，不得清除漂移结论"
    );
    let ops = evaluate(&ctx);
    assert!(
        !ops.allowed(Operation::Send),
        "已漂移操作不得经预算受限的 anchor_to 复活"
    );
    assert_eq!(
        status_reason(&ops, Operation::Send).as_deref(),
        Some(reason::CONTRACT_DRIFT),
        "拒绝原因仍是漂移，而不是预算受限或证据过期"
    );
}

/// 复审修复轮 3 Major-1（部分受限）：候选数受限的报告只重识别了部分操作——
/// 只解除「实际重读且匹配」的操作的漂移；落在 inconclusive 的操作保留漂移。
#[test]
fn partial_budget_anchor_clears_only_reidentified_operations() {
    let report = report_ok(&full_files());
    let mut ctx = context_for(&report);
    ctx.record_native_failure(Operation::Send, "gateway/arguments-invalid");
    ctx.record_native_failure(Operation::MultiAnswer, "gateway/arguments-invalid");

    // 候选数上限 6：会话来源在预算内（Send 有新识别结论且命中），问答来源被
    // 截断（MultiAnswer 无新结论——见 budget_overrun_only_marks_affected_operations_inconclusive）。
    let capped = read_report_with(
        &host(),
        CONTRACT_RULE_VERSION,
        &FakePackage::new(full_package()),
        &ContractBudget {
            candidate_files: 6,
            ..ContractBudget::DEFAULT
        },
    )
    .expect("候选截断仍产出局部证据");
    assert!(capped.budget_limited, "前提：报告确实预算受限");
    assert!(capped.evidence.matches.contains_key(&Operation::Send));
    assert!(capped.inconclusive.contains(&Operation::MultiAnswer));

    let ctx = ctx.anchor_to(&capped).expect("报告为当前代际");
    assert!(
        !ctx.drifted.contains(&Operation::Send),
        "重新识别且命中的操作解除漂移"
    );
    assert!(
        ctx.drifted.contains(&Operation::MultiAnswer),
        "受限未产生新结论的操作保留漂移"
    );
}

// ============================================================
// 复审修复轮 3 Major-2：锚点不感知字符串/注释 → 伪锚点遮蔽真 descriptor
// ============================================================

/// Major-2：前置注释里内嵌一份**原样**的 session/prompt descriptor（全部 Send
/// 片段齐全），真 descriptor 漂移 `accepted` 后必须失配——注释伪锚点供片段、
/// 真块被忽略是 B-1 同级假阳性（锚点只在 `methods:` 对象体内取）。
#[test]
fn comment_pseudo_anchor_does_not_mask_real_descriptor() {
    let prompt_start = SESSION_JS
        .find("    'session/prompt': {")
        .expect("fixture prompt block");
    let prompt_end = SESSION_JS[prompt_start..]
        .find("    'session/modelCatalog': {")
        .map(|offset| prompt_start + offset)
        .expect("next block");
    let prompt_block = &SESSION_JS[prompt_start..prompt_end];
    // 前置块注释：内容 = 完整的原 prompt descriptor（伪锚点 + 供片段的完整上下文）。
    let pseudo: &'static str = Box::leak(
        format!("/* doc: generated descriptor copy:\n{prompt_block}*/\n").into_boxed_str(),
    );
    // 真 descriptor 漂移：accepted 形状改变（注释里的伪锚点保持原样）。
    let drifted: &'static str = Box::leak(
        SESSION_JS
            .replacen(
                r#""accepted":{"type":"boolean","const":true}},"required":["accepted"]}"#,
                r#""accepted":{"type":"string"}},"required":[]}"#,
                1,
            )
            .into_boxed_str(),
    );
    let combined: &'static str = Box::leak(format!("{pseudo}{drifted}").into_boxed_str());
    let evidence = read_ok_package(asar(&with_replaced(SESSION_PATH, combined)));
    assert!(
        !evidence.matches.contains_key(&Operation::Send),
        "注释伪锚点不得替漂移的真 descriptor 命中"
    );
    assert!(
        evidence.matches.contains_key(&Operation::CancelCurrent),
        "真块之外的方法不受影响"
    );
}

/// Major-2：methods 区内同键两块内容分歧 → 该来源多义（照 N-10 副本歧义纪律，
/// 整源不参与匹配），该操作进 unresolved——不 panic、不静默取首块。
#[test]
fn duplicate_method_blocks_with_divergent_content_are_unresolved() {
    // 在 methods 区内、真 prompt 块之后插入一个同键分歧副本（形状不同）。
    let duplicate = r#"    'session/prompt': {
      jsonSchema: {"type":"object","properties":{"mode":{"type":"string","enum":["queue"]}}},
    },
"#;
    let anchor = "    'session/modelCatalog': {";
    let at = SESSION_JS.find(anchor).expect("fixture modelCatalog block");
    let combined: &'static str = Box::leak(
        format!("{}{}{}", &SESSION_JS[..at], duplicate, &SESSION_JS[at..]).into_boxed_str(),
    );
    let report = read_report_with(
        &host(),
        CONTRACT_RULE_VERSION,
        &FakePackage::new(asar(&with_replaced(SESSION_PATH, combined))),
        &ContractBudget::DEFAULT,
    )
    .expect("多义方法块按 unresolved 上报，不 panic");
    assert!(
        !report.evidence.matches.contains_key(&Operation::Send),
        "同键分歧副本存在时不得静默取首块命中"
    );
    assert!(
        report.unresolved.contains(&Operation::Send),
        "该操作必须进 unresolved"
    );
    assert!(!report.budget_limited, "多义不是预算受限");
    assert!(
        report.evidence.matches.contains_key(&Operation::Archive),
        "其它来源（工作区）不受牵连"
    );
}

// ============================================================
// RED 四个固定名用例
// ============================================================

/// RED-1 / 验收①：未登记 build（无 shipped 登记）但当前包契约相同的操作可首次兼容尝试。
#[test]
fn unknown_build_same_contract_is_attemptable() {
    let report = report_ok(&full_files());
    assert!(
        report.evidence.matches.contains_key(&Operation::Send),
        "合成包必须命中文本发送契约"
    );
    let unknown_same = evaluate(&context_for(&report));
    assert!(unknown_same.allowed(Operation::Send));
    // 语义：兼容可尝试 ≠ 真机已验证；证据档 = 契约匹配，且锚定当前包身份。
    let send = unknown_same
        .operations
        .get(&Operation::Send)
        .expect("send status");
    assert_eq!(send.evidence, EvidenceSource::ContractMatched);
    assert!(send.verified, "契约证据锚定当前包 → verified");
    assert!(send.requires_auto_adapt, "契约匹配写动作依赖自动适配活门");
}

/// RED-2：同一包内多选形状漂移只关闭多选，单选与发送不受影响。
#[test]
fn multi_answer_drift_keeps_single_and_send() {
    let evidence = read_ok_package(multi_answer_drift_package());
    assert!(
        !evidence.matches.contains_key(&Operation::MultiAnswer),
        "multiSelect 语义漂移后不得再匹配多选"
    );
    let drift = evaluate(&context_from_evidence(evidence));
    assert!(!drift.allowed(Operation::MultiAnswer));
    assert!(drift.allowed(Operation::SingleAnswer));
    assert!(drift.allowed(Operation::Send));
    assert_eq!(
        status_reason(&drift, Operation::MultiAnswer).as_deref(),
        Some(reason::CONTRACT_UNMATCHED)
    );
}

/// RED-3：关闭自动适配 → 依赖契约匹配的新写动作立即不允许；只读与 shipped 不受影响。
#[test]
fn auto_adapt_off_blocks_new_compat_write() {
    let report = report_ok(&full_files());
    let mut off = context_for(&report);
    off.auto_adapt_enabled = false;
    off.shipped = Some(AnchoredOperations {
        package_sha256: report.evidence.package_sha256.clone(),
        operations: [Operation::Send].into_iter().collect(),
    });
    let ops = evaluate(&off);
    // Send 升级为 shipped 档：shipped 不受活门影响（对照项）。
    assert!(ops.allowed(Operation::Send));

    // 契约档写动作被撤回。
    assert!(!ops.allowed(Operation::MultiAnswer));
    assert_eq!(
        status_reason(&ops, Operation::MultiAnswer).as_deref(),
        Some(reason::AUTO_ADAPT_OFF)
    );
    // 只读收尾保留（Read 与只读目录类操作不受自动适配活门影响）。
    assert!(ops.allowed(Operation::Read));
    assert!(ops.allowed(Operation::SkillsList));

    // 没有 shipped 覆盖时，普通发送也必须被活门撤回。
    let mut off_no_shipped = off.clone();
    off_no_shipped.shipped = None;
    let ops = evaluate(&off_no_shipped);
    assert!(!ops.allowed(Operation::Send));
    assert_eq!(
        status_reason(&ops, Operation::Send).as_deref(),
        Some(reason::AUTO_ADAPT_OFF)
    );
}

/// RED-4 / N-3：只读取证成功只对只读操作构成证据，绝不解锁审批等写操作。
#[test]
fn read_success_does_not_enable_approval() {
    let evidence = read_ok_package(read_only_package());
    assert!(evidence.matches.contains_key(&Operation::Read));
    assert!(!evidence.matches.contains_key(&Operation::ApproveOnce));
    let mut read_only = context_from_evidence(evidence);
    read_only.read_evidence = true;
    let read_only = evaluate(&read_only);
    // N-3：只读运行观测为 Read 提供 ObservedSuccess 级证据（Read 因此 allowed/verified）。
    assert!(read_only.allowed(Operation::Read), "只读契约本身可用");
    let read = read_only
        .operations
        .get(&Operation::Read)
        .expect("read status");
    assert_eq!(read.evidence, EvidenceSource::ObservedSuccess);
    assert!(read.verified);
    // 写操作一律不允许，且 ApproveOnce 的原因码不是「被只读解锁」。
    assert!(!read_only.allowed(Operation::ApproveOnce));
    assert_eq!(
        status_reason(&read_only, Operation::ApproveOnce).as_deref(),
        Some(reason::CONTRACT_UNMATCHED)
    );
    // N-3 边界：`read_evidence` 是**读链**（历史/状态）的成功事实，只覆盖
    // `Operation::Read`；其它只读目录（Skills/Commands/Search）需要它们各自的
    // 契约或真实成功证据——不得借读链证据放行。
    for operation in Operation::ALL {
        if operation != Operation::Read {
            assert!(
                !read_only.allowed(operation),
                "{operation:?} 不得因只读证据解锁"
            );
        }
    }
    for operation in [
        Operation::SessionSearch,
        Operation::SkillsList,
        Operation::CommandsList,
    ] {
        assert_eq!(
            read_only.operations.get(&operation).unwrap().evidence,
            EvidenceSource::None,
            "{operation:?} 不得共享读链证据"
        );
    }
}

// ============================================================
// B-1：按方法切块（兄弟片段不得替目标方法命中）
// ============================================================

/// B-1（Blocker）：规则片段必须落在**目标方法自己的 descriptor 块**内。
///
/// 对 7 个操作各造一个负例：漂移**目标方法自身**的形状，保留兄弟方法原样（兄弟
/// 块里仍有同样的 `accepted:true` / `archivedSessionIds` / `pinnedSessionIds` /
/// `hasMore` 片段作诱饵），断言目标操作不再匹配、兄弟与无关操作不受影响。
#[test]
fn sibling_method_fragments_do_not_satisfy_rule() {
    // 正面基线：原样夹具全部命中（否则负例没有意义）。
    let baseline = read_ok(&full_files());
    for operation in [
        Operation::Read,
        Operation::Send,
        Operation::SessionSearch,
        Operation::Archive,
        Operation::Unarchive,
        Operation::Pin,
        Operation::Unpin,
    ] {
        assert!(
            baseline.matches.contains_key(&operation),
            "基线必须命中 {operation:?}"
        );
    }

    // --- Read：漂移 session/list 的 items 数组形状（session/follow 保持原样）
    let read_drifted: &'static str = Box::leak(
        SESSION_JS
            .replacen(
                r#""items":{"type":"array","items":{}},"hasMore":{"type":"boolean"}"#,
                r#""items":{"type":"object"},"hasMore":{"type":"boolean"}"#,
                1,
            )
            .into_boxed_str(),
    );
    let evidence = read_ok(&with_replaced(SESSION_PATH, read_drifted));
    assert!(
        !evidence.matches.contains_key(&Operation::Read),
        "list 的 items 形状漂移后，follow 的 snapshot 片段不得替 Read 命中"
    );
    assert!(
        evidence.matches.contains_key(&Operation::Send),
        "Send 不受影响"
    );

    // --- Send：漂移 session/prompt 的 accepted 形状（session/cancel 仍有 accepted:true）
    let send_drifted: &'static str = Box::leak(
        SESSION_JS
            .replacen(
                r#""accepted":{"type":"boolean","const":true}},"required":["accepted"]}"#,
                r#""accepted":{"type":"string"}},"required":[]}"#,
                1,
            )
            .into_boxed_str(),
    );
    let evidence = read_ok(&with_replaced(SESSION_PATH, send_drifted));
    assert!(
        !evidence.matches.contains_key(&Operation::Send),
        "prompt 的 accepted 形状漂移后，cancel 的同名字段不得替 Send 命中"
    );
    assert!(
        evidence.matches.contains_key(&Operation::CancelCurrent),
        "cancel 自己的 accepted 仍原样 → 必须仍匹配"
    );

    // --- Search：漂移 session/search 的 hasMore（session/list 仍含 hasMore 诱饵）
    // 整块替换 search（连描述里的 hasMore 一并改名），模拟真实漂移「改名」而非「改类型」。
    let drifted_search = r#"    'session/search': {
      description: 'Search visible Session content without resuming an Agent; moreAvailable describes the bounded result.',
      jsonSchema: {"type":"object","properties":{"query":{"type":"string"}}},
      result: {"type":"object","properties":{"moreAvailable":{"type":"boolean"}}},
    },
"#;
    let original_search = r#"    'session/search': {
      description: 'Search visible Session content without resuming an Agent; hasMore describes the bounded result before that filtering.',
      jsonSchema: {"type":"object","properties":{"query":{"type":"string"}}},
      result: {"type":"object","properties":{"hasMore":{"type":"boolean"}}},
    },
"#;
    let drifted_js = SESSION_JS.replacen(original_search, drifted_search, 1);
    let search_drifted: &'static str = Box::leak(drifted_js.into_boxed_str());
    let evidence = read_ok(&with_replaced(SESSION_PATH, search_drifted));
    assert!(
        !evidence.matches.contains_key(&Operation::SessionSearch),
        "search 的 hasMore 漂移后，list 的 hasMore 不得替 Search 命中"
    );
    assert!(
        evidence.matches.contains_key(&Operation::Read),
        "Read 不受影响"
    );

    // --- Archive：漂移 archiveSession 的结果数组（unarchive 仍有 archivedSessionIds）
    let archive_drifted: &'static str = Box::leak(
        WORKSPACE_JS
            .replacen(
                r#"'workspace/archiveSession': { description: 'Hide one known Session from Workspace grouping surfaces.', result: {"type":"object","properties":{"archivedSessionIds":{"type":"array","items":{"type":"string"}}}} }"#,
                r#"'workspace/archiveSession': { description: 'Hide one known Session from Workspace grouping surfaces.', result: {"type":"object","properties":{"archivedSessionIds":{"type":"string"}}} }"#,
                1,
            )
            .into_boxed_str(),
    );
    let evidence = read_ok(&with_replaced(WORKSPACE_PATH, archive_drifted));
    assert!(
        !evidence.matches.contains_key(&Operation::Archive),
        "archive 的 archivedSessionIds 漂移后，unarchive 的同名字段不得替 Archive 命中"
    );
    assert!(
        evidence.matches.contains_key(&Operation::Unarchive),
        "Unarchive 自身原样 → 必须仍匹配"
    );

    // --- Unarchive：反向漂移（archive 保持原样作诱饵）
    let unarchive_drifted: &'static str = Box::leak(
        WORKSPACE_JS
            .replacen(
                r#"'workspace/unarchiveSession': { description: 'Restore one archived Session to Workspace grouping surfaces.', result: {"type":"object","properties":{"archivedSessionIds":{"type":"array","items":{"type":"string"}}}} }"#,
                r#"'workspace/unarchiveSession': { description: 'Restore one archived Session to Workspace grouping surfaces.', result: {"type":"object","properties":{"archivedSessionIds":{"type":"string"}}} }"#,
                1,
            )
            .into_boxed_str(),
    );
    let evidence = read_ok(&with_replaced(WORKSPACE_PATH, unarchive_drifted));
    assert!(
        !evidence.matches.contains_key(&Operation::Unarchive),
        "unarchive 漂移后，archive 的同名字段不得替 Unarchive 命中"
    );
    assert!(evidence.matches.contains_key(&Operation::Archive));

    // --- Pin：漂移 pinSession（unpin 保留 pinnedSessionIds 诱饵）
    let pin_drifted: &'static str = Box::leak(
        WORKSPACE_JS
            .replacen(
                r#"'workspace/pinSession': { description: 'Surface one known unarchived Session ahead of unpinned Sessions.', result: {"type":"object","properties":{"pinnedSessionIds":{"type":"array","items":{"type":"string"}}}} }"#,
                r#"'workspace/pinSession': { description: 'Surface one known unarchived Session ahead of unpinned Sessions.', result: {"type":"object","properties":{"pinnedSessionIds":{"type":"string"}}} }"#,
                1,
            )
            .into_boxed_str(),
    );
    let evidence = read_ok(&with_replaced(WORKSPACE_PATH, pin_drifted));
    assert!(
        !evidence.matches.contains_key(&Operation::Pin),
        "pin 的 pinnedSessionIds 漂移后，unpin 的同名字段不得替 Pin 命中"
    );
    assert!(evidence.matches.contains_key(&Operation::Unpin));

    // --- Unpin：反向漂移（pin 保持原样作诱饵）
    let unpin_drifted: &'static str = Box::leak(
        WORKSPACE_JS
            .replacen(
                r#"'workspace/unpinSession': { description: "Remove one Session pin without changing its saved order.", result: {"type":"object","properties":{"pinnedSessionIds":{"type":"array","items":{"type":"string"}}}} }"#,
                r#"'workspace/unpinSession': { description: "Remove one Session pin without changing its saved order.", result: {"type":"object","properties":{"pinnedSessionIds":{"type":"string"}}} }"#,
                1,
            )
            .into_boxed_str(),
    );
    let evidence = read_ok(&with_replaced(WORKSPACE_PATH, unpin_drifted));
    assert!(
        !evidence.matches.contains_key(&Operation::Unpin),
        "unpin 漂移后，pin 的同名字段不得替 Unpin 命中"
    );
    assert!(evidence.matches.contains_key(&Operation::Pin));
}

// ============================================================
// 验收①—⑥ 具名测试
// ============================================================

/// 验收①（补充）+ N-8 + N-9：每条规则至少含**一个方法名之外的形状/语义片段**，
/// 且 `method` 是单一 `ns/method`。
#[test]
fn every_rule_carries_shape_fragments_and_a_single_method() {
    let audit = rule_audit();
    assert_eq!(audit.len(), Operation::ALL.len(), "28 个操作各一条规则");
    for entry in &audit {
        assert!(
            entry.method.matches('/').count() == 1,
            "{:?} 的 method 必须是单一 ns/method：{}",
            entry.operation,
            entry.method
        );
        assert!(
            !entry.method.contains(' ') && !entry.method.contains('+'),
            "{:?} 的 method 不得拼多方法：{}",
            entry.operation,
            entry.method
        );
        assert!(
            entry.fragment_count > 0,
            "{:?} 必须至少有一个形状/语义片段",
            entry.operation
        );
        assert!(
            entry.window_count > 0,
            "{:?} 必须有匹配窗口",
            entry.operation
        );
    }
    // 每条规则的片段都真的出现在包内（否则规则是空转）。
    let evidence = read_ok(&full_files());
    for entry in &audit {
        let matched = evidence
            .matches
            .get(&entry.operation)
            .unwrap_or_else(|| panic!("{:?} 未命中规则", entry.operation));
        assert_eq!(matched.method, entry.method);
        assert!(!matched.structural_rule.is_empty());
        assert!(!matched.semantic_rule.is_empty());
    }
    assert_eq!(evidence.rule_version, CONTRACT_RULE_VERSION);
}

/// 方法名出现本身不算契约匹配（必须核形状与语义）。
#[test]
fn method_names_alone_do_not_match() {
    let evidence = read_ok_package(keyword_only_package());
    assert!(
        evidence.matches.is_empty(),
        "只有方法名时不得产生匹配：{:?}",
        evidence.matches.keys().collect::<Vec<_>>()
    );
}

/// 验收②：同 build 号重打包（包指纹变）重新归因；依赖契约不变的操作重新匹配后保留可用。
#[test]
fn repack_same_build_reattributes_and_keeps_unchanged_operations() {
    let old = report_ok(&full_files());
    // 重打包：不动任何契约描述符，只加一个无关文件（包内容指纹必须变化）。
    let mut files = full_files();
    files.push((
        "/dsh/node_modules/@deepseek-ai/dsh-api-session-controller/lib/build-stamp.js",
        "// repack stamp 2026-10-30 (synthetic)\n",
    ));
    let repacked = report_ok(&files);
    assert_ne!(
        old.evidence.package_sha256, repacked.evidence.package_sha256,
        "重打包必须产出不同包内容指纹"
    );

    // 旧证据挂在新包身份下：不得复用（重新归因）。
    let mut stale = context_for(&old);
    stale.current_package_sha256 = repacked.evidence.package_sha256.clone();
    let stale_ops = evaluate(&stale);
    assert!(!stale_ops.allowed(Operation::Send), "旧包证据不得放行新包");
    assert_eq!(
        status_reason(&stale_ops, Operation::Send).as_deref(),
        Some(reason::CONTRACT_STALE)
    );

    // 重新识别后契约未变 → 操作保留可用（不需要人工 build 登记）。
    let fresh = evaluate(&context_for(&repacked));
    assert!(fresh.allowed(Operation::Send));
    assert!(fresh.allowed(Operation::MultiAnswer));
}

/// 验收③：规则版本换代后旧 rule_version 证据不复用。
#[test]
fn rule_generation_change_does_not_reuse_old_evidence() {
    let report = report_ok(&full_files());
    let mut bumped = context_for(&report);
    bumped.contract_rule_version = CONTRACT_RULE_VERSION + 1;
    let ops = evaluate(&bumped);
    assert!(!ops.allowed(Operation::Send));
    assert_eq!(
        status_reason(&ops, Operation::Send).as_deref(),
        Some(reason::RULES_STALE)
    );
}

/// 验收③（生产入口侧）：请求其它规则代际 → 拒绝，不把新规则结果冒充旧代际。
#[test]
fn contract_read_rejects_foreign_rule_generation() {
    let package = FakePackage::new(full_package());
    let err = read_with(&host(), CONTRACT_RULE_VERSION + 1, &package)
        .expect_err("非当前规则代际必须拒绝");
    assert_eq!(err.code, codes::CONTRACT_RULES_MISMATCH);
    // 当前代际成功，且证据版本 = 实际使用的规则表版本。
    let evidence = read_with(&host(), CONTRACT_RULE_VERSION, &package).expect("当前代际可识别");
    assert_eq!(evidence.rule_version, CONTRACT_RULE_VERSION);
}

/// 验收④ + P2-1：网络/额度业务失败不污染契约匹配，且**不会让已漂移的操作复活**。
#[test]
fn network_and_quota_failures_do_not_pollute_contract_evidence() {
    let report = report_ok(&full_files());
    let base = context_for(&report);

    for code in ["QUOTA", "Insufficient Balance", "gateway/uplink-overflow"] {
        let mut ctx = base.clone();
        let verdict = ctx.record_native_failure(Operation::Send, code);
        assert!(verdict.is_none(), "{code} 属网络/业务类，不得产生能力结论");
        let ops = evaluate(&ctx);
        assert!(ops.allowed(Operation::Send), "失败 {code} 不得污染发送契约");
        assert_eq!(
            ops.operations.get(&Operation::Send).unwrap().evidence,
            EvidenceSource::ContractMatched
        );
        assert!(ops.operations.get(&Operation::Send).unwrap().verified);
    }

    // 业务拒绝（队列项已不在）只影响该次动作，不降级无关方法。
    let mut rejected = base.clone();
    assert_eq!(
        classify_native_failure("session/queue-item-not-found"),
        FailureClass::BusinessRejection
    );
    assert!(rejected
        .record_native_failure(Operation::QueueRemove, "session/queue-item-not-found")
        .is_none());
    assert!(evaluate(&rejected).allowed(Operation::QueueRemove));

    // 协议漂移只撤受影响操作（其余操作继续）。
    let mut drifted = base.clone();
    assert_eq!(
        classify_native_failure("gateway/arguments-invalid"),
        FailureClass::ProtocolDrift
    );
    drifted.record_native_failure(Operation::Send, "gateway/arguments-invalid");
    let ops = evaluate(&drifted);
    assert!(!ops.allowed(Operation::Send));
    assert_eq!(
        status_reason(&ops, Operation::Send).as_deref(),
        Some(reason::CONTRACT_DRIFT)
    );
    assert!(ops.allowed(Operation::MultiAnswer), "无关操作不得连带关闭");

    // P2-1（fail-open 修复）：漂移结论按操作持久，另一操作的失败不得使其复活。
    drifted.record_native_failure(Operation::MultiAnswer, "gateway/uplink-overflow");
    drifted.record_native_failure(Operation::ApproveOnce, "QUOTA");
    let after = evaluate(&drifted);
    assert!(
        !after.allowed(Operation::Send),
        "无关失败覆盖单槽后 Send 不得复活"
    );
    assert_eq!(
        status_reason(&after, Operation::Send).as_deref(),
        Some(reason::CONTRACT_DRIFT)
    );
    // 只有重识别才清除漂移结论。
    drifted.clear_drift_after_reidentification();
    assert!(evaluate(&drifted).allowed(Operation::Send));

    // 认证失效 → 全部写动作撤回（不被当作业务失败吞掉）。
    let mut auth = base.clone();
    auth.record_native_failure(Operation::Send, "unpaired");
    let ops = evaluate(&auth);
    for operation in Operation::ALL {
        assert!(
            !ops.allowed(operation),
            "{operation:?} 在认证失效时必须关闭"
        );
    }
    assert_eq!(
        status_reason(&ops, Operation::ApproveOnce).as_deref(),
        Some(reason::AUTH_INVALID)
    );
}

/// 验收⑤ + P1-1：**生产报告入口**（`read_report_with`，与 `read_report` 同路径）
/// 在 header 超预算 / 单源超预算下产出「暂不可识别」，能力求值得到
/// `operation_inconclusive`，无关操作不受影响。
#[test]
fn budget_overrun_only_marks_affected_operations_inconclusive() {
    // --- 候选数预算 6：按包内路径排序丢弃部分来源
    let package = FakePackage::new(full_package());
    let capped = read_report_with(
        &host(),
        CONTRACT_RULE_VERSION,
        &package,
        &ContractBudget {
            candidate_files: 6,
            ..ContractBudget::DEFAULT
        },
    )
    .expect("候选截断仍产出局部证据");
    assert!(capped.budget_limited);
    assert!(
        capped.evidence.matches.contains_key(&Operation::Send),
        "会话描述符在预算内 → 发送仍可识别"
    );
    assert!(capped.inconclusive.contains(&Operation::MultiAnswer));
    assert!(!capped.inconclusive.contains(&Operation::Send));

    let ops = evaluate(&context_for(&capped));
    assert!(ops.allowed(Operation::Send));
    assert!(!ops.allowed(Operation::MultiAnswer));
    assert_eq!(
        status_reason(&ops, Operation::MultiAnswer).as_deref(),
        Some(reason::INCONCLUSIVE),
        "预算受限必须是「暂不可识别」而不是「永久不支持」"
    );

    // --- 单候选文件超预算：只影响锚定该文件的规则，其他照常
    let oversized = read_report_with(
        &host(),
        CONTRACT_RULE_VERSION,
        &FakePackage::new(full_package()),
        &ContractBudget {
            candidate_file_bytes: SESSION_JS.len() as u64 - 1,
            ..ContractBudget::DEFAULT
        },
    )
    .expect("单文件超限仍产出局部证据");
    assert!(oversized.budget_limited);
    assert!(oversized.inconclusive.contains(&Operation::Send));
    assert!(oversized.evidence.matches.contains_key(&Operation::Archive));
    let ops = evaluate(&context_for(&oversized));
    assert!(!ops.allowed(Operation::Send));
    assert_eq!(
        status_reason(&ops, Operation::Send).as_deref(),
        Some(reason::INCONCLUSIVE)
    );
    assert!(ops.allowed(Operation::Archive), "无关来源不受影响");

    // --- 目录头超预算：全部「暂不可识别」；且**不**把空匹配表当「契约未匹配」
    let header = read_report_with(
        &host(),
        CONTRACT_RULE_VERSION,
        &FakePackage::new(full_package()),
        &ContractBudget {
            package_header_bytes: 8,
            ..ContractBudget::DEFAULT
        },
    )
    .expect("目录头超限按 inconclusive 上报");
    assert!(header.budget_limited);
    assert!(header.evidence.matches.is_empty());
    let ops = evaluate(&context_for(&header));
    for operation in Operation::ALL {
        let status = ops.operations.get(&operation).expect("全操作状态");
        assert!(status.implemented, "不是未实现");
        assert!(!status.allowed);
        assert_eq!(
            status.reason_code.as_deref(),
            Some(reason::INCONCLUSIVE),
            "{operation:?}"
        );
    }

    // P1-1 的陷阱面：把 budget-limited 报告**只取 evidence** 交给求值（等价于
    // `read` 的投影：它丢弃 inconclusive 事实）→ 空匹配表被误读成「契约未匹配」。
    let trap = evaluate(&context_from_evidence(header.evidence.clone()));
    // 丢事实的后果是「一个具体但不正确的拒绝理由」：空指纹被读成「证据过期」，
    // 而不是「暂不可识别」。无论措辞如何，结论都是错的——这就是 read 被禁止
    // 用于能力求值的原因。
    assert_eq!(
        status_reason(&trap, Operation::Send).as_deref(),
        Some(reason::CONTRACT_STALE),
        "丢弃预算事实的后果：把「没读到」误读成「证据过期」"
    );
    // 对照：固定签名 `read` 用默认预算读同一包 → 正常全量证据（陷阱是静默的：
    // 调用方看不出自己丢掉了什么，所以纪律必须写在入口文档里）。
    let plain = read_with(
        &host(),
        CONTRACT_RULE_VERSION,
        &FakePackage::new(full_package()),
    )
    .expect("read 用默认预算应成功");
    assert!(plain.matches.contains_key(&Operation::Send));
}

/// 验收⑥：只读取证路径零写调用（文件源只有 `open` 读面）。
#[test]
fn read_path_performs_no_business_writes() {
    let package = FakePackage::new(full_package());
    let evidence = read_with(&host(), CONTRACT_RULE_VERSION, &package).expect("识别成功");
    assert!(!evidence.matches.is_empty());
    let reads = package.reads();
    assert!(!reads.is_empty());
    // P2-2：生产路径只打开一次（同一句柄顺序读）。
    assert_eq!(package.opens(), 1, "只应打开一次包文件");
    // 全部读都落在包内（不越界、不读其它文件）。
    let total = package.bytes.len() as u64;
    for (offset, len) in &reads {
        assert!(
            *offset + *len <= total,
            "读取越界：{offset}+{len} > {total}"
        );
    }
    // 结构面：ContractFileSource 无写方法、ContractEvidence 只有数据字段（编译期事实）。
}

/// P2-2：目录头超预算时**不得**先付出整包哈希代价（只读前缀 + 头判定）。
#[test]
fn header_over_budget_does_not_hash_the_whole_package() {
    let package = FakePackage::new(full_package());
    let report = read_report_with(
        &host(),
        CONTRACT_RULE_VERSION,
        &package,
        &ContractBudget {
            package_header_bytes: 8,
            ..ContractBudget::DEFAULT
        },
    )
    .expect("header over budget is reported, not an error");
    assert!(report.budget_limited);
    assert!(
        package.bytes_read() < package.bytes.len() as u64,
        "header 超预算时不应读完整包（实际读 {} / 总 {}）",
        package.bytes_read(),
        package.bytes.len()
    );
    assert!(
        report.evidence.package_sha256.is_empty(),
        "未读整包时不得编造内容指纹"
    );
}

/// P2-3：IO 失败保留 `STATE_IO`（不被改写成 `DSH_FRAME_INVALID`），且候选读取
/// 失败记 `unresolved` 而**不是** `budget_limited`。
#[test]
fn io_failures_keep_io_code_and_are_not_budget_limited() {
    // 目录头读取失败 → 原样上抛 STATE_IO。
    let broken_header = FakePackage::failing(full_package(), 0);
    let err = read_report_with(
        &host(),
        CONTRACT_RULE_VERSION,
        &broken_header,
        &ContractBudget::DEFAULT,
    )
    .expect_err("header 读取失败必须报错");
    assert_eq!(err.code, codes::STATE_IO, "IO 失败不得伪装成帧错误");

    // 候选文件读取失败 → 该来源的操作进 unresolved，budget_limited 保持 false。
    let package = FakePackage::new(full_package());
    let probe = read_report_with(
        &host(),
        CONTRACT_RULE_VERSION,
        &package,
        &ContractBudget::DEFAULT,
    )
    .expect("baseline ok");
    assert!(probe.evidence.matches.contains_key(&Operation::Send));
    let session_offset = asar_offset_of(&package.bytes, SESSION_PATH);
    let broken = FakePackage::failing(full_package(), session_offset);
    let report = read_report_with(
        &host(),
        CONTRACT_RULE_VERSION,
        &broken,
        &ContractBudget::DEFAULT,
    )
    .expect("候选读失败仍返回局部证据");
    assert!(
        report.unresolved.contains(&Operation::Send),
        "读取失败的来源必须记 unresolved"
    );
    assert!(!report.budget_limited, "读取失败不是预算受限（P2-3/N-7）");
    assert!(!report.inconclusive.contains(&Operation::Send));
    let ops = evaluate(&context_for(&report));
    assert!(!ops.allowed(Operation::Send));
    assert_eq!(
        status_reason(&ops, Operation::Send).as_deref(),
        Some(reason::INCONCLUSIVE),
        "unresolved 与预算受限同样映射为「暂不可识别」"
    );
    assert!(ops.allowed(Operation::Archive), "无关来源不受影响");
}

/// N-10：同后缀多副本内容不一致 → 该来源多义（unresolved），不取第一份。
#[test]
fn ambiguous_duplicate_source_is_unresolved() {
    let mut files = full_files();
    // 第二个会话描述符副本（内容不同：删掉 prompt 块的关键片段）。
    let duplicate: &'static str = Box::leak(
        SESSION_JS
            .replace(
                r#""required":["requestId","sessionId","mode","content"]"#,
                "",
            )
            .into_boxed_str(),
    );
    files.push((
        "/vendor/copy/@deepseek-ai/dsh-api-session-controller/lib/typert.host.js",
        duplicate,
    ));
    let report = read_report_with(
        &host(),
        CONTRACT_RULE_VERSION,
        &FakePackage::new(asar(&files)),
        &ContractBudget::DEFAULT,
    )
    .expect("多义副本按 unresolved 上报");
    assert!(!report.evidence.matches.contains_key(&Operation::Send));
    assert!(report.unresolved.contains(&Operation::Send));
    assert!(!report.budget_limited);
}

/// N-5：asar 目录嵌套超过深度上限 → header 判非法（畸形包防御）。
#[test]
fn over_deep_asar_directory_is_rejected() {
    // 手工构造 40 层嵌套的目录 JSON。
    let mut node = serde_json::json!({"size": 0, "offset": "0"});
    for depth in 0..40 {
        node = serde_json::json!({"files": {format!("d{depth}"): node}});
    }
    let json = serde_json::to_vec(&serde_json::json!({"files": {"top": node}})).expect("json");
    let header_size = 8 + json.len() as u64;
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(&4u32.to_le_bytes());
    bytes.extend_from_slice(&(header_size as u32).to_le_bytes());
    bytes.extend_from_slice(&((json.len() as u32) + 6).to_le_bytes());
    bytes.extend_from_slice(&(json.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&json);
    let err = read_report_with(
        &host(),
        CONTRACT_RULE_VERSION,
        &FakePackage::new(bytes),
        &ContractBudget::DEFAULT,
    )
    .expect_err("过深目录必须拒绝");
    assert_eq!(err.code, codes::FRAME_INVALID);
    assert!(
        err.detail.contains("nesting"),
        "原因必须指向嵌套深度：{}",
        err.detail
    );
}

/// verified 语义 + P1-2：只有锚定**整包内容指纹**的证据才 verified；
/// 描述符级哈希（`LocalHost.package_sha256`）不得冒充当前包身份。
#[test]
fn verified_requires_content_fingerprint_not_descriptor_hash() {
    let report = report_ok(&full_files());
    let content_sha = report.evidence.package_sha256.clone();
    assert_eq!(content_sha.len(), 64);
    let ops = evaluate(&context_for(&report));
    assert!(ops.operations.get(&Operation::Send).unwrap().verified);

    // 陷阱复现：把 identity 的描述符哈希填进上下文 → 必须显式判 stale
    // （锁定保护行为 = 不允许用描述符哈希冒充）。
    let mut wrong = context_for(&report);
    wrong.current_package_sha256 = host().package_sha256.clone();
    assert_ne!(host().package_sha256, content_sha, "两层指纹必须不同");
    let ops = evaluate(&wrong);
    assert!(!ops.allowed(Operation::Send));
    assert_eq!(
        status_reason(&ops, Operation::Send).as_deref(),
        Some(reason::CONTRACT_STALE)
    );

    // shipped 表锚定旧包 → 不参与判定。
    let mut old_shipped = context_for(&report);
    old_shipped.shipped = Some(AnchoredOperations {
        package_sha256: "synthetic-old-package".into(),
        operations: [Operation::Send].into_iter().collect(),
    });
    let ops = evaluate(&old_shipped);
    assert_eq!(
        ops.operations.get(&Operation::Send).unwrap().evidence,
        EvidenceSource::ContractMatched,
        "旧包 shipped 不得覆盖当前包契约证据"
    );

    // shipped 锚定当前包 → 证据升级且不依赖自动适配。
    let mut ctx = context_for(&report);
    ctx.shipped = Some(AnchoredOperations {
        package_sha256: content_sha.clone(),
        operations: [Operation::Send].into_iter().collect(),
    });
    ctx.auto_adapt_enabled = false;
    let ops = evaluate(&ctx);
    let send = ops.operations.get(&Operation::Send).unwrap();
    assert_eq!(send.evidence, EvidenceSource::Shipped);
    assert!(send.verified);
    assert!(!send.requires_auto_adapt);
    assert!(send.allowed);

    // 空包身份不得把空 shipped 表当成锚定。
    let mut empty = context_for(&report);
    empty.current_package_sha256 = String::new();
    empty.shipped = Some(AnchoredOperations {
        package_sha256: String::new(),
        operations: [Operation::Read].into_iter().collect(),
    });
    assert!(
        !evaluate(&empty)
            .operations
            .get(&Operation::Read)
            .unwrap()
            .verified
    );
}

/// `allowed` = implemented ∧ allowed（implemented 是独立事实，不因证据存在而伪造）。
#[test]
fn allowed_requires_implemented_fact() {
    let report = report_ok(&full_files());
    let mut ctx = context_for(&report);
    ctx.implemented.remove(&Operation::Send);
    let ops = evaluate(&ctx);
    assert!(!ops.allowed(Operation::Send));
    assert!(!ops.operations.get(&Operation::Send).unwrap().implemented);
    assert_eq!(
        status_reason(&ops, Operation::Send).as_deref(),
        Some(reason::NOT_IMPLEMENTED)
    );
    // 对照：同一上下文里仍在 implemented 集内的操作不受牵连。
    assert!(ops.allowed(Operation::Pin));
    // 契约命中也救不了 implemented=false：Steer 也从实现集移除 → 同样不允许。
    ctx.implemented.remove(&Operation::Steer);
    let ops = evaluate(&ctx);
    assert!(
        ctx.contract
            .as_ref()
            .is_some_and(|evidence| evidence.matches.contains_key(&Operation::Steer)),
        "前提：Steer 在契约证据里"
    );
    assert!(!ops.allowed(Operation::Steer));
    assert_eq!(
        status_reason(&ops, Operation::Steer).as_deref(),
        Some(reason::NOT_IMPLEMENTED)
    );
}

/// revision 是稳定事实指纹：同事实同值；事实变则变。
#[test]
fn revision_is_stable_fact_fingerprint() {
    let report = report_ok(&full_files());
    let ctx = context_for(&report);
    let a = evaluate(&ctx);
    let b = evaluate(&ctx);
    assert_eq!(a.revision, b.revision);

    let mut changed = ctx.clone();
    changed.auto_adapt_enabled = false;
    let c = evaluate(&changed);
    assert_ne!(a.revision, c.revision);
}

/// 失败分类词表：四档各自可达（类型化，不靠散落字符串）。
#[test]
fn failure_classification_distinguishes_four_classes() {
    assert_eq!(
        classify_native_failure("gateway/arguments-invalid"),
        FailureClass::ProtocolDrift
    );
    assert_eq!(
        classify_native_failure("session/queue-item-not-found"),
        FailureClass::BusinessRejection
    );
    assert_eq!(
        classify_native_failure("unpaired"),
        FailureClass::AuthInvalid
    );
    assert_eq!(
        classify_native_failure("gateway/uplink-overflow"),
        FailureClass::NetworkTransient
    );
    // 未知码保守归入业务拒绝（不得当成协议漂移去降级能力）。
    assert_eq!(
        classify_native_failure("some/future-code"),
        FailureClass::BusinessRejection
    );
}

/// 契约识别失败原因进入逐操作状态（不把「读不到包」显示成「无证据=不支持」）。
#[test]
fn contract_read_failure_is_reported_per_operation() {
    let report = report_ok(&full_files());
    let mut ctx = context_for(&report);
    ctx.contract = None;
    ctx.contract_error = Some("contract_package_malformed".into());
    let ops = evaluate(&ctx);
    assert!(!ops.allowed(Operation::Send));
    assert_eq!(
        status_reason(&ops, Operation::Send).as_deref(),
        Some(reason::CONTRACT_READ_FAILED)
    );
    assert_eq!(
        status_reason(&ops, Operation::ApproveOnce).as_deref(),
        Some(reason::CONTRACT_READ_FAILED)
    );
}

// ============================================================
// 小工具
// ============================================================

/// 在 asar 目录里查某条目的数据区 offset（测试用；只为注入读取失败）。
fn asar_offset_of(bytes: &[u8], entry_suffix: &str) -> u64 {
    let header_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as u64;
    let json_size = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as u64;
    let data_start = 8 + header_size;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes[16..16 + json_size as usize]).expect("header json");
    let mut found = None;
    fn walk(
        node: &serde_json::Value,
        prefix: &str,
        data_start: u64,
        suffix: &str,
        found: &mut Option<u64>,
    ) {
        let Some(files) = node.get("files").and_then(|value| value.as_object()) else {
            return;
        };
        for (name, meta) in files {
            let path = format!("{prefix}/{name}");
            if meta.get("files").is_some() {
                walk(meta, &path, data_start, suffix, found);
            } else if path.ends_with(suffix) {
                let offset = meta
                    .get("offset")
                    .and_then(|value| {
                        value
                            .as_str()
                            .and_then(|text| text.parse::<u64>().ok())
                            .or_else(|| value.as_u64())
                    })
                    .unwrap_or(0);
                *found = Some(data_start + offset);
            }
        }
    }
    walk(&json, "", data_start, entry_suffix, &mut found);
    found.unwrap_or_else(|| panic!("entry {entry_suffix} not found"))
}

/// N-11（跨平台路径）的集成面：Windows 分支纯词法推导，与 Mac 分支并列。
#[test]
fn platform_package_paths_are_lexically_derived() {
    let mac = package_path(&host()).expect("mac path");
    assert!(mac
        .to_string_lossy()
        .ends_with("Contents/Resources/app.asar"));
    let mut windows = host();
    windows.platform = "windows".into();
    windows.executable = PathBuf::from(r"D:\Program Files\Deepseek-Harness\DeepSeek Harness.exe");
    let win = package_path(&windows).expect("windows path");
    assert_eq!(
        win.to_string_lossy(),
        r"D:\Program Files\Deepseek-Harness\resources\app.asar"
    );
}

// ============================================================
// 复审修复轮 3：Minor / Nit（Mi-1..Mi-5）
// ============================================================

/// Mi-1：commands/list 的结果形状（`{"type":"array","items":{…definitionId…}}`，
/// 本机 0.2.1-rc.3 实读全文件唯一）漂移必须失配——散文句与 wire 片段不得替
/// 形状买单。
#[test]
fn commands_list_result_shape_drift_is_detected() {
    // 前提：原样夹具里 CommandsList 命中（形状片段来自真实包 result.jsonSchema）。
    let baseline = read_ok(&full_files());
    assert!(baseline.matches.contains_key(&Operation::CommandsList));

    // 漂移：结果从数组漂成对象（description 散文与 scope.wire 保持原样）。
    let drifted: &'static str = Box::leak(
        COMMANDS_JS
            .replacen(
                r#""type":"array","items":{"type":"object","properties":{"definitionId":{"type":"string"},"name":{"type":"string"}}}}"#,
                r#""type":"object""#,
                1,
            )
            .into_boxed_str(),
    );
    let evidence = read_ok(&with_replaced(COMMANDS_PATH, drifted));
    assert!(
        !evidence.matches.contains_key(&Operation::CommandsList),
        "结果形状漂移后，散文与 wire 片段不得替 CommandsList 命中"
    );
    assert!(
        evidence.matches.contains_key(&Operation::CommandExecute),
        "同文件另一方法不受影响"
    );
}

/// Mi-4：整包哈希期间的数据区短读 = 只读句柄契约被破坏——不得静默截断产出
/// 「伪指纹 + 零匹配」的成功报告。头部必须完整（探针只对数据区短读）。
#[test]
fn short_read_during_hash_is_not_silent() {
    struct ShortDataRegion(Vec<u8>);
    impl ContractFileSource for ShortDataRegion {
        fn open(&self, _path: &Path) -> Result<Box<dyn ContractFileReader + '_>, DshError> {
            struct R<'a>(&'a Vec<u8>);
            impl ContractFileReader for R<'_> {
                fn len(&self) -> u64 {
                    self.0.len() as u64
                }
                fn read_range(&mut self, offset: u64, len: u64) -> Result<Vec<u8>, DshError> {
                    let start = usize::try_from(offset)
                        .map_err(|_| DshError::new(codes::STATE_IO, "offset overflow"))?;
                    let end =
                        (start + usize::try_from(len).unwrap_or(usize::MAX)).min(self.0.len());
                    // 头部区完整交付（json_size 在前 16 字节里）；数据区恒短读。
                    let json_size =
                        u32::from_le_bytes(self.0[12..16].try_into().expect("prefix")) as usize;
                    let header_end = 16 + json_size;
                    if end <= header_end {
                        return Ok(self.0[start..end].to_vec());
                    }
                    let take = (end - start).saturating_sub(1);
                    Ok(self.0[start..start + take].to_vec())
                }
            }
            Ok(Box::new(R(&self.0)))
        }
    }
    let err = read_report_with(
        &host(),
        CONTRACT_RULE_VERSION,
        &ShortDataRegion(full_package()),
        &ContractBudget::DEFAULT,
    )
    .expect_err("数据区短读必须报错，不得返回伪指纹成功报告");
    assert_eq!(err.code, codes::STATE_IO, "短读按 IO 契约破坏上报");
}

/// Mi-5（锁定）：认证失效是目标级结论——`anchor_to` 重识别契约不得顺带复位它
/// （复位走 `clear_auth_invalid`）。
#[test]
fn anchor_to_keeps_auth_invalid() {
    let report = report_ok(&full_files());
    let mut ctx = context_for(&report);
    ctx.record_native_failure(Operation::Send, "unpaired");
    assert!(ctx.auth_invalid);
    let ctx = ctx.anchor_to(&report).expect("报告为当前代际");
    assert!(ctx.auth_invalid, "重识别契约不得复位认证失效");
    assert!(!evaluate(&ctx).allowed(Operation::Send));
    assert_eq!(
        status_reason(&evaluate(&ctx), Operation::Send).as_deref(),
        Some(reason::AUTH_INVALID)
    );
    // 显式复位后才恢复。
    let mut ctx = ctx;
    ctx.clear_auth_invalid();
    assert!(evaluate(&ctx).allowed(Operation::Send));
}

/// Mi-5（锁定）：数据区被截断（descriptor 块 `}` 未闭合）的畸形包——该方法
/// 不得匹配、不得 panic、不得把后续内容吞进块里（不吞尾）。
#[test]
fn unclosed_method_block_is_not_matched() {
    // 在 session/prompt 块内部截断整个文件（模拟数据区被截断的畸形包）。
    let cut = SESSION_JS
        .find(r#""required":["requestId","sessionId","mode","content"]"#)
        .expect("prompt jsonSchema required array");
    let truncated: &'static str = Box::leak(SESSION_JS[..cut].to_string().into_boxed_str());
    let report = read_report_with(
        &host(),
        CONTRACT_RULE_VERSION,
        &FakePackage::new(asar(&with_replaced(SESSION_PATH, truncated))),
        &ContractBudget::DEFAULT,
    )
    .expect("畸形块按未匹配上报，不 panic");
    assert!(!report.budget_limited);
    assert!(
        !report.evidence.matches.contains_key(&Operation::Send),
        "未闭合的 prompt 块不得命中 Send"
    );
    assert!(
        !report
            .evidence
            .matches
            .contains_key(&Operation::CancelCurrent),
        "块不配对时不得吞并后续方法的内容"
    );
    assert!(
        report.evidence.matches.contains_key(&Operation::Archive),
        "其它来源（工作区）不受牵连"
    );
}

// ============================================================
// 复审修复轮 4：N1（干净报告不洗漂移）/ N3（header JSON 短读拒绝）
// ============================================================

/// N1：干净报告里「重读但未匹配」的操作**没有**可解除漂移的新结论——它可能
/// 正是漂移形状本身的证据。`anchor_to` 对三种报告形态统一为「只解除实际重读
/// 且匹配（matches 命中）的操作」，否则 shipped 档会把已漂移操作复活（fail-open）。
#[test]
fn clean_report_unmatched_operation_keeps_drift() {
    // 真 descriptor 漂移：session/prompt 的 accepted 形状改变 → 干净报告 27/28。
    let drifted: &'static str = Box::leak(
        SESSION_JS
            .replacen(
                r#""accepted":{"type":"boolean","const":true}},"required":["accepted"]}"#,
                r#""accepted":{"type":"string"}},"required":[]}"#,
                1,
            )
            .into_boxed_str(),
    );
    let report = read_report_with(
        &host(),
        CONTRACT_RULE_VERSION,
        &FakePackage::new(asar(&with_replaced(SESSION_PATH, drifted))),
        &ContractBudget::DEFAULT,
    )
    .expect("漂移包仍可识别");
    assert!(!report.budget_limited, "前提：干净报告（无预算受限）");
    assert!(
        report.inconclusive.is_empty() && report.unresolved.is_empty(),
        "前提：无 undecided"
    );
    assert!(!report.evidence.matches.contains_key(&Operation::Send));
    assert_eq!(report.evidence.matches.len(), 27, "前提：27/28");

    // 上下文：Send 有同指纹 shipped 事实 + 运行时漂移结论在场。
    let mut ctx = context_for(&report);
    ctx.shipped = Some(AnchoredOperations {
        package_sha256: report.evidence.package_sha256.clone(),
        operations: [Operation::Send].into_iter().collect(),
    });
    ctx.record_native_failure(Operation::Send, "gateway/arguments-invalid");

    let ctx = ctx.anchor_to(&report).expect("报告为当前代际");
    assert!(
        !evaluate(&ctx).allowed(Operation::Send),
        "重读但未匹配的操作不得被 shipped 复活"
    );
    assert_eq!(
        status_reason(&evaluate(&ctx), Operation::Send).as_deref(),
        Some(reason::CONTRACT_DRIFT),
        "拒绝原因仍是漂移"
    );
}

/// N3：header JSON 区短读 = 句柄契约被破坏——注入 reader 返回更短但**合法**的
/// JSON（如 `{"files":{}}`）会产出「干净全零匹配报告 + 非空指纹」，与漂移解除
/// 逻辑组合会清空全部漂移结论。必须按畸形头拒绝。
#[test]
fn short_header_json_is_rejected() {
    struct ShortJson(Vec<u8>);
    impl ContractFileSource for ShortJson {
        fn open(&self, _path: &Path) -> Result<Box<dyn ContractFileReader + '_>, DshError> {
            struct R<'a>(&'a Vec<u8>);
            impl ContractFileReader for R<'_> {
                fn len(&self) -> u64 {
                    self.0.len() as u64
                }
                fn read_range(&mut self, offset: u64, len: u64) -> Result<Vec<u8>, DshError> {
                    let start = usize::try_from(offset)
                        .map_err(|_| DshError::new(codes::STATE_IO, "offset overflow"))?;
                    let end =
                        (start + usize::try_from(len).unwrap_or(usize::MAX)).min(self.0.len());
                    // 头 JSON 区整段读取时恒返回更短但合法的 JSON（目录为空）；
                    // 其余读全部足额（整包哈希因此成功、指纹非空）。
                    let json_size =
                        u32::from_le_bytes(self.0[12..16].try_into().expect("prefix")) as u64;
                    if offset == 16 && len == json_size {
                        return Ok(b"{\"files\":{}}".to_vec());
                    }
                    Ok(self.0[start..end].to_vec())
                }
            }
            Ok(Box::new(R(&self.0)))
        }
    }
    let err = read_report_with(
        &host(),
        CONTRACT_RULE_VERSION,
        &ShortJson(full_package()),
        &ContractBudget::DEFAULT,
    )
    .expect_err("头 JSON 短读必须拒绝，不得产出全零匹配的干净报告");
    assert_eq!(err.code, codes::FRAME_INVALID, "头区域契约破坏按畸形头拒绝");
}

// ============================================================
// 复审修复轮 4：N2（属性键前界）/ N4（块内注释不承载片段）/ N6（行注释）
// ============================================================

/// N2：`submethods:` 等更长标识符里的 `methods:{` 子串不是区域起点——前置伪区
/// 内嵌原样 prompt 副本 + 真 prompt 漂移 → 必须失配（伪区不得遮蔽真 methods 区）。
#[test]
fn shadow_submethods_object_does_not_mask_real_methods() {
    let prompt_start = SESSION_JS
        .find("    'session/prompt': {")
        .expect("fixture prompt block");
    let prompt_end = SESSION_JS[prompt_start..]
        .find("    'session/modelCatalog': {")
        .map(|offset| prompt_start + offset)
        .expect("next block");
    let prompt_block = &SESSION_JS[prompt_start..prompt_end];
    let shadow: &'static str = Box::leak(
        format!("const shadow = {{ submethods: {{{prompt_block}}} }};\n").into_boxed_str(),
    );
    // 真 prompt 漂移（shadow 里的副本保持原样）。
    let drifted: &'static str = Box::leak(
        SESSION_JS
            .replacen(
                r#""accepted":{"type":"boolean","const":true}},"required":["accepted"]}"#,
                r#""accepted":{"type":"string"}},"required":[]}"#,
                1,
            )
            .into_boxed_str(),
    );
    let combined: &'static str = Box::leak(format!("{shadow}{drifted}").into_boxed_str());
    let evidence = read_ok_package(asar(&with_replaced(SESSION_PATH, combined)));
    assert!(
        !evidence.matches.contains_key(&Operation::Send),
        "submethods 伪区不得遮蔽真 methods 区"
    );
    assert!(
        evidence.matches.contains_key(&Operation::CancelCurrent),
        "真区内其它方法不受影响"
    );
}

/// N4：descriptor 块内的块注释（含旧版本残留片段）不承载契约片段——真 accepted
/// 漂移后，注释里的原样片段不得替真形状命中（剥离须字符串感知：字符串里的
/// `/*` 不是注释，字符串承载片段仍可达 = 已申报边界）。
#[test]
fn comment_inside_method_block_does_not_carry_fragments() {
    let drifted_with_comment: &'static str = Box::leak(
        SESSION_JS
            .replacen(
                r#"      result: {"type":"object","properties":{"accepted":{"type":"boolean","const":true}},"required":["accepted"]},"#,
                concat!(
                    r#"      /* stale copy: {"accepted":{"type":"boolean","const":true}},"required":["accepted"]} */"#,
                    "\n",
                    r#"      result: {"type":"object","properties":{"accepted":{"type":"string"}},"required":[]},"#,
                ),
                1,
            )
            .into_boxed_str(),
    );
    let evidence = read_ok_package(asar(&with_replaced(SESSION_PATH, drifted_with_comment)));
    assert!(
        !evidence.matches.contains_key(&Operation::Send),
        "块内注释承载的旧片段不得替漂移的真形状命中"
    );
    assert!(
        evidence.matches.contains_key(&Operation::CancelCurrent),
        "同文件其它方法不受影响"
    );
}

/// N9：methods 区内的块注释里放一份**原样** prompt descriptor 副本（含锚点形态
/// `'ns/method':{`）——注释里的锚点不是锚点（锁定用例；真 prompt 漂移必须失配）。
#[test]
fn pseudo_descriptor_copy_in_region_comment_is_ignored() {
    let prompt_start = SESSION_JS
        .find("    'session/prompt': {")
        .expect("fixture prompt block");
    let prompt_end = SESSION_JS[prompt_start..]
        .find("    'session/modelCatalog': {")
        .map(|offset| prompt_start + offset)
        .expect("next block");
    let prompt_block = &SESSION_JS[prompt_start..prompt_end];
    let comment = format!("    /* stale descriptor copy:\n{prompt_block}*/\n");
    let drifted: &'static str = Box::leak(
        SESSION_JS
            .replacen(
                r#""accepted":{"type":"boolean","const":true}},"required":["accepted"]}"#,
                r#""accepted":{"type":"string"}},"required":[]}"#,
                1,
            )
            .into_boxed_str(),
    );
    let at = drifted
        .find("    'session/prompt': {")
        .expect("prompt in drifted");
    let combined: &'static str =
        Box::leak(format!("{}{}{}", &drifted[..at], comment, &drifted[at..]).into_boxed_str());
    let evidence = read_ok_package(asar(&with_replaced(SESSION_PATH, combined)));
    assert!(
        !evidence.matches.contains_key(&Operation::Send),
        "区内注释副本不得替漂移的真块命中"
    );
    assert!(
        evidence.matches.contains_key(&Operation::CancelCurrent),
        "真块之外的方法不受影响"
    );
}

/// N6：`//` 行注释依赖换行定界——归一化去掉了换行，在归一化文本上定位会把
/// `//` 之后吞到文件尾（合法包被误杀）。定位在**原始文本**上做：methods 区前
/// 的行注释（内嵌 `methods:{` / `'ns/method':{` 诱饵）不得影响匹配。
#[test]
fn line_comment_does_not_truncate_source() {
    let baseline = read_ok(&full_files());
    let commented: &'static str = Box::leak(
        format!(
            "// build-note: the generator emits methods: {{ ... }} blocks like 'session/prompt': {{...}} below\n{SESSION_JS}"
        )
        .into_boxed_str(),
    );
    let evidence = read_ok(&with_replaced(SESSION_PATH, commented));
    assert_eq!(
        evidence.matches.len(),
        baseline.matches.len(),
        "行注释不得改变匹配面（基线 {} 个）",
        baseline.matches.len()
    );
    assert!(evidence.matches.contains_key(&Operation::Send));
}

// ============================================================
// T09 接线前小批次：C-1（Whole/Between 剥注释）/ C-2（return 前界）/ C-3（异代拒绝）
// ============================================================

/// C-1：Whole/Between 窗口的片段匹配必须基于**剥注释**文本——审批文件代码里的
/// `allowed-once` 删掉、块注释里保留原片段 → ApproveOnce 不得再命中（复审者
/// 真实包实证形态；字符串内容保留 = 已申报边界）。
#[test]
fn whole_window_comment_does_not_carry_fragments() {
    // 前提：原样夹具里 ApproveOnce / Reject 都命中（两者共享 OUTCOMES 片段）。
    let baseline = read_ok(&full_files());
    assert!(baseline.matches.contains_key(&Operation::ApproveOnce));
    assert!(baseline.matches.contains_key(&Operation::Reject));

    const COMMENTED_APPROVAL: &str = concat!(
        "/* const OUTCOMES = ['allowed-once', 'rejected', 'cancelled', 'unavailable']; */\n",
        "const OUTCOMES = ['rejected', 'cancelled', 'unavailable'];\n",
    );
    let evidence = read_ok(&with_replaced(APPROVAL_TYPES_PATH, COMMENTED_APPROVAL));
    assert!(
        !evidence.matches.contains_key(&Operation::ApproveOnce),
        "代码删词后，注释里的原片段不得替 ApproveOnce 命中"
    );
    assert!(
        !evidence.matches.contains_key(&Operation::Reject),
        "共享同一片段的 Reject 同样失配"
    );
    assert!(
        evidence.matches.contains_key(&Operation::Archive),
        "无关操作不受影响"
    );
}

/// C-2：`return` 前界回退分支删除——合法键 `returnmethods:` 里的 `methods:{`
/// 子串（前一字节是标识符字符 `n`）不是区域起点；returnmethods 影子 + 真 prompt
/// 漂移 → 必须失配（合法 JS 的 `return { methods: … }` 已被 `{` 分支覆盖）。
#[test]
fn returnmethods_key_is_not_a_methods_region() {
    let prompt_start = SESSION_JS
        .find("    'session/prompt': {")
        .expect("fixture prompt block");
    let prompt_end = SESSION_JS[prompt_start..]
        .find("    'session/modelCatalog': {")
        .map(|offset| prompt_start + offset)
        .expect("next block");
    let prompt_block = &SESSION_JS[prompt_start..prompt_end];
    let shadow: &'static str =
        Box::leak(format!("const x = {{ returnmethods: {{{prompt_block}}} }};\n").into_boxed_str());
    let drifted: &'static str = Box::leak(
        SESSION_JS
            .replacen(
                r#""accepted":{"type":"boolean","const":true}},"required":["accepted"]}"#,
                r#""accepted":{"type":"string"}},"required":[]}"#,
                1,
            )
            .into_boxed_str(),
    );
    let combined: &'static str = Box::leak(format!("{shadow}{drifted}").into_boxed_str());
    let evidence = read_ok_package(asar(&with_replaced(SESSION_PATH, combined)));
    assert!(
        !evidence.matches.contains_key(&Operation::Send),
        "returnmethods 伪区不得成为 methods 区域起点"
    );
    assert!(
        evidence.matches.contains_key(&Operation::CancelCurrent),
        "真区内其它方法不受影响"
    );
}

/// C-1（Between 分支，轻量复审 Nit-1）：声明区跨度内的块注释承载的旧片段不得替
/// 代码命中——`declaration` 里的 `multiSelect?: boolean;` 漂移、Item 锚前块注释
/// 保留原片段 → MultiAnswer 必须失配（Between 窗口与 Whole 同走剥注释文本）。
#[test]
fn between_window_comment_does_not_carry_fragments() {
    let drifted: &'static str = Box::leak(
        QUESTIONS_JS
            .replace("multiSelect?: boolean;", "multiSelect?: number;")
            .into_boxed_str(),
    );
    let commented: &'static str = Box::leak(
        drifted
            .replacen(
                "  {\"name\":\"AskUserQuestionItem\"",
                "  /* multiSelect?: boolean; */\n  {\"name\":\"AskUserQuestionItem\"",
                1,
            )
            .into_boxed_str(),
    );
    let evidence = read_ok_package(asar(&with_replaced(QUESTIONS_PATH, commented)));
    assert!(
        !evidence.matches.contains_key(&Operation::MultiAnswer),
        "注释承载的旧片段不得替漂移的 declaration 命中"
    );
    assert!(
        evidence.matches.contains_key(&Operation::Send),
        "无关操作不受影响"
    );
}

/// C-3：`anchor_to` 拒绝**异代报告**——旧代报告的 matches 讲的是另一代规则下的
/// 结论，把它当「实际重读且匹配」清漂移会让同指纹 shipped 复活已漂移操作（复审
/// 实证：旧代 28/28 报告 + 同指纹 shipped → Send 复活）。拒绝必须整体：不锚指纹、
/// 不清漂移、不碰上下文任何字段。
#[test]
fn stale_generation_report_cannot_be_anchored() {
    let report = report_ok(&full_files());
    let mut stale = report.clone();
    stale.evidence.rule_version = CONTRACT_RULE_VERSION - 1;

    let mut ctx = context_for(&report);
    ctx.shipped = Some(AnchoredOperations {
        package_sha256: report.evidence.package_sha256.clone(),
        operations: [Operation::Send].into_iter().collect(),
    });
    ctx.record_native_failure(Operation::Send, "gateway/arguments-invalid");
    let before = ctx.clone();

    let err = ctx
        .clone()
        .anchor_to(&stale)
        .expect_err("异代报告必须被整体拒绝");
    assert_eq!(
        err.code,
        codes::CONTRACT_RULES_MISMATCH,
        "错误码复用规则代际不一致"
    );

    // 上下文不变：drifted 保留、shipped 不得复活。（轻量复审 Nit-2：按值签名下
    // `ctx == before` 恒真，不再断言；零副作用由 Err 前零副作用的实现注释申报。）
    let _ = before;
    assert!(ctx.drifted.contains(&Operation::Send), "漂移结论必须保留");
    assert!(!evaluate(&ctx).allowed(Operation::Send), "shipped 不得复活");
    assert_eq!(
        status_reason(&evaluate(&ctx), Operation::Send).as_deref(),
        Some(reason::CONTRACT_DRIFT)
    );
}

/// ACP 片段单独出现不得让账本动作进入原生派发。
#[tokio::test]
async fn acp_only_contract_fails_ledger_action_without_native_dispatch() {
    use multi_agents_manager_lib::services::dsh_control::actions::{
        NativeDispatch, NativeSendOutcome,
    };
    use multi_agents_manager_lib::services::dsh_control::model::{
        ActionPayload, ActionRequest, ActionState, DeviceContext, SendPayload, Target,
    };
    use multi_agents_manager_lib::services::dsh_control::{DshControlHub, DshDb};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    struct CountingNative(AtomicUsize);
    impl NativeDispatch for CountingNative {
        fn send_message<'a>(
            &'a self,
            _: &'a Target,
            _: &'a SendPayload,
            _: &'a str,
        ) -> futures::future::BoxFuture<'a, Result<NativeSendOutcome, DshError>> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(NativeSendOutcome::Accepted { native_id: None })
            })
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    let contents = tmp.path().join("DSH.app/Contents");
    std::fs::create_dir_all(contents.join("MacOS")).unwrap();
    std::fs::create_dir_all(contents.join("Resources")).unwrap();
    let executable = contents.join("MacOS/exec");
    std::fs::write(&executable, b"fixture").unwrap();
    std::fs::write(contents.join("Resources/app.asar"), b"fixture").unwrap();
    let descriptor = r#"const $remote = createTypertRemoteInfo({methods:{'session/prompt':{service:'sessionController',jsonSchema:{"properties":{"sessionId":{"type":"string"},"prompt":{"type":"array"}},"required":["sessionId","prompt"]}}}});"#;
    let bytes = asar(&[(SESSION_PATH, descriptor)]);
    let mut local = host();
    local.executable = executable;
    let probe_host = local.clone();
    let read = Arc::new(move |host: &LocalHost| {
        read_report_with(
            host,
            CONTRACT_RULE_VERSION,
            &FakePackage::new(bytes.clone()),
            &ContractBudget::DEFAULT,
        )
    });
    let db = rusqlite::Connection::open_in_memory().unwrap();
    multi_agents_manager_lib::database::schema::init(&db).unwrap();
    let native = Arc::new(CountingNative(AtomicUsize::new(0)));
    let hub = DshControlHub::with_contract_read(
        DshDb::Owned(Arc::new(Mutex::new(db))),
        native.clone(),
        Arc::new(move || Ok(probe_host.clone())),
        read,
        None,
    );
    hub.open_write_gate();
    let device = DeviceContext {
        device_id: "dev".into(),
        device_name: "Dev".into(),
    };
    let request = ActionRequest {
        target: Target {
            host_id: local.host_id,
            epoch: local.epoch,
            session_id: "s".into(),
        },
        action_id: "acp-action".into(),
        expected_revision: None,
        payload: ActionPayload::Send(SendPayload {
            text: "保留草稿".into(),
            receipt_ids: vec![],
            queue_only: false,
            bypass_paused_queue: false,
        }),
    };
    let receipt = hub
        .dispatch(device.clone(), request.clone())
        .await
        .expect("账本回执");
    assert_eq!(native.0.load(Ordering::SeqCst), 0, "契约失配时不得派发");
    assert_eq!(receipt.state, ActionState::Failed);
    assert_eq!(
        receipt.reason_code.as_deref(),
        Some("dsh_capability_denied:operation_contract_unmatched")
    );
    let replay = hub
        .dispatch(device.clone(), request.clone())
        .await
        .expect("同 id 重放");
    assert_eq!(replay, receipt);
    let replay2 = hub
        .replay_session_send(
            device,
            "s",
            "acp-action",
            match request.payload {
                ActionPayload::Send(payload) => payload,
                _ => unreachable!(),
            },
            None,
        )
        .await
        .expect("载荷核验");
    assert_eq!(replay2, Some(receipt));
    assert_eq!(native.0.load(Ordering::SeqCst), 0);
}

// ============================================================
// P1-1：Send 契约核心输入形状判据（session/prompt 逐字段漂移只关受影响子操作）
// ============================================================

/// `session/prompt` 方法块在 fixture 内的边界（起于块锚点、止于下一方法锚点）。
fn prompt_block_bounds(text: &str) -> (usize, usize) {
    let start = text
        .find("    'session/prompt': {")
        .expect("fixture prompt block");
    let end = text[start..]
        .find("    'session/modelCatalog': {")
        .map(|offset| start + offset)
        .expect("next block");
    (start, end)
}

/// prompt 块内指定片段漂移（`from`→`to`，替换块内全部出现；前提 = 块内出现
/// 次数恰 `expected`，防片段分布变化时漂错位置）。真实包 prompt 请求 schema
/// 有 codec/strictCodec 两面（同型双份），fixture 只保留一面。
fn prompt_fragment_drifted(from: &str, to: &str, expected: usize) -> &'static str {
    let (start, end) = prompt_block_bounds(SESSION_JS);
    let block = &SESSION_JS[start..end];
    assert_eq!(
        block.matches(from).count(),
        expected,
        "前提：prompt 块内 {from:?} 出现次数应为 {expected}"
    );
    Box::leak(
        format!(
            "{}{}{}",
            &SESSION_JS[..start],
            block.replace(from, to),
            &SESSION_JS[end..]
        )
        .into_boxed_str(),
    )
}

/// P1-1 / RED：prompt 请求 schema 的核心输入 `sessionId` 类型漂移（string→number）
/// —— 旧规则对 `required` 只锁字段名不锁类型，漂移后 Send 仍判「契约匹配」
/// （RED 已录：对旧实现此断言 FAILED）；补类型判据后 Send 必须失配，且只关
/// Send：FileUpload 的规则锚 uploadFileBinary/receiptId 与 prompt 请求形状独立，
/// 不得受牵连。
#[test]
fn send_core_input_shape_drift_is_detected() {
    let drifted = prompt_fragment_drifted(
        r#""sessionId":{"type":"string""#,
        r#""sessionId":{"type":"number""#,
        1,
    );
    let evidence = read_ok_package(asar(&with_replaced(SESSION_PATH, drifted)));
    assert!(
        !evidence.matches.contains_key(&Operation::Send),
        "sessionId 类型漂移后 Send 不得再匹配"
    );
    assert!(
        evidence.matches.contains_key(&Operation::FileUpload),
        "sessionId 漂移只关 Send，FileUpload（锚 uploadFileBinary/receiptId）不受影响"
    );
    assert!(
        evidence.matches.contains_key(&Operation::CancelCurrent),
        "同文件无关方法不受影响"
    );
    let drift = evaluate(&context_from_evidence(evidence));
    assert!(!drift.allowed(Operation::Send));
    assert!(
        drift.allowed(Operation::FileUpload),
        "FileUpload 子操作隔离"
    );
}

/// P1-1：核心输入 `requestId` 类型漂移（string→number）→ 只关 Send；
/// FileUpload（锚 receiptId/upload 路由）与同文件无关方法不受影响。
#[test]
fn send_request_id_drift_only_closes_send() {
    let drifted = prompt_fragment_drifted(
        r#""requestId":{"type":"string""#,
        r#""requestId":{"type":"number""#,
        1,
    );
    let evidence = read_ok_package(asar(&with_replaced(SESSION_PATH, drifted)));
    assert!(
        !evidence.matches.contains_key(&Operation::Send),
        "requestId 类型漂移后 Send 不得再匹配"
    );
    assert!(
        evidence.matches.contains_key(&Operation::FileUpload),
        "requestId 漂移只关 Send，FileUpload 不受影响"
    );
    assert!(
        evidence.matches.contains_key(&Operation::CancelCurrent),
        "同文件无关方法不受影响"
    );
    let drift = evaluate(&context_from_evidence(evidence));
    assert!(!drift.allowed(Operation::Send));
    assert_eq!(
        status_reason(&drift, Operation::Send).as_deref(),
        Some(reason::CONTRACT_UNMATCHED)
    );
    assert!(
        drift.allowed(Operation::FileUpload),
        "FileUpload 子操作隔离"
    );
}

/// P1-1：核心输入 `content` 容器漂移（array→object）→ 只关 Send；
/// FileUpload/ImageUpload 的 content 变体锚（const file/image、receiptId、
/// mediaType）仍在，不受请求容器漂移连坐。
#[test]
fn send_content_container_drift_only_closes_send() {
    let drifted = prompt_fragment_drifted(
        r#""content":{"type":"array""#,
        r#""content":{"type":"object""#,
        1,
    );
    let evidence = read_ok_package(asar(&with_replaced(SESSION_PATH, drifted)));
    assert!(
        !evidence.matches.contains_key(&Operation::Send),
        "content 容器漂移后 Send 不得再匹配"
    );
    assert!(
        evidence.matches.contains_key(&Operation::FileUpload),
        "content 漂移只关 Send，FileUpload 不受影响"
    );
    assert!(
        evidence.matches.contains_key(&Operation::ImageUpload),
        "ImageUpload（锚 mediaType/imageLimits）同样不受影响"
    );
    let drift = evaluate(&context_from_evidence(evidence));
    assert!(!drift.allowed(Operation::Send));
    assert_eq!(
        status_reason(&drift, Operation::Send).as_deref(),
        Some(reason::CONTRACT_UNMATCHED)
    );
    assert!(
        drift.allowed(Operation::FileUpload),
        "FileUpload 子操作隔离"
    );
    assert!(
        drift.allowed(Operation::ImageUpload),
        "ImageUpload 子操作隔离"
    );
}

/// P1-1 复审 B-1 / RED：text part 的**文本载荷**形状漂移（`"text":{"type":"string"}`
/// → number）→ Send 必须失配。旧规则只有 `"const":"text"`（只锁判别值），载荷字段
/// 形状漂移后仍匹配（复审者真实包实证；本用例对 rule=3 规则表应 FAILED）。
#[test]
fn send_text_payload_shape_drift_is_detected() {
    let drifted = prompt_fragment_drifted(
        r#""text":{"type":"string"}"#,
        r#""text":{"type":"number"}"#,
        1,
    );
    let evidence = read_ok_package(asar(&with_replaced(SESSION_PATH, drifted)));
    assert!(
        !evidence.matches.contains_key(&Operation::Send),
        "文本载荷类型漂移后 Send 不得再匹配"
    );
    assert!(
        evidence.matches.contains_key(&Operation::FileUpload),
        "只关 Send，FileUpload 不受影响"
    );
    assert!(
        evidence.matches.contains_key(&Operation::ImageUpload),
        "只关 Send，ImageUpload 不受影响"
    );
}

/// P1-1 复审 B-1：text part 的 `required` 列表丢字段（`["type","text"]` → `["type"]`）
/// → 发送契约的文本字段归属不再成立，Send 必须失配。
#[test]
fn send_text_part_required_drift_is_detected() {
    let drifted = prompt_fragment_drifted(
        r#""required":["type","text"]}"#,
        r#""required":["type"],"x":1}"#,
        1,
    );
    let evidence = read_ok_package(asar(&with_replaced(SESSION_PATH, drifted)));
    assert!(
        !evidence.matches.contains_key(&Operation::Send),
        "text part required 漂移后 Send 不得再匹配"
    );
}

/// P1-1 复审 M-1（锁定已知边界）：真实包 prompt schema 有 codec/strictCodec 两面，
/// 规则片段在**任一面**命中即满足——单面漂移不被检测。本用例在 prompt 方法块内
/// 插入第二份 schema（模拟两面），只漂第一面的 text 载荷、第二面原样，锁定 Send
/// 仍匹配的实际行为并如实申报。若要关闭单面漂移，需 Window 支持面级窗口（另立
/// 小项，见规则注释）。
#[test]
fn single_face_drift_is_an_acknowledged_boundary() {
    let (start, end) = prompt_block_bounds(SESSION_JS);
    let block = &SESSION_JS[start..end];
    let schema_line = block
        .lines()
        .find(|line| line.trim_start().starts_with("parameters:"))
        .expect("fixture prompt schema line");
    // 第二份 schema（模拟 strictCodec 面）插在 result 行**之前**、原样保留
    // `result:` 键；`schema_line` 自带尾逗号，插入时**不再追加**逗号（复审
    // Minor-4：旧构造会拼出 `},,` 且丢掉 result 键，夹具不是合法 JS）。
    let with_two_faces = block.replacen(
        "      result:",
        &format!(
            "      parametersStrict: {}\n      result:",
            schema_line.trim_start().trim_start_matches("parameters: ")
        ),
        1,
    );
    assert_eq!(
        with_two_faces
            .matches(r#""text":{"type":"string"}"#)
            .count(),
        2,
        "双面夹具必须真的含两份文本载荷片段（防构造退化）"
    );
    let two_faces: &'static str = Box::leak(
        format!(
            "{}{}{}",
            &SESSION_JS[..start],
            with_two_faces,
            &SESSION_JS[end..]
        )
        .into_boxed_str(),
    );
    // 只漂第一面的 text 载荷（第一处出现仍在 face1 内；face2 保留原片段）。
    let first = two_faces
        .find(r#""text":{"type":"string"}"#)
        .expect("first face text payload");
    let mut drifted = String::new();
    drifted.push_str(&two_faces[..first]);
    drifted.push_str(r#""text":{"type":"number"}"#);
    drifted.push_str(&two_faces[first + r#""text":{"type":"string"}"#.len()..]);
    let drifted: &'static str = Box::leak(drifted.into_boxed_str());

    let evidence = read_ok_package(asar(&with_replaced(SESSION_PATH, drifted)));
    // 已知边界：第二面仍满足 → Send 匹配。如实锁定，防止未来误以为已能检测单面漂移。
    assert!(
        evidence.matches.contains_key(&Operation::Send),
        "已知边界：单面漂移不被检测（任一面保持契约即可兼容尝试）；若此处失败说明已引入面级窗口，请同步更新本用例与规则注释"
    );
}

/// P1-1 复审 Minor-1：text part **判别属性自身**的声明类型漂移
/// （`"type":{"type":"string","const":"text"` → number）→ Send 必须失配。
/// 旧规则只有 `"const":"text"`（只锁判别值），判别属性类型漂移后仍匹配。
#[test]
fn send_text_part_discriminator_type_drift_is_detected() {
    let drifted = prompt_fragment_drifted(
        r#""type":{"type":"string","const":"text""#,
        r#""type":{"type":"number","const":"text""#,
        1,
    );
    let evidence = read_ok_package(asar(&with_replaced(SESSION_PATH, drifted)));
    assert!(
        !evidence.matches.contains_key(&Operation::Send),
        "判别属性类型漂移后 Send 不得再匹配"
    );
    assert!(
        evidence.matches.contains_key(&Operation::FileUpload),
        "只关 Send，FileUpload 不受影响"
    );
    assert!(
        evidence.matches.contains_key(&Operation::ImageUpload),
        "只关 Send，ImageUpload 不受影响"
    );
}
