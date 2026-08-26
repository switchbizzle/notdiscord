//! Diagnostic: run the exact blip playback path and report everything the
//! real one swallows. Usage: cargo run -p client --example bliptest

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

fn device_name(device: &cpal::Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_string())
}

fn main() {
    let host = cpal::default_host();
    let preferred: Option<String> = std::fs::read_to_string(
        dirs::config_dir().unwrap().join("NotDiscord").join("settings.json"),
    )
    .ok()
    .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    .and_then(|v| v["output_device"].as_str().map(str::to_owned));
    println!("settings output_device: {preferred:?}");

    println!("default output: {:?}", host.default_output_device().and_then(|d| device_name(&d)));
    println!("--- all output devices:");
    if let Ok(devices) = host.output_devices() {
        for d in devices {
            println!("  - {:?}", device_name(&d));
        }
    }

    // Same selection logic as voice.rs::pick_output_device.
    let device = preferred
        .as_ref()
        .and_then(|name| {
            host.output_devices()
                .ok()?
                .find(|d| device_name(&d).as_deref() == Some(name.as_str()))
        })
        .or_else(|| host.default_output_device())
        .expect("no output device");
    println!("picked: {:?}", device_name(&device));
    println!("default config: {:?}", device.default_output_config());
    println!("--- supported output configs:");
    if let Ok(configs) = device.supported_output_configs() {
        for c in configs {
            println!(
                "  {:?} ch={} rates {}..{}",
                c.sample_format(),
                c.channels(),
                c.min_sample_rate(),
                c.max_sample_rate()
            );
        }
    }

    // The blip, exactly as the app builds it.
    const RATE: u32 = 48000;
    let mut samples: Vec<f32> = Vec::new();
    for (freq, ms) in [(440.0f32, 70u32), (587.33, 90)] {
        let n = RATE * ms / 1000;
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            let env = (1.0 - i as f32 / n as f32).powf(1.4);
            samples.push((t * freq * std::f32::consts::TAU).sin() * env * 0.22);
        }
    }
    let total = samples.len();
    println!("\nblip samples: {total} ({} ms)", total * 1000 / RATE as usize);

    let config = cpal::StreamConfig {
        channels: 2,
        sample_rate: RATE,
        buffer_size: cpal::BufferSize::Default,
    };
    println!("requesting: {config:?}");

    let mut pos = 0usize;
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls_cb = calls.clone();
    let stream = device.build_output_stream(
        config,
        move |out: &mut [f32], _: &_| {
            calls_cb.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            for frame in out.chunks_mut(2) {
                let s = samples.get(pos).copied().unwrap_or(0.0);
                for sample in frame {
                    *sample = s;
                }
                pos += 1;
            }
            if pos >= total {
                let _ = done_tx.send(());
            }
        },
        |e| eprintln!("STREAM ERROR: {e}"),
        None,
    );

    match stream {
        Ok(stream) => {
            println!("build_output_stream: OK");
            match stream.play() {
                Ok(()) => println!("play(): OK — you should hear a rising blip"),
                Err(e) => println!("play() FAILED: {e}"),
            }
            let got = done_rx.recv_timeout(std::time::Duration::from_secs(2));
            println!("drain signal: {got:?}");
            std::thread::sleep(std::time::Duration::from_millis(300));
            println!("callback invocations: {}", calls.load(std::sync::atomic::Ordering::Relaxed));
        }
        Err(e) => println!("build_output_stream FAILED: {e}"),
    }
}
