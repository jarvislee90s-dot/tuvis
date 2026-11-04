// 排队项**展示用**预览（W4 / 计划 §5.1、§5.2）：按「用户可见字素」截断到 20。
//
// 纪律（重要）：
// - 本函数产物**只用于展示**（行内预览 + `title`）。任何提交载荷（`queueDelete` /
//   `queueReorder` / `queueNow` / `queueResume` / `send`）都只接受官方 `inputId` 与
//   **完整原文** `item.text`；截断后的字符串一旦上行就是「改了用户的话」，绝不允许。
// - 按字素（grapheme）而非 code point 截断：`[...text].slice(0, 20)` 会把 emoji ZWJ
//   序列（👨‍👩‍👧‍👦 是 7 个 code point）与组合字符（`e` + U+0301）从中间切开。
// - 目标 WebView 若缺 `Intl.Segmenter`：回退 code point（保守，不引入分词依赖）。
// - 换行/连续空白折叠为单个空格：预览是**一行**，正文原文仍在 `item.text`。

/** `Intl.Segmenter` 的最小结构（ES2022 lib 才带类型，tsconfig lib 是 ES2020）。 */
interface GraphemeSegment {
  segment: string;
}

interface GraphemeSegmenter {
  segment(input: string): Iterable<GraphemeSegment>;
}

type SegmenterCtor = new (
  locales?: string | string[],
  options?: { granularity: "grapheme" }
) => GraphemeSegmenter;

/**
 * 取一个字素分段器；运行时缺失/构造失败 → null（调用方回退 code point）。
 *
 * 每次调用都现取构造器（不缓存实例）：测试需要能在同一进程里模拟「无 Segmenter」
 * 的回退分支，缓存会把两种环境粘在一起。队列长度是个位数，构造开销可忽略。
 */
function graphemeSegmenter(): GraphemeSegmenter | null {
  const ctor = (Intl as unknown as { Segmenter?: SegmenterCtor }).Segmenter;
  if (typeof ctor !== "function") return null;
  try {
    return new ctor(undefined, { granularity: "grapheme" });
  } catch {
    return null;
  }
}

/** 文本 → 用户可见字素数组（无 `Intl.Segmenter` 时按 code point 回退）。 */
export function graphemes(text: string): string[] {
  const segmenter = graphemeSegmenter();
  return segmenter !== null
    ? Array.from(segmenter.segment(text), (part) => part.segment)
    : Array.from(text);
}

/**
 * 单行预览：空白折叠 → 按字素截断到 `limit`（超出加 `…`）。
 *
 * `limit <= 0` 返回空串（调用方不会这样用，但纯函数不抛）。
 * **仅供展示**：绝不用作任何动作载荷（见文件头）。
 */
export function queuePreview(text: string, limit = 20): string {
  if (limit <= 0) return "";
  const oneLine = text.replace(/\s+/gu, " ").trim();
  const units = graphemes(oneLine);
  if (units.length <= limit) return oneLine;
  return `${units.slice(0, limit).join("")}…`;
}
