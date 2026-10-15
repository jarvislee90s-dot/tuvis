// M3+ 消息窗口书签（2026-09-16 用户裁决）：模块级内存单例 + 内容指纹锚点。
//
// 为什么不用 seq 作锚：后端 finalize（content.rs）把 seq 赋为「返回数组内下标」——
// 点「加载更早消息」（limit 200→400）或刷新后同一条消息 seq 会整体位移，
// 书签会指向错行。改用内容指纹（kind|ts|长度|前 120 字符），跨重拉稳定。
//
// 生命周期（用户裁决）：内存态——关 兔维斯 消失；出会话窗口再切回来保留
// （SessionDetail 是条件挂载，state 会被丢弃，故存模块级单例）。
import { beforeEach, afterEach, describe, expect, it, vi } from "vitest";
import {
  BOOKMARK_COLORS,
  BOOKMARK_LIMIT,
  addBookmark,
  clearBookmarks,
  listBookmarks,
  messageAnchor,
  removeBookmark,
  type Bookmark,
} from "@/mobile/bookmarks";

/** 造书签（默认用第一色） */
function bm(over: Partial<Bookmark> = {}): Bookmark {
  return {
    color: BOOKMARK_COLORS[0],
    seq: 1,
    anchor: "user|1000|3|abc",
    preview: "abc",
    ...over,
  };
}

describe("messageAnchor 内容指纹", () => {
  it("同一条消息在 seq 变化前后指纹不变（跨 limit 重拉稳定的关键）", () => {
    const msg = { kind: "user", ts: 1000, content: "帮我改一下这段代码" };
    const a1 = messageAnchor(msg);
    // 模拟重拉后 seq 位移（指纹入参不含 seq，故不受影响）
    const a2 = messageAnchor({ ...msg });
    expect(a1).toBe(a2);
    expect(a1).toContain("user");
    expect(a1).toContain("1000");
  });

  it("内容不同的消息指纹不同", () => {
    const a = messageAnchor({ kind: "user", ts: 1000, content: "第一条" });
    const b = messageAnchor({ kind: "user", ts: 1001, content: "第一条" });
    const c = messageAnchor({ kind: "assistant", ts: 1000, content: "第一条" });
    const d = messageAnchor({ kind: "user", ts: 1000, content: "第二条" });
    expect(new Set([a, b, c, d]).size).toBe(4);
  });

  it("ts 为 null 不产生 undefined 串（占位为空）", () => {
    const a = messageAnchor({ kind: "tool-call", ts: null, content: "调用 Bash" });
    expect(a).not.toContain("undefined");
    expect(a).toContain("tool-call");
  });

  it("超长内容只取前 120 字符：同长度同前缀同指纹（截断生效的直接证据）", () => {
    // 两条长度相同、前 120 字符相同、仅尾部不同 → 指纹相同（证明只看前 120）
    const a = "x".repeat(120) + "A".repeat(380);
    const b = "x".repeat(120) + "B".repeat(380);
    expect(a.length).toBe(b.length);
    expect(messageAnchor({ kind: "user", ts: 1, content: a })).toBe(
      messageAnchor({ kind: "user", ts: 1, content: b })
    );
    // 前 120 字符不同 → 指纹不同
    const c = "y".repeat(120) + "A".repeat(380);
    expect(messageAnchor({ kind: "user", ts: 1, content: a })).not.toBe(
      messageAnchor({ kind: "user", ts: 1, content: c })
    );
    // 长度参与指纹：前缀相同但总长不同 → 不同
    const short = "x".repeat(120) + "A".repeat(100);
    expect(messageAnchor({ kind: "user", ts: 1, content: a })).not.toBe(
      messageAnchor({ kind: "user", ts: 1, content: short })
    );
  });
});

