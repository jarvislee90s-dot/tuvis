/**
 * 宿主半区（DSH Desktop `desktop` profile 内的 Cordis 插件）。
 *
 * 注册形态：cordis.patch.yml 以 `insert: {id, name}` 把本包挂进宿主入口表，
 * 宿主 Loader 导入包默认导出并应用为插件。本半区注入宿主服务
 * `connection`（API 连接）与 `webServer`（路由注册），并以「apply 返回
 * disposer」的 Cordis effect 形态保证卸载时彻底清理。
 *
 * T06：在 health 之上追加 `GET /tuvis-remote/binding-state`（本地授权卡数据源），
 * 并按**运行时门**启动绑定会话（读 rendezvous → 主动连接兔维斯 → 签名挑战 →
 * 批准后经注入兑换交付 owner Cookie）。启动门（保守激活，防止在非真实宿主/
 * 测试环境里自发外连）：
 *   ① `TUVIS_DSH_BINDING=enable` 环境变量（G1 实机探测/验证通道），或
 *   ② 注入的 `connection` 服务暴露可调用的 `authenticatedUrl`（Mac 补探测 §4.1
 *      实测确认：真实宿主恒有该函数，arity=1）。
 * 两门皆关时只注册路由（快照 idle），**绝不**连接兔维斯。
 *
 * 会话形态见 [`BindingSession`]（R04/R05）：稳定待批 + 单连接 + 读循环消费撤权。
 *
 * 兑换（/connect）按 Mac 补探测 §4.1/§4.2 的实测形状真实实现（见
 * [`createHostExchange`]）：真机上的最终确认仍属 G1-Mac，但代码不是占位。
 *
 * **访问边界（N1）**：本半区自注册的每条路由都必须自带门禁（宿主不代挡，
 * 实测见报告 §3.5）；且**没有任何授权写入口**——批准只在兔维斯 Tauri IPC。
 */

import { readFileSync, readdirSync, mkdirSync, chmodSync, writeFileSync, renameSync } from "node:fs";
import type { IncomingMessage, ServerResponse } from "node:http";
import { homedir } from "node:os";
import { basename, dirname, join } from "node:path";
import type { Plugin } from "@deepseek-ai/cordis";
import type { WebRoute, WebServer } from "@deepseek-ai/dsh-host-webserver";
import {
  BindingSession,
  type BindingStateSnapshot,
  type BindingStorage,
  type CookieExchange,
  type Rendezvous,
  type RendezvousReader,
} from "./auth.js";
import { fenceRequest, type FenceHeaders } from "./fence.js";
import { NodeIpcTransport } from "./ipc.js";
import { BINDING_PROTOCOL } from "./types.js";

export type { WebRoute };

/** 本插件需要的宿主上下文最小面（真实 Cordis Context 在运行期满足该结构）。 */
export interface HostContext {
  webServer: WebServer;
  /** 宿主连接服务（`inject: ["connection"]`；兑换注入缝的载体）。 */
  connection?: unknown;
  profileContext?: { home: string; dir: string; name: string };
}

export const name = "@tuvis/dsh-remote-access";

/** 依赖的宿主服务：connection（授权交换用）+ webServer（路由注册）。 */
export const inject: string[] = ["connection", "webServer", "profileContext"];

/** 兔维斯远控在本机 WebServer 上的路由前缀（exact 命名空间，与 /remote 聊天路由无关）。 */
export const ROUTE_PREFIX = "/tuvis-remote";

/** 绑定态路由路径（本地授权卡数据源；相对路径由 client 半区 fetch）。 */
export const BINDING_STATE_PATH = `${ROUTE_PREFIX}/binding-state`;

/** 绑定态快照的对外发布节奏（毫秒；会话内部状态变化经此反映到路由）。 */
const BINDING_STATE_PUBLISH_MS = 500;

// ============================================================
// 运行时默认件（路径推导 + 0600 存储 + rendezvous 读取 + 兑换缝）
// ============================================================

