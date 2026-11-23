// 整组问答的「草稿 → 原生 answer 对象」纯编码层（计划任务 10）。
//
// 与后端 `services/opencode_native/forms.rs::validate_answers` **同口径**，但用途不同：
// 后端那一层是权威复核（必须先于派发），本层是为了「不发请求也能拦住」——用户填到一半、
// 选项已变、必答缺失时，卡片当场说清，不必先打一次 400。两层的判据都来自任务 0 回填的
// 合同修订（`shared-contract.md` §「任务 0 回填的合同修订」第 1—2 条）：
//
// 1. **必答集 = `required === true`**（`null`/`undefined`/`false` 都不是必答）；
// 2. **合法跳过 = 省略键**。`Skip` / 空串 / 空列表一律「不写入 answer 对象」；
//    `""` 绝不作为值发出（对 required 字段它是 400 `Missing required`）；
// 3. **提交原生 `value`，不提交展示 `label`**：字符串等于某选项的 `label` 而与该选项
//    `value` 不同 ⇒ 拒绝（这是「把展示文案当值」的唯一可探测形态）；
// 4. 含 `Unsupported` 字段（`kind` 缺失/未知、未证 `when`、`hidden`、非 bool `required` 都被
//    后端折成 `unsupported`）的表单**整组拒答**：不挑可答字段先写一次。
//
// 编码表（`DraftAnswer` × 字段类型 → 线上值）：
//
// | 草稿 \ 字段 | `text`                                   | `multi`                          |
// | --- | --- | --- |
// | `Skip` | 省略键 | 省略键 |
// | `Text("")` | 省略键（必答 ⇒ `missing_required`） | `kind_mismatch` |
// | `Text(v)` | `v`（需过选项/自定义校验） | `kind_mismatch` |
// | `Multi([])` | `kind_mismatch` | 省略键（必答 ⇒ `missing_required`） |
// | `Multi([v…])` | `kind_mismatch` | `[v…]`（逐项过校验，顺序原样） |
// | 键不在当前 `form.fields` 里 | 省略（孤儿 = 合法跳过，不报错、不锁组） | 同左 |
//
// 本文件是纯函数：不 fetch、不 React、不看时钟、不读写任何存储（答案不得落到 storage）。
import type { DraftAnswer, FormField, PendingView } from "./opencodeNativeApi";

/** `PendingView` 的 Form 分支（TS 侧就是 `PendingForm`；字段名沿用 serde 的 snake_case）。 */
export type PendingForm = Extract<PendingView, { kind: "form" }>;

/** 线上 `answers` 对象：**跳过 = 缺键**，因此这里永远不出现 `{kind:"skip"}` 或空串。 */
export type Answers = Record<string, DraftAnswer>;

/** 本层能明确说清的拒绝理由（与后端 `OC_ANSWER_INCOMPLETE` / `OC_ANSWER_INVALID` /
 *  `OC_ANSWER_UNSUPPORTED` 三类对齐，但粒度更细以支持「当场指出缺哪一题」）。
 *
 *  **没有 `unknown_key`**：草稿里多出当前表单没有的键（表单字段集缩小 / 换了一组题）属于
 *  「旧字段」，编码时**直接省略**（见 {@link buildDraftSubmission}）——它既不是用户能做错的
 *  事，也不该把整组锁死。 */
export type DraftRefusalCode =
  | "unsupported_form"
  | "kind_mismatch"
  | "not_in_options"
  | "label_instead_of_value"
  | "missing_required";

/** 校验拒绝。`buildDraftSubmission` 选择**抛异常**而不是返回联合体：
 *  - 调用方（卡片）本来就要在渲染期算一次「能不能提交」，`try/catch` 比逐处解包判别式短；
 *  - 正常路径（能提交）返回的就是线上 `Answers` 本身，不需要调用方再解一层；
 *  - 漏接异常会在测试里立刻炸，不会被当成「空答案」悄悄放过去。
 *
 *  `fieldKey` 指出具体字段（`unsupported_form` 为 null：整组问题，不归单个字段）。 */
export class DraftRefusal extends Error {
  readonly code: DraftRefusalCode;
  readonly fieldKey: string | null;

