// 预览形态切换器（需求 1，2026-09-16 用户裁决）：三态互切——
// 纵向分屏（上对话下文件）/ 横向分屏（左对话右文件）/ 全屏浮层。
// T1 sheet 化后唯一实例在 SessionDetail 顶栏 sheet bar；FilePreview 内部的
// 切换器仅显式传 onModeChange 的直用场景（独立测试）渲染。
import { Columns2, Maximize2, Rows2 } from "lucide-react";
import i18n from "@/i18n";

/** 预览形态三态：与 SessionDetail.PreviewMode 同源（此文件为避免循环依赖
 *  独立声明；两者由 usage 类型检查保持一致） */
export type PreviewMode = "split" | "split-h" | "fullscreen";

interface PreviewModeSwitcherProps {
  mode: PreviewMode;
  onChange: (mode: PreviewMode) => void;
  /** testid 前缀：SessionDetail sheet bar 用 preview-toggle；直用场景自定前缀
   *  （注意：若直用场景与 sheet bar 可能共存于同一棵树，须换前缀防重复 testid） */
  testIdPrefix: string;
}

export default function PreviewModeSwitcher({
  mode,
  onChange,
  testIdPrefix,
}: PreviewModeSwitcherProps) {
  const cls = (active: boolean) =>
    `rounded-full p-1 ${active ? "bg-[var(--cbg)] text-[var(--tx)]" : "text-[var(--mut)]"}`;
  return (
    <span
      role="group"
      aria-label={i18n.t("mobile.preview.layoutAria")}
      className="flex shrink-0 items-center rounded-full bg-[var(--cb)] p-0.5"
    >
      <button
        type="button"
        data-testid={`${testIdPrefix}-split`}
        aria-pressed={mode === "split"}
        aria-label={i18n.t("mobile.preview.layoutSplit")}
        onClick={() => onChange("split")}
        className={cls(mode === "split")}
      >
        {/* 上下分屏 = 中间一条横切线 = 上下两格 → Rows2 */}
        <Rows2 size={14} />
      </button>
      <button
        type="button"
        data-testid={`${testIdPrefix}-split-h`}
        aria-pressed={mode === "split-h"}
        aria-label={i18n.t("mobile.preview.layoutSplitH")}
        onClick={() => onChange("split-h")}
        className={cls(mode === "split-h")}
      >
        {/* 左右分屏 = 中间一条竖切线 = 左右两格 → Columns2 */}
        <Columns2 size={14} />
      </button>
      <button
        type="button"
        data-testid={`${testIdPrefix}-fullscreen`}
        aria-pressed={mode === "fullscreen"}
        aria-label={i18n.t("mobile.preview.layoutFullscreen")}
        onClick={() => onChange("fullscreen")}
        className={cls(mode === "fullscreen")}
      >
        <Maximize2 size={14} />
      </button>
    </span>
  );
}
