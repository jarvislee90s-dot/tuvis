// scripts/zcode-bridge-probe/oauthFlow.ts — B1 probe-oauth 步骤的可测客户端逻辑。
//
// 归属：工作包 B 探针工具目录，不在产品运行链上。本轮探测的问题 = **官方授权服务器
// 是否接受 Tuvis 控制的本地回环 redirect_uri**（决策文件 §6.3：仓库无法证明/证伪，
// 只能由用户在场浏览器探测定案）。本文件提供三块纯逻辑/本地组件：
//
//   1. buildZaiAuthorizeUrl —— ZAI 通道授权 URL（官方参数形状）：
//      `{chat.z.ai}/api/oauth/authorize?redirect_uri&response_type=code&client_id&state`
//      → packages/web/src/auth/zaiWebOAuthProvider.ts:184-191 @ 3c5b2276；
//        webZaiOAuthConfig.ts:30（默认 origin https://chat.z.ai）、57-58（公开 client_id）
//      BigModel 通道变体 `{bigmodel}/login?redirect&appId=zcode&state` 存在
//      （zaiWebOAuthProvider.ts:175-181；webZaiOAuthConfig.ts:59-61）——本轮探针只走
//      默认 ZAI 通道，BigModel 变体留待需要时另行取证，不猜测其行为。
//   2. LoopbackCallbackServer —— 127.0.0.1 随机空闲端口的本地回调监听：
//      官方回调参数三元组 state + code|authCode|error（zaiWebOAuthProvider.ts:194-209；
//      entry.ts:17-26）。state 精确匹配才消费；不匹配/缺 state 一律忽略并继续等待
//      （防重放/他人回调）；响应体为静态文案，不回显任何参数。
//   3. exchangeCodeForToken —— `POST {zcodeOrigin}/api/v1/oauth/token`，body
//      `{provider:"zai", code, redirect_uri, state}`（官方 provider 常量 "zai"：
//      packages/shared/src/oauth.ts:12；body 形状 zaiWebOAuthProvider.ts:211-239，
//      tokenUrl 相对路径 webZaiOAuthConfig.ts:56 → 与网页同源，即 zcodeOrigin）。
//      官方网页从浏览器同源 fetch；探针从 Node 直连同一端点，形状一致（无 CORS 面）。
//
// 语法纪律：可被 `node`（type-stripping）直接执行——仅可擦除 TS 语法；相对导入带 .ts。

import * as crypto from "node:crypto";
import * as http from "node:http";
import type { AddressInfo } from "node:net";

/**
 * 官方网页第一方公开 OAuth client（webZaiOAuthConfig.ts:57-58 源码注释明示
 * 「client_id 会出现在授权 URL 中，属于公开配置」；zcodeEndpoint.ts:12 同值常量）。
 * 这是公开源码配置，不是凭据。
 */
export const OFFICIAL_OAUTH_CLIENT_ID = "client_P8X5CMWmlaRO9gyO-KSqtg";
/** ZAI 通道授权入口 origin（webZaiOAuthConfig.ts:30；zcodeEndpoint.ts:8）。 */
export const OFFICIAL_AUTHORIZE_ORIGIN = "https://chat.z.ai";
/** ZAI 通道授权路径（webZaiOAuthConfig.ts:30——注意是 /api/oauth 非旧 /auth/oauth）。 */
export const OFFICIAL_AUTHORIZE_PATH = "/api/oauth/authorize";
/** ZCode 端点默认 origin（zcodeEndpoint.ts:3）——exchange 与 relay 的派生基座。 */
export const OFFICIAL_ZCODE_ENDPOINT_ORIGIN = "https://zcode.z.ai";
/** OAuth token 交换路径（webZaiOAuthConfig.ts:56）。 */
export const OFFICIAL_OAUTH_TOKEN_PATH = "/api/v1/oauth/token";
/** 交换 body 的 provider 缺省值（oauth.ts:12；zaiWebOAuthProvider.ts:217,222-223）。 */
export const OAUTH_PROVIDER_ZAI = "zai";

/** 官方 ZAI 通道授权 URL（参数经 URLSearchParams 编码，与官方 buildAuthorizeUrl 同形）。 */
export function buildZaiAuthorizeUrl(params: { redirectUri: string; state: string }): string {
  const query = new URLSearchParams({
    redirect_uri: params.redirectUri,
    response_type: "code",
    client_id: OFFICIAL_OAUTH_CLIENT_ID,
    state: params.state,
  });
  return `${OFFICIAL_AUTHORIZE_ORIGIN}${OFFICIAL_AUTHORIZE_PATH}?${query.toString()}`;
}

