import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import ZcodeControlPanel from "@/mobile/ZcodeControlPanel";
import {
  ZCODE_ACTIONS_PATH,
  ZCODE_CONNECTION_PATH,
  ZCODE_SESSION_PATH,
  type ActionReceipt,
  type CapabilityView,
  type ConnectionView,
  type ControlAction,
  type ProviderChoice,
  type SessionView,
} from "@/lib/api/zcodeControl";
import { pinZhNavigator } from "./pinZhNavigator";

const SID = "session_0000000000000000000000000000000a";
const GEN = 3;
let unpinZh: () => void;
beforeAll(async () => {
  unpinZh = await pinZhNavigator();
});
afterAll(() => unpinZh());
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

const flag = (enabled: boolean) => ({ enabled, reasonCode: enabled ? null : "needs_evidence" });
const caps = (): CapabilityView => ({
  send: flag(true),
  singleSelect: flag(true),
  multiSelect: flag(true),
  freeText: flag(true),
  interrupt: flag(true),
  approvalReply: flag(true),
  model: flag(true),
  thoughtLevel: flag(true),
  queueEdit: flag(false),
  queueNow: flag(false),
  queueResume: flag(false),
});
const connection = (): ConnectionView => ({
  state: "paired",
  generation: GEN,
  reasonCode: null,
  deviceLabel: "Synthetic",
});

const providerA: ProviderChoice = {
  providerId: "a",
  label: "Provider A",
  available: true,
  availabilityKnown: true,
  models: [{ modelId: "a-model", thoughtLevels: ["low", "high"] }],
};
const providerB: ProviderChoice = {
  providerId: "b",
  label: "Provider B",
  available: true,
  availabilityKnown: true,
  models: [{ modelId: "b-model", thoughtLevels: ["low"] }],
};
/** 缺 `available` 字段的服务端投影：目录仍可选，由宿主运行时判定。 */
const unknownProviderB: ProviderChoice = {
  providerId: "b",
  label: "Provider B",
  available: false,
  availabilityKnown: false,
  models: [{ modelId: "b-model", thoughtLevels: ["low"] }],
};
/** 宿主显式宣告不可用：不再发送前禁用，仍由宿主判定。 */
const disabledProviderC: ProviderChoice = {
  providerId: "c",
  label: "Provider C",
  available: false,
  availabilityKnown: true,
  models: [{ modelId: "c-model", thoughtLevels: ["low"] }],
};

const session = (providers: ProviderChoice[] = [providerA, providerB]): SessionView => ({
  sessionId: SID,
  generation: GEN,
  revision: 4,
  ready: true,
  pending: [],
  control: { activeWorks: [] },
  selection: {
    provider: "a",
    model: "a-model",
    thought: "low",
    models: [{ modelId: "a-model", thoughtLevels: ["low", "high"] }],
    providers,
  },
  capabilities: caps(),
});
const json = (body: unknown) =>
  new Response(JSON.stringify(body), {
    status: 200,
    headers: { "content-type": "application/json" },
  });

function install(view: SessionView = session()) {
  const posts: { actionId: string; action: ControlAction }[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(view);
      if (url.startsWith("/m/api/v1/zcode/local-queue?"))
        return json({
          sessionId: SID,
          version: 0,
          autoDrain: true,
          pauseReason: null,
          items: [],
          recoverableEdits: [],
        });
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { actionId: string; action: ControlAction };
        posts.push(body);
        return json({
          actionId: body.actionId,
          sessionId: SID,
          generation: GEN,
          state: "accepted",
          reasonCode: null,
        } satisfies ActionReceipt);
      }
      throw new Error(`unexpected fetch: ${url}`);
    })
  );
  return posts;
}

const providerSelect = () => screen.getByTestId("zcode-provider-select") as HTMLSelectElement;
const modelSelect = () => screen.getByTestId("zcode-model-select") as HTMLSelectElement;
const thoughtSelect = () => screen.getByTestId("zcode-thought-select") as HTMLSelectElement;

