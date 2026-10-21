// 附件图片懒加载与资源生命周期（任务 6）。
//
// 设计约束（spec §四.1 与任务 6 裁决）：
// - 图片卡必须进入视口才读取：每个卡片用 `attach(path, el)` 注册观察，离开视口即释放
//   该消费者；IntersectionObserver 不可用的环境（老 WebView / jsdom）退化为挂载即读，
//   保证图片不因环境缺 API 永久空白。
// - 同会话内同一路径的多个消费者共享同一次请求与同一个 object URL（按 path 归并）；
//   最后一个消费者离开立即 revoke，不保留无消费者的原图缓存。
// - 同时最多两个请求在途，其余排队；完成/失败/中止后立刻补位。
// - 换会话（store 换代）与卸载时中止在途请求并回收全部 URL；迟到结果若资源已被释放
//   或消费者已离开，创建的 object URL 直接 revoke，绝不附着到新视图。
// - 如实登记（Task 6 复审 I1）：布局驱动的重挂载会让所有卡片消费者离开，按「最后一个
//   消费者离开即释放」规则回收可见图片，重建后重新 acquire/读盘。这是继承的 SessionDetail
//   布局结构（split/split-h ↔ fullscreen 换 React 分支）造成的行为，本 hook 按资源规则
//   无法消除；本任务不重构布局，详见 ProjectedArtifacts.tsx 图片卡处的说明。
// - 解码失败（img.onerror）由 `reportDecodeFailure` 回收 URL 并显示占位，不自动重试，
//   也不把失败图留在内存里。
// - 不跨会话复用、不写入任何日志、不代理/内联 base64：读取走既有 `/file` 端点。
import { useEffect, useMemo, useSyncExternalStore } from "react";
import { ApiError, fetchFile } from "./api";

/** 单张图片的资源状态（同会话同路径共享）。 */
export interface ArtifactImageEntry {
  status: "loading" | "ready" | "error";
  /** 可直接放进 <img src> 的 object URL；仅 ready 时非空。 */
  url: string | null;
  /** 占位文案的原因码（由 UI 翻译），非空仅 error 态。 */
  reason: string | null;
}

export interface ArtifactImagesController {
  /** 注册一个视口消费者；返回解除函数（卸载/离开视口时调用）。 */
  attach(path: string, el: Element): () => void;
  /** 当前资源快照；undefined = 尚未进入视口（无消费者）。 */
  stateFor(path: string): ArtifactImageEntry | undefined;
  /** 图片解码失败：回收 object URL 并转入占位。 */
  reportDecodeFailure(path: string): void;
}

/** 同时最多两个缩略图请求在途（spec §四.1 钉值）。 */
export const ARTIFACT_IMAGE_MAX_INFLIGHT = 2;

interface Resource {
  path: string;
  consumers: Set<number>;
  entry: ArtifactImageEntry;
  controller: AbortController | null;
  /** 已占用在途名额（active 集合成员）。 */
  started: boolean;
  /** 本次读取已结束（成功/失败/中止）：不再自动重试。 */
  finished: boolean;
}

function reasonFromError(error: unknown): string {
  if (error instanceof ApiError) {
    const raw = error.data;
    const code =
      raw !== null &&
      typeof raw === "object" &&
      typeof (raw as { error?: unknown }).error === "string"
        ? (raw as { error: string }).error
        : null;
    if (
      code === "sensitive" ||
      code === "not_found" ||
      code === "too_large" ||
      code === "not_file"
    ) {
      return code;
    }
    if (code === "io") return "io";
    // 403 无结构化原因（旧后端/代理页）不可区分具体原因：按通用不可读处理，
    // 不把「可能是删除/越界/超限」中的任意一项冒充为确定结论。
    return "unreadable";
  }
  return "network";
}

interface Store {
  subscribe(listener: () => void): () => void;
  getSnapshot(): number;
  attach(path: string, el: Element): () => void;
  stateFor(path: string): ArtifactImageEntry | undefined;
  reportDecodeFailure(path: string): void;
  dispose(): void;
}

