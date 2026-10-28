# Foxbell 桌宠移植实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 DSH 插件 `dsh-foxbell-pet` v1.3.0 完整移植为 MAM 的独立置顶桌宠窗口（动画/物理/卡片/语音/菜单全套）。

**Architecture:** 前端驱动——`#/pet` 独立 webview 窗口自己轮询会话并做灯色差分；Rust 仅提供建窗与显隐/置顶 command；主窗口 `useNotification` 按宠物状态让渡声音与浮窗。规格见 `docs/engineering/specs/2026-09-01-foxbell-pet-design.md`（下称 spec）。

**Tech Stack:** Tauri 2（已开 macOSPrivateApi）、React 19、TypeScript、vitest（`tests/` 目录 + jsdom + msw）、i18next（en/zh，`scripts/check-i18n.mjs` 校验键对齐）。

## Global Constraints

- 设计文档/注释中文，标识符英文；commit message 英文（conventional commits）。
- 不新增任何 npm / crate 依赖。
- 灯色语义（spec D2）：`waiting`→🔴红；`processing/thinking/compacting`→🟡黄；`idle/finished`→🟢绿（完成=非绿→绿差分）。断联/报错不做（D9）。
- localStorage 键（spec §10.1）：`mam-pet-config` / `mam-pet-visible` / `mam-pet-position`。
- 动作枚举：`jumping|waving|failed|waiting|review|running`；缩放三档 `0.75|1|1.25`，所有像素常量 `px(v)=v×scale` 等比例（D15）。
- 语音字幕=文件名；`muted` 只拦声音不拦动作；`talkative` 只拦字幕（D5）。
- 菜单绑定场景只有三个：双击 / 红灯(waiting) / 绿灯(done)；`errorAction` 键保留但不出现在菜单（D14）。
- 每个任务收尾必须跑：该任务测试 + `pnpm lint`（改动 ts/tsx 时）；Rust 任务跑 `cd src-tauri && cargo check`。
- 浏览器预览兼容：所有 `@tauri-apps/api/window` 调用必须 try/catch 静默降级（tauri-mock 环境部分 API 缺失），保证 `#/pet` 能在纯浏览器渲染。
- 原插件源码（移植参照，勿改动）：`/Users/jarvis/Documents/DeepSeek/DeepSeek-plugins/dsh-foxbell-pet/src/client.js`（906 行）与 `src/index.js`。

---

### Task 1: 素材搬运与 manifest 生成

**Files:**
- Create: `scripts/copy-pet-assets.mjs`
- Create: `public/pet/spritesheet.webp`、`public/pet/voice/**`（生成物）
- Test: `tests/pet/assets.test.ts`

**Interfaces:**
- Produces: `public/pet/manifest.json`，结构 `[{ "index": number, "group": "general"|"approval"|"done"|"error", "name": string, "file": string }]`，`file` 相对 `public/pet/voice/`（如 `"done/搞定咯.m4a"`）。Task 5 的 `parseManifest` 消费此结构。

- [x] **Step 1: 写素材校验测试（先失败）**

```ts
// tests/pet/assets.test.ts — 校验 manifest 与素材文件齐全（spec §6.1）
import { readFileSync, existsSync } from "node:fs";
import { resolve } from "node:path";

const ROOT = resolve(__dirname, "../../public/pet");

describe("pet assets", () => {
  const manifest = JSON.parse(readFileSync(resolve(ROOT, "manifest.json"), "utf-8")) as {
    index: number;
    group: string;
    name: string;
    file: string;
  }[];

  it("精灵图存在", () => {
    expect(existsSync(resolve(ROOT, "spritesheet.webp"))).toBe(true);
  });

  it("manifest 含四个组且索引连续", () => {
    const groups = new Set(manifest.map((v) => v.group));
    expect(groups).toEqual(new Set(["general", "approval", "done", "error"]));
    expect(manifest.map((v) => v.index)).toEqual(manifest.map((_, i) => i));
  });

  it("每个语音文件真实存在且文件名去扩展即 name", () => {
    for (const v of manifest) {
      const p = resolve(ROOT, "voice", v.file);
      expect(existsSync(p)).toBe(true);
      expect(v.name).toBe(v.file.replace(/\.(m4a|mp4)$/i, "").split("/").pop());
    }
  });
});
```

- [x] **Step 2: 运行确认失败**

Run: `pnpm vitest run tests/pet/assets.test.ts`
Expected: FAIL（manifest.json 不存在）

- [x] **Step 3: 写搬运脚本**

```js
// scripts/copy-pet-assets.mjs — 从原插件仓库搬运素材并生成 manifest（一次性，spec §6.1）
// 用法: node scripts/copy-pet-assets.mjs <源assets目录>
import { cpSync, mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { join, relative, resolve } from "node:path";

const SRC = resolve(process.argv[2] ?? "");
const DEST = resolve("public/pet");
const GROUPS = ["general", "approval", "done", "error"]; // spec §6.1 映射表顺序

if (!statSync(join(SRC, "spritesheet.webp")).isFile()) {
  throw new Error(`源目录无效: ${SRC}`);
}
mkdirSync(DEST, { recursive: true });
cpSync(join(SRC, "spritesheet.webp"), join(DEST, "spritesheet.webp"));

const manifest = [];
let index = 0;
for (const group of GROUPS) {
  const dir = join(SRC, "voice", group);
  const files = readdirSync(dir)
    .filter((f) => /\.(m4a|mp4)$/i.test(f))
    .sort((a, b) => a.localeCompare(b, "zh"));
  for (const f of files) {
    cpSync(join(dir, f), join(DEST, "voice", group, f));
    manifest.push({ index: index++, group, name: f.replace(/\.(m4a|mp4)$/i, ""), file: `${group}/${f}` });
  }
}
writeFileSync(join(DEST, "manifest.json"), JSON.stringify(manifest, null, 2) + "\n");
console.log(`copied ${manifest.length} voices + spritesheet -> ${relative(".", DEST)}`);
```

- [x] **Step 4: 执行搬运并验证**

Run: `node scripts/copy-pet-assets.mjs /Users/jarvis/Documents/DeepSeek/DeepSeek-plugins/dsh-foxbell-pet/assets`
Expected: `copied 31 voices + spritesheet -> public/pet`

- [x] **Step 5: 运行测试确认通过**

Run: `pnpm vitest run tests/pet/assets.test.ts`
Expected: PASS（4 个用例）

- [x] **Step 6: Commit**

```bash
git add scripts/copy-pet-assets.mjs public/pet tests/pet/assets.test.ts
git commit -m "feat(pet): vendor foxbell spritesheet and voice assets with manifest"
```

---

### Task 2: petConfig — 配置 store

**Files:**
- Create: `src/components/pet/petConfig.ts`
- Test: `tests/pet/petConfig.test.ts`

**Interfaces:**
- Produces（后续任务全部依赖）:
  - 类型 `PetAction`、`PetScale`（`0.75|1|1.25`）、`PetConfig`（8 字段见下）
  - `loadConfig(): PetConfig` / `saveConfig(patch: Partial<PetConfig>): void`
  - `subscribeConfig(fn: () => void): () => void`（本窗口 set 后立即回调 + 跨窗口 storage 事件）
  - `loadVisible(): boolean`（默认 `false`）/ `saveVisible(v: boolean): void`
  - `loadPosition(): { x: number; y: number } | null` / `savePosition(p): void`
  - `petSoundTakeover(): boolean`（=宠物开启，声音接管判定）
  - `petSuppressPopup(): boolean`（=宠物开启且置顶，浮窗抑制判定）
  - 常量 `PET_ACTIONS: PetAction[]`、`PET_SCALES: PetScale[]`

- [x] **Step 1: 写失败测试**

```ts
// tests/pet/petConfig.test.ts
import { beforeEach, describe, expect, it } from "vitest";
import {
  loadConfig, saveConfig, loadVisible, saveVisible,
  loadPosition, savePosition, petSoundTakeover, petSuppressPopup,
} from "@/components/pet/petConfig";

describe("petConfig", () => {
  beforeEach(() => localStorage.clear());

  it("无存储时返回默认值（spec §10.1）", () => {
    const c = loadConfig();
    expect(c).toMatchObject({
      alwaysOnTop: true, muted: false, talkative: true, gravity: true, scale: 1,
      dblAction: "waving", approvalAction: "waiting", errorAction: "failed", doneAction: "jumping",
    });
    expect(loadVisible()).toBe(false);
    expect(loadPosition()).toBeNull();
  });

  it("非法值回落默认（sanitize）", () => {
    localStorage.setItem("mam-pet-config", JSON.stringify({ scale: 9, dblAction: "hack", muted: "yes" }));
    const c = loadConfig();
    expect(c.scale).toBe(1);
    expect(c.dblAction).toBe("waving");
    expect(c.muted).toBe(false);
  });

  it("patch 保存与读取回环", () => {
    saveConfig({ scale: 1.25, doneAction: "review" });
    expect(loadConfig().scale).toBe(1.25);
    expect(loadConfig().doneAction).toBe("review");
  });

  it("visible / position 回环", () => {
    saveVisible(true);
    expect(loadVisible()).toBe(true);
    savePosition({ x: 100.6, y: -200 });
    expect(loadPosition()).toEqual({ x: 101, y: -200 });
  });

  it("接管判定：开启即接管声音；置顶才抑制浮窗（spec D3/D4）", () => {
    saveVisible(false);
    expect(petSoundTakeover()).toBe(false);
    saveVisible(true);
    expect(petSoundTakeover()).toBe(true);
    saveConfig({ alwaysOnTop: false });
    expect(petSuppressPopup()).toBe(false);
    saveConfig({ alwaysOnTop: true });
    expect(petSuppressPopup()).toBe(true);
  });
});
```

- [x] **Step 2: 运行确认失败**

Run: `pnpm vitest run tests/pet/petConfig.test.ts`
Expected: FAIL（模块不存在）

- [x] **Step 3: 实现**

```ts
// 桌宠配置 — localStorage 单后端，跨窗口 storage 事件同步（spec §10）
export type PetAction = "jumping" | "waving" | "failed" | "waiting" | "review" | "running";
export type PetScale = 0.75 | 1 | 1.25;

export interface PetConfig {
  alwaysOnTop: boolean;
  muted: boolean;
  talkative: boolean;
  gravity: boolean;
  scale: PetScale;
  dblAction: PetAction;
  approvalAction: PetAction;
  errorAction: PetAction; // 占位：v1 无触发场景（spec D9/D14）
  doneAction: PetAction;
}

export const CONFIG_KEY = "mam-pet-config";
export const VISIBLE_KEY = "mam-pet-visible";
export const POSITION_KEY = "mam-pet-position";

export const PET_ACTIONS: PetAction[] = ["jumping", "waving", "failed", "waiting", "review", "running"];
export const PET_SCALES: PetScale[] = [0.75, 1, 1.25];

const DEFAULT_CONFIG: PetConfig = {
  alwaysOnTop: true, muted: false, talkative: true, gravity: true, scale: 1,
  dblAction: "waving", approvalAction: "waiting", errorAction: "failed", doneAction: "jumping",
};

const ACTION_KEYS = ["dblAction", "approvalAction", "errorAction", "doneAction"] as const;
const BOOL_KEYS = ["alwaysOnTop", "muted", "talkative", "gravity"] as const;

function sanitize(raw: unknown): PetConfig {
  const out = { ...DEFAULT_CONFIG };
  if (raw && typeof raw === "object") {
    const p = raw as Record<string, unknown>;
    for (const k of BOOL_KEYS) if (typeof p[k] === "boolean") out[k] = p[k] as boolean;
    if (PET_SCALES.includes(p.scale as PetScale)) out.scale = p.scale as PetScale;
    for (const k of ACTION_KEYS) if (PET_ACTIONS.includes(p[k] as PetAction)) out[k] = p[k] as PetAction;
  }
  return out;
}

export function loadConfig(): PetConfig {
  try {
    const raw = localStorage.getItem(CONFIG_KEY);
    return raw ? sanitize(JSON.parse(raw)) : { ...DEFAULT_CONFIG };
  } catch {
    return { ...DEFAULT_CONFIG };
  }
}

const listeners = new Set<() => void>();
const emit = () => listeners.forEach((fn) => fn());

export function saveConfig(patch: Partial<PetConfig>): void {
  localStorage.setItem(CONFIG_KEY, JSON.stringify(sanitize({ ...loadConfig(), ...patch })));
  emit();
}

export function subscribeConfig(fn: () => void): () => void {
  listeners.add(fn);
  // storage 事件只在"其它窗口"修改时触发；本窗口修改靠 emit
  const onStorage = (e: StorageEvent) => {
    if (e.key === null || e.key === CONFIG_KEY || e.key === VISIBLE_KEY) fn();
  };
  window.addEventListener("storage", onStorage);
  return () => {
    listeners.delete(fn);
    window.removeEventListener("storage", onStorage);
  };
}

export function loadVisible(): boolean {
  try {
    return localStorage.getItem(VISIBLE_KEY) === "1";
  } catch {
    return false;
  }
}

export function saveVisible(v: boolean): void {
  localStorage.setItem(VISIBLE_KEY, v ? "1" : "0");
  emit();
}

export interface PetPosition { x: number; y: number }

export function loadPosition(): PetPosition | null {
  try {
    const raw = localStorage.getItem(POSITION_KEY);
    if (!raw) return null;
    const p = JSON.parse(raw);
    if (Number.isFinite(p?.x) && Number.isFinite(p?.y)) return { x: p.x, y: p.y };
  } catch {
    // ignore
  }
  return null;
}

export function savePosition(pos: PetPosition): void {
  localStorage.setItem(POSITION_KEY, JSON.stringify({ x: Math.round(pos.x), y: Math.round(pos.y) }));
}

/** 完成提示音接管：宠物开启即接管（静音则整体静默，spec D3） */
export function petSoundTakeover(): boolean {
  return loadVisible();
}

/** 通知浮窗抑制：宠物开启且置顶（spec D4） */
export function petSuppressPopup(): boolean {
  return loadVisible() && loadConfig().alwaysOnTop;
}
```

- [x] **Step 4: 运行测试确认通过**

Run: `pnpm vitest run tests/pet/petConfig.test.ts`
Expected: PASS（5 个用例）

- [x] **Step 5: Commit**

```bash
git add src/components/pet/petConfig.ts tests/pet/petConfig.test.ts
git commit -m "feat(pet): add pet config store with sanitize and cross-window sync"
```

---

### Task 3: petStatus — 灯色差分推导（纯函数）

**Files:**
- Create: `src/components/pet/petStatus.ts`
- Test: `tests/pet/petStatus.test.ts`

**Interfaces:**
- Consumes: `Session` 类型（`@/types/session`，已有）。
- Produces:
  - `PetLight = "waiting" | "running" | "done"`
  - `PetCard { id, title, lines: string[], light, unread }`
  - `PetStatusState = Record<string, PetEntry>`（`PetEntry { light, prevColor, unread, vanishedAt, title, lines }`）
  - `computePetStatus(sessions: Session[], prev: PetStatusState | null, now: number): { cards: PetCard[]; moreCount: number; events: { newWaiting: string[]; newCompletion: string[] }; state: PetStatusState }`
  - `cardsFromState(state): PetCard[]`（ack 后即时重算）
  - `ackDone(state, id): void`（绿卡点击已读即消，spec C2/C4）

- [x] **Step 1: 写失败测试**

