// tests/settings/remoteSection.test.tsx — §C4（零豁免计划 Task 9）：通道区重排为
// 4 张对外卡片（局域网连接 / 临时隧道 / 自有域名 / 外部域名），「本机」
// 不再是一条通道（它是访问方式，走局域网卡地址）。线稿 v6（wireframes/
// 2026-09-17-remote-settings-redesign.html）为唯一 UI 契约。
// 覆盖：四卡渲染与状态点映射（评审 C-I1：状态点 = running、开关本体 = enabled）/ 本机不在卡片区 / 点卡片唯一展开
// （默认局域网）/ lan 关闭态显式「未开启」/ lan 开关 P7 Dialog 流（Err 特征文案 →
// 确认 → 重试）/ quick 换址警告 / 教程 popover 开收 / tailscale 卡三态（未安装 /
// 配置中 / 已配置）/ PIN 随机与保存与重置设备确认 / 设备行 via 五值徽标与重命名与
// 踢下线 / N / 10 上限徽标 / 本机名称默认系统名 / i18n zh-en 无缺键。
// mock 模式沿用本文件旧版（vi.hoisted + vi.mock，自带 invoke/event/sonner/qrcode）。
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import path from "node:path";

const { invokeMock, toastSuccessMock, toastErrorMock, toDataUrlMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  toastSuccessMock: vi.fn(),
  toastErrorMock: vi.fn(),
  toDataUrlMock: vi.fn(),
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
// useAppTranslation 内部 listen("@tauri-apps/api/event") 在 jsdom 无 Tauri 内核，须 mock
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
  emit: vi.fn(async () => {}),
}));
// sonner mock 须放文件顶部（vi.mock 提升语义，见旧版注释）
vi.mock("sonner", () => ({
  toast: Object.assign(vi.fn(), {
    info: vi.fn(),
    success: toastSuccessMock,
    error: toastErrorMock,
  }),
}));
// jsdom 无 canvas 内核，qrcode.toDataURL 出不了真图——mock 成固定 dataURL
vi.mock("qrcode", () => ({
  default: { toDataURL: (...args: unknown[]) => toDataUrlMock(...args) },
}));

// tests/setup.ts 未初始化 i18n，显式引入并按默认英文断言（jsdom navigator.language=en）
import i18n from "@/i18n";
import { RemoteSection } from "@/components/settings/RemoteSection";

void i18n;

// ---- mock 载荷工厂（形状契约 = Rust channels_payload 注释，M5 A5 + §C1/§C3 tailscale）----
const channelsOf = (
  over: {
    lan?: Record<string, unknown>;
    quick?: Record<string, unknown>;
    named?: Record<string, unknown>;
    tailscale?: Record<string, unknown>;
  } = {}
) => ({
  local: { running: true, address: "http://127.0.0.1:9420/m" },
  lan: {
    enabled: true,
    running: true,
    addresses: ["http://192.168.66.202:9420/m", "http://192.168.42.216:9420/m"],
    ...over.lan,
  },
  quick: {
    enabled: true,
    running: true,
    address: "https://quick-test.trycloudflare.com/m",
    error: null,
    ...over.quick,
  },
  named: { enabled: false, running: false, address: null, error: null, ...over.named },
  // §C3 地址三重门：address 只在 running ∧ error 空 ∧ reach=verified 时由后端宣称
  tailscale: {
    enabled: false,
    running: false,
    address: null,
    error: null,
    reach: { state: "unverified" },
    ...over.tailscale,
  },
});

const statusOf = (over: Record<string, unknown> = {}) => ({
  enabled: true,
  maxDevices: 10,
  // H3：无头总开关状态（缺省 off——后端缺键即默认关，前端 undefined 同样按关渲染）
  headlessEnabled: false,
  // H4（Task 6）：无头子区另两件——watchdog 超时 + 全局并发上限（后端缺键即默认值）
  headlessTimeoutMs: 600000,
  headlessConcurrency: 2,
  channels: channelsOf(over.channels as never),
  pin: "4827",
  host: { name: "matebook16s", platform: "windows", version: "0.4.2", bootId: "boot-x" },
  enabledTools: [],
  ...over,
});

// 总开关关着的基线（通道 KV 开关仍在——卡片开关态照常渲染）
const disabledStatus = () => statusOf({ enabled: false });

// 默认 invoke 行为：status + 各设置键回填 + 空设备表
beforeEach(() => {
  invokeMock.mockReset();
  toDataUrlMock.mockResolvedValue("data:image/png;base64,mockQR");
  // jsdom 无剪贴板内核：stub writeText（复制按钮 → toast 成功路径）
  Object.defineProperty(window.navigator, "clipboard", {
    value: { writeText: vi.fn(async () => {}) },
    configurable: true,
  });
  invokeMock.mockImplementation(async (cmd: string, args?: { key?: string }) => {
    if (cmd === "remote_status") return statusOf();
    if (cmd === "get_setting") {
      if (args?.key === "remote.tunnel_token") return "eyJh-saved-token";
      return null; // remote.host_name 未设置 → 默认系统名；remote.keepalive → true
    }
    if (cmd === "remote_devices") return [];
    return null;
  });
});

afterEach(() => {
  vi.clearAllMocks();
});

// 卡片定位（线稿四卡一排；data-card 是卡片可点击容器的稳定钩子）
const card = (key: string) => document.querySelector(`[data-card="${key}"]`) as HTMLElement;
const cardSwitch = (key: string) => within(card(key)).getByRole("switch");
const liveDot = (key: string) => card(key).querySelector("[data-live]") as HTMLElement;
// 唯一展开区（同时只显示一个）
const expandedKey = () => document.querySelector("[data-expand]")?.getAttribute("data-expand");
// 展开区「复制」按钮（同一时刻只挂一个通道；访问密码/设备区无同名按钮）
const copyButtons = () => screen.queryAllByRole("button", { name: /^copy$/i });

describe("RemoteSection 四卡渲染与状态点映射（§C4 重排）", () => {
  it("四张卡按线稿顺序渲染（局域网/临时隧道/自有域名/外部域名），本机不在卡片区；状态点 = running、开关 = enabled", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status")
        return statusOf({
          channels: channelsOf({
            lan: { enabled: false, running: false }, // 关 → 灰点 + 开关 off
            quick: { enabled: true, running: false }, // 开关 ON 但服务未运行 → 点转灰（running 判据）、开关仍 ON（enabled 判据）
            named: { enabled: false, running: false }, // 关 → 灰点
            tailscale: { enabled: false, running: false }, // 关 → 灰点
          }),
        });
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      return null;
    });
    render(<RemoteSection />);
    // status 已载（总开关 Switch disabled={busy || !status}，载入后才可点）
    const master = screen.getByRole("switch", { name: /enable remote access/i });
    await waitFor(() => expect(master).toBeEnabled());
    // 四卡齐全（局域网连接 / 临时隧道 / 自有域名 / 外部域名）
    for (const key of ["lan", "quick", "named", "tailscale"]) expect(card(key)).toBeTruthy();
    // 「本机」不再是通道（零豁免 §C4）：不占卡位；设备表为空时全页无该文案
    expect(document.querySelector('[data-card="local"]')).toBeNull();
    expect(screen.queryByText("This machine")).toBeNull();
    // 卡名跟随线稿 v6 改名
    expect(within(card("lan")).getByText("LAN")).toBeTruthy();
    expect(within(card("named")).getByText("Own domain")).toBeTruthy();
    expect(within(card("tailscale")).getByText(/External domain/i)).toBeTruthy();
    // 状态点映射（评审 C-I1：只看 running）——局域网(关)/临时隧道(开关开但未运行)/自有域名(关)/外部域名(关) 全灰
    expect(liveDot("lan")).toHaveClass("bg-gray-300");
    expect(liveDot("quick")).toHaveClass("bg-gray-300");
    expect(liveDot("named")).toHaveClass("bg-gray-300");
    expect(liveDot("tailscale")).toHaveClass("bg-gray-300");
    // 开关态映射（只看 enabled）：临时隧道 ON 尽管服务未运行——开关是用户意图，状态点是展示真值
    expect(cardSwitch("lan")).not.toBeChecked();
    expect(cardSwitch("quick")).toBeChecked();
    expect(cardSwitch("named")).not.toBeChecked();
    expect(cardSwitch("tailscale")).not.toBeChecked();
  });

  it("点总开关关 → remote_toggle(false)；P3-a 修订后关闭语义文案随行展示", async () => {
    render(<RemoteSection />);
    const sw = screen.getByRole("switch", { name: /enable remote access/i });
    await waitFor(() => expect(sw).toBeEnabled());
    fireEvent.click(sw);
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("remote_toggle", { enabled: false })
    );
    expect(screen.getByText(/Turning it off only stops external access/i)).toBeTruthy();
  });
});

