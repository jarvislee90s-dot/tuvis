// 可拖动分隔条（需求，2026-09-16 用户裁决）：预览分屏的对话区 / 文件区之间
// 的尺寸可拖拽调整——横向分屏拖宽窄、纵向分屏拖高低。
//
// 实现要点：
// - 受控比值 `ratio`（文件栏占比，0..1）：父组件持有，拖动回写；切换形态时各自
//   保留（横向拖宽度、纵向拖高度互不干扰）；
// - 拖动期间用 window 级 pointermove/pointerup 监听（而非 setPointerCapture：
//   jsdom 无该方法，且 window 监听在指针滑出 handle 后仍持续跟踪）；
// - 坐标换算：横向按 clientX 增量 / 容器宽；纵向按 clientY 增量 / 容器高。
//   **方向取决于 ratioPane**（文件栏在分隔条之前还是之后，见 prop 注释）——
//   不变式只有一条：分隔条跟随指针（2026-09-16 用户裁决），拖向文件栏 = 文件栏变小；
// - 钳制 [MIN_RATIO, MAX_RATIO]：任一区不被拖到消失（15%–85%）；
// - 键盘可达：handle 为 button，方向键微调 2%（无障碍，不限于指针设备）；
//   方向同样随 ratioPane 取反；
// - touch-action: none 关掉浏览器滚动手势，避免手机上拖动被识别为滚动。
import { useCallback, useEffect, useRef } from "react";
import i18n from "@/i18n";

/** 文件栏占比下/上限：任一区至少保留 15%（防拖没） */
export const MIN_RATIO = 0.15;
export const MAX_RATIO = 0.85;
/** 键盘方向键步长（占容器比例） */
const KEY_STEP = 0.02;

interface SplitHandleProps {
  /** 分屏方向：split-h = 左右（拖 clientX）/ split = 上下（拖 clientY） */
  orientation: "horizontal" | "vertical";
  /** 当前文件栏占比（受控） */
  ratio: number;
  /** 拖动回调：传入已钳制的新占比 */
  onRatioChange: (ratio: number) => void;
  /** 容器尺寸读数（拖动换算的分母来源） */
  containerRef: React.RefObject<HTMLElement | null>;
  /** 文件栏在分隔条之前还是之后（2026-09-20 竖屏换位引入）：
   *  - "after"（默认，split-h 现状）：布局 [对话][分隔条][文件栏]；
   *  - "before"（split 换位后）：布局 [文件栏][分隔条][对话]——文件栏在上方，
   *    拖动/键盘方向据此取反，「分隔条跟随指针」不变式两态均保持 */
  ratioPane?: "after" | "before";
  /** 附加到 handle 的类（竖屏换位用 CSS order 视觉换位时传 order-2） */
  className?: string;
}

export default function SplitHandle({
  orientation,
  ratio,
  onRatioChange,
  containerRef,
  ratioPane = "after",
  className,
}: SplitHandleProps) {
  // 拖动起始快照（起点坐标 + 起点占比）：每次 pointerdown 重置
  const dragRef = useRef<{ pos: number; ratio: number } | null>(null);

  const clamp = (v: number) => Math.min(MAX_RATIO, Math.max(MIN_RATIO, v));

  const onPointerDown = useCallback(
    (e: React.PointerEvent<HTMLButtonElement>) => {
      e.preventDefault();
      dragRef.current = {
        pos: orientation === "horizontal" ? e.clientX : e.clientY,
        ratio,
      };
    },
    [orientation, ratio]
  );

  // window 级监听随组件常挂：靠 dragRef 判空——非拖动期移动指针为 no-op，
  // 拖动期即使指针滑出 handle 也持续跟踪（不依赖 setPointerCapture）
  useEffect(() => {
    const onMove = (e: PointerEvent) => {
      const drag = dragRef.current;
      if (!drag) return;
      const el = containerRef.current;
      if (!el) return;
      const rect = el.getBoundingClientRect();
      const span = orientation === "horizontal" ? rect.width : rect.height;
      if (span <= 0) return; // 无布局引擎 / 未挂载：不换算
      const now = orientation === "horizontal" ? e.clientX : e.clientY;
      // 语义：分隔条跟随指针（2026-09-16 用户裁决；方向随 ratioPane 取反，两态
      // 拖向文件栏都 = 文件栏被压小）。
      // - "after"（文件栏在分隔条之后，split-h 现状 [对话][分隔条][文件栏]）：
      //   取负差值——横向右拖（now 增）→ 右侧文件栏变窄；纵向下拖 → 下方文件栏变矮。
      //   原横向曾用正差值（拖右反而变大＝逆着指针走），用户裁决修正。
      // - "before"（文件栏在分隔条之前，split 换位后 [文件栏][分隔条][对话]）：
      //   取正差值——纵向下拖 → 分隔条下移把上方文件栏撑大 = 占比增。
      const delta = ratioPane === "after" ? drag.pos - now : now - drag.pos;
      onRatioChange(clamp(drag.ratio + delta / span));
    };
    const onUp = () => {
      dragRef.current = null;
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
    window.addEventListener("pointercancel", onUp);
    return () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      window.removeEventListener("pointercancel", onUp);
    };
  }, [containerRef, onRatioChange, orientation, ratioPane]);

  // 键盘微调：方向键按「让文件栏变大的方向」直觉映射；ratioPane="before"
  // （文件栏在上/左）时取反——竖屏换位后 ArrowDown 才是「文件栏变大」
  const onKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLButtonElement>) => {
      const bigger =
        ratioPane === "before"
          ? orientation === "horizontal"
            ? "ArrowLeft"
            : "ArrowDown"
          : orientation === "horizontal"
            ? "ArrowRight"
            : "ArrowUp";
      const smaller =
        ratioPane === "before"
          ? orientation === "horizontal"
            ? "ArrowRight"
            : "ArrowUp"
          : orientation === "horizontal"
            ? "ArrowLeft"
            : "ArrowDown";
      if (e.key === bigger) {
        e.preventDefault();
        onRatioChange(clamp(ratio + KEY_STEP));
      } else if (e.key === smaller) {
        e.preventDefault();
        onRatioChange(clamp(ratio - KEY_STEP));
      }
    },
    [onRatioChange, orientation, ratio, ratioPane]
  );

  return (
    <button
      type="button"
      role="separator"
      aria-orientation={orientation === "horizontal" ? "vertical" : "horizontal"}
      aria-label={i18n.t("mobile.split.handleAria")}
      aria-valuenow={Math.round(ratio * 100)}
      aria-valuemin={Math.round(MIN_RATIO * 100)}
      aria-valuemax={Math.round(MAX_RATIO * 100)}
      data-testid="split-handle"
      onPointerDown={onPointerDown}
      onKeyDown={onKeyDown}
      style={{ touchAction: "none" }}
      className={
        (className ? className + " " : "") +
        (orientation === "horizontal"
          ? "w-1.5 shrink-0 cursor-col-resize bg-[var(--cb)] hover:bg-[var(--btnp)] dark:hover:bg-[var(--btnp)]/60"
          : "h-1.5 shrink-0 cursor-row-resize bg-[var(--cb)] hover:bg-[var(--btnp)] dark:hover:bg-[var(--btnp)]/60")
      }
    />
  );
}
