import type { ReactNode } from "react";

/** 统一交互卡容器（批次丙 T10，按任务书 §2 基准）。
 *
 * 任务书 §1 问题 11：审批界面各家风格不一（红卡/问答卡/计划卡三套组件三种交互）。
 * §2.2 的基准是**单一「交互卡」容器**——所有「等待用户输入」场景渲染同一容器，
 * 差异只在**主体**与**动作区**数据：
 *
 * ```
 * ┌─ 状态色条 + 类型徽标 + 工具名/来源 ──┐
 * │  主体 BODY                            │
 * │  动作区 ACTIONS：编号按钮网格          │
 * │  底注 FOOTER：键位提示 / 分诊文案       │
 * └───────────────────────────────────────┘
 * ```
 *
 * §2.4 的视觉 token（沿用终端内色彩语义）：等待批准=红 / 问答=蓝 / 计划确认=绿。
 *
 * 本组件只承载**结构**（色条 + 徽标 + 主体 + 动作区 + 底注的排版契约），
 * 卡片各自的业务逻辑（数据拉取、按钮语义、注入动作）留在 ApproveCard /
 * QuestionCard 等消费方——§2.5「不做」：不把消息流卡片改造成交互卡（交互只发生在
 * 底部交互卡，消息流只读）。
 */

/** 卡类型（决定色彩 token 与徽标——§2.4 的终端内色彩语义） */
export type InteractiveCardTone = "approve" | "question" | "plan" | "mode" | "neutral";

/** 色彩 token（§2.4：等待批准=红、问答=蓝、计划确认=绿；mode/neutral 用中性色）
 *  每条含：边框/底色（卡容器）、徽标圆点、标题文字、按钮底/字——四件套。 */
const TONES: Record<
  InteractiveCardTone,
  {
    /** 卡容器（含边框与浅底） */
    box: string;
    /** 状态圆点（脉冲） */
    dot: string;
    /** 标题文字 */
    title: string;
    /** 动作区按钮（中性态；主行动/危险态由消费方在 tone 色上叠加强调） */
    action: string;
    /** 选项序号徽标 */
    badge: string;
  }
> = {
  approve: {
    box: "rounded-xl border-2 border-rose-500/60 bg-rose-500/5 dark:border-rose-400/60 dark:bg-rose-400/5",
    dot: "bg-rose-500",
    title: "text-rose-700 dark:text-rose-400",
    action:
      "bg-rose-500/10 text-rose-700 hover:bg-rose-500/20 dark:bg-rose-400/10 dark:text-rose-300 dark:hover:bg-rose-400/20",
    badge: "bg-rose-500/20 dark:bg-rose-400/20",
  },
  question: {
    box: "rounded-xl border-2 border-sky-500/60 bg-sky-500/5 dark:border-sky-400/60 dark:bg-sky-400/5",
    dot: "bg-sky-500",
    title: "text-sky-700 dark:text-sky-400",
    action:
      "bg-sky-500/10 text-sky-700 hover:bg-sky-500/20 dark:bg-sky-400/10 dark:text-sky-300 dark:hover:bg-sky-400/20",
    badge: "bg-sky-500/20 dark:bg-sky-400/20",
  },
  plan: {
    box: "rounded-xl border-2 border-emerald-500/60 bg-emerald-500/5 dark:border-emerald-400/60 dark:bg-emerald-400/5",
    dot: "bg-emerald-500",
    title: "text-emerald-700 dark:text-emerald-400",
    action:
      "bg-emerald-500/10 text-emerald-700 hover:bg-emerald-500/20 dark:bg-emerald-400/10 dark:text-emerald-300 dark:hover:bg-emerald-400/20",
    badge: "bg-emerald-500/20 dark:bg-emerald-400/20",
  },
  mode: {
    box: "rounded-lg border border-slate-300/60 bg-slate-100/60 dark:border-slate-600/60 dark:bg-slate-800/40",
    dot: "bg-slate-400",
    title: "text-slate-700 dark:text-slate-300",
    action:
      "bg-slate-500/15 text-slate-700 hover:bg-slate-500/25 dark:bg-slate-400/15 dark:text-slate-300 dark:hover:bg-slate-400/25",
    badge: "bg-slate-500/20 dark:bg-slate-400/20",
  },
  neutral: {
    box: "rounded-xl border border-slate-300/60 bg-slate-100/60 dark:border-slate-600/60 dark:bg-slate-800/40",
    dot: "bg-slate-400",
    title: "text-slate-700 dark:text-slate-300",
    action:
      "bg-slate-500/15 text-slate-700 hover:bg-slate-500/25 dark:bg-slate-400/15 dark:text-slate-300 dark:hover:bg-slate-400/25",
    badge: "bg-slate-500/20 dark:bg-slate-400/20",
  },
};

/** 取色 token（消费方在按钮/徽标上叠用，保证四套卡同色系） */
export function toneTokens(tone: InteractiveCardTone) {
  return TONES[tone];
}

export interface InteractiveCardProps {
  /** 色彩/语义档（§2.4 token） */
  tone: InteractiveCardTone;
  /** 卡类型标识（data-mode；测试与样式钩子） */
  mode?: string;
  /** 卡根 data-testid（既有测试面依赖 approve-card / question-card 等具体名字） */
  testId: string;
  /** 徽标右侧标题（如「等待批准」「等待回答」） */
  title: ReactNode;
  /** 标题右侧附加（如模式栏的当前档文本） */
  titleSuffix?: ReactNode;
  /** 是否显示脉冲圆点（常驻信息卡不需要） */
  pulsing?: boolean;
  /** 主体区（题干 / 计划全文 / 选项说明……） */
  children?: ReactNode;
  /** 动作区（编号按钮网格 / 操作按钮） */
  actions?: ReactNode;
  /** 底注（键位提示 / 未确认落盘提示 / 分诊文案 / 回执） */
  footer?: ReactNode;
  /** 额外 data-* 钩子（既有卡片把 phase/progress 等挂在根节点）。 */
  dataAttributes?: Record<string, string | number | undefined>;
}

/** 统一交互卡容器（§2.2 结构契约） */
export default function InteractiveCard({
  tone,
  mode,
  testId,
  title,
  titleSuffix,
  pulsing = true,
  children,
  actions,
  footer,
  dataAttributes,
}: InteractiveCardProps) {
  const t = TONES[tone];
  return (
    <div
      data-testid={testId}
      data-mode={mode}
      data-tone={tone}
      {...Object.fromEntries(
        Object.entries(dataAttributes ?? {})
          .filter(([, value]) => value !== undefined)
          .map(([key, value]) => [`data-${key}`, value])
      )}
      className={`card-slide-enter shrink-0 px-3 py-2 ${t.box}`}
    >
      {/* 状态色条头部：脉冲圆点 + 类型徽标（+ 可选附加） */}
      <div className="flex flex-wrap items-center gap-1.5">
        <span
          data-testid={`${testId}-dot`}
          className={`inline-block h-2 w-2 rounded-full ${t.dot} ${pulsing ? "animate-pulse" : ""}`}
        />
        <span className={`text-sm font-semibold ${t.title}`}>{title}</span>
        {titleSuffix}
      </div>
      {/* 主体 */}
      {children}
      {/* 动作区 */}
      {actions != null && <div className="mt-2">{actions}</div>}
      {/* 底注 */}
      {footer}
    </div>
  );
}
