import { readFile, writeFile } from "node:fs/promises";

const indexPath = new URL("../dist/client/index.html", import.meta.url);
let html = await readFile(indexPath, "utf8");
// Tauri's embedded asset protocol is not an HTTP CORS server. Vite's
// crossorigin attributes make WebView2 treat local module/style assets as
// cross-origin requests and can leave the collector window completely blank.
html = html.replace(/\s+crossorigin(?:="")?/g, "");
await writeFile(indexPath, html, "utf8");
