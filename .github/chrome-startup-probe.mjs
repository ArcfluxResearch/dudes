import { readFile } from 'node:fs/promises';
import { ScratchRun } from '../xtask/src/smoke.mjs';

const text = (path) => readFile(path, 'utf8').catch((error) => error.code);

async function observe(pid, elapsed) {
  const [stat, wait, children, ...pressure] = await Promise.all([
    text(`/proc/${pid}/stat`),
    text(`/proc/${pid}/wchan`),
    text(`/proc/${pid}/task/${pid}/children`),
    ...['cpu', 'io', 'memory'].map((kind) => text(`/proc/pressure/${kind}`)),
  ]);
  const fields = stat.slice(stat.lastIndexOf(')') + 2).split(' ');
  const childIds = children.trim().split(/\s+/).filter((id) => /^\d+$/.test(id)).slice(0, 6);
  const names = await Promise.all(childIds.map(async (id) => ({
    pid: Number(id),
    name: (await text(`/proc/${id}/comm`)).trim().slice(0, 32),
  })));
  return {
    elapsed_ms: Math.round(elapsed),
    pid,
    state: fields[0],
    wait: wait.trim().slice(0, 64),
    minor_faults: Number(fields[7]),
    major_faults: Number(fields[9]),
    user_ticks: Number(fields[11]),
    system_ticks: Number(fields[12]),
    threads: Number(fields[17]),
    children: names,
    pressure: Object.fromEntries(['cpu', 'io', 'memory'].map((kind, index) => [
      kind, pressure[index].trim().slice(0, 192),
    ])),
  };
}

const startChrome = ScratchRun.prototype.startChrome;
ScratchRun.prototype.startChrome = async function () {
  const began = performance.now();
  const samples = [];
  let pending = Promise.resolve();
  let outcome = 'failed';
  const sample = () => {
    pending = pending.then(async () => {
      if (!this.chrome?.pid || samples.length === 12) return;
      samples.push(await observe(this.chrome.pid, performance.now() - began));
    }).catch((error) => {
      if (samples.length < 12) samples.push({ error: error.code ?? error.name });
    });
  };
  const running = startChrome.call(this);
  sample();
  const timer = setInterval(sample, 1000);
  try {
    const result = await running;
    outcome = 'ready';
    return result;
  } finally {
    clearInterval(timer);
    sample();
    await pending;
    console.error(`Chrome startup observations ${JSON.stringify({ outcome, samples })}`);
  }
};
