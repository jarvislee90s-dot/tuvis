import { render, screen } from "@testing-library/react";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import App from "@/mobile/App";
import { MockEventSource } from "./eventSourceMock";
import type { SessionsResponse } from "@/types/session";

// 文案走 mobile.pair.* / mobile.board.* 双语键后按 zh 断言（QuestionCard.test 同款：
// 显式引入并固定中文）。App 渲染 Board 会拉 /host 做连接时语言重评估（无语言字段 →
// 浏览器兜底），jsdom 默认 en-US 会翻回 en——浏览器语言一并钉到 zh（language-toggle 同款）
import { pinZhNavigator } from "./pinZhNavigator";

let unpinZh: () => void;

beforeAll(async () => {
  unpinZh = await pinZhNavigator();
});

afterAll(() => {
  unpinZh();
});

// stub 全局 fetch（盖过 setup.ts 的 msw），探测链路完全确定；结束还原防泄漏到其他用例
afterEach(() => {
  vi.unstubAllGlobals();
});

beforeEach(() => {
  MockEventSource.reset();
});

function okSessions(totalCount: number): SessionsResponse {
  return { sessions: [], totalCount, waitingCount: 0 };
}

/** 脚本化 EventSource：首次连接自动送达快照（模拟服务端首帧） */
function installSse(snapshot: SessionsResponse) {
  class ScriptedEventSource extends MockEventSource {
    constructor(url: string) {
      super(url);
      setTimeout(() => this.emit("snapshot", snapshot), 0);
    }
  }
  vi.stubGlobal("EventSource", ScriptedEventSource);
}

describe("App 配对状态机：探测成功 → 配对页卸载（已配对设备刷新免重配）", () => {
  it("已配对设备：Board 首帧快照到达后配对页卸载，直接进看板（SSE 主通道）", async () => {
    installSse(okSessions(2));
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response(JSON.stringify(okSessions(2)), { status: 200 }))
    );
    render(<App />);

    // 首帧：探测中（paired=null），配对页先出（沿用不闪白口径）；
    // P3-b：探测期渲染连接指示器而非密码表单（区分「探测中」与「未配对」）
    expect(screen.getByText("兔维斯 远程接入")).toBeTruthy();
    expect(screen.getByTestId("probe-indicator")).toBeTruthy();
    expect(screen.queryByTestId("pin-input")).toBeNull();

    // 首帧快照到达（"2 个会话" 仅在成功数据到达后渲染；首帧 = 探测）
    expect(await screen.findByText("2 个会话")).toBeTruthy();
    // 配对页已卸载 —— Critical 回归锁：删除首帧成功回调后本断言变红
    expect(screen.queryByText("兔维斯 远程接入")).toBeNull();
  });

  it("网络异常（SSE 起不来 + 轮询失败）：不误判已配对，配对页保持且看板提示重试", async () => {
    vi.stubGlobal("EventSource", undefined); // 环境不支持 SSE → 立即降级轮询
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new TypeError("network down");
      })
    );
    render(<App />);
    // 等降级轮询的 catch 路径执行完（loadError 横幅渲染在隐藏的 Board 里，DOM 存在即可查）
    expect(await screen.findByText(/网络连接失败/)).toBeTruthy();
    // 配对页仍在：网络异常不得触发 null→true 翻转
    expect(screen.getByText("兔维斯 远程接入")).toBeTruthy();
  });
});
