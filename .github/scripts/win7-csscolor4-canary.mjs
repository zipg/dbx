import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { mkdir, writeFile } from "node:fs/promises";
import { resolve, join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";

const [exeInput, runtimeInput, outputInput] = process.argv.slice(2);
if (!exeInput || !runtimeInput || !outputInput) throw new Error("Usage: node win7-csscolor4-canary.mjs <dbx.exe> <runtime> <output>");
const exe = resolve(exeInput),
  runtime = resolve(runtimeInput),
  output = resolve(outputInput);
await mkdir(output, { recursive: true });

async function connect(url) {
  const socket = new WebSocket(url);
  const pending = new Map();
  let nextId = 0;
  socket.addEventListener("message", ({ data }) => {
    const message = JSON.parse(data);
    const request = pending.get(message.id);
    if (!request) return;
    pending.delete(message.id);
    clearTimeout(request.timer);
    if (message.error) request.reject(new Error(JSON.stringify(message.error)));
    else request.resolve(message.result);
  });
  await new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      socket.close();
      reject(new Error("CDP connection timed out"));
    }, 10000);
    socket.addEventListener(
      "open",
      () => {
        clearTimeout(timeout);
        resolve();
      },
      { once: true },
    );
    socket.addEventListener(
      "error",
      (error) => {
        clearTimeout(timeout);
        reject(error);
      },
      { once: true },
    );
  });
  return {
    send(method, params = {}) {
      return new Promise((resolve, reject) => {
        const id = ++nextId;
        const timer = setTimeout(() => {
          pending.delete(id);
          reject(new Error(`CDP timeout: ${method}`));
        }, 15000);
        pending.set(id, { resolve, reject, timer });
        socket.send(JSON.stringify({ id, method, params }));
      });
    },
    close() {
      socket.close();
      for (const p of pending.values()) {
        clearTimeout(p.timer);
        p.reject(new Error("CDP closed"));
      }
      pending.clear();
    },
  };
}

