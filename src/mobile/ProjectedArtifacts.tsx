// 结构化附件卡片（任务 6）：把 IPC 投影里的图片 / 文档 / 外链渲染成可点卡片。
//
// 设计边界（spec §四.1、任务 6 裁决）：
// - 图片单列大卡：图片、回合内序号、来源摘要；文档（含 fileChange）为文件 chip；
//   `external_image` 只给 HTTP(S) 外链（沿用既有 target=_blank + rel=noreferrer），
//   不下载、不代理、不内联 base64。
// - 每回合最多 12 张图片卡：`imageIndexInTurn` / 回合内计数决定可见性，超出部分既不
//   渲染也不注册视口消费者（不会发请求）；省略数量用 `turnImageCount` 只在一条汇总行
//   说明一次。fileChange 每消息最多 6 个 chip，其余显示数量与文件面板入口。
// - 服务端预算省略（`omittedArtifactCount`）另有一条**消息级**提示，与 UI 的 12 图 /
//   6 chip 展示上限分开；文案只说不曾下发，不声称这些引用可定位或已在文件池。
// - 来源摘要优先同回合、附件之前的最近一条非空用户消息；其次附件摘要；再退回来源
//   标签。摘要展示上限 24 字。绝不借用上一回合的用户消息。
// - 来源标签区分上传 / 生成 / 修改 / 查看；fileChange 只展示后端给出的原始状态与
//   变更类型标量，不把未知/进行中的修改说成已完成。
// - 不可用引用（缺路径、fileId、data URL、错误 scheme、超长等）与图片读取/解码失败
//   都渲染可理解占位，不静默删卡、不触发代理或 base64 传输。
// - 图片读取统一交给 `useArtifactImages`（视口懒加载、同会话共享、两个在途上限）。
import { useEffect, useMemo, useRef } from "react";
import type { ReactNode } from "react";
import { FileText, ImageOff, Link2 } from "lucide-react";
import i18n from "@/i18n";
import type { ArtifactSource, ProjectedArtifact, ProjectedMessage } from "./codexIpcApi";
import type { ArtifactImagesController } from "./useArtifactImages";

/** 每回合最多展示的图片卡数（spec §三.3 钉值）。 */
export const MAX_TURN_IMAGE_CARDS = 12;
/** 每条消息最多展示的 `fileChange` 文件 chip 数（spec §三.3 钉值）。 */
export const MAX_FILE_CHANGE_CHIPS = 6;
/** 来源摘要展示上限（spec §四.1「文件池摘要最多显示 24 字」）。 */
export const ARTIFACT_SUMMARY_MAX_CHARS = 24;

/** 单条消息的附件渲染上下文（由 `buildArtifactMessageContexts` 按回合派生）。 */
export interface ArtifactMessageContext {
  /** 同回合、附件之前最近一条非空用户消息；null = 该回合没有可用提示词。 */
  turnPrompt: string | null;
  /** 在 12 图/回合预算内可渲染的图片 artifactId。 */
  visibleImageIds: ReadonlySet<string>;
  /** 本消息承担的「超过 12 图」汇总数量；0 = 不显示汇总（汇总每回合只出现一次）。 */
  hiddenImageCount: number;
  /** 回合内图片总数（含尾窗之外，取 turnImageCount 与可见索引的较大值）。 */
  turnImageCount: number;
}

const SOURCE_KEYS: Record<ArtifactSource, string> = {
  image_generation: "mobile.artifacts.source.image_generation",
  local_image: "mobile.artifacts.source.local_image",
  mention: "mobile.artifacts.source.mention",
  file_change: "mobile.artifacts.source.file_change",
  image_view: "mobile.artifacts.source.image_view",
  image_url: "mobile.artifacts.source.image_url",
};

