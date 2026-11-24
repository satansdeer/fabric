#!/usr/bin/python3
"""Provider-free loopback PTY transport fixture; never an Internet ingress.

The adapter does not allocate a PTY. An independently authenticated endpoint
owns a native target process, which allocates/reaps the real terminal. Keys and
transcripts belong only to this explicit test fixture, never Fabric storage.
"""
import errno
import fcntl
import hashlib
import hmac
import json
import os
import pty
import secrets
import select
import shlex
import signal
import socket
import struct
import subprocess
import sys
import termios
import time
import tty

MAX_FRAME = 65536
OPS = ['prepare', 'start', 'inspect', 'cancel', 'collect', 'reclaim']
ACTIONS = dict(zip(OPS, [dict(primitive='workspace.fork', strategy='clone_or_copy'),
    dict(primitive='process.spawn'), dict(primitive='process.inspect'),
    dict(primitive='process.terminate'), dict(primitive='artifacts.capture'),
    dict(primitive='workspace.reclaim')]))
ACTIONS.update({'environment.ensure': dict(primitive='environment.ensure'),
    'environment.inspect': dict(primitive='environment.inspect'),
    'environment.destroy': dict(primitive='environment.destroy'),
    'environment.workspace': dict(primitive='workspace.fork', strategy='clone_or_copy')})
CANARY = b'issue32-echo-disabled-only'
BINARY = b'\x00\xc3\xa9\x03\x1b'


def send(sock, value):
    data = json.dumps(value, separators=(',', ':')).encode()
    assert len(data) <= MAX_FRAME
    sock.sendall(struct.pack('!I', len(data)) + data)


def receive(sock):
    def exact(count):
        result = bytearray()
        while len(result) < count:
            part = sock.recv(count - len(result))
            if not part:
                raise EOFError()
            result.extend(part)
        return bytes(result)
    size = struct.unpack('!I', exact(4))[0]
    assert 0 < size <= MAX_FRAME
    return json.loads(exact(size))


def proof(key, role, first, second):
    return hmac.new(bytes.fromhex(key), role + bytes.fromhex(first) + bytes.fromhex(second), hashlib.sha256).hexdigest()


def authenticate(sock, config, server=False):
    if server:
        nonce = secrets.token_hex(32)
        send(sock, {'nonce': nonce})
        reply = receive(sock)
        if set(reply) != {'nonce', 'proof'} or not hmac.compare_digest(reply['proof'], proof(config['key'], b'client', nonce, reply['nonce'])):
            raise PermissionError()
        send(sock, {'proof': proof(config['key'], b'server', reply['nonce'], nonce)})
    else:
        challenge = receive(sock)
        nonce = secrets.token_hex(32)
        send(sock, {'nonce': nonce, 'proof': proof(config['key'], b'client', challenge['nonce'], nonce)})
        reply = receive(sock)
        if not hmac.compare_digest(reply['proof'], proof(config['key'], b'server', nonce, challenge['nonce'])):
            raise PermissionError()


class Frames:
    def __init__(self, fd):
        self.fd, self.buffer = fd, bytearray()

    def next(self, timeout=5):
        deadline = time.monotonic() + timeout
        while b'\n' not in self.buffer:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([self.fd], [], [], remaining)[0]:
                raise TimeoutError('bounded fixture wait')
            part = os.read(self.fd, 4096)
            if not part:
                raise EOFError()
            self.buffer.extend(part)
            assert len(self.buffer) <= MAX_FRAME
        line, _, rest = self.buffer.partition(b'\n')
        self.buffer = bytearray(rest)
        return json.loads(line)


def event(kind, **metadata):
    print(json.dumps(dict(event=kind, **metadata)), flush=True)


def reap_backend(child):
    # The child is unreaped until wait succeeds. Never signal a recycled PID.
    child.stdin.close()
    try:
        return child.wait(timeout=2)
    except subprocess.TimeoutExpired:
        os.killpg(child.pid, signal.SIGTERM)
        try:
            return child.wait(timeout=2)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            return child.wait(timeout=2)


