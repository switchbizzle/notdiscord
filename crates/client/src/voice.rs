//! Voice engine: connects to a LiveKit room, publishes the microphone, and
//! plays back remote participants' audio.
//!
//! cpal streams are !Send, so each audio device stream lives on its own OS
//! thread; those threads exit when their stop-channel sender is dropped
//! (i.e. when the `ActiveCall` owning it is dropped).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use dioxus::prelude::*;
use futures_util::StreamExt;
use livekit::options::{AudioEncoding, TrackPublishOptions, VideoEncoding};
use livekit::track::{LocalAudioTrack, LocalTrack, LocalVideoTrack, RemoteTrack, RemoteVideoTrack, TrackSource};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::{RtcVideoSource, VideoResolution};
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::{AudioSourceOptions, RtcAudioSource};
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::{Room, RoomEvent};

#[derive(Clone, Debug, PartialEq)]
pub struct VoiceParticipant {
    pub identity: String,
    pub name: String,
    pub speaking: bool,
    pub is_me: bool,
    pub sharing: bool,
    pub camera: bool,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct VoiceStatus {
    pub channel_id: Option<i64>,
    pub channel_name: String,
    pub connecting: bool,
    pub muted: bool,
    pub deafened: bool,
    /// We are currently sharing our screen.
    pub sharing_self: bool,
    /// The screen share is carrying desktop audio too.
    pub sharing_audio: bool,
    /// Our webcam is currently on.
    pub camera_self: bool,
    /// Push-to-talk mode is active and the key is currently held.
    pub ptt_held: bool,
    pub participants: Vec<VoiceParticipant>,
    /// Playback volume per identity (1.0 = 100%).
    pub volumes: HashMap<String, f32>,
    pub error: String,
    /// The call dropped on its own and we mean to get back in. LiveKit only
    /// reports Disconnected after its own retries have failed, so this means
    /// the room is genuinely gone, not merely stuttering.
    pub dropped: bool,
    /// How many rejoin attempts have been made since the drop, so the caller
    /// can back off and eventually stop.
    pub rejoin_attempts: u32,
}

/// What to tell the encoder it is working on, given the screen's real size.
///
/// Anything at or under the chosen cap is declared as-is; above it we aim at
/// the cap. A 4K desktop encoded at 4K costs more CPU and uplink than a home
/// machine reliably has, and the failure mode there (dropped frames) is worse
/// than a slight downscale. Aspect ratio is preserved so an ultrawide isn't
/// squashed. The cap is a setting because the right answer depends on what is
/// on the screen (Jon).
fn share_encode_size((width, height): (u32, u32), max_height: u32) -> (u32, u32) {
    let max_height = max_height.clamp(240, 2160);
    if height <= max_height || height == 0 {
        return (width.max(1), height.max(1));
    }
    let scaled_w = (u64::from(width) * u64::from(max_height) / u64::from(height)) as u32;
    // Encoders want even dimensions.
    ((scaled_w & !1).max(2), max_height)
}

/// Bitrate ceiling for a screen share of the given size.
///
/// Scaled off the library's own screenshare presets but well above them,
/// because those are tuned for a desktop in motion and this is usually
/// somebody's editor. A ceiling is not a promise: congestion control still
/// spends less when the link can't carry it.
fn screen_share_bitrate(width: u32, height: u32, fps: u32) -> u64 {
    let pixels = u64::from(width) * u64::from(height);
    let base: u64 = match pixels {
        // Up to 1080p
        0..=2_100_000 => 5_000_000,
        // Up to 1440p
        2_100_001..=3_700_000 => 8_000_000,
        // 4K and beyond
        _ => 12_000_000,
    };
    // Twice the frames is not twice the bits — consecutive frames are more
    // alike the faster you sample, so the codec spends less on each. These
    // are the usual broadcast ratios, and they land 1080p60 near the 7-9Mbps
    // Jon put it at. Capped at 20Mbps, which is about what a good home
    // upload actually sustains; above that the ceiling is fiction.
    let scaled = base * u64::from(fps_bitrate_pct(fps)) / 100;
    scaled.min(20_000_000)
}

/// Bitrate as a percentage of the 15fps figure.
fn fps_bitrate_pct(fps: u32) -> u32 {
    match fps {
        0..=15 => 100,
        16..=30 => 140,
        _ => 200,
    }
}

/// What gives first when the link cannot carry the whole picture.
///
/// Naming to be careful with: by the WebRTC spec, MaintainResolution keeps
/// the pixels and drops frames, and MaintainFramerate does the reverse.
/// (The doc comments on the enum in libwebrtc read the other way round; the
/// spec, and the SDK's own choice of MaintainResolution as the screenshare
/// default, are what these follow.) So someone sharing an editor keeps their
/// text legible and loses motion, and someone sharing a game keeps it smooth
/// and loses a little sharpness — which is exactly what picking 60 means.
fn share_degradation(fps: u32) -> livekit::options::DegradationPreference {
    use livekit::options::DegradationPreference;
    if fps >= 60 {
        DegradationPreference::MaintainFramerate
    } else {
        DegradationPreference::MaintainResolution
    }
}

/// Whether you've chosen not to watch your own screen share. A static rather
/// than part of VoiceStatus: the status resets on every join and leave, and
/// this is a preference that outlives calls. Both places that show your share
/// — the Video tab and the call window — read it, and with it on neither asks
/// for frames, so the capture stops making preview copies altogether. What
/// everyone else receives is untouched.
static OWN_PREVIEW_HIDDEN: AtomicBool = AtomicBool::new(false);

pub fn own_preview_hidden() -> bool {
    OWN_PREVIEW_HIDDEN.load(Ordering::Relaxed)
}

pub enum VoiceCmd {
    Join { channel_id: i64, channel_name: String, url: String, token: String },
    Leave,
    ToggleMute,
    SetVolume { identity: String, volume: f32 },
    SetMicVolume(f32),
    SetMasterVolume(f32),
    SetNoiseSuppression(bool),
    /// Automatic mic gain on/off.
    SetAutoGain(bool),
    ToggleDeafen,
    /// mode: "vad" | "ptt"; key: device_query Keycode name.
    SetVoiceMode { mode: String, key: String },
    /// A key that toggles mute from anywhere. key: device_query Keycode name.
    SetMuteHotkey { enabled: bool, key: String },
    /// Voice-activity gate threshold (RMS, 0 = always transmit).
    SetVadThreshold(f32),
    StartScreenShare { target: crate::share::ShareTarget },
    StopScreenShare,
    /// Open a viewer window for this participant's screen share.
    WatchScreen { identity: String },
    StartCamera,
    StopCamera,
    /// Open a viewer window for this participant's webcam.
    WatchCamera { identity: String },
    /// Stop (true) or start (false) showing you your own screen share.
    SetOwnPreviewHidden(bool),
}

pub fn parse_ptt_key(name: &str) -> device_query::Keycode {
    use device_query::Keycode::*;
    match name {
        "F1" => F1, "F2" => F2, "F3" => F3, "F4" => F4, "F5" => F5, "F6" => F6,
        "F7" => F7, "F8" => F8, "F10" => F10, "F11" => F11, "F12" => F12,
        "Grave" => Grave,
        "CapsLock" => CapsLock,
        "LShift" => LShift,
        "LControl" => LControl,
        "LAlt" => LAlt,
        _ => F9,
    }
}

/// True only on the tick a key goes down, false for as long as it stays
/// down. A toggle has to fire on the edge: the poller looks 33 times a
/// second, and an ordinary press lasts far longer than one tick, so reading
/// the level instead would flip mute a dozen times and land on whichever
/// side the release happened to fall.
fn pressed_now(down: bool, was_down: &mut bool) -> bool {
    let edge = down && !*was_down;
    *was_down = down;
    edge
}

pub const PTT_KEY_CHOICES: &[&str] = &[
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
    "Grave", "CapsLock", "LShift", "LControl", "LAlt",
];

/// Mic input level: peak RMS on the i16 scale since the last tick, updated
/// ~10x/sec while in a call (the UI maps it to dB). Kept separate from
/// VoiceStatus so only the meter widget re-renders on ticks.
pub type MicLevelSignal = Signal<f32, SyncStorage>;

/// WASAPI opens the *default* audio device through ActivateAudioInterfaceAsync,
/// which Windows only allows from a multithreaded-apartment (MTA) COM thread.
/// cpal initializes bare threads as STA, so any thread of ours that touches
/// audio must claim MTA first — otherwise "System default" devices fail with
/// "Cannot change thread mode after it is set" (RPC_E_CHANGED_MODE).
#[cfg(windows)]
pub fn com_init_mta() {
    use winapi::um::combaseapi::CoInitializeEx;
    use winapi::um::objbase::COINIT_MULTITHREADED;
    // Deliberately never CoUninitialize: these are audio worker threads whose
    // COM use lasts their whole life. An already-STA thread returns
    // RPC_E_CHANGED_MODE, which we can't fix here — ignore and let the caller
    // fail with its own error if it comes to that.
    unsafe {
        let _ = CoInitializeEx(std::ptr::null_mut(), COINIT_MULTITHREADED);
    }
}

#[cfg(not(windows))]
pub fn com_init_mta() {}

/// Runs `f` on a fresh MTA thread and returns its result. For cpal calls made
/// from async contexts (tokio workers), where we must not leave a COM
/// apartment mode behind on a shared pooled thread.
fn on_mta_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std_mpsc::channel();
    std::thread::spawn(move || {
        com_init_mta();
        let _ = tx.send(f());
    });
    rx.recv().expect("mta helper thread died")
}

/// Automatic mic gain: slowly walks quiet speech up toward a comfortable
/// level, the way every mainstream voice app does. Only adapts on frames
/// that contain real signal, so silence never gets boosted into hiss.
pub struct AutoGain {
    gain: f32,
}

/// Where speech should sit after AGC, in i16 RMS (≈ -20.7 dBFS).
const AGC_TARGET_RMS: f64 = 3000.0;
const AGC_MAX_GAIN: f32 = 16.0;
/// Frames quieter than this are silence/hiss and never drive adaptation.
const AGC_SIGNAL_FLOOR: f64 = 60.0;

impl AutoGain {
    pub fn new() -> Self {
        Self { gain: 1.0 }
    }

    /// Feed one 10ms frame's RMS (pre-AGC); returns the gain to apply to it.
    pub fn feed(&mut self, rms: f64) -> f32 {
        self.feed_against(rms, false)
    }

    /// The same, knowing whether the other side of the call is audible right
    /// now. While it is, the gain may come down but never goes up.
    ///
    /// Without this the auto level was the loudest echo in the call. On a
    /// speaker user, whatever the canceller cannot remove — and in a real
    /// room, with two devices whose clocks drift apart, that is not nothing —
    /// arrives here as a quiet signal above the floor, which is exactly what
    /// this was built to turn up. Measured (echo_measure): with 500ppm of
    /// drift the canceller takes 11.8 dB off the echo, and the auto level put
    /// 21.7 dB back, sending everyone their own voice louder than if there
    /// were no canceller at all. Held while the far end talks, it stays at
    /// whatever level your own voice set, so the echo keeps the reduction
    /// the canceller won.
    pub fn feed_against(&mut self, rms: f64, far_end_audible: bool) -> f32 {
        if rms > AGC_SIGNAL_FLOOR {
            let boosted = rms * self.gain as f64;
            if boosted < AGC_TARGET_RMS && !far_end_audible {
                // ~6 dB/s upward at 100 frames/s: fast enough to converge in
                // a few sentences, slow enough not to pump.
                self.gain = (self.gain * 1.007).min(AGC_MAX_GAIN);
            } else if boosted > AGC_TARGET_RMS * 2.5 {
                // Overshot toward clipping — come down much faster.
                self.gain = (self.gain * 0.97).max(1.0);
            }
        }
        self.gain
    }
}

/// Downmix interleaved device channels to mono by taking the STRONGEST
/// channel over the chunk. Averaging is the obvious move, but some laptop
/// mic arrays ship out-of-phase channel pairs that cancel to near-silence
/// when averaged — measured as "my mic barely picks anything up".
fn downmix_strongest(chunk: &[i16], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return chunk.iter().map(|s| *s as f32).collect();
    }
    let mut energy = vec![0f64; channels];
    for frame in chunk.chunks(channels) {
        for (ch, s) in frame.iter().enumerate() {
            energy[ch] += (*s as f64).abs();
        }
    }
    let strongest = energy
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
        .unwrap_or(0);
    chunk.chunks(channels).map(|f| *f.get(strongest).unwrap_or(&0) as f32).collect()
}

#[cfg(test)]
mod capture_tests {
    use super::*;

    #[test]
    fn antiphase_channels_survive_downmix() {
        // A stereo array sending +x / -x: averaging yields silence, the
        // strongest-channel downmix keeps the signal whole.
        let chunk: Vec<i16> = (0..480).flat_map(|i| {
            let s = (8000.0 * (i as f32 * 0.1).sin()) as i16;
            [s, -s]
        }).collect();
        let mono = downmix_strongest(&chunk, 2);
        let rms = (mono.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>() / mono.len() as f64).sqrt();
        assert!(rms > 4000.0, "signal lost in downmix: rms {rms}");
    }

    #[test]
    fn quiet_channel_is_ignored() {
        // Loud on ch1, near-silent on ch0: pick ch1.
        let chunk: Vec<i16> = (0..480).flat_map(|_| [10i16, 6000i16]).collect();
        let mono = downmix_strongest(&chunk, 2);
        assert!(mono.iter().all(|s| *s == 6000.0));
    }

    #[test]
    fn mono_passes_through() {
        let chunk = vec![100i16, -100, 50];
        assert_eq!(downmix_strongest(&chunk, 1), vec![100.0, -100.0, 50.0]);
    }

    #[test]
    fn agc_boosts_quiet_speech_and_holds_target() {
        let mut agc = AutoGain::new();
        // A quiet laptop mic: speech at RMS 250. Feed ~15s of speech frames.
        let mut gain = 1.0;
        for _ in 0..1500 {
            gain = agc.feed(250.0);
        }
        let level = 250.0 * gain as f64;
        assert!(gain > 4.0, "gain only reached {gain}");
        assert!(level > AGC_TARGET_RMS * 0.6 && level < AGC_TARGET_RMS * 2.5,
            "landed at {level}");
    }

    #[test]
    fn agc_never_turns_up_the_other_side_of_the_call() {
        // A speaker user, silent, while the others talk: what reaches the
        // auto level is whatever echo the canceller left. Ten seconds of it.
        let mut agc = AutoGain::new();
        for _ in 0..1000 {
            agc.feed_against(250.0, true);
        }
        assert_eq!(agc.feed_against(250.0, true), 1.0, "the echo was boosted");

        // Their own voice, with the far end quiet, still brings it up — and
        // the gain it earned holds through the next time the others talk.
        for _ in 0..1500 {
            agc.feed_against(250.0, false);
        }
        let earned = agc.feed_against(250.0, false);
        assert!(earned > 4.0, "own voice only reached {earned}");
        for _ in 0..500 {
            agc.feed_against(250.0, true);
        }
        assert_eq!(agc.feed_against(250.0, true), earned, "far end moved the gain");
    }

