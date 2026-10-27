/**
 * tuvis-tps-observer — OpenCode 服务端最小观察插件（兔维斯 TPS 短时抽样，2026-10-24 计划 Task 6）。
 *
 * ## 宿主形态（实测核对，不凭残留类型猜接口）
 *
 * 锁定宿主 opencode v2.0.22（本机 /Users/jarvis/.opencode/bin/opencode，Bun 单文件）。
 * 插件默认导出必须是 `{ id, setup }` 或 `{ id, effect }`（二进制内校验文案
 * "Plugin must export a default definition with an id and an effect or setup function."）；
 * 服务端 context 的 `event.subscribe()` 返回 **AsyncIterable**（内部 Effect Stream 经
 * makeStreams/Stream.toAsyncStreamable 转出；上游 ChiR24/opencode-tps-meter 同族消费即
 * `for await`）。wire 事件信封 = `{ id, created, type, data }`（二进制事件构造点核实；
 * `properties` 形态属 2.1.x SDK 世代，作防御性回退）。2.0.22 事件词表（二进制核实）：
 * session.step.started/.ended/.failed、session.text.delta、session.reasoning.delta、
 * session.tool.input.delta/.ended、session.retry.scheduled（均带 sessionID +
 * assistantMessageID）。2.1.x 的 `session.next.*` 词表（@opencode-ai/sdk 1.17.13 类型）
 * 作为别名兼容；`session.next.retried` 无 assistantMessageID → 退化为会话级记数。
 *
 * ## 只观察、取数字后丢正文（spec §5）
 *
 * 每条事实一行 JSON（JSONL 追加写），键集固定：
 *   {eventKey,sessionKey,messageKey,ordinal,attempt,type,created,started,tokens,finish,
 *    hasNonEmptyDelta,deltaChars,model,provider,tokensReasoning}
 * * eventKey/sessionKey/messageKey = 事件 id / sessionID / assistantMessageID 的
 *   **稳定 hash 全长 hex**（默认 sha256，绝不截前 8 位；id 缺失时按行内容合成）；
 * * delta 只测 `.length` 即弃，正文/参数/思考内容绝不落盘；
 * * tokens 只取 `session.step.ended` 自带的 output（+reasoning 单列备查）——
 *   `session.usage.updated` / `message.updated` 等**累计面永不消费**（累计 usage
 *   不重复入账）；
 * * 中止（MessageAbortedError）单列 type="cancelled"，其余失败 type="failed"。
 *
 * ## 有界失败、绝不阻塞宿主（spec §5「采集端离线或磁盘失败必须有界失败且不阻塞Harness」）
 *
 * * 写盘异步、缓冲、硬上限；缓冲满即丢行（丢事实，不丢稳定）；
 * * 目录不可用 → 插件自禁用（host log 一次告警，不再尝试）；任何入口 try/catch 兜底，
 *   绝不向宿主 rethrow；定时器 unref，绝不独占进程生命周期；
 * * 订阅/消费失败写 `diag:*` 诊断行（有诊断，不静默吞错）后停止本轮消费，
 *   宿主重启自然重载。
 *
 * ## 事实落点（Rust 读取器同式可推导）
 *
 * `<home>/.tuvis/tps/opencode/facts.jsonl`（home = os.homedir()；目录 0700）。
 * 超过 4MiB 轮转为 facts.jsonl.1（单份回滚位），读取器按「文件收缩」语义幂等重读。
 *
 * ## 借鉴与许可
 *
 * 生命周期/事件分派/结束校准借鉴 ChiR24/opencode-tps-meter（MIT，锁
 * commit a6e050d2cbe340a3906e7233cdc880c3199355e4；同目录 NOTICE.md）。
 * 本文件为原创实现，仅借用法；未引入其 UI、字符估算或未实现的 retry 处理。
 * 仅用 Node 内建模块，零 npm 依赖。
 *
 * ## 分发扩展名（实测裁决）
 *
 * 宿主 2.0.22 的插件目录发现**不装载 `.mjs`**（同内容 `.mjs` 静默不载、`.ts` 正常
 * 载入——2026-10-24 隔离实测），故分发文件为 `.ts`：内容是**纯 JavaScript**（TS 是
 * JS 超集，Bun 转译无需类型），不引入任何类型依赖。
 */

import { createHash } from "node:crypto";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

