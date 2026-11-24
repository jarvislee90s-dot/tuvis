/**
 * 客户端半区入口（`dsh.client` → `exports["./client"]`）：本机授权卡。
 *
 * **本文件是「经典脚本」源码，不是 ES 模块**（Mac 补探测报告 §3.2/§3.3）：
 * 官方宿主以 `<script src>` 下发插件 bundle（classic script），产物必须自带
 * `window.__ModuleLoader__.load({ id, factory })` 自注册信封；任何顶层
 * `import`/`export` 都会在网络侧直接 SyntaxError（官方 Loader 随后抛
 * `could not load "<id>"`，且宿主启动日志干净——这正是 R03 在真机上的表现）。
 * 因此本文件：
 *   - 不写任何 import/export，整个模块体都在 `factory` 内部（`factory` 的参数
 *     就是宿主种子解析器，顶层作用域没有 `require`）；
 *   - 只经该 `require(spec)` 取宿主种子模块（react），绝不把兔维斯自带的
 *     React 19 打进产物（官方种子是 React 18，双 React 会毁掉 hooks 身份——
 *     与 T03「不打包 React」契约同源）；
 *   - 信封 id 是**裸包名** `@tuvis/dsh-remote-access`（官方行名口径，
 *     `stripClientSuffix` 之后的名字；不是 `…/local-ui`、不是子路径）。
 *
 * 挂载面只有 slot 注册表（§3.2）：`ctx.slots.inject(name, () => ctx.slots.register(...))`。
 * 座位可能只在设置页在场时才声明，所以按社区插件同款纪律先用 `slots.spec` 探座位；
 * `inject` 的回调由 slots 服务负责随声明生命周期安装/拆除，fiber 拆解即级联清理。
 *
 * **安全边界（T03 交接①，逐字义务）**：`dsh.client.platform` 是 `"web"`——
 * 本卡在 **Desktop 窗口与 WebUI 都可见**。它只展示绑定状态与两端核对码
 * （display_code，短时比对用、非凭据），不做任何授权决定，也不展示 bindingId
 * 以外的内部字段；真正的安全边界是**兔维斯 Rust 侧的 OS 级对端核验**
 * （peercred PID + 宿主进程身份 + 用户在本机设置卡批准），渲染层可见性
 * **不构成**授权面。
 *
 * 数据源 = 宿主半区在同一本地 WebServer 上暴露的
 * `GET /tuvis-remote/binding-state`（同源相对路径 fetch；宿主半区对该路由
 * 逐路由施加回环 Host/Origin 门禁，见 src/index.ts）。
 */

/** 模块体所在的 loader 作用域（这里声明的名字都只在 factory 内可见）。 */
interface LoaderScope {
  /** 官方裸包名（信封 id；`stripClientSuffix` 之后的行名）。 */
  entryId: string;
  /** 绑定态路由（与宿主半区 index.ts 的 ROUTE_PREFIX 一致）。 */
  bindingStatePath: string;
  /** 卡片座位标识（`settings.section` 内的唯一条目 id）。 */
  seatId: string;
  /** 轮询间隔（毫秒）：绑定态是低频状态，2 秒足够且不打扰宿主。 */
  pollMs: number;
}

/** React 18 种子模块的最小面（只声明本卡真正用到的成员）。 */
interface ReactSeed {
  createElement(
    type: unknown,
    props?: Record<string, unknown> | null,
    ...children: unknown[]
  ): unknown;
  useState<T>(initial: T): [T, (next: T) => void];
  useEffect(run: () => void | (() => void), deps?: readonly unknown[]): void;
}

/** 绑定态（与宿主半区 auth.ts 的 BindingStateSnapshot 同形）。 */
type BindingClientState = "idle" | "pending" | "bound" | "revoked";

interface BindingStateSnapshot {
  state: BindingClientState;
  bindingId: string | null;
  displayCode: string | null;
}

/** 宿主 slots 服务的最小面（只声明本入口用到的成员）。 */
interface SlotsSeat {
  spec?(name: string): unknown;
  inject(name: string, callback: () => (() => void) | readonly (() => void)[]): () => void;
  register(options: Record<string, unknown>, component: unknown): () => void;
}

/** 客户端 fiber 上下文的最小面（真实 Cordis Context 在运行期满足该结构）。 */
interface ClientContext {
  slots: SlotsSeat;
}

interface ModuleSurface {
  apply: (ctx: ClientContext) => () => void;
  inject: string[];
  renderCardText: (snapshot: BindingStateSnapshot | null) => string[];
}

/** 官方 boot 前置脚本装在 window 上的加载器（本文件只用到 `load`）。 */
interface ModuleLoaderGlobal {
  load(registration: {
    id: string;
    factory: (loaderRequire: (specifier: string) => unknown) => Record<string, unknown>;
  }): void;
}

