// 扫描档位跟随（Task 9）：把后端的 2s / 10s / 30s 档位搬到前端。
//
// 两个出口：
//  * `useScanMode()` —— 读 `scan_bus_status` 一次拿当前档位，之后跟随 `scan-bus-mode`
//    事件（档位切换时后端 emit，载荷 `{ mode }`）；返回**轮询间隔毫秒**，直接喂给
//    React Query 的 `refetchInterval`，让会话轮询与后端扫描同频。
//  * `useScanBusHeartbeat(enabled)` —— 可见性心跳：窗口可见且聚焦 = 「有人在看」，
//    后端据此把档位降到 idle（TTL 90s，见 `src-tauri/src/adapter/scan_bus.rs`）。
//
// 两个纪律：
//  ① **载荷必须过类型守卫**：浏览器（vite dev / Playwright）里 `invoke` / `listen` 只返回
//     undefined（见 `src/tauri-mock.ts` 的兜底），若直接把 `undefined` 塞进 `MODE_INTERVAL_MS`
//     会得到 NaN 间隔；故非三档值一律忽略，保持默认 idle。
//  ② **失败静默**：档位与心跳都是优化项，任何 IPC 失败都 `.catch()` 吞掉——渲染不得因
//     非 Tauri 环境（浏览器）抛错；`listen` 在无 `__TAURI_INTERNALS__` 时是 rejected promise，
//     同样要收口。

import { useEffect, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { SESSIONS_QUERY_KEY } from "@/lib/query/optimisticSessions";

export type ScanMode = "active" | "idle" | "away";

/** 与 Rust `adapter::cadence::Mode::interval` 同值（active 2s / idle 10s / away 30s） */
export const MODE_INTERVAL_MS: Record<ScanMode, number> = {
  active: 2_000,
  idle: 10_000,
  away: 30_000,
};

/** 可见性心跳的源标识：主窗 / 宠物浮窗各记一份，后端按源并集判定（不互相清掉） */
export type ScanWatchSource = "main" | "pet";
/** 心跳续期周期；必须显著小于后端 TTL（90s） */
export const HEARTBEAT_INTERVAL_MS = 30_000;

function isScanMode(v: unknown): v is ScanMode {
  return v === "active" || v === "idle" || v === "away";
}

/**
 * 返回当前档位对应的轮询间隔（毫秒）。
 * 默认 idle：首帧不等 IPC 回来就有一个保守间隔，避免 `undefined`/0 造成的密集轮询。
 *
 * 两处竞态/时效处理（评审 Fix 2）：
 *  * **晚到的初始快照不得覆盖新事件**：`scan_bus_status` 的响应可能比第一条
 *    `scan-bus-mode` 事件更晚落地（IPC 往返 vs 广播），此时它携带的是旧档位——
 *    用 `sawEventRef` 记住「本生命周期内已收到事件」，事件先到则丢弃初始响应；
 *  * **事件到达即失效会话查询**：档位切换意味着后端刚发布了新扫描快照，
 *    主动 invalidate 让 UI 立刻读到，不必等下一次轮询（F4 focus refresh）。
 *    只对事件做，初始快照不触发（避免挂载时白白多打一次 `get_all_sessions`）。
 */
export function useScanMode(): number {
  const [mode, setMode] = useState<ScanMode>("idle");
  const queryClient = useQueryClient();
  const sawEventRef = useRef(false);
  useEffect(() => {
    let alive = true;
    void invoke<{ mode?: unknown } | undefined>("scan_bus_status")
      .then((s) => {
        if (!alive || sawEventRef.current) return;
        if (isScanMode(s?.mode)) setMode(s.mode);
      })
      .catch(() => {});
    const un = listen<{ mode?: unknown }>("scan-bus-mode", (e) => {
      if (!alive || !isScanMode(e.payload?.mode)) return;
      sawEventRef.current = true;
      setMode(e.payload.mode);
      void queryClient.invalidateQueries({ queryKey: SESSIONS_QUERY_KEY });
    }).catch(() => () => {});
    return () => {
      alive = false;
      void un.then((f) => f());
    };
  }, [queryClient]);
  return MODE_INTERVAL_MS[mode];
}

/** 当前窗口是否「可见且聚焦」——心跳的全部判据（隐藏/失焦 = 没人在看） */
function isWindowWatching(): boolean {
  const focused = typeof document.hasFocus === "function" ? document.hasFocus() : true;
  return document.visibilityState === "visible" && focused;
}

/**
 * 可见性心跳：挂载即报一次，之后每 30s 与 visibility/focus/blur 变化时续报；
 * 卸载报 false（窗口关闭/重挂载立刻收回本源的「在看」）。
 *
 * `enabled=false` 供非主窗（通知浮窗 / 宠物窗口）跳过：宠物浮窗由 `UsageMiniBar`
 * 自己以 `source: "pet"` 上报（展开 = 在看），主窗心跳不能替它说话。
 */
export function useScanBusHeartbeat(enabled = true, source: ScanWatchSource = "main"): void {
  useEffect(() => {
    if (!enabled) return;
    const send = (watching: boolean) => {
      void invoke("scan_bus_watch", { watching, source }).catch(() => {});
    };
    const report = () => send(isWindowWatching());
    report();
    const id = window.setInterval(report, HEARTBEAT_INTERVAL_MS);
    document.addEventListener("visibilitychange", report);
    window.addEventListener("focus", report);
    window.addEventListener("blur", report);
    return () => {
      window.clearInterval(id);
      document.removeEventListener("visibilitychange", report);
      window.removeEventListener("focus", report);
      window.removeEventListener("blur", report);
      send(false);
    };
  }, [enabled, source]);
}