const REASON_KEYS: Record<string, string> = {
  // DTO 的不可用原因码（任务 1 契约）
  missing_path: "mobile.artifacts.reason.missingPath",
  unresolved_file_id: "mobile.artifacts.reason.unresolvedFileId",
  data_url: "mobile.artifacts.reason.dataUrl",
  unsupported_url_scheme: "mobile.artifacts.reason.unsupportedScheme",
  path_too_long: "mobile.artifacts.reason.pathTooLong",
  file_key_too_long: "mobile.artifacts.reason.fileKeyTooLong",
  missing_url: "mobile.artifacts.reason.missingUrl",
  url_too_long: "mobile.artifacts.reason.urlTooLong",
  // 预览读取 / 图片解码失败（UI 侧原因）
  sensitive: "mobile.artifacts.reason.denied",
  not_found: "mobile.artifacts.reason.removed",
  too_large: "mobile.artifacts.reason.tooLarge",
  not_file: "mobile.artifacts.reason.notFile",
  io: "mobile.artifacts.reason.unreadable",
  network: "mobile.artifacts.reason.unreadable",
  unreadable: "mobile.artifacts.reason.unreadable",
  not_image: "mobile.artifacts.reason.notImage",
  decode_failed: "mobile.artifacts.reason.decodeFailed",
};

/** 来源标签：上传 / 生成 / 修改 / 查看（外链另有「外部图片」）。 */
export function artifactSourceLabel(source: ArtifactSource): string {
  return i18n.t(SOURCE_KEYS[source] ?? "mobile.artifacts.source.unknown");
}

/** 不可用/失败原因的展示文案；未知机器码给通用占位，不回显原始码。 */
export function artifactReasonText(reason: string | null | undefined): string {
  if (typeof reason === "string" && reason !== "") {
    return i18n.t(REASON_KEYS[reason] ?? "mobile.artifacts.reason.unknown");
  }
  return i18n.t("mobile.artifacts.reason.unknown");
}

/** 摘要截断到 24 字（按字符计数，避免把代理对拆半），超出补省略号。 */
export function capArtifactSummary(text: string | null | undefined): string | null {
  if (typeof text !== "string") return null;
  const trimmed = text.trim();
  if (trimmed === "") return null;
  const chars = Array.from(trimmed);
  if (chars.length <= ARTIFACT_SUMMARY_MAX_CHARS) return trimmed;
  return `${chars.slice(0, ARTIFACT_SUMMARY_MAX_CHARS).join("")}…`;
}

/** 展示用文件名：路径末段优先（win/unix 双分隔符），缺失时退回附件摘要。 */
export function artifactFileName(a: ProjectedArtifact): string | null {
  if (typeof a.path === "string" && a.path !== "") {
    const parts = a.path.split(/[\\/]/);
    const last = parts[parts.length - 1];
    if (last !== "") return last;
  }
  const summary = capArtifactSummary(a.summary);
  return summary === null ? null : summary;
}

/** 可打开路径：无路径或不可预览原因 → null。`file_key_too_long` 是例外——
 *  关联键超长但 path 仍在，卡片照常显示并按路径打开（任务 6 接口说明）。 */
export function artifactOpenPath(a: ProjectedArtifact): string | null {
  if (typeof a.path !== "string" || a.path === "") return null;
  if (a.unavailableReason !== null && a.unavailableReason !== "file_key_too_long") return null;
  return a.path;
}

/** 外链安全口径：只认 HTTP(S)，其余 scheme 一律降级占位。 */
export function isHttpUrl(url: string | null | undefined): boolean {
  if (typeof url !== "string" || url === "") return false;
  try {
    const parsed = new URL(url);
    return parsed.protocol === "http:" || parsed.protocol === "https:";
  } catch {
    return false;
  }
}

interface TurnState {
  prompt: string | null;
  declaredCount: number;
  maxIndex: number;
  candidateCount: number;
  /** 已纳入 12 图预算的卡片数（不信任索引完整性，硬上限仍是 12）。 */
  shownCount: number;
  firstHiddenMessageId: string | null;
  lastImageMessageId: string | null;
}

/**
 * 按 canonical 消息顺序派生每条消息的附件上下文：
 * - 同回合提示词取「附件之前最近一条非空用户消息」，用户消息自身的附件用自身文本；
 * - 图片预算按回合跨消息累计，第 13 张起不渲染（也不注册视口消费者）；
 * - 汇总承担者优先为「第一条被隐藏图片所在消息」，窗口内没有隐藏图片时退到该回合
 *   最后一张图片所在消息，保证每回合只出现一行汇总。
 */
