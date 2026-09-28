import { createServer } from "node:http";
import { readFile, stat } from "node:fs/promises";
import { extname, join, normalize } from "node:path";

const ROOT = process.argv[2];
const PORT = Number(process.argv[3]);
const EMULATE = process.argv[4] === "legacy";           // strip @layer + optional probe stubs

const MIME = {
  ".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8", ".json": "application/json",
  ".png": "image/png", ".svg": "image/svg+xml", ".woff2": "font/woff2",
  ".ico": "image/x-icon", ".wasm": "application/wasm", ".map": "application/json",
};

function skipString(css, i) {
  const quote = css[i];
  let j = i + 1;
  while (j < css.length) {
    if (css[j] === "\\") { j += 2; continue; }
    if (css[j] === quote) return j + 1;
    j++;
  }
  return j;
}

function matchBrace(css, openIndex) {
  let depth = 0;
  let i = openIndex;
  while (i < css.length) {
    const c = css[i];
    if (c === "/" && css[i + 1] === "*") { const e = css.indexOf("*/", i + 2); i = e === -1 ? css.length : e + 2; continue; }
    if (c === '"' || c === "'") { i = skipString(css, i); continue; }
    if (c === "{") depth++;
    else if (c === "}") { depth--; if (depth === 0) return i; }
    i++;
  }
  return css.length;
}

// Old WebKit/Chromium ignore unknown at-rules wholesale, so a `@layer` block never applies.
function stripLayers(css) {
  let out = "";
  let i = 0;
  while (i < css.length) {
    const c = css[i];
    if (c === "/" && css[i + 1] === "*") { const e = css.indexOf("*/", i + 2); const s = e === -1 ? css.length : e + 2; out += css.slice(i, s); i = s; continue; }
    if (c === '"' || c === "'") { const s = skipString(css, i); out += css.slice(i, s); i = s; continue; }
    if (c === "@" && /^@layer(?=[\s{;,])/i.test(css.slice(i, i + 8))) {
      let j = i + 6;
      while (j < css.length && css[j] !== ";" && css[j] !== "{") j++;
      if (css[j] === ";") { i = j + 1; continue; }
      if (css[j] === "{") { i = matchBrace(css, j) + 1; continue; }
      i = j; continue;
    }
    out += c;
    i++;
  }
  return out;
}

const STUB = {
  // Answer sets the real engines give for the probes in index.html.
  safari15: { ua: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/15.2 Safari/605.1.15", tauri: true, layer: false, mediaRange: false,
    unsupported: [["color", "oklch"], ["color", "color-mix"], ["selector", ":has"], ["selector", ":focus-visible"]] },
  safari162: { ua: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.2 Safari/605.1.15", tauri: false, layer: true, mediaRange: false,
    unsupported: [] },
  chrome90: { ua: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/90.0.4430.212 Safari/537.36", tauri: false, layer: false, mediaRange: false,
    unsupported: [["color", "oklch"], ["color", "color-mix"], ["selector", ":has"]] },
};

function stubScript(mode) {
  const spec = STUB[mode];
  if (!spec) return "";
  return `<script data-dbx-emulate="${mode}">
(function () {
  var spec = ${JSON.stringify(spec)};
  Object.defineProperty(navigator, "userAgent", { get: function () { return spec.ua; } });
  if (spec.tauri) window.__TAURI_INTERNALS__ = { emulate: true };
  var unsupported = spec.unsupported;
  var originalSupports = CSS.supports.bind(CSS);
  CSS.supports = function (a, b) {
    for (var i = 0; i < unsupported.length; i++) {
      var wantProperty = unsupported[i][0];
      var wantValue = unsupported[i][1];
      if (b === undefined) {
        var condition = String(a);
        if (condition.indexOf(wantProperty) !== -1 && condition.indexOf(wantValue) !== -1) return false;
      } else if (String(a) === wantProperty && String(b).indexOf(wantValue) !== -1) {
        return false;
      }
    }
    return b === undefined ? originalSupports(a) : originalSupports(a, b);
  };
  if (!spec.mediaRange) {
    var originalMatchMedia = window.matchMedia.bind(window);
    window.matchMedia = function (q) { return /[<>]=?/.test(q) ? { matches: false, media: q, addEventListener: function () {}, removeEventListener: function () {} } : originalMatchMedia(q); };
  }
  if (!spec.layer) {
    var originalGetComputedStyle = window.getComputedStyle.bind(window);
    window.getComputedStyle = function (el, ps) {
      var real = originalGetComputedStyle(el, ps);
      if (!el || el !== document.documentElement) return real;
      return new Proxy(real, {
        get: function (t, k) {
          if (k === "getPropertyValue") return function (p) { return p === "--dbx-layer-probe" ? "" : t.getPropertyValue(p); };
          var v = t[k];
          return typeof v === "function" ? v.bind(t) : v;
        },
      });
    };
  }
  document.documentElement.setAttribute("data-dbx-emulated-engine", "${mode}");
})();
</script>`;
}

function inject(html, mode) {
  const stub = stubScript(mode);
  if (!stub) return html;
  return html.replace(/<head>/i, "<head>\n" + stub);
}

const server = createServer(async (req, res) => {
  const url = new URL(req.url, "http://localhost");
  const pathname = decodeURIComponent(url.pathname);

  if (pathname.startsWith("/api/")) {
    console.log(req.method, pathname);
    const MOCKS = {
      "/api/auth/check": { required: false, authenticated: true, setup_required: false },
      "/api/migration/status": { migrationId: "preview", state: "succeeded", needsMigration: false, keyProviderAvailable: true, keyStatus: "present", databasePlaintextCount: 0, connectionCount: 0, pluginSecretCount: 0, aiSecretCount: 0, tunnelSecretCount: 0, syncCredentialCount: 0, legacyJsonFiles: [], backupRequired: false },
      "/api/version": { version: "0.0.0-preview" },
      "/api/connection/list": [],
      "/api/tunnel-profiles/list": [],
      "/api/plugins": [],
      "/api/ai/configs": [],
      "/api/ai/provider-configs": [],
      "/api/ssh/prompts": [],
      "/api/ssh/prompts/pending": [],
      "/api/layout/table-vgroups": [],
      "/api/app-settings/sql-file-upload-max-bytes": 268435456,
    };
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify(MOCKS[pathname] ?? {}));
    return;
  }

  let filePath = join(ROOT, normalize(pathname).replace(/^(\.\.[/\\])+/, ""));
  try {
    const info = await stat(filePath);
    if (info.isDirectory()) filePath = join(filePath, "index.html");
  } catch {
    filePath = join(ROOT, "index.html");
  }

  let body;
  try {
    body = await readFile(filePath);
  } catch {
    res.writeHead(404); res.end("not found"); return;
  }

  const ext = extname(filePath);
  const type = MIME[ext] || "application/octet-stream";
  const headers = { "content-type": type, "cache-control": "no-store" };
  if (ext === ".css" && EMULATE) {
    body = Buffer.from(stripLayers(body.toString("utf8")), "utf8");
    headers["x-dbx-layer-stripped"] = "1";
  }
  if (ext === ".html") {
    const mode = url.searchParams.get("dbx-emulate");
    if (mode) body = Buffer.from(inject(body.toString("utf8"), mode), "utf8");
  }
  headers["content-length"] = String(body.length);
  res.writeHead(200, headers);
  res.end(body);
});

server.listen(PORT, () => console.log(`dbx-preview ${EMULATE ? "legacy-emulation" : "normal"} on http://localhost:${PORT}`));
