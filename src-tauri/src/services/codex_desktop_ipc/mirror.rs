//! Revision Mirror：官方 Desktop `thread-stream-state-changed`（v11）的镜像与
//! Tuvis 状态投影（spec 3.3：IPC 原始 state 不直接进入远程 API）。
//!
//! 纪律：
//! * mirror 绑定当前 lease；换 owner（lease 身份变化）**先清状态**再重绑，
//!   禁止仅由调用方口头保证；
//! * `turns=[]` 是常态——正文以 `turnHistory.history.entitiesByKey/islands`
//!   为正史，按 islands[].entries 顺序取（不遍历无序 map 当时间序）；
//! * patch 严格校验（数组 add=插入 / remove=删除 / replace=只替换现有索引），
//!   一批中任一 op 非法 → 整批原子拒绝 → stale + 重拉 snapshot；
//!   path 直接对 `conversationState` 本体寻址（R1：真实形状无前缀，
//!   来源 reviewer 2026-10-17 实读 Windows 原始日志，脱敏夹具同步）；
//! * 低 revision 重放忽略；revision 缺口 → stale + need_snapshot；
//!   新 epoch（换代后）不与旧 revision 比大小（状态已清）；
//! * 预算：单 Thread 镜像 16 MiB；投影正文尾窗 1 MiB / 500 条并标 truncated；
//!   最新一条自身超窗时截头保底保留（绝不把整份投影清成 0 条）。
//!
//! ## 同代只读分页（任务 4）
//!
//! 分页与尾窗共用**同一个完整 canonical 投影器**（[`project_full_messages`]），但
//! **不经过**尾窗裁剪（[`bound_messages`] 只由 [`ThreadMirror::project`] 使用）：尾窗
//! 是展示窗口，不能用来判断「还有没有更早消息」。
//!
//! `historyToken` 组成（v2，`ThreadMirror::history_token` 是唯一实现）：
//!
//! ```text
//! v2-<clientTag>-<epoch>-<generation>-<buildGeneration>-<contentEpoch>-<digest>
//!   clientTag        = sha256(owner clientId) 前 4 字节（8 hex）
//!   epoch/generation = 镜像绑定的 OwnerLease 代际（换代即失效）
//!   buildGeneration  = 本镜像每装订一次新 snapshot 递增（镜像重建即失效；patch 不动）
//!   contentEpoch     = 已结算内容观察代（见下；只随已结算 Turn 的渲染内容变化）
//!   digest           = 前缀形：sha256("tuvis.codex.messages.history"
//!                               || len(hostId)||hostId || len(threadId)||threadId
//!                               || len(clientId)||clientId || epoch || generation
//!                               || buildGeneration || len(prefix)
//!                               || 游标之前每条消息 id 的 FNV-1a 64 指纹) 前 16 字节
//!                      根锚点形："root" || sha256("tuvis.codex.messages.root"
//!                               || len(hostId)||hostId || len(threadId)||threadId)
//!                               前 4 字节（8 hex）
//!   prefix           = **严格早于游标**的消息身份序（不含游标自身与更新消息）
//! ```
//!
//! 两类 token 的职责：
//! * **根锚点**（`project()` 下发，digest = root+线程标签）：只绑 owner/重建代与
//!   `contentEpoch`，**不绑尾窗被裁掉的前缀长度**——尾窗随普通追加/流式补全前移时
//!   token 逐字不变（R2）。它只用于给「第一页」找锚点；游标本身仍单独校验。
//!   线程标签保证 A 线程的根锚点不能在 B 线程上复用（同 owner/同 buildGeneration
//!   时元数据会重合，只靠 `root` 字面量无法区分线程）。
//! * **前缀形**（`messages_page` 逐页下发）：额外绑定下一页游标之前的前缀 id 序，
//!   续翻安全。
//!
//! 内容观察（P1/P2 修复，复核 F1/F4 后加强）：每次投影/分页在**已算好的完整
//! canonical 投影**上，按连续 `turn_id` 段登记**每一个** Turn 的消息身份序与逐条
//! 渲染指纹（id/role/kind/文本/附件序列化），并**保序**保存。
//!
//! 不推进代的情形：普通尾部追加（新 Turn / Turn 内追加）、仍在产出的 Turn 的末条
//! 消息流式补全、尾窗前移。推进代的情形：任何已观察 Turn 内同 id 的正文/附件改写、
//! 消息删除、Turn 内中段插入或重排、Turn 删除、**跨 Turn 换序**，以及已结算 Turn
//! 末条消息的改写（`status = completed` 的 Turn 没有流式豁免）。
//!
//! 相比「整体排除最新 Turn」：单 Turn 超过一页（例如 600 条）时，其 m050—m099 这类
//! 已分页内容不再是无人看守的可变区；而活动 Turn 的正常补全仍不会清空历史页。
//! 顺序敏感由保序表承担，跨 Turn 换序因此可以被发现（键值表比不出来）。
//!
//! 代际段（tag/epoch/generation/buildGeneration）不一致 → `unavailable`
//! （`generation_mismatch`）；代际一致但 `contentEpoch` 或前缀 digest 不一致 →
//! `cursor_invalid`（`history_rewritten`）。token 不参与鉴权，只做一致性锚。

use super::artifacts::{
    bounded_id, bounded_summary_source, has_input_artifacts, project_input_artifacts,
    project_item_artifacts, TurnArtifacts,
};
use super::history_view::{HistoryView, ViewHealth};
use super::pending::PendingProjector;
use super::types::{
    CapabilitySet, ContentState, ControlAvailability, MessagesPage, MessagesPageStatus, OwnerLease,
    ProjectedMessage, ProtocolError, ThreadKey, ThreadProjection,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// 单 Thread 镜像字节预算。
pub const MIRROR_BUDGET: usize = 16 * 1024 * 1024;
/// 全体镜像总字节预算（2026-10-17 复审 C4）：所有 ThreadMirror 保留状态的合计上限
///——64 个关注名额不是镜像字节数上界（64 × 16MiB ≫ 64MiB）。adopt 前原子预留；
/// patch 原子应用的 clone 是暂态（逐 Thread ≤ 16MiB 上界，不并入长期账目）。
pub const GLOBAL_MIRROR_BUDGET: usize = 64 * 1024 * 1024;
/// 投影正文最大条数。
pub const PROJECTION_MAX_MESSAGES: usize = 500;
/// 投影正文尾窗字节预算。
pub const PROJECTION_TAIL_BYTES: usize = 1024 * 1024;
/// 单条正文尾窗保底上限：最新一条自身超 [`PROJECTION_TAIL_BYTES`] 时该条不整条
/// 裁掉，正文截头到该字节数（字符边界对齐）加显式截断标记。
pub const PROJECTION_HEAD_BYTES: usize = 64 * 1024;
/// 只读历史页最多返回条数（HTTP `limit` 上限；缺省同为 50）。
pub const MESSAGES_PAGE_LIMIT: usize = 50;
/// 只读历史页 `messages` 序列化字节上限（独立计价；**不经过**尾窗裁剪）。
pub const MESSAGES_PAGE_BYTES: usize = 1024 * 1024;
/// 历史页 token 版本前缀（形态变更即不兼容旧 token）。
const HISTORY_TOKEN_VERSION: &str = "v2";
/// 根锚点 digest 的字面前缀（后接 8 hex 线程标签）。
const ROOT_DIGEST_PREFIX: &str = "root";
/// 历史 token 有界上限：最长形态 = 3 + 8 + 4×(1+20) + 1 + 32 ≈ 128 字符。
const HISTORY_TOKEN_MAX_LEN: usize = 160;

/// 单条正文被页预算裁剪时的显式机器码（页级 `reasonCode`；不是错误状态）。
const PAGE_TRUNCATED_REASON: &str = "page_truncated";

/// 全体镜像字节账本：claim(key, old, new) 原子预留，超总预算返回 false（镜像转
/// stale + Capacity 拒绝，等内存释放后由下次 ensure/事件救活）；mirror 卸载/
/// 清态时释放。
#[derive(Default)]
pub struct MirrorLedger {
    total: usize,
    per_key: HashMap<ThreadKey, usize>,
    cap: usize,
}

impl MirrorLedger {
    /// 生产账本（全局 64MiB）。
    pub fn global() -> Self {
        Self {
            total: 0,
            per_key: HashMap::new(),
            cap: GLOBAL_MIRROR_BUDGET,
        }
    }

    /// 测试账本（自定义上限）。
    pub fn with_cap(cap: usize) -> Self {
        Self {
            total: 0,
            per_key: HashMap::new(),
            cap,
        }
    }

    /// 原子预留：把 key 的占用从 old 换成 new；合计超上限 → false（不做部分生效）。
    pub fn claim(&mut self, key: &ThreadKey, old: usize, new: usize) -> bool {
        let delta = new as i64 - old as i64;
        let projected = self.total as i64 + delta;
        if projected < 0 || projected as usize > self.cap {
            return false;
        }
        self.total = projected as usize;
        self.per_key.insert(key.clone(), new);
        true
    }

    /// 退场释放。
    pub fn release(&mut self, key: &ThreadKey, size: usize) {
        self.total = self.total.saturating_sub(size);
        self.per_key.remove(key);
    }

    pub fn total(&self) -> usize {
        self.total
    }

    /// 账本上限（拒绝原因文案用）。
    pub fn cap_of(&self) -> usize {
        self.cap
    }
}

/// apply 的效果：`changed` 是否产生新状态；`need_snapshot` 是否需要重拉快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MirrorEffect {
    pub changed: bool,
    pub need_snapshot: bool,
}

impl MirrorEffect {
    const QUIET: Self = Self {
        changed: false,
        need_snapshot: false,
    };
}

/// 内容观察状态（token v2 的 `contentEpoch` 来源）。
///
/// **登记全部 Turn**（不是「除最新以外」）：最新 Turn 里已翻页 / 已离开尾窗的内容
/// 一旦被改写，必须能让旧页失效——单 Turn 超 500 条时，把整个最新 Turn 当作
/// 「流式可变区」会让 m050—m099 这类已分页内容永远陈旧。
///
/// 记录按 canonical 顺序存放（`Vec`，不是 `HashMap`）：跨 Turn 整体换序同样是
/// 历史改写，键值表比较不出来，顺序表可以。
///
/// 唯一豁免：**仍在产出**的 Turn 的**末条消息**——它可能正在流式补全，每来一个
/// delta 都推进内容代会把用户刚翻出来的历史页清空。已结算 Turn（status =
/// `completed`）没有这个豁免，末条消息的改写同样推进内容代。
#[derive(Default)]
struct ContentObservation {
    /// 内容观察代：已结算内容变化、Turn 删除或换序时 +1。
    epoch: u64,
    /// 每个 Turn 的观察记录，canonical 旧→新。
    turns: Vec<TurnObservation>,
}

/// 单个 Turn 的观察记录（canonical 顺序，与投影同源）。
struct TurnObservation {
    /// Turn 身份指纹（FNV-1a 64 over `turn_id`）。
    turn: u64,
    /// Turn 内每条消息的 id 指纹（canonical 顺序）。
    ids: Vec<u64>,
    /// 与 `ids` 一一对应的渲染内容指纹（id/role/kind/文本/附件）。
    fingerprints: Vec<u64>,
    /// 观察时该 Turn 是否仍在产出：活动 Turn 的末条消息允许流式增长、不参与比对。
    active: bool,
}

pub struct ThreadMirror {
    key: ThreadKey,
    lease: Option<OwnerLease>,
    revision: Option<u64>,
    stale: bool,
    /// conversationState 本体（R1：snapshot 存 `change["conversationState"]`；
    /// patch 直接对该本体寻址——reviewer 2026-10-17 实读 Windows 原始日志定案，
    /// 真实形状无 `conversationState` 路径前缀）
    state: Option<Value>,
    pending: PendingProjector,
    /// Turn 列表完整性（`turnHistory.history.isComplete`；不完整时 adapter 决定
    /// 是否调只读 load-complete-history）
    history_complete: bool,
    /// C4：全体镜像字节账本（生产挂全局账本；None = 独立镜像只受单 Thread 预算）
    ledger: Option<Arc<std::sync::Mutex<MirrorLedger>>>,
    /// 本镜像当前保留字节数（账本对账依据）
    size: usize,
    /// 镜像重建代（历史页 token 的一段）：每成功装订一次**新 snapshot**（含首帧、
    /// 重拉、换代重建）递增；patch 是增量，不递增——否则流式补全会每帧打断分页。
    build_generation: u64,
    /// 最近一次装订是否因镜像预算被拒。预算拒绝后可能没有可用 state；分页必须报
    /// `unavailable/mirror_capacity`，不得把「读不到」伪装成「没有更多」。
    budget_refused: bool,
    /// 已结算内容观察器：每次投影/分页在已算好的完整投影上顺带观察（不重投影）。
    /// 锁内只做一次指纹集合比较与交换，不跨 await、不记录正文或路径。
    content: Mutex<ContentObservation>,
}

impl Drop for ThreadMirror {
    fn drop(&mut self) {
        if let Some(ledger) = &self.ledger {
            ledger
                .lock()
                .expect("ledger lock")
                .release(&self.key, self.size);
        }
    }
}

impl ThreadMirror {
    pub fn new(key: ThreadKey) -> Self {
        Self {
            key,
            lease: None,
            revision: None,
            stale: false,
            state: None,
            pending: PendingProjector::new(),
            history_complete: true,
            ledger: None,
            size: 0,
            build_generation: 0,
            budget_refused: false,
            content: Mutex::new(ContentObservation::default()),
        }
    }

    /// 挂全局字节账本（adapter 构造镜像的唯一生产入口）。
    pub fn with_ledger(key: ThreadKey, ledger: Arc<std::sync::Mutex<MirrorLedger>>) -> Self {
        let mut m = Self::new(key);
        m.ledger = Some(ledger);
        m
    }

    pub fn key(&self) -> &ThreadKey {
        &self.key
    }

    /// 镜像当前绑定的 owner lease（2026-10-17 复审 C3：respond 三阶段同代复核锚——
    /// 阶段 1 记录、阶段 3 与现行 lease 全同比较；换代后旧载荷零写入）。
    pub fn lease(&self) -> Option<&OwnerLease> {
        self.lease.as_ref()
    }

    pub fn revision(&self) -> Option<u64> {
        self.revision
    }

    pub fn is_stale(&self) -> bool {
        self.stale
    }

    pub fn is_history_complete(&self) -> bool {
        self.history_complete
    }

