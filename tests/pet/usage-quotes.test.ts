// Task 12（计划②）· 评语池纯层 `quotePool.ts` + `quotes.ts` + `lang.ts`（9 用例）。
// 判据来源：brief 步骤 3——双语池**各 10 条**、按序首命中 + 兜底；要素 = 命中率档位（≥0.9 / 0<hit<0.5）、
// 总量档位（≥1000 万 / ≥100 万 / =0 / <10 万）、趋势方向（`trendUp` true/false）、多模型形态
// （`multiModel`）+ 兜底；`fillQuote(tpl, vars)` **只认 4 个占位符**：`{range}` `{tokens}` `{hitPct}`
// `{models}`（池内不得出现这 4 个以外的 `{}` 残留）；`pickQuote(agg, lang, custom?, vars?)` 的 custom
// 非空**完全覆盖**预制池、空白回落预制池；`getLang()` 读 `i18n.language` → `"zh" | "en"`。
//
// 纪律：本文件是**文案资产 + 纯规则匹配**的直测，不渲染组件、不模拟画布；池不进 i18n 键表
// （同一批文案两处维护必漂移），所以这里逐条断言池的形状与命中顺序，而不是断言某个 i18n 键。
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import i18n from "@/i18n";
import { getLang } from "@/lib/usage/lang";
import { QUOTE_POOL, type QuoteAgg } from "@/lib/usage/quotePool";
import { fillQuote, pickQuote, type QuoteVars } from "@/lib/usage/quotes";

/** 基准聚合输入：都不命中特殊档（总量 50 万、命中率 0.7、方向算不出、单线）→ 落到兜底 */
const BASE: QuoteAgg = {
  total: 500_000,
  hitPct: 0.7,
  trendUp: null,
  multiModel: false,
  topTool: null,
};

/** 占位符基准值（4 个，逐字对齐 brief 的清单） */
const VARS: QuoteVars = {
  range: "近 7 天",
  tokens: "1,954,268",
  hitPct: "91.2%",
  models: "3",
};

/** 池内出现的全部花括号占位符名（用于「只认 4 个」的绊线） */
function placeholdersOf(text: string): string[] {
  return [...text.matchAll(/\{([^{}]*)\}/g)].map((m) => m[1]);
}

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

afterAll(async () => {
  // 语言是 i18n 全局态：用完还给 zh（本文件与别的文件同 worker 时不留脏状态）
  await i18n.changeLanguage("zh");
});

