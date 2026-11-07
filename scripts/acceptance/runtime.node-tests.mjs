import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { mkdir, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { randomUUID } from "node:crypto";
import { test } from "node:test";
import { availablePort, deadline, DesktopRun, fail, localFetch, localJsonRequest, portIsClosed, repoRoot, runChild, within } from "./runtime.mjs";

test("owned directory checks reject parent traversal, sibling prefixes and the root itself", () => {
  const root = resolve("target/acceptance/synthetic");
  assert.equal(within(root, join(root, "data/file")), true);
  for (const outside of [root, join(root, ".."), `${root}-other/file`, join(root, "../elsewhere")]) assert.equal(within(root, outside), false);
});

test("native JSON requests preserve local-only boundaries and explicit deadlines", async () => {
  const server = createServer((request, response) => {
    if (request.url === "/redirect") { response.writeHead(302, { Location: "https://example.invalid" }); response.end(); }
    else setTimeout(() => { response.setHeader("Content-Type", "application/json"); response.end('{"synthetic":true}'); }, 60);
  });
  await new Promise(done => server.listen(0, "127.0.0.1", done));
  const origin = `http://127.0.0.1:${server.address().port}`;
  try {
    await assert.rejects(localJsonRequest(origin, "https://example.invalid"), { code: "NONLOCAL_TEST_REQUEST" });
    await assert.rejects(localJsonRequest(origin, "/redirect"), { code: "REAL_API_STATUS_302" });
    assert.deepEqual(await localJsonRequest(origin, "/delayed", { timeout: 2000 }), { synthetic: true });
    await assert.rejects(localJsonRequest(origin, "/delayed", { timeout: 10 }), { code: "ABORT_ERR" });
  } finally { server.closeAllConnections(); await new Promise(done => server.close(done)); }
});

test("test HTTP refuses other origins and redirects", async () => {
  await assert.rejects(localFetch("http://127.0.0.1:1", "https://example.invalid/"), { code: "NONLOCAL_TEST_REQUEST" });
  await assert.rejects(localFetch("http://127.0.0.1:1", "http://127.0.0.1:2/"), { code: "NONLOCAL_TEST_REQUEST" });
  const server = createServer((_request, response) => { response.writeHead(302, { Location: "https://example.invalid/" }); response.end(); });
  await new Promise((done) => server.listen(0, "127.0.0.1", done));
  try { await assert.rejects(localFetch(`http://127.0.0.1:${server.address().port}`, "/")); }
  finally { server.closeAllConnections(); await new Promise((done) => server.close(done)); }
});

test("deadline rejects a stalled operation and preserves successful results", async () => {
  await assert.rejects(deadline(new Promise(() => {}), 20, "SYNTHETIC_DEADLINE"), { code: "SYNTHETIC_DEADLINE" });
  assert.equal(await deadline(Promise.resolve(7), 5000, "UNUSED_DEADLINE"), 7);
});

test("port check distinguishes a listening TCP socket from a closed port", async () => {
  const server = createServer();
  await new Promise((done) => server.listen(0, "127.0.0.1", done));
  const port = server.address().port;
  assert.equal(await portIsClosed(port), false);
  await new Promise((done) => server.close(done));
  assert.equal(await portIsClosed(port), true);
  assert.equal(Number.isInteger(await availablePort()), true);
});

test("fallback process termination cannot turn failed graceful cleanup into a pass", async () => {
  const run = new DesktopRun("unused", "unused");
  const session = { name: "main", stopped: false };
  run.sessions.set("main", session);
  let forced = false;
  run.stop = async () => { throw fail("NORMAL_EXIT_TIMEOUT"); };
  run.forceStop = async () => { forced = true; session.stopped = true; };
  await assert.rejects(run.cleanup(), { code: "ACCEPTANCE_CLEANUP_FAILED" });
  assert.equal(forced, true);
  assert.equal(run.cleanupFailures[0].code, "NORMAL_EXIT_TIMEOUT");
});

test("synthetic upload cannot name an arbitrary source or start local collection", async () => {
  const run = new DesktopRun("unused", "unused");
  await assert.rejects(run.upload({}, "../../private.json"), { code: "INVALID_FIXTURE_NAME" });
  await assert.rejects(run.api({}, "/api/v1/collections/local", {}), { code: "UNSAFE_TEST_API" });
  await assert.rejects(run.start("../outside"), { code: "INVALID_INSTANCE_NAME" });
});

test("child timeout terminates its owned helper", async () => {
  await assert.rejects(runChild(process.execPath, ["-e", "setTimeout(() => {}, 60000)"], 100), { code: "TEST_CHILD_TIMEOUT" });
});

async function packageAttempt(name) {
  const child = spawn("powershell.exe", ["-NoProfile", "-NonInteractive", "-File", join(repoRoot, "scripts/package-release.ps1"), "-PackageName", name], {
    cwd: repoRoot, windowsHide: true, stdio: "ignore",
  });
  return deadline(new Promise((done, reject) => { child.once("error", reject); child.once("exit", (code) => done(code)); }), 10_000, "PREFLIGHT_TIMEOUT")
    .finally(() => { if (child.exitCode === null) child.kill(); });
}

test("release entry rejects traversal and reserved names before building", { skip: process.platform !== "win32" }, async () => {
  assert.notEqual(await packageAttempt("../outside"), 0);
  assert.notEqual(await packageAttempt("NUL.exe"), 0);
});

test("release entry refuses an existing package without changing it", { skip: process.platform !== "win32" }, async () => {
  const name = `synthetic-collision-${randomUUID()}`;
  const path = join(repoRoot, "artifacts", name);
  assert.equal(within(join(repoRoot, "artifacts"), path), true);
  await mkdir(path, { recursive: true });
  await writeFile(join(path, "synthetic-sentinel.txt"), "Synthetic existing package", { flag: "wx" });
  try {
    assert.notEqual(await packageAttempt(name), 0);
    assert.deepEqual(await readdir(path), ["synthetic-sentinel.txt"]);
    assert.equal(await readFile(join(path, "synthetic-sentinel.txt"), "utf8"), "Synthetic existing package");
  }
  finally { await rm(path, { recursive: true }); }
});

test("real environment entry requires an explicit authorization switch", { skip: process.platform !== "win32" }, async () => {
  const child = spawn("powershell.exe", ["-NoProfile", "-NonInteractive", "-File", join(repoRoot, "scripts/verify-real-environment.ps1")], {
    cwd: repoRoot, windowsHide: true, stdio: "ignore",
  });
  try {
    const code = await deadline(new Promise((done, reject) => { child.once("error", reject); child.once("exit", done); }), 10_000, "PREFLIGHT_TIMEOUT");
    assert.notEqual(code, 0);
  } finally { if (child.exitCode === null) child.kill(); }
});

test("real optimization acceptance requires explicit authorization before creating runtime data", async () => {
  const child = spawn(process.execPath, [join(repoRoot, "scripts/acceptance/real-optimizations.mjs")], {
    cwd: repoRoot, windowsHide: true, stdio: ["ignore", "ignore", "pipe"],
  });
  let error = "";
  child.stderr.setEncoding("utf8").on("data", chunk => { error += chunk; });
  try {
    const code = await deadline(new Promise((done, reject) => { child.once("error", reject); child.once("exit", done); }), 10_000, "PREFLIGHT_TIMEOUT");
    assert.notEqual(code, 0);
    assert.equal(error.trim(), "EXPLICIT_REAL_ENVIRONMENT_AUTHORIZATION_REQUIRED");
  } finally { if (child.exitCode === null) child.kill(); }
});
