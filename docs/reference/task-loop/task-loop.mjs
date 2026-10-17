#!/usr/bin/env node

// Generic sequential task-loop orchestrator for one local model.
// Design: docs/TASK-LOOP-ORCHESTRATOR.md. This program makes no model calls of
// its own; Pi (RPC mode) is its only LLM client, one session at a time.
//
//   task-loop.mjs init      --project DIR --brief FILE --model P/ID [--thinking L] [--set key=value ...]
//   task-loop.mjs run       --project DIR [--allow-weak-engine]
//   task-loop.mjs status    --project DIR
//   task-loop.mjs preflight --model P/ID [--thinking L]
//
// Common options: --pi BIN (Pi executable, default `pi`).
import { execFile, spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, renameSync, rmSync,
  statSync, writeFileSync, appendFileSync } from 'node:fs';
import { homedir, tmpdir } from 'node:os';
import { deflateSync } from 'node:zlib';
import { basename, dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const TEMPLATES = join(HERE, 'task-loop', 'templates');
const ROLES = ['bootstrap', 'builder', 'reviewer', 'split', 'replan', 'final-reviewer'];
const STATUSES = new Set(['todo', 'active', 'done', 'blocked', 'split', 'dropped']);
const PI_TOOLS = 'read,bash,write,edit,grep,find,ls';
const DEFAULT_PARAMS = {
  review: true,             // fresh-context review of every task diff
  briefing: true,           // orchestrator packs spec/map/card/notes into the prompt
  maxAttempts: 3,           // build attempts per task before splitting
  maxSplitDepth: 2,         // T007 -> T007a -> T007aa, then block
  replanEvery: 5,           // done tasks between replan sessions (0 disables)
  turnBudget: 40,           // default turns per session (a card may override)
  contextBudget: 0.65,      // fraction of the context window a session should stay under
  loopSteer: 3, loopAbort: 5, // the same tool call N times in a row
  repeatWindow: 12, repeatSteer: 4, repeatAbort: 6, // the same call with the same result N times among the last calls
  idleSteer: 12, idleAbort: 20,
  providerErrorAbort: 3,
  engineRetries: 6,         // sessions ended by engine errors are retried with backoff, uncounted, this many times
  engineRetryBaseSec: 60,   // first backoff; doubles per retry up to 15 minutes
  capacityHeadroom: 0.8,    // when the engine reports its maximum context, later sessions are steered at this fraction of it
  contextFloor: 32768,      // a reported maximum below this is memory starvation: wait instead of shrinking the budget
  capacityRetries: 4,       // immediate reruns after the context budget was lowered, before blocking
  bigOutputTokens: 8000,    // a single tool result above this (about 4 characters per token) gets a steer
  contextAbort: 1.3,        // a session past this multiple of its context budget is stopped before the engine overflows
  contextContinuations: 3,  // stopped sessions are continued by a fresh session, uncounted, this many times
  sessionMinutes: 90,
  verifyTimeoutSec: 120,
  maxBootstrapAttempts: 3,
  maxFinalCycles: 2,
  diffLimitBytes: 40000,
  verifyClipBytes: 4000,
  noteClipBytes: 1500,
  mapClipBytes: 8000,
  memorySampleSec: 30,      // macOS memory pressure / swap / server footprint sampling (0 disables)
  notify: true              // macOS notifications when a run starts, waits on the engine, blocks, ends or crashes
};

const argv = process.argv.slice(2);
const command = argv[0];
const option = (name, fallback) => { const at = argv.indexOf(name); return at < 0 ? fallback : argv[at + 1]; };
const options = name => argv.flatMap((value, at) => argv[at - 1] === name ? [value] : []);
const flag = name => argv.includes(name);
const piBin = option('--pi', 'pi');
const project = resolve(option('--project', process.cwd()));
const agentDir = join(project, '.agent');
const privateDir = join(agentDir, 'private');
const ledgerPath = join(agentDir, 'run.json');
const journalPath = join(agentDir, 'journal.jsonl');
const logPath = join(privateDir, 'orchestrator.log');
let P = DEFAULT_PARAMS;
let L = null;
let child = null;
let stopping = false;

// ---------- small utilities ----------

function fail(message) { throw new Error(message); }
function read(path) { return readFileSync(path, 'utf8'); }
function readIf(path) { return existsSync(path) ? read(path) : null; }
function writeAtomic(path, text) { const temp = `${path}.${process.pid}.tmp`; writeFileSync(temp, text); renameSync(temp, path); }
function sha(text) { return createHash('sha256').update(text).digest('hex'); }
function clip(text, limit) {
  if (text == null) return '';
  return text.length <= limit ? text : `${text.slice(0, limit)}\n[... clipped ${text.length - limit} bytes]`;
}
function log(message) {
  const line = `[${new Date().toISOString()}] ${message}`;
  process.stdout.write(`${line}\n`);
  if (existsSync(privateDir)) appendFileSync(logPath, `${line}\n`);
}
function journal(record) { appendFileSync(journalPath, `${JSON.stringify({ at: new Date().toISOString(), ...record })}\n`); }
function saveLedger() { writeAtomic(ledgerPath, `${JSON.stringify(L, null, 2)}\n`); }
const rel = path => relative(project, path);

function git(...args) {
  const result = spawnSync('git', args, { cwd: project, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 });
  if (result.status !== 0) fail(`git ${args.join(' ')} failed: ${(result.stderr || result.stdout).trim()}`);
  return result.stdout;
}
function head() { return git('rev-parse', 'HEAD').trim(); }
function commit(message) {
  git('add', '-A');
  if (git('diff', '--cached', '--name-only').trim()) {
    git('-c', 'user.name=Task Loop', '-c', 'user.email=task-loop@localhost', 'commit', '-q', '-m', message);
  }
  return head();
}
function changedPaths() {
  const out = git('status', '--porcelain=v1', '-z', '--untracked-files=all');
  const paths = [];
  const entries = out.split('\0');
  for (let i = 0; i < entries.length; i++) {
    const entry = entries[i];
    if (!entry) continue;
    paths.push(entry.slice(3));
    if (entry[0] === 'R' || entry[0] === 'C') i++; // rename source follows
  }
  return paths;
}
function inHead(path) { return spawnSync('git', ['cat-file', '-e', `HEAD:${path}`], { cwd: project }).status === 0; }
function headContent(path) {
  const result = spawnSync('git', ['show', `HEAD:${path}`], { cwd: project, encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 });
  return result.status === 0 ? result.stdout : null;
}
function revertPath(path) {
  if (inHead(path)) git('checkout', 'HEAD', '--', path);
  else rmSync(join(project, path), { force: true, recursive: true });
}
function resetTo(sha) { git('reset', '-q', '--hard', sha); git('clean', '-q', '-fd'); }
function worktreeFingerprint() {
  return sha(git('status', '--porcelain=v1', '--untracked-files=all') + git('diff', 'HEAD'));
}

// ---------- model, engine, Pi ----------

function modelConfig(model, thinking) {
  const [provider, ...parts] = model.split('/');
  const id = parts.join('/');
  const settings = join(homedir(), '.pi', 'agent', 'models.json');
  const providerConfig = existsSync(settings) ? JSON.parse(read(settings)).providers?.[provider] : null;
  const entry = providerConfig?.models?.find(item => item.id === id);
  // Without the entry, the context window (and so every context budget) would be a guess.
  if (providerConfig && !entry) fail(`Model ${id} is not listed under provider ${provider} in ${settings}`);
  const version = spawnSync(piBin, ['--version'], { encoding: 'utf8' });
  if (version.status !== 0) fail(`Cannot run ${piBin} --version`);
  return { model, thinking, provider, id, baseUrl: providerConfig?.baseUrl ?? null,
    contextWindow: entry?.contextWindow ?? 131072, input: entry?.input ?? ['text'], pi: piBin, pi_version: version.stdout.trim() };
}
async function ensureServer(config) {
  if (!config.baseUrl) return;
  try {
    const response = await fetch(`${config.baseUrl.replace(/\/$/, '')}/models`, { signal: AbortSignal.timeout(5000) });
    const data = await response.json();
    if (data.data?.some(item => item.id === config.id)) return;
  } catch { /* reported below */ }
  fail(`Model ${config.id} is not being served at ${config.baseUrl}. Start the inference server first.`);
}
// Engine-side timings from a Prometheus /metrics endpoint (TensorFold, vLLM
// and similar). Counters are cumulative; with one request in flight at a time,
// the difference across one model response is that request's own timing.
const ENGINE_SERIES = {
  requests: ['time_to_first_token_seconds_count'],
  ttft_s: ['time_to_first_token_seconds_sum'],
  decode_s: ['request_decode_seconds_sum', 'request_decode_time_seconds_sum'],
  prefill_s: ['request_prefill_time_seconds_sum'],
  latency_s: ['request_latency_seconds_sum', 'e2e_request_latency_seconds_sum'],
  prompt_tokens: ['prompt_tokens_total'],
  cached_tokens: ['prompt_tokens_cached_total', 'prefix_cache_tokens_total'],
  output_tokens: ['generation_tokens_total'],
  drafted: ['mtp_drafted_total', 'spec_decode_num_draft_tokens_total'],
  accepted: ['mtp_accepted_total', 'spec_decode_num_accepted_tokens_total']
};
async function engineMetrics(config) {
  if (!config.baseUrl) return null;
  try {
    const response = await fetch(new URL('/metrics', config.baseUrl), { signal: AbortSignal.timeout(3000) });
    if (!response.ok) return null;
    const values = {};
    for (const line of (await response.text()).split('\n')) {
      const match = line.match(/^(?:[\w]+:)?(\w+?)(?:\{[^}]*\})?\s+([\d.eE+-]+)$/);
      if (match) values[match[1]] = (values[match[1]] ?? 0) + Number(match[2]);
    }
    const out = {};
    for (const [key, names] of Object.entries(ENGINE_SERIES)) {
      const name = names.find(n => n in values);
      if (name) out[key] = values[name];
    }
    return 'requests' in out ? out : null;
  } catch { return null; }
}
const metricsDelta = (now, before) => Object.fromEntries(Object.keys(now).map(k => [k, +(now[k] - (before[k] ?? 0)).toFixed(6)]));

