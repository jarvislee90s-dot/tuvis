// tests/settings/tailscaleWizard.test.tsx — §C2 Task 6 + §C3 Task 7：Tailscale 首次
// 配置引导向导（src/components/settings/TailscaleWizard.tsx）。覆盖：九步渲染与三态 /
// 需要人的步骤动作文案 / 卡住显示 blocked_reason（不许只转圈）/ Windows 未实机校验
// 弱提示 / login「去登录」链接（兔维斯 不代登录）/ run_step 触发与回执后重探 /
// funnel 批准链接展示 / verify §C3 可达性校验（尚未生效 + 重试）/ 动作失败落步骤行
//（不再零反馈）/ 头部看板地址按校验态门控 / tailscale 卡展开区集成（Task 9 收敛：
// 向导挂进 RemoteSection 的 tailscale 卡详情区，独立入口行已撤）/ i18n zh-en
// tsWizard 两级键齐备且两 locale 键集相等。
// mock 模式沿用 tests/settings/remoteSection.test.tsx（vi.hoisted + vi.mock）。
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import path from "node:path";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
// useAppTranslation 内部 listen("@tauri-apps/api/event") 在 jsdom 无 Tauri 内核，须 mock
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
  emit: vi.fn(async () => {}),
}));

// tests/setup.ts 未初始化 i18n，显式引入并按默认英文断言（jsdom navigator.language=en）
import i18n from "@/i18n";
import { RemoteSection } from "@/components/settings/RemoteSection";
import { TailscaleWizard } from "@/components/settings/TailscaleWizard";

void i18n;

// ---- mock 载荷工厂（形状契约 = Rust 端 tailscale::wizard_status 注释）----
const step = (id: string, needsHuman = false, humanActionKey = "", humanOptional = false) => ({
  id,
  needsHuman,
  humanOptional,
  humanActionKey,
});
const state = (id: string, done: boolean, blockedReason: string | null = null) => ({
  id,
  done,
  blockedReason,
});

// macOS 九步（已实测）基线：detect 完成、download/shields_up 待做、sys_ext 完成、
// login 待做（带 authUrl）、funnel 待做、verify 未验（§C3 校验态全局默认 unverified）、
// autostart 恒完成
const macProbe = (over: Record<string, unknown> = {}) => ({
  platform: "mac",
  windowsVerified: true,
  // I-3：Windows 验证位按覆盖面拆细——windowsVerifiedFrom = 实测覆盖从哪一步起，
  // windowsUnverifiedSteps = 没被端到端实机跑过的步骤（非 Windows 平台恒空）
  windowsVerifiedFrom: null,
  windowsUnverifiedSteps: [],
  // B2：写路径（funnel --bg / reset / 批准链接抓取）实机验证位——macOS 仍为零实机验证
  writePathVerified: false,
  steps: [
    step("detect"),
    step("download"),
    step("install", true, "settings.remote.tsWizard.actAdmin"),
    step("sys_ext", true, "settings.remote.tsWizard.actSysExt"),
    step("login", true, "settings.remote.tsWizard.actLogin"),
    step("shields_up"),
    step("funnel", true, "settings.remote.tsWizard.actFunnel"),
    step("verify"),
    step("autostart"),
  ],
  states: [
    state("detect", true),
    state("download", false),
    state("install", false),
    state("sys_ext", true),
    state("login", false),
    state("shields_up", false),
    state("funnel", false),
    state("verify", false),
    state("autostart", true),
  ],
  authUrl: "https://login.tailscale.com/a/abc123",
  running: false,
  boardUrl: null,
  reach: { state: "unverified" },
  ...over,
});

// **M7（2026-10-08 架构评审）**：Windows 载荷工厂——生产**恒**给 Windows 下发实测覆盖起点
// （Rust `WINDOWS_VERIFIED_FROM` = "detect"，见 `windows_verification_for`），而夹具此前
// 传 `windowsVerifiedFrom: null`（那是**非 Windows** 的形态）⇒ 等于用一个生产不会出现的
// 载荷在做断言（"提示撤下"的结论虽不变，但夹具与生产脱钩，后人照抄就会把契约带偏）。
// 本工厂把 Windows 的默认形态钉成生产形态；要测"起点后移"的机制形态，显式覆盖该字段
// （见 I-3 组那条 windowsVerifiedFrom: "shields_up"）。
const windowsProbe = (over: Record<string, unknown> = {}) =>
  macProbe({ platform: "windows", windowsVerifiedFrom: "detect", ...over });

