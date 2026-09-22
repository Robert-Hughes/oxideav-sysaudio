//! Android AAudio output backend.
//!
//! AAudio is available from Android API 26.  We deliberately load
//! `libaaudio.so` at runtime rather than link against it, preserving the
//! crate's no-native-link-time-dependency model and allowing binaries with a
//! lower min-SDK to fail cleanly at runtime on pre-26 devices.
//!
//! Audio is rendered through AAudio's real-time data callback as interleaved
//! f32 PCM.  The callback owns the user `FnMut`; the stream handle only
//! touches atomics while the callback may be running.  `AAudioStream_close`
//! is called before that callback state is freed.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use std::ffi::{c_char, c_void, CStr};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use libloading::{Library, Symbol};

use crate::backend::{Backend, Callback};
use crate::format::{
    CallbackInfo, ContentType, SampleFormat, StreamFormat, StreamRequest, StreamUsage,
};
use crate::stream::StreamImpl;
use crate::{Error, Result};

const AAUDIO_OK: i32 = 0;
const AAUDIO_DIRECTION_OUTPUT: i32 = 0;
const AAUDIO_FORMAT_PCM_FLOAT: i32 = 2;
const AAUDIO_PERFORMANCE_MODE_LOW_LATENCY: i32 = 12;
const AAUDIO_USAGE_MEDIA: i32 = 1;
const AAUDIO_CONTENT_TYPE_SPEECH: i32 = 1;
const AAUDIO_CONTENT_TYPE_MUSIC: i32 = 2;
const AAUDIO_CONTENT_TYPE_MOVIE: i32 = 3;
const AAUDIO_CONTENT_TYPE_SONIFICATION: i32 = 4;
const AAUDIO_CALLBACK_RESULT_CONTINUE: i32 = 0;
const AAUDIO_CALLBACK_RESULT_STOP: i32 = 1;

#[repr(C)]
struct AAudioStreamBuilder {
    _private: [u8; 0],
}

#[repr(C)]
struct AAudioStream {
    _private: [u8; 0],
}

type AAudioStream_dataCallback =
    unsafe extern "C" fn(*mut AAudioStream, *mut c_void, *mut c_void, i32) -> i32;
type AAudioStream_errorCallback = unsafe extern "C" fn(*mut AAudioStream, *mut c_void, i32);

type Fn_AAudio_createStreamBuilder = unsafe extern "C" fn(*mut *mut AAudioStreamBuilder) -> i32;
type Fn_AAudioStreamBuilder_delete = unsafe extern "C" fn(*mut AAudioStreamBuilder) -> i32;
type Fn_AAudioStreamBuilder_setDeviceId = unsafe extern "C" fn(*mut AAudioStreamBuilder, i32);
type Fn_AAudioStreamBuilder_setSampleRate = unsafe extern "C" fn(*mut AAudioStreamBuilder, i32);
type Fn_AAudioStreamBuilder_setChannelCount = unsafe extern "C" fn(*mut AAudioStreamBuilder, i32);
type Fn_AAudioStreamBuilder_setFormat = unsafe extern "C" fn(*mut AAudioStreamBuilder, i32);
type Fn_AAudioStreamBuilder_setDirection = unsafe extern "C" fn(*mut AAudioStreamBuilder, i32);
type Fn_AAudioStreamBuilder_setBufferCapacityInFrames =
    unsafe extern "C" fn(*mut AAudioStreamBuilder, i32);
type Fn_AAudioStreamBuilder_setFramesPerDataCallback =
    unsafe extern "C" fn(*mut AAudioStreamBuilder, i32);
type Fn_AAudioStreamBuilder_setPerformanceMode =
    unsafe extern "C" fn(*mut AAudioStreamBuilder, i32);
type Fn_AAudioStreamBuilder_setUsage = unsafe extern "C" fn(*mut AAudioStreamBuilder, i32);
type Fn_AAudioStreamBuilder_setContentType = unsafe extern "C" fn(*mut AAudioStreamBuilder, i32);
type Fn_AAudioStreamBuilder_setDataCallback =
    unsafe extern "C" fn(*mut AAudioStreamBuilder, Option<AAudioStream_dataCallback>, *mut c_void);
type Fn_AAudioStreamBuilder_setErrorCallback =
    unsafe extern "C" fn(*mut AAudioStreamBuilder, Option<AAudioStream_errorCallback>, *mut c_void);
