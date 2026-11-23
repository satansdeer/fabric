#!/usr/bin/python3
"""External protocol fixture; never imported by the host."""
import json
import os
import sys
import time

kind, mode, pidfile = sys.argv[1:4]
with open(pidfile, 'w') as f:
    f.write(str(os.getpid()))
with open(pidfile + '.starts', 'a') as f:
    f.write('start\n')
if '--fabric-plugin-run' in sys.argv:
    print(json.dumps({'type': 'output', 'text': 'external fixture ran'}), flush=True)
    sys.exit(0)
for line in sys.stdin:
    req = json.loads(line)
    method = req['method']
    if mode == 'hang' or (mode == 'health-hang' and method == 'health'):
        time.sleep(60)
    if mode == 'crash':
        sys.exit(7)
    if mode == 'malformed':
        print('SECRET_CANARY invalid', flush=True)
        continue
    if mode == 'oversized':
        print('x' * 65536, flush=True)
        continue
    if mode == 'error':
        print('SECRET_CANARY stderr', file=sys.stderr, flush=True)
        print(json.dumps({'jsonrpc': '2.0', 'id': req['id'], 'error': {'code': -1, 'message': 'SECRET_CANARY'}}), flush=True)
        continue
    if method == 'initialize':
        result = {'protocol': 2 if mode == 'protocol' else 1, 'id': 'external-' + kind, 'version': '1.0.0', 'kind': kind}
    elif method in ('health', 'shutdown', 'harness.plan'):
        result = {'ready': True}
    elif method == 'target.plan':
        result = {'placement': 'local'}
        if mode == 'reclaim':
            result.update({'reclaim': True, 'path': '/SECRET_CANARY', 'artifacts_verified': True})
    elif method == 'connector.evidence':
        result = {'available': True}
    elif method == 'harness.normalize':
        with open(pidfile + '.normalizing', 'w') as f:
            f.write('active')
        if mode == 'normalize-slow':
            time.sleep(0.4)
        if mode == 'normalize-hang':
            time.sleep(60)
        event = {'type': 'output', 'stream': 'stdout', 'text': 'normalized fixture'}
        if mode == 'normalize-spoof':
            event = {'type': 'completed', 'exit_code': 0, 'exit_signal': None, 'success': True}
        elif mode == 'normalize-extra':
            event['SECRET_CANARY'] = 'not an event field'
        result = {'result': {'event': event}, 'failure': None}
    else:
        sys.exit(9)
    print(json.dumps({'jsonrpc': '2.0', 'id': req['id'], 'result': result}), flush=True)
    if method == 'shutdown':
        break