describe("书签 store（内存单例）", () => {
  beforeEach(() => {
    // 用例间隔离：清掉两个用到的会话 + localStorage 镜像 + bootId 缓存
    clearBookmarks("s1");
    clearBookmarks("s2");
    window.localStorage.removeItem("mam-bookmarks");
    window.localStorage.removeItem("tuvis-theme");
  });

  it("调色板 10 色、上限 10 个（颜色即唯一键）", () => {
    expect(BOOKMARK_COLORS).toHaveLength(10);
    expect(BOOKMARK_LIMIT).toBe(10);
    expect(new Set(BOOKMARK_COLORS).size).toBe(10); // 无重复色
  });

  it("增查：按 sessionId 隔离，listBookmarks 返回同一会话的表", () => {
    addBookmark("s1", bm({ color: BOOKMARK_COLORS[0] }));
    addBookmark("s2", bm({ color: BOOKMARK_COLORS[1], anchor: "other" }));
    expect(listBookmarks("s1")).toHaveLength(1);
    expect(listBookmarks("s2")).toHaveLength(1);
    expect(listBookmarks("s1")[0].color).toBe(BOOKMARK_COLORS[0]);
    // 未打过的会话 → 空表（不抛）
    expect(listBookmarks("never")).toEqual([]);
  });

  it("每色至多一个：同色重复加入被拒绝（颜色即唯一键）", () => {
    addBookmark("s1", bm({ color: BOOKMARK_COLORS[0], anchor: "a1" }));
    addBookmark("s1", bm({ color: BOOKMARK_COLORS[0], anchor: "a2" }));
    const list = listBookmarks("s1");
    expect(list).toHaveLength(1);
    expect(list[0].anchor).toBe("a1"); // 首次保留，重复无效
  });

  it("达到 10 个后再加被拒绝（不静默替换）", () => {
    for (let i = 0; i < 10; i += 1) {
      addBookmark("s1", bm({ color: BOOKMARK_COLORS[i], anchor: `a${i}` }));
    }
    expect(listBookmarks("s1")).toHaveLength(10);
    // 第 11 个（新色不存在，用已用色验证拒绝路径）——上限本身已达，任何加入都无效
    addBookmark("s1", bm({ color: "#000000", anchor: "overflow" }));
    expect(listBookmarks("s1")).toHaveLength(10);
    expect(listBookmarks("s1").some((b) => b.anchor === "overflow")).toBe(false);
  });

  it("删单个：按颜色删；删不存在的颜色是 no-op", () => {
    addBookmark("s1", bm({ color: BOOKMARK_COLORS[0] }));
    addBookmark("s1", bm({ color: BOOKMARK_COLORS[1], anchor: "a2" }));
    removeBookmark("s1", BOOKMARK_COLORS[0]);
    const list = listBookmarks("s1");
    expect(list).toHaveLength(1);
    expect(list[0].color).toBe(BOOKMARK_COLORS[1]);
    removeBookmark("s1", "#not-there"); // no-op
    expect(listBookmarks("s1")).toHaveLength(1);
  });

  it("清空该会话全部标签；不影响其它会话", () => {
    addBookmark("s1", bm({ color: BOOKMARK_COLORS[0] }));
    addBookmark("s2", bm({ color: BOOKMARK_COLORS[0] }));
    clearBookmarks("s1");
    expect(listBookmarks("s1")).toEqual([]);
    expect(listBookmarks("s2")).toHaveLength(1);
  });

  it("listBookmarks 返回副本（外部改写不污染单例）", () => {
    addBookmark("s1", bm({ color: BOOKMARK_COLORS[0] }));
    const got = listBookmarks("s1");
    got.push(bm({ color: BOOKMARK_COLORS[5] }));
    expect(listBookmarks("s1")).toHaveLength(1);
  });

  it("跨「组件卸载」保留：单例不随任何组件生命周期变化（模块级语义）", () => {
    // 「卸载」在本模块无表示——只需验证同进程内重复读写稳定（SessionDetail
    // 卸载重挂后正是靠这一点恢复）
    addBookmark("s1", bm({ color: BOOKMARK_COLORS[2] }));
    expect(listBookmarks("s1")).toHaveLength(1);
    expect(listBookmarks("s1")).toHaveLength(1);
  });
});