const stepRow = (id: string) => document.querySelector(`[data-step="${id}"]`) as HTMLElement;

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string) => {
    if (cmd === "remote_ts_probe") return macProbe();
    return null;
  });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe("TailscaleWizard 步骤渲染与三态（§C2）", () => {
  it("macOS 渲染九步；done 态映射三态点（detect 已完成 / download 待做 / autostart 恒完成）", async () => {
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    const rows = document.querySelectorAll("[data-step]");
    expect(rows.length).toBe(9);
    expect(stepRow("detect").getAttribute("data-done")).toBe("true");
    expect(stepRow("download").getAttribute("data-done")).toBe("false");
    expect(stepRow("autostart").getAttribute("data-done")).toBe("true");
    // 三态文案随行展示
    expect(within(stepRow("detect")).getByText("Done")).toBeTruthy();
    expect(within(stepRow("download")).getByText("To do")).toBeTruthy();
  });

  it("需要人的步骤突出动作文案（后端 humanActionKey 下发 → 前端翻译上墙）", async () => {
    render(<TailscaleWizard />);
    expect(await screen.findByText(/prompt for the admin password/i)).toBeTruthy();
    expect(screen.getByText(/approve the Tailscale system extension/i)).toBeTruthy();
    expect(screen.getByText(/complete Tailscale login in your browser/i)).toBeTruthy();
    expect(screen.getByText(/approve it once in your browser/i)).toBeTruthy();
  });

  // ---- 2026-10-08：安装路径两道提示（用户实测缺口：装到非默认路径 ⇒ 兔维斯 找不到 CLI）----
  // 兔维斯 执行 `msiexec /i <包>` **刻意不加 /qn**——「装在哪里」的选择权给用户。缺口是
  // 兔维斯 原先只在 `C:\Program Files\Tailscale` 找 CLI：装到 `D:\软件\Tailscale` 就判「没装」，
  // 向导又下载又安装、装完还是找不到。治本是后端补「服务登记 ImagePath」第二来源；
  // 前端这两条是**提示**：① Windows 安装步的动作文案（后端 actAdminWinMsi 下发）点明
  // 路径可自选、默认最省事；② 安装行常驻弱提示，说清 兔维斯 的**两个查找位置**并给出
  // 「装完点刷新状态」的动作（重探时机 = 挂载/动作后，不点就一直显示旧结论）。
  it("Windows 安装行常驻「查找位置」弱提示 + 动作文案点明路径可自选", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe")
        return windowsProbe({
          windowsVerified: true,
          windowsUnverifiedSteps: [],
          steps: [
            step("detect"),
            step("download"),
            step("install", true, "settings.remote.tsWizard.actAdminWinMsi"),
            step("login", true, "settings.remote.tsWizard.actLogin"),
            step("shields_up"),
            step("funnel", true, "settings.remote.tsWizard.actFunnel", true),
            step("verify"),
            step("autostart"),
          ],
        });
      return null;
    });
    render(<TailscaleWizard />);
    // ① 后端下发的 Windows 安装动作文案里必须点明「安装向导会让你选路径」
    expect(await screen.findByText(/choose the install path/i)).toBeTruthy();
    // ② 安装行弱提示：两个查找位置 + 装完点刷新
    const hint = screen.getByTestId("ts-install-path-hint");
    expect(hint.textContent).toMatch(/default install directory/i);
    expect(hint.textContent).toMatch(/service/i);
    expect(hint.textContent).toMatch(/refresh/i);
  });

  it("macOS 不显示安装路径提示（.pkg 固定装到 /Applications，用户无从选择）", async () => {
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    expect(screen.queryByTestId("ts-install-path-hint")).toBeNull();
  });

  it("卡住必须点名原因：blockedReason 随行显示「Stuck at this step: 原因」，不许只转圈", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe")
        return macProbe({
          states: [
            state("detect", true),
            state("download", false),
            state("install", false),
            state("sys_ext", true),
            state("login", false),
            state("shields_up", false),
            state("funnel", false, "Funnel 已被其他 serve 配置占用（非 兔维斯）"),
            state("verify", false),
            state("autostart", true),
          ],
        });
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    const blocked = within(stepRow("funnel")).getByTestId("ts-blocked");
    expect(blocked.textContent).toContain("Stuck at this step:");
    expect(blocked.textContent).toContain("Funnel 已被其他 serve 配置占用");
    // M4 对照档：无 blockedTone（或 rose）的卡点是**故障档**——红色 + 「卡住」前缀
    expect(blocked.getAttribute("data-blocked-tone")).toBe("rose");
    expect(blocked.className).toMatch(/rose/);
  });

  // M4（2026-10-07 评审 Minor）：卡点的**语义档位**随后端判据走。恢复窗口内的
  // 「后端正在重连…恢复窗口内不判定/不写」是**中间态**（线稿 82-84：既不是故障 rose
  // 也不是正常完成 green），旧实现一律 text-rose-500 + 「卡在这一步：」把每次开机的
  // 正常恢复涂成红色故障。
  it("M4：恢复窗口的卡点走琥珀档（不涂红、不加「卡住」前缀），故障卡点仍走红档", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe")
        return macProbe({
          states: [
            state("detect", true),
            // 对照：真故障卡点（无 blockedTone 字段 → fail-safe 到 rose）
            state("download", false, "sha256 校验失败（模拟）"),
            state("install", false),
            state("sys_ext", true),
            state("login", false),
            state("shields_up", false),
            {
              id: "funnel",
              done: false,
              blockedReason:
                "Tailscale 后端正在重连（NoState）——恢复窗口内不判定 Funnel 配置，稍候自动复评",
              blockedTone: "amber",
            },
            state("verify", false),
            state("autostart", true),
          ],
        });
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    const amber = within(stepRow("funnel")).getByTestId("ts-blocked");
    expect(amber.getAttribute("data-blocked-tone")).toBe("amber");
    expect(amber.className).toMatch(/amber/);
    expect(amber.className).not.toMatch(/rose/);
    expect(amber.textContent).not.toContain("Stuck at this step:");
    // 成因**原样透出**（后端下发的字符串，前端不翻译不猜）
    expect(amber.textContent).toMatch(/后端正在重连/);
    // 故障卡点不受影响（档位不是一律加琥珀，否则档位本身失去意义）
    const rose = within(stepRow("download")).getByTestId("ts-blocked");
    expect(rose.getAttribute("data-blocked-tone")).toBe("rose");
    expect(rose.className).toMatch(/rose/);
    expect(rose.textContent).toContain("Stuck at this step:");
  });
});

describe("TailscaleWizard Windows 验证位按覆盖面表述（I-3：验证位不得大于证据）", () => {
  // **机制守卫**（②，2026-10-07）：Windows 整条流程已被用户实机走完，故**生产载荷**是
  // `windowsVerified=true ∧ 清单为空`（见下面「整条流程都验过 → 提示撤下」那条 + Rust
  // `windows_verification_list_is_derived_from_coverage_start`）。本组喂的是**清单非空**
  // 的载荷——锁的是「起点一旦后移（将来又出现未实测段落），提示必须照实回来、并按后端
  // 下发的清单点名那几步」这条防线，**不是**当前 Windows 的形态。
  // 旧实现用一个整行布尔把整条提示撤下，等于宣称整条 Windows 流程都验过了。
  it("platform=windows：清单非空时弱提示照实上墙，并点名列尚未端到端跑过的四步", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe")
        return windowsProbe({
          windowsVerified: false,
          // 起点后移的**机制形态**（不是当前生产形态）：清单随起点派生，提示照实回来
          windowsVerifiedFrom: "shields_up",
          windowsUnverifiedSteps: ["detect", "download", "install", "login"],
        });
      return null;
    });
    render(<TailscaleWizard />);
    const hint = await screen.findByTestId("ts-windows-unverified");
    // 文案收窄到精确范围：只说这四步没端到端跑过，不再整行撤下
    expect(hint.textContent).toMatch(/not been run end-to-end/i);
    expect(hint.textContent).toContain("Detect Tailscale");
    expect(hint.textContent).toContain("Install Tailscale");
    expect(hint.textContent).toContain("Sign in to Tailscale");
    // **M1（2026-10-08 架构评审）**：文案必须**从起点派生**——起点名（本夹具 = shields_up）
    // 随载荷 windowsVerifiedFrom 下发并进文案；旧文案写死"只实测了后半段"，起点一挪就是
    // 假陈述。变异：把插值删掉 / 退回硬编码叙事 → 本条必红。
    expect(hint.textContent).toContain("Disable incoming-connection blocking");
    expect(hint.textContent).not.toMatch(/second half|后半段/i);
    // 未被点名的步骤（已实测的后半段）不得出现在"未跑过"清单里
    expect(hint.textContent).not.toContain("Enable Funnel (fixed URL)");
    expect(hint.textContent).not.toContain("Board reachability check");
  });

  // **M1（2026-10-08 架构评审）**：弱提示文案必须**从起点派生**（`{{from}}` 插值），不许再
  // 写死"只实测了后半段"——那个叙事只在起点恰好是后半段开头时成立，起点一挪即假陈述。
  it("M1：Windows 弱提示含起点插值，且两 locale 都不再写死「后半段」叙事", () => {
    const root = process.cwd();
    const zh = JSON.parse(readFileSync(path.join(root, "src/i18n/locales/zh.json"), "utf8"));
    const en = JSON.parse(readFileSync(path.join(root, "src/i18n/locales/en.json"), "utf8"));
    for (const [name, loc] of [
      ["zh", zh],
      ["en", en],
    ] as const) {
      const hintText = loc.settings.remote.tsWizard.windowsUnverified as string;
      expect(hintText, `${name} 必须含起点插值 {{from}}`).toContain("{{from}}");
      expect(hintText, `${name} 不得写死「后半段」叙事`).not.toMatch(/后半段|second half/i);
    }
  });

  it("macOS（本机平台）不显示 Windows 弱提示", async () => {
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    expect(screen.queryByTestId("ts-windows-unverified")).toBeNull();
  });

  it("Windows 整条流程都验过（windowsVerified=true ∧ 未验清单为空）→ 提示撤下", async () => {
    // **M7 夹具自检**：Windows 载荷**与生产同形**（生产恒下发起点 "detect"，见 Rust
    // `windows_verification_for` / `windows_verification_list_is_derived_from_coverage_start`）。
    // 夹具工厂一旦漂移回 `null`（那是**非 Windows** 的形态），本条即红——"拿生产不会出现的
    // 载荷做断言"这类失真，从注释挪进了可执行断言。
    expect(windowsProbe().windowsVerifiedFrom).toBe("detect");
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe")
        return windowsProbe({ windowsVerified: true, windowsUnverifiedSteps: [] });
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    expect(screen.queryByTestId("ts-windows-unverified")).toBeNull();
  });
});