// ---------------------------------------------------------------------------
// 本地回环回调监听
// ---------------------------------------------------------------------------

export type LoopbackCallback =
  | { kind: "code"; code: string; state: string }
  | {
      kind: "authorize-error";
      error: string;
      errorDescription: string | null;
      state: string | null;
    }
  | { kind: "timeout" };

/** 回调响应的静态文案（绝不回显 query 参数）。 */
const CALLBACK_BODY =
  "<!doctype html><meta charset=utf-8><title>Tuvis B1 probe</title>" +
  "<p>Tuvis B1 probe received the callback. You can close this tab and return to the terminal.</p>";

/**
 * 127.0.0.1 随机空闲端口上的单用途回调监听器。
 * - 只绑回环地址；生命周期 = 一次探针运行；
 * - state 不匹配 / 缺 state 的请求：400 + 忽略计数（ignoredRequests），**不消费**、继续等待；
 * - 匹配 state 的首个 code|authCode|error 回调消费等待者；后续请求一律忽略；
 * - waitCallback 有界（timeoutMs 到 → {kind:"timeout"}）；close() 时未决等待同样以
 *   timeout 收尾（有界保证）。
 */
export class LoopbackCallbackServer {
  private readonly server: http.Server;
  private portValue = 0;
  private consumed = false;
  private pending?: {
    expectedState: string;
    resolve: (outcome: LoopbackCallback) => void;
  };
  private ignored = 0;

  private constructor() {
    this.server = http.createServer((req, res) => this.handle(req, res));
  }

  static start(): Promise<LoopbackCallbackServer> {
    const instance = new LoopbackCallbackServer();
    return new Promise((resolve, reject) => {
      const onError = (error: Error) => {
        cleanup();
        reject(error);
      };
      const onListening = () => {
        cleanup();
        instance.portValue = (instance.server.address() as AddressInfo).port;
        resolve(instance);
      };
      const cleanup = () => {
        instance.server.off("error", onError);
        instance.server.off("listening", onListening);
      };
      instance.server.once("error", onError);
      instance.server.once("listening", onListening);
      instance.server.listen(0, "127.0.0.1");
    });
  }

  get port(): number {
    return this.portValue;
  }

  /** 本轮回调 redirect_uri（探测目标：官方授权服务器是否接受回环地址）。 */
  get redirectUri(): string {
    return `http://127.0.0.1:${this.portValue}/callback`;
  }

  /** 被 state 闸忽略的请求数（观测用，不携带任何参数值）。 */
  get ignoredRequests(): number {
    return this.ignored;
  }

  /** 有界等待官方授权回调。同一时刻只允许一个等待者；到时以 timeout 收尾。 */
  waitCallback(expectedState: string, timeoutMs: number): Promise<LoopbackCallback> {
    if (this.pending) {
      return Promise.resolve({ kind: "timeout" });
    }
    return new Promise<LoopbackCallback>((resolve) => {
      const entry: { expectedState: string; resolve: (outcome: LoopbackCallback) => void } = {
        expectedState,
        resolve: (outcome) => {
          clearTimeout(timer);
          resolve(outcome);
        },
      };
      const timer = setTimeout(
        () => {
          if (this.pending === entry) this.pending = undefined;
          resolve({ kind: "timeout" });
        },
        Math.max(1, timeoutMs)
      );
      this.pending = entry;
    });
  }

  async close(): Promise<void> {
    const waiting = this.pending;
    this.pending = undefined;
    waiting?.resolve({ kind: "timeout" });
    if (!this.server.listening) return;
    await new Promise<void>((resolve) => this.server.close(() => resolve()));
  }

