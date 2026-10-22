// 任务 7：`useArtifactJump` 的五页有界查找状态机。
//
// 固定边界（spec §4.2）：第 1 页 = 动作发起时的实时尾窗；其后最多四次更早页读取
// （连同尾窗共五页）。已缓存的更早页只能作为**同一边界内的页批次**参与，缓存再多
// 也不能把搜索扩大到第六页；每页最多 50 条（服务端契约，超限由 Task 5 拒绝）。
// 五页预算计**不同页**：Task 5 在字节预算丢弃页时会重置游标链，读取会回锚并重复
// 返回已搜索页——重复页不推进不同页计数（否则真实第 4/5 页永远搜不到），但仍占
// 一次读取尝试；单次动作的 `loadEarlier` 调用总数恒 ≤ 四次（spec §4.2.4）。
// 失败 ≠ 未命中：loading / unavailable / cursor_invalid / 代际失效各自结束并给出
// 独立原因，绝不消耗成“已搜索的空页”，也不伪装成正常未命中。
import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import useArtifactJump, {
  ARTIFACT_JUMP_MAX_EARLIER_READS,
  ARTIFACT_JUMP_MAX_PAGES,
  type UseArtifactJumpOptions,
} from "@/mobile/useArtifactJump";
import type { SessionFileEntry } from "@/mobile/api";
import type { CodexHistoryLoadResult, CodexHistoryPage } from "@/mobile/codex-history";
import type { ProjectedMessage, ThreadProjection } from "@/mobile/codexIpcApi";

const TARGET_PATH = "/tmp/proj/target.md";

function entry(path: string, over: Partial<SessionFileEntry> = {}): SessionFileEntry {
  return {
    path,
    fileKey: `fk:${path}`,
    lastSeq: 1,
    lastTs: null,
    hits: 1,
    modified: false,
    origin: "user",
    ...over,
  };
}

function msg(id: string, text = ""): ProjectedMessage {
  return { id, role: "user", kind: "userMessage", text, ts: null };
}

function projection(messages: ProjectedMessage[], historyToken = "tok-1"): ThreadProjection {
  return {
    threadId: "thread-1",
    revision: 1,
    stale: false,
    availability: "controllable",
    capabilities: {
      send: true,
      state: true,
      command: true,
      permissions: false,
      blockingInput: true,
    },
    runtimeStatus: "idle",
    contentState: "ready",
    messages,
    truncated: false,
    historyToken,
    pending: [],
  };
}

function historyPage(
  messages: ProjectedMessage[],
  over: Partial<CodexHistoryPage> = {}
): CodexHistoryPage {
  return {
    anchorToken: "tok-1",
    beforeId: "cursor",
    messages,
    nextBefore: "next-cursor",
    nextToken: "tok-1",
    bytes: 10,
    ...over,
  };
}

/** 连续读取页序列：beforeId/nextBefore 首尾相接（Task 5 游标链），默认从尾窗首条开始。 */
function chainedPages(batches: ProjectedMessage[][], startBeforeId = "tail-1"): CodexHistoryPage[] {
  return batches.map((messages, index) =>
    historyPage(messages, {
      beforeId: index === 0 ? startBeforeId : `cursor-${index}`,
      nextBefore: `cursor-${index + 1}`,
    })
  );
}

function ready(page: CodexHistoryPage): CodexHistoryLoadResult {
  return { status: "ready", reasonCode: null, page };
}
function end(): CodexHistoryLoadResult {
  return { status: "end", reasonCode: null, page: null };
}
function unavailable(reasonCode: string | null): CodexHistoryLoadResult {
  return { status: "unavailable", reasonCode, page: null };
}
function loading(): CodexHistoryLoadResult {
  return { status: "loading", reasonCode: null, page: null };
}
function cursorInvalid(): CodexHistoryLoadResult {
  return { status: "cursor_invalid", reasonCode: "cursor_not_found", page: null };
}

const target = { fileKey: `fk:${TARGET_PATH}`, path: TARGET_PATH };
const entries = [entry(TARGET_PATH)];

