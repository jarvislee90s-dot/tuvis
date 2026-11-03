// 任务 7：文件身份关联与“最近一次可识别引用”（纯函数，无 React / DOM 依赖）。
//
// 职责边界（spec §4.2 / §4.4）：
// - `findLastFileReference`：**单页**内从新到旧匹配引用。结构化附件按 `fileKey`
//   匹配；正文只认既有 Markdown 链接器（`linkifyMarkdownSegments` + 已知路径集）已能识别的文件段，
//   经已知 `fileEntries` 映射到 `fileKey`——不从正文抽新路径、不按 basename 猜身份、
//   不猜别名。同消息多处命中取最后一个节点（正文节点在前、附件节点在后，与消息
//   DOM 顺序一致），同文件跨消息取最近消息。
// - `buildArtifactFileView`：后端文件条目优先；只给缺失的结构化引用补条目。已有
//   条目的 `origin` / `modified` **原样保留**（弱来源不覆盖强来源、写过不撤销已修改；
//   附件不提升写来源、不撤销 `modified`）。缺失条目来源映射（spec §4.4）：
//   用户附件 → user、查看 → tool_read、文件修改与生成图 → session_artifact；
//   `modified` **恒为 false**。合成行因此刻意偏离后端不变量
//   `modified == (origin == ToolWrite)`：来源是「来源类别」，不是「写盘成功」声明；
//   本轮没有任何 `fileChange` 已完成的证据（状态枚举未实测），所以
//   `(session_artifact, false)` 的修改类含义是「修改类引用、无完成证据」，原始
//   `sourceStatus` / `changeKind` 仍逐字展示，不凭来源名称虚构成功写盘。既有后端
//   条目一律不提升、不撤销。外链 `external_image` 永不进入本地文件池。
// - 排序：本次传入范围内有引用证据的条目按最近引用倒序；其余既有条目随后保持后端
//   相对顺序。补充条目参与同一排序。**不把 IPC 消息顺序写成 `lastSeq`**（补充条目
//   的 `lastSeq` 取 0 这一中性占位，不参与排序，也不使用负数）。
import type { ArtifactSource, ProjectedArtifact, ProjectedMessage } from "./codexIpcApi";
import type { SessionFileEntry } from "./api";
import { linkifyMarkdownSegments, linkifySegments, sortedPaths } from "./message-render";
import {
  artifactOpenPath,
  buildArtifactMessageContexts,
  capArtifactSummary,
  type ArtifactMessageContext,
} from "./ProjectedArtifacts";

/** 定位目标：文件身份键优先；旧载荷/未知来源可只有路径（此时按路径匹配）。 */
export interface ArtifactJumpTarget {
  /** 关联键（`SessionFileEntry.fileKey` / `ProjectedArtifact.fileKey` 同口径）。 */
  fileKey: string | null;
  /** 展示/匹配用路径（未知来源也能发起查找；不预造 messageId）。 */
  path: string;
}

/** 单页命中：定位到哪条消息的哪个节点。 */
export interface FileReferenceHit {
  messageId: string;
  /** artifact = 结构化附件节点；body = 既有链接器产出的正文文件链接节点。 */
  via: "artifact" | "body";
  /** 附件节点身份（via=artifact 时非 null，用于 DOM 定位）。 */
  artifactId: string | null;
  /** 正文链接路径（via=body 时非 null，用于 DOM 定位）。 */
  path: string | null;
  /** 消息内引用节点序号（0 起；正文节点在前、附件节点在后）。 */
  nodeIndex: number;
}

/** 范围内的一条引用（`buildArtifactFileView` 的排序/来源依据）。 */
export interface ArtifactFileReference {
  messageId: string;
  /** canonical 消息位置（越大越新；仅本次传入范围，不跨页比较）。 */
  messageIndex: number;
  /** 消息内节点序号（越大越靠后）。 */
  nodeIndex: number;
  via: "artifact" | "body";
  artifactId: string | null;
  path: string | null;
  fileKey: string | null;
}

