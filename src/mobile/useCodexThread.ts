// Codex Thread 投影订阅 hook（Task 9）：详情页唯一的数据通道接驳层。
//
// 职责（计划 Task 9 Step 3）：
// - SSE 首帧（snapshot）/增量（state）同 DTO 落 state——只消费 Tuvis 投影，
//   不触碰 IPC 原始事件；
// - 连续 3 次失败降级为 3s GET 轮询（transport: "poll"；subscribeCodexThread 管
//   退避与降级信号，本 hook 接管轮询节奏——与 Board 的 connectEvents 分工同构）；
// - 卸载 abort 在途 fetch / 关流 / 清定时器，无泄漏；
// - 过时响应不盖新 session（session 切换防护：旧会话的任何迟到帧/轮询响应一律
//   作废——Review Focus 2 的「旧 epoch 数据不得污染新镜像」在前端的对应面）；
// - 轮询单次失败/403 不清空已有视图（stale 语义第一道防线：保留最后视图，
//   禁写由投影 stale 字段驱动）。
//
// Task 5 扩展（计划任务 5）：
// - `state.messages` 仍是**实时尾窗**（现有详情页消费方零改动）；
// - `loadedMessages` = 缓存页（旧→新）+ 已展示保留缓冲（去重）+ 尾窗（去重），
//   canonical 旧→新、最新在最后；`historyPages` = 更早页展示缓存（新→旧，
//   ≤4 页/≤50 条/≤4 MiB）；`loadEarlier` = 沿游标链读取一页更早消息；
// - R2 尾窗接缝：同 token 普通追加/尾窗前移时，滚出尾窗的消息先进入保留缓冲
//   （≤2000 条 / ≤1 MiB，超限丢最旧），直到被某一页真正覆盖；不再靠清缓存掩盖；
// - 失效：historyToken 换代/不匹配（含线程重建）、禁用订阅、切会话、卸载时清空
//   页缓存**与保留缓冲**、在途请求与游标链；旧会话/旧 token 的迟到响应一律不落地；
// - 3 次 SSE 失败 → 3s GET 轮询的既有行为不变。
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  fetchCodexMessagesPage,
  fetchCodexThread,
  subscribeCodexThread,
  type MessagesPage,
  type MessagesPageStatus,
  type ProjectedMessage,
  type ThreadProjection,
} from "./codexIpcApi";
import { shouldApplyProjection, type ProjectionApplyContext } from "./codex-projection-order";
import {
  CODEX_HISTORY_PAGE_LIMIT,
  appendCodexHistoryPage,
  mergeCodexHistoryWithRetained,
  normalizeCodexHistoryPage,
  retainCodexHistoryMessages,
  type CodexHistoryPage,
} from "./codex-history";

/** 降级后的 GET 轮询周期（计划钉值：3s） */
export const CODEX_THREAD_POLL_MS = 3000;

/** R03 有界再武装阈值：连续跨 instance 拒绝达到该次数即补发一张新握手票。
 *  取 1 = 一次拒绝即补票：恢复延迟一个轮询周期；安全性由「票只能被补票后新发出
 *  的请求捕获」保证——单活 hub 下新请求只可能解析到在世 instance，旧世界响应
 * （重启前挂起/同票迟到）捕获的是旧号或 null，结构性无法借补票装回。 */
const CODEX_HANDSHAKE_REARM_REJECTS = 1;

/** 传输通道：sse = 事件流在线；poll = SSE 三连失败后的 GET 轮询；off = 未启用 */
export type CodexTransport = "sse" | "poll" | "off";

/** `loadEarlier` 结果状态：服务端五态，外加客户端不发起/失效时的同类取值。 */
export type CodexHistoryLoadStatus = MessagesPageStatus;

/** `loadEarlier` 签名：显式游标可选（见 {@link UseCodexThreadResult.loadEarlier}）。 */
export type LoadEarlier = (beforeId?: string | null) => Promise<CodexHistoryLoadResult>;

