// Task 9（2026-10-26 计划）· 前端双指标查询与显示状态契约（vitest + jsdom）。
// 判据来源：task-9-brief Step1（metric_fallback_is_not_a_preference_change）+ Spec §5.3
// （口径切换：缺能力不伪装数值、不破坏用户选择）+ 公共契约（DTO camelCase 逐字镜像）。
//
// 覆盖：
//  * effectiveMetric 只读能力位：Codex（stream=false）⇒ 临时 fallback 到 response，
//    但用户的偏好本身不动；切回六源 ⇒ 回 stream；区间 n=0 不影响模式（mode 与样本无关）；
//  * rateForMode 返回模式自己的 rate（含 null）；两模式 n 独立；isSmallSample 边界
//    （n=0 false；output 49 true；50 false——沿既有 TPS 小输出阈值 50）；
//  * sortSeries：rate 排序用当前模式的值且 null 排末；tool/model 走 tool→model→channel
//    稳定序（与后端 fallback_cmp 三元组一致，method 不在比较器里）；输入数组不被原地改动；
//  * seriesKeyId：channel null / 缺失 / 有值互不碰撞，method 参与区分，序列化稳定；
//  * translateSpeedReason：reason/streamReason 的 i18n 唯一出口——六个已知 token 命中
//    译文（含冒号的 Codex token 不被 nsSeparator 截断），未知 token 原样降级；
//  * useSpeedDashboardQuery（假时钟，opts 必填）：active:true ⇒ 首查 + 5 秒轮询；
//    active:false ⇒ 60 秒零请求、focus（visibilitychange）也零请求；翻回 active 恢复；
//    SpeedDashboardQuery 六个字段任一变化都换 queryKey（缓存隔离）；
//  * 切用量区间（既有 usage 查询键）不产生任何新 tps_* 请求（invoke 层 spy——两层解耦）；
//  * mock 夹具对 TS 类型做运行时结构断言（字段全集 + 可空位）；mock 的排序/搜索/选择
//    回退与后端 v2/query.rs 同口径（防下游从 mock 学到错误预期）。
//
// 纪律：真实 hook 路径——invoke 走 tests/setup.ts 接线的 tauriInvokeMock；不 mock react-query。
import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { tauriInvokeMock, setMockSpeedDashboard } from "../msw/tauriMocks";
import {
  defaultDirectionFor,
  effectiveMetric,
  isSmallSample,
  rateForMode,
  seriesKeyId,
  sortSeries,
  translateSpeedReason,
} from "@/lib/speed/view";
import { SPEED_KEY, speedDashboardKey, useSpeedDashboardQuery } from "@/lib/query/queries/speed";
import { TPS_KEY } from "@/lib/query/queries/tps";
import { useUsageDashboardQuery } from "@/lib/query/queries/usage";
import { mockSpeedDashboard } from "@/lib/usage/mockFixtures";
import i18n from "@/i18n";
import type {
  SpeedCapabilities,
  SpeedDashboardQuery,
  SpeedDashboardSnapshot,
  SpeedMetrics,
  SpeedRate,
  SpeedReasonToken,
  SpeedSeriesSummary,
  SeriesKey,
} from "@/types/speed";

// ==== 测试夹具 ====

/** 双能力来源（六个原生源）。 */
const bothCapabilities: SpeedCapabilities = {
  stream: true,
  response: true,
  streamReason: null,
  responseReason: null,
};
/** 仅响应来源（Codex：stream=false + 稳定缺口 token）。 */
const responseOnly: SpeedCapabilities = {
  stream: false,
  response: true,
  streamReason: "stream-gap:insufficient-generation-timing",
  responseReason: null,
};

const zcode: SeriesKey = {
  tool: "zcode",
  model: "glm-5.3-flash",
  channel: null,
  method: "stream-observed",
};
const codex: SeriesKey = {
  tool: "codex",
  model: "gpt-5-codex",
  channel: null,
  method: "log-boundary-approx",
};

function rate(
  value: number | null,
  outputTokens: number,
  durationMs: number,
  sampleCount: number
): SpeedRate {
  return { value, outputTokens, durationMs, sampleCount };
}

function metrics(overrides: Partial<SpeedMetrics> = {}): SpeedMetrics {
  return {
    stream: rate(86.4, 3_888, 45_000, 12),
    response: rate(62.1, 3_888, 62_600, 10),
    avgFirstTokenMs: 1_230,
    ttftSampleCount: 9,
    modelMs: 45_200,
    toolMs: 8_100,
    candidateCount: 15,
    failedCount: 2,
    cancelledCount: 1,
    missingTimingCount: 0,
    sessionCount: 4,
    latestStreamEndedAt: 1_000,
    latestResponseEndedAt: 1_000,
    outputScope: "providerReported",
    scopeMixed: false,
    ...overrides,
  };
}

