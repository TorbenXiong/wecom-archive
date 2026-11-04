import { Download } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { ReactNode } from "react";
import type { MessageItem } from "../domain/types";
import { getMediaBlob } from "../lib/server-api";
import { silkDuration, voicePlaybackBlob } from "../lib/voice-playback";

function secondsLabel(seconds: number): string {
  return `${Math.max(1, Math.ceil(seconds))}″`;
}

export function VoiceAttachment({ token, attachment, transcript }: { token: string; attachment: NonNullable<MessageItem["attachment"]>; transcript?: ReactNode }) {
  const [duration, setDuration] = useState<number>();
  const [elapsed, setElapsed] = useState(0);
  const [playing, setPlaying] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const row = useRef<HTMLDivElement>(null);
  const audio = useRef<HTMLAudioElement>(null);
  const original = useRef<Promise<Blob> | undefined>(undefined);
  const controller = useRef<AbortController | undefined>(undefined);
  const playbackUrl = useRef<string | undefined>(undefined);
  const loadOriginal = () => {
    if (!original.current) {
      const signal = controller.current?.signal;
      original.current = getMediaBlob(token, attachment.contentHash!, signal)
        .catch((cause) => {
          if (controller.current?.signal === signal) original.current = undefined;
          throw cause;
        });
    }
    return original.current;
  };

  useEffect(() => {
    const abort = new AbortController();
    controller.current = abort;
    const element = audio.current;
    const readDuration = async () => {
      try {
        const blob = await loadOriginal();
        // Only inspect visible messages, and only their packet framing. The
        // original stays available for saving even when decoding is unsupported.
        if (blob.size > 10 * 1024 * 1024) return;
        const total = silkDuration(new Uint8Array(await blob.arrayBuffer()));
        if (!abort.signal.aborted) setDuration(total);
      } catch { /* A playback/save attempt reports actionable errors. */ }
    };
    let observer: IntersectionObserver | undefined;
    if (attachment.contentHash) {
      if (typeof IntersectionObserver === "undefined") void readDuration();
      else {
        observer = new IntersectionObserver((entries) => {
          if (entries.some((entry) => entry.isIntersecting)) {
            observer?.disconnect();
            void readDuration();
          }
        }, { rootMargin: "120px" });
        if (row.current) observer.observe(row.current);
      }
    }
    return () => {
      abort.abort();
      observer?.disconnect();
      original.current = undefined;
      element?.pause();
      element?.removeAttribute("src");
      element?.load();
      if (playbackUrl.current) URL.revokeObjectURL(playbackUrl.current);
      playbackUrl.current = undefined;
    };
  }, [token, attachment.contentHash]);

  const togglePlayback = async () => {
    const element = audio.current;
    const signal = controller.current?.signal;
    if (!element || !signal || !attachment.contentHash || busy) return;
    if (!element.paused) { element.pause(); return; }
    setBusy(true);
    setError(undefined);
    try {
      if (!playbackUrl.current) {
        const blob = await voicePlaybackBlob(await loadOriginal(), signal);
        signal.throwIfAborted();
        playbackUrl.current = URL.createObjectURL(blob);
        element.src = playbackUrl.current;
      }
      signal.throwIfAborted();
      document.querySelectorAll<HTMLAudioElement>("audio[data-voice]").forEach((other) => {
        if (other !== element) other.pause();
      });
      if (element.ended) element.currentTime = 0;
      await element.play();
    } catch {
      if (!signal.aborted) setError("此语音暂时无法播放，可以保存原文件后重试。");
    } finally {
      if (!signal.aborted) setBusy(false);
    }
  };

  const save = async () => {
    const signal = controller.current?.signal;
    if (!attachment.contentHash || !signal || busy) return;
    setBusy(true);
    setError(undefined);
    let url: string | undefined;
    try {
      const blob = await loadOriginal();
      signal.throwIfAborted();
      url = URL.createObjectURL(blob);
      const link = document.createElement("a");
      link.href = url;
      link.download = attachment.name;
      document.body.appendChild(link);
      link.click();
      link.remove();
    } catch {
      if (!signal.aborted) setError("语音保存失败，请重试。");
    } finally {
      // Give the WebView download time to consume the URL before releasing it.
      if (url) setTimeout(() => URL.revokeObjectURL(url!), 1_000);
      if (!signal.aborted) setBusy(false);
    }
  };

  return <div className="voice-attachment" ref={row}>
    <div className="voice-row">
      <button className="voice-save" type="button" onClick={() => void save()} disabled={!attachment.contentHash || busy} aria-label="保存语音" title="保存原始语音"><Download size={16} /></button>
      <span className="voice-duration" aria-label="语音时长">{duration ? (elapsed > 0 ? `${Math.floor(elapsed)}″ / ${secondsLabel(duration)}` : secondsLabel(duration)) : "语音"}</span>
      <button className={`voice-bubble${playing ? " playing" : ""}`} type="button" onClick={() => void togglePlayback()} disabled={!attachment.contentHash || busy} aria-label={playing ? "暂停语音" : "播放语音"} aria-pressed={playing} title={busy ? "正在处理语音…" : playing ? "暂停语音" : "播放语音"} style={{ width: `${Math.min(190, 68 + (duration || 0) * 2)}px` }}>
        <span className="voice-wave" aria-hidden="true"><i /><i /><i /></span>
        {busy && <span className="voice-loading">···</span>}
      </button>
      {transcript && <div className="voice-transcript">{transcript}</div>}
    </div>
    {!attachment.contentHash && <p className="attachment-status" role="status">未采集语音内容。请使用“包含全内容”重新采集。</p>}
    {error && <p className="attachment-status" role="alert">{error}</p>}
    <audio ref={audio} data-voice="true" preload="metadata" onPlay={() => setPlaying(true)} onPause={() => setPlaying(false)} onEnded={() => { setPlaying(false); setElapsed(0); }} onTimeUpdate={() => setElapsed(audio.current?.currentTime || 0)} onLoadedMetadata={() => { const total = audio.current?.duration; if (total && Number.isFinite(total)) setDuration(total); }} onError={() => { setPlaying(false); setError("此语音暂时无法播放，可以保存原文件。"); }} />
  </div>;
}
