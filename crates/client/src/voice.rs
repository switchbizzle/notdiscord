//! Voice engine: connects to a LiveKit room, publishes the microphone, and
//! plays back remote participants' audio.
//!
//! cpal streams are !Send, so each audio device stream lives on its own OS
//! thread; those threads exit when their stop-channel sender is dropped
//! (i.e. when the `ActiveCall` owning it is dropped).

use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use dioxus::prelude::*;
use futures_util::StreamExt;
use livekit::options::TrackPublishOptions;
use livekit::track::{LocalAudioTrack, LocalTrack, RemoteTrack, TrackSource};
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
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct VoiceStatus {
    pub channel_id: Option<i64>,
    pub channel_name: String,
    pub connecting: bool,
    pub muted: bool,
    pub participants: Vec<VoiceParticipant>,
    pub error: String,
}

pub enum VoiceCmd {
    Join { channel_id: i64, channel_name: String, url: String, token: String },
    Leave,
    ToggleMute,
}

/// Thread-safe signal: voice status is updated from tokio worker tasks.
pub type VoiceStatusSignal = Signal<VoiceStatus, SyncStorage>;

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
        status.write().error = format!("mic stream error: {e}");
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
    event_task: tokio::task::JoinHandle<()>,
}

impl Drop for ActiveCall {
    fn drop(&mut self) {
        self.event_task.abort();
        self.playback_stops.lock().unwrap().clear();
    }
}

pub async fn voice_task(mut rx: UnboundedReceiver<VoiceCmd>, mut status: VoiceStatusSignal) {
    let mut call: Option<ActiveCall> = None;

    while let Some(cmd) = rx.next().await {
        match cmd {
            VoiceCmd::Join { channel_id, channel_name, url, token } => {
                // Leave any current room first.
                if let Some(old) = call.take() {
                    old.room.close().await.ok();
                }
                status.set(VoiceStatus {
                    channel_id: Some(channel_id),
                    channel_name: channel_name.clone(),
                    connecting: true,
                    ..Default::default()
                });
                match connect(&url, &token, status).await {
                    Ok(active) => {
                        call = Some(active);
                        status.write().connecting = false;
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
                if let Some(old) = call.take() {
                    old.room.close().await.ok();
                }
                status.set(VoiceStatus::default());
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
        }
    }
}

async fn connect(url: &str, token: &str, status: VoiceStatusSignal) -> anyhow::Result<ActiveCall> {
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

    let source = NativeAudioSource::new(
        AudioSourceOptions {
            echo_cancellation: true,
            noise_suppression: true,
            auto_gain_control: true,
        },
        sample_rate,
        channels,
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

    // Pump captured samples into the LiveKit source in 10ms frames.
    let samples_per_frame = (sample_rate / 100 * channels) as usize;
    tokio::spawn(async move {
        let mut buffer: Vec<i16> = Vec::with_capacity(samples_per_frame * 4);
        while let Some(chunk) = frame_rx.recv().await {
            buffer.extend_from_slice(&chunk);
            while buffer.len() >= samples_per_frame {
                let data: Vec<i16> = buffer.drain(..samples_per_frame).collect();
                let frame = AudioFrame {
                    data: data.into(),
                    sample_rate,
                    num_channels: channels,
                    samples_per_channel: (samples_per_frame as u32) / channels,
                };
                if source.capture_frame(&frame).await.is_err() {
                    return;
                }
            }
        }
    });

    // ---- Room events: participants, remote audio playback ----
    let playback_stops: Arc<Mutex<HashMap<String, std_mpsc::Sender<()>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let stops = playback_stops.clone();
    let room_handle = room.clone();
    let output_device = settings.output_device.clone();
    let event_task = tokio::spawn(async move {
        let mut events = events;
        refresh_participants(&room_handle, status);
        while let Some(event) = events.recv().await {
            match event {
                RoomEvent::TrackSubscribed { track, participant, .. } => {
                    if let RemoteTrack::Audio(audio) = track {
                        let sid = audio.sid().to_string();
                        let stop = spawn_playback(audio, output_device.clone(), status);
                        stops.lock().unwrap().insert(sid, stop);
                    }
                    let _ = participant;
                    refresh_participants(&room_handle, status);
                }
                RoomEvent::TrackUnsubscribed { track, .. } => {
                    if let RemoteTrack::Audio(audio) = track {
                        stops.lock().unwrap().remove(&audio.sid().to_string());
                    }
                    refresh_participants(&room_handle, status);
                }
                RoomEvent::ParticipantConnected(_)
                | RoomEvent::ParticipantDisconnected(_)
                | RoomEvent::Connected { .. } => refresh_participants(&room_handle, status),
                RoomEvent::ActiveSpeakersChanged { speakers } => {
                    let speaking: Vec<String> =
                        speakers.iter().map(|p| p.identity().to_string()).collect();
                    let mut s = status;
                    let mut st = s.write();
                    for p in st.participants.iter_mut() {
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

    Ok(ActiveCall { room, mic_publication, _mic_stop: mic_stop_tx, playback_stops, event_task })
}

fn refresh_participants(room: &Room, status: VoiceStatusSignal) {
    let mut list: Vec<VoiceParticipant> = Vec::new();
    let me = room.local_participant();
    list.push(VoiceParticipant {
        identity: me.identity().to_string(),
        name: me.name().to_string(),
        speaking: false,
    });
    for (_, p) in room.remote_participants() {
        list.push(VoiceParticipant {
            identity: p.identity().to_string(),
            name: p.name().to_string(),
            speaking: false,
        });
    }
    list.sort_by(|a, b| a.name.cmp(&b.name));
    let mut s = status;
    s.write().participants = list;
}

/// Play one remote audio track on the configured (or default) output device.
/// Returns a stop handle; dropping it ends the playback thread.
fn spawn_playback(
    track: livekit::track::RemoteAudioTrack,
    output_device: Option<String>,
    mut status: VoiceStatusSignal,
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
                let mut buf = cb_buffer.lock().unwrap();
                for sample in out.iter_mut() {
                    *sample = buf.pop_front().map(|s| s as f32 / 32768.0).unwrap_or(0.0);
                }
            },
            move |e| {
                let mut s = status;
                s.write().error = format!("audio output error: {e}");
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

fn tracing_log(msg: &str) {
    eprintln!("[voice] {msg}");
}
