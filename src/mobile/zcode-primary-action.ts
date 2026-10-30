// ZCode 主按钮动作推导（R5/U3）：发送与打断共用**同一个**主按钮。
//
// 纪律：
// - 纯函数：无副作用、无 DOM/网络依赖，决策表可逐分支单测锁定。
// - `running` / `stopAllowed` 只能来自**官方新鲜 control**（`session.control.activeWorks`）
//   与既有 stop 守卫；**不得**用看板颜色或本地忙态猜测。
// - `hasContent` = 文字**或就绪附件**（就绪 = 上传完成、带 path 的附件）。
// - `busy` = 有动作在途：在途期间主按钮恒禁用，绝不产生第二个动作。
//
// 决策表（顺序即优先级）：
// 1. 在途 → disabled（零第二个动作）。
// 2. 有内容 → send（**有内容发送不得先打断**）；发送闸关 → disabled。
// 3. 无内容且官方判定运行中 → stop（复用原 stop 守卫）；无守卫 → disabled。
// 4. 无内容且未运行 → send（发送占位：点击不得发空正文，由面板层拦空消息）。

export type ZcodePrimaryAction = "send" | "stop" | "disabled";

export function deriveZcodePrimaryAction(input: {
  hasContent: boolean;
  running: boolean;
  sendAllowed: boolean;
  stopAllowed: boolean;
  busy: boolean;
}): ZcodePrimaryAction {
  if (input.busy) return "disabled";
  if (input.hasContent) return input.sendAllowed ? "send" : "disabled";
  if (input.running) return input.stopAllowed ? "stop" : "disabled";
  return input.sendAllowed ? "send" : "disabled";
}