    /// 只读 hydrate 触发判据（2026-10-19 复审 P2-2）：**列表完整性不够**——Turn
    /// 列表完整但某个 turn 的 items 分页未载完/淘汰/明确未加载（itemsView）时同样
    /// 需要 load-complete-history；结构不可读也做有界只读重载尝试（可能是坏快照）。
    /// 无快照不触发（follow 首帧随后到达再判）。目标 Turn 终态证据不走本判据。
    pub fn needs_hydration(&self) -> bool {
        match self.state.as_ref() {
            None => false,
            Some(state) => HistoryView::new(state).needs_hydration(),
        }
    }

    /// 原始 conversationState 本体（pending permissions 回显等只读用途；不进远程 API）。
    pub fn raw_state(&self) -> Option<&Value> {
        self.state.as_ref()
    }

    /// 校验 pending context_token（委托 pending 投影器）。
    pub fn token_valid(&self, request_key: &str, context_token: &str) -> bool {
        self.pending.token_valid(request_key, context_token)
    }

    /// 指定 Turn 实体的状态（2026-10-17 复审 C1：Turn 顺序屏障的目标终态证据来源）。
    /// 2026-10-18 真机形态适配：证据经统一读层（[`HistoryView`]）——按 canonical
    /// 引用链（islands entries.value → 实体）并以实体 `turnId` 字段认领目标；
    /// 实体 map key（`turn:`/`tail:` 合成键）不是 id，无 `type` 字段是正常形态，
    /// 未被引用的孤立实体不作数。不在场 / 无镜像 → None（调用方按「无证据」保守）。
    pub fn turn_status(&self, turn_id: &str) -> Option<String> {
        let state = self.state.as_ref()?;
        HistoryView::new(state)
            .turn_status(turn_id)
            .map(str::to_string)
    }