// ① 卡片标题被截断（2026-10-07 用户实测 + 裁决）：zh 的 chanTailscale 原为
// 「外部域名（免域名）」9 字，而四列卡位的标题可用宽只有约 94px（见下），于是被
// `truncate` 切成「外部域名（免…」——用户看到的就是这个。用户裁决原话：
// 「改标题反正就改吧，那个括号我感觉不用。这 4 个字就挺好的，其他注释写在下面」。
// 故：① zh 收成 4 个字「外部域名」；② en 同步去括号；③ 括号里那层「免域名」信息
// **不许丢**——改由卡面备注（chanTailscaleDesc）承载；④ 标题挂原生 title tooltip
// 兜底（将来某语言仍超宽时悬停可见全名）。线稿已同步（硬契约）。
describe("RemoteSection ①通道卡标题不截断（用户裁决：4 个字、去掉括号）", () => {
  const root = process.cwd();
  const locale = (name: string) =>
    JSON.parse(readFileSync(path.join(root, `src/i18n/locales/${name}.json`), "utf8"));

  // 卡位标题可用宽（≈94px）逐项来源：设置窗口默认 880 − 侧栏 w-40(160) = 720；
  // 内容区 p-4(32) ⇒ 688；grid-cols-4 + gap-2(8×3=24) ⇒ 每卡 166；卡边框 1.5×2 +
  // p-2.5(10×2) ⇒ 内宽 143；标题行右侧开关 w-8(32) + gap-1.5(6) ⇒ 105；标题内状态点
  // 7px + gap-1(4) ⇒ **94px**。字符宽按最宽语言的上界估：汉字 13px、拉丁 7.4px
  // （实测 Hiragino/Helvetica 13px bold：汉字 13.0、拉丁词组均值 ≈6.6–7.0）。
  const TITLE_BUDGET_PX = 94;
  const widthPx = (s: string) =>
    [...s].reduce((w, ch) => w + (/[\u2e80-\u9fff\uff00-\uffef]/.test(ch) ? 13 : 7.4), 0);

  it("外部域名卡名收成「外部域名」（恰 4 字、无括号）；en 同步去括号", () => {
    const zh = locale("zh");
    const en = locale("en");
    expect(zh.settings.remote.chanTailscale).toBe("外部域名");
    expect(en.settings.remote.chanTailscale).not.toContain("(");
    expect(en.settings.remote.chanTailscale).not.toContain("（");
  });

  it("zh 四张卡名都落在卡位标题宽内（旧「外部域名（免域名）」= 117px 必被截断）", () => {
    const zh = locale("zh");
    for (const k of ["chanLan", "chanQuick", "chanNamed", "chanTailscale"]) {
      const name = zh.settings.remote[k] as string;
      expect(
        widthPx(name),
        `卡名「${name}」≈${widthPx(name).toFixed(1)}px > 卡位 ${TITLE_BUDGET_PX}px，会被 truncate`
      ).toBeLessThanOrEqual(TITLE_BUDGET_PX);
    }
  });

  // **M2（2026-10-08 架构评审）**：宽度预算此前**只遍历 zh**——en 的
  // `chanTailscale` = "External domain"（15 拉丁字符 ≈111px）**超预算**，而它同样会被
  // `truncate` 切掉（用户可见后果与 zh 那条用户裁决的缺陷同类）。
  // 本组把 en 也纳入断言，并把超预算那条**如实登记为已知边界**（不是"通过"）：
  // 收短英文措辞会改用户可见文案，属**控制方裁决**（评审原话："改文案要先确认"），
  // 故实现侧不擅自改；本断言一旦翻红，正是"有人收短了"的信号——那时请把它并入上面的
  // 预算断言，并同步控制方的裁决记录。兜底机制（标题 title tooltip）由下一条测试锁。
  it("M2：en 卡名纳入宽度断言——三张在预算内，chanTailscale 超预算是已登记边界（待裁决措辞）", () => {
    const en = locale("en");
    for (const k of ["chanLan", "chanQuick", "chanNamed"]) {
      const name = en.settings.remote[k] as string;
      expect(
        widthPx(name),
        `en 卡名「${name}」≈${widthPx(name).toFixed(1)}px > 卡位 ${TITLE_BUDGET_PX}px，会被 truncate`
      ).toBeLessThanOrEqual(TITLE_BUDGET_PX);
    }
    const tail = en.settings.remote.chanTailscale as string;
    const w = widthPx(tail);
    expect(
      w,
      `en 卡名「${tail}」≈${w.toFixed(1)}px 已被登记为**超预算边界**（tooltip 兜底）；` +
        "若它已收短到预算内，请把 en 并入上面的预算断言并撤回本边界登记"
    ).toBeGreaterThan(TITLE_BUDGET_PX);
  });

  it("被删掉的限定信息不许丢：备注（chanTailscaleDesc）说明「无需自备域名」", () => {
    const zh = locale("zh");
    const en = locale("en");
    expect(zh.settings.remote.chanTailscaleDesc).toContain("不需要域名");
    expect(en.settings.remote.chanTailscaleDesc).toMatch(/no domain/i);
  });

  it("四张卡的标题元素挂原生 title（tooltip 兜底：某语言仍超宽时悬停可见全名）", async () => {
    render(<RemoteSection />);
    await waitFor(() =>
      expect(screen.getByRole("switch", { name: /enable remote access/i })).toBeEnabled()
    );
    const names: Array<[string, string]> = [
      ["lan", "LAN"],
      ["quick", "Quick tunnel"],
      ["named", "Own domain"],
      ["tailscale", "External domain"],
    ];
    for (const [key, name] of names) {
      // 标题=截断元素本身：title 必须挂在那个 span 上，否则悬停拿不到全名
      const el = within(card(key)).getByText(name);
      expect(el.getAttribute("title"), `${key} 卡标题缺 title tooltip`).toBe(name);
    }
  });
});

describe("RemoteSection 点卡片唯一展开详情区（§C4）", () => {
  it("默认展开局域网（本机卡已删）；点临时隧道/外部域名卡切换——同一时刻只有一个", async () => {
    render(<RemoteSection />);
    // 默认选中 = lan：局域网地址直接上屏（不再有本机展开块）
    expect(await screen.findByText("http://192.168.66.202:9420/m")).toBeTruthy();
    expect(expandedKey()).toBe("lan");
    // 点临时隧道卡 → 切换：换址警告上屏，局域网地址消失（唯一展开）
    fireEvent.click(card("quick"));
    expect(expandedKey()).toBe("quick");
    expect(screen.getByText(/regenerates the tunnel address/i)).toBeTruthy();
    expect(screen.queryByText("http://192.168.66.202:9420/m")).toBeNull();
    // 点外部域名卡 → 再切换：三态容器上屏（探测载荷未到位时也不崩）
    fireEvent.click(card("tailscale"));
    expect(expandedKey()).toBe("tailscale");
    expect(document.querySelector("[data-testid='ts-phase']")).toBeTruthy();
  });

  it("局域网关闭态：显式「未开启」徽标 + 无地址行（本机访问也进不去的原因要可见）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status")
        // 载荷仍带真实局域网地址（后端 addresses 不受 running/总开关约束）——关着就一条都不许宣称
        return statusOf({ channels: channelsOf({ lan: { enabled: false, running: false } }) });
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      return null;
    });
    render(<RemoteSection />);
    expect(await screen.findByText("Off")).toBeTruthy();
    expect(
      screen.getByText(/neither LAN devices nor this computer's browser can get in/i)
    ).toBeTruthy();
    expect(screen.queryByText("http://192.168.66.202:9420/m")).toBeNull();
    expect(screen.queryByText("http://192.168.42.216:9420/m")).toBeNull();
    // 死链不宣称：复制按钮与二维码整块不渲染（评审 C-I1 渲染条件锁）
    expect(copyButtons()).toHaveLength(0);
    expect(toDataUrlMock).not.toHaveBeenCalled();
    // 本机访问口径提示恒在（零豁免：本机浏览器访问同样走这张卡，前提是开关已打开）
    expect(screen.getByText(/Local access from this computer uses this address too/i)).toBeTruthy();
  });

  it("局域网详情：推荐徽标落首条地址 + 二维码（地址#pin=<pin>）+ 换网络说明", async () => {
    render(<RemoteSection />);
    await screen.findByText("http://192.168.66.202:9420/m"); // 默认展开即局域网
    // 首条带推荐徽标，第二条不带（getAllByText 恰 1 次）
    expect(screen.getAllByText("Recommended")).toHaveLength(1);
    expect(screen.getByText("http://192.168.42.216:9420/m")).toBeTruthy();
    // 二维码内容 = 地址#pin=<pin>（PIN 来自 remote_status.pin = 4827）
    await waitFor(() =>
      expect(toDataUrlMock).toHaveBeenCalledWith(
        "http://192.168.66.202:9420/m#pin=4827",
        expect.anything()
      )
    );
    expect(screen.getByText(/The address changes when the network changes/i)).toBeTruthy();
  });
});

// 评审 C-I1（展示态取 running、开关取 enabled）：旧判据 chanOn = enabled||running 使
// 「未开启」徽标的条件退化为 !enabled（因 running ⇒ enabled），**永远捕捉不到
// 「通道开关开着但服务没在监听」**——而「总开关关闭 + 通道开关开着」是一键可达的默认
// 路径（stop_server(false) 只停监听/隧道，不改通道 KV）：载荷
// {enabled:true, running:false, addresses:[真实局域网地址]} 下旧版照旧渲染绿色「推荐」
// 徽标 + 地址 + 复制 + 二维码 + 一行「前提是此开关已打开」（而它确实开着）——服务没在
// 监听，扫码/复制拿到的是死链。§C4 原文要求「卡片必须显式呈现开关状态（用户要知道
// 为什么本机也进不去）」，总开关关闭正是「本机也进不去」的最常见成因。
describe("RemoteSection 展示态取 running、开关取 enabled（评审 C-I1）", () => {
  const mockStatus = (payload: Record<string, unknown>) => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status") return statusOf(payload);
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      return null;
    });
  };

  it("总开关关闭 ∧ 局域网开关开着：未运行徽标 + 总开关成因可见；地址/复制/二维码一律不渲染", async () => {
    mockStatus({
      enabled: false, // 总开关关（stop_server(false) 只停监听，不改通道 KV）
      channels: channelsOf({ lan: { enabled: true, running: false } }), // 载荷仍带真实局域网地址
    });
    render(<RemoteSection />);
    expect(await screen.findByText("Not running", undefined, { timeout: 3000 })).toBeTruthy();
    expect(screen.getByText(/Remote access is off/i)).toBeTruthy();
    // 展示态与开关态分离：开关本体仍 ON（enabled 驱动）、卡片状态点转灰（running 驱动）
    expect(cardSwitch("lan")).toBeChecked();
    expect(liveDot("lan")).toHaveClass("bg-gray-300");
    // 服务没在监听 → 地址与二维码不得被宣称可用（对齐隧道族「已停通道绝不宣称地址」口径）
    expect(screen.queryByText("http://192.168.66.202:9420/m")).toBeNull();
    expect(screen.queryByText("http://192.168.42.216:9420/m")).toBeNull();
    expect(screen.queryByText("Recommended")).toBeNull();
    expect(copyButtons()).toHaveLength(0);
    expect(toDataUrlMock).not.toHaveBeenCalled();
  });

  it("两开关都开但服务没在监听：同样给未运行徽标，且不谎报成因（不说总开关关闭）", async () => {
    mockStatus({
      enabled: true,
      channels: channelsOf({ lan: { enabled: true, running: false } }),
    });
    render(<RemoteSection />);
    expect(await screen.findByText("Not running", undefined, { timeout: 3000 })).toBeTruthy();
    expect(screen.getByText(/is not listening yet/i)).toBeTruthy();
    expect(screen.queryByText(/Remote access is off/i)).toBeNull();
    expect(screen.queryByText("http://192.168.66.202:9420/m")).toBeNull();
    expect(copyButtons()).toHaveLength(0);
  });

  it("临时隧道 enabled ∧ !running：未运行徽标 + 说明（旧版只剩一个「公网」徽标、无地址无说明）", async () => {
    mockStatus({
      enabled: true,
      channels: channelsOf({
        quick: { enabled: true, running: false, address: null, error: null },
      }),
    });
    render(<RemoteSection />);
    fireEvent.click(card("quick"));
    expect(await screen.findByText("Not running", undefined, { timeout: 3000 })).toBeTruthy();
    expect(screen.getByText(/is not listening yet/i)).toBeTruthy();
    expect(screen.queryByText("Running")).toBeNull();
    expect(copyButtons()).toHaveLength(0);
    expect(toDataUrlMock).not.toHaveBeenCalled();
  });

  it("临时隧道通道开关关（总开关开着）：未开启徽标 + 该通道自己的成因文案", async () => {
    mockStatus({
      enabled: true,
      channels: channelsOf({
        quick: { enabled: false, running: false, address: null, error: null },
      }),
    });
    render(<RemoteSection />);
    fireEvent.click(card("quick"));
    expect(await screen.findByText("Off", undefined, { timeout: 3000 })).toBeTruthy();
    expect(screen.getByText(/no public address exists yet/i)).toBeTruthy();
    expect(screen.queryByText(/Remote access is off/i)).toBeNull();
  });
});

