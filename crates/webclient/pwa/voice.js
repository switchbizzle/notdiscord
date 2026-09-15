// NotDiscord voice glue: wraps livekit-client (UMD, loaded alongside) behind
// a tiny API the wasm app calls. State flows back by polling getState() —
// no closures across the wasm boundary.
window.ndVoice = (() => {
  let room = null;
  const state = {
    connected: false,
    connecting: false,
    error: "",
    muted: false,
    deafened: false,
    // Publishing our own picture, and whether this browser can even offer a
    // screen (phones can't: getDisplayMedia is desktop-only).
    camera_on: false,
    sharing_on: false,
    can_share: !!(navigator.mediaDevices && navigator.mediaDevices.getDisplayMedia),
    participants: [], // {identity, name, speaking, local, camera, sharing}
  };
  const audioEls = new Map();

  function refresh() {
    if (!room) {
      state.participants = [];
      state.camera_on = false;
      state.sharing_on = false;
      return;
    }
    const parts = [];
    const add = (p, local) =>
      parts.push({
        identity: p.identity,
        name: p.name || p.identity,
        speaking: !!p.isSpeaking,
        local,
        camera: !!p.isCameraEnabled,
        sharing: !!p.isScreenShareEnabled,
      });
    add(room.localParticipant, true);
    room.remoteParticipants.forEach((p) => add(p, false));
    state.participants = parts;
    // Read back from the room rather than remembered: the browser's own
    // "Stop sharing" button ends a share without asking us, and the state
    // has to follow it or the app's button lies.
    state.camera_on = !!room.localParticipant.isCameraEnabled;
    state.sharing_on = !!room.localParticipant.isScreenShareEnabled;
  }

  // Wire every <video data-nd-vid="identity|source"> the app has rendered to
  // the matching live track. Called on the app's poll: declarative and
  // self-healing, so a tile that rendered before its track arrived (or a
  // track that arrived before its tile) meets its partner within a beat.
  function attachVideos() {
    if (!room) return;
    document.querySelectorAll("video[data-nd-vid]").forEach((el) => {
      const at = el.dataset.ndVid.lastIndexOf("|");
      const identity = el.dataset.ndVid.slice(0, at);
      const source = el.dataset.ndVid.slice(at + 1);
      const p =
        room.localParticipant.identity === identity
          ? room.localParticipant
          : room.remoteParticipants.get(identity);
      if (!p) return;
      let pub = null;
      p.videoTrackPublications.forEach((tp) => {
        if (tp.source === source) pub = tp;
      });
      const track = pub && pub.track;
      if (!track) {
        if (el.dataset.ndSid) {
          el.srcObject = null;
          delete el.dataset.ndSid;
        }
        return;
      }
      if (el.dataset.ndSid === pub.trackSid) return;
      track.attach(el);
      // Sound rides the separate audio elements; the picture stays silent.
      el.muted = true;
      el.dataset.ndSid = pub.trackSid;
    });
  }

  async function join(url, token) {
    if (room) await leave();
    state.connecting = true;
    state.error = "";
    try {
      // webAudioMix routes remote audio through an AudioContext, which is
      // the only way the per-friend volume works on a phone: without it
      // livekit falls back to setting element.volume, and Chrome on Android
      // ignores that outright — the slider moved and nothing happened
      // (switchb). A gain node is honoured everywhere.
      const r = new LivekitClient.Room({
        adaptiveStream: true,
        dynacast: true,
        webAudioMix: true,
      });
      r.on("trackSubscribed", (track, pub, participant) => {
        if (track.kind === "audio") {
          const el = track.attach();
          el.style.display = "none";
          el.muted = state.deafened;
          document.body.appendChild(el);
          audioEls.set(pub.trackSid, el);
          // A saved per-friend volume applies as soon as their track lands.
          const v = volumes[participant.identity];
          if (v !== undefined && track.setVolume) track.setVolume(v);
        }
        // Video tracks wait in the room; attachVideos() pairs them with
        // whatever elements the app renders.
      });
      r.on("trackUnsubscribed", (track, pub) => {
        const el = audioEls.get(pub.trackSid);
        if (el) {
          track.detach(el);
          el.remove();
          audioEls.delete(pub.trackSid);
        }
      });
      ["participantConnected", "participantDisconnected", "activeSpeakersChanged"].forEach((ev) =>
        r.on(ev, refresh),
      );
      r.on("disconnected", () => {
        state.connected = false;
      });
      await r.connect(url, token);
      room = r;
      state.connected = true;
      // Publish the mic; without permission we stay connected listen-only.
      try {
        await r.localParticipant.setMicrophoneEnabled(true);
        state.muted = false;
      } catch (e) {
        state.muted = true;
        state.error = "mic unavailable — listening only";
      }
      // Mobile browsers hold audio until a gesture; the join tap is one.
      try {
        await r.startAudio();
      } catch (e) {}
      refresh();
    } catch (e) {
      state.error = "voice connect failed: " + (e && e.message ? e.message : e);
      room = null;
      state.connected = false;
    }
    state.connecting = false;
  }

  async function leave() {
    if (room) {
      const r = room;
      room = null;
      try {
        await r.disconnect();
      } catch (e) {}
    }
    audioEls.forEach((el) => el.remove());
    audioEls.clear();
    state.connected = false;
    state.deafened = false;
    state.camera_on = false;
    state.sharing_on = false;
    state.participants = [];
    state.error = "";
  }

  async function setMuted(m) {
    if (!room) return;
    try {
      await room.localParticipant.setMicrophoneEnabled(!m);
      state.muted = m;
    } catch (e) {
      state.error = "mic toggle failed: " + (e && e.message ? e.message : e);
    }
  }

  // Deafen: silence every attached remote element (and future ones), and
  // mute the mic too — deafen implies muted, like every voice app.
  async function setDeafened(d) {
    state.deafened = d;
    audioEls.forEach((el) => (el.muted = d));
    if (d && !state.muted) await setMuted(true);
  }

  async function setCamera(on) {
    if (!room) return;
    try {
      await room.localParticipant.setCameraEnabled(on);
    } catch (e) {
      state.error = on
        ? "camera unavailable — is it allowed for this site?"
        : "camera wouldn't stop: " + (e && e.message ? e.message : e);
    }
    refresh();
  }

  // Returns "ok", "cancelled" (the person closed the browser's picker —
  // their decision, not an error) or "failed".
  async function setShare(on) {
    if (!room) return "failed";
    try {
      // Tab/system audio goes with the picture where the browser offers it.
      await room.localParticipant.setScreenShareEnabled(on, { audio: true });
    } catch (e) {
      refresh();
      if (e && e.name === "NotAllowedError") return "cancelled";
      state.error = "screen share failed: " + (e && e.message ? e.message : e);
      return "failed";
    }
    refresh();
    return "ok";
  }

  // Per-friend volume, 0..2 (>1 boosts via the track's own gain).
  function setVolume(identity, v) {
    if (!room) return;
    const p = room.remoteParticipants.get(identity);
    if (!p) return;
    p.audioTrackPublications.forEach((pub) => {
      if (pub.track && pub.track.setVolume) pub.track.setVolume(v);
    });
    volumes[identity] = v;
  }
  const volumes = {};

  function getState() {
    refresh();
    return JSON.stringify(state);
  }

  // _room is for diagnostics (dev tooling publishes synthetic tracks
  // through it); not part of the app's API.
  return {
    join,
    leave,
    setMuted,
    setDeafened,
    setCamera,
    setShare,
    setVolume,
    attachVideos,
    getState,
    _room: () => room,
  };
})();
