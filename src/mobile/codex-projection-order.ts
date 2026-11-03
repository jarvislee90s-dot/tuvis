// Codex 投影落地守卫（T02）：GET／SSE／轮询／手动刷新共用的**唯一**版本比较。
//
// 服务端事实模型 = `src-tauri/src/remote/codex_ipc.rs` 的 `supersedes`
// （epoch > generation > stale/revision > local_seq）；本模块把同一比较搬到 TS，
// 并加 **instance 维度**：`instanceId` 绑定服务端 hub 世系（`owner_boot`，随服务
// 重启更换），跨 instance 的 revision/epoch 不可直接比较——只有携带**当前有效
// 握手票**的响应有权确立新 instance（票的发放/捕获/消费在调用方 useCodexThread：
// SSE `snapshot` 事件恒有权；GET 按发出时捕获的票号结算）。旧 GET 无权主动切回
// 旧 instance。
//
// 兼容纪律（契约 T02）：服务端版本不明确（载荷缺 `projectionVersion`）时不能
// 通过新写资格——写门本就不在前端；本函数在缺版本束时退化为既有 revision
// 规则，只决定「落地与否」，绝不据此放宽任何既有守卫（只读语义、stale 禁写、
// historyToken 失效逻辑均在调用方保持不变）。
import type { ProjectionVersion, ThreadProjection } from "./codexIpcApi";

/** 投影落地判定的订阅上下文（调用方以它声明「这个响应属于哪次订阅」）。 */
export interface ProjectionApplyContext {
  /** 当前订阅的会话（调用方已在进入本函数前核对会话归属；保留在上下文中以便审计）。 */
  sessionId: string;
  /** 当前订阅代际（切会话/重挂订阅 +1）。 */
  subscriptionGeneration: number;
  /** 响应发出时的订阅代际（在途 GET 捕获；不等于当前代际 = 迟到响应，一律作废）。 */
  responseSubscriptionGeneration: number;
  /**
   * 是否允许切换 instance。R03 起语义 = 「该响应携带的握手票号仍是当前有效票」
   * （调用方按请求发出时捕获的票号结算；SSE `snapshot` 恒为 true）。此后任何
   * 无票响应都无权切换 instance（含切回旧 instance）。
   */
  allowInstanceChange: boolean;
}

/**
 * 迟到的响应能否落地为当前投影。
 *
 * 返回 true 仅表示「incoming 比 current 新（或属于新订阅基线）」，不携带任何
 * 写资格语义；`current === null` 时除订阅代际检查外直接接受（首个基线）。
 */
export function shouldApplyProjection(
  current: ThreadProjection | null,
  incoming: ThreadProjection,
  context: ProjectionApplyContext
): boolean {
  // 迟到的旧订阅代际响应（切会话/禁用重挂/重订阅后的在途 GET）：一律作废。
  if (context.responseSubscriptionGeneration !== context.subscriptionGeneration) {
    return false;
  }
  if (current === null) {
    // 首个基线投影（仍受上面的代际检查约束）。
    return true;
  }
  // 跨 thread：不属于当前订阅的投影不落地（与服务端 supersedes 的 thread_id
  // 检查同源——线程重建/会话切换由调用方的失效逻辑处理，不走本守卫）。
  if (current.threadId !== incoming.threadId) {
    return false;
  }
  const incomingVersion = incoming.projectionVersion;
  const currentVersion = current.projectionVersion;
  if (incomingVersion === undefined || currentVersion === undefined) {
    // 旧载荷（缺版本束）：退化为既有 revision 规则。同 revision 落地（现有
    // 行为：开关/暂停 republish 帧、重发快照无副作用）；更低 revision 拒绝。
    if (incoming.revision === null) return current.revision === null;
    if (current.revision === null) return true;
    return incoming.revision >= current.revision;
  }
  return supersedesByVersion(current, incoming, currentVersion, incomingVersion, context);
}

/** 版本束齐全：与 Rust `supersedes` 逐条一致（epoch > generation > stale/revision > localSeq），外加 instance 维度。 */
function supersedesByVersion(
  current: ThreadProjection,
  incoming: ThreadProjection,
  cur: ProjectionVersion,
  inc: ProjectionVersion,
  context: ProjectionApplyContext
): boolean {
  if (inc.instanceId !== cur.instanceId) {
    // 跨 hub 世系（服务重启）：随机 instanceId 之间 revision/epoch 不可比较。
    // 只有当前代际最新握手有权确立新 instance——旧 GET 无权切回旧 instance。
    return context.allowInstanceChange;
  }
  if (inc.ownerEpoch !== cur.ownerEpoch) {
    return inc.ownerEpoch > cur.ownerEpoch;
  }
  if (inc.ownerGeneration !== cur.ownerGeneration) {
    return inc.ownerGeneration > cur.ownerGeneration;
  }
  if (incoming.stale !== current.stale) {
    if (incoming.stale) {
      // 断链送达（stale = 当前视图失效，必须禁写）：不早于当前恢复状态才可
      // 落地——较旧 stale 不得把已恢复的卡面拉回禁写；同 revision 的 stale
      // 保守送达（宁禁写、不假可控）；无 revision 的 invalidate 无法证明更旧，
      // 同样保守送达（可被后续更高 revision 的恢复帧清除）。
      if (incoming.revision === null || current.revision === null) return true;
      return incoming.revision >= current.revision;
    }
    // 恢复：只有更高 revision（或当前无 revision）证明世界已更新；同 revision
    // 的「非 stale」不能证明任何新数据。
    // 【有意偏离 Rust supersedes（2026-10-29 集中 review 申报）】本块并非与
    // Rust 逐字一致，偏离两臂（行为保持，仅展示层取舍）：① 上方 stale 臂——
    // 双方都有 revision 且 incoming 更旧时本处判 false，Rust（codex_ipc.rs
    // `supersedes`）对 stale 一律送达（比 Rust 紧：不把已前进的展示拉回禁写）；
    // ② 本恢复臂——incoming/current 都无 revision 时本处 return true，Rust
    // 判 false（`_ => false`，比 Rust 松、方向偏乐观：无 revision 证明也接受
    // 恢复帧、把卡面拉出禁写展示）。偏离仅作用在展示层：本函数只决定「落地
    // 与否」，不携带写资格语义（见文件头），Desktop 侧写门独立于本守卫，不受
    // 影响；是否与 Rust 对齐，留后续批带锁测试再决策。
    if (current.revision === null) return true;
    return incoming.revision !== null && incoming.revision > current.revision;
  }
  // 同 stale：revision 竞争，同 revision 按 localSeq 决胜（终审 F5）。
  if (incoming.revision !== null && current.revision !== null) {
    return (
      incoming.revision > current.revision ||
      (incoming.revision === current.revision && inc.localSeq > cur.localSeq)
    );
  }
  if (incoming.revision !== null) return true; // (Some, None)
  if (current.revision !== null) return false; // (None, Some)
  return inc.localSeq > cur.localSeq; // (None, None)：本地重发布按序号决胜
}
