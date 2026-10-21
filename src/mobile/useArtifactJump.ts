// 任务 7：从文件身份触发“最近一次正文引用”的有界定位状态机（spec §4.2 / §4.3）。
//
// 边界（钉值，勿放宽）：
// - 第 1 页 = 动作发起时的**实时尾窗**（`state.messages`，不是屏幕高度），
//   定义与 spec §4.2 一致，不并入保留缓冲；
// - 第 2..k 页 = 从尾窗向前**连续、有界**的更早批次（每批 ≤50 条）：刚滚出尾窗
//   的保留缓冲先填（它们仍在列表里显示，否则「看得见、定位不到」），再是已缓存
//   的更早页，最后沿游标链读取。三者共用同一个批次预算——本地数据不能扩大边界
//   （复核 G1：全部 retained 并入第一页会把范围从 700 条扩到 2500 条）；
// - 其后沿 `loadEarlier` 继续读取，直到**不同页**的已搜索页数达到五页；读到的页
//   按新→旧顺序逐个检查。五页预算计**不同页**：Task 5 在字节预算丢弃页时会重置
//   游标链，下一次读取会从尾窗回锚、重复返回已缓存的页——重复页既不再搜索，也不
//   消耗五页预算（否则真实第 4/5 页永远搜不到）；单次动作的 `loadEarlier` 调用
//   总数有硬上限**四次**（`ARTIFACT_JUMP_MAX_EARLIER_READS`，spec §4.2 第 4 条）：重复/
//   回锚页同样占掉四次中的一次，四次用尽仍不足五页不同页时以
//   `load_failed/page_repeated` 诚实终止，绝不伪造“五页未找到”。
// - 命中即停（当前页命中零分页请求）；五页查完未命中 = `miss`；服务端明确到头 =
//   `end`（两者都不提示继续加载，`miss` 也不暗示文件从未被引用）。
//
// 失败 ≠ 未命中（spec §4.3）：loading / 历史不可用 / 游标失效 / 代际失效立即结束，
// 各自给出独立原因（`history_incomplete` / `load_failed` / `cursor_invalid` /
// `generation_invalid`）并保留可重试语义——失败尝试**不计数成“已搜索的一页”**，
// 也不继续读取。新点击 = 新动作；会话切换/卸载作废在途动作，迟到响应不落地。
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { ProjectedMessage, ThreadProjection } from "./codexIpcApi";
import type { SessionFileEntry } from "./api";
import { selectCodexHistoryWindow, type CodexHistoryPage } from "./codex-history";
import type { CodexHistoryLoadResult } from "./useCodexThread";
import {
  findLastFileReference,
  type ArtifactJumpTarget,
  type FileReferenceHit,
} from "./artifact-links";

/** 单次定位最多搜索的页批次（含当前尾窗；spec §4.2 钉值）。不同页计数。 */
export const ARTIFACT_JUMP_MAX_PAGES = 5;
/**
 * 单次动作允许的 `loadEarlier` 调用总数（spec §4.2 第 4 条「最多四次更早页读取」）。
 * 重复/回锚页也占一次读取尝试（它仍是一次真实读取），只是不推进不同页计数；
 * 四次读完仍不足五页不同页时以失败（`page_repeated`）收尾，绝不越过读取上限。
 */
export const ARTIFACT_JUMP_MAX_EARLIER_READS = ARTIFACT_JUMP_MAX_PAGES - 1;

/** 定位失败原因（各自独立提示；不伪装成正常未命中）。 */
export type ArtifactJumpFailureKind =
  "history_incomplete" | "load_failed" | "cursor_invalid" | "generation_invalid";

export interface ArtifactJumpFailure {
  kind: ArtifactJumpFailureKind;
  /** 服务端/客户端机器码（只进 data 属性，不上屏）。 */
  code: string | null;
}

/** 命中结果 + 命中页批次（1 = 当前尾窗）。 */
export interface ArtifactJumpHit extends FileReferenceHit {
  page: number;
}

export type ArtifactJumpPhase = "idle" | "searching" | "hit" | "miss" | "end" | "failed";

export interface ArtifactJumpState {
  phase: ArtifactJumpPhase;
  /** 本次动作已搜索的**不同页**数（含尾窗；1..5）。重复页/失败尝试不计。 */
  pages: number;
  /** 本次动作更早页读取成功次数（含命中预算前的重复页；失败尝试不计）。 */
  reads: number;
  hit: ArtifactJumpHit | null;
  failure: ArtifactJumpFailure | null;
}

