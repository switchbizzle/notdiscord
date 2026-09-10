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

  // Take the file out of an <input type=file>, describe it, and clear the
  // input so choosing the same file again still fires its change event.
  // Returns "" when nothing was picked (the dialog was cancelled).
  function stage(inputId) {
    const input = document.getElementById(inputId);
    const picked = input && input.files && input.files[0];
    if (!picked) return "";
    discard();
    file = picked;
    try { input.value = ""; } catch (_) {}
    const type = file.type || "";
    const kind = type.startsWith("image/") ? "image" : type.startsWith("video/") ? "video" : "file";
    if (kind !== "file") objectUrl = URL.createObjectURL(file);
    return JSON.stringify({ name: file.name, size: file.size, kind, url: objectUrl });
  }

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

  return { stage, send, progress, discard };
})();
