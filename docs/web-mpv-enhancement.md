# Web-owned mpv enhancement

The Jellyfin Web enhancement menu chooses algorithms, model versions, effect
order, CRT parameters and processing dimensions. Web exports an ordered list
of complete mpv GLSL source strings. Jellium caches these strings and loads
their paths through libmpv. The desktop contains no algorithm registry or model
files. This path does not use the experimental decoded-frame bypass or create
a browser video/canvas for native playback.

## Bridge

Jellium injects this API into the main Web browser:

```js
const result = await window.JelliumMpvEnhancement.set({
    revision: 1,
    shaders: [
        { id: 'web-selected-stage', source: '//!HOOK MAIN\n...' }
    ]
});
await window.JelliumMpvEnhancement.clear();
```

`version` is `1`. A successful result contains `revision`, `state` (`loaded` or
`off`), `shaderCount` and `validated: false`. Success means mpv accepted the
ordered shader list; it does **not** prove that the current GPU compiled or
executed every pass. Native rejection, caching errors, timeouts and superseded
requests reject the promise. The menu reports this distinction and does not
invent WebGPU FPS measurements for native mpv.

Limits: 64 shader files, 8 MiB UTF-8 per source, 32 MiB total source, 48 MiB JSON
payload, and a non-negative JavaScript-safe revision. Identifiers are labels,
not paths. The native process validates these bounds again.

The injected bridge correlates responses with a random document identifier and
request ID. A newer request cancels the previous promise. The native worker
keeps only one queued request and supersedes stale work. Revision is echoed
from Web and does not control native ordering.

## Cache and lifecycle

Files live in `jfn_paths::cache_dir()/mpv-web-glsl-v1/`, with SHA-256 filenames
derived from source content. Writes are atomic; changed identifiers reuse the
same source file, duplicate entries retain their position in the list, and
damaged cached contents are repaired. Old inactive entries are pruned to a
256 MiB budget. Web cannot specify a local filename or cache directory.

The ordered list uses an `MPV_FORMAT_NODE_ARRAY` property write, avoiding
platform-specific path-list escaping. Submission is asynchronous and mpv's
event pump completes the property receipt; no synchronous mpv property query
is made from a callback.

Playback load, stop and browser close invalidate queued work and clear the
active list. Load/stop also notify Web through
`jellium-mpv-enhancement-reset`. The menu controller re-exports the selection
after load/playing and source metadata changes. Disable, controller destruction
and export errors clear the list. Browser HTML video continues to use its
existing WebGPU controller; native mpv takes precedence over frame bypass.
The existing SDR eligibility rule remains in Web.

## Web export

The companion Jellyfin Web implementation is in
`src/lib/nijilucid/mpv/` and `src/components/videoEnhancement/mpvController.ts`.
It includes pinned Anime4K, ArtCNN, ACNet/ARNet and CuNNy GLSL, plus the GLSL port
of the current CRT effect. Upstream sources and licenses are shipped alongside
the Web build under `licenses/nijilucid-mpv/`.

Web adapts saved texture names per effect instance and schedules effects on
`MAIN` so arbitrary RGB/luma effect order and repeated upscalers work. Luma
models receive the same BT.709 RGB luma extraction/recomposition as the current
Web implementation. Intermediate and final resizes honor the selected target.
Optional half-precision models use their upstream float fallback for mpv
compatibility; fragment passes with many bound textures use a compute wrapper
to avoid OpenGL vertex attribute limits. Model weights remain unchanged.

These changes do not establish pixel identity with WebGPU: color conversion,
texture precision and GPU arithmetic depend on mpv and the driver. Real-time
performance requires testing at the intended video resolution on the target GPU.

The validated rendering path is `vo=gpu-next` with libplacebo/Vulkan. Exploratory
headless EGL tests using the older libmpv OpenGL render backend produced black
readbacks in some configurations, so that path is not validated by this change.
The bridge's successful list receipt must not be interpreted as GPU validation.

## Verification

Native bridge tests:

```sh
node src/web/tests/mpv-enhancement.test.cjs
cargo test --manifest-path src/Cargo.toml -p jfn-cef enhancement::tests
cargo test --manifest-path src/Cargo.toml -p jfn-mpv shaders::tests
```

Web regression tests also verify all registered effects, source hashes,
arbitrary chain order, repeated stages, menu integration and stale-request
handling. They can export representative plans for real shader checks:

```sh
MPV_SHADER_PLAN_DUMP=/tmp/mpv-plans.json npx vitest run tests/unit/nijilucid/mpv.test.ts
```

From the Jellium repository, run the exported plans through the libplacebo
shader engine used by gpu-next, using an FFmpeg build with libplacebo/Vulkan:

```sh
python3 dev/tests/mpv_shader_smoke.py --backend libplacebo-vulkan \
  --plans /tmp/mpv-plans.json --report /tmp/mpv-vulkan.json
```

These checks compile and render small fixtures, reject shader errors and black
output, and record pixel hashes. They do not exercise CEF IPC end to end,
measure production GPU speed, or replace visible-browser playback testing.
