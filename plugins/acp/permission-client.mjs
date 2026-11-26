import { permissionControl } from './permission.mjs';
import { readBounded } from './sdk/protocol.mjs';

// Private owner terminal only. JSON escapes control characters in untrusted details.
export async function main() {
  try {
    const [op, endpoint, decision] = process.argv.slice(2);
    let message;
    if (op === 'list' && process.argv.length === 4) message = { op: 'list' };
    else if (op === 'decide' && process.argv.length === 5) {
      message = JSON.parse(readBounded(decision, 4096));
      if (message.op !== 'decide' || message.endpoint !== endpoint) throw new Error();
    } else throw new Error();
    const result = await permissionControl(endpoint, message);
    console.log(JSON.stringify(result).replace(/[\u007f-\u009f\u202a-\u202e\u2066-\u2069]/g,
      value => `\\u${value.charCodeAt(0).toString(16).padStart(4, '0')}`));
    if (result.error) process.exitCode = 1;
  } catch { console.error('acp_permission_control_failed'); process.exitCode = 1; }
}
