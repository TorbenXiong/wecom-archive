// @vitest-environment node
import { decode, encode } from "silk-wasm";
import { pcmToWav, silkDuration, voicePlaybackBlob } from "./voice-playback";

it("decodes a synthesized SILK voice and produces a playable mono WAV without changing the original", async () => {
  const pcm = new Int16Array(24_000);
  for (let index = 0; index < pcm.length; index++) pcm[index] = Math.round(Math.sin(index * Math.PI * 2 * 440 / 24_000) * 8_000);
  const encoded = await encode(pcm, 24_000);
  const before = encoded.data.slice();
  expect(silkDuration(encoded.data)).toBeCloseTo(1);
  const decoded = await decode(encoded.data, 24_000);
  const wav = pcmToWav(decoded.data);
  const header = new DataView(wav.buffer);
  expect(new TextDecoder().decode(wav.subarray(0, 4))).toBe("RIFF");
  expect(header.getUint16(22, true)).toBe(1);
  expect(header.getUint32(24, true)).toBe(24_000);
  expect(header.getUint32(40, true)).toBe(decoded.data.length);
  expect(decoded.data.length / 48_000).toBeCloseTo(1);
  expect(decoded.data.some((value) => value !== 0)).toBe(true);
  expect(encoded.data).toEqual(before);
});

it("accepts both SILK headers and a terminal marker, but rejects truncated packet framing", () => {
  const header = new TextEncoder().encode("#!SILK_V3");
  for (const prefix of [[], [2]]) {
    expect(silkDuration(new Uint8Array([...prefix, ...header, 1, 0, 42, 255, 255]))).toBe(0.02);
    expect(() => silkDuration(new Uint8Array([...prefix, ...header, 3, 0, 42]))).toThrow("不完整");
    expect(() => silkDuration(new Uint8Array([...prefix, ...header, 1]))).toThrow("不完整");
  }
  expect(silkDuration(new TextEncoder().encode("RIFF synthetic WAV"))).toBeUndefined();
  expect(() => pcmToWav(new Uint8Array(3))).toThrow("无效");
});

it("terminates an in-progress decoder when navigation cancels playback", async () => {
  let started!: () => void;
  const ready = new Promise<void>((resolve) => { started = resolve; });
  const terminate = vi.fn();
  vi.stubGlobal("Worker", class {
    onmessage = null;
    onerror = null;
    terminate = terminate;
    postMessage() { started(); }
  });
  try {
    const controller = new AbortController();
    const bytes = new TextEncoder().encode("#!SILK_V3\x01\x00x");
    const result = voicePlaybackBlob(new Blob([bytes]), controller.signal);
    await ready;
    controller.abort();
    await expect(result).rejects.toMatchObject({ name: "AbortError" });
    expect(terminate).toHaveBeenCalledOnce();
  } finally { vi.unstubAllGlobals(); }
});

it("rejects oversized or excessively long audio before starting a decoder", async () => {
  const signal = new AbortController().signal;
  await expect(voicePlaybackBlob(new Blob([new Uint8Array(10 * 1024 * 1024 + 1)]), signal)).rejects.toThrow("大小限制");
  const frames = new Uint8Array(9 + 30_001 * 3);
  frames.set(new TextEncoder().encode("#!SILK_V3"));
  for (let offset = 9; offset < frames.length; offset += 3) frames.set([1, 0, 42], offset);
  await expect(voicePlaybackBlob(new Blob([frames]), signal)).rejects.toThrow("长度限制");
});