type Fn_AAudioStreamBuilder_openStream =
    unsafe extern "C" fn(*mut AAudioStreamBuilder, *mut *mut AAudioStream) -> i32;

type Fn_AAudioStream_close = unsafe extern "C" fn(*mut AAudioStream) -> i32;
type Fn_AAudioStream_requestStart = unsafe extern "C" fn(*mut AAudioStream) -> i32;
type Fn_AAudioStream_requestPause = unsafe extern "C" fn(*mut AAudioStream) -> i32;
type Fn_AAudioStream_requestStop = unsafe extern "C" fn(*mut AAudioStream) -> i32;
type Fn_AAudioStream_getSampleRate = unsafe extern "C" fn(*mut AAudioStream) -> i32;
type Fn_AAudioStream_getChannelCount = unsafe extern "C" fn(*mut AAudioStream) -> i32;
type Fn_AAudioStream_getFormat = unsafe extern "C" fn(*mut AAudioStream) -> i32;
type Fn_AAudioStream_getBufferCapacityInFrames = unsafe extern "C" fn(*mut AAudioStream) -> i32;
type Fn_AAudioStream_getDeviceId = unsafe extern "C" fn(*mut AAudioStream) -> i32;
type Fn_AAudioStream_getFramesWritten = unsafe extern "C" fn(*mut AAudioStream) -> i64;
type Fn_AAudioStream_getFramesRead = unsafe extern "C" fn(*mut AAudioStream) -> i64;
type Fn_AAudio_convertResultToText = unsafe extern "C" fn(i32) -> *const c_char;

struct AAudioLib {
    _lib: Library,
    AAudio_createStreamBuilder: Fn_AAudio_createStreamBuilder,
    AAudioStreamBuilder_delete: Fn_AAudioStreamBuilder_delete,
    AAudioStreamBuilder_setDeviceId: Fn_AAudioStreamBuilder_setDeviceId,
    AAudioStreamBuilder_setSampleRate: Fn_AAudioStreamBuilder_setSampleRate,
    AAudioStreamBuilder_setChannelCount: Fn_AAudioStreamBuilder_setChannelCount,
    AAudioStreamBuilder_setFormat: Fn_AAudioStreamBuilder_setFormat,
    AAudioStreamBuilder_setDirection: Fn_AAudioStreamBuilder_setDirection,
    AAudioStreamBuilder_setBufferCapacityInFrames: Fn_AAudioStreamBuilder_setBufferCapacityInFrames,
    AAudioStreamBuilder_setFramesPerDataCallback: Fn_AAudioStreamBuilder_setFramesPerDataCallback,
    AAudioStreamBuilder_setPerformanceMode: Fn_AAudioStreamBuilder_setPerformanceMode,
    AAudioStreamBuilder_setUsage: Option<Fn_AAudioStreamBuilder_setUsage>,
    AAudioStreamBuilder_setContentType: Option<Fn_AAudioStreamBuilder_setContentType>,
    AAudioStreamBuilder_setDataCallback: Fn_AAudioStreamBuilder_setDataCallback,
    AAudioStreamBuilder_setErrorCallback: Fn_AAudioStreamBuilder_setErrorCallback,
    AAudioStreamBuilder_openStream: Fn_AAudioStreamBuilder_openStream,
    AAudioStream_close: Fn_AAudioStream_close,
    AAudioStream_requestStart: Fn_AAudioStream_requestStart,
    AAudioStream_requestPause: Fn_AAudioStream_requestPause,
    AAudioStream_requestStop: Fn_AAudioStream_requestStop,
    AAudioStream_getSampleRate: Fn_AAudioStream_getSampleRate,
    AAudioStream_getChannelCount: Fn_AAudioStream_getChannelCount,
    AAudioStream_getFormat: Fn_AAudioStream_getFormat,
    AAudioStream_getBufferCapacityInFrames: Fn_AAudioStream_getBufferCapacityInFrames,
    AAudioStream_getDeviceId: Fn_AAudioStream_getDeviceId,
    AAudioStream_getFramesWritten: Fn_AAudioStream_getFramesWritten,
    AAudioStream_getFramesRead: Fn_AAudioStream_getFramesRead,
    AAudio_convertResultToText: Fn_AAudio_convertResultToText,
}

unsafe impl Send for AAudioLib {}
unsafe impl Sync for AAudioLib {}

