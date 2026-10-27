// 导出动作条（计划② Task 11 步骤 5 + Task 12 步骤 7；spec P4）——复制文本 / 导出图片 / 复制图片 / 导出 CSV。
//
// 通道纪律（§3 第 24–28 条）：
//  * 导出**必须走 Rust 落盘**（`saveText` / `saveBytes` → 契约的两条落盘命令），**不走** `<a download>`；
//    本组件不碰对象 URL，也不自己拼下载链接。
//  * 定位复用既有 `revealDir`（经 `openContainingDir` 包装），传的是**文件路径本身**——不是父目录。
//  * 剪贴板（`copyText` / `copyImage`）只作**副本**入口：两条复制链一次 IPC 都不发。
//  * CSV 内容**全部来自后端**：`usageExportCsv` 返回成品字符串，本组件只给文件名并落盘 ——
//    不拼列、不改列序、不加表头（`.csv` 的 UTF-8 BOM 由 Rust 侧 `with_bom_if_csv` 加）。
//  * 导出失败是 `String` **自由文本**（不是 `{code, detail}`）⇒ 透传 `String(e)` 原因，**不得**喂给
//    `usageErrMsg`；而取数那一步（`usageExportCsv`）是**用量命令**，它的结构化码必须走 `usageErrMsg`
//    （自己 `String(e)` 会印出 `[object Object]`）。两条错误口径各归其位，见 `onCsv` 的两段。
//
// 交互纪律：
//  * 反馈是**行内** `role="status"`（testid `usage-export-toast`），**不用** toast —— 大看板是二级窗口，
//    未必挂载 `Toaster`，用 toast 会把反馈丢进虚空。
//  * `openContainingDir` **失败上抛**（Task 10 口径：本层不吞、也不翻文案）⇒ 处理器自带 `try`/`catch`：
//    写成 `onClick={() => void openContainingDir(path)}` 会把 rejection 吞掉、界面毫无反应。
//    定位失败时撤掉「打开所在目录」入口并显示原因 —— 文件其实已经落盘，但入口再留着只会一直失败。
//  * 记录页签**只留 CSV**：文本摘要与分享图都是看板口径的产物，放在记录页会产生口径歧义。
//  * 按钮一律 `@/components/ui/button` 原语、取色走主题 token（§3 第 19/40 条）。
//
// ⚠️ 依赖方向（**硬约束**，Task 12 起）：`useExportShare`（分享图整条链）**只允许**本文件 import ——
//    页面与其它组件一律不得 import，免得同一条导出链出现第二个装配点。Task 11 落盘时这里写的是
//    「**不得** import `useExportShare`」（那时模块还不存在，提前 import 会得到不可解析的 import，
//    而 `pnpm lint` 拦不住 ⇒ 以「绿」的错觉提交一个坏构建）；Task 12 把两个图片按钮接回来，
//    本文件就是那个唯一入口。props 形状**只增不改**：图片按钮不需要新字段
//    （`useExportShare(t)` 在组件内部按需读设置，看板数据由调用方逐次传进来）。
import { useState } from "react";
import { Button } from "@/components/ui/button";
import { UsageExportSettings } from "@/components/usage/UsageExportSettings";
import { usageErrMsg } from "@/components/usage/usageErrors";
import { usageExportCsv } from "@/lib/api/usage";
import { usageCostExportCsv } from "@/lib/api/usageV2";
import { buildTextSummary, csvFilename } from "@/lib/usage/exportText";
import { copyText, openContainingDir, saveText } from "@/lib/usage/saveFile";
import { useExportShare } from "@/lib/usage/useExportShare";
import type { TFn } from "@/lib/usage/range";
import type { UsageDashboard, UsageFilters, UsageGroupBy, UsageRange } from "@/types/usage";

/** 页签：与页面同形的两个字面量（不 import 页面的私有类型，避免组件反向依赖页面） */
type ExportTab = "board" | "records";

