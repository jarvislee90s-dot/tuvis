import { describe, expect, it } from "vitest";
import {
  CODEX_HISTORY_MAX_BYTES,
  CODEX_HISTORY_MAX_PAGES,
  CODEX_HISTORY_MAX_RETAINED_BYTES,
  CODEX_HISTORY_MAX_RETAINED_MESSAGES,
  CODEX_HISTORY_PAGE_LIMIT,
  appendCodexHistoryPage,
  boundedCodexHistoryWindow,
  capCodexHistoryPages,
  capCodexRetainedMessages,
  codexHistoryPageBytes,
  mergeCodexHistory,
  mergeCodexHistoryWithRetained,
  normalizeCodexHistoryPage,
  retainCodexHistoryMessages,
  type CodexHistoryPage,
  type CodexHistoryPageInput,
} from "@/mobile/codex-history";
import type { ProjectedMessage } from "@/mobile/codexIpcApi";

// Task 5：历史页纯合并 / 缓存预算锁（计划任务 5）。
// 规则来源：
// - `mergeCodexHistory` 按消息身份合并，canonical 顺序（旧→新，最新在最后），
//   当前 canonical 投影覆盖重叠区；**不按时间戳重排**、不让旧页盖新投影；
// - 缓存最多四页、每页最多 50 条、合计最多 4 MiB，超预算丢弃更旧页；
// - `boundedCodexHistoryWindow` 是 Task 7 的五页查找窗口：当前尾窗 + 最多四页更早缓存，
//   第六页及更早即使误入缓存也不参与。

function msg(id: string, over: Partial<ProjectedMessage> = {}): ProjectedMessage {
  return {
    id,
    role: "user",
    kind: "user_input",
    text: id,
    ts: null,
    turnId: null,
    artifacts: [],
    turnImageCount: 0,
    omittedArtifactCount: 0,
    ...over,
  };
}

/** 构造一页（默认 token 链 "tok"；beforeId/nextBefore 取页内最早一条） */
function page(ids: string[], over: Partial<CodexHistoryPageInput> = {}): CodexHistoryPage {
  const stored = normalizeCodexHistoryPage({
    anchorToken: "tok",
    beforeId: ids[0] ?? null,
    messages: ids.map((id) => msg(id)),
    nextBefore: ids[0] ?? null,
    nextToken: "tok-next",
    ...over,
  });
  if (stored === null) throw new Error(`测试夹具页超过 ${CODEX_HISTORY_PAGE_LIMIT} 条`);
  return stored;
}

function ids(messages: readonly ProjectedMessage[]): string[] {
  return messages.map((m) => m.id);
}