/**
 * `loadEarlier` 结果。`page` 仅 `ready` 时非 null（可能因展示缓存预算未保留在
 * `historyPages` 中，调用方若需逐页搜索应同时检查本字段）。
 *
 * 客户端本地 reasonCode（服务端机器码之外的补充，均非正常页）：
 * - `subscription_disabled`：订阅已禁用或非当前会话；
 * - `projection_unavailable`：尚无投影（state=null）；
 * - `history_token_missing`：旧载荷没有历史代际 token（禁止猜 token 分页）；
 * - `request_failed`：真实网络/HTTP/载荷异常（加载失败，可重试）；
 * - `stale_response`：请求被 abort 或响应到达时代际/会话/token 已变
 *   （失效态，绝不落地；Task 7 显示「历史代际失效，可重试」而非「加载失败」）；
 * - `page_over_limit`：单页超过服务端契约的 50 条（协议违约；整页拒绝、不裁剪、
 *   不推进游标链、不动已有缓存）；
 * - `empty_page`：协议违约的 ready 空页（不当成完整一页）。
 */
export interface CodexHistoryLoadResult {
  status: CodexHistoryLoadStatus;
  reasonCode: string | null;
  page: CodexHistoryPage | null;
}

export interface UseCodexThreadResult {
  /** 当前实时投影（`messages` 是**实时尾窗**，语义与既有消费方不变）。 */
  state: ThreadProjection | null;
  /**
   * 缓存页（旧→新）+ 已展示保留缓冲（去重）+ 尾窗（去重），canonical 旧→新、
   * 最新在最后。保留缓冲装「已滚出尾窗、尚未被页覆盖」的消息（≤2000 条 / ≤1 MiB，
   * 超限丢最旧：超出上限的旧消息在它被分页覆盖之前可能暂时缺席）。
   */
  loadedMessages: ProjectedMessage[];
  /** 更早页展示缓存（新→旧：索引 0 紧邻尾窗；≤4 页 / 每页 ≤50 条 / 合计 ≤4 MiB）。 */
  historyPages: CodexHistoryPage[];
  /**
   * 已滚出尾窗、尚未被缓存页覆盖的保留缓冲（旧→新，≤2000 条 / ≤1 MiB）。
   * 它同时参与列表显示（`loadedMessages`）与文件池来源/定位窗口——三者必须同源，
   * 否则会出现「正文看得见、定位找不到」（复核 F2）。
   */
  retained: readonly ProjectedMessage[];
  /**
   * 沿游标链读取一页更早消息（只读；并发调用共享同一在途请求）。
   * 传入 `beforeId` 时忽略本地游标链、从该游标重新锚定——定位遇到「本地连续前沿
   * 与旧缓存页之间有缺口」时用它补页（复核 S1）。
   */
  loadEarlier: LoadEarlier;
  /** 手动重拉（只读 GET；transport 不变） */
  refresh: () => Promise<void>;
  transport: CodexTransport;
}

/** 历史游标链：下一更早页的 `before_id` + 代际 token（服务端页响应直接给出）。 */
interface HistoryCursorChain {
  beforeId: string | null;
  token: string;
  /** 服务端已明确到头：不再发请求。 */
  ended: boolean;
}

/** 在途页请求（同会话同代际共享，避免重复请求/竞态）。 */
interface InflightPageRequest {
  sid: string;
  anchorToken: string;
  /** 请求游标：同代不同游标不得共享在途请求（显式重锚与链式续翻是两次读取）。 */
  beforeId: string | null;
  generation: number;
  ctrl: AbortController;
  promise: Promise<CodexHistoryLoadResult>;
}

