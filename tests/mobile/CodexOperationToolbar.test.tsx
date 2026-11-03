import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import CodexOperationToolbar from "@/mobile/CodexOperationToolbar";
import type { CodexEffectiveOperations } from "@/mobile/codexIpcApi";

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

afterEach(cleanup);

function operations(over: Partial<CodexEffectiveOperations> = {}): CodexEffectiveOperations {
  return {
    policyRevision: 7,
    operations: {
      send: {
        implemented: true,
        allowed: true,
        requiresAutoAdapt: false,
        evidence: "contract_matched",
        reasonCode: null,
      },
      async_answer_idle: {
        implemented: true,
        allowed: false,
        requiresAutoAdapt: true,
        evidence: "contract_matched",
        reasonCode: "operation_runtime_active",
      },
      command_approval: {
        implemented: true,
        allowed: true,
        requiresAutoAdapt: false,
        evidence: "shipped",
        reasonCode: null,
      },
    },
    ...over,
  };
}

describe("F08/T10 一行操作工具栏", () => {
  it("row_shows_available_and_count_only：常态一行，不渲染未实现操作", () => {
    render(<CodexOperationToolbar operations={operations()} />);
    const row = screen.getByTestId("codex-ops-row");
    expect(row.textContent).toContain("可用操作");
    expect(row.textContent).toContain("发消息");
    expect(row.textContent).toContain("批准");
    // 未实现的操作（request_cancel 等）不出现在任何位置
    expect(row.textContent).not.toContain("request_cancel");
    expect(screen.queryByTestId("codex-ops-blocked")).toBeNull();
  });

  it("b02_unavailable_stays_out_of_main_row：不可用项与原因只进详情", () => {
    const onOpenDetails = vi.fn();
    render(<CodexOperationToolbar operations={operations()} onOpenDetails={onOpenDetails} />);
    const row = screen.getByTestId("codex-ops-row");
    // 主区域没有「N 项不可用」/展开列表/更多入口，只有一个详情入口
    expect(screen.queryByTestId("codex-ops-expand")).toBeNull();
    expect(screen.queryByTestId("codex-ops-blocked")).toBeNull();
    expect(screen.queryByTestId("codex-ops-more")).toBeNull();
    expect(row.textContent).not.toContain("不可用");
    fireEvent.click(screen.getByTestId("codex-ops-details"));
    expect(onOpenDetails).toHaveBeenCalledTimes(1);
  });

  it("missing_operations_is_unknown_not_all_green：缺字段 = 未知，不假装全可用", () => {
    render(<CodexOperationToolbar operations={undefined} />);
    const row = screen.getByTestId("codex-ops-row");
    expect(row.textContent).toContain("状态待核对");
    expect(row.textContent).not.toContain("发消息");
    expect(screen.queryByTestId("codex-ops-expand")).toBeNull();
  });
});

describe("B02 单行横向", () => {
  it("b02_all_available_chips_on_one_row_without_more", () => {
    const op = (allowed: boolean): CodexEffectiveOperations["operations"][string] => ({
      implemented: true,
      allowed,
      requiresAutoAdapt: false,
      evidence: "shipped",
      reasonCode: null,
    });
    render(
      <CodexOperationToolbar
        onOpenDetails={() => undefined}
        operations={{
          policyRevision: 9,
          operations: {
            send: op(true),
            command_approval: op(true),
            file_change_approval: op(true),
            blocking_input: op(true),
            async_answer_idle: op(false),
          },
        }}
      />
    );
    const chips = screen.getByTestId("codex-ops-chips");
    expect(chips.className).toContain("overflow-x-auto");
    expect(chips.className).toContain("whitespace-nowrap");
    expect(screen.getByTestId("codex-ops-chip-send")).toBeTruthy();
    expect(screen.getByTestId("codex-ops-chip-blocking_input")).toBeTruthy();
    expect(screen.queryByTestId("codex-ops-more")).toBeNull();
    expect(screen.getByTestId("codex-ops-details")).toBeTruthy();
  });
});

