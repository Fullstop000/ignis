// Spawns `ignis --engine` and exposes the NDJSON protocol as callbacks.
//
// stdio: ['pipe','pipe','inherit'] — the child's stdin/stdout are the protocol
// pipes; its stderr inherits ours so engine logs land on our stderr. The Ink
// app keeps the real process.stdin/stdout (the TTY) for render + keys. This is
// topology (ii): the frontend owns the terminal and hosts the headless core.
import { spawn } from 'node:child_process';
import readline from 'node:readline';

export function spawnEngine({ bin = process.env.IGNIS_ENGINE_BIN || 'ignis', args = [] } = {}) {
  const child = spawn(bin, ['--engine', ...args], { stdio: ['pipe', 'pipe', 'inherit'] });
  const rl = readline.createInterface({ input: child.stdout });
  const frameHandlers = [];
  const closeHandlers = [];
  const pending = []; // frames that arrive before onFrame is registered (e.g. the startup snapshot)

  rl.on('line', (line) => {
    const t = line.trim();
    if (!t) return; // skip blanks/keepalives
    let frame;
    try {
      frame = JSON.parse(t);
    } catch {
      return; // ignore malformed lines rather than crash the UI
    }
    if (frameHandlers.length === 0) {
      pending.push(frame); // buffer until the UI subscribes
      return;
    }
    for (const h of frameHandlers) h(frame);
  });
  child.on('close', (code) => {
    for (const h of closeHandlers) h(code);
  });
  child.on('error', (err) => {
    process.stderr.write(`ignis-tui: failed to spawn engine (${bin} --engine): ${err.message}\n`);
    for (const h of closeHandlers) h(1);
  });

  return {
    onFrame: (cb) => {
      frameHandlers.push(cb);
      if (pending.length) pending.splice(0).forEach(cb); // flush buffered frames
    },
    onClose: (cb) => closeHandlers.push(cb),
    send: (cmd) => {
      if (child.stdin.writable) child.stdin.write(JSON.stringify(cmd) + '\n');
    },
    close: () => {
      try {
        child.stdin.end();
      } catch {
        /* already closed */
      }
    },
  };
}
