import { useEffect, useState } from "react";
import QRCode from "qrcode";
import { SETTINGS_REMOTE_QR_CAPTION } from "@/components/settings/typography";

/** Generate a QR only for the caller's already-verified address. */
export function ChannelQr({ url, caption }: { url: string; caption: string }) {
  const [dataUrl, setDataUrl] = useState<string | null>(null);
  useEffect(() => {
    let alive = true;
    setDataUrl(null);
    QRCode.toDataURL(url, { width: 132, margin: 1 })
      .then((data) => {
        if (alive) setDataUrl(data);
      })
      .catch((error: unknown) => console.error("qrcode render failed:", error));
    return () => {
      alive = false;
    };
  }, [url]);
  return (
    <div className="flex flex-none flex-col items-center gap-1">
      {dataUrl ? (
        <img src={dataUrl} alt={caption} className="h-[132px] w-[132px] rounded-lg border" />
      ) : (
        <div className="bg-muted/40 h-[132px] w-[132px] animate-pulse rounded-lg border" />
      )}
      <p className={`text-muted-foreground text-center ${SETTINGS_REMOTE_QR_CAPTION}`}>{caption}</p>
    </div>
  );
}
