// DraftEnvelope（草稿信封）：Codex 异步题草稿的**唯一 SSOT**（模块级仓库）。
//
// 设计（2026-11-14 题卡跳过计划前端部分）：
// - 信封按 scope（sessionId + sourceTurnId + sourceItemId）存于模块级 Map——
//   组件挂载/卸载/父层重排都不丢稿；**只有 AsyncQuestionGroupCard（子）写**，
//   父层（SessionDetail）不持草稿状态、只经同一仓库读（「父只完整镜像、子唯一写」）；
// - record 身份 = 本地 id（newActionId），**绝不是题目身份**；题目身份由
//   origin + question 元数据承载：
//   - origin {kind:"verified", group4}：服务端投影确证的组归属——同 group4 + 同
//     题体（qkey/index/题干/有序 options）时 **token 轮换自动回填**（observedToken
//     仅观察，绝不提交）；
//   - origin {kind:"unverified"}：旧载荷/无投影事实——保持旧行为（qkey+token 变化
//     即隔离旧稿），不猜组归属；
// - question 元数据（真实 qkey/index/题干/有序 options）随记录保存：内容/代际真变
//   时旧稿**隔离不丢**，跨多次 unmount 仍可查看/恢复（恢复目标 = 当前同 source 的
//   真实 qkey/index 唯一成员，且不覆盖非空）；
// - 成员变化一律走**一次纯转换**（convertDraftEnvelope）：seed 新成员、回填 token、
//   旧记录原样保留（隔离态由消费方派生），绝无「先清后隔离」；
// - 写纪律：所有 commit 都在事件处理器 / layout effect 中同步进行，**禁止 render
//   期写仓库**（防 Suspense 污染）；commit 带 scope 守卫——旧会话回调写不进新槽。
import { newActionId } from "./codexIpcApi";
import { sameGroupIdentity4, type GroupIdentity4 } from "./codexQuestionSkipApi";

/** 信封作用域：会话 + 来源 turn/item（与停靠区分组键同源）。 */
export interface DraftScope {
  sessionId: string;
  sourceTurnId: string;
  sourceItemId: string;
}

/** 记录来源。verified 只来自服务端投影确证（questionDisplayGroup），其余一律 unverified。 */
export type DraftOrigin = { kind: "verified"; group: GroupIdentity4 } | { kind: "unverified" };

/** 题目元数据快照（恢复展示与恢复定位的唯一依据；内容真变后旧值保留在隔离记录里）。 */
export interface DraftQuestionMeta {
  questionKey: string;
  questionIndex: number;
  title: string;
  orderedOptions: string[];
}

/** 单题草稿记录。id 是本地身份（UUID），与题目身份完全解耦。 */
export interface DraftRecord {
  id: string;
  origin: DraftOrigin;
  /** 真实题元数据（恢复展示与恢复定位的唯一依据）；读取方仍按 null 防御——
   *  元数据不可知的记录无处恢复，不猜目标。 */
  question: DraftQuestionMeta | null;
  /** 观察到的 contextToken；**仅观察用，绝不可作为提交 token**（提交只取当前成员的
   *  实时 token）。 */
  observedToken: string | null;
  value: { selected: string | null; text: string };
}

/** 信封本体。version 只有一版（1）；后续格式演进必须写迁移，不原地改语义。 */
export interface DraftEnvelope {
  version: 1;
  scope: DraftScope;
  records: DraftRecord[];
}

// ==== descriptor memo（verified 组归属的 lastKnown 记忆）====
//
// 描述符**短暂未知 ≠ 已证换代**：questionDisplayGroup 缺席一帧（旧后端帧/事实迟到）
// 不得把 verified 记录降级成 unverified 身份（那会 seed 空 unverified 抢占原
// verified 非空稿）。本 memo 按 scope 记住最近一次**服务端确证**的组；缺席时沿用
// lastKnown 参与匹配（仅用于匹配与回填，绝不伪造成「当前已证」的新事实）。
// 写入只在 layout effect（remember）发生；render 只读（peek）。
const verifiedGroupMemo = new Map<string, GroupIdentity4>();

/** 纯读：显式确证在场优先；否则沿用 lastKnown；从未确证 → null（真 unverified）。 */
export function peekVerifiedDraftGroup(
  scope: DraftScope,
  explicit: GroupIdentity4 | null | undefined
): GroupIdentity4 | null {
  if (explicit != null) return explicit;
  return verifiedGroupMemo.get(draftScopeKey(scope)) ?? null;
}

