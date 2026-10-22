// Codex 历史页展示缓存与合并（Task 5）：纯函数，无 React / fetch 依赖。
//
// 定位规则（计划任务 5）：
// - `mergeCodexHistory`：按消息身份（`ProjectedMessage.id`）合并，canonical 顺序
//   （旧→新，最新在最后）；**当前 canonical 投影覆盖重叠区**，旧页不得盖新投影；
//   不按 `ts` 重排（时间戳只作展示，不是顺序依据）。
// - 缓存顺序 `pages[0]` 紧邻当前尾窗（最新的一页更早消息），索引越大越旧。
// - 缓存预算：最多 4 页、每页最多 50 条、合计最多 4 MiB；超预算**从最旧一侧丢弃**
//   （不能只跳页保留，否则合并视图出现空洞）。
// - 单页 50 条是**服务端契约**：超限载荷是协议违约，整页拒绝（`normalize...` 返回
//   null），绝不裁剪——裁剪只能保留一侧，与调用方持有的 nextBefore/nextToken
//   （指向原页最早一条之前）不再一致，下一次翻页会跳过被裁掉的消息、制造永久空洞。
// - 缓存只是**展示缓存**，不是搜索范围：五页查找窗口 = 当前尾窗 + 最多四页更早缓存，
//   更早的页即使误入缓存也不参与（见 `boundedCodexHistoryWindow`，Task 7 消费）。
//
// R2 尾窗接缝（`retainCodexHistoryMessages` / `mergeCodexHistoryWithRetained`）：
// 服务端 token 在普通追加与尾窗前移时保持稳定，因此尾窗滚出的消息不再靠清缓存
// 掩盖——hook 把「已展示过、刚滚出尾窗」的消息按顺序放进**保留缓冲**（`retained`），
// `loadedMessages` = 缓存页（旧→新）+ 保留缓冲（去重）+ 当前尾窗（去重）。
// 保留缓冲有独立上限（2000 条 / 1 MiB，见下）；超出后最旧的消息会被丢弃，
// 在它被一页真正覆盖之前，那一条可能暂时缺席（不会伪造占位、也不会打乱顺序）。
//
// 安全语义：消息内容不写日志、不落盘；`bytes` 用序列化 UTF-8 字节计账，与服务端
// 页预算同一口径（近似值，用于客户端内存上限，不参与协议判定）。
import type { ProjectedMessage } from "./codexIpcApi";

/** 最多缓存的更早页数（不含当前尾窗；尾窗 + 四页 = 五页查找边界）。 */
export const CODEX_HISTORY_MAX_PAGES = 4;
/** 单页消息条数上限（服务端 `limit` 上限 50；超限页按协议违约整页拒绝）。 */
export const CODEX_HISTORY_PAGE_LIMIT = 50;
/** 历史页缓存合计字节上限（4 MiB）。 */
export const CODEX_HISTORY_MAX_BYTES = 4 * 1024 * 1024;
/**
 * 已展示消息保留缓冲的条数上限（R2 尾窗接缝）。
 * 缓冲只装「已展示过、已滚出尾窗、尚未被缓存页覆盖」的消息，从最旧一侧丢弃。
 */
export const CODEX_HISTORY_MAX_RETAINED_MESSAGES = 2000;
/**
 * 已展示消息保留缓冲的合计字节上限（1 MiB）。与页预算同口径（序列化 UTF-8 近似），
 * 与条数上限**先到者生效**；超出后最旧的消息被丢弃，直到被分页真正覆盖。
 */
export const CODEX_HISTORY_MAX_RETAINED_BYTES = 1024 * 1024;

/** 一页已归一化的更早历史（展示缓存条目）。 */
export interface CodexHistoryPage {
  /** 该页所属历史代际：发起请求时的当前投影 `historyToken`。token 变即整批失效。 */
  anchorToken: string;
  /** 该页的游标（`before_id`）；`null` = 历史末端锚点（空历史才可能 ready）。 */
  beforeId: string | null;
  /** 页内消息（canonical 旧→新，≤ {@link CODEX_HISTORY_PAGE_LIMIT} 条）。 */
  messages: ProjectedMessage[];
  /** 下一页游标（服务端 `nextBefore`）；不可继续时为 null。 */
  nextBefore: string | null;
  /** 下一页代际 token（服务端 `historyToken`）；不可继续时为空串。 */
  nextToken: string;
  /** 该页消息序列化 UTF-8 字节数（缓存预算计账）。 */
  bytes: number;
}

