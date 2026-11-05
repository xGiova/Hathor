//! `hathor-clap`: the thin safe wrapper between our plugins and the CLAP host.
//!
//! # Scope, honestly stated
//!
//! This is **not** a general CLAP library — it is the wrapper for *our* plugins
//! (Hathor's personal DAW, solo author, Windows-only). It owns the raw ABI:
//! entry/factory/instance vtables, extension structs, audio-buffer mapping. It
//! owns no DSP, no GUI (there is no GUI anywhere in this project), and no plugin
//! implementation: consumers implement [`plugin::Plugin`] in safe Rust and are
//! wired to a host by [`clap_entry!`]. `hathor-dsp-core` is a *normal*
//! dependency because translating `clap_audio_buffer` into the core `Block`
//! view *is* this wrapper's job — the public surface legitimately exposes those
//! types.
//!
//! # Where the `unsafe` lives, and why it stays here
//!
//! The house rule (STYLE §6, same argument as `apps/hathor-audio-probe/src/pump.rs`):
//! unsafe is confined to pointer reinterpreting at the FFI boundary and carries
//! zero cost — every module doc in this crate names the pointers it touches and
//! who owns them. Concretely:
//!
//! * [`plugin`] — one leaked `Box<Instance<P>>` per live instance; the returned
//!   `*const clap_plugin` points at the `base` header inside it, and every
//!   callback recovers the box from `plugin_data` (`Instance` is `repr(Rust)`,
//!   so no address equality is assumed); `destroy` is the single reclaim point.
//! * [`entry`] — raw `init`/`deinit`/`get_factory`: null-safe string compare,
//!   one atomic flag, no allocation on any init path.
//! * [`host`] — borrowed `*const clap_host`: extensions fetched per call and
//!   never cached, every vtable slot null-checked.
//! * [`process`] — host-owned audio arrays reborrowed as slices/`Block`s,
//!   confined to the call's lifetime; the layout argument (CLAP is planar by
//!   contract, so a channel *is* a contiguous f32 array) is the no-copy bridge
//!   to `hathor_dsp_core::buffer::Block`.
//! * [`events`] — call-scoped event lists; a wide event is read only after its
//!   own header says size/type/space match.
//!
//! A consumer that implements [`plugin::Plugin`] writes no `unsafe` and never
//! sees a raw pointer; a `cdylib` plugin binary is one `static` plus the macro.
//!
//! # Verification story (and its limits)
//!
//! The integration test `tests/mock_host.rs` drives the entire C-ABI lifecycle
//! against a minimal mock host — init → get_factory → create → add_extensions →
//! activate(48000,64) → process (sine in, output touched & non-NaN) → get_state
//! → reload → params flush → deactivate → destroy → deinit — and asserts zero
//! allocations on the process path under the workspace `AudioSection` STRICT
//! guard. **That mock test is the official proxy for "the plugin loads in a
//! host"; loading in a real DAW (Reaper) is an OWNER step after task-4, not
//! something this crate can claim yet.** Host-behavior assumptions the wrapper
//! makes (pointer stability, call ordering) are labeled *expected, NOT VERIFIED*
//! at each site.
//!
//! ```ignore
//! // the whole plugin binary, roughly:
//! struct MyPlugin;            // impl hathor_clap::Plugin — safe Rust only
//! static DESC: clap_plugin_descriptor = hathor_clap::make_descriptor(…);
//! hathor_clap::clap_entry!(my_entry, MyPlugin);
//! // that's the complete wiring: the macro itself emits the
//! // `#[no_mangle] pub static clap_entry` the host resolves; add
//! // `pub use my_entry::clap_entry;` if the crate wants the Rust path too.
//! ```

#![warn(missing_docs)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::exit)
)]

pub mod abi;
mod cstr;
mod descriptor;
pub mod entry;
mod events;
mod host;
pub mod plugin;
mod process;

/// Raw CLAP ABI, re-exported so plugin binaries can name the entry static's
/// type without depending on `clap-sys` themselves.
pub mod sys {
    pub use clap_sys::*;
}

/// The macro's own re-export surface: [`clap_cstr!`] expands through it so it
/// works inside other crates without a `std::ffi` import of their own.
#[doc(hidden)]
pub mod __reexport {
    pub use core::ffi::CStr;
}

// `clap_cstr!` and `clap_entry!` are `#[macro_export]` macros: they land at this
// crate's root automatically — no re-export line to write (or double-write).
pub use cstr::read_padded_name;
pub use descriptor::{make_descriptor, FeatureList};
pub use entry::is_initialized;
pub use events::{InEvents, OutEvents, ParamUpdate};
pub use host::{Host, LogSeverity};
pub use plugin::{
    create_instance, AudioPortInfo, InputStream, OutputStream, ParamInfo, ParamText, Plugin,
};
pub use process::{ProcessStatus, ProcessView};