```ts
// tests/pet/petStatus.test.ts
import { describe, expect, it } from "vitest";
import type { Session } from "@/types/session";
import { computePetStatus, ackDone, cardsFromState } from "@/components/pet/petStatus";

const mk = (id: string, status: Session["status"], over: Partial<Session> = {}): Session => ({
  id, agentType: "claude", projectName: "P", projectPath: "/p", title: null, gitBranch: null,
  githubUrl: null, status, lastMessage: "msg", lastMessageRole: null, lastActivityAt: "",
  pid: 1, cpuUsage: 0, activeSubagentCount: 0, form: "cli", jumpSupported: true, ...over,
});

describe("computePetStatus", () => {
  it("灯色映射：waiting红 / 运行三态黄 / idle·finished绿（spec D2）", () => {
    const first = computePetStatus([mk("a", "waiting"), mk("b", "thinking"), mk("c", "idle")], null, 0);
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
    expect(s2.cards.find((x) => x.id === "c")).toMatchObject({ light: "done", unread: true, lines: ["已完成"] });
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

  it("会话消失：卡片保留 60s 后清理，不亮断联灯（H4/D9）", () => {
    const s1 = computePetStatus([mk("a", "waiting")], null, 0);
    const s2 = computePetStatus([], s1.state, 30_000);
    expect(s2.cards.find((x) => x.id === "a")?.light).toBe("waiting");
    const s3 = computePetStatus([], s1.state, 90_000);
    expect(s3.cards.find((x) => x.id === "a")).toBeUndefined();
  });

  it("排序 waiting>running>done，最多 6 张 + moreCount（H5/C3）", () => {
    const mkDone = (i: string) => {
      const a = computePetStatus([mk(i, "thinking")], null, 0);
      return computePetStatus([mk(i, "idle")], a.state, 10).state;
    };
    let state = { ...mkDone("d1"), ...mkDone("d2") };
    const sess = [mk("w", "waiting"), mk("r1", "processing"), mk("r2", "compacting"), ...["d1", "d2"].map((i) => mk(i, "idle"))];
    const r = computePetStatus(sess, state, 100);
    expect(r.cards.slice(0, 3).map((c) => c.light)).toEqual(["waiting", "running", "running"]);
    expect(r.cards.length).toBeLessThanOrEqual(6);
    expect(r.cards.length + r.moreCount).toBe(5);
  });

  it("卡片标题与摘要：title 优先，lastMessage 截断（H3）", () => {
    const r = computePetStatus([mk("a", "processing", { title: "自定义标题", lastMessage: "x".repeat(200) })], null, 0);
    const card = r.cards[0];
    expect(card.title).toBe("自定义标题");
    expect(card.lines[0].length).toBeLessThan(60);
    expect(card.lines[0].endsWith("…")).toBe(true);
  });
});
```

- [x] **Step 2: 运行确认失败**

Run: `pnpm vitest run tests/pet/petStatus.test.ts`
Expected: FAIL（模块不存在）

- [x] **Step 3: 实现**

```ts
// 六态 → 桌宠灯色差分推导（纯函数，spec §5）。卡片=状态展示，事件/未读=差分。
import type { Session, SessionStatus } from "@/types/session";

export type PetLight = "waiting" | "running" | "done";
export type StatusColor = "red" | "yellow" | "green";

export interface PetCard {
  id: string;
  title: string;
  lines: string[];
  light: PetLight;
  unread: boolean;
}

export interface PetEntry {
  light: PetLight | null;
  prevColor: StatusColor | null;
  unread: boolean;
  vanishedAt: number | null;
  title: string;
  lines: string[];
}

export type PetStatusState = Record<string, PetEntry>;

const RUNNING_SET: ReadonlySet<SessionStatus> = new Set(["processing", "thinking", "compacting"]);
const MAX_CARDS = 6;
const VANISH_TTL_MS = 60_000;

export function statusColor(s: SessionStatus): StatusColor {
  if (s === "waiting") return "red";
  if (RUNNING_SET.has(s)) return "yellow";
  return "green"; // idle / finished
}

// 词元感知截断（移植原版 estimateTokens/truncate，CJK 每字 1 词元）
function estimateTokens(text: string): number {
  let n = 0;
  let inWord = false;
  for (const ch of text) {
    if (/[\u4e00-\u9fff]/.test(ch)) { n += 1; inWord = false; }
    else if (/\s/.test(ch)) inWord = false;
    else if (!inWord) { n += 1; inWord = true; }
  }
  return n;
}

export function truncate(s: string, maxTokens = 24): string {
  const t = (s || "").replace(/\s+/g, " ").trim();
  if (!t || estimateTokens(t) <= maxTokens) return t;
  let n = 0; let inWord = false; let cut = t.length;
  for (let i = 0; i < t.length; i++) {
    const ch = t[i];
    if (/[\u4e00-\u9fff]/.test(ch)) { n += 1; inWord = false; }
    else if (/\s/.test(ch)) inWord = false;
    else if (!inWord) { n += 1; inWord = true; }
    if (n >= maxTokens) { cut = i + 1; break; }
  }
  return t.slice(0, cut).trim() + "…";
}

function cardLines(color: StatusColor, session: Session): string[] {
  if (color === "red") return ["等待操作", ...(session.lastMessage ? [truncate(session.lastMessage)] : [])];
  if (color === "green") return ["已完成"];
  return [session.lastMessage ? truncate(session.lastMessage) : "运行中"];
}

export function computePetStatus(
  sessions: Session[],
  prev: PetStatusState | null,
  now: number
): { cards: PetCard[]; moreCount: number; events: { newWaiting: string[]; newCompletion: string[] }; state: PetStatusState } {
  const first = prev === null;
  const state: PetStatusState = {};
  const events = { newWaiting: [] as string[], newCompletion: [] as string[] };

  for (const s of sessions) {
    const color = statusColor(s.status);
    const p = prev?.[s.id];
    const completion = !first && !!p?.prevColor && p.prevColor !== "green" && color === "green";
    const unread = first ? false : completion || (!!p && p.light === "done" && p.unread);
    const light: PetLight | null =
      color === "red" ? "waiting" : color === "yellow" ? "running" : unread ? "done" : null;
    const title = s.title || s.projectName || s.id;
    state[s.id] = { light, prevColor: color, unread, vanishedAt: null, title, lines: cardLines(color, s) };
    if (completion) events.newCompletion.push(s.id);
    if (!first && color === "red" && p?.prevColor !== "red") events.newWaiting.push(s.id);
  }

  // 消失会话：保留卡片 60s（未读绿卡不瞬间消失），不亮断联灯（spec D9/H4）
  for (const [id, p] of Object.entries(prev ?? {})) {
    if (state[id]) continue;
    if (p.vanishedAt !== null && now - p.vanishedAt > VANISH_TTL_MS) continue; // 清理
    state[id] = { ...p, vanishedAt: p.vanishedAt ?? now };
  }

  const all = cardsFromState(state);
  return { cards: all.slice(0, MAX_CARDS), moreCount: all.length - MAX_CARDS, events, state };
}

const LIGHT_RANK: Record<PetLight, number> = { waiting: 0, running: 1, done: 2 };

export function cardsFromState(state: PetStatusState): PetCard[] {
  return Object.entries(state)
    .filter(([, e]) => e.light !== null)
    .map(([id, e]) => ({ id, title: e.title, lines: e.lines, light: e.light as PetLight, unread: e.unread }))
    .sort((a, b) => LIGHT_RANK[a.light] - LIGHT_RANK[b.light] || a.title.localeCompare(b.title, "zh"));
}

/** 绿卡点击已读即消（spec C2） */
export function ackDone(state: PetStatusState, id: string): void {
  const e = state[id];
  if (!e || e.light !== "done") return;
  e.unread = false;
  e.light = null;
}
```

- [x] **Step 4: 运行测试确认通过**

Run: `pnpm vitest run tests/pet/petStatus.test.ts`
Expected: PASS（8 个用例）。若排序/映射有偏差按测试修正实现。

- [x] **Step 5: Commit**

```bash
git add src/components/pet/petStatus.ts tests/pet/petStatus.test.ts
git commit -m "feat(pet): add light-status differential derivation with unread and vanish ttl"
```

---

### Task 4: petAnimations — 精灵动画常量与帧样式

**Files:**
- Create: `src/components/pet/petAnimations.ts`
- Test: `tests/pet/petAnimations.test.ts`

**Interfaces:**
- Produces:
  - `PetAnimKey = "idle"|"run-right"|"run-left"|"waving"|"jumping"|"failed"|"waiting"|"running"|"review"|"look"`
  - `ANIM: Record<Exclude<PetAnimKey,"look">, { row: number; d: number[] }>`（逐帧 ms 时长，移植原版）
  - `LOOK_FRAMES: { x: number; y: number }[]`（16 向）
  - `frameStyle(anim, frame, lookFrame, scale): { backgroundPosition: string; backgroundSize: string }`
  - 常量 `FRAME_W=192`、`FRAME_H=208`、`SHEET_COLS=8`

- [x] **Step 1: 写失败测试**

```ts
// tests/pet/petAnimations.test.ts
import { describe, expect, it } from "vitest";
import { ANIM, LOOK_FRAMES, frameStyle, FRAME_W, FRAME_H } from "@/components/pet/petAnimations";

describe("petAnimations", () => {
  it("ANIM 表与原版一致（行号与逐帧时长）", () => {
    expect(ANIM.idle).toEqual({ row: 0, d: [280, 110, 110, 140, 140, 320] });
    expect(ANIM["run-right"].row).toBe(1);
    expect(ANIM["run-left"].row).toBe(2);
    expect(ANIM.waving).toEqual({ row: 3, d: [140, 140, 140, 280] });
    expect(ANIM.jumping.row).toBe(4);
    expect(ANIM.failed.row).toBe(5);
    expect(ANIM.waiting.row).toBe(6);
    expect(ANIM.running.row).toBe(7);
    expect(ANIM.review.row).toBe(8);
  });

  it("look 16 向：前 8 帧行 9、后 8 帧行 10，列循环", () => {
    expect(LOOK_FRAMES).toHaveLength(16);
    expect(LOOK_FRAMES[0]).toEqual({ x: 0, y: -9 * FRAME_H });
    expect(LOOK_FRAMES[7]).toEqual({ x: -7 * FRAME_W, y: -9 * FRAME_H });
    expect(LOOK_FRAMES[8]).toEqual({ x: 0, y: -10 * FRAME_H });
    expect(LOOK_FRAMES[15]).toEqual({ x: -7 * FRAME_W, y: -10 * FRAME_H });
  });

  it("frameStyle：scale 等比例（spec D15）", () => {
    const s1 = frameStyle("idle", 2, -1, 1);
    expect(s1.backgroundPosition).toBe(`${-2 * FRAME_W}px ${0}px`);
    expect(s1.backgroundSize).toBe("1536px 2288px");
    const s125 = frameStyle("idle", 0, -1, 1.25);
    expect(s125.backgroundSize).toBe("1920px 2860px");
    expect(s125.backgroundPosition).toBe("0px 0px");
    const look = frameStyle("look", 0, 9, 1);
    expect(look.backgroundPosition).toBe(`0px ${-10 * FRAME_H}px`);
  });
});
```

- [x] **Step 2: 运行确认失败**

Run: `pnpm vitest run tests/pet/petAnimations.test.ts`
Expected: FAIL

- [x] **Step 3: 实现**

```ts
// 精灵动画 — Codex V2 图集（8 列×11 行，每帧 192×208），逐帧时长 + look 16 向（spec §7）
export const FRAME_W = 192;
export const FRAME_H = 208;
export const SHEET_COLS = 8;
export const SHEET_ROWS = 11;

export type PetAnimKey =
  | "idle" | "run-right" | "run-left" | "waving" | "jumping"
  | "failed" | "waiting" | "running" | "review" | "look";

export const ANIM: Record<Exclude<PetAnimKey, "look">, { row: number; d: number[] }> = {
  idle: { row: 0, d: [280, 110, 110, 140, 140, 320] },
  "run-right": { row: 1, d: [120, 120, 120, 120, 120, 120, 120, 220] },
  "run-left": { row: 2, d: [120, 120, 120, 120, 120, 120, 120, 220] },
  waving: { row: 3, d: [140, 140, 140, 280] },
  jumping: { row: 4, d: [140, 140, 140, 140, 280] },
  failed: { row: 5, d: [140, 140, 140, 140, 140, 140, 140, 240] },
  waiting: { row: 6, d: [150, 150, 150, 150, 150, 260] },
  running: { row: 7, d: [120, 120, 120, 120, 120, 220] },
  review: { row: 8, d: [150, 150, 150, 150, 150, 280] },
};

// look 行 9→10：16 向顺时针连续扫视（行9 列0..7 → 行10 列0..7）
export const LOOK_FRAMES = Array.from({ length: 16 }, (_, i) => ({
  x: -(i % SHEET_COLS) * FRAME_W,
  y: -(i < 8 ? 9 : 10) * FRAME_H,
}));

export function frameStyle(
  anim: PetAnimKey,
  frame: number,
  lookFrame: number,
  scale: number
): { backgroundPosition: string; backgroundSize: string } {
  const w = FRAME_W * scale;
  const h = FRAME_H * scale;
  let x: number;
  let y: number;
  if (anim === "look") {
    const f = LOOK_FRAMES[Math.max(0, Math.min(LOOK_FRAMES.length - 1, lookFrame))];
    x = f.x * scale;
    y = f.y * scale;
  } else {
    const def = ANIM[anim];
    const i = ((frame % def.d.length) + def.d.length) % def.d.length;
    x = -i * w;
    y = -def.row * h;
  }
  return {
    backgroundPosition: `${x}px ${y}px`,
    backgroundSize: `${w * SHEET_COLS}px ${h * SHEET_ROWS}px`,
  };
}
```

- [x] **Step 4: 运行测试确认通过**

Run: `pnpm vitest run tests/pet/petAnimations.test.ts`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add src/components/pet/petAnimations.ts tests/pet/petAnimations.test.ts
git commit -m "feat(pet): add sprite animation tables and scaled frame style"
```

---

### Task 5: petVoices — 语音系统

**Files:**
- Create: `src/components/pet/petVoices.ts`
- Test: `tests/pet/petVoices.test.ts`

**Interfaces:**
- Consumes: Task 1 的 `public/pet/manifest.json` 结构。
- Produces:
  - `VoiceGroup = "general"|"approval"|"done"|"error"`、`VoiceEntry { index, group, name, file }`
  - `parseManifest(raw: unknown): VoiceEntry[]`（按组重排索引、组内 zh 排序）
  - `pickIndex(len, lastIndex): number`（随机且不与上次连续重复；len≤0 → -1；len=1 → 0）
  - `subtitleMs(durationSec): number`（=max(2500, duration×1000+250)）
  - `class VoicePlayer`：`load(entries)`、`pick(group): VoiceEntry | null`、`play(entry, { muted, onSubtitle }): void`、`unlock(): void`、`dispose(): void`（Audio 元素由它持有；IPC/音频不可测部分走人工验收）

- [x] **Step 1: 写失败测试**

```ts
// tests/pet/petVoices.test.ts
import { describe, expect, it } from "vitest";
import { parseManifest, pickIndex, subtitleMs } from "@/components/pet/petVoices";

