// tests/settings/autostartSection.test.ts — 开机自动启动 + 看板工具筛选接线锁
//（2026-10-11；源码锁，对齐 settingsBlocks.test.ts / UsageStatusSection.test.tsx
// 的静态扫描先例）：渲染整个 SettingsPage / HomePage 要备齐多套 mock，而本锁
// 判的只是静态接线存在性——挂块、命令链路、事件回流、mock parity、失败回滚。
import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

const SRC = readFileSync(path.join(process.cwd(), "src/pages/settings.tsx"), "utf8");
const MOCK = readFileSync(path.join(process.cwd(), "src/tauri-mock.ts"), "utf8");
const HOME = readFileSync(path.join(process.cwd(), "src/pages/home.tsx"), "utf8");
const TRAY = readFileSync(path.join(process.cwd(), "src-tauri/src/plugins/system_tray.rs"), "utf8");

describe("开机自动启动：接线锁", () => {
  it("① 设置页分区挂进桌面块（恰一次）且恰一处渲染守卫", () => {
    expect(SRC).toContain('sections: ["notifications", "pet", "autostart"]');
    const guards = SRC.match(/\{visibleIds\.has\("autostart"\)/g) ?? [];
    expect(guards).toHaveLength(1);
  });

  it("② 真值源命令（OS 侧实时读，不落 KV）+ 托盘事件回流 + 失败回滚", () => {
    expect(SRC).toContain('invoke<boolean>("get_autostart_enabled")');
    expect(SRC).toContain('invoke("set_autostart_enabled", { enabled: next })');
    expect(SRC).toContain('listen<{ enabled: boolean }>("autostart-changed"');
    // 乐观更新失败必须回滚（注册表/LaunchAgent 写入可能被系统策略拦截）
    expect(SRC).toContain("setAutostartEnabled(!next);");
  });

  it("③ 双 mock parity：浏览器/Playwright mock 两条命令都在场", () => {
    expect(MOCK).toContain('case "get_autostart_enabled"');
    expect(MOCK).toContain('case "set_autostart_enabled"');
  });

  it("④ 托盘接线：autostart 项、翻转臂与实时勾选态在 Rust 侧在场", () => {
    // 勾选态 = OS 侧实时真值（插件 is_enabled），三条重建路径统一走 helper
    expect(TRAY).toContain('CheckMenuItem::with_id(app, "autostart"');
    expect(TRAY).toContain('"autostart" => {');
    expect(TRAY).toContain("app.autolaunch().is_enabled()");
    expect(TRAY).toContain("flip_and_sync(app, !current)");
    // 读失败不猜方向（评审 M1）：Err 分支只告警不翻转
    expect(TRAY).toContain("当前档不可判定，不翻转");
  });
});

describe("看板工具筛选：接线锁", () => {
  it("⑤ home：chips 挂载 + localStorage 记忆 + 托盘标签传参", () => {
    expect(HOME).toContain("<ToolFilterChips");
    expect(HOME).toContain("loadToolFilter()");
    expect(HOME).toContain("saveToolFilter(filter)");
    expect(HOME).toContain('autostartText: t("tray.autostart")');
  });
});
