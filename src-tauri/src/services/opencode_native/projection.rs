//! 消息、pending 与 inbox 的权威只读投影（计划任务 4）。
//!
//! 纪律：
//!
//! * **只读**：本模块只调用 [`NativeClient::read_json`] 与
//!   [`NativeClient::read_events`]；任何写能力都在后续任务里由动作账本持有，这里连
//!   `write_once` 都不引用。投影层自己**不建** `reqwest::Client`——唯一 HTTP 通道是
//!   任务 3 的 [`NativeClient`]。
//! * **代际优先**：每次投影都重读登记文件；`RuntimeKey` 变化即新代际：旧连接作废、
//!   旧 SSE 停止、投影缓存清空。一个代际最多一条 SSE 上游，事件只触发**只读重建**。
//! * **证据分级**：分页读只有走到游标耗尽才算完整；部分历史可以展示，但
//!   [`NativeHub::delivery_evidence`] 为 false，调用方不得用「列表里没有某 id」证明
//!   未投递。pending 读失败或回执目录不是本会话时，返回受控错误，**不**降级成空列表。
//! * **token 只随身份/内容变化**：`binding_token` 锚定服务代际 + 会话 id + 归一目录；
//!   `context_token` 再锚定该项内容（表单 `content_hash`）。消息增长不轮换，
//!   回答过程中新消息不会把 token 作废。
//! * **生命周期**：`NativeHub::new` 只是惰性容器（不发现、不连接、不启任务）；
//!   SSE 与缓存只在有订阅者时存活，最后一个订阅者离开后释放；`shutdown` 停止上游并
//!   关闭所有订阅频道。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::remote::attachments::attachment_dir;
use crate::remote::content::SessionMessage;
use crate::remote::pairing::DeviceStore;
use crate::session::Session;

use super::actions::NativeActionStore;
use super::bridge::{BridgePaths, BridgeRuntime, BridgeStatus};
use super::client::{NativeClient, MAX_RESPONSE_BYTES};
use super::contract::{evaluate_contract, parse_form};
use super::discovery::{bind_session, discover};
use super::types::{
    codes, Capabilities, DeliveryKind, DiscoveryInput, FieldKind, FormAnswerValue, FormField,
    FormSpec, InboxView, NativeAgentOption, NativeAgentView, NativeError, NativeFileView,
    NativeFormAnswerView, NativeFormResult, NativeOperationId, NativeTarget, PendingView,
    PublicNativeView, RuntimeKey, ServiceDescriptor,
};

/// 桥 start/stop 共用的进程内串行锁（局部 static 会各自独立，不能互斥）。
static BRIDGE_LIFECYCLE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 服务信息（代际互证用）。
const INFO_PATH: &str = "/api/info";

/// C1：对**实际写通道**重新执行完整服务身份核对（pid/version/urls 与代际锚互证）。
///
/// 快照时的核对与 claim 之间隔着会话锁等待与操作特有读取——期间同端口接收方可能
/// 已被换成冒名服务（登记与原 PID 事实仍可读，共享 DB 使后续会话读取也可能成功），
/// 只有写通道自己的 `/api/info` 能证伪。在全部异步等待与操作特有读取结束之后、
/// claim 之前调用；读不到身份即拒绝（fail-closed，零写出）。
pub(crate) async fn reverify_write_channel_identity(source: &Source) -> Result<(), NativeError> {
    let info = source.client.read_json(INFO_PATH, &[]).await?;
    super::discovery::verify_service_info(&info, &source.descriptor)
}
/// 契约（能力判定）来源；每个代际只读一次。
const OPENAPI_PATH: &str = "/openapi.json";
/// 全局 pending 集合端点：必须带 `location[directory]`（任务 0 回填第 6 条）。
const GLOBAL_FORM_PATH: &str = "/api/form";
const GLOBAL_PERMISSION_PATH: &str = "/api/permission/request";
/// 作用域参数是 deepObject，裸 `location=` 任务 0 已证为 400。
const SCOPE_QUERY_KEY: &str = "location[directory]";
/// 模型清单（只读展示名解析；不参与任何写入/能力判定）。
const MODEL_LIST_PATH: &str = "/api/model";
/// 单页响应超限时的降档阶梯：同一 cursor 依次折半 20→10→5→2→1（R3）。
const MESSAGE_LIMIT_LADDER: [usize; 5] = [20, 10, 5, 2, 1];
/// 单页消息条数（服务端 `limit`，任务 0 已证分页参数）：取阶梯首档，避免两处常量漂移。
const MESSAGE_LIMIT: usize = MESSAGE_LIMIT_LADDER[0];
/// 单次投影最多翻的页数：20 页 × 20 条 = 400 条，超过即按「部分历史」处理。
const MAX_MESSAGE_PAGES: usize = 20;
/// 单次投影最多发出的页请求数：成功页预算 × 阶梯长度。降档重试不占页预算，但总量
/// 有界，不会因为服务端持续回超限或游标异常而无限重试。
const MAX_MESSAGE_PAGE_ATTEMPTS: usize = MAX_MESSAGE_PAGES * MESSAGE_LIMIT_LADDER.len();
/// 无订阅时的释放检查周期。
const REAP_INTERVAL: Duration = Duration::from_millis(100);
/// 上游收尾等待上限（只用于让连接干净关闭，不影响任何读预算）。
const PUMP_DRAIN: Duration = Duration::from_millis(500);

/// 与 actions::is_agent_id 同形态；列表展示和实际可切换目标用同一判据。
fn agent_id_shape_ok(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn contract_missing(message: &str) -> NativeError {
    NativeError::new(codes::OC_CONTRACT_MISSING, message)
}

fn stopped() -> NativeError {
    NativeError::new(codes::OC_TRANSPORT_FAILED, "opencode native hub is stopped")
}

/// 监听代际已前进（pause/shutdown 之后）：本次投影属于旧监听，不得发布到新监听。
fn detached() -> NativeError {
    NativeError::new(
        codes::OC_TRANSPORT_FAILED,
        "opencode native listener generation changed",
    )
}

fn scope_mismatch() -> NativeError {
    NativeError::new(
        codes::OC_DIRECTORY_MISMATCH,
        "native pending scope does not match the bound directory",
    )
}

/// 生产发现读源：只读给定登记文件（不探其它路径、不起服务）。
fn read_state_file(path: &Path) -> Result<Vec<u8>, NativeError> {
    std::fs::read(path).map_err(|_| {
        NativeError::new(
            codes::OC_SERVICE_NOT_REGISTERED,
            "opencode background service is not registered",
        )
    })
}

/// 进程启动秒 → 代际毫秒锚：只做单位换算，不掺墙钟或 uptime。
///
/// `start_time()` 是「进程启动时刻距 UNIX_EPOCH 的整秒数」，同一进程每次读都相同，
/// 因此同一代际的 [`RuntimeKey`] / binding token 也恒定。0/不可用即 `None`
/// （发现侧 fail-closed 为 `OC_RUNTIME_UNVERIFIED`），绝不用当前时间顶替。
fn started_at_ms_from_started_secs(started_secs: u64) -> Option<i64> {
    let started_secs = i64::try_from(started_secs).ok()?;
    if started_secs <= 0 {
        return None;
    }
    started_secs.checked_mul(1000)
}

/// 生产进程事实：pid 的启动时刻（epoch 毫秒）。拿不到即返回 `None`（发现侧 fail-closed）。
fn production_process_fact(pid: u32) -> Option<i64> {
    let pid = sysinfo::Pid::from_u32(pid);
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    let process = system.process(pid)?;
    started_at_ms_from_started_secs(process.start_time())
}

/// G3/C6 三形态代际事实：pid → [`ProcessFactProbe`]。
pub(crate) type GenerationFact = Arc<dyn Fn(u32) -> super::actions::ProcessFactProbe + Send + Sync>;

/// H2：单一事实机制对一个 pid 的一次**可区分**观测（生产探针的源级注入缝形态）。
///
/// 「可区分」= 观测自己声明属于哪一类：在场（是否可证启动时刻）、明确缺席、
/// 机制不可用。死亡判定只消费**明确缺席**；其余形态一律 fail-closed。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FactSighting {
    /// 在场且启动时刻可读（epoch 毫秒；只有 sysinfo 源能给）。
    Present(i64),
    /// 在场已正面证明但启动时刻不可证（sysinfo `start_time` = 0；libproc 读到
    /// 路径或 `EPERM`——后者是「进程存在但路径不可读」，存在性已被正面证明）。
    PresentUnprovenAge,
    /// 机制健康且明确报告无此 pid（正向缺席证据：sysinfo = 健康刷新下的 None；
    /// libproc = `ESRCH`）。
    Absent,
    /// 机制对该 pid 无法给出可区分回答（未识别错误/机制级失败）。
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    Unavailable,
}

/// 健康探针的「必真在场」只认正面在场形态（缺席/不可用都不算机制健康）。
fn fact_sighting_is_presence(sighting: FactSighting) -> bool {
    matches!(
        sighting,
        FactSighting::Present(_) | FactSighting::PresentUnprovenAge
    )
}

/// H2 生产三形态判定本体（源级注入缝）：目标 pid 的三形态由事实机制的观测组合。
///
/// * 主源正面报告在场 → [`ProcessFactProbe::Alive`]（lstart 可读）或
///   `ReadFailure`（在场但启动时刻不可证）——在场是安全方向，不咨询死亡机制；
/// * 主源报告目标缺席时，「缺席」只是**候选死亡**。双源形态（`crosscheck` =
///   `Some`，macOS 生产）要求**四证据齐备**才升级为
///   [`ProcessFactProbe::NoSuchProcess`]：主源目标缺席、交叉机制独立确认缺席、
///   两机制各自的自身健康探针都正面在场——单源缺席、交叉矛盾、任一机制健康
///   不可证一律 `ReadFailure`（能读取自身进程或服务登记，不能证明另一个
///   pid 已退出）；
/// * 单源形态（`crosscheck` = `None`，非 macOS 生产保持 G 轮既有语义）：主源
///   自身健康探针在场 + 目标缺席 ⇒ `NoSuchProcess`，机制健康也不可证 ⇒
///   `ReadFailure`（零收紧零放宽）。
fn production_generation_fact_with(
    target: u32,
    sysinfo_source: &dyn Fn(u32) -> FactSighting,
    crosscheck: Option<&dyn Fn(u32) -> FactSighting>,
) -> super::actions::ProcessFactProbe {
    use super::actions::ProcessFactProbe;
    let myself = std::process::id();
    match sysinfo_source(target) {
        FactSighting::Present(started) => ProcessFactProbe::Alive(started),
        // 在场但启动时刻不可证 / 目标观测不可用：死亡不可证（fail-closed）。
        FactSighting::PresentUnprovenAge | FactSighting::Unavailable => {
            ProcessFactProbe::ReadFailure
        }
        FactSighting::Absent => {
            let probe_healthy = fact_sighting_is_presence(sysinfo_source(myself));
            match crosscheck {
                None => {
                    if probe_healthy {
                        ProcessFactProbe::NoSuchProcess
                    } else {
                        ProcessFactProbe::ReadFailure
                    }
                }
                Some(crosscheck) => {
                    let cross_confirms = crosscheck(target) == FactSighting::Absent;
                    let cross_healthy = fact_sighting_is_presence(crosscheck(myself));
                    if probe_healthy && cross_healthy && cross_confirms {
                        ProcessFactProbe::NoSuchProcess
                    } else {
                        ProcessFactProbe::ReadFailure
                    }
                }
            }
        }
    }
}

/// H2 源 A：sysinfo 单 pid 刷新（macOS 底层 = sysctl `KERN_PROC_PID`）。
/// 目标缺席（None）与机制级局部失效在此源**不可区分**——这正是缺席判据必须
/// 交叉确认的原因；机制健康由判定本体另行探测（自身 pid 必真在场）。
fn sysinfo_sighting(pid: u32) -> FactSighting {
    let pid = sysinfo::Pid::from_u32(pid);
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    match system.process(pid) {
        Some(process) => started_at_ms_from_started_secs(process.start_time())
            .map(FactSighting::Present)
            .unwrap_or(FactSighting::PresentUnprovenAge),
        None => FactSighting::Absent,
    }
}

/// H2 源 B（macOS）：libproc `proc_pidpath`（libSystem，与 sysinfo 的 sysctl
/// 路径相互独立）。errno 语义自带可区分回答：返回 >0 = 在场且路径可读；
/// `ESRCH` = **明确无此 pid**（正向缺席证据）；`EPERM` = **在场**但路径不可读
/// （他用户进程，存在性已被正面证明）；其余 = 机制不可用。
#[cfg(target_os = "macos")]
fn proc_pidpath_sighting(pid: u32) -> FactSighting {
    const BUFFER_SIZE: u32 = 4096;
    let Ok(pid) = libc::c_int::try_from(pid) else {
        return FactSighting::Unavailable;
    };
    let mut buffer = vec![0i8; BUFFER_SIZE as usize];
    let written =
        unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr() as *mut libc::c_void, BUFFER_SIZE) };
    if written > 0 {
        return FactSighting::PresentUnprovenAge;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => FactSighting::Absent,
        Some(libc::EPERM) => FactSighting::PresentUnprovenAge,
        _ => FactSighting::Unavailable,
    }
}

/// 生产三形态进程事实（G3 建形、H2 深化判据）。
///
/// macOS 生产 = **双源交叉**：sysinfo（源 A）+ libproc `proc_pidpath`（源 B）。
/// 目标 pid 的「缺席」只有在两个相互独立的内核机制一致报告缺席、且两机制各自
/// 的自身健康探针都正面在场时，才构成正向死亡证明（`NoSuchProcess`）；其余一切
/// 形态（正面在场 / 读取失败 / 单源缺席 / 双源矛盾 / 机制不健康）fail-closed 为
/// `ReadFailure`。非 macOS 生产保持 G 轮单源形态（自身健康探针对冲机制级失败），
/// 零行为变化。
fn production_generation_fact(pid: u32) -> super::actions::ProcessFactProbe {
    #[cfg(target_os = "macos")]
    let crosscheck: Option<&dyn Fn(u32) -> FactSighting> = Some(&proc_pidpath_sighting);
    #[cfg(not(target_os = "macos"))]
    let crosscheck: Option<&dyn Fn(u32) -> FactSighting> = None;
    production_generation_fact_with(pid, &sysinfo_sighting, crosscheck)
}

/// 两态事实 → 三形态事实的保守适配（注入构造的默认桥）：`Some(lstart)` ⇒
/// `Alive`；`None` ⇒ `ReadFailure`——两态缝无法区分「明确缺席 vs 通道失败」，
/// 一律按**读取失败**处理（G3 fail-closed：不做超出缝语义的猜测）。需要表达
/// 「明确无此 pid」的用例走 [`NativeHub::with_coordinator_facts`] 注入三形态缝。
fn generation_fact_from_two_state(fact: ProcessFact) -> GenerationFact {
    Arc::new(move |pid: u32| match fact(pid) {
        Some(started) => super::actions::ProcessFactProbe::Alive(started),
        None => super::actions::ProcessFactProbe::ReadFailure,
    })
}

/// 一个原生消息页 → 统一消息列表。
///
/// `seq` 是**本数组内**的稳定序号（去重之后才生成），不是跨请求身份；跨请求身份只有
/// 原生 `id`。`user` 的两种形态（顶层 `text` / `payload.text`）都必须保留。
pub fn project_messages(page: &Value) -> Result<Vec<SessionMessage>, NativeError> {
    let items = page
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| contract_missing("native message page has no data array"))?;
    project_records(items)
}

fn project_records(records: &[Value]) -> Result<Vec<SessionMessage>, NativeError> {
    Ok(project_records_with_seq(records)?.messages)
}

/// 一次消息投影 + 每条记录产生的**第一条**消息在消息数组里的下标。
///
/// 附件按消息 `seq` 关联：user 记录恰好产出一条消息，assistant 记录可能产出多条
/// （text/thinking/tool-call/tool-result）。保存记录起点后，附件映射不需要再猜消息
/// 与记录的对应关系。
struct MessageProjection {
    message_ids: BTreeMap<i64, String>,
    messages: Vec<SessionMessage>,
    /// 投影下标 → 该条 `question` 调用的正式答案（题目 + 答案文本）。
    question_answers: BTreeMap<i64, Vec<NativeFormAnswerView>>,
    /// 投影下标 → 该条 tool-call 的原生调用 id（`block.id`）。
    ///
    /// 表单结果的原生关联同时带 `messageID` 与调用 `id`：只有两者都对上才算同一组题。
    /// 同一消息里可能有多组提问，仅凭 messageID 无法区分——所以调用 id 必须透出。
    tool_call_ids: BTreeMap<i64, String>,
    /// 与 `records` 一一对应；`None` = 该记录不是消息（idle 等）。
    starts: Vec<Option<usize>>,
}

fn project_records_with_seq(records: &[Value]) -> Result<MessageProjection, NativeError> {
    let mut messages: Vec<SessionMessage> = Vec::new();
    let mut message_ids = BTreeMap::new();
    let mut tool_call_ids: BTreeMap<i64, String> = BTreeMap::new();
    let mut question_answers: BTreeMap<i64, Vec<NativeFormAnswerView>> = BTreeMap::new();
    let mut starts: Vec<Option<usize>> = Vec::with_capacity(records.len());
    for record in records {
        let start = messages.len();
        match record.get("type").and_then(Value::as_str) {
            Some("user") => {
                messages.push(SessionMessage {
                    seq: 0,
                    role: "user".to_string(),
                    kind: "user".to_string(),
                    // 只有附件、没有正文的用户消息也保留（不丢记录），正文为空。
                    content: text_of(record).unwrap_or_default().to_string(),
                    ts: record.pointer("/time/created").and_then(Value::as_i64),
                    tool_name: None,
                    tool_args: None,
                    collapsed: false,
                });
            }
            Some("assistant") => {
                let ts = record.pointer("/time/created").and_then(Value::as_i64);
                project_assistant(
                    record,
                    ts,
                    &mut messages,
                    &mut tool_call_ids,
                    &mut question_answers,
                );
            }
            // `synthetic` = OpenCode 的系统注入记录（切 agent 时的模式提醒等，
            // 正文形如 `<system-reminder> You are in Plan mode … </system-reminder>`）。
            // 它不是用户消息、也不进待发队列（见 `read_inbox`），但属于会话可见历史的
            // 一部分：以默认折叠的过程消息（`kind="system-note"`）保留在正文区，
            // 包裹标签剥掉，只把提醒正文交给前端。没有正文的记录不产空消息。
            Some("synthetic") => {
                if let Some(text) = text_of(record) {
                    let body = strip_system_reminder_tags(text);
                    if !body.is_empty() {
                        messages.push(SessionMessage {
                            seq: 0,
                            role: "assistant".to_string(),
                            kind: "system-note".to_string(),
                            content: body,
                            ts: record.pointer("/time/created").and_then(Value::as_i64),
                            tool_name: None,
                            tool_args: None,
                            collapsed: true,
                        });
                    }
                }
            }
            // `idle`（回合终态）、`agent-switched` 等是记录/状态，不是消息：只进 runtime_state。
            _ => {}
        }
        if let Some(id) = record
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            for index in start..messages.len() {
                message_ids.insert(index as i64, id.to_string());
            }
        }
        starts.push((messages.len() > start).then_some(start));
    }
    for (index, message) in messages.iter_mut().enumerate() {
        message.seq = index as i64;
    }
    Ok(MessageProjection {
        messages,
        starts,
        message_ids,
        tool_call_ids,
        question_answers,
    })
}

/// 附件名缺失时的受控占位：条目仍然展示，不因缺名被丢掉。
const ATTACHMENT_PLACEHOLDER: &str = "attachment";

/// 原生 user 消息的附件 → 按消息 `seq` 关联的最小展示视图（R5c）。
///
/// 只解析元数据；`data` 仅用于估算大小（不保留、不下发）。`preview_path` 只对
/// **canonical 后仍位于该会话 `.tuvis-attachments/<session_id>/` 内**的 `file://`
/// 产生；其他 URI（`data:`、`http(s):`、裸路径、越界路径）只给名称/类型/大小。
/// 任何异常形态（`files` 不是数组、条目不是对象、字段类型不对）都只跳过该条/该段，
/// 绝不让整份投影失败。
fn project_attachments(
    records: &[Value],
    projection: &MessageProjection,
    target: Option<&NativeTarget>,
) -> BTreeMap<i64, Vec<NativeFileView>> {
    // 受控根每次投影只归一一次；不存在/不可读 ⇒ 任何 URI 都不给 previewPath。
    let root = target.and_then(|target| {
        attachment_dir(&target.directory, &target.session_id)
            .canonicalize()
            .ok()
    });
    let mut files = BTreeMap::new();
    for (record, start) in records.iter().zip(&projection.starts) {
        // 附件只属于 user 消息（顶层 `files` 与 `payload.files` 两种形态）。
        if record.get("type").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let Some(start) = start else {
            continue;
        };
        let views = attachment_views(record, root.as_deref());
        if !views.is_empty() {
            files.insert(*start as i64, views);
        }
    }
    files
}

/// 顶层 `files` 优先，退回 `payload.files`（读回是顶层、写应答在 payload）。
/// 顶层存在但为空数组时仍看 payload：空数组不携带任何附件，不应遮住另一形态。
fn attachment_entries(record: &Value) -> Option<&[Value]> {
    record
        .get("files")
        .and_then(Value::as_array)
        .filter(|files| !files.is_empty())
        .or_else(|| record.pointer("/payload/files").and_then(Value::as_array))
        .map(Vec::as_slice)
}

fn attachment_views(record: &Value, controlled_root: Option<&Path>) -> Vec<NativeFileView> {
    attachment_entries(record)
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| attachment_view(entry, controlled_root))
        .collect()
}

fn attachment_view(entry: &Value, controlled_root: Option<&Path>) -> Option<NativeFileView> {
    // 非对象不是文件条目：跳过，不伪造一条「attachment」。
    let object = entry.as_object()?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| ATTACHMENT_PLACEHOLDER.to_string());
    let mime = object
        .get("mime")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|mime| !mime.is_empty())
        .map(str::to_string);
    // 只按 base64 字符数估算，不 decode、不保留内容。
    let size = object
        .get("data")
        .and_then(Value::as_str)
        .and_then(approximate_base64_bytes);
    let preview_path = object
        .get("uri")
        .and_then(Value::as_str)
        .or_else(|| {
            object
                .get("source")
                .and_then(|source| source.get("uri"))
                .and_then(Value::as_str)
        })
        .and_then(|uri| controlled_file_path(uri, controlled_root));
    Some(NativeFileView {
        name,
        mime,
        size,
        preview_path,
    })
}

/// base64 近似原始字节数：`长度 ×3/4`，尾部 `=` 不计（最多 2 个）。
fn approximate_base64_bytes(data: &str) -> Option<u64> {
    let trimmed = data.trim();
    if trimmed.is_empty() {
        return None;
    }
    let padding = trimmed
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'=')
        .count()
        .min(2) as u64;
    Some(
        (trimmed.len() as u64)
            .saturating_mul(3)
            .saturating_div(4)
            .saturating_sub(padding),
    )
}

/// `file://` URI → 受控预览路径：只有 canonical 后位于会话附件目录内且确实是文件
/// 才返回；其余一律 None（不信任任意本机 URI，也不把越界路径下发给手机）。
fn controlled_file_path(uri: &str, controlled_root: Option<&Path>) -> Option<String> {
    let root = controlled_root?;
    let url = reqwest::Url::parse(uri.trim()).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    let path = url.to_file_path().ok()?;
    let canonical = path.canonicalize().ok()?;
    if !canonical.starts_with(root) || !canonical.is_file() {
        return None;
    }
    Some(canonical.to_string_lossy().into_owned())
}

fn project_assistant(
    record: &Value,
    ts: Option<i64>,
    messages: &mut Vec<SessionMessage>,
    tool_call_ids: &mut BTreeMap<i64, String>,
    question_answers: &mut BTreeMap<i64, Vec<NativeFormAnswerView>>,
) {
    let Some(blocks) = record.get("content").and_then(Value::as_array) else {
        // 没有分块时退回顶层 `text`（与 user 侧同一条「不丢已确认内容」纪律）。
        if let Some(text) = text_of(record) {
            messages.push(assistant_text(text, ts));
        }
        return;
    };
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let Some(text) = block.get("text").and_then(Value::as_str) else {
                    continue;
                };
                messages.push(assistant_text(text, ts));
            }
            Some("reasoning") => {
                let Some(text) = block.get("text").and_then(Value::as_str) else {
                    continue;
                };
                messages.push(SessionMessage {
                    seq: 0,
                    role: "assistant".to_string(),
                    kind: "thinking".to_string(),
                    content: text.to_string(),
                    ts,
                    tool_name: None,
                    tool_args: None,
                    collapsed: true,
                });
            }
            Some("tool") => {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let tool_args = block.pointer("/state/input").map(|input| input.to_string());
                let content = match block
                    .pointer("/state/error/message")
                    .and_then(Value::as_str)
                {
                    Some(error) => format!("{name}: {error}"),
                    None => name.clone(),
                };
                // 调用 id 与**这一条** tool-call 的投影下标绑定：必须在 push 之前取索引。
                // （不能用 content 顺序二次推断：completed 工具随后还会多推一条 tool-result，
                //   游标会从那里起错位，导致整条消息的 toolCallIds 全空——实机验证发现。）
                if let Some(call_id) = block
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                {
                    tool_call_ids.insert(messages.len() as i64, call_id.to_string());
                }
                let call_index = messages.len() as i64;
                // `question` 工具的正式答案随回合落在消息记录里（`state.metadata.answers`
                // + `state.input.questions`）。这是表单被删除后**唯一**可持久展示的来源，
                // 因此在这里就派生可读答案并绑定到本条 tool-call 的投影下标。
                if matches!(block.get("name").and_then(Value::as_str), Some("question")) {
                    if let Some(answers) = question_answers_from(block) {
                        if !answers.is_empty() {
                            question_answers.insert(call_index, answers);
                        }
                    }
                }
                messages.push(SessionMessage {
                    seq: 0,
                    role: "assistant".to_string(),
                    kind: "tool-call".to_string(),
                    content,
                    ts,
                    tool_name: Some(name),
                    tool_args,
                    collapsed: true,
                });
                // R5a：终态工具结果也要成为一等消息（此前只有 input/error，前端只能
                // 自造 tool-result）。running 没有结果；未知 output 形态如实序列化，
                // 空/不可序列化则不伪造。H06 D1：collapsed 遵守 content.rs 契约——
                // 默认折叠只归 thinking / tool-call，tool-result 出场即展开
                // （运行态收起由前端「过程折叠」叠加层负责，wire 层不得整体折叠）。
                if block.pointer("/state/status").and_then(Value::as_str) == Some("completed") {
                    let output = block
                        .pointer("/state/output")
                        .or_else(|| block.pointer("/state/content"));
                    if let Some(text) = render_tool_output(output) {
                        messages.push(SessionMessage {
                            seq: 0,
                            role: "assistant".to_string(),
                            kind: "tool-result".to_string(),
                            content: text,
                            ts,
                            tool_name: None,
                            tool_args: None,
                            collapsed: false,
                        });
                    }
                }
            }
            // 未覆盖的分块类型不猜语义：跳过，不让整份视图失败。
            _ => {}
        }
    }
}

/// 生产显式形态扫描：进程表 argv 匹配 `--server <url> --session <sid>`，且该 pid
/// 到 server 端口存在 ESTABLISHED 连接（argv 意图 + 实连复核，lsof 失败按无证据处理）。
fn production_explicit_scan(session_id: &str) -> Vec<(u32, Vec<String>)> {
    let system = sysinfo::System::new_all();
    let mut matched = Vec::new();
    for process in system.processes().values() {
        let args: Vec<String> = process
            .cmd()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let Some((server, session)) = argv_server_session(&args) else {
            continue;
        };
        if session != session_id {
            continue;
        }
        let Some(port) = reqwest::Url::parse(&server)
            .ok()
            .and_then(|url| url.port_or_known_default())
        else {
            continue;
        };
        if !desktop_process_connected(process.pid().as_u32(), port) {
            continue;
        }
        matched.push((process.pid().as_u32(), args));
    }
    matched
}

/// 生产显式形态凭据读取：**客户端**进程环境的 `OPENCODE_PASSWORD`。
/// 读不到/为空 = fail closed，生产来源真机验收归 R4。
fn production_process_env(pid: u32, key: &str) -> Option<String> {
    let mut system = sysinfo::System::new();
    let pid = sysinfo::Pid::from_u32(pid);
    // sysinfo 默认刷新集合**不含 environ**（macOS 仅显式启用才填充）——必须
    // specifics + with_environ(UpdateKind::Always)，否则凭据恒缺失、显式路径恒拒。
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[pid]),
        true,
        sysinfo::ProcessRefreshKind::nothing().with_environ(sysinfo::UpdateKind::Always),
    );
    let process = system.process(pid)?;
    process.environ().iter().find_map(|entry| {
        let rendered = entry.to_string_lossy();
        let (name, value) = rendered.split_once('=')?;
        (name == key && !value.is_empty()).then(|| value.to_string())
    })
}

/// 生产显式形态服务信息读取：向 argv 指认端点带凭据 GET `/api/info`。
/// 凭据核对内含于该读（401 = 凭据代际消失 → `OC_UNAUTHORIZED`，绝不重放写）。
/// 在 `fetch_explicit_service_info` 的 spawn_blocking 内执行（reqwest blocking）。
fn production_explicit_info_fetch(
    endpoint: reqwest::Url,
    password: String,
) -> Result<Value, NativeError> {
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        // Policy::none：不跟随重定向（与 NativeClient 同规则）——argv 端点虽已验证
        // 回环，但 302 Location 可指向外部地址；urls 核对发生在请求之后拦不住越界
        // 访问，Basic 头也不允许被搬到别的主机/端口。
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|_| {
            NativeError::new(
                codes::OC_TRANSPORT_FAILED,
                "explicit opencode info client failed",
            )
        })?;
    let url = format!("{}/api/info", endpoint.as_str().trim_end_matches('/'));
    let response = client
        .get(url)
        .basic_auth("opencode", Some(&password))
        .send()
        .map_err(|_| {
            NativeError::new(
                codes::OC_TRANSPORT_FAILED,
                "explicit opencode service is unreachable",
            )
        })?;
    if !response.status().is_success() {
        let code = if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            codes::OC_UNAUTHORIZED
        } else {
            // 3xx（重定向被禁后原样返回）与其余非 2xx 一样走受控状态错误。
            codes::OC_HTTP_STATUS
        };
        return Err(NativeError::new(
            code,
            "explicit opencode service info is unavailable",
        ));
    }
    let body = read_explicit_body_capped(response, super::client::MAX_RESPONSE_BYTES)?;
    serde_json::from_slice::<Value>(&body).map_err(|_| {
        NativeError::new(
            codes::OC_RESPONSE_INVALID,
            "explicit opencode service info is not valid JSON",
        )
    })
}

/// 显式身份读的响应体预算：先看声明长度，再按累计字节停下（与
/// `NativeClient::read_body_capped` 同一语义的 blocking 版；超限 `OC_RESPONSE_TOO_LARGE`）。
fn read_explicit_body_capped(
    mut response: reqwest::blocking::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, NativeError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(super::client::response_too_large());
    }
    use std::io::Read;
    let mut body = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let read = response
            .read(&mut chunk)
            .map_err(|_| super::client::transport_failed())?;
        if read == 0 {
            break;
        }
        if body.len() + read > max_bytes {
            return Err(super::client::response_too_large());
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Ok(body)
}

/// 工具终态结果的展示文本：字符串原样；其它 JSON 序列化；空/null 不产出。
///
/// 只做**展示**，不解析业务语义；未知形态保留为 JSON 文本，避免把结构悄悄丢掉。
/// `question` 工具终态 → 可读答案（题目 + 答案文本）。
///
/// 数据形态（实机取证，OpenCode 2.2.5）：
/// `state.input.questions[] = {question, header?, options[]{label,description?}, multiple?}`
/// `state.metadata.answers = [["绿"], ["probe"]]`（按题序，每组是该题选中的 label 列表）。
///
/// 规则：只按官方 `answers` 展示；题目文本取 `question`（缺失时退回 `header`）；
/// 答案逐项用 `、` 并列；题数与答案组数不一致时**不猜**（返回 None，宁可缺失）。
fn question_answers_from(block: &Value) -> Option<Vec<NativeFormAnswerView>> {
    let questions = block.pointer("/state/input/questions")?.as_array()?;
    let answers = block.pointer("/state/metadata/answers")?.as_array()?;
    if questions.is_empty() || questions.len() != answers.len() {
        return None;
    }
    let mut out = Vec::with_capacity(questions.len());
    for (question, answer) in questions.iter().zip(answers) {
        let title = question
            .get("question")
            .and_then(Value::as_str)
            .or_else(|| question.get("header").and_then(Value::as_str))
            .map(str::trim)
            .filter(|text| !text.is_empty())?;
        let picked: Vec<String> = answer
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .collect();
        if picked.is_empty() {
            continue; // 该题没答：不展示空行（不等于「未作答」结论）
        }
        out.push(NativeFormAnswerView {
            title: title.chars().take(200).collect(),
            answer: picked.join("、"),
        });
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

fn render_tool_output(output: Option<&Value>) -> Option<String> {
    match output? {
        Value::String(text) => {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| text.clone())
        }
        Value::Null => None,
        other => {
            let text = serde_json::to_string(other).ok()?;
            (!text.trim().is_empty()).then_some(text)
        }
    }
}

fn assistant_text(text: &str, ts: Option<i64>) -> SessionMessage {
    SessionMessage {
        seq: 0,
        role: "assistant".to_string(),
        kind: "assistant".to_string(),
        content: text.to_string(),
        ts,
        tool_name: None,
        tool_args: None,
        collapsed: false,
    }
}

/// 消息正文：顶层 `text` 优先，其次 `payload.text`（任务 0 实证的两种 user 形态）。
fn text_of(value: &Value) -> Option<&str> {
    value
        .get("text")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/payload/text").and_then(Value::as_str))
}

/// OpenCode `session.synthetic` 记录的系统提醒 XML 包裹标签。
const SYSTEM_REMINDER_OPEN: &str = "<system-reminder>";
const SYSTEM_REMINDER_CLOSE: &str = "</system-reminder>";

/// 剥掉 `<system-reminder>…</system-reminder>` 包裹，只留内部正文。
///
/// 判据（按形态、不猜语义）：
/// - 有成对包裹 → 取**第一段**内部正文（trim；多段时第一段即当前生效提醒）；
/// - 只有开标签、没有闭标签 → 取开标签之后的全部文本（trim）；
/// - 没有标签（上游可能注入裸文本）→ 原样 trim 返回；
/// - 孤立闭标签按分隔符丢掉；
/// - 正文为空白（如 `<system-reminder>   </system-reminder>`）→ 返回空串，
///   调用方据此跳过，不产空消息。
///
/// 任何分支下返回文本都**不含** `<system-reminder>` 标签（验收条件：标签不展示给用户）。
fn strip_system_reminder_tags(text: &str) -> String {
    match text.find(SYSTEM_REMINDER_OPEN) {
        Some(start) => {
            let rest = &text[start + SYSTEM_REMINDER_OPEN.len()..];
            match rest.find(SYSTEM_REMINDER_CLOSE) {
                Some(close_rel) => rest[..close_rel].trim().to_string(),
                // 只有开标签：其后全部算提醒正文（仍需去掉可能残留的闭标签）。
                None => rest.replace(SYSTEM_REMINDER_CLOSE, "").trim().to_string(),
            }
        }
        // 无开标签：裸文本照常展示，孤立闭标签当分隔符丢掉。
        None => text.replace(SYSTEM_REMINDER_CLOSE, "").trim().to_string(),
    }
}

/// 回合状态（**仅展示**）：尾部 `idle` 记录是原生回合终态标记，其余视为仍在进行。
/// 它不参与投递判定，也不把待答表单变成已答。
///
/// 当前会话模型（只读展示）：只接受 `data.model` 的 `{id, providerID, variant?}` 完整
/// 可读形态；任一必填字段缺失/空白/非字符串 → `None`（序列化省略）。**不猜**默认模型，
/// 也不拿消息级 `assistant.model` 顶替——那描述的是上一回合。
fn session_model_from(native: &Value) -> Option<super::types::NativeModelRef> {
    let model = native.pointer("/data/model")?;
    let id = model.get("id").and_then(Value::as_str)?.trim();
    let provider_id = model.get("providerID").and_then(Value::as_str)?.trim();
    if id.is_empty() || provider_id.is_empty() {
        return None;
    }
    let variant = model
        .get("variant")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(64).collect());
    Some(super::types::NativeModelRef {
        id: id.chars().take(128).collect(),
        provider_id: provider_id.chars().take(128).collect(),
        variant,
    })
}

/// 模型展示名：`/api/model` 清单里按 `providerID + id` **精确且唯一**命中时取其 `name`。
///
/// 任何不确定（清单读失败、无命中、多条同名不同 provider、`name` 空白）都返回 `None`：
/// 前端据此只显示 id。**绝不**用相近名字或默认模型顶替当前模型。
async fn model_display_name(
    client: &NativeClient,
    reference: &super::types::NativeModelRef,
) -> Option<String> {
    let value = client.read_json(MODEL_LIST_PATH, &[]).await.ok()?;
    let items = value.get("data").and_then(Value::as_array)?;
    // 匹配用**未截断的原值**：截断后再比会把两个长 id 的前缀当成同一项（假命中）。
    let mut matched: Option<&str> = None;
    for item in items {
        let provider = item
            .get("providerID")
            .and_then(Value::as_str)
            .or_else(|| item.get("canonical").and_then(Value::as_str));
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| item.get("modelID").and_then(Value::as_str));
        if provider != Some(reference.provider_id.as_str()) || id != Some(reference.id.as_str()) {
            continue;
        }
        let name = item.get("name").and_then(Value::as_str).map(str::trim)?;
        if name.is_empty() || name.chars().count() > 128 {
            // 空白或超长展示名都按「不可用」处理（不截断、不伪造）。
            return None;
        }
        if matched.is_some() {
            // 同一 provider+id 出现多条（歧义）：不选，交给前端显示 id。
            return None;
        }
        matched = Some(name);
    }
    matched.map(str::to_owned)
}

/// 只读原始权限规则：不排序、不合并、不推断默认 effect；形状不完整时 known=false。
fn permission_rules_from(native: &Value) -> (Vec<super::types::NativePermissionRule>, bool) {
    let Some(value) = native.get("data").and_then(|data| data.get("permissions")) else {
        // Session.Info.permissions 可选；缺席表示没有额外规则。
        return (Vec::new(), true);
    };
    let Some(items) = value.as_array() else {
        return (Vec::new(), false);
    };
    let mut rules = Vec::new();
    for item in items.iter().take(100) {
        let action = item.get("action").and_then(Value::as_str);
        let resource = item.get("resource").and_then(Value::as_str);
        let effect = item.get("effect").and_then(Value::as_str);
        let (Some(action), Some(resource), Some(effect)) = (action, resource, effect) else {
            return (Vec::new(), false);
        };
        if action.is_empty() || resource.is_empty() || effect.is_empty() {
            return (Vec::new(), false);
        }
        rules.push(super::types::NativePermissionRule {
            action: action.chars().take(128).collect(),
            resource: resource.chars().take(256).collect(),
            effect: effect.chars().take(32).collect(),
        });
    }
    (rules, true)
}

/// argv 解析（显式形态证据）：可执行名必须是 `opencode`，提取 `--server <url>` 与
/// `--session <sid>`；缺任一即 None。args 已是完整 argv（含 argv[0]）。
fn argv_server_session(args: &[String]) -> Option<(String, String)> {
    let exe = args.first().map(|arg| arg.as_str()).unwrap_or_default();
    let exe_name = std::path::Path::new(exe)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if exe_name != "opencode" {
        return None;
    }
    let mut server = None;
    let mut session = None;
    let mut index = 0;
    while index + 1 < args.len() {
        match args[index].as_str() {
            "--server" => server = Some(args[index + 1].trim_end_matches('/').to_string()),
            "--session" | "-s" => session = Some(args[index + 1].clone()),
            _ => {}
        }
        index += 1;
    }
    Some((server?, session?))
}

/// 桌面进程参数校验：必须同时出现 `--server <固定服务>` 与 `--session <本会话>`。
/// 只认 `opencode` 可执行文件；`--stdio`/其他工具进程不算。
fn desktop_argv_matches(args: &[String], session_id: &str, endpoint: &str) -> bool {
    let Some((server, session)) = argv_server_session(args) else {
        return false;
    };
    server == endpoint.trim_end_matches('/') && session == session_id
}

pub(crate) fn desktop_client_bound(session_id: &str, endpoint: &str) -> bool {
    let Some(port) = reqwest::Url::parse(endpoint)
        .ok()
        .and_then(|url| url.port_or_known_default())
    else {
        return false;
    };
    let system = sysinfo::System::new_all();
    system.processes().values().any(|process| {
        let args: Vec<String> = process
            .cmd()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        desktop_argv_matches(&args, session_id, endpoint)
            && desktop_process_connected(process.pid().as_u32(), port)
    })
}

/// 在进程 argv 匹配后，再要求该 PID 到固定服务端口存在 ESTABLISHED 连接。
/// 仅凭同名/同参数进程不构成绑定；lsof 失败按未绑定处理（fail closed）。
fn desktop_process_connected(pid: u32, port: u16) -> bool {
    let pid_arg = pid.to_string();
    let network_arg = format!("-iTCP:{port}");
    let args = [
        "-nP",
        "-a",
        "-p",
        pid_arg.as_str(),
        network_arg.as_str(),
        "-sTCP:ESTABLISHED",
    ];
    // GUI 进程的 PATH 不一定含 /usr/sbin；绝对路径失败再回落 PATH。
    let output = std::process::Command::new("/usr/sbin/lsof")
        .args(args)
        .output()
        .or_else(|_| std::process::Command::new("lsof").args(args).output());
    output.is_ok_and(|out| out.status.success())
}

fn runtime_state(records: &[Value]) -> String {
    for record in records.iter().rev() {
        match record.get("type").and_then(Value::as_str) {
            Some("idle") => {
                return match record.get("outcome").and_then(Value::as_str) {
                    Some(outcome) => format!("idle:{outcome}"),
                    None => "idle".to_string(),
                };
            }
            Some("assistant") | Some("user") => return "running".to_string(),
            _ => continue,
        }
    }
    "unknown".to_string()
}

/// 原生 id → 单段路径参数：只允许 `[A-Za-z0-9_-]`，杜绝借 id 改写请求路径。
pub(crate) fn segment(raw: &str) -> Result<&str, NativeError> {
    let valid = !raw.is_empty()
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if valid {
        Ok(raw)
    } else {
        Err(NativeError::new(
            codes::OC_PATH_INVALID,
            "native identifier is not accepted",
        ))
    }
}

/// `short_hex` 取摘要的前多少字节（16 字节 → 32 位十六进制）。
const SHORT_HEX_BYTES: usize = 16;

/// `contract::content_hash` 的输出长度（SHA-256 全长，64 位十六进制）。
const CONTENT_HASH_LEN: usize = 64;

fn hash_text(hasher: &mut Sha256, text: &str) {
    hasher.update(text.len().to_string().as_bytes());
    hasher.update(b":");
    hasher.update(text.as_bytes());
}

fn short_hex(hasher: Sha256) -> String {
    hasher
        .finalize()
        .iter()
        .take(SHORT_HEX_BYTES)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 上下文 token：同一 binding 下同一项、同一内容 → 稳定；内容变化 → 换新。
///
/// 本函数是 token 的**唯一**派生处：表单与审批共用它，只由 `kind` 区分。回答校验
/// （`forms::same_form_context_for`）也调它重算，不要在别处另抄一份哈希配方。
pub(crate) fn context_token(binding: &str, kind: &str, id: &str, content: &str) -> String {
    let mut hasher = Sha256::new();
    hash_text(&mut hasher, "context");
    hash_text(&mut hasher, binding);
    hash_text(&mut hasher, kind);
    hash_text(&mut hasher, id);
    hash_text(&mut hasher, content);
    short_hex(hasher)
}

/// 投影产出的 token 形状：`short_hex` 的小写十六进制、`SHORT_HEX_BYTES * 2` 位。
///
/// 形状与 `context_token` 同源，随 `SHORT_HEX_BYTES` 一起变；调用方用它挡住「表单 id /
/// 展示文案 / 手搓串被当成 token」。
pub(crate) fn is_context_token(value: &str) -> bool {
    value.len() == SHORT_HEX_BYTES * 2 && is_lower_hex(value)
}

/// `parse_form` 的 `content_hash` 形状：`format!("{byte:02x}")` 只产出小写 hex，
/// 长度 `CONTENT_HASH_LEN`。大写串不是解析产物。
pub(crate) fn is_content_hash(value: &str) -> bool {
    value.len() == CONTENT_HASH_LEN && is_lower_hex(value)
}

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// 待处理表单（已解析出字段与内容锚）。
struct PendingForm {
    id: String,
    title: String,
    fields: Vec<FormField>,
    content_hash: String,
}

/// 一次成功投影的完整快照（watch 发布的修订号与它对应）。
struct Snapshot {
    source_client: Arc<NativeClient>,
    message_ids: BTreeMap<i64, String>,
    tool_call_ids: BTreeMap<i64, String>,
    question_answers: BTreeMap<i64, Vec<NativeFormAnswerView>>,
    session: Session,
    target: NativeTarget,
    capabilities: Capabilities,
    messages: Vec<SessionMessage>,
    /// R5c：原生 user 消息附件，按消息 `seq` 关联。
    files: BTreeMap<i64, Vec<NativeFileView>>,
    /// 分页是否读到游标耗尽：只有 true 才能作为「缺席即未投递」的证据。
    complete: bool,
    /// D-N5-4：`Some(OC_RESPONSE_TOO_LARGE)` = 消息枚举因「单页降到 limit=1 仍超预算」
    /// 终止——「部分内容过大未载入」是可归因事实，必须与预算耗尽等其它部分原因区分。
    /// None = 部分历史另有原因或读到完整。
    partial_reason: Option<String>,
    pending: Vec<PendingView>,
    inbox: Vec<InboxView>,
    runtime_state: String,
    service_url: String,
    permission_rules: Vec<super::types::NativePermissionRule>,
    permission_rules_known: bool,
    /// 只读：当前会话后续回合使用的官方模型；形状不可读时 None。
    model: Option<super::types::NativeModelRef>,
    binding_token: String,
}

/// 一次只读重建的**原始**产物：保留原生 `id` 的消息记录。
///
/// 与 [`Snapshot`] 的差别只有一处：消息保留原始 JSON（带 `id`）。投影给手机看的
/// [`SessionMessage`] 没有 `id` 字段，而 `Confirmed` 的唯一确定性依据正是原生 id
/// 出现在消息列表里——所以任务 7 的回读必须能拿到原始记录。
struct RawSnapshot {
    source_client: Arc<NativeClient>,
    message_ids: BTreeMap<i64, String>,
    tool_call_ids: BTreeMap<i64, String>,
    question_answers: BTreeMap<i64, Vec<NativeFormAnswerView>>,
    session: Session,
    target: NativeTarget,
    capabilities: Capabilities,
    records: Vec<Value>,
    /// 已投影消息（与 `records` 同源）：`refresh` 直接复用，避免对同一页投影两次。
    messages: Vec<SessionMessage>,
    /// R5c：附件视图（与 `messages` 的 `seq` 对齐），同一次投影算出。
    files: BTreeMap<i64, Vec<NativeFileView>>,
    complete: bool,
    /// D-N5-4：见 [`Snapshot::partial_reason`]。
    partial_reason: Option<String>,
    pending: Vec<PendingView>,
    /// 审批列表的负面证据是否可信：任一条目归属不可证（`sessionID` 缺失/非字符串）⇒
    /// false。只有 true 时「列表里没有该 requestId」才可作「请求已消失」的证据。
    permissions_trusted: bool,
    inbox: Vec<InboxView>,
    runtime_state: String,
    service_url: String,
    permission_rules: Vec<super::types::NativePermissionRule>,
    permission_rules_known: bool,
    /// 只读：当前会话后续回合使用的官方模型；形状不可读时 None。
    model: Option<super::types::NativeModelRef>,
    binding_token: String,
}

/// 派发期的当前投影（任务 7 的复核依据）。
///
/// 一次只读重建的结果：绑定令牌（换代际/换会话/换目录即变）、逐操作能力、
/// pending（含当前 context token）、inbox、以及带原生 id 的消息记录。
pub(crate) struct DispatchSnapshot {
    pub(crate) source_client: Arc<NativeClient>,
    /// 已绑定的原生目标（会话 + 已归一目录 + 服务代际）。
    pub target: NativeTarget,
    /// 逐操作能力表（缺键 = 未判定 = 不放行）。
    pub capabilities: Capabilities,
    /// 当前 binding token：命令携带的令牌必须与它逐字相等。
    pub binding_token: String,
    /// 原始消息记录（带原生 `id`）。
    pub records: Vec<Value>,
    /// 分页是否读到游标耗尽：只有 true 时「列表里没有」才可作否定证据。
    pub complete: bool,
    /// 待答表单与审批（含当前 context token）。
    pub pending: Vec<PendingView>,
    /// 审批列表的负面证据是否可信：任一审批条目归属不可证 ⇒ false。
    /// `false` 时「pending 里没有该 requestId」不得用于 Confirmed/Settled。
    pub permissions_trusted: bool,
    /// 原生 inbox 项。
    pub inbox: Vec<InboxView>,
}

/// 单代际连接 + 该代际的逐操作能力表（能力只随契约变化，不随视图重建重算）。
pub(crate) struct Source {
    pub(crate) descriptor: ServiceDescriptor,
    pub(crate) client: Arc<NativeClient>,
    pub(crate) capabilities: Capabilities,
}

/// 一条下游订阅所依赖的固定来源与上游实例。
pub(crate) struct NativeSubscriptionPin {
    target: NativeTarget,
    require_active: bool,
    client: Arc<NativeClient>,
    pump: Arc<watch::Sender<bool>>,
}

struct Entry {
    tx: watch::Sender<u64>,
    revision: u64,
    snapshot: Option<Arc<Snapshot>>,
    /// 空闲回收可释放正文，但保留轻量目标以核验现有只读绑定。
    selected_target: Option<NativeTarget>,
    selected_session: Option<Session>,
}

impl Entry {
    fn new() -> Self {
        let (tx, _) = watch::channel(0);
        Self {
            tx,
            revision: 0,
            snapshot: None,
            selected_target: None,
            selected_session: None,
        }
    }
}

/// 一个代际的一条 SSE 上游。
struct Pump {
    generation: RuntimeKey,
    /// 与上游任务共享的取消信号，同时也是「在册身份」：回填 handle、摘除时都用它确认是自己的。
    cancel: Arc<watch::Sender<bool>>,
    /// spawn 前是 `None`（占位）：并发调用者看到占位就不会再 spawn 第二条流。
    handle: Option<JoinHandle<()>>,
}

/// 候选预览凭据（内存、短 TTL）：把「用户在预览里看到的目标」固定为确认时的唯一可登记对象。
/// 只签发给同一设备、同一会话、同一监听代际；pause/shutdown 后一律失效。
struct CandidatePin {
    session_id: String,
    device_id: String,
    target: NativeTarget,
    descriptor: ServiceDescriptor,
    window: Option<super::binding_gate::LockedWindow>,
    epoch: u64,
    expires_at: std::time::Instant,
}

/// 预览结果：脱敏摘要 + 稳定候选绑定令牌（供回执核对）+ 一次性随机预览令牌
/// （确认凭据，与稳定 token 分离：多设备/多次预览互不覆盖、过期不可续期）。
pub struct CandidatePreview {
    pub target: NativeTarget,
    pub binding_token: String,
    pub preview_token: String,
    pub expires_at_ms: i64,
}

/// 预览凭据有效期（毫秒）：只覆盖「看完就确认」的短窗口，过期必须重新预览。
const CANDIDATE_PREVIEW_TTL_MS: i64 = 60_000;

#[derive(Default)]
struct State {
    source: Option<Arc<Source>>,
    /// source 安装或失效时递增，防迟到的固定读取覆盖更新的来源。
    source_revision: u64,
    sessions: HashMap<String, Entry>,
    pump: Option<Pump>,
}

/// 在飞投影计数守卫：即使 `view` 中途 panic，也必须归还计数，否则释放逻辑永远看不到「空闲」。
struct ViewGuard<'a>(&'a AtomicUsize);

impl<'a> ViewGuard<'a> {
    fn new(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for ViewGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct HubInner {
    // 仅底层协调器测试可绕过产品归属门；不代表真实实例证明。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    coordinator_test_only: bool,
    /// 仅单元测试注入归属事实；服务身份与代际核验仍执行。
    #[cfg(test)]
    ownership_test_evidence: bool,
    #[cfg(test)]
    before_claim_test_hold: Mutex<Option<Arc<tests::Hold>>>,
    #[cfg(test)]
    after_claim_test_hold: Mutex<Option<Arc<tests::ThreadHold>>>,
    #[cfg(test)]
    pump_retire_test_hold: Mutex<Option<Arc<tests::PumpRetireHold>>>,
    #[cfg(test)]
    pump_after_route_test_hold: Mutex<Option<Arc<tests::Hold>>>,
    #[cfg(test)]
    verify_before_store_test_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// G1 确定性并发回归闸：派发链「写权链通过后、最终 claim/入队前」的单次闸。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    dispatch_preclaim_test_hold: Mutex<Option<Arc<DispatchPreclaimHold>>>,
    /// I2 仅测试：计时落账闸（证明「回执不等待计时写库」；见
    /// [`NativeHub::arm_timing_persist_hold_for_tests`]）。生产恒 None。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    timing_persist_test_hold: Mutex<Option<Arc<super::budget::TimingPersistHold>>>,
    /// I2 仅测试：claim 事务内「写权后、提交前」的持锁缝（见
    /// [`NativeHub::arm_claim_post_write_hold_for_tests`]）。生产恒 None。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    claim_post_write_test_hold: Mutex<Option<Arc<ClaimPostWriteHold>>>,
    /// I2 fix1 仅测试：claim 提交完成后、原生请求开始前的持锁缝（见
    /// [`NativeHub::arm_claim_post_commit_hold_for_tests`]）。生产恒 None。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    claim_post_commit_test_hold: Mutex<Option<Arc<ClaimPostCommitHold>>>,
    /// H5 统一端到端预算的测试注入缝（毫秒；0 = 用生产预算）。生产构建零此字段
    /// （预算恒 [`budget::ACTION_E2E_BUDGET_MS`]），测试可中途调小做确定性截断
    /// （如「停在派发闸内 → 缩预算 → 放行 ⇒ claim 拒绝授权写出」）。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    action_budget_override_ms: std::sync::atomic::AtomicI64,
    /// 动作账本共用同一存储（任务 5/7）；投影本身不读写它。
    store: DeviceStore,
    /// 动作账本实例：**每进程一个**（首次使用时构造，构造即崩溃恢复点）。
    actions: OnceLock<Arc<NativeActionStore>>,
    input: DiscoveryInput,
    process_fact: Arc<dyn Fn(u32) -> Option<i64> + Send + Sync>,
    /// G3/C6 三形态代际事实（`classify_original_generation` 与 patch5 死亡代际豁免
    /// 消费）：生产 = [`production_generation_fact`]（macOS 双源交叉的正向死亡
    /// 证据，H2）；注入构造默认从两态缝保守桥接（None ⇒ ReadFailure），显式形态经
    /// [`NativeHub::with_coordinator_facts`] 注入。
    generation_fact: GenerationFact,
    /// 同目录多 service 保守闸：true = 无法证明固定执行者，原生写拒绝。
    service_conflict: Arc<dyn Fn(&Path) -> bool + Send + Sync>,
    /// 显式连接形态（argv `--server`+`--session`，无登记文件）的 argv 扫描与环境读取。
    /// 登记存在时从不使用；判定规则与普通形态相同（pid/urls 互证，版本无锚跳过）。
    explicit_scan: ExplicitScan,
    explicit_env: ExplicitEnv,
    /// 显式形态的服务信息读取（生产 = 带凭据的 `/api/info` HTTP 读）。
    explicit_info: ExplicitInfoFetch,
    /// token 的随机锚：不落盘、不进 DTO，只在进程内参与派生。
    secret: [u8; 16],
    stopped: AtomicBool,
    /// 监听代际：每次 `pause`/`shutdown` 前进一格。在途 view/pump 只把开始时的
    /// 代际带进 `commit`；代际不再相等就丢弃结果，绝不写进新监听的投影。
    epoch: AtomicU64,
    views: AtomicUsize,
    state: Mutex<State>,
    /// 任务 8 旧路径写屏障的每会话许可：read = 旧写入口/ flush 在飞；write = 切路由中。
    /// 只按 session_id 分锁；其他工具与路径不取本锁。表项随会话 id 首次出现而建、
    /// 不回收——单条目是 `Arc<RwLock<()>>`，量级 = 历史会话数，可接受。
    legacy_permits: Mutex<HashMap<String, Arc<tokio::sync::RwLock<()>>>>,
    /// 同一 action ID 的入口串行；弱引用防止历史 ID 持续占用锁对象。
    action_permits: Mutex<HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    /// 候选预览凭据（candidate binding token → pin）：确认时把登记对象固定为预览目标。
    candidate_pins: Mutex<HashMap<String, CandidatePin>>,
    /// 预览 TTL（毫秒）：仅协调器测试可缩短，生产恒为 [`CANDIDATE_PREVIEW_TTL_MS`]。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    candidate_preview_ttl_ms: std::sync::atomic::AtomicI64,
    outbox: super::outbox::OutboxRuntime,
    /// 本进程**真实在飞**的动作（action_id → 引用计数）。
    ///
    /// 这是「活跃 future」事实，不是账本里的可改写展示状态：`verify` 可以把
    /// `dispatching` 改成 `unconfirmed`，但原写仍可能执行；force 必须据此拒绝作废。
    /// **必须计数**：同一 actionId 的并发派发（`reserve` 的 Existing 自愈路径）可同时
    /// 登记；若用集合+无条件 remove，claim 失败者的 Drop 会删掉胜者仍在飞的登记。
    in_flight: Mutex<HashMap<String, usize>>,
    /// 已启动的实际桥（用于状态/安装/停机）；测试不启动。
    /// 服务桥和登记信息只能核验服务身份，不能证明 TUI 的实际连接归属。
    bridge: std::sync::Mutex<Option<Arc<BridgeRuntime>>>,
    /// **echo-challenge 归属写门**（H03）运行态。`None` = 门未装配（**仅注入构造
    /// `with_sources` / `with_coordinator_sources` 的既有用例**；生产构造 `new`
    /// 恒装配）——这是可测缝，不是产品开关：装配与否在构造时定型，无运行时旁路。
    binding_gate: Option<Arc<super::binding_gate::GateRuntime>>,
    /// 锁定窗口查询（目标会话 → 新鲜 `Opened` TUI 身份）。生产 = 观察注册表。
    gate_locked_window: super::binding_gate::LockedWindowLookup,
    /// TUI 进程事实（pid → 启动毫秒）。生产 = sysinfo。
    gate_tui_fact: super::binding_gate::TuiProcessFact,
    /// N1 固定窗口绑定（session_id → 选择时刻绑定的窗口身份 + 当时目标）。
    /// 只在**选择事件**（首次认领 / 显式重进 / 换代际重绑）时写入；活跃卡与展示仍走
    /// 当前 `Opened`（观察注册表），本表只服务已固定目标的写权链。
    pinned_windows: Mutex<HashMap<String, PinnedWindowBinding>>,
    /// N2 语义证据准入：锚定 TUI 构建解析缝（pid → 二进制 SHA-256 小写 hex；
    /// `None` = 取不到，按不适用处理）。生产 = [`super::attribution_evidence::
    /// production_tui_binary_sha`]；装门测试注入桩（与 [`Self::gate_tui_fact`] 同级缝）。
    tui_binary_sha: super::attribution_evidence::TuiBinarySha,
    /// M2 语义证据准入：**服务**构建解析缝（服务进程 pid → 二进制 SHA-256 小写
    /// hex；`None` = 取不到，按不适用处理）。生产 = `production_tui_binary_sha`
    /// （pid 泛用）；装门测试注入桩。归属语义由 TUI 与服务共同表现（复审 M2-1），
    /// 登记组合必须核对两个构建。
    service_binary_sha: super::attribution_evidence::ServiceBinarySha,
    /// N2 SHA 每代际缓存：(锚定进程 pid, 启动毫秒) → SHA 小写 hex。键 = 进程代际
    /// 身份（PID 复用换启动时刻即新键）——同代际进程存活期二进制不变，命中零开销；
    /// **只缓存成功值**：失败形态可能瞬态（读失败/超时），下一轮重算，绝不把一次
    /// 失败钉死整个代际（可用性与 fail-closed 的平衡，见模块文档）。
    semantic_sha_cache: Mutex<HashMap<(u32, i64), String>>,
    /// M2 服务构建 SHA 每代际缓存（键 = 服务进程 (pid, started_at_ms)；惯例与
    /// `semantic_sha_cache` 逐字同源：只缓存成功值，失败下一轮重算）。
    service_sha_cache: Mutex<HashMap<(u32, i64), String>>,
}

/// 已固定目标在**选择时刻**绑定的窗口身份（N1：固定手机目标与窗口导航解耦）。
///
/// 缺口背景：手机固定 A 后，同一 TUI 窗口在同一服务内浏览到 B（`Opened(B)`）时，
/// 写门若按「当前新鲜 `Opened`」找锁定窗口会找不到 A → 写被拒。固定目标不应被
/// 窗口当前导航劫持：初次选择 A 时从新鲜观察取得候选窗口身份（pid/启动时刻/插件
/// boot），与会话/目录/服务代际一起保存；原生写用该身份核对存活、插件代际
///（控制文件 expectBoot）和实际连接（challenge 经服务 S 回声 + ack pid 比对），
/// 沿用回声门与 ACK 后终验。**绑定身份本身无写权**：逐次 echo 门判定不变。
#[derive(Debug, Clone)]
struct PinnedWindowBinding {
    /// 选择时核验通过的完整目标（会话/目录/服务代际）；写授权核对时必须与当前
    /// 目标全等、且路由行仍 `active` 同代际同目录才适用。
    target: NativeTarget,
    /// 选择时新鲜观察给出的窗口身份（pid + 启动时刻 + 插件 boot）。
    window: super::binding_gate::LockedWindow,
}

/// 原生只读投影中枢。
///
/// `new` 只建立一个惰性容器：不发现服务、不建连接、不起任务；第一次 `view`/`watch`
/// 才按需工作。测试与集成用 `with_sources` 注入临时登记文件与进程事实。
pub struct NativeHub {
    inner: Arc<HubInner>,
}

/// 在飞动作的 RAII 标记：dispatch 无论成功/失败/被丢弃，Drop 都会摘除。
pub(crate) struct InFlightGuard {
    inner: Arc<HubInner>,
    action_id: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut in_flight = self.inner.in_flight.lock().unwrap();
        let Some(count) = in_flight.get_mut(&self.action_id) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            in_flight.remove(&self.action_id);
        }
    }
}

/// 进程事实来源：pid → 进程启动毫秒（`None` = 拿不到，fail-closed）。
pub(crate) type ProcessFact = Arc<dyn Fn(u32) -> Option<i64> + Send + Sync>;
/// 多 service 冲突探针：目标目录存在 ≥2 个可执行 service ⇒ true（证据不明）。
pub(crate) type ServiceConflictProbe = Arc<dyn Fn(&Path) -> bool + Send + Sync>;
/// 显式形态 argv 扫描：目标会话 → [(客户端 pid, 完整 argv)]（生产 = 进程表 + ESTABLISHED 复核）。
pub type ExplicitScan = Arc<dyn Fn(&str) -> Vec<(u32, Vec<String>)> + Send + Sync>;
/// 显式形态凭据读取：**客户端**进程环境取 `OPENCODE_PASSWORD`（生产 = sysinfo environ）。
pub type ExplicitEnv = Arc<dyn Fn(u32, &str) -> Option<String> + Send + Sync>;
/// 显式形态服务信息读取缝：向 argv 指认端点取 `/api/info`（带凭据）。
/// 生产 = reqwest blocking（spawn_blocking 内执行）；测试注入即时桩。
pub type ExplicitInfoFetch =
    Arc<dyn Fn(reqwest::Url, String) -> Result<Value, NativeError> + Send + Sync>;

/// 显式连接形态的证据源（可测缝；判定分支始终走 [`HubInner::verify_service_identity`]）。
pub struct ExplicitEvidenceSources {
    pub scan: ExplicitScan,
    pub env: ExplicitEnv,
    /// 服务自身 `/api/info` 读取（生产 = 带凭据 HTTP；测试注入桩）。
    pub info: ExplicitInfoFetch,
}

/// 候选路由认领结论（[`NativeHub::view_and_claim`] 的第二个返回值；任务 8 路由层
/// 据此给手机下发 `route` 与可选原因词）。
pub struct RouteClaim {
    /// 当前是否已归 Native（本次认领成功 ∨ 既有 `active` 归属）。
    pub owned: bool,
    /// 路由/写入不可用的受控原因词。当前有两类来源：
    /// 1. 「确实尝试过认领但未进入」（如旧 pending 未决 / 显式退回态）；
    /// 2. **同实例归属未证**（B1）：即使 `owned=true`（升级前已有 active 归属）也会下发，
    ///    因为此时原生写被安全门禁用。消费方不能把「有原因词」等同于「未归原生」。
    pub declined_reason: Option<&'static str>,
    pub route_version: Option<i64>,
}

/// 认领被拒的原因词：旧 `inject_queue` 该会话仍有未决行（不迁移、不重复提交，
/// 由用户先处理旧项）。换绑代际且有未决 native 动作时不成立——那种情况下旧路由
/// 仍是 `active`（`owned=true`），不产生原因词。
pub const ROUTE_DECLINED_LEGACY_PENDING: &str = "legacy_pending";

/// 认领被拒的原因词（评审 R-2）：该会话已被**显式退回**旧路径（路由 `draining`）。
/// 它不会因为「又打开了一次视图」自动复活——重新进入必须显式调用
/// `POST /opencode/route/native`。
pub const ROUTE_DECLINED_RETURNED: &str = "returned_to_legacy";

/// 仅测试：派发链「写权链（回声验证）通过后、最终 claim/入队前」的单次确定性闸
///（G1 换窗并发回归的注入点：闸内做同服务显式重选/伴表破坏，放行后派发才进入
/// 最终事务）。生产路径零分支——未装配即直通。语义与 `tests::Hold` 同形，独立
/// 定义以便 actions 模块的派发链与测试装配（跨模块可见面）。
#[cfg(any(test, feature = "opencode-coordinator-tests"))]
pub struct DispatchPreclaimHold {
    armed: std::sync::atomic::AtomicBool,
    /// 闸已到达（派发任务停在闸内）。
    pub started: tokio::sync::Notify,
    /// 放行。
    pub release: tokio::sync::Notify,
}

#[cfg(any(test, feature = "opencode-coordinator-tests"))]
impl DispatchPreclaimHold {
    fn new() -> Self {
        Self {
            armed: std::sync::atomic::AtomicBool::new(true),
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        }
    }

    async fn pass(&self) {
        use std::sync::atomic::Ordering;
        if self.armed.swap(false, Ordering::SeqCst) {
            self.started.notify_one();
            self.release.notified().await;
        }
    }
}

/// 仅测试：claim 事务内「写权（claim UPDATE）取得后、提交前」的**同步**等待闸
///（I2 写权后复核的确定性注入点）。消费是**同步自旋**（闸在 `store().with` 的
/// sync 闭包内，不能 await）：到达即通知 `started`（Notify 允许先发后订），轮询
/// `release` 原子位放行。单次有效。复现形态 = GFix 口径的「预算内进事务、写权
/// 在握后等待跨截止」（外部锁在 SQLite 3.54 读事务内写升级上立即 BUSY 不等待，
/// 见 ib_write_lock_wait_crossing_budget… 测试注释；真实跨截止等待由本闸以同
/// 位置、同语义确定性复现）。
#[cfg(any(test, feature = "opencode-coordinator-tests"))]
pub struct ClaimPostWriteHold {
    armed: std::sync::atomic::AtomicBool,
    /// 闸已到达（写权已在握，链停在本闸）。
    pub started: tokio::sync::Notify,
    /// 放行位。
    pub release: std::sync::atomic::AtomicBool,
}

#[cfg(any(test, feature = "opencode-coordinator-tests"))]
impl ClaimPostWriteHold {
    fn new() -> Self {
        Self {
            armed: std::sync::atomic::AtomicBool::new(true),
            started: tokio::sync::Notify::new(),
            release: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 同步消费：单次有效；到达即通知，然后自旋等放行（闭包内不可 await——
    /// 测试侧放行用 `release` 原子位）。
    pub(crate) fn pass_sync(&self) {
        use std::sync::atomic::Ordering;
        if !self.armed.swap(false, Ordering::SeqCst) {
            return;
        }
        self.started.notify_one();
        while !self.release.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}

/// 仅测试：claim **提交完成后**、原生请求开始前的**同步**等待闸（I2 fix1 发包前
/// 复核 ⑥ 的确定性注入点）。与 [`ClaimPostWriteHold`] 同形（同步自旋消费，单次
/// 有效）：闸在 dispatch 的 async 体里、`store().with` 之外，但保持同一消费形态
/// 以复用同一套测试钟纪律（OS 线程真实时钟 + 纯 waker 等待）。
#[cfg(any(test, feature = "opencode-coordinator-tests"))]
pub struct ClaimPostCommitHold {
    armed: std::sync::atomic::AtomicBool,
    /// 闸已到达（claim 已提交、链停在本闸）。
    pub started: tokio::sync::Notify,
    /// 放行位。
    pub release: std::sync::atomic::AtomicBool,
}

#[cfg(any(test, feature = "opencode-coordinator-tests"))]
impl ClaimPostCommitHold {
    fn new() -> Self {
        Self {
            armed: std::sync::atomic::AtomicBool::new(true),
            started: tokio::sync::Notify::new(),
            release: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 同步消费：单次有效；到达即通知，然后自旋等放行。
    pub(crate) fn pass_sync(&self) {
        use std::sync::atomic::Ordering;
        if !self.armed.swap(false, Ordering::SeqCst) {
            return;
        }
        self.started.notify_one();
        while !self.release.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}

impl NativeHub {
    /// 新会话尚无 TUI 关联证据，只能使用唯一且已登记的后台服务。
    pub(crate) fn discover_create_service(
        &self,
        directory: &Path,
    ) -> Result<ServiceDescriptor, NativeError> {
        self.require_unambiguous_create_service(directory)?;
        discover(
            &self.inner.input,
            &read_state_file,
            &*self.inner.process_fact,
        )
    }

    fn require_unambiguous_create_service(&self, directory: &Path) -> Result<(), NativeError> {
        if (self.inner.service_conflict)(directory) {
            return Err(NativeError::new(
                codes::OC_INSTANCE_AMBIGUOUS,
                "multiple OpenCode services are running; refusing to choose an execution owner",
            ));
        }
        Ok(())
    }

    pub(crate) fn verify_create_generation(
        &self,
        directory: &Path,
        original: &ServiceDescriptor,
    ) -> Result<String, NativeError> {
        self.require_unambiguous_create_service(directory)?;
        let bytes = read_state_file(self.registration_file())?;
        let current = discover(
            &self.inner.input,
            &|_| Ok(bytes.clone()),
            &*self.inner.process_fact,
        )?;
        if current.runtime != original.runtime
            || current.endpoint != original.endpoint
            || current.version != original.version
            || current.password != original.password
        {
            return Err(NativeError::new(
                codes::OC_GENERATION_MISMATCH,
                "the registered OpenCode service changed during creation",
            ));
        }
        Ok(Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect())
    }

    pub(crate) fn registration_file(&self) -> &Path {
        &self.inner.input.state_file
    }

    /// 生产构造（惰性）：登记文件与进程事实都走真实环境，但**现在不做任何 IO**。
    ///
    /// 与注入构造的唯一差别：**装配 echo-challenge 归属写门**（H03）——每一次原生写
    /// 在预留前与取得派发权前都必须通过探针回声挑战核验（fail-closed）。
    pub fn new(store: DeviceStore) -> Self {
        let mut hub = Self::build(
            store,
            super::discovery::production_input(),
            Arc::new(production_process_fact),
            None,
            None,
        );
        let inner = Arc::get_mut(&mut hub.inner).expect("fresh hub must not be shared");
        inner.binding_gate = Some(Arc::new(super::binding_gate::GateRuntime::production()));
        inner.gate_locked_window =
            Arc::new(crate::monitor::opencode_observation::fresh_opened_window);
        inner.gate_tui_fact =
            Arc::new(crate::monitor::opencode_observation::production_process_fact);
        // G3 建形、H2 深化：生产三形态代际事实（macOS = sysinfo + libproc 双源
        // 交叉，四证据齐备才构成正向死亡证明；单源缺席/读取失败一律 fail-closed）。
        inner.generation_fact = Arc::new(production_generation_fact);
        hub
    }

    /// 注入构造：与 [`NativeHub::new`] 共用同一实现，供测试/集成 crate 指定登记文件、
    /// 进程启动时间与多 service 冲突探针（禁止靠改进程 HOME 换发现位置）。
    ///
    /// 探针只是**环境读取**的可测缝（与 `process_fact` 同级），判定分支始终走
    /// [`HubInner::verify_service_identity`] 的生产路径——构造不绕过任何门。
    /// 传 `None` 使用生产探针（进程表扫描）。
    ///
    /// 与 [`NativeHub::new`] 的唯一差别：**不装配 echo-challenge 归属写门**（H03）。
    /// 既有用例测的是门之前的各道防线（目标确认 / 候选 / 身份 / 绑定）；写门行为由
    /// 装配后的 hub（[`NativeHub::arm_binding_gate_for_tests`]）专项覆盖。这不是
    /// 产品开关：生产构造 [`NativeHub::new`] 恒装配，本构造仅测试/夹具可达。
    pub fn with_sources(
        store: DeviceStore,
        input: DiscoveryInput,
        process_fact: ProcessFact,
        service_conflict: Option<ServiceConflictProbe>,
        explicit: Option<ExplicitEvidenceSources>,
    ) -> Self {
        Self::build(store, input, process_fact, service_conflict, explicit)
    }

    /// 单元测试注入桌面归属事实，保留服务冲突和代际校验；不得用于生产证明。
    #[cfg(test)]
    pub(crate) fn with_test_ownership_evidence(mut self) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("test hub must not be shared")
            .ownership_test_evidence = true;
        self
    }

    /// 底层协调器夹具专用，不是可信绑定来源，发布构建不可包含此入口。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    pub fn with_coordinator_sources(
        store: DeviceStore,
        input: DiscoveryInput,
        process_fact: ProcessFact,
    ) -> Self {
        let mut hub = Self::build(store, input, process_fact, None, None);
        Arc::get_mut(&mut hub.inner).unwrap().coordinator_test_only = true;
        hub
    }

    /// 底层协调器夹具专用（G3）：与 [`Self::with_coordinator_sources`] 同一构造，
    /// 另注入**三形态代际事实**——「明确无此 pid」等显式死亡证明形态只能经三形态缝
    /// 表达（两态缝的 None 一律保守桥接为 ReadFailure）。仅供测试表达显式形态，
    /// 不改变任何生产判定分支。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    pub(crate) fn with_coordinator_facts(
        store: DeviceStore,
        input: DiscoveryInput,
        process_fact: ProcessFact,
        generation_fact: GenerationFact,
    ) -> Self {
        let mut hub = Self::build(store, input, process_fact, None, None);
        {
            let inner = Arc::get_mut(&mut hub.inner).unwrap();
            inner.coordinator_test_only = true;
            inner.generation_fact = generation_fact;
        }
        hub
    }

    /// 仅测试：装配 echo-challenge 归属写门并注入全部环境缝（锁定窗口查询、TUI 进程
    /// 事实、窗口/轮询/控制文件根/时钟）。生产装配见 [`NativeHub::new`]；本入口不改变
    /// 判定分支——三件套评估与 fail-closed 编排走的都是 `binding_gate` 生产路径。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    #[allow(clippy::too_many_arguments)] // 测试装配入口：8 个注入缝一一显式（见函数注释）
    pub fn arm_binding_gate_for_tests(
        &mut self,
        locked_window: super::binding_gate::LockedWindowLookup,
        tui_fact: super::binding_gate::TuiProcessFact,
        tui_binary_sha: super::attribution_evidence::TuiBinarySha,
        service_binary_sha: super::attribution_evidence::ServiceBinarySha,
        window_ms: i64,
        poll_interval_ms: u64,
        home: std::path::PathBuf,
    ) {
        let inner = Arc::get_mut(&mut self.inner).expect("test hub must not be shared");
        inner.gate_locked_window = locked_window;
        inner.gate_tui_fact = tui_fact;
        inner.tui_binary_sha = tui_binary_sha;
        inner.service_binary_sha = service_binary_sha;
        inner.binding_gate = Some(Arc::new(super::binding_gate::GateRuntime::with(
            window_ms,
            poll_interval_ms,
            home,
            Arc::new(|| chrono::Utc::now().timestamp_millis()),
        )));
    }

    /// 启动生产服务插件桥：监听 `~/.tuvis/opencode-bridge/bridge.sock`，等待
    /// OpenCode 服务内安装的插件连接。安装/重载插件由 GUI 命令负责；本方法只启动
    /// 认证端。已有桥时幂等。R2 起桥只提供状态/诊断，不再是写门。
    pub async fn start_production_bridge(&self) -> Result<(), String> {
        let input = super::discovery::production_input();
        let paths = BridgePaths::production(&input.home);
        self.start_bridge_with(paths, input, Arc::new(production_process_fact))
            .await
    }

    /// 注入路径的桥启动（测试/隔离环境）。
    pub async fn start_bridge_with(
        &self,
        paths: BridgePaths,
        input: super::types::DiscoveryInput,
        process_fact: ProcessFact,
    ) -> Result<(), String> {
        let _guard = BRIDGE_LIFECYCLE.lock().await;
        let existing = self
            .inner
            .bridge
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(existing) = existing {
            if !existing.is_stopped() {
                return Ok(());
            }
            // 停止后重装/重载：允许同进程恢复；旧 socket 已由 shutdown 清掉，
            // start 会重新 bind。
            existing.shutdown().await;
        }
        let runtime = BridgeRuntime::start(paths, input, process_fact).await?;
        *self
            .inner
            .bridge
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&runtime));
        Ok(())
    }

    pub fn bridge_runtime(&self) -> Option<Arc<BridgeRuntime>> {
        self.inner
            .bridge
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub async fn bridge_status(&self) -> Option<BridgeStatus> {
        match self.bridge_runtime() {
            Some(runtime) => Some(runtime.status().await),
            None => None,
        }
    }

    pub async fn stop_bridge(&self) {
        let _guard = BRIDGE_LIFECYCLE.lock().await;
        let runtime = self
            .inner
            .bridge
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(runtime) = runtime {
            runtime.shutdown().await;
        }
    }

    /// **写权链**（H03 C1 接通后的公开核对入口）：服务三互证前置 + echo-challenge
    /// 归属证明 + ACK 后终验。判定细节见 [`HubInner::verify_write_authority`]。
    pub async fn verify_target_identity(
        &self,
        target: &super::types::NativeTarget,
    ) -> Result<(), NativeError> {
        self.inner.verify_write_authority(target, None).await
    }

    /// 写权链的**显式重选专用**形态（M1）：锁定窗口经 [`HubInner::reselect_window_
    /// lookup`] 覆盖（新鲜候选优先，既有绑定兜底），判据本体不放宽。仅
    /// `reenter_native_route_versioned` 的候选核验消费——一般写路径不经此入口。
    ///
    /// F12 项 1：成功时返回本轮回声验证通过的锁定窗口身份，供认领事务原样落绑定
    /// 行（提交的就是验证的那个窗口）。门未装配（测试逃逸）时为 `None`。
    async fn verify_target_identity_for_reselect(
        &self,
        target: &super::types::NativeTarget,
        candidate_client: &Arc<NativeClient>,
        previewed: Option<&super::binding_gate::LockedWindow>,
    ) -> Result<Option<super::binding_gate::LockedWindow>, NativeError> {
        let lookup = if let Some(window) = previewed {
            self.inner.recheck_previewed_window(target, window)?;
            let window = window.clone();
            Arc::new(move |_: &str| Some(window.clone())) as super::binding_gate::LockedWindowLookup
        } else {
            self.inner.reselect_window_lookup(target)
        };
        // 归属核验使用已固定的候选连接。探针不能提前安装候选来源，否则认领的来源 CAS 会拒绝本请求。
        self.inner
            .verify_write_authority_using(
                target,
                Some(candidate_client),
                Some(lookup),
                previewed.is_some(),
            )
            .await
    }

    /// 写权链的**捕获形态**（F12 项 1，view 认领路径）：与
    /// [`NativeHub::verify_target_identity`] 同一条判定链，成功时返回回声验证通过
    /// 的锁定窗口身份（门未装配时 `None`）。
    pub(crate) async fn verify_target_identity_capturing(
        &self,
        target: &super::types::NativeTarget,
    ) -> Result<Option<super::binding_gate::LockedWindow>, NativeError> {
        self.inner
            .verify_write_authority_using(target, None, None, false)
            .await
    }

    /// **写权链 · 实际写通道形态**：与 [`NativeHub::verify_target_identity`] 同一条
    /// 判定链，但 ACK 后终验与探针 IO 都绑定调用方给定的**实际写通道** client
    /// （派发路径在 `current_source_for` 之后传入 `source.client`）。
    ///
    /// G1 捕获形态：成功时返回本轮回声验证通过的锁定窗口身份（门未装配 = None），
    /// 供派发链把它连同路由版本带进最终派发事务做窗口 CAS（换窗后旧请求零写）。
    pub(crate) async fn verify_write_authority_with(
        &self,
        target: &super::types::NativeTarget,
        client: &Arc<NativeClient>,
    ) -> Result<Option<super::binding_gate::LockedWindow>, NativeError> {
        // G1 捕获形态：直通 `verify_write_authority_using`（不经 `verify_write_authority`
        // 的 `map(|_| ())` 丢弃臂），把本轮回声验证通过的锁定窗口身份交给派发链，
        // 供最终派发事务做窗口 CAS。门未装配（测试逃逸）时 None ⇒ 不加窗条件，
        // 与既有语义逐字一致。
        self.inner
            .verify_write_authority_using(target, Some(client), None, false)
            .await
    }

    /// **写权前置**（轻量）：只跑登记/info/pid 三互证与代际核对，不做归属证明。
    /// 派发中期复核点消费（归属证明由同一次派发更靠后的写权链承担）。
    pub async fn verify_target_preconditions(
        &self,
        target: &super::types::NativeTarget,
    ) -> Result<(), NativeError> {
        self.inner.verify_service_preconditions(target).await?;
        Ok(())
    }

    /// 探针目标身份检查（I4）：该会话 id 是否是本进程创建的写门探针会话。
    pub(crate) fn is_probe_target(&self, session_id: &str) -> bool {
        self.inner
            .binding_gate
            .as_ref()
            .is_some_and(|gate| gate.is_probe_session(session_id))
    }

    /// H5 统一端到端预算（毫秒）：测试注入缝优先（>0），否则生产常量。
    pub(crate) fn action_budget_ms(&self) -> i64 {
        #[cfg(any(test, feature = "opencode-coordinator-tests"))]
        {
            let injected = self
                .inner
                .action_budget_override_ms
                .load(std::sync::atomic::Ordering::SeqCst);
            if injected > 0 {
                return injected;
            }
        }
        super::budget::ACTION_E2E_BUDGET_MS
    }

    /// 仅测试：中途调整统一预算（确定性截断注入点；见 [`Self::action_budget_ms`]）。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    pub fn set_action_budget_ms_for_tests(&self, budget_ms: i64) {
        self.inner
            .action_budget_override_ms
            .store(budget_ms, std::sync::atomic::Ordering::SeqCst);
    }

    /// H5 分段计时读数：echo 门单飞等待累计值（毫秒）。动作链在每次门入口前后取
    /// 差分计入「排队」段；门未装配（测试逃逸）= 恒 0。
    pub(crate) fn gate_flight_wait_total_ms(&self) -> i64 {
        self.inner
            .binding_gate
            .as_ref()
            .map(|gate| gate.flight_wait_total_ms())
            .unwrap_or(0)
    }

    /// 仅测试：在派发链「写权链（回声验证）通过后、最终 claim/入队前」装配单次闸，
    /// 返回句柄供测试等待/放行（G1 换窗并发回归的确定性注入点）。生产恒直通。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    pub fn arm_dispatch_preclaim_hold_for_tests(&mut self) -> Arc<DispatchPreclaimHold> {
        let hold = Arc::new(DispatchPreclaimHold::new());
        Arc::get_mut(&mut self.inner)
            .expect("test hub must not be shared")
            .dispatch_preclaim_test_hold = Mutex::new(Some(Arc::clone(&hold)));
        hold
    }

    /// 仅测试：派发任务侧消费闸（未装配 = 直通）。由 actions 的派发链在写权链
    /// 通过后调用。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    pub(crate) async fn pass_dispatch_preclaim_hold(&self) {
        let hold = self
            .inner
            .dispatch_preclaim_test_hold
            .lock()
            .unwrap()
            .clone();
        if let Some(hold) = hold {
            hold.pass().await;
        }
    }

    /// 仅测试：装配计时落账闸，返回句柄供测试等待/放行（I2「回执不等待计时写库」
    /// 的确定性注入点）。生产恒直通（字段恒 None）。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    pub fn arm_timing_persist_hold_for_tests(&mut self) -> Arc<super::budget::TimingPersistHold> {
        let hold = Arc::new(super::budget::TimingPersistHold::new());
        Arc::get_mut(&mut self.inner)
            .expect("test hub must not be shared")
            .timing_persist_test_hold = Mutex::new(Some(Arc::clone(&hold)));
        hold
    }

    /// 计时落账闸读取（未装配 = None ⇒ 落账直通）。仅测试装配可达。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    pub(crate) fn timing_persist_test_hold(&self) -> Option<Arc<super::budget::TimingPersistHold>> {
        self.inner.timing_persist_test_hold.lock().unwrap().clone()
    }

    /// 仅测试：装配「写权后持锁缝」——claim 事务内、写权（claim UPDATE）取得后、
    /// 提交前的单次同步等待闸（I2 写权后复核的确定性注入点）。生产恒直通。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    pub fn arm_claim_post_write_hold_for_tests(&mut self) -> Arc<ClaimPostWriteHold> {
        let hold = Arc::new(ClaimPostWriteHold::new());
        Arc::get_mut(&mut self.inner)
            .expect("test hub must not be shared")
            .claim_post_write_test_hold = Mutex::new(Some(Arc::clone(&hold)));
        hold
    }

    /// 写权后持锁缝读取（未装配 = None ⇒ 直通）。仅测试装配可达。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    pub(crate) fn claim_post_write_test_hold(&self) -> Option<Arc<ClaimPostWriteHold>> {
        self.inner
            .claim_post_write_test_hold
            .lock()
            .unwrap()
            .clone()
    }

    /// 仅测试：装配「提交后持锁缝」——claim **提交完成后**、原生请求（write_native
    /// / POST /interrupt）开始前的单次同步等待闸（I2 fix1 发包前复核 ⑥ 的确定性
    /// 注入点：在此闸内把链挂过统一预算截止，发包前复核必须在零包状态下拒绝）。
    /// 生产恒直通。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    pub fn arm_claim_post_commit_hold_for_tests(&mut self) -> Arc<ClaimPostCommitHold> {
        let hold = Arc::new(ClaimPostCommitHold::new());
        Arc::get_mut(&mut self.inner)
            .expect("test hub must not be shared")
            .claim_post_commit_test_hold = Mutex::new(Some(Arc::clone(&hold)));
        hold
    }

    /// 提交后持锁缝读取（未装配 = None ⇒ 直通）。仅测试装配可达。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    pub(crate) fn claim_post_commit_test_hold(&self) -> Option<Arc<ClaimPostCommitHold>> {
        self.inner
            .claim_post_commit_test_hold
            .lock()
            .unwrap()
            .clone()
    }

    fn build(
        store: DeviceStore,
        input: DiscoveryInput,
        process_fact: ProcessFact,
        service_conflict: Option<ServiceConflictProbe>,
        explicit: Option<ExplicitEvidenceSources>,
    ) -> Self {
        let (explicit_scan, explicit_env, explicit_info) = match explicit {
            Some(sources) => (sources.scan, sources.env, sources.info),
            None => (
                Arc::new(production_explicit_scan) as ExplicitScan,
                Arc::new(production_process_env) as ExplicitEnv,
                Arc::new(production_explicit_info_fetch) as ExplicitInfoFetch,
            ),
        };
        let mut secret = [0u8; 16];
        rand::fill(&mut secret);
        // 默认从两态缝保守桥接（None ⇒ ReadFailure，fail-closed）；生产构造
        // `new` 覆写为三形态生产探针，显式形态测试经 `with_coordinator_facts`。
        let generation_fact = generation_fact_from_two_state(Arc::clone(&process_fact));
        Self {
            inner: Arc::new(HubInner {
                #[cfg(any(test, feature = "opencode-coordinator-tests"))]
                coordinator_test_only: false,
                #[cfg(test)]
                ownership_test_evidence: false,
                #[cfg(test)]
                before_claim_test_hold: Mutex::new(None),
                #[cfg(test)]
                after_claim_test_hold: Mutex::new(None),
                #[cfg(test)]
                pump_retire_test_hold: Mutex::new(None),
                #[cfg(test)]
                pump_after_route_test_hold: Mutex::new(None),
                #[cfg(test)]
                verify_before_store_test_hook: Mutex::new(None),
                #[cfg(any(test, feature = "opencode-coordinator-tests"))]
                dispatch_preclaim_test_hold: Mutex::new(None),
                #[cfg(any(test, feature = "opencode-coordinator-tests"))]
                timing_persist_test_hold: Mutex::new(None),
                #[cfg(any(test, feature = "opencode-coordinator-tests"))]
                claim_post_write_test_hold: Mutex::new(None),
                #[cfg(any(test, feature = "opencode-coordinator-tests"))]
                claim_post_commit_test_hold: Mutex::new(None),
                #[cfg(any(test, feature = "opencode-coordinator-tests"))]
                action_budget_override_ms: std::sync::atomic::AtomicI64::new(0),
                store,
                actions: OnceLock::new(),
                input,
                process_fact,
                generation_fact,
                service_conflict: service_conflict
                    .unwrap_or_else(|| Arc::new(production_service_conflict)),
                explicit_scan,
                explicit_env,
                explicit_info,
                secret,
                stopped: AtomicBool::new(false),
                epoch: AtomicU64::new(0),
                views: AtomicUsize::new(0),
                state: Mutex::new(State::default()),
                legacy_permits: Mutex::new(HashMap::new()),
                action_permits: Mutex::new(HashMap::new()),
                candidate_pins: Mutex::new(HashMap::new()),
                #[cfg(any(test, feature = "opencode-coordinator-tests"))]
                candidate_preview_ttl_ms: std::sync::atomic::AtomicI64::new(
                    CANDIDATE_PREVIEW_TTL_MS,
                ),
                outbox: super::outbox::OutboxRuntime::default(),
                in_flight: Mutex::new(HashMap::new()),
                bridge: std::sync::Mutex::new(None),
                // 写门默认未装配；`new`（生产）构造后立即装配，
                // `arm_binding_gate_for_tests` 是测试装配的唯一入口。
                binding_gate: None,
                gate_locked_window: Arc::new(|_session: &str| None),
                gate_tui_fact: Arc::new(|_pid: u32| None),
                pinned_windows: Mutex::new(HashMap::new()),
                tui_binary_sha: Arc::new(super::attribution_evidence::production_tui_binary_sha),
                service_binary_sha: Arc::new(
                    super::attribution_evidence::production_tui_binary_sha,
                ),
                semantic_sha_cache: Mutex::new(HashMap::new()),
                service_sha_cache: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// 获取同一动作 ID 的入口锁；已无使用者的弱引用按需清理。
    pub(crate) fn action_permit(&self, action_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut permits = self
            .inner
            .action_permits
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        permits.retain(|_, permit| permit.strong_count() > 0);
        if let Some(permit) = permits.get(action_id).and_then(std::sync::Weak::upgrade) {
            return permit;
        }
        let permit = Arc::new(tokio::sync::Mutex::new(()));
        permits.insert(action_id.to_string(), Arc::downgrade(&permit));
        permit
    }

    /// 标记一次派发进入「真实在飞」（claim 成功后、收敛前），Drop 即摘除。
    ///
    /// `force` 只认这个集合，不认账本状态：状态可被 verify 改写，future 的存活不可。
    pub(crate) fn begin_in_flight(&self, action_id: &str) -> InFlightGuard {
        *self
            .inner
            .in_flight
            .lock()
            .unwrap()
            .entry(action_id.to_string())
            .or_insert(0) += 1;
        InFlightGuard {
            inner: Arc::clone(&self.inner),
            action_id: action_id.to_string(),
        }
    }

    /// 当前仍在飞的动作 id 快照（force 的排除集）。
    pub(crate) fn active_in_flight(&self) -> Vec<String> {
        self.inner
            .in_flight
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }

    pub(crate) fn outbox_runtime(&self) -> &super::outbox::OutboxRuntime {
        &self.inner.outbox
    }

    /// 与动作账本共用的设备存储（任务 5/7 取用；投影层不读它）。
    pub fn store(&self) -> &DeviceStore {
        &self.inner.store
    }

    /// I2（2026-11-18）：五段计时行**异步落账**——回执等待路径零计时写库。
    ///
    /// 形态论证（brief 三选一，取最小改动）：`tokio::spawn` 每链一任务，行数据
    /// 所有权移交（row 已 owned、store 是 `Arc` 克隆/单位变体），零新组件零通道；
    /// 「回执返回后落账」需改 HTTP 层面回调面（侵入大），「有界 channel 刷写」需
    /// 常驻 writer 任务与背压语义（新增面大）。丢失窗口 = spawn 到落库之间（进程
    /// 崩溃等）——`finish` 的结构化日志行恒同步输出兜底（既有语义）。计时绝不参与
    /// 判定：落库失败只记日志（与本实现前一致）。生产动作链恒在 tokio 运行时内
    /// 调用 `finish`；无运行时上下文（防御面）退回同步落账。
    pub(crate) fn persist_action_timing_row(
        &self,
        row: crate::database::dao::opencode_action_timings::ActionTimingRow,
    ) {
        let store = self.store().clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                #[cfg(any(test, feature = "opencode-coordinator-tests"))]
                let hold = self.timing_persist_test_hold();
                #[cfg(not(any(test, feature = "opencode-coordinator-tests")))]
                let hold: Option<std::sync::Arc<super::budget::TimingPersistHold>> = None;
                handle.spawn(async move {
                    super::budget::pass_timing_persist_hold(hold).await;
                    if let Err(error) = store.with(|conn| {
                        crate::database::dao::opencode_action_timings::record_conn(conn, &row)
                    }) {
                        log::warn!("OC_ACTION_TIMING persist failed: {error}");
                    }
                });
            }
            // 防御面：无 tokio 运行时（生产动作链不会走到）⇒ 退回同步落账。
            Err(_) => {
                if let Err(error) = store.with(|conn| {
                    crate::database::dao::opencode_action_timings::record_conn(conn, &row)
                }) {
                    log::warn!("OC_ACTION_TIMING persist failed: {error}");
                }
            }
        }
    }

    /// 动作账本（**每进程一个实例**）：首次取用时构造，构造即崩溃恢复点
    /// （上一次进程遗留的 `dispatching` → `unconfirmed`，绝不重发）。
    ///
    /// 任务 5 的裁决：构造要写库（恢复在飞动作），所以它**不在** `new`/`with_sources`
    /// 里做——那两个构造仍是零 IO 的惰性容器；第一次派发/查询/核对时才建账本。
    pub(crate) fn action_store(&self) -> &Arc<NativeActionStore> {
        self.inner
            .actions
            .get_or_init(|| Arc::new(NativeActionStore::new(self.inner.store.clone())))
    }

    /// 当前代际的源（派发/核对路径用）：与投影同一条 HTTP 通道（`NativeClient`）。
    /// 会话感知：显式连接形态按目标会话的 argv 证据发现（登记缺失时）。
    pub(crate) async fn current_source_for(
        &self,
        session_id: &str,
    ) -> Result<Arc<Source>, NativeError> {
        self.inner.ensure_live()?;
        self.inner.source_for_session(session_id).await
    }

    /// 只读核对使用捕获来源；不重新发现或切换其他服务。
    pub(crate) fn snapshot_source(&self, snapshot: &DispatchSnapshot) -> Option<Arc<Source>> {
        let state = self.inner.state_lock();
        state
            .source
            .as_ref()
            .filter(|source| {
                source.descriptor.runtime == snapshot.target.runtime
                    && Arc::ptr_eq(&source.client, &snapshot.source_client)
                    && !source.client.is_invalidated()
            })
            .cloned()
    }

    #[cfg(test)]
    pub(crate) fn set_verify_before_store_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.inner.verify_before_store_test_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    pub(crate) fn run_verify_before_store_hook(&self) {
        let hook = self
            .inner
            .verify_before_store_test_hook
            .lock()
            .unwrap()
            .clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    pub(crate) fn observation_epoch(&self) -> u64 {
        self.inner.epoch()
    }

    /// 调用方持有 DeviceStore 时使用；source 校验与同步落账保持同一状态临界区。
    pub(crate) fn snapshot_process_current(&self, snapshot: &DispatchSnapshot) -> bool {
        (self.inner.process_fact)(snapshot.target.runtime.pid)
            == Some(snapshot.target.runtime.started_at_ms)
    }

    pub(crate) fn with_current_snapshot<T>(
        &self,
        snapshot: &DispatchSnapshot,
        epoch: u64,
        f: impl FnOnce() -> T,
    ) -> Option<T> {
        let state = self.inner.state_lock();
        if self.inner.epoch() != epoch
            || !state.source.as_ref().is_some_and(|source| {
                source.descriptor.runtime == snapshot.target.runtime
                    && Arc::ptr_eq(&source.client, &snapshot.source_client)
                    && !source.client.is_invalidated()
            })
        {
            return None;
        }
        Some(f())
    }

    /// 派发期的当前投影（一次只读重建，不发布修订）。
    ///
    /// 与 [`NativeHub::view`] 的差别：不发布 watch 修订、不拉起事件上游；只把「当前绑定 +
    /// 能力 + pending + inbox + 带 id 的消息记录」交给派发路径做最终复核。
    pub(crate) async fn dispatch_snapshot(
        &self,
        session: &Session,
    ) -> Result<DispatchSnapshot, NativeError> {
        self.dispatch_snapshot_pinned(session, None).await
    }

    pub(crate) async fn dispatch_snapshot_pinned(
        &self,
        session: &Session,
        expected: Option<&NativeTarget>,
    ) -> Result<DispatchSnapshot, NativeError> {
        self.inner.ensure_live()?;
        let raw = self.inner.refresh_raw_pinned(session, expected).await?;
        Ok(DispatchSnapshot {
            source_client: raw.source_client,
            target: raw.target,
            capabilities: raw.capabilities,
            binding_token: raw.binding_token,
            records: raw.records,
            complete: raw.complete,
            pending: raw.pending,
            permissions_trusted: raw.permissions_trusted,
            inbox: raw.inbox,
        })
    }

    /// 校验客户端保存的绑定是否仍指向同一固定目标；令牌只用于识别，不替代设备认证。
    pub(crate) fn binding_matches(&self, target: &NativeTarget, token: &str) -> bool {
        !token.is_empty() && self.inner.binding_token(target) == token
    }

    /// 未认领的只读视图也可凭本进程刚发布的快照保持同一目标。
    pub(crate) fn cached_target_for_binding(
        &self,
        session_id: &str,
        token: &str,
    ) -> Option<NativeTarget> {
        let state = self.inner.state_lock();
        let target = state.sessions.get(session_id)?.selected_target.as_ref()?;
        self.binding_matches(target, token).then(|| target.clone())
    }

    /// 一次状态锁内恢复同一已选目标及其会话上下文，不从新扫描卡补目录。
    pub(crate) fn cached_context_for_binding(
        &self,
        session_id: &str,
        token: &str,
    ) -> Option<(Session, NativeTarget)> {
        let state = self.inner.state_lock();
        let entry = state.sessions.get(session_id)?;
        let target = entry.selected_target.as_ref()?;
        if !self.binding_matches(target, token) {
            return None;
        }
        Some((entry.selected_session.as_ref()?.clone(), target.clone()))
    }

    /// 只读订阅也必须绑定原目标；发现换代不得安装或订阅新服务。
    pub(crate) async fn validate_pinned_subscription(
        &self,
        session: &Session,
        expected: &NativeTarget,
        require_active: bool,
    ) -> Result<(), NativeError> {
        let snapshot = self
            .dispatch_snapshot_pinned(session, Some(expected))
            .await?;
        let _ = self
            .inner
            .view_ownership_for(&snapshot.target, true)
            .await?;
        if !self.pinned_subscription_current(expected, require_active) {
            return Err(NativeError::new(
                codes::OC_BINDING_STALE,
                "the fixed native route changed",
            ));
        }
        Ok(())
    }

    /// 供事件流每次发出修订前检查，路由或 source 变化即结束旧订阅。
    pub(crate) fn pinned_subscription_current(
        &self,
        expected: &NativeTarget,
        require_active: bool,
    ) -> bool {
        self.inner.store.with(|conn| {
            let state = self.inner.state_lock();
            let route_ok = match crate::database::dao::opencode_actions::route_conn(
                conn,
                &expected.session_id,
            ) {
                Ok(Some(route)) => {
                    route.route_state == crate::database::dao::opencode_actions::ROUTE_ACTIVE
                        && route.same_runtime(expected)
                }
                Ok(None) if !require_active => state
                    .sessions
                    .get(&expected.session_id)
                    .and_then(|entry| entry.selected_target.as_ref())
                    .is_some_and(|target| target == expected),
                _ => false,
            };
            route_ok
                && state
                    .source
                    .as_ref()
                    .is_some_and(|source| source.descriptor.runtime == expected.runtime)
        })
    }

    /// 在 watch 已登记并启动 pump 后固定连接实例；不能仅用服务 PID 代表连接。
    pub(crate) fn capture_subscription_pin(
        &self,
        expected: &NativeTarget,
        require_active: bool,
    ) -> Option<Arc<NativeSubscriptionPin>> {
        if !self.pinned_subscription_current(expected, require_active) {
            return None;
        }
        let state = self.inner.state_lock();
        let source = state.source.as_ref()?;
        let pump = state.pump.as_ref()?;
        if source.descriptor.runtime != expected.runtime
            || source.client.is_invalidated()
            || pump.generation != expected.runtime
            || *pump.cancel.borrow()
        {
            return None;
        }
        Some(Arc::new(NativeSubscriptionPin {
            target: expected.clone(),
            require_active,
            client: Arc::clone(&source.client),
            pump: Arc::clone(&pump.cancel),
        }))
    }

    /// 同时确认固定目标、连接和上游实例仍有效；任一失效就结束下游流。
    pub(crate) fn subscription_pin_current(&self, pin: &NativeSubscriptionPin) -> bool {
        self.inner.store.with(|conn| {
            let state = self.inner.state_lock();
            let route_ok = match crate::database::dao::opencode_actions::route_conn(
                conn,
                &pin.target.session_id,
            ) {
                Ok(Some(route)) => {
                    route.route_state == crate::database::dao::opencode_actions::ROUTE_ACTIVE
                        && route.same_runtime(&pin.target)
                }
                Ok(None) if !pin.require_active => state
                    .sessions
                    .get(&pin.target.session_id)
                    .and_then(|entry| entry.selected_target.as_ref())
                    .is_some_and(|target| *target == pin.target),
                _ => false,
            };
            route_ok
                && state.source.as_ref().is_some_and(|source| {
                    source.descriptor.runtime == pin.target.runtime
                        && Arc::ptr_eq(&source.client, &pin.client)
                        && !source.client.is_invalidated()
                })
                && state.pump.as_ref().is_some_and(|pump| {
                    pump.generation == pin.target.runtime
                        && Arc::ptr_eq(&pump.cancel, &pin.pump)
                        && !*pump.cancel.borrow()
                })
        })
    }

    /// **原生表单结果**（R5b）：按原 `session + formId` 只读回读官方状态/答案。
    ///
    /// 纪律：
    /// * 先用 [`NativeHub::dispatch_snapshot`] 走一次与派发同源的绑定/代际/目录核对，
    ///   拿到的 `target` 才用于 detail 读——服务未登记/代际不符/会话目录不匹配 ⇒ 整体
    ///   失败，不猜目标；
    /// * detail 端点带权威 `state.status` / `state.answer`（列表读没有），且其 `spec.id`
    ///   必须与请求的 `form_id` 逐字相等（服务回错表单 ⇒ 失败，不串结果）；
    /// * **只读**：不读写动作账本、不写路由归属、不发布修订；未知/读不到都是错误，
    ///   由路由层统一映射为 404（不泄露表单是否存在）。
    ///
    /// 这条路径**不依赖** `metadata.tool`：没有 tool 关联的表单同样可回读（R5b 的目的）。
    pub async fn form_result(
        &self,
        session: &Session,
        form_id: &str,
    ) -> Result<NativeFormResult, NativeError> {
        self.form_result_pinned(session, form_id, None).await
    }

    pub(crate) async fn form_result_pinned(
        &self,
        session: &Session,
        form_id: &str,
        expected: Option<&NativeTarget>,
    ) -> Result<NativeFormResult, NativeError> {
        self.inner.ensure_live()?;
        let form_id = form_id.trim();
        if form_id.is_empty() {
            return Err(NativeError::new(codes::OC_PATH_INVALID, "form id is empty"));
        }
        // 监听代际：detail 是长读，等待期间可能发生远程 pause/重开；返回后必须
        // 再验 epoch，不能凭“请求前 source 相同”把旧监听/旧答案当当前结果。
        let epoch = self.inner.epoch();
        let snapshot = self.dispatch_snapshot_pinned(session, expected).await?;
        let source = self.snapshot_source(&snapshot).ok_or_else(|| {
            NativeError::new(
                codes::OC_RESPONSE_INVALID,
                "native source changed during form result read",
            )
        })?;
        let detail = read_form_detail(&source.client, &snapshot.target, form_id).await?;
        // 终验（长读后）：用捕获的同一 client 重读 `/api/info` 核对服务身份——
        // 同端点换接收方时 client 指针/PID 事实都可能仍成立，只有端点自报能分辨。
        reverify_write_channel_identity(&source).await?;
        if !self.snapshot_process_current(&snapshot)
            || self
                .with_current_snapshot(&snapshot, epoch, || ())
                .is_none()
        {
            return Err(NativeError::new(
                codes::OC_RESPONSE_INVALID,
                "native source changed while reading form result",
            ));
        }
        // 服务回错表单（id 不一致）不能把它的答案挂到请求的 formId 上。
        if detail.spec.id != form_id {
            return Err(NativeError::new(
                codes::OC_FORM_INVALID,
                "form detail does not match the requested id",
            ));
        }
        Ok(form_result_from(
            &snapshot.target.session_id,
            form_id,
            &detail.spec,
            detail.status.as_deref(),
            detail.answer.as_ref(),
            detail
                .tool_link
                .as_ref()
                .map(|(message_id, call_id)| (message_id.as_str(), call_id.as_str())),
        ))
    }

    /// 权威只读视图：一次成功调用 = 一次重建，返回的 `revision` 与 watch 发布值一致。
    pub async fn view(&self, session: &Session) -> Result<PublicNativeView, NativeError> {
        self.inner.ensure_live()?;
        // 本次投影所属的监听代际：pause/shutdown 之后 commit 必须拒绝这份旧结果
        //（F2：旧监听在途 view 不得发布到新监听）。
        let epoch = self.inner.epoch();
        // RAII：panic 也必须归还在飞计数，否则释放逻辑会永远认为「还有投影在跑」。
        let _in_flight = ViewGuard::new(&self.inner.views);
        let snapshot = self.inner.refresh(session).await?;
        // R2：绑定状态由服务身份核验给出（多 service 保守闸 / 代际漂移 ⇒ true）。
        let verified =
            view_ownership_status(self.inner.verify_service_identity(&snapshot.target).await)?;
        let view = self.inner.commit(snapshot, epoch, false, !verified)?;
        self.inner.ensure_pump();
        Ok(view)
    }

    /// 只读 agent 视图：当前会话 agent + 当前项目可切换的 primary agent 列表。
    ///
    /// 目标校验与 view 同源：先走 `refresh_raw`（服务/会话/目录），再确认读用的
    /// source 与投影同代际；会话 agent 读不到时返回 `None`，不把默认值猜成事实。
    pub async fn agent_view(&self, session: &Session) -> Result<NativeAgentView, NativeError> {
        self.agent_view_pinned(session, None).await
    }

    pub(crate) async fn agent_view_pinned(
        &self,
        session: &Session,
        expected: Option<&NativeTarget>,
    ) -> Result<NativeAgentView, NativeError> {
        self.inner.ensure_live()?;
        let epoch = self.inner.epoch();
        let raw = self.inner.refresh_raw_pinned(session, expected).await?;
        let source = {
            let state = self.inner.state_lock();
            state
                .source
                .as_ref()
                .filter(|source| {
                    self.inner.epoch() == epoch
                        && source.descriptor.runtime == raw.target.runtime
                        && Arc::ptr_eq(&source.client, &raw.source_client)
                        && !source.client.is_invalidated()
                })
                .cloned()
        }
        .ok_or_else(|| NativeError::new(codes::OC_BINDING_STALE, "agent view source changed"))?;
        let session_path = format!("/api/session/{}", raw.target.session_id);
        let value = source.client.read_json(&session_path, &[]).await?;
        let root = value.get("data").unwrap_or(&value);
        let directory = root
            .pointer("/location/directory")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                NativeError::new(
                    codes::OC_DIRECTORY_UNRESOLVED,
                    "session response has no directory",
                )
            })?;
        let canonical = std::fs::canonicalize(directory).map_err(|_| {
            NativeError::new(
                codes::OC_DIRECTORY_UNRESOLVED,
                "session directory unresolved",
            )
        })?;
        let expected = std::fs::canonicalize(&raw.target.directory).map_err(|_| {
            NativeError::new(
                codes::OC_DIRECTORY_UNRESOLVED,
                "target directory unresolved",
            )
        })?;
        if canonical != expected {
            return Err(NativeError::new(
                codes::OC_DIRECTORY_MISMATCH,
                "agent view session directory mismatch",
            ));
        }
        let current = root.get("agent").and_then(Value::as_str).map(str::to_owned);
        let model = session_model_from(&value);
        let query = [(
            SCOPE_QUERY_KEY.to_string(),
            raw.target.directory.to_string_lossy().into_owned(),
        )];
        let list = source.client.read_json("/api/agent", &query).await?;
        let options = list
            .get("data")
            .and_then(Value::as_array)
            .map(|agents| {
                agents
                    .iter()
                    .filter(|agent| agent.get("mode").and_then(Value::as_str) == Some("primary"))
                    .filter(|agent| agent.get("hidden").and_then(Value::as_bool) != Some(true))
                    .filter(|agent| {
                        agent
                            .get("id")
                            .and_then(Value::as_str)
                            .is_some_and(agent_id_shape_ok)
                    })
                    .filter_map(|agent| {
                        let id = agent.get("id")?.as_str()?.to_string();
                        let name = agent
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or(&id)
                            .to_string();
                        Some(NativeAgentOption {
                            id,
                            name,
                            description: agent
                                .get("description")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        // 展示名解析也是**额外一次读**：必须发生在身份终验**之前**，否则目录读回后服务
        // 换代，这份展示名就不再被任何终验覆盖（挂在旧代际上的展示信息）。
        let model_name = match model.as_ref() {
            Some(reference) => model_display_name(&source.client, reference).await,
            None => None,
        };
        // 终验（长读后）：用捕获的同一 client 重读 `/api/info` 核对服务身份——
        // 同端点换接收方时 client 指针/PID 事实都可能仍成立，只有端点自报能分辨。
        reverify_write_channel_identity(&source).await?;
        if (self.inner.process_fact)(raw.target.runtime.pid)
            != Some(raw.target.runtime.started_at_ms)
        {
            return Err(NativeError::new(
                codes::OC_BINDING_STALE,
                "agent view process changed",
            ));
        }
        let valid = {
            let state = self.inner.state_lock();
            self.inner.epoch() == epoch
                && state.source.as_ref().is_some_and(|source| {
                    source.descriptor.runtime == raw.target.runtime
                        && Arc::ptr_eq(&source.client, &raw.source_client)
                        && !source.client.is_invalidated()
                })
        };
        if !valid {
            return Err(NativeError::new(
                codes::OC_BINDING_STALE,
                "agent view source changed",
            ));
        }
        Ok(NativeAgentView {
            current,
            options,
            model,
            model_name,
        })
    }

    /// 原生新建首次认领。只认领已创建、已确认的目标与精确窗口，不替换既有绑定。
    pub(crate) async fn claim_created_route(
        &self,
        device: &str,
        request: &str,
        target: &NativeTarget,
        window: &super::binding_gate::LockedWindow,
    ) -> Result<String, NativeError> {
        use crate::database::dao::{
            opencode_actions as dao, opencode_creates, opencode_pinned_windows,
        };
        let store_error = |err: rusqlite::Error| {
            log::warn!("opencode created route: {err}");
            NativeError::new(
                super::actions::action_codes::STORE_FAILED,
                "native route unavailable",
            )
        };
        let stale = || {
            NativeError::new(
                codes::OC_BINDING_STALE,
                "the original created window has changed",
            )
        };
        self.inner.ensure_live()?;
        let epoch = self.inner.epoch();
        let _route_permit = self.legacy_lock(&target.session_id).write_owned().await;
        self.verify_target_preconditions(target).await?;
        self.inner.ensure_epoch(epoch)?;
        if self.is_probe_target(&target.session_id)
            || (self.inner.gate_tui_fact)(window.pid) != Some(window.started_at_ms)
        {
            return Err(stale());
        }
        self.inner.store.with(|conn| {
            let tx = conn.unchecked_transaction().map_err(&store_error)?;
            if !opencode_creates::get(&tx, device, request)
                .map_err(&store_error)?
                .is_some_and(|record| record.matches_bound_window(target, window))
            {
                return Err(stale());
            }
            let route = dao::route_conn(&tx, &target.session_id).map_err(&store_error)?;
            if route.as_ref().is_some_and(|current| {
                current.route_state != dao::ROUTE_ACTIVE || !current.same_runtime(target)
            }) {
                return Err(stale());
            }
            opencode_pinned_windows::ensure_table_conn(&tx).map_err(&store_error)?;
            let pin = HubInner::pinned_row_for(target, window);
            if let Some(existing) =
                opencode_pinned_windows::pinned_window_conn(&tx, &target.session_id)
                    .map_err(&store_error)?
            {
                if existing != pin {
                    return Err(stale());
                }
            }
            if !dao::claim_route_and_pin_in_transaction(
                &tx,
                target,
                true,
                chrono::Utc::now().timestamp_millis(),
                false,
                Some(&route),
                Some(&pin),
            )
            .map_err(&store_error)?
            {
                return Err(NativeError::new(
                    codes::OC_ROUTE_NOT_OWNED,
                    "native route has pending work",
                ));
            }
            tx.commit().map_err(&store_error)
        })?;
        self.inner.record_pinned_window_in_registry(target, window);
        Ok(self.binding_token_for(target))
    }

    /// 候选路由入口（任务 8 路由层专用）：一次完整只读投影 + 按需原子认领原生归属。
    ///
    /// 次序刻意（勿重排）：
    /// 1. **投影**（HTTP 只读）：给出绑定 target、逐操作能力与视图内容——认领要用的
    ///    `NativeTarget` 只能来自这次成功投影（服务未登记/代际不符/目录不匹配 ⇒ 整体
    ///    失败，零认领）；
    /// 2. **认领**（`claim=true` 且服务身份核验通过时）：[`NativeHub::select_native_route`]
    ///    持同会话 write permit（等所有在飞旧写结束后）在同一事务内复核旧 pending /
    ///    未决 native 动作并原子登记 `active`；`claim=false`（验证开关关闭）或**服务
    ///    身份未证**（R2 门）时只读不认领——撤回新原生写权限，既有归属原样报告；
    /// 3. **发布修订**（commit + ensure_pump）：与 [`NativeHub::view`] 同一收尾。
    pub async fn view_and_claim(
        &self,
        session: &Session,
        claim: bool,
    ) -> Result<(PublicNativeView, RouteClaim), NativeError> {
        self.view_and_claim_pinned(session, claim, None).await
    }

    pub(crate) async fn view_and_claim_pinned(
        &self,
        session: &Session,
        claim: bool,
        expected: Option<&NativeTarget>,
    ) -> Result<(PublicNativeView, RouteClaim), NativeError> {
        self.view_and_claim_bound(session, claim, expected, expected.is_some())
            .await
    }

    pub(crate) async fn view_and_claim_bound(
        &self,
        session: &Session,
        claim: bool,
        expected: Option<&NativeTarget>,
        require_active: bool,
    ) -> Result<(PublicNativeView, RouteClaim), NativeError> {
        self.inner.ensure_live()?;
        let epoch = self.inner.epoch();
        let _in_flight = ViewGuard::new(&self.inner.views);
        let route_before = self.inner.store.with(|conn| {
            crate::database::dao::opencode_actions::route_conn(conn, &session.id).map_err(|_| {
                NativeError::new(
                    super::actions::action_codes::STORE_FAILED,
                    "native route unavailable",
                )
            })
        })?;
        if let Some(expected) = expected {
            let valid = match route_before.as_ref() {
                Some(route) => {
                    route.route_state == crate::database::dao::opencode_actions::ROUTE_ACTIVE
                        && route.same_runtime(expected)
                }
                None => !require_active,
            };
            if !valid {
                return Err(NativeError::new(
                    codes::OC_BINDING_STALE,
                    "the fixed native route changed",
                ));
            }
        }
        let snapshot = self.inner.refresh_pinned(session, expected).await?;
        // 旧监听的只读结果连「认领路由」也不得写：先验代际再进入 DB 事务。
        self.inner.ensure_epoch(epoch)?;
        // 固定目标可在原服务仍可核验时只读；写权仍由独立归属门决定。
        let verified = self
            .inner
            .view_ownership_for(&snapshot.target, expected.is_some())
            .await?;
        // F12 项 1：认领走写权链时捕获回声验证通过的窗口身份（门装配的生产路径此值
        // 为 None——view 认领零远端写；测试逃逸形态有 echo 时优先用它提交绑定）。
        let mut verified_window = None;
        if claim && verified {
            verified_window = self
                .verify_target_identity_capturing(&snapshot.target)
                .await?;
        }
        // 路由改变与视图发布互斥；不能让等待期间的旧投影覆盖新路由。
        let _route_permit = self
            .legacy_lock(&snapshot.target.session_id)
            .write_owned()
            .await;
        self.inner.ensure_epoch(epoch)?;
        // 等锁期间外部服务可能换代，而内存 source 与路由都尚未变化。
        // H03（复审 N1/N2）：读取/认领路径**零远端写副作用**——GET /opencode/view
        // 不做 echo 挑战（上一轮的「认领同链收口」收回；echo 写权证明只在写操作与
        // 显式操作边界完成）。门装配后：
        //   * 认领依据回到「一次成功投影」（R4 既有契约：打开原生视图即选择原生
        //     路由）；归属证明逐次由写权链（execute 预留前 + dispatch 派发权前，
        //     含 ACK 后终验）承担——写不可用只由逐次门拒绝如实呈现；
        //   * 展示态与逐次写权分离：active/新认领路由不再因旧的无条件拒绝发布
        //     bindingRequired（N1：第二次 GET / 泵更新不得把健康路由重新宣告不可写）；
        //   * 探针目标是受保护对象：本地检查即拒绝认领（零远端 IO，I4）。
        let legacy_verified = self
            .inner
            .view_ownership_for(&snapshot.target, expected.is_some())
            .await?;
        self.inner.ensure_epoch(epoch)?;
        if self.inner.binding_gate.is_some() {
            let lookup = self.inner.locked_window_for_write(&snapshot.target);
            let locked = lookup(&snapshot.target.session_id);
            self.inner
                .reject_reloaded_window(&snapshot.target, locked.as_ref())?;
        }
        let (verified, declined_override) = match self.inner.binding_gate.as_ref() {
            Some(gate) => {
                let probe_target = gate.is_probe_session(&snapshot.target.session_id);
                (
                    !probe_target,
                    probe_target.then(|| {
                        super::binding_gate::reason_static(codes::OC_GATE_PROBE_TARGET)
                            .unwrap_or(codes::OC_GATE_PROBE_TARGET)
                    }),
                )
            }
            None => (legacy_verified, None),
        };
        #[cfg(test)]
        {
            let hold = self.inner.before_claim_test_hold.lock().unwrap().clone();
            if let Some(hold) = hold {
                hold.pass().await;
            }
        }
        // 所有可阻塞的进程扫描和动作账本初始化都在双锁外完成。
        let desktop_bound =
            desktop_client_bound(&snapshot.target.session_id, &snapshot.service_url);
        let _ = self.action_store();
        let snapshot = Arc::new(snapshot);
        let (view, route_claim, _claimed_now, committed_window) =
            self.inner.store.with(|conn| {
                // 锁顺序固定：legacy write → DeviceStore → source 状态。
                let mut state = self.inner.state_lock();
                self.inner.ensure_live()?;
                self.inner.ensure_epoch(epoch)?;
                if !state.source.as_ref().is_some_and(|source| {
                    source.descriptor.runtime == snapshot.target.runtime
                        && Arc::ptr_eq(&source.client, &snapshot.source_client)
                        && !source.client.is_invalidated()
                }) {
                    return Err(NativeError::new(
                        codes::OC_GENERATION_MISMATCH,
                        "native source changed during projection",
                    ));
                }
                let current = crate::database::dao::opencode_actions::route_conn(conn, &session.id)
                    .map_err(|_| {
                        NativeError::new(
                            super::actions::action_codes::STORE_FAILED,
                            "native route unavailable",
                        )
                    })?;
                if current != route_before {
                    return Err(NativeError::new(
                        codes::OC_BINDING_STALE,
                        "native route changed during projection",
                    ));
                }
                // 恢复的固定目标只确认既有归属，不允许重新认领或换代。
                let mut committed_window = None;
                let claimed = if claim && verified && expected.is_none() {
                    // F12 项 1：路由 CAS 与窗口绑定行**同事务**提交（绑定写不进 → 路由一并
                    // 回滚）。窗口身份优先用回声验证通过的那个；无 echo 证据时回退新鲜观察
                    // 候选捕获（本地读，view 零远端写语义不变）。捕获失败不写绑定行——
                    // 旧行保留，读取侧判据兜底。同代际活跃路由的刷新（same_route）不算
                    // 选择：不写绑定、不递增版本。
                    let same_route = route_before
                        .as_ref()
                        .is_some_and(|route| route.same_runtime(&snapshot.target));
                    let window_for_pin = if !same_route {
                        verified_window.clone().or_else(|| {
                            (self.inner.gate_locked_window)(&snapshot.target.session_id)
                        })
                    } else {
                        None
                    };
                    let pinned_row = window_for_pin
                        .as_ref()
                        .map(|window| HubInner::pinned_row_for(&snapshot.target, window));
                    let claimed =
                        crate::database::dao::opencode_actions::claim_route_and_pin_checked_conn(
                            conn,
                            &snapshot.target,
                            true,
                            chrono::Utc::now().timestamp_millis(),
                            false,
                            Some(&route_before),
                            pinned_row.as_ref(),
                        )
                        .map_err(|error| match error {
                            rusqlite::Error::QueryReturnedNoRows => NativeError::new(
                                codes::OC_BINDING_STALE,
                                "native route changed during projection",
                            ),
                            _ => NativeError::new(
                                super::actions::action_codes::STORE_FAILED,
                                "native route unavailable",
                            ),
                        })?;
                    if claimed {
                        committed_window = window_for_pin;
                    }
                    claimed
                } else {
                    false
                };
                #[cfg(test)]
                {
                    let hold = self.inner.after_claim_test_hold.lock().unwrap().clone();
                    if let Some(hold) = hold {
                        tokio::task::block_in_place(|| hold.pass());
                    }
                }
                let owned = claimed
                    || current.as_ref().is_some_and(|route| {
                        route.route_state == crate::database::dao::opencode_actions::ROUTE_ACTIVE
                    });
                let declined_reason = if let Some(reason) = declined_override {
                    // 探针目标认领拒绝：原因词 = 具体门受控码（可区分，本地检查零 IO）。
                    Some(reason)
                } else if claim && !verified && self.inner.binding_gate.is_none() {
                    // 门未装配（注入构造既有用例）：保持 B1 展示语义；门装配后归属证明
                    // 逐次由写权链承担，不再因旧的无条件拒绝发布「归属未证」（N1）。
                    Some(codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED)
                } else if claim && !claimed && !owned {
                    if current.as_ref().is_some_and(|route| {
                        route.route_state == crate::database::dao::opencode_actions::ROUTE_DRAINING
                    }) {
                        Some(ROUTE_DECLINED_RETURNED)
                    } else {
                        Some(ROUTE_DECLINED_LEGACY_PENDING)
                    }
                } else {
                    None
                };
                let route_version =
                    crate::database::dao::opencode_actions::route_conn(conn, &session.id)
                        .map_err(|_| {
                            NativeError::new(
                                super::actions::action_codes::STORE_FAILED,
                                "native route unavailable",
                            )
                        })?
                        .map(|route| route.route_version);
                let view = HubInner::publish_locked(
                    &mut state,
                    Arc::clone(&snapshot),
                    false,
                    !verified,
                    desktop_bound,
                );
                Ok((
                    view,
                    RouteClaim {
                        owned,
                        declined_reason,
                        route_version,
                    },
                    claimed,
                    committed_window,
                ))
            })?;
        // F12 项 1：绑定行已随路由 CAS 在同一事务落库（见上方 claim 分支）；此处只把
        // **同一**窗口身份同步进内存注册表（伴表失败已随事务回滚，注册表与账本同源）。
        // 同代际刷新（committed_window = None）不写注册表——绝不因刷新自动换窗。
        if let Some(window) = committed_window {
            self.inner
                .record_pinned_window_in_registry(&snapshot.target, &window);
        }
        self.inner.ensure_pump();
        Ok((view, route_claim))
    }

    /// **显式重新进入原生交互**（评审 R-2）：`POST /opencode/route/native` 的唯一实现。
    ///
    /// 与 [`NativeHub::view_and_claim`] 的差别只有两点：
    /// 1. 它**总是**尝试认领（用户刚点了「重新进入」，不是读请求的副作用）；
    /// 2. 它允许把 `draining` 拉回 `active`（`allow_reactivate=true`）。
    ///
    /// 其余规则完全复用：先做一次成功投影拿到 `NativeTarget`（绑定/代际/契约不符即失败、
    /// 零写），再由 `try_claim_route` 在同一事务里复核旧 `inject_queue` 未决行与未决动作。
    pub async fn reenter_native_route(
        &self,
        session: &Session,
        expected_binding: Option<&str>,
    ) -> Result<Option<String>, NativeError> {
        Ok(self
            .reenter_native_route_versioned(session, expected_binding, None, None)
            .await?
            .map(|(token, _)| token))
    }

    /// 候选预览（**只读**）：为「重新选择当前目标」固定用户看到的目标。
    ///
    /// 发现 → 复用同代际缓存连接或临时建连（**不安装 source**）→ `/api/info` + 会话读 →
    /// `bind_session` 三重核对 → 进程事实复核。全程零状态副作用：不作废旧连接、不清快照、
    /// 不写路由行，只发生 GET 读。签发的候选绑定令牌在 TTL 内、同设备、同监听代际才可用于
    /// `POST /opencode/route/native` 的 `candidateToken` 确认。
    pub async fn preview_candidate(
        &self,
        session: &Session,
        device_id: &str,
    ) -> Result<CandidatePreview, NativeError> {
        self.inner.preview_candidate(session, device_id).await
    }

    pub(crate) fn confirm_create_candidate(
        &self,
        session: &Session,
        device_id: &str,
        token: &str,
        service: &ServiceDescriptor,
    ) -> Result<(), NativeError> {
        let (target, preview, _) =
            self.inner
                .candidate_pin(token, device_id, &session.id, self.inner.epoch())?;
        if target.directory != Path::new(&session.project_path)
            || !same_service_descriptor(&preview, service)
        {
            return Err(NativeError::new(
                codes::OC_BINDING_STALE,
                "the previewed creation target changed",
            ));
        }
        Ok(())
    }

    /// 仅协调器测试：缩短预览 TTL，避免过期用例真实等待。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    pub fn set_candidate_preview_ttl_ms_for_tests(&self, ttl_ms: i64) {
        self.inner.set_candidate_preview_ttl_ms_for_tests(ttl_ms);
    }

    /// 仅测试缝：模拟 Tuvis 进程重启对 N1 窗口绑定注册表的清空（内存态丢失；
    /// 持久化绑定身份不受影响）。生产重启是新进程，注册表天然为空，本缝只服务
    /// 「同一进程内模拟重启后绑定行为」的回归用例（M1）。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    pub fn clear_pinned_windows_for_restart_tests(&self) {
        self.inner.pinned_windows.lock().unwrap().clear();
    }

    /// 仅测试缝：只注入「当前新鲜 `Opened`」锁定窗口查询（不装配 echo 门）。
    ///
    /// 与 [`Self::arm_binding_gate_for_tests`] 的差别：echo-challenge 门保持未装配
    ///（verify 走 coordinator 测试逃逸），只让选择事件/提交路径能从「新鲜观察」捕获
    /// 窗口身份——服务 F12 项 1 的原子提交/回声窗口直传/派发 fail-closed 单元与
    /// 服务级回归（生产中该查询由真实门装配提供，测试逃逸形态不改变任何判据）。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    pub fn set_locked_window_provider_for_tests(
        &mut self,
        lookup: super::binding_gate::LockedWindowLookup,
    ) {
        let inner = Arc::get_mut(&mut self.inner).expect("test hub must not be shared");
        inner.gate_locked_window = lookup;
    }

    /// 目标绑定令牌（只读核对端点用）：与投影派生同一算法，供丢回执恢复核对。
    pub(crate) fn binding_token_for(&self, target: &NativeTarget) -> String {
        self.inner.binding_token(target)
    }

    pub(crate) async fn reenter_native_route_versioned(
        &self,
        session: &Session,
        expected_binding: Option<&str>,
        expected_version: Option<i64>,
        candidate: Option<(&str, &str)>,
    ) -> Result<Option<(String, i64)>, NativeError> {
        self.reenter_native_route_recorded(
            session,
            expected_binding,
            expected_version,
            candidate,
            None,
        )
        .await
    }

    pub(crate) async fn reenter_native_route_recorded(
        &self,
        session: &Session,
        expected_binding: Option<&str>,
        expected_version: Option<i64>,
        candidate: Option<(&str, &str)>,
        request: Option<(&str, &str)>,
    ) -> Result<Option<(String, i64)>, NativeError> {
        self.inner.ensure_live()?;
        let epoch = self.inner.epoch();
        let request_id = request.map(|(_, id)| id).unwrap_or("legacy");
        let started = std::time::Instant::now();
        let route_before = self
            .inner
            .store
            .with(|conn| crate::database::dao::opencode_actions::route_conn(conn, &session.id))
            .map_err(|_| {
                NativeError::new(
                    super::actions::action_codes::STORE_FAILED,
                    "native route unavailable",
                )
            })?;
        if let Some(route) = &route_before {
            if expected_version.is_some_and(|version| version != route.route_version) {
                return Err(NativeError::new(
                    codes::OC_BINDING_STALE,
                    "the selected native route changed",
                ));
            }
            let target = NativeTarget {
                runtime: RuntimeKey {
                    service_id: route.service_id.clone(),
                    pid: u32::try_from(route.runtime_pid).map_err(|_| {
                        NativeError::new(codes::OC_BINDING_STALE, "native route changed")
                    })?,
                    started_at_ms: route.runtime_started_at_ms,
                },
                session_id: session.id.clone(),
                directory: std::path::PathBuf::from(&route.directory),
            };
            if !expected_binding.is_some_and(|token| self.binding_matches(&target, token)) {
                // D-N5-3（候选重启 + 服务换代 → 残留 active 路由重选永久 409 死锁）：
                // binding token 派生含**本进程内随机锚**（`binding_token`），候选应用重启后
                // 旧 token 随旧 secret 消亡、永不可再生；若路由行锁定的代际已**可证明死亡**
                // （G3 三形态：事实机制明确报告无此 pid，或 pid 在场但 lstart 不匹配），
                // 旧代际不可能再有活体写者——本函数就是显式重选端点，放行后候选预览复核/
                // 身份终验/进程事实复核/路由行 CAS（`claim_route_checked_conn`
                // expected=Some(&route_before)）仍然逐闸核验，显式重选按既有语义原子换代
                // 登记新绑定（N1 捕获点）。
                // 同 pid 存活（死亡不可证明）维持拒绝，fail-closed 不变；G3 收紧同步生效：
                // 事实**读取失败**（三形态缝的 ReadFailure，含两态缝保守桥接的 None）不再
                // 冒充死亡证明——显式重选此时维持 OC_BINDING_STALE（不回退放宽）。
                let provably_dead = match (self.inner.generation_fact)(target.runtime.pid) {
                    // 机制明确报告无此 pid ⇒ 退出可证明。
                    super::actions::ProcessFactProbe::NoSuchProcess => true,
                    // pid 在场但 lstart≠账本 ⇒ PID 已复用（换代）可证明。
                    super::actions::ProcessFactProbe::Alive(started) => {
                        started != target.runtime.started_at_ms
                    }
                    // 通道不可用 ⇒ 死亡不可证明：维持拒绝（G3 收紧，fail-closed）。
                    super::actions::ProcessFactProbe::ReadFailure => false,
                };
                if !provably_dead {
                    return Err(NativeError::new(
                        codes::OC_BINDING_STALE,
                        "the selected native binding changed",
                    ));
                }
            }
        } else if let Some(token) = expected_binding {
            if expected_version.is_some()
                || self.cached_target_for_binding(&session.id, token).is_none()
            {
                return Err(NativeError::new(
                    codes::OC_BINDING_STALE,
                    "the selected native binding changed",
                ));
            }
        }
        // 预览凭据先于任何发现/写入取用：候选过期或跨设备/跨代际立即保守拒绝，零变更。
        let pin = match candidate {
            Some((token, device_id)) => {
                Some(
                    self.inner
                        .candidate_pin(token, device_id, &session.id, epoch)?,
                )
            }
            None => None,
        };
        // 发现比对必须发生在任何读取之前：登记改指别的服务时直接拒绝，绝不把
        // 新服务的连接安装进来（那会作废原连接并清快照，破坏现有只读）。
        if let Some((_, pin_descriptor, _)) = &pin {
            let descriptor_now = self.inner.discover_for_session(&session.id).await?;
            if !same_service_descriptor(&descriptor_now, pin_descriptor) {
                return Err(NativeError::new(
                    codes::OC_BINDING_STALE,
                    "the previewed native candidate changed",
                ));
            }
        }
        // 候选读取走**临时/缓存连接**：本函数在认领成功前绝不安装新 source、
        // 不作废原连接、不清快照——拒绝路径（身份未证/未决动作/读取失败）必须
        // 保留原路由目标的只读能力；换源只作为认领成功的既定效果（见下方 install）。
        let (candidate_source, installed_at_read) = {
            let descriptor = self.inner.discover_for_session(&session.id).await?;
            match self.inner.current(&descriptor.runtime) {
                Some(source) if same_service_descriptor(&source.descriptor, &descriptor) => {
                    let reused = Arc::clone(&source);
                    (reused, Some(source))
                }
                _ => {
                    let installed_now = self.inner.state_lock().source.clone();
                    let client = Arc::new(NativeClient::new(descriptor.clone())?);
                    let openapi = client.read_json(OPENAPI_PATH, &[]).await?;
                    // 能力表只承载结构性判定；写归属门在下面逐次核对，不在这里预判。
                    let source = Arc::new(Source {
                        descriptor,
                        client,
                        capabilities: evaluate_contract(&openapi),
                    });
                    (source, installed_now)
                }
            }
        };
        let descriptor_at_read = candidate_source.descriptor.clone();
        // 绑定目标只能来自这次成功读取（与 view 同源：info + 会话读 + 三重核对）。
        let info = candidate_source.client.read_json(INFO_PATH, &[]).await?;
        let session_path = format!("/api/session/{}", segment(&session.id)?);
        let native = candidate_source
            .client
            .read_json(&session_path, &[])
            .await?;
        let target = bind_session(session, &descriptor_at_read, &info, &native)?;
        // M1：候选核验用「新鲜候选优先」的锁定窗口（死绑定不得锁死恢复路径；
        // 判据本体不放宽——见 `reselect_window_lookup` / `verify_write_authority_using`）。
        log::info!(
            "opencode_reselect request_id={request_id} stage=candidate_read elapsed_ms={}",
            started.elapsed().as_millis()
        );
        let phase = std::time::Instant::now();
        let previewed_window = pin.as_ref().and_then(|(_, _, window)| window.as_ref());
        if pin.is_some() && self.inner.binding_gate.is_some() && previewed_window.is_none() {
            return Err(NativeError::new(
                codes::OC_BINDING_STALE,
                "the preview did not establish a current native window",
            ));
        }
        self.verify_target_identity_for_reselect(
            &target,
            &candidate_source.client,
            previewed_window,
        )
        .await?;
        log::info!(
            "opencode_reselect request_id={request_id} stage=ownership elapsed_ms={}",
            phase.elapsed().as_millis()
        );
        let phase = std::time::Instant::now();
        let _write = self.legacy_lock(&session.id).write_owned().await;
        log::info!(
            "opencode_reselect request_id={request_id} stage=lock_wait elapsed_ms={}",
            phase.elapsed().as_millis()
        );
        let phase = std::time::Instant::now();
        self.inner.ensure_epoch(epoch)?;
        // F12 项 1：捕获本轮（锁后复核）写权链回声验证通过的锁定窗口身份——M2 同轮
        // 单一身份产物，认领事务将原样落绑定行（提交的就是验证的那个窗口）。
        let echo_window = self
            .verify_target_identity_for_reselect(
                &target,
                &candidate_source.client,
                previewed_window,
            )
            .await?;
        if (self.inner.process_fact)(target.runtime.pid) != Some(target.runtime.started_at_ms) {
            return Err(NativeError::new(
                codes::OC_GENERATION_MISMATCH,
                "native source changed during selection",
            ));
        }
        // 终验（所有异步等待结束后、认领前）：用候选的同一连接重读 `/api/info`
        // 核对服务身份——等待期间同端点换接收方时，登记/PID/指针都可能仍成立。
        reverify_write_channel_identity(&candidate_source).await?;
        let latest_descriptor = self.inner.discover_for_session(&session.id).await?;
        if !same_service_descriptor(&latest_descriptor, &descriptor_at_read) {
            return Err(NativeError::new(
                codes::OC_BINDING_STALE,
                "native registration changed during selection",
            ));
        }
        // 提交点复核：当前读取的目标必须仍是预览候选——目标全等 + 同服务连接。
        // 登记改指别的服务（B→C）、目录变化或仅同代际换连接，一律拒绝并保留原路由。
        if let Some((pin_target, pin_descriptor, _)) = &pin {
            if target != *pin_target
                || !same_service_descriptor(&descriptor_at_read, pin_descriptor)
            {
                return Err(NativeError::new(
                    codes::OC_BINDING_STALE,
                    "the previewed native candidate changed",
                ));
            }
        }
        if let Some(window) = previewed_window {
            self.inner.recheck_previewed_window(&target, window)?;
        }
        log::info!(
            "opencode_reselect request_id={request_id} stage=final_verify elapsed_ms={}",
            phase.elapsed().as_millis()
        );
        let phase = std::time::Instant::now();
        let binding_token = self.inner.binding_token(&target);
        let result = self
            .inner
            .store
            .with(|conn| {
                let state = self.inner.state_lock();
                // 认领前守卫（临时读取不安装）：代际未变，且**我开始读取时在册的 source**
                // 仍是当前在册者（指针同源）——等待期间任何换源（含同代际换连接）都拒绝。
                // 不要求在册连接未作废：401 作废但未摘除的前驱不应阻断向新目标显式切换
                //（候选连接的有效性由上面的同通道终验独立核验）。
                if self.inner.epoch() != epoch {
                    return Err(rusqlite::Error::QueryReturnedNoRows);
                }
                let installed_ok = match (&installed_at_read, state.source.as_ref()) {
                    (None, None) => true,
                    (Some(read), Some(current)) => Arc::ptr_eq(&read.client, &current.client),
                    _ => false,
                };
                if !installed_ok {
                    return Err(rusqlite::Error::QueryReturnedNoRows);
                }
                // 候选连接自身的有效性独立检查：终验成功后、等待数据库锁期间，
                // 并发读取收到 401 仍可作废同一连接（不受状态锁保护）。已作废的
                // 候选绝不认领——否则 draining→active 已提交而连接已死。
                if candidate_source.client.is_invalidated()
                    || previewed_window.is_some_and(|window| {
                        self.inner
                            .recheck_previewed_window(&target, window)
                            .is_err()
                    })
                {
                    return Err(rusqlite::Error::QueryReturnedNoRows);
                }
                // 进程事实在取得数据库锁之后复核（与 verify 的锁后复核同口径）：
                // 等待锁期间 PID 退出或复用不得认领。
                if (self.inner.process_fact)(target.runtime.pid)
                    != Some(target.runtime.started_at_ms)
                {
                    return Err(rusqlite::Error::QueryReturnedNoRows);
                }
                // F12 项 1：路由 CAS 与窗口绑定行**同事务**提交。窗口身份 = 上面回声
                // 验证通过的那个（echo_window）；门未装配（测试逃逸）时回退新鲜观察
                // 捕获。捕获失败（此刻无新鲜 Opened）不写绑定行——旧行保留，读取侧
                // 判据兜底。绑定行写不进（约束冲突/注入错误）⇒ 整个事务回滚（路由
                // CAS 一并撤销，绝不半提交）。
                let window_for_pin = echo_window
                    .clone()
                    .or_else(|| (self.inner.gate_locked_window)(&target.session_id));
                let pinned_row = window_for_pin
                    .as_ref()
                    .map(|window| HubInner::pinned_row_for(&target, window));
                let tx = conn.unchecked_transaction()?;
                if request.is_some() && window_for_pin.is_none() {
                    return Ok(None);
                }
                let claimed =
                    crate::database::dao::opencode_actions::claim_route_and_pin_in_transaction(
                        &tx,
                        &target,
                        true,
                        chrono::Utc::now().timestamp_millis(),
                        true,
                        Some(&route_before),
                        pinned_row.as_ref(),
                    )?;
                if !claimed {
                    return Ok(None);
                }
                let version = crate::database::dao::opencode_actions::route_conn(&tx, &session.id)?
                    .map(|route| route.route_version)
                    .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
                if let Some((device, request_id)) = request {
                    use crate::database::dao::opencode_reselect::{self, Committed};
                    let receipt = Committed {
                        binding_token: binding_token.clone(),
                        route_version: version,
                        window: window_for_pin
                            .clone()
                            .ok_or(rusqlite::Error::QueryReturnedNoRows)?,
                    };
                    if !opencode_reselect::finish(
                        &tx,
                        device,
                        &session.id,
                        request_id,
                        Some(&receipt),
                    )? {
                        return Err(rusqlite::Error::QueryReturnedNoRows);
                    }
                }
                tx.commit()?;
                let source_revision = state.source_revision;
                Ok::<_, rusqlite::Error>(Some((version, source_revision, window_for_pin)))
            })
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => NativeError::new(
                    codes::OC_BINDING_STALE,
                    "native route changed during selection",
                ),
                _ => NativeError::new(
                    super::actions::action_codes::STORE_FAILED,
                    "native route unavailable",
                ),
            })?;
        log::info!(
            "opencode_reselect request_id={request_id} stage=commit elapsed_ms={}",
            phase.elapsed().as_millis()
        );
        if let Some((_, source_revision, _)) = &result {
            // 认领成功后才换源：作废旧连接/停旧 pump/清快照是路由变更的既定效果。
            // 版本 CAS：认领与换源两段临界区之间若已有别的 source 胜出（revision
            // 前进），保留胜者，不回写覆盖。代际前进（pause/shutdown）同理——路由
            // 提交已生效，读取侧随后按新路由惰性重装；不能向客户端谎报失败。
            if let Err(error) =
                self.inner
                    .install_guarded(candidate_source, epoch, Some(*source_revision), false)
            {
                log::warn!(
                    "路由认领成功但换源被跳过（source 已换代或代际前进）: {}",
                    error.code
                );
            }
            // F12 项 1：绑定行已随路由 CAS 在同一事务落库；此处把**同一**窗口身份
            // 同步进内存注册表（伴表失败已随事务回滚，注册表与账本同源）。显式重选
            // 是换窗的唯一合法途径（隐式刷新绝不换窗）；捕获失败（此刻无新鲜 Opened
            // 且无 echo 窗口）不写注册表，旧绑定按不适用回退。
            if let Some((_, _, Some(window))) = &result {
                self.inner.record_pinned_window_in_registry(&target, window);
            }
        }
        Ok(result.map(|(version, _, _)| (binding_token, version)))
    }

    /// F12 项 2 + G3 取用缝：三形态进程事实（只读）——原动作代际三态判定与
    /// patch5 死亡代际豁免的证据源（判定本体在 `actions::classify_original_generation_with`）。
    pub(crate) fn original_generation_fact(
        &self,
    ) -> &dyn Fn(u32) -> super::actions::ProcessFactProbe {
        &*self.inner.generation_fact
    }

    /// 当前服务代际（**只读**：登记文件 + 进程事实；不建连接、不发 HTTP、不写任何状态）。
    ///
    /// `None` = 服务未登记 / 进程事实不可得 —— 与发现层同一口径，**绝不猜代际**。
    /// 用途：强制交回时判定「哪些动作行已经无法再核对」（评审 Blocker 2）。
    pub(crate) fn current_runtime(&self) -> Option<RuntimeKey> {
        discover(
            &self.inner.input,
            &read_state_file,
            &*self.inner.process_fact,
        )
        .ok()
        .map(|descriptor| descriptor.runtime)
    }

    /// 上传拒绝码面判定（D-N5-1）：「归属未证」展示面是否适用于本 hub。
    ///
    /// 门未装配的注入构造（`with_sources` 系既有用例）里，投影的 bindingRequired、
    /// 能力禁用原因与恢复派发拒绝统一报 `OC_INSTANCE_OWNERSHIP_UNVERIFIED`；上传口
    /// 无 active 路由的拒绝必须与之同码——该会话**从未有过绑定**，报「绑定已变」
    /// 是误传。生产门恒装配 → 归属证明逐次由写权链承担（N1），本判定恒 false，
    /// 保持 `OC_BINDING_STALE` 契约不变。与 [`HubInner::reject_unproven_ownership`]
    /// 同一套测试逃逸口径；纯构造检查，零网络读、零状态写。
    pub fn ownership_display_unverified(&self) -> bool {
        #[cfg(any(test, feature = "opencode-coordinator-tests"))]
        if self.inner.coordinator_test_only {
            return false;
        }
        #[cfg(test)]
        if self.inner.ownership_test_evidence {
            return false;
        }
        self.inner.binding_gate.is_none()
    }

    /// 「强制交回」的预读：当前**无法再核对**的动作条数（零副作用）。
    ///
    /// 有当前代际 ⇒ 只数「存储代际 ≠ 当前代际」的未终态行；代际不可证 ⇒ 数**全部**
    /// 未终态行（评审 R-1：没有可比的代际，就再也无法核对）。本进程在飞的写不计入。
    /// 调用方据此决定是否给出强制交回的二次确认。
    pub fn unverifiable_actions(&self, session_id: &str) -> Result<usize, NativeError> {
        // 代际不可证时**不是 0**：没有可比的当前代际 ⇒ 每条未终态行都无法再核对（R-1）。
        let current = self.current_runtime();
        // 与 force 同一排除集：真实在飞的写即使已被 verify 改写成 Unconfirmed，
        // 也不该在 409 回执里被宣称为「可作废」。
        let in_flight = self.active_in_flight();
        self.action_store()
            .unverifiable_count_excluding(session_id, current.as_ref(), &in_flight)
    }

    /// 绑定探测（`session-send-info` 的**候选**路由判定，评审 Blocker 1）：
    /// 一次只读发现 + 契约 + `/api/info` + 会话读 + 三重核对，**不认领、不发布修订、
    /// 不写路由行**。任何失败都只是 `false`——候选判定永远不能变成错误响应。
    ///
    /// 判据与 [`NativeHub::view_and_claim`] 的投影前半段同源（同一 `discover` /
    /// `bind_session` / 同一客户端），因此「候选」= 「此刻打开视图大概率能认领」，
    /// 而不是另一套猜测。
    pub async fn can_bind(&self, session: &Session) -> bool {
        if self.inner.ensure_live().is_err() {
            return false;
        }
        let Ok(source) = self.inner.source_for_session(&session.id).await else {
            return false;
        };
        let Ok(info) = source.client.read_json(INFO_PATH, &[]).await else {
            return false;
        };
        let Ok(session_segment) = segment(&session.id) else {
            return false;
        };
        let path = format!("/api/session/{session_segment}");
        let Ok(native) = source.client.read_json(&path, &[]).await else {
            return false;
        };
        bind_session(session, &source.descriptor, &info, &native).is_ok()
    }

    /// 旧路径写许可（read permit，任务 8 屏障）：旧 OpenCode 写入口与 flush 在写出前
    /// 取用，**持有到本次写入与结果记录全部结束**。与 [`NativeHub::select_native_route`]
    /// 的 write permit 互斥——后者必须等所有在飞旧写结束才原子登记归属，杜绝
    /// 「守卫检查后才切路由」的竞态。
    ///
    /// **不可失败**（M-1，评审裁定）：锁表是纯内存短临界区、`read_owned` 只有
    /// 「写者在场需等待」一种语义（等待不是错误），因此这里没有错误变体——调用方
    /// 不需要（也不允许）为它编造 500 死臂。让位语义只存在于同步变体
    /// [`NativeHub::try_legacy_permit_now`]（拿不到即本轮让位）。
    pub async fn try_legacy_permit(
        &self,
        session_id: &str,
    ) -> tokio::sync::OwnedRwLockReadGuard<()> {
        self.legacy_lock(session_id).read_owned().await
    }

    /// 同步变体（flush 内核运行在 spawn_blocking，无 async 上下文）：拿不到即让位——
    /// write permit 在场（切路由进行中）时返回 `None`，调用方本轮不投递不消费队首。
    /// 常规无竞争时与 [`NativeHub::try_legacy_permit`] 等价（多个读者共享）。
    pub fn try_legacy_permit_now(
        &self,
        session_id: &str,
    ) -> Option<tokio::sync::OwnedRwLockReadGuard<()>> {
        self.legacy_lock(session_id).try_read_owned().ok()
    }

    /// 每会话许可锁（不存在则建；纯内存短临界区，零 IO）。
    /// `pub(crate)`：`actions::select_native_route` 的 write 侧取用同一把锁。
    pub(crate) fn legacy_lock(&self, session_id: &str) -> Arc<tokio::sync::RwLock<()>> {
        self.inner.legacy_lock(session_id)
    }

    /// 订阅某会话的修订号。只登记频道；上游由 `view`/`watch` 按需拉起并共享。
    ///
    /// 永久 shutdown 之后不再给出「活着但永远静默」的频道：返回一个值 0、发送端已丢弃
    /// 的频道，调用方的 `changed()` 立即得到关闭信号。**pause 不在此列**：pause 只是
    /// 关闭旧监听的订阅，重新开启后新 `watch` 照常可用（F2）。
    pub fn watch(&self, session_id: &str) -> watch::Receiver<u64> {
        let receiver = {
            let mut state = self.inner.state_lock();
            if self.inner.stopped.load(Ordering::SeqCst) {
                let (tx, rx) = watch::channel(0);
                drop(tx);
                return rx;
            }
            let entry = state
                .sessions
                .entry(session_id.to_string())
                .or_insert_with(Entry::new);
            entry.tx.subscribe()
        };
        self.inner.ensure_pump();
        receiver
    }

    /// 投递证据门：只有「最后一次消息读取完整」时，消息列表缺席某个 id 才可作为
    /// 「未投递」的否定证据。未知会话、部分历史、被释放的缓存一律 false。
    pub fn delivery_evidence(&self, session_id: &str) -> bool {
        self.inner
            .state_lock()
            .sessions
            .get(session_id)
            .and_then(|entry| entry.snapshot.as_ref())
            .is_some_and(|snapshot| snapshot.complete)
    }

    /// **可恢复的监听停用**（F2）：停远程 / 热重启的停半程调用它，而不是永久
    /// [`NativeHub::shutdown`]。
    ///
    /// 做的事与 shutdown 相同——取消 SSE 上游、作废旧 source client、丢弃全部会话
    /// 订阅（旧 watch 立即关闭）——但**不置 `stopped`**：下一次 `view`/`watch` 走
    /// 只读重新发现，重建投影与上游。动作账本、路由归属、设备都在共享存储里，本方法
    /// 不触碰。
    ///
    /// 代际先前进再清状态：任何在 pause 之前开始的 view/pump 都带着旧代际进
    /// `commit`，结果会因代际不等被丢弃，绝不污染新监听（评审 F2 的旧任务隔离要求）。
    pub fn pause(&self) {
        self.stop_outbox();
        self.inner.detach();
    }

    /// 停止上游与所有后台任务，关闭订阅频道（永久语义：此后 `view` 不再服务）。
    ///
    /// 进程退出/永久销毁路径用它；停远程、热重启用可恢复的 [`NativeHub::pause`]。
    pub fn shutdown(&self) {
        self.stop_outbox();
        self.inner.stopped.store(true, Ordering::SeqCst);
        self.inner.detach();
    }
}

/// pump 任务的一次性输入：把 7 个参数收成结构体，避免函数参数膨胀
///（clippy too_many_arguments），字段语义仍一目了然。
struct PumpInputs {
    client: Arc<NativeClient>,
    generation: RuntimeKey,
    epoch: u64,
    cancel: Arc<watch::Sender<bool>>,
    stop: watch::Receiver<bool>,
    events_tx: mpsc::Sender<RuntimeKey>,
    events: mpsc::Receiver<RuntimeKey>,
}

/// 全局只允许一个可执行 service 进程；多个时产品无法证明 session 卡属于谁。
/// 不接受目录参数（保留签名兼容）；目标目录仅用于上层语义，不参与计数。
/// `serve --stdio` 是 ACP/TUI 内嵌服务，不是 managed/service 目标，排除。
pub(crate) fn production_service_conflict(_directory: &Path) -> bool {
    let system = sysinfo::System::new_all();
    // J6：排除候选应用自身的 serve 子进程（exe 在候选 .app 目录内）——候选核心
    // 引擎不是「用户侧工具 serve」，否则用户真实 serve 常驻时原生新建永远死路
    // （用户裁决口径：现有真实 OpenCode 服务继续运行）。两个**用户侧**工具 serve
    // 无法区分时仍明确拒绝（保守闸不变）。
    let self_exe = std::env::current_exe().ok();
    let is_candidate_bundle_serve = |process: &sysinfo::Process| -> bool {
        self_exe
            .as_ref()
            .and_then(|own| {
                process
                    .exe()
                    .map(|exe| exe.starts_with(own.ancestors().nth(3).unwrap_or(own.as_path())))
            })
            .unwrap_or(false)
    };
    let count = running_service_count(
        system
            .processes()
            .values()
            .filter(|process| !is_candidate_bundle_serve(process))
            .map(|process| {
                (
                    process.name().to_string_lossy().into_owned(),
                    process
                        .cmd()
                        .iter()
                        .map(|part| part.to_string_lossy().into_owned())
                        .collect(),
                )
            }),
    );
    count > 1
}

/// 全局运行中的可执行 service 数（排除 ACP `serve --stdio`）。没有可靠 DB/owner
/// 事实时，多个 service 一律保守拒绝：宁可多拦不同项目的多开，也不把会话交给
/// 无法证明的执行者。
fn running_service_count(processes: impl Iterator<Item = (String, Vec<String>)>) -> usize {
    processes
        .filter(|(name, cmd)| {
            name.eq_ignore_ascii_case("opencode")
                && cmd.iter().any(|part| part == "serve")
                && !cmd.iter().any(|part| part == "--stdio")
        })
        .count()
}

/// 纯计数：name 精确 opencode、命令含 serve 且不含 --stdio、cwd 与目标目录 canonical 相等。
/// 返回 >1 即冲突。测试注入进程描述，不依赖本机进程表。
#[cfg(test)]
fn conflicting_service_count(
    processes: impl Iterator<Item = (String, Vec<String>, Option<std::path::PathBuf>)>,
    directory: &Path,
) -> usize {
    let expected = std::fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
    processes
        .filter(|(name, cmd, cwd)| {
            name.eq_ignore_ascii_case("opencode")
                && cmd.iter().any(|part| part == "serve")
                && !cmd.iter().any(|part| part == "--stdio")
                && cwd.as_ref().is_some_and(|cwd| {
                    std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.clone()) == expected
                })
        })
        .count()
}

/// 相同代际还须拥有相同端点、版本与凭据，才能复用连接。
fn same_service_descriptor(a: &ServiceDescriptor, b: &ServiceDescriptor) -> bool {
    a.runtime == b.runtime
        && a.endpoint == b.endpoint
        && a.version == b.version
        && a.explicit == b.explicit
        && a.password == b.password
}

/// 只把缺少桌面归属视为只读；服务代际或发现失效的投影不可发布。
fn view_ownership_status(result: Result<(), NativeError>) -> Result<bool, NativeError> {
    match result {
        Ok(()) => Ok(true),
        Err(error) if error.code == codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED => Ok(false),
        Err(error) => Err(error),
    }
}

impl HubInner {
    fn legacy_lock(&self, session_id: &str) -> Arc<tokio::sync::RwLock<()>> {
        let mut permits = self
            .legacy_permits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        permits
            .entry(session_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::RwLock::new(())))
            .clone()
    }

    /// 固定目标允许读取仍可核验的原服务；全局发现不匹配时只读而不授予写权。
    async fn view_ownership_for(
        &self,
        target: &NativeTarget,
        pinned: bool,
    ) -> Result<bool, NativeError> {
        match self.verify_service_identity(target).await {
            Ok(()) => Ok(true),
            Err(_) if pinned => {
                self.source_for_session_pinned(&target.session_id, Some(target))
                    .await?;
                Ok(false)
            }
            Err(error) => view_ownership_status(Err(error)),
        }
    }

    /// 写前检查服务代际及桌面归属（**不含** echo-challenge 证据）。
    ///
    /// 服务登记和 `/api/info` 仅能证明接收服务，不能证明目标 TUI 当前连接了它；
    /// 在可信的连接证据来源接入之前保守拒写（H02 既有裁决）。测试协调器的显式
    /// 注入只用于独立测试，不代表生产归属证据。
    ///
    /// 消费方：投影泵/视图归属（**只读、展示与软判定**——写权不经过这里；写权链
    /// 见 [`HubInner::verify_write_authority`]，H03 起生产写链以 echo-challenge
    /// 证据替代本函数的无条件拒绝）。
    async fn verify_service_identity(
        &self,
        target: &super::types::NativeTarget,
    ) -> Result<(), NativeError> {
        self.verify_service_preconditions(target).await?;
        self.reject_unproven_ownership()
    }

    /// 写权链前置：同目录多 service 保守闸 + 发现 + 代际核对（登记/info/pid 三互证的
    /// 必要前置）。返回发现的描述符供 ACK 后的服务通道终验复用。
    async fn verify_service_preconditions(
        &self,
        target: &super::types::NativeTarget,
    ) -> Result<ServiceDescriptor, NativeError> {
        #[cfg(any(test, feature = "opencode-coordinator-tests"))]
        if self.coordinator_test_only {
            return self.discover_for_session(&target.session_id).await;
        }
        if (self.service_conflict)(&target.directory) {
            return Err(NativeError::new(
                codes::OC_INSTANCE_AMBIGUOUS,
                "multiple OpenCode services are running; refusing to choose an execution owner",
            ));
        }
        // 与 source_for_session 同一发现次序：登记缺失（且仅缺失）时按目标会话回退
        // 显式形态证据（服务身份锚 = argv 端点的 /api/info，客户端 PID 不进 RuntimeKey）
        // ——普通启动与显式连接复用同一目标与代际核验规则。
        let descriptor = self.discover_for_session(&target.session_id).await?;
        if descriptor.runtime != target.runtime {
            return Err(NativeError::new(
                codes::OC_GENERATION_MISMATCH,
                "opencode service generation does not match the verified target",
            ));
        }
        Ok(descriptor)
    }

    /// 旧归属裁决（H02 之前）：无可信连接证据 → 保守拒写；仅测试逃逸可过。
    fn reject_unproven_ownership(&self) -> Result<(), NativeError> {
        #[cfg(any(test, feature = "opencode-coordinator-tests"))]
        if self.coordinator_test_only {
            return Ok(());
        }
        #[cfg(test)]
        if self.ownership_test_evidence {
            return Ok(());
        }
        Err(NativeError::new(
            codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED,
            "the target TUI's current service connection is not verified",
        ))
    }

    /// N1 选择事件：从新鲜观察捕获目标会话的窗口身份，与会话/目录/服务代际一起保存。
    ///
    /// 捕获失败（此刻无新鲜 `Opened`）**不写**注册表——该目标随后回退到「当前新鲜
    /// `Opened`」查询（fail-closed 语义与既有门一致）。重复选择覆盖旧绑定
    ///（显式重选 = 换窗口的唯一合法途径；「禁止自动换窗」由读取侧判据保证）。
    ///
    /// M1（复审 P1）：同一身份同步持久化到伴表（[`super::super::super::database::
    /// dao::opencode_pinned_windows`]），Tuvis 重启清空内存注册表后由
    /// [`Self::locked_window_for_write`] 恢复——否则重启后持久待发项的后台派发会
    /// 经「新鲜 `Opened`」回退用另一扇正打开 A 的窗口取得授权。持久化失败只降级为
    /// N1 原语义（本进程内存态），不阻断选择事件；读取侧对持久层错误 fail-closed。
    /// F12 项 1：把窗口身份构造为绑定行（伴表 DAO 形态；提交事务内落库用）。
    fn pinned_row_for(
        target: &NativeTarget,
        window: &super::binding_gate::LockedWindow,
    ) -> crate::database::dao::opencode_pinned_windows::PinnedWindowRow {
        crate::database::dao::opencode_pinned_windows::PinnedWindowRow {
            service_id: target.runtime.service_id.clone(),
            runtime_pid: i64::from(target.runtime.pid),
            runtime_started_at_ms: target.runtime.started_at_ms,
            directory: target.directory.to_string_lossy().into_owned(),
            window_pid: i64::from(window.pid),
            window_started_at_ms: window.started_at_ms,
            window_boot: window.boot.clone(),
        }
    }

    /// F12 项 1：提交成功后把**同一**窗口身份记入内存注册表（伴表行已在同一事务
    /// 落库，此处只补进程内注册表；两者同源，读取侧判据逐字一致）。
    fn record_pinned_window_in_registry(
        &self,
        target: &NativeTarget,
        window: &super::binding_gate::LockedWindow,
    ) {
        self.pinned_windows.lock().unwrap().insert(
            target.session_id.clone(),
            PinnedWindowBinding {
                target: target.clone(),
                window: window.clone(),
            },
        );
    }

    #[cfg(test)]
    fn bind_pinned_window(&self, target: &NativeTarget) {
        let Some(window) = (self.gate_locked_window)(&target.session_id) else {
            return;
        };
        self.pinned_windows.lock().unwrap().insert(
            target.session_id.clone(),
            PinnedWindowBinding {
                target: target.clone(),
                window: window.clone(),
            },
        );
        let persisted = self.store.with(|conn| {
            crate::database::dao::opencode_pinned_windows::ensure_table_conn(conn)?;
            crate::database::dao::opencode_pinned_windows::pin_conn(
                conn,
                &target.session_id,
                &crate::database::dao::opencode_pinned_windows::PinnedWindowRow {
                    service_id: target.runtime.service_id.clone(),
                    runtime_pid: i64::from(target.runtime.pid),
                    runtime_started_at_ms: target.runtime.started_at_ms,
                    directory: target.directory.to_string_lossy().into_owned(),
                    window_pid: i64::from(window.pid),
                    window_started_at_ms: window.started_at_ms,
                    window_boot: window.boot.clone(),
                },
                chrono::Utc::now().timestamp_millis(),
            )
        });
        if let Err(error) = persisted {
            log::warn!("窗口绑定身份持久化失败（本进程内仍按内存注册表生效）: {error}");
        }
    }

    /// M1 重启恢复：注册表无该目标条目时（进程重启后的必然形态）读持久化绑定身份。
    ///
    /// 三态返回，区分「无绑定」与「持久层错误」——两者处理不同：
    /// * `Ok(Some(binding))`：读回的选择时刻绑定身份，交回既有 `applies` 判据
    ///   （绑定目标与写目标全等 + 路由行 `active` 同代际同目录）即时核对——适用则
    ///   锁定该窗口（重启后同一窗口仍开 A 照常放行），不适用则与内存注册表一致地
    ///   回退新鲜查询；
    /// * `Ok(None)`：该会话从无持久绑定（未绑定过 / 旧路由行时代）→ 回退新鲜查询，
    ///   与 N1 之前的门语义一致；
    /// * `Err`：持久层读失败——**绝不冒充「无绑定」回退新鲜查询**（那正是复审 P1
    ///   的授权漏洞面），fail-closed：本次写对该会话无锁定窗口（门按缺身份拒绝）。
    fn restore_pinned_window(
        &self,
        target: &NativeTarget,
    ) -> Result<Option<PinnedWindowBinding>, ()> {
        let row = self
            .store
            .with(|conn| {
                crate::database::dao::opencode_pinned_windows::ensure_table_conn(conn)?;
                crate::database::dao::opencode_pinned_windows::pinned_window_conn(
                    conn,
                    &target.session_id,
                )
            })
            .map_err(|_| ())?;
        let Some(row) = row else {
            return Ok(None);
        };
        let runtime = RuntimeKey {
            service_id: row.service_id,
            pid: u32::try_from(row.runtime_pid).map_err(|_| ())?,
            started_at_ms: row.runtime_started_at_ms,
        };
        let window_pid = u32::try_from(row.window_pid).map_err(|_| ())?;
        Ok(Some(PinnedWindowBinding {
            target: NativeTarget {
                runtime,
                session_id: target.session_id.clone(),
                directory: std::path::PathBuf::from(row.directory),
            },
            window: super::binding_gate::LockedWindow {
                pid: window_pid,
                started_at_ms: row.window_started_at_ms,
                boot: row.window_boot,
            },
        }))
    }

    /// N1 写授权锁定窗口：已固定目标的绑定窗口身份优先，其余回退当前新鲜查询。
    ///
    /// 绑定仅在**全部**成立时适用（任一不成立 → 回退 [`Self::gate_locked_window`]，
    /// 绝不放宽）：
    /// * 绑定记录的目标与本次写目标全等（会话/目录/服务代际）；
    /// * 路由行仍 `active` 且与目标同代际同目录（交回/换代后旧绑定不再适用——
    ///   读取时即时核对，交回路径无需额外清理也能保持正确）。
    ///
    /// M1（复审 P1）：内存注册表无该目标条目时（Tuvis 重启后的必然形态）先读
    /// 持久化绑定身份（[`Self::restore_pinned_window`]）再走同一 `applies` 判据——
    /// 同一窗口仍开 A 照常放行、原窗口退出/换代/换窗仍拒；持久层**读错误**则对该
    /// 会话 fail-closed（不冒充「无绑定」回退新鲜查询）。适用判据与回退语义对两种
    /// 绑定来源逐字同源，绝不放宽既有门。
    ///
    /// F12 项 1.5：本会话有活动路由（`active`）但窗口绑定记录**缺失或不匹配**时，
    /// **不再**回退新鲜查询——本会话 fail-closed（None）：后台派发不得自行换窗，
    /// 恢复路径 = 显式重选（其新鲜候选优先的豁免在 [`Self::reselect_window_lookup`]，
    /// patch5 死亡代际语义不变）；落未知/待发由 M3/patch6 既有出口承接。路由非
    /// active（交回/无路由）的回退保留：该面没有派发权，不构成换窗授权。
    ///
    /// 其他会话照常回退生产新鲜查询。绑定窗口仍要过门内全套判据：
    /// 进程存活与代际（`tui_fact`）、插件代际（控制文件 `expectBoot`）、实际连接
    ///（challenge 经服务 S 回声 + ack pid 比对）与 ACK 后终验。
    fn locked_window_for_write(
        &self,
        target: &NativeTarget,
    ) -> super::binding_gate::LockedWindowLookup {
        /// 锁定窗口来源三态：绑定可用 / 回退新鲜查询 / 持久层未知（fail-closed）。
        enum BindingSource {
            /// 内存注册表命中，或注册表 miss 后从持久层恢复（适用性由后续判据即时核对）。
            Binding(PinnedWindowBinding),
            /// 无绑定（注册表 miss 且持久层确认无行）→ 与 N1 之前一致回退新鲜查询。
            FreshFallback,
            /// 持久层读错误 → 本会话无锁定窗口（fail-closed），其他会话照常新鲜查询。
            FailClosed,
        }
        // 注册表守卫用 `let` 落地（match scrutinee 的临时量会活过全部分支——
        // 守卫横跨 store 锁会引入锁序风险），先摘取再判来源。
        let cached = self
            .pinned_windows
            .lock()
            .unwrap()
            .get(&target.session_id)
            .cloned();
        let source = match cached {
            Some(binding) => BindingSource::Binding(binding),
            None => match self.restore_pinned_window(target) {
                Ok(Some(binding)) => BindingSource::Binding(binding),
                Ok(None) => BindingSource::FreshFallback,
                Err(()) => BindingSource::FailClosed,
            },
        };
        let binding = match &source {
            BindingSource::Binding(binding) => Some(binding.clone()),
            _ => None,
        };
        let route_row = self
            .store
            .with(|conn| {
                crate::database::dao::opencode_actions::route_conn(conn, &target.session_id)
            })
            .ok()
            .flatten();
        let route_active = route_row.as_ref().is_some_and(|route| {
            route.route_state == crate::database::dao::opencode_actions::ROUTE_ACTIVE
        });
        let applies = binding.as_ref().is_some_and(|binding| {
            binding.target == *target
                && route_row.as_ref().is_some_and(|route| {
                    route.route_state == crate::database::dao::opencode_actions::ROUTE_ACTIVE
                        && route.same_runtime(target)
                })
        });
        match (source, applies) {
            (BindingSource::Binding(binding), true) => {
                let session_id = target.session_id.clone();
                let window = binding.window;
                let fallback = Arc::clone(&self.gate_locked_window);
                Arc::new(move |sid: &str| {
                    if sid == session_id {
                        Some(window.clone())
                    } else {
                        fallback(sid)
                    }
                })
            }
            // 持久层读错误 → 本会话无锁定窗口（fail-closed：不冒充「无绑定」回退
            // 新鲜查询——那正是复审 P1 的授权漏洞面）；其他会话不受影响。
            (BindingSource::FailClosed, _) => {
                let session_id = target.session_id.clone();
                let fallback = Arc::clone(&self.gate_locked_window);
                Arc::new(move |sid: &str| {
                    if sid == session_id {
                        None
                    } else {
                        fallback(sid)
                    }
                })
            }
            // F12 项 1.5：路由 active 但窗口绑定记录**缺失或不匹配** ⇒ 本会话无锁定
            // 窗口（fail-closed）——后台派发不得经新鲜查询把「另一扇正打开同一会话的
            // 窗口」当成授权自行换窗；恢复路径 = 显式重选（patch5 死亡代际豁免在
            // `reselect_window_lookup` 的新鲜候选优先里，不经本回退面）。落未知/待发
            // 语义由 M3/patch6 既有出口承接。路由非 active（交回/无路由）保留回退：
            // 该面没有派发权，回退只服务展示语义，不构成换窗授权。
            (_, _) if route_active => {
                let session_id = target.session_id.clone();
                let fallback = Arc::clone(&self.gate_locked_window);
                Arc::new(move |sid: &str| {
                    if sid == session_id {
                        None
                    } else {
                        fallback(sid)
                    }
                })
            }
            _ => Arc::clone(&self.gate_locked_window),
        }
    }

    /// 新鲜的同窗观察只可否定旧插件身份。换窗、进程换代或无观察仍走既有写门。
    fn reject_reloaded_window(
        &self,
        target: &NativeTarget,
        locked: Option<&super::binding_gate::LockedWindow>,
    ) -> Result<(), NativeError> {
        let Some(locked) = locked.filter(|window| window.boot.is_some()) else {
            return Ok(());
        };
        let fresh = crate::monitor::opencode_observation::fresh_opened_window_for_pid(
            &target.session_id,
            locked.pid,
        )
        .or_else(|| (self.gate_locked_window)(&target.session_id));
        if crate::monitor::opencode_observation::plugin_boot_changed(locked)
            || fresh.is_some_and(|fresh| {
                fresh.pid == locked.pid
                    && fresh.started_at_ms == locked.started_at_ms
                    && fresh.boot.is_some()
                    && fresh.boot != locked.boot
            })
        {
            return Err(NativeError::new(
                codes::OC_BINDING_STALE,
                "the observation plugin reloaded in the fixed native window",
            ));
        }
        Ok(())
    }

    /// **N2/M2 语义证据准入**（写权链第一道，先于探针创建/改写；机制依据 = H02 结论
    /// 速览 9 的版本绑定警示）：当前「OpenCode 构建（二进制 SHA-256）/平台/启动形态」
    /// 组合必须命中 [`attribution_evidence`] 登记表的同一条目，才允许开始一轮依赖
    /// 「数据层 per-session SSE 失效刷新」行为事实的 echo-challenge 归属核验。
    ///
    /// M2 起生产写权链经 `verify_write_authority_using` 的单次解析入口；本入口
    /// 保留给准入单元测试（解析来源与 [`Self::admit_semantic_evidence_for_window`]
    /// 判据零改动）。
    #[cfg(test)]
    fn admit_semantic_evidence(
        &self,
        target: &NativeTarget,
        descriptor: &ServiceDescriptor,
    ) -> Result<(), NativeError> {
        let lookup = self.locked_window_for_write(target);
        let window = lookup(target.session_id.as_str());
        self.admit_semantic_evidence_for_window(descriptor, window.as_ref())
    }

    /// 语义准入本体（M2）：登记组合 =（锚定 TUI 构建 SHA, 服务构建 SHA, 平台,
    /// 启动形态）四元组对登记表**同一条目**精确匹配。
    ///
    /// fail-closed 三面：
    /// * 锚定 TUI 身份取不到（`window` = `None`——与门内锁定同一次解析的结果）
    ///   → 缺 TUI 身份，按不适用拒绝；
    /// * 任一构建 SHA 解析失败（`None`：进程死/无 exe/读失败；TUI 与服务各自
    ///   解析、各自缓存）→ 按不适用拒绝，不猜版本；
    /// * 组合未登记（含只换 TUI 构建、只换服务构建、双换、其他平台、未覆盖启动
    ///   形态）→ 稳定原因码 `OC_GATE_SEMANTICS_UNPROVEN`，双语恢复说明在前端
    ///   H05 族呈现。
    ///
    /// 已证组合只获得「继续走门内判据」的资格：绑定核对、回声门三件套、ACK 后终验
    /// 逐动作照旧；读取与非依赖能力不经本函数（各自契约控制）。
    fn admit_semantic_evidence_for_window(
        &self,
        descriptor: &ServiceDescriptor,
        window: Option<&super::binding_gate::LockedWindow>,
    ) -> Result<(), NativeError> {
        let launch_form = if descriptor.explicit {
            super::attribution_evidence::LAUNCH_EXPLICIT
        } else {
            super::attribution_evidence::LAUNCH_MANAGED
        };
        // 锚定窗口 = 门即将锁定的窗口（调用方解析一次传两处）。取不到 = 缺 TUI
        // 身份：语义证据无从锚定 → fail-closed。
        let Some(window) = window else {
            return Err(super::attribution_evidence::unproven_error());
        };
        let Some(tui_sha) = self.tui_binary_sha_cached(window.pid, window.started_at_ms) else {
            return Err(super::attribution_evidence::unproven_error());
        };
        // 服务构建（M2 补全的维度）：锚 = 服务描述符的 RuntimeKey（pid + 启动毫秒，
        // 托管/显式两形态均为服务进程身份）；解析经服务进程可执行路径，每代际缓存。
        let Some(service_sha) = self
            .service_binary_sha_cached(descriptor.runtime.pid, descriptor.runtime.started_at_ms)
        else {
            return Err(super::attribution_evidence::unproven_error());
        };
        let platform = super::attribution_evidence::platform_word();
        if super::attribution_evidence::admitted(platform, launch_form, &tui_sha, &service_sha) {
            Ok(())
        } else {
            Err(super::attribution_evidence::unproven_error())
        }
    }

    /// **显式重选端点专用**的锁定窗口查询（M1）：本会话「新鲜候选优先，既有绑定
    /// 兜底」，其他会话走常规查询。
    ///
    /// 为什么需要：重启恢复后既有绑定可能锁定**已死窗口**（一般写 fail-closed 拒绝
    /// 是正确语义），但显式重选的目的正是换绑当前窗口——候选核验（语义准入锚定 +
    /// 门内锁定）必须锚定**将要被绑定的新鲜候选**，否则死绑定会把恢复路径永久锁死
    ///（重选被旧绑定挡住 → 无法换绑 → 永远拒绝）。旧绑定仍存活且无新鲜候选时回退
    /// 常规查询（原判定不变）；门内判据不因覆盖放宽（换绑后逐次写照常全链核验）。
    fn reselect_window_lookup(
        &self,
        target: &NativeTarget,
    ) -> super::binding_gate::LockedWindowLookup {
        let fallback = self.locked_window_for_write(target);
        let fresh = Arc::clone(&self.gate_locked_window);
        let session_id = target.session_id.clone();
        Arc::new(move |sid: &str| {
            if sid == session_id {
                fresh(sid).or_else(|| fallback(sid))
            } else {
                fallback(sid)
            }
        })
    }

    fn fresh_window_for_pid(
        &self,
        target: &NativeTarget,
        pid: u32,
    ) -> Option<super::binding_gate::LockedWindow> {
        crate::monitor::opencode_observation::fresh_opened_window_for_pid(&target.session_id, pid)
            .or_else(|| {
                (self.gate_locked_window)(&target.session_id).filter(|window| window.pid == pid)
            })
    }

    fn preview_window(&self, target: &NativeTarget) -> Option<super::binding_gate::LockedWindow> {
        let lookup = self.locked_window_for_write(target);
        if let Some(locked) = lookup(&target.session_id) {
            let fresh = self.fresh_window_for_pid(target, locked.pid);
            if crate::monitor::opencode_observation::plugin_boot_changed(&locked)
                || fresh.as_ref().is_some_and(|window| {
                    window.started_at_ms == locked.started_at_ms && window.boot != locked.boot
                })
            {
                return fresh.filter(|window| window.started_at_ms == locked.started_at_ms);
            }
        }
        (self.gate_locked_window)(&target.session_id)
    }

    fn recheck_previewed_window(
        &self,
        target: &NativeTarget,
        window: &super::binding_gate::LockedWindow,
    ) -> Result<(), NativeError> {
        if crate::monitor::opencode_observation::plugin_boot_changed(window)
            || self.fresh_window_for_pid(target, window.pid).as_ref() != Some(window)
        {
            return Err(NativeError::new(
                codes::OC_BINDING_STALE,
                "the previewed native window no longer observes this session",
            ));
        }
        Ok(())
    }

    /// 构建 SHA 的每代际缓存：键 = 锚定进程身份 (pid, 启动毫秒)。同代际进程存活期
    /// 二进制不变 → 只算一次；PID 复用换启动时刻即新键。**只缓存成功值**：失败形态
    /// 可能瞬态（读失败/超时），下一轮重算——绝不让一次失败钉死整个代际的可用性
    ///（fail-closed 每轮保持，恢复即放行，见 T4/单测 `admission_resolution_failure_
    /// is_fail_closed_and_not_cached`）。
    fn tui_binary_sha_cached(&self, pid: u32, started_at_ms: i64) -> Option<String> {
        let mut cache = self
            .semantic_sha_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = (pid, started_at_ms);
        if let Some(sha) = cache.get(&key) {
            return Some(sha.clone());
        }
        let sha = (self.tui_binary_sha)(pid)?;
        cache.insert(key, sha.clone());
        Some(sha)
    }

    /// M2 服务构建 SHA 的每代际缓存：键 = 服务进程 (pid, 启动毫秒)——与 TUI 缓存
    ///（`semantic_sha_cache`）分表：两者是不同进程，键语义不同（服务代际锚 =
    /// descriptor.runtime；TUI 代际锚 = 锁定窗口身份），同代际只算一次、只缓存
    /// 成功值的惯例逐字同源（失败瞬态不钉死代际，见 `tui_binary_sha_cached`）。
    fn service_binary_sha_cached(&self, pid: u32, started_at_ms: i64) -> Option<String> {
        let mut cache = self
            .service_sha_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = (pid, started_at_ms);
        if let Some(sha) = cache.get(&key) {
            return Some(sha.clone());
        }
        let sha = (self.service_binary_sha)(pid)?;
        cache.insert(key, sha.clone());
        Some(sha)
    }

    /// **写权链**（H03 C1 接通）：三互证前置 → echo-challenge 归属证明 → ACK 后终验。
    ///
    /// * 门**装配**（生产构造 `NativeHub::new`，或测试显式装门）：前置通过后执行一轮
    ///   探针回声挑战；**echo 门通过 = 生产归属证明成立，替代旧的无条件拒绝**；拒绝
    ///   按具体 `OC_GATE_*` 受控码（fail-closed）。ACK 成功后、返回前对锁定 TUI 做
    ///   **同代际终验**（pid+启动时间）与**服务通道终验**（`/api/info` 与代际锚互证，
    ///   与 C1 同源），覆盖「应答后退出/换代仍取得派发权」的时序（复审 I1）。
    /// * 门**未装配**（注入构造既有用例）：保持旧行为（测试逃逸 / `OC_INSTANCE_
    ///   OWNERSHIP_UNVERIFIED`）——这是可测缝，不是产品开关。
    async fn verify_write_authority(
        &self,
        target: &super::types::NativeTarget,
        client: Option<&Arc<NativeClient>>,
    ) -> Result<(), NativeError> {
        self.verify_write_authority_using(target, client, None, false)
            .await
            .map(|_| ())
    }

    /// 写权链的锁定窗口可覆盖形态（M1，仅供**显式重选**端点使用）：`lookup` 给定时，
    /// 语义准入锚定与门内锁定都用它；`None` = 常规 `locked_window_for_write`。
    /// 判据本体（存活/插件代际/回声/ACK 后终验）不因覆盖而放宽——覆盖只改「锁定
    /// 哪扇窗口」，见 [`Self::reselect_window_lookup`]。
    ///
    /// F12 项 1：成功返回**本轮回声验证通过的锁定窗口身份**（M2 同轮单一身份产物），
    /// 供显式重选把它直接带进认领事务（同一事务内落路由 CAS + 绑定行）。门未装配
    ///（测试逃逸）或核对未走门内 echo 时返回 `None`——调用方回退新鲜观察捕获。
    async fn verify_write_authority_using(
        &self,
        target: &super::types::NativeTarget,
        client: Option<&Arc<NativeClient>>,
        lookup: Option<super::binding_gate::LockedWindowLookup>,
        require_current_window: bool,
    ) -> Result<Option<super::binding_gate::LockedWindow>, NativeError> {
        let descriptor = self.verify_service_preconditions(target).await?;
        match self.binding_gate.as_ref() {
            Some(gate) => {
                // M2（复审「准入与实际回声分两次选窗」）：锁定窗口解析提到准入
                // 之前、**只解析一次**——语义准入（SHA 语义适用性）与回声门
                //（challenge/ack）共用同一窗口身份。无固定绑定时两次解析之间窗口
                // 世界可能变化（等待期间换窗），双选窗会让「准入所验的构建/身份」
                // 与「实际回声所用」不一致；单次解析后，解析窗口之外的世界变化由
                // 门内既有判据兜底：解析窗口的换代核对（进程存活/换代，探针前）、
                // 控制文件 expectBoot（插件代际）、ack pid 比对（他窗应答，探针
                // 轮内）与 ACK 后终验——判据零放宽。
                let locked_lookup = match lookup {
                    Some(lookup) => lookup,
                    None => self.locked_window_for_write(target),
                };
                let locked_window = locked_lookup(target.session_id.as_str());
                self.reject_reloaded_window(target, locked_window.as_ref())?;
                // N2 语义证据准入（写权链第一道）：当前构建/平台/启动形态组合不在
                // H02 已证登记表内（或锚定 TUI 身份/任一构建 SHA 取不到）→ 在探针
                // 创建或改写之前拒绝依赖该证明的新写（三零：零探针写、零动作行、
                // 零队列项）。已证组合继续逐动作走门内全套判据（准入不替代绑定
                // 核对/回声门/ACK 终验）；服务身份与代际证据仍先于本准入（前置链
                // 未倒转）。
                self.admit_semantic_evidence_for_window(&descriptor, locked_window.as_ref())?;
                let client = match client {
                    Some(c) => Arc::clone(c),
                    None => {
                        let source = self.source_for_session(&target.session_id).await?;
                        Arc::clone(&source.client)
                    }
                };
                // I4 前置：目标不可是探针会话（探针目标身份注册表）。
                if gate.is_probe_session(&target.session_id) {
                    return Err(NativeError::new(
                        codes::OC_GATE_PROBE_TARGET,
                        "the target session is the binding gate probe",
                    ));
                }
                let transport = super::binding_gate::GateTransport::for_client(
                    Arc::clone(&client),
                    target.directory.clone(),
                );
                // 回声门消费**同一个**已解析窗口（同轮单一窗口身份）；门内判据
                // （存活/插件代际/实际连接）与 ACK 后终验不变。
                let (tui_pid, tui_started) = gate
                    .verify_with_locked(
                        &target.runtime,
                        &transport,
                        &target.directory,
                        locked_window.as_ref(),
                        &self.gate_tui_fact,
                        |locked| {
                            if require_current_window {
                                self.recheck_previewed_window(target, locked)
                            } else {
                                self.reject_reloaded_window(target, Some(locked))
                            }
                        },
                    )
                    .await?;
                let verified_window = locked_window.clone();
                // I1 ACK 后终验（次序 = 复审残余修正）：先做**最后一次网络等待**
                // （对实际写通道重读 `/api/info` 与代际锚互证——ACK 等待窗口内同端口
                // 换接收方时只有端点自报能分辨），**之后**再做进程代际终验——保证
                // 取得派发权前的最后一刻仍有「锁定 TUI 存活且同代际」的证据，
                // 覆盖「终验等待期间退出」的时序。
                let info = client.read_json(INFO_PATH, &[]).await?;
                super::discovery::verify_service_info(&info, &descriptor)?;
                self.reject_reloaded_window(target, verified_window.as_ref())?;
                if (self.gate_tui_fact)(tui_pid) != Some(tui_started) {
                    return Err(NativeError::new(
                        codes::OC_GATE_PID_UNVERIFIED,
                        "locked tui exited or regenerated after the ack",
                    ));
                }
                Ok(verified_window)
            }
            // 门未装配（注入构造既有用例）：无 echo 证据，归属维持旧裁决
            //（测试逃逸 / 无条件拒）。给定了**实际写通道**时保留原 C1 复核（同端点
            // 换接收方只有端点自报能分辨）；未给定 client 的通用身份核对保持旧形态
            //（零额外 IO，认领/重入路径的原语义不变）。
            None => {
                self.reject_unproven_ownership()?;
                match client {
                    Some(c) => {
                        let info = c.read_json(INFO_PATH, &[]).await?;
                        super::discovery::verify_service_info(&info, &descriptor)?;
                        Ok(None)
                    }
                    None => Ok(None),
                }
            }
        }
    }

    /// 状态锁：缓存 poisoned 也继续用内部值——一次 panic 不能让整个 hub 永久不可用。
    fn state_lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 当前监听代际（只读快照；在途工作据此判断自己是否已过期）。
    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// 代际守卫：不等于当前代际即视为旧监听的在途结果。
    fn ensure_epoch(&self, epoch: u64) -> Result<(), NativeError> {
        if self.epoch() == epoch {
            Ok(())
        } else {
            Err(detached())
        }
    }

    /// 监听停用/销毁的共用清理：代际前进（在状态锁内，保证与 source/pump 变更原子），
    /// 停上游、作废旧凭据、丢弃会话订阅。调用方决定是否永久置 stopped。
    fn detach(&self) {
        let mut state = self.state_lock();
        // 代际与状态在同一把锁内推进：ensure_pump/commit 看到的
        // epoch 与 source/pump 永远属于同一代，不出现「新代际 + 旧 source」。
        self.epoch.fetch_add(1, Ordering::SeqCst);
        if let Some(pump) = state.pump.take() {
            let _ = pump.cancel.send(true);
            if let Some(handle) = pump.handle {
                handle.abort();
            }
        }
        if let Some(source) = state.source.take() {
            // 旧凭据立即作废：pause/shutdown 后不允许任何残留路径再用它打服务。
            source.client.invalidate();
            state.source_revision = state.source_revision.wrapping_add(1);
        }
        state.sessions.clear();
    }

    fn ensure_live(&self) -> Result<(), NativeError> {
        if self.stopped.load(Ordering::SeqCst) {
            Err(stopped())
        } else {
            Ok(())
        }
    }

    /// 当前代际的源：命中即复用；新代际则新建连接并重读契约。
    ///
    /// 会话感知：登记缺失（且仅缺失）时按目标会话回退**显式连接形态证据**
    /// （argv `--server`+`--session` + 进程事实 + 进程环境凭据，R0 §4d 单列）；
    /// 其余发现失败原样拒绝，不换证据源。
    async fn source_for_session(&self, session_id: &str) -> Result<Arc<Source>, NativeError> {
        self.source_for_session_pinned(session_id, None).await
    }

    async fn source_for_session_pinned(
        &self,
        session_id: &str,
        expected: Option<&NativeTarget>,
    ) -> Result<Arc<Source>, NativeError> {
        self.ensure_live()?;
        let epoch = self.epoch();
        let source_revision = self.state_lock().source_revision;
        if let Some(target) = expected {
            if target.session_id != session_id {
                return Err(NativeError::new(
                    codes::OC_BINDING_STALE,
                    "the fixed native target changed",
                ));
            }
            if let Some(cached) = self.current(&target.runtime) {
                if (self.process_fact)(target.runtime.pid) != Some(target.runtime.started_at_ms) {
                    return Err(NativeError::new(
                        codes::OC_RUNTIME_UNVERIFIED,
                        "the fixed native service process is unavailable",
                    ));
                }
                reverify_write_channel_identity(&cached).await?;
                self.ensure_epoch(epoch)?;
                if !self
                    .current(&target.runtime)
                    .is_some_and(|current| Arc::ptr_eq(&current, &cached))
                {
                    return Err(NativeError::new(
                        codes::OC_GENERATION_MISMATCH,
                        "native source changed during verification",
                    ));
                }
                return Ok(cached);
            }
            if self.state_lock().source.as_ref().is_some_and(|source| {
                source.descriptor.runtime != target.runtime || !source.client.is_invalidated()
            }) {
                return Err(NativeError::new(
                    codes::OC_BINDING_STALE,
                    "another native source is active",
                ));
            }
        }
        let descriptor = self.discover_for_session(session_id).await?;
        // 固定目标不能先安装新 source 再拒绝：那会清除原实例的监听和快照。
        if expected.is_some_and(|target| {
            target.runtime != descriptor.runtime || target.session_id != session_id
        }) {
            return Err(NativeError::new(
                codes::OC_BINDING_STALE,
                "the fixed native target changed",
            ));
        }
        if let Some(cached) = self.current(&descriptor.runtime) {
            if same_service_descriptor(&cached.descriptor, &descriptor) {
                return Ok(cached);
            }
        }
        let client = Arc::new(NativeClient::new(descriptor.clone())?);
        let openapi = client.read_json(OPENAPI_PATH, &[]).await?;
        // 能力表只承载结构性判定（路径/形状/决定词是否被契约支持）。写归属门是
        // 动态证据判定（R2 服务身份门），在 reserve/claim 前逐次核对，不在这里
        // 按任何静态归属状态预关写能力。
        let capabilities = evaluate_contract(&openapi);
        self.install_guarded(
            Arc::new(Source {
                descriptor,
                client,
                capabilities,
            }),
            epoch,
            Some(source_revision),
            expected.is_some(),
        )
    }

    // source 自己可能重发现同一服务；它的 epoch 不能替代调用方的监听代际。
    // 必须在最后一次 await 后复核，避免旧表单结果跨 pause/重连返回。
    #[cfg(test)]
    async fn source_for_epoch(
        &self,
        epoch: u64,
        session_id: &str,
    ) -> Result<Arc<Source>, NativeError> {
        let source = self.source_for_session(session_id).await?;
        self.ensure_epoch(epoch)?;
        Ok(source)
    }

    /// 登记发现优先；登记缺失（且仅缺失）时按会话回退显式形态证据。
    async fn discover_for_session(
        &self,
        session_id: &str,
    ) -> Result<ServiceDescriptor, NativeError> {
        match discover(&self.input, &read_state_file, &*self.process_fact) {
            Ok(descriptor) => Ok(descriptor),
            Err(err) if err.code == codes::OC_SERVICE_NOT_REGISTERED => {
                self.discover_explicit_service(session_id).await
            }
            Err(err) => Err(err),
        }
    }

    /// 显式形态服务发现（R0 §4d）：**客户端证据**（argv 端点 + 客户端环境凭据）与
    /// **接收服务代际**分别建模。服务身份锚取自 argv 指认端点的 `/api/info`
    /// （服务自身 pid/version/urls，凭据核对内含于该读），服务 pid 再经进程事实
    /// 取启动时间；**客户端 PID 绝不进入服务 RuntimeKey**（TUI 与服务是两个进程）。
    async fn discover_explicit_service(
        &self,
        session_id: &str,
    ) -> Result<ServiceDescriptor, NativeError> {
        let evidence = super::discovery::discover_explicit_client(
            session_id,
            &*self.explicit_scan,
            &*self.explicit_env,
        )?;
        let info = self
            .fetch_explicit_service_info(&evidence.endpoint, &evidence.password)
            .await?;
        let (service_pid, version) =
            super::discovery::explicit_service_identity(&info, &evidence.endpoint)?;
        let started_at_ms = (self.process_fact)(service_pid).ok_or_else(|| {
            NativeError::new(
                codes::OC_RUNTIME_UNVERIFIED,
                "explicit opencode service process start time is unavailable",
            )
        })?;
        // 代际锚的稳定串：as_str()（scheme+host+port+规范化路径）；显式形态无登记 id。
        let service_id = format!(
            "explicit:{}",
            evidence.endpoint.as_str().trim_end_matches('/')
        );
        Ok(ServiceDescriptor {
            runtime: RuntimeKey {
                service_id,
                pid: service_pid,
                started_at_ms,
            },
            endpoint: evidence.endpoint,
            password: evidence.password,
            version,
            explicit: true,
        })
    }

    /// 取显式端点的服务 `/api/info`（生产实现含阻塞 HTTP，放 blocking 池执行）。
    async fn fetch_explicit_service_info(
        &self,
        endpoint: &reqwest::Url,
        password: &str,
    ) -> Result<Value, NativeError> {
        let fetch = Arc::clone(&self.explicit_info);
        let endpoint = endpoint.clone();
        let password = password.to_string();
        tokio::task::spawn_blocking(move || (fetch)(endpoint, password))
            .await
            .map_err(|_| {
                NativeError::new(
                    codes::OC_TRANSPORT_FAILED,
                    "explicit opencode service info task failed",
                )
            })?
    }

    /// 候选预览（只读，零状态副作用）：发现 → **独立临时连接** GET → `bind_session` → 进程事实。
    /// 不安装 source、不作废旧连接、不清快照、不写路由行。
    async fn preview_candidate(
        &self,
        session: &Session,
        device_id: &str,
    ) -> Result<CandidatePreview, NativeError> {
        self.ensure_live()?;
        let epoch = self.epoch();
        let descriptor = self.discover_for_session(&session.id).await?;
        // 恒用独立临时连接：预览读到的 401 只作废临时对象。复用在册连接会让一次
        // 失败的预览把原连接置为 invalid（401 作废不受状态锁保护），破坏零副作用。
        let client = Arc::new(NativeClient::new(descriptor.clone())?);
        let info = client.read_json(INFO_PATH, &[]).await?;
        let session_path = format!("/api/session/{}", segment(&session.id)?);
        let native = client.read_json(&session_path, &[]).await?;
        let target = bind_session(session, &descriptor, &info, &native)?;
        if (self.process_fact)(target.runtime.pid) != Some(target.runtime.started_at_ms) {
            return Err(NativeError::new(
                codes::OC_RUNTIME_UNVERIFIED,
                "native candidate process is unavailable",
            ));
        }
        let binding_token = self.binding_token(&target);
        // 预览令牌必须一次性、不可预测：稳定 binding token 做键会让多设备互相覆盖，
        // 也会让过期 token 被同设备重刷续期。
        let preview_token = {
            let mut bytes = [0u8; 16];
            rand::fill(&mut bytes);
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        let ttl_ms = self.candidate_preview_ttl();
        let expires_at =
            std::time::Instant::now() + std::time::Duration::from_millis(ttl_ms as u64);
        {
            let mut pins = self
                .candidate_pins
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            pins.retain(|_, pin| pin.expires_at > std::time::Instant::now());
            pins.insert(
                preview_token.clone(),
                CandidatePin {
                    session_id: session.id.clone(),
                    device_id: device_id.to_string(),
                    window: self.preview_window(&target),
                    target: target.clone(),
                    descriptor,
                    epoch,
                    expires_at,
                },
            );
        }
        self.ensure_epoch(epoch)?;
        Ok(CandidatePreview {
            target,
            binding_token,
            preview_token,
            expires_at_ms: chrono::Utc::now().timestamp_millis() + ttl_ms,
        })
    }

    /// 取预览凭据：同设备、同会话、同监听代际且未过期才有效；否则保守拒绝（零变更）。
    fn candidate_pin(
        &self,
        token: &str,
        device_id: &str,
        session_id: &str,
        epoch: u64,
    ) -> Result<
        (
            NativeTarget,
            ServiceDescriptor,
            Option<super::binding_gate::LockedWindow>,
        ),
        NativeError,
    > {
        let mut pins = self
            .candidate_pins
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        pins.retain(|_, pin| pin.expires_at > std::time::Instant::now());
        let pin = pins.get(token).ok_or_else(|| {
            NativeError::new(
                codes::OC_BINDING_STALE,
                "the previewed native candidate is missing or expired",
            )
        })?;
        if pin.device_id != device_id || pin.session_id != session_id || pin.epoch != epoch {
            return Err(NativeError::new(
                codes::OC_BINDING_STALE,
                "the previewed native candidate belongs to another device or generation",
            ));
        }
        Ok((
            pin.target.clone(),
            pin.descriptor.clone(),
            pin.window.clone(),
        ))
    }

    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    fn candidate_preview_ttl(&self) -> i64 {
        use std::sync::atomic::Ordering;
        self.candidate_preview_ttl_ms.load(Ordering::SeqCst)
    }

    #[cfg(not(any(test, feature = "opencode-coordinator-tests")))]
    fn candidate_preview_ttl(&self) -> i64 {
        CANDIDATE_PREVIEW_TTL_MS
    }

    /// 仅协调器测试：缩短预览 TTL，避免过期用例真实等待。
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    pub fn set_candidate_preview_ttl_ms_for_tests(&self, ttl_ms: i64) {
        use std::sync::atomic::Ordering;
        self.candidate_preview_ttl_ms
            .store(ttl_ms, Ordering::SeqCst);
    }

    fn current(&self, runtime: &RuntimeKey) -> Option<Arc<Source>> {
        let state = self.state_lock();
        match &state.source {
            Some(source)
                if &source.descriptor.runtime == runtime && !source.client.is_invalidated() =>
            {
                Some(Arc::clone(source))
            }
            _ => None,
        }
    }

    /// 安装（或发现已有）本代际的源；换代际时作废旧连接、停旧上游、清投影缓存。
    ///
    /// 复用条件必须同时满足「同代际」与「未被作废」：401 之后 `current` 已经拒用旧源，
    /// 若这里只比 `RuntimeKey`，就会把刚作废的同代际源再还回去，形成
    /// 「每次 view 读一次 /openapi.json 然后立刻 OC_UNAUTHORIZED」的死循环。
    ///
    /// `epoch` 是调用方开始发现时的监听代际：pause/shutdown 之后才完成的发现不得
    /// 安装进新监听（否则旧 view 的迟到结果会换掉新 source）。
    #[cfg(test)]
    fn install(&self, source: Arc<Source>, epoch: u64) -> Result<Arc<Source>, NativeError> {
        self.install_guarded(source, epoch, None, false)
    }

    fn install_guarded(
        &self,
        source: Arc<Source>,
        epoch: u64,
        expected_revision: Option<u64>,
        pinned: bool,
    ) -> Result<Arc<Source>, NativeError> {
        let mut state = self.state_lock();
        if self.epoch() != epoch {
            source.client.invalidate();
            return Err(detached());
        }
        // 并发发现同一完整服务时复用胜出的连接；不同端点或凭据不能只凭 PID 复用。
        if let Some(existing) = &state.source {
            // 同一连接对象（重入复用/重复安装）：已作废的连接不能当安装成功返回，
            // 交由调用方按 stale 记录并留给后续读取惰性重装；未作废则幂等返回。
            if Arc::ptr_eq(&existing.client, &source.client) {
                if source.client.is_invalidated() {
                    return Err(NativeError::new(
                        codes::OC_BINDING_STALE,
                        "native source invalidated before installation",
                    ));
                }
                return Ok(Arc::clone(existing));
            }
            if same_service_descriptor(&existing.descriptor, &source.descriptor)
                && !existing.client.is_invalidated()
            {
                source.client.invalidate();
                return Ok(Arc::clone(existing));
            }
        }
        let revision_changed =
            expected_revision.is_some_and(|revision| revision != state.source_revision);
        let fixed_conflict = pinned
            && state.source.as_ref().is_some_and(|existing| {
                !same_service_descriptor(&existing.descriptor, &source.descriptor)
                    || !existing.client.is_invalidated()
            });
        if revision_changed || fixed_conflict {
            source.client.invalidate();
            return Err(NativeError::new(
                codes::OC_BINDING_STALE,
                "native source changed before installation",
            ));
        }
        if let Some(existing) = &state.source {
            existing.client.invalidate();
        }
        if let Some(pump) = state.pump.take() {
            // 只发取消：本函数可能正跑在该 pump 的重建里，abort 会打断自己。
            let _ = pump.cancel.send(true);
        }
        for entry in state.sessions.values_mut() {
            entry.snapshot = None;
        }
        state.source = Some(Arc::clone(&source));
        state.source_revision = state.source_revision.wrapping_add(1);
        Ok(source)
    }

    /// 一次完整只读重建（不发布）：所有读都走唯一 HTTP 通道。
    ///
    /// 逐能力设门：`evaluate_contract` 关掉的操作（缺路径/缺形状/未知词）只让**那一段**
    /// 返回空，其余段照常读——计划要求「缺能力仅限制对应操作」，不得连坐整份视图。
    async fn refresh(&self, session: &Session) -> Result<Snapshot, NativeError> {
        self.refresh_pinned(session, None).await
    }

    async fn refresh_pinned(
        &self,
        session: &Session,
        expected: Option<&NativeTarget>,
    ) -> Result<Snapshot, NativeError> {
        let raw = self.refresh_raw_pinned(session, expected).await?;
        Ok(Self::snapshot_from_raw(raw))
    }

    fn snapshot_from_raw(raw: RawSnapshot) -> Snapshot {
        Snapshot {
            source_client: raw.source_client,
            message_ids: raw.message_ids,
            tool_call_ids: raw.tool_call_ids,
            question_answers: raw.question_answers,
            session: raw.session,
            target: raw.target,
            capabilities: raw.capabilities,
            messages: raw.messages,
            files: raw.files,
            complete: raw.complete,
            partial_reason: raw.partial_reason,
            pending: raw.pending,
            inbox: raw.inbox,
            runtime_state: raw.runtime_state,
            service_url: raw.service_url,
            permission_rules: raw.permission_rules,
            permission_rules_known: raw.permission_rules_known,
            model: raw.model,
            binding_token: raw.binding_token,
        }
    }

    /// 一次完整只读重建的原始产物（保留消息的原生 `id`）。仅测试消费；
    /// 生产确认链路已改为临时来源读取。
    #[cfg(test)]
    async fn refresh_raw(&self, session: &Session) -> Result<RawSnapshot, NativeError> {
        self.refresh_raw_pinned(session, None).await
    }

    async fn refresh_raw_pinned(
        &self,
        session: &Session,
        expected: Option<&NativeTarget>,
    ) -> Result<RawSnapshot, NativeError> {
        if let Some(target) = expected {
            if std::path::Path::new(&session.project_path)
                .canonicalize()
                .is_ok_and(|directory| directory != target.directory)
            {
                return Err(NativeError::new(
                    codes::OC_BINDING_STALE,
                    "the fixed native target changed",
                ));
            }
        }
        let source = self
            .source_for_session_pinned(&session.id, expected)
            .await?;
        self.refresh_raw_from_source(session, expected, source)
            .await
    }

    async fn refresh_raw_from_source(
        &self,
        session: &Session,
        expected: Option<&NativeTarget>,
        source: Arc<Source>,
    ) -> Result<RawSnapshot, NativeError> {
        if expected.is_some_and(|target| {
            target.runtime != source.descriptor.runtime || target.session_id != session.id
        }) {
            return Err(NativeError::new(
                codes::OC_BINDING_STALE,
                "the fixed native target changed",
            ));
        }
        let info = source.client.read_json(INFO_PATH, &[]).await?;
        let session_path = format!("/api/session/{}", segment(&session.id)?);
        let native = source.client.read_json(&session_path, &[]).await?;
        let target = bind_session(session, &source.descriptor, &info, &native)?;
        if expected.is_some_and(|expected| expected != &target) {
            return Err(NativeError::new(
                codes::OC_BINDING_STALE,
                "the fixed native target changed",
            ));
        }
        let (permission_rules, permission_rules_known) = permission_rules_from(&native);
        let model = session_model_from(&native);
        let capabilities = &source.capabilities;
        let (records, complete, oversize_stopped) =
            if capability_allowed(capabilities, NativeOperationId::ReadMessages) {
                read_records(&source.client, &target).await?
            } else {
                // 关掉只读消息：不给历史，也不声称「读完整」（不可作投递证据）。
                (Vec::new(), false, false)
            };
        // D-N5-4：超限终止是可归因事实，与预算耗尽等其它部分原因如实区分。
        let partial_reason = if oversize_stopped {
            Some(codes::OC_RESPONSE_TOO_LARGE.to_owned())
        } else {
            None
        };
        let runtime_state = runtime_state(&records);
        let binding_token = self.binding_token(&target);
        // R5c：消息与附件同源产出；异常 files 形态只影响附件条目，不影响消息投影。
        let projection = project_records_with_seq(&records)?;
        let files = project_attachments(&records, &projection, Some(&target));
        let (pending, permissions_trusted) =
            if capability_allowed(capabilities, NativeOperationId::ReadPending) {
                read_pending(&source.client, &target, &binding_token, capabilities).await?
            } else {
                // 没读列表就谈不上「列表里没有」：缺席不可作负面证据。
                (Vec::new(), false)
            };
        let inbox = if capability_allowed(capabilities, NativeOperationId::ReadInbox) {
            read_inbox(&source.client, &target).await?
        } else {
            Vec::new()
        };
        Ok(RawSnapshot {
            source_client: Arc::clone(&source.client),
            message_ids: projection.message_ids,
            tool_call_ids: projection.tool_call_ids,
            question_answers: projection.question_answers,
            session: session.clone(),
            target,
            capabilities: source.capabilities.clone(),
            records,
            messages: projection.messages,
            files,
            complete,
            partial_reason,
            pending,
            permissions_trusted,
            inbox,
            runtime_state,
            service_url: source.descriptor.endpoint.to_string(),
            permission_rules,
            permission_rules_known,
            model,
            binding_token,
        })
    }

    /// 发布一次重建：修订号单调递增，watch 只收到这个数。
    ///
    /// `epoch` 是本次重建开始时所属的监听代际；pause/shutdown 之后旧代际的结果
    /// 一律拒绝（包括 pump 在取消前已经读完的那一份），不推进新监听的修订号。
    /// `binding_required` 由调用方按 R2 服务身份核验结果给出（快照构建成功只证明
    /// 发现链成立；多 service 冲突 / 两次读取间的代际漂移仍会拒绝新写）。
    fn commit(
        &self,
        snapshot: Snapshot,
        epoch: u64,
        publish_even_if_unchanged: bool,
        binding_required: bool,
    ) -> Result<PublicNativeView, NativeError> {
        self.ensure_live()?;
        let desktop_bound =
            desktop_client_bound(&snapshot.target.session_id, &snapshot.service_url);
        let snapshot = Arc::new(snapshot);
        let mut state = self.state_lock();
        if self.epoch() != epoch {
            return Err(detached());
        }
        // source 与投影在同一状态锁下比较，迟到的旧代际不得覆盖新视图。
        if !state.source.as_ref().is_some_and(|source| {
            source.descriptor.runtime == snapshot.target.runtime
                && Arc::ptr_eq(&source.client, &snapshot.source_client)
                && !source.client.is_invalidated()
        }) {
            return Err(NativeError::new(
                codes::OC_GENERATION_MISMATCH,
                "native source changed during projection",
            ));
        }
        Ok(Self::publish_locked(
            &mut state,
            snapshot,
            publish_even_if_unchanged,
            binding_required,
            desktop_bound,
        ))
    }

    /// 调用方已持有状态锁并完成代际校验；发布过程不再执行可失败的业务操作。
    fn publish_locked(
        state: &mut State,
        snapshot: Arc<Snapshot>,
        publish_even_if_unchanged: bool,
        binding_required: bool,
        desktop_bound: bool,
    ) -> PublicNativeView {
        let entry = state
            .sessions
            .entry(snapshot.target.session_id.clone())
            .or_insert_with(Entry::new);
        entry.selected_target = Some(snapshot.target.clone());
        let mut selected_session = snapshot.session.clone();
        selected_session.title = None;
        selected_session.git_branch = None;
        selected_session.github_url = None;
        selected_session.last_message = None;
        selected_session.last_message_role = None;
        entry.selected_session = Some(selected_session);
        // 修订 = **视图内容**的版本，不是“读了一次”的计数。纯 HTTP 读取若内容
        // 未变就不能发布新 revision：否则两个只读客户端会互相把对方的读取当成
        // “变更”，形成 SSE→GET→SSE 的持续重读（C1 的根因）。
        let unchanged = entry.snapshot.as_ref().is_some_and(|prev| {
            prev.capabilities == snapshot.capabilities
                && prev.messages == snapshot.messages
                && prev.message_ids == snapshot.message_ids
                && prev.files == snapshot.files
                && prev.complete == snapshot.complete
                && prev.pending == snapshot.pending
                && prev.inbox == snapshot.inbox
                && prev.runtime_state == snapshot.runtime_state
                && prev.binding_token == snapshot.binding_token
        });
        let revision = if unchanged && !publish_even_if_unchanged {
            entry.revision
        } else {
            entry.revision += 1;
            entry.snapshot = Some(Arc::clone(&snapshot));
            let revision = entry.revision;
            entry.tx.send_replace(revision);
            revision
        };
        let mut effective_capabilities = snapshot.capabilities.clone();
        if binding_required {
            for operation in [
                NativeOperationId::Send,
                NativeOperationId::Retract,
                NativeOperationId::PermissionOnce,
                NativeOperationId::PermissionReject,
                NativeOperationId::PermissionAlways,
                NativeOperationId::AnswerGroup,
                NativeOperationId::StopSession,
                NativeOperationId::SetAgent,
            ] {
                if let Some(capability) = effective_capabilities.get_mut(&operation) {
                    capability.allowed = false;
                    capability.reason_code =
                        Some(codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED.to_owned());
                }
            }
        }
        PublicNativeView {
            message_ids: snapshot.message_ids.clone(),
            session_id: snapshot.target.session_id.clone(),
            service_id: Some(snapshot.target.runtime.service_id.clone()),
            service_pid: Some(snapshot.target.runtime.pid),
            service_url: Some(snapshot.service_url.clone()),
            directory: Some(snapshot.target.directory.to_string_lossy().into_owned()),
            binding_required,
            desktop_bound,
            binding_token: snapshot.binding_token.clone(),
            revision,
            complete: snapshot.complete,
            partial_reason: snapshot.partial_reason.clone(),
            capabilities: effective_capabilities,
            messages: snapshot.messages.clone(),
            pending: snapshot.pending.clone(),
            inbox: snapshot.inbox.clone(),
            runtime_state: snapshot.runtime_state.clone(),
            model: snapshot.model.clone(),
            permission_rules: snapshot.permission_rules.clone(),
            permission_rules_known: snapshot.permission_rules_known,
            files: snapshot.files.clone(),
            tool_call_ids: snapshot.tool_call_ids.clone(),
            question_answers: snapshot.question_answers.clone(),
        }
    }

    /// binding token：服务代际 + 会话 id + 归一目录 + 进程内随机锚。
    /// 消息增长、pending 变化都不参与，因此不会在回答途中失效。
    fn binding_token(&self, target: &NativeTarget) -> String {
        let mut hasher = Sha256::new();
        hash_text(&mut hasher, "binding");
        hash_text(&mut hasher, &format!("{:?}", self.secret));
        hash_text(&mut hasher, &target.runtime.service_id);
        hash_text(&mut hasher, &target.runtime.pid.to_string());
        hash_text(&mut hasher, &target.runtime.started_at_ms.to_string());
        hash_text(&mut hasher, &target.session_id);
        hash_text(&mut hasher, &target.directory.to_string_lossy());
        short_hex(hasher)
    }

    /// 有订阅者且没有在飞的投影时才拉起上游；一个代际最多一条。
    ///
    /// 「检查 + 占位」在同一次锁内完成：并发调用者要么看到占位（同代际直接返回），
    /// 要么看到已被摘除（自己成为唯一的新上游）。因此不会出现两条流同时被视为在册、
    /// 也不会出现「后写者覆盖前者、前者的任务再没人能停」的孤儿流。
    fn ensure_pump(self: &Arc<Self>) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            // 没有运行时上下文（同步调用方）：等下一次 view 再拉起。
            return;
        };

        let (client, generation, epoch, cancel, replaced) = {
            let mut state = self.state_lock();
            if self.stopped.load(Ordering::SeqCst) {
                return;
            }
            let Some(source) = state.source.as_ref() else {
                return;
            };
            let generation = source.descriptor.runtime.clone();
            // 与 source/pump 同锁读取：pause 在同一把锁内推进代际并摘走 source，
            // 因此这里的 epoch 与新上游必然属于同一代。
            let epoch = self.epoch();
            if state
                .pump
                .as_ref()
                .is_some_and(|pump| pump.generation == generation)
            {
                return;
            }
            if !state
                .sessions
                .values()
                .any(|entry| entry.tx.receiver_count() > 0)
            {
                return;
            }
            let client = Arc::clone(&source.client);
            let cancel = Arc::new(watch::Sender::new(false));
            // 先登记占位（handle 为空），再在锁外 spawn。
            let replaced = state.pump.replace(Pump {
                generation: generation.clone(),
                cancel: Arc::clone(&cancel),
                handle: None,
            });
            (client, generation, epoch, cancel, replaced)
        };
        if let Some(pump) = replaced {
            let _ = pump.cancel.send(true);
            if let Some(handle) = pump.handle {
                handle.abort();
            }
        }

        let (events_tx, events_rx) = mpsc::channel(8);
        let task = Arc::clone(self);
        let task_cancel = Arc::clone(&cancel);
        let stop = cancel.subscribe();
        let task_generation = generation.clone();
        let task_epoch = epoch;
        let handle = runtime.spawn(async move {
            task.run_pump(PumpInputs {
                client,
                generation: task_generation,
                epoch: task_epoch,
                cancel: task_cancel,
                stop,
                events_tx,
                events: events_rx,
            })
            .await;
        });

        let mut state = self.state_lock();
        let still_ours = state
            .pump
            .as_ref()
            .is_some_and(|pump| Arc::ptr_eq(&pump.cancel, &cancel));
        if !still_ours {
            // spawn 期间占位被 install/shutdown 摘走：不留没人能停的孤儿流。
            drop(state);
            let _ = cancel.send(true);
            handle.abort();
            return;
        }
        if handle.is_finished() {
            // 极短窗口内上游已自行结束：不要在状态里留一个死句柄。
            state.pump = None;
        } else if let Some(pump) = state.pump.as_mut() {
            pump.handle = Some(handle);
        }
    }

    /// SSE 上游 + 事件驱动的只读重建。事件只用来「提示有变化」，内容一律另读。
    async fn run_pump(self: Arc<Self>, input: PumpInputs) {
        let PumpInputs {
            client,
            generation,
            epoch,
            cancel,
            mut stop,
            events_tx,
            mut events,
        } = input;
        let mut upstream = Box::pin(client.read_events(cancel.subscribe(), events_tx));
        let mut ticker = tokio::time::interval(REAP_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            if *stop.borrow() {
                break;
            }
            tokio::select! {
                _ = stop.changed() => break,
                _ = ticker.tick() => {
                    if self.take_idle_pump(&cancel) {
                        break;
                    }
                }
                event = events.recv() => {
                    match event {
                        // 上游结束（取消或出错）时本任务也收尾。
                        None => break,
                        Some(key) => {
                            // 陈旧代际的事件不参与重建（传输层已守卫，这里再挡一次）。
                            if key != generation {
                                continue;
                            }
                            // 合并连续事件：只做一次只读重建。
                            while events.try_recv().is_ok() {}
                            self.rebuild_watched_for_pump(epoch, Some((&cancel, &client))).await;
                        }
                    }
                }
                outcome = &mut upstream => {
                    let invalidated = matches!(&outcome, Err(err) if err.code == codes::OC_UNAUTHORIZED);
                    self.detach_pump(&cancel);
                    #[cfg(test)]
                    let hold = if invalidated { self.pump_retire_test_hold.lock().unwrap().clone() } else { None };
                    #[cfg(test)]
                    if let Some(hold) = &hold { hold.started.notify_one(); hold.release.notified().await; }
                    self.retire_source(&generation, &client, invalidated, epoch);
                    #[cfg(test)]
                    if let Some(hold) = &hold { hold.finished.notify_one(); }
                    return;
                }
            }
        }
        let _ = cancel.send(true);
        let _ = tokio::time::timeout(PUMP_DRAIN, &mut upstream).await;
        self.detach_pump(&cancel);
        self.retire_source(&generation, &client, false, epoch);
    }

    /// 空闲判定与摘除是同一次锁内原子操作：要么新订阅在检查前被看见（不释放），
    /// 要么在摘除之后才登记（`ensure_pump` 必然看到空槽并拉起新流）。
    /// 二者之间没有「已判定空闲但还在册」的窗口，因此不会丢掉上游。
    fn take_idle_pump(&self, cancel: &Arc<watch::Sender<bool>>) -> bool {
        if self.views.load(Ordering::SeqCst) > 0 {
            return false;
        }
        let mut state = self.state_lock();
        if state
            .sessions
            .values()
            .any(|entry| entry.tx.receiver_count() > 0)
        {
            return false;
        }
        // 只摘自己：同代际可能已经换过一条新流，不能把别人的摘掉。
        if !state
            .pump
            .as_ref()
            .is_some_and(|pump| Arc::ptr_eq(&pump.cancel, cancel))
        {
            return false;
        }
        for entry in state.sessions.values_mut() {
            entry.snapshot = None;
        }
        state.pump = None;
        true
    }

    /// 对每个有订阅者的会话做一次只读重建；失败保留上次快照，不发布修订。
    ///
    /// `epoch` 是本条上游所属的监听代际：pause 之后即使重建已经读完，`commit` 也会
    /// 以代际不等拒绝，旧 pump 的结果不会出现在新监听的 watch 上。
    #[cfg(test)]
    async fn rebuild_watched(&self, epoch: u64) {
        self.rebuild_watched_for_pump(epoch, None).await;
    }

    async fn rebuild_watched_for_pump(
        &self,
        epoch: u64,
        pin: Option<(&Arc<watch::Sender<bool>>, &Arc<NativeClient>)>,
    ) {
        let sessions: Vec<(Session, NativeTarget)> = {
            let state = self.state_lock();
            state
                .sessions
                .values()
                .filter(|entry| entry.tx.receiver_count() > 0)
                .filter_map(|entry| {
                    Some((
                        entry.selected_session.as_ref()?.clone(),
                        entry.selected_target.as_ref()?.clone(),
                    ))
                })
                .collect()
        };
        for (session, target) in sessions {
            if let Some((cancel, client)) = pin {
                let state = self.state_lock();
                if *cancel.borrow()
                    || !state
                        .pump
                        .as_ref()
                        .is_some_and(|pump| Arc::ptr_eq(&pump.cancel, cancel))
                    || !state.source.as_ref().is_some_and(|source| {
                        Arc::ptr_eq(&source.client, client) && !client.is_invalidated()
                    })
                {
                    return;
                }
            }
            let route_before = match self
                .store
                .with(|conn| crate::database::dao::opencode_actions::route_conn(conn, &session.id))
            {
                Ok(route) => route,
                Err(_) => continue,
            };
            // 已有路由必须仍是该目标的 active 归属；无路由仅用于未认领只读视图。
            if route_before.as_ref().is_some_and(|route| {
                route.route_state != crate::database::dao::opencode_actions::ROUTE_ACTIVE
                    || !route.same_runtime(&target)
            }) {
                continue;
            }
            #[cfg(test)]
            if pin.is_some() {
                let hold = self.pump_after_route_test_hold.lock().unwrap().clone();
                if let Some(hold) = hold {
                    hold.pass().await;
                }
            }
            let snapshot = if let Some((cancel, client)) = pin {
                if (self.process_fact)(target.runtime.pid) != Some(target.runtime.started_at_ms) {
                    let _ = cancel.send(true);
                    return;
                }
                let source = {
                    let state = self.state_lock();
                    if self.epoch() != epoch
                        || *cancel.borrow()
                        || !state
                            .pump
                            .as_ref()
                            .is_some_and(|pump| Arc::ptr_eq(&pump.cancel, cancel))
                    {
                        return;
                    }
                    let Some(source) = state.source.as_ref().filter(|source| {
                        Arc::ptr_eq(&source.client, client) && !client.is_invalidated()
                    }) else {
                        return;
                    };
                    Arc::clone(source)
                };
                let raw = match self
                    .refresh_raw_from_source(&session, Some(&target), source)
                    .await
                {
                    Ok(raw) => raw,
                    Err(error) => {
                        // 身份失配（端点自报换了 pid/版本/目录）证明原固定目标已无法
                        // 证明有效：明确结束这条流，而不是反复静默跳过重建（既不发
                        // 有效更新也不结束）。传输类失败保留快照，下一 tick 有界重试。
                        if matches!(
                            error.code.as_str(),
                            codes::OC_GENERATION_MISMATCH
                                | codes::OC_VERSION_MISMATCH
                                | codes::OC_DIRECTORY_MISMATCH
                                | codes::OC_DIRECTORY_UNRESOLVED
                        ) {
                            if let Some((cancel, _)) = pin {
                                let _ = cancel.send(true);
                            }
                            return;
                        }
                        continue;
                    }
                };
                Self::snapshot_from_raw(raw)
            } else {
                let Ok(snapshot) = self.refresh_pinned(&session, Some(&target)).await else {
                    continue;
                };
                snapshot
            };
            // 泵只能沿用自己的连接；身份发现失败时不重新发现或安装 source。
            // H03（N1）：下面的身份判定仍完整执行（代际失配的断流控制保留），
            // 但门装配时**展示态**不再消费其归属结论——active 路由经 SSE 更新
            // 不得被重新宣告 bindingRequired（归属证明逐次由写权链承担）。
            let legacy_verified = if pin.is_some() {
                match self.verify_service_identity(&snapshot.target).await {
                    Ok(()) => true,
                    Err(error) if error.code == codes::OC_GENERATION_MISMATCH => {
                        // 登记指向另一端点时，已固定来源仍可经进程事实和 /api/info 只读核验。
                        // 同端点换代则无法证明仍是旧来源，主动结束流而非保持静默。
                        let endpoint = {
                            let state = self.state_lock();
                            state
                                .source
                                .as_ref()
                                .filter(|source| {
                                    Arc::ptr_eq(&source.client, &snapshot.source_client)
                                })
                                .map(|source| source.descriptor.endpoint.clone())
                        };
                        match (endpoint, self.discover_for_session(&session.id).await) {
                            (Some(original), Ok(discovered))
                                if discovered.endpoint.origin() != original.origin() =>
                            {
                                false
                            }
                            _ => {
                                if let Some((cancel, _)) = pin {
                                    let _ = cancel.send(true);
                                }
                                return;
                            }
                        }
                    }
                    Err(_) => false,
                }
            } else {
                let Ok(verified) = self.view_ownership_for(&snapshot.target, true).await else {
                    continue;
                };
                verified
            };
            let verified = if self.binding_gate.is_some() {
                true
            } else {
                legacy_verified
            };
            let desktop_bound =
                desktop_client_bound(&snapshot.target.session_id, &snapshot.service_url);
            let _route_permit = self.legacy_lock(&session.id).read_owned().await;
            if let Some((cancel, _)) = pin {
                if (self.process_fact)(target.runtime.pid) != Some(target.runtime.started_at_ms) {
                    let _ = cancel.send(true);
                    return;
                }
            }
            let snapshot = Arc::new(snapshot);
            self.store.with(|conn| {
                let mut state = self.state_lock();
                if self.ensure_live().is_err() || self.epoch() != epoch {
                    return;
                }
                if let Some((cancel, client)) = pin {
                    if *cancel.borrow()
                        || !state
                            .pump
                            .as_ref()
                            .is_some_and(|pump| Arc::ptr_eq(&pump.cancel, cancel))
                        || !Arc::ptr_eq(client, &snapshot.source_client)
                    {
                        return;
                    }
                    // 进程事实在**拿到数据库锁之后**复核：等待锁期间 PID 退出或复用
                    // 不得发布旧代际（与 verify 的锁后复核同口径）。
                    if (self.process_fact)(target.runtime.pid) != Some(target.runtime.started_at_ms)
                    {
                        let _ = cancel.send(true);
                        return;
                    }
                }
                let Ok(current_route) =
                    crate::database::dao::opencode_actions::route_conn(conn, &session.id)
                else {
                    return;
                };
                if current_route != route_before
                    || !state.source.as_ref().is_some_and(|source| {
                        source.descriptor.runtime == target.runtime
                            && Arc::ptr_eq(&source.client, &snapshot.source_client)
                            && !source.client.is_invalidated()
                    })
                {
                    return;
                }
                let _ = Self::publish_locked(&mut state, snapshot, true, !verified, desktop_bound);
            });
        }
    }

    /// 只摘「自己」这条流：同代际可能已有更新的流在册。
    fn detach_pump(&self, cancel: &Arc<watch::Sender<bool>>) {
        let mut state = self.state_lock();
        if state
            .pump
            .as_ref()
            .is_some_and(|pump| Arc::ptr_eq(&pump.cancel, cancel))
        {
            state.pump = None;
        }
    }

    /// 上游因凭据失效结束时，丢掉本代际的源：下一次 `view` 走只读重新发现。
    ///
    /// `epoch` 是这条上游建立时的监听代际：旧代际的 pump 收尾时不得摘掉新监听的
    /// source（pause 后重新发现可能拿到同一 RuntimeKey 的新连接）。
    fn retire_source(
        &self,
        generation: &RuntimeKey,
        client: &Arc<NativeClient>,
        invalidated: bool,
        epoch: u64,
    ) {
        let mut state = self.state_lock();
        if self.epoch() != epoch {
            return;
        }
        let stale = invalidated
            || state
                .source
                .as_ref()
                .is_some_and(|s| s.client.is_invalidated());
        if stale
            && state.source.as_ref().is_some_and(|s| {
                &s.descriptor.runtime == generation && Arc::ptr_eq(&s.client, client)
            })
        {
            state.source = None;
            state.source_revision = state.source_revision.wrapping_add(1);
        }
    }
}

/// 全量/分页消息读：按原生 id 去重（后到者覆盖），权威终止才算完整。
///
/// 首页**非超限**失败即失败（连会话是否可读都无法确认）；后续页失败保留已读部分并标记
/// 不完整；首页超限且降到 limit=1 仍超限时按部分历史返回（空记录 + `complete=false`），
/// 不上抛——超限不是「会话不可读」，但也绝不能算完整。**401 例外**：凭据代际已消失不是
/// 「这一页读不到」，任何一页都必须上抛（同时让调用方走重新发现），不能被降级成部分历史。
///
/// 终止规则（任务 2 裁定 R8，对应跨平台实测形状）：
/// * 首页 `order=asc`（不带 cursor）；后续页只带 `limit`+`cursor`（不再带 `order`）；
/// * 游标读回同时支持标量 `cursor`（Windows 2.2.2）与 `cursor.next`（Mac 2.0.22）；
/// * `data` 为空 ⇒ 权威终止且 `complete=true`——**即使回执还带 token**；
/// * 游标字段缺失或为 `null`（文档尾页形状）⇒ 读到尾页，`complete=true`；
/// * 游标字段存在但畸形（空串/数字/布尔/数组/`next` 缺失、null 或非字符串）⇒ 保守终止且
///   `complete=false`——存在一个读不懂的游标不能证明读完；
/// * 重复 token / 单页没有新增记录 id ⇒ 无进展终止且 `complete=false`
///   （服务端可能静默忽略游标并回默认页，翻页预算不是进度证明）；
/// * 最多 [`MAX_MESSAGE_PAGES`] 页，预算停止 ⇒ `complete=false`。
///
/// 响应超限（任务 3 裁定 R3）：单页体超过整包预算时**对同一 cursor** 折半 `limit`
/// 重试（20→10→5→2→1）；limit=1 仍超限 ⇒ 停止、保留已读记录、`complete=false`。
/// 重试不占成功页预算，但受 [`MAX_MESSAGE_PAGE_ATTEMPTS`] 总尝试上限约束。降档只作用于
/// 触发超限的那一页：换到新 cursor 后 limit 回到 20，避免一次大页把后续所有页都压小
/// 而烧穿 20 页预算、把本可完整的历史误报成不完整。
///
/// 读回**原始记录**（分页去重前的原生形态）：只有原始记录带原生 `id`，
/// 而 `Confirmed` 的唯一依据正是这个 id 出现在消息列表里。
///
/// 第三返回值 = 超限终止标注（D-N5-4，语义见 [`read_records_with`]）。
async fn read_records(
    client: &NativeClient,
    target: &NativeTarget,
) -> Result<(Vec<Value>, bool, bool), NativeError> {
    read_records_with(
        client,
        target,
        MAX_MESSAGE_PAGES,
        MAX_MESSAGE_PAGE_ATTEMPTS,
        MAX_RESPONSE_BYTES,
    )
    .await
}

/// [`read_records`] 的可注入预算版本（生产固定 20 页 / 100 次尝试 / 32 MiB 整包）：
/// `page_budget` 是成功页数上限，`attempt_budget` 是页请求总数上限（含降档重试），
/// `response_budget` 是单页响应体预算。
///
/// 响应预算单列出来只为一个目的：让「超限 → 折半 → 继续」能在几 MB 的小夹具上被证明，
/// 不必生成 >32 MiB 的真实响应。生产调用方见 [`read_records`]。
/// 返回值第三项 = **枚举因「单页超限且降到 limit=1 仍超限」终止**（D-N5-4）：
/// 视图据此把「部分内容过大未载入」与预算耗尽等其它部分历史原因区分开，如实标注。
async fn read_records_with(
    client: &NativeClient,
    target: &NativeTarget,
    page_budget: usize,
    attempt_budget: usize,
    response_budget: usize,
) -> Result<(Vec<Value>, bool, bool), NativeError> {
    let path = format!("/api/session/{}/message", segment(&target.session_id)?);
    let mut records: Vec<Value> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut seen_cursors: HashSet<String> = HashSet::new();
    let mut complete = false;
    let mut oversize_stopped = false;
    let mut cursor: Option<String> = None;
    let mut limit = MESSAGE_LIMIT;
    let mut pages = 0usize;
    let mut attempts = 0usize;
    while pages < page_budget && attempts < attempt_budget {
        let mut query = vec![("limit".to_string(), limit.to_string())];
        match cursor.as_deref() {
            // 后续页只带 limit+cursor；首页（含首页降档重试）保持 order=asc、不带 cursor。
            Some(token) => query.push(("cursor".to_string(), token.to_string())),
            None => query.push(("order".to_string(), "asc".to_string())),
        }
        attempts += 1;
        let page = match client
            .read_json_within(&path, &query, response_budget)
            .await
        {
            Ok(page) => page,
            // 超限页不是「读到空」：同一 cursor 折半 limit 再试，不消耗成功页预算。
            Err(err) if err.code == codes::OC_RESPONSE_TOO_LARGE => {
                match next_limit(limit) {
                    Some(next) => {
                        limit = next;
                        continue;
                    }
                    // limit=1 仍超限：停止，保留已读记录，complete 保持 false。
                    // D-N5-4：终止原因 = 超限，必须如实标注（区别于预算耗尽等其它
                    // 部分历史原因——「部分内容过大未载入」是可归因事实，不冒充）。
                    None => {
                        oversize_stopped = true;
                        break;
                    }
                }
            }
            // 401：凭据已作废，任何一页都得上抛；其它错误只让本次读变成「部分历史」。
            Err(err) if cursor.is_none() || err.code == codes::OC_UNAUTHORIZED => return Err(err),
            Err(_) => break,
        };
        pages += 1;
        let items = page
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| contract_missing("native message page has no data array"))?;
        // 终止条件按优先级：空页（true）→ 无新增记录（false）→ 文档尾页形状（true）
        // → 存在但畸形的游标（false）→ 重复 token（false）→ 20 页预算（false）。
        // 空页最高优先：这一页带不带 token 都不再追。
        if items.is_empty() {
            complete = true;
            break;
        }
        let before = records.len();
        for item in items {
            merge_record(&mut records, &mut seen, item);
        }
        // 没有新增记录 = 服务端没有前进（重复页/忽略游标）：只能按部分历史收场
        // （`complete` 初始即 false，这里不再重复赋值）。
        if records.len() == before {
            break;
        }
        match next_cursor(&page) {
            PageCursor::End => {
                complete = true;
                break;
            }
            // 存在但畸形的游标（空串/数字/布尔/数组/`next` 非字符串）既不是「无游标」
            // 也不能证明读到尾部：保守按部分历史。
            PageCursor::Empty => break,
            PageCursor::Next(next) => {
                // 重复 token 立即停止：绝不靠翻页预算兜底。
                if !seen_cursors.insert(next.to_string()) {
                    break;
                }
                if pages >= page_budget {
                    // 预算耗尽：循环自然结束，`complete` 保持 false。
                    break;
                }
                cursor = Some(next.to_string());
                // 降档只修这一页：新 cursor 从默认 limit 重新开始。
                limit = MESSAGE_LIMIT;
            }
        }
    }
    Ok((records, complete, oversize_stopped))
}

/// 降档阶梯的下一档：`20→10→5→2→1`；已是 1（或不在阶梯上）⇒ 没有更小的档。
fn next_limit(current: usize) -> Option<usize> {
    MESSAGE_LIMIT_LADDER
        .iter()
        .copied()
        .find(|limit| *limit < current)
}

/// 一页回执给出的游标语义。
enum PageCursor<'a> {
    /// 文档尾页形状：游标字段缺失或为标量 `null`。本页即尾页。
    End,
    /// 有效 token：可继续翻页。
    Next(&'a str),
    /// 游标字段**存在但畸形**（空串/数字/布尔/数组，或对象的 `next` 缺失/为 null/
    /// 非字符串）：既不是「无游标」也不能证明读到尾部，按未完成终止。
    Empty,
}

/// 一页回执的 next 游标：标量 `cursor`（Windows 2.2.2）或 `cursor.next`（Mac 2.0.22）。
///
/// 标量形态在 Windows 实测里是唯一被接受的参数；deepObject 形态只作为**回执**出现。
/// 两种形态归一为同一规则：
/// * 字段缺失或 `cursor: null`（文档尾页形状）⇒ [`PageCursor::End`]；
/// * 非空标量字符串 / 对象里非空字符串 `next` ⇒ [`PageCursor::Next`]；
/// * 其余**存在但畸形**的形态（空串、数字、布尔、数组、`next` 缺失/null/非字符串）
///   ⇒ [`PageCursor::Empty`]——不能证明读完，绝不当作尾页。
fn next_cursor(page: &Value) -> PageCursor<'_> {
    match page.get("cursor") {
        None | Some(Value::Null) => PageCursor::End,
        Some(Value::String(token)) if !token.is_empty() => PageCursor::Next(token),
        Some(Value::Object(cursor)) => match cursor.get("next") {
            Some(Value::String(next)) if !next.is_empty() => PageCursor::Next(next),
            _ => PageCursor::Empty,
        },
        _ => PageCursor::Empty,
    }
}

fn merge_record(records: &mut Vec<Value>, seen: &mut HashMap<String, usize>, item: &Value) {
    match item.get("id").and_then(Value::as_str) {
        Some(id) => match seen.get(id) {
            // 重叠页：同一 id 只保留一份（用后到的最新形态），位置保持首次出现处。
            Some(&index) => records[index] = item.clone(),
            None => {
                seen.insert(id.to_string(), records.len());
                records.push(item.clone());
            }
        },
        // 无 id 的记录无法去重，也不可作为身份证据：按序保留。
        None => records.push(item.clone()),
    }
}

/// pending：先读会话级端点；不可用时回退全局读，且必须带绑定目录并核对回执目录。
///
/// 第二个返回值是**审批列表的负面证据可信度**（见 [`permissions_from`]）：`false` 时
/// 「pending 里没有该 requestId」不能当作「审批请求已消失」。
async fn read_pending(
    client: &NativeClient,
    target: &NativeTarget,
    binding_token: &str,
    capabilities: &Capabilities,
) -> Result<(Vec<PendingView>, bool), NativeError> {
    let mut pending = read_forms(client, target, binding_token).await?;
    let (permissions, trusted) =
        read_permissions(client, target, binding_token, capabilities).await?;
    pending.extend(permissions);
    Ok((pending, trusted))
}

/// pending 表单：会话级端点优先，不可用时回退全局读（必须带绑定目录）。
async fn read_forms(
    client: &NativeClient,
    target: &NativeTarget,
    binding_token: &str,
) -> Result<Vec<PendingView>, NativeError> {
    let session = format!("/api/session/{}", segment(&target.session_id)?);
    let forms = match client.read_json(&format!("{session}/form"), &[]).await {
        Ok(value) => forms_from(&value, &target.session_id)?,
        Err(err) if err.code == codes::OC_UNAUTHORIZED => return Err(err),
        Err(_) => forms_from(
            &scoped_read(client, target, GLOBAL_FORM_PATH).await?,
            &target.session_id,
        )?,
    };
    Ok(forms
        .into_iter()
        .map(|form| PendingView::Form {
            context_token: context_token(binding_token, "form", &form.id, &form.content_hash),
            id: form.id,
            title: form.title,
            fields: form.fields,
            // 列表读没有权威状态（见 `forms_from` 注释）：不猜 pending，也不猜 answered。
            state: "unknown".to_string(),
        })
        .collect())
}

/// pending 审批：会话级端点优先，不可用时回退全局读（必须带绑定目录）。
///
/// 审批的**权威状态就是「还在不在这份列表里」**（任务 0：`reply` 204 后 pending 清空），
/// 因此投影与派发路径都以本函数的列表为审批的当前事实。但「不在列表里」只有在列表
/// **完整可信**时才是「请求已消失」：任一不可证归属项都会把可信度降为 false（R3），
/// 调用方（`decide_outcome`/`verify_decide`）不得据此落 Confirmed/Settled。
async fn read_permissions(
    client: &NativeClient,
    target: &NativeTarget,
    binding_token: &str,
    capabilities: &Capabilities,
) -> Result<(Vec<PendingView>, bool), NativeError> {
    let session = format!("/api/session/{}", segment(&target.session_id)?);
    let (permissions, trusted) = match client
        .read_json(&format!("{session}/permission"), &[])
        .await
    {
        Ok(value) => permissions_from(&value, &target.session_id)?,
        Err(err) if err.code == codes::OC_UNAUTHORIZED => return Err(err),
        Err(_) => permissions_from(
            &scoped_read(client, target, GLOBAL_PERMISSION_PATH).await?,
            &target.session_id,
        )?,
    };
    let decisions = allowed_decisions(capabilities);
    let views = permissions
        .into_iter()
        .map(|permission| PendingView::Permission {
            context_token: context_token(
                binding_token,
                "permission",
                &permission.id,
                &permission.title,
            ),
            id: permission.id,
            title: permission.title,
            decisions: decisions.clone(),
        })
        .collect();
    Ok((views, trusted))
}

/// 表单详情（派发期权威读）：解析产物 + 权威状态。
///
/// 只有 detail 端点带 `state.status`（列表读没有，见 [`forms_from`] 注释）：这是「表单
/// 是否已定案」的**唯一**权威来源。`PendingView::Form.state` 恒为 `"unknown"`，
/// 绝不可拿它当派发依据。
pub(crate) struct FormDetail {
    /// 解析产物（`id` + `content_hash` + 字段）：内容锚与投影同源。
    pub spec: FormSpec,
    /// 官方 `metadata.tool.{messageID,id}`：表单与提问位置的关联。两个字段都不可读时
    /// `None`——调用方不得用启发式补位（MCP 表单等没有该 metadata）。
    pub tool_link: Option<(String, String)>,
    /// `state.status` 原词：只有 `answered` / `cancelled` 是已支持终态；`pending` =
    /// 仍未定案；其它词或 `None` = 无法判定（R3：不得当作已定案）。
    pub status: Option<String>,
    /// `state.answer` 原值（官方答案；只有已答时才有意义）。保留原始 JSON 形态，
    /// 由 R5b 的结果映射决定可展示形状——不在这里猜字段类型。
    pub answer: Option<Value>,
}

/// 读表单 detail：派发前复核与回读都走它（权威状态 + 当前内容锚 + 官方答案）。
pub(crate) async fn read_form_detail(
    client: &NativeClient,
    target: &NativeTarget,
    form_id: &str,
) -> Result<FormDetail, NativeError> {
    let path = format!(
        "/api/session/{}/form/{}",
        segment(&target.session_id)?,
        segment(form_id)?
    );
    let value = client.read_json(&path, &[]).await?;
    let spec = parse_form(&value)?;
    let status = value
        .pointer("/data/state/status")
        .and_then(Value::as_str)
        .map(str::to_string);
    let answer = value
        .pointer("/data/state/answer")
        .filter(|answer| !answer.is_null())
        .cloned();
    Ok(FormDetail {
        spec,
        tool_link: form_tool_link(&value)
            .map(|(message_id, call_id)| (message_id.to_string(), call_id.to_string())),
        status,
        answer,
    })
}

/// 表单 detail → 只读结果 DTO。
///
/// 状态只认四个词（`unknown` 兜底，不冒充已答/已取消）；**只有 `answered` 才携带**答案：
/// 未定案、已取消与未知状态下的 `state.answer` 都不是可展示的定案结果。
fn form_result_from(
    session_id: &str,
    form_id: &str,
    spec: &FormSpec,
    status: Option<&str>,
    answer: Option<&Value>,
    tool_link: Option<(&str, &str)>,
) -> NativeFormResult {
    let status = match status {
        Some("answered") => "answered",
        Some("cancelled") => "cancelled",
        Some("pending") => "pending",
        _ => "unknown",
    };
    let answers = if status == "answered" {
        answer
            .and_then(Value::as_object)
            .map(|answers| {
                answers
                    .iter()
                    .map(|(key, value)| (key.clone(), form_answer_value(value)))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        BTreeMap::new()
    };
    let readable_answers = if status == "answered" {
        readable_answers_from(spec, answer)
    } else {
        Vec::new()
    };
    NativeFormResult {
        form_id: form_id.to_string(),
        session_id: session_id.to_string(),
        status: status.to_string(),
        answers,
        tool_message_id: tool_link.map(|(message_id, _)| message_id.to_string()),
        tool_call_id: tool_link.map(|(_, call_id)| call_id.to_string()),
        readable_answers,
    }
}

/// 官方答案 → 可读文本（题目 + 选项 label），按表单字段顺序。
///
/// 映射规则（每条都可证伪，不做启发式）：
/// * 字段有选项集且答案值命中某项 `value` ⇒ 显示该选项 `label`；
/// * 未命中（`custom:true` 的自由文本或旧值）⇒ 原样显示答案值；
/// * 多选 ⇒ 逐项按上面两条解析后以 `、` 并列；
/// * 字段顺序取表单规格，不在答案对象里猜顺序；
/// * 答案里有、规格里没有的键 ⇒ 忽略（内部键不上屏）。
fn readable_answers_from(spec: &FormSpec, answer: Option<&Value>) -> Vec<NativeFormAnswerView> {
    let Some(answers) = answer.and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for field in &spec.fields {
        let Some(value) = answers.get(&field.key) else {
            continue;
        };
        let title = if field.title.trim().is_empty() {
            continue; // 没有题目文本就不展示（避免退化成内部 key）
        } else {
            field.title.trim().chars().take(200).collect::<String>()
        };
        let labels: Vec<String> = match value {
            Value::String(text) => vec![option_label_or_value(field, text)],
            Value::Array(items) if items.iter().all(Value::is_string) => items
                .iter()
                .filter_map(Value::as_str)
                .map(|text| option_label_or_value(field, text))
                .collect(),
            other => vec![other.to_string()],
        }
        .into_iter()
        .filter(|text| !text.trim().is_empty())
        .collect();
        if labels.is_empty() {
            continue;
        }
        out.push(NativeFormAnswerView {
            title,
            answer: labels.join("、"),
        });
    }
    out
}

/// 选项值 → 展示 label（命中选项集才替换；自由文本原样保留）。
fn option_label_or_value(field: &FormField, value: &str) -> String {
    let options = match &field.kind {
        FieldKind::Text { options, .. } | FieldKind::Multi { options, .. } => options,
        FieldKind::Unsupported => return value.to_string(),
    };
    options
        .iter()
        .find(|option| option.value == value)
        .map(|option| option.label.clone())
        .unwrap_or_else(|| value.to_string())
}

/// 官方 `metadata.tool.{messageID,id}`：只有两个字段都是非空字符串才成立。
///
/// 生产提问路径写入的就是这对值（`questions` 工具创建表单时带 `kind:"question"` +
/// `tool:{messageID,id}`）；MCP 表单等其它形态可能完全没有或只有一部分——此时返回
/// `None`，调用方**不得**用「最近一条提问」之类的启发式补位。
fn form_tool_link(detail: &Value) -> Option<(&str, &str)> {
    let tool = detail.pointer("/data/metadata/tool")?;
    let message_id = tool.get("messageID").and_then(Value::as_str)?.trim();
    let call_id = tool.get("id").and_then(Value::as_str)?.trim();
    if message_id.is_empty() || call_id.is_empty() {
        return None;
    }
    Some((message_id, call_id))
}

/// 单个官方答案值：字符串 → 单值；全字符串数组 → 多选；其它 JSON 原样归 unknown。
fn form_answer_value(value: &Value) -> FormAnswerValue {
    match value {
        Value::String(text) => FormAnswerValue::Text(text.clone()),
        Value::Array(items) if items.iter().all(Value::is_string) => FormAnswerValue::Multi(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
        ),
        other => FormAnswerValue::Unknown(other.clone()),
    }
}

/// 某个只读操作是否被契约放行（缺键 = 未判定 = 不放行）。
fn capability_allowed(capabilities: &Capabilities, operation: NativeOperationId) -> bool {
    capabilities
        .get(&operation)
        .is_some_and(|capability| capability.allowed)
}

/// 本端实际放行的决定词：只来自契约放行的 once/reject/always。
fn allowed_decisions(capabilities: &Capabilities) -> Vec<String> {
    let mut decisions = Vec::new();
    if capabilities
        .get(&NativeOperationId::PermissionOnce)
        .is_some_and(|capability| capability.allowed)
    {
        decisions.push("once".to_string());
    }
    if capabilities
        .get(&NativeOperationId::PermissionReject)
        .is_some_and(|capability| capability.allowed)
    {
        decisions.push("reject".to_string());
    }
    if capabilities
        .get(&NativeOperationId::PermissionAlways)
        .is_some_and(|capability| capability.allowed)
    {
        decisions.push("always".to_string());
    }
    decisions
}

/// 会话级/全局 pending 列表项 → 可展示字段。
///
/// **列表项不携带权威 state**：任务 0 实测（`followup/fixtures/g4.scope.json`）会话级与全局
/// 列表都只有 `id / sessionID / title / fields`，只有 detail 端点才回 `state.status`。
/// 因此 `PendingView::Form.state` 恒为 `"unknown"`，含义是「本次读没有带回状态」——
/// 既不是 `pending` 也不是 `answered`：手机端**不得**把它当成可答，
/// 也不得据此判断「已被另一端答完」。写入前的权威状态只能由任务 7 在派发时读 detail
/// （`state.status`）并用 `forms::same_form_context_for`（带**当前** binding token
/// 重算）固化后再决定是否写出；
/// 本层刻意不做 N+1 的 detail 预读（只读视图不做扇出，也不拿一个自己无权担保的状态）。
fn forms_from(value: &Value, session_id: &str) -> Result<Vec<PendingForm>, NativeError> {
    let items = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| contract_missing("native pending forms have no data array"))?;
    let mut forms = Vec::with_capacity(items.len());
    for item in items
        .iter()
        .filter(|item| ownership_of(item, session_id) == Ownership::Target)
    {
        let spec = parse_form(item)?;
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(&spec.id)
            .to_string();
        forms.push(PendingForm {
            id: spec.id,
            title,
            fields: spec.fields,
            content_hash: spec.content_hash,
        });
    }
    Ok(forms)
}

/// 列表项归属：目标会话 / 已知其他会话 / 归属不可证。
///
/// 会话级与全局回退读共用本判定：全局集合里混着其他会话的条目是常态。三者语义不同：
/// * [`Ownership::Target`] 可展示、可派发；
/// * [`Ownership::OtherSession`] 过滤、不串写，但**可证不是目标会话**——不会污染
///   「目标请求不在列表里」这一负面证据；
/// * [`Ownership::Unassignable`]（`sessionID` 缺失或非字符串）归属不可证：不能猜成目标
///   会话，也不能用于「请求已消失」的终态推断（R3）。只留一条 debug 日志（含条目 `id`，
///   不含标题/正文），单条脏数据不得连坐整份视图。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ownership {
    Target,
    OtherSession,
    Unassignable,
}

fn ownership_of(item: &Value, session_id: &str) -> Ownership {
    match item.get("sessionID").and_then(Value::as_str) {
        Some(found) if found == session_id => Ownership::Target,
        Some(_) => Ownership::OtherSession,
        None => {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("<missing id>");
            log::debug!("opencode native pending item {id} has no string sessionID; unowned");
            Ownership::Unassignable
        }
    }
}

struct PendingPermission {
    id: String,
    title: String,
}

/// 待处理审批列表 → 可展示字段 + **负面证据可信度**。
///
/// 可信度聚合规则：列表里只要有任一条目归属不可证（[`Ownership::Unassignable`]），整份
/// 列表就不能用于「目标 requestId 已不在 pending」的否定推断——那条不可证项可能正是
/// 目标请求，只是归属字段坏了。已知其他会话的条目可证不是目标请求，过滤它们**不**降低
/// 可信度；空列表（读成功、无不可证项）是可信的负面证据。
fn permissions_from(
    value: &Value,
    session_id: &str,
) -> Result<(Vec<PendingPermission>, bool), NativeError> {
    let items = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| contract_missing("native pending permissions have no data array"))?;
    let mut permissions = Vec::with_capacity(items.len());
    let mut trusted = true;
    for item in items {
        match ownership_of(item, session_id) {
            Ownership::Target => {}
            Ownership::OtherSession => continue,
            Ownership::Unassignable => {
                trusted = false;
                continue;
            }
        }
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| contract_missing("native permission request has no id"))?;
        let action = item.get("action").and_then(Value::as_str);
        let resources: Vec<&str> = item
            .get("resources")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let title = match (action, resources.is_empty()) {
            (Some(action), false) => format!("{action} {}", resources.join(", ")),
            (Some(action), true) => action.to_string(),
            (None, false) => resources.join(", "),
            (None, true) => id.to_string(),
        };
        permissions.push(PendingPermission {
            id: id.to_string(),
            title,
        });
    }
    Ok((permissions, trusted))
}

/// 全局 pending 读：必须带绑定目录的 deepObject scope，并核对回执的
/// `location.directory`——错目录的空列表不是「无待处理请求」的证据。
async fn scoped_read(
    client: &NativeClient,
    target: &NativeTarget,
    path: &str,
) -> Result<Value, NativeError> {
    let directory = target.directory.to_string_lossy().to_string();
    let value = client
        .read_json(path, &[(SCOPE_QUERY_KEY.to_string(), directory)])
        .await?;
    let echoed = value
        .pointer("/location/directory")
        .and_then(Value::as_str)
        .ok_or_else(scope_mismatch)?;
    // 回执目录归一失败说明它不可能是已绑定目录（后者已 realpath 成功）：按 scope 不符处理。
    let echoed = Path::new(echoed)
        .canonicalize()
        .map_err(|_| scope_mismatch())?;
    if echoed != target.directory {
        return Err(scope_mismatch());
    }
    Ok(value)
}

/// inbox：`payload.text`（原生形态）与顶层 `text` 都接受；未知 delivery 词不猜。
///
/// 派发路径经投影间接消费本函数的结果（`DispatchSnapshot::inbox`）：撤回的 204 不是
/// 证据，**回读**才是。
async fn read_inbox(
    client: &NativeClient,
    target: &NativeTarget,
) -> Result<Vec<InboxView>, NativeError> {
    let value = client
        .read_json(
            &format!("/api/session/{}/inbox", segment(&target.session_id)?),
            &[],
        )
        .await?;
    let items = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| contract_missing("native inbox has no data array"))?;
    let mut inbox = Vec::with_capacity(items.len());
    for item in items {
        // inbox 是**持久待处理工作队列**，真实项带 `type`：`user` = 用户待发消息；
        // `synthetic`（模式提醒等系统注入）、`compaction`（压缩任务）、`move`（迁移）
        // 都是内部工作项，**不得**出现在手机待发列表里——否则用户会看到
        // `<system-reminder> You are in Plan mode …` 这种系统提示词，还以为是自己发的内容。
        // 未知/缺失 type 同样跳过：宁可少列，也不把内部注入当用户消息展示。
        let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
        if kind != "user" {
            continue;
        }
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| contract_missing("native inbox item has no id"))?;
        let text =
            text_of(item).ok_or_else(|| contract_missing("native inbox item has no text"))?;
        let delivery = match item.get("delivery").and_then(Value::as_str) {
            Some("queue") => DeliveryKind::Queue,
            Some("steer") => DeliveryKind::Steer,
            _ => return Err(contract_missing("native inbox delivery word is not known")),
        };
        let state = inbox_state_word(&InboxView {
            id: id.to_string(),
            text: text.to_string(),
            delivery,
            state: String::new(),
        })
        .to_string();
        inbox.push(InboxView {
            id: id.to_string(),
            text: text.to_string(),
            delivery,
            state,
        });
    }
    Ok(inbox)
}

/// inbox 项的状态词（前端文案的唯一分派依据）。
///
/// 用户现场教训（2026-11-22）：`steer` 项已被受理、正在交给当前回合，却被统一写成
/// 「排队中（agent 空闲后处理）」，看起来像消息没发出去。两种投递方式必须给不同的词：
/// `queue` 才是在排队等待，`steer` 是已受理并在当前回合交付。
fn inbox_state_word(item: &InboxView) -> &'static str {
    match item.delivery {
        DeliveryKind::Queue => "waiting",
        DeliveryKind::Steer => "steering",
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{json, Value};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::broadcast;

    use super::*;
    use crate::remote::pairing::DeviceStore;
    use crate::services::opencode_native::types::{codes, DiscoveryInput, PendingView};
    use crate::session::{AgentType, ProcessForm, Session, SessionStatus};
    use std::collections::HashMap;
    use tokio::sync::Notify;

    mod route_race_tests {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/services/opencode_native/projection_route_race_tests.rs"
        ));
    }

    #[test]
    fn desktop_binding_requires_server_and_session_argv() {
        let args = vec![
            "/Users/me/.opencode/bin/opencode".to_string(),
            "--server".to_string(),
            "http://127.0.0.1:49374/".to_string(),
            "--session".to_string(),
            "ses_test".to_string(),
            "/Users/me".to_string(),
        ];
        assert!(desktop_argv_matches(
            &args,
            "ses_test",
            "http://127.0.0.1:49374"
        ));
        assert!(!desktop_argv_matches(
            &args,
            "ses_other",
            "http://127.0.0.1:49374"
        ));
        assert!(!desktop_argv_matches(
            &args,
            "ses_test",
            "http://127.0.0.1:49375"
        ));
        let mut stdio = args.clone();
        stdio.retain(|arg| arg != "--server" && arg != "http://127.0.0.1:49374/");
        assert!(!desktop_argv_matches(
            &stdio,
            "ses_test",
            "http://127.0.0.1:49374"
        ));
        let wrong_exe = vec![
            "/usr/bin/sleep".to_string(),
            "--server".to_string(),
            "http://127.0.0.1:49374".to_string(),
            "--session".to_string(),
            "ses_test".to_string(),
        ];
        assert!(!desktop_argv_matches(
            &wrong_exe,
            "ses_test",
            "http://127.0.0.1:49374"
        ));
    }

    #[test]
    fn permission_rules_are_raw_and_unknown_shape_is_not_inferred() {
        let native = json!({
            "data": {
                "permissions": [
                    { "action": "read", "resource": "*.env", "effect": "ask" },
                    { "action": "edit", "resource": "*", "effect": "deny" }
                ]
            }
        });
        let (rules, known) = permission_rules_from(&native);
        assert!(known);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].action, "read");
        assert_eq!(rules[0].effect, "ask");
        assert_eq!(rules[1].resource, "*");

        let (empty, known) = permission_rules_from(&json!({ "data": {} }));
        assert!(known && empty.is_empty());
        let (bad, known) =
            permission_rules_from(&json!({ "data": { "permissions": [{"action": "read"}] } }));
        assert!(!known);
        assert!(bad.is_empty());
    }

    #[test]
    fn running_service_count_blocks_cross_directory_multi_instance() {
        let cmd = |parts: &[&str]| {
            parts
                .iter()
                .map(|part| (*part).to_string())
                .collect::<Vec<_>>()
        };
        let one = vec![
            (
                "opencode".to_string(),
                cmd(&["opencode", "serve", "--service"]),
            ),
            (
                "opencode".to_string(),
                cmd(&["opencode", "serve", "--stdio", "--port", "0"]),
            ),
        ];
        assert_eq!(running_service_count(one.into_iter()), 1);
        let two_dirs = vec![
            (
                "opencode".to_string(),
                cmd(&["opencode", "serve", "--service"]),
            ),
            (
                "opencode".to_string(),
                cmd(&["opencode", "serve", "--port", "59991"]),
            ),
        ];
        assert_eq!(running_service_count(two_dirs.into_iter()), 2);
    }

    #[test]
    fn service_conflict_counts_only_serve_processes_in_same_directory() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let other = temp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let other = other.canonicalize().unwrap();
        let cmd = |parts: &[&str]| {
            parts
                .iter()
                .map(|part| (*part).to_string())
                .collect::<Vec<_>>()
        };
        let one = vec![
            (
                "opencode".to_string(),
                cmd(&["opencode", "serve", "--service"]),
                Some(project.clone()),
            ),
            (
                "opencode".to_string(),
                cmd(&["opencode", "serve", "--stdio", "--port", "0"]),
                Some(project.clone()),
            ),
        ];
        assert_eq!(conflicting_service_count(one.into_iter(), &project), 1);
        let two = vec![
            (
                "opencode".to_string(),
                cmd(&["opencode", "serve", "--service"]),
                Some(project.clone()),
            ),
            (
                "opencode".to_string(),
                cmd(&["opencode", "serve", "--port", "0"]),
                Some(project.clone()),
            ),
            (
                "opencode".to_string(),
                cmd(&["opencode", "serve", "--service"]),
                Some(other.clone()),
            ),
        ];
        assert_eq!(conflicting_service_count(two.into_iter(), &project), 2);
    }

    /// 单次请求阻塞闸：把一次 inbox 读按在「已开始、未应答」之间，用来制造
    /// 「旧监听的在途投影跨过 pause」的确定性交错（测试专用，不进生产路径）。
    pub(super) struct Hold {
        armed: AtomicBool,
        started: Notify,
        release: Notify,
    }

    impl Hold {
        fn new() -> Self {
            Self {
                armed: AtomicBool::new(true),
                started: Notify::new(),
                release: Notify::new(),
            }
        }

        pub(super) async fn pass(&self) {
            if self.armed.swap(false, Ordering::SeqCst) {
                self.started.notify_one();
                self.release.notified().await;
            }
        }
    }

    #[derive(Default)]
    pub(super) struct PumpRetireHold {
        pub(super) started: Notify,
        pub(super) release: Notify,
        pub(super) finished: Notify,
    }

    /// 认领与发布之间的同步闸，可在持有同步锁时使用。
    pub(super) struct ThreadHold {
        started: Notify,
        released: Mutex<bool>,
        wake: std::sync::Condvar,
    }

    impl ThreadHold {
        fn new() -> Self {
            Self {
                started: Notify::new(),
                released: Mutex::new(false),
                wake: std::sync::Condvar::new(),
            }
        }

        pub(super) fn pass(&self) {
            let released = self.released.lock().unwrap();
            self.started.notify_one();
            let (released, timeout) = self
                .wake
                .wait_timeout_while(released, Duration::from_secs(15), |released| !*released)
                .unwrap();
            assert!(*released && !timeout.timed_out(), "测试同步闸未被放行");
        }

        fn release(&self) {
            *self.released.lock().unwrap() = true;
            self.wake.notify_one();
        }
    }

    /// 测试专用凭据：任何错误文案/日志里出现即失败。
    const PASSWORD: &str = "s3cret-credential-value";
    const SERVICE_ID: &str = "87bd7e45-27bf-490d-bced-58d79c108910";
    const PID: u32 = 60541;
    const STARTED_AT_MS: i64 = 1_793_289_345_000;
    const SESSION_ID: &str = "ses_fixture_0001";
    const SSE_HEAD: &str =
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n";
    /// 等待后台（SSE 建连、重建、释放）完成的窗口：只用来等真实网络事件，不缩短实现预算。
    const WAIT: Duration = Duration::from_secs(5);

    fn process_fact() -> Arc<dyn Fn(u32) -> Option<i64> + Send + Sync> {
        Arc::new(|pid: u32| {
            Some(if pid == PID {
                STARTED_AT_MS
            } else {
                STARTED_AT_MS + 60_000
            })
        })
    }

    /// 一次已解析的请求：断言方法与目标都以线上真实请求为准（不看返回值自证）。
    #[derive(Clone, Debug)]
    struct Request {
        method: String,
        target: String,
        /// 请求体（keep-alive 正确性本就要求读尽；F12 探针回声用例需要标题内容）。
        body: String,
    }

    impl Request {
        fn path(&self) -> &str {
            self.target.split('?').next().unwrap_or("")
        }
    }

    /// T3 真服务纯附件的正式记录与未知状态，均走生产读取函数。
    #[test]
    fn product_real_attachment_record_and_unknown_runtime_are_not_guessed() {
        let raw: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/opencode-native/product-messages.json"
        ))
        .unwrap();
        let mut records = raw["data"].as_array().unwrap().clone();
        // API 默认倒序；生产状态函数消费时间正序。
        records.reverse();
        assert_eq!(runtime_state(&records), "idle:failed");
        let user = records.iter().find(|v| v["type"] == "user").unwrap();
        assert_eq!(text_of(user), Some(""));
        assert_eq!(user["files"][0]["mime"], "text/plain");
        assert_eq!(
            runtime_state(&[serde_json::json!({"type":"future-state"})]),
            "unknown"
        );
        assert_eq!(
            runtime_state(&[serde_json::json!({"type":"idle","outcome":"future-outcome"})]),
            "idle:future-outcome"
        );
    }

    /// 服务端计数：写入次数用来证明只读，事件连接数用来证明「每代际最多一个上游」。
    #[derive(Default)]
    struct Counters {
        total: AtomicUsize,
        non_get: AtomicUsize,
        event_conns: AtomicUsize,
        event_closed: AtomicUsize,
    }

    /// 消息分页的游标回执形态（两种真实形态都要支持）。
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum CursorShape {
        /// Windows 2.2.2 实测：`"cursor": "<token>"` 标量。
        Scalar,
        /// Mac 2.0.22 证据：`"cursor": { "previous": ..., "next": ... }`。
        DeepObject,
    }

    /// 假 peer 的可变剧本：每个用例按相位改这里的字段。
    struct Script {
        session_id: String,
        directory: String,
        global_directory: String,
        pid: u32,
        openapi: String,
        /// 消息分页：下标即 cursor 序号（0 = 首页）。
        pages: Vec<Vec<Value>>,
        /// 游标回执形态（缺省 deepObject：既有夹具都是这个形状）。
        cursor_shape: CursorShape,
        /// 服务端**忽略** cursor、永远回默认页（「无进展」陷阱的线上形态）。
        cursor_stuck: bool,
        /// 指定下标页的 `next` 游标覆盖（造「重复 token」剧本）。
        cursor_override: HashMap<usize, Value>,
        /// 该下标页返回 500（用于「部分历史」）。
        page_failure: Option<usize>,
        /// 该下标页返回 401（凭据代际消失，必须上抛）。
        page_unauthorized: Option<usize>,
        /// 模拟「单页体超限」：请求 `limit` ≥ 该值时，响应 JSON 追加
        /// `oversize_padding_bytes` 字节填充（只在 cursor 下标 ≥ `oversize_from_cursor`
        /// 的页生效）。用于在小夹具上触发客户端 `OC_RESPONSE_TOO_LARGE`。
        oversize_trigger_limit: Option<usize>,
        /// 超限剧本从哪个 cursor 下标开始生效（缺省 0 = 首页也超限）。
        oversize_from_cursor: usize,
        /// 超限剧本追加的填充字节数（测试预算比它小即可触发）。
        oversize_padding_bytes: usize,
        /// `/api/info` 的状态码：非 200 用来制造一次真实的 401 作废。
        info_status: u16,
        event_status: u16,
        /// 会话级 form 列表的状态码：200 = 用 `forms`，其它 = 触发全局回退。
        session_forms_status: u16,
        forms: Vec<Value>,
        /// 全局 `/api/form` 的回执；None = 500。
        global_forms: Option<Value>,
        /// 会话级 permission 列表的状态码：200 = 用 `permissions`，其它 = 触发全局回退。
        session_permissions_status: u16,
        permissions: Vec<Value>,
        /// 全局 `/api/permission/request` 的 data（回执目录恒用 `global_directory`）。
        global_permissions: Vec<Value>,
        inbox: Vec<Value>,
        /// 测试闸：当下一次 inbox 读到达时先挂起，直到测试显式放行（其余阶段照常）。
        hold_inbox: Option<Arc<Hold>>,
        hold_openapi: Option<Arc<Hold>>,
        hold_detail: Option<Arc<Hold>>,
        form_metadata: Option<Value>,
        session_model: Option<Value>,
        hold_agent: Option<Arc<Hold>>,
        /// 第 N 次（1-based）`/api/info` GET 到达时挂起一次（F12 回声窗口用例：
        /// 在「最后一次通道终验」与提交之间翻转新鲜观察）。
        hold_info: Option<(Arc<Hold>, usize)>,
        info_gets: usize,
        /// 写门探针会话（F12）：`POST /api/session`（title=probe:tuvis）创建；
        /// GET 回标题；PATCH 覆盖写标题，`auto_ack` 时把 challenge 翻转为合法 ack。
        probe: Option<ProbeSlot>,
    }

    /// 探针会话槽位（F12 测试面）。
    #[derive(Clone)]
    struct ProbeSlot {
        id: String,
        title: String,
        /// 自动 ack 的锁定 TUI pid（challenge → ack 的翻转依据）。
        ack_pid: u32,
        auto_ack: bool,
    }

    impl Script {
        fn new(directory: &Path) -> Self {
            let directory = directory.to_string_lossy().to_string();
            Self {
                session_id: SESSION_ID.to_string(),
                global_directory: directory.clone(),
                directory,
                pid: PID,
                openapi: std::fs::read_to_string(format!(
                    "{}/tests/fixtures/opencode-native/openapi.contract.json",
                    env!("CARGO_MANIFEST_DIR")
                ))
                .expect("读取 openapi 夹具"),
                pages: vec![Vec::new()],
                cursor_shape: CursorShape::DeepObject,
                cursor_stuck: false,
                cursor_override: HashMap::new(),
                page_failure: None,
                page_unauthorized: None,
                oversize_trigger_limit: None,
                oversize_from_cursor: 0,
                oversize_padding_bytes: 0,
                info_status: 200,
                event_status: 200,
                session_forms_status: 200,
                forms: Vec::new(),
                global_forms: None,
                session_permissions_status: 200,
                permissions: Vec::new(),
                global_permissions: Vec::new(),
                inbox: Vec::new(),
                hold_inbox: None,
                hold_openapi: None,
                hold_detail: None,
                form_metadata: None,
                session_model: None,
                hold_agent: None,
                hold_info: None,
                info_gets: 0,
                probe: None,
            }
        }
    }

    struct Shared {
        script: Mutex<Script>,
        counters: Counters,
        log: Mutex<Vec<Request>>,
        events: broadcast::Sender<()>,
        endpoint: String,
    }

    /// 手写回环假 peer：手写 HTTP/1.1 应答，支持 keep-alive 与一条长连 SSE。
    struct Peer {
        shared: Arc<Shared>,
        port: u16,
    }

    impl Peer {
        async fn start(script: Script) -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind peer");
            let port = listener.local_addr().expect("local addr").port();
            let (events, _) = broadcast::channel(16);
            let shared = Arc::new(Shared {
                script: Mutex::new(script),
                counters: Counters::default(),
                log: Mutex::new(Vec::new()),
                events,
                endpoint: format!("http://127.0.0.1:{port}"),
            });
            let accepted = Arc::clone(&shared);
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        break;
                    };
                    let shared = Arc::clone(&accepted);
                    tokio::spawn(async move { serve_connection(&mut stream, shared).await });
                }
            });
            Self { shared, port }
        }

        fn set(&self, edit: impl FnOnce(&mut Script)) {
            edit(&mut self.shared.script.lock().unwrap());
        }

        fn log(&self) -> Vec<Request> {
            self.shared.log.lock().unwrap().clone()
        }

        fn total(&self) -> usize {
            self.shared.counters.total.load(Ordering::SeqCst)
        }

        fn non_get(&self) -> usize {
            self.shared.counters.non_get.load(Ordering::SeqCst)
        }

        fn event_conns(&self) -> usize {
            self.shared.counters.event_conns.load(Ordering::SeqCst)
        }

        fn event_closed(&self) -> usize {
            self.shared.counters.event_closed.load(Ordering::SeqCst)
        }

        /// 向已建立的事件流推一帧（触发一次只读重建）。
        fn push_event(&self) {
            let _ = self.shared.events.send(());
        }

        /// F12：武装「从现在起第 `delta` 次 `/api/info` GET」挂起闸（一次性）。
        fn arm_info_hold_after(&self, delta: usize) -> Arc<Hold> {
            let hold = Arc::new(Hold::new());
            let mut script = self.shared.script.lock().unwrap();
            script.hold_info = Some((Arc::clone(&hold), script.info_gets + delta));
            hold
        }

        /// F12：开探针面（创建/读/写端点可用），`ack_pid` = 自动 ack 的锁定 TUI pid。
        fn arm_probe(&self, ack_pid: u32) {
            self.shared.script.lock().unwrap().probe = Some(ProbeSlot {
                id: "ses_probe_tuvis".to_string(),
                title: "probe:tuvis".to_string(),
                ack_pid,
                auto_ack: true,
            });
        }
    }

    async fn serve_connection(stream: &mut TcpStream, shared: Arc<Shared>) {
        loop {
            let Some(request) = read_request(stream).await else {
                return;
            };
            shared.counters.total.fetch_add(1, Ordering::SeqCst);
            if request.method != "GET" {
                shared.counters.non_get.fetch_add(1, Ordering::SeqCst);
            }
            let path = request.path().to_string();
            let query = request
                .target
                .split_once('?')
                .map(|(_, query)| query.to_string())
                .unwrap_or_default();
            // F12 探针回声面（写门传输的三条端点）：POST /api/session 建探针；
            // GET 探针回标题；PATCH 覆盖写标题（auto_ack 时 challenge → 合法 ack）。
            // 在日志登记之前分诊（request 随后被 move 进日志）。
            let probe_hit = probe_reply(&shared, &request);
            shared.log.lock().unwrap().push(request);
            if let Some(reply) = probe_hit {
                if write_json(stream, reply.0, &reply.1).await.is_err() {
                    return;
                }
                continue;
            }
            if path == "/api/event" {
                shared.counters.event_conns.fetch_add(1, Ordering::SeqCst);
                let status = shared.script.lock().unwrap().event_status;
                if status != 200 {
                    let _ = write_json(stream, status, "{}").await;
                    return;
                }
                serve_events(stream, &shared.events).await;
                shared.counters.event_closed.fetch_add(1, Ordering::SeqCst);
                return;
            }
            // 测试闸只拦「最后一次读」（inbox）：旧 view 读完消息后才被按在这里，
            // 放行后它会直接走到 commit——正是代际检查必须拦住的窗口。
            let hold = {
                let script = shared.script.lock().unwrap();
                if path == format!("/api/session/{}/inbox", script.session_id) {
                    script.hold_inbox.clone()
                } else if path == OPENAPI_PATH {
                    script.hold_openapi.clone()
                } else if path == format!("/api/session/{}/form/frm-test", script.session_id) {
                    script.hold_detail.clone()
                } else if path == "/api/agent" {
                    script.hold_agent.clone()
                } else {
                    None
                }
            };
            if let Some(hold) = hold {
                hold.pass().await;
            }
            // /api/info 的第 N 次 GET 挂起一次（先计数再按需取闸，未到序号装回）。
            if path == "/api/info" {
                let hold = {
                    let mut script = shared.script.lock().unwrap();
                    script.info_gets += 1;
                    let armed = script.hold_info.take();
                    match armed {
                        Some((hold, nth)) if script.info_gets == nth => Some(hold),
                        Some((hold, nth)) => {
                            script.hold_info = Some((hold, nth));
                            None
                        }
                        None => None,
                    }
                };
                if let Some(hold) = hold {
                    hold.pass().await;
                }
            }
            let (status, body) = reply(&shared, &path, &query);
            if write_json(stream, status, &body).await.is_err() {
                return;
            }
        }
    }

    /// F12 写门探针回声面：生产门传输打的三条端点的假服务实现。
    ///
    /// * `POST /api/session`（title=`probe:tuvis`）→ 建探针会话，回 `{"data":{"id":…}}`；
    /// * `GET /api/session/{probe}` → `{"data":{"id","title"}}`（既有会话读同形）；
    /// * `PATCH /api/session/{probe}` → 覆盖标题；`auto_ack` 时把 challenge 翻转为
    ///   `ack:{nonce}:{ack_pid}:{acktime}`（模拟插件读到 challenge 后的槽位翻转）。
    ///
    /// 仅当请求命中探针槽位（或建探针）时返回 `Some`，其余请求交回主 `reply`。
    fn probe_reply(shared: &Shared, request: &Request) -> Option<(u16, String)> {
        const PROBE_TITLE: &str = "probe:tuvis";
        let mut script = shared.script.lock().unwrap();
        if request.method == "POST" && request.path() == "/api/session" {
            if !request.body.contains(PROBE_TITLE) {
                return None;
            }
            let slot = script
                .probe
                .get_or_insert_with(|| ProbeSlot {
                    id: "ses_probe_tuvis".to_string(),
                    title: PROBE_TITLE.to_string(),
                    ack_pid: 0,
                    auto_ack: false,
                })
                .clone();
            return Some((200, json!({ "data": { "id": slot.id } }).to_string()));
        }
        let slot_id = script.probe.as_ref().map(|slot| slot.id.clone())?;
        let slot_path = format!("/api/session/{slot_id}");
        if request.path() != slot_path {
            return None;
        }
        let slot = script.probe.as_mut().expect("probe slot 在场");
        match request.method.as_str() {
            "GET" => Some((
                200,
                json!({ "data": { "id": slot.id, "title": slot.title } }).to_string(),
            )),
            "PATCH" => {
                let title = serde_json::from_str::<Value>(&request.body)
                    .ok()
                    .and_then(|body| body.get("title").and_then(Value::as_str).map(str::to_owned))
                    .unwrap_or_default();
                if slot.auto_ack {
                    if let crate::services::opencode_native::binding_gate::ProbeTitle::Challenge {
                        nonce,
                        ..
                    } = crate::services::opencode_native::binding_gate::parse_probe_title(&title)
                    {
                        let acktime = chrono::Utc::now().timestamp_millis();
                        slot.title = format!("ack:{nonce}:{}:{acktime}", slot.ack_pid);
                        return Some((204, String::new()));
                    }
                }
                slot.title = title;
                Some((204, String::new()))
            }
            _ => None,
        }
    }

    fn reply(shared: &Shared, path: &str, query: &str) -> (u16, String) {
        let script = shared.script.lock().unwrap();
        let session = format!("/api/session/{}", script.session_id);
        if path == "/openapi.json" {
            return (200, script.openapi.clone());
        }
        if path == "/api/info" {
            let body = json!({
                "version": "2.0.22",
                "pid": script.pid,
                "urls": [shared.endpoint.clone()],
            });
            return (script.info_status, body.to_string());
        }
        if path == "/api/form" {
            return match &script.global_forms {
                Some(body) => (200, body.to_string()),
                None => (500, "{}".to_string()),
            };
        }
        if path == "/api/permission/request" {
            let body = json!({
                "location": { "directory": script.global_directory },
                "data": script.global_permissions,
            });
            return (200, body.to_string());
        }
        if path == session {
            let body =
                json!({ "id": script.session_id, "location": { "directory": script.directory } });
            let body = if let Some(model) = &script.session_model {
                let mut data = body;
                data["model"] = model.clone();
                json!({"data": data})
            } else {
                body
            };
            return (200, body.to_string());
        }
        if path == "/api/agent" {
            return (
                200,
                json!({"data":[{"id":"agent-a","name":"A","mode":"primary"}]}).to_string(),
            );
        }
        if path == format!("{session}/form/frm-test") {
            let mut body = json!({"data":{"id":"frm-test","sessionID":script.session_id,"title":"Questions","fields":[{"key":"q0","title":"Color","type":"string","required":true,"options":[{"value":"blue","label":"blue"}],"custom":true}],"state":{"status":"answered","answer":{"q0":"blue"}}}});
            if let Some(metadata) = &script.form_metadata {
                body["data"]["metadata"] = metadata.clone();
            }
            return (200, body.to_string());
        }
        if path == format!("{session}/message") {
            return message_page(&script, query);
        }
        if path == format!("{session}/form") {
            if script.session_forms_status == 200 {
                return (200, json!({ "data": script.forms }).to_string());
            }
            return (script.session_forms_status, "{}".to_string());
        }
        if path == format!("{session}/permission") {
            if script.session_permissions_status == 200 {
                return (200, json!({ "data": script.permissions }).to_string());
            }
            return (script.session_permissions_status, "{}".to_string());
        }
        if path == format!("{session}/inbox") {
            return (200, json!({ "data": script.inbox }).to_string());
        }
        (404, "{}".to_string())
    }

    fn message_page(script: &Script, query: &str) -> (u16, String) {
        // 线上陷阱：服务端静默忽略游标（尤其 deepObject）时总是回默认页，永不前进。
        let index = if script.cursor_stuck {
            0
        } else {
            cursor_index(query)
        };
        if script.page_unauthorized == Some(index) {
            return (401, "{}".to_string());
        }
        if script.page_failure == Some(index) {
            return (500, "{}".to_string());
        }
        let Some(page) = script.pages.get(index) else {
            return (500, "{}".to_string());
        };
        let next = match script.cursor_override.get(&index) {
            Some(value) => value.clone(),
            None if index + 1 < script.pages.len() => Value::String(format!("c{}", index + 1)),
            None => Value::Null,
        };
        let mut body = match script.cursor_shape {
            CursorShape::Scalar => json!({ "data": page, "cursor": next }),
            CursorShape::DeepObject => json!({
                "data": page,
                "cursor": { "previous": Value::Null, "next": next },
            }),
        };
        // 超限剧本：只靠填充把响应体撑过测试预算，不伪造畸形 JSON。
        let padding = index >= script.oversize_from_cursor
            && script
                .oversize_trigger_limit
                .is_some_and(|trigger| query_limit(query).is_some_and(|limit| limit >= trigger));
        if padding {
            body["padding"] = Value::String("p".repeat(script.oversize_padding_bytes));
        }
        (200, body.to_string())
    }

    fn query_limit(query: &str) -> Option<usize> {
        for pair in query.split('&') {
            if let Some(value) = pair.strip_prefix("limit=") {
                return value.parse().ok();
            }
        }
        None
    }

    fn cursor_index(query: &str) -> usize {
        for pair in query.split('&') {
            if let Some(value) = pair.strip_prefix("cursor=") {
                return value.trim_start_matches('c').parse().unwrap_or(0);
            }
        }
        0
    }

    async fn serve_events(stream: &mut TcpStream, events: &broadcast::Sender<()>) {
        let mut pushed = events.subscribe();
        let (mut reader, mut writer) = stream.split();
        if writer.write_all(SSE_HEAD.as_bytes()).await.is_err() {
            return;
        }
        let _ = writer.flush().await;
        let mut probe = [0u8; 64];
        loop {
            tokio::select! {
                frame = pushed.recv() => {
                    if frame.is_err() {
                        return;
                    }
                    if writer
                        .write_all(b"data: {\"type\":\"session.updated\"}\n\n")
                        .await
                        .is_err()
                    {
                        return;
                    }
                    if writer.flush().await.is_err() {
                        return;
                    }
                }
                // 客户端关闭（读 0）即本次事件连接结束。
                probe_result = reader.read(&mut probe) => {
                    let _ = probe_result;
                    return;
                }
            }
        }
    }

    async fn write_raw(stream: &mut TcpStream, raw: &str) -> std::io::Result<()> {
        stream.write_all(raw.as_bytes()).await?;
        stream.flush().await
    }

    async fn write_json(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
        let raw = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        write_raw(stream, &raw).await
    }

    fn head_end(buffer: &[u8]) -> Option<usize> {
        buffer.windows(4).position(|window| window == b"\r\n\r\n")
    }

    async fn read_request(stream: &mut TcpStream) -> Option<Request> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            if let Some(end) = head_end(&buffer) {
                let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
                let mut lines = head.split("\r\n");
                let mut request_line = lines.next()?.split_whitespace();
                let method = request_line.next()?.to_string();
                let target = request_line.next()?.to_string();
                let mut declared = 0usize;
                for line in lines {
                    let (name, value) = line.split_once(':')?;
                    if name.trim().eq_ignore_ascii_case("content-length") {
                        declared = value.trim().parse().ok()?;
                    }
                }
                let mut body = buffer[end + 4..].len();
                while body < declared {
                    let read = stream.read(&mut chunk).await.ok()?;
                    if read == 0 {
                        break;
                    }
                    body += read;
                }
                let body =
                    String::from_utf8_lossy(&buffer[end + 4..end + 4 + declared]).into_owned();
                return Some(Request {
                    method,
                    target,
                    body,
                });
            }
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
    }

    /// 一个隔离夹具：临时目录里的项目目录 + 临时 state_file（绝不碰真实 HOME）。
    struct Fixture {
        _tmp: tempfile::TempDir,
        project: PathBuf,
        home: PathBuf,
        state_file: PathBuf,
        card: Session,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let state_file = tmp.path().join("state/opencode/service.json");
        let card = Session {
            id: SESSION_ID.to_string(),
            agent_type: AgentType::OpenCode,
            project_name: "project".to_string(),
            project_path: project.to_string_lossy().into_owned(),
            title: Some("fixture".to_string()),
            git_branch: None,
            github_url: None,
            status: SessionStatus::Idle,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: "2026-10-30T00:00:00Z".to_string(),
            pid: 12345,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::Cli,
            jump_supported: true,
            unread: false,
            unread_turned_green_at_ms: None,
        };
        Fixture {
            _tmp: tmp,
            project,
            home,
            state_file,
            card,
        }
    }

    fn write_registration(state_file: &Path, port: u16, pid: u32) {
        if let Some(parent) = state_file.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let raw = format!(
            r#"{{"id":"{SERVICE_ID}","version":"2.0.22","url":"http://127.0.0.1:{port}","pid":{pid},"password":"{PASSWORD}"}}"#
        );
        std::fs::write(state_file, raw).unwrap();
    }

    fn hub(fixture: &Fixture, port: u16) -> NativeHub {
        write_registration(&fixture.state_file, port, PID);
        NativeHub::with_coordinator_sources(
            DeviceStore::memory(),
            DiscoveryInput {
                state_file: fixture.state_file.clone(),
                home: fixture.home.clone(),
            },
            process_fact(),
        )
    }

    /// Fix-round-3 负例：argv 回环端点返回 302 指向「外部目标」替身 ⇒ 身份读
    /// 必须禁随重定向：受控 HTTP 状态错误 + 对替身零外联（Basic 头不可外带）。
    #[test]
    fn explicit_identity_read_forbids_redirects_to_outer_targets() {
        use std::io::{Read, Write};
        // 「外部目标」替身：仍是回环端口（测试不许出网），只作外联计数器。
        let outer = std::net::TcpListener::bind("127.0.0.1:0").expect("bind outer");
        let outer_port = outer.local_addr().expect("outer addr").port();
        outer.set_nonblocking(true).expect("outer nonblocking");
        // argv 端点：一次请求后回 302 → Location 指向替身。
        let inner = std::net::TcpListener::bind("127.0.0.1:0").expect("bind inner");
        let inner_port = inner.local_addr().expect("inner addr").port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = inner.accept().expect("inner accept");
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let redirect = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{outer_port}/api/info\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(redirect.as_bytes());
        });
        let endpoint =
            reqwest::Url::parse(&format!("http://127.0.0.1:{inner_port}")).expect("endpoint");
        let error = production_explicit_info_fetch(endpoint, PASSWORD.to_string())
            .expect_err("重定向必须被拒绝");
        server.join().expect("inner server");
        assert_eq!(
            error.code,
            codes::OC_HTTP_STATUS,
            "3xx 必须走受控 HTTP 状态错误，不得跟随重定向"
        );
        // 「外部目标」零外联：无任何挂起连接。
        assert!(outer.accept().is_err(), "重定向目标必须收到零请求");
    }

    /// Fix-round-3 负例：身份响应声明长度超预算 ⇒ 不读体、立即 `OC_RESPONSE_TOO_LARGE`
    /// （与 `NativeClient` 同一响应预算语义；5 秒超时管不住接收/分配/解析量）。
    #[test]
    fn explicit_identity_read_bounds_response_size() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let declared = crate::services::opencode_native::client::MAX_RESPONSE_BYTES as u64 + 1;
            let oversized = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(oversized.as_bytes());
            // 不发送体：预算检查必须在声明长度处先失败。
        });
        let endpoint = reqwest::Url::parse(&format!("http://127.0.0.1:{port}")).expect("endpoint");
        let error = production_explicit_info_fetch(endpoint, PASSWORD.to_string())
            .expect_err("超限响应必须被拒");
        server.join().expect("server");
        assert_eq!(error.code, codes::OC_RESPONSE_TOO_LARGE);
    }

    /// New-Important 焦点测试：显式启用 environ 刷新后，生产读取必须能取到本进程
    /// 环境。sysinfo 默认刷新集合不含 environ（macOS 仅 `with_environ` 才填充）——
    /// 刷新配置漏写时本测试必红（真实 sysinfo 刷新路径，非注入桩）。
    #[test]
    fn production_process_env_reads_live_process_environ() {
        let value = production_process_env(std::process::id(), "PATH")
            .expect("本进程 PATH 必须经显式 environ 刷新可读");
        assert!(!value.is_empty());
        assert!(
            production_process_env(std::process::id(), "TUVIS_DEFINITELY_NOT_SET").is_none(),
            "不存在的键必须返回 None（fail-closed 语义不变）"
        );
    }

    /// 默认 cargo test 也执行生产服务身份门；不借底层协调器构造伪造证明。
    /// R2 语义：构造本身零放行依据——放行只发生在操作时证据链核验通过；登记缺失
    /// 即「服务不可用」，读/写/核对路径统一给出受控拒绝，绝不猜端口接私有 TUI。
    #[tokio::test]
    async fn b1_unregistered_constructor_refuses_with_chain_error() {
        let fixture = fixture();
        let hub = NativeHub::with_sources(
            DeviceStore::memory(),
            DiscoveryInput {
                state_file: fixture.state_file.clone(),
                home: fixture.home.clone(),
            },
            Arc::new(|_| panic!("登记缺失时不得靠服务进程猜测放行")),
            Some(Arc::new(|_| false)),
            None,
        );
        assert_eq!(
            hub.view(&fixture.card).await.unwrap_err().code,
            codes::OC_SERVICE_NOT_REGISTERED
        );
        assert_eq!(
            hub.verify_target_identity(&crate::services::opencode_native::NativeTarget {
                runtime: RuntimeKey {
                    service_id: "svc-x".into(),
                    pid: 1,
                    started_at_ms: 1,
                },
                session_id: fixture.card.id.clone(),
                directory: fixture.project.clone(),
            })
            .await
            .unwrap_err()
            .code,
            codes::OC_SERVICE_NOT_REGISTERED
        );
        assert!(!fixture.state_file.exists());
    }

    fn user_message(id: &str, text: &str, created: i64) -> Value {
        json!({ "id": id, "type": "user", "time": { "created": created }, "text": text })
    }

    /// 任务 0 实测的 pending 列表形态（`g4.scope.json`）：只有 id/sessionID/title/fields，
    /// **没有** `state`。任何给列表项塞 `state` 的夹具都是自造形状。
    fn pending_form(fields: Value) -> Value {
        json!({
            "id": "frm_fixture",
            "sessionID": SESSION_ID,
            "title": "Questions",
            "fields": fields,
        })
    }

    /// 待处理表单列表项：`session: None` = 真实服务端可能缺失的 `sessionID`。
    fn form_item(id: &str, session: Option<&str>, fields: Value) -> Value {
        let mut item = json!({ "id": id, "title": "Questions", "fields": fields });
        if let Some(session) = session {
            item["sessionID"] = json!(session);
        }
        item
    }

    /// 待处理审批列表项：`session: None` = 缺失 `sessionID`（必须丢弃，不得猜归属）。
    fn permission_item(id: &str, session: Option<&str>) -> Value {
        let mut item = json!({ "id": id, "action": "read", "resources": [".env"] });
        if let Some(session) = session {
            item["sessionID"] = json!(session);
        }
        item
    }

    fn message_requests(peer: &Peer) -> Vec<Request> {
        peer.log()
            .into_iter()
            .filter(|request| request.path() == format!("/api/session/{SESSION_ID}/message"))
            .collect()
    }

    fn query_param(target: &str, key: &str) -> Option<String> {
        let query = target.split_once('?').map(|(_, query)| query)?;
        query.split('&').find_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            (name == key).then(|| value.to_string())
        })
    }

    /// 直达分页读的测试装配：不经过 hub，方便注入更小的响应预算做降档 seam。
    fn native_target(project: &Path) -> NativeTarget {
        NativeTarget {
            runtime: RuntimeKey {
                service_id: SERVICE_ID.to_string(),
                pid: PID,
                started_at_ms: STARTED_AT_MS,
            },
            session_id: SESSION_ID.to_string(),
            directory: project.canonicalize().expect("归一项目目录"),
        }
    }

    fn native_client(port: u16) -> NativeClient {
        NativeClient::new(ServiceDescriptor {
            runtime: RuntimeKey {
                service_id: SERVICE_ID.to_string(),
                pid: PID,
                started_at_ms: STARTED_AT_MS,
            },
            endpoint: reqwest::Url::parse(&format!("http://127.0.0.1:{port}")).expect("loopback"),
            password: PASSWORD.to_string(),
            version: "2.0.22".to_string(),
            explicit: false,
        })
        .expect("回环描述符")
    }

    /// 把 delivery 枚举改成三种词：任务 1 的契约判定会据此关掉 `ReadInbox`。
    fn openapi_with_foreign_delivery_word() -> String {
        let mut doc: Value = serde_json::from_str(
            &std::fs::read_to_string(format!(
                "{}/tests/fixtures/opencode-native/openapi.contract.json",
                env!("CARGO_MANIFEST_DIR")
            ))
            .expect("读取 openapi 夹具"),
        )
        .expect("解析 openapi 夹具");
        doc["components"]["schemas"]["Session.Inbox.Delivery"]["enum"] =
            json!(["queue", "steer", "broadcast"]);
        doc.to_string()
    }

    fn form_token(view: &crate::services::opencode_native::types::PublicNativeView) -> String {
        match &view.pending[0] {
            PendingView::Form { context_token, .. } => context_token.clone(),
            other => panic!("期望表单分支，实际 {other:?}"),
        }
    }

    async fn wait_for(mut check: impl FnMut() -> bool, what: &str) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if check() {
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "等待超时：{what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// R5a：工具终态 output 必须进入生产投影的 tool-result 消息；running 不产出，
    /// 对象 output 以 JSON 文本保留，空/缺失不伪造结果。
    #[test]
    fn native_tool_output_reaches_projected_message() {
        let page = json!({
            "data": [{
                "id": "msg_tool_output",
                "type": "assistant",
                "content": [
                    {"type": "tool", "name": "shell",
                     "state": {"status": "completed", "input": {"cmd": "echo hi"},
                               "output": "hi\n"}},
                    {"type": "tool", "name": "read",
                     "state": {"status": "completed", "input": {},
                               "content": [{"type": "text", "text": "file body"}]}},
                    {"type": "tool", "name": "obj",
                     "state": {"status": "completed", "input": {},
                               "output": {"ok": true}}},
                    {"type": "tool", "name": "running",
                     "state": {"status": "running", "input": {}}}
                ]
            }]
        });
        let messages = project_messages(&page).expect("tool 投影不得失败");
        let calls: Vec<&str> = messages
            .iter()
            .filter(|m| m.kind == "tool-call")
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(calls.len(), 4, "每个 tool 块都保留 tool-call");

        let results: Vec<&str> = messages
            .iter()
            .filter(|m| m.kind == "tool-result")
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(
            results,
            vec![
                "hi\n",
                "[{\"type\":\"text\",\"text\":\"file body\"}]",
                "{\"ok\":true}"
            ]
        );
        assert!(
            !messages
                .iter()
                .any(|m| m.kind == "tool-result" && m.content.contains("running")),
            "running 没有终态结果，不能伪造"
        );
    }

    /// H06 D1 回归：tool-result 的 wire 折叠态必须遵守 content.rs 的 SessionMessage
    /// 契约（collapsed：thinking 与 tool-call 默认折叠）——tool-result 一等消息默认
    /// **展开**（运行态的「过程折叠」由前端叠加层收口，wire 层不得整体折叠）。
    /// tool-call / thinking 两类维持默认折叠，一并锁定防止矫枉过正。
    #[test]
    fn tool_result_wire_collapsed_follows_content_contract() {
        let page = json!({
            "data": [{
                "id": "msg_fold",
                "type": "assistant",
                "content": [
                    {"type": "tool", "name": "shell",
                     "state": {"status": "completed", "input": {"cmd": "echo hi"},
                               "output": "hi\n"}},
                    {"type": "reasoning", "text": "想一想"},
                ]
            }]
        });
        let messages = project_messages(&page).expect("投影不得失败");
        let result = messages
            .iter()
            .find(|m| m.kind == "tool-result")
            .expect("终态工具结果必须挂回消息流");
        assert!(
            !result.collapsed,
            "tool-result 按契约默认展开（折叠只归 thinking/tool-call）"
        );
        assert!(
            messages
                .iter()
                .any(|m| m.kind == "tool-call" && m.collapsed),
            "tool-call 维持默认折叠"
        );
        assert!(
            messages.iter().any(|m| m.kind == "thinking" && m.collapsed),
            "thinking 维持默认折叠"
        );
    }

    /// user 消息存在两种形态：顶层 `text` 与 `payload.text`，两者都必须保留。
    #[test]
    fn top_level_and_payload_user_text_are_preserved() {
        let page = json!({
            "data": [
                { "id": "msg_top", "type": "user", "time": { "created": 11 }, "text": "顶层文本" },
                {
                    "id": "msg_payload",
                    "type": "user",
                    "time": { "created": 12 },
                    "payload": { "text": "负载文本" },
                    "delivery": "queue",
                },
            ],
            "cursor": { "previous": null, "next": null },
        });

        let messages = project_messages(&page).expect("两种 user 形态都必须可投影");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].kind, "user");
        assert_eq!(messages[0].content, "顶层文本");
        assert_eq!(messages[0].ts, Some(11));
        assert_eq!(messages[1].content, "负载文本");
        assert_eq!(messages[1].ts, Some(12));
        assert!(!messages[0].collapsed && !messages[1].collapsed);
        // seq 是数组内稳定序号，最后生成。
        assert_eq!(messages[0].seq, 0);
        assert_eq!(messages[1].seq, 1);

        // 读不了的页形态是受控错误，不是空消息列表。
        let err = project_messages(&json!({ "data": {} })).expect_err("非法页必须失败");
        assert_eq!(err.code, codes::OC_CONTRACT_MISSING);
    }

    /// 实机回归（2026-11-22，任务 A）：`type:"synthetic"` 的系统提醒记录
    /// （切 agent 时注入的 `<system-reminder> You are in Plan mode … </system-reminder>`）
    /// 必须留在**正文区**，作为默认折叠的过程消息（`kind="system-note"`）出现，
    /// 且 XML 包裹标签**不得**原样显示给用户。
    ///
    /// 现场记录形态：`{id, time.created, type:"synthetic",
    /// text:"<system-reminder>\nYou are in Plan mode. …\n</system-reminder>"}`。
    #[test]
    fn system_note_projection_strips_reminder_tags_and_collapses() {
        let page = json!({
            "data": [
                { "id": "msg_user", "type": "user", "time": { "created": 1 }, "text": "hi" },
                {
                    "id": "msg_plan",
                    "type": "synthetic",
                    "time": { "created": 2 },
                    "text": "<system-reminder>\nYou are in Plan mode. Discuss the plan with the user directly.\n</system-reminder>",
                },
                { "id": "msg_switch", "type": "agent-switched", "time": { "created": 3 },
                  "agent": "plan", "previous": "build" },
                { "id": "msg_idle", "type": "idle", "time": { "created": 4 } },
            ],
            "cursor": null,
        });

        let records = page["data"].as_array().expect("夹具 data 是数组");
        let projection = project_records_with_seq(records).expect("synthetic 记录不得让投影失败");
        let notes: Vec<&SessionMessage> = projection
            .messages
            .iter()
            .filter(|m| m.kind == "system-note")
            .collect();
        assert_eq!(
            notes.len(),
            1,
            "synthetic 记录产出一条系统提醒：{:?}",
            notes
        );
        let note = notes[0];
        assert_eq!(
            note.role, "assistant",
            "系统提醒归 assistant 侧（非用户气泡）"
        );
        assert!(note.collapsed, "系统提醒默认折叠");
        assert_eq!(note.ts, Some(2), "保留原生 created 时间");
        assert_eq!(
            note.content, "You are in Plan mode. Discuss the plan with the user directly.",
            "只保留提醒内部正文"
        );
        assert!(
            !note.content.contains("system-reminder"),
            "XML 标签不得原样显示给用户"
        );
        // 既有消息不受影响：user 仍在，agent-switched / idle 不产消息。
        assert_eq!(projection.messages.len(), 2);
        assert_eq!(projection.messages[0].kind, "user");
        // 记录 → 消息的 id 映射照旧（synthetic 也落进 message_ids）。
        assert_eq!(
            projection.message_ids.get(&1).map(String::as_str),
            Some("msg_plan")
        );
    }

    /// `synthetic` 记录没有可读 `text`（缺失/非字符串/标签内为空）→ 不产出空消息；
    /// `payload.text` 形态同样接受（与 user 侧 `text_of` 同一条读法）。
    #[test]
    fn system_note_projection_skips_textless_synthetic_records() {
        let page = json!({
            "data": [
                { "id": "msg_none", "type": "synthetic", "time": { "created": 1 } },
                { "id": "msg_num", "type": "synthetic", "time": { "created": 2 }, "text": 42 },
                { "id": "msg_empty", "type": "synthetic", "time": { "created": 3 },
                  "text": "<system-reminder>   </system-reminder>" },
                { "id": "msg_payload", "type": "synthetic", "time": { "created": 4 },
                  "payload": { "text": "<system-reminder>Payload 形态提醒</system-reminder>" } },
            ],
            "cursor": null,
        });

        let messages = project_messages(&page).expect("无正文的 synthetic 不得让投影失败");
        assert_eq!(
            messages.len(),
            1,
            "只有带正文的那条产出消息：{:?}",
            messages
        );
        assert_eq!(messages[0].kind, "system-note");
        assert_eq!(messages[0].content, "Payload 形态提醒");
        assert!(messages[0].collapsed);
    }

    /// Windows OpenCode 2.2.2 消息读回是**顶层** `{text,files}`、没有 `payload`
    /// （写应答才是 `payload{text,files}`）：带 `files` 的用户消息不得让投影失败，
    /// 正文照常保留（附件本身不要求展示）。
    #[test]
    fn top_level_files_readback_does_not_break_projection() {
        let page = json!({
            "data": [{
                "id": "msg_files",
                "type": "user",
                "time": { "created": 13 },
                "text": "看这个附件",
                "files": [{ "uri": "file:///tmp/note.txt", "name": "note.txt" }],
            }],
            "cursor": null,
        });

        let messages = project_messages(&page).expect("顶层 files 回读不得让投影失败");
        assert_eq!(messages.len(), 1, "带附件的用户消息仍是一条消息");
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "看这个附件");
        assert_eq!(messages[0].ts, Some(13));
    }

    /// R5c：顶层 `files` 与 `payload.files` 都要按消息 seq 关联；名字保真
    /// （中文/空格）、多附件不丢、非 user 记录不产生附件映射。
    #[test]
    fn native_user_files_are_associated_with_their_message_seq() {
        let records = vec![
            json!({
                "id": "msg_top",
                "type": "user",
                "text": "顶层附件",
                "files": [
                    {
                        "name": "中文 space 报告.txt",
                        "mime": "text/plain",
                        "data": "aGVsbG8=",
                    },
                    { "name": "second.bin", "mime": "application/octet-stream" },
                ],
            }),
            json!({
                "id": "msg_payload",
                "type": "user",
                "payload": {
                    "text": "负载附件",
                    "files": [{
                        "name": "payload.txt",
                        "mime": "text/plain",
                        "source": { "type": "uri", "uri": "file:///tmp/payload.txt" },
                    }],
                },
            }),
            json!({
                "id": "msg_assistant",
                "type": "assistant",
                "content": [
                    { "type": "text", "text": "ok" },
                    { "type": "tool", "name": "shell", "state": { "status": "running" } },
                ],
            }),
            json!({ "id": "msg_no_files", "type": "user", "text": "没有附件", "files": [] }),
        ];

        let projection = project_records_with_seq(&records).expect("消息投影");
        assert_eq!(
            projection
                .messages
                .iter()
                .map(|message| message.seq)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4],
            "assistant 两个分块占两条消息"
        );
        let files = project_attachments(&records, &projection, None);
        assert_eq!(
            files.keys().copied().collect::<Vec<_>>(),
            vec![0, 1],
            "只有带附件的 user 消息有键，assistant/空 files 不伪造条目"
        );
        let top = &files[&0];
        assert_eq!(top.len(), 2, "多附件一条不丢");
        assert_eq!(top[0].name, "中文 space 报告.txt");
        assert_eq!(top[0].mime.as_deref(), Some("text/plain"));
        assert_eq!(top[0].size, Some(5), "8 字符 base64（含 1 个 =）≈ 5 字节");
        assert_eq!(top[0].preview_path, None, "没有会话作用域就不给预览路径");
        assert_eq!(top[1].name, "second.bin");
        assert_eq!(top[1].size, None, "没有 data 不编造大小");
        assert_eq!(files[&1][0].name, "payload.txt");
        assert_eq!(files[&1][0].preview_path, None);
    }

    /// R5c：`data` 只换成近似大小；DTO 里不得出现任何 base64 内容。
    #[test]
    fn native_file_data_only_becomes_approximate_size() {
        let records = vec![json!({
            "id": "msg_data",
            "type": "user",
            "text": "base64",
            "files": [
                { "name": "hello.txt", "mime": "text/plain", "data": "aGVsbG8=" },
                { "name": "empty.txt", "data": "" },
                { "name": "bad.txt", "data": 42 },
            ],
        })];
        let projection = project_records_with_seq(&records).expect("消息投影");
        let files = project_attachments(&records, &projection, None);
        let views = &files[&0];
        assert_eq!(views[0].size, Some(5));
        assert_eq!(views[1].size, None, "空 data 不冒充 0 字节");
        assert_eq!(views[2].size, None, "非字符串 data 不猜大小");
        let wire = serde_json::to_value(&views[0]).expect("序列化附件");
        assert!(wire.get("data").is_none(), "base64 绝不下发：{wire}");
        assert!(
            !wire.to_string().contains("aGVsbG8="),
            "内容不得泄漏：{wire}"
        );
    }

    /// R5c：只有 canonical 后仍在 `<cwd>/.tuvis-attachments/<session_id>/` 内的
    /// `file://` 才给 `previewPath`；越界/符号链接逃逸/非 file scheme 一律只给元数据。
    #[test]
    fn native_file_preview_only_for_controlled_file_uri() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().canonicalize().expect("项目 realpath");
        let controlled = project.join(".tuvis-attachments").join(SESSION_ID);
        std::fs::create_dir_all(&controlled).expect("受控目录");
        let inside = controlled.join("中文 space 报告.txt");
        std::fs::write(&inside, "hello attachment").expect("受控文件");
        let outside = project.join("stray.txt");
        std::fs::write(&outside, "outside").expect("越界文件");

        let inside_uri = reqwest::Url::from_file_path(&inside)
            .expect("file url")
            .to_string();
        let outside_uri = reqwest::Url::from_file_path(&outside)
            .expect("file url")
            .to_string();
        let files = vec![
            json!({ "name": "inside.txt", "uri": inside_uri }),
            json!({ "name": "outside.txt", "uri": outside_uri }),
            json!({ "name": "remote.txt", "uri": "https://example.com/remote.txt" }),
            json!({ "name": "data.txt", "uri": "data:text/plain;base64,aGVsbG8=" }),
            json!({ "name": "bare.txt", "uri": "/tmp/bare.txt" }),
        ];
        // Windows 没有 `std::os::unix::fs::symlink` 的同一逃逸样本：只在 unix 追加，
        // 期望条数随之变化（否则非 unix 测试目标会 unused_mut/条数错）。
        #[cfg(unix)]
        let files = {
            let mut files = files;
            let escape = controlled.join("escape.txt");
            std::os::unix::fs::symlink(&outside, &escape).expect("符号链接");
            let escape_uri = reqwest::Url::from_file_path(&escape)
                .expect("file url")
                .to_string();
            files.push(json!({ "name": "escape.txt", "uri": escape_uri }));
            files
        };
        let expected = if cfg!(unix) { 6 } else { 5 };
        let records = vec![json!({
            "id": "msg_files",
            "type": "user",
            "text": "附件",
            "files": files,
        })];
        let projection = project_records_with_seq(&records).expect("消息投影");
        let target = native_target(&project);
        let projected = project_attachments(&records, &projection, Some(&target));

        let views = &projected[&0];
        assert_eq!(views.len(), expected, "每个条目都保留（含只读元数据）");
        assert_eq!(
            views[0].preview_path.as_deref(),
            Some(
                inside
                    .canonicalize()
                    .expect("归一受控文件")
                    .to_string_lossy()
                    .as_ref()
            ),
            "受控目录内文件给 canonical previewPath"
        );
        for view in &views[1..] {
            assert_eq!(
                view.preview_path, None,
                "越界/远程/data/裸路径/符号链接逃逸不给预览：{}",
                view.name
            );
        }
    }

    /// R5c：异常 files 形态只丢该条，不让整份投影失败；缺名给受控占位。
    #[test]
    fn native_malformed_files_do_not_fail_projection() {
        let records = vec![
            json!({ "id": "m1", "type": "user", "text": "坏形状", "files": "not-an-array" }),
            json!({
                "id": "m2",
                "type": "user",
                "text": "混合条目",
                "files": [null, "x", 42, {}, { "name": 42, "data": [] }],
            }),
        ];
        let projection = project_records_with_seq(&records).expect("异常 files 不得让投影失败");
        assert_eq!(projection.messages.len(), 2);
        let files = project_attachments(&records, &projection, None);
        assert_eq!(
            files.keys().copied().collect::<Vec<_>>(),
            vec![1],
            "非数组 files 静默忽略；非对象条目跳过"
        );
        let views = &files[&1];
        assert_eq!(views.len(), 2, "两个对象条目都保留");
        assert_eq!(views[0].name, ATTACHMENT_PLACEHOLDER);
        assert_eq!(views[1].name, ATTACHMENT_PLACEHOLDER);
        assert_eq!(views[1].mime, None);
        assert_eq!(views[1].size, None);
        assert_eq!(views[1].preview_path, None);
    }

    /// assistant 的 content[] 分块映射：text → assistant、reasoning → 折叠 thinking、
    /// tool → 折叠 tool-call；idle 是回合终态记录，不是消息。
    #[test]
    fn assistant_blocks_and_idle_are_projected_by_kind() {
        let page = json!({
            "data": [
                {
                    "id": "msg_asst",
                    "type": "assistant",
                    "time": { "created": 20 },
                    "content": [
                        { "type": "reasoning", "text": "想一想" },
                        { "type": "text", "text": "好的" },
                        { "type": "tool", "name": "shell", "state": { "status": "completed", "input": { "command": "ls" } } },
                    ],
                },
                { "id": "msg_idle", "type": "idle", "time": { "created": 21 }, "outcome": "succeeded" },
            ],
        });

        let messages = project_messages(&page).expect("assistant 分块必须可投影");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].kind, "thinking");
        assert!(messages[0].collapsed, "思考块默认折叠");
        assert_eq!(messages[1].kind, "assistant");
        assert!(!messages[1].collapsed);
        assert_eq!(messages[1].content, "好的");
        assert_eq!(messages[2].kind, "tool-call");
        assert_eq!(messages[2].tool_name.as_deref(), Some("shell"));
        assert_eq!(
            messages[2].tool_args.as_deref(),
            Some(r#"{"command":"ls"}"#),
            "工具参数原样作为 JSON 字符串"
        );
        assert!(messages[2].collapsed, "工具行默认折叠");
        assert!(messages.iter().all(|message| message.role == "assistant"));
        assert!(
            messages.iter().all(|message| message.ts == Some(20)),
            "idle 记录不得混进消息流"
        );
    }

    /// 错目录的空 pending 不是「无待处理请求」：失败读不得清空已知请求，也不得发布新修订。
    #[tokio::test]
    async fn wrong_location_empty_pending_does_not_clear_requests() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.forms = vec![pending_form(
                json!([{ "key": "q0", "title": "COLOR", "type": "string" }]),
            )];
        });
        let hub = hub(&fixture, peer.port);
        let mut rx = hub.watch(SESSION_ID);

        let first = hub.view(&fixture.card).await.expect("首次投影");
        assert_eq!(first.pending.len(), 1);
        let token = form_token(&first);
        let _ = rx.borrow_and_update();

        // 会话级端点不可用 → 全局回退；回执 location 是**另一个真实目录**（不是本会话），
        // 因此这份空列表不是证据（用一个真实目录，避免被「目录归一失败」提前拒绝）。
        let elsewhere = tempfile::tempdir().expect("另一个真实目录");
        let elsewhere = elsewhere.path().to_string_lossy().to_string();
        peer.set(|script| {
            script.session_forms_status = 404;
            script.global_forms = Some(json!({
                "location": { "directory": elsewhere },
                "data": [],
            }));
        });
        let err = hub
            .view(&fixture.card)
            .await
            .expect_err("错目录的空 pending 不得被当成空成功");
        assert_eq!(err.code, codes::OC_DIRECTORY_MISMATCH);
        assert!(
            !rx.has_changed().unwrap(),
            "失败的 pending 读不得发布新修订（也就不得清空已知请求）"
        );

        // 全局读本身失败同样不得吞成空列表。
        peer.set(|script| script.global_forms = None);
        let err = hub
            .view(&fixture.card)
            .await
            .expect_err("全局 pending 读失败不得吞成空");
        assert_eq!(err.code, codes::OC_HTTP_STATUS);
        assert!(!rx.has_changed().unwrap());

        // 恢复会话级端点：已知请求仍在，内容未变则 token 未变。
        peer.set(|script| script.session_forms_status = 200);
        let again = hub.view(&fixture.card).await.expect("恢复后投影");
        assert_eq!(again.pending.len(), 1);
        assert_eq!(
            form_token(&again),
            token,
            "内容未变时 context token 不得轮换"
        );

        // 全局回退必须带绑定目录的 deepObject scope（任务 0 回填第 6 条）。
        let global: Vec<Request> = peer
            .log()
            .into_iter()
            .filter(|request| request.path() == "/api/form")
            .collect();
        assert_eq!(global.len(), 2, "两次回退各一次全局读");
        for request in &global {
            assert!(
                request.target.contains("location%5Bdirectory%5D=%2F"),
                "全局 pending 读必须带绝对目录的 deepObject scope：{}",
                request.target
            );
        }
    }

    /// G2：会话级读失败回退全局读时，只保留本会话的表单（其他会话不得进入 pending）。
    #[tokio::test]
    async fn global_fallback_filters_other_session_forms() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        let fields = json!([{ "key": "q0", "title": "COLOR", "type": "string" }]);
        peer.set(|script| {
            script.session_forms_status = 404;
            script.global_forms = Some(json!({
                "location": { "directory": script.global_directory },
                "data": [
                    form_item("frm_foreign", Some("ses_other"), fields.clone()),
                    form_item("frm_target", Some(SESSION_ID), fields.clone()),
                ],
            }));
        });
        let hub = hub(&fixture, peer.port);

        let view = hub.view(&fixture.card).await.expect("全局回退投影");
        let ids: Vec<&str> = view
            .pending
            .iter()
            .map(|item| match item {
                PendingView::Form { id, .. } => id.as_str(),
                other => panic!("期望表单分支，实际 {other:?}"),
            })
            .collect();
        assert_eq!(
            ids,
            vec!["frm_target"],
            "全局回退只保留 sessionID == 目标会话的表单"
        );
    }

    /// G2：缺失 `sessionID` 的全局表单归属不可证，必须丢弃（且不得让整份视图失败）。
    #[tokio::test]
    async fn global_fallback_drops_missing_session() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        let fields = json!([{ "key": "q0", "title": "COLOR", "type": "string" }]);
        peer.set(|script| {
            script.session_forms_status = 404;
            script.global_forms = Some(json!({
                "location": { "directory": script.global_directory },
                "data": [
                    form_item("frm_orphan", None, fields.clone()),
                    form_item("frm_target", Some(SESSION_ID), fields.clone()),
                ],
            }));
        });
        let hub = hub(&fixture, peer.port);

        let view = hub
            .view(&fixture.card)
            .await
            .expect("缺失 sessionID 只丢该项，不得失败整份视图");
        let ids: Vec<&str> = view
            .pending
            .iter()
            .map(|item| match item {
                PendingView::Form { id, .. } => id.as_str(),
                other => panic!("期望表单分支，实际 {other:?}"),
            })
            .collect();
        assert_eq!(
            ids,
            vec!["frm_target"],
            "缺失 sessionID 的表单不得生成 token 或进入 pending"
        );
    }

    /// G2：审批的全局回退同样按 `sessionID` 过滤（缺失字段丢弃）。
    #[tokio::test]
    async fn permissions_fallback_filters_other_session() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.session_permissions_status = 404;
            script.global_permissions = vec![
                permission_item("per_foreign", Some("ses_other")),
                permission_item("per_orphan", None),
                permission_item("per_target", Some(SESSION_ID)),
            ];
        });
        let hub = hub(&fixture, peer.port);

        let view = hub.view(&fixture.card).await.expect("全局回退投影");
        let ids: Vec<&str> = view
            .pending
            .iter()
            .map(|item| match item {
                PendingView::Permission { id, .. } => id.as_str(),
                other => panic!("期望审批分支，实际 {other:?}"),
            })
            .collect();
        assert_eq!(
            ids,
            vec!["per_target"],
            "全局回退只保留 sessionID == 目标会话的审批"
        );
    }

    /// R3：审批列表的负面证据可信度按三态归属聚合——任一项归属不可证（缺失/非字符串
    /// `sessionID`）⇒ 整份列表不可用于「请求已消失」；已知其他会话可证「不是目标会话」，
    /// 过滤它们不降低可信度。
    #[test]
    fn permissions_trust_tracks_unassignable_ownership() {
        let (kept, trusted) = permissions_from(
            &json!({ "data": [permission_item("per_1", Some(SESSION_ID))] }),
            SESSION_ID,
        )
        .expect("解析目标会话审批");
        assert_eq!(kept.len(), 1);
        assert!(trusted, "全部可证为目标会话");

        let (kept, trusted) = permissions_from(
            &json!({ "data": [permission_item("per_2", Some("ses_other"))] }),
            SESSION_ID,
        )
        .expect("解析其他会话审批");
        assert!(kept.is_empty(), "已知其他会话仍过滤、不串写");
        assert!(trusted, "已知其他会话不污染负面证据");

        let (_, trusted) = permissions_from(
            &json!({ "data": [
                permission_item("per_3", Some(SESSION_ID)),
                permission_item("per_4", None),
            ] }),
            SESSION_ID,
        )
        .expect("解析混入缺失归属的列表");
        assert!(!trusted, "缺失 sessionID 的条目使整份列表不可作负面证据");

        let (kept, trusted) = permissions_from(
            &json!({ "data": [{ "id": "per_5", "sessionID": 42 }] }),
            SESSION_ID,
        )
        .expect("解析非字符串归属");
        assert!(kept.is_empty(), "非字符串 sessionID 同样不生成待处理项");
        assert!(!trusted, "非字符串 sessionID 同样归属不可证");

        let (kept, trusted) =
            permissions_from(&json!({ "data": [] }), SESSION_ID).expect("解析空列表");
        assert!(kept.is_empty());
        assert!(trusted, "读成功且无不可证项的空列表是可信负面证据");
    }

    /// G2：会话级列表同样过滤（正常情况下不该混入其他会话，但脏数据不得进 pending）。
    #[tokio::test]
    async fn session_level_forms_filter_foreign_and_missing_session() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        let fields = json!([{ "key": "q0", "title": "COLOR", "type": "string" }]);
        peer.set(|script| {
            script.session_forms_status = 200;
            script.forms = vec![
                form_item("frm_foreign", Some("ses_other"), fields.clone()),
                form_item("frm_orphan", None, fields.clone()),
                form_item("frm_target", Some(SESSION_ID), fields.clone()),
            ];
        });
        let hub = hub(&fixture, peer.port);

        let view = hub
            .view(&fixture.card)
            .await
            .expect("会话级脏数据只丢该项，不得失败整份视图");
        let ids: Vec<&str> = view
            .pending
            .iter()
            .map(|item| match item {
                PendingView::Form { id, .. } => id.as_str(),
                other => panic!("期望表单分支，实际 {other:?}"),
            })
            .collect();
        assert_eq!(
            ids,
            vec!["frm_target"],
            "会话级列表也必须只保留 sessionID == 目标会话的条目"
        );
    }

    /// 部分历史只能给出「已知消息」，永远不能作为投递证据。
    #[tokio::test]
    async fn partial_history_never_confirms_delivery() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.pages = vec![
                vec![
                    user_message("msg_a", "第一句", 1),
                    user_message("msg_b", "第二句", 2),
                ],
                vec![user_message("msg_c", "第三句", 3)],
            ];
            // 尾页用标量 `cursor:null`（文档形状）；对象里的 `next:null` 属于「存在但
            // 畸形」，读路径会保守判不完整（不是这里要测的部分历史语义）。
            script.cursor_shape = CursorShape::Scalar;
            script.page_failure = Some(1);
        });
        let hub = hub(&fixture, peer.port);

        let partial = hub.view(&fixture.card).await.expect("部分页仍给出已知消息");
        assert_eq!(partial.messages.len(), 2);
        assert!(
            !hub.delivery_evidence(SESSION_ID),
            "只读到部分页时不得声称消息列表缺席某 id 可作为投递证据"
        );

        peer.set(|script| script.page_failure = None);
        let full = hub.view(&fixture.card).await.expect("完整读");
        assert_eq!(full.messages.len(), 3);
        assert!(hub.delivery_evidence(SESSION_ID), "完整读才可作投递证据");
        assert_eq!(
            full.messages
                .iter()
                .map(|message| message.seq)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    /// G6：空页即使还带着 cursor 也是终止信号（不能继续追 token）。
    #[tokio::test]
    async fn empty_page_with_cursor_terminates_complete() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.pages = vec![
                vec![user_message("msg_a", "a", 1)],
                Vec::new(),
                vec![user_message("msg_b", "b", 2)],
            ];
        });
        let hub = hub(&fixture, peer.port);

        let view = hub.view(&fixture.card).await.expect("投影");
        assert_eq!(view.messages.len(), 1, "空页必须立即终止：其后的页不得再读");
        assert_eq!(
            message_requests(&peer).len(),
            2,
            "首页 + 空页 = 两次请求；空页携带的 cursor 不得被继续消费"
        );
        assert!(view.complete, "空页是权威终止：必须标记为完整");
    }

    /// G6：重复 token 立即停止（不得进入「同一页读到预算耗尽」的死循环）。
    #[tokio::test]
    async fn repeated_cursor_stops_incomplete() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.pages = vec![
                vec![user_message("msg_a", "a", 1)],
                vec![user_message("msg_b", "b", 2)],
            ];
            // 第二页回同一个 token c1：重复游标必须立即停止。
            script.cursor_override.insert(1, json!("c1"));
        });
        let hub = hub(&fixture, peer.port);

        let view = hub.view(&fixture.card).await.expect("投影");
        assert_eq!(view.messages.len(), 2, "重复 token 之前的记录都保留");
        assert_eq!(
            message_requests(&peer).len(),
            2,
            "重复 token 必须立即停止，不得反复重读同一页"
        );
        assert!(!view.complete, "重复 token 停止 = 不完整");
        assert!(
            !hub.delivery_evidence(SESSION_ID),
            "重复 token 停止 = 不完整，不得作投递证据"
        );
    }

    /// G6：标量 `cursor`（Windows 2.2.2）必须被消费；首页 order=asc，后续只带 limit+cursor。
    #[tokio::test]
    async fn scalar_cursor_is_consumed() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.cursor_shape = CursorShape::Scalar;
            script.pages = vec![
                vec![user_message("msg_a", "a", 1)],
                vec![user_message("msg_b", "b", 2)],
            ];
        });
        let hub = hub(&fixture, peer.port);

        let view = hub.view(&fixture.card).await.expect("投影");
        assert_eq!(view.messages.len(), 2, "标量 cursor 必须读到第二页");
        let requests = message_requests(&peer);
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0].target.contains("order=asc") && !requests[0].target.contains("cursor="),
            "首页必须 order=asc 且不带 cursor：{}",
            requests[0].target
        );
        assert!(
            requests[1].target.contains("cursor=c1") && !requests[1].target.contains("order="),
            "后续页只带 limit+cursor（不得再带 order）：{}",
            requests[1].target
        );
        assert!(view.complete, "游标耗尽必须标记为完整");
        assert!(hub.delivery_evidence(SESSION_ID), "读完整页才可作投递证据");
    }

    /// G6：服务端忽略 cursor（deepObject 陷阱）时只回默认页 —— 必须靠「无进展」停下。
    #[tokio::test]
    async fn deepobject_cursor_page_repeat_stops() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.pages = vec![
                vec![user_message("msg_a", "a", 1)],
                vec![user_message("msg_b", "b", 2)],
            ];
            // 服务端静默忽略游标：每次请求都回首页（线上陷阱）。
            script.cursor_stuck = true;
        });
        let hub = hub(&fixture, peer.port);

        let view = hub.view(&fixture.card).await.expect("投影");
        assert_eq!(view.messages.len(), 1, "同一页重复读不得重复记录");
        let requests = message_requests(&peer);
        assert_eq!(requests.len(), 2, "无进展必须第二页就停下，不得烧满预算");
        assert!(
            requests[1].target.contains("cursor=c1") && !requests[1].target.contains("cursor%5B"),
            "后续页必须用标量 cursor，而不是被服务端静默忽略的 deepObject：{}",
            requests[1].target
        );
        assert!(!view.complete, "无进展停止 = 不完整");
        assert!(
            !hub.delivery_evidence(SESSION_ID),
            "无进展停止 = 不完整，不得作投递证据"
        );
    }

    /// G6：空字符串 token 不是尾页（两种回执形态归一）：无法证明读完 ⇒ 不完整。
    #[tokio::test]
    async fn empty_string_cursor_stops_incomplete() {
        for shape in [CursorShape::Scalar, CursorShape::DeepObject] {
            let fixture = fixture();
            let project = fixture.project.canonicalize().unwrap();
            let peer = Peer::start(Script::new(&project)).await;
            peer.set(|script| {
                script.cursor_shape = shape;
                script.pages = vec![
                    vec![user_message("msg_a", "a", 1)],
                    vec![user_message("msg_b", "b", 2)],
                ];
                // 首页回空串 token：既不是「没有游标」，也不能当作读完了。
                script.cursor_override.insert(0, json!(""));
            });
            let hub = hub(&fixture, peer.port);

            let view = hub.view(&fixture.card).await.expect("投影");
            assert_eq!(
                message_requests(&peer).len(),
                1,
                "空串 token 必须原地终止，不得发 `cursor=` 再读一遍"
            );
            assert_eq!(view.messages.len(), 1);
            assert!(!view.complete, "空串 token 不能声明完整");
            assert!(
                !hub.delivery_evidence(SESSION_ID),
                "空串 token 终止不得作投递证据"
            );
        }
    }

    /// 构造一页合法记录 + 一个**存在但畸形**的游标回执：读路径必须原地停止、
    /// 保留已读记录、`complete=false`，且不得作投递证据。
    async fn assert_malformed_cursor_stops_incomplete(cursor: Value, shape: CursorShape) {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.cursor_shape = shape;
            script.pages = vec![
                vec![user_message("msg_a", "a", 1)],
                vec![user_message("msg_b", "b", 2)],
            ];
            script.cursor_override.insert(0, cursor);
        });
        let hub = hub(&fixture, peer.port);

        let view = hub.view(&fixture.card).await.expect("投影");
        assert_eq!(view.messages.len(), 1, "畸形游标之前的记录必须保留");
        assert_eq!(
            message_requests(&peer).len(),
            1,
            "畸形游标必须原地终止，不得拿它再发一轮"
        );
        assert!(!view.complete, "畸形游标不能证明读到尾页");
        assert!(
            !hub.delivery_evidence(SESSION_ID),
            "畸形游标终止不得作投递证据"
        );
    }

    /// G6：数字游标是「存在但畸形」⇒ 不完整终止（旧实现会当成尾页）。
    #[tokio::test]
    async fn numeric_cursor_stops_incomplete() {
        assert_malformed_cursor_stops_incomplete(json!(42), CursorShape::Scalar).await;
    }

    /// G6：布尔游标同样是「存在但畸形」⇒ 不完整终止。
    #[tokio::test]
    async fn boolean_cursor_stops_incomplete() {
        assert_malformed_cursor_stops_incomplete(json!(false), CursorShape::Scalar).await;
    }

    /// G6：数组游标不能被当成「没有游标」⇒ 不完整终止。
    #[tokio::test]
    async fn array_cursor_stops_incomplete() {
        assert_malformed_cursor_stops_incomplete(json!(["c1"]), CursorShape::Scalar).await;
    }

    /// G6：`cursor.next` 是数字（非字符串）⇒ 对象存在但畸形 ⇒ 不完整终止。
    #[tokio::test]
    async fn object_cursor_with_numeric_next_stops_incomplete() {
        assert_malformed_cursor_stops_incomplete(json!(42), CursorShape::DeepObject).await;
    }

    /// G6：`cursor.next:null` 也是存在但畸形；文档尾页是 `cursor:null`/缺字段，
    /// 不能把「对象里没有有效 next」当成读完。
    #[tokio::test]
    async fn object_cursor_with_null_next_stops_incomplete() {
        assert_malformed_cursor_stops_incomplete(Value::Null, CursorShape::DeepObject).await;
    }

    /// G6：游标分类矩阵——只有真正的尾页形状才是 `End`；存在但畸形一律 `Empty`。
    #[test]
    fn next_cursor_classifies_tail_and_malformed_shapes() {
        assert!(matches!(next_cursor(&json!({})), PageCursor::End));
        assert!(matches!(
            next_cursor(&json!({ "cursor": null })),
            PageCursor::End
        ));
        assert!(matches!(
            next_cursor(&json!({ "cursor": "c1" })),
            PageCursor::Next("c1")
        ));
        assert!(matches!(
            next_cursor(&json!({ "cursor": { "next": "c2" } })),
            PageCursor::Next("c2")
        ));
        for page in [
            json!({ "cursor": "" }),
            json!({ "cursor": 1 }),
            json!({ "cursor": true }),
            json!({ "cursor": ["c1"] }),
            json!({ "cursor": {} }),
            json!({ "cursor": { "next": null } }),
            json!({ "cursor": { "next": 7 } }),
            json!({ "cursor": { "next": "" } }),
        ] {
            assert!(
                matches!(next_cursor(&page), PageCursor::Empty),
                "存在但畸形的游标必须按 Empty 处理：{page}"
            );
        }
    }

    /// G6：同一页里重复的原生 id 只能保留一条（merge_record 去重）。
    #[tokio::test]
    async fn duplicate_ids_within_one_page_are_merged() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.pages = vec![vec![
                user_message("msg_a", "a", 1),
                user_message("msg_a", "a-latest", 2),
            ]];
            // 单页即尾页：用标量 `cursor:null`（对象 `next:null` 现在是畸形，见 next_cursor）。
            script.cursor_shape = CursorShape::Scalar;
        });
        let hub = hub(&fixture, peer.port);

        let view = hub.view(&fixture.card).await.expect("投影");
        assert_eq!(view.messages.len(), 1, "同页重复 id 只能出现一次");
        assert!(view.complete, "单页且无游标是权威尾页");
        assert!(hub.delivery_evidence(SESSION_ID));
        assert_eq!(message_requests(&peer).len(), 1);
    }

    /// G6：单页没有新增记录 id（全是重复）同样是无进展，必须停下且不完整。
    #[tokio::test]
    async fn no_new_ids_stops_incomplete() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.pages = vec![
                vec![user_message("msg_a", "a", 1)],
                vec![user_message("msg_a", "a", 1)],
            ];
        });
        let hub = hub(&fixture, peer.port);

        let view = hub.view(&fixture.card).await.expect("投影");
        assert_eq!(view.messages.len(), 1, "重复 id 不得重复记录");
        assert_eq!(message_requests(&peer).len(), 2, "无新 id 的那一页即终止");
        assert!(!view.complete, "无新 id 不是「读完了」");
        assert!(
            !hub.delivery_evidence(SESSION_ID),
            "无新 id 不是「读完了」，不得作投递证据"
        );
    }

    /// G6：最多 20 页；预算耗尽停止 = 不完整。
    #[tokio::test]
    async fn page_budget_stops_incomplete() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.pages = (0..MAX_MESSAGE_PAGES + 1)
                .map(|index| {
                    vec![user_message(
                        &format!("msg_{index}"),
                        &format!("m{index}"),
                        index as i64,
                    )]
                })
                .collect();
        });
        let hub = hub(&fixture, peer.port);

        let view = hub.view(&fixture.card).await.expect("投影");
        assert_eq!(view.messages.len(), MAX_MESSAGE_PAGES);
        assert_eq!(message_requests(&peer).len(), MAX_MESSAGE_PAGES);
        assert!(!view.complete, "预算耗尽停止 = 不完整");
        assert!(
            !hub.delivery_evidence(SESSION_ID),
            "预算耗尽停止 = 不完整，不得作投递证据"
        );
    }

    /// R3：`OC_RESPONSE_TOO_LARGE` 时对**同一 cursor** 折半重试 limit（20→10→5→2→1）。
    ///
    /// 用 1 MiB 的测试预算 + 2 MiB 的假页把降档变成小夹具可证；同时断言重试不消耗
    /// 页预算（页预算 2 仍读到两个成功页）、降档只作用于当前页（换 cursor 后回到 20）。
    #[tokio::test]
    async fn oversized_message_page_downshifts_limit() {
        assert_eq!(MESSAGE_LIMIT, 20, "R3：单页 limit 必须是 20");
        assert_eq!(MESSAGE_LIMIT_LADDER, [20, 10, 5, 2, 1], "降档阶梯固定");
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.pages = vec![
                vec![user_message("msg_a", "a", 1)],
                vec![user_message("msg_b", "b", 2)],
                vec![user_message("msg_c", "c", 3)],
            ];
            // limit ≥ 10 的页超过 1 MiB 测试预算；limit 5 正常（2 MiB 填充）。
            script.oversize_trigger_limit = Some(10);
            script.oversize_padding_bytes = 2 * 1024 * 1024;
        });
        let client = native_client(peer.port);
        let target = native_target(&project);

        let (records, complete, _oversize) =
            read_records_with(&client, &target, 2, 10, 1024 * 1024)
                .await
                .expect("降档后必须读成功");

        assert_eq!(records.len(), 2, "页预算 2：两个成功页各一条记录");
        assert!(!complete, "页预算耗尽停止 = 不完整");
        let requests = message_requests(&peer);
        let limits: Vec<String> = requests
            .iter()
            .map(|request| query_param(&request.target, "limit").expect("每页都带 limit"))
            .collect();
        assert_eq!(
            limits,
            vec!["20", "10", "5", "20", "10", "5"],
            "每页各自的同一 cursor 折半 20→10→5；换页后从默认 20 重新开始"
        );
        assert!(
            requests[..3].iter().all(|request| {
                request.target.contains("order=asc") && !request.target.contains("cursor=")
            }),
            "首页的**每一次**降档重试都必须 order=asc 且不带 cursor：{:?}",
            requests[..3]
                .iter()
                .map(|request| request.target.as_str())
                .collect::<Vec<_>>()
        );
        assert!(
            requests[3..]
                .iter()
                .all(|request| query_param(&request.target, "cursor").as_deref() == Some("c1")),
            "第二页的三次尝试必须落在同一个 cursor c1 上"
        );
        assert!(
            requests[3..]
                .iter()
                .all(|request| !request.target.contains("order=")),
            "后续页（含降档重试）只带 limit+cursor，不得带 order：{:?}",
            requests[3..]
                .iter()
                .map(|request| request.target.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            requests.len(),
            6,
            "四次降档重试不得消耗 2 页预算（2 个成功页 + 4 次重试）"
        );
    }

    /// R3：limit=1 仍超限 ⇒ 停止、保留已读记录、`complete=false`，绝不冒充完整；
    /// 不得在 limit=1 上继续重试（总尝试 1 + 5 次即止）。
    #[tokio::test]
    async fn oversize_at_limit_one_marks_incomplete() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.pages = vec![
                vec![user_message("msg_a", "a", 1)],
                vec![user_message("msg_b", "b", 2)],
            ];
            // 第二页起、任何 limit 都超预算（2 MiB 填充 > 1 MiB 预算）。
            script.oversize_trigger_limit = Some(1);
            script.oversize_from_cursor = 1;
            script.oversize_padding_bytes = 2 * 1024 * 1024;
        });
        let client = native_client(peer.port);
        let target = native_target(&project);

        let (records, complete, _oversize) = read_records_with(
            &client,
            &target,
            MAX_MESSAGE_PAGES,
            MAX_MESSAGE_PAGE_ATTEMPTS,
            1024 * 1024,
        )
        .await
        .expect("limit=1 仍超限只停止，不上抛错误");

        assert_eq!(records.len(), 1, "已读记录必须保留");
        assert!(!complete, "limit=1 仍超限停止 = 不完整");
        let requests = message_requests(&peer);
        let limits: Vec<String> = requests
            .iter()
            .map(|request| query_param(&request.target, "limit").expect("每页都带 limit"))
            .collect();
        assert_eq!(
            limits,
            vec!["20", "20", "10", "5", "2", "1"],
            "首页一次 + 超限页阶梯 20→10→5→2→1 后停止"
        );
        assert_eq!(requests.len(), 6, "limit=1 之后不得再重试");
        assert!(
            requests[1..]
                .iter()
                .all(|request| query_param(&request.target, "cursor").as_deref() == Some("c1")),
            "阶梯上的每次重试都必须带同一个 cursor"
        );
    }

    /// G5 回归：单条 16 MiB 附件的 22.4 MB 级读回在旧 4 MiB 上限下直接
    /// `OC_RESPONSE_TOO_LARGE`。这里用 >4 MiB（5 MiB）消息页证明真实 32 MiB 预算下
    /// 能完整解析且 `complete` 正确；不提交大文件，正文在内存里生成。
    #[tokio::test]
    async fn large_message_page_over_four_mib_reads_under_real_budget() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        // 5 MiB 正文 → JSON 体 > 4 MiB（旧上限），但远小于 32 MiB。
        let text = "x".repeat(5 * 1024 * 1024);
        peer.set(|script| {
            script.pages = vec![vec![user_message("msg_big", &text, 1)]];
            script.cursor_shape = CursorShape::Scalar; // 尾页 = 标量 null
        });
        let hub = hub(&fixture, peer.port);

        let view = hub
            .view(&fixture.card)
            .await
            .expect(">4 MiB 的消息页在 32 MiB 预算内必须读回");
        assert_eq!(view.messages.len(), 1);
        assert_eq!(view.messages[0].content.len(), text.len());
        assert!(view.complete, "单页无游标 = 权威尾页");
        assert!(hub.delivery_evidence(SESSION_ID));
        assert_eq!(message_requests(&peer).len(), 1, "5 MiB 不需要降档重试");
    }

    /// G5：22.4 MB 附件读回在真实 32 MiB 整包预算内必须解析成功。
    ///
    /// 旧 4 MiB 上限会把合法的 16 MiB 附件（base64 后约 22.4 MB）直接判成
    /// `OC_RESPONSE_TOO_LARGE`。这里用生产常量 [`MAX_RESPONSE_BYTES`] 作为注入预算
    /// （不是更小的测试预算），让假 peer 回一页带 `files[0].data` 的记录：
    /// 请求一次、记录可投影、页尾无游标 ⇒ `complete=true`。正文在内存生成，不提交夹具。
    #[tokio::test]
    async fn large_attachment_readback_parses_under_real_32_mib_budget() {
        // 22.4 MB（十进制）= 22_400_000 B；16 B 一段，正好整除。
        let data = "QUJDREVGR0gAAAA=".repeat(22_400_000 / 16);
        let data_len = data.len();
        assert_eq!(data_len, 22_400_000);
        assert!(
            data_len + 1024 < MAX_RESPONSE_BYTES,
            "22.4 MB 附件必须落在 32 MiB 预算内：{data_len} vs {MAX_RESPONSE_BYTES}"
        );
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        let mut record = user_message("msg_big_attachment", "看这个大附件", 1);
        record["files"] = json!([{ "name": "big.bin", "data": data }]);
        peer.set(|script| {
            script.pages = vec![vec![record]];
            script.cursor_shape = CursorShape::Scalar; // 尾页 = 标量 null
        });
        let client = native_client(peer.port);
        let target = native_target(&project);

        let (records, complete, _oversize) = read_records_with(
            &client,
            &target,
            MAX_MESSAGE_PAGES,
            MAX_MESSAGE_PAGE_ATTEMPTS,
            MAX_RESPONSE_BYTES,
        )
        .await
        .expect("合法 22.4 MB 附件必须在真实 32 MiB 预算内读回");

        assert_eq!(records.len(), 1, "一页一条记录");
        assert!(complete, "单页且无游标 = 权威尾页");
        assert_eq!(message_requests(&peer).len(), 1, "合法大页不需要降档重读");
        assert_eq!(
            records[0]
                .pointer("/files/0/data")
                .and_then(Value::as_str)
                .map(str::len),
            Some(data_len),
            "附件正文必须完整保留（不得截断）"
        );
        let messages = project_messages(&json!({ "data": records })).expect("消息必须可投影");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "看这个大附件");
    }

    /// D-N5-4（恰 20 MiB 合计附件形态：单条记录超真实 32 MiB 预算）：**读面整体不得 409**。
    ///
    /// 视图必须优雅降级（200 形状：可达内容保留 + `complete=false`），且终止原因
    /// 如实标注为超限（`partialReason=OC_RESPONSE_TOO_LARGE`），不与预算耗尽等其它
    /// 部分原因混同；超限记录的数量与内容保持未知（不冒充读过）。回归钉住：
    /// 任何把超限读面重新变成整体错误的改动都会在本用例变红。
    #[tokio::test]
    async fn oversize_message_page_degrades_view_with_honest_annotation() {
        // 40 MB > 32 MiB 预算：任何 limit 档（含 1）都超限（N5 的「单条 20 MiB base64
        // ≈27 MB 级记录」在更小预算下的同构放大，正文内存生成、不提交夹具）。
        let data = "QUJDREVGR0gAAAA=".repeat(40_000_000 / 16);
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        let mut record = user_message("msg_huge", "恰 20 MiB 合计", 1);
        record["files"] = json!([{ "name": "a.bin", "data": data }]);
        peer.set(|script| {
            script.pages = vec![vec![record]];
            script.cursor_shape = CursorShape::Scalar;
        });
        let hub = hub(&fixture, peer.port);
        let view = hub
            .view(&fixture.card)
            .await
            .expect("超限会话的视图必须 200 形状可用（读写面保留），不得整体 409");
        assert_eq!(view.messages.len(), 0, "超限页不可达：零记录（不冒充读过）");
        assert!(!view.complete, "从未读到权威终止：不得声明完整");
        assert_eq!(
            view.partial_reason.as_deref(),
            Some(codes::OC_RESPONSE_TOO_LARGE),
            "终止原因必须如实标注为超限（D-N5-4）"
        );
        // 降档阶梯走满后停止（预算有界），不再有任何一次成功消息页。
        let limits: Vec<String> = message_requests(&peer)
            .iter()
            .filter_map(|request| query_param(&request.target, "limit"))
            .collect();
        assert_eq!(
            limits,
            vec!["20", "10", "5", "2", "1"],
            "视图链路同样走完整降档阶梯后如实停止"
        );
    }

    /// R3：**首页**每一档 limit 都超预算，limit=1 仍超限 ⇒ 零记录 + 不完整，
    /// 尝试次数受阶梯约束（不是无限降档/重试）。区别于既有「第二页才超限」用例。
    #[tokio::test]
    async fn first_page_all_oversize_stops_with_zero_records() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.pages = vec![vec![user_message("msg_a", "a", 1)]];
            // 首页任何 limit 都超 1 MiB 测试预算（2 MiB 填充）。
            script.oversize_trigger_limit = Some(1);
            script.oversize_from_cursor = 0;
            script.oversize_padding_bytes = 2 * 1024 * 1024;
        });
        let client = native_client(peer.port);
        let target = native_target(&project);

        let (records, complete, _oversize) = read_records_with(
            &client,
            &target,
            MAX_MESSAGE_PAGES,
            MAX_MESSAGE_PAGE_ATTEMPTS,
            1024 * 1024,
        )
        .await
        .expect("首页超限只降档，不上抛错误");

        assert!(records.is_empty(), "首页从未读成功：零记录");
        assert!(!complete, "首页都没读到，不能声明完整");
        // D-N5-4：降档到 1 仍超限的终止必须带超限标注（区别于预算耗尽等其它部分原因）。
        assert!(_oversize, "首页超限终止必须如实标注 oversize_stopped");
        let requests = message_requests(&peer);
        let limits: Vec<String> = requests
            .iter()
            .map(|request| query_param(&request.target, "limit").expect("每页都带 limit"))
            .collect();
        assert_eq!(
            limits,
            vec!["20", "10", "5", "2", "1"],
            "首页也必须走完整降档阶梯后停在 1"
        );
        assert!(
            requests.len() <= MESSAGE_LIMIT_LADDER.len() + 1,
            "尝试次数必须有界（阶梯长度 + 1），实际 {}",
            requests.len()
        );
        assert!(
            requests.iter().all(|request| {
                request.target.contains("order=asc") && !request.target.contains("cursor=")
            }),
            "首页（含降档重试）必须始终 order=asc 且不带 cursor：{:?}",
            requests
                .iter()
                .map(|request| request.target.as_str())
                .collect::<Vec<_>>()
        );
    }

    /// 重连后的重建会读到重叠页：按原生消息 id 去重，seq 在去重之后生成。
    #[tokio::test]
    async fn reconnect_rebuild_deduplicates_message_ids() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.pages = vec![vec![
                user_message("msg_a", "a", 1),
                user_message("msg_b", "b", 2),
            ]];
        });
        let hub = hub(&fixture, peer.port);
        let mut rx = hub.watch(SESSION_ID);

        let first = hub.view(&fixture.card).await.expect("首次投影");
        assert_eq!(first.messages.len(), 2);
        let _ = rx.borrow_and_update();

        // 断线重连后补读：第二页与第一页有重叠 id。
        peer.set(|script| {
            script.pages = vec![
                vec![user_message("msg_a", "a", 1), user_message("msg_b", "b", 2)],
                vec![user_message("msg_b", "b", 2), user_message("msg_c", "c", 3)],
            ];
        });
        wait_for(|| peer.event_conns() >= 1, "事件流建连").await;
        peer.push_event();
        tokio::time::timeout(WAIT, rx.changed())
            .await
            .expect("事件必须触发只读重建并发布修订")
            .expect("watch 未关闭");

        let rebuilt = hub.view(&fixture.card).await.expect("重建后投影");
        let contents: Vec<&str> = rebuilt
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(contents, vec!["a", "b", "c"], "重叠 id 不得重复出现");
        assert_eq!(
            rebuilt.messages.iter().map(|m| m.seq).collect::<Vec<_>>(),
            vec![0, 1, 2],
            "seq 在去重之后生成"
        );
        assert_eq!(peer.event_conns(), 1, "重建不得另开事件流");
    }

    #[tokio::test]
    async fn public_message_identity_survives_reordering_and_revision() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|s| s.pages = vec![vec![user_message("native_a", "first", 1)]]);
        let hub = hub(&fixture, peer.port);
        let first = serde_json::to_value(hub.view(&fixture.card).await.unwrap()).unwrap();
        assert_eq!(first["messageIds"]["0"], "native_a");
        peer.set(|s| {
            s.pages = vec![vec![
                user_message("native_b", "earlier", 0),
                user_message("native_a", "revised", 1),
            ]]
        });
        let next = serde_json::to_value(hub.view(&fixture.card).await.unwrap()).unwrap();
        assert_eq!(next["messageIds"]["1"], "native_a");
        assert_eq!(next["messages"][1]["content"], "revised");
    }

    #[tokio::test]
    async fn unchanged_client_reads_do_not_publish_read_feedback() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        let hub = hub(&fixture, peer.port);
        let mut a = hub.watch(SESSION_ID);
        let mut b = hub.watch(SESSION_ID);
        let first = hub.view(&fixture.card).await.unwrap();
        a.borrow_and_update();
        b.borrow_and_update();
        let second = hub.view(&fixture.card).await.unwrap();
        assert_eq!(second.messages, first.messages);
        assert_eq!(second.revision, first.revision);
        assert!(!a.has_changed().unwrap(), "无内容变化不应唤醒客户端 A");
        assert!(!b.has_changed().unwrap(), "无内容变化不应唤醒客户端 B");
        assert_eq!(
            hub.view(&fixture.card).await.unwrap().revision,
            first.revision
        );
    }

    /// 两个订阅者共享同一个 SSE 上游：假 peer 只看到一条事件连接。
    #[tokio::test]
    async fn two_watchers_share_one_upstream_stream() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.pages = vec![vec![user_message("msg_a", "a", 1)]];
        });
        let hub = hub(&fixture, peer.port);
        let mut first = hub.watch(SESSION_ID);
        hub.view(&fixture.card).await.expect("投影");
        wait_for(|| peer.event_conns() >= 1, "事件流建连").await;
        // 第二个订阅者与再次投影都必须复用同一条上游，而不是各自新开。
        let mut second = hub.watch(SESSION_ID);
        hub.view(&fixture.card).await.expect("二次投影");
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(peer.event_conns(), 1, "两个订阅者只能有一个上游流");

        let _ = first.borrow_and_update();
        let _ = second.borrow_and_update();
        peer.push_event();
        tokio::time::timeout(WAIT, first.changed())
            .await
            .expect("第一个订阅者必须收到修订")
            .expect("watch 未关闭");
        tokio::time::timeout(WAIT, second.changed())
            .await
            .expect("第二个订阅者必须收到修订")
            .expect("watch 未关闭");
        assert_eq!(peer.event_conns(), 1, "重建不得新增事件连接");
    }

    /// 实机回归（2026-11-22）：tool-call 的 id 必须绑定到**它自己的**投影下标。
    ///
    /// 曾经的实现按 content 顺序二次推断下标，遇到「completed 工具随后多推一条 tool-result」
    /// 就从那里起错位，最终整条消息的 `toolCallIds` 为空——手机端于是永远无法把官方表单结果
    /// 归位到原提问（实机浏览器验证发现：结果读到了，但一处也不挂）。
    #[test]
    fn chat_projection_tool_call_ids_survive_completed_tool_results() {
        let records = vec![json!({
            "id": "msg_a",
            "type": "assistant",
            "time": {"created": 1},
            "content": [
                {"type": "reasoning", "text": "think"},
                {"type": "tool", "id": "call_q1", "name": "question",
                 "state": {"status": "completed", "input": {}, "output": "ok"}},
                {"type": "text", "text": "after"}
            ]
        })];
        let projection = project_records_with_seq(&records).unwrap();
        // 三条消息：thinking(0) / tool-call(1) / tool-result(2) / text(3) —— 共 4 条；
        // 关键是 tool-call 的下标 1 必须映射到 call_q1，其余下标不得映射。
        assert_eq!(
            projection.tool_call_ids.get(&1).map(String::as_str),
            Some("call_q1")
        );
        assert_eq!(
            projection.tool_call_ids.len(),
            1,
            "{:?}",
            projection.tool_call_ids
        );
        assert_eq!(projection.messages[1].kind, "tool-call");
        assert_eq!(
            projection.messages[1].tool_name.as_deref(),
            Some("question")
        );
    }

    // OpenCode 2.x 生产问答 T.ask 将 assistant 消息和工具调用 ID 放在 metadata.tool。
    #[tokio::test]
    async fn chat_projection_form_result_serializes_native_tool_link() {
        let metadata = json!({"kind": "question", "tool": {
            "messageID": "msg_assistant", "id": "call_question"
        }});
        let result = chat_projection_form_detail_json(Some(metadata)).await;
        assert_eq!(
            (result.get("toolMessageId"), result.get("toolCallId")),
            (Some(&json!("msg_assistant")), Some(&json!("call_question")))
        );
    }

    #[tokio::test]
    async fn chat_projection_incomplete_tool_link_keeps_answered_result() {
        for metadata in [
            None,
            Some(json!({"kind": "question", "tool": "invalid"})),
            Some(json!({"kind": "question", "tool": {"id": "call_only"}})),
        ] {
            let result = chat_projection_form_detail_json(metadata).await;
            assert_eq!(result["status"], "answered");
            assert!(result["answers"].get("q0").is_some());
            for key in ["toolMessageId", "toolCallId"] {
                assert!(
                    result.get(key).is_none_or(Value::is_null),
                    "{key}: {result}"
                );
            }
        }
    }

    async fn chat_projection_form_detail_json(metadata: Option<Value>) -> Value {
        let fixture = fixture();
        let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
        peer.set(|script| script.form_metadata = metadata);
        let result = hub(&fixture, peer.port)
            .form_result(&fixture.card, "frm-test")
            .await
            .expect("metadata 不应阻止读取原生结果");
        serde_json::to_value(result).unwrap()
    }

    #[test]
    fn chat_projection_form_status_only_answered_keeps_answers() {
        let answer = json!({"q0": "blue"});
        for (status, expected) in [
            (Some("answered"), "answered"),
            (Some("cancelled"), "cancelled"),
            (Some("pending"), "pending"),
            (Some("unknown"), "unknown"),
            (Some("unrecognized"), "unknown"),
            (None, "unknown"),
        ] {
            let spec = FormSpec {
                id: "frm-test".into(),
                fields: Vec::new(),
                content_hash: String::new(),
            };
            let result =
                form_result_from(SESSION_ID, "frm-test", &spec, status, Some(&answer), None);
            assert_eq!(
                (result.status.as_str(), result.answers.contains_key("q0")),
                (expected, expected == "answered")
            );
        }
    }

    // 会话 GET 的 data.model 是只读来源；模型只允许给公开视图新增一个顶层键。
    #[tokio::test]
    async fn chat_projection_public_view_adds_only_model_for_valid_session_model() {
        let fixture = fixture();
        let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
        let hub = hub(&fixture, peer.port);
        let baseline = serde_json::to_value(hub.view(&fixture.card).await.unwrap()).unwrap();
        let mut expected_keys: std::collections::BTreeSet<_> =
            baseline.as_object().unwrap().keys().cloned().collect();
        assert!(expected_keys.insert("model".to_owned()));
        for model in [
            json!({"id": "test-model", "providerID": "test-provider", "variant": "fast"}),
            json!({"id": "test-model", "providerID": "test-provider"}),
        ] {
            peer.set(|script| script.session_model = Some(model.clone()));
            let view = serde_json::to_value(hub.view(&fixture.card).await.unwrap()).unwrap();
            assert_eq!(view.get("model"), Some(&model));
            assert_eq!(
                view.as_object()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>(),
                expected_keys
            );
        }
    }

    #[tokio::test]
    async fn chat_projection_public_view_omits_missing_or_invalid_session_model() {
        let fixture = fixture();
        let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
        let hub = hub(&fixture, peer.port);
        for model in [
            None,
            Some(json!("invalid")),
            Some(json!({"id": "no-provider"})),
        ] {
            peer.set(|script| script.session_model = model);
            let view = serde_json::to_value(hub.view(&fixture.card).await.unwrap()).unwrap();
            assert!(!view.as_object().unwrap().contains_key("model"));
        }
    }

    /// view 与事件重建都只读：假 peer 记录到的非 GET 请求必须为 0。
    #[tokio::test]
    async fn view_and_event_rebuild_never_write() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.pages = vec![vec![user_message("msg_a", "a", 1)]];
            script.forms = vec![pending_form(json!([
                { "key": "q0", "title": "COLOR", "type": "string" }
            ]))];
            script.permissions = vec![json!({
                "id": "per_fixture",
                "sessionID": SESSION_ID,
                "action": "read",
                "resources": [".env"],
                "source": { "type": "tool", "messageID": "msg_a", "id": "call_1" },
            })];
            script.inbox = vec![json!({
                "id": "msg_queue",
                "type": "user",
                "payload": { "text": "排队中" },
                "delivery": "queue",
            })];
        });
        let hub = hub(&fixture, peer.port);
        let mut rx = hub.watch(SESSION_ID);

        let view = hub.view(&fixture.card).await.expect("投影");
        assert_eq!(view.pending.len(), 2, "表单与审批都属于 pending");
        assert_eq!(view.inbox.len(), 1);
        let _ = rx.borrow_and_update();
        wait_for(|| peer.event_conns() >= 1, "事件流建连").await;
        peer.push_event();
        tokio::time::timeout(WAIT, rx.changed())
            .await
            .expect("事件重建")
            .expect("watch 未关闭");
        hub.view(&fixture.card).await.expect("二次投影");

        assert_eq!(peer.non_get(), 0, "只读任务不得发出任何写请求");
        assert!(
            peer.log().iter().all(|request| request.method == "GET"),
            "所有原生请求都必须是 GET"
        );
        assert!(peer.total() > 0, "必须真的读过原生端点");
    }

    /// 惰性容器：`new`/`with_sources` 不发现、不连接；未登记服务在投影时才给出受控错误。
    #[tokio::test]
    async fn hub_is_lazy_and_reports_unregistered_service_on_view() {
        let fixture = fixture();
        let hub = NativeHub::with_coordinator_sources(
            DeviceStore::memory(),
            DiscoveryInput {
                state_file: fixture.state_file.clone(),
                home: fixture.home.clone(),
            },
            process_fact(),
        );
        // 没有登记文件也能建容器、能订阅（这一步不触达服务）。
        let receiver = hub.watch(SESSION_ID);
        assert_eq!(*receiver.borrow(), 0);

        let err = hub
            .view(&fixture.card)
            .await
            .expect_err("未登记服务不得投影");
        assert_eq!(err.code, codes::OC_SERVICE_NOT_REGISTERED);
        assert!(
            !err.message.contains(PASSWORD),
            "错误文案不得含凭据：{}",
            err.message
        );
    }

    /// token 规则：消息增长不轮换；表单内容变化轮换 context token；代际变化轮换 binding token。
    #[tokio::test]
    async fn tokens_stay_stable_across_messages_and_rotate_on_change() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.pages = vec![vec![user_message("msg_a", "a", 1)]];
            script.forms = vec![pending_form(json!([
                { "key": "q0", "title": "COLOR", "type": "string" }
            ]))];
        });
        let hub = hub(&fixture, peer.port);

        let first = hub.view(&fixture.card).await.expect("投影");
        let binding = first.binding_token.clone();
        let token = form_token(&first);

        // 回答过程中消息增长：token 不得失效。
        peer.set(|script| {
            script.pages = vec![vec![
                user_message("msg_a", "a", 1),
                user_message("msg_b", "b", 2),
            ]];
        });
        let grown = hub.view(&fixture.card).await.expect("消息增长后投影");
        assert_eq!(grown.messages.len(), 2);
        assert_eq!(
            grown.binding_token, binding,
            "消息增长不得轮换 binding token"
        );
        assert_eq!(form_token(&grown), token, "消息增长不得轮换 context token");

        // 表单内容变化（新增必答字段）：context token 必须换新。
        peer.set(|script| {
            script.forms = vec![pending_form(json!([
                { "key": "q0", "title": "COLOR", "type": "string" },
                { "key": "q1", "title": "NAME", "type": "string", "required": true }
            ]))];
        });
        let changed = hub.view(&fixture.card).await.expect("表单内容变化后投影");
        assert_eq!(
            changed.binding_token, binding,
            "表单内容变化不影响 binding token"
        );
        assert_ne!(
            form_token(&changed),
            token,
            "表单内容变化必须轮换 context token"
        );

        // 同端口换进程（新代际）：binding token 必须换新。
        peer.set(|script| script.pid = PID + 1);
        write_registration(&fixture.state_file, peer.port, PID + 1);
        let restarted = hub.view(&fixture.card).await.expect("新代际投影");
        assert_ne!(
            restarted.binding_token, binding,
            "代际变化必须轮换 binding token"
        );
    }

    /// 实机回归（2026-11-22）：`synthetic` 类型的 inbox 项是 OpenCode 的模式提醒等
    /// **系统注入**，不是用户待发消息——绝不能出现在手机的「原生待发队列」里。
    ///
    /// 现场：切到 Plan 后，app 的输入框上方出现一条
    /// `<system-reminder> You are in Plan mode … </system-reminder>`，带「打断注入 / 撤回」，
    /// 看起来像用户自己能撤回的消息。真实 inbox 项都带 `type`：`user` 才是待发消息，
    /// `synthetic` / `compaction` / `move` 都是内部工作项。
    #[tokio::test]
    async fn inbox_projection_hides_synthetic_system_reminders() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.pages = vec![vec![user_message("msg_a", "hi", 1)]];
            script.forms = vec![];
            script.permissions = vec![];
            script.inbox = vec![
                json!({
                    "id": "msg_mode",
                    "type": "synthetic",
                    "payload": { "text": "<system-reminder> You are in Plan mode. </system-reminder>" },
                    "delivery": "steer"
                }),
                json!({
                    "id": "msg_compact",
                    "type": "compaction",
                    "payload": {},
                    "delivery": "steer"
                }),
                json!({
                    "id": "msg_user",
                    "type": "user",
                    "payload": { "text": "真正的待发消息" },
                    "delivery": "queue"
                }),
            ];
        });
        let hub = hub(&fixture, peer.port);
        let view = hub.view(&fixture.card).await.expect("投影");

        assert_eq!(
            view.inbox.len(),
            1,
            "只允许用户待发项进入：{:?}",
            view.inbox
        );
        assert_eq!(view.inbox[0].id, "msg_user");
        assert_eq!(view.inbox[0].text, "真正的待发消息");
        assert!(
            !view
                .inbox
                .iter()
                .any(|item| item.text.contains("system-reminder")),
            "系统模式提醒不得出现在待发队列"
        );
    }

    /// 实机回归（2026-11-22 用户现场）：inbox 里的 `steer` 项**已经被受理**、正在交给当前
    /// 回合，却在手机上被写成「排队中（agent 空闲后处理）」——与实际相反，用户以为消息没发出去。
    ///
    /// 因此 inbox 项必须携带一个**可区分的状态词**：`queue` = 等 agent 空闲后被消费；
    /// `steer` = 已打断投递、正在交给当前回合。前端据此写不同文案，不得共用一句。
    #[tokio::test]
    async fn inbox_projection_distinguishes_steer_from_queue_state() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.pages = vec![vec![user_message("msg_a", "hi", 1)]];
            script.forms = vec![];
            script.permissions = vec![];
            script.inbox = vec![
                json!({
                    "id": "msg_q",
                    "type": "user",
                    "payload": { "text": "排队的那条" },
                    "delivery": "queue"
                }),
                json!({
                    "id": "msg_s",
                    "type": "user",
                    "payload": { "text": "打断的那条" },
                    "delivery": "steer"
                }),
            ];
        });
        let hub = hub(&fixture, peer.port);
        let view = hub.view(&fixture.card).await.expect("投影");
        assert_eq!(view.inbox.len(), 2);
        let by_id = |id: &str| view.inbox.iter().find(|i| i.id == id).cloned();
        let q = by_id("msg_q").expect("queue 项在场");
        let s = by_id("msg_s").expect("steer 项在场");
        assert_eq!(q.delivery, DeliveryKind::Queue);
        assert_eq!(s.delivery, DeliveryKind::Steer);
        // 状态词必须可区分（不是同一句「排队中」）：queue=等待消费，steer=已受理正在交付。
        assert_eq!(
            inbox_state_word(&q),
            "waiting",
            "queue 项应表达「等待被消费」"
        );
        assert_eq!(
            inbox_state_word(&s),
            "steering",
            "steer 项应表达「已受理、正在交付当前回合」，不能与 queue 同词"
        );
    }

    /// pending/inbox 投影：无 metadata.tool 的表单仍可显示；idle 不等于表单已答。
    #[tokio::test]
    async fn pending_and_inbox_projection_never_treats_idle_as_answered() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.pages = vec![vec![
                user_message("msg_a", "提问", 1),
                json!({ "id": "msg_idle", "type": "idle", "time": { "created": 2 }, "outcome": "succeeded" }),
            ]];
            // 任务 0 的原生 create 表单没有 metadata.tool。
            script.forms = vec![pending_form(json!([{
                "key": "q0",
                "title": "COLOR",
                "type": "string",
                "options": [{ "value": "red", "label": "Red" }],
                "custom": true,
            }]))];
            script.permissions = vec![json!({
                "id": "per_fixture",
                "sessionID": SESSION_ID,
                "action": "read",
                "resources": [".env"],
                "save": ["*"],
                "source": { "type": "tool", "messageID": "msg_a", "id": "call_1" },
            })];
            script.inbox = vec![
                json!({ "id": "msg_steer", "type": "user", "payload": { "text": "打断" }, "delivery": "steer" }),
                json!({ "id": "msg_queue", "type": "user", "payload": { "text": "排队" }, "delivery": "queue" }),
            ];
        });
        let hub = hub(&fixture, peer.port);

        let view = hub.view(&fixture.card).await.expect("投影");
        assert_eq!(view.runtime_state, "idle:succeeded");

        let form = view
            .pending
            .iter()
            .find_map(|item| match item {
                PendingView::Form {
                    id, state, fields, ..
                } => Some((id, state, fields)),
                _ => None,
            })
            .expect("表单分支必须存在");
        assert_eq!(form.0, "frm_fixture");
        assert_eq!(
            form.1, "unknown",
            "列表读不携带状态：既不是 pending 也不是 answered，idle 记录更不能把它变成已答"
        );
        assert_eq!(form.2.len(), 1);

        let permission = view
            .pending
            .iter()
            .find_map(|item| match item {
                PendingView::Permission {
                    id,
                    title,
                    decisions,
                    ..
                } => Some((id, title, decisions)),
                _ => None,
            })
            .expect("审批分支必须存在");
        assert_eq!(permission.0, "per_fixture");
        assert!(
            permission.1.contains("read"),
            "标题须可辨识：{}",
            permission.1
        );
        // 等值断言（不是子集）：顺序或数量上多出/少掉任何一个词都会红。
        assert_eq!(
            permission.2.as_slice(),
            ["once", "reject"],
            "T7 未完成前决定词集合恰好按序是 once + reject（always 关闭）"
        );

        let deliveries: Vec<(&str, &str)> = view
            .inbox
            .iter()
            .map(|item| (item.id.as_str(), item.text.as_str()))
            .collect();
        assert_eq!(
            deliveries,
            vec![("msg_steer", "打断"), ("msg_queue", "排队")]
        );
        assert!(view.inbox.iter().all(|item| {
            matches!(
                item.delivery,
                crate::services::opencode_native::types::DeliveryKind::Queue
                    | crate::services::opencode_native::types::DeliveryKind::Steer
            )
        }));
    }

    /// 无订阅且详情关闭时释放上游；shutdown 停止上游并关闭订阅频道。
    #[tokio::test]
    async fn no_subscribers_release_upstream_and_shutdown_stops_it() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        let hub = hub(&fixture, peer.port);

        let receiver = hub.watch(SESSION_ID);
        hub.view(&fixture.card).await.expect("投影");
        wait_for(|| peer.event_conns() >= 1, "事件流建连").await;
        drop(receiver);
        wait_for(|| peer.event_closed() >= 1, "无订阅后释放上游").await;
        assert_eq!(peer.event_conns(), 1, "释放后不得自动重连");

        let mut receiver = hub.watch(SESSION_ID);
        hub.view(&fixture.card).await.expect("再次投影");
        wait_for(|| peer.event_conns() >= 2, "重新订阅后重建上游").await;
        let _ = receiver.borrow_and_update();

        hub.shutdown();
        wait_for(|| peer.event_closed() >= 2, "shutdown 停止上游").await;
        tokio::time::timeout(WAIT, receiver.changed())
            .await
            .expect("shutdown 必须关闭订阅频道")
            .expect_err("shutdown 后订阅频道应关闭");

        let before = peer.total();
        let err = hub
            .view(&fixture.card)
            .await
            .expect_err("shutdown 后不再服务");
        assert_eq!(err.code, codes::OC_TRANSPORT_FAILED);
        assert_eq!(peer.total(), before, "shutdown 后不得再发任何请求");
    }

    /// R2 / F2：停远程用可恢复的 pause——旧 watch 立即关闭，重新开启后
    /// view/watch 重新发现并恢复；永久 shutdown 语义由上一个用例继续锁定。
    #[tokio::test]
    async fn native_remote_stop_start_restores_view_and_watch() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        let hub = hub(&fixture, peer.port);

        let first = hub.view(&fixture.card).await.expect("首次投影");
        assert!(first.revision >= 1);
        let mut old_watch = hub.watch(SESSION_ID);

        // 生产 stop_server_with 在停远程时调用它（不是永久 shutdown）。
        hub.pause();
        let closed = tokio::time::timeout(WAIT, old_watch.changed())
            .await
            .expect("pause 必须在时限内关闭旧 watch");
        assert!(
            closed.is_err(),
            "pause 后旧订阅必须观察到频道关闭（发送端被丢弃）"
        );

        // 重启后的第一个 view 必须重新发现、重建投影，而不是吃 stopped。
        let reopened = hub
            .view(&fixture.card)
            .await
            .expect("pause 后必须恢复 view");
        assert_eq!(reopened.revision, 1, "新监听从新的修订序列开始");
        let mut fresh_watch = hub.watch(SESSION_ID);
        peer.set(|script| {
            script.pages = vec![vec![user_message("msg_rev", "真实变更", 1)]];
        });
        let third = hub.view(&fixture.card).await.expect("新监听第二次投影");
        tokio::time::timeout(WAIT, fresh_watch.changed())
            .await
            .expect("新 watch 必须在时限内收到修订")
            .expect("新 watch 频道不得关闭");
        assert_eq!(*fresh_watch.borrow(), third.revision);
        assert_eq!(third.revision, 2);
    }

    /// R2 / F2：pause 不碰动作账本与路由归属——重启前后动作仍可读、verify 仍可用、
    /// 归属保留，重新打开视图仍报 owned。
    #[tokio::test]
    async fn native_hot_restart_preserves_action_recovery() {
        use crate::services::opencode_native::{
            ActionState, NativeCommand, NativeOperation, ReserveOutcome, SendPayload,
        };

        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        let hub = hub(&fixture, peer.port);

        let (first, claim) = hub
            .view_and_claim(&fixture.card, true)
            .await
            .expect("首次投影并认领");
        assert!(claim.owned, "首次投影必须认领原生路由");
        let target = hub
            .dispatch_snapshot(&fixture.card)
            .await
            .expect("派发快照")
            .target;
        let command = NativeCommand {
            action_id: "act_restart".to_string(),
            session_id: SESSION_ID.to_string(),
            binding_token: first.binding_token.clone(),
            operation: NativeOperation::Send(SendPayload {
                text: "热重启保留".to_string(),
                attachment_paths: Vec::new(),
                delivery: DeliveryKind::Queue,
                client_message_id: "msg_act_restart".to_string(),
            }),
        };
        assert!(matches!(
            hub.action_store()
                .reserve("dev-1", &target, &command)
                .expect("登记动作"),
            ReserveOutcome::New
        ));
        assert!(
            hub.action_store().claim("act_restart").expect("取得写出权"),
            "派发前必须能 claim"
        );

        hub.pause();

        let kept = hub
            .action_store()
            .read("dev-1", "act_restart")
            .expect("pause 后动作账本仍可读");
        assert_eq!(kept.action_id, "act_restart");
        assert!(
            hub.action_store()
                .owns_session(SESSION_ID)
                .expect("路由归属可读"),
            "pause 不得重置路由归属"
        );
        let verified = hub
            .verify("dev-1", "act_restart")
            .await
            .expect("pause 后只读核对仍可用");
        assert!(
            matches!(
                verified.state,
                ActionState::Dispatching | ActionState::Unconfirmed
            ),
            "核对只能如实收敛在飞行，实际 {:?}",
            verified.state
        );

        let (reopened, claim) = hub
            .view_and_claim(&fixture.card, true)
            .await
            .expect("重启后投影");
        assert!(claim.owned, "既有归属必须在重启后保留");
        assert_eq!(reopened.revision, 1);
        assert_eq!(
            hub.action_store()
                .read("dev-1", "act_restart")
                .expect("重启后动作仍可读")
                .state,
            verified.state,
            "重启不得改写账本状态"
        );
    }

    /// R2 / F2：旧监听的在途 view 跨过 pause 后不得发布到新监听。用一次阻塞的
    /// inbox 读制造确定性交错：旧 view 已读完消息、只差 commit；重启后的新 view
    /// 先发布，旧结果放行时必须被代际检查拒绝，且不影响新修订序列/视图内容。
    #[tokio::test]
    async fn old_listener_work_cannot_publish_after_restart() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.pages = vec![vec![user_message("msg_old", "旧监听", 1)]];
        });
        let hold = Arc::new(Hold::new());
        peer.set(|script| script.hold_inbox = Some(hold.clone()));
        let hub = Arc::new(hub(&fixture, peer.port));
        let session = fixture.card.clone();

        let old_hub = Arc::clone(&hub);
        let old_session = session.clone();
        let old = tokio::spawn(async move { old_hub.view(&old_session).await });
        tokio::time::timeout(WAIT, hold.started.notified())
            .await
            .expect("旧 view 必须阻塞在最后一次读（inbox）");

        hub.pause();
        peer.set(|script| {
            script.pages = vec![vec![user_message("msg_new", "新监听", 2)]];
        });
        let fresh = hub.view(&session).await.expect("重启后的新投影");
        assert_eq!(fresh.revision, 1);
        assert_eq!(fresh.messages.len(), 1);
        assert!(
            fresh.messages[0].content.contains("新监听"),
            "新监听必须读到新内容"
        );

        hold.release.notify_one();
        let stale = tokio::time::timeout(WAIT, old)
            .await
            .expect("旧 view 必须在放行后收尾")
            .expect("旧 view 任务不得 panic");
        assert!(stale.is_err(), "旧监听的在途 view 不得在新监听上发布");

        let after = hub.view(&session).await.expect("新监听仍可投影");
        assert_eq!(
            after.revision, 1,
            "内容未变且旧 view 被拒绝：修订号必须保持不变"
        );
        assert!(
            after.messages.iter().all(|m| !m.content.contains("旧监听")),
            "旧快照不得污染新视图"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn first_claim_rejects_source_replaced_after_final_identity_check() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer_a = Peer::start(Script::new(&project)).await;
        let peer_b = Peer::start(Script::new(&project)).await;
        peer_b.set(|script| script.pid = PID + 1);
        write_registration(&fixture.state_file, peer_a.port, PID);
        let hub = Arc::new(
            NativeHub::with_sources(
                DeviceStore::memory(),
                DiscoveryInput {
                    state_file: fixture.state_file.clone(),
                    home: fixture.home.clone(),
                },
                process_fact(),
                Some(Arc::new(|_| false)),
                None,
            )
            .with_test_ownership_evidence(),
        );
        assert!(hub
            .inner
            .store
            .with(
                |conn| crate::database::dao::opencode_actions::route_conn(conn, SESSION_ID)
                    .unwrap()
            )
            .is_none());
        let hold = Arc::new(Hold::new());
        *hub.inner.before_claim_test_hold.lock().unwrap() = Some(hold.clone());
        let view_hub = hub.clone();
        let card = fixture.card.clone();
        let view_task = tokio::spawn(async move { view_hub.view_and_claim(&card, true).await });
        tokio::time::timeout(WAIT, hold.started.notified())
            .await
            .expect("A 完成最终身份核验");
        write_registration(&fixture.state_file, peer_b.port, PID + 1);
        let b_hub = hub.clone();
        let b_task = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(b_hub.inner.source_for_session(SESSION_ID))
        });
        let source_b = tokio::time::timeout(WAIT, b_task)
            .await
            .expect("B 必须完成安装")
            .expect("B 线程不得 panic")
            .expect("安装 B");
        assert_eq!(source_b.descriptor.runtime.pid, PID + 1);
        hold.release.notify_one();
        let result = tokio::time::timeout(WAIT, view_task)
            .await
            .expect("A 必须结束")
            .unwrap();
        let route = hub.inner.store.with(|conn| {
            crate::database::dao::opencode_actions::route_conn(conn, SESSION_ID).unwrap()
        });
        let published_a = hub
            .inner
            .state_lock()
            .sessions
            .get(SESSION_ID)
            .and_then(|entry| entry.snapshot.as_ref())
            .is_some_and(|snapshot| snapshot.target.runtime.pid == PID);
        assert!(result.is_err(), "B 安装后 A 不得返回 token");
        assert!(route.is_none(), "B 安装后 A 不得首次认领路由");
        assert!(!published_a, "B 安装后不得发布 A 快照");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn source_install_cannot_interleave_first_claim_and_publication() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer_a = Peer::start(Script::new(&project)).await;
        let peer_b = Peer::start(Script::new(&project)).await;
        peer_b.set(|script| script.pid = PID + 1);
        write_registration(&fixture.state_file, peer_b.port, PID + 1);
        let preparation_hub = NativeHub::with_sources(
            DeviceStore::memory(),
            DiscoveryInput {
                state_file: fixture.state_file.clone(),
                home: fixture.home.clone(),
            },
            process_fact(),
            Some(Arc::new(|_| false)),
            None,
        );
        let source_b = preparation_hub
            .inner
            .source_for_session(SESSION_ID)
            .await
            .expect("准备 B source");
        assert_eq!(source_b.descriptor.runtime.pid, PID + 1);
        write_registration(&fixture.state_file, peer_a.port, PID);
        let hub = Arc::new(
            NativeHub::with_sources(
                DeviceStore::memory(),
                DiscoveryInput {
                    state_file: fixture.state_file.clone(),
                    home: fixture.home.clone(),
                },
                process_fact(),
                Some(Arc::new(|_| false)),
                None,
            )
            .with_test_ownership_evidence(),
        );
        let hold = Arc::new(ThreadHold::new());
        *hub.inner.after_claim_test_hold.lock().unwrap() = Some(hold.clone());
        let view_hub = hub.clone();
        let card = fixture.card.clone();
        let view_task = tokio::spawn(async move { view_hub.view_and_claim(&card, true).await });
        tokio::time::timeout(WAIT, hold.started.notified())
            .await
            .expect("A 首次认领后必须到达发布前");
        let attempted = Arc::new(Notify::new());
        let b_attempted = attempted.clone();
        let b_hub = hub.clone();
        let epoch = hub.inner.epoch();
        let mut b_task = tokio::task::spawn_blocking(move || {
            b_attempted.notify_one();
            b_hub.inner.install(source_b, epoch)
        });
        if tokio::time::timeout(WAIT, attempted.notified())
            .await
            .is_err()
        {
            hold.release();
            panic!("B 线程未到达安装入口");
        }
        // 200ms 只用于反证 B 已提前完成安装，不作为事件顺序的证据。
        let early_result = tokio::time::timeout(Duration::from_millis(200), &mut b_task).await;
        hold.release();
        let installed_before_publish = match early_result {
            Ok(result) => {
                result.expect("B 线程不得 panic").expect("B 安装成功");
                true
            }
            Err(_) => false,
        };
        let view_result = tokio::time::timeout(WAIT, view_task)
            .await
            .expect("A 必须结束")
            .unwrap();
        if !installed_before_publish {
            tokio::time::timeout(WAIT, b_task)
                .await
                .expect("发布后 B 必须结束")
                .unwrap()
                .expect("B 安装成功");
        }
        assert!(!installed_before_publish, "B 不得在 A 认领和发布之间安装");
        let (view, claim) = view_result.expect("A 先进入边界应成功发布");
        assert!(claim.owned);
        assert_eq!(view.service_pid, Some(PID));
    }

    // 服务换代不调用 pause：监听 epoch 与数据库路由都保持不变。
    async fn service_generation_publication_race(claim: bool, wait_on_route: bool) {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let old_peer = Peer::start(Script::new(&project)).await;
        old_peer.set(|s| s.pages = vec![vec![user_message("msg_a", "服务 A", 1)]]);
        let new_peer = Peer::start(Script::new(&project)).await;
        new_peer.set(|s| {
            s.pid = PID + 1;
            s.pages = vec![vec![user_message("msg_b", "服务 B", 2)]];
        });
        let hub = Arc::new(hub(&fixture, old_peer.port));
        if claim {
            hub.view_and_claim(&fixture.card, true)
                .await
                .expect("建立既有 A 路由");
        }
        let route_before = hub.inner.store.with(|conn| {
            crate::database::dao::opencode_actions::route_conn(conn, SESSION_ID).unwrap()
        });
        let hold = Arc::new(Hold::new());
        old_peer.set(|s| s.hold_inbox = Some(hold.clone()));
        let lock = hub.legacy_lock(SESSION_ID);
        let permit = if wait_on_route {
            Some(lock.clone().read_owned().await)
        } else {
            None
        };
        let old_hub = hub.clone();
        let session = fixture.card.clone();
        let old = tokio::spawn(async move {
            if claim {
                old_hub
                    .view_and_claim(&session, true)
                    .await
                    .map(|(view, _)| view)
            } else {
                old_hub.view(&session).await
            }
        });
        tokio::time::timeout(WAIT, hold.started.notified())
            .await
            .expect("A 必须到达最后一次读取");
        if wait_on_route {
            hold.release.notify_one();
            tokio::time::timeout(WAIT, async {
                while lock.clone().try_read_owned().is_ok() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("A 必须排队等待路由写锁");
        }
        write_registration(&fixture.state_file, new_peer.port, PID + 1);
        let fresh = hub.view(&fixture.card).await.expect("B 发布视图");
        assert_eq!(fresh.service_pid, Some(PID + 1));
        assert!(fresh.messages[0].content.contains("服务 B"));
        if wait_on_route {
            drop(permit);
        } else {
            hold.release.notify_one();
        }
        let result = tokio::time::timeout(WAIT, old)
            .await
            .expect("A 必须结束")
            .unwrap();
        let route_after = hub.inner.store.with(|conn| {
            crate::database::dao::opencode_actions::route_conn(conn, SESSION_ID).unwrap()
        });
        assert_eq!(route_before, route_after, "路由必须保持不变");
        let state = hub.inner.state_lock();
        let entry = state.sessions.get(SESSION_ID).unwrap();
        let published = entry.snapshot.as_ref().expect("B 快照必须保留");
        assert_eq!(
            published.binding_token, fresh.binding_token,
            "A 不得覆盖 B 的 token"
        );
        assert!(
            published.messages[0].content.contains("服务 B"),
            "A 不得覆盖 B 的视图"
        );
        assert!(result.is_err(), "旧服务投影不得成功返回旧 token");
    }

    #[tokio::test]
    async fn old_service_view_cannot_overwrite_new_service_publication() {
        service_generation_publication_race(false, false).await;
    }

    #[tokio::test]
    async fn old_service_claim_cannot_overwrite_new_service_publication() {
        service_generation_publication_race(true, false).await;
    }

    #[tokio::test]
    async fn service_change_while_waiting_for_route_lock_rejects_old_claim() {
        service_generation_publication_race(true, true).await;
    }

    #[tokio::test]
    async fn registration_change_while_waiting_for_route_lock_rejects_unobserved_generation() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let old_peer = Peer::start(Script::new(&project)).await;
        old_peer.set(|s| s.pages = vec![vec![user_message("msg_initial", "初始视图", 1)]]);
        let new_peer = Peer::start(Script::new(&project)).await;
        new_peer.set(|s| s.pid = PID + 1);
        write_registration(&fixture.state_file, old_peer.port, PID);
        let hub = Arc::new(
            NativeHub::with_sources(
                DeviceStore::memory(),
                DiscoveryInput {
                    state_file: fixture.state_file.clone(),
                    home: fixture.home.clone(),
                },
                process_fact(),
                Some(Arc::new(|_| false)),
                None,
            )
            .with_test_ownership_evidence(),
        );
        let (initial, _) = hub
            .view_and_claim(&fixture.card, true)
            .await
            .expect("建立 A 的视图与既有路由");
        let route_before = hub.inner.store.with(|conn| {
            crate::database::dao::opencode_actions::route_conn(conn, SESSION_ID).unwrap()
        });
        old_peer.set(|s| s.pages = vec![vec![user_message("msg_late", "迟到的 A", 2)]]);

        let lock = hub.legacy_lock(SESSION_ID);
        let permit = lock.clone().read_owned().await;
        let old_hub = hub.clone();
        let session = fixture.card.clone();
        let old = tokio::spawn(async move { old_hub.view_and_claim(&session, true).await });
        tokio::time::timeout(WAIT, async {
            while lock.clone().try_read_owned().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("A 必须完成投影与身份核验并排队等待路由写锁");

        write_registration(&fixture.state_file, new_peer.port, PID + 1);
        {
            let state = hub.inner.state_lock();
            assert_eq!(state.source.as_ref().unwrap().descriptor.runtime.pid, PID);
        }
        assert_eq!(new_peer.total(), 0, "换登记后不调用 B，也不刷新缓存");
        drop(permit);
        let result = tokio::time::timeout(WAIT, old)
            .await
            .expect("旧请求必须结束")
            .expect("旧请求任务不得 panic");
        let route_after = hub.inner.store.with(|conn| {
            crate::database::dao::opencode_actions::route_conn(conn, SESSION_ID).unwrap()
        });
        assert_eq!(route_before, route_after, "旧请求不得修改既有路由");
        if let Ok((view, _)) = &result {
            assert_eq!(
                view.binding_token, initial.binding_token,
                "成功返回的是 A 的 token"
            );
        }
        assert!(result.is_err(), "登记已换代，旧 A token 不得返回");
        let state = hub.inner.state_lock();
        let entry = state.sessions.get(SESSION_ID).unwrap();
        assert_eq!(entry.revision, initial.revision, "旧投影不得发布新修订");
        assert_eq!(entry.snapshot.as_ref().unwrap().messages, initial.messages);
    }

    // 同一 RuntimeKey 的连接失效后重新发现：旧读取不能借用新 client 的身份发布。
    async fn replace_same_runtime_client(hub: &NativeHub, old: &Arc<Source>) -> Arc<Source> {
        old.client.invalidate();
        let new = hub
            .inner
            .source_for_session(SESSION_ID)
            .await
            .expect("重新发现 client B");
        assert_eq!(new.descriptor.runtime, old.descriptor.runtime);
        assert!(!Arc::ptr_eq(&new.client, &old.client));
        new
    }

    #[tokio::test]
    async fn same_runtime_replaced_client_cannot_commit_old_snapshot() {
        let fixture = fixture();
        let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
        peer.set(|s| s.pages = vec![vec![user_message("old", "旧投影", 1)]]);
        let hub = hub(&fixture, peer.port);
        let epoch = hub.inner.epoch();
        let old_snapshot = hub.inner.refresh(&fixture.card).await.expect("读取 A 投影");
        let old = hub.inner.state_lock().source.as_ref().unwrap().clone();
        replace_same_runtime_client(&hub, &old).await;
        peer.set(|s| s.pages = vec![vec![user_message("new", "新投影", 2)]]);
        let fresh = hub.view(&fixture.card).await.expect("B 发布");
        let mut receiver = hub.watch(SESSION_ID);
        receiver.borrow_and_update();
        let result = hub.inner.commit(old_snapshot, epoch, false, false);
        let state = hub.inner.state_lock();
        let entry = &state.sessions[SESSION_ID];
        assert!(result.is_err(), "替换 client 后不得提交 A 投影");
        assert_eq!(entry.revision, fresh.revision);
        assert!(entry.snapshot.as_ref().unwrap().messages[0]
            .content
            .contains("新投影"));
        assert!(!receiver.has_changed().unwrap());
    }

    #[tokio::test]
    async fn same_runtime_replaced_client_cannot_claim_or_publish_old_snapshot() {
        let fixture = fixture();
        let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
        peer.set(|s| s.pages = vec![vec![user_message("old", "旧投影", 1)]]);
        let hub = Arc::new(hub(&fixture, peer.port).with_test_ownership_evidence());
        let hold = Arc::new(Hold::new());
        *hub.inner.before_claim_test_hold.lock().unwrap() = Some(hold.clone());
        let task_hub = hub.clone();
        let card = fixture.card.clone();
        let task = tokio::spawn(async move { task_hub.view_and_claim(&card, true).await });
        tokio::time::timeout(WAIT, hold.started.notified())
            .await
            .expect("A 完成读取和身份核验");
        let old = hub.inner.state_lock().source.as_ref().unwrap().clone();
        replace_same_runtime_client(&hub, &old).await;
        peer.set(|s| s.pages = vec![vec![user_message("new", "新投影", 2)]]);
        let fresh = hub.view(&fixture.card).await.expect("B 发布");
        let mut receiver = hub.watch(SESSION_ID);
        receiver.borrow_and_update();
        hold.release.notify_one();
        let result = tokio::time::timeout(WAIT, task)
            .await
            .expect("A 收尾")
            .unwrap();
        let route = hub.inner.store.with(|conn| {
            crate::database::dao::opencode_actions::route_conn(conn, SESSION_ID).unwrap()
        });
        let state = hub.inner.state_lock();
        let entry = &state.sessions[SESSION_ID];
        assert!(result.is_err(), "替换 client 后 A 不得认领或返回旧视图");
        assert!(route.is_none(), "A 不得认领路由");
        assert_eq!(entry.revision, fresh.revision);
        assert!(entry.snapshot.as_ref().unwrap().messages[0]
            .content
            .contains("新投影"));
        assert!(!receiver.has_changed().unwrap());
    }

    #[tokio::test]
    async fn same_runtime_replaced_client_cannot_publish_old_watched_rebuild() {
        let fixture = fixture();
        let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
        peer.set(|s| s.pages = vec![vec![user_message("initial", "初始", 1)]]);
        let hub = hub(&fixture, peer.port);
        hub.view(&fixture.card).await.expect("初始投影");
        let mut receiver = hub.watch(SESSION_ID);
        wait_for(|| peer.event_conns() > 0, "上游建连").await;
        peer.set(|s| s.pages = vec![vec![user_message("old", "旧投影", 2)]]);
        let hold = Arc::new(Hold::new());
        peer.set(|s| s.hold_inbox = Some(hold.clone()));
        let inner = hub.inner.clone();
        let epoch = inner.epoch();
        let task = tokio::spawn(async move { inner.rebuild_watched(epoch).await });
        tokio::time::timeout(WAIT, hold.started.notified())
            .await
            .expect("A 已读取消息并等待最后响应");
        let old = hub.inner.state_lock().source.as_ref().unwrap().clone();
        replace_same_runtime_client(&hub, &old).await;
        peer.set(|s| s.pages = vec![vec![user_message("new", "新投影", 3)]]);
        let fresh = hub.view(&fixture.card).await.expect("B 发布");
        receiver.borrow_and_update();
        hold.release.notify_one();
        tokio::time::timeout(WAIT, task)
            .await
            .expect("旧重建收尾")
            .unwrap();
        let state = hub.inner.state_lock();
        let entry = &state.sessions[SESSION_ID];
        assert_eq!(entry.revision, fresh.revision, "旧重建不得推进修订");
        assert!(entry.snapshot.as_ref().unwrap().messages[0]
            .content
            .contains("新投影"));
        assert!(!receiver.has_changed().unwrap());
    }

    async fn assert_pump_rejects_stale_process_fact(fact_after_pin: i64) {
        let fixture = fixture();
        let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
        write_registration(&fixture.state_file, peer.port, PID);
        let fact = Arc::new(std::sync::atomic::AtomicI64::new(STARTED_AT_MS));
        let supplied_fact = fact.clone();
        let hub = NativeHub::with_sources(
            DeviceStore::memory(),
            DiscoveryInput {
                state_file: fixture.state_file.clone(),
                home: fixture.home.clone(),
            },
            Arc::new(move |pid| {
                if pid != PID {
                    return None;
                }
                match supplied_fact.load(Ordering::SeqCst) {
                    0 => None,
                    started => Some(started),
                }
            }),
            Some(Arc::new(|_| false)),
            None,
        );
        hub.view(&fixture.card).await.expect("建立初始投影");
        let mut watch = hub.watch(SESSION_ID);
        wait_for(|| peer.event_conns() > 0, "pump 建立回环事件连接").await;
        let (cancel, client, revision) = {
            let state = hub.inner.state_lock();
            (
                state.pump.as_ref().unwrap().cancel.clone(),
                state.source.as_ref().unwrap().client.clone(),
                state.sessions[SESSION_ID].revision,
            )
        };
        watch.borrow_and_update();
        let log_start = peer.log().len();
        fact.store(fact_after_pin, Ordering::SeqCst);
        hub.inner
            .rebuild_watched_for_pump(hub.inner.epoch(), Some((&cancel, &client)))
            .await;
        let business_reads: Vec<_> = peer
            .log()
            .into_iter()
            .skip(log_start)
            .filter(|request| {
                request
                    .path()
                    .starts_with(&format!("/api/session/{SESSION_ID}"))
            })
            .collect();
        let actual_revision = hub.inner.state_lock().sessions[SESSION_ID].revision;
        assert_eq!(
            (
                business_reads.len(),
                actual_revision,
                watch.has_changed().unwrap()
            ),
            (0, revision, false),
            "进程事实失效后不得读取会话或发布旧绑定；请求: {business_reads:?}"
        );
    }

    #[tokio::test]
    async fn pump_does_not_read_or_publish_when_pid_is_reused() {
        assert_pump_rejects_stale_process_fact(STARTED_AT_MS + 1).await;
    }

    #[tokio::test]
    async fn pump_does_not_read_or_publish_when_process_fact_is_unavailable() {
        assert_pump_rejects_stale_process_fact(0).await;
    }

    #[tokio::test]
    async fn pump_does_not_publish_read_only_when_discovery_reports_generation_mismatch() {
        let fixture = fixture();
        let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
        write_registration(&fixture.state_file, peer.port, PID);
        let hub = NativeHub::with_sources(
            DeviceStore::memory(),
            DiscoveryInput {
                state_file: fixture.state_file.clone(),
                home: fixture.home.clone(),
            },
            process_fact(),
            Some(Arc::new(|_| false)),
            None,
        );
        hub.view(&fixture.card).await.expect("建立初始投影");
        let mut watch = hub.watch(SESSION_ID);
        wait_for(|| peer.event_conns() > 0, "pump 建立回环事件连接").await;
        let (cancel, client, target, revision) = {
            let state = hub.inner.state_lock();
            let entry = &state.sessions[SESSION_ID];
            (
                state.pump.as_ref().unwrap().cancel.clone(),
                state.source.as_ref().unwrap().client.clone(),
                entry.selected_target.as_ref().unwrap().clone(),
                entry.revision,
            )
        };
        let registration = std::fs::read_to_string(&fixture.state_file)
            .unwrap()
            .replace(SERVICE_ID, "another-service-generation");
        std::fs::write(&fixture.state_file, registration).unwrap();
        let error = hub
            .inner
            .verify_service_identity(&target)
            .await
            .expect_err("发现不同代际");
        assert_eq!(error.code, codes::OC_GENERATION_MISMATCH);
        peer.set(|script| script.pages = vec![vec![user_message("stale", "旧绑定内容", 2)]]);
        watch.borrow_and_update();
        hub.inner
            .rebuild_watched_for_pump(hub.inner.epoch(), Some((&cancel, &client)))
            .await;
        let actual_revision = hub.inner.state_lock().sessions[SESSION_ID].revision;
        assert_eq!(
            (actual_revision, watch.has_changed().unwrap()),
            (revision, false),
            "发现代际不符时不得以只读状态发布旧绑定"
        );
    }

    #[tokio::test]
    async fn replaced_pump_stops_multi_session_rebuild_before_reading_or_publishing() {
        const SECOND: &str = "ses_fixture_0002";
        let fixture = fixture();
        let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
        let hub = hub(&fixture, peer.port);
        hub.view(&fixture.card).await.expect("建立首个会话");
        let mut second_card = fixture.card.clone();
        second_card.id = SECOND.to_string();
        peer.set(|script| script.session_id = SECOND.to_string());
        hub.view(&second_card).await.expect("建立第二个会话");
        let mut first_watch = hub.watch(SESSION_ID);
        let mut second_watch = hub.watch(SECOND);
        wait_for(|| peer.event_conns() > 0, "旧 pump 上游建连").await;

        // 与重建使用相同的稳定遍历顺序，避免依赖 HashMap 的随机顺序。
        let (first, later, old_source, old_pump) = {
            let state = hub.inner.state_lock();
            let ids: Vec<_> = state
                .sessions
                .values()
                .filter(|entry| entry.tx.receiver_count() > 0)
                .map(|entry| entry.selected_session.as_ref().unwrap().id.clone())
                .collect();
            assert_eq!(ids.len(), 2);
            (
                ids[0].clone(),
                ids[1].clone(),
                state.source.as_ref().unwrap().clone(),
                state.pump.as_ref().unwrap().cancel.clone(),
            )
        };
        let hold = Arc::new(Hold::new());
        peer.set(|script| {
            script.session_id = first.clone();
            script.hold_inbox = Some(hold.clone());
        });
        peer.push_event();
        tokio::time::timeout(WAIT, hold.started.notified())
            .await
            .expect("旧 pump 等待首个会话");
        let new_source = replace_same_runtime_client(&hub, &old_source).await;
        hub.inner.ensure_pump();
        {
            let state = hub.inner.state_lock();
            let new_pump = state.pump.as_ref().expect("新 pump 已登记");
            assert!(!Arc::ptr_eq(&new_pump.cancel, &old_pump));
            assert!(Arc::ptr_eq(
                &state.source.as_ref().unwrap().client,
                &new_source.client
            ));
        }
        wait_for(|| peer.event_conns() >= 2, "新 pump 上游建连").await;
        let revisions = (
            *first_watch.borrow_and_update(),
            *second_watch.borrow_and_update(),
        );
        peer.set(|script| script.session_id = later.clone());
        let log_start = peer.log().len();
        hold.release.notify_one();
        wait_for(|| peer.event_closed() >= 1, "旧 pump 完成重建并关闭上游").await;
        let later_prefix = format!("/api/session/{later}");
        let later_reads: Vec<_> = peer
            .log()
            .into_iter()
            .skip(log_start)
            .filter(|request| request.path().starts_with(&later_prefix))
            .collect();
        let state = hub.inner.state_lock();
        let actual_revisions = (
            state.sessions[SESSION_ID].revision,
            state.sessions[SECOND].revision,
        );
        assert_eq!(
            (
                later_reads.len(),
                actual_revisions,
                first_watch.has_changed().unwrap(),
                second_watch.has_changed().unwrap()
            ),
            (0, revisions, false, false),
            "旧 pump 不得读取后续会话或发布修订；后续请求: {later_reads:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retired_pump_cannot_rediscover_after_route_read_and_pause() {
        let fixture = fixture();
        let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
        let endpoint = format!("http://127.0.0.1:{}", peer.port);
        let scans = Arc::new(AtomicUsize::new(0));
        let scan_count = scans.clone();
        let hub = NativeHub::with_sources(
            DeviceStore::memory(),
            DiscoveryInput {
                state_file: fixture.state_file.clone(),
                home: fixture.home.clone(),
            },
            process_fact(),
            Some(Arc::new(|_| false)),
            Some(ExplicitEvidenceSources {
                scan: Arc::new(move |_| {
                    scan_count.fetch_add(1, Ordering::SeqCst);
                    vec![(
                        12345,
                        vec![
                            "opencode".into(),
                            "--server".into(),
                            endpoint.clone(),
                            "--session".into(),
                            SESSION_ID.into(),
                        ],
                    )]
                }),
                env: Arc::new(|_, _| Some(PASSWORD.into())),
                info: Arc::new(|url, _| {
                    Ok(json!({"pid": PID, "version": "2.0.22", "urls": [url.as_str()]}))
                }),
            }),
        );
        hub.view(&fixture.card).await.expect("建立显式服务快照");
        let mut watch = hub.watch(SESSION_ID);
        wait_for(|| peer.event_conns() > 0, "旧 pump 建连").await;
        let (old_source, old_pump) = {
            let state = hub.inner.state_lock();
            (
                state.source.as_ref().unwrap().clone(),
                state.pump.as_ref().unwrap().cancel.clone(),
            )
        };
        let hold = Arc::new(Hold::new());
        *hub.inner.pump_after_route_test_hold.lock().unwrap() = Some(hold.clone());
        peer.push_event();
        tokio::time::timeout(WAIT, hold.started.notified())
            .await
            .expect("旧 pump 必须通过身份检查并完成路由读取");

        let replacement = replace_same_runtime_client(&hub, &old_source).await;
        assert_eq!(
            replacement.descriptor.runtime,
            old_source.descriptor.runtime
        );
        assert!(*old_pump.borrow(), "替换 source 必须取消旧 pump");
        assert!(
            hub.inner.state_lock().pump.is_none(),
            "替换 source 必须摘除旧 pump"
        );
        let epoch = hub.inner.epoch();
        hub.pause();
        assert_eq!(
            hub.inner.epoch(),
            epoch + 1,
            "pause 必须在释放旧 pump 前完成"
        );
        {
            let state = hub.inner.state_lock();
            assert!(state.source.is_none() && state.pump.is_none() && state.sessions.is_empty());
        }
        watch.borrow_and_update();
        let scans_before = scans.load(Ordering::SeqCst);
        let log_start = peer.log().len();
        hold.release.notify_one();
        wait_for(|| peer.event_closed() > 0, "旧 pump 完成收尾").await;
        let requests: Vec<_> = peer
            .log()
            .into_iter()
            .skip(log_start)
            .map(|request| request.target)
            .collect();
        let state = hub.inner.state_lock();
        assert_eq!(
            (
                scans.load(Ordering::SeqCst) - scans_before,
                requests,
                state.source.is_none(),
                state.pump.is_none(),
                state.sessions.is_empty(),
                watch.has_changed().is_err()
            ),
            (0, Vec::<String>::new(), true, true, true, true),
            "旧 pump 跨 pause 后不得重新发现、读取、安装 source 或发布"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retired_pump_cannot_rediscover_source_after_pause_during_identity_check() {
        let fixture = fixture();
        let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
        let endpoint = format!("http://127.0.0.1:{}", peer.port);
        let armed = Arc::new(AtomicBool::new(false));
        let hold = Arc::new(ThreadHold::new());
        let info_calls = Arc::new(AtomicUsize::new(0));
        let scan_endpoint = endpoint.clone();
        let info_armed = armed.clone();
        let info_hold = hold.clone();
        let calls = info_calls.clone();
        let hub = NativeHub::with_sources(
            DeviceStore::memory(),
            DiscoveryInput {
                state_file: fixture.state_file.clone(),
                home: fixture.home.clone(),
            },
            process_fact(),
            Some(Arc::new(|_| false)),
            Some(ExplicitEvidenceSources {
                scan: Arc::new(move |_| {
                    vec![(
                        12345,
                        vec![
                            "opencode".into(),
                            "--server".into(),
                            scan_endpoint.clone(),
                            "--session".into(),
                            SESSION_ID.into(),
                        ],
                    )]
                }),
                env: Arc::new(|_, _| Some(PASSWORD.into())),
                info: Arc::new(move |url, _| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if info_armed.swap(false, Ordering::SeqCst) {
                        info_hold.pass();
                        return Err(NativeError::new(
                            codes::OC_TRANSPORT_FAILED,
                            "test identity failure",
                        ));
                    }
                    Ok(json!({"pid": PID, "version": "2.0.22", "urls": [url.as_str()]}))
                }),
            }),
        );
        hub.view(&fixture.card).await.expect("建立显式服务快照");
        let mut watch = hub.watch(SESSION_ID);
        wait_for(|| peer.event_conns() > 0, "旧 pump 建连").await;
        let old_source = hub.inner.state_lock().source.as_ref().unwrap().clone();
        armed.store(true, Ordering::SeqCst);
        peer.push_event();
        tokio::time::timeout(WAIT, hold.started.notified())
            .await
            .expect("旧 pump 必须停在显式身份发现");

        let replacement = replace_same_runtime_client(&hub, &old_source).await;
        assert!(
            hub.inner.state_lock().pump.is_none(),
            "换 source 必须摘除旧 pump"
        );
        assert!(!Arc::ptr_eq(&replacement.client, &old_source.client));
        let epoch = hub.inner.epoch();
        hub.pause();
        assert_eq!(hub.inner.epoch(), epoch + 1);
        watch.borrow_and_update();
        let log_start = peer.log().len();
        let calls_before = info_calls.load(Ordering::SeqCst);
        hold.release();
        wait_for(|| peer.event_closed() > 0, "旧 pump 完成收尾").await;
        let new_requests: Vec<_> = peer
            .log()
            .into_iter()
            .skip(log_start)
            .map(|request| request.target)
            .collect();
        let state = hub.inner.state_lock();
        assert_eq!(
            (
                state.source.is_none(),
                state.pump.is_none(),
                state.sessions.is_empty(),
                info_calls.load(Ordering::SeqCst) - calls_before,
                new_requests,
                watch.has_changed().is_err()
            ),
            (true, true, true, 0, Vec::<String>::new(), true),
            "旧 pump 跨 pause 后不得重新发现、读取、安装 source 或发布"
        );
    }

    #[tokio::test]
    async fn source_guard_old_pump_401_does_not_retire_replacement_client() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| script.pages = vec![vec![user_message("msg_initial", "A 初始", 1)]]);
        let hub = hub(&fixture, peer.port);
        hub.view(&fixture.card)
            .await
            .expect("建立旧 A client 和快照");
        let old_source = hub.inner.state_lock().source.as_ref().unwrap().clone();
        let hold = Arc::new(PumpRetireHold::default());
        *hub.inner.pump_retire_test_hold.lock().unwrap() = Some(hold.clone());
        peer.set(|script| script.event_status = 401);
        let _watch = hub.watch(SESSION_ID);
        hub.inner.ensure_pump();
        tokio::time::timeout(WAIT, hold.started.notified())
            .await
            .expect("旧 pump 的真实 401 必须到达收尾屏障");
        assert!(
            old_source.client.is_invalidated(),
            "旧 client 必须由真实 401 作废"
        );

        peer.set(|script| {
            script.event_status = 200;
            script.pages = vec![vec![user_message("msg_new", "A 新快照", 2)]];
        });
        hub.view(&fixture.card)
            .await
            .expect("同代际 A 重新发现并发布");
        let (new_source, new_pump, new_snapshot) = {
            let state = hub.inner.state_lock();
            let source = state.source.as_ref().unwrap().clone();
            assert_eq!(source.descriptor.runtime, old_source.descriptor.runtime);
            assert!(!Arc::ptr_eq(&source.client, &old_source.client));
            let pump = state.pump.as_ref().expect("新 pump 已登记").cancel.clone();
            let snapshot = state.sessions[SESSION_ID]
                .snapshot
                .as_ref()
                .unwrap()
                .clone();
            assert!(snapshot
                .messages
                .iter()
                .any(|message| message.content.contains("A 新快照")));
            (source, pump, snapshot)
        };
        tokio::time::timeout(WAIT, async {
            while peer.event_conns() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("新 pump 必须连接假服务");
        hold.release.notify_one();
        tokio::time::timeout(WAIT, hold.finished.notified())
            .await
            .expect("旧 pump 必须完成收尾");
        let state = hub.inner.state_lock();
        assert_eq!(
            (
                state
                    .source
                    .as_ref()
                    .is_some_and(|source| Arc::ptr_eq(source, &new_source)),
                !new_source.client.is_invalidated(),
                state
                    .pump
                    .as_ref()
                    .is_some_and(|pump| Arc::ptr_eq(&pump.cancel, &new_pump)),
                !*new_pump.borrow(),
                state.sessions[SESSION_ID]
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| Arc::ptr_eq(snapshot, &new_snapshot))
            ),
            (true, true, true, true, true),
            "旧 pump 的 401 收尾不得摘除同代际的新 client 或破坏新 pump 和快照"
        );
    }

    #[tokio::test]
    async fn source_guard_pinned_invalidated_client_recovers_same_generation() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        let hub = hub(&fixture, peer.port);
        let old = hub
            .inner
            .source_for_session(SESSION_ID)
            .await
            .expect("发现 A");
        let target = NativeTarget {
            runtime: old.descriptor.runtime.clone(),
            session_id: SESSION_ID.to_string(),
            directory: project,
        };
        peer.set(|script| script.info_status = 401);
        let error = old
            .client
            .read_json(INFO_PATH, &[])
            .await
            .expect_err("真实 401 作废旧 client");
        assert_eq!(error.code, codes::OC_UNAUTHORIZED);
        assert!(old.client.is_invalidated());
        assert!(hub
            .inner
            .state_lock()
            .source
            .as_ref()
            .is_some_and(|source| Arc::ptr_eq(source, &old)));
        peer.set(|script| script.info_status = 200);

        let recovered = hub
            .inner
            .source_for_session_pinned(SESSION_ID, Some(&target))
            .await;
        let (same_generation, new_client, valid, error) = match recovered {
            Ok(source) => (
                source.descriptor.runtime == target.runtime,
                !Arc::ptr_eq(&source.client, &old.client),
                !source.client.is_invalidated(),
                None,
            ),
            Err(error) => (false, false, false, Some(error.code)),
        };
        assert_eq!(
            (same_generation, new_client, valid, error),
            (true, true, true, None),
            "固定 A 的失效 client 应重新发现同代际并自愈"
        );
    }

    #[tokio::test]
    async fn source_guard_unpinned_late_openapi_cannot_replace_published_b() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer_a = Peer::start(Script::new(&project)).await;
        let peer_b = Peer::start(Script::new(&project)).await;
        peer_b.set(|script| {
            script.pid = PID + 1;
            script.pages = vec![vec![user_message("msg_b", "服务 B", 1)]];
        });
        let hold = Arc::new(Hold::new());
        peer_a.set(|script| script.hold_openapi = Some(hold.clone()));
        let hub = Arc::new(hub(&fixture, peer_a.port));
        let inner = hub.inner.clone();
        let task = tokio::spawn(async move { inner.source_for_session(SESSION_ID).await });
        tokio::time::timeout(WAIT, hold.started.notified())
            .await
            .expect("A 已发现并等待 OpenAPI");
        write_registration(&fixture.state_file, peer_b.port, PID + 1);
        hub.view(&fixture.card).await.expect("A 等待期间发布 B");
        let _watch = hub.watch(SESSION_ID);
        hub.inner.ensure_pump();
        let (source_b, pump_b, snapshot_b) = {
            let state = hub.inner.state_lock();
            let source = state.source.as_ref().unwrap().clone();
            assert_eq!(source.descriptor.runtime.pid, PID + 1);
            let pump = state.pump.as_ref().expect("B pump 已登记").cancel.clone();
            let snapshot = state.sessions[SESSION_ID]
                .snapshot
                .as_ref()
                .unwrap()
                .clone();
            assert!(snapshot
                .messages
                .iter()
                .any(|message| message.content.contains("服务 B")));
            (source, pump, snapshot)
        };
        hold.release.notify_one();
        let result = tokio::time::timeout(WAIT, task)
            .await
            .expect("A 必须结束")
            .expect("A 任务不得 panic");
        let error = result.err().map(|error| error.code);
        let rejected = error.as_deref().is_some_and(|code| {
            code == codes::OC_BINDING_STALE || code == codes::OC_GENERATION_MISMATCH
        });
        let state = hub.inner.state_lock();
        assert_eq!(
            (
                rejected,
                state
                    .source
                    .as_ref()
                    .is_some_and(|source| Arc::ptr_eq(source, &source_b)),
                !source_b.client.is_invalidated(),
                state
                    .pump
                    .as_ref()
                    .is_some_and(|pump| Arc::ptr_eq(&pump.cancel, &pump_b)),
                !*pump_b.borrow(),
                state.sessions[SESSION_ID]
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| Arc::ptr_eq(snapshot, &snapshot_b))
            ),
            (true, true, true, true, true, true),
            "无 pin 的迟到 A 不得覆盖 B；错误码 {error:?}"
        );
    }

    async fn pinned_cold_a_preserves_b_source(b_installed_first: bool) {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer_a = Peer::start(Script::new(&project)).await;
        let peer_b = Peer::start(Script::new(&project)).await;
        peer_b.set(|script| {
            script.pid = PID + 1;
            script.pages = vec![vec![user_message("msg_b", "服务 B", 1)]];
        });
        let hub = Arc::new(hub(&fixture, peer_a.port));
        let target_a = NativeTarget {
            runtime: RuntimeKey {
                service_id: SERVICE_ID.to_string(),
                pid: PID,
                started_at_ms: STARTED_AT_MS,
            },
            session_id: SESSION_ID.to_string(),
            directory: project,
        };
        let hold = Arc::new(Hold::new());
        if !b_installed_first {
            peer_a.set(|script| script.hold_openapi = Some(hold.clone()));
        }

        if b_installed_first {
            write_registration(&fixture.state_file, peer_b.port, PID + 1);
            hub.view(&fixture.card).await.expect("先发布 B 的快照");
            write_registration(&fixture.state_file, peer_a.port, PID);
        }
        let mut task = None;
        if !b_installed_first {
            let inner = hub.inner.clone();
            let target = target_a.clone();
            task = Some(tokio::spawn(async move {
                inner
                    .source_for_session_pinned(SESSION_ID, Some(&target))
                    .await
            }));
            tokio::time::timeout(WAIT, hold.started.notified())
                .await
                .expect("A 必须进入 OpenAPI 请求屏障");
            write_registration(&fixture.state_file, peer_b.port, PID + 1);
            hub.view(&fixture.card)
                .await
                .expect("A 等待期间发布 B 的快照");
        }
        let _watch = hub.watch(SESSION_ID);
        hub.inner.ensure_pump();
        let (source_b, pump_b, snapshot_b) = {
            let state = hub.inner.state_lock();
            let source = state.source.as_ref().unwrap().clone();
            assert_eq!(source.descriptor.runtime.pid, PID + 1);
            let pump = state.pump.as_ref().expect("B pump 已登记").cancel.clone();
            let snapshot = state.sessions[SESSION_ID]
                .snapshot
                .as_ref()
                .unwrap()
                .clone();
            assert!(snapshot
                .messages
                .iter()
                .any(|message| message.content.contains("服务 B")));
            (source, pump, snapshot)
        };
        let task = match task {
            Some(task) => {
                hold.release.notify_one();
                task
            }
            None => {
                let inner = hub.inner.clone();
                tokio::spawn(async move {
                    inner
                        .source_for_session_pinned(SESSION_ID, Some(&target_a))
                        .await
                })
            }
        };
        let result = tokio::time::timeout(WAIT, task)
            .await
            .expect("A 读取必须结束")
            .expect("A 任务不得 panic");
        let state = hub.inner.state_lock();
        let error = result.err().map(|error| error.code);
        let controlled = error.as_deref().is_some_and(|code| {
            code == codes::OC_BINDING_STALE || code == codes::OC_GENERATION_MISMATCH
        });
        assert_eq!(
            (
                controlled,
                state
                    .source
                    .as_ref()
                    .is_some_and(|source| Arc::ptr_eq(source, &source_b)),
                !source_b.client.is_invalidated(),
                state
                    .pump
                    .as_ref()
                    .is_some_and(|pump| Arc::ptr_eq(&pump.cancel, &pump_b)),
                !*pump_b.borrow(),
                state.sessions[SESSION_ID]
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| Arc::ptr_eq(snapshot, &snapshot_b))
            ),
            (true, true, true, true, true, true),
            "固定 A 不能覆盖 B 的 source、client、pump 或快照；错误码 {error:?}"
        );
    }

    #[tokio::test]
    async fn pinned_cold_a_cannot_replace_b_installed_during_openapi() {
        pinned_cold_a_preserves_b_source(false).await;
    }

    #[tokio::test]
    async fn pinned_cold_a_cannot_replace_existing_b_source() {
        pinned_cold_a_preserves_b_source(true).await;
    }

    #[tokio::test]
    async fn pinned_view_keeps_authorized_a_read_only_after_registration_changes_to_b() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer_a = Peer::start(Script::new(&project)).await;
        peer_a.set(|script| script.pages = vec![vec![user_message("msg_a", "A 原消息", 1)]]);
        let peer_b = Peer::start(Script::new(&project)).await;
        peer_b.set(|script| script.pid = PID + 1);
        write_registration(&fixture.state_file, peer_a.port, PID);
        let hub = NativeHub::with_sources(
            DeviceStore::memory(),
            DiscoveryInput {
                state_file: fixture.state_file.clone(),
                home: fixture.home.clone(),
            },
            process_fact(),
            Some(Arc::new(|_| false)),
            None,
        );
        hub.view(&fixture.card)
            .await
            .expect("先缓存 A 的 source 与快照");
        let (source_a, target_a) = {
            let state = hub.inner.state_lock();
            (
                state.source.as_ref().unwrap().clone(),
                state.sessions[SESSION_ID]
                    .snapshot
                    .as_ref()
                    .unwrap()
                    .target
                    .clone(),
            )
        };
        let route_before = hub.inner.store.with(|conn| {
            assert!(crate::database::dao::opencode_actions::claim_route_conn(
                conn, &target_a, true, 1, false
            )
            .unwrap());
            crate::database::dao::opencode_actions::route_conn(conn, SESSION_ID).unwrap()
        });
        let (baseline, baseline_claim) = hub
            .view_and_claim_pinned(&fixture.card, true, Some(&target_a))
            .await
            .expect("登记 A 时，既有固定授权必须可读");
        assert!(baseline_claim.owned);
        assert_eq!(
            baseline_claim.declined_reason,
            Some(codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED)
        );
        assert!(baseline.binding_required, "生产归属缺证时保持只读");
        peer_a.set(|script| script.pages = vec![vec![user_message("msg_a_new", "A 新消息", 2)]]);
        write_registration(&fixture.state_file, peer_b.port, PID + 1);

        let result = hub
            .view_and_claim_pinned(&fixture.card, true, Some(&target_a))
            .await;
        let source_preserved = hub
            .inner
            .state_lock()
            .source
            .as_ref()
            .is_some_and(|source| {
                Arc::ptr_eq(source, &source_a) && !source.client.is_invalidated()
            });
        let route_after = hub.inner.store.with(|conn| {
            crate::database::dao::opencode_actions::route_conn(conn, SESSION_ID).unwrap()
        });
        let b_requests: Vec<_> = peer_b
            .log()
            .into_iter()
            .map(|request| request.target)
            .collect();
        let (returned_a, new_message, read_only, owned, reason, error) = match result {
            Ok((view, claim)) => (
                view.service_pid == Some(PID),
                view.messages
                    .iter()
                    .any(|message| message.content.contains("A 新消息")),
                view.binding_required,
                claim.owned,
                claim.declined_reason,
                None,
            ),
            Err(error) => (false, false, false, false, None, Some(error.code)),
        };
        assert_eq!(
            (
                returned_a,
                new_message,
                read_only,
                owned,
                reason,
                source_preserved,
                route_after == route_before,
                b_requests,
                error
            ),
            (
                true,
                true,
                true,
                true,
                Some(codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED),
                true,
                true,
                Vec::<String>::new(),
                None
            ),
            "已授权且存活的 A 应继续可读；缺桌面归属证据时只读，不能切换或读取 B"
        );
    }

    async fn replaced_client_cannot_mix_snapshot_with_new_connection(form: bool) {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer_a = Peer::start(Script::new(&project)).await;
        let peer_b = Peer::start(Script::new(&project)).await;
        let hub = Arc::new(hub(&fixture, peer_a.port));
        hub.view(&fixture.card).await.unwrap();
        let target = hub.inner.state_lock().sessions[SESSION_ID]
            .snapshot
            .as_ref()
            .unwrap()
            .target
            .clone();
        let hold = Arc::new(Hold::new());
        peer_a.set(|script| script.hold_inbox = Some(hold.clone()));
        let task_hub = hub.clone();
        let card = fixture.card.clone();
        let task = tokio::spawn(async move {
            if form {
                task_hub
                    .form_result_pinned(&card, "frm-test", Some(&target))
                    .await
                    .map(|_| ())
            } else {
                task_hub
                    .agent_view_pinned(&card, Some(&target))
                    .await
                    .map(|_| ())
            }
        });
        tokio::time::timeout(WAIT, hold.started.notified())
            .await
            .expect("A 投影必须到达最后一次读取");
        {
            let mut state = hub.inner.state_lock();
            let old = state.source.as_ref().unwrap();
            let mut descriptor = old.descriptor.clone();
            descriptor.endpoint = format!("http://127.0.0.1:{}", peer_b.port).parse().unwrap();
            let replacement = Arc::new(Source {
                client: Arc::new(NativeClient::new(descriptor.clone()).unwrap()),
                descriptor,
                capabilities: old.capabilities.clone(),
            });
            assert_eq!(replacement.descriptor.runtime, old.descriptor.runtime);
            state.source = Some(replacement);
        }
        hold.release.notify_one();
        let result = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
        let business_reads: Vec<_> = peer_b
            .log()
            .into_iter()
            .filter(|r| r.path() != "/api/info")
            .map(|r| r.target)
            .collect();
        assert!(
            result.is_err() && business_reads.is_empty(),
            "旧投影不能读取新连接：result_ok={}, B 业务读={business_reads:?}",
            result.is_ok()
        );
    }

    #[tokio::test]
    async fn pinned_form_does_not_mix_snapshot_with_replacement_client() {
        replaced_client_cannot_mix_snapshot_with_new_connection(true).await;
    }

    #[tokio::test]
    async fn pinned_agent_does_not_mix_snapshot_with_replacement_client() {
        replaced_client_cannot_mix_snapshot_with_new_connection(false).await;
    }

    async fn replaced_client_rejects_late_pinned_read(form: bool) {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        let hub = Arc::new(hub(&fixture, peer.port));
        hub.view(&fixture.card).await.unwrap();
        let target = hub.inner.state_lock().sessions[SESSION_ID]
            .snapshot
            .as_ref()
            .unwrap()
            .target
            .clone();
        let hold = Arc::new(Hold::new());
        peer.set(|script| {
            if form {
                script.hold_detail = Some(hold.clone());
            } else {
                script.hold_agent = Some(hold.clone());
            }
        });
        let task_hub = hub.clone();
        let card = fixture.card.clone();
        let task = tokio::spawn(async move {
            if form {
                task_hub
                    .form_result_pinned(&card, "frm-test", Some(&target))
                    .await
                    .map(|_| ())
            } else {
                task_hub
                    .agent_view_pinned(&card, Some(&target))
                    .await
                    .map(|_| ())
            }
        });
        tokio::time::timeout(WAIT, hold.started.notified())
            .await
            .expect("读取必须到达屏障");
        {
            let mut state = hub.inner.state_lock();
            let old = state.source.as_ref().unwrap();
            let replacement = Arc::new(Source {
                descriptor: old.descriptor.clone(),
                client: Arc::new(NativeClient::new(old.descriptor.clone()).unwrap()),
                capabilities: old.capabilities.clone(),
            });
            assert!(!Arc::ptr_eq(&old.client, &replacement.client));
            state.source = Some(replacement);
        }
        hold.release.notify_one();
        let result = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
        assert!(
            result.is_err(),
            "同一运行代际更换 client 后不得返回旧连接的迟到结果"
        );
    }

    #[tokio::test]
    async fn pinned_form_rejects_late_result_after_same_runtime_client_replacement() {
        replaced_client_rejects_late_pinned_read(true).await;
    }

    #[tokio::test]
    async fn pinned_agent_rejects_late_result_after_same_runtime_client_replacement() {
        replaced_client_rejects_late_pinned_read(false).await;
    }

    #[tokio::test]
    async fn pinned_refresh_keeps_reading_cached_a_when_registration_changes_to_b() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer_a = Peer::start(Script::new(&project)).await;
        peer_a.set(|s| s.pages = vec![vec![user_message("msg_a", "服务 A", 1)]]);
        let peer_b = Peer::start(Script::new(&project)).await;
        peer_b.set(|s| s.pid = PID + 1);
        let hub = hub(&fixture, peer_a.port);
        hub.view(&fixture.card).await.expect("建立 A 快照");
        let _watch = hub.watch(SESSION_ID);
        hub.inner.ensure_pump();
        let (source_a, pump_a, snapshot_a, target_a) = {
            let state = hub.inner.state_lock();
            let source = state.source.as_ref().unwrap().clone();
            let pump = state.pump.as_ref().expect("A pump 已登记").cancel.clone();
            let snapshot = state.sessions[SESSION_ID]
                .snapshot
                .as_ref()
                .unwrap()
                .clone();
            let target = snapshot.target.clone();
            (source, pump, snapshot, target)
        };

        let a_reads_before = peer_a
            .log()
            .iter()
            .filter(|request| request.path() == "/api/info")
            .count();
        write_registration(&fixture.state_file, peer_b.port, PID + 1);
        let result = hub
            .inner
            .refresh_raw_pinned(&fixture.card, Some(&target_a))
            .await;
        let a_read_again = peer_a
            .log()
            .iter()
            .filter(|request| request.path() == "/api/info")
            .count()
            > a_reads_before;
        let (source_preserved, client_valid, pump_preserved, pump_active, snapshot_preserved) = {
            let state = hub.inner.state_lock();
            (
                state
                    .source
                    .as_ref()
                    .is_some_and(|source| Arc::ptr_eq(source, &source_a)),
                !source_a.client.is_invalidated(),
                state
                    .pump
                    .as_ref()
                    .is_some_and(|pump| Arc::ptr_eq(&pump.cancel, &pump_a)),
                !*pump_a.borrow(),
                state.sessions[SESSION_ID]
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| Arc::ptr_eq(snapshot, &snapshot_a)),
            )
        };
        let business_reads: Vec<_> = peer_b
            .log()
            .into_iter()
            .filter(|request| request.path() != OPENAPI_PATH)
            .map(|request| request.target)
            .collect();
        let (returned_a, a_message, error) = match result {
            Ok(raw) => (
                raw.target == target_a,
                raw.records.iter().any(|record| record["id"] == "msg_a"),
                None,
            ),
            Err(error) => (false, false, Some(error.code)),
        };
        assert_eq!(
            (
                returned_a,
                a_message,
                a_read_again,
                source_preserved,
                client_valid,
                pump_preserved,
                pump_active,
                snapshot_preserved,
                business_reads,
                error
            ),
            (
                true,
                true,
                true,
                true,
                true,
                true,
                true,
                true,
                Vec::<String>::new(),
                None
            ),
            "A 仍可验证和读取时，固定读取应继续使用 A，保留原状态且不读取 B 的业务数据"
        );
    }

    #[tokio::test]
    async fn pinned_refresh_rejects_invalid_a_generation_without_reading_b() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer_a = Peer::start(Script::new(&project)).await;
        let peer_b = Peer::start(Script::new(&project)).await;
        peer_b.set(|script| script.pid = PID + 1);
        write_registration(&fixture.state_file, peer_a.port, PID);
        let a_alive = Arc::new(AtomicBool::new(true));
        let facts_alive = a_alive.clone();
        let hub = NativeHub::with_coordinator_sources(
            DeviceStore::memory(),
            DiscoveryInput {
                state_file: fixture.state_file.clone(),
                home: fixture.home.clone(),
            },
            Arc::new(move |pid| {
                if pid == PID && !facts_alive.load(Ordering::SeqCst) {
                    None
                } else if pid == PID {
                    Some(STARTED_AT_MS)
                } else {
                    Some(STARTED_AT_MS + 60_000)
                }
            }),
        );
        hub.view(&fixture.card).await.expect("建立 A 快照");
        let (source_a, target_a) = {
            let state = hub.inner.state_lock();
            (
                state.source.as_ref().unwrap().clone(),
                state.sessions[SESSION_ID]
                    .snapshot
                    .as_ref()
                    .unwrap()
                    .target
                    .clone(),
            )
        };
        a_alive.store(false, Ordering::SeqCst);
        write_registration(&fixture.state_file, peer_b.port, PID + 1);
        let result = hub
            .inner
            .refresh_raw_pinned(&fixture.card, Some(&target_a))
            .await;
        let b_business_reads: Vec<_> = peer_b
            .log()
            .into_iter()
            .filter(|request| request.path() != OPENAPI_PATH)
            .map(|request| request.target)
            .collect();
        let b_installed = hub
            .inner
            .state_lock()
            .source
            .as_ref()
            .is_some_and(|source| !Arc::ptr_eq(source, &source_a));
        assert!(result.is_err(), "A 的进程代际已不可验证，固定读取必须拒绝");
        assert!(!b_installed, "不得把 B 安装为固定目标");
        assert!(
            b_business_reads.is_empty(),
            "不得向 B 发起业务读取: {b_business_reads:?}"
        );
    }

    #[tokio::test]
    async fn old_service_pump_rebuild_cannot_overwrite_new_service_publication() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let old_peer = Peer::start(Script::new(&project)).await;
        old_peer.set(|s| s.pages = vec![vec![user_message("msg_a", "服务 A", 1)]]);
        let new_peer = Peer::start(Script::new(&project)).await;
        new_peer.set(|s| {
            s.pid = PID + 1;
            s.pages = vec![vec![user_message("msg_b", "服务 B", 2)]];
        });
        let hub = Arc::new(hub(&fixture, old_peer.port));
        hub.view(&fixture.card).await.unwrap();
        let _watch = hub.watch(SESSION_ID);
        let hold = Arc::new(Hold::new());
        old_peer.set(|s| s.hold_inbox = Some(hold.clone()));
        let epoch = hub.inner.epoch();
        let inner = hub.inner.clone();
        let old = tokio::spawn(async move { inner.rebuild_watched(epoch).await });
        tokio::time::timeout(WAIT, hold.started.notified())
            .await
            .expect("旧 pump 必须到达最后一次读取");
        write_registration(&fixture.state_file, new_peer.port, PID + 1);
        let fresh = hub.view(&fixture.card).await.expect("B 发布视图");
        hold.release.notify_one();
        tokio::time::timeout(WAIT, old)
            .await
            .expect("旧 pump 必须结束")
            .unwrap();
        let state = hub.inner.state_lock();
        let entry = state.sessions.get(SESSION_ID).unwrap();
        let published = entry.snapshot.as_ref().unwrap();
        assert_eq!(
            published.binding_token, fresh.binding_token,
            "旧 pump 不得覆盖 B 的 token"
        );
        assert!(
            published.messages[0].content.contains("服务 B"),
            "旧 pump 不得覆盖 B 的视图"
        );
    }

    // 路由在重建开始前已失效时，也不能仅凭「重建前后路由相等」发布旧目标。
    async fn watched_rebuild_with_preexisting_route(route_kind: u8) {
        use crate::database::dao::opencode_actions::{
            claim_route_conn, disengage_route_conn, route_conn, ROUTE_ACTIVE, ROUTE_DRAINING,
        };

        let fixture = fixture();
        let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
        peer.set(|script| script.pages = vec![vec![user_message("msg_initial", "初始快照", 1)]]);
        let hub = hub(&fixture, peer.port);
        let initial = hub.view(&fixture.card).await.expect("建立 A 快照");
        let (source, target, snapshot, mut receiver) = {
            let state = hub.inner.state_lock();
            let entry = &state.sessions[SESSION_ID];
            let snapshot = entry.snapshot.as_ref().unwrap().clone();
            (
                state.source.as_ref().unwrap().clone(),
                snapshot.target.clone(),
                snapshot,
                entry.tx.subscribe(),
            )
        };
        assert_eq!(*receiver.borrow_and_update(), initial.revision);
        let expected_route = hub.inner.store.with(|conn| {
            assert!(route_conn(conn, SESSION_ID).unwrap().is_none());
            if route_kind != 0 {
                assert!(claim_route_conn(conn, &target, true, 1, false).unwrap());
                if route_kind == 1 {
                    assert!(disengage_route_conn(conn, SESSION_ID, 2).unwrap());
                } else {
                    let mut other = target.clone();
                    other.runtime.pid += 1;
                    other.runtime.started_at_ms += 60_000;
                    assert!(claim_route_conn(conn, &other, true, 2, false).unwrap());
                }
            }
            let route = route_conn(conn, SESSION_ID).unwrap();
            match route_kind {
                0 => assert!(route.is_none()),
                1 => {
                    let route = route.as_ref().unwrap();
                    assert_eq!(route.route_state, ROUTE_DRAINING);
                    assert!(route.same_runtime(&target));
                }
                _ => {
                    let route = route.as_ref().unwrap();
                    assert_eq!(route.route_state, ROUTE_ACTIVE);
                    assert!(!route.same_runtime(&target));
                }
            }
            route
        });
        peer.set(|script| script.pages = vec![vec![user_message("msg_new", "A 的新消息", 2)]]);
        tokio::time::timeout(WAIT, hub.inner.rebuild_watched(hub.inner.epoch()))
            .await
            .expect("重建必须结束");

        let (revision, snapshot_unchanged, source_unchanged, new_message) = {
            let state = hub.inner.state_lock();
            let entry = &state.sessions[SESSION_ID];
            let current = entry.snapshot.as_ref().unwrap();
            (
                entry.revision,
                Arc::ptr_eq(current, &snapshot),
                state
                    .source
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &source)),
                current
                    .messages
                    .iter()
                    .any(|message| message.content.contains("A 的新消息")),
            )
        };
        let route_unchanged = hub
            .inner
            .store
            .with(|conn| route_conn(conn, SESSION_ID).unwrap() == expected_route);
        let watch_changed = receiver.has_changed().expect("watch 必须保持打开");
        if route_kind == 0 {
            assert_eq!(
                (
                    revision,
                    snapshot_unchanged,
                    source_unchanged,
                    route_unchanged,
                    watch_changed,
                    new_message
                ),
                (initial.revision + 1, false, true, true, true, true),
                "未认领的只读会话仍可重建并发布新消息"
            );
        } else {
            assert_eq!(
                (
                    revision,
                    snapshot_unchanged,
                    source_unchanged,
                    route_unchanged,
                    watch_changed,
                    new_message
                ),
                (initial.revision, true, true, true, false, false),
                "重建开始前路由已失效，不得发布缓存 A 或推进修订"
            );
        }
    }

    #[tokio::test]
    async fn watched_rebuild_rejects_preexisting_draining_route() {
        watched_rebuild_with_preexisting_route(1).await;
    }

    #[tokio::test]
    async fn watched_rebuild_rejects_preexisting_different_target() {
        watched_rebuild_with_preexisting_route(2).await;
    }

    #[tokio::test]
    async fn watched_rebuild_updates_unclaimed_read_only_session() {
        watched_rebuild_with_preexisting_route(0).await;
    }

    async fn watched_rebuild_rejects_changed_route(draining: bool) {
        use crate::database::dao::opencode_actions::{
            claim_route_conn, disengage_route_conn, route_conn, ROUTE_ACTIVE, ROUTE_DRAINING,
        };

        let fixture = fixture();
        let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
        peer.set(|script| script.pages = vec![vec![user_message("msg_initial", "初始快照", 1)]]);
        let hub = hub(&fixture, peer.port);
        let initial = hub.view(&fixture.card).await.expect("建立 A 快照");
        let (source, target, initial_snapshot) = {
            let state = hub.inner.state_lock();
            let snapshot = state.sessions[SESSION_ID]
                .snapshot
                .as_ref()
                .unwrap()
                .clone();
            (
                state.source.as_ref().unwrap().clone(),
                snapshot.target.clone(),
                snapshot,
            )
        };
        hub.inner.store.with(|conn| {
            assert!(claim_route_conn(conn, &target, true, 1, false).unwrap());
        });
        let mut receiver = hub.watch(SESSION_ID);
        assert_eq!(*receiver.borrow_and_update(), initial.revision);
        peer.set(|script| script.pages = vec![vec![user_message("msg_late", "迟到的 A", 2)]]);
        let hold = Arc::new(Hold::new());
        peer.set(|script| script.hold_inbox = Some(hold.clone()));
        let epoch = hub.inner.epoch();
        let inner = hub.inner.clone();
        let rebuild = tokio::spawn(async move { inner.rebuild_watched(epoch).await });
        tokio::time::timeout(WAIT, hold.started.notified())
            .await
            .expect("固定 A 的重建必须停在最后一次 HTTP 读取");

        let changed_route = hub.inner.store.with(|conn| {
            let before = route_conn(conn, SESSION_ID).unwrap().unwrap();
            assert_eq!(before.route_state, ROUTE_ACTIVE);
            assert!(before.same_runtime(&target));
            if draining {
                assert!(disengage_route_conn(conn, SESSION_ID, 2).unwrap());
            } else {
                let mut other = target.clone();
                other.runtime.pid += 1;
                other.runtime.started_at_ms += 60_000;
                assert!(claim_route_conn(conn, &other, true, 2, false).unwrap());
            }
            let after = route_conn(conn, SESSION_ID).unwrap().unwrap();
            assert_ne!(before, after);
            if draining {
                assert_eq!(after.route_state, ROUTE_DRAINING);
            } else {
                assert_eq!(after.route_state, ROUTE_ACTIVE);
                assert!(!after.same_runtime(&target));
            }
            after
        });
        assert!(
            hub.inner
                .state_lock()
                .source
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &source)),
            "source 必须保持 A"
        );
        hold.release.notify_one();
        tokio::time::timeout(WAIT, rebuild)
            .await
            .expect("重建必须结束")
            .unwrap();

        let (revision, snapshot_unchanged, source_unchanged) = {
            let state = hub.inner.state_lock();
            let entry = &state.sessions[SESSION_ID];
            (
                entry.revision,
                entry
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| Arc::ptr_eq(snapshot, &initial_snapshot)),
                state
                    .source
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &source)),
            )
        };
        let route_preserved = hub
            .inner
            .store
            .with(|conn| route_conn(conn, SESSION_ID).unwrap() == Some(changed_route));
        let watch_changed = receiver.has_changed().expect("watch 必须保持打开");
        assert_eq!(
            (
                revision,
                snapshot_unchanged,
                source_unchanged,
                route_preserved,
                watch_changed
            ),
            (initial.revision, true, true, true, false),
            "路由改变后，固定 A 的旧重建不得发布快照或推进修订"
        );
    }

    #[tokio::test]
    async fn watched_rebuild_does_not_publish_after_route_becomes_draining() {
        watched_rebuild_rejects_changed_route(true).await;
    }

    #[tokio::test]
    async fn watched_rebuild_does_not_publish_after_route_changes_target() {
        watched_rebuild_rejects_changed_route(false).await;
    }

    /// F6：pending 列表项不携带权威状态。真实列表形态（无 `state`）与「把 detail 才有的
    /// `state.status` 塞进列表」两种，`PendingView::Form.state` 都必须是 `"unknown"`：
    /// 本层不拷贝、也不解释列表里未承诺的状态字段（权威状态由任务 7 派发前读 detail）。
    #[tokio::test]
    async fn form_list_items_carry_no_authoritative_state() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            let real = pending_form(json!([{ "key": "q0", "title": "COLOR", "type": "string" }]));
            assert!(
                real.get("state").is_none(),
                "列表真实形态必须没有 state（g4.scope.json）"
            );
            let mut detail_shaped = real.clone();
            detail_shaped["id"] = json!("frm_detail_shaped");
            detail_shaped["state"] = json!({ "status": "answered" });
            script.forms = vec![real, detail_shaped];
        });
        let hub = hub(&fixture, peer.port);

        let view = hub.view(&fixture.card).await.expect("投影");
        assert_eq!(view.pending.len(), 2);
        for item in &view.pending {
            match item {
                PendingView::Form { id, state, .. } => {
                    assert_eq!(
                        state, "unknown",
                        "列表读不携带权威状态（{id}）：unknown 既不是 pending 也不是 answered"
                    );
                }
                other => panic!("期望表单分支，实际 {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn t11_final_source_await_rechecks_epoch_after_concurrent_rebind() {
        let fixture = fixture();
        let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
        write_registration(&fixture.state_file, peer.port, PID);
        let armed = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(tokio::sync::Notify::new());
        let (release, wait) = std::sync::mpsc::channel();
        let wait = Mutex::new(wait);
        let facts = process_fact();
        let gated_facts = {
            let armed = Arc::clone(&armed);
            let entered = Arc::clone(&entered);
            Arc::new(move |pid| {
                if armed.swap(false, Ordering::SeqCst) {
                    entered.notify_one();
                    wait.lock()
                        .unwrap()
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                }
                facts(pid)
            })
        };
        let hub = Arc::new(NativeHub::with_coordinator_sources(
            DeviceStore::memory(),
            DiscoveryInput {
                state_file: fixture.state_file.clone(),
                home: fixture.home.clone(),
            },
            gated_facts,
        ));
        let before = hub.inner.source_for_session(SESSION_ID).await.unwrap();
        let epoch = hub.inner.epoch();
        armed.store(true, Ordering::SeqCst);
        let reading = Arc::clone(&hub);
        let pending =
            tokio::spawn(async move { reading.inner.source_for_epoch(epoch, SESSION_ID).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        // source 已进入发现（调用者 epoch 尚有效），但尚未取得缓存源。
        // pause 后同服务重建；旧 source 调用恢复时会命中新监听的同 runtime 缓存。
        hub.pause();
        let after = hub.inner.source_for_session(SESSION_ID).await.unwrap();
        assert_eq!(before.descriptor.runtime, after.descriptor.runtime);
        release.send(()).unwrap();
        let result = pending.await.unwrap();
        assert!(
            result.is_err(),
            "await 前检查不足：最终 source 等待期间已换监听代际"
        );
        assert!(hub
            .inner
            .source_for_epoch(hub.inner.epoch(), SESSION_ID)
            .await
            .is_ok());
        assert_eq!(peer.non_get(), 0);
    }

    #[tokio::test]
    async fn t11_final_source_check_rejects_old_epoch_even_with_same_runtime() {
        let fixture = fixture();
        let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
        let hub = hub(&fixture, peer.port);
        let before = hub.inner.source_for_session(SESSION_ID).await.unwrap();
        let epoch = hub.inner.epoch();
        hub.pause();
        // 同密码、同服务 id/PID 的新监听仍属于新 epoch，不能接受旧结果。
        let after = hub.inner.source_for_session(SESSION_ID).await.unwrap();
        assert_eq!(before.descriptor.runtime, after.descriptor.runtime);
        let result = hub.inner.source_for_epoch(epoch, SESSION_ID).await;
        assert!(result.is_err(), "最后一次 source 读取不能掩盖监听代际变化");
        assert!(hub
            .inner
            .source_for_epoch(hub.inner.epoch(), SESSION_ID)
            .await
            .is_ok());
        assert_eq!(peer.shared.counters.non_get.load(Ordering::SeqCst), 0);
    }

    /// F1：401 作废之后必须真的换一条新代际连接。修复前 `install` 会把刚作废的同代际源
    /// 再还回去，于是每次都「读一次 /openapi.json 然后立即 OC_UNAUTHORIZED」，永不自愈。
    #[tokio::test]
    async fn invalidated_source_is_rediscovered_on_next_view() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| script.pages = vec![vec![user_message("msg_a", "a", 1)]]);
        let hub = hub(&fixture, peer.port);
        hub.view(&fixture.card).await.expect("首轮投影");

        // 真实 401：客户端据此作废本代际连接（不是伪造 is_invalidated）。
        peer.set(|script| script.info_status = 401);
        let err = hub.view(&fixture.card).await.expect_err("凭据已失效");
        assert_eq!(err.code, codes::OC_UNAUTHORIZED);

        // 服务恢复：必须用**新** client 自愈，而不是复用已作废的那个。
        peer.set(|script| script.info_status = 200);
        let recovered = hub.view(&fixture.card).await.expect("401 之后必须能自愈");
        assert_eq!(recovered.messages.len(), 1);
        assert_eq!(
            peer.log()
                .iter()
                .filter(|request| request.path() == "/openapi.json")
                .count(),
            2,
            "每个 client 各读一次契约：第二次读必须被真正用上"
        );
    }

    /// F2：并发 `watch`/`view` 不能各起一条上游。修复前「检查 + spawn + 写回」分三段加锁，
    /// 并发调用者都能看到空槽而双双 spawn（后写者覆盖前者，前者成为无人能停的孤儿流）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_watchers_and_views_share_one_upstream_stream() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| script.pages = vec![vec![user_message("msg_a", "a", 1)]]);
        let hub = Arc::new(hub(&fixture, peer.port));

        let mut watchers = Vec::new();
        for _ in 0..4 {
            let hub = Arc::clone(&hub);
            watchers.push(tokio::spawn(async move { hub.watch(SESSION_ID) }));
        }
        let mut views = Vec::new();
        for _ in 0..4 {
            let hub = Arc::clone(&hub);
            let card = fixture.card.clone();
            views.push(tokio::spawn(async move { hub.view(&card).await }));
        }
        // 首批不等完就再来一轮订阅，尽量把「占位前」的窗口踩满。
        for _ in 0..2 {
            let hub = Arc::clone(&hub);
            watchers.push(tokio::spawn(async move { hub.watch(SESSION_ID) }));
        }

        let mut receivers = Vec::new();
        for task in watchers {
            receivers.push(task.await.expect("watch 任务不得 panic"));
        }
        for task in views {
            let view = task
                .await
                .expect("view 任务不得 panic")
                .expect("并发投影成功");
            assert!(view.revision > 0);
        }
        wait_for(|| peer.event_conns() >= 1, "事件流建连").await;
        // 给潜在的重复 spawn 一点落地时间，然后必须恒为一条。
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(peer.event_conns(), 1, "并发订阅/投影只能产生一条上游");
        drop(receivers);
    }

    /// F3：释放与重新订阅相撞时不能丢上游。修复前「判定空闲」与「摘除」之间隔着
    /// cancel + drain，新订阅会看到在册 pump 直接返回，随后旧 pump 摘除 → 上游已死且不自愈。
    #[tokio::test]
    async fn release_and_resubscribe_keeps_upstream_alive() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| script.pages = vec![vec![user_message("msg_a", "a", 1)]]);
        let hub = hub(&fixture, peer.port);

        for round in 0..3 {
            let receiver = hub.watch(SESSION_ID);
            hub.view(&fixture.card).await.expect("投影");
            wait_for(|| peer.event_conns() > peer.event_closed(), "事件流建连").await;
            drop(receiver);
            // 释放完成 = 已建立的流全部关闭（此后不应再有新的连接）。
            wait_for(
                || peer.event_closed() == peer.event_conns(),
                "无订阅后释放上游",
            )
            .await;

            // 释放完成的下一秒就重新订阅：必须拉起一条新的活流（不是「在册但已死」）。
            let closed = peer.event_closed();
            let mut receiver = hub.watch(SESSION_ID);
            hub.view(&fixture.card).await.expect("重新订阅后投影");
            wait_for(|| peer.event_conns() == closed + 1, "重新订阅后重建上游").await;
            let _ = receiver.borrow_and_update();
            peer.push_event();
            tokio::time::timeout(WAIT, receiver.changed())
                .await
                .unwrap_or_else(|_| panic!("第 {round} 轮：重新订阅后收不到事件（上游已丢）"))
                .expect("watch 未关闭");
            // 下一轮从「已重新订阅」的状态继续：先确认这轮流是否还活着。
            assert!(peer.event_conns() > peer.event_closed());
        }
    }

    #[tokio::test]
    async fn sse_only_reconnect_after_idle_reap_receives_updates() {
        let fixture = fixture();
        let peer = Peer::start(Script::new(&fixture.project.canonicalize().unwrap())).await;
        peer.set(|s| s.pages = vec![vec![user_message("initial", "初始", 1)]]);
        let hub = hub(&fixture, peer.port);
        let receiver = hub.watch(SESSION_ID);
        hub.view(&fixture.card).await.expect("初始投影");
        wait_for(|| peer.event_conns() > peer.event_closed(), "初始上游建连").await;
        drop(receiver);
        wait_for(|| peer.event_closed() == peer.event_conns(), "空闲回收上游").await;
        let closed = peer.event_closed();
        let mut receiver = hub.watch(SESSION_ID);
        receiver.borrow_and_update();
        wait_for(|| peer.event_conns() == closed + 1, "仅重连 SSE").await;
        wait_for(
            || peer.shared.events.receiver_count() == 1,
            "假服务事件订阅就绪",
        )
        .await;
        peer.set(|s| s.pages = vec![vec![user_message("updated", "后续更新", 2)]]);
        peer.push_event();
        tokio::time::timeout(WAIT, receiver.changed())
            .await
            .expect("仅重连 SSE 后必须收到更新")
            .expect("订阅保持打开");
        let state = hub.inner.state_lock();
        assert!(state.sessions[SESSION_ID]
            .snapshot
            .as_ref()
            .unwrap()
            .messages[0]
            .content
            .contains("后续更新"));
    }

    /// F4：分页中途的 401 是「凭据代际消失」，不是「这一页读不到」：必须上抛，
    /// 不能被吞成部分历史（那会让调用方以为只是历史不全）。
    #[tokio::test]
    async fn mid_pagination_unauthorized_is_not_swallowed() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.pages = vec![
                vec![user_message("msg_a", "a", 1)],
                vec![user_message("msg_b", "b", 2)],
            ];
            script.page_unauthorized = Some(1);
        });
        let hub = hub(&fixture, peer.port);

        let err = hub
            .view(&fixture.card)
            .await
            .expect_err("分页中途 401 必须上抛");
        assert_eq!(err.code, codes::OC_UNAUTHORIZED);
        assert!(
            !hub.delivery_evidence(SESSION_ID),
            "失败的投影不得留下投递证据"
        );
    }

    /// F5：契约关掉某个只读能力只关掉那一段（返回空），不得连坐整份视图。
    /// 任务 1 已证：delivery 出现第三种词 ⇒ `ReadInbox=false`。
    #[tokio::test]
    async fn disabled_capability_only_disables_that_section() {
        let fixture = fixture();
        let project = fixture.project.canonicalize().unwrap();
        let peer = Peer::start(Script::new(&project)).await;
        peer.set(|script| {
            script.openapi = openapi_with_foreign_delivery_word();
            script.pages = vec![vec![user_message("msg_a", "a", 1)]];
            script.forms = vec![pending_form(json!([
                { "key": "q0", "title": "COLOR", "type": "string" }
            ]))];
            // 这个 inbox 项用的词正是被关掉的第三种词：读它一定会失败。
            script.inbox = vec![json!({
                "id": "msg_broadcast",
                "payload": { "text": "广播" },
                "delivery": "broadcast",
            })];
        });
        let hub = hub(&fixture, peer.port);

        let view = hub
            .view(&fixture.card)
            .await
            .expect("单段能力关闭不得连坐整份视图");
        assert!(
            !view.capabilities[&NativeOperationId::ReadInbox].allowed,
            "第三种 delivery 词必须关闭 ReadInbox"
        );
        assert!(view.inbox.is_empty(), "被关掉的那一段返回空");
        assert_eq!(view.messages.len(), 1, "消息段照常");
        assert_eq!(view.pending.len(), 1, "pending 段照常");
        assert!(
            !peer
                .log()
                .iter()
                .any(|request| request.path() == format!("/api/session/{SESSION_ID}/inbox")),
            "能力关闭时根本不该去读那一段"
        );
    }

    /// F9：shutdown 之后订阅必须立即拿到关闭信号，而不是「活着但永远静默」。
    #[tokio::test]
    async fn watch_after_shutdown_is_closed() {
        let fixture = fixture();
        let hub = hub(&fixture, 1);
        hub.shutdown();

        let mut receiver = hub.watch(SESSION_ID);
        assert_eq!(*receiver.borrow(), 0);
        assert!(
            receiver.changed().await.is_err(),
            "shutdown 后不得给出活着但永不推进的频道"
        );
    }

    /// F7：在飞计数用 RAII，panic 也必须归还，否则释放逻辑永远等不到「空闲」。
    #[test]
    fn view_guard_releases_counter_on_panic() {
        let counter = AtomicUsize::new(0);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ViewGuard::new(&counter);
            assert_eq!(counter.load(Ordering::SeqCst), 1);
            panic!("视图构建中途 panic");
        }));
        std::panic::set_hook(previous);

        assert!(outcome.is_err(), "用例本身必须真的 panic 过");
        assert_eq!(counter.load(Ordering::SeqCst), 0, "panic 后计数必须归零");
    }

    /// F7：状态锁中毒不能让 hub 永久不可用（读锁用 `into_inner` 自愈）。
    #[test]
    fn poisoned_state_lock_does_not_brick_the_hub() {
        let fixture = fixture();
        let hub = hub(&fixture, 1);

        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = hub.inner.state.lock().unwrap();
            panic!("毒化状态锁");
        }));
        std::panic::set_hook(previous);
        assert!(outcome.is_err());
        assert!(
            hub.inner.state.lock().is_err(),
            "锁必须已被毒化（否则本用例什么都没测）"
        );

        // 自愈：读路径不得 panic，也不得因此变成「永远释放不了」。
        let receiver = hub.watch(SESSION_ID);
        assert_eq!(*receiver.borrow(), 0);
        assert!(!hub.delivery_evidence(SESSION_ID));
    }

    /// 真实验收缺陷：每次读都退回墙钟时，同一进程的启动锚会随读取时刻漂移，
    /// `RuntimeKey`/binding token 随之轮换，紧接的写操作恒 409。
    #[test]
    fn production_process_fact_is_stable_across_reads() {
        let pid = std::process::id();
        let first = production_process_fact(pid).expect("当前进程的启动时间必须可读");
        std::thread::sleep(Duration::from_millis(150));
        let second = production_process_fact(pid).expect("当前进程的启动时间必须可读");

        assert_eq!(first, second, "同一进程两次读取的启动时间必须相同");
        assert!(first > 0, "启动时间必须是正的 epoch 毫秒");
        assert_eq!(first % 1000, 0, "启动锚必须只来自整秒的进程 start_time");
    }

    /// 纯换算：同一整秒恒得同一毫秒锚；0/不可转换/乘 1000 溢出都必须 fail-closed。
    #[test]
    fn started_at_ms_from_started_secs_is_deterministic_and_fails_closed() {
        const STARTED_SECS: u64 = 1_793_289_345;
        let first = started_at_ms_from_started_secs(STARTED_SECS);
        let second = started_at_ms_from_started_secs(STARTED_SECS);

        assert_eq!(first, Some(1_793_289_345_000));
        assert_eq!(first, second, "同一整秒必须恒得同一毫秒锚");
        assert_eq!(started_at_ms_from_started_secs(0), None, "0 不可用必须拒绝");
        assert_eq!(
            started_at_ms_from_started_secs(u64::MAX),
            None,
            "无法转成 i64 的秒数必须拒绝"
        );
        // 能转成 i64、但 ×1000 会溢出：专门命中 checked_mul 的 None 分支。
        let overflowing_secs = (i64::MAX / 1000 + 1) as u64;
        assert!(
            i64::try_from(overflowing_secs).is_ok(),
            "该输入必须能通过 i64 转换，才能测到乘法溢出"
        );
        assert_eq!(started_at_ms_from_started_secs(overflowing_secs), None);
    }

    /// 同一进程的两次**真实**启动事实派生出的 `RuntimeKey` 与 binding token 必须逐字相等。
    /// 旧实现把墙钟毫秒掺进启动锚：两次事实相差 150+ ms，token 随之轮换，紧随其后的写
    /// 就会 409 `OC_BINDING_STALE`——本用例在旧实现上因此是红的，不是拿常量自证。
    #[test]
    fn same_process_facts_yield_same_runtime_key_and_binding_token() {
        let fixture = fixture();
        let directory = fixture.project.canonicalize().expect("归一项目目录");
        let pid = std::process::id();
        let target_for = |started_at_ms: i64| NativeTarget {
            runtime: RuntimeKey {
                service_id: SERVICE_ID.to_string(),
                pid,
                started_at_ms,
            },
            session_id: SESSION_ID.to_string(),
            directory: directory.clone(),
        };

        let first_started = production_process_fact(pid).expect("当前进程的启动时间必须可读");
        std::thread::sleep(Duration::from_millis(150));
        let second_started = production_process_fact(pid).expect("当前进程的启动时间必须可读");
        let first = target_for(first_started);
        let second = target_for(second_started);

        let hub = NativeHub::with_coordinator_sources(
            DeviceStore::memory(),
            DiscoveryInput {
                state_file: fixture.state_file.clone(),
                home: fixture.home.clone(),
            },
            Arc::new(production_process_fact),
        );
        let first_token = hub.inner.binding_token(&first);
        let second_token = hub.inner.binding_token(&second);
        assert_eq!(
            first_token, second_token,
            "同一进程两次真实事实的 binding token 必须稳定"
        );
        assert_eq!(first, second, "同一进程两次真实事实的 RuntimeKey 必须相等");
    }
    #[tokio::test]
    async fn bridge_can_restart_after_stop_in_the_same_process() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let state_file = temp.path().join("service.json");
        std::fs::write(
            &state_file,
            serde_json::json!({
                "id": "svc-restart",
                "version": "2.2.5",
                "url": "http://127.0.0.1:1",
                "pid": std::process::id(),
                "password": "pw",
            })
            .to_string(),
        )
        .unwrap();
        let input = DiscoveryInput {
            state_file,
            home: temp.path().to_path_buf(),
        };
        let hub = NativeHub::with_sources(
            DeviceStore::memory(),
            input.clone(),
            Arc::new(|_pid| Some(12_000)),
            None,
            None,
        );
        let paths = BridgePaths::in_dir(temp.path().join("bridge"));
        let fact: Arc<dyn Fn(u32) -> Option<i64> + Send + Sync> = Arc::new(|_pid| Some(12_000));
        hub.start_bridge_with(paths.clone(), input.clone(), Arc::clone(&fact))
            .await
            .unwrap();
        assert!(hub.bridge_status().await.is_some());
        hub.stop_bridge().await;
        assert!(hub.bridge_status().await.is_none());
        hub.start_bridge_with(paths, input, fact).await.unwrap();
        assert!(hub.bridge_status().await.is_some());
        hub.stop_bridge().await;
    }

    // ==== N1 固定窗口绑定（pinned_windows）判据 ====

    /// N1 单元判据专用：TUI 窗口身份常量（与服务代际 PID 无关，TUI 是另一个进程）。
    const TUI_PID: u32 = 61001;
    const TUI_STARTED_AT_MS: i64 = 1_793_290_000_000;
    const TUI_BOOT: &str = "boot-n1-lib-fixture";
    /// N1 第二窗口（「两个窗口都打开 A」判据用）。
    const TUI_ALT_PID: u32 = 61100;
    const TUI_ALT_STARTED_AT_MS: i64 = 1_793_290_050_000;

    /// 装门 + 可变「当前新鲜 Opened」观察的 hub（与集成夹具同形的单元版；
    /// 控制文件根 = 临时 home，真实 ~/.tuvis 零读写）。
    fn pinned_window_hub(
        fixture: &Fixture,
        port: u16,
        opened: Arc<std::sync::Mutex<Option<(String, u32, i64)>>>,
        tui_alive: Arc<AtomicBool>,
        locked_generation: Arc<AtomicI64>,
        binary_sha: crate::services::opencode_native::attribution_evidence::TuiBinarySha,
        service_binary_sha: crate::services::opencode_native::attribution_evidence::ServiceBinarySha,
    ) -> NativeHub {
        write_registration(&fixture.state_file, port, PID);
        let mut hub = NativeHub::with_sources(
            DeviceStore::memory(),
            DiscoveryInput {
                state_file: fixture.state_file.clone(),
                home: fixture.home.clone(),
            },
            process_fact(),
            Some(Arc::new(|_dir: &Path| false)),
            None,
        );
        let observation = Arc::clone(&opened);
        let fact_observation = Arc::clone(&opened);
        let alive = Arc::clone(&tui_alive);
        let generation = Arc::clone(&locked_generation);
        hub.arm_binding_gate_for_tests(
            // 与生产 `fresh_opened_window` 同形：只认当前新鲜 Opened 的会话。
            Arc::new(move |sid: &str| {
                let guard = observation.lock().unwrap();
                guard
                    .as_ref()
                    .filter(|(open_sid, _, _)| open_sid == sid)
                    .map(
                        |(_, pid, started)| crate::services::opencode_native::LockedWindow {
                            pid: *pid,
                            started_at_ms: *started,
                            boot: Some(TUI_BOOT.to_string()),
                        },
                    )
            }),
            Arc::new(move |pid: u32| {
                if !alive.load(Ordering::SeqCst) {
                    return None;
                }
                if pid == TUI_PID {
                    return Some(generation.load(Ordering::SeqCst));
                }
                fact_observation
                    .lock()
                    .unwrap()
                    .as_ref()
                    .filter(|(_, open_pid, _)| *open_pid == pid)
                    .map(|(_, _, started)| *started)
            }),
            binary_sha,
            service_binary_sha,
            600,
            10,
            fixture.home.clone(),
        );
        hub
    }

    /// N1 单元判据的目标（与登记/进程事实同代际）。
    fn pinned_target(fixture: &Fixture) -> NativeTarget {
        NativeTarget {
            runtime: RuntimeKey {
                service_id: SERVICE_ID.to_string(),
                pid: PID,
                started_at_ms: STARTED_AT_MS,
            },
            session_id: SESSION_ID.to_string(),
            directory: fixture.project.canonicalize().unwrap(),
        }
    }

    /// 绑定后窗口导航 / 观察消失：写授权锁定窗口仍返回**选择时绑定**的身份；
    /// 其他会话不消费该绑定（回退生产新鲜查询）。
    #[test]
    fn pinned_window_binding_survives_navigation_and_observation_loss() {
        let fixture = fixture();
        let opened = Arc::new(std::sync::Mutex::new(Some((
            SESSION_ID.to_string(),
            TUI_PID,
            TUI_STARTED_AT_MS,
        ))));
        let hub = pinned_window_hub(
            &fixture,
            59001,
            Arc::clone(&opened),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicI64::new(TUI_STARTED_AT_MS)),
            Arc::new(|_pid: u32| None),
            Arc::new(|_pid: u32| None),
        );
        let target = pinned_target(&fixture);
        hub.inner.bind_pinned_window(&target);
        // 前置：路由 active 且同代际（绑定只在既有归属上适用）。
        hub.inner
            .store
            .with(|conn| {
                crate::database::dao::opencode_actions::claim_route_conn(
                    conn, &target, true, 1_000, true,
                )
            })
            .unwrap();
        // 模拟同一窗口浏览到别的会话（观察不再有 Opened(A)）。
        *opened.lock().unwrap() = None;
        let lookup = hub.inner.locked_window_for_write(&target);
        let bound = lookup(SESSION_ID).expect("已绑定目标必须用绑定窗口身份");
        assert_eq!(bound.pid, TUI_PID);
        assert_eq!(bound.started_at_ms, TUI_STARTED_AT_MS);
        assert_eq!(bound.boot.as_deref(), Some(TUI_BOOT));
        // 其他会话不消费该绑定（回退生产查询 → 当前无观察 → None）。
        assert!(lookup("ses_other").is_none(), "绑定不外溢到别的会话");
    }

    /// 绑定不适用面：另一扇窗口打开同一会话时绑定窗口优先（新鲜查询指向别窗也不换）；
    /// 路由交回（非 active）→ 回退生产查询（拿到的是**新鲜窗口**，不是旧绑定）；
    /// 路由 active 且绑定与写目标不全等 → fail-closed（F12 项 1.5：后台派发面不得
    /// 经新鲜查询自行换窗）——fail-closed 方向恒不放宽。
    #[test]
    fn pinned_window_binding_falls_back_when_route_or_target_changed() {
        let fixture = fixture();
        let opened = Arc::new(std::sync::Mutex::new(Some((
            SESSION_ID.to_string(),
            TUI_PID,
            TUI_STARTED_AT_MS,
        ))));
        let hub = pinned_window_hub(
            &fixture,
            59002,
            Arc::clone(&opened),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicI64::new(TUI_STARTED_AT_MS)),
            Arc::new(|_pid: u32| None),
            Arc::new(|_pid: u32| None),
        );
        let target = pinned_target(&fixture);
        hub.inner.bind_pinned_window(&target);
        // 前置：路由 active 且同代际 → 绑定适用。
        hub.inner
            .store
            .with(|conn| {
                crate::database::dao::opencode_actions::claim_route_conn(
                    conn, &target, true, 1_000, true,
                )
            })
            .unwrap();
        // 另一扇窗口（更大 pid）打开同一会话：新鲜查询指向它，写授权仍用绑定窗口。
        *opened.lock().unwrap() =
            Some((SESSION_ID.to_string(), TUI_ALT_PID, TUI_ALT_STARTED_AT_MS));
        let lookup = hub.inner.locked_window_for_write(&target);
        assert_eq!(
            lookup(SESSION_ID).map(|window| window.pid),
            Some(TUI_PID),
            "绑定适用时不随新鲜查询换窗"
        );
        // 交回（draining）→ 绑定不再适用：回退生产查询，拿到的是**新鲜窗口**（别窗）。
        hub.inner
            .store
            .with(|conn| {
                crate::database::dao::opencode_actions::disengage_route_conn(
                    conn, SESSION_ID, 2_000,
                )
            })
            .unwrap();
        let lookup = hub.inner.locked_window_for_write(&target);
        assert_eq!(
            lookup(SESSION_ID).map(|window| window.pid),
            Some(TUI_ALT_PID),
            "路由交回后旧绑定不再适用（回退生产查询，返回当前新鲜窗口）"
        );
        // F12 项 1.5：重新认领（路由 active）后，绑定目标与写目标不全等（服务代际变了）
        // ⇒ **fail-closed**：本会话无锁定窗口，绝不经新鲜查询自行换窗（后台派发面）。
        hub.inner
            .store
            .with(|conn| {
                crate::database::dao::opencode_actions::claim_route_conn(
                    conn, &target, true, 3_000, true,
                )
            })
            .unwrap();
        let regenerated = NativeTarget {
            runtime: RuntimeKey {
                started_at_ms: STARTED_AT_MS + 120_000,
                ..target.runtime.clone()
            },
            ..target.clone()
        };
        let lookup = hub.inner.locked_window_for_write(&regenerated);
        assert!(
            lookup(SESSION_ID).is_none(),
            "路由 active + 绑定与写目标不全等（服务代际变了）⇒ fail-closed（不得回退新鲜查询换窗）"
        );
        // 其他会话不受影响（仍走生产新鲜查询：无观察 → None）。
        assert!(lookup("ses_other").is_none());
    }

    /// 选择时无新鲜观察：捕获是空操作（不写注册表），写授权回退生产查询——
    /// 候选/绑定身份本身无写权，缺失不放宽也不虚设。
    #[test]
    fn bind_pinned_window_without_fresh_observation_is_a_no_op() {
        let fixture = fixture();
        let opened = Arc::new(std::sync::Mutex::new(None));
        let hub = pinned_window_hub(
            &fixture,
            59003,
            opened,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicI64::new(TUI_STARTED_AT_MS)),
            Arc::new(|_pid: u32| None),
            Arc::new(|_pid: u32| None),
        );
        let target = pinned_target(&fixture);
        hub.inner.bind_pinned_window(&target);
        assert!(
            hub.inner.pinned_windows.lock().unwrap().is_empty(),
            "捕获失败不得写注册表"
        );
        let lookup = hub.inner.locked_window_for_write(&target);
        assert!(
            lookup(SESSION_ID).is_none(),
            "回退生产查询（当前无观察 → None）"
        );
    }

    // ==== M1 重启后的固定绑定（伴表持久化 + 注册表 miss 恢复）====

    /// M1 单元判据：选择事件把绑定身份写入伴表；注册表清空（模拟 Tuvis 重启）后
    /// `locked_window_for_write` 从伴表恢复同一窗口身份——新鲜查询指向别窗也不换。
    #[test]
    fn pinned_window_binding_persists_and_restores_after_registry_clear() {
        let fixture = fixture();
        let opened = Arc::new(std::sync::Mutex::new(Some((
            SESSION_ID.to_string(),
            TUI_PID,
            TUI_STARTED_AT_MS,
        ))));
        let hub = pinned_window_hub(
            &fixture,
            59004,
            Arc::clone(&opened),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicI64::new(TUI_STARTED_AT_MS)),
            Arc::new(|_pid: u32| None),
            Arc::new(|_pid: u32| None),
        );
        let target = pinned_target(&fixture);
        hub.inner
            .store
            .with(|conn| {
                crate::database::dao::opencode_actions::claim_route_conn(
                    conn, &target, true, 1_000, true,
                )
            })
            .unwrap();
        hub.inner.bind_pinned_window(&target);
        // 前置：选择事件同步落伴表（持久化面）。
        let row = hub
            .inner
            .store
            .with(|conn| {
                crate::database::dao::opencode_pinned_windows::pinned_window_conn(conn, SESSION_ID)
            })
            .unwrap()
            .expect("选择事件必须把绑定身份持久化到伴表");
        assert_eq!(row.window_pid, i64::from(TUI_PID));
        assert_eq!(row.window_started_at_ms, TUI_STARTED_AT_MS);
        assert_eq!(row.window_boot.as_deref(), Some(TUI_BOOT));
        assert_eq!(row.runtime_pid, i64::from(target.runtime.pid));
        // 模拟 Tuvis 重启：注册表清空，伴表保留。
        hub.inner.pinned_windows.lock().unwrap().clear();
        // 新鲜查询切到另一扇窗口：恢复的绑定仍锁定选择时的窗口（不随回退换窗）。
        *opened.lock().unwrap() =
            Some((SESSION_ID.to_string(), TUI_ALT_PID, TUI_ALT_STARTED_AT_MS));
        let lookup = hub.inner.locked_window_for_write(&target);
        let bound = lookup(SESSION_ID).expect("重启后必须从伴表恢复绑定身份");
        assert_eq!(
            bound.pid, TUI_PID,
            "恢复的身份 = 选择时绑定的窗口，不是新鲜查询的别窗"
        );
        assert_eq!(bound.started_at_ms, TUI_STARTED_AT_MS);
        assert_eq!(bound.boot.as_deref(), Some(TUI_BOOT));
        // 恢复的绑定不外溢到其他会话（回退生产查询 → 无观察 → None）。
        assert!(lookup("ses_other").is_none());
    }

    /// M1 单元判据：恢复的绑定与内存绑定走同一套适用判据——路由交回（非 active）
    /// 时回退生产查询；路由 active 但绑定与写目标不全等 / 伴表无行时 **fail-closed**
    ///（F12 项 1.5：派发面不得经新鲜查询自行换窗，要求显式重选）。
    #[test]
    fn pinned_window_restore_follows_same_applicability_rules_as_registry() {
        let fixture = fixture();
        let opened = Arc::new(std::sync::Mutex::new(Some((
            SESSION_ID.to_string(),
            TUI_ALT_PID,
            TUI_ALT_STARTED_AT_MS,
        ))));
        let hub = pinned_window_hub(
            &fixture,
            59005,
            opened,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicI64::new(TUI_STARTED_AT_MS)),
            Arc::new(|_pid: u32| None),
            Arc::new(|_pid: u32| None),
        );
        let target = pinned_target(&fixture);
        hub.inner
            .store
            .with(|conn| {
                crate::database::dao::opencode_actions::claim_route_conn(
                    conn, &target, true, 1_000, true,
                )
            })
            .unwrap();
        hub.inner.bind_pinned_window(&target);
        hub.inner.pinned_windows.lock().unwrap().clear();
        // 交回（draining）→ 恢复的绑定不再适用：回退生产查询（新鲜窗口 = 别窗）。
        hub.inner
            .store
            .with(|conn| {
                crate::database::dao::opencode_actions::disengage_route_conn(
                    conn, SESSION_ID, 2_000,
                )
            })
            .unwrap();
        let lookup = hub.inner.locked_window_for_write(&target);
        assert_eq!(
            lookup(SESSION_ID).map(|window| window.pid),
            Some(TUI_ALT_PID),
            "路由交回后恢复绑定不适用（回退生产查询）"
        );
        // F12 项 1.5：重新认领（路由 active）后，伴表里的旧目标与新写目标不全等
        // ⇒ fail-closed（不得回退新鲜查询换窗）。
        hub.inner
            .store
            .with(|conn| {
                crate::database::dao::opencode_actions::claim_route_conn(
                    conn, &target, true, 3_000, true,
                )
            })
            .unwrap();
        let regenerated = NativeTarget {
            runtime: RuntimeKey {
                started_at_ms: STARTED_AT_MS + 120_000,
                ..target.runtime.clone()
            },
            ..target.clone()
        };
        let lookup = hub.inner.locked_window_for_write(&regenerated);
        assert!(
            lookup(SESSION_ID).is_none(),
            "路由 active + 伴表目标与写目标不全等 ⇒ fail-closed（不得回退新鲜查询换窗）"
        );
        // F12 项 1.5：伴表无行（删行模拟「未绑定过 / 旧路由行时代」）且路由 active
        // ⇒ 同样 fail-closed——后台派发不得把另一扇窗当成授权（要求显式重选）。
        hub.inner
            .store
            .with(|conn| {
                conn.execute(
                    "DELETE FROM opencode_native_pinned_windows WHERE session_id = ?1",
                    [SESSION_ID],
                )
            })
            .unwrap();
        let lookup = hub.inner.locked_window_for_write(&target);
        assert!(
            lookup(SESSION_ID).is_none(),
            "路由 active + 无持久绑定 ⇒ fail-closed（不得经新鲜查询自行换窗）"
        );
        // 其他会话不受影响（无路由行 → 走生产新鲜查询：无观察 → None）。
        assert!(lookup("ses_other").is_none());
    }

    /// M1 单元判据：捕获失败（选择时刻无新鲜观察）不写伴表——与内存注册表的
    /// no-op 语义逐字一致；重启后同样无绑定可恢复（回退生产查询）。
    #[test]
    fn pinned_window_capture_failure_persists_nothing() {
        let fixture = fixture();
        let hub = pinned_window_hub(
            &fixture,
            59006,
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicI64::new(TUI_STARTED_AT_MS)),
            Arc::new(|_pid: u32| None),
            Arc::new(|_pid: u32| None),
        );
        let target = pinned_target(&fixture);
        hub.inner
            .store
            .with(|conn| {
                crate::database::dao::opencode_actions::claim_route_conn(
                    conn, &target, true, 1_000, true,
                )
            })
            .unwrap();
        hub.inner.bind_pinned_window(&target);
        let row = hub
            .inner
            .store
            .with(|conn| {
                crate::database::dao::opencode_pinned_windows::ensure_table_conn(conn)?;
                crate::database::dao::opencode_pinned_windows::pinned_window_conn(conn, SESSION_ID)
            })
            .unwrap();
        assert!(row.is_none(), "捕获失败不得写伴表");
        hub.inner.pinned_windows.lock().unwrap().clear();
        let lookup = hub.inner.locked_window_for_write(&target);
        assert!(
            lookup(SESSION_ID).is_none(),
            "路由 active + 无持久绑定 ⇒ fail-closed（F12 项 1.5：不回退新鲜查询；当前也无观察 → None）"
        );
    }

    // ==== N2 语义证据准入（attribution_evidence · 写权链第一道）====
    //
    // 准入判定 = 平台 × 启动形态 × 锚定 TUI 构建 SHA 三元组对登记表精确匹配；
    // 全链路（前置 → 准入 → 门内判据）的次序由集成用例考，本组只考准入函数本体：
    // 未登记构建 / 缺 TUI 身份 / 解析失败三面 fail-closed + 每代际缓存语义。

    /// N2 准入单测的登记组合（H02 锚定构建）与未登记构建（同版本不同构建）。
    const N2_UNPROVEN_SHA: &str =
        "abe4d88594bffb12b364375b8a6732d2d1f7a977195ed1a35c5c76f8e89c9e0d";

    /// 准入单测用服务描述符（显式形态用例传 `explicit = true`）。
    fn admit_descriptor(explicit: bool) -> ServiceDescriptor {
        ServiceDescriptor {
            runtime: RuntimeKey {
                service_id: SERVICE_ID.to_string(),
                pid: PID,
                started_at_ms: STARTED_AT_MS,
            },
            endpoint: reqwest::Url::parse("http://127.0.0.1:59010").expect("endpoint"),
            password: PASSWORD.to_string(),
            version: "2.0.22".to_string(),
            explicit,
        }
    }

    /// 装门 hub + 可注入的观察与 SHA 缝（N2/M2 准入单测专用薄封装）。
    /// 服务构建缝缺省 = 登记构建（既有 TUI 维度用例的世界逐字保留）。
    fn admission_hub(
        fixture: &Fixture,
        opened: Arc<Mutex<Option<(String, u32, i64)>>>,
        sha: crate::services::opencode_native::attribution_evidence::TuiBinarySha,
        service_sha: crate::services::opencode_native::attribution_evidence::ServiceBinarySha,
    ) -> NativeHub {
        pinned_window_hub(
            fixture,
            59010,
            opened,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicI64::new(TUI_STARTED_AT_MS)),
            sha,
            service_sha,
        )
    }

    /// 服务构建缝缺省：任意 pid 返回登记构建（与集成夹具同形）。
    fn registered_service_sha(
    ) -> crate::services::opencode_native::attribution_evidence::ServiceBinarySha {
        Arc::new(|_pid: u32| {
            Some(
                crate::services::opencode_native::attribution_evidence::H02_ANCHOR_SHA256
                    .to_string(),
            )
        })
    }

    fn opened_window(pid: u32, started_at_ms: i64) -> Arc<Mutex<Option<(String, u32, i64)>>> {
        Arc::new(Mutex::new(Some((
            SESSION_ID.to_string(),
            pid,
            started_at_ms,
        ))))
    }

    /// 计数 + 可变返回值的 SHA 桩：`None` 返回值 = 解析失败形态；字符串即时可换
    ///（模拟同 pid 代际换二进制 / 解析恢复）。
    fn counting_sha_knob() -> (
        crate::services::opencode_native::attribution_evidence::TuiBinarySha,
        Arc<AtomicUsize>,
        Arc<Mutex<String>>,
    ) {
        let calls = Arc::new(AtomicUsize::new(0));
        let value = Arc::new(Mutex::new(String::new()));
        let calls_for_closure = Arc::clone(&calls);
        let value_for_closure = Arc::clone(&value);
        let knob: crate::services::opencode_native::attribution_evidence::TuiBinarySha =
            Arc::new(move |_pid: u32| {
                calls_for_closure.fetch_add(1, Ordering::SeqCst);
                let current = value_for_closure.lock().unwrap().clone();
                (!current.is_empty()).then_some(current)
            });
        (knob, calls, value)
    }

    #[test]
    fn admission_rejects_unregistered_build() {
        let fixture = fixture();
        let hub = admission_hub(
            &fixture,
            opened_window(TUI_PID, TUI_STARTED_AT_MS),
            Arc::new(|_pid: u32| Some(N2_UNPROVEN_SHA.to_string())),
            registered_service_sha(),
        );
        let error = hub
            .inner
            .admit_semantic_evidence(&pinned_target(&fixture), &admit_descriptor(false))
            .expect_err("未登记构建必须拒绝");
        assert_eq!(error.code, codes::OC_GATE_SEMANTICS_UNPROVEN);
    }

    #[test]
    fn admission_rejects_missing_tui_identity() {
        let fixture = fixture();
        // 登记构建也救不了「缺 TUI 身份」：无新鲜锁定窗口 = 锚定进程不可证（fail-closed）。
        let hub = admission_hub(
            &fixture,
            Arc::new(Mutex::new(None)),
            Arc::new(|_pid: u32| {
                Some(
                    crate::services::opencode_native::attribution_evidence::H02_ANCHOR_SHA256
                        .to_string(),
                )
            }),
            registered_service_sha(),
        );
        let error = hub
            .inner
            .admit_semantic_evidence(&pinned_target(&fixture), &admit_descriptor(false))
            .expect_err("缺 TUI 身份必须按不适用拒绝");
        assert_eq!(error.code, codes::OC_GATE_SEMANTICS_UNPROVEN);
    }

    #[test]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "requires macOS OpenCode ownership or process semantics"
    )]
    fn admission_accepts_registered_combination_and_caches_per_generation() {
        let fixture = fixture();
        let (knob, calls, value) = counting_sha_knob();
        *value.lock().unwrap() =
            crate::services::opencode_native::attribution_evidence::H02_ANCHOR_SHA256.to_string();
        let hub = admission_hub(
            &fixture,
            opened_window(TUI_PID, TUI_STARTED_AT_MS),
            knob,
            registered_service_sha(),
        );
        let target = pinned_target(&fixture);
        let descriptor = admit_descriptor(false);
        hub.inner
            .admit_semantic_evidence(&target, &descriptor)
            .expect("登记组合必须准入");
        hub.inner
            .admit_semantic_evidence(&target, &descriptor)
            .expect("第二次判定命中缓存仍准入");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "同代际只解析一次构建 SHA（每代际一次，缓存命中零重算）"
        );
    }

    #[test]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "requires macOS OpenCode ownership or process semantics"
    )]
    fn admission_generation_swap_recomputes_and_refuses_stale_verdicts() {
        let fixture = fixture();
        let (knob, calls, value) = counting_sha_knob();
        *value.lock().unwrap() =
            crate::services::opencode_native::attribution_evidence::H02_ANCHOR_SHA256.to_string();
        let opened = opened_window(TUI_PID, TUI_STARTED_AT_MS);
        let hub = admission_hub(
            &fixture,
            Arc::clone(&opened),
            knob,
            registered_service_sha(),
        );
        let target = pinned_target(&fixture);
        let descriptor = admit_descriptor(false);
        hub.inner
            .admit_semantic_evidence(&target, &descriptor)
            .expect("代际 1（登记构建）准入");
        // 同 pid 新代际（升级模拟）：观察换代 → 缓存键换代 → 必须重算，不得复用旧代际判定。
        *value.lock().unwrap() = N2_UNPROVEN_SHA.to_string();
        *opened.lock().unwrap() =
            Some((SESSION_ID.to_string(), TUI_PID, TUI_STARTED_AT_MS + 60_000));
        let error = hub
            .inner
            .admit_semantic_evidence(&target, &descriptor)
            .expect_err("新代际换未登记构建必须重新拒绝");
        assert_eq!(error.code, codes::OC_GATE_SEMANTICS_UNPROVEN);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "新代际缓存键未命中 → 重算构建 SHA"
        );
    }

    #[test]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "requires macOS OpenCode ownership or process semantics"
    )]
    fn admission_resolution_failure_is_fail_closed_and_not_cached() {
        let fixture = fixture();
        let (knob, calls, value) = counting_sha_knob();
        // 初值空串 → 桩返回 None = SHA 获取失败（错误形态）。
        let opened = opened_window(TUI_PID, TUI_STARTED_AT_MS);
        let hub = admission_hub(
            &fixture,
            Arc::clone(&opened),
            knob,
            registered_service_sha(),
        );
        let target = pinned_target(&fixture);
        let descriptor = admit_descriptor(false);
        let error = hub
            .inner
            .admit_semantic_evidence(&target, &descriptor)
            .expect_err("SHA 获取失败必须按不适用拒绝");
        assert_eq!(error.code, codes::OC_GATE_SEMANTICS_UNPROVEN);
        // 失败不缓存：解析恢复（值就位）后同代际立即重算 → 准入。
        *value.lock().unwrap() =
            crate::services::opencode_native::attribution_evidence::H02_ANCHOR_SHA256.to_string();
        hub.inner
            .admit_semantic_evidence(&target, &descriptor)
            .expect("解析恢复后同代际重算并准入");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "失败值不入缓存");
    }

    #[test]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "requires macOS OpenCode ownership or process semantics"
    )]
    fn admission_covers_explicit_launch_form_of_the_registered_entry() {
        let fixture = fixture();
        let hub = admission_hub(
            &fixture,
            opened_window(TUI_PID, TUI_STARTED_AT_MS),
            Arc::new(|_pid: u32| {
                Some(
                    crate::services::opencode_native::attribution_evidence::H02_ANCHOR_SHA256
                        .to_string(),
                )
            }),
            registered_service_sha(),
        );
        hub.inner
            .admit_semantic_evidence(&pinned_target(&fixture), &admit_descriptor(true))
            .expect("登记条目覆盖显式启动形态（H02 E6）→ 准入");
    }

    // ==== M2 语义证据准入补全：服务构建维度 ====
    //
    // 归属语义由 TUI 与服务共同表现（复审「准入只验了一半」）：登记组合 =
    //（TUI SHA, 服务 SHA, 平台, 形态），服务构建未登记/取不到同样 fail-closed。

    /// M2「只换服务构建」：TUI 是登记锚、服务构建未登记 → 准入拒绝（集成侧
    /// `gate_semantics_service_build_swap_rejects_before_probe` 考全链三零）。
    #[test]
    fn admission_rejects_unregistered_service_build() {
        let fixture = fixture();
        let hub = admission_hub(
            &fixture,
            opened_window(TUI_PID, TUI_STARTED_AT_MS),
            Arc::new(|_pid: u32| {
                Some(
                    crate::services::opencode_native::attribution_evidence::H02_ANCHOR_SHA256
                        .to_string(),
                )
            }),
            Arc::new(|_pid: u32| Some(N2_UNPROVEN_SHA.to_string())),
        );
        let error = hub
            .inner
            .admit_semantic_evidence(&pinned_target(&fixture), &admit_descriptor(false))
            .expect_err("服务构建未登记的组合必须拒绝");
        assert_eq!(error.code, codes::OC_GATE_SEMANTICS_UNPROVEN);
    }

    /// M2「服务 SHA 解析失败」：取不到服务构建身份 → 按不适用拒绝；失败不入缓存
    ///（解析恢复后同代际立即重算并准入），惯例与 TUI SHA 缓存逐字同源。
    #[test]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "requires macOS OpenCode ownership or process semantics"
    )]
    fn admission_service_sha_resolution_failure_is_fail_closed_and_not_cached() {
        let fixture = fixture();
        let (knob, calls, value) = counting_sha_knob();
        let hub = admission_hub(
            &fixture,
            opened_window(TUI_PID, TUI_STARTED_AT_MS),
            Arc::new(|_pid: u32| {
                Some(
                    crate::services::opencode_native::attribution_evidence::H02_ANCHOR_SHA256
                        .to_string(),
                )
            }),
            knob,
        );
        let target = pinned_target(&fixture);
        let descriptor = admit_descriptor(false);
        let error = hub
            .inner
            .admit_semantic_evidence(&target, &descriptor)
            .expect_err("服务 SHA 获取失败必须按不适用拒绝");
        assert_eq!(error.code, codes::OC_GATE_SEMANTICS_UNPROVEN);
        // 失败不缓存：解析恢复（值就位）后同代际立即重算 → 准入。
        *value.lock().unwrap() =
            crate::services::opencode_native::attribution_evidence::H02_ANCHOR_SHA256.to_string();
        hub.inner
            .admit_semantic_evidence(&target, &descriptor)
            .expect("解析恢复后同代际重算并准入");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "失败值不入缓存");
    }

    /// M2 服务 SHA 每代际缓存：服务描述符（RuntimeKey）同代际只解析一次；服务
    /// 换代（pid 同、启动毫秒换）必须重算，不得复用旧代际判定。
    #[test]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "requires macOS OpenCode ownership or process semantics"
    )]
    fn admission_service_sha_is_cached_per_service_generation() {
        let fixture = fixture();
        let (knob, calls, value) = counting_sha_knob();
        *value.lock().unwrap() =
            crate::services::opencode_native::attribution_evidence::H02_ANCHOR_SHA256.to_string();
        let hub = admission_hub(
            &fixture,
            opened_window(TUI_PID, TUI_STARTED_AT_MS),
            Arc::new(|_pid: u32| {
                Some(
                    crate::services::opencode_native::attribution_evidence::H02_ANCHOR_SHA256
                        .to_string(),
                )
            }),
            knob,
        );
        let target = pinned_target(&fixture);
        let descriptor = admit_descriptor(false);
        hub.inner
            .admit_semantic_evidence(&target, &descriptor)
            .expect("服务代际 1（登记构建）准入");
        hub.inner
            .admit_semantic_evidence(&target, &descriptor)
            .expect("第二次判定命中缓存仍准入");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "服务同代际只解析一次构建 SHA（每代际一次，缓存命中零重算）"
        );
        // 服务换代（同 pid 新启动毫秒）：缓存键换代 → 必须重算。
        let regenerated = ServiceDescriptor {
            runtime: RuntimeKey {
                started_at_ms: STARTED_AT_MS + 120_000,
                ..descriptor.runtime.clone()
            },
            ..descriptor.clone()
        };
        hub.inner
            .admit_semantic_evidence(&target, &regenerated)
            .expect("服务换代后同锚构建重新准入");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "服务换代缓存键未命中 → 重算服务构建 SHA"
        );
    }

    // ==== H2：生产适配层死亡判定深化（双源正向死亡证据 + 源级注入缝） ====

    use crate::services::opencode_native::actions::ProcessFactProbe as H2Probe;

    /// 源观测表驱动的小工具：`(自身观测, 目标观测)` → 源闭包（自身 pid 按真实
    /// `std::process::id()` 匹配，其余 pid 一律按目标观测回答）。
    fn h2_source(
        self_sighting: FactSighting,
        target_sighting: FactSighting,
    ) -> impl Fn(u32) -> FactSighting {
        let myself = std::process::id();
        move |pid: u32| {
            if pid == myself {
                self_sighting
            } else {
                target_sighting
            }
        }
    }

    /// H2 RED d：自身读取成功 + 目标读取失败（单 pid 读不出）的组合，在生产探针
    /// 判定路径（源级注入缝）下也必须归 `ReadFailure`。目标读不出时，无论自身
    /// 多健康、登记多可读，死亡都不可证明（能读取自身进程不能证明他者已退出）。
    #[test]
    fn h2_self_read_success_with_target_read_failure_is_read_failure() {
        let sysinfo = h2_source(
            FactSighting::Present(STARTED_AT_MS),
            FactSighting::Unavailable,
        );
        // 交叉机制无论怎么回答都不得翻转判定：不可用 / 缺席 / 在场都一样。
        for cross_target in [
            FactSighting::Unavailable,
            FactSighting::Absent,
            FactSighting::PresentUnprovenAge,
        ] {
            let cross = h2_source(FactSighting::PresentUnprovenAge, cross_target);
            assert_eq!(
                production_generation_fact_with(PID, &sysinfo, Some(&cross)),
                H2Probe::ReadFailure,
                "目标读取失败 ⇒ 死亡不可证明（cross 观测 = {cross_target:?} 也不翻转）"
            );
        }
    }

    /// H2 行为红（机制收紧本体）：sysinfo 单源报告「目标缺席 + 自身在场的」旧形态
    /// 在双源模式下不再构成死亡证明——交叉机制不可用（给不出可区分回答）时，单源
    /// 缺席降级为 `ReadFailure`（旧生产探针在此直接报 `NoSuchProcess`）。
    #[test]
    fn h2_single_source_absence_without_crosscheck_confirmation_is_read_failure() {
        let sysinfo = h2_source(FactSighting::Present(STARTED_AT_MS), FactSighting::Absent);
        let cross_down = h2_source(FactSighting::PresentUnprovenAge, FactSighting::Unavailable);
        assert_eq!(
            production_generation_fact_with(PID, &sysinfo, Some(&cross_down)),
            H2Probe::ReadFailure,
            "交叉机制不可用：单源缺席只是「读不到」，不是死亡证明"
        );
    }

    /// 双源矛盾不可判死：交叉机制正面报告目标在场（存在性已被对方证明，如
    /// libproc EPERM = 在场但路径不可读）→ 与 sysinfo 缺席矛盾，且身份（lstart）
    /// 不可证 ⇒ `ReadFailure`（在场但启动时刻不可证形态），绝不升级为死亡。
    #[test]
    fn h2_contradictory_cross_presence_is_read_failure() {
        let sysinfo = h2_source(FactSighting::Present(STARTED_AT_MS), FactSighting::Absent);
        let cross_sees_target = h2_source(
            FactSighting::PresentUnprovenAge,
            FactSighting::PresentUnprovenAge,
        );
        assert_eq!(
            production_generation_fact_with(PID, &sysinfo, Some(&cross_sees_target)),
            H2Probe::ReadFailure,
            "交叉机制证明在场 ⇒ 与缺席矛盾且 lstart 不可证：ReadFailure"
        );
    }

    /// 交叉机制报缺席但其自身健康探针读不出 → 交叉机制健康不可证 → `ReadFailure`
    ///（机制自身不可用的「缺席」回答不可信）。
    #[test]
    fn h2_cross_absent_with_unhealthy_cross_probe_is_read_failure() {
        let sysinfo = h2_source(FactSighting::Present(STARTED_AT_MS), FactSighting::Absent);
        let cross_self_down = h2_source(FactSighting::Unavailable, FactSighting::Absent);
        assert_eq!(
            production_generation_fact_with(PID, &sysinfo, Some(&cross_self_down)),
            H2Probe::ReadFailure,
            "交叉机制自身读不出 ⇒ 其缺席回答不可信"
        );
    }

    /// sysinfo 自身健康探针读不出（同一机制对必真的自身 pid 都给不出在场回答）
    /// → 机制级失败 ⇒ 即便交叉机制报缺席也 `ReadFailure`。
    #[test]
    fn h2_sysinfo_probe_unhealthy_blocks_death_proof() {
        let sysinfo_self_down = h2_source(FactSighting::Unavailable, FactSighting::Absent);
        let cross = h2_source(FactSighting::PresentUnprovenAge, FactSighting::Absent);
        assert_eq!(
            production_generation_fact_with(PID, &sysinfo_self_down, Some(&cross)),
            H2Probe::ReadFailure,
            "sysinfo 机制健康不可证 ⇒ 缺席判据失效"
        );
    }

    /// 正向死亡证据可达（收紧不锁死）：双源目标缺席 + 双源自身健康探针在场
    /// ⇒ `NoSuchProcess`（四个证据齐备才判死）。
    #[test]
    fn h2_dual_source_agreement_proves_death() {
        let sysinfo = h2_source(FactSighting::Present(STARTED_AT_MS), FactSighting::Absent);
        let cross = h2_source(FactSighting::PresentUnprovenAge, FactSighting::Absent);
        assert_eq!(
            production_generation_fact_with(PID, &sysinfo, Some(&cross)),
            H2Probe::NoSuchProcess,
            "双源一致缺席 + 双机制健康 = 正向死亡证据"
        );
    }

    /// G 轮单源形态守卫（非 macOS 生产保持既有语义）：crosscheck=None 时
    /// 「目标缺席 + 自身在场 ⇒ NoSuchProcess；自身也读不出 ⇒ ReadFailure」。
    #[test]
    fn h2_single_source_form_is_preserved_without_crosscheck() {
        let sysinfo_healthy = h2_source(FactSighting::Present(STARTED_AT_MS), FactSighting::Absent);
        assert_eq!(
            production_generation_fact_with(PID, &sysinfo_healthy, None),
            H2Probe::NoSuchProcess,
            "单源形态：健康探针在场 + 目标缺席 = 既有 NoSuchProcess 语义"
        );
        let sysinfo_down = h2_source(FactSighting::Unavailable, FactSighting::Absent);
        assert_eq!(
            production_generation_fact_with(PID, &sysinfo_down, None),
            H2Probe::ReadFailure,
            "单源形态：机制自身不可用 ⇒ ReadFailure（既有语义）"
        );
    }

    /// 在场路径零交叉依赖：sysinfo 正面报告在场（lstart 可读）⇒ `Alive`——
    /// 在场是安全方向，不需要（也不咨询）死亡机制。
    #[test]
    fn h2_alive_from_positive_sighting_does_not_consult_crosscheck() {
        let sysinfo = h2_source(
            FactSighting::Present(STARTED_AT_MS),
            FactSighting::Present(STARTED_AT_MS),
        );
        // 交叉机制一律报缺席：若在场路径咨询了它，Alive 判定就会被污染。
        let cross_always_absent = h2_source(FactSighting::Absent, FactSighting::Absent);
        assert_eq!(
            production_generation_fact_with(PID, &sysinfo, Some(&cross_always_absent)),
            H2Probe::Alive(STARTED_AT_MS),
            "正面在场观测 ⇒ Alive（死亡机制不参与在场判定）"
        );
    }

    /// 真机制锚（macOS）：生产探针接真源（sysinfo + proc_pidpath）后，自身 pid
    /// ⇒ `Alive`（两次同值，lstart 稳定）；必然不存在的 pid ⇒ `NoSuchProcess`
    /// ——正向死亡证据由**真实内核通道**产出（不是只有注入缝可达）。
    #[test]
    #[cfg(target_os = "macos")]
    fn h2_production_probe_real_mechanisms_yield_positive_evidence() {
        let myself = std::process::id();
        let alive = production_generation_fact(myself);
        assert!(
            matches!(alive, H2Probe::Alive(started) if started > 0),
            "自身 pid 必须正面在场且 lstart 可读：{alive:?}"
        );
        assert_eq!(
            production_generation_fact(myself),
            alive,
            "同代际两次探测同值（lstart 恒定）"
        );
        // pid 高于 macOS pid 上限（~99999）且在 c_int 值域内：必然不存在。
        let never_assigned: u32 = 999_999_000;
        assert_eq!(
            production_generation_fact(never_assigned),
            H2Probe::NoSuchProcess,
            "必然不存在的 pid：真双源（sysinfo + proc_pidpath）一致缺席 = 正向死亡证据"
        );
    }
}