/** layout effect 写入：仅显式服务端确证在场时刷新 lastKnown。 */
export function rememberVerifiedDraftGroup(scope: DraftScope, group: GroupIdentity4): void {
  verifiedGroupMemo.set(draftScopeKey(scope), group);
}

// ==== 模块级 SSOT 仓库 ====

const store = new Map<string, DraftEnvelope>();

export function draftScopeKey(scope: DraftScope): string {
  return `${scope.sessionId}\u0000${scope.sourceTurnId}\u0000${scope.sourceItemId}`;
}

/** 纯 seed 读取：无记录 → 空信封（不写回仓库——空信封不落库，避免无谓写放大）。 */
export function loadDraftEnvelope(scope: DraftScope): DraftEnvelope {
  const existing = store.get(draftScopeKey(scope));
  if (existing !== undefined) return existing;
  return { version: 1, scope, records: [] };
}

/** 唯一写入口（同步）：scope 守卫——信封 scope 与目标 scope 不一致（旧会话回调）
 *  一律拒写（返回 false），旧 callback 写不进新槽。 */
export function commitDraftEnvelope(scope: DraftScope, next: DraftEnvelope): boolean {
  if (draftScopeKey(next.scope) !== draftScopeKey(scope)) return false;
  if (next.version !== 1) return false;
  store.set(draftScopeKey(scope), next);
  return true;
}

/** 测试/诊断用：读取原始信封（不 seed）。 */
export function peekDraftEnvelope(scope: DraftScope): DraftEnvelope | undefined {
  return store.get(draftScopeKey(scope));
}

// ==== 成员引用与匹配 ====

/** 当前成员的最小引用（与 CodexAsyncQuestionView 字段子集对齐；避免反向依赖组件类型）。 */
export interface DraftMemberRef {
  questionKey: string;
  contextToken: string;
  questionIndex: number;
  title: string;
  options: string[];
}

/** 题体相等：qkey + 题号 + 题干 + 有序 options 全等（verified 组的「同体」判据；
 *  token 不参与——token 轮换正是要回填的场景）。 */
function questionBodyEquals(meta: DraftQuestionMeta, member: DraftMemberRef): boolean {
  return (
    meta.questionKey === member.questionKey &&
    meta.questionIndex === member.questionIndex &&
    meta.title === member.title &&
    meta.orderedOptions.length === member.options.length &&
    meta.orderedOptions.every((option, index) => option === member.options[index])
  );
}

/** 记录 ↔ 成员匹配（**唯一**匹配判据）：
 *  - verified：origin.group 与当前确证组全等 且 题体相等（token 无关 → 轮换回填）；
 *  - unverified：qkey + observedToken 相等且题体相等（旧行为：token 变化即不匹配）；
 *  - 升级匹配：确证组在场而记录尚为 unverified 时，题体 + token 全等即承接并升级
 *    origin（服务端事实迟到一拍不闪隔离；升级只发生在题体完全一致的记录上）。 */
function recordMatchesMember(
  record: DraftRecord,
  member: DraftMemberRef,
  verifiedGroup: GroupIdentity4 | null
): boolean {
  if (record.question === null || !questionBodyEquals(record.question, member)) return false;
  if (verifiedGroup !== null) {
    if (record.origin.kind === "verified") {
      return sameGroupIdentity4(record.origin.group, verifiedGroup);
    }
    return record.observedToken === member.contextToken;
  }
  return record.origin.kind === "unverified" && record.observedToken === member.contextToken;
}

function metaOf(member: DraftMemberRef): DraftQuestionMeta {
  return {
    questionKey: member.questionKey,
    questionIndex: member.questionIndex,
    title: member.title,
    orderedOptions: [...member.options],
  };
}

// ==== 一次纯转换（成员变化的唯一入口）====

export interface ConvertDraftEnvelopeResult {
  envelope: DraftEnvelope;
  /** 结构是否变化（调用方据此决定是否 commit/setState，避免自激重渲染）。 */
  changed: boolean;
  /** 成员引用（排序后）到记录 id 的映射（消费方据此派生活动稿）。 */
  memberRecordIds: string[];
}

/**
 * 成员变化的**一次纯转换**（不先清后隔离）：
 * - 每个成员找唯一匹配记录（按 members 顺序、records 插入序首个未占用匹配）：
 *   匹配 → 沿用记录（回填 observedToken 到成员当前 token）；无匹配 → 纯 seed 新记录；
 * - 未匹配到的旧记录**原样保留**（隔离态由消费方按「不匹配任何当前成员」派生），
 *   绝不在转换里删除；
 * - matchGroup 可为 lastKnown descriptor memo（旧稿匹配用）；seedGroup 只能是
 *   **当前帧确证**的组——seed/升级 origin 必须有 current 内容证据，lastKnown 不得
 *   认领新 body。
 */
