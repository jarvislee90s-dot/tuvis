// OpenCode 原生视图订阅 hook（计划任务 9）：详情页/输入区唯一的数据通道接驳层。
//
// 职责与纪律：
// - 只读：所有通道（首次 GET、SSE 修订号事件、手动 refresh、断流后重连）都只调用
//   `GET /opencode/view`。**SSE 断线只刷新，绝不提交**——本 hook 里不存在任何
//   `POST /opencode/actions` 调用（写只有用户点击才会发生，见 OpenCodeNativePanel）。
// - 代际隔离：换会话 / 关闭启用 / 卸载都会推进代际并 abort 在途请求，旧响应一律不落地
//   （与 useCodexThread 的「旧会话不得污染新镜像」同款防线）。
// - 视图不清空：一次刷新失败（网络异常 / 受控拒绝）保留上一份视图并给出 error——
//   「部分投影」不得被当成「空会话」，也不得被当成完整历史。
// - 不接管轮询：首批只有修订号事件流；断流后按固定退避重连，每次重连成功/失败都只是
//   一次只读重拉，不做「失败即提交」的任何补偿动作。
import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError } from "./api";
import {
  OPENCODE_NATIVE_EVENTS_PATH,
  fetchOpenCodeView,
  type PublicNativeView,
} from "./opencodeNativeApi";

/** 断流后重连间隔（毫秒）：固定退避，不做指数升级（原生事件只是「该重读了」的信号，
 *  退避过长会让手机长时间停在旧投影上；每次重连本身也只读）。 */
export const OPENCODE_NATIVE_RECONNECT_MS = 3000;

export interface UseOpenCodeNativeResult {
  /** 最近一次成功读取的权威投影；null = 尚未读到（加载中 / 拉取失败）。 */
  view: PublicNativeView | null;
  /** 最近一次读取的错误（清除时机 = 下一次成功读取）；null = 无错误。 */
  error: ApiError | null;
  /** I2：「最后成功视图」的**当前有效 / 读取失败**独立状态。true = 这份 view 是
   *  读取失败前留下的旧快照——消费方必须保留其内容（正文/题卡/草稿/锚不清不卸），
   *  但不得把它当作当前可写视图（阻断全部新原生写入口）；只读核对不受影响。 */
  readFailed: boolean;
  /** 手动只读重拉（SSE 事件 / 断线 / 用户点击「重新读取」共用同一出口）。 */
  refresh: () => void;
  /** 用户显式确认后重新选择当前目标；失败时保留原视图和绑定。 */
  reselect: (bindingToken: string, routeVersion?: number) => Promise<boolean>;
}

