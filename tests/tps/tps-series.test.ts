// @vitest-environment node
import { describe, expect, it } from "vitest";

import { parseTpsStatus, tpsSegments } from "@/lib/usage/tpsSeries";
import type { TpsPoint } from "@/types/tps";

describe("tpsSeries 纯函数", () => {
  it("R10：parseTpsStatus 解析 tps-installed 机器 token（独立于 status），原文余量保留", () => {
    expect(parseTpsStatus("tps-status=active; tps-installed=true; status=supported")).toEqual({
      status: "active",
      installed: true,
      rest: "status=supported",
    });
    expect(parseTpsStatus("tps-status=unconfigured; tps-installed=false")).toEqual({
      status: "unconfigured",
      installed: false,
      rest: null,
    });
    // 非可安装来源：无 installed token → null
    expect(parseTpsStatus("tps-status=active")).toEqual({
      status: "active",
      installed: null,
      rest: null,
    });
    expect(parseTpsStatus("tps-status=read-failed; tps-receiver-port-conflict: x")).toEqual({
      status: "read-failed",
      installed: null,
      rest: "tps-receiver-port-conflict: x",
    });
    expect(parseTpsStatus(null)).toEqual({ status: null, installed: null, rest: null });
  });

  describe("R11：缺失计划点按点自带的计划节奏断线（不猜通用阈值）", () => {
    const SERIES = { tool: "zcode", model: "m", channel: null, method: "client-native" } as const;
    function pt(
      sampledAt: number,
      cadenceMs: number | null,
      over: Partial<TpsPoint> = {}
    ): TpsPoint {
      return {
        modelMs: null,
        toolMs: null,
        avgFirstTokenMs: null,
        tps: 50,
        sampleCount: 1,
        ttftSampleCount: 0,
        candidateCount: 1,
        timedModelCount: 1,
        timedToolCount: 0,
        failedCount: 0,
        cancelledCount: 0,
        missingTimingCount: 0,
        latestEndedAt: sampledAt - 1000,
        ageMs: 1000,
        method: "client-native",
        outputScope: "provider-reported",
        quality: "normal",
        sumOutputTokens: 100,
        sumGenerationMs: 2000,
        ttftSumMs: null,
        pointId: `p-${sampledAt}`,
        seriesKey: SERIES,
        sampledAt,
        windowMs: 120_000,
        runId: "r1",
        trigger: "scheduled",
        status: "value",
        cadenceMs,
        sampledKeys: ["k"],
        ...over,
      } as TpsPoint;
    }
    const times = (pts: TpsPoint[]) => tpsSegments(pts).map((seg) => seg.map((p) => p.sampledAt));

    it("可见期 0 / 120000ms（缺中间 60 秒点）→ 断线", () => {
      expect(times([pt(0, 60_000), pt(120_000, 60_000)])).toEqual([[0], [120_000]]);
    });
    it("后台 0 / 1200000ms（缺中间 600 秒点）→ 断线", () => {
      expect(times([pt(0, 600_000), pt(1_200_000, 600_000)])).toEqual([[0], [1_200_000]]);
    });
    it("合法后台 600 秒连续点不断线；抖动在 1.5× 节奏内不断线", () => {
      expect(times([pt(0, 600_000), pt(600_000, 600_000), pt(1_200_000, 600_000)])).toEqual([
        [0, 600_000, 1_200_000],
      ]);
      expect(times([pt(0, 60_000), pt(75_000, 60_000)])).toEqual([[0, 75_000]]);
    });
    it("manual 点夹在计划点之间不断线", () => {
      expect(
        times([pt(0, 60_000), pt(20_000, 60_000, { trigger: "manual" }), pt(60_000, 60_000)])
      ).toEqual([[0, 20_000, 60_000]]);
    });
    it("显式 gap 点处断线（gap 不出现在任何段）", () => {
      expect(
        times([pt(0, 600_000), pt(600_000, 600_000, { status: "gap" }), pt(1_200_000, 600_000)])
      ).toEqual([[0], [1_200_000]]);
    });
    it("模式切换（可见→隐藏）：下一后台点带 600 秒节奏，不误判为缺点", () => {
      expect(times([pt(0, 60_000), pt(600_000, 600_000)])).toEqual([[0, 600_000]]);
    });
    it("runId 更换处断线", () => {
      expect(times([pt(0, 600_000), pt(600_000, 600_000, { runId: "r2" })])).toEqual([
        [0],
        [600_000],
      ]);
    });
    it("旧库行无节奏信息（null）：不据此断线，只认 gap / runId", () => {
      expect(times([pt(0, null), pt(5_000_000, null)])).toEqual([[0, 5_000_000]]);
    });
    it("R11'：manual 点不得遮蔽缺失的计划锚点（可见反例：0 → manual@50k → 120k，60k 锚点缺失）", () => {
      expect(
        times([pt(0, 60_000), pt(50_000, 60_000, { trigger: "manual" }), pt(120_000, 60_000)])
      ).toEqual([[0, 50_000], [120_000]]);
    });
    it("R11'：后台对应场景（0 → manual@700k → 1800k，600k/1200k 锚点缺失）", () => {
      expect(
        times([pt(0, 600_000), pt(700_000, 600_000, { trigger: "manual" }), pt(1_800_000, 600_000)])
      ).toEqual([[0, 700_000], [1_800_000]]);
    });
    it("NB4：manual 浮点不得桥接 gap（anchor@0、anchor@60k、gap@80k、manual@90k → 两段）", () => {
      expect(
        times([
          pt(0, 60_000),
          pt(60_000, 60_000),
          pt(80_000, null, { status: "gap" }),
          pt(90_000, 60_000, { trigger: "manual" }),
        ])
      ).toEqual([[0, 60_000], [90_000]]);
    });
    it("NB4：manual 浮点不得跨过缺失的计划锚点连线（anchor@0(60k) + manual@100k → 两段）", () => {
      expect(times([pt(0, 60_000), pt(100_000, 60_000, { trigger: "manual" })])).toEqual([
        [0],
        [100_000],
      ]);
    });
    it("B2：早于全部锚点的浮点前插不得跨显式 gap（manual@0 + gap@30k + anchor@60k → 两段）", () => {
      expect(
        times([
          pt(0, 60_000, { trigger: "manual" }),
          pt(30_000, null, { status: "gap" }),
          pt(60_000, 60_000),
        ])
      ).toEqual([[0], [60_000]]);
    });
    it("B2：早于全部锚点的浮点前插不得跨缺失计划锚点（manual@0 + anchor@100k(60k) → 两段）", () => {
      expect(times([pt(0, 60_000, { trigger: "manual" }), pt(100_000, 60_000)])).toEqual([
        [0],
        [100_000],
      ]);
    });
  });
});
