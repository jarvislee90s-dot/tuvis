/**
 * 本机授权 IPC 传输抽象（T06）。
 *
 * 可注入（[`IpcTransport`]）：测试用内存/假传输，生产用 Node 内置 `node:net`
 * （macOS/Linux = Unix domain socket 路径；Windows = 命名管道名——DSH Desktop
 * 的 Node 运行时两种地址形态都能直连）。帧协议与 Rust 侧一致：NDJSON，一行一帧。
 */

/** 单条连接（客户端视角；一行一帧 NDJSON）。 */
export interface IpcConnection {
  send(frame: string): Promise<void>;
  recv(): Promise<string>;
  close(): void;
  readonly closed: boolean;
}

/** 传输工厂：按 rendezvous 里的 address 建立连接。 */
export interface IpcTransport {
  connect(address: string): Promise<IpcConnection>;
}

/** 帧上限护栏（与 Rust local_ipc::MAX_FRAME_BYTES 一致）。 */
export const MAX_FRAME_BYTES = 1024 * 1024;

/** 生产传输：node:net 直连（UDS 路径或命名管道名；仅本机回环面）。 */
export class NodeIpcTransport implements IpcTransport {
  async connect(address: string): Promise<IpcConnection> {
    const net = await import("node:net");
    const socket = net.createConnection(address);
    type LineHandlers = { resolve: (line: string) => void; reject: (e: Error) => void };
    const queue: string[] = [];
    let waiter: LineHandlers | null = null;
    let failure: Error | null = null;
    let buffer = Buffer.alloc(0);
    const deliver = (): void => {
      if (!waiter) return;
      const line = queue.shift();
      if (line === undefined) return;
      const w = waiter;
      waiter = null;
      w.resolve(line);
    };
    socket.on("data", (chunk: Buffer) => {
      buffer = Buffer.concat([buffer, chunk]);
      for (;;) {
        const idx = buffer.indexOf(0x0a);
        if (idx < 0) break;
        const bytes = buffer.subarray(0, idx);
        buffer = buffer.subarray(idx + 1);
        if (bytes.length === 0) continue;
        if (bytes.length > MAX_FRAME_BYTES) {
          failure = new Error("dsh ipc: frame over budget");
          socket.destroy();
          return;
        }
        queue.push(bytes.toString("utf8").replace(/\r$/, ""));
      }
      if (buffer.length > MAX_FRAME_BYTES) {
        failure = new Error("dsh ipc: frame over budget");
        socket.destroy();
        return;
      }
      deliver();
    });
    socket.on("error", (err: Error) => {
      failure = err;
      const w = waiter;
      waiter = null;
      w?.reject(err);
    });
    socket.on("close", () => {
      const w = waiter;
      waiter = null;
      w?.reject(failure ?? new Error("dsh ipc: connection closed"));
    });
    await new Promise<void>((resolve, reject) => {
      socket.once("connect", resolve);
      socket.once("error", reject);
    });
    return {
      async send(frame: string): Promise<void> {
        if (socket.destroyed) throw new Error("dsh ipc: connection closed");
        await new Promise<void>((resolve, reject) => {
          socket.write(`${frame}\n`, (err) => (err ? reject(err) : resolve()));
        });
      },
      async recv(): Promise<string> {
        const queued = queue.shift();
        if (queued !== undefined) return queued;
        if (failure) throw failure;
        if (socket.destroyed) throw new Error("dsh ipc: connection closed");
        return await new Promise<string>((resolve, reject) => {
          waiter = { resolve, reject };
        });
      },
      close(): void {
        socket.destroy();
      },
      get closed(): boolean {
        return socket.destroyed;
      },
    };
  }
}