// macOS memory sampling. Swap-ins between samples are the signal that the
// model or its cache no longer fits; the pressure level is the kernel's own
// verdict (1 normal, 2 warning, 4 critical).
const exec = (file, args) => new Promise(r => execFile(file, args, { timeout: 10_000 }, (error, stdout) => r(error ? '' : stdout)));
const gib = text => { const m = String(text).replace(',', '.').match(/([\d.]+)([KMGT])?/); return m ? +(Number(m[1]) * { K: 2 ** -20, M: 2 ** -10, G: 1, T: 1024 }[m[2] ?? 'G']).toFixed(2) : null; };
function memorySampler(config) {
  if (process.platform !== 'darwin' || !P.memorySampleSec) return null;
  const port = config.baseUrl ? new URL(config.baseUrl).port || '80' : null;
  let previous = null, window = null, timer = null;
  const sample = async () => {
    const [pressure, swap, vm, pid] = await Promise.all([exec('sysctl', ['-n', 'kern.memorystatus_vm_pressure_level']), exec('sysctl', ['-n', 'vm.swapusage']),
      exec('vm_stat', []), port ? exec('lsof', ['-nP', `-iTCP:${port}`, '-sTCP:LISTEN', '-t']) : '']);
    const page = Number(vm.match(/page size of (\d+)/)?.[1] ?? 16384);
    const pages = name => Number(vm.match(new RegExp(`${name}:\\s+(\\d+)`))?.[1] ?? NaN);
    const server = pid.trim().split('\n')[0];
    // top's CMPRS column is not used: for a Metal process it reports GPU memory, not
    // compressed pages (it showed 46 GiB while the whole compressor held under 9).
    const top = server ? (await exec('top', ['-l', '1', '-pid', server, '-stats', 'pid,mem'])).trim().split('\n').at(-1).trim().split(/\s+/) : [];
    const now = { at: new Date().toISOString(), session: L?.inflight?.label ?? null, pressure_level: Number(pressure) || null,
      swap_used_gib: gib(swap.match(/used = ([\d.,]+M)/)?.[1]), swapins: pages('Swapins'), swapouts: pages('Swapouts'),
      compressor_gib: +(pages('Pages occupied by compressor') * page / 2 ** 30).toFixed(2), free_gib: +(pages('Pages free') * page / 2 ** 30).toFixed(2),
      wired_gib: +(pages('Pages wired down') * page / 2 ** 30).toFixed(2),
      server_pid: server ? Number(server) : null, server_mem_gib: top[0] === server ? gib(top[1]) : null };
    const record = { ...now, swapins_delta: previous ? now.swapins - previous.swapins : 0, swapouts_delta: previous ? now.swapouts - previous.swapouts : 0, page_bytes: page };
    previous = now;
    appendFileSync(join(privateDir, 'memory.jsonl'), `${JSON.stringify(record)}\n`);
    if (window) {
      window.samples++;
      window.pressure_max = Math.max(window.pressure_max, record.pressure_level ?? 0);
      window.swapins += record.swapins_delta; window.swapouts += record.swapouts_delta;
      window.server_mem_max_gib = Math.max(window.server_mem_max_gib, record.server_mem_gib ?? 0);
      window.swap_used_max_gib = Math.max(window.swap_used_max_gib, record.swap_used_gib ?? 0);
    }
  };
  let running = Promise.resolve();
  const tick = () => { running = running.then(sample).catch(() => {}); return running; };
  return {
    start() { tick(); timer = setInterval(tick, P.memorySampleSec * 1000); timer.unref(); },
    stop() { clearInterval(timer); return running; },
    // Per-session summary: sample once at both ends so short sessions are covered.
    async begin() { await tick(); window = { samples: 0, pressure_max: 0, swapins: 0, swapouts: 0, server_mem_max_gib: 0, swap_used_max_gib: 0 }; },
    async end() { await tick(); const w = window; window = null; return w && { ...w, swap_in_gib: +(w.swapins * (previous?.page_bytes ?? 16384) / 2 ** 30).toFixed(2), swap_out_gib: +(w.swapouts * (previous?.page_bytes ?? 16384) / 2 ** 30).toFixed(2) }; }
  };
}
let sampler = null;

function otherPiInProject() {
  const ps = spawnSync('ps', ['-axo', 'pid=,command='], { encoding: 'utf8' });
  const pids = ps.stdout.split('\n').map(line => line.trim().match(/^(\d+)\s+(\S+)/)).filter(Boolean)
    .filter(([, pid, name]) => Number(pid) !== process.pid && basename(name) === 'pi').map(([, pid]) => pid);
  return pids.filter(pid => spawnSync('lsof', ['-a', '-p', pid, '-d', 'cwd', '-Fn'], { encoding: 'utf8' })
    .stdout.split('\n').includes(`n${project}`));
}
function piArgs(config, sessionDir) {
  // --no-skills stops discovery; skills pinned at init are still loaded explicitly.
  const skills = (L?.skills ?? []).flatMap(skill => ['--skill', join(agentDir, 'skills', skill.name)]);
  return ['--mode', 'rpc', ...(sessionDir ? ['--session-dir', sessionDir] : ['--no-session']),
    '--model', config.model, '--thinking', config.thinking, '--tools', PI_TOOLS,
    '--no-mcp', '--no-extensions', '--no-skills', ...skills, '--approve', '--offline'];
}
// Skills are copied into .agent/skills/ at init, made read-only and hashed, so a
// run is reproducible and cannot change the user's own copy.
const SKILL_SKIP = new Set(['.git', '.github', 'node_modules']);
function treeFiles(dir, base = dir) {
  return readdirSync(dir, { withFileTypes: true }).filter(e => !SKILL_SKIP.has(e.name)).sort((a, b) => a.name.localeCompare(b.name))
    .flatMap(e => e.isDirectory() ? treeFiles(join(dir, e.name), base) : e.isFile() ? [relative(base, join(dir, e.name))] : []);
}
const treeHash = dir => sha(treeFiles(dir).map(f => `${f}\0${sha(readFileSync(join(dir, f)))}`).join('\n'));
function installSkill(source) {
  const from = resolve(source.replace(/^~(?=\/)/, homedir()));
  if (!existsSync(join(from, 'SKILL.md'))) fail(`${from} has no SKILL.md`);
  const name = basename(from);
  const to = join(agentDir, 'skills', name);
  for (const file of treeFiles(from)) {
    mkdirSync(dirname(join(to, file)), { recursive: true });
    copyFileSync(join(from, file), join(to, file));
    chmodSync(join(to, file), statSync(join(from, file)).mode & 0o555);
  }
  // Read-only directories too, so a build inside the skill fails loudly instead of changing it.
  const dirs = [...new Set(treeFiles(to).map(f => dirname(join(to, f))))].sort((a, b) => b.length - a.length);
  for (const dir of [...dirs, to]) chmodSync(dir, 0o555);
  return { name, source: from, sha256: treeHash(to), files: treeFiles(to).length };
}
function checkSkills() {
  for (const skill of L.skills ?? []) {
    if (treeHash(join(agentDir, 'skills', skill.name)) !== skill.sha256) {
      journal({ type: 'skill-changed', skill: skill.name });
      fail(`The pinned skill ${skill.name} in .agent/skills/ changed during the run; it must stay read-only`);
    }
  }
}