function setup(
  over: Partial<UseArtifactJumpOptions> & Pick<UseArtifactJumpOptions, "loadEarlier">
) {
  const options: UseArtifactJumpOptions = {
    sessionId: "s1",
    state: projection([msg("tail-1")]),
    historyPages: [],
    retained: [],
    fileEntries: entries,
    ...over,
  };
  return renderHook((props: UseArtifactJumpOptions) => useArtifactJump(props), {
    initialProps: options,
  });
}

describe("useArtifactJump：五页边界与页序", () => {
  it("当前页命中：零分页请求，状态为 hit", async () => {
    const loadEarlier = vi.fn(async () => ready(historyPage([msg("p2")])));
    const { result } = setup({
      loadEarlier,
      state: projection([msg("tail-1", `看 ${TARGET_PATH}`)]),
    });
    await act(async () => {
      await result.current.jump(target);
    });
    expect(result.current.state.phase).toBe("hit");
    expect(result.current.state.hit?.messageId).toBe("tail-1");
    expect(result.current.state.pages).toBe(1);
    expect(loadEarlier).not.toHaveBeenCalled();
  });

  it("近处接缝可命中：已滚出尾窗、仍在显示的消息作为第 2 页参与搜索（零请求）", async () => {
    // 尾窗前移后接缝消息仍在显示；搜索边界仍是五批，它落在第 2 批。
    const loadEarlier = vi.fn(async () => ready(historyPage([msg("p2")])));
    const { result } = setup({
      loadEarlier,
      state: projection([msg("tail-1"), msg("tail-2")]),
      retained: [msg("seam-1", `看 ${TARGET_PATH}`)],
    });
    await act(async () => {
      await result.current.jump(target);
    });
    expect(result.current.state.phase).toBe("hit");
    expect(result.current.state.hit?.messageId).toBe("seam-1");
    expect(result.current.state.pages).toBe(2);
    expect(loadEarlier).not.toHaveBeenCalled();
  });

  it("远处 retained 不命中：超过四批（200 条）的保留消息不进入搜索，也不发请求", async () => {
    // 复核 G1：五页 = 尾窗 + 4×50，本地 retained 不能扩大边界。
    const loadEarlier = vi.fn(async () => ready(historyPage([msg("p2")])));
    const far = Array.from({ length: 2000 }, (_, i) => msg(`r${i}`));
    far[0] = msg("r0", `看 ${TARGET_PATH}`); // 最旧一条：落在第 5 批之外
    const { result } = setup({
      loadEarlier,
      state: projection([msg("tail-1")]),
      retained: far,
    });
    await act(async () => {
      await result.current.jump(target);
    });
    expect(result.current.state.phase).toBe("miss");
    expect(result.current.state.pages).toBe(ARTIFACT_JUMP_MAX_PAGES);
    expect(loadEarlier).not.toHaveBeenCalled();
  });

  it("缺口不得越过：retained 丢掉中段时从真实前沿补页，命中更近引用（S1）", async () => {
    // 复核 S1：retained 的字节上限把 m100—m593 驱逐后，旧缓存页（m050—m099）与
    // 前沿（m594）之间就有缺口。正确行为是从 m594 补页取到 m580；绝不能越过缺口
    // 直接搜缓存页、把更旧的 m070 报成「最近引用」。
    const range = (prefix: string, start: number, count: number) =>
      Array.from({ length: count }, (_, i) => msg(`${prefix}${start + i}`));
    const tail = range("m", 600, 4);
    const retained = range("m", 594, 6);
    const cached = [
      historyPage(
        range("m", 50, 50).map((message) =>
          message.id === "m70" ? msg("m70", `看 ${TARGET_PATH}`) : message
        ),
        { beforeId: "m100", nextBefore: "m050" }
      ),
    ];
    const loadEarlier = vi.fn(async () =>
      ready(
        historyPage(
          range("m", 544, 50).map((message) =>
            message.id === "m580" ? msg("m580", `看 ${TARGET_PATH}`) : message
          ),
          { beforeId: "m594", nextBefore: "m544" }
        )
      )
    );
    const { result } = setup({
      loadEarlier,
      state: projection(tail),
      retained,
      historyPages: cached,
    });
    await act(async () => {
      await result.current.jump(target);
    });
    expect(result.current.state.phase).toBe("hit");
    expect(result.current.state.hit?.messageId).toBe("m580");
    // 补页必须从真实前沿重锚，不是旧链的 m100
    expect(loadEarlier.mock.calls[0]?.[0]).toBe("m594");
  });

  it("第二至第五页按顺序读取，命中即停", async () => {
    const pages = chainedPages([
      [msg("p2", "无关")],
      [msg("p3", "无关")],
      [msg("p4", `看 ${TARGET_PATH}`)],
      [msg("p5", "更早")],
    ]);
    let index = 0;
    const loadEarlier = vi.fn(async () => ready(pages[index++]));
    const { result } = setup({ loadEarlier });
    await act(async () => {
      await result.current.jump(target);
    });
    expect(loadEarlier).toHaveBeenCalledTimes(3);
    expect(result.current.state.phase).toBe("hit");
    expect(result.current.state.hit?.messageId).toBe("p4");
    expect(result.current.state.pages).toBe(4);
    expect(result.current.state.reads).toBe(3);
  });

  it("第五页最后一条消息也能命中（四次更早读取用满）", async () => {
    const pages = chainedPages([
      [msg("p2")],
      [msg("p3")],
      [msg("p4")],
      [msg("p5-first"), msg("p5-last", `看 ${TARGET_PATH}`)],
    ]);
    let index = 0;
    const loadEarlier = vi.fn(async () => ready(pages[index++]));
    const { result } = setup({ loadEarlier });
    await act(async () => {
      await result.current.jump(target);
    });
    expect(loadEarlier).toHaveBeenCalledTimes(4);
    expect(result.current.state.phase).toBe("hit");
    expect(result.current.state.hit?.messageId).toBe("p5-last");
    expect(result.current.state.pages).toBe(ARTIFACT_JUMP_MAX_PAGES);
  });

  it("五页未命中：恰好四次更早读取后停止，不继续加载", async () => {
    const pages = chainedPages([[msg("p2")], [msg("p3")], [msg("p4")], [msg("p5")]]);
    let index = 0;
    const loadEarlier = vi.fn(async () => ready(pages[index++]));
    const { result } = setup({ loadEarlier });
    await act(async () => {
      await result.current.jump(target);
    });
    expect(loadEarlier).toHaveBeenCalledTimes(4);
    expect(result.current.state.phase).toBe("miss");
    expect(result.current.state.pages).toBe(5);
    expect(result.current.state.reads).toBe(4);
    expect(result.current.state.hit).toBeNull();
  });

  it("真到头：end 立即结束（不当作未命中、不继续读）", async () => {
    const loadEarlier = vi.fn(async () => end());
    const { result } = setup({ loadEarlier });
    await act(async () => {
      await result.current.jump(target);
    });
    expect(loadEarlier).toHaveBeenCalledTimes(1);
    expect(result.current.state.phase).toBe("end");
    expect(result.current.state.hit).toBeNull();
  });
});

