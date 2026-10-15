#!/usr/bin/env node
// =============================================================================
// check-error-codes.mjs —— 错误码 ⇄ locale 键 双向对账（#123/#150/#157 全量迁移）
// =============================================================================
// 约定全文见 src-tauri/src/errcode.rs（与本脚本互为镜像）：
//   Rust 错误串前缀码 `CODE: ...` / `CODE:param: ...`；locale 键 = errors.camelCase(CODE)。
// 本脚本做三件事（任一失败非零退出）：
//   ① 扫 src-tauri/src 生产代码提取全部使用的码（`"CODE: ..."` 字面前缀形态；
//     coded() 助手已删——见 errcode.rs「迁移实操口径」，剔除 #[cfg(test)] 模块）；
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
  // 剔除 `#[cfg(test)] mod tests { …}`（花括号配平）。复审 N1 的两课：
  //  ① 锚点必须闭合 `\]`（上版缺 `\]` → 全库 0 命中，剔除恒为 no-op）；
  //  ② 锚点后先判 `;` 与 `{` 谁先到——先 `;`（`mod x;`/`use …;` 等无体声明，
  //     全库 33 处）按语句跳过不删块，否则 `indexOf("{")` 会跳到很远的块、
  //     吞掉中间的生产代码（可见字节 73.8%→30.2% 的实证形态）。
  let out = "";
  let i = 0;
  while (i < src.length) {
    const rest = src.slice(i);
    const lines = rest.split("\n");
    let hit = null; // 锚点行首在 rest 中的偏移
    let consumed = 0;
    for (const l of lines) {
      if (/^\s*#\[cfg\((?:test|all\(test[^\)]*\))\)\]\s*$/.test(l)) {
        hit = consumed;
        break;
      }
      consumed += l.length + 1;
    }
    if (hit === null) {
      out += rest;
      break;
    }
    out += rest.slice(0, hit);
    const after = rest.slice(hit);
    const semi = after.indexOf(";");
    const brace = after.indexOf("{");
    if (semi >= 0 && (brace < 0 || semi < brace)) {
      // 无体声明（`;` 先到）：只跳过这条语句，锚点本身保留在扫描面外
      i = i + hit + semi + 1;
      continue;
    }
    if (brace < 0) break;
    // 词法态配平（三轮评审 I1：此前不认生命周期 'a、raw string r#"…"#、块注释，
    // 三者任一出现即让深度提前归零、模块只删一半——10 个文件约 672KB 测试代码
    // 被当生产面扫描，遮蔽攻击在泄漏区可行）。状态机按 Rust 词法优先级：
    // 块注释 → raw string → 字符串/字符字面量（含生命周期判别：' 后跟标识符
    // 且前一个字符不是标识符/字面量内容时是生命周期，不是字符串开）。
    let depth = 0;
    let j = brace;
    let mode = "code"; // code | str | chr | blockComment | rawStr
    let strCh = "";
    let rawHashes = 0;
    let prevIdent = false; // 上一字符是否标识符字符（生命周期判别用）
    while (j < after.length) {
      const c = after[j];
      const next = after[j + 1] ?? "";
      if (mode === "blockComment") {
        if (c === "*" && next === "/") {
          mode = "code";
          j++;
        }
      } else if (mode === "rawStr") {
        if (c === '"') {
          // 检查后跟的 # 数是否与开时一致
          let k = j + 1;
          let hashes = 0;
          while (after[k] === "#") {
            hashes++;
            k++;
          }
          if (hashes === rawHashes) {
            mode = "code";
            j = k - 1;
          }
        }
      } else if (mode === "str" || mode === "chr") {
        if (c === "\\") j++;
        else if (c === strCh) mode = "code";
      } else {
        // code 态
        if (c === "/" && next === "*") {
          mode = "blockComment";
          j++;
        } else if (c === "/" && next === "/") {
          // 行注释：跳到行尾（stripComments 已处理过一轮，这里兜底跨状态残留）
          while (j < after.length && after[j] !== "\n") j++;
        } else if (c === "r" && (next === '"' || (next === "#" && after[j + 2] === '"'))) {
          // raw string 开：r"…" 或 r#"…"#（含 r##"…"##）
          let k = j + 1;
          rawHashes = 0;
          while (after[k] === "#") {
            rawHashes++;
            k++;
          }
          j = k; // 跳到开引号（循环尾 j++ 进入串内）
          mode = "rawStr";
        } else if (c === '"') {
          mode = "str";
          strCh = '"';
        } else if (c === "'") {
          // 生命周期判别：' 后是标识符字符且 ' 前不是标识符字符 → 'a（不是字符字面量）。
          // b'x'（字节字面量）前是 b：prevIdent=true → 按字面量走。
          const nextIsIdent = /[A-Za-z0-9_]/.test(next);
          if (nextIsIdent && !prevIdent && after[j + 2] !== "'") {
            // 生命周期 'a：直接跳过（j++ 越过 '，下一轮越过标识符首字符）
          } else {
            mode = "chr";
            strCh = "'";
          }
        } else if (c === "{") depth++;
        else if (c === "}") {
          depth--;
          if (depth === 0) break;
        }
      }
      prevIdent = /[A-Za-z0-9_]/.test(c);
      j++;
    }
    i = i + hit + j + 1;
  }
  return out;
}