function summary(key: SeriesKey, m: Partial<SpeedMetrics> = {}): SpeedSeriesSummary {
  return { seriesKey: key, metrics: metrics(m) };
}

const baseQuery: SpeedDashboardQuery = {
  range: "1h",
  selection: null,
  metricMode: "stream",
  toolId: null,
  search: "",
  sort: "tool",
};

function createWrapper(
  client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
) {
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  return { client, wrapper };
}

function callsOf(cmd: string): number {
  return tauriInvokeMock.mock.calls.filter(([c]) => c === cmd).length;
}

function tpsIpcCalls(): number {
  return tauriInvokeMock.mock.calls.filter(([c]) => c.startsWith("tps_")).length;
}

beforeEach(() => {
  tauriInvokeMock.mockClear();
  setMockSpeedDashboard(null);
});

afterEach(() => {
  vi.useRealTimers();
});

// ==== 1. 口径 fallback 不是偏好变化（brief Step1 逐字判据）====

describe("effectiveMetric（Spec §5.3 口径切换）", () => {
  it("metric_fallback_is_not_a_preference_change", () => {
    // stream 偏好遇到仅响应的 Codex ⇒ 实际视图临时显示 response
    expect(effectiveMetric("stream", responseOnly)).toBe("response");
    // 切回支持流式的来源 ⇒ 回 stream（fallback 只作用于本次计算，不改偏好）
    expect(effectiveMetric("stream", bothCapabilities)).toBe("stream");
    // 显式选 response 就用 response（七来源统一观察）
    expect(effectiveMetric("response", bothCapabilities)).toBe("response");
    expect(effectiveMetric("response", responseOnly)).toBe("response");

    // 小状态机：选 Codex 显示 response，但偏好保持 stream；回到六源即恢复 stream
    const preferred: "stream" | "response" = "stream";
    const shownOnCodex = effectiveMetric(preferred, responseOnly);
    expect(shownOnCodex).toBe("response");
    // fallback 是纯函数：它不写偏好（模拟组件状态保持不动）
    const preferredModeAfterCodexFallback = preferred;
    const shownAfterBackToSixSources = effectiveMetric(
      preferredModeAfterCodexFallback,
      bothCapabilities
    );
    expect(preferredModeAfterCodexFallback).toBe("stream");
    expect(shownAfterBackToSixSources).toBe("stream");

    // 支持流式但区间 n=0 的来源**不**因此切换模式：effectiveMetric 只读能力位
    const streamCapableZeroSamples: SpeedCapabilities = {
      stream: true,
      response: true,
      streamReason: null,
      responseReason: null,
    };
    expect(effectiveMetric("stream", streamCapableZeroSamples)).toBe("stream");
    // 缺口原因存在与否不参与判断（能力位是唯一真值源）
    expect(effectiveMetric("stream", { ...bothCapabilities, streamReason: "read-failed" })).toBe(
      "stream"
    );
  });
});

// ==== 2. rateForMode / isSmallSample ====

describe("rateForMode 与 isSmallSample", () => {
  it("rateForMode 返回模式自己的 rate（含 null），两模式 n 独立", () => {
    const m = metrics({
      stream: rate(null, 0, 0, 0),
      response: rate(41.3, 1_200, 29_000, 5),
    });
    // stream 无样本 ⇒ null 原样透出（不伪装 0 速度）
    expect(rateForMode(m, "stream")).toEqual({
      value: null,
      outputTokens: 0,
      durationMs: 0,
      sampleCount: 0,
    });
    // response 是另一套独立统计
    expect(rateForMode(m, "response").value).toBe(41.3);
    expect(rateForMode(m, "response").sampleCount).toBe(5);
    // 两模式 n 互不相同且互不影响
    expect(rateForMode(m, "stream").sampleCount).not.toBe(rateForMode(m, "response").sampleCount);
  });

  it("isSmallSample 边界：n=0 false；输出 49 true；50 false（沿既有 TPS 小输出阈值）", () => {
    expect(isSmallSample(rate(1.0, 0, 100, 0))).toBe(false); // n=0
    expect(isSmallSample(rate(1.0, 49, 100, 1))).toBe(true); // < 50
    expect(isSmallSample(rate(1.0, 50, 100, 1))).toBe(false); // 阈值内不标小样本
    expect(isSmallSample(rate(null, 30, 0, 2))).toBe(true); // value=null 仍按输出合计判
  });
});

// ==== 3. sortSeries ====