/** 插件 id（宿主 `{id, setup}` 形态的 id 位；安装文件名同族可读） */
export const OBSERVER_ID = "tuvis-tps-observer";

/** 事实目录/文件（home 相对固定路径；Rust 读取器同式推导） */
export const FACT_RELATIVE_DIR = path.join(".tuvis", "tps", "opencode");
export const FACT_FILE_NAME = "facts.jsonl";
/** 轮转阈值（ facts.jsonl → facts.jsonl.1，单份回滚位） */
const ROTATE_BYTES = 4 * 1024 * 1024;
/** 缓冲行数硬上限（满即丢行，绝不无限增长） */
const MAX_BUFFER_LINES = 2000;
/** 单行字节上限（超长行 = 异常形态，丢弃不写） */
const MAX_LINE_BYTES = 8192;
/** eventKey 去重窗口（有界 FIFO；重复投递的旧事件超出窗口后重记一行，
 *  读取器按稳定键 UPSERT 幂等，不产生双计） */
const SEEN_CAP = 512;
/** 每键状态（ordinal/起点回显/retry 计数）的键数上限（长运行宿主的有界保证） */
const KEY_STATE_CAP = 512;

// ==== 事件词表（2.0.22 实测 + 2.1.x `session.next.*` 别名）====

const START_TYPES = new Set(["session.step.started", "session.next.step.started"]);
const DELTA_TYPES = new Set([
  "session.text.delta",
  "session.next.text.delta",
  "session.reasoning.delta",
  "session.next.reasoning.delta",
  "session.tool.input.delta",
  "session.next.tool.input.delta",
]);
/** 宿主可能缓冲工具参数、只发 ended（上游实测）；text 长度计一次 delta，内容即弃 */
const BUFFERED_INPUT_TYPES = new Set([
  "session.tool.input.ended",
  "session.next.tool.input.ended",
]);
const END_TYPES = new Set(["session.step.ended", "session.next.step.ended"]);
const FAILED_TYPES = new Set(["session.step.failed", "session.next.step.failed"]);
const RETRY_TYPES = new Set(["session.retry.scheduled", "session.next.retried"]);

/** 中止错误名（SDK 错误词表；abort = 用户取消，与一般失败分列） */
const ABORTED_ERROR_NAME = "MessageAbortedError";

/**
 * 创建观察器实例（依赖注入形态；生产用默认导出的默认依赖）。
 *
 * @param deps.homedir     home 解析（默认 os.homedir；测试注入临时目录）
 * @param deps.nowMs       墙钟（默认 Date.now；外源 created 的时钟偏移护栏基准）
 * @param deps.flushDelayMs 缓冲冲刷去抖（默认 150ms；测试注入 0~几 ms）
 * @param deps.hash        稳定 hash（默认 sha256 全长 hex；测试可注入 identity）
 */