def server(config):
    listener = socket.socket()
    listener.bind(('127.0.0.1', 0))
    listener.listen(1)
    event('ready', port=listener.getsockname()[1])
    drop = [False]
    signal.signal(signal.SIGUSR1, lambda *_: drop.__setitem__(0, True))
    stopping = [False]
    signal.signal(signal.SIGTERM, lambda *_: stopping.__setitem__(0, True))
    listener.settimeout(0.05)
    while not stopping[0]:
        try:
            connection, _ = listener.accept()
        except socket.timeout:
            continue
        event('accepted')
        child = None
        try:
            connection.settimeout(0.75)
            authenticate(connection, config, server=True)
            event('authenticated')
            child = subprocess.Popen([config['backend'], '--fabric-plugin-stdio'],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                env={'PATH': '/usr/bin:/bin'}, cwd='/', start_new_session=True)
            frames, request_id = Frames(child.stdout.fileno()), 0
            def rpc(method, params):
                nonlocal request_id
                request_id += 1
                child.stdin.write((json.dumps(dict(jsonrpc='2.0', id=request_id, method=method, params=params)) + '\n').encode())
                child.stdin.flush()
                reply = frames.next(timeout=0.75)
                assert set(reply) == {'jsonrpc', 'id', 'result'} and reply['jsonrpc'] == '2.0' and reply['id'] == request_id
                return reply['result']
            rpc('initialize', {'protocol': 1})
            rpc('health', {})
            rpc('target.capabilities', {})
            rpc('target.placement', {'placement': 'local', 'backends': ['host.local']})
            rpc('target.environment.capabilities', {'version': 1})
            opened = False
            while not stopping[0]:
                if drop[0]:
                    drop[0] = False
                    break
                if not select.select([connection], [], [], 0.05)[0]:
                    continue
                request = receive(connection)
                method, params = request['method'], request['params']
                if method == 'target.pty.openExec':
                    assert not opened
                    assert params['owner'] == 'alice' and params['owner_uid'] == os.geteuid()
                    assert params['environment_id'] == 'remote-home' and params['placement'] == 'remote-fixture'
                    assert params['home'] == config['home'] == os.path.realpath(params['home'])
                    params['placement'] = 'local'
                    reply = rpc(method, params)
                    opened = True
                    event('opened', backend_pid=child.pid)
                else:
                    assert opened and method in ['target.pty.exchange', 'target.pty.close']
                    reply = rpc(method, params)
                send(connection, reply)
                if method == 'target.pty.close':
                    rpc('shutdown', {})
                    break
        except (OSError, EOFError, AssertionError, ValueError, KeyError, TimeoutError):
            event('transport_closed')
        finally:
            connection.close()
            if child is not None:
                reap_backend(child)
                event('reaped', backend_pid=child.pid)
            else:
                event('denied')


def adapter(config):
    remote, pending, seen = None, set(), set()
    try:
        for expected, line in enumerate(sys.stdin.buffer, 1):
            assert len(line) <= MAX_FRAME
            request = json.loads(line)
            assert set(request) == {'jsonrpc', 'id', 'method', 'params'} and request['id'] == expected and request['jsonrpc'] == '2.0'
            method, params = request['method'], request['params']
            if method == 'initialize':
                result = dict(protocol=1, id='remote-fixture', version='0.1.0', kind='target')
            elif method in ['health', 'shutdown']:
                assert not pending
                result = dict(ready=True)
            elif method == 'target.capabilities':
                result = dict(placements=['remote-fixture'], operations=OPS, optional=['environment.v1', 'pty.v1'])
            elif method == 'target.placement':
                result = dict(backend='host.local')
            elif method == 'target.environment.capabilities':
                result = dict(version=1, persistent_home=True, interactive_exec_pty=True,
                    infrastructure_diagnostics=False, operations=['ensure', 'inspect', 'destroy', 'workspace'])
            elif method.startswith('target.pty.'):
                if method == 'target.pty.openExec':
                    assert remote is None
                    remote = socket.create_connection(('127.0.0.1', config['port']), timeout=0.75)
                    authenticate(remote, config)
                assert remote is not None
                send(remote, dict(method=method, params=params))
                result = receive(remote)
                if method == 'target.pty.close':
                    remote.close()
                    remote = None
            elif method == 'target.completed':
                pending.remove(params['grant'])
                result = dict(received=params['grant'])
            else:
                operation = method.removeprefix('target.')
                assert operation in ACTIONS and params['placement'] == 'remote-fixture'
                grant = params['grant']
                assert grant not in seen and ACTIONS[operation] in params['actions']
                seen.add(grant)
                pending.add(grant)
                result = dict(grant=grant, action=ACTIONS[operation])
            sys.stdout.write(json.dumps(dict(jsonrpc='2.0', id=expected, result=result)) + '\n')
            sys.stdout.flush()
            if method == 'shutdown':
                return
        raise EOFError()
    finally:
        if remote is not None:
            remote.close()


