#!/usr/bin/env node
// =============================================================================
// check-error-codes.mjs —— 错误码 ⇄ locale 键 双向对账（#123/#150/#157 全量迁移）
// =============================================================================
// 约定全文见 src-tauri/src/errcode.rs（与本脚本互为镜像）：
//   Rust 错误串前缀码 `CODE: ...` / `CODE:param: ...`；locale 键 = errors.camelCase(CODE)。
// 本脚本做三件事（任一失败非零退出）：
//   ① 扫 src-tauri/src 生产代码提取全部使用的码（coded()/coded_param() 调用与
//     `"CODE: ..."` 字面前缀，剔除 #[cfg(test)] 模块）；
//   ② 对账 zh/en 两份 locale：每个码必须有双键；键存在而 Rust 无码 → 孤儿键报错
//     （防拼写漂移与删除域后忘删键）；
//   ③ legacy 白名单（W5_* / PRESET_* 等既有别名键）显式登记，不走 camelCase 推导。
import { readFileSync, readdirSync, statSync } from "node:fs";
import path from "node:path";

const ROOT = process.cwd();
const SRC = path.join(ROOT, "src-tauri/src");

// legacy：既有别名码（码名 ≠ 键名，invokeError.ts 显式映射），不走 camelCase 对账
const LEGACY_CODES = new Set([
  "W5_TOOL_DISABLED", // → errors.toolDisabled
  "W5_APPLY_IN_PROGRESS", // → errors.applyInProgress
  "W5_APPLY_TASK_FAILED", // → errors.applyTaskFailed
]);
// 非 error 码的同形前缀（不是错误，是 marker 协议），显式排除
const EXCLUDE_CODES = new Set(["TUVIS"]); // window/disambig.rs 的标题 marker 前缀
const LEGACY_KEYS = new Set([
  "toolDisabled",
  "applyInProgress",
  "applyTaskFailed",
  "presetActive",
  "presetNativeInUniversal",
]);

const camel = (code) =>
  code
    .toLowerCase()
    .split("_")
    .map((w, i) => (i === 0 ? w : w.charAt(0).toUpperCase() + w.slice(1)))
    .join("");

// ---- ① 提取 Rust 使用的码 ----
const codes = new Set();
const CODED_CALL = /\bcoded(?:_param)?\(\s*"([A-Z][A-Z0-9_]+)"/g;
// 任意字符串字面前缀形态（Err(/format!/Some(/map_err 一网打尽——只要引号内以码+冒号开头）
const CODED_LITERAL = /"([A-Z][A-Z0-9_]{2,}):/g;

function stripComments(src) {
  // 只剥【行】注释（行内注释仅当 // 前有空白——避开字符串里的 https://）。
  // 不剥块注释：本库注释里大量 `monitor/*_parser.rs`、`projects/*/` 形态的 /* 与 */
  // 会被块注释正则配对吞掉中间的真代码（content.rs 实证——六处码凭空消失）；
  // 真有 /* */ 内示例码被误报时登记 EXCLUDE_CODES 即可。
  return src
    .split("\n")
    .map((l) => (/^\s*\/\//.test(l) ? "" : l.replace(/\s\/\/.*$/, "")))
    .join("\n");
}

function stripTests(src) {
  // 剔除 #[cfg(test)] mod tests { ... }（到文件尾或下一个顶层项的粗切：按花括号配平）
  let out = "";
  let i = 0;
  while (i < src.length) {
    const m = src.slice(i).search(/#\[cfg\(test\)\]/);
    if (m < 0) {
      out += src.slice(i);
      break;
    }
    out += src.slice(i, i + m);
    const brace = src.indexOf("{", i + m);
    if (brace < 0) break;
    let depth = 0;
    let j = brace;
    for (; j < src.length; j++) {
      if (src[j] === "{") depth++;
      else if (src[j] === "}") {
        depth--;
        if (depth === 0) break;
      }
    }
    i = j + 1;
  }
  return out;
}

function walk(dir) {
  for (const f of readdirSync(dir)) {
    const p = path.join(dir, f);
    if (statSync(p).isDirectory()) {
      walk(p);
      continue;
    }
    if (!f.endsWith(".rs")) continue;
    const src = stripComments(stripTests(readFileSync(p, "utf8")));
    for (const re of [CODED_CALL, CODED_LITERAL]) {
      for (const m of src.matchAll(re)) codes.add(m[1]);
    }
  }
}
walk(SRC);

// ---- ② locale 对账 ----
const zhErr = JSON.parse(readFileSync(path.join(ROOT, "src/i18n/locales/zh.json"), "utf8")).errors ?? {};
const enErr = JSON.parse(readFileSync(path.join(ROOT, "src/i18n/locales/en.json"), "utf8")).errors ?? {};

const fail = [];
for (const code of [...codes].sort()) {
  if (LEGACY_CODES.has(code) || EXCLUDE_CODES.has(code)) continue;
  const key = camel(code);
  if (!(key in zhErr)) fail.push(`码 ${code} → errors.${key} 在 zh.json 缺键`);
  if (!(key in enErr)) fail.push(`码 ${code} → errors.${key} 在 en.json 缺键`);
}
for (const key of Object.keys(zhErr)) {
  if (LEGACY_KEYS.has(key)) continue;
  // 孤儿键：locale 有键而 Rust 无对应码（反查：camel→大写下划线；首字符小写不产生前导下划线）
  const code = key.replace(/([A-Z])/g, "_$1").toUpperCase().replace(/^_/, "");
  if (!codes.has(code)) fail.push(`errors.${key} 是孤儿键（Rust 无码 ${code}；若为 legacy 请登记 scripts/check-error-codes.mjs 的 LEGACY_KEYS）`);
}
if (enErr && Object.keys(enErr).length !== Object.keys(zhErr).length) {
  fail.push("zh/en errors 段键数不一致（由 check-i18n 兜底，此处冗余校验）");
}

if (fail.length) {
  console.error(`错误码对账失败 ${fail.length} 项：`);
  for (const f of fail) console.error(`  - ${f}`);
  console.error(`Rust 侧共提取 ${codes.size} 个码；约定见 src-tauri/src/errcode.rs`);
  process.exit(1);
}
console.log(`错误码对账通过：${codes.size} 码 / errors 段 ${Object.keys(zhErr).length} 键（legacy ${LEGACY_KEYS.size}）`);
