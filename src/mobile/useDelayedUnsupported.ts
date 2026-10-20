import { useEffect, useState } from "react";

/**
 * 瞬态 Unsupported 提示去抖（计划 Task 7）。
 *
 * 真机证据：Turn 起始 patch 后 `turnHistory` 会短暂不可读，投影在单拍上落到
 * `unsupported`（Windows 4894 rev2 / rev25，过渡 164–382ms），随后恢复 ready。
 * 直接显示「读取异常」会造成闪烁；本 hook 只延迟**前端错误提示**——后端即时
 * contentState、禁写、stale 与屏障一律不去抖（真值不被遮蔽）。
 *
 * 窗口 500ms 覆盖已观察的 164–382ms null-ID 过渡；这是提示窗口，不是协议超时。
 */
export const UNSUPPORTED_NOTICE_MS = 500;

export default function useDelayedUnsupported(sessionKey: string, active: boolean): boolean {
  const [shownFor, setShownFor] = useState<string | null>(null);
  useEffect(() => {
    setShownFor(null);
    if (!active) return;
    const timer = setTimeout(() => setShownFor(sessionKey), UNSUPPORTED_NOTICE_MS);
    return () => clearTimeout(timer);
  }, [sessionKey, active]);
  return active && shownFor === sessionKey;
}
