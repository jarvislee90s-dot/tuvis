// scripts/zcode-bridge-probe/auth-check.ts — ZCode 官方桥接「真实连接」探针入口（B1）。
//
// 当前状态（2026-10-24，B1 源码+fake 阶段）：**骨架，不连接任何远端**。
//   - 本文件只声明真实连接获批后将执行的步骤与安全闸；main() 一律拒绝执行；
//   - 真实连接半场被两个用户动作阻塞（决策文件 B1 节「停等项」）：
//       (a) 用户本人提供当前有效的官方远控链接并经 Tuvis A4 设置卡保存
//           （旧链接已 Not Found，禁止复用；链接本体不进仓库/日志/夹具）；
//       (b) 用户指定专用测试工作区/会话，并逐批授权真实动作；
//     以及 auth 通路的关键未决项：官方是否接受 Tuvis 控制的 OAuth redirect_uri——
//     只能由用户本人在场做浏览器授权探测，Agent 不得代登、不得读取浏览器凭据。
//   - 本文件不被产品引用；不进应用启动/扫描（工作包 B「固定产物与接口」）。
//
// wire 依据 = 固定官方源码 zai-org/ZCode @ 3c5b227649371507ddc4eb46ac5133afe6db1cab
// 的客户端侧实现 + tests/remote/zcodeBridgeRelay.test.ts 的 fake relay 锁形；
// 逐条行号见 scripts/zcode-bridge-probe/relayAuth.ts 头注与决策文件 B1 节。

import { RelayTerminalConnection, requestTerminalToken } from "./relayAuth";

/** 探针安全闸（红线，工作包 B 全局约束）：全部硬编码，运行期不可放宽。 */
export const PROBE_GUARDS = {
  /** 全程只允许存在一条上游连接；连接失败/被拒不重试。 */
  singleConnection: true,
  /** 无自动重连：任何 error/close 都是终态（官方终端有退避重连，探针一律不学）。 */
  reconnectPolicy: "never" as const,
  /** 收到 KICKED 立即退出并保持退出（不与用户官方页面互踢）。 */
  stopOnKicked: true,
  /** 只读白名单：仅读取设备/workspace/session 状态类帧；发送/审批/问答/打断/切模型全禁。 */
  readonlyOnly: true,
  /** 单次探针硬超时；到时主动断开并落脱敏证据。 */
  maxDurationMs: 60_000,
  /** 凭据只经运行期输入（操作员现场提供），禁止写盘、禁止进环境变量快照与日志。 */
  credentialsInMemoryOnly: true,
} as const;

/** 唯一放行开关：真实连接阶段必须由操作员显式提供，仓库内不得出现其真值。 */
const REAL_CONNECTION_UNLOCK_FLAG = "--i-am-operator-unlock-real-connection";

export type ProbeStep = {
  id: string;
  title: string;
  /** 该步骤的前置授权/输入（缺一即中止，不猜测）。 */
  requires: string;
  detail: string;
};

/** 真实连接获批后按序执行的步骤（每步产出脱敏证据，私有目录留原件）。 */
export const PROBE_STEPS: readonly ProbeStep[] = [
  {
    id: "s1-acquire-link",
    title: "取当前有效官方链接",
    requires: "用户本人复制链接并经 A4 设置卡保存；操作员现场确认 Saved 状态",
    detail:
      "链接仅经运行期输入进入本进程；校验其符合 A4 合同（https + zcode.z.ai + /remote/vN + 非空 sid）。" +
      "解析出 device_sid（sid）与可选 mid——这是 auth_init 的目标绑定（contract.ts:19-24）。",
  },
  {
    id: "s2-account-jwt",
    title: "账号凭据通路定案（interim→proven/unsupported 的分岔点）",
    requires: "用户本人在场完成官方 OAuth；Agent 不代登、不读浏览器/其他应用凭据",
    detail:
      "源码已钉死：终端 token 需要 Bearer 账号 JWT（webRemoteControlV2Token.ts:17），" +
      "而 JWT 只能来自官方网页 OAuth（authorize=chat.z.ai/api/oauth/authorize，" +
      "redirect_uri 钉死官方 /remote/v4，webZaiOAuthConfig.ts:55-62）。" +
      "本步由用户在浏览器完成官方授权后，仅当存在官方允许的通路把本轮 JWT 交给本进程时继续；" +
      "否则按计划输出 auth.status=unsupported、原因与合法备选，永久停止本探针。",
  },
  {
    id: "s3-terminal-token",
    title: "申请 role=terminal 一次性 token",
    requires: "s2 取得通路 + relay origin（源自链接主机）",
    detail:
      '调用 requestTerminalToken()（POST /api/v1/remote/token，Bearer JWT，{role:"terminal"}）；' +
      "auth-required → 中止；relay-unavailable → 中止；不重试。token 只在内存，用后即弃。",
  },
  {
    id: "s4-single-connect",
    title: "单次 WS 连接 + auth_init 只读握手",
    requires: "s3 的 token；用户确认此刻不依赖官方网页会话（同 role 连接会互踢）",
    detail:
      "RelayTerminalConnection 单连接连 /api/v1/remote/ws2?token=…；" +
      "auth_init 只带 device_sid/meta/client_ts；等 auth_ack(pair_status)；" +
      "KICKED/AUTH_FAILED/任何 error → 立即终态退出（PROBE_GUARDS）。",
  },
  {
    id: "s5-readonly-observe",
    title: "只读观察设备/workspace/session 状态",
    requires: "s4 进入 paired；专用测试工作区已指定",
    detail:
      "仅接收并记录快照类帧（脱敏后入 tests/fixtures 或私有 sanitized 目录）；" +
      "不发送 data、不应答 pending、不切模型/思考强度。超时 maxDurationMs 主动断开。",
  },
  {
    id: "s6-teardown",
    title: "断开与证据落盘",
    requires: "无",
    detail:
      "主动 close；统计握手往返/权限范围/退出码；核对 token 不可复用（同 token 二连必被拒的" +
      "官方一次性语义已在 fake 锁形）；脱敏证据写私有 run 目录，仓库只进 sanitized fixture。",
  },
];

/** 骨架入口：真实连接解锁前一律拒绝执行（本阶段不连接任何远端）。 */
export function main(argv: readonly string[] = process.argv): never {
  if (argv.includes(REAL_CONNECTION_UNLOCK_FLAG)) {
    throw new Error(
      "auth-check: 真实连接阶段尚未解锁——需先完成决策文件 B1 节「停等项」的用户动作，" +
        "并由用户裁决 s2 的账号凭据通路。解锁前的任何连接尝试都违反工作包 B 约束。"
    );
  }
  throw new Error(
    "auth-check: 当前为 B1 源码+fake 阶段骨架，不允许连接任何远端。" +
      `真实连接须由操作员携带 ${REAL_CONNECTION_UNLOCK_FLAG} 且停等项全部完成后才可运行。`
  );

  // —— 解锁后（未来批次）的实现要点（此处不可达，仅登记形状）：
  //
  // const result = await requestTerminalToken({ relayOrigin, accountJwt });
  // if (result.kind !== "ok") fail(result.kind);
  // const conn = new RelayTerminalConnection({ relayWsUrl, token: result.token, deviceSid });
  // conn.onData = (payload) => sanitizedRecord(payload); // 只读记录
  // const outcome = await conn.connect();
  // if (outcome !== "paired") fail(outcome);             // KICKED/rejected 即停
  // await observeReadonly(conn);                          // 白名单内只读
  // conn.close();
}

if (import.meta.url === `file://${process.argv[1]}`) {
  main();
}
