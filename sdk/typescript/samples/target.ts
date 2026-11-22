import { defineTarget, serve, type TargetJob } from '../index.mjs';

await serve(defineTarget({
  id: 'sample-target',
  version: '1.0.0',
  plan: (job: TargetJob) => ({ placement: job.placement }),
}));