export default function useCodexThread(sessionId: string, enabled: boolean): UseCodexThreadResult {
  const [state, setState] = useState<ThreadProjection | null>(null);
  const [transport, setTransport] = useState<CodexTransport>("off");
  const [historyPages, setHistoryPages] = useState<CodexHistoryPage[]>([]);
  /** R2 已展示保留缓冲（旧→新；与 ref 同步，供 applyProjection 同步读写） */
  const [retained, setRetained] = useState<readonly ProjectedMessage[]>([]);
  /** 当前会话哨兵：apply 前比对，旧会话的迟到帧/响应不得写入 */
  const sessionRef = useRef(sessionId);
  /** 当前投影（供 loadEarlier 同步读取，避免回调闭包滞后一帧） */
  const stateRef = useRef<ThreadProjection | null>(null);
  /** 当前投影的 historyToken（历史代际；空串 = 旧载荷，不可分页） */
  const tokenRef = useRef("");
  /** 页缓存（与 historyPages state 同步；请求回调以 ref 为准） */
  const pagesRef = useRef<CodexHistoryPage[]>([]);
  /** 保留缓冲（与 retained state 同步；事件回调以 ref 为准） */
  const retainedRef = useRef<readonly ProjectedMessage[]>([]);
  const chainRef = useRef<HistoryCursorChain | null>(null);
  const inflightRef = useRef<InflightPageRequest | null>(null);
  /** 历史代际计数：每次失效 +1；在途页响应必须带回同代才可落地 */
  const generationRef = useRef(0);
  const enabledRef = useRef(enabled);
  /** T02 订阅代际计数：每次订阅（重挂/切会话/启用翻转）+1；在途 GET 必须带回
   *  同代才可落地（切订阅代际作废旧在途 GET——abort 之外的迟到响应兜底）。 */
  const subscriptionGenerationRef = useRef(0);
  /** R03 握手代际票：非 null = 当前持有一张**未消费**的跨 instance 建立权
   * （SSE→poll 降级点发放；消费后由「连续跨 instance 拒绝」有界再武装补发）；
   *  null = 无有效票（SSE 在线语义：GET 无权切换 instance）。GET 发出时捕获票号，
   *  响应落地时仅当「捕获票号非空且仍是当前票」才有权确立新 instance——票随跨
   *  instance 建立消费失效，同票的重叠/迟到在途响应一律按普通规则拒绝；重启前
   *  发出的挂起 GET 捕获的是旧票号（或 null），结构性无法借后续再武装越权。 */
  const handshakeTicketRef = useRef<{ seq: number; consumed: boolean } | null>(null);
  /** 票号单调递增源：再武装只发新号，旧票号永不复用（捕获比较才可靠）。 */
  const ticketSeqRef = useRef(0);
  /** R03 有界再武装：连续跨 instance 拒绝计数，达到阈值补发一张新票——修复
   *  「票已消费后服务再重启 → 新 instance 永久锁死」的死角。阈值 1：一次跨
   *  instance 拒绝即补票（再武装只可能被**补票后新发出**的请求捕获，单活 hub
   *  下它们只能解析到在世 instance，旧世界响应无法受益——见报告选型说明）。 */
  const crossInstanceRejectStreakRef = useRef(0);

  /** 发放一张新握手票（单调新号、未消费；旧票号随即失效）。
   *  C01：仅在**没有未消费有效票**时发放——旧票请求的拒绝不得作废已有人在途
   *  持有的票（RTT > 轮询周期时那会造成稳定饥饿：每次过期拒绝都换票，正确
   *  在途请求永远追不上当前票号）。 */
  const mintHandshakeTicket = useCallback(() => {
    const current = handshakeTicketRef.current;
    if (current !== null && !current.consumed) return;
    ticketSeqRef.current += 1;
    handshakeTicketRef.current = { seq: ticketSeqRef.current, consumed: false };
  }, []);

  /** 握手授权捕获 → 落地判定：请求发出时捕获的票号仍是当前**未消费**有效票才
   *  拥有跨 instance 建立权（null 票 = 无权；已消费/被新票取代 = 无权）。 */
  const ticketGrantsInstanceChange = useCallback((ticketAtRequest: number | null) => {
    const current = handshakeTicketRef.current;
    return (
      ticketAtRequest !== null &&
      current !== null &&
      !current.consumed &&
      ticketAtRequest === current.seq
    );
  }, []);

  /** 清空历史分页状态：页缓存 + 保留缓冲 + 游标链 + 在途请求。
   *  触发点：historyToken 换代/不匹配、线程重建、禁用订阅、切会话、卸载。 */
  const invalidateHistory = useCallback(() => {
    generationRef.current += 1;
    const inflight = inflightRef.current;
    inflightRef.current = null;
    inflight?.ctrl.abort();
    const hadPages = pagesRef.current.length > 0;
    pagesRef.current = [];
    chainRef.current = null;
    // 空数组不重复 setState（首帧/旧载荷场景避免无谓重渲染）
    if (hadPages) setHistoryPages([]);
    const hadRetained = retainedRef.current.length > 0;
    retainedRef.current = [];
    if (hadRetained) setRetained([]);
  }, []);

  /** 投影落地（SSE 帧 / 轮询 GET / 手动刷新唯一入口）。
   *
   *  T02 版本守卫：所有来源统一经 `shouldApplyProjection` 判定——迟到的旧响应
   *  （旧订阅代际、旧 instance、更低的 revision/localSeq）不得覆盖新状态；
   *  未获准落地时不触碰 token/页缓存/保留缓冲。
   *
   *  R03 握手票结算：跨 instance 建立消费当前票（同票的重叠/迟到在途响应随之
   *  失去建立权——先到先得）；同 instance 落地不清票（票留给后续重启）。被拒
   *  响应只统计「当前订阅代际的跨 instance 拒绝」，达到阈值有界再武装补票。
   *
   *  token 变化（线程重建/代际换代）→ 旧页、保留缓冲与游标链整体失效；
   *  同 token（普通追加/尾窗前移）→ 把已滚出尾窗的消息按顺序接进保留缓冲。 */
  const applyProjection = useCallback(
    (
      p: ThreadProjection,
      source: { allowInstanceChange: boolean; responseGeneration: number }
    ): boolean => {
      const context: ProjectionApplyContext = {
        sessionId: sessionRef.current,
        subscriptionGeneration: subscriptionGenerationRef.current,
        responseSubscriptionGeneration: source.responseGeneration,
        allowInstanceChange: source.allowInstanceChange,
      };
      if (!shouldApplyProjection(stateRef.current, p, context)) {
        // R03 有界再武装：只统计**当前订阅代际**的跨 instance 拒绝（旧代际迟到
        // 响应与本代际握手状态无关，不得计数也不得触发补票）；双方都带版本束且
        // instanceId 不同才算跨 instance 拒绝（旧载荷退化路径与此无关）。
        if (source.responseGeneration === subscriptionGenerationRef.current) {
          const prevInstance = stateRef.current?.projectionVersion?.instanceId;
          const incomingInstance = p.projectionVersion?.instanceId;
          if (
            prevInstance !== undefined &&
            incomingInstance !== undefined &&
            prevInstance !== incomingInstance
          ) {
            crossInstanceRejectStreakRef.current += 1;
            if (crossInstanceRejectStreakRef.current >= CODEX_HANDSHAKE_REARM_REJECTS) {
              crossInstanceRejectStreakRef.current = 0;
              mintHandshakeTicket();
            }
          }
        }
        return false;
      }
      const nextToken = p.historyToken ?? "";
      const previous = stateRef.current;
      if (nextToken !== tokenRef.current) {
        invalidateHistory();
        tokenRef.current = nextToken;
      } else if (previous !== null) {
        const nextRetained = retainCodexHistoryMessages(
          previous.messages,
          p.messages,
          retainedRef.current,
          // 合法窗口收缩（条数/字节预算裁剪）与「消息被删」必须靠服务端信号区分
          { truncated: p.truncated }
        );
        if (nextRetained !== retainedRef.current) {
          retainedRef.current = nextRetained;
          setRetained(nextRetained);
        }
      }
      stateRef.current = p;
      setState(p);
      // R03 握手票结算：跨 instance 建立消费当前票（先到先得，同票重叠响应随后
      // 全部失去建立权）；同 instance 落地保留未消费的票（它属于后续可能的重启）
      // 并打断连续拒绝计数。SSE snapshot 的跨 instance 建立同样消费（票只属于
      // 建立那一刻，落地后的世界由 revision/localSeq 规则接管）。
      const previousInstance = previous?.projectionVersion?.instanceId;
      const landedInstance = p.projectionVersion?.instanceId;
      if (
        previousInstance !== undefined &&
        landedInstance !== undefined &&
        previousInstance !== landedInstance
      ) {
        // C01：建立消费当前票——标 consumed 保留 seq（再次重启走「无未消费票 →
        // 补票」重新武装）。
        const current = handshakeTicketRef.current;
        if (current !== null) handshakeTicketRef.current = { ...current, consumed: true };
        crossInstanceRejectStreakRef.current = 0;
      } else {
        crossInstanceRejectStreakRef.current = 0;
      }
      return true;
    },
    [invalidateHistory, mintHandshakeTicket]
  );

  useEffect(() => {
    enabledRef.current = enabled;
    // T02：每次订阅重挂（切会话/启用翻转）换新订阅代际——旧代际的在途 GET 一律
    // 作废（abort 之外的迟到响应兜底）；R03：握手票随代际清空（新订阅从 SSE
    // 语义起步：GET 无权切换 instance，降级时再发新票）。
    subscriptionGenerationRef.current += 1;
    handshakeTicketRef.current = null;
    crossInstanceRejectStreakRef.current = 0;
    if (!enabled) {
      // 禁用订阅：清历史分页状态（保留最后投影视图——现有 state 语义不变）
      setTransport("off");
      invalidateHistory();
      tokenRef.current = "";
      return;
    }
    sessionRef.current = sessionId;
    const sid = sessionId;
    let alive = true;
    let pollTimer: ReturnType<typeof setInterval> | null = null;
    // 轮询 fetch 的中止器：卸载即 abort（在途请求不再产生任何效果）
    const ctrl = new AbortController();

    // 换会话先清旧投影与旧页：旧会话的内容不得跨会话显示（防串台第一拍）
    invalidateHistory();
    tokenRef.current = "";
    stateRef.current = null;
    setState(null);
    setTransport("sse");

    const isCurrent = () => alive && sessionRef.current === sid;

    /** SSE 帧落地（非当前会话/已卸载一律丢弃）。
     *  T02：`snapshot` = 当前代际最新握手（连接建立/重连基线校正），有权确立新
     *  instance；`state` 是普通增量，无权切换 instance。R03：snapshot 的跨
     *  instance 建立同样消费当前握手票（经 applyProjection 票结算）。 */
    const applyFrame = (p: ThreadProjection | null, event: "snapshot" | "state") => {
      if (!isCurrent() || p === null) return;
      applyProjection(p, {
        allowInstanceChange: event === "snapshot",
        responseGeneration: subscriptionGenerationRef.current,
      });
    };

    const pollOnce = async () => {
      // 请求发出时的代际：若在途期间 SSE 已推进到新 token，旧 GET 响应作废
      const startedToken = tokenRef.current;
      // T02：响应必须带回发出时的订阅代际才可落地
      const requestGeneration = subscriptionGenerationRef.current;
      // R03：握手票随**请求**捕获。响应期再读会被降级点的补票/再武装污染——
      // 重启前已发出的挂起 GET 捕获的是旧票号（或 null），结构性无法借后续
      // 补票获得跨 instance 建立权。
      const ticketAtRequest = handshakeTicketRef.current?.seq ?? null;
      try {
        const p = await fetchCodexThread(sid, ctrl.signal);
        if (!isCurrent() || p === null || tokenRef.current !== startedToken) return;
        applyProjection(p, {
          // 仅当捕获的票仍是当前有效票（未消费、未被新票取代）才有权确立新
          // instance；此后的 GET 无权切换（含切回旧 instance）。
          allowInstanceChange: ticketGrantsInstanceChange(ticketAtRequest),
          responseGeneration: requestGeneration,
        });
      } catch {
        /* 单次轮询失败忽略（保留最后视图），下一轮再试 */
      }
    };

    const startPoll = () => {
      if (!alive || pollTimer !== null) return;
      // R03：SSE→poll 降级点发放一张**新号**握手票。降级前发出的请求捕获的是
      // 旧号/null，无法借这张票越权；降级后的首个跨 instance GET 用它确立新
      // instance（票随建立消费）。消费后的再次重启由「连续跨 instance 拒绝 →
      // 有界再武装」补票——不再像旧布尔握手那样一次性锁死在旧 instance 视图。
      crossInstanceRejectStreakRef.current = 0;
      mintHandshakeTicket();
      setTransport("poll");
      void pollOnce();
      pollTimer = setInterval(() => {
        void pollOnce();
      }, CODEX_THREAD_POLL_MS);
    };

    const stop = subscribeCodexThread(sid, applyFrame, startPoll);

    return () => {
      alive = false;
      enabledRef.current = false;
      stop();
      if (pollTimer !== null) clearInterval(pollTimer);
      ctrl.abort(); // 在途轮询 fetch 一并终止
      invalidateHistory();
    };
  }, [
    sessionId,
    enabled,
    applyProjection,
    invalidateHistory,
    mintHandshakeTicket,
    ticketGrantsInstanceChange,
  ]);

  const refresh = useCallback(async () => {
    const sid = sessionId;
    // 请求发出时的代际：在途期间已有更新帧落地则丢弃本次响应（旧 token 不覆盖新状态）
    const startedToken = tokenRef.current;
    // T02：响应必须带回发出时的订阅代际才可落地
    const requestGeneration = subscriptionGenerationRef.current;
    // R03：握手票随请求捕获（与 pollOnce 同理——响应期读会被降级点的补票污染，
    // 重启前已发出的挂起刷新不得借后续补票切换 instance）。
    const ticketAtRequest = handshakeTicketRef.current?.seq ?? null;
    try {
      const p = await fetchCodexThread(sid);
      if (p === null || sessionRef.current !== sid || tokenRef.current !== startedToken) return;
      applyProjection(p, {
        // 仅当捕获的票仍是当前有效票才可切换 instance（SSE 在线时票为 null：
        // 手动刷新无权把旧服务实例装回——T02 既有纪律）。
        allowInstanceChange: ticketGrantsInstanceChange(ticketAtRequest),
        responseGeneration: requestGeneration,
      });
    } catch {
      /* 手动刷新失败静默（详情页保留现有内容） */
    }
  }, [sessionId, applyProjection, ticketGrantsInstanceChange]);

  const loadEarlier = useCallback<LoadEarlier>(
    async (before) => {
      const sid = sessionId;
      if (!enabledRef.current || sessionRef.current !== sid) {
        return { status: "unavailable", reasonCode: "subscription_disabled", page: null };
      }
      const current = stateRef.current;
      if (current === null) {
        return { status: "unavailable", reasonCode: "projection_unavailable", page: null };
      }
      const anchorToken = current.historyToken ?? "";
      if (anchorToken === "") {
        // 旧载荷没有历史代际 token：不猜 token 分页（服务端会拒绝无 token 请求）
        return { status: "unavailable", reasonCode: "history_token_missing", page: null };
      }
      const chain = chainRef.current;
      // 显式游标 = 重锚：忽略本地游标链，直接从该游标向前读。定位用它补齐「本地连续
      // 前沿」与旧缓存页之间的缺口——绝不能拿旧链的游标越过缺口（复核 S1）。
      const explicit = before !== undefined;
      if (!explicit && chain !== null && chain.ended) {
        return { status: "end", reasonCode: null, page: null };
      }
      // 每次定位固定「当前尾窗 + 游标链」：未分页用尾窗最早一条，已分页用上页游标
      const beforeId = explicit
        ? before
        : chain !== null
          ? chain.beforeId
          : (current.messages[0]?.id ?? null);
      // 显式游标没有前缀形 token（客户端不持有该页前缀）：用根锚点 token——服务端对
      // 根锚点只校验代际与内容代，游标本身单独校验。
      const requestToken = explicit || chain === null ? anchorToken : chain.token;
      // 同会话同代际同游标共享在途请求（Task 7 连续调度不重复取页）
      const inflight = inflightRef.current;
      if (
        inflight !== null &&
        inflight.sid === sid &&
        inflight.anchorToken === anchorToken &&
        inflight.beforeId === beforeId
      ) {
        return inflight.promise;
      }
      if (inflight !== null) {
        // 防御：代际变化本应已在 applyProjection 里 abort 并清空
        inflightRef.current = null;
        inflight.ctrl.abort();
      }
      const generation = generationRef.current;
      const ctrl = new AbortController();

      const promise = (async (): Promise<CodexHistoryLoadResult> => {
        let page: MessagesPage;
        try {
          page = await fetchCodexMessagesPage({
            sessionId: sid,
            historyToken: requestToken,
            beforeId,
            limit: CODEX_HISTORY_PAGE_LIMIT,
            signal: ctrl.signal,
          });
        } catch {
          // 主动 abort（token 换代/切会话/禁用订阅）是**失效态**而非加载失败：
          // 与落地后的代际校验同一语义，避免 Task 7 把失效显示成「加载失败/重试」。
          if (ctrl.signal.aborted) {
            return { status: "unavailable" as const, reasonCode: "stale_response", page: null };
          }
          return { status: "unavailable" as const, reasonCode: "request_failed", page: null };
        }
        // 迟到/失效：会话、代际、当前 token 或 abort 任一变化都不得落地
        if (
          ctrl.signal.aborted ||
          !enabledRef.current ||
          sessionRef.current !== sid ||
          generation !== generationRef.current ||
          (stateRef.current?.historyToken ?? "") !== anchorToken
        ) {
          return { status: "unavailable" as const, reasonCode: "stale_response", page: null };
        }
        if (page.status === "ready" && page.messages.length > CODEX_HISTORY_PAGE_LIMIT) {
          // 协议违约（服务端契约单页 ≤50）：不裁剪、不推进游标链、不动已有缓存，
          // 让调用方拿到明确的不可用态，而不是在裁剪接缝上继续翻出一个永久空洞。
          return { status: "unavailable" as const, reasonCode: "page_over_limit", page: null };
        }
        if (page.status === "ready" && page.messages.length > 0) {
          const stored = normalizeCodexHistoryPage({
            anchorToken,
            beforeId,
            messages: page.messages,
            nextBefore: page.nextBefore,
            nextToken: page.historyToken,
          });
          if (stored === null) {
            // 防御：上面的长度守卫已覆盖；归一化拒绝时不落地、不消耗游标
            return { status: "unavailable" as const, reasonCode: "page_over_limit", page: null };
          }
          const appended = appendCodexHistoryPage(pagesRef.current, stored, {
            // 新链第一页 / 显式重锚页都紧邻当前前沿 → 插到最新端；沿链继续 → 追加到更旧端
            front: chain === null || explicit,
          });
          pagesRef.current = appended.pages;
          setHistoryPages(appended.pages);
          if (appended.dropped) {
            // 展示缓存预算丢弃本页：下次从当前尾窗重新锚定，避免跳过被丢弃页产生空洞
            chainRef.current = null;
          } else {
            chainRef.current = {
              beforeId: page.nextBefore,
              token: page.historyToken,
              ended: page.nextBefore === null || page.historyToken === "",
            };
          }
          return { status: "ready", reasonCode: page.reasonCode, page: stored };
        }
        if (page.status === "end") {
          chainRef.current = { beforeId: null, token: page.historyToken, ended: true };
          return { status: "end", reasonCode: page.reasonCode, page: null };
        }
        if (page.status === "cursor_invalid") {
          // 游标/前缀失效（历史改写、游标不存在）：页、保留缓冲与游标链一并作废。
          // 保留缓冲不能等「下一帧 SSE 换 token」——SSE 断开或轮询窗口内，它已经
          // 是服务端明确告知不可信的历史（复核 F5）。
          invalidateHistory();
          return { status: page.status, reasonCode: page.reasonCode, page: null };
        }
        if (page.status === "unavailable" && page.reasonCode === "generation_mismatch") {
          // 服务端代际已换代（重建/owner 换代）但推送帧未到：旧页与保留缓冲整体失效
          invalidateHistory();
          return { status: page.status, reasonCode: page.reasonCode, page: null };
        }
        if (page.status === "ready") {
          // ready 却不带消息：协议违约，不当作「一页」消耗游标链
          return { status: "unavailable" as const, reasonCode: "empty_page", page: null };
        }
        // loading / 其他 unavailable：历史不完整或暂不可用——不消耗成完整一页，保留游标链
        return { status: page.status, reasonCode: page.reasonCode, page: null };
      })();

      const request: InflightPageRequest = {
        sid,
        anchorToken,
        beforeId,
        generation,
        ctrl,
        promise,
      };
      inflightRef.current = request;
      promise.then(
        () => {
          if (inflightRef.current === request) inflightRef.current = null;
        },
        () => {
          if (inflightRef.current === request) inflightRef.current = null;
        }
      );
      return promise;
    },
    [sessionId, invalidateHistory]
  );

  const loadedMessages = useMemo(
    () =>
      mergeCodexHistoryWithRetained(
        state?.messages ?? [],
        historyPages,
        retained,
        state?.historyToken ?? ""
      ),
    [state, historyPages, retained]
  );

  return { state, loadedMessages, historyPages, retained, loadEarlier, refresh, transport };
}