/** 费用 CSV 文件名：`tuvis-api-equivalent-<window>-<YYYYMMDD-HHmmss>.csv`（稳定可排序） */
function costCsvFilename(range: UsageRange, now: Date = new Date()): string {
  const p = (v: number) => String(v).padStart(2, "0");
  const stamp =
    `${now.getFullYear()}${p(now.getMonth() + 1)}${p(now.getDate())}-` +
    `${p(now.getHours())}${p(now.getMinutes())}${p(now.getSeconds())}`;
  const base =
    range.preset === "custom" && range.from && range.to
      ? `${range.from}_${range.to}`
      : range.preset;
  return `tuvis-api-equivalent-${base}-${stamp}.csv`;
}

export function UsageExportActions(props: {
  /** 看板数据；`null` = 查询未就绪 / 记录页签（三个看板口径按钮据此禁用，CSV 不依赖它） */
  dash: UsageDashboard | null;
  range: UsageRange;
  tab: ExportTab;
  /**
   * 导出筛选。**由页面按当前页签给**（2026-10-06 用户裁决「CSV 要接收记录页的筛选条件」）：
   * * 记录页签 → `useRecordsFilters` 的**第三层守卫产物**（工具 chips + 子代理模式；日档不含
   *   `subagentMode`）—— 屏幕上筛什么，这份文件就是什么；
   * * 看板页签 → `{}`（看板没有筛选控件）。
   * 本组件**只透传**，不自行拼装、也不加第二层筛选。
   */
  filters: UsageFilters;
  groupBy: UsageGroupBy;
  t: TFn;
}) {
  const { t, tab, dash, range, groupBy, filters } = props;
  /** 行内反馈（成功与失败共用一条通路：后一条覆盖前一条） */
  const [note, setNote] = useState("");
  /** 是否有导出动作在途（X4）——四个动作共用一道门闸，见 `run` */
  const [busy, setBusy] = useState(false);
  /** 落盘绝对路径；空串 = 没有可定位的文件（失败态与初始态都是它；复制图片**不**置它） */
  const [savedPath, setSavedPath] = useState("");
  /** 分享图（出图 + 落盘 / 出图 + 剪贴板）：两个动作都是点击时才干活，挂载期零 IPC（见 hook 注释） */
  const share = useExportShare(t);

  /**
   * 单一在途门闸（X4 缺陷修复）：四个导出动作**都不允许重入**。
   *
   * 为什么必须有（一致性 + 真缺陷）：同一个产品里阶段① 的导出按钮**有** `exporting` 防重入与
   * 进行中文案（`UsageStatusSection.tsx:64/360/363`），而这里四个 handler 全是 `void onX()`、
   * 按钮一个都不禁用 ⇒ 狂点会并发出图 / 并发落盘；而分享图文件名在修复 X1 之前**是固定的**
   * （`mam-usage-<preset>.png`）⇒ 两次并发落盘会互相覆盖，用户拿到的可能是半张图。
   * 把「置忙 → 执行 → 复位」收在这一个函数里、五个入口共用，杜绝漏掉某一个按钮。
   */
  const run = async (fn: () => Promise<void>) => {
    if (busy) return;
    setBusy(true);
    try {
      await fn();
    } finally {
      setBusy(false);
    }
  };

  const onCopyText = async () => {
    if (!dash) return;
    const r = await copyText(buildTextSummary(dash, t));
    // 复制失败走**复制专属**文案（X5 修复）：原来复用了 `usage.export.saveFailed`（「导出失败：…」），
    // 可这条路径**根本没有导出**任何文件——文案把人引向磁盘，而真正要去查的是剪贴板权限。
    setNote(r.ok ? t("usage.export.copied") : t("usage.export.copyFailed", { error: r.error }));
  };

  const onCsv = async () => {
    let csv: string;
    try {
      csv = await usageExportCsv(range, groupBy, filters);
    } catch (e) {
      // 用量命令：结构化 `{ code, detail }` → 码表文案（§3 第 28 条）
      setSavedPath("");
      setNote(t("usage.export.saveFailed", { error: usageErrMsg(e, t) }));
      return;
    }
    // ⚠️ **CSV 是「没量到 token 就不显示数值」这条裁决的明确例外**（2026-10-06）：它是**数据文件**
    // ——11 列原始整数、给 Excel / 脚本吃的，**保留原始 0**，不做展示态替换。理由：把 `—` 写进数字列会
    // 让下游（求和、透视、脚本）整列变成文本、并且把「0」与「没有这一列」混为一谈；而展示面
    // （看板 / 浮窗 / 记录页 / 文本摘要 / 分享图）才需要 `—`。CSV 的内容**全部由后端生成**
    // （`usageExportCsv` → `usage_export_csv`），前端只给文件名并落盘，故这条口径的落点在后端与这里。
    const r = await saveText(csv, csvFilename(range));
    if (r.ok) {
      setSavedPath(r.path);
      // 落盘绝对路径**可见**（用户可以自己复制；「打开所在目录」只是便捷入口）
      setNote(t("usage.export.savedTo", { path: r.path }));
    } else {
      // 落盘命令：自由文本原因 → 原样透传（**不得**走 usageErrMsg）
      setSavedPath("");
      setNote(t("usage.export.saveFailed", { error: r.error }));
    }
  };

  /** 费用 CSV：只导当前时间窗全部费用行，**不接收**记录页筛选 / B 局部筛选 / 旧 groupBy。 */
  const onCostCsv = async () => {
    let content: string;
    try {
      content = await usageCostExportCsv(range);
    } catch (e) {
      setSavedPath("");
      setNote(t("usage.export.saveFailed", { error: usageErrMsg(e, t) }));
      return;
    }
    // 前端不添加 BOM：`.csv` 的 UTF-8 BOM 由 Rust 落盘层统一处理，前置会变成双 BOM。
    const r = await saveText(content, costCsvFilename(range));
    if (!r.ok) {
      setSavedPath("");
      setNote(t("usage.export.saveFailed", { error: r.error }));
      return;
    }
    // 文件已落盘 ⇒ 即使定位失败也保留绝对路径，不提示重新导出
    setSavedPath(r.path);
    setNote(t("usage.export.savedTo", { path: r.path }));
    try {
      await openContainingDir(r.path);
    } catch (e) {
      setNote(
        `${t("usage.export.savedTo", { path: r.path })} · ${t("usage.export.revealFailed", {
          error: String(e),
        })}`
      );
    }
  };

  const onImage = async () => {
    if (!dash) return;
    try {
      const r = await share.exportImage(dash);
      if (r.ok) {
        setSavedPath(r.path);
        setNote(t("usage.export.savedTo", { path: r.path }));
      } else {
        // 落盘失败：自由文本原因 → 原样透传（与 `onCsv` 同款）
        setSavedPath("");
        setNote(t("usage.export.saveFailed", { error: r.error }));
      }
    } catch (e) {
      // 出图这条链**会**抛（渲染 / `toBlob` 失败都走 rejection）：处理器是 `onClick={() => void onX()}`，
      // `void` 会把 rejection 吞掉 —— 没有这个 catch 就只剩控制台 unhandled rejection、界面「点了没反应」
      setSavedPath("");
      setNote(t("usage.export.saveFailed", { error: String(e) }));
    }
  };

  const onCopyImage = async () => {
    if (!dash) return;
    try {
      const r = await share.copyImage(dash);
      if (r.ok) {
        // best-effort 副本：成功**也不**置 `savedPath` —— 剪贴板里没有路径，没什么可「打开所在目录」的
        setNote(t("usage.export.copiedImage"));
      } else {
        setNote(t("usage.export.copyFailed", { error: r.error }));
      }
    } catch (e) {
      // 与 `onImage` 同因：`void` 不吞错误，失败必须行内可见
      setNote(t("usage.export.copyFailed", { error: String(e) }));
    }
  };

  const onOpenDir = async () => {
    if (!savedPath) return;
    try {
      await openContainingDir(savedPath);
    } catch (e) {
      // 定位失败不改「导出成功」的结论（文件已在磁盘上），但入口撤掉 + 如实报原因：
      // 留着入口只会让用户反复点、反复失败（开发机设了 TUVIS_HOME 时导出目录会重定向，正会走到这里）
      setSavedPath("");
      setNote(t("usage.export.saveFailed", { error: String(e) }));
    }
  };

  return (
    // min-w-0（实机反馈轮 2026-10-27 #3b）：本根是页头动作槽（min-w-0 flex-wrap）的
    // flex 子项，不放开 min-width 会被内容（超长「已保存」路径）撑到不收缩，
    // 连锁把收缩压力传给标题；自身换行 + 末尾反馈截断，超宽到此为止
    <div className="flex min-w-0 flex-wrap items-center gap-2">
      {/* 三个「看板口径」按钮（复制文本 / 导出图片 / 复制图片）：记录页签下**不再整组消失**，
          改为**禁用 + `title` 说明原因**（X7 修复）。口径不变（文本摘要与分享图都是看板口径的产物，
          放在记录页会产生口径歧义），但「按钮凭空消失」会让用户以为功能坏了、也看不出为什么。
          `disabled` 里带上 `busy`：四个动作共用 `run` 的那道门闸（X4）。 */}
      {(
        [
          ["usage-export-copy-text", "usage.export.copyText", onCopyText],
          ["usage-export-image", "usage.export.image", onImage],
          ["usage-export-copy-image", "usage.export.copyImage", onCopyImage],
        ] as const
      ).map(([testid, key, fn]) => (
        <Button
          key={testid}
          size="sm"
          variant="outline"
          data-testid={testid}
          disabled={!dash || busy || tab !== "board"}
          title={tab !== "board" ? t("usage.export.boardOnly") : undefined}
          onClick={() => void run(fn)}
        >
          {t(key)}
        </Button>
      ))}

      {/* 导出设置（2026-10-07：评语 B1 + 姿态本轮，两项都从设置页整体搬来）。
          与三个看板口径按钮**同纪律**：记录页签下在位但禁用（不是凭空消失）——
          它们只作用于看板口径的两份产物（文本摘要 / 分享图）。 */}
      <UsageExportSettings
        disabled={!dash || busy || tab !== "board"}
        title={tab !== "board" ? t("usage.export.boardOnly") : undefined}
      />

      <Button
        size="sm"
        variant="outline"
        data-testid="usage-export-csv"
        disabled={busy}
        onClick={() => void run(onCsv)}
      >
        {t("usage.export.csv")}
      </Button>

      {/* 费用 CSV 与 Token CSV 并列；只导当前时间窗全部费用行，不随记录页筛选/分组变化 */}
      <Button
        size="sm"
        variant="outline"
        data-testid="usage-export-cost-csv"
        disabled={busy}
        title={t("usage.cost.exportCsvUnit")}
        onClick={() => void run(onCostCsv)}
      >
        {t("usage.cost.exportCsv")}
      </Button>

      {savedPath ? (
        <Button
          size="sm"
          variant="ghost"
          data-testid="usage-export-open-dir"
          onClick={() => void onOpenDir()}
        >
          {t("usage.export.openDir")}
        </Button>
      ) : null}

      {/* 行内反馈：在途时出进行中文案（与阶段① 的 `exporting` 同款）。
          ⚠️ **不换行**（X6）：`savedTo` 带的是绝对路径，原先 `break-all` 会把它折成好几行，
          而本组件就住在看板的**吸顶头**里 ⇒ 头部一忽儿高、一忽儿矮，底下的内容跟着跳。
          改用 `truncate` + `title`：一眼看得出来是成功还是失败，完整路径 hover 可读
          （`textContent` 仍是完整串 ⇒ 既有断言不受影响）。 */}
      {busy ? (
        <span
          role="status"
          data-testid="usage-export-toast"
          className="text-muted-foreground text-xs"
        >
          {t("usage.export.exporting")}
        </span>
      ) : note ? (
        <span
          role="status"
          data-testid="usage-export-toast"
          title={note}
          className="text-muted-foreground max-w-[18rem] min-w-0 truncate text-xs"
        >
          {note}
        </span>
      ) : null}
    </div>
  );
}
