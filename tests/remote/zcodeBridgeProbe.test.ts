// @vitest-environment node
// tests/remote/zcodeBridgeProbe.test.ts — B1 实连半场探针的**客户端可测逻辑**红测。
//
// 覆盖面（全部为纯逻辑 / 仅 127.0.0.1 本地 socket / 仅合成凭据，不连接任何真实远端）：
//   1. 官方链接解析（sid/mid 提取 + A4 拒绝面）
//      → src-tauri/src/services/zcode_control/config.rs:17-27（键/主机/上限）、
//        决策文件 §6.2-15（/remote/vN、legacy hash/t 拒收）
//   2. OAuth 授权 URL 构造（ZAI 通道参数形状）
//      → packages/web/src/auth/zaiWebOAuthProvider.ts:184-191 @ 3c5b2276；
//        webZaiOAuthConfig.ts:30,56-58（authorize origin / token 路径 / 公开 client_id）
//   3. 本地回环回调监听（state 校验 + code|authCode|error 分型 + 不回显参数）
//      → zaiWebOAuthProvider.ts:194-209（回调参数解析）；
//        entry.ts:17-26（callback 判定 = state + code|authCode|error）
//   4. code 换 token 请求形状（POST /api/v1/oauth/token {provider,code,redirect_uri,state}）
//      → zaiWebOAuthProvider.ts:211-239（body 形状与信封归一）、oauth.ts:12（provider="zai"）
//   5. 凭据脱敏（稳定别名替换 / JWT 掩码 / fail-closed 断言）
//   6. 证据落盘 fail-closed（含密内容拒写、不产生文件）
//   7. settings 只读读取官方链接（node:sqlite readOnly，settings(key,value)）
//      → src-tauri/src/database/schema.rs:14（表形状）、
//        src-tauri/src/database/dao/settings.rs:20（读取语句）
//   8. 只读 payload 通道（白名单闸 + requestId 匹配 + 超时/未 solicited 分型）
//      → packages/web/src/web-remote-control/app/main.tsx:1504-1514,1572-1591 @ 3c5b2276
//        （bootstrap-request / workspace-list-request 的官方终端只读查询）、
//        shared/web-remote-control.ts:406-428,672-676（payload 形状）
//
// 全部凭据为 b1-fake-* 合成值；不含任何真实 SID/JWT/token/链接。
import * as fs from "node:fs";
import * as http from "node:http";
import * as os from "node:os";
import * as path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

// 本文件跑真实 127.0.0.1 socket：先关掉全局 MSW（仅本文件作用域，与 relay 测试同款）。
import { server as mswServer } from "../msw/server";

beforeAll(() => {
  mswServer.close();
});
afterAll(() => {
  mswServer.listen({ onUnhandledRequest: "bypass" });
});

import { parseOfficialLink } from "../../scripts/zcode-bridge-probe/officialLink";
import { redactPayload } from "../../scripts/zcode-bridge-probe/auth-check";
import {
  OFFICIAL_OAUTH_CLIENT_ID,
  LoopbackCallbackServer,
  buildZaiAuthorizeUrl,
  exchangeCodeForToken,
} from "../../scripts/zcode-bridge-probe/oauthFlow";
import {
  SecretRedactor,
  createEvidenceRunDir,
  maskJwt,
  writeEvidenceFiles,
} from "../../scripts/zcode-bridge-probe/sanitize";
import { readOfficialLinkFromSettings } from "../../scripts/zcode-bridge-probe/settingsLink";
import {
  READONLY_REQUEST_TYPES,
  ReadonlyPayloadChannel,
  type ReadonlyRequestType,
} from "../../scripts/zcode-bridge-probe/appPayload";

/** 合成官方链接（b1-fake-* 全合成，形状与真实链接同构）。 */
const FAKE_LINK =
  "https://zcode.z.ai/remote/v4?sid=b1-fake-sid-0001&mid=b1-fake-mid-0001&name=b1-fake-device&app_version=3.16.2";

