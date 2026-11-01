// R07（D3）：适配详情的**能力事实同源**与**真实重连展示**。
//
// 验收（集中审查 R07 原文）：
// - 详情消费页面同版 `effectiveOperations`（不是打开时独立 GET 的旧快照）；
// - 独立 GET 只补版本/设置/诊断，且失败时能力表仍可用；
// - 同一完整目标（capabilityIdentity 相同）重连期间沿用上次支持展示并如实标
//   「连接恢复中」；身份变化即清展示缓存（不继承旧目标能力）。
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import CodexAdaptSheet from "@/mobile/CodexAdaptSheet";
import type { CodexAdaptDetails, CodexEffectiveOperations } from "@/mobile/codexIpcApi";

const fetchCodexAdaptStatus = vi.fn();
vi.mock("@/mobile/codexIpcApi", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/mobile/codexIpcApi")>();
  return {
    ...actual,
    fetchCodexAdaptStatus: (...args: unknown[]) => fetchCodexAdaptStatus(...args),
    updateCodexAdaptSettings: vi.fn(),
  };
});

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

afterEach(() => {
  cleanup();
  fetchCodexAdaptStatus.mockReset();
});

function details(over: Partial<CodexAdaptDetails> = {}): CodexAdaptDetails {
  return {
    sessionId: "s1",
    systemVersion: "macos 27",
    tuvisVersion: "0.6.3",
    desktopBuild: "26.1023.51421",
    autoAdaptEnabled: true,
    hintSeen: true,
    lastFailure: null,
    ...over,
  };
}

function operations(
  allowed: boolean,
  support: CodexEffectiveOperations["operations"][string] extends infer T
    ? T extends { support?: infer S }
      ? S
      : never
    : never = "supported"
): CodexEffectiveOperations {
  return {
    policyRevision: allowed ? 7 : 8,
    capabilityIdentity: "cap-id-1",
    operations: {
      async_answer_idle: {
        implemented: true,
        allowed,
        requiresAutoAdapt: true,
        evidence: "contract_matched",
        support,
        reasonCode: allowed ? null : "operation_auto_adapt_off",
      },
    },
  };
}

describe("R07 详情能力事实同源", () => {
  it("r07_detail_renders_page_scope_operations_not_its_own_get", async () => {
    // 独立 GET 给出**旧**快照（allowed=true）；页面同版是 allowed=false。
    fetchCodexAdaptStatus.mockResolvedValue(
      details({
        effectiveOperations: operations(true),
        autoAdaptEnabled: false,
      })
    );
    render(
      <CodexAdaptSheet
        sessionId="s1"
        open
        onClose={() => undefined}
        effectiveOperations={operations(false)}
      />
    );
    // 版本/设置/诊断来自独立 GET（辅助面）。
    expect(await screen.findByTestId("codex-adapt-system")).toHaveTextContent("macos 27");
    expect(screen.getByTestId("codex-adapt-toggle")).toHaveAttribute("aria-checked", "false");
    // 能力表来自**页面同版**，不是 GET 的旧快照。
    const row = screen.getByTestId("codex-adapt-op-async_answer_idle");
    expect(row).toHaveAttribute("data-allowed", "false");
    expect(row).toHaveTextContent("适配已关闭");
  });

  it("r07_detail_follows_page_operations_when_they_change_after_open", async () => {
    fetchCodexAdaptStatus.mockResolvedValue(details({ effectiveOperations: operations(true) }));
    const { rerender } = render(
      <CodexAdaptSheet
        sessionId="s1"
        open
        onClose={() => undefined}
        effectiveOperations={operations(true, "supported")}
      />
    );
    await screen.findByTestId("codex-adapt-system");
    expect(screen.getByTestId("codex-adapt-op-async_answer_idle")).toHaveAttribute(
      "data-allowed",
      "true"
    );

    // 页面换了一版：allowed 翻转 + support 变为识别中（新目标/未识别）。
    rerender(
      <CodexAdaptSheet
        sessionId="s1"
        open
        onClose={() => undefined}
        effectiveOperations={operations(false, "identifying")}
      />
    );
    const row = screen.getByTestId("codex-adapt-op-async_answer_idle");
    expect(row).toHaveAttribute("data-allowed", "false");
    expect(row).toHaveAttribute("data-support", "identifying");
    expect(row).toHaveTextContent("识别中");
  });

  it("r07_detail_operations_survive_independent_get_failure", async () => {
    fetchCodexAdaptStatus.mockRejectedValue(new Error("network down"));
    render(
      <CodexAdaptSheet
        sessionId="s1"
        open
        onClose={() => undefined}
        effectiveOperations={operations(true)}
      />
    );
    // 独立 GET 失败只影响辅助面（版本/设置/诊断 + 错误提示）。
    expect(await screen.findByRole("alert")).toHaveTextContent("读取适配状态失败");
    // 能力表照常可用（页面同版事实不依赖独立 GET）。
    expect(screen.getByTestId("codex-adapt-op-async_answer_idle")).toHaveAttribute(
      "data-allowed",
      "true"
    );
  });

  it("r07_missing_page_operations_does_not_fall_back_to_stale_get", async () => {
    // 页面还没给出能力事实（旧载荷/未知）：详情不得改用独立 GET 的旧快照冒充。
    fetchCodexAdaptStatus.mockResolvedValue(details({ effectiveOperations: operations(true) }));
    render(<CodexAdaptSheet sessionId="s1" open onClose={() => undefined} />);
    await screen.findByTestId("codex-adapt-system");
    expect(screen.queryByTestId("codex-adapt-op-async_answer_idle")).toBeNull();
  });
});
