// Copying a picture itself, not its address (switchb: "when you view an
// image you cant copy it. you can only download it").
//
// The clipboard takes image/png and little else, so whatever the picture is
// (a JPEG from a camera, a WebP, a GIF's first frame) is redrawn on a canvas
// and handed over as PNG. The ClipboardItem is built straight away with a
// promise of the PNG rather than after it's ready: Safari only allows a
// clipboard write that starts inside the tap, and the redraw takes longer
// than that lasts.
window.ndClip = (() => {
  function pngOf(url) {
    return new Promise((resolve, reject) => {
      const img = new Image();
      // Pictures from elsewhere (GIPHY) draw on a canvas only with CORS.
      img.crossOrigin = "anonymous";
      img.onload = () => {
        const canvas = document.createElement("canvas");
        canvas.width = img.naturalWidth;
        canvas.height = img.naturalHeight;
        canvas.getContext("2d").drawImage(img, 0, 0);
        canvas.toBlob((blob) => (blob ? resolve(blob) : reject(new Error("encode"))), "image/png");
      };
      img.onerror = () => reject(new Error("load"));
      img.src = url;
    });
  }

  // "ok", "unsupported" (no image clipboard in this browser, or not a
  // secure page), or "failed".
  async function copyImage(url) {
    if (!window.isSecureContext || !navigator.clipboard || !navigator.clipboard.write || typeof ClipboardItem === "undefined") {
      return "unsupported";
    }
    try {
      await navigator.clipboard.write([new ClipboardItem({ "image/png": pngOf(url) })]);
      return "ok";
    } catch (e) {
      return "failed";
    }
  }

  return { copyImage };
})();
