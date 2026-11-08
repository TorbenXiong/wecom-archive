// Reuse an explicitly authorized, isolated real archive. Never collect again here.
import { randomUUID } from "node:crypto";
import { lstat, readFile, readdir, realpath } from "node:fs/promises";
import { basename, join, relative, resolve, sep } from "node:path";
import { connect, digest, fail, localFetch, until, validateRun, within } from "./runtime.mjs";

const uuid = /^[a-f0-9]{8}-[a-f0-9]{4}-[1-8][a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/i;

async function ownedPath(root, path, directory) {
  if (typeof path !== "string" || !within(root, path)) throw fail("UNSAFE_REAL_RECOVERY_PATH");
  let current = resolve(root);
  for (const component of relative(current, resolve(path)).split(sep)) {
    current = join(current, component);
    if ((await lstat(current)).isSymbolicLink()) throw fail("UNSAFE_REAL_RECOVERY_PATH");
  }
  const info = await lstat(path);
  if (!(directory ? info.isDirectory() : info.isFile()) || !within(await realpath(root), await realpath(path))) {
    throw fail("UNSAFE_REAL_RECOVERY_PATH");
  }
  return info;
}

async function authorizedRun(root, desktop, session) {
  try {
    await validateRun(root);
    if ((await lstat(root)).isSymbolicLink()
      || await readFile(join(root, ".authorized-real-run"), "utf8") !== "real-optimizations.v1") {
      throw fail("UNAUTHORIZED_REAL_RECOVERY_RUN");
    }
  } catch { throw fail("UNAUTHORIZED_REAL_RECOVERY_RUN"); }
  if (!desktop || !session || typeof desktop.root !== "string" || resolve(desktop.root) !== resolve(root)
    || !(desktop.sessions instanceof Map) || desktop.sessions.get(session.name) !== session || session.stopped !== false) {
    throw fail("UNOWNED_REAL_RECOVERY_SESSION");
  }
  if (!Number.isInteger(session.port) || session.port < 1 || session.port > 65535
    || !Number.isInteger(session.debugPort) || session.debugPort < 1 || session.debugPort > 65535
    || session.origin !== `http://127.0.0.1:${session.port}`) {
    throw fail("NONLOCAL_REAL_RECOVERY_SESSION");
  }
  try {
    await ownedPath(root, desktop.executable, false);
    await ownedPath(root, session.dataRoot, true);
  } catch { throw fail("UNSAFE_REAL_RECOVERY_PATH"); }
}

async function exportedFile(root, session, result) {
  if (!result || typeof result.fileName !== "string" || basename(result.fileName) !== result.fileName
    || !/^wecom-\d{8}-\d{6}\.json$/.test(result.fileName)
    || !/^[a-f0-9]{64}$/i.test(result.manifestSha256 || "")) throw fail("INVALID_REAL_RECOVERY_EXPORT");
  const directory = join(session.dataRoot, "exports"), file = join(directory, result.fileName);
  await ownedPath(root, directory, true);
  const info = await ownedPath(root, file, false);
  if (info.size <= 0 || info.size > 64 * 1024 * 1024) throw fail("REAL_RECOVERY_EXPORT_SIZE_INVALID");
  return file;
}

export async function realTaskRecoveryCases({ root, desktop, session, request, check }) {
  await authorizedRun(root, desktop, session);
  const auth = await desktop.auth(session);
  const api = async (path, body, taskId) => {
    const response = await localFetch(session.origin, path, {
      method: body === undefined ? "GET" : "POST",
      headers: { Authorization: auth, "Content-Type": "application/json", ...(taskId ? { "X-Archive-Task-Id": taskId } : {}) },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    return { status: response.status, value: await response.json() };
  };
  const c = await connect(session.debugPort, session.port);
  let installed = false;
  try {
    await c.evaluate(`(() => {
      if (window.__realTaskRecovery) throw Error('REAL_RECOVERY_HOOK_ALREADY_INSTALLED');
      const s = window.__realTaskRecovery = {
        previousFetch: window.fetch, exportPosts: 0, importPosts: 0,
        exportGets: [], importGets: [], blockImportStatus: true,
      };
      const timeout = () => Response.json({code:'UPSTREAM_TIMEOUT',message:'Acceptance response unavailable.'},{status:503});
      s.set = (e,v) => {
        Object.getOwnPropertyDescriptor(e instanceof HTMLSelectElement ? HTMLSelectElement.prototype : HTMLInputElement.prototype,'value').set.call(e,v);
        e.dispatchEvent(new Event(e instanceof HTMLSelectElement ? 'change' : 'input',{bubbles:true}));
      };
      s.click = (text, scope=document) => {
        const b = [...scope.querySelectorAll('button')].find(e => e.textContent.trim() === text);
        if (!b || b.disabled) throw Error('REAL_RECOVERY_BUTTON_UNAVAILABLE');
        b.click();
      };
      window.fetch = async (input,init={}) => {
        const u = new URL(typeof input === 'string' ? input : input.url,location.href);
        const method = (init.method || (input instanceof Request ? input.method : 'GET')).toUpperCase();
        const headers = new Headers(input instanceof Request ? input.headers : undefined);
        new Headers(init.headers).forEach((v,k) => headers.set(k,v));
        if (u.origin === location.origin && method === 'POST' && u.pathname === '/api/v1/exports/start') {
          s.exportPosts++; s.exportId = headers.get('X-Archive-Task-Id'); s.exportRequest = JSON.parse(init.body);
          if (s.exportRequest.scope !== 'current_conversation' || s.exportRequest.conversationId !== s.expectedConversationId
            || s.exportRequest.format !== 'json' || s.exportRequest.simplify || s.exportRequest.dataRedaction) throw Error('REAL_RECOVERY_EXPORT_SCOPE_INVALID');
          const response = await s.previousFetch.call(window,input,init);
          s.exportAccepted = {status:response.status,value:await response.clone().json()};
          return response.ok ? timeout() : response;
        }
        if (u.origin === location.origin && method === 'POST' && u.pathname === '/api/v1/imports/json') {
          s.importPosts++; s.importId = headers.get('X-Archive-Task-Id');
          const response = await s.previousFetch.call(window,input,init);
          s.importAccepted = {status:response.status,value:await response.clone().json()};
          return response.ok ? timeout() : response;
        }
        if (u.origin === location.origin && method === 'GET' && u.pathname.startsWith('/api/v1/exports/')) s.exportGets.push(u.pathname.slice('/api/v1/exports/'.length));
        if (u.origin === location.origin && method === 'GET' && u.pathname.startsWith('/api/v1/tasks/')) {
          s.importGets.push(u.pathname.slice('/api/v1/tasks/'.length));
          if (s.blockImportStatus) return timeout();
        }
        return s.previousFetch.call(window,input,init);
      };
    })()`);
    installed = true;

    await c.evaluate("window.dispatchEvent(new KeyboardEvent('keydown',{key:'f',ctrlKey:true,altKey:true}))");
    await c.wait("!!document.querySelector('.global-search-overlay')");
    const choices = new Set(await c.evaluate("[...document.querySelector('[aria-label=\"筛选会话\"]').options].map(e=>e.value)"));
    const conversations = await request("/api/v1/conversations?limit=200&offset=0");
    const selected = conversations.filter(row => choices.has(row.conversation_id) && row.message_count > 0)
      .sort((a, b) => a.message_count - b.message_count)[0];
    if (!selected) throw fail("REAL_RECOVERY_CONVERSATION_UNAVAILABLE");
    await c.evaluate(`window.__realTaskRecovery.expectedConversationId=${JSON.stringify(selected.conversation_id)}`);
    await c.evaluate("document.querySelector('.global-search-reset').click()");
    await c.wait("document.querySelector('[aria-label=\"筛选会话\"]').value === '' && document.querySelectorAll('.global-search-item').length === 0");
    await c.evaluate(`window.__realTaskRecovery.set(document.querySelector('[aria-label="筛选会话"]'),${JSON.stringify(selected.conversation_id)})`);
    await c.wait("!!document.querySelector('.global-search-item') && !document.querySelector('.global-search-feedback.error')");
    await c.evaluate("document.querySelector('.global-search-item').click()");
    await c.wait("!document.querySelector('.global-search-overlay') && !!document.querySelector('.message-row')");

    await c.evaluate("window.__realTaskRecovery.click('导出')");
    await c.wait("!!document.querySelector('.export-dialog-card')");
    await c.evaluate(`(() => {
      const s=window.__realTaskRecovery, card=document.querySelector('.export-dialog-card');
      [...card.querySelectorAll('label')].find(e=>e.textContent.trim()==='当前会话').querySelector('input').click();
      s.click('JSON',card);
      for (const label of card.querySelectorAll('.export-presentation-options label')) {
        const e=label.querySelector('input'); if(e.checked)e.click();
      }
    })()`);
    await c.wait("[...document.querySelectorAll('.export-presentation-options input')].every(e=>!e.checked) && [...document.querySelectorAll('.format-grid .selected')].some(e=>e.textContent==='JSON')");
    await c.evaluate("window.__realTaskRecovery.click('开始导出',document.querySelector('.export-dialog-card'))");
    await c.wait("!!window.__realTaskRecovery.exportAccepted && [...document.querySelectorAll('.export-dialog-card button')].some(e=>e.textContent.trim()==='继续查询' && !e.disabled)");
    const exportStart = await c.evaluate(`(() => {
      const s=window.__realTaskRecovery;
      const options=[...document.querySelectorAll('.export-dialog-options input, .export-dialog-options select, .export-dialog-options button')];
      return {id:s.exportId,accepted:s.exportAccepted,request:s.exportRequest,posts:s.exportPosts,gets:s.exportGets.length,
        locked:options.length > 0 && options.every(e=>e.matches(':disabled'))};
    })()`);
    if (!uuid.test(exportStart.id || "")) throw fail("INVALID_REAL_EXPORT_TASK_ID");
    check("real-export-accepted-response-unknown", exportStart.accepted.status === 200
      && exportStart.accepted.value.taskId === exportStart.id && exportStart.posts === 1 && exportStart.gets === 0 && exportStart.locked
      && exportStart.request.scope === "current_conversation" && exportStart.request.conversationId === selected.conversation_id
      && exportStart.request.format === "json" && !exportStart.request.simplify && !exportStart.request.dataRedaction);
    await c.evaluate("window.__realTaskRecovery.click('继续查询',document.querySelector('.export-dialog-card'))");
    await c.wait("!document.querySelector('.export-dialog-card')");
    const exported = await request(`/api/v1/exports/${exportStart.id}`);
    const exportQueries = await c.evaluate("({posts:window.__realTaskRecovery.exportPosts,ids:window.__realTaskRecovery.exportGets})");
    check("real-export-query-original-task-once", exported.status === "completed" && exported.result?.taskId === exportStart.id
      && exported.result?.messageCount > 0 && exportQueries.posts === 1
      && exportQueries.ids.length > 0 && exportQueries.ids.every(id => id === exportStart.id));
    const file = await exportedFile(root, session, exported.result);
    const hash = digest(await readFile(file));
    check("real-export-recovered-file-hash", hash === exported.result.manifestSha256.toLowerCase());
    const exportFiles = await readdir(join(session.dataRoot, "exports"));

    const input = await c.send("Runtime.evaluate", { expression: "document.querySelector('[aria-label=\"导入会话\"]')", returnByValue: false });
    if (!input.result?.objectId) throw fail("REAL_IMPORT_INPUT_UNAVAILABLE");
    await c.send("DOM.setFileInputFiles", { objectId: input.result.objectId, files: [file] });
    await c.wait("!!window.__realTaskRecovery.importAccepted && [...document.querySelectorAll('.archive-task-status button')].some(e=>e.textContent.trim()==='继续查询' && !e.disabled)", 90_000);
    const imported = await c.evaluate("({id:window.__realTaskRecovery.importId,accepted:window.__realTaskRecovery.importAccepted,posts:window.__realTaskRecovery.importPosts,ids:window.__realTaskRecovery.importGets,locked:document.querySelector('[aria-label=\"导入会话\"]').disabled,uncertain:document.querySelector('.archive-task-status')?.textContent.includes('结果暂未确认')})");
    if (!uuid.test(imported.id || "")) throw fail("INVALID_REAL_IMPORT_TASK_ID");
    check("real-import-accepted-response-unknown", imported.accepted.status === 200 && imported.posts === 1
      && imported.locked && imported.uncertain && imported.ids.length >= 3 && imported.ids.every(id => id === imported.id));
    const result = imported.accepted.value;
    check("real-import-duplicate-preserves-archive", result.inserted === 0 && result.revised === 0
      && result.unchanged === exported.result.messageCount && result.unchanged > 0);
    await c.evaluate("window.__realTaskRecovery.blockImportStatus=false; window.__realTaskRecovery.click('继续查询',document.querySelector('.archive-task-status'))");
    await c.wait("!document.querySelector('.archive-task-status') && !document.querySelector('[aria-label=\"导入会话\"]').disabled");
    const importTask = await request(`/api/v1/tasks/${imported.id}`);
    const importQueries = await c.evaluate("({posts:window.__realTaskRecovery.importPosts,ids:window.__realTaskRecovery.importGets})");
    check("real-import-query-original-task-once", importTask.taskId === imported.id && importTask.status === "completed"
      && importTask.result?.inserted === 0 && importTask.result?.revised === 0 && importTask.result?.unchanged === result.unchanged
      && importQueries.posts === 1 && importQueries.ids.length > imported.ids.length && importQueries.ids.every(id => id === imported.id));

    // Invalid scope arguments fail after registration and do not create files.
    // Run this last: the intentionally full registry rejects subsequent exports.
    const invalid = { scope: "current_conversation", format: "json" };
    for (let index = 0; index < 63; index++) {
      const id = randomUUID(), started = await api("/api/v1/exports/start", invalid, id);
      if (started.status !== 200 || started.value.taskId !== id) throw fail("REAL_EXPORT_CAPACITY_SETUP_FAILED");
      await until(async () => {
        const task = await request(`/api/v1/exports/${id}`);
        if (task.status === "completed" || task.status === "cancelled") throw fail("REAL_EXPORT_CAPACITY_SETUP_FAILED");
        return task.status === "failed";
      });
    }
    const rejected = await api("/api/v1/exports/start", invalid, randomUUID());
    check("real-export-capacity-explicit-refusal", rejected.status === 503 && rejected.value.code === "EXPORT_TASK_CAPACITY_REACHED");
    const preserved = await request(`/api/v1/exports/${exportStart.id}`);
    const duplicate = await api("/api/v1/exports/start", invalid, exportStart.id);
    check("real-export-capacity-retains-original-result", JSON.stringify(preserved) === JSON.stringify(exported)
      && digest(await readFile(await exportedFile(root, session, preserved.result))) === hash
      && duplicate.status === 409 && duplicate.value.code === "TASK_ALREADY_EXISTS"
      && JSON.stringify((await readdir(join(session.dataRoot, "exports"))).sort()) === JSON.stringify(exportFiles.sort()));
  } finally {
    try {
      if (installed) await c.evaluate("if(window.__realTaskRecovery){window.fetch=window.__realTaskRecovery.previousFetch;delete window.__realTaskRecovery}");
    } finally { c.close(); }
  }
}
