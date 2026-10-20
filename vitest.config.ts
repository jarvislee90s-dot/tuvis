import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import path from "path";

export default defineConfig({
  plugins: [react()],
  test: {
    environment: "jsdom",
    globals: true,
    include: ["tests/**/*.test.{ts,tsx}"],
    setupFiles: ["./tests/setup.ts"],
    // Node >= 25 的实验性 Web Storage 会在 globalThis 上定义返回 undefined 的 localStorage getter，
    // 遮蔽 jsdom 实现；关闭后由 jsdom 原生接管（消除 ExperimentalWarning）
    execArgv: ["--no-experimental-webstorage"],
    // Windows 并发上限（2026-10-19 最终回归实测）：Vitest 5 默认 maxWorkers =
    // availableParallelism() - 1（20 核机器 → 19 个 fork），jsdom 环境下 19 路并行会让
    // 各 worker 事件循环饥饿——waitFor 默认 1s / 用例默认 5s 预算在高负载下被拉爆，
    // 且失败集合每次不同（SessionDetail 滚动补偿、remoteSection 随机按钮、
    // v2-acceptance 用例 1、v2-provider-alias-ui 预览等；同一批用例定向跑/4 workers 全绿）。
    // 仅 Windows 限并发；Linux/macOS（含 CI）保持 Vitest 默认，行为不变。
    ...(process.platform === "win32" ? { maxWorkers: 4 } : {}),
    coverage: {
      provider: "v8",
      reporter: ["text", "html"],
      exclude: ["node_modules/", "tests/", "src/lib/api/", "src/components/ui/"],
    },
  },
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
});
