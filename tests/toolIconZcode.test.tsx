// tests/toolIconZcode.test.tsx — ZCode 图标专项（2026-10-14 徽标统一批重写）
// 源码侧：zcode 图标随统一批改为官方参考图重绘（BrandIcons.tsx：白底板 +
// 炭灰 #2D2D2D 圆角方 + 白色斜切 Z），旧自绘 SVG 的 --tool-zcode-rim 变量与
// 蓝紫渐变（#3B5BFD/#8A4FF5）已退役。本文件锁：炭灰底色 + 白 Z path 几何
// 片段 + 旧体系残留负断言。与 workbuddy 等其余工具同在 toolIcon.test.tsx
// 做委托渲染/色值锁，本文件只做 zcode 形状专项。
import { render } from "@testing-library/react";
import { describe, it, expect } from "vitest";
import { ToolIcon } from "@/components/common/ToolIcon";

describe("ToolIcon zcode", () => {
  it("renders the official charcoal tile with the white Z path（2026-10-14 官方重绘）", () => {
    const { container } = render(<ToolIcon toolId="zcode" />);
    const html = container.innerHTML;
    // 炭灰圆角方（官方标重绘定案色）
    expect(html).toContain("#2D2D2D");
    // 白色斜切 Z 的 path 数据片段（横杠折点与斜杠脚点，锁几何不被静默改回旧形）
    expect(html).toContain("L260 348");
    // 旧体系残留负断言：CSS 变量与蓝紫渐变已随统一批退役
    expect(html).not.toContain("--tool-zcode-rim");
    expect(html).not.toContain("#3B5BFD");
    expect(html).not.toContain("#8A4FF5");
  });

  it("zcode icon renders an svg glyph without crashing", () => {
    const { container } = render(<ToolIcon toolId="zcode" size={20} />);
    expect(container.querySelector("svg")).toBeTruthy();
  });
});
