// issue #160 · 漏传兜底网：src/ 内 fmtTokens/fmtDur/fmtLongest 只允许「显式传 locale（≥2 实参）
// 的调用」出现——单参调用与函数引用传递（如 UsageMiniBar 的 miniBarToolLine(dash.rows, limit, fmtTokens)）
// 都会让 en 界面出中文单位。纯文本扫描（node 上下文，无 jsdom 依赖），路径解析与
// usage-theme-tokens.test.ts 同法（process.cwd()）。
import { readFileSync, readdirSync, statSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

const SRC_ROOT = path.join(process.cwd(), "src");
const FNS = ["fmtTokens", "fmtDur", "fmtLongest"] as const;

/** 匹配串 → 等量换行：剥除时不塌行，lineOf 才能报真实文件行号（第二轮 P1-A——
 *  原实现把跨行块注释折成单空格、多行 import 吃掉内部换行，行号系统偏差 7–23 行，
 *  复查实跑 UsageMiniBar.tsx 报 99/112/113/120/144 vs 真实 109/122/123/130/154） */
const keepLf = (m: string) => "\n".repeat((m.match(/\n/g) ?? []).length);

/** 剥块注释与行注释（保护 "http://" 形态的冒号前缀），换行如数回填 */
function stripComments(src: string): string {
  return src.replace(/\/\*[\s\S]*?\*\//g, keepLf).replace(/(^|[^:])\/\/[^\n]*/g, "$1");
}

/** 剥整条 import 语句（单行与多行都收；import 名单不算出现点），换行如数回填 */
function stripImports(src: string): string {
  return src.replace(/import[\s\S]*?from\s+["'][^"']+["'];?/g, keepLf);
}

function listTsFiles(dir: string): string[] {
  const out: string[] = [];
  for (const name of readdirSync(dir)) {
    const p = path.join(dir, name);
    if (statSync(p).isDirectory()) out.push(...listTsFiles(p));
    else if (/\.(ts|tsx)$/.test(name)) out.push(p);
  }
  return out;
}

function lineOf(src: string, idx: number): number {
  return src.slice(0, idx).split("\n").length;
}

/** 违规清单：`相对路径:行 简述`（跨行调用按起始行计） */
function offenders(): string[] {
  const hits: string[] = [];
  for (const file of listTsFiles(SRC_ROOT)) {
    const rel = path.relative(SRC_ROOT, file).split(path.sep).join("/");
    if (rel === "lib/usage/format.ts") continue; // 定义本体（签名自身即双参）
    const src = stripImports(stripComments(readFileSync(file, "utf8")));
    const re = new RegExp(`\\b(${FNS.join("|")})\\b`, "g");
    for (const m of src.matchAll(re)) {
      const after = src.slice(m.index + m[0].length);
      if (!/^\s*\(/.test(after)) {
        hits.push(
          `${rel}:${lineOf(src, m.index)} ${m[1]} 以函数引用形态出现（须改为 locale 绑定闭包）`
        );
        continue;
      }
      let depth = 0;
      let commas = 0;
      for (const ch of after) {
        if (ch === "(") depth++;
        else if (ch === ")") {
          depth--;
          if (depth === 0) break;
        } else if (depth === 1 && ch === ",") commas++;
      }
      if (commas < 1) hits.push(`${rel}:${lineOf(src, m.index)} ${m[1]}() 单参调用（缺 locale）`);
    }
  }
  return hits;
}

describe("usage locale 扫描（issue #160 兜底网）", () => {
  it(
    "src/ 内三函数的全部出现点都显式传 locale（无单参调用、无裸引用传递）",
    { timeout: 20000 },
    () => {
      expect(offenders()).toEqual([]);
    }
  );
});