export interface UseArtifactJumpOptions {
  /** 会话切换哨兵：变化即清空状态并作废在途动作。 */
  sessionId: string;
  /** 当前投影（`messages` = 实时尾窗）。 */
  state: ThreadProjection | null;
  /** Task 5 的更早页展示缓存（同代；本 hook 只取预算内的页，绝不越过五页边界）。 */
  historyPages: readonly CodexHistoryPage[];
  /** Task 5 的保留缓冲：已滚出尾窗、仍显示的消息，并入第 1 页（复核 F2）。 */
  retained: readonly ProjectedMessage[];
  /** Task 5 的沿游标链取页（每次 = 一次更早页读取）。
   *  传入 `beforeId` 时从该游标重锚——定位遇到缺口时用它补页，不越过缺口取旧页。 */
  loadEarlier: (beforeId?: string | null) => Promise<CodexHistoryLoadResult>;
  /** 已知文件条目（正文链接 → fileKey 映射；不从正文抽新文件）。 */
  fileEntries: readonly SessionFileEntry[];
}

export interface UseArtifactJumpResult {
  state: ArtifactJumpState;
  /** 发起一次定位（每次点击 = 新动作，重置计数、作废旧动作）。 */
  jump: (target: ArtifactJumpTarget) => Promise<void>;
  /** 重试上一目标（失败态的可重试入口；没有目标时 no-op）。 */
  retry: () => Promise<void>;
  /** 结束本次动作（关闭提示；清空状态）。 */
  reset: () => void;
}

const IDLE: ArtifactJumpState = {
  phase: "idle",
  pages: 0,
  reads: 0,
  hit: null,
  failure: null,
};

/** `loadEarlier` 结果 → 独立的失败原因（绝不返回 null 伪装成未命中）。 */
function failureOf(result: CodexHistoryLoadResult): ArtifactJumpFailure {
  const code = result.reasonCode;
  if (result.status === "cursor_invalid") return { kind: "cursor_invalid", code };
  if (result.status === "loading") return { kind: "history_incomplete", code };
  if (code === "generation_mismatch" || code === "stale_response") {
    return { kind: "generation_invalid", code };
  }
  if (code === "request_failed" || code === "page_over_limit" || code === "empty_page") {
    return { kind: "load_failed", code };
  }
  if (result.status === "unavailable") return { kind: "history_incomplete", code };
  return { kind: "load_failed", code };
}

