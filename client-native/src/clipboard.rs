//! Windows clipboard monitor and writer.
//!
//! Polls GetClipboardSequenceNumber every 200 ms.
//! On change, reads CF_UNICODETEXT and sends via the transport.
//! Deduplicates against the last text we received from the server
//! to prevent echo loops.

use std::sync::{Arc, Mutex};

/// Spawn the clipboard monitor thread.
///
/// Returns an `Arc<Mutex<Option<String>>>` that is written to every time we
/// set the Windows clipboard from a server-pushed value.  The poll thread
/// reads it to suppress echoing that value back to the server.
pub fn spawn_monitor(sender: crate::transport::TransportSender) -> Arc<Mutex<Option<String>>> {
    let last_set: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let last_set_clone = last_set.clone();

    std::thread::Builder::new()
        .name("clipboard-poll".into())
        .spawn(move || {
            poll_loop(sender, last_set_clone);
        })
        .expect("failed to spawn clipboard poll thread");

    last_set
}

fn poll_loop(sender: crate::transport::TransportSender, last_set: Arc<Mutex<Option<String>>>) {
    let mut last_seq = unsafe { get_sequence_number() };

    loop {
        std::thread::sleep(std::time::Duration::from_millis(200));

        let seq = unsafe { get_sequence_number() };
        if seq == last_seq {
            continue;
        }
        last_seq = seq;

        let text = match unsafe { read_clipboard_text() } {
            Some(t) if !t.is_empty() => t,
            _ => continue,
        };

        // Skip if this matches what we just set from the server (echo prevention).
        let skip = last_set
            .lock()
            .map(|ls| ls.as_deref() == Some(&text))
            .unwrap_or(false);
        if skip {
            continue;
        }

        sender.send(crate::protocol::encode_clipboard_data(&text));
    }
}

/// Set the Windows clipboard to `text`.  Also records it in `last_set` so
/// the poll thread does not echo it back to the server.
pub fn set_clipboard(text: &str, last_set: &Arc<Mutex<Option<String>>>) {
    // Record before writing so the poll thread sees it even if it wakes mid-write.
    if let Ok(mut guard) = last_set.lock() {
        *guard = Some(text.to_string());
    }
    unsafe { write_clipboard_text(text) };
}

// ---- Windows API wrappers ----

#[cfg(windows)]
mod win {
    use std::ptr;
    use windows_sys::Win32::{
        Foundation::{GlobalFree, HANDLE, HGLOBAL},
        System::{
            DataExchange::{
                CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
                OpenClipboard, SetClipboardData,
            },
            Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
            Ole::CF_UNICODETEXT,
        },
    };

    pub unsafe fn get_sequence_number() -> u32 {
        GetClipboardSequenceNumber()
    }

    pub unsafe fn read_clipboard_text() -> Option<String> {
        if OpenClipboard(ptr::null_mut()) == 0 {
            return None;
        }
        let handle: HANDLE = GetClipboardData(CF_UNICODETEXT as u32);
        let result = if handle.is_null() {
            None
        } else {
            let p = GlobalLock(handle as HGLOBAL) as *const u16;
            if p.is_null() {
                None
            } else {
                let mut len = 0;
                while *p.add(len) != 0 {
                    len += 1;
                }
                let slice = std::slice::from_raw_parts(p, len);
                GlobalUnlock(handle as HGLOBAL);
                String::from_utf16(slice).ok()
            }
        };
        CloseClipboard();
        result
    }

    pub unsafe fn write_clipboard_text(text: &str) {
        let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let byte_size = utf16.len() * 2;
        let hmem: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, byte_size);
        if hmem.is_null() {
            return;
        }
        let p = GlobalLock(hmem) as *mut u16;
        if p.is_null() {
            GlobalFree(hmem);
            return;
        }
        std::ptr::copy_nonoverlapping(utf16.as_ptr(), p, utf16.len());
        GlobalUnlock(hmem);

        if OpenClipboard(ptr::null_mut()) == 0 {
            GlobalFree(hmem);
            return;
        }
        EmptyClipboard();
        if SetClipboardData(CF_UNICODETEXT as u32, hmem as HANDLE).is_null() {
            GlobalFree(hmem);
        }
        CloseClipboard();
    }
}

#[cfg(not(windows))]
mod win {
    pub unsafe fn get_sequence_number() -> u32 {
        0
    }
    pub unsafe fn read_clipboard_text() -> Option<String> {
        None
    }
    pub unsafe fn write_clipboard_text(_text: &str) {}
}

pub unsafe fn get_sequence_number() -> u32 {
    win::get_sequence_number()
}
pub unsafe fn read_clipboard_text() -> Option<String> {
    win::read_clipboard_text()
}
pub unsafe fn write_clipboard_text(text: &str) {
    win::write_clipboard_text(text)
}
