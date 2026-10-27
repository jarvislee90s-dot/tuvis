// 分享图执行层（计划② Task 12 步骤 5）——把 `exportLayout` 的步骤列表**照本宣科**地画到 canvas 并出 PNG。
// 消费方 = `useExportShare.ts`（注入生产 deps）。本文件是**全仓唯一允许硬编码色值**的用量模块之一
// （另一处是 `exportLayout.ts`），**不读任何 CSS 变量**（`usage-theme-tokens.test.ts` 用例 4 反向断言）。
//
// 两遍绘制（brief 步骤 5）：
//  ① **度量遍**：只建一张 `createCanvas(EXPORT_W, 10)` 当尺子，`ctx.measureText` 供 `layoutExport` 算
//     气泡宽与换行——这一遍不画任何东西；
//  ② **绘制遍**：`createCanvas(EXPORT_W, layout.height)` → 逐步 `execute` → `toBlob`。
// 高度由布局**一次算定**（`layoutExport` 返回 `height`），两遍步进结构上不可能分歧。
//
// 三条硬约束：
//  * `toBlob` 的回调经**任务队列异步**触发（WKWebView / WebView2 均如此）⇒ 必须包成 Promise 并 await；
//    写成「同步调 → 立刻读回调写入的变量」在真机上恒为 `null`。
//  * 被跨源位图污染的画布，`toBlob` **同步抛** `SecurityError`（发生在 executor 内 ⇒ await 下自动转
//    rejection）⇒ 捕获后 `paint(null)` **去掉立绘重绘一张**，绝不整张失败（此时绘制遍的
//    `createCanvas` 调用次数 = 2）。
//  * 任何一步的坐标都来自步骤数据：本层不做投影、不做换行、不做格式化（那是布局层与装配层的事）。
import { ACCENT_A, ACCENT_B, EXPORT_W, layoutExport } from "@/lib/usage/exportLayout";
import type { ExportInput, ExportLayout, ExportStep, MeasureText } from "@/lib/usage/exportLayout";
import type { SheetBitmap } from "@/lib/usage/sheet";

/** 执行层外部依赖（**可注入**：单测传假画布，绝不碰真 canvas —— jsdom 没有 2d 内核） */
export interface ExportDeps {
  /** 建画布（度量遍与每次绘制遍各调一次；尺寸由本层给定） */
  createCanvas(w: number, h: number): HTMLCanvasElement;
  /** 图集位图：`null` = 无立绘（图集加载失败 / 去立绘重绘）；只有 `sprite` 步骤读它 */
  bitmap: SheetBitmap | null;
}

/** 生产依赖：真 DOM 画布 + 调用方给的位图（尺寸必须显式设上——`canvas.width` 默认 300×150） */
export function defaultExportDeps(bitmap: SheetBitmap | null): ExportDeps {
  return {
    createCanvas: (w, h) => {
      const canvas = document.createElement("canvas");
      canvas.width = w;
      canvas.height = h;
      return canvas;
    },
    bitmap,
  };
}

/** 圆角矩形路径（自绘 `arcTo`：不依赖 `roundRect`——WKWebView / WebView2 的覆盖度不齐） */
function roundRectPath(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  w: number,
  h: number,
  r: number
): void {
  const rr = Math.max(0, Math.min(r, w / 2, h / 2));
  ctx.beginPath();
  ctx.moveTo(x + rr, y);
  ctx.arcTo(x + w, y, x + w, y + h, rr);
  ctx.arcTo(x + w, y + h, x, y + h, rr);
  ctx.arcTo(x, y + h, x, y, rr);
  ctx.arcTo(x, y, x + w, y, rr);
  ctx.closePath();
}