/** 文件面板消费的合并视图条目：既有后端字段 + 引用证据。 */
export interface ArtifactFileEntry extends SessionFileEntry {
  /** 最近一次**结构化引用**的来源（无结构化引用 → null）；只作行为标签。 */
  artifactSource: ArtifactSource | null;
  /** 最近一次结构化引用的原始状态标量（不映射为成功/失败，原样展示）。 */
  sourceStatus: string | null;
  /** 最近一次结构化引用的原始变更类型标量（原样展示，不猜 add/modify/delete）。 */
  changeKind: string | null;
  /** 来源摘要（同回合附件前最近非空用户消息优先，其次附件摘要；≤24 字）：
   *  无结构化引用证据的后端行保持 null。 */
  summary: string | null;
  /** 本次范围内的最近引用（无引用证据 → null）。 */
  lastReference: ArtifactFileReference | null;
}

/** 来源优先级（与后端 `FileOrigin::rank` 同序）：session_artifact < user < tool_read < tool_write。 */
const ORIGIN_RANK: Record<NonNullable<SessionFileEntry["origin"]>, number> = {
  session_artifact: 0,
  user: 1,
  tool_read: 2,
  tool_write: 3,
};

/** 附件来源 → 文件池来源池（spec §4.4；外链 image_url 已被排除，不参与）。
 *
 *  `file_change` 只映射到中性来源：来源类别说明「这是修改类引用」，**不是**「写盘
 *  已完成」。批准计划任务 7 要求「只有证据证明修改已完成才提升写来源和 `modified`」，
 *  而 fileChange 的状态枚举尚未实测（见
 *  `docs/engineering/evidence/codex-desktop-ipc/2026-10-20-codex-artifact-field-evidence.md`），
 *  没有可用的完成证据 → 一律不进入「工具读写」池。真实来源与原始状态仍由
 *  `artifactSource` / `sourceStatus` 如实展示，信息不丢。 */
const SOURCE_ORIGIN: Record<ArtifactSource, NonNullable<SessionFileEntry["origin"]>> = {
  local_image: "user",
  mention: "user",
  image_view: "tool_read",
  file_change: "session_artifact",
  image_generation: "session_artifact",
  image_url: "session_artifact",
};

/**
 * fileChange 的「已完成」状态白名单。**当前刻意留空**：该枚举只有字段名有静态
 * 证据、取值未实测，猜一个值就等于凭来源名称虚构成功写盘（R3 的原错）。
 * 取证后只在这里加值即可恢复写池提升，不需要新增产品枚举。
 */
const CHANGE_COMPLETED_STATUSES: ReadonlySet<string> = new Set<string>();

/** 修改类附件是否有「已完成」证据（决定是否提升写来源与 `modified`）。 */
function changeCompleted(artifact: ProjectedArtifact): boolean {
  if (artifact.source !== "file_change") return false;
  if (artifact.sourceStatus === null) return false;
  return CHANGE_COMPLETED_STATUSES.has(artifact.sourceStatus);
}

/** 附件来源 → 本次来源池。写来源只在有完成证据时提升。 */
function originOf(artifact: ProjectedArtifact): NonNullable<SessionFileEntry["origin"]> {
  if (artifact.source === "file_change" && changeCompleted(artifact)) return "tool_write";
  return SOURCE_ORIGIN[artifact.source];
}

/** 结构化附件是否能作为本地文件池/引用节点：外链永不进入；其余需有键或路径。 */
function isLocalArtifact(artifact: ProjectedArtifact): boolean {
  if (artifact.kind === "external_image") return false;
  return artifact.fileKey !== null || (artifact.path !== null && artifact.path !== "");
}

/** 附件引用节点（含池条目可行性需要的可打开性判定）。 */
interface ArtifactNode {
  artifact: ProjectedArtifact;
  /** 可按当前规则打开（`artifactOpenPath`）——只有可打开引用能给缺失文件补条目。 */
  openable: boolean;
}

