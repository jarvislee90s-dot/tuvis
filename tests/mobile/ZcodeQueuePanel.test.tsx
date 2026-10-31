// W4：ZCode 官方输入队列面板的行为锁（行布局 / 按钮门 / 恢复运行 / 预览）。
//
// 为什么需要它：官方在运行中把新消息放进自己的输入队列。手机端此前只能看，W4 起
// 每条排队项有了「引导 / 修改 / 撤回」三个动作入口，并把「恢复运行」放到了队列头。
// 这些入口全部是**官方写命令**，所以本文件的重点不是「按钮能点」，而是：
// - 未知/空/暂停三态可区分（未知绝不冒充空队列）；
// - 每条**视觉一行**：序号 → 20 字素预览（`title` 全文）→ 引导 → 修改 → 撤回；
// - 「引导」**恒禁用**并给原因（没有「已排队项转引导」的证据，绝不接 queueNow）；
// - 「修改」在带附件/已消费/资格关闭时禁用并给原因；「撤回」不受附件影响；
// - 单项在途只让该项显示「撤回中…」，不重挂其他行；
// - `paused=true` 且 `items=[]` 仍显示「恢复运行」，点击只调 onResume 一次（不自动发送）。
//
// 契约（测试只走用户可见面）：
// - 根节点 `zcode-queue`，`data-state ∈ empty|active|paused|pause-unknown`；
// - 行 `zcode-queue-item-<id>` / `zcode-queue-row-<id>`、序号 `zcode-queue-position-<id>`、
//   预览 `zcode-queue-preview-<id>`、动作 `zcode-queue-{guide,edit,delete}-<id>`；
// - 默认收起（数量 + 暂停 + 恢复运行），点 `zcode-queue-toggle` 展开条目。
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import type { ComponentProps } from "react";
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import ZcodeQueuePanel from "@/mobile/ZcodeQueuePanel";
import { queuePreview } from "@/mobile/queuePreview";
import type { QueueItemView, QueueView } from "@/lib/api/zcodeControl";
import { pinZhNavigator } from "./pinZhNavigator";

let unpinZh: () => void;

beforeAll(async () => {
  unpinZh = await pinZhNavigator();
});

afterAll(() => {
  unpinZh();
});

afterEach(async () => {
  cleanup();
  if (i18n.language !== "zh") await i18n.changeLanguage("zh");
});

function item(overrides: Partial<QueueItemView> & { inputId: string }): QueueItemView {
  return {
    text: "排队正文",
    position: 0,
    kind: "sendText",
    dispatchState: "queued",
    hasAttachments: false,
    ...overrides,
  };
}

function queue(overrides: Partial<QueueView> = {}): QueueView {
  return {
    known: true,
    paused: false,
    items: [],
    availability: { known: true, queueEditAllowed: true, sendQueuedNowAllowed: false },
    ...overrides,
  };
}

/** 面板默认：写面开、资格开（各用例只覆盖自己关心的那一个门）。 */
function renderPanel(props: Partial<ComponentProps<typeof ZcodeQueuePanel>> = {}) {
  return render(<ZcodeQueuePanel queue={queue()} canWrite queueEditAllowed {...props} />);
}

/** 展开条目列表（默认收起）。 */
function expand() {
  fireEvent.click(screen.getByTestId("zcode-queue-toggle"));
}

function row(itemId: string): HTMLElement {
  return screen.getByTestId(`zcode-queue-row-${itemId}`);
}