// H3（Task 5）：无头注入总开关——默认关 / 开启弹一次性安全说明 / 确认即知悉并落盘
// 记忆（KV remote.headless_notice_ack，同 codex 一次性提示的持久化口径）/ 已确认过
// 再开不再弹 / 关闭方向直接落盘不弹。开关状态唯一数据源 = remote_status.headlessEnabled。
describe("RemoteSection 无头注入总开关（H3 / Task 5）", () => {
  const headlessSwitch = () => screen.getByRole("switch", { name: /headless injection/i });
  const ackCalls = () =>
    invokeMock.mock.calls.filter(
      (call) =>
        call[0] === "set_setting" &&
        (call[1] as { key?: string } | undefined)?.key === "remote.headless_notice_ack"
    );

  it("渲染「无头注入」开关：status.headlessEnabled=false → 默认关 + 行提示（不影响终端四家）", async () => {
    render(<RemoteSection />);
    await waitFor(() => expect(headlessSwitch()).toBeEnabled());
    expect(headlessSwitch()).not.toBeChecked();
    expect(screen.getByText(/the four terminal-injection tools are unaffected/i)).toBeTruthy();
  });

  it("缺键（旧后端载荷 undefined）也按关渲染——不谎报开", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status") return statusOf({ headlessEnabled: undefined });
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      return null;
    });
    render(<RemoteSection />);
    await waitFor(() => expect(headlessSwitch()).toBeEnabled());
    expect(headlessSwitch()).not.toBeChecked();
  });

  it("status.headlessEnabled=true → 开关为开（后端唯一数据源）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status") return statusOf({ headlessEnabled: true });
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      return null;
    });
    render(<RemoteSection />);
    await waitFor(() => expect(headlessSwitch()).toBeChecked());
  });

  it("点开（未确认过）→ 弹一次性安全说明（含 yolo 档与「开启即知悉」）；确认 → remote_toggle_headless(true) + 落盘 ack + Dialog 关", async () => {
    render(<RemoteSection />);
    await waitFor(() => expect(headlessSwitch()).toBeEnabled());
    fireEvent.click(headlessSwitch());
    // 安全说明：不经终端可视确认 + zcode 默认 yolo 档（spec H3 文案）
    expect(await screen.findByText(/without visible confirmation in the terminal/i)).toBeTruthy();
    expect(screen.getByText(/zcode channel defaults to yolo mode/i)).toBeTruthy();
    // 未确认前不落盘开关（确认才开启）
    expect(invokeMock).not.toHaveBeenCalledWith("remote_toggle_headless", expect.anything());
    fireEvent.click(screen.getByRole("button", { name: /I understand, enable/i }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("remote_toggle_headless", { enabled: true })
    );
    // 一次性记忆落盘（后续开启不再弹）
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("set_setting", {
        key: "remote.headless_notice_ack",
        value: "true",
      })
    );
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  });

  it("安全说明取消 → 不调 remote_toggle_headless、不落盘 ack，开关停在关", async () => {
    render(<RemoteSection />);
    await waitFor(() => expect(headlessSwitch()).toBeEnabled());
    fireEvent.click(headlessSwitch());
    fireEvent.click(await screen.findByRole("button", { name: /^cancel$/i }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(invokeMock).not.toHaveBeenCalledWith("remote_toggle_headless", expect.anything());
    expect(ackCalls()).toEqual([]);
    expect(headlessSwitch()).not.toBeChecked();
  });

  it("已确认过（ack=true）→ 再开启不再弹说明，直接 remote_toggle_headless(true)", async () => {
    invokeMock.mockImplementation(async (cmd: string, args?: { key?: string }) => {
      if (cmd === "remote_status") return statusOf();
      if (cmd === "get_setting" && args?.key === "remote.headless_notice_ack") return "true";
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      return null;
    });
    render(<RemoteSection />);
    // ack 回填完成后再点（effect 异步读 KV）
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("get_setting", { key: "remote.headless_notice_ack" })
    );
    await waitFor(() => expect(headlessSwitch()).toBeEnabled());
    fireEvent.click(headlessSwitch());
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("remote_toggle_headless", { enabled: true })
    );
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("关闭方向：headlessEnabled=true 点关 → 直接 remote_toggle_headless(false)，不弹说明", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status") return statusOf({ headlessEnabled: true });
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      return null;
    });
    render(<RemoteSection />);
    await waitFor(() => expect(headlessSwitch()).toBeChecked());
    fireEvent.click(headlessSwitch());
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("remote_toggle_headless", { enabled: false })
    );
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });
});

// H4（Task 6）：「无头」子区三件套收齐——总开关（H3）+ watchdog 超时 + 全局并发上限。
// 数据源 = remote_status.headlessTimeoutMs / headlessConcurrency（缺键 → 文档默认值
// 600000ms / 2）；保存走 remote_set_headless_limits（后端 clamp + 审计 + 广播）。
describe("RemoteSection 无头子区三件套（H4 / Task 6）", () => {
  const timeoutInput = () => screen.getByLabelText("Headless timeout (ms)") as HTMLInputElement;
  const concurrencyInput = () =>
    screen.getByLabelText("Headless concurrency cap") as HTMLInputElement;
  const limitsRow = () => document.querySelector("[data-headless-limits]") as HTMLElement;
  const saveButton = () =>
    within(limitsRow()).getByRole("button", { name: /save headless settings/i });

  it("三控件同区渲染：总开关 + 超时 + 并发上限", async () => {
    render(<RemoteSection />);
    await waitFor(() => expect(timeoutInput()).toBeTruthy());
    expect(screen.getByRole("switch", { name: /headless injection/i })).toBeTruthy();
    expect(concurrencyInput()).toBeTruthy();
    expect(saveButton()).toBeTruthy();
    // 行提示（默认值口径写进 UI，不让用户猜）
    expect(screen.getByText(/Default 600000/i)).toBeTruthy();
    expect(screen.getByText(/Default 2\b/i)).toBeTruthy();
  });

  it("缺键（旧后端载荷 undefined）→ 渲染文档默认值 600000 / 2", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status")
        return statusOf({ headlessTimeoutMs: undefined, headlessConcurrency: undefined });
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      return null;
    });
    render(<RemoteSection />);
    await waitFor(() => expect(timeoutInput().value).toBe("600000"));
    expect(concurrencyInput().value).toBe("2");
  });

  it("status 带值 → 控件回填（后端是唯一数据源）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status")
        return statusOf({ headlessTimeoutMs: 120000, headlessConcurrency: 3 });
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      return null;
    });
    render(<RemoteSection />);
    await waitFor(() => expect(timeoutInput().value).toBe("120000"));
    expect(concurrencyInput().value).toBe("3");
  });

  it("改值保存 → remote_set_headless_limits({timeoutMs, concurrency}) + 成功提示", async () => {
    render(<RemoteSection />);
    await waitFor(() => expect(timeoutInput().value).toBe("600000"));
    fireEvent.change(timeoutInput(), { target: { value: "120000" } });
    fireEvent.change(concurrencyInput(), { target: { value: "3" } });
    fireEvent.click(saveButton());
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("remote_set_headless_limits", {
        timeoutMs: 120000,
        concurrency: 3,
      })
    );
    expect(toastSuccessMock).toHaveBeenCalled();
  });

  it("保存失败 → toast 报错（不静默吞），控件值不回弹", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status") return statusOf();
      if (cmd === "remote_set_headless_limits") throw "后端拒绝（模拟）";
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      return null;
    });
    render(<RemoteSection />);
    await waitFor(() => expect(timeoutInput().value).toBe("600000"));
    fireEvent.change(timeoutInput(), { target: { value: "90000" } });
    fireEvent.click(saveButton());
    await waitFor(() => expect(toastErrorMock).toHaveBeenCalled());
    expect(timeoutInput().value).toBe("90000");
  });
});

