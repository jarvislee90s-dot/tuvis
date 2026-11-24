import { useEffect, useRef, useState } from "react";
import { ApiError, fetchDshCurrentTurn, stopDshCurrentTurn } from "./api";
import { newActionId } from "./codexIpcApi";
import type { Session } from "@/types/session";

export type DshTurnControl = ReturnType<typeof useDshTurnControl>;

export function useDshTurnControl(session: Session, refreshTick: number) {
  const instanceId = session.agentType === "dsh" ? session.dshInstanceId : undefined;
  const sessionId = session.id;
  const working =
    session.agentType === "dsh" &&
    ["processing", "thinking", "compacting", "waiting"].includes(session.status);
  const [observed, setObserved] = useState<string | null | undefined>(undefined);
  const [phase, setPhase] = useState<"idle" | "stopping" | "unknown" | "failed">("idle");
  const activeStop = useRef<string | null>(null);
  const [refresh, setRefresh] = useState(0);

  useEffect(() => {
    setObserved(undefined);
    setPhase("idle");
    activeStop.current = null;
  }, [instanceId, sessionId]);

  useEffect(() => {
    if (!instanceId || (!working && activeStop.current === null && !observed)) return;
    const controller = new AbortController();
    const { signal } = controller;
    void fetchDshCurrentTurn(instanceId, sessionId, signal)
      .then((turn) => {
        if (signal.aborted) return;
        setObserved(turn);
        if (activeStop.current !== null && activeStop.current !== turn) {
          activeStop.current = null;
          setPhase("idle");
        }
      })
      .catch(() => {
        if (!signal.aborted) setObserved(undefined);
      });
    return () => controller.abort();
    // 正文刷新、回合状态变更或提交回执触发读回，避免由读取结果自触发轮询。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [instanceId, sessionId, working, refreshTick, refresh]);

  const running =
    (observed !== null && (working || observed !== undefined)) ||
    phase === "stopping" ||
    phase === "unknown";
  async function stop() {
    if (!instanceId || !observed || activeStop.current !== null) return;
    const turn = observed;
    activeStop.current = turn;
    setPhase("stopping");
    try {
      const id = newActionId();
      const { receipt } = await stopDshCurrentTurn(instanceId, sessionId, turn, id);
      if (activeStop.current !== turn) return;
      if (receipt.state === "unknown_delivery") setPhase("unknown");
      else if (["failed", "rejected", "cancelled"].includes(receipt.state)) {
        activeStop.current = null;
        setPhase("failed");
      }
    } catch (error) {
      if (activeStop.current !== turn) return;
      if (
        error instanceof ApiError &&
        error.status !== null &&
        error.status >= 400 &&
        error.status < 500
      ) {
        activeStop.current = null;
        setPhase("failed");
      } else setPhase("unknown");
    } finally {
      setRefresh((value) => value + 1);
    }
  }
  return {
    running,
    ready: Boolean(instanceId && observed) && phase !== "stopping" && phase !== "unknown",
    phase,
    stop,
  };
}