describe("petVoices", () => {
  it("parseManifest：组序重排索引、组内 zh 排序、忽略非法项", () => {
    const raw = [
      { group: "done", name: "b", file: "done/b.m4a" },
      { group: "general", name: "乙", file: "general/乙.m4a" },
      { group: "general", name: "甲", file: "general/甲.m4a" },
      { group: "hack", name: "x", file: "hack/x.m4a" },
      null,
    ];
    const out = parseManifest(raw);
    expect(out.map((v) => v.index)).toEqual([0, 1, 2]);
    expect(out[0].group).toBe("general");
    expect(out[0].name).toBe("甲"); // zh 排序：甲 < 乙
    expect(out[2].group).toBe("done");
  });

  it("pickIndex：不连续重复；边界", () => {
    expect(pickIndex(0, -1)).toBe(-1);
    expect(pickIndex(1, 0)).toBe(0);
    for (let i = 0; i < 50; i++) {
      const idx = pickIndex(3, 1);
      expect(idx).not.toBe(1);
      expect(idx).toBeGreaterThanOrEqual(0);
      expect(idx).toBeLessThanOrEqual(2);
    }
  });

  it("subtitleMs：与音频时长对齐，最短 2.5s（spec E4）", () => {
    expect(subtitleMs(0)).toBe(2500);
    expect(subtitleMs(NaN)).toBe(2500);
    expect(subtitleMs(1.2)).toBe(2500); // 1200+250=1450 < 2500
    expect(subtitleMs(4)).toBe(4250);
  });
});
```

- [x] **Step 2: 运行确认失败**

Run: `pnpm vitest run tests/pet/petVoices.test.ts`
Expected: FAIL

- [x] **Step 3: 实现**

```ts
// 语音系统 — manifest 解析、组内随机不重复、字幕时长对齐、预载播放（spec §6.2）
export type VoiceGroup = "general" | "approval" | "done" | "error";
export interface VoiceEntry { index: number; group: VoiceGroup; name: string; file: string }

const GROUPS: VoiceGroup[] = ["general", "approval", "done", "error"];

export function parseManifest(raw: unknown): VoiceEntry[] {
  if (!Array.isArray(raw)) return [];
  const out: VoiceEntry[] = [];
  let index = 0;
  for (const g of GROUPS) {
    const items = raw.filter(
      (v): v is { group: string; name?: unknown; file: unknown } =>
        !!v && typeof v === "object" && (v as { group?: unknown }).group === g && typeof (v as { file?: unknown }).file === "string"
    );
    items.sort((a, b) => String(a.name ?? "").localeCompare(String(b.name ?? ""), "zh"));
    for (const it of items) {
      out.push({ index: index++, group: g, name: String(it.name ?? ""), file: it.file });
    }
  }
  return out;
}

/** 组内随机、不与上次连续重复（spec E3） */
export function pickIndex(len: number, lastIndex: number): number {
  if (len <= 0) return -1;
  if (len === 1) return 0;
  let i = Math.floor(Math.random() * len);
  while (i === lastIndex) i = Math.floor(Math.random() * len);
  return i;
}

export const MIN_SPEECH_MS = 2500;

/** 字幕时长 = max(2.5s, 音频时长+0.25s)（spec E4） */
export function subtitleMs(durationSec: number): number {
  const d = Number.isFinite(durationSec) && durationSec > 0 ? durationSec * 1000 : 0;
  return Math.max(MIN_SPEECH_MS, d + 250);
}

/**
 * 播放器：每条语音一个预载 Audio 元素（即时出声）。
 * muted 只拦声音；talkative 由调用方决定是否传 onSubtitle。
 * unlock：首次用户手势内 muted 试播，解除 WKWebView 自动播放限制（spec E6）。
 */
export class VoicePlayer {
  private entries: VoiceEntry[] = [];
  private els: HTMLAudioElement[] = [];
  private lastIdx: Partial<Record<VoiceGroup, number>> = {};
  private shared: HTMLAudioElement | null = null;
  private unlocked = false;

  load(entries: VoiceEntry[]): void {
    this.dispose();
    this.entries = entries;
    try {
      this.els = entries.map((v) => {
        const a = new Audio(`/pet/voice/${v.file}`);
        a.preload = "auto";
        a.load();
        return a;
      });
    } catch {
      this.els = []; // 浏览器测试环境无音频：静默降级
    }
  }

  /** 组内挑一条（组空返回 null，spec E5） */
  pick(group: VoiceGroup): VoiceEntry | null {
    const list = this.entries.filter((v) => v.group === group);
    const pool = list.length > 0 ? list : group === "general" ? this.entries : [];
    if (pool.length === 0) return null;
    const i = pickIndex(pool.length, this.lastIdx[group] ?? -1);
    this.lastIdx[group] = i;
    return pool[i];
  }

  /** 播放 + 字幕回调（ms 后隐藏字幕由调用方定时） */
  play(entry: VoiceEntry, opts: { muted: boolean; onSubtitle?: (name: string, ms: number) => void }): void {
    if (opts.muted) return;
    const el = this.els[entry.index];
    try {
      if (el) {
        for (const a of this.els) if (a !== el && !a.paused) a.pause();
        el.currentTime = 0;
        const pr = el.play();
        if (pr && typeof pr.catch === "function") pr.catch(() => { /* blocked：等 unlock */ });
        // 元数据已就绪→按时长对齐；未就绪→按最短 2.5s 兜底立即出字幕（spec E4）
        const dur = Number.isFinite(el.duration) && el.duration > 0 ? el.duration : 0;
        opts.onSubtitle?.(entry.name, subtitleMs(dur));
      } else {
        if (!this.shared) this.shared = new Audio();
        this.shared.src = `/pet/voice/${entry.file}`;
        const pr = this.shared.play();
        if (pr && typeof pr.catch === "function") pr.catch(() => { /* ignore */ });
        const s = this.shared;
        const dur = Number.isFinite(s.duration) && s.duration > 0 ? s.duration : 0;
        opts.onSubtitle?.(entry.name, subtitleMs(dur));
      }
    } catch {
      // ignore
    }
  }

  /** 首次手势内调用：muted 试播解锁自动播放（spec E6） */
  unlock(): void {
    if (this.unlocked) return;
    this.unlocked = true;
    const el = this.els[0] ?? this.shared;
    try {
      if (el) {
        el.muted = true;
        const pr = el.play();
        if (pr && typeof pr.catch === "function") pr.catch(() => {});
        el.pause();
        el.muted = false;
      }
    } catch {
      // ignore
    }
  }

  dispose(): void {
    for (const a of this.els) { try { a.pause(); a.src = ""; } catch { /* ignore */ } }
    this.els = [];
    this.entries = [];
    this.lastIdx = {};
  }
}
```

- [x] **Step 4: 运行测试确认通过**

Run: `pnpm vitest run tests/pet/petVoices.test.ts`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add src/components/pet/petVoices.ts tests/pet/petVoices.test.ts
git commit -m "feat(pet): add voice manifest parser, picker and preload player"
```

---

### Task 6: usePetWindow — 窗口控制 hook 与物理纯函数

**Files:**
- Create: `src/components/pet/usePetWindow.ts`
- Test: `tests/pet/usePetWindow.test.ts`

**Interfaces:**
- Produces:
  - 纯函数：`bottomAnchoredY(oldY, oldH, newH): number`、`clampToWorkArea(x, y, w, h, work): { x: number; y: number }`、`hitTest(rects: Rect[], px, py): boolean`（`Rect={x,y,w,h}`）、`stepFall(s: FallState, dt, groundY): FallState & { landed: boolean; rest: boolean }`（`FallState={x,y,vx,vy}`）
  - 常量 `GRAVITY=1400`、`DAMP=0.86`、`MIN_VX=24`
  - `usePetWindow(): { contentRef, registerInteractive(el: HTMLElement|null): void, syncSize(w: number, h: number): Promise<void>, moveBy(dx, dy): void, beginDrag(e): void, releaseDrag(e, throwVelocity): void, setMenuOpen(b): void }`（IPC 部分手工验收；测试只覆盖纯函数。注意：窗口尺寸由调用方在 `useLayoutEffect` 里量测内容 DOM 后经 `syncSize` 驱动——不用 ResizeObserver 监听 `position:fixed; inset:0` 的根，那会量到窗口自身形成反馈回路）

- [x] **Step 1: 写失败测试（纯函数）**

```ts
// tests/pet/usePetWindow.test.ts
import { describe, expect, it } from "vitest";
import { bottomAnchoredY, clampToWorkArea, hitTest, stepFall, GRAVITY } from "@/components/pet/usePetWindow";

describe("usePetWindow pure helpers", () => {
  it("bottomAnchoredY：新高度下保持底边不动（spec §4.2）", () => {
    expect(bottomAnchoredY(500, 260, 360)).toBe(400); // 500+260-360
    expect(bottomAnchoredY(500, 260, 620)).toBe(140);
  });

  it("clampToWorkArea：越界夹紧", () => {
    const work = { x: 0, y: 40, width: 1000, height: 960 };
    expect(clampToWorkArea(-50, 0, 340, 260, work)).toEqual({ x: 0, y: 40 });
    expect(clampToWorkArea(900, 900, 340, 260, work)).toEqual({ x: 660, y: 740 });
    expect(clampToWorkArea(100, 100, 340, 260, work)).toEqual({ x: 100, y: 100 });
  });

  it("hitTest：点在任一矩形内", () => {
    const rects = [{ x: 10, y: 10, w: 100, h: 50 }];
    expect(hitTest(rects, 50, 30)).toBe(true);
    expect(hitTest(rects, 5, 30)).toBe(false);
    expect(hitTest([], 50, 30)).toBe(false);
  });

  it("stepFall：重力加速、阻尼衰减、落地判定、静止阈值（spec §8）", () => {
    let s = { x: 0, y: 0, vx: 500, vy: 0 };
    let landed = false;
    for (let i = 0; i < 300 && !s.rest; i++) {
      const r = stepFall(s, 1 / 60, 700);
      s = { x: r.x, y: r.y, vx: r.vx, vy: r.vy };
      landed = landed || r.landed;
    }
    expect(landed).toBe(true);
    expect(s.y).toBe(700);
    expect(s.vx).toBeLessThan(24); // 阻尼后静止
    // 无初速垂直坠落 0.5s：y ≈ ½gt²
    const f = stepFall({ x: 0, y: 0, vx: 0, vy: 0 }, 0.5, 100000);
    expect(Math.abs(f.y - (0.5 * GRAVITY * 0.25))).toBeLessThan(1e-6);
  });
});
```

- [x] **Step 2: 运行确认失败**

Run: `pnpm vitest run tests/pet/usePetWindow.test.ts`
Expected: FAIL

- [x] **Step 3: 实现**

```ts
// 宠物窗口控制 — 尺寸/位置/穿透/物理（spec §4/§8）。IPC 调用一律 try/catch 静默降级（浏览器预览兼容）。
import { useCallback, useEffect, useRef } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { LogicalPosition, LogicalSize } from "@tauri-apps/api/dpi";
import { loadPosition, savePosition } from "./petConfig";

export const GRAVITY = 1400; // px/s²（spec §8，原版同值）
export const DAMP = 0.86;
export const MIN_VX = 24;

export interface Rect { x: number; y: number; w: number; h: number }
export interface FallState { x: number; y: number; vx: number; vy: number }

export function bottomAnchoredY(oldY: number, oldH: number, newH: number): number {
  return oldY + oldH - newH;
}

export function clampToWorkArea(x: number, y: number, w: number, h: number, work: Rect): { x: number; y: number } {
  return {
    x: Math.min(Math.max(x, work.x), work.x + work.width - w),
    y: Math.min(Math.max(y, work.y), work.y + work.height - h),
  };
}

export function hitTest(rects: Rect[], px: number, py: number): boolean {
  return rects.some((r) => px >= r.x && px <= r.x + r.w && py >= r.y && py <= r.y + r.h);
}

export function stepFall(s: FallState, dt: number, groundY: number): FallState & { landed: boolean; rest: boolean } {
  const vy = s.vy + GRAVITY * dt;
  let y = s.y + vy * dt;
  const vx = s.vx * Math.pow(DAMP, dt * 60);
  const x = s.x + vx * dt;
  const landed = y >= groundY;
  if (landed) y = groundY;
  return { x, y, vx, vy, landed, rest: landed && Math.abs(vx) < MIN_VX };
}

async function getWorkArea(): Promise<Rect | null> {
  try {
    const mon = await getCurrentWindow().currentMonitor();
    if (!mon?.workArea) return null;
    const k = mon.scaleFactor || 1;
    const wa = mon.workArea; // 物理像素 → 逻辑
    return { x: wa.x / k, y: wa.y / k, w: wa.width / k, h: wa.height / k };
  } catch {
    return null;
  }
}

/**
 * 窗口几何与穿透控制：
 * - registerInteractive 登记交互实体（精灵/卡片/菜单）；forward mousemove 命中切换穿透（spec §4.4）
 * - syncSize(w, h)：调用方在 useLayoutEffect 量测内容 DOM 后驱动窗口尺寸（防抖 50ms + 底部锚定，spec §4.2）
 * - beginDrag/releaseDrag 拖拽窗口与抛掷物理
 */
export function usePetWindow() {
  const contentRef = useRef<HTMLDivElement | null>(null);
  const interactiveEls = useRef(new Set<HTMLElement>());
  const dragRef = useRef<{ pointerId: number; startX: number; startY: number; winX: number; winY: number; samples: { t: number; x: number; y: number }[] } | null>(null);
  const geoRef = useRef<{ x: number; y: number; w: number; h: number } | null>(null);
  const ignoringRef = useRef(true);
  const menuOpenRef = useRef(false);
  const fallRafRef = useRef(0);
  const resizeTimer = useRef<number | null>(null);

  const readGeometry = useCallback(async () => {
    try {
      const win = getCurrentWindow();
      const pos = await win.outerPosition();
      const size = await win.outerSize();
      const k = (await win.scaleFactor()) || 1;
      const geo = { x: pos.x / k, y: pos.y / k, w: size.width / k, h: size.height / k };
      geoRef.current = geo;
      return geo;
    } catch {
      return geoRef.current;
    }
  }, []);

  const setIgnoring = useCallback(async (ignore: boolean) => {
    if (ignoringRef.current === ignore) return;
    ignoringRef.current = ignore;
    try {
      // forward: macOS 把 mousemove 转发给 webview（spec D11）
      await getCurrentWindow().setIgnoreCursorEvents(ignore, { forward: true });
    } catch {
      // 浏览器预览/旧版本：跳过
    }
  }, []);

  /** 内容实测尺寸 → 窗口 setSize + 底部锚定 setPosition（防抖 50ms，spec §4.2） */
  const syncSize = useCallback(async (w: number, h: number) => {
    if (resizeTimer.current) window.clearTimeout(resizeTimer.current);
    resizeTimer.current = window.setTimeout(async () => {
      if (w <= 0 || h <= 0) return;
      try {
        const win = getCurrentWindow();
        const geo = (await readGeometry()) ?? { x: 0, y: 0, w, h };
        if (geo.w === w && geo.h === h) return;
        const work = await getWorkArea();
        let nx = geo.x;
        let ny = bottomAnchoredY(geo.y, geo.h, h); // 底部锚定：精灵不动
        if (work) ({ x: nx, y: ny } = clampToWorkArea(nx, ny, w, h, work));
        await win.setSize(new LogicalSize(w, h));
        await win.setPosition(new LogicalPosition(nx, ny));
        geoRef.current = { x: nx, y: ny, w, h };
      } catch {
        // ignore
      }
    }, 50);
  }, [readGeometry]);

  // 启动：恢复记忆位置 + 初始置底
  useEffect(() => {
    (async () => {
      const saved = loadPosition();
      const geo = await readGeometry();
      if (!geo) return;
      if (saved) {
        const work = await getWorkArea();
        const target = work ? clampToWorkArea(saved.x, saved.y, geo.w, geo.h, work) : saved;
        try {
          await getCurrentWindow().setPosition(new LogicalPosition(target.x, target.y));
        } catch { /* ignore */ }
      }
      await setIgnoring(true);
    })();
  }, [readGeometry, setIgnoring]);

  // forward mousemove → 命中切换穿透（spec §4.4）
  useEffect(() => {
    const onMove = (e: MouseEvent) => {
      if (dragRef.current || menuOpenRef.current) {
        void setIgnoring(false);
        return;
      }
      const rects: Rect[] = [];
      const base = contentRef.current?.getBoundingClientRect();
      if (base) {
        for (const el of interactiveEls.current) {
          const r = el.getBoundingClientRect();
          rects.push({ x: r.x, y: r.y, w: r.width, h: r.height });
        }
        const hit = hitTest(rects, e.clientX, e.clientY);
        void setIgnoring(!hit);
      }
    };
    window.addEventListener("mousemove", onMove);
    return () => window.removeEventListener("mousemove", onMove);
  }, [setIgnoring]);

  const registerInteractive = useCallback((el: HTMLElement | null) => {
    if (el) interactiveEls.current.add(el);
    return () => {
      if (el) interactiveEls.current.delete(el);
    };
  }, []);

  const moveBy = useCallback(async (dx: number, dy: number) => {
    const geo = (await readGeometry()) ?? geoRef.current;
    if (!geo) return;
    const nx = geo.x + dx;
    const ny = geo.y + dy;
    geoRef.current = { ...geo, x: nx, y: ny };
    try {
      await getCurrentWindow().setPosition(new LogicalPosition(nx, ny));
    } catch { /* ignore */ }
  }, [readGeometry]);

  const beginDrag = useCallback((e: React.PointerEvent) => {
    void readGeometry().then((geo) => {
      if (!geo) return;
      dragRef.current = {
        pointerId: e.pointerId,
        startX: e.clientX,
        startY: e.clientY,
        winX: geo.x,
        winY: geo.y,
        samples: [],
      };
    });
  }, [readGeometry]);

  const trackDrag = useCallback((e: React.PointerEvent) => {
    const d = dragRef.current;
    if (!d || d.pointerId !== e.pointerId) return null;
    d.samples.push({ t: performance.now(), x: e.clientX, y: e.clientY });
    while (d.samples.length > 0 && performance.now() - d.samples[0].t > 150) d.samples.shift();
    return { dx: e.clientX - d.startX, dy: e.clientY - d.startY, movedX: e.clientX - (d.samples[0]?.x ?? e.clientX), movedY: e.clientY - (d.samples[0]?.y ?? e.clientY) };
  }, []);

  /** 松手：gravity 开→抛物坠落（rAF 循环 moveWindow）；否则停驻记忆（spec §8） */
  const releaseDrag = useCallback((opts: { gravity: boolean; onLand?: () => void }) => {
    const d = dragRef.current;
    dragRef.current = null;
    if (!d) return;
    const geo = geoRef.current;
    if (!geo) return;
    const px = d.samples.length >= 2 ? d.samples[d.samples.length - 1] : null;
    const first = d.samples[0] ?? null;
    const dt = px && first ? (px.t - first.t) / 1000 : 0;
    const vx0 = px && first && dt > 0 ? (px.x - first.x) / dt : 0;
    if (!opts.gravity || !px) {
      savePosition({ x: geo.x, y: geo.y });
      return;
    }
    let st: FallState = { x: geo.x, y: geo.y, vx: vx0, vy: 0 };
    let landedFired = false;
    let last = performance.now();
    const tick = async (t: number) => {
      const dts = Math.min(0.05, (t - last) / 1000);
      last = t;
      const work = await getWorkArea();
      const geoNow = geoRef.current;
      if (!geoNow) return;
      const groundY = work ? work.y + work.h - geoNow.h : st.y;
      const r = stepFall(st, dts, groundY);
      st = { x: r.x, y: r.y, vx: r.vx, vy: r.vy };
      geoRef.current = { ...geoNow, x: r.x, y: r.y };
      try {
        await getCurrentWindow().setPosition(new LogicalPosition(r.x, r.y));
      } catch { /* ignore */ }
      if (r.landed && !landedFired) {
        landedFired = true;
        opts.onLand?.();
      }
      if (r.rest) {
        savePosition({ x: r.x, y: r.y });
        fallRafRef.current = 0;
        return;
      }
      fallRafRef.current = requestAnimationFrame(tick);
    };
    fallRafRef.current = requestAnimationFrame(tick);
  }, []);

  const setMenuOpen = useCallback((open: boolean) => {
    menuOpenRef.current = open;
    if (open) void setIgnoring(false);
  }, [setIgnoring]);

  // 卸载清理
  useEffect(() => () => {
    if (fallRafRef.current) cancelAnimationFrame(fallRafRef.current);
    if (resizeTimer.current) window.clearTimeout(resizeTimer.current);
  }, []);

  return { contentRef, registerInteractive, syncSize, beginDrag, trackDrag, releaseDrag, moveBy, setMenuOpen, readGeometry };
}
```

