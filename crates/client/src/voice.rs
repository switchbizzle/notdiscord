//! Voice engine: connects to a LiveKit room, publishes the microphone, and
//! plays back remote participants' audio.
//!
//! cpal streams are !Send, so each audio device stream lives on its own OS
//! thread; those threads exit when their stop-channel sender is dropped
//! (i.e. when the `ActiveCall` owning it is dropped).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use dioxus::prelude::*;
use futures_util::StreamExt;
use livekit::options::TrackPublishOptions;
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
    /// Our webcam is currently on.
    pub camera_self: bool,
    /// Push-to-talk mode is active and the key is currently held.
    pub ptt_held: bool,
    pub participants: Vec<VoiceParticipant>,
    /// Playback volume per identity (1.0 = 100%).
    pub volumes: HashMap<String, f32>,
    pub error: String,
}

pub enum VoiceCmd {
    Join { channel_id: i64, channel_name: String, url: String, token: String },
    Leave,
    ToggleMute,
    SetVolume { identity: String, volume: f32 },
    SetMicVolume(f32),
    SetMasterVolume(f32),
    SetNoiseSuppression(bool),
    ToggleDeafen,
    /// mode: "vad" | "ptt"; key: device_query Keycode name.
    SetVoiceMode { mode: String, key: String },
    /// Voice-activity gate threshold (RMS, 0 = always transmit).
    SetVadThreshold(f32),
    /// monitor: 1-based index from share::list_monitors; None = primary.
    StartScreenShare { monitor: Option<usize> },
    StopScreenShare,
    /// Open a viewer window for this participant's screen share.
    WatchScreen { identity: String },
    StartCamera,
    StopCamera,
    /// Open a viewer window for this participant's webcam.
    WatchCamera { identity: String },
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

pub const PTT_KEY_CHOICES: &[&str] = &[
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
    "Grave", "CapsLock", "LShift", "LControl", "LAlt",
];

/// Mic input level (0..1), updated ~10x/sec while in a call. Kept separate
/// from VoiceStatus so only the meter widget re-renders on ticks.
pub type MicLevelSignal = Signal<f32, SyncStorage>;

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

/// Convert any cpal input sample slice to i16.
fn build_capture_stream(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    tx: tokio::sync::mpsc::UnboundedSender<Vec<i16>>,
    mut status: VoiceStatusSignal,
) -> Result<cpal::Stream, cpal::Error> {
    let cfg = config.config();
    let err_cb = move |e: cpal::Error| {
        // Buffer under/overruns are transient on WASAPI and self-recover;
        // only surface errors that actually stop the stream.
        let text = e.to_string();
        if text.contains("underrun") || text.contains("overrun") {
            return;
        }
        status.write().error = format!("mic stream error: {text}");
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
    playback_stops: Arc<Mutex<HashMap<String, std_mpsc::Sender<()>>>>,
    /// Per-identity playback gain, read lock-free by audio callbacks.
    gains: Arc<Mutex<HashMap<String, Arc<AtomicU32>>>>,
    mic_gain: Arc<AtomicU32>,
    master_gain: Arc<AtomicU32>,
    ns_enabled: Arc<AtomicBool>,
    deafened: Arc<AtomicBool>,
    /// True when voice mode is push-to-talk.
    ptt_mode: Arc<AtomicBool>,
    /// VAD gate threshold as f32 bits.
    vad_threshold: Arc<AtomicU32>,
    /// The configured PTT key, read by the polling thread each tick.
    ptt_key: Arc<Mutex<device_query::Keycode>>,
    /// Dropping ends the PTT polling thread.
    _ptt_stop: std_mpsc::Sender<()>,
    /// Active screen capture + its published track sid.
    share: Option<(crate::share::ShareControl, livekit::id::TrackSid)>,
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
        self.playback_stops.lock().unwrap().clear();
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
                                VoiceCmd::StartScreenShare { monitor: None }
                            }
                        }
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
                status.set(VoiceStatus {
                    channel_id: Some(channel_id),
                    channel_name: channel_name.clone(),
                    connecting: true,
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
                match connect(&url, &token, status, mic_level).await {
                    Ok(active) => {
                        *live_tracks.lock().unwrap() = Some(active.video_tracks.clone());
                        call = Some(active);
                        status.write().connecting = false;
                        // You hear your own arrival too, like Discord.
                        play_voice_blip(true);
                    }
                    Err(e) => {
                        status.set(VoiceStatus {
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
            VoiceCmd::StartScreenShare { monitor } => {
                if let Some(active) = call.as_mut() {
                    if active.share.is_none() {
                        let source = NativeVideoSource::new(
                            VideoResolution { width: 1920, height: 1080 },
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
                                match crate::share::start_capture(source, monitor, preview) {
                                Ok(control) => {
                                    active.share = Some((control, publication.sid()));
                                    let mut s = status.write();
                                    s.sharing_self = true;
                                    if let Some(me) = s.participants.iter_mut().find(|p| p.is_me) {
                                        me.sharing = true;
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
                                // Show yourself what you're broadcasting, as a
                                // picture-in-picture in the call window.
                                let slot: crate::share::SharedFrame = Default::default();
                                let alive = Arc::new(AtomicBool::new(true));
                                call_state.lock().unwrap().self_preview = Some(slot.clone());
                                // Your own tile in the Video tab comes from the
                                // same preview slot, not off the wire.
                                crate::frames::publish_shared("self:camera".into(), slot.clone());
                                let _ = crate::share::open_call_window(call_state.clone(), action_tx.clone());
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
            VoiceCmd::SetVadThreshold(threshold) => {
                let threshold = threshold.clamp(0.0, 3000.0);
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
) -> anyhow::Result<ActiveCall> {
    let (room, events) = Room::connect(url, token, livekit::RoomOptions::default()).await?;
    let room = Arc::new(room);

    // ---- Microphone capture ----
    let settings = crate::api::load_settings();
    let host = cpal::default_host();
    let mic = pick_input_device(&host, &settings.input_device)
        .ok_or_else(|| anyhow::anyhow!("no microphone found"))?;
    let mic_name = device_name(&mic).unwrap_or_else(|| "unknown".into());
    let mic_config = mic
        .default_input_config()
        .map_err(|e| anyhow::anyhow!("cannot open mic '{mic_name}': {e}"))?;
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
        match build_capture_stream(&mic, &mic_config, frame_tx, mic_status) {
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
    let deafened = Arc::new(AtomicBool::new(false));
    let ptt_mode = Arc::new(AtomicBool::new(settings.voice_mode == "ptt"));
    let ptt_key = Arc::new(Mutex::new(parse_ptt_key(&settings.ptt_key)));
    let ptt_active = Arc::new(AtomicBool::new(false));
    let vad_threshold = Arc::new(AtomicU32::new(settings.vad_threshold.clamp(0.0, 3000.0).to_bits()));

    // PTT key poller: 30ms ticks, no global hotkey registration, so the key
    // keeps working in other apps and is never swallowed system-wide.
    let (ptt_stop_tx, ptt_stop_rx) = std_mpsc::channel::<()>();
    {
        let key = ptt_key.clone();
        let active = ptt_active.clone();
        let mode = ptt_mode.clone();
        let mut ptt_status = status;
        std::thread::spawn(move || {
            use device_query::DeviceQuery;
            let device = device_query::DeviceState::new();
            loop {
                match ptt_stop_rx.recv_timeout(std::time::Duration::from_millis(30)) {
                    Err(std_mpsc::RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
                let held = if mode.load(Ordering::Relaxed) {
                    let target = *key.lock().unwrap();
                    device.get_keys().contains(&target)
                } else {
                    false
                };
                let prev = active.swap(held, Ordering::Relaxed);
                if prev != held {
                    ptt_status.write().ptt_held = held;
                }
            }
        });
    }

    let mut pump_status = status;
    let pump_gain = mic_gain.clone();
    let pump_ns = ns_enabled.clone();
    let pump_ptt_mode = ptt_mode.clone();
    let pump_ptt_active = ptt_active.clone();
    let pump_vad = vad_threshold.clone();
    tokio::spawn(async move {
        const SPEAK_HOLD: std::time::Duration = std::time::Duration::from_millis(600);
        const METER_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
        let mut denoise = nnnoiseless::DenoiseState::new();
        let mut denoised = [0.0f32; NS_FRAME];
        let mut mono48: Vec<f32> = Vec::with_capacity(NS_FRAME * 8);
        let mut resample_pos: f64 = 0.0;
        let mut last_sample: f32 = 0.0;
        let step = sample_rate as f64 / NS_RATE as f64;
        let mut speaking = false;
        let mut last_voice = std::time::Instant::now() - SPEAK_HOLD;
        let mut last_meter = std::time::Instant::now() - METER_INTERVAL;

        while let Some(chunk) = frame_rx.recv().await {
            // Downmix interleaved device channels to mono f32 (i16 scale).
            let mono: Vec<f32> = chunk
                .chunks(channels.max(1) as usize)
                .map(|frame| frame.iter().map(|s| *s as f32).sum::<f32>() / frame.len() as f32)
                .collect();

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

                let gain = f32::from_bits(pump_gain.load(Ordering::Relaxed));
                if (gain - 1.0).abs() > f32::EPSILON {
                    for s in frame.iter_mut() {
                        *s = (*s * gain).clamp(-32768.0, 32767.0);
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
                if now.duration_since(last_meter) >= METER_INTERVAL {
                    last_meter = now;
                    mic_level.set(((rms / 10000.0) as f32).min(1.0));
                }
                let gate_threshold = f32::from_bits(pump_vad.load(Ordering::Relaxed)) as f64;
                // The speaking ring needs a floor so "always transmit" doesn't
                // glow constantly on room hum.
                if rms >= gate_threshold.max(300.0) {
                    last_voice = now;
                }
                let voice_recent = now.duration_since(last_voice) < SPEAK_HOLD;
                let muted = pump_status.peek().muted;
                let gate_open = if pump_ptt_mode.load(Ordering::Relaxed) {
                    pump_ptt_active.load(Ordering::Relaxed)
                } else {
                    // Voice activity: transmit only while above the threshold
                    // (with hold); 0 = classic open mic.
                    gate_threshold <= 0.0 || voice_recent
                };
                let transmitting = !muted && gate_open;
                let now_speaking = transmitting && voice_recent;
                if now_speaking != speaking {
                    speaking = now_speaking;
                    let mut s = pump_status.write();
                    if let Some(me) = s.participants.iter_mut().find(|p| p.is_me) {
                        me.speaking = speaking;
                    }
                }

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
    let playback_stops: Arc<Mutex<HashMap<String, std_mpsc::Sender<()>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let video_tracks: Arc<Mutex<VideoTracks>> = Arc::new(Mutex::new(VideoTracks::default()));
    let tracks_for_events = video_tracks.clone();
    let gains: Arc<Mutex<HashMap<String, Arc<AtomicU32>>>> = Arc::new(Mutex::new(HashMap::new()));
    let stops = playback_stops.clone();
    let gains_for_events = gains.clone();
    let master_for_events = master_gain.clone();
    let deafen_for_events = deafened.clone();
    let saved_volumes = settings.volumes.clone();
    let room_handle = room.clone();
    let output_device = settings.output_device.clone();
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
                            let stop = spawn_playback(audio, output_device.clone(), status, gain, master_for_events.clone(), deafen_for_events.clone());
                            stops.lock().unwrap().insert(sid, stop);
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
                            stops.lock().unwrap().remove(&audio.sid().to_string());
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
                    // Local speaking is driven by the mic-level detector.
                    for p in st.participants.iter_mut().filter(|p| !p.is_me) {
                        p.speaking = speaking.contains(&p.identity);
                    }
                }
                RoomEvent::Disconnected { .. } => {
                    let mut s = status;
                    s.set(VoiceStatus { error: "disconnected from voice".into(), ..Default::default() });
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
        playback_stops,
        gains,
        mic_gain,
        master_gain,
        ns_enabled,
        deafened,
        ptt_mode,
        vad_threshold,
        ptt_key,
        _ptt_stop: ptt_stop_tx,
        share: None,
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
fn spawn_playback(
    track: livekit::track::RemoteAudioTrack,
    output_device: Option<String>,
    mut status: VoiceStatusSignal,
    gain: Arc<AtomicU32>,
    master: Arc<AtomicU32>,
    deafened: Arc<AtomicBool>,
) -> std_mpsc::Sender<()> {
    let (stop_tx, stop_rx) = std_mpsc::channel::<()>();

    // Shared buffer between the LiveKit frame reader and the cpal callback.
    let buffer: Arc<Mutex<std::collections::VecDeque<i16>>> =
        Arc::new(Mutex::new(std::collections::VecDeque::new()));

    // Reader: LiveKit frames -> buffer (48kHz stereo, resampled by libwebrtc).
    let reader_buffer = buffer.clone();
    let rtc_track = track.rtc_track();
    let reader = tokio::spawn(async move {
        let mut stream = NativeAudioStream::new(rtc_track, 48000, 2);
        while let Some(frame) = stream.next().await {
            let mut buf = reader_buffer.lock().unwrap();
            buf.extend(frame.data.iter().copied());
            // Cap latency: drop oldest samples beyond ~250ms.
            let cap = 48000 / 2;
            let extra = buf.len().saturating_sub(cap);
            if extra > 0 {
                buf.drain(..extra);
            }
        }
    });

    std::thread::spawn(move || {
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
        let cb_buffer = buffer.clone();
        let stream = device.build_output_stream(
            config,
            move |out: &mut [f32], _: &_| {
                let g = if deafened.load(Ordering::Relaxed) {
                    0.0
                } else {
                    f32::from_bits(gain.load(Ordering::Relaxed))
                        * f32::from_bits(master.load(Ordering::Relaxed))
                };
                let mut buf = cb_buffer.lock().unwrap();
                for sample in out.iter_mut() {
                    let s = buf.pop_front().map(|s| s as f32 / 32768.0).unwrap_or(0.0);
                    *sample = (s * g).clamp(-1.0, 1.0);
                }
            },
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
        reader.abort();
    });

    stop_tx
}

