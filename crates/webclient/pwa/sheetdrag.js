// Bottom sheets follow a finger down, and let go past a point they close.
// switchb: "those draggable looking windows that popup are never draggable".
// They had a grip and nothing behind it.
//
// One delegated listener for every sheet, rather than touch handlers in each
// of the app's sheets: every .sheet is rendered straight after the .scrim
// that closes it, so closing is a click on that scrim, and the app keeps the
// say over whether it actually closes (a photo mid-upload keeps its sheet,
// and this puts it back).
(() => {
  const CLOSE_PX = 90; // dragged this far: close
  const FLICK = 0.6; // or flicked this fast (px/ms) over a short distance
  let drag = null;

  // Something between the finger and the sheet that is itself scrolled down
  // wants the finger for scrolling back up, not for closing.
  function scrolledBetween(target, sheet) {
    for (let el = target; el && el !== sheet.parentElement; el = el.parentElement) {
      if (el.scrollTop > 0) return true;
    }
    return false;
  }

  function scrimOf(sheet) {
    const prev = sheet.previousElementSibling;
    return prev && prev.classList.contains("scrim") ? prev : null;
  }

  function settle(sheet, scrim) {
    sheet.classList.add("settling");
    sheet.style.transform = "";
    if (scrim) {
      scrim.style.transition = "opacity 0.2s";
      scrim.style.opacity = "";
    }
    setTimeout(() => sheet.classList.remove("settling"), 220);
  }

  document.addEventListener(
    "touchstart",
    (e) => {
      drag = null;
      const target = e.target instanceof Element ? e.target : null;
      const sheet = target && target.closest(".sheet");
      if (!sheet || e.touches.length !== 1) return;
      // Never take a touch meant for typing.
      if (target.closest("input, textarea, select, [contenteditable]")) return;
      const t = e.touches[0];
      drag = {
        sheet,
        target,
        x0: t.clientX,
        y0: t.clientY,
        t0: 0,
        dy: 0,
        active: false,
        // The grip and the heading always drag, however the list is scrolled.
        onGrip: !!target.closest(".sheet-grip, .sheet-head, .sheet-preview"),
      };
    },
    { passive: true, capture: true },
  );

  document.addEventListener(
    "touchmove",
    (e) => {
      if (!drag || e.touches.length !== 1) return;
      const t = e.touches[0];
      if (!drag.active) {
        const dx = t.clientX - drag.x0;
        const dy = t.clientY - drag.y0;
        if (Math.abs(dx) < 8 && Math.abs(dy) < 8) return;
        // Sideways, upwards, or a list with somewhere to scroll back to:
        // an ordinary scroll, left alone.
        if (Math.abs(dx) > Math.abs(dy) || dy < 0 || (!drag.onGrip && scrolledBetween(drag.target, drag.sheet))) {
          drag = null;
          return;
        }
        drag.active = true;
        drag.y0 = t.clientY;
        drag.t0 = performance.now();
        drag.sheet.classList.add("dragging");
      }
      drag.dy = Math.max(0, t.clientY - drag.y0);
      drag.sheet.style.transform = `translateY(${drag.dy}px)`;
      const scrim = scrimOf(drag.sheet);
      if (scrim) {
        scrim.style.transition = "none";
        scrim.style.opacity = String(Math.max(0.2, 1 - drag.dy / Math.max(1, drag.sheet.offsetHeight)));
      }
      if (e.cancelable) e.preventDefault();
    },
    { passive: false, capture: true },
  );

  function end(cancelled) {
    const d = drag;
    drag = null;
    if (!d || !d.active) return;
    const sheet = d.sheet;
    const scrim = scrimOf(sheet);
    sheet.classList.remove("dragging");
    const speed = d.dy / Math.max(1, performance.now() - d.t0);
    const close = !cancelled && scrim && (d.dy > CLOSE_PX || (speed > FLICK && d.dy > 24));
    if (!close) {
      settle(sheet, scrim);
      return;
    }
    sheet.classList.add("settling");
    sheet.style.transform = "translateY(100%)";
    scrim.style.transition = "opacity 0.18s";
    scrim.style.opacity = "0";
    setTimeout(() => {
      scrim.click();
      // Still here a moment later: the app said no. Put it back.
      setTimeout(() => {
        if (sheet.isConnected) settle(sheet, scrim);
      }, 150);
    }, 170);
  }

  document.addEventListener("touchend", () => end(false), { passive: true, capture: true });
  document.addEventListener("touchcancel", () => end(true), { passive: true, capture: true });
})();
