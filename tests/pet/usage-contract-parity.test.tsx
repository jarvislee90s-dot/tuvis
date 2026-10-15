// 用量契约对账（计划② Task 15 步骤 2）：**真实后端返回**的四份 JSON ↔ `src/types/usage.ts` 的 TS 镜像。
//
// 分两组，语义刻意不同（**这不是失败，是提醒**）：
//
//  * **第一组**（5 条）：拿 `tests/fixtures/usage/` 里人类在 DevTools 抓下来的**真实返回**逐字对账。
//    目录里四份 fixture **不齐备就整组 skip**（`describe.skipIf`）——空目录时这是「还没联调」的
//    正确状态，不是红灯；但也**绝不能假绿**：半套（抓了几份、缺几份）由第二组响亮打红。
//    抓取方式 = 同目录 `README.md`（唯一出处）。
//  * **第二组**（1 条）：**联调前的常绿组**。在没有任何 fixture 时也要钉住「必需字段表 ↔ 前端类型
//    镜像」这件事本身：拿 `@/lib/usage/mockFixtures` 的同源夹具喂**同一份**断言函数。
//    夹具在 `src/` 内、被 `tsc` 按 `UsageDashboard` / `UsageRecords` / `UsageSettings` /
//    `UsageCollectResult` 逐字校验 ⇒「mock 的键集 == 必需表」等价于「接口的键集 == 必需表」，
//    REQUIRED 表因此不可能悄悄漏掉一个字段（漏了 `recentSession` = 浮窗第 ④ 行永远空白，
//    而第一组在空目录下是 skip 的，没人会看见）。fixture 齐备后本组整组 skip。
//
// 纪律（brief 步骤 3）：第一组**红**时按报错字段名改**消费点**（`src/types/usage.ts` 与 `t()` 用法），
// **不改后端也不改契约**——契约是冻结件，漂移一定是单方面违约，此时停下报告。
//
// 隐私（spec P8）：fixture 是**真机数据**，不得含会话正文 / 密钥；本文件用字面量扫描兜底
// （`lastMessage` / `prompt` / `apiKey` / `Bearer `）。
import { describe, expect, it } from "vitest";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import {
  mockUsageCollect,
  mockUsageDashboard,
  mockUsageRecords,
  mockUsageSettings,
} from "@/lib/usage/mockFixtures";
import type {
  UsageCollectResult,
  UsageDashboard,
  UsageMetric,
  UsageRecords,
  UsageSettings,
} from "@/types/usage";

/** 契约 §2 的 12 个可得性指标值（类型桥：写错一个 → `readonly UsageMetric[]` 不过） */
const USAGE_METRICS: readonly UsageMetric[] = [
  "sessions",
  "turn",
  "errorModel",
  "errorTurn",
  "errorTool",
  "interrupted",
  "longestTurn",
  "toolCalls",
  "toolAvgMs",
  "topTool",
  "topToolMs",
  "userEst",
];

/** 契约 §2 的 7 个采集源 id（与 ① 的 parity 断同集合） */
const SOURCE_IDS = ["claude", "codex", "kimi", "opencode", "workbuddy", "zcode", "dsh"];

/** 设置项 `miniBarRange` 的三档（Rust `MiniBarRange` 的 camelCase 序列化形态） */
const MINI_BAR_RANGES = ["today", "last5h", "last7d"];

/** 隐私红线（spec P8）：fixture 里出现任一即红 */
const PRIVACY_LITERALS = ["lastMessage", "prompt", "apiKey", "Bearer "] as const;

/** 逐字取自契约 / `src/types/usage.ts` 的键集（多一个键与少一个键都算漂移） */
const ROW_KEYS = ["key", "label", "buckets", "metrics", "sourceKind", "isSubagent"];
const METRIC_KEYS = ["requestTotal", "cacheHitRate", "userEst", "requests"];
const BUCKET_KEYS = ["inputFresh", "cacheRead", "cacheWrite", "output"];
const TREND_KEYS = ["key", "label", "buckets", "metrics"];
const RECENT_KEYS = ["sourceId", "sessionId", "title", "buckets", "metrics"];
const CARD_KEYS = ["toolId", "toolLabel", "buckets", "metrics", "rows"];
/**
 * brief 步骤 2 的「`workSummary` 六个 key 在场」= 会话数（`sessions` / `turnsPerTool`）
 * + D18 三层报错与用户打断（四行**分列不合并**）。
 */
