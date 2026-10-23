import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import PairPage from "@/mobile/PairPage";
import { ApiError, pairWithPin } from "@/mobile/api";

// 文案走 mobile.pair.* 双语键后按 zh 断言（QuestionCard.test 同款：显式引入并固定中文）
import i18n from "@/i18n";

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

// 只 mock pairWithPin（交互用例不依赖真实 fetch）；ApiError 保持真实现
vi.mock("@/mobile/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/mobile/api")>();
  return { ...actual, pairWithPin: vi.fn() };
});

afterEach(() => {
  vi.mocked(pairWithPin).mockReset();
  // 清掉用例塞的 #pin= hash，防串用例
  window.history.replaceState(null, "", "/");
});

/** 冲刷一帧微任务（submit promise 链走完） */
const flush = () => act(async () => {});

describe("PairPage 访问密码单入口（M5 A7）", () => {
  it("手输 4 位密码点进入 → pairWithPin 以输入值调用，成功回调 onPaired", async () => {
    vi.mocked(pairWithPin).mockResolvedValue({ ok: true });
    const onPaired = vi.fn();
    render(<PairPage onPaired={onPaired} />);
    fireEvent.change(screen.getByPlaceholderText("0000"), { target: { value: "4827" } });
    fireEvent.click(screen.getByRole("button", { name: "进入看板" }));
    await flush();
    expect(pairWithPin).toHaveBeenCalledWith("4827");
    expect(onPaired).toHaveBeenCalledTimes(1);
  });

  it("未满 4 位按钮禁用，不发起提交（手输口径不烧限速次数）", () => {
    render(<PairPage onPaired={vi.fn()} />);
    const btn = screen.getByRole("button", { name: "进入看板" }) as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
    fireEvent.change(screen.getByPlaceholderText("0000"), { target: { value: "48" } });
    expect((screen.getByRole("button", { name: "进入看板" }) as HTMLButtonElement).disabled).toBe(
      true
    );
    expect(pairWithPin).not.toHaveBeenCalled();
  });

  it("#pin= 扫码自动填并自动提交一次（重渲染不重复提交），hash 随即清除", async () => {
    // 挂起态：便于断言「已从链接自动填入」在场
    let resolve!: (v: { ok: boolean }) => void;
    vi.mocked(pairWithPin).mockReturnValue(new Promise((res) => (resolve = res)));
    window.history.replaceState(null, "", "/?board#pin=4827");
    const onPaired = vi.fn();
    const { rerender } = render(<PairPage onPaired={onPaired} />);
    await flush();

    // 输入框已预填；提示行在场；StrictMode/重渲染不重复提交
    expect((screen.getByPlaceholderText("0000") as HTMLInputElement).value).toBe("4827");
    expect(screen.getByText(/已从链接自动填入/)).toBeTruthy();
    rerender(<PairPage onPaired={onPaired} />);
    await flush();
    expect(pairWithPin).toHaveBeenCalledTimes(1);
    expect(window.location.hash).toBe("");

    resolve({ ok: true });
    await flush();
    expect(onPaired).toHaveBeenCalledTimes(1);
  });

  it("401 invalid_pin + remaining=3 → 剩余次数文案（含锁定预告）", async () => {
    vi.mocked(pairWithPin).mockRejectedValue(
      new ApiError(401, "401", { error: "invalid_pin", remaining: 3 })
    );
    render(<PairPage onPaired={vi.fn()} />);
    fireEvent.change(screen.getByPlaceholderText("0000"), { target: { value: "1111" } });
    fireEvent.click(screen.getByRole("button", { name: "进入看板" }));
    await flush();
    expect(screen.getByText(/还可尝试 3 次/)).toBeTruthy();
    expect(screen.getByText(/锁定 10 分钟/)).toBeTruthy();
  });

  it("401 invalid_pin + remaining=0 → 次数用尽文案", async () => {
    vi.mocked(pairWithPin).mockRejectedValue(
      new ApiError(401, "401", { error: "invalid_pin", remaining: 0 })
    );
    render(<PairPage onPaired={vi.fn()} />);
    fireEvent.change(screen.getByPlaceholderText("0000"), { target: { value: "1111" } });
    fireEvent.click(screen.getByRole("button", { name: "进入看板" }));
    await flush();
    expect(screen.getByText(/次数过多，已锁定/)).toBeTruthy();
  });

  it("429 retryAfter=600 → 锁定分钟数文案；45 秒向上取整为 1 分钟", async () => {
    vi.mocked(pairWithPin).mockRejectedValue(new ApiError(429, "429", { retryAfter: 600 }));
    render(<PairPage onPaired={vi.fn()} />);
    fireEvent.change(screen.getByPlaceholderText("0000"), { target: { value: "1111" } });
    fireEvent.click(screen.getByRole("button", { name: "进入看板" }));
    await flush();
    expect(screen.getByText(/约 10 分钟后再试/)).toBeTruthy();

    vi.mocked(pairWithPin).mockRejectedValue(new ApiError(429, "429", { retryAfter: 45 }));
    fireEvent.click(screen.getByRole("button", { name: "进入看板" }));
    await flush();
    expect(screen.getByText(/约 1 分钟后再试/)).toBeTruthy();
  });

  it("401 pin_not_set → 桌面端尚未设置密码文案", async () => {
    vi.mocked(pairWithPin).mockRejectedValue(new ApiError(401, "401", { error: "pin_not_set" }));
    render(<PairPage onPaired={vi.fn()} />);
    fireEvent.change(screen.getByPlaceholderText("0000"), { target: { value: "1111" } });
    fireEvent.click(screen.getByRole("button", { name: "进入看板" }));
    await flush();
    expect(screen.getByText(/尚未设置访问密码/)).toBeTruthy();
  });

  it("403 cap_full → 设备已满文案", async () => {
    vi.mocked(pairWithPin).mockRejectedValue(new ApiError(403, "403", { error: "cap_full" }));
    render(<PairPage onPaired={vi.fn()} />);
    fireEvent.change(screen.getByPlaceholderText("0000"), { target: { value: "1111" } });
    fireEvent.click(screen.getByRole("button", { name: "进入看板" }));
    await flush();
    expect(screen.getByText(/设备数量已达上限/)).toBeTruthy();
  });

  it("网络异常（status=null）→ 网络异常文案", async () => {
    vi.mocked(pairWithPin).mockRejectedValue(new ApiError(null, "网络异常"));
    render(<PairPage onPaired={vi.fn()} />);
    fireEvent.change(screen.getByPlaceholderText("0000"), { target: { value: "1111" } });
    fireEvent.click(screen.getByRole("button", { name: "进入看板" }));
    await flush();
    expect(screen.getByText(/网络异常/)).toBeTruthy();
  });

  it("旧入口已移除：无「请求接入」/「4 位确认码」/接入码输入", () => {
    render(<PairPage onPaired={vi.fn()} />);
    expect(screen.queryByText("请求接入")).toBeNull();
    expect(screen.queryByText("4 位确认码")).toBeNull();
    expect(screen.queryByPlaceholderText("接入码")).toBeNull();
    expect(screen.queryByPlaceholderText("设备名称（选填）")).toBeNull();
  });
});
