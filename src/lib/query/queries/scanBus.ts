// 扫描总线前端接入：档位跟随 + 快照事件同步 + 窗口级可见性心跳。
//
// 三个出口：
//  * `useScanSnapshotSync(enabled)` —— **主路径**：监听后端 `scan-bus-snapshot`，
//    新版本直接 `setQueryData(["sessions"])`（载荷自带完整 SessionsResponse，不再额外 IPC）。
//    每窗口维护 `lastVersion`：相同/更旧版本忽略，多窗口广播与同窗多实例都不会重复刷新。
//  * `useScanMode()` —— 返回**轮询间隔毫秒**喂给 React Query 的 `refetchInterval`，
//    作为快照事件的兜底（事件无重放，不能当唯一来源）。档位优先取快照事件的 mode，
//    其次 `scan-bus-mode` 事件，挂载时用 `scan_bus_status` 命令兜底。
//  * `useScanBusHeartbeat(enabled)` —— 可见性心跳：窗口可见且聚焦 = 「有人在看」，
//    上报当前 WebView label（主窗 "main" / 宠物 "pet"），后端按窗口记账（TTL 90s）。
//
// 三个纪律：
//  ① **载荷必须过类型守卫**：浏览器（vite dev / Playwright）里 `invoke` / `listen` 只返回
//     undefined（见 `src/tauri-mock.ts` 的兜底），若直接把 `undefined` 塞进 `MODE_INTERVAL_MS`
//     会得到 NaN 间隔；故非三档值一律忽略，保持默认 idle。
//  ② **失败静默**：档位、快照与心跳都是优化项，任何 IPC 失败都 `.catch()` 吞掉——渲染不得因
//     非 Tauri 环境（浏览器）抛错；`listen` 在无 `__TAURI_INTERNALS__` 时是 rejected promise，
//     同样要收口。
//  ③ **事件只做加速**：数据一致性以 version 为准；快照事件丢失时仍有档位轮询兜底。

import { useContext, useEffect, useRef, useState } from "react";
import { QueryClientContext } from "@tanstack/react-query";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { SESSIONS_QUERY_KEY } from "@/lib/query/optimisticSessions";
import { queryClient as appQueryClient } from "@/lib/query/queryClient";
import type { SessionsResponse } from "@/types/session";

export type ScanMode = "active" | "idle" | "away";

/** 与 Rust `adapter::cadence::Mode::interval` 同值（active 2s / idle 10s / away 30s） */
export const MODE_INTERVAL_MS: Record<ScanMode, number> = {
  active: 2_000,
  idle: 10_000,
  away: 30_000,
};

/** 心跳续期周期；必须显著小于后端 TTL（90s） */
export const HEARTBEAT_INTERVAL_MS = 30_000;

/** 后端 `scan-bus-snapshot` 事件载荷（字段与 `snapshot_payload` 逐项对应） */
export interface ScanSnapshotPayload {
  version: number;
  epoch: number;
  reason: "cold_start" | "periodic" | "event" | "action";
  /** 墙上时间，仅展示/排查（时效判定不看这个字段） */
  scannedAtMs: number;
  mode: ScanMode;
  degraded: boolean;
  /** 完整 SessionsResponse（camelCase），直接写 Query 缓存 */
  data: SessionsResponse;
}

function isScanMode(v: unknown): v is ScanMode {
  return v === "active" || v === "idle" || v === "away";
}

/** 会话响应形状守卫：载荷来自后端，但浏览器 mock/版本漂移时不得把垃圾写进缓存 */
function isSessionsResponse(v: unknown): v is SessionsResponse {
  if (typeof v !== "object" || v === null) return false;
  const r = v as Partial<SessionsResponse>;
  return (
    Array.isArray(r.sessions) &&
    typeof r.totalCount === "number" &&
    typeof r.waitingCount === "number"
  );
}

/**
 * 当前 WebView 的窗口 label（后端按窗口记账的键）。非 Tauri 环境/取不到时回落 "main"，
 * 保证浏览器预览与单测仍有一个稳定源，不抛错。
 */
export function currentScanWindowLabel(): string {
  try {
    const label = getCurrentWebviewWindow().label;
    return typeof label === "string" && label.trim() !== "" ? label : "main";
  } catch {
    return "main";
  }
}

/**
 * 每个窗口已应用的最大快照版本。模块级（= 单个 WebView 的 JS 上下文）：
 * 同窗挂多个 `useScanSnapshotSync` 实例时只写一次；不同窗口各有自己的上下文，
 * 互不影响。
 */
const snapshotVersionByWindow = new Map<string, number>();

/** 单测用：清空版本水位（避免用例间共享模块状态） */
export function __resetScanSnapshotVersionsForTest(): void {
  snapshotVersionByWindow.clear();
}

