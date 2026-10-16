// Uses an existing local Chromium browser and synthetic acceptance host only.
// No downloads, real archives, or credentials are used.
import { spawn } from "node:child_process";
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { resolve } from "node:path";

const [rootArgument, browser] = process.argv.slice(2);
if (!rootArgument || !browser) throw new Error("usage: node verify-search-ui.mjs <synthetic-run-dir> <installed-chromium>");
const root = resolve(rootArgument);
if (!root.startsWith(resolve("target/ui-acceptance") + "\\")) throw new Error("synthetic acceptance directory required");
const { pageUrl } = JSON.parse(await readFile(resolve(root, "connection.json"), "utf8"));
const address = new URL(pageUrl);
if (address.hostname !== "127.0.0.1" || address.port !== "19812") throw new Error("unexpected acceptance host");
const token = new URLSearchParams(address.hash.slice(1)).get("token");
const request = async (path, options = {}) => {
  const response = await fetch(new URL(path, address), { ...options, headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json", ...options.headers } });
  if (!response.ok) throw new Error(`acceptance API status ${response.status}`);
  return response.json();
};
const criteria = new URLSearchParams({ q: "项目", locate: "false", conversation_id: "R:synthetic-project", participant_id: "synthetic-member-a", message_type: "text", starts_at: "2026-10-12T16:00:00Z", ends_at: "2026-10-13T15:59:59.999Z" });
const search = await request(`/api/v1/search/messages?${criteria}`);
if (search.length !== 50 || search.some(row => row.offset_in_conversation != null || row.sender_id !== "synthetic-member-a" || row.message_type !== "text")) throw new Error("HTTP combined search failed");
const position = await request(`/api/v1/messages/${search[0].stable_message_id}/position?sort=desc`);
if (!Number.isSafeInteger(position.offset_in_conversation)) throw new Error("HTTP positioning failed");
await mkdir(resolve(root, "browser"), { recursive: true });
const child = spawn(browser, ["--headless=new", "--disable-gpu", "--no-first-run", "--no-default-browser-check", "--disable-background-networking", "--remote-debugging-port=19222", `--user-data-dir=${resolve(root, "browser")}`, "about:blank"], { windowsHide: true, stdio: "ignore" });
let ws;
const pending = new Map();
let sequence = 0;
const waitUntil = async (action, timeout = 15000) => {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) { const value = await action(); if (value) return value; await new Promise(r => setTimeout(r, 100)); }
  throw new Error("synthetic browser acceptance deadline exceeded");
};
try {
  const targets = await waitUntil(async () => { try { return await (await fetch("http://127.0.0.1:19222/json/list")).json(); } catch { return false; } });
  ws = new WebSocket(targets.find(t => t.type === "page").webSocketDebuggerUrl);
  ws.addEventListener("message", ({ data }) => {
    const message = JSON.parse(data);
    const entry = pending.get(message.id);
    if (!entry) return;
    pending.delete(message.id);
    if (message.error) entry.reject(new Error("browser protocol error")); else entry.resolve(message.result);
  });
  await new Promise((resolve, reject) => { ws.addEventListener("open", resolve, { once: true }); ws.addEventListener("error", reject, { once: true }); });
  const send = (method, params = {}) => new Promise((resolve, reject) => { const id = ++sequence; pending.set(id, { resolve, reject }); ws.send(JSON.stringify({ id, method, params })); });
  const evaluate = async expression => {
    const result = await send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
    if (result.exceptionDetails) throw new Error("synthetic browser evaluation failed");
    return result.result.value;
  };
  await send("Page.enable");
  await send("Emulation.setDeviceMetricsOverride", { width: 1496, height: 939, deviceScaleFactor: 1, mobile: false });
  await send("Page.navigate", { url: pageUrl });
  await waitUntil(() => evaluate(`Boolean(document.querySelector('.app-shell'))`));
  await evaluate(`Array.from(document.querySelectorAll('.primary-nav button')).find(e => e.textContent === '会话').click()`);
  await waitUntil(() => evaluate(`Boolean(document.querySelector('.conversation-item'))`));
  await evaluate(`window.dispatchEvent(new KeyboardEvent('keydown', {key:'f',ctrlKey:true,altKey:true}));`);
  await waitUntil(() => evaluate(`Boolean(document.querySelector('.global-search-overlay'))`));
  await evaluate(`(() => { const input=document.querySelector('[aria-label="全局搜索聊天记录"]'); Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(input,'项目'); input.dispatchEvent(new Event('input',{bubbles:true})); })()`);
  await waitUntil(() => evaluate(`document.querySelectorAll('.global-search-item').length === 50`));
  const report = { httpSearch: search.length, httpPosition: true, layouts: [] };
  for (const [width, height] of [[1496, 939], [1024, 768], [760, 700]]) {
    await send("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: false });
    await new Promise(r => setTimeout(r, 150));
    const layout = await evaluate(`(() => { const box = e => { const r=e.getBoundingClientRect(); return {top:r.top,bottom:r.bottom,height:r.height,left:r.left,right:r.right}; }; return {content:box(document.querySelector('.global-search-content')),filters:box(document.querySelector('.global-search-filters')),result:box(document.querySelector('.global-search-item')),footer:box(document.querySelector('.global-search-footer')),overflow:document.querySelector('.global-search-filters').scrollWidth > document.querySelector('.global-search-filters').clientWidth}; })()`);
    if (layout.content.height < 200 || layout.result.bottom > layout.content.bottom || layout.overflow) throw new Error(`search layout invalid at ${width}`);
    report.layouts.push({ width, height, contentHeight: Math.round(layout.content.height), resultVisible: true, overflow: false });
    const screenshot = await send("Page.captureScreenshot", { format: "png" });
    await writeFile(resolve(root, `search-${width}.png`), Buffer.from(screenshot.data, "base64"));
  }
  await evaluate(`document.querySelector('[aria-label="筛选会话"]').value='R:synthetic-project'; document.querySelector('[aria-label="筛选会话"]').dispatchEvent(new Event('change',{bubbles:true}));`);
  await waitUntil(() => evaluate(`document.querySelector('[aria-label="筛选发送人"]').options.length === 3`));
  await evaluate(`document.querySelector('[aria-label="筛选发送人"]').value='synthetic-member-a'; document.querySelector('[aria-label="筛选发送人"]').dispatchEvent(new Event('change',{bubbles:true}));`);
  await waitUntil(() => evaluate(`document.querySelectorAll('.global-search-item').length === 50 && Array.from(document.querySelectorAll('.global-search-sender')).every(e=>e.textContent==='测试成员甲')`));
  report.senderByName = true;
  await evaluate(`document.querySelector('.global-search-item').click()`);
  await waitUntil(() => evaluate(`!document.querySelector('.global-search-overlay')`));
  await waitUntil(() => evaluate(`Boolean(document.querySelector('.message-row.highlighted'))`));
  report.messagePosition = true;
  await evaluate(`Array.from(document.querySelectorAll('.primary-nav button')).find(e=>e.textContent==='设置').click()`);
  await waitUntil(() => evaluate(`document.querySelector('.settings-page')?.textContent.includes('0.0.9')`));
  report.settings = true;
  const started = await request("/api/v1/exports/start", { method: "POST", body: JSON.stringify({ scope: "entire_archive", format: "json", simplify: true }) });
  const completed = await waitUntil(async () => { const p = await request(`/api/v1/exports/${started.taskId}`); if (p.status === "failed") throw new Error("HTTP export failed"); return p.status === "completed" ? p : false; });
  if (completed.result.messageCount !== 6000) throw new Error("HTTP export count mismatch");
  const afterCancel = await request(`/api/v1/exports/${started.taskId}/cancel`, { method: "POST" });
  if (afterCancel.status !== "completed") throw new Error("completed file changed after cancel");
  const toCancel = await request("/api/v1/exports/start", { method: "POST", body: JSON.stringify({ scope: "entire_archive", format: "json" }) });
  await request(`/api/v1/exports/${toCancel.taskId}/cancel`, { method: "POST" });
  const cancelled = await waitUntil(async () => { const p = await request(`/api/v1/exports/${toCancel.taskId}`); return ["cancelled", "completed"].includes(p.status) ? p : false; });
  if (cancelled.status !== "cancelled") throw new Error("synthetic export completed before cancellation could be verified");
  report.exportMessages = completed.result.messageCount;
  report.cancellation = "confirmed";
  await writeFile(resolve(root, "browser-report.json"), JSON.stringify(report, null, 2));
  console.log(JSON.stringify(report));
} finally {
  ws?.close();
  child.kill();
}