describe("sortSeries", () => {
  // 稳定序的字典序素材：claude<codex<zcode；sonnet<opus（同 tool 下）；null 渠道 < 有值渠道
  const a: SeriesKey = { tool: "claude", model: "sonnet", channel: null, method: "client-native" };
  const b: SeriesKey = {
    tool: "claude",
    model: "opus",
    channel: "proxy",
    method: "stream-observed",
  };
  const c: SeriesKey = { tool: "claude", model: "opus", channel: null, method: "client-native" };
  const d: SeriesKey = { tool: "zcode", model: "glm", channel: null, method: "stream-observed" };
  // Codex：stream 无值（null），response 有值 —— null 是否排末随模式而定
  const codexSummary = summary(codex, {
    stream: rate(null, 0, 0, 0),
    response: rate(41.3, 1_200, 29_000, 5),
  });

  it("rate 排序用当前模式的值、null 排末；换模式即换序；不原地改输入", () => {
    const list = [
      summary(d, { stream: rate(10, 500, 50_000, 1), response: rate(5, 500, 100_000, 1) }),
      codexSummary,
      summary(a, { stream: rate(86.4, 3_888, 45_000, 12), response: rate(20, 500, 25_000, 2) }),
    ];
    const input = [...list];

    const byStream = sortSeries(list, "rate", "stream");
    expect(byStream.map((s) => s.seriesKey)).toEqual([a, d, codex].map((k) => k)); // 86.4 > 10 > null(末位)

    const byResponse = sortSeries(list, "rate", "response");
    expect(byResponse.map((s) => s.seriesKey)).toEqual([codex, a, d].map((k) => k)); // 41.3 > 20 > 5

    expect(list).toEqual(input); // 输入数组不被原地改动
    expect(sortSeries(list, "rate", "stream")).not.toBe(list); // 返回新数组
  });

  it("rate 同值并列时落回 tool→model→channel 稳定序", () => {
    const list = [
      summary(d, { stream: rate(50, 500, 10_000, 1) }),
      summary(a, { stream: rate(50, 500, 10_000, 1) }),
    ];
    expect(sortSeries(list, "rate", "stream").map((s) => s.seriesKey.tool)).toEqual([
      "claude",
      "zcode",
    ]);
  });

  it("tool 排序走 tool→model→channel 稳定序；model 排序把 model 提为主键；均不原地改", () => {
    const list = [summary(d), summary(c), summary(b), summary(a)];
    const input = [...list];

    // tool 主键：claude(opus,null) → claude(opus,proxy) → claude(sonnet) → zcode
    //（同 tool 下 model 字典序 opus < sonnet；同 model 下 null 渠道在前）
    const byTool = sortSeries(list, "tool", "stream");
    expect(byTool.map((s) => s.seriesKey)).toEqual([c, b, a, d]);

    // model 主键：glm(zcode) → opus(claude,null) → opus(claude,proxy) → sonnet(claude)
    const byModel = sortSeries(list, "model", "stream");
    expect(byModel.map((s) => s.seriesKey)).toEqual([d, c, b, a]);

    expect(list).toEqual(input);
  });

  it("method 不参与排序（与后端 fallback_cmp 三元组一致）：仅 method 不同的并列键保输入稳定序", () => {
    const lateMethod: SeriesKey = {
      tool: "claude",
      model: "opus",
      channel: null,
      method: "zzz-late",
    };
    const earlyMethod: SeriesKey = {
      tool: "claude",
      model: "opus",
      channel: null,
      method: "aaa-early",
    };
    const list = [summary(lateMethod), summary(earlyMethod)];
    // 比较器并列（tool/model/channel 全同）→ 稳定排序保留输入次序，method 不翻序
    expect(sortSeries(list, "tool", "stream").map((s) => s.seriesKey.method)).toEqual([
      "zzz-late",
      "aaa-early",
    ]);
  });

  // ==== 2026-10-27 实机反馈 #5：客户端三键 × 升降序 ====

  it("defaultDirectionFor：组合名升序，速度/会话数降序", () => {
    expect(defaultDirectionFor("name")).toBe("asc");
    expect(defaultDirectionFor("rate")).toBe("desc");
    expect(defaultDirectionFor("sessions")).toBe("desc");
  });

  it("name 键：asc = 稳定序原样，desc 翻转整个比较器结果（含 null 渠道规则）", () => {
    const list = [summary(d), summary(c), summary(b), summary(a)];
    // asc 与旧 tool 键同序：claude(opus,null) → claude(opus,proxy) → claude(sonnet) → zcode
    expect(sortSeries(list, "name", "stream", "asc").map((s) => s.seriesKey)).toEqual([c, b, a, d]);
    // desc = 整个比较器取反：zcode → claude(sonnet) → claude(opus,proxy) → claude(opus,null)
    //（asc 里 null 渠道在前 ⇒ desc 里 proxy 渠道先于 null 渠道——逐字翻转）
    expect(sortSeries(list, "name", "stream", "desc").map((s) => s.seriesKey)).toEqual([
      d,
      a,
      b,
      c,
    ]);
    // 缺省方向 = defaultDirectionFor(name) = asc
    expect(sortSeries(list, "name", "stream").map((s) => s.seriesKey)).toEqual([c, b, a, d]);
    expect(list.map((s) => s.seriesKey)).toEqual([d, c, b, a]); // 输入不被原地改动
  });

  it("rate 键双方向：null 一律排末（asc 与 desc 都是），非空段随方向翻转，并列落稳定序", () => {
    const nullStream = summary(codex, {
      stream: rate(null, 0, 0, 0),
      response: rate(41.3, 1_200, 29_000, 5),
    });
    const list = [
      summary(d, { stream: rate(10, 500, 50_000, 1) }),
      nullStream, // stream null
      summary(a, { stream: rate(50, 500, 10_000, 2) }),
      summary(b, { stream: rate(50, 500, 10_000, 2) }), // 与 a 同值并列
    ];
    // desc：高→低，a/b 并列落稳定序（同 tool 下 opus < sonnet ⇒ b 在 a 前），null 末位
    expect(sortSeries(list, "rate", "stream", "desc").map((s) => s.seriesKey)).toEqual([
      b,
      a,
      d,
      codex,
    ]);
    // asc：低→高，null **仍然**排末（不因方向翻到队首）
    expect(sortSeries(list, "rate", "stream", "asc").map((s) => s.seriesKey)).toEqual([
      d,
      b,
      a,
      codex,
    ]);
    // 缺省方向 = desc（旧三参调用方语义逐字保留）
    expect(sortSeries(list, "rate", "stream")).toEqual(sortSeries(list, "rate", "stream", "desc"));
  });

  it("sessions 键双方向：降序多→少，升序少→多，并列落稳定序；不原地改输入", () => {
    const list = [
      summary(d, { sessionCount: 3 }),
      summary(a, { sessionCount: 7 }),
      summary(b, { sessionCount: 3 }),
      summary(c, { sessionCount: 9 }),
    ];
    // desc：9 → 7 → 3 → 3（两个 3 并列落稳定序：claude(opus,proxy) 在 zcode 前）
    expect(sortSeries(list, "sessions", "stream", "desc").map((s) => s.seriesKey)).toEqual([
      c,
      a,
      b,
      d,
    ]);
    // asc：3 → 3 → 7 → 9
    expect(sortSeries(list, "sessions", "stream", "asc").map((s) => s.seriesKey)).toEqual([
      b,
      d,
      a,
      c,
    ]);
    // 缺省方向 = desc
    expect(sortSeries(list, "sessions", "stream")).toEqual(
      sortSeries(list, "sessions", "stream", "desc")
    );
    expect(list.map((s) => s.metrics.sessionCount)).toEqual([3, 7, 3, 9]); // 输入不动
  });
});