// ③ 让用户知道 Tailscale 是什么（用户要求）：向导里此前没有任何地方解释它是什么，
// 而用户被要求装一个没听过的第三方软件并去它的官网登录一次。故向导页脚恒挂一行说明
// ——它是什么 + **为什么需要它**（固定私有地址 ⇒ 免自备域名的原理）+ 官网外链。
describe("TailscaleWizard ③向导页脚解释 Tailscale 是什么（含官网外链）", () => {
  it("页脚说明恒在（未装/配置中/恢复中任何挂载形态都回答「这是什么」），并说清「免域名」原理", async () => {
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    const line = screen.getByTestId("ts-about-tailscale");
    // 一句话说清它是免费的组网工具 + 为什么需要它（固定地址 ⇒ 不需要自备域名）
    expect(line.textContent).toMatch(/free/i);
    expect(line.textContent).toMatch(/domain/i);
  });

  it("外链安全惯例：href = tailscale.com + target=_blank + rel 同时含 noreferrer 与 noopener", async () => {
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    const link = within(screen.getByTestId("ts-about-tailscale")).getByRole(
      "link"
    ) as HTMLAnchorElement;
    expect(link.getAttribute("href")).toBe("https://tailscale.com/");
    expect(link.getAttribute("target")).toBe("_blank");
    const rel = link.getAttribute("rel") ?? "";
    expect(rel).toContain("noreferrer");
    expect(rel).toContain("noopener");
  });

  it("zh/en 双语齐备且两 locale 键集相等（新增键不许单边）", () => {
    const root = process.cwd();
    const zh = JSON.parse(readFileSync(path.join(root, "src/i18n/locales/zh.json"), "utf8"));
    const en = JSON.parse(readFileSync(path.join(root, "src/i18n/locales/en.json"), "utf8"));
    const flat = (obj: Record<string, unknown>, prefix = ""): string[] =>
      Object.entries(obj).flatMap(([k, v]) =>
        typeof v === "object" && v !== null
          ? flat(v as Record<string, unknown>, `${prefix}${k}.`)
          : [`${prefix}${k}`]
      );
    const zhKeys = new Set(flat(zh.settings.remote));
    const enKeys = new Set(flat(en.settings.remote));
    expect([...zhKeys].filter((k) => !enKeys.has(k))).toEqual([]);
    expect([...enKeys].filter((k) => !zhKeys.has(k))).toEqual([]);
    const zk = "tsWizard.aboutTailscale";
    expect(zhKeys.has(zk)).toBe(true);
    expect(enKeys.has(zk)).toBe(true);
    // 中文那句也要说清「免费」与「不需要域名」
    expect(zh.settings.remote.tsWizard.aboutTailscale).toMatch(/免费/);
    expect(zh.settings.remote.tsWizard.aboutTailscale).toMatch(/域名/);
  });
});

