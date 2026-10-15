#!/usr/bin/env node
// 错误码 ⇄ locale 双向对账（#123/#150/#157）；约定见 src-tauri/src/errcode.rs。
// 生产字符串的码/重复前缀/同串多码/半迁移、用量 detail 与迁移棘轮共用源码视图。
import { readFileSync, readdirSync, existsSync } from "node:fs";
import path from "node:path";
import { sourceView, frontendView, stripTests, callRanges } from "./error-code-source.mjs";

const ROOT = process.cwd();
const LEGACY_CODES = new Set(["W5_TOOL_DISABLED", "W5_APPLY_IN_PROGRESS", "W5_APPLY_TASK_FAILED"]);
const EXCLUDE_CODES = new Set(["TUVIS"]); // 标题 marker，不是错误码
const LEGACY_KEYS = new Set([
  "toolDisabled",
  "applyInProgress",
  "applyTaskFailed",
  "presetActive",
  "presetNativeInUniversal",
]);
const camel = (code) => code.toLowerCase().replace(/_([a-z0-9])/g, (_, c) => c.toUpperCase());
const normalize = (s) => s.replace(/\$?\{[^}]*\}/g, "◇").replace(/\s+/g, "");
const CODE = /^([A-Z][A-Z0-9_]{2,})(?::[^:\s]+)?:\s+([\s\S]*)$/;
const CJK = /[\u3400-\u9fff]/;
const fail = [];
const codes = new Set();
const coded = new Map(); // 归一化正文 → Set<code>，Rust 与前端对称收集
const literalCodes = new Map(); // 同串同码用原正文，不把不同参数语义误判为同句
const raw = []; // 不限行窗口：const/helper 间接返回的同句裸串也不能漏
const pending = {}; // 迁移启发式计数（仅 Rust 错误上下文，非完整上屏链路证明）

function files(dir, extension) {
  return readdirSync(path.join(ROOT, dir), { recursive: true })
    .filter((f) => extension.test(String(f)))
    .map((f) => path.join(dir, String(f)));
}

for (const file of [...files("src-tauri/src", /\.rs$/), ...files("src", /\.tsx?$/)]) {
  const relative = file.replaceAll("\\", "/");
  if (relative.startsWith("src/i18n/")) continue;
  const rust = file.endsWith(".rs");
  const original = readFileSync(path.join(ROOT, file), "utf8");
  const src = rust ? stripTests(original) : original;
  const view = rust ? sourceView(src) : await frontendView(src, file);
  const ignored = rust
    ? callRanges(
        view.skeleton,
        /\b(?:log::\w+!|PetRpcError::new|UsageError::(?:new|internal))\s*\(/g
      )
    : [];
  const usage = rust ? callRanges(view.skeleton, /\bUsageError::(?:new|internal)\s*\(/g) : [];
  const lines = view.text.split("\n");
  const location = (literal) => `${relative}:${src.slice(0, literal.start).split("\n").length}`;
  const inside = (literal, ranges) =>
    ranges.some(([lo, hi]) => lo < literal.start && literal.end <= hi);
  for (const literal of view.literals) {
    const value = literal.text;
    const prefix = /^([A-Z][A-Z0-9_]{2,}):/.exec(value);
    if (prefix) codes.add(prefix[1]);
    if (
      /^(?:[A-Z][A-Z0-9_]{2,}(?::[^:\s]+)?:\s+){2,}/.test(value) ||
      /^(?:[A-Z][A-Z0-9_]{2,}:){2,}/.test(value)
    ) {
      fail.push(`${location(literal)} 重复码前缀：${value.slice(0, 80)}`);
    }
    const match = CODE.exec(value);
    if (match && !EXCLUDE_CODES.has(match[1])) {
      if (match[2].trim()) {
        if (!literalCodes.has(match[2])) literalCodes.set(match[2], new Set());
        literalCodes.get(match[2]).add(match[1]);
      }
      const key = normalize(match[2]);
      if (!coded.has(key)) coded.set(key, new Set());
      coded.get(key).add(match[1]);
    } else if (CJK.test(value) && !inside(literal, ignored)) {
      raw.push({ key: normalize(value), sample: value, label: location(literal) });
      if (rust) {
        const line = src.slice(0, literal.start).split("\n").length - 1;
        const ctx = lines.slice(Math.max(0, line - 3), line + 4).join("\n");
        if (/Err\(|format!|map_err|json!|StageAbort|failed\(|error/.test(ctx))
          pending[relative] = (pending[relative] ?? 0) + 1;
      }
    }
    if (CJK.test(value) && inside(literal, usage)) {
      fail.push(
        `${location(literal)} 用量 detail 含中文固定文案（路径/底层错误参数可保留原值）：${value.slice(0, 80)}`
      );
    }
  }
}

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
for (const key of new Set([...Object.keys(zhErr), ...Object.keys(enErr)])) {
  if (LEGACY_KEYS.has(key)) continue;
  const code = key.replace(/([A-Z])/g, "_$1").toUpperCase();
  if (!codes.has(code)) fail.push(`errors.${key} 是孤儿键（源码无码 ${code}）`);
  if (!(key in zhErr) || !(key in enErr)) fail.push(`errors.${key} 的 zh/en 键不一致`);
}
for (const [key, set] of literalCodes) {
  if (set.size > 1)
    fail.push(`同一句中文映射了多个码："${key.slice(0, 50)}" → ${[...set].join(" / ")}`);
}
for (const { key, sample, label } of raw) {
  if (coded.has(key))
    fail.push(
      `${label} 半迁移（跨文件归一化匹配）："${sample.slice(0, 60)}"（码形态：${[...coded.get(key)].join("/")}）`
    );
}

// 固定口径的按文件棘轮，阻止新增裸错误串；只降不升的基线由迁移批显式更新。
const baselineFile = path.join(ROOT, "scripts/error-code-baseline.json");
if (existsSync(baselineFile)) {
  const baseline = JSON.parse(readFileSync(baselineFile, "utf8"));
  if (!baseline?.files || typeof baseline.files !== "object")
    fail.push("迁移基线无效：缺 files 对象");
  else {
    for (const [file, count] of Object.entries(pending)) {
      const max = baseline.files[file] ?? 0;
      if (!Number.isInteger(max) || max < 0 || count > max)
        fail.push(`${file} 未迁移错误串棘轮上升：${max} → ${count}（新文案请添加错误码）`);
    }
  }
} else fail.push("迁移基线缺失：scripts/error-code-baseline.json");
if (process.argv.includes("--report-pending"))
  console.log(JSON.stringify({ files: pending }, null, 2));
if (fail.length) {
  console.error(`错误码对账失败 ${fail.length} 项：`);
  for (const f of fail) console.error(`  - ${f}`);
  console.error(`源码共提取 ${codes.size} 个码；约定见 src-tauri/src/errcode.rs`);
  process.exit(1);
}
console.log(
  `错误码对账通过：${codes.size} 码 / errors 段 ${Object.keys(zhErr).length} 键（legacy ${LEGACY_KEYS.size}）；未迁移启发式计数 ${Object.values(pending).reduce((a, b) => a + b, 0)}`
);
