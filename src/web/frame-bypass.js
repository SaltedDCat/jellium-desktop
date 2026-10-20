// Injected only into the main web profile when the native experiment is enabled.
(function() {
    'use strict';
    const clientId = crypto.randomUUID();
    let nextId = 0;
    let pending;
    window._onFrameBypass = function(json, data) {
        const meta = JSON.parse(json);
        if (meta.clientId !== clientId || !pending || meta.requestId !== pending.id) return;
        const request = pending;
        pending = undefined;
        clearTimeout(request.timer);
        const received = performance.now();
        const nativeDuration = meta.sentMs - meta.receivedMs;
        const networkRtt = Math.max(0, received - request.sent - nativeDuration);
        // Midpoint estimate; uncertainty is retained for diagnostics.
        const offset = ((request.sent + received) - (meta.receivedMs + meta.sentMs)) / 2;
        request.resolve({ ...meta, data, received, clockOffsetMs: offset,
            clockUncertaintyMs: networkRtt / 2, deadline: meta.targetMs + offset });
    };
    window.JelliumFrameBypass = Object.freeze({
        version: 1,
        pull() {
            if (pending) return pending.promise;
            let resolveRequest;
            let rejectRequest;
            const promise = new Promise((resolve, reject) => { resolveRequest = resolve; rejectRequest = reject; });
            const id = ++nextId;
            const timer = setTimeout(() => {
                if (pending?.id !== id) return;
                pending = undefined;
                rejectRequest(new Error('Frame bypass IPC timeout'));
            }, 3000);
            pending = { id, sent: performance.now(), resolve: resolveRequest, timer, promise };
            window.jmpNative.frameBypassPull(id, clientId);
            return promise;
        }
    });
    window.dispatchEvent(new CustomEvent('jellium-frame-bypass-ready'));
})();