    #[test]
    fn far_end_audibility_follows_the_mix_and_lapses() {
        let m = Mixer::new(
            Arc::new(AtomicU32::new(1.0f32.to_bits())),
            Arc::new(AtomicBool::new(false)),
            None,
        );
        let far = m.far_end();
        assert!(!far_end_audible(&far), "nothing has played yet");

        m.add("a".into(), Arc::new(AtomicU32::new(1.0f32.to_bits())));
        m.push("a", &[0; 960]);
        let mut out = [0.0f32; 960];
        m.fill(&mut out);
        assert!(!far_end_audible(&far), "silence is not the far end talking");

        m.push("a", &[6000; 960]);
        m.fill(&mut out);
        assert!(far_end_audible(&far), "a voice in the mix was missed");

        // The echo of the last word is still on its way for a while, then
        // the room has gone quiet.
        let at = far.load(Ordering::Relaxed);
        assert!(far_end_recent(at, at + FAR_END_HOLD_MS - 1), "let go before the echo could arrive");
        assert!(!far_end_recent(at, at + FAR_END_HOLD_MS), "the hold never lapses");
    }

    #[test]
    fn agc_ignores_silence() {
        let mut agc = AutoGain::new();
        for _ in 0..2000 {
            agc.feed(20.0); // below the signal floor
        }
        assert_eq!(agc.feed(20.0), 1.0);
    }

    #[test]
    fn agc_backs_off_loud_input() {
        let mut agc = AutoGain::new();
        for _ in 0..500 {
            agc.feed(500.0); // gain climbs
        }
        let peak = agc.feed(500.0);
        for _ in 0..500 {
            agc.feed(20000.0); // suddenly shouting into a hot mic
        }
        assert!(agc.feed(20000.0) < peak.min(1.5), "did not back off");
    }
}

/// The voice-activity gate, fed one 10ms frame at a time. Opening takes
/// ATTACK_FRAMES consecutive frames above the threshold (so a keyboard click
/// can't pop it open); once open, any hot frame keeps it open, and it closes
/// SPEAK_HOLD after the last one. A floor keeps room hum from ever counting
/// as voice. Threshold 0 = classic open mic: always transmit, but the
/// speaking ring still only lights on real voice.
pub struct VadGate {
    hot_frames: u32,
    last_voice: std::time::Instant,
}

/// How long the gate (and speaking ring) stays hot past the last voice frame.
const SPEAK_HOLD: std::time::Duration = std::time::Duration::from_millis(600);
/// Consecutive 10ms frames above threshold needed to open a closed gate.
const ATTACK_FRAMES: u32 = 3;
/// RMS below this is never voice, whatever the slider says.
const VOICE_FLOOR: f64 = 300.0;

impl VadGate {
    pub fn new() -> Self {
        Self { hot_frames: 0, last_voice: std::time::Instant::now() - SPEAK_HOLD }
    }

    /// Returns (gate open, voice recent). Transmission in voice-activity mode
    /// follows the first; the local speaking ring follows both.
    pub fn feed(&mut self, rms: f64, threshold: f64, now: std::time::Instant) -> (bool, bool) {
        if rms >= threshold.max(VOICE_FLOOR) {
            self.hot_frames += 1;
            let already_open = now.duration_since(self.last_voice) < SPEAK_HOLD;
            if already_open || self.hot_frames >= ATTACK_FRAMES {
                self.last_voice = now;
            }
        } else {
            self.hot_frames = 0;
        }
        let voice_recent = now.duration_since(self.last_voice) < SPEAK_HOLD;
        (threshold <= 0.0 || voice_recent, voice_recent)
    }
}

#[cfg(test)]
mod share_quality_tests {
    use super::*;

    #[test]
    fn bitrate_follows_the_screen_being_shared() {
        // The point of the change: a bigger screen is not encoded as if it
        // were 1080p, which is what the hardcoded source resolution caused.
        let hd = screen_share_bitrate(1920, 1080, 15);
        let qhd = screen_share_bitrate(2560, 1440, 15);
        let uhd = screen_share_bitrate(3840, 2160, 15);
        assert!(hd < qhd && qhd < uhd, "more pixels should get more bits");
        // And every tier clears the library's own 1080p30 screenshare preset,
        // which is 3 Mbps and the ceiling we were living under.
        assert!(hd > 3_000_000, "1080p should beat the old default");

        // Odd shapes still land somewhere sensible rather than panicking.
        assert_eq!(screen_share_bitrate(1280, 720, 15), hd, "smaller than 1080p uses the same tier");
        assert_eq!(screen_share_bitrate(0, 0, 15), hd, "a zero size falls in the lowest tier");
        assert_eq!(screen_share_bitrate(5120, 2880, 15), uhd, "5K lands in the top tier");
        // An ultrawide has 1440p's height but far more pixels; it should be
        // treated by area, not by height.
        assert!(screen_share_bitrate(3440, 1440, 15) >= qhd);
    }

    #[test]
    fn a_faster_share_gets_more_bits_but_not_proportionally() {
        let slow = screen_share_bitrate(1920, 1080, 15);
        let mid = screen_share_bitrate(1920, 1080, 30);
        let fast = screen_share_bitrate(1920, 1080, 60);
        assert!(slow < mid && mid < fast, "more frames need more bits");
        // Four times the frames must not cost four times the link: the
        // frames are more alike the faster you sample them.
        assert!(fast < slow * 4, "bitrate should not scale with the frame count");
        // And it lands where broadcast practice puts 1080p60.
        assert!(
            (7_000_000..=11_000_000).contains(&fast),
            "1080p60 came out at {fast}, which is nowhere near the usual 7-9 Mbps"
        );
    }

    #[test]
    fn the_ceiling_stays_inside_a_real_home_upload() {
        // 4K at 60 would ask for 24 Mbps unclamped, which no home link
        // sustains; a ceiling nobody can reach is just a lie to the encoder.
        assert_eq!(screen_share_bitrate(3840, 2160, 60), 20_000_000);
    }

    #[test]
    fn what_gives_first_follows_the_rate_you_asked_for() {
        use livekit::options::DegradationPreference;
        // Sharing an editor: keep the pixels, drop frames.
        assert_eq!(share_degradation(15), DegradationPreference::MaintainResolution);
        assert_eq!(share_degradation(30), DegradationPreference::MaintainResolution);
        // Sharing something that moves: keep it smooth, lose a little detail.
        assert_eq!(share_degradation(60), DegradationPreference::MaintainFramerate);
    }

    #[test]
    fn the_resolution_cap_is_whatever_was_chosen() {
        // 720p cap on a 1440p screen: aimed down, aspect kept.
        let (w, h) = share_encode_size((2560, 1440), 720);
        assert_eq!(h, 720);
        assert_eq!(w, 1280, "16:9 should stay 16:9");
        // Already under the cap: untouched, not upscaled.
        assert_eq!(share_encode_size((1280, 720), 1440), (1280, 720));
        // An ultrawide keeps its shape on the way down.
        let (uw, uh) = share_encode_size((3440, 1440), 1080);
        assert_eq!(uh, 1080);
        assert!((uw as f64 / uh as f64 - 3440.0 / 1440.0).abs() < 0.01);
        assert_eq!(uw % 2, 0, "encoders want even dimensions");
    }

    #[test]
    fn a_4k_screen_is_aimed_at_1440p_without_squashing_it() {
        // Below the cap, left exactly alone.
        assert_eq!(share_encode_size((1920, 1080), 1440), (1920, 1080));
        assert_eq!(share_encode_size((2560, 1440), 1440), (2560, 1440));

        // 4K comes down to 1440p and keeps 16:9.
        let (w, h) = share_encode_size((3840, 2160), 1440);
        assert_eq!(h, 1440);
        assert_eq!(w, 2560);

        // An ultrawide keeps its shape rather than being squeezed to 16:9.
        let (uw, uh) = share_encode_size((5120, 2160), 1440);
        assert_eq!(uh, 1440);
        assert!((uw as f32 / uh as f32 - 5120.0 / 2160.0).abs() < 0.01, "aspect kept");

        // Even dimensions, and a zero height can't divide by zero.
        assert_eq!(share_encode_size((3441, 2161), 1440).0 % 2, 0);
        assert_eq!(share_encode_size((0, 0), 1440), (1, 1));
    }

    /// Depends on the machine's actual display, so it is not part of the
    /// normal run. Proves the source no longer declares a hardcoded 1080p:
    ///   cargo test -p client -- --ignored declared --nocapture
    #[test]
    #[ignore]
    fn the_declared_size_is_the_real_screen() {
        let size = crate::share::target_size(&crate::share::ShareTarget::PrimaryMonitor);
        let (w, h) = size.expect("this machine has a primary monitor");
        println!("primary monitor {w}x{h} -> {} bps", screen_share_bitrate(w, h, 15));
        assert!(w > 0 && h > 0);
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Feed `n` frames of the given level, 10ms apart, returning the last verdict.
    fn feed_n(gate: &mut VadGate, t: &mut Instant, rms: f64, thr: f64, n: u32) -> (bool, bool) {
        let mut out = (false, false);
        for _ in 0..n {
            *t += Duration::from_millis(10);
            out = gate.feed(rms, thr, *t);
        }
        out
    }

    #[test]
    fn click_does_not_open_gate() {
        let mut gate = VadGate::new();
        let mut t = Instant::now();
        // Two hot frames (a 20ms click), then quiet: never opens.
        assert_eq!(feed_n(&mut gate, &mut t, 5000.0, 1000.0, 2), (false, false));
        assert_eq!(feed_n(&mut gate, &mut t, 100.0, 1000.0, 5), (false, false));
    }

    #[test]
    fn sustained_voice_opens_then_holds_then_closes() {
        let mut gate = VadGate::new();
        let mut t = Instant::now();
        assert_eq!(feed_n(&mut gate, &mut t, 5000.0, 1000.0, 3), (true, true));
        // Quiet again: stays open through the hold...
        assert_eq!(feed_n(&mut gate, &mut t, 100.0, 1000.0, 50), (true, true));
        // ...and closes once the hold has fully elapsed.
        assert_eq!(feed_n(&mut gate, &mut t, 100.0, 1000.0, 20), (false, false));
    }

    #[test]
    fn open_gate_refreshes_on_single_hot_frame() {
        let mut gate = VadGate::new();
        let mut t = Instant::now();
        feed_n(&mut gate, &mut t, 5000.0, 1000.0, 3);
        // 40 quiet frames (400ms), then ONE hot frame: still open, hold reset.
        feed_n(&mut gate, &mut t, 100.0, 1000.0, 40);
        assert_eq!(feed_n(&mut gate, &mut t, 5000.0, 1000.0, 1), (true, true));
        assert_eq!(feed_n(&mut gate, &mut t, 100.0, 1000.0, 55), (true, true));
    }

    #[test]
    fn below_threshold_never_transmits() {
        let mut gate = VadGate::new();
        let mut t = Instant::now();
        // Loud-ish room noise under the user's threshold: closed forever.
        assert_eq!(feed_n(&mut gate, &mut t, 900.0, 1000.0, 200), (false, false));
    }

    #[test]
    fn floor_applies_when_threshold_is_lower() {
        let mut gate = VadGate::new();
        let mut t = Instant::now();
        // Threshold 50 but hum at 200 is under the 300 floor: closed.
        assert_eq!(feed_n(&mut gate, &mut t, 200.0, 50.0, 200), (false, false));
    }

    #[test]
    fn open_mic_transmits_but_ring_needs_voice() {
        let mut gate = VadGate::new();
        let mut t = Instant::now();
        // Threshold 0: gate open on silence, ring dark.
        assert_eq!(feed_n(&mut gate, &mut t, 10.0, 0.0, 5), (true, false));
        // Real voice: both.
        assert_eq!(feed_n(&mut gate, &mut t, 5000.0, 0.0, 3), (true, true));
    }
}

/// Thread-safe signal: voice status is updated from tokio worker tasks.
pub type VoiceStatusSignal = Signal<VoiceStatus, SyncStorage>;

/// The published mic track format: 48kHz mono, 10ms (480-sample) frames —
/// also what RNNoise requires.
const NS_RATE: u32 = 48000;
const NS_FRAME: usize = nnnoiseless::DenoiseState::FRAME_SIZE;

fn device_name(device: &cpal::Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_string())
}

pub fn list_input_devices() -> Vec<String> {
    cpal::default_host()
        .input_devices()
        .map(|devices| devices.filter_map(|d| device_name(&d)).collect())
        .unwrap_or_default()
}

pub fn list_output_devices() -> Vec<String> {
    cpal::default_host()
        .output_devices()
        .map(|devices| devices.filter_map(|d| device_name(&d)).collect())
        .unwrap_or_default()
}

fn pick_input_device(host: &cpal::Host, preferred: &Option<String>) -> Option<cpal::Device> {
    if let Some(name) = preferred {
        if let Ok(mut devices) = host.input_devices() {
            if let Some(d) = devices.find(|d| device_name(d).as_deref() == Some(name)) {
                return Some(d);
            }
        }
    }
    host.default_input_device()
}

fn pick_output_device(host: &cpal::Host, preferred: &Option<String>) -> Option<cpal::Device> {
    if let Some(name) = preferred {
        if let Ok(mut devices) = host.output_devices() {
            if let Some(d) = devices.find(|d| device_name(d).as_deref() == Some(name)) {
                return Some(d);
            }
        }
    }
    host.default_output_device()
}

/// 10ms of 48kHz stereo, the frame size the track is fed in.
const SHARE_FRAME_PER_CH: usize = 480;

/// A running desktop-audio capture. Dropping it stops the device thread and
/// the pump; the track is unpublished separately, by whoever published it.
pub struct ShareAudio {
    _stop: std_mpsc::Sender<()>,
    pump: tokio::task::JoinHandle<()>,
}