/** 收集页内引用：canonical 旧→新；同消息内正文节点在前、附件节点在后。 */
function collectReferences(
  messages: readonly ProjectedMessage[],
  entries: readonly SessionFileEntry[]
): ArtifactFileReference[] {
  const knownPaths = sortedPaths(new Set(entries.map((e) => e.path)));
  const out: ArtifactFileReference[] = [];
  messages.forEach((message, messageIndex) => {
    let nodeIndex = 0;
    const text = message.text ?? "";
    if (text !== "") {
      const bodySegments =
        message.role === "user" || message.role === "assistant"
          ? linkifyMarkdownSegments(text, knownPaths)
          : linkifySegments(text, knownPaths);
      for (const segment of bodySegments) {
        if (segment.type !== "file") continue;
        // 正文只认既有链接器可识别的已知路径（经条目映射身份）；不抽取新路径
        const matched = entries.find((e) => e.path === segment.value);
        if (matched === undefined) continue;
        out.push({
          messageId: message.id,
          messageIndex,
          nodeIndex,
          via: "body",
          artifactId: null,
          path: segment.value,
          fileKey: matched.fileKey ?? null,
        });
        nodeIndex += 1;
      }
    }
    for (const artifact of message.artifacts ?? []) {
      if (!isLocalArtifact(artifact)) continue;
      out.push({
        messageId: message.id,
        messageIndex,
        nodeIndex,
        via: "artifact",
        artifactId: artifact.artifactId,
        path: artifact.path,
        fileKey: artifact.fileKey,
      });
      nodeIndex += 1;
    }
  });
  return out;
}

/** 引用是否命中目标身份：双方都有 fileKey 时以键为准，否则退化为 path 全等。 */
function referenceMatches(reference: ArtifactFileReference, target: ArtifactJumpTarget): boolean {
  if (target.fileKey !== null && reference.fileKey !== null) {
    return target.fileKey === reference.fileKey;
  }
  return target.path !== "" && reference.path !== null && reference.path === target.path;
}

/**
 * 在**一页**消息里找目标文件的最近一次引用（从新到旧）。
 *
 * 返回 null 表示这一页没有可识别引用；调用方据此决定是否继续读取更早页——不是
 * “该文件从未被引用”的结论（见 `useArtifactJump` 的有界状态机）。
 */
export function findLastFileReference(
  page: readonly ProjectedMessage[],
  target: ArtifactJumpTarget,
  entries: readonly SessionFileEntry[]
): FileReferenceHit | null {
  const references = collectReferences(page, entries);
  for (let i = references.length - 1; i >= 0; i -= 1) {
    const reference = references[i];
    if (!referenceMatches(reference, target)) continue;
    return {
      messageId: reference.messageId,
      via: reference.via,
      artifactId: reference.artifactId,
      path: reference.path,
      nodeIndex: reference.nodeIndex,
    };
  }
  return null;
}

/** 按引用找到既有条目下标：键优先、缺失键退化为路径；键不同不猜同文件。 */
function findEntryIndex(
  entries: readonly SessionFileEntry[],
  reference: ArtifactFileReference
): number | null {
  for (let i = 0; i < entries.length; i += 1) {
    const entry = entries[i];
    if (reference.fileKey !== null && entry.fileKey !== undefined && entry.fileKey !== "") {
      if (entry.fileKey === reference.fileKey) return i;
      continue;
    }
    if (reference.path !== null && entry.path === reference.path) return i;
  }
  return null;
}

interface ReferenceGroup {
  entryIndex: number | null;
  /** 该组出现过的显式 fileKey（同 path 但两侧键不同 = 不同身份，不猜测合并）。 */
  keys: Set<string>;
  /** 范围内最近一条引用（任意节点类型；用于排序与“来源定位指向最近引用”）。 */
  newest: ArtifactFileReference;
  /** 最近一条结构化引用（用于来源/状态标签）。 */
  newestArtifact: ArtifactFileReference | null;
  /** 最近一条可打开的附件引用（用于缺失条目建行）。 */
  newestOpenable: { node: ArtifactNode; reference: ArtifactFileReference } | null;
  /** 引用中可打开附件的最强来源池（写来源不被弱来源撤销）。 */
  strongestOrigin: NonNullable<SessionFileEntry["origin"]>;
  /** 本组是否出现过「修改已完成」的证据（决定 `modified` 与写池提升）。 */
  writeCompleted: boolean;
  /** 该组已知的第一个非空 fileKey（旧载荷无键的后续引用不能抹掉已知身份键）。 */
  knownKey: string | null;
  count: number;
}

