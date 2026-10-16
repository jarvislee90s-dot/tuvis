// 分组分布卡（计划② Task 7 步骤 4）——三维切换（工具 / 项目 / 供应商·模型）+ 逐行
// 「名称 + 数值 + 占比条」，超出 10 行折「等 N」。
// 消费方：大看板页（board 分支，工作小结之后）。
//
// 四条纪律：
//  ① **行/占比/折叠全交 `distributionRows`**（Task 2 纯层，唯一出处）：≤ `DIST_MAX_ROWS` 行 +
//     `moreCount`；`share` 是 **0–1 分数**且已夹好 `DIST_MIN_BAR_PCT / 100` 下限与 1 上限；
//     零值行在那里就被丢掉 ⇒ 空结果出空态，**绝不渲染一行 0**（spec P8）。
//  ② **行名分派逐维不同、不得互相借用**（§3 第 10/11/13 条 / D21）：
//     provider → `t(label)`（不可得时 `label` 是 i18n 键 `usage.label.unknownProvider`，直渲会印键名）；
//     model → `label` **原文**（2026-10-06 用户裁决：模型维度已合并为「供应商 / 模型」，Rust 侧
//     与记录页卡内行共用 `query.rs::route_label`，**前端不得再短化**——短化会把
//     `deepseek-v4.1-flash` 砍成与另一个模型无法分辨的 `deepseek-v…`）；
//     project → `label` **原文**（不做 realpath、不做大小写规范化、不用 `projectPath`）；
//     tool → `usageAgentLabel(label)`（真机这个维度的 `label` 是**工具 id**，展示名只由 `AGENT_BADGE` 解析）。
//  ③ 取色只认主题变量：底轨 `--usage-track`、填充 `--usage-accent-a`（§3 第 19/20 条），
//     零硬编码色值；按钮/卡片用 `ui/{button,card}` 原语（§3 第 40 条）。
//  ④ **空态只换内容区，不换整卡**（Task 7 空态修复）：卡头与三维切换按钮在空窗口里照常在位
//     （那是用户此时唯一还能做的事）；空标签直接落 `CardContent`，不再整卡退化成裸 `UsageEmpty`。
//
// ⑤ **名称列宽度按「整列」取，不按行取**（2026-10-06 排版修复）：以前每行是一个 flex，名称列写死
//    `w-28`(112px) ⇒ 真机两个模型名双双被截成 `deepseek-v…` / `DeepSeek-V…`，而同一行的占比条却
//    独占约 600px 空转。现在**整张列表共用一个 CSS grid**（`grid-cols-[minmax(0,auto)_auto_minmax(80px,1fr)]`）
//    ⇒ `auto` 列按**所有行里最长的名称**定宽，名称能完整显示、且所有占比条起点落在同一条竖线上。
//    **刻意不做 JS 实测宽度**（参考实现 dsh-foxbell-pet 的 `--model-name-w` 探针那套）：本项目
//    `getBoundingClientRect()` 在 jsdom 恒 0 ⇒ 那种实现**测不到**、门禁形同不存在（§3 第 33 条）。
//    `minmax(0,auto)` 让名称列在真的放不下时才被压缩、由 `truncate` 收尾（此时 hover 给全名）。
import { Fragment } from "react";
import { Button } from "@/components/ui/button";
import { Card, CardAction, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { DIST_MIN_BAR_PCT, distributionRows, type DistRow } from "@/lib/usage/distribution";
import { fmtInt, fmtTokens, usageAgentLabel } from "@/lib/usage/format";
import { getLang } from "@/lib/usage/lang";
import type { TFn } from "@/lib/usage/range";
import type { UsageGroupBy, UsageRow } from "@/types/usage";

/**
 * 三维顺序 = 用户裁决后的展示顺序（工具 / 项目 / 供应商·模型）⇒ 数组顺序即按钮顺序。
 * ⚠️ `provider` **不再**出现在这里（2026-10-06 用户裁决：供应商与模型合并成一个 sheet）；
 * 它仍是契约 `UsageGroupBy` 的合法取值、仍是 CSV/内部查询的入参，`usage.group.provider` 这个
 * i18n 键也**保留**（`exportText.ts` 走 `t(\`usage.group.${groupBy}\`)` 查表，键必须在）。
 */
const GROUP_DIMS: readonly UsageGroupBy[] = ["tool", "project", "model"];

/**
 * 占比条宽度（%）。`share` 是 **0–1 分数**（绝不是 0–100），`distributionRows` 已把
 * `DIST_MIN_BAR_PCT / 100 = 0.04` 下限与上限 1 夹进去 ⇒ 这里只做一次「分数 → 百分比」的 `× 100`。
 * 再取一次**百分比**下限是幂等的防御式写法；**绝不可**把 4 当分数与 `share` 相乘或相除
 * （那会把短条压成 0.16%，是计划点名禁止的「平方缩小」）。
 */
function barPctOf(share: number): string {
  return Math.max(DIST_MIN_BAR_PCT, share * 100).toFixed(1);
}

/** 行名分派（逐维不同，见文件头纪律 ②）；`full` 只在「显示名与原文不同」时给（进名称的 `title`） */
function nameOf(
  row: DistRow,
  groupBy: UsageGroupBy,
  t: TFn
): { text: string; full: string | null } {
  if (groupBy === "provider") return { text: t(row.label), full: null };
  if (groupBy === "model") return { text: row.label, full: row.label };
  if (groupBy === "tool") return { text: usageAgentLabel(row.label), full: null };
  return { text: row.label, full: row.label };
}

export interface UsageDistributionCardProps {
  /** 当前维度的分组行（页面传 `dash.rows`）；`null` / 空 / 全零 → 空态 */
  rows: readonly Pick<UsageRow, "key" | "label" | "buckets" | "metrics">[] | null | undefined;
  /** 当前分组维度（受控组件：切换只回调，不自己改状态） */
  groupBy: UsageGroupBy;
  onGroupByChange(groupBy: UsageGroupBy): void;
  /** 译函数（`usage.card.distribution` / `usage.group.*` / `usage.moreN` / `usage.exact` / `usage.empty`） */
  t: TFn;
}

export function UsageDistributionCard({
  rows,
  groupBy,
  onGroupByChange,
  t,
}: UsageDistributionCardProps) {
  // 格式化语言（issue #160）：数值单位（万/亿 vs K/M/B）随界面语言走，组件体取一次。
  const lang = getLang();
  const { rows: distRows, moreCount } = distributionRows(rows);
  const isEmpty = distRows.length === 0;

  return (
    <Card>
      <CardHeader>
        <CardTitle>{t("usage.card.distribution")}</CardTitle>
        <CardAction>
          <div className="flex flex-wrap items-center gap-1.5">
            {GROUP_DIMS.map((dim) => (
              <Button
                key={dim}
                size="sm"
                variant={dim === groupBy ? "default" : "outline"}
                aria-pressed={dim === groupBy}
                data-testid={`usage-dist-group-${dim}`}
                onClick={() => onGroupByChange(dim)}
              >
                {t(`usage.group.${dim}`)}
              </Button>
            ))}
          </div>
        </CardAction>
      </CardHeader>

      <CardContent className="space-y-2">
        {isEmpty ? (
          // 空态（Task 7 修复：**只换内容区，不换整卡**）：真机可达路径是「窗口内零行」——
          // 后端空账本 `rows` 为空（`services/usage/query.rs` 的空账本测试 `assert!(d.rows.is_empty())`），
          // 而**四维切换按钮在当前空窗口里是用户唯一还能做的事**：把整卡换成裸 `UsageEmpty`
          // 会把按钮一并抽掉，等于抽掉出口（且与趋势卡的空态重复出第二个 `usage-empty`）。
          // 这里渲染的是**标签本身**（不是嵌一层 `UsageEmpty` 的 Card）：外层卡壳已在位，
          // 再套一张卡就成了卡中卡；字号/弱化色与 `UsageStates.UsageEmpty` 的内容区保持一致。
          <p data-testid="usage-empty" className="text-muted-foreground text-sm">
            {t("usage.empty")}
          </p>
        ) : (
          /* 整张列表**共用一个 grid**（文件头纪律 ⑤）：名称列 `auto` 按所有行的最长名称定宽
             ⇒ 名称完整显示、所有占比条起点落在同一条竖线上。每行的三个格子是该 grid 的
             **直接子节点**（用 `Fragment` 成组，不套行容器——套了容器 `auto` 列就退化成逐行各自
             计算，等于没改）。「等 N」横跨三列。 */
          <div className="grid grid-cols-[minmax(0,auto)_auto_minmax(80px,1fr)] items-center gap-x-2 gap-y-2 text-sm">
            {distRows.map((row) => {
              const name = nameOf(row, groupBy, t);
              return (
                <Fragment key={row.key}>
                  {/* 名称格：行 testid 落在这里（`usage-dist-<key>` 的既有断言仍指向「这一行」）。
                      名称可能被 CSS 截断 ⇒ 全名常驻 `title`。 */}
                  <span
                    data-testid={`usage-dist-${row.key}`}
                    className="min-w-0 truncate"
                    title={name.full ?? undefined}
                  >
                    {name.text}
                  </span>
                  {/* 数值格：缩写值；**精确值常驻 `title`**（spec P8「所有缩写数值都必须能在 hover
                      时读到精确值」）。testid 用 `usage-val-<key>` 而**不是** `usage-dist-val-<key>`：
                      后者会落进 `getAllByTestId(/^usage-dist-k/)` 的匹配面，把「10 行」数成 20。 */}
                  <span
                    data-testid={`usage-val-${row.key}`}
                    title={t("usage.exact", { value: fmtInt(row.value) })}
                    className="text-right font-medium whitespace-nowrap tabular-nums"
                  >
                    {fmtTokens(row.value, lang)}
                  </span>
                  <span
                    data-testid={`usage-dist-bar-${row.key}`}
                    className="h-1.5 min-w-0 overflow-hidden rounded-full bg-[var(--usage-track)]"
                  >
                    <span
                      className="block h-full rounded-full bg-[var(--usage-accent-a)]"
                      style={{ width: `${barPctOf(row.share)}%` }}
                    />
                  </span>
                </Fragment>
              );
            })}

            {moreCount > 0 ? (
              <p data-testid="usage-dist-more" className="text-muted-foreground col-span-3 text-xs">
                {t("usage.moreN", { n: moreCount })}
              </p>
            ) : null}
          </div>
        )}
      </CardContent>
    </Card>
  );
}
