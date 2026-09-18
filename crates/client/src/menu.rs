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

// ---------- spelling ----------

/// The Windows spell checker — the same engine WebView2 draws the squiggles
/// with, so the menu's opinion and the underline can't disagree. One checker
/// per thread, made lazily; every call degrades to "no opinion" rather than
/// an error, because a menu that won't open is worse than one without
/// suggestions.
#[cfg(windows)]
mod spell {
    use std::cell::RefCell;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Globalization::{
        GetUserDefaultLocaleName, ISpellChecker, ISpellCheckerFactory, ISpellingError,
        SpellCheckerFactory,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };

    thread_local! {
        // Outer Option: "have we tried"; inner: "did it work".
        static CHECKER: RefCell<Option<Option<ISpellChecker>>> = const { RefCell::new(None) };
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn make() -> Option<ISpellChecker> {
        unsafe {
            // The webview's thread is already an apartment; joining it again
            // is a no-op and RPC_E_CHANGED_MODE just means "already one".
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let factory: ISpellCheckerFactory =
                CoCreateInstance(&SpellCheckerFactory, None, CLSCTX_INPROC_SERVER).ok()?;
            // The user's own language first, English as the fallback.
            let mut buf = [0u16; 85];
            let len = GetUserDefaultLocaleName(&mut buf);
            let mut langs: Vec<Vec<u16>> = Vec::new();
            if len > 1 {
                langs.push(buf[..len as usize].to_vec());
            }
            langs.push(wide("en-US"));
            for lang in langs {
                let p = PCWSTR(lang.as_ptr());
                if factory.IsSupported(p).map(|b| b.as_bool()).unwrap_or(false) {
                    if let Ok(checker) = factory.CreateSpellChecker(p) {
                        return Some(checker);
                    }
                }
            }
            None
        }
    }

    fn with<T>(f: impl FnOnce(&ISpellChecker) -> Option<T>) -> Option<T> {
        CHECKER.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_none() {
                *slot = Some(make());
            }
            slot.as_ref().unwrap().as_ref().and_then(f)
        })
    }

    pub fn misspelled(word: &str) -> bool {
        with(|c| unsafe {
            let w = wide(word);
            let errors = c.Check(PCWSTR(w.as_ptr())).ok()?;
            // Any spelling error in a single word means the word is one.
            let mut first: Option<ISpellingError> = None;
            let hr = errors.Next(&mut first);
            Some(hr.is_ok() && first.is_some())
        })
        .unwrap_or(false)
    }

    /// Up to five corrections, best first, never echoing the word back.
    pub fn suggestions(word: &str) -> Vec<String> {
        with(|c| unsafe {
            let w = wide(word);
            let iter = c.Suggest(PCWSTR(w.as_ptr())).ok()?;
            let mut out = Vec::new();
            loop {
                let mut item = [PWSTR::null()];
                let mut got = 0u32;
                let hr = iter.Next(&mut item, Some(&mut got));
                if !hr.is_ok() || got == 0 {
                    break;
                }
                if let Ok(s) = item[0].to_string() {
                    if s != word {
                        out.push(s);
                    }
                }
                CoTaskMemFree(Some(item[0].0 as _));
                if out.len() >= 5 {
                    break;
                }
            }
            Some(out)
        })
        .unwrap_or_default()
    }

    /// Into the user's own Windows dictionary — the squiggle goes everywhere,
    /// not just here.
    pub fn learn(word: &str) {
        let _ = with(|c| unsafe {
            let w = wide(word);
            c.Add(PCWSTR(w.as_ptr())).ok()
        });
    }
}

#[cfg(not(windows))]
mod spell {
    pub fn misspelled(_: &str) -> bool {
        false
    }
    pub fn suggestions(_: &str) -> Vec<String> {
        Vec::new()
    }
    pub fn learn(_: &str) {}
}

// ---------- text fields ----------

/// Right-click on a text field: Cut/Copy/Paste/Select all — and when the
/// click landed on a misspelled word in a field that spellchecks, the
/// checker's corrections above them, plus Add to dictionary (switchb: "right
/// click an underlined word and it has the suggestions"). Chromium moves the
/// caret to the right-click spot before contextmenu fires, so the field's own
/// selectionStart is the word the pointer meant. Fields with
/// spellcheck="false" — names, codes — skip straight to the plain four.
pub fn open_for_text_field(mut menu: MenuSignal, event: &Event<MouseData>) {
    event.prevent_default();
    event.stop_propagation();
    let at = event.client_coordinates();
    spawn(async move {
        let mut eval = dioxus::document::eval(
            "const el = document.activeElement;
             if (!el || (el.tagName !== 'INPUT' && el.tagName !== 'TEXTAREA') || !el.spellcheck
                 || el.selectionStart !== el.selectionEnd) { dioxus.send(''); }
             else {
                 const v = el.value, s = el.selectionStart;
                 const isW = (ch) => /[\\p{L}\\p{M}'\u{2019}]/u.test(ch);
                 let a = s, b = s;
                 while (a > 0 && isW(v[a - 1])) a--;
                 while (b < v.length && isW(v[b])) b++;
                 dioxus.send(JSON.stringify({ w: v.slice(a, b), a: a, b: b, prev: a > 0 ? v[a - 1] : '' }));
             }",
        );
        let raw = eval.recv::<String>().await.unwrap_or_default();
        let mut items: Vec<Item> = Vec::new();
        if let Ok(hit) = serde_json::from_str::<serde_json::Value>(&raw) {
            let word = hit["w"].as_str().unwrap_or("").to_string();
            let (a, b) = (hit["a"].as_u64().unwrap_or(0), hit["b"].as_u64().unwrap_or(0));
            let prev = hit["prev"].as_str().unwrap_or("");
            // A @mention, #channel or :emoji: is a name, not a typo.
            if word.chars().count() >= 2 && !matches!(prev, "@" | "#" | ":") && spell::misspelled(&word) {
                for s in spell::suggestions(&word) {
                    let text = s.clone();
                    items.push(item(s, "", move || replace_field_range(a, b, text.clone())));
                }
                let learned = word.clone();
                items.push(item(format!("Add \u{201c}{word}\u{201d} to dictionary"), "plus", move || {
                    spell::learn(&learned)
                }));
            }
        }
        items.extend(text_field_items());
        menu.set(Some(Menu { x: at.x, y: at.y, items }));
    });
}

/// Replace [a, b) — UTF-16 offsets, the units the field itself counts in —
/// through insertText, so the input event fires and the Rust-side signal
/// follows. The same route a paste takes.
fn replace_field_range(a: u64, b: u64, text: String) {
    let literal = serde_json::to_string(&text).unwrap_or_else(|_| "\"\"".into());
    dioxus::document::eval(&format!(
        "const el = document.activeElement;
         if (el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA')) {{
             el.focus();
             el.setSelectionRange({a}, {b});
             document.execCommand('insertText', false, {literal});
         }}"
    ));
}

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

    /// The real Windows checker, asked directly. `learn` is deliberately not
    /// tested: it would write into the developer's actual custom dictionary.
    #[cfg(windows)]
    #[test]
    fn the_spell_checker_knows_a_typo_and_offers_the_fix() {
        assert!(spell::misspelled("helllo"));
        assert!(!spell::misspelled("hello"));
        let s = spell::suggestions("helllo");
        assert!(!s.is_empty());
        assert!(s.iter().any(|w| w.eq_ignore_ascii_case("hello")), "{s:?}");
    }

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
