//! Small cross-platform oxideav-sysaudio acceptance test.
//!
//! Silent by default so it is safe to run repeatedly on a development
//! workstation. Pass `--tone` for a quiet 440 Hz audible check.

use std::f32::consts::TAU;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use oxideav_sysaudio::{driver_by_name, drivers, open, Driver, Error, SampleFormat, StreamRequest};

const USAGE: &str = "Usage: cargo run --example smoke -- [OPTIONS]\n\n\
Options:\n\
  --driver <NAME>          Force a backend (for example oss, aaudio, mock)\n\
  --device <ID>            Backend-native device id\n\
  --rate <HZ>              Requested sample rate (default: preferred or 48000)\n\
  --channels <N>           Requested channels (default: preferred or 2)\n\
  --buffer-frames <N>      Request a callback/period size\n\
  --tone                   Play a quiet 440 Hz tone instead of silence\n\
  -h, --help               Show this help";

#[derive(Debug, Default)]
struct Args {
    driver: Option<String>,
    device: Option<String>,
    rate: Option<u32>,
    channels: Option<u16>,
    buffer_frames: Option<u32>,
    tone: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "--tone" => parsed.tone = true,
            "--driver" => {
                parsed.driver = Some(
                    args.next()
                        .ok_or_else(|| "--driver requires a backend name".to_owned())?,
                );
            }
            "--device" => {
                parsed.device = Some(
                    args.next()
                        .ok_or_else(|| "--device requires an id".to_owned())?,
                );
            }
            "--rate" => {
                parsed.rate = Some(parse_value(
                    "--rate",
                    args.next()
                        .ok_or_else(|| "--rate requires a value".to_owned())?,
                )?);
            }
            "--channels" => {
                parsed.channels = Some(parse_value(
                    "--channels",
                    args.next()
                        .ok_or_else(|| "--channels requires a value".to_owned())?,
                )?);
            }
            "--buffer-frames" => {
                parsed.buffer_frames = Some(parse_value(
                    "--buffer-frames",
                    args.next()
                        .ok_or_else(|| "--buffer-frames requires a value".to_owned())?,
                )?);
            }
            _ => return Err(format!("unknown option {arg:?}\n\n{USAGE}")),
        }
    }

    Ok(parsed)
}

fn parse_value<T>(flag: &str, value: String) -> Result<T, String>
where
    T: std::str::FromStr,
{
    value
        .parse::<T>()
        .map_err(|_| format!("{flag}: invalid value {value:?}"))
}