/** 收集页内附件节点（artifactId → 可打开性/来源）；正文与池建行共用。 */
function artifactNodes(messages: readonly ProjectedMessage[]): Map<string, ArtifactNode> {
  const out = new Map<string, ArtifactNode>();
  for (const message of messages) {
    for (const artifact of message.artifacts ?? []) {
      out.set(artifact.artifactId, {
        artifact,
        openable: artifactOpenPath(artifact) !== null,
      });
    }
  }
  return out;
}

/**
 * 合并后端文件条目与结构化引用，得到文件面板视图（顺序已按本任务规则排好）。
 *
 * @param entries 后端 `/session-files` 条目（已按 `lastSeq` 降序）。
 * @param messages 本次参考范围（canonical 旧→新，最多五页：尾窗 + 至多四页缓存）。
 */
export function buildArtifactFileView(
  entries: readonly SessionFileEntry[],
  messages: readonly ProjectedMessage[]
): ArtifactFileEntry[] {
  const references = collectReferences(messages, entries);
  const nodes = artifactNodes(messages);
  // 摘要规则与卡片同源：同回合、附件之前最近一条非空用户消息；不借上一回合。
  const contexts = buildArtifactMessageContexts(messages);
  const groups = new Map<string, ReferenceGroup>();
  /** 已出现身份索引：同一 fileKey / 同一 path 的后续引用并入同一组（不猜别名）。 */
  const byKey = new Map<string, string>();
  const byPath = new Map<string, string>();

  for (const reference of references) {
    const entryIndex = findEntryIndex(entries, reference);
    let identity: string | null = null;
    if (entryIndex !== null) {
      identity = `entry:${entryIndex}`;
    } else if (reference.fileKey !== null && byKey.has(reference.fileKey)) {
      identity = byKey.get(reference.fileKey) as string;
    } else if (reference.path !== null && byPath.has(reference.path)) {
      const candidate = byPath.get(reference.path) as string;
      const candidateGroup = groups.get(candidate);
      // 两侧都有显式 fileKey 且不同：同一展示路径也不猜测为同一身份
      const conflictsWithKey =
        reference.fileKey !== null &&
        candidateGroup !== undefined &&
        candidateGroup.keys.size > 0 &&
        !candidateGroup.keys.has(reference.fileKey);
      if (!conflictsWithKey) identity = candidate;
    }
    if (identity === null) {
      identity = reference.fileKey !== null ? `key:${reference.fileKey}` : `path:${reference.path}`;
    }
    if (reference.fileKey !== null && !byKey.has(reference.fileKey)) {
      byKey.set(reference.fileKey, identity);
    }
    if (reference.path !== null && !byPath.has(reference.path)) {
      byPath.set(reference.path, identity);
    }

    const node = reference.artifactId !== null ? nodes.get(reference.artifactId) : undefined;
    let group = groups.get(identity);
    if (group === undefined) {
      group = {
        entryIndex,
        keys: new Set<string>(),
        newest: reference,
        newestArtifact: null,
        newestOpenable: null,
        strongestOrigin: "session_artifact",
        writeCompleted: false,
        knownKey: reference.fileKey,
        count: 0,
      };
      groups.set(identity, group);
    }
    if (reference.fileKey !== null) {
      group.keys.add(reference.fileKey);
      if (group.knownKey === null) group.knownKey = reference.fileKey;
    }
    group.count += 1;
    group.newest = reference; // references 已按时间顺序：后写即更新
    if (reference.via === "artifact" && node !== undefined) {
      group.newestArtifact = reference;
      const origin = originOf(node.artifact);
      if (ORIGIN_RANK[origin] > ORIGIN_RANK[group.strongestOrigin]) {
        group.strongestOrigin = origin;
      }
      if (changeCompleted(node.artifact)) group.writeCompleted = true;
      if (node.openable) group.newestOpenable = { node, reference };
    }
  }

  const view: ArtifactFileEntry[] = entries.map((entry) => ({
    ...entry,
    artifactSource: null,
    sourceStatus: null,
    changeKind: null,
    summary: null,
    lastReference: null,
  }));

  for (const group of groups.values()) {
    const source = group.newestArtifact;
    if (group.entryIndex !== null) {
      // 既有条目：来源池/已修改标志一律沿用后端，附件只补行为标签与最近引用
      const target = view[group.entryIndex];
      target.artifactSource = artifactSourceOf(nodes, source);
      target.sourceStatus = artifactField(nodes, source, "sourceStatus");
      target.changeKind = artifactField(nodes, source, "changeKind");
      target.summary = artifactSummaryOf(nodes, source, contexts);
      target.lastReference = group.newest;
      continue;
    }
    const seed = group.newestOpenable;
    if (seed === null || seed.reference.path === null) continue; // 无路径无法成为本地池条目
    view.push({
      path: seed.reference.path,
      // 旧载荷的新引用可能没有键：优先用本组已知键，别把已知身份降级成 path-only
      fileKey: group.knownKey ?? seed.reference.fileKey ?? undefined,
      // 没有 rollout seq 证据：中性占位 0（不把 IPC 顺序写成 lastSeq、不用负数）
      lastSeq: 0,
      lastTs: null,
      hits: group.count,
      // `origin` 是 spec §4.4 的来源类别映射，不是成功声明；写来源与 `modified`
      // 只在**有完成证据**时提升（批准计划任务 7）。fileChange 的状态枚举未实测，
      // 因此修改类引用当前一律落在中性来源、`modified=false`——既满足
      // `modified == (origin == ToolWrite)`，又不凭来源名称虚构成功写盘。
      // 状态/变更类型仍按原始标量展示。仅**新建**行如此；既有条目一律不提升、
      // 不撤销（见上方分支）。
      modified: group.writeCompleted,
      origin: group.strongestOrigin,
      artifactSource: artifactSourceOf(nodes, source),
      sourceStatus: artifactField(nodes, source, "sourceStatus"),
      changeKind: artifactField(nodes, source, "changeKind"),
      summary: artifactSummaryOf(nodes, source, contexts),
      lastReference: group.newest,
    });
  }

  // 排序：有引用证据的按最近引用倒序（同日按节点序号）；其余保持后端相对顺序
  return view
    .map((entry, index) => ({ entry, index }))
    .sort((a, b) => {
      const left = a.entry.lastReference;
      const right = b.entry.lastReference;
      if (left !== null && right !== null) {
        if (left.messageIndex !== right.messageIndex) {
          return right.messageIndex - left.messageIndex;
        }
        if (left.nodeIndex !== right.nodeIndex) return right.nodeIndex - left.nodeIndex;
        return a.index - b.index;
      }
      if (left !== null) return -1;
      if (right !== null) return 1;
      return a.index - b.index;
    })
    .map((item) => item.entry);
}

