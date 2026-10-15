#!/usr/bin/env node
/**
 * tauri 版本对齐检查（Rust crate ↔ @tauri-apps/* NPM 包）
 *
 * 为什么存在：tauri-cli 自带一条硬门禁——`tauri`/`tauri-plugin-*` 这些 Rust crate 与
 * 对应的 `@tauri-apps/*` NPM 包，**major.minor 必须相等**（crates/tauri-cli/src/info/plugins.rs）。
 * 它只在 `pnpm tauri build` 内部触发，而那个 job 要先 apt-get + 装 Rust + 编译：
 * PR #86 / #91 各跑了约 2.5 分钟才在最后吐出一行 `Found version mismatched Tauri packages`。
 * 本脚本把同一条规则提前到 10 秒级的独立 CI job，失败时直接指名道姓 + 给修复方向。
 *
 * 判定规则（与 CLI 对齐，另有两点刻意取舍）：
 *   1. 只比对「双侧都作为直接依赖」的配对：tauri ↔ @tauri-apps/api、
 *      tauri-plugin-X ↔ @tauri-apps/plugin-X；
 *   2. 只比 major.minor，patch 差异合法（CLI 原文 "same major/minor releases"）；
 *   3. 单侧存在的对不判失败，只作为信息列出（Rust-only 的 tauri-plugin-fs /
 *      tauri-plugin-single-instance、NPM-only 的 @tauri-apps/plugin-process 都属此类）；
 *   4. 比 CLI 略严：CLI 只认识它内置的 known_plugins 名单，本脚本按「存在同名对手方」配对。
 *      对官方插件两者等价，且本脚本不会因为 CLI 名单过期而漏判。
 *
 * 数据来源（都是仓库里已有的文件，不需要 pnpm install / cargo）：
 *   - Rust 侧：src-tauri/Cargo.lock 中工作区包的多 agents-manager 依赖列表（= 直接依赖）
 *   - NPM 侧：pnpm-lock.yaml 顶层 importer "." 的 dependencies / devDependencies（= 直接依赖）
 *
 * 退出码：0 = 对齐；1 = 存在不一致或解析失败。
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const read = (rel) => readFileSync(path.join(repoRoot, rel), "utf8");

const CARGO_LOCK = "src-tauri/Cargo.lock";
const PNPM_LOCK = "pnpm-lock.yaml";
const WORKSPACE_PACKAGE = "tuvis";
const NPM_SCOPE = "@tauri-apps/";

function die(msg) {
  console.error(`✗ tauri 版本对齐检查无法完成：${msg}`);
  process.exit(1);
}

const majorMinor = (v) => v.split(".").slice(0, 2).join(".");

/** 解析 Cargo.lock：name -> [{ version, deps: string[] }]（deps 条目形如 "name" 或 "name version"） */
function parseCargoLock(text) {
  const packages = new Map();
  for (const chunk of text.split("[[package]]")) {
    const name = chunk.match(/^\s*name = "([^"]+)"/m);
    if (!name) continue;
    const version = chunk.match(/^\s*version = "([^"]+)"/m);
    const depsBlock = chunk.match(/^\s*dependencies = \[([\s\S]*?)^\s*\]/m);
    const deps = depsBlock ? [...depsBlock[1].matchAll(/"([^"]+)"/g)].map((m) => m[1]) : [];
    const list = packages.get(name[1]) ?? [];
    list.push({ version: version ? version[1] : null, deps });
    packages.set(name[1], list);
  }
  return packages;
}

/** 工作区包的直接依赖里，tauri / tauri-plugin-* 的解析版本 */
function rustCrateVersions() {
  const packages = parseCargoLock(read(CARGO_LOCK));
  const self = packages.get(WORKSPACE_PACKAGE);
  if (!self || self.length !== 1) {
    die(`${CARGO_LOCK} 中找不到唯一的 ${WORKSPACE_PACKAGE} 包块`);
  }
  const out = new Map();
  for (const entry of self[0].deps) {
    const [name, pinned] = entry.split(" ");
    if (name !== "tauri" && !name.startsWith("tauri-plugin-")) continue;
    let version = pinned;
    if (!version) {
      // 无版本后缀 = 该 crate 在锁里只有一个版本
      const candidates = packages.get(name) ?? [];
      if (candidates.length !== 1) {
        die(`${CARGO_LOCK} 中 ${name} 有 ${candidates.length} 个版本，且依赖条目未指明版本`);
      }
      version = candidates[0].version;
    }
    if (!version) die(`无法解析 ${name} 的版本`);
    out.set(name, version);
  }
  if (out.size === 0) die(`${CARGO_LOCK} 中 ${WORKSPACE_PACKAGE} 没有任何 tauri 系直接依赖`);
  return out;
}

