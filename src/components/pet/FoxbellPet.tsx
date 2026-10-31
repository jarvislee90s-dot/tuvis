// FoxbellPet — 桌宠本体（spec §7/§8/§9）。Task 8：精灵 + 帧步进 + look 环顾 + 缩放；
// Task 9：指针交互（拖拽方向动画/松手物理/单击/双击）+ 语音字幕；Task 10：状态卡片 + 跳转/歧义候选；Task 12 追加事件接线。
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import { toast } from "sonner";
import { useTranslation } from "react-i18next";
import type { Session } from "@/types/session";
import { useSessionJump } from "@/hooks/useSessionJump";
import { useSessionsQuery } from "@/lib/query/queries/sessions";
import { useScanSnapshotSync } from "@/lib/query/queries/scanBus";
import { sessionTitleOrUndefined } from "@/lib/sessionTitle";
import { setupSessionReadListener } from "@/lib/query/sessionReadSync";
import { ANIM, frameStyle, FRAME_H, FRAME_W, type PetAnimKey } from "./petAnimations";
import {
  DEFAULT_PET,
  builtinPet,
  loadActiveId,
  resolveActivePet,
  type ActivePet,
} from "./petRuntime";
import {
  loadConfig,
  loadVisible,
  saveVisible,
  subscribeConfig,
  type PetAction,
  type PetConfig,
} from "./petConfig";
import {
  ackDone,
  cardsFromState,
  computePetStatus,
  type PetCard,
  type PetStatusState,
} from "./petStatus";
import { MIN_SPEECH_MS, VoicePlayer, type VoiceGroup } from "./petVoices";
import { PetMenu } from "./PetMenu";
import { UsageMiniBar } from "./UsageMiniBar";
import { usePetWindow } from "./usePetWindow";
import { useTheme } from "@/components/common/theme-provider";
import { openUsageDashboard } from "@/lib/usage/openWindow";
import {
  MINI_GRACE_MS,
  MINI_HOVER_MS,
  MINI_RESTORE_MS,
  miniBarGap,
  miniBarHeight,
  windowHeightSum,
} from "@/lib/usage/miniBar";
import "./pet-cursor.css";