  constructor(code: DraftRefusalCode, fieldKey: string | null = null) {
    super(fieldKey === null ? code : `${code}:${fieldKey}`);
    this.name = "DraftRefusal";
    this.code = code;
    this.fieldKey = fieldKey;
  }
}

/** 已覆盖的字段类型（判据与后端 `FieldKind` 的两个可答变体一致）。 */
type AnswerableKind =
  | { kind: "text"; options: { value: string; label: string }[]; custom: boolean }
  | { kind: "multi"; options: { value: string; label: string }[]; custom: boolean };

/** 字段类型判定（防御性）：形状不对的一律按 unsupported，不猜。 */
function answerableKind(field: FormField): AnswerableKind | null {
  const kind = field.kind as unknown as
    { kind?: unknown; options?: unknown; custom?: unknown } | null | undefined;
  if (kind === null || kind === undefined) return null;
  if (kind.kind !== "text" && kind.kind !== "multi") return null;
  if (!Array.isArray(kind.options) || typeof kind.custom !== "boolean") return null;
  const options: { value: string; label: string }[] = [];
  for (const option of kind.options as unknown[]) {
    if (option === null || typeof option !== "object") return null;
    const { value, label } = option as { value?: unknown; label?: unknown };
    if (typeof value !== "string" || typeof label !== "string") return null;
    options.push({ value, label });
  }
  return { kind: kind.kind, options, custom: kind.custom };
}

/** `required` 三态里只有 `true` 是必答；非 bool 的取值属于「规则未覆盖」。 */
function requiredIsKnown(field: FormField): boolean {
  return (
    field.required === undefined || field.required === null || typeof field.required === "boolean"
  );
}

/** 该字段为什么不可答（null = 可答）。后端已把未覆盖规则折成 `unsupported`，
 *  这里仍按形状复核一遍：读到畸形字段时宁可整组只读，也不发半份答案。 */
export function unsupportedFieldReason(field: FormField): "kind" | "required" | null {
  if (answerableKind(field) === null) return "kind";
  if (!requiredIsKnown(field)) return "required";
  return null;
}

/** 整组不可答的字段 key（空数组 = 可答）。 */
export function unsupportedFieldKeys(form: PendingForm): string[] {
  return form.fields.filter((field) => unsupportedFieldReason(field) !== null).map((f) => f.key);
}

/** 整组是否可答（与后端 `FormSpec::is_answerable` 同义）。 */
export function isFormAnswerable(form: PendingForm): boolean {
  return (
    unsupportedFieldKeys(form).length === 0 &&
    form.fields.every((field, index) => {
      if (field.when === undefined) return true;
      const condition = field.when;
      if (
        condition === null ||
        typeof condition.key !== "string" ||
        typeof condition.value !== "string" ||
        Object.keys(condition).length !== 2 ||
        field.kind.kind !== "text"
      )
        return false;
      const source = form.fields.slice(0, index).find((item) => item.key === condition.key);
      return (
        source !== undefined &&
        source.when === undefined &&
        source.required === true &&
        source.kind.kind === "text"
      );
    })
  );
}

/** 仅求值后端公开的已证 eq；不执行任意表达式。 */
export function fieldIsActive(field: FormField, drafts: Record<string, DraftAnswer>): boolean {
  if (field.when === undefined) return true;
  if (
    field.when === null ||
    typeof field.when.key !== "string" ||
    typeof field.when.value !== "string"
  )
    return false;
  const source = drafts[field.when.key];
  return source?.kind === "text" && source.value === field.when.value;
}

/** 必答判定：只有 `required === true`。 */
export function fieldIsRequired(field: FormField): boolean {
  return field.required === true;
}

/** 草稿是否「等于没答」（跳过 / 空串 / 空列表）；这类值一律编码成省略键。 */
export function draftIsEmpty(draft: DraftAnswer | undefined): boolean {
  if (draft === undefined) return true;
  switch (draft.kind) {
    case "skip":
      return true;
    case "text":
      return draft.value === "";
    case "multi":
      return draft.value.length === 0;
  }
}

