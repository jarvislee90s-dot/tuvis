// scripts/zcode-bridge-probe/sanitize.ts — 探针证据脱敏与 fail-closed 落盘（B1 实连半场）。
//
// 归属：工作包 B 探针工具目录，不在产品运行链上。
// 纪律（工作包 B 全局约束）：真实 SID/JWT/一次性 token/state/授权码/完整链接**只经
// 运行期内存**，绝不写入仓库、stdout 日志或证据文件。本模块是唯一出口：
//   - SecretRedactor：运行期登记真实值 → 稳定别名（SID_A / JWT_1 / TOK_1 …），
//     sanitizeText / sanitizeDeep 供证据与控制台输出统一走别名化；
//   - assertClean：落盘前 fail-closed 断言——任何登记过的真实值出现在待写内容中即抛错拒写；
//   - maskJwt / maskOpaque：控制台与 run.md 的掩码形态（JWT → `eyJ…<len> sha256:8hex`）；
//   - createEvidenceRunDir / writeEvidenceFiles：私有证据目录（~/Documents/tuvis-verify-logs/
//     zcode/ 下，owner-only 0600 文件），目录名 `macos-B1<UTC 时间戳>-<随机短码>`。
//
// 语法纪律：可被 `node`（type-stripping）直接执行——仅可擦除 TS 语法。

import * as crypto from "node:crypto";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

/** 登记的密钥类别；别名前缀与计数规则见 register。 */
export type SecretKind =
  "sid" | "mid" | "jwt" | "relay-token" | "state" | "auth-code" | "link" | "path" | "name";

const KIND_TAG: Record<SecretKind, string> = {
  sid: "SID",
  mid: "MID",
  jwt: "JWT",
  "relay-token": "TOK",
  state: "STATE",
  "auth-code": "CODE",
  link: "LINK",
  path: "PATH",
  name: "NAME",
};

/**
 * fail-closed 断言的口径：凭据类（jwt/token/state/auth-code/sid/mid/link）无论长短
 * 一律全查；展示类（path/name）值本身可能极短（如会话名），只在 ≥4 字符时参与断言，
 * 防止穷举式误报架空断言本身。替换（sanitizeText）则不分长短全量替换。
 */
const STRICT_KINDS: ReadonlySet<SecretKind> = new Set([
  "sid",
  "mid",
  "jwt",
  "relay-token",
  "state",
  "auth-code",
  "link",
]);

export class SecretRedactor {
  private readonly entries: Array<{ kind: SecretKind; value: string; alias: string }> = [];
  private readonly counters = new Map<SecretKind, number>();

  /** 登记真实值并返回稳定别名；空值返回 null（无副作用）。重复值复用已有别名。 */
  register(kind: SecretKind, value: string | null | undefined): string | null {
    if (!value) return null;
    const existing = this.entries.find((entry) => entry.value === value);
    if (existing) return existing.alias;
    const next = (this.counters.get(kind) ?? 0) + 1;
    this.counters.set(kind, next);
    // sid/mid 按任务约定用字母序（SID_A、MID_A…），其余数值序（JWT_1、TOK_1…）。
    const label =
      kind === "sid" || kind === "mid"
        ? `${KIND_TAG[kind]}_${String.fromCharCode(64 + next)}`
        : `${KIND_TAG[kind]}_${next}`;
    this.entries.push({ kind, value, alias: label });
    return label;
  }

  get aliasCount(): number {
    return this.entries.length;
  }

  /** 全文替换所有已登记真实值 → 别名（长值优先，避免前缀包含互相干扰）。 */
  sanitizeText(text: string): string {
    let output = text;
    const ordered = [...this.entries].sort((a, b) => b.value.length - a.value.length);
    for (const entry of ordered) {
      output = output.split(entry.value).join(entry.alias);
    }
    return output;
  }