describe("useArtifactJump：缓存不能扩大五页边界", () => {
  it("已缓存的第六页不搜索；缓存满五页时零读取后按未命中结束", async () => {
    // 注入 5 页缓存（超出 Task 5 的 4 页上限）：第五页（整体第六页）里的目标不得命中
    const cached = chainedPages([
      [msg("p2")],
      [msg("p3")],
      [msg("p4")],
      [msg("p5")],
      [msg("p6", `看 ${TARGET_PATH}`)],
    ]);
    const loadEarlier = vi.fn(async () => end());
    const { result } = setup({ loadEarlier, historyPages: cached });
    await act(async () => {
      await result.current.jump(target);
    });
    expect(result.current.state.hit).toBeNull();
    expect(result.current.state.phase).toBe("miss");
    expect(result.current.state.pages).toBe(5);
    expect(loadEarlier).not.toHaveBeenCalled();
  });

  it("缓存页按新到旧搜索：最近缓存页命中即停，零读取", async () => {
    const cached = chainedPages([[msg("p2", `看 ${TARGET_PATH}`)], [msg("p3")]]);
    const loadEarlier = vi.fn(async () => ready(historyPage([msg("p4")])));
    const { result } = setup({ loadEarlier, historyPages: cached });
    await act(async () => {
      await result.current.jump(target);
    });
    expect(result.current.state.phase).toBe("hit");
    expect(result.current.state.hit?.messageId).toBe("p2");
    expect(loadEarlier).not.toHaveBeenCalled();
  });

  it("缓存页之后继续按顺序读取，总页数仍受五页限制", async () => {
    const cached = chainedPages([[msg("p2")], [msg("p3")]]);
    const loadEarlier = vi.fn(async () =>
      ready(
        historyPage([msg("p4", `看 ${TARGET_PATH}`)], {
          beforeId: "cursor-2",
          nextBefore: "cursor-3",
        })
      )
    );
    const { result } = setup({ loadEarlier, historyPages: cached });
    await act(async () => {
      await result.current.jump(target);
    });
    expect(loadEarlier).toHaveBeenCalledTimes(1);
    expect(result.current.state.phase).toBe("hit");
    expect(result.current.state.hit?.messageId).toBe("p4");
    expect(result.current.state.pages).toBe(4);
  });

  it("代际不匹配的缓存页不参与搜索（不占页数、不命中）", async () => {
    const cached = [historyPage([msg("stale", `看 ${TARGET_PATH}`)], { anchorToken: "tok-old" })];
    const loadEarlier = vi.fn(async () => end());
    const { result } = setup({ loadEarlier, historyPages: cached });
    await act(async () => {
      await result.current.jump(target);
    });
    expect(result.current.state.hit).toBeNull();
    expect(result.current.state.phase).toBe("end");
    expect(loadEarlier).toHaveBeenCalledTimes(1);
  });
});

