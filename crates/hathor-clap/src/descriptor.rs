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

/// A static, NULL-terminated CLAP feature list: the rows that
/// `clap_plugin_descriptor::features` publishes. The pointers it carries are
/// `as_ptr` of `'static` `CStr`s (the `clap_cstr!` macro or clap-sys's
/// `plugin_features` constants) — the same soundness argument clap-sys itself
/// uses to mark the descriptor `Sync`, re-stated for the list. Built inline
/// in a `static` initializer (const promotion), never as a `static` of raw
/// pointers (those are not `Sync`).
pub struct FeatureList(&'static [*const c_char]);

impl FeatureList {
    /// Wraps a feature array; the `clap.h` rule — the list ends with a null
    /// row — is verified at compile time here, so a malformed list cannot
    /// even finish assembling the descriptor.
    pub const fn new(list: &'static [*const c_char]) -> Self {
        assert!(
            !list.is_empty(),
            "a feature list holds at least the terminator"
        );
        assert!(
            list[list.len() - 1].is_null(),
            "clap.h: the feature list is NULL-terminated"
        );
        Self(list)
    }

    /// The pointer to publish into the descriptor.
    pub const fn ptr(&self) -> *const *const c_char {
        self.0.as_ptr()
    }
}

/// Assembles a [`clap_plugin_descriptor`] from NUL-terminated static strings.
///
/// `features` is a NULL-terminated array of feature `char*` (the CLAP
/// `plugin_features` constants, see `crate::sys::plugin_features`); pass
/// `None` until a plugin has real features to declare. A null list is read
/// here as "no features", which matches the `clap.h` rule that the list is
/// NULL-terminated (NOT VERIFIED against hosts: a host treating null as
/// malformed would be a plugin-loading bug we have not observed).
// The 9 arguments ARE the ABI: `clap_plugin_descriptor` has exactly these
// fields — the spec's strings one by one, plus the feature-list pointer.
// A builder or options struct would only add ceremony around what the spec
// names individually.
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
    features: Option<FeatureList>,
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
        features: match features {
            // The published pointer's validity: every row is an `as_ptr` of a
            // `'static` CStr (FeatureList's own contract), and the NULL
            // terminator was const-verified at its construction.
            Some(list) => list.ptr(),
            None => ptr::null::<*const c_char>(),
        },
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
        None,
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
            None,
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

    // Assembled in a `static` (like every real descriptor): the list's slice
    // promotion only exists in const-initializer position.
    static DESC_FEAT: clap_plugin_descriptor = make_descriptor(
        clap_cstr!("com.hathor.test"),
        clap_cstr!("Hathor Test"),
        clap_cstr!("Hathor"),
        clap_cstr!("https://hathor.local"),
        clap_cstr!(""),
        clap_cstr!(""),
        clap_cstr!("0.1.0"),
        clap_cstr!("feature-carrying"),
        Some(FeatureList::new(&[
            clap_cstr!("audio-effect").as_ptr(),
            clap_cstr!("analyzer").as_ptr(),
            ptr::null(),
        ])),
    );

    #[test]
    fn feature_list_survives_assembly_and_terminates() {
        let d = &DESC_FEAT;
        assert!(
            !d.features.is_null(),
            "a declared list is published, not nulled"
        );
        // SAFETY: `FLIST` is a static, NULL-terminated by construction.
        let (first, second, end) = unsafe {
            (
                CStr::from_ptr(*d.features),
                CStr::from_ptr(*d.features.add(1)),
                *d.features.add(2),
            )
        };
        assert_eq!(first, clap_cstr!("audio-effect"));
        assert_eq!(second, clap_cstr!("analyzer"));
        assert!(
            end.is_null(),
            "the list terminates per clap.h — a host walks it"
        );
    }
}