describe("codex-history：mergeCodexHistory（身份合并 / canonical 顺序 / 投影优先）", () => {
  it("merge_pages_oldest_first_then_tail：缓存页按游标链由旧到新，尾窗最新在最后", () => {
    // 缓存新→旧：pages[0] 紧邻尾窗（m5,m6），pages[1] 更早（m3,m4）
    const pages = [page(["m5", "m6"]), page(["m3", "m4"])];
    const merged = mergeCodexHistory([msg("m7")], pages);
    expect(ids(merged)).toEqual(["m3", "m4", "m5", "m6", "m7"]);
  });

  it("projection_wins_overlap_without_duplicate：重叠消息取当前投影版本，位置不变", () => {
    const pages = [page(["m2", "m3"])];
    const tail = [msg("m3", { text: "投影新文本", ts: 99 }), msg("m4")];
    const merged = mergeCodexHistory(tail, pages);
    expect(ids(merged)).toEqual(["m2", "m3", "m4"]);
    expect(merged).toHaveLength(3);
    expect(merged[1].text).toBe("投影新文本");
    expect(merged[1].ts).toBe(99);
  });

  it("canonical_order_not_timestamp：不按 ts 重排（旧页 ts 更大也保持 canonical 位置）", () => {
    // `normalizeCodexHistoryPage` 对非法页返回 null（Task 5：超 50 条即拒收）：
    // 测试侧必须先收窄再交给 `mergeCodexHistory`，否则在含 tests 的 strict tsconfig 下
    // 是真实的 TS2345（仓库 tsconfig 只含 src，门禁不会暴露该类错误）。
    const older = normalizeCodexHistoryPage({
      anchorToken: "tok",
      beforeId: "m1",
      messages: [msg("m1", { ts: 5000 })],
      nextBefore: "m1",
      nextToken: "tok-next",
    });
    if (older === null) throw new Error("测试夹具页不应被拒绝");
    const merged = mergeCodexHistory([msg("m2", { ts: 1 })], [older]);
    expect(ids(merged)).toEqual(["m1", "m2"]);
  });

  it("tail_roll_seam_no_gap_no_duplicate：尾窗前移与页在接缝重叠时无空洞、无重复", () => {
    // 页覆盖 m2,m3；尾窗移动到 m2,m3,m4（接缝重叠 m2,m3）
    const merged = mergeCodexHistory([msg("m2"), msg("m3"), msg("m4")], [page(["m2", "m3"])]);
    expect(ids(merged)).toEqual(["m2", "m3", "m4"]);
    expect(new Set(ids(merged)).size).toBe(merged.length);
  });

  it("duplicate_across_pages_newer_page_wins：跨页重复只在较新页保留一次", () => {
    // pages[0] 较新（紧邻尾窗），pages[1] 较旧且含重复
    const pages = [page(["m2", "m3"]), page(["m1", "m2"])];
    expect(ids(mergeCodexHistory([], pages))).toEqual(["m1", "m2", "m3"]);
  });

  it("stale_anchor_pages_ignored：代际 token 不符的旧页不参与合并（旧 token 不得到新状态）", () => {
    const pages = [page(["m1"], { anchorToken: "tok-old" })];
    expect(ids(mergeCodexHistory([msg("m2")], pages, "tok-new"))).toEqual(["m2"]);
    expect(ids(mergeCodexHistory([msg("m2")], pages, "tok-old"))).toEqual(["m1", "m2"]);
    // 空 token（旧载荷无 historyToken）不能匹配任何已缓存页
    expect(ids(mergeCodexHistory([msg("m2")], pages, ""))).toEqual(["m2"]);
  });

  it("empty_inputs：空页空尾窗 → 空数组", () => {
    expect(mergeCodexHistory([], [])).toEqual([]);
  });
});

describe("codex-history：缓存预算（四页 / 每页 50 条 / 4 MiB）", () => {
  it("page_over_limit_rejected_not_trimmed：单页超过 50 条是协议违约——拒绝整页，绝不裁剪", () => {
    const sixty = Array.from({ length: 60 }, (_, i) => msg(`m${i + 1}`));
    expect(CODEX_HISTORY_PAGE_LIMIT).toBe(50);
    // 裁剪只能保留一侧，与调用方手里的 nextBefore/nextToken（指向原页最早一条之前）
    // 不再一致，会在下一次翻页时制造永久空洞——所以整页拒绝（null），不进入缓存。
    expect(
      normalizeCodexHistoryPage({
        anchorToken: "tok",
        beforeId: "m60",
        messages: sixty,
        nextBefore: "m1",
        nextToken: "tok-next",
      })
    ).toBeNull();
    // 51 条同样拒绝（边界值 50 才是合法上限）
    expect(
      normalizeCodexHistoryPage({
        anchorToken: "tok",
        beforeId: "m51",
        messages: sixty.slice(0, 51),
        nextBefore: "m1",
        nextToken: "tok-next",
      })
    ).toBeNull();
  });

  it("valid_50_message_page_appends：恰好 50 条的合法页正常归一化并追加进缓存", () => {
    const fifty = Array.from({ length: 50 }, (_, i) => msg(`m${i + 1}`));
    const stored = normalizeCodexHistoryPage({
      anchorToken: "tok",
      beforeId: "m50",
      messages: fifty,
      nextBefore: "m1",
      nextToken: "tok-next",
    });
    expect(stored).not.toBeNull();
    expect(ids(stored!.messages)).toHaveLength(50);
    expect(ids(stored!.messages)[0]).toBe("m1");
    expect(ids(stored!.messages)[49]).toBe("m50");
    expect(stored!.bytes).toBe(codexHistoryPageBytes(stored!.messages));

    const appended = appendCodexHistoryPage([], stored!);
    expect(appended.dropped).toBe(false);
    expect(appended.pages).toHaveLength(1);
    expect(ids(appended.pages[0].messages)).toHaveLength(50);
  });

  it("cap_keeps_newest_four_pages：超过四页从最旧一侧丢弃", () => {
    const pages = ["p1", "p2", "p3", "p4", "p5", "p6"].map((id) => page([id]));
    const capped = capCodexHistoryPages(pages);
    expect(CODEX_HISTORY_MAX_PAGES).toBe(4);
    expect(capped).toHaveLength(4);
    expect(capped.map((p) => p.beforeId)).toEqual(["p1", "p2", "p3", "p4"]);
  });

  it("byte_budget_drops_oldest_pages：合计超 4 MiB 丢弃更旧页，不做无洞保留", () => {
    const big = (id: string) =>
      page([id], { messages: [msg(id, { text: "x".repeat(1_500_000) })] });
    let cache: CodexHistoryPage[] = [];
    for (const id of ["p1", "p2", "p3"]) {
      cache = appendCodexHistoryPage(cache, big(id)).pages;
    }
    expect(CODEX_HISTORY_MAX_BYTES).toBe(4 * 1024 * 1024);
    expect(cache.map((p) => p.beforeId)).toEqual(["p1", "p2"]);
    const total = cache.reduce((sum, p) => sum + p.bytes, 0);
    expect(total).toBeLessThanOrEqual(CODEX_HISTORY_MAX_BYTES);
  });

  it("append_reports_dropped_and_supports_front_insert：丢弃有信号；重新锚定的第一页插到最新端", () => {
    const first = page(["m5"], { beforeId: "m6" });
    const reanchored = page(["m5b"], { beforeId: "m6", nextToken: "tok-2" });
    const replaced = appendCodexHistoryPage([first], reanchored, { front: true });
    expect(replaced.dropped).toBe(false);
    expect(replaced.pages).toHaveLength(1);
    expect(ids(replaced.pages[0].messages)).toEqual(["m5b"]);
    expect(replaced.pages[0].nextToken).toBe("tok-2");

    const big = page(["huge"], { messages: [msg("huge", { text: "x".repeat(4 * 1024 * 1024) })] });
    const dropped = appendCodexHistoryPage([], big);
    expect(dropped.dropped).toBe(true);
    expect(dropped.pages).toHaveLength(0);
  });

  it("page_bytes_utf8：预算按 UTF-8 字节计（中文 3 字节/字符，不是 JS 字符数）", () => {
    const messages = [msg("m", { text: "你好" })];
    expect(codexHistoryPageBytes(messages)).toBeGreaterThan(JSON.stringify(messages).length);
  });
});

