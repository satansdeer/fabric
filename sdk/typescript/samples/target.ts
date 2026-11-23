import { defineTarget, serve, TARGET_OPERATIONS } from '../index.mjs';

await serve(defineTarget({
  id: 'sample-target',
  version: '1.0.0',
  capabilities: () => ({ placements: ['sample-target'], operations: TARGET_OPERATIONS, optional: [] }),
  placement: () => ({ backend: 'unsupported' }),
  completed: ({ grant }) => ({ received: grant }),
  // Fake remote: refuses placement and never requests a host primitive.
  prepare: ({ grant }) => ({ grant, action: { primitive: 'unsupported' } }),
  start: ({ grant }) => ({ grant, action: { primitive: 'unsupported' } }),
  inspect: ({ grant }) => ({ grant, action: { primitive: 'unsupported' } }),
  cancel: ({ grant }) => ({ grant, action: { primitive: 'unsupported' } }),
  collect: ({ grant }) => ({ grant, action: { primitive: 'unsupported' } }),
  reclaim: ({ grant }) => ({ grant, action: { primitive: 'unsupported' } }),
}));
