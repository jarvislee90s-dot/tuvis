// R07（D3）：Codex 能力**展示缓存**——按 `capabilityIdentity` 沿用上次支持事实。
//
// 背景（集中审查 R07）：真实重连会换连接代际，后端契约证据槽随之失配 → 投影的
// `support` 回到 `identifying`。若适配已关闭，取证函数直接退出，同一完整目标可能
// 长期显示"识别中"。前端据此把**上次已识别的支持展示**按 identity 缓存一次，
// 同 identity 的断链/重识别窗沿用，identity 变化即清。
//
// 三条纪律（与后端 `DisplaySupport` 同语义）：
// 1. **纯展示**：只替换 `support`（与 `capabilityIdentity` 的一致性判定），
//    `allowed`/`evidence`/`reasonCode` 一字不动 —— 展示缓存**不是**授权缓存；
// 2. **同身份才沿用**：`capabilityIdentity` 必须非空且与缓存相同；缺失或变化 →
//    立即清缓存（新 build/SHA/规则身份不得继承旧目标能力）；
// 3. **实时事实优先**：非 `identifying` 的实时结论（supported/unmatched/
//    not_implemented）直接采用并刷新缓存，缓存只在 `identifying` 档兜底。
import type { CodexEffectiveOperations, CodexOperation, CodexSupportState } from "./codexIpcApi";

/** 一次已识别结论的展示缓存（同 `capabilityIdentity` 下有效）。 */
export interface CapabilityDisplayCache {
  /** 缓存所属的能力身份（完整目标指纹 + 包 SHA + 规则版本的摘要）。 */
  identity: string;
  /** 逐操作支持结论（上次识别的事实）。 */
  support: Partial<Record<CodexOperation, CodexSupportState>>;
}

/** `retainCapabilityDisplay` 的结果。 */
export interface CapabilityDisplayResult {
  /** 实际用于渲染的集合（可能被展示缓存补过 `support`）。 */
  operations?: CodexEffectiveOperations;
  /** 下一次调用应传入的缓存（`null` = 无缓存/已清）。 */
  cache: CapabilityDisplayCache | null;
  /** 本次是否真的沿用了缓存里的支持展示。 */
  retained: boolean;
  /** 是否应显示「连接恢复中」（连接 stale 且本次沿用了展示缓存）。 */
  reconnecting: boolean;
  /** C03：沿用的展示身份**尚未通过本代际内容 SHA 核验**（「待核对」——支持可
   *  如实显示，但不得当作当前已确认身份）。已核对/未沿用 → false。 */
  identityPending: boolean;
}

/** 两份缓存内容是否等价（调用方用它短路 setState，避免自激重渲染）。 */
export function sameDisplayCache(
  a: { sessionId: string; cache: CapabilityDisplayCache } | null,
  b: { sessionId: string; cache: CapabilityDisplayCache } | null
): boolean {
  if (a === null || b === null) return a === b;
  if (a.sessionId !== b.sessionId || a.cache.identity !== b.cache.identity) return false;
  const aKeys = Object.keys(a.cache.support).sort();
  const bKeys = Object.keys(b.cache.support).sort();
  if (aKeys.length !== bKeys.length) return false;
  for (let index = 0; index < aKeys.length; index += 1) {
    const key = aKeys[index] as CodexOperation;
    if (key !== bKeys[index]) return false;
    if (a.cache.support[key] !== b.cache.support[key]) return false;
  }
  return true;
}

/** 实时事实档：这些 support 值直接采用，不进缓存兜底。 */
function isRealtime(support: CodexSupportState | undefined): boolean {
  return support !== undefined && support !== "identifying";
}

/**
 * 按 identity 维护展示缓存，并返回本次实际渲染的集合。
 *
 * 两条「连接恢复中」来源（任一成立即显示，都是纯展示）：
 * - **后端沿用标记** `supportInherited`（R07 修复轮 1：生产真实路径——后端已把
 *   `support` 替换成上次结论并置位，投影本身就不带 `identifying`）；
 * - **前端缓存兜底**：页面投影仍是 `identifying` 而同 identity 缓存有上次结论
 *   （覆盖投影发布与缓存落地之间的时序窗），且连接 stale。
 *
 * @param cache 上一次调用的结果缓存（`null` = 无）。
 * @param incoming 页面当前投影的 `effectiveOperations`（`undefined` = 页面还没有事实）。
 * @param flags `stale` = 连接 stale（断链/换代），用于前端缓存兜底路径的标记判定。
 */
