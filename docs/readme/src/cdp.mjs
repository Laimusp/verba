/**
 * A headless Chrome over the DevTools protocol — just what the README pictures need: open a page, set its size,
 * run a script in it, take a screenshot. Chrome comes from env CHROME or the usual install path.
 */
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const CHROME =
  process.env.CHROME ||
  [
    'C:/Program Files/Google/Chrome/Application/chrome.exe',
    'C:/Program Files (x86)/Google/Chrome/Application/chrome.exe',
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    '/usr/bin/google-chrome',
    '/usr/bin/chromium',
  ].find((p) => fs.existsSync(p));

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export async function launch(extraArgs = []) {
  if (!CHROME) throw new Error('Chrome не найден: укажи путь в переменной CHROME');
  const port = 9400 + Math.floor(Math.random() * 400);
  const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'qed-chrome-'));
  const proc = spawn(
    CHROME,
    [
      '--headless=new', `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, '--hide-scrollbars',
      '--no-first-run', '--no-default-browser-check', '--force-color-profile=srgb', '--font-render-hinting=none',
      ...extraArgs, 'about:blank',
    ],
    { stdio: 'ignore', windowsHide: true },
  );
  for (let i = 0; ; i++) {
    try {
      await (await fetch(`http://127.0.0.1:${port}/json/version`)).json();
      break;
    } catch {
      if (i > 100) throw new Error('Chrome не ответил за 10 с');
      await sleep(100);
    }
  }

  async function page({ width = 1280, height = 720, dpr = 1, mobile = false, transparent = false } = {}) {
    const target = await (await fetch(`http://127.0.0.1:${port}/json/new?about:blank`, { method: 'PUT' })).json();
    const ws = new WebSocket(target.webSocketDebuggerUrl);
    await new Promise((r, j) => { ws.onopen = r; ws.onerror = j; });
    let id = 0;
    const pending = new Map();
    const listeners = [];
    ws.onmessage = (e) => {
      const m = JSON.parse(e.data);
      if (m.id && pending.has(m.id)) {
        const { r, j } = pending.get(m.id);
        pending.delete(m.id);
        if (m.error) j(new Error(JSON.stringify(m.error)));
        else r(m.result);
      } else if (m.method) listeners.forEach((f) => f(m));
    };
    const send = (method, params = {}) =>
      new Promise((r, j) => {
        const i = ++id;
        pending.set(i, { r, j });
        ws.send(JSON.stringify({ id: i, method, params }));
      });
    await send('Page.enable');
    await send('Emulation.setDeviceMetricsOverride', { width, height, deviceScaleFactor: dpr, mobile });
    if (mobile) await send('Emulation.setTouchEmulationEnabled', { enabled: true, maxTouchPoints: 5 });
    if (transparent) await send('Emulation.setDefaultBackgroundColorOverride', { color: { r: 0, g: 0, b: 0, a: 0 } });
    return {
      send,
      async goto(url) {
        const loaded = new Promise((r) => listeners.push((m) => m.method === 'Page.loadEventFired' && r()));
        await send('Page.navigate', { url });
        await loaded;
      },
      async eval(expression) {
        const r = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
        if (r.exceptionDetails) throw new Error(JSON.stringify(r.exceptionDetails).slice(0, 500));
        return r.result.value;
      },
      /** PNG or WebP (format by the file's extension) of the viewport or a clip in CSS px */
      async shot(file, clip, quality = 90) {
        const format = file.endsWith('.webp') ? 'webp' : 'png';
        const r = await send('Page.captureScreenshot', {
          format, ...(format === 'webp' ? { quality } : {}), ...(clip ? { clip: { ...clip, scale: 1 } } : {}),
        });
        fs.writeFileSync(file, Buffer.from(r.data, 'base64'));
      },
      async close() {
        try { await fetch(`http://127.0.0.1:${port}/json/close/${target.id}`); } catch {}
        ws.close();
      },
    };
  }

  async function close() {
    proc.kill();
    await sleep(1000); // Chrome lets go of its profile a moment after it exits
    try { fs.rmSync(profile, { recursive: true, force: true }); } catch {}
  }
  return { page, close };
}
