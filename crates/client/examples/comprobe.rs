//! Repro + fix proof for "Failed to build audio client: Cannot change thread
//! mode after it is set": cpal 0.18 opens DEFAULT devices through
//! ActivateAudioInterfaceAsync, which Windows only permits from an MTA COM
//! thread — and cpal pins bare threads to STA. Opens the default input and
//! output from an STA thread and from an MTA thread and reports both.
//! Also prints per-channel mic energy to justify the strongest-channel
//! downmix (out-of-phase array pairs cancel when averaged).

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

fn open_defaults(label: &str, com: impl FnOnce() + Send + 'static) -> std::thread::JoinHandle<()> {
    let label = label.to_owned();
    std::thread::spawn(move || {
        com();
        let host = cpal::default_host();

        let out = host.default_output_device();
        match out {
            None => println!("[{label}] output: no default device"),
            Some(d) => {
                let cfg = cpal::StreamConfig { channels: 2, sample_rate: 48000, buffer_size: cpal::BufferSize::Default };
                let r = d.build_output_stream(cfg, |_: &mut [f32], _: &_| {}, |_| {}, None);
                println!("[{label}] default OUTPUT open: {}", match &r { Ok(_) => "OK".into(), Err(e) => format!("ERR: {e}") });
            }
        }

        match host.default_input_device() {
            None => println!("[{label}] input: no default device"),
            Some(d) => match d.default_input_config() {
                Err(e) => println!("[{label}] default INPUT config: ERR: {e}"),
                Ok(cfg) => {
                    println!("[{label}] default INPUT config: OK ({} ch @ {} Hz, {:?})",
                        cfg.channels(), cfg.sample_rate(), cfg.sample_format());
                    let ch = cfg.channels() as usize;
                    let (tx, rx) = std::sync::mpsc::channel::<Vec<f32>>();
                    let stream = d.build_input_stream(
                        cfg.config(),
                        move |data: &[f32], _: &_| { let _ = tx.send(data.to_vec()); },
                        |_| {},
                        None,
                    );
                    match stream {
                        Err(e) => println!("[{label}] default INPUT open: ERR: {e}"),
                        Ok(s) => {
                            let _ = s.play();
                            let mut per: Vec<f64> = vec![0.0; ch];
                            let mut n = 0usize;
                            let end = std::time::Instant::now() + std::time::Duration::from_secs(2);
                            while std::time::Instant::now() < end {
                                if let Ok(buf) = rx.recv_timeout(std::time::Duration::from_millis(200)) {
                                    for f in buf.chunks(ch) {
                                        for (c, v) in f.iter().enumerate() {
                                            per[c] += (*v as f64) * (*v as f64);
                                        }
                                        n += 1;
                                    }
                                }
                            }
                            let rms: Vec<i64> = per.iter()
                                .map(|e| ((e / n.max(1) as f64).sqrt() * 32768.0) as i64)
                                .collect();
                            println!("[{label}] default INPUT open: OK — {n} frames, per-channel ambient RMS {rms:?}");
                        }
                    }
                }
            },
        }
    })
}

fn main() {
    // STA first: this is the thread state cpal's own COM init produces.
    open_defaults("STA", || {
        unsafe {
            use winapi::um::combaseapi::CoInitializeEx;
            use winapi::um::objbase::COINIT_APARTMENTTHREADED;
            let _ = CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED);
        }
    })
    .join()
    .unwrap();

    // MTA: the fix.
    open_defaults("MTA", || {
        unsafe {
            use winapi::um::combaseapi::CoInitializeEx;
            use winapi::um::objbase::COINIT_MULTITHREADED;
            let _ = CoInitializeEx(std::ptr::null_mut(), COINIT_MULTITHREADED);
        }
    })
    .join()
    .unwrap();
}