// Pi RPC session with watchdogs. `prompts` is a list; each is sent after the
// previous one settles (preflight uses two). Returns metrics for the journal.
function runPi(config, { prompts, cwd, sessionDir, label, watch }) {
  const outputPath = sessionDir ? join(privateDir, `${label}.rpc.jsonl`) : null;
  const started = Date.now();
  const m = { label, turns: 0, usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, reasoning: 0 },
    reasoningChars: 0, contextPeak: 0, compactions: 0, providerErrors: 0, toolCalls: 0, writes: 0, capacityLimits: [], bigOutputs: 0,
    events: [], assistant: [], callArgs: new Map(), requests: [], settled: false, aborted: false, abortReason: null, exitCode: null, lastStopReason: null };
  // Engine metrics are read in order after each response; `pending` serialises them.
  let pending = engineMetrics(config);
  let turnStarted = 0, firstToken = 0;
  const recordRequest = (usage, stopReason, clientMs) => {
    pending = pending.then(async before => {
      let now = before && await engineMetrics(config);
      // The engine may publish its counters just after the stream closes.
      for (let wait = 0; before && now && now.requests === before.requests && wait < 10; wait++) {
        await new Promise(r => setTimeout(r, 100));
        now = await engineMetrics(config);
      }
      const engine = before && now ? metricsDelta(now, before) : null;
      const request = { session: label, sequence: m.requests.length + 1, stop_reason: stopReason,
        prompt_tokens: (Number(usage.input) || 0) + (Number(usage.cacheRead) || 0), cached_tokens: Number(usage.cacheRead) || 0,
        output_tokens: Number(usage.output) || 0, ...clientMs, engine };
      // Engine timings belong to this response only if exactly one request finished
      // and its token counts agree with Pi's.
      request.engine_matched = !!engine && engine.requests === 1 && engine.prompt_tokens === request.prompt_tokens
        && engine.output_tokens === request.output_tokens;
      m.requests.push(request);
      if (outputPath) appendFileSync(join(privateDir, 'requests.jsonl'), `${JSON.stringify(request)}\n`);
      return now ?? before;
    });
  };
  return new Promise((resolveRun, rejectRun) => {
    child = spawn(piBin, piArgs(config, sessionDir), { cwd, stdio: ['pipe', 'pipe', 'pipe'] });
    const proc = child;
    let buffer = '';
    let promptIndex = 0;
    let closing = false;
    const steered = new Set();
    let lastKey = null, sameCount = 0, idleTurns = 0, recent = [], fingerprint = watch?.idle ? worktreeFingerprint() : null;
    const send = value => { if (proc.stdin.writable) proc.stdin.write(`${JSON.stringify(value)}\n`); };
    const event = (kind, detail) => { m.events.push({ kind, detail, turn: m.turns, ms: Date.now() - started }); log(`  ${label}: ${kind}${detail ? ` — ${detail}` : ''}`); };
    const steer = (key, message) => { if (steered.has(key) || m.abortReason) return; steered.add(key); event(`steer:${key}`); send({ type: 'steer', message: `[orchestrator] ${message}` }); };
    const abort = reason => {
      if (m.abortReason) return;
      m.abortReason = reason; event('abort', reason);
      send({ type: 'clear_queue' }); send({ type: 'abort' });
      setTimeout(() => { if (!closing) proc.kill('SIGTERM'); }, 60_000).unref();
    };
    const wall = setTimeout(() => abort(`session exceeded ${P.sessionMinutes} minutes`), P.sessionMinutes * 60_000);
    const sendNext = () => {
      if (promptIndex >= prompts.length) { closing = true; proc.stdin.end(); return; }
      send({ id: `prompt-${promptIndex}`, type: 'prompt', message: prompts[promptIndex++] });
    };
    const onEvent = e => {
      if (e.type === 'response' && e.command === 'prompt' && (!e.success || e.data?.disposition === 'handled')) {
        abort(`prompt rejected: ${e.error ?? e.data?.disposition}`);
      }
      if (e.type === 'tool_execution_start') {
        m.toolCalls++;
        m.callArgs.set(e.toolCallId, [e.toolName, e.args]);
        const key = `${e.toolName}:${JSON.stringify(e.args)}`;
        sameCount = key === lastKey ? sameCount + 1 : 1;
        lastKey = key;
        if (watch && sameCount >= P.loopAbort) abort(`same ${e.toolName} call ${sameCount} times in a row`);
        else if (watch && sameCount >= P.loopSteer) {
          steer(`loop-${sha(key).slice(0, 8)}`, `You have made the same ${e.toolName} call ${sameCount} times in a row with the same arguments. Repeating it will not change the result. Read the relevant code or output, change your approach, or finish your handoff files and stop.`);
        }
        if (e.toolName === 'write' || e.toolName === 'edit') { m.writes++; idleTurns = -1; }
      }
      if (e.type === 'tool_execution_end') {
        // Catches loops that alternate calls (write, wc, write, wc, ...), which the
        // consecutive check misses and the idle check counts as progress.
        const args = m.callArgs.get(e.toolCallId);
        const key = `${e.toolName}:${JSON.stringify(args ?? null)}:${sha(JSON.stringify(e.result ?? null))}`;
        m.callArgs.delete(e.toolCallId);
        // One whole-file read of a large reference can add 20k tokens; a few of them
        // overflow what the engine can hold.
        // Images are sent as base64 but cost few tokens, so only text is counted.
        const text = Array.isArray(e.result?.content) ? e.result.content.filter(b => b.type === 'text').map(b => b.text ?? '').join('') : JSON.stringify(e.result ?? '');
        const outputTokens = Math.round(text.length / 4);
        if (watch && outputTokens > P.bigOutputTokens && ++m.bigOutputs <= 3) {
          steer(`big-output-${m.bigOutputs}`, `That ${e.toolName} result was about ${outputTokens} tokens, and this session's context budget is ${watch.contextTokens}. Do not read large files whole: grep for the part you need and read that range (offset/limit), and pipe command output through head, tail or grep.`);
        }
        recent = [...recent.slice(1 - P.repeatWindow), key];
        const count = recent.filter(k => k === key).length;
        if (watch && count >= P.repeatAbort) abort(`same ${e.toolName} call with the same result ${count} times in the last ${recent.length} calls`);
        else if (watch && count >= P.repeatSteer) {
          steer(`repeat-${sha(key).slice(0, 8)}`, `You have made the same ${e.toolName} call and got the same result ${count} times in your last ${recent.length} tool calls. You are going in circles. Change your approach, or, if the task is done, stop now.`);
        }
      }
      if (e.type === 'turn_start') { turnStarted = Date.now(); firstToken = 0; }
      if (e.type === 'message_update' && !firstToken) firstToken = Date.now();
      if (e.type === 'message_end' && e.message?.role === 'assistant') {
        const msg = e.message;
        const u = msg.usage ?? {};
        const end = Date.now();
        // Client-side timings include Pi's request setup; the engine's own figures are in `engine`.
        recordRequest(u, msg.stopReason, { client_first_token_s: turnStarted && firstToken ? (firstToken - turnStarted) / 1000 : null,
          client_total_s: turnStarted ? (end - turnStarted) / 1000 : null });
        for (const k of Object.keys(m.usage)) m.usage[k] += Number(u[k]) || 0;
        for (const block of msg.content ?? []) if (block.type === 'thinking') m.reasoningChars += (block.thinking ?? '').length;
        m.contextPeak = Math.max(m.contextPeak, (Number(u.input) || 0) + (Number(u.cacheRead) || 0) + (Number(u.output) || 0));
        m.lastStopReason = msg.stopReason;
        const text = (msg.content ?? []).filter(b => b.type === 'text').map(b => b.text).join('');
        if (text) (m.texts ??= []).push(text);
        m.assistant.push({ stopReason: msg.stopReason, usage: u, thinking: (msg.content ?? []).filter(b => b.type === 'thinking').reduce((n, b) => n + (b.thinking ?? '').length, 0) });
        if (msg.stopReason === 'error') {
          m.providerErrors++;
          event('provider-error', msg.errorMessage ?? '');
          // Sushi names the largest prompt it can hold right now.
          const limit = Number(/Maximum context[^:]*:\s*(\d+) tokens/.exec(msg.errorMessage ?? '')?.[1]);
          if (limit) m.capacityLimits.push(limit);
          // A vision engine can refuse an image the session read: too many, too large, or
          // a format it does not decode. The image stays in the conversation, so every
          // later request fails the same way.
          if (/at most \d+ images|image tokens|\bimage \d+ |images need|does not decode/i.test(msg.errorMessage ?? '')) m.imageRefusal = msg.errorMessage;
          if (watch && m.providerErrors >= P.providerErrorAbort) abort(`${m.providerErrors} provider errors`);
        }
        if (watch && m.contextPeak > watch.contextTokens * P.contextAbort) abort(`context budget exceeded (${m.contextPeak}/${watch.contextTokens} tokens)`);
        else if (watch && m.contextPeak > watch.contextTokens) {
          steer('context', `This session has used ${m.contextPeak} tokens of context, over its budget of ${watch.contextTokens}. Finish the smallest coherent step, run ./verify, write your handoff notes, and stop. The next session continues from the repository.`);
        }
      }
      if (e.type === 'turn_end') {
        m.turns++;
        if (watch?.idle) {
          const now = worktreeFingerprint();
          idleTurns = now !== fingerprint || idleTurns < 0 ? 0 : idleTurns + 1;
          fingerprint = now;
          if (idleTurns >= P.idleAbort) abort(`${idleTurns} turns without changing any file`);
          else if (idleTurns >= P.idleSteer) steer('idle', `You have gone ${idleTurns} turns without changing any file. Stop investigating and make the change the task needs, or write your notes explaining what blocks you and stop.`);
        }
        if (watch && m.turns >= Math.ceil(watch.turns * 1.5)) abort(`turn budget exceeded (${m.turns}/${watch.turns})`);
        else if (watch && m.turns >= watch.turns) steer('turns', `You have used your turn budget (${watch.turns}). Finish the current step, run ./verify, write your handoff notes, and stop.`);
      }
      if (e.type === 'compaction_start') {
        m.compactions++;
        event('compaction', e.reason);
        if (watch && m.compactions >= 2) abort('second context compaction');
        else if (watch) steer('compaction', 'Context is being compacted, which loses detail. Finish the current step, run ./verify, write your handoff notes, and stop.');
      }
      if (e.type === 'agent_settled') {
        if (m.abortReason || e.aborted) { m.aborted = !!e.aborted || !!m.abortReason; closing = true; proc.stdin.end(); return; }
        if (promptIndex >= prompts.length) m.settled = true;
        sendNext();
      }
    };
    const output = outputPath ? { write: line => appendFileSync(outputPath, `${line}\n`) } : null;
    proc.stdout.on('data', chunk => {
      buffer += chunk.toString('utf8');
      for (let at = buffer.indexOf('\n'); at >= 0; at = buffer.indexOf('\n')) {
        const line = buffer.slice(0, at).replace(/\r$/, '');
        buffer = buffer.slice(at + 1);
        if (!line) continue;
        output?.write(line);
        try { onEvent(JSON.parse(line)); } catch (error) { if (!(error instanceof SyntaxError)) throw error; }
      }
    });
    if (sessionDir) proc.stderr.on('data', chunk => appendFileSync(join(privateDir, `${label}.stderr.log`), chunk));
    else proc.stderr.resume();
    proc.on('error', error => { clearTimeout(wall); child = null; rejectRun(error); });
    proc.on('spawn', sendNext);
    proc.on('close', code => {
      clearTimeout(wall);
      child = null;
      m.exitCode = code;
      m.durationMs = Date.now() - started;
      pending.then(() => resolveRun(m), () => resolveRun(m));
    });
  });
}
function sessionOk(m) {
  return m.settled && !m.aborted && !m.abortReason && m.exitCode === 0 && m.lastStopReason !== 'error';
}

// A 64x64 PNG: a red square on a blue background, written without dependencies.
function testImage(path) {
  const size = 64, rows = [];
  for (let y = 0; y < size; y++) {
    const row = [0];
    for (let x = 0; x < size; x++) row.push(...(x >= 16 && x < 48 && y >= 16 && y < 48 ? [220, 30, 30] : [30, 60, 220]));
    rows.push(...row);
  }
  const crcTable = Array.from({ length: 256 }, (_, n) => { for (let k = 0; k < 8; k++) n = n & 1 ? 0xedb88320 ^ (n >>> 1) : n >>> 1; return n >>> 0; });
  const crc = buf => { let c = 0xffffffff; for (const b of buf) c = crcTable[(c ^ b) & 0xff] ^ (c >>> 8); return (c ^ 0xffffffff) >>> 0; };
  const chunk = (type, data) => { const body = Buffer.concat([Buffer.from(type), data]); const out = Buffer.alloc(body.length + 8);
    out.writeUInt32BE(data.length, 0); body.copy(out, 4); out.writeUInt32BE(crc(body), body.length + 4); return out; };
  const header = Buffer.alloc(13); header.writeUInt32BE(size, 0); header.writeUInt32BE(size, 4); header.set([8, 2, 0, 0, 0], 8);
  writeFileSync(path, Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), chunk('IHDR', header),
    chunk('IDAT', deflateSync(Buffer.from(rows))), chunk('IEND', Buffer.alloc(0))]));
}

