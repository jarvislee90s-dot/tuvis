// 跨窗导航的可靠消费（2026-10-26 计划 Task 7）。**只在主窗口宿主挂载一次**。
//
// 三条消费路径，全部收敛到 `applyNavigation`（revision 单调裁决）+ ACK：
//  ① 订阅 `tuvis-workspace-navigation` 事件——正常路径（Rust 先存后 emit）；
//  ② 订阅后立即 `peek` 一次——恢复**监听就绪前**到达的目标（启动早期竞态，
//     spec §2.4「不能只依赖一次广播」）；
//  ③ 窗口重新获得焦点 / 文档重新可见时再 `peek`——主窗隐藏期间到达的目标在
//     唤起后补上（事件可能已在隐藏期被错过；peek 语义幂等，多查无害）。
//
// ACK 纪律：应用后 ACK 自己的 revision；返回 false（已被更新的目标替换）静默忽略——
// 更新的目标自有它自己的事件/peek 在路上。
import { useEffect } from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useWorkspaceStore } from "@/stores/workspaceStore";
import {
  WORKSPACE_NAVIGATION_EVENT,
  ackWorkspaceNavigation,
  peekWorkspaceNavigation,
} from "@/lib/workspaceNavigation";
import type { WorkspaceNavigation } from "@/types/workspace";

export function useWorkspaceNavigation(): void {
  const applyNavigation = useWorkspaceStore((s) => s.applyNavigation);

  useEffect(() => {
    let disposed = false;
    const cleanups: Array<() => void> = [];

    // peek 一轮：取走 pending（若有）→ 应用 → ACK。乱序到达的旧目标在 store 里被忽略，
    // 但仍会对它 ACK（false，无害）；queue 里的最新目标由它自己的轮次消费。
    const consume = () => {
      void (async () => {
        try {
          const nav = await peekWorkspaceNavigation();
          if (!nav || disposed) return;
          applyNavigation(nav);
          const ok = await ackWorkspaceNavigation(nav.revision);
          if (!ok) console.warn("[workspace] 过期 ACK（目标已被更新的请求替换）：", nav.revision);
        } catch (error) {
          // 队列命令失败（如 Tauri 环境外）：静默，导航不是关键路径
          console.warn("[workspace] peek 导航目标失败：", error);
        }
      })();
    };

    // ① 事件路径（正常路径）
    void listen<WorkspaceNavigation>(WORKSPACE_NAVIGATION_EVENT, (event) => {
      applyNavigation(event.payload);
      // ACK 只是消费语义：失败/过期都不影响 store（revision 已裁决）
      void ackWorkspaceNavigation(event.payload.revision).catch(() => {});
    })
      .then((unlisten) => {
        if (disposed) unlisten();
        else cleanups.push(unlisten);
      })
      .catch((error) => console.warn("[workspace] 导航事件订阅失败：", error));

    // ② 订阅就绪后立即 peek：恢复监听前到达的目标
    consume();

    // ③ 唤起恢复路径：窗口聚焦（Tauri focus 事件）与文档重新可见都再 peek 一次。
    //    浏览器/jsdom 无 Tauri 窗口 API → try/catch 降级（与 main.tsx 同模式）。
    try {
      void getCurrentWindow()
        .onFocusChanged(({ payload: focused }) => {
          if (focused) consume();
        })
        .then((unlisten) => {
          if (disposed) unlisten();
          else cleanups.push(unlisten);
        });
    } catch {
      // 非 Tauri 环境（浏览器预览/测试）：只留 visibilitychange 一条路
    }
    const onVisibilityChange = () => {
      if (document.visibilityState === "visible") consume();
    };
    document.addEventListener("visibilitychange", onVisibilityChange);
    cleanups.push(() => document.removeEventListener("visibilitychange", onVisibilityChange));

    return () => {
      disposed = true;
      for (const cleanup of cleanups) cleanup();
    };
  }, [applyNavigation]);
}
