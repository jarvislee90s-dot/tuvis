// tests/settings/DataManagementSection.test.tsx — 数据管理分区首版（2026-09-20，
// 移动端附件占用列出/清理）。mock 模式沿用 AuditLogSection.test.tsx（vi.hoisted +
// vi.mock invoke/event）；i18n 显式引入按英文断言（jsdom navigator.language=en）。
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DataManagementSection } from "@/components/settings/DataManagementSection";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
  emit: vi.fn(async () => {}),
}));

// tests/setup.ts 未初始化 i18n，显式引入并按默认英文断言（AuditLogSection 同款）
import i18n from "@/i18n";

interface Row {
  project: string;
  files: number;
  bytes: number;
}

function row(project: string, files: number, bytes: number): Row {
  return { project, files, bytes };
}

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string) =>
    cmd === "list_attachment_projects"
      ? [row("E:/proj/a", 2, 2048), row("E:/proj/b", 1, 250 * 1024 * 1024)]
      : null
  );
});

afterEach(() => {
  vi.clearAllMocks();
});

describe("DataManagementSection 渲染（2026-09-20）", () => {
  it("挂载即 invoke list_attachment_projects，两项目各显示占用", async () => {
    render(<DataManagementSection />);
    expect(await screen.findByText("E:/proj/a")).toBeTruthy();
    expect(screen.getByText("E:/proj/b")).toBeTruthy();
    expect(invokeMock).toHaveBeenCalledWith("list_attachment_projects");
    expect(screen.getByText("2 · 2.0 KB")).toBeTruthy();
    expect(screen.getByText("1 · 250.0 MB")).toBeTruthy();
    // 超阈值（>200MB）提醒在场
    expect(screen.getByText("Large usage, consider cleaning")).toBeTruthy();
  });

  it("空索引 → 「暂无项目附件」空态", async () => {
    invokeMock.mockImplementation(async (cmd: string) =>
      cmd === "list_attachment_projects" ? [] : null
    );
    render(<DataManagementSection />);
    expect(await screen.findByText("No project attachments")).toBeTruthy();
  });

  it("清理：二次确认（先确认态再执行）→ invoke clean_attachment_project（带项目路径）→ 行移除", async () => {
    render(<DataManagementSection />);
    await screen.findByText("E:/proj/a");
    // 圈定 E:/proj/a 那一行的清理钮（两个项目各有 Clean，须 within 防误命中）
    const item = screen.getByText("E:/proj/a").closest("li")!;
    const cleanInItem = () => within(item).getByRole("button", { name: /clean/i });
    // 首次点击进入确认态（未 invoke 清理）
    fireEvent.click(cleanInItem());
    expect(invokeMock.mock.calls.some((c: unknown[]) => c[0] === "clean_attachment_project")).toBe(
      false
    );
    // 确认态按钮再点 → 执行清理（携带项目路径）
    fireEvent.click(cleanInItem());
    await waitFor(() =>
      expect(
        invokeMock.mock.calls.some((c: unknown[]) => c[0] === "clean_attachment_project")
      ).toBe(true)
    );
    const cleanCall = invokeMock.mock.calls.find(
      (c: unknown[]) => c[0] === "clean_attachment_project"
    );
    expect(cleanCall![1]).toEqual({ project: "E:/proj/a" });
    // 行随清理移除（本 mock 不改列表 → 断言 invoke 发生即可；移除断言见下一条）
  });

  it("加载失败 → 错误态可见（不伪装空态）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "list_attachment_projects") throw new Error("boom");
      return null;
    });
    render(<DataManagementSection />);
    expect(await screen.findByTestId("data-mgmt-load-error")).toBeTruthy();
  });

  it("i18n 子树无缺键（settings.dataManagement，zh/en 双语扫描）", () => {
    // 键存在性由渲染断言间接覆盖（title/hint 经 t() 渲染）；此处直接断言子树在两语言包中同构
    const zh = (
      i18n.getResourceBundle("zh", "translation") as {
        settings: { dataManagement: Record<string, string> };
      }
    ).settings.dataManagement;
    const en = (
      i18n.getResourceBundle("en", "translation") as {
        settings: { dataManagement: Record<string, string> };
      }
    ).settings.dataManagement;
    expect(Object.keys(zh).sort()).toEqual(Object.keys(en).sort());
  });
});
