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
 * **豁免面**：小字号在这里是「装得下优先」，抬字号会把版面挤爆 —— 属版面问题，不属排版规范。
 *  * 前四个 = **密集数据网格**（五列审计表 / 信号矩阵 / 远程设备表 / 采集状态表）；
 *  * 第五个 = **缩小的设备预览**（`RemoteAppearanceSection` 里是一台模拟手机的聊天/通知/卡片
 *    预览，`text-[8px]`/`text-[9px]` 是它内部的**缩微**字号，放大就不像「预览」而像「正文」了）。
 * 要动这两类必须先重排版面，并同步改本清单与 `typography.ts` 的说明。
 */
const EXEMPT = new Set([
  "src/components/settings/AuditLogSection.tsx",
  "src/components/settings/SignalHealthSection.tsx",
  "src/components/settings/RemoteSection.tsx",
  "src/components/settings/UsageStatusSection.tsx",
  "src/components/settings/RemoteAppearanceSection.tsx",
]);

/** 豁免档允许出现的小字号（徽标 + 密集网格单元格） */
const ALLOWED_SMALL = ["text-[10px]", "text-[9px]", "text-[8px]", "text-[12.5px]", "text-[10.5px]"];

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

  it("③ 小字号**只**允许出现在登记过的豁免面里（外溢即红）", () => {
    const offenders: string[] = [];
    for (const f of SURFACE) {
      const r = rel(f);
      const src = readFileSync(f, "utf8");
      for (const small of ALLOWED_SMALL) {
        if (!src.includes(small)) continue;
        // 徽标档在**任何**设置面文件里都合法（它靠小区别于正文，与层级无关）
        if (small === "text-[10px]" && src.includes("SETTINGS_BADGE")) continue;
        if (!EXEMPT.has(r)) offenders.push(`${r} 出现 ${small}`);
      }
    }
    expect(offenders, "小字号外溢到非豁免文件（要加豁免请连同理由写进 typography.ts）").toEqual([]);
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
    // 逐个钉：这四个文件里至少各有一处小字号，且它们**不在**上面 ② 的标题档位替换范围里
    for (const r of EXEMPT) {
      const src = readFileSync(path.join(ROOT, r), "utf8");
      expect(
        ALLOWED_SMALL.some((s) => src.includes(s)),
        `${r} 是密集网格豁免面，但已找不到任何小字号——若确实重排了版面，请同时更新本清单与 typography.ts`
      ).toBe(true);
    }
  });
});