  /** 深度遍历对象/数组，字符串统一过 sanitizeText；其余原样。 */
  sanitizeDeep<T>(value: T): T {
    if (typeof value === "string") return this.sanitizeText(value) as unknown as T;
    if (Array.isArray(value)) return value.map((item) => this.sanitizeDeep(item)) as unknown as T;
    if (value && typeof value === "object") {
      const out: Record<string, unknown> = {};
      for (const [key, item] of Object.entries(value as Record<string, unknown>)) {
        out[key] = this.sanitizeDeep(item);
      }
      return out as unknown as T;
    }
    return value;
  }

  /**
   * fail-closed：待落盘内容若仍含任何已登记真实值即抛错（调用方不得吞掉该错误——
   * 宁可丢证据，不可落密）。
   */
  assertClean(text: string, where: string): void {
    for (const entry of this.entries) {
      const enforce = STRICT_KINDS.has(entry.kind) || entry.value.length >= 4;
      if (enforce && text.includes(entry.value)) {
        throw new Error(
          `sanitize: refusing to write ${where} — raw ${entry.kind} material still present ` +
            `(should be alias ${entry.alias})`
        );
      }
    }
  }
}

function sha8(value: string): string {
  return crypto.createHash("sha256").update(value, "utf8").digest("hex").slice(0, 8);
}

/** JWT 掩码形态（任务约定）：`eyJ…<len=N> sha256:xxxxxxxx`——只泄长度与指纹。 */
export function maskJwt(jwt: string): string {
  return `eyJ…<len=${jwt.length}> sha256:${sha8(jwt)}`;
}

/** 不透明短值（一次性 relay token / 授权码等）掩码：`xx…<len=N> sha256:xxxxxxxx`。 */
export function maskOpaque(value: string): string {
  return `${value.slice(0, 2)}…<len=${value.length}> sha256:${sha8(value)}`;
}

// ---------------------------------------------------------------------------
// 私有证据目录与 fail-closed 落盘
// ---------------------------------------------------------------------------

export type EvidenceRunDir = { dir: string; runId: string };

/** 默认证据基目录：~/Documents/tuvis-verify-logs/zcode（私有目录，不入仓库）。 */
export function defaultEvidenceBase(): string {
  return path.join(os.homedir(), "Documents", "tuvis-verify-logs", "zcode");
}

/** 建 `macos-B1<UTC 时间戳>-<随机短码>` 运行目录（不覆盖旧目录：秒级时间戳+短码）。 */
export function createEvidenceRunDir(baseDir: string = defaultEvidenceBase()): EvidenceRunDir {
  const now = new Date();
  const stamp = [
    String(now.getUTCFullYear()),
    String(now.getUTCMonth() + 1).padStart(2, "0"),
    String(now.getUTCDate()).padStart(2, "0"),
    "T",
    String(now.getUTCHours()).padStart(2, "0"),
    String(now.getUTCMinutes()).padStart(2, "0"),
    String(now.getUTCSeconds()).padStart(2, "0"),
    "Z",
  ].join("");
  const rand = crypto
    .randomBytes(4)
    .toString("base64url")
    .replace(/[^a-z0-9]/gi, "")
    .slice(0, 6);
  const runId = `macos-B1${stamp}-${rand || "r00000"}`;
  const dir = path.join(baseDir, runId);
  fs.mkdirSync(dir, { recursive: true });
  return { dir, runId };
}

/**
 * fail-closed 落盘：逐文件先过 redactor.assertClean，任何真实值命中即整体拒写
 * （该文件不产生）；通过后以 owner-only 0600 写入。
 */
export function writeEvidenceFiles(
  redactor: SecretRedactor,
  dir: string,
  files: Array<{ name: string; content: string }>
): void {
  for (const file of files) {
    redactor.assertClean(file.content, file.name);
  }
  for (const file of files) {
    fs.writeFileSync(path.join(dir, file.name), file.content, { mode: 0o600, encoding: "utf8" });
  }
}
