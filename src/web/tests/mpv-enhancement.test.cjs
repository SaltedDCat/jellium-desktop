const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync(require('node:path').join(__dirname, '../mpv-enhancement.js'), 'utf8');

function setup(options = {}) {
    let nextTimer = 0;
    const timers = new Map();
    const requests = [];
    const events = [];
    const ctx = {
        crypto: options.http ? { getRandomValues: bytes => bytes.fill(7) } : { randomUUID: () => 'page-a' },
        TextEncoder, Uint8Array,
        setTimeout(callback) { const id = ++nextTimer; timers.set(id, callback); return id; },
        clearTimeout(id) { timers.delete(id); },
        CustomEvent: class { constructor(type, options) { this.type = type; this.detail = options?.detail; } },
        dispatchEvent(event) { events.push(event); },
        jmpNative: { playerSetEnhancement(...args) { requests.push(args); } }
    };
    ctx.window = ctx;
    vm.runInNewContext(source, ctx);
    return { ctx, requests, events, timers,
        reply(result = {}, request = requests.at(-1)) {
            const plan = JSON.parse(request[0]);
            ctx._onMpvEnhancement(JSON.stringify({ requestId: request[1], clientId: request[2],
                revision: plan.revision, state: plan.shaders.length ? 'loaded' : 'off',
                shaderCount: plan.shaders.length, validated: false, ...result }));
        }
    };
}

const shader = (id, source = '//!HOOK MAIN\nvec4 hook(){return vec4(1.0);}') => ({ id, source });

test('passes Web shader content and duplicate order through unchanged', async () => {
    const env = setup();
    const plan = { revision: 9, shaders: [shader('restore'), shader('scale', 'abc'), shader('restore')] };
    const result = env.ctx.JelliumMpvEnhancement.set(plan);
    assert.deepEqual(JSON.parse(env.requests[0][0]), plan);
    env.reply();
    assert.equal((await result).validated, false);
    assert.equal(env.timers.size, 0);
    assert.equal(env.events[0].type, 'jellium-mpv-enhancement-ready');
});

test('clear preserves payload revision and later Web revisions remain valid', async () => {
    const env = setup();
    const first = env.ctx.JelliumMpvEnhancement.set({ revision: 3, shaders: [shader('a')] });
    env.reply(); await first;
    const clear = env.ctx.JelliumMpvEnhancement.clear();
    assert.deepEqual(JSON.parse(env.requests.at(-1)[0]), { revision: 3, shaders: [] });
    env.reply(); assert.equal((await clear).state, 'off');
    const later = env.ctx.JelliumMpvEnhancement.set({ revision: 4, shaders: [shader('b')] });
    env.reply(); assert.equal((await later).revision, 4);
});

test('newer calls supersede old promises and ignore late old-document responses', async () => {
    const env = setup();
    const first = env.ctx.JelliumMpvEnhancement.set({ revision: 99, shaders: [shader('a')] });
    const rejected = assert.rejects(first, /superseded/);
    const second = env.ctx.JelliumMpvEnhancement.set({ revision: 1, shaders: [shader('b')] });
    await rejected;
    env.reply({}, env.requests[0]);
    env.reply({ clientId: 'old-document' });
    assert.equal(env.timers.size, 1);
    env.reply(); assert.equal((await second).revision, 1);
});

test('validates plan bounds and reports native rejection and IPC timeout', async () => {
    const env = setup();
    for (const plan of [null, { revision: -1, shaders: [] }, { revision: 1.1, shaders: [] },
        { revision: 1, shaders: [shader('a', '')] }, { revision: 1, shaders: [shader('a', 'a\0b')] },
        { revision: 1, shaders: Array(65).fill(shader('a')) },
        { revision: 1, shaders: [shader('a', 'a'.repeat(8 * 1024 * 1024 + 1))] }]) {
        await assert.rejects(env.ctx.JelliumMpvEnhancement.set(plan));
    }
    assert.equal(env.requests.length, 0);
    const invalid = env.ctx.JelliumMpvEnhancement.set({ revision: 1, shaders: [shader('a')] });
    env.reply({ error: 'mpv rejected GLSL list' });
    await assert.rejects(invalid, /mpv rejected/);
    const timeout = env.ctx.JelliumMpvEnhancement.clear();
    [...env.timers.values()][0]();
    await assert.rejects(timeout, /timeout/);
});

test('load reset cancels in-flight plans and notifies the Web controller', async () => {
    const env = setup();
    const pending = env.ctx.JelliumMpvEnhancement.set({ revision: 4, shaders: [shader('a')] });
    env.ctx._onMpvEnhancement(JSON.stringify({ requestId: 0, state: 'reset', reason: 'load' }));
    await assert.rejects(pending, /reset/);
    assert.equal(env.events.at(-1).type, 'jellium-mpv-enhancement-reset');
    assert.equal(env.events.at(-1).detail.reason, 'load');
    const next = env.ctx.JelliumMpvEnhancement.set({ revision: 5, shaders: [shader('b')] });
    env.reply(); await next;
});

test('HTTP contexts without randomUUID can initialize and match replies', async () => {
    const env = setup({ http: true });
    const request = env.ctx.JelliumMpvEnhancement.set({ revision: 1, shaders: [] });
    assert.equal(env.requests[0][2], '07'.repeat(16));
    env.reply(); await request;
});
