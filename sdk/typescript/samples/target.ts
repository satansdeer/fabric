import { defineTarget, serve, TARGET_OPERATIONS } from '../index.mjs';

await serve(defineTarget({
  id: 'sample-target',
  version: '1.0.0',
  capabilities: () => ({ placements: ['sample-target'], operations: TARGET_OPERATIONS, optional: [] }),
  // A fake remote target: every placement operation refuses; it never requests local execution.
  prepare: () => ({ operation: 'unsupported' }),
  start: () => ({ operation: 'unsupported' }),
  inspect: () => ({ operation: 'unsupported' }),
  cancel: () => ({ operation: 'unsupported' }),
  collect: () => ({ operation: 'unsupported' }),
  reclaim: () => ({ operation: 'unsupported' }),
}));
