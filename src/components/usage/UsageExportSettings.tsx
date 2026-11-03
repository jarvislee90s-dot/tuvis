// 导出设置（2026-10-07 用户裁决 B1 + 本轮追加；2026-10-27 实机反馈轮 #3a 再追加）：
// **凡是只在导出时起作用的东西，入口就放在导出所在的地方**，不留在设置页里。
//
// 本弹层现在装三块：
//  * **出图模块**（#3a）：分享图画哪些段。总览 + 趋势**必选不可关**；分组分布四维度多选；
//    API 等效费用、工作小结 + 桌宠发言各自开关。**localStorage 记忆**（`lib/usage/exportModules`，
//    先例 `lib/toolFilter`），勾选即存、**不走 Rust 设置**（评语/姿态那条 `usage_set_settings`
//    通路保持原样，两边互不知晓也不需要知晓）。
//  * **分享图评语**（B1）：留空即用默认评语池，可用 `{range} {tokens} {hitPct} {models}` 变量；
//  * **分享图姿态**（2026-10-07）：token 用量里的那只宠物，在分享图上摆什么姿势。
//    原来在设置页叫「分享图姿态」，用户反馈「不太理解是什么的姿态」⇒ 标签写明「token 用量里的
//    宠物姿态」，并给两处都加 `title` 提示（用户建议的 information 浮窗做法）。
//
// 评语/姿态的**数据通路与设置页同一条**（`usageGetSettings` 读 + `usageSetSettings` 写），
// **不自立第二套**：两处各写各的就会漂。**刻意不做 query 缓存失效** —— 这两个字段已没有任何
// 「读出来展示」的消费方（设置页那两格就是被本项删掉的），导出路径每次动作都现读设置，
// 失效通知没有对象。副作用正好是想要的：本组件**不依赖 `QueryClientProvider`**，而导出条在
// 既有用例里是裸渲染的（出图模块走 localStorage 后这条性质保持不变）。
//
// ⚠️ **读是惰性的（点开才读一次）**，刻意不用 `useUsageSettingsQuery`：那个 hook 一挂载就发 IPC，
// 本组件住在看板吸顶头里 ⇒ 每次渲染平白多一次设置读取，还会打乱既有导出用例
// 按调用序编排的一次性 mock（「先给设置、再让落盘失败」）。
import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { useAppTranslation } from "@/hooks/use-app-translation";
import { usageGetSettings, usageSetSettings } from "@/lib/api/usage";
import { usageErrMsg } from "@/components/usage/usageErrors";
import { poseKeysFor } from "@/lib/usage/sheet";
import {
  loadExportModules,
  saveExportModules,
  type ExportModules,
} from "@/lib/usage/exportModules";
import type { UsageGroupBy } from "@/types/usage";

/** 分组分布四个子项（`UsageGroupBy` 恰好四值；勾选顺序即出图段序） */
const DISTRIBUTION_OPTIONS: readonly UsageGroupBy[] = ["tool", "project", "provider", "model"];