// ④ 下载安装包时给「在走动」的反馈（2026-10-07 用户裁决）：download 步是**一次同步
// 下载**（Rust 侧 `download_url_to` 一次性取回），期间前端此前完全没有动静——用户不知道
// 是不是卡住了。用户原话：「如果下载没有百分比，你可以给一个模拟进度条，表示在走动。
// 因为下载也不会花很长时间，就让用户知道没有卡住就行了。如果工作量很大，你也不用特意去
// 做一个准确的下载百分比条了。」
// 关键前提（已读码验证）：`remote_ts_run_step` 是 async + spawn_blocking（Rust 主线程/
// IPC 派发线程不被占用），`invoke()` 返回 Promise 不阻塞 JS 事件循环 ⇒ **纯前端动画够用**。
// 故：不确定进度指示器（转圈 + 「在走动」条，role=progressbar 无 aria-valuenow）+
// 文案；发起即显示、返回即收起；**绝不给假百分比**（与移动端「速率未知只显示已传字节，
// 不显示假百分比」同一条纪律）。
describe("TailscaleWizard ④下载步「在走动」反馈（不确定进度，不给假百分比）", () => {
  it("download 在途（IPC 未返回）即渲染不确定进度指示器——阻塞期前端仍能渲染；返回后收起", async () => {
    let release: (v: unknown) => void = () => {};
    invokeMock.mockImplementation(async (cmd: string, args?: { step?: string }) => {
      if (cmd === "remote_ts_probe") return macProbe();
      if (cmd === "remote_ts_run_step" && args?.step === "download")
        // 挂住不 resolve：模拟 Rust 侧正在同步下载（IPC 尚未返回）
        return await new Promise((r) => {
          release = r;
        });
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    // 未发起时没有指示器
    expect(screen.queryByTestId("ts-step-progress")).toBeNull();
    fireEvent.click(within(stepRow("download")).getByRole("button", { name: /run/i }));
    // **核心断言**：invoke 仍挂着，但界面已经显示「在走动」——证明下载阻塞期间前端能渲染
    const bar = await within(stepRow("download")).findByTestId("ts-step-progress");
    expect(bar.getAttribute("role")).toBe("progressbar");
    // 不确定态：没有 aria-valuenow（有值就是"假百分比"的前身）
    expect(bar.getAttribute("aria-valuenow")).toBeNull();
    expect(bar.textContent).toMatch(/downloading/i);
    // 纪律：不知道总量就不给百分比
    expect(bar.textContent).not.toMatch(/%|\d+\s*\/\s*\d+/);
    // 步骤返回 → 收起
    release({ ok: true });
    await waitFor(() => expect(screen.queryByTestId("ts-step-progress")).toBeNull());
  });

  it("install 在途也给走动反馈（msiexec/installer 等待期同样没有其它动静）", async () => {
    let release: (v: unknown) => void = () => {};
    invokeMock.mockImplementation(async (cmd: string, args?: { step?: string }) => {
      if (cmd === "remote_ts_probe") return macProbe();
      if (cmd === "remote_ts_run_step" && args?.step === "install")
        return await new Promise((r) => {
          release = r;
        });
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    fireEvent.click(within(stepRow("install")).getByRole("button", { name: /run/i }));
    const bar = await within(stepRow("install")).findByTestId("ts-step-progress");
    expect(bar.textContent).toMatch(/installing/i);
    expect(bar.textContent).not.toMatch(/%/);
    release({ ok: true });
    await waitFor(() => expect(screen.queryByTestId("ts-step-progress")).toBeNull());
  });

  it("非长阻塞步（shields_up）不挂走动反馈——不给每一步都加噪音", async () => {
    invokeMock.mockImplementation(async (cmd: string, args?: { step?: string }) => {
      if (cmd === "remote_ts_probe") return macProbe();
      if (cmd === "remote_ts_run_step" && args?.step === "shields_up")
        return await new Promise((r) => setTimeout(() => r({ ok: true }), 0));
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    fireEvent.click(within(stepRow("shields_up")).getByRole("button", { name: /run/i }));
    expect(within(stepRow("shields_up")).queryByTestId("ts-step-progress")).toBeNull();
  });
});

// ⑤ 登录步必须**可点**（2026-10-07 用户实测：登录那一步没有可点的东西，只能自己去客户端
// 手动登录）。诊断（读码 + 注入缝复现）：新装机器上 `status --json` 是
// `NeedsLogin ∧ AuthURL=""`——授权链接由尾网在**发起一次交互式登录**时才生成（tailscale
// login / GUI 的「Log in」按钮），而 兔维斯 此前从不发起（全仓 run_cli 调用点无 up/login）；
// 前端又只在 `probe.authUrl` 非空时才渲染链接 ⇒ 登录行里有"需要你操作：去浏览器登录"的
// 文案，却没有任何可点的东西。修法：兔维斯 **主动取链接**（后端后台发起 + 有界轮询，见 Rust
// `login_step_with`），前端把链接做成按钮、拿不到时给明确的下一步。
// 合规红线不变：兔维斯 **只递链接，不代登录、不持凭据**（登录始终在浏览器由用户完成）。
describe("TailscaleWizard ⑤登录步可点（新装 AuthURL 为空不再是死胡同）", () => {
  const noLinkProbe = () => macProbe({ authUrl: "" });

  it("authUrl 为空：登录行给出明确的「Get login link」按钮 + 下一步文案（旧实现此处空无一物）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe") return noLinkProbe();
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    const btn = within(stepRow("login")).getByTestId("ts-login-link-btn");
    expect(btn.tagName).toBe("BUTTON");
    expect(btn.textContent).toMatch(/get login link/i);
    // 拿不到链接时给**明确的下一步**（不是让用户自己去客户端里找）
    expect(within(stepRow("login")).getByTestId("ts-login-hint").textContent).toMatch(
      /tailscale app/i
    );
    // 未点击前不发任何 run_step（不偷跑登录尝试）
    expect(invokeMock.mock.calls.filter((c) => c[0] === "remote_ts_run_step").length).toBe(0);
  });

  it("点按钮 → remote_ts_run_step(login)（兔维斯 只递链接不代登录）；回执链接在探测落地前立即上墙", async () => {
    // **M4（2026-10-08 架构评审）**：回执是**过渡态**（"点了立刻上墙"），权威源仍是探测。
    // 本用例把动作后的那次重探**挂住**，模拟"探测在途"的真实窗口 ⇒ 回执的链接必须先上墙
    // （否则用户点了没反应）；随后释放探测，断言上墙的换成**探测那条**（回执让位）。
    let releaseProbe: (v: unknown) => void = () => {};
    let probes = 0;
    invokeMock.mockImplementation(async (cmd: string, args?: { step?: string }) => {
      if (cmd === "remote_ts_probe") {
        probes += 1;
        if (probes === 1) return noLinkProbe(); // 挂载：无链接
        return await new Promise((r) => {
          releaseProbe = r; // 动作后的重探：挂住（在途）
        });
      }
      if (cmd === "remote_ts_run_step" && args?.step === "login")
        return { ok: true, authUrl: "https://login.tailscale.com/a/fetched", triggered: true };
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    fireEvent.click(within(stepRow("login")).getByTestId("ts-login-link-btn"));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("remote_ts_run_step", { step: "login" })
    );
    const link = await within(stepRow("login")).findByTestId("ts-auth-link");
    expect(link.getAttribute("href")).toBe("https://login.tailscale.com/a/fetched");
    expect(link.getAttribute("target")).toBe("_blank");
    expect(link.getAttribute("rel")).toContain("noreferrer");
    // 有链接就有明路：兜底文案收起
    expect(within(stepRow("login")).queryByTestId("ts-login-hint")).toBeNull();
    // 探测落地（权威源给的是另一条）⇒ 上墙的必须是探测那条，回执作废
    releaseProbe(macProbe({ authUrl: "https://login.tailscale.com/a/from-probe" }));
    await waitFor(() =>
      expect(within(stepRow("login")).getByTestId("ts-auth-link").getAttribute("href")).toBe(
        "https://login.tailscale.com/a/from-probe"
      )
    );
  });

  // **M4（2026-10-08 架构评审）**：旧实现的回执链接**没有失效机制**——`probe.authUrl`
  // 为空（后端已不再持有那条链接：轮换/失效/尾网重来）时仍拿旧回执硬撑上墙，与"探测是
  // 权威源"的注释矛盾。修法：回执只在**它之后还没有新探测落地**的窗口里有效；下一次探测
  // 一落地就以探测为准（哪怕是"没有链接"）。
  it("M4：探测（权威源）落地说没有链接 ⇒ 回执链接作废，不再硬撑", async () => {
    invokeMock.mockImplementation(async (cmd: string, args?: { step?: string }) => {
      if (cmd === "remote_ts_probe") return noLinkProbe(); // 权威源**始终**没有链接
      if (cmd === "remote_ts_run_step" && args?.step === "login")
        return { ok: true, authUrl: "https://login.tailscale.com/a/stale", triggered: true };
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    fireEvent.click(within(stepRow("login")).getByTestId("ts-login-link-btn"));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("remote_ts_run_step", { step: "login" })
    );
    // 动作后的重探已落地（权威源：无链接）⇒ 那条回执不许继续当链接挂着
    await waitFor(() => expect(within(stepRow("login")).queryByTestId("ts-auth-link")).toBeNull());
    // 回到"拿不到链接"的如实呈现：明路仍是「获取登录链接」+ 兜底文案（不是无可点之物）
    expect(within(stepRow("login")).getByTestId("ts-login-link-btn")).toBeTruthy();
    expect(within(stepRow("login")).getByTestId("ts-login-hint")).toBeTruthy();
  });

  // **I3（2026-10-08 架构评审）**：登录行的琥珀动作文案必须**指向真实存在的按钮**。
  // 旧形态两种（无链接 / 有链接）共用 `actLogin` =「点击「去登录」」，而「去登录」按钮
  // **只在 loginLink 非空时渲染** ⇒ 无链接时行内只有「获取登录链接」，同一行两条矛盾指令
  // （正是 ⑤ 要消灭的"无可点之物"的降级版）。修法：无链接时换一个键，且文案里点名的就是
  // 行内那个真按钮。
  it("I3：无链接时动作文案指向「Get login link」（行内真实存在的按钮），不指向渲染不出来的「Sign in」", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe") return noLinkProbe();
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    const row = stepRow("login");
    const action = within(row).getByText(/your action/i);
    // 行内没有「Sign in」按钮（无链接 ⇒ 不渲染），文案不得让用户去点它
    expect(within(row).queryByTestId("ts-auth-link")).toBeNull();
    expect(action.textContent).not.toMatch(/click\s+“?sign in/i);
    // 文案必须点名行内**真的存在**的那个按钮（比对按钮自身的文案，不写死测试侧字面量）
    const realButton = within(row).getByTestId("ts-login-link-btn");
    expect(realButton.textContent).toBeTruthy();
    expect(action.textContent).toContain(realButton.textContent!);
  });

  it("I3：有链接时动作文案仍指向「Sign in」，且那个按钮确实在行内", async () => {
    render(<TailscaleWizard />); // 默认 macProbe 带 authUrl
    await screen.findByText("Detect Tailscale");
    const row = stepRow("login");
    const action = within(row).getByText(/your action/i);
    const signIn = within(row).getByTestId("ts-auth-link");
    expect(signIn.textContent).toBeTruthy();
    expect(action.textContent).toContain(signIn.textContent!);
  });

  it("点按钮在途期间也给走动反馈（后端最长 9.5s 取链接，同样要看得见 兔维斯 去要了）", async () => {
    let release: (v: unknown) => void = () => {};
    invokeMock.mockImplementation(async (cmd: string, args?: { step?: string }) => {
      if (cmd === "remote_ts_probe") return noLinkProbe();
      if (cmd === "remote_ts_run_step" && args?.step === "login")
        return await new Promise((r) => {
          release = r;
        });
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    fireEvent.click(within(stepRow("login")).getByTestId("ts-login-link-btn"));
    const bar = await within(stepRow("login")).findByTestId("ts-step-progress");
    expect(bar.textContent).toMatch(/getting the login link/i);
    expect(bar.textContent).not.toMatch(/%/);
    release({ ok: true, authUrl: "https://login.tailscale.com/a/x" });
    await waitFor(() =>
      expect(within(stepRow("login")).queryByTestId("ts-step-progress")).toBeNull()
    );
  });

  it("焦点回到窗口时重探一次（在浏览器里登录完切回来 → 登录步自动翻已完成；不做时刻轮询）", async () => {
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    // 夹具自检：默认载荷是"有链接、登录未完成"——这正是用户会离开窗口的那一态
    expect(within(stepRow("login")).getByTestId("ts-auth-link")).toBeTruthy();
    const before = invokeMock.mock.calls.filter((c) => c[0] === "remote_ts_probe").length;
    act(() => {
      window.dispatchEvent(new Event("focus"));
    });
    await waitFor(() => {
      const after = invokeMock.mock.calls.filter((c) => c[0] === "remote_ts_probe").length;
      expect(after).toBeGreaterThan(before);
    });
  });

  // **M5（2026-10-08 架构评审）**：focus 复评的作用域**收窄到"链接已在屏幕上"这一窗口**。
  // 旧判据只有"登录步未完成"，于是"已装未登录"的整段时间里每次聚焦都白跑一轮探测
  // （一轮 = 3 个 CLI 派生：status / get / funnel status）——用户切个窗口回来就付这个代价。
  // 用户路径（点链接 → 浏览器登录 → 切回）**必然先有链接**；没有链接时手上还有
  // 「获取登录链接」与「刷新状态」两条明路，不需要后台偷跑 CLI。
  it("M5：无链接（还没到可点之处）时 focus 不重探——作用域收窄，不白跑 CLI", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe") return macProbe({ authUrl: "" });
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    expect(within(stepRow("login")).queryByTestId("ts-auth-link")).toBeNull();
    const before = invokeMock.mock.calls.filter((c) => c[0] === "remote_ts_probe").length;
    act(() => {
      window.dispatchEvent(new Event("focus"));
    });
    // 给微任务/副作用一个落地机会再判"没有新增探测"
    await act(async () => {
      await Promise.resolve();
    });
    const after = invokeMock.mock.calls.filter((c) => c[0] === "remote_ts_probe").length;
    expect(after).toBe(before);
  });

  it("M5：登录已完成（登录步 done）时 focus 也不重探", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe")
        return macProbe({
          states: macProbe().states.map((s: { id: string; done: boolean }) =>
            s.id === "login" ? { ...s, done: true } : s
          ),
        });
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    const before = invokeMock.mock.calls.filter((c) => c[0] === "remote_ts_probe").length;
    act(() => {
      window.dispatchEvent(new Event("focus"));
    });
    await act(async () => {
      await Promise.resolve();
    });
    const after = invokeMock.mock.calls.filter((c) => c[0] === "remote_ts_probe").length;
    expect(after).toBe(before);
  });

  it("登录已完成 → 不渲染「获取登录链接」按钮，也不挂兜底文案", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe")
        return macProbe({
          authUrl: "",
          states: macProbe().states.map(
            (s: { id: string; done: boolean; blockedReason: string | null }) =>
              s.id === "login" ? { ...s, done: true } : s
          ),
        });
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    expect(within(stepRow("login")).queryByTestId("ts-login-link-btn")).toBeNull();
    expect(within(stepRow("login")).queryByTestId("ts-login-hint")).toBeNull();
  });
});

describe("TailscaleWizard A1：Funnel 批准是「可能不出现」的分支，不是必经步骤", () => {
  it("可选人工步（humanOptional）带「可能无需此步」弱提示；必需人工步不带", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe")
        return macProbe({
          steps: [
            step("detect"),
            step("download"),
            step("install", true, "settings.remote.tsWizard.actAdmin"),
            step("sys_ext", true, "settings.remote.tsWizard.actSysExt"),
            step("login", true, "settings.remote.tsWizard.actLogin"),
            step("shields_up"),
            step("funnel", true, "settings.remote.tsWizard.actFunnel", true),
            step("verify"),
            step("autostart"),
          ],
        });
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    const optional = within(stepRow("funnel")).getByTestId("ts-optional-step");
    // 文案必须说清「本平台/本尾网可能无需此步」——不能写成"等你点批准"
    expect(optional.textContent).toMatch(/may not be needed/i);
    // 必需人工步不得带这个提示（否则用户以为可以不点）
    expect(within(stepRow("install")).queryByTestId("ts-optional-step")).toBeNull();
    expect(within(stepRow("login")).queryByTestId("ts-optional-step")).toBeNull();
  });

  it("funnel 回执无批准链接（Windows 实测形态）→ 无批准提示、无错误行、不阻塞", async () => {
    invokeMock.mockImplementation(async (cmd: string, args?: { step?: string }) => {
      if (cmd === "remote_ts_probe") return macProbe();
      if (cmd === "remote_ts_run_step" && args?.step === "funnel")
        return { ok: true, approvalUrl: null };
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    fireEvent.click(within(stepRow("funnel")).getByRole("button", { name: /run/i }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("remote_ts_run_step", { step: "funnel" })
    );
    expect(screen.queryByTestId("ts-approval")).toBeNull();
    expect(within(stepRow("funnel")).queryByTestId("ts-step-error")).toBeNull();
  });
});

describe("TailscaleWizard 写路径未实测弱提示（B2：不得把未验证伪装成已验证）", () => {
  it("writePathVerified=false（macOS 现状）→ 顶部弱提示上墙，且说清是「写路径（开通/撤销）」", async () => {
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    const hint = screen.getByTestId("ts-writepath-unverified");
    expect(hint.textContent).toMatch(/write path/i);
    // M-4：macOS 那份"只读探测已实测"的表述只在 macOS 上成立（Linux/Other 会张冠李戴）
    expect(hint.textContent).toMatch(/read-only probes are verified/i);
  });

  it("M-4：非 macOS 且写路径未验证（Linux/Other）→ 文案不得自称「只读探测已实测」", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe")
        return macProbe({ platform: "other", writePathVerified: false });
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    const hint = screen.getByTestId("ts-writepath-unverified");
    expect(hint.textContent).toMatch(/not been probed on this platform|no probing/i);
    // 本平台零探测：不得出现 macOS 的实测声明（张冠李戴）
    expect(hint.textContent).not.toMatch(/read-only probes are verified/i);
  });

  it("writePathVerified=true（Windows 已实测三条写路径）→ 不显示弱提示", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe") return macProbe({ writePathVerified: true });
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    expect(screen.queryByTestId("ts-writepath-unverified")).toBeNull();
  });

  it("Windows 整条流程都验过（windowsVerified=true）→ 不再显示「Windows 未校验」旧提示", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe")
        return windowsProbe({ windowsVerified: true, windowsUnverifiedSteps: [] });
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    expect(screen.queryByTestId("ts-windows-unverified")).toBeNull();
  });
});

describe("TailscaleWizard 单步触发（remote_ts_run_step）", () => {
  it("可执行步出「Run」按钮，点击调 remote_ts_run_step 并在回执后重探一次", async () => {
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    const probesBefore = invokeMock.mock.calls.filter((c) => c[0] === "remote_ts_probe").length;
    fireEvent.click(within(stepRow("shields_up")).getByRole("button", { name: /run/i }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("remote_ts_run_step", { step: "shields_up" })
    );
    // 回执后重探（GET 时机 = 挂载/动作后，不做时刻轮询）
    await waitFor(() => {
      const probes = invokeMock.mock.calls.filter((c) => c[0] === "remote_ts_probe").length;
      expect(probes).toBe(probesBefore + 1);
    });
  });

  it("detect / autostart 不出执行按钮（只读探针 / 兔维斯 自身行为）；verify 走专属重试钮", async () => {
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    for (const id of ["detect", "autostart"]) {
      expect(within(stepRow(id)).queryByRole("button")).toBeNull();
    }
    // verify 未验时出「Retry」重试按钮（调 remote_ts_run_step("verify")，见下组用例）
    expect(within(stepRow("verify")).getByTestId("ts-verify-retry")).toBeTruthy();
  });

  it("login 步：兔维斯 不代登录——「去登录」为 authUrl 链接（href 原样透传）", async () => {
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    const link = within(stepRow("login")).getByTestId("ts-auth-link") as HTMLAnchorElement;
    expect(link.getAttribute("href")).toBe("https://login.tailscale.com/a/abc123");
    // 链接不是触发命令的按钮——login 步零 invoke
    expect(invokeMock.mock.calls.filter((c) => c[0] === "remote_ts_run_step").length).toBe(0);
  });

  it("funnel 回执带批准链接 → 展示批准提示与链接（兔维斯 只递不代点）", async () => {
    invokeMock.mockImplementation(async (cmd: string, args?: { step?: string }) => {
      if (cmd === "remote_ts_probe") return macProbe();
      if (cmd === "remote_ts_run_step" && args?.step === "funnel")
        return { ok: true, approvalUrl: "https://login.tailscale.com/funnel/abc" };
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    fireEvent.click(within(stepRow("funnel")).getByRole("button", { name: /run/i }));
    const approval = await screen.findByTestId("ts-approval");
    const link = approval.querySelector("a") as HTMLAnchorElement;
    expect(link.getAttribute("href")).toBe("https://login.tailscale.com/funnel/abc");
  });
});

describe("TailscaleWizard verify §C3（尚未生效 + 重试，Task 7）", () => {
  it('reach=failed：verify 行显示「固定地址尚未生效」+ reason + 重试按钮；点重试调 remote_ts_run_step("verify")', async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe")
        return macProbe({
          reach: { state: "failed", reason: "公网解析不到该地址" },
          states: [
            state("detect", true),
            state("download", false),
            state("install", false),
            state("sys_ext", true),
            state("login", false),
            state("shields_up", false),
            state("funnel", false),
            state("verify", false, "公网解析不到该地址"),
            state("autostart", true),
          ],
        });
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    const row = stepRow("verify");
    // 不得只显示泛用 blocked 行：必须是「尚未生效」口径 + reason（不让用户干等假地址）
    const notLive = within(row).getByTestId("ts-not-live");
    expect(notLive.textContent).toContain("Fixed address not live yet");
    expect(notLive.textContent).toContain("公网解析不到该地址");
    expect(within(row).queryByText("Done")).toBeNull();
    // 非连不通类失败 → 不挂瞬时抖动引导（那条只属于「地址已解析但连不通」）
    expect(within(row).queryByTestId("ts-transient-hint")).toBeNull();
    // 重试按钮触发 verify 步（后端自动含自愈）
    fireEvent.click(within(row).getByTestId("ts-verify-retry"));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("remote_ts_run_step", { step: "verify" })
    );
  });

  // 2026-10-13 真机定位：连不通类失败 = 公网链路瞬时抖动（60s 轮询自愈），重试门槛
  // 与安抚文案跟上——reason 前缀「地址已解析但连不通」锚定（与 reach.rs 格式串同步）
  it("reach=failed 且连不通类 reason → 「尚未生效」下挂瞬时抖动引导（ts-transient-hint）", async () => {
    const unreachable =
      "REACH_ADDR_UNREACHABLE: 地址已解析但连不通（试了 2 条）: 103.84.155.153: error sending request";
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe")
        return macProbe({
          reach: { state: "failed", reason: unreachable },
          states: [
            state("detect", true),
            state("download", false),
            state("install", false),
            state("sys_ext", true),
            state("login", false),
            state("shields_up", false),
            state("funnel", false),
            state("verify", false, unreachable),
            state("autostart", true),
          ],
        });
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    const row = stepRow("verify");
    expect(within(row).getByTestId("ts-not-live")).toBeTruthy();
    const hint = within(row).getByTestId("ts-transient-hint");
    expect(hint.textContent).toMatch(/transient jitter/i);
    expect(hint.textContent).toMatch(/1[–-]2 minutes/i);
  });

  it("reach=verified：verify 行 Done 且无重试按钮；头部看板地址上墙", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe")
        return macProbe({
          running: true,
          boardUrl: "https://a.ts.net/m",
          reach: { state: "verified" },
          states: [
            state("detect", true),
            state("download", false),
            state("install", false),
            state("sys_ext", true),
            state("login", false),
            state("shields_up", false),
            state("funnel", false),
            state("verify", true),
            state("autostart", true),
          ],
        });
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    const row = stepRow("verify");
    expect(row.getAttribute("data-done")).toBe("true");
    expect(within(row).queryByTestId("ts-verify-retry")).toBeNull();
    expect(within(row).getByText("Done")).toBeTruthy();
    // 头部地址只在 Verified 显示
    expect(screen.getByText("https://a.ts.net/m")).toBeTruthy();
  });

  it("reach 未验证：running + boardUrl 在场也不显示头部地址（不把未验证地址当可用展示）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe")
        return macProbe({
          running: true,
          boardUrl: "https://a.ts.net/m",
          reach: { state: "unverified" },
        });
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    expect(screen.queryByText("https://a.ts.net/m")).toBeNull();
  });

  it("A2：通道在跑但记录未发布 → 给带预期时长的提示（不让用户干等）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe")
        return macProbe({
          running: true,
          boardUrl: "https://a.ts.net/m",
          reach: { state: "unverified" },
        });
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    // 实测：首次开通后公网记录约 5–6 分钟才发布，这期间用户唯一感受是"连不上"——
    // 必须告知这是正常现象与预期时长（旧文案只说「尚未生效」，不给时长）
    const hint = screen.getByTestId("ts-pending");
    expect(hint.textContent).toMatch(/5–6 minutes/i);
    expect(hint.textContent).toMatch(/not published yet/i);
  });

  // W-B（真机实测三档时长）：固定一句 5 分钟会把 reset 后重开（实测 30–49 秒）与
  // 开机恢复（记录不撤销、只需等后端重连 1–2 分钟）都误导。文案按**档位**取，
  // 档位由后端载荷给出（reach.state + republish），前端不猜成因。
  it("W-B：reach=record_pending 且 republish=false → 首次开通档（5–6 分钟）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe")
        return macProbe({
          running: true,
          boardUrl: "https://a.ts.net/m",
          reach: { state: "record_pending", republish: false },
        });
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    const hint = screen.getByTestId("ts-pending");
    expect(hint.textContent).toMatch(/5–6 minutes/i);
    // M3：首开档照线稿（357 行）**不带** badge——两档不得一律加徽标
    expect(hint.querySelector("span")).toBeNull();
    expect(screen.queryByTestId("ts-republish")).toBeNull();
  });

  it("W-B：reach=record_pending 且 republish=true → 重新开通档（30 秒～1 分钟），不套用 5 分钟", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe")
        return macProbe({
          running: true,
          boardUrl: "https://a.ts.net/m",
          reach: { state: "record_pending", republish: true },
        });
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    const hint = screen.getByTestId("ts-republish");
    expect(hint.textContent).toMatch(/30 seconds|1 minute/i);
    expect(hint.textContent).not.toMatch(/5–6 minutes/i);
    // M3（2026-10-07 评审）：线稿状态二把「发布中」画成**带琥珀 badge** 的行（wireframe
    // 355），实现必须同形——否则线稿与实现各说各话
    expect(hint.querySelector("span")?.textContent).toBe("Republishing");
    expect(hint.querySelector("span")?.className).toMatch(/amber/);
    // 两档互斥：重开档不得同时挂首开档那块
    expect(screen.queryByTestId("ts-pending")).toBeNull();
  });

  it("W-A：reach=recovering（开机恢复窗口）→ 「恢复中」语义：不是故障、也不是「域名生效中」", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_ts_probe")
        return macProbe({
          running: false,
          boardUrl: null,
          reach: { state: "recovering" },
        });
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    const hint = screen.getByTestId("ts-recovering");
    expect(hint.textContent).toMatch(/reconnecting/i);
    expect(hint.textContent).toMatch(/1[–-]2 minutes/i);
    expect(hint.textContent).toMatch(/nothing to do|no action/i);
    // M3：线稿状态二 354 行同形琥珀 badge（与状态五 tsRecoveringBadge 同一档语义）
    expect(hint.querySelector("span")?.textContent).toBe("Reconnecting");
    expect(hint.querySelector("span")?.className).toMatch(/amber/);
    // 成因不得指向「域名发布」（重启后记录不撤销），也不得报成故障/尚未生效
    expect(screen.queryByTestId("ts-pending")).toBeNull();
    expect(screen.queryByTestId("ts-republish")).toBeNull();
    expect(screen.queryByTestId("ts-not-live")).toBeNull();
    expect(screen.queryByText("https://a.ts.net/m")).toBeNull();
  });

  it("A2：通道没在跑（校验态未验证）时不显示「生效中」提示——成因不谎报", async () => {
    render(<TailscaleWizard />); // macProbe 默认 running=false
    await screen.findByText("Detect Tailscale");
    expect(screen.queryByTestId("ts-pending")).toBeNull();
  });

  it("run_step 失败（Err reject）：错误落到对应步骤行的可见错误行（Task 6 移交指针：不再零反馈）", async () => {
    invokeMock.mockImplementation(async (cmd: string, args?: { step?: string }) => {
      if (cmd === "remote_ts_probe") return macProbe();
      if (cmd === "remote_ts_run_step" && args?.step === "funnel")
        throw "检测到非 兔维斯 的 Tailscale serve/Funnel 配置，为避免覆盖已中止开通";
      return null;
    });
    render(<TailscaleWizard />);
    await screen.findByText("Detect Tailscale");
    fireEvent.click(within(stepRow("funnel")).getByRole("button", { name: /run/i }));
    const err = await within(stepRow("funnel")).findByTestId("ts-step-error");
    expect(err.textContent).toContain("Action failed");
    expect(err.textContent).toContain("检测到非 兔维斯");
  });
});

