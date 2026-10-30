// 宠物浮窗迷你条（计划② Task 13；spec D12 / D13 / D20 / P2）——本文件两个成员：
//
//  * `UsageMiniBarView`（展示层，**纯 props**）：固定 5 行、行序钉死（请求+命中率 / 缓存命中+产出 /
//    分工具汇总 1 行 / 本会话 / 详情 »）。行高按 `MINI_LINE_H` 给死、每行 nowrap + ellipsis，
//    容器另加 `maxHeight` + `overflowY: auto` 兜底 ⇒ 窗口高度与纯层公式一致、不随文案抖动。
//  * `UsageMiniBar`（数据容器）：三态与数据全部来自 `useUsageDashboardQuery`（档位取设置
//    `miniBarRange`、分组恒 `tool`），并在**悬停那一刻**按需采集一次（`useUsageCollect`：
//    `force=false`、最小间隔与单飞由后端保证；兜底 ticker 也在那个 hook 里，本文件不另起）。
//
// 五条纪律（逐条都有出处）：
//  ① **不可得一律空态、绝不填 0**（§3 第 5 条）：值取不到 → `EM_DASH`；`collectedAt === 0`
//     （尚未采集 / 总开关关闭）→ `usage.notCollected`（§3 第 8 条）；`recentSession === null` →
//     整行空态。第 3 行全零 → 空串 → 空态（纯层就不出 0）。
//     **2026-10-06 用户裁决补的一条**：「只有是真实的数值，你才能写它的数值。如果没有产生数值，
//     就不显示了」⇒ 本窗口**没量到 token**（四桶全零、但可能有 `requests`——真机的**真空回合**行）
//     时，第 ①②③ 行的 token 位一律 `EM_DASH`，判据只用 `tokenUnmeasured`（`range.ts`）这一处。
//     （第 ④ 行不需要这条：那种窗口下 `recentSession` 必为 `null`，见该行注释。）
//  ② **唯一可点元素是「详情 »」**（D12）：容器 `pointerEvents: "none"`、详情行 `auto`
//     （CSS 父 none + 子 auto 才能命中），点击走 `onDetail` → `openUsageDashboard`。
//     加载态 / 错误态**照常**渲染它（错误态因此保留恢复入口，Task 13 步骤 3）。
//  ③ **滚轮不劫持**（D13）：本组件**不注册**任何滚轮监听、也不吞任何事件的默认行为。
//     注：滚轮不劫持在 OS 层面本就无法靠 CSS 实现（宠物窗口整窗常驻交互），本计划给出的保证
//     就是「我们不注册」——所以这里连一句注释都不写那两个字面量（源码锁在用例 9）。
//  ④ **取色一律走主题 token**（§3 第 19 条）：本文件零硬编码色值（主题门禁 Group A 成员）。
//  ⑤ **「本会话」的数据源是契约 `recentSession`**（spec P2 第 4 条）：前端**不读会话列表**、不自己
//     挑「当前会话」。`title` 为 `null` → `usage.mini.untitled`（绝不印 `null` 字面量）。
//     ⚠️ **日档（last7d / last30d / custom）后端恒给 `recentSession: null`**（日聚合行不带
//     `session_id`，见契约 §2 与 `query.rs` 的口径）⇒ 该行必然空态，这是**预期行为不是 bug**。
import { useEffect } from "react";
import {
  useUsageCollect,
  useUsageDashboardQuery,
  useUsageSettingsQuery,
} from "@/lib/query/queries/usage";
import { HEARTBEAT_INTERVAL_MS } from "@/lib/query/queries/scanBus";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { EM_DASH, fmtPct, fmtTokens, usageAgentLabel } from "@/lib/usage/format";
import {
  MINI_BAR_TOOL_LIMIT,
  MINI_LINE_H,
  miniBarMaxHeight,
  miniBarToolLine,
  miniBarToolRowText,
} from "@/lib/usage/miniBar";
import { getLang } from "@/lib/usage/lang";
import { isDashboardEmpty, tokenUnmeasured } from "@/lib/usage/range";
import type { TFn } from "@/lib/usage/range";
import type { UsageDashboard, UsageRange } from "@/types/usage";

