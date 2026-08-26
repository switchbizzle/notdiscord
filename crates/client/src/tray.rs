//! System tray: blurple dot icon (red badge when unread), left-click to
//! restore the window (dioxus built-in), menu with Open/Quit. The window's X
//! button hides to tray; Quit here is the real exit.
//!
//! Menu events arrive through dioxus's `use_tray_menu_event_handler` hook in
//! main.rs. They CANNOT be received via `MenuEvent::set_event_handler`:
//! dioxus-desktop claims that slot during launch, and muda/tray-icon handler
//! slots are write-once OnceCells — our later set would be silently ignored
//! (which is exactly why the old polling approach never got a single event).

use std::cell::RefCell;
use std::rc::Rc;

use tray_icon::menu::{Menu, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

pub struct Tray {
    icon: TrayIcon,
    pub open_id: MenuId,
    pub quit_id: MenuId,
}

pub type TrayHandle = Rc<RefCell<Option<Tray>>>;

/// Render a simple circular icon; `unread` adds a red badge.
fn make_icon(unread: bool) -> Icon {
    const S: i32 = 32;
    let mut data = vec![0u8; (S * S * 4) as usize];
    let center = (S - 1) as f32 / 2.0;
    let radius = S as f32 / 2.0 - 1.5;

    let mut put = |x: i32, y: i32, rgba: [u8; 4]| {
        if (0..S).contains(&x) && (0..S).contains(&y) {
            let i = ((y * S + x) * 4) as usize;
            data[i..i + 4].copy_from_slice(&rgba);
        }
    };

    for y in 0..S {
        for x in 0..S {
            let d = ((x as f32 - center).powi(2) + (y as f32 - center).powi(2)).sqrt();
            if d <= radius {
                // Blurple disc with a soft edge.
                let alpha = ((radius - d + 1.0).clamp(0.0, 1.0) * 255.0) as u8;
                put(x, y, [88, 101, 242, alpha]);
            }
        }
    }

    if unread {
        let (bx, by, br) = (23.0f32, 23.0f32, 8.0f32);
        for y in 0..S {
            for x in 0..S {
                let d = ((x as f32 - bx).powi(2) + (y as f32 - by).powi(2)).sqrt();
                if d <= br {
                    let alpha = ((br - d + 1.0).clamp(0.0, 1.0) * 255.0) as u8;
                    put(x, y, [242, 63, 67, alpha.max(200)]);
                }
            }
        }
    }

    Icon::from_rgba(data, S as u32, S as u32).expect("valid icon rgba")
}

pub fn create() -> Option<Tray> {
    let menu = Menu::new();
    let open_item = MenuItem::new("Open NotDiscord", true, None);
    let quit_item = MenuItem::new("Quit", true, None);
    menu.append_items(&[&open_item, &quit_item]).ok()?;

    let icon = TrayIconBuilder::new()
        .with_tooltip("NotDiscord")
        .with_icon(make_icon(false))
        .with_menu(Box::new(menu))
        .build()
        .ok()?;

    Some(Tray { icon, open_id: open_item.id().clone(), quit_id: quit_item.id().clone() })
}

impl Tray {
    pub fn set_unread(&self, unread: bool) {
        let _ = self.icon.set_icon(Some(make_icon(unread)));
    }
}
