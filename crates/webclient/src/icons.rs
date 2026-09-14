//! The SAME icon files the desktop app compiles in (crates/client/assets/
//! icons), included straight from there — one source of truth, so the two
//! apps look like one app. They stroke with currentColor.

use dioxus::prelude::*;

pub fn icon_svg(name: &str) -> &'static str {
    match name {
        "menu" => include_str!("../../client/assets/icons/menu.svg"),
        "music" => include_str!("../../client/assets/icons/music.svg"),
        "shuffle" => include_str!("../../client/assets/icons/shuffle.svg"),
        "user" => include_str!("../../client/assets/icons/user.svg"),
        "send" => include_str!("../../client/assets/icons/send.svg"),
        "plus" => include_str!("../../client/assets/icons/plus.svg"),
        "play" => include_str!("../../client/assets/icons/play.svg"),
        "pause" => include_str!("../../client/assets/icons/pause.svg"),
        "skip" => include_str!("../../client/assets/icons/skip.svg"),
        "stop" => include_str!("../../client/assets/icons/stop.svg"),
        "x" => include_str!("../../client/assets/icons/x.svg"),
        "volume" => include_str!("../../client/assets/icons/volume.svg"),
        "mic" => include_str!("../../client/assets/icons/mic.svg"),
        "mic-off" => include_str!("../../client/assets/icons/mic-off.svg"),
        "phone" => include_str!("../../client/assets/icons/phone.svg"),
        "phone-off" => include_str!("../../client/assets/icons/phone-off.svg"),
        "headphones" => include_str!("../../client/assets/icons/headphones.svg"),
        "headphones-off" => include_str!("../../client/assets/icons/headphones-off.svg"),
        "reply" => include_str!("../../client/assets/icons/reply.svg"),
        "smile" => include_str!("../../client/assets/icons/smile.svg"),
        "file" => include_str!("../../client/assets/icons/file.svg"),
        "settings" => include_str!("../../client/assets/icons/settings.svg"),
        "check" => include_str!("../../client/assets/icons/check.svg"),
        "download" => include_str!("../../client/assets/icons/download.svg"),
        "edit" => include_str!("../../client/assets/icons/edit.svg"),
        "trash" => include_str!("../../client/assets/icons/trash.svg"),
        "search" => include_str!("../../client/assets/icons/search.svg"),
        "at-sign" => include_str!("../../client/assets/icons/at-sign.svg"),
        "message" => include_str!("../../client/assets/icons/message.svg"),
        "pin" => include_str!("../../client/assets/icons/pin.svg"),
        "camera" => include_str!("../../client/assets/icons/camera.svg"),
        "image" => include_str!("../../client/assets/icons/image.svg"),
        "lock" => include_str!("../../client/assets/icons/lock.svg"),
        "screen" => include_str!("../../client/assets/icons/screen.svg"),
        "chevron-up" => include_str!("../../client/assets/icons/chevron-up.svg"),
        "chevron-down" => include_str!("../../client/assets/icons/chevron-down.svg"),
        "chevron-left" => include_str!("../../client/assets/icons/chevron-left.svg"),
        "copy" => include_str!("../../client/assets/icons/copy.svg"),
        "link" => include_str!("../../client/assets/icons/link.svg"),
        "tag" => include_str!("../../client/assets/icons/tag.svg"),
        "power" => include_str!("../../client/assets/icons/power.svg"),
        _ => "",
    }
}

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