/**
 * rendezvous 路径推导（R10 统一：**与 Rust 侧逐字同义**）。
 *
 *  `TUVIS_DSH_STATE_DIR` 已设 → **它就是状态目录**，rendezvous 直接落在其下；
 *  未设 → 默认根 `~/.tuvis` 下的 `dsh/`（即 `~/.tuvis/dsh/rendezvous.json`）。
 *
 * 刻意**不读** `TUVIS_HOME`（Rust 只在 debug 读它，release 连 env 都不读；插件
 * 跟着猜会与 release 行为差一层 `.tuvis`），也不再保留插件私有的
 * `TUVIS_DSH_RENDEZVOUS` 覆盖（Rust 无处消费，等于两套机制）。
 */
function deriveStateDir(): string {
  const stateDir = process.env["TUVIS_DSH_STATE_DIR"];
  if (stateDir !== undefined && stateDir.length > 0) return stateDir;
  return join(homedir(), ".tuvis", "dsh");
}

function deriveRendezvousPath(): string {
  return join(deriveStateDir(), "rendezvous.json");
}

/**
 * 生产 rendezvous 路径（导出供测试与诊断；`TUVIS_DSH_STATE_DIR` 同义约定见上）。
 */
export { deriveRendezvousPath };

/**
 * DSH home 推导（身份存储的根；与宿主自身的 home 解析同源）。
 */
function deriveDshHome(): string {
  return process.env["DSH_HOME"] ?? join(homedir(), ".dsh");
}

/**
 * 实例 ID 规范形态（登记派生输出；H05 审查 M1：进 join 前必验）。
 */
const INSTANCE_ID_SHAPE = /^[0-9a-f]{32}$/;

/**
 * 候选记录 + 来源（H05 审查 I3/I4/M1 的路由依据）：
 *  - `"legacy"` = 根 `rendezvous.json`（旧单例域；身份走旧 profile 存储路径，逐字保留）；
 *  - `"instance"` = `instances/<id>/rendezvous.json`（实例域；身份按 hostId 分段）。
 */
export interface RendezvousCandidate {
  record: Rendezvous;
  origin: "legacy" | "instance";
  /** 实例域目录名（= 登记 ID；记录 hostId 必须与其一致——M1）。 */
  dirName?: string;
}

/**
 * 实例作用域 rendezvous 候选集（H05 按实例段）：legacy 单文件 +
 * `instances/<instanceId>/rendezvous.json` 全扫。目录名只接受 32 位十六进制
 * 注册形态（实例 ID 的派生形状；防路径遍历/防任意目录名注入）。缺 `instances/`
 * 目录 = 只有 legacy 单文件（旧单例布局照常可读）。
 */
export function rendezvousCandidatePaths(stateDir: string): string[] {
  const out = [join(stateDir, "rendezvous.json")];
  let entries: Array<{ name: string; isDirectory: () => boolean }>;
  try {
    entries = readdirSync(join(stateDir, "instances"), { withFileTypes: true });
  } catch {
    return out;
  }
  for (const entry of entries) {
    if (!entry.isDirectory()) continue;
    if (!INSTANCE_ID_SHAPE.test(entry.name)) continue;
    out.push(join(stateDir, "instances", entry.name, "rendezvous.json"));
  }
  return out;
}

/**
 * 全部候选的读取（坏文件/缺文件逐个容忍：跳过，不整体失败）。
 *  - I3：`retired = true` 的记录（实例域上线后的 legacy 交接标记）→ 忽略；
 *  - M1：实例记录的 `hostId` 必须是规范 ID **且与所在目录名一致**——不一致/
 *    穿越形状直接丢弃（绝不进任何 join）。
 */
function readRendezvousCandidates(stateDir: string): RendezvousCandidate[] {
  const legacyPath = join(stateDir, "rendezvous.json");
  const out: RendezvousCandidate[] = [];
  for (const path of rendezvousCandidatePaths(stateDir)) {
    let record: Rendezvous;
    try {
      record = JSON.parse(readFileSync(path, "utf8")) as Rendezvous;
    } catch {
      continue; // 坏文件/缺文件跳过（多实例布局下别台实例的残档不阻塞本实例）
    }
    if (record?.retired === true) continue; // I3：已交接停用的记录不参与路由
    if (path === legacyPath) {
      out.push({ record, origin: "legacy" });
      continue;
    }
    const dirName = basename(dirname(path));
    // N2：hostId 必须先过 typeof（先于任何正则/比较）——畸形记录（如 hostId 是
    // 带 toString 属性的对象）在 RegExp.test 的 ToString 转换处会抛 TypeError，
    // 单条坏记录不得阻塞全部候选读取。M1：形态/目录一致性不符 → 丢弃并记日志。
    const hostId: unknown = record?.hostId;
    if (typeof hostId !== "string" || !INSTANCE_ID_SHAPE.test(hostId) || hostId !== dirName) {
      console.warn(
        `dsh instance rendezvous record rejected (hostId shape/directory mismatch): ${dirName}`,
      );
      continue; // M1：hostId 形态/目录一致性校验失败 → 丢弃（不猜、不 join）
    }
    out.push({ record, origin: "instance", dirName });
  }
  return out;
}