function createStore(sessionId: string): Store {
  const resources = new Map<string, Resource>();
  const observation = new Map<Element, { path: string; token: number | null }>();
  const queue: Resource[] = [];
  const active = new Set<Resource>();
  const listeners = new Set<() => void>();
  let observer: IntersectionObserver | null = null;
  let consumerSeq = 0;
  let version = 0;
  let disposed = false;

  const emit = () => {
    version += 1;
    for (const listener of listeners) listener();
  };

  const release = (path: string, token: number) => {
    const res = resources.get(path);
    if (!res) return;
    res.consumers.delete(token);
    if (res.consumers.size > 0) return;
    // 最后一个消费者离开：取消排队/在途，回收 URL，资源整体移除（无消费者缓存）。
    // 注意：布局驱动的重挂载会让所有消费者同时离开，因此这里会释放全部可见图片，
    // 重建后重新读取（继承的布局行为，见文件头与 ProjectedArtifacts 的说明）。
    resources.delete(path);
    const queuedAt = queue.indexOf(res);
    if (queuedAt >= 0) queue.splice(queuedAt, 1);
    res.controller?.abort();
    res.controller = null;
    if (res.entry.url !== null) URL.revokeObjectURL(res.entry.url);
    res.entry = { status: "loading", url: null, reason: null };
    res.finished = true;
    emit();
  };

  const acquire = (path: string): number => {
    if (disposed) return -1;
    const token = ++consumerSeq;
    let res = resources.get(path);
    if (res === undefined) {
      res = {
        path,
        consumers: new Set(),
        entry: { status: "loading", url: null, reason: null },
        controller: null,
        started: false,
        finished: false,
      };
      resources.set(path, res);
    }
    res.consumers.add(token);
    emit();
    schedule(res);
    return token;
  };

  const finish = (res: Resource) => {
    res.started = false;
    res.finished = true;
    if (active.delete(res)) pump();
  };

  const start = (res: Resource) => {
    if (disposed || res.started || res.finished || res.consumers.size === 0) return;
    if (!resources.has(res.path)) return;
    res.started = true;
    active.add(res);
    const ctrl = new AbortController();
    res.controller = ctrl;
    res.entry = { status: "loading", url: null, reason: null };
    emit();
    fetchFile(sessionId, res.path, undefined, ctrl.signal)
      .then((payload) => {
        const alive =
          !disposed && !res.finished && resources.get(res.path) === res && res.consumers.size > 0;
        if (!alive) {
          // 迟到结果：消费者已离开 / 已换会话 / 已卸载——直接回收，不附着
          if (payload.kind === "image") URL.revokeObjectURL(payload.url);
          return;
        }
        if (payload.kind === "image") {
          res.entry = { status: "ready", url: payload.url, reason: null };
        } else {
          res.entry = { status: "error", url: null, reason: "not_image" };
        }
        emit();
      })
      .catch((error: unknown) => {
        if (ctrl.signal.aborted) return;
        if (disposed || res.finished || resources.get(res.path) !== res) return;
        res.entry = { status: "error", url: null, reason: reasonFromError(error) };
        emit();
      })
      .finally(() => {
        res.controller = null;
        finish(res);
      });
  };

  const pump = () => {
    while (!disposed && active.size < ARTIFACT_IMAGE_MAX_INFLIGHT && queue.length > 0) {
      const next = queue.shift() as Resource;
      if (next.finished || next.started || next.consumers.size === 0) continue;
      if (resources.get(next.path) !== next) continue;
      start(next);
    }
  };

  const schedule = (res: Resource) => {
    if (disposed || res.started || res.finished) return;
    if (res.consumers.size === 0) return;
    if (active.size >= ARTIFACT_IMAGE_MAX_INFLIGHT) {
      if (!queue.includes(res)) queue.push(res);
      return;
    }
    start(res);
  };

  const ensureObserver = (): IntersectionObserver | null => {
    if (observer !== null) return observer;
    if (typeof IntersectionObserver === "undefined") return null;
    observer = new IntersectionObserver((entries) => {
      for (const entry of entries) {
        const rec = observation.get(entry.target);
        if (rec === undefined) continue;
        if (entry.isIntersecting) {
          if (rec.token === null) rec.token = acquire(rec.path);
        } else if (rec.token !== null) {
          const token = rec.token;
          rec.token = null;
          release(rec.path, token);
        }
      }
    });
    return observer;
  };

  return {
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    getSnapshot: () => version,
    stateFor: (path) => resources.get(path)?.entry,
    reportDecodeFailure(path) {
      const res = resources.get(path);
      if (res === undefined) return;
      if (res.entry.url !== null) URL.revokeObjectURL(res.entry.url);
      res.entry = { status: "error", url: null, reason: "decode_failed" };
      emit();
    },
    attach(path, el) {
      // React StrictMode 开发期会对 effect 做「挂载 → 清理 → 再挂载」：dispose 只负责
      // 释放资源，不永久杀死 store——同一 store 重新 attach 时恢复；真正换会话时
      // hook 已换新 store，旧 store 不会再被使用。
      if (disposed) disposed = false;
      const rec: { path: string; token: number | null } = { path, token: null };
      observation.set(el, rec);
      const io = ensureObserver();
      if (io !== null) io.observe(el);
      // 无 IntersectionObserver：退化为挂载即读（老 WebView 不让图片永久空白）
      else rec.token = acquire(path);
      return () => {
        observation.delete(el);
        io?.unobserve(el);
        if (rec.token !== null) {
          const token = rec.token;
          rec.token = null;
          release(path, token);
        }
      };
    },
    dispose() {
      if (disposed) return;
      disposed = true;
      observer?.disconnect();
      observer = null;
      observation.clear();
      queue.length = 0;
      for (const res of resources.values()) {
        res.controller?.abort();
        if (res.entry.url !== null) URL.revokeObjectURL(res.entry.url);
        res.entry = { status: "loading", url: null, reason: null };
        res.finished = true;
        res.started = false;
      }
      resources.clear();
      active.clear();
      emit();
    },
  };
}

/**
 * 会话级附件图片资源控制器。
 *
 * 同一 `SessionDetail` 内所有图片卡共用这一实例，从而真正落实「同会话消费者共享
 * 一次请求 / 一个 object URL」与全局两个在途请求上限。换会话时旧 store 销毁：
 * 中止在途、回收 URL、迟到结果不得回填。
 */
export default function useArtifactImages(sessionId: string): ArtifactImagesController {
  const store = useMemo(() => createStore(sessionId), [sessionId]);
  useEffect(() => () => store.dispose(), [store]);
  // 资源状态变化驱动渲染（store 的版本号是稳定快照）
  useSyncExternalStore(store.subscribe, store.getSnapshot, store.getSnapshot);
  return useMemo(
    () => ({
      attach: store.attach,
      stateFor: store.stateFor,
      reportDecodeFailure: store.reportDecodeFailure,
    }),
    [store]
  );
}
