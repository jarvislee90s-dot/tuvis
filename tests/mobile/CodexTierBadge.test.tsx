// #172/T9b：移动端 Codex 三态徽标 + 探测中骨架 + 可取消 + 失败态原因文案。
//
// 覆盖四条验收线：
// - 三态映射**只取唯一映射源** `codexCapabilityTier()`（徽标与测试同源，杜绝两份
//   对照表漂移）；
// - 探测在飞 → 出**骨架**，且**不出现**「未解锁」定论（结论未定不抢跑）；
// - 可取消 → 点「取消等待」后骨架收起、出现「已取消等待」文案（客户端放弃等待，
//   服务端探测 15s 硬上限自行结束，界面如实写清这层语义）；
// - 失败态 → `probe_failed:P2` 显示基线文案且带上 P2。
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it } from "vitest";
import i18n from "@/i18n";
import { CodexAvailabilityBanner } from "@/mobile/PendingRequestCard";
import {
  codexCapabilityTier,
  probeReasonErrorKey,
  probeReasonStage,
  type ControlAvailability,
  type ProbeStatus,
} from "@/mobile/codexIpcApi";

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

afterEach(() => {
  cleanup();
});

describe("codexCapabilityTier：四档 → 三态（唯一映射源）", () => {
  it("tier_map：controllable/send_only → verified，read_only/compat_only → compat，unavailable → locked", () => {
    expect(codexCapabilityTier("controllable")).toBe("verified");
    expect(codexCapabilityTier("send_only")).toBe("verified");
    expect(codexCapabilityTier("read_only")).toBe("compat");
    expect(codexCapabilityTier("compat_only")).toBe("compat");
    expect(codexCapabilityTier("unavailable")).toBe("locked");
  });

  it("tier_map_unknown：词表外取值不猜「已验证」→ 落到最保守档", () => {
    expect(codexCapabilityTier("brand_new" as ControlAvailability)).toBe("locked");
  });
});

describe("CodexAvailabilityBanner：三态徽标", () => {
  it("tier_badge：徽标按三态呈现（文案 + data-tier）", () => {
    const { unmount } = render(
      <CodexAvailabilityBanner availability="controllable" stale={false} />
    );
    expect(screen.getByTestId("codex-tier-badge").textContent).toBe("已验证");
    expect(screen.getByTestId("codex-tier-badge").getAttribute("data-tier")).toBe("verified");
    unmount();

    render(<CodexAvailabilityBanner availability="compat_only" stale={false} />);
    expect(screen.getByTestId("codex-tier-badge").textContent).toBe("自动适配（未完全解锁）");
    expect(screen.getByTestId("codex-tier-badge").getAttribute("data-tier")).toBe("compat");
    cleanup();

    render(<CodexAvailabilityBanner availability="unavailable" stale={false} />);
    expect(screen.getByTestId("codex-tier-badge").textContent).toBe("未解锁");
    expect(screen.getByTestId("codex-tier-badge").getAttribute("data-tier")).toBe("locked");
  });
});

describe("CodexAvailabilityBanner：探测中骨架", () => {
  it("probe_skeleton：probing → 出骨架，且不显示「未解锁」定论", () => {
    const probe: ProbeStatus = { state: "probing", stage: null, reasonCode: null };
    render(
      <CodexAvailabilityBanner availability="unavailable" stale={false} probeStatus={probe} />
    );
    expect(screen.getByTestId("codex-probe-skeleton")).toBeTruthy();
    expect(screen.getByTestId("codex-probe-skeleton-label").textContent).toContain("正在探测");
    // 结论未定 → 不得出现任何档位定论（含「未解锁」）
    expect(screen.queryByText("未解锁")).toBeNull();
    expect(screen.queryByTestId("codex-tier-badge")).toBeNull();
  });

  it("probe_skeleton_absent：非 probing 不出骨架（老后端无 probeStatus 也不出）", () => {
    render(<CodexAvailabilityBanner availability="controllable" stale={false} />);
    expect(screen.queryByTestId("codex-probe-skeleton")).toBeNull();
    cleanup();

    const probe: ProbeStatus = { state: "probed", stage: "P3", reasonCode: null };
    render(
      <CodexAvailabilityBanner availability="controllable" stale={false} probeStatus={probe} />
    );
    expect(screen.queryByTestId("codex-probe-skeleton")).toBeNull();
  });
});