    /// 当前镜像中已存在的全部 Turn ID（派发边界快照用）：回执 turnId 若落在此集合
    /// 中，说明它是**派发前就存在**的旧 Turn 回显，不是本动作产生的新 Turn。
    pub fn turn_ids(&self) -> std::collections::HashSet<String> {
        self.state
            .as_ref()
            .map(|s| {
                HistoryView::new(s)
                    .turns()
                    .iter()
                    .map(|t| t.turn_id.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 当前镜像中处于 `inProgress` 的 Turn id（canonical 序最后一个 = 最新活动
    /// 回合）。**原生回合与受理回合一视同仁**（2026-10-22 原生回合打断扩展）：
    /// 它是端点打断阀值的回退来源——在册屏障不存在（桌面手打的回合）时，用它
    /// 定位「桌面上正在跑的那个回合」。stale / 无镜像 / 无活动回合 → None
    /// （调用方按「无阀值」保守拒发，与 P1-1 纪律一致）。
    pub fn active_turn_id(&self) -> Option<String> {
        if self.stale {
            return None;
        }
        let state = self.state.as_ref()?;
        HistoryView::new(state)
            .turns()
            .into_iter()
            .filter(|t| t.status == Some("inProgress"))
            .map(|t| t.turn_id.to_string())
            .next_back()
    }

    /// **本次请求的正式消息身份是否已在该 Turn 中落地**（2026-10-20 复审 P1）：
    /// 只认 `userMessage` item 的 `clientId == client_message_id`（正式消息身份）。
    /// `params.input` / `params.clientUserMessageId` 都是请求侧回显，不能作为送达
    /// 证据；Turn ID 新也不够（本地新建的 Turn 会抢先出现）。
    pub fn turn_has_client_message(&self, turn_id: &str, client_message_id: &str) -> bool {
        let Some(state) = self.state.as_ref() else {
            return false;
        };
        HistoryView::new(state).turns().iter().any(|t| {
            t.turn_id == turn_id
                && t.items.iter().any(|item| {
                    item.get("type").and_then(Value::as_str) == Some("userMessage")
                        && item.get("clientId").and_then(Value::as_str) == Some(client_message_id)
                })
        })
    }

    /// 指定 Turn 是否已以终态出现在当前镜像（completed/failed/**interrupted**）。
    /// 不在场 / inProgress / 未知状态 / stale / 无镜像 → false（保守不释放）。
    ///
    /// **interrupted（2026-10-21）**：Desktop 侧打断当前回合后把该 Turn 的 status
    /// 置 `interrupted`（打包产物中 `t.status = "interrupted"` 实证）。它**是**终态
    /// ——回合已结束，不会再有后续输出。不认它的话：远程打断 → 回合变 interrupted →
    /// 终态门不认 → 屏障永久在册 → 队列永远暂停（比打断前更糟：用户以为已解围）。
    /// 认它之后链路闭环：打断 → 终态证据 → 屏障释放 → 队列接力下一条。
    pub fn turn_is_terminal(&self, turn_id: &str) -> bool {
        !self.stale
            && matches!(
                self.turn_status(turn_id).as_deref(),
                Some("completed") | Some("failed") | Some("interrupted")
            )
    }

    /// 标记过期（连接断开/缺口/预算超限）。保留最后视图但禁写（adapter 侧门）。
    pub fn mark_stale(&mut self) {
        self.stale = true;
    }

    /// owner 失效（`client-status-changed(disconnected)` 等）：清状态 + 全部 pending
    /// token 失效（区别于 [`mark_stale`](Self::mark_stale) 的保视图只读）。之后只能
    /// 由换代 lease 的新 snapshot 重建。
    pub fn invalidate(&mut self) {
        self.clear_state();
    }

    fn clear_state(&mut self) {
        if let Some(ledger) = &self.ledger {
            ledger
                .lock()
                .expect("ledger lock")
                .release(&self.key, self.size);
        }
        self.size = 0;
        self.revision = None;
        self.state = None;
        self.stale = true;
        self.history_complete = true;
        // 清态后「上次预算拒绝」不再是当前事实：分页按 mirror_unavailable 如实上报，
        // 不沿用旧原因码。
        self.budget_refused = false;
        self.pending.reset();
    }

    /// 应用一帧 `thread-stream-state-changed` 的 change。
    pub fn apply(
        &mut self,
        lease: &OwnerLease,
        change: &Value,
    ) -> Result<MirrorEffect, ProtocolError> {
        // 换 owner：先清状态再重绑（世代不同一概不比较 revision）
        if self.lease.as_ref().is_none_or(|l| {
            l.epoch != lease.epoch
                || l.generation != lease.generation
                || l.client_id != lease.client_id
        }) {
            self.clear_state();
            self.lease = Some(lease.clone());
        }

        let change_type = change.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match change_type {
            "snapshot" => self.apply_snapshot(change),
            "patches" => self.apply_patches(change),
            _ => {
                // 未知 change 形态：保守 stale + 重拉（不猜测语义）
                self.stale = true;
                Ok(MirrorEffect {
                    changed: false,
                    need_snapshot: true,
                })
            }
        }
    }

    fn apply_snapshot(&mut self, change: &Value) -> Result<MirrorEffect, ProtocolError> {
        let rev = change.get("revision").and_then(|r| r.as_u64());
        let Some(rev) = rev else {
            self.stale = true;
            return Ok(MirrorEffect {
                changed: false,
                need_snapshot: true,
            });
        };
        // 低/等 revision 重放忽略（新绑定 revision=None 时接受任意）
        if let Some(cur) = self.revision {
            if rev <= cur {
                return Ok(MirrorEffect::QUIET);
            }
        }
        // 只存 conversationState 本体：patch 相对该本体寻址（R1 真实根路径）
        let state = change
            .get("conversationState")
            .cloned()
            .unwrap_or(Value::Null);
        self.adopt(rev, state)?;
        // 镜像重建代：只在新 snapshot 装订成功时递增（patch 是增量，不动它）。
        self.build_generation = self.build_generation.saturating_add(1);
        Ok(MirrorEffect {
            changed: true,
            need_snapshot: false,
        })
    }

    fn apply_patches(&mut self, change: &Value) -> Result<MirrorEffect, ProtocolError> {
        let base = change.get("baseRevision").and_then(|r| r.as_u64());
        let rev = change.get("revision").and_then(|r| r.as_u64());
        let patches = change.get("patches").and_then(|p| p.as_array()).cloned();
        let (Some(base), Some(rev), Some(patches)) = (base, rev, patches) else {
            self.stale = true;
            return Ok(MirrorEffect {
                changed: false,
                need_snapshot: true,
            });
        };
        // 重放：低于等于当前 revision 的旧批直接忽略
        if let Some(cur) = self.revision {
            if rev <= cur {
                return Ok(MirrorEffect::QUIET);
            }
        }
        let Some(cur) = self.revision else {
            // 尚无快照基线：缺口
            self.stale = true;
            return Ok(MirrorEffect {
                changed: false,
                need_snapshot: true,
            });
        };
        if base != cur {
            // revision 缺口/乱序：整批不应用，stale + 重拉
            self.stale = true;
            return Ok(MirrorEffect {
                changed: false,
                need_snapshot: true,
            });
        }
        // 原子应用：clone 上逐条 op，任一失败整批作废
        let Some(state) = self.state.clone() else {
            self.stale = true;
            return Ok(MirrorEffect {
                changed: false,
                need_snapshot: true,
            });
        };
        let mut next = state;
        for op in &patches {
            if apply_op(&mut next, op).is_err() {
                self.stale = true;
                return Ok(MirrorEffect {
                    changed: false,
                    need_snapshot: true,
                });
            }
        }
        // 2026-10-18 形态适配：完整性从**新 state** 派生（snapshot/patch 同一判定
        // 单点），不再沿用上一状态的缓存 bool
        self.adopt(rev, next)?;
        Ok(MirrorEffect {
            changed: true,
            need_snapshot: false,
        })
    }

    /// 提交新状态（预算校验：单 Thread 16MiB + 全体 64MiB；超限 stale + 拒绝，
    /// 不冒充无 owner）。完整性在此**单点派生**：`turnHistory.history.isComplete ===
    /// true` 才算 Turn 列表完整（真机形态在 history 层；缺字段 ≠ 完整——hydrate
    /// 触发判据不能缺省当完整）。
    fn adopt(&mut self, rev: u64, state: Value) -> Result<(), ProtocolError> {
        let size = serde_json::to_vec(&state)
            .map(|b| b.len())
            .unwrap_or(usize::MAX);
        if size > MIRROR_BUDGET {
            self.stale = true;
            self.budget_refused = true;
            return Err(ProtocolError::Capacity(format!(
                "mirror exceeds per-thread budget: {size} > {MIRROR_BUDGET}"
            )));
        }
        // C4：全体总预算——原子预留（old→new 换占），超限整批作废转 stale
        if let Some(ledger) = &self.ledger {
            let mut l = ledger.lock().expect("ledger lock");
            if !l.claim(&self.key, self.size, size) {
                self.stale = true;
                self.budget_refused = true;
                return Err(ProtocolError::Capacity(format!(
                    "mirror exceeds global budget: total would exceed {} bytes",
                    l.cap_of()
                )));
            }
        }
        self.budget_refused = false;
        self.size = size;
        self.revision = Some(rev);
        self.history_complete = HistoryView::new(&state).turn_list_complete();
        self.state = Some(state);
        self.stale = false;
        Ok(())
    }

    /// 统一状态投影（远程 API / SSE / 移动端唯一消费形态）。
    /// token 缓存存于 PendingProjector 内部互斥锁，`&self` 即可投影。
    ///
    /// `historyToken` 是**根锚点**（v2：只绑 owner/重建代与已结算内容代，不绑尾窗
    /// 被裁掉的前缀长度）：客户端用它 + 尾窗最早一条作为第一次更早分页的入参。
    /// 尾窗前移本身不换 token（R2）。
    pub fn project(&self, caps: CapabilitySet) -> ThreadProjection {
        // R1：state 即 conversationState 本体，按根直读
        let state = self.state.clone().unwrap_or(Value::Null);
        let all = project_full_messages(&state);
        // 已结算内容观察与投影共用同一次完整 canonical 投影（不额外投影）。
        let content_epoch = self.observe_content(&all);
        let (messages, truncated) = bound_messages(all);
        let history_token = self.history_token(content_epoch, HistoryTokenDigest::Root);
        let content_state = content_state_of(self.state.as_ref());
        let requests = state
            .get("requests")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        let pending = self.pending.project(&requests, &caps);
        ThreadProjection {
            thread_id: self.key.thread_id.clone(),
            revision: self.revision,
            stale: self.stale,
            availability: availability_of(&caps),
            capabilities: caps,
            runtime_status: runtime_status(&state),
            content_state,
            truncated,
            messages,
            history_token,
            pending,
            // 代际来自镜像**绑定的 lease**（广播事件与 GET/首快照同源）——首快照
            // 不带 0，否则 SSE 启动缓冲的 `supersedes` 会把任何事件都判成更新
            //（2026-10-20 第三轮复审 Important）。
            owner_epoch: self.lease.as_ref().map(|l| l.epoch).unwrap_or(0),
            owner_generation: self.lease.as_ref().map(|l| l.generation).unwrap_or(0),
        }
    }

    /// **同代只读历史分页**（`GET /codex/thread/messages` 的唯一实现；同步、不跨
    /// await、不重扫 rollout/磁盘）。复用完整 canonical 投影器，**不经过**尾窗裁剪。
    ///
    /// 状态映射（`reasonCode` 为有界机器码；非正常状态恒为空消息、`nextBefore=null`）：
    ///
    /// | 条件 | status | reasonCode |
    /// | --- | --- | --- |
    /// | 返回一页更早消息 | `ready` | null，或内容被裁时 `page_truncated` |
    /// | 游标之前没有消息（空历史同） | `end` | null |
    /// | 非空历史但没给游标 | `cursor_invalid` | `cursor_required` |
    /// | 游标不在当前 canonical 投影里 | `cursor_invalid` | `cursor_not_found` |
    /// | token 畸形 / 版本未知 | `cursor_invalid` | `invalid_history_token` |
    /// | token 代际一致但已结算内容变化，或前缀已改写/删除/重排 | `cursor_invalid` | `history_rewritten` |
    /// | token 的 owner/镜像重建代与当前不一致 | `unavailable` | `generation_mismatch` |
    /// | 无镜像 state（未 hydrate / 已清态） | `unavailable` | `mirror_unavailable` |
    /// | 镜像预算拒绝且无可用 state | `unavailable` | `mirror_capacity` |
    /// | 历史结构不可读 | `unavailable` | `history_unreadable` |
    /// | 历史列表不完整 / items 未载完 | `loading` | `history_incomplete` |
    ///
    /// 页预算：最多 `limit` 条且 `messages` 序列化 ≤ [`MESSAGES_PAGE_BYTES`]。预算不
    /// 够时**从最旧一侧收缩**（保持与游标相邻、无空洞）；最新一条自身超预算时按
    /// [`PROJECTION_HEAD_BYTES`] 截头并置 `page_truncated`，游标链仍可继续往前。
    pub fn messages_page(
        &self,
        cursor: Option<&str>,
        history_token: &str,
        limit: usize,
    ) -> MessagesPage {
        let revision = self.revision;
        let Some(state) = self.state.as_ref() else {
            let reason = if self.budget_refused {
                "mirror_capacity"
            } else {
                "mirror_unavailable"
            };
            return status_page(MessagesPageStatus::Unavailable, reason, revision);
        };
        // 历史健康先于游标判定：不完整/不可读时不能拿残缺投影回答「到头」。
        let view = HistoryView::new(state);
        if !view.readable() || view.health() != ViewHealth::Ok {
            return status_page(
                MessagesPageStatus::Unavailable,
                "history_unreadable",
                revision,
            );
        }
        if !view.turn_list_complete() || view.items_incomplete() {
            return status_page(MessagesPageStatus::Loading, "history_incomplete", revision);
        }
        let all = project_full_messages(state);
        // 与投影同源的内容观察：分页也要在比对 token 前观察到最新已结算内容，
        // 否则「先改写、后分页」会漏掉 epoch 推进。
        let content_epoch = self.observe_content(&all);
        if cursor.is_none() && !all.is_empty() {
            // 非空历史必须显式给游标：不把「没给锚点」猜成最新一页（也不猜成到头）。
            return status_page(
                MessagesPageStatus::CursorInvalid,
                "cursor_required",
                revision,
            );
        }
        let anchor = match cursor {
            None => all.len(),
            Some(id) => match all.iter().position(|m| m.id == id) {
                Some(index) => index,
                None => {
                    return status_page(
                        MessagesPageStatus::CursorInvalid,
                        "cursor_not_found",
                        revision,
                    )
                }
            },
        };
        let fingerprints: Vec<u64> = all.iter().map(|m| id_fingerprint(&m.id)).collect();
        match self.verify_history_token(history_token, &fingerprints[..anchor], content_epoch) {
            HistoryTokenCheck::Ok => {}
            HistoryTokenCheck::Malformed => {
                return status_page(
                    MessagesPageStatus::CursorInvalid,
                    "invalid_history_token",
                    revision,
                )
            }
            HistoryTokenCheck::Generation => {
                return status_page(
                    MessagesPageStatus::Unavailable,
                    "generation_mismatch",
                    revision,
                )
            }
            HistoryTokenCheck::Rewritten => {
                return status_page(
                    MessagesPageStatus::CursorInvalid,
                    "history_rewritten",
                    revision,
                )
            }
        }
        let limit = limit.clamp(1, MESSAGES_PAGE_LIMIT);
        let start = anchor.saturating_sub(limit);
        // 页字节预算：与尾窗同一计价口径（真实 JSON、数组括号与逗号）；从最新一条
        // 往旧累计——预算不够只丢**更旧**的候选，保证游标链无空洞。
        let mut kept = anchor;
        let mut bytes = 2usize; // `[]`
        let mut oversized = false;
        while kept > start {
            let index = kept - 1;
            let extra = serialized_message_bytes(&all[index]) + usize::from(kept != anchor);
            if bytes + extra > MESSAGES_PAGE_BYTES {
                if kept == anchor {
                    // 只有最新一条（紧邻游标）自身超预算：不丢弃、不假装完整。
                    kept = index;
                    oversized = true;
                }
                break;
            }
            bytes += extra;
            kept = index;
        }
        if kept == anchor {
            // 游标之前没有任何消息：真正到头（不是加载失败/预算拒绝）。
            return MessagesPage {
                messages: Vec::new(),
                history_token: self.history_token(
                    content_epoch,
                    HistoryTokenDigest::Prefix(&fingerprints[..anchor]),
                ),
                revision,
                next_before: None,
                status: MessagesPageStatus::End,
                reason_code: None,
            };
        }
        let mut messages: Vec<ProjectedMessage> = all[kept..anchor].to_vec();
        let mut reason_code = None;
        if oversized && fit_page_message(&mut messages[0]) {
            reason_code = Some(PAGE_TRUNCATED_REASON.to_string());
        }
        let next_before = messages.first().map(|m| m.id.clone());
        MessagesPage {
            messages,
            // 下一页 token 绑定**下一页锚点**（本页最早一条）之前的前缀。
            history_token: self.history_token(
                content_epoch,
                HistoryTokenDigest::Prefix(&fingerprints[..kept]),
            ),
            revision,
            next_before,
            status: MessagesPageStatus::Ready,
            reason_code,
        }
    }

    /// 历史代际 token（组成见模块文档）。`digest` 决定它是根锚点（投影）还是
    /// 前缀形（下一页游标）。
    fn history_token(&self, content_epoch: u64, digest: HistoryTokenDigest<'_>) -> String {
        let (client_id, epoch, generation) = self
            .lease
            .as_ref()
            .map(|l| (l.client_id.as_str(), l.epoch, l.generation))
            .unwrap_or(("", 0, 0));
        let digest = match digest {
            HistoryTokenDigest::Root => self.root_digest(),
            HistoryTokenDigest::Prefix(prefix) => self.prefix_digest(prefix),
        };
        format!(
            "{HISTORY_TOKEN_VERSION}-{tag}-{epoch}-{generation}-{build}-{content_epoch}-{digest}",
            tag = short_hash(client_id.as_bytes()),
            build = self.build_generation,
        )
    }

    /// 前缀形 digest：绑定**严格早于游标**的消息 id 指纹序（含线程/代际上下文）。
    fn prefix_digest(&self, prefix: &[u64]) -> String {
        let (client_id, epoch, generation) = self
            .lease
            .as_ref()
            .map(|l| (l.client_id.as_str(), l.epoch, l.generation))
            .unwrap_or(("", 0, 0));
        let mut hasher = Sha256::new();
        hasher.update(b"tuvis.codex.messages.history\n");
        for field in [
            self.key.host_id.as_str(),
            self.key.thread_id.as_str(),
            client_id,
        ] {
            hasher.update((field.len() as u64).to_le_bytes());
            hasher.update(field.as_bytes());
        }
        hasher.update(epoch.to_le_bytes());
        hasher.update(generation.to_le_bytes());
        hasher.update(self.build_generation.to_le_bytes());
        hasher.update((prefix.len() as u64).to_le_bytes());
        for fingerprint in prefix {
            hasher.update(fingerprint.to_le_bytes());
        }
        hasher
            .finalize()
            .iter()
            .take(16)
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// 根锚点 digest：`root` + 线程短标签（不绑尾窗被裁掉的前缀长度，也不绑
    /// contentEpoch——后者已在 metadata 段）。尾窗前移因此不改 token；线程标签
    /// 保证 A 线程的根锚点不能在 B 线程上复用（同 owner / 同 buildGeneration 时
    /// 其余元数据会重合）。
    fn root_digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"tuvis.codex.messages.root\n");
        for field in [self.key.host_id.as_str(), self.key.thread_id.as_str()] {
            hasher.update((field.len() as u64).to_le_bytes());
            hasher.update(field.as_bytes());
        }
        let tag: String = hasher
            .finalize()
            .iter()
            .take(4)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("{ROOT_DIGEST_PREFIX}{tag}")
    }

    /// 内容观察：在已算好的完整 canonical 投影上按连续 Turn 段登记每个 Turn 的
    /// 消息身份序与渲染指纹，并与上次观察比对。新 Turn 追加、活动 Turn 的末条流式
    /// 增长不推进代；任何已观察内容的改写、删除、重排 → `epoch + 1`。返回 epoch。
    fn observe_content(&self, all: &[ProjectedMessage]) -> u64 {
        let observed = self.turn_observations(all);
        let mut observation = self.content.lock().expect("content observation lock");
        if !content_unchanged(&observation.turns, &observed) {
            observation.epoch = observation.epoch.saturating_add(1);
        }
        // 无论是否推进代都换存：活动 Turn 的末条指纹必须跟上最新一次观察，
        // 否则它结算那一刻会被误判成「内容刚刚变过」。
        observation.turns = observed;
        observation.epoch
    }

    /// **有证据**仍在产出的 Turn id 集合。证据面只有观测夹具里的 `inProgress`
    /// （活动 Turn 的唯一已知形态）。`completed`、失败、已终止、缺状态都不在此列：
    /// 未知状态不能自动获得历史一致性豁免（复核 G2），否则历史 failed Turn 的
    /// 末条改写会继续漏检——「不是 completed」不等于「仍在流式」。
    fn active_turn_ids(&self) -> HashSet<&str> {
        const ACTIVE_STATUSES: &[&str] = &["inProgress"];
        let Some(state) = self.state.as_ref() else {
            return HashSet::new();
        };
        HistoryView::new(state)
            .turns()
            .iter()
            .filter(|turn| {
                turn.status
                    .is_some_and(|status| ACTIVE_STATUSES.contains(&status))
            })
            .map(|turn| turn.turn_id)
            .collect()
    }

    /// 把完整 canonical 投影切成按连续 `turn_id` 分组的观察记录。
    /// 旧载荷缺 `turn_id` 时同属空串组（整段按一个 Turn 观察）。
    ///
    /// 活动豁免要**两个条件同时成立**：该 Turn 是有证据的活动状态（`inProgress`），
    /// 且它是 canonical 顺序里的**最后一个** Turn——只有那里才可能正在产出。历史
    /// Turn 无论标注什么状态都按已结算内容比对，多义/错标换不来豁免。
    fn turn_observations(&self, all: &[ProjectedMessage]) -> Vec<TurnObservation> {
        let active_ids = self.active_turn_ids();
        let mut turns: Vec<TurnObservation> = Vec::new();
        for message in all {
            let turn_id = message.turn_id.as_deref().unwrap_or("");
            let turn = id_fingerprint(turn_id);
            if turns.last().is_none_or(|last| last.turn != turn) {
                turns.push(TurnObservation {
                    turn,
                    ids: Vec::new(),
                    fingerprints: Vec::new(),
                    active: active_ids.contains(turn_id),
                });
            }
            let last = turns.last_mut().expect("turn group just pushed");
            last.ids.push(id_fingerprint(&message.id));
            let mut hash = Fnv64::offset();
            fingerprint_message(&mut hash, message);
            last.fingerprints.push(hash.finish());
        }
        if let Some((_, history)) = turns.split_last_mut() {
            for turn in history {
                turn.active = false;
            }
        }
        turns
    }

    /// token 校验：畸形 → Malformed；owner/重建代不同 → Generation；内容观察代
    /// 不同或前缀 digest 不同 → Rewritten；根锚点在代际/内容一致时放行（游标已
    /// 单独校验；续翻仍由前缀形 token 绑定）。
    fn verify_history_token(
        &self,
        token: &str,
        prefix: &[u64],
        content_epoch: u64,
    ) -> HistoryTokenCheck {
        let Some(given) = parse_history_token(token) else {
            return HistoryTokenCheck::Malformed;
        };
        let (client_id, epoch, generation) = self
            .lease
            .as_ref()
            .map(|l| (l.client_id.as_str(), l.epoch, l.generation))
            .unwrap_or(("", 0, 0));
        if given.tag != short_hash(client_id.as_bytes())
            || given.epoch != epoch
            || given.generation != generation
            || given.build_generation != self.build_generation
        {
            return HistoryTokenCheck::Generation;
        }
        if given.content_epoch != content_epoch {
            return HistoryTokenCheck::Rewritten;
        }
        if given.digest.starts_with(ROOT_DIGEST_PREFIX) {
            return if given.digest == self.root_digest() {
                HistoryTokenCheck::Ok
            } else {
                HistoryTokenCheck::Rewritten
            };
        }
        if given.digest != self.prefix_digest(prefix) {
            return HistoryTokenCheck::Rewritten;
        }
        HistoryTokenCheck::Ok
    }
}

/// 历史 token 的 digest 形态。
enum HistoryTokenDigest<'a> {
    /// 投影根锚点（只绑线程身份与元数据代际；不绑尾窗前缀）。
    Root,
    /// 下一页游标之前的消息 id 指纹序。
    Prefix(&'a [u64]),
}

/// 已解析的 v2 token。计数器都以 `u64` 解析；tag/digest 只认固定长度 ASCII hex。
struct ParsedHistoryToken<'a> {
    tag: &'a str,
    epoch: u64,
    generation: u64,
    build_generation: u64,
    content_epoch: u64,
    digest: &'a str,
}

/// 历史 token 校验结果（映射见 [`ThreadMirror::messages_page`] 的状态表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryTokenCheck {
    Ok,
    /// 形态/版本不认识（客户端乱填、旧版本 token）。
    Malformed,
    /// 代际段不一致：owner 换代、进程重启或镜像重建。
    Generation,
    /// 代际一致但内容观察代或前缀身份序变了：已结算历史被改写/删除，或已分页
    /// 前缀被改写/删除/重排。
    Rewritten,
}

/// 非正常状态页：空消息、`nextBefore=null`、空 token（不得被当作完整空页）。
fn status_page(
    status: MessagesPageStatus,
    reason_code: &str,
    revision: Option<u64>,
) -> MessagesPage {
    MessagesPage {
        messages: Vec::new(),
        history_token: String::new(),
        revision,
        next_before: None,
        status,
        reason_code: Some(reason_code.to_string()),
    }
}

/// 解析 `v2-<tag>-<epoch>-<generation>-<build>-<contentEpoch>-<digest>`；形态不合法
/// （版本/tag/计数器/digest 段）→ None。计数器只做十进制解析，tag/digest 只认固定
/// 长度 ASCII hex，杜绝把任意字符串当 token 比较；超长一律畸形（有界）。
fn parse_history_token(token: &str) -> Option<ParsedHistoryToken<'_>> {
    if token.len() > HISTORY_TOKEN_MAX_LEN {
        return None;
    }
    let (meta, digest) = token.rsplit_once('-')?;
    let mut parts = meta.split('-');
    if parts.next()? != HISTORY_TOKEN_VERSION {
        return None;
    }
    let tag = parts.next()?;
    if tag.len() != 8 || !tag.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let epoch = parts.next()?.parse::<u64>().ok()?;
    let generation = parts.next()?.parse::<u64>().ok()?;
    let build_generation = parts.next()?.parse::<u64>().ok()?;
    let content_epoch = parts.next()?.parse::<u64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let digest_ok = match digest.strip_prefix(ROOT_DIGEST_PREFIX) {
        Some(tag) => tag.len() == 8 && tag.bytes().all(|b| b.is_ascii_hexdigit()),
        None => digest.len() == 32 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
    };
    if !digest_ok {
        return None;
    }
    Some(ParsedHistoryToken {
        tag,
        epoch,
        generation,
        build_generation,
        content_epoch,
        digest,
    })
}

