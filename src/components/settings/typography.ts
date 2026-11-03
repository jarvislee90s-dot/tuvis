// 设置页**排版层级的唯一出处**（2026-10-07 用户裁决 C1）。
//
// ## 为什么需要它
// 用户原话：「设置页里面字体大大小小、粗粗细细的，风格各异。你至少要把标题的层级和文字的类型，
// 在字体大小、颜色上都规范一下。比如一级标题是什么样，二级标题是什么样，注释是什么样，
// 把它们统一规划一下。」
// 实测（改动前对本目录 + settings.tsx 做类名普查）确实如此：**9 种字号**
// （`text-lg` / `text-sm` / `text-xs` / `text-[12.5px]` / `text-[11px]` / `text-[10.5px]` /
// `text-[10px]` / `text-[9px]` / `text-[8px]`）与 **3 种字重**（semibold / medium / bold）混用，
// 且同一层级的标题在不同文件里写法不同（`text-lg font-semibold` 13 处 vs `text-sm font-semibold` 7 处）。
//
// ## 层级表（**新增文字必须先在这里找一档**，找不到就说明该档该补进表里）
//
// | 档 | 常量 | 类 | 用途 |
// |---|---|---|---|
// | 一级标题（页面） | `SETTINGS_PAGE_TITLE` | `text-lg font-semibold`（18） | **每页唯一**：当前功能块的标题 |
// | 二级标题（卡片） | `SETTINGS_CARD_TITLE` | `text-sm font-semibold`（14） | 每个设置分区 / 卡的 `h2`/`h3` |
// | 副标题（段落级说明） | `SETTINGS_SUBTITLE` | `text-sm text-muted-foreground`（14 弱） | 标题正下方那句「这块是干什么的」 |
// | 字段名 | `SETTINGS_FIELD` | `text-sm font-medium`（14 中） | 设置项的 `label` |
// | 说明（细） | `SETTINGS_HINT` | `text-xs text-muted-foreground`（12 弱） | 字段旁/下的补充说明 |
// | 注释（脚注级） | `SETTINGS_NOTE` | `text-[11px] text-muted-foreground`（11 弱） | 口径、边界、细则 |
//
// ## 为什么**二级标题比一级小一档**（14 vs 18）
// 改动前两者都是 `text-lg`（18）⇒ 块标题与卡标题**看不出层级**，而这正是两级导航之后最需要
// 一眼看懂的关系（点大块 → 里面几张卡）。
//
// ## 豁免面（**刻意不统一**，改前先想清楚）
//  * `SETTINGS_BADGE`（`text-[10px]`）：徽标/角标。它靠**小**来区别于正文，与层级无关。
//  * **密集数据网格**里的 `text-[9px]` / `text-[10px]` / `text-[12.5px]`
//    （`AuditLogSection` 五列、`SignalHealthSection` 矩阵、`RemoteSection` 的表格、
//    `UsageStatusSection` 的采集表）：这些是**塞得下优先**的表格单元格。把它们抬到 11/12px
//    会把列挤爆 —— 那是版面事故，不是排版规范。要动它们得连着列宽一起重排，属另一件事。
//  * **缩小的设备预览**（`RemoteAppearanceSection`，8/9/10px）：那是模拟手机的聊天气泡 / 通知卡 /
//    选项样例，字号是**缩微**的一部分；放大就不像「预览」而像「正文」了。
//
// 这层豁免由 `tests/settings/settingsTypography.test.ts` **逐条钉住**（谁把密集网格的字号
// 顺手「规范」了，或用内联 `text-lg font-semibold` 新写一个标题，都会红）。
//
// ## 线稿硬契约档（**不属上面的层级表**：按 UI 唯一契约原样落地，不得按「凑成整档」改数）
//  * `SETTINGS_REMOTE_BADGE`（`text-[10.5px]`）：远程接入分区的徽标档。出处 =
//    `docs/engineering/architecture/wireframes/2026-09-17-remote-settings-redesign.html` 的
//    `.badge { font-size: 10.5px; border-radius: 999px; padding: 2px 8px; flex: none; }`
//    ——该线稿是 UI 唯一契约（「实现与线稿不一致即缺陷」），且**全部徽标色**（green /
//    blue / gray / violet / amber）共用这一处字号定义。
//    **为什么是 10.5 而不是 10**：10 是设置页角标档 `SETTINGS_BADGE` 的值（用在
//    `settings.tsx` 的角标上，**不在本线稿覆盖范围内**）；远程分区徽标以线稿为准取 10.5。
//    两档**不得互相替代**——把远程徽标压到 10 就是"实现与线稿不一致"。