describe("ZcodeProviderSelection", () => {
  it("provider_and_model_stay_pending_until_apply_sends_one_full_triple", async () => {
    const posts = install();
    render(<ZcodeControlPanel sessionId={SID} />);
    await waitFor(() => expect(screen.getByTestId("zcode-provider-select")).toBeVisible());
    // 初始投影落地后 effect 会把 pendingProvider 清回服务端值；等这次同步完成再改，
    // 否则慢机上 change 会被 late effect 覆盖，模型目录仍停在 a-model。
    await waitFor(() => expect(providerSelect().value).toBe("a"));
    await waitFor(() => expect(modelSelect().value).toBe("a-model"));
    const apply = screen.getByTestId("zcode-selection-apply") as HTMLButtonElement;
    // 无待选偏离：确认修改禁用（下拉只是浏览，不构成变更）。
    expect(apply).toBeDisabled();

    fireEvent.change(screen.getByTestId("zcode-provider-select"), { target: { value: "b" } });
    await waitFor(() => expect(posts).toHaveLength(0));
    // 待选偏离 → 确认修改可点，但此刻仍零 POST。
    await waitFor(() => expect(apply).toBeEnabled());
    // provider 变更后的模型目录由 React 状态刷新；不能只等 posts==0（初始即为 0），
    // 否则旧 model 在 CI 慢机上仍是 a-model，形成与实现无关的竞态。
    await waitFor(() => {
      expect(Array.from(modelSelect().options).map((option) => option.value)).toEqual(["b-model"]);
    });
    fireEvent.change(modelSelect(), { target: { value: "b-model" } });
    // 模型下拉同样只改本地待选：下拉显示用户选值，但零 POST。
    expect(modelSelect().value).toBe("b-model");
    await waitFor(() => expect(posts).toHaveLength(0));

    // 显式确认才提交一次完整三元组（provider 也随待选值上行）。
    fireEvent.click(apply);
    await waitFor(() => expect(posts).toHaveLength(1));
    expect(posts[0].action).toMatchObject({
      kind: "selection",
      provider: "b",
      model: "b-model",
      thought: "low",
    });
  });

  it("availability_fields_do_not_disable_provider_options", async () => {
    install(session([providerA, disabledProviderC, unknownProviderB]));
    render(<ZcodeControlPanel sessionId={SID} />);
    await waitFor(() => expect(screen.getByTestId("zcode-provider-select")).toBeVisible());
    for (const id of ["b", "c"]) {
      const option = Array.from(providerSelect().options).find((item) => item.value === id);
      expect(option).toBeDefined();
      expect(option?.disabled).toBe(false);
    }
    expect(screen.queryByTestId("zcode-selection-unknown-hint")).toBeNull();
  });

  it("unknown_provider_switch_posts_only_after_apply_without_confirmation", async () => {
    const posts = install(session([providerA, unknownProviderB]));
    render(<ZcodeControlPanel sessionId={SID} />);
    await waitFor(() => expect(screen.getByTestId("zcode-provider-select")).toBeVisible());
    // 同上：等初始投影（a / a-model）落定，避免 late effect 把 pendingProvider 清回 a。
    await waitFor(() => expect(providerSelect().value).toBe("a"));
    await waitFor(() => expect(modelSelect().value).toBe("a-model"));
    const apply = screen.getByTestId("zcode-selection-apply") as HTMLButtonElement;

    fireEvent.change(providerSelect(), { target: { value: "b" } });
    await waitFor(() => expect(posts).toHaveLength(0));
    fireEvent.change(modelSelect(), { target: { value: "b-model" } });
    expect(modelSelect().value).toBe("b-model");
    await waitFor(() => expect(apply).toBeEnabled());
    // 未知可用性不触发任何预判弹窗；确认前零 POST。
    expect(screen.queryByTestId("zcode-selection-confirm-dialog")).toBeNull();
    await waitFor(() => expect(posts).toHaveLength(0));

    fireEvent.click(apply);
    await waitFor(() => expect(posts).toHaveLength(1));
    expect(posts[0].action).toEqual({
      kind: "selection",
      sessionId: SID,
      generation: GEN,
      provider: "b",
      model: "b-model",
      thought: "low",
    });
    expect(screen.queryByTestId("zcode-selection-confirm-dialog")).toBeNull();
  });

  it("apply_is_disabled_without_pending_drift_and_click_emits_zero_post", async () => {
    // 无待选偏离时确认按钮禁用；把下拉 change 成**同值**仍不算变更：
    // 按钮保持 disabled，点击是硬门，不得发出 selection。
    const posts = install();
    render(<ZcodeControlPanel sessionId={SID} />);
    await waitFor(() => expect(providerSelect().value).toBe("a"));
    await waitFor(() => expect(modelSelect().value).toBe("a-model"));
    const apply = screen.getByTestId("zcode-selection-apply") as HTMLButtonElement;
    // 文案来自 mobile.zcodeControl.selectionApply（zh=确认修改），不是裸 testid 按钮。
    expect(apply.textContent).toBe("确认修改");
    expect(apply).toBeDisabled();
    // 同值 change 不构成偏离：按钮保持禁用。
    fireEvent.change(modelSelect(), { target: { value: "a-model" } });
    fireEvent.change(thoughtSelect(), { target: { value: "low" } });
    expect(apply).toBeDisabled();
    fireEvent.click(apply);
    await waitFor(() => expect(posts).toHaveLength(0));
    expect(posts).toHaveLength(0);
  });

  it("apply_keeps_pending_selection_in_dropdowns_before_projection_changes", async () => {
    // 待选偏离时确认可点；提交后投影未变（回执只到 accepted、会话未刷新）：
    // 下拉保持用户待选值，不能被旧投影立刻拉回——这正是「显式生效时间点」的可见承诺。
    const posts = install();
    render(<ZcodeControlPanel sessionId={SID} />);
    await waitFor(() => expect(providerSelect().value).toBe("a"));
    await waitFor(() => expect(modelSelect().value).toBe("a-model"));
    await waitFor(() =>
      expect(Array.from(thoughtSelect().options).map((option) => option.value)).toEqual([
        "low",
        "high",
      ])
    );
    const apply = screen.getByTestId("zcode-selection-apply") as HTMLButtonElement;
    expect(apply).toBeDisabled();

    // 档位 low → high：待选偏离，但确认前零 POST。
    fireEvent.change(thoughtSelect(), { target: { value: "high" } });
    expect(thoughtSelect().value).toBe("high");
    await waitFor(() => expect(apply).toBeEnabled());
    expect(posts).toHaveLength(0);

    fireEvent.click(apply);
    await waitFor(() => expect(posts).toHaveLength(1));
    expect(posts[0].action).toMatchObject({
      kind: "selection",
      provider: "a",
      model: "a-model",
      thought: "high",
    });
    // 投影（selection.thought="low"）没变：待选 high 必须仍在屏上，不得回弹。
    expect(thoughtSelect().value).toBe("high");
  });
});