/**
 * 快照事件同步（F4/F6 主路径）：后端每次发布快照都会广播 `scan-bus-snapshot`
 * （ColdStart/Periodic/Event/Action 全覆盖），这里按 version 单调应用：
 *  * `version > lastVersion` → `setQueryData`（含档位未变化的事件扫描）；
 *  * 相同/更旧版本 → 忽略（多窗口广播、乱序到达都不会让缓存回退或重复写）。
 */
export function useScanSnapshotSync(enabled = true): void {
  // Provider 在场用 Provider 的 client（测试/嵌入场景），缺席回落应用单例
  // （宠物窗口单元测试常直接渲染 FoxbellPet，无 QueryClientProvider）。
  // 注意用 QueryClientContext 而非 useQueryClient：后者在无 Provider 时抛错。
  const queryClient = useContext(QueryClientContext) ?? appQueryClient;
  useEffect(() => {
    if (!enabled) return;
    const windowLabel = currentScanWindowLabel();
    let alive = true;
    const un = listen<Partial<ScanSnapshotPayload>>("scan-bus-snapshot", (e) => {
      if (!alive) return;
      const payload = e.payload;
      const version = payload?.version;
      if (typeof version !== "number" || !Number.isFinite(version)) return;
      if (version <= (snapshotVersionByWindow.get(windowLabel) ?? 0)) return;
      if (!isSessionsResponse(payload.data)) return;
      // 先记水位再写缓存：写入期间重入/并发事件不会重复应用同一版本
      snapshotVersionByWindow.set(windowLabel, version);
      queryClient.setQueryData(SESSIONS_QUERY_KEY, payload.data);
    }).catch(() => () => {});
    return () => {
      alive = false;
      void un.then((f) => f());
    };
  }, [queryClient, enabled]);
}

/**
 * 返回当前档位对应的轮询间隔（毫秒）——**兜底**通道，主路径是快照事件。
 * 默认 idle：首帧不等 IPC 回来就有一个保守间隔，避免 `undefined`/0 造成的密集轮询。
 *
 * 竞态处理：`scan_bus_status` 的响应可能比第一条事件更晚落地（IPC 往返 vs 广播），
 * 此时它携带的是旧档位——用 `sawEventRef` 记住「本生命周期内已收到事件」，
 * 事件先到则丢弃初始响应。
 */
export function useScanMode(): number {
  const [mode, setMode] = useState<ScanMode>("idle");
  const sawEventRef = useRef(false);
  useEffect(() => {
    let alive = true;
    const apply = (next: unknown) => {
      if (!alive || !isScanMode(next)) return;
      sawEventRef.current = true;
      setMode(next);
    };
    void invoke<{ mode?: unknown } | undefined>("scan_bus_status")
      .then((s) => {
        if (!alive || sawEventRef.current) return;
        if (isScanMode(s?.mode)) setMode(s.mode);
      })
      .catch(() => {});
    const unMode = listen<{ mode?: unknown }>("scan-bus-mode", (e) => apply(e.payload?.mode)).catch(
      () => () => {}
    );
    // Task 2：快照事件也携带发布时的档位（主路径）；数据同步由 useScanSnapshotSync 负责，
    // 这里只更新间隔，不再 invalidate（避免事件后又多打一次 get_all_sessions）
    const unSnapshot = listen<{ mode?: unknown }>("scan-bus-snapshot", (e) =>
      apply(e.payload?.mode)
    ).catch(() => () => {});
    return () => {
      alive = false;
      void unMode.then((f) => f());
      void unSnapshot.then((f) => f());
    };
  }, []);
  return MODE_INTERVAL_MS[mode];
}

/** 当前窗口是否「可见且聚焦」——心跳的全部判据（隐藏/失焦 = 没人在看） */
function isWindowWatching(): boolean {
  const focused = typeof document.hasFocus === "function" ? document.hasFocus() : true;
  return document.visibilityState === "visible" && focused;
}

/**
 * 可见性心跳：挂载即报一次，之后每 30s 与 visibility/focus/blur 变化时续报；
 * 卸载报 false（窗口关闭/重挂载立刻收回本窗口的「在看」）。
 *
 * 上报当前 WebView 的 label（`getCurrentWebviewWindow().label`）而不是写死 source：
 * 后端按窗口独立记账，任一窗口的 false→true 都会触发一次 Action 补扫
 * （「桌宠已展开时主窗重新聚焦」也能刷新）。
 *
 * `enabled=false` 供非主窗（通知浮窗 / 宠物窗口）跳过：宠物窗口由 `UsageMiniBar`
 * 自己按迷你条可见性上报（同一 label 键，展开 = 在看）。
 */
export function useScanBusHeartbeat(enabled = true): void {
  useEffect(() => {
    if (!enabled) return;
    const windowLabel = currentScanWindowLabel();
    const send = (watching: boolean) => {
      void invoke("scan_bus_watch", { window: windowLabel, watching }).catch(() => {});
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
  }, [enabled]);
}
