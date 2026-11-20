// Node digest vs Rust digest-rs parity check with fake Prometheus / Grafana / Loki / Discord.
//   EXPO_MONITORING_DIR=../Expo-Monitoring node dev/verify-digest-parity.mjs
// Each scenario runs the Node version first, reads the `now` it used from its Prometheus `time` parameter,
// then runs Rust with DIGEST_NOW_MS set to that instant, so outputs can be compared byte for byte.
// Compared: exit code, stdout, stderr, every request the fake sources received, and every Discord payload
// (multipart parsed into parts, since the boundary is random).
import http from 'node:http';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const NODE_ENTRY = path.join(path.resolve(process.env.EXPO_MONITORING_DIR ?? '../Expo-Monitoring'), 'digest/index.mjs');
const RUST_BIN = path.resolve(process.env.DIGEST_RS_BIN ?? path.join(ROOT, 'digest-rs/target/debug/digest-rs'));

const PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==', 'base64');
const SECRET = 'SECRET-WEBHOOK-TOKEN-123';
const vec = (rows) => ({ status: 'success', data: { resultType: 'vector', result: rows.map(([m, v]) => ({ metric: m, value: [1, String(v)] })) } });
let mode = {};
const requests = [];
const hooks = [];
let hookCount = 0;

function serve(name, handler) {
  return new Promise((resolve) => {
    const s = http.createServer(async (req, res) => {
      const chunks = [];
      for await (const c of req) chunks.push(c);
      const body = Buffer.concat(chunks);
      if (name !== 'discord') requests.push(`${name} ${req.method} ${req.url} auth=${req.headers.authorization ?? ''} type=${req.headers['content-type'] ?? ''} ${body}`);
      await handler(req, res, body);
    }).listen(0, '127.0.0.1', () => resolve({ s, url: `http://127.0.0.1:${s.address().port}` }));
  });
}
const json = (res, body, status = 200, headers = {}) => { res.writeHead(status, { 'content-type': 'application/json', ...headers }); res.end(JSON.stringify(body)); };

// Same shapes as dev/verify-digest.mjs, plus failure switches. Times derive from the request, never the clock.
const prom = await serve('prom', (req, res) => {
  if (mode.prom500) return json(res, {}, 500);
  const q = new URL(req.url, 'http://x').searchParams.get('query') ?? '';
  const row = (v, svc = 'expo-form') => [{ service: svc }, v];
  if (q.startsWith('sum by (service) (increase(http_server_requests_seconds_count{')) return json(res, vec([row(40), row(2, 'expo-gateway')]));
  if (q.startsWith('sum by (service) (increase')) return json(res, vec([row(1000), row(5000, 'expo-gateway')]));
  if (q.startsWith('histogram')) return json(res, vec([row(1.4), row(0.3, 'expo-gateway')]));
  if (q.startsWith('count by (service) (up{job="eureka-services"} == 1')) return json(res, vec([row(2), row(1, 'expo-gateway')]));
  if (q.startsWith('count by (service) (up')) return json(res, vec([row(2), row(2, 'expo-gateway')]));
  if (q.startsWith('sum by (status) (form_outbox_records')) return json(res, vec([[{ status: 'pending' }, 7], [{ status: 'failed' }, 1]]));
  if (q.includes('form_outbox_oldest')) return json(res, vec([[{}, 120]]));
  if (q.includes('form_db_pool')) return json(res, vec([]));
  if (q.includes('ALERTS')) return json(res, vec([[{ alertname: 'ServiceHigh5xxRate', severity: 'warning' }, 1]]));
  return json(res, vec([]));
});

