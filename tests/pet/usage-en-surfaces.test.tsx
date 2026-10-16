// issue #160 · en locale 用量面断言：英文界面零中文单位。
// 纪律（同 usage-minibar.test.tsx 文件头）：jsdom 下 i18n detector 默认读 en-US ⇒ en 是默认前置；
// 仍显式 changeLanguage("en") 钉死，防执行环境差异（Nit③）。
// 夹具 = mockUsageDashboard（同源纪律）：四桶汇总 82.71K–1.95M，全 ≥1K（P2-4）。
import { cleanup, render } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it } from "vitest";
import i18n from "@/i18n";
import { UsageMiniBarView } from "@/components/pet/UsageMiniBar";
import { UsageSummary } from "@/components/usage/UsageSummary";
import { UsageWorkSummary } from "@/components/usage/UsageWorkSummary";
import { mockUsageDashboard } from "@/lib/usage/mockFixtures";

const tf = i18n.t.bind(i18n);
const DASH = mockUsageDashboard({ preset: "today" }, "tool");

beforeAll(async () => {
  await i18n.changeLanguage("en");
});
afterEach(cleanup);

describe("en locale 用量面（issue #160 目标 4）", () => {
  it("浮窗迷你条：K/M/B 单位，零中文单位", () => {
    const { container } = render(
      <UsageMiniBarView
        status="ready"
        dash={DASH}
        scale={1}
        mode="hover"
        onDetail={() => {}}
        t={tf}
      />
    );
    const text = container.textContent ?? "";
    expect(text).toContain("1.95M"); // 行① 请求输入（requestTotal 1,954,268）
    expect(text).toContain("1.32M"); // 行② 缓存命中（cacheRead 1,318,123）
    expect(text).toContain("82.71K"); // 行② 产出（output 82,713）
    expect(text).toContain("1.25M"); // 行③ 分工具首行（claude 1,246,567）
    expect(text).not.toMatch(/万|亿|分钟|秒|毫秒/);
  });

  it("首屏汇总：四桶 K/M 缩写，零中文单位", () => {
    const { container } = render(<UsageSummary dash={DASH} retentionDays={30} t={tf} />);
    const text = container.textContent ?? "";
    expect(text).toContain("1.95M"); // 请求输入
    expect(text).toContain("1.32M"); // 缓存命中
    expect(text).toContain("82.71K"); // 产出
    expect(text).toContain("~18.02K"); // 用户输入(估)（userEst 18,023；用户裁决：该行渲染 userEst，
    // inputFresh 不在本组件渲染面——原拟 620.14K 已按现场裁决改为夹具实算值）
    expect(text).not.toMatch(/万|亿|分钟|秒|毫秒/);
  });

  it("工作小结：min/s/ms，零中文时长单位", () => {
    const { container } = render(
      <UsageWorkSummary work={DASH.workSummary} availability={DASH.availability} t={tf} />
    );
    const text = container.textContent ?? "";
    expect(text).toContain("31.1 min"); // 耗时第一（topToolMs 1,864,000）
    expect(text).toContain("79 ms"); // 平均耗时（toolAvgMs 79）
    expect(text).toContain("838.5 min"); // zcode 最长单 turn（max 50,311,958）
    expect(text).not.toMatch(/万|亿|分钟|秒|毫秒/);
  });
});
