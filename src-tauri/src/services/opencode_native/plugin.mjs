// Tuvis OpenCode service bridge plugin (generated/installed by Tuvis).
//
// Runs inside the OpenCode service process. It only answers "does this session
// belong to this service, and what is its canonical directory?" over a local
// Unix socket. Tuvis separately verifies the kernel peer identity (PID/UID/
// start time/executable) against the OpenCode service registration; this
// plugin's replies are not treated as proof of process identity on their own.
//
// The plugin reads the socket path and one-time token from bridge.json, which
// Tuvis writes with mode 0600. It never logs message text, credentials or
// session contents.
import fs from "node:fs";
import net from "node:net";
import os from "node:os";

const configPath =
  process.env.TUVIS_OPENCODE_BRIDGE_CONFIG ||
  `${os.homedir()}/.tuvis/opencode-bridge/bridge.json`;

function readConfig() {
  const value = JSON.parse(fs.readFileSync(configPath, "utf8"));
  if (!value || typeof value.socket !== "string" || typeof value.token !== "string") {
    throw new Error("tuvis bridge config is invalid");
  }
  return value;
}

function canonical(value) {
  if (typeof value !== "string" || value.length === 0) return null;
  try {
    return fs.realpathSync(value);
  } catch {
    return null;
  }
}

async function lookupSession(ctx, sessionID) {
  if (ctx?.session?.get) {
    const value = await ctx.session.get({ sessionID });
    const session = value?.data ?? value;
    if (session && typeof session === "object") return session;
  }
  if (ctx?.session?.list) {
    const value = await ctx.session.list();
    const list = value?.data ?? value;
    if (Array.isArray(list)) {
      return list.find((item) => item && item.id === sessionID) ?? null;
    }
  }
  return null;
}

export default {
  id: "tuvis.opencode.bridge",
  async setup(ctx) {
    let config;
    try {
      config = readConfig();
    } catch {
      return () => {};
    }
    let socket;
    let buffer = "";
    let closed = false;
    const incarnation =
      globalThis.crypto?.randomUUID?.() ??
      `tuvis_${Date.now().toString(36)}_${Math.random().toString(36).slice(2)}`;
    const location = canonical(ctx?.location?.directory ?? process.cwd());
    const send = (value) => {
      if (!socket || socket.destroyed) return;
      try {
        socket.write(JSON.stringify(value) + "\n");
      } catch {
        /* connection closed; state is invalidated by Tuvis */
      }
    };
    const handle = async (message) => {
      if (!message || message.type !== "confirm_session") return;
      const requestId = typeof message.requestId === "string" ? message.requestId : "";
      const sessionID = typeof message.sessionID === "string" ? message.sessionID : "";
      const expected = canonical(message.directory);
      if (!requestId || !sessionID) {
        send({ type: "confirm_session_result", requestId, ok: false, reason: "bad-request" });
        return;
      }
      try {
        const session = await lookupSession(ctx, sessionID);
        const actual = canonical(
          session?.location?.directory ?? session?.directory ?? session?.path ?? null
        );
        if (!session || session.id !== sessionID) {
          send({ type: "confirm_session_result", requestId, ok: false, reason: "unknown-session" });
          return;
        }
        if (!expected || !actual || expected !== actual) {
          send({
            type: "confirm_session_result",
            requestId,
            ok: false,
            reason: "directory-mismatch",
          });
          return;
        }
        send({
          type: "confirm_session_result",
          requestId,
          ok: true,
          session: { id: session.id, directory: actual },
        });
      } catch {
        send({ type: "confirm_session_result", requestId, ok: false, reason: "lookup-failed" });
      }
    };
    const schedule = () => {
      if (closed) return;
      setTimeout(() => {
        if (!closed) connect();
      }, 500);
    };
    const connect = () => {
      if (closed) return;
      // Tuvis 重启/重装可能重写 bridge.json；每次重连都现读，避免拿着首次
      // setup 时的旧 token 永久被拒。
      let current = config;
      try {
        current = readConfig();
      } catch {
        schedule();
        return;
      }
      config = current;
      const next = net.createConnection(current.socket);
      socket = next;
      next.setEncoding("utf8");
      next.on("connect", () => {
        send({
          type: "hello",
          token: current.token,
          incarnation,
          location,
          pid: process.pid,
        });
      });
      next.on("data", (chunk) => {
        buffer += chunk;
        if (buffer.length > 65536) {
          buffer = "";
          next.destroy();
          return;
        }
        let index;
        while ((index = buffer.indexOf("\n")) >= 0) {
          const line = buffer.slice(0, index);
          buffer = buffer.slice(index + 1);
          if (!line) continue;
          try {
            void handle(JSON.parse(line));
          } catch {
            /* ignore malformed frames */
          }
        }
      });
      next.on("error", () => {});
      next.on("close", () => {
        socket = undefined;
        schedule();
      });
    };
    connect();
    return () => {
      closed = true;
      try {
        socket?.destroy();
      } catch {
        /* already closed */
      }
      void closed;
    };
  },
};
