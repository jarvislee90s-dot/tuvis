import React from "react";
import ReactDOM from "react-dom/client";
// 移动端入口：纯浏览器页面，零 Tauri API——无 tauri-mock、无路由库、无 QueryProvider
// 不 import 桌面全量样式 src/index.css，仅引入 tailwind 入口
import "./mobile.css";
import App from "./App";
import { applyInitialTheme, applyInitialUiConfig, applyUiConfig } from "./theme";
import { fetchUiConfig } from "./api";
import { initMobileLanguage, resolveFirstLanguage } from "./language";

// P8f：应用初始主题（读 localStorage/系统偏好 → 设 documentElement 类）。
// 防闪白已由 mobile.html 内联脚本在首帧前完成；此处是权威应用点，React 挂载前执行，
// 保证首屏渲染与 html 类一致（内联脚本被 CSP 或旧 WebView 跳过时的兜底）
applyInitialTheme();

// 2026-10-05 UI 改版：外观配置三级回退（spec §6.2）——镜像先行（与防闪脚本同值，
// React 挂载前生效），服务器值异步覆写（403/失败静默保留镜像，不阻塞首帧）
applyInitialUiConfig();
void fetchUiConfig()
  .then((c) => applyUiConfig(c))
  .catch(() => {});

// 三终端语言自适应（C-6/C-7）：启动即按「本地手动缓存 > 浏览器归流」定启动语言
// （同步，含 html lang 应用）；随后等首个连接决策（/host 或超时兜底）才放行首帧。
// C-7 首帧闸：决策未定期间**不渲染任何内容**（此窗口为纯底色加载态），防「先浏览器
// 语言渲染、连接决策到达后翻桌面语言」的可见闪变；/host 失败或超时按浏览器语言
// fail-open（M2），resolveFirstLanguage 全路径不拒绝。ui-config 拉取与语言无关，
// 不被闸阻塞（其应用目标是 CSS 属性，非文案）
initMobileLanguage();
void resolveFirstLanguage().then(() => {
  ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
    <React.StrictMode>
      <App />
    </React.StrictMode>
  );
});