describe("RemoteSection lan 开关 P7 TLS Dialog 流（M5 A6）", () => {
  const lanOffStatus = () =>
    statusOf({ channels: channelsOf({ lan: { enabled: false, running: false } }) });

  it("开 lan 收到 P7 特征 Err → 弹 TLS 确认 Dialog，不 toast 报错", async () => {
    invokeMock.mockImplementation(async (cmd: string, args?: Record<string, unknown>) => {
      if (cmd === "remote_status") return lanOffStatus();
      if (cmd === "remote_toggle_channel" && args?.channel === "lan" && args?.on === true) {
        throw "对外绑定需先确认已配置 TLS 反向代理（remote_confirm_public）";
      }
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      return null;
    });
    render(<RemoteSection />);
    await waitFor(() => expect(cardSwitch("lan")).toBeEnabled());
    fireEvent.click(cardSwitch("lan"));
    // 特征文案命中 → 既有 TLS 确认 Dialog（标题即 P7 门槛文案），错误不落 toast
    expect(await screen.findByText("Confirm external binding")).toBeTruthy();
    expect(toastErrorMock).not.toHaveBeenCalled();
  });

  it("Dialog 确认 → remote_confirm_public 先行、重试 remote_toggle_channel(lan,true) 成功、Dialog 关", async () => {
    let lanTries = 0;
    const calls: string[] = [];
    invokeMock.mockImplementation(async (cmd: string, args?: Record<string, unknown>) => {
      calls.push(cmd);
      if (cmd === "remote_status") return lanOffStatus();
      if (cmd === "remote_toggle_channel" && args?.channel === "lan" && args?.on === true) {
        lanTries += 1;
        if (lanTries === 1) throw "对外绑定需先确认已配置 TLS 反向代理（remote_confirm_public）";
        return; // 重试成功
      }
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      return null;
    });
    render(<RemoteSection />);
    await waitFor(() => expect(cardSwitch("lan")).toBeEnabled());
    fireEvent.click(cardSwitch("lan"));
    fireEvent.click(await screen.findByRole("button", { name: /I understand, enable/i }));
    await waitFor(() => expect(lanTries).toBe(2));
    // 顺序契约：先置位 ack（后端门据此放行）再重试
    expect(calls.indexOf("remote_confirm_public")).toBeGreaterThan(-1);
    expect(calls.indexOf("remote_confirm_public")).toBeLessThan(
      calls.lastIndexOf("remote_toggle_channel")
    );
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  });

  it("Dialog 取消 → 不调 remote_confirm_public、不重试，仅关 Dialog", async () => {
    let lanTries = 0;
    invokeMock.mockImplementation(async (cmd: string, args?: Record<string, unknown>) => {
      if (cmd === "remote_status") return lanOffStatus();
      if (cmd === "remote_toggle_channel" && args?.channel === "lan" && args?.on === true) {
        lanTries += 1;
        throw "对外绑定需先确认已配置 TLS 反向代理（remote_confirm_public）";
      }
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      return null;
    });
    render(<RemoteSection />);
    await waitFor(() => expect(cardSwitch("lan")).toBeEnabled());
    fireEvent.click(cardSwitch("lan"));
    fireEvent.click(await screen.findByRole("button", { name: /^cancel$/i }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(invokeMock).not.toHaveBeenCalledWith("remote_confirm_public");
    // 仅首次触发的那一次调用，无重试
    await act(async () => {});
    expect(lanTries).toBe(1);
  });

  it("非 lan 通道开关失败走 toast 报错，不弹 Dialog（P7 Dialog 仅 lan 开启路径）", async () => {
    invokeMock.mockImplementation(async (cmd: string, args?: Record<string, unknown>) => {
      if (cmd === "remote_status") return lanOffStatus();
      if (cmd === "remote_toggle_channel" && args?.channel === "quick") {
        throw "隧道拉起失败（模拟）";
      }
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      return null;
    });
    render(<RemoteSection />);
    await waitFor(() => expect(cardSwitch("quick")).toBeEnabled());
    // quick 默认开着（enabled）→ 点击为关方向；失败仍走 toast 而非 Dialog
    fireEvent.click(cardSwitch("quick"));
    await waitFor(() => expect(toastErrorMock).toHaveBeenCalled());
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(invokeMock).toHaveBeenCalledWith("remote_toggle_channel", {
      channel: "quick",
      on: false,
    });
  });
});

describe("RemoteSection quick 换址警告与运行态（M5 A6）", () => {
  it("运行中：公网徽标 + 地址 + 运行中/断线自动重连 + 换址警告块 + 二维码", async () => {
    render(<RemoteSection />);
    fireEvent.click(card("quick"));
    expect(await screen.findByText("https://quick-test.trycloudflare.com/m")).toBeTruthy();
    expect(screen.getAllByText("Public").length).toBeGreaterThan(0);
    expect(screen.getByText("Running")).toBeTruthy();
    expect(screen.getByText(/auto-reconnects on disconnect/i)).toBeTruthy();
    expect(screen.getByText(/regenerates the tunnel address/i)).toBeTruthy();
    await waitFor(() =>
      expect(toDataUrlMock).toHaveBeenCalledWith(
        "https://quick-test.trycloudflare.com/m#pin=4827",
        expect.anything()
      )
    );
    expect(screen.getByText(/cellular/i)).toBeTruthy();
  });

  it("错误态：error 原文展示、无「运行中」徽标", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status")
        return statusOf({
          channels: channelsOf({
            quick: { enabled: true, running: false, address: null, error: "cloudflared 下载失败" },
          }),
        });
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      return null;
    });
    render(<RemoteSection />);
    fireEvent.click(card("quick"));
    expect(await screen.findByText("cloudflared 下载失败")).toBeTruthy();
    expect(screen.queryByText("Running")).toBeNull();
  });
});

describe("RemoteSection 自有域名：Token 保存与教程 popover（M5 A6）", () => {
  it("Token 输入回填已存值，保存走 set_setting(remote.tunnel_token)", async () => {
    render(<RemoteSection />);
    fireEvent.click(card("named"));
    const input = (await screen.findByLabelText("Tunnel Token")) as HTMLInputElement;
    await waitFor(() => expect(input.value).toBe("eyJh-saved-token"));
    fireEvent.change(input, { target: { value: "eyJh-new-token" } });
    // Token 保存按钮与 PIN 保存同名（线稿均为「保存」），作用域限定在 Token 输入框所在行
    fireEvent.click(within(input.closest("div")!).getByRole("button", { name: /^save$/i }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("set_setting", {
        key: "remote.tunnel_token",
        value: "eyJh-new-token",
      })
    );
    // toast 在 `await set_setting` 的续体里才弹，而上面的 waitFor 只看"**调用**已发生"
    // （mock 的调用在 promise 落定前就记账）——两者之间隔着一个微任务，满载时会早读。
    // 2026-10-07：所有命令人为延后 30ms 即稳定复现（同一类"读到回执到达之前"）。
    await waitFor(() => expect(toastSuccessMock).toHaveBeenCalled());
  });

  it("教程 popover：点击展开六步教程，再点收起（可保持展开边看边操作）", async () => {
    render(<RemoteSection />);
    fireEvent.click(card("named"));
    // 收起态：教程正文不在文档
    expect(screen.queryByText(/Prerequisite: a domain hosted on Cloudflare/i)).toBeNull();
    fireEvent.click(screen.getByText(/How-to \(click to expand\/collapse\)/i));
    // 展开：前置 + 六步 + 收尾说明；data-help 钩子随开合翻转（A8 评审：勿留死钩子）
    expect(screen.getByText(/Prerequisite: a domain hosted on Cloudflare/i)).toBeTruthy();
    expect(
      (screen.getByText(/How-to \(click to expand\/collapse\)/i) as HTMLElement).dataset["help"]
    ).toBe("open");
    expect(screen.getByText(/Sign in at dash\.cloudflare\.com/i)).toBeTruthy();
    expect(screen.getByText(/Create a tunnel, connection type Cloudflared/i)).toBeTruthy();
    expect(screen.getByText(/Save tunnel;/i)).toBeTruthy();
    expect(screen.getByText(/install nothing/i)).toBeTruthy();
    expect(screen.getByText(/Public Hostname tab/i)).toBeTruthy();
    expect(screen.getByText(/paste the token into the input and click Save/i)).toBeTruthy();
    expect(screen.getByText(/never changes afterwards/i)).toBeTruthy();
    // 再点收起
    fireEvent.click(screen.getByText(/How-to \(click to expand\/collapse\)/i));
    expect(screen.queryByText(/Prerequisite: a domain hosted on Cloudflare/i)).toBeNull();
  });

  it("自有域名详情含公网地址与二维码；错误态展示 error 原文", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status")
        return statusOf({
          channels: channelsOf({
            named: {
              enabled: true,
              running: true,
              address: "https://mam.jarvis.example.com/m",
              error: null,
            },
          }),
        });
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      return null;
    });
    render(<RemoteSection />);
    fireEvent.click(card("named"));
    expect(await screen.findByText("https://mam.jarvis.example.com/m")).toBeTruthy();
    await waitFor(() =>
      expect(toDataUrlMock).toHaveBeenCalledWith(
        "https://mam.jarvis.example.com/m#pin=4827",
        expect.anything()
      )
    );
  });
});

// ---- tailscale 卡三态（简报 Step 4）探测载荷工厂（形状 = Rust wizard_status 注释）----
const tsWStep = (id: string, needsHuman = false, humanActionKey = "") => ({
  id,
  needsHuman,
  humanActionKey,
});
const tsWState = (id: string, done: boolean, blockedReason: string | null = null) => ({
  id,
  done,
  blockedReason,
});
// macOS 九步基线：detect 完成、其余待做（= 配置中）
const probeOf = (over: Record<string, unknown> = {}) => ({
  platform: "mac",
  windowsVerified: true,
  steps: [
    tsWStep("detect"),
    tsWStep("download"),
    tsWStep("install", true, "settings.remote.tsWizard.actAdmin"),
    tsWStep("sys_ext", true, "settings.remote.tsWizard.actSysExt"),
    tsWStep("login", true, "settings.remote.tsWizard.actLogin"),
    tsWStep("shields_up"),
    tsWStep("funnel", true, "settings.remote.tsWizard.actFunnel"),
    tsWStep("verify"),
    tsWStep("autostart"),
  ],
  states: [
    tsWState("detect", true),
    tsWState("download", false),
    tsWState("install", false),
    tsWState("sys_ext", true),
    tsWState("login", false),
    tsWState("shields_up", false),
    tsWState("funnel", false),
    tsWState("verify", false),
    tsWState("autostart", true),
  ],
  authUrl: "https://login.tailscale.com/a/abc123",
  running: false,
  boardUrl: null,
  reach: { state: "unverified" },
  ...over,
});
const phaseEl = () => document.querySelector("[data-testid='ts-phase']") as HTMLElement;