- [x] **Step 4: 运行测试确认通过**

Run: `pnpm vitest run tests/pet/usePetWindow.test.ts`
Expected: PASS（4 个用例）

- [x] **Step 5: Lint + Commit**

Run: `pnpm lint`
```bash
git add src/components/pet/usePetWindow.ts tests/pet/usePetWindow.test.ts
git commit -m "feat(pet): add window control hook with bottom-anchored sizing, hit-test pass-through and fall physics"
```

---

### Task 7: Rust 建窗 command + capability + 启动创建

**Files:**
- Create: `src-tauri/src/commands/pet.rs`、`src-tauri/capabilities/pet.json`
- Modify: `src-tauri/src/commands/mod.rs`（加 `pub mod pet;`）
- Modify: `src-tauri/src/lib.rs`（setup 建窗 + 注册 command）

**Interfaces:**
- Produces: `#[tauri::command] set_pet_visible(app, visible: bool)`、`#[tauri::command] set_pet_always_on_top(app, on_top: bool)`；窗口 label `"pet"`、URL `index.html#/pet`。Task 13/14 前端 invoke 这两个命令。

- [x] **Step 1: 实现 pet.rs**

```rust
// 桌宠窗口管理 — 建窗参数与显隐/置顶（spec §4.1/§4.5）
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

const PET_W: f64 = 340.0;
const PET_H: f64 = 260.0;

/// 创建桌宠窗口（隐藏态；前端加载后按 localStorage 决定显隐，避免启动闪现）
pub fn create_pet_window(app: &AppHandle) -> Result<(), String> {
    if app.get_webview_window("pet").is_some() {
        return Ok(());
    }
    // 默认主显示器右下角；前端随后按记忆位置与实测尺寸重排
    let mut mx = 1440.0;
    let mut my = 900.0;
    if let Ok(Some(m)) = app.primary_monitor() {
        mx = m.size().width as f64 / m.scale_factor();
        my = m.size().height as f64 / m.scale_factor();
    }
    let x = mx - PET_W - 24.0;
    let y = my - PET_H - 76.0;
    WebviewWindowBuilder::new(app, "pet", WebviewUrl::App("index.html#/pet".into()))
        .title("mam-pet")
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .visible(false)
        .inner_size(PET_W, PET_H)
        .position(x, y)
        .build()
        .map(|_| ())
        .map_err(|e| format!("创建桌宠窗口失败: {}", e))
}

#[tauri::command]
pub async fn set_pet_visible(app: AppHandle, visible: bool) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("pet") {
        if visible {
            w.show().map_err(|e| e.to_string())?;
        } else {
            w.hide().map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn set_pet_always_on_top(app: AppHandle, on_top: bool) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("pet") {
        w.set_always_on_top(on_top).map_err(|e| e.to_string())?;
    }
    Ok(())
}
```

- [x] **Step 2: capability 文件**

```json
{
  "$schema": "../gen/schemas/desktop-schema.json",
  "identifier": "pet",
  "description": "桌宠窗口权限（spec §4）",
  "windows": ["pet"],
  "permissions": [
    "core:default",
    "core:window:allow-show",
    "core:window:allow-hide",
    "core:window:allow-set-size",
    "core:window:allow-set-position",
    "core:window:allow-outer-position",
    "core:window:allow-outer-size",
    "core:window:allow-set-always-on-top",
    "core:window:allow-set-ignore-cursor-events",
    "core:window:allow-current-monitor",
    "core:window:allow-scale-factor",
    "core:window:allow-start-dragging",
    "core:window:allow-is-visible",
    "core:event:allow-emit",
    "core:event:allow-listen"
  ]
}
```

- [x] **Step 3: 接线（mod.rs / lib.rs）**

`src-tauri/src/commands/mod.rs` 增加：

```rust
pub mod pet;
```

`src-tauri/src/lib.rs` 的 `.setup(|app| {...})` 闭包内（devtools 打开之后）增加：

```rust
            // 桌宠窗口：启动即创建（隐藏），前端按配置决定显隐（spec §4.1）
            if let Err(e) = commands::pet::create_pet_window(app.handle()) {
                log::warn!("pet window create failed: {}", e);
            }
```

并在 `generate_handler![...]` 列表追加两项（与现有 `commands::notification::show_notification_window` 同级）：

```rust
        commands::pet::set_pet_visible,
        commands::pet::set_pet_always_on_top,
```

> 若 `log` crate 未引入，改用 `eprintln!`。

- [x] **Step 4: 编译验证**

Run: `cd src-tauri && cargo check`
Expected: 编译通过，零 error（warning 视现有基线）

Run: `cd src-tauri && cargo clippy --all-targets -- -D warnings 2>&1 | tail -3`
Expected: 无新增告警

- [x] **Step 5: Commit**

```bash
git add src-tauri/src/commands/pet.rs src-tauri/src/commands/mod.rs src-tauri/src/lib.rs src-tauri/capabilities/pet.json
git commit -m "feat(pet): create transparent always-on-top pet window at startup with visibility commands"
```

---

### Task 8: 宠物页面骨架 + 路由 + 精灵渲染与 idle 动画

**Files:**
- Create: `src/pages/pet.tsx`
- Create: `src/components/pet/FoxbellPet.tsx`（本任务只做骨架：精灵 + idle/look 帧步进 + 配置订阅；交互在 Task 9）
- Modify: `src/main.tsx`（`#/pet` 分流）
- Test: `tests/pet/foxbell-render.test.tsx`

**Interfaces:**
- Consumes: Task 2 `loadConfig/subscribeConfig/loadVisible/saveVisible`、Task 4 `ANIM/frameStyle`、Task 6 `usePetWindow`。
- Produces: `export function FoxbellPet(): JSX.Element`；`pages/pet.tsx` 默认导出页面（含显隐/置顶应用逻辑）。

- [x] **Step 1: 写失败测试**

```tsx
// tests/pet/foxbell-render.test.tsx — 渲染骨架 + 帧步进 + 显隐应用（窗口 API 全 mock）
import { render, screen, act } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const showMock = vi.fn();
const setAlwaysOnTopMock = vi.fn();
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    show: showMock,
    hide: vi.fn(),
    setAlwaysOnTop: setAlwaysOnTopMock,
    setIgnoreCursorEvents: vi.fn(),
    setPosition: vi.fn(),
    setSize: vi.fn(),
    outerPosition: vi.fn(),
    outerSize: vi.fn(),
    scaleFactor: vi.fn(),
    currentMonitor: vi.fn(),
  }),
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@/lib/query/queries/sessions", () => ({ useSessionsQuery: () => ({ data: undefined }) }));

import { FoxbellPet } from "@/components/pet/FoxbellPet";

describe("FoxbellPet 骨架", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    localStorage.clear();
  });
  afterEach(() => vi.useRealTimers());

  it("渲染精灵：图集尺寸随 scale 等比例（spec D15）", () => {
    localStorage.setItem("mam-pet-config", JSON.stringify({ scale: 1.25 }));
    render(<FoxbellPet />);
    const sprite = screen.getByTestId("pet-sprite");
    expect(sprite.style.width).toBe("240px"); // 192×1.25
    expect(sprite.style.height).toBe("260px"); // 208×1.25
    expect(sprite.style.backgroundSize).toBe("1920px 2860px");
  });

  it("idle 帧步进：前进到第 2 帧（spec F1）", async () => {
    render(<FoxbellPet />);
    const sprite = screen.getByTestId("pet-sprite");
    const before = sprite.style.backgroundPosition;
    await act(async () => {
      await vi.advanceTimersByTimeAsync(280); // 第 1 帧时长
    });
    expect(sprite.style.backgroundPosition).not.toBe(before);
  });
});
```

- [x] **Step 2: 运行确认失败**

Run: `pnpm vitest run tests/pet/foxbell-render.test.tsx`
Expected: FAIL（组件不存在）

- [x] **Step 3: 实现 FoxbellPet 骨架**

```tsx
// FoxbellPet — 桌宠本体（spec §7/§8/§9）。Task 8：精灵 + 帧步进 + look 环顾 + 缩放；
// Task 9 追加交互（拖拽/物理/点击/菜单）；Task 10 追加卡片；Task 12 追加事件接线。
import { useEffect, useRef, useState } from "react";
import { ANIM, frameStyle, FRAME_H, FRAME_W, type PetAnimKey } from "./petAnimations";
import { loadConfig, subscribeConfig, type PetConfig } from "./petConfig";
import { usePetWindow } from "./usePetWindow";

export function FoxbellPet() {
  const [cfg, setCfg] = useState<PetConfig>(() => loadConfig());
  const cfgRef = useRef(cfg);
  cfgRef.current = cfg;
  useEffect(() => subscribeConfig(() => setCfg(loadConfig())), []);

  const pet = usePetWindow();
  const spriteRef = useRef<HTMLDivElement | null>(null);
  useEffect(() => pet.registerInteractive(spriteRef.current), [pet]);

  // ---- 动画状态机（spec §7：拖拽 > 瞬时 > 任务态 > look > idle）----
  const [anim, setAnim] = useState<PetAnimKey>("idle");
  const [frame, setFrame] = useState(0);
  const [lookFrame, setLookFrame] = useState(-1);
  const animRef = useRef<PetAnimKey>("idle");
  const frameRef = useRef(0);
  const stepTimer = useRef<number | null>(null);
  const stateRef = useRef<{ drag: PetAnimKey | null; transient: PetAnimKey | null; task: PetAnimKey | null; look: boolean }>({
    drag: null, transient: null, task: null, look: false,
  });
  const lookStop = useRef<(() => void) | null>(null);
  const genRef = useRef({ transient: 0, look: 0 });

  const later = useRef((fn: () => void, ms: number) => {
    const id = window.setTimeout(fn, ms);
    return () => window.clearTimeout(id);
  }).current;

  const cancelStep = () => {
    if (stepTimer.current !== null) {
      window.clearTimeout(stepTimer.current);
      stepTimer.current = null;
    }
  };

  const stepLoop = () => {
    const def = ANIM[(animRef.current === "look" ? "idle" : animRef.current) as keyof typeof ANIM];
    const i = frameRef.current;
    const ms = def.d[i] ?? 160;
    stepTimer.current = window.setTimeout(() => {
      frameRef.current = (i + 1) % def.d.length;
      setFrame(frameRef.current);
      stepLoop();
    }, ms);
  };

  const applyAnim = (key: PetAnimKey) => {
    if (animRef.current === key) return;
    animRef.current = key;
    setAnim(key);
    cancelStep();
    frameRef.current = 0;
    setFrame(0);
    stepLoop();
  };

  const refreshAnim = () => {
    const s = stateRef.current;
    applyAnim(s.drag ?? s.transient ?? s.task ?? (s.look ? "look" : "idle"));
  };

  /** 瞬时动作（代数计数防过期覆盖，spec F4） */
  const playTransient = useRef((key: PetAnimKey, ms: number) => {
    const gen = ++genRef.current.transient;
    stateRef.current.transient = key;
    refreshAnim();
    later(() => {
      if (genRef.current.transient === gen && stateRef.current.transient === key) {
        stateRef.current.transient = null;
        refreshAnim();
      }
    }, ms);
  }).current;

  // ---- look 环顾：空闲 6s 触发，16 向 250ms/帧，任何状态打断（spec F2）----
  const stopLook = () => {
    lookStop.current?.();
    lookStop.current = null;
    if (stateRef.current.look) {
      stateRef.current.look = false;
      setLookFrame(-1);
    }
  };
  const scheduleNextLook = () => {
    const gen = ++genRef.current.look;
    later(() => {
      if (genRef.current.look !== gen) return;
      const s = stateRef.current;
      if (!s.drag && !s.transient && !s.task) {
        s.look = true;
        setLookFrame(0);
        refreshAnim();
        let i = 0;
        const id = window.setInterval(() => {
          i += 1;
          if (i >= 16) {
            window.clearInterval(id);
            lookStop.current = null;
            stopLook();
            refreshAnim();
            scheduleNextLook();
          } else {
            setLookFrame(i);
          }
        }, 250);
        lookStop.current = () => window.clearInterval(id);
      } else {
        scheduleNextLook();
      }
    }, 6000);
  };

  useEffect(() => {
    stepLoop();
    scheduleNextLook();
    return () => {
      cancelStep();
      stopLook();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const px = (v: number) => Math.round(v * cfg.scale);
  const style = frameStyle(anim, frame, lookFrame, cfg.scale);

  return (
    <div ref={pet.contentRef} style={{ position: "fixed", inset: 0, overflow: "visible" }}>
      <div
        ref={spriteRef}
        data-testid="pet-sprite"
        style={{
          position: "absolute",
          left: "50%",
          transform: "translateX(-50%)",
          bottom: px(50), // 底部气泡区（spec §4.2）
          width: px(FRAME_W),
          height: px(FRAME_H),
          backgroundImage: "url(/pet/spritesheet.webp)",
          backgroundPosition: style.backgroundPosition,
          backgroundSize: style.backgroundSize,
          backgroundRepeat: "no-repeat",
          cursor: "grab",
          touchAction: "none",
          userSelect: "none",
        }}
      />
    </div>
  );
}
```