const WORK_SUMMARY_SIX = [
  "sessions",
  "turnsPerTool",
  "errorModel",
  "errorTurn",
  "errorTool",
  "interrupted",
];
/**
 * 其余五个（工具统计 / Top 工具 / D19 逐工具最长 turn）：Rust `WorkSummary` 的字段全是裸 `pub`、
 * **没有 `skip_serializing_if`** ⇒ 同样必须显式在场（`null` 是值，不是缺键）。brief 只要求六个，
 * 这里一并钉住：把「不可得」误写成「缺键」是同一类漂移。
 */
const WORK_SUMMARY_REST = ["toolCalls", "toolAvgMs", "topTool", "topToolMs", "longestTurnPerTool"];
const SOURCE_STATUS_KEYS = ["sourceId", "ok", "parsedFiles", "newRecords"];

/** fixture 目录与四个文件名（抓取方式见同目录 README.md） */
const FIXTURE_DIR = path.join(process.cwd(), "tests/fixtures/usage");
const FIXTURE_FILES = {
  dashboard: "dashboard.last7d.json",
  records: "records.project.json",
  settings: "settings.json",
  collect: "collect.json",
} as const;
type FixtureKind = keyof typeof FIXTURE_FILES;
const FIXTURE_KINDS = Object.keys(FIXTURE_FILES) as FixtureKind[];

const fixturePath = (k: FixtureKind) => path.join(FIXTURE_DIR, FIXTURE_FILES[k]);
/** 目录里**实际存在**的 fixture（只认这四个文件名；README 与别的文件不参与判定） */
const present = FIXTURE_KINDS.filter((k) => existsSync(fixturePath(k)));
const missing = FIXTURE_KINDS.filter((k) => !present.includes(k));
/** 四份齐备才跑第一组：**半套不跑**（缺一份整组 skip，由第二组打红，绝不半绿） */
const HAS_ALL_FIXTURES = missing.length === 0;

function readFixture<T>(k: FixtureKind): T {
  return JSON.parse(readFileSync(fixturePath(k), "utf8")) as T;
}

// ---------------------------------------------------------------------------
// 共用断言（两组跑**同一份**判据：第一组喂真实返回，第二组喂同源 mock）
// ---------------------------------------------------------------------------

/**
 * 键名列表的**类型桥**：`keyof T` 逼 `src/types/usage.ts` 真有这些字段
 * （镜像改名 / 拼错 → 编辑器与 `tsc` 报错；运行时保险见文件头第二组的说明）。
 */
function requiredKeysOf<T>(keys: readonly (keyof T)[]): string[] {
  return [...(keys as readonly string[])].sort();
}

const sortedKeys = (o: unknown): string[] => Object.keys(o as Record<string, unknown>).sort();

/** 键集**逐字相等**（不是「包含」：fixture 多出契约没有的键同样是漂移） */
function expectExactKeys(actual: unknown, required: readonly string[], what: string): void {
  expect(sortedKeys(actual), `${what}：键集与必需字段表不一致（契约是冻结件）`).toEqual(
    [...required].sort()
  );
}

const isInt = (v: unknown): boolean => typeof v === "number" && Number.isInteger(v);
const isNumOrNull = (v: unknown): boolean => v === null || typeof v === "number";

function assertBuckets(b: unknown, what: string): void {
  expectExactKeys(b, BUCKET_KEYS, what);
  for (const k of BUCKET_KEYS) {
    expect(isInt((b as Record<string, unknown>)[k]), `${what}.${k} 必须是整数`).toBe(true);
  }
}

function assertMetrics(m: unknown, what: string): void {
  expectExactKeys(m, METRIC_KEYS, what);
  const x = m as Record<string, unknown>;
  expect(isInt(x.requestTotal), `${what}.requestTotal 必须是整数`).toBe(true);
  expect(typeof x.cacheHitRate, `${what}.cacheHitRate 是 0–1 分数（不是百分数）`).toBe("number");
  expect(x.cacheHitRate as number, `${what}.cacheHitRate 必须落在 [0,1]`).toBeGreaterThanOrEqual(0);
  expect(x.cacheHitRate as number, `${what}.cacheHitRate 必须落在 [0,1]`).toBeLessThanOrEqual(1);
  // 不可得 = 显式 `null`（R1：不得填 0，也不得缺键）
  expect(
    isNumOrNull(x.userEst),
    `${what}.userEst 必须是 number | null（不可得给 null，不给 0、不缺键）`
  ).toBe(true);
  expect(isInt(x.requests), `${what}.requests 必须是整数`).toBe(true);
}

