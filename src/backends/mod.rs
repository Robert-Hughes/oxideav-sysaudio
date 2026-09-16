//! Per-target-os backend registration. The `DRIVERS` slice is ordered
//! by the preference documented in the crate README: on Linux,
//! `pipewire → pulse → alsa → oss`; on Windows, `wasapi → asio`; on
//! macOS, `coreaudio`; on Android, `aaudio`.

use crate::backend::Backend;

#[cfg(all(target_os = "linux", feature = "alsa"))]
pub(crate) mod alsa;
#[cfg(all(any(target_os = "linux", target_os = "freebsd"), feature = "oss"))]
pub(crate) mod oss;
#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub(crate) mod pipewire;
#[cfg(all(target_os = "linux", feature = "pulse"))]
pub(crate) mod pulse;

#[cfg(all(target_os = "windows", feature = "asio"))]
pub(crate) mod asio;
#[cfg(all(target_os = "windows", feature = "wasapi"))]
pub(crate) mod wasapi;

#[cfg(all(target_os = "macos", feature = "coreaudio"))]
pub(crate) mod coreaudio;

#[cfg(all(target_os = "android", feature = "aaudio"))]
pub(crate) mod aaudio;

// The virtual mock backend is target-independent and always last in
// the preference order, so it never shadows a working real backend.
#[cfg(feature = "mock")]
pub(crate) mod mock;

pub(crate) fn drivers() -> &'static [&'static dyn Backend] {
    #[cfg(target_os = "linux")]
    {
        &[
            #[cfg(feature = "pipewire")]
            &pipewire::PipeWireBackend,
            #[cfg(feature = "pulse")]
            &pulse::PulseBackend,
            #[cfg(feature = "alsa")]
            &alsa::AlsaBackend,
            #[cfg(feature = "oss")]
            &oss::OssBackend,
            #[cfg(feature = "mock")]
            &mock::MockBackend,
        ]
    }
    #[cfg(target_os = "freebsd")]
    {
        &[
            #[cfg(feature = "oss")]
            &oss::OssBackend,
            #[cfg(feature = "mock")]
            &mock::MockBackend,
        ]
    }
    #[cfg(target_os = "windows")]
    {
        &[
            #[cfg(feature = "wasapi")]
            &wasapi::WasapiBackend,
            #[cfg(feature = "asio")]
            &asio::AsioBackend,
            #[cfg(feature = "mock")]
            &mock::MockBackend,
        ]
    }
    #[cfg(target_os = "macos")]
    {
        &[
            #[cfg(feature = "coreaudio")]
            &coreaudio::CoreAudioBackend,
            #[cfg(feature = "mock")]
            &mock::MockBackend,
        ]
    }
    #[cfg(target_os = "android")]
    {
        &[
            #[cfg(feature = "aaudio")]
            &aaudio::AAudioBackend,
            #[cfg(feature = "mock")]
            &mock::MockBackend,
        ]
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "freebsd",
        target_os = "windows",
        target_os = "macos",
        target_os = "android"
    )))]
    {
        &[
            #[cfg(feature = "mock")]
            &mock::MockBackend,
        ]
    }
}