/** `normalizeCodexHistoryPage` 的输入（字节数由归一化计算）。 */
export interface CodexHistoryPageInput {
  anchorToken: string;
  beforeId: string | null;
  messages: readonly ProjectedMessage[];
  nextBefore: string | null;
  nextToken: string;
}

/** @see appendCodexHistoryPage */
export interface CodexHistoryAppendOptions {
  /** true = 本次是重新锚定的第一页（紧邻当前尾窗），插到最新一端；
   *  缺省 false = 沿游标链向更旧方向追加。 */
  front?: boolean;
}

/** @see appendCodexHistoryPage */
export interface CodexHistoryAppendResult {
  /** 预算裁剪后的缓存（新→旧）。 */
  pages: CodexHistoryPage[];
  /** 新页是否被预算丢弃（调用方据此重置游标链，避免后续翻页产生空洞）。 */
  dropped: boolean;
}

const encoder = typeof TextEncoder === "function" ? new TextEncoder() : null;

/** 一页消息的 JSON 序列化 UTF-8 字节数（与页预算计账口径一致）。 */
export function codexHistoryPageBytes(messages: readonly ProjectedMessage[]): number {
  const json = JSON.stringify(messages);
  // 极端环境无 TextEncoder：按 UTF-16 上界估算（只会偏保守，不会突破内存上限承诺）。
  return encoder === null ? json.length * 2 : encoder.encode(json).length;
}

/**
 * 归一化一页历史：复制消息并计算字节数；**超过单页 50 条返回 null（整页拒绝）**。
 *
 * 服务端契约保证单页 ≤ {@link CODEX_HISTORY_PAGE_LIMIT}；超限载荷是协议违约，不是
 * 可以静默重塑的数据。裁剪只能保留一侧：保留最新 50 条后，调用方手里的
 * `nextBefore`/`nextToken` 仍指向原页最早一条之前，下一次翻页会从更早处继续，
 * 被裁掉的中段消息永久缺失；保留最旧 50 条则与尾窗/更新页出现接缝空洞。
 * 因此这里不裁剪、不重排，只拒绝；调用方必须把 null 显式上报为不可用
 * （hook 映射 `unavailable/page_over_limit`），并且不推进游标链。
 */
export function normalizeCodexHistoryPage(input: CodexHistoryPageInput): CodexHistoryPage | null {
  if (input.messages.length > CODEX_HISTORY_PAGE_LIMIT) return null;
  const messages = [...input.messages];
  return {
    anchorToken: input.anchorToken,
    beforeId: input.beforeId,
    messages,
    nextBefore: input.nextBefore,
    nextToken: input.nextToken,
    bytes: codexHistoryPageBytes(messages),
  };
}

/**
 * 合并当前投影尾窗与已缓存更早页（纯函数）。
 *
 * - 页按新→旧存放，合并时从最旧页开始，再压入尾窗；
 * - 消息身份 = `id`；跨页/接缝重复只保留一次（较新页优先），尾窗同 id 条目**原位替换**
 *   为投影版本（当前投影权威，不移动位置、不按时间戳重排）；
 * - 给定 `token` 时只合并 `anchorToken` 相同的页：旧代际页不得混入新投影
 *   （传空串可表达「当前载荷没有可用代际」——任何已缓存页都不匹配）。
 */
export function mergeCodexHistory(
  tail: readonly ProjectedMessage[],
  pages: readonly CodexHistoryPage[],
  token?: string
): ProjectedMessage[] {
  const usable = token === undefined ? pages : pages.filter((page) => page.anchorToken === token);
  const merged: ProjectedMessage[] = [];
  const index = new Map<string, number>();
  for (let i = usable.length - 1; i >= 0; i -= 1) {
    for (const message of usable[i].messages) {
      if (index.has(message.id)) continue;
      index.set(message.id, merged.length);
      merged.push(message);
    }
  }
  for (const message of tail) {
    const at = index.get(message.id);
    if (at === undefined) {
      index.set(message.id, merged.length);
      merged.push(message);
    } else {
      merged[at] = message;
    }
  }
  return merged;
}

