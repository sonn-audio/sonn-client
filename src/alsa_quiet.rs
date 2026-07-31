//! Stop libasound from writing to stderr.
//!
//! Enumerating sound cards walks every PCM in ALSA's configuration, including the ones that cannot
//! work on this machine: `dmix` on a capture device, `dsnoop` on a playback device, an `asym` with no
//! capture slave. libasound prints each refusal to stderr itself, so a single enumeration produces
//! sixteen lines of `ALSA lib pcm_dmix.c:973 ...` that describe ALSA's own configuration file rather
//! than anything about this device -- and under systemd every one of them lands in the journal, where
//! they bury the lines that matter.
//!
//! The complaints are not errors we can act on: the devices they refer to are skipped either way, and
//! `devices --log-level debug` now reports the skip in our own words. So the handler is replaced with
//! one that does nothing.

/// Silence libasound's own error output.
///
/// Safe to call more than once, and a no-op where the library is absent.
pub fn install() {
    #[cfg(target_os = "linux")]
    unsafe {
        // `snd_lib_error_set_handler` takes a C variadic function pointer, which stable Rust cannot
        // define. Transmuting a non-variadic one is the accepted way round it and is sound here for
        // the reason that makes it useful: the handler reads none of its arguments, so no calling
        // convention can be got wrong.
        let handler: alsa_sys::snd_lib_error_handler_t = Some(std::mem::transmute::<
            *const (),
            SilentHandler,
        >(silent as *const ()));
        alsa_sys::snd_lib_error_set_handler(handler);
    }
}

#[cfg(target_os = "linux")]
type SilentHandler = unsafe extern "C" fn(
    *const std::os::raw::c_char,
    std::os::raw::c_int,
    *const std::os::raw::c_char,
    std::os::raw::c_int,
    *const std::os::raw::c_char,
    ...
);

#[cfg(target_os = "linux")]
unsafe extern "C" fn silent(
    _file: *const std::os::raw::c_char,
    _line: std::os::raw::c_int,
    _function: *const std::os::raw::c_char,
    _err: std::os::raw::c_int,
    _fmt: *const std::os::raw::c_char,
) {
}