/** 按宿主 PID 在候选（含来源）中选定本实例记录；零/多匹配 → null（fail-closed）。 */
function selectCandidateForPid(
  candidates: RendezvousCandidate[],
  pid: number,
): RendezvousCandidate | null {
  const hits = candidates.filter((c) => c.record?.processIdentity?.pid === pid);
  return hits.length === 1 ? (hits[0] ?? null) : null;
}

/**
 * 按宿主 PID 选定本实例的 rendezvous（H05 路由核心）：插件与宿主同进程
 * （OS peercred 核验依赖这一点），`processIdentity.pid` 即宿主进程 PID——
 * 与本进程 PID 唯一对应的那份记录就是本实例的授权域。零匹配/多匹配 →
 * `null`（fail-closed，不猜）。已停用（retired）与 hostId 不合规的记录不参与。
 */
export function selectRendezvousForPid(
  records: Rendezvous[],
  pid: number,
): Rendezvous | null {
  const hits = records.filter((r) => r?.processIdentity?.pid === pid && r?.retired !== true);
  return hits.length === 1 ? (hits[0] ?? null) : null;
}

/**
 * 生产 rendezvous 读取器（H05 按实例路由）：扫候选集（retired/非法 hostId 已滤）
 * → 按本进程 PID（= 宿主 PID）选定本实例记录。A/B 实例各自连接各自的授权域，
 * 互不串线；「先 legacy 后选择」经 retired 交接标记显式换线。
 */
export function instanceRendezvousReader(stateDir: string = deriveStateDir()): RendezvousReader {
  return {
    async read(): Promise<Rendezvous | null> {
      return selectCandidateForPid(readRendezvousCandidates(stateDir), process.pid)?.record ?? null;
    },
  };
}

/** legacy 身份文件路径（旧 deriveStorageDir 布局逐字保留；H05 审查 I4）。 */
function legacyIdentityFile(dshHome: string): string {
  return join(dshHome, "profiles", "desktop", "tuvis-remote", "identity.json");
}

/**
 * 实例域撤权证据检查（A01/A02 缺口②）：读本实例的绑定账本 `<stateDir>/instances/
 * <hostId>/bindings.json`（与 Rust 侧 `BindingLedger` 同一写入/读取链：camelCase
 * 字段 + snake_case state；撤权墓碑行 = 钥移入 `revokedKeySpki`、`publicKeySpki`
 * 清空）。任一 `state == "revoked"` 行的 `revokedKeySpki` 与候选钥相同 → 该钥已被
 * **本实例域**撤权。账本缺失/畸形/格式不认 → 视为无证据（不因此拒绝）；spki 比对
 * = base64url 字符串值等（两端同链产生，逐字节等值）。只读本实例的账本路径，
 * 其他实例的墓碑互不可见（按 hostId 作用域）。
 */
function keyIsInstanceRevoked(ledgerFile: string, candidateSpki: string): boolean {
  let rows: unknown;
  try {
    rows = (JSON.parse(readFileSync(ledgerFile, "utf8")) as { bindings?: unknown })?.bindings;
  } catch {
    return false;
  }
  if (!Array.isArray(rows)) return false;
  return rows.some((row) => {
    if (row === null || typeof row !== "object") return false;
    const entry = row as { state?: unknown; revokedKeySpki?: unknown; revoked_key_spki?: unknown };
    if (typeof entry.state !== "string" || entry.state.toLowerCase() !== "revoked") return false;
    const spki = entry.revokedKeySpki ?? entry.revoked_key_spki;
    return typeof spki === "string" && spki === candidateSpki;
  });
}

