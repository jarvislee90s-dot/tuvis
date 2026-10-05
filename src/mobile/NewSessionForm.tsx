import { useCallback, useEffect, useState } from "react";
import { ApiError, fetchZcodeCreateInfo, sessionCreateZcode } from "./api";
import type { ZcodeCreateInfo, ZcodeCreateResult } from "./api";
import { headlessDurationText, headlessStageText } from "./SessionDetail";

// H10（Task 12）：zcode 无头新建表单（**独立交付**）。
//
// **范围（本轮）**：工具选择器的 **zcode 分组**——本批只有 zcode 的无头新建已接线，
// 其余工具如实置灰并说明去向（H11 三家 CLI 归 Task 13、终端/CLI 的新建入口归独立轨道
// session-create Phase C）。**不做**与既有新建入口的 UI 融合（计划明确留待 Phase C），
// 故本组件自包含、可单独挂载。
//
// **文案纪律（与后端诚实红线同源）**：
// - 会话号 / 可见性提示 / 黄字信号 / 失败分诊**全部来自后端**：`visibilityNote` 是
//   `Visibility::note()` 的逐字文案，阶段分诊复用 `SessionDetail.headlessStageText`
//   单点（跨语言锁 `tests/fixtures/headless_stages.json`），前端只渲染不另编；
// - **未确认**（`confirmation === "none"`，`sessionId` 为空串）必须原样说成「未确认新会话」
//   ——前端**绝不**为它补一个会话号，也不承诺任何可见性；
// - 黄字信号是**提示不拦截**（同项目已有在册 zcode 会话）：渲染它，但不禁用提交。

/** 未接线工具的如实说明（去向写清，不假装能发）——文案内联（移动页无 i18n 运行时） */
const PENDING_TOOLS: Array<{ id: string; label: string; note: string }> = [
  { id: "claude", label: "Claude Code", note: "无头新建未接线（H11 归 Task 13）" },
  { id: "kimi", label: "Kimi Code", note: "无头新建未接线（H11 归 Task 13）" },
  { id: "opencode", label: "OpenCode", note: "无头新建未接线（H11 归 Task 13）" },
];

/** 确认来源 → 用户可读文案（后端 wire 词一一对应，勿漂移） */
export function confirmationText(c: ZcodeCreateResult["confirmation"]): string {
  switch (c) {
    case "stdout_frame":
      return "CLI 回执帧已点名会话号";
    case "store":
      return "会话库已确认新会话";
    default:
      return "未确认新会话";
  }
}

