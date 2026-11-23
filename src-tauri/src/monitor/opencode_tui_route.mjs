// 兔维斯 OpenCode TUI 路由观察插件（R1 来源；R0b §9 实测形态；H03 追加 echo-challenge ack）
//
// 装载形态：<config>/opencode/plugins/tuvis-route-observer/tui.mjs
// （v2.2.5 对 <config>/plugins/ 下每个子目录自动发现；**不改 config**、无驻留——
//  TUI 退出时插件与 HTTP 服务共亡）
//
// 读数通道（R0b §9.3 实测）：
//   GET /health  → {"boot":"<本次 TUI 进程 boot UUID>","pid":<TUI pid>}
//   GET /current → {"route":<router.current() 原样>,"at":<请求时刻毫秒>}
//     route 形态：首页 {"type":"home"}；会话页 {"type":"session","sessionID":"ses_…"}
//     （请求时刻同步调用 router.current()——读时刻快照，读与消费之间的导航竞态由
//      消费方「读时刻为真」语义容忍，R0b §9.6 边界 1）
//
// 端点交付（约束 2，禁止端口扫描）：插件在 setup 时把 {pid, boot, port} 自述写入
// ~/.tuvis/opencode-tui/<pid>.json（0600，目录 0700）；兔维斯侧只读该登记文件。
// 尽力清理：cleanup 时删除登记文件与探针控制文件（SIGKILL 等异常退出由兔维斯侧
// 残迹清理兜底）。
//
// 只读边界（H03 收窄）：**除探针会话 ack 标题外不写任何 OpenCode 数据**——
// 不驱动路由导航（router.navigate 契约不触碰）、不订阅事件、不写用户会话/配置。
// 唯一写动作 = echo-challenge 协议的 ack：向探针会话（兔维斯自建专用 session，
// 非用户会话）覆盖写标题 `ack:<nonce>:<tui-pid>:<acktime>`。探针会话写纪律
// （H02 R1 复审裁定）：探针会话只允许兔维斯（challenge）与锁定 TUI（ack）写标题，
// 零非标题写入；本插件严格遵守（只调 session.update 改标题，不发消息不改元数据）。
//
// echo-challenge ack 循环（H03；机制依据 = H02 证据文档）：
//   每 1s tick：读探针控制文件 ~/.tuvis/opencode-tui/<pid>.probe（兔维斯写门每轮
//   challenge 前重写、轮次结束撤销，内容 {"sessionID","expectPid","expectBoot",
//   "nonce","deadlineMs"}，0600——授权绑定轮次+进程代际+有效期，I5）→ 授权复核：
//   expectPid ≠ 本进程 pid（多 TUI 只有被锁定窗口应答）、expectBoot ≠ 本次 boot
//   （PID 复用/换代不继承授权）、now > deadlineMs（过期轮不应答）→ 任一不符跳过 →
//   ctx.data.session.get(探针会话) 读标题（**缓存层，只随 SSE 失效刷新**——这是
//   v2.2.5 的实测行为事实、非正式契约，已 hash 锚定；失效为 per-session 粒度）→
//   标题匹配 ^chal:<nonce>:<writetime>$ 且 nonce == 授权轮次且未应答过 →
//   fresh ctx.client.session.update 写 ack 标题（写路径落哪个实例无所谓，判据看
//   内容）。ACK 任务**串行化**（上一 ACK 未完成不开启下一个，防旧请求迟到覆盖新
//   轮槽位）；写前复核授权与槽位（撤销/换轮/过期即放弃）；写结果未知（异常）时
//   **保守保留去重记录**，绝不无界重写（I6）。失效方向：缓存不随 SSE 刷新 =
//   challenge 不可见 = 零 ack = fail-closed（可用性损失）；升级锚定二进制后必须
//   复验 E1/E1c（H02 结论速览 9）。

import http from "node:http";
import fs from "node:fs";
import os from "node:os";
import crypto from "node:crypto";