/**
 * 显式迁移（H05 审查 I4；A01/A02 撤权守卫）：legacy 身份在场且实例槽为空 → 把
 * **同一把钥**带进实例域（非破坏：legacy 文件不动、幂等：写完即从实例槽直读）。
 * 信任锚与撤权墓碑是**域关系**——实例域是另一套 Rust 授权钥，必须重锚 + 全新批准
 * （TOFU pending）。撤权证据闸（任一命中 → 拒绝迁移，**不迁已撤钥**）：
 *   ① legacy 文件自身的 `revoked === true`（旧域内已撤的钥）；
 *   ② 实例域撤权账本指名本钥——Web 实例曾用迁移钥授权、退役撤权后重装时实例槽
 *     为空，绝不把同一把已撤钥以 `revoked: false` 复活。
 * 迁移失败/被拒（无/坏 legacy 文件、撤权证据命中）→ null = 全新生成路径
 * （TOFU pending 重新批准），绝不抛错卡死插件启动。
 */
function migrateLegacyIdentity(
  instanceFile: string,
  legacyFile: string,
  instanceLedgerFile: string | null,
): string | null {
  let raw: string;
  try {
    raw = readFileSync(legacyFile, "utf8");
  } catch {
    return null;
  }
  let parsed: { privateKeyPem?: unknown; publicKeySpki?: unknown; revoked?: unknown };
  try {
    parsed = JSON.parse(raw) as { privateKeyPem?: unknown; publicKeySpki?: unknown; revoked?: unknown };
  } catch {
    return null;
  }
  if (typeof parsed.privateKeyPem !== "string" || typeof parsed.publicKeySpki !== "string") {
    return null;
  }
  if (parsed.revoked === true) return null;
  if (instanceLedgerFile !== null && keyIsInstanceRevoked(instanceLedgerFile, parsed.publicKeySpki)) {
    return null;
  }
  const migrated = JSON.stringify({
    version: 1,
    privateKeyPem: parsed.privateKeyPem,
    publicKeySpki: parsed.publicKeySpki,
    trustAnchorSpki: null,
    revoked: false,
    revokedBindingId: null,
  });
  mkdirSync(dirname(instanceFile), { recursive: true });
  if (process.platform !== "win32") chmodSync(dirname(instanceFile), 0o700);
  const tmp = `${instanceFile}.tmp`;
  writeFileSync(tmp, migrated, { mode: 0o600 });
  if (process.platform !== "win32") chmodSync(tmp, 0o600);
  renameSync(tmp, instanceFile);
  return migrated;
}

/**
 * 实例作用域身份存储（H05 审查 I4 重构·按**选定记录的来源**路由）：
 *  - 选定 legacy 记录（无选择/未交接的旧单例域）→ **旧 profile 存储路径逐字保留**
 *    （`<DSH_HOME>/profiles/desktop/tuvis-remote/identity.json`）——旧钥/信任锚/
 *    撤权墓碑原样可用，绝不因升级触发重新授权；
 *  - 选定实例记录 → `<DSH_HOME>/tuvis-remote/<hostId>/identity.json`，实例槽为空
 *    时执行上面的显式迁移（同钥、清域关系标记；撤权证据命中则拒绝——不迁已撤钥，
 *    见 [`migrateLegacyIdentity`]）。
 *
 * 未选到 rendezvous（未发布/多义/全部停用）→ 读 null、写明确失败（fail-closed）。
 */
