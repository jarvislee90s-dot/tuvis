// 设置页「④ 工具特殊配置」里的 ZCode 折叠卡（A5，设计 §4.1/§4.2/§4.3）。
//
// **卡的边界**（逐条对应设计红线）：
// - 默认收起；收起态只有卡头（工具图标 + ZCode + 简短配置状态 + 展开指示）。展开状态是
//   **临时 UI 状态**，链接值由后端 settings 单独持久化：折叠既不删配置、也不触发连接。
// - 展开内容是官方远控链接的输入框 + 保存 / 删除 / 复制；**空白保存与删除同源**（都写
//   空串，服务端据此删键）。校验单点在服务端（A4），失败回执**紧邻输入框**并挂
//   aria-describedby／aria-invalid（不只有 toast），且**保留已存值与草稿**。
// - 复制与「打开官方页面」都用**已保存值**（不是草稿），且都是用户显式动作；挂载/展开
//   从不自动扫描登录态、不注册设备、不打开或关闭用户的 ZCode。
// - **语义红线（§4.2/§4.3）**：A4 的 `saved` 只表示「格式已校验并落库」，本卡据此显示
//   「连接待确认」，**任何情况下都不宣称已联机**；真实连接/官方身份状态由 B 包的适配器
//   回填到同一张卡。链接状态由本卡持有，不上传到别处。
// - 失效证据分层里「仅打开外部页看到 Not Found」这一档：外跳后 Tuvis 读不到那个页面，
//   因此外跳**不改变**卡上任何状态，也不自动删设置（用户可手工替换）。
import { useCallback, useEffect, useState } from "react";
import { ChevronDown, ChevronRight } from "lucide-react";
import { toast } from "sonner";
import { openUrl } from "@tauri-apps/plugin-opener";
import { invoke } from "@tauri-apps/api/core";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { ToolIcon } from "@/components/common/ToolIcon";
import { SETTINGS_FIELD, SETTINGS_HINT, SETTINGS_NOTE } from "@/components/settings/typography";
import { useAppTranslation } from "@/hooks/use-app-translation";
import { formatInvokeError } from "@/lib/invokeError";
import {
  getZcodeRemoteConfig,
  setZcodeRemoteConfig,
  type ZcodeRemoteConfig,
} from "@/lib/api/zcodeRemote";
// 连接 DTO 唯一定义处（`src/lib/api/zcodeControl.ts`，与 Rust golden JSON 对拍）：
// 设置卡只 import，不再本地复制枚举/字段，避免契约漂移。
import {
  isConnectionState,
  type ConnectionState,
  type ConnectionView,
} from "@/lib/api/zcodeControl";

// DOM id 单点（label 关联／aria-controls／错误关联都引它，改一处即可）
const PANEL_ID = "zcode-remote-card-panel";
const INPUT_ID = "zcode-remote-url";
const HINT_ID = `${INPUT_ID}-hint`;
const ERROR_ID = `${INPUT_ID}-error`;
const TUTORIAL_ID = "zcode-remote-tutorial";
const PROVIDER_ID = "zcode-remote-provider";

// 官方授权通道（T7.2）：BigModel 已实跑回环授权；ZAI 目前只有官方源码/模拟证据，
// **没有** live 实跑证据，因此选项禁用且授权按钮一律不开闸（证据门，不是偏好）。
const PROVIDER_BIGMODEL = "bigmodel";
const PROVIDER_ZAI = "zai";
const ZAI_LIVE_VERIFIED = false;
type ProviderId = typeof PROVIDER_BIGMODEL | typeof PROVIDER_ZAI;

// `ConnectionView` 的**白名单**消费形态：只取公共 DTO 的四个字段，后端多给的
// URL/token 一律丢弃，因此任何凭据都不会因为 IPC 回执而被渲染或缓存。
// `deviceLabel` 仅供显示，不做绑定。枚举/字段类型来自 `@/lib/api/zcodeControl`。
/** 严格解析连接视图：形态不符 → null（= 未知），绝不猜成 needs_auth/paired。 */
function parseConnection(raw: unknown): ConnectionView | null {
  if (!raw || typeof raw !== "object") return null;
  const view = raw as Record<string, unknown>;
  if (!isConnectionState(view.state)) return null;
  return {
    state: view.state,
    generation: typeof view.generation === "number" ? view.generation : 0,
    reasonCode: typeof view.reasonCode === "string" ? view.reasonCode : null,
    deviceLabel: typeof view.deviceLabel === "string" ? view.deviceLabel : null,
  };
}

