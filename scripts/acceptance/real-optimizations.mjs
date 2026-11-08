// Explicitly authorized local acceptance. Never print account data or retain its test archive.
import { copyFile, mkdir, readdir, realpath, rm, stat, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { connect, createRun, DesktopRun, fail, localJsonRequest, pause, repoRoot, until, validateRun, within } from "./runtime.mjs";
import { realTaskRecoveryCases } from "./real-task-recovery.mjs";

if (process.argv.length !== 3 || process.argv[2] !== "--authorized") {
  console.error("EXPLICIT_REAL_ENVIRONMENT_AUTHORIZATION_REQUIRED");
  process.exit(1);
}
if (process.platform !== "win32") {
  console.error("WINDOWS_REAL_ENVIRONMENT_REQUIRED");
  process.exit(1);
}

const root = await createRun();
await writeFile(join(root, ".authorized-real-run"), "real-optimizations.v1", { flag: "wx" });
const executable = join(root, "app/WeComArchive.exe");
const desktop = new DesktopRun(root, executable);
let page, session, auth, planId, stage = "prepare", passed = 0;
const previousCacheMode = process.env.WECOM_ARCHIVE_SNAPSHOT_CACHE;
const previousProbe = process.env.WECOM_ARCHIVE_PROBE_EXE;
const check = (id, condition = true) => {
  if (!condition) throw fail("REAL_OPTIMIZATION_ASSERTION_FAILED");
  console.log(`PASS ${id}`); passed++;
};
const request = (path, body, method) => localJsonRequest(session.origin, path, {
    method: method ?? (body === undefined ? "GET" : "POST"),
    headers: { Authorization: auth, "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
    timeout: 15 * 60_000,
  });
const messagePath = (query) => `/api/v1/messages?${new URLSearchParams(query)}`;
const ids = (rows) => JSON.stringify(rows.map(row => row.stable_message_id));
async function cacheFiles(directory) {
  const files = new Map();
  async function visit(folder) {
    for (const entry of await readdir(folder, { withFileTypes: true })) {
      if (entry.isSymbolicLink()) throw fail("UNEXPECTED_REAL_CACHE_LINK");
      const path = join(folder, entry.name);
      if (entry.isDirectory()) await visit(path);
      else if (entry.isFile() && folder.endsWith("\\files")) files.set(path, (await stat(path, { bigint: true })).ino);
    }
  }
  await visit(directory);
  return files;
}

try {
  process.env.WECOM_ARCHIVE_SNAPSHOT_CACHE = "1";
  process.env.WECOM_ARCHIVE_PROBE_EXE = executable;
  await mkdir(join(root, "app"));
  await copyFile(join(repoRoot, "target/release/WeComArchive.exe"), executable);
  session = await desktop.start("real");
  auth = await desktop.auth(session);
  await request("/api/v1/enterprise/config", {
    organizationName: "Authorized local acceptance", uploadUrl: session.origin,
    superAdminEnabled: true, dataRedaction: true, serverLogLevel: "off", collectorLogLevel: "off",
    collectorSchedule: { mode: "disabled", intervalMinutes: 60, dailyTime: "02:00" },
  }, "PUT");
  stage = "full-media-collection";
  console.log("Running real full-media collection (isolated, redacted)");
  await request("/api/v1/collections/local?include_media=true&data_redaction=true", {});
  const initial = await request("/api/v1/archive/summary");
  check("real-full-media-collection", initial.message_count > 0 && initial.media_count > 0);
  console.log(`Real archive counters: messages=${initial.message_count}, media=${initial.media_count}`);

  stage = "scheduled-cache-cycles";
  const schedules = await request("/api/v1/collection-schedules", {
    name: "Authorized cache regression", includeMedia: false,
    schedule: { mode: "interval", intervalMinutes: 1, dailyTime: "02:00" },
  });
  planId = schedules.localPlans[0].id;
  let lastRun, seeded;
  for (let cycle = 1; cycle <= 3; cycle++) {
    console.log(`Waiting for real scheduled cycle ${cycle}/3`);
    const plan = await until(async () => {
      const next = (await request("/api/v1/collection-schedules")).localPlans.find(item => item.id === planId);
      if (next?.lastStatus === "error") throw fail("REAL_SCHEDULED_COLLECTION_FAILED");
      return next?.lastRunAt && next.lastRunAt !== lastRun && next;
    }, 15 * 60_000);
    lastRun = plan.lastRunAt;
    check(`real-scheduled-cycle-${cycle}`, plan.lastStatus === "success");
    if (cycle === 2) seeded = await cacheFiles(join(session.dataRoot, "snapshot-cache"));
    if (cycle === 3) {
      const reused = await cacheFiles(join(session.dataRoot, "snapshot-cache"));
      check("real-cross-cycle-cache-reused", seeded.size > 0 && [...seeded].some(([path, ino]) => ino !== 0n && reused.get(path) === ino));
    }
  }
  await request(`/api/v1/collection-schedules/${planId}`, undefined, "DELETE");
  planId = undefined;
  await until(async () => !(await request("/api/v1/collections/local/progress")).running);
  const summary = await request("/api/v1/archive/summary");
  check("real-text-cycles-retain-media", summary.media_count >= initial.media_count);

  stage = "deep-pages";
  const conversations = await request("/api/v1/conversations?limit=200&offset=0");
  const conversation = conversations.sort((a, b) => b.message_count - a.message_count)[0];
  if (!conversation || conversation.message_count < 300) throw fail("REAL_DEEP_PAGE_DATA_INSUFFICIENT");
  for (const sort of ["asc", "desc"]) {
    const offset = Math.max(100, Math.min(Math.floor(conversation.message_count * 0.8 / 100), Math.floor((conversation.message_count - 100) / 100)) * 100);
    const query = { conversation_id: conversation.conversation_id, limit: "100", offset: String(offset), sort };
    const expected = await request(messagePath(query));
    const previous = await request(messagePath({ ...query, limit: "1", offset: String(offset - 1) }));
    const anchored = await request(messagePath({ ...query, offset: "0", anchor_message_id: previous[0].stable_message_id, anchor_shift: "1", anchor_revision: String(summary.revision) }));
    check(`real-deep-anchor-${sort}`, expected.length === 100 && ids(expected) === ids(anchored));
    const target = expected[37];
    const position = await request(`/api/v1/messages/${encodeURIComponent(target.stable_message_id)}/position?sort=${sort}`);
    const located = await request(messagePath({ ...query, offset: "0", anchor_message_id: target.stable_message_id, anchor_shift: "-37", anchor_revision: String(summary.revision) }));
    check(`real-located-page-${sort}`, position.offset_in_conversation === offset + 37 && ids(located) === ids(expected));
    const fallback = await request(messagePath({ ...query, anchor_message_id: target.stable_message_id, anchor_shift: "0", anchor_revision: String(summary.revision + 1) }));
    check(`real-stale-anchor-fallback-${sort}`, ids(fallback) === ids(expected));
  }

  stage = "real-image-selection";
  const images = [];
  let selected;
  for (let offset = 0; offset < 500 && !selected; offset += 50) {
    const batch = await request(`/api/v1/search/messages?q=&message_type=image&media_only=true&limit=50&offset=${offset}&sort=desc&locate=false`);
    images.push(...batch);
    for (let index = offset; index < images.length; index++) {
      const image = images[index];
      const media = image.media[0];
      if (image.lifecycle === "recalled" || media?.integrity !== "verified" || !media.content_hash) continue;
      const position = await request(`/api/v1/messages/${encodeURIComponent(image.stable_message_id)}/position?sort=desc`);
      if (position.offset_in_conversation % 500 < 30) continue;
      const pageIndex = Math.floor(position.offset_in_conversation / 500);
      const rows = await request(messagePath({ conversation_id: image.conversation_id, limit: "500", offset: String(pageIndex * 500), sort: "desc" }));
      const count = await request(`/api/v1/messages/count?${new URLSearchParams({ conversation_id: image.conversation_id })}`);
      if (count.total <= 500 || rows.filter(row => row.media.some(item => item.content_hash === media.content_hash)).length !== 1) continue;
      selected = { image, media, index, pageIndex }; break;
    }
    if (batch.length < 50) break;
  }
  if (!selected) throw fail("REAL_OFFSCREEN_IMAGE_DATA_INSUFFICIENT");
  page = await connect(session.debugPort, session.port);
  await page.evaluate(`(() => {
    const s = window.__realAcceptance = { nativeFetch: window.fetch.bind(window), media: [] };
    window.fetch = (input, init) => { const u = new URL(typeof input === 'string' ? input : input.url, location.href); if (u.pathname.startsWith('/api/v1/media/')) s.media.push(u.pathname); return s.nativeFetch(input, init); };
    s.set = (e, v) => { Object.getOwnPropertyDescriptor(e instanceof HTMLSelectElement ? HTMLSelectElement.prototype : HTMLInputElement.prototype, 'value').set.call(e, v); e.dispatchEvent(new Event(e instanceof HTMLSelectElement ? 'change' : 'input', {bubbles:true})); };
    [...document.querySelectorAll('.primary-nav button')].find(b => b.textContent === '会话').click();
  })()`);
  await page.wait("!!document.querySelector('[aria-label=\"每页数量\"]')");
  await page.evaluate("window.__realAcceptance.set(document.querySelector('[aria-label=\"每页数量\"]'),'500'); window.dispatchEvent(new KeyboardEvent('keydown',{key:'f',ctrlKey:true,altKey:true}))");
  await page.wait("!!document.querySelector('.global-search-overlay')");
  await page.evaluate("window.__realAcceptance.set(document.querySelector('[aria-label=\"筛选消息类型\"]'),'image'); document.querySelector('.global-search-filter-actions input[type=checkbox]').click()");
  await page.wait(`document.querySelectorAll('.global-search-item').length === ${Math.min(images.length, 50)}`);
  for (let count = 50; count <= selected.index; count += 50) {
    await page.evaluate("document.querySelector('.global-search-load-more').click()");
    await page.wait(`document.querySelectorAll('.global-search-item').length === ${Math.min(images.length, count + 50)}`);
  }
  await page.evaluate(`document.querySelectorAll('.global-search-item')[${selected.index}].click()`);
  const selector = `[data-message-id=${JSON.stringify(selected.image.stable_message_id)}]`;
  const row = `document.querySelector(${JSON.stringify(selector)})`;
  await page.wait(`!document.querySelector('.global-search-overlay') && !!${row}?.classList.contains('highlighted')`);
  await page.wait(`!!${row}?.querySelector('.image-preview img')?.naturalWidth`);
  stage = "real-image-preview";
  const loads = () => page.evaluate(`window.__realAcceptance.media.filter(path => path === ${JSON.stringify(`/api/v1/media/${selected.media.content_hash}`)}).length`);
  const beforePreview = await loads();
  await page.evaluate(`${row}.querySelector('.image-preview').click()`);
  await page.wait("!!document.querySelector('.media-preview-dialog img')?.naturalWidth");
  check("real-image-preview-reuses-load", await loads() === beforePreview);
  await page.evaluate("document.querySelector('[aria-label=\"关闭图片预览\"]').click()");
  const jump = async (index) => {
    await page.evaluate(`(() => { const e=document.querySelector('[aria-label="跳转页码"]'); window.__realAcceptance.set(e,${JSON.stringify(String(index + 1))}); e.dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',bubbles:true})); })()`);
  };
  await jump(selected.pageIndex === 0 ? 1 : 0);
  await page.wait(`!${row}`);
  await pause(200);
  const beforeReload = await loads();
  stage = "real-image-offscreen";
  await jump(selected.pageIndex);
  await page.wait(`!!${row}?.querySelector('.image-preview')`);
  await page.wait(`${row}.getBoundingClientRect().top > document.querySelector('.message-scroll').getBoundingClientRect().bottom + 120`);
  await pause(200);
  check("real-image-offscreen-no-fetch", await loads() === beforeReload && await page.evaluate(`!${row}.querySelector('.image-preview img')`));
  await page.evaluate(`${row}.scrollIntoView({block:'center',behavior:'instant'})`);
  await page.wait(`!!${row}?.querySelector('.image-preview img')?.naturalWidth`);
  check("real-image-visible-load", await loads() === beforeReload + 1);
  stage = "real-task-recovery";
  await realTaskRecoveryCases({ root, desktop, session, request, check });
} catch (error) {
  const code = error?.code || error?.cause?.code;
  console.error(`FAIL real-${stage}`, /^[A-Z_0-9]{1,80}$/.test(code || "") ? code : "REAL_ACCEPTANCE_FAILED");
  process.exitCode = 1;
} finally {
  if (previousCacheMode === undefined) delete process.env.WECOM_ARCHIVE_SNAPSHOT_CACHE;
  else process.env.WECOM_ARCHIVE_SNAPSHOT_CACHE = previousCacheMode;
  if (previousProbe === undefined) delete process.env.WECOM_ARCHIVE_PROBE_EXE;
  else process.env.WECOM_ARCHIVE_PROBE_EXE = previousProbe;
  if (page) { try { await page.evaluate("window.fetch=window.__realAcceptance.nativeFetch; delete window.__realAcceptance"); } catch { /* Owned process cleanup follows. */ } page.close(); }
  if (planId) { try { await request(`/api/v1/collection-schedules/${planId}`, undefined, "DELETE"); } catch { /* Owned process cleanup follows. */ } }
  try { await desktop.cleanup(); check("real-process-and-port-cleanup"); }
  catch { console.error("FAIL real-process-and-port-cleanup"); process.exitCode = 1; }
  try {
    await validateRun(root);
    const resolved = await realpath(root), base = await realpath(join(repoRoot, "target/acceptance"));
    if (!within(base, resolved) || !within(repoRoot, resolved)) throw fail("UNSAFE_REAL_CLEANUP_PATH");
    await rm(resolved, { recursive: true, force: true, maxRetries: 20, retryDelay: 200 });
    check("real-private-test-data-cleanup");
  } catch (error) { console.error("FAIL real-private-test-data-cleanup", /^[A-Z_0-9]{1,80}$/.test(error?.code || "") ? error.code : "REAL_CLEANUP_FAILED"); process.exitCode = 1; }
  console.log(`Real optimization acceptance: ${passed} passed`);
}
