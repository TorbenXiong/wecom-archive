import { Image, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { MessageItem } from "../domain/types";
import { getMediaObjectUrl } from "../lib/server-api";

export function ImageAttachment({ token, attachment }: { token: string; attachment: NonNullable<MessageItem["attachment"]> }) {
  const [objectUrl, setObjectUrl] = useState<string>();
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string>();
  const [previewOpen, setPreviewOpen] = useState(false);
  const button = useRef<HTMLButtonElement>(null);
  const controller = useRef<AbortController | undefined>(undefined);
  const pending = useRef<Promise<string> | undefined>(undefined);
  const loaded = useRef<string | undefined>(undefined);

  const load = () => {
    const signal = controller.current!.signal;
    if (!pending.current) {
      setLoading(true);
      setError(undefined);
      pending.current = getMediaObjectUrl(token, attachment.contentHash!, signal).then((url) => {
        if (signal.aborted) {
          URL.revokeObjectURL(url);
          signal.throwIfAborted();
        }
        loaded.current = url;
        setObjectUrl(url);
        return url;
      }).catch((cause) => {
        if (!signal.aborted) {
          pending.current = undefined;
          setError("图片读取失败，请点击重试。");
        }
        throw cause;
      }).finally(() => {
        if (!signal.aborted) setLoading(false);
      });
    }
    // Visibility and user clicks share the same request and successful result.
    return pending.current;
  };

  useEffect(() => {
    const abort = new AbortController();
    controller.current = abort;
    const element = button.current;
    let observer: IntersectionObserver | undefined;
    if (attachment.contentHash && element && typeof IntersectionObserver !== "undefined") {
      observer = new IntersectionObserver((entries) => {
        if (abort.signal.aborted || !entries.some((entry) => entry.isIntersecting)) return;
        observer?.disconnect();
        void load().catch(() => undefined);
      }, { root: element.closest(".message-scroll"), rootMargin: "120px 0px" });
      observer.observe(element);
    }
    return () => {
      abort.abort();
      observer?.disconnect();
      pending.current = undefined;
      if (loaded.current) URL.revokeObjectURL(loaded.current);
      loaded.current = undefined;
    };
  }, [token, attachment.contentHash]);

  const open = async () => {
    const signal = controller.current!.signal;
    try {
      await load();
      if (!signal.aborted) setPreviewOpen(true);
    } catch { /* The shared load reports errors; cancellation stays silent. */ }
  };

  return <>
    <button ref={button} className="image-preview" type="button" aria-label={`查看图片 ${attachment.name}`} aria-busy={loading} onClick={() => void open()} disabled={!attachment.contentHash}>
      {objectUrl ? <img src={objectUrl} alt={attachment.name} decoding="async" /> : <div className="mini-layout"><span /><span /><span /><span /></div>}
      <div><Image size={15} />{loading ? "正在读取…" : error || (!attachment.contentHash ? "未采集图片内容" : objectUrl ? attachment.meta : "点击加载图片")}</div>
    </button>
    {previewOpen && objectUrl && <div className="media-preview-backdrop" role="presentation" onMouseDown={() => setPreviewOpen(false)}><section className="media-preview-dialog" role="dialog" aria-modal="true" aria-label={`图片预览 ${attachment.name}`} onMouseDown={(event) => event.stopPropagation()}><button type="button" aria-label="关闭图片预览" onClick={() => setPreviewOpen(false)}><X size={20} /></button><img src={objectUrl} alt={attachment.name} /><span>{attachment.name}</span></section></div>}
  </>;
}
