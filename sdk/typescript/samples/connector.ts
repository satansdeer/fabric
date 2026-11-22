import { defineConnector, serve } from '../index.mjs';

await serve(defineConnector({
  id: 'sample-connector',
  version: '1.0.0',
  evidence: () => ({ available: false }),
}));