describe("queuePreview：按字素截断（不是 code point）", () => {
  it("exactly_20_graphemes_is_not_truncated", () => {
    const text = "一二三四五六七八九十一二三四五六七八九十"; // 20 个汉字
    expect(queuePreview(text)).toBe(text);
    expect(queuePreview(text)).not.toContain("…");
  });

  it("21_graphemes_is_truncated_with_ellipsis", () => {
    const text = "一二三四五六七八九十一二三四五六七八九十多"; // 21 个汉字
    expect(queuePreview(text)).toBe(`${"一二三四五六七八九十一二三四五六七八九十"}…`);
  });

  it("emoji_zwj_family_counts_as_one_grapheme", () => {
    // 👨‍👩‍👧‍👦 是 7 个 code point / 1 个字素：code point 截断会把它劈成半截。
    const family = "👨‍👩‍👧‍👦";
    expect(queuePreview(family.repeat(20))).toBe(family.repeat(20));
    expect(queuePreview(family.repeat(21))).toBe(`${family.repeat(20)}…`);
    // 反证：code point 计数会远小于 20 个字素就加省略号。
    expect(Array.from(family)).toHaveLength(7);
  });

  it("combining_marks_stay_attached", () => {
    // "e" + U+0301 是 1 个字素 / 2 个 code point：按 code point 截断会把重音切开。
    const combined = "e\u0301";
    const text = combined.repeat(20);
    expect(queuePreview(text)).toBe(text);
    expect(queuePreview(text)).not.toContain("…");
  });

  it("newlines_and_runs_of_whitespace_collapse_to_one_space", () => {
    expect(queuePreview("第一行\n第二行")).toBe("第一行 第二行");
    expect(queuePreview("  前后留白 \t\t 多余  ")).toBe("前后留白 多余");
    expect(queuePreview("")).toBe("");
  });

  it("limit_is_honored_and_mixed_width_text_is_cut_by_grapheme", () => {
    expect(queuePreview("abcdefghij", 5)).toBe("abcde…");
    const mixed = "修复login问题by检查repo"; // 17 个字素（中英混排）
    expect(queuePreview(mixed)).toBe(mixed);
    expect(queuePreview(`${mixed}再加几个字`, 17)).toBe(`${mixed}…`);
    expect(queuePreview("任意文本", 0)).toBe("");
  });

  it("falls_back_to_code_points_without_intl_segmenter", () => {
    const original = (Intl as unknown as { Segmenter?: unknown }).Segmenter;
    Object.defineProperty(Intl, "Segmenter", { value: undefined, configurable: true });
    try {
      // 回退后按 code point：家庭 emoji（7 个 code point）在 limit=1 时被切碎。
      expect(queuePreview("👨‍👩‍👧‍👦", 1)).toBe("👨…");
      // 组合字符同样被切开（这正是 Segmenter 的用武之地）。
      expect(queuePreview("e\u0301e\u0301", 1)).toBe("e…");
    } finally {
      Object.defineProperty(Intl, "Segmenter", { value: original, configurable: true });
    }
  });
});

describe("ZCode 队列面板：三态可区分", () => {
  it("unknown_queue_never_renders_as_empty", () => {
    // 缺字段（旧载荷）与宿主明确「未宣告」都必须保持未知：既不渲染面板，
    // 也绝不出现「空队列」这类断言性文案。
    const { rerender } = render(<ZcodeQueuePanel queue={undefined} />);
    expect(screen.queryByTestId("zcode-queue")).toBeNull();

    rerender(<ZcodeQueuePanel queue={null} />);
    expect(screen.queryByTestId("zcode-queue")).toBeNull();

    rerender(<ZcodeQueuePanel queue={{ known: false, paused: null, items: [] }} />);
    expect(screen.queryByTestId("zcode-queue")).toBeNull();
    expect(screen.queryByText(/空队列/)).toBeNull();
  });

  it("known_empty_queue_is_distinguishable_but_takes_no_space", () => {
    render(<ZcodeQueuePanel queue={queue({ items: [] })} />);
    const root = screen.getByTestId("zcode-queue");
    // 与「未知」可区分：根节点在，状态是 empty。
    expect(root.dataset.state).toBe("empty");
    // 不占常驻段落：没有任何可见文本。
    expect(root.textContent).toBe("");
  });

  it("collapsed_line_shows_count_and_paused_flag", () => {
    render(
      <ZcodeQueuePanel
        queue={queue({
          paused: true,
          items: [
            item({ inputId: "queue_cmd-1", text: "第一条排队正文", position: 0 }),
            item({ inputId: "queue_cmd-2", text: "第二条排队正文", position: 1 }),
          ],
        })}
      />
    );
    const root = screen.getByTestId("zcode-queue");
    expect(root.dataset.state).toBe("paused");
    expect(screen.getByTestId("zcode-queue-count").textContent).toContain("2");
    expect(screen.getByTestId("zcode-queue-paused").textContent).toBe(
      i18n.t("mobile.zcodeControl.queuePaused")
    );
    // 默认收起：条目行不出现在首屏（紧凑，不抢正文可读性）。
    expect(screen.queryByText("第一条排队正文")).toBeNull();
    expect(screen.getByTestId("zcode-queue-toggle").getAttribute("aria-expanded")).toBe("false");
  });

  it("pause_unknown_is_not_reported_as_paused_or_running", () => {
    render(
      <ZcodeQueuePanel
        queue={queue({ paused: null, items: [item({ inputId: "queue_cmd-3", text: "正文" })] })}
      />
    );
    const root = screen.getByTestId("zcode-queue");
    expect(root.dataset.state).toBe("pause-unknown");
    // paused=None 不得当成「已暂停」，也不得凭空宣称未暂停。
    expect(screen.queryByText(i18n.t("mobile.zcodeControl.queuePaused"))).toBeNull();
    expect(screen.getByTestId("zcode-queue-pause-unknown")).toBeTruthy();
  });
});