describe("RemoteSection tailscale 卡三态（§C4 / 简报 Step 4）", () => {
  it("未安装（detect 未完成）：价值主张 + 一键配置按钮；点击进入向导本体", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status") return statusOf();
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      if (cmd === "remote_ts_probe")
        return probeOf({
          states: [
            tsWState("detect", false),
            tsWState("download", false),
            tsWState("install", false),
            tsWState("sys_ext", false),
            tsWState("login", false),
            tsWState("shields_up", false),
            tsWState("funnel", false),
            tsWState("verify", false),
            tsWState("autostart", true),
          ],
        });
      return null;
    });
    render(<RemoteSection />);
    fireEvent.click(card("tailscale"));
    await waitFor(() => expect(phaseEl().getAttribute("data-phase")).toBe("notInstalled"));
    expect(screen.getByText(/no domain of your own/i)).toBeTruthy(); // 价值主张
    expect(screen.queryByTestId("ts-wizard")).toBeNull(); // 点击前不挂向导本体
    fireEvent.click(screen.getByRole("button", { name: /set up in one click/i }));
    expect(await screen.findByTestId("ts-wizard")).toBeTruthy();
  });

  it("配置中（有步骤未完成且已开始）：向导进度 = 平台步骤表数据（每步可见 + 卡点 blocked_reason）；未验证地址不上墙", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status")
        return statusOf({
          channels: channelsOf({
            // 载荷即便带着未验证地址，UI 也不得绕过 §C3 三重门自行展示
            tailscale: {
              enabled: true,
              running: true,
              address: "https://mam.tail1234.ts.net/m",
              error: "尚未生效",
              reach: { state: "unverified" },
            },
          }),
        });
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      if (cmd === "remote_ts_probe")
        return probeOf({
          states: [
            tsWState("detect", true),
            tsWState("download", false, "sha256 校验失败（模拟）"),
            tsWState("install", false),
            tsWState("sys_ext", true),
            tsWState("login", false),
            tsWState("shields_up", false),
            tsWState("funnel", false),
            tsWState("verify", false),
            tsWState("autostart", true),
          ],
        });
      return null;
    });
    render(<RemoteSection />);
    fireEvent.click(card("tailscale"));
    await waitFor(() => expect(phaseEl().getAttribute("data-phase")).toBe("configuring"));
    // 向导进度渲染 remote_ts_probe 的平台步骤表（每步一行），卡点随行展示原因
    expect(await screen.findByTestId("ts-wizard")).toBeTruthy();
    // 步骤行来自 remote_ts_probe 的回执（ts-wizard **根元素**先挂、行后到）：必须等"行齐"
    // 再数，否则读到的是回执到达**之前**的 DOM（0 行）——满载/慢机器上偶发恒红。
    // 2026-10-07 定位：把 mock 回执人为延后 50ms，旧写法 100% 复现（收到 0 而非 18，
    // 故**不是**"别的测试留下的 DOM"；本文件 afterEach 亦有 cleanup）；改等条件后同一
    // 延后下 100% 绿。**断言强度不变**：仍要求恰好 9 行（真回归 / 多挂一份向导照样红）。
    await waitFor(() => expect(document.querySelectorAll("[data-step]").length).toBe(9));
    expect(document.querySelector("[data-testid='ts-blocked']")?.textContent).toContain(
      "sha256 校验失败（模拟）"
    );
    // §C3：reach 未验证 → 固定地址不得显示成可用（UI 不拼不猜）
    expect(screen.queryByText("https://mam.tail1234.ts.net/m")).toBeNull();
  });

  it("已配置（全部步骤完成 ∧ reach=Verified）：固定地址 + 复制 + 运行态 + 二维码", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status")
        return statusOf({
          channels: channelsOf({
            tailscale: {
              enabled: true,
              running: true,
              address: "https://mam.tail1234.ts.net/m",
              error: null,
              reach: { state: "verified" },
            },
          }),
        });
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      if (cmd === "remote_ts_probe")
        return probeOf({
          states: [
            tsWState("detect", true),
            tsWState("download", true),
            tsWState("install", true),
            tsWState("sys_ext", true),
            tsWState("login", true),
            tsWState("shields_up", true),
            tsWState("funnel", true),
            tsWState("verify", true),
            tsWState("autostart", true),
          ],
          running: true,
          boardUrl: "https://mam.tail1234.ts.net/m",
          reach: { state: "verified" },
        });
      return null;
    });
    render(<RemoteSection />);
    fireEvent.click(card("tailscale"));
    await waitFor(() => expect(phaseEl().getAttribute("data-phase")).toBe("configured"));
    expect(screen.getByText("https://mam.tail1234.ts.net/m")).toBeTruthy();
    expect(screen.getByText("Running")).toBeTruthy();
    expect(screen.getByText(/Auto-restores on launch/i)).toBeTruthy();
    await waitFor(() =>
      expect(toDataUrlMock).toHaveBeenCalledWith(
        "https://mam.tail1234.ts.net/m#pin=4827",
        expect.anything()
      )
    );
  });
});

describe("RemoteSection B3：永久地址卡（自有域名 / 外部域名）的运行态与成因口径", () => {
  const mock = (payload: Record<string, unknown>) => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status") return statusOf(payload);
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      if (cmd === "remote_ts_probe") return macLikeProbeDone();
      return null;
    });
  };
  // 外部域名卡要落到「已配置」相位才渲染运行态行：全步骤完成 + 校验通过
  // I-1（评审）：夹具必须是**后端可产出**的载荷——`verify.done` 的判据在 Rust 侧是
  // 「校验态 Verified ∧ 快照 running」（wizard.rs probe_steps_from），故 `running:false`
  // 与 `verify.done:true` **在稳态互斥**；旧夹具同时造了这两样，才让一条实际不可达的
  // 渲染分支（状态三里的成因行）在测试里"绿"着。这里按后端真能产出的形态造：
  // 开通/登录/shields 都完成（Funnel 配置在、已登录），但通道没在运行 ⇒ verify 未完成。
  const macLikeProbeDone = () => ({
    platform: "windows" as const,
    windowsVerified: true,
    writePathVerified: true,
    steps: ["detect", "download", "install", "login", "shields_up", "funnel", "verify", "autostart"].map(
      (id) => ({ id, needsHuman: false, humanOptional: false, humanActionKey: "" })
    ),
    states: ["detect", "download", "install", "login", "shields_up", "funnel", "autostart"].map((id) => ({
      id,
      done: true,
      blockedReason: null,
    })).concat([{ id: "verify", done: false, blockedReason: null }]),
    authUrl: "",
    running: false,
    boardUrl: null,
    reach: { state: "verified" },
  });

  it("自有域名 running=false：地址与复制**保留**（永久地址），但带「未开启」徽标 + 成因，二维码不渲染", async () => {
    mock({
      channels: channelsOf({
        named: {
          enabled: false,
          running: false,
          address: null,
          error: null,
          lastAddr: "https://mam.jarvis.example.com/m",
        },
      }),
    });
    render(<RemoteSection />);
    fireEvent.click(card("named"));
    expect(await screen.findByText("Off", undefined, { timeout: 3000 })).toBeTruthy();
    // 永久地址保留显示（可复制存书签）——判据原则：用户自己的地址不因未运行而消失
    expect(screen.getByText("https://mam.jarvis.example.com/m")).toBeTruthy();
    expect(copyButtons().length).toBeGreaterThan(0);
    // 但绝不宣称可用：成因文案在场、二维码（=扫码即可用）不渲染
    expect(screen.getByText(/turn it on to restore/i)).toBeTruthy();
    expect(toDataUrlMock).not.toHaveBeenCalled();
    expect(screen.queryByText("Running")).toBeNull();
  });

  it("自有域名 running=true：绿「运行中」徽标 + 二维码（可扫码即用）", async () => {
    mock({
      channels: channelsOf({
        named: {
          enabled: true,
          running: true,
          address: "https://mam.jarvis.example.com/m",
          error: null,
        },
      }),
    });
    render(<RemoteSection />);
    fireEvent.click(card("named"));
    expect(await screen.findByText("Running", undefined, { timeout: 3000 })).toBeTruthy();
    expect(screen.getByText("https://mam.jarvis.example.com/m")).toBeTruthy();
    await waitFor(() => expect(toDataUrlMock).toHaveBeenCalled());
  });

  it("自有域名总开关关闭：成因指向总开关（不谎报成「该通道开关未打开」）", async () => {
    mock({
      enabled: false,
      channels: channelsOf({
        named: { enabled: true, running: false, address: null, error: null },
      }),
    });
    render(<RemoteSection />);
    fireEvent.click(card("named"));
    expect(await screen.findByText("Not running", undefined, { timeout: 3000 })).toBeTruthy();
    expect(screen.getByText(/Remote access is off/i)).toBeTruthy();
  });

  // W-A（真机重启逐秒实测）：重启后有 1–2 分钟恢复窗口（T+58s 服务已 Running 但
  // BackendState=NoState；T+73s 配置逐字段自恢复；T+89s 公网仍 TLS 失败；T+2.5min 200）。
  // 这窗口内卡面必须给**「恢复中」**独立相位：既不是故障（红色错误），也不是
  // 「通道未运行」（用户会去点开关重开），更不是「域名生效中」（成因指错）。
  it("W-A：reach=recovering → 卡面落「恢复中」相位（不是故障、也不是「未运行」），地址与二维码不渲染", async () => {
    mock({
      channels: channelsOf({
        tailscale: {
          enabled: true,
          running: false,
          address: null,
          // 载荷 error 与后端同口径（RECOVERING_HINT）——但 UI 不该把它渲染成红色故障
          error: "Tailscale 后端正在重连（开机后通常 1–2 分钟）——配置与地址都不会变",
          reach: { state: "recovering" },
        },
      }),
    });
    render(<RemoteSection />);
    fireEvent.click(card("tailscale"));
    await waitFor(() => expect(phaseEl().getAttribute("data-phase")).toBe("recovering"));
    const row = screen.getByTestId("ts-recovering");
    expect(row.textContent).toMatch(/reconnecting/i);
    expect(row.textContent).toMatch(/1[–-]2 minutes/i);
    // 不是「未运行」成因档、不是「配置中」向导、也没有红色「尚未生效」
    expect(screen.queryByText("Not running")).toBeNull();
    expect(screen.queryByText("Setting up")).toBeNull();
    expect(screen.queryByTestId("ts-wizard")).toBeNull();
    expect(screen.queryByTestId("ts-not-live")).toBeNull();
    // 地址与二维码不渲染（§C3 三门：未验证不宣称可用）；安抚文案在场
    expect(screen.queryByText("https://mam.tail1234.ts.net/m")).toBeNull();
    expect(toDataUrlMock).not.toHaveBeenCalled();
    expect(screen.getByTestId("ts-addr-permanent")).toBeTruthy();
  });

  // I3（2026-10-07 评审 Important）：相位机的「恢复中」必须**先过通道开关门**。
  // 竞态下载荷可能带着 `enabled=false ∧ reach=recovering`（后端同一竞态已在载荷侧收口，
  // 这里是前端侧纵深防御）——若相位机只看 reach.state，用户刚关掉的通道会被渲染成
  // 「恢复中……配置与地址都不会变，无需任何操作」，与既成事实背离。
  it("I3：通道开关关着时 reach=recovering 不得落「恢复中」相位（enabled 前置门）", async () => {
    mock({
      channels: channelsOf({
        tailscale: {
          enabled: false,
          running: false,
          address: null,
          error: null,
          reach: { state: "recovering" },
        },
      }),
    });
    render(<RemoteSection />);
    fireEvent.click(card("tailscale"));
    await waitFor(() => expect(phaseEl().getAttribute("data-phase")).toBe("configuredIdle"));
    expect(screen.queryByTestId("ts-recovering")).toBeNull();
    expect(screen.queryByText(/reconnecting/i)).toBeNull();
    // 因果如实：落「未开启」档（本通道开关关着），不是「恢复中」
    expect(screen.getByText("Off")).toBeTruthy();
    expect(screen.getByText(/Funnel is not enabled/i)).toBeTruthy();
  });

  // M1（2026-10-07 评审 Minor）：「恢复中」必须有界——超宽限窗后后端持续未就绪时，
  // 后端降级为 reach=failed + 快照 error（如实点名「后端持续未就绪（NoState，已 X 分钟）」）；
  // 卡面必须（a）如实展示成因（不再是「通常 1–2 分钟」的无界安抚），（b）给出**向导重试
  // 入口**（线稿状态五第 2 行的承诺：宽限窗过后回状态四，由向导 verify 行点名 + 重试）。
  it("M1：后端持续未就绪（有界降级）→ 成因如实 + 向导重试入口可展开", async () => {
    const degradeMsg =
      "Tailscale 后端持续未就绪（NoState，已 5 分钟）——已超出开机恢复的宽限窗；可在配置向导中重试，或检查 Tailscale 服务是否正常";
    mock({
      channels: channelsOf({
        tailscale: {
          enabled: true,
          running: false,
          address: null,
          error: degradeMsg,
          reach: { state: "failed", reason: degradeMsg },
        },
      }),
    });
    render(<RemoteSection />);
    fireEvent.click(card("tailscale"));
    await waitFor(() => expect(phaseEl().getAttribute("data-phase")).toBe("configuredIdle"));
    // （a）成因如实：点名后端状态与已等时长（不是「恢复中，无需任何操作」）
    expect(screen.getByText(/持续未就绪/)).toBeTruthy();
    expect(screen.getByText(/NoState/)).toBeTruthy();
    expect(screen.queryByText(/nothing to do/i)).toBeNull();
    // （b）升级路径：向导重试入口在场，点击后挂出向导本体（verify 行的重试按钮在其中）
    expect(screen.queryByTestId("ts-wizard")).toBeNull();
    fireEvent.click(screen.getByTestId("ts-wizard-retry-entry"));
    expect(await screen.findByTestId("ts-wizard")).toBeTruthy();
  });

  it("外部域名卡 running=false ∧ 总开关开着：落到「已配置·未运行」态，成因是「服务没在监听」，不是笼统的「开关已关」", async () => {
    mock({
      channels: channelsOf({
        tailscale: {
          enabled: true,
          running: false,
          address: null,
          error: null,
          reach: { state: "verified" },
        },
      }),
    });
    render(<RemoteSection />);
    fireEvent.click(card("tailscale"));
    expect(await screen.findByText("Not running", undefined, { timeout: 3000 })).toBeTruthy();
    // I-1：运行态行所在的是**状态四「通道未运行」**（线稿 p-ext 第四块），不是
    // 「配置中」——旧相位机要求全步完成 ∧ reach=Verified 才给成因行，而 running=false
    // 时 verify 步在后端判据下永远未完成 ⇒ 那条成因行**稳态不可达**，用户实际看到的
    // 是「配置中」+ 向导列表（成因被吞掉）
    expect(phaseEl().getAttribute("data-phase")).toBe("configuredIdle");
    expect(screen.queryByText("Setting up")).toBeNull();
    expect(screen.queryByTestId("ts-wizard")).toBeNull();
    // B3：旧版无论哪种成因都只给一句 tsOffHint（「开关已关」）——那是谎报
    expect(screen.getByText(/is not listening yet/i)).toBeTruthy();
    expect(screen.queryByText(/Switch is off/i)).toBeNull();
  });

  it("外部域名卡通道开关关：给「未开启」徽标 + 本通道自己的成因文案", async () => {
    mock({
      channels: channelsOf({
        tailscale: {
          enabled: false,
          running: false,
          address: null,
          error: null,
          reach: { state: "verified" },
        },
      }),
    });
    render(<RemoteSection />);
    fireEvent.click(card("tailscale"));
    expect(await screen.findByText("Off", undefined, { timeout: 3000 })).toBeTruthy();
    expect(screen.getByText(/Funnel is not enabled/i)).toBeTruthy();
  });

  // M-5（用户裁决：只加文案，不改逻辑）：本卡在未运行态**不渲染地址**（§C3 三门——地址
  // 只在 running ∧ error 空 ∧ 校验通过时由后端宣称），用户因此怀疑「这链接到底是不是固定
  // 的、下次开还是不是这个」。补一句安抚文案：地址固定不变、通道恢复后自动回来。
  // 只在 configuredIdle 出现——运行中态地址已上墙（该态的话由 tsRunningHint 承担），
  // 多挂一句就是把「兜底」变成噪音。
  it("M-5：未运行态补「地址固定不变、恢复后自动回来」的安抚文案；运行中态不出现", async () => {
    // ① 已配置·未运行（configuredIdle）：地址按 §C3 三门不渲染 → 安抚文案必须在场
    mock({
      channels: channelsOf({
        tailscale: {
          enabled: true,
          running: false,
          address: null,
          error: null,
          reach: { state: "verified" },
        },
      }),
    });
    render(<RemoteSection />);
    fireEvent.click(card("tailscale"));
    await waitFor(() => expect(phaseEl().getAttribute("data-phase")).toBe("configuredIdle"));
    expect(screen.queryByText("https://mam.tail1234.ts.net/m")).toBeNull();
    const idleHint = screen.getByTestId("ts-addr-permanent");
    expect(idleHint.textContent).toMatch(/never changes/i);
    expect(idleHint.textContent).toMatch(/comes back automatically/i);

    // ② 运行中（configured）：同一句不得出现——地址已上墙、二维码可扫码即用
    cleanup();
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status")
        return statusOf({
          channels: channelsOf({
            tailscale: {
              enabled: true,
              running: true,
              address: "https://mam.tail1234.ts.net/m",
              error: null,
              reach: { state: "verified" },
            },
          }),
        });
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      if (cmd === "remote_ts_probe")
        return probeOf({
          states: [
            tsWState("detect", true),
            tsWState("download", true),
            tsWState("install", true),
            tsWState("sys_ext", true),
            tsWState("login", true),
            tsWState("shields_up", true),
            tsWState("funnel", true),
            tsWState("verify", true),
            tsWState("autostart", true),
          ],
          running: true,
          boardUrl: "https://mam.tail1234.ts.net/m",
          reach: { state: "verified" },
        });
      return null;
    });
    render(<RemoteSection />);
    fireEvent.click(card("tailscale"));
    await waitFor(() => expect(phaseEl().getAttribute("data-phase")).toBe("configured"));
    expect(screen.queryByTestId("ts-addr-permanent")).toBeNull();
  });
});

