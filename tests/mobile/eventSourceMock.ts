// EventSource 测试替身（M3 Task 6）：jsdom 无 EventSource，SSE 客户端与 Board 的
// 实时链路测试需要一个**可驱动的假事件流**（能手动投帧、手动触发 onerror）。
//
// 放置位置：测试目录内的共享 helper（非 src、非 tests/setup.ts 全局）——只有
// api 与 Board 两个用例文件消费；共享一份避免同类定义在多文件漂移
// （tests/msw/ 的共享 helper 同先例）。
export class MockEventSource {
  /** 创建过的全部实例（按创建顺序）：断言「退避后新建连接」与「降级后不再新建」 */
  static instances: MockEventSource[] = [];

  static reset() {
    MockEventSource.instances = [];
  }

  /** 最近创建的实例（不存在时抛错——测试写错顺序时立即失败而非静默 undefined） */
  static latest(): MockEventSource {
    const es = MockEventSource.instances[MockEventSource.instances.length - 1];
    if (!es) throw new Error("MockEventSource: 尚无实例（connectEvents 未建连？）");
    return es;
  }

  readonly url: string;
  /** 0=CONNECTING / 1=OPEN / 2=CLOSED（与真实 EventSource 常量同值） */
  readyState = 0;
  onerror: ((ev: Event) => unknown) | null = null;
  closed = false;
  private listeners = new Map<string, Set<(e: MessageEvent) => void>>();

  constructor(url: string) {
    this.url = url;
    MockEventSource.instances.push(this);
  }

  addEventListener(type: string, listener: (e: MessageEvent) => void) {
    if (!this.listeners.has(type)) this.listeners.set(type, new Set());
    this.listeners.get(type)!.add(listener);
  }

  removeEventListener(type: string, listener: (e: MessageEvent) => void) {
    this.listeners.get(type)?.delete(listener);
  }

  close() {
    this.closed = true;
    this.readyState = 2;
  }

  /** 测试驱动：投递一帧（data 为字符串则原样使用——便于构造坏帧；否则 JSON 序列化）。
   *  close() 之后投递是 no-op：与真实 EventSource 一致（关闭后不再分发事件） */
  emit(type: string, data: unknown) {
    if (this.closed) return;
    this.emitQueued(type, data);
  }

  /** 注入浏览器已经排队、但在 close 后才交付的回调；Board 的连接代次闸应丢弃它。 */
  emitQueued(type: string, data: unknown) {
    const e = {
      data: typeof data === "string" ? data : JSON.stringify(data),
    } as MessageEvent;
    for (const listener of this.listeners.get(type) ?? []) listener(e);
  }

  /** 测试驱动：模拟连接失败/中断（浏览器在断流与 403 拒绝时都只投递 error 事件） */
  fail() {
    if (this.closed) return; // 同 emit：关闭后不再分发
    this.onerror?.(new Event("error"));
  }
}