export function FoxbellPet() {
  const { t } = useTranslation();
  // Task 2：宠物窗口也接快照事件（本窗有自己的 Query 缓存）——后端每次发布快照
  // 直接 setQueryData，桌宠状态卡不必等档位轮询；档位轮询仍作兜底。
  useScanSnapshotSync();
  // 当前主题（计划② Task 6 步骤 11）：看板窗口按它建窗（`system` → undefined，跟随系统；
  // 主题事实源是 DB settings KV，见 theme-provider 的模块级接线）
  const { theme } = useTheme();
  const [cfg, setCfg] = useState<PetConfig>(() => loadConfig());
  const cfgRef = useRef(cfg);
  // 渲染期禁止写 ref（react-hooks/refs）：改在 effect 中同步，供 Task 9 交互读取最新配置
  useEffect(() => {
    cfgRef.current = cfg;
  }, [cfg]);
  useEffect(() => subscribeConfig(() => setCfg(loadConfig())), []);

  // ---- 激活宠物（外部宠物热切换，spec §12）----
  const [active, setActive] = useState<ActivePet>(() => builtinPet(loadActiveId()) ?? DEFAULT_PET);
  const activeRef = useRef(active);
  const rowsRef = useRef<9 | 11>(11);
  // 上一个 ActivePet（blob 快照防泄漏，FIX-2）：切换时 revoke 旧快照；
  // StrictMode 双挂载各持独立快照，第一个被下一次 refresh 替换时 dispose 是正确行为
  const prevActiveRef = useRef<ActivePet | null>(null);
  // 刷新代数（FIX-6 后到者胜）：并发 refresh 时旧解析后到不得回滚显示、不得 dispose 活跃快照；
  // 被丢弃的解析（过期/卸载）结果回收其 blob 快照
  const refreshGen = useRef(0);
  const applyActive = (next: ActivePet) => {
    const prev = prevActiveRef.current;
    if (prev && prev !== next) prev.dispose?.();
    prevActiveRef.current = next;
    setActive(next);
  };
  useEffect(() => {
    activeRef.current = active;
    rowsRef.current = active.rows;
  }, [active]);
  // 组件卸载：释放当前快照（refresh effect 的 disposed 闸门保证不会在卸载后 setActive）。
  // 已知 dev-only 行为（issue #33-13）：StrictMode 双挂载时第一棵树的 cleanup 会 dispose
  // 仍被 state 引用显示的快照 → 重挂载 refresh 完成前音频 blob URL 短暂失效（图像走
  // convertFileSrc 静态路径不受影响）。生产不双挂载，无影响；勿据此调整 prevActiveRef 生命周期。
  useEffect(
    () => () => {
      prevActiveRef.current?.dispose?.();
    },
    []
  );

  const pet = usePetWindow();
  const { registerInteractive, contentRef, setMenuOpen } = pet; // useCallback/useRef 稳定引用，避免依赖整个 pet 对象（每次渲染重建）
  const spriteRef = useRef<HTMLDivElement | null>(null);
  useEffect(() => registerInteractive(spriteRef.current), [registerInteractive]);

  // ---- 动画状态机（spec §7：拖拽 > 瞬时 > 任务态 > look > idle）----
  const [anim, setAnim] = useState<PetAnimKey>("idle");
  const [frame, setFrame] = useState(0);
  const [lookFrame, setLookFrame] = useState(-1);
  const animRef = useRef<PetAnimKey>("idle");
  const frameRef = useRef(0);
  const stepTimer = useRef<number | null>(null);
  const stateRef = useRef<{
    drag: PetAnimKey | null;
    transient: PetAnimKey | null;
    task: PetAnimKey | null;
    look: boolean;
  }>({
    drag: null,
    transient: null,
    task: null,
    look: false,
  });
  const lookStop = useRef<(() => void) | null>(null);
  const genRef = useRef({ transient: 0, look: 0 });

  const later = useRef((fn: () => void, ms: number) => {
    const id = window.setTimeout(fn, ms);
    return () => window.clearTimeout(id);
  }).current;

  const cancelStep = () => {
    if (stepTimer.current !== null) {
      window.clearTimeout(stepTimer.current);
      stepTimer.current = null;
    }
  };

  const stepLoop = () => {
    const def = ANIM[(animRef.current === "look" ? "idle" : animRef.current) as keyof typeof ANIM];
    const i = frameRef.current;
    const ms = def.d[i] ?? 160;
    stepTimer.current = window.setTimeout(() => {
      frameRef.current = (i + 1) % def.d.length;
      setFrame(frameRef.current);
      stepLoop();
    }, ms);
  };

  const applyAnim = (key: PetAnimKey) => {
    if (animRef.current === key) return;
    animRef.current = key;
    setAnim(key);
    cancelStep();
    // 离开 look（更高优先级状态抢占）时完整停掉扫视：interval + 状态位 + 帧号复位（Task 8 评审遗留）
    if (animRef.current !== "look") stopLook();
    frameRef.current = 0;
    setFrame(0);
    stepLoop();
  };

  const refreshAnim = () => {
    const s = stateRef.current;
    applyAnim(s.drag ?? s.transient ?? s.task ?? (s.look ? "look" : "idle"));
  };

  /** 瞬时动作（代数计数防过期覆盖，spec F4） */
  const playTransient = useRef((key: PetAnimKey, ms: number) => {
    const gen = ++genRef.current.transient;
    stateRef.current.transient = key;
    refreshAnim();
    later(() => {
      if (genRef.current.transient === gen && stateRef.current.transient === key) {
        stateRef.current.transient = null;
        refreshAnim();
      }
    }, ms);
  }).current;

  // ---- 语音与字幕（Task 12 事件接线复用，spec §6.2）----
  const [subtitle, setSubtitle] = useState<string | null>(null);
  const bubbleGen = useRef(0);
  const voiceRef = useRef<VoicePlayer | null>(null);
  const unlockedRef = useRef(false);

  // 激活宠物解析：启动一次 + 监听热切换事件（失败回落 foxbell，spec §5.2/§12）。
  // StrictMode 双挂载：卸载时丢弃过期响应，保证重挂载后仍会重新拉取
  useEffect(() => {
    let disposed = false;
    const refresh = () => {
      const gen = ++refreshGen.current;
      resolveActivePet()
        .then((p) => {
          if (disposed || gen !== refreshGen.current) {
            p.dispose?.(); // 组件卸载或过期结果：回收其 blob 快照，防泄漏（P3-1）
            return;
          }
          applyActive(p);
        })
        .catch(() => {
          if (!disposed && gen === refreshGen.current) applyActive(DEFAULT_PET);
        });
    };
    refresh();
    let un: (() => void) | null = null;
    void listen("pet-active-changed", refresh).then((f) => {
      if (disposed) f();
      else un = f;
    });
    return () => {
      disposed = true;
      un?.();
    };
  }, []);

  // VoicePlayer 随激活宠物重建（外部宠物 = blob 快照 URL，EP6）
  useEffect(() => {
    const player = new VoicePlayer();
    player.load(active.voices, active.resolveVoiceUrl);
    voiceRef.current = player;
    if (unlockedRef.current) player.unlock();
    return () => {
      player.dispose();
      voiceRef.current = null;
    };
  }, [active]);

  const showBubble = (text: string, ms: number) => {
    const gen = ++bubbleGen.current;
    setSubtitle(text);
    later(() => {
      if (bubbleGen.current === gen) setSubtitle(null);
    }, ms);
  };

  /** 播一组语音 + 动作 + 字幕（muted 只拦声音不拦动作/字幕，spec D5）。
   *  桌宠关闭（窗口隐藏）时整条语音线不生效——隐藏窗口组件仍挂载、轮询照跑，
   *  不加闸门会在用户以为「关了桌宠」时继续出声（问题 6） */
  const playVoice = (group: VoiceGroup, action: PetAnimKey) => {
    if (!loadVisible()) return;
    playTransient(action, 1700); // 动作照播（spec §5.1 最低档）：置于语音闸门之前，无语音宠物也要有动作动画
    if (!activeRef.current.hasVoice) return; // 无语音宠物：不出声不出字幕（spec §5.2）
    const player = voiceRef.current;
    if (!player) return;
    const entry = player.pick(group);
    if (!entry) return; // 空组静默跳过（spec E5）
    // 字幕独立于声音闸门：talkative 即显示，最短 2.5s（spec D5 + E4）；声音由 muted 单独拦截
    if (cfgRef.current.talkative && activeRef.current.hasSubtitle)
      showBubble(entry.name, MIN_SPEECH_MS);
    player.play(entry, {
      muted: cfgRef.current.muted,
      onSubtitle: (name, ms) => {
        // 声音路径的字幕仅在非静音时生效，且时长与真实音频对齐（> 2.5s 时覆盖上面的兜底时长）
        if (
          !cfgRef.current.muted &&
          cfgRef.current.talkative &&
          activeRef.current.hasSubtitle &&
          ms > MIN_SPEECH_MS
        ) {
          showBubble(name, ms);
        }
      },
    });
  };
  // 渲染期禁止写 ref（react-hooks/refs）：改在每次渲染后的 effect 中同步，供 Task 12 事件接线调用
  const playVoiceRef = useRef<(g: VoiceGroup, a: PetAnimKey) => void>(() => {});
  useEffect(() => {
    playVoiceRef.current = playVoice;
  });

  // ---- 状态卡片（Task 10；spec §5/C1-C4）----
  const [cards, setCards] = useState<PetCard[]>([]);
  const [moreCount, setMoreCount] = useState(0);
  const [candidates, setCandidates] = useState<
    import("@/hooks/useSessionJump").JumpWindowCandidate[] | null
  >(null);
  // P2-4：跳转统一走 useSessionJump（与看板同一实现，含歧义候选返回与 via=app-fallback 的 M3 提示）
  const { focus: sessionJumpFocus, focusHwnd: sessionJumpFocusHwnd } = useSessionJump();
  const statusStateRef = useRef<PetStatusState | null>(null);
  const sessionIndexRef = useRef<Map<string, Session>>(new Map());
  const cardsWrapRef = useRef<HTMLDivElement | null>(null);
  const pendingAckRef = useRef(""); // 歧义跳转：点击卡片时先记待 ack 的会话 id（spec D12）
  // 稳定回调引用作依赖（与精灵 effect 同款），避免依赖整个 pet 对象每次渲染导致反复注销/登记
  useEffect(() => registerInteractive(cardsWrapRef.current), [registerInteractive]);
  const jumpCandidatesRef = useRef<HTMLDivElement | null>(null); // 候选浮层实测高度并入窗口几何（Task 11 评审遗留）

  // T1 跨窗口已读同步：看板点掉未读卡（X 关闭 / 跳转成功）后，后端广播 session-read，
  // 本窗口对状态机做与 ackDone 等价的已读置位（unread=false、绿卡 light=null）→
  // 卡片立即消隐，与看板一致。此前宠物感知不到别处的已读，头顶卡晚 ~60s 才消。
  // 组件内接线（宠物状态机是唯一消费者；unlisten 随组件卸载自动清理）
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let disposed = false;
    void setupSessionReadListener((_, sessionId) => {
      const state = statusStateRef.current;
      if (!state) return;
      ackDone(state, sessionId);
      setCards(cardsFromState(state));
    }).then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  // 候选浮层关闭路径（spec §11）：Esc/点外关闭，不 ack、卡片保留（与 PetMenu 的 B10 同款监听）。
  // 用 layout effect：提交阶段同步注册监听，浮层 DOM 可被观察之前必然已挂上
  // （useEffect 是提交后另排的被动任务，测试 findByTestId 观察到 DOM 即 pointerDown 会输给它，FIX-11）
  useLayoutEffect(() => {
    if (!candidates) return;
    const onDown = (e: PointerEvent) => {
      if (jumpCandidatesRef.current?.contains(e.target as Node)) return;
      setCandidates(null);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setCandidates(null);
    };
    window.addEventListener("pointerdown", onDown, true);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("pointerdown", onDown, true);
      window.removeEventListener("keydown", onKey);
    };
  }, [candidates]);

  const { data } = useSessionsQuery();
  const lastApprovalAtRef = useRef(0); // approval 语音 10s 限频窗口起点（spec D3）
  const previewLoopRef = useRef<number | null>(null); // 动作绑定子页预览循环句柄（spec B4）

  /** 停止动作预览循环（幂等：无循环时 no-op）。仅读写 ref，依赖为空（Fix 3：稳定身份供菜单回调复用） */
  const stopPreview = useCallback(() => {
    if (previewLoopRef.current !== null) {
      window.clearInterval(previewLoopRef.current);
      previewLoopRef.current = null;
    }
  }, []);

  /** 显隐广播给主窗口/托盘（PetPage 已监听回写 localStorage；主窗口入口 Task 13 统一监听） */
  const emitPetVisibility = useCallback((visible: boolean) => {
    emit("pet-visibility-changed", { visible }).catch(() => {});
  }, []);

  useEffect(() => {
    if (!data) return;
    sessionIndexRef.current = new Map(data.sessions.map((s) => [s.id, s]));
    const r = computePetStatus(data.sessions, statusStateRef.current, Date.now());
    statusStateRef.current = r.state;
    setCards(r.cards);
    setMoreCount(r.moreCount);

    // ---- 事件差分 → 语音（spec §5/§6.2）----
    if (r.events.newCompletion.length > 0) {
      playVoiceRef.current("done", cfgRef.current.doneAction);
    }
    if (r.events.newWaiting.length > 0 && Date.now() - lastApprovalAtRef.current > 10_000) {
      lastApprovalAtRef.current = Date.now();
      playVoiceRef.current("approval", cfgRef.current.approvalAction);
    }

    // ---- 任务姿态：waiting > running；全绿（全部完成）或无卡回落 idle/look 站立 ----
    // 实测反馈 1：卡片全绿即任务都已做完，无需保持忙碌姿态，与无卡状态一致站立；
    // review 动画保留给菜单动作绑定（PET_ACTIONS），不再用作任务姿态
    const anyWaiting = r.cards.some((c) => c.light === "waiting");
    const anyRunning = r.cards.some((c) => c.light === "running");
    stateRef.current.task = anyWaiting ? "waiting" : anyRunning ? "running" : null;
    refreshAnim(); // 组件内稳定闭包（仅读写 ref + setState），勿入依赖
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [data]);

  /** 点击卡片跳转终端；歧义时弹候选浮层；失败静默保留卡片（spec C2/§13）。
   *  P2-4：统一走 useSessionJump（与看板一致）——含歧义候选返回、以及 CLI 会话
   *  TTY 聚焦失败走 APP 级保底时的 M3 提示（via=app-fallback） */
  const jump = async (card: PetCard) => {
    const s = sessionIndexRef.current.get(card.id);
    if (!s) return;
    pendingAckRef.current = card.id; // 候选选中后按此 id ack
    try {
      const ambiguous = await sessionJumpFocus({
        pid: s.pid,
        id: s.id,
        agentType: s.agentType,
        projectName: s.projectName,
        lastMessage: s.lastMessage ?? undefined,
        title: sessionTitleOrUndefined(s),
        unread: s.unread, // 歧义点选回标已读（spec W4 已读信号 1）
        form: s.form, // M3：CLI 会话 APP 级保底时的 UX 提示依据
      });
      if (ambiguous && ambiguous.length) {
        setCandidates(ambiguous); // 歧义候选浮层（spec D12）
        return;
      }
    } catch {
      // 跳转失败也清除气泡（spec W1）：气泡是瞬时提醒，不因跳转失败卡死；
      // 看板上的未读状态由 W4 已读机制独立管理，不在此处丢
    }
    ackDone(statusStateRef.current ?? {}, card.id); // 点击已读即消（spec C2）
    setCards(cardsFromState(statusStateRef.current ?? {}));
  };

  // ---- look 环顾：空闲 6s 触发，16 向 250ms/帧，任何状态打断（spec F2）----
  const stopLook = () => {
    lookStop.current?.();
    lookStop.current = null;
    if (stateRef.current.look) {
      stateRef.current.look = false;
      setLookFrame(-1);
    }
  };
  const scheduleNextLook = () => {
    const gen = ++genRef.current.look;
    later(() => {
      if (genRef.current.look !== gen) return;
      if (rowsRef.current !== 11) {
        scheduleNextLook(); // v1 无环视行：静默续期（EP1）
        return;
      }
      const s = stateRef.current;
      if (!s.drag && !s.transient && !s.task) {
        s.look = true;
        setLookFrame(0);
        refreshAnim();
        let i = 0;
        const id = window.setInterval(() => {
          i += 1;
          if (i >= 16) {
            window.clearInterval(id);
            lookStop.current = null;
            stopLook();
            refreshAnim();
            scheduleNextLook();
          } else {
            setLookFrame(i);
          }
        }, 250);
        lookStop.current = () => window.clearInterval(id);
      } else {
        scheduleNextLook();
      }
    }, 6000);
  };

  /** 使在途的 look 调度链失效（卸载清理用）：链式续期回调按代数自检后全部 no-op（Task 8 评审遗留） */
  const invalidateLookChain = () => {
    genRef.current.look += 1;
  };

  useEffect(() => {
    stepLoop();
    scheduleNextLook();
    return () => {
      cancelStep();
      stopLook();
      invalidateLookChain();
      stopPreview(); // 预览循环 interval 卸载清理（Fix 1 评审随附：previewLoopRef 泄漏）
      // 浮窗三个在途定时器一并收口（修复轮 1 评审 Minor）：卸载时最多残留 0.5s 的定时器，
      // 虽不会重复悬停泄漏，但按本文件「定时器在卸载清理里收口」的约定补齐
      clearMiniHover();
      clearMiniGrace();
      clearMiniRestore();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // ---- 指针交互（spec A1/A2/A3/§8）----
  const lastDeltaRef = useRef({ dx: 0, dy: 0 });
  // 拖动中握拳、悬停张开手掌（问题 2 实测反馈：grab/grabbing 仅是系统手型，不足以区分按住态）
  const [dragging, setDragging] = useState(false);

  const onPointerDown = (e: React.PointerEvent) => {
    // 浮窗与指针的交汇（D13）：任何按下都先停掉在途的悬停 / 宽限 / 恢复定时器，避免层叠竞态
    clearMiniHover();
    clearMiniGrace();
    clearMiniRestore();
    if (e.button !== 0) return; // 右键留给菜单（spec A6）：浮窗由菜单守卫压住，模式不动
    // 左键按下 = 拖拽起点（或单击）：记下模式并隐藏浮窗，松手 MINI_RESTORE_MS 后再恢复
    miniBeforeDragRef.current = miniMode;
    setMiniMode(null);
    stopLook();
    if (!unlockedRef.current) {
      unlockedRef.current = true;
      voiceRef.current?.unlock(); // 手势内解锁自动播放（spec E6）
    }
    lastDeltaRef.current = { dx: 0, dy: 0 }; // 重置采样增量，避免上一次拖拽残留误判为「移动过」
    setDragging(true);
    pet.beginDrag(e);
  };

  const onPointerMove = (e: React.PointerEvent) => {
    const r = pet.trackDrag(e);
    if (!r) return;
    // 铆钉式：直接以按下时的窗口位置 + 屏幕位移定位（trackDrag 返回的 dx/dy 即屏幕绝对增量），
    // 不再逐帧读窗口几何（旧实现每帧 3 次 IPC + moveBy 累加，既延迟又受 setPosition 落地竞态影响）
    pet.dragTo(r.dx, r.dy);
    // 方向动画按 150ms 采样窗增量判定（原版逐帧增量语义，spec A3 阈值同原版）
    const dir: PetAnimKey | null =
      r.movedY < -8 ? "jumping" : r.movedX < -6 ? "run-left" : r.movedX > 6 ? "run-right" : null;
    const s = stateRef.current;
    if (dir) s.drag = dir;
    refreshAnim();
  };

  const onPointerUp = (_e: React.PointerEvent) => {
    const moved = lastDeltaRef.current.dx !== 0 || lastDeltaRef.current.dy !== 0;
    stateRef.current.drag = null;
    setDragging(false);
    if (!moved) playTransient("waving", 1700); // 单击：固定挥手（spec A1）
    refreshAnim();
    pet.releaseDrag({
      gravity: cfgRef.current.gravity,
      onLand: () => {
        // 落地压扁回弹 + 补跳（spec §8）；transform 追加/移除 scaleY，基础 translateX(-50%) 不受影响
        const el = spriteRef.current;
        if (!el) return;
        el.style.transition = "transform 60ms ease-out";
        el.style.transform += " scaleY(0.55)";
        later(() => {
          el.style.transition = "transform 240ms cubic-bezier(.34,1.56,.64,1)";
          el.style.transform = el.style.transform.replace(" scaleY(0.55)", "");
          later(() => {
            el.style.transition = "";
            playTransient("jumping", 1500);
          }, 260);
        }, 60);
      },
    });
    lastDeltaRef.current = { dx: 0, dy: 0 };
    // 松手 MINI_RESTORE_MS 后恢复浮窗（D13）：恢复的是**拖拽前那个模式**（hover 悬停 / manual 唤回）；
    // 拖拽前没有浮窗（null）则什么都不做。到点前先与指针对账：指针已不在精灵/浮窗上 ⇒ **丢弃这次恢复**
    // （否则 hover 层会挂出一个没有关闭通道的常驻浮窗，见 `pointerOnPetRef` 的注释）
    const prevMode = miniBeforeDragRef.current;
    miniBeforeDragRef.current = null;
    if (prevMode !== null) {
      clearMiniRestore();
      miniRestoreTimerRef.current = later(() => {
        miniRestoreTimerRef.current = null;
        if (!pointerOnPetRef.current) return; // 对账不过 ⇒ 不恢复
        setMiniMode(prevMode);
      }, MINI_RESTORE_MS);
    }
  };

  /** 精灵悬停：MINI_HOVER_MS 后出浮窗（manual 模式不重设定时器）；回到精灵即取消宽限 */
  const onSpritePointerEnter = () => {
    pointerOnPetRef.current = true; // 指针回到宠物身上（悬停层揭幕的对账依据）
    clearMiniGrace();
    if (miniMode === "manual") return;
    clearMiniHover();
    miniHoverTimerRef.current = later(() => {
      miniHoverTimerRef.current = null;
      setMiniMode("hover");
    }, MINI_HOVER_MS);
  };

  /**
   * 精灵移开：停掉未到点的悬停定时器，**并取消在途的「松手恢复」**（修复轮 1 评审 Important）：
   * 单击/拖拽松手后排的那次恢复，若指针随后离开精灵，到点就会把浮窗挂出来——而 hover 层没有任何
   * 关闭通道。离开即作废，恢复必须重新由悬停挣来（真拖拽时指针一直压在精灵上，不受影响）。
   */
  const onSpritePointerLeave = () => {
    pointerOnPetRef.current = false;
    clearMiniHover();
    clearMiniRestore();
    startMiniGrace();
  };

  /** 指针进入浮窗（或其内的「详情 »」）：算「在宠物身上」，并取消宽限（MINI_GRACE_MS，2026-10-07 起 2s） */
  const onMiniWrapPointerEnter = () => {
    pointerOnPetRef.current = true;
    clearMiniGrace();
  };

  /** 指针离开浮窗：算「离开宠物」——回到精灵会由精灵的 enter 重新置位；hover 层重新起宽限 */
  const onMiniWrapPointerLeave = () => {
    pointerOnPetRef.current = false;
    startMiniGrace();
  };

  const onDoubleClick = (e: React.MouseEvent) => {
    e.stopPropagation();
    playVoice("general", cfgRef.current.dblAction); // 双击说话（spec A2）
  };

  // ---- 窗口几何同步（spec §4.2）：宽度恒 340×scale；高度 = 气泡区 + 精灵 + 间隙 + max(卡片区, 菜单, 候选浮层) ----
  // Task 12 引入 PetMenu（挂 menuWrapRef）；候选浮层挂 jumpCandidatesRef 一并实测并入 Math.max（Task 11 评审遗留）
  const menuWrapRef = useRef<HTMLDivElement | null>(null);
  // 右键菜单位置（spec A6）：x=夹紧后的光标横坐标；lift=光标高于精灵底边的距离（菜单向上展开的锚距）
  const [menu, setMenu] = useState<{ x: number; lift: number } | null>(null);

  // ---- 浮窗迷你条（计划② Task 13；spec D12/D13/D20）----
  // 两个触发方式（精灵悬停 / 右键菜单「🏷 今日用量」）**共用同一个容器**。渲染守卫 `menu === null`
  // 保证菜单打开时浮窗不渲染（层序：菜单在上；syncSize 那一支因此传 miniH = 0）。
  const [miniMode, setMiniMode] = useState<"hover" | "manual" | null>(null);
  // 浮窗定位基准：卡片区**实测**高度（jsdom 恒 0）。浮窗贴在卡片上方 ⇒ 卡片增减后必须重算 bottom。
  const [cardsHeight, setCardsHeight] = useState(0);
  /**
   * 诊断（H1 的判据 + 行为中性守卫）：上一次**真正写进** `cardsHeight` 的量测值，以及亚像素
   * 抖动计数。布局 effect 里的 `setCardsHeight(cardsH)` 用的是 `getBoundingClientRect().height`
   * 的**浮点**值，而 `cardsHeight` **不在那个 effect 的依赖数组里** ⇒ 抖动一次就 setState 一次、
   * 进而再量测一次（「宠物不显示」事故的嫌疑假设 H1）。React 对**相同值**本来就 bail out
   * ⇒ 「值没变就不 set」不改变任何行为，只是把「其实什么都没发生」这句话落实，并把抖动留成证据。
   */
  const cardsHeightRef = useRef<number | null>(null);
  const cardsJitterRef = useRef<{ count: number; since: number }>({ count: 0, since: 0 });
  const miniWrapRef = useRef<HTMLDivElement | null>(null);
  const miniHoverTimerRef = useRef<(() => void) | null>(null); // 悬停 MINI_HOVER_MS 出浮窗
  const miniGraceTimerRef = useRef<(() => void) | null>(null); // 移开 MINI_GRACE_MS 宽限后消失
  const miniRestoreTimerRef = useRef<(() => void) | null>(null); // 松手 MINI_RESTORE_MS 后恢复
  const miniBeforeDragRef = useRef<"hover" | "manual" | null>(null); // 拖拽前的模式（松手后恢复它）
  // 「指针此刻在不在宠物身上」（精灵或浮窗，含其中的「详情 »」）：由 enter/leave 维护。
  // 悬停层的**每一次揭幕都要过这一关**（恢复定时器到点、菜单关闭守卫解除）——否则会出现
  // 「浮窗在指针早已移开时冒出来，而 hover 层没有任何关闭通道（ESC/点外只注册于 manual）」
  // 的常驻浮窗（修复轮 1 评审 Important）。
  const pointerOnPetRef = useRef(false);

  /** 三个浮窗定时器的取消口（幂等：空位 no-op）。`later` 返回的就是取消函数本身 */
  const clearMiniHover = () => {
    const cancel = miniHoverTimerRef.current;
    miniHoverTimerRef.current = null;
    cancel?.();
  };
  const clearMiniGrace = () => {
    const cancel = miniGraceTimerRef.current;
    miniGraceTimerRef.current = null;
    cancel?.();
  };
  const clearMiniRestore = () => {
    const cancel = miniRestoreTimerRef.current;
    miniRestoreTimerRef.current = null;
    cancel?.();
  };

  /**
   * hover 模式起一次宽限卸载（D13）：精灵移开、指针从浮窗移开都走这里；
   * 指针回到精灵**或**进入浮窗都会取消它（宽限的全部意义就是够鼠标移到「详情 »」上）。
   * manual 模式不受影响：它由菜单唤回，只能被 ESC / 点外部 / 拖拽收掉。
   */
  const startMiniGrace = () => {
    if (miniMode !== "hover") return;
    clearMiniGrace();
    miniGraceTimerRef.current = later(() => {
      miniGraceTimerRef.current = null;
      setMiniMode(null);
    }, MINI_GRACE_MS);
  };

  /**
   * 菜单是 hover 层的「遮罩」：菜单一关（`menu → null`）就与指针对一次账（修复轮 1 评审）。
   * 右键唤菜单时 hover 模式**不动**（指针若仍在精灵上，菜单关掉后浮窗回来是对的）；但指针若早已
   * 离开，hover 浮窗不得随守卫解除而「立刻冒出来」——悬停层的出现必须真的由悬停挣来。
   * 只在 `miniMode`/`menu` 变化时跑：正常悬停路径不受影响（那时指针必在宠物身上）。
   */
  useEffect(() => {
    if (menu !== null || miniMode !== "hover") return;
    if (!pointerOnPetRef.current) setMiniMode(null);
  }, [menu, miniMode]);

  // 手动模式（D12）：点浮窗外部 / ESC 关闭。**ESC 逐层**（D13）：菜单开着时本监听**根本不注册**
  // ——那一次 ESC 只会落到菜单自己那只监听上，一次只关一层（关键坑第 4 条）。
  // 与候选浮层同款用 layout effect：提交阶段同步注册，浮窗 DOM 可被观察到之前监听必然已在位。
  useLayoutEffect(() => {
    if (miniMode !== "manual" || menu !== null) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setMiniMode(null);
    };
    const onDown = (e: PointerEvent) => {
      const target = e.target as Node;
      // 浮窗内（「详情 »」）与宠物本体上的按下都不算「点外部」：后者是拖拽 / 右键唤菜单的起点
      if (miniWrapRef.current?.contains(target)) return;
      if (spriteRef.current?.contains(target)) return;
      setMiniMode(null);
    };
    window.addEventListener("keydown", onKey);
    window.addEventListener("pointerdown", onDown, true);
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("pointerdown", onDown, true);
    };
  }, [miniMode, menu]);

  useLayoutEffect(() => {
    const menuH = menuWrapRef.current?.getBoundingClientRect().height ?? 0;
    const cardsH = cardsWrapRef.current?.getBoundingClientRect().height ?? 0;
    const candidatesH = jumpCandidatesRef.current?.getBoundingClientRect().height ?? 0;
    // 浮窗定位基准（jsdom 恒 0）+ 诊断：只有**真的变了**才 setState（React 对相同值本就 bail out，
    // 故这不是行为变更），并把亚像素抖动记下来 —— 抖动即自激的燃料（见 `cardsHeightRef` 的注释）。
    const prevCardsH = cardsHeightRef.current;
    if (prevCardsH === null || cardsH !== prevCardsH) {
      const delta = prevCardsH === null ? 0 : cardsH - prevCardsH;
      if (prevCardsH !== null && delta > 0 && delta < 0.5) {
        const j = cardsJitterRef.current;
        const now = performance.now();
        if (now - j.since > 2000) cardsJitterRef.current = { count: 1, since: now };
        else j.count += 1;
        console.debug("[pet] cardsHeight 亚像素抖动", { from: prevCardsH, to: cardsH, delta });
        if (cardsJitterRef.current.count === 9) {
          console.warn("[pet] cardsHeight 在 2s 内抖动 9 次 —— 迷你条布局 effect 可能自激（H1）");
        }
      }
      cardsHeightRef.current = cardsH;
      setCardsHeight(cardsH);
    }
    // 浮窗高度：菜单打开时浮窗被渲染守卫隐藏 ⇒ miniH = 0（否则菜单与浮窗的高度会被一起算进去）
    const miniH = miniMode !== null && menu === null ? miniBarHeight(cfg.scale) : 0;
    // 高度 = base + (浮窗显示 ? 卡片 + 浮窗 : max(卡片, 菜单, 候选)) —— D20 核心修订：**求和**
    void pet.syncSize(
      px(340),
      windowHeightSum({ scale: cfg.scale, cardsH, menuH, candidatesH, miniH })
    );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [cfg.scale, menu, cards, moreCount, candidates, miniMode, pet.syncSize]);

  const px = (v: number) => Math.round(v * cfg.scale);
  const style = frameStyle(anim, frame, lookFrame, cfg.scale, active.rows);

  // ---- 菜单回调（Fix 3：useCallback 稳定身份，避免 PetMenu 预览 effect 每次渲染重触发 → interval 抖动）----
  /** 关闭菜单（外点/Esc/关于行）：停预览 + 恢复穿透 */
  const handleMenuClose = useCallback(() => {
    setMenu(null);
    setMenuOpen(false);
    stopPreview();
    // `setMenu` 进依赖数组：Task 13 引入浮窗后 React Compiler 的依赖推断把这条 setter 也算作
    // 本回调的依赖（`pnpm lint` 的 preserve-manual-memoization 报错逐字点名它）。setter 身份恒稳定，
    // 写进数组**不改变**回调的重建时机（行为与原先完全一致），只是让编译器对得上账。
  }, [setMenu, setMenuOpen, stopPreview]);

  /** 动作子页预览循环（spec B4）：进入子页立即播一次，~1700ms 循环；返回主菜单（null）即停 */
  const handleMenuPreview = useCallback(
    (action: PetAction | null) => {
      if (action) {
        stopPreview();
        const loop = () => {
          playTransient(action, 1600);
        };
        loop(); // 进入子页立即预览一次
        previewLoopRef.current = window.setInterval(loop, 1700); // 子页循环预览（spec B4）
      } else {
        stopPreview();
      }
    },
    [playTransient, stopPreview]
  );

  /** 打开用量大看板（计划② Task 6 步骤 11）：菜单项自身先 onClose，这里经 openUsageDashboard
   *  请求主窗切「模型用量」页签（四页同窗后不再建独立窗口；title/theme 形参兼容保留，已无效果） */
  const handleOpenDashboard = useCallback(() => {
    void openUsageDashboard({
      title: t("usage.title"),
      theme: theme === "system" ? undefined : theme,
    });
  }, [t, theme]);

  /** 右键菜单「🏷 今日用量」：手动唤回浮窗（D12）。菜单项自身已先 onClose，这里**只置模式** */
  const handleMiniUsage = useCallback(() => {
    setMiniMode("manual");
  }, []);

  /** 「详情 »」钻取（spec P2 第 5 条 / D12）：直达大看板**并关闭浮窗**——复用菜单入口的同一个开窗函数 */
  const handleMiniDetail = useCallback(() => {
    setMiniMode(null);
    handleOpenDashboard();
  }, [handleOpenDashboard]);

  /** 隐藏桌宠（spec §10/§10.2）：关菜单 + 本地持久化 + invoke + 广播 */
  const handleMenuHide = useCallback(() => {
    setMenu(null);
    setMenuOpen(false);
    stopPreview();
    // 隐藏即收掉浮窗（修复轮 1 评审）：三个在途定时器停掉 + 模式归零，否则「隐藏 → 再显示」
    // 浮窗还挂着（显示路径不会重置它）。直接操作 ref（不调上面的 helper）：那些函数每次渲染新建，
    // 进了依赖数组会打断 Fix 3 的稳定身份。
    for (const ref of [miniHoverTimerRef, miniGraceTimerRef, miniRestoreTimerRef]) {
      ref.current?.();
      ref.current = null;
    }
    setMiniMode(null);
    saveVisible(false); // 本地状态 + 订阅同步（spec §10）
    invoke("set_pet_visible", { visible: false }).catch(() => {});
    emitPetVisibility(false); // 广播给主窗口/托盘同步（spec §10.2）
    // 同 `handleMenuClose`：`setMenu` 是编译器点名的依赖（setter 稳定 ⇒ 行为不变）
  }, [emitPetVisibility, setMenu, setMenuOpen, stopPreview]);

  return (
    <div ref={contentRef} style={{ position: "fixed", inset: 0, overflow: "visible" }}>
      <div
        ref={spriteRef}
        data-testid="pet-sprite"
        // 悬停张开手掌 / 按住拖动握拳（pet-cursor.css，问题 1 附带需求）
        className={dragging ? "pet-cursor-fist" : "pet-cursor-hand"}
        style={{
          position: "absolute",
          left: "50%",
          transform: "translateX(-50%)",
          bottom: px(50), // 底部气泡区（spec §4.2）
          width: px(FRAME_W),
          height: px(FRAME_H),
          backgroundImage: `url(${active.spritesheetUrl})`,
          backgroundPosition: style.backgroundPosition,
          backgroundSize: style.backgroundSize,
          backgroundRepeat: "no-repeat",
          touchAction: "none",
          userSelect: "none",
        }}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerCancel={onPointerUp}
        onDoubleClick={onDoubleClick}
        // 浮窗悬停时序（D13）：进入即（重）起 500ms 悬停定时器 + 取消宽限；移开则停悬停、起 MINI_GRACE_MS（2026-10-07 起 2s）宽限
        onPointerEnter={onSpritePointerEnter}
        onPointerLeave={onSpritePointerLeave}
        onContextMenu={(e) => {
          e.preventDefault(); // 屏蔽系统菜单（spec A6）
          // Fix 1（向上展开 + A6 夹紧）：记录光标相对精灵底边的高度 lift——精灵 bottom 锚定，
          // 窗口向上生长后 lift 不变，菜单 BOTTOM = px(50)+lift 恒贴光标；x 左移夹紧保证菜单不出窗
          const spriteBottom = spriteRef.current?.getBoundingClientRect().bottom ?? 0;
          setMenu({
            x: Math.min(e.clientX, px(340) - 180), // minWidth≈170，窗宽 px(340)
            lift: Math.max(0, spriteBottom - e.clientY),
          });
          setMenuOpen(true); // 菜单打开期间关闭穿透
        }}
      />
      {/* 浮窗迷你条（计划② Task 13；spec D12/D13/D20）：两个触发方式**共用这一个挂载点**。
          渲染守卫 `menu === null`：菜单打开时浮窗隐藏（层序：菜单在上 ⇒ syncSize 那一支传 miniH = 0）；
          拖拽期间由 onPointerDown 直接置 null（松手 MINI_RESTORE_MS 后恢复原模式）。
          ⚠️ **吃点击面积如实记录**：宠物窗口**整窗常驻交互**（`setIgnoring(true)` 全仓零调用点、
          `ignoringRef` 恒 false，`registerInteractive` / `hitTest` 是活代码），且 CSS
          `pointer-events: none` **无法**穿透到别的 OS 窗口 ⇒ 挂上浮窗会让吃点击面积增加约
          `340 × Δh`（逻辑 px × scale，Δh = 浮窗高度）。这是本方案**接受**的代价：容器只是不抢
          宠物窗口**内部**的指针事件，透明像素与浮窗区域照样吃 OS 点击，不假装可穿透。 */}
      {miniMode !== null && menu === null && (
        <div
          ref={miniWrapRef}
          data-testid="pet-mini-wrap"
          // 指针进/出浮窗都维护「在不在宠物身上」的对账位（悬停层揭幕要用），并处理宽限（MINI_GRACE_MS）
          onPointerEnter={onMiniWrapPointerEnter}
          onPointerLeave={onMiniWrapPointerLeave}
          style={{
            position: "absolute",
            bottom: px(50 + FRAME_H + 10) + cardsHeight + miniBarGap(cfg.scale), // 卡片区之上 6px（同 scale）
            left: "50%",
            transform: "translateX(-50%)",
            width: px(320), // 与状态卡片同宽；窗口宽度 px(340) 不动（spec P2「不加宽窗口」）
            zIndex: 4,
          }}
        >
          <UsageMiniBar scale={cfg.scale} mode={miniMode} onDetail={handleMiniDetail} t={t} />
        </div>
      )}
      {subtitle && (
        <div
          data-testid="pet-bubble"
          style={{
            position: "absolute",
            left: "50%",
            transform: "translateX(-50%)",
            bottom: 8,
            maxWidth: px(320),
            padding: `${px(6)}px ${px(12)}px`,
            fontSize: px(13),
            lineHeight: 1.4,
            whiteSpace: "nowrap",
            overflow: "hidden",
            textOverflow: "ellipsis",
            borderRadius: px(12),
            pointerEvents: "none",
            background: "rgba(255,255,255,0.96)",
            color: "#7a4a2b",
            border: "1px solid rgba(122,74,43,0.35)",
            boxShadow: "0 2px 10px rgba(0,0,0,0.18)",
            zIndex: 2,
          }}
        >
          {subtitle}
        </div>
      )}
      {/* 状态卡片区（spec §5）：精灵上方居中，点击跳转终端 */}
      <div
        ref={cardsWrapRef}
        data-testid="pet-cards"
        style={{
          position: "absolute",
          bottom: px(50 + FRAME_H + 10),
          left: "50%",
          transform: "translateX(-50%)",
          display: "flex",
          flexDirection: "column",
          alignItems: "center",
          gap: px(5),
          width: px(320),
          zIndex: 3,
        }}
      >
        {cards.map((c) => (
          <div
            key={c.id}
            data-testid={`pet-card-${c.id}`}
            onClick={(e) => {
              e.stopPropagation();
              void jump(c);
            }}
            style={{
              display: "flex",
              alignItems: "flex-start",
              gap: px(7),
              width: "100%",
              boxSizing: "border-box",
              padding: `${px(5)}px ${px(10)}px`,
              borderRadius: px(10),
              cursor: "pointer",
              fontSize: px(12),
              lineHeight: 1.45,
              background: "rgba(255,252,248,0.97)",
              border: "1px solid rgba(122,74,43,0.3)",
              boxShadow: "0 2px 8px rgba(0,0,0,0.14)",
            }}
          >
            <span
              style={{
                width: px(8),
                height: px(8),
                borderRadius: "50%",
                flex: "none",
                marginTop: px(4),
                background:
                  c.light === "waiting" ? "#ef4444" : c.light === "running" ? "#eab308" : "#22c55e",
                boxShadow: `0 0 0 2px ${c.light === "waiting" ? "rgba(239,68,68,.25)" : c.light === "running" ? "rgba(234,179,8,.25)" : "rgba(34,197,94,.25)"}`,
              }}
            />
            <div style={{ minWidth: 0, flex: 1 }}>
              <div
                style={{
                  fontWeight: 700,
                  color: "#7a4a2b",
                  overflow: "hidden",
                  textOverflow: "ellipsis",
                  whiteSpace: "nowrap",
                }}
              >
                {c.title}
              </div>
              {c.lines.map((l, i) => (
                <div
                  key={i}
                  style={{
                    color: "#a07050",
                    overflow: "hidden",
                    textOverflow: "ellipsis",
                    whiteSpace: "nowrap",
                  }}
                >
                  {l}
                </div>
              ))}
            </div>
            {/* T2：宠物卡 X——与看板同语义。未读绿卡走已读，其余走 dismiss
               （status 取 sessionIndexRef 的精确状态传后端）；点击不触发跳转 */}
            <span
              onClick={(e) => {
                e.stopPropagation();
                const s = sessionIndexRef.current.get(c.id);
                if (!s) return;
                if (c.light === "done" && c.unread) {
                  invoke("mark_session_read", {
                    agentType: s.agentType,
                    sessionId: s.id,
                  }).catch(() => {});
                } else {
                  invoke("dismiss_session_card", {
                    agentType: s.agentType,
                    sessionId: s.id,
                    status: s.status,
                  }).catch(() => {});
                }
                // 本地立即消卡（后端 dismiss 在下一轮扫描过滤生效，payload 随之不含该卡）
                const state = statusStateRef.current;
                if (state?.[c.id]) {
                  state[c.id].light = null;
                  setCards(cardsFromState(state));
                }
              }}
              style={{
                flex: "none",
                cursor: "pointer",
                color: "#a07050",
                padding: px(2),
                lineHeight: 1,
                borderRadius: px(4),
              }}
              title={t("sessions.dismissCard")}
            >
              ✕
            </span>
          </div>
        ))}
        {moreCount > 0 && (
          <div
            style={{
              color: "#a07050",
              fontSize: px(11),
              background: "rgba(255,252,248,0.9)",
              borderRadius: 999,
              padding: `${px(2)}px ${px(8)}px`,
            }}
          >
            +{moreCount} {t("pet.card.more")}
          </div>
        )}
      </div>
      {/* 歧义候选浮层（spec D12）：选中按 hwnd 聚焦并 ack 发起跳转的卡片 */}
      {candidates && (
        <div
          ref={jumpCandidatesRef}
          data-testid="pet-jump-candidates"
          style={{
            position: "absolute",
            bottom: px(50 + FRAME_H + 10),
            left: "50%",
            transform: "translateX(-50%)",
            width: px(320),
            maxHeight: px(240),
            overflowY: "auto",
            zIndex: 5,
            background: "rgba(30,30,34,0.96)",
            color: "#eee",
            borderRadius: px(10),
            fontSize: px(12),
            padding: `${px(4)}px 0`,
          }}
        >
          {candidates.map((w) => (
            <div
              key={w.hwnd}
              onClick={() => {
                // 歧义点选：经 useSessionJump.focusHwnd 聚焦并回标已读（spec W4 已读信号 1，
                // 与看板歧义分支一致）；随后 pet 自己的卡片 ack 消气泡
                void sessionJumpFocusHwnd(w.hwnd)
                  .then(() => {
                    ackDone(statusStateRef.current ?? {}, pendingAckRef.current);
                    setCards(cardsFromState(statusStateRef.current ?? {}));
                  })
                  .catch((e) => {
                    // P2-7（issue #34）：点选聚焦失败（窗口可能已关）——不产生
                    // unhandled rejection、不 ack（卡保留可重试）；候选层已无条件
                    // 关闭（spec W1 无论成败清除）。提示与看板同款文案
                    console.error("sessionJumpFocusHwnd failed:", e);
                    toast.error(t("sessions.jumpFailed", { error: e }));
                  });
                setCandidates(null);
              }}
              style={{ padding: `${px(3)}px ${px(14)}px`, cursor: "pointer" }}
            >
              {w.title}
              <span style={{ color: "#a1a1aa" }}> · {w.process}</span>
            </div>
          ))}
        </div>
      )}
      {/* 右键菜单（spec §9/B10）：Fix 1 — 包裹层 absolute 参与布局，menuWrapRef 才能实测到真实高度；
          菜单 BOTTOM 锚在光标处向上展开，窗口底部锚定向上生长后整份菜单可见（spec §4.2） */}
      {menu && (
        <div
          ref={menuWrapRef}
          style={{ position: "absolute", left: menu.x, bottom: px(50) + menu.lift, zIndex: 10 }}
        >
          <PetMenu
            onClose={handleMenuClose}
            onPreview={handleMenuPreview}
            onHide={handleMenuHide}
            onOpenDashboard={handleOpenDashboard}
            onMiniUsage={handleMiniUsage}
            voiceCapable={active.hasVoice}
            subtitleCapable={active.hasSubtitle}
          />
        </div>
      )}
    </div>
  );
}