const grafana = await serve('grafana', (req, res, body) => {
  const u = new URL(req.url, 'http://x');
  if (u.pathname === '/api/prometheus/grafana/api/v1/rules') {
    if (mode.rules500) return json(res, {}, 500);
    return json(res, { status: 'success', data: { groups: [{ name: 'g', rules: [{ name: 'Form outbox stuck', state: 'firing', health: 'ok', labels: { severity: 'critical' } }, { name: 'ok-rule', state: 'inactive', health: 'ok' }] }] } });
  }
  if (u.pathname === '/api/annotations') {
    const to = Number(u.searchParams.get('to'));
    return json(res, u.searchParams.get('tags') === 'deploy'
      ? [{ time: to - 3600e3, text: 'expo-form v1.2.3 deployed by dev@school.kr', tags: ['deploy'] }]
      : [{ alertName: 'Form outbox stuck', newState: 'Alerting' }]);
  }
  if (u.pathname === '/api/dashboards/uid/msa-overview') return json(res, { dashboard: { uid: 'msa-overview', title: 'x', templating: { list: [{ name: 'service', allValue: '.+' }] }, panels: [{ id: 7, type: 'timeseries', datasource: { type: 'prometheus', uid: 'prometheus' }, targets: [{ refId: 'A', expr: 'x{service=~"$service"}' }] }] } });
  if (u.pathname === '/api/ds/query') return json(res, { results: { A: { frames: [{ schema: { refId: 'A', fields: [] }, data: { values: [] } }] } } });
  if (u.pathname === '/api/snapshots' && req.method === 'POST') {
    if (mode.snapshot500) return json(res, {}, 500);
    return json(res, { key: 'snapkey123', url: 'http://internal:3000/dashboard/snapshot/snapkey123', deleteKey: 'DELETEKEY' });
  }
  if (u.pathname.startsWith('/render/d-solo/')) {
    if (mode.render10fail && u.searchParams.get('panelId') === '10') return json(res, {}, 502);
    res.writeHead(200, { 'content-type': 'image/png' });
    return res.end(PNG);
  }
  return json(res, {}, 404);
});

const loki = await serve('loki', (req, res) => {
  if (mode.loki500) return json(res, {}, 500);
  return json(res, { status: 'success', data: { resultType: 'streams', result: [{ stream: { level: 'error' }, values: [
    ['1', JSON.stringify({ message: 'sms send failed to 010-1234-5678', requestBody: { phone: '01012345678' } })],
    ['2', JSON.stringify({ message: 'sms send failed to 010-9999-8888' })],
    ['3', 'db timeout for user kim@school.kr token=abcdefabcdefabcdefabcdefabcdefabcdef'],
    ['4', `${'긴 메시지 😀 '.repeat(12)}끝`],
  ] }] } });
});

const discord = await serve('discord', (req, res, body) => {
  if (!req.url.includes(SECRET)) return json(res, {}, 404);
  hookCount += 1;
  hooks.push(parseHook(req.headers['content-type'] ?? '', body));
  if (mode.discord500) return json(res, {}, 500);
  if (mode.discord400) return json(res, {}, 400);
  if (mode.discord429 && hookCount === 1) return json(res, {}, 429, { 'retry-after': '1' });
  res.writeHead(204);
  res.end();
});

/** JSON body as text; multipart split into {name, filename, type, body}. */
function parseHook(type, body) {
  const m = type.match(/^multipart\/form-data; boundary=(.+)$/);
  if (!m) return { type, body: body.toString('utf8') };
  const parts = body.toString('latin1').split(`--${m[1]}`).slice(1, -1).map((p) => {
    const [head, ...rest] = p.replace(/^\r\n/, '').split('\r\n\r\n');
    const raw = rest.join('\r\n\r\n').replace(/\r\n$/, '');
    const h = (k) => head.match(new RegExp(`${k}="([^"]*)"`, 'i'))?.[1] ?? null;
    return { name: h('name'), filename: h('filename'), type: head.match(/content-type:\s*(.+)/i)?.[1]?.trim() ?? null, body: Buffer.from(raw, 'latin1').toString(h('filename') ? 'base64' : 'utf8') };
  });
  return { type: 'multipart/form-data', parts };
}

function run(cmd, args, env) {
  return new Promise((resolve) => {
    const p = spawn(cmd, args, { env });
    let stdout = '';
    let stderr = '';
    p.stdout.on('data', (d) => { stdout += d; });
    p.stderr.on('data', (d) => { stderr += d; });
    p.on('close', (code) => resolve({ code, stdout, stderr }));
  });
}