/** 单个值的合法性：完全复刻后端 `forms::check_value` 的**判定顺序**。
 *  1. 命中某个选项的原生 `value` ⇒ 接受；
 *  2. 只命中某个选项的展示 `label`（且该 `value` 不同）⇒ 拒绝「label 顶替 value」；
 *  3. `custom:true` 或选项列表为空 ⇒ 按自由文本接受（服务端不拒，见后端模块「残余边界」）；
 *  4. 其余（旧选项集里的陈旧值）⇒ 拒绝。 */
function checkValue(fieldKey: string, kind: AnswerableKind, value: string): void {
  if (value === "") throw new DraftRefusal("not_in_options", fieldKey);
  if (kind.options.some((option) => option.value === value)) return;
  if (kind.options.some((option) => option.label === value)) {
    throw new DraftRefusal("label_instead_of_value", fieldKey);
  }
  if (kind.custom || kind.options.length === 0) return;
  throw new DraftRefusal("not_in_options", fieldKey);
}

/** 单字段编码：返回 null = 不写入 answer 对象。 */
function encodeField(field: FormField, draft: DraftAnswer): DraftAnswer | null {
  const kind = answerableKind(field);
  if (kind === null) throw new DraftRefusal("unsupported_form", null);
  if (draft.kind === "skip") return null;

  if (kind.kind === "text") {
    if (draft.kind !== "text") throw new DraftRefusal("kind_mismatch", field.key);
    if (draft.value === "") return null; // 清空 = 跳过；空串绝不编码
    checkValue(field.key, kind, draft.value);
    return { kind: "text", value: draft.value };
  }

  if (draft.kind !== "multi") throw new DraftRefusal("kind_mismatch", field.key);
  if (draft.value.length === 0) return null; // 空列表 = 跳过
  for (const value of draft.value) checkValue(field.key, kind, value);
  return { kind: "multi", value: draft.value.slice() };
}

/** 整组编码：产出的对象**就是** `answer` 的线上值（跳过项缺键，不是 `{kind:"skip"}`）。
 *
 *  **孤儿键 = 合法跳过**：草稿里有、当前 `form.fields` 里没有的键（表单字段集缩小、或换了
 *  另一组题而草稿槽还在）**不写入答案对象**。理由：后端只校验它收到的键，省略一个它已经
 *  删掉的字段与「跳过」在线上完全等价；把它当错误会把整组锁死，用户唯一的出口变成连有效
 *  答案一起丢掉。孤儿也不产生任何请求，只在卡片上以一行提示说出「旧字段已移除」。
 *
 *  拒绝时抛 {@link DraftRefusal}。后端仍会独立复核同一份答案——本层不是权威，
 *  它只是让「必答缺项 / 旧选项 / label 当值 / 未支持表单」在点提交前就说清楚。 */
export function buildDraftSubmission(
  form: PendingForm,
  drafts: Record<string, DraftAnswer>
): Answers {
  if (!isFormAnswerable(form)) throw new DraftRefusal("unsupported_form", null);

  const byKey = new Map(form.fields.map((field) => [field.key, field]));
  const answers: Answers = {};
  for (const [key, draft] of Object.entries(drafts)) {
    const field = byKey.get(key);
    if (field === undefined || !fieldIsActive(field, drafts)) continue; // 孤儿：当前表单里没有这个字段 ⇒ 省略（合法跳过）
    const encoded = encodeField(field, draft);
    if (encoded !== null) answers[key] = encoded;
  }

  for (const field of form.fields) {
    if (fieldIsActive(field, drafts) && fieldIsRequired(field) && !(field.key in answers)) {
      throw new DraftRefusal("missing_required", field.key);
    }
  }
  return answers;
}

/** 缺必答的字段（按表单顺序），供卡片当场列出「缺哪一题」（不产生任何请求）。 */
export function missingRequiredFields(
  form: PendingForm,
  drafts: Record<string, DraftAnswer>
): FormField[] {
  return form.fields.filter(
    (field) =>
      fieldIsActive(field, drafts) && fieldIsRequired(field) && draftIsEmpty(drafts[field.key])
  );
}