/** `UsageRow` 形状 + `isSubagent` 的**档位分叉**（本文件最容易被漏掉的一条） */
function assertRow(row: unknown, what: string, dayTier: boolean): void {
  expectExactKeys(row, ROW_KEYS, what);
  const r = row as Record<string, unknown>;
  expect(typeof r.key, `${what}.key 必须是字符串`).toBe("string");
  expect(typeof r.label, `${what}.label 必须是字符串`).toBe("string");
  assertBuckets(r.buckets, `${what}.buckets`);
  assertMetrics(r.metrics, `${what}.metrics`);
  expect(["measured", "inferred", "unknown"], `${what}.sourceKind 三态之一`).toContain(
    r.sourceKind
  );
  const sub = r.isSubagent;
  if (dayTier) {
    // 契约 §2（2026-10-03 裁决）：日档与记录页卡内行的行维度不含 `session_id` ⇒ **算不出 = null**。
    // 写 `false` = 谎称「不含子代理」（与 userEst 的「不可得 ≠ 0」同一逻辑）。
    expect(sub, `${what}.isSubagent：日档必须 null（不得回落 false）`).toBeNull();
  } else {
    expect(typeof sub, `${what}.isSubagent：小时档必须是真布尔（真值才承载 D17 分层语义）`).toBe(
      "boolean"
    );
  }
}

function assertAvailability(list: unknown, what: string): void {
  expect(Array.isArray(list), `${what} 必须是数组`).toBe(true);
  const entries = list as unknown[];
  // **只断入场条目的形状**：① 运行时只出 8 条，缺 `sessions`/`toolAvgMs`/`topTool`/`topToolMs`
  // 是合法的——**缺条目 ≠ 不可得**，空态由**值本身为 `null`** 驱动（brief 关键坑第 2 条）。
  // 故**不得**断言 12 个 metric 全在场。
  for (const [i, a] of entries.entries()) {
    const av = a as Record<string, unknown>;
    expect(USAGE_METRICS, `${what}[${i}].metric 必须在契约 12 值内`).toContain(av.metric);
    expect(typeof av.available, `${what}[${i}].available 必须是布尔`).toBe("boolean");
    if (av.reason !== undefined) {
      expect(typeof av.reason, `${what}[${i}].reason 给了就必须是字符串`).toBe("string");
    }
    if (av.perSource !== undefined) {
      expect(typeof av.perSource, `${what}[${i}].perSource 给了就必须是「源 → 布尔」映射`).toBe(
        "object"
      );
      for (const v of Object.values(av.perSource as Record<string, unknown>)) {
        expect(typeof v, `${what}[${i}].perSource 的值必须是布尔`).toBe("boolean");
      }
    }
  }
}

function assertWorkSummary(ws: unknown, what: string): void {
  const w = ws as Record<string, unknown>;
  // 指标级「六个 key 在场」（brief 步骤 2）
  for (const k of WORK_SUMMARY_SIX) {
    expect(k in w, `${what} 缺 key \`${k}\``).toBe(true);
  }
  // 其余五个：Rust 侧无 skip ⇒ 同样必须显式在场
  for (const k of WORK_SUMMARY_REST) {
    expect(k in w, `${what} 缺 key \`${k}\`（Rust 侧无 skip_serializing_if）`).toBe(true);
  }
  // 值域：不可得一律 `null`，不得缺键、不得填 0 冒充
  for (const k of [
    "sessions",
    "errorModel",
    "errorTurn",
    "errorTool",
    "interrupted",
    "toolCalls",
    "toolAvgMs",
  ]) {
    expect(isNumOrNull(w[k]), `${what}.${k} 必须是 number | null`).toBe(true);
  }
  expect(typeof w.turnsPerTool, `${what}.turnsPerTool 是「工具 → number|null」映射`).toBe("object");
  for (const [tool, v] of Object.entries(w.turnsPerTool as Record<string, unknown>)) {
    expect(
      isNumOrNull(v),
      `${what}.turnsPerTool.${tool} 必须是 number | null（无回合概念 = null）`
    ).toBe(true);
  }
  expect(typeof w.longestTurnPerTool, `${what}.longestTurnPerTool 是「工具 → p50·max|null」`).toBe(
    "object"
  );
  for (const [tool, v] of Object.entries(w.longestTurnPerTool as Record<string, unknown>)) {
    if (v !== null) {
      expectExactKeys(v, ["p50", "max"], `${what}.longestTurnPerTool.${tool}`);
    }
  }
  if (w.topTool !== null) {
    expectExactKeys(w.topTool, ["name", "count"], `${what}.topTool`);
  }
  if (w.topToolMs !== null) {
    expectExactKeys(w.topToolMs, ["name", "ms"], `${what}.topToolMs`);
  }
}

