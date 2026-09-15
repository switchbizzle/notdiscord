//! Right-click menus.
//!
//! The webview's own menu (Back / Reload / Save as / Print) is switched off in
//! `main`, because none of it means anything in a chat app. What replaces it is
//! per-element: a message offers Reply and Copy text, an image offers Save
//! image as, a text field offers Cut/Copy/Paste. Anything with nothing useful
//! to offer opens no menu at all, which is quieter than a menu of dead entries.

use dioxus::prelude::*;
use std::rc::Rc;

use crate::icons::Icon;

/// One row. `run` fires on click, then the menu closes.
#[derive(Clone)]
pub struct Item {
    pub label: String,
    pub icon: &'static str,
    pub danger: bool,
    pub run: Rc<dyn Fn()>,
}

pub fn item(label: impl Into<String>, icon: &'static str, run: impl Fn() + 'static) -> Item {
    Item { label: label.into(), icon, danger: false, run: Rc::new(run) }
}

/// A row for something that destroys or removes; rendered in red.
pub fn danger(label: impl Into<String>, icon: &'static str, run: impl Fn() + 'static) -> Item {
    Item { label: label.into(), icon, danger: true, run: Rc::new(run) }
}

/// An open menu: where it sits and what's in it.
#[derive(Clone)]
pub struct Menu {
    pub x: f64,
    pub y: f64,
    pub items: Vec<Item>,
}

pub type MenuSignal = Signal<Option<Menu>>;

/// Open a menu at the pointer. An empty item list opens nothing — that's the
/// "no noise" rule: elements without relevant actions stay silent.
pub fn open(mut menu: MenuSignal, event: &Event<MouseData>, items: Vec<Item>) {
    event.prevent_default();
    event.stop_propagation();
    if items.is_empty() {
        menu.set(None);
        return;
    }
    let at = event.client_coordinates();
    menu.set(Some(Menu { x: at.x, y: at.y, items }));
}