/** 画一个步骤（纯执行：所有几何、文案、颜色都在步骤里）。`bitmap = null` 时 `sprite` 直接跳过。 */
function execute(
  ctx: CanvasRenderingContext2D,
  step: ExportStep,
  bitmap: SheetBitmap | null
): void {
  switch (step.kind) {
    case "rect": {
      // 方角走 fillRect、圆角走自绘路径（`roundRect` 的 WebView 覆盖度不齐，见文件头）
      if (step.r) {
        roundRectPath(ctx, step.x, step.y, step.w, step.h, step.r);
        ctx.fillStyle = step.color;
        ctx.fill();
      } else {
        ctx.fillStyle = step.color;
        ctx.fillRect(step.x, step.y, step.w, step.h);
      }
      if (step.stroke) {
        roundRectPath(ctx, step.x, step.y, step.w, step.h, step.r ?? 0);
        ctx.strokeStyle = step.stroke;
        ctx.lineWidth = 1;
        ctx.stroke();
      }
      return;
    }
    case "line": {
      ctx.beginPath();
      ctx.moveTo(step.x1, step.y1);
      ctx.lineTo(step.x2, step.y2);
      ctx.strokeStyle = step.color;
      ctx.lineWidth = step.w;
      ctx.stroke();
      return;
    }
    case "text": {
      ctx.font = step.font;
      ctx.fillStyle = step.color;
      ctx.textAlign = step.align ?? "left";
      ctx.fillText(step.text, step.x, step.y);
      return;
    }
    case "bar": {
      // 轨道恒为 `w`（608），填充宽 = `w × share`——**执行层**才算（把 w 定义成填充宽会平方）
      const draw = (w: number, color: string): void => {
        if (w <= 0) return;
        if (step.r) {
          roundRectPath(ctx, step.x, step.y, w, step.h, step.r);
          ctx.fillStyle = color;
          ctx.fill();
        } else {
          ctx.fillStyle = color;
          ctx.fillRect(step.x, step.y, w, step.h);
        }
      };
      draw(step.w, step.track);
      draw(step.w * step.share, step.fill);
      // 描边样式（二轮实机反馈 ②：费用段淡紫填充 + 紫描边）：沿**填充条**同一圆角路径
      // 描一圈（同一 r，路径与填充逐像素重合）；描边不改变几何，布局层高度不受影响。
      // save/restore 包夹（Batch 5 补充，评审 Nit）：lineJoin/lineWidth 是同一 ctx 上的
      // 共享状态，不复位会流进后续步骤
      if (step.stroke && step.w * step.share > 0) {
        ctx.save();
        roundRectPath(ctx, step.x, step.y, step.w * step.share, step.h, step.r ?? 0);
        ctx.strokeStyle = step.stroke;
        ctx.lineWidth = 1.5;
        ctx.lineJoin = "round";
        ctx.stroke();
        ctx.restore();
      }
      return;
    }
    case "trend": {
      if (step.pts.length === 0) return;
      const grad = ctx.createLinearGradient(step.x, 0, step.x + step.w, 0);
      grad.addColorStop(0, ACCENT_A);
      grad.addColorStop(1, ACCENT_B);
      const baseY = step.y + step.h;
      // 面积（12% 透明度，与趋势卡同观感）
      ctx.beginPath();
      ctx.moveTo(step.pts[0].x, baseY);
      for (const p of step.pts) ctx.lineTo(p.x, p.y);
      ctx.lineTo(step.pts[step.pts.length - 1].x, baseY);
      ctx.closePath();
      ctx.globalAlpha = 0.12;
      ctx.fillStyle = grad;
      ctx.fill();
      ctx.globalAlpha = 1;
      // 折线
      ctx.beginPath();
      step.pts.forEach((p, i) => (i === 0 ? ctx.moveTo(p.x, p.y) : ctx.lineTo(p.x, p.y)));
      ctx.strokeStyle = grad;
      ctx.lineWidth = 3;
      ctx.lineJoin = "round";
      ctx.lineCap = "round";
      ctx.stroke();
      // 数据点（白心 + 渐变描边；末点略大）
      step.pts.forEach((p, i) => {
        ctx.beginPath();
        ctx.arc(p.x, p.y, i === step.pts.length - 1 ? 4.5 : 3.2, 0, Math.PI * 2);
        ctx.fillStyle = "#ffffff";
        ctx.fill();
        ctx.strokeStyle = grad;
        ctx.lineWidth = 1.6;
        ctx.stroke();
      });
      return;
    }
    case "sprite": {
      if (!bitmap) return; // 无位图 = 不画立绘（**不是**失败）
      ctx.drawImage(bitmap, step.sx, step.sy, step.sw, step.sh, step.x, step.y, step.w, step.h);
      return;
    }
  }
}

/** 画一遍：新画布 + 逐步执行（`bitmap = null` 的这一遍就是「去掉立绘重绘」） */
function paint(
  deps: ExportDeps,
  layout: ExportLayout,
  bitmap: SheetBitmap | null
): HTMLCanvasElement {
  const canvas = deps.createCanvas(EXPORT_W, layout.height);
  const ctx = canvas.getContext("2d");
  // 真机上不可能走到（`getContext("2d")` 恒可用）；写清楚是为了不把失败伪装成一张空白图
  if (!ctx) throw new Error("canvas 2d context unavailable");
  for (const step of layout.steps) execute(ctx, step, bitmap);
  return canvas;
}

/** `toBlob` → Promise（回调**异步**触发；`null` 一律当失败，绝不放行半张图） */
function toBlobPng(canvas: HTMLCanvasElement): Promise<Blob> {
  return new Promise<Blob>((resolve, reject) => {
    canvas.toBlob(
      (blob) => (blob ? resolve(blob) : reject(new Error("canvas toBlob returned null"))),
      "image/png"
    );
  });
}

/** 判「跨源位图污染」：`toBlob` 同步抛的 `SecurityError`（各家 WebView 可能给 DOMException 或 Error，故只认 `name`） */
function isSecurityError(e: unknown): boolean {
  return typeof e === "object" && e !== null && (e as { name?: unknown }).name === "SecurityError";
}

/**
 * 出图：度量一遍（尺子）→ 画一遍 → `toBlob`；**只有**画布被跨源位图污染时才去掉立绘重绘一次。
 * 返回 PNG `Blob`（落盘 / 剪贴板两条通路由调用方决定，本层不碰 IPC）。
 */
export async function renderShareImage(input: ExportInput, deps: ExportDeps): Promise<Blob> {
  // ① 度量遍：只当尺子（`measureText`），不画
  const meter = deps.createCanvas(EXPORT_W, 10);
  const meterCtx = meter.getContext("2d");
  const measure: MeasureText = (text, font) => {
    if (!meterCtx) return 0;
    meterCtx.font = font;
    return meterCtx.measureText(text).width;
  };

  // ② 绘制遍：高度由布局**一次算定**，这一遍（含污染重绘）只执行同一份步骤 —— 两遍不可能分歧
  const layout = layoutExport(input, measure);
  try {
    return await toBlobPng(paint(deps, layout, deps.bitmap));
  } catch (e) {
    // 跨源位图污染画布时 `toBlob` 同步抛 SecurityError：去掉立绘重绘一张，**绝不整张失败**。
    // 其它失败（如回调给 null）**不重绘**：重绘只会再失败一次，还会把原因说成「污染」（日志失真）。
    if (!isSecurityError(e)) throw e;
    console.warn("[usage] 分享图立绘污染画布，已按无立绘重绘", e);
    return await toBlobPng(paint(deps, layout, null));
  }
}
