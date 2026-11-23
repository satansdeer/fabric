#!/usr/bin/env node
// Opt-in real installation + terminal-evidence response check. Never creates a token or worker.
import { resolve } from 'node:path';
import { loadConfig, privateJSON, inspect, requireThat } from '../integrations/github/connector.mjs';
import { exchange } from '../integrations/github/bridge.mjs';

async function main(args) {
  const options = {};
  requireThat(args.shift() === '--allow-github-comment-write');
  const keys = ['config', 'plugin', 'sha256', 'request', 'repo', 'issue', 'approval-sha256'];
  for (let i = 0; i < args.length; i += 2) {
    const key = args[i].slice(2);
    requireThat(args[i].startsWith('--') && keys.includes(key) && !Object.hasOwn(options, key) && typeof args[i + 1] === 'string');
    options[key] = args[i + 1];
  }
  requireThat(keys.every(k => Object.hasOwn(options, k)) && /^[1-9][0-9]{0,9}$/.test(options.issue) && /^[a-f0-9]{64}$/.test(options['approval-sha256']));
  options.config = resolve(options.config);
  options.plugin = resolve(options.plugin);
  const c = loadConfig(options.config), request = privateJSON(resolve(options.request));
  requireThat(c.response.api_origin === 'https://api.github.com' && !c.response.allow_loopback_http &&
    c.connect.endpoint.startsWith('https://') && !c.connect.allow_loopback_http && c.repo === options.repo &&
    c.approvals.length === 1 && c.approvals[0].issue_number === Number(options.issue) && c.approvals[0].sha256 === options['approval-sha256']);
  const { data } = inspect(c, request, c.webhook_secret_ref);
  requireThat(data.issue_number === Number(options.issue) && data.approval_sha256 === options['approval-sha256']);
  const installation = await exchange(options, c, undefined, 'subscribe');
  // This cannot post without an existing real terminal hosted receipt and local hashed artifacts.
  const reply = await exchange(options, c, request, 'reply');
  requireThat(reply.posted === true);
  process.stdout.write(JSON.stringify({ check: 'real-github-installation-response', repository: c.repo,
    issue_number: data.issue_number, installation, ...reply }) + '\n');
}
main(process.argv.slice(2)).catch(() => {
  process.stderr.write('Real GitHub check rejected: explicit scope, permissions or terminal local evidence unavailable.\n');
  process.exitCode = 1;
});