export function instanceFileStorage(
  dshHome: string,
  stateDir: string,
  pid: number = process.pid,
): BindingStorage {
  const resolveTarget = (): {
    file: string;
    origin: "legacy" | "instance";
    record: Rendezvous;
    /** 实例域撤权账本路径（A01/A02 迁移守卫证据源；legacy 域无实例账本 = null）。 */
    ledgerFile: string | null;
  } | null => {
    const candidate = selectCandidateForPid(readRendezvousCandidates(stateDir), pid);
    if (candidate === null) return null;
    if (candidate.origin === "legacy") {
      // I4：无选择语义逐字保留——旧存储路径（含其 desktop 布局）。
      return { file: legacyIdentityFile(dshHome), origin: "legacy", record: candidate.record, ledgerFile: null };
    }
    // M1 双保险：join 前再验 hostId 形态（候选读取已核过目录一致性）。
    if (!INSTANCE_ID_SHAPE.test(candidate.record.hostId)) return null;
    return {
      file: join(dshHome, "tuvis-remote", candidate.record.hostId, "identity.json"),
      origin: "instance",
      record: candidate.record,
      // 实例撤权账本（Rust BindingLedger 同一写入/读取链），按本实例 hostId 分段。
      ledgerFile: join(stateDir, "instances", candidate.record.hostId, "bindings.json"),
    };
  };
  const ensure = (dir: string): void => {
    mkdirSync(dir, { recursive: true });
    if (process.platform !== "win32") chmodSync(dir, 0o700);
  };
  type Target = NonNullable<ReturnType<typeof resolveTarget>>;
  const makeStorage = (fixed?: Target | null): BindingStorage => ({
    async read() {
      const target = fixed === undefined ? resolveTarget() : fixed;
      if (target === null) return null;
      try {
        return readFileSync(target.file, "utf8");
      } catch {
        if (target.origin !== "instance") return null;
        return migrateLegacyIdentity(target.file, legacyIdentityFile(dshHome), target.ledgerFile);
      }
    },
    async write(data: string) {
      const current = resolveTarget();
      const target = fixed === undefined ? current : fixed;
      if (target === null || current === null) {
        throw new Error("dsh instance storage: no selectable rendezvous");
      }
      if (target.file !== current.file || target.origin !== current.origin) {
        throw new Error("dsh instance storage: authorization domain changed");
      }
      ensure(dirname(target.file));
      const tmp = `${target.file}.tmp`;
      writeFileSync(tmp, data, { mode: 0o600 });
      if (process.platform !== "win32") chmodSync(tmp, 0o600);
      renameSync(tmp, target.file);
    },
    scope: () => makeStorage(resolveTarget()),
    matchesRendezvous(rv: Rendezvous) {
      const target = fixed === undefined ? resolveTarget() : fixed;
      const current = resolveTarget();
      return target !== null && current !== null && target.file === current.file &&
        target.origin === current.origin && JSON.stringify(target.record) === JSON.stringify(current.record) &&
        JSON.stringify(target.record) === JSON.stringify(rv);
    },
  });
  return makeStorage();
}

/**
 * 兑换失败的类型化错误：只暴露**阶段名**，绝不携带 token / cookie / 对端 URL。
 *
 * 为什么单独一个类：调用方（绑定会话）只需按阶段决定重试与否；同时保证任何
 * `console.error(err)` / 栈打印都不可能把授权材料带出去（§4.2 的 token 查询串
 * 与 Cookie 值都是凭据级秘密）。
 */
export class CookieExchangeError extends Error {
  readonly stage: CookieExchangeStages;

  constructor(stage: CookieExchangeStages, detail: string) {
    // 消息里只允许阶段与固定文案——detail 由本模块自己写死，不含请求数据。
    super(`dsh /connect exchange failed at stage "${stage}": ${detail}`);
    this.name = "CookieExchangeError";
    this.stage = stage;
  }
}

/** 兑换阶段（诊断用；每个 stage 对应一处可观测的契约偏离）。 */
export type CookieExchangeStages =
  | "authenticated-url"
  | "connect-request"
  | "connect-status"
  | "set-cookie";

/** 宿主 WebServer 上兑换所需的最小面（官方 `ctx.webServer`）。 */
export interface ExchangeWebServer {
  readonly protocol: "http:" | "https:";
  readonly port: number | undefined;
}

/** 从 `set-cookie` 头里挑出官方认证 cookie 并返回完整 `name=value`。 */
function pickAuthCookie(setCookieHeaders: readonly string[]): string | null {
  for (const header of setCookieHeaders) {
    const pair = header.split(";", 1)[0]?.trim() ?? "";
    const eq = pair.indexOf("=");
    if (eq <= 0) continue;
    // 官方名口径：`dsh-auth-<base64url(sha256(authority))>`（§4.2 逐字核验过）
    if (!pair.slice(0, eq).startsWith("dsh-auth-")) continue;
    return pair;
  }
  return null;
}