impl AAudioLib {
    fn load() -> Result<Arc<Self>> {
        let lib = unsafe { Library::new("libaaudio.so") }.map_err(|source| Error::LibraryLoad {
            backend: "aaudio",
            soname: "libaaudio.so",
            source,
        })?;

        unsafe {
            macro_rules! opt_sym {
                ($name:ident, $ty:ty) => {{
                    lib.get::<$ty>(concat!(stringify!($name), "\0").as_bytes())
                        .ok()
                        .map(|symbol| *symbol)
                }};
            }

            macro_rules! sym {
                ($name:ident, $ty:ty) => {{
                    let symbol: Symbol<$ty> = lib
                        .get(concat!(stringify!($name), "\0").as_bytes())
                        .map_err(|source| Error::SymbolMissing {
                            backend: "aaudio",
                            symbol: stringify!($name),
                            source,
                        })?;
                    *symbol
                }};
            }

            Ok(Arc::new(Self {
                AAudio_createStreamBuilder: sym!(
                    AAudio_createStreamBuilder,
                    Fn_AAudio_createStreamBuilder
                ),
                AAudioStreamBuilder_delete: sym!(
                    AAudioStreamBuilder_delete,
                    Fn_AAudioStreamBuilder_delete
                ),
                AAudioStreamBuilder_setDeviceId: sym!(
                    AAudioStreamBuilder_setDeviceId,
                    Fn_AAudioStreamBuilder_setDeviceId
                ),
                AAudioStreamBuilder_setSampleRate: sym!(
                    AAudioStreamBuilder_setSampleRate,
                    Fn_AAudioStreamBuilder_setSampleRate
                ),
                AAudioStreamBuilder_setChannelCount: sym!(
                    AAudioStreamBuilder_setChannelCount,
                    Fn_AAudioStreamBuilder_setChannelCount
                ),
                AAudioStreamBuilder_setFormat: sym!(
                    AAudioStreamBuilder_setFormat,
                    Fn_AAudioStreamBuilder_setFormat
                ),
                AAudioStreamBuilder_setDirection: sym!(
                    AAudioStreamBuilder_setDirection,
                    Fn_AAudioStreamBuilder_setDirection
                ),
                AAudioStreamBuilder_setBufferCapacityInFrames: sym!(
                    AAudioStreamBuilder_setBufferCapacityInFrames,
                    Fn_AAudioStreamBuilder_setBufferCapacityInFrames
                ),
                AAudioStreamBuilder_setFramesPerDataCallback: sym!(
                    AAudioStreamBuilder_setFramesPerDataCallback,
                    Fn_AAudioStreamBuilder_setFramesPerDataCallback
                ),
                AAudioStreamBuilder_setPerformanceMode: sym!(
                    AAudioStreamBuilder_setPerformanceMode,
                    Fn_AAudioStreamBuilder_setPerformanceMode
                ),
                AAudioStreamBuilder_setUsage: opt_sym!(
                    AAudioStreamBuilder_setUsage,
                    Fn_AAudioStreamBuilder_setUsage
                ),
                AAudioStreamBuilder_setContentType: opt_sym!(
                    AAudioStreamBuilder_setContentType,
                    Fn_AAudioStreamBuilder_setContentType
                ),
                AAudioStreamBuilder_setDataCallback: sym!(
                    AAudioStreamBuilder_setDataCallback,
                    Fn_AAudioStreamBuilder_setDataCallback
                ),
                AAudioStreamBuilder_setErrorCallback: sym!(
                    AAudioStreamBuilder_setErrorCallback,
                    Fn_AAudioStreamBuilder_setErrorCallback
                ),
                AAudioStreamBuilder_openStream: sym!(
                    AAudioStreamBuilder_openStream,
                    Fn_AAudioStreamBuilder_openStream
                ),
                AAudioStream_close: sym!(AAudioStream_close, Fn_AAudioStream_close),
                AAudioStream_requestStart: sym!(
                    AAudioStream_requestStart,
                    Fn_AAudioStream_requestStart
                ),
                AAudioStream_requestPause: sym!(
                    AAudioStream_requestPause,
                    Fn_AAudioStream_requestPause
                ),
                AAudioStream_requestStop: sym!(
                    AAudioStream_requestStop,
                    Fn_AAudioStream_requestStop
                ),
                AAudioStream_getSampleRate: sym!(
                    AAudioStream_getSampleRate,
                    Fn_AAudioStream_getSampleRate
                ),
                AAudioStream_getChannelCount: sym!(
                    AAudioStream_getChannelCount,
                    Fn_AAudioStream_getChannelCount
                ),
                AAudioStream_getFormat: sym!(AAudioStream_getFormat, Fn_AAudioStream_getFormat),
                AAudioStream_getBufferCapacityInFrames: sym!(
                    AAudioStream_getBufferCapacityInFrames,
                    Fn_AAudioStream_getBufferCapacityInFrames
                ),
                AAudioStream_getDeviceId: sym!(
                    AAudioStream_getDeviceId,
                    Fn_AAudioStream_getDeviceId
                ),
                AAudioStream_getFramesWritten: sym!(
                    AAudioStream_getFramesWritten,
                    Fn_AAudioStream_getFramesWritten
                ),
                AAudioStream_getFramesRead: sym!(
                    AAudioStream_getFramesRead,
                    Fn_AAudioStream_getFramesRead
                ),
                AAudio_convertResultToText: sym!(
                    AAudio_convertResultToText,
                    Fn_AAudio_convertResultToText
                ),
                _lib: lib,
            }))
        }
    }

