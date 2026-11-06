import { readFile, readdir, stat } from "node:fs/promises";
import { basename, join } from "node:path";
import { connect, digest, fail, localFetch, pause, until } from "./runtime.mjs";

async function bootstrap(session) {
  const c = await connect(session.debugPort, session.port);
  try {
    await c.evaluate(`(() => {
      const s = window.__acceptance = { nativeFetch: window.fetch.bind(window), positions: [] };
      window.fetch = async (input, init = {}) => {
        const u = new URL(typeof input === 'string' ? input : input.url, location.href);
        const r = await s.nativeFetch(input, init);
        if (u.pathname.endsWith('/position')) s.positions.push({ sort: u.searchParams.get('sort'), status: r.status });
        if (u.pathname === '/api/v1/exports/start' && r.ok) s.lastExport = await r.clone().json();
        if (u.pathname === '/api/v1/imports/json') s.lastImport = { status: r.status, value: await r.clone().json() };
        return r;
      };
      s.set = (e, v) => {
        Object.getOwnPropertyDescriptor(e instanceof HTMLSelectElement ? HTMLSelectElement.prototype : HTMLInputElement.prototype, 'value').set.call(e, v);
        e.dispatchEvent(new Event(e instanceof HTMLSelectElement ? 'change' : 'input', { bubbles: true }));
      };
      s.click = t => {
        const b = [...document.querySelectorAll('button')].find(x => x.textContent.trim() === t);
        if (!b) throw Error('MISSING_BUTTON');
        b.click();
      };
      s.click('会话');
    })()`);
    await c.wait("!!document.querySelector('.conversation-search-box')");
    return c;
  } catch (error) { c.close(); throw error; }
}

async function importUi(c, file) {
  await c.evaluate("window.__acceptance.lastImport = null");
  const obj = await c.send("Runtime.evaluate", { expression: "document.querySelector('[aria-label=\"导入会话\"]')", returnByValue: false });
  await c.send("DOM.setFileInputFiles", { objectId: obj.result.objectId, files: [file] });
  await c.wait("!!window.__acceptance.lastImport && !document.querySelector('[aria-label=\"导入会话\"]').disabled");
  return c.evaluate("({status:window.__acceptance.lastImport.status,inserted:window.__acceptance.lastImport.value.inserted,unchanged:window.__acceptance.lastImport.value.unchanged,revised:window.__acceptance.lastImport.value.revised})");
}

