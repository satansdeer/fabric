// Provider-free initialize-only adversarial peer. No native stores or providers.
import { appendFileSync } from 'node:fs';
import { createInterface } from 'node:readline';

const [log, mode, literal] = process.argv.slice(2);
const record = value => appendFileSync(log, JSON.stringify(value) + '\n');
const send = value => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', ...value }) + '\n');
record({ pid: process.pid, cwd: process.cwd(), home: process.env.HOME,
  environment: Object.keys(process.env).sort(), literal });
process.stderr.write('SECRET_STDERR_CANARY\n');
process.on('SIGTERM', () => {
  record({ terminating: process.pid });
  if (!['hang', 'ignore-term'].includes(mode)) process.exit(0);
});
if (['hang', 'ignore-term'].includes(mode)) setInterval(() => {}, 1000);
for await (const line of createInterface({ input: process.stdin })) {
  const request = JSON.parse(line);
  if (!Object.hasOwn(request, 'method')) { record({ response: request }); continue; }
  record({ method: request.method });
  if (request.method !== 'initialize') process.exit(31);
  if (mode === 'hang') continue;
  if (mode === 'exit') process.exit(0);
  if (mode === 'error') { send({ id: request.id, error: { code: -32000, message: 'SECRET_ERROR_CANARY', data: 'SECRET_DATA_CANARY' } }); continue; }
  if (mode === 'request') { send({ id: 'blocking', method: 'cursor/ask_question', params: { secret: 'SECRET_REQUEST_CANARY' } }); continue; }
  if (mode === 'malformed') { process.stdout.write('SECRET_MALFORMED_CANARY\n'); continue; }
  if (mode === 'oversized') { process.stdout.write('x'.repeat(65_537)); continue; }
  if (mode === 'notifications') {
    process.stdout.write(Array.from({ length: 17 }, () => JSON.stringify({ jsonrpc: '2.0', method: 'unknown', params: { secret: 'SECRET_NOTIFY_CANARY' } }) + '\n').join(''));
    continue;
  }
  send({ method: 'unknown', params: { secret: 'SECRET_NOTIFY_CANARY' } });
  const response = { jsonrpc: '2.0', id: request.id, result: {
    protocolVersion: mode === 'protocol' ? 2 : 1, agentCapabilities: {},
    authMethods: [{ id: 'SECRET_AUTH_CANARY', name: 'SECRET_AUTH_CANARY' }],
    agentInfo: { name: 'UNTRUSTED_NAME_CANARY', version: 'UNTRUSTED_VERSION_CANARY', _meta: { secret: 'SECRET_META_CANARY' } },
    _meta: { secret: 'SECRET_META_CANARY' },
  } };
  const suffix = mode === 'late-request' ? JSON.stringify({ jsonrpc: '2.0', id: 'late', method: 'terminal/create', params: {} }) + '\n' : '';
  process.stdout.write(JSON.stringify(response) + '\n' + suffix);
}
