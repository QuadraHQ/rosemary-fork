//! Raw access to the host's own shared libraries, for guest code that is meant
//! to drive one: an injected library that talks to the host's OpenXR runtime,
//! say.
//!
//! A guest's `dlopen` of a host library already works, but its `dlsym` is not
//! the host's: names whose host and Darwin behavior differ (`getenv`,
//! `setenv`, `getuid`, ...) are answered with this runtime's implementation,
//! which reads and writes the guest's environment and not the process's. A
//! host library such as the OpenXR loader reads the process's, so the guest
//! needs the host's symbol itself, and these exports are how it asks.
//!
//! Only the native backend can use what comes back. Its guest instructions
//! are the host CPU's, so a returned pointer is called directly: both sides
//! speak the System V AMD64 ABI, the host keeps its thread pointer in `%fs`
//! while the guest keeps Darwin's thread-specific data in `%gs`, and a guest
//! thread is a host thread with its own stack. The emulated backend would need
//! an ABI row per host function, so it is refused.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::Mutex;

use crate::exec::Engine;

/// A guest compiled against Darwin's headers passes Darwin's `dlopen` modes,
/// whose values are not glibc's.
const DARWIN_RTLD_NOW: c_int = 0x2;
const DARWIN_RTLD_GLOBAL: c_int = 0x8;

static ERROR: Mutex<Option<CString>> = Mutex::new(None);

/// What `quadra_host_dlerror` last reported, kept until the next call.
static REPORTED: Mutex<Option<CString>> = Mutex::new(None);

fn fail(message: String) {
    *ERROR.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(CString::new(message).unwrap_or_default());
}

/// Whether guest code may call a host pointer directly.
fn callable() -> bool {
    matches!(
        crate::exec::active().map(|exec| exec.engine()),
        Some(Engine::Native)
    )
}

/// Why a host library cannot be opened, if it cannot.
fn refusal() -> Option<String> {
    if darwin::strict::enabled() {
        return Some(format!("refused by {}", darwin::strict::VAR));
    }
    (!callable()).then(|| "host libraries can only be called from the native backend".into())
}

/// `int quadra_host_native(void)`: 1 when a pointer from [`dlsym`] can be called
/// as a plain C function, 0 under the emulated backend or `ROSEMARY_NO_PASSTHROUGH`.
pub extern "C" fn native() -> c_int {
    c_int::from(refusal().is_none())
}

/// `void *quadra_host_dlopen(const char *path, int mode)`
///
/// The host loader's `dlopen`. `mode` takes Darwin's `RTLD_LAZY`, `RTLD_NOW`
/// and `RTLD_GLOBAL`. Never closed, so a pointer a guest already holds stays
/// valid.
///
/// # Safety
///
/// `path` is null or NUL-terminated.
pub unsafe extern "C" fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void {
    if let Some(reason) = refusal() {
        fail(reason);
        return std::ptr::null_mut();
    }
    let mut flags = if mode & DARWIN_RTLD_NOW != 0 {
        libc::RTLD_NOW
    } else {
        libc::RTLD_LAZY
    };
    flags |= if mode & DARWIN_RTLD_GLOBAL != 0 {
        libc::RTLD_GLOBAL
    } else {
        libc::RTLD_LOCAL
    };
    // SAFETY: the caller states `path` is null or NUL-terminated; the host's
    // loader takes either.
    let handle = unsafe { libc::dlopen(path, flags) };
    if handle.is_null() {
        // SAFETY: `dlerror` returns null or a NUL-terminated string valid until
        // the next call on this thread, copied at once.
        let reason = unsafe { libc::dlerror() };
        let reason = if reason.is_null() {
            "dlopen failed".into()
        } else {
            unsafe { CStr::from_ptr(reason) }
                .to_string_lossy()
                .into_owned()
        };
        fail(reason);
    }
    handle
}

/// `void *quadra_host_dlsym(void *handle, const char *name)`
///
/// The host loader's `dlsym` and nothing else: no Darwin substitute is
/// returned in place of a host symbol. A null `handle` searches the process,
/// as `RTLD_DEFAULT` does.
///
/// # Safety
///
/// `name` is NUL-terminated; `handle` is null or from [`dlopen`].
pub unsafe extern "C" fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void {
    if let Some(reason) = refusal() {
        fail(reason);
        return std::ptr::null_mut();
    }
    let handle = if handle.is_null() {
        libc::RTLD_DEFAULT
    } else {
        handle
    };
    // SAFETY: the caller's guarantees above.
    let found = unsafe { libc::dlsym(handle, name) };
    if found.is_null() {
        // SAFETY: as in `dlopen`.
        let reason = unsafe { libc::dlerror() };
        let reason = if reason.is_null() {
            "symbol not found".into()
        } else {
            unsafe { CStr::from_ptr(reason) }
                .to_string_lossy()
                .into_owned()
        };
        fail(reason);
    }
    found
}

/// `const char *quadra_host_dlerror(void)`, which clears the error it reports.
pub extern "C" fn dlerror() -> *const c_char {
    let taken = ERROR.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut reported = REPORTED.lock().unwrap_or_else(|e| e.into_inner());
    *reported = taken;
    reported
        .as_ref()
        .map_or(std::ptr::null(), |text| text.as_ptr())
}

/// The names this module defines, and the functions they bind to.
fn entries() -> [(&'static str, *const ()); 4] {
    [
        ("_quadra_host_native", native as *const ()),
        ("_quadra_host_dlopen", dlopen as *const ()),
        ("_quadra_host_dlsym", dlsym as *const ()),
        ("_quadra_host_dlerror", dlerror as *const ()),
    ]
}

/// The bridge as a provider. Flat: a library built `-undefined dynamic_lookup`
/// records no library for what it imports.
pub fn provider() -> machish::resolve::SyntheticDylib {
    let mut lib = machish::resolve::SyntheticDylib::new("<rosemary host bridge>");
    lib.flat(true);
    for (name, function) in entries() {
        lib.define_fn(name, function);
    }
    lib
}
