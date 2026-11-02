import { render, screen, within } from "@testing-library/react";
import { beforeAll, afterAll, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import ZcodeLocalQueuePanel from "@/mobile/ZcodeLocalQueuePanel";
import type { LocalQueueView } from "@/lib/api/zcodeControl";
import { pinZhNavigator } from "./pinZhNavigator";

let unpinZh: () => void;
beforeAll(async () => {
  unpinZh = await pinZhNavigator();
});
afterAll(() => unpinZh());

const view = (overrides: Partial<LocalQueueView> = {}): LocalQueueView => ({
  sessionId: "s1",
  version: 4,
  autoDrain: true,
  pauseReason: null,
  items: [
    {
      itemId: "i1",
      itemVersion: 1,
      state: "queued",
      position: 1,
      textPreview: "第一条",
      hasAttachments: false,
    },
    {
      itemId: "i2",
      itemVersion: 1,
      state: "queued",
      position: 2,
      textPreview: "第二条",
      hasAttachments: true,
    },
  ],
  recoverableEdits: [],
  ...overrides,
});

const noop = vi.fn();

describe("ZcodeLocalQueuePanel", () => {
  it("local_queue_has_index_preview_and_three_buttons", () => {
    render(
      <ZcodeLocalQueuePanel
        view={view()}
        busy={false}
        onPrioritize={noop}
        onEdit={noop}
        onRetract={noop}
        onResume={noop}
      />
    );
    const rows = screen.getAllByTestId("zcode-local-item");
    expect(rows).toHaveLength(2);
    expect(within(rows[0]).getByText("第 1 条")).toBeVisible();
    expect(within(rows[0]).getByText("第一条")).toBeVisible();
    expect(within(rows[0]).getByRole("button", { name: "插队" })).toBeEnabled();
    expect(within(rows[0]).getByRole("button", { name: "再编辑" })).toBeEnabled();
    expect(within(rows[0]).getByRole("button", { name: "撤回" })).toBeEnabled();
    expect(within(rows[1]).getByText("第 2 条")).toBeVisible();
  });

  it("paused_queue_shows_resume_and_keeps_all_rows", () => {
    render(
      <ZcodeLocalQueuePanel
        view={view({ autoDrain: false, pauseReason: "manual_stop" })}
        busy={false}
        onPrioritize={noop}
        onEdit={noop}
        onRetract={noop}
        onResume={noop}
      />
    );
    expect(screen.getByText(i18n.t("mobile.zcodeControl.localQueuePaused"))).toBeVisible();
    expect(screen.getAllByTestId("zcode-local-item")).toHaveLength(2);
    expect(
      screen.getByRole("button", { name: i18n.t("mobile.zcodeControl.localQueueResume") })
    ).toBeEnabled();
  });

  it("busy_disables_every_write_button", () => {
    render(
      <ZcodeLocalQueuePanel
        view={view()}
        busy
        onPrioritize={noop}
        onEdit={noop}
        onRetract={noop}
        onResume={noop}
      />
    );
    for (const button of screen.getAllByRole("button")) expect(button).toBeDisabled();
  });

  it("unknown_item_stays_visible_but_is_not_editable", () => {
    render(
      <ZcodeLocalQueuePanel
        view={view({
          items: [
            {
              itemId: "unknown-1",
              itemVersion: 2,
              state: "unknown",
              position: 1,
              textPreview: "停止结果待确认",
              hasAttachments: false,
            },
          ],
        })}
        busy={false}
        onPrioritize={noop}
        onEdit={noop}
        onRetract={noop}
        onResume={noop}
      />
    );
    expect(screen.getByTestId("zcode-local-item")).toBeVisible();
    expect(screen.getByTestId("zcode-local-item-state").getAttribute("data-state")).toBe("unknown");
    expect(screen.getByRole("button", { name: "再编辑" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "撤回" })).toBeDisabled();
  });

  it("recoverable_edit_has_explicit_recover_button", () => {
    render(
      <ZcodeLocalQueuePanel
        view={view({ recoverableEdits: [{ requestId: "edit-1", itemId: "i1", expiresAtMs: 1 }] })}
        busy={false}
        onPrioritize={noop}
        onEdit={noop}
        onRetract={noop}
        onResume={noop}
        onRecoverEdit={noop}
      />
    );
    expect(screen.getByRole("button", { name: "恢复编辑" })).toBeEnabled();
  });
});
