//! Ordered GLSL file loading through libmpv's asynchronous property API.
//! A successful reply confirms property acceptance, not GPU compilation.
use crate::{Error, Event, Result, boot, sys};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

fn pending() -> &'static Mutex<HashMap<u64, Sender<i32>>> {
    static PENDING: OnceLock<Mutex<HashMap<u64, Sender<i32>>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Receipt for the eventual MPV_EVENT_SET_PROPERTY_REPLY. The event pump
/// must continue running while a caller waits on a background thread.
pub struct ShaderWrite {
    id: u64,
    receiver: Receiver<i32>,
}

impl ShaderWrite {
    /// `Ok(None)` means no reply yet; callers can check cancellation between polls.
    pub fn poll(&self, timeout: Duration) -> std::result::Result<Option<()>, String> {
        match self.receiver.recv_timeout(timeout) {
            Ok(code) if code >= 0 => Ok(Some(())),
            Ok(code) => Err(format!(
                "mpv rejected GLSL list: {} ({code})",
                Error::new(code)
            )),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err("mpv GLSL reply channel closed".to_owned()),
        }
    }
}

impl Drop for ShaderWrite {
    fn drop(&mut self) {
        pending().lock().remove(&self.id);
    }
}

pub fn set_glsl_shaders(paths: &[String]) -> Result<ShaderWrite> {
    static NEXT: AtomicU64 = AtomicU64::new(0x4a45_4c4c_0000_0000);
    let handle = boot::current_handle()
        .ok_or_else(|| Error::new(sys::mpv_error::MPV_ERROR_UNINITIALIZED.0))?;
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let (sender, receiver) = bounded(1);
    pending().lock().insert(id, sender);
    let write = ShaderWrite { id, receiver };
    handle.set_property_string_array_async(id, "glsl-shaders", paths)?;
    Ok(write)
}

/// Clear the active list without waiting. Suitable for stop/switch cleanup.
pub fn clear_glsl_shaders() -> Result<()> {
    let handle = boot::current_handle()
        .ok_or_else(|| Error::new(sys::mpv_error::MPV_ERROR_UNINITIALIZED.0))?;
    handle.set_property_string_array_async(0, "glsl-shaders", &[])
}

pub(crate) fn complete(event: &Event) {
    if let Event::SetPropertyReply { reply, error } = event
        && let Some(sender) = pending().lock().remove(reply)
    {
        let _ = sender.try_send(*error);
    }
}