describe("useArtifactJump：Task 5 重置游标链后的重复页（五页预算计不同页）", () => {
  /** 3 页缓存 + 曾在字节预算处被丢弃的第 5 页：链被重置，读取从尾窗回锚。 */
  function cachedThree() {
    return chainedPages([[msg("p2")], [msg("p3")], [msg("p4")]]);
  }

  /** 回锚后 Task 5 依次重放 p2/p3/p4（重复页），再给出真实更早页 p5。 */
  function reanchoredReads(p5: ProjectedMessage[]): CodexHistoryPage[] {
    return [
      historyPage([msg("p2")], { beforeId: "tail-1", nextBefore: "cursor-1" }),
      historyPage([msg("p3")], { beforeId: "cursor-1", nextBefore: "cursor-2" }),
      historyPage([msg("p4")], { beforeId: "cursor-2", nextBefore: "cursor-3" }),
      historyPage(p5, { beforeId: "cursor-3", nextBefore: "cursor-4" }),
    ];
  }

  it("重复缓存页不推进不同页计数：真实第五页（曾因字节预算被丢弃）仍在四次读取内命中", async () => {
    expect(ARTIFACT_JUMP_MAX_EARLIER_READS).toBe(4); // 读取调用总数上限（spec §4.2.4）
    const pages = reanchoredReads([msg("p5", `看 ${TARGET_PATH}`)]);
    let index = 0;
    const loadEarlier = vi.fn(async () => ready(pages[index++]));
    const { result } = setup({ loadEarlier, historyPages: cachedThree() });
    await act(async () => {
      await result.current.jump(target);
    });
    // 回锚 p2、p3、p4 各占一次读取，第四次读到真实 p5 → 命中；仍不越四次上限
    expect(loadEarlier).toHaveBeenCalledTimes(4);
    expect(result.current.state.phase).toBe("hit");
    expect(result.current.state.hit?.messageId).toBe("p5");
    expect(result.current.state.pages).toBe(5);
    expect(result.current.state.reads).toBe(4);
  });

  it("同一重置场景全未命中：按五个不同页结束（不是重复页凑数的假 miss）", async () => {
    const pages = reanchoredReads([msg("p5", "更早，无引用")]);
    let index = 0;
    const loadEarlier = vi.fn(async () => ready(pages[index++]));
    const { result } = setup({ loadEarlier, historyPages: cachedThree() });
    await act(async () => {
      await result.current.jump(target);
    });
    expect(loadEarlier).toHaveBeenCalledTimes(4);
    expect(result.current.state.phase).toBe("miss");
    expect(result.current.state.pages).toBe(5);
    expect(result.current.state.hit).toBeNull();
  });

  it("回锚页持续重复：恰好在第四次读取后以读取失败收尾（不越过读取上限）", async () => {
    const reanchored = () =>
      ready(
        historyPage([msg("p2")], {
          beforeId: "tail-1",
          nextBefore: "cursor-1",
        })
      );
    const loadEarlier = vi.fn(async () => reanchored());
    const { result } = setup({ loadEarlier });
    await act(async () => {
      await result.current.jump(target);
    });
    expect(loadEarlier).toHaveBeenCalledTimes(4);
    expect(result.current.state.phase).toBe("failed");
    expect(result.current.state.failure?.kind).toBe("load_failed");
    expect(result.current.state.failure?.code).toBe("page_repeated");
    expect(result.current.state.pages).toBeLessThan(ARTIFACT_JUMP_MAX_PAGES);
  });
});

