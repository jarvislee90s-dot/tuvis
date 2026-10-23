// 文件传输进度（Task 10 §C5）：下行流式读进度 + 上行 XHR 进度 + 通道装饰端点。
// fetch 全量 stub（盖过 setup.ts 的 msw）——transfer.test 与 api.test.ts 同款纪律。
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { fetchFile, uploadAttachment } from "@/mobile/api";
import FilePanel from "@/mobile/FilePanel";
import FilePreview from "@/mobile/FilePreview";
import { etaRemainingText, formatBytes, transferRateBps } from "@/mobile/board-logic";
import type { SessionFileEntry } from "@/mobile/api";

// 文案走 mobile.* 双语键后按 zh 断言（Board.test 同款：显式引入并固定中文）。
// 只 changeLanguage 不够：jsdom 默认 en-US 会在语言重评估时翻回 en——浏览器
// 语言一并钉到 zh（language-toggle 同款）
import { pinZhNavigator } from "./pinZhNavigator";

let unpinZh: () => void;

beforeAll(async () => {
  unpinZh = await pinZhNavigator();
});

afterAll(() => {
  unpinZh();
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

/** jsdom 无 createObjectURL（图片分支落 object URL）——与 FilePreview.test.tsx 同款桩 */
Object.defineProperty(URL, "createObjectURL", {
  value: () => "blob:mock-url",
  configurable: true,
  writable: true,
});
Object.defineProperty(URL, "revokeObjectURL", {
  value: () => {},
  configurable: true,
  writable: true,
});

describe("下行进度：fetchFile 流式读（§C5）", () => {
  it("下行按累计字节回调进度（分块流）", async () => {
    const chunks = [new Uint8Array(100), new Uint8Array(150)];
    const fake = {
      ok: true,
      headers: new Headers({ "content-type": "image/png", "content-length": "250" }),
      body: new ReadableStream({
        start(c) {
          chunks.forEach((x) => c.enqueue(x));
          c.close();
        },
      }),
    };
    const seen: Array<[number, number | null]> = [];
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(fake));
    await fetchFile("s1", "/tmp/a.png", (loaded, total) => seen.push([loaded, total]));
    expect(seen).toEqual([
      [100, 250],
      [250, 250],
    ]);
  });

  it("content-length 缺失（隧道不透传）→ total 回调 null（UI 不编假百分比）", async () => {
    const fake = {
      ok: true,
      headers: new Headers({ "content-type": "image/png" }),
      body: new ReadableStream({
        start(c) {
          c.enqueue(new Uint8Array(64));
          c.close();
        },
      }),
    };
    const seen: Array<[number, number | null]> = [];
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(fake));
    await fetchFile("s1", "/tmp/a.png", (loaded, total) => seen.push([loaded, total]));
    expect(seen).toEqual([[64, null]]);
  });

  /** FilePreview 渲染（makeSession 与 FilePreview.test.tsx 同款最小夹具） */
  function renderPreview(filePath: string) {
    const session = {
      id: "sess-1",
      agentType: "claude",
      projectName: "proj",
      projectPath: "/tmp/proj",
      title: null,
      gitBranch: null,
      githubUrl: null,
      status: "processing",
      lastMessage: null,
      lastMessageRole: null,
      lastActivityAt: "2026-09-15T00:00:00Z",
      pid: 1,
      cpuUsage: 0,
      activeSubagentCount: 0,
      form: "cli",
      jumpSupported: false,
      unread: false,
    } as SessionT;
    render(
      <FilePreview session={session} filePath={filePath} mode="fullscreen" onClose={() => {}} />
    );
  }
  type SessionT = import("@/types/session").Session;

  it("FilePreview 加载态显示已传字节 + 百分比（下行进度 UI 落点）", async () => {
    // 分块流 + 门闩：第一块到达后停在 loading（进度可见），放行后完成。
    // pull 是状态机：两块发完即 close——不 close 会被再次 pull 无限 enqueue（OOM 实锤）
    let release: (() => void) | null = null;
    const gate = new Promise<void>((r) => {
      release = r;
    });
    let sent = 0;
    const fake = {
      ok: true,
      headers: new Headers({ "content-type": "image/png", "content-length": "250" }),
      body: new ReadableStream({
        async pull(c) {
          if (sent === 0) {
            sent = 1;
            c.enqueue(new Uint8Array(100));
          } else if (sent === 1) {
            sent = 2;
            await gate;
            c.enqueue(new Uint8Array(150));
          } else {
            c.close();
          }
        },
      }),
    };
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(fake));
    renderPreview("/tmp/pic.png");
    // 第一块已计：进度文本出现（100 B / 250 B · 40%）
    const text = await screen.findByTestId("preview-progress-text");
    expect(text.textContent).toContain("100 B");
    expect(text.textContent).toContain("250 B");
    expect(text.textContent).toContain("40%");
    // 放行 → 完成 → 进度让位图片
    release!();
    await screen.findByTestId("preview-image");
  });
});

describe("进度纯函数（board-logic，上下行进度 UI 共用）", () => {
  it("formatBytes：B/KB/MB 分档", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(1024)).toBe("1.0 KB");
    expect(formatBytes(6.4 * 1024 * 1024)).toBe("6.4 MB");
  });

  it("transferRateBps：跨度不足 / 零前进 → null（不编假速率）", () => {
    expect(transferRateBps({ loaded: 0, atMs: 0 }, { loaded: 100, atMs: 100 })).toBeNull();
    expect(transferRateBps({ loaded: 100, atMs: 0 }, { loaded: 100, atMs: 5000 })).toBeNull();
    expect(transferRateBps({ loaded: 100, atMs: 0 }, { loaded: 1100, atMs: 1000 })).toBe(1000);
  });

  it("etaRemainingText：total/rate 未知 → null；<90s 按秒，否则按分钟", () => {
    expect(etaRemainingText(100, null, 1000)).toBeNull();
    expect(etaRemainingText(100, 1100, null)).toBeNull();
    expect(etaRemainingText(100, 1100, 1000)).toBe("预计剩余 ~1 秒");
    expect(etaRemainingText(0, 300 * 1024 * 1024, 1024 * 1024)).toBe("预计剩余 ~5 分钟");
  });
});