/**
 * 真实 Cookie 兑换（报告 §4.1/§4.2 的实测形状，不再是占位）。
 *
 * 链路：
 *  1. 由 `ctx.webServer` 的 protocol+port 拼出**绝对** origin
 *     （官方 Desktop 宿主同款：`${protocol}//127.0.0.1:${port}`）；
 *  2. `connection.authenticatedUrl(origin)`（arity=1，入参必须是可 `new URL()` 的
 *     绝对 URL；返回 `http://127.0.0.1:<port>/?token=<43 字符>`）；
 *  3. `GET <origin>/connect?token=<T>&interactions=answer`，**强制
 *     `redirect: "manual"`**——自动跟随会把 302 变成 200 页面并丢掉 `set-cookie`
 *     的原始形态；
 *  4. 断言 302，取 `set-cookie` 里 `dsh-auth-*` 的完整 `name=value`（cookie 只在
 *     内存里活着，随它那条 mux socket 生死）。
 *
 * 任何偏离都抛 [`CookieExchangeError`]（带阶段，不带秘密）。本路径的**真机**
 * 验证仍待 G1-Mac（Desktop 宿主上重跑同一形状），但代码是真实实现而非占位。
 *
 * @param connection - 宿主连接服务（`inject: ["connection"]`）。
 * @param webServer - 宿主 WebServer（取 protocol/port 拼 origin）。
 * @param fetchImpl - 注入的 fetch（测试注入假件；生产走宿主全局 fetch）。
 */
export function createHostExchange(
  connection: unknown,
  webServer: ExchangeWebServer,
  fetchImpl: typeof fetch = fetch,
): CookieExchange {
  return {
    async exchange(): Promise<string> {
      const svc = connection as { authenticatedUrl?: unknown } | undefined;
      if (typeof svc?.authenticatedUrl !== "function") {
        throw new CookieExchangeError(
          "authenticated-url",
          "host connection service does not expose authenticatedUrl",
        );
      }
      const port = webServer.port;
      if (port === undefined) {
        throw new CookieExchangeError("authenticated-url", "host webServer has no listening port");
      }
      // 官方宿主只对回环 authority 兑换；这里固定 127.0.0.1（不信任任何请求头）。
      const origin = `${webServer.protocol}//127.0.0.1:${String(port)}`;

      let tokenUrl: unknown;
      try {
        tokenUrl = (svc.authenticatedUrl as (url: string) => unknown)(origin);
      } catch {
        throw new CookieExchangeError("authenticated-url", "authenticatedUrl rejected the origin");
      }
      if (typeof tokenUrl !== "string") {
        throw new CookieExchangeError("authenticated-url", "authenticatedUrl did not return a URL");
      }
      let token: string | null;
      try {
        // 只取 token 这一个查询参数；URL 其余部分不参与后续请求拼接（防串改）。
        token = new URL(tokenUrl).searchParams.get("token");
      } catch {
        throw new CookieExchangeError("authenticated-url", "authenticatedUrl returned a relative URL");
      }
      if (token === null || token.length === 0) {
        throw new CookieExchangeError("authenticated-url", "authenticatedUrl returned no token");
      }

      let res: Response;
      try {
        res = await fetchImpl(`${origin}/connect?token=${token}&interactions=answer`, {
          method: "GET",
          redirect: "manual",
          // 不跟系统代理、不带任何凭证（本机回环直连）。
          headers: { accept: "text/plain" },
        });
      } catch {
        // 只有阶段，不带底层错误串（它可能含完整 URL = token）。
        throw new CookieExchangeError("connect-request", "request to /connect failed");
      }

      if (res.status !== 302) {
        throw new CookieExchangeError("connect-status", `expected 302 but received ${res.status}`);
      }

      const setCookies =
        typeof res.headers.getSetCookie === "function"
          ? res.headers.getSetCookie()
          : [res.headers.get("set-cookie") ?? ""].filter((value) => value.length > 0);
      const cookie = pickAuthCookie(setCookies);
      if (cookie === null) {
        throw new CookieExchangeError("set-cookie", "response carries no dsh-auth cookie");
      }
      return cookie;
    },
  };
}