export function UsageExportSettings(props: { disabled?: boolean; title?: string }) {
  const { t } = useAppTranslation();
  const [open, setOpen] = useState(false);
  const [quote, setQuote] = useState("");
  const [pose, setPose] = useState("random");
  /** 出图模块（localStorage 读侧；点开才读、勾选即存，见文件头） */
  const [modules, setModules] = useState<ExportModules>(() => loadExportModules());
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState("");
  const [err, setErr] = useState("");

  /** 点开才读当前值（见文件头「读是惰性的」）；读失败也要如实可见，不能装作空评语 */
  const start = async () => {
    setOpen(true);
    setNote("");
    setErr("");
    setModules(loadExportModules());
    setBusy(true);
    try {
      const s = await usageGetSettings();
      setQuote(s.exportQuote ?? "");
      setPose(s.exportPose || "random");
    } catch (e) {
      setErr(usageErrMsg(e, t));
      console.error("[usage] 导出设置读取失败", e);
    } finally {
      setBusy(false);
    }
  };

  /** 模块勾选 → **立即**写 localStorage（无 Rust 往返）+ 行内「已保存」提示（复用既有 note 位） */
  const applyModules = (next: ExportModules) => {
    setModules(next);
    saveExportModules(next);
    setNote(t("usage.export.settingsSaved"));
  };

  const toggleDistribution = (groupBy: UsageGroupBy) => {
    applyModules({
      ...modules,
      distribution: modules.distribution.includes(groupBy)
        ? modules.distribution.filter((g) => g !== groupBy)
        : [...modules.distribution, groupBy],
    });
  };

  const save = async () => {
    setBusy(true);
    setErr("");
    try {
      // 两项**一次 patch** 提交：分两次发会让「保存成功」的语义变成「一半成功」✗
      await usageSetSettings({ exportQuote: quote, exportPose: pose });
      setOpen(false);
      setNote(t("usage.export.settingsSaved"));
    } catch (e) {
      // 与设置页同一条码表（`usage.rpc.<code>` + detail），不自造第二套错误文案
      setErr(usageErrMsg(e, t));
      console.error("[usage] 导出设置保存失败", e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <Button
        size="sm"
        variant="outline"
        data-testid="usage-export-settings-open"
        disabled={props.disabled}
        // 与另三个看板口径按钮同纪律（X7）：禁用时必须说明原因
        title={props.title}
        onClick={() => void start()}
      >
        {t("usage.export.settings")}
      </Button>
      {note ? (
        <span
          role="status"
          data-testid="usage-export-settings-note"
          className="text-muted-foreground text-xs"
        >
          {note}
        </span>
      ) : null}

      <Dialog open={open} onOpenChange={(v) => (v ? setOpen(true) : setOpen(false))}>
        <DialogContent data-testid="usage-export-settings-dialog">
          <DialogHeader>
            <DialogTitle>{t("usage.export.settingsTitle")}</DialogTitle>
          </DialogHeader>

          {/* 出图模块（2026-10-27 #3a）：勾选即存 localStorage（无 Rust 往返）、行内出「已保存」。
              总览 + 趋势必选：勾选框在场但**勾死 + 禁用**（不是藏起来——藏了会有人问「总览在哪关」）。
              取色全走主题 token（深色可读）；复用 `usage.group.*` 的四个分组标签，不另造文案。 */}
          <fieldset className="mt-1 space-y-2" data-testid="usage-export-modules">
            <legend className="text-sm font-medium">{t("usage.export.modulesTitle")}</legend>
            <label className="flex items-center gap-2 text-sm">
              <Checkbox data-testid="usage-export-module-overview" checked disabled />
              <span>{t("usage.export.modulesOverview")}</span>
            </label>
            <p className="text-muted-foreground text-xs">{t("usage.export.modulesOverviewHint")}</p>
            <div className="space-y-1.5">
              <span className="text-sm">{t("usage.card.distribution")}</span>
              <div className="grid grid-cols-2 gap-x-3 gap-y-1.5">
                {DISTRIBUTION_OPTIONS.map((groupBy) => (
                  <label key={groupBy} className="flex items-center gap-2 text-sm">
                    <Checkbox
                      data-testid={`usage-export-module-dist-${groupBy}`}
                      checked={modules.distribution.includes(groupBy)}
                      onCheckedChange={() => toggleDistribution(groupBy)}
                    />
                    <span>{t(`usage.group.${groupBy}`)}</span>
                  </label>
                ))}
              </div>
            </div>
            <label className="flex items-center gap-2 text-sm">
              <Checkbox
                data-testid="usage-export-module-cost"
                checked={modules.cost}
                onCheckedChange={(v) => applyModules({ ...modules, cost: v === true })}
              />
              <span>{t("usage.cost.title")}</span>
            </label>
            <label className="flex items-center gap-2 text-sm">
              <Checkbox
                data-testid="usage-export-module-work"
                checked={modules.work}
                onCheckedChange={(v) => applyModules({ ...modules, work: v === true })}
              />
              <span>{t("usage.export.modulesWork")}</span>
            </label>
            <p className="text-muted-foreground text-xs">{t("usage.export.modulesHint")}</p>
          </fieldset>

          {/* 评语 */}
          <label className="text-sm font-medium" htmlFor="usage-export-quote-input">
            {t("usage.export.quote")}
          </label>
          <Input
            id="usage-export-quote-input"
            data-testid="usage-export-quote-input"
            // 占位符提示可用变量（单花括号是字面量，不是 i18next 插值）
            placeholder={t("usage.export.quotePlaceholder")}
            value={quote}
            onChange={(e) => setQuote(e.currentTarget.value)}
          />
          <p className="text-muted-foreground text-xs">{t("usage.export.quoteHint")}</p>

          {/* 姿态：键族取自 Task 12 的 `poseKeysFor(11)`（**不得**自造第二份）；`look` 项保留 */}
          <label
            className="mt-2 text-sm font-medium"
            htmlFor="usage-export-pose"
            title={t("usage.export.poseHint")}
          >
            {t("usage.export.poseLabel")}
            <span className="text-muted-foreground ml-1 text-xs">ⓘ</span>
          </label>
          <select
            id="usage-export-pose"
            data-testid="usage-export-pose"
            title={t("usage.export.poseHint")}
            className="bg-background h-8 rounded-md border px-2 text-sm"
            value={pose}
            onChange={(e) => setPose(e.currentTarget.value)}
          >
            {poseKeysFor(11).map((key) => (
              <option key={key} value={key}>
                {t(`usage.pose.${key}`)}
              </option>
            ))}
          </select>
          <p className="text-muted-foreground text-xs">{t("usage.export.poseHint")}</p>

          {err ? (
            <p className="text-sm text-red-500" data-testid="usage-export-settings-error">
              {err}
            </p>
          ) : null}
          <div className="flex justify-end gap-2">
            <Button
              size="sm"
              variant="ghost"
              data-testid="usage-export-settings-cancel"
              onClick={() => setOpen(false)}
            >
              {t("usage.export.quoteCancel")}
            </Button>
            <Button
              size="sm"
              data-testid="usage-export-settings-save"
              disabled={busy}
              onClick={() => void save()}
            >
              {t("usage.export.quoteSave")}
            </Button>
          </div>
        </DialogContent>
      </Dialog>
    </>
  );
}
