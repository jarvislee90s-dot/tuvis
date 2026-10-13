import "./tauri-mock";
import React, { lazy, Suspense, useEffect } from "react";
import ReactDOM from "react-dom/client";
import { getCurrentWindow } from "@tauri-apps/api/window";
import "./index.css";
import "./i18n";
// 主题全局接线（issue #3）：所有窗口（主窗口/设置/关于等子窗口）加载同一 SPA。
// 侧效应导入保证模块级「首帧应用缓存主题 + 订阅全局主题事件」在每个窗口都执行；
// 显式调用 initTheme() 从 DB 事实源拉取当前主题校正（覆盖不消费 useTheme 的窗口）
import "@/components/common/theme-provider";
import { initTheme } from "@/components/common/theme-provider";
void initTheme();
// 物理坠落按应用重启归位（2026-10-13 用户裁决）：所有窗口入口统一前置触发一次
// （幂等——同进程多窗口拿同一 run id，标记一致即空过；见 petConfig.resetGravityPerRun）
import { resetGravityPerRun } from "@/components/pet/petConfig";
void resetGravityPerRun();
import { QueryClientProvider } from "@tanstack/react-query";
import { queryClient } from "@/lib/query/queryClient";
import { setupToolsChangedListener } from "@/lib/query/toolsChangedSync";
import { useRemoteEvents } from "@/hooks/useRemoteEvents";

const HomePage = lazy(() => import("./pages/home"));
const AboutPage = lazy(() => import("./pages/about"));
const SettingsPage = lazy(() => import("./pages/settings"));
// 用量大看板（计划② Task 6）：独立窗口 `usage-dashboard` 的页面（见 lib/usage/openWindow.ts）
const UsageDashboardPage = lazy(() => import("./pages/usage-dashboard"));

const NotificationPage = lazy(() => import("./pages/notification"));
const PetPage = lazy(() => import("./pages/pet"));

const pageMap = {
  "/": HomePage,
  "/about": AboutPage,
  "/settings": SettingsPage,
  // 漏注册会**静默回落首页**（有源码锁：tests/pet/usage-window.test.tsx 用例 1）
  "/usage": UsageDashboardPage,
};

const pathname = window.location.pathname;
// 通知浮窗与宠物窗口通过 hash 路由分流，与主窗口页面互不干扰
const isNotificationWindow = window.location.hash === "#/notification";
const isPetWindow = window.location.hash === "#/pet";
const PageComponent = isNotificationWindow
  ? NotificationPage
  : isPetWindow
    ? PetPage
    : (pageMap[pathname as keyof typeof pageMap] ?? HomePage);

function AppWrapper() {
  // M4 T2：桌面全局远程事件（配对请求系统通知 / 隧道地址与守护失败通知 /
  // 托盘触发的地址复制与开关失败提示）——本窗口挂载一次（浮窗/宠物窗口由
  // hook 内部 hash 守卫跳过，避免多 WebView 重复弹通知）
  useRemoteEvents();
  useEffect(() => {
    // 通知浮窗的显隐由 notification:new 事件驱动，宠物窗口由显隐状态驱动，创建时保持隐藏（避免空白窗抢显示）
    if (isNotificationWindow || isPetWindow) return;
    // Show window after React is ready (safe in browser too)
    try {
      getCurrentWindow().show();
    } catch {
      // Not in Tauri environment — ignore
    }
    // N2：设置窗口（独立 WebView）保存工具勾选后，后端广播 tools-changed，
    // 本窗口据此失效 enabled-tools / ssot-resources 等缓存——跨窗口缓存同步的唯一通道
    const disposed = setupToolsChangedListener(queryClient);
    return () => {
      void disposed.then((unlisten) => unlisten());
    };
  }, []);

  return <PageComponent />;
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <Suspense fallback={null}>
        <AppWrapper />
      </Suspense>
    </QueryClientProvider>
  </React.StrictMode>
);
