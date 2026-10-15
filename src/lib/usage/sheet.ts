// 图集几何与加载层（计划② Task 12 步骤 2）——「怎么从图集里裁出一帧」的唯一出处。
// 消费方 = `exportLayout.ts`（裁切框几何 / 姿态行）与 `useExportShare.ts`（取图集一次）。
//
// 三件事：
//  ① `sheetGeometry`：帧几何**从真实图集尺寸推导**（§3 第 31 条）；`FRAME_W`/`FRAME_H` 只作自检基准，
//     不一致只告警、仍按推导值绘制（硬编码 192/208 会在换图集时静默裁错帧）；
//  ② 姿态键族与代表帧（§3 第 32 条）：`poseKeysFor` / `resolvePoseRow` / `poseFrameCount` / `poseCol`；
//     `poseKeysFor(11)` 的键序**即** i18n `usage.pose.*` 的键序（`usage-sheet.test.ts` 用例 5 有绊线）；
//  ③ 图集取用：`activePetSheet`（当前宠物是谁）+ `loadSheet`（**fetch → blob → createImageBitmap**）。
//
// 四条纪律：
//  * **禁止 `<img>` 取位图**（§3 第 30 条）：`asset://` 与 `tauri://` 跨源会污染画布，`toBlob()` 抛
//    `SecurityError`。`probeSheetRows` 的那次 `<img>` 只探行数、**不画**，不污染画布。
//  * 任何一步失败（url 空 / fetch / 解码 / 几何非法）一律 `null`：**不画立绘也要出图**，调用方不因
//    图集不可得而整张失败。
//  * 几何推导不硬编码 192/208：实测 1536×2288/11 与 1536×1872/9 都推出 192×208，但那是**结果**。
//  * `row === 9` 没有 `look` 行：`look` 在 9 行图集上回落第 0 行待机，**不得**画空白格（与
//    `FoxbellPet` 运行时门控对齐）。
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import {
  ANIM,
  FRAME_H,
  FRAME_W,
  SHEET_COLS,
  type PetAnimKey,
} from "@/components/pet/petAnimations";
import {
  builtinPet,
  loadActiveId,
  probeSheetRows,
  type PetRows,
} from "@/components/pet/petRuntime";
import type { PetScan } from "@/components/pet/petValidation";

export type { PetRows };

/** `look` 扫视动画所在行（第 9 行起，16 向跨第 9/10 两行；本层只裁第 9 行） */
export const LOOK_ROW = 9;

/** 帧几何：`frameW × frameH` 由真实图集尺寸推出；`matchesConstants` 只作自检（不一致不影响绘制） */
export interface SheetGeometry {
  frameW: number;
  frameH: number;
  matchesConstants: boolean;
}

/**
 * 图集位图的最小契约：几何推导只读 `width`/`height`，绘制只用 `drawImage` 的源。
 * 收窄成结构类型（而不是 `ImageBitmap`）是为了让 jsdom 假实现能直接替身——本层不需要真位图。
 */
export type SheetBitmap = CanvasImageSource & { readonly width: number; readonly height: number };

/** 取图集的全部外部依赖（可注入：单测传假实现，绝不碰真网络、真解码） */
export interface SheetDeps {
  /** `fetch(assetUrl)`；只取 `blob()`，不读 `ok`——`asset://` / `tauri://` 无标准 CORS 语义，失败以抛错为准 */
  fetch(url: string): Promise<{ blob(): Promise<Blob> }>;
  /** `createImageBitmap(blob)`：**不得**改成 `<img>`（见文件头第一条纪律） */
  createImageBitmap(blob: Blob): Promise<SheetBitmap>;
}

/** 已加载的图集：几何与位图**同源**（同一次 fetch 的产物，不可能一处新一处旧） */
export interface LoadedSheet {
  geometry: SheetGeometry;
  bitmap: SheetBitmap;
}

/**
 * 生产依赖：两个原语都用**调用时求值**的箭头（`fetch` / `createImageBitmap` 在非安全上下文或老
 * WebView 里可能不存在；包在箭头里，缺失会落进 `loadSheet` 的 `try` 而**不是**在构造 deps 时炸）。
 */
export function defaultSheetDeps(): SheetDeps {
  return {
    fetch: (url) => fetch(url),
    createImageBitmap: (blob) => createImageBitmap(blob),
  };
}

/**
 * 图集尺寸 → 帧几何。`frameW = sheetW / cols`、`frameH = sheetH / rows`。
 * 尺寸 ≤ 0 / 非有限 / 非整除 / `rows` 不在 `{9, 11}` / `cols ≤ 0` → `null`（调用方跳过立绘）。
 * ⚠️ 调用方**不得**把 `matchesConstants === false` 当失败：那只是「图集与 `petAnimations.ts` 的
 * 常量不一致」，按推导值照样绘制（只有 192/208 的图集才会 true）。
 */
export function sheetGeometry(
  sheetW: number,
  sheetH: number,
  rows: PetRows,
  cols: number = SHEET_COLS
): SheetGeometry | null {
  if (!Number.isFinite(sheetW) || !Number.isFinite(sheetH) || sheetW <= 0 || sheetH <= 0)
    return null;
  if (!Number.isInteger(cols) || cols <= 0) return null;
  if (rows !== 9 && rows !== 11) return null;
  const frameW = sheetW / cols;
  const frameH = sheetH / rows;
  if (!Number.isInteger(frameW) || !Number.isInteger(frameH)) return null;
  return { frameW, frameH, matchesConstants: frameW === FRAME_W && frameH === FRAME_H };
}