function assertDashboard(d: UsageDashboard, what: string): void {
  // 12 个必需字段（`recentSession` 在内：浮窗第 ④ 行的唯一数据源，漏列 = fixture 缺它也照样「绿」）
  expectExactKeys(d, DASHBOARD_REQUIRED, what);
  expect(d.range.preset, `${what}.range.preset 必须是契约五档之一`).toMatch(
    /^(last5h|today|last7d|last30d|custom)$/
  );
  // 档位分叉：`last5h` / `today` = 小时档（契约 §2 的 `RangeGranularity`），其余三档 = 日档
  const dayTier = d.range.preset !== "last5h" && d.range.preset !== "today";
  expect(Array.isArray(d.rows), `${what}.rows 必须是数组`).toBe(true);
  expect(
    d.rows.length,
    `${what}.rows 为空：fixture 的价值就是用**真数据**钉住行形状，请换一个有数据的档位重抓（属抓取质量问题，不是契约漂移）`
  ).toBeGreaterThan(0);
  for (const [i, row] of d.rows.entries()) {
    assertRow(row, `${what}.rows[${i}]`, dayTier);
  }
  assertMetrics(d.totals, `${what}.totals`);
  assertBuckets(d.totalsBuckets, `${what}.totalsBuckets`);
  // hero = 请求输入 + 产出（说明书 P1 第 2 条；后端 `semantics::hero_of`）
  expect(d.hero, `${what}.hero ≠ totals.requestTotal + totalsBuckets.output`).toBe(
    (d.totals.requestTotal as number) + (d.totalsBuckets.output as number)
  );
  // 命中率是**同一个分式**（后端 `semantics::cache_hit_rate`）：不是百分数、不另行取整
  const expectedRate =
    d.totals.requestTotal > 0 ? d.totalsBuckets.cacheRead / d.totals.requestTotal : 0;
  expect(
    d.totals.cacheHitRate as number,
    `${what}.totals.cacheHitRate ≠ cacheRead ÷ requestTotal`
  ).toBeCloseTo(expectedRate, 10);
  expect(Array.isArray(d.trend), `${what}.trend 必须是数组`).toBe(true);
  expect(d.trend.length, `${what}.trend 为空：该档位一个桶都没有`).toBeGreaterThan(0);
  for (const [i, p] of d.trend.entries()) {
    expectExactKeys(p, TREND_KEYS, `${what}.trend[${i}]`);
    assertBuckets(p.buckets, `${what}.trend[${i}].buckets`);
    assertMetrics(p.metrics, `${what}.trend[${i}].metrics`);
  }
  // 环比：`| null` 且必须**显式**给值（缺键 = 违约）
  if (d.compare !== null) {
    expectExactKeys(d.compare, ["prevBuckets", "prevMetrics"], `${what}.compare`);
    assertBuckets(d.compare.prevBuckets, `${what}.compare.prevBuckets`);
    assertMetrics(d.compare.prevMetrics, `${what}.compare.prevMetrics`);
  }
  // 「本会话」：日档**恒 null**（`query.rs::recent_session_with` 的 Day 分支）；非 null 时五字段齐备
  if (dayTier) {
    expect(
      d.recentSession,
      `${what}.recentSession：日档后端恒 null（日聚合行不带 session_id）。若真机给了值，说明档位口径漂了 → 停下报告，不要改这里的断言`
    ).toBeNull();
  } else if (d.recentSession !== null) {
    expectExactKeys(d.recentSession, RECENT_KEYS, `${what}.recentSession`);
    assertBuckets(d.recentSession.buckets, `${what}.recentSession.buckets`);
    assertMetrics(d.recentSession.metrics, `${what}.recentSession.metrics`);
    expect("title" in d.recentSession, `${what}.recentSession.title 必须在场（值可为 null）`).toBe(
      true
    );
  }
  assertWorkSummary(d.workSummary, `${what}.workSummary`);
  assertAvailability(d.availability, `${what}.availability`);
  expect(isInt(d.collectedAt), `${what}.collectedAt 必须是整数（0 = 尚未采集哨兵）`).toBe(true);
  expect(
    d.collectedAt as number,
    `${what}.collectedAt === 0：抓取前**先跑一次采集**（见同目录 README 的顺序），否则拿到的是「尚未采集」哨兵`
  ).toBeGreaterThan(0);
}

