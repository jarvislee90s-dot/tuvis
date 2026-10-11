import { useTranslation } from "react-i18next";
import { SessionCard } from "@/components/sessions/SessionCard";
import { pairingAmbiguitySet, pairingKey } from "@/lib/pairing";
import type { Session } from "@/types/session";
import { Monitor } from "lucide-react";

export function SessionGrid({
  sessions,
  filterToolLabel,
}: {
  sessions: Session[];
  /** 「按工具筛选」空态专用：在场 = 当前筛选了某工具且筛空 → 空态文案换成
   *  「该工具暂无会话」（计数条不动，与移动端口径一致）；缺省 = 无筛选的通用空态 */
  filterToolLabel?: string;
}) {
  const { t } = useTranslation();
  // issue #48：同工具同项目双开的会话标记「配对不确定」角标（与跳转侧判定同口径）
  const ambiguity = pairingAmbiguitySet(sessions);
  if (sessions.length === 0) {
    return (
      <div className="flex flex-1 flex-col items-center justify-center gap-3 py-20 text-center">
        <Monitor className="text-muted-foreground/40 h-12 w-12" />
        <div>
          <p className="text-muted-foreground text-sm">
            {filterToolLabel
              ? t("sessions.filterEmpty", { tool: filterToolLabel })
              : t("sessions.empty")}
          </p>
          {!filterToolLabel && (
            <p className="text-muted-foreground/60 mt-1 text-xs">{t("sessions.emptyHint")}</p>
          )}
        </div>
      </div>
    );
  }

  return (
    <div className="grid grid-cols-1 gap-2 md:grid-cols-2 xl:grid-cols-3 2xl:grid-cols-4">
      {sessions.map((session) => (
        <SessionCard
          key={`${session.agentType}-${session.id}`}
          session={session}
          pairingAmbiguous={ambiguity.has(pairingKey(session))}
        />
      ))}
    </div>
  );
}
