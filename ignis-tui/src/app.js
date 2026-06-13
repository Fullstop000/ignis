// The Ink UI. Written with `React.createElement` (aliased `e`) rather than JSX
// so it runs under plain `node` with no build/transform step — only `ink` +
// `react` need installing.
//
// REAL-BUT-MINIMAL (PR #174, phase 3b): this is an initial frontend, not full
// parity with the ratatui TUI. It renders the streaming transcript (user /
// assistant / tool / inject / notice blocks), a single-select `ask_user`
// picker, and a one-line composer; it sends submit / cancel / inject / reply.
// Not yet covered: multi-select & free-text ("Other") picker answers, markdown
// rendering, scrollback paging, /connect-style text-input prompts. The pure
// state/command logic lives in protocol.js (unit-tested via `node --test`).
import React, { useState, useEffect } from 'react';
import { Box, Text, useInput, useApp } from 'ink';
import {
  initialState,
  reduceOutbound,
  submit,
  cancel,
  inject,
  reply,
  answerSingle,
  answerCancelled,
} from './protocol.js';

const e = React.createElement;

export default function App({ engine }) {
  const { exit } = useApp();
  const [state, setState] = useState(initialState());
  const [input, setInput] = useState('');
  const [cursor, setCursor] = useState(0);

  useEffect(() => {
    engine.onFrame((frame) => setState((s) => reduceOutbound(s, frame)));
    engine.onClose(() => exit());
  }, [engine, exit]);

  const req = state.request;

  useInput((ch, key) => {
    // A picker captures all keys while open.
    if (req) {
      const opts = req.questions[0]?.options ?? [];
      if (key.upArrow) setCursor((c) => Math.max(0, c - 1));
      else if (key.downArrow) setCursor((c) => Math.min(Math.max(opts.length - 1, 0), c + 1));
      else if (key.escape) {
        engine.send(reply(req.id, answerCancelled()));
        setState((s) => ({ ...s, request: null }));
        setCursor(0);
      } else if (key.return) {
        const label = opts[cursor]?.label;
        if (label != null) {
          engine.send(reply(req.id, answerSingle(label)));
          setState((s) => ({ ...s, request: null }));
          setCursor(0);
        }
      }
      return;
    }

    if (key.ctrl && ch === 'c') {
      if (state.status !== 'idle') engine.send(cancel());
      else {
        engine.close();
        exit();
      }
      return;
    }
    if (key.ctrl && ch === 's') {
      if (input.trim()) {
        engine.send(inject(input));
        setInput('');
      }
      return;
    }
    if (key.return) {
      if (input.trim()) {
        engine.send(submit(input));
        setInput('');
      }
      return;
    }
    if (key.backspace || key.delete) {
      setInput((t) => t.slice(0, -1));
      return;
    }
    if (ch && !key.ctrl && !key.meta) setInput((t) => t + ch);
  });

  const children = [];
  state.blocks.forEach((b, i) => children.push(e(Block, { key: `b${i}`, block: b })));
  if (state.stream != null) children.push(e(Text, { key: 'stream' }, state.stream));
  children.push(
    req
      ? e(Picker, { key: 'picker', req, cursor })
      : e(Composer, { key: 'composer', input, status: state.status }),
  );
  return e(Box, { flexDirection: 'column' }, children);
}

function Block({ block }) {
  switch (block.kind) {
    case 'user':
      return e(Text, { color: 'magenta' }, `▌ ${block.text}`);
    case 'assistant':
      return e(Text, null, block.text);
    case 'tool':
      return e(
        Text,
        { color: block.done ? 'green' : 'yellow' },
        `● ${block.name}(${block.args ?? ''})${block.done ? '' : ' …'}`,
      );
    case 'inject':
      return e(Text, { color: 'cyan' }, `↳ ${block.text}`);
    default:
      return e(Text, { dimColor: true }, block.text);
  }
}

function Composer({ input, status }) {
  return e(
    Box,
    { marginTop: 1 },
    e(Text, { color: 'gray' }, status === 'idle' ? '› ' : '… '),
    e(Text, null, input),
  );
}

function Picker({ req, cursor }) {
  const q = req.questions[0] ?? {};
  const rows = [e(Text, { key: 'q', bold: true }, q.question ?? '')];
  (q.options ?? []).forEach((o, i) =>
    rows.push(
      e(Text, { key: `o${i}`, color: i === cursor ? 'cyan' : undefined }, `${i === cursor ? '❯ ' : '  '}${o.label}`),
    ),
  );
  rows.push(e(Text, { key: 'hint', dimColor: true }, '↑/↓ select · enter confirm · esc cancel'));
  return e(Box, { flexDirection: 'column', marginTop: 1, borderStyle: 'round', paddingX: 1 }, rows);
}
