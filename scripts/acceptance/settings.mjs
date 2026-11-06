import { spawn } from "node:child_process";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { connect, deadline, digest, fail, repoRoot } from "./runtime.mjs";

async function lockConfig(root) {
  const child = spawn("powershell.exe", ["-NoProfile", "-NonInteractive", "-File", join(repoRoot, "scripts/acceptance/hold-config.ps1"), "-RunRoot", root], {
    cwd: repoRoot, windowsHide: true, stdio: ["pipe", "pipe", "ignore"],
  });
  const exited = new Promise((done) => { child.once("exit", done); child.once("error", done); });
  child.stdin.on("error", () => {});
  try {
    await deadline(new Promise((done, reject) => {
      let output = "";
      child.stdout.on("data", (chunk) => { output += chunk; if (output.includes("ready")) done(); });
      child.once("error", () => reject(fail("CONFIG_LOCK_FAILED")));
      child.once("exit", () => reject(fail("CONFIG_LOCK_FAILED")));
    }), 10_000, "CONFIG_LOCK_TIMEOUT");
  } catch (error) { child.kill(); await exited; throw error; }
  return async () => {
    child.stdin.end("release\n");
    try { await deadline(exited, 10_000, "CONFIG_UNLOCK_TIMEOUT"); }
    finally { if (child.exitCode === null) child.kill(); }
  };
}

export async function settingsCases({ root, desktop, main, check }) {
  const version = JSON.parse(await readFile(join(repoRoot, "package.json"), "utf8")).version;
  const c = await connect(main.debugPort, main.port);
  try {
    await c.evaluate("[...document.querySelectorAll('.primary-nav button')].find(b => b.textContent === '设置').click()");
    await c.wait(`[...document.querySelectorAll('.settings-page .setting-fact')].some(row => row.querySelector('span')?.textContent === '版本' && row.querySelector('strong')?.textContent === ${JSON.stringify(version)})`);
    check("settings-ui-version");
  } finally { c.close(); }
  const request = (...args) => desktop.api(main, ...args);
  const configFile = join(main.dataRoot, "enterprise-config.json");
  const readConfig = async () => JSON.parse(await readFile(configFile, "utf8"));
  const before = (await request("/api/v1/enterprise/config")).value;
  const diskBefore = await readFile(configFile);
  const unlock = await lockConfig(root);
  try {
    const denied = await request("/api/v1/settings/super-admin", { enabled: false }, "PUT");
    const after = (await request("/api/v1/enterprise/config")).value;
    const disk = await readConfig();
    const unchanged = digest(diskBefore) === digest(await readFile(configFile));
    check("failed-setting-save",
      denied.status === 500 && before.superAdminEnabled === true && after.superAdminEnabled === true && disk.superAdminEnabled === true && unchanged);
    const enterprise = await request("/api/v1/enterprise/config", {
      organizationName: "Synthetic rejected setting", uploadUrl: main.origin,
      serverLogLevel: before.serverLogLevel === "off" ? "normal" : "off", superAdminEnabled: false,
    }, "PUT");
    const again = (await request("/api/v1/enterprise/config")).value;
    const preserved = again.organizationName === before.organizationName && again.serverLogLevel === before.serverLogLevel &&
      again.superAdminEnabled === true && digest(diskBefore) === digest(await readFile(configFile));
    check("failed-enterprise-save", enterprise.status === 500 && preserved);
  } finally { await unlock(); }
  const retry = await request("/api/v1/settings/super-admin", { enabled: false }, "PUT");
  const state = (await request("/api/v1/enterprise/config")).value, disk = await readConfig();
  check("unlocked-setting-retry",
    retry.status === 200 && state.superAdminEnabled === false && disk.superAdminEnabled === false);
  const enabled = await request("/api/v1/settings/super-admin", { enabled: true }, "PUT");
  const updated = await request("/api/v1/enterprise/config", {
    organizationName: "Synthetic committed settings", uploadUrl: main.origin, dataRedaction: true,
    serverLogLevel: "off", collectorLogLevel: "off", superAdminEnabled: true,
  }, "PUT");
  check("successful-enterprise-save",
    enabled.status === 200 && updated.status === 200 && (await readConfig()).organizationName === "Synthetic committed settings");
  const auth = await desktop.auth(main);
  await desktop.stop(main);
  main = await desktop.start("main");
  main.auth = auth;
  const restarted = await desktop.api(main, "/api/v1/enterprise/config");
  const restartedDisk = JSON.parse(await readFile(join(main.dataRoot, "enterprise-config.json"), "utf8"));
  check("settings-survive-restart", restarted.status === 200 &&
    restarted.value.organizationName === "Synthetic committed settings" && restartedDisk.organizationName === restarted.value.organizationName && restarted.value.superAdminEnabled === true);
  return main;
}
