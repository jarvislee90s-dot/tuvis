// Task 13（计划②）· 浮窗迷你条**展示层** `UsageMiniBarView`（13 用例；第 11 条是 2026-10-06 补的
// 「空窗口不许印 0」，第 12/13 条是 2026-10-06 用户裁决补的「第③行口径标签」与「没量到 token 就不印 0」）。
// 判据来源：spec P2（5 行行序 / 唯一可点元素「详情 »」/ maxHeight 兜底 / 「本会话」= recentSession）、
// D12（唯一可点元素 + 钻取）、D13（滚轮不劫持）、D20（固定 5 行、第 3 行汇总 1 行折「等N」）、
// 计划 §3 第 5/8/33 条与 Task 13 步骤 3。
//
// 纪律：
//  * jsdom 里 `getBoundingClientRect()` **恒 0** ⇒ **禁止**「渲染高度 = Npx」这类断言；可断言的只有
//    行数、DOM 序、inline style 字符串（如 `maxHeight`）、文案、`pointer-events` 值与监听注册情况
//    （计划 §3 第 33 条）。高度容量归纯层 `usage-minibar-layout.test.ts` 与实机目验（U-5…U-10）。
//  * 语言固定 zh（同 `usage-page.test.tsx`：i18n detector 在 jsdom 下读 en-US ⇒ 默认英文）。
//  * 数据用**同源夹具** `mockUsageDashboard`（Task 1）：行①/②/③/④ 的期望值由夹具求和推出，
//    夹具改口径时本文件跟着改，不另存一份期望值。
//  * 行③的工具名一律 `usageAgentLabel`（夹具里 `label` = 采集源 id，不是展示名）；行④的标题 `null`
//    必须走 `usage.mini.untitled`——**绝不出现 `null` 字面量**（Task 13 步骤 3）。
import { readFileSync } from "node:fs";
import path from "node:path";
import type { ComponentProps } from "react";
import { fireEvent, render, screen } from "@testing-library/react";
import { beforeAll, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { UsageMiniBarView } from "@/components/pet/UsageMiniBar";
import { EM_DASH } from "@/lib/usage/format";
import { MINI_BAR_ROWS, MINI_BAR_TOOL_LIMIT, miniBarMaxHeight } from "@/lib/usage/miniBar";
import { mockUsageDashboard } from "@/lib/usage/mockFixtures";
import { isDashboardEmpty, tokenUnmeasured } from "@/lib/usage/range";
import type { UsageDashboard, UsageRangePreset } from "@/types/usage";

/** i18next 的 `t` 直接作 `TFn` 传入（与 `usage-worksummary.test.tsx` 同法） */
const tf = i18n.t.bind(i18n);

/** 日档判定（口径同后端 `RangeGranularity`：`last5h` / `today` 是小时档，其余三档是日档） */
const isDayTier = (preset: UsageRangePreset): boolean => preset !== "last5h" && preset !== "today";

/** 浮窗默认档 = 设置项 `miniBarRange` 默认值「当日」+ 按工具分组（第 3 行是分工具汇总） */
const DASH = mockUsageDashboard({ preset: "today" }, "tool");
/** 日档（近 7 天）：真机后端在这一档**恒**给 `recentSession: null`（日聚合行不带 `session_id`） */
const DASH_DAY = mockUsageDashboard({ preset: "last7d" }, "tool");

/**
 * 第 3 行的**口径标签**（i18n 键 `usage.mini.toolsBasis`）：本文件语言固定 zh（见文件头纪律），
 * 故这里逐字钉住 zh 值——标签是**用户裁决要求可见**的东西，不能因为改文案而悄悄消失。
 * 2026-10-20 Task 8：计量维度值统一，键值由「按工具 请求输入：」改为「按工具 新增输入：」。
 */
const TOOLS_BASIS = "按工具 新增输入：";

type ViewProps = ComponentProps<typeof UsageMiniBarView>;

function renderView(over: Partial<ViewProps> = {}) {
  return render(
    <UsageMiniBarView
      status="ready"
      dash={DASH}
      scale={1}
      mode="hover"
      onDetail={() => {}}
      t={tf}
      {...over}
    />
  );
}

/** 取某一行（行名见组件：request / cache / tools / session / detail） */
const rowOf = (name: string): HTMLElement => screen.getByTestId(`usage-mini-row-${name}`);

/** 五行行名，**DOM 序**即渲染序 */
const ROW_ORDER = [
  "usage-mini-row-request",
  "usage-mini-row-cache",
  "usage-mini-row-tools",
  "usage-mini-row-session",
  "usage-mini-row-detail",
];

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

describe("usage-minibar（计划② Task 13 步骤 3：浮窗展示层）", () => {
  it("1. 五行固定行序 + 每行 nowrap/ellipsis + 容器 maxHeight 与 overflowY 兜底", () => {
    renderView();
    expect(MINI_BAR_ROWS).toBe(5);
    const ids = Array.from(
      document.querySelectorAll<HTMLElement>('[data-testid^="usage-mini-row-"]')
    ).map((el) => el.dataset.testid);
    expect(ids).toEqual(ROW_ORDER); // 行数 5 + 行序固定（多一行/少一行/次序变了都红）
    for (const id of ROW_ORDER) {
      const el = screen.getByTestId(id);
      // 单行 nowrap + ellipsis：行高确定 ⇒ 窗口高度不抖（Task 13 关键坑第 2 条）
      expect(el.style.whiteSpace).toBe("nowrap");
      expect(el.style.overflow).toBe("hidden");
      expect(el.style.textOverflow).toBe("ellipsis");
    }
    const box = screen.getByTestId("usage-mini");
    expect(box.dataset.mode).toBe("hover");
    expect(box.style.maxHeight).toBe(`${miniBarMaxHeight(1)}px`); // "120px"
    expect(box.style.overflowY).toBe("auto"); // 放不下就截断/滚动，而不是把窗口撑爆
  });

  it("2. 行①新增输入 + 命中率：fmtTokens 全文口径、命中率是 0–1 分数（无二次换算）；已采集哨兵走 notCollected", () => {
    renderView();
    // 夹具四工具求和：ΣrequestTotal 1,954,268 → 195.43万；命中率 1,318,123 / 1,954,268 = 0.6744…
    expect(rowOf("request").textContent).toBe("新增输入 195.43万 · 命中率 67.4%");
    // collectedAt === 0 =「尚未采集」哨兵（§3 第 8 条）：显示 usage.notCollected，**不是** 0 也不是 —
    renderView({ dash: { ...DASH, collectedAt: 0 } });
    const notCollected = screen.getAllByTestId("usage-mini-row-request")[1];
    expect(notCollected.textContent).toBe(
      `新增输入 ${tf("usage.notCollected")} · 命中率 ${EM_DASH}`
    );
    expect(notCollected.textContent).not.toMatch(/\d/); // 一行数字都不许出现（更不许 0）
  });

  it("3. 行②缓存命中 + 输出：两个数都走 fmtTokens（缓存命中 131.81万 · 输出 8.27万）", () => {
    renderView();
    expect(rowOf("cache").textContent).toBe("缓存命中 131.81万 · 输出 8.27万");
  });

  it("4. 行③分工具**汇总 1 行**：行首口径标签 + 按新增输入降序取前 limit 条，超出折「等 N」（不做每工具一行）", () => {
    renderView({ toolLimit: MINI_BAR_TOOL_LIMIT });
    // 夹具 4 工具：claude 1,246,567 > codex 460,789 > zcode 234,567 > workbuddy 12,345
    // 行首标签是 2026-10-06 用户裁决新增的**口径说明**（本行显示的是「新增输入」，不是分布卡的
    // 「新增输入 + 输出」）——数值口径本身**没变**（标签文字 2026-10-20 统一为「新增输入」）。
    expect(rowOf("tools").textContent).toBe(
      `${TOOLS_BASIS}Claude 124.66万 · Codex 46.08万 · ZCode 23.46万 · 等 1`
    );
    // 条数由设置给（spec P7 的 miniBarToolRows）：放到 4 ⇒ 4 条全出、无「等 N」
    renderView({ toolLimit: 4 });
    expect(screen.getAllByTestId("usage-mini-row-tools")[1].textContent).toBe(
      `${TOOLS_BASIS}Claude 124.66万 · Codex 46.08万 · ZCode 23.46万 · WorkBuddy 1.23万`
    );
  });

  it("5. 行③空态：全零 / 空输入 → 空态占位（不出现 0，也不出现「等 0」，**更不得只留一个光秃秃的标签**）", () => {
    const allZero: UsageDashboard = {
      ...DASH,
      rows: DASH.rows.map((r) => ({ ...r, metrics: { ...r.metrics, requestTotal: 0 } })),
    };
    renderView({ dash: allZero });
    expect(rowOf("tools").textContent).toBe(EM_DASH);
    expect(rowOf("tools").textContent).not.toContain("0");
    // 空态**不得**退化成「按工具 新增输入：」——那既不是空态占位，也读不出「没有数据」（P8）
    expect(rowOf("tools").textContent).not.toContain(TOOLS_BASIS);
    renderView({ dash: { ...DASH, rows: [] } });
    const emptyTools = screen.getAllByTestId("usage-mini-row-tools")[1];
    expect(emptyTools.textContent).toBe(EM_DASH);
    expect(emptyTools.textContent).not.toContain(TOOLS_BASIS);
  });

  it("6. 行④本会话：数据源是契约 recentSession —— 工具名走 usageAgentLabel + 新增输入 + 标题", () => {
    renderView();
    // 夹具 recentSession：sourceId claude、requestTotal 57,000、title 演示会话
    expect(rowOf("session").textContent).toBe("本会话 Claude 5.70万 · 演示会话");
    // 「本会话」的语义提示（spec P2：最近有活动的会话，**不是**当前打开的那个）
    expect(rowOf("session").getAttribute("title")).toBe(tf("usage.mini.sessionHint"));
  });

  it("7. 行④标题为 null → usage.mini.untitled（绝不出现 null 字面量）", () => {
    renderView({ dash: { ...DASH, recentSession: { ...DASH.recentSession!, title: null } } });
    expect(rowOf("session").textContent).toBe("本会话 Claude 5.70万 · 无标题");
    expect(rowOf("session").textContent).not.toContain("null");
  });

  it("8. 行④ recentSession 为 null → 整行空态（暂无会话 + —，不显示 0）；title 覆盖**两种**空态原因", () => {
    // 真机口径：**日档后端恒给 `null`**（`query.rs::recent_session_with` 的 Day 分支——日聚合行不带
    // `session_id`，照旧聚合会得到一个「本会话 0」的假测量值）⇒ 该行必然空态，属**预期行为不是 bug**。
    // Task 15 已把这条档位分叉补进夹具（`mockUsageDashboard` 日档恒 `null`，原先登记的保真缺口已收口）
    // ⇒ 本用例直接用 `DASH_DAY` 断言，顺带把**夹具本身**守这条口径也钉住（不再手工置 null）。
    expect(isDayTier(DASH_DAY.range.preset)).toBe(true);
    expect(DASH_DAY.recentSession).toBeNull();
    renderView({ dash: DASH_DAY }); // 日档：真机后端恒给 null，属**预期行为不是 bug**
    expect(rowOf("session").textContent).toBe(`本会话 ${EM_DASH} · 暂无会话`);
    expect(rowOf("session").textContent).not.toContain("0");
    const hint = rowOf("session").getAttribute("title") ?? "";
    expect(hint).toBe(tf("usage.mini.sessionEmptyHint"));
    // 两种原因都要在文案里：① 范围内没有最近会话；② 日档聚合不带会话标识
    expect(hint).toContain("没有最近会话");
    expect(hint).toContain("日档");
  });

  it("9. 唯一可点元素是「详情 »」：容器 pointer-events:none、详情 auto 且可点；不注册 wheel、不 preventDefault", () => {
    const winAdd = vi.spyOn(window, "addEventListener");
    const docAdd = vi.spyOn(document, "addEventListener");
    const onDetail = vi.fn();
    renderView({ onDetail, mode: "manual" });
    const box = screen.getByTestId("usage-mini");
    // 容器 pointer-events:none 的作用**只是**不抢宠物窗口内部的指针事件，**不是**穿透到别的 OS 窗口
    // （宠物窗口整窗常驻交互，见 FoxbellPet 的「吃点击面积」说明）
    expect(box.style.pointerEvents).toBe("none");
    expect(box.dataset.mode).toBe("manual");
    // 其余三行不自行恢复可点；行④（本会话）与「详情 »」同法自开 auto——它没有 onClick，故
    // 「唯一**可点**元素 = 详情 »」这条结论不变，只是让它的 title 真的悬得到（验收 E-4）。
    for (const id of ROW_ORDER.slice(0, 3)) {
      expect(screen.getByTestId(id).style.pointerEvents).toBe("");
    }
    fireEvent.click(rowOf("detail"));
    expect(onDetail).toHaveBeenCalledTimes(1);
    expect(rowOf("detail").style.pointerEvents).toBe("auto");
    // E-4（Task 15 最终修复波）：容器是 `pointer-events:none` ⇒ 不参与命中测试 ⇒ 挂在**容器**上的
    // title 真机悬不到。会话行必须自己 auto，且它的 title 非空；jsdom 弹不出原生 tooltip，
    // 能锁的只有这两件（文案本身由用例 6/8 分别按有会话 / 空态逐字断言）。
    expect(rowOf("session").style.pointerEvents).toBe("auto");
    expect((rowOf("session").getAttribute("title") ?? "").trim().length).toBeGreaterThan(0);
    // 会话行 auto **不**新增可点元素：它没有 onClick，点它不该触发详情
    fireEvent.click(rowOf("session"));
    expect(onDetail).toHaveBeenCalledTimes(1);
    // 浮窗**唯一可点**元素是详情行；会话行只是额外可悬停（为它的 title，见上），不是可点元素。
    // 截断提示挂在详情行上（13 个 usage.mini.* 键都有真实消费点，不留死键）
    expect(rowOf("detail").getAttribute("title")).toBe(tf("usage.mini.truncated"));
    // 滚轮**不劫持**（D13）：本组件的保证就是「不注册 wheel 监听、不 preventDefault」
    const wheelCalls = [...winAdd.mock.calls, ...docAdd.mock.calls].filter(([k]) => k === "wheel");
    expect(wheelCalls).toHaveLength(0);
    winAdd.mockRestore();
    docAdd.mockRestore();
    // 源码锁补足「不 preventDefault」这条（行为上断言「没发生」需要一个不可能发生的事件；
    // 计划保证的正是「我们不注册」——扫描面 = 本组件源码，注释里也不许出现这两个词）
    const src = readFileSync(
      path.join(process.cwd(), "src/components/pet/UsageMiniBar.tsx"),
      "utf8"
    );
    expect(src.includes("wheel")).toBe(false);
    expect(src.includes("preventDefault")).toBe(false);
  });

  it("10. 加载态 / 错误态照常渲染 5 行与「详情 »」（错误文案指向详情 » 重试），占位不出现 0", () => {
    for (const status of ["loading", "error"] as const) {
      const { unmount } = renderView({ status });
      expect(screen.getAllByTestId(/^usage-mini-row-/)).toHaveLength(MINI_BAR_ROWS);
      expect(rowOf("detail").textContent).toBe("详情 »"); // 「详情 »」在三态分支之外，错误态因此有恢复入口
      expect(rowOf("request").textContent).toBe(tf(`usage.mini.${status}`));
      // 其余三行保留行名与空态占位（行数恒 5 ⇒ 窗口高度不随三态抖动），**一个数字都不出现**
      expect(rowOf("cache").textContent).toBe(`缓存命中 ${EM_DASH} · 输出 ${EM_DASH}`);
      expect(rowOf("tools").textContent).toBe(EM_DASH);
      expect(rowOf("session").textContent).toBe(`本会话 ${EM_DASH} · 暂无会话`);
      for (const id of ["request", "cache", "tools", "session"]) {
        expect(screen.getByTestId(`usage-mini-row-${id}`).textContent).not.toMatch(/\d/);
      }
      unmount();
    }
    // 错误态要给出恢复入口（唯一可点元素就是它）：文案里必须点到「详情 »」这个名字
    renderView({ status: "error" });
    expect(rowOf("request").textContent).toContain("详情 »");
  });

  it("11. **空窗口**（采集过、但本期一行都没有）→ 空态，绝不渲染成 0 / 0.0%（spec P8 空态条；2026-10-06 评审 I-2）", () => {
    // 真机高频可达：`miniBarRange` 默认 `today`，今天还没用过任何工具时窗口就是空的。
    // 旧实现只排除 `collectedAt === 0` ⇒ 这里会渲染 `请求输入 0 · 命中率 0.0%` / `缓存命中 0 · 产出 0`
    // —— 一个**看起来像真实测量值的 0**，而看板同一个窗口显示的是「暂无数据」。
    const emptyWindow: UsageDashboard = {
      ...DASH,
      rows: [],
      hero: 0,
      totals: { ...DASH.totals, requestTotal: 0, requests: 0, cacheHitRate: 0, userEst: null },
      totalsBuckets: { inputFresh: 0, cacheRead: 0, cacheWrite: 0, output: 0 },
      // 保真（2026-10-06 第二轮）：四桶全零 ⇒ 每行 `request_total` 也是 0 ⇒ 后端候选集为空 ⇒
      // 真机**必给 `null`**（恒等式见 `mockFixtures.ts::zeroTokensOf` 的注释）。此前"刻意保留一个
      // 非 null 会话"是在造后端不可能产生的形状，已改。`live` 那条门控仍由用例 10（三态）覆盖。
      recentSession: null,
    };
    renderView({ dash: emptyWindow });
    // **前提锚**（2026-10-06 第三轮补）：这一档走的是**空窗口**那条判据 `isDashboardEmpty`
    // （零行），**不是**「没量到 token」那条 `tokenUnmeasured`。两条判据在这一档**同时为真**
    // （零行 ⇒ 四桶也全零），真正把两者分开的是**渲染出来的文案**：空窗口出 `usage.empty`、
    // 没量到 token 出 `—`（下一行断言）。这里把两条判据与「不是尚未采集哨兵」都显式钉住，
    // 免得后来者以为这一档考的是 `tokenUnmeasured`。
    expect(emptyWindow.collectedAt).not.toBe(0); // 不是 notCollected 哨兵
    expect(emptyWindow.rows).toHaveLength(0);
    expect(isDashboardEmpty(emptyWindow)).toBe(true);
    expect(tokenUnmeasured(emptyWindow)).toBe(true); // 同时为真 ⇒ 判别力只能来自文案
    // 判据**复用看板的** `isDashboardEmpty`（不另立第二套）：采集过 ⇒ 走 `usage.empty`，不是 notCollected
    expect(rowOf("request").textContent).toBe(`新增输入 ${tf("usage.empty")} · 命中率 ${EM_DASH}`);
    // 反向锚：这一档**不是**「没量到 token」的形态（那一档第①行的新增输入位是 `—`，不是 `usage.empty`）
    expect(rowOf("request").textContent).not.toBe(`新增输入 ${EM_DASH} · 命中率 ${EM_DASH}`);
    expect(rowOf("cache").textContent).toBe(`缓存命中 ${EM_DASH} · 输出 ${EM_DASH}`);
    expect(rowOf("tools").textContent).toBe(EM_DASH);
    expect(rowOf("session").textContent).toBe(`本会话 ${EM_DASH} · 暂无会话`);
    // 行数恒 5（D20：窗口高度由公式算 ⇒ 不随三态抖动）
    expect(screen.getAllByTestId(/^usage-mini-row-/)).toHaveLength(MINI_BAR_ROWS);
    // 四条数据行**一个数字都不许出现**（0 / 0.0% / 57,000 都不行）
    for (const id of ["request", "cache", "tools", "session"]) {
      expect(screen.getByTestId(`usage-mini-row-${id}`).textContent).not.toMatch(/\d/);
    }
  });

  it("12. 行③**行首口径标签**：zh/en 同父同键、标签自带分隔符且不得只身出现（2026-10-06 用户裁决）", () => {
    // 用户裁决：第③行的数值口径**不变**（仍是「新增输入」，第①行的按工具拆解），
    // 但**必须让用户看得出这一行的口径**——它旁边就是分布卡/文本摘要/分享图的「新增输入 + 输出」，
    // 同一个工具在两个面上是两个数（这不是 bug，是两条被分别钉死的口径）。
    expect(tf("usage.mini.toolsBasis")).toBe(TOOLS_BASIS);
    // en 侧必须是**自己的**分隔符（半角冒号 + 空格），不能借 zh 的全角冒号 ⇒ 两语各读各的
    const tEn = i18n.getFixedT("en");
    expect(`${tEn("usage.mini.toolsBasis")}Claude`).toMatch(/[:：] ?Claude$/);
    expect(tEn("usage.mini.toolsBasis")).not.toBe(TOOLS_BASIS);
    // 「标签 · 口径」的间隔点不得再丢（2026-10-20 复审 Nit：值统一时 en 曾丢成 "By tool New input: "）
    expect(tEn("usage.mini.toolsBasis")).toContain("·");

    renderView();
    const tools = rowOf("tools");
    // 标签在**行首**（`textContent` 以它开头），整行仍是**单行**（nowrap + ellipsis 由用例 1 锁）
    expect(tools.textContent?.startsWith(TOOLS_BASIS)).toBe(true);
    expect(tools.textContent).toBe(
      `${TOOLS_BASIS}Claude 124.66万 · Codex 46.08万 · ZCode 23.46万 · 等 1`
    );
    // 标签只此一处；行数恒 5（加标签**不得**变成第 6 行，也不得让第③行占两行）
    expect(screen.getAllByTestId(/^usage-mini-row-/)).toHaveLength(MINI_BAR_ROWS);
    expect(screen.getAllByTestId("usage-mini-row-tools")).toHaveLength(1);
    // 行③的文本里只有**一个**分隔符「：」——即标签自带的那一个（列表内部用的是 ` · `）
    expect(tools.textContent?.split("：")).toHaveLength(2);
  });

  it("13. **没量到 token 的窗口**（四桶全零、但 requests ≥ 1）→ token 位一律 `—`，计数类真值照常（P8）", () => {
    // 真机形状（用户 2026-10-06 给出的账本实测）：**真空回合**行——四桶全零、requests ≥ 1
    // （15 行 / 50 requests，分布在 claude / opencode / zcode）。这种窗口 `isDashboardEmpty`
    // 为**假**（有行）⇒ 旧实现渲染 `请求输入 0 · 命中率 0.0%` / `缓存命中 0 · 产出 0`，
    // 一个**看起来像真实测量值的 0**。用户裁决：没产生数值就不显示数值 ⇒ 一律 `EM_DASH`。
    const noTokens: UsageDashboard = {
      ...DASH,
      rows: DASH.rows.map((r) => ({
        ...r,
        buckets: { inputFresh: 0, cacheRead: 0, cacheWrite: 0, output: 0 },
        metrics: { ...r.metrics, requestTotal: 0, cacheHitRate: 0 }, // requests 是真值，保留
      })),
      totalsBuckets: { inputFresh: 0, cacheRead: 0, cacheWrite: 0, output: 0 },
      totals: { ...DASH.totals, requestTotal: 0, cacheHitRate: 0, requests: 50 },
      hero: 0,
      // **保真锁**（2026-10-06 第二轮）：这种窗口下真机**必给 `null`**——候选会话要求
      // `request_total > 0`，而窗口四桶之和 ≥ 任何行的 `request_total`（`semantics.rs` 两套语义都成立）
      // ⇒ 四桶全零时候选集为空。**不许**再摆一个非 null 的会话来「锁行④的门控」：那个形状后端产生不出来，
      // 那条门控在真机不可达（= 空锁），本轮已按保真把行④的第二道门控删掉。
      recentSession: null,
    };
    // 前提：不是空窗口（有行、有请求次数）⇒ 「没量到 token」是独立于 `isDashboardEmpty` 的一条判据
    expect(noTokens.rows.length).toBeGreaterThan(0);
    expect(noTokens.totals.requests).toBe(50);
    expect(noTokens.recentSession).toBeNull(); // 保真锁：不许用「非 null 会话 + 全零四桶」造空锁
    renderView({ dash: noTokens });
    expect(rowOf("request").textContent).toBe(`新增输入 ${EM_DASH} · 命中率 ${EM_DASH}`);
    expect(rowOf("cache").textContent).toBe(`缓存命中 ${EM_DASH} · 输出 ${EM_DASH}`);
    expect(rowOf("tools").textContent).toBe(EM_DASH);
    // 行④走**空态**分支（真机形态）：这一档 `recentSession` 必为 null ⇒ 既没有会话名、也没有数字
    expect(rowOf("session").textContent).toBe(`本会话 ${EM_DASH} · 暂无会话`);
    // 行数恒 5；四条数据行**一个数字都不许出现**
    expect(screen.getAllByTestId(/^usage-mini-row-/)).toHaveLength(MINI_BAR_ROWS);
    for (const id of ["request", "cache", "tools", "session"]) {
      expect(screen.getByTestId(`usage-mini-row-${id}`).textContent).not.toMatch(/\d/);
    }
  });
});