/**
 * 浏览器会话凭据兑换：`GET /?token=<launchToken>` → 303 + 浏览器 cookie。
 *
 * 为什么必须单独一趟（P1-2a）：官方 `authorizeIndex` 只接受 payload
 * `version: 1` 的**浏览器** cookie，而 `/connect` 签的是 `version: 2` 的 **API
 * operator** 凭据。两者不可互换——拿 API 凭据请求首页恒未授权，控制台会得到一个
 * 打不开的页面。两个入口用的是同一个 launch token（`authenticatedUrl` 产出），
 * 因此这里复用同一条 token 获取逻辑，只换请求路径与期望的重定向码。
 *
 * 失败语义：**不抛**（返回 null）。首页凭据缺失时控制台的 IPC/API 通道仍然可用，
 * 调用方据此把「首页」这条能力如实标为不可用，而不是让整个绑定失败。
 */
export function createBrowserCookieExchange(
  connection: unknown,
  webServer: ExchangeWebServer,
  fetchImpl: typeof fetch = fetch,
): () => Promise<string | null> {
  return async () => {
    const svc = connection as { authenticatedUrl?: unknown } | undefined;
    if (typeof svc?.authenticatedUrl !== "function") return null;
    const port = webServer.port;
    if (port === undefined) return null;
    const origin = `${webServer.protocol}//127.0.0.1:${String(port)}`;
    let tokenUrl: unknown;
    try {
      tokenUrl = (svc.authenticatedUrl as (url: string) => unknown)(origin);
    } catch {
      return null;
    }
    if (typeof tokenUrl !== "string") return null;
    let token: string | null;
    try {
      token = new URL(tokenUrl).searchParams.get("token");
    } catch {
      return null;
    }
    if (token === null || token.length === 0) return null;
    let res: Response;
    try {
      res = await fetchImpl(`${origin}/?token=${token}`, {
        method: "GET",
        redirect: "manual",
        headers: { accept: "text/html" },
      });
    } catch {
      return null;
    }
    // `authorizeIndex` 成功时是 303（`/connect` 的 API 路径是 302）。
    if (res.status !== 303) return null;
    const setCookies =
      typeof res.headers.getSetCookie === "function"
        ? res.headers.getSetCookie()
        : [res.headers.get("set-cookie") ?? ""].filter((value) => value.length > 0);
    return pickAuthCookie(setCookies);
  };
}

// ============================================================
// 健康探针 + 绑定态路由
// ============================================================

/**
 * 路由包装：先过逐路由门禁（N1），再过处理函数。
 *
 * **为什么每条路由都要包**（报告 §3.5）：自注册 `webServer` 路由**不继承**官方
 * `/api` 认证与 Host/Origin 围栏，伪装 Host/Origin 的请求照样 200。因此门禁在
 * 这里逐个套上，而不是假设宿主帮挡（判据见 ./fence.ts，与 §11.3 矩阵逐格一致）。
 *
 * 拒绝一律 403 + 固定短原因：不回声攻击者输入、不泄漏主机内部信息。
 */
function fenced(
  handler: (req: IncomingMessage, res: ServerResponse) => void,
): (req: IncomingMessage, res: ServerResponse) => void {
  return (req, res) => {
    const verdict = fenceRequest(req.headers as FenceHeaders);
    if (!verdict.allowed) {
      res.writeHead(403, { "content-type": "text/plain; charset=utf-8" });
      res.end(verdict.reason);
      return;
    }
    handler(req, res);
  };
}

/**
 * 健康探针：证明 bundle 已装进 profile 且 effect 挂载成功。
 *
 * 响应体刻意最小（插件名 + 协议代际）：不含主机名/端口/路径/进程身份——
 * 该路由只回答「在不在」，不回答「这是哪台机器、跑在哪里」。
 */
function healthHandler(_req: IncomingMessage, res: ServerResponse): void {
  res.writeHead(200, { "content-type": "application/json" });
  res.end(JSON.stringify({ plugin: name, bindingProtocol: BINDING_PROTOCOL }));
}

function jsonHandler(payload: () => BindingStateSnapshot): (req: IncomingMessage, res: ServerResponse) => void {
  return (_req, res) => {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify(payload()));
  };
}

