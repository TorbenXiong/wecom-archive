import { cp, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { digest, fail, fileHashes, validateRun } from "./runtime.mjs";

export async function recoveryCases({ root, desktop, main, check }) {
  await validateRun(root);
  const auth = await desktop.auth(main);
  const before = (await desktop.api(main, "/api/v1/archive/summary")).value;
  const configBytes = await readFile(join(main.dataRoot, "enterprise-config.json"));
  const config = JSON.parse(configBytes);
  await desktop.forceStop(main);
  const backupRoot = join(root, "cold-backup");
  const originalHashes = await fileHashes(main.dataRoot);
  await cp(main.dataRoot, backupRoot, { recursive: true, errorOnExist: true, force: false });
  const backupHashes = await fileHashes(backupRoot);
  const sameBackup = JSON.stringify(originalHashes) === JSON.stringify(backupHashes);
  check("cold-backup-integrity", sameBackup);
  main = await desktop.start("main");
  main.auth = auth;
  const response = await desktop.api(main, "/api/v1/archive/summary");
  const after = response.value;
  const configUnchanged = digest(await readFile(join(main.dataRoot, "enterprise-config.json"))) === digest(configBytes);
  check("forced-exit-restart",
    response.status === 200 && after.message_count === before.message_count && after.conversation_count === before.conversation_count && after.revision === before.revision && configUnchanged);
  const retry = await desktop.upload(main, "fixture-1.json");
  check("forced-exit-import-retry", retry.status === 200 && retry.value.inserted === 0 && retry.value.unchanged === 1);
  const restoredRoot = join(root, "restored-data");
  await cp(backupRoot, restoredRoot, { recursive: true, errorOnExist: true, force: false });
  const recovered = await readFile(join(restoredRoot, "enterprise-config.json.bak"));
  const recoveryConfig = JSON.parse(recovered);
  const sameIdentity = ["organization_id", "key_id", "private_key_protected_hex"].every((key) => recoveryConfig[key] === config[key]);
  if (!sameIdentity) throw fail("RECOVERY_IDENTITY_MISMATCH");
  // Exercise the documented manual restore on a copy, never the running source directory.
  await writeFile(join(restoredRoot, "enterprise-config.json"), "Truncated synthetic configuration");
  const damaged = digest(await readFile(join(restoredRoot, "enterprise-config.json"))) !== digest(recovered);
  await writeFile(join(restoredRoot, "enterprise-config.json"), recovered);
  const repaired = digest(await readFile(join(restoredRoot, "enterprise-config.json"))) === digest(recovered);
  check("configuration-backup-recovery", damaged && repaired && sameIdentity);
  const restored = await desktop.start("restored");
  restored.auth = auth;
  const restoredResponse = await desktop.api(restored, "/api/v1/archive/summary");
  const summary = restoredResponse.value;
  const mediaOriginal = await fileHashes(join(backupRoot, "media"));
  const mediaRestored = await fileHashes(join(restoredRoot, "media"));
  const sameMedia = JSON.stringify(mediaOriginal) === JSON.stringify(mediaRestored);
  check("same-user-complete-restore",
    restoredResponse.status === 200 && summary.message_count === before.message_count && summary.conversation_count === before.conversation_count &&
    summary.media_count === before.media_count && summary.revision === before.revision && sameMedia);
  await desktop.stop(restored);
  check("restored-instance-normal-exit");
}