// ==== 4. seriesKeyId ====

describe("seriesKeyId", () => {
  it("channel null / 缺失 / 有值互不碰撞；method 参与区分；序列化稳定", () => {
    const nullChannel: SeriesKey = {
      tool: "zcode",
      model: "glm",
      channel: null,
      method: "stream-observed",
    };
    const withChannel: SeriesKey = {
      tool: "zcode",
      model: "glm",
      channel: "proxy",
      method: "stream-observed",
    };
    const otherMethod: SeriesKey = {
      tool: "zcode",
      model: "glm",
      channel: null,
      method: "client-native",
    };

    expect(seriesKeyId(nullChannel)).toBe(seriesKeyId({ ...nullChannel }));
    expect(seriesKeyId(nullChannel)).not.toBe(seriesKeyId(withChannel));
    expect(seriesKeyId(nullChannel)).not.toBe(seriesKeyId(otherMethod));
    // 重复调用稳定（可作 queryKey 成员）
    expect(seriesKeyId(nullChannel)).toBe(seriesKeyId(nullChannel));
  });
});

// ==== 4.5 translateSpeedReason（reason token 的 i18n 唯一出口）====

describe("translateSpeedReason", () => {
  // i18next 的 t 即可（helper 收 TFunction；i18n.t 与 useTranslation().t 同型）
  const t = i18n.t;

  it("六个已知 token 命中译文（不含键前缀）；含冒号的 Codex token 不被 nsSeparator 截断", async () => {
    await i18n.changeLanguage("zh");
    const known: SpeedReasonToken[] = [
      "stream-gap:insufficient-generation-timing",
      "not-installed",
      "never-read",
      "read-failed",
      "parser-errors",
      "await-restart",
    ];
    for (const token of known) {
      const text = translateSpeedReason(t, token);
      expect(text).not.toContain("speed.reason."); // 不是键串回退
      expect(text).not.toBe(token); // 是译文不是原文
      expect(text.length).toBeGreaterThan(0);
    }
    // 与 locales/zh.json 的 speed.reason.* 逐字对位（含 `:` 键的冒号安全抽查）
    expect(translateSpeedReason(t, "not-installed")).toBe("未安装统计");
    expect(translateSpeedReason(t, "await-restart")).toBe("已安装，等待宿主重启后生效");
    expect(translateSpeedReason(t, "stream-gap:insufficient-generation-timing")).toBe(
      "缺少同调用流式生成窗（响应口径为日志近似）"
    );
  });

  it("未知 token 如实返回原文（不猜、不显示键串）；en 侧同样命中", async () => {
    await i18n.changeLanguage("zh");
    expect(translateSpeedReason(t, "some-future-token")).toBe("some-future-token");
    await i18n.changeLanguage("en");
    expect(translateSpeedReason(t, "not-installed")).toBe("Stats hook not installed");
    expect(translateSpeedReason(t, "never-read")).toBe("Never read any data yet");
    await i18n.changeLanguage("zh");
  });
});