// 连接态 → 本地化键（每态一句，绝不靠「是否有链接」推导配对）
const CONNECTION_LABEL_KEYS: Record<ConnectionState, string> = {
  needs_config: "settings.remote.zcode.connNeedsConfig",
  needs_auth: "settings.remote.zcode.connNeedsAuth",
  connecting: "settings.remote.zcode.connConnecting",
  paired: "settings.remote.zcode.connPaired",
  offline: "settings.remote.zcode.connOffline",
  conflict: "settings.remote.zcode.connConflict",
  unknown: "settings.remote.zcode.connUnknown",
};

// 冻结错误码 → 本卡原因键（词表外的码回退域入口文案，不展示码原文）
const CONNECTION_REASON_KEYS: Record<string, string> = {
  needs_config: "settings.remote.zcode.reasonNeedsConfig",
  needs_auth: "settings.remote.zcode.reasonNeedsAuth",
  offline: "settings.remote.zcode.reasonOffline",
  conflict: "settings.remote.zcode.reasonConflict",
  unknown: "settings.remote.zcode.reasonUnknown",
};

export function ZcodeRemoteSettingsCard() {
  const { t } = useAppTranslation();
  // 展开态 = 页内临时状态（不落库、不进 i18n、不参与任何判定）
  const [expanded, setExpanded] = useState(false);
  // 已存配置（null = 首拍未到或读取失败——**不**当成「未配置」）
  const [config, setConfig] = useState<ZcodeRemoteConfig | null>(null);
  // 读取失败：如实说「读不到」，不伪造未配置、也不判链接永久失效
  const [readFailed, setReadFailed] = useState(false);
  // 草稿与已存值**分开**：输入框改动作草稿，保存才走服务端（复制/外跳只认已存值）
  const [draft, setDraft] = useState("");
  // 保存失败的就地错误（已本地化）
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [tutorialOpen, setTutorialOpen] = useState(false);
  // ---- T7.2 官方账号授权/连接 ----
  // 授权通道：默认 BigModel（唯一有 live 证据的通道）；ZAI 未 live 不开闸。
  const [provider, setProvider] = useState<ProviderId>(PROVIDER_BIGMODEL);
  // 真实连接态（null = 首拍未到/读取失败 = **未知**，不是 needs_auth 也不是 paired）
  const [connection, setConnection] = useState<ConnectionView | null>(null);
  // 授权/连接/断开进行中（同一时刻只允许一个控制动作）
  const [controlBusy, setControlBusy] = useState<"authorize" | "connect" | "disconnect" | null>(
    null
  );
  // 控制动作失败的就地错误（已本地化；不弹 toast 重复播报）
  const [controlError, setControlError] = useState<string | null>(null);

  // 连接态唯一来源 = 后端 `zcode_control_status`（同 Hub 视图，桌面与手机一致）。
  // 链接已保存**从不**被推导成已配对；读取失败按「未知」显示，不猜。
  const refreshConnection = useCallback(async () => {
    try {
      setConnection(parseConnection(await invoke<unknown>("zcode_control_status")));
    } catch (e) {
      console.error("zcode control status read failed:", e);
      setConnection(null);
    }
  }, []);

  useEffect(() => {
    void refreshConnection();
  }, [refreshConnection]);

  const providerVerified = provider === PROVIDER_BIGMODEL || ZAI_LIVE_VERIFIED;
  const connectionState: ConnectionState = connection?.state ?? "unknown";
  const busy = saving || controlBusy !== null;

  // 授权 / 连接 / 断开：只走 IPC，参数里没有 URL/token（凭据只在本机 Rust 内存里）；
  // 回执按四字段白名单消费。失败就地报错并照实重读后端状态。
  const runControl = useCallback(
    async (action: "authorize" | "connect" | "disconnect") => {
      if (action === "authorize" && !providerVerified) return;
      setControlBusy(action);
      setControlError(null);
      try {
        const raw =
          action === "authorize"
            ? await invoke<unknown>("zcode_control_authorize", { provider })
            : action === "connect"
              ? await invoke<unknown>("zcode_control_connect")
              : await invoke<unknown>("zcode_control_disconnect");
        setConnection(parseConnection(raw));
      } catch (e) {
        setControlError(formatInvokeError(e, t));
        await refreshConnection();
      } finally {
        setControlBusy(null);
      }
    },
    [provider, providerVerified, refreshConnection, t]
  );

  // 挂载读一次（只读 settings，不触网、不探测连接——§4.3 的「读取即复验」在服务端）
  useEffect(() => {
    let alive = true;
    void (async () => {
      try {
        const cfg = await getZcodeRemoteConfig();
        if (!alive) return;
        // 载荷缺失（旧后端/异常）→ 如实说「读不到」，不猜成「未配置」（§4.3 不擅自判失效）
        if (!cfg || typeof cfg.configState !== "string") {
          setReadFailed(true);
          return;
        }
        setConfig(cfg);
        setReadFailed(false);
        setDraft(cfg.url ?? "");
      } catch (e) {
        if (!alive) return;
        console.error("zcode remote config read failed:", e);
        setReadFailed(true);
      }
    })();
    return () => {
      alive = false;
    };
  }, []);

  const savedUrl = config?.url ?? null;
  const configState = readFailed ? "readFailed" : (config?.configState ?? "loading");
  // 首拍未回来之前禁止编辑/保存：否则用户刚粘贴的链接会被迟到的 GET 无条件覆盖。
  const loading = config === null && !readFailed;
  // 卡头简短状态：四种形态各一句，全部只说「本机配置」，不说联机
  const stateLabel =
    configState === "saved"
      ? t("settings.remote.zcode.stateSaved")
      : configState === "invalid"
        ? t("settings.remote.zcode.stateInvalid")
        : configState === "missing"
          ? t("settings.remote.zcode.stateMissing")
          : configState === "readFailed"
            ? t("settings.remote.zcode.readFailed")
            : t("settings.remote.zcode.stateLoading");

  // 保存 / 删除同一条出口（空白 = 删除；服务端按格式校验裁决）
  const persist = useCallback(
    async (raw: string) => {
      setSaving(true);
      setError(null);
      try {
        const next = await setZcodeRemoteConfig(raw);
        setConfig(next);
        setReadFailed(false);
        setDraft(next.url ?? "");
        // 保存成功只**刷新**后端连接态：改链接会让 Hub 推进代际并断旧连接，
        // 新态由后端说了算——不在这里凭「Saved」自证任何联机状态。
        await refreshConnection();
        toast.success(
          next.configState === "missing"
            ? t("settings.remote.zcode.deletedToast")
            : t("settings.remote.zcode.savedToast")
        );
      } catch (e) {
        // 失败**不**动已存值、不清草稿（用户可就地改几个字符再试），也不刷新连接态
        setError(formatInvokeError(e, t));
      } finally {
        setSaving(false);
      }
    },
    [refreshConnection, t]
  );

  // 复制 = 已保存值（草稿没保存出去就不该出现在剪贴板里）；无已存值时钮本就更disabled
  const copySaved = useCallback(async () => {
    if (savedUrl === null) return;
    try {
      await navigator.clipboard.writeText(savedUrl);
      toast.success(t("settings.remote.copied"));
    } catch (e) {
      console.error("clipboard write failed:", e);
      toast.error(t("settings.remote.copyFailed"));
    }
  }, [savedUrl, t]);

  // 显式外跳：用户点了才开，且用已保存值；开完不改状态（Tuvis 读不到那个页面）
  const openOfficial = useCallback(async () => {
    if (savedUrl === null) return;
    try {
      await openUrl(savedUrl);
    } catch (e) {
      console.error("open official page failed:", e);
      toast.error(formatInvokeError(e, t));
    }
  }, [savedUrl, t]);

  return (
    <div data-tool-card="zcode" className="mt-2 overflow-hidden rounded-xl border">
      {/* 卡头 = 唯一的收起/展开入口（原生 button：Enter／Space 免费拿到） */}
      <button
        type="button"
        data-testid="zcode-card-header"
        aria-expanded={expanded}
        aria-controls={PANEL_ID}
        onClick={() => setExpanded((v) => !v)}
        className="bg-muted/40 hover:bg-muted flex w-full items-center gap-2 px-4 py-2.5 text-left"
      >
        <ToolIcon toolId="zcode" size={18} />
        <span className={SETTINGS_FIELD}>ZCode</span>
        <span data-testid="zcode-config-state" className={`ml-auto ${SETTINGS_HINT}`}>
          {stateLabel}
        </span>
        {/* 展开指示（装饰，状态已由 aria-expanded 承载） */}
        {expanded ? (
          <ChevronDown className="text-muted-foreground flex-none" size={16} aria-hidden />
        ) : (
          <ChevronRight className="text-muted-foreground flex-none" size={16} aria-hidden />
        )}
      </button>

      {expanded && (
        <div
          id={PANEL_ID}
          data-testid="zcode-card-panel"
          role="region"
          aria-label={t("settings.remote.zcode.title")}
          className="space-y-2 border-t p-4"
        >
          <p className={SETTINGS_HINT}>{t("settings.remote.zcode.desc")}</p>

          <label htmlFor={INPUT_ID} className={`block ${SETTINGS_FIELD}`}>
            {t("settings.remote.zcode.urlLabel")}
          </label>
          <Input
            id={INPUT_ID}
            value={draft}
            spellCheck={false}
            autoComplete="off"
            aria-invalid={error !== null}
            aria-describedby={error !== null ? `${HINT_ID} ${ERROR_ID}` : HINT_ID}
            disabled={saving || loading}
            onChange={(e) => setDraft(e.target.value)}
            className="font-mono text-xs"
          />
          <div className="flex flex-wrap items-center gap-2">
            <Button
              size="sm"
              data-testid="zcode-save"
              disabled={saving || loading}
              onClick={() => void persist(draft)}
            >
              {t("settings.remote.save")}
            </Button>
            {/* 删除 = 空白保存同源（同一条 persist("")，不是第二套动作） */}
            <Button
              size="sm"
              variant="outline"
              data-testid="zcode-delete"
              disabled={saving || loading || configState !== "saved"}
              onClick={() => void persist("")}
            >
              {t("settings.remote.zcode.delete")}
            </Button>
            <Button
              size="sm"
              variant="outline"
              data-testid="zcode-copy"
              disabled={savedUrl === null}
              onClick={() => void copySaved()}
            >
              {t("settings.remote.copy")}
            </Button>
          </div>

          {/* 就地错误：紧邻输入框，role=alert 播报；不是只弹 toast */}
          {error !== null && (
            <p
              id={ERROR_ID}
              data-testid="zcode-url-error"
              role="alert"
              className="rounded-lg bg-rose-50 px-2.5 py-1.5 text-xs text-rose-600 dark:bg-rose-500/10 dark:text-rose-400"
            >
              {error}
            </p>
          )}
          <p id={HINT_ID} className={SETTINGS_HINT}>
            {t("settings.remote.zcode.urlHint")}
          </p>
          {configState === "invalid" && (
            <p data-testid="zcode-invalid-note" className={SETTINGS_HINT}>
              {t("settings.remote.zcode.invalidNote")}
            </p>
          )}
          {configState === "saved" && (
            <p data-testid="zcode-pending-note" className={SETTINGS_HINT}>
              {t("settings.remote.zcode.pendingNote")}
            </p>
          )}
          <p className={SETTINGS_NOTE}>{t("settings.remote.zcode.deleteNote")}</p>

          {/* ---- 官方账号授权与连接（T7.2）：授权走本机 IPC，手机永不持有凭据 ---- */}
          <div
            data-testid="zcode-account"
            className="bg-muted/20 space-y-2 rounded-lg border border-dashed p-3"
          >
            <p className={SETTINGS_FIELD}>{t("settings.remote.zcode.accountTitle")}</p>
            <p className={SETTINGS_HINT}>{t("settings.remote.zcode.accountDesc")}</p>

            <label htmlFor={PROVIDER_ID} className={`block ${SETTINGS_FIELD}`}>
              {t("settings.remote.zcode.providerLabel")}
            </label>
            <select
              id={PROVIDER_ID}
              data-testid="zcode-provider"
              value={provider}
              disabled={busy}
              onChange={(e) =>
                setProvider(e.target.value === PROVIDER_ZAI ? PROVIDER_ZAI : PROVIDER_BIGMODEL)
              }
              className="border-input bg-background focus-visible:border-ring focus-visible:ring-ring/50 dark:bg-input/30 h-8 w-full rounded-md border px-2 text-sm outline-none focus-visible:ring-[3px] disabled:cursor-not-allowed disabled:opacity-50"
            >
              <option value={PROVIDER_BIGMODEL}>
                {t("settings.remote.zcode.providerBigmodel")}
              </option>
              <option value={PROVIDER_ZAI} disabled={!ZAI_LIVE_VERIFIED}>
                {t("settings.remote.zcode.providerZai")}
              </option>
            </select>
            {/* ZAI 禁用的**可见原因**：未验证 + live 未证（证据门，不是隐藏开关） */}
            {!ZAI_LIVE_VERIFIED && (
              <p data-testid="zcode-zai-reason" className={SETTINGS_HINT}>
                {t("settings.remote.zcode.providerZaiReason")}
              </p>
            )}

            <div className="flex flex-wrap items-center gap-2">
              <Button
                size="sm"
                data-testid="zcode-authorize"
                aria-busy={controlBusy === "authorize"}
                disabled={busy || !providerVerified}
                onClick={() => void runControl("authorize")}
              >
                {t("settings.remote.zcode.authorize")}
              </Button>
              <Button
                size="sm"
                variant="outline"
                data-testid="zcode-connect"
                aria-busy={controlBusy === "connect"}
                disabled={
                  busy ||
                  connectionState === "paired" ||
                  connectionState === "connecting" ||
                  connectionState === "conflict"
                }
                onClick={() => void runControl("connect")}
              >
                {t("settings.remote.zcode.connect")}
              </Button>
              <Button
                size="sm"
                variant="outline"
                data-testid="zcode-disconnect"
                aria-busy={controlBusy === "disconnect"}
                disabled={busy || connection === null}
                onClick={() => void runControl("disconnect")}
              >
                {t("settings.remote.zcode.disconnect")}
              </Button>
            </div>

            {/* 真实连接态：唯一权威 = 后端 status；已保存链接不参与判定 */}
            <div className="flex flex-wrap items-baseline gap-x-2 gap-y-1">
              <span className={SETTINGS_HINT}>{t("settings.remote.zcode.connectionLabel")}</span>
              <span
                data-testid="zcode-connection-state"
                data-connection-state={connectionState}
                aria-live="polite"
                className={SETTINGS_FIELD}
              >
                {t(CONNECTION_LABEL_KEYS[connectionState])}
              </span>
              {connection?.deviceLabel !== null && connection?.deviceLabel !== undefined && (
                <span data-testid="zcode-device-label" className={SETTINGS_HINT}>
                  {t("settings.remote.zcode.deviceLabel", { label: connection.deviceLabel })}
                </span>
              )}
            </div>
            {connection?.reasonCode !== null && connection?.reasonCode !== undefined && (
              <p
                data-testid="zcode-connection-reason"
                data-reason-code={connection.reasonCode}
                className={SETTINGS_HINT}
              >
                {CONNECTION_REASON_KEYS[connection.reasonCode]
                  ? t(CONNECTION_REASON_KEYS[connection.reasonCode])
                  : t("errors.zcodeControl")}
              </p>
            )}
            {controlError !== null && (
              <p
                data-testid="zcode-control-error"
                role="alert"
                className="rounded-lg bg-rose-50 px-2.5 py-1.5 text-xs text-rose-600 dark:bg-rose-500/10 dark:text-rose-400"
              >
                {controlError}
              </p>
            )}
            <p className={SETTINGS_NOTE}>{t("settings.remote.zcode.authorizeNote")}</p>
          </div>

          {/* 获取教程：显式动作才展开；应用内取链四步，不承诺自动登录／网页引导／固定有效期 */}
          <Button
            size="sm"
            variant="ghost"
            className="px-0 text-xs"
            data-testid="zcode-tutorial-toggle"
            aria-expanded={tutorialOpen}
            aria-controls={TUTORIAL_ID}
            onClick={() => setTutorialOpen((v) => !v)}
          >
            {tutorialOpen
              ? t("settings.remote.zcode.tutorialHide")
              : t("settings.remote.zcode.tutorialShow")}
          </Button>
          {tutorialOpen && (
            <div
              id={TUTORIAL_ID}
              data-testid="zcode-tutorial"
              className="bg-muted/30 rounded-lg border border-dashed p-3"
            >
              <p className={SETTINGS_HINT}>{t("settings.remote.zcode.tutorialIntro")}</p>
              <ol className="text-muted-foreground mt-1 list-decimal space-y-0.5 pl-4 text-xs">
                <li>{t("settings.remote.zcode.tutorialStep1")}</li>
                <li>{t("settings.remote.zcode.tutorialStep2")}</li>
                <li>{t("settings.remote.zcode.tutorialStep3")}</li>
                <li>{t("settings.remote.zcode.tutorialStep4")}</li>
              </ol>
              <p className={SETTINGS_NOTE}>{t("settings.remote.zcode.tutorialNoAuto")}</p>
            </div>
          )}

          <div className="flex flex-wrap items-center gap-2">
            <Button
              size="sm"
              variant="outline"
              data-testid="zcode-open-official"
              disabled={savedUrl === null}
              onClick={() => void openOfficial()}
            >
              {t("settings.remote.zcode.openOfficial")}
            </Button>
            <span className={SETTINGS_NOTE}>{t("settings.remote.zcode.openOfficialHint")}</span>
          </div>
        </div>
      )}
    </div>
  );
}
