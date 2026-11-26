/**
 * Emulated Xbox Controller & Edge Virtual-Cursor Test Harness for NulKast MediaServer.
 * Runs in real headless Chromium at 1920x1080 against web/ with a lightweight mock API.
 *
 * Tests all 3 Xbox input modes:
 *   1. Xbox Gamepad API (navigator.getGamepads() with A/B/X/LB/RB, D-pad, Left Stick)
 *      - Including late-connected gamepad WITHOUT gamepadconnected event (Xbox Edge quirk)
 *   2. Xbox WebView2 / Keyboard D-pad (Arrow*, Enter, Escape, MediaPlayPause)
 *   3. Xbox Edge Virtual Mouse Cursor (mousemove wake, hover persistence on HUD, click video)
 */

import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { chromium } = require('playwright');

const WEB_DIR = path.resolve(process.cwd(), 'web');

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'application/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.json': 'application/json',
};

function startMockServer() {
  const server = http.createServer((req, res) => {
    const urlPath = (req.url || '/').split('?')[0];

    if (urlPath === '/api/health') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ status: 'ok', hw_accel: true, active_sessions: 0 }));
      return;
    }
    if (urlPath === '/api/movies') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({
        items: [
          {
            id: 101,
            kind: 'movie',
            title: 'Test Sci-Fi Feature',
            year: 2026,
            genre: 'Sci-Fi',
            duration_secs: 5400,
            position_secs: 600,
            watched: false,
            subtitles_json: '[{"index":2,"language":"eng","title":"English","is_text":true}]',
          },
        ],
      }));
      return;
    }
    if (urlPath === '/api/shows' || urlPath === '/api/continue') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify(urlPath === '/api/shows' ? { shows: [] } : { items: [] }));
      return;
    }
    if (urlPath === '/api/discover/status') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ enabled: false }));
      return;
    }
    if (urlPath === '/api/diagnostics') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ enabled: false, events: [] }));
      return;
    }
    if (urlPath.startsWith('/api/stream/') && urlPath.endsWith('/start')) {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({
        session_id: 'mock-session-1',
        mode: 'direct',
        hw_accel: true,
        stream_url: '/mock-video.mp4',
      }));
      return;
    }
    if (urlPath.startsWith('/api/progress') || urlPath.startsWith('/api/client-log') || urlPath.startsWith('/api/hls/')) {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ ok: true }));
      return;
    }
    if (urlPath === '/mock-video.mp4') {
      res.writeHead(200, { 'Content-Type': 'video/mp4' });
      res.end(Buffer.alloc(0));
      return;
    }

    let rel = urlPath === '/' ? 'index.html' : urlPath.replace(/^\/web\//, '').replace(/^\//, '');
    const full = path.join(WEB_DIR, rel);
    if (fs.existsSync(full) && fs.statSync(full).isFile()) {
      const ext = path.extname(full);
      res.writeHead(200, { 'Content-Type': MIME[ext] || 'application/octet-stream' });
      fs.createReadStream(full).pipe(res);
      return;
    }
    res.writeHead(404);
    res.end('Not found');
  });

  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => {
      const addr = server.address();
      resolve({ server, url: `http://127.0.0.1:${addr.port}` });
    });
  });
}