def peer():
    saved = termios.tcgetattr(0)
    try:
        private = termios.tcgetattr(0)
        private[3] &= ~termios.ECHO
        termios.tcsetattr(0, termios.TCSANOW, private)
        os.write(1, b'\r\nLOGIN_SECRET:')
        secret = sys.stdin.buffer.readline().rstrip(b'\n')
        assert secret == CANARY
        with open(os.path.join(os.environ['XDG_CONFIG_HOME'], 'login-marker'), 'wb') as output:
            output.write(secret)
        with open(os.path.join(os.environ['XDG_STATE_HOME'], 'owned-pid'), 'w') as output:
            output.write(str(os.getpid()))
        tty.setraw(0)
        os.write(1, b'\r\nAUTHENTICATED\r\n')
        incoming = bytearray()
        while len(incoming) < len(BINARY):
            incoming.extend(os.read(0, len(BINARY) - len(incoming)))
        assert bytes(incoming) == BINARY
        outgoing = memoryview(b'BINARY:' + incoming + b'B' * 16384 + b'\r\nBINARY_DONE\r\n')
        while outgoing:
            outgoing = outgoing[os.write(1, outgoing):]
        while True:
            command = os.read(0, 1)
            if command == b'q':
                return
            if command == b's':
                rows, columns, _, _ = struct.unpack('HHHH', fcntl.ioctl(0, termios.TIOCGWINSZ, b'\0' * 8))
                os.write(1, ('SIZE:%d:%d\r\n' % (rows, columns)).encode())
            elif command == b'h':
                os.write(1, b'HEARTBEAT\r\n')
            elif command == b'c':
                termios.tcsetattr(0, termios.TCSANOW, saved)
                signal.signal(signal.SIGINT, signal.SIG_DFL)
                os.write(1, b'CONTROL_READY\n')
                signal.pause()
            elif not command:
                return
    finally:
        termios.tcsetattr(0, termios.TCSANOW, saved)