/** 浮窗的两个触发方式（spec D12：悬停 / 右键菜单手动唤回；**同一个容器**） */
export type MiniBarMode = "hover" | "manual";

/** 数据三态（来自 `useUsageDashboardQuery`；`ready` 且 `dash === null` 视为空态） */
export type MiniBarStatus = "loading" | "error" | "ready";

export interface UsageMiniBarViewProps {
  /** 查询三态；`loading` / `error` 时数据行出空态占位，但「详情 »」照常渲染 */
  status: MiniBarStatus;
  /** 看板数据（`ready` 时给；`null` = 空态） */
  dash: UsageDashboard | null;
  /** 第 3 行最多列几个工具（设置项 `miniBarToolRows`）；缺省 `MINI_BAR_TOOL_LIMIT` */
  toolLimit?: number;
  /** 宠物三档缩放（`PET_SCALES`）：字号 / 行距 / 位置等比 */
  scale: number;
  /** 当前触发方式（`data-mode` 供样式与用例辨识；两个方式共用本容器） */
  mode: MiniBarMode;
  /** 「详情 »」钻取：打开大看板（唯一可点元素的唯一行为） */
  onDetail(): void;
  /** 译函数（`usage.mini.*` / `usage.moreN` / `usage.notCollected`） */
  t: TFn;
}