    fn result_text(&self, result: i32) -> String {
        let ptr = unsafe { (self.AAudio_convertResultToText)(result) };
        if ptr.is_null() {
            return format!("AAudio result {result}");
        }
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

fn lib() -> Result<Arc<AAudioLib>> {
    static CACHED: OnceLock<Mutex<Option<Arc<AAudioLib>>>> = OnceLock::new();
    let slot = CACHED.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().unwrap();
    if let Some(lib) = guard.as_ref() {
        return Ok(lib.clone());
    }
    let loaded = AAudioLib::load()?;
    *guard = Some(loaded.clone());
    Ok(loaded)
}

struct Builder {
    lib: Arc<AAudioLib>,
    ptr: *mut AAudioStreamBuilder,
}

impl Builder {
    fn new(lib: Arc<AAudioLib>) -> Result<Self> {
        let mut ptr = ptr::null_mut();
        let result = unsafe { (lib.AAudio_createStreamBuilder)(&mut ptr) };
        if result != AAUDIO_OK || ptr.is_null() {
            return Err(Error::DeviceOpen {
                backend: "aaudio",
                detail: format!(
                    "AAudio_createStreamBuilder failed: {} ({result})",
                    lib.result_text(result)
                ),
            });
        }
        Ok(Self { lib, ptr })
    }
}

impl Drop for Builder {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                (self.lib.AAudioStreamBuilder_delete)(self.ptr);
            }
            self.ptr = ptr::null_mut();
        }
    }
}

struct RawStream {
    lib: Arc<AAudioLib>,
    ptr: *mut AAudioStream,
}

impl RawStream {
    fn close(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                (self.lib.AAudioStream_close)(self.ptr);
            }
            self.ptr = ptr::null_mut();
        }
    }

    fn into_raw(mut self) -> *mut AAudioStream {
        let ptr = self.ptr;
        self.ptr = ptr::null_mut();
        ptr
    }
}

impl Drop for RawStream {
    fn drop(&mut self) {
        self.close();
    }
}

fn aaudio_usage(usage: StreamUsage) -> i32 {
    match usage {
        StreamUsage::Media => AAUDIO_USAGE_MEDIA,
    }
}

fn aaudio_content_type(content_type: ContentType) -> i32 {
    match content_type {
        ContentType::Music => AAUDIO_CONTENT_TYPE_MUSIC,
        ContentType::Movie => AAUDIO_CONTENT_TYPE_MOVIE,
        ContentType::Speech => AAUDIO_CONTENT_TYPE_SPEECH,
        ContentType::Sonification => AAUDIO_CONTENT_TYPE_SONIFICATION,
    }
}
fn parse_device_id(device: Option<&str>) -> Result<Option<i32>> {
    device
        .map(|value| {
            value
                .parse::<i32>()
                .map_err(|_| Error::DeviceOpen {
                    backend: "aaudio",
                    detail: format!(
                        "device id {value:?} is not an Android AudioDeviceInfo numeric id"
                    ),
                })
                .and_then(|id| {
                    if id <= 0 {
                        Err(Error::DeviceOpen {
                            backend: "aaudio",
                            detail: format!("device id {id} must be a positive AudioDeviceInfo id"),
                        })
                    } else {
                        Ok(id)
                    }
                })
        })
        .transpose()
}

