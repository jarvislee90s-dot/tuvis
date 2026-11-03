// 远程端方向判定（2026-10-27 用户裁决 U1：纯按宽高比）
//
// 判据单点：宽 > 高 × 1.2 → 横屏（进入详情默认「左消息 + 右文件窗格」）；否则竖屏
// （默认聊天框）。近方形（宽 > 高但不超 1.2 倍）按竖屏算——只看这一条比例，不掺
// 屏幕宽度阈值。
//
// **用媒体查询而不是 window.innerWidth / innerHeight**，三个理由：
// 1. 旋转必然触发 MQL change，resize 不必然（个别 WKWebView 漏发时另有兜底）；
// 2. 软键盘免疫——iOS / Android 默认只缩视觉视口（mobile.html 未设 interactive-widget，
//    刻意不加：resizes-content 会把布局视口压小，打字时误判成竖屏把右栏关掉）；
// 3. jsdom 没有 matchMedia → 归为竖屏，与移动端测试「matchMedia 缺失 = 窄屏」的既有
//    口径一致（若读 innerWidth/innerHeight，jsdom 默认 1024×768 = 横屏，既有用例会在
//    挂载时就多出一个右栏）。

export type ViewportKind = "portrait" | "landscape";

/** 横屏判据：（宽 / 高）≥ 1.2。媒体查询只能表达 ≥，与「大于 1.2」在临界点差一像素级 */
export const LANDSCAPE_QUERY = "(min-aspect-ratio: 6/5)";

/** 读当前方向：matchMedia 缺失（jsdom / 旧 WebView）或抛错 → 竖屏（保守回退，theme.ts 同款防御式口径） */
export function readViewportKind(): ViewportKind {
  try {
    // `?.` 必须落在 matches 之前：只保护调用的话，无 matchMedia 时每次渲染都要抛一次
    // TypeError 再被 catch 吃掉（getSnapshot 每次渲染都会调这个函数）
    return window.matchMedia?.(LANDSCAPE_QUERY)?.matches === true ? "landscape" : "portrait";
  } catch {
    return "portrait"; // 隐私模式 / 内核异常：按竖屏处理
  }
}

/**
 * 订阅方向变化：MQL change 为主判据，orientationchange 为兜底（存在才挂）。
 *
 * 回调忽略事件参数、统一重读 readViewportKind()——MediaQueryListEvent 与 legacy
 * MediaQueryList 两种事件形态都能吃下。MQL 两套 API 都没有（老 shim / 极端内核）→
 * 静默不订阅，不抛错。**不做去抖**：上层 useViewportKind 以字符串快照去重，同值
 * 事件不会引发重渲染。
 */
export function subscribeViewportKind(onChange: () => void): () => void {
  const cleanups: Array<() => void> = [];
  try {
    const mq = window.matchMedia?.(LANDSCAPE_QUERY);
    if (mq) {
      if (typeof mq.addEventListener === "function") {
        mq.addEventListener("change", onChange);
        cleanups.push(() => mq.removeEventListener("change", onChange));
      } else if (typeof mq.addListener === "function") {
        // iOS 13 及更早只有 legacy 两件套
        mq.addListener(onChange);
        cleanups.push(() => mq.removeListener(onChange));
      }
    }
  } catch {
    // matchMedia 抛错：主判据不可用，只剩 orientationchange 兜底
  }
  // orientationchange 在部分 WebKit / 旧 Android 上**先于布局更新**派发：那一刻读到的
  // 还是旧方向。若同步唤醒，会把刚落定的新方向打回相反值（典型后果：旋转到横屏后右栏
  // 先出现又被关掉，且此后没有新事件）。故延后一帧再唤醒，让布局与媒体查询求值先落定；
  // MQL 主通道不受影响，仍然同步。
  const onOrientationChange = () => {
    if (typeof window.requestAnimationFrame === "function") {
      window.requestAnimationFrame(onChange);
    } else {
      window.setTimeout(onChange, 0);
    }
  };
  try {
    window.addEventListener("orientationchange", onOrientationChange, { passive: true });
    cleanups.push(() => window.removeEventListener("orientationchange", onOrientationChange));
  } catch {
    // 非浏览器环境：无兜底
  }
  return () => {
    for (const off of cleanups) off();
  };
}
