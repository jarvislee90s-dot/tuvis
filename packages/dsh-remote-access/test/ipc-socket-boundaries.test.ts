/**
 * NodeIpcTransport 的真实本地 socket 边界；只使用合成帧和独立临时目录。
 * 诊断通道仅观察真实客户端，等 data/close 事件后推进，不靠 sleep 猜分片时序。
 */
import { randomUUID } from "node:crypto";
import { channel } from "node:diagnostics_channel";
import { once } from "node:events";
import { mkdtempSync, rmSync } from "node:fs";
import { createServer, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { MAX_FRAME_BYTES, NodeIpcTransport, type IpcConnection } from "../src/ipc.js";

interface LocalSocket {
  connection: IpcConnection;
  client: Socket;
  peer: Socket;
  writeObserved(bytes: Buffer): Promise<void>;
}

async function within<T>(operation: Promise<T>): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      operation,
      new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new Error("local socket test did not reach its event barrier")), 3_000);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

async function withLocalSocket(run: (socket: LocalSocket) => Promise<void>): Promise<void> {
  const root = mkdtempSync(join(tmpdir(), "dsh-ipc-boundary-"));
  const address = process.platform === "win32"
    ? `\\\\.\\pipe\\dsh-ipc-boundary-${randomUUID()}`
    : join(root, "peer.sock");
  const server = createServer();
  const clients = new Set<Socket>();
  const peers = new Set<Socket>();
  const peerErrors: Error[] = [];
  server.on("connection", (peer) => {
    peers.add(peer);
    peer.on("error", (error) => peerErrors.push(error));
  });
  const socketChannel = channel("net.client.socket");
  const observe = (message: unknown): void => {
    clients.add((message as { socket: Socket }).socket);
  };
  let connection: IpcConnection | undefined;
  try {
    server.listen(address);
    await within(once(server, "listening"));
    const accepted = once(server, "connection");
    socketChannel.subscribe(observe);
    try {
      connection = await within(new NodeIpcTransport().connect(address));
    } finally {
      socketChannel.unsubscribe(observe);
    }
    const [peer] = await within(accepted) as [Socket];
    expect(clients.size, "observe exactly the real transport client").toBe(1);
    const client = [...clients][0]!;
    await within(run({
      connection,
      client,
      peer,
      async writeObserved(bytes) {
        // write() 边界不保证等于 data 边界；累计实际收到的字节再发送下一段。
        let received = 0;
        let onData: (chunk: Buffer) => void = () => {};
        const observed = new Promise<void>((resolve) => {
          onData = (chunk) => {
            received += chunk.length;
            if (received >= bytes.length) resolve();
          };
          client.on("data", onData);
        });
        try {
          peer.write(bytes);
          await within(observed);
          expect(received).toBe(bytes.length);
        } finally {
          client.off("data", onData);
        }
      },
    }));
    expect(peerErrors).toEqual([]);
  } finally {
    socketChannel.unsubscribe(observe);
    connection?.close();
    clients.forEach((socket) => socket.destroy());
    peers.forEach((socket) => socket.destroy());
    try {
      if (server.listening) {
        await within(new Promise<void>((resolve, reject) => {
          server.close((error) => error ? reject(error) : resolve());
        }));
      }
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  }
}

/** 合成合法 JSON 帧，精确填到指定 UTF-8 字节数。 */
function jsonFrame(byteLength: number, character: string): string {
  const envelope = '{"text":""}';
  const payloadBytes = byteLength - Buffer.byteLength(envelope);
  const width = Buffer.byteLength(character);
  return `{"text":"${character.repeat(Math.floor(payloadBytes / width))}${"x".repeat(payloadBytes % width)}"}`;
}

describe("NodeIpcTransport real local socket frame boundaries", () => {
  it.each(["x", "中", "😀"])("accepts a %s frame at exactly the UTF-8 byte budget", async (character) => {
    const frame = jsonFrame(MAX_FRAME_BYTES, character);
    expect(Buffer.byteLength(frame)).toBe(MAX_FRAME_BYTES);
    await withLocalSocket(async ({ connection, writeObserved }) => {
      const received = connection.recv();
      await writeObserved(Buffer.from(`${frame}\n`));
      expect(await received).toBe(frame);
      expect(connection.closed).toBe(false);
    });
  });

  it("rejects a newline-terminated multibyte frame one byte over budget even when its character count fits", async () => {
    const frame = jsonFrame(MAX_FRAME_BYTES + 1, "中");
    expect(frame.length).toBeLessThan(MAX_FRAME_BYTES);
    expect(Buffer.byteLength(frame)).toBe(MAX_FRAME_BYTES + 1);
    await withLocalSocket(async ({ connection, client, peer }) => {
      const rejected = expect(connection.recv()).rejects.toThrow("dsh ipc: frame over budget");
      const closed = once(client, "close");
      peer.write(`${frame}\n`);
      await rejected;
      await closed;
      expect(connection.closed).toBe(true);
      await expect(connection.recv()).rejects.toThrow("dsh ipc: frame over budget");
    });
  });

  it("waits for LF when CRLF is split across observed data events and preserves an embedded CR", async () => {
    await withLocalSocket(async ({ connection, writeObserved }) => {
      let settled = false;
      const received = connection.recv();
      void received.then(() => { settled = true; }, () => { settled = true; });
      await writeObserved(Buffer.from("left\rright\r"));
      expect(settled).toBe(false);
      await writeObserved(Buffer.from("\n"));
      expect(await received).toBe("left\rright");
    });
  });

  it("accepts a CRLF frame whose bytes before LF exactly fill the budget", async () => {
    const frame = jsonFrame(MAX_FRAME_BYTES - 1, "中");
    expect(Buffer.byteLength(`${frame}\r`)).toBe(MAX_FRAME_BYTES);
    await withLocalSocket(async ({ connection, writeObserved }) => {
      const received = connection.recv();
      await writeObserved(Buffer.from(`${frame}\r`));
      expect(connection.closed).toBe(false);
      await writeObserved(Buffer.from("\n"));
      expect(await received).toBe(frame);
    });
  });

  it("keeps an unterminated frame at the byte budget pending until LF arrives", async () => {
    const frame = jsonFrame(MAX_FRAME_BYTES, "😀");
    await withLocalSocket(async ({ connection, writeObserved }) => {
      let settled = false;
      const received = connection.recv();
      void received.then(() => { settled = true; }, () => { settled = true; });
      await writeObserved(Buffer.from(frame));
      expect(settled).toBe(false);
      expect(connection.closed).toBe(false);
      await writeObserved(Buffer.from("\n"));
      expect(await received).toBe(frame);
    });
  });

  it("rejects an unterminated frame as soon as one extra byte crosses the budget", async () => {
    await withLocalSocket(async ({ connection, client, peer, writeObserved }) => {
      const rejected = expect(connection.recv()).rejects.toThrow("dsh ipc: frame over budget");
      await writeObserved(Buffer.from(jsonFrame(MAX_FRAME_BYTES, "中")));
      expect(connection.closed).toBe(false);
      const closed = once(client, "close");
      peer.write("x"); // 不发 LF 或 EOF，超限本身必须让接收结束。
      await rejected;
      await closed;
      expect(connection.closed).toBe(true);
      await expect(connection.recv()).rejects.toThrow("dsh ipc: frame over budget");
    });
  });

  it.each(["", '{"partial":"未完成']) (
    "drains queued complete frames in order after EOF and never emits the trailing fragment %j",
    async (tail) => {
      await withLocalSocket(async ({ connection, client, peer, writeObserved }) => {
        const frames = ['{"id":1,"text":"中文"}', '{"id":2,"text":"😀"}'];
        // EOF 前不调 recv，确保覆盖已排队帧，而非已有 waiter 的分支。
        await writeObserved(Buffer.from(`${frames[0]}\n${frames[1]}\r\n${tail}`));
        const closed = once(client, "close");
        peer.end();
        await closed;
        expect(connection.closed).toBe(true);
        expect(await connection.recv()).toBe(frames[0]);
        expect(await connection.recv()).toBe(frames[1]);
        await expect(connection.recv()).rejects.toThrow("dsh ipc: connection closed");
      });
    },
  );

  it("rejects a waiting receiver at EOF without treating an unterminated fragment as a frame", async () => {
    await withLocalSocket(async ({ connection, client, peer, writeObserved }) => {
      const rejected = expect(connection.recv()).rejects.toThrow("dsh ipc: connection closed");
      await writeObserved(Buffer.from('{"partial":"合成数据"}'));
      const closed = once(client, "close");
      peer.end();
      await rejected;
      await closed;
      await expect(connection.recv()).rejects.toThrow("dsh ipc: connection closed");
    });
  });
});
