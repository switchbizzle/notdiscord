// Upload glue. Like voice.js and push.js, the wasm app calls in and polls
// the result back out — nothing async crosses the boundary except the one
// promise for the request itself.
//
// The file never enters wasm. The app hands over the id of the <input> it
// was picked with; this takes the File out, makes a preview URL, and later
// streams the same File straight from disk into an XMLHttpRequest. XHR
// rather than fetch() because fetch has no upload progress, and a 12 MB
// photo going up on mobile data with nothing on screen for ten seconds is
// exactly what "it didn't send" looks like.
window.ndUpload = (() => {
  let file = null;
  let objectUrl = "";
  let sent = 0;
  let total = 0;
  let xhr = null;

  function discard() {
    if (objectUrl) {
      URL.revokeObjectURL(objectUrl);
      objectUrl = "";
    }
    file = null;
    sent = 0;
    total = 0;
    if (xhr) {
      try { xhr.abort(); } catch (_) {}
      xhr = null;
    }
  }

  // The staged file's description, kept so a paste/drop can hand it to the
  // app after the fact.
  let lastInfo = "";

  function describe(picked) {
    discard();
    file = picked;
    const type = file.type || "";
    const kind = type.startsWith("image/") ? "image" : type.startsWith("video/") ? "video" : "file";
    if (kind !== "file") objectUrl = URL.createObjectURL(file);
    // A pasted screenshot has no name worth keeping ("image.png"); date it
    // so two in a row don't look like the same file.
    let name = picked.name || "";
    if (!name) name = "pasted-" + Date.now() + (type === "image/png" ? ".png" : "");
    lastInfo = JSON.stringify({ name, size: file.size, kind, url: objectUrl });
    return lastInfo;
  }

  // Take the file out of an <input type=file>, describe it, and clear the
  // input so choosing the same file again still fires its change event.
  // Returns "" when nothing was picked (the dialog was cancelled).
  function stage(inputId) {
    const input = document.getElementById(inputId);
    const picked = input && input.files && input.files[0];
    if (!picked) return "";
    const info = describe(picked);
    try { input.value = ""; } catch (_) {}
    return info;
  }

  // What's staged right now — how the app reads back a paste or a drop.
  function stagedInfo() {
    return file ? lastInfo : "";
  }

  // Paste a screenshot (or a file copied in Explorer) or drop one anywhere
  // while a channel is open: stage it exactly as if it had been picked,
  // then tap the app's hidden hook so the preview sheet opens — the page
  // talks to the app by clicking the app's own elements, the same trick
  // sheetdrag.js pulls on the scrim.
  function stageExternal(picked) {
    describe(picked);
    const hook = document.getElementById("nd-staged-hook");
    if (hook) hook.click();
  }
  document.addEventListener("paste", (e) => {
    const files = e.clipboardData && e.clipboardData.files;
    if (!files || !files.length) return;
    // No composer on screen means nowhere to send it.
    if (!document.getElementById("nd-composer")) return;
    e.preventDefault();
    stageExternal(files[0]);
  });
  document.addEventListener("dragover", (e) => {
    // preventDefault is what makes the page a drop target at all.
    if (e.dataTransfer && Array.from(e.dataTransfer.types).includes("Files")) e.preventDefault();
  });
  document.addEventListener("drop", (e) => {
    const files = e.dataTransfer && e.dataTransfer.files;
    if (!files || !files.length) return;
    if (!document.getElementById("nd-composer")) return;
    e.preventDefault();
    stageExternal(files[0]);
  });

  // 0 to 1. Polled by the app while a send is in flight.
  function progress() {
    return total ? sent / total : 0;
  }

  // Resolves with the server's response body; rejects with a message fit
  // to show. The staged file is kept either way, so a failed send can be
  // retried without picking it again.
  function send(url, token) {
    return new Promise((resolve, reject) => {
      if (!file) {
        reject("nothing to send");
        return;
      }
      const req = new XMLHttpRequest();
      xhr = req;
      req.open("POST", url);
      req.setRequestHeader("Authorization", "Bearer " + token);
      req.upload.onprogress = (e) => {
        if (e.lengthComputable) {
          sent = e.loaded;
          total = e.total;
        }
      };
      req.onload = () => {
        if (xhr === req) xhr = null;
        const body = req.responseText || "";
        if (req.status >= 200 && req.status < 300) {
          sent = total;
          resolve(body);
          return;
        }
        let message = "upload failed (" + req.status + ")";
        try { message = JSON.parse(body).error || message; } catch (_) {}
        reject(message);
      };
      req.onerror = () => {
        if (xhr === req) xhr = null;
        reject("upload failed — check the connection");
      };
      req.onabort = () => {
        if (xhr === req) xhr = null;
        reject("cancelled");
      };
      total = file.size;
      sent = 0;
      req.send(file);
    });
  }

  return { stage, stagedInfo, send, progress, discard };
})();