describe("quotes（计划② Task 12：评语池 + 填充 + 语言判定）", () => {
  it("1. 池形状：zh/en 各 10 条，文本只含 4 个允许的占位符且 4 个都被用到", () => {
    for (const lang of ["zh", "en"] as const) {
      const pool = QUOTE_POOL[lang];
      expect(pool, `${lang} 池条数`).toHaveLength(10);
      for (const entry of pool) {
        expect(typeof entry.when, `${lang} 谓词`).toBe("function");
        for (const name of placeholdersOf(entry.text)) {
          expect(
            ["range", "tokens", "hitPct", "models"],
            `${lang} 池出现越界占位符 {${name}}`
          ).toContain(name);
        }
      }
      const used = new Set(pool.flatMap((e) => placeholdersOf(e.text)));
      for (const name of ["range", "tokens", "hitPct", "models"]) {
        expect(used.has(name), `${lang} 池没有用到 {${name}}`).toBe(true);
      }
    }
  });

  it("2. 命中率档位：≥0.9 与 0<hit<0.5 各命中一条，且值按 vars 填充", () => {
    const high = pickQuote({ ...BASE, hitPct: 0.93 }, "zh", "", VARS);
    expect(high).toBe(QUOTE_POOL.zh[0].text.split("{hitPct}").join(VARS.hitPct));
    expect(high).toContain("91.2%");

    const cold = pickQuote({ ...BASE, hitPct: 0.3 }, "zh", "", VARS);
    expect(cold).toBe(QUOTE_POOL.zh[1].text.split("{hitPct}").join(VARS.hitPct));

    // 0.5 ≤ hit < 0.9：两档都不命中 → 落到后面的总量档（50 万 < 100 万）→ 兜底
    expect(pickQuote({ ...BASE, hitPct: 0.7 }, "en", "", VARS)).toBe(QUOTE_POOL.en[9].text);
  });

  it("3. 总量四档：≥1000 万 / ≥100 万 / =0 / <10 万 各命中一条", () => {
    const zh = QUOTE_POOL.zh;
    expect(pickQuote({ ...BASE, total: 12_000_000 }, "zh", "", VARS)).toBe(
      zh[2].text.split("{range}").join(VARS.range).split("{tokens}").join(VARS.tokens)
    );
    expect(pickQuote({ ...BASE, total: 1_000_000 }, "zh", "", VARS)).toBe(
      zh[3].text.split("{tokens}").join(VARS.tokens)
    );
    expect(pickQuote({ ...BASE, total: 99_999 }, "zh", "", VARS)).toBe(
      zh[4].text.split("{tokens}").join(VARS.tokens)
    );
    expect(pickQuote({ ...BASE, total: 0 }, "zh", "", VARS)).toBe(
      zh[5].text.split("{range}").join(VARS.range)
    );
  });

  it("4. 趋势方向：trendUp true / false 各命中一条；null（算不出方向）两条都不命中", () => {
    const zh = QUOTE_POOL.zh;
    expect(pickQuote({ ...BASE, trendUp: true }, "zh", "", VARS)).toBe(zh[6].text);
    expect(pickQuote({ ...BASE, trendUp: false }, "zh", "", VARS)).toBe(zh[7].text);
    expect(pickQuote({ ...BASE, trendUp: true }, "en", "", VARS)).toBe(QUOTE_POOL.en[6].text);
    // 末两点相等 / 不足两点（`trendUpOf` 出 null）→ **不冒充**方向，一路落到兜底
    expect(pickQuote({ ...BASE, trendUp: null }, "zh", "", VARS)).toBe(zh[9].text);
  });

  it("5. 多模型形态：multiModel 命中一条，且 {models} 填充的是并行路数", () => {
    const zh = QUOTE_POOL.zh;
    const line = pickQuote({ ...BASE, multiModel: true }, "zh", "", VARS);
    expect(line).toBe(zh[8].text.split("{models}").join(VARS.models));
    expect(line).toContain("3 路并行");
  });

  it("6. 按序首命中：命中率优先于总量、趋势优先于多模型（不是「最匹配」，是**第一条**命中的）", () => {
    const zh = QUOTE_POOL.zh;
    // 命中率 ≥0.9 与总量 ≥1000 万同时成立 → 取靠前的命中率条
    const both = pickQuote({ ...BASE, hitPct: 0.95, total: 20_000_000 }, "zh", "", VARS);
    expect(both).toBe(zh[0].text.split("{hitPct}").join(VARS.hitPct));
    // 趋势与多模型同时成立 → 取靠前的趋势条
    const trendFirst = pickQuote({ ...BASE, trendUp: true, multiModel: true }, "zh", "", VARS);
    expect(trendFirst).toBe(zh[6].text);
    // 兜底：一条都不命中时也要有话说（不是空串、不是占位符残留）
    const fallback = pickQuote(BASE, "zh", "", VARS);
    expect(fallback).toBe(zh[9].text);
    expect(placeholdersOf(fallback)).toEqual([]);
  });

  it("7. fillQuote 只认 4 个占位符：它们全部替换，其余花括号原样保留", () => {
    const out = fillQuote("{range} / {tokens} / {hitPct} / {models} / {tool}", VARS);
    expect(out).toBe("近 7 天 / 1,954,268 / 91.2% / 3 / {tool}");
    // 无占位符的模板原样返回（不做任何隐式插值）
    expect(fillQuote("干得漂亮", VARS)).toBe("干得漂亮");
  });

  it("8. pickQuote：custom 非空**完全覆盖**预制池（哪怕池里有更靠前的命中）；空白回落预制池", () => {
    const zh = QUOTE_POOL.zh;
    const agg: QuoteAgg = { ...BASE, hitPct: 0.99 };
    // custom 命中池第一档也不管——custom 就是最终文案
    expect(pickQuote(agg, "zh", "今天 {tokens} token，{range}", VARS)).toBe(
      "今天 1,954,268 token，近 7 天"
    );
    // 纯空白 = 没填 → 回落预制池
    expect(pickQuote(agg, "zh", "   ", VARS)).toBe(zh[0].text.split("{hitPct}").join(VARS.hitPct));
    expect(pickQuote(agg, "zh", undefined, VARS)).toBe(
      zh[0].text.split("{hitPct}").join(VARS.hitPct)
    );
    // 不给 vars：池文案按空串填充，不留 `{…}` 残骸
    const noVars = pickQuote({ ...BASE, hitPct: 0.99 }, "zh");
    expect(placeholdersOf(noVars)).toEqual([]);
    expect(noVars).not.toBe(zh[0].text);
  });

  it("9. getLang：读 i18n.language → zh / en（zh-CN 归 zh，其余一律 en，与 fallbackLng 同口径）", async () => {
    await i18n.changeLanguage("zh");
    expect(getLang()).toBe("zh");
    await i18n.changeLanguage("zh-CN");
    expect(getLang()).toBe("zh");
    await i18n.changeLanguage("en");
    expect(getLang()).toBe("en");
    await i18n.changeLanguage("en-US");
    expect(getLang()).toBe("en");
    await i18n.changeLanguage("fr");
    expect(getLang()).toBe("en");
  });
});