describe("RemoteSection 访问密码（M5 A6）", () => {
  const pinInput = () => screen.getByLabelText("Access PIN") as HTMLInputElement;

  it("PIN 输入框回填 remote_status.pin（4 位数字居中）", async () => {
    render(<RemoteSection />);
    await waitFor(() => expect(pinInput().value).toBe("4827"));
  });

  it("随机按钮生成 1000-9999 的 4 位数字", async () => {
    render(<RemoteSection />);
    await waitFor(() => expect(pinInput().value).toBe("4827"));
    for (let i = 0; i < 20; i++) {
      fireEvent.click(screen.getByRole("button", { name: /random/i }));
      const v = pinInput().value;
      expect(v).toMatch(/^\d{4}$/);
      expect(Number(v)).toBeGreaterThanOrEqual(1000);
      expect(Number(v)).toBeLessThanOrEqual(9999);
    }
  });

  it("保存 → remote_set_pin 以输入值调用 + 成功提示「所有设备需重新输入」语义", async () => {
    render(<RemoteSection />);
    await waitFor(() => expect(pinInput().value).toBe("4827"));
    fireEvent.change(pinInput(), { target: { value: "1357" } });
    fireEvent.click(screen.getByRole("button", { name: /^save$/i }));
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith("remote_set_pin", { pin: "1357" }));
    await waitFor(() =>
      expect(toastSuccessMock).toHaveBeenCalledWith(
        expect.stringMatching(/all devices must re-enter/i)
      )
    );
  });

  it("保存未满 4 位时禁用（A8 评审：不发起注定失败的后端往返）", async () => {
    render(<RemoteSection />);
    await waitFor(() => expect(pinInput().value).toBe("4827"));
    fireEvent.change(pinInput(), { target: { value: "13" } });
    expect((screen.getByRole("button", { name: /^save$/i }) as HTMLButtonElement).disabled).toBe(
      true
    );
    expect(invokeMock).not.toHaveBeenCalledWith("remote_set_pin", expect.anything());
    fireEvent.change(pinInput(), { target: { value: "1357" } });
    expect((screen.getByRole("button", { name: /^save$/i }) as HTMLButtonElement).disabled).toBe(
      false
    );
  });

  it("重置设备：二次确认 Dialog——确认调 remote_reset_devices，取消零调用", async () => {
    render(<RemoteSection />);
    await waitFor(() => expect(pinInput().value).toBe("4827"));
    fireEvent.click(screen.getByRole("button", { name: /reset devices/i }));
    expect(await screen.findByText("Reset devices?")).toBeTruthy();
    // 取消：不调后端
    fireEvent.click(screen.getByRole("button", { name: /^cancel$/i }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(invokeMock).not.toHaveBeenCalledWith("remote_reset_devices");
    // 再开 → 确认：remote_reset_devices 被调，Dialog 关
    fireEvent.click(screen.getByRole("button", { name: /reset devices/i }));
    fireEvent.click(await screen.findByRole("button", { name: /reset$/i }));
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith("remote_reset_devices"));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  });
});