export function retainCapabilityDisplay(
  cache: CapabilityDisplayCache | null,
  incoming: CodexEffectiveOperations | undefined,
  flags: { stale: boolean }
): CapabilityDisplayResult {
  if (!incoming) {
    // 页面还没给出事实：不渲染能力表，缓存保持不变（等真实投影）。
    return {
      operations: undefined,
      cache,
      retained: false,
      reconnecting: false,
      identityPending: false,
    };
  }
  const identity = incoming.capabilityIdentity;
  const sameIdentity = identity !== undefined && identity !== null && cache?.identity === identity;
  // R07 修复轮 1：后端沿用标记 = 投影自述"这是上次结论的展示"，生产真实路径的
  // 「连接恢复中」信号（不依赖前端缓存，也不依赖 stale 标志）。
  const backendInherited = Object.values(incoming.operations).some(
    (status) => status?.supportInherited === true
  );
  // C03：沿用中但本代际内容 SHA 核验未完成/未通过 → 「待核对」展示（支持可如实
  // 显示，但身份不得当作已确认；后端核验落账后会带 supportIdentityVerified 重发布）。
  const identityPending =
    backendInherited &&
    Object.values(incoming.operations).some(
      (status) => status?.supportInherited === true && status?.supportIdentityVerified !== true
    );

  // 本轮的实时支持结论（用于刷新缓存）。
  const realtime: Partial<Record<CodexOperation, CodexSupportState>> = {};
  for (const [operation, status] of Object.entries(incoming.operations) as [
    CodexOperation,
    NonNullable<CodexEffectiveOperations["operations"][CodexOperation]>,
  ][]) {
    if (status?.support !== undefined) realtime[operation] = status.support;
  }

  if (!sameIdentity) {
    // identity 变化/缺失：清缓存，只按实时事实展示（不继承旧目标）。
    return {
      operations: incoming,
      cache: identity ? { identity, support: realtime } : null,
      // 身份变了也仍然要如实显示后端自述的沿用（身份变化时后端不会置位，此处
      // 只是不吞掉信号）。
      retained: false,
      reconnecting: backendInherited,
      identityPending,
    };
  }

  // 同 identity：只在 `identifying` 档用缓存兜底；实时结论优先并覆盖缓存。
  let retained = false;
  const operations: CodexEffectiveOperations["operations"] = {};
  for (const [operation, status] of Object.entries(incoming.operations) as [
    CodexOperation,
    NonNullable<CodexEffectiveOperations["operations"][CodexOperation]>,
  ][]) {
    if (!status) continue;
    if (isRealtime(status.support)) {
      operations[operation] = status;
      continue;
    }
    // 识别中/缺失：若缓存里有上次的真实结论（supported/unmatched），沿用展示。
    const cached = cache?.support[operation];
    if (status.implemented && (cached === "supported" || cached === "unmatched")) {
      retained = true;
      operations[operation] = { ...status, support: cached };
    } else {
      operations[operation] = status;
    }
  }

  // 缓存刷新：实时结论覆盖旧缓存（同 identity 的识别已完成/进行中互不覆盖）。
  const merged: Partial<Record<CodexOperation, CodexSupportState>> = { ...cache?.support };
  for (const [operation, support] of Object.entries(realtime) as [
    CodexOperation,
    CodexSupportState,
  ][]) {
    if (isRealtime(support)) merged[operation] = support;
  }

  return {
    operations: { ...incoming, operations },
    cache: { identity: identity as string, support: merged },
    retained,
    // 两条来源取或：后端自述（生产路径）或前端兜底窗（且连接 stale）。
    reconnecting: backendInherited || (retained && flags.stale),
    identityPending,
  };
}
