//! System tray: blurple dot icon (red badge when unread), left-click to
//! restore the window, menu with Open/Quit. The window's X button hides to
//! tray; Quit here is the real exit.

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

/// Our own event queue: dioxus registers its own muda menu handler, which
/// starves the crates' default receiver channels — so we claim the handler
/// slots ourselves and forward into this queue.
static QUEUE: OnceLock<Mutex<std_mpsc::Receiver<RawEvent>>> = OnceLock::new();

pub struct Tray {
    icon: TrayIcon,
    open_id: MenuId,
    quit_id: MenuId,
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

    // Claim the event-handler slots (replacing any default/dioxus routing)
    // and forward everything into our queue.
    let (tx, rx) = std_mpsc::channel::<RawEvent>();
    let _ = QUEUE.set(Mutex::new(rx));
    let menu_tx = Mutex::new(tx.clone());
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if let Ok(tx) = menu_tx.lock() {
            let _ = tx.send(RawEvent::Menu(event.id));
        }
    }));
    let tray_tx = Mutex::new(tx);
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
            if let Ok(tx) = tray_tx.lock() {
                let _ = tx.send(RawEvent::LeftClick);
            }
        }
    }));

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

/// Poll tray/menu events; returns actions to apply on the UI side.
pub enum TrayAction {
    Show,
    Quit,
}

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