async function run(disabled, port) {
  const mode = disabled ? "off" : "on";
  const env = {
    ...process.env,
    WEBVIEW2_BROWSER_EXECUTABLE_FOLDER: runtime,
    WEBVIEW2_USER_DATA_FOLDER: join(output, `profile-${mode}`),
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${port}`,
    DBX_STARTUP_LOG_DIR: join(output, `startup-${mode}`),
    DBX_KEEP_STARTUP_LOG: "1",
    DBX_DISABLE_ENTERPRISE_COMPAT: "1",
    DBX_DISABLE_WIN7_CSS_COLOR4: disabled ? "1" : "0",
  };
  let stderr = "";
  console.log(`Starting CSSColor4 ${mode}`);
  const child = spawn(exe, [], { env, stdio: ["ignore", "ignore", "pipe"] });
  child.stderr.on("data", (chunk) => {
    stderr += chunk;
  });
  let failure;
  child.on("error", (error) => {
    failure = error;
  });
  let cdp;
  try {
    const deadline = Date.now() + 60000;
    let target;
    while (Date.now() < deadline) {
      if (failure) throw failure;
      if (child.exitCode !== null) throw new Error(`DBX exited: ${child.exitCode}`);
      try {
        const response = await fetch(`http://127.0.0.1:${port}/json/list`, { signal: AbortSignal.timeout(2000) });
        const targets = await response.json();
        target = targets.find((t) => t.type === "page" && /tauri\.localhost|localhost/.test(t.url) && t.webSocketDebuggerUrl);
        if (target) break;
      } catch {
        /* WebView2 may still be starting. */
      }
      await delay(250);
    }
    assert.ok(target, "No DBX page appeared in WebView2 CDP");
    cdp = await connect(target.webSocketDebuggerUrl);
    const version = await cdp.send("Browser.getVersion");
    const evaluated = await cdp.send("Runtime.evaluate", {
      returnByValue: true,
      expression: `JSON.stringify({userAgent:navigator.userAgent,oklch:CSS.supports('color','oklch(0.55 0.2 260)'),mix:CSS.supports('color','color-mix(in srgb,red 50%,blue)'),mixOklab:CSS.supports('color','color-mix(in oklab,red 50%,blue)')})`,
    });
    assert.ok(!evaluated.exceptionDetails, JSON.stringify(evaluated.exceptionDetails));
    const capabilities = JSON.parse(evaluated.result.value);
    const result = { mode, runtimeVersion: version.product, capabilities, targetUrl: target.url, scope: "WebView2 109 on Windows Actions runner; not a Win7 OS test" };
    await writeFile(join(output, `result-${mode}.json`), JSON.stringify(result, null, 2));
    try {
      await cdp.send("Page.enable");
      const { frameTree } = await cdp.send("Page.getFrameTree");
      const html = `<!doctype html><meta charset="utf-8"><title>WebView2 109 color probe</title>
<style>
body{margin:0;padding:40px;font:16px/1.5 Segoe UI,Arial,sans-serif;color:#182135;background:#f5f6f8}
h1{margin:0 0 12px;font-size:28px}p{margin:8px 0}code{font-size:14px}
.grid{display:grid;grid-template-columns:repeat(3,1fr);gap:20px;margin:30px 0}
.card{padding:18px;border:1px solid #c9cdd4;border-radius:12px;background:white}
.swatch{height:135px;border-radius:8px;margin:16px 0;background:rgb(230,232,236)}
.note{padding:16px;background:#fff;border:1px solid #ccd2dc;border-radius:8px}
</style><h1>WebView2 109 | CSSColor4 ${mode.toUpperCase()}</h1>
<p>Diagnostic samples rendered by the installed DBX WebView2.</p>
<p>Runtime: ${version.product} | Windows Server 2022 CI (not Windows 7)</p>
<div class="grid">
<div class="card"><b>OKLCH color</b><div id="oklch" class="swatch" style="background:oklch(0.62 0.22 260)"></div><code>oklch(0.62 0.22 260)</code></div>
<div class="card"><b>sRGB color mix</b><div id="mix" class="swatch" style="background:color-mix(in srgb,red 50%,blue)"></div><code>color-mix(in srgb, red 50%, blue)</code></div>
<div class="card"><b>Oklab color mix</b><div id="mixOklab" class="swatch" style="background:color-mix(in oklab,red 50%,blue)"></div><code>color-mix(in oklab, red 50%, blue)</code></div>
</div><div class="note">CSS.supports: OKLCH=${capabilities.oklch}, sRGB mix=${capabilities.mix}, Oklab mix=${capabilities.mixOklab}<p>Unsupported declarations use a grey fallback. This is a capability probe, not a DBX interface screenshot.</p></div>`;
      await cdp.send("Page.setDocumentContent", { frameId: frameTree.frame.id, html });
      const samples = await cdp.send("Runtime.evaluate", {
        awaitPromise: true, returnByValue: true,
        expression: `new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(()=>resolve(Object.fromEntries(['oklch','mix','mixOklab'].map(id=>[id,getComputedStyle(document.getElementById(id)).backgroundColor]))))))`,
      });
      assert.ok(!samples.exceptionDetails, JSON.stringify(samples.exceptionDetails));
      result.computedColors = samples.result.value;
      for (const value of Object.values(result.computedColors)) {
        if (disabled) assert.equal(value, "rgb(230, 232, 236)");
        else assert.notEqual(value, "rgb(230, 232, 236)");
      }
      await writeFile(join(output, `result-${mode}.json`), JSON.stringify(result, null, 2));
      const screenshot = await cdp.send("Page.captureScreenshot", { format: "png" });
      await writeFile(join(output, `dbx-${mode}.png`), Buffer.from(screenshot.data, "base64"));
    } catch (error) {
      throw new Error(`Visual probe ${mode}: ${error.message}`);
    }
    console.log(JSON.stringify(result));
    return result;
  } finally {
    cdp?.close();
    if (child.pid && child.exitCode === null) {
      try {
        execFileSync("taskkill.exe", ["/PID", String(child.pid), "/T", "/F"], { stdio: "ignore" });
      } catch {
        /* Already exited. */
      }
    }
    await writeFile(join(output, `stderr-${mode}.log`), stderr);
    await delay(1500);
  }
}

try {
  const off = await run(true, 19227);
  const on = await run(false, 19228);
  assert.match(on.capabilities.userAgent, /Chrome\/109\./, "Canary must use the bundled 109 engine");
  assert.equal(off.capabilities.oklch, false, "Off control should reject OKLCH");
  assert.equal(off.capabilities.mix, false, "Off control should reject color-mix");
  assert.equal(on.capabilities.oklch, true, "CSSColor4 should enable OKLCH");
  assert.equal(on.capabilities.mix, true, "CSSColor4 should enable color-mix");
  assert.equal(on.capabilities.mixOklab, true, "CSSColor4 should enable OKLab color mixing");
} catch (error) {
  await writeFile(join(output, "failure.txt"), error.stack || String(error));
  throw error;
}
