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
import { useCallback, useEffect, useRef, useState } from "react";
import { fetchCodexThread, subscribeCodexThread, type ThreadProjection } from "./codexIpcApi";

/** 降级后的 GET 轮询周期（计划钉值：3s） */
export const CODEX_THREAD_POLL_MS = 3000;

/** 传输通道：sse = 事件流在线；poll = SSE 三连失败后的 GET 轮询；off = 未启用 */
export type CodexTransport = "sse" | "poll" | "off";

export interface UseCodexThreadResult {
  state: ThreadProjection | null;
  /** 手动重拉（只读 GET；transport 不变） */
  refresh: () => Promise<void>;
  transport: CodexTransport;
}

export default function useCodexThread(sessionId: string, enabled: boolean): UseCodexThreadResult {
  const [state, setState] = useState<ThreadProjection | null>(null);
  const [transport, setTransport] = useState<CodexTransport>("off");
  /** 当前会话哨兵：apply 前比对，旧会话的迟到帧/响应不得写入 */
  const sessionRef = useRef(sessionId);

  useEffect(() => {
    if (!enabled) {
      setTransport("off");
      return;
    }
    sessionRef.current = sessionId;
    const sid = sessionId;
    let alive = true;
    let pollTimer: ReturnType<typeof setInterval> | null = null;
    // 轮询 fetch 的中止器：卸载即 abort（在途请求不再产生任何效果）
    const ctrl = new AbortController();

    // 换会话先清旧投影：旧会话的内容不得跨会话显示（防串台第一拍）
    setState(null);
    setTransport("sse");

    const isCurrent = () => alive && sessionRef.current === sid;

    /** 投影落地（SSE 帧与轮询 GET 共用入口）：非当前会话/已卸载一律丢弃 */
    const apply = (p: ThreadProjection | null) => {
      if (!isCurrent() || p === null) return;
      setState(p);
    };

    const pollOnce = async () => {
      try {
        apply(await fetchCodexThread(sid, ctrl.signal));
      } catch {
        /* 单次轮询失败忽略（保留最后视图），下一轮再试 */
      }
    };

    const startPoll = () => {
      if (!alive || pollTimer !== null) return;
      setTransport("poll");
      void pollOnce();
      pollTimer = setInterval(() => {
        void pollOnce();
      }, CODEX_THREAD_POLL_MS);
    };

    const stop = subscribeCodexThread(sid, apply, startPoll);

    return () => {
      alive = false;
      stop();
      if (pollTimer !== null) clearInterval(pollTimer);
      ctrl.abort(); // 在途轮询 fetch 一并终止
    };
  }, [sessionId, enabled]);

  const refresh = useCallback(async () => {
    const sid = sessionId;
    try {
      const p = await fetchCodexThread(sid);
      // 手动刷新同样受会话哨兵约束：响应落地前切走则丢弃
      if (p !== null && sessionRef.current === sid) setState(p);
    } catch {
      /* 手动刷新失败静默（详情页保留现有内容） */
    }
  }, [sessionId]);

  return { state, refresh, transport };
}
