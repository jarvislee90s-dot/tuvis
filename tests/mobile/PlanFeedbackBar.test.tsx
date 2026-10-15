import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import PlanFeedbackBar from "@/mobile/PlanFeedbackBar";

// 计划反馈条（2026-10-04 计划批准卡）：claude 计划批准框反馈通道的卡内 UI。
// fetch 全量 stub，按 /session-plan-feedback 单端点分路。

let fetchMock: ReturnType<typeof vi.fn>;
let routes: { body?: Record<string, unknown>; status?: number };

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function installFetch() {
  fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes("/session-plan-feedback")) {
      if (routes.status) {
        return new Response(JSON.stringify({ error: "internal" }), { status: routes.status });
      }
      return new Response(JSON.stringify(routes.body ?? { status: "done" }), { status: 200 });
    }
    throw new Error(`unexpected fetch: ${url}`);
  });
  vi.stubGlobal("fetch", fetchMock);
}

async function flushAsync() {
  for (let i = 0; i < 6; i += 1) {
    await act(async () => {
      await Promise.resolve();
    });
  }
}

function callsTo(action: string): Array<Array<unknown>> {
  return fetchMock.mock.calls.filter((c: unknown[]) => {
    const init = c[1] as RequestInit | undefined;
    return (
      String(c[0]).includes("/session-plan-feedback") &&
      JSON.parse(String(init?.body ?? "{}")).action === action
    );
  });
}

describe("PlanFeedbackBar：计划反馈条（2026-10-04）", () => {
  it("发送：POST {action:'type', text, submit:true} → 成功后显示已发送并自隐上报", async () => {
    vi.useFakeTimers();
    try {
      installFetch();
      routes = { body: { status: "done" } };
      const onDismiss = vi.fn();
      render(<PlanFeedbackBar sessionId="sess-fb-1" onDismiss={onDismiss} />);
      fireEvent.change(screen.getByTestId("plan-feedback-input"), {
        target: { value: "第二版压缩到 800 字" },
      });
      fireEvent.click(screen.getByTestId("plan-feedback-send"));
      await act(async () => {
        await Promise.resolve();
      });
      const init = callsTo("type")[0][1] as RequestInit;
      expect(JSON.parse(String(init.body))).toEqual({
        sessionId: "sess-fb-1",
        action: "type",
        text: "第二版压缩到 800 字",
        submit: true,
      });
      expect(screen.getByTestId("plan-feedback-sent").textContent).toContain("已发送");
      // 自隐倒计时（SENT_DISMISS_MS）后上报父级卸载
      await act(async () => {
        vi.advanceTimersByTime(2000);
      });
      expect(onDismiss).toHaveBeenCalledTimes(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("覆盖写入：submit=false 仅暂存（不清空输入框内容之外的终端提交），清空：action='clear'", async () => {
    installFetch();
    routes = { body: { status: "done" } };
    render(<PlanFeedbackBar sessionId="sess-fb-2" onDismiss={() => {}} />);
    fireEvent.change(screen.getByTestId("plan-feedback-input"), {
      target: { value: "暂存内容" },
    });
    fireEvent.click(screen.getByTestId("plan-feedback-overwrite"));
    await flushAsync();
    const init = callsTo("type")[0][1] as RequestInit;
    expect(JSON.parse(String(init.body))).toEqual({
      sessionId: "sess-fb-2",
      action: "type",
      text: "暂存内容",
      submit: false,
    });
    // 暂存态提示出现；输入框为空后「发送」仍可点（仅回车提交暂存那份）
    expect(screen.getByTestId("plan-feedback-bar").textContent).toContain("已暂存");
    expect((screen.getByTestId("plan-feedback-send") as HTMLButtonElement).disabled).toBe(false);
    // 清空 → clear 动作
    fireEvent.click(screen.getByTestId("plan-feedback-clear"));
    await flushAsync();
    expect(callsTo("clear")).toHaveLength(1);
  });

  it("failed 回执：显示错误文案；输入框为空且无暂存 → 发送禁用", async () => {
    installFetch();
    routes = { body: { status: "failed", error: "反馈编辑行不在场，请到终端核对" } };
    render(<PlanFeedbackBar sessionId="sess-fb-3" onDismiss={() => {}} />);
    // 无内容无暂存：发送/覆盖写入/清空都禁用
    expect((screen.getByTestId("plan-feedback-send") as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByTestId("plan-feedback-overwrite") as HTMLButtonElement).disabled).toBe(
      true
    );
    expect((screen.getByTestId("plan-feedback-clear") as HTMLButtonElement).disabled).toBe(true);
    fireEvent.change(screen.getByTestId("plan-feedback-input"), { target: { value: "x" } });
    fireEvent.click(screen.getByTestId("plan-feedback-send"));
    expect(
      await screen
        .findByTestId("plan-feedback-error")
        .then((el) => el.textContent)
        .then((t) => t?.includes("反馈编辑行不在场"))
    ).toBe(true);
  });
});