describe("T03 支持状态与当前资格分离", () => {
  /** 已匹配目标的契约态（support=supported 的事实底座）。 */
  const matched = (
    over: Partial<CodexEffectiveOperations["operations"][string]> = {}
  ): CodexEffectiveOperations["operations"][string] => ({
    implemented: true,
    allowed: true,
    requiresAutoAdapt: true,
    evidence: "contract_matched",
    support: "supported",
    reasonCode: null,
    ...over,
  });

  function operations(asyncAnswer: CodexEffectiveOperations["operations"][string]) {
    return {
      policyRevision: 7,
      capabilityIdentity: "cap-id-1",
      operations: {
        send: matched({ requiresAutoAdapt: false }),
        async_answer_idle: asyncAnswer,
        command_approval: matched({ evidence: "shipped", requiresAutoAdapt: false }),
      },
    } satisfies CodexEffectiveOperations;
  }

  /** 当前芯片序（chip testid；reason testid 前缀不同不会被误计）。 */
  function chipOrder(): string[] {
    return Array.from(
      document.querySelectorAll<HTMLElement>("[data-testid^='codex-ops-chip-']")
    ).map((el) => el.getAttribute("data-testid")!);
  }

  it("t03_chip_presence_and_position_stable_when_allowed_flips_idle_to_active", () => {
    render(<CodexOperationToolbar operations={operations(matched())} />);
    const idleOrder = chipOrder();
    const idleChipText = screen.getByTestId("codex-ops-chip-async_answer_idle").textContent;
    expect(idleOrder).toContain("codex-ops-chip-async_answer_idle");
    expect(screen.queryByTestId("codex-ops-reason-async_answer_idle")).toBeNull();
    cleanup();

    render(
      <CodexOperationToolbar
        operations={operations(matched({ allowed: false, reasonCode: "operation_runtime_active" }))}
      />
    );
    // 芯片存在性与位置（相对顺序）稳定——运行中不撤芯片，当前原因只放详情。
    expect(chipOrder()).toEqual(idleOrder);
    // 芯片自身文案不变（原因不再出现在主行，详情保留状态）。
    expect(screen.getByTestId("codex-ops-chip-async_answer_idle").textContent).toBe(idleChipText);
    expect(screen.getByTestId("codex-ops-chip-async_answer_idle").textContent).toBe(
      i18n.t("mobile.codex.ops.labels.asyncAnswer")
    );
    expect(screen.queryByTestId("codex-ops-reason-async_answer_idle")).toBeNull();
  });

  it("t03_short_disconnect_keeps_chip_and_only_swaps_reason", () => {
    render(
      <CodexOperationToolbar
        operations={operations(matched({ allowed: false, reasonCode: "operation_target_stale" }))}
      />
    );
    expect(screen.getByTestId("codex-ops-chip-async_answer_idle")).toBeTruthy();
    expect(screen.queryByTestId("codex-ops-reason-async_answer_idle")).toBeNull();
    // 断链不得把已支持说成"该版本未匹配"。
    expect(screen.getByTestId("codex-ops-chips").textContent).not.toContain("该版本未匹配");
  });

  it("t03_auto_adapt_off_keeps_supported_chip_and_shows_adapt_off_reason", () => {
    render(
      <CodexOperationToolbar
        operations={operations(matched({ allowed: false, reasonCode: "operation_auto_adapt_off" }))}
      />
    );
    expect(screen.getByTestId("codex-ops-chip-async_answer_idle").textContent).toBe(
      i18n.t("mobile.codex.ops.labels.asyncAnswer")
    );
    expect(screen.queryByTestId("codex-ops-reason-async_answer_idle")).toBeNull();
  });

  it("t03_legacy_payload_without_support_field_still_renders_display_only", () => {
    // 旧载荷：无 support 字段——按 evidence 推导展示（仅展示，不产生新写资格）。
    const legacy = operations(matched({ allowed: false, reasonCode: "operation_runtime_active" }));
    for (const status of Object.values(legacy.operations)) {
      delete (status as { support?: unknown }).support;
    }
    render(<CodexOperationToolbar operations={legacy} />);
    expect(screen.getByTestId("codex-ops-chip-async_answer_idle").textContent).toBe(
      i18n.t("mobile.codex.ops.labels.asyncAnswer")
    );
    expect(screen.queryByTestId("codex-ops-reason-async_answer_idle")).toBeNull();
  });

  it("t03_identifying_and_unmatched_get_their_own_copy", () => {
    render(
      <CodexOperationToolbar
        operations={{
          policyRevision: 3,
          operations: {
            send: matched({ requiresAutoAdapt: false }),
            async_answer_idle: matched({
              evidence: "none",
              support: "identifying",
              allowed: false,
              reasonCode: "operation_no_evidence",
            }),
            blocking_input: matched({
              evidence: "none",
              support: "unmatched",
              allowed: false,
              reasonCode: "operation_contract_unmatched",
            }),
            // 后端 evaluate() 恒发全部 12 键：本阶段未实现项也随集合下发——
            // 「未接入」在哪个分区都不得出现（改造前行为，组件契约）。
            request_cancel: {
              implemented: false,
              allowed: false,
              requiresAutoAdapt: false,
              evidence: "none",
              support: "not_implemented",
              reasonCode: "operation_not_implemented",
            },
          },
        }}
      />
    );
    // 非 supported 的已实现操作：不进主行，详情如实展示。
    expect(screen.queryByTestId("codex-ops-chip-async_answer_idle")).toBeNull();
    expect(screen.queryByTestId("codex-ops-unsupported-async_answer_idle")).toBeNull();
    expect(screen.queryByTestId("codex-ops-unsupported-blocking_input")).toBeNull();
    // 未实现操作仍不出现在任何位置（元素级 + 子串双断言）。
    expect(screen.queryByTestId("codex-ops-unsupported-request_cancel")).toBeNull();
    expect(screen.getByTestId("codex-ops-chips").textContent).not.toContain("request_cancel");
  });
});