function assertRecords(r: UsageRecords, what: string): void {
  expectExactKeys(r, RECORDS_REQUIRED, what);
  expect(r.groupBy, `${what}.groupBy 必须与文件名一致（本 fixture = project）`).toBe("project");
  expect(Array.isArray(r.cards), `${what}.cards 必须是数组`).toBe(true);
  expect(r.cards.length, `${what}.cards 为空：卡片的行维度是真数据才钉得住`).toBeGreaterThan(0);
  for (const [i, card] of r.cards.entries()) {
    const at = `${what}.cards[${i}]`;
    expectExactKeys(card, CARD_KEYS, at);
    // 项目卡：键是**小写规范化**项目键（D21），显示名保留原文大小写
    expect(card.toolId, `${at}.toolId 必须非空`).not.toBe("");
    expect(card.toolId, `${at}.toolId 必须是 D21 的小写规范化项目键`).toBe(
      card.toolId.toLowerCase()
    );
    expect(card.toolLabel, `${at}.toolLabel（卡片显示名）必须非空`).not.toBe("");
    expect(Array.isArray(card.rows), `${at}.rows 必须是数组`).toBe(true);
    expect(card.rows.length, `${at}.rows 为空：卡片由行聚合而来，不该有空卡`).toBeGreaterThan(0);
    for (const [j, row] of card.rows.entries()) {
      // 记录页**卡内行**的维度恒为「供应商 / 模型」（D6），与卡片分组解耦（D7）；
      // 其 `isSubagent` **两档都算不出** → 恒 null（`query.rs` 的 `is_subagent: None`）。
      assertRow(row, `${at}.rows[${j}]`, true);
      expect(
        [card.toolId, card.toolLabel],
        `${at}.rows[${j}].label === 卡片名：卡内行跟着卡片维度变了（D6/D7 分工被破坏）`
      ).not.toContain(row.label);
    }
  }
  assertAvailability(r.availability, `${what}.availability`);
  expect(isInt(r.collectedAt), `${what}.collectedAt 必须是整数（0 = 尚未采集哨兵）`).toBe(true);
}

function assertSettings(s: UsageSettings, what: string): void {
  expectExactKeys(s, SETTINGS_REQUIRED, what);
  expect(typeof s.enabled, `${what}.enabled 必须是布尔（总开关）`).toBe("boolean");
  expect(MINI_BAR_RANGES, `${what}.miniBarRange 必须是浮窗三档之一`).toContain(s.miniBarRange);
  const bounds: ReadonlyArray<readonly [keyof UsageSettings, number, number]> = [
    // 边界逐字来自 Rust `settings::merge_patch` 的校验（越界一律 `usage-settings-invalid`，不静默夹取）
    ["miniBarToolRows", 1, 7],
    ["detailRetentionDays", 1, 3650],
    ["collectIntervalMin", 1, 1440],
  ];
  for (const [k, lo, hi] of bounds) {
    const v = s[k] as unknown;
    expect(isInt(v), `${what}.${k} 必须是整数`).toBe(true);
    expect(
      v as number,
      `${what}.${k} 必须落在 ${lo}–${hi}（与 Rust 校验同边界）`
    ).toBeGreaterThanOrEqual(lo);
    expect(
      v as number,
      `${what}.${k} 必须落在 ${lo}–${hi}（与 Rust 校验同边界）`
    ).toBeLessThanOrEqual(hi);
  }
  expect(typeof s.providerMapRules, `${what}.providerMapRules 必须是字符串（JSON 文本）`).toBe(
    "string"
  );
  expect(typeof s.exportQuote, `${what}.exportQuote 必须是字符串（可为空串）`).toBe("string");
  expect(typeof s.exportPose, `${what}.exportPose 必须是字符串`).toBe("string");
  expect(
    s.exportPose.trim(),
    `${what}.exportPose 不得为空串（随机的合法写法是 "random"）`
  ).not.toBe("");
}

