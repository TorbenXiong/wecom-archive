// Production frontend + synthetic SILK only. Uses an installed Chromium browser;
// no browser downloads, real archives or credentials are needed.
import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { resolve, extname, sep } from "node:path";
import { encode } from "silk-wasm";

const [browser] = process.argv.slice(2);
if (!browser) throw new Error("usage: node scripts/verify-voice-ui.mjs <installed-chromium>");
const root = resolve(`target/ui-acceptance/voice-${Date.now()}`);
const webRoot = resolve("dist/server");
const downloadRoot = resolve(root, "downloads");
await mkdir(downloadRoot, { recursive: true });
const pcm = new Int16Array(24_000 * 3);
for (let index = 0; index < pcm.length; index++) pcm[index] = Math.round(Math.sin(index * 2 * Math.PI * 440 / 24_000) * 8_000);
const { data: silk } = await encode(pcm, 24_000);
const corrupt = Buffer.from("#!SILK_V3\x01\x00x");
const messages = [0, 1, 2].map((index) => ({
  stable_message_id: `synthetic-voice-${index}`, conversation_id: "synthetic-voices", sender_id: "synthetic-sender", sender_name: "合成语音测试",
  sent_at: `2026-11-04T08:00:0${index}Z`, direction: "incoming", message_type: "audio", body_text: index === 0 ? "离线播放测试" : undefined,
  lifecycle: "active", raw_type: "16", media: [{ content_hash: `synthetic-${index}`, original_name: `synthetic-${index}.silk`, mime_type: "audio/silk", size_bytes: index === 2 ? corrupt.length : silk.length, source_locator: "synthetic", integrity: "verified" }],
}));
const responseJson = (response, value) => { response.writeHead(200, { "Content-Type": "application/json" }); response.end(JSON.stringify(value)); };
const server = createServer(async (request, response) => {
  try {
    const path = new URL(request.url, "http://127.0.0.1").pathname;
    if (path === "/api/v1/archive/summary") return responseJson(response, { conversation_count: 1, message_count: 3, media_count: 3, revision: 1, local_collection_available: false });
    if (path === "/api/v1/enterprise/config") return responseJson(response, { configured: false, superAdminEnabled: true, dataRedaction: false });
    if (path === "/api/v1/archive/collected-users" || path.endsWith("/participants")) return responseJson(response, []);
    if (path === "/api/v1/archive/updates") { const timer = setTimeout(() => responseJson(response, { revision: 1 }), 1_000); response.on("close", () => clearTimeout(timer)); return; }
    if (path === "/api/v1/conversations") return responseJson(response, [{ conversation_id: "synthetic-voices", display_name: "语音验收", last_message_at: messages[0].sent_at, message_count: 3, media_count: 3, participant_count: 1 }]);
    if (path === "/api/v1/messages/count") return responseJson(response, { total: 3 });
    if (path === "/api/v1/messages") return responseJson(response, messages);
    if (path.startsWith("/api/v1/media/")) { response.writeHead(200, { "Content-Type": "audio/silk" }); response.end(path.endsWith("synthetic-2") ? corrupt : silk); return; }
    if (path.startsWith("/api/")) { response.writeHead(404); response.end(); return; }
    const file = resolve(webRoot, `.${path === "/" ? "/index.html" : path}`);
    if (!file.startsWith(webRoot + sep)) throw new Error("invalid asset path");
    const bytes = await readFile(file);
    response.writeHead(200, { "Content-Type": ({ ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".wasm": "application/wasm" })[extname(file)] || "application/octet-stream" });
    response.end(bytes);
  } catch { response.writeHead(404); response.end(); }
});
await new Promise((done) => server.listen(0, "127.0.0.1", done));
const pageUrl = `http://127.0.0.1:${server.address().port}/#token=synthetic-voice-test-token-only`;
const child = spawn(browser, ["--headless=new", "--disable-gpu", "--no-first-run", "--no-default-browser-check", "--disable-background-networking", "--remote-debugging-port=0", `--user-data-dir=${resolve(root, "browser")}`, "about:blank"], { windowsHide: true, stdio: "ignore" });
const waitUntil = async (action, timeout = 15_000) => {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) { const value = await action(); if (value) return value; await new Promise((done) => setTimeout(done, 100)); }
  throw new Error("synthetic voice acceptance timed out");
};
let ws;
const pending = new Map();
let sequence = 0;
try {
  const port = await waitUntil(async () => { try { return (await readFile(resolve(root, "browser/DevToolsActivePort"), "utf8")).split("\n")[0]; } catch { return false; } });
  const targets = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
  ws = new WebSocket(targets.find((target) => target.type === "page").webSocketDebuggerUrl);
  ws.addEventListener("message", ({ data }) => {
    const message = JSON.parse(data);
    const entry = pending.get(message.id);
    if (!entry) return;
    pending.delete(message.id);
    if (message.error) entry.reject(new Error("browser protocol failed")); else entry.resolve(message.result);
  });
  await new Promise((done, reject) => { ws.addEventListener("open", done, { once: true }); ws.addEventListener("error", reject, { once: true }); });
  const send = (method, params = {}) => new Promise((done, reject) => { const id = ++sequence; pending.set(id, { resolve: done, reject }); ws.send(JSON.stringify({ id, method, params })); });
  const evaluate = async (expression) => {
    const result = await send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
    if (result.exceptionDetails) throw new Error(`browser evaluation failed: ${result.exceptionDetails.text}`);
    return result.result.value;
  };
  const click = async (selector, index = 0) => {
    const point = await evaluate(`(() => { const r=document.querySelectorAll(${JSON.stringify(selector)})[${index}].getBoundingClientRect(); return {x:r.x+r.width/2,y:r.y+r.height/2}; })()`);
    await send("Input.dispatchMouseEvent", { type: "mousePressed", ...point, button: "left", clickCount: 1 });
    await send("Input.dispatchMouseEvent", { type: "mouseReleased", ...point, button: "left", clickCount: 1 });
  };
  await send("Browser.setDownloadBehavior", { behavior: "allow", downloadPath: downloadRoot });
  await send("Emulation.setDeviceMetricsOverride", { width: 1100, height: 760, deviceScaleFactor: 1, mobile: false });
  await send("Page.navigate", { url: pageUrl });
  await waitUntil(() => evaluate("Boolean(document.querySelector('.app-shell'))"));
  await evaluate("Array.from(document.querySelectorAll('.primary-nav button')).find(e=>e.textContent==='会话').click()");
  await waitUntil(() => evaluate("document.querySelectorAll('.voice-bubble').length===3 && document.querySelector('.voice-duration')?.textContent==='3″'"));
  await click(".voice-bubble");
  await waitUntil(() => evaluate("document.querySelector('audio').currentTime>0.2 && !document.querySelector('audio').paused"));
  const report = { productionWorkerPlayback: true, decodedSeconds: await evaluate("document.querySelector('audio').duration") };
  if (Math.abs(report.decodedSeconds - 3) > 0.05) throw new Error("decoded duration mismatch");
  report.layout = await evaluate("(() => { const row=document.querySelector('.voice-row'); const a=row.querySelector('.voice-bubble').getBoundingClientRect(), b=row.querySelector('.voice-save').getBoundingClientRect(), c=row.querySelector('.voice-transcript').getBoundingClientRect(); return {inlineSave:Math.abs((a.top+a.height/2)-(b.top+b.height/2))<1, saveFirst:row.firstElementChild.classList.contains('voice-save'), transcriptRight:c.left>=a.right && Math.abs((c.top+c.height/2)-(a.top+a.height/2))<1, width:a.width}; })()");
  if (!report.layout.inlineSave || !report.layout.saveFirst || !report.layout.transcriptRight) throw new Error("voice layout is invalid");
  const screenshot = await send("Page.captureScreenshot", { format: "png" });
  await writeFile(resolve(root, "voice-playing.png"), Buffer.from(screenshot.data, "base64"));
  await click(".voice-bubble");
  await waitUntil(() => evaluate("document.querySelector('audio').paused"));
  report.pauseResume = true;
  await click(".voice-bubble");
  await waitUntil(() => evaluate("!document.querySelector('audio').paused"));
  await click(".voice-bubble", 1);
  await waitUntil(() => evaluate("document.querySelectorAll('audio')[0].paused && !document.querySelectorAll('audio')[1].paused"));
  report.singleActiveVoice = true;
  await waitUntil(() => evaluate("document.querySelectorAll('audio')[1].ended"));
  report.endReset = await evaluate("document.querySelectorAll('.voice-duration')[1].textContent==='3″'");
  await click(".voice-save");
  await waitUntil(async () => { try { return (await readFile(resolve(downloadRoot, "synthetic-0.silk"))).equals(Buffer.from(silk)); } catch { return false; } });
  report.originalSilkSave = true;
  await click(".voice-bubble", 2);
  await waitUntil(() => evaluate("Boolean(document.querySelector('[role=alert]'))"));
  report.decodeFailureCanSave = await evaluate("!document.querySelectorAll('.voice-save')[2].disabled");
  if (!report.endReset || !report.decodeFailureCanSave) throw new Error("voice lifecycle validation failed");
  await writeFile(resolve(root, "report.json"), JSON.stringify(report, null, 2));
  console.log(JSON.stringify({ root, ...report }, null, 2));
} finally {
  ws?.close();
  child.kill();
  server.closeAllConnections();
  await new Promise((done) => server.close(done));
}