/** 空保留缓冲的稳定引用（重复返回同一对象，调用方可用 `!==` 判等）。 */
const NO_RETAINED: readonly ProjectedMessage[] = [];

/**
 * 从「上一帧尾窗 → 本帧尾窗」的变化里取出已滚出窗口的消息（R2 尾窗接缝）。
 *
 * 返回**新的保留缓冲**（旧→新）：
 * - 上一帧尾窗中仍在本帧尾窗里的消息不重复保留；
 * - 已在缓冲里的消息不重复追加；
 * - 没有新增时返回原数组引用（调用方据此跳过无谓的 setState）；
 * - 追加后按 {@link capCodexRetainedMessages} 的上限从最旧一侧裁剪。
 *
 * **前提必须先被证明**（复核 F3 修正）：同 token 只保证「历史只追加」，**不保证**
 * 尾窗只向前移动——服务端允许在最新 Turn 内删除/替换而不换 token。因此这里先验证
 * 本次确实是「老的整段滚出去、新的整段接上来」，三条同时满足才保留：
 * 1. 窗口变短必须同时有裁剪证据（`truncated`）——合法收缩保留接缝，未裁剪的
 *    变短按删除处理；
 * 2. 上一帧尾窗的某个后缀按 id 逐条等于本帧尾窗的前缀（保序重叠 ≥ 1）；
 * 3. 本帧尾窗在重叠之后的**新增段**不含上一帧出现过的任何 id（含 = 重排/替换）。
 * 任一条不满足 → 返回空缓冲：此时「消失的消息」是**被删掉**的，不是滚出去的，
 * 按权威投影协调，绝不用保留缓冲把它们复活。
 */
export interface RetainCodexHistoryOptions {
  /**
   * 本帧投影是否**因窗口预算被裁剪**（服务端 `truncated`：条数上限、1 MiB 字节
   * 预算或最新条正文截头）。用于区分「合法收缩」与「消息被删」——长度变短本身
   * 不是删除的证据（复核 G4）。
   */
  truncated?: boolean;
}

export function retainCodexHistoryMessages(
  previousTail: readonly ProjectedMessage[],
  nextTail: readonly ProjectedMessage[],
  retained: readonly ProjectedMessage[],
  options: RetainCodexHistoryOptions = {}
): readonly ProjectedMessage[] {
  if (previousTail.length === 0) return retained;
  // 窗口变短：只有服务端确实裁剪了窗口时才是合法收缩；未裁剪却变短 = 删除/回退。
  if (nextTail.length < previousTail.length && options.truncated !== true) return NO_RETAINED;
  const overlap = tailSuffixPrefixOverlap(previousTail, nextTail);
  if (overlap === 0) return NO_RETAINED;
  const previousIds = new Set(previousTail.map((message) => message.id));
  const added = nextTail.slice(overlap);
  if (added.some((message) => previousIds.has(message.id))) return NO_RETAINED;
  const known = new Set(retained.map((message) => message.id));
  const dropped = previousTail
    .slice(0, previousTail.length - overlap)
    .filter((message) => !known.has(message.id));
  if (dropped.length === 0) return retained;
  return capCodexRetainedMessages([...retained, ...dropped]);
}

/**
 * 上一帧尾窗的**后缀**与本帧尾窗的**前缀**按 id 保序的最大重叠长度。
 * 纯追加时等于上一帧长度；整窗前移 k 条时等于 `长度 - k`；完全无关时为 0。
 */
function tailSuffixPrefixOverlap(
  previousTail: readonly ProjectedMessage[],
  nextTail: readonly ProjectedMessage[]
): number {
  const max = Math.min(previousTail.length, nextTail.length);
  for (let length = max; length > 0; length -= 1) {
    const offset = previousTail.length - length;
    let same = true;
    for (let i = 0; i < length; i += 1) {
      if (previousTail[offset + i].id !== nextTail[i].id) {
        same = false;
        break;
      }
    }
    if (same) return length;
  }
  return 0;
}

/**
 * 保留缓冲上限（2000 条 / 1 MiB，先到者生效）：从**最旧一侧**丢弃，保序。
 * 输入为旧→新；返回原数组引用表示未超限（调用方据此跳过无谓的 setState）。
 * 超限后最旧的消息在「被某一页真正覆盖」之前可能暂时缺席——如实缺一条，不补占位。
 */