describe("RemoteSection 已接入设备列表（M5 A6）", () => {
  const devicesOf = () => [
    {
      id: "d1",
      name: "JARVIS 的 iPhone",
      firstPairedAt: 1,
      lastSeenAt: Date.now(),
      online: true,
      via: "quick",
    },
    {
      id: "d2",
      name: "matebook16s · Edge",
      firstPairedAt: 2,
      lastSeenAt: Date.now() - 7_200_000,
      online: false,
      via: "lan",
    },
    { id: "d3", name: "Desktop-A", firstPairedAt: 3, lastSeenAt: 3, online: false, via: "local" },
    { id: "d4", name: "iPad", firstPairedAt: 4, lastSeenAt: 4, online: false, via: "named" },
    { id: "d5", name: "Legacy", firstPairedAt: 5, lastSeenAt: 5, online: false },
    { id: "d6", name: "Nas", firstPairedAt: 6, lastSeenAt: 6, online: false, via: "tailscale" },
  ];

  beforeEach(() => {
    invokeMock.mockImplementation(async (cmd: string, args?: { key?: string }) => {
      if (cmd === "remote_status") return statusOf();
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return devicesOf();
      return null;
    });
  });

  it("上限徽标 N / maxDevices——默认 10（决策 #17），后端改上限随之变化（A8：不再硬编码）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status") return statusOf({ maxDevices: 7 });
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return devicesOf();
      return null;
    });
    render(<RemoteSection />);
    expect(await screen.findByText("6 / 7")).toBeTruthy();
  });

  it("via 五值徽标映射：quick→临时隧道、lan→局域网连接、local→本机、named→自有域名、tailscale→外部域名；无 via 不渲染徽标", async () => {
    render(<RemoteSection />);
    const row1 = (await screen.findByText("JARVIS 的 iPhone")).closest("li")!;
    expect(within(row1).getByText("Quick tunnel")).toBeTruthy();
    const row2 = screen.getByText("matebook16s · Edge").closest("li")!;
    expect(within(row2).getByText("LAN")).toBeTruthy();
    const row3 = screen.getByText("Desktop-A").closest("li")!;
    expect(within(row3).getByText("This machine")).toBeTruthy();
    const row4 = screen.getByText("iPad").closest("li")!;
    expect(within(row4).getByText("Own domain")).toBeTruthy();
    const row6 = screen.getByText("Nas").closest("li")!;
    expect(within(row6).getByText("External domain")).toBeTruthy();
    const row5 = screen.getByText("Legacy").closest("li")!;
    expect(within(row5).queryByText(/tunnel|LAN|machine|domain/i)).toBeNull();
  });

  it("在线点 + 在线/相对时间文案", async () => {
    render(<RemoteSection />);
    const row1 = (await screen.findByText("JARVIS 的 iPhone")).closest("li")!;
    expect(within(row1).getByText("Online")).toBeTruthy();
    expect(within(row1).getByText(/active just now/i)).toBeTruthy();
  });

  it("重命名：点重命名出输入框与保存，保存调 remote_rename_device(id, 新名)", async () => {
    render(<RemoteSection />);
    const row1 = (await screen.findByText("JARVIS 的 iPhone")).closest("li")!;
    fireEvent.click(within(row1).getByRole("button", { name: /rename/i }));
    const input = within(row1).getByLabelText(/rename/i) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "我的手机" } });
    fireEvent.click(within(row1).getByRole("button", { name: /^save$/i }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("remote_rename_device", {
        id: "d1",
        name: "我的手机",
      })
    );
  });

  it("踢下线：调 remote_revoke_device(id)", async () => {
    render(<RemoteSection />);
    const row2 = (await screen.findByText("matebook16s · Edge")).closest("li")!;
    fireEvent.click(within(row2).getByRole("button", { name: /kick/i }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("remote_revoke_device", { id: "d2" })
    );
  });

  it("空设备表：渲染占位文案，徽标 0 / 10", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status") return disabledStatus();
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      return null;
    });
    render(<RemoteSection />);
    expect(await screen.findByText("No paired devices")).toBeTruthy();
    expect(screen.getByText("0 / 10")).toBeTruthy();
  });

  it("远程关闭态（冷挂载）：花名册仍拉取并渲染 DB 行，徽标 5 / 10——不因停服清空（Mac 报告七-6 回归锁）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status") return disabledStatus();
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return devicesOf();
      return null;
    });
    render(<RemoteSection />);
    // 关闭态下 remote_devices 照常发起（旧版 disabled 直接清空且不拉取），行照常渲染
    expect(await screen.findByText("JARVIS 的 iPhone")).toBeTruthy();
    expect(screen.getByText("matebook16s · Edge")).toBeTruthy();
    expect(screen.getByText("6 / 10")).toBeTruthy();
  });

  it("总开关关掉：花名册保留不清空（吊销/重命名管理不随停服消失）", async () => {
    let on = true;
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status") return on ? statusOf() : disabledStatus();
      if (cmd === "remote_toggle") {
        on = false;
        return;
      }
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return devicesOf();
      return null;
    });
    render(<RemoteSection />);
    const sw = screen.getByRole("switch", { name: /enable remote access/i });
    await screen.findByText("JARVIS 的 iPhone");
    fireEvent.click(sw);
    // 旧版此处 enabled 翻 false → effect 立即 setDevices([]) 清空列表；修订后行保留
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("remote_toggle", { enabled: false })
    );
    expect(screen.getByText("JARVIS 的 iPhone")).toBeTruthy();
    expect(screen.getByText("6 / 10")).toBeTruthy();
  });
});

// §G3 回归锚（零豁免计划 Task 4）：2026-10-06 零豁免后服务端不再产生 via="local"
//（本机不是一条通道），但历史设备行已落的 local 标注必须保留可读——不得变成空白。
// 断言沿用本文件「默认英文环境」口径：zh 文案「本机」对应 en「This machine」。
describe("RemoteSection B1：撤销 tailscale 前先预览「会连带清掉哪些条目」", () => {
  // 载荷工厂：tailscale 开着（运行 + 已验证）——撤销入口才可达
  const tsOnStatus = () =>
    statusOf({
      channels: channelsOf({
        tailscale: {
          enabled: true,
          running: true,
          address: "https://mam.tail1234.ts.net/m",
          error: null,
          reach: { state: "verified" },
        },
      }),
    });
  const PREVIEW_MIXED = {
    ok: true,
    foreign: false,
    wouldClear: 1,
    entries: [
      { ours: true, label: "x.ts.net:443 / → http://127.0.0.1:9420" },
      { ours: false, label: "x.ts.net:443 /media → http://127.0.0.1:7000" },
    ],
  };
  const mockWithPreview = (preview: unknown) => {
    invokeMock.mockImplementation(async (cmd: string, args?: { step?: string }) => {
      if (cmd === "remote_status") return tsOnStatus();
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      if (cmd === "remote_ts_probe") return null;
      if (cmd === "remote_ts_run_step" && args?.step === "disable_preview") return preview;
      if (cmd === "remote_ts_run_step" && args?.step === "disable_force")
        return { ok: true, clearedExtraServeEntries: 1, forced: false };
      return null;
    });
  };
  const runStepCalls = (step: string) =>
    invokeMock.mock.calls.filter((c) => c[0] === "remote_ts_run_step" && (c[1] as { step?: string })?.step === step);
  // 开关在 status 到达前是 disabled（受控态），故先等它可用再点
  const renderAndToggleTsOff = async () => {
    render(<RemoteSection />);
    await waitFor(() => expect(cardSwitch("tailscale")).toBeEnabled());
    fireEvent.click(document.querySelector('[data-card="tailscale"]')!);
    fireEvent.click(cardSwitch("tailscale"));
  };

  it("叠加形态（wouldClear>0）关开关：先弹确认框逐条列出；未确认前零撤销调用", async () => {
    mockWithPreview(PREVIEW_MIXED);
    await renderAndToggleTsOff();
    const dlg = await screen.findByTestId("ts-disable-confirm");
    // 逐条列出将清除的条目（用户要能认出「这是我的 /media」）
    expect(dlg.textContent).toContain("/media");
    expect(dlg.textContent).toContain("http://127.0.0.1:7000");
    expect(runStepCalls("disable_preview").length).toBe(1);
    // 未确认前：不得撤销（既不发 toggle_channel 也不发 disable/disable_force）
    expect(invokeMock.mock.calls.filter((c) => c[0] === "remote_toggle_channel").length).toBe(0);
    expect(runStepCalls("disable").length).toBe(0);
    expect(runStepCalls("disable_force").length).toBe(0);
  });

  // W-A 第三处判据点（撤销预览）：预览的职责是"这次撤销会连带清掉什么"，而开机恢复
  // 窗口内 `funnel status --json` 会短暂为 {}——照旧读就把"读不到"报成"什么都没有"，
  // 用户点下的同意建立在假信息上（知情同意缺口）。后端此时回 unreadable=true，
  // 前端必须照 foreign 处理：**弹确认框并说明读不到**，绝不静默直接撤销。
  it("W-A：预览回 unreadable（后端重连中读不到配置形态）→ 照样弹确认框并说明读不到，不静默撤销", async () => {
    mockWithPreview({ ok: true, unreadable: true, foreign: false, wouldClear: 0, entries: [] });
    await renderAndToggleTsOff();
    const dlg = await screen.findByTestId("ts-disable-confirm");
    expect(dlg.textContent).toMatch(/cannot be read|读不到/i);
    expect(runStepCalls("disable_preview").length).toBe(1);
    // 未确认前零撤销（与 foreign/wouldClear 两档同一条出口）
    expect(runStepCalls("disable").length).toBe(0);
    expect(runStepCalls("disable_force").length).toBe(0);
  });

  it("确认后才走 disable_force（不是普通 disable——普通撤销会静默清掉用户条目）；成功后落 KV 关：开关随刷新后的真值回到 OFF", async () => {
    // C-1：撤销成功的唯一可观测面 = 后端把通道开关位落关后，remote_status 里
    // channels.tailscale.enabled 翻 false（前端**不**自己补一次 toggle 去"假装"关了）。
    // 旧版这条测试只断言 remote_toggle_channel 调用数 = 0——恰好把「撤销不落开关位、
    // 重启恢复会把 Funnel 重新开通」这个 Critical 缺陷锁死成期望行为。
    let revokeCalls = 0;
    invokeMock.mockImplementation(async (cmd: string, args?: { step?: string }) => {
      if (cmd === "remote_status")
        return revokeCalls > 0
          ? statusOf({
              channels: channelsOf({
                tailscale: {
                  enabled: false, // ← 撤销后 KV 落关（后端 disable 步写 write_chan_flag(false)）
                  running: false,
                  address: null,
                  error: null,
                  reach: { state: "unverified" },
                },
              }),
            })
          : tsOnStatus();
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      if (cmd === "remote_ts_probe") return null;
      if (cmd === "remote_ts_run_step" && args?.step === "disable_preview") return PREVIEW_MIXED;
      if (cmd === "remote_ts_run_step" && args?.step === "disable_force") {
        revokeCalls += 1;
        return { ok: true, clearedExtraServeEntries: 1, forced: false };
      }
      return null;
    });
    const first = render(<RemoteSection />);
    await waitFor(() => expect(cardSwitch("tailscale")).toBeEnabled());
    fireEvent.click(document.querySelector('[data-card="tailscale"]')!);
    fireEvent.click(cardSwitch("tailscale"));
    const dlg = await screen.findByTestId("ts-disable-confirm");
    fireEvent.click(within(dlg).getByRole("button", { name: /force revoke|强制撤销/i }));
    await waitFor(() => expect(runStepCalls("disable_force").length).toBe(1));
    expect(runStepCalls("disable").length).toBe(0);
    // 落 KV 关：撤销后必须重新拉取 status，且开关回到 OFF（不是靠前端自己改状态）
    await waitFor(() => expect(cardSwitch("tailscale")).not.toBeChecked());
    expect(invokeMock.mock.calls.filter((c) => c[0] === "remote_toggle_channel").length).toBe(0);

    // 「重启后不重开」的前端侧代理：冷挂载重来（= 重启后读同一份 KV 派生载荷）开关仍 OFF
    first.unmount();
    render(<RemoteSection />);
    await waitFor(() => expect(cardSwitch("tailscale")).toBeEnabled());
    expect(cardSwitch("tailscale")).not.toBeChecked();
    expect(revokeCalls).toBe(1);
  });

  it("取消确认框：零撤销调用（不得偷偷撤销）", async () => {
    mockWithPreview(PREVIEW_MIXED);
    await renderAndToggleTsOff();
    const dlg = await screen.findByTestId("ts-disable-confirm");
    fireEvent.click(within(dlg).getByRole("button", { name: /^cancel$/i }));
    await waitFor(() => expect(screen.queryByTestId("ts-disable-confirm")).toBeNull());
    expect(runStepCalls("disable_force").length).toBe(0);
    expect(runStepCalls("disable").length).toBe(0);
    expect(invokeMock.mock.calls.filter((c) => c[0] === "remote_toggle_channel").length).toBe(0);
  });

  it("无额外条目（wouldClear=0 ∧ !foreign）：不弹框，走**普通 disable**（非 force）并消费回执", async () => {
    // M-1：不弹框那条路也必须经向导步消费回执——走 remote_toggle_channel 的话
    // stop_channel() 会把 ServeResetReport 丢掉，窗口内配置变了用户只看到日志
    mockWithPreview({ ok: true, foreign: false, wouldClear: 0, entries: [{ ours: true, label: "ours" }] });
    await renderAndToggleTsOff();
    await waitFor(() => expect(runStepCalls("disable").length).toBe(1));
    expect(runStepCalls("disable_force").length).toBe(0);
    expect(screen.queryByTestId("ts-disable-confirm")).toBeNull();
  });

  it("无框路径也把「实际被清条目」上墙——预览说 0 条、撤销回执说 2 条（窗口内配置变了）", async () => {
    // M-1：预览与撤销之间存在窗口；「N 条」与列表**同源**（都取回执），故必须列回执那两条
    invokeMock.mockImplementation(async (cmd: string, args?: { step?: string }) => {
      if (cmd === "remote_status") return tsOnStatus();
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      if (cmd === "remote_ts_probe") return null;
      if (cmd === "remote_ts_run_step" && args?.step === "disable_preview")
        return { ok: true, foreign: false, wouldClear: 0, entries: [{ ours: true, label: "ours" }] };
      if (cmd === "remote_ts_run_step" && args?.step === "disable")
        return {
          ok: true,
          clearedExtraServeEntries: 2,
          clearedEntries: [
            { ours: false, label: "x.ts.net:443 /newapp → http://127.0.0.1:7100" },
            { ours: false, label: "x.ts.net:443 /docs → http://127.0.0.1:7200" },
          ],
          forced: false,
        };
      return null;
    });
    await renderAndToggleTsOff();
    const notice = await screen.findByTestId("ts-cleared-notice");
    expect(notice.textContent).toContain("2");
    expect(notice.textContent).toContain("/newapp");
    expect(notice.textContent).toContain("/docs");
    // 「N 条」与列表同源：toast 条数 = 列表长度（都来自回执），不取预览的 0 条
    expect(toastErrorMock).toHaveBeenCalledWith(expect.stringContaining("2"));
  });

  it("回执 clearedExtraServeEntries>0 → 卡面如实呈现被清条目（不静默）", async () => {
    mockWithPreview(PREVIEW_MIXED);
    await renderAndToggleTsOff();
    const dlg = await screen.findByTestId("ts-disable-confirm");
    fireEvent.click(within(dlg).getByRole("button", { name: /force revoke|强制撤销/i }));
    const notice = await screen.findByTestId("ts-cleared-notice");
    expect(notice.textContent).toContain("1");
    expect(notice.textContent).toContain("/media");
  });

  it("M-1：「N 条」与列表同源——回执条目与预览不一致时以回执为准（不列预览的旧条目）", async () => {
    invokeMock.mockImplementation(async (cmd: string, args?: { step?: string }) => {
      if (cmd === "remote_status") return tsOnStatus();
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      if (cmd === "remote_ts_probe") return null;
      if (cmd === "remote_ts_run_step" && args?.step === "disable_preview") return PREVIEW_MIXED;
      if (cmd === "remote_ts_run_step" && args?.step === "disable_force")
        return {
          ok: true,
          clearedExtraServeEntries: 2,
          // 撤销时实际被清的是两条**新**条目（预览时的 /media 已不在了）
          clearedEntries: [
            { ours: false, label: "x.ts.net:443 /newapp → http://127.0.0.1:7100" },
            { ours: false, label: "x.ts.net:443 /docs → http://127.0.0.1:7200" },
          ],
          forced: false,
        };
      return null;
    });
    await renderAndToggleTsOff();
    const dlg = await screen.findByTestId("ts-disable-confirm");
    fireEvent.click(within(dlg).getByRole("button", { name: /force revoke|强制撤销/i }));
    const notice = await screen.findByTestId("ts-cleared-notice");
    expect(notice.textContent).toContain("/newapp");
    expect(notice.textContent).toContain("/docs");
    expect(notice.textContent).not.toContain("/media");
    expect(notice.textContent).toContain("2");
  });

  it("M-1：撤销回看在切换卡片后收起（旧实现一旦置上永不清）", async () => {
    mockWithPreview(PREVIEW_MIXED);
    await renderAndToggleTsOff();
    const dlg = await screen.findByTestId("ts-disable-confirm");
    fireEvent.click(within(dlg).getByRole("button", { name: /force revoke|强制撤销/i }));
    await screen.findByTestId("ts-cleared-notice");
    fireEvent.click(card("lan"));
    fireEvent.click(card("tailscale"));
    await waitFor(() => expect(screen.queryByTestId("ts-cleared-notice")).toBeNull());
  });

  it("预览失败（读不到 Funnel 状态）：如实报错且不撤销（fail-closed，不静默跳过守卫）", async () => {
    invokeMock.mockImplementation(async (cmd: string, args?: { step?: string }) => {
      if (cmd === "remote_status") return tsOnStatus();
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      if (cmd === "remote_ts_run_step" && args?.step === "disable_preview")
        throw "读不到 Funnel 状态，拒绝预览撤销（无法确认是否会覆盖他人的 serve 配置）";
      return null;
    });
    await renderAndToggleTsOff();
    await waitFor(() => expect(toastErrorMock).toHaveBeenCalled());
    expect(invokeMock.mock.calls.filter((c) => c[0] === "remote_toggle_channel").length).toBe(0);
    expect(runStepCalls("disable_force").length).toBe(0);
  });
});

