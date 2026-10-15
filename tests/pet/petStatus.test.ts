// tests/pet/petStatus.test.ts
import { describe, expect, it } from "vitest";
import type { Session } from "@/types/session";
import { computePetStatus, ackDone, cardsFromState } from "@/components/pet/petStatus";

const mk = (id: string, status: Session["status"], over: Partial<Session> = {}): Session => ({
  id,
  agentType: "claude",
  projectName: "P",
  projectPath: "/p",
  title: null,
  gitBranch: null,
  githubUrl: null,
  status,
  lastMessage: "msg",
  lastMessageRole: null,
  lastActivityAt: "",
  pid: 1,
  cpuUsage: 0,
  activeSubagentCount: 0,
  form: "cli",
  jumpSupported: true,
  ...over,
});

describe("computePetStatus", () => {
  it("灯色映射：waiting红 / 运行三态黄 / idle·finished绿（spec D2）", () => {
    const first = computePetStatus(
      [mk("a", "waiting"), mk("b", "thinking"), mk("c", "idle")],
      null,
      0
    );
    const lights = Object.fromEntries(first.cards.map((c) => [c.id, c.light]));
    expect(lights).toEqual({ a: "waiting", b: "running" }); // 绿无未读不显示卡
  });

  it("首帧不触发任何事件（spec §5）", () => {
    const first = computePetStatus([mk("a", "waiting"), mk("c", "idle")], null, 0);
    expect(first.events.newWaiting).toEqual([]);
    expect(first.events.newCompletion).toEqual([]);
  });

  it("完成差分：黄→绿 触发 newCompletion + 绿卡未读；稳态绿不触发", () => {
    const s1 = computePetStatus([mk("c", "thinking")], null, 0);
    const s2 = computePetStatus([mk("c", "idle")], s1.state, 1000);
    expect(s2.events.newCompletion).toEqual(["c"]);
    expect(s2.cards.find((x) => x.id === "c")).toMatchObject({
      light: "done",
      unread: true,
      lines: ["已完成"],
    });
    const s3 = computePetStatus([mk("c", "idle")], s2.state, 2000);
    expect(s3.events.newCompletion).toEqual([]);
    expect(s3.cards.find((x) => x.id === "c")?.unread).toBe(true); // 未读保留（C4）
  });

  it("waiting 差分触发 newWaiting；红卡持续显示", () => {
    const s1 = computePetStatus([mk("a", "thinking")], null, 0);
    const s2 = computePetStatus([mk("a", "waiting")], s1.state, 1000);
    expect(s2.events.newWaiting).toEqual(["a"]);
    const s3 = computePetStatus([mk("a", "waiting")], s2.state, 2000);
    expect(s3.events.newWaiting).toEqual([]);
    expect(s3.cards.find((x) => x.id === "a")?.light).toBe("waiting");
  });

  it("ackDone 后绿卡消失；再次完成重新亮起（C2/C4）", () => {
    const s1 = computePetStatus([mk("c", "thinking")], null, 0);
    const s2 = computePetStatus([mk("c", "idle")], s1.state, 1000);
    ackDone(s2.state, "c");
    expect(cardsFromState(s2.state).find((x) => x.id === "c")).toBeUndefined();
    const s3 = computePetStatus([mk("c", "thinking")], s2.state, 2000);
    const s4 = computePetStatus([mk("c", "idle")], s3.state, 3000);
    expect(s4.cards.find((x) => x.id === "c")?.unread).toBe(true);
  });

  it("会话消失：任何卡片立即消失，与看板一致（T1：不再保留未读绿卡 60s）", () => {
    // 红（等待操作）：终端关闭 → 下一轮立即消失
    const r1 = computePetStatus([mk("a", "waiting")], null, 0);
    const r2 = computePetStatus([], r1.state, 30_000);
    expect(r2.cards.find((x) => x.id === "a")).toBeUndefined();
    // 未读绿卡（完成未读）：同样消失即清——绿未读的持续可见由后端 unread pool
    // 保证（App 类会话一直在 payload 里），宠物本地不再做 TTL 保留
    const g1 = computePetStatus([mk("c", "thinking")], null, 0);
    const g2 = computePetStatus([mk("c", "idle")], g1.state, 1000);
    expect(g2.cards.find((x) => x.id === "c")?.light).toBe("done");
    const g3 = computePetStatus([], g2.state, 31_000);
    expect(g3.cards.find((x) => x.id === "c")).toBeUndefined();
  });

  it("排序 waiting>running>done，最多 6 张 + moreCount（H5/C3）", () => {
    const mkDone = (i: string) => {
      const a = computePetStatus([mk(i, "thinking")], null, 0);
      return computePetStatus([mk(i, "idle")], a.state, 10).state;
    };
    const state = { ...mkDone("d1"), ...mkDone("d2") };
    const sess = [
      mk("w", "waiting"),
      mk("r1", "processing"),
      mk("r2", "compacting"),
      ...["d1", "d2"].map((i) => mk(i, "idle")),
    ];
    const r = computePetStatus(sess, state, 100);
    expect(r.cards.slice(0, 3).map((c) => c.light)).toEqual(["waiting", "running", "running"]);
    expect(r.cards.length).toBeLessThanOrEqual(6);
    expect(r.cards.length + r.moreCount).toBe(5);
  });

  it("卡片题头与摘要：题头=工具名+项目+会话名（问题 3），lastMessage 截断（H3）", () => {
    const r = computePetStatus(
      [mk("a", "processing", { title: "自定义标题", lastMessage: "x".repeat(200) })],
      null,
      0
    );
    const card = r.cards[0];
    // 题头与看板 SessionCard 一致：agentLabel + projectName + 会话名
    expect(card.title).toBe("Claude    P    自定义标题");
    expect(card.lines[0].length).toBeLessThan(60);
    expect(card.lines[0].endsWith("…")).toBe(true);
  });

  it("无会话名时题头回退 id 前 8 位；codex 区分 APP/CLI 形态", () => {
    const r = computePetStatus(
      [mk("abcdefgh1234", "waiting", { agentType: "codex", form: "app" })],
      null,
      0
    );
    expect(r.cards[0].title).toBe("Codex APP    P    abcdefgh");
  });

  it("P2-6：首帧消费后端 s.unread 点亮恢复的未读卡；首帧仍零事件", () => {
    // 兔维斯 重启/宠物窗口重建后 prev=null：既存未读卡没有本地转绿差分可回放，
    // 只有消费 payload 的 unread 才能点亮（此前 first 强制 false → 通知静默盲区）
    const first = computePetStatus([mk("c", "idle", { unread: true, form: "app" })], null, 0);
    expect(first.cards.find((x) => x.id === "c")).toMatchObject({ light: "done", unread: true });
    // 首帧仍零事件：只点亮显示，不重放通知（F5 老卡静默语义）
    expect(first.events.newCompletion).toEqual([]);
    // 后端已读的绿卡首帧不点亮
    const read = computePetStatus([mk("d", "idle", { unread: false, form: "app" })], null, 0);
    expect(read.cards.find((x) => x.id === "d")).toBeUndefined();
    // 后续帧本地差分接管：持续亮（不依赖 payload 继续携带 unread）
    const second = computePetStatus(
      [mk("c", "idle", { unread: false, form: "app" })],
      first.state,
      1000
    );
    expect(second.cards.find((x) => x.id === "c")).toMatchObject({ light: "done", unread: true });
  });

  it("P2-6：已读置位后不被 s.unread 闪回（仅首帧消费后端未读）", () => {
    // 点击已读（ackDone）后、后端池删除落地的窗口期 payload 仍带 unread=true，
    // 本地差分必须优先——否则已消卡闪回
    const first = computePetStatus([mk("c", "idle", { unread: true, form: "app" })], null, 0);
    ackDone(first.state, "c");
    const second = computePetStatus(
      [mk("c", "idle", { unread: true, form: "app" })],
      first.state,
      1000
    );
    expect(second.cards.find((x) => x.id === "c")).toBeUndefined();
  });
});
