import { useTranslation } from "react-i18next";
import type { CodexDesktopObservation } from "@/lib/api/remote";

export default function CodexDesktopDiagnostic({
  observation,
}: {
  observation: CodexDesktopObservation;
}) {
  const { t } = useTranslation();
  return (
    <div data-testid="codex-desktop-observation" className="space-y-0.5 text-xs">
      <p>
        {t("mobile.codex.adapt.diag_endpoint")} ·{" "}
        {t(
          observation.endpoint === "connected"
            ? "mobile.codex.adapt.diag_connected"
            : "mobile.codex.adapt.diag_ownership_unknown"
        )}
      </p>
      <p>
        {t("mobile.codex.adapt.diag_accessibility")} ·{" "}
        {t(
          observation.accessibility === "authorized"
            ? "mobile.codex.adapt.diag_authorized"
            : observation.accessibility === "not_authorized"
              ? "mobile.codex.adapt.diag_not_authorized"
              : "mobile.codex.adapt.diag_not_applicable"
        )}
      </p>
      <p>
        {t("mobile.codex.adapt.diag_thread")} ·{" "}
        {t(
          observation.thread === "ipc_observed"
            ? "mobile.codex.adapt.diag_ipc_observed"
            : observation.thread === "stale"
              ? "mobile.codex.adapt.diag_stale"
              : "mobile.codex.adapt.diag_unknown"
        )}
      </p>
    </div>
  );
}
