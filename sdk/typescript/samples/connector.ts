import { defineConnector, serve, verifyWebhook, normalizeWebhook } from '../index.mjs';

await serve(defineConnector({
  id: 'sample-connector',
  version: '1.0.0',
  capabilities: () => ({ version: 1, optional: [] }),
  verify: verifyWebhook,
  normalize: normalizeWebhook,
}));