- [x] **Step 4: 实现页面与路由**

```tsx
// src/pages/pet.tsx — 宠物窗口路由页：应用显隐/置顶后渲染桌宠（spec §4.1/§4.5）
import { useEffect } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { FoxbellPet } from "@/components/pet/FoxbellPet";
import { loadConfig, loadVisible, saveVisible, subscribeConfig } from "@/components/pet/petConfig";
import { invoke } from "@tauri-apps/api/core";

export default function PetPage() {
  useEffect(() => {
    const apply = async () => {
      const cfg = loadConfig();
      try {
        await getCurrentWindow().setAlwaysOnTop(cfg.alwaysOnTop);
        if (loadVisible()) await getCurrentWindow().show();
      } catch {
        // 浏览器预览：忽略
      }
    };
    apply();
    // 托盘/主窗口切换显隐后同步本地状态（spec §10.2）
    const un1 = listen<{ visible: boolean }>("pet-visibility-changed", (e) => {
      saveVisible(e.payload.visible);
    }).catch(() => Promise.resolve(() => {}));
    const un2 = subscribeConfig(() => {
      const cfg = loadConfig();
      getCurrentWindow().setAlwaysOnTop(cfg.alwaysOnTop).catch(() => {});
    });
    // 兜底：窗口存活但从未显式 show（如首次开启）
    invoke("set_pet_visible", { visible: loadVisible() }).catch(() => {});
    return () => {
      un1.then((f) => f());
      un2();
    };
  }, []);
  return <FoxbellPet />;
}
```

`src/main.tsx` 修改（分流处，对照现有 `isNotificationWindow`）：

```tsx
const NotificationPage = lazy(() => import("./pages/notification"));
const PetPage = lazy(() => import("./pages/pet")); // 新增
...
const isNotificationWindow = window.location.hash === "#/notification";
const isPetWindow = window.location.hash === "#/pet"; // 新增
const PageComponent = isNotificationWindow
  ? NotificationPage
  : isPetWindow
    ? PetPage
    : (pageMap[pathname as keyof typeof pageMap] ?? HomePage);
```

AppWrapper 的 effect 改为宠物窗口也不自动 show：

```tsx
    if (isNotificationWindow || isPetWindow) return;
```

- [x] **Step 5: 运行测试确认通过**

Run: `pnpm vitest run tests/pet/foxbell-render.test.tsx`
Expected: PASS（2 个用例）

- [x] **Step 6: Lint + Commit**

Run: `pnpm lint`
```bash
git add src/pages/pet.tsx src/components/pet/FoxbellPet.tsx src/main.tsx tests/pet/foxbell-render.test.tsx
git commit -m "feat(pet): add pet page route with sprite rendering, idle stepping and look-around"
```

---

### Task 9: 完整指针交互 — 拖拽/物理/单击/双击/压扁回弹

**Files:**
- Modify: `src/components/pet/FoxbellPet.tsx`
- Test: `tests/pet/foxbell-interactions.test.tsx`

**Interfaces:**
- Consumes: Task 6 `usePetWindow` 的 `beginDrag/trackDrag/releaseDrag/moveBy`、Task 5 `VoicePlayer`。
- Produces: 精灵 DOM 事件 `onPointerDown/Move/Up/DoubleClick`；`playVoice(group, action)` 内部函数（Task 12 事件接线复用）；语音解锁 `voiceRef.current.unlock()`。

- [x] **Step 1: 写失败测试**

```tsx
// tests/pet/foxbell-interactions.test.tsx — 指针交互与语音触发（窗口 API mock）
import { fireEvent, render, screen, act } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    show: vi.fn(), hide: vi.fn(), setAlwaysOnTop: vi.fn(), setIgnoreCursorEvents: vi.fn(),
    setPosition: vi.fn(), setSize: vi.fn(), outerPosition: vi.fn(async () => ({ x: 0, y: 0 })),
    outerSize: vi.fn(async () => ({ width: 680, height: 520 })), scaleFactor: vi.fn(async () => 1),
    currentMonitor: vi.fn(async () => ({ workArea: { x: 0, y: 0, width: 1440, height: 900 }, scaleFactor: 1 })),
  }),
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@/lib/query/queries/sessions", () => ({ useSessionsQuery: () => ({ data: undefined }) }));
const fetchMock = vi.fn(async () => ({ json: async () => [] }));
vi.stubGlobal("fetch", fetchMock);

import { FoxbellPet } from "@/components/pet/FoxbellPet";

describe("FoxbellPet 指针交互", () => {
  beforeEach(() => { vi.useFakeTimers(); localStorage.clear(); });
  afterEach(() => vi.useRealTimers());

  it("单击：挥手不出声（spec A1）", async () => {
    render(<FoxbellPet />);
    const sprite = screen.getByTestId("pet-sprite");
    fireEvent.pointerDown(sprite, { pointerId: 1, button: 0, clientX: 100, clientY: 100 });
    fireEvent.pointerUp(sprite, { pointerId: 1, clientX: 100, clientY: 100 });
    // waving 行 3：backgroundPosition y = -3×208×scale
    await act(async () => { await vi.advanceTimersByTimeAsync(10); });
    expect(sprite.style.backgroundPosition).toContain("-624px");
  });

  it("双击：说话（general 语音 + 字幕气泡，spec A2）", async () => {
    localStorage.setItem("mam-pet-config", JSON.stringify({ muted: false, talkative: true }));
    render(<FoxbellPet />);
    const sprite = screen.getByTestId("pet-sprite");
    fireEvent.dblClick(sprite);
    // manifest 拉取异步完成 + 播放：推进微任务
    await act(async () => { await vi.advanceTimersByTimeAsync(20); });
    const bubble = screen.queryByTestId("pet-bubble");
    expect(bubble).not.toBeNull(); // 测试环境 Audio 缺失仍显示字幕（经 shared 回退）
  });

  it("拖拽方向动画：上拖跳跃（spec A3）", async () => {
    render(<FoxbellPet />);
    const sprite = screen.getByTestId("pet-sprite");
    fireEvent.pointerDown(sprite, { pointerId: 1, button: 0, clientX: 100, clientY: 300 });
    fireEvent.pointerMove(sprite, { pointerId: 1, clientX: 100, clientY: 250 }); // dy=-50
    await act(async () => { await vi.advanceTimersByTimeAsync(10); });
    // jumping 行 4：y = -4×208
    expect(sprite.style.backgroundPosition).toContain("-832px");
  });
});
```

- [x] **Step 2: 运行确认失败**

Run: `pnpm vitest run tests/pet/foxbell-interactions.test.tsx`
Expected: FAIL（事件处理器缺失）

- [x] **Step 3: 在 FoxbellPet 中实现交互（骨架代码中追加）**

在组件内追加（放 `playTransient` 定义之后；`subtitle` 状态与 `bubbleGen` 代数）：

```tsx
  // ---- 语音与字幕（Task 12 事件接线复用，spec §6.2）----
  const [subtitle, setSubtitle] = useState<string | null>(null);
  const bubbleGen = useRef(0);
  const voiceRef = useRef<VoicePlayer | null>(null);
  const manifestLoaded = useRef(false);
  const unlockedRef = useRef(false);

  useEffect(() => {
    if (manifestLoaded.current) return;
    manifestLoaded.current = true;
    fetch("/pet/manifest.json")
      .then((r) => r.json())
      .then((raw) => {
        const entries = parseManifest(raw);
        const player = new VoicePlayer();
        player.load(entries);
        voiceRef.current = player;
      })
      .catch(() => { /* 素材缺失：语音静默降级（spec §13） */ });
    return () => voiceRef.current?.dispose();
  }, []);

  const showBubble = (text: string, ms: number) => {
    const gen = ++bubbleGen.current;
    setSubtitle(text);
    later(() => {
      if (bubbleGen.current === gen) setSubtitle(null);
    }, ms);
  };

  /** 播一组语音 + 动作 + 字幕（muted 只拦声音不拦动作，spec D5） */
  const playVoice = (group: VoiceGroup, action: PetAnimKey) => {
    playTransient(action, 1700);
    const player = voiceRef.current;
    if (!player) return;
    const entry = player.pick(group);
    if (!entry) return; // 空组静默跳过（spec E5）
    player.play(entry, {
      muted: cfgRef.current.muted,
      onSubtitle: (name, ms) => {
        if (cfgRef.current.talkative) showBubble(name, ms);
      },
    });
  };
  playVoiceRef.current = playVoice; // 供 Task 12 事件接线调用
```

（同时在文件顶部 import：`import { VoicePlayer, parseManifest, type VoiceGroup } from "./petVoices";`，并声明 `const playVoiceRef = useRef<(g: VoiceGroup, a: PetAnimKey) => void>(() => {});`）

指针事件（绑定到精灵 div）：

```tsx
  const onPointerDown = (e: React.PointerEvent) => {
    if (e.button !== 0) return; // 右键留给菜单（spec A6）
    stopLook();
    if (!unlockedRef.current) {
      unlockedRef.current = true;
      voiceRef.current?.unlock(); // 手势内解锁自动播放（spec E6）
    }
    pet.beginDrag(e);
  };

  const onPointerMove = (e: React.PointerEvent) => {
    const r = pet.trackDrag(e);
    if (!r) return;
    void pet.moveBy(r.dx - lastDeltaRef.current.dx, r.dy - lastDeltaRef.current.dy);
    lastDeltaRef.current = { dx: r.dx, dy: r.dy };
    // 方向动画按 150ms 采样窗增量判定（原版逐帧增量语义，spec A3 阈值同原版）
    const dir: PetAnimKey | null =
      r.movedY < -8 ? "jumping" : r.movedX < -6 ? "run-left" : r.movedX > 6 ? "run-right" : null;
    const s = stateRef.current;
    if (dir) s.drag = dir;
    refreshAnim();
  };

  const onPointerUp = (e: React.PointerEvent) => {
    const moved = lastDeltaRef.current.dx !== 0 || lastDeltaRef.current.dy !== 0;
    const d = stateRef.current;
    d.drag = null;
    if (!moved) playTransient("waving", 1700); // 单击：固定挥手（spec A1）
    refreshAnim();
    pet.releaseDrag({
      gravity: cfgRef.current.gravity,
      onLand: () => {
        // 落地压扁回弹 + 补跳（spec §8）
        const el = spriteRef.current;
        if (!el) return;
        el.style.transition = "transform 60ms ease-out";
        el.style.transform += " scaleY(0.55)";
        later(() => {
          el.style.transition = "transform 240ms cubic-bezier(.34,1.56,.64,1)";
          el.style.transform = el.style.transform.replace(" scaleY(0.55)", "");
          later(() => {
            el.style.transition = "";
            playTransient("jumping", 1500);
          }, 260);
        }, 60);
      },
    });
    lastDeltaRef.current = { dx: 0, dy: 0 };
  };

  const onDoubleClick = (e: React.MouseEvent) => {
    e.stopPropagation();
    playVoice("general", cfgRef.current.dblAction); // 双击说话（spec A2）
  };
```

（声明 `const lastDeltaRef = useRef({ dx: 0, dy: 0 });`；`onPointerDown` 里 `lastDeltaRef.current = { dx: 0, dy: 0 };` 重置。）

几何同步（本任务建立，卡片/菜单高度变化统一走这条路，spec §4.2）：在 FoxbellPet 内加 `useLayoutEffect`，以卡片区与菜单的实测高度驱动 `syncSize`——

```tsx
  // 窗口高度 = 气泡区 + 精灵 + 间隙 + max(卡片区, 菜单)，宽度恒 340×scale；底部锚定
  useLayoutEffect(() => {
    const baseH = px(50 + FRAME_H + 10);
    const cardsH = cardsWrapRef.current?.getBoundingClientRect().height ?? 0;
    const menuH = menuWrapRef.current?.getBoundingClientRect().height ?? 0;
    void pet.syncSize(px(340), Math.ceil(baseH + Math.max(cardsH, menuH)));
  }, [cfg.scale, cards, moreCount, menu]);
```

（Task 10 引入 `cardsWrapRef`，Task 12 引入 `menuWrapRef`（挂 PetMenu 根元素，无菜单时为 null 高度取 0）与 `menu` 状态；本任务先声明 `const menuWrapRef = useRef<HTMLDivElement | null>(null); const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);` 供依赖数组编译通过。）

精灵 div 追加属性与字幕气泡渲染：

```tsx
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerCancel={onPointerUp}
        onDoubleClick={onDoubleClick}
```

```tsx
      {subtitle && (
        <div
          data-testid="pet-bubble"
          style={{
            position: "absolute", left: "50%", transform: "translateX(-50%)",
            bottom: 8, maxWidth: px(320), padding: `${px(6)}px ${px(12)}px`,
            fontSize: px(13), lineHeight: 1.4, whiteSpace: "nowrap", overflow: "hidden",
            textOverflow: "ellipsis", borderRadius: px(12), pointerEvents: "none",
            background: "rgba(255,255,255,0.96)", color: "#7a4a2b",
            border: "1px solid rgba(122,74,43,0.35)", boxShadow: "0 2px 10px rgba(0,0,0,0.18)",
            zIndex: 2,
          }}
        >
          {subtitle}
        </div>
      )}
```

- [x] **Step 4: 运行测试确认通过**

Run: `pnpm vitest run tests/pet/foxbell-interactions.test.tsx tests/pet/foxbell-render.test.tsx`
Expected: 全部 PASS

- [x] **Step 5: Lint + Commit**

Run: `pnpm lint`
```bash
git add src/components/pet/FoxbellPet.tsx tests/pet/foxbell-interactions.test.tsx
git commit -m "feat(pet): add drag with direction anims, fall physics, squash rebound and voice interactions"
```