/// The menu, rendered once near the app root. Plain function rather than a
/// component so the items can hold closures without needing `PartialEq`.
pub fn view(mut menu: MenuSignal) -> Element {
    let Some(current) = menu() else {
        return rsx! {};
    };
    // Keep it on screen: min() flips the anchor when the pointer is near an edge.
    let height = current.items.len() as f64 * 30.0 + 10.0;
    let style = format!(
        "left: min({:.0}px, calc(100vw - 232px)); top: min({:.0}px, calc(100vh - {height:.0}px))",
        current.x, current.y
    );

    rsx! {
        div {
            class: "ctx-backdrop",
            onclick: move |_| menu.set(None),
            oncontextmenu: move |e: Event<MouseData>| {
                e.prevent_default();
                menu.set(None);
            },
            div {
                class: "ctx-menu",
                style: "{style}",
                onclick: move |e: Event<MouseData>| e.stop_propagation(),
                for (i, entry) in current.items.into_iter().enumerate() {
                    {
                        let Item { label, icon, danger, run } = entry;
                        rsx! {
                            button {
                                key: "{i}",
                                class: if danger { "ctx-item danger" } else { "ctx-item" },
                                // Don't steal focus: the clipboard rows act on
                                // whatever text field was right-clicked.
                                onmousedown: move |e: Event<MouseData>| e.prevent_default(),
                                onclick: move |_| {
                                    run();
                                    menu.set(None);
                                },
                                if icon.is_empty() {
                                    span { class: "ctx-icon-gap" }
                                } else {
                                    Icon { name: icon, size: 15 }
                                }
                                span { class: "ctx-label", "{label}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ---------- clipboard ----------

pub fn copy_to_clipboard(text: String) {
    if let Ok(mut clipboard) = arboard::Clipboard::new() {
        let _ = clipboard.set_text(text);
    }
}

/// Put a picture itself on the clipboard, not its address — what pastes into
/// a message, Paint, or another chat as an image (switchb: "when you view an
/// image you cant copy it. you can only download it"). A GIF copies its
/// first frame: the Windows clipboard has no moving pictures.
pub async fn copy_image(url: String) -> Result<(), String> {
    let response = reqwest::get(&url).await.map_err(|_| "couldn't fetch the picture".to_string())?;
    if !response.status().is_success() {
        return Err(format!("couldn't fetch the picture ({})", response.status()));
    }
    let bytes = response.bytes().await.map_err(|_| "couldn't fetch the picture".to_string())?;
    tokio::task::spawn_blocking(move || {
        let img = image::load_from_memory(&bytes)
            .map_err(|_| "that kind of picture can't be copied".to_string())?
            .into_rgba8();
        let (width, height) = img.dimensions();
        let mut clipboard = arboard::Clipboard::new().map_err(|_| "the clipboard is busy".to_string())?;
        clipboard
            .set_image(arboard::ImageData {
                width: width as usize,
                height: height as usize,
                bytes: std::borrow::Cow::Owned(img.into_raw()),
            })
            .map_err(|_| "the clipboard is busy".to_string())
    })
    .await
    .map_err(|_| "copy failed".to_string())?
}

/// Copy what the user has highlighted, or `fallback` when nothing is.
/// Right-clicking a message after selecting part of it should copy the part.
pub fn copy_selection_or(fallback: String) {
    spawn(async move {
        let mut eval = dioxus::document::eval(
            "dioxus.send(window.getSelection ? window.getSelection().toString() : '');",
        );
        let selected = eval.recv::<String>().await.unwrap_or_default();
        copy_to_clipboard(if selected.trim().is_empty() { fallback } else { selected });
    });
}

/// Download `url` to wherever the user picks. The name from the URL is the
/// suggested filename, which is what the browser would offer too.
/// `photo.png` in a folder that already has one becomes `photo (1).png`.
/// Counts up rather than clobbering, and gives up politely if a folder is
/// somehow full of them.
fn unique_name(dir: &std::path::Path, name: &str) -> String {
    if !dir.join(name).exists() {
        return name.to_owned();
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    for n in 1..1000 {
        let candidate = format!("{stem} ({n}){ext}");
        if !dir.join(&candidate).exists() {
            return candidate;
        }
    }
    name.to_owned()
}

pub fn save_url_as(url: String) {
    spawn(async move {
        let suggested = url
            .rsplit('/')
            .next()
            .unwrap_or("download")
            .split(['?', '#'])
            .next()
            .unwrap_or("download")
            .to_owned();
        // Server files are named alike often enough that saving two in a row
        // meant an overwrite prompt every time (Jon). Offer a name that
        // doesn't collide instead of one that does.
        let dir = dirs::download_dir().unwrap_or_else(std::env::temp_dir);
        let suggested = unique_name(&dir, &suggested);
        let mut dialog = rfd::AsyncFileDialog::new().set_file_name(&suggested);
        if dir.is_dir() {
            dialog = dialog.set_directory(&dir);
        }
        let Some(handle) = dialog.save_file().await else {
            return;
        };
        let Ok(response) = reqwest::get(&url).await else { return };
        let Ok(bytes) = response.bytes().await else { return };
        let _ = handle.write(&bytes).await;
    });
}

// ---------- text fields ----------

/// Cut/Copy/Paste/Select all for the focused input, in that order. Every text
/// field gets the same four, because that's what a text field can do.
pub fn text_field_items() -> Vec<Item> {
    vec![
        item("Cut", "scissors", || field_copy(true)),
        item("Copy", "copy", || field_copy(false)),
        item("Paste", "clipboard", field_paste),
        item("Select all", "check-square", field_select_all),
    ]
}

/// Copy the field's selection (or the whole field when nothing is selected),
/// deleting it afterwards for a cut. The edit goes through `execCommand` so it
/// fires an `input` event and the Rust-side signal stays in step.
fn field_copy(cut: bool) {
    spawn(async move {
        let mut eval = dioxus::document::eval(&format!(
            "const el = document.activeElement;
             if (!el || (el.tagName !== 'INPUT' && el.tagName !== 'TEXTAREA')) {{ dioxus.send(''); }}
             else {{
                 const from = el.selectionStart, to = el.selectionEnd;
                 const whole = from === to;
                 dioxus.send(whole ? el.value : el.value.slice(from, to));
                 if ({cut}) {{
                     if (whole) {{ el.select(); }}
                     document.execCommand('delete');
                 }}
             }}"
        ));
        let text = eval.recv::<String>().await.unwrap_or_default();
        if !text.is_empty() {
            copy_to_clipboard(text);
        }
    });
}

fn field_paste() {
    spawn(async move {
        let text = tokio::task::spawn_blocking(|| {
            let mut clipboard = arboard::Clipboard::new().ok()?;
            clipboard.get_text().ok()
        })
        .await
        .ok()
        .flatten();
        let Some(text) = text else { return };
        // Through insertText so it lands at the caret and replaces any selection.
        let literal = serde_json::to_string(&text).unwrap_or_else(|_| "\"\"".into());
        dioxus::document::eval(&format!(
            "const el = document.activeElement;
             if (el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA')) {{
                 el.focus();
                 document.execCommand('insertText', false, {literal});
             }}"
        ));
    });
}

fn field_select_all() {
    dioxus::document::eval(
        "const el = document.activeElement; if (el && el.select) { el.focus(); el.select(); }",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saving_the_same_name_twice_counts_up_instead_of_clobbering() {
        let dir = std::env::temp_dir().join("nd-unique-name-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        // Nothing there yet: the name is used as-is.
        assert_eq!(unique_name(&dir, "photo.png"), "photo.png");

        std::fs::write(dir.join("photo.png"), b"x").unwrap();
        assert_eq!(unique_name(&dir, "photo.png"), "photo (1).png");

        std::fs::write(dir.join("photo (1).png"), b"x").unwrap();
        assert_eq!(unique_name(&dir, "photo.png"), "photo (2).png");

        // The extension survives, and a name without one still works.
        std::fs::write(dir.join("notes"), b"x").unwrap();
        assert_eq!(unique_name(&dir, "notes"), "notes (1)");

        // A dotfile has no stem to speak of; don't turn ".env" into " (1).env".
        std::fs::write(dir.join(".env"), b"x").unwrap();
        assert_eq!(unique_name(&dir, ".env"), ".env (1)");

        // Several dots keep everything but the last as the stem.
        std::fs::write(dir.join("clip.tar.gz"), b"x").unwrap();
        assert_eq!(unique_name(&dir, "clip.tar.gz"), "clip.tar (1).gz");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