/// 客户端身份短标签（8 hex）；只用于 token 代际段，不泄露原 clientId。
fn short_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 消息 id 的稳定指纹（FNV-1a 64）：分页 token 只绑定消息**身份序**，不绑正文
/// 字节——流式补全（同 id 文本增长）不得使历史页失效。
fn id_fingerprint(id: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// FNV-1a 64 增量哈希：流式写入已投影字段/附件 JSON，不额外分配。
struct Fnv64(u64);

impl Fnv64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn offset() -> Self {
        Self(Self::OFFSET)
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    /// 长度前缀 + 字节：字段拼接不产生歧义。
    fn push_str(&mut self, value: &str) {
        self.write_bytes(&(value.len() as u64).to_le_bytes());
        self.write_bytes(value.as_bytes());
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

impl std::io::Write for Fnv64 {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.write_bytes(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 一条消息里客户端可见字段的指纹：id/role/kind/文本 + 序列化附件。附件直接
/// `to_writer` 喂给哈希器（与下发 JSON 同源，且不分配）。
fn fingerprint_message(hash: &mut Fnv64, message: &ProjectedMessage) {
    hash.push_str(&message.id);
    hash.push_str(&message.role);
    hash.push_str(&message.kind);
    hash.push_str(&message.text);
    serde_json::to_writer(&mut *hash, &message.artifacts).expect("附件只含 JSON 可序列化字段");
}

/// 两次内容观察是否等价（true = 不推进内容代）。
///
/// 规则（对应复核 F1/F4）：
/// * Turn 只能**追加**：数量变少、同位置 Turn 身份不同、或某个 Turn 的 id 序不再是
///   上次的前缀 → 删除 / 跨 Turn 换序 / Turn 内中段插入，一律判为变化；
/// * 已结算 Turn：id 前缀内每条消息的渲染指纹必须逐字相同（含末条）；
/// * 活动 Turn：末条消息可能仍在流式补全，只比到「当前末条之前」——正常补全不
///   推进代，而这条之前任何同 id 改写/删除都会被抓到；
/// * 上一观察里那条活动末条：即使本次它已不再是末条（末次增量与追加下一条、或与
///   turn 结算同批到达），也只放过它这一次——否则正常的结算增量会清空用户刚翻出
///   的历史页（复核 G3）。下一次观察它已是历史消息，按已结算内容全量比对。
fn content_unchanged(previous: &[TurnObservation], observed: &[TurnObservation]) -> bool {
    if observed.len() < previous.len() {
        return false;
    }
    for (old, new) in previous.iter().zip(observed.iter()) {
        if old.turn != new.turn {
            return false;
        }
        if new.ids.len() < old.ids.len() || new.ids[..old.ids.len()] != old.ids[..] {
            return false;
        }
        let limit = if new.active {
            new.ids.len().saturating_sub(1)
        } else {
            new.ids.len()
        };
        let mut compared = old.ids.len().min(limit);
        if old.active {
            // 上一观察允许补全的那条：只豁免一次（它随后就是历史内容）。
            compared = compared.min(old.ids.len().saturating_sub(1));
        }
        if new.fingerprints[..compared] != old.fingerprints[..compared] {
            return false;
        }
    }
    true
}

/// 单条撑破页预算：先按既有展示截断纪律截正文头部，附件元数据仍超限时按服务端
/// 预算省略并计数（不静默丢整条）。返回是否发生内容裁剪（`page_truncated`）。
fn fit_page_message(message: &mut ProjectedMessage) -> bool {
    let mut size = serialized_message_bytes(message) + 2;
    let mut trimmed = false;
    if size > MESSAGES_PAGE_BYTES && message.text.len() > PROJECTION_HEAD_BYTES {
        message.text = truncate_text_head(&message.text, PROJECTION_HEAD_BYTES);
        size = serialized_message_bytes(message) + 2;
        trimmed = true;
    }
    while size > MESSAGES_PAGE_BYTES {
        let Some(popped) = message.artifacts.pop() else {
            break;
        };
        size -= serde_json::to_vec(&popped)
            .expect("附件只含 JSON 可序列化字段")
            .len()
            + usize::from(!message.artifacts.is_empty());
        message.omitted_artifact_count += 1;
        trimmed = true;
    }
    trimmed
}

/// 内容健康状态（与写能力正交——能发送不代表历史已载入；读取出错不得呈现为
/// 「真实空会话」）：
/// * 无状态（尚未收到首帧 snapshot）→ Loading；
/// * `turnHistory.history` 形态不可读 → Unsupported（不伪造 0 条空历史）；
/// * Turn 列表不完整（history.isComplete ≠ true）→ Loading（hydrate 判据同源）；
/// * Turn 列表完整但任一 turn 的 items 分页未载完 → Partial；
/// * 其余 → Ready（此时真实 0 条才允许显示「暂无消息」）。stale 只影响写侧，
///   不改内容健康语义（保留最后视图照常展示）。
fn content_state_of(state: Option<&Value>) -> ContentState {
    let Some(state) = state else {
        return ContentState::Loading;
    };
    let view = HistoryView::new(state);
    if !view.readable() || view.health() != super::history_view::ViewHealth::Ok {
        // 结构不可读 ≠ 真空会话（2026-10-19 复审 P2-3：非法 entries/items 类型/
        // 悬空引用不得 Ready+0 条）
        ContentState::Unsupported
    } else if !view.turn_list_complete() {
        ContentState::Loading
    } else if view.items_incomplete() {
        ContentState::Partial
    } else {
        ContentState::Ready
    }
}

fn availability_of(caps: &CapabilitySet) -> ControlAvailability {
    if caps.send && caps.state {
        ControlAvailability::Controllable
    } else if caps.send {
        ControlAvailability::SendOnly
    } else {
        ControlAvailability::CompatOnly
    }
}

/// runtime status：threadRuntimeStatus.type + turn 状态综合；缺字段 unknown。
fn runtime_status(state: &Value) -> String {
    let declared = state
        .get("threadRuntimeStatus")
        .and_then(|t| t.get("type"))
        .and_then(|t| t.as_str());
    match declared {
        Some("active") => return "active".into(),
        Some("systemError") => return "failed".into(),
        Some("idle") => {}
        Some("notLoaded") => {}
        Some(_) => return "unknown".into(),
        None => {}
    }
    // idle/缺省时看 turn 历史：有 inProgress → active；有 failed → failed
    // 2026-10-18 形态适配：经统一读层（canonical 引用链 + turnId 字段认领），
    // 不再要求实体 type=="turn"（真机 turn 实体无该字段）
    {
        let view = HistoryView::new(state);
        let mut saw_failed = false;
        for turn in view.turns() {
            match turn.status {
                Some("inProgress") => return "active".into(),
                Some("failed") => saw_failed = true,
                _ => {}
            }
        }
        if saw_failed {
            return "failed".into();
        }
    }
    if declared == Some("idle") {
        "idle".into()
    } else {
        "unknown".into()
    }
}

/// **完整** canonical 历史投影（不分页、不裁尾窗）：尾窗与历史页共用的唯一投影器。
/// 回合内编号在完整序列上生成，翻页不重编号。
/// 字节尾窗**绝不把最新一条整条裁掉**：单条自身超 1 MiB（agentMessage 贴大日志/
/// 大代码块真实可达）→ 保底保留该 1 条，正文截头（[`PROJECTION_HEAD_BYTES`]）+
/// 显式标记，truncated 置位；多条累计超限才丢弃较旧条目（维持既有逻辑）。
/// 2026-10-18 真机形态适配：经统一读层走 canonical 引用链（entries.value → 实体，
/// turnId 字段认领），正文取 `turn.items[]` **内联对象**（camelCase）——
/// `userMessage.content[]` 的 text 块、`agentMessage.text`；`error.message` 走系统
/// 安全摘要；`reasoning` 按产品语义过滤；未知 kind 保留 `[unsupported item]` 摘要，
/// 绝不静默滤成 0 条。
/// 开场用户项（计划 Task 6）：**只处理已经由 HistoryView 验明身份的 Turn**；
/// `params.input` 非文本块只在存在附件时保留消息。返回
/// `(投影 ID, 正文, 原始输入块)`，不猜非文本块的内容。
///
/// 投影 ID = `{turnId}/opening/{clientUserMessageId}`——它是**显示身份**，不是投递
/// 身份：不参与 `ThreadMirror` 健康、完整性、runtime、送达或屏障。
fn opening_input<'a>(turn_id: &str, params: &'a Value) -> Option<(String, String, &'a [Value])> {
    let client = params.get("clientUserMessageId")?.as_str()?;
    if client.is_empty() {
        return None;
    }
    let blocks = params.get("input")?.as_array()?;
    let text = input_text(blocks);
    if text.is_empty() && !has_input_artifacts(blocks) {
        return None;
    }
    Some((
        bounded_id(&format!("{turn_id}/opening/{client}")),
        text,
        blocks,
    ))
}

/// 可渲染的正式用户项：有文本，或只有附件（纯附件消息也必须可见）。
fn is_renderable_user_item(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("userMessage")
        && (user_message_text(item).is_some()
            || item
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| has_input_artifacts(blocks)))
}

fn project_full_messages(state: &Value) -> Vec<ProjectedMessage> {
    let view = HistoryView::new(state);
    let turns = view.turns();
    let cwd = state.get("cwd").and_then(Value::as_str);
    // 先确认正式用户项：键迁移双引用窗口里旧副本的开场后备不得先于新副本输出。
    let has_formal_user: HashSet<&str> = turns
        .iter()
        .filter(|turn| turn.items.iter().any(is_renderable_user_item))
        .map(|turn| turn.turn_id)
        .collect();
    let mut all: Vec<ProjectedMessage> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    // 编号上下文按 canonical 回合聚合：尾窗只切消息，不重编号。
    let mut contexts: HashMap<String, TurnArtifacts> = HashMap::new();
    // 同回合“附件之前最近一条非空用户消息”的有界副本（摘要来源）。
    let mut user_summaries: HashMap<&str, String> = HashMap::new();

    for turn in turns {
        let context = contexts.entry(bounded_id(turn.turn_id)).or_default();
        let opening = turn
            .params
            .and_then(|params| opening_input(turn.turn_id, params));
        // 开场后备：正式用户项未到达（或已被历史裁剪）时补一条；正式项到达后由
        // `has_formal_user` 压住，同一逻辑消息不出现第二条。
        if !has_formal_user.contains(turn.turn_id) {
            if let Some((id, text, blocks)) = &opening {
                if seen_ids.insert(id.clone()) {
                    if !text.trim().is_empty() {
                        user_summaries.insert(turn.turn_id, bounded_summary_source(text));
                    }
                    let summary = user_summaries.get(turn.turn_id).map(String::as_str);
                    let batch = project_input_artifacts(blocks, id, cwd, summary, context);
                    all.push(ProjectedMessage {
                        id: id.clone(),
                        role: "user".into(),
                        kind: "userMessage".into(),
                        text: text.clone(),
                        ts: None,
                        turn_id: Some(bounded_id(turn.turn_id)),
                        artifacts: batch.artifacts,
                        omitted_artifact_count: batch.omitted,
                        turn_image_count: 0,
                    });
                }
            }
        }

        for (position, item) in turn.items.iter().enumerate() {
            let Some(obj) = item.as_object() else {
                continue; // 非对象 item：无可读形态，保守跳过（不猜）
            };
            let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
            // 稳定标识 = item.id；缺 id 的兜底拼接仅保序去重。正式 userMessage 的
            // clientId 与 params.clientUserMessageId 相等时复用开场投影 ID：
            // 同一逻辑消息在「后备 → 正式」替换时身份不变。
            let mut id = obj
                .get("id")
                .and_then(Value::as_str)
                .map(bounded_id)
                .unwrap_or_else(|| bounded_id(&format!("{}/{}", turn.turn_id, position)));
            if kind == "userMessage" {
                if let (Some((opening_id, _, _)), Some(client)) = (
                    opening.as_ref(),
                    obj.get("clientId").and_then(Value::as_str),
                ) {
                    if turn
                        .params
                        .and_then(|params| params.get("clientUserMessageId"))
                        .and_then(Value::as_str)
                        == Some(client)
                    {
                        id = opening_id.clone();
                    }
                }
            }
            if !seen_ids.insert(id.clone()) {
                continue;
            }
            let (role, text) = match kind {
                "userMessage" => {
                    let text = user_message_text(item).unwrap_or_default();
                    // 无可渲染正文且无附件 → 不造空泡；纯附件消息必须保留。
                    if text.is_empty()
                        && !obj
                            .get("content")
                            .and_then(Value::as_array)
                            .is_some_and(|blocks| has_input_artifacts(blocks))
                    {
                        continue;
                    }
                    if !text.trim().is_empty() {
                        user_summaries.insert(turn.turn_id, bounded_summary_source(&text));
                    }
                    ("user", text)
                }
                "agentMessage" => {
                    let text = obj
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    if text.is_empty() {
                        continue; // 流式空文本占位不造空泡（文本经 replace 原位出现）
                    }
                    ("assistant", text)
                }
                // 推理过程不上墙（复审裁决：允许按产品语义过滤 reasoning）
                "reasoning" | "" => continue,
                "error" => ("system", error_item_summary(item)),
                // 结构化附件载体：正文为空，内容在 `artifacts` 上（Task 3）。
                "imageGeneration" | "fileChange" | "imageView" => ("system", String::new()),
                other => (
                    "system",
                    format!("[unsupported item: {}]", bounded_id(other)),
                ),
            };
            let batch = project_item_artifacts(
                item,
                &id,
                cwd,
                user_summaries.get(turn.turn_id).map(String::as_str),
                context,
            );
            if text.is_empty() && batch.artifacts.is_empty() {
                continue; // 既无正文也无附件（如空 changes）：不留空气泡
            }
            all.push(ProjectedMessage {
                id,
                role: role.into(),
                kind: bounded_id(kind),
                text,
                ts: obj.get("ts").and_then(Value::as_i64),
                turn_id: Some(bounded_id(turn.turn_id)),
                artifacts: batch.artifacts,
                omitted_artifact_count: batch.omitted,
                turn_image_count: 0,
            });
        }
    }

    // 回合内图片卡总数在完整 canonical 顺序上生成；尾窗与以后分页只切消息。
    for message in &mut all {
        message.turn_image_count = message
            .turn_id
            .as_deref()
            .and_then(|turn_id| contexts.get(turn_id))
            .map_or(0, |turn| turn.image_count);
    }
    all
}

/// 尾窗预算：500 条 / 1 MiB，按**实际序列化 JSON** 计（含字段、附件、转义、
/// 引号与数组标点）。`truncated` 只表达消息窗口裁剪（丢消息）或最新条正文截头
/// （预有语义）；只弹最新条的附件**不**置 `truncated`，由递增的
/// `omittedArtifactCount` 单独表达，避免单条投影被横幅误报为“仅显示最近的消息”。
fn bound_messages(mut all: Vec<ProjectedMessage>) -> (Vec<ProjectedMessage>, bool) {
    let mut truncated = false;
    if all.len() > PROJECTION_MAX_MESSAGES {
        all = all.split_off(all.len() - PROJECTION_MAX_MESSAGES);
        truncated = true;
    }
    // 最新一条保底：先按实际序列化体积裁正文/附件，绝不把整份投影清成 0 条。
    if let Some(last) = all.last_mut() {
        let mut size = serialized_message_bytes(last);
        if size + 2 > PROJECTION_TAIL_BYTES {
            if last.text.len() > PROJECTION_HEAD_BYTES {
                // 预有语义：最新条正文截头属于消息窗口裁剪，置 truncated。
                truncated = true;
                last.text = truncate_text_head(&last.text, PROJECTION_HEAD_BYTES);
                size = serialized_message_bytes(last);
            }
            // 优先保留正文与较早附件；被预算裁掉的元数据只计数，不冒充 UI 展示限额。
            while size + 2 > PROJECTION_TAIL_BYTES {
                let Some(popped) = last.artifacts.pop() else {
                    break;
                };
                size -= serde_json::to_vec(&popped)
                    .expect("附件只含 JSON 可序列化字段")
                    .len()
                    + usize::from(!last.artifacts.is_empty());
                last.omitted_artifact_count += 1;
            }
        }
    }
    // 从新到旧累计实际序列化字节（含数组括号与逗号）；不足预算的更早消息丢弃。
    let mut bytes = 2; // 空数组 `[]`
    let mut keep_from = all.len();
    for (index, message) in all.iter().enumerate().rev() {
        let next = serialized_message_bytes(message) + usize::from(keep_from < all.len());
        if bytes + next > PROJECTION_TAIL_BYTES {
            truncated = true;
            break;
        }
        bytes += next;
        keep_from = index;
    }
    if keep_from == all.len() && !all.is_empty() {
        // 最新条自身仍超预算：当前所有字段均有界，理论上已被上面裁进预算。
        // 防御式保底保留最新一条（debug 下断言暴露不变量被破坏），绝不清成 0 条。
        debug_assert!(
            false,
            "newest projected message still exceeds the tail budget after trimming"
        );
        if let Some(newest) = all.pop() {
            all.clear();
            all.push(newest);
            truncated = true;
        }
    } else if keep_from > 0 {
        all = all.split_off(keep_from);
    }
    (all, truncated)
}

fn serialized_message_bytes(message: &ProjectedMessage) -> usize {
    serde_json::to_vec(message)
        .expect("投影只含 JSON 可序列化字段")
        .len()
}

fn input_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// userMessage 正文：`content[]` 中 `type=="text"` 块的 `text` 拼接（真机形态；
/// 块级包装字段如 text_elements 不下发）。无可渲染正文 → None。
fn user_message_text(item: &Value) -> Option<String> {
    let blocks = item.get("content")?.as_array()?;
    let text = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

/// error item 的系统摘要：`message` 为桌面侧给用户看的错误文案（截断到 240 字符，
/// 不透传 errorInfo/additionalDetails 等内部结构）；缺 message → 固定占位。
fn error_item_summary(item: &Value) -> String {
    const CAP: usize = 240;
    let raw = item
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty());
    match raw {
        Some(m) if m.len() <= CAP => format!("[error] {m}"),
        Some(m) => {
            let mut cut = CAP;
            while !m.is_char_boundary(cut) {
                cut -= 1;
            }
            format!("[error] {}…", &m[..cut])
        }
        None => "[error]".to_string(),
    }
}

/// 正文截头（字节上限，按字符边界回退）+ 显式截断标记（自报原文总字符数）。
/// 展示截断纪律（`DISPLAY_TRUNCATED: ` 前缀 + 原文总字符数）——
/// 绝不静默砍；未超限逐字原文。
fn truncate_text_head(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut cut = max_bytes;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "DISPLAY_TRUNCATED: {}…已截断，原文共 {} 字符",
        &text[..cut],
        text.chars().count()
    )
}

/// 单个 patch op（严格形态）：add=数组插入/对象新增，replace=只替换现有，
/// remove=删除；path 数组元素为对象键（字符串）或数组下标（整数）。
fn apply_op(root: &mut Value, op: &Value) -> Result<(), ProtocolError> {
    let op_name = op
        .get("op")
        .and_then(|o| o.as_str())
        .ok_or_else(|| ProtocolError::InvalidState("patch op missing".into()))?;
    let path: Vec<&Value> = op
        .get("path")
        .and_then(|p| p.as_array())
        .map(|a| a.iter().collect())
        .ok_or_else(|| ProtocolError::InvalidState("patch path missing".into()))?;
    if path.is_empty() {
        return Err(ProtocolError::InvalidState("patch path empty".into()));
    }
    let value = op.get("value").cloned();

    // 逐级下行（父节点必须存在）
    let mut node = root;
    for seg in &path[..path.len() - 1] {
        node = descend(node, seg)?;
    }
    let last = path[path.len() - 1];
    match op_name {
        "add" => {
            if let Some(idx) = index_of(last)? {
                if !node.is_array() {
                    return Err(ProtocolError::InvalidState("add into non-array".into()));
                }
                let arr = node.as_array_mut().expect("checked");
                if idx > arr.len() {
                    return Err(ProtocolError::InvalidState(format!(
                        "array add index {idx} > len {}",
                        arr.len()
                    )));
                }
                arr.insert(idx, value.unwrap_or(Value::Null));
                Ok(())
            } else {
                let key = key_of(last)?;
                if !node.is_object() {
                    return Err(ProtocolError::InvalidState("add into non-object".into()));
                }
                node.as_object_mut()
                    .expect("checked")
                    .insert(key, value.unwrap_or(Value::Null));
                Ok(())
            }
        }
        "replace" => {
            if let Some(idx) = index_of(last)? {
                let arr = node
                    .as_array_mut()
                    .ok_or_else(|| ProtocolError::InvalidState("replace on non-array".into()))?;
                if idx >= arr.len() {
                    return Err(ProtocolError::InvalidState(format!(
                        "replace index {idx} >= len {}",
                        arr.len()
                    )));
                }
                arr[idx] = value.unwrap_or(Value::Null);
                Ok(())
            } else {
                let key = key_of(last)?;
                let obj = node
                    .as_object_mut()
                    .ok_or_else(|| ProtocolError::InvalidState("replace on non-object".into()))?;
                if !obj.contains_key(&key) {
                    return Err(ProtocolError::InvalidState(format!(
                        "replace missing key {key}"
                    )));
                }
                obj.insert(key, value.unwrap_or(Value::Null));
                Ok(())
            }
        }
        "remove" => {
            if let Some(idx) = index_of(last)? {
                let arr = node
                    .as_array_mut()
                    .ok_or_else(|| ProtocolError::InvalidState("remove on non-array".into()))?;
                if idx >= arr.len() {
                    return Err(ProtocolError::InvalidState(format!(
                        "remove index {idx} >= len {}",
                        arr.len()
                    )));
                }
                arr.remove(idx);
                Ok(())
            } else {
                let key = key_of(last)?;
                let obj = node
                    .as_object_mut()
                    .ok_or_else(|| ProtocolError::InvalidState("remove on non-object".into()))?;
                if obj.remove(&key).is_none() {
                    return Err(ProtocolError::InvalidState(format!(
                        "remove missing key {key}"
                    )));
                }
                Ok(())
            }
        }
        other => Err(ProtocolError::InvalidState(format!(
            "unknown patch op: {other}"
        ))),
    }
}

fn descend<'a>(node: &'a mut Value, seg: &Value) -> Result<&'a mut Value, ProtocolError> {
    if let Some(idx) = index_of(seg)? {
        let arr = node
            .as_array_mut()
            .ok_or_else(|| ProtocolError::InvalidState("descend into non-array".into()))?;
        arr.get_mut(idx)
            .ok_or_else(|| ProtocolError::InvalidState(format!("descend index {idx} missing")))
    } else {
        let key = key_of(seg)?;
        node.get_mut(key.as_str())
            .ok_or_else(|| ProtocolError::InvalidState(format!("descend key {key} missing")))
    }
}