/** 一级标题（页面级）：**每页唯一**，当前功能块的标题 */
export const SETTINGS_PAGE_TITLE = "text-lg font-semibold";

/** 二级标题（卡片级）：每个设置分区 / 卡的 `h2` / `h3` */
export const SETTINGS_CARD_TITLE = "text-sm font-semibold";

/** 副标题（段落级说明）：标题正下方那句「这块是干什么的」 */
export const SETTINGS_SUBTITLE = "text-sm text-muted-foreground";

/** 字段名：设置项的 `label` */
export const SETTINGS_FIELD = "text-sm font-medium";

/** 说明（细）：字段旁 / 下的补充说明 */
export const SETTINGS_HINT = "text-xs text-muted-foreground";

/** 注释（脚注级）：口径、边界、细则 */
export const SETTINGS_NOTE = "text-[11px] text-muted-foreground";

/** 徽标 / 角标（**豁免档**：靠小区别于正文，与层级无关）。用于 `settings.tsx` 的角标 */
export const SETTINGS_BADGE = "text-[10px]";

/**
 * 远程接入分区的徽标档（**线稿硬契约**，非层级档）。
 *
 * 出处：`docs/engineering/architecture/wireframes/2026-09-17-remote-settings-redesign.html` 的
 * `.badge { font-size: 10.5px; border-radius: 999px; padding: 2px 8px; flex: none; }`
 * ——线稿是 UI 唯一契约，且全部徽标色（green / blue / gray / violet / amber）共用它。
 *
 * **不是 `SETTINGS_BADGE`**：后者 10px 服务于 `settings.tsx` 的角标，不在本线稿范围内。
 * 远程徽标一律用本档，压到 10px 即"实现与线稿不一致"。
 */
export const SETTINGS_REMOTE_BADGE = "text-[10.5px]";

/**
 * 远程接入分区的二维码说明档（**线稿硬契约**，非层级档）。
 *
 * 出处：同一线稿的 `.qr { font-size: 10.5px; text-align: center; … }`——线稿把
 * `.badge` 与 `.qr` 当两条**独立规则**（当前取值同为 10.5px 是巧合而非同档）；合成一条
 * 的话，将来线稿只改其中之一时代码侧无法区分该改哪处。与徽标档不得互相替代。
 * （#124：此处原为 `RemoteSection` 的内联字面量，2026-10-14 收口立档。）
 */
export const SETTINGS_REMOTE_QR_CAPTION = "text-[10.5px]";

/**
 * 远程接入分区的行正文档（**线稿硬契约**，非层级档）。
 *
 * 出处：同一线稿的 `.row { padding: 12px 0; …; font-size: 13px }`、
 * `.sub { color: var(--muted); font-size: 13px }`、`.mode .name { font-weight: 700;
 * font-size: 13px }`——行级正文与模式名。该值此前散落为裸字面量且从未登记
 * （#125 核查时因此被排版锁视为「看不见」），2026-10-14 立档。
 */
export const SETTINGS_REMOTE_ROW = "text-[13px]";

/**
 * 远程接入分区的行内提示档（**线稿硬契约**，非层级档）。
 *
 * 出处：同一线稿的 `.row .hint { color: var(--muted); font-size: 12px }`。
 * （#125 立档，同上。）
 */
export const SETTINGS_REMOTE_HINT = "text-[12px]";

/**
 * 远程接入分区的组标题档（**线稿硬契约**，非层级档）。
 *
 * 出处：同一线稿的 `.group-label { font-size: 12.5px; color: var(--muted);
 * font-weight: 600; margin: 16px 0 2px; }`——分区内的组头（① 通用 / ② 远程控制 /
 * ⑤ Codex 自动适配…）共用这一档。此前该值只以裸字面量存在于豁免面
 * （`RemoteSection`）内；S5 拆出 `CodexAdaptSection` 后组头落到非豁免文件，
 * 由 S6 依测试指引立档（非豁免面要用请走 typography.ts 常量）。
 */
export const SETTINGS_REMOTE_GROUP_LABEL = "text-[12.5px]";