describe("tailscale 卡展开区集成（Task 9 收敛：向导挂进卡片详情区，独立入口行已撤）", () => {
  const remoteStatusPayload = () => ({
    enabled: false,
    maxDevices: 10,
    channels: {
      local: { running: false, address: "http://127.0.0.1:9420/m" },
      lan: { enabled: false, running: false, addresses: [] },
      quick: { enabled: false, running: false, address: null, error: null },
      named: { enabled: false, running: false, address: null, error: null },
      tailscale: {
        enabled: false,
        running: false,
        address: null,
        error: null,
        reach: { state: "unverified" },
      },
    },
    pin: "4827",
    host: { name: "matebook16s", platform: "mac", version: "0.5.0", bootId: "boot-x" },
    enabledTools: [],
  });
  // detect 未完成 = 未安装态（卡面出「Set up in one click」入口）；autostart 恒完成
  const notInstalledProbe = () =>
    macProbe({
      states: macProbe().states.map(
        (s: { id: string; done: boolean; blockedReason: string | null }) =>
          s.id === "autostart" ? s : { ...s, done: false }
      ),
    });

  it("未点开 tailscale 卡不发起 remote_ts_probe（不偷跑探测）；点卡片 → 展开区拉取探测载荷 → 未安装态 → 点一键配置展开向导", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status") return remoteStatusPayload();
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      if (cmd === "remote_ts_probe") return notInstalledProbe();
      return null;
    });
    render(<RemoteSection />);
    expect(invokeMock.mock.calls.filter((c) => c[0] === "remote_ts_probe").length).toBe(0);
    fireEvent.click(document.querySelector('[data-card="tailscale"]')!);
    await waitFor(() =>
      expect(invokeMock.mock.calls.some((c) => c[0] === "remote_ts_probe")).toBe(true)
    );
    const phase = await screen.findByTestId("ts-phase");
    // data-phase 由展开区的探测回执**派生**（ts-phase 元素先挂、属性后更）：等它落定再读，
    // 否则读到的是回执到达前的 "unknown"。2026-10-07 定位：把 mock 回执人为延后即 100%
    // 复现（同一类"读到回执到达之前"）；等条件后 100% 绿。断言强度不变（仍要求
    // notInstalled——真回归 / 相位判错照样红）。
    await waitFor(() => expect(phase.getAttribute("data-phase")).toBe("notInstalled"));
    // 向导本体先不挂；一键配置按钮 → 向导上墙
    expect(screen.queryByTestId("ts-wizard")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: /set up in one click/i }));
    expect(await screen.findByTestId("ts-wizard")).toBeTruthy();
  });

  it("全步骤完成且校验通过 → 已配置态直接呈现固定地址（无入口行、无向导本体）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status")
        return {
          ...remoteStatusPayload(),
          channels: {
            ...remoteStatusPayload().channels,
            tailscale: {
              enabled: true,
              running: true,
              address: "https://mam.tail1234.ts.net/m",
              error: null,
              reach: { state: "verified" },
            },
          },
        };
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      if (cmd === "remote_ts_probe")
        return macProbe({
          states: macProbe().states.map((s: { id: string }) => ({ ...s, done: true })),
          running: true,
          boardUrl: "https://mam.tail1234.ts.net/m",
          reach: { state: "verified" },
        });
      return null;
    });
    render(<RemoteSection />);
    fireEvent.click(document.querySelector('[data-card="tailscale"]')!);
    const phase = await screen.findByTestId("ts-phase");
    await waitFor(() => expect(phase.getAttribute("data-phase")).toBe("configured"));
    expect(screen.getByText("https://mam.tail1234.ts.net/m")).toBeTruthy();
    expect(screen.queryByTestId("ts-wizard")).toBeNull();
  });
});

