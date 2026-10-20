//! Web chooses the GLSL sources and order; native only caches and loads files.
use crate::ipc::{BrowserMessage, list_int, list_string};
use cef::*;
use crossbeam_channel::{Receiver, Sender, bounded};
use parking_lot::Mutex;
use ring::digest::{SHA256, digest};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const MAX_SHADERS: usize = 64;
const MAX_SOURCE: usize = 8 * 1024 * 1024;
const MAX_TOTAL: usize = 32 * 1024 * 1024;
const MAX_PAYLOAD: usize = 48 * 1024 * 1024;
const CACHE_BUDGET: u64 = 256 * 1024 * 1024;
const MAX_SAFE_REVISION: u64 = 9_007_199_254_740_991;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    revision: u64,
    shaders: Vec<Shader>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Shader {
    id: String,
    source: String,
}

impl Plan {
    fn parse(payload: &str) -> Result<Self, String> {
        if payload.len() > MAX_PAYLOAD {
            return Err("GLSL request exceeds 48 MiB".to_owned());
        }
        let plan: Self =
            serde_json::from_str(payload).map_err(|e| format!("Invalid GLSL plan: {e}"))?;
        if plan.revision > MAX_SAFE_REVISION || plan.shaders.len() > MAX_SHADERS {
            return Err("Invalid GLSL revision or shader count (maximum 64)".to_owned());
        }
        let mut total = 0;
        for shader in &plan.shaders {
            if shader.id.is_empty() || shader.id.len() > 128 {
                return Err("GLSL shader id must contain 1 to 128 UTF-8 bytes".to_owned());
            }
            if shader.source.is_empty()
                || shader.source.len() > MAX_SOURCE
                || shader.source.contains('\0')
            {
                return Err(
                    "GLSL source must contain 1 byte to 8 MiB without NUL characters".to_owned(),
                );
            }
            total += shader.source.len();
            if total > MAX_TOTAL {
                return Err("GLSL source total exceeds 32 MiB".to_owned());
            }
        }
        Ok(plan)
    }
}

fn cache_name(source: &str) -> String {
    let hash = digest(&SHA256, source.as_bytes());
    let hex: String = hash
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{hex}.glsl")
}

fn cache_plan(root: &Path, plan: &Plan) -> Result<Vec<String>, String> {
    fs::create_dir_all(root).map_err(|e| format!("Cannot create GLSL cache: {e}"))?;
    let mut paths = Vec::with_capacity(plan.shaders.len());
    for shader in &plan.shaders {
        let path = root.join(cache_name(&shader.source));
        // Content check also repairs a truncated or otherwise damaged cache entry.
        let matches = fs::symlink_metadata(&path).is_ok_and(|m| m.is_file())
            && fs::read(&path).is_ok_and(|bytes| bytes == shader.source.as_bytes());
        if !matches {
            jfn_paths::write_atomic(&path, shader.source.as_bytes())
                .map_err(|e| format!("Cannot cache GLSL source: {e}"))?;
        }
        paths.push(
            path.to_str()
                .ok_or("GLSL cache path is not UTF-8")?
                .to_owned(),
        );
    }
    Ok(paths)
}

fn prune_cache(root: &Path, protected: &[String]) {
    let protected: HashSet<_> = protected.iter().map(PathBuf::from).collect();
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut entries: Vec<_> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            if name.len() != 69
                || !name.ends_with(".glsl")
                || !name[..64].bytes().all(|b| b.is_ascii_hexdigit())
            {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            if !metadata.is_file() {
                return None;
            }
            Some((metadata.modified().ok(), metadata.len(), path))
        })
        .collect();
    entries.sort_by_key(|entry| entry.0);
    let mut size: u64 = entries.iter().map(|entry| entry.1).sum();
    for (_, length, path) in entries {
        if size <= CACHE_BUDGET {
            break;
        }
        if !protected.contains(&path) && fs::remove_file(path).is_ok() {
            size = size.saturating_sub(length);
        }
    }
}

// Serialize the last-generation check with submission/stop to avoid a stale
// cache worker submitting after a newer stop has already cleared mpv.
static GENERATION: Mutex<u64> = Mutex::new(0);