describe("RemoteSection 历史 local 徽标保留（零豁免 §G3）", () => {
  it("历史 local 标注仍可读（零豁免后服务端不再产生，但历史行不得变成空白）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_status") return statusOf();
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices")
        return [
          {
            id: "d1",
            name: "旧手机",
            firstPairedAt: 1,
            lastSeenAt: Date.now(),
            online: false,
            via: "local",
          },
        ];
      return null;
    });
    render(<RemoteSection />);
    const row = (await screen.findByText("旧手机")).closest("li")!;
    expect(within(row).getByText("This machine")).toBeTruthy();
  });
});

describe("RemoteSection 本机名称默认系统名（M5 A6）", () => {
  it("remote.host_name 未设置（null）→ 默认填 remote_status.host.name（系统名），去掉灰字提示", async () => {
    render(<RemoteSection />);
    const input = (await screen.findByLabelText("Machine name")) as HTMLInputElement;
    await waitFor(() => expect(input.value).toBe("matebook16s"));
    // 无占位灰字提示（占位符为空串）
    expect(input.placeholder).toBe("");
    fireEvent.change(input, { target: { value: "my-pc" } });
    fireEvent.blur(input);
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("set_setting", {
        key: "remote.host_name",
        value: "my-pc",
      })
    );
  });

  it("已存 host_name 优先于系统名（不回填覆盖用户命名）", async () => {
    invokeMock.mockImplementation(async (cmd: string, args?: { key?: string }) => {
      if (cmd === "remote_status") return statusOf();
      if (cmd === "get_setting" && args?.key === "remote.host_name") return "JARVIS-Win";
      if (cmd === "get_setting") return null;
      if (cmd === "remote_devices") return [];
      return null;
    });
    render(<RemoteSection />);
    const input = (await screen.findByLabelText("Machine name")) as HTMLInputElement;
    await waitFor(() => expect(input.value).toBe("JARVIS-Win"));
  });
});

// i18n 契约：RemoteSection / useRemoteEvents 源码里引用的每个 settings.remote.* 键，
// 必须在 zh 与 en 两个 locale 同齐备（缺键即键路径泄漏到 UI）；两 locale 键集必须相等。
describe("RemoteSection i18n zh/en 无缺键（M5 A6）", () => {
  // vitest jsdom 环境下 import.meta.url 非 file 协议，用进程 cwd（vitest 以仓库根启动）
  const root = process.cwd();
  const flat = (obj: Record<string, unknown>, prefix = ""): string[] =>
    Object.entries(obj).flatMap(([k, v]) =>
      typeof v === "object" && v !== null
        ? flat(v as Record<string, unknown>, `${prefix}${k}.`)
        : [`${prefix}${k}`]
    );

  it("源码引用键 zh/en 双语齐备，且两 locale 的 settings.remote 键集相等", () => {
    const zh = JSON.parse(readFileSync(path.join(root, "src/i18n/locales/zh.json"), "utf8"));
    const en = JSON.parse(readFileSync(path.join(root, "src/i18n/locales/en.json"), "utf8"));
    const zhKeys = new Set(flat(zh.settings.remote).map((k) => `settings.remote.${k}`));
    const enKeys = new Set(flat(en.settings.remote).map((k) => `settings.remote.${k}`));
    // 两 locale 键集相等（对齐 scripts/check-i18n 的子树版）
    expect([...zhKeys].filter((k) => !enKeys.has(k))).toEqual([]);
    expect([...enKeys].filter((k) => !zhKeys.has(k))).toEqual([]);

    // 源码扫描：两个消费方引用的键必须双 locale 存在
    const sources = [
      path.join(root, "src/components/settings/RemoteSection.tsx"),
      path.join(root, "src/hooks/useRemoteEvents.ts"),
    ];
    const used = new Set<string>();
    for (const f of sources) {
      const text = readFileSync(f, "utf8");
      for (const m of text.matchAll(/settings\.remote\.([A-Za-z0-9_]+)/g)) {
        used.add(m[0]);
      }
    }
    expect(used.size).toBeGreaterThan(20);
    const missingZh = [...used].filter((k) => !zhKeys.has(k));
    const missingEn = [...used].filter((k) => !enKeys.has(k));
    expect(missingZh).toEqual([]);
    expect(missingEn).toEqual([]);
  });
});