fn verify_device_id(
    lib: &AAudioLib,
    stream: *mut AAudioStream,
    requested: Option<i32>,
) -> Result<()> {
    let Some(expected) = requested else {
        return Ok(());
    };
    let actual = unsafe { (lib.AAudioStream_getDeviceId)(stream) };
    if actual == expected {
        Ok(())
    } else {
        Err(Error::DeviceOpen {
            backend: "aaudio",
            detail: format!(
                "requested Android audio device id {expected}, but AAudio opened device id {actual}"
            ),
        })
    }
}

fn open_query_stream(lib: Arc<AAudioLib>, device: Option<&str>) -> Result<RawStream> {
    let requested_device = parse_device_id(device)?;
    let builder = Builder::new(lib.clone())?;
    unsafe {
        (lib.AAudioStreamBuilder_setDirection)(builder.ptr, AAUDIO_DIRECTION_OUTPUT);
        (lib.AAudioStreamBuilder_setFormat)(builder.ptr, AAUDIO_FORMAT_PCM_FLOAT);
        if let Some(id) = requested_device {
            (lib.AAudioStreamBuilder_setDeviceId)(builder.ptr, id);
        }
    }

    let mut stream = ptr::null_mut();
    let result = unsafe { (lib.AAudioStreamBuilder_openStream)(builder.ptr, &mut stream) };
    if result != AAUDIO_OK || stream.is_null() {
        return Err(Error::DeviceOpen {
            backend: "aaudio",
            detail: format!(
                "AAudioStreamBuilder_openStream failed: {} ({result})",
                lib.result_text(result)
            ),
        });
    }

    let raw = RawStream { lib, ptr: stream };
    verify_device_id(&raw.lib, raw.ptr, requested_device)?;
    Ok(raw)
}

fn queried_format(lib: &AAudioLib, stream: *mut AAudioStream) -> Result<StreamFormat> {
    let rate = unsafe { (lib.AAudioStream_getSampleRate)(stream) };
    let channels = unsafe { (lib.AAudioStream_getChannelCount)(stream) };
    let format = unsafe { (lib.AAudioStream_getFormat)(stream) };

    if rate <= 0 || channels <= 0 || channels > i32::from(u16::MAX) {
        return Err(Error::UnsupportedFormat {
            backend: "aaudio",
            detail: format!(
                "AAudio returned invalid stream shape: sample_rate={rate} channels={channels}"
            ),
        });
    }
    if format != AAUDIO_FORMAT_PCM_FLOAT {
        return Err(Error::UnsupportedFormat {
            backend: "aaudio",
            detail: format!(
                "AAudio returned format {format}; sysaudio requires interleaved PCM_FLOAT ({AAUDIO_FORMAT_PCM_FLOAT})"
            ),
        });
    }

    Ok(StreamFormat {
        sample_rate: rate as u32,
        channels: channels as u16,
        format: SampleFormat::F32,
    })
}

struct CallbackState {
    callback: Callback,
    channels: usize,
    frames_played: AtomicU64,
    stopped: AtomicBool,
    async_error: AtomicI32,
}

unsafe extern "C" fn data_callback(
    _stream: *mut AAudioStream,
    user_data: *mut c_void,
    audio_data: *mut c_void,
    num_frames: i32,
) -> i32 {
    if user_data.is_null() || audio_data.is_null() || num_frames < 0 {
        return AAUDIO_CALLBACK_RESULT_STOP;
    }
    if num_frames == 0 {
        return AAUDIO_CALLBACK_RESULT_CONTINUE;
    }

    let state = unsafe { &mut *(user_data as *mut CallbackState) };
    if state.stopped.load(Ordering::Acquire) {
        return AAUDIO_CALLBACK_RESULT_STOP;
    }

    let frames = num_frames as usize;
    let Some(samples) = frames.checked_mul(state.channels) else {
        state.stopped.store(true, Ordering::Release);
        return AAUDIO_CALLBACK_RESULT_STOP;
    };
    let out = unsafe { slice::from_raw_parts_mut(audio_data as *mut f32, samples) };
    out.fill(0.0);

    let info = CallbackInfo {
        frames_played: state.frames_played.load(Ordering::Relaxed),
    };
    (state.callback)(out, &info);
    state
        .frames_played
        .fetch_add(num_frames as u64, Ordering::Release);
    AAUDIO_CALLBACK_RESULT_CONTINUE
}