export function convertDraftEnvelope(
  prev: DraftEnvelope,
  scope: DraftScope,
  members: DraftMemberRef[],
  matchGroup: GroupIdentity4 | null,
  seedGroup: GroupIdentity4 | null
): ConvertDraftEnvelopeResult {
  const used = new Set<string>();
  const memberRecordIds: string[] = [];
  const keptRecords: DraftRecord[] = [];
  let changed = false;

  for (const member of members) {
    let matched: DraftRecord | null = null;
    for (const record of prev.records) {
      if (used.has(record.id)) continue;
      if (recordMatchesMember(record, member, matchGroup)) {
        matched = record;
        break;
      }
    }
    if (matched !== null) {
      used.add(matched.id);
      let adjusted = matched;
      // 事实在场升级：unverified 记录只在**当前帧确证组**在场且题体一致时承接为
      // verified（lastKnown 不给升级背书——新 body 必须有 current 证据）。
      if (seedGroup !== null && adjusted.origin.kind === "unverified") {
        adjusted = { ...adjusted, origin: { kind: "verified", group: { ...seedGroup } } };
        changed = true;
      }
      const backfilled =
        adjusted.observedToken === member.contextToken
          ? adjusted
          : { ...adjusted, observedToken: member.contextToken };
      if (backfilled !== adjusted) changed = true;
      keptRecords.push(backfilled);
      memberRecordIds.push(matched.id);
      continue;
    }
    // 纯 seed：新成员新记录（空值），origin 按确证与否分流。
    const seeded: DraftRecord = {
      id: newActionId(),
      origin:
        seedGroup !== null ? { kind: "verified", group: { ...seedGroup } } : { kind: "unverified" },
      question: metaOf(member),
      observedToken: member.contextToken,
      value: { selected: null, text: "" },
    };
    changed = true;
    keptRecords.push(seeded);
    memberRecordIds.push(seeded.id);
  }

  // 未匹配旧记录原样保留（隔离态派生；不删不改——「不先清后隔离」）。
  for (const record of prev.records) {
    if (!used.has(record.id)) keptRecords.push(record);
  }
  if (keptRecords.length !== prev.records.length) changed = true;

  const envelope: DraftEnvelope = { version: 1, scope: { ...scope }, records: keptRecords };
  return { envelope, changed, memberRecordIds };
}

// ==== 记录级操作（纯函数，返回新信封；commit 由调用方完成）====

/** 更新某记录的草稿值（选项/自由文本互斥语义由调用方保证，此处只落值）。 */
export function withDraftValue(
  envelope: DraftEnvelope,
  recordId: string,
  patch: Partial<{ selected: string | null; text: string }>
): DraftEnvelope {
  return {
    ...envelope,
    records: envelope.records.map((record) =>
      record.id === recordId
        ? {
            ...record,
            value: {
              selected: patch.selected !== undefined ? patch.selected : record.value.selected,
              text: patch.text !== undefined ? patch.text : record.value.text,
            },
          }
        : record
    ),
  };
}

/** 丢弃一条隔离记录（用户显式丢弃；转换路径绝不调用）。 */
export function withoutDraftRecord(envelope: DraftEnvelope, recordId: string): DraftEnvelope {
  return { ...envelope, records: envelope.records.filter((record) => record.id !== recordId) };
}

/** 恢复：把隔离记录的值转移到目标活动记录（值的落法——旧选项失效转可见自由文本、
 *  不覆盖非空——由调用方计算），并移除隔离记录。 */
export function transferDraftValue(
  envelope: DraftEnvelope,
  fromRecordId: string,
  toRecordId: string,
  value: { selected: string | null; text: string }
): DraftEnvelope {
  return {
    ...envelope,
    records: envelope.records.flatMap((record) => {
      if (record.id === fromRecordId) return [];
      if (record.id === toRecordId) return [{ ...record, value }];
      return [record];
    }),
  };
}

// ==== 隔离态派生（消费方视角）====

/** 信封中的隔离记录：有非空值、且不在给定活动 id 集合内的记录（供旧稿 UI 展示）。 */
export function isolatedDraftRecords(
  envelope: DraftEnvelope,
  activeRecordIds: readonly string[]
): DraftRecord[] {
  const active = new Set(activeRecordIds);
  return envelope.records.filter(
    (record) =>
      !active.has(record.id) &&
      (record.value.text.trim().length > 0 || record.value.selected !== null)
  );
}