export function buildArtifactMessageContexts(
  messages: readonly ProjectedMessage[]
): Map<string, ArtifactMessageContext> {
  const out = new Map<string, ArtifactMessageContext>();
  const turns = new Map<string, TurnState>();
  const perMessage = new Map<
    string,
    { visible: Set<string>; prompt: string | null; turnKey: string }
  >();

  for (const message of messages) {
    const hasTurn = typeof message.turnId === "string" && message.turnId !== "";
    const turnKey = hasTurn ? `turn:${message.turnId as string}` : `msg:${message.id}`;
    let turn = turns.get(turnKey);
    if (turn === undefined) {
      turn = {
        prompt: null,
        declaredCount: 0,
        maxIndex: 0,
        candidateCount: 0,
        shownCount: 0,
        firstHiddenMessageId: null,
        lastImageMessageId: null,
      };
      turns.set(turnKey, turn);
    }
    // 用户消息（含纯附件消息）先更新回合提示词：附件挂在自身消息上时用自身正文
    const ownText = message.role === "user" ? (message.text ?? "").trim() : "";
    if (ownText !== "") turn.prompt = ownText;
    const prompt = hasTurn ? turn.prompt : ownText === "" ? null : ownText;

    const visible = new Set<string>();
    for (const a of message.artifacts ?? []) {
      if (a.kind !== "image") continue;
      // 不可用引用只出占位，不占图片预算、也不触发读取
      if (artifactOpenPath(a) === null) continue;
      turn.candidateCount += 1;
      const index =
        typeof a.imageIndexInTurn === "number" && a.imageIndexInTurn > 0
          ? a.imageIndexInTurn
          : turn.candidateCount;
      if (index > turn.maxIndex) turn.maxIndex = index;
      if (index <= MAX_TURN_IMAGE_CARDS && turn.shownCount < MAX_TURN_IMAGE_CARDS) {
        turn.shownCount += 1;
        visible.add(a.artifactId);
      } else if (turn.firstHiddenMessageId === null) {
        turn.firstHiddenMessageId = message.id;
      }
      turn.lastImageMessageId = message.id;
    }
    const declared =
      typeof message.turnImageCount === "number" && message.turnImageCount > 0
        ? message.turnImageCount
        : 0;
    if (declared > turn.declaredCount) turn.declaredCount = declared;
    perMessage.set(message.id, { visible, prompt, turnKey });
  }

  for (const [messageId, info] of perMessage) {
    const turn = turns.get(info.turnKey) as TurnState;
    const total = Math.max(turn.declaredCount, turn.maxIndex, turn.candidateCount);
    out.set(messageId, {
      turnPrompt: info.prompt,
      visibleImageIds: info.visible,
      hiddenImageCount: 0,
      turnImageCount: total,
    });
  }

  for (const turn of turns.values()) {
    const total = Math.max(turn.declaredCount, turn.maxIndex, turn.candidateCount);
    const hidden = Math.max(0, total - MAX_TURN_IMAGE_CARDS);
    if (hidden === 0) continue;
    const hostId = turn.firstHiddenMessageId ?? turn.lastImageMessageId;
    if (hostId === null) continue;
    const ctx = out.get(hostId);
    if (ctx !== undefined) out.set(hostId, { ...ctx, hiddenImageCount: hidden });
  }
  return out;
}

/* ------------------------------------------------------------------ */
/* 子组件                                                              */
/* ------------------------------------------------------------------ */

function SourceLabel({ source }: { source: ArtifactSource }) {
  return (
    <span
      data-testid="artifact-source-label"
      data-source={source}
      className="shrink-0 rounded-full bg-[var(--cb)] px-1.5 py-0.5 text-[10px] font-medium text-[var(--mut)]"
    >
      {artifactSourceLabel(source)}
    </span>
  );
}

function ArtifactPlaceholder({
  artifact,
  reason,
}: {
  artifact: ProjectedArtifact;
  reason: string;
}) {
  const name = artifactFileName(artifact);
  return (
    <div
      data-testid={`artifact-placeholder-${artifact.artifactId}`}
      data-reason={reason}
      role="note"
      className="flex items-center gap-2 rounded-xl border border-dashed border-[var(--cb)] bg-[var(--cbg)] px-3 py-2 text-xs text-[var(--mut)]"
    >
      <ImageOff size={14} className="shrink-0" />
      <span className="min-w-0 flex-1">{artifactReasonText(reason)}</span>
      {name !== null && name !== "" && (
        <span className="max-w-[40%] shrink-0 truncate" title={artifact.path ?? name}>
          {name}
        </span>
      )}
    </div>
  );
}

