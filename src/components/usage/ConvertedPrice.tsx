// 汇率换算价标识（2026-10-20 用户裁决）：原生币种价格正常显示；
// 由另一种币种按当日汇率换算来的金额/单价加虚线下划线，并带可访问的说明。
import type { ReactNode } from "react";

export interface ConvertedPriceProps {
  converted: boolean;
  hint: string;
  children: ReactNode;
  /** DOM 定位（费用表/明细用） */
  testId?: string;
}

export function ConvertedPrice({ converted, hint, children, testId }: ConvertedPriceProps) {
  if (!converted) {
    return (
      <span data-testid={testId} data-converted="false">
        {children}
      </span>
    );
  }
  return (
    <span
      data-testid={testId}
      data-converted="true"
      title={hint}
      className="decoration-muted-foreground underline decoration-dashed underline-offset-2"
    >
      {children}
      <span className="sr-only"> ({hint})</span>
    </span>
  );
}
