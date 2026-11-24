import { useEffect, useRef, useState } from "react";
import i18n from "@/i18n";
import { fetchDshMedia, type DshMediaReference } from "./api";

export default function DshInlineMedia({
  media,
  instanceId,
  sessionId,
  onOpen,
}: {
  media: DshMediaReference;
  instanceId?: string;
  sessionId: string;
  onOpen: () => void;
}) {
  const button = useRef<HTMLButtonElement>(null);
  const [image, setImage] = useState<{ url?: string; failed?: boolean }>({});
  const imageAvailable = media.kind === "image" && media.unavailable !== true;
  useEffect(() => {
    if (!imageAvailable || !instanceId) return;
    const controller = new AbortController();
    let objectUrl: string | undefined;
    const start = () => {
      void fetchDshMedia(instanceId, sessionId, media.refId, undefined, controller.signal)
        .then((payload) => {
          if (payload.kind === "image") {
            if (controller.signal.aborted) URL.revokeObjectURL(payload.url);
            else {
              objectUrl = payload.url;
              setImage({ url: payload.url });
            }
          } else if (!controller.signal.aborted) setImage({ failed: true });
        })
        .catch(() => {
          if (!controller.signal.aborted) setImage({ failed: true });
        });
    };
    // 不为离屏图片建立请求，没有观察器时仍可点击打开预览。
    const target = button.current;
    if (!target || typeof IntersectionObserver !== "function") return;
    const observer = new IntersectionObserver((entries) => {
      if (!entries.some((entry) => entry.isIntersecting)) return;
      observer.disconnect();
      start();
    });
    observer.observe(target);
    return () => {
      controller.abort();
      observer.disconnect();
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [imageAvailable, instanceId, media.refId, sessionId]);

  return (
    <button
      type="button"
      ref={button}
      aria-label={media.name}
      onClick={onOpen}
      className="my-1 block max-w-full rounded-md border border-[var(--cb)] px-2 py-1 text-left text-xs"
    >
      {image.url && !image.failed ? (
        <img
          src={image.url}
          alt={media.name}
          onError={() => setImage({ failed: true })}
          className="max-h-56 max-w-full rounded object-contain"
        />
      ) : (
        media.name
      )}
      {(media.unavailable || image.failed || !instanceId) && (
        <span className="ml-1 text-[var(--mut)]">
          {i18n.t("mobile.preview.dshMediaUnavailable")}
        </span>
      )}
    </button>
  );
}
