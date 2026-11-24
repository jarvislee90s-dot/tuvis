#!/usr/bin/env node
/**
 * DSH 外部插件 bundle 的确定性预构建打包器。
 *
 * 产物（纳入版本管理，发行时不需要系统 Node/npm）：
 *   resources/dsh/remote-access.tgz  — 可被 `dsh plugin --profile desktop add` 安装的 npm 形态包
 *   resources/dsh/manifest.json      — { pluginVersion, sha256, bindingProtocol: 1 }
 *
 * 确定性（同输入必出同字节）：
 *   1. tar 条目按路径排序；全部条目 mtime = 0（epoch）、uid/gid = 0、uname/gname 空；
 *   2. gzip 不写时间戳（zlib mtime: 0），无原始文件名；
 *   3. 包内 package.json 按**固定键序白名单**重建（剥掉 scripts/devDependencies），
 *      不直接序列化源文件，键序与内容与开发者本机无关；
 *   4. 归档内只有相对路径，绝无绝对开发路径；
 *   5. 前置条件：`pnpm --filter @tuvis/dsh-remote-access build` 已产出 lib/
 *      （tsc 输出本身不含时间戳/路径）。
 *   6. `--check` 从当前源码在空临时目录编译两套 tsconfig，再逐字节比对产物。
 *
 * 用法：
 *   node scripts/build-dsh-plugin.mjs                # 重建并写入 resources/dsh/
 *   node scripts/build-dsh-plugin.mjs --check        # 重建到临时目录并与已提交产物逐字节比对
 *   node scripts/build-dsh-plugin.mjs --out-dir DIR  # 写到指定目录（测试/复现用）
 */

import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  realpathSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { gzipSync, gunzipSync } from "node:zlib";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(scriptDir, "..");
const packageDir = join(repoRoot, "packages", "dsh-remote-access");
const defaultOutDir = join(repoRoot, "resources", "dsh");

function bindingProtocol(libDir) {
  const protocol = Number(
    /BINDING_PROTOCOL\s*=\s*(\d+)/.exec(readFileSync(join(libDir, "types.js"), "utf8"))?.[1]
  );
  if (!Number.isInteger(protocol) || protocol < 1) {
    throw new Error("无法从编译产物 types.js 解析 BINDING_PROTOCOL");
  }
  return protocol;
}

/** 打进包内的文件白名单（packaged package.json 的 files 字段与此一致）。 */
const EXTRA_FILES = ["cordis.patch.yml", "LICENSE", "NOTICE", "README.md", "README.zh.md"];

/** 打包后 package.json 保留的键，固定顺序（刻意不含 scripts / devDependencies）。 */
const PACKED_KEYS = [
  "name",
  "version",
  "description",
  "type",
  "license",
  "engines",
  "main",
  "types",
  "exports",
  "files",
  "dsh",
  "peerDependencies",
  "peerDependenciesMeta",
  "dependencies",
];

/* ------------------------------------------------------------------ *
 * 最小 ustar 写入器 / 读取器（零第三方依赖，保证产物逐字节可控）
 * ------------------------------------------------------------------ */

const BLOCK = 512;

/** 把字符串/Buffer 写进定长字段：超长截断，右侧补 NUL。 */
function tarField(value, length) {
  const buf = Buffer.alloc(length, 0);
  Buffer.from(String(value), "utf8").copy(buf, 0, 0, length);
  return buf;
}

/** 八进制数值字段：Octal_digits + NUL（tar 标准 7+1 / 11+1）。 */
function tarOctal(value, length) {
  return tarField(value.toString(8).padStart(length - 1, "0"), length);
}

function tarHeader({ path: entryPath, size, typeFlag }) {
  const header = Buffer.alloc(BLOCK, 0);
  // name(100) mode(8) uid(8) gid(8) size(12) mtime(12) chksum(8) typeflag(1)
  // linkname(100) magic(6) version(2) uname(32) gname(32) devmajor(8) devminor(8) prefix(155)
  tarField(entryPath, 100).copy(header, 0);
  tarOctal(typeFlag === "5" ? 0o755 : 0o644, 8).copy(header, 100);
  tarOctal(0, 8).copy(header, 108); // uid = 0
  tarOctal(0, 8).copy(header, 116); // gid = 0
  tarOctal(typeFlag === "5" ? 0 : size, 12).copy(header, 124);
  tarOctal(0, 12).copy(header, 136); // mtime = epoch（确定性关键）
  header.write("        ", 148, 8, "utf8"); // chksum 位先填空格参与求和
  header.write(typeFlag, 156, 1, "utf8");
  header.write("ustar\0", 257, 6, "utf8");
  header.write("00", 263, 2, "utf8");
  tarField("", 32).copy(header, 265); // uname 空
  tarField("", 32).copy(header, 297); // gname 空
  let sum = 0;
  for (const byte of header) sum += byte;
  tarOctal(sum, 8).copy(header, 148);
  return header;
}

