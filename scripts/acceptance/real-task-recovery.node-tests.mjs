import assert from "node:assert/strict";
import { mkdir, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { test } from "node:test";
import { realTaskRecoveryCases } from "./real-task-recovery.mjs";
import { createRun, validateRun } from "./runtime.mjs";

async function ownedFixture(action) {
  const root = await createRun();
  try {
    await mkdir(join(root, "app"));
    await writeFile(join(root, "app/WeComArchive.exe"), "Synthetic executable placeholder", { flag: "wx" });
    await mkdir(join(root, "real-data"));
    const session = { name: "real", port: 12345, debugPort: 12346, origin: "http://127.0.0.1:12345", dataRoot: join(root, "real-data"), stopped: false };
    let touched = false;
    const desktop = {
      root, executable: join(root, "app/WeComArchive.exe"), sessions: new Map([[session.name, session]]),
      auth: async () => { touched = true; throw Error("AUTH_MUST_NOT_BE_READ"); },
    };
    await action({ root, desktop, session, request: async () => { touched = true; throw Error("REQUEST_MUST_NOT_RUN"); }, check: () => { touched = true; throw Error("CHECK_MUST_NOT_RUN"); } });
    assert.equal(touched, false);
  } finally {
    await validateRun(root);
    await rm(root, { recursive: true });
  }
}

test("real task recovery requires this run's explicit authorization before accessing the archive", async () => {
  await ownedFixture(async args => {
    await assert.rejects(realTaskRecoveryCases(args), { code: "UNAUTHORIZED_REAL_RECOVERY_RUN" });
    await writeFile(join(args.root, ".authorized-real-run"), "unrelated-authorization", { flag: "wx" });
    await assert.rejects(realTaskRecoveryCases(args), { code: "UNAUTHORIZED_REAL_RECOVERY_RUN" });
  });
});

test("real task recovery refuses external origins, loopback aliases and URL suffixes before authentication", async () => {
  await ownedFixture(async args => {
    await writeFile(join(args.root, ".authorized-real-run"), "real-optimizations.v1", { flag: "wx" });
    for (const origin of ["https://example.invalid", "http://localhost:12345", "http://127.0.0.1:12345/path", "http://127.0.0.1:12345?next=external", "http://private@127.0.0.1:12345"]) {
      args.session.origin = origin;
      await assert.rejects(realTaskRecoveryCases(args), { code: "NONLOCAL_REAL_RECOVERY_SESSION" });
    }
  });
});

test("real task recovery refuses a session not owned by the active desktop run", async () => {
  await ownedFixture(async args => {
    await writeFile(join(args.root, ".authorized-real-run"), "real-optimizations.v1", { flag: "wx" });
    args.desktop.sessions.set(args.session.name, { ...args.session });
    await assert.rejects(realTaskRecoveryCases(args), { code: "UNOWNED_REAL_RECOVERY_SESSION" });
  });
});

test("real task recovery refuses outside data and executable paths before authentication", async () => {
  await ownedFixture(async args => {
    await writeFile(join(args.root, ".authorized-real-run"), "real-optimizations.v1", { flag: "wx" });
    args.session.dataRoot = join(args.root, "../other-data");
    await assert.rejects(realTaskRecoveryCases(args), { code: "UNSAFE_REAL_RECOVERY_PATH" });
    args.session.dataRoot = join(args.root, "real-data");
    args.desktop.executable = join(args.root, "../other-app/WeComArchive.exe");
    await assert.rejects(realTaskRecoveryCases(args), { code: "UNSAFE_REAL_RECOVERY_PATH" });
  });
});
