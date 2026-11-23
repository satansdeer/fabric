import { defineTarget, serve, TARGET_OPERATIONS } from '../index.mjs';

await serve(defineTarget({
  id: 'sample-target',
  version: '1.0.0',
  capabilities: () => ({ placements: ['sample-target'], operations: TARGET_OPERATIONS, optional: [] }),
  placement: () => ({ backend: 'host.local' }),
  completed: ({ grant }) => ({ received: grant }),
  prepare: ({ grant, actions }) => ({ grant, action: actions[0] }),
  start: ({ grant }) => ({ grant, action: { primitive: 'process.spawn' } }),
  inspect: ({ grant }) => ({ grant, action: { primitive: 'process.inspect' } }),
  cancel: ({ grant }) => ({ grant, action: { primitive: 'process.terminate' } }),
  collect: ({ grant, actions }) => ({ grant, action: actions[0] }),
  reclaim: ({ grant }) => ({ grant, action: { primitive: 'workspace.reclaim' } }),
}));
