// 设置页排版层级锁（2026-10-07 用户裁决 C1）。
//
// 用户原话：「设置页里面字体大大小小、粗粗细细的，风格各异……把它们统一规划一下。」
// 改动前的实测：**9 种字号**（`text-lg`/`text-sm`/`text-xs`/`text-[12.5px]`/`text-[11px]`/
// `text-[10.5px]`/`text-[10px]`/`text-[9px]`/`text-[8px]`）＋ **3 种字重**混用，
// 同一个「分区标题」这一层在 13 处写 `text-lg font-semibold`、在 7 处写 `text-sm font-semibold`。
//
// 本锁守三件事：
//  ① **档位表本身不许悄悄漂**（谁把二级标题改回 18，立刻红）；
//  ② **设置面的标题必须走档位常量**，不得再写内联 `text-lg font-semibold`（正是它造成 13 vs 7 的分裂）；
//  ③ **豁免面是显式的**：密集数据网格里的小字号只允许出现在登记过的文件里，且不得外溢。
import { readFileSync, readdirSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";
import {
  SETTINGS_BADGE,
  SETTINGS_CARD_TITLE,
  SETTINGS_FIELD,
  SETTINGS_HINT,
  SETTINGS_NOTE,
  SETTINGS_PAGE_TITLE,
  SETTINGS_REMOTE_HINT,
  SETTINGS_REMOTE_QR_CAPTION,
  SETTINGS_REMOTE_ROW,
  SETTINGS_SUBTITLE,
} from "@/components/settings/typography";

const ROOT = process.cwd();
const SETTINGS_DIR = path.join(ROOT, "src/components/settings");
const SURFACE = [
  path.join(ROOT, "src/pages/settings.tsx"),
  ...readdirSync(SETTINGS_DIR)
    .filter((f) => f.endsWith(".tsx"))
    .map((f) => path.join(SETTINGS_DIR, f)),
];
// rel 统一正斜杠：EXEMPT 登记的是正斜杠路径，win32 的 path.relative 返回反斜杠
// 会导致集合查不中、把豁免面自己误报成外溢（2026-10-08 实测 Windows 全红）
const rel = (f: string) => path.relative(ROOT, f).replace(/\\/g, "/");

/**
 * **豁免面**（file → 具名理由）：小字号在这里是「装得下优先」，抬字号会把版面挤爆
 *  —— 属版面问题，不属排版规范。要动必须先重排版面，并同步改本清单与 `typography.ts`。
 * （2026-10-14 #125 方案 A：豁免仍按文件，但**放行的是登记值**——文件进豁免面
 *  不再等于「里面写什么字号都没人管」，未登记值在豁免面内同样报红。）
 */
const EXEMPT = new Map<string, string>([
  ["src/components/settings/AuditLogSection.tsx", "五列审计表（密集网格）"],
  ["src/components/settings/SignalHealthSection.tsx", "信号矩阵（密集网格）"],
  ["src/components/settings/RemoteSection.tsx", "远程设备/文件表与模式名（密集网格）"],
  ["src/components/settings/UsageStatusSection.tsx", "采集状态表（密集网格）"],
  ["src/components/settings/RemoteAppearanceSection.tsx", "缩微设备预览（小字号是缩微的一部分）"],
]);

/**
 * **登记值总表**（2026-10-14 #125 方案 A）：一切合法 `text-[Npx]` 的值域。
 * 未登记的值（如 12 与 13 曾是、将来的 13.5/7 等）在**任何**设置面文件里都报红
 * ——「野字号」不再因为「没被枚举过」而对规则隐身（旧实现只枚举 5 个登记值，
 * 谁写一个新字号谁就免检，正是本条要修的盲区）。出处：
 *  * `8` / `9` / `12.5` —— 密集网格单元格与缩微预览（豁免档，仅豁免面内可用）；
 *  * `10`   —— `SETTINGS_BADGE`；
 *  * `10.5` —— `SETTINGS_REMOTE_BADGE` / `SETTINGS_REMOTE_QR_CAPTION`（线稿硬契约）；
 *  * `11`   —— `SETTINGS_NOTE`（层级档，任何文件可用）；
 *  * `12` / `13` —— `SETTINGS_REMOTE_HINT` / `SETTINGS_REMOTE_ROW`
 *    （2026-09-17 线稿硬契约，#125 核查时发现散落未登记，2026-10-14 立档）。
 */
const REGISTERED = new Set(["8", "9", "10", "10.5", "11", "12", "12.5", "13"]);

/**
 * 登记值中的**小字号**：裸字面量只允许出现在豁免面内（外溢仍红）；非豁免文件
 * 要用请走 `typography.ts` 常量（常量插值不算裸字面量）。`11` 不在列——它是
 * `SETTINGS_NOTE` 层级档，与旧口径一致地在任何文件可用。
 */
const SMALL_LITERAL_ONLY_IN_EXEMPT = ["8", "9", "10", "10.5", "12", "12.5", "13"];

/** 扫一段源码里的全部 `text-[Npx]` 字面量（规则本体抽成函数，反例测试才能直接打它） */
const scanFontLiterals = (src: string): string[] =>
  [...src.matchAll(/text-\[([0-9.]+)px\]/g)].map((m) => m[1]);

describe("设置页排版层级（C1）", () => {
  it("① 档位表逐字钉住（二级标题必须比一级小一档 —— 改动前两者都是 18）", () => {
    expect(SETTINGS_PAGE_TITLE).toBe("text-lg font-semibold"); // 18 一级
    expect(SETTINGS_CARD_TITLE).toBe("text-sm font-semibold"); // 14 二级
    expect(SETTINGS_SUBTITLE).toBe("text-sm text-muted-foreground");
    expect(SETTINGS_FIELD).toBe("text-sm font-medium");
    expect(SETTINGS_HINT).toBe("text-xs text-muted-foreground");
    expect(SETTINGS_NOTE).toBe("text-[11px] text-muted-foreground");
    expect(SETTINGS_BADGE).toBe("text-[10px]");
    // 层级真的分开了（这条是「一级/二级」这件事本身）
    expect(SETTINGS_PAGE_TITLE).not.toBe(SETTINGS_CARD_TITLE);
  });

  it("② 设置面的每个 h2/h3 都走档位常量，且**没有一个**内联类名", () => {
    let checked = 0;
    for (const f of SURFACE) {
      const src = readFileSync(f, "utf8");
      // 多行标签：`<h2\n  className={...}` —— `[^>]*?` 允许换行但**不许跨过标签结束**（跨过去就会
      // 把标签之后某个 `<div className="space-y-4">` 误判成「标题的内联类名」）
      const inline = [...src.matchAll(/<h[23]\b[^>]*?className="([^"]*)"/g)].map((m) => m[1]);
      expect(inline, `${rel(f)} 的标题不得再用内联类名（走 typography.ts 的档位）`).toEqual([]);
      const tiered = [...src.matchAll(/<h[23]\b[^>]*?className=\{([^}]*)\}/g)].map((m) => m[1]);
      for (const expr of tiered) {
        expect(expr, `${rel(f)} 的标题必须引用 SETTINGS_* 档位常量，实际：${expr}`).toMatch(
          /SETTINGS_(PAGE_TITLE|CARD_TITLE)/
        );
      }
      checked += tiered.length;
    }
    // 设置面标题总数：settings.tsx 7（1 一级色条 + 6 卡头条 h2）+ 7 个组件卡头条各 1
    //（Remote/Audit/Signal/DataMgmt/Usage/UsageStatus/RemoteAppearance）+ UsageStatusSection 的 h3 1 = 15。
    // 2026-10-07 D4 把「快捷键」整节移除 ⇒ 账曾由 14 降为 13；2026-10-11（a）开机自启
    // 分区加入、（b）远程端外观卡头条化并补 h2 语义（原折叠壳无 h2）⇒ 14 → 15（**本锁
    // 当场抓到过多次变化**：分区/卡增删时它先红，逼我们把数字与新事实对齐，而不是让计数悄悄漂）。
    expect(checked, "设置面标题总数（少一个就说明有标题漏掉了档位）").toBe(15);
  });

  it("③ 字号按值登记：未登记值在任何文件都红；登记小字号的裸字面量不得外溢豁免面", () => {
    const offenders: string[] = [];
    for (const f of SURFACE) {
      const r = rel(f);
      const src = readFileSync(f, "utf8");
      for (const v of new Set(scanFontLiterals(src))) {
        if (!REGISTERED.has(v)) {
          offenders.push(`${r} 出现未登记字号 text-[${v}px]（先在 typography.ts 立档，再用药）`);
          continue;
        }
        if (SMALL_LITERAL_ONLY_IN_EXEMPT.includes(v) && !EXEMPT.has(r)) {
          // 徽标档在**任何**设置面文件里都合法（它靠小区别于正文，与层级无关）
          if (v === "10" && src.includes("SETTINGS_BADGE")) continue;
          offenders.push(
            `${r} 出现小字号裸字面量 text-[${v}px]（非豁免面请走 typography.ts 常量）`
          );
        }
      }
    }
    expect(
      offenders,
      "野字号（未登记值）与小字号外溢（要加豁免请连同理由写进 typography.ts 与 EXEMPT）"
    ).toEqual([]);
  });

  it("③c 反例：登记表外的值必须被抓到（规则不能只靠「恰好没人踩到」验证）", () => {
    // 构造含未登记字号的源码段——旧实现（枚举 5 个登记值）对这类源码是盲的
    const wild = scanFontLiterals(
      '<p className="text-[13.5px]">x</p><i className="text-[7px]">y</i>'
    );
    expect(wild).toEqual(["13.5", "7"]);
    expect(
      wild.every((v) => !REGISTERED.has(v)),
      "13.5/7 不在登记表，规则必须能看见"
    ).toBe(true);
    // 登记值照常通过（12.5 = 密集网格档）
    expect(scanFontLiterals('<p className="text-[12.5px]">x</p>')).toEqual(["12.5"]);
    // 线稿硬契约档的值逐字钉住（与 ① 同级纪律：防「顺手规范化」把线稿值改成整数档）
    expect(SETTINGS_REMOTE_QR_CAPTION).toBe("text-[10.5px]");
    expect(SETTINGS_REMOTE_ROW).toBe("text-[13px]");
    expect(SETTINGS_REMOTE_HINT).toBe("text-[12px]");
  });

  it("③b 字段名也必须走档位（内联 `text-sm font-medium` 在设置面绝迹）", () => {
    const offenders = SURFACE.filter((f) =>
      readFileSync(f, "utf8").includes('className="text-sm font-medium"')
    ).map(rel);
    expect(
      offenders,
      "设置面的字段名必须用 SETTINGS_FIELD（否则「字段名」这一档只是纸面规范）"
    ).toEqual([]);
  });

  it("④ 豁免面自身仍在（防止「顺手规范密集网格」把列挤爆）", () => {
    // 逐个钉：每个豁免文件里至少各有一处登记过的小字号裸字面量，且它们**不在** ② 的
    // 标题档位替换范围里（Map 遍历取 key，理由在 EXEMPT 登记处）
    for (const [r, reason] of EXEMPT) {
      const src = readFileSync(path.join(ROOT, r), "utf8");
      expect(
        SMALL_LITERAL_ONLY_IN_EXEMPT.some((v) => src.includes(`text-[${v}px]`)),
        `${r}（${reason}）是豁免面，但已找不到任何登记小字号——若确实重排了版面，请同时更新本清单与 typography.ts`
      ).toBe(true);
    }
  });
});