describe("codex-history：五页查找窗口（Task 7 消费）", () => {
  it("bounded_window_ignores_fifth_earlier_page：尾窗 + 最多四页更早，第六页不参与", () => {
    // pages 新→旧：p1 紧邻尾窗 … p5 是第六页（更早）
    // 游标链必须与真实契约一致：页的 beforeId = 紧邻它更新一条消息的 id。
    const pages = [
      page(["m5"], { beforeId: "m6", nextBefore: "m5" }),
      page(["m4"], { beforeId: "m5", nextBefore: "m4" }),
      page(["m3"], { beforeId: "m4", nextBefore: "m3" }),
      page(["m2"], { beforeId: "m3", nextBefore: "m2" }),
      page(["m1"], { beforeId: "m2", nextBefore: "m1" }),
    ];
    const window = boundedCodexHistoryWindow([msg("m6")], pages, "tok");
    expect(ids(window)).toEqual(["m2", "m3", "m4", "m5", "m6"]);
    expect(ids(window)).not.toContain("m1");
  });
});

describe("codex-history：R2 尾窗接缝（保留缓冲 / canonical 合并 / 缓冲上限）", () => {
  it("retain_messages_that_left_tail：滚出尾窗的消息按顺序进入缓冲；仍在窗口或已在缓冲的不重复", () => {
    // 尾窗 m1..m3 → m2..m4：只有 m1 滚出
    const first = retainCodexHistoryMessages(
      [msg("m1"), msg("m2"), msg("m3")],
      [msg("m2"), msg("m3"), msg("m4")],
      []
    );
    expect(ids(first)).toEqual(["m1"]);
    // 再前移一条：m2 也滚出，接在 m1 之后（旧→新）
    const second = retainCodexHistoryMessages(
      [msg("m2"), msg("m3"), msg("m4")],
      [msg("m3"), msg("m4"), msg("m5")],
      first
    );
    expect(ids(second)).toEqual(["m1", "m2"]);
    // 没有新增（普通尾部追加/同窗口重发）→ 原数组引用不变，调用方可跳过 setState
    expect(retainCodexHistoryMessages([msg("m3"), msg("m4")], [msg("m3"), msg("m4")], second)).toBe(
      second
    );
  });

  it("retain_deleted_message_not_resurrected：最新 Turn 内删除消息（不换 token）不得被缓冲复活", () => {
    // 复核 F3 直接复现：上一帧单 Turn [A,B]，新 canonical 投影只剩 [B]。
    // 这不是「A 滚出窗口」，而是 A 被删除——缓冲必须整体失效，显示以投影为准。
    const result = retainCodexHistoryMessages([msg("A"), msg("B")], [msg("B")], []);
    expect(ids(result)).toEqual([]);
    const withOld = retainCodexHistoryMessages([msg("A"), msg("B")], [msg("B")], [msg("Z")]);
    expect(ids(withOld)).toEqual([]);
  });

  it("retain_reorder_clears_buffer：同长度换序（[A,B] → [B,A]）不是前移，缓冲整体失效", () => {
    const result = retainCodexHistoryMessages(
      [msg("A"), msg("B")],
      [msg("B"), msg("A")],
      [msg("Z")]
    );
    expect(ids(result)).toEqual([]);
  });

  it("retain_window_shrink_clears_buffer：窗口收缩（字节预算裁边）保守清空，不猜是删除还是滚动", () => {
    const result = retainCodexHistoryMessages(
      [msg("A"), msg("B"), msg("C")],
      [msg("B")],
      [msg("Z")]
    );
    expect(ids(result)).toEqual([]);
  });

  it("retain_continuous_shrink_keeps_seam：合法字节预算收缩（truncated）保留接缝", () => {
    // 复核 G4：旧窗 m100—m599（500 条）→ 新窗 m400—m600（201 条）是「旧窗后缀 +
    // 新消息」的合法收缩（大消息进窗触发 1 MiB 预算），不是删除。
    const seq = (start: number, count: number) =>
      Array.from({ length: count }, (_, i) => msg(`m${start + i}`));
    const previous = seq(100, 500);
    const next = [...seq(400, 200), msg("m600")];
    const retained = seq(1, 99);

    const result = retainCodexHistoryMessages(previous, next, retained, { truncated: true });
    expect(ids(result)).toEqual([...ids(retained), ...ids(seq(100, 300))]);
    expect(result).toHaveLength(399);
  });

  it("retain_truncated_without_overlap_clears：合法裁剪但新旧窗完全无重叠 → 保守清空（已知退化）", () => {
    // 已知残余退化：服务端确实裁剪过，但新旧尾窗毫无重叠时无法证明连续性，只能丢
    // 掉缓冲——可见接缝在这一极端场景下消失。宁可少一条旧消息，也不复活删除内容。
    const result = retainCodexHistoryMessages(
      [msg("A"), msg("B"), msg("C")],
      [msg("X"), msg("Y"), msg("Z")],
      [msg("Z")],
      { truncated: true }
    );
    expect(ids(result)).toEqual([]);
  });

  it("retain_shrink_without_truncation_clears：同样形状但无裁剪证据 → 按删除处理", () => {
    const seq = (start: number, count: number) =>
      Array.from({ length: count }, (_, i) => msg(`m${start + i}`));
    const previous = seq(100, 500);
    const next = [...seq(400, 200), msg("m600")];
    const result = retainCodexHistoryMessages(previous, next, seq(1, 99));
    expect(ids(result)).toEqual([]);
  });

  it("retain_append_keeps_buffer：纯尾部追加不产生新的滚出消息，原引用不变", () => {
    const retained = [msg("m0")];
    expect(retainCodexHistoryMessages([msg("A")], [msg("A"), msg("B")], retained)).toBe(retained);
  });

  it("merge_retained_between_pages_and_tail：缓存页（旧→新）→ 保留缓冲 → 尾窗，接缝连续无重复", () => {
    // 页新→旧：m3,m4 紧邻尾窗，m1,m2 更早；保留 m5,m6；尾窗 m7
    const merged = mergeCodexHistoryWithRetained(
      [msg("m7")],
      [page(["m3", "m4"]), page(["m1", "m2"])],
      [msg("m5"), msg("m6")]
    );
    expect(ids(merged)).toEqual(["m1", "m2", "m3", "m4", "m5", "m6", "m7"]);
    expect(new Set(ids(merged)).size).toBe(merged.length);
  });

  it("bounded_window_includes_retained：保留缓冲并入五页查找窗口（复核 F2）", () => {
    const win = boundedCodexHistoryWindow([msg("m101")], [], "tok", [msg("m100")]);
    expect(ids(win)).toEqual(["m100", "m101"]);
  });

  it("bounded_window_stops_at_gap：retained 丢中段后，旧缓存页不得越过缺口（S1）", () => {
    const range = (start: number, count: number) =>
      Array.from({ length: count }, (_, i) => msg(`m${start + i}`));
    // 尾窗 m600—m603、retained m594—m599、缓存页 m050—m099（锚点 m100 ≠ 前沿 m594）
    const win = boundedCodexHistoryWindow(
      range(600, 4),
      [
        page(
          range(50, 50).map((m) => m.id),
          { beforeId: "m100", nextBefore: "m050" }
        ),
      ],
      "tok",
      range(594, 6)
    );
    expect(ids(win)).not.toContain("m70");
    expect(ids(win)).toContain("m594");
  });

  it("bounded_window_accepts_ordered_overlap：缓存页与前沿有序重叠时纳入旧侧前缀（T2）", () => {
    const range = (start: number, count: number) =>
      Array.from({ length: count }, (_, i) => msg(`m${start + i}`));
    // 尾窗 m594—m1093；缓存页 before=m600、内容 m550—m599，后六条与尾窗前六条同序重叠。
    // 这不是缺口：新的只有 m550—m593，且紧邻前沿 m594。
    const pageIds = [...range(550, 44), ...range(594, 6)].map((m) => m.id);
    const win = boundedCodexHistoryWindow(
      range(594, 500),
      [page(pageIds, { beforeId: "m600", nextBefore: "m550" })],
      "tok"
    );
    expect(ids(win)).toContain("m580");
    expect(ids(win)).not.toContain("m1094");
    expect(win).toHaveLength(544);
  });

  it("bounded_window_skips_duplicate_pages_like_the_locator：整页重复免费跳过，第三页仍参与（S2）", () => {
    const range = (start: number, count: number) =>
      Array.from({ length: count }, (_, i) => msg(`m${start + i}`));
    // 池侧与定位 hook 必须选中同一批页：前两页整页落在 retained 里 → 不占预算
    const pages = [
      page(
        range(150, 50).map((m) => m.id),
        { beforeId: "m200", nextBefore: "m150" }
      ),
      page(
        range(100, 50).map((m) => m.id),
        { beforeId: "m150", nextBefore: "m100" }
      ),
      page(
        range(50, 50).map((m) => m.id),
        { beforeId: "m100", nextBefore: "m050" }
      ),
    ];
    const win = boundedCodexHistoryWindow([msg("m200")], pages, "tok", range(100, 100));
    expect(ids(win)).toContain("m70");
  });

  it("bounded_window_caps_retained_at_four_batches：2000 条 retained 只并入最近 200 条", () => {
    const seq = (start: number, count: number) =>
      Array.from({ length: count }, (_, i) => msg(`m${start + i}`));
    const win = boundedCodexHistoryWindow([msg("m2001")], [], "tok", seq(1, 2000));
    expect(win).toHaveLength(201);
    expect(ids(win)[0]).toBe("m1801");
    expect(ids(win)).not.toContain("m1");
  });

  it("bounded_window_shares_budget_with_pages：retained 占满四批时缓存页不参与", () => {
    const seq = (start: number, count: number) =>
      Array.from({ length: count }, (_, i) => msg(`m${start + i}`));
    const pages = [page(["m1700", "m1701"])];
    const win = boundedCodexHistoryWindow([msg("m2001")], pages, "tok", seq(1, 2000));
    expect(ids(win)).not.toContain("m1700");
    expect(win).toHaveLength(201);
  });

  it("merge_retained_without_pages：没有缓存页时保留缓冲直接接在新尾窗之前", () => {
    const merged = mergeCodexHistoryWithRetained([msg("m101"), msg("m102")], [], [msg("m100")]);
    expect(ids(merged)).toEqual(["m100", "m101", "m102"]);
  });

  it("merge_page_overlap_with_retained_keeps_order：页与保留缓冲重叠时保持既有顺序，页内容优先", () => {
    // 保留缓冲 m5..m12；页 m8..m12（游标 m13，尾窗 m13）：重叠段不重排、不重复
    const merged = mergeCodexHistoryWithRetained(
      [msg("m13")],
      [
        page(["m8", "m9", "m10", "m11", "m12"], {
          beforeId: "m13",
          nextBefore: "m8",
          messages: ["m8", "m9", "m10", "m11", "m12"].map((id) => msg(id, { text: `页-${id}` })),
        }),
      ],
      ["m5", "m6", "m7", "m8", "m9", "m10", "m11", "m12"].map((id) => msg(id))
    );
    expect(ids(merged)).toEqual(["m5", "m6", "m7", "m8", "m9", "m10", "m11", "m12", "m13"]);
    expect(merged[3].text).toBe("页-m8"); // 页比保留缓冲新
  });

  it("merge_page_newer_than_retained_goes_before_tail：页比保留缓冲新时按游标锚点落在尾窗之前", () => {
    // 保留 m100..m149（已滚出），页 m150..m151 紧邻尾窗 m200，游标锚 m200
    const merged = mergeCodexHistoryWithRetained(
      [msg("m200")],
      [page(["m150", "m151"], { beforeId: "m200", nextBefore: "m150" })],
      ["m100", "m149"].map((id) => msg(id))
    );
    expect(ids(merged)).toEqual(["m100", "m149", "m150", "m151", "m200"]);
  });

  it("merge_retained_before_page_by_cursor：整页都比保留缓冲旧时，页落在缓冲之前", () => {
    // 页 m50..m99 的游标是 m100（此时 m100 已被保留缓冲接住）
    const merged = mergeCodexHistoryWithRetained(
      [msg("m101")],
      [page(["m50", "m51"], { beforeId: "m100", nextBefore: "m50" })],
      [msg("m100")]
    );
    expect(ids(merged)).toEqual(["m50", "m51", "m100", "m101"]);
  });

  it("merge_tail_wins_over_retained：尾窗同 id 原位覆盖保留缓冲版本，不新增不重排", () => {
    const merged = mergeCodexHistoryWithRetained(
      [msg("m2", { text: "投影新文本" }), msg("m3")],
      [],
      [msg("m1"), msg("m2", { text: "旧文本" })]
    );
    expect(ids(merged)).toEqual(["m1", "m2", "m3"]);
    expect(merged[1].text).toBe("投影新文本");
  });

  it("merge_page_wins_over_retained：同 id 时以服务端页内容为准（页比保留缓冲新）", () => {
    const merged = mergeCodexHistoryWithRetained(
      [],
      [page(["m1"], { messages: [msg("m1", { text: "服务端页文本" })] })],
      [msg("m1", { text: "缓冲旧文本" })]
    );
    expect(ids(merged)).toEqual(["m1"]);
    expect(merged[0].text).toBe("服务端页文本");
  });

  it("merge_stale_token_pages_ignored：代际不匹配的页不参与，保留缓冲不受 token 参数影响", () => {
    const pages = [page(["m1"], { anchorToken: "tok-old" })];
    expect(ids(mergeCodexHistoryWithRetained([msg("m2")], pages, [msg("m0")], "tok-new"))).toEqual([
      "m0",
      "m2",
    ]);
  });

  it("retained_cap_drops_oldest_first_count：超过 2000 条从最旧一侧丢弃，其余保持顺序（精确条数）", () => {
    const retained = Array.from({ length: 2100 }, (_, i) => msg(`m${i + 1}`));
    const capped = capCodexRetainedMessages(retained);
    expect(CODEX_HISTORY_MAX_RETAINED_MESSAGES).toBe(2000);
    expect(capped).toHaveLength(2000);
    expect(ids(capped)).toEqual(Array.from({ length: 2000 }, (_, i) => `m${i + 101}`));
    // 未超限 → 原数组引用不变
    const small = [msg("a"), msg("b")];
    expect(capCodexRetainedMessages(small)).toBe(small);
  });

  it("retained_byte_cap_drops_oldest：超过 1 MiB 从最旧一侧丢弃，总字节守住上限", () => {
    const big = (id: string) => msg(id, { text: "x".repeat(200_000) });
    const retained = ["a", "b", "c", "d", "e", "f"].map(big);
    const capped = capCodexRetainedMessages(retained);
    expect(CODEX_HISTORY_MAX_RETAINED_BYTES).toBe(1024 * 1024);
    // 每条约 200 KB：6 条超 1 MiB，丢最旧一条后剩 5 条
    expect(ids(capped)).toEqual(["b", "c", "d", "e", "f"]);
    expect(codexHistoryPageBytes(capped)).toBeLessThanOrEqual(CODEX_HISTORY_MAX_RETAINED_BYTES);
  });
});