// ==== 5. useSpeedDashboardQuery（假时钟）====

describe("useSpeedDashboardQuery", () => {
  it("键前缀 = [...TPS_KEY,'v2']；六个字段任一变化都换 queryKey", () => {
    expect(SPEED_KEY).toEqual([...TPS_KEY, "v2"]);

    const variants: SpeedDashboardQuery[] = [
      { ...baseQuery, range: "15m" },
      { ...baseQuery, range: "24h" },
      { ...baseQuery, selection: zcode },
      { ...baseQuery, metricMode: "response" },
      { ...baseQuery, toolId: "codex" },
      { ...baseQuery, search: "glm" },
      { ...baseQuery, sort: "rate" },
    ];
    const keys = new Set(variants.map((q) => JSON.stringify(speedDashboardKey(q))));
    expect(keys.size).toBe(variants.length);
    expect(JSON.stringify(speedDashboardKey(baseQuery))).not.toBe(
      JSON.stringify(speedDashboardKey(variants[0]))
    );
    // selection 进键用 seriesKeyId 稳定串（undefined/null 同形）
    expect(JSON.stringify(speedDashboardKey({ ...baseQuery, selection: { ...zcode } }))).toBe(
      JSON.stringify(speedDashboardKey({ ...baseQuery, selection: zcode }))
    );
  });

  it("active:true（opts 必填，显式传）：首查 + 5 秒轮询", async () => {
    vi.useFakeTimers();
    const { wrapper } = createWrapper();
    const { result } = renderHook(() => useSpeedDashboardQuery(baseQuery, { active: true }), {
      wrapper,
    });

    await act(async () => {
      await vi.advanceTimersByTimeAsync(0); // 排空首查 promise（notifyManager 走 setTimeout 调度）
    });
    expect(callsOf("tps_speed_dashboard")).toBe(1); // 挂载即发首查
    expect(result.current.data?.series.length).toBeGreaterThan(0);
    expect(result.current.data?.buckets.length).toBeGreaterThan(0);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(5_000);
    });
    expect(callsOf("tps_speed_dashboard")).toBe(2);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(10_000);
    });
    expect(callsOf("tps_speed_dashboard")).toBe(4);
  });

  it("active:false：60 秒零请求、聚焦零请求；翻回 active 恢复首查与轮询", async () => {
    vi.useFakeTimers();
    const { wrapper } = createWrapper();
    const { result, rerender } = renderHook(
      ({ active }: { active: boolean }) => useSpeedDashboardQuery(baseQuery, { active }),
      { wrapper, initialProps: { active: false } }
    );

    await act(async () => {});
    expect(callsOf("tps_speed_dashboard")).toBe(0);
    expect(result.current.fetchStatus).toBe("idle");

    // 拨过 60 秒：没有轮询；聚焦（visibilitychange ⇒ react-query focusManager）：也没有
    await act(async () => {
      await vi.advanceTimersByTimeAsync(60_000);
    });
    window.dispatchEvent(new Event("visibilitychange"));
    await act(async () => {});
    expect(callsOf("tps_speed_dashboard")).toBe(0);
    expect(result.current.data).toBeUndefined();

    // 翻回 active：enabled false→true 即恢复首查，随后 5 秒节奏接上
    rerender({ active: true });
    await act(async () => {});
    expect(callsOf("tps_speed_dashboard")).toBe(1);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5_000);
    });
    expect(callsOf("tps_speed_dashboard")).toBe(2);
  });

  it("不同 query 字段各自的缓存独立（换 range 不复用另一 range 的数据位）", async () => {
    vi.useFakeTimers();
    const { client, wrapper } = createWrapper();
    const { rerender } = renderHook(
      ({ q }: { q: SpeedDashboardQuery }) => useSpeedDashboardQuery(q, { active: true }),
      { wrapper, initialProps: { q: baseQuery } }
    );
    await act(async () => {});
    expect(callsOf("tps_speed_dashboard")).toBe(1);

    rerender({ q: { ...baseQuery, range: "24h" } });
    await act(async () => {});
    expect(callsOf("tps_speed_dashboard")).toBe(2);

    const keys = client
      .getQueryCache()
      .getAll()
      .map((query) => JSON.stringify(query.queryKey));
    expect(keys).toHaveLength(2);
    expect(new Set(keys).size).toBe(2);
    // 两个键都带 ["tps","v2"] 前缀（与 usage 域隔离）
    for (const k of keys) expect(JSON.parse(k)[0]).toBe("tps");
  });
});