pub(crate) fn reset() {
    let mut generation = GENERATION.lock();
    *generation = generation.wrapping_add(1);
    if let Err(error) = jfn_mpv::shaders::clear_glsl_shaders() {
        jfn_logging::log(
            jfn_logging::CATEGORY_CEF,
            jfn_logging::LEVEL_WARN,
            &format!("Cannot clear Web GLSL list: {error}"),
        );
    }
}

/// Notify the injected bridge so Web can invalidate its cached applied plan.
pub(crate) fn reset_for(message: &BrowserMessage, reason: &str) {
    reset();
    if let Some(frame) = message.main_frame() {
        crate::ipc::send_to_renderer(&frame, "mpvEnhancementResult", |args| {
            args.set_string(
                0,
                Some(&CefString::from(
                    json!({
                        "requestId": 0, "state": "reset", "reason": reason,
                    })
                    .to_string()
                    .as_str(),
                )),
            );
        });
    }
}

struct Request {
    frame: Frame,
    request_id: i32,
    client_id: String,
    generation: u64,
    payload: String,
}

struct Mailbox {
    sender: Sender<Request>,
    receiver: Receiver<Request>,
}

fn worker() -> &'static Mailbox {
    static WORKER: OnceLock<Mailbox> = OnceLock::new();
    WORKER.get_or_init(|| {
        let (sender, receiver) = bounded::<Request>(1);
        let worker_receiver = receiver.clone();
        std::thread::spawn(move || {
            let root = jfn_paths::cache_dir().join("mpv-web-glsl-v1");
            let mut active_paths = Vec::new();
            for request in worker_receiver {
                let result = apply(&request, &root, &mut active_paths);
                reply(request, result);
            }
        });
        Mailbox { sender, receiver }
    })
}

fn apply(request: &Request, root: &Path, active_paths: &mut Vec<String>) -> Value {
    let plan = match Plan::parse(&request.payload) {
        Ok(plan) => plan,
        Err(error) => return json!({ "error": error }),
    };
    let mut protected = active_paths.clone();
    let result = (|| {
        if *GENERATION.lock() != request.generation {
            return Err("GLSL plan superseded".to_owned());
        }
        let paths = cache_plan(root, &plan)?;
        protected.extend(paths.iter().cloned());
        let write = {
            let generation = GENERATION.lock();
            if *generation != request.generation {
                return Err("GLSL plan superseded".to_owned());
            }
            jfn_mpv::shaders::set_glsl_shaders(&paths)
                .map_err(|e| format!("Cannot load GLSL list: {e}"))?
        };
        let started = Instant::now();
        loop {
            if *GENERATION.lock() != request.generation {
                return Err("GLSL plan superseded".to_owned());
            }
            if write.poll(Duration::from_millis(100))?.is_some() {
                break;
            }
            if started.elapsed() >= Duration::from_secs(10) {
                return Err("mpv GLSL property reply timed out".to_owned());
            }
        }
        *active_paths = paths;
        if *GENERATION.lock() != request.generation {
            return Err("GLSL plan superseded".to_owned());
        }
        Ok(())
    })();
    // Include newly submitted paths even after a timeout/superseding request:
    // mpv may still be compiling them before the next property write arrives.
    prune_cache(root, &protected);
    match result {
        Ok(()) => {
            json!({ "revision": plan.revision, "state": if plan.shaders.is_empty() { "off" } else { "loaded" },
            "shaderCount": plan.shaders.len(), "validated": false })
        }
        Err(error) => json!({ "revision": plan.revision, "error": error }),
    }
}

pub(crate) fn request(message: &BrowserMessage) -> bool {
    let (Some(frame), Some(args)) = (message.main_frame(), message.args()) else {
        return true;
    };
    let request_id = list_int(args, 1);
    let client_id = list_string(args, 2);
    if request_id <= 0 || client_id.is_empty() || client_id.len() > 128 {
        return true;
    }
    let payload = list_string(args, 0);
    let mut generation = GENERATION.lock();
    *generation = generation.wrapping_add(1);
    let request = Request {
        frame,
        request_id,
        client_id,
        generation: *generation,
        payload,
    };
    if request.payload.len() > MAX_PAYLOAD {
        reply(request, json!({ "error": "GLSL request exceeds 48 MiB" }));
    } else {
        let mailbox = worker();
        while let Ok(stale) = mailbox.receiver.try_recv() {
            reply(stale, json!({ "error": "GLSL plan superseded" }));
        }
        if let Err(error) = mailbox.sender.try_send(request) {
            reply(
                error.into_inner(),
                json!({ "error": "GLSL loader is unavailable" }),
            );
        }
    }
    true
}