describe("CodexAvailabilityBanner：可取消等待", () => {
  it("probe_cancel：点取消 → 骨架收起、出现「已取消等待」文案", () => {
    const probe: ProbeStatus = { state: "probing", stage: null, reasonCode: null };
    render(
      <CodexAvailabilityBanner availability="unavailable" stale={false} probeStatus={probe} />
    );
    expect(screen.getByTestId("codex-probe-skeleton")).toBeTruthy();

    fireEvent.click(screen.getByTestId("codex-probe-cancel"));

    expect(screen.queryByTestId("codex-probe-skeleton")).toBeNull();
    const note = screen.getByTestId("codex-probe-wait-cancelled");
    expect(note.textContent).toContain("已取消等待");
    // 语义必须写清：取消的是「等待」，服务端探测自己会结束
    expect(note.textContent).toContain("15 秒");
  });

  it("probe_cancel_sticky：取消后不再反复显示骨架（同一 probing 态重渲染也不回弹）", () => {
    const probe: ProbeStatus = { state: "probing", stage: null, reasonCode: null };
    const { rerender } = render(
      <CodexAvailabilityBanner availability="unavailable" stale={false} probeStatus={probe} />
    );
    fireEvent.click(screen.getByTestId("codex-probe-cancel"));
    // 服务端仍在探测（同一 probing 态）→ 骨架不得回弹
    rerender(
      <CodexAvailabilityBanner availability="unavailable" stale={false} probeStatus={probe} />
    );
    expect(screen.queryByTestId("codex-probe-skeleton")).toBeNull();
    expect(screen.getByTestId("codex-probe-wait-cancelled")).toBeTruthy();
  });
});

describe("CodexAvailabilityBanner：失败/不确定态原因", () => {
  it("probe_failed_stage：reasonCode=probe_failed:P2 → 基线文案 + 带 P2", () => {
    const probe: ProbeStatus = { state: "failed", stage: "P2", reasonCode: "probe_failed:P2" };
    render(
      <CodexAvailabilityBanner availability="compat_only" stale={false} probeStatus={probe} />
    );
    const reason = screen.getByTestId("codex-probe-reason");
    expect(reason.textContent).toContain(i18n.t("errors.probeFailed"));
    expect(reason.textContent).toContain("P2");
  });

  it("probe_inconclusive_reason：inconclusive → 显示漂移疑似基线文案", () => {
    const probe: ProbeStatus = {
      state: "inconclusive",
      stage: "P1",
      reasonCode: "protocol_drift_suspected",
    };
    render(
      <CodexAvailabilityBanner availability="compat_only" stale={false} probeStatus={probe} />
    );
    const reason = screen.getByTestId("codex-probe-reason");
    expect(reason.textContent).toContain(i18n.t("errors.protocolDriftSuspected"));
  });

  it("probe_reason_unknown：词表外的码不猜 → 通用「原因未知」", () => {
    const probe: ProbeStatus = { state: "failed", stage: null, reasonCode: "method_not_found" };
    render(
      <CodexAvailabilityBanner availability="compat_only" stale={false} probeStatus={probe} />
    );
    const reason = screen.getByTestId("codex-probe-reason");
    expect(reason.textContent).toBe(i18n.t("mobile.codex.probe.reasonUnknown"));
  });

  it("probe_idle_unknown：idle + 无码（取不到）→ 不编一句「没通过」出来", () => {
    const probe: ProbeStatus = { state: "idle", stage: null, reasonCode: null };
    render(
      <CodexAvailabilityBanner availability="compat_only" stale={false} probeStatus={probe} />
    );
    expect(screen.queryByTestId("codex-probe-reason")).toBeNull();
    // 档位徽标照常（idle 不是探测在飞）
    expect(screen.getByTestId("codex-tier-badge").textContent).toBe("自动适配（未完全解锁）");
  });

  it("probe_unverified_reason：idle + desktop_build_unverified → 显示「该版本尚未验证」基线文案", () => {
    const probe: ProbeStatus = {
      state: "idle",
      stage: null,
      reasonCode: "desktop_build_unverified",
    };
    render(
      <CodexAvailabilityBanner availability="compat_only" stale={false} probeStatus={probe} />
    );
    const reason = screen.getByTestId("codex-probe-reason");
    expect(reason.textContent).toContain(i18n.t("errors.desktopBuildUnverified"));
  });
});

describe("reasonCode → errors 段键（词表与前缀匹配）", () => {
  it("reason_key_map：四个探测码各自命中 errors 键；动态码按前缀取基线", () => {
    expect(probeReasonErrorKey("desktop_build_unverified")).toBe("errors.desktopBuildUnverified");
    expect(probeReasonErrorKey("probe_failed")).toBe("errors.probeFailed");
    expect(probeReasonErrorKey("probe_failed:P2")).toBe("errors.probeFailed");
    expect(probeReasonErrorKey("protocol_drift_suspected")).toBe("errors.protocolDriftSuspected");
    expect(probeReasonErrorKey("probe_inconclusive")).toBe("errors.probeInconclusive");
    // 词表外 → null（不猜）
    expect(probeReasonErrorKey("method_not_found")).toBeNull();
    expect(probeReasonErrorKey(null)).toBeNull();
  });

  it("reason_stage：动态码里的 stage 单独取出", () => {
    expect(probeReasonStage("probe_failed:P2")).toBe("P2");
    expect(probeReasonStage("probe_failed:P9")).toBeNull();
    expect(probeReasonStage("probe_failed")).toBeNull();
    expect(probeReasonStage(null)).toBeNull();
  });
});
