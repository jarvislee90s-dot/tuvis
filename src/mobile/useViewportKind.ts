// 方向判定 hook（2026-10-27 用户裁决 U1，判据见 ./viewport）
//
// useSyncExternalStore 一次解决三件事：首帧即读到正确值（不用 useState + effect 的
// 「首渲染错值再纠正」）、render → subscribe 之间发生的旋转不漏、无 matchMedia 环境
// （jsdom / 隐私模式）的兜底值有明确出处。快照是字符串，同值事件不会引发重渲染。
import { useSyncExternalStore } from "react";
import { readViewportKind, subscribeViewportKind, type ViewportKind } from "./viewport";

/** 服务端 / 无 matchMedia 环境的快照：与 readViewportKind 的兜底一致（模块级常量，
 *  保持 getServerSnapshot 的引用稳定——useSyncExternalStore 的三参都不该每次渲染换身份） */
const SERVER_SNAPSHOT: ViewportKind = "portrait";

export function useViewportKind(): ViewportKind {
  return useSyncExternalStore(subscribeViewportKind, readViewportKind, () => SERVER_SNAPSHOT);
}
