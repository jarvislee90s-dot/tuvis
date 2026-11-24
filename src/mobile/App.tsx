import { useCallback, useState } from "react";
import { Toaster } from "sonner";
import ArchiveBoard from "./ArchiveBoard";
import ArchiveDetail from "./ArchiveDetail";
import Board from "./Board";
import PairPage from "./PairPage";
import SessionDetail from "./SessionDetail";
import type { ArchivedSession } from "./api";
import type { Session } from "@/types/session";

/** Session 字段全为原始值（string|number|boolean|null）→ 逐字段浅比较即全等。
 *  T1 活状态流：用于「活会话与本轮数据无变化时复用旧引用」，避免每拍数据都换
 *  selected 对象身份引起详情页无谓重渲染 */
function shallowEqualSession(a: Session, b: Session): boolean {
  const keys = Object.keys(a) as (keyof Session)[];
  if (keys.length !== Object.keys(b).length) return false;
  return keys.every((k) => a[k] === b[k]);
}

// 配对状态机（轮询全部由 Board 自持，App 只持状态标记）：
// - null  探测中：首帧即出配对页（沿用 Task 5 不闪白口径），Board 在底下挂载完成首次探测；
//         Board 首拍拉到数据后回调 onPaired（Board 内 ref 保证只发一次）翻转为 true，配对页随即卸载
//         —— 已配对设备（cookie 有效）刷新页面 / 重开 PWA 即由此免重配
// - true  已配对：Board 与详情页二选一显示（M3 Task 8 三态：board / detail；
//         文件预览是 SessionDetail 内部状态，不进 App 路由）
// - false 设备失效：Board 收 403 回调后置 false 并卸载（停轮询），回配对页；
//         重新配对成功 → true → Board 重新挂载恢复轮询
// 注意：网络异常不走 403 通道，Board 内部保数据重试，不会误置 false
export default function App() {
  const [paired, setPaired] = useState<boolean | null>(null);
  // 当前查看的会话（board → detail 的唯一路由状态）：null = 看板。
  // T1 活状态流：进入详情时定格的快照经 handleSessionsChanged 按 (agentType,id)
  // 对齐到 Board 既有轮询数据（SSE 跃迁/快照 + 降级 3s 轮询）——停留详情期间
  // status 自动更新（红卡/总结横幅自动切换），无需重进页面
  const [selected, setSelected] = useState<Session | null>(null);
  // 历史会话区（spec §7.1）：historyOpen = 历史页开关；archiveSelected = 当前查看的
  // 归档会话（历史页 → 归档详情）。激活回执（onActivated）一次清两级——乐观回看板
  const [historyOpen, setHistoryOpen] = useState(false);
  const [archiveSelected, setArchiveSelected] = useState<ArchivedSession | null>(null);
  // onPaired 双通道复用（PairPage 手动配对成功 / Board 首拍探测成功）：
  // Board 侧一次挂载只发一次，重复置 true 时 React 对相同值自动 bail out，无谓重渲染可忽略
  const onPaired = useCallback(() => setPaired(true), []);
  const onUnpaired = useCallback(() => setPaired(false), []);
  // Board 每拍数据上报（回调引用稳定）：按会话身份找回活会话替换 selected。
  // 找不到（会话已下卡 / 过滤掉）→ 保留原快照（详情页不闪空）；
  // 字段全等 → 复用旧引用（React bail out，零重渲染）
  const handleSessionsChanged = useCallback((sessions: Session[]) => {
    setSelected((prev) => {
      if (prev === null) return prev;
      const live = sessions.find(
        (s) =>
          s.agentType === prev.agentType &&
          s.id === prev.id &&
          (s.agentType !== "dsh" ||
            (prev.dshInstanceId !== undefined && s.dshInstanceId === prev.dshInstanceId))
      );
      if (live === undefined || shallowEqualSession(live, prev)) return prev;
      return live;
    });
  }, []);
  // 返回看板：清空选中即可——Board 全程常驻挂载（仅 hidden 类切换），会话数据与
  // 滚动位置原生保留（对齐 paired!=='false' 的既有 hidden 模式）
  const onBackToBoard = useCallback(() => setSelected(null), []);

  return (
    <>
      {/* Board 常驻持轮询（探测也来自轮询首拍）；未配对态 / 查看详情 / 历史页时隐藏
          （hidden 而非卸载：会话数据与轮询保持，返回免重拉） */}
      {paired !== false && (
        <div className={paired === true && !selected && !historyOpen ? "contents" : "hidden"}>
          <Board
            onPaired={onPaired}
            onUnpaired={onUnpaired}
            onOpenSession={setSelected}
            onOpenHistory={() => setHistoryOpen(true)}
            onSessionsChanged={handleSessionsChanged}
          />
        </div>
      )}
      {paired === true && selected && (
        <SessionDetail
          key={`${selected.agentType}:${selected.dshInstanceId ?? ""}:${selected.id}`}
          session={selected}
          onBack={onBackToBoard}
        />
      )}
      {paired === true && historyOpen && !archiveSelected && (
        <ArchiveBoard
          onBack={() => setHistoryOpen(false)}
          onOpenCard={setArchiveSelected}
          onUnpaired={onUnpaired}
        />
      )}
      {paired === true && historyOpen && archiveSelected && (
        <ArchiveDetail
          session={archiveSelected}
          onBack={() => setArchiveSelected(null)}
          onActivated={() => {
            setArchiveSelected(null);
            setHistoryOpen(false);
          }}
        />
      )}
      {/* M5 P3-b：null（探测中）与 false（未配对）分診——探测期渲染连接指示器，
          不再出现密码表单（实测走隧道探测有一二十秒延迟，密码页先出像「时滞掉线」） */}
      {paired !== true && <PairPage onPaired={onPaired} probing={paired === null} />}
      {/* T3 未信任预检的 toast 承载面：移动端入口此前零 sonner 挂载（toast 会落进
          虚空，UsageExportActions 同款顾虑）——App 根常驻一枚，路由切换不丢 */}
      <Toaster position="top-center" />
    </>
  );
}
