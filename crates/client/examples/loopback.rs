//! Does WASAPI loopback actually hear what the machine is playing?
//!
//! The whole desktop-audio feature rests on one cpal behaviour: building an
//! INPUT stream on an OUTPUT device transparently turns on
//! AUDCLNT_STREAMFLAGS_LOOPBACK. This plays a tone out of the chosen device
//! and listens to that same device at the same time, then reports what came
//! back — so "loopback works here" is measured rather than assumed.
//!
//!   cargo run -p client --example loopback
//!   cargo run -p client --example loopback -- "Speakers (Realtek(R) Audio)"
//!
//! Expect LOUD. Anything near silence means loopback gave us nothing, which
//! is the failure this exists to catch.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// cpal 0.18 names a device through its description.
fn name_of(d: &cpal::Device) -> String {
    d.description().map(|x| x.name().to_string()).unwrap_or_else(|_| "?".into())
}

/// Running peak and sum-of-squares, so the listener stays allocation-free.
#[derive(Default)]
struct Meter {
    peak: AtomicU64,
    sum_sq: AtomicU64,
    samples: AtomicU64,
}

impl Meter {
    fn feed(&self, v: f32) {
        let a = (v.abs() * 32768.0) as u64;
        self.peak.fetch_max(a, Ordering::Relaxed);
        self.sum_sq.fetch_add(a * a, Ordering::Relaxed);
        self.samples.fetch_add(1, Ordering::Relaxed);
    }
    fn rms(&self) -> f64 {
        let n = self.samples.load(Ordering::Relaxed).max(1);
        ((self.sum_sq.load(Ordering::Relaxed) as f64) / n as f64).sqrt()
    }
}

fn main() -> anyhow::Result<()> {
    let wanted = std::env::args().nth(1);
    let host = cpal::default_host();

    println!("output devices:");
    if let Ok(devs) = host.output_devices() {
        for d in devs {
            println!("  - {}", name_of(&d));
        }
    }

    let device = match &wanted {
        Some(name) => host
            .output_devices()?
            .find(|d| name_of(d) == *name)
            .ok_or_else(|| anyhow::anyhow!("no output device named {name}"))?,
        None => host.default_output_device().ok_or_else(|| anyhow::anyhow!("no default output"))?,
    };
    let name = name_of(&device);
    let config = device.default_output_config()?;
    println!(
        "\ncapturing: {name}\n  {} Hz, {} ch, {:?}",
        config.sample_rate(),
        config.channels(),
        config.sample_format()
    );

    // Listener: an INPUT stream on an OUTPUT device == loopback.
    let meter = Arc::new(Meter::default());
    let m = meter.clone();
    let listen = device.build_input_stream(
        config.config(),
        move |data: &[f32], _: &_| {
            for s in data {
                m.feed(*s);
            }
        },
        |e| eprintln!("listen error: {e}"),
        None,
    )?;
    listen.play()?;

    // Silence first, to show the meter isn't just reporting noise.
    std::thread::sleep(std::time::Duration::from_millis(600));
    let quiet_peak = meter.peak.load(Ordering::Relaxed);
    let quiet_rms = meter.rms();
    println!("\nwith nothing playing: peak {quiet_peak}, rms {quiet_rms:.1}");

    // Now make a noise on that same device: 440Hz, both channels.
    let channels = config.channels() as usize;
    let rate = config.sample_rate() as f32;
    let mut phase = 0.0f32;
    let tone = device.build_output_stream(
        config.config(),
        move |out: &mut [f32], _: &_| {
            for frame in out.chunks_mut(channels) {
                let v = (phase * std::f32::consts::TAU).sin() * 0.25;
                phase = (phase + 440.0 / rate).fract();
                for s in frame.iter_mut() {
                    *s = v;
                }
            }
        },
        |e| eprintln!("tone error: {e}"),
        None,
    )?;

    meter.peak.store(0, Ordering::Relaxed);
    meter.sum_sq.store(0, Ordering::Relaxed);
    meter.samples.store(0, Ordering::Relaxed);

    tone.play()?;
    println!("playing a 440Hz tone for 2s…");
    std::thread::sleep(std::time::Duration::from_secs(2));
    drop(tone);

    let loud_peak = meter.peak.load(Ordering::Relaxed);
    let loud_rms = meter.rms();
    println!("while playing:        peak {loud_peak}, rms {loud_rms:.1}");

    println!();
    if loud_rms > quiet_rms * 8.0 && loud_peak > 1000 {
        println!("LOOPBACK WORKS — it heard the tone ({:.0}x louder than silence)", loud_rms / quiet_rms.max(1.0));
        Ok(())
    } else {
        anyhow::bail!(
            "loopback captured nothing useful (quiet rms {quiet_rms:.1}, loud rms {loud_rms:.1})"
        )
    }
}
