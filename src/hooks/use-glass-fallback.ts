// 轻玻璃降级探测（2026-10-26 计划 Task 8，spec §3.2）：无法使用背景模糊、或系统
// 要求减少透明度时，`.workspace-glass` 面必须退成同色实心。CSS 侧已有
// @supports / prefers-reduced-transparency 双保险；本 hook 把探测结果以
// `data-glass-fallback="<原因>"` 挂到玻璃元素上，让降级可检验、可测试。
import { useEffect, useState } from "react";

export type GlassFallback = "glass" | "no-backdrop-filter" | "reduced-transparency";

export function detectGlassFallback(): GlassFallback {
  try {
    if (
      typeof window.matchMedia === "function" &&
      window.matchMedia("(prefers-reduced-transparency: reduce)").matches
    ) {
      return "reduced-transparency";
    }
    const supports = (window as { CSS?: { supports?: unknown } }).CSS?.supports;
    if (typeof supports === "function") {
      const ok =
        (supports as (a: string, b: string) => boolean)("backdrop-filter", "blur(1px)") ||
        (supports as (a: string, b: string) => boolean)("-webkit-backdrop-filter", "blur(1px)");
      if (!ok) return "no-backdrop-filter";
    }
    return "glass";
  } catch {
    return "glass";
  }
}

/** 挂在玻璃宿主（导航/标题栏）上：返回降级原因，"glass" = 无需降级。 */
export function useGlassFallback(): GlassFallback {
  const [fallback, setFallback] = useState<GlassFallback>(detectGlassFallback);
  useEffect(() => {
    if (typeof window.matchMedia !== "function") return;
    try {
      const mq = window.matchMedia("(prefers-reduced-transparency: reduce)");
      const onChange = () => setFallback(detectGlassFallback());
      mq.addEventListener?.("change", onChange);
      return () => mq.removeEventListener?.("change", onChange);
    } catch {
      // 无 matchMedia 的环境（jsdom 等）：保持首帧探测值
    }
  }, []);
  return fallback;
}