const REGISTRATION_DIR = `${os.homedir()}/.tuvis/opencode-tui`;
// 探针控制文件（兔维斯写门 → 插件；不带 .json 后缀，观察采集只枚举 *.json）。
const PROBE_CONTROL_SUFFIX = ".probe";
// ack tick 周期（毫秒）：H02 实测 challenge→ack 典型 ≤1.2s，1s tick 将量化误差减半。
const ACK_TICK_MS = 1000;
// challenge 标题形态（与兔维斯写门 binding_gate.rs 同一协议：nonce 32 位小写 hex）。
const CHALLENGE_PATTERN = /^chal:([0-9a-f]{32}):(\d+)$/;

export default {
  id: "tuvis-route-observer",
  setup(ctx) {
    const router = ctx?.ui?.router;
    const data = ctx?.data;
    const client = ctx?.client;
    // 路由 API 不可用（上游版本形态漂移）：静默退出——消费侧保持「未知」语义。
    // （ack 循环依赖 data/client；路由缺失时本插件整体不 setup，与既有语义一致。）
    if (!router || typeof router.current !== "function") {
      return () => {};
    }
    // 同一 TUI 进程可能复用模块并再次 setup，每次启用都需要独立身份。
    const BOOT = crypto.randomUUID();
    // 探针控制文件缓存（每 tick 重读，文件极小；写门每轮 challenge 前重写 → 自愈陈旧）。
    function readProbeControl() {
      try {
        const raw = fs.readFileSync(
          `${REGISTRATION_DIR}/${process.pid}${PROBE_CONTROL_SUFFIX}`,
          "utf8",
        );
        const parsed = JSON.parse(raw);
        if (
          parsed &&
          typeof parsed.sessionID === "string" &&
          parsed.sessionID.length > 0 &&
          Number.isFinite(parsed.expectPid)
        ) {
          return parsed;
        }
      } catch {}
      return null;
    }

    // 进程内已答 nonce 集合：**只进不出**（写结果未知时保守去重，绝不无界重写；
    // 每 gate 轮一个新 nonce，增长速率 = 写门轮次，量级可忽略——不设清空上限）。
    const answered = new Set();
    // ACK 任务串行化（I6）：上一 ACK 未完成时跳过本 tick，防旧请求迟到完成覆盖
    // 新轮槽位（守卫在前轮结束后才写新 nonce，旧 ack 一旦覆盖即 NonceMismatch
    // 误伤新轮）。
    let ackBusy = false;

    // 授权复核（I5/I6）：控制文件仍在场 ∧ 仍是同一轮次 ∧ 进程代际匹配 ∧ 未过期。
    // 写 ack 前调用一次；不符 = 授权已被撤销/换轮/过期 → 放弃本 tick。
    function authorizedFor(control, nonce) {
      const current = readProbeControl();
      if (!current) return false; // 已撤销（轮次结束）
      if (current.expectPid !== process.pid) return false;
      if (current.expectBoot && current.expectBoot !== BOOT) return false;
      if (typeof current.deadlineMs === "number" && Date.now() > current.deadlineMs) {
        return false;
      }
      return current.nonce === nonce && current.sessionID === control.sessionID;
    }

    // 一次 ack tick。任何失败静默跳过；绝不影响 /health /current。
    async function ackTick() {
      const control = readProbeControl();
      // 授权复核（读侧）：expectPid（多 TUI 只有被锁定窗口应答）+ expectBoot
      // （PID 复用/换代不继承）+ deadlineMs（过期轮不应答）。
      if (!control || control.expectPid !== process.pid) return;
      if (control.expectBoot && control.expectBoot !== BOOT) return;
      if (typeof control.deadlineMs === "number" && Date.now() > control.deadlineMs) return;
      let session = null;
      try {
        session = await data.session.get(control.sessionID);
      } catch {
        return; // 探针会话读不到（缓存未刷新/已删）：本轮不作为，fail-closed 语义
      }
      const title = session && typeof session.title === "string" ? session.title : null;
      if (!title) return;
      const match = CHALLENGE_PATTERN.exec(title);
      if (!match) return; // 非 challenge 形态（含自己/他者的 ack 回显）：不动作
      const nonce = match[1];
      // 只答**当前授权轮次**（I5）：nonce 与控制文件绑定，旧轮 challenge 不应答。
      if (control.nonce !== nonce) return;
      if (answered.has(nonce)) return; // 重复 challenge / 未知结果不二次应答（保守去重）
      if (ackBusy) return; // 串行化：上一 ACK 未完成不开启下一个
      ackBusy = true;
      try {
        let fresh = null;
        try {
          fresh = await data.session.get(control.sessionID);
        } catch {
          return; // 槽位复核失败：不盲写
        }
        if (!fresh || fresh.title !== title) return; // 槽位已变（他轮覆盖）：放弃
        // 写前复核（I6 残余修正）：放在**最后一次异步等待之后**——槽位读取与授权
        // 复核之间不再有任何 await，撤销/换轮/过期在写前必然可见。
        if (!authorizedFor(control, nonce)) return;
        if (answered.has(nonce)) return;
        answered.add(nonce); // 未知结果保守去重：写入前登记，异常也不重写
        const ack = `ack:${nonce}:${process.pid}:${Date.now()}`;
        try {
          // fresh client 写 ack 标题（H02 实测：分裂脑下写路径可落旧实例——判据看
          // 内容）。旧轮 ACK 若在 Rust 新轮开始后才迟到落库：守卫按 nonce 判
          // OC_GATE_NONCE_MISMATCH（fail-closed，轮次隔离由 nonce 承担）。
          await client.session.update({ sessionID: control.sessionID, title: ack });
        } catch {
          // 写结果未知：保守保留去重记录（绝不无界重写）；本轮由守卫 fail-closed
          // 拒绝，下一轮换新 nonce。
        }
      } finally {
        ackBusy = false;
      }
    }

    const ackTimer = data && client ? setInterval(() => {
      ackTick().catch(() => {});
    }, ACK_TICK_MS) : null;
    if (typeof ackTimer === "object" && ackTimer && typeof ackTimer.unref === "function") {
      ackTimer.unref();
    }

    const server = http.createServer((req, res) => {
      if (req.url === "/health") {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify({ boot: BOOT, pid: process.pid }));
        return;
      }
      if (req.url === "/current") {
        let route = null;
        try {
          route = router.current();
        } catch {
          route = null; // 读失败 → route=null → 消费侧按「未知」处理（不冒充首页）
        }
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify({ route, at: Date.now() }));
        return;
      }
      res.writeHead(404, { "content-type": "application/json" });
      res.end("{}");
    });
    let registered = null;
    server.listen(0, "127.0.0.1", () => {
      const port = server.address().port;
      try {
        fs.mkdirSync(REGISTRATION_DIR, { recursive: true, mode: 0o700 });
        registered = `${REGISTRATION_DIR}/${process.pid}.json`;
        fs.writeFileSync(
          registered,
          JSON.stringify({ pid: process.pid, boot: BOOT, port, at: Date.now() }),
          { mode: 0o600 },
        );
        fs.chmodSync(registered, 0o600);
      } catch {
        registered = null; // 登记写失败：观察不可达，消费侧保持「未知」
      }
    });
    return () => {
      if (typeof ackTimer !== "undefined" && ackTimer) {
        clearInterval(ackTimer);
      }
      if (registered) {
        try {
          fs.rmSync(registered, { force: true });
        } catch {}
      }
      // 尽力清理探针控制文件（下一轮写门会重写；异常退出由兔维斯残迹清理兜底）。
      try {
        fs.rmSync(`${REGISTRATION_DIR}/${process.pid}${PROBE_CONTROL_SUFFIX}`, { force: true });
      } catch {}
      try {
        server.close();
      } catch {}
    };
  },
};
