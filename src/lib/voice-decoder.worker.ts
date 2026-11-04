import { decode } from "silk-wasm";

self.onmessage = async ({ data }: MessageEvent<Uint8Array>) => {
  try {
    const result = await decode(data, 24_000);
    self.postMessage({ pcm: result.data }, { transfer: [result.data.buffer] });
  } catch {
    // Decoder errors can contain source details; expose only a generic failure.
    self.postMessage({ failed: true });
  }
};
