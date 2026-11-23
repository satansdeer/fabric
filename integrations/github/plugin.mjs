import { defineConnector, serve } from '../../sdk/typescript/index.mjs';
import { loadConfig, verify, normalize, subscribe, reply } from './connector.mjs';

const args = process.argv.slice(2);
if (args.length !== 3 || args[0] !== '--config' || args[2] !== '--fabric-plugin-stdio') {
  process.stderr.write('GitHub connector: explicit config and SDK stdio mode required.\n');
  process.exitCode = 1;
} else {
  try {
    const config = loadConfig(args[1]);
    process.argv.splice(2, 2); // SDK owns transport mode validation.
    await serve(defineConnector({
      id: 'github-issues', version: '1.0.0',
      capabilities: () => ({ version: 1, optional: ['subscribe', 'reply'] }),
      verify: input => verify(config, input),
      normalize: input => normalize(config, input.request, input.policy.secret_ref),
      subscribe: async input => ({ version: 1, data: await subscribe(config, input.data) }),
      reply: async input => ({ version: 1, data: await reply(config, input.data) }),
    }));
  } catch {
    process.stderr.write('GitHub connector: rejected config or operation.\n');
    process.exitCode = 1;
  }
}
