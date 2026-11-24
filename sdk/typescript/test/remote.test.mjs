import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { once } from 'node:events';

function session(t, optional) {
  const source = `import {defineTarget,serve,TARGET_OPERATIONS} from ${JSON.stringify(new URL('../index.mjs', import.meta.url).href)};
    process.argv = [process.execPath,'fixture','--fabric-plugin-stdio'];
    function grant(j) { if(this.id !== 'remote-proof') throw Error('plugin receiver lost'); return {grant:j.grant, action:j.actions[0]}; }
    await serve(defineTarget({id:'remote-proof',version:'1.0.0',
      capabilities:()=>({placements:['remote-proof'],operations:TARGET_OPERATIONS,optional:${JSON.stringify(optional)}}),
      placement:p=>({backend:p.backends[0]}),
      ...Object.fromEntries(TARGET_OPERATIONS.map(k=>[k,grant])), completed:j=>({received:j.grant}),
      environment:{state:'environment-owner',ensure(j){if(this.state !== 'environment-owner') throw Error('environment receiver lost');return {grant:j.grant,action:j.actions[0]};},inspect:grant,destroy:grant,workspace:grant},
      remote:async()=>{throw Object.assign(Error('PRIVATE_SENTINEL'),{scope:'harness',kind:'unsupported_version'});}
    }));`;
  const child = spawn(process.execPath, ['--input-type=module', '-e', source, '--', '--fabric-plugin-stdio'], { stdio: ['pipe', 'pipe', 'pipe'] });
  t.after(() => { child.stdin.destroy(); if (child.exitCode === null) child.kill(); });
  const lines = createInterface({ input: child.stdout })[Symbol.asyncIterator]();
  let id = 0, stderr = '';
  child.stderr.on('data', b => { stderr += b; });
  return { child, stderr: () => stderr, async call(method, params = {}) {
    child.stdin.write(JSON.stringify({jsonrpc:'2.0',id:++id,method,params})+'\n');
    const line = await lines.next();
    if (line.done) return null;
    const reply = JSON.parse(line.value); assert.equal(reply.id,id); return reply.result;
  } };
}
async function negotiate(s, remote) {
  await s.call('initialize',{protocol:1}); await s.call('health'); await s.call('target.capabilities');
  assert.deepEqual(await s.call('target.placement',{placement:'remote-proof',backends:[remote?'remote.v1':'host.local']}),{backend:remote?'remote.v1':'host.local'});
}

test('remote failure crosses public async protocol without secret text or successful fallback', {timeout:5000}, async t => {
  const s = session(t,['environment.v1','pty.v1','remote.v1']); await negotiate(s,true);
  const {ticket} = await s.call('target.remote.invoke',{version:1,operation:'harness.probe',input:{}});
  let result;
  do { result = await s.call('target.remote.poll',{ticket}); } while (result.state === 'pending');
  assert.deepEqual(result,{state:'failed',scope:'harness',kind:'unsupported_version'});
  const closed = once(s.child,'close'); await s.call('shutdown'); await closed; assert.equal(s.child.exitCode,0);
  assert.equal(s.stderr().includes('PRIVATE_SENTINEL'),false);
});

test('remote selection rejects host process grants instead of executing them', {timeout:5000}, async t => {
  const s = session(t,['environment.v1','pty.v1','remote.v1']); await negotiate(s,true);
  const closed = once(s.child,'close');
  assert.equal(await s.call('target.start',{placement:'remote-proof',run_id:'run',grant:'grant',actions:[{primitive:'process.spawn'}],context:{}}),null);
  await closed; assert.equal(s.child.exitCode,1);
});

for (const optional of [[],['environment.v1'],['environment.v1','pty.v1']]) {
  test(`legacy target grants still execute with ${JSON.stringify(optional)}`, {timeout:5000}, async t => {
    const s = session(t,optional); await negotiate(s,false);
    const environment = optional.length > 0;
    if (environment) await s.call('target.environment.capabilities',{version:1});
    const actions = [{method:'target.start',key:'run_id',primitive:'process.spawn'}];
    if (environment) actions.push({method:'target.environment.ensure',key:'environment_id',primitive:'environment.ensure'});
    for (const [i, operation] of actions.entries()) {
      const action = {primitive:operation.primitive};
      const grant = `once_${i}`;
      const job = {placement:'remote-proof',[operation.key]:'proof',grant,actions:[action],context:{}};
      assert.deepEqual(await s.call(operation.method,job),{grant,action});
      assert.deepEqual(await s.call('target.completed',{grant,evidence:{observed:true}}),{received:grant});
    }
    const closed = once(s.child,'close'); await s.call('shutdown'); await closed; assert.equal(s.child.exitCode,0);
  });
}