---

### Task 10: 状态卡片 + 跳转/歧义候选

**Files:**
- Modify: `src/components/pet/FoxbellPet.tsx`
- Test: `tests/pet/foxbell-cards.test.tsx`

**Interfaces:**
- Consumes: Task 3 `computePetStatus/ackDone/cardsFromState`、`useSessionJump`（`@/hooks/useSessionJump`，已有）。
- Produces: 头顶卡片区（`data-testid="pet-cards"`，每卡 `data-session-id`）、候选浮层（`data-testid="pet-jump-candidates"`）；`cards`/`moreCount` React 状态。

- [x] **Step 1: 写失败测试**

```tsx
// tests/pet/foxbell-cards.test.tsx — 卡片渲染 + 点击跳转 ack（msw/invoke mock 走 tests/msw）
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    show: vi.fn(), hide: vi.fn(), setAlwaysOnTop: vi.fn(), setIgnoreCursorEvents: vi.fn(),
    setPosition: vi.fn(), setSize: vi.fn(), outerPosition: vi.fn(), outerSize: vi.fn(),
    scaleFactor: vi.fn(), currentMonitor: vi.fn(),
  }),
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

const sessionsData = {
  sessions: [{
    id: "s1", agentType: "claude", projectName: "项目A", projectPath: "/a", title: "标题A",
    gitBranch: null, githubUrl: null, status: "waiting", lastMessage: "等你确认",
    lastMessageRole: null, lastActivityAt: "", pid: 42, cpuUsage: 0,
    activeSubagentCount: 0, form: "cli", jumpSupported: true,
  }],
  totalCount: 1, waitingCount: 1,
};
vi.mock("@/lib/query/queries/sessions", () => ({
  useSessionsQuery: () => ({ data: sessionsData }),
}));

import { invoke } from "@tauri-apps/api/core";
import { FoxbellPet } from "@/components/pet/FoxbellPet";

describe("FoxbellPet 卡片", () => {
  it("waiting 会话渲染红卡；点击调用 focus_session（spec C1）", async () => {
    render(<FoxbellPet />);
    const card = await screen.findByTestId("pet-card-s1");
    expect(card.textContent).toContain("标题A");
    expect(card.textContent).toContain("等待操作");
    fireEvent.click(card);
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("focus_session", expect.objectContaining({ pid: 42, sessionId: "s1" }))
    );
  });
});
```

> `tests/msw/tauriMocks` 的 `focus_session` 当前返回 `undefined`，会让组件读 `result.type` 抛错走 catch。在 `tauriMocks.ts` 中把 `focus_session` 的返回改为 `Promise.resolve({ type: "ok" })`（与 `kill_session` 分开），只改 tests/msw 不改 src。

- [x] **Step 2: 运行确认失败**

Run: `pnpm vitest run tests/pet/foxbell-cards.test.tsx`
Expected: FAIL（无卡片渲染）

- [x] **Step 3: 实现（FoxbellPet 追加）**

```tsx
  // ---- 状态卡片（Task 10；spec §5/C1-C4）----
  const [cards, setCards] = useState<PetCard[]>([]);
  const [moreCount, setMoreCount] = useState(0);
  const [candidates, setCandidates] = useState<JumpWindowCandidate[] | null>(null);
  const statusStateRef = useRef<PetStatusState | null>(null);
  const sessionIndexRef = useRef<Map<string, Session>>(new Map());
  const cardsWrapRef = useRef<HTMLDivElement | null>(null);
  useEffect(() => pet.registerInteractive(cardsWrapRef.current), [pet]);

  const { data } = useSessionsQuery();
  useEffect(() => {
    if (!data) return;
    sessionIndexRef.current = new Map(data.sessions.map((s) => [s.id, s]));
    const r = computePetStatus(data.sessions, statusStateRef.current, Date.now());
    statusStateRef.current = r.state;
    setCards(r.cards);
    setMoreCount(r.moreCount);
    // 事件接线在 Task 12 补充（newWaiting/newCompletion → 语音）
  }, [data]);

  const jump = async (card: PetCard) => {
    const s = sessionIndexRef.current.get(card.id);
    if (!s) return;
    try {
      const result = await invoke<{ type: string; windows?: JumpWindowCandidate[] }>("focus_session", {
        pid: s.pid, sessionId: s.id, agentType: s.agentType,
        projectName: s.projectName, lastMessage: s.lastMessage ?? undefined,
      });
      if (result.type === "ambiguous" && result.windows?.length) {
        setCandidates(result.windows); // 歧义候选浮层（spec D12）
        return;
      }
    } catch {
      return; // 跳转失败：卡片保留（spec §13）
    }
    ackDone(statusStateRef.current ?? {}, card.id); // 点击已读即消（spec C2）
    setCards(cardsFromState(statusStateRef.current ?? {}));
  };
```

渲染（放在精灵 div 之前、内容根内）：

```tsx
      <div
        ref={cardsWrapRef}
        data-testid="pet-cards"
        style={{
          position: "absolute", bottom: px(50 + FRAME_H + 10), left: "50%",
          transform: "translateX(-50%)", display: "flex", flexDirection: "column",
          alignItems: "center", gap: px(5), width: px(320), zIndex: 3,
        }}
      >
        {cards.map((c) => (
          <div
            key={c.id}
            data-testid={`pet-card-${c.id}`}
            onClick={(e) => { e.stopPropagation(); void jump(c); }}
            style={{
              display: "flex", alignItems: "flex-start", gap: px(7), width: "100%",
              boxSizing: "border-box", padding: `${px(5)}px ${px(10)}px`, borderRadius: px(10),
              cursor: "pointer", fontSize: px(12), lineHeight: 1.45,
              background: "rgba(255,252,248,0.97)", border: "1px solid rgba(122,74,43,0.3)",
              boxShadow: "0 2px 8px rgba(0,0,0,0.14)",
            }}
          >
            <span style={{
              width: px(8), height: px(8), borderRadius: "50%", flex: "none", marginTop: px(4),
              background: c.light === "waiting" ? "#ef4444" : c.light === "running" ? "#eab308" : "#60a5fa",
              boxShadow: `0 0 0 2px ${c.light === "waiting" ? "rgba(239,68,68,.25)" : c.light === "running" ? "rgba(234,179,8,.25)" : "rgba(96,165,250,.25)"}`,
            }} />
            <div style={{ minWidth: 0 }}>
              <div style={{ fontWeight: 700, color: "#7a4a2b", overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{c.title}</div>
              {c.lines.map((l, i) => (
                <div key={i} style={{ color: "#a07050", overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{l}</div>
              ))}
            </div>
          </div>
        ))}
        {moreCount > 0 && (
          <div style={{ color: "#a07050", fontSize: px(11), background: "rgba(255,252,248,0.9)", borderRadius: 999, padding: `${px(2)}px ${px(8)}px` }}>
            +{moreCount} {t("pet.card.more")}
          </div>
        )}
      </div>
      {candidates && (
        <div data-testid="pet-jump-candidates" style={{
          position: "absolute", bottom: px(50 + FRAME_H + 10), left: "50%", transform: "translateX(-50%)",
          width: px(320), maxHeight: px(240), overflowY: "auto", zIndex: 5,
          background: "rgba(30,30,34,0.96)", color: "#eee", borderRadius: px(10),
          fontSize: px(12), padding: `${px(4)}px 0`,
        }}>
          {candidates.map((w) => (
            <div key={w.hwnd} onClick={() => { void invoke("focus_hwnd", { hwnd: w.hwnd }); setCandidates(null); ackDone(statusStateRef.current ?? {}, cards[0]?.id ?? ""); }}
              style={{ padding: `${px(3)}px ${px(14)}px`, cursor: "pointer" }}>
              {w.title}<span style={{ color: "#a1a1aa" }}> · {w.process}</span>
            </div>
          ))}
        </div>
      )}
```

（import 补充：`useSessionsQuery`、`computePetStatus/ackDone/cardsFromState/PetCard/PetStatusState`、`invoke`、`JumpWindowCandidate`、`useTranslation` 的 `t`。候选点击 ack 逻辑：记录 `pendingAckId`，聚焦后 ack 该 id——实现时把点击卡片时先记 `pendingAckRef.current = card.id`，候选选中后 `ackDone(state, pendingAckRef.current)`。）

- [x] **Step 4: 运行测试确认通过**

Run: `pnpm vitest run tests/pet/foxbell-cards.test.tsx`
Expected: PASS

- [x] **Step 5: Lint + Commit**

Run: `pnpm lint`
```bash
git add src/components/pet/FoxbellPet.tsx tests/pet/foxbell-cards.test.tsx tests/msw
git commit -m "feat(pet): render status cards with jump and ambiguous candidate list"
```

---

### Task 11: PetMenu — 右键菜单 / 大小三档 / 动作绑定子页 / 关于

**Files:**
- Create: `src/components/pet/PetMenu.tsx`
- Modify: `src/i18n/locales/zh.json`、`src/i18n/locales/en.json`
- Test: `tests/pet/petMenu.test.tsx`

**Interfaces:**
- Consumes: Task 2 `PetConfig/saveConfig/PET_ACTIONS/PET_SCALES`。
- Produces: `PetMenu(props: { anchor: { x: number; y: number }; onClose(): void; onPreview(action: PetAction): void; onHide(): void })`；菜单项行为全部通过 `saveConfig` 生效，配置变化由 `subscribeConfig` 回流。

- [x] **Step 1: i18n 键（两份 locale 同步加，`pnpm check:i18n` 校验）**

zh.json 增补（放到顶层与现有键并列）：

```json
{
  "pet": {
    "menu": {
      "sound": "🔊 出声", "subtitle": "💬 语音字幕", "physics": "🧲 物理坠落", "onTop": "📌 悬浮最前",
      "size": "📏 大小", "dblAction": "🖱️ 双击动作", "redAction": "🔴 红灯动作", "greenAction": "🟢 绿灯动作",
      "hide": "🦊 隐藏桌宠", "about": "ℹ️ 关于", "back": "← 返回", "on": "开", "off": "关",
      "aboutText": "Foxbell 桌宠 for MultiAgents Manager v1.0.0"
    },
    "scale": { "small": "小", "medium": "中", "large": "大" },
    "action": { "jumping": "跳一跳", "waving": "挥挥手", "failed": "委屈", "waiting": "等待", "review": "审查", "running": "工作" },
    "card": { "done": "已完成", "waiting": "等待操作", "running": "运行中", "more": "更多" }
  }
}
```

en.json 对应英文：`"sound": "🔊 Sound"`, `"subtitle": "💬 Voice subtitles"`, `"physics": "🧲 Drop physics"`, `"onTop": "📌 Always on top"`, `"size": "📏 Size"`, `"dblAction": "🖱️ Double-click action"`, `"redAction": "🔴 Red light action"`, `"greenAction": "🟢 Green light action"`, `"hide": "🦊 Hide pet"`, `"about": "ℹ️ About"`, `"back": "← Back"`, `"on": "On"`, `"off": "Off"`, `"aboutText": "Foxbell Pet for MultiAgents Manager v1.0.0"`, scale: `Small/Medium/Large`, action: `Jump/Wave/Sulking/Waiting/Review/Working`, card: `Done/Waiting/Running/More`。

> 注意 JSON 合并：用脚本或手工把 `pet` 键并入现有 JSON 根对象（不是整文件替换）。

- [x] **Step 2: 写失败测试**

```tsx
// tests/pet/petMenu.test.tsx
import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { PetMenu } from "@/components/pet/PetMenu";
import { loadConfig } from "@/components/pet/petConfig";

describe("PetMenu", () => {
  beforeEach(() => localStorage.clear());

  it("主菜单：三开关 + 大小 + 三动作绑定 + 隐藏/关于（spec §9 B1-B11/D14）", () => {
    const onPreview = vi.fn();
    render(<PetMenu anchor={{ x: 10, y: 10 }} onClose={() => {}} onPreview={onPreview} onHide={() => {}} />);
    expect(screen.getByText("🔊 出声")).toBeTruthy();
    expect(screen.getByText("💬 语音字幕")).toBeTruthy();
    expect(screen.getByText("🧲 物理坠落")).toBeTruthy();
    expect(screen.getByText("📏 大小")).toBeTruthy();
    expect(screen.getByText("🖱️ 双击动作")).toBeTruthy();
    expect(screen.getByText("🔴 红灯动作")).toBeTruthy();
    expect(screen.getByText("🟢 绿灯动作")).toBeTruthy();
    expect(screen.queryByText(/failed/i)).toBeNull(); // error 场景不出现（D14）
    expect(screen.getByText("🦊 隐藏桌宠")).toBeTruthy();
  });

  it("点出声开关：muted 翻转写配置（开=有声，spec B1）", () => {
    render(<PetMenu anchor={{ x: 10, y: 10 }} onClose={() => {}} onPreview={() => {}} onHide={() => {}} />);
    fireEvent.click(screen.getByText("🔊 出声"));
    expect(loadConfig().muted).toBe(true);
  });

  it("动作子页：进入即预览、选择即生效并回主菜单（spec B4-B7）", () => {
    const onPreview = vi.fn();
    render(<PetMenu anchor={{ x: 10, y: 10 }} onClose={() => {}} onPreview={onPreview} onHide={() => {}} />);
    fireEvent.click(screen.getByText("🟢 绿灯动作"));
    expect(onPreview).toHaveBeenCalled(); // 进入子页预览当前选中
    fireEvent.click(screen.getByText("委屈"));
    expect(loadConfig().doneAction).toBe("failed");
    expect(screen.getByText("← 返回")).toBeTruthy(); // 选择后回主菜单
  });

  it("大小子页：三档选择写配置（spec D15）", () => {
    render(<PetMenu anchor={{ x: 10, y: 10 }} onClose={() => {}} onPreview={() => {}} onHide={() => {}} />);
    fireEvent.click(screen.getByText("📏 大小"));
    fireEvent.click(screen.getByText("大"));
    expect(loadConfig().scale).toBe(1.25);
  });
});
```

- [x] **Step 3: 运行确认失败**

Run: `pnpm vitest run tests/pet/petMenu.test.tsx`
Expected: FAIL

- [x] **Step 4: 实现 PetMenu**

