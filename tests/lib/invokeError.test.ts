// invokeError 泛化解析的单测（#123/#150/#157 错误码全量迁移的机制基座）。
// 约定：形态 A `CODE: 原句` / 形态 B `CODE:机器参数: 原句` / 未知码回退原文。
// fake t 模拟 i18next：键存在则插值渲染，不存在返回键本身（真实行为）。
import { describe, expect, it } from "vitest";
import { formatInvokeError } from "@/lib/invokeError";

const dict: Record<string, string> = {
  "errors.toolDisabled": "工具 {{tools}} 未启用",
  "errors.applyInProgress": "保存中",
  "errors.presetActive": "预设 {{param}} 正在激活",
  "errors.presetNativeInUniversal": "通用预设不得含原生技能 {{param}}",
  "errors.remoteNoTunnelToken": "Custom domain needs a Tunnel Token",
  "errors.sessionPidGone": "Process {{param}} no longer exists",
};
const t = (key: string, opts?: Record<string, unknown>) => {
  const tpl = dict[key];
  if (tpl === undefined) return key;
  if (!opts) return tpl;
  return tpl.replace(/\{\{(\w+)\}\}/g, (_, k) => String(opts[k] ?? ""));
};

describe("formatInvokeError（错误码解析）", () => {
  it("legacy 三码保持原口径（含参数捕获）", () => {
    expect(formatInvokeError("W5_TOOL_DISABLED:claude", t)).toBe("工具 claude 未启用");
    expect(formatInvokeError("W5_APPLY_IN_PROGRESS: task", t)).toBe("保存中");
    expect(formatInvokeError("PRESET_ACTIVE:p1", t)).toBe("预设 p1 正在激活");
    expect(
      formatInvokeError("PRESET_NATIVE_IN_UNIVERSAL:skill-x", t)
    ).toBe("通用预设不得含原生技能 skill-x");
  });

  it("形态 A：已知码整句替换（en 语义句）", () => {
    expect(
      formatInvokeError("REMOTE_NO_TUNNEL_TOKEN: 自有域名缺少 Tunnel Token", t)
    ).toBe("Custom domain needs a Tunnel Token");
  });

  it("形态 B：机器参数经 {{param}} 插值", () => {
    expect(
      formatInvokeError("SESSION_PID_GONE:12345: 进程 12345 不存在", t)
    ).toBe("Process 12345 no longer exists");
  });

  it("未知码 / 无码 / 纯英文错误一律原文回退（不静默吞）", () => {
    expect(formatInvokeError("SOME_FUTURE_CODE: 还没迁移的域", t)).toBe(
      "SOME_FUTURE_CODE: 还没迁移的域"
    );
    expect(formatInvokeError("纯中文错误没有码", t)).toBe("纯中文错误没有码");
    expect(formatInvokeError("Failed to fetch", t)).toBe("Failed to fetch");
  });

  it("legacy 键缺失的中间态也走原文回退", () => {
    const tEmpty = (key: string) => key;
    expect(formatInvokeError("PRESET_ACTIVE:p1", tEmpty)).toBe("PRESET_ACTIVE:p1");
  });

  it("非字符串输入先 String 化再判定", () => {
    expect(formatInvokeError({ msg: "x" } as unknown, t)).toBe('[object Object]');
    expect(formatInvokeError(42, t)).toBe("42");
  });
});