// ==== 6. 用量区间切换不产生 tps_* 请求（两层解耦）====

describe("用量域与速度域解耦", () => {
  it("切用量区间只发 usage 域请求，零新增 tps_*（invoke 层 spy）", async () => {
    vi.useFakeTimers();
    const { wrapper } = createWrapper();
    const { rerender } = renderHook(
      ({ usageRange }: { usageRange: "today" | "last7d" }) => {
        useSpeedDashboardQuery(baseQuery, { active: true });
        useUsageDashboardQuery({ preset: usageRange }, "tool");
      },
      { wrapper, initialProps: { usageRange: "today" as const } }
    );
    await act(async () => {});
    const speedAtStart = callsOf("tps_speed_dashboard");
    const tpsAtStart = tpsIpcCalls();
    expect(speedAtStart).toBe(1);

    // 切用量区间：usage 换键重查；速度域纹丝不动
    rerender({ usageRange: "last7d" });
    await act(async () => {});
    expect(callsOf("tps_speed_dashboard")).toBe(speedAtStart);
    expect(tpsIpcCalls()).toBe(tpsAtStart);
    expect(callsOf("usage_dashboard")).toBe(2);
    // 速度查询从不触发采集/刷新类命令（只读契约：query 路径只有 dashboard 一条命令）
    const tpsCommands = new Set(
      tauriInvokeMock.mock.calls.filter(([c]) => c.startsWith("tps_")).map(([c]) => c)
    );
    expect(tpsCommands).toEqual(new Set(["tps_speed_dashboard"]));
  });
});

// ==== 7. mock 与后端语义对齐（防 Task 10 从 mock 学到错误预期）====

describe("mock 与后端 v2/query.rs 同口径", () => {
  it("series 顺序由 sort + metricMode 决定（rate 当前模式降序、null 排末；换模式即换序）", () => {
    // 默认 sort=tool ⇒ tool 字典序
    const byTool = mockSpeedDashboard();
    const tools = byTool.series.map((s) => s.seriesKey.tool);
    expect(tools).toEqual([...tools].sort());

    // rate+stream：降序，Codex（stream null）排末
    const byRate = mockSpeedDashboard({ ...baseQuery, sort: "rate", metricMode: "stream" });
    const streamValues = byRate.series.map((s) => rateForMode(s.metrics, "stream").value);
    expect(streamValues[streamValues.length - 1]).toBeNull();
    const nonNull = streamValues.filter((v) => v !== null) as number[];
    for (let i = 1; i < nonNull.length; i++) {
      expect(nonNull[i - 1]).toBeGreaterThanOrEqual(nonNull[i]);
    }

    // 换 metricMode 重排：response 模式下 Codex 不再垫底（首名也不同）
    const byRateResponse = mockSpeedDashboard({
      ...baseQuery,
      sort: "rate",
      metricMode: "response",
    });
    expect(byRateResponse.series[0].seriesKey).not.toEqual(byRate.series[0].seriesKey);

    // model 主键：最小 model 是 claude-sonnet-4-5（claude < workbuddy 决定先后）
    const byModel = mockSpeedDashboard({ ...baseQuery, sort: "model" });
    expect(byModel.series[0].seriesKey.model).toBe("claude-sonnet-4-5");
  });

  it("selection 只在筛选后的集合保留，否则回退排序第一（后端 selected_series 同款回退）", () => {
    const plain = mockSpeedDashboard();
    expect(plain.selectedSeries).toEqual(plain.series[0].seriesKey); // 无 selection ⇒ 排序第一
    // 命中：原样保留
    const hit = mockSpeedDashboard({ ...baseQuery, selection: plain.series[2].seriesKey });
    expect(hit.selectedSeries).toEqual(plain.series[2].seriesKey);
    // 未命中筛选集：回退排序第一
    const ghost: SeriesKey = {
      tool: "codex",
      model: "does-not-exist",
      channel: null,
      method: "client-native",
    };
    const missed = mockSpeedDashboard({ ...baseQuery, selection: ghost });
    expect(missed.selectedSeries).toEqual(missed.series[0].seriesKey);
  });

  it("search 为字面子串（tool 或 model 命中）；toolId 叠加过滤；空集 ⇒ 无选择无桶", () => {
    const byModelNeedle = mockSpeedDashboard({ ...baseQuery, search: "glm" });
    expect(byModelNeedle.series.map((s) => s.seriesKey.tool).sort()).toEqual(["opencode", "zcode"]);
    const byToolNeedle = mockSpeedDashboard({ ...baseQuery, search: "codex" });
    expect(byToolNeedle.series).toHaveLength(1);
    expect(byToolNeedle.series[0].seriesKey.tool).toBe("codex");
    const both = mockSpeedDashboard({ ...baseQuery, toolId: "zcode", search: "glm" });
    expect(both.series).toHaveLength(1);
    const none = mockSpeedDashboard({ ...baseQuery, search: "不存在的串" });
    expect(none.series).toHaveLength(0);
    expect(none.selectedSeries).toBeNull();
    expect(none.buckets).toHaveLength(0);
  });
});

