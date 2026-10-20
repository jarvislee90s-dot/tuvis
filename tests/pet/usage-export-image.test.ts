// Task 12（计划②）· 分享图执行层 `src/lib/usage/exportImage.ts`（5 用例）。
// 判据来源：brief 步骤 5（`ExportDeps = { createCanvas, bitmap }` 注入；**度量遍**只取 `measureText`，
// 画布 `createCanvas(EXPORT_W, 10)`；**绘制遍** `createCanvas(EXPORT_W, layout.height)` → 逐步 execute
// → `toBlob`；`toBlob` 必须包成 Promise 并 **await**；被污染的画布 `toBlob` **同步抛** `SecurityError`
// → 捕获后去掉立绘重绘一张，**绝不整张失败**，此时**绘制遍**的 `createCanvas` 次数 = 2）+ 关键坑第 1 条。
//
// 假桩纪律（brief 关键坑第 1 条）：`toBlob` 的回调经**任务队列异步**触发，假桩必须用 `setTimeout`
// 模拟真实时序——同步返回的假桩会让「没 await」的错误实现全绿，真机必挂。
// 假画布还**如实建模跨源污染**：`taint: true` 时画过立绘的画布 `toBlob` 同步抛 `SecurityError`
// （与浏览器同行为）；`taint: false` 是同源位图（`fetch → createImageBitmap` 的正常路径，不污染）。
// 这样「重绘时没去掉立绘」会被用例 4 抓住，而「正常路径被误判成污染」会被用例 1 抓住。
//
// 纪律：jsdom 没有 canvas 内核（§3 第 33 条），故本用例全程注入假画布；布局用真实的
// `layoutExport` 做对照（值导入 + 类型一起 import）。假上下文把每次 `fill` 的**路径外接框**记下来，
// 于是「轨道画满 w、填充画 w × share」可以核对到像素，而不是只看调用次数。
import { afterEach, describe, expect, it, vi } from "vitest";
import { defaultExportDeps, renderShareImage, type ExportDeps } from "@/lib/usage/exportImage";
import {
  EXPORT_W,
  layoutExport,
  type ExportInput,
  type ExportStep,
  type MeasureText,
} from "@/lib/usage/exportLayout";
import type { SheetBitmap } from "@/lib/usage/sheet";

/** 与布局用例同款的假测量（1 字符 10px）：两遍绘制必须共用同一份测量口径 */
const measure: MeasureText = (text) => text.length * 10;

/** 布局入参：与 `usage-export-layout.test.ts` 同形（这里的关注点是「画得对不对」） */
function input(over: Partial<ExportInput> = {}): ExportInput {
  return {
    brand: "兔维斯 · 用量看板",
    rangeLabel: "近 7 天 · 09/27 – 10/03",
    heroLabel: "近 7 天 Token 合计",
    hero: "2,036,981",
    heroSub: "数据截止时间: 2026-10-03 12:00:00",
    metrics: [
      ["请求输入", "1,954,268"],
      ["缓存命中", "1,200,000"],
      ["未缓存命中输入", "754,268"],
      ["产出", "82,713"],
      ["命中率", "91.2%"],
      ["请求次数", "320"],
    ],
    trendTitle: "用量趋势",
    emptyLabel: "暂无数据",
    points: [
      { label: "09/27", value: 100 },
      { label: "09/28", value: 250 },
      { label: "10/03", value: 400 },
    ],
    groupTitle: "分组分布",
    groups: [{ name: "Claude", value: "1,246,567", share: 0.5 }],
    groupsMore: null,
    toolsTitle: "工具调用",
    tools2x2: [
      ["调用总数", "1,234"],
      ["平均耗时", "2.4 秒"],
      ["次数第一", "Bash 120"],
      ["耗时第一", "Edit 31.2 秒"],
    ],
    quote: "缓存吃得干干净净（命中率 91.2%），省钱小能手！",
    footer: "纯 token · 含子代理 · 本地聚合",
    pose: "waving",
    poseColIndex: 0,
    sheet: { rows: 11, geometry: { frameW: 192, frameH: 208, matchesConstants: true } },
    ...over,
  };
}

/** 一次上色区域的外接框 */
interface Box {
  x: number;
  y: number;
  w: number;
  h: number;
}

interface CtxCall {
  m: string;
  args: unknown[];
}

interface FakeCanvas {
  w: number;
  h: number;
  calls: CtxCall[];
  /** 该画布是否被画过立绘（跨源污染的前提） */
  drewSprite: boolean;
  /** 传给 toBlob 的 MIME（必须显式 image/png） */
  blobType: string | null;
}