async function searchCases(c, main, auth, check) {
  const request = (path) => localFetch(main.origin, path, { headers: { Authorization: auth } });
  const criteria = new URLSearchParams({ q: "项目", locate: "false", conversation_id: "R:acceptance-000", participant_id: "acceptance-a", message_type: "text", starts_at: "2026-01-02T00:00:00Z", ends_at: "2026-01-02T23:59:59.999Z" });
  const response = await request(`/api/v1/search/messages?${criteria}`), results = await response.json();
  check("search-combined-filters", response.ok && results.length === 50 && results.every((row) =>
    row.offset_in_conversation == null && row.conversation_id === "R:acceptance-000" && row.sender_id === "acceptance-a" && row.message_type === "text"));
  const positioned = await request(`/api/v1/messages/${encodeURIComponent(results[0].stable_message_id)}/position?sort=desc`);
  const position = await positioned.json();
  check("search-http-position", positioned.ok && Number.isSafeInteger(position.offset_in_conversation));
  const deepResponse = await request('/api/v1/messages/acceptance-00000/position?sort=desc');
  const deep = await deepResponse.json();
  const summary = await (await request('/api/v1/archive/summary')).json();
  const start = Math.floor(deep.offset_in_conversation / 100) * 100;
  const page = new URLSearchParams({ conversation_id: deep.conversation_id, limit: '100', offset: String(start), sort: 'desc' });
  const expectedResponse = await request(`/api/v1/messages?${page}`), expected = await expectedResponse.json();
  const anchor = new URLSearchParams(page);
  anchor.set('offset', '0'); // Prove that the anchor path actually overrides the deep OFFSET.
  anchor.set('anchor_message_id', 'acceptance-00000');
  anchor.set('anchor_shift', String(-(deep.offset_in_conversation % 100)));
  anchor.set('anchor_revision', String(summary.revision));
  const anchoredResponse = await request(`/api/v1/messages?${anchor}`), anchored = await anchoredResponse.json();
  const ids = rows => JSON.stringify(rows.map(row => row.stable_message_id));
  check('message-anchor-http-deep-page', deepResponse.ok && start > 0 && expectedResponse.ok && anchoredResponse.ok && ids(expected) === ids(anchored) && anchored.some(row => row.stable_message_id === 'acceptance-00000'));
  anchor.set('offset', String(start)); anchor.set('anchor_revision', String(summary.revision + 1));
  const staleResponse = await request(`/api/v1/messages?${anchor}`), stale = await staleResponse.json();
  check('message-anchor-http-stale-fallback', staleResponse.ok && ids(expected) === ids(stale));
  await c.evaluate("window.dispatchEvent(new KeyboardEvent('keydown',{key:'f',ctrlKey:true,altKey:true}))");
  await c.wait("!!document.querySelector('.global-search-overlay')");
  await c.evaluate("window.__acceptance.set(document.querySelector('[aria-label=\"全局搜索聊天记录\"]'),'项目')");
  await c.wait("document.querySelectorAll('.global-search-item').length === 50");
  try {
    for (const [width, height] of [[1496, 939], [1024, 768], [760, 700]]) {
      await c.send("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: false });
      await pause(150);
      const layout = await c.evaluate(`(() => {
        const content = document.querySelector('.global-search-content').getBoundingClientRect();
        const result = document.querySelector('.global-search-item').getBoundingClientRect();
        const filters = document.querySelector('.global-search-filters');
        return content.height >= 200 && result.bottom <= content.bottom && filters.scrollWidth <= filters.clientWidth;
      })()`);
      check(`search-layout-${width}`, layout);
    }
  } finally { await c.send("Emulation.clearDeviceMetricsOverride"); }
  await c.evaluate("window.__acceptance.set(document.querySelector('[aria-label=\"筛选会话\"]'),'R:acceptance-000')");
  await c.wait("document.querySelector('[aria-label=\"筛选发送人\"]').options.length === 3");
  await c.evaluate("window.__acceptance.set(document.querySelector('[aria-label=\"筛选发送人\"]'),'acceptance-a')");
  await c.wait("document.querySelectorAll('.global-search-item').length === 50 && [...document.querySelectorAll('.global-search-sender')].every(e => e.textContent === '测试成员甲')");
  check("search-sender-by-name");
  await c.evaluate("document.querySelector('.global-search-item').click()");
  await c.wait("!document.querySelector('.global-search-overlay') && !!document.querySelector('.message-row.highlighted')");
  check("search-result-position");
}

async function locateSource(c) {
  await c.evaluate("window.dispatchEvent(new KeyboardEvent('keydown',{key:'f',ctrlKey:true,altKey:true}))");
  await c.wait("!!document.querySelector('.global-search-overlay')");
  await c.evaluate("document.querySelector('.global-search-reset').click(); window.__acceptance.set(document.querySelector('[aria-label=\"全局搜索聊天记录\"]'),'合成图片')");
  await c.wait("document.querySelectorAll('.global-search-item').length === 1");
  await c.evaluate("document.querySelector('.global-search-item').click()");
  await c.wait("!!document.querySelector('[data-message-id=\"acceptance-00013\"] .quote-block') && !!document.querySelector('[data-message-id=\"acceptance-00011\"].highlighted') && !document.querySelector('.global-search-overlay')");
  return c.evaluate("({page:Number(document.querySelector('[aria-label=\"跳转页码\"]').value),targetInitiallyAbsent:!document.querySelector('[data-message-id=\"acceptance-00000\"]')})");
}

export async function archiveCases({ desktop, main, fresh, check }) {
  // Capture auth before installing page hooks; auth navigation would otherwise change the test page.
  const mainAuth = await desktop.auth(main);
  await desktop.auth(fresh);
  const c = await bootstrap(main);
  let freshC;
  const api = (...args) => desktop.api(main, ...args);
  const task = (id) => until(async () => {
    const result = await api(`/api/v1/exports/${id}`);
    return ["completed", "failed", "cancelled"].includes(result.value.status) && result.value;
  });
  const exportPath = (result) => {
    if (result.status !== "completed" || basename(result.result.fileName) !== result.result.fileName) throw fail("EXPORT_FAILED");
    return join(main.dataRoot, "exports", result.result.fileName);
  };
  try {
    freshC = await bootstrap(fresh);
    const before = (await api("/api/v1/archive/summary")).value;
    const empty = (await desktop.api(fresh, "/api/v1/archive/summary")).value;
    check("archive-baseline",
      before.message_count === 6209 && before.conversation_count === 125 && empty.message_count === 0);
    await c.wait("!!document.querySelector('.conversation-scroll .conversation-load-more button')");
    await c.evaluate("document.querySelector('.conversation-scroll .conversation-load-more button').click()");
    await c.wait("document.querySelectorAll('.conversation-item').length === 100");
    await c.evaluate(`(() => {
      const s = window.__acceptance; s.preDirectory = window.fetch; s.failDirectoryReset = true;
      window.fetch = async (input, init = {}) => {
        const u = new URL(typeof input === 'string' ? input : input.url, location.href);
        if (u.pathname === '/api/v1/conversations') {
          const offset = Number(u.searchParams.get('offset') || 0);
          if (!offset && s.failDirectoryReset) {
            s.failDirectoryReset = false; s.resetFailed = true;
            return Response.json({code:'SYNTHETIC_REFRESH_FAILURE',message:'Synthetic refresh failed; retry.'},{status:503});
          }
          if (offset > 0 && s.resetFailed) s.appendAfterFailedReset = offset;
        }
        return s.preDirectory(input, init);
      };
    })()`);
    if ((await desktop.upload(main, "fixture-1.json")).status !== 200) throw fail("DIRECTORY_NOTIFICATION_FAILED");
    await c.wait("window.__acceptance.resetFailed && !!document.querySelector('.conversation-load-more [role=\"alert\"]')");
    const kept = await c.evaluate("document.querySelectorAll('.conversation-item').length === 100");
    await c.evaluate("document.querySelector('.conversation-scroll .conversation-load-more button').click()");
    await c.wait("window.__acceptance.appendAfterFailedReset === 100 && document.querySelectorAll('.conversation-item').length === 125");
    check("directory-refresh-failure-cursor", kept);
    await c.evaluate("window.fetch = window.__acceptance.preDirectory");
    await searchCases(c, main, mainAuth, check);
    await c.evaluate("window.__acceptance.click('导出')");
    await c.wait("!!document.querySelector('.export-dialog-card')");
    await c.evaluate(`(() => {
      const s = window.__acceptance, card = document.querySelector('.export-dialog-card');
      [...card.querySelectorAll('label')].find(e => e.textContent.trim() === '全部档案').querySelector('input').click();
      for (const l of card.querySelectorAll('.export-presentation-options label')) {
        const e = l.querySelector('input'), desired = l.textContent.trim() === '美化信息';
        if (e.checked !== desired) e.click();
      }
      s.set(card.querySelector('[aria-label="会话内容时间排序"]'), 'ascending');
    })()`);
    await c.evaluate("window.__acceptance.click('开始导出')");
    await c.wait("!!window.__acceptance.lastExport");
    const exportId = await c.evaluate("window.__acceptance.lastExport.taskId");
    const exported = await task(exportId);
    await c.wait("!document.querySelector('.export-dialog-card')");
    const file = exportPath(exported), bytes = await readFile(file), doc = JSON.parse(bytes);
    const messages = doc.batches.flatMap((batch) => batch.messages);
    check("complete-json-contract",
      doc.schema_version === "client-export.v1" && typeof doc.content_sha256 === "string" && doc.participants.length === 2 &&
      messages.length === 6209 && new Set(messages.map((m) => m.stable_message_id)).size === 6209 &&
      doc.conversations.length === 125 && doc.conversations.flatMap((row) => row.messages).length === 6209);
    const completedCancel = await api(`/api/v1/exports/${exportId}/cancel`, {});
    check("completed-export-cancel-preserves-file", completedCancel.status === 200 && completedCancel.value.status === "completed" && digest(await readFile(file)) === digest(bytes));
    const same = await importUi(c, file);
    check("same-archive-roundtrip", same.status === 200 && same.inserted === 0 && same.unchanged === 6209 && same.revised === 0);
    const loaded = await importUi(freshC, file);
    check("fresh-archive-roundtrip", loaded.status === 200 && loaded.inserted === 6209);
    const repeated = await importUi(freshC, file);
    check("repeat-import", repeated.status === 200 && repeated.inserted === 0 && repeated.unchanged === 6209 && repeated.revised === 0);
    for (const [scope, extra, count] of [
      ["current_conversation", { conversationId: "R:acceptance-000" }, 6009],
      ["current_filter", { conversationId: "R:acceptance-000", text: "shared-match", participantId: "acceptance-b", messageType: "file", mediaOnly: true }, 1],
      ["selected_conversations", { conversationIds: ["R:acceptance-001", "R:acceptance-002"] }, 4],
      ["date_range", { startDate: "2026-01-02", endDate: "2026-01-02" }, 201],
    ]) {
      const start = await api("/api/v1/exports/start", { scope, format: "json", simplify: false, pretty: false, dataRedaction: false, messageOrder: "descending", ...extra });
      const b = await readFile(exportPath(await task(start.value.taskId))), d = JSON.parse(b);
      const response = await localFetch(main.origin, "/api/v1/imports/json", { method: "POST", headers: { Authorization: mainAuth }, body: b });
      const r = await response.json();
      check(`export-scope-${scope}`,
        d.schema_version === "client-export.v1" && d.message_count === count && response.status === 200 && r.inserted === 0 && r.revised === 0);
    }
    const prior = (await desktop.api(fresh, "/api/v1/archive/summary")).value;
    const tampered = structuredClone(doc);
    tampered.batches[0].messages[0].body_text = "Tampered synthetic payload";
    const bad = await desktop.api(fresh, "/api/v1/imports/json", tampered);
    const afterBad = (await desktop.api(fresh, "/api/v1/archive/summary")).value;
    check("json-tamper-rejected",
      bad.status === 422 && prior.revision === afterBad.revision && prior.message_count === afterBad.message_count);
    const simple = await api("/api/v1/exports/start", { scope: "current_conversation", conversationId: "R:acceptance-000", format: "json", simplify: true, pretty: false });
    const simplified = JSON.parse(await readFile(exportPath(await task(simple.value.taskId)), "utf8"));
    const simpleMessages = simplified.conversations.flatMap((row) => row.messages);
    const idsOmitted = simpleMessages.every((m) => !m.stable_message_id && !m.raw_payload);
    check("simplified-reading-json",
      simpleMessages.length === 6009 && idsOmitted && !simplified.schema_version && !simplified.batches);
    const exportRoot = join(main.dataRoot, "exports"), filesBefore = await readdir(exportRoot);
    const cancel = await api("/api/v1/exports/start", { scope: "entire_archive", format: "json", simplify: false, pretty: true });
    let observed = false;
    for (let i = 0; i < 200 && !observed; i++) {
      for (const f of (await readdir(exportRoot)).filter((name) => name.endsWith(".partial"))) {
        try { if ((await stat(join(exportRoot, f))).size > 0) observed = true; } catch { /* May finish between listing and stat. */ }
      }
      if (!observed) await pause(2);
    }
    await api(`/api/v1/exports/${cancel.value.taskId}/cancel`, {});
    const cancelled = await task(cancel.value.taskId), filesAfter = await readdir(exportRoot);
    const unchanged = digest(await readFile(file)) === digest(bytes);
    check("cancel-export-cleanup",
      observed && cancelled.status === "cancelled" && filesBefore.length === filesAfter.length && !filesAfter.some((f) => f.endsWith(".partial")) && unchanged);
    for (const size of [100, 500]) {
      await c.evaluate(`window.__acceptance.set(document.querySelector('[aria-label="每页数量"]'), '${size}')`);
      await c.wait(`document.querySelector('[aria-label="每页数量"]').value === '${size}'`);
      const source = await locateSource(c);
      await c.evaluate("document.querySelector('[data-message-id=\"acceptance-00013\"] .quote-block').click()");
      await c.wait("!!document.querySelector('[data-message-id=\"acceptance-00000\"].highlighted')");
      const page = await c.evaluate("Number(document.querySelector('[aria-label=\"跳转页码\"]').value)");
      check(`quote-desc-cross-page-${size}`, source.targetInitiallyAbsent && source.page !== page);
    }
    await c.evaluate("window.__acceptance.click('按时间降序')");
    await c.wait("!!document.querySelector('[data-message-id=\"acceptance-00013\"] .quote-block')");
    const positions = await c.evaluate("window.__acceptance.positions.length");
    await c.evaluate("document.querySelector('[data-message-id=\"acceptance-00013\"] .quote-block').click()");
    await c.wait("!!document.querySelector('[data-message-id=\"acceptance-00000\"].highlighted')");
    const noExtra = positions === await c.evaluate("window.__acceptance.positions.length");
    check("quote-ascending-same-page", noExtra);
    await c.evaluate("window.__acceptance.click('按时间升序')");
    await locateSource(c);
    await c.evaluate(`(() => {
      const s = window.__acceptance; s.preDelay = window.fetch;
      window.fetch = async (input, init = {}) => {
        const u = new URL(typeof input === 'string' ? input : input.url, location.href);
        const target = u.pathname.endsWith('/acceptance-00000/position');
        if (target && s.failPosition) { s.failPosition = false; return Response.json({code:'NOT_FOUND',message:'合成原消息暂不可用，请重试。'},{status:404}); }
        const r = await s.preDelay(input, init);
        if (!target || !s.delayPosition) return r;
        s.delayPosition = false;
        const b = await r.arrayBuffer();
        await new Promise(resolve => s.releasePosition = resolve);
        return new Response(b, {status:r.status,headers:r.headers});
      };
      s.delayPosition = true; document.querySelector('[data-message-id="acceptance-00013"] .quote-block').click();
    })()`);
    await c.wait("typeof window.__acceptance.releasePosition === 'function'");
    await c.evaluate("document.querySelector('[aria-label=\"上一页\"]').click()");
    const manualPage = await c.evaluate("document.querySelector('[aria-label=\"跳转页码\"]').value");
    await c.evaluate("window.__acceptance.releasePosition()");
    await pause(400);
    const stayed = await c.evaluate(`document.querySelector('[aria-label="跳转页码"]').value === ${JSON.stringify(manualPage)}`);
    check("quote-stale-location", stayed);
    await locateSource(c);
    await c.evaluate("window.__acceptance.releasePosition=null; window.__acceptance.delayPosition=true; document.querySelector('[data-message-id=\"acceptance-00013\"] .quote-block').click()");
    await c.wait("typeof window.__acceptance.releasePosition === 'function'");
    const cancelPage = await c.evaluate("document.querySelector('[aria-label=\"跳转页码\"]').value");
    await c.evaluate("window.__acceptance.click('取消定位'); window.__acceptance.releasePosition()");
    await pause(300);
    const cancelledQuote = await c.evaluate(`document.querySelector('[aria-label="跳转页码"]').value === ${JSON.stringify(cancelPage)} && !document.querySelector('.timeline-location-status[role="status"]')`);
    check("quote-user-cancel", cancelledQuote);
    await c.evaluate("window.__acceptance.failPosition=true; document.querySelector('[data-message-id=\"acceptance-00013\"] .quote-block').click()");
    await c.wait("!!document.querySelector('.timeline-location-status[role=\"alert\"]')");
    await c.evaluate("document.querySelector('[data-message-id=\"acceptance-00013\"] .quote-block').click()");
    await c.wait("!!document.querySelector('[data-message-id=\"acceptance-00000\"].highlighted')");
    check("quote-position-error-retry");
    const final = (await api("/api/v1/archive/summary")).value, freshSummary = (await desktop.api(fresh, "/api/v1/archive/summary")).value;
    check("archive-final-counts",
      final.message_count === 6209 && freshSummary.message_count === 6209 && final.conversation_count === 125 && freshSummary.conversation_count === 125);
  } finally {
    const cleanup = await Promise.allSettled([c, freshC].filter(Boolean).map(async (page) => {
      try { await page.evaluate("window.__acceptance?.releasePosition?.(); if(window.__acceptance){window.fetch=window.__acceptance.nativeFetch; delete window.__acceptance}"); }
      finally { page.close(); }
    }));
    if (cleanup.some((item) => item.status === "rejected")) throw fail("PAGE_HOOK_CLEANUP_FAILED");
  }
}
