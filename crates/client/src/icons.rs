//! UI icons: inline SVGs generated at build time from assets/icons/.
//! They stroke with `currentColor`, so they inherit the surrounding text
//! color and hover states like any glyph would.

use dioxus::prelude::*;

include!(concat!(env!("OUT_DIR"), "/icons_gen.rs"));

#[component]
pub fn Icon(name: &'static str, #[props(default = 18)] size: u32) -> Element {
    rsx! {
        span {
            class: "icon",
            style: "width: {size}px; height: {size}px",
            dangerous_inner_html: icon_svg(name),
        }
    }
}
