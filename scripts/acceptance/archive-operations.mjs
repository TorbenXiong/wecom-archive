import { randomUUID } from "node:crypto";
import { readFile, writeFile, rename } from "node:fs/promises";
import { join } from "node:path";
import { connect, digest, fail, localFetch, pause, until, validateRun } from "./runtime.mjs";

async function layoutCases(c, page, root, check) {
  try {
    for (const [width, height] of [[1496, 939], [1024, 768], [760, 700]]) {
      await c.send("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: false });
      await pause(150);
      const fits = await c.evaluate(`(() => {
        const main = document.querySelector('main[aria-label="${page === "health" ? "归档健康" : "任务中心"}"]');
        const box = main.getBoundingClientRect();
        const controls = [...main.querySelectorAll('.operations-page-heading button, .task-row-main button')];
        return main.clientWidth > 300 && main.scrollWidth <= main.clientWidth + 1 && controls.every(button => {
          const r = button.getBoundingClientRect(); return r.width >= 28 && r.left >= box.left && r.right <= box.right;
        });
      })()`);
      check(page + "-compact-layout-" + width, fits);
      if (process.env.WECOM_ARCHIVE_ACCEPTANCE_VISUALS === "1") {
        const shot = await c.send("Page.captureScreenshot", { format: "png" });
        await writeFile(join(root, "ui-" + page + "-" + width + ".png"), Buffer.from(shot.data, "base64"), { flag: "wx" });
      }
    }
  } finally { await c.send("Emulation.clearDeviceMetricsOverride"); }
}

// Uses only this run's generated media and task receipts. Never collects sources.
export async function operationsCases({ root, desktop, main, check }) {
  await validateRun(root);
  const auth = await desktop.auth(main);
  const request = async (path, body) => {
    const response = await localFetch(main.origin, path, {
      method: body === undefined ? "GET" : "POST",
      headers: { Authorization: auth, "Content-Type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    if (!response.ok) throw fail("OPERATIONS_API_FAILED");
    return response.json();
  };
  const verify = async () => {
    const started = await request("/api/v1/archive/health-check", {});
    return until(async () => {
      const health = await request("/api/v1/archive/health");
      const record = health.latestVerification;
      if (record?.taskId !== started.taskId) return false;
      if (record.status === "failed") throw fail("OPERATIONS_VERIFICATION_FAILED");
      return record.verification || false;
    });
  };
  let c = await connect(main.debugPort, main.port);
  try {
    const before = await request("/api/v1/archive/health");
    check("health-summary-counts", before.messageCount === 6209 && before.conversationCount === 125 && before.verifiedMedia > 0);
    const denied = await localFetch(main.origin, "/api/v1/task-history");
    await denied.arrayBuffer();
    check("task-history-authentication", denied.status === 401);
    await c.evaluate("[...document.querySelectorAll('.primary-nav button')].find(b => b.textContent === '归档健康').click()");
    await c.wait("!!document.querySelector('main[aria-label=\"归档健康\"] .health-metric')");
    const layout = await c.evaluate("(() => { const m=document.querySelector('main[aria-label=\"归档健康\"]'); return m.innerText.includes('媒体概况') && m.innerText.includes('磁盘可用') && !m.innerText.includes('仅检查已归档内容') && !m.querySelector('details[open]'); })()");
    check("health-native-ui", layout);
    await c.evaluate("[...document.querySelectorAll('button')].find(b => b.textContent === '开始完整校验').click()");
    await c.wait("document.querySelector('.health-check-title')?.textContent === '最近校验通过'", 45_000);
    const healthy = (await request("/api/v1/archive/health")).latestVerification;
    check("health-native-verification", healthy.status === "completed" && healthy.verification.databaseOk && healthy.verification.checkedMedia === before.verifiedMedia && healthy.verification.missingFiles === 0 && healthy.verification.damagedFiles === 0 && healthy.verification.unreadableFiles === 0);
    await layoutCases(c, "health", root, check);
    await c.evaluate("document.querySelector('.health-check-details summary').click()");
    const disclosed = await c.evaluate("document.querySelector('.health-check-details').open && document.querySelector('.health-check-details').innerText.includes('归档文件缺失')");
    check("health-verification-details-on-demand", disclosed);
    await c.evaluate("document.querySelector('.health-check-details summary').click()");

    const fixture = await readFile(join(root, "fixture.txt"));
    const media = join(main.dataRoot, "media", digest(fixture));
    const original = await readFile(media);
    try {
      await writeFile(media, Buffer.alloc(original.length, 0x58));
      const damaged = await verify();
      check("health-detects-corrupt-archive-media", damaged.databaseOk && damaged.damagedFiles === 1 && damaged.missingFiles === 0);
    } finally { await writeFile(media, original); }
    const held = join(main.dataRoot, "media", `synthetic-held-${randomUUID()}`);
    await rename(media, held);
    try {
      const missing = await verify();
      check("health-detects-lost-archive-media", missing.databaseOk && missing.missingFiles === 1 && missing.damagedFiles === 0);
    } finally { await rename(held, media); }
    const clean = await verify();
    check("health-original-media-preserved", clean.missingFiles === 0 && clean.damagedFiles === 0 && digest(await readFile(media)) === digest(original));

    const started = await desktop.api(main, "/api/v1/exports/start", { scope: "entire_archive", format: "json", simplify: true });
    if (started.status !== 200) throw fail("OPERATIONS_EXPORT_START_FAILED");
    const exported = await until(async () => {
      const result = await desktop.api(main, `/api/v1/exports/${started.value.taskId}`);
      if (result.value.status === "failed") throw fail("OPERATIONS_EXPORT_FAILED");
      return result.value.status === "completed" ? result.value.result : false;
    });
    const first = await request("/api/v1/task-history?limit=1&offset=0");
    const second = await request("/api/v1/task-history?limit=1&offset=1");
    check("task-history-pagination", first.total >= 2 && first.records.length === 1 && second.records.length === 1 && first.records[0].taskId !== second.records[0].taskId && !first.persistenceError);
    const record = JSON.parse(await readFile(join(main.dataRoot, "task-history", `${started.value.taskId}.json`), "utf8"));
    check("task-history-durable-output", record.status === "completed" && record.output.sha256 === exported.manifestSha256 && !JSON.stringify(record).includes('"directory"'));

    c.close(); c = null;
    await desktop.stop(main);
    // Seed an interrupted checkpoint only after shutdown, in this owned fixture.
    const pendingId = randomUUID();
    await writeFile(join(main.dataRoot, "task-history", `${pendingId}.json`), JSON.stringify({
      ...record, taskId: pendingId, status: "running", phase: "处理数据", output: null,
      startedAt: new Date().toISOString(), updatedAt: new Date().toISOString(),
    }), { flag: "wx" });
    main = await desktop.start("main");
    main.auth = auth;
    const recovered = await desktop.api(main, `/api/v1/exports/${started.value.taskId}`);
    check("task-history-completed-survives-native-restart", recovered.value.status === "completed" && recovered.value.result.manifestSha256 === exported.manifestSha256);
    const pending = await desktop.api(main, `/api/v1/exports/${pendingId}`);
    check("task-history-interrupted-native-restart", pending.value.status === "unknown" && !pending.value.result);
    const repeated = await localFetch(main.origin, "/api/v1/exports/start", {
      method: "POST", headers: { Authorization: auth, "Content-Type": "application/json", "X-Archive-Task-Id": pendingId },
      body: JSON.stringify({ scope: "entire_archive", format: "json" }),
    });
    const rejection = await repeated.json();
    check("task-history-restart-rejects-resubmission", repeated.status === 409 && rejection.code === "TASK_ALREADY_EXISTS");
    c = await connect(main.debugPort, main.port);
    await c.evaluate("[...document.querySelectorAll('.primary-nav button')].find(b => b.textContent === '任务中心').click()");
    await c.wait("!!document.querySelector('.task-history-list .task-row')");
    const concise = await c.evaluate("!document.querySelector('.task-identifier') && !document.querySelector('.task-row-details')");
    check("task-center-collapsed-by-default", concise);
    await layoutCases(c, "tasks", root, check);
    await c.evaluate("(() => { const e=[...document.querySelectorAll('.operations-filters label')].find(l=>l.textContent.startsWith('任务状态')).querySelector('select'); Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype,'value').set.call(e,'unknown'); e.dispatchEvent(new Event('change',{bubbles:true})); })()");
    await c.wait(`!!document.querySelector('[data-task-id="${pendingId}"]')`);
    const filtered = await c.evaluate("[...document.querySelectorAll('.task-history-list .task-state')].every(e => e.textContent === '结果待核对') && ![...document.querySelectorAll('.task-history-list button')].some(b => /重试|重新执行/.test(b.textContent))");
    check("task-center-native-unknown-filter", filtered);
    await c.evaluate(`document.querySelector('[data-task-id="${pendingId}"] .task-expand-button').click()`);
    await c.wait(`document.querySelector('[data-task-id="${pendingId}"] .task-identifier')?.textContent === ${JSON.stringify(pendingId)}`);
    const visibleDetails = await c.evaluate(`document.querySelector('[data-task-id="${pendingId}"] .task-row-details').innerText.includes('不会自动重跑')`);
    check("task-center-details-on-demand", visibleDetails);
    // Reuse this isolated instance's startup authentication on reload. The app
    // deliberately removes the credential fragment after its first connection.
    await c.evaluate(`(() => { history.replaceState(null, '', location.pathname + '#token=' + encodeURIComponent(${JSON.stringify(auth.slice("Bearer ".length))})); window.__operationsReload = true; })()`);
    await c.send("Page.reload");
    await until(async () => {
      try { return await c.evaluate("!window.__operationsReload && !!document.querySelector('.app-shell')"); }
      catch { return false; } // The old execution context is destroyed by reload.
    });
    await c.evaluate("[...document.querySelectorAll('.primary-nav button')].find(b => b.textContent === '任务中心').click()");
    await c.wait(`!!document.querySelector('[data-task-id="${pendingId}"]')`);
    check("task-center-native-refresh-preserves-history");
    if (process.env.WECOM_ARCHIVE_ACCEPTANCE_VISUALS === "1") {
      for (const [label, selector, file] of [["本机", ".local-export-card", "local-export"], ["采集端", ".collector-management-panel", "collectors"], ["设置", ".general-settings-grid", "settings"]]) {
        await c.evaluate(`[...document.querySelectorAll('.primary-nav button')].find(b => b.textContent === ${JSON.stringify(label)}).click()`);
        await c.wait(`!!document.querySelector(${JSON.stringify(selector)})`);
        const shot = await c.send("Page.captureScreenshot", { format: "png" });
        await writeFile(join(root, "ui-" + file + "-1496.png"), Buffer.from(shot.data, "base64"), { flag: "wx" });
      }
    }
    return main;
  } finally { c?.close(); }
}
