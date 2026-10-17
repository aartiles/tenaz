#!/usr/bin/env node

// Scripted stand-in for `pi --mode rpc` that exercises every orchestrator
// path without a model: bootstrap, review rejection, a tool-call loop that
// must be steered and aborted, an alternating loop after finished work, a
// task that fails until it is split, a replan,
// out-of-role edits that must be reverted, and the final review.
import { existsSync, mkdirSync, readFileSync, writeFileSync, chmodSync } from 'node:fs';
import { dirname, join } from 'node:path';

if (process.argv.includes('--version')) { process.stdout.write('fake-pi 0.0.1\n'); process.exit(0); }

const cwd = process.cwd();
const statePath = join(cwd, '.agent', 'private', 'fake-state.json');
const state = existsSync(statePath) ? JSON.parse(readFileSync(statePath, 'utf8')) : {};
const bump = key => { state[key] = (state[key] ?? 0) + 1; if (existsSync(dirname(statePath))) writeFileSync(statePath, JSON.stringify(state)); return state[key]; };
const emit = value => process.stdout.write(`${JSON.stringify(value)}\n`);
const sleep = ms => new Promise(r => setTimeout(r, ms));
let aborted = false;
let prompts = 0;
let call = 0;

function write(path, text, mode) {
  mkdirSync(dirname(join(cwd, path)), { recursive: true });
  writeFileSync(join(cwd, path), text);
  if (mode) chmodSync(join(cwd, path), mode);
}
function card(id, { requires, depends = [], parent }) {
  return `---\nid: ${id}\ntitle: Fake task ${id}\nstatus: todo\nrequires: [${requires.join(', ')}]\ndepends: [${depends.join(', ')}]\n${parent ? `parent: ${parent}\n` : ''}files: [src/${id}.txt]\n---\n## Goal\nCreate src/${id}.txt.\n## Acceptance\n- check ${id}.exists passes\n`;
}
async function turn(toolName, args, effect, text = 'ok', input = 1000 + (call + 1) * 10) {
  const id = `call_${++call}`;
  emit({ type: 'turn_start' });
  emit({ type: 'tool_execution_start', toolCallId: id, toolName, args });
  effect?.();
  emit({ type: 'tool_execution_end', toolCallId: id, toolName, result: { content: [{ type: 'text', text }] }, isError: false });
  const message = { role: 'assistant', content: [{ type: 'thinking', thinking: 'plan' }, { type: 'toolCall', id, name: toolName, arguments: args }],
    stopReason: 'toolUse', usage: { input, output: 20, cacheRead: 0, cacheWrite: 0 } };
  emit({ type: 'message_end', message });
  emit({ type: 'turn_end', message, toolResults: [] });
  await sleep(2);
}
const writeTool = (path, text, mode) => turn('write', { path }, () => write(path, text, mode));
function finish(text = 'Done.') {
  const message = { role: 'assistant', content: [{ type: 'thinking', thinking: 'wrap up' }, { type: 'text', text }],
    stopReason: aborted ? 'aborted' : 'stop', usage: { input: 1200, output: 10, cacheRead: prompts > 1 ? 900 : 0, cacheWrite: 0 } };
  emit({ type: 'message_end', message });
  emit({ type: 'turn_end', message, toolResults: [] });
  emit({ type: 'agent_end', messages: [], willRetry: false });
  emit({ type: 'agent_settled', aborted });
}

const VERIFY = `#!/bin/sh
# Runs checks/<ID>.<name>.sh; --task T007 selects the T007. prefix.
prefix=""
[ "$1" = "--task" ] && prefix="$2."
passed=0; failed=""
for f in checks/*.sh; do
  id=$(basename "$f" .sh)
  case "$id" in "$prefix"*) ;; *) continue ;; esac
  if sh "$f" >/dev/null 2>&1; then passed=$((passed+1)); else failed="$failed{\\"id\\":\\"$id\\",\\"msg\\":\\"failed\\"},"; fi
done
failed=\${failed%,}
if [ -z "$failed" ]; then echo "{\\"ok\\":true,\\"passed\\":$passed,\\"failed\\":[]}"; exit 0; fi
echo "{\\"ok\\":false,\\"passed\\":$passed,\\"failed\\":[$failed]}"; exit 1
`;

