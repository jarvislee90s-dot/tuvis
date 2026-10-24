// scripts/zcode-bridge-probe/relayAuth.ts — B1 客户端侧最小认证状态机（Tuvis 参照实现）。
//
// 归属：工作包 B 探针工具目录（docs/superpowers/plans/2026-10-24-zcode-official-bridge-validation.md
// 「固定产物与接口」），**不在产品运行链上**：不被 src/ 引用、不挂应用启动/扫描。
// wire 形状全部钉在固定官方源码 zai-org/ZCode @ 3c5b227649371507ddc4eb46ac5133afe6db1cab
// 的客户端侧实现（官方云 relay 服务端不在该仓库），每条注释标行号；决策文件
// docs/superpowers/decisions/2026-10-24-zcode-official-bridge-contract.md B1 节为文字版。
//
// 探针红线（工作包 B 全局约束）：
//   - 单连接：本模块没有任何自动重连/退避计时器——KICKED 或拒绝即终态停止；
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
  "connecting" | "authenticating" | "paired" | "rejected" | "kicked" | "closed";

export type RelayConnectionResult = "paired" | "rejected" | "kicked" | "closed";

export type RelayTerminalConnectionOptions = {
  /** 完整 relay WS URL（含路径），如 ws(s)://host/api/v1/remote/ws2。 */
  relayWsUrl: string;
  /** 一次性 relay token（role=terminal）；由调用方在每次连接前现取。 */
  token: string;
  /** 目标桌面端 device_sid（来自官方链接的 sid 参数）。 */
  deviceSid: string;
  /** 供测试注入 WebSocket 实现；默认 Node ≥22 全局 WebSocket。 */
  webSocketImpl?: typeof WebSocket;
};

/**
 * 单发终端连接：connect() 一次，任何失败形态都以终态收尾，**没有重连路径**。
 * 与官方终端传输（web/.../webRemoteControlTransport.ts）的差异即探针红线：
 * 官方对 KICKED 有一次 2-5 秒重连、对断线有退避重连；探针一律停。
 */
export class RelayTerminalConnection {
  private state: RelayConnectionState = "connecting";
  private socket?: WebSocket;
  private readonly webSocketImpl: typeof WebSocket;

  constructor(private readonly options: RelayTerminalConnectionOptions) {
    this.webSocketImpl = options.webSocketImpl ?? WebSocket;
  }

  get currentState(): RelayConnectionState {
    return this.state;
  }

  /** 建立连接并完成 auth_init→auth_ack；resolve 后连接保持打开（数据面可用）。 */
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
          // device_sid 回显校验（web/.../webRemoteControlTransport.ts:318）
          if (msg.device_sid !== this.options.deviceSid || this.state !== "authenticating") return;
          this.state = msg.pair_status === "matched" ? "paired" : "rejected";
          finish(this.state === "paired" ? "paired" : "rejected");
          return;
        }
        if (msg.type === "error") {
          // KICKED → kicked 终态；其余（AUTH_FAILED/WRONG_PARAM/INTERNAL/DEVICE_OFFLINE）→
          // rejected 终态。探针不区分可恢复性：一律停（红线）。
          this.state = msg.code === "KICKED" ? "kicked" : "rejected";
          finish(this.state);
          this.socket?.close();
          return;
        }
        if (msg.type === "data") {
          // 数据面交给 onData；未 paired 时服务端帧直接忽略（客户端不消费）。
          if (this.state === "paired") this.options.onData?.(msg.payload);
        }
      });
      ws.addEventListener("close", () => {
        if (this.state === "authenticating" || this.state === "connecting") {
          // 握手未完成即断：视为被拒（不重试）。
          this.state = "rejected";
          finish("rejected");
          return;
        }
        this.state =
          this.state === "paired" || this.state === "rejected" || this.state === "kicked"
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

  private mapStateToResult(): RelayConnectionResult {
    if (this.state === "paired") return "paired";
    if (this.state === "kicked") return "kicked";
    if (this.state === "rejected") return "rejected";
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
