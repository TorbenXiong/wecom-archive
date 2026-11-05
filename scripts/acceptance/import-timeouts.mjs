import http from "node:http";
import { readFile, readdir } from "node:fs/promises";
import { join } from "node:path";
import { deadline, fail, localFetch, until } from "./runtime.mjs";

function slowImport(session, auth) {
  let request;
  const done = new Promise((resolve) => {
    request = http.request({ host: "127.0.0.1", port: session.port, path: "/api/v1/imports/json", method: "POST",
      headers: { Authorization: auth, "Content-Type": "application/json" } }, (response) => {
      const chunks = [];
      response.on("data", (chunk) => chunks.push(chunk));
      response.on("error", () => resolve({ disconnected: true }));
      response.on("end", () => {
        try { resolve({ status: response.statusCode, value: JSON.parse(Buffer.concat(chunks)) }); }
        catch { resolve({ invalidResponse: true }); }
      });
    });
    request.on("error", () => resolve({ disconnected: true }));
    request.on("close", () => resolve({ disconnected: true }));
    request.write("{");
  });
  return { request, done };
}

export async function timeoutCases({ root, desktop, main, fresh, check }) {
  let idle, active, tick;
  const controller = new AbortController();
  let waiting;
  const imports = (session) => readdir(join(session.dataRoot, "imports"));
  try {
    idle = slowImport(main, await desktop.auth(main));
    active = slowImport(fresh, await desktop.auth(fresh));
    tick = setInterval(() => active.request.write(" "), 10_000);
    await until(async () => (await imports(main)).length === 1 && (await imports(fresh)).length === 1);
    waiting = localFetch(fresh.origin, "/api/v1/imports/json", {
      method: "POST", headers: { Authorization: await desktop.auth(fresh) },
      body: await readFile(join(root, "fixture-1.json")), signal: controller.signal,
    }).then(async (response) => ({ status: response.status, value: await response.json() }));
    // Install rejection handling immediately, including for a server that exits mid-test.
    const responses = Promise.all([idle.done, waiting]);
    const health = await Promise.all([main, fresh].map(async (s) => (await localFetch(s.origin, "/api/v1/health", { signal: AbortSignal.timeout(5000) })).ok));
    const [received, queued] = await deadline(responses, 85_000, "IMPORT_TIMEOUT_CASE_EXCEEDED");
    check("import-idle-timeout", received.status === 408 && received.value?.code === "IMPORT_RECEIVE_TIMEOUT");
    check("import-queue-timeout", queued.status === 503 && queued.value?.code === "IMPORT_BUSY");
    clearInterval(tick); tick = undefined;
    active.request.destroy();
    await active.done;
    await until(async () => (await imports(main)).length === 0 && (await imports(fresh)).length === 0);
    for (const session of [main, fresh]) {
      const retry = await desktop.upload(session, "fixture-1.json");
      if (retry.status !== 200 || retry.value.inserted !== 0 || retry.value.unchanged !== 1) throw fail("IMPORT_TIMEOUT_RETRY_FAILED");
    }
    check("import-timeout-cleanup-retry");
    check("responsive-during-slow-import", health.every(Boolean));
  } finally {
    clearInterval(tick);
    controller.abort();
    idle?.request.destroy(); active?.request.destroy();
    await Promise.allSettled([idle?.done, active?.done, waiting]);
  }
}
