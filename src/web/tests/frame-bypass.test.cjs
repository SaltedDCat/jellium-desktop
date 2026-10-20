const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync(require('node:path').join(__dirname, '../frame-bypass.js'), 'utf8');
function setup() {
    let now = 100;
    let timeout;
    const requests = [];
    const ctx = { crypto: { randomUUID: () => 'page-a' }, performance: { now: () => now },
        setTimeout: callback => { timeout = callback; return 1; }, clearTimeout() {},
        CustomEvent: class {}, dispatchEvent() {}, jmpNative: { frameBypassPull: (...args) => requests.push(args) } };
    ctx.window = ctx;
    vm.runInNewContext(source, ctx);
    return { ctx, requests, time: value => { now = value; }, timeout: () => timeout(),
        reply: extra => ctx._onFrameBypass(JSON.stringify({ requestId: 1, clientId: 'page-a',
            receivedMs: 1000, sentMs: 1002, targetMs: 1010, ...extra }), new ArrayBuffer(0)) };
}
test('one pull remains in flight until its reply, including a new subscriber', async () => {
    const env = setup();
    const first = env.ctx.JelliumFrameBypass.pull();
    assert.equal(env.ctx.JelliumFrameBypass.pull(), first);
    assert.equal(env.requests.length, 1);
    env.time(112);
    env.reply();
    const packet = await first;
    assert.equal(packet.clockOffsetMs, -895);
    assert.equal(packet.clockUncertaintyMs, 5);
    assert.equal(packet.deadline, 115);
    env.ctx.JelliumFrameBypass.pull();
    assert.equal(env.requests.length, 2);
});
test('late replies from another page or request cannot consume the pending pull', async () => {
    const env = setup();
    const pending = env.ctx.JelliumFrameBypass.pull();
    env.reply({ clientId: 'old-page' });
    env.reply({ requestId: 99 });
    assert.equal(env.ctx.JelliumFrameBypass.pull(), pending);
    env.reply();
    await pending;
});
test('IPC timeout rejects and permits a later retry', async () => {
    const env = setup();
    const pending = env.ctx.JelliumFrameBypass.pull();
    env.timeout();
    await assert.rejects(pending, /timeout/);
    const retry = env.ctx.JelliumFrameBypass.pull();
    env.reply({ requestId: 1 });
    env.reply({ requestId: 2 });
    await retry;
});