unsafe extern "C" fn error_callback(
    _stream: *mut AAudioStream,
    user_data: *mut c_void,
    error: i32,
) {
    if user_data.is_null() {
        return;
    }
    let state = unsafe { &*(user_data as *const CallbackState) };
    state.async_error.store(error, Ordering::Release);
    state.stopped.store(true, Ordering::Release);
}

pub(crate) struct AAudioBackend;

impl Backend for AAudioBackend {
    fn name(&self) -> &'static str {
        "aaudio"
    }

    fn description(&self) -> &'static str {
        "Android AAudio native output"
    }

    fn probe(&self) -> Result<()> {
        let stream = open_query_stream(lib()?, None)?;
        let _ = queried_format(&stream.lib, stream.ptr)?;
        Ok(())
    }

    fn preferred_format(&self, device_id: Option<&str>) -> Result<StreamFormat> {
        let stream = open_query_stream(lib()?, device_id)?;
        queried_format(&stream.lib, stream.ptr)
    }

    fn open(&self, req: StreamRequest, cb: Callback) -> Result<Box<dyn StreamImpl>> {
        let lib = lib()?;
        let requested_device = parse_device_id(req.device.as_deref())?;

        let sample_rate = i32::try_from(req.sample_rate).map_err(|_| Error::UnsupportedFormat {
            backend: "aaudio",
            detail: format!(
                "sample rate {} exceeds AAudio's int32 range",
                req.sample_rate
            ),
        })?;
        let channels = i32::from(req.channels);
        let callback_frames = req
            .buffer_frames
            .map(i32::try_from)
            .transpose()
            .map_err(|_| Error::UnsupportedFormat {
                backend: "aaudio",
                detail: format!(
                    "buffer_frames {:?} exceeds AAudio's int32 range",
                    req.buffer_frames
                ),
            })?;

        let mut state = Box::new(CallbackState {
            callback: cb,
            channels: usize::from(req.channels),
            frames_played: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
            async_error: AtomicI32::new(AAUDIO_OK),
        });

        let builder = Builder::new(lib.clone())?;
        unsafe {
            (lib.AAudioStreamBuilder_setDirection)(builder.ptr, AAUDIO_DIRECTION_OUTPUT);
            (lib.AAudioStreamBuilder_setFormat)(builder.ptr, AAUDIO_FORMAT_PCM_FLOAT);
            (lib.AAudioStreamBuilder_setSampleRate)(builder.ptr, sample_rate);
            (lib.AAudioStreamBuilder_setChannelCount)(builder.ptr, channels);
            (lib.AAudioStreamBuilder_setPerformanceMode)(
                builder.ptr,
                AAUDIO_PERFORMANCE_MODE_LOW_LATENCY,
            );
            if let Some(set_usage) = lib.AAudioStreamBuilder_setUsage {
                set_usage(builder.ptr, aaudio_usage(req.usage));
            }
            if let Some(set_content_type) = lib.AAudioStreamBuilder_setContentType {
                set_content_type(builder.ptr, aaudio_content_type(req.content_type));
            }

            if let Some(id) = requested_device {
                (lib.AAudioStreamBuilder_setDeviceId)(builder.ptr, id);
            }

            if let Some(frames) = callback_frames {
                (lib.AAudioStreamBuilder_setFramesPerDataCallback)(builder.ptr, frames);
                let capacity = frames.saturating_mul(2).max(frames);
                (lib.AAudioStreamBuilder_setBufferCapacityInFrames)(builder.ptr, capacity);
            }

            let user_data = (&mut *state as *mut CallbackState).cast::<c_void>();
            (lib.AAudioStreamBuilder_setDataCallback)(builder.ptr, Some(data_callback), user_data);
            (lib.AAudioStreamBuilder_setErrorCallback)(
                builder.ptr,
                Some(error_callback),
                user_data,
            );
        }

        let mut raw = ptr::null_mut();
        let result = unsafe { (lib.AAudioStreamBuilder_openStream)(builder.ptr, &mut raw) };
        if result != AAUDIO_OK || raw.is_null() {
            return Err(Error::DeviceOpen {
                backend: "aaudio",
                detail: format!(
                    "AAudioStreamBuilder_openStream failed: {} ({result})",
                    lib.result_text(result)
                ),
            });
        }
        let raw = RawStream {
            lib: lib.clone(),
            ptr: raw,
        };
        verify_device_id(&lib, raw.ptr, requested_device)?;

        let format = queried_format(&lib, raw.ptr)?;
        state.channels = usize::from(format.channels);
        let capacity =
            unsafe { (lib.AAudioStream_getBufferCapacityInFrames)(raw.ptr) }.max(0) as u64;

        let result = unsafe { (lib.AAudioStream_requestStart)(raw.ptr) };
        if result != AAUDIO_OK {
            return Err(Error::Runtime {
                backend: "aaudio",
                detail: format!(
                    "AAudioStream_requestStart failed: {} ({result})",
                    lib.result_text(result)
                ),
            });
        }

        Ok(Box::new(AAudioStreamHandle {
            lib,
            stream: raw.into_raw(),
            state,
            format,
            fallback_capacity_frames: capacity,
        }))
    }
}