function assertCollect(c: UsageCollectResult, what: string): void {
  expectExactKeys(c, COLLECT_REQUIRED, what);
  expect(isInt(c.collectedAt), `${what}.collectedAt 必须是整数`).toBe(true);
  expect(c.collectedAt, `${what}.collectedAt 必须 > 0（本轮采集的时刻）`).toBeGreaterThan(0);
  expect(isInt(c.durationMs), `${what}.durationMs 必须是整数`).toBe(true);
  expect(c.durationMs, `${what}.durationMs 不得为负`).toBeGreaterThanOrEqual(0);
  expect(Array.isArray(c.sources), `${what}.sources 必须是数组`).toBe(true);
  expect(
    c.sources.length,
    `${what}.sources 不是 7 源：若是 0，先确认设置页「用量统计」总开关为开（关闭时后端在读游标前早退，返回空 sources——那是另一状态，不是契约违约）；抓取前请把开关打开`
  ).toBe(7);
  let sum = 0;
  for (const [i, st] of c.sources.entries()) {
    const at = `${what}.sources[${i}]`;
    expect(SOURCE_IDS, `${at}.sourceId 必须是 7 个采集源之一`).toContain(st.sourceId);
    expect(typeof st.ok, `${at}.ok 必须是布尔`).toBe("boolean");
    expect(isInt(st.parsedFiles), `${at}.parsedFiles 必须是整数`).toBe(true);
    expect(isInt(st.newRecords), `${at}.newRecords 必须是整数`).toBe(true);
    // 成功源**不得有 `errorCode` 键**（Rust 侧 `skip_serializing_if = "Option::is_none"`）；
    // 失败源必须有码——「静默失败」是契约明令禁止的
    if (st.ok) {
      expect(
        sortedKeys(st),
        `${at} 是成功源，不得带 errorCode 键（Rust 侧 skip_serializing_if）`
      ).toEqual([...SOURCE_STATUS_KEYS].sort());
    } else {
      expect(typeof st.errorCode, `${at} 是失败源，必须有 errorCode（不得静默失败）`).toBe(
        "string"
      );
    }
    sum += st.newRecords;
  }
  // `totalNewRecords` = 各源 `newRecords` 之和（Rust `collect_sources` 的 `total += status.new_records`）
  expect(c.totalNewRecords, `${what}.totalNewRecords ≠ 各源 newRecords 之和`).toBe(sum);
}

/** 隐私扫描：**raw 文本**扫（键名与值一视同仁，最不容易被绕过） */
function privacyHits(k: FixtureKind): string[] {
  const raw = readFileSync(fixturePath(k), "utf8");
  return PRIVACY_LITERALS.filter((lit) => raw.includes(lit));
}

// ---------------------------------------------------------------------------
// 必需字段表（brief 步骤 2 逐字）
// ---------------------------------------------------------------------------
const DASHBOARD_REQUIRED = requiredKeysOf<UsageDashboard>([
  "range",
  "groupBy",
  "rows",
  "totals",
  "totalsBuckets",
  "hero",
  "trend",
  "compare",
  "workSummary",
  // ⚠️ `recentSession` **不可漏列**：浮窗第 ④ 行的唯一数据源。漏了它，fixture 缺这个字段
  // 也照样「绿」，实机才发现那行永远空白（brief 原话）。
  "recentSession",
  "availability",
  "collectedAt",
]);
const RECORDS_REQUIRED = requiredKeysOf<UsageRecords>([
  "range",
  "groupBy",
  "cards",
  "availability",
  "collectedAt",
]);
const SETTINGS_REQUIRED = requiredKeysOf<UsageSettings>([
  // 8 项逐一（契约 §P7）
  "enabled",
  "miniBarRange",
  "miniBarToolRows",
  "detailRetentionDays",
  "collectIntervalMin",
  "providerMapRules",
  "exportQuote",
  "exportPose",
]);
const COLLECT_REQUIRED = requiredKeysOf<UsageCollectResult>([
  "collectedAt",
  "durationMs",
  "sources",
  "totalNewRecords",
]);