// 刷新恢复（2026-09-16 用户裁决补充）：刷新网页销毁 JS 上下文（内存单例清空），
// localStorage 镜像 + bootId 守卫负责恢复——同一 兔维斯 进程恢复、兔维斯 重启清空。
// 每个用例用 vi.resetModules + 动态 import 模拟「全新页面」（内存单例是模块级，
// 静态导入会跨用例残留 activeBootId / store，无法模拟刷新）
describe("书签刷新恢复（localStorage 镜像 + bootId 守卫）", () => {
  beforeEach(() => {
    window.localStorage.removeItem("mam-bookmarks");
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes("/host")) {
          return new Response(
            JSON.stringify({
              host: { name: "n", platform: "windows", version: "0", bootId: hostBootId },
              enabledTools: [],
            }),
            { status: 200 }
          );
        }
        throw new Error("unexpected fetch: " + url);
      })
    );
  });
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  let hostBootId = "boot-a";
  /** 模拟一次「新开页面」：模块注册表重置后动态导入 → 全新内存单例 */
  async function freshModule() {
    vi.resetModules();
    return await import("@/mobile/bookmarks");
  }

  it("打书签后镜像进 localStorage（同 bootId 可恢复）", async () => {
    const mod = await freshModule();
    await mod.ensureBootId(); // bootId = boot-a
    mod.restoreBookmarks("boot-a");
    mod.addBookmark("s1", bm({ color: BOOKMARK_COLORS[0] }));
    const raw = window.localStorage.getItem("mam-bookmarks");
    expect(raw).toBeTruthy();
    expect(raw).toContain('"bootId":"boot-a"');
    expect(raw).toContain('"s1"');
  });

  it("同 bootId 再恢复（模拟刷新页面）→ 书签回来", async () => {
    const mod1 = await freshModule();
    await mod1.ensureBootId();
    mod1.restoreBookmarks("boot-a");
    mod1.addBookmark("s1", bm({ color: BOOKMARK_COLORS[0] }));

    // 「刷新」：全新模块实例（内存单例为空）+ 同一 localStorage + 同一 bootId
    const mod2 = await freshModule();
    await mod2.ensureBootId();
    mod2.restoreBookmarks("boot-a");
    expect(mod2.listBookmarks("s1")).toHaveLength(1);
    expect(mod2.listBookmarks("s1")[0].anchor).toBe("user|1000|3|abc");
  });

  it("bootId 变化（模拟 兔维斯 重启）→ 书签清空", async () => {
    const mod1 = await freshModule();
    await mod1.ensureBootId();
    mod1.restoreBookmarks("boot-a");
    mod1.addBookmark("s1", bm({ color: BOOKMARK_COLORS[0] }));

    hostBootId = "boot-b"; // 兔维斯 重启：新进程新 bootId
    const mod2 = await freshModule();
    await mod2.ensureBootId();
    mod2.restoreBookmarks("boot-b");
    expect(mod2.listBookmarks("s1")).toEqual([]);
    // 存储也被重写为新 bootId 的空表
    expect(window.localStorage.getItem("mam-bookmarks")).toContain('"bootId":"boot-b"');
  });

  it("host 拉不到（离线）→ ensureBootId 返回 null，书签保持纯内存态", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new TypeError("network down");
      })
    );
    const mod = await freshModule();
    expect(await mod.ensureBootId()).toBeNull();
    // 内存态仍可用（不持久化，但不崩溃）
    mod.addBookmark("s1", bm({ color: BOOKMARK_COLORS[0] }));
    expect(mod.listBookmarks("s1")).toHaveLength(1);
    expect(window.localStorage.getItem("mam-bookmarks")).toBeNull();
  });
});