fn index_of(seg: &Value) -> Result<Option<usize>, ProtocolError> {
    match seg {
        Value::Number(n) => Ok(Some(
            n.as_u64()
                .ok_or_else(|| ProtocolError::InvalidState("array index must be u64".into()))?
                as usize,
        )),
        Value::String(_) => Ok(None),
        _ => Err(ProtocolError::InvalidState(
            "path segment must be string or number".into(),
        )),
    }
}

fn key_of(seg: &Value) -> Result<String, ProtocolError> {
    match seg {
        Value::String(s) => Ok(s.clone()),
        _ => Err(ProtocolError::InvalidState(
            "object key must be string".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::codex_desktop_ipc::types::{PendingKind, ThreadKey};

    fn key() -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: "00000000-0000-4000-8000-0000000000m1".into(),
        }
    }

    fn lease() -> OwnerLease {
        OwnerLease {
            key: key(),
            client_id: "owner-1".into(),
            epoch: 1,
            generation: 1,
        }
    }

    fn caps() -> CapabilitySet {
        CapabilitySet {
            send: true,
            state: true,
            command: false,
            permissions: true,
            blocking_input: false,
            interrupt: false,
        }
    }

    fn fixture(name: &str) -> Value {
        let path = format!(
            "{}/tests/fixtures/codex-desktop-ipc/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
    }

    /// macos-snapshot.json 的 change 段（apply 的入参形态）。
    fn macos_change() -> Value {
        fixture("macos-snapshot.json")["params"]["change"].clone()
    }

    #[test]
    fn empty_turns_uses_canonical_history() {
        let mut m = ThreadMirror::new(key());
        let eff = m.apply(&lease(), &macos_change()).unwrap();
        assert!(eff.changed);
        assert!(!eff.need_snapshot);
        let p = m.project(caps());
        // turns=[] 是常态：正文来自 turnHistory 正史（内联 items，稳定 item ID）
        assert_eq!(p.messages.len(), 2);
        assert_eq!(p.messages[0].id, "00000000-0000-4000-8000-0000000000it1");
        assert_eq!(p.messages[0].role, "user");
        assert_eq!(p.messages[1].id, "00000000-0000-4000-8000-0000000000it2");
        assert_eq!(p.messages[1].role, "assistant");
        // 完整 pending：permissions 请求（macOS profile permissions=true → actionable）
        assert_eq!(p.pending.len(), 1);
        assert_eq!(p.pending[0].kind, PendingKind::PermissionsApproval);
        assert!(p.pending[0].actionable);
        assert_eq!(p.pending[0].request_key, "n:12");
        assert_eq!(p.runtime_status, "idle");
        assert_eq!(p.revision, Some(209));
        assert!(!p.stale);
    }

    #[test]
    fn real_root_path_patch_updates_mirror() {
        // reviewer 2026-10-17 复现形状：patch 直接对 conversationState 本体寻址
        //（path 无 conversationState 前缀），snapshot 后 replace threadRuntimeStatus.type。
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &macos_change()).unwrap();
        assert_eq!(m.project(caps()).runtime_status, "idle");
        // 先推进到 batch[4] 的基线（revision 212），再应用真实根路径 replace
        let batches = fixture("patches.json")["batches"].clone();
        let batches = batches.as_array().unwrap().clone();
        for idx in [0usize, 1, 3] {
            m.apply(&lease(), &batches[idx]).unwrap();
        }
        let eff = m.apply(&lease(), &batches[4]).unwrap();
        assert!(eff.changed, "真实根路径增量必须可应用");
        assert!(!eff.need_snapshot);
        let p = m.project(caps());
        assert_eq!(p.runtime_status, "active", "runtime_status 增量生效");
        assert!(!p.stale);
        assert!(!m.is_stale());
        assert_eq!(p.revision, Some(213));
    }

    #[test]
    fn patch_batch_is_atomic() {
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &macos_change()).unwrap();
        // 一批：第一条合法（replace 内联 agentMessage 文本）+ 第二条非法（replace
        // 不存在的键）（R1：path 直接对 conversationState 本体寻址，无前缀）
        let batch = serde_json::json!({
            "type": "patches",
            "baseRevision": 209,
            "revision": 210,
            "patches": [
                { "op": "replace",
                  "path": ["turnHistory", "history", "entitiesByKey",
                           "turn:00000000-0000-4000-8000-0000000000tu1", "items", 1, "text"],
                  "value": "更新" },
                { "op": "replace",
                  "path": ["noSuchKey"],
                  "value": 1 }
            ]
        });
        let eff = m.apply(&lease(), &batch).unwrap();
        assert!(!eff.changed, "非法批次不得部分生效");
        assert!(eff.need_snapshot);
        assert!(m.is_stale());
        // 原子性：第一条的效果也不在
        let p = m.project(caps());
        assert_eq!(p.messages[1].text, "TUVis macOS IPC probe OK");
    }

    #[test]
    fn revision_gap_requires_snapshot() {
        let mut m = ThreadMirror::new(key());
        // 先落到 revision=7
        let seed = serde_json::json!({
            "type": "snapshot", "revision": 7, "conversationState": { "turns": [], "requests": [] }
        });
        m.apply(&lease(), &seed).unwrap();
        // 在 revision=7 的镜像上，baseRevision=6 / revision=8 的增量：缺口不应用
        let gap = serde_json::json!({
            "type": "patches", "baseRevision": 6, "revision": 8, "patches": []
        });
        let eff = m.apply(&lease(), &gap).unwrap();
        assert!(eff.need_snapshot);
        assert!(!eff.changed);
        assert!(m.project(caps()).stale);
        assert_eq!(m.revision(), Some(7), "缺口批不推进 revision");
    }

    #[test]
    fn stale_replays_are_ignored_quietly() {
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &macos_change()).unwrap();
        // 低 revision 的重复 snapshot / patch 静默忽略
        let old_snapshot = serde_json::json!({
            "type": "snapshot", "revision": 100, "conversationState": { "turns": [] }
        });
        let eff = m.apply(&lease(), &old_snapshot).unwrap();
        assert!(!eff.changed && !eff.need_snapshot);
        assert_eq!(m.revision(), Some(209), "旧 snapshot 不回退镜像");
        let old_patch = serde_json::json!({
            "type": "patches", "baseRevision": 209, "revision": 209, "patches": []
        });
        let eff = m.apply(&lease(), &old_patch).unwrap();
        assert!(!eff.changed && !eff.need_snapshot);
    }

    #[test]
    fn new_epoch_clears_state_without_revision_comparison() {
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &macos_change()).unwrap();
        assert_eq!(m.revision(), Some(209));
        // 换代 lease（epoch=2）：不比较旧 revision，先清状态等新 snapshot
        let mut lease2 = lease();
        lease2.epoch = 2;
        lease2.generation = 2;
        let old_revision_snapshot = serde_json::json!({
            "type": "snapshot", "revision": 5, "conversationState": { "turns": [] }
        });
        let eff = m.apply(&lease2, &old_revision_snapshot).unwrap();
        assert!(eff.changed, "新 epoch 接受任意 revision 的首个 snapshot");
        assert_eq!(m.revision(), Some(5), "换代后不与旧 revision 比大小");
        assert!(!m.is_stale());
    }

    #[test]
    fn streaming_replace_updates_same_bubble() {
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &macos_change()).unwrap();
        let patches = fixture("patches.json")["batches"][0].clone();
        let eff = m.apply(&lease(), &patches).unwrap();
        assert!(eff.changed);
        let p = m.project(caps());
        assert_eq!(p.messages.len(), 2, "流式 replace 不追加重复气泡");
        assert_eq!(p.messages[1].text, "TUVis macOS IPC probe OK（流式追加）");
        assert_eq!(
            p.messages[1].id, "00000000-0000-4000-8000-0000000000it2",
            "item ID 稳定"
        );
        // batch[1]：新增 turn；batch[3]：remove pending（跳过 batch[2] 缺口样本）
        let batches = fixture("patches.json")["batches"].clone();
        let batches = batches.as_array().unwrap().clone();
        for idx in [1usize, 3] {
            let eff = m.apply(&lease(), &batches[idx]).unwrap();
            assert!(eff.changed, "顺序批 {idx} 应可应用");
        }
        let p = m.project(caps());
        assert!(p.pending.is_empty(), "remove 后 pending 消失");
        assert_eq!(p.messages.len(), 4, "新 turn 的内联 items 入墙（无重复）");
        assert_eq!(p.revision, Some(212));
    }

    #[test]
    fn projection_tail_window_marks_truncated() {
        // 构造 600 条消息的快照：截到 500 条并标 truncated（真实内联 items 形态）
        let mut entities = serde_json::Map::new();
        let mut entries = Vec::new();
        for i in 0..600 {
            let turn_id = format!("t{i}");
            let item_id = format!("i{i}");
            entries.push(serde_json::json!({ "key": turn_id, "value": turn_id }));
            entities.insert(
                turn_id.clone(),
                serde_json::json!({ "turnId": turn_id, "status": "completed",
                                    "items": [ { "type": "userMessage", "id": item_id,
                                        "content": [ { "type": "text", "text": "m" } ] } ] }),
            );
        }
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [],
                "requests": [],
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true, "entitiesByKey": Value::Object(entities),
                                 "islands": [{ "entries": entries }] } }
            }
        });
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        let p = m.project(caps());
        assert_eq!(p.messages.len(), 500);
        assert!(p.truncated);
        assert_eq!(p.messages[0].id, "i100", "保留尾窗（最新 500 条）");
        assert_eq!(p.messages[499].id, "i599");
    }

    #[test]
    fn oversized_newest_message_is_head_truncated_not_dropped() {
        // 单条正文自身超 1 MiB（agentMessage 贴大日志/大代码块真实可达）：投影
        // 不得清成 0 条——保底保留最新 1 条，正文截头 + 显式标记，truncated 置位
        let original_chars = PROJECTION_TAIL_BYTES / 3 + 1000;
        let body = "汉".repeat(original_chars); // 3 B/char → 字节数 > 1 MiB
        assert!(body.len() > PROJECTION_TAIL_BYTES);
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [], "requests": [],
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true,
                        "entitiesByKey": {
                            "turn:t1": { "turnId": "t1", "status": "completed",
                                "items": [ { "type": "agentMessage", "id": "big1",
                                             "text": body } ] }
                        },
                        "islands": [{ "entries": [ { "key": "turn:t1",
                                                     "value": "turn:t1" } ] }]
                    } }
            }
        });
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        let p = m.project(caps());
        assert_eq!(
            p.messages.len(),
            1,
            "超窗最新条必须保底保留，不得投影成 0 条"
        );
        assert!(p.truncated, "截头分支必须置位 truncated");
        let text = &p.messages[0].text;
        // 展示截断标记沿用 `DISPLAY_TRUNCATED: ` 前缀（错误码对账门禁要求自报码）；
        // 头部原文紧随其后。
        const MARK: &str = "DISPLAY_TRUNCATED: ";
        assert!(text.starts_with(MARK), "截断标记前缀: {text}");
        assert!(text.contains('汉'), "保留头部原文");
        assert!(text.len() < body.len(), "正文确实被截短");
        assert!(text.len() < PROJECTION_TAIL_BYTES, "截头后落回尾窗预算内");
        let expected_tail = format!("…已截断，原文共 {original_chars} 字符");
        assert!(text.ends_with(&expected_tail), "显式自报原文总字符数");
        // 64 KiB 切点不在 3 B 字符边界 → 回退对齐后头部必须是整字符（无半个字符）
        let head_len = text.len() - MARK.len() - expected_tail.len();
        assert_eq!(head_len % 3, 0, "切点落在字符边界");
    }

    #[test]
    fn byte_tail_window_drops_only_older_messages() {
        // 多条累计超 1 MiB：旧条被裁、最新条整条保留（维持既有丢弃逻辑，
        // 不触发单条截头——3×400 KB 才破窗，最新 2 条 < 1 MiB）
        let body = "a".repeat(400_000);
        let mut entities = serde_json::Map::new();
        let mut entries = Vec::new();
        for i in 0..5 {
            let turn_id = format!("t{i}");
            entries.push(serde_json::json!({ "key": turn_id, "value": turn_id }));
            entities.insert(
                turn_id.clone(),
                serde_json::json!({ "turnId": turn_id, "status": "completed",
                                    "items": [ { "type": "agentMessage",
                                        "id": format!("m{i}"), "text": body } ] }),
            );
        }
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [], "requests": [],
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true, "entitiesByKey": entities,
                                 "islands": [{ "entries": entries }] } }
            }
        });
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        let p = m.project(caps());
        assert_eq!(p.messages.len(), 2, "累计超窗只裁旧条：保留最新 2 条");
        assert_eq!(p.messages[0].id, "m3");
        assert_eq!(p.messages[1].id, "m4");
        assert_eq!(p.messages[1].text.len(), 400_000, "最新条整条保留不截头");
        assert!(!p.messages[1].text.contains("已截断"), "无单条截断标记");
        assert!(p.truncated, "裁旧条必须置位 truncated");
    }

    #[test]
    fn global_mirror_budget_bounds_all_mirrors() {
        // C4：全体总预算——两镜像合计超上限 → 后到者 Capacity + stale；内存释放后
        // 可再收（不靠 OOM 证明有界）
        let ledger = Arc::new(std::sync::Mutex::new(MirrorLedger::with_cap(300)));
        let mut a = ThreadMirror::with_ledger(key(), ledger.clone());
        let mut b = ThreadMirror::with_ledger(
            ThreadKey {
                host_id: "local".into(),
                thread_id: "00000000-0000-4000-8000-0000000000m2".into(),
            },
            ledger.clone(),
        );
        let snap = |rev: u64, blob: &str| {
            serde_json::json!({
                "type": "snapshot", "revision": rev,
                "conversationState": { "turns": [], "requests": [], "big": blob }
            })
        };
        a.apply(&lease(), &snap(1, &"x".repeat(200))).unwrap();
        let after_a = ledger.lock().unwrap().total();
        // B 的 200B 会让合计超 300 → 拒绝（账目不变）
        let err = b.apply(&lease(), &snap(1, &"y".repeat(200))).unwrap_err();
        assert!(matches!(err, ProtocolError::Capacity(_)));
        assert!(b.is_stale());
        assert_eq!(ledger.lock().unwrap().total(), after_a, "拒绝不部分生效");
        // A 缩小 → B 可再收（合计仍 < 上限）
        a.apply(&lease(), &snap(2, "small")).unwrap();
        b.apply(&lease(), &snap(2, &"y".repeat(200))).unwrap();
        let after_b = ledger.lock().unwrap().total();
        assert!(after_b < 300 && after_b > 200, "合计有界：{after_b}");
        // Drop 释放占用（账目只减不增）
        drop(a);
        let after_drop = ledger.lock().unwrap().total();
        assert!(after_drop < after_b, "Drop 释放：{after_drop} < {after_b}");
    }

    #[test]
    fn oversized_mirror_is_rejected_and_stale() {
        let blob = "x".repeat(MIRROR_BUDGET + 1);
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": { "turns": [], "requests": [], "big": blob }
        });
        let mut m = ThreadMirror::new(key());
        let err = m.apply(&lease(), &change).unwrap_err();
        assert!(matches!(err, ProtocolError::Capacity(_)));
        assert!(m.is_stale());
    }

    #[test]
    fn turn_terminal_evidence_requires_the_bound_turn_itself() {
        // 2026-10-17 复审 C1：终态证据按 **turnId 精确查询**——本 turn inProgress、
        // 实体不在场、镜像 stale 一律不是证据；跨 turn「完成不互为证据」的负例在
        // router 层钉死（release 只查绑定 id，见 codex_control_remote 的
        // turn_restriction_needs_matching_completion_evidence）。
        // 2026-10-18 形态适配：实体按真实形态（无 type、turnId 字段、entries.value
        // 引用、键与 id 解耦）。
        let mut m = ThreadMirror::new(key());
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [], "requests": [],
                "threadRuntimeStatus": { "type": "idle" },
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true,
                    "entitiesByKey": {
                        "turn:mine": { "turnId": "turn-mine",
                                       "status": "inProgress", "items": [] },
                        "turn:other": { "turnId": "turn-other",
                                        "status": "completed", "items": [] }
                    },
                    "islands": [{ "entries": [
                        { "key": "turn:mine", "value": "turn:mine" },
                        { "key": "turn:other", "value": "turn:other" }
                    ] }]
                } }
            }
        });
        m.apply(&lease(), &change).unwrap();
        assert!(
            !m.turn_is_terminal("turn-mine"),
            "本 turn inProgress 不是证据"
        );
        assert!(
            m.turn_is_terminal("turn-other"),
            "turn-other 自身已终态（对它自己的绑定就是证据——精确按 id，不模糊）"
        );
        assert!(!m.turn_is_terminal("turn-absent"), "实体不在场不是证据");
        // 本 turn 转 completed → 证据成立
        let done = serde_json::json!({
            "type": "patches", "baseRevision": 1, "revision": 2,
            "patches": [
                { "op": "replace",
                  "path": ["turnHistory", "history", "entitiesByKey", "turn:mine", "status"],
                  "value": "completed" }
            ]
        });
        m.apply(&lease(), &done).unwrap();
        assert!(
            m.turn_is_terminal("turn-mine"),
            "绑定 turn 以终态在场 = 证据"
        );
        // failed 也是终态
        assert_eq!(m.turn_status("turn-mine").as_deref(), Some("completed"));
        // stale 镜像一律无证据
        m.mark_stale();
        assert!(!m.turn_is_terminal("turn-mine"), "stale 镜像不出证据");
    }

    /// 打断后的回合（status=interrupted）**是终态**——2026-10-21 远程打断闭环的
    /// 关键一环：打断 → Desktop 把回合状态置 `interrupted` → 终态门放行 → 屏障释放
    /// → 队列接力。若不认它，打断后屏障永久在册（比打断前更糟：用户以为已解围）。
    #[test]
    fn interrupted_turn_counts_as_terminal_evidence() {
        let mut m = ThreadMirror::new(key());
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [], "requests": [],
                "threadRuntimeStatus": { "type": "active" },
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true,
                    "entitiesByKey": {
                        "turn:mine": { "turnId": "turn-mine",
                                       "status": "interrupted", "items": [] }
                    },
                    "islands": [{ "entries": [
                        { "key": "turn:mine", "value": "turn:mine" }
                    ] }]
                } }
            }
        });
        m.apply(&lease(), &change).unwrap();
        assert_eq!(m.turn_status("turn-mine").as_deref(), Some("interrupted"));
        assert!(
            m.turn_is_terminal("turn-mine"),
            "被打断的回合是终态——屏障据此释放"
        );
        // inProgress 仍不是终态（对照）
        let mut m2 = ThreadMirror::new(key());
        let running = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [], "requests": [],
                "threadRuntimeStatus": { "type": "active" },
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true,
                    "entitiesByKey": {
                        "turn:mine": { "turnId": "turn-mine",
                                       "status": "inProgress", "items": [] }
                    },
                    "islands": [{ "entries": [
                        { "key": "turn:mine", "value": "turn:mine" }
                    ] }]
                } }
            }
        });
        m2.apply(&lease(), &running).unwrap();
        assert!(!m2.turn_is_terminal("turn-mine"), "进行中的回合不是终态");
    }

    /// `active_turn_id`（2026-10-22 原生回合打断扩展）：inProgress 回合 → Some；
    /// stale 镜像一律 None（阀值回退的保守方向——过期镜像不给打断阀值，复审
    /// Nit-4 钉死）。端点 409 负例由集成测试 refused_without_valve 覆盖。
    #[test]
    fn active_turn_id_follows_inprogress_and_stale_is_none() {
        let mut m = ThreadMirror::new(key());
        let running = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [], "requests": [],
                "threadRuntimeStatus": { "type": "active" },
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true,
                    "entitiesByKey": {
                        "turn:done": { "turnId": "turn-done",
                                       "status": "completed", "items": [] },
                        "turn:live": { "turnId": "turn-live",
                                       "status": "inProgress", "items": [] }
                    },
                    "islands": [{ "entries": [
                        { "key": "turn:done", "value": "turn:done" },
                        { "key": "turn:live", "value": "turn:live" }
                    ] }]
                } }
            }
        });
        m.apply(&lease(), &running).unwrap();
        assert_eq!(
            m.active_turn_id().as_deref(),
            Some("turn-live"),
            "completed 不算活动，inProgress 的最新回合才去"
        );
        m.mark_stale();
        assert!(m.active_turn_id().is_none(), "stale 镜像不给阀值（保守）");
    }

    #[test]
    fn unknown_item_kind_projects_safe_summary_only() {
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [],
                "requests": [],
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true,
                        "entitiesByKey": {
                            "turn:t1": { "turnId": "t1", "status": "completed",
                                    "items": [ { "type": "mcp_tool_call", "id": "raw1",
                                        "arguments": { "cmd": "secret-cmd", "token": "TKN" } } ] }
                        },
                        "islands": [{ "entries": [ { "key": "turn:t1", "value": "turn:t1" } ] }]
                    } }
            }
        });
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        let p = m.project(caps());
        assert_eq!(p.messages.len(), 1);
        assert_eq!(p.messages[0].role, "system");
        assert_eq!(p.messages[0].kind, "mcp_tool_call");
        let rendered = serde_json::to_string(&p.messages[0]).unwrap();
        assert!(!rendered.contains("secret-cmd"), "原始参数不下发");
        assert!(!rendered.contains("TKN"), "参数值不下发");
    }

    // ===== contentState（内容健康状态，与写能力正交）=====

    #[test]
    fn content_state_ready_only_when_turns_and_items_complete() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("canonical-before-load.sanitized.json"),
        )
        .unwrap();
        assert_eq!(m.project(caps()).content_state, ContentState::Ready);
        // history.isComplete=false → Loading（hydrate 判据同源）
        let mut incomplete = observed_change("canonical-before-load.sanitized.json");
        incomplete["conversationState"]["turnHistory"]["history"]["isComplete"] =
            serde_json::json!(false);
        let mut m2 = ThreadMirror::new(key());
        m2.apply(&lease(), &incomplete).unwrap();
        assert_eq!(m2.project(caps()).content_state, ContentState::Loading);
        // Turn 列表完整但 items 分页未载完 → Partial
        let mut partial = observed_change("canonical-before-load.sanitized.json");
        partial["conversationState"]["turnHistory"]["history"]["entitiesByKey"]
            ["turn:00000000-0000-4000-8000-000000000002"]["itemsPagination"] =
            serde_json::json!({ "hasLoadedOldest": false, "isLoadingOlder": false });
        let mut m3 = ThreadMirror::new(key());
        m3.apply(&lease(), &partial).unwrap();
        assert_eq!(m3.project(caps()).content_state, ContentState::Partial);
        // 无 turnHistory.history（形态不可读）→ Unsupported（不冒充空历史）
        let mut broken = observed_change("canonical-before-load.sanitized.json");
        broken["conversationState"]
            .as_object_mut()
            .unwrap()
            .remove("turnHistory");
        let mut m4 = ThreadMirror::new(key());
        m4.apply(&lease(), &broken).unwrap();
        assert_eq!(m4.project(caps()).content_state, ContentState::Unsupported);
        // 尚无任何 snapshot → Loading
        let empty = ThreadMirror::new(key());
        assert_eq!(empty.project(caps()).content_state, ContentState::Loading);
    }

    // ===== 2026-10-18 真机金样（observed/）：canonical 实体形态适配 =====
    // 证据来源与形态事实见 observed/README.md。以下红→绿基线针对本轮修复：
    // 实体按合成键索引（turn:/tail: 前缀）、turn 实体无 type 以 turnId 字段标识、
    // islands.entries 为 {key,value} 对象、items 为内联 camelCase 对象、
    // isComplete 在 history 层。派生负例（改字段构造）在测试名以 derived 标注。

    fn observed_change(name: &str) -> Value {
        fixture(&format!("observed/{name}"))
    }

    #[test]
    fn observed_golden_target_turn_terminal_evidence() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("canonical-before-load.sanitized.json"),
        )
        .unwrap();
        // 目标 turn 实体键是 turn:<uuid>：证据按实体 turnId 字段认领（键不是 id）
        assert_eq!(
            m.turn_status("00000000-0000-4000-8000-000000000006")
                .as_deref(),
            Some("completed"),
            "真实形态下必须能按 turnId 字段读到终态（旧读者查键恒 None）"
        );
        assert!(
            m.turn_is_terminal("00000000-0000-4000-8000-000000000006"),
            "屏障放行的完成证据在真机形态下必须成立"
        );
        // tail 合成键实体：键尾 UUID(…0021) ≠ turnId(…0011)——只能按字段认领
        assert_eq!(
            m.turn_status("00000000-0000-4000-8000-000000000011")
                .as_deref(),
            Some("completed"),
            "从键尾猜业务 ID 必然失配，按字段必中"
        );
        assert!(!m.turn_is_terminal("00000000-0000-4000-8000-000000000099"));
    }

    #[test]
    fn observed_golden_projection_inline_camelcase_items() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("canonical-before-load.sanitized.json"),
        )
        .unwrap();
        let p = m.project(caps());
        let conversation: Vec<(String, String, String)> = p
            .messages
            .iter()
            .filter(|msg| msg.role != "system")
            .map(|msg| (msg.id.clone(), msg.role.clone(), msg.text.clone()))
            .collect();
        assert_eq!(
            conversation.len(),
            6,
            "6 条用户/助手正文（reasoning 过滤；error 走系统摘要不计正文）"
        );
        let ids: Vec<&str> = p.messages.iter().map(|msg| msg.id.as_str()).collect();
        // Task 6 起：有 `params.input` 且正式 userMessage.clientId 与之相等的**逻辑
        // 开场消息**使用 turnId+clientUserMessageId 的投影身份（保证「后备 → 正式」同
        // 一条消息）；其余 item 仍用 item.id。canonical 顺序与 error 摘要不变。
        assert_eq!(
            ids,
            [
                "00000000-0000-4000-8000-000000000002/opening/00000000-0000-4000-8000-000000000001",
                "00000000-0000-4000-8000-000000000004",
                "00000000-0000-4000-8000-000000000006/opening/00000000-0000-4000-8000-000000000005",
                "00000000-0000-4000-8000-000000000009",
                "00000000-0000-4000-8000-000000000011/opening/00000000-0000-4000-8000-000000000010",
                "00000000-0000-4000-8000-000000000013",
                "00000000-0000-4000-8000-000000000014",
                "00000000-0000-4000-8000-000000000015",
                "00000000-0000-4000-8000-000000000016",
                "00000000-0000-4000-8000-000000000017",
            ],
            "逻辑开场 ID（opening）与 item.id 两类标识，canonical 顺序（error 保留为系统摘要）"
        );
        assert_eq!(conversation[0].1, "user");
        assert_eq!(conversation[0].2, "Synthetic user 1");
        assert_eq!(conversation[1].1, "assistant");
        assert_eq!(conversation[1].2, "Synthetic reply 1");
        assert_eq!(conversation[5].2, "Synthetic reply 3");
        assert!(p.messages[5].kind == "error" && p.messages[5].role == "system");
        let rendered = serde_json::to_string(&p).unwrap();
        assert!(!rendered.contains("text_elements"), "块级包装字段不下发");
    }

    #[test]
    fn observed_golden_reload_snapshot_keeps_ids_stable() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("canonical-before-load.sanitized.json"),
        )
        .unwrap();
        let first = m.project(caps());
        // load-complete-history 后的重放 snapshot（rev 4）：同内容不得追加重复气泡
        m.apply(
            &lease(),
            &observed_change("canonical-after-load.sanitized.json"),
        )
        .unwrap();
        let second = m.project(caps());
        assert_eq!(second.messages.len(), first.messages.len(), "reload 不追加");
        assert_eq!(second.messages, first.messages, "id 与顺序稳定");
    }

    #[test]
    fn observed_derived_busy_fallback_sees_real_inprogress_turn() {
        // 派生负例：真实结构 + turn 0011 置 inProgress（SYNTHETIC derived）
        let mut change = observed_change("canonical-before-load.sanitized.json");
        change["conversationState"]["turnHistory"]["history"]["entitiesByKey"]
            ["tail:0:local:00000000-0000-4000-8000-000000000021"]["status"] =
            serde_json::json!("inProgress");
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        let p = m.project(caps());
        assert_eq!(
            p.runtime_status, "active",
            "真实 inProgress turn 必须计入忙态（旧读者要求 type=turn 恒漏）"
        );
    }

    #[test]
    fn observed_derived_patch_recomputes_completeness() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("canonical-before-load.sanitized.json"),
        )
        .unwrap();
        assert!(m.is_history_complete());
        // 派生 patch：history.isComplete → false；采用后必须按新 state 重算
        let patch = serde_json::json!({
            "type": "patches", "baseRevision": 3, "revision": 4,
            "patches": [
                { "op": "replace",
                  "path": ["turnHistory", "history", "isComplete"],
                  "value": false }
            ]
        });
        m.apply(&lease(), &patch).unwrap();
        assert!(
            !m.is_history_complete(),
            "patch 采用后完整性必须从新 state 重算（不得沿用旧缓存 true）"
        );
    }

    #[test]
    fn observed_derived_missing_iscomplete_is_not_complete() {
        // 派生：删去 history.isComplete——缺字段 ≠ 完整
        let mut change = observed_change("canonical-before-load.sanitized.json");
        change["conversationState"]["turnHistory"]["history"]
            .as_object_mut()
            .unwrap()
            .remove("isComplete");
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        assert!(
            !m.is_history_complete(),
            "缺 isComplete 不得默认完整（ hydrate 触发判据）"
        );
    }

    #[test]
    fn observed_derived_unreferenced_entity_is_not_evidence() {
        // 派生：目标 turnId 只出现在未被 islands 引用的孤立实体上——不放行
        let mut change = observed_change("canonical-before-load.sanitized.json");
        let hist = &mut change["conversationState"]["turnHistory"]["history"];
        let entities = hist["entitiesByKey"].as_object_mut().unwrap();
        let target = entities["turn:00000000-0000-4000-8000-000000000006"].clone();
        entities.remove("turn:00000000-0000-4000-8000-000000000006");
        entities.insert("orphan".into(), target);
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        assert!(
            !m.turn_is_terminal("00000000-0000-4000-8000-000000000006"),
            "未被 canonical 引用的孤立实体不得作为完成证据"
        );
    }

    /// 在 observed 金样上为目标 Turn 追加一个副本（同 turnId，可指定 status/items；
    /// `prepend` 把副本引用插到 canonical 序列最前——顺序敏感性回归用）。
    fn with_target_copy(prepend: bool, status: Option<&str>, items: Value) -> Value {
        let mut change = observed_change("canonical-before-load.sanitized.json");
        let hist = &mut change["conversationState"]["turnHistory"]["history"];
        {
            let entities = hist["entitiesByKey"].as_object_mut().unwrap();
            let mut entity = serde_json::json!({
                "turnId": "00000000-0000-4000-8000-000000000006",
                "items": items,
            });
            if let Some(st) = status {
                entity["status"] = serde_json::json!(st);
            }
            entities.insert("tail:9:local:dupref".into(), entity);
        }
        let entries = hist["islands"][0]["entries"].as_array_mut().unwrap();
        let reference = serde_json::json!({
            "key": "tail:9:local:dupref", "value": "tail:9:local:dupref"
        });
        if prepend {
            entries.insert(0, reference);
        } else {
            entries.push(reference);
        }
        change
    }

    #[test]
    fn observed_derived_duplicate_turn_reference_dedup_and_ambiguity() {
        // 报告 §4：两个被引用实体**都携带同一个 item ID**——只有生产去重逻辑
        // 在场才能通过本条（旧断言只在副本里放 dup-item，删掉 seen_ids 也能绿）
        let shared = serde_json::json!([{ "type": "agentMessage", "id": "dup-item",
                                          "text": "重复气泡候选" }]);
        let mut change = observed_change("canonical-before-load.sanitized.json");
        {
            let hist = &mut change["conversationState"]["turnHistory"]["history"];
            let entities = hist["entitiesByKey"].as_object_mut().unwrap();
            entities["turn:00000000-0000-4000-8000-000000000006"]["items"] = shared.clone();
            entities.insert(
                "tail:9:local:dupref".into(),
                serde_json::json!({
                    "turnId": "00000000-0000-4000-8000-000000000006",
                    "status": "completed",
                    "items": shared,
                }),
            );
            let entries = hist["islands"][0]["entries"].as_array_mut().unwrap();
            entries.push(serde_json::json!({
                "key": "tail:9:local:dupref", "value": "tail:9:local:dupref"
            }));
        }
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        assert!(m.turn_is_terminal("00000000-0000-4000-8000-000000000006"));
        let p = m.project(caps());
        let dup: Vec<_> = p
            .messages
            .iter()
            .filter(|msg| msg.id == "dup-item")
            .collect();
        assert_eq!(dup.len(), 1, "双引用重叠 items 按去重，不追加重复气泡");

        // 空占位副本在前 + 原体正文在后：去重不得吞掉原体已加载的正文（顺序断言）
        let mut ordered = observed_change("canonical-before-load.sanitized.json");
        {
            let hist = &mut ordered["conversationState"]["turnHistory"]["history"];
            let entities = hist["entitiesByKey"].as_object_mut().unwrap();
            entities.insert(
                "tail:9:local:emptycopy".into(),
                serde_json::json!({
                    "turnId": "00000000-0000-4000-8000-000000000011",
                    "items": []
                }),
            );
            let entries = hist["islands"][0]["entries"].as_array_mut().unwrap();
            let pos = entries
                .iter()
                .position(|e| e["value"] == "tail:0:local:00000000-0000-4000-8000-000000000021")
                .expect("tail turn entry");
            entries.insert(
                pos,
                serde_json::json!({
                    "key": "tail:9:local:emptycopy", "value": "tail:9:local:emptycopy"
                }),
            );
        }
        let mut m_ordered = ThreadMirror::new(key());
        m_ordered.apply(&lease(), &ordered).unwrap();
        let p2 = m_ordered.project(caps());
        assert_eq!(
            p2.messages
                .iter()
                .filter(|msg| msg
                    .id
                    .ends_with("/opening/00000000-0000-4000-8000-000000000010"))
                .count(),
            1,
            "空占位副本在前不得遮蔽/重复原体已加载正文（Task 6 起该逻辑消息用 opening ID）"
        );
    }

    #[test]
    fn observed_derived_multi_copy_status_is_order_independent() {
        // 报告 2.1（P1）：缺 status 副本出现在前/后必须同判——多义一律无证据
        for prepend in [true, false] {
            let mut m = ThreadMirror::new(key());
            m.apply(
                &lease(),
                &with_target_copy(prepend, None, serde_json::json!([])),
            )
            .unwrap();
            assert!(
                !m.turn_is_terminal("00000000-0000-4000-8000-000000000006"),
                "缺 status 副本（prepend={prepend}）在场即多义，不得按另一副本放行"
            );
            let mut m2 = ThreadMirror::new(key());
            m2.apply(
                &lease(),
                &with_target_copy(prepend, Some("inProgress"), serde_json::json!([])),
            )
            .unwrap();
            assert!(
                !m2.turn_is_terminal("00000000-0000-4000-8000-000000000006"),
                "状态冲突（prepend={prepend}）不得放行"
            );
            let mut m3 = ThreadMirror::new(key());
            m3.apply(
                &lease(),
                &with_target_copy(prepend, Some("completed"), serde_json::json!([])),
            )
            .unwrap();
            assert!(
                m3.turn_is_terminal("00000000-0000-4000-8000-000000000006"),
                "多副本同 completed 可认定终态（prepend={prepend}）"
            );
        }
    }

    #[test]
    fn observed_derived_structure_errors_are_not_ready_empty() {
        // 报告 2.3：结构错误不得 Ready + 0 条（不冒充真空会话）
        let mut bad_entries = observed_change("canonical-before-load.sanitized.json");
        bad_entries["conversationState"]["turnHistory"]["history"]["islands"][0]["entries"] =
            serde_json::json!(["not-an-object"]);
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &bad_entries).unwrap();
        assert_eq!(
            m.project(caps()).content_state,
            ContentState::Unsupported,
            "非法 entries 必须 Unsupported，不得 Ready+0 条"
        );
        let mut bad_items = observed_change("canonical-before-load.sanitized.json");
        bad_items["conversationState"]["turnHistory"]["history"]["entitiesByKey"]
            ["turn:00000000-0000-4000-8000-000000000002"]["items"] =
            serde_json::json!("not-an-array");
        let mut m2 = ThreadMirror::new(key());
        m2.apply(&lease(), &bad_items).unwrap();
        assert_eq!(m2.project(caps()).content_state, ContentState::Unsupported);
        let mut dangling = observed_change("canonical-before-load.sanitized.json");
        dangling["conversationState"]["turnHistory"]["history"]["islands"][0]["entries"][0]
            ["value"] = serde_json::json!("no-such-entity");
        let mut m3 = ThreadMirror::new(key());
        m3.apply(&lease(), &dangling).unwrap();
        assert_eq!(m3.project(caps()).content_state, ContentState::Unsupported);
        let mut not_loaded = observed_change("canonical-before-load.sanitized.json");
        not_loaded["conversationState"]["turnHistory"]["history"]["entitiesByKey"]
            ["turn:00000000-0000-4000-8000-000000000002"]["itemsView"] =
            serde_json::json!("notLoaded");
        let mut m4 = ThreadMirror::new(key());
        m4.apply(&lease(), &not_loaded).unwrap();
        assert_eq!(m4.project(caps()).content_state, ContentState::Partial);
        // 合法空 canonical 历史（真空会话）仍 Ready
        let empty = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [], "requests": [],
                "threadRuntimeStatus": { "type": "idle" },
                "turnHistory": { "kind": "canonical", "history": {
                    "generation": 0, "isComplete": true,
                    "entitiesByKey": {}, "islands": [{ "entries": [] }]
                } }
            }
        });
        let mut m5 = ThreadMirror::new(key());
        m5.apply(&lease(), &empty).unwrap();
        let p = m5.project(caps());
        assert_eq!(p.content_state, ContentState::Ready, "合法空历史=Ready");
        assert!(p.messages.is_empty());
    }

    #[test]
    fn observed_derived_items_incomplete_needs_hydration() {
        // 报告 2.2：列表完整 + items 未加载 → needs_hydration 必须为真
        let mut change = observed_change("canonical-before-load.sanitized.json");
        change["conversationState"]["turnHistory"]["history"]["entitiesByKey"]
            ["turn:00000000-0000-4000-8000-000000000002"]["itemsPagination"] =
            serde_json::json!({ "hasLoadedOldest": false, "isLoadingOlder": false });
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        assert!(m.is_history_complete(), "列表完整性不变");
        assert!(
            m.needs_hydration(),
            "items 未加载必须触发只读 hydrate（旧判据只看列表完整性）"
        );
        // 目标终态证据不受其他 Turn 正文缺口影响
        assert!(m.turn_is_terminal("00000000-0000-4000-8000-000000000006"));
    }

    #[test]
    fn observed_derived_entry_reference_is_value_not_key() {
        // CODE 派生（Desktop 自身读法）：引用取 entry.value；key 不参与查找
        let mut change = observed_change("canonical-before-load.sanitized.json");
        {
            let entries = change["conversationState"]["turnHistory"]["history"]["islands"][0]
                ["entries"]
                .as_array_mut()
                .unwrap();
            let target = entries.remove(1); // turn:…006
            assert_eq!(target["value"], "turn:00000000-0000-4000-8000-000000000006");
            // key 改成无关值、value 不变 → 仍按 value 引用
            entries.insert(
                1,
                serde_json::json!({ "key": "unrelated-ref", "value": target["value"] }),
            );
        }
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        assert!(
            m.turn_is_terminal("00000000-0000-4000-8000-000000000006"),
            "entry.key 与 value 不同时按 value 引用（Desktop 同款读法）"
        );
        // value 指向不存在的实体 → 无证据（key 命中也不算）
        {
            let entries = change["conversationState"]["turnHistory"]["history"]["islands"][0]
                ["entries"]
                .as_array_mut()
                .unwrap();
            entries[1] = serde_json::json!({ "key": "turn:00000000-0000-4000-8000-000000000006",
                                             "value": "no-such-entity" });
        }
        let mut m2 = ThreadMirror::new(key());
        m2.apply(&lease(), &change).unwrap();
        assert!(
            !m2.turn_is_terminal("00000000-0000-4000-8000-000000000006"),
            "引用失效 = 无证据；key 不兜底"
        );
    }

    // ============================================================
    // Task 6：已有 Turn 开场用户消息的显示后备
    // ============================================================

    /// 合成 canonical state（键 = "turn:<id>"，实体按 turnId 字段认领）
    fn opening_state(turns: Vec<(&str, Value, Value)>) -> Value {
        let mut map = serde_json::Map::new();
        let mut entries = Vec::new();
        for (tid, params, items) in turns {
            let key = format!("turn:{tid}");
            map.insert(
                key.clone(),
                serde_json::json!({
                    "turnId": tid,
                    "status": "inProgress",
                    "params": params,
                    "items": items,
                }),
            );
            entries.push(serde_json::json!({ "key": key, "value": key }));
        }
        serde_json::json!({
            "turnHistory": {
                "kind": "canonical",
                "history": {
                    "isComplete": true,
                    "entitiesByKey": Value::Object(map),
                    "islands": [{ "entries": entries }],
                }
            }
        })
    }

    fn project_of(state: Value) -> ThreadProjection {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &serde_json::json!({ "type": "snapshot", "revision": 1, "conversationState": state }),
        )
        .unwrap();
        m.project(caps())
    }

    fn input_params(client: &str, text: &str) -> Value {
        serde_json::json!({
            "clientUserMessageId": client,
            "input": [{ "type": "text", "text": text }],
        })
    }

    /// 正式用户项到达后，同一逻辑消息沿用开场投影 ID（后备 → 正式替换，不追加第二条）
    #[test]
    fn opening_input_then_user_message_keeps_one_identity() {
        let fallback = project_of(opening_state(vec![(
            "t1",
            input_params("c1", "hello"),
            serde_json::json!([]),
        )]));
        assert_eq!(fallback.messages.len(), 1, "只有开场后备一条");
        assert_eq!(fallback.messages[0].id, "t1/opening/c1");
        assert_eq!(fallback.messages[0].role, "user");
        assert_eq!(fallback.messages[0].text, "hello");

        let formal = project_of(opening_state(vec![(
            "t1",
            input_params("c1", "hello"),
            serde_json::json!([{
                "type": "userMessage",
                "id": "m1",
                "clientId": "c1",
                "content": [{ "type": "text", "text": "hello" }],
            }]),
        )]));
        assert_eq!(formal.messages.len(), 1, "正式项压住后备，不重复");
        assert_eq!(
            formal.messages[0].id, "t1/opening/c1",
            "同 clientUserMessageId ⇒ 同一投影身份（replace 不追加气泡）"
        );
        assert_eq!(formal.messages[0].text, "hello");
    }

    /// 跨 Turn 相同正文不合并（ID 含 turnId，天然不同）
    #[test]
    fn opening_does_not_merge_equal_text_across_turns() {
        let p = project_of(opening_state(vec![
            ("t1", input_params("c1", "same"), serde_json::json!([])),
            ("t2", input_params("c2", "same"), serde_json::json!([])),
        ]));
        let users: Vec<&str> = p
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.text.as_str())
            .collect();
        assert_eq!(users, ["same", "same"], "两条独立开场，不按正文去重");
    }

    /// null turnId / 未被引用的孤立实体不产生用户后备
    #[test]
    fn opening_ignores_null_turn_and_unreferenced_entity() {
        let state = serde_json::json!({
            "turnHistory": {
                "kind": "canonical",
                "history": {
                    "isComplete": true,
                    "entitiesByKey": {
                        "turn:null": {
                            "turnId": null,
                            "params": {
                                "clientUserMessageId": "c1",
                                "input": [{ "type": "text", "text": "orphan" }]
                            },
                            "items": []
                        },
                        "turn:orphan": {
                            "turnId": "orphan",
                            "params": {
                                "clientUserMessageId": "c2",
                                "input": [{ "type": "text", "text": "unreferenced" }]
                            },
                            "items": []
                        }
                    },
                    "islands": [{ "entries": [] }]
                }
            }
        });
        let p = project_of(state);
        assert!(
            p.messages.is_empty(),
            "未验明身份的实体不得产出用户后备: {:?}",
            p.messages
        );
    }

    /// 键迁移副本：同一 turnId 两实体，一个只有开场、一个已有正式项 → 正式项胜出且仅一条
    #[test]
    fn opening_formal_copy_wins_during_key_migration() {
        let mut state = opening_state(vec![(
            "t1",
            input_params("c1", "hello"),
            serde_json::json!([]),
        )]);
        // 第二份副本（另一个键）带正式 userMessage
        let entities = state["turnHistory"]["history"]["entitiesByKey"]
            .as_object_mut()
            .unwrap();
        entities.insert(
            "tail:0:local:t1".to_string(),
            serde_json::json!({
                "turnId": "t1",
                "status": "inProgress",
                "params": input_params("c1", "hello"),
                "items": [{
                    "type": "userMessage",
                    "id": "m1",
                    "clientId": "c1",
                    "content": [{ "type": "text", "text": "hello" }],
                }],
            }),
        );
        let entries = state["turnHistory"]["history"]["islands"][0]["entries"]
            .as_array_mut()
            .unwrap();
        entries.push(serde_json::json!({ "key": "tail:0:local:t1", "value": "tail:0:local:t1" }));

        let p = project_of(state);
        let users: Vec<&ProjectedMessage> =
            p.messages.iter().filter(|m| m.role == "user").collect();
        assert_eq!(users.len(), 1, "跨键副本不重复: {users:?}");
        assert_eq!(users[0].id, "t1/opening/c1");
        assert_eq!(users[0].text, "hello");
    }

    /// 派发边界快照的 Turn 集合（2026-10-20 复审 P1）：canonical 引用链上的 Turn
    /// 必须全部收进 `turn_ids`，供 adapter 判定「回执 turnId 是否为派发前旧 Turn」。
    #[test]
    fn turn_ids_collects_canonical_turns() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("canonical-before-load.sanitized.json"),
        )
        .unwrap();
        // 首快照/GET 的代际必须来自绑定 lease（不再恒 0），否则 SSE 启动缓冲
        // 会把任何事件都判成更新（2026-10-20 复审 I4）。
        let proj = m.project(caps());
        assert_eq!(proj.owner_epoch, lease().epoch, "epoch 来自绑定 lease");
        assert_eq!(
            proj.owner_generation,
            lease().generation,
            "generation 来自绑定 lease"
        );
        let ids = m.turn_ids();
        assert!(ids.len() >= 3, "至少三条 canonical Turn：{ids:?}");
        assert!(ids.iter().all(|id| !id.is_empty()), "不含空 id");
        // 同一时刻的投影与集合同源（不引入第二套读取逻辑）
        assert!(ids.contains("00000000-0000-4000-8000-000000000011"));
    }

    /// Task 6 真机可追溯样本（DERIVED，源 `raw/t4-1.jsonl` T4 rev3）：Turn 只有
    /// `params.input` 时投影出开场用户气泡。
    #[test]
    fn observed_derived_opening_falls_back_to_turn_input() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("windows-4894-opening.derived.sanitized.json"),
        )
        .unwrap();
        let p = m.project(caps());
        let users: Vec<&ProjectedMessage> =
            p.messages.iter().filter(|m| m.role == "user").collect();
        assert!(!users.is_empty(), "开场后备必须出现");
        assert!(
            users.iter().any(|m| m.id.contains("/opening/")),
            "后备 ID 必须带 /opening/ 身份: {users:?}"
        );
    }

    /// Task 6 忙态负例（DERIVED，源 `raw/t7-1.jsonl`）：第二次 start-turn 的 input 被
    /// 写进同一 Turn 的 params，但**没有新 userMessage** —— 该文本不得成为幽灵气泡。
    #[test]
    fn observed_derived_busy_second_prompt_is_not_a_bubble() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("windows-4894-busy-negative.derived.sanitized.json"),
        )
        .unwrap();
        let p = m.project(caps());
        assert!(
            !p.messages
                .iter()
                .any(|m| m.text.contains("Synthetic undelivered busy prompt")),
            "未落 Turn 的第二条 prompt 不得进投影: {:?}",
            p.messages
                .iter()
                .map(|m| (&m.id, &m.text))
                .collect::<Vec<_>>()
        );
    }
}
