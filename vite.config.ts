import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

const host = process.env.TAURI_DEV_HOST;

export default defineConfig(({ mode }) => {
  const target = mode === "client" ? "client" : "server";
  return {
    plugins: [react()],
    clearScreen: false,
    // The collector is loaded through Tauri's embedded asset protocol rather
    // than a normal HTTP origin. Absolute `/assets/...` URLs can therefore
    // resolve outside the bundled frontend and leave the native window blank.
    // Keep the workspace's normal root-relative URLs while making the
    // standalone collector self-contained.
    base: target === "client" ? "./" : "/",
    define: {
      "import.meta.env.VITE_APP_TARGET": JSON.stringify(target),
    },
    server: {
      port: target === "client" ? 1420 : 1430,
      strictPort: true,
      host: host || false,
      proxy: target === "server" ? { "/api": "http://127.0.0.1:9812" } : undefined,
      hmr: host
        ? {
            protocol: "ws",
            host,
            port: 1421,
          }
        : undefined,
    },
    envPrefix: ["VITE_", "TAURI_ENV_*"],
    build: {
      outDir: `dist/${target}`,
      target: process.env.TAURI_ENV_PLATFORM === "windows" ? "chrome105" : "safari13",
      minify: process.env.TAURI_ENV_DEBUG ? false : "oxc",
      sourcemap: Boolean(process.env.TAURI_ENV_DEBUG),
    },
    test: {
      environment: "jsdom",
      setupFiles: ["./src/test/setup.ts"],
      globals: true,
      css: true,
    },
  };
});
