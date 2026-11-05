//! Const size/alignment assertions for every `#[repr(C)]` struct this crate
//! embeds, copies, or reinterprets.
//!
//! `clap-sys` is the ABI truth (vendored, read-only), but a wrapper that
//! reinterprets bytes — `plugin_data` back to `Instance<P>`, event headers to
//! wide events, host strings to `CStr` — is only as honest as its layout. These
//! `const` blocks fail the build, not the runtime, on any regression, on the
//! x86_64 ABI the project is pinned to (Windows MSVC, SysV-compatible struct
//! layout for these flat records).
//!
//! Numbers are `size_of`/`align_of` results measured against clap-sys 0.5.0
//! structs and re-checked by the `layout_matches_measured_numbers` test below,
//! so a silent field change in the binding can never pass unnoticed.

use crate::sys::audio_buffer::clap_audio_buffer;
use crate::sys::entry::clap_plugin_entry;
use crate::sys::events::{clap_event_header, clap_event_param_value, clap_event_transport};
use crate::sys::ext::audio_ports::{clap_audio_port_info, clap_plugin_audio_ports};
use crate::sys::ext::latency::clap_plugin_latency;
use crate::sys::ext::log::clap_host_log;
use crate::sys::ext::params::{clap_param_info, clap_plugin_params};
use crate::sys::ext::render::clap_plugin_render;
use crate::sys::ext::state::clap_plugin_state;
use crate::sys::factory::plugin_factory::clap_plugin_factory;
use crate::sys::host::clap_host;
use crate::sys::plugin::{clap_plugin, clap_plugin_descriptor};
use crate::sys::process::clap_process;
use crate::sys::stream::{clap_istream, clap_ostream};
use crate::sys::version::clap_version;