/** 姿态键：`random` 是「未指定」档（设置缺省值），其余与图集行一一对应（`look` 需 11 行图集） */
export type PoseKey = "random" | PetAnimKey;

/** 姿态键族的**唯一**出处（11 键，键序 = i18n `usage.pose.*` 的键序） */
export const POSE_KEYS: readonly PoseKey[] = [
  "random",
  "idle",
  "run-right",
  "run-left",
  "waving",
  "jumping",
  "failed",
  "waiting",
  "running",
  "review",
  "look",
];

/** 某图集行数下可选的姿态键：**`rows === 11` 才含 `look`**（9 行图集没有那一行） */
export function poseKeysFor(rows: PetRows): PoseKey[] {
  return POSE_KEYS.filter((key) => rows === 11 || key !== "look");
}

/**
 * 姿态键 → 图集行号：动作键走该行动画；`look` 仅 11 行图集走第 9 行；
 * 未知键与 9 行图集的 `look` 一律回落第 0 行待机（**不得**画空白格）。
 * 注：设置里的 `random`（未指定）不在本表的动作键内，按未知键口径回落待机行——
 * 本层是纯函数，不掷骰子（随机选行会让同一份输入产出不同图，断言与复现都失去意义）。
 */
export function resolvePoseRow(pose: string, rows: PetRows): number {
  if (pose === "look") return rows === 11 ? LOOK_ROW : ANIM.idle.row;
  const def = ANIM[pose as Exclude<PetAnimKey, "look">];
  return def ? def.row : ANIM.idle.row;
}

/**
 * 该姿态的**代表帧帧数** = 该行动画帧数（`ANIM[key].d.length`；**不是** `SHEET_COLS`）。
 * `look` 只裁第 9 行 → 该行有效列数 = `SHEET_COLS`（16 向跨两行，本层不跨行）。
 * 未知键/`random` → 回落待机行的帧数（与 `resolvePoseRow` 同一套回落）。
 */
export function poseFrameCount(pose: string): number {
  if (pose === "look") return SHEET_COLS;
  const def = ANIM[pose as Exclude<PetAnimKey, "look">];
  return def ? def.d.length : ANIM.idle.d.length;
}

/**
 * 代表帧列 = `min(col, 帧数 - 1)`：跨宠物没有通用「最好看」的列，调用方给的 `col 0` 是安全默认；
 * 越界列必须夹到该行动画的最后一帧（不然裁到的是下一行的画面）。负数与非法值 → 0。
 */
export function poseCol(pose: string, col: number): number {
  const frames = poseFrameCount(pose);
  const c = Number.isFinite(col) ? Math.floor(col) : 0;
  return Math.max(0, Math.min(c, frames - 1));
}

/**
 * 当前激活宠物的图集来源（url + 行数）。
 *
 * **刻意不复用 `resolveActivePet()`**：那个入口顺带快照语音（拉音频 blob URL、写语音能力标记、
 * 派发变更事件），对「导一张图」是纯负担与**跨面副作用**。这里只要一张能 fetch 的图 + 行数：
 *  * 内置宠物 → 静态描述符（零 IPC、零探测）；
 *  * 外部宠物 → 既有 `pet_scan` 目录扫描（4 处同款先例）+ `probeSheetRows` 探行数
 *    （`rowsFromSize` 是行数的唯一判据，§3 第 31 条）。
 * 取不到（扫描失败 / 图集缺失 / 非 9·11 尺寸）→ `null` → 不画立绘也要出图。
 */
export async function activePetSheet(): Promise<{ url: string; rows: PetRows } | null> {
  const id = loadActiveId();
  const builtin = builtinPet(id);
  if (builtin) return { url: builtin.spritesheetUrl, rows: builtin.rows };
  try {
    const scan = await invoke<PetScan>("pet_scan", { id });
    if (!scan.spritesheet.exists) return null;
    const url = convertFileSrc(`${scan.dir}/spritesheet.webp`);
    return { url, rows: await probeSheetRows(url) };
  } catch {
    return null;
  }
}

/**
 * 加载图集：`fetch(assetUrl) → blob → createImageBitmap`，几何**与位图同源**（同一次 fetch 的尺寸）。
 * 任何一步失败 → `null`（调用方跳过立绘、照常出图）。推导几何与常量不一致时**只告警**一次，
 * 仍按推导值绘制（§3 第 31 条：常量只作自检基准）。
 */
export async function loadSheet(
  url: string,
  rows: PetRows,
  deps: SheetDeps = defaultSheetDeps()
): Promise<LoadedSheet | null> {
  if (!url) return null;
  try {
    const blob = await (await deps.fetch(url)).blob();
    const bitmap = await deps.createImageBitmap(blob);
    const geometry = sheetGeometry(bitmap.width, bitmap.height, rows);
    if (!geometry) return null;
    if (!geometry.matchesConstants) {
      console.warn(
        `[usage] 图集帧几何与常量不一致：推导 ${geometry.frameW}×${geometry.frameH}，` +
          `常量 ${FRAME_W}×${FRAME_H}（仍按推导值绘制）`
      );
    }
    return { geometry, bitmap };
  } catch {
    return null;
  }
}
