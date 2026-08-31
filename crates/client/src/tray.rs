//! System tray: blurple dot icon (red badge when unread), left-click to
//! restore the window, menu with Open/Quit. The window's X button hides to
//! tray; Quit here is the real exit.
//!
//! Menu delivery is the fiddly part. muda's and tray-icon's handler slots are
//! write-once OnceCells, and dioxus-desktop claims both while launching —
//! before any component mounts. So `claim_event_handlers()` must run from
//! `main()` BEFORE the launch, which makes our handler the one muda calls and
//! turns dioxus's later registration into the no-op instead. Events land in
//! our own queue, which the UI drains.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc as std_mpsc;
use std::sync::{Mutex, OnceLock};

use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

enum RawEvent {
    Menu(MenuId),
    LeftClick,
}

static QUEUE: OnceLock<Mutex<std_mpsc::Receiver<RawEvent>>> = OnceLock::new();

/// Claim the process-wide muda/tray-icon handler slots. MUST be called from
/// `main()` before dioxus launches, or dioxus wins the OnceCell race and our
/// menu clicks vanish into its no-op handler.
pub fn claim_event_handlers() {
    let (tx, rx) = std_mpsc::channel::<RawEvent>();
    if QUEUE.set(Mutex::new(rx)).is_err() {
        return;
    }
    let menu_tx = Mutex::new(tx.clone());
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        crate::api::debug_log(&format!("tray menu event: {:?}", event.id));
        if let Ok(tx) = menu_tx.lock() {
            let _ = tx.send(RawEvent::Menu(event.id));
        }
    }));
    let tray_tx = Mutex::new(tx);
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            if let Ok(tx) = tray_tx.lock() {
                let _ = tx.send(RawEvent::LeftClick);
            }
        }
    }));
}

pub struct Tray {
    icon: TrayIcon,
    pub open_id: MenuId,
    pub quit_id: MenuId,
}

pub type TrayHandle = Rc<RefCell<Option<Tray>>>;

/// Render a simple circular icon; `unread` adds a red badge.
/// The tray icon: the app's own logo, with a red dot when something is
/// waiting. Drawn from the same PNG as the window and exe icons, so the three
/// places Windows shows this app all show the same thing.
fn make_icon(unread: bool) -> Icon {
    const S: u32 = 32;
    let mut img = match crate::logo_rgba() {
        Some((rgba, w, h)) => image::RgbaImage::from_raw(w, h, rgba)
            .map(|img| image::imageops::resize(&img, S, S, image::imageops::FilterType::Lanczos3))
            .unwrap_or_else(|| image::RgbaImage::new(S, S)),
        None => image::RgbaImage::new(S, S),
    };

    if unread {
        // Top-right, where Jon asked for it and where every other app puts
        // it. Clear of the mark either way.
        let (bx, by, br) = (23.0f32, 9.0f32, 8.0f32);
        for y in 0..S {
            for x in 0..S {
                let d = ((x as f32 - bx).powi(2) + (y as f32 - by).powi(2)).sqrt();
                if d <= br {
                    let alpha = ((br - d + 1.0).clamp(0.0, 1.0) * 255.0) as u8;
                    img.put_pixel(x, y, image::Rgba([242, 63, 67, alpha.max(200)]));
                }
            }
        }
    }

    Icon::from_rgba(img.into_raw(), S, S).expect("valid icon rgba")
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

/// What the UI should do about tray activity.
pub enum TrayAction {
    Show,
    Quit,
}

/// Drain queued tray events. Called on a short timer by the UI.
pub fn poll_events(tray: &TrayHandle) -> Vec<TrayAction> {
    let mut actions = Vec::new();
    let borrow = tray.borrow();
    let Some(tray) = borrow.as_ref() else {
        return actions;
    };
    let Some(queue) = QUEUE.get() else {
        return actions;
    };
    let Ok(rx) = queue.lock() else {
        return actions;
    };
    while let Ok(event) = rx.try_recv() {
        match event {
            RawEvent::LeftClick => actions.push(TrayAction::Show),
            RawEvent::Menu(id) if id == tray.open_id => actions.push(TrayAction::Show),
            RawEvent::Menu(id) if id == tray.quit_id => actions.push(TrayAction::Quit),
            RawEvent::Menu(_) => {}
        }
    }
    actions
}