/** 构建确定性 tar：条目按路径排序、固定元数据、两块全零收尾。 */
export function buildTarBuffer(entries) {
  const sorted = [...entries].sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));
  const chunks = [];
  for (const entry of sorted) {
    const typeFlag = entry.type === "dir" ? "5" : "0";
    chunks.push(tarHeader({ path: entry.path, size: entry.data.length, typeFlag }));
    if (typeFlag === "0") {
      chunks.push(entry.data);
      const pad = (BLOCK - (entry.data.length % BLOCK)) % BLOCK;
      if (pad > 0) chunks.push(Buffer.alloc(pad, 0));
    }
  }
  chunks.push(Buffer.alloc(2 * BLOCK, 0)); // tar 收尾两块全零
  return Buffer.concat(chunks);
}

/** 解析 gzip+tar 字节流，返回 [{ path, type, data }]（供测试与 --check 复用）。 */
export function listTarEntries(tgzBuffer) {
  const tar = gunzipSync(tgzBuffer);
  const entries = [];
  let offset = 0;
  while (offset + BLOCK <= tar.length) {
    const header = tar.subarray(offset, offset + BLOCK);
    if (header.every((byte) => byte === 0)) break;
    const path = header.subarray(0, 100).toString("utf8").replace(/\0+$/, "");
    const typeFlag = header.toString("utf8", 156, 157);
    const size = parseInt(header.toString("utf8", 124, 136).replace(/\0.*$/, "").trim(), 8) || 0;
    offset += BLOCK;
    const data =
      typeFlag === "0" ? Buffer.from(tar.subarray(offset, offset + size)) : Buffer.alloc(0);
    if (typeFlag === "0") offset += Math.ceil(size / BLOCK) * BLOCK;
    entries.push({ path, type: typeFlag === "5" ? "dir" : "file", data });
  }
  return entries;
}

/* ------------------------------------------------------------------ *
 * 打包：lib/ + 白名单文件 → 确定性 tgz + manifest
 * ------------------------------------------------------------------ */

/** 递归收集 lib/ 下文件（相对路径），顺序无关——buildTarBuffer 内部排序。 */
function collectLibFiles(libDir) {
  const files = [];
  const walk = (dir, prefix) => {
    for (const name of readdirSync(dir).sort()) {
      const full = join(dir, name);
      if (statSync(full).isDirectory()) walk(full, `${prefix}${name}/`);
      else files.push({ prefix: `${prefix}${name}`, full });
    }
  };
  walk(libDir, "lib/");
  return files;
}

/** 由源 package.json 按固定键序重建打包清单（剥 scripts/devDependencies）。 */
export function buildPackedPackageManifest() {
  const source = JSON.parse(readFileSync(join(packageDir, "package.json"), "utf8"));
  const packed = {};
  for (const key of PACKED_KEYS) {
    if (source[key] !== undefined) packed[key] = source[key];
  }
  return packed;
}

/**
 * 构建一次完整产物。返回 { tarball, manifest, sha256, version }；
 * 传入 outDir 时把 remote-access.tgz / manifest.json 写到该目录。
 * lib/ 缺失时抛出带补救指引的错误（不代跑构建，保持本脚本单一职责）。
 */