export default function useArtifactJump(options: UseArtifactJumpOptions): UseArtifactJumpResult {
  const {
    sessionId,
    state: projection,
    historyPages,
    // 缺省空缓冲：老调用方（未接线 Task 5 保留缓冲）行为与之前一致
    retained = [],
    loadEarlier,
    fileEntries,
  } = options;
  const [state, setState] = useState<ArtifactJumpState>(IDLE);
  /** 动作代际：新动作/换会话/卸载都递增，旧动作的迟到结果一律丢弃。 */
  const actionRef = useRef(0);
  const sessionRef = useRef(sessionId);
  const targetRef = useRef<ArtifactJumpTarget | null>(null);
  const mountedRef = useRef(true);

  // 会话切换必须**同步于渲染提交**生效：在途页的续体是微任务，被动 effect 落地前
  // 可能先跑一帧并把旧会话结果写到新会话上。布局 effect 在 commit 阶段同步执行
  // （早于任何后续微任务），哨兵/代际在同一处推进；渲染期不触碰 ref（react-hooks/refs）。
  useLayoutEffect(() => {
    sessionRef.current = sessionId;
    actionRef.current += 1;
    targetRef.current = null;
    setState(IDLE);
  }, [sessionId]);

  // 卸载：作废在途动作（StrictMode 重挂时重新置位）
  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      actionRef.current += 1;
    };
  }, []);

  const jump = useCallback(
    async (target: ArtifactJumpTarget): Promise<void> => {
      const actionId = actionRef.current + 1;
      actionRef.current = actionId;
      targetRef.current = target;
      const sid = sessionId;
      const isCurrent = () =>
        mountedRef.current && sessionRef.current === sid && actionRef.current === actionId;

      if (projection === null) {
        // 还没有可搜索的投影：历史不完整（不是未命中）
        setState({
          phase: "failed",
          pages: 0,
          reads: 0,
          hit: null,
          failure: { kind: "history_incomplete", code: "projection_unavailable" },
        });
        return;
      }
      setState({ phase: "searching", pages: 1, reads: 0, hit: null, failure: null });

      // 五页预算计**不同页**：已搜索消息 id 集合用于识别重复页（Task 5 链重置回锚 /
      // 页缝重复）。重复页不再搜索、不推进不同页计数，但仍消耗一次读取尝试（它仍是
      // 一次真实读取），读取尝试总数恒 ≤ 四次（spec §4.2 第 4 条，不因重复页放大）。
      const searchedIds = new Set<string>();
      const markSearched = (messages: readonly ProjectedMessage[]) => {
        for (const message of messages) searchedIds.add(message.id);
      };
      const freshCount = (messages: readonly ProjectedMessage[]) => {
        let fresh = 0;
        for (const message of messages) if (!searchedIds.has(message.id)) fresh += 1;
        return fresh;
      };

      // 五页窗口的选页结果（与文件池共用同一份判断）
      const window = selectCodexHistoryWindow(
        projection.messages,
        historyPages,
        retained,
        projection.historyToken ?? ""
      );

      // 第 1 页 = 实时尾窗（spec §4.2 定义不变）
      let searched = 1;
      let reads = 0;
      markSearched(projection.messages);
      const tailHit = findLastFileReference(projection.messages, target, fileEntries);
      if (tailHit !== null) {
        setState({
          phase: "hit",
          pages: searched,
          reads,
          hit: { ...tailHit, page: searched },
          failure: null,
        });
        return;
      }

      // 第 2..k 页 = 有界窗口给出的**连续**批次（retained → 相邻缓存页）。
      // 选页与池侧共用 `selectCodexHistoryWindow`：去重、连续性、批次预算只有一处
      // 定义，不会再出现「池侧少一页、定位多一页」（复核 S2）。
      //
      // 连续性（复核 S1）：retained 的 1 MiB 上限可能把中间一段驱逐掉，旧缓存页
      // 仍锚定更早位置。窗口只接受与前沿相邻的页；有缺口时 frontier 停在缺口前，
      // 下面从该前沿重新锚定补页——绝不越过缺口去拿更旧引用报命中。
      let frontier = window.frontierBeforeId;
      for (const batch of window.batches.slice(1)) {
        if (searched >= ARTIFACT_JUMP_MAX_PAGES) break;
        if (!isCurrent()) return;
        if (freshCount(batch) === 0) continue;
        searched += 1;
        const hit = findLastFileReference(batch, target, fileEntries);
        markSearched(batch);
        if (hit !== null) {
          setState({
            phase: "hit",
            pages: searched,
            reads,
            hit: { ...hit, page: searched },
            failure: null,
          });
          return;
        }
        setState({ phase: "searching", pages: searched, reads, hit: null, failure: null });
      }

      // 本地连续数据用尽（或遇到缺口）后，从**实际前沿**重锚读取；读取调用总数
      // 恒 ≤ 四次（重复页也占一次），页数预算仍是五页。
      let attempts = 0;
      while (
        searched < ARTIFACT_JUMP_MAX_PAGES &&
        attempts < ARTIFACT_JUMP_MAX_EARLIER_READS &&
        frontier !== null
      ) {
        if (!isCurrent()) return;
        attempts += 1;
        const requested = frontier;
        const result = await loadEarlier(requested);
        if (!isCurrent()) return;
        if (result.status === "ready") {
          if (result.page === null || result.page.messages.length === 0) {
            // ready 却给不出页：协议违约，不能消耗成“已搜索的一页”
            setState({
              phase: "failed",
              pages: searched,
              reads,
              hit: null,
              failure: { kind: "load_failed", code: "missing_page" },
            });
            return;
          }
          reads += 1;
          const page = result.page;
          // 重复页（回锚重取）：不搜索、不消耗五页预算，继续沿链读取
          if (freshCount(page.messages) === 0) {
            setState({ phase: "searching", pages: searched, reads, hit: null, failure: null });
            continue;
          }
          frontier = page.nextBefore;
          searched += 1;
          const hit = findLastFileReference(page.messages, target, fileEntries);
          markSearched(page.messages);
          if (hit !== null) {
            setState({
              phase: "hit",
              pages: searched,
              reads,
              hit: { ...hit, page: searched },
              failure: null,
            });
            return;
          }
          setState({ phase: "searching", pages: searched, reads, hit: null, failure: null });
          continue;
        }
        if (result.status === "end") {
          // 真正到头：结束（不是“五页未命中”，也不提示继续加载）
          setState({ phase: "end", pages: searched, reads, hit: null, failure: null });
          return;
        }
        // loading / unavailable / cursor_invalid：失败/失效立即停止，独立原因
        setState({
          phase: "failed",
          pages: searched,
          reads,
          hit: null,
          failure: failureOf(result),
        });
        return;
      }

      if (searched >= ARTIFACT_JUMP_MAX_PAGES) {
        setState({ phase: "miss", pages: searched, reads, hit: null, failure: null });
        return;
      }
      // 四次读取用尽仍不足五页不同页（重复/回锚页耗掉了读取次数）：如实报读取失败，
      // 不伪装成“五页未命中”；用户可重试（新动作）
      setState({
        phase: "failed",
        pages: searched,
        reads,
        hit: null,
        failure: { kind: "load_failed", code: "page_repeated" },
      });
    },
    [sessionId, projection, historyPages, retained, loadEarlier, fileEntries]
  );

  const retry = useCallback(async (): Promise<void> => {
    const target = targetRef.current;
    if (target === null) return;
    await jump(target);
  }, [jump]);

  const reset = useCallback((): void => {
    actionRef.current += 1;
    targetRef.current = null;
    setState(IDLE);
  }, []);

  return { state, jump, retry, reset };
}
