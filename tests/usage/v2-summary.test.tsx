// Task 12 · A Token 总览：数值派生 + 行顺序 + cacheMiss 上期公式 + 未测量空态。
// 2026-10-20 Task 8：「缓存写入（解释）」整块删除（usage-cache-write 不再存在）；
// 计量维度文案统一（新增输入 / 输出）由 i18n 值生效，键名与行序不变。
import { render, screen } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import i18n from "@/i18n";
import { UsageSummary } from "@/components/usage/UsageSummary";
import { EM_DASH, fmtTokens } from "@/lib/usage/format";
import { getLang } from "@/lib/usage/lang";
import { mockUsageDashboard } from "@/lib/usage/mockFixtures";
import { summaryNumbers } from "@/lib/usage/summaryV2";
import { tokenUnmeasured } from "@/lib/usage/range";

const tf = i18n.t.bind(i18n);

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

beforeEach(() => {
  localStorage.clear();
});

describe("summaryV2（纯函数）", () => {
  it("1. 逐字锁住 Plan 用例的 summaryNumbers 派生", () => {
    expect(
      summaryNumbers(
        { requestTotal: 100, cacheHitRate: 0.3, userEst: 9, requests: 2 },
        { inputFresh: 60, cacheRead: 30, cacheWrite: 10, output: 20 }
      )
    ).toEqual({
      total: 120,
      requestInput: 100,
      cacheRead: 30,
      cacheMiss: 70,
      output: 20,
      hitRate: 0.3,
      requests: 2,
    });
  });

  it("2. cacheMiss 上期值用同一公式（requestTotal - cacheRead），不是四桶相加", () => {
    const prev = summaryNumbers(
      { requestTotal: 80, cacheHitRate: 0.1, userEst: null, requests: 1 },
      { inputFresh: 60, cacheRead: 10, cacheWrite: 30, output: 20 }
    );
    expect(prev.cacheMiss).toBe(70);
    expect(prev.cacheMiss).not.toBe(60 + 30 + 20);
  });
});

describe("UsageSummary（A 固定展开卡）", () => {
  it("3. 七行固定顺序、删除 userEst、cacheMiss 行、缓存写入解释块不再存在", () => {
    const dash = mockUsageDashboard({ preset: "last7d" }, "tool");
    render(<UsageSummary dash={dash} t={tf} />);

    // hero 与新显示口径一致：万/亿缩写，不再用完整千分位
    expect(screen.getByTestId("usage-hero")).toHaveTextContent(fmtTokens(dash.hero, getLang()));

    expect(screen.getAllByTestId(/^usage-grid-/).map((el) => el.dataset.testid)).toEqual([
      "usage-grid-requestInput",
      "usage-grid-cacheRead",
      "usage-grid-cacheMiss",
      "usage-grid-output",
      "usage-grid-hitRate",
      "usage-grid-requests",
      "usage-grid-asOf",
    ]);
    expect(screen.queryByTestId("usage-grid-userEst")).toBeNull();

    const lang = getLang();
    const cacheMiss = dash.totals.requestTotal - dash.totalsBuckets.cacheRead;
    const missRow = screen.getByTestId("usage-grid-cacheMiss");
    expect(missRow).toHaveTextContent(fmtTokens(cacheMiss, lang));
    // Token 数值不再挂完整千分位原始值 tooltip（环比格仍保留上一周期提示）
    expect(missRow.querySelector(".font-medium")).not.toHaveAttribute("title");

    // 2026-10-20 Task 8：「缓存写入（解释）」整块删除——details 与网格行都不存在
    expect(screen.queryByTestId("usage-cache-write")).toBeNull();
    expect(document.querySelector("details")).toBeNull();
    expect(screen.queryByTestId("usage-grid-cacheWrite")).toBeNull();
    // 首行标签跟随 i18n 值统一（新增输入），键名与行序不变
    expect(
      screen.getByTestId("usage-grid-requestInput").querySelector(".text-muted-foreground")
        ?.textContent
    ).toBe(tf("usage.grid.requestInput"));
    expect(tf("usage.grid.requestInput")).toBe("新增输入");
  });

  it("4. 未测量 token：四项 token 派生行 + hitRate 出 —，计数/asOf 真值保留", () => {
    localStorage.setItem("mam-mock-usage", "unmeasured");
    const dash = mockUsageDashboard({ preset: "last7d" }, "tool");
    expect(tokenUnmeasured(dash)).toBe(true);

    render(<UsageSummary dash={dash} t={tf} />);

    expect(screen.getByTestId("usage-hero")).toHaveTextContent(EM_DASH);
    for (const key of ["requestInput", "cacheRead", "cacheMiss", "output", "hitRate"]) {
      const row = screen.getByTestId(`usage-grid-${key}`);
      expect(row).toHaveTextContent(EM_DASH);
      expect(row.textContent).not.toMatch(/\d/);
      expect(row.textContent).not.toContain("上一周期");
    }

    const requests = screen.getByTestId("usage-grid-requests");
    expect(requests).not.toHaveTextContent(EM_DASH);
    expect(requests.textContent).toMatch(/\d/);
    expect(screen.getByTestId("usage-grid-asOf")).toHaveTextContent(/\d{2}:\d{2}:\d{2}/);
  });
});