/** 假 2D 上下文：只实现执行层真正用到的方法；`fill` 记路径外接框，`fillText` 记 (文本, x, y) */
function fakeCtx(canvas: FakeCanvas): CanvasRenderingContext2D {
  let path: { x: number; y: number }[] = [];
  const method =
    (m: string) =>
    (...args: unknown[]) => {
      if (m === "beginPath") path = [];
      if (m === "moveTo" || m === "lineTo")
        path.push({ x: args[0] as number, y: args[1] as number });
      if (m === "arcTo") {
        path.push({ x: args[0] as number, y: args[1] as number });
        path.push({ x: args[2] as number, y: args[3] as number });
      }
      canvas.calls.push({ m, args });
    };
  const boxOf = (): Box => {
    const xs = path.map((p) => p.x);
    const ys = path.map((p) => p.y);
    if (xs.length === 0) return { x: 0, y: 0, w: 0, h: 0 };
    return {
      x: Math.min(...xs),
      y: Math.min(...ys),
      w: Math.max(...xs) - Math.min(...xs),
      h: Math.max(...ys) - Math.min(...ys),
    };
  };
  const ctx: Record<string, unknown> = {
    fillStyle: "",
    strokeStyle: "",
    lineWidth: 1,
    font: "",
    textAlign: "left",
    globalAlpha: 1,
    lineJoin: "round",
    lineCap: "round",
    fillRect: method("fillRect"),
    beginPath: method("beginPath"),
    moveTo: method("moveTo"),
    lineTo: method("lineTo"),
    arcTo: method("arcTo"),
    arc: method("arc"),
    closePath: method("closePath"),
    stroke: method("stroke"),
    fill: () => canvas.calls.push({ m: "fill", args: [boxOf()] }),
    fillText: method("fillText"),
    measureText: (text: string) => ({ width: text.length * 10 }),
    createLinearGradient: () => ({ addColorStop: method("addColorStop") }),
    drawImage: (...args: unknown[]) => {
      canvas.drewSprite = true;
      canvas.calls.push({ m: "drawImage", args });
    },
  };
  return ctx as unknown as CanvasRenderingContext2D;
}

/** 假画布工厂：`toBlob` 异步回调（setTimeout）+ 污染画布同步抛 SecurityError（真实行为） */
function fakeDeps(opts: { blob: Blob | null; bitmap: SheetBitmap | null; taint?: boolean }): {
  deps: ExportDeps;
  canvases: FakeCanvas[];
} {
  const canvases: FakeCanvas[] = [];
  const deps: ExportDeps = {
    createCanvas: (w, h) => {
      const canvas: FakeCanvas = { w, h, calls: [], drewSprite: false, blobType: null };
      canvases.push(canvas);
      const ctx = fakeCtx(canvas);
      return {
        width: w,
        height: h,
        getContext: (kind: string) => (kind === "2d" ? ctx : null),
        toBlob: (cb: BlobCallback, type?: string) => {
          canvas.blobType = type ?? null;
          // 跨源位图画过的画布：toBlob **同步**抛（浏览器行为；在 executor 内 → await 下变 rejection）
          if (canvas.drewSprite && opts.taint) {
            throw new DOMException("Tainted canvases may not be exported.", "SecurityError");
          }
          // 回调经任务队列异步触发（WKWebView / WebView2 均如此）
          setTimeout(() => cb(opts.blob), 0);
        },
      } as unknown as HTMLCanvasElement;
    },
    bitmap: opts.bitmap,
  };
  return { deps, canvases };
}

const bitmap: SheetBitmap = { width: 1536, height: 2288 } as unknown as SheetBitmap;
const pngBlob = new Blob([new Uint8Array([1, 2, 3])], { type: "image/png" });

/** 文本基线序列（与布局的 text 步骤一一对应） */
const textYs = (c: FakeCanvas): number[] =>
  c.calls.filter((call) => call.m === "fillText").map((call) => call.args[2] as number);
const layoutTextYs = (steps: ExportStep[]): number[] =>
  steps.filter((s) => s.kind === "text").map((s) => (s as { y: number }).y);
/** 全部「已上色区域」：`fillRect` 的直接入参 + `fill` 的路径外接框 */
const fillBoxes = (c: FakeCanvas): Box[] =>
  c.calls.flatMap((call) => {
    if (call.m === "fillRect") {
      return [
        {
          x: call.args[0] as number,
          y: call.args[1] as number,
          w: call.args[2] as number,
          h: call.args[3] as number,
        },
      ];
    }
    return call.m === "fill" ? [call.args[0] as Box] : [];
  });

afterEach(() => {
  vi.restoreAllMocks();
});

