// Generic Web-owned mpv shader transport. No algorithm selection lives here.
(function() {
    'use strict';
    // Jellyfin servers commonly use HTTP, where randomUUID is unavailable.
    const clientId = typeof crypto.randomUUID === 'function' ? crypto.randomUUID() :
        Array.from(crypto.getRandomValues(new Uint8Array(16)), byte => byte.toString(16).padStart(2, '0')).join('');
    const encoder = new TextEncoder();
    let nextId = 0;
    let lastRevision = 0;
    let pending;

    function cancel(message) {
        if (!pending) return;
        const request = pending;
        pending = undefined;
        clearTimeout(request.timer);
        request.reject(new Error(message));
    }

    function validate(plan) {
        if (!plan || !Number.isSafeInteger(plan.revision) || plan.revision < 0 ||
            !Array.isArray(plan.shaders) || plan.shaders.length > 64) {
            throw new Error('Invalid GLSL plan revision or shader count (maximum 64)');
        }
        let total = 0;
        const shaders = plan.shaders.map(shader => {
            if (!shader || typeof shader.id !== 'string' || !shader.id || encoder.encode(shader.id).length > 128 ||
                typeof shader.source !== 'string' || !shader.source || shader.source.includes('\0')) {
                throw new Error('Invalid GLSL shader id or source');
            }
            const bytes = encoder.encode(shader.source).length;
            total += bytes;
            if (bytes > 8 * 1024 * 1024 || total > 32 * 1024 * 1024) {
                throw new Error('GLSL source limit exceeded (8 MiB per shader, 32 MiB total)');
            }
            return { id: shader.id, source: shader.source };
        });
        const payload = JSON.stringify({ revision: plan.revision, shaders });
        if (encoder.encode(payload).length > 48 * 1024 * 1024) throw new Error('GLSL request exceeds 48 MiB');
        return payload;
    }

    function submit(plan) {
        const payload = validate(plan);
        if (typeof window.jmpNative?.playerSetEnhancement !== 'function') {
            throw new Error('Native mpv GLSL loading is unavailable');
        }
        cancel('GLSL plan superseded');
        lastRevision = plan.revision;
        nextId = nextId >= 2147483647 ? 1 : nextId + 1;
        const id = nextId;
        return new Promise((resolve, reject) => {
            const timer = setTimeout(() => {
                if (pending?.id === id) cancel('mpv GLSL IPC timeout');
            }, 15000);
            pending = { id, revision: plan.revision, resolve, reject, timer };
            try {
                window.jmpNative.playerSetEnhancement(payload, id, clientId);
            } catch (error) {
                cancel(error instanceof Error ? error.message : String(error));
            }
        });
    }

    window._onMpvEnhancement = function(json) {
        let result;
        try { result = JSON.parse(json); } catch (_) { return; }
        if (!result || typeof result !== 'object') return;
        if (result.requestId === 0 && result.state === 'reset') {
            cancel('mpv playback changed; GLSL plan reset');
            window.dispatchEvent(new CustomEvent('jellium-mpv-enhancement-reset', { detail: { reason: result.reason } }));
            return;
        }
        if (result.clientId !== clientId || !pending || result.requestId !== pending.id) return;
        const request = pending;
        pending = undefined;
        clearTimeout(request.timer);
        if (result.error) {
            request.reject(new Error(String(result.error)));
        } else if (result.revision !== request.revision || !['loaded', 'off'].includes(result.state)) {
            request.reject(new Error('Invalid mpv GLSL response'));
        } else {
            request.resolve(result);
        }
    };

    window.JelliumMpvEnhancement = Object.freeze({
        version: 1,
        async set(plan) { return submit(plan); },
        async clear() { return submit({ revision: lastRevision, shaders: [] }); }
    });
    window.dispatchEvent(new CustomEvent('jellium-mpv-enhancement-ready'));
})();