export default function useOpenCodeNative(
  sessionId: string,
  enabled: boolean,
  onSnapshot?: (view: PublicNativeView) => void,
  initialBindingToken?: string | null,
  initialRouteVersion?: number
): UseOpenCodeNativeResult {
  const snapshotCallback = useRef(onSnapshot);
  useEffect(() => {
    snapshotCallback.current = onSnapshot;
  }, [onSnapshot]);
  const [view, setView] = useState<PublicNativeView | null>(null);
  const [error, setError] = useState<ApiError | null>(null);
  const [binding, setBinding] = useState<string | null>(null);
  const bindingRef = useRef<string | null>(null);
  /** 代际：换会话/关闭/卸载 +1；迟到响应按代际作废（不写 state） */
  const generationRef = useRef(0);
  /** 在途只读请求（切会话/卸载时 abort） */
  const inflightRef = useRef<AbortController | null>(null);
  const selectingRef = useRef(false);
  /** 当前会话（回调闭包里读 ref，避免依赖项抖动） */
  const sessionRef = useRef(sessionId);
  /** 最近一次已落地的 revision：view 自身发布也会触发 SSE revision；若不比较，
   *  每次 GET 都把 revision+1 再触发一次 GET，形成自激循环（真机 UI 会一直“读取中”）。
   *  <= 当前值的 revision 只是自己的回显/旧事件，忽略。 */
  const lastRevisionRef = useRef(0);
  /** SSE 在 GET 在途时到达：不要 abort（会把刚发出的读又取消，形成
   *  “读→事件→abort→再读”的自激）；只记一个待刷新，当前读落地后再补一次。 */
  const pendingReloadRef = useRef(false);
  /** SSE 在 GET 在途时到达的**修订号**（不是 bool）：响应落地后若该修订已被
   *  返回值覆盖（<= lastRevision），就不该再发一次 GET；否则每次纯读都会
   *  “通知→读→再通知”自激。只有真正更大的修订才补一次。 */
  const pendingRevisionRef = useRef<number | null>(null);

  /** 旧绑定 stale 后的单次只读恢复闸：去绑定重读一次，不循环。 */
  const staleRecoveredRef = useRef(false);
  /** 最近一次**带绑定采纳**的视图目标（目录 + 服务代际）：恢复时核对原目标，
   *  防止自动恢复把用户从原选 A 静默换到同 ID 的另一目录/服务。 */
  const initialVersionRef = useRef(initialRouteVersion);
  const lastTargetRef = useRef<{ directory: string; serviceId: string; pid: number } | null>(null);

  /** 单次只读重拉：成功后清错误；失败（含受控拒绝）保留上一份视图。 */
  const load = useCallback((force = false) => {
    if (selectingRef.current) {
      pendingReloadRef.current = true;
      return;
    }
    const generation = generationRef.current;
    if (inflightRef.current !== null && !force) {
      pendingReloadRef.current = true;
      return;
    }
    inflightRef.current?.abort();
    const ctrl = new AbortController();
    inflightRef.current = ctrl;
    const sid = sessionRef.current;
    fetchOpenCodeView(sid, ctrl.signal, bindingRef.current)
      .then((next) => {
        if (generation !== generationRef.current || inflightRef.current !== ctrl) return; // 旧代际：不落地
        if (
          initialVersionRef.current !== undefined &&
          next.routeVersion !== initialVersionRef.current
        ) {
          setError(
            new ApiError(409, "native route version changed", { error: "OC_BINDING_STALE" })
          );
          return;
        }
        initialVersionRef.current = undefined;
        if (bindingRef.current !== null && next.bindingToken !== bindingRef.current) {
          setError(new ApiError(409, "native binding changed", { error: "OC_BINDING_STALE" }));
          recoverFromStaleBinding();
          return;
        }
        if (bindingRef.current === null && next.bindingToken) {
          const nextTarget = {
            directory: typeof next.directory === "string" ? next.directory : "",
            serviceId: typeof next.serviceId === "string" ? next.serviceId : "",
            pid: typeof next.servicePid === "number" ? next.servicePid : 0,
          };
          if (staleRecoveredRef.current && lastTargetRef.current) {
            // 恢复核对（P2，含重复刷新）：恢复模式在代际内持久——首次拒绝后
            // 绑定已清空，后续每次无绑定响应仍必须通过同一核对，绝不因重试
            // 次数绕过。只自动采纳**可证实为同一目标**的视图（目录一致；服务
            // 重启换 PID/代际仍算同一目标）。目标漂移 = 保留旧视图与错误态，
            // 交给显式重选入口（预览→确认）。
            const sameTarget =
              nextTarget.directory !== "" &&
              nextTarget.directory === lastTargetRef.current.directory &&
              nextTarget.serviceId === lastTargetRef.current.serviceId;
            if (!sameTarget) {
              setError(
                new ApiError(409, "native target changed during recovery", {
                  error: "OC_TARGET_CHANGED",
                })
              );
              return;
            }
          }
          bindingRef.current = next.bindingToken;
          setBinding(next.bindingToken);
          if (nextTarget.directory !== "") lastTargetRef.current = nextTarget;
        } else if (bindingRef.current !== null) {
          const nextTarget = {
            directory: typeof next.directory === "string" ? next.directory : "",
            serviceId: typeof next.serviceId === "string" ? next.serviceId : "",
            pid: typeof next.servicePid === "number" ? next.servicePid : 0,
          };
          if (nextTarget.directory !== "") lastTargetRef.current = nextTarget;
        }
        // 不能用 max：远程停开/热重启后 revision 从 1 重新开始，max 会把新序列
        // 全部当成回显（客户端会一直停在旧视图/旧错误）。顺序 load 由 abort/代际闸保证。
        lastRevisionRef.current = next.revision;
        // 在途期间收到的 SSE 若已被本次响应覆盖，丢掉；只有更大修订才补读。
        if (pendingRevisionRef.current !== null && pendingRevisionRef.current <= next.revision) {
          pendingRevisionRef.current = null;
        }
        snapshotCallback.current?.(next);
        setView(next);
        setError(null);
      })
      .catch((e: unknown) => {
        if (generation !== generationRef.current || inflightRef.current !== ctrl) return;
        // 失败时消费掉本次待刷新：否则 pendingRevision 会在每个失败的 finally 里
        // 再次触发 load，形成无退避的失败重试环。下一次真实 SSE/手动刷新再试。
        pendingReloadRef.current = false;
        pendingRevisionRef.current = null;
        if (e instanceof DOMException && e.name === "AbortError") return; // 主动中止不算错误
        setError(e instanceof ApiError ? e : new ApiError(null, String(e)));
        // 服务换代后旧绑定的只读恢复（P2）：服务端返回 OC_BINDING_STALE 时，
        // 旧绑定只是本端期待提示——丢弃后按无绑定重读一次；草稿、未决动作锚
        // 与其它本地状态不受影响。只恢复一次，绝不循环，也绝不因此发写请求。
        // 旧服务进程退出时后端返回 OC_RUNTIME_UNVERIFIED（固定目标进程事实不可
        // 证）；与 stale 同属「旧绑定已失效、可无绑定重读」的恢复触发。
        const recoverable =
          e instanceof ApiError &&
          e.status === 409 &&
          (e.data?.error === "OC_BINDING_STALE" || e.data?.error === "OC_RUNTIME_UNVERIFIED") &&
          bindingRef.current !== null &&
          !staleRecoveredRef.current;
        if (recoverable) {
          recoverFromStaleBinding();
        }
      })
      .finally(() => {
        if (inflightRef.current !== ctrl) return;
        inflightRef.current = null;
        if (pendingReloadRef.current) {
          pendingReloadRef.current = false;
          pendingRevisionRef.current = null;
          load();
        } else if (
          pendingRevisionRef.current !== null &&
          pendingRevisionRef.current > lastRevisionRef.current
        ) {
          // 消费这条通知再补读：服务重连可返回低 revision，旧通知不能
          // 因为数字仍较大而无限重试；补读期间的新通知仍会单独登记。
          pendingRevisionRef.current = null;
          load();
        }
      });
  }, []);

  /** 旧绑定 stale 的单次只读恢复：丢弃本端期待绑定（只是提示，不是服务端状态），
   *  按无绑定重读一次——服务换代后的只读会话由此在同一页面恢复，草稿与未决
   *  动作锚全部保留；绝不因此发送任何写请求。 */
  const recoverFromStaleBinding = useCallback(() => {
    // 显式重选在途：绝不并发恢复（那会与重选的事务目标竞争）；挂起一次普通刷新，
    // 重选结束后由既有 pendingReload 机制按当时绑定执行。
    if (selectingRef.current) {
      pendingReloadRef.current = true;
      return;
    }
    if (staleRecoveredRef.current) return;
    staleRecoveredRef.current = true;
    bindingRef.current = null;
    setBinding(null);
    load();
    // eslint-disable-next-line react-hooks/exhaustive-deps -- load 稳定（[] 依赖），仅取首例
  }, []);

  const refresh = useCallback(() => {
    if (!enabled) return;
    load(true);
  }, [enabled, load]);

  const reselect = useCallback(
    async (bindingToken: string, routeVersion?: number): Promise<boolean> => {
      if (!enabled || !bindingToken || selectingRef.current) return false;
      const generation = generationRef.current;
      selectingRef.current = true;
      pendingReloadRef.current = false;
      pendingRevisionRef.current = null;
      inflightRef.current?.abort();
      const ctrl = new AbortController();
      inflightRef.current = ctrl;
      try {
        const next = await fetchOpenCodeView(sessionRef.current, ctrl.signal, bindingToken);
        if (generation !== generationRef.current || inflightRef.current !== ctrl) return false;
        if (
          next.bindingToken !== bindingToken ||
          (routeVersion !== undefined && next.routeVersion !== routeVersion)
        ) {
          setError(new ApiError(409, "native binding changed", { error: "OC_BINDING_STALE" }));
          return false;
        }
        initialVersionRef.current = undefined;
        bindingRef.current = bindingToken;
        setBinding(bindingToken);
        // 显式重选成功 = 用户确认的新目标：刷新基准目标并退出恢复模式，
        // 后续自动恢复以新目标为核对基准。
        lastTargetRef.current = {
          directory: typeof next.directory === "string" ? next.directory : "",
          serviceId: typeof next.serviceId === "string" ? next.serviceId : "",
          pid: typeof next.servicePid === "number" ? next.servicePid : 0,
        };
        staleRecoveredRef.current = false;
        lastRevisionRef.current = next.revision;
        if (pendingRevisionRef.current !== null) pendingReloadRef.current = true;
        pendingRevisionRef.current = null;
        snapshotCallback.current?.(next);
        setView(next);
        setError(null);
        return true;
      } catch (error) {
        if (
          generation === generationRef.current &&
          inflightRef.current === ctrl &&
          !(error instanceof DOMException && error.name === "AbortError")
        ) {
          setError(error instanceof ApiError ? error : new ApiError(null, String(error)));
        }
        return false;
      } finally {
        if (inflightRef.current === ctrl) {
          inflightRef.current = null;
          selectingRef.current = false;
          if (pendingReloadRef.current) {
            pendingReloadRef.current = false;
            load();
          }
        }
      }
    },
    [enabled, load]
  );

  // 会话/启用态变化：推进代际 + 丢弃上一会话视图（视图是会话级事实，不跨会话复用）
  useEffect(() => {
    sessionRef.current = sessionId;
    generationRef.current += 1;
    inflightRef.current?.abort();
    inflightRef.current = null;
    selectingRef.current = false;
    pendingReloadRef.current = false;
    pendingRevisionRef.current = null;
    setView(null);
    setError(null);
    initialVersionRef.current = initialRouteVersion;
    bindingRef.current = initialBindingToken ?? null;
    setBinding(initialBindingToken ?? null);
    lastRevisionRef.current = 0;
    staleRecoveredRef.current = false;
    lastTargetRef.current = null;
  }, [sessionId, enabled, initialBindingToken, initialRouteVersion]);

  useEffect(() => {
    if (enabled) load();
  }, [enabled, sessionId, load, initialBindingToken, initialRouteVersion]);

  useEffect(() => {
    if (!enabled || binding === null) return;

    // 修订号事件流：只当「该重读了」的信号用。环境无 EventSource（老内核/测试未替身）
    // → 不做任何补偿请求（不轮询、不提交）。
    const ES = (globalThis as { EventSource?: typeof EventSource }).EventSource;
    let es: EventSource | null = null;
    let reconnect: ReturnType<typeof setTimeout> | null = null;
    let stopped = false;
    // **单飞**（M11）：同一时刻最多一个连接、**最多一个**重连定时器。浏览器的
    // EventSource 在断流时会连续投多次 error，早期实现每次都排一个 setTimeout 并建
    // 新连接——定时器与连接会成倍叠加。这里三条闸：`connect` 入口看 `es`/`reconnect`，
    // 处理器只认自己那一个 source，重连前先清已在排队的定时器。
    const connect = () => {
      reconnect = null;
      if (stopped || es !== null || typeof ES !== "function") return;
      let source: EventSource;
      try {
        source = new ES(
          `${OPENCODE_NATIVE_EVENTS_PATH}?${new URLSearchParams({ session_id: sessionRef.current, expected_binding_token: binding })}`
        );
      } catch {
        return; // 建连即失败：放弃订阅（视图仍可用，用户可手动重读）
      }
      es = source;
      // 新连接（含远程停开/热重启后的重连）：修订序列可能从 1 重新开始。
      // 清基线并主动只读重拉一次，不能等“更大的 revision”把旧值比下去。
      source.addEventListener("open", () => {
        if (stopped || es !== source) return;
        lastRevisionRef.current = 0;
        pendingRevisionRef.current = null;
        load();
      });
      source.addEventListener("revision", (event) => {
        if (stopped || es !== source) return;
        // 修订推进 ⇒ 只读重拉。但 view 自己发布 revision 也会回显到这里：忽略
        // <= 已落地值的事件，否则 GET→revision→GET 自激（真机表现为永远加载）。
        const revision = Number((event as MessageEvent).data);
        if (!Number.isFinite(revision) || revision <= lastRevisionRef.current) return;
        if (inflightRef.current !== null) {
          // 通知先于 GET 响应：只记修订号，响应落地后再判断是否已被覆盖，
          // 不能立即排队第二次读（那会把每次纯读变成下一次读的触发器）。
          pendingRevisionRef.current = Math.max(pendingRevisionRef.current ?? 0, revision);
          return;
        }
        load();
      });
      source.onerror = () => {
        // 只处理当前连接的事件（已被替换/关闭的旧 source 迟到 error 一律忽略）
        if (stopped || es !== source) return;
        es = null;
        source.close();
        // **断线只刷新**：先重拉一次，再按固定间隔重连（从不提交、从不回放写）
        load();
        if (reconnect === null) reconnect = setTimeout(connect, OPENCODE_NATIVE_RECONNECT_MS);
      };
    };
    connect();

    return () => {
      stopped = true;
      es?.close();
      es = null;
      if (reconnect !== null) clearTimeout(reconnect);
    };
  }, [enabled, sessionId, load, binding]);

  // 卸载：abort 在途请求 + 代际失效（避免 setState on unmounted）
  useEffect(
    () => () => {
      generationRef.current += 1;
      inflightRef.current?.abort();
      inflightRef.current = null;
    },
    []
  );

  return { view, error, readFailed: error !== null, refresh, reselect };
}