export function UsageMiniBarView({
  status,
  dash,
  toolLimit = MINI_BAR_TOOL_LIMIT,
  scale,
  mode,
  onDetail,
  t,
}: UsageMiniBarViewProps) {
  // 格式化语言（issue #160）：本组件收 `t` prop 而非 useTranslation，语言切换由父层
  // 触发重渲染，故函数体开头取一次即可；数值单位（万/亿 vs K/M/B）随界面语言走。
  const lang = getLang();
  const px = (v: number) => Math.round(v * scale);

  // 数据是否可用：三态非 ready / 无数据 / 尚未采集哨兵 / **本期窗口为空** ⇒ 一律空态
  // （绝不把哨兵印成 0，也绝不把「本期没数据」印成 0 —— spec P8 空态条：不显示 0 或空白）
  const ready = status === "ready" && dash !== null;
  const notCollected = ready && dash.collectedAt === 0;
  // **空窗口**（2026-10-06 评审 I-2 修复）：采集过、但本期一行都没有。判据**复用看板那一条**
  // `isDashboardEmpty`（`range.ts`）——不另立第二套，否则两个面又会各说各话。
  // 为什么必须有这一支：`live` 只排除了 `collectedAt === 0`，于是真空窗口下第①②行会渲染
  // `请求输入 0 · 命中率 0.0%` / `缓存命中 0 · 产出 0` —— 一个**看起来像真实测量值的 0**，
  // 而看板同一个窗口显示的是「暂无数据」。真机高频可达：`miniBarRange` 默认 `today`，
  // 今天还没用过任何工具时窗口就是空的。
  const emptyWindow = ready && !notCollected && isDashboardEmpty(dash);
  const live = ready && !notCollected && !emptyWindow;
  // **没量到 token**（2026-10-06 用户裁决）：采集过、也有行（故 `isDashboardEmpty` 为假），
  // 但本窗口四个 token 桶全零——真机的**真空回合**形状。此时 token 位出 `EM_DASH`，
  // 计数类真值照常（浮窗这一档本来就不显示计数）。判据只此一处（`range.ts::tokenUnmeasured`）。
  const noTokens = live && tokenUnmeasured(dash);
  // token 位出数值的条件：**同一个判据的四个消费点**（行①的请求输入与命中率、行②的缓存命中与产出）
  // 共用这一个派生值，不再各写一遍 `live && !noTokens`。
  // ⚠️ **行③与行④刻意不并进来**：行③的那条 `!noTokens` 是纵深防御（理由见该行注释）、行④的 token
  // 位不设第二道门控（真机互斥，理由见该行注释）——两处都不是「同一判据的重复」。
  const tokenShown = live && !noTokens;

  // 行①：加载 / 错误时整行让位给状态文案（「详情 »」不在这三态分支里，见文件头纪律 ②）
  // 三个空态文案各有所指，不得混用：未采集 → notCollected；空窗口 → usage.empty；没量到 token → —
  const requestInputValue = notCollected
    ? t("usage.notCollected")
    : emptyWindow
      ? t("usage.empty")
      : tokenShown
        ? fmtTokens(dash.totals.requestTotal, lang)
        : EM_DASH;
  const row1 =
    status === "loading"
      ? t("usage.mini.loading")
      : status === "error"
        ? t("usage.mini.error")
        : `${t("usage.mini.requestInput")} ${requestInputValue} · ${t("usage.mini.hitRate")} ${
            tokenShown ? fmtPct(dash.totals.cacheHitRate) : EM_DASH
          }`;

  // 行②：缓存命中 + 产出（四桶口径，与 hero 同源）
  const row2 = `${t("usage.mini.cacheHit")} ${
    tokenShown ? fmtTokens(dash.totalsBuckets.cacheRead, lang) : EM_DASH
  } · ${t("usage.mini.output")} ${tokenShown ? fmtTokens(dash.totalsBuckets.output, lang) : EM_DASH}`;

  // 行③：分工具汇总**一**行（纯层过滤/排序/折叠 + 行首口径标签；全零 → 空串 → 空态）
  // `!noTokens` 与纯层的 `requestTotal > 0` 过滤是**同一结论**（四桶全零 ⇒ 每行 requestTotal 也必为 0），
  // 这里显式写出来是纵深防御：用户可见的「不许印 0」不该只依赖另一层的算式蕴含。
  const tool =
    live && !noTokens
      ? miniBarToolLine(dash.rows, toolLimit, (n: number) => fmtTokens(n, lang))
      : { text: "", moreCount: 0 };
  const toolText = miniBarToolRowText(tool, t("usage.mini.toolsBasis"), (n) =>
    t("usage.moreN", { n })
  );
  const row3 = toolText === "" ? EM_DASH : toolText;

  // 行④：最近有活动的会话（契约 `recentSession`，不读会话列表）；标题为 null → untitled
  // ⚠️ **本行的 token 位不需要再判「没量到」**（2026-10-06 第二轮保真修复，第三轮把推理补全）：
  // 真机上「窗口没量到 token」与「有 `recentSession`」**互斥**，逐步证明如下 ——
  //   ① `semantics.rs` 的**两套语义都**满足恒等式 **四桶之和 ≥ 每行的 `request_total`**
  //      （Exclusive：`request_total = input_fresh + cache_read + cache_write`，和 = 它 + `output`；
  //       Subset：`request_total = input_raw = input_fresh + cache_read`，和 = 它 + `cache_write` + `output`）；
  //   ② 「本窗口没量到 token」= 四桶之和为 0（`range.ts::tokenUnmeasured` 的定义）；
  //   ③ ① + ② ⇒ **窗口内每一行的 `request_total` 都是 0**；
  //   ④ 后端 `query.rs::recent_session_with` 的候选集**要求该会话在本期窗口内 `request_total > 0`**
  //      （「假零修复」那条判据）⇒ ③ 之下候选集**为空**；
  //   ⑤ ⇒ `recentSession` **必为 `null`** ⇒ 本行**必然**走下面的「暂无会话」空态分支。
  // 故这里**不设第二条门控**：留着 `&& !noTokens` 就是真机不可达的空锁（夹具曾用「非 null 会话 +
  // 全零四桶」造出过这个形状，那是后端不可能产生的、已按保真修掉，见 `mockFixtures.ts::zeroTokensOf`）。
  // 行①②③ 的 token 位仍各带这条判据（①②用上面的共用派生值 `tokenShown`，③保留自己那一道）：
  // 那三处与本题无关（它们读的是 `totals` / `totalsBuckets` / 行值，不依赖「候选集非空」这条后端保证）。
  const session = live ? dash.recentSession : null;
  const row4 = session
    ? `${t("usage.mini.session")} ${usageAgentLabel(session.sourceId)} ${fmtTokens(
        session.metrics.requestTotal,
        lang
      )} · ${session.title ?? t("usage.mini.untitled")}`
    : `${t("usage.mini.session")} ${EM_DASH} · ${t("usage.mini.noSession")}`;
  const row4Title = session ? t("usage.mini.sessionHint") : t("usage.mini.sessionEmptyHint"); // 两种空态原因（范围内没有 / 日档算不出）都在这一条里
  // 行④的 `title` 必须**真的悬得到**（验收 E-4 的承诺）：容器整块 `pointer-events: none`（纪律 ②），
  // 而 `pointer-events: none` 的节点**不参与命中测试** ⇒ 只在容器上写 title 等于写了看不见的说明。
  // 故本行与「详情 »」同法自开 `pointerEvents: "auto"`（见下方 rowStyle 处的行内合并）。
  // 副作用核查（如实登记）：manual 模式的「点外关闭」**不受影响** —— 该行与容器同属
  // `pet-mini-wrap`，而关闭监听本就豁免 wrap 内的按下（`FoxbellPet` 的 `miniWrapRef.contains`），
  // 故「点这一行」在改前改后都不算「点外面」；auto 只让 title 由「悬不到」变「悬得到」。

  /** 单行样式：nowrap + ellipsis（行高确定）+ 行高按公式给死（窗口高度 = 公式值，不抖） */
  const rowStyle: React.CSSProperties = {
    lineHeight: `${MINI_LINE_H * scale}px`,
    whiteSpace: "nowrap",
    overflow: "hidden",
    textOverflow: "ellipsis",
  };

  return (
    <div
      data-testid="usage-mini"
      data-mode={mode}
      style={{
        // 容器自身不抢宠物窗口内部的指针事件；**不是**穿透到别的 OS 窗口（见 FoxbellPet 的
        // 「吃点击面积」说明——宠物窗口整窗常驻交互，CSS 做不到跨窗口穿透）
        pointerEvents: "none",
        boxSizing: "border-box",
        padding: `${px(8)}px ${px(10)}px`, // 上下合计 = MINI_BAR_PAD 的同 scale 换算
        fontSize: px(12),
        maxHeight: miniBarMaxHeight(scale), // 兜底：放不下就截断，不把窗口撑爆
        overflowY: "auto",
      }}
      className="bg-card/95 text-card-foreground border-border rounded-lg border shadow-md"
    >
      <div data-testid="usage-mini-row-request" style={rowStyle} className="font-medium">
        {row1}
      </div>
      <div data-testid="usage-mini-row-cache" style={rowStyle} className="text-muted-foreground">
        {row2}
      </div>
      <div data-testid="usage-mini-row-tools" style={rowStyle} className="text-muted-foreground">
        {row3}
      </div>
      {/* 行④：会话行同理自开 auto —— 它的 `title`（本会话语义提示 / 两种空态原因）要悬得到（E-4）；
          它**不**注册 onClick，故不新增可点元素，也不改变 manual 模式的「点外关闭」判定（见上）。 */}
      <div
        data-testid="usage-mini-row-session"
        style={{ ...rowStyle, pointerEvents: "auto" }}
        className="text-muted-foreground"
        title={row4Title}
      >
        {row4}
      </div>
      {/* 「详情 »」——浮窗**唯一**可点元素（D12）；父容器 pointer-events:none ⇒ 这一行必须自己 auto。
          可悬停的不止它：上面的会话行也为自己的 `title` 自开 auto（E-4），其余元素仍是
          `pointer-events:none`。`usage.mini.truncated`（放不下会被截断、点这里看完整数据）
          挂在**这一行**上：容器是 none，挂容器上真机悬不到，挂这里才真的看得见。 */}
      <div
        data-testid="usage-mini-row-detail"
        style={{ ...rowStyle, pointerEvents: "auto", cursor: "pointer" }}
        className="text-primary text-right"
        title={t("usage.mini.truncated")}
        onClick={() => onDetail()}
      >
        {t("usage.mini.detail")}
      </div>
    </div>
  );
}

