// R2：ZCode 官方输入队列的**只读**紧凑面板（手机）行为锁。
//
// 为什么需要它：官方在运行中把新消息放进自己的输入队列（桌面端能看到队列与
// 「已暂停」），手机端此前完全看不到。本面板只做投影展示，**不**建自建队列、
// 不提供插队/撤回/立即发送入口（那些是官方写命令，本任务不做）。
//
// 契约（测试只走用户可见面）：
// - 根节点 `data-testid="zcode-queue"`，`data-state` ∈
//   `empty | active | paused | pause-unknown`。
// - `known !== true`（旧载荷缺字段/宿主未宣告）→ **不渲染**：未知绝不冒充「空队列」。
// - `known=true` 且 `items=[]` → 保留根节点（`data-state="empty"`）但**无可见文本**：
//   与「未知」可区分，又不给每个会话常驻一段空文案。
// - 默认一行：数量 + 暂停标志；点 `zcode-queue-toggle` 才展开正文列表
//   （`aria-expanded` 同步）；正文原样展示，不截断语义。
// - `paused === null`（宿主未给 `autoDrain`）→ 显示「暂停状态未知」，绝不写成
//   「已暂停」，也绝不写成「运行中」。
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import i18n from "@/i18n";
import ZcodeQueuePanel from "@/mobile/ZcodeQueuePanel";
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

function item(inputId: string, text: string, position: number | null = null): QueueItemView {
  return { inputId, text, position };
}

function queue(overrides: Partial<QueueView> = {}): QueueView {
  return { known: true, paused: false, items: [], ...overrides };
}

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
            item("queue_cmd-1", "第一条排队正文", 0),
            item("queue_cmd-2", "第二条排队正文", 1),
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
    // 默认收起：正文不出现在首屏（紧凑，不抢正文可读性）。
    expect(screen.queryByText("第一条排队正文")).toBeNull();
    expect(screen.getByTestId("zcode-queue-toggle").getAttribute("aria-expanded")).toBe("false");
  });

  it("pause_unknown_is_not_reported_as_paused_or_running", () => {
    render(
      <ZcodeQueuePanel queue={queue({ paused: null, items: [item("queue_cmd-3", "正文", 0)] })} />
    );
    const root = screen.getByTestId("zcode-queue");
    expect(root.dataset.state).toBe("pause-unknown");
    // paused=None 不得当成「已暂停」，也不得凭空宣称未暂停。
    expect(screen.queryByText(i18n.t("mobile.zcodeControl.queuePaused"))).toBeNull();
    expect(screen.getByTestId("zcode-queue-pause-unknown")).toBeTruthy();
  });
});

describe("ZCode 队列面板：展开只读列表", () => {
  it("expand_and_collapse_reveal_host_text_and_position", () => {
    render(
      <ZcodeQueuePanel
        queue={queue({
          items: [
            item("queue_cmd-a", "第一条排队正文", 0),
            item("queue_cmd-b", "第二条排队正文", null),
          ],
        })}
      />
    );
    const toggle = screen.getByTestId("zcode-queue-toggle");
    expect(screen.queryByText("第一条排队正文")).toBeNull();

    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByText("第一条排队正文")).toBeTruthy();
    expect(screen.getByText("第二条排队正文")).toBeTruthy();
    // 位置只在宿主给了 `order.queuePosition` 时展示（0 基 → 展示 1 基），
    // 缺席 = 不猜位置。
    expect(screen.getByTestId("zcode-queue-position-queue_cmd-a").textContent).toBe("#1");
    expect(screen.queryByTestId("zcode-queue-position-queue_cmd-b")).toBeNull();

    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByText("第一条排队正文")).toBeNull();
  });

  it("expanded_list_has_no_write_entry", () => {
    // 只读：展开后除了收起开关，不存在任何按钮（插队/撤回/立即发送都是官方写命令，
    // 本任务不做，面板也不得给出假出口）。
    render(<ZcodeQueuePanel queue={queue({ items: [item("queue_cmd-c", "正文", 0)] })} />);
    fireEvent.click(screen.getByTestId("zcode-queue-toggle"));
    const root = screen.getByTestId("zcode-queue");
    expect(root.querySelectorAll("button")).toHaveLength(1);
  });
});
