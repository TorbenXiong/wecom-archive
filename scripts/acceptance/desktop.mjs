import { copyFile, mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { createRun, DesktopRun, fail, localFetch, prepareMedia, repoRoot, runChild } from "./runtime.mjs";
import { archiveCases } from "./archive.mjs";
import { settingsCases } from "./settings.mjs";
import { timeoutCases } from "./import-timeouts.mjs";
import { recoveryCases } from "./recovery.mjs";

const root = await createRun();
const executable = join(root, "app/WeComArchive.exe");
const desktop = new DesktopRun(root, executable);
let passed = 0;
let stage = "prepare";
const check = (id, pass = true) => {
  console.log(`${pass ? "PASS" : "FAIL"} ${id}`);
  if (!pass) throw fail("ACCEPTANCE_ASSERTION_FAILED");
  passed++;
};

try {
  await mkdir(join(root, "app"));
  await copyFile(join(repoRoot, "target/release/WeComArchive.exe"), executable);
  await prepareMedia(root);
  let main = await desktop.start("main");
  const config = await desktop.api(main, "/api/v1/enterprise/config", {
    organizationName: "Synthetic acceptance", uploadUrl: main.origin,
    superAdminEnabled: true, dataRedaction: false, serverLogLevel: "off", collectorLogLevel: "off",
    collectorSchedule: { mode: "disabled", intervalMinutes: 60, dailyTime: "02:00" },
  }, "PUT");
  if (config.status !== 200) throw fail("SYNTHETIC_CONFIG_FAILED");
  const collector = await desktop.api(main, "/api/v1/enterprise/collectors", {});
  if (collector.status !== 200 || !collector.value.executableGenerated) throw fail("SYNTHETIC_ENROLLMENT_FAILED");
  const binding = JSON.parse(collector.value.artifact);
  // Only the public binding crosses the generator process boundary. Never run the collector.
  await writeFile(join(root, "enrollment.json"), JSON.stringify({
    organizationId: binding.organizationId, keyId: binding.keyId, publicKeyHex: binding.publicKeyHex,
  }), { flag: "wx" });
  await runChild(join(repoRoot, "target/release/examples/acceptance-fixtures.exe"), [root]);
  stage = "seed";
  for (let index = 0; index < 10; index++) {
    const result = await desktop.upload(main, `fixture-${index}.json`);
    if (result.status !== 200 || result.value.inserted !== (index === 0 ? 6200 : 1)) throw fail("SYNTHETIC_SEED_FAILED");
  }
  const encrypted = await desktop.upload(main, "fixture-1.wca");
  check("encrypted-package-idempotence",
    encrypted.status === 200 && encrypted.value.inserted === 0 && encrypted.value.unchanged === 1);
  const before = (await desktop.api(main, "/api/v1/archive/summary")).value;
  const damaged = await readFile(join(root, "fixture-1.wca"));
  damaged[damaged.length - 1] ^= 1;
  const rejected = await localFetch(main.origin, "/api/v1/imports/enterprise", {
    method: "POST", headers: { Authorization: await desktop.auth(main) }, body: damaged,
  });
  await rejected.arrayBuffer();
  const after = (await desktop.api(main, "/api/v1/archive/summary")).value;
  check("encrypted-package-tamper",
    rejected.status === 422 && before.revision === after.revision);
  const fresh = await desktop.start("fresh");
  stage = "archive";
  await archiveCases({ desktop, main, fresh, check });
  stage = "settings";
  main = await settingsCases({ root, desktop, main, check });
  stage = "import-timeouts";
  await timeoutCases({ root, desktop, main, fresh, check });
  stage = "recovery";
  await recoveryCases({ root, desktop, main, check });
} catch {
  console.error(`FAIL desktop-${stage}`);
  process.exitCode = 1;
} finally {
  try { await desktop.cleanup(); check("process-and-port-cleanup"); }
  catch { console.error("FAIL process-and-port-cleanup", desktop.cleanupFailures); process.exitCode = 1; }
  console.log(`Desktop acceptance: ${passed} passed`);
}
