import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { createServer, createConnection } from "node:net";
import { mkdir, readFile, writeFile, readdir, realpath, lstat } from "node:fs/promises";
import { resolve, dirname, join, relative, isAbsolute } from "node:path";
import { fileURLToPath } from "node:url";

export const repoRoot = fileURLToPath(new URL("../../", import.meta.url));
export const pause = (ms) => new Promise((done) => setTimeout(done, ms));
export const fail = (code) => Object.assign(new Error(code), { code });
export const digest = (bytes) => createHash("sha256").update(bytes).digest("hex");
export async function deadline(promise, timeout, code) {
  let timer;
  try { return await Promise.race([promise, new Promise((_, reject) => { timer = setTimeout(() => reject(fail(code)), timeout); })]); }
  finally { clearTimeout(timer); }
}
export function within(root, path) {
  const child = relative(resolve(root), resolve(path));
  return child !== "" && child !== ".." && !child.startsWith("..\\") && !child.startsWith("../") && !isAbsolute(child);
}
export async function until(action, timeout = 30_000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    const result = await action();
    if (result) return result;
    await pause(100);
  }
  throw fail("ACCEPTANCE_TIMEOUT");
}
export async function localFetch(origin, path, options = {}) {
  const url = new URL(path, origin);
  if (url.hostname !== "127.0.0.1" || url.origin !== origin || url.protocol !== "http:") throw fail("NONLOCAL_TEST_REQUEST");
  return fetch(url, { ...options, redirect: "error", signal: options.signal ?? AbortSignal.timeout(90_000) });
}
export async function availablePort() {
  const server = createServer();
  await new Promise((done, reject) => { server.once("error", reject); server.listen(0, "127.0.0.1", done); });
  const port = server.address().port;
  await new Promise((done, reject) => server.close((error) => error ? reject(error) : done()));
  return port;
}
export async function createRun() {
  const target = resolve(repoRoot, "target");
  await mkdir(target, { recursive: true });
  if ((await lstat(target)).isSymbolicLink() || !within(repoRoot, await realpath(target))) throw fail("UNSAFE_ACCEPTANCE_ROOT");
  const base = resolve(target, "acceptance");
  await mkdir(base, { recursive: true });
  if ((await lstat(base)).isSymbolicLink() || !within(await realpath(target), await realpath(base))) throw fail("UNSAFE_ACCEPTANCE_ROOT");
  const root = join(base, `desktop-${Date.now()}-${randomUUID()}`);
  await mkdir(root);
  await writeFile(join(root, ".synthetic-run"), "desktop-acceptance.v1", { flag: "wx" });
  return root;
}
export async function validateRun(root) {
  const base = await realpath(resolve(repoRoot, "target/acceptance"));
  if (!within(repoRoot, base) || !within(base, await realpath(root)) || await readFile(join(root, ".synthetic-run"), "utf8") !== "desktop-acceptance.v1") throw fail("UNSAFE_ACCEPTANCE_ROOT");
}
export async function fileHashes(root) {
  const hashes = {};
  async function visit(folder) {
    for (const entry of (await readdir(folder, { withFileTypes: true })).sort((a, b) => a.name.localeCompare(b.name))) {
      const path = join(folder, entry.name);
      if ((await lstat(path)).isSymbolicLink()) throw fail("UNEXPECTED_TEST_LINK");
      if (entry.isDirectory()) await visit(path);
      else hashes[relative(root, path).replaceAll("\\", "/")] = digest(await readFile(path));
    }
  }
  await visit(root);
  return hashes;
}

