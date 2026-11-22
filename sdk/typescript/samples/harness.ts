import { defineHarness, serve, type HarnessContext } from '../index.mjs';

await serve(defineHarness({
  id: 'sample-harness',
  version: '1.0.0',
  plan: () => ({ ready: true }),
  async run({ prompt, emit }: HarnessContext) {
    await emit({ type: 'text', text: `Provider-free fixture: ${prompt}` });
    await emit({ type: 'done' });
  },
}));