// ==== 8. mock 夹具对 TS 类型的运行时结构断言 ====

describe("mock 夹具结构契约", () => {
  const RATE_KEYS = ["value", "outputTokens", "durationMs", "sampleCount"].sort();
  const METRICS_KEYS = [
    "stream",
    "response",
    "avgFirstTokenMs",
    "ttftSampleCount",
    "modelMs",
    "toolMs",
    "candidateCount",
    "failedCount",
    "cancelledCount",
    "missingTimingCount",
    "sessionCount",
    "latestStreamEndedAt",
    "latestResponseEndedAt",
    "outputScope",
    "scopeMixed",
  ];

  function assertRate(r: SpeedRate, label: string) {
    expect(Object.keys(r).sort()).toEqual(RATE_KEYS);
    expect(typeof r.outputTokens).toBe("number");
    expect(typeof r.durationMs).toBe("number");
    expect(typeof r.sampleCount).toBe("number");
    expect(r.value === null || typeof r.value === "number").toBe(true);
    expect(r.value === null ? r.sampleCount === 0 : true).toBe(true); // n=0 ⇒ value=null
    if (label) expect(label).toBeTruthy();
  }

  function assertMetrics(m: SpeedMetrics) {
    expect(Object.keys(m).sort()).toEqual([...METRICS_KEYS].sort());
    assertRate(m.stream, "stream");
    assertRate(m.response, "response");
    for (const k of [
      "avgFirstTokenMs",
      "modelMs",
      "toolMs",
      "latestStreamEndedAt",
      "latestResponseEndedAt",
    ] as const) {
      expect(m[k] === null || typeof m[k] === "number").toBe(true);
    }
    for (const k of [
      "ttftSampleCount",
      "candidateCount",
      "failedCount",
      "cancelledCount",
      "missingTimingCount",
      "sessionCount",
    ] as const) {
      expect(typeof m[k]).toBe("number");
    }
    expect(["exclusiveOutput", "includesReasoning", "providerReported", "unknown"]).toContain(
      m.outputScope
    );
    expect(typeof m.scopeMixed).toBe("boolean");
  }

  function assertSnapshot(s: SpeedDashboardSnapshot) {
    expect(Object.keys(s).sort()).toEqual(
      [
        "asOf",
        "rangeFrom",
        "rangeTo",
        "disabled",
        "sourceStates",
        "series",
        "selectedSeries",
        "buckets",
      ].sort()
    );
    expect(typeof s.asOf).toBe("number");
    expect(typeof s.rangeFrom).toBe("number");
    expect(typeof s.rangeTo).toBe("number");
    expect(typeof s.disabled).toBe("boolean");
    for (const src of s.sourceStates) {
      expect(Object.keys(src).sort()).toEqual(
        ["source", "status", "installed", "capabilities", "lastRecordAt", "reason", "detail"].sort()
      );
      expect(["ready", "unconfigured", "await-restart", "needs-check", "unsupported"]).toContain(
        src.status
      );
      expect(typeof src.capabilities.stream).toBe("boolean");
      expect(typeof src.capabilities.response).toBe("boolean");
      expect(
        src.capabilities.streamReason === null || typeof src.capabilities.streamReason === "string"
      ).toBe(true);
      expect(src.reason === null || typeof src.reason === "string").toBe(true);
    }
    for (const item of s.series) {
      expect(Object.keys(item).sort()).toEqual(["metrics", "seriesKey"]);
      expect(typeof item.seriesKey.tool).toBe("string");
      expect(item.seriesKey.channel === null || typeof item.seriesKey.channel === "string").toBe(
        true
      );
      assertMetrics(item.metrics);
    }
    for (const bucket of s.buckets) {
      expect(Object.keys(bucket).sort()).toEqual(
        ["startMs", "endMs", "isPartialBucket", "metrics"].sort()
      );
      expect(typeof bucket.startMs).toBe("number");
      expect(typeof bucket.endMs).toBe("number");
      expect(typeof bucket.isPartialBucket).toBe("boolean");
      assertMetrics(bucket.metrics);
      expect(bucket.endMs).toBeGreaterThan(bucket.startMs);
    }
  }

  it("mockSpeedDashboard 默认夹具：双源双率+TTFT、Codex 仅 response、空桶/部分桶/混 scope/空工具时长", () => {
    const snap = mockSpeedDashboard();
    assertSnapshot(snap);

    // 七来源各一条 series
    expect(snap.series).toHaveLength(7);
    // Codex：stream 无值（n=0，null 不伪装），response 有值
    const codexSeries = snap.series.find((s) => s.seriesKey.tool === "codex");
    expect(codexSeries).toBeDefined();
    expect(rateForMode(codexSeries!.metrics, "stream").value).toBeNull();
    expect(rateForMode(codexSeries!.metrics, "stream").sampleCount).toBe(0);
    expect(rateForMode(codexSeries!.metrics, "response").value).not.toBeNull();
    expect(codexSeries!.metrics.avgFirstTokenMs).toBeNull(); // TTFT 不伪造
    // 原生源双率齐备
    const native = snap.series.find((s) => s.seriesKey.tool === "zcode")!;
    expect(rateForMode(native.metrics, "stream").value).not.toBeNull();
    expect(rateForMode(native.metrics, "response").value).not.toBeNull();
    expect(native.metrics.avgFirstTokenMs).not.toBeNull();
    // 混 scope 一条
    expect(snap.series.some((s) => s.metrics.scopeMixed)).toBe(true);
    // toolMs=null 一条（非 Codex）
    expect(snap.series.some((s) => s.metrics.toolMs === null && s.seriesKey.tool !== "codex")).toBe(
      true
    );
    // 桶：有部分桶、有全空桶
    expect(snap.buckets.some((b) => b.isPartialBucket)).toBe(true);
    expect(snap.buckets.some((b) => rateForMode(b.metrics, "stream").sampleCount === 0)).toBe(true);
    // 来源状态里 Codex 的 stream 缺口 token 可见（i18n 映射的素材）
    const codexView = snap.sourceStates.find((v) => v.source === "codex");
    expect(codexView?.capabilities.streamReason).toBe("stream-gap:insufficient-generation-timing");
  });

  it("mockSpeedDashboard 尊重传入 query（范围决定桶几何）", () => {
    const snap = mockSpeedDashboard({ ...baseQuery, range: "15m" });
    const widths = snap.buckets.map((b) => b.endMs - b.startMs);
    // 首桶是边缘裁剪出的部分桶（≤1 分钟；恰好整点对齐时退化为满桶）；至少一个满 1 分钟桶；
    // 桶间首尾相接无缝隙
    expect(snap.buckets[0].isPartialBucket || widths[0] === 60_000).toBe(true);
    expect(Math.max(...widths)).toBeLessThanOrEqual(60_000);
    expect(widths.some((w) => w === 60_000)).toBe(true);
    for (let i = 1; i < snap.buckets.length; i++) {
      expect(snap.buckets[i].startMs).toBe(snap.buckets[i - 1].endMs);
    }
    // 覆盖恰为 [rangeFrom, rangeTo]
    expect(snap.buckets[0].startMs).toBe(snap.rangeFrom);
    expect(snap.buckets[snap.buckets.length - 1].endMs).toBe(snap.rangeTo);

    const snap24 = mockSpeedDashboard({ ...baseQuery, range: "24h" });
    const widths24 = snap24.buckets.map((b) => b.endMs - b.startMs);
    expect(Math.max(...widths24)).toBeLessThanOrEqual(3_600_000);
    expect(widths24.some((w) => w === 3_600_000)).toBe(true);
  });

  it("tests/msw 注入点可变能力位（setMockSpeedDashboard 覆盖后命令返回覆盖值）", async () => {
    const overridden: SpeedDashboardSnapshot = {
      ...mockSpeedDashboard(),
      series: [summary(codex, { stream: rate(null, 0, 0, 0), response: rate(30, 900, 30_000, 3) })],
      disabled: true,
    };
    setMockSpeedDashboard(() => overridden);
    const { wrapper } = createWrapper();
    const { result } = renderHook(() => useSpeedDashboardQuery(baseQuery, { active: true }), {
      wrapper,
    });
    await waitFor(() => expect(result.current.data).toBeDefined());
    expect(result.current.data?.disabled).toBe(true);
    expect(result.current.data?.series).toHaveLength(1);
  });
});
