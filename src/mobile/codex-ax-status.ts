import i18n from "@/i18n";
import type { SendInfo } from "./api";

export function codexAxStatusCopy(info: SendInfo): string {
  if (info.injectable) return i18n.t("mobile.codex.adapt.axTextAvailable");
  if (info.reasonCode === "ax_action_unresolved") {
    return i18n.t("mobile.codex.composer.desktopAx_reason_unresolved");
  }
  return i18n.t("mobile.codex.adapt.axTextUnavailable");
}