// 失败清单（三轮评审 I3：必须在 walkers 之前初始化——checkRepeated 在 walk 里
// 被调用，命中即 push；原先声明在 locale 对账段，命中时 TDZ 崩溃丢诊断）
const fail = [];

function walk(dir) {
  for (const f of readdirSync(dir)) {
    const p = path.join(dir, f);
    if (statSync(p).isDirectory()) {
      walk(p);
      continue;
    }
    if (!f.endsWith(".rs")) continue;
    const raw = readFileSync(p, "utf8");
    checkRepeated(stripComments(raw), p);
    const src = stripTests(stripComments(raw)); // I1：先剥注释再锚定 cfg(test)——反序会把文档注释里的 cfg(test) 当锚点吞掉生产代码
    for (const re of [CODED_LITERAL]) {
      for (const m of src.matchAll(re)) codes.add(m[1]);
    }
  }
}
walk(SRC);

// 前端自产码（src/ 的 TS/TSX，同款前缀形态——mobile/api.ts 的 MOBILE_NETWORK_FAIL 族）
const FE_SRC = path.join(ROOT, "src");
const FE_SKIP = new Set(["i18n"]); // locale 目录不算
function walkFe(dir) {
  for (const f of readdirSync(dir)) {
    const p = path.join(dir, f);
    if (statSync(p).isDirectory()) {
      if (FE_SKIP.has(f)) continue;
      walkFe(p);
      continue;
    }
    if (!/\.(tsx?|ts)$/.test(f)) continue;
    const raw = readFileSync(p, "utf8");
    checkRepeated(stripComments(raw), p);
    const src = stripComments(raw);
    for (const m of src.matchAll(/["'`]([A-Z][A-Z0-9_]{2,}):/g)) codes.add(m[1]);
  }
}
walkFe(FE_SRC);

// ---- ② locale 对账 ----
const zhErr =
  JSON.parse(readFileSync(path.join(ROOT, "src/i18n/locales/zh.json"), "utf8")).errors ?? {};
const enErr =
  JSON.parse(readFileSync(path.join(ROOT, "src/i18n/locales/en.json"), "utf8")).errors ?? {};

for (const code of [...codes].sort()) {
  if (LEGACY_CODES.has(code) || EXCLUDE_CODES.has(code)) continue;
  const key = camel(code);
  if (!(key in zhErr)) fail.push(`码 ${code} → errors.${key} 在 zh.json 缺键`);
  if (!(key in enErr)) fail.push(`码 ${code} → errors.${key} 在 en.json 缺键`);
}
for (const key of Object.keys(zhErr)) {
  if (LEGACY_KEYS.has(key)) continue;
  // 孤儿键：locale 有键而 Rust 无对应码（反查：camel→大写下划线；首字符小写不产生前导下划线）
  const code = key
    .replace(/([A-Z])/g, "_$1")
    .toUpperCase()
    .replace(/^_/, "");
  if (!codes.has(code))
    fail.push(
      `errors.${key} 是孤儿键（Rust 无码 ${code}；若为 legacy 请登记 scripts/check-error-codes.mjs 的 LEGACY_KEYS）`
    );
}
if (enErr && Object.keys(enErr).length !== Object.keys(zhErr).length) {
  fail.push("zh/en errors 段键数不一致（由 check-i18n 兜底，此处冗余校验）");
}

// 重复前缀检测（B1 事故防复发：正则批量改写非幂等叠加会把同一码叠成多重前缀）
function checkRepeated(src, label) {
  src.split("\n").forEach((l, i) => {
    const t = l.trim();
    if (t.startsWith("//") || t.startsWith("*")) return;
    // I2：覆盖形态 B（A:param: A:param: / A: CODE:param: 等叠加形态都要抓）
    if (/([A-Z][A-Z0-9_]{2,}(?::[^:\s]+)?: ){2,}/.test(l) || /([A-Z][A-Z0-9_]{2,}:){2,}/.test(l))
      fail.push(`${label}:${i + 1} 重复码前缀（批量改写叠加事故？）：${t.slice(0, 60)}`);
  });
}

// 同串同码（评审 I4）：剥码后的同一字面量若映射到多个码 → 红（防「同句不同命」
// ——同一句中文一处打码一处漏/换码，用户看到同句不同前缀）。**仅 Rust 侧**（前缀码
// 形态在前端源里只出现在 api.ts 一族，暂无双码风险；交集规则才是双源）。
const literalCodes = new Map(); // 剥码后原文 → Set<code>
function collectLiteralCodes(src) {
  // 转义引号容忍：字面量含 \" 时不能截断（jsonc.rs 的 section "{}" 实证）
  for (const m of src.matchAll(/"([A-Z][A-Z0-9_]{2,})(?::[^:\s]+)?: ((?:[^"\\]|\\.)+)"/g)) {
    const lit = m[2];
    if (!literalCodes.has(lit)) literalCodes.set(lit, new Set());
    literalCodes.get(lit).add(m[1]);
  }
}
for (const f of readdirSync(SRC, { recursive: true })) {
  if (typeof f !== "string" || !f.endsWith(".rs")) continue;
  const raw = readFileSync(path.join(SRC, f), "utf8");
  collectLiteralCodes(stripTests(stripComments(raw)));
}
for (const [lit, set] of literalCodes) {
  if (set.size > 1)
    fail.push(
      `同一句中文映射了多个码（同串必须同码）："${lit.slice(0, 40)}…" → ${[...set].join(" / ")}`
    );
}

// coded↔raw 半迁移判定（复审 N3 + 三轮 I2）：同一句中文既以「码: 原文」出现、又以
// 裸串出现在错误上下文 → 红。判定从行级提升为**跨文件、按剥码后正文归一化**
// （去空白 + 参数占位符 {x}/{:?}/{} 统一为 ◇）求交——行级匹配会漏掉
// 「question.rs 裸 ↔ api.rs 带码」这类逐字节同句的跨文件半迁移（三轮实锤）。
// Rust 与前端两源同收（前端 TS/TSX 的字符串字面量同样参与两表）。
const normalize = (s) => s.replace(/\s+/g, "").replace(/\{[^}]*\}/g, "◇");
const codedNorm = new Map(); // 归一化正文 → Set<code>
for (const [lit, set] of literalCodes) codedNorm.set(normalize(lit), set);

