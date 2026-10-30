// R5/U3：主按钮（发送／打断共用）动作推导的纯函数行为锁。
//
// 契约（逐字来自 task-R5-brief.md §R5 Interfaces）：
// - `deriveZcodePrimaryAction({hasContent, running, sendAllowed, stopAllowed, busy})`
//   返回 `"send" | "stop" | "disabled"`；纯函数，无 DOM/网络依赖。
// - `hasContent`：文字**或就绪附件**（就绪 = 上传完成的附件；上传中/失败附件不是
//   可发内容，但面板会把打断位一并关掉，见 ZcodeControlPanel 的 `attachBlocked`）。
// - `running`：官方新鲜 control 判定运行中（primaryTurn 且带前台执行 id）。
// - `sendAllowed` / `stopAllowed`：写面硬门 + 能力矩阵 + 既有守卫 + 代际新鲜度。
// - `busy`：有动作在途（零第二个动作）。
//
// 面板落地见 src/mobile/ZcodeControlPanel.tsx；本文件只锁决策表本身。

import { describe, expect, it } from "vitest";
import { deriveZcodePrimaryAction } from "@/mobile/zcode-primary-action";

describe("ZCode 主按钮动作推导（R5/U3）", () => {
  it("运行中且无内容 → 打断（复用原 stop 守卫）", () => {
    expect(
      deriveZcodePrimaryAction({
        hasContent: false,
        running: true,
        sendAllowed: true,
        stopAllowed: true,
        busy: false,
      })
    ).toBe("stop");
  });

  it("运行中且有内容 → 发送（有内容发送不得先打断）", () => {
    expect(
      deriveZcodePrimaryAction({
        hasContent: true,
        running: true,
        sendAllowed: true,
        stopAllowed: true,
        busy: false,
      })
    ).toBe("send");
  });

  it("运行中但无守卫（无唯一 stop 目标）→ 禁用", () => {
    expect(
      deriveZcodePrimaryAction({
        hasContent: false,
        running: true,
        sendAllowed: true,
        stopAllowed: false,
        busy: false,
      })
    ).toBe("disabled");
  });

  it("busy（在途）→ 禁用：任何输入组合都不产生第二个动作", () => {
    // 在途闸优先级最高：即便有内容且运行中（看起来可以发/可以打断），按钮也必须禁用。
    expect(
      deriveZcodePrimaryAction({
        hasContent: true,
        running: true,
        sendAllowed: true,
        stopAllowed: true,
        busy: true,
      })
    ).toBe("disabled");
    expect(
      deriveZcodePrimaryAction({
        hasContent: false,
        running: true,
        sendAllowed: true,
        stopAllowed: true,
        busy: true,
      })
    ).toBe("disabled");
    expect(
      deriveZcodePrimaryAction({
        hasContent: false,
        running: false,
        sendAllowed: true,
        stopAllowed: false,
        busy: true,
      })
    ).toBe("disabled");
  });

  it("附件-only（无文字但有就绪附件）→ 发送", () => {
    // hasContent 含就绪附件：附件本身就是可提交内容（面板口径：文字 || 就绪附件）。
    expect(
      deriveZcodePrimaryAction({
        hasContent: true,
        running: false,
        sendAllowed: true,
        stopAllowed: false,
        busy: false,
      })
    ).toBe("send");
    // 运行中 + 就绪附件：仍是发送（有内容不得先打断）。
    expect(
      deriveZcodePrimaryAction({
        hasContent: true,
        running: true,
        sendAllowed: true,
        stopAllowed: true,
        busy: false,
      })
    ).toBe("send");
  });

  it("上传中／失败附件：不可发送，但绝不退化成 stop", () => {
    // 附件闸未过时面板同时关掉发送位与打断位（attachBlocked）→ 结果必须是 disabled，
    // 不得是 send（有内容未就绪不得上行）也不得是 stop（用户正在组装消息）。
    expect(
      deriveZcodePrimaryAction({
        hasContent: false,
        running: true,
        sendAllowed: false,
        stopAllowed: false,
        busy: false,
      })
    ).toBe("disabled");
    expect(
      deriveZcodePrimaryAction({
        hasContent: true,
        running: true,
        sendAllowed: false,
        stopAllowed: false,
        busy: false,
      })
    ).toBe("disabled");
    expect(
      deriveZcodePrimaryAction({
        hasContent: false,
        running: false,
        sendAllowed: false,
        stopAllowed: false,
        busy: false,
      })
    ).toBe("disabled");
  });

  it("旧代际（generation 不匹配，两闸皆关）→ 禁用", () => {
    // 代际不匹配的投影：写面硬门与既有守卫都不放行（sendAllowed/stopAllowed 皆 false），
    // 因此按钮必须禁用——既不发也不打断。
    expect(
      deriveZcodePrimaryAction({
        hasContent: true,
        running: true,
        sendAllowed: false,
        stopAllowed: false,
        busy: false,
      })
    ).toBe("disabled");
    expect(
      deriveZcodePrimaryAction({
        hasContent: false,
        running: true,
        sendAllowed: false,
        stopAllowed: false,
        busy: false,
      })
    ).toBe("disabled");
  });

  it("空闲无内容 → 发送占位（点击不得发空正文）", () => {
    // 非运行中且无内容：按钮是发送位（占位），空消息由面板层拦住（不发空正文）。
    expect(
      deriveZcodePrimaryAction({
        hasContent: false,
        running: false,
        sendAllowed: true,
        stopAllowed: false,
        busy: false,
      })
    ).toBe("send");
    // 写面未就绪（闸关）→ 禁用。
    expect(
      deriveZcodePrimaryAction({
        hasContent: false,
        running: false,
        sendAllowed: false,
        stopAllowed: false,
        busy: false,
      })
    ).toBe("disabled");
  });
});
