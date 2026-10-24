// 设置页「远程接入」⑤无头注入 / ⑥兼容发送子区（A2 自 RemoteSection 抽取，设计 §4.1）。
// **纯呈现子组件**：状态 owner 仍是 RemoteSection——
//   * 超时/并发两份草稿（含 ref 闸回填、3s 轮询不 clobber 编辑中值）仍在父级；
//   * 3s 轮询（status + 设备花名册）只有父级那一个，本组件不建第二个定时器；
//   * 保存/失败回执（toggle_headless / toggle_compat_send / set_headless_limits + toast）
//     由父级既有回调承接，本组件只负责触发。
// 开关真值同父级口径：缺键 = 新默认开（A1），只有显式 false 才是用户关闭；加载中
// （!status）禁用，不把缺键闪成关闭。兼容发送的资格字段（compatSendEligible）独立于
// 偏好默认值：只在「从关切到开」方向上置灰并提示需先完成只读探测。
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { useAppTranslation } from "@/hooks/use-app-translation";
import type { RemoteStatus } from "@/lib/api/remote";

export type HeadlessSettingsSectionProps = {
  status: RemoteStatus | null;
  busy: boolean;
  /** H4 超时草稿（毫秒字符串；父级首个 status 到达时回填，编辑中不被轮询覆盖） */
  timeout: string;
  /** H4 并发上限草稿（字符串，同上） */
  concurrency: string;
  onTimeoutChange: (value: string) => void;
  onConcurrencyChange: (value: string) => void;
  /** H3 无头总开关落盘（父级 applyHeadless：KV + 审计 + 广播，回读 status） */
  onToggleHeadless: (enabled: boolean) => void;
  /** #172/T8 兼容发送开关落盘（父级 applyCompatSend；开启仍由后端资格裁决） */
  onToggleCompatSend: (enabled: boolean) => void;
  /** H4 两件保存（父级 saveHeadlessLimits：后端 clamp 后落库） */
  onSaveLimits: () => void;
};

export function HeadlessSettingsSection({
  status,
  busy,
  timeout,
  concurrency,
  onTimeoutChange,
  onConcurrencyChange,
  onToggleHeadless,
  onToggleCompatSend,
  onSaveLimits,
}: HeadlessSettingsSectionProps) {
  const { t } = useAppTranslation();
  // H3 无头总开关态：唯一数据源 = remote_status.headlessEnabled（缺键/旧载荷 =
  // 新默认开；只有显式 false 才是用户关闭——加载中由 !status 禁用，不闪成关）。
  const headlessEnabled = status?.headlessEnabled ?? true;
  // #172/T8 兼容发送（实验）两个数据源（同源 remote_status）：
  // - compatSendEnabled：开关真值（缺键 = 新默认开，显式 false 保留）；
  // - compatSendEligible：前置判据（该 build 读链已取证 = 可从关切到开；
  //   false → 未取证提示照常显示，资格不从偏好默认继承）。
  const compatSendEnabled = status?.compatSendEnabled ?? true;
  const compatSendEligible = status?.compatSendEligible ?? false;
  // A2 修正（A1 遗留文案）：开关已开着时不再宣称「需先完成只读探测」——探测只是
  // 「从关切到开」这一方向的前置；资格判定本身不变（data-group 供顺序锁定位）。
  const showProbeHint = !compatSendEnabled && !compatSendEligible;

  return (
    <>
      {/* ⑤ 无头注入（H3）：**单一总开关**，缺键默认开、显式关闭保留；
          H4 的 watchdog 超时 / 并发上限两件同住本分组。开关关闭时兼容发送入口
          一律 403/置灰。加载中（!status）禁用，不把缺键显示成用户关闭。 */}
      <div data-group="headless" className="text-muted-foreground mt-4 text-[12.5px] font-semibold">
        {t("settings.remote.groupHeadless")}
      </div>
      <div className="flex items-center justify-between gap-4 py-3">
        <div className="flex-1">
          <label className="text-sm font-semibold">{t("settings.remote.headlessTitle")}</label>
          <p className="text-muted-foreground mt-0.5 text-xs">
            {t("settings.remote.headlessHint")}
          </p>
        </div>
        <Switch
          aria-label={t("settings.remote.headlessTitle")}
          checked={headlessEnabled}
          disabled={busy || !status}
          onCheckedChange={(v) => onToggleHeadless(v)}
        />
      </div>
      <div
        data-headless-limits
        className="flex flex-wrap items-start gap-4 border-t border-dashed py-3"
      >
        <div className="min-w-[190px] flex-1">
          <label htmlFor="headless-timeout" className="text-sm font-semibold">
            {t("settings.remote.headlessTimeoutLabel")}
          </label>
          <Input
            id="headless-timeout"
            type="number"
            min={1000}
            max={3600000}
            step={1000}
            value={timeout}
            onChange={(e) => onTimeoutChange(e.target.value)}
            className="mt-1"
          />
          <p className="text-muted-foreground mt-0.5 text-xs">
            {t("settings.remote.headlessTimeoutHint")}
          </p>
        </div>
        <div className="min-w-[160px] flex-1">
          <label htmlFor="headless-concurrency" className="text-sm font-semibold">
            {t("settings.remote.headlessConcurrencyLabel")}
          </label>
          <Input
            id="headless-concurrency"
            type="number"
            min={1}
            max={8}
            step={1}
            value={concurrency}
            onChange={(e) => onConcurrencyChange(e.target.value)}
            className="mt-1"
          />
          <p className="text-muted-foreground mt-0.5 text-xs">
            {t("settings.remote.headlessConcurrencyHint")}
          </p>
        </div>
        <Button
          className="mt-6"
          aria-label={t("settings.remote.headlessLimitsSave")}
          disabled={busy || !status}
          onClick={() => onSaveLimits()}
        >
          {t("settings.remote.save")}
        </Button>
      </div>

      {/* #172/T8 兼容发送（实验）：缺键默认开、显式关闭保留；**从关切到开
          仍要求读探测通过**——未取证且当前为关时提示先做只读探测（后端开启时同样拒：
          同一判据）。资格字段独立，不随偏好默认值变化。已验证的 Codex
          桌面版本不受此开关影响（它的发送行为逐字不变）。 */}
      <div data-group="compat" className="text-muted-foreground mt-4 text-[12.5px] font-semibold">
        {t("settings.remote.groupCompatSend")}
      </div>
      <div className="flex items-center justify-between gap-4 py-3">
        <div className="flex-1">
          <label className="text-sm font-semibold">{t("settings.remote.compatSendTitle")}</label>
          <p className="text-muted-foreground mt-0.5 text-xs">
            {t("settings.remote.compatSendHint")}
          </p>
          {showProbeHint && (
            <p className="text-muted-foreground mt-0.5 text-xs">
              {t("settings.remote.compatSendNeedsProbe")}
            </p>
          )}
        </div>
        <Switch
          aria-label={t("settings.remote.compatSendTitle")}
          checked={compatSendEnabled}
          disabled={busy || !status || (!compatSendEnabled && !compatSendEligible)}
          onCheckedChange={(v) => onToggleCompatSend(v)}
        />
      </div>
    </>
  );
}