describe("exportImage（计划② Task 12：分享图执行层）", () => {
  it("1. 度量遍 (720×10) + 绘制遍 (720×height)：尺寸、MIME、步骤序与布局逐条一致", async () => {
    const src = input();
    // V2：分享图 layout 入参不得包含用户输入估值
    expect(src.metrics.map(([label]) => label)).not.toContain("用户输入(估) · 含子代理");
    const layout = layoutExport(src, measure);
    const { deps, canvases } = fakeDeps({ blob: pngBlob, bitmap });

    const blob = await renderShareImage(src, deps);

    expect(canvases.map((c) => [c.w, c.h])).toEqual([
      [EXPORT_W, 10],
      [EXPORT_W, layout.height],
    ]);
    expect(blob).toBe(pngBlob);
    expect(canvases[1].blobType).toBe("image/png");

    // 文本基线序列 = 布局里 text 步骤的 y 序列（结构上不可能分歧）
    expect(textYs(canvases[1])).toEqual(layoutTextYs(layout.steps));

    // 立绘：drawImage 的源框/目标框 = 布局 sprite 步骤（(col×fw, row×fh, fw, fh) → (petX, petY, petW, 360)）
    const sprite = layout.steps.find((s) => s.kind === "sprite") as Extract<
      ExportStep,
      { kind: "sprite" }
    >;
    const drawn = canvases[1].calls.filter((c) => c.m === "drawImage");
    expect(drawn).toHaveLength(1);
    expect(drawn[0].args).toEqual([
      bitmap,
      sprite.sx,
      sprite.sy,
      sprite.sw,
      sprite.sh,
      sprite.x,
      sprite.y,
      sprite.w,
      sprite.h,
    ]);

    // 占比条：轨道画满 w、填充画 w × share（执行层才算，轨道恒 608——写成「填充宽」会平方）
    const bar = layout.steps.find((s) => s.kind === "bar") as Extract<ExportStep, { kind: "bar" }>;
    const boxes = fillBoxes(canvases[1]);
    expect(boxes).toContainEqual({ x: bar.x, y: bar.y, w: bar.w, h: bar.h });
    expect(boxes).toContainEqual({ x: bar.x, y: bar.y, w: bar.w * bar.share, h: bar.h });

    // 生产 deps：画布尺寸必须被真的设上（jsdom 无 2d 内核，只核对属性）
    const real = defaultExportDeps(bitmap).createCanvas(EXPORT_W, 1234);
    expect([real.width, real.height]).toEqual([EXPORT_W, 1234]);
  });

  it("2. `toBlob` 回调是**异步**触发：不在同一 tick 结算（同步返回的假桩会放过没 await 的实现）", async () => {
    const { deps } = fakeDeps({ blob: pngBlob, bitmap });
    let settled = false;
    const p = renderShareImage(input(), deps).then((b) => {
      settled = true;
      return b;
    });
    expect(settled).toBe(false); // 回调还没轮到
    const blob = await p;
    expect(settled).toBe(true);
    expect(blob.type).toBe("image/png");
    expect(blob.size).toBe(3);
  });

  it("3. `toBlob` 回调给 null → 以固定文案 reject（不得把 null 当图交出去，也不得乱重绘）", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const src = input();
    const { deps, canvases } = fakeDeps({ blob: null, bitmap });
    await expect(renderShareImage(src, deps)).rejects.toThrow("canvas toBlob returned null");
    // 回调给 null ≠ 画布被污染：**不重绘**（重绘只会再失败一次，还会把原因说成「污染」）
    expect(canvases.filter((c) => c.h !== 10)).toHaveLength(1);
    expect(warn).not.toHaveBeenCalled();
  });

  it("4. 画布被污染（`toBlob` 同步抛 SecurityError）→ 去掉立绘重绘一张，绝不整张失败", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const src = input();
    const layout = layoutExport(src, measure);
    const { deps, canvases } = fakeDeps({ blob: pngBlob, bitmap, taint: true });

    const blob = await renderShareImage(src, deps);

    expect(blob).toBe(pngBlob);
    // **绘制遍**两次（首绘 + 去立绘重绘）；度量那张（高 10）不计入
    const drawPasses = canvases.filter((c) => c.h === layout.height);
    expect(drawPasses).toHaveLength(2);
    expect(drawPasses[0].drewSprite).toBe(true);
    expect(drawPasses[1].drewSprite).toBe(false); // 重绘必须**没有**立绘
    expect(drawPasses[1].calls.filter((c) => c.m === "drawImage")).toHaveLength(0);
    // 重绘不是残图：文本与条照旧全画
    expect(textYs(drawPasses[1])).toEqual(layoutTextYs(layout.steps));
    const bar = layout.steps.find((s) => s.kind === "bar") as Extract<ExportStep, { kind: "bar" }>;
    expect(fillBoxes(drawPasses[1])).toContainEqual({ x: bar.x, y: bar.y, w: bar.w, h: bar.h });
    expect(warn).toHaveBeenCalledTimes(1);
  });

  it("5. `bitmap = null`：sprite 步骤被跳过，其余步骤一个不少（不画立绘也要出图）", async () => {
    const src = input();
    const layout = layoutExport(src, measure);
    const { deps, canvases } = fakeDeps({ blob: pngBlob, bitmap: null });

    const blob = await renderShareImage(src, deps);

    expect(blob).toBe(pngBlob);
    const drawn = canvases[1];
    expect(drawn.calls.filter((c) => c.m === "drawImage")).toHaveLength(0);
    expect(textYs(drawn)).toEqual(layoutTextYs(layout.steps));
    // 布局里确实**有** sprite 步骤（不是布局没画，而是执行层按 bitmap 跳过）
    expect(layout.steps.some((s) => s.kind === "sprite")).toBe(true);
  });
});