```tsx
// PetMenu — 右键菜单：开关 / 大小三档 / 三场景动作绑定（带实时预览）/ 隐藏 / 关于（spec §9 B/D14/D15）
import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  loadConfig, saveConfig, subscribeConfig,
  PET_ACTIONS, PET_SCALES, type PetAction, type PetConfig, type PetScale,
} from "./petConfig";

type MenuPage = null | "Size" | "Dbl" | "Red" | "Green" | "About";
const ACTION_PAGE: Record<"Dbl" | "Red" | "Green", keyof PetConfig> = {
  Dbl: "dblAction", Red: "approvalAction", Green: "doneAction",
};

export function PetMenu(props: {
  anchor: { x: number; y: number };
  onClose(): void;
  onPreview(action: PetAction): void;
  onHide(): void;
}) {
  const { t } = useTranslation();
  const [cfg, setCfg] = useState<PetConfig>(() => loadConfig());
  const [page, setPage] = useState<MenuPage>(null);
  const ref = useRef<HTMLDivElement | null>(null);
  useEffect(() => subscribeConfig(() => setCfg(loadConfig())), []);

  // 菜单外点击 / Esc 关闭（spec B10）
  useEffect(() => {
    const onDown = (e: PointerEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) props.onClose();
    };
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") props.onClose(); };
    window.addEventListener("pointerdown", onDown, true);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("pointerdown", onDown, true);
      window.removeEventListener("keydown", onKey);
    };
  }, [props]);

  // 动作子页实时预览：进入/切选项都触发，返回主菜单停（由父组件以 onPreview(null) 停）
  useEffect(() => {
    if (page && page in ACTION_PAGE) props.onPreview(cfg[ACTION_PAGE[page as keyof typeof ACTION_PAGE]] as PetAction);
    else if (page === null) props.onPreview(null as unknown as PetAction);
  }, [page, cfg, props]);

  const rowStyle: React.CSSProperties = { display: "flex", alignItems: "center", justifyContent: "space-between", gap: 10, padding: "3px 14px", cursor: "pointer" };
  const itemStyle: React.CSSProperties = { padding: "3px 14px", cursor: "pointer" };
  const btn = (on: boolean, onClick: () => void, label: string) => (
    <button onClick={(e) => { e.stopPropagation(); onClick(); }}
      style={{ background: on ? "#16a34a" : "#3f3f46", color: "#eee", border: "none", borderRadius: 6, fontSize: 12, padding: "1px 10px", cursor: "pointer" }}>
      {label}
    </button>
  );

  const actionLabel = (a: PetAction) => t(`pet.action.${a}`);

  return (
    <div ref={ref} data-testid="pet-menu" style={{
      position: "fixed", left: props.anchor.x, top: props.anchor.y, minWidth: 170,
      background: "rgba(30,30,34,0.96)", color: "#eee", fontSize: 13, lineHeight: 1.9,
      borderRadius: 10, padding: "4px 0", boxShadow: "0 6px 20px rgba(0,0,0,0.4)", zIndex: 10,
    }}>
      {page === "About" ? (
        <div data-testid="pet-menu-about" style={itemStyle} onClick={props.onClose}>{t("pet.menu.aboutText")}</div>
      ) : page === "Size" ? (
        <>
          <div style={{ ...itemStyle, cursor: "pointer" }} onClick={() => setPage(null)}>{t("pet.menu.back")}</div>
          {PET_SCALES.map((s: PetScale) => (
            <div key={s} style={{ ...itemStyle, color: cfg.scale === s ? "#fbbf24" : undefined }}
              onClick={() => { saveConfig({ scale: s }); setPage(null); }}>
              {s === 0.75 ? t("pet.scale.small") : s === 1 ? t("pet.scale.medium") : t("pet.scale.large")}
            </div>
          ))}
        </>
      ) : page && page in ACTION_PAGE ? (
        <>
          <div style={itemStyle} onClick={() => setPage(null)}>{t("pet.menu.back")}</div>
          {PET_ACTIONS.map((a) => (
            <div key={a} style={{ ...itemStyle, color: cfg[ACTION_PAGE[page as keyof typeof ACTION_PAGE]] === a ? "#fbbf24" : undefined }}
              onClick={() => { saveConfig({ [ACTION_PAGE[page as keyof typeof ACTION_PAGE]]: a } as Partial<PetConfig>); setPage(null); }}>
              {actionLabel(a)}
            </div>
          ))}
        </>
      ) : (
        <>
          <div style={rowStyle}>
            <span>{t("pet.menu.sound")}</span>
            {btn(!cfg.muted, () => saveConfig({ muted: !cfg.muted }), !cfg.muted ? t("pet.menu.on") : t("pet.menu.off"))}
          </div>
          <div style={rowStyle}>
            <span>{t("pet.menu.subtitle")}</span>
            {btn(cfg.talkative, () => saveConfig({ talkative: !cfg.talkative }), cfg.talkative ? t("pet.menu.on") : t("pet.menu.off"))}
          </div>
          <div style={rowStyle}>
            <span>{t("pet.menu.physics")}</span>
            {btn(cfg.gravity, () => saveConfig({ gravity: !cfg.gravity }), cfg.gravity ? t("pet.menu.on") : t("pet.menu.off"))}
          </div>
          <div style={rowStyle}>
            <span>{t("pet.menu.onTop")}</span>
            {btn(cfg.alwaysOnTop, () => saveConfig({ alwaysOnTop: !cfg.alwaysOnTop }), cfg.alwaysOnTop ? t("pet.menu.on") : t("pet.menu.off"))}
          </div>
          <div style={{ height: 1, margin: "4px 10px", background: "rgba(255,255,255,0.12)" }} />
          <div style={rowStyle} onClick={() => setPage("Size")}>
            <span>{t("pet.menu.size")}</span>
            <span style={{ color: "#a1a1aa", fontSize: 12 }}>
              {cfg.scale === 0.75 ? t("pet.scale.small") : cfg.scale === 1 ? t("pet.scale.medium") : t("pet.scale.large")}
            </span>
          </div>
          {(["Dbl", "Red", "Green"] as const).map((p) => (
            <div key={p} style={rowStyle} onClick={() => setPage(p)}>
              <span>{t(`pet.menu.${p === "Dbl" ? "dblAction" : p === "Red" ? "redAction" : "greenAction"}`)}</span>
              <span style={{ color: "#a1a1aa", fontSize: 12 }}>{actionLabel(cfg[ACTION_PAGE[p]] as PetAction)}</span>
            </div>
          ))}
          <div style={{ height: 1, margin: "4px 10px", background: "rgba(255,255,255,0.12)" }} />
          <div style={itemStyle} onClick={props.onHide}>{t("pet.menu.hide")}</div>
          <div style={itemStyle} onClick={() => setPage("About")}>{t("pet.menu.about")}</div>
        </>
      )}
    </div>
  );
}
```

> 预览契约：`onPreview(action)` 有值=循环播该动作；传 `null`=停。父组件（Task 12 接线时在 FoxbellPet 内）实现为 `action ? playTransientLoop(action) : stopLoop()`。

- [x] **Step 5: 运行测试确认通过**

Run: `pnpm vitest run tests/pet/petMenu.test.tsx && pnpm check:i18n`
Expected: PASS / 无缺键

- [x] **Step 6: Lint + Commit**

Run: `pnpm lint`
```bash
git add src/components/pet/PetMenu.tsx src/i18n/locales tests/pet/petMenu.test.tsx
git commit -m "feat(pet): add context menu with toggles, size presets and action bindings"
```

---

### Task 12: 事件接线 — 状态差分驱动语音/姿态 + 菜单挂载

**Files:**
- Modify: `src/components/pet/FoxbellPet.tsx`
- Test: `tests/pet/foxbell-events.test.tsx`

**Interfaces:**
- Consumes: Task 3 事件、Task 5 `playVoiceRef`、Task 9 `stateRef.task`、Task 11 `PetMenu`。
- Produces: 完整事件流（spec §5/§6）；`onContextMenu` 挂菜单。

- [x] **Step 1: 写失败测试**

```tsx
// tests/pet/foxbell-events.test.tsx — 差分事件触发语音与任务姿态（spec D1-D4）
import { render, screen, act, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    show: vi.fn(), hide: vi.fn(), setAlwaysOnTop: vi.fn(), setIgnoreCursorEvents: vi.fn(),
    setPosition: vi.fn(), setSize: vi.fn(), outerPosition: vi.fn(), outerSize: vi.fn(),
    scaleFactor: vi.fn(), currentMonitor: vi.fn(),
  }),
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
const manifest = [
  { index: 0, group: "done", name: "搞定咯", file: "done/x.m4a" },
  { index: 1, group: "approval", name: "快批快批", file: "approval/y.m4a" },
];
vi.stubGlobal("fetch", vi.fn(async () => ({ json: async () => manifest })));

let data: unknown = undefined;
vi.mock("@/lib/query/queries/sessions", () => ({ useSessionsQuery: () => ({ data }) }));

import { FoxbellPet } from "@/components/pet/FoxbellPet";

const mk = (id: string, status: string) => ({
  id, agentType: "claude", projectName: "P", projectPath: "/", title: null, gitBranch: null,
  githubUrl: null, status, lastMessage: "m", lastMessageRole: null, lastActivityAt: "",
  pid: 1, cpuUsage: 0, activeSubagentCount: 0, form: "cli", jumpSupported: true,
});

describe("FoxbellPet 事件接线", () => {
  beforeEach(() => { vi.useFakeTimers(); localStorage.clear(); });
  afterEach(() => vi.useRealTimers());

  it("运行中 → idle：播 done 组语音 + 绿卡（spec D1/D2）", async () => {
    data = { sessions: [mk("a", "thinking")], totalCount: 1, waitingCount: 0 };
    const { rerender } = render(<FoxbellPet />);
    await act(async () => { await vi.advanceTimersByTimeAsync(50); }); // 首帧 + manifest
    data = { sessions: [mk("a", "idle")], totalCount: 1, waitingCount: 0 };
    rerender(<FoxbellPet />);
    await act(async () => { await vi.advanceTimersByTimeAsync(50); });
    await waitFor(() => expect(screen.queryByTestId("pet-bubble")?.textContent).toBe("搞定咯"));
    expect(screen.getByTestId("pet-card-a").textContent).toContain("已完成");
  });

  it("运行中 → waiting：播 approval 组语音且绿卡不出现（spec D2/D3）", async () => {
    data = { sessions: [mk("a", "processing")], totalCount: 1, waitingCount: 0 };
    const { rerender } = render(<FoxbellPet />);
    await act(async () => { await vi.advanceTimersByTimeAsync(50); });
    data = { sessions: [mk("a", "waiting")], totalCount: 1, waitingCount: 1 };
    rerender(<FoxbellPet />);
    await act(async () => { await vi.advanceTimersByTimeAsync(50); });
    await waitFor(() => expect(screen.queryByTestId("pet-bubble")?.textContent).toBe("快批快批"));
    expect(screen.getByTestId("pet-card-a").textContent).toContain("等待操作");
  });

  it("waiting 持续：10s 内再次出现不重复播（spec D3 限频）", async () => {
    data = { sessions: [mk("a", "idle")], totalCount: 1, waitingCount: 0 };
    const { rerender } = render(<FoxbellPet />);
    await act(async () => { await vi.advanceTimersByTimeAsync(50); });
    data = { sessions: [mk("a", "waiting")], totalCount: 1, waitingCount: 1 };
    rerender(<FoxbellPet />);
    await act(async () => { await vi.advanceTimersByTimeAsync(50); });
    const first = screen.queryByTestId("pet-bubble");
    expect(first?.textContent).toBe("快批快批");
    data = { sessions: [mk("a", "idle")], totalCount: 1, waitingCount: 0 };
    rerender(<FoxbellPet />);
    await act(async () => { await vi.advanceTimersByTimeAsync(2000); });
    data = { sessions: [mk("a", "waiting")], totalCount: 1, waitingCount: 1 };
    rerender(<FoxbellPet />);
    await act(async () => { await vi.advanceTimersByTimeAsync(50); });
    // 2s < 10s 限频窗口：无新字幕
    await act(async () => { await vi.advanceTimersByTimeAsync(2600); });
    expect(screen.queryByTestId("pet-bubble")).toBeNull();
  });
});
```

- [x] **Step 2: 运行确认失败**

Run: `pnpm vitest run tests/pet/foxbell-events.test.tsx`
Expected: FAIL（事件未接线）

- [x] **Step 3: 实现（FoxbellPet 的 data effect 扩展）**

把 Task 10 的 `useEffect([data])` 扩展为：

```tsx
  const lastApprovalAtRef = useRef(0);
  const previewLoopRef = useRef<number | null>(null);

  useEffect(() => {
    if (!data) return;
    sessionIndexRef.current = new Map(data.sessions.map((s) => [s.id, s]));
    const r = computePetStatus(data.sessions, statusStateRef.current, Date.now());
    statusStateRef.current = r.state;
    setCards(r.cards);
    setMoreCount(r.moreCount);

    // ---- 事件差分 → 语音（spec §5/§6.2）----
    if (r.events.newCompletion.length > 0) {
      playVoiceRef.current("done", cfgRef.current.doneAction);
    }
    if (r.events.newWaiting.length > 0 && Date.now() - lastApprovalAtRef.current > 10_000) {
      lastApprovalAtRef.current = Date.now();
      playVoiceRef.current("approval", cfgRef.current.approvalAction);
    }

    // ---- 任务姿态：waiting > review > running（spec D4）----
    const anyWaiting = r.cards.some((c) => c.light === "waiting");
    const anyDoneUnread = r.cards.some((c) => c.light === "done" && c.unread);
    const anyRunning = r.cards.some((c) => c.light === "running");
    stateRef.current.task = anyWaiting ? "waiting" : anyDoneUnread ? "review" : anyRunning ? "running" : null;
    refreshAnim();
  }, [data]);
```

菜单挂载（精灵 div 加 `onContextMenu`；内容根尾部渲染 PetMenu）：

```tsx
        onContextMenu={(e) => {
          e.preventDefault();
          setMenu({ x: e.clientX, y: e.clientY });
          pet.setMenuOpen(true);
        }}
```

```tsx
      {menu && (
        <div ref={menuWrapRef}>
          <PetMenu
            anchor={menu}
            onClose={() => { setMenu(null); pet.setMenuOpen(false); stopPreview(); }}
            onPreview={(action) => {
              if (action) {
                stopPreview();
                const loop = () => { playTransient(action, 1600); };
                loop();
                previewLoopRef.current = window.setInterval(loop, 1700); // 子页循环预览（spec B4）
              } else {
                stopPreview();
              }
            }}
            onHide={() => {
              setMenu(null);
              pet.setMenuOpen(false);
              saveVisible(false);
              invoke("set_pet_visible", { visible: false }).catch(() => {});
              emitPetVisibility(false); // 广播给主窗口/托盘同步（见下）
            }}
          />
        </div>
      )}
```

辅助（组件外或组件内）：

```tsx
  const stopPreview = () => {
    if (previewLoopRef.current !== null) {
      window.clearInterval(previewLoopRef.current);
      previewLoopRef.current = null;
    }
  };
```

`emitPetVisibility`（从 `@tauri-apps/api/event` import `emit`）：

```ts
  const emitPetVisibility = (visible: boolean) => {
    emit("pet-visibility-changed", { visible }).catch(() => {});
  };
```

（PetPage 已在 Task 8 监听 `pet-visibility-changed` 回写 localStorage；主窗口入口在 Task 13 统一监听。）

- [x] **Step 4: 运行全部宠物测试确认通过**

Run: `pnpm vitest run tests/pet/`
Expected: 全部 PASS

- [x] **Step 5: Lint + Commit**

Run: `pnpm lint`
```bash
git add src/components/pet/FoxbellPet.tsx tests/pet/foxbell-events.test.tsx
git commit -m "feat(pet): wire status events to voices and task postures, mount context menu"
```

---

### Task 13: 主窗口集成 — 声音接管/浮窗抑制/🦊 按钮/设置分区

**Files:**
- Modify: `src/hooks/useNotification.ts`、`src/pages/home.tsx`、`src/pages/settings.tsx`
- Modify: `src/i18n/locales/zh.json`、`en.json`（settings/home/tray 键）
- Test: `tests/pet/notificationTakeover.test.ts`、`tests/pet/petSettings.test.tsx`

**Interfaces:**
- Consumes: Task 2 `petSoundTakeover/petSuppressPopup/loadVisible/saveVisible/subscribeConfig/loadConfig/saveConfig`、Task 7 command。
- Produces: 主窗口行为变更（spec §6.3/§6.4/§10.2）。

- [x] **Step 1: 写失败测试**