async function preflight(config) {
  const filler = 'The orchestrator measures whether the engine reuses a cached prompt prefix. '.repeat(40);
  const vision = (config.input ?? []).includes('image');
  const imageDir = vision ? mkdtempSync(join(tmpdir(), 'task-loop-preflight-')) : null;
  if (vision) testImage(join(imageDir, 'probe.png'));
  const m = await runPi(config, { cwd: imageDir ?? process.cwd(), label: 'preflight', watch: null, prompts: [
    `${filler}\nThink briefly, then answer with only the number: what is 17 * 23?`,
    'Now answer with only the number: what is 19 * 21?',
    ...(vision ? ['Use the read tool to open probe.png in the current directory and look at it. Answer with two words only: the colour of the square, then the colour of the background.'] : [])
  ] });
  const second = m.assistant[m.assistant.findIndex((a, i) => i > 0 && m.assistant[i - 1].stopReason === 'stop')] ?? m.assistant.at(-1);
  const lastText = (m.texts ?? []).at(-1) ?? '';
  if (imageDir) rmSync(imageDir, { recursive: true, force: true });
  const result = {
    at: new Date().toISOString(), settled: m.settled,
    reasoning: m.reasoningChars > 0 || m.usage.reasoning > 0,
    cache: (Number(second?.usage?.cacheRead) || 0) > 0,
    // null when the model declares no image input; otherwise whether it read the probe image correctly.
    vision: vision ? /red/i.test(lastText) && /blue/i.test(lastText) && lastText.search(/red/i) < lastText.search(/blue/i) : null,
    vision_answer: vision ? lastText.trim().slice(0, 80) : undefined,
    reasoning_chars: m.reasoningChars, second_turn_cache_read: Number(second?.usage?.cacheRead) || 0,
    duration_ms: m.durationMs,
    // Informational: engine timings are recorded only when /metrics is available.
    engine_metrics: m.requests.some(r => r.engine_matched), engine: engineSummary(m.requests)
  };
  result.ok = result.settled && result.reasoning && result.cache && result.vision !== false;
  return result;
}

// ---------- project model: spec, tasks, verify ----------

