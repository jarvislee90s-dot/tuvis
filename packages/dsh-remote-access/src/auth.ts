/**
 * 本机双向身份授权（T06 插件半区）。
 *
 * 插件读兔维斯发布的 rendezvous（`TUVIS_DSH_STATE_DIR/rendezvous.json`，只含
 * 随机 IPC 地址/协议代际/进程身份/Rust 公钥）**主动连接**；以 Node 内置 crypto
 * 做 Ed25519 签名挑战（与 Rust 侧 ed25519-dalek 同曲线同消息字节）。首次绑定 =
 * TOFU + 人工确认 + 两端核对码：用户在兔维斯 UI 批准前，本侧**不兑换、不交付**
 * 任何凭据材料；撤权后停止交付新凭据（直到显式重置身份）。
 *
 * 数据根（R10 统一）：`TUVIS_DSH_STATE_DIR` **就是**状态目录（两端同名同义，
 * Rust 侧同一解析口径）；未设则回落 `~/.tuvis/dsh`。本模块**不读** `TUVIS_HOME`
 * （那是 Rust debug 构建的数据根旋钮，插件猜它会与 release 行为分裂），也不再有
 * `TUVIS_DSH_RENDEZVOUS` 私有覆盖——路径推导在 index.ts 的 deriveRendezvousPath。
 *
 * 身份存储假设（生产接线处，G1 核对）：[`BindingStorage`] 的生产实现落
 * profile 作用域目录（`$DSH_HOME|~/.dsh`/profiles/desktop/tuvis-remote/），
 * 0600 等价权限；本模块不管路径推导，只消费注入的存储面。
 */

import {
  createHash,
  createPrivateKey,
  createPublicKey,
  generateKeyPairSync,
  randomBytes,
  sign as cryptoSign,
  verify as cryptoVerify,
} from "node:crypto";
import { BINDING_PROTOCOL } from "./types.js";

export { BINDING_PROTOCOL };

// ============================================================
// 注入面
// ============================================================

/** rendezvous 记录（与 Rust `RendezvousRecord` 逐字段对应；非敏感）。 */
export interface Rendezvous {
  bindingProtocol: number;
  address: string;
  hostId: string;
  /** 十进制字符串（u64 全域超出 JS 安全整数）。 */
  epoch: string;
  processIdentity: {
    platform: string;
    pid: number;
    startedAtMs: number;
    executable: string;
  };
  publicKeySpki: string;
  /**
   * 停用标记（H05 审查 I3 交接）：实例域上线时把仍在场的 legacy 记录改写为
   * `true`——候选读取据此忽略，同宿主 PID 不再双匹配停摆。缺席（旧记录）= false。
   */
  retired?: boolean;
}

export interface RendezvousReader {
  read(): Promise<Rendezvous | null>;
}

/** 插件身份存储（profile 作用域；0600 等价权限由实现保证）。 */
export interface BindingStorage {
  read(): Promise<string | null>;
  write(data: string): Promise<void>;
  scope?(): BindingStorage;
  matchesRendezvous?(rv: Rendezvous): boolean;
}

/** 官方 API Cookie 兑换（薄注入；真实 /connect 在宿主内）。 */
export interface CookieExchange {
  exchange(): Promise<string>;
}

/** 传输抽象（见 ./ipc.js；测试注入假传输）。 */
export interface IpcConnection {
  send(frame: string): Promise<void>;
  recv(): Promise<string>;
  close(): void;
  readonly closed: boolean;
}

export interface IpcTransport {
  connect(address: string): Promise<IpcConnection>;
}

// ============================================================
// 签名消息（固定 6 字段、固定顺序；与 Rust BindingChallengeMessage 逐字节一致）
// ============================================================

export interface HomeBinding {
  homeRoot: string;
  hostPid: number;
  hostStartedAtMs: number;
}

export interface BindingChallengeMessage {
  bindingProtocol: number;
  /** [Rust 公钥摘要, 客户端公钥摘要]（SHA-256 hex）。 */
  keyDigests: [string, string];
  nonce: string;
  hostId: string;
  /** 十进制字符串（跨 JS 精度）。 */
  epoch: string;
  expiry: number;
  homeBinding?: HomeBinding;
}

/** 被签字节（紧凑 UTF-8 JSON；对象字面量插入序 = wire 字段序，不许改动）。 */
export function signingBytes(msg: BindingChallengeMessage): Buffer {
  return Buffer.from(
    JSON.stringify({
      bindingProtocol: msg.bindingProtocol,
      keyDigests: msg.keyDigests,
      nonce: msg.nonce,
      hostId: msg.hostId,
      epoch: msg.epoch,
      expiry: msg.expiry,
      ...(msg.homeBinding === undefined
        ? {}
        : {
            homeBinding: {
              homeRoot: msg.homeBinding.homeRoot,
              hostPid: msg.homeBinding.hostPid,
              hostStartedAtMs: msg.homeBinding.hostStartedAtMs,
            },
          }),
    }),
    "utf8"
  );
}

// ============================================================
// wire 帧（与 Rust auth.rs 同形状；NDJSON 一行一帧）
// ============================================================

export interface HelloFrame {
  type: "hello";
  homeBinding: HomeBinding;
  bindingProtocol: number;
  publicKeySpki: string;
  executable: string;
  pid: number;
  startedAtMs: number;
  hostId: string;
  epoch: string;
  nonce: string;
  expiry: number;
  signature: string;
}

export interface CredentialFrame {
  type: "credential";
  bindingId: string;
  /**
   * **API 凭据**（`/connect?interactions=answer` 的 302 set-cookie，payload
   * `version: 2` + operator id）。只够开 `/api/remote.mux` 这条 Peer 通道，
   * **不足以加载官方首页**——首页要浏览器会话 cookie（见 `browserCookie`）。
   */
  cookie: string;
  /**
   * **浏览器会话 cookie**（`GET /?token=<launchToken>` 的 303 set-cookie，payload
   * `version: 1`）。官方首页的 `authorizeIndex` 只认这个版本；拿 API 凭据请求
   * 首页恒未授权。控制台代理必须用它拉首页与静态资源。
   *
   * 可选：旧对端不发该字段 → 控制台首页通道明确不可用（不拿 API 凭据顶替，
   * 那只会得到一个恒失败的页面）。
   */
  browserCookie?: string;
}

export type ClientFrame = HelloFrame | CredentialFrame;

/**
 * 兑换并构造交付帧（P1-2a）：API 凭据必需，浏览器凭据尽力而为。
 *
 * 为什么浏览器凭据不阻塞交付：API 凭据撑起 `/api/remote.mux`（消息/子任务等
 * 核心能力），浏览器凭据只决定**官方首页**能否在控制台里加载。首页凭据拿不到时
 * 应当把「首页不可用」如实上报，而不是让整个绑定失败——但**绝不**用 API 凭据
 * 去顶替它（那只会得到一个恒 401 的页面）。
 *
 * **API 凭据由调用方兑换后传入**：交付路径必须保证一次交付只兑换一次
 * （重复兑换会让服务端看到两个 peer，也会让用例的「恰好一次」断言失败）。
 */