  private handle(req: http.IncomingMessage, res: http.ServerResponse): void {
    const url = new URL(req.url ?? "/", "http://127.0.0.1");
    if (this.consumed || !this.pending) {
      this.ignored += 1;
      res.writeHead(410, { "Content-Type": "text/html; charset=utf-8" });
      res.end(CALLBACK_BODY);
      return;
    }
    const waiting = this.pending;
    const state = url.searchParams.get("state")?.trim();
    const code = url.searchParams.get("code")?.trim() || url.searchParams.get("authCode")?.trim();
    const error = url.searchParams.get("error")?.trim();
    // state 精确匹配（官方回调判定含 state：entry.ts:17-26；parseCallbackParams
    // 缺 state 即抛错：zaiWebOAuthProvider.ts:196-199）。不匹配/缺 state → 忽略不消费。
    if (!state || state !== waiting.expectedState) {
      this.ignored += 1;
      res.writeHead(400, { "Content-Type": "text/html; charset=utf-8" });
      res.end(CALLBACK_BODY);
      return;
    }
    this.consumed = true;
    this.pending = undefined;
    if (error) {
      waiting.resolve({
        kind: "authorize-error",
        error,
        errorDescription: url.searchParams.get("error_description")?.trim() || null,
        state,
      });
    } else if (code) {
      waiting.resolve({ kind: "code", code, state });
    } else {
      // state 匹配但既无 code 也无 error：按忽略处理（不消费，继续等待）。
      this.consumed = false;
      this.pending = waiting;
      this.ignored += 1;
    }
    res.writeHead(200, { "Content-Type": "text/html; charset=utf-8" });
    res.end(CALLBACK_BODY);
  }
}

// ---------------------------------------------------------------------------
// code 换 token（账号 JWT）
// ---------------------------------------------------------------------------

export type OAuthExchangeResult =
  | {
      kind: "ok";
      /** ZCode 账号 JWT（data.token，zaiWebOAuthProvider.ts:121-124）。只在内存。 */
      jwt: string;
      expiresInSeconds: number | null;
      hasUserInfo: boolean;
    }
  | {
      kind: "rejected";
      reason: "http-status" | "server-code" | "bad-json" | "missing-token";
      httpStatus?: number;
      serverCode?: number | null;
      /** 服务端 msg 原文仅经调用方脱敏后入证据；本模块不打印。 */
      serverMsg: string | null;
    }
  | { kind: "unreachable" };

/**
 * 以授权码换取 ZCode 账号 JWT（探针只消费 data.token；官方还要求 data.<provider>.
 * access_token，探针不使用 upstream access_token，故不作为成功条件——
 * data.token 即 B1 所需账号凭据，zaiWebOAuthProvider.ts:121-124,151-156）。
 * 任何失败都返回结构化结果而非抛错；错误不携带凭据。
 */
export async function exchangeCodeForToken(options: {
  zcodeOrigin: string;
  code: string;
  redirectUri: string;
  state: string;
  timeoutMs?: number;
  fetchImpl?: typeof fetch;
}): Promise<OAuthExchangeResult> {
  const doFetch = options.fetchImpl ?? fetch;
  let response: Response;
  try {
    response = await doFetch(new URL(OFFICIAL_OAUTH_TOKEN_PATH, options.zcodeOrigin), {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        provider: OAUTH_PROVIDER_ZAI,
        code: options.code,
        redirect_uri: options.redirectUri,
        state: options.state,
      }),
      redirect: "error",
      signal: AbortSignal.timeout(options.timeoutMs ?? 30_000),
    });
  } catch {
    return { kind: "unreachable" };
  }
  if (!response.ok) {
    return {
      kind: "rejected",
      reason: "http-status",
      httpStatus: response.status,
      serverMsg: null,
    };
  }
  let payload: {
    code?: unknown;
    msg?: unknown;
    data?: { token?: unknown; expires_in?: unknown; user?: unknown } | null;
  };
  try {
    payload = await response.json();
  } catch {
    return { kind: "rejected", reason: "bad-json", serverMsg: null };
  }
  const serverMsg = typeof payload.msg === "string" ? payload.msg : null;
  if (payload.code !== 0) {
    return {
      kind: "rejected",
      reason: "server-code",
      serverCode: typeof payload.code === "number" ? payload.code : null,
      serverMsg,
    };
  }
  const jwt = typeof payload.data?.token === "string" ? payload.data.token.trim() : "";
  if (!jwt) {
    return { kind: "rejected", reason: "missing-token", serverMsg };
  }
  const expiresIn = payload.data?.expires_in;
  return {
    kind: "ok",
    jwt,
    expiresInSeconds:
      typeof expiresIn === "number" && Number.isFinite(expiresIn) ? expiresIn : null,
    hasUserInfo: payload.data?.user != null,
  };
}

/** 生成单次 OAuth state（16 字节随机 → base64url 22 字符）。 */
export function generateOAuthState(): string {
  return crypto.randomBytes(16).toString("base64url");
}
