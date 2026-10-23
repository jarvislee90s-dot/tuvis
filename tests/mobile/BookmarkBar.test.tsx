// M3+ 消息窗口书签条（2026-09-16 用户裁决）：消息区上方常驻横条——
// 左侧「+ 书签」（弹 10 色调色板，已占用色置灰）、右侧一排色点（点即跳转）、
// 最右「管理」钮（编辑态：每个色点显示 ×、「清空全部」出现，再点「完成」退出）。
// 纯展示组件：书签数据由 SessionDetail 从 store 取，增删查全部经回调上抛。
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import BookmarkBar from "@/mobile/BookmarkBar";
import { BOOKMARK_COLORS, type Bookmark } from "@/mobile/bookmarks";

// aria-label 等文案走 mobile.bookmark.* 双语键后按 zh 断言（QuestionCard.test 同款：
// 显式引入并固定中文）
import i18n from "@/i18n";

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

afterEach(cleanup);

function bm(over: Partial<Bookmark> = {}): Bookmark {
  return {
    color: BOOKMARK_COLORS[0],
    seq: 1,
    anchor: "user|1000|3|abc",
    preview: "第一条消息",
    ...over,
  };
}

function renderBar(bookmarks: Bookmark[] = [], over: { full?: boolean } = {}) {
  const onAdd = vi.fn();
  const onJump = vi.fn();
  const onRemove = vi.fn();
  const onClear = vi.fn();
  render(
    <BookmarkBar
      bookmarks={bookmarks}
      atLimit={over.full ?? bookmarks.length >= 10}
      onAdd={onAdd}
      onJump={onJump}
      onRemove={onRemove}
      onClear={onClear}
    />
  );
  return { onAdd, onJump, onRemove, onClear };
}

describe("BookmarkBar 渲染", () => {
  it("无书签时只有「+ 书签」与管理钮（无色点）", () => {
    renderBar([]);
    expect(screen.getByTestId("bookmark-add")).toBeTruthy();
    expect(screen.getByTestId("bookmark-manage")).toBeTruthy();
    expect(screen.queryAllByTestId(/^bookmark-dot-/)).toHaveLength(0);
  });

  it("有书签时逐色点渲染（颜色即 testid 键）", () => {
    renderBar([bm({ color: BOOKMARK_COLORS[0] }), bm({ color: BOOKMARK_COLORS[3] })]);
    expect(screen.getAllByTestId(/^bookmark-dot-/)).toHaveLength(2);
    expect(screen.getByTestId(`bookmark-dot-${BOOKMARK_COLORS[0]}`)).toBeTruthy();
    expect(screen.getByTestId(`bookmark-dot-${BOOKMARK_COLORS[3]}`)).toBeTruthy();
  });
});

describe("BookmarkBar 打标签（颜色选择）", () => {
  it("点 + 弹调色板：10 色全列", () => {
    renderBar([]);
    expect(screen.queryByTestId("bookmark-palette")).toBeNull();
    fireEvent.click(screen.getByTestId("bookmark-add"));
    const palette = screen.getByTestId("bookmark-palette");
    expect(within(palette).getAllByRole("button")).toHaveLength(BOOKMARK_COLORS.length);
    for (const c of BOOKMARK_COLORS) {
      expect(within(palette).getByTestId(`bookmark-color-${c}`)).toBeTruthy();
    }
  });

  it("已占用色在调色板中置灰不可点", () => {
    const used = BOOKMARK_COLORS[2];
    renderBar([bm({ color: used })]);
    fireEvent.click(screen.getByTestId("bookmark-add"));
    const usedBtn = screen.getByTestId(`bookmark-color-${used}`) as HTMLButtonElement;
    expect(usedBtn.disabled).toBe(true);
    // 未占用色可点
    const freeBtn = screen.getByTestId(`bookmark-color-${BOOKMARK_COLORS[5]}`) as HTMLButtonElement;
    expect(freeBtn.disabled).toBe(false);
  });

  it("选色 → onAdd(color)，调色板收起", () => {
    const { onAdd } = renderBar([]);
    fireEvent.click(screen.getByTestId("bookmark-add"));
    fireEvent.click(screen.getByTestId(`bookmark-color-${BOOKMARK_COLORS[4]}`));
    expect(onAdd).toHaveBeenCalledWith(BOOKMARK_COLORS[4]);
    expect(screen.queryByTestId("bookmark-palette")).toBeNull();
  });

  it("10 个满 → + 按钮 disabled", () => {
    renderBar(
      BOOKMARK_COLORS.map((c, i) => bm({ color: c, seq: i, anchor: `a${i}` })),
      { full: true }
    );
    expect((screen.getByTestId("bookmark-add") as HTMLButtonElement).disabled).toBe(true);
  });
});