describe("useArtifactJump：失败与失效单独结束，不伪装未命中", () => {
  const cases: Array<[string, CodexHistoryLoadResult, string]> = [
    ["历史尚未载入（loading）", loading(), "history_incomplete"],
    ["历史不可用（unavailable）", unavailable("request_failed"), "load_failed"],
    ["其他不可用（mirror_unavailable）", unavailable("mirror_unavailable"), "history_incomplete"],
    ["游标失效（cursor_invalid）", cursorInvalid(), "cursor_invalid"],
    ["代际失效（generation_mismatch）", unavailable("generation_mismatch"), "generation_invalid"],
    ["代际失效（stale_response）", unavailable("stale_response"), "generation_invalid"],
    ["协议违约（page_over_limit）", unavailable("page_over_limit"), "load_failed"],
    ["ready 却无页", { status: "ready", reasonCode: null, page: null }, "load_failed"],
  ];

  it.each(cases)("%s：立即停止并给出独立原因", async (_label, failure, kind) => {
    const loadEarlier = vi.fn(async () => failure);
    const { result } = setup({ loadEarlier });
    await act(async () => {
      await result.current.jump(target);
    });
    expect(loadEarlier).toHaveBeenCalledTimes(1);
    expect(result.current.state.phase).toBe("failed");
    expect(result.current.state.failure?.kind).toBe(kind);
    // 失败尝试不消耗成“已搜索的一页”，也不再继续读取
    expect(result.current.state.pages).toBe(1);
  });

  it("没有投影（state=null）→ 历史不完整，零读取", async () => {
    const loadEarlier = vi.fn(async () => end());
    const { result } = setup({ loadEarlier, state: null });
    await act(async () => {
      await result.current.jump(target);
    });
    expect(loadEarlier).not.toHaveBeenCalled();
    expect(result.current.state.phase).toBe("failed");
    expect(result.current.state.failure?.kind).toBe("history_incomplete");
  });

  it("失败后再次点击是一次新动作（可重试并命中）", async () => {
    const loadEarlier = vi
      .fn<() => Promise<CodexHistoryLoadResult>>()
      .mockResolvedValueOnce(unavailable("request_failed"))
      .mockResolvedValueOnce(ready(historyPage([msg("p2", `看 ${TARGET_PATH}`)])));
    const { result } = setup({ loadEarlier });
    await act(async () => {
      await result.current.jump(target);
    });
    expect(result.current.state.phase).toBe("failed");
    // retry 复用上一目标，重新走完整五次预算
    await act(async () => {
      await result.current.retry();
    });
    expect(result.current.state.phase).toBe("hit");
    expect(result.current.state.hit?.messageId).toBe("p2");
    expect(loadEarlier).toHaveBeenCalledTimes(2);
  });
});

