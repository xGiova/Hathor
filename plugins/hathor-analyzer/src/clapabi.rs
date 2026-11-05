//! The hand-wired CLAP entry table — the only `unsafe` in this crate.
//!
//! ## Why hand-wired
//!
//! See the crate doc: a host does `dlsym(handle, "clap_entry")` and treats the
//! returned address as a `const clap_plugin_entry`. That means the struct must
//! live **by value at the exported symbol**, and Rust static initializers
//! cannot copy another static's value — so the `clap_entry!` macro's
//! `pub static CLAP_ENTRY` cannot be re-exported under a new symbol by value.
//! Everything below instead re-derives the same tiny table from the wrapper's
//! audited public pieces; no policy (init refusal, double-init, factory id
//! matching, instance creation) is reimplemented here — it all delegates.
//!
//! ## Raw pointers and who owns them
//!
//! * `init` / `get_factory` receive pointers from the host, valid for the call
//!   only (CLAP contract, expected NOT VERIFIED); we forward without storing.
//! * `&raw const FACTORY` is the address of a private static — valid for the
//!   life of the library; the host may keep it until `deinit`.
//! * `create_plugin` returns a wrapper-managed instance pointer (leaked box,
//!   reclaimed in `destroy`) — ownership rules live in `hathor_clap::plugin`.

use core::ffi::{c_char, c_void};

use hathor_clap::sys::entry::clap_plugin_entry;
use hathor_clap::sys::factory::plugin_factory::clap_plugin_factory;
use hathor_clap::sys::host::clap_host;
use hathor_clap::sys::plugin::{clap_plugin, clap_plugin_descriptor};
use hathor_clap::sys::version::CLAP_VERSION;
use hathor_clap::Plugin;

use crate::AnalyzerPlugin;

unsafe extern "C" fn get_plugin_count(_factory: *const clap_plugin_factory) -> u32 {
    // This factory carries exactly one plugin (crate decision, v0).
    1
}

unsafe extern "C" fn get_plugin_descriptor(
    _factory: *const clap_plugin_factory,
    index: u32,
) -> *const clap_plugin_descriptor {
    if index == 0 {
        <AnalyzerPlugin as Plugin>::DESCRIPTOR
    } else {
        core::ptr::null()
    }
}

unsafe extern "C" fn create_plugin(
    _factory: *const clap_plugin_factory,
    host: *const clap_host,
    plugin_id: *const c_char,
) -> *const clap_plugin {
    // SAFETY: `host` and `plugin_id` are the host's pointers for the duration
    // of the call — exactly the contract `create_instance` documents; it
    // null-checks the id and leaks one instance per success, reclaimed in
    // `destroy`.
    unsafe { hathor_clap::create_instance::<AnalyzerPlugin>(host, plugin_id) }
}

static FACTORY: clap_plugin_factory = clap_plugin_factory {
    get_plugin_count: Some(get_plugin_count),
    get_plugin_descriptor: Some(get_plugin_descriptor),
    create_plugin: Some(create_plugin),
};

unsafe extern "C" fn init(plugin_path: *const c_char) -> bool {
    // SAFETY: `plugin_path` is the host's string for this call; `init_impl`
    // never dereferences it (documented policy: presence check only).
    unsafe { hathor_clap::entry::init_impl(plugin_path) }
}

extern "C" fn deinit() {
    hathor_clap::entry::deinit_impl();
}

unsafe extern "C" fn get_factory(factory_id: *const c_char) -> *const c_void {
    // SAFETY: `factory_id` is the host's string for this call and `FACTORY` is
    // a static whose address is valid for the life of the library — the host
    // keeps it at most until `deinit` (CLAP contract; expected, NOT VERIFIED).
    unsafe { hathor_clap::entry::factory_for(factory_id, &raw const FACTORY) }
}

/// The table the host resolves as the symbol `clap_entry`. Exported BY VALUE
/// at that address — which is exactly what a CLAP host's `dlsym` expects.
#[no_mangle]
pub static clap_entry: clap_plugin_entry = clap_plugin_entry {
    clap_version: CLAP_VERSION,
    init: Some(init),
    deinit: Some(deinit),
    get_factory: Some(get_factory),
};