/** 绑定循环是否允许启动（见文件头「启动门」）。 */
export function bindingAutoStartEnabled(ctx: HostContext): boolean {
  if (process.env["TUVIS_DSH_BINDING"] === "enable") return true;
  const svc = ctx.connection as { authenticatedUrl?: unknown } | undefined;
  return typeof svc?.authenticatedUrl === "function";
}

/**
 * 插件主体：注册路由 + （过门时）启动绑定会话，返回 disposer。
 *
 * 必须是箭头函数：Cordis 用 isConstructor 分派插件形态，function 声明
 * 可被 new（有 [[Construct]]），会被误当类插件走构造路径、丢掉返回的
 * disposer（dispose 测试抓获过这一真实缺陷）。
 */
export const apply = (ctx: HostContext): (() => void) => {
  const unbinders: Array<() => void> = [];

  const snapshot: BindingStateSnapshot = { state: "idle", bindingId: null, displayCode: null };
  // 路由表 = 全部插件路由。**只读两条，永不新增授权写入口**：用户批准只发生在
  // 兔维斯 Tauri 设置卡（IPC），插件侧没有任何 approve/grant/revoke 路由——
  // 这条不变量由 fence.test.ts 的 `only_two_named_routes_exist_and_both_are_fenced`
  // 守住（新增路由必须同时改该用例，等于强制一次复核）。
  // snapshot 由绑定会话就地更新（同一对象引用），路由读到的永远是最新值。
  const routes: WebRoute[] = [
    { kind: "exact", path: `${ROUTE_PREFIX}/health`, handler: fenced(healthHandler) },
    { kind: "exact", path: BINDING_STATE_PATH, handler: fenced(jsonHandler(() => ({ ...snapshot }))) },
  ];
  for (const route of routes) {
    const unregister = ctx.webServer.register(route);
    unbinders.push(unregister);
  }

  // 运行时门：只在真实宿主信号或显式 env 开关下启动绑定会话（绝不自发外连）。
  // 门 ② = 宿主暴露可调用的 `authenticatedUrl`（Mac 报告 §4.1 实测确认存在），
  // 这是「真的是官方宿主」的探针级信号；TUVIS_DSH_BINDING=enable 保留为测试/
  // 影子环境的显式覆盖。
  if (bindingAutoStartEnabled(ctx)) {
    // H05 按实例路由：rendezvous 按本进程 PID（= 宿主 PID）选定本实例记录，
    // 身份存储按该记录的 hostId 分段——多实例并装时各自的绑定/凭据互不可见。
    const session = new BindingSession({
      transport: new NodeIpcTransport(),
      homeBinding: (rv) => {
        const homeRoot = ctx.profileContext?.home;
        if (!homeRoot) return undefined;
        return { homeRoot, hostPid: process.pid, hostStartedAtMs: rv.processIdentity.startedAtMs };
      },
      rendezvous: instanceRendezvousReader(deriveStateDir()),
      storage: instanceFileStorage(ctx.profileContext?.home ?? deriveDshHome(), deriveStateDir()),
      exchange: createHostExchange(ctx.connection, ctx.webServer),
      // P1-2a：官方首页要求**浏览器**会话 cookie（payload version 1），与
      // `/connect` 签出的 API operator 凭据（version 2）不可互换。控制台代理用
      // 这一份拉首页与静态资源；兑换失败留空（首页通道如实标不可用）。
      browserExchange: createBrowserCookieExchange(ctx.connection, ctx.webServer),
      clock: () => Date.now(),
    });
    let published = snapshot;
    const publish = (): void => {
      const next = session.snapshot();
      if (
        next.state === published.state &&
        next.bindingId === published.bindingId &&
        next.displayCode === published.displayCode
      ) {
        return;
      }
      // 就地更新（路由闭包持有同一对象）；
      snapshot.state = next.state;
      snapshot.bindingId = next.bindingId;
      snapshot.displayCode = next.displayCode;
      published = { ...snapshot };
    };
    const timer = setInterval(publish, BINDING_STATE_PUBLISH_MS);
    session.start();
    unbinders.push(() => {
      clearInterval(timer);
      session.dispose();
    });
  }

  return () => {
    for (const unbind of unbinders.splice(0)) unbind();
  };
};

const plugin: Plugin.Object = { name, inject, apply };

export default plugin;