async function buildCredentialFrame(
  options: { browserExchange?: () => Promise<string | null> },
  bindingId: string,
  cookie: string,
): Promise<CredentialFrame> {
  let browserCookie: string | undefined;
  if (options.browserExchange) {
    try {
      const value = await options.browserExchange();
      if (typeof value === "string" && value.length > 0) browserCookie = value;
    } catch {
      // 首页凭据兑不出来：留空，由控制台如实标「首页通道不可用」。
      browserCookie = undefined;
    }
  }
  const frame: CredentialFrame = { type: "credential", bindingId, cookie };
  if (browserCookie !== undefined) frame.browserCookie = browserCookie;
  return frame;
}

export interface ServerHelloFrame {
  type: "server_hello";
  homeBinding: HomeBinding;
  bindingProtocol: number;
  publicKeySpki: string;
  nonce: string;
  expiry: number;
  hostId: string;
  epoch: string;
  signature: string;
  decisionSignature: string;
  state: string;
  bindingId: string | null;
  displayCode: string | null;
  /**
   * 待批记录的**真实审批截止**（毫秒；仅 `pending_approval`，非秘密、不参与签名）。
   *
   * 有效期内同钥重连是幂等的（同一对 id/码），但 `expiry` 是**本次挑战**的新到期
   * （now+60s），记录的实际截止仍是首次那一个。Rust 侧显式带出记录截止，插件据此
   * 把到期定时器裁定在两者的**较早者**上——否则 T+55s 重连拿回来的码会显示成
   * 「还能活 60 秒」，实际 5 秒后就批不了（点了就是 AUTH_EXPIRED）。
   */
  pendingExpiresAt?: number | null;
}

export type ServerFrame =
  | ServerHelloFrame
  | { type: "granted"; bindingId: string; event?: string; eventSignature?: string }
  | { type: "revoked"; bindingId: string; event?: string; eventSignature?: string }
  | { type: "error"; error: string; detail: string };

/** 挑战 TTL（毫秒；与 Rust CHALLENGE_TTL_MS 一致）。 */
export const CHALLENGE_TTL_MS = 60_000;

/**
 * 待批应答的**真实**审批截止（F3/复审 Important-3）。
 *
 * `expiry` 是本次挑战的新到期，而服务端记录的审批截止可能更早（有效期内同钥重连
 * 是幂等的，记录不换新）。两端取较早者：显示与定时器都必须对齐「实际还能批到哪
 * 一刻」，否则重连回来的码会看起来比真实寿命长。
 *
 * 字段缺席（旧版 Rust / authorized 路径）时退回 `expiry`——行为与修复前等价。
 */
function pendingDeadlineOf(frame: ServerHelloFrame): number {
  const recordDeadline = frame.pendingExpiresAt;
  // 只采信**正数**截止：缺席/非数字/0 或负数（旧版 Rust、authorized 路径、
  // 畸形帧）一律退回本轮 expiry——宁可退回修复前行为，也不拿一个假 deadline
  // 把定时器武装成「立刻到期」。
  const trusted = typeof recordDeadline === "number" && recordDeadline > 0;
  if (!trusted) return frame.expiry;
  return Math.min(frame.expiry, recordDeadline);
}

// ============================================================
// base64url（Node 无内建 urlsafe 无填充便捷层；自包一层）
// ============================================================

