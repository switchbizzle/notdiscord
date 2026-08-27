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
        let Some(handle) =
            rfd::AsyncFileDialog::new().set_file_name(&suggested).save_file().await
        else {
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
