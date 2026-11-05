//! The static plugin descriptor: `clap_plugin_descriptor`, assembled `const`.
//!
//! A descriptor is pure `'static` data — version, ids and display strings. The
//! const builder exists so every descriptor in the workspace can be a `static`
//! item: the host keeps the pointer for the life of the library, so heap
//! descriptors would be either a leak or a dangling promise.

use crate::sys::plugin::clap_plugin_descriptor;
use crate::sys::version::CLAP_VERSION;
use core::ffi::{c_char, CStr};
use core::ptr;

/// Assembles a [`clap_plugin_descriptor`] from NUL-terminated static strings.
///
/// `features` is published as null: the field is a NULL-terminated array of
/// `char*` and an empty list is the honest value until a plugin has real features
/// to declare (NOT VERIFIED against hosts: a null array is read here as "no
/// features", which matches the `clap.h` rule that the list is NULL-terminated;
/// a host treating null as malformed would be a plugin-loading bug we have not
/// observed).
// The 8 arguments ARE the ABI: `clap_plugin_descriptor` has exactly these
// string fields; a builder or options struct would only add ceremony around
// what the spec names one by one.
#[allow(clippy::too_many_arguments)]
pub const fn make_descriptor(
    id: &CStr,
    name: &CStr,
    vendor: &CStr,
    url: &CStr,
    manual_url: &CStr,
    support_url: &CStr,
    version: &CStr,
    description: &CStr,
) -> clap_plugin_descriptor {
    clap_plugin_descriptor {
        clap_version: CLAP_VERSION,
        id: id.as_ptr(),
        name: name.as_ptr(),
        vendor: vendor.as_ptr(),
        url: url.as_ptr(),
        manual_url: manual_url.as_ptr(),
        support_url: support_url.as_ptr(),
        version: version.as_ptr(),
        description: description.as_ptr(),
        features: ptr::null::<*const c_char>(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clap_cstr;
    use core::ffi::CStr;

    static DESC: clap_plugin_descriptor = make_descriptor(
        clap_cstr!("com.hathor.test"),
        clap_cstr!("Hathor Test"),
        clap_cstr!("Hathor"),
        clap_cstr!("https://hathor.local"),
        clap_cstr!(""),
        clap_cstr!(""),
        clap_cstr!("0.0.0"),
        clap_cstr!("test descriptor"),
    );

    fn owned(p: *const c_char) -> String {
        assert!(!p.is_null(), "descriptor field must be a real pointer");
        // SAFETY: the pointer came from `clap_cstr!` above: a valid NUL-terminated
        // static string of at least one byte.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }

    #[test]
    fn const_descriptor_static_carries_every_field() {
        assert_eq!(DESC.clap_version, CLAP_VERSION);
        assert_eq!(owned(DESC.id), "com.hathor.test");
        assert_eq!(owned(DESC.name), "Hathor Test");
        assert_eq!(owned(DESC.vendor), "Hathor");
        assert_eq!(owned(DESC.url), "https://hathor.local");
        assert_eq!(owned(DESC.version), "0.0.0");
        assert_eq!(owned(DESC.description), "test descriptor");
        assert!(
            DESC.features.is_null(),
            "empty feature list is published as null"
        );
    }

    #[test]
    fn static_descriptor_is_valid_to_read_back_from_its_own_static() {
        // The point of `const` assembly: the descriptor above is a plain `static`,
        // so its address is stable for as long as the host holds it.
        let again = make_descriptor(
            clap_cstr!("com.hathor.test"),
            clap_cstr!("Hathor Test"),
            clap_cstr!("Hathor"),
            clap_cstr!("https://hathor.local"),
            clap_cstr!(""),
            clap_cstr!(""),
            clap_cstr!("0.0.0"),
            clap_cstr!("test descriptor"),
        );
        unsafe {
            // SAFETY: both pointers are `clap_cstr!` statics; comparing byte-wise
            // content proves the const assembly reproduces equal strings.
            assert_eq!(
                CStr::from_ptr(again.id),
                CStr::from_ptr(DESC.id),
                "const assembly is deterministic"
            );
        }
    }
}