export async function connect(debugPort, serverPort) {
  const origin = `http://127.0.0.1:${debugPort}`;
  const target = await until(async () => {
    try {
      const targets = await (await localFetch(origin, "/json/list", { signal: AbortSignal.timeout(1000) })).json();
      return targets.find((item) => item.type === "page" && item.url.startsWith(`http://127.0.0.1:${serverPort}/`));
    } catch { return false; }
  });
  const address = new URL(target.webSocketDebuggerUrl);
  if (address.protocol !== "ws:" || address.hostname !== "127.0.0.1" || Number(address.port) !== debugPort) throw fail("INVALID_CDP_ADDRESS");
  const ws = new WebSocket(address);
  const pending = new Map();
  let sequence = 0;
  const disconnect = () => {
    for (const entry of pending.values()) { clearTimeout(entry.timer); entry.reject(fail("CDP_CLOSED")); }
    pending.clear();
  };
  ws.addEventListener("close", disconnect);
  ws.addEventListener("error", disconnect);
  ws.addEventListener("message", ({ data }) => {
    const message = JSON.parse(data), entry = pending.get(message.id);
    if (!entry) return;
    pending.delete(message.id); clearTimeout(entry.timer);
    if (message.error) entry.reject(fail("CDP_COMMAND_FAILED")); else entry.resolve(message.result);
  });
  try {
    await new Promise((done, reject) => {
      const timer = setTimeout(() => reject(fail("CDP_CONNECT_TIMEOUT")), 10_000);
      ws.addEventListener("open", () => { clearTimeout(timer); done(); }, { once: true });
      ws.addEventListener("error", () => { clearTimeout(timer); reject(fail("CDP_CONNECT_FAILED")); }, { once: true });
    });
  } catch (error) { ws.close(); throw error; }
  const send = (method, params = {}) => new Promise((done, reject) => {
    if (ws.readyState !== WebSocket.OPEN) return reject(fail("CDP_CLOSED"));
    const id = ++sequence;
    const timer = setTimeout(() => { pending.delete(id); reject(fail("CDP_COMMAND_TIMEOUT")); }, 20_000);
    pending.set(id, { resolve: done, reject, timer }); ws.send(JSON.stringify({ id, method, params }));
  });
  const evaluate = async (expression) => {
    const result = await send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
    if (result.exceptionDetails) throw fail("PAGE_EVALUATION_FAILED");
    return result.result?.value;
  };
  return { send, evaluate, wait: (expression, timeout) => until(() => evaluate(expression), timeout), close() { disconnect(); ws.close(); } };
}

