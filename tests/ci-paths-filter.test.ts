import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

// Vite 已依赖 YAML 解析器；从 Vite 的依赖树解析，避免依赖 pnpm 的扁平化布局。
const require = createRequire(import.meta.url);
const viteRequire = createRequire(require.resolve("vite"));
const yaml = viteRequire("yaml") as { parse: (source: string) => unknown };
const picomatch = viteRequire("picomatch") as (
  pattern: string,
  options: { dot: boolean }
) => (path: string) => boolean;

const workflow = yaml.parse(
  readFileSync(resolve(process.cwd(), ".github/workflows/ci.yml"), "utf8")
) as {
  jobs: {
    changes: { steps: Array<{ id?: string; uses?: string; with?: { filters?: string } }> };
    frontend: {
      steps: Array<{ run?: string; if?: string; "working-directory"?: string }>;
    };
  };
};
const filterStep = workflow.jobs.changes.steps.find((step) => step.id === "filter");
if (!filterStep?.uses?.startsWith("dorny/paths-filter@") || !filterStep.with?.filters) {
  throw new Error("CI changes job 缺少 paths-filter 配置");
}
const filters = yaml.parse(filterStep.with.filters) as Record<string, string[]>;

function triggers(area: "frontend" | "backend", path: string): boolean {
  const patterns = filters[area];
  if (!Array.isArray(patterns) || !patterns.every((pattern) => typeof pattern === "string")) {
    throw new Error(`无效的 ${area} 路径过滤器`);
  }
  return patterns.some((pattern) => picomatch(pattern, { dot: true })(path));
}

describe("CI frontend DSH checks", () => {
  const steps = workflow.jobs.frontend.steps;
  const installIndex = steps.findIndex(
    (step) =>
      !step.if &&
      !step["working-directory"] &&
      step.run?.trim() === "pnpm install --frozen-lockfile"
  );

  it("runs the DSH package tests after the frozen install", () => {
    expect(installIndex).toBeGreaterThanOrEqual(0);
    const testIndex = steps.findIndex((step) => {
      if (step.if) return false;
      const command = step.run?.trim();
      const directory = step["working-directory"]?.replace(/^\.\//, "");
      return (
        (!directory && command === "pnpm --filter @tuvis/dsh-remote-access test") ||
        (!directory && command === "pnpm -C packages/dsh-remote-access test") ||
        (directory === "packages/dsh-remote-access" && command === "pnpm test")
      );
    });
    expect(testIndex).toBeGreaterThan(installIndex);
  });

  it("checks the DSH archive after the frozen install", () => {
    expect(installIndex).toBeGreaterThanOrEqual(0);
    const checkIndex = steps.findIndex(
      (step) =>
        !step.if &&
        !step["working-directory"] &&
        step.run?.trim() === "node scripts/build-dsh-plugin.mjs --check"
    );
    expect(checkIndex).toBeGreaterThan(installIndex);
  });
});

describe("CI paths-filter", () => {
  it.each([
    "packages/dsh-remote-access/src/index.ts",
    "packages/dsh-remote-access/package.json",
    "packages/dsh-remote-access/tsconfig.json",
    "packages/dsh-remote-access/.config/settings.json",
    "packages/dsh-remote-access/test/nested/example.test.ts",
    "resources/dsh/remote-access.tgz",
    "resources/dsh/nested/config.json",
  ])("triggers frontend when only %s changes", (path) => {
    expect(triggers("frontend", path)).toBe(true);
  });

  it.each(["resources/dsh/remote-access.tgz", "resources/dsh/nested/config.json"])(
    "triggers backend when only %s changes",
    (path) => {
      expect(triggers("backend", path)).toBe(true);
    }
  );

  it.each(["frontend", "backend"] as const)(
    "does not trigger %s for an ordinary documentation change",
    (area) => {
      expect(triggers(area, "docs/product/manual/zh/overview.md")).toBe(false);
    }
  );
});
