// 测试用 IntersectionObserver 替身（jsdom 无此实现）。
// 观察元素由用例手动触发进入/离开视口，从而验证「无视口不加载」与并发调度。
import { vi } from "vitest";

export class MockIntersectionObserver {
  static instances: MockIntersectionObserver[] = [];

  static reset() {
    MockIntersectionObserver.instances = [];
  }

  /** 触发所有实例当前观察中的元素（默认视为进入视口） */
  static triggerAll(isIntersecting = true) {
    for (const io of MockIntersectionObserver.instances) {
      for (const el of [...io.elements]) io.trigger(el, isIntersecting);
    }
  }

  callback: IntersectionObserverCallback;
  elements = new Set<Element>();
  root: Element | Document | null = null;
  rootMargin = "";
  thresholds: number[] = [];

  constructor(callback: IntersectionObserverCallback) {
    this.callback = callback;
    MockIntersectionObserver.instances.push(this);
  }

  observe(el: Element) {
    this.elements.add(el);
  }

  unobserve(el: Element) {
    this.elements.delete(el);
  }

  disconnect() {
    this.elements.clear();
  }

  takeRecords(): IntersectionObserverEntry[] {
    return [];
  }

  trigger(el: Element, isIntersecting = true) {
    this.callback(
      [
        {
          target: el,
          isIntersecting,
          intersectionRatio: isIntersecting ? 1 : 0,
        } as IntersectionObserverEntry,
      ],
      this as unknown as IntersectionObserver
    );
  }

  static install() {
    MockIntersectionObserver.reset();
    vi.stubGlobal(
      "IntersectionObserver",
      MockIntersectionObserver as unknown as typeof IntersectionObserver
    );
  }
}