function ArtifactImageCard({
  artifact,
  path,
  index,
  summary,
  images,
  openFile,
}: {
  artifact: ProjectedArtifact;
  path: string;
  index: number;
  summary: string;
  images: ArtifactImagesController;
  openFile: (path: string) => void;
}) {
  // 视口观察元素：始终存在（加载前/失败后也在），只有进入视口才注册消费者。
  // 依赖只有 images/path：同一挂载实例内，父级重渲染 / 摘要或布局类名变化不会
  // 重新 attach，也不会重复读盘。
  // **布局驱动的重挂载是例外**（Task 6 复审 I1，如实登记）：SessionDetail 在
  // split/split-h 与 fullscreen 之间切换 React 分支时，整个消息列会卸载重建——
  // 本组件 detach → 最后一个消费者离开 → hook 释放 object URL；重建后可见图片
  // 会重新 acquire/读盘，预览（FilePreview）也会因重挂载再读一次。这是继承的
  // 布局行为（本任务按「沿用现状」不重构布局）；消除它需要在布局层提升预览/
  // 图片资源，已登记为后续项。
  const shellRef = useRef<HTMLSpanElement | null>(null);
  useEffect(() => {
    const el = shellRef.current;
    if (el === null) return;
    return images.attach(path, el);
  }, [images, path]);

  const entry = images.stateFor(path);
  const ready = entry?.status === "ready" && entry.url !== null;
  const fileName = artifactFileName(artifact) ?? i18n.t("mobile.artifacts.imageAlt");
  const placeholder =
    entry?.status === "error"
      ? artifactReasonText(entry.reason)
      : entry?.status === "loading"
        ? i18n.t("mobile.artifacts.imageLoading")
        : i18n.t("mobile.artifacts.imagePending");

  return (
    <button
      type="button"
      data-testid={`artifact-card-${artifact.artifactId}`}
      data-path={path}
      data-artifact-id={artifact.artifactId}
      aria-label={i18n.t("mobile.artifacts.openImage", { name: fileName })}
      onClick={() => openFile(path)}
      className="block w-full overflow-hidden rounded-xl border border-[var(--cb)] bg-[var(--cbg)] text-left data-[jump-node=true]:ring-2 data-[jump-node=true]:ring-amber-500/70"
    >
      <span
        ref={shellRef}
        className="flex min-h-36 w-full items-center justify-center bg-[var(--cbg)] p-2"
      >
        {ready ? (
          <img
            data-testid={`artifact-image-${artifact.artifactId}`}
            src={entry.url as string}
            alt={fileName}
            className="max-h-80 w-full object-contain"
            onError={() => images.reportDecodeFailure(path)}
          />
        ) : (
          <span
            data-testid={
              entry?.status === "error" ? `artifact-placeholder-${artifact.artifactId}` : undefined
            }
            data-reason={entry?.status === "error" ? (entry.reason ?? undefined) : undefined}
            className="px-2 py-6 text-center text-xs text-[var(--mut)]"
          >
            {placeholder}
          </span>
        )}
      </span>
      <span className="flex flex-col gap-1 px-2 py-1.5">
        <span className="flex items-center gap-2">
          <span
            data-testid="artifact-image-index"
            className="shrink-0 rounded bg-[var(--cb)] px-1.5 py-0.5 text-[10px] text-[var(--mut)] tabular-nums"
          >
            {i18n.t("mobile.artifacts.imageIndex", { n: index })}
          </span>
          <SourceLabel source={artifact.source} />
          <span
            data-testid="artifact-file-name"
            className="min-w-0 flex-1 truncate font-mono text-[10px] text-[var(--mut)]"
            title={fileName}
          >
            {fileName}
          </span>
        </span>
        <span
          data-testid="artifact-summary"
          className="min-w-0 truncate text-xs text-[var(--tx)]"
          title={summary}
        >
          {summary}
        </span>
      </span>
    </button>
  );
}