fn reply(request: Request, mut result: Value) {
    result["requestId"] = json!(request.request_id);
    result["clientId"] = json!(request.client_id);
    let mut task = ReplyTask::new(request.frame, result.to_string());
    post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct ReplyTask { frame: Frame, json: String, }
    impl Task {
        fn execute(&self) {
            if self.frame.is_valid() != 1 { return; }
            crate::ipc::send_to_renderer(&self.frame, "mpvEnhancementResult", |args| {
                args.set_string(0, Some(&CefString::from(self.json.as_str())));
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_is_sha256_and_identifiers_cannot_supply_paths() {
        assert_eq!(
            cache_name("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad.glsl"
        );
        assert!(
            Plan::parse(r#"{"revision":1,"shaders":[{"id":"../../x","source":"abc"}]}"#).is_ok()
        );
        assert!(Plan::parse(r#"{"revision":1,"shaders":[{"id":"x","path":"/tmp/x"}]}"#).is_err());
    }

    #[test]
    fn invalid_protocol_is_rejected() {
        for json in [
            r#"{"revision":-1,"shaders":[]}"#,
            r#"{"revision":1.5,"shaders":[]}"#,
            r#"{"revision":9007199254740992,"shaders":[]}"#,
            r#"{"revision":1,"shaders":[{"id":"a","source":""}]}"#,
            r#"{"revision":1,"shaders":[{"id":"a","source":"a\u0000b"}]}"#,
        ] {
            assert!(Plan::parse(json).is_err(), "{json}");
        }
        let shaders = (0..65)
            .map(|_| json!({ "id": "a", "source": "abc" }))
            .collect::<Vec<_>>();
        assert!(Plan::parse(&json!({ "revision":1,"shaders":shaders }).to_string()).is_err());
    }

    #[test]
    fn cache_pruning_keeps_active_sources_and_ignores_unrelated_files()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let active = root.path().join(cache_name("active"));
        let stale = root.path().join(cache_name("stale"));
        fs::write(&active, "active")?;
        fs::File::create(&stale)?.set_len(CACHE_BUDGET + 1)?;
        fs::write(root.path().join("README"), "unrelated")?;
        prune_cache(root.path(), &[active.to_string_lossy().into_owned()]);
        assert!(active.exists());
        assert!(!stale.exists());
        assert!(root.path().join("README").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn cache_repairs_symlinks_without_writing_their_target()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let target = root.path().join("outside-cache");
        fs::write(&target, "unchanged")?;
        let cache = root.path().join("cache");
        fs::create_dir(&cache)?;
        let path = cache.join(cache_name("abc"));
        std::os::unix::fs::symlink(&target, &path)?;
        let plan = Plan::parse(r#"{"revision":1,"shaders":[{"id":"../target","source":"abc"}]}"#)?;
        cache_plan(&cache, &plan)?;
        assert_eq!(fs::read_to_string(target)?, "unchanged");
        assert_eq!(fs::read_to_string(&path)?, "abc");
        assert!(!fs::symlink_metadata(path)?.is_symlink());
        Ok(())
    }

    #[test]
    fn cache_reuses_content_preserves_order_and_repairs_corruption()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let plan = Plan::parse(
            r#"{"revision":2,"shaders":[{"id":"../x","source":"abc"},{"id":"y","source":"xyz"},{"id":"z","source":"abc"}]}"#,
        )?;
        let paths = cache_plan(root.path(), &plan)?;
        assert_eq!(paths[0], paths[2]);
        assert_ne!(paths[0], paths[1]);
        assert_eq!(fs::read_dir(root.path())?.count(), 2);
        fs::write(&paths[0], "damaged")?;
        assert_eq!(cache_plan(root.path(), &plan)?, paths);
        assert_eq!(fs::read_to_string(&paths[0])?, "abc");
        assert!(
            paths
                .iter()
                .all(|p| Path::new(p).parent() == Some(root.path()))
        );
        Ok(())
    }
}