describe("zcode 官方桥接探针：客户端可测逻辑（B1 实连半场）", () => {
  it("官方链接解析：提取 sid/mid，并拒绝 A4 合同外的形状", () => {
    const ok = parseOfficialLink(FAKE_LINK);
    expect(ok).toMatchObject({
      ok: true,
      origin: "https://zcode.z.ai",
      pathVersion: "v4",
      sid: "b1-fake-sid-0001",
      mid: "b1-fake-mid-0001",
    });

    expect(parseOfficialLink("http://zcode.z.ai/remote/v4?sid=x").ok).toBe(false);
    expect(parseOfficialLink("http://zcode.z.ai/remote/v4?sid=x")).toMatchObject({
      ok: false,
      reason: "scheme",
    });
    expect(parseOfficialLink("https://evil.example.com/remote/v4?sid=x")).toMatchObject({
      ok: false,
      reason: "host",
    });
    expect(parseOfficialLink("https://zcode.z.ai/remote/v2?sid=x")).toMatchObject({
      ok: false,
      reason: "path",
    });
    expect(parseOfficialLink("https://zcode.z.ai/remote/v4")).toMatchObject({
      ok: false,
      reason: "sid-missing",
    });
    expect(parseOfficialLink("https://zcode.z.ai/remote/v4?sid=a&sid=b")).toMatchObject({
      ok: false,
      reason: "sid-duplicate",
    });
    expect(parseOfficialLink(`https://zcode.z.ai/remote/v4?sid=${"s".repeat(129)}`)).toMatchObject({
      ok: false,
      reason: "sid-invalid",
    });
    // legacy hash / t 参数被官方解析器直接拒收（决策文件 §6.2-15）。
    expect(parseOfficialLink("https://zcode.z.ai/remote/v4?sid=a&hash=xyz")).toMatchObject({
      ok: false,
      reason: "legacy-param",
    });
    expect(parseOfficialLink("https://zcode.z.ai:8443/remote/v4?sid=a")).toMatchObject({
      ok: false,
      reason: "port",
    });
    expect(parseOfficialLink("https://u:p@zcode.z.ai/remote/v4?sid=a")).toMatchObject({
      ok: false,
      reason: "userinfo",
    });
    expect(parseOfficialLink(`https://zcode.z.ai/remote/v4?sid=${"s".repeat(5000)}`)).toMatchObject(
      { ok: false, reason: "too-long" }
    );
    expect(parseOfficialLink("not a url")).toMatchObject({ ok: false, reason: "parse" });
  });

  it("授权 URL 构造：ZAI 通道官方参数形状（redirect_uri/response_type/client_id/state）", () => {
    const url = buildZaiAuthorizeUrl({
      redirectUri: "http://127.0.0.1:49152/callback",
      state: "b1-fake-state-1",
    });
    expect(url.startsWith("https://chat.z.ai/api/oauth/authorize?")).toBe(true);
    const parsed = new URL(url);
    expect(parsed.searchParams.get("redirect_uri")).toBe("http://127.0.0.1:49152/callback");
    expect(parsed.searchParams.get("response_type")).toBe("code");
    expect(parsed.searchParams.get("client_id")).toBe(OFFICIAL_OAUTH_CLIENT_ID);
    expect(parsed.searchParams.get("state")).toBe("b1-fake-state-1");
  });

  it("回环回调监听：state 不匹配继续等待；code/authCode/error 分型；响应不回显参数", async () => {
    const srv = await LoopbackCallbackServer.start();
    try {
      expect(srv.redirectUri.startsWith("http://127.0.0.1:")).toBe(true);

      // 先打一个 state 不匹配的回调：必须被忽略并继续等待（不可消费他人/重放的回调）。
      const pending = srv.waitCallback("b1-fake-state-1", 3_000);
      const wrong = await fetch(`${srv.redirectUri}?state=WRONG&code=x`);
      expect(wrong.status).toBe(400);
      await new Promise((resolve) => setTimeout(resolve, 40));
      expect(srv.ignoredRequests).toBe(1);

      const res = await fetch(`${srv.redirectUri}?state=b1-fake-state-1&code=b1-fake-code-1`);
      expect(res.status).toBe(200);
      expect(await res.text()).not.toContain("b1-fake-code-1"); // 响应体不回显任何参数
      await expect(pending).resolves.toMatchObject({
        kind: "code",
        code: "b1-fake-code-1",
        state: "b1-fake-state-1",
      });

      // error 重定向 → authorize-error 分型（state 匹配）。
      const srvErr = await LoopbackCallbackServer.start();
      try {
        const pendingErr = srvErr.waitCallback("b1-fake-state-2", 3_000);
        await fetch(
          `${srvErr.redirectUri}?state=b1-fake-state-2&error=access_denied&error_description=nope`
        );
        await expect(pendingErr).resolves.toMatchObject({
          kind: "authorize-error",
          error: "access_denied",
          errorDescription: "nope",
        });
      } finally {
        await srvErr.close();
      }

      // authCode 别名参数（官方 parseCallbackParams 接受 code|authCode）。
      const srvAlias = await LoopbackCallbackServer.start();
      try {
        const pendingAlias = srvAlias.waitCallback("b1-fake-state-3", 3_000);
        await fetch(`${srvAlias.redirectUri}?state=b1-fake-state-3&authCode=b1-fake-code-9`);
        await expect(pendingAlias).resolves.toMatchObject({
          kind: "code",
          code: "b1-fake-code-9",
        });
      } finally {
        await srvAlias.close();
      }

      // 有界超时：窗口尽 → timeout（不会永久挂起）。
      const outcome = await srv.waitCallback("b1-never-arrives", 60);
      expect(outcome).toMatchObject({ kind: "timeout" });
    } finally {
      await srv.close();
    }
  });

  it("code 换 token：POST /api/v1/oauth/token 的请求形状与信封解析对齐官方", async () => {
    const seen: { method: string; path: string; body: unknown }[] = [];
    const fake = http.createServer((req, res) => {
      let body = "";
      req.on("data", (chunk: Buffer) => {
        body += chunk.toString("utf8");
      });
      req.on("end", () => {
        seen.push({ method: req.method ?? "", path: req.url ?? "", body: JSON.parse(body) });
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(
          JSON.stringify({
            code: 0,
            msg: "ok",
            data: {
              token: "b1-fake-jwt-header.b1-fake-jwt-payload.b1-fake-jwt-sig",
              zai: { access_token: "b1-fake-zai-at" },
              expires_in: 3600,
              user: { id: "b1-fake-user" },
            },
          })
        );
      });
    });
    await new Promise<void>((resolve) => fake.listen(0, "127.0.0.1", () => resolve()));
    const port = (fake.address() as AddressInfo).port;
    try {
      const result = await exchangeCodeForToken({
        zcodeOrigin: `http://127.0.0.1:${port}`,
        code: "b1-fake-code-1",
        redirectUri: "http://127.0.0.1:49152/callback",
        state: "b1-fake-state-1",
      });
      expect(result).toMatchObject({ kind: "ok" });
      if (result.kind !== "ok") return;
      expect(result.jwt).toBe("b1-fake-jwt-header.b1-fake-jwt-payload.b1-fake-jwt-sig");
      expect(result.expiresInSeconds).toBe(3600);
      expect(result.hasUserInfo).toBe(true);

      expect(seen).toHaveLength(1);
      expect(seen[0].method).toBe("POST");
      expect(seen[0].path).toBe("/api/v1/oauth/token");
      expect(seen[0].body).toEqual({
        provider: "zai",
        code: "b1-fake-code-1",
        redirect_uri: "http://127.0.0.1:49152/callback",
        state: "b1-fake-state-1",
      });
    } finally {
      await new Promise<void>((resolve) => fake.close(() => resolve()));
    }

    // 负例工厂：起一个一次性本地 server 跑完断言即关。
    const mk = (handler: (res: http.ServerResponse) => void) =>
      new Promise<{ url: string; close: () => Promise<void> }>((resolve) => {
        const s = http.createServer((_req, res) => handler(res));
        s.listen(0, "127.0.0.1", () => {
          const p = (s.address() as AddressInfo).port;
          resolve({
            url: `http://127.0.0.1:${p}`,
            close: () => new Promise<void>((r) => s.close(() => r())),
          });
        });
      });

    const codeFail = await mk((res) => {
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ code: 1, msg: "redirect_uri not allowed", data: null }));
    });
    try {
      await expect(
        exchangeCodeForToken({
          zcodeOrigin: codeFail.url,
          code: "c",
          redirectUri: "http://127.0.0.1:1/callback",
          state: "s",
        })
      ).resolves.toMatchObject({
        kind: "rejected",
        serverCode: 1,
        serverMsg: "redirect_uri not allowed",
      });
    } finally {
      await codeFail.close();
    }

    const httpFail = await mk((res) => {
      res.writeHead(500);
      res.end();
    });
    try {
      await expect(
        exchangeCodeForToken({
          zcodeOrigin: httpFail.url,
          code: "c",
          redirectUri: "http://127.0.0.1:1/callback",
          state: "s",
        })
      ).resolves.toMatchObject({ kind: "rejected", httpStatus: 500 });
    } finally {
      await httpFail.close();
    }

    // 不可达（无监听端口）：unreachable，不抛异常。
    const dead = await mk((_res) => {});
    const deadPort = new URL(dead.url).port;
    await dead.close();
    await expect(
      exchangeCodeForToken({
        zcodeOrigin: `http://127.0.0.1:${deadPort}`,
        code: "c",
        redirectUri: "http://127.0.0.1:1/callback",
        state: "s",
      })
    ).resolves.toMatchObject({ kind: "unreachable" });
  });

  it("凭据脱敏：稳定别名替换、JWT 掩码形态、fail-closed 断言", () => {
    const redactor = new SecretRedactor();
    expect(redactor.register("sid", "b1-fake-sid-AAAA-real")).toBe("SID_A");
    expect(redactor.register("sid", "b1-fake-sid-BBBB-real")).toBe("SID_B");
    expect(redactor.register("mid", "b1-fake-mid-AAAA-real")).toBe("MID_A");
    expect(redactor.register("jwt", "eyJfake.header.sig")).toBe("JWT_1");
    expect(redactor.register("relay-token", "b1-fake-tok-1")).toBe("TOK_1");

    expect(
      redactor.sanitizeText("sid=b1-fake-sid-AAAA-real jwt=eyJfake.header.sig tok=b1-fake-tok-1")
    ).toBe("sid=SID_A jwt=JWT_1 tok=TOK_1");

    const deep = redactor.sanitizeDeep({
      list: [{ sid: "b1-fake-sid-BBBB-real" }],
      mid: "b1-fake-mid-AAAA-real",
      keep: "plain",
    });
    expect(deep).toEqual({ list: [{ sid: "SID_B" }], mid: "MID_A", keep: "plain" });

    const masked = maskJwt("eyJfake.header.sig");
    expect(masked).toMatch(/^eyJ…<len=\d+> sha256:[0-9a-f]{8}$/);
    expect(masked).toContain(`len=${"eyJfake.header.sig".length}`);
    expect(masked).not.toContain("header");

    expect(() => redactor.assertClean("leak: eyJfake.header.sig", "unit")).toThrow();
    expect(() => redactor.assertClean("clean: SID_A / TOK_1", "unit")).not.toThrow();
  });

  it("脱敏正则网：exact 名单外的敏感形键一律别名化且 fail-closed；非敏感键照常透传", () => {
    const redactor = new SecretRedactor();
    // 模拟入站推送帧：含评审示例的假设键（sessionId / access_token）与 net 其余分支。
    const frame = {
      type: "data",
      client_ts: 1_760_000_000_000,
      payload: {
        platform: "darwin",
        requestId: "b1-net-1",
        sessionId: "b1-fake-session-777",
        access_token: "b1-fake-access-token-777",
        account_jwt: "b1-fake-jwt-777.a.b",
        client_secret: "b1-fake-secret-777",
        callback_url: "https://zcode.z.ai/remote/v4?sid=b1-fake-sid-0001",
        config_path: "/Users/x/project",
        keepPlain: "plain-value",
      },
    };
    const out = redactPayload(redactor, frame) as typeof frame;
    const inner = out.payload as Record<string, unknown>;
    // 合法非敏感键照常透传（评审要求）。
    expect(out.type).toBe("data");
    expect(out.client_ts).toBe(1_760_000_000_000);
    expect(inner.platform).toBe("darwin");
    expect(inner.requestId).toBe("b1-net-1");
    expect(inner.keepPlain).toBe("plain-value");
    // 正则网命中键一律别名化（宁过勿漏）。
    expect(inner.sessionId).toMatch(/^SID_[A-Z]$/);
    expect(inner.access_token).toBe("TOK_1");
    expect(inner.client_secret).toBe("TOK_2");
    expect(inner.account_jwt).toBe("JWT_1");
    expect(inner.callback_url).toMatch(/^LINK_\d+$/);
    expect(inner.config_path).toMatch(/^PATH_\d+$/);
    // 序列化形态不再含任何 raw 值；assertClean 对登记值 fail-closed。
    const serialized = JSON.stringify(out);
    for (const raw of [
      "b1-fake-session-777",
      "b1-fake-access-token-777",
      "b1-fake-jwt-777.a.b",
      "b1-fake-secret-777",
      "b1-fake-sid-0001",
      "/Users/x/project",
    ]) {
      expect(serialized).not.toContain(raw);
    }
    expect(() => redactor.assertClean(serialized, "unit")).not.toThrow();
    expect(() =>
      redactor.assertClean("raw access_token=b1-fake-access-token-777", "unit")
    ).toThrow();
  });

  it("证据落盘 fail-closed：含密内容拒写且不产生文件；干净内容以 0600 落盘", () => {
    const base = fs.mkdtempSync(path.join(os.tmpdir(), "b1-evidence-"));
    try {
      const redactor = new SecretRedactor();
      const secret = "eyJleGFtcGxlLXJlYWwtc2VjcmV0LnNpZw";
      redactor.register("jwt", secret);
      const run = createEvidenceRunDir(base);
      expect(fs.existsSync(run.dir)).toBe(true);
      // 私有证据运行目录权限收紧为 owner-only（评审 Minor：mkdir 带 mode 0700）。
      expect(fs.statSync(run.dir).mode & 0o777).toBe(0o700);

      expect(() =>
        writeEvidenceFiles(redactor, run.dir, [
          { name: "transcript.json", content: `{"leak":"${secret}"}` },
        ])
      ).toThrow();
      expect(fs.existsSync(path.join(run.dir, "transcript.json"))).toBe(false);

      writeEvidenceFiles(redactor, run.dir, [
        { name: "transcript.json", content: '{"clean":"JWT_1"}' },
      ]);
      const written = path.join(run.dir, "transcript.json");
      expect(fs.existsSync(written)).toBe(true);
      // 私有证据文件权限收紧为 owner-only。
      const mode = fs.statSync(written).mode & 0o777;
      expect(mode).toBe(0o600);
    } finally {
      fs.rmSync(base, { recursive: true, force: true });
    }
  });

  it("settings 只读读取官方链接：命中返回原值；缺键 missing；库不可用 unavailable", () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "b1-db-"));
    try {
      const dbPath = path.join(dir, "tuvis.db");
      const db = new DatabaseSync(dbPath);
      db.exec("CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)");
      db.prepare("INSERT INTO settings (key, value) VALUES (?, ?)").run(
        "remote.zcode_official_url",
        FAKE_LINK
      );
      db.close();

      expect(readOfficialLinkFromSettings(dbPath)).toMatchObject({ kind: "ok", link: FAKE_LINK });
      expect(readOfficialLinkFromSettings(dbPath, "nope.key")).toMatchObject({ kind: "missing" });
      expect(readOfficialLinkFromSettings(path.join(dir, "absent.db"))).toMatchObject({
        kind: "unavailable",
      });
    } finally {
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });

  it("只读 payload 通道：白名单闸、requestId 匹配、超时与未 solicited 帧分型", async () => {
    const channel = new ReadonlyPayloadChannel();
    const sent: unknown[] = [];
    const conn: { onData?: (payload: unknown) => void; sendData: (p: unknown) => boolean } = {
      sendData: (p) => {
        sent.push(p);
        return true;
      },
    };
    channel.attach(conn);

    const pending = channel.request(conn, "bootstrap-request", 2_000);
    expect(sent).toHaveLength(1);
    const first = sent[0] as { zcode_type: string; requestId: string };
    expect(first.zcode_type).toBe("bootstrap-request");
    expect(typeof first.requestId).toBe("string");

    // requestId 不匹配的响应不消费。
    conn.onData?.({ zcode_type: "bootstrap-response", requestId: "other", success: true });
    expect(channel.unsolicited).toHaveLength(1);
    // 正确响应消费 pending。
    conn.onData?.({
      zcode_type: "bootstrap-response",
      requestId: first.requestId,
      success: true,
      result: { workspaces: [] },
    });
    await expect(pending).resolves.toMatchObject({ kind: "ok" });

    // 有界超时。
    const timed = await channel.request(conn, "workspace-list-request", 40);
    expect(timed).toMatchObject({ kind: "timeout" });

    // 发送被门禁挡下（未 paired 等）→ send-failed，不挂起。
    const refusing: { onData?: (p: unknown) => void; sendData: (p: unknown) => boolean } = {
      sendData: () => false,
    };
    channel.attach(refusing);
    await expect(channel.request(refusing, "workspace-list-request", 500)).resolves.toMatchObject({
      kind: "send-failed",
    });

    // 白名单闸：非只读类型一律拒绝（运行期强制 guards.readonlyOnly）。
    expect(() =>
      channel.request(conn, "workspace-bridge-open" as unknown as ReadonlyRequestType, 50)
    ).toThrow();
    expect(READONLY_REQUEST_TYPES).toEqual(["bootstrap-request", "workspace-list-request"]);
  });
});
