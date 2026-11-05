//! C-string helpers: the `clap_cstr!` literal macro and the fixed-array writers.
//!
//! ## What lives here and why
//!
//! CLAP speaks C: descriptors, extension ids and parameter names are NUL-terminated
//! strings, and the fixed `char[256]`/`char[1024]` arrays inside `clap_param_info`
//! and `clap_audio_port_info` must always stay NUL-terminated even when a name does
//! not fit. `clap-sys`'s own `cstr!` is `pub(crate)` (verified by reading the
//! vendored 0.5.0 source), so this crate carries its own.
//!
//! Everything here is allocation-free by construction: `clap_cstr!` produces a
//! `&'static CStr` in a `const` context (the string lives in the binary), and the
//! writers copy into caller-provided fixed arrays.

/// A `const`-usable NUL-terminated `&'static CStr` literal.
///
/// Equivalent to clap-sys's crate-private `cstr!`, written so it can appear in
/// `static` initializers (the descriptor fields demand that).
///
/// ```
/// use hathor_clap::clap_cstr;
///
/// const ID: &std::ffi::CStr = clap_cstr!("com.hathor.example");
/// assert_eq!(ID.to_str().unwrap(), "com.hathor.example");
/// ```
#[macro_export]
macro_rules! clap_cstr {
    ($str:literal) => {
        // SAFETY: `concat!($str, "\0")` ends in exactly one NUL byte and the literal
        // cannot contain interior NULs (a `c"…"`-style violation in a normal literal
        // is a compile error), so `from_bytes_with_nul_unchecked`'s precondition
        // holds by construction, checked at compile time by the literal's type.
        unsafe {
            $crate::__reexport::CStr::from_bytes_with_nul_unchecked(concat!($str, "\0").as_bytes())
        }
    };
}

/// Copies `src` into a CLAP fixed `char[N]` field, always leaving a terminating NUL
/// inside the array; over-long names are truncated (CLAP's `clap.h` documents the
/// same truncating behavior: the field is a `char` array, not a pointer).
pub(crate) fn write_padded_name<const N: usize>(dst: &mut [std::ffi::c_char; N], src: &str) {
    let bytes = src.as_bytes();
    let keep = bytes.len().min(N - 1);
    for (d, &b) in dst[..keep].iter_mut().zip(bytes) {
        *d = b as std::ffi::c_char;
    }
    dst[keep] = 0; // terminates even when truncated
}

/// Reborrows a CLAP `char[N]` name field as a `&CStr`, stopping at the first NUL.
///
/// Tolerates a field with no NUL at all (a misbehaving *writer*, never ours): the
/// scan is bounded by `N`, so malformed input degrades to the whole array, not to
/// UB. This is the same shape `clap.h` prescribes for reading these fields.
pub fn read_padded_name<const N: usize>(src: &[std::ffi::c_char; N]) -> &str {
    let bytes: &[u8] = unsafe {
        // SAFETY: `c_char` on every supported target is i8/u8-sized with align 1, and
        // the slice length is `N` both before and after; reinterpreting a fixed array
        // of bytes as bytes keeps every validity and alignment invariant.
        std::slice::from_raw_parts(src.as_ptr().cast::<u8>(), N)
    };
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(N);
    // The bytes came from a Rust `&str` on our side of the API; a foreign writer that
    // leaves invalid UTF-8 yields `""` rather than a panic — names are display data.
    std::str::from_utf8(&bytes[..end]).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padded_name_writes_terminate_within_the_array() {
        let mut dst = [0x7a; 8]; // 0x7a is not NUL: pre-polluted, a leak would show
        write_padded_name(&mut dst, "abcdef");
        assert_eq!(read_padded_name(&dst), "abcdef", "fits: full copy");

        write_padded_name(&mut dst, "abcdefghij");
        assert_eq!(
            read_padded_name(&dst),
            "abcdefg",
            "truncated at N-1 with the NUL always inside the array"
        );
        assert_eq!(
            dst[7], 0,
            "the terminator is the last byte when the name is too long"
        );
    }

    #[test]
    fn padded_name_empty_string_is_just_the_terminator() {
        let mut dst = [7; 4];
        write_padded_name(&mut dst, "");
        assert_eq!(dst[0], 0);
        assert_eq!(read_padded_name(&dst), "");
    }

    #[test]
    fn cstr_macro_produces_a_static_cstr_with_nul() {
        const S: &std::ffi::CStr = clap_cstr!("clap.params");
        assert_eq!(S.to_str().unwrap(), "clap.params");
        assert_eq!(S.to_bytes_with_nul().last(), Some(&0u8), "ends in NUL");
        // The macro result is const-evaluable: usable in a `static` initializer, which
        // is exactly what the descriptor assembly needs.
        static STATIC_OK: usize = S.count_bytes();
        assert_eq!(STATIC_OK, 11);
    }
}
