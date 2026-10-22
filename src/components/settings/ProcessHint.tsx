import type { ReactNode } from "react";
import { Loader2 } from "lucide-react";

/**
 * 过程提示的统一长相（2026-10-22 四卡设计语言统一）：转圈 + 灰字。
 *
 * 「系统正在自动干活」的场景全部走这里——临时隧道取址、两套向导的 DNS 等待与复验、
 * 摘要的自动复验提示。统一前有四种等待长相（脉动灰字 / 琥珀字 / 琥珀块 / 转圈），
 * 用户无从把它认成同一个语义：「正在推进、暂不需要你操作」。
 *
 * 边界：错误与完成各有自己的台词，不得借用本组件；琥珀色块承载多句信息的组合块
 * （如 ts-pending）仍可用色块容器，内层的等待行换成本组件。
 */
export function ProcessHint({
  testId,
  children,
  className = "",
}: {
  testId?: string;
  children: ReactNode;
  className?: string;
}) {
  return (
    <p
      data-testid={testId}
      className={`text-muted-foreground flex items-center gap-1.5 text-xs ${className}`}
    >
      <Loader2 className="h-3 w-3 flex-none animate-spin" aria-hidden="true" />
      <span className="min-w-0">{children}</span>
    </p>
  );
}