describe("ZCode 队列面板：暂停原因与空队列", () => {
  it("paused_empty_queue_still_shows_pause_flag", () => {
    // 官方暂停但队列已被清空（如回合被停止后 drain）：暂停事实仍必须可见，
    // 不能被「0 条」吞成隐藏的空节点。
    render(<ZcodeQueuePanel queue={queue({ paused: true, items: [] })} />);
    const root = screen.getByTestId("zcode-queue");
    expect(root.dataset.state).toBe("paused");
    expect(screen.getByTestId("zcode-queue-paused").textContent).toBe(
      i18n.t("mobile.zcodeControl.queuePaused")
    );
    expect(screen.getByTestId("zcode-queue-count").textContent).toContain("0");
    // 没有条目就没有可展开的列表（展开也只是空列表）：仍可点，但无正文。
    expect(screen.queryByTestId("zcode-queue-items")).toBeNull();
  });

  it("pause_reason_is_rendered_as_readable_text", () => {
    const { rerender } = render(
      <ZcodeQueuePanel queue={queue({ paused: true, pauseReason: "stopped", items: [] })} />
    );
    expect(screen.getByTestId("zcode-queue-pause-reason").textContent).toBe(
      i18n.t("mobile.zcodeControl.queuePauseStopped")
    );

    rerender(<ZcodeQueuePanel queue={queue({ paused: true, pauseReason: "manual", items: [] })} />);
    expect(screen.getByTestId("zcode-queue-pause-reason").textContent).toBe(
      i18n.t("mobile.zcodeControl.queuePauseManual")
    );

    rerender(<ZcodeQueuePanel queue={queue({ paused: true, pauseReason: "error", items: [] })} />);
    expect(screen.getByTestId("zcode-queue-pause-reason").textContent).toBe(
      i18n.t("mobile.zcodeControl.queuePauseError")
    );
  });

  it("unknown_pause_reason_is_not_guessed", () => {
    // 非枚举值：仍然确认「已暂停」，但绝不把未知值编成某一种原因。
    const { rerender } = render(
      <ZcodeQueuePanel queue={queue({ paused: true, pauseReason: "suspended", items: [] })} />
    );
    expect(screen.getByTestId("zcode-queue-paused")).toBeTruthy();
    expect(screen.queryByTestId("zcode-queue-pause-reason")).toBeNull();

    // 未暂停时残留的原因值也不得渲染（不给「暂停」假象）。
    rerender(
      <ZcodeQueuePanel
        queue={queue({
          paused: false,
          pauseReason: "stopped",
          items: [item({ inputId: "queue_cmd-p", text: "正文" })],
        })}
      />
    );
    expect(screen.getByTestId("zcode-queue").dataset.state).toBe("active");
    expect(screen.queryByTestId("zcode-queue-pause-reason")).toBeNull();
  });

  it("pause_reason_is_localized_in_english", async () => {
    await i18n.changeLanguage("en");
    render(<ZcodeQueuePanel queue={queue({ paused: true, pauseReason: "error", items: [] })} />);
    expect(screen.getByTestId("zcode-queue-pause-reason").textContent).toBe(
      i18n.t("mobile.zcodeControl.queuePauseError")
    );
    expect(i18n.t("mobile.zcodeControl.queuePauseError")).toMatch(/error/i);
  });
});

