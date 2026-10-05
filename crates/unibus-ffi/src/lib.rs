//! C ABI v1. All pointer arguments must be valid; calls on a handle are serialized by the host.
use std::{
    ffi::{CStr, CString, c_char},
    panic::{AssertUnwindSafe, catch_unwind},
};
use unibus_sdk::Session;

fn guarded(f: impl FnOnce() -> i32) -> i32 {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(-2)
}
#[unsafe(no_mangle)]
pub extern "C" fn unibus_sdk_abi_version() -> u32 {
    1
}

/// # Safety
/// config must be a NUL-terminated UTF-8 string; out must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unibus_sdk_connect(config: *const c_char, out: *mut *mut Session) -> i32 {
    guarded(|| {
        if config.is_null() || out.is_null() {
            return -1;
        }
        unsafe {
            *out = std::ptr::null_mut();
        }
        let result = (|| {
            let config = unsafe { CStr::from_ptr(config) }.to_str().ok()?;
            Session::connect(serde_json::from_str(config).ok()?).ok()
        })();
        match result {
            Some(session) => {
                unsafe {
                    *out = Box::into_raw(Box::new(session));
                }
                0
            }
            None => -1,
        }
    })
}
/// # Safety
/// handle must be live; json must be NUL-terminated UTF-8; out writable.
/// Free the returned request ID with unibus_sdk_string_free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unibus_sdk_publish(
    handle: *mut Session,
    json: *const c_char,
    out: *mut *mut c_char,
) -> i32 {
    guarded(|| {
        if handle.is_null() || json.is_null() || out.is_null() {
            return -1;
        }
        unsafe {
            *out = std::ptr::null_mut();
        }
        let result = (|| {
            let text = unsafe { CStr::from_ptr(json) }.to_str().ok()?;
            unsafe { &*handle }
                .publish(serde_json::from_str(text).ok()?)
                .ok()
        })();
        match result.and_then(|id| CString::new(id).ok()) {
            Some(id) => {
                unsafe {
                    *out = id.into_raw();
                }
                0
            }
            None => -1,
        }
    })
}
/// # Safety
/// handle must be live; out writable. Free returned JSON with unibus_sdk_string_free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unibus_sdk_poll(handle: *mut Session, out: *mut *mut c_char) -> i32 {
    guarded(|| {
        if handle.is_null() || out.is_null() {
            return -1;
        }
        unsafe {
            *out = std::ptr::null_mut();
        }
        let Some(event) = unsafe { &*handle }.poll() else {
            return 0;
        };
        let Ok(json) = serde_json::to_string(&event) else {
            return -1;
        };
        let Ok(text) = CString::new(json) else {
            return -1;
        };
        unsafe {
            *out = text.into_raw();
        }
        1
    })
}
/// # Safety
/// Same contract as poll; returns a JSON status snapshot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unibus_sdk_status(handle: *mut Session, out: *mut *mut c_char) -> i32 {
    guarded(|| {
        if handle.is_null() || out.is_null() {
            return -1;
        }
        unsafe {
            *out = std::ptr::null_mut();
        }
        let Ok(json) = serde_json::to_string(&unsafe { &*handle }.status()) else {
            return -1;
        };
        let Ok(text) = CString::new(json) else {
            return -1;
        };
        unsafe {
            *out = text.into_raw();
        }
        0
    })
}
/// # Safety
/// pointer must be NULL or a not-yet-freed allocation from this ABI.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unibus_sdk_string_free(pointer: *mut c_char) {
    let _ = guarded(|| {
        if !pointer.is_null() {
            drop(unsafe { CString::from_raw(pointer) });
        }
        0
    });
}
/// # Safety
/// handle must be NULL or a live allocation returned by connect; use once only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unibus_sdk_close(handle: *mut Session) {
    let _ = guarded(|| {
        if !handle.is_null() {
            drop(unsafe { Box::from_raw(handle) });
        }
        0
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn null_inputs_and_owned_string_roundtrip() {
        assert_eq!(unibus_sdk_abi_version(), 1);
        unsafe {
            assert_eq!(
                unibus_sdk_connect(std::ptr::null(), std::ptr::null_mut()),
                -1
            );
            assert_eq!(
                unibus_sdk_publish(std::ptr::null_mut(), std::ptr::null(), std::ptr::null_mut()),
                -1
            );
            assert_eq!(
                unibus_sdk_poll(std::ptr::null_mut(), std::ptr::null_mut()),
                -1
            );
            unibus_sdk_close(std::ptr::null_mut());
            unibus_sdk_string_free(std::ptr::null_mut());
            let string = CString::new("{\"connected\":false}").unwrap().into_raw();
            assert_eq!(
                CStr::from_ptr(string).to_str().unwrap(),
                "{\"connected\":false}"
            );
            unibus_sdk_string_free(string);
        }
    }
}
