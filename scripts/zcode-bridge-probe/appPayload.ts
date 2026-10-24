// scripts/zcode-bridge-probe/appPayload.ts — paired 后的只读 payload 通道（B1 实连半场）。
//
// 归属：工作包 B 探针工具目录，不在产品运行链上。
//
// 只读白名单（唯一允许发出的两类 data payload，均为主桌面→终端的**纯读查询**，官方
// 终端在 paired 后同款使用；行号 @ 固定 SHA 3c5b2276）：
//   - `bootstrap-request`：`{zcode_type, requestId}` → `bootstrap-response`
//     （请求 packages/web/src/web-remote-control/app/main.tsx:1504-1514；payload 形状
//     packages/shared/src/web-remote-control.ts:406-416；result=窗口/版本/workspace 列表/
//     活动态，web-remote-control.ts:726-735）
//   - `workspace-list-request`：`{zcode_type, requestId}` → `workspace-list-response`
//     （main.tsx:1572-1591；形状 web-remote-control.ts:418-428,672-676）
//   之外一切 payload（workspace-bridge-open / platform-request 写向 / mobile-task-view 等）
//   一律不在白名单——探针不打开工作区桥、不应答交互、不改任何状态。
//   帧信封：`{type:"data", payload, client_ts}` 由 relayAuth.RelayTerminalConnection.sendData
//   统一封装（shared/web-remote-relay/contract.ts:36-44；发送门禁 transport.ts:216-222）。
//
// 语法纪律：可被 `node`（type-stripping）直接执行——仅可擦除 TS 语法。

import * as crypto from "node:crypto";

/** 只读请求白名单（运行期强制 PROBE_GUARDS.readonlyOnly 的唯一出口）。 */
export const READONLY_REQUEST_TYPES = ["bootstrap-request", "workspace-list-request"] as const;
export type ReadonlyRequestType = (typeof READONLY_REQUEST_TYPES)[number];

export type AppPayloadOutcome =
  { kind: "ok"; payload: Record<string, unknown> } | { kind: "send-failed" } | { kind: "timeout" };

interface ReadonlySender {
  sendData(payload: unknown): boolean;
}

/**
 * 单通道只读 payload 收发：持有 conn.onData 单槽，按 requestId+type 匹配响应；
 * 不匹配的入站帧全部记入 unsolicited（脱敏前原样持有在内存，由编排层脱敏后落证）。
 */
export class ReadonlyPayloadChannel {
  private readonly pending = new Map<
    string,
    (outcome: Extract<AppPayloadOutcome, { kind: "ok" }>) => void
  >();
  private attachedConn?: { onData?: (payload: unknown) => void };
  /** 未经 request 匹配的入站 data payload（含推送），只增不清（探针运行期短）。 */
  readonly unsolicited: unknown[] = [];

  /** 把本通道挂到连接的 onData 单槽上（重复 attach 换连接：探针单连接，不并发）。 */
  attach(conn: { onData?: (payload: unknown) => void }): void {
    this.attachedConn = conn;
    conn.onData = (payload) => this.handle(payload);
  }

  detach(): void {
    if (this.attachedConn) this.attachedConn.onData = undefined;
    this.attachedConn = undefined;
  }

  /**
   * 发出白名单内只读请求并等待同 requestId 的响应。有界（timeoutMs）；
   * 发送被客户端门禁挡下（未 paired / socket 非 OPEN）→ send-failed，不排队不重发。
   */
  request(
    sender: ReadonlySender,
    type: ReadonlyRequestType,
    timeoutMs: number
  ): Promise<AppPayloadOutcome> {
    // 运行期白名单闸：类型层 ReadonlyRequestType 之外的一切（含未来的新 payload）
    // 在这里被硬拒——这是 PROBE_GUARDS.readonlyOnly 的强制点，不是注释。
    if (!READONLY_REQUEST_TYPES.includes(type)) {
      throw new Error(`readonly-whitelist: payload type not allowed: ${type}`);
    }
    const requestId = `b1-${type}-${crypto.randomBytes(6).toString("hex")}`;
    return new Promise<AppPayloadOutcome>((resolve) => {
      const timer = setTimeout(
        () => {
          this.pending.delete(requestId);
          resolve({ kind: "timeout" });
        },
        Math.max(1, timeoutMs)
      );
      this.pending.set(requestId, (payload) => {
        clearTimeout(timer);
        resolve({ kind: "ok", payload });
      });
      const sent = sender.sendData({ zcode_type: type, requestId });
      if (!sent) {
        clearTimeout(timer);
        this.pending.delete(requestId);
        resolve({ kind: "send-failed" });
      }
    });
  }

  private handle(payload: unknown): void {
    if (payload && typeof payload === "object") {
      const record = payload as { zcode_type?: unknown; requestId?: unknown };
      if (typeof record.requestId === "string" && this.pending.has(record.requestId)) {
        const resolve = this.pending.get(record.requestId);
        this.pending.delete(record.requestId);
        resolve?.({ kind: "ok", payload: payload as Record<string, unknown> });
        return;
      }
    }
    this.unsolicited.push(payload);
  }
}
