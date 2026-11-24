import { useEffect, useRef, useState } from "react";
import { useAppTranslation } from "@/hooks/use-app-translation";
import { newActionId } from "./codexIpcApi";
import type { DshModelSummary } from "./api";

interface Selection {
  provider: string;
  model: string;
  reasoningEffort?: string;
}

interface Option {
  id: string;
  name: string;
  reasoning?: { efforts: { id: string; name: string }[] };
}

interface Settings {
  permission: { currentValue: string; options: { value: string; name: string }[] } | null;
  model: {
    next: Selection | null;
    lastUsed: Selection | null;
    default: Selection | null;
    groups: { id: string; name: string; models: Option[] }[];
  };
}

type Change =
  | { type: "permission"; preset: string; acknowledged: boolean }
  | ({ type: "model"; forCurrentModel?: Selection } & Selection);

type Field = "permission" | "model" | "effort";

interface Props {
  instanceId: string;
  sessionId: string;
  refreshTick: number;
  running: boolean;
  history: DshModelSummary | null;
}

const endpoint = "/m/api/v1/dsh/session-settings";
const buttonClass =
  "rounded-full border border-[var(--cb)] px-2.5 py-1 text-xs text-[var(--tx)] disabled:opacity-40";
const selectionKey = (value: Selection) => JSON.stringify([value.provider, value.model]);