export class DesktopRun {
  constructor(root, executable) { this.root = root; this.executable = executable; this.sessions = new Map(); this.ports = new Set(); this.cleanupFailures = []; }
  async port() {
    let port;
    do { port = await availablePort(); } while (this.ports.has(port));
    this.ports.add(port); return port;
  }
  async start(name) {
    if (!/^[a-z]+$/.test(name)) throw fail("INVALID_INSTANCE_NAME");
    if (this.sessions.has(name) && !this.sessions.get(name).stopped) throw fail("INSTANCE_ALREADY_RUNNING");
    await validateRun(this.root);
    const port = await this.port(), debugPort = await this.port();
    const dataRoot = join(this.root, `${name}-data`);
    const child = spawn(this.executable, [], {
      cwd: dirname(this.executable), windowsHide: true, stdio: "ignore",
      env: { ...process.env, WECOM_ARCHIVE_SERVER_DATA: dataRoot, WECOM_ARCHIVE_SERVER_LISTEN: `127.0.0.1:${port}`,
        WECOM_ARCHIVE_SERVER_LOG: "off", WECOM_ARCHIVE_COLLECTOR_LOG: "off",
        WEBVIEW2_USER_DATA_FOLDER: join(this.root, `webview-${name}-${debugPort}`),
        WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${debugPort} --remote-debugging-address=127.0.0.1 --disable-background-networking` },
    });
    const session = { name, child, dataRoot, port, debugPort, origin: `http://127.0.0.1:${port}`, stopped: false };
    session.exited = new Promise((done) => { child.once("exit", () => { session.stopped = true; done(); }); child.once("error", () => { session.stopped = true; done(); }); });
    this.sessions.set(name, session);
    await until(async () => {
      if (session.stopped) throw fail("DESKTOP_START_FAILED");
      try { const response = await localFetch(session.origin, "/api/v1/health", { signal: AbortSignal.timeout(1000) }); return response.ok; } catch { return false; }
    }, 45_000);
    const c = await connect(debugPort, port);
    try { await c.wait("!!document.querySelector('.app-shell')"); } finally { c.close(); }
    return session;
  }
  async auth(session) {
    if (session.auth) return session.auth;
    const c = await connect(session.debugPort, session.port);
    try {
      await c.evaluate(`(() => { const previous=window.__acceptanceAuth; if(previous)window.fetch=previous.native; const s=window.__acceptanceAuth={native:fetch.bind(window),auth:null}; window.fetch=(input,init={})=>{const h=new Headers(init.headers||{}).get('authorization');if(h)s.auth=h;return s.native(input,init);}; const nav=[...document.querySelectorAll('.primary-nav button')]; nav.find(b=>b.textContent==='设置').click(); })()`);
      await c.wait("!!document.querySelector('.settings-page')");
      await c.evaluate("[...document.querySelectorAll('.primary-nav button')].find(b=>b.textContent==='采集端').click()");
      await c.wait("!!window.__acceptanceAuth.auth");
      session.auth = await c.evaluate("window.__acceptanceAuth.auth");
      return session.auth;
    } finally {
      try { await c.evaluate("if(window.__acceptanceAuth){window.fetch=window.__acceptanceAuth.native;delete window.__acceptanceAuth}"); } finally { c.close(); }
    }
  }
  async api(session, path, body, method) {
    const allowed = /^\/api\/v1\/(archive\/summary|enterprise\/(config|collectors)|settings\/super-admin|imports\/json|exports\/(start|[a-f0-9-]{36}(\/cancel)?))$/;
    if (!allowed.test(path)) throw fail("UNSAFE_TEST_API");
    const response = await localFetch(session.origin, path, {
      method: method ?? (body === undefined ? "GET" : "POST"),
      headers: { Authorization: await this.auth(session), "Content-Type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    return { status: response.status, value: await response.json() };
  }
  async upload(session, file, token) {
    if (!/^fixture-[0-9]\.(json|wca)$/.test(file)) throw fail("INVALID_FIXTURE_NAME");
    const response = await localFetch(session.origin, `/api/v1/imports/${file.endsWith(".wca") ? "enterprise" : "json"}`, {
      method: "POST", headers: { Authorization: token ?? await this.auth(session) }, body: await readFile(join(this.root, file)),
    });
    return { status: response.status, value: await response.json() };
  }
  async forceStop(session) {
    if (!session.stopped) session.child.kill();
    await deadline(session.exited, 10_000, "PROCESS_STOP_TIMEOUT");
  }
  async stop(session) {
    if (session.stopped) return;
    const c = await connect(session.debugPort, session.port);
    try {
      // Leaving conversations cancels its long poll before native shutdown.
      await c.evaluate("[...document.querySelectorAll('.primary-nav button')].find(b=>b.textContent==='设置').click()");
      await c.wait("!!document.querySelector('.settings-page')");
      void c.send("Runtime.evaluate", { expression: "void window.__TAURI_INTERNALS__.invoke('exit_workspace')", awaitPromise: false }).catch(() => {});
      await deadline(session.exited, 15_000, "NORMAL_EXIT_TIMEOUT");
    } finally { c.close(); }
  }
  async cleanup() {
    for (const session of this.sessions.values()) {
      if (!session.stopped) {
        try { await this.stop(session); }
        catch (error) {
          this.cleanupFailures.push({ instance: session.name, code: /^[A-Z_]{1,64}$/.test(error.code) ? error.code : "NORMAL_EXIT_FAILED" });
          try { await this.forceStop(session); }
          catch { this.cleanupFailures.push({ instance: session.name, code: "FORCE_STOP_FAILED" }); }
        }
      }
    }
    for (const port of this.ports) {
      try { await until(() => portIsClosed(port), 10_000); }
      catch { this.cleanupFailures.push({ code: "TEST_PORT_NOT_CLOSED" }); }
    }
    if (this.cleanupFailures.length) throw fail("ACCEPTANCE_CLEANUP_FAILED");
  }
}

export function portIsClosed(port) {
  return new Promise((done) => {
    const socket = createConnection({ host: "127.0.0.1", port });
    const finish = (closed) => { socket.destroy(); done(closed); };
    socket.setTimeout(500, () => finish(false));
    socket.once("connect", () => finish(false));
    socket.once("error", (error) => finish(error.code === "ECONNREFUSED"));
  });
}

export async function prepareMedia(root) {
  await validateRun(root);
  const { encode } = await import("silk-wasm");
  const pcm = new Int16Array(24_000 * 3);
  for (let i = 0; i < pcm.length; i++) pcm[i] = Math.round(Math.sin(i * 2 * Math.PI * 440 / 24_000) * 8000);
  for (const [name, bytes] of [
    ["fixture.png", Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a0ioAAAAASUVORK5CYII=", "base64")],
    ["fixture.txt", Buffer.from("Synthetic attachment.\n")],
    ["fixture.silk", (await encode(pcm, 24_000)).data],
  ]) await writeFile(join(root, name), bytes, { flag: "wx" });
}

export async function runChild(executable, args, timeout = 180_000) {
  const child = spawn(executable, args, { cwd: repoRoot, windowsHide: true, stdio: "ignore" });
  const exited = new Promise((done) => {
    child.once("error", () => done({ started: false }));
    child.once("exit", (code) => done({ started: true, code }));
  });
  try {
    const result = await deadline(exited, timeout, "TEST_CHILD_TIMEOUT");
    if (!result.started) throw fail("TEST_CHILD_START_FAILED");
    if (result.code !== 0) throw fail("TEST_CHILD_FAILED");
  } finally {
    if (child.exitCode === null) child.kill();
    await deadline(exited, 10_000, "TEST_CHILD_STOP_TIMEOUT");
  }
}