function b64url(bytes: Buffer): string {
  return bytes.toString("base64").replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function sha256Hex(bytes: Buffer): string {
  return createHash("sha256").update(bytes).digest("hex");
}

// ============================================================
// 身份存储形状
// ============================================================

interface StoredIdentity {
  version: 1;
  /** 私钥 PKCS8 PEM（秘密；0600 存储由注入实现保证，绝不入日志/DTO）。 */
  privateKeyPem: string;
  /** 本端公钥（SPKI DER base64url；非秘密）。 */
  publicKeySpki: string;
  /** 信任锚：首缘固定的 Rust 公钥（后续 rendezvous 不一致即拒——防假宿主）。 */
  trustAnchorSpki: string | null;
  /** 撤权态：停止交付新凭据，直到 resetIdentity。 */
  revoked: boolean;
  revokedBindingId: string | null;
}

interface Identity {
  privateKey: ReturnType<typeof createPrivateKey>;
  spki: Buffer;
  raw: StoredIdentity;
}

// ============================================================
// 握手结果与客户端
// ============================================================

/** 一次握手尝试的结果观测。 */
export type HandshakeOutcome =
  | { state: "pending_approval"; bindingId: string; displayCode: string; expiresAtMs: number }
  | { state: "authorized"; bindingId: string; cookieDelivered: boolean }
  | { state: "revoked"; bindingId: string | null }
  | { state: "refused"; reason: string };

function resolveHomeBinding(
  source: HomeBinding | ((rv: Rendezvous) => HomeBinding | undefined) | undefined,
  rv: Rendezvous,
): HomeBinding | null {
  const value = typeof source === "function" ? source(rv) : source;
  if (!value || typeof value.homeRoot !== "string" || !value.homeRoot.trim() ||
      !Number.isSafeInteger(value.hostPid) || value.hostPid <= 0 ||
      !Number.isSafeInteger(value.hostStartedAtMs) || value.hostStartedAtMs <= 0 ||
      value.hostPid !== rv.processIdentity.pid || value.hostStartedAtMs !== rv.processIdentity.startedAtMs) {
    return null;
  }
  return value;
}

function matchesHomeBinding(actual: HomeBinding | undefined, expected: HomeBinding): boolean {
  return !!actual && actual.homeRoot === expected.homeRoot &&
    actual.hostPid === expected.hostPid && actual.hostStartedAtMs === expected.hostStartedAtMs;
}

function matchesServerHello(frame: ServerHelloFrame, msg: BindingChallengeMessage, rv: Rendezvous, home: HomeBinding): boolean {
  return frame.bindingProtocol === BINDING_PROTOCOL && frame.publicKeySpki === rv.publicKeySpki &&
    frame.nonce === msg.nonce && frame.expiry === msg.expiry && frame.hostId === msg.hostId &&
    frame.epoch === msg.epoch && matchesHomeBinding(frame.homeBinding, home) &&
    typeof frame.signature === "string" && typeof frame.bindingId === "string" && frame.bindingId.length > 0 &&
    typeof frame.decisionSignature === "string" && Object.hasOwn(frame, "displayCode") &&
    Object.hasOwn(frame, "pendingExpiresAt") &&
    (frame.state === "authorized" || frame.state === "pending_approval" || frame.state === "revoked");
}

function decisionBytes(msg: BindingChallengeMessage, frame: ServerHelloFrame): Buffer {
  return Buffer.from(JSON.stringify({
    purpose: "tuvis.dsh.binding-decision.v2",
    ...JSON.parse(signingBytes(msg).toString("utf8")),
    state: frame.state,
    bindingId: frame.bindingId,
    displayCode: frame.displayCode,
    pendingExpiresAt: frame.pendingExpiresAt ?? null,
  }), "utf8");
}

function verifyEvent(
  frame: ServerFrame | null, event: "approval" | "credential_ack" | "revocation",
  bindingId: string, msg: BindingChallengeMessage, decisionSignature: string, rv: Rendezvous,
): boolean {
  if (!frame || (frame.type !== "granted" && frame.type !== "revoked") ||
      frame.type !== (event === "revocation" ? "revoked" : "granted") ||
      frame.bindingId !== bindingId || frame.event !== event || typeof frame.eventSignature !== "string") return false;
  try {
    const data = Buffer.from(JSON.stringify({
      purpose: "tuvis.dsh.binding-event.v2",
      ...JSON.parse(signingBytes(msg).toString("utf8")),
      decisionSignature,
      event,
      bindingId,
    }), "utf8");
    return cryptoVerify(null, data,
      createPublicKey({ key: Buffer.from(rv.publicKeySpki, "base64"), type: "spki", format: "der" }),
      Buffer.from(frame.eventSignature, "base64"));
  } catch {
    return false;
  }
}

function decodeServerFrame(line: string): ServerFrame | null {
  try {
    const value = JSON.parse(line);
    if (!value || typeof value.type !== "string") return null;
    if (value.type === "granted" || value.type === "revoked") {
      const id = value.bindingId ?? value.binding_id;
      if (typeof id !== "string" || !id) return null;
      return { type: value.type, bindingId: id, event: value.event, eventSignature: value.eventSignature };
    }
    if (value.type !== "server_hello" && value.type !== "error") return null;
    return value as ServerFrame;
  } catch {
    return null;
  }
}

export interface BindingClientOptions {
  homeBinding?: HomeBinding | ((rv: Rendezvous) => HomeBinding | undefined);
  transport: IpcTransport;
  rendezvous: RendezvousReader;
  storage: BindingStorage;
  exchange: CookieExchange;
  /**
   * 浏览器会话凭据兑换（P1-2a；可选）。缺省 = 控制台首页通道不可用。
   * 返回 null 表示兑换失败/不可用——**不拿 API 凭据顶替**。
   */
  browserExchange?: () => Promise<string | null>;
  /** 毫秒时钟（测试注入假时钟）。 */
  clock: () => number;
}

export type BindingClientState = "idle" | "pending" | "bound" | "revoked";

/**
 * 单次握手客户端（**wire 形状的单元测试面，非生产装配**）。
 *
 * `runOnce()` 每次调用开一条新连接、握手结束即按需关闭——这正是 R04/R05 判定
 * 不足的形态（待批不保活、绑定后无人读、重连换码）。**生产装配请用
 * [`BindingSession`]**（单连接、稳定待批、读循环消费撤权）。
 *
 * 保留本类的原因：T06 的四类 wire 行为（未批准不交付 / 重放换 nonce /
 * 旧代际跟随 / 撤权停止交付）在这里以最小步伐逐条可断言，且与 BindingSession
 * 共用同一套帧类型、签名消息与存储形状；删掉会同时删掉这些保证。
 */
export class BindingClient {
  private readonly options: BindingClientOptions;
  private storage: BindingStorage;
  private currentState: BindingClientState = "idle";
  private lastSnapshot: {
    state: BindingClientState;
    bindingId: string | null;
    displayCode: string | null;
  } = { state: "idle", bindingId: null, displayCode: null };

  constructor(options: BindingClientOptions) {
    this.options = options;
    this.storage = options.storage;
  }

  /** 客户端当前绑定态。 */
  get state(): BindingClientState {
    return this.currentState;
  }

  /** 本地 UI 可渲染的快照（经 webServer 路由暴露；无秘密）。 */
  snapshot(): { state: BindingClientState; bindingId: string | null; displayCode: string | null } {
    return { ...this.lastSnapshot };
  }

  /** 显式重置身份（撤权后重新走 TOFU 的唯一路径；旧钥墓碑在 Rust 侧已拒）。 */
  async resetIdentity(): Promise<void> {
    const raw = await this.readIdentity();
    const fresh = await this.generateIdentity();
    await this.storage.write(
      JSON.stringify({
        ...fresh.raw,
        trustAnchorSpki: raw?.raw.trustAnchorSpki ?? null,
      } satisfies StoredIdentity)
    );
    this.currentState = "idle";
    this.lastSnapshot = { state: "idle", bindingId: null, displayCode: null };
  }

  /** 一次真实握手尝试：读 rendezvous → 连接 → 签名挑战 → 按服务端决策行动。 */
  async runOnce(): Promise<HandshakeOutcome> {
    const rv = await this.options.rendezvous.read();
    this.storage = this.options.storage.scope?.() ?? this.options.storage;
    if (rv === null || (this.storage.matchesRendezvous && !this.storage.matchesRendezvous(rv))) {
      return this.refuse("DSH_AUTH_PROTOCOL");
    }
    const stored = await this.readIdentity();
    if (this.storage.matchesRendezvous && !this.storage.matchesRendezvous(rv)) return this.refuse("DSH_AUTH_PROTOCOL");
    // 撤权态：不连接、不兑换、不交付（直到显式 resetIdentity）。
    if (stored?.raw.revoked) {
      this.currentState = "revoked";
      this.lastSnapshot = {
        state: "revoked",
        bindingId: stored.raw.revokedBindingId,
        displayCode: null,
      };
      return { state: "revoked", bindingId: stored.raw.revokedBindingId };
    }
    if (rv.bindingProtocol !== BINDING_PROTOCOL) {
      return this.refuse("DSH_AUTH_PROTOCOL");
    }
    // 信任锚：rendezvous 的 Rust 公钥与首缘钉住的不一致 → 拒（疑似假宿主）。
    if (stored?.raw.trustAnchorSpki && stored.raw.trustAnchorSpki !== rv.publicKeySpki) {
      return this.refuse("DSH_AUTH_SIGNATURE");
    }
    const homeBinding = resolveHomeBinding(this.options.homeBinding, rv);
    if (!homeBinding) return this.refuse("DSH_AUTH_PROTOCOL");
    const identity = stored ?? (await this.generateAndStore());
    let conn: IpcConnection;
    try {
      conn = await this.options.transport.connect(rv.address);
    } catch (e) {
      return this.refuse(`transport connect failed: ${String(e)}`);
    }
    try {
      if (this.storage.matchesRendezvous && !this.storage.matchesRendezvous(rv)) {
        return this.refuse("DSH_AUTH_PROTOCOL");
      }
      return await this.handshake(conn, rv, identity, homeBinding);
    } finally {
      // 拒绝路径关线；授权路径保留连接（受信 Peer 的 granted/revoked 推送通道）。
      if (this.currentState !== "bound") conn.close();
    }
  }

  /** 握手主体（连接已建立）。 */
  private async handshake(
    conn: IpcConnection,
    rv: Rendezvous,
    identity: Identity,
    homeBinding: HomeBinding
  ): Promise<HandshakeOutcome> {
    const now = this.options.clock();
    const nonce = b64url(randomBytes(32));
    const expiry = now + CHALLENGE_TTL_MS;
    const msg: BindingChallengeMessage = {
      bindingProtocol: BINDING_PROTOCOL,
      keyDigests: [sha256Hex(Buffer.from(rv.publicKeySpki, "base64")), sha256Hex(identity.spki)],
      nonce,
      hostId: rv.hostId,
      epoch: rv.epoch,
      expiry,
      homeBinding,
    };
    const signature = b64url(cryptoSign(null, signingBytes(msg), identity.privateKey));
    const hello: HelloFrame = {
      type: "hello",
      homeBinding,
      bindingProtocol: BINDING_PROTOCOL,
      publicKeySpki: identity.raw.publicKeySpki,
      executable: rv.processIdentity.executable,
      pid: rv.processIdentity.pid,
      startedAtMs: rv.processIdentity.startedAtMs,
      hostId: rv.hostId,
      epoch: rv.epoch,
      nonce,
      expiry,
      signature,
    };
    await conn.send(JSON.stringify(hello));
    const reply = this.parseServerFrame(await conn.recv());
    if (reply === null) return this.refuse("DSH_AUTH_PROTOCOL");

    if (reply.type === "error") return this.refuse(reply.error);
    // 首帧未经服务端签名认证，不能持久化撤权墓碑。
    if (reply.type !== "server_hello") return this.refuse("DSH_AUTH_PROTOCOL");
    const serverHello = reply;
    if (!matchesServerHello(serverHello, msg, rv, homeBinding)) {
      return this.refuse("DSH_AUTH_PROTOCOL");
    }
    const rustOk = cryptoVerify(
      null,
      signingBytes(msg),
      createPublicKey({
        key: Buffer.from(rv.publicKeySpki, "base64"),
        type: "spki",
        format: "der",
      }),
      Buffer.from(serverHello.signature, "base64")
    );
    if (!rustOk || !cryptoVerify(null, decisionBytes(msg, serverHello),
      createPublicKey({ key: Buffer.from(rv.publicKeySpki, "base64"), type: "spki", format: "der" }),
      Buffer.from(serverHello.decisionSignature, "base64"))) {
      return this.refuse("DSH_AUTH_SIGNATURE");
    }
    if (serverHello.state === "revoked") {
      await this.markRevoked(serverHello.bindingId);
      return { state: "revoked", bindingId: serverHello.bindingId };
    }
    if (serverHello.state === "pending_approval") {
      this.currentState = "pending";
      this.lastSnapshot = {
        state: "pending",
        bindingId: serverHello.bindingId,
        displayCode: serverHello.displayCode,
      };
      return {
        state: "pending_approval",
        bindingId: serverHello.bindingId ?? "",
        displayCode: serverHello.displayCode ?? "",
        expiresAtMs: pendingDeadlineOf(serverHello),
      };
    }
    // authorized（或批准推来的 granted）：兑换并交付 cookie（只经已授权本机 IPC）。
    const bindingId = serverHello.bindingId ?? "";
    const cookie = await this.options.exchange.exchange();
    const credential = await buildCredentialFrame(this.options, bindingId, cookie);
    await conn.send(JSON.stringify(credential));
    const ack = this.parseServerFrame(await conn.recv());
    if (!verifyEvent(ack, "credential_ack", bindingId, msg, serverHello.decisionSignature, rv)) {
      return this.refuse("credential delivery was not acknowledged");
    }
    // 首缘固定信任锚（此后 rendezvous 公钥漂移即拒）。
    if (!identity.raw.trustAnchorSpki) {
      identity.raw.trustAnchorSpki = rv.publicKeySpki;
      await this.storage.write(JSON.stringify(identity.raw));
    }
    this.currentState = "bound";
    this.lastSnapshot = { state: "bound", bindingId, displayCode: null };
    return { state: "authorized", bindingId, cookieDelivered: true };
  }

  // --------------------------------------------------------
  // 内部
  // --------------------------------------------------------

  private refuse(reason: string): HandshakeOutcome {
    if (this.currentState !== "bound") this.currentState = "idle";
    return { state: "refused", reason };
  }

  private async markRevoked(bindingId: string | null): Promise<void> {
    const stored = await this.readIdentity();
    if (stored) {
      stored.raw.revoked = true;
      stored.raw.revokedBindingId = bindingId;
      await this.storage.write(JSON.stringify(stored.raw));
    }
    this.currentState = "revoked";
    this.lastSnapshot = { state: "revoked", bindingId, displayCode: null };
  }

  private parseServerFrame(line: string): ServerFrame | null {
    return decodeServerFrame(line);
  }

  private async readIdentity(): Promise<Identity | null> {
    const raw = await this.storage.read();
    if (raw === null) return null;
    try {
      const parsed = JSON.parse(raw) as StoredIdentity;
      if (parsed.version !== 1 || typeof parsed.privateKeyPem !== "string") return null;
      return {
        privateKey: createPrivateKey(parsed.privateKeyPem),
        spki: Buffer.from(parsed.publicKeySpki, "base64"),
        raw: parsed,
      };
    } catch {
      return null;
    }
  }

  private async generateIdentity(): Promise<Identity> {
    const { publicKey, privateKey } = generateKeyPairSync("ed25519");
    const spkiDer = publicKey.export({ type: "spki", format: "der" }) as Buffer;
    const pem = privateKey.export({ type: "pkcs8", format: "pem" }) as string;
    const raw: StoredIdentity = {
      version: 1,
      privateKeyPem: pem,
      publicKeySpki: b64url(spkiDer),
      trustAnchorSpki: null,
      revoked: false,
      revokedBindingId: null,
    };
    return { privateKey, spki: spkiDer, raw };
  }

  private async generateAndStore(): Promise<Identity> {
    const identity = await this.generateIdentity();
    await this.storage.write(JSON.stringify(identity.raw));
    return identity;
  }
}

/**
 * 本地 UI（渲染层）可订阅的绑定态快照（宿主半区经 webServer 路由暴露；
 * 这里只声明形状）。
 */
export interface BindingStateSnapshot {
  state: BindingClientState;
  bindingId: string | null;
  displayCode: string | null;
}

// ============================================================
// 绑定会话（R04 稳定待批 + R05 单连接生命周期）
// ============================================================

/**
 * 定时器抽象（生产 = 宿主全局 `setTimeout`；测试注入可手动推进的记录型调度器，
 * 用于「零泄漏定时器」断言）。
 */
export interface BindingScheduler {
  setTimeout(run: () => void, ms: number): unknown;
  clearTimeout(handle: unknown): void;
}

/** 默认调度器（宿主/Node 全局定时器；不 unref —— 与宿主同生命周期）。 */
const defaultScheduler: BindingScheduler = {
  setTimeout: (run, ms) => globalThis.setTimeout(run, ms),
  clearTimeout: (handle) => globalThis.clearTimeout(handle as ReturnType<typeof setTimeout>),
};

/** 重连退避（毫秒）：从 1s 起翻倍，封顶 30s（掉线不刷屏、也不无限快连）。 */
const RECONNECT_BASE_MS = 1_000;
const RECONNECT_MAX_MS = 30_000;

/** 服务端错误码：本钥已被撤权（重连一律拒绝；Rust 侧同名字面量）。 */
const PEER_REVOKED_CODE = "DSH_PEER_REVOKED";
/** 服务端错误码：本钥尚未批准（**可恢复**；K5 分档后不再当永久撤权）。 */
const AUTH_NOT_APPROVED_CODE = "DSH_AUTH_NOT_APPROVED";
/** 服务端错误码：binding_id 不在待批记录里（待批过期/并发替换；同样可恢复）。 */
const AUTH_UNKNOWN_BINDING_CODE = "DSH_AUTH_UNKNOWN_BINDING";

interface EventProofContext {
  msg: BindingChallengeMessage;
  decisionSignature: string;
  rv: Rendezvous;
  bindingId: string;
}

export interface BindingSessionOptions {
  homeBinding?: HomeBinding | ((rv: Rendezvous) => HomeBinding | undefined);
  transport: IpcTransport;
  rendezvous: RendezvousReader;
  storage: BindingStorage;
  exchange: CookieExchange;
  /** 浏览器会话凭据兑换（P1-2a；可选）。缺省 = 控制台首页通道不可用。 */
  browserExchange?: () => Promise<string | null>;
  clock: () => number;
  /** 定时器来源（测试注入记录型调度器；缺省用全局定时器）。 */
  scheduler?: BindingScheduler;
}

/**
 * 绑定会话：**整个生命周期最多一条 IPC 连接**。
 *
 * 状态机（R04/R05 的核心；K5/M-2 后的错误码分档见下）：

 * ```
 * idle ──hello──► pending ──Granted──► bound ──Revoked/PEER_REVOKED──► revoked
 *   ▲               │                    │
 *   └──掉线退避重连──┴────────────────────┘（仅掉线重连，撤权不重连）
 * ```

 * 错误码分档（K5 修复，**这是本节最重要的一条**）：
 *
 * | 服务端回绝码 | 语义 | 插件动作 |
 * | --- | --- | --- |
 * | `DSH_PEER_REVOKED` | **永久**：本绑定已被用户撤权 | 进 `revoked`、写 identity.json 墓碑、停止兑换/交付/重连 |
 * | `DSH_AUTH_NOT_APPROVED` / 未知绑定 | **可恢复**：尚未批准、待批记录过期、并发替换 | 退回 `pending`/`idle` + 退避重连，**不写墓碑**、凭据交付路径保持可用 |
 *
 * 为什么必须分档：`gates` 与待批记录都可能在用户操作/竞争下变化，把「还没批准」
 * 当成「被永久撤销」会让插件自钉死（写墓碑后既不重连也没人调用 `resetIdentity()`），
 * 一次竞争就永久停摆。真正的永久撤权由服务端显式推的 `revoked` 帧或
 * `DSH_PEER_REVOKED` 表达。
 *
 * 三条纪律（对应审查发现）：
 *  1. **待批不换码**：待批期间连接被保留（不「握手回完就关」）；只有真掉线才
 *     退避重连，并依赖 Rust 侧「同一客户端公钥在挑战到期前幂等」拿回同一对
 *     binding_id/display_code。快照在掉线重连时**不清空**，核对码不翻动。
 *  2. **单连接**：任一时点最多一条连接；绑定后保留该连接并跑读循环消费
 *     granted/revoked 推送（旧实现绑定后无人读，撤权推送被丢弃，还每 3 秒
 *     重连一条把旧连接泄漏掉）。
 *  3. **撤权即停**：收到 `revoked` 帧、或服务端以 `DSH_PEER_REVOKED` 拒绝 →
 *     进入 revoked，停止兑换与交付、**不再重连**（不静默换绑；重新绑定必须显式
 *     resetIdentity 换新钥）。
 *
 * disposer：关连接 + 清掉所有它登记的定时器 + 停读循环；二次调用是 no-op。
 */
export class BindingSession {
  private readonly options: BindingSessionOptions;
  private storage: BindingStorage;
  private readonly scheduler: BindingScheduler;
  /** 待批到期定时器（P1-①；连接更换/收场/释放时清理）。 */
  private pendingTimer: unknown = null;
  private domainTimer: unknown = null;
  private connection: IpcConnection | null = null;
  private timer: unknown = null;
  /** 退避代数（每次成功握手清零；每次掉线 +1）。 */
  private backoffStep = 0;
  private disposed = false;
  /** 一轮「连接 → 握手 → 读循环」是否仍在推进（防止同一连接被两条路径处理）。 */
  private running = false;
  private lastSnapshot: BindingStateSnapshot = {
    state: "idle",
    bindingId: null,
    displayCode: null,
  };

  constructor(options: BindingSessionOptions) {
    this.options = options;
    this.storage = options.storage;
    this.scheduler = options.scheduler ?? defaultScheduler;
  }

  /** 当前绑定态（本地 UI 数据源）。 */
  get state(): BindingClientState {
    return this.lastSnapshot.state;
  }

  /** 本地 UI 可渲染的快照（无秘密）。 */
  snapshot(): BindingStateSnapshot {
    return { ...this.lastSnapshot };
  }

  /** 启动会话（幂等：已启动/已释放时不再启动第二条连接）。 */
  start(): void {
    if (this.disposed || this.running) return;
    void this.cycle();
  }

  /** 卸载：关连接、清定时器、停循环。二次调用是 no-op。 */
  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.clearTimer();
    this.clearPendingExpiry();
    this.clearDomainTimer();
    const conn = this.connection;
    this.connection = null;
    if (conn !== null) conn.close();
  }

  // --------------------------------------------------------
  // 内部：一轮连接生命周期
  // --------------------------------------------------------

  /**
   * 一次「连接 → 握手 → （待批/绑定）保持并读」。
   *
   * 返回后若仍需重试，由 [`scheduleReconnect`] 排下一轮；撤权路径不再排。
   */
  private async cycle(): Promise<void> {
    if (this.disposed) return;
    this.running = true;
    try {
      await this.runConnection();
    } catch {
      // 异常也必须释放已建立的连接并安排重试。
      this.clearPendingExpiry();
      if (this.connection) this.clearConnection(this.connection);
      this.scheduleReconnect();
    } finally {
      this.running = false;
    }
  }

  private async runConnection(): Promise<void> {
    const rv = await this.options.rendezvous.read();
    this.storage = this.options.storage.scope?.() ?? this.options.storage;
    if (rv === null) {
      // 兔维斯还没发布 rendezvous：不连接、保持 idle，稍后再看（不猜路径）。
      this.lastSnapshot = { state: "idle", bindingId: null, displayCode: null };
      this.scheduleReconnect();
      return;
    }
    if (rv.bindingProtocol !== BINDING_PROTOCOL) {
      this.scheduleReconnect();
      return;
    }

    const stored = await this.readIdentity();
    if (this.storage.matchesRendezvous && !this.storage.matchesRendezvous(rv)) {
      this.scheduleReconnect();
      return;
    }
    // 已撤权（本地墓碑）：不连接、不交付，直接呈现撤权态。
    if (stored?.raw.revoked) {
      this.lastSnapshot = {
        state: "revoked",
        bindingId: stored.raw.revokedBindingId,
        displayCode: null,
      };
      return;
    }
    // 信任锚：rendezvous 公钥与首缘钉住的不一致 → 疑似假宿主，不连接。
    if (stored?.raw.trustAnchorSpki && stored.raw.trustAnchorSpki !== rv.publicKeySpki) {
      this.scheduleReconnect();
      return;
    }

    const homeBinding = resolveHomeBinding(this.options.homeBinding, rv);
    if (!homeBinding) {
      this.scheduleReconnect();
      return;
    }
    const identity = stored ?? (await this.generateAndStore());
    let conn: IpcConnection;
    try {
      conn = await this.options.transport.connect(rv.address);
    } catch {
      this.scheduleReconnect();
      return;
    }
    if (this.disposed) {
      conn.close();
      return;
    }
    this.connection = conn;
    if (this.storage.matchesRendezvous && !this.storage.matchesRendezvous(rv)) {
      this.clearConnection(conn);
      this.scheduleReconnect();
      return;
    }

    const outcome = await this.handshake(conn, rv, identity, homeBinding);
    if (this.disposed) {
      this.clearConnection(conn);
      return;
    }

    if (outcome.kind === "bound" || outcome.kind === "pending") this.watchDomain(conn, rv);
    switch (outcome.kind) {
      case "bound":
        // 绑定成功：**保留这条连接并继续读**（撤权推送的唯一消费面）。
        // 读到尽头后的「关连接 + 退避重连」由 readLoop 尾部统一处理——这里**不得**
        // 再排一次（复审 Blocker-2：两处各排一次会让 backoffStep 双步进，首次重连
        // 从 1s 变成 2s）。
        await this.readLoop(conn, outcome.bindingId, identity, rv.publicKeySpki, outcome.proof);
        return;
      case "pending":
        // 待批：同样保留连接（Rust 批准时向这条连接推 Granted）。
        // P1-①（2026-10-30 复审 T08 前门槛）：挑战 60 秒到期后必须自动断开
        // 重握手——否则旧核对码一直展示、旧码不可批而插件永不重发 hello。
        this.armPendingExpiry(conn, outcome.expiresAtMs);
        await this.readLoop(conn, null, identity, rv.publicKeySpki, outcome.proof);
        this.clearPendingExpiry();
        // 待批连接结束（掉线或到期关线）：Rust 按钥幂等给回同码（有效期内）
        // 或新码（到期后），退避重连——同样由 readLoop 尾部排程（撤权在 readLoop
        // 内已提前收场返回，不会走到尾部的那次排程）。
        return;
      case "revoked":
        this.clearConnection(conn);
        return; // 撤权不再重连
      case "retry":
        this.clearConnection(conn);
        this.scheduleReconnect();
        return;
    }
  }

  private watchDomain(conn: IpcConnection, rv: Rendezvous): void {
    this.clearDomainTimer();
    if (!this.storage.matchesRendezvous) return;
    const check = (): void => {
      this.domainTimer = null;
      if (this.disposed || this.connection !== conn || conn.closed) return;
      if (!this.storage.matchesRendezvous?.(rv)) {
        conn.close();
        return;
      }
      this.domainTimer = this.scheduler.setTimeout(check, 3_000);
    };
    this.domainTimer = this.scheduler.setTimeout(check, 3_000);
  }

  private clearDomainTimer(): void {
    if (this.domainTimer !== null) this.scheduler.clearTimeout(this.domainTimer);
    this.domainTimer = null;
  }

  /**
   * 待批到期定时器（P1-①）：挑战 expiry 一到就本地关线——readLoop 随之结束、
   * 走重连重握手；Rust 侧同钥幂等在有效期内给回同码，过期给回新码并关闭旧线。
   *
   * 加固（复审 Blocker-1）：定时器**武装在 readLoop 之前**，而批准发生在 readLoop
   * 之内（快照翻 bound、连接保留）。除了在批准分支显式清除之外，回调本身也必须
   * 双条件自检——只有「仍是当前连接」且「仍处于待批」才允许关线，否则一次到点
   * 就会误杀一条活着的已绑定连接（无谓重握手 + 重新兑换）。
   */
  private armPendingExpiry(conn: IpcConnection, expiresAtMs: number): void {
    this.clearPendingExpiry();
    const delay = Math.max(0, expiresAtMs - this.options.clock());
    this.pendingTimer = this.scheduler.setTimeout(() => {
      this.pendingTimer = null;
      if (this.connection !== conn || this.lastSnapshot.state !== "pending") return;
      conn.close();
    }, delay);
  }

  private clearPendingExpiry(): void {
    if (this.pendingTimer !== null) {
      this.scheduler.clearTimeout(this.pendingTimer);
      this.pendingTimer = null;
    }
  }

  /**
   * 握手：发 hello → 验 Rust 签名 → 按决策行动。
   *
   * 只在 authorized 时兑换并交付 cookie；pending 期零凭据（纪律不变）。
   */
  private async handshake(
    conn: IpcConnection,
    rv: Rendezvous,
    identity: Identity,
    homeBinding: HomeBinding
  ): Promise<
    | { kind: "pending"; expiresAtMs: number; proof: EventProofContext }
    | { kind: "bound"; bindingId: string; proof: EventProofContext }
    | { kind: "revoked" }
    | { kind: "retry" }
  > {
    const now = this.options.clock();
    const nonce = b64url(randomBytes(32));
    const expiry = now + CHALLENGE_TTL_MS;
    const msg: BindingChallengeMessage = {
      bindingProtocol: BINDING_PROTOCOL,
      keyDigests: [sha256Hex(Buffer.from(rv.publicKeySpki, "base64")), sha256Hex(identity.spki)],
      nonce,
      hostId: rv.hostId,
      epoch: rv.epoch,
      expiry,
      homeBinding,
    };
    const signature = b64url(cryptoSign(null, signingBytes(msg), identity.privateKey));
    const hello: HelloFrame = {
      type: "hello",
      homeBinding,
      bindingProtocol: BINDING_PROTOCOL,
      publicKeySpki: identity.raw.publicKeySpki,
      executable: rv.processIdentity.executable,
      pid: rv.processIdentity.pid,
      startedAtMs: rv.processIdentity.startedAtMs,
      hostId: rv.hostId,
      epoch: rv.epoch,
      nonce,
      expiry,
      signature,
    };

    let reply: ServerFrame | null;
    try {
      await conn.send(JSON.stringify(hello));
      reply = this.parseServerFrame(await conn.recv());
    } catch {
      return { kind: "retry" };
    }
    if (reply === null) return { kind: "retry" };

    if (reply.type === "error") {
      // K5（M-2）分档：只有 `DSH_PEER_REVOKED` 是**永久**撤权（进 revoked + 落盘
      // 墓碑 + 停止重连）。`DSH_AUTH_NOT_APPROVED`（未批准 / 待批过期 / 并发替换）
      // 与未知绑定变体都是**可恢复**的：退回待批/退避重连，绝不写墓碑——把可恢复
      // 错误当永久撤权会让插件在竞争下自钉死（且 resetIdentity 没有生产入口）。
      if (reply.error === PEER_REVOKED_CODE) return { kind: "retry" };
      if (reply.error === AUTH_NOT_APPROVED_CODE || reply.error === AUTH_UNKNOWN_BINDING_CODE) {
        // 可恢复：保留/落定待批态（规格上「尚未批准」就是 pending），退避重连。
        if (this.lastSnapshot.state !== "pending") {
          this.lastSnapshot = { state: "pending", bindingId: null, displayCode: null };
        }
        return { kind: "retry" };
      }
      return { kind: "retry" };
    }
    // 未认证首帧不能写永久撤权墓碑。
    if (reply.type !== "server_hello") return { kind: "retry" };
    const serverHello = reply;
    if (!matchesServerHello(serverHello, msg, rv, homeBinding)) return { kind: "retry" };
    {
      const rustOk = cryptoVerify(
        null,
        signingBytes(msg),
        createPublicKey({
          type: "spki",
          format: "der",
          key: Buffer.from(rv.publicKeySpki, "base64"),
        }),
        Buffer.from(serverHello.signature, "base64")
      );
      if (!rustOk || !cryptoVerify(null, decisionBytes(msg, serverHello),
        createPublicKey({ type: "spki", format: "der", key: Buffer.from(rv.publicKeySpki, "base64") }),
        Buffer.from(serverHello.decisionSignature, "base64"))) return { kind: "retry" };
    }

    if (serverHello.state === "revoked") {
      await this.markRevoked(serverHello.bindingId);
      return { kind: "revoked" };
    }
    if (serverHello.state === "pending_approval") {
      // R04：待批快照只在**首次**落定时写；掉线重连回来的同一对 id/码保持原值，
      // 核对码在人工核对窗口里不翻动。到期重握后服务端给的是**新对**，id/码
      // 均为本次应答值（不回落旧快照）。
      const bindingId = serverHello.bindingId ?? null;
      const displayCode = serverHello.displayCode ?? null;
      this.lastSnapshot = {
        state: "pending",
        bindingId: bindingId ?? this.lastSnapshot.bindingId,
        displayCode: displayCode ?? this.lastSnapshot.displayCode,
      };
      this.backoffStep = 0;
      return { kind: "pending", expiresAtMs: pendingDeadlineOf(serverHello),
        proof: { msg, decisionSignature: serverHello.decisionSignature, rv, bindingId: serverHello.bindingId! } };
    }

    // authorized：兑换 + 交付（只经已授权本机 IPC 连接）。
    const bindingId = serverHello.bindingId ?? "";
    let cookie: string;
    try {
      cookie = await this.options.exchange.exchange();
    } catch {
      // 兑换失败 → 不交付半个凭据，按重试处理（下次重新握手再兑换）。
      return { kind: "retry" };
    }
    const credential = await buildCredentialFrame(this.options, bindingId, cookie);
    try {
      await conn.send(JSON.stringify(credential));
      const ack = this.parseServerFrame(await conn.recv());
      if (!verifyEvent(ack, "credential_ack", bindingId, msg, serverHello.decisionSignature, rv)) return { kind: "retry" };
    } catch {
      return { kind: "retry" };
    }
    // 首缘固定信任锚（此后 rendezvous 公钥漂移即拒）。
    if (!identity.raw.trustAnchorSpki) {
      identity.raw.trustAnchorSpki = rv.publicKeySpki;
      await this.storage.write(JSON.stringify(identity.raw));
    }
    this.lastSnapshot = { state: "bound", bindingId, displayCode: null };
    this.backoffStep = 0;
    return { kind: "bound", bindingId, proof: { msg, decisionSignature: serverHello.decisionSignature, rv, bindingId } };
  }

  /**
   * 读循环：消费服务端推送直到连接结束。
   *
   * 这是 R05 的核心——绑定成功后连接**必须有人读**，否则撤权推送被丢弃、
   * 旧连接只会被下一次重连泄漏掉。读到 EOF/错误时：
   *  - 已绑定 → 视为掉线，退避重连一次（凭据与连接生死绑定，必须重新握手）；
   *  - 待批 → 同样退避重连（Rust 幂等给回同一对 id/码）；
   *  - 已撤权 → 不重连。
   */
  private async readLoop(conn: IpcConnection, boundBindingId: string | null, identity: Identity, serverKey: string, proof: EventProofContext): Promise<void> {
    // 本条连接上是否已交付过凭据。**必须区分两种 granted 帧**：待批连接收到用户
    // 批准的那一帧是「放行信号」，而我们随后发 credential 得到的 ack **也是**
    // granted——若不区分，就会把 ack 当成再次放行，形成无限兑换循环（测试
    // `approval_on_the_pending_connection_binds_without_a_new_socket` 抓获过这条）。
    let delivered = boundBindingId !== null;
    for (;;) {
      if (this.disposed || conn.closed) break;
      let line: string;
      try {
        line = await conn.recv();
      } catch {
        break; // EOF / IO 错误
      }
      const frame = this.parseServerFrame(line);
      if (frame === null) continue;
      if (frame.type === "revoked") {
        if (!verifyEvent(frame, "revocation", proof.bindingId, proof.msg, proof.decisionSignature, proof.rv)) break;
        // 服务端撤权：转撤权态、关连接、**不再重连**。
        await this.markRevoked(frame.bindingId ?? boundBindingId);
        this.clearConnection(conn);
        return;
      }
      if (frame.type === "granted") {
        if (delivered) continue; // credential 的 ack，不是新的放行
        if (!verifyEvent(frame, "approval", proof.bindingId, proof.msg, proof.decisionSignature, proof.rv)) break;
        // 待批连接上收到批准：这条连接已是受信 Peer，无需新 socket，直接兑换交付。
        //
        // K6（M-4）：快照**只能**在交付真正成功后置 bound。Rust 侧的真值是「有没有
        // 收到 cookie」——兑换或投递失败时 Rust 手里什么都没有，卡面若先显 bound
        // 就是假象（旧实现先置 bound 再在 deliverCredential 里 `catch { return; }`）。
        delivered = true;
        const ok = await this.deliverCredential(conn, frame.bindingId, proof);
        if (ok) {
          if (!identity.raw.trustAnchorSpki) {
            identity.raw.trustAnchorSpki = serverKey;
            await this.storage.write(JSON.stringify(identity.raw));
          }
          // 复审 Blocker-1：批准成功即已离开待批态，到期定时器必须立刻失效——
          // 否则它会在原 expiry 到点时关掉这条**活着的已绑定连接**。
          this.clearPendingExpiry();
          this.lastSnapshot = { state: "bound", bindingId: frame.bindingId, displayCode: null };
          continue;
        }
        // 交付失败 = Rust 没拿到凭据。快照退回待批（保留原核对码，不翻动），
        // 并**丢掉这条连接**重走一次握手+兑换（下面的收尾统一退避重连）。
        this.lastSnapshot = {
          state: "pending",
          bindingId: this.lastSnapshot.bindingId ?? frame.bindingId,
          displayCode: this.lastSnapshot.displayCode,
        };
        break;
      }
      if (frame.type === "error") {
        // K5 分档（与握手路径同一口径）：永久撤权 vs 可恢复未批准。
        if (frame.error === PEER_REVOKED_CODE) break;
        // 可恢复（未批准/未知绑定）：退回待批，交给下面的「读到尽头 → 退避重连」。
        if (frame.error === AUTH_NOT_APPROVED_CODE || frame.error === AUTH_UNKNOWN_BINDING_CODE) {
          if (this.lastSnapshot.state !== "pending") {
            this.lastSnapshot = { state: "pending", bindingId: null, displayCode: null };
          }
        }
      }
    }
    // 读到尽头：连接已不可用 → 已撤权就不再重连，否则退避重连。
    this.clearConnection(conn);
    if (this.lastSnapshot.state !== "revoked") this.scheduleReconnect();
  }

  /**
   * 在已受信连接上兑换并交付凭据（待批连接收到 Granted 后的路径）。
   *
   * 返回是否**确实交付成功**（服务端回了 granted ack）。调用方（读循环）据此决定
   * 快照：只有 `true` 才置 bound——失败一律不得假显已绑定（K6/M-4，两端以 Rust
   * 侧「有没有 cookie」这个真值为准）。
   */
  private async deliverCredential(conn: IpcConnection, bindingId: string, proof: EventProofContext): Promise<boolean> {
    let cookie: string;
    try {
      cookie = await this.options.exchange.exchange();
    } catch {
      return false; // 兑换失败：不交付半个凭据
    }
    const credential = await buildCredentialFrame(this.options, bindingId, cookie);
    try {
      await conn.send(JSON.stringify(credential));
      // 交付必须被服务端确认：没有 ack 就等于 Rust 手里没有凭据。
      const ack = this.parseServerFrame(await conn.recv());
      return verifyEvent(ack, "credential_ack", bindingId, proof.msg, proof.decisionSignature, proof.rv);
    } catch {
      return false;
    }
  }

  // --------------------------------------------------------
  // 内部：定时器 / 连接 / 状态
  // --------------------------------------------------------

  /** 排下一轮重连（退避；撤权/已释放则不排）。 */
  private scheduleReconnect(): void {
    if (this.disposed || this.lastSnapshot.state === "revoked") return;
    this.clearTimer();
    const delay = Math.min(RECONNECT_BASE_MS * 2 ** this.backoffStep, RECONNECT_MAX_MS);
    this.backoffStep += 1;
    this.timer = this.scheduler.setTimeout(() => {
      this.timer = null;
      void this.cycle();
    }, delay);
  }

  private clearTimer(): void {
    if (this.timer === null) return;
    this.scheduler.clearTimeout(this.timer);
    this.timer = null;
  }

  /** 关掉并忘记这条连接（幂等；只有当前连接被忘记时不动别条）。 */
  private clearConnection(conn: IpcConnection): void {
    if (this.connection === conn) {
      this.clearDomainTimer();
      this.connection = null;
    }
    if (!conn.closed) conn.close();
  }

  private parseServerFrame(line: string): ServerFrame | null {
    return decodeServerFrame(line);
  }

  private async markRevoked(bindingId: string | null): Promise<void> {
    const stored = await this.readIdentity();
    if (stored) {
      stored.raw.revoked = true;
      stored.raw.revokedBindingId = bindingId;
      await this.storage.write(JSON.stringify(stored.raw));
    }
    this.lastSnapshot = { state: "revoked", bindingId, displayCode: null };
    this.clearTimer();
  }

  private async readIdentity(): Promise<Identity | null> {
    const raw = await this.storage.read();
    if (raw === null) return null;
    try {
      const parsed = JSON.parse(raw) as StoredIdentity;
      if (parsed.version !== 1 || typeof parsed.privateKeyPem !== "string") return null;
      return {
        privateKey: createPrivateKey(parsed.privateKeyPem),
        spki: Buffer.from(parsed.publicKeySpki, "base64"),
        raw: parsed,
      };
    } catch {
      return null;
    }
  }

  private async generateIdentity(): Promise<Identity> {
    const { publicKey, privateKey } = generateKeyPairSync("ed25519");
    const spkiDer = publicKey.export({ type: "spki", format: "der" }) as Buffer;
    const pem = privateKey.export({ type: "pkcs8", format: "pem" }) as string;
    const raw: StoredIdentity = {
      version: 1,
      privateKeyPem: pem,
      publicKeySpki: b64url(spkiDer),
      trustAnchorSpki: null,
      revoked: false,
      revokedBindingId: null,
    };
    return { privateKey, spki: spkiDer, raw };
  }

  private async generateAndStore(): Promise<Identity> {
    const identity = await this.generateIdentity();
    await this.storage.write(JSON.stringify(identity.raw));
    return identity;
  }
}