export interface UsageMiniBarProps {
  /** 宠物三档缩放（`PET_SCALES`）：字号 / 行距 / 位置等比 */
  scale: number;
  /** 当前触发方式（hover 悬停 / manual 菜单唤回；两个方式**共用**本容器，D12） */
  mode: MiniBarMode;
  /** 「详情 »」钻取：打开大看板（唯一可点元素的唯一行为） */
  onDetail(): void;
  /** 译函数（`usage.mini.*` / `usage.moreN` / `usage.notCollected`） */
  t: TFn;
}

/**
 * 数据容器：档位与第 3 行条数取设置，三态与数据取看板查询。
 *
 * * 档位只发**预设**（`miniBarRange` 的三值之一），窗口边界由后端裁定（契约 §3 要点 1）——
 *   本容器没有自定义区间，也不自行推算窗口；
 * * 分组恒 `tool`：第 3 行是分工具汇总（D20 的前提）；
 * * **悬停即按需采集**（spec §7.2 的触发清单：「打开大看板 / 悬停浮窗」）：`useUsageCollect` 挂载
 *   采一次（`force=false`）+ 沿用它的兜底 ticker。重复调用只拿上次结果（§3 第 16 条：最小间隔与
 *   单飞由后端保证，前端不做二次节流）；三个读查询**绝不**触发扫描（§3 第 15 条）。
 */