function parseValue(raw) {
  const v = raw.trim();
  if (/^\[.*\]$/.test(v)) return v.slice(1, -1).split(',').map(s => s.trim().replace(/^["']|["']$/g, '')).filter(Boolean);
  if (/^\{.*\}$/.test(v)) {
    return Object.fromEntries(v.slice(1, -1).split(',').map(pair => pair.split(':').map(s => s.trim()))
      .filter(([k]) => k).map(([k, x]) => [k, parseValue(x ?? '')]));
  }
  if (/^-?\d+(\.\d+)?$/.test(v)) return Number(v);
  if (v === 'true' || v === 'false') return v === 'true';
  return v.replace(/^["']|["']$/g, '');
}
function parseCard(text) {
  const match = text.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n?([\s\S]*)$/);
  if (!match) fail('missing --- frontmatter ---');
  const meta = {};
  for (const line of match[1].split(/\r?\n/)) {
    const clean = line.replace(/\s+#.*$/, '');
    if (!clean.trim()) continue;
    const at = clean.indexOf(':');
    if (at < 0) fail(`bad frontmatter line: ${line}`);
    meta[clean.slice(0, at).trim()] = parseValue(clean.slice(at + 1));
  }
  return { meta, body: match[2] };
}
function setCardFields(id, values) {
  const path = join(agentDir, 'tasks', `${id}.md`);
  let text = read(path);
  const end = text.indexOf('\n---', 4);
  let front = text.slice(0, end), rest = text.slice(end);
  for (const [key, value] of Object.entries(values)) {
    const line = `${key}: ${value}`;
    const pattern = new RegExp(`^${key}:.*$`, 'm');
    front = pattern.test(front) ? front.replace(pattern, line) : `${front}\n${line}`;
  }
  writeFileSync(path, front + rest);
}
function specRequirements() {
  const text = readIf(join(agentDir, 'spec.md')) ?? '';
  const reqs = new Map();
  for (const line of text.split('\n')) {
    const match = line.match(/^\s*(?:[-*]\s*)?\**(R\d+)\**\s*[:.)-]/);
    if (match) reqs.set(match[1], { id: match[1], manual: /\bmanual\s*:/i.test(line), text: line.trim() });
  }
  return reqs;
}
function loadTasks() {
  const dir = join(agentDir, 'tasks');
  const tasks = new Map();
  const errors = [];
  if (!existsSync(dir)) return { tasks, errors: ['.agent/tasks/ does not exist'] };
  for (const file of readdirSync(dir).filter(f => f.endsWith('.md')).sort()) {
    try {
      const { meta, body } = parseCard(read(join(dir, file)));
      const id = String(meta.id ?? '');
      if (!/^T\d{3}[a-z]*$/.test(id)) fail(`id "${id}" must look like T007 or T007a`);
      if (file !== `${id}.md`) fail(`file name must be ${id}.md`);
      const task = { id, title: String(meta.title ?? ''), status: String(meta.status ?? 'todo'),
        requires: [].concat(meta.requires ?? []), depends: [].concat(meta.depends ?? []), files: [].concat(meta.files ?? []),
        parent: meta.parent ? String(meta.parent) : null, attempts: Number(meta.attempts ?? 0),
        budget: typeof meta.budget === 'object' ? meta.budget : {}, body, file: `.agent/tasks/${file}` };
      if (!STATUSES.has(task.status)) fail(`unknown status ${task.status}`);
      tasks.set(id, task);
    } catch (error) { errors.push(`.agent/tasks/${file}: ${error.message}`); }
  }
  const reqs = specRequirements();
  for (const task of tasks.values()) {
    for (const dep of task.depends) if (!tasks.has(dep)) errors.push(`${task.id}: depends on missing ${dep}`);
    for (const r of task.requires) if (!reqs.has(r)) errors.push(`${task.id}: requires unknown ${r}`);
    if (task.parent && !tasks.has(task.parent)) errors.push(`${task.id}: parent ${task.parent} does not exist`);
  }
  const state = new Map();
  const visit = (id, trail) => {
    if (state.get(id) === 'done' || !tasks.has(id)) return;
    if (state.get(id) === 'open') { errors.push(`dependency cycle: ${[...trail, id].join(' -> ')}`); return; }
    state.set(id, 'open');
    for (const dep of tasks.get(id).depends) visit(dep, [...trail, id]);
    state.set(id, 'done');
  };
  for (const id of tasks.keys()) visit(id, []);
  return { tasks, errors };
}
function resolved(tasks, id, seen = new Set()) {
  const task = tasks.get(id);
  if (!task || seen.has(id)) return false;
  seen.add(id);
  if (task.status === 'done' || task.status === 'dropped') return true;
  if (task.status !== 'split') return false;
  const children = [...tasks.values()].filter(t => t.parent === id);
  return children.length > 0 && children.every(c => resolved(tasks, c.id, seen));
}
function readyTasks(tasks) {
  return [...tasks.values()].filter(t => (t.status === 'todo' || t.status === 'active')
    && t.depends.every(dep => resolved(tasks, dep)))
    .sort((a, b) => (a.status === 'active' ? -1 : 0) - (b.status === 'active' ? -1 : 0) || a.id.localeCompare(b.id));
}
function coverage(tasks, { doneOnly }) {
  const reqs = specRequirements();
  const missing = [];
  for (const r of reqs.values()) {
    if (r.manual) continue;
    const covered = [...tasks.values()].some(t => t.requires.includes(r.id) && t.status !== 'dropped' && t.status !== 'split'
      && (!doneOnly || t.status === 'done'));
    if (!covered) missing.push(r.id);
  }
  return { total: reqs.size, manual: [...reqs.values()].filter(r => r.manual).map(r => r.id), missing };
}
function runVerify(args) {
  const path = join(project, 'verify');
  if (!existsSync(path)) return { ok: false, summary: 'verify does not exist at the project root', json: null };
  try { chmodSync(path, statSync(path).mode | 0o111); } catch { /* reported by spawn */ }
  const result = spawnSync(path, args, { cwd: project, encoding: 'utf8', timeout: P.verifyTimeoutSec * 1000,
    maxBuffer: 64 * 1024 * 1024, env: { ...process.env, CI: '1' } });
  const lines = (result.stdout ?? '').trim().split('\n');
  let json = null;
  try { json = JSON.parse(lines.at(-1)); } catch { /* contract violation, reported below */ }
  const timedOut = result.error?.code === 'ETIMEDOUT';
  const ok = result.status === 0 && json?.ok === true;
  let summary;
  if (timedOut) summary = `verify ${args.join(' ')} timed out after ${P.verifyTimeoutSec}s`;
  else if (!json) summary = `verify ${args.join(' ')} exited ${result.status} without a JSON last line.\nstdout tail:\n${clip((result.stdout ?? '').slice(-2000), 2000)}\nstderr tail:\n${clip((result.stderr ?? '').slice(-1500), 1500)}`;
  else summary = clip(`verify ${args.join(' ')} -> exit ${result.status}\n${JSON.stringify(json)}`, P.verifyClipBytes);
  return { ok, exit: result.status, json, summary };
}

// ---------- briefing ----------

function section(title, text) { return text ? `\n\n## ${title}\n\n${text.trim()}` : ''; }
function rolePrompt(role) { return read(join(agentDir, 'prompts', `${role}.md`)); }
function noteFor(id) { return readIf(join(agentDir, 'notes', `${id}.md`)); }
function taskTable(tasks) {
  return [...tasks.values()].map(t => `${t.id} [${t.status}${t.attempts ? `, ${t.attempts} attempts` : ''}] ${t.title}`
    + `${t.depends.length ? ` (depends ${t.depends.join(', ')})` : ''}${t.parent ? ` (parent ${t.parent})` : ''}`).join('\n');
}
function header(role, fields) {
  return [`Role: ${role.toUpperCase()}`, ...Object.entries(fields).map(([k, v]) => `${k}: ${v}`),
    'This is a fresh session. Do only this unit of work, then stop.'].join('\n');
}
// Stable content first (role prompt, spec, map) so the engine can reuse the prefix cache.
function briefing(role, { fields = {}, task = null, extra = [] } = {}) {
  let text = rolePrompt(role);
  const packed = P.briefing || role !== 'builder';
  if (role !== 'bootstrap' && packed) {
    text += section('Spec (.agent/spec.md)', readIf(join(agentDir, 'spec.md')));
    text += section('Repository map (.agent/map.md)', clip(readIf(join(agentDir, 'map.md')), P.mapClipBytes));
  }
  text += `\n\n## Session\n\n${header(role, fields)}`;
  if (task) text += packed ? section(`Task card (${task.file})`, read(join(project, task.file))) : `\n\nYour task card is ${task.file}. Read it, then the spec and map as needed.`;
  for (const [title, body] of extra) text += section(title, body);
  return text;
}
function depNotes(tasks, task) {
  const ids = new Set(['BOOTSTRAP', ...task.depends]);
  for (const dep of task.depends) for (const t of tasks.values()) if (t.parent === dep) ids.add(t.id);
  return [...ids].map(id => [id, noteFor(id)]).filter(([, n]) => n).map(([id, n]) => `### ${id}\n${clip(n.trim(), P.noteClipBytes)}`).join('\n\n');
}

// ---------- sessions with ownership enforcement ----------

const OWNERSHIP = {
  bootstrap: { code: true, agent: p => /^\.agent\/(spec\.md|map\.md|tasks\/|notes\/)/.test(p) },
  builder: { code: true, agent: (p, ctx) => p === '.agent/map.md' || p === `.agent/notes/${ctx.task}.md` },
  reviewer: { code: false, agent: (p, ctx) => p === `.agent/reviews/${ctx.task}.md` },
  split: { code: false, agent: p => p.startsWith('.agent/tasks/') || p === '.agent/notes/PLANNER.md' },
  replan: { code: false, agent: p => p.startsWith('.agent/tasks/') || p === '.agent/notes/PLANNER.md' },
  'final-reviewer': { code: false, agent: p => p === '.agent/reviews/FINAL.md' || (p.startsWith('.agent/tasks/') && !inHead(p)) }
};
const PROTECTED = p => p === 'BRIEF.md' || p === 'AGENTS.md' || p.startsWith('.agent/prompts/');
const revertedNote = (role, paths) => `The orchestrator reverted your changes to ${paths.join(', ')}, because a ${role} session may not write there. ` +
  `Under .agent/ a bootstrap session writes only spec.md, map.md, tasks/ and notes/; BRIEF.md, AGENTS.md and .agent/prompts/ are never edited. ` +
  `Put verify and any code it runs in the project itself (for example a checks/ or scripts/ folder at the root), not under .agent/.`;
function enforceOwnership(role, ctx) {
  const rule = OWNERSHIP[role];
  const reverted = [];
  for (const path of changedPaths()) {
    const isAgent = path.startsWith('.agent/');
    let allowed = !PROTECTED(path) && (isAgent ? rule.agent(path, ctx) : rule.code);
    // A done card is history: nobody may rewrite it.
    if (allowed && path.startsWith('.agent/tasks/') && /^status:\s*done\s*$/m.test(headContent(path) ?? '')) allowed = false;
    if (!allowed) { revertPath(path); reverted.push(path); }
  }
  return reverted;
}
// An engine failure (out of memory, server down) says nothing about the task,
// so it must not use up the task's attempts: wait, then rerun the same session.
class EngineUnavailable extends Error {}
// Ctrl-C or SIGTERM during a session: the run pauses where it is (see run()).
class Paused extends Error {}
const CONTINUATION = `## Continuation

An earlier session on this same work was stopped because its context grew past the budget. Its file changes are kept in the working tree. Start with \`git status\`, \`git diff --stat\` and the task's notes file to see what it did, then continue from there. Do not reread large files whole.`;
const imageContinuation = refusal => `## Continuation

An earlier session on this same work was stopped because the engine refused an image it had read: "${String(refusal).replace(/\s+/g, ' ').slice(0, 300)}". Every image you read stays in the conversation. Its file changes are kept in the working tree. Start with \`git status\`, \`git diff --stat\` and the task's notes file to see what it did, then continue from there. Read only PNG or JPEG images; convert any other format with ffmpeg into \`.agent/private/\` first. Read few images: combine several stills into one small contact sheet (for example with ffmpeg's \`tile\` filter) and read that, and do not read the same image twice.`;
const engineFailed = r => r.last_stop_reason === 'error' || /provider errors/.test(r.abort_reason ?? '');
// The budget a session is steered at: the configured share of the window, or less
// once the engine has said it cannot hold more (see capacityHeadroom).
const contextBudgetTokens = requested => Math.min(requested ?? Math.floor(L.config.contextWindow * P.contextBudget), L.context_cap ?? Infinity);
const contextStopped = r => /^context budget exceeded/.test(r.abort_reason ?? '');
async function session(role, prompt, ctx = {}) {
  for (let retry = 0, capacity = 0, continued = 0, imageStopped = null; ; ) {
    const resumed = L.pending_continuation;
    if (resumed) { delete L.pending_continuation; saveLedger(); }
    const note = imageStopped ? imageContinuation(imageStopped) : CONTINUATION;
    const record = await sessionOnce(role, continued || resumed ? `${prompt}\n\n${note}` : prompt, ctx);
    if (stopping) {
      L.pending_continuation = true; L.paused_at = new Date().toISOString(); saveLedger();
      journal({ type: 'paused', label: record.label, role, task: ctx.task ?? null, attempt: ctx.attempt ?? null });
      throw new Paused(record.label);
    }
    // The steer alone does not stop a model in the middle of a step, and past the
    // engine's maximum the session ends in an error. A session stopped at its
    // context budget keeps its work; a fresh session picks it up from the repository.
    if (contextStopped(record) && !stopping && continued < P.contextContinuations) {
      continued++;
      journal({ type: 'context-continuation', label: record.label, context_peak: record.context_peak, context_budget: record.context_budget, continuation: continued });
      log(`  ${record.label}: stopped at its context budget; a fresh session continues the same work (${continued}/${P.contextContinuations}, not counted as an attempt)`);
      continue;
    }
    // A refused image belongs to the session, like its context: a fresh session
    // without it continues the work, told what was refused and how to avoid it.
    if (record.image_refusal && !stopping && continued < P.contextContinuations) {
      continued++; imageStopped = record.image_refusal;
      journal({ type: 'image-continuation', label: record.label, refusal: record.image_refusal.slice(0, 300), continuation: continued });
      log(`  ${record.label}: the engine refused an image the session read; a fresh session continues the same work (${continued}/${P.contextContinuations}, not counted as an attempt)`);
      continue;
    }
    imageStopped = null;
    if (!engineFailed(record) || stopping) return record;
    // A prompt larger than the engine's current memory allows fails the same way
    // after any wait. If the reported maximum is still usable, lower the budget and
    // rerun at once; a tiny maximum means something else holds the memory, so wait.
    const limit = Math.min(...(record.capacity_limits ?? []));
    if (Number.isFinite(limit) && limit >= P.contextFloor && capacity < P.capacityRetries) {
      const cap = Math.floor(limit * P.capacityHeadroom);
      if (cap < (L.context_cap ?? Infinity)) { L.context_cap = cap; saveLedger(); }
      capacity++;
      journal({ type: 'context-cap', label: record.label, engine_max_context: limit, context_cap: L.context_cap, retry: capacity });
      log(`  ${record.label}: the engine can hold about ${limit} tokens now; context budget lowered to ${L.context_cap}; rerunning the same session (${capacity}/${P.capacityRetries}, not counted as an attempt)`);
      await new Promise(r => setTimeout(r, 10_000));
      continue;
    }
    if (capacity >= P.capacityRetries && Number.isFinite(limit) && limit >= P.contextFloor) {
      throw new EngineUnavailable(`sessions still overflow the engine's memory after the context budget was lowered to ${L.context_cap} (last: ${record.label})`);
    }
    if (retry >= P.engineRetries) throw new EngineUnavailable(`${retry + 1} sessions in a row ended with engine errors (last: ${record.label})`);
    const wait = Math.min(P.engineRetryBaseSec * 2 ** retry, 900);
    journal({ type: 'engine-retry', label: record.label, retry: retry + 1, wait_s: wait });
    log(`  ${record.label}: engine error; retrying the same session in ${wait} s (${retry + 1}/${P.engineRetries}, not counted as an attempt)`);
    if (retry === 0) notify('The engine is failing (often memory). Free memory; the run retries the same session.');
    await new Promise(r => setTimeout(r, wait * 1000));
    retry++;
    for (let i = 0; i < 30 && !stopping; i++) {
      try { await ensureServer(L.config); break; } catch { await new Promise(r => setTimeout(r, 20_000)); }
    }
  }
}
async function sessionOnce(role, prompt, ctx = {}) {
  const pre = changedPaths();
  if (pre.length) commit(`task-loop: bookkeeping before ${role}`);
  L.session_seq++;
  const label = `${String(L.session_seq).padStart(3, '0')}-${role}${ctx.task ? `-${ctx.task}` : ''}`;
  L.inflight = { label, role, task: ctx.task ?? null, started_at: new Date().toISOString() };
  saveLedger();
  writeFileSync(join(privateDir, `${label}.prompt.md`), prompt);
  log(`Session ${label} started`);
  const watch = { turns: ctx.turns ?? P.turnBudget, contextTokens: contextBudgetTokens(ctx.contextTokens),
    idle: role === 'builder' || role === 'bootstrap' };
  await sampler?.begin();
  const m = await runPi(L.config, { prompts: [prompt], cwd: project, sessionDir: join(privateDir, 'sessions'), label, watch });
  const memory = await sampler?.end() ?? null;
  const reverted = enforceOwnership(role, ctx);
  checkSkills();
  if (reverted.length) log(`  ${label}: reverted out-of-role changes: ${reverted.join(', ')}`);
  const ok = sessionOk(m);
  const record = { type: 'session', label, role, task: ctx.task ?? null, attempt: ctx.attempt ?? null, ok,
    abort_reason: m.abortReason, exit_code: m.exitCode, last_stop_reason: m.lastStopReason,
    turns: m.turns, tool_calls: m.toolCalls, writes: m.writes, usage: m.usage, reasoning_chars: m.reasoningChars,
    context_peak: m.contextPeak, context_budget: watch.contextTokens, compactions: m.compactions, provider_errors: m.providerErrors,
    capacity_limits: m.capacityLimits, image_refusal: m.imageRefusal ?? null, big_outputs: m.bigOutputs,
    watchdog: m.events, reverted, prompt_chars: prompt.length, duration_ms: m.durationMs, engine: engineSummary(m.requests), memory };
  journal(record);
  L.inflight = null;
  saveLedger();
  log(`Session ${label} ${ok ? 'settled' : `failed (${m.abortReason ?? m.lastStopReason ?? m.exitCode})`} after ${m.turns} turns, peak context ${m.contextPeak}`);
  return { ...record, metrics: m };
}
// Medians of engine-matched requests and the output-weighted decode rate
// (total output tokens / total decode seconds).
function engineSummary(requests) {
  const matched = requests.filter(r => r.engine_matched);
  if (!requests.length) return null;
  const median = values => { const v = values.filter(x => Number.isFinite(x)).sort((a, b) => a - b); return v.length ? +(v.length % 2 ? v[(v.length - 1) / 2] : (v[v.length / 2 - 1] + v[v.length / 2]) / 2).toFixed(3) : null; };
  const decodeS = matched.reduce((n, r) => n + r.engine.decode_s, 0);
  const output = matched.reduce((n, r) => n + r.engine.output_tokens, 0);
  return { requests: requests.length, engine_matched: matched.length,
    ttft_median_s: median(matched.map(r => r.engine.ttft_s)),
    decode_tps_median: median(matched.filter(r => r.engine.decode_s > 0).map(r => r.engine.output_tokens / r.engine.decode_s)),
    decode_tps_weighted: decodeS > 0 ? +(output / decodeS).toFixed(2) : null,
    ttft_s_total: +matched.reduce((n, r) => n + r.engine.ttft_s, 0).toFixed(3), decode_s_total: +decodeS.toFixed(3) };
}
function memoryLine() {
  const samples = (readIf(join(privateDir, 'memory.jsonl')) ?? '').split('\n').filter(Boolean).map(l => JSON.parse(l));
  if (!samples.length) return 'Memory: not sampled';
  const last = samples.at(-1), page = last.page_bytes ?? 16384;
  const levels = { 1: 'normal', 2: 'warning', 4: 'critical' };
  const worst = Math.max(...samples.map(s => s.pressure_level ?? 0));
  const swapIn = samples.reduce((n, s) => n + (s.swapins_delta ?? 0), 0) * page / 2 ** 30;
  const freeMin = Math.min(...samples.map(s => s.free_gib ?? Infinity));
  return `Memory: pressure ${levels[last.pressure_level] ?? last.pressure_level} now, worst ${levels[worst] ?? worst}; swapped in ${swapIn.toFixed(1)} GiB during the run; `
    + `server ${last.server_mem_gib ?? '?'} GiB, wired ${last.wired_gib ?? '?'} GiB, free ${last.free_gib} GiB now (min ${Number.isFinite(freeMin) ? freeMin : '?'}); ${samples.length} samples`;
}
function engineLine() {
  const requests = (readIf(join(privateDir, 'requests.jsonl')) ?? '').split('\n').filter(Boolean).map(l => JSON.parse(l));
  const e = engineSummary(requests);
  if (!e) return 'Engine: no requests recorded';
  const cached = requests.reduce((n, r) => n + r.cached_tokens, 0) / Math.max(1, requests.reduce((n, r) => n + r.prompt_tokens, 0));
  return `Engine: ${e.requests} requests (${e.engine_matched} with engine timings), prompt cache ${(cached * 100).toFixed(1)}%`
    + (e.engine_matched ? `, TTFT median ${e.ttft_median_s} s, decode ${e.decode_tps_weighted} tok/s (output-weighted)` : '');
}
function verdict(path) {
  const text = readIf(path);
  const match = text?.match(/^\s*Verdict:\s*(ACCEPT|CHANGES)\b/m);
  return match ? { verdict: match[1], text } : { verdict: null, text };
}

// ---------- phases ----------

function validateBootstrap() {
  const errors = [];
  const reqs = specRequirements();
  if (!reqs.size) errors.push('.agent/spec.md has no requirement lines like "- R1: ..."');
  if (!existsSync(join(agentDir, 'map.md'))) errors.push('.agent/map.md is missing');
  const { tasks, errors: taskErrors } = loadTasks();
  errors.push(...taskErrors);
  if (!tasks.size) errors.push('no task cards in .agent/tasks/');
  const cov = coverage(tasks, { doneOnly: false });
  if (cov.missing.length) errors.push(`requirements without a task: ${cov.missing.join(', ')}`);
  const first = runVerify(['--all']);
  if (!first.ok) errors.push(`./verify --all does not pass:\n${first.summary}`);
  else {
    const second = runVerify(['--all']);
    if (!second.ok || JSON.stringify(second.json) !== JSON.stringify(first.json)) {
      errors.push(`./verify --all is not deterministic:\nrun 1: ${JSON.stringify(first.json)}\nrun 2: ${second.summary}`);
    }
  }
  return { errors, tasks };
}
async function bootstrapPhase() {
  if (L.bootstrap_attempts >= P.maxBootstrapAttempts) return block(`bootstrap failed ${L.bootstrap_attempts} times: ${L.last_errors?.join('; ')}`);
  L.bootstrap_attempts++;
  const extra = [['Original brief (BRIEF.md)', read(join(project, 'BRIEF.md'))]];
  if (L.last_errors?.length) extra.push(['Problems the orchestrator found in the previous bootstrap session', L.last_errors.map(e => `- ${e}`).join('\n')]);
  const s = await session('bootstrap', briefing('bootstrap', { fields: { Attempt: `${L.bootstrap_attempts} of ${P.maxBootstrapAttempts}` }, extra }));
  const { errors, tasks } = validateBootstrap();
  // A file the session wrote where it may not was reverted after it. When the bootstrap
  // fails, say so: the next session otherwise sees only the failure the revert caused
  // (verify calling a file that is gone) and repeats it.
  if (errors.length && s.reverted?.length) errors.unshift(revertedNote('bootstrap', s.reverted));
  journal({ type: 'bootstrap-validation', attempt: L.bootstrap_attempts, errors, session_ok: s.ok });
  if (errors.length) { L.last_errors = errors; saveLedger(); log(`Bootstrap not accepted: ${errors.length} problem(s)`); return; }
  for (const t of tasks.values()) setCardFields(t.id, { status: 'todo', attempts: 0 });
  delete L.last_errors;
  L.phase = 'LOOP';
  L.checkpoint = commit('task-loop: bootstrap accepted');
  saveLedger();
  log(`Bootstrap accepted: ${tasks.size} tasks, ${specRequirements().size} requirements`);
}
async function plannerSession(role, { task = null, reason }) {
  const before = commit(`task-loop: bookkeeping before ${role}`);
  const { tasks } = loadTasks();
  const extra = [['Task list', taskTable(tasks)], ['Why this session', reason]];
  if (role === 'split') extra.push(['Failure history', task.history.join('\n\n')]);
  const recent = readIf(journalPath)?.trim().split('\n').slice(-12).map(line => {
    const r = JSON.parse(line);
    return r.type === 'session' ? `${r.label}: ${r.ok ? 'ok' : `failed (${r.abort_reason ?? r.last_stop_reason})`}, ${r.turns} turns` : `${r.type}${r.task ? ` ${r.task}` : ''}${r.outcome ? `: ${r.outcome}` : ''}`;
  }).join('\n');
  extra.push(['Recent orchestrator journal', recent]);
  const planner = readIf(join(agentDir, 'notes', 'PLANNER.md'));
  if (planner) extra.push(['Previous planner notes', planner]);
  await session(role, briefing(role, { fields: task ? { Task: task.id } : {}, task: task ? task.card : null, extra }), { task: task?.id });
  const { tasks: after, errors } = loadTasks();
  const cov = coverage(after, { doneOnly: false });
  if (cov.missing.length) errors.push(`requirements left uncovered: ${cov.missing.join(', ')}`);
  for (const t of after.values()) {
    const old = tasks.get(t.id);
    if (!old && t.status !== 'todo') setCardFields(t.id, { status: 'todo', attempts: 0 });
    if (old && old.status !== t.status && !(t.status === 'dropped' && ['todo', 'blocked'].includes(old.status))) setCardFields(t.id, { status: old.status });
  }
  for (const id of tasks.keys()) if (!after.has(id)) errors.push(`card ${id} was deleted`);
  if (errors.length) {
    git('checkout', before, '--', '.agent/tasks'); git('clean', '-q', '-fd', '--', '.agent/tasks');
    journal({ type: `${role}-rejected`, task: task?.id ?? null, errors });
    log(`${role} rejected: ${errors.join('; ')}`);
    return false;
  }
  commit(`task-loop: ${role}${task ? ` ${task.id}` : ''}`);
  journal({ type: `${role}-accepted`, task: task?.id ?? null, added: [...after.keys()].filter(id => !tasks.has(id)) });
  return true;
}
function splitDepth(id) { return (id.match(/[a-z]+$/)?.[0].length) ?? 0; }
async function escalate(tasks, task, history) {
  resetTo(L.current.base);
  if (splitDepth(task.id) < P.maxSplitDepth) {
    const card = { ...task, file: task.file };
    const ok = await plannerSession('split', { task: { id: task.id, card, history }, reason: `${task.id} failed ${P.maxAttempts} attempts.` });
    const children = [...loadTasks().tasks.values()].filter(t => t.parent === task.id);
    if (ok && children.length >= 2) {
      setCardFields(task.id, { status: 'split' });
      L.checkpoint = commit(`task-loop: ${task.id} split into ${children.map(c => c.id).join(', ')}`);
      journal({ type: 'task', task: task.id, outcome: 'split', children: children.map(c => c.id) });
      return;
    }
    if (ok) resetTo(L.current.base);
  }
  setCardFields(task.id, { status: 'blocked' });
  L.checkpoint = commit(`task-loop: ${task.id} blocked`);
  journal({ type: 'task', task: task.id, outcome: 'blocked', history });
  log(`${task.id} blocked`);
}
async function runTask(tasks, task) {
  if (L.current?.task !== task.id) {
    setCardFields(task.id, { status: 'active' });
    L.current = { task: task.id, base: commit(`task-loop: start ${task.id}`), history: [], failure: null };
    saveLedger();
  }
  for (;;) {
    const attempts = loadTasks().tasks.get(task.id).attempts;
    if (attempts >= P.maxAttempts) { await escalate(tasks, task, L.current.history); L.current = null; saveLedger(); return; }
    const attempt = attempts + 1;
    setCardFields(task.id, { attempts: attempt });
    const card = loadTasks().tasks.get(task.id);
    const extra = [];
    const notes = depNotes(tasks, card);
    if (notes && P.briefing) extra.push(['Notes from earlier tasks', notes]);
    if (L.current.failure) {
      extra.push(['Previous attempt failed', L.current.failure]);
      const diff = git('diff', '--stat', L.current.base, '--', '.', ':(exclude).agent').trim();
      if (diff) extra.push(['Changes already made since the task started (kept)', diff]);
    }
    extra.push(['Latest ./verify --all', runVerify(['--all']).summary]);
    const fields = { Task: task.id, Attempt: `${attempt} of ${P.maxAttempts}` };
    let s;
    try {
      s = await session('builder', briefing('builder', { fields, task: card, extra }),
        { task: task.id, attempt, turns: card.budget.turns, contextTokens: card.budget.tokens });
    } catch (error) {
      // A paused attempt, or one cut short by the engine, is not a failed one: give it back.
      if (error instanceof Paused || error instanceof EngineUnavailable) setCardFields(task.id, { attempts: attempt - 1 });
      throw error;
    }
    const all = runVerify(['--all']);
    const own = runVerify(['--task', task.id]);
    const ownChecks = own.json?.passed ?? 0;
    let failure = null;
    if (!all.ok) failure = `./verify --all fails after the attempt:\n${all.summary}`;
    else if (!own.ok || ownChecks < 1) failure = `./verify --task ${task.id} must run at least one passing check with ID prefix "${task.id}." (got ${ownChecks}).\n${own.summary}`;
    const stopped = s.ok ? null : `${s.abort_reason ?? s.last_stop_reason}`;
    if (stopped && failure) failure = `The session was stopped: ${stopped}.\n${failure}`;
    // A stopped session whose work still passes goes on to review, but is recorded as such.
    if (stopped && !failure) (L.current.stopped ??= []).push({ attempt, reason: stopped });
    journal({ type: 'verify', task: task.id, attempt, all_ok: all.ok, task_ok: own.ok, task_checks: ownChecks, all: all.json, task_json: own.json });
    if (failure) {
      L.current.failure = failure;
      L.current.history.push(`Attempt ${attempt}: ${clip(failure, 1500)}`);
      saveLedger();
      continue;
    }
    commit(`task ${task.id}: ${card.title} (attempt ${attempt})`);
    let reviewVerdict = null;
    if (P.review) {
      const diffBase = L.current.base;
      let diff = git('diff', diffBase, 'HEAD', '--', '.', ':(exclude).agent');
      if (diff.length > P.diffLimitBytes) diff = `${git('diff', '--stat', diffBase, 'HEAD').trim()}\n\n[Diff of ${diff.length} bytes clipped. Read the changed files directly.]\n\n${diff.slice(0, P.diffLimitBytes)}`;
      const reviewPath = join(agentDir, 'reviews', `${task.id}.md`);
      rmSync(reviewPath, { force: true });
      mkdirSync(dirname(reviewPath), { recursive: true });
      // A reviewer that ends without a verdict (for example, stuck repeating
      // itself in its reasoning) gets one fresh retry before verify alone decides.
      let v = { verdict: null };
      for (let review = 1; review <= 2 && !v.verdict; review++) {
        if (review > 1) log(`  reviewer left no verdict for ${task.id}; retrying the review`);
        await session('reviewer', briefing('reviewer', { fields: { Task: task.id, 'Review file': rel(reviewPath) }, task: card,
        extra: [...(stopped && !failure ? [['Builder session', `The orchestrator stopped the builder session (${stopped}). The checks pass, but look for unfinished work.`]] : []),
          ['Builder notes', noteFor(task.id)], ['Diff since the task started', `\`\`\`diff\n${diff}\n\`\`\``],
          ['Verify results', `${all.summary}\n\n${own.summary}`]] }), { task: task.id, attempt, turns: 25 });
        v = verdict(reviewPath);
        journal({ type: 'review', task: task.id, attempt, review, verdict: v.verdict });
      }
      if (v.verdict === 'CHANGES') {
        L.current.failure = `The reviewer requested changes:\n${clip(v.text, 3000)}`;
        L.current.history.push(`Attempt ${attempt}: review requested changes:\n${clip(v.text, 1500)}`);
        commit(`task-loop: review ${task.id} attempt ${attempt}`);
        saveLedger();
        continue;
      }
      if (!v.verdict) log(`  reviewer left no verdict for ${task.id}; accepting on verify alone`);
      reviewVerdict = v.verdict;
    }
    setCardFields(task.id, { status: 'done' });
    L.checkpoint = commit(`task-loop: ${task.id} done`);
    const stoppedSessions = L.current.stopped ?? [];
    journal({ type: 'task', task: task.id, outcome: 'done', attempts: attempt, reviewed: !!reviewVerdict, stopped_sessions: stoppedSessions });
    L.current = null;
    L.done_since_replan++;
    L.stuck_replanned = false;
    saveLedger();
    log(`${task.id} done (${attempt} attempt${attempt > 1 ? 's' : ''}${stoppedSessions.length ? `; ${stoppedSessions.length} stopped builder session${stoppedSessions.length > 1 ? 's' : ''} passed verify` : ''})`);
    return;
  }
}
async function loopPhase() {
  const { tasks, errors } = loadTasks();
  if (errors.length) return block(`task cards are invalid: ${errors.join('; ')}`);
  if (P.replanEvery && L.done_since_replan >= P.replanEvery && !L.current) {
    L.done_since_replan = 0; saveLedger();
    await plannerSession('replan', { reason: `Periodic replan after ${P.replanEvery} completed tasks.` });
    return;
  }
  const ready = readyTasks(tasks);
  if (ready.length) return runTask(tasks, ready[0]);
  const open = [...tasks.values()].filter(t => !resolved(tasks, t.id) && t.status !== 'split');
  if (!open.length) { L.phase = 'FINAL'; saveLedger(); return; }
  if (!L.stuck_replanned) {
    L.stuck_replanned = true; saveLedger();
    await plannerSession('replan', { reason: `No task is ready. Blocked or unreachable: ${open.map(t => `${t.id} [${t.status}]`).join(', ')}. Route the work around the blocked tasks.` });
    return;
  }
  return block(`no task is ready; open tasks: ${open.map(t => `${t.id} [${t.status}]`).join(', ')}`);
}
async function finalPhase() {
  const { tasks } = loadTasks();
  const all = runVerify(['--all']);
  const cov = coverage(tasks, { doneOnly: true });
  const report = `Requirements: ${cov.total}. Manual: ${cov.manual.join(', ') || 'none'}. Not covered by a done task: ${cov.missing.join(', ') || 'none'}.`;
  const reviewPath = join(agentDir, 'reviews', 'FINAL.md');
  rmSync(reviewPath, { force: true });
  mkdirSync(dirname(reviewPath), { recursive: true });
  await session('final-reviewer', briefing('final-reviewer', { fields: { Cycle: L.final_cycles + 1 },
    extra: [['Task list', taskTable(tasks)], ['Coverage', report], ['./verify --all', all.summary]] }), { turns: 60 });
  const v = verdict(reviewPath);
  const added = [...loadTasks().tasks.values()].filter(t => !tasks.has(t.id));
  for (const t of added) setCardFields(t.id, { status: 'todo', attempts: 0 });
  L.final_cycles++;
  commit(`task-loop: final review ${L.final_cycles}`);
  journal({ type: 'final-review', cycle: L.final_cycles, verdict: v.verdict, verify_ok: all.ok, coverage: cov, added: added.map(t => t.id) });
  if (v.verdict === 'ACCEPT' && all.ok && !cov.missing.length) return finish('COMPLETE');
  if (added.length && L.final_cycles < P.maxFinalCycles) { L.phase = 'LOOP'; saveLedger(); log(`Final review added ${added.length} task(s)`); return; }
  if (L.final_cycles >= P.maxFinalCycles) return finish('COMPLETE_WITH_GAPS');
  log('Final review did not produce a verdict or new tasks; retrying');
}
// A desktop notification, so the person sharing the machine knows a run holds it.
function notify(message) {
  if (process.platform !== 'darwin' || !P?.notify) return;
  const quote = text => `"${String(text).replace(/[\\"]/g, '\\$&').slice(0, 200)}"`;
  spawn('osascript', ['-e', `display notification ${quote(message)} with title "Task loop" subtitle ${quote(basename(dirname(project)))}`],
    { stdio: 'ignore', detached: true }).unref();
}
function finish(status) {
  L.phase = status; L.ended_at = new Date().toISOString(); L.checkpoint = commit(`task-loop: ${status}`);
  saveLedger(); journal({ type: 'run', outcome: status }); log(`Run finished: ${status}`);
  notify(`Finished: ${status}. The engine is free.`);
}
function block(reason) {
  L.phase_before_block = L.phase; L.phase = 'BLOCKED'; L.blocked_reason = reason;
  saveLedger(); journal({ type: 'run', outcome: 'BLOCKED', reason }); log(`BLOCKED: ${reason}`);
  notify(`Blocked: ${reason}`);
}

// ---------- commands ----------

function parseSets() {
  const params = { ...DEFAULT_PARAMS };
  for (const pair of options('--set')) {
    const at = pair.indexOf('=');
    const key = pair.slice(0, at);
    if (!(key in DEFAULT_PARAMS)) fail(`Unknown parameter ${key}. Known: ${Object.keys(DEFAULT_PARAMS).join(', ')}`);
    params[key] = parseValue(pair.slice(at + 1));
  }
  return params;
}
function init() {
  const brief = option('--brief');
  const model = option('--model');
  if (!model) fail('--model provider/id is required');
  if (existsSync(ledgerPath)) fail(`${rel(ledgerPath)} already exists; this project is already initialized`);
  mkdirSync(project, { recursive: true });
  if (brief) copyFileSync(resolve(brief), join(project, 'BRIEF.md'));
  if (!existsSync(join(project, 'BRIEF.md'))) fail('Pass --brief FILE or put BRIEF.md in the project');
  if (existsSync(join(project, 'AGENTS.md')) && read(join(project, 'AGENTS.md')) !== read(join(TEMPLATES, 'AGENTS.md'))) {
    fail('The project already has a different AGENTS.md. Move it away first; the task loop owns that file.');
  }
  copyFileSync(join(TEMPLATES, 'AGENTS.md'), join(project, 'AGENTS.md'));
  mkdirSync(join(agentDir, 'prompts'), { recursive: true });
  for (const dir of ['tasks', 'notes', 'reviews', 'private']) mkdirSync(join(agentDir, dir), { recursive: true });
  for (const role of ROLES) copyFileSync(join(TEMPLATES, 'prompts', `${role}.md`), join(agentDir, 'prompts', `${role}.md`));
  const ignore = join(project, '.gitignore');
  const wanted = ['.agent/private/', '.agent/run.json', '.agent/journal.jsonl', '.agent/skills/'];
  const current = readIf(ignore) ?? '';
  const missing = wanted.filter(line => !current.split('\n').includes(line));
  if (missing.length) writeFileSync(ignore, `${current}${current && !current.endsWith('\n') ? '\n' : ''}${missing.join('\n')}\n`);
  if (!existsSync(join(project, '.git'))) git('init', '-q');
  P = parseSets();
  L = { schema_version: 1, created_at: new Date().toISOString(), brief_sha256: sha(read(join(project, 'BRIEF.md'))),
    templates_sha256: sha(ROLES.map(r => read(join(TEMPLATES, 'prompts', `${r}.md`))).join('') + read(join(TEMPLATES, 'AGENTS.md'))),
    orchestrator_sha256: sha(read(fileURLToPath(import.meta.url))),
    config: modelConfig(model, option('--thinking', 'medium')), params: P, skills: options('--skill').map(installSkill),
    phase: 'BOOTSTRAP', session_seq: 0, bootstrap_attempts: 0, done_since_replan: 0, stuck_replanned: false,
    final_cycles: 0, current: null, inflight: null, preflight: null };
  writeFileSync(journalPath, '');
  saveLedger();
  L.checkpoint = commit('task-loop: init');
  saveLedger();
  log(`Initialized ${project} for ${model}`);
}
async function run() {
  if (!existsSync(ledgerPath)) fail(`No ${rel(ledgerPath)}; run init first`);
  L = JSON.parse(read(ledgerPath));
  P = { ...DEFAULT_PARAMS, ...L.params };
  mkdirSync(join(privateDir, 'sessions'), { recursive: true });
  if (sha(read(join(project, 'BRIEF.md'))) !== L.brief_sha256) fail('BRIEF.md changed since init');
  checkSkills();
  const now = modelConfig(L.config.model, L.config.thinking);
  for (const key of ['provider', 'id', 'baseUrl', 'contextWindow', 'pi_version']) {
    if (now[key] !== L.config[key]) fail(`Pinned ${key} changed (${L.config[key]} -> ${now[key]})`);
  }
  if (L.phase === 'BLOCKED' && flag('--resume')) { L.phase = L.phase_before_block; delete L.blocked_reason; saveLedger(); }
  // Picks up changes to the shared rules and role prompts in a run that has already
  // started; the journal records it, because it changes the run's configuration.
  if (flag('--refresh-templates')) {
    copyFileSync(join(TEMPLATES, 'AGENTS.md'), join(project, 'AGENTS.md'));
    for (const role of ROLES) copyFileSync(join(TEMPLATES, 'prompts', `${role}.md`), join(agentDir, 'prompts', `${role}.md`));
    const templates = sha(ROLES.map(r => read(join(TEMPLATES, 'prompts', `${r}.md`))).join('') + read(join(TEMPLATES, 'AGENTS.md')));
    journal({ type: 'templates-refreshed', from: L.templates_sha256 ?? null, to: templates });
    L.templates_sha256 = templates; saveLedger();
    // Commit only these files; work left by an interrupted session stays for the next one.
    git('add', '--', 'AGENTS.md', '.agent/prompts');
    if (git('diff', '--cached', '--name-only').trim()) git('-c', 'user.name=Task Loop', '-c', 'user.email=task-loop@localhost', 'commit', '-q', '-m', 'task-loop: refresh shared rules and prompts');
  }
  const orchestrator = sha(read(fileURLToPath(import.meta.url)));
  if (orchestrator !== L.orchestrator_sha256) {
    journal({ type: 'orchestrator-changed', from: L.orchestrator_sha256 ?? null, to: orchestrator });
    L.orchestrator_sha256 = orchestrator; saveLedger();
  }
  if (!['BOOTSTRAP', 'LOOP', 'FINAL'].includes(L.phase)) { log(`Run is ${L.phase}${L.blocked_reason ? `: ${L.blocked_reason}` : ''}`); return; }
  const others = otherPiInProject();
  if (others.length) fail(`Other Pi processes are working in this project (PIDs ${others.join(', ')}); close them first`);
  await ensureServer(L.config);
  if (!L.preflight?.ok) {
    L.preflight = await preflight(L.config);
    saveLedger();
    log(`Preflight: reasoning=${L.preflight.reasoning} cache=${L.preflight.cache}`);
    if (!L.preflight.ok && !flag('--allow-weak-engine')) {
      fail('Engine preflight failed (see .agent/run.json "preflight"). Fix thinking/prefix cache, or pass --allow-weak-engine to record the run anyway.');
    }
    if (!L.preflight.ok) { L.preflight.accepted_weak = true; L.preflight.ok = true; saveLedger(); }
  }
  if (L.inflight) {
    journal({ type: 'interrupted', ...L.inflight });
    log(`Previous session ${L.inflight.label} was interrupted; continuing`);
    L.inflight = null; saveLedger();
  }
  if (L.paused_at) {
    journal({ type: 'resumed', paused_at: L.paused_at });
    log(`Resuming after a pause since ${L.paused_at}`);
    delete L.paused_at; saveLedger();
  }
  sampler = memorySampler(L.config);
  sampler?.start();
  notify(`Started (${L.phase}). It holds the engine and most memory until it ends.`);
  try {
    while (!stopping && ['BOOTSTRAP', 'LOOP', 'FINAL'].includes(L.phase)) {
      if (L.phase === 'BOOTSTRAP') await bootstrapPhase();
      else if (L.phase === 'LOOP') await loopPhase();
      else await finalPhase();
    }
    if (stopping && ['BOOTSTRAP', 'LOOP', 'FINAL'].includes(L.phase)) { // stopped between sessions
      L.paused_at = new Date().toISOString(); saveLedger();
      journal({ type: 'paused', label: null });
      log('PAUSED between sessions. Run again (no --resume needed) to continue.');
    }
  } catch (error) {
    if (error instanceof Paused) {
      log(`PAUSED during ${error.message}; its changes are kept. Run again (no --resume needed) to continue.`);
      notify('Paused. The engine is free; run again to continue.');
      return;
    }
    if (!(error instanceof EngineUnavailable)) throw error;
    block(`engine unavailable: ${error.message}. Fix the engine, then run with --resume`);
  } finally { await sampler?.stop(); }
}
function status() {
  if (!existsSync(ledgerPath)) { process.stdout.write('Not initialized.\n'); return; }
  L = JSON.parse(read(ledgerPath));
  const { tasks } = loadTasks();
  const counts = {};
  for (const t of tasks.values()) counts[t.status] = (counts[t.status] ?? 0) + 1;
  const sessions = (readIf(journalPath) ?? '').split('\n').filter(Boolean).map(l => JSON.parse(l)).filter(r => r.type === 'session');
  process.stdout.write([
    `Phase: ${L.phase}${L.blocked_reason ? ` (${L.blocked_reason})` : ''}`,
    `Model: ${L.config.model} (thinking ${L.config.thinking}), Pi ${L.config.pi_version}`,
    `Preflight: ${L.preflight ? `reasoning=${L.preflight.reasoning} cache=${L.preflight.cache}${L.preflight.accepted_weak ? ' (accepted weak)' : ''}` : 'not run'}`,
    `Tasks: ${Object.entries(counts).map(([k, v]) => `${k} ${v}`).join(', ') || 'none'}`,
    `Current: ${L.current ? `${L.current.task} (${L.current.history.length} failed attempts)` : 'none'}${L.inflight ? `, in flight ${L.inflight.label}` : ''}`,
    `Context budget: ${Math.floor(L.config.contextWindow * (L.params?.contextBudget ?? DEFAULT_PARAMS.contextBudget))} tokens${L.context_cap ? `, lowered to ${L.context_cap} after the engine ran out of memory` : ''}`,
    `Sessions: ${sessions.length} (${sessions.filter(s => !s.ok).length} stopped or failed), compactions ${sessions.reduce((n, s) => n + s.compactions, 0)}`,
    engineLine(),
    memoryLine(),
    ''
  ].join('\n'));
}

process.on('SIGINT', () => { stopping = true; child?.kill('SIGINT'); });
process.on('SIGTERM', () => { stopping = true; child?.kill('SIGTERM'); });
try {
  if (command === 'init') init();
  else if (command === 'run') await run();
  else if (command === 'status') status();
  else if (command === 'preflight') {
    const config = modelConfig(option('--model') ?? fail('--model is required'), option('--thinking', 'medium'));
    await ensureServer(config);
    process.stdout.write(`${JSON.stringify(await preflight(config), null, 2)}\n`);
  } else {
    process.stdout.write('Usage: task-loop.mjs init|run|status|preflight --project DIR [options]\nSee docs/TASK-LOOP-ORCHESTRATOR.md.\n');
    process.exitCode = command ? 1 : 0;
  }
} catch (error) {
  log(`ERROR: ${error.message}`);
  if (L && existsSync(ledgerPath)) { L.last_error = error.message; L.inflight = null; saveLedger(); }
  if (command === 'run') notify(`Stopped with an error: ${error.message}`);
  process.exitCode = 1;
}
