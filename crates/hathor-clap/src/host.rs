//! The safe [`Host`] handle: everything a plugin may call *out* to the host.
//!
//! ## Raw pointers in this module, and why they are sound
//!
//! The only raw state here is the borrowed `*const clap_host` the factory handed
//! to `create_plugin`. Ownership: the host owns the struct and its extension
//! structs; the pointer is valid at least until `destroy` of the plugin that
//! received it (CLAP 1.2 host contract — expected, NOT VERIFIED against specific
//! hosts). The wrapper therefore (a) never stores an extension pointer: every
//! host call re-fetches by id and consumes the borrow inside one closure, so even
//! a host that frees the struct the moment the plugin dies cannot leave a dangling
//! borrow here; (b) checks every vtable function pointer for null before calling
//! (hosts legally omit extensions); (c) is a plain pointer copy — no ownership,
//! no data — which is what makes it safe to mark [`Host`] `Send` even though the
//! CLAP *threading rules* for each call remain the consumer's to obey.

use core::ffi::CStr;

use crate::sys;
use crate::sys::ext::audio_ports::{clap_host_audio_ports, CLAP_EXT_AUDIO_PORTS};
use crate::sys::ext::latency::{clap_host_latency, CLAP_EXT_LATENCY};
use crate::sys::ext::log::{clap_host_log, CLAP_EXT_LOG};
use crate::sys::ext::params::{clap_host_params, CLAP_EXT_PARAMS};
use crate::sys::ext::state::{clap_host_state, CLAP_EXT_STATE};
use crate::sys::host::clap_host;

/// A borrowed handle on the host that loaded this plugin.
///
/// `Copy` by design: it is one pointer. `Send` so a plugin created on the main
/// thread can log from the audio thread; deliberately **not** `Sync`: the wrapper
/// will not promise that a host's vtable is reentrant from two threads at once
/// (expected, NOT VERIFIED — hosts vary), and a consumer that needs that has to
/// say so out loud.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Host {
    sys: *const clap_host,
}

/// CLAP log severities, mapped to the `clap.log` constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LogSeverity {
    /// Chatty development detail.
    Debug,
    /// Normal information.
    Info,
    /// Recoverable oddity.
    Warning,
    /// Recoverable failure.
    Error,
    /// The plugin cannot continue.
    Fatal,
}

impl LogSeverity {
    fn to_sys(self) -> sys::ext::log::clap_log_severity {
        match self {
            Self::Debug => sys::ext::log::CLAP_LOG_DEBUG,
            Self::Info => sys::ext::log::CLAP_LOG_INFO,
            Self::Warning => sys::ext::log::CLAP_LOG_WARNING,
            Self::Error => sys::ext::log::CLAP_LOG_ERROR,
            Self::Fatal => sys::ext::log::CLAP_LOG_FATAL,
        }
    }
}

impl Host {
    /// Wraps the host pointer from `create_plugin`. `None` for null: the wrapper
    /// never builds a live handle out of nothing.
    pub(crate) const fn from_raw(sys: *const clap_host) -> Option<Self> {
        if sys.is_null() {
            None
        } else {
            Some(Self { sys })
        }
    }

    /// The raw pointer, for diagnostics and for the plugin task's own bookkeeping.
    pub const fn as_raw(&self) -> *const clap_host {
        self.sys
    }

    /// Borrows the host struct itself for one read: the returned reference lives
    /// only inside the caller's expression because Rust ties it to `&self`.
    fn host(&self) -> &clap_host {
        // SAFETY: `from_raw` rejected null; the host owns this struct and keeps it
        // valid for the life of the plugin (CLAP host contract, expected NOT
        // VERIFIED). Reading it is a plain aligned load of a `#[repr(C)]` struct
        // whose layout is pinned by the compile-time asserts in [`crate::abi`].
        unsafe { &*self.sys }
    }

    /// The only door to host extensions: fetch by id and immediately consume the
    /// borrow; the extension pointer is never stored.
    fn with_ext<T>(&self, id: &CStr, use_it: impl FnOnce(&T) -> bool) -> bool {
        let Some(get) = self.host().get_extension else {
            return false; // a host with no get_extension advertises no extensions
        };
        let raw = unsafe {
            // SAFETY: the host contract makes `get_extension(self, id)` legal for
            // any NUL-terminated id; ours are `clap_cstr!` constants from
            // `clap-sys` itself. The answer is either null or a pointer to the
            // `T` named by that exact id (the id/type pairing is the CLAP spec,
            // and every `T` used here has its layout pinned in [`crate::abi`]).
            get(self.sys, id.as_ptr())
        };
        if raw.is_null() {
            return false;
        }
        let typed: *const T = raw.cast();
        if typed.align_offset(core::mem::align_of::<T>()) != 0 {
            // A well-behaved host returns a real `T`; a misaligned answer is host
            // misbehavior — refuse the call rather than risk an unaligned load.
            return false;
        }
        // SAFETY: null and alignment just checked; the pointee is a valid `T` by
        // the id/type pairing above; the borrow dies inside this statement.
        unsafe { use_it(&*typed) }
    }