async function capture(cmd, args, env) {
  requests.length = 0;
  hooks.length = 0;
  hookCount = 0;
  const r = await run(cmd, args, env);
  return { exit: r.code, stdout: r.stdout, stderr: r.stderr, requests: [...requests].sort(), hooks: JSON.parse(JSON.stringify(hooks)) };
}

const scenarios = [
  ['daily send', {}, ['once', 'daily']],
  ['weekly send', {}, ['once', 'weekly']],
  ['daily dry-run', {}, ['once', 'daily', '--dry-run']],
  ['weekly dry-run', {}, ['once', 'weekly', '--dry-run']],
  ['prometheus 500 + snapshot 500', { prom500: true, snapshot500: true }, ['once', 'daily']],
  ['loki 500', { loki500: true }, ['once', 'daily']],
  ['grafana rules 500 + one panel fails', { rules500: true, render10fail: true }, ['once', 'daily']],
  ['no Loki / Grafana', {}, ['once', 'daily'], { LOKI_URL: '', GRAFANA_URL: '' }],
  ['basic auth + public URL', {}, ['once', 'daily'], { GRAFANA_TOKEN: '', GRAFANA_USER: 'admin', GRAFANA_PASSWORD: 'pass-word', GRAFANA_PUBLIC_URL: 'https://grafana.example/' }],
  ['discord 429 then ok', { discord429: true }, ['once', 'daily']],
  ['discord 500 (retries, exit 1)', { discord500: true }, ['once', 'daily']],
  ['discord 400 (no retry, exit 1)', { discord400: true }, ['once', 'daily']],
];

const firstDiff = (a, b) => {
  const [x, y] = [JSON.stringify(a, null, 2).split('\n'), JSON.stringify(b, null, 2).split('\n')];
  const i = x.findIndex((l, k) => l !== y[k]);
  const at = i < 0 ? x.length : i;
  return `line ${at + 1}\n  node: ${x[at] ?? '(end)'}\n  rust: ${y[at] ?? '(end)'}`;
};

let failed = 0;
for (const [name, m, args, extra = {}] of scenarios) {
  mode = m;
  const env = { PATH: process.env.PATH, PROMETHEUS_URL: prom.url, GRAFANA_URL: grafana.url, GRAFANA_TOKEN: 'glsa_faketoken', LOKI_URL: loki.url, DISCORD_WEBHOOK_URL: `${discord.url}/api/webhooks/1/${SECRET}`, ...extra };
  const node = await capture('node', [NODE_ENTRY, ...args], env);
  // weekly also queries the week before, so the latest `time` is the run's own clock
  const times = node.requests.map((r) => r.match(/[?&]time=([\d.]+)/)?.[1]).filter(Boolean).map((t) => Math.round(Number(t) * 1000));
  if (!times.length) { failed += 1; console.log(`FAIL ${name}: Node made no Prometheus request to read its clock from`); continue; }
  const now = Math.max(...times);
  const rust = await capture(RUST_BIN, args, { ...env, DIGEST_NOW_MS: String(now) });
  const leaks = [node, rust].some((r) => `${r.stdout}${r.stderr}`.includes(SECRET));
  const diffs = ['exit', 'stdout', 'stderr', 'requests', 'hooks'].filter((k) => JSON.stringify(node[k]) !== JSON.stringify(rust[k]));
  if (diffs.length || leaks) {
    failed += 1;
    console.log(`FAIL ${name}${leaks ? ' (webhook secret printed)' : ''}`);
    for (const k of diffs) console.log(` ${k} differs at ${firstDiff(node[k], rust[k])}`);
  } else {
    console.log(`PASS ${name} (exit ${node.exit}, ${node.requests.length} requests, ${node.hooks.length} webhook posts)`);
  }
}

for (const x of [prom, grafana, loki, discord]) x.s.close();
console.log(failed ? `${failed} FAILED` : 'all identical');
process.exit(failed ? 1 : 0);