describe("useArtifactJump：动作/会话生命周期", () => {
  it("新动作重置计数：首次未命中四次读取，第二次当前页命中零读取", async () => {
    const pages = chainedPages([[msg("p2")], [msg("p3")], [msg("p4")], [msg("p5")]]);
    let index = 0;
    const loadEarlier = vi.fn(async () => ready(pages[index++]));
    const { result, rerender } = setup({ loadEarlier });
    await act(async () => {
      await result.current.jump(target);
    });
    expect(result.current.state.phase).toBe("miss");
    expect(loadEarlier).toHaveBeenCalledTimes(4);

    rerender({
      sessionId: "s1",
      state: projection([msg("tail-new", `看 ${TARGET_PATH}`)]),
      historyPages: [],
      retained: [],
      loadEarlier,
      fileEntries: entries,
    });
    await act(async () => {
      await result.current.jump(target);
    });
    expect(result.current.state.phase).toBe("hit");
    expect(result.current.state.pages).toBe(1);
    expect(loadEarlier).toHaveBeenCalledTimes(4);
  });

  it("会话切换与迟到页同拍：渲染期哨兵先更新，页面响应不落到新会话", async () => {
    let resolvePage: (value: CodexHistoryLoadResult) => void = () => {};
    const pending = new Promise<CodexHistoryLoadResult>((resolve) => {
      resolvePage = resolve;
    });
    const loadEarlier = vi.fn(() => pending);
    const { result, rerender } = setup({ loadEarlier });
    let jumpPromise: Promise<void> = Promise.resolve();
    act(() => {
      jumpPromise = result.current.jump(target);
    });
    expect(result.current.state.phase).toBe("searching");

    // 新会话渲染与迟到页解析在同一拍：页面续体（微任务）必须被渲染期哨兵作废
    act(() => {
      rerender({
        sessionId: "s2",
        state: projection([msg("other")]),
        historyPages: [],
        loadEarlier,
        fileEntries: entries,
      });
      resolvePage(ready(historyPage([msg("p2", `看 ${TARGET_PATH}`)])));
    });
    expect(result.current.state.phase).toBe("idle");

    await act(async () => {
      await jumpPromise;
    });
    expect(result.current.state.phase).toBe("idle");
    expect(result.current.state.hit).toBeNull();
  });

  it("未知来源（目标无 fileKey、正文无已知链接）也能发起查找，不预造 messageId", async () => {
    const pages = chainedPages([[msg("p2", "无关")], [msg("p3")], [msg("p4")], [msg("p5")]]);
    let index = 0;
    const loadEarlier = vi.fn(async () => ready(pages[index++]));
    const { result } = setup({ loadEarlier, fileEntries: [] });
    await act(async () => {
      await result.current.jump({ fileKey: null, path: "/tmp/proj/unknown.md" });
    });
    expect(result.current.state.phase).toBe("miss");
    expect(result.current.state.hit).toBeNull();
    // 未命中前不存在任何伪造的 messageId
    expect(result.current.state.pages).toBe(5);
  });

  it("reset 回到 idle（关闭提示/结束动作）", async () => {
    const loadEarlier = vi.fn(async () => ready(historyPage([msg("p2", `看 ${TARGET_PATH}`)])));
    const { result } = setup({ loadEarlier });
    await act(async () => {
      await result.current.jump(target);
    });
    expect(result.current.state.phase).toBe("hit");
    act(() => {
      result.current.reset();
    });
    expect(result.current.state.phase).toBe("idle");
    expect(result.current.state.hit).toBeNull();
  });
});