describe("R07 连接恢复中提示", () => {
  const matched = (
    over: Partial<CodexEffectiveOperations["operations"][string]> = {}
  ): CodexEffectiveOperations["operations"][string] => ({
    implemented: true,
    allowed: false,
    requiresAutoAdapt: true,
    evidence: "none",
    support: "supported",
    reasonCode: "operation_target_stale",
    ...over,
  });

  it("r07_reconnecting_marker_is_display_only", () => {
    const ops: CodexEffectiveOperations = {
      policyRevision: 5,
      capabilityIdentity: "cap-1",
      operations: { async_answer_idle: matched() },
    };
    const { rerender } = render(<CodexOperationToolbar operations={ops} reconnecting />);
    // 全局标记保留，芯片仅展示名称，逐项原因在详情。
    expect(screen.getByTestId("codex-ops-reconnecting")).toBeTruthy();
    expect(screen.getByTestId("codex-ops-chip-async_answer_idle").textContent).toBe(
      i18n.t("mobile.codex.ops.labels.asyncAnswer")
    );
    expect(screen.queryByTestId("codex-ops-reason-async_answer_idle")).toBeNull();

    // 未标记时不出现（不默认谎报恢复中）。
    rerender(<CodexOperationToolbar operations={ops} />);
    expect(screen.queryByTestId("codex-ops-reconnecting")).toBeNull();
  });

  it("c03_identity_pending_marker_is_display_only", () => {
    const ops: CodexEffectiveOperations = {
      policyRevision: 5,
      capabilityIdentity: "cap-1",
      operations: { async_answer_idle: matched() },
    };
    const { rerender } = render(
      <CodexOperationToolbar operations={ops} reconnecting identityPending />
    );
    // 「待核对」与「连接恢复中」并列如实显示；芯片仅显示名称，不参与资格。
    expect(screen.getByTestId("codex-ops-identity-pending")).toBeTruthy();
    expect(screen.getByTestId("codex-ops-reconnecting")).toBeTruthy();
    expect(screen.getByTestId("codex-ops-chip-async_answer_idle").textContent).toBe(
      i18n.t("mobile.codex.ops.labels.asyncAnswer")
    );

    // 核验通过（标记撤下）后不出现（与已确认能力明确区分）。
    rerender(<CodexOperationToolbar operations={ops} reconnecting />);
    expect(screen.queryByTestId("codex-ops-identity-pending")).toBeNull();
  });
});

it("only_capability_names_reasons_stay_in_details", () => {
  const open = vi.fn();
  render(<CodexOperationToolbar operations={operations()} onOpenDetails={open} />);
  const row = screen.getByTestId("codex-ops-row");
  expect(screen.getByTestId("codex-ops-chip-send").textContent).toBe(
    i18n.t("mobile.codex.ops.labels.send")
  );
  expect(row.textContent).not.toContain("已支持");
  expect(row.textContent).not.toContain("回合运行中");
  fireEvent.click(screen.getByTestId("codex-ops-details"));
  expect(open).toHaveBeenCalledTimes(1);
});