export function UsageMiniBar({ scale, mode, onDetail, t }: UsageMiniBarProps) {
  const settings = useUsageSettingsQuery();
  useUsageCollect();
  // 扫描档位（Task 9）：浮窗**展开期间**＝有人在看 → 以 source="pet" 报心跳。
  // 与主窗心跳分开记账（后端按源并集判定）：本窗卸载只收回自己那份，不清主窗的；
  // 反向亦然（主窗失焦心跳报 false 也不会把浮窗这份清掉）。
  //
  // 可见性判据（评审 Fix 1）：**不能只按「浮窗挂着」报 true**。托盘/菜单把宠物窗口隐藏后，
  // 本 WebView 仍然活着（浮窗不会卸载）⇒ 若无条件续报 true，pet 源永不收回，后端档位
  // 钉在 idle，Away 的 30s 空闲节奏永远进不去。故 `visible` 同时跟两个信号：
  //  * `document.visibilityState`（窗口被隐藏/最小化时 WebView 会触发 visibilitychange）；
  //  * `pet-visibility-changed`（`FoxbellPet` / 托盘 emit，pet.tsx 同款消费），载荷 `{visible}`。
  // 30s 续期始终按**当前** visible 报：隐藏期间续报 false，档位才能落到 away。
  useEffect(() => {
    const send = (watching: boolean) => {
      void invoke("scan_bus_watch", { watching, source: "pet" }).catch(() => {});
    };
    let visible = document.visibilityState === "visible";
    let alive = true;
    const update = (next: boolean) => {
      if (!alive || next === visible) return;
      visible = next;
      send(visible);
    };
    // 挂载即报一次（浮窗展开 = 在看）；手动唤回的浮窗可长期停留，30s 续期必须
    // 小于后端 90s TTL，否则驻留期间会静默掉出「在看」
    send(visible);
    const id = window.setInterval(() => send(visible), HEARTBEAT_INTERVAL_MS);
    const onVisibilityChange = () => update(document.visibilityState === "visible");
    document.addEventListener("visibilitychange", onVisibilityChange);
    // 载荷过类型守卫：非布尔值忽略（浏览器 mock 下 listen 只返回 undefined）
    const un = listen<{ visible?: unknown }>("pet-visibility-changed", (e) => {
      if (!alive) return;
      const next = e.payload?.visible;
      if (typeof next === "boolean") update(next);
    }).catch(() => () => {});
    return () => {
      alive = false;
      window.clearInterval(id);
      document.removeEventListener("visibilitychange", onVisibilityChange);
      void un.then((f) => f());
      send(false); // 卸载收回本源的「在看」
    };
  }, []);
  const range: UsageRange = { preset: settings.data?.miniBarRange ?? "today" };
  const dashboard = useUsageDashboardQuery(range, "tool");

  return (
    <UsageMiniBarView
      status={dashboard.isPending ? "loading" : dashboard.isError ? "error" : "ready"}
      dash={dashboard.data ?? null}
      toolLimit={settings.data?.miniBarToolRows ?? MINI_BAR_TOOL_LIMIT}
      scale={scale}
      mode={mode}
      onDetail={onDetail}
      t={t}
    />
  );
}
