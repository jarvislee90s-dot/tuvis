// 本地待发队列的纯状态规则（Q6）：版本单调、20 字素摘要、恢复快照查找。
// 只消费服务端 LocalQueueView；不在这里拼官方信封，也不发网络请求。
import type { LocalQueueView } from "@/lib/api/zcodeControl";

/** 20 个字素（grapheme）摘要；缺 Segmenter 的旧 WebView 回退到码点，绝不按 UTF-16 截断代理对。 */
export function zcodeQueuePreview(text: string, limit = 20): string {
  const normalized = text.replace(/\s+/g, " ").trim();
  if (normalized.length === 0) return "";
  const graphemes: string[] = [];
  const Segmenter = (
    Intl as unknown as {
      Segmenter?: new (
        locale?: string,
        options?: { granularity: string }
      ) => { segment: (s: string) => Iterable<{ segment: string }> };
    }
  ).Segmenter;
  if (Segmenter !== undefined) {
    for (const part of new Segmenter(undefined, { granularity: "grapheme" }).segment(normalized)) {
      graphemes.push(part.segment);
      if (graphemes.length > limit) break;
    }
  } else {
    graphemes.push(...Array.from(normalized));
  }
  return graphemes.length > limit ? `${graphemes.slice(0, limit).join("")}…` : graphemes.join("");
}

/**
 * 合并本地队列视图：只接受本会话、版本不倒退的快照。
 * 同版本以后到者为准（服务端同一版本不应出现内容差异；以后到者纠偏）。
 */
export function mergeLocalQueue(
  current: LocalQueueView | null,
  incoming: LocalQueueView
): LocalQueueView {
  if (current === null || incoming.sessionId !== current.sessionId) return incoming;
  if (incoming.version < current.version) return current;
  return incoming;
}

/** 从 recoverableEdits 找指定请求；仅返回引用，全文另取。 */
export function findRecoverableEdit(
  view: LocalQueueView | null,
  requestId: string
): LocalQueueView["recoverableEdits"][number] | null {
  return view?.recoverableEdits.find((edit) => edit.requestId === requestId) ?? null;
}