fn select_driver(args: &Args) -> Result<Driver, String> {
    if let Some(name) = args.driver.as_deref() {
        return driver_by_name(name)
            .ok_or_else(|| format!("backend {name:?} is not compiled for this target"));
    }

    oxideav_sysaudio::default_driver()
        .ok_or_else(|| "probe found no usable audio output backend".to_owned())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("FAIL: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;

    println!("oxideav-sysaudio smoke test");
    println!(
        "target: {}-{}",
        std::env::consts::ARCH,
        std::env::consts::OS
    );

    let compiled = drivers();
    if compiled.is_empty() {
        return Err("no sysaudio backends were compiled for this target".into());
    }

    println!("compiled backends:");
    for driver in &compiled {
        println!(
            "  {:10} {:<12} {}",
            driver.name(),
            driver.status(),
            driver.description()
        );
    }

    let driver = select_driver(&args)?;
    println!("selected backend: {}", driver.name());

    let devices = driver
        .output_devices()
        .map_err(|error| format!("device enumeration failed: {error}"))?;
    if devices.is_empty() {
        println!("devices: backend does not expose native enumeration");
    } else {
        println!("devices:");
        for device in &devices {
            let default = if device.is_default { " [default]" } else { "" };
            println!("  {}{} ({})", device.name, default, device.id);
        }
    }

    match driver.default_output_device() {
        Ok(Some(device)) => println!("default device: {} ({})", device.name, device.id),
        Ok(None) => println!("default device: not exposed by backend"),
        Err(error) => return Err(format!("default-device query failed: {error}")),
    }

    let preferred = driver
        .preferred_format(None)
        .map_err(|error| format!("preferred-format query failed: {error}"))?;
    match preferred {
        Some(format) => println!(
            "preferred format: {} Hz, {} ch, {:?}",
            format.sample_rate, format.channels, format.format
        ),
        None => println!("preferred format: not exposed by backend"),
    }

    match open(driver, StreamRequest::new(0, 2), |_, _| {}) {
        Err(Error::UnsupportedFormat { .. }) => {
            println!("invalid-request guard: PASS");
        }
        Err(error) => {
            return Err(format!(
                "invalid-request guard returned the wrong error: {error}"
            ));
        }
        Ok(stream) => {
            stream.stop();
            return Err("invalid sample_rate=0 request unexpectedly opened".into());
        }
    }

    let rate = args
        .rate
        .or(preferred.map(|format| format.sample_rate))
        .unwrap_or(48_000);
    let channels = args
        .channels
        .or(preferred.map(|format| format.channels))
        .unwrap_or(2);

    let mut request = StreamRequest::new(rate, channels);
    request.format = SampleFormat::F32;
    if let Some(frames) = args.buffer_frames {
        request = request.with_buffer_frames(Some(frames));
    }
    if let Some(device) = args.device.as_deref() {
        request = request.with_device(device);
    }

    println!(
        "open request: {} Hz, {} ch, buffer_frames={:?}, device={:?}, tone={}",
        request.sample_rate, request.channels, request.buffer_frames, request.device, args.tone
    );

    let callback_calls = Arc::new(AtomicU64::new(0));
    let rendered_frames = Arc::new(AtomicU64::new(0));
    let callback_calls_audio = callback_calls.clone();
    let rendered_frames_audio = rendered_frames.clone();

    let tone = args.tone;
    let tone_channels = usize::from(channels.max(1));
    let phase_step = TAU * 440.0 / rate as f32;
    let mut phase = 0.0f32;

    let mut stream = open(driver, request, move |out, info| {
        callback_calls_audio.fetch_add(1, Ordering::Relaxed);
        rendered_frames_audio.store(info.frames_played, Ordering::Relaxed);

        if tone {
            for frame in out.chunks_mut(tone_channels) {
                let sample = phase.sin() * 0.04;
                frame.fill(sample);
                phase += phase_step;
                if phase >= TAU {
                    phase -= TAU;
                }
            }
        } else {
            out.fill(0.0);
        }
    })
    .map_err(|error| format!("stream open failed: {error}"))?;

    let actual = stream.format();
    println!(
        "opened format: {} Hz, {} ch, {:?}",
        actual.sample_rate, actual.channels, actual.format
    );
    if !stream.is_playing() {
        return Err("new stream did not report playing state".into());
    }

    thread::sleep(Duration::from_millis(350));
    let warm_calls = callback_calls.load(Ordering::Relaxed);
    if warm_calls == 0 {
        return Err("audio callback did not run after open".into());
    }
    println!(
        "callback warm-up: {warm_calls} calls, frames_played={} latency={:?}",
        rendered_frames.load(Ordering::Relaxed),
        stream.latency()
    );

    stream.set_volume(0.25);
    if stream.volume() != 0.25 {
        return Err(format!(
            "software volume round-trip failed: got {}",
            stream.volume()
        ));
    }
    println!("software volume: PASS");

    stream
        .pause()
        .map_err(|error| format!("pause failed: {error}"))?;
    if stream.is_playing() {
        return Err("stream still reports playing after pause".into());
    }
    let before_pause_wait = callback_calls.load(Ordering::Relaxed);
    thread::sleep(Duration::from_millis(200));
    let after_pause_wait = callback_calls.load(Ordering::Relaxed);
    println!(
        "pause: PASS (callbacks during asynchronous pause window: {})",
        after_pause_wait.saturating_sub(before_pause_wait)
    );

    stream
        .play()
        .map_err(|error| format!("resume failed: {error}"))?;
    if !stream.is_playing() {
        return Err("stream does not report playing after resume".into());
    }
    thread::sleep(Duration::from_millis(350));
    let resumed_calls = callback_calls.load(Ordering::Relaxed);
    if resumed_calls <= after_pause_wait {
        return Err("audio callback did not resume after play()".into());
    }
    println!(
        "resume: PASS ({} callbacks after resume)",
        resumed_calls - after_pause_wait
    );

    stream.set_volume(1.0);
    stream.stop();
    println!("stop/drop: PASS");
    println!("PASS");

    Ok(())
}