impl Drop for ShareAudio {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

/// Open a render endpoint for loopback capture.
///
/// cpal turns an input stream on an *output* device into WASAPI loopback,
/// which is what lets us hear what the machine is playing without a virtual
/// cable. The device is whatever the user picked, so they can share their
/// interface while the call itself plays somewhere else.
fn open_loopback(
    preferred: &Option<String>,
) -> anyhow::Result<(cpal::Device, cpal::SupportedStreamConfig)> {
    let host = cpal::default_host();
    let device = pick_output_device(&host, preferred)
        .ok_or_else(|| anyhow::anyhow!("no output device to capture"))?;
    let name = device_name(&device).unwrap_or_else(|| "unknown".into());
    let config = device
        .default_output_config()
        .map_err(|e| anyhow::anyhow!("cannot listen in on '{name}': {e}"))?;
    Ok((device, config))
}

/// Resample interleaved stereo to 48kHz, linearly, keeping the two channels
/// independent. `carry` holds the fractional read position and the last
/// frame across calls, so block boundaries don't click.
fn resample_stereo(input: &[i16], from_rate: u32, carry: &mut (f64, [f32; 2]), out: &mut Vec<i16>) {
    if input.is_empty() {
        return;
    }
    if from_rate == NS_RATE {
        out.extend_from_slice(input);
        return;
    }
    let step = from_rate as f64 / NS_RATE as f64;
    let frames = input.len() / 2;
    let (mut pos, mut last) = *carry;
    while pos < frames as f64 {
        let i = pos.floor() as usize;
        let frac = (pos - i as f64) as f32;
        for ch in 0..2 {
            let prev = if i == 0 { last[ch] } else { input[(i - 1) * 2 + ch] as f32 };
            let cur = input[i * 2 + ch] as f32;
            out.push((prev + (cur - prev) * frac).clamp(-32768.0, 32767.0) as i16);
        }
        pos += step;
    }
    last = [input[(frames - 1) * 2] as f32, input[(frames - 1) * 2 + 1] as f32];
    *carry = (pos - frames as f64, last);
}

/// Fold however many channels the endpoint has into interleaved stereo.
fn to_stereo(data: &[i16], channels: usize, out: &mut Vec<i16>) {
    match channels {
        0 => {}
        1 => out.extend(data.iter().flat_map(|s| [*s, *s])),
        2 => out.extend_from_slice(data),
        n => {
            // Surround: take the front pair and leave the rest. Mixing the
            // centre and surrounds in would need gains we cannot verify here,
            // and a wrong fold is worse than a plain one.
            for frame in data.chunks_exact(n) {
                out.push(frame[0]);
                out.push(frame[1]);
            }
        }
    }
}

/// Capture a render endpoint and publish it as its own stereo track.
///
/// Everything the microphone path does to make speech intelligible is left
/// out on purpose: no echo cancellation, no noise suppression (which is
/// trained on voices and eats reverb tails and pads), no automatic gain
/// (which pumps exactly the dynamics a mix is made of), and no voice gate
/// (which would drop quiet intros entirely). DTX and RED go too: DTX stops
/// transmitting through passages it reads as silence, and RED spends
/// bitrate on redundancy. Stereo Opus at whatever the setting says, which
/// is otherwise the same shape the music bot has been using.
async fn publish_share_audio(
    room: &Room,
    preferred: Option<String>,
    bitrate: u32,
    mut status: VoiceStatusSignal,
) -> anyhow::Result<(ShareAudio, livekit::id::TrackSid)> {
    let (device, config) =
        tokio::task::spawn_blocking(move || { com_init_mta(); open_loopback(&preferred) }).await??;
    let rate = config.sample_rate();
    let channels = config.channels() as usize;

    let source = NativeAudioSource::new(
        AudioSourceOptions {
            echo_cancellation: false,
            noise_suppression: false,
            auto_gain_control: false,
        },
        NS_RATE,
        2,
        1000,
    );
    let track =
        LocalAudioTrack::create_audio_track("screen-audio", RtcAudioSource::Native(source.clone()));
    let publication = room
        .local_participant()
        .publish_track(
            LocalTrack::Audio(track),
            TrackPublishOptions {
                source: TrackSource::ScreenshareAudio,
                audio_encoding: Some(AudioEncoding { max_bitrate: bitrate as u64 }),
                dtx: false,
                red: false,
                ..Default::default()
            },
        )
        .await?;

    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<i16>>();
    let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
    let mut err_status = status;
    std::thread::spawn(move || {
        com_init_mta();
        match build_capture_stream(&device, &config, frame_tx, err_status, "desktop audio") {
            Ok(stream) => match stream.play() {
                Ok(()) => {
                    let _ = stop_rx.recv();
                }
                Err(e) => {
                    err_status.write().error = format!("desktop audio failed to start: {e}");
                }
            },
            Err(e) => {
                err_status.write().error = format!("desktop audio capture failed: {e}");
            }
        }
    });

    // The pump runs on its own clock rather than on arriving packets: a
    // silent endpoint delivers nothing at all through loopback, and a track
    // that stops being fed stalls rather than going quiet. Silence is sent
    // instead, so the stream stays alive through the gaps between tracks.
    let pump = tokio::spawn(async move {
        let mut staged: Vec<i16> = Vec::with_capacity(SHARE_FRAME_PER_CH * 4);
        let mut stereo: Vec<i16> = Vec::with_capacity(SHARE_FRAME_PER_CH * 4);
        let mut carry = (0.0f64, [0.0f32; 2]);
        let want = SHARE_FRAME_PER_CH * 2;
        let mut tick =
            tokio::time::interval(std::time::Duration::from_millis(10));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            while let Ok(chunk) = frame_rx.try_recv() {
                stereo.clear();
                to_stereo(&chunk, channels, &mut stereo);
                resample_stereo(&stereo, rate, &mut carry, &mut staged);
            }
            // Anything beyond a quarter second is latency nobody asked for.
            let cap = NS_RATE as usize / 2;
            if staged.len() > cap {
                let extra = staged.len() - cap;
                staged.drain(..extra);
            }
            let data: Vec<i16> = if staged.len() >= want {
                staged.drain(..want).collect()
            } else {
                vec![0i16; want]
            };
            let frame = AudioFrame {
                data: data.into(),
                sample_rate: NS_RATE,
                num_channels: 2,
                samples_per_channel: SHARE_FRAME_PER_CH as u32,
            };
            if source.capture_frame(&frame).await.is_err() {
                break;
            }
        }
    });

    status.write().sharing_audio = true;
    Ok((ShareAudio { _stop: stop_tx, pump }, publication.sid()))
}

/// Convert any cpal input sample slice to i16.
fn build_capture_stream(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    tx: tokio::sync::mpsc::UnboundedSender<Vec<i16>>,
    mut status: VoiceStatusSignal,
    // What to call this stream when it goes wrong. The same builder serves
    // the microphone and the desktop-audio capture, and "mic stream error"
    // on a screen share would send someone hunting the wrong device.
    label: &'static str,
) -> Result<cpal::Stream, cpal::Error> {
    let cfg = config.config();
    let err_cb = move |e: cpal::Error| {
        // Buffer under/overruns are transient on WASAPI and self-recover;
        // only surface errors that actually stop the stream.
        let text = e.to_string();
        if text.contains("underrun") || text.contains("overrun") {
            return;
        }
        status.write().error = format!("{label} error: {text}");
    };
    macro_rules! stream_as {
        ($ty:ty, $conv:expr) => {{
            let tx = tx.clone();
            device.build_input_stream(
                cfg.clone(),
                move |data: &[$ty], _: &_| {
                    let conv: fn(&$ty) -> i16 = $conv;
                    let _ = tx.send(data.iter().map(conv).collect());
                },
                err_cb,
                None,
            )
        }};
    }
    match config.sample_format() {
        cpal::SampleFormat::I16 => stream_as!(i16, |s| *s),
        cpal::SampleFormat::U16 => stream_as!(u16, |s| (*s as i32 - 32768) as i16),
        cpal::SampleFormat::I32 => stream_as!(i32, |s| (*s >> 16) as i16),
        cpal::SampleFormat::U8 => stream_as!(u8, |s| ((*s as i16 - 128) << 8) as i16),
        cpal::SampleFormat::F64 => stream_as!(f64, |s| (s.clamp(-1.0, 1.0) * 32767.0) as i16),
        _ => stream_as!(f32, |s| (s.clamp(-1.0, 1.0) * 32767.0) as i16),
    }
}