/** FilePanel 渲染助手（FilePanel.test.tsx 同款回调 mock；mode/onModeChange/onClose 等顶栏收编 props 已删） */
function renderPanel(entries: SessionFileEntry[]) {
  render(
    <FilePanel
      entries={entries}
      truncated={false}
      scope={200}
      loading={false}
      onScopeChange={() => {}}
      onOpenFile={() => {}}
    />
  );
}

function entry(path: string): SessionFileEntry {
  return { path, lastSeq: 1, lastTs: 1_700_000_000_000, hits: 1, modified: true };
}

describe("带宽受限提示 + 满速升级指引（§C5，装饰不进安全判定）", () => {
  it("limited=true → 面板底部出现带宽提示（是通道限制不是故障 + Tailscale 升级指引）", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({ via: "quick", limited: true, est_mbps_down: 1.8, est_mbps_up: 0.8 }),
            {
              status: 200,
            }
          )
      )
    );
    renderPanel([entry("/p/a.rs")]);
    const note = await screen.findByTestId("panel-bw-note");
    expect(note.textContent).toContain("通道限制");
    expect(note.textContent).toContain("不是故障");
    expect(note.textContent).toContain("Tailscale");
    // 按实测速率预估耗时（20 MB 参考：下行 20*8/1.8/60≈1.5 分钟 / 上行 ≈3.3 分钟）
    expect(note.textContent).toContain("1.5");
    expect(note.textContent).toContain("3.3");
  });

  it("limited=false（局域网直连）→ 不出现提示", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({ via: "lan", limited: false, est_mbps_down: 0, est_mbps_up: 0 }),
            {
              status: 200,
            }
          )
      )
    );
    renderPanel([entry("/p/a.rs")]);
    await screen.findByTestId("panel-preview-help"); // 挂载拉取已落定
    expect(screen.queryByTestId("panel-bw-note")).toBeNull();
  });

  it("畸形载荷（limited=true 但 est=0）→ 不出现提示（不渲染 Infinity 分钟）", async () => {
    // 畸形/未测得速率的载荷：limited 仍真但 est 全零——按原式 (20*8)/0/60 会得出
    // 「Infinity 分钟」；守卫口径：est 任一 ≤0 一律不渲染横幅（无数据不如不提示）
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({ via: "quick", limited: true, est_mbps_down: 0, est_mbps_up: 0 }),
            {
              status: 200,
            }
          )
      )
    );
    renderPanel([entry("/p/a.rs")]);
    await screen.findByTestId("panel-preview-help"); // 挂载拉取已落定
    expect(screen.queryByTestId("panel-bw-note")).toBeNull();
  });

  it("拉取失败（网络异常 / 403 设备失效）→ 静默不渲染（装饰能力不阻塞面板）", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response("", { status: 403 }))
    );
    renderPanel([entry("/p/a.rs")]);
    await screen.findByTestId("panel-preview-help");
    expect(screen.queryByTestId("panel-bw-note")).toBeNull();
  });
});

describe("上行进度：uploadAttachment 换 XHR（§C5）", () => {
  it("上传进度回调必须在 send() 之前注册（否则收不到事件）", async () => {
    const order: string[] = [];
    class FakeXHR {
      upload = { onprogress: null as null | ((e: ProgressEvent) => void) };
      onload: ((e: ProgressEvent) => void) | null = null;
      open() {
        order.push("open");
      }
      send() {
        order.push("send");
        this.upload.onprogress?.(new ProgressEvent("progress", { loaded: 5, total: 10 }));
        // 简报夹具不结算 promise 会令任何正确实现都挂起——补一发 load 结束
        this.onload?.(new ProgressEvent("load"));
      }
      setRequestHeader() {}
      get status() {
        return 200;
      }
      get responseText() {
        return "{}";
      }
    }
    vi.stubGlobal("XMLHttpRequest", FakeXHR as unknown as typeof XMLHttpRequest);
    const seen: number[] = [];
    // ⚠️ onProgress 是**第 4 参**——第 3 参是既有签名里的 signal（传 undefined 占位）。
    // 真实签名：uploadAttachment(sessionId, file, signal?, onProgress?)（api.ts:426-430）
    await uploadAttachment("s1", new File([new Uint8Array(10)], "a.bin"), undefined, (loaded) => {
      order.push("progress");
      seen.push(loaded);
    });
    expect(seen).toEqual([5]);
    // 「注册早于 send」的编码修正（简报断言字面恒假，报告已披露）：progress 事件在
    // send() 内部、"send" 入栈**之后**同步触发 → indexOf("progress") 恒等于
    // indexOf("send")+1，`< indexOf("send")+1` 永不成立。改用组合表达同一意图：
    // ① seen=[5] —— 若 onprogress 注册晚于 send()，假体的 upload.onprogress 在
    //    事件触发时仍是 null，事件被丢弃、seen 为空（这是「注册早于 send」的硬证据）；
    // ② progress 在 send 之后入栈 —— 事件确实在 send 期间到达（而非事后补发）。
    expect(order.indexOf("progress")).toBeGreaterThan(order.indexOf("send"));
  });
});
