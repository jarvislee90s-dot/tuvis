/**
 * R03 客户端入口契约测试（RED 先行）——**跑真实构建产物**，不用测试工厂代替入口。
 *
 * 官方加载链（Mac 补探测报告 §3.2/§3.3）的硬契约：
 *  1. 产物必须自带经典脚本信封
 *     `window.__ModuleLoader__.load({ id: <裸包名>, factory: (require) => exports })`；
 *  2. 裸包名 id 必须是 `@tuvis/dsh-remote-access`（不是 `…/local-ui`、不是子路径）；
 *  3. `exports.apply` 必须是函数、`exports.inject` 必须是数组；
 *  4. 产物里**不得有顶层 import/export**（classic script 直接 SyntaxError）；
 *  5. 种子模块（react 等）只能经 loader 提供的 `require(spec)` 在运行期取用，
 *     不得内联进产物（宿主自带 React 18，双 React 会毁掉 hooks 身份）。
 *
 * 观测边界 = 磁盘上的 `lib/client.js`（由 `pnpm --filter @tuvis/dsh-remote-access build`
 * 产出；package.json 的 test 脚本已内置构建），在 node:vm 里以假 `window.__ModuleLoader__`
 * 与假种子表执行，断言信封、导出面、座位注册/卸载与产物形态。
 */

import { existsSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";
import { describe, expect, it } from "vitest";

const thisDir = dirname(fileURLToPath(import.meta.url));
const packageDir = resolve(thisDir, "..");
const artifactPath = join(packageDir, "lib", "client.js");

/** 官方裸包名口径（`stripClientSuffix` 之后的行名）。 */
const ENTRY_ID = "@tuvis/dsh-remote-access";

interface Registration {
  id: string;
  factory: (require: (spec: string) => unknown) => Record<string, unknown>;
}

/** 假 React：只提供入口初始化时真实需要的成员（不跑渲染）。 */
const fakeReact = {
  createElement: (type: unknown, props?: unknown, ...children: unknown[]) => ({ type, props, children }),
  useState: <T>(initial: T): [T, (next: T) => void] => [initial, () => {}],
  useEffect: () => {},
};

/** 在 vm 里执行产物文件，捕获 __ModuleLoader__ 注册与 require 读到的种子。 */
function loadArtifact(): {
  registrations: Registration[];
  requestedSpecs: string[];
  exportsOf: () => Record<string, unknown>;
  source: string;
} {
  expect(
    existsSync(artifactPath),
    `客户端产物缺失：${artifactPath}（先运行 pnpm --filter @tuvis/dsh-remote-access build；test 脚本已内置构建）`,
  ).toBe(true);

  const source = readFileSync(artifactPath, "utf8");
  const registrations: Registration[] = [];
  const requestedSpecs: string[] = [];
  const seedTable: Record<string, unknown> = { react: fakeReact };

  const sandbox: Record<string, unknown> = {
    window: {
      __ModuleLoader__: {
        load(registration: Registration) {
          registrations.push(registration);
        },
      },
    },
    fetch: async () => ({ ok: false, json: async () => ({}) }),
    console,
  };
  sandbox.globalThis = sandbox;
  vm.runInNewContext(source, sandbox, { filename: "client.js" });

  const exportsOf = (): Record<string, unknown> => {
    const registration = registrations[0];
    if (registration === undefined) throw new Error("产物未通过 __ModuleLoader__.load 注册");
    return registration.factory((spec: string) => {
      requestedSpecs.push(spec);
      const mod = seedTable[spec];
      if (mod === undefined) throw new Error(`测试种子表没有 ${spec}`);
      return mod;
    });
  };
  return { registrations, requestedSpecs, exportsOf, source };
}

/**
 * 在**同一个** `node:vm` 上下文里把产物执行两次（K2/I-2）。
 *
 * 为什么必须同上下文：官方宿主以 `<script src>` 下发插件 bundle；HMR / 代际换代
 * 会被宿主重新插入同一个 `<script>`，于是同一份源码在**同一个全局词法作用域**里
 * 第二次求值。classic script 语义下，顶层 `const`/`let`/`class` 重复声明是
 * `SyntaxError: Identifier 'x' has already been declared` —— 卡片会静默永不挂载
 * （宿主启动日志干净，正是 I-2 的现场）。官方 bundle 的顶层因此只有
 * `"use strict";` + 单条 `window.__ModuleLoader__.load({...})` 语句，无任何声明。
 */
function runArtifactTwiceInOneContext(): {
  registrations: Registration[];
  context: vm.Context;
  source: string;
} {
  expect(existsSync(artifactPath), `客户端产物缺失：${artifactPath}`).toBe(true);
  const source = readFileSync(artifactPath, "utf8");
  const registrations: Registration[] = [];
  const sandbox: Record<string, unknown> = {
    window: {
      __ModuleLoader__: {
        load(registration: Registration) {
          registrations.push(registration);
        },
      },
    },
    fetch: async () => ({ ok: false, json: async () => ({}) }),
    console,
  };
  sandbox.globalThis = sandbox;
  const context = vm.createContext(sandbox);
  // 两次求值都在**同一** context（同一全局词法作用域）。
  vm.runInContext(source, context, { filename: "client.js" });
  vm.runInContext(source, context, { filename: "client.js" });
  return { registrations, context: context, source };
}

/** 假 slots 服务：记录座位注册与卸载（座位在场/缺席两态）。 */
function fakeSlots(seat: "present" | "absent") {
  const registered: Array<{ options: Record<string, unknown>; component: unknown }> = [];
  const disposers: string[] = [];
  const slots = {
    spec: () => (seat === "present" ? { kind: "list" } : undefined),
    inject(key: string, callback: () => () => void): () => void {
      if (key !== "settings.section") throw new Error(`unexpected slot ${key}`);
      const disposeRegistration = callback();
      let disposed = false;
      return () => {
        if (disposed) return;
        disposed = true;
        disposers.push(key);
        disposeRegistration?.();
      };
    },
    register(options: Record<string, unknown>, component: unknown): () => void {
      registered.push({ options, component });
      let disposed = false;
      return () => {
        if (disposed) return;
        disposed = true;
        const index = registered.findIndex((entry) => entry.options === options);
        if (index >= 0) registered.splice(index, 1);
      };
    },
  };
  return { slots, registered, disposers };
}

describe("R03 客户端入口（真实产物）", () => {
  it("artifact_registers_official_envelope_with_bare_package_id", () => {
    const { registrations } = loadArtifact();

    expect(registrations).toHaveLength(1);
    expect(registrations[0]!.id).toBe(ENTRY_ID);
    expect(typeof registrations[0]!.factory).toBe("function");
  });

  it("artifact_exports_apply_and_inject", () => {
    const { exportsOf } = loadArtifact();
    const surface = exportsOf();

    expect(typeof surface["apply"]).toBe("function");
    expect(Array.isArray(surface["inject"])).toBe(true);
    // 只声明组件真正用到的 fiber 服务（不列不用的）
    expect(surface["inject"]).toEqual(["slots"]);
  });

  it("artifact_has_no_top_level_import_or_export", () => {
    const { source } = loadArtifact();

    // classic script 语义：顶层 import/export 直接 SyntaxError
    expect(source).not.toMatch(/^\s*(?:import|export)\s/m);
    expect(source).not.toMatch(/\bimport\s*\(/);
    // 种子模块只经 require(...) 取用，不内联 React 产物
    expect(source).toMatch(/require\(\s*["']react["']\s*\)/);
    expect(source).not.toMatch(/from\s*["']react["']/);
  });

  /**
   * K2（I-2）：产物必须可**重复注入**——同一份源码在同一全局作用域里求值两次
   * 都要成功注册。
   *
   * 官方宿主按 `<script src>` 下发 classic script；HMR 与代际换代会让宿主把同一
   * 个 `<script>` 重新插入，即同一份源码在同一全局词法作用域里第二次求值。
   * 旧产物顶层有 `const moduleLoader = ...`，第二次求值直接
   * `SyntaxError: Identifier 'moduleLoader' has already been declared`——卡片
   * 静默永不挂载（宿主日志干净，最坏形态：功能消失且无任何报错）。
   *
   * 契约：产物顶层只有 `"use strict";` + 单条 `window.__ModuleLoader__.load(...)`
   * 语句，**零声明**；loader 的取用移进 factory 内。
   */
  it("artifact_can_be_executed_twice_in_the_same_global_scope", () => {
    const { registrations } = runArtifactTwiceInOneContext();

    // 两次求值都必须完成注册（不是只成功第一次）
    expect(registrations).toHaveLength(2);
    for (const registration of registrations) expect(registration.id).toBe(ENTRY_ID);
  });

  it("artifact_top_level_declares_nothing_but_the_loader_call", () => {
    const { source } = loadArtifact();

    // 顶层（第 0 列）不得出现任何声明关键字——官方 bundle 同款形状。
    // 缩进行（factory 内部）不受限。
    expect(source).not.toMatch(/^(?:const|let|var|class|function)\s/m);
    // loader 只经一条顶层语句自注册
    expect(source).toMatch(/^\s*window\.__ModuleLoader__\.load\(/m);
  });

  it("apply_mounts_settings_section_card_and_disposer_cleans_up", () => {
    const { exportsOf } = loadArtifact();
    const surface = exportsOf();
    const apply = surface["apply"] as (ctx: unknown) => () => void;
    const fake = fakeSlots("present");

    const dispose = apply({ slots: fake.slots });

    expect(fake.registered).toHaveLength(1);
    expect(fake.registered[0]!.options).toMatchObject({
      name: "settings.section",
      id: "tuvis-remote",
    });
    expect(typeof fake.registered[0]!.component).toBe("function");
    expect(typeof dispose).toBe("function");

    // 卸载（fiber 拆解级联）= 座位注册被摘除
    dispose();
    expect(fake.registered).toEqual([]);
    expect(fake.disposers).toEqual(["settings.section"]);
    // 二次 dispose 幂等
    expect(() => dispose()).not.toThrow();
  });

  it("apply_tolerates_absent_seat_without_guessing", () => {
    const { exportsOf } = loadArtifact();
    const surface = exportsOf();
    const apply = surface["apply"] as (ctx: unknown) => () => void;
    const fake = fakeSlots("absent");

    const dispose = apply({ slots: fake.slots });

    // 座位未声明 → 不强行 register（社区插件 slots.spec 探针同款纪律）
    expect(fake.registered).toEqual([]);
    expect(() => dispose()).not.toThrow();
  });

  it("card_text_shows_binding_state_and_compare_code_only", () => {
    const { exportsOf } = loadArtifact();
    const surface = exportsOf();
    const renderCardText = surface["renderCardText"] as (snap: unknown) => string[];

    const pending = renderCardText({ state: "pending", bindingId: "binding-1", displayCode: "AB23CD" });
    expect(pending.join(" ")).toContain("AB23CD");
    expect(pending.join(" ")).toContain("pending");

    // 只展示状态与核对码，不展示任何凭据材料 / bindingId 之外的内部字段
    expect(pending.join(" ")).not.toContain("binding-1");

    expect(renderCardText({ state: "bound", bindingId: "binding-9", displayCode: null }).join(" ")).toContain("bound");
    expect(renderCardText({ state: "revoked", bindingId: null, displayCode: null }).join(" ")).toContain("revoked");
    expect(renderCardText(null).join(" ")).toContain("unavailable");
  });
});