describe("BookmarkBar 跳转与删除", () => {
  it("常态点色点 → onJump(anchor)", () => {
    const target = bm({ color: BOOKMARK_COLORS[1], anchor: "assistant|2000|5|hello" });
    const { onJump } = renderBar([target]);
    fireEvent.click(screen.getByTestId(`bookmark-dot-${BOOKMARK_COLORS[1]}`));
    expect(onJump).toHaveBeenCalledWith("assistant|2000|5|hello");
  });

  it("管理态：色点变删除钮（×），点击 → onRemove(color)；再点完成退出", () => {
    const { onRemove } = renderBar([
      bm({ color: BOOKMARK_COLORS[0] }),
      bm({ color: BOOKMARK_COLORS[6], anchor: "a6" }),
    ]);
    // 进入管理态
    fireEvent.click(screen.getByTestId("bookmark-manage"));
    expect(screen.getByTestId(`bookmark-remove-${BOOKMARK_COLORS[0]}`)).toBeTruthy();
    expect(screen.queryByTestId(`bookmark-dot-${BOOKMARK_COLORS[0]}`)).toBeNull();
    fireEvent.click(screen.getByTestId(`bookmark-remove-${BOOKMARK_COLORS[6]}`));
    expect(onRemove).toHaveBeenCalledWith(BOOKMARK_COLORS[6]);
    // 退出管理态：色点回来
    fireEvent.click(screen.getByTestId("bookmark-manage-done"));
    expect(screen.getByTestId(`bookmark-dot-${BOOKMARK_COLORS[0]}`)).toBeTruthy();
  });

  it("管理态才有「清空全部」；点击 → onClear()", () => {
    const { onClear } = renderBar([bm({ color: BOOKMARK_COLORS[0] })]);
    expect(screen.queryByTestId("bookmark-clear-all")).toBeNull();
    fireEvent.click(screen.getByTestId("bookmark-manage"));
    fireEvent.click(screen.getByTestId("bookmark-clear-all"));
    expect(onClear).toHaveBeenCalled();
  });

  it("无书签时管理态不给「清空全部」（避免空操作）", () => {
    renderBar([]);
    fireEvent.click(screen.getByTestId("bookmark-manage"));
    expect(screen.queryByTestId("bookmark-clear-all")).toBeNull();
  });

  it("色点带 title 摘要（preview），便于识别", () => {
    renderBar([bm({ color: BOOKMARK_COLORS[0], preview: "设计意图那一段" })]);
    const dot = screen.getByTestId(`bookmark-dot-${BOOKMARK_COLORS[0]}`);
    expect(dot.getAttribute("title")).toContain("设计意图那一段");
  });
});

// 功能区块聚集 + 过程折叠开关（2026-09-20 用户裁决）
describe("BookmarkBar 功能聚集重排与过程折叠（2026-09-20）", () => {
  it("管理钮紧挨「书签」新增钮（功能区块聚集，不再被色点区隔到最右端）", () => {
    const { container } = render(
      <BookmarkBar
        bookmarks={[bm()]}
        atLimit={false}
        onAdd={vi.fn()}
        onJump={vi.fn()}
        onRemove={vi.fn()}
        onClear={vi.fn()}
      />
    );
    const add = screen.getByTestId("bookmark-add");
    const manage = screen.getByTestId("bookmark-manage");
    // 顺序断言：add 的下一个兄弟就是 manage（回归锁：防再被挪远）
    expect(add.nextElementSibling).toBe(manage);
    // 色点区在 manage 之后
    const chips = container.querySelector("span.flex.min-w-0");
    expect(manage.compareDocumentPosition(chips!) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  it("processToggle：有可折叠过程时渲染开关；allCollapsed 决定图标语义与动作", () => {
    const onToggle = vi.fn();
    const { rerender } = render(
      <BookmarkBar
        bookmarks={[]}
        atLimit={false}
        onAdd={vi.fn()}
        onJump={vi.fn()}
        onRemove={vi.fn()}
        onClear={vi.fn()}
        processToggle={{ allCollapsed: true, onToggle }}
      />
    );
    const toggle = screen.getByTestId("process-collapse-toggle");
    expect(toggle.getAttribute("aria-label")).toBe("展开全部过程");
    fireEvent.click(toggle);
    expect(onToggle).toHaveBeenCalledTimes(1);

    // 全部已展开 → 动作翻转为全部折叠
    rerender(
      <BookmarkBar
        bookmarks={[]}
        atLimit={false}
        onAdd={vi.fn()}
        onJump={vi.fn()}
        onRemove={vi.fn()}
        onClear={vi.fn()}
        processToggle={{ allCollapsed: false, onToggle }}
      />
    );
    expect(screen.getByTestId("process-collapse-toggle").getAttribute("aria-label")).toBe(
      "折叠全部过程"
    );
  });

  it("缺省（无 processToggle）：开关不渲染，既有布局不受影响", () => {
    renderBar([]);
    expect(screen.queryByTestId("process-collapse-toggle")).toBeNull();
  });
});