export default function DshSessionSettings({
  instanceId,
  sessionId,
  refreshTick,
  running,
  history,
}: Props) {
  const { t } = useAppTranslation();
  const [view, setView] = useState<Settings | null>(null);
  const [valid, setValid] = useState(false);
  const [editor, setEditor] = useState<Field | null>(null);
  const [selected, setSelected] = useState("");
  const [acknowledged, setAcknowledged] = useState(false);
  const [feedback, setFeedback] = useState<"confirmed" | "unconfirmed" | "rejected" | null>(null);
  const [refresh, setRefresh] = useState(0);
  const [saving, setSaving] = useState(false);
  const inFlight = useRef(false);
  const actionIds = useRef(new Map<string, string>());
  const pending = useRef<Change | null>(null);
  const [effortBase, setEffortBase] = useState<Selection | null>(null);

  const catalogs = editor !== null;
  useEffect(() => {
    const controller = new AbortController();
    const q = new URLSearchParams({ instanceId, sessionId, catalogs: String(catalogs) });
    void fetch(`${endpoint}?${q}`, { signal: controller.signal })
      .then(async (response) => {
        if (!response.ok) throw new Error("DSH settings unavailable");
        const next: Settings = await response.json();
        if (!next.model) throw new Error("DSH settings unavailable");
        if (!controller.signal.aborted) {
          setView(next);
          setValid(true);
          const change = pending.current;
          if (
            change &&
            (change.type === "permission"
              ? next.permission?.currentValue === change.preset
              : next.model.next?.provider === change.provider &&
                next.model.next.model === change.model &&
                next.model.next.reasoningEffort === change.reasoningEffort)
          ) {
            pending.current = null;
            actionIds.current.delete(JSON.stringify(change));
            setFeedback("confirmed");
          }
        }
      })
      .catch(() => {
        if (!controller.signal.aborted) setValid(false);
      });
    return () => controller.abort();
  }, [catalogs, instanceId, refresh, refreshTick, sessionId]);

  const permission = view?.permission;
  const model = view?.model;
  const current = model?.next;
  const defaultModel = model?.default;
  const lastUsed = model?.lastUsed ?? history;
  const display = current ?? defaultModel ?? lastUsed;
  const models =
    model?.groups.flatMap((group) =>
      group.models.map((option) => ({
        key: selectionKey({ provider: group.id, model: option.id }),
        provider: group.id,
        group: group.name,
        option,
      }))
    ) ?? [];
  const effortModel = current ?? defaultModel;
  const effortSelectionValid =
    editor !== "effort" ||
    (effortBase !== null &&
      effortModel != null &&
      JSON.stringify(effortBase) === JSON.stringify(effortModel));
  const chosenModel =
    effortModel && models.find((entry) => entry.key === selectionKey(effortModel));
  const efforts = chosenModel && chosenModel.option.reasoning?.efforts;
  const permissionLabel = (value: string) => {
    switch (value) {
      case "read-only":
        return t("mobile.dshSettings.readOnly");
      case "workspace-write":
        return t("mobile.dshSettings.workspaceWrite");
      case "danger-full-access":
        return t("mobile.dshSettings.fullAccess");
      case "auto":
        return t("mobile.dshSettings.autoReview");
      case "custom":
        return t("mobile.dshSettings.custom");
      default:
        return permission?.options.find((option) => option.value === value)?.name ?? value;
    }
  };
  const modelName =
    display &&
    (models.find(
      (entry) =>
        entry.option.id === display.model &&
        (!("provider" in display) || entry.provider === display.provider)
    )?.option.name ??
      display.model);
  const effortName = display?.reasoningEffort;
  const modelCaption = current
    ? t("mobile.dshSettings.model", { model: modelName })
    : defaultModel
      ? t("mobile.dshSettings.defaultModel", { model: modelName })
      : lastUsed
        ? t("mobile.dshSettings.lastModel", { model: modelName })
        : t("mobile.dshSettings.unknownModel");
  const effortCaption =
    current || defaultModel
      ? t("mobile.dshSettings.effort", {
          effort: effortName ?? t("mobile.dshSettings.providerDefault"),
        })
      : effortName
        ? t("mobile.dshSettings.lastEffort", { effort: effortName })
        : t("mobile.dshSettings.unknownEffort");

  function edit(field: Field) {
    setEditor((open) => (open === field ? null : field));
    setSelected("");
    setEffortBase(null);
    setAcknowledged(false);
    setFeedback(null);
  }

  async function save() {
    if (!valid || running || inFlight.current || !selected || !effortSelectionValid) return;
    let change: Change;
    if (editor === "permission") {
      if (!acknowledged || !permission?.options.some((option) => option.value === selected)) return;
      change = { type: "permission", preset: selected, acknowledged };
    } else if (editor === "model") {
      const option = models.find((model) => model.key === selected);
      if (!option) return;
      change = { type: "model", provider: option.provider, model: option.option.id };
    } else {
      if (!effortModel || !efforts || !efforts.some((effort) => effort.id === selected)) return;
      change = {
        type: "model",
        ...effortModel,
        reasoningEffort: selected,
        forCurrentModel: effortModel,
      };
    }
    const actionKey = JSON.stringify(change);
    const actionId = actionIds.current.get(actionKey) ?? newActionId();
    actionIds.current.set(actionKey, actionId);
    pending.current = change;
    inFlight.current = true;
    setSaving(true);
    setFeedback(null);
    try {
      const response = await fetch(endpoint, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ instanceId, sessionId, actionId, change }),
      });
      if (!response.ok) {
        if (response.status < 500) pending.current = null;
        setFeedback(response.status >= 500 ? "unconfirmed" : "rejected");
        return;
      }
      const result = await response.json();
      const rejected = ["failed", "cancelled", "rejected"].includes(result.state);
      setFeedback(result.confirmed ? "confirmed" : rejected ? "rejected" : "unconfirmed");
      if (result.confirmed || rejected) {
        pending.current = null;
        actionIds.current.delete(actionKey);
      }
      if (result.confirmed) setEditor(null);
    } catch {
      setFeedback("unconfirmed");
    } finally {
      inFlight.current = false;
      setSaving(false);
      setRefresh((tick) => tick + 1);
    }
  }

  const disabled = running || saving;
  const mode = editor === "permission";
  const dangerous = mode && selected && !["read-only", "workspace-write"].includes(selected);
  const options =
    editor === "model"
      ? models.map(({ key, option, group }) => ({ value: key, name: `${option.name} · ${group}` }))
      : mode
        ? (permission?.options
            .filter(({ value }) => value !== permission.currentValue)
            .map(({ value }) => ({ value, name: permissionLabel(value) })) ?? [])
        : efforts
          ? efforts.map(({ id, name }) => ({ value: id, name }))
          : [];
  return (
    <div className="contents" data-testid="dsh-session-settings">
      <button
        type="button"
        className={buttonClass}
        disabled={disabled}
        aria-expanded={mode}
        data-testid="dsh-permission"
        onClick={() => edit("permission")}
      >
        {permission
          ? t("mobile.dshSettings.permission", { mode: permissionLabel(permission.currentValue) })
          : t("mobile.dshSettings.unknownPermission")}
      </button>
      <button
        type="button"
        className={buttonClass}
        disabled={disabled}
        aria-expanded={editor === "model"}
        data-testid="dsh-model"
        onClick={() => edit("model")}
      >
        {modelCaption}
      </button>
      <button
        type="button"
        className={buttonClass}
        disabled={disabled}
        aria-expanded={editor === "effort"}
        data-testid="dsh-effort"
        onClick={() => edit("effort")}
      >
        {effortCaption}
      </button>
      {editor && (
        <div className="flex basis-full flex-wrap items-center gap-2 rounded-lg border border-[var(--cb)] p-2 text-xs">
          <select
            value={selected}
            aria-label={t(`mobile.dshSettings.${editor}Label`)}
            data-testid="dsh-setting-choice"
            disabled={disabled || !valid || !options.length}
            onChange={(event) => {
              setSelected(event.target.value);
              if (editor === "effort") setEffortBase(effortModel ?? null);
              setAcknowledged(false);
            }}
            className="max-w-full min-w-0 rounded border border-[var(--cb)] bg-[var(--cd)] px-2 py-1"
          >
            <option value="">{t("mobile.dshSettings.choose")}</option>
            {options.map((option) => (
              <option key={option.value} value={option.value}>
                {option.name}
              </option>
            ))}
          </select>
          <button
            type="button"
            className={buttonClass}
            disabled={
              disabled || !valid || !selected || !effortSelectionValid || (mode && !acknowledged)
            }
            data-testid="dsh-setting-apply"
            onClick={() => void save()}
          >
            {t("mobile.dshSettings.apply")}
          </button>
          <button
            type="button"
            className={buttonClass}
            disabled={saving}
            onClick={() => setEditor(null)}
          >
            {t("mobile.dshSettings.cancel")}
          </button>
          {!options.length && (
            <p className="basis-full text-[var(--mut)]">{t("mobile.dshSettings.unavailable")}</p>
          )}
          {selected && !mode && (
            <p className="basis-full text-[var(--mut)]">{t("mobile.dshSettings.defaultNotice")}</p>
          )}
          {selected && mode && (
            <label className="flex basis-full items-start gap-1.5">
              <input
                type="checkbox"
                checked={acknowledged}
                disabled={disabled}
                onChange={(event) => setAcknowledged(event.target.checked)}
              />
              <span>
                {t(
                  dangerous
                    ? "mobile.dshSettings.permissionRisk"
                    : "mobile.dshSettings.permissionConfirm"
                )}
              </span>
            </label>
          )}
        </div>
      )}
      {feedback && (
        <p role="status" className="basis-full text-xs text-[var(--mut)]">
          {t(`mobile.dshSettings.${feedback}`)}
        </p>
      )}
    </div>
  );
}
