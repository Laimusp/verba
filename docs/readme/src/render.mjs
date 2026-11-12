#!/usr/bin/env node
/**
 * Renders the README video (intro.html) frame by frame in headless Chrome and encodes it with ffmpeg
 * (the same pipeline as the README video of QED):
 *   docs/readme/intro.mp4    1920×1080, 60 к/с, H.264 — the link «в полном качестве»
 *   docs/readme/intro.avif   1920×1080, 30 к/с, AV1 — what the README shows (GitHub serves .avif as image/avif)
 *
 *   node docs/readme/src/render.mjs                    # both
 *   node docs/readme/src/render.mjs --times 1,5.5,17   # only these moments → PNG in the temp folder (to check a layout)
 *
 * Needs Chrome (env CHROME or the usual path) and ffmpeg with libx264 and libsvtav1 in PATH.
 * The scene is a pure function of time (window.VERBA_INTRO.render(t)), so every frame is the same on every run;
 * its last frame is its first, so the AVIF loops with no seam.
 */
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { launch } from './cdp.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const outDir = path.resolve(here, '..');
const argv = process.argv.slice(2);
const arg = (name, dflt) => {
  const i = argv.indexOf(name);
  return i >= 0 && argv[i + 1] != null ? argv[i + 1] : dflt;
};
const FPS = Number(arg('--fps', 60));
const TIMES = arg('--times', null);

function ffmpeg(args) {
  const r = spawnSync('ffmpeg', ['-hide_banner', '-loglevel', 'error', '-y', ...args], {
    stdio: 'inherit', windowsHide: true, encoding: 'utf8',
  });
  if (r.status !== 0) throw new Error(`ffmpeg ${args.at(-1)}: код ${r.status}`);
}

const work = fs.mkdtempSync(path.join(os.tmpdir(), 'verba-intro-'));
const chrome = await launch();
let ok = false;
try {
  const page = await chrome.page({ width: 1280, height: 720, dpr: 1.5 });
  await page.goto(`${pathToFileURL(path.join(here, 'intro.html')).href}?render`);
  await page.eval(`window.VERBA_INTRO.ready.then(() => { document.body.style.display = 'block'; document.body.style.minHeight = '0'; return true; })`);
  const { duration, loop } = await page.eval('({ duration: window.VERBA_INTRO.duration, loop: window.VERBA_INTRO.loop })');
  const shot = async (t, file) => {
    await page.eval(`window.VERBA_INTRO.render(${t}); new Promise((r) => requestAnimationFrame(() => r(true)))`);
    await page.shot(file, { x: 0, y: 0, width: 1280, height: 720 });
  };

  if (TIMES) {
    for (const t of TIMES.split(',').map(Number)) {
      const file = path.join(work, `t-${t.toFixed(2)}.png`);
      await shot(t, file);
      console.log(file);
    }
  } else {
    const frames = path.join(work, 'frames');
    fs.mkdirSync(frames);
    const total = Math.round(duration * FPS);
    for (let i = 0; i < total; i++) {
      await shot(i / FPS, path.join(frames, `${String(i).padStart(5, '0')}.png`));
      if (i % FPS === 0) process.stderr.write(`кадр ${i} из ${total}\n`);
    }
    const input = ['-framerate', String(FPS), '-i', path.join(frames, '%05d.png')];
    ffmpeg([...input, '-c:v', 'libx264', '-preset', 'slow', '-crf', '17', '-tune', 'animation', '-pix_fmt', 'yuv420p',
      '-movflags', '+faststart', path.join(outDir, 'intro.mp4')]);
    ffmpeg([...input, '-vf', 'fps=30', '-frames:v', String(Math.round(loop * 30)), '-c:v', 'libsvtav1', '-crf', '30',
      '-preset', '5', '-pix_fmt', 'yuv420p', path.join(outDir, 'intro.avif')]);
    for (const f of ['intro.mp4', 'intro.avif']) {
      console.log(`${f}: ${(fs.statSync(path.join(outDir, f)).size / 1048576).toFixed(2)} МБ`);
    }
  }
  ok = true;
} finally {
  await chrome.close();
  if (!TIMES) {
    try { fs.rmSync(work, { recursive: true, force: true }); } catch {}
  }
}
process.exit(ok ? 0 : 1);