// i18n 契约（两级感知版）：TailscaleWizard.tsx 源码引用的每个 settings.remote.* 键
//（含 tsWizard 两级子键）必须在 zh 与 en 同齐备；两 locale 的 settings.remote 子树
// 键集必须相等。RemoteSection 的旧扫描只认单层键——本组件的字面量由本测试把关。
describe("TailscaleWizard i18n zh/en（tsWizard 两级键）", () => {
  // vitest jsdom 环境下 import.meta.url 非 file 协议，用进程 cwd（vitest 以仓库根启动）
  const root = process.cwd();
  const flat = (obj: Record<string, unknown>, prefix = ""): string[] =>
    Object.entries(obj).flatMap(([k, v]) =>
      typeof v === "object" && v !== null
        ? flat(v as Record<string, unknown>, `${prefix}${k}.`)
        : [`${prefix}${k}`]
    );

  it("组件引用键双语齐备 + 两 locale 子树键集相等 + 后端下发的四个动作键在场", () => {
    const zh = JSON.parse(readFileSync(path.join(root, "src/i18n/locales/zh.json"), "utf8"));
    const en = JSON.parse(readFileSync(path.join(root, "src/i18n/locales/en.json"), "utf8"));
    const zhKeys = new Set(flat(zh.settings.remote).map((k) => `settings.remote.${k}`));
    const enKeys = new Set(flat(en.settings.remote).map((k) => `settings.remote.${k}`));
    // 两 locale 键集相等（settings.remote 全子树，含 tsWizard）
    expect([...zhKeys].filter((k) => !enKeys.has(k))).toEqual([]);
    expect([...enKeys].filter((k) => !zhKeys.has(k))).toEqual([]);

    // 两级感知扫描组件源码的字面量键
    const text = readFileSync(
      path.join(root, "src/components/settings/TailscaleWizard.tsx"),
      "utf8"
    );
    const used = new Set<string>();
    for (const m of text.matchAll(/settings\.remote\.[A-Za-z0-9_.]+[A-Za-z0-9_]/g)) {
      used.add(m[0]);
    }
    expect(used.size).toBeGreaterThan(20);
    expect([...used].filter((k) => !zhKeys.has(k))).toEqual([]);
    expect([...used].filter((k) => !enKeys.has(k))).toEqual([]);

    // Rust 端 wizard_steps 的 human_action_key 五值（随载荷下发，前端 t() 动态翻译）。
    // 第 5 个（actAdminWinMsi）是 2026-10-08 的安装路径分叉：Windows 的 MSI 会让用户
    // 自选安装路径 ⇒ 动作文案点明「用默认路径最省事」；macOS 仍走 actAdmin。
    for (const k of [
      "settings.remote.tsWizard.actAdmin",
      "settings.remote.tsWizard.actAdminWinMsi",
      "settings.remote.tsWizard.actSysExt",
      "settings.remote.tsWizard.actLogin",
      "settings.remote.tsWizard.actFunnel",
    ]) {
      expect(zhKeys.has(k)).toBe(true);
      expect(enKeys.has(k)).toBe(true);
    }
  });

  // **I3（2026-10-08 架构评审）**：登录行的动作文案必须指向**行内真实存在**的按钮——两
  // locale 都要。有链接 ⇒ 行内是「去登录」（openAuthUrl）；无链接 ⇒ 行内只有「获取登录
  // 链接」（getLoginLink），故无链接那条键必须点名**后者**。语言级锁（渲染级锁见 ⑤ 组）。
  it("I3：两 locale 的登录动作文案各自点名本 locale 真正会渲染的那个按钮", () => {
    const zh = JSON.parse(readFileSync(path.join(root, "src/i18n/locales/zh.json"), "utf8"));
    const en = JSON.parse(readFileSync(path.join(root, "src/i18n/locales/en.json"), "utf8"));
    for (const loc of [zh, en]) {
      const w = loc.settings.remote.tsWizard;
      expect(w.actLogin).toContain(w.openAuthUrl); // 有链接 ⇒ 指向「去登录」
      expect(w.actLoginNoLink).toContain(w.getLoginLink); // 无链接 ⇒ 指向「获取登录链接」
      // 无链接那条**不得**再叫用户点「去登录」（那个按钮此刻根本没渲染）
      expect(w.actLoginNoLink).not.toContain(w.openAuthUrl);
    }
  });
});
