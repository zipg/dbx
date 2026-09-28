import { readFileSync } from "node:fs";
import vm from "node:vm";

const html = readFileSync(process.argv[2], "utf8");
const match = html.match(/<script data-dbx-compat-probe>([\s\S]*?)<\/script>/);
if (!match) throw new Error("probe script not found");
const source = match[1];

// Feature availability thresholds of the real engines (first version that ships the feature).
const THRESHOLDS = {
  chrome: { layer: 99, oklch: 111, colormix: 111, range: 104, has: 105, focus: 86 },
  edge: { layer: 99, oklch: 111, colormix: 111, range: 104, has: 105, focus: 86 },
  firefox: { layer: 97, oklch: 113, colormix: 113, range: 102, has: 121, focus: 85 },
  safari: { layer: 15.4, oklch: 15.4, colormix: 16.2, range: 16.4, has: 15.4, focus: 15.4 },
};

function capsFor(engine, version) {
  const t = THRESHOLDS[engine];
  const caps = {};
  for (const key of Object.keys(t)) caps[key] = version >= t[key];
  return caps;
}

function uaFor(engine, version) {
  const chromeLike = (extra) => `Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/${version}.0.0.0 Safari/537.36${extra}`;
  if (engine === "chrome") return chromeLike("");
  if (engine === "edge") return chromeLike(` Edg/${version}.0.0.0`);
  if (engine === "firefox") return `Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:${version}.0) Gecko/20100101 Firefox/${version}.0`;
  return `Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/${version} Safari/605.1.15`;
}

class FakeEl {
  constructor(tag) { this.tagName = tag; this.style = {}; this.children = []; this.attrs = {}; this.textContent = ""; this.parentNode = null; this.classList = { contains: () => false, add: () => {}, remove: () => {}, toggle: () => false }; this.dataset = {}; }
  appendChild(node) { node.parentNode = this; this.children.push(node); return node; }
  removeChild(node) { const i = this.children.indexOf(node); if (i >= 0) this.children.splice(i, 1); node.parentNode = null; return node; }
  remove() { const p = this.parentNode; if (p) { const i = p.children.indexOf(this); if (i >= 0) p.children.splice(i, 1); this.parentNode = null; } }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return k in this.attrs ? this.attrs[k] : null; }
  removeAttribute(k) { delete this.attrs[k]; }
  addEventListener() {}
  removeEventListener() {}
}

function run(engine, version, label) {
  const caps = capsFor(engine, version);
  const documentElement = new FakeEl("html");
  const head = new FakeEl("head");
  const body = new FakeEl("body");
  const document = {
    documentElement, head, body,
    createElement: (tag) => new FakeEl(tag),
    querySelector: () => null,
    getElementById: () => null,
    addEventListener: () => {},
  };
  const store = () => ({ getItem: () => null, setItem: () => {} });
  const window = {
    location: { search: "", reload: () => {} },
    sessionStorage: store(),
    localStorage: store(),
    addEventListener: () => {},
    dispatchEvent: () => {},
    matchMedia: (query) => ({ matches: caps.range && /[<>]=?/.test(query), media: query, addEventListener: () => {}, removeEventListener: () => {} }),
    getComputedStyle: (node) => ({ getPropertyValue: (p) => (p === "--dbx-layer-probe" && caps.layer ? "1" : "") }),
  };
  const CSS = {
    supports: (a, b) => {
      if (b === undefined) {
        const condition = String(a);
        if (condition.includes("selector(:has")) return caps.has;
        if (condition.includes("selector(:focus-visible")) return caps.focus;
        return true;
      }
      const value = String(b);
      if (value.includes("oklch")) return caps.oklch;
      if (value.includes("color-mix")) return caps.colormix;
      return true;
    },
  };
  const navigator = { userAgent: uaFor(engine, version), platform: "MacIntel", languages: ["zh-CN"], language: "zh-CN" };
  const context = vm.createContext({
    window, document, navigator, CSS, console, URLSearchParams, setTimeout, clearTimeout,
  });
  vm.runInContext(source, context, { filename: "probe.js" });
  const report = window.__DBX_WEBVIEW_COMPAT__;
  return { label: label ?? `${engine} ${version}`, tier: report.tier, engine: report.engine, missing: report.missing.join(", ") || "-" };
}

const cases = [
  ["chrome", 90], ["chrome", 98], ["chrome", 99], ["chrome", 104], ["chrome", 105], ["chrome", 110], ["chrome", 111], ["chrome", 120],
  ["edge", 98], ["edge", 104], ["edge", 111],
  ["firefox", 100], ["firefox", 112], ["firefox", 113], ["firefox", 121],
  ["safari", 14.1], ["safari", 15.2], ["safari", 15.4], ["safari", 16.1], ["safari", 16.2], ["safari", 16.3], ["safari", 16.4], ["safari", 17.6],
].map(([engine, version]) => run(engine, version));

const rows = [...cases];
const pad = (text, width) => String(text).padEnd(width);
console.log(pad("engine", 16) + pad("detected as", 16) + pad("tier", 10) + "missing probes");
for (const row of rows) {
  console.log(pad(row.label, 16) + pad(row.engine, 16) + pad(row.tier, 10) + row.missing);
}