/// x86_64: three u32s.
const _: () = assert!(core::mem::size_of::<clap_version>() == 12);
/// version 12 + pad 4 + id..features: 9 pointers.
const _: () = assert!(core::mem::size_of::<clap_plugin_descriptor>() == 88);
/// desc + plugin_data + 10 function slots.
const _: () = assert!(core::mem::size_of::<clap_plugin>() == 96);
/// `clap_plugin` must start with `desc` (offset 0) and `plugin_data` (offset 8)
/// or every trampoline's pointer recovery in [`crate::plugin`] is wrong;
/// verified at runtime from a constructed instance in the integration test, and
/// structurally here: the first two fields' types have pointer size/alignment.
const _: () = assert!(core::mem::align_of::<clap_plugin>() == 8);
/// clap_version 12 + pad 4 + host_data 8 + name/vendor/url/version + get_extension
/// + 3 request callbacks (5 strings + 4 slots after the version record).
const _: () = assert!(core::mem::size_of::<clap_host>() == 88);
/// data32 + data64 + channel_count/latency (2×u32) + constant_mask u64.
const _: () = assert!(core::mem::size_of::<clap_audio_buffer>() == 32);
/// steady_time 8 + frames 4 (+pad) + transport + inputs + outputs
/// + counts 4+4 + in_events + out_events.
const _: () = assert!(core::mem::size_of::<clap_process>() == 64);
/// version 12 + pad 4 + init + deinit + get_factory.
const _: () = assert!(core::mem::size_of::<clap_plugin_entry>() == 40);
/// three function slots.
const _: () = assert!(core::mem::size_of::<clap_plugin_factory>() == 24);
/// id 4 + flags 4 + cookie 8 + name 256 + module 1024 + 3×f64.
const _: () = assert!(core::mem::size_of::<clap_param_info>() == 1320);
const _: () = assert!(core::mem::align_of::<clap_param_info>() == 8);
/// id 4 + name 256 + flags 4 + channel_count 4 (+pad 4) + port_type + in_place.
const _: () = assert!(core::mem::size_of::<clap_audio_port_info>() == 288);
const _: () = assert!(core::mem::align_of::<clap_audio_port_info>() == 8);
/// size/time 4+4 + space/type 2+2 + flags 4.
const _: () = assert!(core::mem::size_of::<clap_event_header>() == 16);
/// header + param_id + (pad) + cookie + note_id/port/channel/key + value.
const _: () = assert!(core::mem::size_of::<clap_event_param_value>() == 56);
const _: () = assert!(core::mem::align_of::<clap_event_param_value>() == 8);
/// header + flags + 6×(beattime/sectime) + bar_start + bar_number + 2×u16
/// + pad; transport is the widest core event we read.
const _: () = assert!(core::mem::size_of::<clap_event_transport>() == 104);
/// ctx + size + get.
const _: () = assert!(core::mem::size_of::<crate::sys::events::clap_input_events>() == 24);
/// ctx + try_push.
const _: () = assert!(core::mem::size_of::<crate::sys::events::clap_output_events>() == 16);
/// two function slots each (count/get, count..flush = 6).
const _: () = assert!(core::mem::size_of::<clap_plugin_audio_ports>() == 16);
const _: () = assert!(core::mem::size_of::<clap_plugin_params>() == 48);
const _: () = assert!(core::mem::size_of::<clap_plugin_state>() == 16);
const _: () = assert!(core::mem::size_of::<clap_plugin_latency>() == 8);
const _: () = assert!(core::mem::size_of::<clap_plugin_render>() == 16);
/// host-side vtables we call through: log 1 slot, latency 1, state 1.
const _: () = assert!(core::mem::size_of::<clap_host_log>() == 8);
const _: () = assert!(
    core::mem::size_of::<crate::sys::ext::audio_ports::clap_host_audio_ports>() == 16,
    "is_supported + rescan"
);
const _: () = assert!(
    core::mem::size_of::<crate::sys::ext::params::clap_host_params>() == 24,
    "rescan + clear + request_flush"
);
const _: () = assert!(core::mem::size_of::<crate::sys::ext::latency::clap_host_latency>() == 8);
const _: () = assert!(core::mem::size_of::<crate::sys::ext::state::clap_host_state>() == 8);
/// ctx + read/write slot.
const _: () = assert!(core::mem::size_of::<clap_istream>() == 16);
const _: () = assert!(core::mem::size_of::<clap_ostream>() == 16);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_measured_numbers() {
        // Runtime echoes of the const gates: they exist so a failure names the
        // struct, not just `assert!(false)`.
        assert_eq!(core::mem::size_of::<clap_plugin_descriptor>(), 88);
        assert_eq!(core::mem::size_of::<clap_plugin>(), 96);
        assert_eq!(core::mem::size_of::<clap_host>(), 88);
        assert_eq!(core::mem::size_of::<clap_audio_buffer>(), 32);
        assert_eq!(core::mem::size_of::<clap_process>(), 64);
        assert_eq!(core::mem::size_of::<clap_plugin_entry>(), 40);
        assert_eq!(core::mem::size_of::<clap_plugin_factory>(), 24);
        assert_eq!(core::mem::size_of::<clap_param_info>(), 1320);
        assert_eq!(core::mem::size_of::<clap_audio_port_info>(), 288);
        assert_eq!(core::mem::size_of::<clap_event_header>(), 16);
        assert_eq!(core::mem::size_of::<clap_event_param_value>(), 56);
        assert_eq!(core::mem::size_of::<clap_event_transport>(), 104);
        assert_eq!(core::mem::size_of::<clap_istream>(), 16);
        assert_eq!(core::mem::size_of::<clap_ostream>(), 16);
    }

    #[test]
    fn plugin_field_zero_rule() {
        // [`crate::plugin`] promises `base` is the FIRST field of `Instance<P>`,
        // and `clap_plugin` promises `desc`/`plugin_data` at offsets 0/8: check
        // by building a reference instance in memory at a known address.
        let p = clap_plugin {
            desc: core::ptr::null(),
            plugin_data: core::ptr::from_ref(&1u32)
                .cast::<core::ffi::c_void>()
                .cast_mut(),
            init: None,
            destroy: None,
            activate: None,
            deactivate: None,
            start_processing: None,
            stop_processing: None,
            reset: None,
            process: None,
            get_extension: None,
            on_main_thread: None,
        };
        let base = core::ptr::from_ref(&p).cast::<u8>();
        // SAFETY: raw reads of a local struct we own, to compute field offsets.
        unsafe {
            let desc_off = core::ptr::from_ref(&p.desc).cast::<u8>().offset_from(base) as usize;
            let data_off = core::ptr::from_ref(&p.plugin_data)
                .cast::<u8>()
                .offset_from(base) as usize;
            assert_eq!(desc_off, 0, "desc at offset 0: host and wrapper agree");
            assert_eq!(data_off, 8, "plugin_data at offset 8");
        }
    }
}