export function capCodexRetainedMessages(
  retained: readonly ProjectedMessage[]
): readonly ProjectedMessage[] {
  if (retained.length === 0) return retained;
  let firstKept = retained.length;
  let bytes = 0;
  for (let i = retained.length - 1; i >= 0; i -= 1) {
    if (retained.length - i > CODEX_HISTORY_MAX_RETAINED_MESSAGES) break;
    const messageBytes = codexHistoryPageBytes([retained[i]]);
    if (bytes + messageBytes > CODEX_HISTORY_MAX_RETAINED_BYTES) break;
    bytes += messageBytes;
    firstKept = i;
  }
  return firstKept === 0 ? retained : retained.slice(firstKept);
}

/**
 * 合并更早页、已展示保留缓冲与当前尾窗（R2 尾窗接缝，纯函数）。
 *
 * 目标顺序 = canonical 旧→新，去重（身份 = `id`）：
 * 1. 缓存页从最旧页开始（`pages` 新→旧，倒序展开）；
 * 2. 保留缓冲中尚未出现的消息（已滚出尾窗、还没被页覆盖的接缝消息）；
 * 3. 当前尾窗消息；与 1/2 重叠时**原位替换**为当前投影版本（投影权威，不重排）。
 *
 * 与 `mergeCodexHistory` 的差别只在第 2 步，以及页的落位方式：页的 `beforeId`
 * 是取页时的游标（页内消息全部严格早于它）。保留缓冲可能同时跨越「页之前」与
 * 「页之后」两段，因此不能简单把整页拼在缓冲前面/后面——这里把每页插到其游标
 * 锚点之前；若页与已知时间线有重叠，则插到最早重叠条目的位置之前（缺失的旧前缀
 * 落在重叠段之前）。这样指定用例（页紧邻尾窗）得到与「页 → 缓冲 → 尾窗」完全
 * 相同的顺序，而缓冲跨页时也不会出现可见乱序。
 *
 * 页内容优先于保留缓冲（同 id 时以服务端页为准，页比缓冲新）；当前尾窗仍覆盖
 * 二者（投影权威）。`token` 语义与 `mergeCodexHistory` 相同：只合并同代页。
 */
export function mergeCodexHistoryWithRetained(
  tail: readonly ProjectedMessage[],
  pages: readonly CodexHistoryPage[],
  retained: readonly ProjectedMessage[],
  token?: string
): ProjectedMessage[] {
  const merged: ProjectedMessage[] = [];
  const index = new Map<string, number>();
  for (const message of retained) {
    if (index.has(message.id)) continue;
    index.set(message.id, merged.length);
    merged.push(message);
  }
  const tailIds = new Set<string>();
  for (const message of tail) {
    tailIds.add(message.id);
    const at = index.get(message.id);
    if (at === undefined) {
      index.set(message.id, merged.length);
      merged.push(message);
    } else {
      merged[at] = message;
    }
  }
  const usable = token === undefined ? pages : pages.filter((page) => page.anchorToken === token);
  /** 已由（较新的）缓存页占据的身份：较旧页不得覆盖它们。 */
  const pageKnown = new Set<string>();
  // 页按新→旧处理：后一页的 beforeId 必是前一页的最早一条，插入后即可作为锚点。
  for (const page of usable) {
    if (page.messages.length === 0) continue;
    const pageIds = new Set(page.messages.map((message) => message.id));
    let firstOverlap = -1;
    for (let i = 0; i < merged.length; i += 1) {
      if (pageIds.has(merged[i].id)) {
        firstOverlap = i;
        break;
      }
    }
    const anchor = page.beforeId === null ? -1 : merged.findIndex((m) => m.id === page.beforeId);
    const missing: ProjectedMessage[] = [];
    for (const message of page.messages) {
      const at = index.get(message.id);
      if (at === undefined) {
        missing.push(message);
      } else if (!tailIds.has(message.id) && !pageKnown.has(message.id)) {
        // 页比保留缓冲新；当前尾窗消息仍以投影为准。
        merged[at] = message;
      }
      pageKnown.add(message.id);
    }
    if (missing.length === 0) continue;
    // 缺失的都是页内较旧前缀：有重叠就落在最早重叠项之前，否则落在游标锚点之前。
    // 锚点与重叠都不存在（防御：缓存页与当前时间线完全脱节）时插到最前（页更旧）。
    const insertAt = firstOverlap >= 0 ? firstOverlap : anchor >= 0 ? anchor : 0;
    merged.splice(insertAt, 0, ...missing);
    index.clear();
    merged.forEach((message, at) => index.set(message.id, at));
  }
  return merged;
}