export function createTpsObserver(deps = {}) {
  const homedir = deps.homedir ?? (() => os.homedir());
  const nowMs = deps.nowMs ?? (() => Date.now());
  const flushDelayMs = deps.flushDelayMs ?? 150;
  const hash = deps.hash ?? ((s) => createHash("sha256").update(s).digest("hex"));

  /** 事实文件绝对路径（setup 时一次性推导；此后不再逃逸该路径） */
  let factFile = null;

  // ---- 写出面（有界缓冲 + 异步追加；任何失败只丢自身，不碰宿主）----
  let sinkReady = false;
  let warned = false;
  const buffer = [];
  let timer = null;
  let flushing = false;
  /** 事件 id（hash 后）去重窗口 + 每键 ordinal + retry 计数 + 调用起点回显 */
  const seen = new Set();
  const seenOrder = [];
  const ordinals = new Map(); // sk\0mk -> 已记行数
  const retries = new Map(); // sk\0mk（或会话级 sk\0*）-> 未对齐重试次数
  const openStarted = new Map(); // sk\0mk -> 调用起点（start 行后回显到后续行）

  function keyOf(sk, mk) {
    return `${sk}\u0000${mk ?? "*"}`;
  }

  function warnOnce(message) {
    if (warned) return;
    warned = true;
    try {
      // host log 面：目录不可用等「无处写事实」的失败至少可见（不抛、不静默装好）
      console.warn(`[${OBSERVER_ID}] ${message}`);
    } catch {
      /* console 不可用：真无处可说，保持不抛 */
    }
  }

  function enableSink() {
    try {
      const dir = path.join(homedir(), FACT_RELATIVE_DIR);
      fs.mkdirSync(dir, { recursive: true });
      fs.accessSync(dir, fs.constants.W_OK);
      factFile = path.join(dir, FACT_FILE_NAME);
      // 已存在的轮转位不动（append-only 主文件语义；读取器按字节界增量消费）
      sinkReady = true;
      return true;
    } catch {
      sinkReady = false;
      factFile = null;
      return false;
    }
  }

  function flush() {
    if (flushing || !sinkReady || buffer.length === 0) return;
    const target = factFile;
    const lines = buffer.splice(0, buffer.length);
    flushing = true;
    try {
      // 轮转：主文件超阈值 → 换成 .1（覆盖上一份回滚位）
      try {
        const size = fs.statSync(target).size;
        if (size > ROTATE_BYTES) {
          fs.renameSync(target, `${target}.1`);
        }
      } catch {
        /* stat 失败（首写不存在等）按不轮转处理 */
      }
      fs.appendFile(target, lines.join("\n") + "\n", "utf8", () => {
        // 写失败回调：丢数据可接受， destabilize 宿主不可接受（探针同款裁决）
        flushing = false;
      });
    } catch {
      flushing = false;
    }
  }

  /** 落一行事实（完整固定键集；任何异常就地吞掉——写出面永不抛） */
  function record(row) {
    if (!sinkReady) return;
    try {
      const line = JSON.stringify(row);
      if (line.length > MAX_LINE_BYTES) return;
      if (buffer.length >= MAX_BUFFER_LINES) return; // 有界：满即丢
      buffer.push(line);
      if (timer) return;
      timer = setTimeout(() => {
        timer = null;
        try {
          flush();
        } catch {
          /* 冲刷失败不抛 */
        }
      }, flushDelayMs);
      if (typeof timer.unref === "function") timer.unref();
    } catch {
      /* 序列化失败不抛 */
    }
  }

  // ---- 事实行构造（固定 15 键，缺值 null——读取器按 Option 消费）----

  function blankRow(type) {
    return {
      eventKey: null,
      sessionKey: null,
      messageKey: null,
      ordinal: 0,
      attempt: 0,
      type,
      created: 0,
      started: null,
      tokens: null,
      finish: null,
      hasNonEmptyDelta: false,
      deltaChars: 0,
      model: null,
      provider: null,
      tokensReasoning: null,
    };
  }

  /** 诊断行（type=diag:<code>；订阅/形状/ID 缺失等失败可见不静默；带墙钟便于取证） */
  function diagRow(code) {
    const row = blankRow(`diag:${code}`);
    row.created = nowMs();
    return row;
  }

  /** 信封时刻：宿主 created 优先，缺失/外轴（偏移 >24h）回退墙钟（上游同款护栏） */
  function eventMs(ev) {
    const now = nowMs();
    for (const candidate of [ev?.created, ev?.timestamp]) {
      if (typeof candidate === "number" && Number.isFinite(candidate) && candidate > 0) {
        if (Math.abs(candidate - now) <= 86_400_000) return candidate;
      }
    }
    const dataTs = ev?.data?.timestamp ?? ev?.properties?.timestamp;
    if (typeof dataTs === "number" && Number.isFinite(dataTs) && dataTs > 0) {
      if (Math.abs(dataTs - now) <= 86_400_000) return dataTs;
    }
    return now;
  }

  /** 载荷：2.0.22 = data；2.1.x 世代 = properties（防御性回退，不猜接口形状之外的字段） */
  function payloadOf(ev) {
    const d = ev?.data;
    if (d && typeof d === "object") return d;
    const p = ev?.properties;
    if (p && typeof p === "object") return p;
    return {};
  }

  function finiteNumber(v) {
    return typeof v === "number" && Number.isFinite(v) ? v : null;
  }

  function str(v) {
    return typeof v === "string" && v.length > 0 ? v : null;
  }

  /** 事件 → 事实行；不属于观察词表返回 null（累计面/杂事件不入行） */
  function rowFor(ev) {
    const type = str(ev?.type);
    if (!type) return null;
    const data = payloadOf(ev);
    const sk = str(data.sessionID);
    if (!sk) return null;

    if (START_TYPES.has(type)) {
      const mk = str(data.assistantMessageID) ?? str(data.messageID);
      if (!mk) return diagRow("missing-ids");
      const row = blankRow("start");
      row.sessionKey = hash(sk);
      row.messageKey = hash(mk);
      row.created = eventMs(ev);
      // 宿主 step.started 可能惰性发布：data.started 才是真起点（上游实测同款）
      const started = finiteNumber(data.started);
      row.started = started ?? row.created;
      row.model = str(data.model?.id) ?? str(data.model?.modelID);
      row.provider = str(data.model?.providerID);
      const okey = keyOf(row.sessionKey, row.messageKey);
      openStarted.set(okey, row.started);
      evictOldest(openStarted);
      return finishRow(ev, row, sk, mk);
    }

    if (DELTA_TYPES.has(type) || BUFFERED_INPUT_TYPES.has(type)) {
      const mk = str(data.assistantMessageID) ?? str(data.messageID);
      if (!mk) return diagRow("missing-ids");
      const row = blankRow("delta");
      row.sessionKey = hash(sk);
      row.messageKey = hash(mk);
      row.created = eventMs(ev);
      // 正文/参数/思考只测长即弃：delta 直取；缓冲形态取 text 长度
      const content =
        typeof data.delta === "string" ? data.delta : typeof data.text === "string" ? data.text : "";
      row.hasNonEmptyDelta = content.length > 0;
      row.deltaChars = content.length;
      row.started = openStarted.get(keyOf(row.sessionKey, row.messageKey)) ?? null;
      return finishRow(ev, row, sk, mk);
    }

    if (END_TYPES.has(type)) {
      const mk = str(data.assistantMessageID) ?? str(data.messageID);
      if (!mk) return diagRow("missing-ids");
      const row = blankRow("end");
      row.sessionKey = hash(sk);
      row.messageKey = hash(mk);
      row.created = eventMs(ev);
      row.started = openStarted.get(keyOf(row.sessionKey, row.messageKey)) ?? null;
      // usage 只取本事件自带数值；累计面永不消费 → 不重复入账
      row.tokens = finiteNumber(data.tokens?.output);
      row.tokensReasoning = finiteNumber(data.tokens?.reasoning);
      row.finish = str(data.finish);
      // 未对齐重试折叠进终行 attempt（识别而不入速的载体），随后清零
      const rkey = keyOf(row.sessionKey, row.messageKey);
      row.attempt = retries.get(rkey) ?? 0;
      retries.delete(rkey);
      openStarted.delete(rkey);
      return finishRow(ev, row, sk, mk);
    }

    if (FAILED_TYPES.has(type)) {
      const mk = str(data.assistantMessageID) ?? str(data.messageID);
      if (!mk) return diagRow("missing-ids");
      const aborted = data.error?.name === ABORTED_ERROR_NAME;
      const row = blankRow(aborted ? "cancelled" : "failed");
      row.sessionKey = hash(sk);
      row.messageKey = hash(mk);
      row.created = eventMs(ev);
      row.started = openStarted.get(keyOf(row.sessionKey, row.messageKey)) ?? null;
      row.finish = str(data.finish);
      const rkey = keyOf(row.sessionKey, row.messageKey);
      row.attempt = retries.get(rkey) ?? 0;
      retries.delete(rkey);
      openStarted.delete(rkey);
      return finishRow(ev, row, sk, mk);
    }

    if (RETRY_TYPES.has(type)) {
      const mk = str(data.assistantMessageID) ?? str(data.messageID);
      const row = blankRow("retry");
      row.sessionKey = hash(sk);
      // 2.0.22 带 assistantMessageID；2.1.x retried 不带 → 会话级（messageKey=null）
      row.messageKey = mk ? hash(mk) : null;
      row.created = eventMs(ev);
      row.attempt = finiteNumber(data.attempt) ?? 0;
      const rkey = keyOf(row.sessionKey, row.messageKey);
      retries.set(rkey, (retries.get(rkey) ?? 0) + 1);
      evictOldest(retries);
      if (mk) {
        const started = openStarted.get(keyOf(row.sessionKey, row.messageKey));
        if (started !== undefined) row.started = started;
      }
      return finishRow(ev, row, sk, mk);
    }

    return null; // 观察词表之外（idle/compaction/permission/…）不入行
  }

  /** 事件键 + 幂等去重 + 每键 ordinal（交错不串键的序数面） */
  function finishRow(ev, row, rawSession, rawMessage) {
    const idSource = str(ev?.id) ?? JSON.stringify([rawSession, rawMessage, row.type, row.created]);
    row.eventKey = hash(idSource);
    if (seen.has(row.eventKey)) return null; // 重复 event：幂等，不再记行
    seen.add(row.eventKey);
    seenOrder.push(row.eventKey);
    while (seenOrder.length > SEEN_CAP) {
      const oldest = seenOrder.shift();
      seen.delete(oldest);
    }
    const okey = keyOf(row.sessionKey, row.messageKey);
    row.ordinal = (ordinals.get(okey) ?? 0) + 1;
    ordinals.set(okey, row.ordinal);
    evictOldest(ordinals);
    return row;
  }

  /** 有界保证：最旧插入序键驱逐（Map 保插入序；现实远达不到上限，纯纪律兜底） */
  function evictOldest(map) {
    while (map.size > KEY_STATE_CAP) {
      const oldest = map.keys().next().value;
      if (oldest === undefined) break;
      map.delete(oldest);
    }
  }

  // ---- 订阅消费（acceptance 关卡：合法 AsyncIterable 无 next 也能消费）----

  async function consume(context) {
    const subscribe = context?.event?.subscribe;
    if (typeof subscribe !== "function") {
      record(diagRow("no-event-subscribe"));
      return;
    }
    let stream;
    try {
      stream = await subscribe.call(context.event);
    } catch {
      record(diagRow("subscribe-throw")); // 订阅失败有诊断，不静默吞
      return;
    }
    if (stream == null) {
      record(diagRow("stream-missing"));
      return;
    }
    try {
      // 标准协议：for-await 会先取 [Symbol.asyncIterator]() 得到迭代器再调 next——
      // iterable 自身没有 next 也照常消费（探针旧写法把 iterable 当迭代器包一层，
      // 恰是上一轮「loaded 但收不到事件」的根因，评估报告已点名）。
      if (typeof stream[Symbol.asyncIterator] === "function") {
        record(diagRow("stream-asynciterable"));
        for await (const ev of stream) {
          const row = safeRow(ev);
          if (row) record(row);
        }
        return;
      }
      // 裸迭代器形态（无 Symbol.asyncIterator、有 next）：显式 next 循环
      if (typeof stream.next === "function") {
        record(diagRow("stream-iterator"));
        for (;;) {
          const chunk = await stream.next();
          if (!chunk || chunk.done) break;
          const row = safeRow(chunk.value);
          if (row) record(row);
        }
        return;
      }
      // v1 回调形态（防御分支，2.0.22 已核实不走此支）
      if (typeof stream === "function") {
        record(diagRow("stream-callback"));
        stream((ev) => {
          const row = safeRow(ev);
          if (row) record(row);
        });
        return;
      }
      // 无法识别的返回形态（如 Effect Stream 描述符直传）：诊断可见，不猜
      record(diagRow("stream-unsupported"));
    } catch {
      record(diagRow("loop-error")); // 消费中断有诊断；本轮停止，宿主重启自然重载
    }
  }

  /** 单事件处理兜底：坏事件绝不逃逸进宿主事件循环 */
  function safeRow(ev) {
    try {
      return rowFor(ev);
    } catch {
      return diagRow("event-error");
    }
  }

  // ---- 宿主接口 ----

  return {
    id: OBSERVER_ID,

    /**
     * 服务端 setup（2.0.22 `{id, setup}` 形态）。返回 teardown（宿主会 await）；
     * 任何准备失败都退化为惰性插件（自禁用），绝不向宿主抛。
     */
    async setup(context) {
      try {
        if (!enableSink()) {
          warnOnce("事实目录不可用，观察器自禁用（不阻塞宿主）");
          return undefined;
        }
        record(diagRow("loaded")); // 装载足迹（loaded ≠ 支持；验收靠真实 start→delta→end）
        consume(context); // 有自身 try/catch 兜底，fire-and-forget
        return async () => {
          try {
            if (timer) {
              clearTimeout(timer);
              timer = null;
            }
            flush(); // 退出前把缓冲写完（异步 append 已发起即尽力）
          } catch {
            /* teardown 不抛 */
          }
        };
      } catch {
        return undefined;
      }
    },
  };
}

/** 生产实例（真实依赖；`{id, setup}` 默认导出即宿主装载形态） */
export default createTpsObserver();