export default function NewSessionForm({ onBack }: { onBack?: () => void }) {
  const [info, setInfo] = useState<ZcodeCreateInfo | null>(null);
  /** info 拉取失败态：`null` = 设备失效（403，与 fetchSendInfo 同语义） */
  const [loadFailed, setLoadFailed] = useState<"unpaired" | string | null>(null);
  const [project, setProject] = useState("");
  const [firstText, setFirstText] = useState("hi");
  /** 选中项目的黄字信号（前端预判面；提交回执里的那次才是当时的权威结论） */
  const [probeWarning, setProbeWarning] = useState<string | null>(null);
  const [sending, setSending] = useState(false);
  const [result, setResult] = useState<ZcodeCreateResult | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    void (async () => {
      try {
        const v = await fetchZcodeCreateInfo();
        if (!alive) return;
        if (v === null) {
          setLoadFailed("unpaired");
          return;
        }
        setInfo(v);
        if (v.defaultFirstText) setFirstText(v.defaultFirstText);
        if (v.warning) setProbeWarning(v.warning);
      } catch (e) {
        if (alive) setLoadFailed(e instanceof Error ? e.message : String(e));
      }
    })();
    return () => {
      alive = false;
    };
  }, []);

  /** 选中/填写项目后补拉一次该项目的黄字信号（表单预判面；失败静默——提交回执照旧） */
  const probeProject = useCallback((p: string) => {
    if (!p.trim()) return;
    void (async () => {
      try {
        const v = await fetchZcodeCreateInfo(p.trim());
        setProbeWarning(v?.warning ?? null);
      } catch {
        /* 预判面失败不阻断表单 */
      }
    })();
  }, []);

  const available = info?.available === true;
  const canSubmit = available && project.trim().length > 0 && !sending;

  async function submit() {
    if (!canSubmit) return;
    setSending(true);
    setError(null);
    setResult(null);
    try {
      setResult(await sessionCreateZcode(project.trim(), firstText.trim() || undefined));
    } catch (e) {
      // 非 2xx：403 的 reason 是后端逐字文案（headless_disabled 等），优先展示
      const reason =
        e instanceof ApiError && typeof e.data?.reason === "string"
          ? (e.data.reason as string)
          : null;
      setError(reason ?? (e instanceof Error ? e.message : String(e)));
    } finally {
      setSending(false);
    }
  }

  if (loadFailed) {
    return (
      <div
        data-testid="create-load-failed"
        className="rounded-lg bg-amber-500/10 px-3 py-2 text-xs text-amber-700 dark:text-amber-400"
      >
        {loadFailed === "unpaired"
          ? "设备失效或未配对——请在电脑端 MAM 重新配对后再试"
          : `新建表单加载失败：${loadFailed}`}
      </div>
    );
  }

  return (
    <div data-testid="new-session-form" className="space-y-3 text-slate-800 dark:text-slate-200">
      <header className="flex items-baseline justify-between">
        <h2 className="text-base font-semibold">新建会话</h2>
        {onBack && (
          <button
            type="button"
            data-testid="new-session-back"
            onClick={onBack}
            className="rounded-lg border border-slate-200 px-2 py-1 text-xs dark:border-slate-800"
          >
            返回
          </button>
        )}
      </header>

      {!available && info && (
        <p
          data-testid="create-disabled"
          className="rounded-lg bg-amber-500/10 px-3 py-2 text-xs text-amber-700 dark:text-amber-400"
        >
          {info.reason ?? "无头新建当前不可用"}
        </p>
      )}

      {/* 工具选择器：本批只有 zcode 接线 */}
      <section>
        <h3 className="mb-1 text-xs text-slate-500 dark:text-slate-400">工具</h3>
        <div className="flex flex-wrap gap-1.5">
          <button
            type="button"
            data-testid="tool-option-zcode"
            aria-disabled={!available}
            aria-pressed={true}
            className="rounded-lg border border-sky-500/60 bg-sky-500/10 px-2 py-1 text-xs text-sky-700 dark:text-sky-300"
          >
            ZCode
          </button>
          {PENDING_TOOLS.map((t) => (
            <button
              key={t.id}
              type="button"
              data-testid={`tool-option-${t.id}`}
              aria-disabled={true}
              disabled
              title={t.note}
              className="rounded-lg border border-slate-200 px-2 py-1 text-xs text-slate-400 disabled:opacity-70 dark:border-slate-800 dark:text-slate-500"
            >
              {t.label}·{t.note}
            </button>
          ))}
        </div>
      </section>

      {/* 候选列表：信任档如实标注（未信任目录的新会话 APP 永不收录，仅 MAM 可见） */}
      {available && (
        <section>
          <h3 className="mb-1 text-xs text-slate-500 dark:text-slate-400">
            项目（APP 已信任工作区 ∪ 看板）
          </h3>
          <ul className="space-y-1">
            {(info?.candidates ?? []).map((c) => (
              <li key={c.path}>
                <button
                  type="button"
                  data-testid={`candidate-${c.path}`}
                  aria-pressed={project === c.path}
                  onClick={() => {
                    setProject(c.path);
                    probeProject(c.path);
                  }}
                  className={`w-full rounded-lg border px-2 py-1 text-left text-xs ${
                    project === c.path
                      ? "border-sky-500/60 bg-sky-500/10"
                      : "border-slate-200 dark:border-slate-800"
                  }`}
                >
                  <span className="block truncate">{c.path}</span>
                  <span
                    data-testid={`candidate-note-${c.path}`}
                    className="block text-[11px] text-slate-500 dark:text-slate-400"
                  >
                    {/* 信任档文案**逐字来自后端**（Visibility::note() 单点，与回执的
                        visibilityNote 同源）——前端不再自带一份措辞（复审 Minor 2） */}
                    {c.note}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        </section>
      )}

      {/* 手填完整路径（后端校验：绝对性 / 盘符 / 敏感黑名单 / 绝不递归建目录） */}
      {available && (
        <section>
          <label
            className="mb-1 block text-xs text-slate-500 dark:text-slate-400"
            htmlFor="manual-path"
          >
            或手填完整路径
          </label>
          <input
            id="manual-path"
            data-testid="manual-path"
            value={project}
            onChange={(e) => setProject(e.target.value)}
            onBlur={(e) => probeProject(e.target.value)}
            placeholder="D:\\projects\\demo"
            className="w-full rounded-lg border border-slate-200 bg-transparent px-2 py-1 text-xs dark:border-slate-800"
          />
        </section>
      )}

      {available && (
        <section>
          <label
            className="mb-1 block text-xs text-slate-500 dark:text-slate-400"
            htmlFor="first-text"
          >
            首句（缺省探针 hi）
          </label>
          <input
            id="first-text"
            data-testid="first-text"
            value={firstText}
            onChange={(e) => setFirstText(e.target.value)}
            className="w-full rounded-lg border border-slate-200 bg-transparent px-2 py-1 text-xs dark:border-slate-800"
          />
        </section>
      )}

      {probeWarning && (
        <p
          data-testid="create-warning"
          className="rounded-lg bg-amber-500/10 px-3 py-2 text-xs text-amber-700 dark:text-amber-400"
        >
          {probeWarning}
        </p>
      )}

      <button
        type="button"
        data-testid="create-submit"
        disabled={!canSubmit}
        onClick={() => void submit()}
        className="w-full rounded-lg bg-sky-600 px-3 py-2 text-sm text-white disabled:opacity-40"
      >
        {sending ? "创建中…（无头回合需数秒）" : "创建并发送首句"}
      </button>

      {error && (
        <p
          data-testid="create-error"
          className="rounded-lg bg-rose-500/10 px-3 py-2 text-xs text-rose-700 dark:text-rose-400"
        >
          {error}
        </p>
      )}

      {result && <CreateReceiptCard result={result} />}
    </div>
  );
}

/** 新建回执卡：**确认/未确认两态由后端说话**——未确认时不显示任何会话号，
 *  也不承诺可见性（`sessionId` 空串即「没有确认到会话」，如实照说）。 */
function CreateReceiptCard({ result }: { result: ZcodeCreateResult }) {
  const confirmed = result.confirmation !== "none" && result.sessionId !== "";
  const r = result.receipt;
  return (
    <div
      data-testid="create-receipt"
      data-confirmation={result.confirmation}
      className="space-y-1 rounded-lg border border-slate-200 px-3 py-2 text-xs dark:border-slate-800"
    >
      <p
        className={
          confirmed
            ? "font-semibold text-emerald-700 dark:text-emerald-400"
            : "font-semibold text-rose-700 dark:text-rose-400"
        }
      >
        {confirmed ? `已建会话 ${result.sessionId}` : "未确认新会话"}
      </p>
      <p className="text-slate-500 dark:text-slate-400">{confirmationText(result.confirmation)}</p>
      {r.lastAssistant && <p className="truncate">末条回复：{r.lastAssistant}</p>}
      {typeof r.tokens === "number" && <p>tokens：{r.tokens}</p>}
      <p className="text-slate-500 dark:text-slate-400">
        {r.status === "failed" ? headlessStageText(r.stage) : null} 耗时{" "}
        {headlessDurationText(r.durationMs)}
      </p>
      {r.reason && <p className="whitespace-pre-wrap">{r.reason}</p>}
      {result.visibilityNote && <p>{result.visibilityNote}</p>}
      {result.warning && (
        <p className="rounded bg-amber-500/10 px-2 py-1 text-amber-700 dark:text-amber-400">
          {result.warning}
        </p>
      )}
    </div>
  );
}
