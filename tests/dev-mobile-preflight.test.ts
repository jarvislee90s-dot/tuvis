// tests/dev-mobile-preflight.test.ts —— `pnpm tauri:dev` 的移动产物前置链回归（N02）。
//
// 缺陷现场：源码已更新（如 `dshRetryScope`、新版回执文案），但 `dist-mobile/`
// 仍是旧构建，`/m` 继续伺服旧入口与旧 hash JS —— 浏览器看到的是旧代码，而开发者
// 以为「源码改了就是新页面」。因此 `tauri:dev` 必须**先构建移动产物**再启动。
//
// 本文件锁三件事（纯静态契约，不启动 dev server）：
//  ① `tauri:dev` 脚本链包含 `build:mobile`，且位置在 `tauri dev` **之前**；
//  ② 链是 `&&` 串联（移动构建失败即中止，绝不带着旧产物启动）；
//  ③ `build:mobile` 本身产出 `dist-mobile/mobile.html` 入口（输出目录与入口名契约）。
// 真实 GET /m 与浏览器加载资源的验证在隔离联验中执行（见验收报告）。
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const pkg = JSON.parse(readFileSync(resolve(repoRoot, "package.json"), "utf8")) as {
  scripts: Record<string, string>;
};

describe("tauri:dev 移动产物前置链", () => {
  it("tauri:dev 在启动 tauri dev 之前构建移动产物", () => {
    const dev = pkg.scripts["tauri:dev"];
    expect(dev, "tauri:dev 脚本必须存在").toBeTruthy();
    const mobileAt = dev.indexOf("build:mobile");
    const tauriAt = dev.indexOf("tauri dev");
    expect(
      mobileAt,
      "tauri:dev 必须调用 build:mobile（否则 /m 伺服旧产物）"
    ).toBeGreaterThanOrEqual(0);
    expect(tauriAt).toBeGreaterThanOrEqual(0);
    expect(mobileAt, "build:mobile 必须先于 tauri dev 执行").toBeLessThan(tauriAt);
  });

  it("前置链用 && 串联：移动构建失败即中止，不带旧产物启动", () => {
    const dev = pkg.scripts["tauri:dev"];
    // 片段间必须是 &&（顺序执行且失败短路），不允许 ; 或 & 这类失败仍继续的分隔。
    const chain = dev.split("&&").map((part) => part.trim());
    expect(chain.length).toBeGreaterThanOrEqual(3);
    expect(chain.some((part) => part.includes("build:mobile"))).toBe(true);
    expect(dev).not.toMatch(/build:mobile[^&]*[;&](?!&)/);
  });

  it("build:mobile 产出 dist-mobile 入口 mobile.html（/m 的伺服目标）", () => {
    expect(pkg.scripts["build:mobile"]).toContain("vite.config.mobile.ts");
    const viteConfig = readFileSync(resolve(repoRoot, "vite.config.mobile.ts"), "utf8");
    expect(viteConfig).toContain('"dist-mobile"');
    // 入口 HTML 名由 vite 的 rollup 输入键决定：mobile.html。
    expect(viteConfig).toMatch(/mobile\.html/);
  });
});