// ===== 组内草稿的页内存形态 =====
//
// 答案绝不落 storage：这里的对象只活在组件 state 里，页面关掉即消失。
//
// `touched` 记的是「这个值是在该题的哪个**内容版本**下填的」，版本 = {@link fieldSignature}
// （**题干标题** + 类型 + 选项集的 value/label + custom）。用**逐字段**版本而不是整张表单的
// `context_token`：
// - 整表单 token 会在**任何**字段变化（含兄弟字段被删）时轮换，于是没变过的题也被标成
//   「待核对」，字段集一缩小就锁死整组（评审 I-2）；
// - 逐字段版本只在「这一题的答案可能已经不是同一个意思」时变，漂移提示因此精确到题
//   （评审 M-3），也允许「只删了一个字段」的表单继续提交幸存题；
// - **题干必须参与**（G4）：同一组选项换了一道题（「删除测试库」→「删除生产库」，选项仍是
//   是/否）时，旧答案的「是」指向的是另一个对象；选项全同不代表答案还能静默沿用。

/** 一组表单的草稿。 */
export interface StoredDraft {
  values: Record<string, DraftAnswer>;
  /** 逐字段记录的**填写时内容版本**（缺键 = 来历不明，按陈旧处理）。 */
  touched: Record<string, string>;
}

/** 字段的内容版本：包含「会让已有答案变味」的东西（**题干**、类型、选项 value/label、
 *  是否自定义）。题干变化必须使旧答案 stale（同一组选项被挪到另一道题上，
 *  「是」/「否」的指代对象已经变了）；`required` 变化由必答检查兜住。 */
export function fieldSignature(field: FormField): string {
  const kind = answerableKind(field);
  if (kind === null) return "unsupported";
  const options = kind.options
    .map((option) => [option.value, option.label])
    .sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
  return JSON.stringify({
    title: field.title,
    kind: kind.kind,
    custom: kind.custom,
    options,
    when: field.when,
  });
}

/** 写入/清空一个字段（返回新对象，不改入参）。`value` 传 `{kind:"skip"}` 即「清除」。 */
export function applyDraftAnswer(
  stored: StoredDraft | undefined,
  signature: string,
  fieldKey: string,
  value: DraftAnswer
): StoredDraft {
  const base: StoredDraft = stored ?? { values: {}, touched: {} };
  return {
    values: { ...base.values, [fieldKey]: value },
    touched: { ...base.touched, [fieldKey]: signature },
  };
}

/** 该组的答案里哪些字段是「旧题目下填的」（有值，但填写时的内容版本与当前不符）。
 *  孤儿键（当前表单已没有该字段）**不在这里**：它们不需要核对，只需要被告知已移除。 */
export function staleAnswerKeys(form: PendingForm, stored: StoredDraft | undefined): string[] {
  if (stored === undefined) return [];
  const signatures = new Map(
    form.fields
      .filter((field) => fieldIsActive(field, stored.values))
      .map((field) => [field.key, fieldSignature(field)])
  );
  return Object.entries(stored.values)
    .filter(([, value]) => !draftIsEmpty(value))
    .filter(([key]) => signatures.has(key))
    .filter(([key]) => stored.touched[key] !== signatures.get(key))
    .map(([key]) => key);
}

/** 草稿里有值、但当前表单已没有的字段（旧字段：提交时省略，界面上只做一行告知）。 */
export function orphanAnswerKeys(form: PendingForm, stored: StoredDraft | undefined): string[] {
  if (stored === undefined) return [];
  const known = new Set(form.fields.map((field) => field.key));
  return Object.entries(stored.values)
    .filter(([, value]) => !draftIsEmpty(value))
    .filter(([key]) => !known.has(key))
    .map(([key]) => key);
}

/** 用户显式「已核对，沿用这些答案」：仅把当前适用的有值字段登记成当前内容版本，
 *  并顺手丢掉孤儿键（它们已经不属于这张表，也没有任何东西可沿用）。 */
export function reaffirmDraft(form: PendingForm, stored: StoredDraft): StoredDraft {
  const values: Record<string, DraftAnswer> = {};
  const touched: Record<string, string> = {};
  for (const field of form.fields) {
    const value = stored.values[field.key];
    if (value === undefined) continue;
    values[field.key] = value;
    // 隐藏草稿保留原版本，重新适用时仍须针对新题干核对。
    if (stored.touched[field.key] !== undefined) touched[field.key] = stored.touched[field.key];
    if (fieldIsActive(field, stored.values) && !draftIsEmpty(value)) {
      touched[field.key] = fieldSignature(field);
    }
  }
  return { values, touched };
}