const rawErrorLiterals = new Map(); // 归一化正文 → 首个原文样本（报错用）
function collectRawLiteral(line) {
  if (!/(Err\(|format!|map_err|json!|StageAbort|failed\(|error)/.test(line)) return;
  if (/log::[a-z]+!/.test(line)) return; // 日志不上屏
  if (/PetRpcError::new|UsageError::new/.test(line)) return; // 结构化码族（#157 补译批边界）
  for (const m of line.matchAll(/"((?:[^"\\]|\\)*[\u4e00-\u9fff](?:[^"\\]|\\)*)"/g)) {
    if (/^[A-Z][A-Z0-9_]{2,}(:[^:\s]+)?: /.test(m[1])) continue; // 自带码前缀 = coded 形态
    const key = normalize(m[1]);
    if (!rawErrorLiterals.has(key)) rawErrorLiterals.set(key, m[1]);
  }
}
for (const f of readdirSync(SRC, { recursive: true })) {
  if (typeof f !== "string" || !f.endsWith(".rs")) continue;
  const stripped = stripTests(stripComments(readFileSync(path.join(SRC, f), "utf8")));
  stripped.split("\n").forEach(collectRawLiteral);
}
for (const f of readdirSync(FE_SRC, { recursive: true })) {
  if (typeof f !== "string" || !/\.(tsx?|ts)$/.test(f)) continue;
  const stripped = stripComments(readFileSync(path.join(FE_SRC, f), "utf8"));
  stripped.split("\n").forEach(collectRawLiteral);
}
for (const [key, sample] of rawErrorLiterals) {
  if (codedNorm.has(key)) {
    fail.push(
      `半迁移（跨文件归一化匹配）：该句既有码形态又有裸串形态——裸串处漏码："${sample.slice(0, 40)}…"（码形态：${[...codedNorm.get(key)].join("/")}）`
    );
  }
}

if (fail.length) {
  console.error(`错误码对账失败 ${fail.length} 项：`);
  for (const f of fail) console.error(`  - ${f}`);
  console.error(`Rust 侧共提取 ${codes.size} 个码；约定见 src-tauri/src/errcode.rs`);
  process.exit(1);
}
console.log(
  `错误码对账通过：${codes.size} 码 / errors 段 ${Object.keys(zhErr).length} 键（legacy ${LEGACY_KEYS.size}）`
);