struct AAudioStreamHandle {
    lib: Arc<AAudioLib>,
    stream: *mut AAudioStream,
    state: Box<CallbackState>,
    format: StreamFormat,
    fallback_capacity_frames: u64,
}

// AAudio owns the callback thread.  The public Stream handle may move between
// application threads; its raw AAudio pointer is only operated on through
// AAudio's thread-safe stream-control functions.  The callback state remains
// heap-stable for the lifetime of the stream.
unsafe impl Send for AAudioStreamHandle {}

impl AAudioStreamHandle {
    fn check_async_error(&self) -> Result<()> {
        let error = self.state.async_error.load(Ordering::Acquire);
        if error == AAUDIO_OK {
            Ok(())
        } else {
            Err(Error::Runtime {
                backend: "aaudio",
                detail: format!(
                    "AAudio reported asynchronous stream error: {} ({error})",
                    self.lib.result_text(error)
                ),
            })
        }
    }

    fn request(
        &self,
        operation: &'static str,
        f: unsafe extern "C" fn(*mut AAudioStream) -> i32,
    ) -> Result<()> {
        self.check_async_error()?;
        if self.stream.is_null() {
            return Err(Error::Runtime {
                backend: "aaudio",
                detail: format!("{operation} on closed stream"),
            });
        }
        let result = unsafe { f(self.stream) };
        if result == AAUDIO_OK {
            Ok(())
        } else {
            Err(Error::Runtime {
                backend: "aaudio",
                detail: format!(
                    "{operation} failed: {} ({result})",
                    self.lib.result_text(result)
                ),
            })
        }
    }

    fn close(&mut self) {
        if self.stream.is_null() {
            return;
        }
        self.state.stopped.store(true, Ordering::Release);
        unsafe {
            let _ = (self.lib.AAudioStream_requestStop)(self.stream);
            let _ = (self.lib.AAudioStream_close)(self.stream);
        }
        self.stream = ptr::null_mut();
    }
}

impl StreamImpl for AAudioStreamHandle {
    fn play(&mut self) -> Result<()> {
        self.state.stopped.store(false, Ordering::Release);
        if let Err(error) = self.request(
            "AAudioStream_requestStart",
            self.lib.AAudioStream_requestStart,
        ) {
            self.state.stopped.store(true, Ordering::Release);
            return Err(error);
        }
        Ok(())
    }

    fn pause(&mut self) -> Result<()> {
        self.request(
            "AAudioStream_requestPause",
            self.lib.AAudioStream_requestPause,
        )
    }

    fn format(&self) -> StreamFormat {
        self.format
    }

    fn latency(&self) -> Option<Duration> {
        if self.stream.is_null() {
            return None;
        }

        let written = unsafe { (self.lib.AAudioStream_getFramesWritten)(self.stream) };
        let read = unsafe { (self.lib.AAudioStream_getFramesRead)(self.stream) };
        let frames = if written >= 0 && read >= 0 {
            written.saturating_sub(read).max(0) as u64
        } else {
            self.fallback_capacity_frames
        };

        Some(frames_to_duration(frames, self.format.sample_rate))
    }

    fn stop(&mut self) {
        self.close();
    }
}

fn frames_to_duration(frames: u64, sample_rate: u32) -> Duration {
    let rate = u64::from(sample_rate.max(1));
    let nanos = frames.saturating_mul(1_000_000_000) / rate;
    Duration::from_nanos(nanos)
}
