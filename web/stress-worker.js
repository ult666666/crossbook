// Runs one stress test off the main thread so the page stays responsive. The page creates
// a fresh worker per run and terminates it afterwards, which also releases the staged
// commands (about 140 MB for three million) in one step.

import init, { Playground } from './pkg/crossbook_wasm.js';

const ready = init();

self.onmessage = async ({ data }) => {
  const { count, seed } = data;
  try {
    await ready;
    const playground = new Playground();
    try {
      self.postMessage({ type: 'progress', phase: 'generate' });
      const g0 = performance.now();
      playground.stage(count, seed);
      const generationMs = performance.now() - g0;

      self.postMessage({ type: 'progress', phase: 'match' });
      const t0 = performance.now();
      const summary = JSON.parse(playground.runStaged());
      const engineMs = performance.now() - t0;

      self.postMessage({ type: 'done', generationMs, engineMs, summary });
    } finally {
      playground.free();
    }
  } catch (error) {
    self.postMessage({ type: 'error', message: error instanceof Error ? error.message : String(error) });
  }
};
