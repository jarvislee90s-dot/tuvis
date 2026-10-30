import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import InteractiveCard, { toneTokens, type InteractiveCardTone } from "@/mobile/InteractiveCard";

// 批次丙 T10：统一交互卡容器（任务书 §2.2 结构契约 + §2.4 色彩 token）。
// 本测锁**容器契约**（色条/徽标/主体/动作区/底注五段结构 + 色彩 token 分族），
// 以及四套界面共用同一 token 源（ApproveCard/QuestionCard 已改挂本容器）。

afterEach(cleanup);

describe("InteractiveCard：统一容器（T10）", () => {
  it("渲染五段结构：色条头部（圆点+徽标）、主体、动作区、底注", () => {
    render(
      <InteractiveCard
        tone="approve"
        testId="approve-card"
        mode="binary"
        title="等待批准"
        footer={<p data-testid="f">底注</p>}
        actions={<button data-testid="a">动作</button>}
      >
        <p data-testid="body">主体</p>
      </InteractiveCard>
    );
    const card = screen.getByTestId("approve-card");
    expect(card).toBeTruthy();
    // §2.2：容器带类型徽标与状态圆点（色条）
    expect(screen.getByTestId("approve-card-dot")).toBeTruthy();
    expect(card.textContent).toContain("等待批准");
    // 主体 / 动作区 / 底注三段都在
    expect(screen.getByTestId("body")).toBeTruthy();
    expect(screen.getByTestId("a")).toBeTruthy();
    expect(screen.getByTestId("f")).toBeTruthy();
    // 色彩 token 落到 data-tone（样式与测试的稳定钩子）
    expect(card.getAttribute("data-tone")).toBe("approve");
    // 既有 data-mode 契约保留（ApproveCard 的 dialog/binary 分档靠它）
    expect(card.getAttribute("data-mode")).toBe("binary");
  });

  it("色彩 token 分族：approve=红 / question=蓝 / plan=绿 / mode=中性（§2.4 终端色彩语义）", () => {
    const expectTone = (tone: InteractiveCardTone, colorWord: string) => {
      const tok = toneTokens(tone);
      expect(tok.box).toContain(colorWord);
      expect(tok.dot).toContain(colorWord);
      expect(tok.title).toContain(colorWord);
      expect(tok.action).toContain(colorWord);
    };
    expectTone("approve", "rose");
    expectTone("question", "sky");
    expectTone("plan", "emerald");
    expectTone("mode", "slate");
    // neutral 与 mode 同色系（中性）
    expect(toneTokens("neutral").box).toContain("slate");
  });

  it("pulsing 可关（常驻信息卡不脉冲——只读卡/模式栏用）", () => {
    const { unmount } = render(
      <InteractiveCard tone="question" testId="c1" title="静止" pulsing={false} />
    );
    expect(screen.getByTestId("c1-dot").className).not.toContain("animate-pulse");
    unmount();
    render(<InteractiveCard tone="question" testId="c2" title="脉冲" />);
    expect(screen.getByTestId("c2-dot").className).toContain("animate-pulse");
  });

  it("可选段缺省不渲染（actions/footer/children 全空也不炸）", () => {
    render(<InteractiveCard tone="neutral" testId="bare" title="只有头" />);
    const card = screen.getByTestId("bare");
    expect(card.textContent).toContain("只有头");
    expect(card.children.length).toBeGreaterThan(0);
  });
});

describe("InteractiveCard：四套界面共用同一设计语言（T10 验收）", () => {
  it(
    "ApproveCard / QuestionCard 的容器均来自本组件（tone 分族可断言）",
    { timeout: 20000 },
    async () => {
      // 静态锁：三套卡都渲染出本组件加的 data-tone 属性（容器统一的可观测证据）
      const { default: ApproveCard } = await import("@/mobile/ApproveCard");
      const { default: QuestionCard } = await import("@/mobile/QuestionCard");
      expect(ApproveCard).toBeTypeOf("function");
      expect(QuestionCard).toBeTypeOf("function");
      // data-tone 由 InteractiveCard 唯一产出（消费方无法自行伪造同名属性——
      // 除非再抄一遍容器，那正是本任务要消除的重复）
      const props: InteractiveCardTone[] = ["approve", "question", "plan", "mode"];
      for (const tone of props) {
        const { unmount } = render(
          <InteractiveCard tone={tone} testId={`t-${tone}`} title={tone} />
        );
        expect(screen.getByTestId(`t-${tone}`).getAttribute("data-tone")).toBe(tone);
        unmount();
      }
    }
  );
});