function ArtifactChip({
  artifact,
  path,
  openFile,
}: {
  artifact: ProjectedArtifact;
  path: string;
  openFile: (path: string) => void;
}) {
  const name = artifactFileName(artifact) ?? path;
  return (
    <button
      type="button"
      data-testid={`artifact-chip-${artifact.artifactId}`}
      data-path={path}
      data-artifact-id={artifact.artifactId}
      data-source={artifact.source}
      data-source-status={artifact.sourceStatus ?? undefined}
      onClick={() => openFile(path)}
      title={artifact.path ?? name}
      className="flex w-full items-center gap-2 rounded-lg border border-[var(--cb)] bg-[var(--cbg)] px-2.5 py-1.5 text-left text-xs text-[var(--tx)] hover:bg-[var(--cb)]/60 data-[jump-node=true]:ring-2 data-[jump-node=true]:ring-amber-500/70 dark:hover:bg-[var(--btnp)]"
    >
      <FileText size={14} className="shrink-0 text-[var(--mut)]" />
      <span data-testid="artifact-file-name" className="min-w-0 flex-1 truncate font-mono">
        {name}
      </span>
      {artifact.changeKind !== null && artifact.changeKind !== "" && (
        <span data-testid="artifact-change-kind" className="shrink-0 text-[10px] text-[var(--mut)]">
          {artifact.changeKind}
        </span>
      )}
      <SourceLabel source={artifact.source} />
    </button>
  );
}

/** 消息级服务端预算省略提示：只报告不曾下发的附件数，与 UI 展示限额分开。 */
function ServerOmittedNote({ count }: { count: number }) {
  return (
    <p
      data-testid="artifact-server-omitted"
      role="note"
      className="rounded-lg bg-[var(--cbg)] px-3 py-2 text-xs text-[var(--mut)]"
    >
      {i18n.t("mobile.artifacts.serverOmitted", { count })}
    </p>
  );
}

/* ------------------------------------------------------------------ */
/* 主组件                                                              */
/* ------------------------------------------------------------------ */

export interface ProjectedArtifactsProps {
  message: ProjectedMessage;
  /** 调用方按回合预算派生的上下文；缺省按单条消息即时推导（测试/独立用法）。 */
  context?: ArtifactMessageContext;
  /** 既有预览入口（与正文链接同一个 openFile）。 */
  openFile: (path: string) => void;
  /** 会话级图片资源控制器（懒加载 / 共享 / 释放）。 */
  images: ArtifactImagesController;
  /** 文件池入口（chip 超限时的「打开文件面板」）。 */
  onOpenFilePanel?: () => void;
}