/** 附件节点的来源（正文引用没有来源 → null）。 */
function artifactSourceOf(
  nodes: Map<string, ArtifactNode>,
  reference: ArtifactFileReference | null
): ArtifactSource | null {
  if (reference === null || reference.artifactId === null) return null;
  return nodes.get(reference.artifactId)?.artifact.source ?? null;
}

/** 附件节点的原始标量字段（不翻译、不映射成功/失败语义）。 */
function artifactField(
  nodes: Map<string, ArtifactNode>,
  reference: ArtifactFileReference | null,
  field: "sourceStatus" | "changeKind"
): string | null {
  if (reference === null || reference.artifactId === null) return null;
  return nodes.get(reference.artifactId)?.artifact[field] ?? null;
}

/** 来源摘要：与卡片同规则（同回合、附件前最近非空用户消息 → 附件摘要 → null），
 *  统一 24 字封顶。无结构化附件引用（纯正文/后端行）→ null，不猜来源。 */
function artifactSummaryOf(
  nodes: Map<string, ArtifactNode>,
  reference: ArtifactFileReference | null,
  contexts: Map<string, ArtifactMessageContext>
): string | null {
  if (reference === null || reference.artifactId === null) return null;
  const artifact = nodes.get(reference.artifactId)?.artifact;
  if (artifact === undefined) return null;
  return (
    capArtifactSummary(contexts.get(reference.messageId)?.turnPrompt) ??
    capArtifactSummary(artifact.summary)
  );
}
