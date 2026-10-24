// scripts/zcode-bridge-probe/relayAuth.ts — B1 客户端侧最小认证状态机（Tuvis 参照实现）。
//
// 归属：工作包 B 探针工具目录（docs/superpowers/plans/2026-10-24-zcode-official-bridge-validation.md
// 「固定产物与接口」），**不在产品运行链上**：不被 src/ 引用、不挂应用启动/扫描。
// wire 形状全部钉在固定官方源码 zai-org/ZCode @ 3c5b227649371507ddc4eb46ac5133afe6db1cab
// 的客户端侧实现（官方云 relay 服务端不在该仓库），每条注释标行号；决策文件
// docs/superpowers/decisions/2026-10-24-zcode-official-bridge-contract.md B1 节为文字版。
//
// 探针红线（工作包 B 全局约束）：
//   - 单连接：本模块没有任何自动重连/退避计时器——KICKED/拒绝/配对窗口超时皆终态停止；
//     唯一的计时器 = waiting 配对窗口超时（官方同款 30s，见下），属终态判定、不是重连；
//   - token 一次性：只在每次 connect 前现取，用后即弃，不缓存不落盘；
//   - 未 paired 不发送任何 data 帧，不排队不重发；
//   - 本模块不持有任何真实凭据；调用方传入什么就用什么，日志/错误不携带凭据正文。
//
// 官方行为对照（行号 = 固定 SHA 文件）：
//   POST /api/v1/remote/token，Authorization: Bearer <账号JWT>，body {role:"terminal"}
//     → packages/shared/src/web-remote-relay/contract.ts:96-101（路径表）
//     → packages/web/src/web-remote-control/adapters/webRemoteControlV2Token.ts:13-22
//   响应 {code:0,msg,data:{token,expires_in}}，token 22 字符 base64URL（末字符∈[AQgw]）
//     → contract.ts:119-133
//   401 → auth-required（交上层回登录，不重试）→ webRemoteControlV2Token.ts:23
//   WS /api/v1/remote/ws2?token=<一次性token>（+可选 mid）
//     → contract.ts:102；web/.../webRemoteControlTransport.ts:106-110
//   open 后 auth_init{device_sid,meta,client_ts} → auth_ack{device_sid,pair_status}
//     → contract.ts:19-24,53-59；web/.../webRemoteControlTransport.ts:119-143,317-324
//   pair_status ∈ waiting|matched（contract.ts:53-59）：waiting 是合法配对前态——官方以
//   pair_status_query 心跳维持，30s 未 matched → desktop-disconnected 终态
//     → web/.../webRemoteControlTransport.ts:460-473
//   本实现取同一 30s 窗口（OFFICIAL_PAIR_WINDOW_MS，测试可注入短窗口）：窗口内保活被动等
//   matched（探针从简不发心跳，真连批次按需补）；到时 result="pair-timeout"，与显式拒绝
//   （rejected）分账，绝不静默并案
//   data 帧仅在 paired 态收发；未 paired 发送在客户端即被门禁挡下
//     → contract.ts:36-44,66-70；web/.../webRemoteControlTransport.ts:216-222,329-331
//   error{code}：AUTH_FAILED/WRONG_PARAM → 终态失败；KICKED → 官方允许一次 2-5 秒重连、
//   60 秒窗口内第二次 KICKED 即终态（session-conflict）；本参照实现取更保守口径：
//   KICKED 直接终态停止（探针不得与官方页面互踢）
//     → contract.ts:18,71-76；domain/retry.ts:26-30；web/.../webRemoteControlTransport.ts:389-394

/** relay HTTP token 端点路径（contract.ts:100）。 */
export const RELAY_TOKEN_PATH = "/api/v1/remote/token";
/** relay WebSocket 路径（contract.ts:102）；完整 URL 由调用方拼 origin。 */
export const RELAY_WS_PATH = "/api/v1/remote/ws2";
/**
 * 官方客户端侧 token 形状 schema（contract.ts:121）：16 字节无填充 Base64URL 的 22 字符，
 * 末字符只能取高两位集合 [AQgw]。只做形状闸，不做语义判定。
 */