    /// Forwards a NUL-terminated message to `clap.log`. `false` if the host does
    /// not offer the extension or the vtable slot is null.
    pub fn log(&self, severity: LogSeverity, message: &CStr) -> bool {
        let sev = severity.to_sys();
        let ptr = message.as_ptr();
        self.with_ext::<clap_host_log>(CLAP_EXT_LOG, |ext| {
            let Some(log) = ext.log else { return false };
            unsafe {
                // SAFETY: `message`'s bytes are a valid C string for the duration
                // of this call (the caller passed a borrowed `&CStr`); the host
                // reads it synchronously (CLAP rule — expected, NOT VERIFIED per
                // host).
                log(self.sys, sev, ptr)
            }
            true
        })
    }

    /// Requests the host restart the plugin (re-create with new ports/params).
    pub fn request_restart(&self) -> bool {
        let Some(f) = self.host().request_restart else {
            return false;
        };
        unsafe {
            // SAFETY: plain host callback, no arguments to keep valid.
            f(self.sys)
        }
        true
    }

    /// Asks the host to run at least one more process cycle (tail-off and the
    /// like). Main-thread-only per CLAP; the wrapper forwards, the caller obeys.
    pub fn request_process(&self) -> bool {
        let Some(f) = self.host().request_process else {
            return false;
        };
        unsafe {
            // SAFETY: plain host callback, no borrowed arguments.
            f(self.sys)
        }
        true
    }

    /// Asks the host to call `on_main_thread` soon.
    pub fn request_callback(&self) -> bool {
        let Some(f) = self.host().request_callback else {
            return false;
        };
        unsafe {
            // SAFETY: plain host callback, no borrowed arguments.
            f(self.sys)
        }
        true
    }

    /// Tells the host the port list changed (`CLAP_AUDIO_PORTS_RESCAN_*` flags).
    pub fn audio_ports_rescan(&self, flags: u32) -> bool {
        self.with_ext::<clap_host_audio_ports>(CLAP_EXT_AUDIO_PORTS, |ext| {
            let Some(rescan) = ext.rescan else {
                return false;
            };
            unsafe {
                // SAFETY: plain host callback, `flags` is a value.
                rescan(self.sys, flags)
            }
            true
        })
    }

    /// Tells the host parameter values/info changed (`CLAP_PARAM_RESCAN_*` flags).
    pub fn params_rescan(&self, flags: u32) -> bool {
        self.with_ext::<clap_host_params>(CLAP_EXT_PARAMS, |ext| {
            let Some(rescan) = ext.rescan else {
                return false;
            };
            unsafe {
                // SAFETY: plain host callback, `flags` is a value.
                rescan(self.sys, flags)
            }
            true
        })
    }

    /// Asks the host to flush pending parameter events.
    pub fn params_request_flush(&self) -> bool {
        self.with_ext::<clap_host_params>(CLAP_EXT_PARAMS, |ext| {
            let Some(f) = ext.request_flush else {
                return false;
            };
            unsafe {
                // SAFETY: plain host callback, no borrowed arguments.
                f(self.sys)
            }
            true
        })
    }

    /// Declares that the plugin's latency changed: the host re-reads
    /// `clap.latency` and re-compensates.
    pub fn latency_changed(&self) -> bool {
        self.with_ext::<clap_host_latency>(CLAP_EXT_LATENCY, |ext| {
            let Some(f) = ext.changed else { return false };
            unsafe {
                // SAFETY: plain host callback, no borrowed arguments.
                f(self.sys)
            }
            true
        })
    }

    /// Marks the plugin state dirty so the host re-saves it (project file).
    pub fn state_mark_dirty(&self) -> bool {
        self.with_ext::<clap_host_state>(CLAP_EXT_STATE, |ext| {
            let Some(f) = ext.mark_dirty else {
                return false;
            };
            unsafe {
                // SAFETY: plain host callback, no borrowed arguments.
                f(self.sys)
            }
            true
        })
    }

    /// The host's advertised name, as a borrowed C string.
    pub fn name(&self) -> &CStr {
        // SAFETY: `clap_host.name` is a NUL-terminated string owned by the host
        // for at least the life of this struct (CLAP contract; expected, NOT
        // VERIFIED).
        unsafe { CStr::from_ptr(self.host().name) }
    }
}

/// `Host` carries no ownership and no data — one borrowed, host-stable pointer.
/// Moving the *value* between threads cannot alias anything the wrapper itself
/// protects; the CLAP per-call thread rules stay the consumer's contract to honor
/// (rescan/restart on the main thread, log legal from the audio thread in the
/// hosts we will test — expected, NOT VERIFIED).
unsafe impl Send for Host {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_host_is_rejected_at_the_door() {
        let h = Host::from_raw(core::ptr::null());
        assert!(h.is_none(), "the wrapper never fabricates a live handle");
    }

    #[test]
    fn equality_is_pointer_equality() {
        let a = [0u8; 64];
        let p = core::ptr::from_ref(&a).cast::<clap_host>();
        let h1 = Host::from_raw(p).expect("non-null");
        let h2 = Host::from_raw(p).expect("non-null");
        assert_eq!(h1, h2, "one host, two copies, same handle");
    }

    #[test]
    fn severity_maps_to_clap_constants() {
        assert_eq!(LogSeverity::Debug.to_sys(), sys::ext::log::CLAP_LOG_DEBUG);
        assert_eq!(
            LogSeverity::Fatal.to_sys(),
            sys::ext::log::CLAP_LOG_FATAL,
            "the mapping table is the one in log.rs, not a guess"
        );
    }
}