describe("ZCode 队列面板：行布局（§5.1）", () => {
  it("three_items_render_one_visual_row_each_with_guide_edit_delete_in_order", () => {
    renderPanel({
      queue: queue({
        items: [
          item({
            inputId: "queue_cmd-a",
            text: "优先把登录的问题修复一下，然后再补齐回归测试用例",
            position: 0,
          }),
          item({ inputId: "queue_cmd-b", text: "请继续检查仓库里面还有没有别的问题", position: 1 }),
          item({ inputId: "queue_cmd-c", text: "测试结束后给出修复报告", position: 2 }),
        ],
      }),
    });
    expand();

    for (const id of ["queue_cmd-a", "queue_cmd-b", "queue_cmd-c"]) {
      const buttons = within(row(id)).getAllByRole("button");
      // 序号 + 三个动作：一行的动作顺序固定（引导 → 修改 → 撤回）。
      expect(buttons.map((button) => button.textContent)).toEqual([
        id === "queue_cmd-a" ? "#1" : id === "queue_cmd-b" ? "#2" : "#3",
        i18n.t("mobile.zcodeControl.queueGuide"),
        i18n.t("mobile.zcodeControl.queueEdit"),
        i18n.t("mobile.zcodeControl.queueDelete"),
      ]);
    }
  });

  it("preview_keeps_20_graphemes_and_full_text_lives_in_title", () => {
    const long = "优先把登录的问题修复一下，然后补齐回归测试用例并且再跑一遍全量检查";
    renderPanel({
      queue: queue({ items: [item({ inputId: "queue_cmd-long", text: long, position: 0 })] }),
    });
    expand();
    const preview = screen.getByTestId("zcode-queue-preview-queue_cmd-long");
    expect(preview.textContent).toBe(queuePreview(long));
    expect(preview.textContent).toBe(`${Array.from(long).slice(0, 20).join("")}…`);
    // 全文只在 title 里：预览绝不作为任何载荷（面板本身不发请求）。
    expect(preview.getAttribute("title")).toBe(long);
  });

  it("missing_position_shows_dash_and_never_invents_a_number", () => {
    renderPanel({
      queue: queue({
        items: [
          item({ inputId: "queue_cmd-a", text: "第一条", position: 0 }),
          item({ inputId: "queue_cmd-b", text: "第二条", position: null }),
        ],
      }),
    });
    expand();
    expect(screen.getByTestId("zcode-queue-position-queue_cmd-a").textContent).toBe("#1");
    const position = screen.getByTestId("zcode-queue-position-queue_cmd-b").textContent;
    expect(position).toBe("—");
    expect(position).not.toMatch(/#/);
  });
});

describe("ZCode 队列面板：按钮门（§5.2）", () => {
  it("guide_is_always_disabled_with_a_readable_reason_and_fires_nothing", () => {
    renderPanel({
      queue: queue({ items: [item({ inputId: "queue_cmd-a", text: "正文" })] }),
    });
    expand();
    const guide = screen.getByTestId("zcode-queue-guide-queue_cmd-a") as HTMLButtonElement;
    expect(guide).toBeDisabled();
    expect(guide.getAttribute("title")).toBe(i18n.t("mobile.zcodeControl.queueGuideReason"));
    // 原因在列表上方常驻可见（不是只藏在 title 里）。
    expect(screen.getByTestId("zcode-queue-guide-reason").textContent).toBe(
      i18n.t("mobile.zcodeControl.queueGuideReason")
    );
    fireEvent.click(guide);
    // 恒禁用 = 任何点击都不产生动作（没有 onGuide 通道，更不可能接 queueNow）。
    expect(screen.queryByTestId("zcode-queue-notice")).toBeNull();
  });

  it("edit_and_delete_are_disabled_when_host_forbids_queue_edit", () => {
    const onEdit = vi.fn();
    const onDelete = vi.fn();
    renderPanel({
      queue: queue({ items: [item({ inputId: "queue_cmd-a", text: "正文" })] }),
      queueEditAllowed: false,
      onEditToDraft: onEdit,
      onDelete: onDelete,
    });
    expand();
    const edit = screen.getByTestId("zcode-queue-edit-queue_cmd-a") as HTMLButtonElement;
    const del = screen.getByTestId("zcode-queue-delete-queue_cmd-a") as HTMLButtonElement;
    expect(edit).toBeDisabled();
    expect(del).toBeDisabled();
    expect(edit.getAttribute("title")).toBe(
      i18n.t("mobile.zcodeControl.capabilityQueueEditClosed")
    );
    fireEvent.click(edit);
    fireEvent.click(del);
    expect(onEdit).not.toHaveBeenCalled();
    expect(onDelete).not.toHaveBeenCalled();
  });

  it("attachment_item_keeps_delete_but_disables_edit_with_reason", () => {
    const onEdit = vi.fn();
    const onDelete = vi.fn();
    renderPanel({
      queue: queue({
        items: [item({ inputId: "queue_cmd-file", text: "带附件的排队项", hasAttachments: true })],
      }),
      onEditToDraft: onEdit,
      onDelete: onDelete,
    });
    expand();
    const edit = screen.getByTestId("zcode-queue-edit-queue_cmd-file") as HTMLButtonElement;
    const del = screen.getByTestId("zcode-queue-delete-queue_cmd-file") as HTMLButtonElement;
    expect(edit).toBeDisabled();
    expect(edit.getAttribute("title")).toBe(
      i18n.t("mobile.zcodeControl.queueEditAttachmentsReason")
    );
    expect(screen.getByTestId("zcode-queue-item-reason-queue_cmd-file").textContent).toBe(
      i18n.t("mobile.zcodeControl.queueEditAttachmentsReason")
    );
    // 撤回不受附件影响：只恢复文字不等于丢附件。
    expect(del).toBeEnabled();
    fireEvent.click(del);
    expect(onDelete).toHaveBeenCalledTimes(1);
    expect(onEdit).not.toHaveBeenCalled();
  });

  it("already_dispatched_items_are_not_writable", () => {
    for (const dispatchState of ["promoting", "drained", null]) {
      cleanup();
      renderPanel({
        queue: queue({
          items: [item({ inputId: "queue_cmd-run", text: "正在执行", dispatchState })],
        }),
      });
      expand();
      expect(screen.getByTestId("zcode-queue-edit-queue_cmd-run")).toBeDisabled();
      expect(screen.getByTestId("zcode-queue-delete-queue_cmd-run")).toBeDisabled();
      expect(screen.getByTestId("zcode-queue-item-reason-queue_cmd-run").textContent).toBe(
        i18n.t("mobile.zcodeControl.queueItemConsumedReason")
      );
    }
  });

  it("busy_item_shows_only_its_own_progress_and_blocks_other_rows", () => {
    renderPanel({
      busyItemId: "queue_cmd-a",
      queue: queue({
        items: [
          item({ inputId: "queue_cmd-a", text: "第一条", position: 0 }),
          item({ inputId: "queue_cmd-b", text: "第二条", position: 1 }),
        ],
      }),
    });
    expand();
    // 只有该项进入「撤回中…」loading；其他行的按钮文字保持原样（不重挂、不 loading）。
    expect(screen.getByTestId("zcode-queue-delete-queue_cmd-a").textContent).toBe(
      i18n.t("mobile.zcodeControl.queueDeleting")
    );
    expect(screen.getByTestId("zcode-queue-delete-queue_cmd-b").textContent).toBe(
      i18n.t("mobile.zcodeControl.queueDelete")
    );
    expect(screen.getByTestId("zcode-queue-item-queue_cmd-a").dataset.busy).toBe("true");
    expect(screen.getByTestId("zcode-queue-item-queue_cmd-b").dataset.busy).toBe("false");
    // 单写槽：在途期间其他动作一律禁用（绝不产生第二个写命令）。
    expect(screen.getByTestId("zcode-queue-delete-queue_cmd-b")).toBeDisabled();
    expect(screen.getByTestId("zcode-queue-edit-queue_cmd-b")).toBeDisabled();
  });

  it("move_menu_uses_explicit_target_ids_and_never_null", () => {
    const onMove = vi.fn();
    renderPanel({
      onMove,
      queue: queue({
        items: [
          item({ inputId: "queue_cmd-a", text: "第一条", position: 0 }),
          item({ inputId: "queue_cmd-b", text: "第二条", position: 1 }),
          item({ inputId: "queue_cmd-c", text: "第三条", position: 2 }),
        ],
      }),
    });
    expand();

    // 第二项 → 移到最前：显式目标 = 第一项 ID（不是 position=0）。
    fireEvent.click(screen.getByTestId("zcode-queue-position-queue_cmd-b"));
    fireEvent.click(screen.getByTestId("zcode-queue-move-first-queue_cmd-b"));
    expect(onMove).toHaveBeenLastCalledWith("queue_cmd-b", "queue_cmd-a");

    // 第二项 → 下移：放到第三项之后需要「下一项的下一个」目标；没有目标即禁用。
    fireEvent.click(screen.getByTestId("zcode-queue-position-queue_cmd-b"));
    expect(screen.getByTestId("zcode-queue-move-down-queue_cmd-b")).toBeDisabled();
    // 第一项已在最前：移到最前/上移都禁用。
    fireEvent.click(screen.getByTestId("zcode-queue-position-queue_cmd-a"));
    expect(screen.getByTestId("zcode-queue-move-first-queue_cmd-a")).toBeDisabled();
    expect(screen.getByTestId("zcode-queue-move-up-queue_cmd-a")).toBeDisabled();
  });
});

describe("ZCode 队列面板：恢复运行（§5.5）", () => {
  it("paused_with_empty_items_still_offers_resume", () => {
    const onResume = vi.fn();
    renderPanel({
      queue: queue({ paused: true, items: [] }),
      queueResumeAllowed: true,
      onResume,
    });
    const resume = screen.getByTestId("zcode-queue-resume") as HTMLButtonElement;
    expect(resume.textContent).toBe(i18n.t("mobile.zcodeControl.queueResume"));
    expect(resume).toBeEnabled();
    // 打开面板本身绝不自动恢复。
    expect(onResume).not.toHaveBeenCalled();
    fireEvent.click(resume);
    expect(onResume).toHaveBeenCalledTimes(1);
  });

  it("resume_is_disabled_without_capability_or_write_gate", () => {
    const onResume = vi.fn();
    renderPanel({ queue: queue({ paused: true, items: [] }), onResume });
    expect(screen.getByTestId("zcode-queue-resume")).toBeDisabled();
    cleanup();
    renderPanel({
      queue: queue({ paused: true, items: [] }),
      canWrite: false,
      queueResumeAllowed: true,
      onResume,
    });
    expect(screen.getByTestId("zcode-queue-resume")).toBeDisabled();
    expect(onResume).not.toHaveBeenCalled();
  });

  it("resume_done_appears_only_after_a_fresh_unpaused_queue", () => {
    const { rerender } = renderPanel({
      queue: queue({ paused: true, items: [] }),
      queueResumeAllowed: true,
      busyResume: true,
    });
    expect(screen.getByTestId("zcode-queue-resume").textContent).toBe(
      i18n.t("mobile.zcodeControl.queueResuming")
    );
    expect(screen.queryByTestId("zcode-queue-resume-done")).toBeNull();

    // ACK 阶段：仍暂停 → 绝不显示「已恢复」。
    rerender(
      <ZcodeQueuePanel
        queue={queue({ paused: true, items: [] })}
        canWrite
        queueEditAllowed
        queueResumeAllowed
        resumeConfirmed={false}
      />
    );
    expect(screen.queryByTestId("zcode-queue-resume-done")).toBeNull();

    // 新鲜队列 paused=false 才允许说「已恢复」。
    rerender(
      <ZcodeQueuePanel
        queue={queue({ paused: false, items: [] })}
        canWrite
        queueEditAllowed
        queueResumeAllowed
        resumeConfirmed
      />
    );
    expect(screen.getByTestId("zcode-queue-resume-done").textContent).toBe(
      i18n.t("mobile.zcodeControl.queueResumeDone")
    );
    // 未暂停时不再显示恢复入口（没有可恢复的暂停态）。
    expect(screen.queryByTestId("zcode-queue-resume")).toBeNull();
  });
});