// **顶层零声明**（I-2）：官方宿主以 classic script 下发本文件，并会在 HMR /
// 代际换代时把同一份源码在**同一个全局词法作用域**里再次求值。顶层任何
// `const`/`let`/`class` 都会让第二次求值抛
// `SyntaxError: Identifier 'x' has already been declared`——卡片从此静默永不挂载
// （宿主日志干净，最坏形态）。
//
// 因此 loader 的取用**必须在 load 调用点就地完成**（不写 `const moduleLoader = …`）：
// 顶层只剩 `"use strict";` 与这一条 `window.__ModuleLoader__.load({...})` 语句。
// 顶层的 interface/type 声明是纯类型、编译后消失，不占运行时词法作用域。
(window as unknown as { __ModuleLoader__: ModuleLoaderGlobal }).__ModuleLoader__.load({
  id: "@tuvis/dsh-remote-access",
  factory: (require: (specifier: string) => unknown) => {
    const scope: LoaderScope = {
      entryId: "@tuvis/dsh-remote-access",
      bindingStatePath: "/tuvis-remote/binding-state",
      seatId: "tuvis-remote",
      pollMs: 2_000,
    };

    // 种子模块在运行期经 loader 的 require 取用（绝不内联进产物）。
    const React = require("react") as ReactSeed;

    /**
     * 卡片文案（纯函数，便于测试直接核对展示面）。
     *
     * 只输出状态与两端核对码：**没有任何凭据材料**、不输出 bindingId。
     */
    const renderCardText = (snapshot: BindingStateSnapshot | null): string[] => {
      if (snapshot === null) return ["Tuvis remote: binding state unavailable"];
      const lines: string[] = [`Tuvis remote: ${snapshot.state}`];
      if (snapshot.state === "pending") {
        if (snapshot.displayCode !== null) lines.push(`compare code: ${snapshot.displayCode}`);
        lines.push("approve in Tuvis settings to bind this device.");
      }
      if (snapshot.state === "revoked") {
        lines.push("authorization revoked in Tuvis; new credentials are not delivered.");
      }
      return lines;
    };

    /** 同源读取绑定态；任何失败都返回 null（卡片如实显示不可用，不猜）。 */
    const fetchSnapshot = async (): Promise<BindingStateSnapshot | null> => {
      try {
        const res = await fetch(scope.bindingStatePath, {
          headers: { accept: "application/json" },
        });
        if (!res.ok) return null;
        const body = (await res.json()) as BindingStateSnapshot;
        if (typeof body?.state !== "string") return null;
        return body;
      } catch {
        return null;
      }
    };

    /**
     * 本机授权卡：只读展示绑定态与核对码。
     *
     * 不在 `document.body` 上另挂 DOM（官方纪律：插件只在 slot 里渲染）；
     * 返回的清理函数停轮询（组件卸载即停）。
     */
    const TuvisBindingCard = (): unknown => {
      const [snapshot, setSnapshot] = React.useState<BindingStateSnapshot | null>(null);
      React.useEffect(() => {
        let stopped = false;
        const tick = async (): Promise<void> => {
          const next = await fetchSnapshot();
          if (!stopped) setSnapshot(next);
        };
        void tick();
        const timer = setInterval(() => void tick(), scope.pollMs);
        return () => {
          stopped = true;
          clearInterval(timer);
        };
      }, []);
      const lines = renderCardText(snapshot);
      return React.createElement(
        "div",
        { "data-tuvis-binding-card": "true", style: { font: "12px/1.6 sans-serif", opacity: 0.85 } },
        ...lines.map((line, index) => React.createElement("div", { key: `line-${index}` }, line)),
      );
    };

    /**
     * 挂载本机授权卡：座位在 `settings.section`。
     *
     * - 座位缺席（`slots.spec` 返回 undefined）→ 本次不注册（不猜、不硬塞）；
     *   `slots.inject` 的等待语义会在座位随后声明时再执行回调；
     * - 返回 disposer：摘除座位注册（宿主 fiber 拆解 = 级联卸载）。
     */
    const apply = (ctx: ClientContext): (() => void) =>
      ctx.slots.inject("settings.section", () => {
        const spec =
          typeof ctx.slots.spec === "function" ? ctx.slots.spec("settings.section") : undefined;
        if (spec === undefined) return () => {};
        return ctx.slots.register(
          // 官方样例形态（ui-settings 契约）：id 是座位键、order 是导航位、label 是显示名。
          // label 用字面量而非 `ctx.locale.bind(...)`：本卡不注入 locale 服务，
          // 少一个 fiber 依赖就少一个「等服务」卡点（官方也允许纯字符串 label）。
          { name: "settings.section", id: scope.seatId, order: 90, label: "Tuvis Remote" },
          TuvisBindingCard,
        );
      });

    /** 本入口真正依赖的 fiber 服务（只列用到的；不列 connection/locale）。 */
    const inject: string[] = ["slots"];

    const surface: ModuleSurface = { apply, inject, renderCardText };
    return surface as unknown as Record<string, unknown>;
  },
});
