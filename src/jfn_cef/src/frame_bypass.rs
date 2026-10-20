//! Opt-in validation transport. Pull/ack bounds the complete IPC pipeline;
//! pixel packing never runs on mpv's core or CEF's UI thread.
use crate::ipc::BrowserMessage;
use cef::*;

pub(crate) fn enabled() -> bool {
    cfg!(feature = "frame-bypass")
        && std::env::var("JELLIUM_EXPERIMENTAL_FRAME_BYPASS").as_deref() == Ok("1")
}

pub(crate) fn request(message: &BrowserMessage) -> bool {
    #[cfg(feature = "frame-bypass")]
    if enabled() {
        return producer::request(message);
    }
    let _ = message;
    false
}

pub(crate) fn receive(frame: &Frame, args: Option<&ListValue>) {
    let Some(args) = args else { return };
    let Some(ctx) = frame.v8_context() else {
        return;
    };
    if ctx.enter() != 1 {
        return;
    }
    // No V8 value escapes this context or outlives the owned byte buffer.
    if let Some(global) = ctx.global()
        && let Some(callback) = global.value_bykey(Some(&CefString::from("_onFrameBypass")))
        && callback.is_function() == 1
    {
        let metadata = CefString::from(crate::ipc::list_string(args, 0).as_str());
        let mut bytes = args.binary(1).map_or_else(Vec::new, |binary| {
            let mut bytes = vec![0; binary.size().min(1920 * 1080 * 3 / 2)];
            let size = binary.data(Some(&mut bytes), 0);
            bytes.truncate(size);
            bytes
        });
        let values = [
            v8_value_create_string(Some(&metadata)),
            v8_value_create_array_buffer_with_copy(bytes.as_mut_ptr(), bytes.len()),
        ];
        callback.execute_function(None, Some(&values));
    }
    ctx.exit();
}

#[cfg(feature = "frame-bypass")]
mod producer {
    use super::*;
    use crossbeam_channel::{Sender, bounded};
    use jfn_mpv::sys;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    static BUSY: AtomicBool = AtomicBool::new(false);
    #[derive(Clone)]
    struct Request {
        frame: Frame,
        id: i32,
        client_id: String,
        received_ns: i64,
    }

    fn worker() -> &'static Sender<Request> {
        static WORKER: OnceLock<Sender<Request>> = OnceLock::new();
        WORKER.get_or_init(|| {
            let (tx, rx) = bounded::<Request>(1);
            std::thread::spawn(move || {
                let mut buffer = vec![0; 1920 * 1080 * 3 / 2];
                for req in rx {
                    // Acquire and release the mpv owner on the worker. CEF UI
                    // tasks must not become its final owner during shutdown.
                    let Some(handle) = jfn_mpv::boot::current_handle() else {
                        BUSY.store(false, Ordering::Release);
                        continue;
                    };
                    let started = Instant::now();
                    let mut info: sys::mpv_frame_bypass_info = unsafe { std::mem::zeroed() };
                    let size = unsafe {
                        sys::mpv_frame_bypass_read(
                            handle.raw(),
                            buffer.as_mut_ptr(),
                            buffer.len(),
                            &mut info,
                        )
                    };
                    let bytes = buffer[..size.max(0) as usize].to_vec();
                    drop(handle);
                    let metadata = serde_json::json!({
                        "requestId": req.id, "clientId": req.client_id, "generation": info.generation,
                        "sequence": info.sequence, "nativeDropped": info.dropped,
                        "ptsUs": info.pts_us, "targetMs": info.target_ns as f64 / 1e6,
                        "captureMs": info.capture_ns as f64 / 1e6,
                        "receivedMs": req.received_ns as f64 / 1e6,
                        "width": info.width, "height": info.height,
                        "fullRange": info.full_range != 0, "paused": info.paused != 0,
                        "status": if size < 0 { 2 } else { info.status },
                        "readMs": started.elapsed().as_secs_f64() * 1000.0,
                    });
                    let mut task = ReplyTask::new(req.frame, bytes, metadata);
                    if post_task(ThreadId::UI, Some(&mut task)) != 1 {
                        BUSY.store(false, Ordering::Release);
                    }
                }
            });
            tx
        })
    }

    pub(super) fn request(message: &BrowserMessage) -> bool {
        let Some(frame) = message.main_frame() else {
            return true;
        };
        let Some(args) = message.args() else {
            return true;
        };
        if BUSY.swap(true, Ordering::AcqRel) {
            return true;
        }
        if worker()
            .try_send(Request {
                frame,
                id: args.int(0),
                client_id: crate::ipc::list_string(args, 1),
                received_ns: unsafe { sys::mpv_frame_bypass_clock_ns() },
            })
            .is_err()
        {
            BUSY.store(false, Ordering::Release);
        }
        true
    }

    wrap_task! {
        struct ReplyTask {
            frame: Frame,
            bytes: Vec<u8>,
            metadata: serde_json::Value,
        }
        impl Task {
            fn execute(&self) {
                BUSY.store(false, Ordering::Release);
                if self.frame.is_valid() != 1 { return; }
                let mut metadata = self.metadata.clone();
                metadata["sentMs"] = serde_json::json!(unsafe {
                    sys::mpv_frame_bypass_clock_ns() as f64 / 1e6
                });
                crate::ipc::send_to_renderer(&self.frame, "frameBypassFrame", |args| {
                    args.set_string(0, Some(&CefString::from(metadata.to_string().as_str())));
                    if !self.bytes.is_empty()
                        && let Some(mut binary) = binary_value_create(Some(&self.bytes))
                    {
                        args.set_binary(1, Some(&mut binary));
                    }
                });
            }
        }
    }
}