async function run() {
  const { server, url } = await startMockServer();
  const browser = await chromium.launch({ headless: true });
  let passed = 0;
  const check = (name, cond, detail = '') => {
    if (!cond) throw new Error(`FAIL: ${name}${detail ? ' — ' + detail : ''}`);
    passed += 1;
    console.log(`  ✓ ${name}`);
  };

  try {
    const context = await browser.newContext({ viewport: { width: 1920, height: 1080 } });
    const page = await context.newPage();

    // Inject virtual Xbox controller & deterministic HTMLMediaElement playback mock
    await page.addInitScript(() => {
      window.__xboxPad = {
        connected: false,
        id: 'Xbox Wireless Controller (STANDARD GAMEPAD)',
        index: 0,
        mapping: 'standard',
        axes: [0, 0, 0, 0],
        buttons: Array.from({ length: 17 }, () => ({ pressed: false, touched: false, value: 0 })),
      };
      navigator.getGamepads = () => (window.__xboxPad.connected ? [window.__xboxPad, null, null, null] : [null, null, null, null]);

      // Mock <video> play/pause/currentTime so headless Chromium simulates real active playback at t=600s
      let _paused = true;
      let _time = 600;
      let _duration = 5400;
      Object.defineProperty(HTMLMediaElement.prototype, 'paused', { get: () => _paused, configurable: true });
      Object.defineProperty(HTMLMediaElement.prototype, 'duration', { get: () => _duration, configurable: true });
      Object.defineProperty(HTMLMediaElement.prototype, 'readyState', { get: () => 4, configurable: true });
      Object.defineProperty(HTMLMediaElement.prototype, 'currentTime', {
        get: () => _time,
        set: (v) => { _time = Number(v) || 0; },
        configurable: true,
      });
      HTMLMediaElement.prototype.play = function () {
        _paused = false;
        this.dispatchEvent(new Event('playing'));
        return Promise.resolve();
      };
      HTMLMediaElement.prototype.pause = function () {
        _paused = true;
        this.dispatchEvent(new Event('pause'));
      };
      HTMLMediaElement.prototype.load = function () {};
    });

    await page.goto(url, { waitUntil: 'networkidle' });
    await page.waitForSelector('.card');

    // Open movie player
    console.log('1. Opening movie player...');
    await page.click('.card');
    await page.waitForSelector('#player-overlay:not(.hidden)');
    await page.waitForTimeout(150);

    const initialState = await page.evaluate(() => ({
      paused: document.getElementById('video-el').paused,
      time: document.getElementById('video-el').currentTime,
      spinnerPE: getComputedStyle(document.getElementById('player-spinner')).pointerEvents,
    }));
    check('Video starts playing at resumed timestamp (600s)', !initialState.paused && initialState.time >= 600);
    check('Buffering spinner has pointer-events: none (never blocks HUD)', initialState.spinnerPE === 'none');

    // Helper: force idle-hide as if 5.5s passed and drop focus to <body> (exact Xbox Edge playback state)
    const simulateIdlePlaying = async () => {
      await page.evaluate(() => {
        const ov = document.getElementById('player-overlay');
        ov.classList.add('idle-hide');
        document.activeElement?.blur?.();
        document.body.focus();
      });
    };

    // --- TEST SUITE A: Xbox Gamepad API (including late-connect WITHOUT gamepadconnected event!) ---
    console.log('2. Testing Xbox Gamepad API (late connect, no gamepadconnected event)...');
    await simulateIdlePlaying();
    await page.evaluate(() => { window.__xboxPad.connected = true; });
    await page.waitForTimeout(1150); // wait for 1s fallback poller to detect pad

    // Press A (button 0) while HUD is hidden and focus is on <body>
    await page.evaluate(() => { window.__xboxPad.buttons[0].pressed = true; });
    await page.waitForTimeout(60);
    await page.evaluate(() => { window.__xboxPad.buttons[0].pressed = false; });
    await page.waitForTimeout(60);

    const afterPadA = await page.evaluate(() => ({
      paused: document.getElementById('video-el').paused,
      time: document.getElementById('video-el').currentTime,
      idle: document.getElementById('player-overlay').classList.contains('idle-hide'),
      activeId: document.activeElement?.id,
    }));
    check('Gamepad A while playing + idle-hide pauses video', afterPadA.paused === true);
    check('Gamepad A does NOT jump back to 0:00', afterPadA.time >= 600, `time=${afterPadA.time}`);
    check('Gamepad A wakes HUD and focuses btn-play-pause', !afterPadA.idle && afterPadA.activeId === 'btn-play-pause', JSON.stringify(afterPadA));

    // Press A again to resume
    await page.evaluate(() => { window.__xboxPad.buttons[0].pressed = true; });
    await page.waitForTimeout(60);
    await page.evaluate(() => { window.__xboxPad.buttons[0].pressed = false; });
    await page.waitForTimeout(60);
    check('Gamepad A again resumes playback', await page.evaluate(() => !document.getElementById('video-el').paused));

    // Simulate idle-hide again, push Left Stick Down (axes[1] = 0.9)
    await simulateIdlePlaying();
    await page.evaluate(() => { window.__xboxPad.axes[1] = 0.9; });
    await page.waitForTimeout(220);
    await page.evaluate(() => { window.__xboxPad.axes[1] = 0; });
    await page.waitForTimeout(60);

    const afterStickDown = await page.evaluate(() => ({
      idle: document.getElementById('player-overlay').classList.contains('idle-hide'),
      activeId: document.activeElement?.id,
      paused: document.getElementById('video-el').paused,
    }));
    check('Left Stick Down while idle wakes HUD & focuses btn-play-pause', !afterStickDown.idle && afterStickDown.activeId === 'btn-play-pause', JSON.stringify(afterStickDown));

    // Disconnect virtual pad for keyboard tests
    await page.evaluate(() => { window.__xboxPad.connected = false; });

    // --- TEST SUITE B: Xbox WebView2 / Keyboard D-pad & Enter ---
    console.log('3. Testing Keyboard / WebView2 D-pad + Enter...');
    await simulateIdlePlaying();
    await page.keyboard.press('Enter');
    await page.waitForTimeout(60);
    const afterEnter = await page.evaluate(() => ({
      paused: document.getElementById('video-el').paused,
      time: document.getElementById('video-el').currentTime,
      idle: document.getElementById('player-overlay').classList.contains('idle-hide'),
      activeId: document.activeElement?.id,
    }));
    check('Keyboard Enter while idle + loose focus pauses video', afterEnter.paused === true);
    check('Keyboard Enter does NOT restart movie', afterEnter.time >= 600);
    check('Keyboard Enter wakes HUD & focuses btn-play-pause', !afterEnter.idle && afterEnter.activeId === 'btn-play-pause');

    // Resume, idle-hide, press ArrowDown
    await page.keyboard.press('Enter');
    await page.waitForTimeout(50);
    await simulateIdlePlaying();
    await page.keyboard.press('ArrowDown');
    await page.waitForTimeout(50);
    const afterKeyDown = await page.evaluate(() => ({
      idle: document.getElementById('player-overlay').classList.contains('idle-hide'),
      activeId: document.activeElement?.id,
    }));
    check('Keyboard ArrowDown while idle wakes HUD & focuses btn-play-pause', !afterKeyDown.idle && afterKeyDown.activeId === 'btn-play-pause');

    // --- TEST SUITE C: Xbox Edge Virtual Mouse Cursor Mode ---
    console.log('4. Testing Xbox Edge Virtual Cursor Mode (hover persistence + click)...');
    await simulateIdlePlaying();
    // Wiggle cursor -> wakes HUD
    await page.mouse.move(960, 540);
    await page.mouse.move(980, 550);
    await page.waitForTimeout(60);
    check('Mouse/stick wiggle wakes HUD', await page.evaluate(() => !document.getElementById('player-overlay').classList.contains('idle-hide')));

    // Hover onto bottom HUD bar and wait 6 seconds (longer than 5.5s idle timeout!)
    const btnBox = await page.locator('#btn-play-pause').boundingBox();
    assert(btnBox, 'btn-play-pause bounding box exists');
    await page.mouse.move(btnBox.x + btnBox.width / 2, btnBox.y + btnBox.height / 2);
    await page.waitForTimeout(6000);
    const stillVisibleOnHover = await page.evaluate(() => !document.getElementById('player-overlay').classList.contains('idle-hide'));
    check('HUD stays open >6s while cursor hovers bottom controls', stillVisibleOnHover);

    // Click btn-play-pause
    await page.mouse.click(btnBox.x + btnBox.width / 2, btnBox.y + btnBox.height / 2);
    await page.waitForTimeout(60);
    check('Clicking Pause button pauses video', await page.evaluate(() => document.getElementById('video-el').paused === true));

    // Click video area -> resumes
    await page.mouse.click(960, 400);
    await page.waitForTimeout(60);
    check('Clicking video toggles playback back to playing', await page.evaluate(() => document.getElementById('video-el').paused === false));

    // Press B (Escape) -> closes player cleanly
    await page.keyboard.press('Escape');
    await page.waitForTimeout(100);
    check('Back/B closes player overlay cleanly', await page.evaluate(() => document.getElementById('player-overlay').classList.contains('hidden')));

    console.log(`\nALL ${passed} XBOX CONTROLLER HARNESS CHECKS PASSED!`);
  } finally {
    await browser.close();
    server.close();
  }
}

run().catch((err) => {
  console.error(err);
  process.exit(1);
});
