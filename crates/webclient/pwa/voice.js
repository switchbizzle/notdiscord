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
    participants: [], // {identity, name, speaking, local}
  };
  const audioEls = new Map();

  function refresh() {
    if (!room) {
      state.participants = [];
      return;
    }
    const parts = [];
    const add = (p, local) =>
      parts.push({
        identity: p.identity,
        name: p.name || p.identity,
        speaking: !!p.isSpeaking,
        local,
      });
    add(room.localParticipant, true);
    room.remoteParticipants.forEach((p) => add(p, false));
    state.participants = parts;
  }

  async function join(url, token) {
    if (room) await leave();
    state.connecting = true;
    state.error = "";
    try {
      const r = new LivekitClient.Room({ adaptiveStream: true, dynacast: true });
      r.on("trackSubscribed", (track, pub) => {
        if (track.kind === "audio") {
          const el = track.attach();
          el.style.display = "none";
          document.body.appendChild(el);
          audioEls.set(pub.trackSid, el);
        }
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

  function getState() {
    refresh();
    return JSON.stringify(state);
  }

  // _room is for diagnostics (dev tooling publishes synthetic tracks
  // through it); not part of the app's API.
  return { join, leave, setMuted, getState, _room: () => room };
})();