export const OFFICIAL_TOKEN_SHAPE = /^[A-Za-z0-9_-]{21}[AQgw]$/;

/**
 * 官方配对窗口：pair_status=waiting 后 30s 未 matched 即 desktop-disconnected 终态
 * （web/.../webRemoteControlTransport.ts:460-473）。
 */
export const OFFICIAL_PAIR_WINDOW_MS = 30_000;

export type TerminalTokenResult =
  | { kind: "ok"; token: string; expiresInSeconds: number }
  | { kind: "auth-required" }
  | { kind: "relay-unavailable" };

/**
 * 以账号 JWT 申请 role=terminal 的一次性 relay token。
 * 401 → auth-required（对应官方：401 交给入口恢复登录，绝不缓存重试）；
 * 其余非 0 code / 非法形状 / 网络故障 → relay-unavailable。错误不携带服务端正文。
 */
export async function requestTerminalToken(options: {
  relayOrigin: string;
  accountJwt: string;
  signal?: AbortSignal;
  fetchImpl?: typeof fetch;
}): Promise<TerminalTokenResult> {
  const doFetch = options.fetchImpl ?? fetch;
  let response: Response;
  try {
    response = await doFetch(new URL(RELAY_TOKEN_PATH, options.relayOrigin), {
      method: "POST",
      headers: {
        Authorization: `Bearer ${options.accountJwt}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ role: "terminal" }),
      signal: options.signal,
      redirect: "error",
    });
  } catch {
    return { kind: "relay-unavailable" };
  }
  if (response.status === 401) {
    return { kind: "auth-required" };
  }
  if (!response.ok) {
    return { kind: "relay-unavailable" };
  }
  let value: unknown;
  try {
    value = await response.json();
  } catch {
    return { kind: "relay-unavailable" };
  }
  const envelope = value as {
    code?: unknown;
    data?: { token?: unknown; expires_in?: unknown } | null;
  };
  if (
    envelope?.code !== 0 ||
    typeof envelope.data?.token !== "string" ||
    !OFFICIAL_TOKEN_SHAPE.test(envelope.data.token) ||
    typeof envelope.data.expires_in !== "number" ||
    !Number.isFinite(envelope.data.expires_in) ||
    envelope.data.expires_in <= 0
  ) {
    return { kind: "relay-unavailable" };
  }
  return { kind: "ok", token: envelope.data.token, expiresInSeconds: envelope.data.expires_in };
}

export type RelayConnectionState =
  | "connecting"
  | "authenticating"
  /** 合法配对前态（contract.ts:53-59）：保活等待 matched，非终态。 */
  | "waiting"
  | "paired"
  | "rejected"
  | "kicked"
  /** 配对窗口超时（官方 desktop-disconnected 同因终态，web/.../transport.ts:460-473）。 */
  | "pair-timeout"
  | "closed";

/**
 * 连接结果。`pair-timeout` = waiting 窗口内未 matched（官方 desktop-disconnected 同因终态，
 * web/.../webRemoteControlTransport.ts:460-473）——与显式被拒（rejected）分账，不得混同。
 */
export type RelayConnectionResult = "paired" | "rejected" | "kicked" | "pair-timeout" | "closed";

export type RelayTerminalConnectionOptions = {
  /** 完整 relay WS URL（含路径），如 ws(s)://host/api/v1/remote/ws2。 */
  relayWsUrl: string;
  /** 一次性 relay token（role=terminal）；由调用方在每次连接前现取。 */
  token: string;
  /** 目标桌面端 device_sid（来自官方链接的 sid 参数）。 */
  deviceSid: string;
  /**
   * waiting→matched 的配对窗口上限（ms）。官方 30s
   * （web/.../webRemoteControlTransport.ts:460-473）；默认同官方，测试可注入短窗口。
   */
  pairWindowMs?: number;
  /** 供测试注入 WebSocket 实现；默认 Node ≥22 全局 WebSocket。 */
  webSocketImpl?: typeof WebSocket;
};

/**
 * 单发终端连接：connect() 一次，任何失败形态都以终态收尾，**没有重连路径**。
 * 与官方终端传输（web/.../webRemoteControlTransport.ts）的差异即探针红线：
 * 官方对 KICKED 有一次 2-5 秒重连、对断线有退避重连；探针一律停（差异说明见文件头）。
 * waiting 配对窗口为官方同款 30s（pairWindowMs 可注入覆盖）。
 */
export class RelayTerminalConnection {
  private state: RelayConnectionState = "connecting";
  private socket?: WebSocket;
  private pairWindowTimer?: ReturnType<typeof setTimeout>;
  private readonly webSocketImpl: typeof WebSocket;

  constructor(private readonly options: RelayTerminalConnectionOptions) {
    this.webSocketImpl = options.webSocketImpl ?? WebSocket;
  }

  get currentState(): RelayConnectionState {
    return this.state;
  }

  /**
   * 建立连接并完成 auth_init→auth_ack；resolve 时机 = 终态（paired/rejected/kicked/
   * pair-timeout）或对端断开。waiting 是 auth_ack 的**非终态前态**：连接保持打开，
   * 直到 matched（paired）/窗口超时（pair-timeout）/对端断开/显式 error 帧。
   */
  connect(): Promise<RelayConnectionResult> {
    return new Promise<RelayConnectionResult>((resolve) => {
      let settled = false;
      const finish = (result: RelayConnectionResult) => {
        if (settled) return;
        settled = true;
        resolve(result);
      };
      const url = new URL(this.options.relayWsUrl);
      url.searchParams.set("token", this.options.token);
      const ws = new this.webSocketImpl(url.toString());
      this.socket = ws;
      ws.addEventListener("open", () => {
        if (this.state !== "connecting") return;
        this.state = "authenticating";
        // auth_init 形状钉 contract.ts:19-24；meta 参照官方终端的 {platform,version,name}
        // 形状（web/.../webRemoteControlTransport.ts:133-142），取值为本探针自报身份。
        this.sendJson({
          type: "auth_init",
          device_sid: this.options.deviceSid,
          meta: { platform: "tuvis-probe", name: "tuvis-zcode-bridge-probe" },
          client_ts: Math.floor(Date.now() / 1000),
        });
      });
      ws.addEventListener("message", (ev: WebSocketEventMap["message"]) => {
        let msg: {
          type?: unknown;
          device_sid?: unknown;
          pair_status?: unknown;
          code?: unknown;
          payload?: unknown;
        };
        try {
          msg = JSON.parse(String(ev.data));
        } catch {
          return;
        }
        if (msg.type === "auth_ack") {
          // device_sid 回显校验（web/.../webRemoteControlTransport.ts:318）；
          // waiting 态下后续 auth_ack（官方 pair_status_query 心跳的回应）同进本分支。
          if (msg.device_sid !== this.options.deviceSid) return;
          if (this.state !== "authenticating" && this.state !== "waiting") return;
          if (msg.pair_status === "matched") {
            this.clearPairWindow();
            this.state = "paired";
            finish("paired");
            return;
          }
          if (msg.pair_status === "waiting") {
            // waiting = 合法配对前态（contract.ts:53-59；web/.../transport.ts:460-473）：
            // 保持连接继续等 matched，绝不在此终态化；窗口超时由 startPairWindow 判定。
            if (this.state === "waiting") return; // 重复 waiting ack 不重置窗口
            this.state = "waiting";
            this.startPairWindow(finish);
            return;
          }
          // 官方 union（waiting|matched）之外的 pair_status：终态拒绝。
          this.clearPairWindow();
          this.state = "rejected";
          finish("rejected");
          this.socket?.close();
          return;
        }
        if (msg.type === "error") {
          this.clearPairWindow();
          // KICKED → kicked 终态——**更保守口径**（官方允许 2-5s 后重连一次，探针一律
          // 不重试，见文件头）；其余（AUTH_FAILED/WRONG_PARAM/INTERNAL/DEVICE_OFFLINE）→
          // rejected 终态。探针不区分可恢复性：一律停（红线）。
          this.state = msg.code === "KICKED" ? "kicked" : "rejected";
          finish(this.state);
          this.socket?.close();
          return;
        }
        if (msg.type === "data") {
          // 数据面交给 onData；未 paired 时服务端帧直接忽略（客户端不消费）。
          if (this.state === "paired") this.onData?.(msg.payload);
        }
      });
      ws.addEventListener("close", () => {
        this.clearPairWindow();
        if (this.state === "authenticating" || this.state === "connecting") {
          // 握手未完成即断：视为被拒（不重试）。
          this.state = "rejected";
          finish("rejected");
          return;
        }
        // waiting 期对端断开：未 matched、也非显式拒绝/超时 → 按连接关闭收尾（非 paired）。
        this.state =
          this.state === "paired" ||
          this.state === "rejected" ||
          this.state === "kicked" ||
          this.state === "pair-timeout"
            ? this.state
            : "closed";
        finish(this.mapStateToResult());
      });
      ws.addEventListener("error", () => {
        // 错误细节交 close 分支终态化；此处不落任何含 token URL 的日志（官方同款纪律，
        // desktop/.../webRemoteControlTransport.ts:155-158）。
      });
    });
  }

  /**
   * 发送 data 帧。客户端门禁（web/.../webRemoteControlTransport.ts:216-222）：
   * 未 paired 或 socket 不在 OPEN 态 → 不发送、不排队、不重发，返回 false。
   */
  sendData(payload: unknown): boolean {
    if (this.state !== "paired" || this.socket?.readyState !== WebSocket.OPEN) {
      return false;
    }
    try {
      this.socket.send(
        JSON.stringify({ type: "data", payload, client_ts: Math.floor(Date.now() / 1000) })
      );
      return true;
    } catch {
      return false;
    }
  }

  close(): void {
    this.clearPairWindow();
    if (this.state !== "closed") {
      this.state = "closed";
    }
    try {
      this.socket?.close();
    } catch {
      // 忽略重复关闭
    }
  }

  onData?: (payload: unknown) => void;

  /**
   * waiting 配对窗口：官方 30s 未 matched 即 desktop-disconnected 终态
   * （web/.../webRemoteControlTransport.ts:460-473）。这是终态判定计时器，
   * **不是**重连/退避计时器（探针无重连路径，见文件头红线）。
   */
  private startPairWindow(finish: (result: RelayConnectionResult) => void): void {
    this.clearPairWindow();
    this.pairWindowTimer = setTimeout(() => {
      this.pairWindowTimer = undefined;
      if (this.state !== "waiting") return;
      this.state = "pair-timeout";
      finish("pair-timeout");
      this.socket?.close();
    }, this.options.pairWindowMs ?? OFFICIAL_PAIR_WINDOW_MS);
  }

  private clearPairWindow(): void {
    if (this.pairWindowTimer !== undefined) {
      clearTimeout(this.pairWindowTimer);
      this.pairWindowTimer = undefined;
    }
  }

  private mapStateToResult(): RelayConnectionResult {
    if (this.state === "paired") return "paired";
    if (this.state === "kicked") return "kicked";
    if (this.state === "rejected") return "rejected";
    if (this.state === "pair-timeout") return "pair-timeout";
    return "closed";
  }

  private sendJson(value: unknown): void {
    try {
      this.socket?.send(JSON.stringify(value));
    } catch {
      // 发送失败交由 close/error 分支终态化
    }
  }
}