async function play(prompt) {
  const role = prompt.match(/^Role: (\S+)/m)?.[1];
  const task = prompt.match(/^Task: (\S+)/m)?.[1];
  const attempt = Number(prompt.match(/^Attempt: (\d+)/m)?.[1] ?? 1);
  if (!role) return finish('323');
  const boot = role === 'BOOTSTRAP' ? bump('bootstrap') : 0;
  if (boot === 1 || boot === 2) { // engine out of memory, then a prompt over its current maximum: rerun, not counted
    const errorMessage = boot === 1 ? '400: not enough GPU memory'
      : '400: {"message":"Prompt (98353 tokens) requires ~4406MB GPU memory but only ~3494MB is available. Maximum context at KV8 and prefill chunk 512: 61488 tokens."}';
    await turn('read', { path: 'big.md' }, null, 'x'.repeat(40000)); // about 10k tokens: steered
    const message = { role: 'assistant', content: [], stopReason: 'error', errorMessage, usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } };
    emit({ type: 'message_end', message }); emit({ type: 'turn_end', message, toolResults: [] });
    emit({ type: 'agent_end', messages: [], willRetry: false }); emit({ type: 'agent_settled', aborted: false });
    return;
  }
  if (role === 'BOOTSTRAP') {
    await writeTool('.agent/spec.md', '# Spec\n\n- R1: one\n- R2: two\n- R3: three\n- R4: looks good (manual: visual)\n');
    await writeTool('.agent/map.md', '- verify: runs checks/*.sh\n- src/: one file per task\n');
    await writeTool('verify', VERIFY, 0o755);
    await writeTool('checks/T000.smoke.sh', 'exit 0\n');
    await writeTool('.agent/tasks/T001.md', card('T001', { requires: ['R1'] }));
    await writeTool('.agent/tasks/T002.md', card('T002', { requires: ['R2'], depends: ['T001'] }));
    await writeTool('.agent/tasks/T003.md', card('T003', { requires: ['R3'], depends: ['T001'] }));
    await writeTool('.agent/tasks/T004.md', card('T004', { requires: ['R1'], depends: ['T002'] }));
    await writeTool('.agent/notes/BOOTSTRAP.md', 'Fake stack.\n');
    await writeTool('BRIEF.md', 'tampered\n'); // must be reverted
    return finish();
  }
  if (role === 'BUILDER') {
    if (process.env.FAKE_SLOW_TASK === task) await sleep(5000); // gives the test time to pause the run
    if (task === 'T002' && attempt === 1) {
      while (!aborted) await turn('bash', { command: 'ls checks' });
      return finish();
    }
    if (task === 'T003') {
      await writeTool('checks/T003.exists.sh', 'exit 1\n');
      return finish();
    }
    await writeTool(`src/${task}.txt`, `${task}\n`);
    await writeTool(`checks/${task}.exists.sh`, `test -f src/${task}.txt\n`);
    await writeTool(`.agent/notes/${task}.md`, `Built ${task}.\n`);
    if (task === 'T004') { // finished, then circles between two calls with the same results
      while (!aborted) { await writeTool(`.agent/notes/${task}.md`, `Built ${task}.\n`); await turn('bash', { command: `wc -l .agent/notes/${task}.md` }); }
      return finish();
    }
    if (task === 'T001' && bump('context-T001') === 1) { // context grows past 1.3x the budget: stopped and continued
      for (let n = 1; !aborted && n < 20; n++) await turn('read', { path: `src/part${n}.md` }, null, 'ok', 30000 * n);
      return finish();
    }
    if (task === 'T001') await writeTool('.agent/tasks/T001.md', 'tampered\n'); // must be reverted
    return finish();
  }
  if (role === 'REVIEWER') {
    const file = prompt.match(/^Review file: (\S+)/m)[1];
    const n = bump(`review-${task}`);
    if (task === 'T004' && n === 1) return finish('(no verdict written)'); // must be retried
    await writeTool(file, task === 'T001' && n === 1 ? 'Verdict: CHANGES\n- [MAJOR] fake finding\n' : 'Verdict: ACCEPT\n');
    return finish();
  }
  if (role === 'SPLIT') {
    await writeTool(`.agent/tasks/${task}a.md`, card(`${task}a`, { requires: ['R3'], depends: ['T001'], parent: task }));
    await writeTool(`.agent/tasks/${task}b.md`, card(`${task}b`, { requires: ['R3'], depends: [`${task}a`], parent: task }));
    await writeTool('.agent/notes/PLANNER.md', 'Split.\n');
    return finish();
  }
  if (role === 'REPLAN') {
    await writeTool('.agent/notes/PLANNER.md', 'Plan is fine.\n');
    if (existsSync(join(cwd, '.agent/tasks/T001.md'))) await writeTool('.agent/tasks/T001.md', 'tampered done card\n'); // must be reverted
    return finish();
  }
  if (role === 'FINAL-REVIEWER') {
    await writeTool('.agent/reviews/FINAL.md', 'Verdict: ACCEPT\n');
    return finish();
  }
  return finish(`unknown role ${role}`);
}

let buffer = '';
process.stdin.on('data', chunk => {
  buffer += chunk;
  for (let at = buffer.indexOf('\n'); at >= 0; at = buffer.indexOf('\n')) {
    const line = buffer.slice(0, at); buffer = buffer.slice(at + 1);
    if (!line.trim()) continue;
    const cmd = JSON.parse(line);
    if (cmd.type === 'prompt') {
      prompts++;
      emit({ id: cmd.id, type: 'response', command: 'prompt', success: true, data: { disposition: 'started' } });
      emit({ type: 'agent_start' });
      play(cmd.message);
    } else if (cmd.type === 'abort') { aborted = true; emit({ type: 'response', command: 'abort', success: true }); }
    else emit({ type: 'response', command: cmd.type, success: true, data: { disposition: 'queued' } });
  }
});
process.stdin.on('end', () => process.exit(0));