/**
 * 缓存预算裁剪：保留最新的页，直到四页 / 4 MiB / 单页 50 条任一上限。
 *
 * 输入应为新→旧顺序。一旦某页放不下就**连同更旧的页一起丢弃**（不是跳过该页
 * 继续保留更旧页）——跳过会留下无法自洽的显示空洞。
 */
export function capCodexHistoryPages(pages: readonly CodexHistoryPage[]): CodexHistoryPage[] {
  const kept: CodexHistoryPage[] = [];
  let bytes = 0;
  for (const page of pages) {
    if (kept.length >= CODEX_HISTORY_MAX_PAGES) break;
    if (bytes + page.bytes > CODEX_HISTORY_MAX_BYTES) break;
    kept.push(page);
    bytes += page.bytes;
  }
  return kept;
}

/**
 * 把一页并入展示缓存（新→旧）：
 * - 同 `beforeId` 视为同一游标位置（重新锚定/重取）：原位替换，避免堆积；
 * - `front`（新链第一页）插到最新端；否则视为更旧页追加到末尾；
 * - 最后按预算裁剪并报告新页是否被丢弃。
 */
export function appendCodexHistoryPage(
  pages: readonly CodexHistoryPage[],
  page: CodexHistoryPage,
  options: CodexHistoryAppendOptions = {}
): CodexHistoryAppendResult {
  const rest = pages.filter((existing) => existing.beforeId !== page.beforeId);
  const at = pages.findIndex((existing) => existing.beforeId === page.beforeId);
  let combined: CodexHistoryPage[];
  if (at >= 0) {
    // 同游标 = 同一位置重取：原位替换，不堆积
    combined = pages.map((existing, i) => (i === at ? page : existing));
  } else {
    // 按**游标连接关系**插页（复核 T1）：显式补页既可能比缓存新、也可能比缓存旧，
    // 一律插到最新端会把「先 m50—m99、再 m0—m49」排成 [m0—m49, m50—m99]，随后共享
    // 选择器从 m100 出发第一个看到的页就是 m0—m49，直接判缺口、两页全部出局。
    // 契约：页的 `nextBefore` 指向更旧一页的 `beforeId`，反之亦然。
    const newerAt = pages.findIndex(
      (existing) => existing.nextBefore !== null && existing.nextBefore === page.beforeId
    );
    const olderAt =
      page.nextBefore === null
        ? -1
        : pages.findIndex((existing) => existing.beforeId === page.nextBefore);
    if (newerAt >= 0) {
      combined = [...pages.slice(0, newerAt + 1), page, ...pages.slice(newerAt + 1)];
    } else if (olderAt >= 0) {
      combined = [...pages.slice(0, olderAt), page, ...pages.slice(olderAt)];
    } else if (options.front === true) {
      combined = [page, ...rest];
    } else {
      combined = [...rest, page];
    }
  }
  const capped = capCodexHistoryPages(combined);
  return { pages: capped, dropped: !capped.includes(page) };
}

/**
 * 有界五页窗口的**选页结果**：池侧来源/排序与定位 hook 共用同一份判断
 * （复核 S2：两处各写一套近似规则时，「有界」不等于「同一个窗口」）。
 */
export interface CodexHistoryWindow {
  /**
   * 从尾窗向前的**连续**批次（新→旧）：`batches[0]` 恒为实时尾窗（可能为空），
   * 其后每批 ≤{@link CODEX_HISTORY_PAGE_LIMIT} 条，最多 {@link CODEX_HISTORY_MAX_PAGES} 批。
   */
  batches: ProjectedMessage[][];
  /**
   * 已覆盖范围的最老一条消息 id：下一更早批次必须严格早于它。
   * `null` = 没有可用前沿（空尾窗且无更早本地数据）。
   */
  frontierBeforeId: string | null;
  /**
   * 因**缺口**提前结束：本地还有更早的缓存页，但它与前沿不相邻（中间一段既不在
   * retained 也不在缓存里），不能越过它去拿更旧的引用（复核 S1）。
   */
  gap: boolean;
}