def scenario(fabric, backend, root, registry, script):
    environment = {'PATH': '/usr/bin:/bin', 'HOME': os.path.dirname(root), 'FABRIC_PLUGIN_REGISTRY': registry}
    common = [fabric, '--root', root]
    def cli(*args, success=True):
        result = subprocess.run(common + list(args), env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
        assert (result.returncode == 0) == success, 'CLI contract failure: ' + result.stderr.decode(errors='replace')
        assert CANARY not in result.stdout + result.stderr
        return result
    config_path = os.path.join(os.path.dirname(root), 'remote-config.json')
    config = dict(home=os.path.join(root, 'environments/remote-home/tree'), key=secrets.token_hex(32), backend=backend)
    def save_config(value):
        with open(config_path, 'w') as output:
            json.dump(value, output)
        os.chmod(config_path, 0o600)
    save_config(config)
    endpoint = subprocess.Popen(['/usr/bin/python3', script, 'server', config_path],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, env={'PATH': '/usr/bin:/bin'}, start_new_session=True)
    events = Frames(endpoint.stdout.fileno())
    history = []
    active = []
    def release(state):
        child, master, slave, _, _ = state
        try:
            if child.returncode is None:
                child.send_signal(signal.SIGTERM)
                try:
                    child.wait(timeout=6)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=6)
        finally:
            os.close(master)
            os.close(slave)
            active.remove(state)
    def until_event(name):
        while True:
            item = events.next()
            history.append(item['event'])
            if item['event'] == name:
                return item
    try:
        config['port'] = until_event('ready')['port']
        save_config(config)
        manifest = os.path.join(os.path.dirname(root), 'remote-manifest.json')
        with open(manifest, 'w') as output:
            json.dump(dict(id='remote-fixture', version='0.1.0', kind='target', protocol=dict(min=1, max=1),
                executable=script, args=['target', config_path], capabilities=['target.execute']), output)
    except BaseException:
        endpoint.send_signal(signal.SIGTERM)
        endpoint.wait(timeout=6)
        raise
    try:
        cli('plugins', '--registry', registry, 'install', manifest)
        cli('environments', 'ensure', 'remote-home', '--owner', 'alice', '--target', 'remote-fixture', '--interactive-exec-pty', '--json')
        home = config['home']
        def launch(owner='alice', target='remote-fixture', env_id='remote-home', limits=()):
            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 80, 0, 0))
            before = termios.tcgetattr(slave)
            flags = fcntl.fcntl(slave, fcntl.F_GETFL)
            try:
                child = subprocess.Popen(common + ['shell', env_id, '--owner', owner, '--target', target] + list(limits),
                    env=environment, stdin=slave, stdout=slave, stderr=subprocess.PIPE)
            except BaseException:
                os.close(master)
                os.close(slave)
                raise
            state = child, master, slave, before, flags
            active.append(state)
            return state
        def finish(state, success):
            child, master, slave, before, flags = state
            try:
                code = child.wait(timeout=6)
                diagnostics = child.stderr.read()
                assert (code == 0) == success and CANARY not in diagnostics
                after = termios.tcgetattr(slave)
                # Darwin sets PENDIN when restoring canonical input, even for a
                # plain tcsetattr round trip. It is kernel queue state, not a mode.
                if sys.platform == 'darwin':
                    after[3] &= ~termios.PENDIN
                    before[3] &= ~termios.PENDIN
                assert after == before, 'terminal attributes not restored'
                after_flags = fcntl.fcntl(slave, fcntl.F_GETFL)
                # XNU's read-only FWASWRITTEN bit appears after any slave write;
                # F_SETFL cannot clear it. Preserve every user-settable flag.
                if sys.platform == 'darwin':
                    after_flags &= ~0x00010000
                    flags &= ~0x00010000
                assert after_flags == flags, 'terminal flags not restored'
                return diagnostics
            finally:
                release(state)
        # These refusals must not even make a TCP connection to the endpoint.
        import sqlite3
        with sqlite3.connect(os.path.join(root, 'fabric.db')) as database:
            for name, uid, desired, runtime in [
                ('foreign-home', os.geteuid() + 1, 'available', 'available'),
                ('unavailable-home', os.geteuid(), 'destroyed', 'absent')]:
                database.execute('INSERT INTO environments SELECT ?, owner, ?, target_json, capabilities_json, resource_json, ?, ?, observed_at, observation, destroy_intent FROM environments WHERE environment_id=?',
                    (name, uid, desired, runtime, 'remote-home'))
        for kwargs in [dict(owner='mallory'), dict(target='local'), dict(env_id='other-home'),
            dict(env_id='foreign-home'), dict(env_id='unavailable-home')]:
            finish(launch(**kwargs), False)
        cli('environments', 'ensure', 'legacy-home', '--owner', 'alice', '--target', 'legacy-env', '--json')
        finish(launch(target='legacy-env', env_id='legacy-home'), False)
        # Replace an XDG root, not merely the logical owner string.
        os.rename(os.path.join(home, '.config'), os.path.join(home, '.config-owned'))
        os.symlink(os.path.join(home, '.config-owned'), os.path.join(home, '.config'))
        try:
            finish(launch(), False)
        finally:
            os.unlink(os.path.join(home, '.config'))
            os.rename(os.path.join(home, '.config-owned'), os.path.join(home, '.config'))
        assert not select.select([endpoint.stdout], [], [], 0)[0], 'denial opened remote transport'
        # Authentication failure is real: the server's private key is unchanged.
        bad = dict(config, key=secrets.token_hex(32))
        save_config(bad)
        finish(launch(), False)
        until_event('denied')
        assert 'opened' not in history
        save_config(config)
        for mode in ['normal', 'cancel', 'drop', 'control', 'idle', 'maximum']:
            print('PTY fixture scenario: ' + mode, flush=True)
            limits = ('--idle-seconds', '3', '--max-seconds', '10') if mode == 'idle' else (
                ('--idle-seconds', '5', '--max-seconds', '5') if mode == 'maximum' else ())
            state = launch(limits=limits)
            child, master, slave, _, _ = state
            transcript = bytearray()
            def expect(marker):
                deadline = time.monotonic() + 5
                while marker not in transcript:
                    left = deadline - time.monotonic()
                    assert left > 0 and select.select([master], [], [], left)[0], 'terminal exchange timed out'
                    transcript.extend(os.read(master, 4096))
                    assert len(transcript) <= 131072 and CANARY not in transcript, 'default echo leaked secret'
            until_event('opened')
            # First forwarded output proves CLI entered raw mode before sending secrets.
            assert select.select([master], [], [], 5)[0]
            transcript.extend(os.read(master, 4096))
            os.write(master, ('exec /usr/bin/python3 ' + shlex.quote(script) + ' peer\n').encode())
            expect(b'\r\nLOGIN_SECRET:')
            os.write(master, CANARY + b'\n')
            expect(b'\r\nAUTHENTICATED\r\n')
            os.write(master, BINARY)
            expect(b'\r\nBINARY_DONE\r\n')
            assert b'BINARY:' + BINARY + b'B' * 16384 in transcript
            pid_path = os.path.join(home, '.local/state/owned-pid')
            with open(pid_path) as source:
                owned_pid = int(source.read())
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 37, 91, 0, 0))
            os.write(master, b's')
            expect(b'SIZE:37:91\r\n')
            refused = cli('environments', 'destroy', 'remote-home', '--owner', 'alice', '--target', 'remote-fixture', '--confirm', 'remote-home', '--json', success=False)
            assert b'environment_lease_busy' in refused.stderr
            if mode == 'normal':
                os.write(master, b'q')
            elif mode == 'cancel':
                child.send_signal(signal.SIGTERM)
            elif mode == 'drop':
                endpoint.send_signal(signal.SIGUSR1)
            elif mode == 'control':
                os.write(master, b'c')
                expect(b'CONTROL_READY\r\n')
                os.write(master, b'\x03')
            elif mode == 'idle':
                # Stop only the proven owned CLI, not its adapter/target.
                # Target-side idle expiry must reap without another host RPC.
                child.send_signal(signal.SIGSTOP)
                try:
                    deadline = time.monotonic() + 5
                    while True:
                        try:
                            os.kill(owned_pid, 0)
                        except ProcessLookupError:
                            break
                        assert time.monotonic() < deadline, 'target idle timer did not reap'
                        select.select([], [], [], 0.02)
                finally:
                    child.send_signal(signal.SIGCONT)
            elif mode == 'maximum':
                # Acknowledged activity defeats idle timeout; max duration still closes.
                deadline = time.monotonic() + 7
                while child.poll() is None and time.monotonic() < deadline:
                    os.write(master, b'h')
                    ready = select.select([master], [], [], 0.1)[0]
                    if ready:
                        transcript.extend(os.read(master, 4096))
                        assert CANARY not in transcript
            finish(state, mode == 'normal')
            until_event('reaped')
            try:
                os.kill(owned_pid, 0)
            except ProcessLookupError:
                pass
            else:
                raise AssertionError('owned PTY child not reaped')
            with open(os.path.join(home, '.config/login-marker'), 'rb') as marker:
                assert marker.read() == CANARY
        assert history.count('opened') == 6 and history.count('reaped') == 6
        # Shell did not manufacture worker/run records or a Connect inbox.
        import sqlite3
        with sqlite3.connect(os.path.join(root, 'fabric.db')) as database:
            assert database.execute('select count(*) from runs').fetchone()[0] == 0
        assert not os.path.exists(os.path.join(root, 'connect'))
    finally:
        for state in active[:]:
            release(state)
        endpoint.send_signal(signal.SIGTERM)
        endpoint.wait(timeout=6)
        assert CANARY not in endpoint.stderr.read()


if __name__ == '__main__':
    mode = sys.argv[1]
    try:
        if mode == 'peer':
            peer()
        elif mode == 'scenario':
            scenario(*sys.argv[2:])
        else:
            with open(sys.argv[2]) as source:
                configuration = json.load(source)
            if mode == 'server':
                server(configuration)
            elif mode == 'target':
                adapter(configuration)
            else:
                raise ValueError('fixture mode')
    except (OSError, EOFError, AssertionError, ValueError, KeyError, TimeoutError):
        # Never interpolate request, response, exception or interactive bytes.
        trace = sys.exc_info()[2]
        while trace is not None:
            print('PTY fixture failure at %s:%d' % (
                trace.tb_frame.f_code.co_name, trace.tb_lineno), file=sys.stderr)
            trace = trace.tb_next
        print('PTY fixture contract failed', file=sys.stderr)
        sys.exit(1)
