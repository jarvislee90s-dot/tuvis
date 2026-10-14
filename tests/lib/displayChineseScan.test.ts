// 上屏中文扫描（#123/#150/#157 错误码迁移批的回归锁）。
//
// 纪律：**桌面端**组件/页面的字符串字面量不得再出现新的硬编码中文（显示文案走
// t()，错误走错误码/formatInvokeError）；**mobile/ 与既有登记面**尚未完成 i18n
// 改造（#157 显式追踪），进白名单——白名单是边界快照不是永久豁免，mobile 改造
// 完成后应整目录移除并在本测试收紧。
import { readFileSync, readdirSync, statSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

const ROOT = process.cwd();
const SRC = path.join(ROOT, "src");

/** 目录级白名单（改造中的面，#157 追踪） */
const DIR_WHITELIST = ["mobile"];
/** 文件级白名单：mock 数据 / 有意单语 / 已登记待迁的桌面残留 */
const FILE_WHITELIST = new Set([
  "tauri-mock.ts", // 浏览器/Playwright 渲染 mock（不上真机）
  "lib/usage/mockFixtures.ts", // mock 数据
  "lib/usage/sheet.ts", // 导出 Excel 表头与日志（导出物内容双语另议 #157）
  "lib/usage/quotePool.ts", // 用量看板语录（有意中文语料，双语另议 #157）
  "lib/usage/format.ts", // 万/亿 单位格式化（显示面，登记待迁）
  "lib/schemas/manifest.ts", // 权限标签表（显示面，登记待迁）
  "lib/usage/useExportShare.ts", // 分享图品牌串（品牌名有意）
  "components/pet/petStatus.ts", // 状态标签表（显示面，登记待迁）
  "components/common/updater-dialog.tsx", // 单条「图片未能加载」（登记待迁）
  "components/settings/RemoteAppearanceSection.tsx", // 角标「改」（登记待迁）
  "components/common/language-toggle.tsx", // 「Switch to 中文」按钮自身（有意）
  "hooks/useNotification.ts", // 系统通知 action 标题（登记待迁）
]);
/** 逐行豁免（文件 → 行号理由表）：日志行不上屏 */
const LINE_WHITELIST: Record<string, (line: number) => boolean> = {
  // pet 快照导出内部 throw（被 catch 落 console，不上 UI；登记 #157 待随 pet 面处理）
  "components/pet/petRuntime.ts": (l) => l === 214,
};

const CJK = /["'`][^"'`\n]*[\u4e00-\u9fff][^"'`\n]*["'`]/;

function walk(dir: string, files: string[] = []) {
  for (const f of readdirSync(dir)) {
    const p = path.join(dir, f);
    const rel = path.relative(SRC, p).replace(/\\/g, "/");
    if (statSync(p).isDirectory()) {
      if (DIR_WHITELIST.includes(f)) continue;
      walk(p, files);
    } else if (/\.(tsx?|ts)$/.test(f)) {
      files.push(rel);
    }
  }
  return files;
}

/** 剥注释：整行注释/块注释行/JSX 行内注释清空；行内 // 仅当前有空白时剥（避 https://） */
function stripComments(line: string): string {
  const t = line.trimStart();
  if (t.startsWith("//") || t.startsWith("*") || t.startsWith("/*")) return "";
  return line
    .replace(/\{\/\*[\s\S]*?\*\/\}/g, "")
    .replace(/\s\/\/.*$/, "");
}

describe("上屏中文扫描（桌面零容忍 / mobile 白名单）", () => {
  it("桌面端源码的字符串字面量不得含硬编码中文", () => {
    const files = walk(SRC);
    expect(files.length).toBeGreaterThan(100); // 自检：扫描确实覆盖了源码树
    const offenders: string[] = [];
    for (const rel of files) {
      if (FILE_WHITELIST.has(rel)) continue;
      const src = readFileSync(path.join(SRC, rel), "utf8");
      // 块注释状态机（多行 /* … */ 内的行不带 * 前缀，逐行判不出——FoxbellPet 实证）
      let inBlock = false;
      let consoleDepth = 0; // 跨行 console.( 调用的括号深度（日志串不上屏）
      src.split("\n").forEach((raw, i) => {
        let line = raw;
        if (inBlock) {
          const end = line.indexOf("*/");
          if (end < 0) return; // 块注释体内，整行跳过
          line = line.slice(end + 2);
          inBlock = false;
        }
        // 行内可能再开块注释：截掉 /* 之后的部分（含跨行开启）
        const open = line.indexOf("/*");
        if (open >= 0) {
          const close = line.indexOf("*/", open);
          if (close < 0) {
            inBlock = true;
            line = line.slice(0, open);
          } else {
            line = line.slice(0, open) + line.slice(close + 2);
          }
        }
        line = stripComments(line);
        if (/console\.(warn|error|log|info|debug)\(/.test(line)) {
          // 日志不上屏：跨行调用用括号深度跟踪（初值=本行净括号，单行闭合即归零）
          consoleDepth = (line.match(/\(/g) ?? []).length - (line.match(/\)/g) ?? []).length;
          return;
        }
        if (consoleDepth > 0) {
          consoleDepth += (line.match(/\(/g) ?? []).length - (line.match(/\)/g) ?? []).length;
          if (consoleDepth <= 0) consoleDepth = 0;
          return;
        }
        if (LINE_WHITELIST[rel]?.(i + 1)) return;
        if (CJK.test(line)) offenders.push(`${rel}:${i + 1}: ${line.trim().slice(0, 60)}`);
      });
    }
    expect(
      offenders,
      `桌面端发现硬编码中文（新文案请走 t()；错误走错误码——见 src-tauri/src/errcode.ts 约定；\n` +
        `若确属白名单面请在 FILE_WHITELIST 登记理由并同步 #157）：\n${offenders.join("\n")}`
    ).toEqual([]);
  });
});