/**
 * 选择五页窗口（纯函数，唯一实现）。
 *
 * 边界（spec §4.2，复核 G1 后的口径）：第 1 页 = 实时尾窗；其后最多四批、每批
 * ≤50 条，**连续**向前。批次来源按新到旧：
 * 1. retained——它装的正是从尾窗滚出的消息，天然与尾窗连续（裁剪只丢最旧一端）；
 * 2. 缓存页——只接受与该前沿**相邻**（`beforeId === frontier`）的页；整页重复的
 *    免费跳过（不占预算、也不算缺口）；有缺口即停止，绝不越过缺口取更旧内容。
 *
 * 池侧（{@link boundedCodexHistoryWindow}）与定位 hook 都消费本结果，因此
 * 「窗口里有谁」「还剩哪条前沿」只有一处定义。
 */
export function selectCodexHistoryWindow(
  tail: readonly ProjectedMessage[],
  pages: readonly CodexHistoryPage[],
  retained: readonly ProjectedMessage[],
  token?: string
): CodexHistoryWindow {
  const batches: ProjectedMessage[][] = [[...tail]];
  const covered = new Set<string>(tail.map((message) => message.id));
  const cover = (messages: readonly ProjectedMessage[]) => {
    for (const message of messages) covered.add(message.id);
  };
  let frontier = tail.length > 0 ? tail[0].id : null;
  let budget = CODEX_HISTORY_MAX_PAGES;

  let end = retained.length;
  while (end > 0 && budget > 0) {
    const start = Math.max(0, end - CODEX_HISTORY_PAGE_LIMIT);
    const batch = retained.slice(start, end);
    batches.push(batch);
    cover(batch);
    frontier = batch[0].id;
    budget -= 1;
    end = start;
  }

  const usable = token === undefined ? pages : pages.filter((page) => page.anchorToken === token);
  let gap = false;
  for (const page of capCodexHistoryPages(usable)) {
    if (budget === 0) break;
    if (page.messages.length === 0) continue;
    if (!page.messages.some((message) => !covered.has(message.id))) continue; // 整页重复
    // 相邻性有两条路径（复核 T2）：
    //   a) `beforeId === 前沿`——整页都严格早于前沿，直接相邻；
    //   b) 页内**含前沿那条消息**——页的新侧后缀已被尾窗覆盖，只有旧侧前缀是新的，
    //      且与前沿有序重叠。活动末条变短、尾窗从字节预算放宽回条数预算时会出现。
    // 两者都不成立才是真缺口（S1 的拒绝规则保持不变）；不按 id 大小猜顺序。
    const anchorIndex =
      frontier === null ? -1 : page.messages.findIndex((message) => message.id === frontier);
    if (frontier === null || (page.beforeId !== frontier && anchorIndex < 0)) {
      gap = true;
      break;
    }
    const fresh = anchorIndex >= 0 ? page.messages.slice(0, anchorIndex) : [...page.messages];
    cover(page.messages);
    if (fresh.length === 0) continue; // 只覆盖到前沿本身：不占预算
    batches.push(fresh);
    // 下一页必须从这个游标继续（服务端契约：nextBefore 是更旧一页的 before_id）。
    frontier = page.nextBefore === null ? fresh[0].id : page.nextBefore;
    budget -= 1;
  }
  return { batches, frontierBeforeId: frontier, gap };
}

/**
 * 五页查找窗口（Task 7 文件池消费）：把 {@link selectCodexHistoryWindow} 选中的
 * 连续批次摊平成 canonical 旧→新。
 *
 * 池侧与定位 hook 因此共用同一套去重、连续性判断与批次预算：窗口总消息数上界恒为
 * 「尾窗 + 200」（四批 × 50），且不会越过 retained／缓存页之间的缺口。
 */
export function boundedCodexHistoryWindow(
  tail: readonly ProjectedMessage[],
  pages: readonly CodexHistoryPage[],
  token?: string,
  retained: readonly ProjectedMessage[] = []
): ProjectedMessage[] {
  const { batches } = selectCodexHistoryWindow(tail, pages, retained, token);
  const merged: ProjectedMessage[] = [];
  for (let i = batches.length - 1; i >= 0; i -= 1) merged.push(...batches[i]);
  return merged;
}