/** 解析 pnpm-lock.yaml 顶层 importer "." 的直接依赖 -> 解析版本（去 peer 后缀） */
function npmDirectDeps() {
  const lines = read(PNPM_LOCK).split("\n");
  const out = new Map();
  const dotImporter = lines.findIndex((l) => /^ {2}'?\.'?:\s*$/.test(l));
  if (dotImporter < 0) die(`${PNPM_LOCK} 中找不到顶层 importer "."，无法确定直接依赖`);
  let section = null;
  for (let i = dotImporter + 1; i < lines.length; i++) {
    const line = lines[i];
    if (line.trim() === "") continue;
    if (/^ {0,2}\S/.test(line)) break; // 离开 "." importer
    const sec = line.match(/^ {4}(dependencies|devDependencies|optionalDependencies):\s*$/);
    if (sec) {
      section = sec[1];
      continue;
    }
    const pkg = line.match(/^ {6}'?([^':]+)'?:\s*$/);
    if (!pkg || !section) continue;
    for (let k = i + 1; k < lines.length; k++) {
      if (/^ {0,6}\S/.test(lines[k])) break; // 下一个包条目或更外层
      const vm = lines[k].match(/^ {8}version:\s*(\S+)/);
      if (vm) {
        out.set(pkg[1], vm[1].replace(/\(.*$/, ""));
        break;
      }
    }
  }
  return out;
}

const rust = rustCrateVersions();
const npm = npmDirectDeps();

const npmNameFor = (crate) =>
  crate === "tauri"
    ? `${NPM_SCOPE}api`
    : `${NPM_SCOPE}plugin-${crate.slice("tauri-plugin-".length)}`;

const pairs = [];
const rustOnly = [];
const pairedNpm = new Set();
for (const [crate, crateVersion] of [...rust].sort()) {
  const pkg = npmNameFor(crate);
  const npmVersion = npm.get(pkg);
  if (!npmVersion) {
    rustOnly.push(crate);
    continue;
  }
  pairedNpm.add(pkg);
  pairs.push({ crate, crateVersion, pkg, npmVersion });
}
const npmOnly = [...npm]
  .filter(
    ([name]) =>
      (name === `${NPM_SCOPE}api` || name.startsWith(`${NPM_SCOPE}plugin-`)) && !pairedNpm.has(name)
  )
  .map(([name, version]) => `${name} ${version}`);

const mismatched = pairs.filter((p) => majorMinor(p.crateVersion) !== majorMinor(p.npmVersion));

console.log(`tauri 版本对齐检查（Rust crate ↔ ${NPM_SCOPE}* NPM 包，仅直接依赖配对）`);
for (const p of pairs) {
  const ok = majorMinor(p.crateVersion) === majorMinor(p.npmVersion);
  console.log(
    `  ${ok ? "✓" : "✗"} ${p.crate.padEnd(30)} ${p.crateVersion.padEnd(10)} ${ok ? "=" : "≠"}  ${p.pkg.padEnd(34)} ${p.npmVersion}`
  );
}
if (rustOnly.length) console.log(`  · 仅 Rust 侧存在（不判定）：${rustOnly.join(", ")}`);
if (npmOnly.length) console.log(`  · 仅 NPM 侧存在（不判定）：${npmOnly.join(", ")}`);

if (mismatched.length) {
  console.error("");
  console.error(`✗ tauri 版本未对齐：${mismatched.length} 对 major.minor 不一致`);
  console.error("  修复：tauri 系依赖必须同批升级，Rust crate 与 NPM 包的 major.minor 必须相等。");
  console.error("  最常见场景：dependabot 的 npm 分组 PR 与 cargo 分组 PR 是同一批升级的两半，");
  console.error(
    "  单独合任何一个都必红——把两者放进同一次变更（或把 NPM 侧锁回与 Rust 相同的 minor）再合。"
  );
  console.error("  另注意 `^2` 这类松范围：rebase 重新解析锁文件时 NPM 侧会漂到更新的 minor。");
  process.exit(1);
}
console.log(`\n✓ tauri 版本对齐通过（${pairs.length} 对已比对）`);