export function buildReleaseArtifacts({ outDir, libDir = join(packageDir, "lib") } = {}) {
  if (!existsSync(join(libDir, "index.js"))) {
    throw new Error("lib/index.js 不存在：先运行 `pnpm --filter @tuvis/dsh-remote-access build`");
  }
  const source = JSON.parse(readFileSync(join(packageDir, "package.json"), "utf8"));

  const entries = [{ path: "package", type: "dir", data: Buffer.alloc(0) }];
  const packedJson = Buffer.from(
    `${JSON.stringify(buildPackedPackageManifest(), null, 2)}\n`,
    "utf8"
  );
  entries.push({ path: "package/package.json", type: "file", data: packedJson });
  entries.push({ path: "package/lib", type: "dir", data: Buffer.alloc(0) });
  for (const { prefix, full } of collectLibFiles(libDir)) {
    entries.push({ path: `package/${prefix}`, type: "file", data: readFileSync(full) });
    const dir = prefix.slice(0, prefix.lastIndexOf("/"));
    if (dir && !entries.some((entry) => entry.path === `package/${dir}`)) {
      entries.push({ path: `package/${dir}`, type: "dir", data: Buffer.alloc(0) });
    }
  }
  for (const name of EXTRA_FILES) {
    const full = join(packageDir, name);
    if (!existsSync(full)) throw new Error(`包内白名单文件缺失：${name}`);
    entries.push({ path: `package/${name}`, type: "file", data: readFileSync(full) });
  }

  const tarBuffer = buildTarBuffer(entries);
  // 确定性 gzip：不写 mtime、不写原文件名。
  const tarball = gzipSync(tarBuffer, { mtime: 0 });
  const sha256 = createHash("sha256").update(tarball).digest("hex");
  const manifest = {
    pluginVersion: source.version,
    sha256,
    bindingProtocol: bindingProtocol(libDir),
  };
  const manifestJson = Buffer.from(`${JSON.stringify(manifest, null, 2)}\n`, "utf8");

  if (outDir) {
    mkdirSync(outDir, { recursive: true });
    writeFileSync(join(outDir, "remote-access.tgz"), tarball);
    writeFileSync(join(outDir, "manifest.json"), manifestJson);
  }
  return { tarball, manifest: manifestJson, sha256, version: source.version };
}

/* ------------------------------------------------------------------ *
 * CLI
 * ------------------------------------------------------------------ */

function parseArgs(argv) {
  const args = { check: false, outDir: undefined };
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--check") args.check = true;
    else if (argv[i] === "--out-dir") args.outDir = resolve(argv[++i]);
    else throw new Error(`未知参数：${argv[i]}`);
  }
  return args;
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  if (!args.check) {
    const { sha256, version } = buildReleaseArtifacts({ outDir: args.outDir ?? defaultOutDir });
    const target = args.outDir ?? defaultOutDir;
    console.log(`已写入 ${join(target, "remote-access.tgz")}`);
    console.log(
      `已写入 ${join(target, "manifest.json")}（pluginVersion=${version} sha256=${sha256}）`
    );
    return 0;
  }

  // --check：重建到临时目录，与已提交产物逐字节比对（复现性门禁）。
  const committedTgz = join(defaultOutDir, "remote-access.tgz");
  const committedManifest = join(defaultOutDir, "manifest.json");
  const problems = [];
  for (const file of [committedTgz, committedManifest]) {
    if (!existsSync(file)) problems.push(`缺少已提交产物：${file}`);
  }
  if (problems.length === 0) {
    const tempDir = mkdtempSync(join(tmpdir(), "dsh-plugin-check-"));
    try {
      const libDir = join(tempDir, "lib");
      const tsc = join(repoRoot, "node_modules", "typescript", "bin", "tsc");
      for (const config of ["tsconfig.json", "tsconfig.client.json"]) {
        const result = spawnSync(
          process.execPath,
          [tsc, "-p", join(packageDir, config), "--outDir", libDir],
          {
            cwd: packageDir,
            encoding: "utf8",
          }
        );
        if (result.error || result.status !== 0) {
          throw new Error(
            `编译 ${config} 失败：${result.error ?? result.stderr ?? ""}\n${result.stdout ?? ""}`
          );
        }
      }
      const fresh = buildReleaseArtifacts({ outDir: tempDir, libDir });
      const freshTgz = readFileSync(join(tempDir, "remote-access.tgz"));
      const freshManifest = readFileSync(join(tempDir, "manifest.json"));
      if (!freshTgz.equals(readFileSync(committedTgz))) {
        problems.push("remote-access.tgz 与重建结果不一致（产物过期或被改动）");
      }
      if (!freshManifest.equals(readFileSync(committedManifest))) {
        problems.push(`manifest.json 与重建结果不一致（应为 sha256=${fresh.sha256}）`);
      }
    } finally {
      rmSync(tempDir, { recursive: true, force: true });
    }
  }
  if (problems.length > 0) {
    for (const problem of problems) console.error(`--check 失败：${problem}`);
    return 1;
  }
  console.log("--check 通过：已提交的 tgz 与 manifest 可逐字节复现");
  return 0;
}

let isMain = false;
try {
  isMain =
    process.argv[1] !== undefined &&
    realpathSync(resolve(process.argv[1])) === fileURLToPath(import.meta.url);
} catch {
  // 宿主可能使用虚拟入口；导入模块时不应要求该路径存在。
}
if (isMain) {
  process.exitCode = main();
}