// ---------------------------------------------------------------------------
// 第一组：真实返回对账（fixture 齐备才跑；空目录 = 整组 skip）
// ---------------------------------------------------------------------------
describe.skipIf(!HAS_ALL_FIXTURES)("用量契约对账 · 真实返回（fixture 齐备才跑）", () => {
  it("1. dashboard.last7d.json：12 个必需字段 + 行/趋势/环比/本会话/小结/可得性深断言", () => {
    assertDashboard(readFixture<UsageDashboard>("dashboard"), "dashboard.last7d");
    // 文件名即抓取参数约定（README 步骤 1）：`usage_dashboard({range:{preset:"last7d"},groupBy:"tool"})`
    expect(
      readFixture<UsageDashboard>("dashboard").range.preset,
      "dashboard fixture 的档位不是 last7d：请按 README 的参数重抓（文件名即约定）"
    ).toBe("last7d");
  });

  it("2. records.project.json：5 个必需字段 + 按项目卡片（原文大小写）与卡内行（供应商/模型）分工", () => {
    assertRecords(readFixture<UsageRecords>("records"), "records.project");
  });

  it("3. settings.json：8 个设置项逐一在场 + 取值在契约边界内", () => {
    assertSettings(readFixture<UsageSettings>("settings"), "settings");
  });

  it("4. collect.json：4 个必需字段 + 7 源状态（成功源无 errorCode / 失败源必有码 / 总量 = 各源之和）", () => {
    assertCollect(readFixture<UsageCollectResult>("collect"), "collect");
  });

  it("5. 隐私扫描：四份 fixture 内不得出现 lastMessage / prompt / apiKey / Bearer （spec P8）", () => {
    for (const k of FIXTURE_KINDS) {
      const hits = privacyHits(k);
      expect(
        hits,
        `${FIXTURE_FILES[k]} 命中隐私字面量 ${hits.join("、")}：fixture 是真机数据，不得含会话正文 / 密钥；请重抓（不要手工删除命中内容后入库）`
      ).toEqual([]);
    }
  });
});

// ---------------------------------------------------------------------------
// 第二组：常驻结构检查（联调前常绿；fixture 齐备后整组 skip）
// ---------------------------------------------------------------------------
describe.skipIf(HAS_ALL_FIXTURES)(
  "用量契约对账 · 常驻结构检查（联调前常绿；fixture 齐备后整组 skip）",
  () => {
    it("0. 前端类型镜像 ↔ 同源 mock 夹具：必需字段表逐字相等 + 档位分叉 + 抓取机具在位", () => {
      // ① 半套 fixture 必须**响亮打红**：缺一份就整组 skip，半套看起来像「绿」最危险
      if (present.length > 0) {
        expect(
          missing,
          `${FIXTURE_DIR} 已有 ${present.length} 份 fixture 但缺 ${missing
            .map((k) => FIXTURE_FILES[k])
            .join("、")}：抓齐四份再跑（半套会让第一组整组 skip，不是「通过」）`
        ).toEqual([]);
      }

      // ② 前端类型镜像：mock 在 `src/` 内、被 `tsc` 按 `@/types/usage` 校验
      //    ⇒「mock 键集 == 必需表」等价于「接口键集 == 必需表」；REQUIRED 表因此漏不掉字段。
      //    同一份断言函数（与第一组**完全同判据**）：真实返回换成同源夹具。
      assertDashboard(mockUsageDashboard({ preset: "last7d" }, "tool"), "mock·日档(last7d)");
      // 小时档反向对照：`isSubagent` 必须是**真布尔**（日档那一支在上面必须 null）
      assertDashboard(mockUsageDashboard({ preset: "today" }, "tool"), "mock·小时档(today)");
      assertRecords(mockUsageRecords({ preset: "last7d" }, "project", {}), "mock·记录页(project)");
      assertSettings(mockUsageSettings(), "mock·设置");
      assertCollect(mockUsageCollect(true), "mock·采集");

      // ③ 抓取机具在位：README 是「怎么抓 / 抓到哪 / 禁止写什么」的唯一出处；
      //    它必须点出四个文件名与四条隐私禁令，否则人类无从下手（本组的价值就在于联调前就守住它）。
      const readmePath = path.join(FIXTURE_DIR, "README.md");
      expect(existsSync(readmePath), `${readmePath} 不存在：抓取方式的唯一出处，不能删`).toBe(true);
      const readme = readFileSync(readmePath, "utf8");
      for (const f of Object.values(FIXTURE_FILES)) {
        expect(readme, `README 必须写清 fixture 文件名 \`${f}\``).toContain(f);
      }
      for (const lit of PRIVACY_LITERALS) {
        expect(readme, `README 必须写清隐私禁令字面量 \`${lit}\``).toContain(lit);
      }
    });
  }
);
