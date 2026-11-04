const SAMPLE_RATE = 24_000;
const MAX_VOICE_BYTES = 10 * 1024 * 1024;
const MAX_DURATION_SECONDS = 600;

export function silkDuration(bytes: Uint8Array): number | undefined {
  const start = bytes[0] === 2 ? 1 : 0;
  if (new TextDecoder().decode(bytes.subarray(start, start + 9)) !== "#!SILK_V3") return undefined;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  let offset = start + 9;
  let frames = 0;
  while (offset < bytes.length) {
    if (offset + 2 > bytes.length) throw new Error("语音文件不完整，请重新采集。");
    const length = view.getInt16(offset, true);
    offset += 2;
    if (length === -1 && offset === bytes.length) break;
    if (length <= 0 || offset + length > bytes.length) throw new Error("语音文件不完整，请重新采集。");
    offset += length;
    frames++;
  }
  // Cached WeCom SILK packets normally represent 20 ms. Playback replaces
  // this estimate with the duration of the actual decoded PCM samples.
  return frames ? frames * 0.02 : undefined;
}

export function pcmToWav(pcm: Uint8Array): Uint8Array<ArrayBuffer> {
  if (!pcm.length || pcm.length % 2 || pcm.length > SAMPLE_RATE * 2 * MAX_DURATION_SECONDS) {
    throw new Error("语音解码结果无效或超出播放长度限制，请保存原文件。");
  }
  const wav = new Uint8Array(44 + pcm.length);
  const view = new DataView(wav.buffer);
  for (const [offset, value] of [[0, "RIFF"], [8, "WAVE"], [12, "fmt "], [36, "data"]] as const) {
    wav.set(new TextEncoder().encode(value), offset);
  }
  view.setUint32(4, 36 + pcm.length, true);
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true);
  view.setUint16(22, 1, true);
  view.setUint32(24, SAMPLE_RATE, true);
  view.setUint32(28, SAMPLE_RATE * 2, true);
  view.setUint16(32, 2, true);
  view.setUint16(34, 16, true);
  view.setUint32(40, pcm.length, true);
  wav.set(pcm, 44);
  return wav;
}

export async function voicePlaybackBlob(original: Blob, signal: AbortSignal): Promise<Blob> {
  if (original.size > MAX_VOICE_BYTES) throw new Error("语音超出播放大小限制，请保存原文件。");
  const bytes = new Uint8Array(await original.arrayBuffer());
  signal.throwIfAborted();
  const estimatedDuration = silkDuration(bytes);
  if (estimatedDuration === undefined) return original;
  if (estimatedDuration > MAX_DURATION_SECONDS) throw new Error("语音超出播放长度限制，请保存原文件。");
  const pcm = await new Promise<Uint8Array>((resolve, reject) => {
    // Decode off the UI thread; terminate the decoder on completion, navigation
    // or timeout so malformed media cannot leave a worker running indefinitely.
    const worker = new Worker(new URL("./voice-decoder.worker.ts", import.meta.url), { type: "module" });
    const finish = (error?: Error, data?: Uint8Array) => {
      clearTimeout(timeout);
      signal.removeEventListener("abort", abort);
      worker.terminate();
      if (error) reject(error);
      else resolve(data!);
    };
    const abort = () => finish(new DOMException("Aborted", "AbortError"));
    const timeout = setTimeout(() => finish(new Error("语音解码超时，请保存原文件。")), 30_000);
    signal.addEventListener("abort", abort, { once: true });
    worker.onmessage = ({ data }: MessageEvent<{ pcm?: Uint8Array }>) => {
      if (data.pcm instanceof Uint8Array) finish(undefined, data.pcm);
      else finish(new Error("此语音暂时无法播放，可以保存原文件。"));
    };
    worker.onerror = (event) => {
      event.preventDefault();
      finish(new Error("语音解码器加载失败，可以保存原文件后重试。"));
    };
    worker.postMessage(bytes, [bytes.buffer]);
  });
  signal.throwIfAborted();
  return new Blob([pcmToWav(pcm)], { type: "audio/wav" });
}
