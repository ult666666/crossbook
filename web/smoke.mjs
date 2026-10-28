// End-to-end smoke test for the playground. It serves this directory on a free port,
// drives the page in headless Chromium and checks the main flows. It exits with status 1
// on the first failure, on any console error or page error, and on any request that
// leaves the local server.
//
//   npm ci && npx playwright install --with-deps chromium && npm run smoke
//
// The WebAssembly package must exist in ./pkg first; if it is missing, the test prints the
// command that builds it.

import { createServer } from 'node:http';
import { access, readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { extname, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

// Loaded through require so a globally installed Playwright also works (NODE_PATH).
const require = createRequire(import.meta.url);
const { chromium } = require('playwright');

const ROOT = fileURLToPath(new URL('.', import.meta.url));
const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.wasm': 'application/wasm',
  '.json': 'application/json; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.ico': 'image/x-icon',
  '.txt': 'text/plain; charset=utf-8',
};

function startServer() {
  const server = createServer(async (request, response) => {
    try {
      let path = decodeURIComponent(new URL(request.url, 'http://localhost').pathname);
      if (path.endsWith('/')) path += 'index.html';
      const file = resolve(ROOT, `.${path}`);
      if (!file.startsWith(ROOT.endsWith(sep) ? ROOT : ROOT + sep)) {
        response.writeHead(403).end();
        return;
      }
      const body = await readFile(file);
      response.writeHead(200, {
        'content-type': MIME[extname(file)] ?? 'application/octet-stream',
        'cache-control': 'no-store',
      });
      response.end(body);
    } catch {
      response.writeHead(404, { 'content-type': 'text/plain; charset=utf-8' }).end('Not found');
    }
  });
  return new Promise((resolveServer, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => resolveServer(server));
  });
}

class SmokeFailure extends Error {}

function check(condition, message) {
  if (!condition) throw new SmokeFailure(message);
}

async function main() {
  try {
    await access(resolve(ROOT, 'pkg/crossbook_wasm_bg.wasm'));
  } catch {
    throw new SmokeFailure(
      'web/pkg is missing. Build it first: cargo build --release --target wasm32-unknown-unknown -p crossbook-wasm ' +
        '&& wasm-bindgen --target web --out-dir web/pkg --no-typescript target/wasm32-unknown-unknown/release/crossbook_wasm.wasm',
    );
  }

  const server = await startServer();
  const origin = `http://127.0.0.1:${server.address().port}`;
  const problems = [];
  const browser = await chromium.launch();

  const step = async (name, run) => {
    await run();
    check(problems.length === 0, `${name}: ${problems.join('\n')}`);
    console.log(`ok   ${name}`);
  };

  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    page.on('console', (message) => {
      if (message.type() === 'error') problems.push(`console error: ${message.text()}`);
    });
    page.on('pageerror', (error) => problems.push(`page error: ${error.message}`));
    page.on('request', (request) => {
      const url = request.url();
      if (!url.startsWith(`${origin}/`) && !url.startsWith('data:')) problems.push(`external request: ${url}`);
    });

    await step('page loads and the engine starts', async () => {
      await page.goto(`${origin}/`, { waitUntil: 'load' });
      await page.waitForSelector('html[data-engine="ready"]', { timeout: 15_000 });
    });

    await step('priority scenario prints two trades', async () => {
      await page.click('#scenario-list button[data-id="priority"]');
      await page.click('#sc-action');
      await page.waitForFunction(
        () => document.querySelectorAll('#log-list > li:first-child .event[data-type="trade"]').length === 2,
        null,
        { timeout: 5_000 },
      );
      const title = (await page.textContent('#log-list > li:first-child .group-title')).replace(/\s+/g, ' ').trim();
      check(title === 'Ben buys 8 @ 101 (GTC)', `unexpected command header: "${title}"`);
      await page.locator('#sc-explain').waitFor({ state: 'visible' });
      await page.waitForFunction(() => /2 trades for 8 lots/.test(document.getElementById('announcer').textContent));
    });

    let orderId = '';
    await step('ticket places an order that rests on the book', async () => {
      await page.click('[data-field="side"] button[data-value="sell"]');
      await page.fill('#price', '105');
      await page.fill('#qty', '3');
      await page.click('[data-field="account"] button[data-value="3"]');
      const label = (await page.textContent('#submit')).replace(/\s+/g, ' ').trim();
      check(label === 'Sell 3 @ 105', `unexpected submit label: "${label}"`);
      await page.click('#submit');
      orderId = (await page.textContent('#log-list > li:first-child .group-meta')).replace('#', '').trim();
      check(/^\d+$/.test(orderId), `no order id in the log header: "${orderId}"`);
      await page.locator(`.chip[data-id="${orderId}"]`).waitFor({ timeout: 5_000 });
      const booked = await page.locator('#log-list > li:first-child .event[data-type="booked"]').count();
      check(booked === 1, 'the new order did not rest on the book');
    });

    await step('order chip selects the order and cancels it', async () => {
      await page.click(`.chip[data-id="${orderId}"]`);
      check(
        (await page.getAttribute(`.chip[data-id="${orderId}"]`, 'aria-pressed')) === 'true',
        'the chip is not marked as selected',
      );
      await page.click('#cancel-btn');
      await page.locator(`.chip[data-id="${orderId}"]`).waitFor({ state: 'detached', timeout: 5_000 });
      const event = page.locator('#log-list > li:first-child .event[data-type="canceled"]');
      check((await event.count()) === 1, 'no CANCELED event for the canceled order');
      const text = await event.textContent();
      check(text.includes(`#${orderId}`) && text.includes('canceled on request'), `unexpected cancel text: "${text}"`);
    });

    await step('100,000-command stress test matches the native build', async () => {
      await page.click('[data-field="count"] button[data-value="100000"]');
      await page.fill('#seed', '42');
      await page.click('#stress-run');
      const verdict = page.locator('#stress-verdict');
      await verdict.waitFor({ timeout: 60_000 });
      const text = (await verdict.textContent()).trim();
      check(text === 'Matches the native x86-64 build', `unexpected stress verdict: "${text}"`);
      check((await verdict.getAttribute('data-match')) === 'true', 'stress verdict is not a match');
    });

    await step('no horizontal overflow at 390px', async () => {
      await page.setViewportSize({ width: 390, height: 844 });
      await page.waitForFunction(() => window.innerWidth === 390);
      const width = await page.evaluate(() => ({
        scroll: Math.max(document.documentElement.scrollWidth, document.body.scrollWidth),
        client: document.documentElement.clientWidth,
      }));
      check(width.scroll <= width.client, `page is ${width.scroll}px wide at a ${width.client}px viewport`);
    });
  } finally {
    await browser.close();
    server.close();
  }
}

main().then(
  () => {
    console.log('smoke test passed');
  },
  (error) => {
    console.error(`FAIL ${error instanceof SmokeFailure ? error.message : (error.stack ?? error)}`);
    process.exitCode = 1;
  },
);
