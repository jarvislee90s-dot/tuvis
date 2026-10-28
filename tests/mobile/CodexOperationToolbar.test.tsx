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