struct ActiveCall {
    room: Arc<Room>,
    mic_publication: livekit::publication::LocalTrackPublication,
    /// Dropping these ends the audio device threads.
    _mic_stop: std_mpsc::Sender<()>,
    /// One reader per remote track, feeding the mix. Aborted when the track
    /// goes away; dropping the call takes the lot.
    readers: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    /// The call's audio, and the single stream playing it.
    mixer: Arc<Mixer>,
    _mix_stop: std_mpsc::Sender<()>,
    /// Per-identity playback gain, read lock-free by audio callbacks.
    gains: Arc<Mutex<HashMap<String, Arc<AtomicU32>>>>,
    mic_gain: Arc<AtomicU32>,
    master_gain: Arc<AtomicU32>,
    ns_enabled: Arc<AtomicBool>,
    agc_enabled: Arc<AtomicBool>,
    deafened: Arc<AtomicBool>,
    /// True when voice mode is push-to-talk.
    ptt_mode: Arc<AtomicBool>,
    /// VAD gate threshold as f32 bits.
    vad_threshold: Arc<AtomicU32>,
    /// The configured PTT key, read by the polling thread each tick.
    ptt_key: Arc<Mutex<device_query::Keycode>>,
    /// Mute hotkey, read by the same polling thread.
    mute_hotkey_on: Arc<AtomicBool>,
    mute_hotkey_key: Arc<Mutex<device_query::Keycode>>,
    /// Dropping ends the PTT polling thread.
    _ptt_stop: std_mpsc::Sender<()>,
    /// Active screen capture + its published track sid.
    share: Option<(crate::share::ShareControl, livekit::id::TrackSid)>,
    /// Desktop audio riding along with that share, when it is switched on.
    share_audio: Option<(ShareAudio, livekit::id::TrackSid)>,
    /// Active webcam capture + its published track sid.
    camera: Option<(crate::camera::CameraHandle, livekit::id::TrackSid)>,
    /// Remote video tracks by participant identity, split by source.
    video_tracks: Arc<Mutex<VideoTracks>>,
    event_task: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct VideoTracks {
    screen: HashMap<String, RemoteVideoTrack>,
    camera: HashMap<String, RemoteVideoTrack>,
}

async fn stop_share(active: &mut ActiveCall, mut status: VoiceStatusSignal) {
    if let Some((audio, sid)) = active.share_audio.take() {
        // Drop first: the pump and the device thread go with it, so nothing
        // is still feeding a track that is about to stop existing.
        drop(audio);
        let _ = active.room.local_participant().unpublish_track(&sid).await;
        status.write().sharing_audio = false;
    }
    if let Some((control, sid)) = active.share.take() {
        let _ = control.stop();
        let _ = active.room.local_participant().unpublish_track(&sid).await;
    }
    crate::frames::unpublish("self:screen");
    let mut s = status.write();
    s.sharing_self = false;
    if let Some(me) = s.participants.iter_mut().find(|p| p.is_me) {
        me.sharing = false;
    }
}

async fn stop_camera(active: &mut ActiveCall, mut status: VoiceStatusSignal) {
    if let Some((handle, sid)) = active.camera.take() {
        handle.stop();
        let _ = active.room.local_participant().unpublish_track(&sid).await;
    }
    crate::frames::unpublish("self:camera");
    let mut s = status.write();
    s.camera_self = false;
    if let Some(me) = s.participants.iter_mut().find(|p| p.is_me) {
        me.camera = false;
    }
}

fn participant_name(status: VoiceStatusSignal, identity: &str) -> String {
    status
        .peek()
        .participants
        .iter()
        .find(|p| p.identity == identity)
        .map(|p| p.name.clone())
        .unwrap_or_else(|| identity.to_owned())
}

fn gain_handle(
    gains: &Arc<Mutex<HashMap<String, Arc<AtomicU32>>>>,
    identity: &str,
    initial: f32,
) -> Arc<AtomicU32> {
    gains
        .lock()
        .unwrap()
        .entry(identity.to_owned())
        .or_insert_with(|| Arc::new(AtomicU32::new(initial.to_bits())))
        .clone()
}

impl Drop for ActiveCall {
    fn drop(&mut self) {
        self.event_task.abort();
        for (_, reader) in self.readers.lock().unwrap().drain() {
            reader.abort();
        }
        self.mixer.clear();
    }
}


/// Avatar colour for an identity, matching the app's `hsl(hue, 55%, 42%)`.
fn avatar_color(identity: &str) -> u32 {
    let id: i64 = identity.strip_prefix("user-").and_then(|i| i.parse().ok()).unwrap_or(1);
    let hue = ((id * 137) % 360) as f64;
    let (s, l): (f64, f64) = (0.55, 0.42);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match hue as u32 / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let to8 = |v: f64| (((v + m) * 255.0).round() as u32).min(255);
    (to8(r) << 16) | (to8(g) << 8) | to8(b)
}

/// Rebuild the call window's tiles: one per person in the call, showing their
/// video when they have some and their avatar when they don't. Existing tiles
/// are reused so their frame pumps keep running.
fn rebuild_call_tiles(
    call_state: &crate::share::SharedCall,
    snapshot: &VoiceStatus,
    tracks: Option<&Arc<Mutex<VideoTracks>>>,
) {
    use crate::share::{Tile, TileKind};

    let tracks = tracks.map(|t| t.lock().unwrap());
    let mut view = call_state.lock().unwrap();
    let self_preview = view.self_preview.clone();
    let self_share = view.self_share.clone();
    let mut old: Vec<Tile> = std::mem::take(&mut view.tiles);
    let mut next: Vec<Tile> = Vec::new();

    let mut take_or_make = |label: String, identity: &str, make: &mut dyn FnMut() -> Option<TileKind>| {
        if let Some(pos) = old.iter().position(|t| t.label == label) {
            next.push(old.remove(pos));
            return;
        }
        if let Some(kind) = make() {
            next.push(Tile {
                identity: identity.to_owned(),
                label,
                kind,
                speaking: Arc::new(AtomicBool::new(false)),
                alive: Arc::new(AtomicBool::new(true)),
            });
        }
    };

    for participant in &snapshot.participants {
        let mut has_video = false;

        // Other people's streams come off the room; ours is the local preview.
        if let Some(tracks) = tracks.as_ref() {
            for (kind, map) in [("screen", &tracks.screen), ("camera", &tracks.camera)] {
                let Some(track) = map.get(&participant.identity) else { continue };
                has_video = true;
                let label = format!("{} · {kind}", participant.name);
                let track = track.clone();
                take_or_make(label, &participant.identity, &mut || {
                    let frame: crate::share::SharedFrame = Default::default();
                    let alive = Arc::new(AtomicBool::new(true));
                    crate::share::pump_track(&track, frame.clone(), alive);
                    Some(TileKind::Video(frame))
                });
            }
        }
        // Your own share is a copy of the capture, not a subscription.
        if participant.is_me && snapshot.sharing_self {
            if let Some((slot, _)) = self_share.clone() {
                has_video = true;
                if own_preview_hidden() {
                    // Asked not to watch it: a plain tile, and no touch, so
                    // the capturer stops copying frames for this window. A
                    // different label, so the video tile isn't reused.
                    let initial = participant
                        .name
                        .chars()
                        .next()
                        .map(|c| c.to_uppercase().to_string())
                        .unwrap_or_else(|| "?".into());
                    let color = avatar_color(&participant.identity);
                    take_or_make(
                        format!("{} · screen (preview off)", participant.name),
                        &participant.identity,
                        &mut || Some(TileKind::Avatar { initial: initial.clone(), color }),
                    );
                } else {
                    // Tells the capturer someone's looking, so it keeps teeing.
                    // Must go through frames so it's stamped on the same clock
                    // the capturer compares against.
                    crate::frames::touch("self:screen");
                    take_or_make(
                        format!("{} · screen", participant.name),
                        &participant.identity,
                        &mut || Some(TileKind::Video(slot.clone())),
                    );
                }
            }
        }
        if participant.is_me && snapshot.camera_self {
            if let Some(slot) = self_preview.clone() {
                has_video = true;
                take_or_make(
                    format!("{} · camera", participant.name),
                    &participant.identity,
                    &mut || Some(TileKind::Video(slot.clone())),
                );
            }
        }

        if !has_video {
            let initial = participant
                .name
                .chars()
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_else(|| "?".into());
            let color = avatar_color(&participant.identity);
            take_or_make(participant.name.clone(), &participant.identity, &mut || {
                Some(TileKind::Avatar { initial: initial.clone(), color })
            });
        }
    }

    // Whatever is left has gone away; clearing `alive` stops its frame pump.
    for gone in old {
        gone.alive.store(false, Ordering::Relaxed);
    }
    view.tiles = next;
}

pub async fn voice_task(
    mut rx: UnboundedReceiver<VoiceCmd>,
    mut status: VoiceStatusSignal,
    mut mic_level: MicLevelSignal,
) {
    let mut call: Option<ActiveCall> = None;
    OWN_PREVIEW_HIDDEN.store(crate::api::load_settings().hide_own_share_preview, Ordering::Relaxed);
    // Shared with the call window: what it draws, and what its buttons do.
    let call_state: crate::share::SharedCall = Default::default();
    let (action_tx, mut action_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::share::CallAction>();
    // The current call's video tracks, readable by the mirror task below.
    let live_tracks: Arc<Mutex<Option<Arc<Mutex<VideoTracks>>>>> = Default::default();

    // Keep the call window in step with the app: control states, who's
    // talking, and tiles whose track has ended.
    {
        let call_state = call_state.clone();
        let live_tracks = live_tracks.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                let snapshot = status.peek().clone();
                {
                    let Ok(mut view) = call_state.lock() else { continue };
                    view.muted = snapshot.muted;
                    view.deafened = snapshot.deafened;
                    view.camera_on = snapshot.camera_self;
                    view.sharing = snapshot.sharing_self;
                    view.preview_hidden = own_preview_hidden();
                }
                // Everyone in the call gets a tile — video if they have it,
                // avatar if they don't.
                let tracks = live_tracks.lock().ok().and_then(|t| t.clone());
                rebuild_call_tiles(&call_state, &snapshot, tracks.as_ref());
                if let Ok(view) = call_state.lock() {
                    for tile in &view.tiles {
                        let talking = snapshot
                            .participants
                            .iter()
                            .any(|p| p.identity == tile.identity && p.speaking);
                        tile.speaking.store(talking, Ordering::Relaxed);
                    }
                }
            }
        });
    }

    loop {
        // Commands arrive from the UI and from the call window's buttons.
        let cmd = tokio::select! {
            next = rx.next() => match next {
                Some(cmd) => cmd,
                None => break,
            },
            action = action_rx.recv() => match action {
                Some(action) => {
                    let snapshot = status.peek().clone();
                    match action {
                        crate::share::CallAction::Mic => VoiceCmd::ToggleMute,
                        crate::share::CallAction::Deafen => VoiceCmd::ToggleDeafen,
                        crate::share::CallAction::Camera => {
                            if snapshot.camera_self { VoiceCmd::StopCamera } else { VoiceCmd::StartCamera }
                        }
                        crate::share::CallAction::Screen => {
                            if snapshot.sharing_self {
                                VoiceCmd::StopScreenShare
                            } else {
                                VoiceCmd::StartScreenShare {
                                    target: crate::share::ShareTarget::PrimaryMonitor,
                                }
                            }
                        }
                        crate::share::CallAction::ShareEnded => VoiceCmd::StopScreenShare,
                        crate::share::CallAction::Preview => VoiceCmd::SetOwnPreviewHidden(!own_preview_hidden()),
                        crate::share::CallAction::Leave => VoiceCmd::Leave,
                    }
                }
                None => continue,
            },
        };
        match cmd {
            VoiceCmd::Join { channel_id, channel_name, url, token } => {
                // Leave any current room first.
                if let Some(mut old) = call.take() {
                    stop_share(&mut old, status).await;
                    stop_camera(&mut old, status).await;
                    old.room.close().await.ok();
                }
                // Carried across the attempt: a rejoin is a Join, and losing
                // the counter here would restart the backoff every time.
                let (was_dropped, attempts) = {
                    let now = status.peek();
                    (now.dropped, now.rejoin_attempts)
                };
                status.set(VoiceStatus {
                    channel_id: Some(channel_id),
                    channel_name: channel_name.clone(),
                    connecting: true,
                    rejoin_attempts: attempts,
                    ..Default::default()
                });
                {
                    let mut view = call_state.lock().unwrap();
                    view.title = channel_name.clone();
                    view.started_at = Some(std::time::Instant::now());
                    view.tiles.clear();
                    view.self_preview = None;
                    view.muted = false;
                    view.deafened = false;
                    view.camera_on = false;
                    view.sharing = false;
                }
                match connect(&url, &token, status, mic_level, action_tx.clone()).await {
                    Ok(active) => {
                        *live_tracks.lock().unwrap() = Some(active.video_tracks.clone());
                        call = Some(active);
                        status.write().connecting = false;
                        // You hear your own arrival too, like Discord.
                        play_voice_blip(true);
                    }
                    Err(e) => {
                        // If this was an attempt to get back into a dropped
                        // call, stay dropped so the backoff keeps going —
                        // clearing it here would give up after one try.
                        status.set(VoiceStatus {
                            channel_id: was_dropped.then_some(channel_id),
                            channel_name: if was_dropped { channel_name.clone() } else { String::new() },
                            dropped: was_dropped,
                            rejoin_attempts: attempts,
                            error: format!("voice connect failed: {e}"),
                            ..Default::default()
                        });
                    }
                }
            }
            VoiceCmd::Leave => {
                if let Some(mut old) = call.take() {
                    stop_share(&mut old, status).await;
                    stop_camera(&mut old, status).await;
                    old.room.close().await.ok();
                    play_voice_blip(false);
                }
                *live_tracks.lock().unwrap() = None;
                // Nothing left to show in the Video tab.
                crate::frames::clear();
                {
                    let mut view = call_state.lock().unwrap();
                    for tile in &view.tiles {
                        tile.alive.store(false, Ordering::Relaxed);
                    }
                    view.tiles.clear();
                    view.self_preview = None;
                    view.open = false;
                    view.started_at = None;
                }
                status.set(VoiceStatus::default());
                mic_level.set(0.0);
            }
            VoiceCmd::StartScreenShare { target } => {
                if let Some(active) = call.as_mut() {
                    if active.share.is_none() {
                        // Declare the real size: the encoder's whole plan is
                        // computed from this, not from the frames, and it used
                        // to say 1920x1080 whatever the monitor actually was.
                        // Capped at 1440p on the way, deliberately: encoding
                        // 4K text in software is heavy enough to cost frames,
                        // and 1440p is already far more readable than the
                        // 1080p everyone was silently getting.
                        let quality = crate::api::load_settings();
                        let fps = quality.share_fps.clamp(1, 120);
                        let (cap_w, cap_h) = share_encode_size(
                            crate::share::target_size(&target).unwrap_or((1920, 1080)),
                            quality.share_max_height,
                        );
                        let source = NativeVideoSource::new(
                            VideoResolution { width: cap_w, height: cap_h },
                            true,
                        );
                        let track = LocalVideoTrack::create_video_track(
                            "screen",
                            RtcVideoSource::Native(source.clone()),
                        );
                        match active
                            .room
                            .local_participant()
                            .publish_track(
                                LocalTrack::Video(track),
                                TrackPublishOptions {
                                    source: TrackSource::Screenshare,
                                    // The library's own screenshare presets
                                    // stop at 3 Mbps for 1080p30, which is
                                    // thin for a screen full of code: text is
                                    // the hardest thing a video codec does.
                                    // 15fps of readable text beats 30 of
                                    // mush for a shared editor, which is why
                                    // that is still the default — but a game
                                    // wants the opposite trade, so both the
                                    // rate and what congestion takes first
                                    // are the sharer's to choose now.
                                    video_encoding: Some(VideoEncoding {
                                        max_bitrate: screen_share_bitrate(cap_w, cap_h, fps),
                                        max_framerate: f64::from(fps),
                                    }),
                                    degradation_preference: Some(share_degradation(fps)),
                                    ..Default::default()
                                },
                            )
                            .await
                        {
                            Ok(publication) => {
                                // You never subscribe to your own track, so a
                                // copy of the capture is the only way you get
                                // to see your own share.
                                let slot: crate::share::SharedFrame = Default::default();
                                let interest = crate::frames::publish_shared(
                                    "self:screen".into(),
                                    slot.clone(),
                                );
                                call_state.lock().unwrap().self_share =
                                    Some((slot.clone(), interest.clone()));
                                let preview = Some(crate::share::SelfShare { slot, interest });
                                let closed = Arc::new(AtomicBool::new(false));
                                // Watch for the capture ending on its own (the
                                // shared window was closed): unpublish instead
                                // of streaming a frozen last frame. The watcher
                                // exits quietly once the capture is dropped.
                                {
                                    let closed = closed.clone();
                                    let ended_tx = action_tx.clone();
                                    tokio::spawn(async move {
                                        loop {
                                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                                            if closed.load(Ordering::Relaxed) {
                                                let _ = ended_tx.send(crate::share::CallAction::ShareEnded);
                                                break;
                                            }
                                            if Arc::strong_count(&closed) == 1 {
                                                break;
                                            }
                                        }
                                    });
                                }
                                match crate::share::start_capture(source, target, preview, closed, fps) {
                                Ok(control) => {
                                    active.share = Some((control, publication.sid()));
                                    {
                                        let mut s = status.write();
                                        s.sharing_self = true;
                                        if let Some(me) = s.participants.iter_mut().find(|p| p.is_me) {
                                            me.sharing = true;
                                        }
                                    }
                                    // The picture is already going out; audio
                                    // is a bonus, so a failure here says so
                                    // and leaves the share running.
                                    let settings = crate::api::load_settings();
                                    if settings.share_audio {
                                        match publish_share_audio(
                                            &active.room,
                                            settings.share_audio_device.clone(),
                                            settings.share_audio_bitrate,
                                            status,
                                        )
                                        .await
                                        {
                                            Ok(pair) => active.share_audio = Some(pair),
                                            Err(e) => {
                                                status.write().error =
                                                    format!("desktop audio: {e}")
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    let sid = publication.sid();
                                    let _ = active.room.local_participant().unpublish_track(&sid).await;
                                    crate::frames::unpublish("self:screen");
                                    call_state.lock().unwrap().self_share = None;
                                    status.write().error = e;
                                }
                                }
                            }
                            Err(e) => status.write().error = format!("screen share failed: {e}"),
                        }
                    }
                }
            }
            VoiceCmd::StopScreenShare => {
                if let Some(active) = call.as_mut() {
                    stop_share(active, status).await;
                }
                call_state.lock().unwrap().self_share = None;
            }
            VoiceCmd::WatchScreen { identity } => {
                if let Some(active) = &call {
                    let track = active.video_tracks.lock().unwrap().screen.get(&identity).cloned();
                    match track {
                        Some(_) => {
                            if let Err(e) =
                                crate::share::open_call_window(call_state.clone(), action_tx.clone())
                            {
                                status.write().error = e;
                            }
                        }
                        None => status.write().error = "that screen share is no longer available".into(),
                    }
                }
            }
            VoiceCmd::StartCamera => {
                if let Some(active) = call.as_mut() {
                    if active.camera.is_none() {
                        let source = NativeVideoSource::new(
                            VideoResolution { width: 1280, height: 720 },
                            false,
                        );
                        let track = LocalVideoTrack::create_video_track(
                            "camera",
                            RtcVideoSource::Native(source.clone()),
                        );
                        match active
                            .room
                            .local_participant()
                            .publish_track(
                                LocalTrack::Video(track),
                                TrackPublishOptions {
                                    source: TrackSource::Camera,
                                    ..Default::default()
                                },
                            )
                            .await
                        {
                            Ok(publication) => {
                                // Show yourself what you're broadcasting: the
                                // same preview slot feeds your Video-tab tile
                                // and (if someone pops it open via Watch) the
                                // call window PiP. The window no longer opens
                                // itself — the Video tab is where video lives.
                                let slot: crate::share::SharedFrame = Default::default();
                                let alive = Arc::new(AtomicBool::new(true));
                                call_state.lock().unwrap().self_preview = Some(slot.clone());
                                crate::frames::publish_shared("self:camera".into(), slot.clone());
                                let preview = Some((slot, alive));
                                // Opening the webcam can take seconds; don't
                                // stall the voice command loop while it does.
                                let opened = tokio::task::spawn_blocking(move || {
                                    crate::camera::start_camera(source, preview)
                                })
                                .await
                                .unwrap_or_else(|e| Err(format!("camera thread panicked: {e}")));
                                match opened {
                                    Ok(handle) => {
                                        active.camera = Some((handle, publication.sid()));
                                        let mut s = status.write();
                                        s.camera_self = true;
                                        if let Some(me) = s.participants.iter_mut().find(|p| p.is_me) {
                                            me.camera = true;
                                        }
                                    }
                                    Err(e) => {
                                        let sid = publication.sid();
                                        let _ = active.room.local_participant().unpublish_track(&sid).await;
                                        status.write().error = e;
                                    }
                                }
                            }
                            Err(e) => status.write().error = format!("camera failed: {e}"),
                        }
                    }
                }
            }
            VoiceCmd::StopCamera => {
                if let Some(active) = call.as_mut() {
                    stop_camera(active, status).await;
                }
            }
            VoiceCmd::WatchCamera { identity } => {
                if let Some(active) = &call {
                    let track = active.video_tracks.lock().unwrap().camera.get(&identity).cloned();
                    match track {
                        Some(_) => {
                            if let Err(e) =
                                crate::share::open_call_window(call_state.clone(), action_tx.clone())
                            {
                                status.write().error = e;
                            }
                        }
                        None => status.write().error = "that camera is no longer available".into(),
                    }
                }
            }
            VoiceCmd::ToggleMute => {
                if let Some(active) = &call {
                    let muted = !status.peek().muted;
                    if muted {
                        active.mic_publication.mute();
                    } else {
                        active.mic_publication.unmute();
                    }
                    status.write().muted = muted;
                }
            }
            VoiceCmd::ToggleDeafen => {
                if let Some(active) = &call {
                    let deafened = !status.peek().deafened;
                    active.deafened.store(deafened, Ordering::Relaxed);
                    // Deafen implies mute; undeafen restores both.
                    if deafened {
                        active.mic_publication.mute();
                    } else {
                        active.mic_publication.unmute();
                    }
                    let mut s = status.write();
                    s.deafened = deafened;
                    s.muted = deafened;
                }
            }
            VoiceCmd::SetVoiceMode { mode, key } => {
                if let Some(active) = &call {
                    active.ptt_mode.store(mode == "ptt", Ordering::Relaxed);
                    *active.ptt_key.lock().unwrap() = parse_ptt_key(&key);
                }
                let mut settings = crate::api::load_settings();
                settings.voice_mode = mode;
                settings.ptt_key = key;
                crate::api::save_settings(&settings);
            }
            VoiceCmd::SetMuteHotkey { enabled, key } => {
                if let Some(active) = &call {
                    active.mute_hotkey_on.store(enabled, Ordering::Relaxed);
                    *active.mute_hotkey_key.lock().unwrap() = parse_ptt_key(&key);
                }
                let mut settings = crate::api::load_settings();
                settings.mute_hotkey = enabled;
                settings.mute_key = key;
                crate::api::save_settings(&settings);
            }
            VoiceCmd::SetVadThreshold(threshold) => {
                let threshold = threshold.clamp(0.0, 32768.0);
                if let Some(active) = &call {
                    active.vad_threshold.store(threshold.to_bits(), Ordering::Relaxed);
                }
                let mut settings = crate::api::load_settings();
                settings.vad_threshold = threshold;
                crate::api::save_settings(&settings);
            }
            VoiceCmd::SetNoiseSuppression(enabled) => {
                if let Some(active) = &call {
                    active.ns_enabled.store(enabled, Ordering::Relaxed);
                }
                let mut settings = crate::api::load_settings();
                settings.noise_suppression = enabled;
                crate::api::save_settings(&settings);
            }
            VoiceCmd::SetAutoGain(enabled) => {
                if let Some(active) = &call {
                    active.agc_enabled.store(enabled, Ordering::Relaxed);
                }
                let mut settings = crate::api::load_settings();
                settings.auto_gain = enabled;
                crate::api::save_settings(&settings);
            }
            VoiceCmd::SetOwnPreviewHidden(hidden) => {
                // Takes effect by itself: both viewers read the flag, and once
                // neither asks, the capturer's interest stamp goes stale and
                // it stops teeing — no capture restart, nothing sent to anyone.
                OWN_PREVIEW_HIDDEN.store(hidden, Ordering::Relaxed);
                let mut settings = crate::api::load_settings();
                settings.hide_own_share_preview = hidden;
                crate::api::save_settings(&settings);
            }
            VoiceCmd::SetMicVolume(volume) => {
                let volume = volume.clamp(0.0, 2.0);
                if let Some(active) = &call {
                    active.mic_gain.store(volume.to_bits(), Ordering::Relaxed);
                }
                let mut settings = crate::api::load_settings();
                settings.input_volume = volume;
                crate::api::save_settings(&settings);
            }
            VoiceCmd::SetMasterVolume(volume) => {
                let volume = volume.clamp(0.0, 2.0);
                if let Some(active) = &call {
                    active.master_gain.store(volume.to_bits(), Ordering::Relaxed);
                }
                let mut settings = crate::api::load_settings();
                settings.output_volume = volume;
                crate::api::save_settings(&settings);
            }
            VoiceCmd::SetVolume { identity, volume } => {
                let volume = volume.clamp(0.0, 2.0);
                if let Some(active) = &call {
                    gain_handle(&active.gains, &identity, volume).store(volume.to_bits(), Ordering::Relaxed);
                }
                status.write().volumes.insert(identity.clone(), volume);
                let mut settings = crate::api::load_settings();
                settings.volumes.insert(identity, volume);
                crate::api::save_settings(&settings);
            }
        }
    }
}

async fn connect(
    url: &str,
    token: &str,
    status: VoiceStatusSignal,
    mut mic_level: MicLevelSignal,
    // The mute hotkey presses the same button the UI does, rather than
    // reaching into the mic itself: one mute, one code path, no chance of
    // the icon and the microphone disagreeing.
    mute_hotkey_tx: tokio::sync::mpsc::UnboundedSender<crate::share::CallAction>,
) -> anyhow::Result<ActiveCall> {
    let (room, events) = Room::connect(url, token, livekit::RoomOptions::default()).await?;
    let room = Arc::new(room);

    // ---- Microphone capture ----
    let settings = crate::api::load_settings();
    // Device + config acquisition happens on an MTA thread: for the default
    // device this activates WASAPI through a COM call that Windows rejects
    // from the STA mode cpal would otherwise pin this pooled thread to.
    let preferred_mic = settings.input_device.clone();
    let (mic, mic_name, mic_config) = on_mta_thread(move || {
        let host = cpal::default_host();
        let mic = pick_input_device(&host, &preferred_mic)
            .ok_or_else(|| anyhow::anyhow!("no microphone found"))?;
        let mic_name = device_name(&mic).unwrap_or_else(|| "unknown".into());
        let mic_config = mic
            .default_input_config()
            .map_err(|e| anyhow::anyhow!("cannot open mic '{mic_name}': {e}"))?;
        Ok::<_, anyhow::Error>((mic, mic_name, mic_config))
    })?;
    let sample_rate: u32 = mic_config.sample_rate();
    let channels = mic_config.channels() as u32;

    // The published track is always 48kHz mono: the capture pump downmixes,
    // resamples, and (optionally) denoises to match.
    let source = NativeAudioSource::new(
        AudioSourceOptions {
            echo_cancellation: true,
            noise_suppression: true,
            auto_gain_control: true,
        },
        NS_RATE,
        1,
        1000,
    );

    let track = LocalAudioTrack::create_audio_track("mic", RtcAudioSource::Native(source.clone()));
    let mic_publication = room
        .local_participant()
        .publish_track(
            LocalTrack::Audio(track),
            TrackPublishOptions { source: TrackSource::Microphone, ..Default::default() },
        )
        .await?;

    // Device thread: cpal callback -> unbounded channel of i16 buffers.
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<i16>>();
    let (mic_stop_tx, mic_stop_rx) = std_mpsc::channel::<()>();
    let mic_status = status;
    std::thread::spawn(move || {
        com_init_mta();
        match build_capture_stream(&mic, &mic_config, frame_tx, mic_status, "mic stream") {
            Ok(stream) => match stream.play() {
                Ok(()) => {
                    // Block until the ActiveCall drops the sender.
                    let _ = mic_stop_rx.recv();
                }
                Err(e) => {
                    let mut s = mic_status;
                    s.write().error = format!("mic '{mic_name}' failed to start: {e}");
                }
            },
            Err(e) => {
                let mut s = mic_status;
                s.write().error =
                    format!("mic '{mic_name}' failed to open: {e} — try another input device in the audio settings");
            }
        }
    });

    // Pump: device chunks -> mono 48k -> gain -> RNNoise -> LiveKit frames.
    // Also drives the local speaking indicator and mic meter (both computed
    // after denoise, so background noise doesn't light the ring).
    let mic_gain = Arc::new(AtomicU32::new(settings.input_volume.clamp(0.0, 2.0).to_bits()));
    let master_gain = Arc::new(AtomicU32::new(settings.output_volume.clamp(0.0, 2.0).to_bits()));
    let ns_enabled = Arc::new(AtomicBool::new(settings.noise_suppression));
    let agc_enabled = Arc::new(AtomicBool::new(settings.auto_gain));
    let deafened = Arc::new(AtomicBool::new(false));
    // One canceller per call, shared by the output (which tells it what was
    // played) and the microphone pump (which has the echo taken out of it).
    let echo = settings.echo_cancellation.then(Canceller::new);
    let pump_echo = echo.clone();
    // One stream for the call, opened below and kept until it ends:
    // everything anyone says is summed into it. Built here so the microphone
    // pump can ask it whether the far end is talking.
    let mixer = Mixer::new(master_gain.clone(), deafened.clone(), echo);
    let pump_far_end = mixer.far_end();
    let ptt_mode = Arc::new(AtomicBool::new(settings.voice_mode == "ptt"));
    let ptt_key = Arc::new(Mutex::new(parse_ptt_key(&settings.ptt_key)));
    let ptt_active = Arc::new(AtomicBool::new(false));
    let mute_hotkey_on = Arc::new(AtomicBool::new(settings.mute_hotkey));
    let mute_hotkey_key = Arc::new(Mutex::new(parse_ptt_key(&settings.mute_key)));
    let vad_threshold = Arc::new(AtomicU32::new(settings.vad_threshold.clamp(0.0, 32768.0).to_bits()));

    // PTT key poller: 30ms ticks, no global hotkey registration, so the key
    // keeps working in other apps and is never swallowed system-wide.
    // The mute hotkey rides the same tick — one read of the keyboard covers
    // both keys, and they can never disagree about what is down.
    let (ptt_stop_tx, ptt_stop_rx) = std_mpsc::channel::<()>();
    {
        let key = ptt_key.clone();
        let active = ptt_active.clone();
        let mode = ptt_mode.clone();
        let mute_key = mute_hotkey_key.clone();
        let mute_on = mute_hotkey_on.clone();
        let mut ptt_status = status;
        std::thread::spawn(move || {
            use device_query::DeviceQuery;
            let device = device_query::DeviceState::new();
            // The mute key toggles, so only the moment it goes down counts.
            // Without this the 30ms tick would read a normal keypress as
            // some dozen presses and land wherever it happened to stop.
            let mut mute_was_down = false;
            loop {
                match ptt_stop_rx.recv_timeout(std::time::Duration::from_millis(30)) {
                    Err(std_mpsc::RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
                let ptt = mode.load(Ordering::Relaxed);
                let watch_mute = mute_on.load(Ordering::Relaxed);
                if !ptt && !watch_mute {
                    // Nothing to watch — don't wake the keyboard at all.
                    if active.swap(false, Ordering::Relaxed) {
                        ptt_status.write().ptt_held = false;
                    }
                    mute_was_down = false;
                    continue;
                }
                let keys = device.get_keys();
                let held = ptt && keys.contains(&*key.lock().unwrap());
                if active.swap(held, Ordering::Relaxed) != held {
                    ptt_status.write().ptt_held = held;
                    // The edge, which is exactly when the microphone opens
                    // and closes — so the tick cannot drift out of step with
                    // what is actually being transmitted.
                    play_ptt_tick(held);
                }

                let mute_down = watch_mute && keys.contains(&*mute_key.lock().unwrap());
                if pressed_now(mute_down, &mut mute_was_down) {
                    // Same message the mic button sends.
                    let _ = mute_hotkey_tx.send(crate::share::CallAction::Mic);
                }
            }
        });
    }

    let mut pump_status = status;
    let pump_gain = mic_gain.clone();
    let pump_ns = ns_enabled.clone();
    let pump_agc = agc_enabled.clone();
    let pump_ptt_mode = ptt_mode.clone();
    let pump_ptt_active = ptt_active.clone();
    let pump_vad = vad_threshold.clone();
    tokio::spawn(async move {
        const METER_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
        let mut denoise = nnnoiseless::DenoiseState::new();
        let mut denoised = [0.0f32; NS_FRAME];
        let mut mono48: Vec<f32> = Vec::with_capacity(NS_FRAME * 8);
        let mut resample_pos: f64 = 0.0;
        let mut last_sample: f32 = 0.0;
        let step = sample_rate as f64 / NS_RATE as f64;
        let mut gate = VadGate::new();
        let mut agc = AutoGain::new();
        let mut last_meter = std::time::Instant::now() - METER_INTERVAL;
        let mut meter_peak: f64 = 0.0;

        while let Some(chunk) = frame_rx.recv().await {
            // Downmix interleaved device channels to mono f32 (i16 scale).
            let mono = downmix_strongest(&chunk, channels.max(1) as usize);

            // Resample device rate -> 48kHz (linear; identity when already 48k).
            if sample_rate == NS_RATE {
                mono48.extend(mono);
            } else {
                let src: Vec<f32> = std::iter::once(last_sample).chain(mono.iter().copied()).collect();
                let mut idx = resample_pos;
                while idx + 1.0 < src.len() as f64 {
                    let i = idx as usize;
                    let frac = (idx - i as f64) as f32;
                    mono48.push(src[i] * (1.0 - frac) + src[i + 1] * frac);
                    idx += step;
                }
                resample_pos = idx - (src.len() as f64 - 1.0);
                last_sample = *src.last().unwrap_or(&0.0);
            }

            while mono48.len() >= NS_FRAME {
                let mut frame: Vec<f32> = mono48.drain(..NS_FRAME).collect();

                // Echo first, on the rawest signal there is. Everything below
                // changes the microphone non-linearly — the gain knob, the
                // auto gain, RNNoise — and a canceller works by comparing the
                // microphone against what was played, so anything that alters
                // one and not the other makes the echo harder to recognise.
                //
                // Every frame goes through, including ones the gate is about
                // to throw away: the canceller tracks the room continuously,
                // and feeding it only the frames where somebody spoke would
                // leave it permanently misaligned against the reverse stream.
                if let Some(echo) = &pump_echo {
                    let mut pcm: Vec<i16> =
                        frame.iter().map(|s| s.clamp(-32768.0, 32767.0) as i16).collect();
                    echo.heard(&mut pcm);
                    for (dst, src) in frame.iter_mut().zip(pcm) {
                        *dst = src as f32;
                    }
                }

                let gain = f32::from_bits(pump_gain.load(Ordering::Relaxed));
                if (gain - 1.0).abs() > f32::EPSILON {
                    for s in frame.iter_mut() {
                        *s = (*s * gain).clamp(-32768.0, 32767.0);
                    }
                }

                // Auto gain runs after the manual knob and before denoise, so
                // a quiet laptop mic reaches RNNoise (and the gate, and the
                // wire) at normal speech level.
                if pump_agc.load(Ordering::Relaxed) {
                    let raw_rms = (frame.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>()
                        / frame.len() as f64)
                        .sqrt();
                    let boost = agc.feed_against(raw_rms, far_end_audible(&pump_far_end));
                    if boost > 1.0 {
                        for s in frame.iter_mut() {
                            *s = (*s * boost).clamp(-32768.0, 32767.0);
                        }
                    }
                }

                if pump_ns.load(Ordering::Relaxed) {
                    denoise.process_frame(&mut denoised, &frame);
                    frame.copy_from_slice(&denoised);
                }

                let rms = (frame.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>()
                    / frame.len() as f64)
                    .sqrt();
                let now = std::time::Instant::now();
                // The meter reports the PEAK since the last tick, in raw RMS
                // (i16 scale). Sampling one 10ms frame in ten made the bar
                // miss the very peaks people calibrate the threshold against.
                meter_peak = meter_peak.max(rms);
                if now.duration_since(last_meter) >= METER_INTERVAL {
                    last_meter = now;
                    mic_level.set(meter_peak as f32);
                    meter_peak = 0.0;
                }
                let gate_threshold = f32::from_bits(pump_vad.load(Ordering::Relaxed)) as f64;
                // The gate only decides what gets TRANSMITTED. The speaking
                // ring (yours included) comes from LiveKit's ActiveSpeakers,
                // judged from the audio that actually goes out — so the ring
                // can never disagree with what people hear.
                let (vad_open, _voice_recent) = gate.feed(rms, gate_threshold, now);
                let muted = pump_status.peek().muted;
                let gate_open = if pump_ptt_mode.load(Ordering::Relaxed) {
                    pump_ptt_active.load(Ordering::Relaxed)
                } else {
                    vad_open
                };
                let transmitting = !muted && gate_open;

                // In PTT mode with the key up, send nothing at all.
                if !transmitting {
                    continue;
                }
                let data: Vec<i16> = frame.iter().map(|s| s.clamp(-32768.0, 32767.0) as i16).collect();
                let audio_frame = AudioFrame {
                    data: data.into(),
                    sample_rate: NS_RATE,
                    num_channels: 1,
                    samples_per_channel: NS_FRAME as u32,
                };
                if source.capture_frame(&audio_frame).await.is_err() {
                    return;
                }
            }
        }
    });

    // ---- Room events: participants, remote audio playback ----
    {
        let mut s = status;
        s.write().volumes = settings.volumes.clone();
    }
    let readers: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mix_stop = start_mix_output(mixer.clone(), settings.output_device.clone(), status);
    let video_tracks: Arc<Mutex<VideoTracks>> = Arc::new(Mutex::new(VideoTracks::default()));
    let tracks_for_events = video_tracks.clone();
    let gains: Arc<Mutex<HashMap<String, Arc<AtomicU32>>>> = Arc::new(Mutex::new(HashMap::new()));
    let readers_for_events = readers.clone();
    let mixer_for_events = mixer.clone();
    let gains_for_events = gains.clone();
    let saved_volumes = settings.volumes.clone();
    let room_handle = room.clone();
    let event_task = tokio::spawn(async move {
        let mut events = events;
        refresh_participants(&room_handle, status, &tracks_for_events);
        while let Some(event) = events.recv().await {
            match event {
                RoomEvent::TrackSubscribed { track, publication, participant } => {
                    match track {
                        RemoteTrack::Audio(audio) => {
                            let sid = audio.sid().to_string();
                            let identity = participant.identity().to_string();
                            let initial = *saved_volumes.get(&identity).unwrap_or(&1.0);
                            let gain = gain_handle(&gains_for_events, &identity, initial);
                            let reader =
                                spawn_reader(audio, sid.clone(), mixer_for_events.clone(), gain);
                            readers_for_events.lock().unwrap().insert(sid, reader);
                        }
                        RemoteTrack::Video(video) => {
                            let identity = participant.identity().to_string();
                            let mut tracks = tracks_for_events.lock().unwrap();
                            match publication.source() {
                                TrackSource::Screenshare => {
                                    // Also feed the in-app Video tab. It only
                                    // costs anything while someone's looking.
                                    crate::frames::publish_track(
                                        format!("{identity}:screen"),
                                        &video,
                                    );
                                    tracks.screen.insert(identity, video);
                                }
                                TrackSource::Camera => {
                                    crate::frames::publish_track(
                                        format!("{identity}:camera"),
                                        &video,
                                    );
                                    tracks.camera.insert(identity, video);
                                }
                                _ => {}
                            }
                        }
                    }
                    refresh_participants(&room_handle, status, &tracks_for_events);
                }
                RoomEvent::TrackUnsubscribed { track, publication, participant } => {
                    match track {
                        RemoteTrack::Audio(audio) => {
                            let sid = audio.sid().to_string();
                            if let Some(reader) = readers_for_events.lock().unwrap().remove(&sid) {
                                reader.abort();
                            }
                            mixer_for_events.remove(&sid);
                        }
                        RemoteTrack::Video(_) => {
                            let identity = participant.identity().to_string();
                            let mut tracks = tracks_for_events.lock().unwrap();
                            match publication.source() {
                                TrackSource::Screenshare => {
                                    crate::frames::unpublish(&format!("{identity}:screen"));
                                    tracks.screen.remove(&identity);
                                }
                                TrackSource::Camera => {
                                    crate::frames::unpublish(&format!("{identity}:camera"));
                                    tracks.camera.remove(&identity);
                                }
                                _ => {}
                            }
                        }
                    }
                    refresh_participants(&room_handle, status, &tracks_for_events);
                }
                RoomEvent::ParticipantConnected(_) => {
                    play_voice_blip(true);
                    refresh_participants(&room_handle, status, &tracks_for_events);
                }
                RoomEvent::ParticipantDisconnected(_) => {
                    play_voice_blip(false);
                    refresh_participants(&room_handle, status, &tracks_for_events);
                }
                RoomEvent::Connected { .. } => refresh_participants(&room_handle, status, &tracks_for_events),
                RoomEvent::ActiveSpeakersChanged { speakers } => {
                    let speaking: Vec<String> =
                        speakers.iter().map(|p| p.identity().to_string()).collect();
                    let mut s = status;
                    let mut st = s.write();
                    // EVERY ring — including your own — follows LiveKit's
                    // read of the audio actually on the wire. The local
                    // mic-gate heuristic used to drive the self ring and
                    // kept disagreeing with what people really heard; the
                    // web client always did it this way and was right.
                    for p in st.participants.iter_mut() {
                        p.speaking = speaking.contains(&p.identity);
                    }
                }
                RoomEvent::Disconnected { .. } => {
                    // Keep the channel: wiping it made a dropped call
                    // indistinguishable from hanging up, and left the person
                    // sitting in silence with nothing to rejoin (Jon).
                    let mut s = status;
                    let was = s.peek().clone();
                    s.set(VoiceStatus {
                        channel_id: was.channel_id,
                        channel_name: was.channel_name.clone(),
                        muted: was.muted,
                        deafened: was.deafened,
                        volumes: was.volumes.clone(),
                        dropped: true,
                        rejoin_attempts: was.rejoin_attempts,
                        error: "connection lost \u{2014} reconnecting\u{2026}".into(),
                        ..Default::default()
                    });
                    break;
                }
                _ => {}
            }
        }
    });

    Ok(ActiveCall {
        room,
        mic_publication,
        _mic_stop: mic_stop_tx,
        readers,
        mixer,
        _mix_stop: mix_stop,
        gains,
        mic_gain,
        master_gain,
        ns_enabled,
        agc_enabled,
        deafened,
        ptt_mode,
        vad_threshold,
        ptt_key,
        mute_hotkey_on,
        mute_hotkey_key,
        _ptt_stop: ptt_stop_tx,
        share: None,
        share_audio: None,
        camera: None,
        video_tracks,
        event_task,
    })
}

/// Short two-tone blip: rising for a join, falling for a leave. Played on the
/// output device picked in voice settings (PlaySound only knew the Windows
/// default device, which made blips inaudible for anyone routing voice to
/// headphones that aren't the system default).
const CUE_RATE: u32 = 48000;

fn blip_samples(join: bool) -> Vec<f32> {
    let (f1, f2) = if join { (440.0, 587.33) } else { (587.33, 392.0) };
    let mut samples: Vec<f32> = Vec::new();
    for (freq, ms) in [(f1, 70u32), (f2, 90u32)] {
        let n = CUE_RATE * ms / 1000;
        for i in 0..n {
            let t = i as f32 / CUE_RATE as f32;
            let env = (1.0 - i as f32 / n as f32).powf(1.4);
            samples.push((t * freq * std::f32::consts::TAU).sin() * env * 0.3);
        }
    }
    samples
}

/// A short tick for the push-to-talk key: up in pitch when the microphone
/// opens, down when it closes.
///
/// Deliberately smaller than the join/leave blip — one note, 45ms, quieter.
/// This one can fire many times a minute, and anything with presence would
/// become infuriating by the third sentence. It is also kept brief because
/// on speakers the open tick lands while the microphone is already live, so
/// whatever it is, the room hears a little of it.
fn ptt_tick_samples(open: bool) -> Vec<f32> {
    let freq = if open { 1046.5 } else { 659.25 };
    let n = CUE_RATE * 45 / 1000;
    (0..n)
        .map(|i| {
            let t = i as f32 / CUE_RATE as f32;
            // Fade both ends: a tick that starts or stops abruptly clicks.
            let p = i as f32 / n as f32;
            let env = (p * std::f32::consts::PI).sin().powf(0.8);
            (t * freq * std::f32::consts::TAU).sin() * env * 0.12
        })
        .collect()
}

fn play_ptt_tick(open: bool) {
    if !crate::api::load_settings().ptt_sounds {
        return;
    }
    play_samples_on_voice_output(ptt_tick_samples(open), CUE_RATE);
}

fn play_voice_blip(join: bool) {
    if !crate::api::load_settings().voice_join_sounds {
        crate::api::debug_log("blip skipped: voice_join_sounds is off");
        return;
    }
    crate::api::debug_log(if join { "blip: join" } else { "blip: leave" });
    play_samples_on_voice_output(blip_samples(join), CUE_RATE);
}

/// Settings → Voice "Test" button: play the join cue on the selected output
/// device, so it's obvious where NotDiscord's audio actually goes.
pub fn play_test_cue() {
    crate::api::debug_log("blip: test button");
    play_samples_on_voice_output(blip_samples(true), CUE_RATE);
}

/// Name of the Windows default output device (what everything else uses).
pub fn default_output_name() -> Option<String> {
    cpal::default_host().default_output_device().as_ref().and_then(device_name)
}

/// Fire-and-forget playback of mono samples on the configured voice output
/// device (default device when none is picked). Each call runs on its own
/// short-lived thread; errors are swallowed — a missing device shouldn't
/// break anything, the cue just doesn't play.
pub fn play_samples_on_voice_output(samples: Vec<f32>, rate: u32) {
    std::thread::spawn(move || {
        use cpal::traits::{DeviceTrait, StreamTrait};
        com_init_mta();
        let host = cpal::default_host();
        let preferred = crate::api::load_settings().output_device;
        let Some(device) = pick_output_device(&host, &preferred) else {
            crate::api::debug_log("cue: no output device found");
            return;
        };
        crate::api::debug_log(&format!("cue device: {:?}", device_name(&device)));
        let config = cpal::StreamConfig {
            channels: 2,
            sample_rate: rate,
            buffer_size: cpal::BufferSize::Default,
        };
        let total = samples.len();
        let mut pos = 0usize;
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let stream = device.build_output_stream(
            config,
            move |out: &mut [f32], _: &_| {
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
            |_| {},
            None,
        );
        match stream {
            Ok(stream) => match stream.play() {
                Ok(()) => {
                    // Wait until the callback has drained the samples (or bail
                    // after 2s if the device stalls).
                    let drained = done_rx.recv_timeout(std::time::Duration::from_secs(2));
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    crate::api::debug_log(&format!("cue played (drained: {})", drained.is_ok()));
                }
                Err(e) => crate::api::debug_log(&format!("cue play failed: {e}")),
            },
            Err(e) => crate::api::debug_log(&format!("cue stream failed: {e}")),
        }
    });
}

fn refresh_participants(
    room: &Room,
    status: VoiceStatusSignal,
    video_tracks: &Mutex<VideoTracks>,
) {
    let mut s = status;
    let (sharing_ids, camera_ids): (std::collections::HashSet<String>, std::collections::HashSet<String>) = {
        let tracks = video_tracks.lock().unwrap();
        (
            tracks.screen.keys().cloned().collect(),
            tracks.camera.keys().cloned().collect(),
        )
    };
    // Preserve current speaking flags so a roster refresh doesn't blink them off.
    let (previous, self_sharing, self_camera) = {
        let st = s.peek();
        let prev: HashMap<String, bool> =
            st.participants.iter().map(|p| (p.identity.clone(), p.speaking)).collect();
        (prev, st.sharing_self, st.camera_self)
    };

    let mut list: Vec<VoiceParticipant> = Vec::new();
    let me = room.local_participant();
    let me_identity = me.identity().to_string();
    list.push(VoiceParticipant {
        speaking: *previous.get(&me_identity).unwrap_or(&false),
        identity: me_identity,
        name: me.name().to_string(),
        is_me: true,
        sharing: self_sharing,
        camera: self_camera,
    });
    for (_, p) in room.remote_participants() {
        let identity = p.identity().to_string();
        list.push(VoiceParticipant {
            speaking: *previous.get(&identity).unwrap_or(&false),
            sharing: sharing_ids.contains(&identity),
            camera: camera_ids.contains(&identity),
            identity,
            name: p.name().to_string(),
            is_me: false,
        });
    }
    list.sort_by(|a, b| a.name.cmp(&b.name));
    s.write().participants = list;
}

/// Play one remote audio track on the configured (or default) output device.
/// Returns a stop handle; dropping it ends the playback thread.
/// How far behind the microphone hears what the speakers played.
///
/// The canceller aligns itself from here, so it only has to be in the right
/// area: a wrong guess costs convergence time, not correctness. Output
/// buffer plus input buffer plus a couple of feet of air is tens of
/// milliseconds on a normal machine.
const ECHO_DELAY_MS: i32 = 80;

/// The echo canceller, and the 10ms chunking both sides of it need.
///
/// WebRTC wants exactly 10ms at a time, from both ends: the microphone
/// (`process_stream`, which is where the echo is removed) and whatever the
/// speakers are playing (`process_reverse_stream`, the reference it subtracts
/// against). The microphone side already arrives in 10ms frames; the output
/// callback is handed whatever size the device feels like, so it is buffered
/// into chunks here.
pub struct Canceller {
    apm: Mutex<livekit::webrtc::native::apm::AudioProcessingModule>,
    /// Mixed output waiting to make up a whole 10ms chunk.
    render: Mutex<Vec<i16>>,
}

/// 10ms of 48kHz stereo, the reverse stream's chunk.
const RENDER_CHUNK: usize = (NS_RATE as usize / 100) * 2;

impl Canceller {
    fn new() -> Arc<Self> {
        // Echo canceller and high-pass only. Noise suppression and gain
        // control are left to the chain that already does them — RNNoise and
        // our own AutoGain are tuned and tested, and running two of each
        // would fight.
        let apm = livekit::webrtc::native::apm::AudioProcessingModule::new(true, false, true, false);
        let this = Arc::new(Self { apm: Mutex::new(apm), render: Mutex::new(Vec::new()) });
        if let Ok(mut apm) = this.apm.lock() {
            let _ = apm.set_stream_delay_ms(ECHO_DELAY_MS);
        }
        this
    }

    /// What the speakers are about to play.
    fn played(&self, out: &[f32]) {
        let Ok(mut render) = self.render.lock() else { return };
        render.extend(out.iter().map(|s| (s * 32767.0).clamp(-32768.0, 32767.0) as i16));
        let whole = render.len() / RENDER_CHUNK * RENDER_CHUNK;
        if whole == 0 {
            return;
        }
        let mut chunk: Vec<i16> = render.drain(..whole).collect();
        drop(render);
        if let Ok(mut apm) = self.apm.lock() {
            let _ = apm.process_reverse_stream(&mut chunk, NS_RATE as i32, 2);
        }
    }

    /// One 10ms microphone frame, with the echo taken out of it in place.
    fn heard(&self, frame: &mut [i16]) {
        if let Ok(mut apm) = self.apm.lock() {
            let _ = apm.process_stream(frame, NS_RATE as i32, 1);
        }
    }
}

/// One remote track's audio, waiting its turn to be mixed.
struct MixTrack {
    samples: std::collections::VecDeque<i16>,
    /// That person's volume slider. Shared with the UI, so moving it is
    /// heard on the next callback without touching the mixer.
    gain: Arc<AtomicU32>,
}

/// Everything being played, summed in one place.
///
/// This used to be one cpal output stream per remote track, each opening the
/// device and mixing itself in by writing on top of whatever was there. That
/// works, but it means the machine is playing N independent streams and
/// nothing in this process ever knows what the speakers are actually
/// producing — which is precisely the signal an echo canceller needs, and
/// why ours has never been able to do anything.
///
/// So: one stream, one callback, one place where the call's audio exists as
/// a single buffer.
pub struct Mixer {
    tracks: Mutex<HashMap<String, MixTrack>>,
    master: Arc<AtomicU32>,
    deafened: Arc<AtomicBool>,
    /// Told what came out, so the microphone can have it subtracted again.
    /// None when the setting is off.
    echo: Option<Arc<Canceller>>,
    /// When anything was last audible through the speakers, on `clock_ms`.
    /// The microphone side reads it to know when what it hears might be
    /// the call coming back.
    far_end_at: Arc<AtomicU64>,
}

/// Milliseconds on one clock both audio threads can read.
fn clock_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    // +1 so that 0 can mean "never".
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + 1
}

/// Output quieter than this (RMS, full scale 1.0, about -54 dBFS) is not the
/// far end talking. Opus with DTX decodes silence to near-zero, so this only
/// has to clear rounding, not a noise floor.
const FAR_END_FLOOR: f32 = 0.002;

/// How long after the far end stops that its echo can still be arriving:
/// the output buffer, the trip through the air, the input buffer, and the
/// room's tail.
const FAR_END_HOLD_MS: u64 = 400;

/// Whether the far end was audible recently enough to still be echoing.
fn far_end_audible(far_end_at: &AtomicU64) -> bool {
    far_end_recent(far_end_at.load(Ordering::Relaxed), clock_ms())
}

fn far_end_recent(at: u64, now: u64) -> bool {
    at != 0 && now.saturating_sub(at) < FAR_END_HOLD_MS
}

/// Latency cap per track: anything older than this is stale by the time it
/// would be heard, so it is dropped rather than queued.
const MIX_LATENCY_CAP: usize = 48_000 / 2;

impl Mixer {
    fn new(
        master: Arc<AtomicU32>,
        deafened: Arc<AtomicBool>,
        echo: Option<Arc<Canceller>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            tracks: Mutex::new(HashMap::new()),
            master,
            deafened,
            echo,
            far_end_at: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Shared with the microphone pump.
    fn far_end(&self) -> Arc<AtomicU64> {
        self.far_end_at.clone()
    }

    fn add(&self, sid: String, gain: Arc<AtomicU32>) {
        self.tracks
            .lock()
            .unwrap()
            .insert(sid, MixTrack { samples: std::collections::VecDeque::new(), gain });
    }

    fn remove(&self, sid: &str) {
        self.tracks.lock().unwrap().remove(sid);
    }

    fn clear(&self) {
        self.tracks.lock().unwrap().clear();
    }

    /// A track's reader hands over whatever LiveKit gave it.
    fn push(&self, sid: &str, samples: &[i16]) {
        let mut tracks = self.tracks.lock().unwrap();
        let Some(track) = tracks.get_mut(sid) else { return };
        track.samples.extend(samples.iter().copied());
        let extra = track.samples.len().saturating_sub(MIX_LATENCY_CAP);
        if extra > 0 {
            track.samples.drain(..extra);
        }
    }

    /// Sum every track into `out`, at that person's volume times the master.
    ///
    /// Deafened plays silence but still drains: holding the audio back would
    /// just queue it up, and un-deafening would replay the conversation you
    /// deliberately missed.
    fn fill(&self, out: &mut [f32]) {
        for sample in out.iter_mut() {
            *sample = 0.0;
        }
        let silent = self.deafened.load(Ordering::Relaxed);
        let master = f32::from_bits(self.master.load(Ordering::Relaxed));
        let mut tracks = self.tracks.lock().unwrap();
        for track in tracks.values_mut() {
            let gain = if silent {
                0.0
            } else {
                f32::from_bits(track.gain.load(Ordering::Relaxed)) * master
            };
            for sample in out.iter_mut() {
                let s = track.samples.pop_front().unwrap_or(0);
                *sample += s as f32 / 32768.0 * gain;
            }
        }
        // Several people at once can sum past full scale; clip at the end,
        // once, rather than per track.
        for sample in out.iter_mut() {
            *sample = sample.clamp(-1.0, 1.0);
        }
        drop(tracks);
        if !out.is_empty() {
            let mean_square = out.iter().map(|s| s * s).sum::<f32>() / out.len() as f32;
            if mean_square > FAR_END_FLOOR * FAR_END_FLOOR {
                self.far_end_at.store(clock_ms(), Ordering::Relaxed);
            }
        }
        // Hand the finished mix to the canceller. It has to be THIS signal —
        // after volumes, after the master, after clipping — because that is
        // what the speakers get and therefore what the microphone will hear
        // a moment later.
        if let Some(echo) = &self.echo {
            echo.played(out);
        }
    }
}

/// Open the call's one output stream. Lives until the call ends.
fn start_mix_output(
    mixer: Arc<Mixer>,
    output_device: Option<String>,
    mut status: VoiceStatusSignal,
) -> std_mpsc::Sender<()> {
    let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
    std::thread::spawn(move || {
        com_init_mta();
        let host = cpal::default_host();
        let Some(device) = pick_output_device(&host, &output_device) else {
            status.write().error = "no audio output device found".into();
            return;
        };
        let config = cpal::StreamConfig {
            channels: 2,
            sample_rate: 48000,
            buffer_size: cpal::BufferSize::Default,
        };
        let stream = device.build_output_stream(
            config,
            move |out: &mut [f32], _: &_| mixer.fill(out),
            move |e| {
                let text = e.to_string();
                if text.contains("underrun") || text.contains("overrun") {
                    return;
                }
                let mut s = status;
                s.write().error = format!("audio output error: {text}");
            },
            None,
        );
        match stream {
            Ok(stream) => {
                if stream.play().is_ok() {
                    let _ = stop_rx.recv();
                }
            }
            Err(e) => {
                let mut s = status;
                s.write().error = format!("audio output failed to open: {e}");
            }
        }
    });
    stop_tx
}

/// Feed one remote track into the mix. Returns the reader, to abort when the
/// track goes away.
fn spawn_reader(
    track: livekit::track::RemoteAudioTrack,
    sid: String,
    mixer: Arc<Mixer>,
    gain: Arc<AtomicU32>,
) -> tokio::task::JoinHandle<()> {
    mixer.add(sid.clone(), gain);
    let rtc_track = track.rtc_track();
    tokio::spawn(async move {
        // 48kHz stereo, resampled by libwebrtc on the way out.
        let mut stream = NativeAudioStream::new(rtc_track, 48000, 2);
        while let Some(frame) = stream.next().await {
            mixer.push(&sid, &frame.data);
        }
    })
}

#[cfg(test)]
mod mute_hotkey_tests {
    use super::*;

    #[test]
    fn a_held_key_toggles_once_not_once_per_tick() {
        // One unhurried press: down for six 30ms ticks, then released.
        let mut was_down = false;
        let ticks = [false, true, true, true, true, true, true, false, false];
        let fires = ticks.iter().filter(|&&d| pressed_now(d, &mut was_down)).count();
        assert_eq!(fires, 1, "holding the key must not re-toggle every tick");
    }

    #[test]
    fn two_presses_toggle_twice() {
        let mut was_down = false;
        let ticks = [true, true, false, false, true, true, true, false];
        let fires = ticks.iter().filter(|&&d| pressed_now(d, &mut was_down)).count();
        assert_eq!(fires, 2);
    }
}

#[cfg(test)]
mod share_audio_tests {
    use super::*;

    #[test]
    fn surround_and_mono_both_become_stereo() {
        let mut out = Vec::new();
        to_stereo(&[7, -7], 1, &mut out);
        assert_eq!(out, vec![7, 7, -7, -7], "mono goes to both ears");

        out.clear();
        to_stereo(&[1, 2, 3, 4], 2, &mut out);
        assert_eq!(out, vec![1, 2, 3, 4], "stereo is left alone");

        out.clear();
        // 5.1: front L, front R, centre, LFE, rear L, rear R.
        to_stereo(&[1, 2, 99, 99, 99, 99], 6, &mut out);
        assert_eq!(out, vec![1, 2], "the front pair, and nothing invented");
    }

    #[test]
    fn resampling_keeps_the_two_channels_apart() {
        // The thing that would ruin this feature quietly: left bleeding into
        // right turns a mix into mush and still sounds like "audio works".
        let frames = 441;
        let mut input = Vec::with_capacity(frames * 2);
        for _ in 0..frames {
            input.push(1000);
            input.push(-1000);
        }
        let mut carry = (0.0f64, [0.0f32; 2]);
        let mut out = Vec::new();
        resample_stereo(&input, 44_100, &mut carry, &mut out);

        assert_eq!(out.len() % 2, 0, "whole frames only");
        // 10ms at 44.1k resampled to 48k is ~480 frames.
        let got = out.len() / 2;
        assert!((470..=490).contains(&got), "expected ~480 frames, got {got}");
        // Sign separation holds everywhere, ramp included: if the channels
        // were bleeding into each other these would converge on zero.
        for pair in out.chunks_exact(2) {
            assert!(pair[0] >= 0, "left took the right's sign");
            assert!(pair[1] <= 0, "right took the left's sign");
        }
        // 48k is the faster clock, so the first couple of output frames land
        // before input frame 1 and interpolate against the carried-in last
        // sample — zero on the very first block, which is a stream starting
        // from silence. Past that it must be exact.
        let ramp = (1.0 / (44_100.0 / NS_RATE as f64)).ceil() as usize;
        for pair in out.chunks_exact(2).skip(ramp) {
            assert_eq!(pair[0], 1000, "left drifted");
            assert_eq!(pair[1], -1000, "right drifted");
        }
    }

    #[test]
    fn a_matching_rate_is_passed_straight_through() {
        let mut carry = (0.0f64, [0.0f32; 2]);
        let mut out = Vec::new();
        resample_stereo(&[5, 6, 7, 8], NS_RATE, &mut carry, &mut out);
        assert_eq!(out, vec![5, 6, 7, 8], "48k needs no arithmetic at all");
    }
}

#[cfg(test)]
mod mixer_tests {
    use super::*;

    fn gain(v: f32) -> Arc<AtomicU32> {
        Arc::new(AtomicU32::new(v.to_bits()))
    }

    fn mixer(master: f32, deaf: bool) -> Arc<Mixer> {
        Mixer::new(gain(master), Arc::new(AtomicBool::new(deaf)), None)
    }

    #[test]
    fn two_people_talking_are_summed() {
        let m = mixer(1.0, false);
        m.add("a".into(), gain(1.0));
        m.add("b".into(), gain(1.0));
        m.push("a", &[8000, 8000]);
        m.push("b", &[4000, 4000]);
        let mut out = [0.0f32; 2];
        m.fill(&mut out);
        let want = 12000.0 / 32768.0;
        assert!((out[0] - want).abs() < 1e-4, "got {}, wanted {want}", out[0]);
    }

    #[test]
    fn each_person_keeps_their_own_volume() {
        let m = mixer(1.0, false);
        m.add("loud".into(), gain(1.0));
        m.add("quiet".into(), gain(0.25));
        m.push("loud", &[8000, 8000]);
        m.push("quiet", &[8000, 8000]);
        let mut out = [0.0f32; 2];
        m.fill(&mut out);
        let want = (8000.0 + 2000.0) / 32768.0;
        assert!((out[0] - want).abs() < 1e-4, "got {}, wanted {want}", out[0]);
    }

    #[test]
    fn the_master_scales_everyone() {
        let m = mixer(0.5, false);
        m.add("a".into(), gain(1.0));
        m.push("a", &[10000, 10000]);
        let mut out = [0.0f32; 2];
        m.fill(&mut out);
        assert!((out[0] - 5000.0 / 32768.0).abs() < 1e-4);
    }

    #[test]
    fn deafened_is_silent_but_still_drains() {
        // The subtle one. Holding the audio back instead of dropping it
        // would queue up the conversation and replay it the moment you
        // un-deafened, which is the opposite of what the button is for.
        let deaf = Arc::new(AtomicBool::new(true));
        let m = Mixer::new(gain(1.0), deaf.clone(), None);
        m.add("a".into(), gain(1.0));
        m.push("a", &[9000; 8]);
        let mut out = [0.0f32; 4];
        m.fill(&mut out);
        assert!(out.iter().all(|s| *s == 0.0), "deafened must be silent");

        // Those four samples are gone, not waiting.
        deaf.store(false, Ordering::Relaxed);
        let mut out = [0.0f32; 8];
        m.fill(&mut out);
        let heard = out.iter().filter(|s| **s != 0.0).count();
        assert_eq!(heard, 4, "only what arrived after un-deafening should play");
    }

    #[test]
    fn a_silent_track_does_not_mute_the_others() {
        let m = mixer(1.0, false);
        m.add("talking".into(), gain(1.0));
        m.add("idle".into(), gain(1.0));
        m.push("talking", &[6000, 6000]);
        let mut out = [0.0f32; 2];
        m.fill(&mut out);
        assert!((out[0] - 6000.0 / 32768.0).abs() < 1e-4, "the idle track contributed noise");
    }

    #[test]
    fn a_backlog_is_dropped_rather_than_queued() {
        let m = mixer(1.0, false);
        m.add("a".into(), gain(1.0));
        // Three seconds of audio at once: a stall, then a flood.
        m.push("a", &vec![1234i16; 48_000 * 2 * 3]);
        let held = m.tracks.lock().unwrap()["a"].samples.len();
        assert!(held <= MIX_LATENCY_CAP, "kept {held} samples, cap is {MIX_LATENCY_CAP}");
    }

    #[test]
    fn a_loud_room_clips_instead_of_wrapping() {
        let m = mixer(1.0, false);
        for who in ["a", "b", "c"] {
            m.add(who.into(), gain(1.0));
            m.push(who, &[30000, 30000]);
        }
        let mut out = [0.0f32; 2];
        m.fill(&mut out);
        assert!(out[0] <= 1.0 && out[0] >= 0.99, "should sit at full scale, got {}", out[0]);
    }

    #[test]
    fn a_track_that_leaves_stops_being_mixed() {
        let m = mixer(1.0, false);
        m.add("a".into(), gain(1.0));
        m.push("a", &[7000, 7000]);
        m.remove("a");
        let mut out = [0.0f32; 2];
        m.fill(&mut out);
        assert_eq!(out, [0.0, 0.0]);
    }
}

/// Real-hardware check for the mix. Opens the default output, plays two
/// tones through the mixer at different volumes, and listens to that same
/// device via WASAPI loopback to hear what actually came out.
///
/// Ignored by default because it makes a noise and needs a working output.
///   cargo test -p client -- --ignored mix_output --nocapture
#[cfg(test)]
mod mix_hardware_tests {
    use super::*;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    /// Energy at one frequency, by Goertzel — cheaper than an FFT and all
    /// this needs is "is that tone in there".
    fn tone_energy(samples: &[f32], freq: f32, rate: f32) -> f32 {
        let k = (0.5 + samples.len() as f32 * freq / rate).floor();
        let w = std::f32::consts::TAU * k / samples.len() as f32;
        let coeff = 2.0 * w.cos();
        let (mut s0, mut s1, mut s2) = (0.0f32, 0.0f32, 0.0f32);
        for x in samples {
            s0 = x + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        (s1 * s1 + s2 * s2 - coeff * s1 * s2).sqrt() / samples.len() as f32
    }

    fn tone(freq: f32, frames: usize, phase: &mut f32) -> Vec<i16> {
        let mut out = Vec::with_capacity(frames * 2);
        for _ in 0..frames {
            let v = (*phase * std::f32::consts::TAU).sin() * 0.4;
            *phase = (*phase + freq / 48_000.0).fract();
            let s = (v * 32767.0) as i16;
            out.push(s);
            out.push(s);
        }
        out
    }

    #[test]
    #[ignore]
    fn mix_output_is_audible_on_the_real_device() {
        let master = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let deaf = Arc::new(AtomicBool::new(false));
        let mixer = Mixer::new(master, deaf, None);
        // Two people: one at full volume, one turned down to a quarter.
        mixer.add("loud".into(), Arc::new(AtomicU32::new(1.0f32.to_bits())));
        mixer.add("quiet".into(), Arc::new(AtomicU32::new(0.25f32.to_bits())));

        // Listen to the device we are about to play through.
        let host = cpal::default_host();
        let device = host.default_output_device().expect("an output device");
        let config = device.default_output_config().expect("a config");
        let rate = config.sample_rate() as f32;
        let heard: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = heard.clone();
        let listener = device
            .build_input_stream(
                config.config(),
                move |data: &[f32], _: &_| sink.lock().unwrap().extend_from_slice(data),
                |e| eprintln!("listen: {e}"),
                None,
            )
            .expect("loopback capture");
        listener.play().expect("listening");

        // The same callback start_mix_output installs, without needing a
        // dioxus runtime for the error signal it would otherwise report to.
        let cb_mixer = mixer.clone();
        let player = device
            .build_output_stream(
                cpal::StreamConfig {
                    channels: 2,
                    sample_rate: 48000,
                    buffer_size: cpal::BufferSize::Default,
                },
                move |out: &mut [f32], _: &_| cb_mixer.fill(out),
                |e| eprintln!("play: {e}"),
                None,
            )
            .expect("output stream");
        player.play().expect("playing");

        let (mut p1, mut p2) = (0.0f32, 0.0f32);
        for _ in 0..100 {
            mixer.push("loud", &tone(440.0, 480, &mut p1));
            mixer.push("quiet", &tone(1760.0, 480, &mut p2));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));

        let samples = heard.lock().unwrap().clone();
        assert!(samples.len() > 48_000, "captured almost nothing: {}", samples.len());
        let mono: Vec<f32> = samples.chunks(2).map(|c| c[0]).collect();
        let loud = tone_energy(&mono, 440.0, rate);
        let quiet = tone_energy(&mono, 1760.0, rate);
        println!("440Hz (full volume): {loud:.5}\n1760Hz (quarter):    {quiet:.5}");
        assert!(loud > 0.001, "the full-volume track never made it out");
        assert!(quiet > 0.0002, "the quiet track was dropped entirely");
        assert!(loud > quiet * 2.0, "per-track volume was not applied: {loud} vs {quiet}");
    }
}

#[cfg(test)]
mod echo_measure {
    use super::*;

    /// Deterministic noise, so runs are comparable.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
        }
    }

    /// Speech-shaped far end: noise in syllable-rate bursts.
    fn far_end(seconds: usize) -> Vec<f32> {
        let mut rng = Lcg(7);
        let mut lp = 0.0f32;
        (0..NS_RATE as usize * seconds)
            .map(|n| {
                let t = n as f32 / NS_RATE as f32;
                let env = (t * 3.1 * std::f32::consts::TAU).sin().max(0.0).powi(2)
                    * (0.6 + 0.4 * (t * 0.37 * std::f32::consts::TAU).sin().abs());
                lp = 0.7 * lp + 0.3 * rng.next();
                lp * env * 0.6
            })
            .collect()
    }

    /// A small room: direct path plus a few reflections, after `delay_ms`.
    fn room(x: &[f32], delay_ms: usize, level: f32) -> Vec<f32> {
        let d = NS_RATE as usize * delay_ms / 1000;
        let taps: [(usize, f32); 5] = [(0, 1.0), (240, 0.45), (620, -0.3), (1300, 0.18), (2900, 0.08)];
        (0..x.len())
            .map(|n| {
                taps.iter()
                    .filter_map(|(off, g)| n.checked_sub(d + off).map(|i| x[i] * g))
                    .sum::<f32>()
                    * level
            })
            .collect()
    }

    fn energy(x: &[f32]) -> f64 {
        x.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>() / x.len().max(1) as f64
    }

    /// Run the canceller the way a call does, with the echo arriving
    /// `delay_ms` after it was played. Returns (ERLE dB, echo-return dB after
    /// the auto gain stage).
    fn run(delay_ms: usize, with_agc: bool) -> (f64, f64) {
        let secs = 12;
        let x = far_end(secs);
        let y = room(&x, delay_ms, 0.5);
        let echo = Canceller::new();
        let mut agc = AutoGain::new();
        let mut mic_in = Vec::new();
        let mut aec_out = Vec::new();
        let mut agc_out = Vec::new();
        for tick in 0..(x.len() / NS_FRAME) {
            let span = tick * NS_FRAME..(tick + 1) * NS_FRAME;
            let played: Vec<f32> = x[span.clone()].iter().flat_map(|v| [*v, *v]).collect();
            echo.played(&played);
            let mut pcm: Vec<i16> = y[span.clone()]
                .iter()
                .map(|v| (v * 32767.0).clamp(-32768.0, 32767.0) as i16)
                .collect();
            let before: Vec<f32> = pcm.iter().map(|s| *s as f32).collect();
            echo.heard(&mut pcm);
            let mut after: Vec<f32> = pcm.iter().map(|s| *s as f32).collect();
            if tick * NS_FRAME >= (secs - 4) * NS_RATE as usize {
                mic_in.extend(before.iter().copied());
                aec_out.extend(after.iter().copied());
            }
            if with_agc {
                let rms = (after.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / after.len() as f64).sqrt();
                let boost = agc.feed(rms);
                for s in after.iter_mut() {
                    *s = (*s * boost).clamp(-32768.0, 32767.0);
                }
            }
            if tick * NS_FRAME >= (secs - 4) * NS_RATE as usize {
                agc_out.extend(after);
            }
        }
        let erle = 10.0 * (energy(&mic_in) / energy(&aec_out).max(1e-9)).log10();
        let after_agc = 10.0 * (energy(&mic_in) / energy(&agc_out).max(1e-9)).log10();
        (erle, after_agc)
    }

    /// The same, but with the two sides arriving the way they do in the
    /// app: the speaker side in device-sized callbacks, the microphone side
    /// in device-sized chunks that then wait a scheduling delay of up to
    /// `jitter_ms` before the pump gets to them. Returns ERLE in dB.
    fn run_jittered(render_ms: usize, capture_ms: usize, jitter_ms: usize) -> f64 {
        let secs = 12;
        let rate = NS_RATE as usize;
        let x = far_end(secs);
        let r = rate * render_ms / 1000;
        let c = rate * capture_ms / 1000;
        // Output buffer + a 40ms device/air path.
        let delay_ms = render_ms + 40;
        let y = room(&x, delay_ms, 0.5);
        let echo = Canceller::new();
        let mut rng = Lcg(99);

        // (time in samples, is_render, chunk index)
        let mut events: Vec<(usize, bool, usize)> = Vec::new();
        for k in 0..x.len() / r {
            events.push((k * r, true, k));
        }
        let mut last = 0usize;
        for j in 0..y.len() / c {
            let ready = (j + 1) * c;
            let wait = (rng.next().abs() * (rate * jitter_ms / 1000) as f32) as usize;
            let at = (ready + wait).max(last);
            last = at;
            events.push((at, false, j));
        }
        events.sort_by_key(|e| (e.0, !e.1));

        let mut pending: Vec<f32> = Vec::new();
        let mut mic_in = Vec::new();
        let mut out = Vec::new();
        let tail = (secs - 4) * rate;
        for (_, is_render, idx) in events {
            if is_render {
                let played: Vec<f32> = x[idx * r..(idx + 1) * r].iter().flat_map(|v| [*v, *v]).collect();
                echo.played(&played);
            } else {
                pending.extend_from_slice(&y[idx * c..(idx + 1) * c]);
                let start = idx * c + c - pending.len();
                let mut pos = start;
                while pending.len() >= NS_FRAME {
                    let frame: Vec<f32> = pending.drain(..NS_FRAME).collect();
                    let mut pcm: Vec<i16> =
                        frame.iter().map(|v| (v * 32767.0).clamp(-32768.0, 32767.0) as i16).collect();
                    let before: Vec<f32> = pcm.iter().map(|s| *s as f32).collect();
                    echo.heard(&mut pcm);
                    if pos >= tail {
                        mic_in.extend(before);
                        out.extend(pcm.iter().map(|s| *s as f32));
                    }
                    pos += NS_FRAME;
                }
            }
        }
        10.0 * (energy(&mic_in) / energy(&out).max(1e-9)).log10()
    }

    #[test]
    #[ignore = "measurement; run with --nocapture"]
    fn measure_cancellation_against_scheduling() {
        for (r, c, j) in [
            (10, 10, 0),
            (10, 10, 10),
            (10, 10, 30),
            (10, 10, 60),
            (20, 20, 0),
            (20, 20, 30),
            (30, 10, 0),
            (10, 20, 50),
            (10, 10, 120),
        ] {
            println!(
                "speaker callback {r:>2}ms, mic chunks {c:>2}ms, pump delay up to {j:>3}ms: removes {:>6.1} dB",
                run_jittered(r, c, j)
            );
        }
    }

    /// Cheap speakers: driven hard they saturate, and their clock is not the
    /// microphone's. Returns (ERLE, echo return after auto gain) in dB.
    fn run_real_world(drive: f32, drift_ppm: f64, near_noise: f32) -> (f64, f64) {
        let secs = 14;
        let x = far_end(secs);
        // What actually leaves the speaker: saturated, then resampled by the
        // drift between the two devices' clocks.
        let sat: Vec<f32> = x.iter().map(|v| (v * drive).tanh() / drive.max(1.0)).collect();
        let ratio = 1.0 + drift_ppm / 1e6;
        let drifted: Vec<f32> = (0..sat.len())
            .map(|n| {
                let pos = n as f64 * ratio;
                let i = pos as usize;
                let f = (pos - i as f64) as f32;
                match (sat.get(i), sat.get(i + 1)) {
                    (Some(a), Some(b)) => a * (1.0 - f) + b * f,
                    _ => 0.0,
                }
            })
            .collect();
        let mut rng = Lcg(3);
        let y: Vec<f32> = room(&drifted, 80, 0.5).iter().map(|v| v + rng.next() * near_noise).collect();
        let echo = Canceller::new();
        let mut agc = AutoGain::new();
        let (mut mic_in, mut aec_out, mut agc_out) = (Vec::new(), Vec::new(), Vec::new());
        let tail = (secs - 4) * NS_RATE as usize;
        for tick in 0..(x.len() / NS_FRAME) {
            let span = tick * NS_FRAME..(tick + 1) * NS_FRAME;
            let played: Vec<f32> = x[span.clone()].iter().flat_map(|v| [*v, *v]).collect();
            echo.played(&played);
            let mut pcm: Vec<i16> =
                y[span.clone()].iter().map(|v| (v * 32767.0).clamp(-32768.0, 32767.0) as i16).collect();
            let before: Vec<f32> = pcm.iter().map(|s| *s as f32).collect();
            echo.heard(&mut pcm);
            let mut after: Vec<f32> = pcm.iter().map(|s| *s as f32).collect();
            let rms = (after.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / after.len() as f64).sqrt();
            let boost = agc.feed_against(rms, true);
            if tick * NS_FRAME >= tail {
                mic_in.extend(before);
                aec_out.extend(after.iter().copied());
            }
            for s in after.iter_mut() {
                *s = (*s * boost).clamp(-32768.0, 32767.0);
            }
            if tick * NS_FRAME >= tail {
                agc_out.extend(after);
            }
        }
        (
            10.0 * (energy(&mic_in) / energy(&aec_out).max(1e-9)).log10(),
            10.0 * (energy(&mic_in) / energy(&agc_out).max(1e-9)).log10(),
        )
    }

    #[test]
    #[ignore = "measurement; run with --nocapture"]
    fn measure_cancellation_in_a_real_room() {
        for (drive, ppm, noise) in [
            (1.0, 0.0, 0.0),
            (3.0, 0.0, 0.0),
            (6.0, 0.0, 0.0),
            (1.0, 100.0, 0.0),
            (1.0, 500.0, 0.0),
            (3.0, 200.0, 0.002),
            (6.0, 500.0, 0.004),
        ] {
            let (erle, agc) = run_real_world(drive, ppm, noise);
            println!(
                "speaker drive {drive:.0}x, clock drift {ppm:>3.0}ppm, room noise {noise:.3}: canceller {erle:>5.1} dB, after auto gain {agc:>6.1} dB"
            );
        }
    }

    #[test]
    #[ignore = "measurement; run with --nocapture"]
    fn measure_cancellation_against_delay() {
        for delay in [0usize, 40, 80, 120, 200, 300, 450] {
            let (erle, agc) = run(delay, true);
            println!("delay {delay:>3}ms: canceller removes {erle:>6.1} dB; after auto gain {agc:>6.1} dB");
        }
    }
}

#[cfg(test)]
mod echo_tests {
    use super::*;

    /// Energy at one frequency, by Goertzel.
    fn tone_energy(samples: &[f32], freq: f32) -> f32 {
        let k = (0.5 + samples.len() as f32 * freq / NS_RATE as f32).floor();
        let w = std::f32::consts::TAU * k / samples.len() as f32;
        let coeff = 2.0 * w.cos();
        let (mut s0, mut s1, mut s2) = (0.0f32, 0.0f32, 0.0f32);
        for x in samples {
            s0 = x + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        (s1 * s1 + s2 * s2 - coeff * s1 * s2).sqrt() / samples.len() as f32
    }

    #[test]
    fn it_subtracts_what_the_speakers_played() {
        // The worst case a speaker user produces: the microphone hears
        // exactly what came out, and nothing else. Everything the canceller
        // leaves behind is echo it failed to remove.
        let echo = Canceller::new();
        let mut phase = 0.0f32;
        let mut first = Vec::new();
        let mut last = Vec::new();

        // Three seconds. AEC3 has to hear the room before it can cancel it,
        // so the beginning is expected to leak — the question is where it
        // ends up.
        for frame_no in 0..300 {
            let mut played = Vec::with_capacity(RENDER_CHUNK);
            let mut mono = Vec::with_capacity(NS_FRAME);
            for _ in 0..NS_FRAME {
                let v = (phase * std::f32::consts::TAU).sin() * 0.5;
                phase = (phase + 440.0 / NS_RATE as f32).fract();
                mono.push((v * 32767.0) as i16);
                played.push(v);
                played.push(v);
            }
            echo.played(&played);
            echo.heard(&mut mono);
            let out: Vec<f32> = mono.iter().map(|s| *s as f32 / 32768.0).collect();
            if frame_no < 20 {
                first.extend(out);
            } else if frame_no >= 280 {
                last.extend(out);
            }
        }

        let before = tone_energy(&first, 440.0);
        let after = tone_energy(&last, 440.0);
        println!("echo at the start: {before:.6}\necho at the end:   {after:.6}");
        assert!(
            after < before * 0.5,
            "the canceller never converged: {before:.6} -> {after:.6}"
        );
    }

    #[test]
    fn silence_from_the_speakers_leaves_the_voice_alone() {
        // Nothing is playing, so nothing should be subtracted: somebody on
        // headphones must not have their own voice eaten.
        let echo = Canceller::new();
        let mut phase = 0.0f32;
        let mut spoken = Vec::new();
        let mut heard = Vec::new();
        for _ in 0..100 {
            let mut mono = Vec::with_capacity(NS_FRAME);
            for _ in 0..NS_FRAME {
                let v = (phase * std::f32::consts::TAU).sin() * 0.4;
                phase = (phase + 300.0 / NS_RATE as f32).fract();
                mono.push((v * 32767.0) as i16);
            }
            spoken.extend(mono.iter().map(|s| *s as f32 / 32768.0));
            echo.played(&vec![0.0f32; RENDER_CHUNK]);
            echo.heard(&mut mono);
            heard.extend(mono.iter().map(|s| *s as f32 / 32768.0));
        }
        let sent = tone_energy(&spoken, 300.0);
        let kept = tone_energy(&heard, 300.0);
        println!("voice in: {sent:.6}\nvoice out: {kept:.6}");
        assert!(kept > sent * 0.5, "it ate the voice: {sent:.6} -> {kept:.6}");
    }
}

#[cfg(test)]
mod ptt_tick_tests {
    use super::*;

    #[test]
    fn the_two_ticks_are_tellable_apart() {
        let open = ptt_tick_samples(true);
        let close = ptt_tick_samples(false);
        assert_eq!(open.len(), close.len());

        // Count zero crossings: more of them means a higher note, so opening
        // reads as "up" and closing as "down" without having to listen.
        let crossings = |s: &[f32]| {
            s.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count()
        };
        assert!(
            crossings(&open) > crossings(&close),
            "the open tick should be the higher of the two"
        );
    }

    #[test]
    fn it_is_short_quiet_and_does_not_click() {
        let tick = ptt_tick_samples(true);
        let ms = tick.len() * 1000 / CUE_RATE as usize;
        assert!((30..=60).contains(&ms), "a tick that fires every sentence must stay short: {ms}ms");

        let peak = tick.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak < 0.2, "too loud to live with: {peak}");
        // A tick that starts or ends on a step clicks; both ends must be at
        // rest. The join blip only fades out, so this is its own check.
        assert!(tick[0].abs() < 0.01, "starts with a step: {}", tick[0]);
        assert!(tick.last().unwrap().abs() < 0.01, "ends with a step: {}", tick.last().unwrap());
    }

    #[test]
    fn it_is_quieter_than_the_join_blip() {
        // It fires far more often, so it has to sit further back.
        let peak = |s: &[f32]| s.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak(&ptt_tick_samples(true)) < peak(&blip_samples(true)));
    }
}