```ts
// tests/pet/notificationTakeover.test.ts
import { describe, expect, it, beforeEach } from "vitest";
import { petSoundTakeover, petSuppressPopup } from "@/components/pet/petConfig";

describe("通知让渡判定（spec D3/D4）", () => {
  beforeEach(() => localStorage.clear());
  it("宠物关闭：不接管、不抑制", () => {
    localStorage.setItem("mam-pet-visible", "0");
    expect(petSoundTakeover()).toBe(false);
    expect(petSuppressPopup()).toBe(false);
  });
  it("宠物开启即接管声音；置顶才抑制浮窗", () => {
    localStorage.setItem("mam-pet-visible", "1");
    expect(petSoundTakeover()).toBe(true);
    localStorage.setItem("mam-pet-config", JSON.stringify({ alwaysOnTop: false }));
    expect(petSuppressPopup()).toBe(false);
    localStorage.setItem("mam-pet-config", JSON.stringify({ alwaysOnTop: true }));
    expect(petSuppressPopup()).toBe(true);
  });
});
```

`tests/pet/petSettings.test.tsx`：

```tsx
// tests/pet/petSettings.test.tsx — 设置页桌宠分区控件与开关行为（spec §10.2/D8）
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import SettingsPage from "@/pages/settings";
import { invoke } from "@tauri-apps/api/core";
import { loadVisible, loadConfig } from "@/components/pet/petConfig";

describe("settings 桌宠分区", () => {
  beforeEach(() => {
    localStorage.clear();
    vi.mocked(invoke).mockClear();
  });

  it("切到桌宠分区：三个控件齐备（开启/置顶/大小）", () => {
    render(<SettingsPage />);
    fireEvent.click(screen.getByText("Pet")); // i18n 默认 en
    expect(screen.getByText("Enable pet")).toBeTruthy();
    expect(screen.getByText("Always on top")).toBeTruthy();
    expect(screen.getByText("Size")).toBeTruthy();
  });

  it("开启开关：写 localStorage 并调用 set_pet_visible", async () => {
    render(<SettingsPage />);
    fireEvent.click(screen.getByText("Pet"));
    const toggles = screen.getAllByRole("switch");
    fireEvent.click(toggles[0]); // 分区内第一个 Switch = 开启桌宠
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("set_pet_visible", { visible: true }));
    expect(loadVisible()).toBe(true);
  });

  it("大小三档：点 Large 写 scale=1.25", async () => {
    render(<SettingsPage />);
    fireEvent.click(screen.getByText("Pet"));
    fireEvent.click(screen.getByText("Large"));
    await waitFor(() => expect(loadConfig().scale).toBe(1.25));
  });
});
```

- [x] **Step 2: useNotification 让渡（两处修改）**

`src/hooks/useNotification.ts`：

import 增加：

```ts
import { petSoundTakeover, petSuppressPopup } from "@/components/pet/petConfig";
```

变绿播声处（原 `if (currColor === "green") playCompletionSound(session.agentType);`）改为：

```ts
        // 宠物开启即接管完成提示音（静音则整体静默，spec D3）
        if (currColor === "green" && !petSoundTakeover()) playCompletionSound(session.agentType);
```

浮窗发送块（原 `try { await invoke("show_notification_window", {...}) }` 外包一层判定）：

```ts
        // 宠物置顶时抑制浮窗：头顶状态栏常显（spec D4）；历史与 toast 降级不受影响
        if (!petSuppressPopup()) {
          try {
            await invoke("show_notification_window", {
              payload: {
                agentType: session.agentType,
                agentLabel: toolLabel,
                projectName: session.projectName,
                statusColor: statusToColor(session.status),
                status: session.status,
                lastMessage: session.lastMessage ?? "",
                pid: session.pid,
                sessionId: session.id,
              },
            });
          } catch (e) {
            // 浮窗失败降级系统 toast，保证通知不丢（spec 008 错误处理要求）
            console.error("show_notification_window failed:", e);
            if (permissionGranted.current) {
              sendNotification({
                title: `${toolLabel} — ${session.projectName}`,
                body: `${statusLabel}${session.lastMessage ? ": " + session.lastMessage.slice(0, 80) : ""}`,
                actionTypeId: "focus-session",
                extra: {
                  pid: session.pid,
                  sessionId: session.id,
                  agentType: session.agentType,
                  projectName: session.projectName,
                  lastMessage: session.lastMessage ?? "",
                },
              });
            }
          }
        }
```

（`addHistory` 调用保持在判定之前，任何情况都记录。）

- [x] **Step 3: home.tsx 🦊 按钮**

状态摘要栏（`<NotificationBell />` 旁）增加：

```tsx
import { loadVisible, saveVisible, subscribeConfig } from "@/components/pet/petConfig";
import { invoke } from "@tauri-apps/api/core";

  const [petOn, setPetOn] = useState(() => loadVisible());
  useEffect(() => subscribeConfig(() => setPetOn(loadVisible())), []);

  const togglePet = async () => {
    const next = !petOn;
    saveVisible(next);
    setPetOn(next);
    try {
      await invoke("set_pet_visible", { visible: next });
    } catch (e) {
      console.error("set_pet_visible failed:", e);
    }
  };
```

```tsx
        <button
          onClick={togglePet}
          title={t("home.petToggle")}
          className={`rounded px-2 py-1 text-sm transition-colors ${petOn ? "" : "opacity-45 grayscale"}`}
        >
          🦊
        </button>
```

（放在标签栏右侧 `<NotificationBell />` 之前。）

- [x] **Step 4: settings.tsx 桌宠分区**

`SettingSection` 类型加 `"pet"`；`menuItems` 追加：

```tsx
    {
      id: "pet" as SettingSection,
      label: t("settings.pet.title"),
      icon: Dog, // lucide-react 图标（import { Dog } from "lucide-react"）
    },
```

分区渲染（与 notifications 同构）：

```tsx
          {activeSection === "pet" && (
            <div className="space-y-6">
              <div>
                <h2 className="mb-1 text-lg font-semibold">{t("settings.pet.title")}</h2>
                <p className="text-muted-foreground text-sm">{t("settings.pet.desc")}</p>
              </div>
              {/* 开启开关 */}
              <div className="flex items-center justify-between">
                <label className="text-sm font-medium">{t("settings.pet.enable")}</label>
                <Switch checked={petVisible} onCheckedChange={onPetVisibleChange} />
              </div>
              {/* 置顶开关 */}
              <div className="flex items-center justify-between">
                <label className="text-sm font-medium">{t("settings.pet.alwaysOnTop")}</label>
                <Switch checked={petCfg.alwaysOnTop} onCheckedChange={(v) => onPetCfgChange({ alwaysOnTop: v })} />
              </div>
              {/* 大小三档 */}
              <div className="flex items-center justify-between">
                <label className="text-sm font-medium">{t("settings.pet.scale")}</label>
                <div className="flex gap-1">
                  {PET_SCALES.map((s) => (
                    <button key={s} onClick={() => onPetCfgChange({ scale: s })}
                      className={`rounded px-3 py-1 text-sm ${petCfg.scale === s ? "bg-accent text-accent-foreground" : "text-muted-foreground hover:bg-accent/50"}`}>
                      {s === 0.75 ? t("pet.scale.small") : s === 1 ? t("pet.scale.medium") : t("pet.scale.large")}
                    </button>
                  ))}
                </div>
              </div>
            </div>
          )}
```

组件内状态与处理器（复用 petConfig；`Switch` 用项目现有 shadcn/ui 组件，参照 notifications 分区用法）：

```tsx
  const [petVisible, setPetVisible] = useState(() => loadVisible());
  const [petCfg, setPetCfg] = useState(() => loadConfig());
  useEffect(() => subscribeConfig(() => { setPetVisible(loadVisible()); setPetCfg(loadConfig()); }), []);

  const onPetVisibleChange = async (v: boolean) => {
    saveVisible(v);
    setPetVisible(v);
    try {
      await invoke("set_pet_visible", { visible: v });
    } catch (e) {
      console.error("set_pet_visible failed:", e);
    }
    toast.success(v ? t("settings.pet.enabledToast") : t("settings.pet.disabledToast"));
  };
  const onPetCfgChange = (patch: Partial<PetConfig>) => {
    saveConfig(patch);
    setPetCfg(loadConfig());
    if (patch.alwaysOnTop !== undefined) {
      invoke("set_pet_always_on_top", { onTop: patch.alwaysOnTop }).catch(() => {});
    }
  };
```

i18n 增补（zh/en 同步）：`home.petToggle`: "显示/隐藏桌宠" / "Show/hide pet"；`settings.pet.title`: "桌宠" / "Pet"；`settings.pet.desc`: "Foxbell 桌宠：悬浮状态卡片与语音提醒" / "Foxbell pet: floating status cards and voice alerts"；`settings.pet.enable`: "开启桌宠" / "Enable pet"；`settings.pet.alwaysOnTop`: "悬浮在所有程序最前" / "Always on top"；`settings.pet.scale`: "大小" / "Size"；`settings.pet.enabledToast`: "桌宠已开启" / "Pet enabled"；`settings.pet.disabledToast`: "桌宠已隐藏" / "Pet hidden"。

- [x] **Step 5: 运行测试与 i18n 校验**

Run: `pnpm vitest run tests/pet/ && pnpm check:i18n`
Expected: 全部 PASS

- [x] **Step 6: Lint + Commit**

Run: `pnpm lint`
```bash
git add src/hooks/useNotification.ts src/pages/home.tsx src/pages/settings.tsx src/i18n/locales tests/pet
git commit -m "feat(pet): integrate takeover of sound and popup suppression, add toggle button and settings section"
```

---

### Task 14: 托盘菜单 + 显隐事件同步

**Files:**
- Modify: `src-tauri/src/plugins/system_tray.rs`、`src-tauri/src/lib.rs`（`update_tray_menu` 透传 pet 文案）
- Modify: `src/pages/home.tsx`（`update_tray_menu` 调用加 pet 文案参数）
- Test: `cd src-tauri && cargo test`（现有基线全绿）+ `cargo clippy`

**Interfaces:**
- Consumes: Task 7 窗口（`app.get_webview_window("pet")`）。
- Produces: 托盘菜单项 id `"pet"`；切换时 Rust `emit("pet-visibility-changed", { visible })`（PetPage 已监听）。

- [x] **Step 1: system_tray.rs 修改**

`update_tray_menu` 签名扩展（原两参变三参，两处 `Menu::with_id_and_items` 同步）：

```rust
pub fn update_tray_menu(
    app: &AppHandle,
    show_text: &str,
    quit_text: &str,
    pet_text: &str,
) -> Result<(), String> {
    let menu = Menu::with_id_and_items(
        app,
        "system-tray",
        &[
            &MenuItem::with_id(app, "show", show_text, true, None::<&str>)
                .map_err(|e| e.to_string())?,
            &MenuItem::with_id(app, "pet", pet_text, true, None::<&str>)
                .map_err(|e| e.to_string())?,
            &PredefinedMenuItem::separator(app).map_err(|e| e.to_string())?,
            &MenuItem::with_id(app, "quit", quit_text, true, None::<&str>)
                .map_err(|e| e.to_string())?,
        ],
    )
    .map_err(|e| e.to_string())?;

    if let Some(tray) = app.tray_by_id("main-tray") {
        tray.set_menu(Some(menu)).map_err(|e| e.to_string())?;
    }
    Ok(())
}
```

`init()` 里初始菜单同样加 pet 项（默认 "Show Pet"）；`on_menu_event` 增加：

```rust
                    "pet" => {
                        // 托盘切换桌宠显隐：以窗口实际可见性为准，并广播给前端同步（spec §10.2）
                        if let Some(w) = app.get_webview_window("pet") {
                            let visible = w.is_visible().unwrap_or(false);
                            let next = !visible;
                            if next {
                                let _ = w.show();
                            } else {
                                let _ = w.hide();
                            }
                            let _ = app.emit("pet-visibility-changed", serde_json::json!({ "visible": next }));
                        }
                    }
```

（文件顶部 `use tauri::Emitter;` 若未有则加；serde_json 已是依赖。）

- [x] **Step 2: lib.rs 透传文案**

`update_tray_menu` command 签名加 `pet_text: String` 并透传；`system_tray::update_tray_menu(&app, &show_text, &quit_text, &pet_text)`。

- [x] **Step 3: home.tsx 调用处**

`invoke("update_tray_menu", { showText: t("tray.show"), quitText: t("tray.quit") })` 改为：

```ts
      await invoke("update_tray_menu", {
        showText: t("tray.show"),
        quitText: t("tray.quit"),
        petText: petOn ? t("tray.petHide") : t("tray.petShow"),
      });
```

（依赖 Step 3 的 `petOn` 状态；`petOn` 变化的 `useEffect` 里也重刷托盘文案。）i18n：`tray.petShow`: "显示桌宠"/"Show Pet"；`tray.petHide`: "隐藏桌宠"/"Hide Pet"。

- [x] **Step 4: Rust 验证**

Run: `cd src-tauri && cargo check && cargo test && cargo clippy --all-targets -- -D warnings`
Expected: 全绿（cargo test 基线 96+ 通过，无新增失败/告警）

- [x] **Step 5: Commit**

```bash
git add src-tauri/src/plugins/system_tray.rs src-tauri/src/lib.rs src/pages/home.tsx src/i18n/locales
git commit -m "feat(pet): add tray toggle with visibility event sync"
```

---

### Task 15: 全量验证与人工验收清单

**Files:**
- Modify: `docs/engineering/specs/2026-09-01-foxbell-pet-design.md`（末尾附验收记录，不改动设计内容）

- [x] **Step 1: 全量自动检查**

Run: `pnpm check`（format:check + lint + check:i18n + build）
Expected: 全绿

Run: `pnpm test`
Expected: 全绿（含既有基线与新增 tests/pet/）

Run: `cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings`
Expected: 全绿

- [x] **Step 2: `pnpm tauri:dev` 人工验收（spec §9 清单逐条）**

按 spec §14 验收清单执行并记录（通过/不通过 + 备注），重点：

1. 设置页开启桌宠 → 右下角出现，idle 动画逐帧步进；🦊 按钮与托盘同步状态
2. 空闲 6s 环顾一圈，交互即断
3. 拖动：左/右跑、上跳；松手坠落 + 压扁回弹 + 补跳；gravity 关则停驻
4. 穿透：宠物外透明区点击可透到下层应用；精灵/卡片/菜单可交互
5. 双击说话出字幕（时长对齐）；单击只挥手；静音开→动作有声音无
6. 会话 waiting → 红卡 + approval 语音（10s 限频）；运行中黄卡不发声；完成 → 绿卡 + done 语音
7. 完成时主窗口不播提示音（宠物开启）；宠物置顶时通知浮窗不弹、非置顶照弹
8. 点卡片跳转终端；多候选浮层可选；绿卡点后消失、再次完成重亮
9. 右键菜单：四开关 + 大小三档（等比例缩放精灵/卡片/气泡）+ 三动作子页实时预览 + 隐藏 + 关于
10. 大小切换与卡片增减时窗口底部锚定（精灵不跳动）
11. 位置重启记忆（含夹紧屏幕内）
12. 托盘"显示/隐藏桌宠"切换生效且各入口状态同步

- [x] **Step 3: 记录验收结果并提交**

在 spec 末尾追加"## 17. 验收记录"小节，逐条记录结果。

```bash
git add docs/engineering/specs/2026-09-01-foxbell-pet-design.md
git commit -m "docs(pet): record pet acceptance results"
```

---

## 任务依赖

```
T1 ─┬─> T5 ─┐
T2 ─┼─> T3 ─┼─> T12
    ├─> T4 ─┤
    └─> T6 ─┴─> T8 ─> T9 ─> T10 ─> T12
T7 ──────────────> T8（路由窗口）/ T13 / T14
T11 ─> T12
T13 / T14 依赖 T7、T2
T15 收尾
```

执行顺序建议：1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10 → 11 → 12 → 13 → 14 → 15（严格顺序，接口依赖已对齐）。