export default function ProjectedArtifacts({
  message,
  context,
  openFile,
  images,
  onOpenFilePanel,
}: ProjectedArtifactsProps) {
  const artifacts = message.artifacts ?? [];
  // 服务端预算省略数（只由后端给出，≠ UI 的 12 图 / 6 chip 展示上限）。
  const omittedArtifactCount =
    typeof message.omittedArtifactCount === "number" && message.omittedArtifactCount > 0
      ? message.omittedArtifactCount
      : 0;
  const ctx = useMemo(
    () => context ?? buildArtifactMessageContexts([message]).get(message.id),
    [context, message]
  );
  if (artifacts.length === 0 || ctx === undefined) {
    // 附件全部被服务端预算省略时也要如实提示；完全没有可渲染内容才不出现。
    if (omittedArtifactCount === 0) return null;
    return (
      <div className="space-y-2">
        <ServerOmittedNote count={omittedArtifactCount} />
      </div>
    );
  }

  const nodes: ReactNode[] = [];
  let shownImages = 0;
  let shownChangeChips = 0;
  let hiddenChangeChips = 0;
  let imageOverflowRendered = false;

  const pushImageOverflow = () => {
    if (imageOverflowRendered || ctx.hiddenImageCount <= 0) return;
    imageOverflowRendered = true;
    nodes.push(
      <p
        key="artifact-image-overflow"
        data-testid="artifact-image-overflow"
        className="rounded-lg bg-amber-500/10 px-3 py-2 text-xs text-amber-700 dark:text-amber-400"
      >
        {i18n.t("mobile.artifacts.imageOverflow", {
          total: ctx.turnImageCount,
          shown: MAX_TURN_IMAGE_CARDS,
        })}
      </p>
    );
  };

  for (const artifact of artifacts) {
    if (artifact.kind === "image") {
      const path = artifactOpenPath(artifact);
      if (path === null) {
        nodes.push(
          <ArtifactPlaceholder
            key={artifact.artifactId}
            artifact={artifact}
            reason={artifact.unavailableReason ?? "missing_path"}
          />
        );
        continue;
      }
      if (!ctx.visibleImageIds.has(artifact.artifactId)) {
        // 超出 12 图预算：不渲染卡片、不注册视口消费者；汇总在本消息（或末尾）出现一次
        pushImageOverflow();
        continue;
      }
      shownImages += 1;
      const summary =
        capArtifactSummary(ctx.turnPrompt) ??
        capArtifactSummary(artifact.summary) ??
        i18n.t("mobile.artifacts.sourceFallback", {
          label: artifactSourceLabel(artifact.source),
        });
      nodes.push(
        <ArtifactImageCard
          key={artifact.artifactId}
          artifact={artifact}
          path={path}
          index={artifact.imageIndexInTurn ?? shownImages}
          summary={summary}
          images={images}
          openFile={openFile}
        />
      );
      continue;
    }

    if (artifact.kind === "document") {
      const path = artifactOpenPath(artifact);
      if (path === null) {
        nodes.push(
          <ArtifactPlaceholder
            key={artifact.artifactId}
            artifact={artifact}
            reason={artifact.unavailableReason ?? "missing_path"}
          />
        );
        continue;
      }
      if (artifact.source === "file_change") {
        if (shownChangeChips >= MAX_FILE_CHANGE_CHIPS) {
          hiddenChangeChips += 1;
          continue;
        }
        shownChangeChips += 1;
      }
      nodes.push(
        <ArtifactChip
          key={artifact.artifactId}
          artifact={artifact}
          path={path}
          openFile={openFile}
        />
      );
      continue;
    }

    // external_image：只允许 HTTP(S) 外链，绝不下载或代理
    if (artifact.kind === "external_image") {
      if (artifact.unavailableReason === null && isHttpUrl(artifact.url)) {
        const url = artifact.url as string;
        nodes.push(
          <a
            key={artifact.artifactId}
            data-testid={`artifact-external-${artifact.artifactId}`}
            href={url}
            target="_blank"
            rel="noreferrer"
            className="flex w-full items-center gap-2 rounded-lg border border-[var(--cb)] bg-[var(--cbg)] px-2.5 py-1.5 text-left text-xs text-[var(--tx)] hover:bg-[var(--cb)]/60 dark:hover:bg-[var(--btnp)]"
          >
            <Link2 size={14} className="shrink-0 text-[var(--mut)]" />
            <span data-testid="artifact-source-label" className="shrink-0 text-[var(--mut)]">
              {i18n.t("mobile.artifacts.externalImage")}
            </span>
            <span className="min-w-0 flex-1 truncate font-mono">{url}</span>
          </a>
        );
      } else {
        const reason =
          artifact.unavailableReason ??
          (typeof artifact.url === "string" && artifact.url !== ""
            ? "unsupported_url_scheme"
            : "missing_url");
        nodes.push(
          <ArtifactPlaceholder key={artifact.artifactId} artifact={artifact} reason={reason} />
        );
      }
      continue;
    }

    // 防御：未知 kind（旧载荷/未来扩展）不静默吞掉
    nodes.push(
      <ArtifactPlaceholder
        key={artifact.artifactId}
        artifact={artifact}
        reason={artifact.unavailableReason ?? "unknown"}
      />
    );
  }

  // 汇总兜底：承担汇总的消息里没有第 13 张卡（例如尾窗只带来前几张）时仍出现一次
  pushImageOverflow();

  if (hiddenChangeChips > 0) {
    nodes.push(
      <div
        key="artifact-chip-overflow"
        data-testid="artifact-chip-overflow"
        className="flex flex-wrap items-center gap-2 rounded-lg bg-[var(--cbg)] px-2.5 py-1.5 text-xs text-[var(--mut)]"
      >
        <span className="min-w-0 flex-1">
          {i18n.t("mobile.artifacts.fileChangeOverflow", { count: hiddenChangeChips })}
        </span>
        {onOpenFilePanel !== undefined && (
          <button
            type="button"
            data-testid="artifact-file-panel"
            onClick={onOpenFilePanel}
            className="shrink-0 rounded-md bg-[var(--cb)] px-2 py-0.5 text-xs text-[var(--tx)]"
          >
            {i18n.t("mobile.artifacts.openFilePanel")}
          </button>
        )}
      </div>
    );
  }

  // 服务端预算省略：消息级提示只出现一次，且不并入上面的 UI 展示限额汇总。
  if (omittedArtifactCount > 0) {
    nodes.push(<ServerOmittedNote key="artifact-server-omitted" count={omittedArtifactCount} />);
  }

  return <div className="space-y-2">{nodes}</div>;
}
