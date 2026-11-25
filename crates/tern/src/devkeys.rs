//! Debug builds, and release builds made with the `dev-keys` feature (never shipped):
//! `TERN_DEV_KEYS="cmd-k s t b"` replays keystrokes into the window after start-up, so a scripted
//! visual check can drive the app without taking keyboard focus from whatever window the person at
//! the machine is using. Besides keys: `sleep-500` waits half a second (for a prompt to arrive);
//! `type:text` types; `click:x,y`, `dblclick:x,y`, `rclick:x,y`, `scroll:x,y,dy` and
//! `drag:x0,y0,x1,y1` act at window points, and with modifiers named first they act with those
//! held (`cmd-click:x,y`, `shift-click:x,y`); `hold:alt-cmd` leaves modifiers held (for what
//! shows only while they are, like the ⌥⌘ pane numbers, so a screenshot can catch it) until
//! `release`. On macOS, `close-window` presses the native close button and
//! `agent-menu:Open_Tern` selects the named status menu item.

use std::time::Duration;

use gpui::{
    App, AppContext, Capslock, Keystroke, Modifiers, ModifiersChangedEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, Point, WindowHandle, point, px,
};

use crate::shell::Shell;

pub fn replay(window: WindowHandle<Shell>, cx: &mut App) {
    let Ok(script) = std::env::var("TERN_DEV_KEYS") else {
        return;
    };
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(Duration::from_millis(1500))
            .await;
        for source in script.split_whitespace() {
            #[cfg(target_os = "macos")]
            if source == "close-window" {
                if let Some(mtm) = objc2::MainThreadMarker::new() {
                    let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
                    if let Some(window) = app
                        .windows()
                        .iter()
                        .find(|w| w.isVisible() && w.canBecomeMainWindow())
                    {
                        window.performClose(None);
                    }
                }
                continue;
            }
            #[cfg(target_os = "macos")]
            if let Some(title) = source.strip_prefix("agent-menu:") {
                cx.update(|cx| {
                    if let Ok(shell) = window.read(cx)
                        && let Some(menu) = shell.agent_menu()
                    {
                        menu.replay(&title.replace('_', " "));
                    }
                });
                continue;
            }
            if let Some(ms) = source.strip_prefix("sleep-").and_then(|n| n.parse().ok()) {
                cx.background_executor()
                    .timer(Duration::from_millis(ms))
                    .await;
                continue;
            }
            // `type:abc-1` types each character as itself; `-` cannot be a parsed key name.
            if let Some(text) = source.strip_prefix("type:") {
                for ch in text.chars() {
                    let key = ch.to_string();
                    let keystroke = Keystroke {
                        modifiers: Modifiers::default(),
                        key: key.clone(),
                        key_char: Some(key),
                    };
                    let _ = cx.update_window(window.into(), |_, window, cx| {
                        window.dispatch_keystroke(keystroke, cx)
                    });
                    cx.background_executor()
                        .timer(Duration::from_millis(40))
                        .await;
                }
                continue;
            }
            if let Some(modifiers) = held(source) {
                let event = changed(modifiers);
                let _ = cx.update_window(window.into(), |_, window, cx| {
                    window.dispatch_event(event, cx);
                });
                continue;
            }
            if let Some(events) = mouse(source) {
                for event in events {
                    let _ = cx.update_window(window.into(), |_, window, cx| {
                        window.dispatch_event(event, cx);
                    });
                    cx.background_executor()
                        .timer(Duration::from_millis(30))
                        .await;
                }
                continue;
            }
            let Ok(keystroke) = Keystroke::parse(source) else {
                tracing::warn!(key = source, "dev_key_unparsed");
                continue;
            };
            let handled = cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_keystroke(keystroke, cx)
            });
            tracing::debug!(key = source, ?handled, "dev_key");
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
        }
    })
    .detach();
}

fn at(x: f32, y: f32) -> Point<gpui::Pixels> {
    point(px(x), px(y))
}

/// A pointer at a window point with some modifiers held: the events one mouse token is made of.
#[derive(Clone, Copy)]
struct Pointer {
    at: Point<gpui::Pixels>,
    held: Modifiers,
}

impl Pointer {
    fn down(self, clicks: usize, button: MouseButton) -> PlatformInput {
        PlatformInput::MouseDown(MouseDownEvent {
            button,
            position: self.at,
            modifiers: self.held,
            click_count: clicks,
            first_mouse: false,
        })
    }

    fn up(self, clicks: usize, button: MouseButton) -> PlatformInput {
        PlatformInput::MouseUp(MouseUpEvent {
            button,
            position: self.at,
            modifiers: self.held,
            click_count: clicks,
        })
    }

    fn moved(self, pressed: bool) -> PlatformInput {
        PlatformInput::MouseMove(MouseMoveEvent {
            position: self.at,
            pressed_button: pressed.then_some(MouseButton::Left),
            modifiers: self.held,
        })
    }
}

fn changed(modifiers: Modifiers) -> PlatformInput {
    PlatformInput::ModifiersChanged(ModifiersChangedEvent {
        modifiers,
        capslock: Capslock::default(),
    })
}

/// The window events for a mouse token: `click:`, `dblclick:`, `rclick:`, `scroll:` or `drag:`,
/// alone or after the modifiers it is made with (`cmd-click:`, `shift-cmd-click:`).
fn mouse(token: &str) -> Option<Vec<PlatformInput>> {
    let (kind, args) = token.split_once(':')?;
    let (held, kind) = match kind.rsplit_once('-') {
        Some((names, kind)) => (modifiers(names)?, kind),
        None => (Modifiers::default(), kind),
    };
    let n: Vec<f32> = args.split(',').filter_map(|v| v.parse().ok()).collect();
    let p = |x: f32, y: f32| Pointer { at: at(x, y), held };
    let left = MouseButton::Left;
    let events = match (kind, n.as_slice()) {
        ("rclick", [x, y]) => {
            let p = p(*x, *y);
            vec![
                p.moved(false),
                p.down(1, MouseButton::Right),
                p.up(1, MouseButton::Right),
            ]
        }
        ("click", [x, y]) => {
            let p = p(*x, *y);
            vec![p.moved(false), p.down(1, left), p.up(1, left)]
        }
        ("dblclick", [x, y]) => {
            let p = p(*x, *y);
            vec![
                p.moved(false),
                p.down(1, left),
                p.up(1, left),
                p.down(2, left),
                p.up(2, left),
            ]
        }
        ("scroll", [x, y, dy]) => vec![
            p(*x, *y).moved(false),
            PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                position: at(*x, *y),
                delta: gpui::ScrollDelta::Pixels(point(px(0.), px(*dy))),
                modifiers: held,
                touch_phase: gpui::TouchPhase::Moved,
            }),
        ],
        ("drag", [x0, y0, x1, y1]) => {
            let mut events = vec![p(*x0, *y0).moved(false), p(*x0, *y0).down(1, left)];
            for step in 1..=8 {
                let f = step as f32 / 8.0;
                events.push(p(x0 + (x1 - x0) * f, y0 + (y1 - y0) * f).moved(true));
            }
            events.push(p(*x1, *y1).up(1, left));
            events
        }
        _ => return None,
    };
    if held == Modifiers::default() {
        return Some(events);
    }
    // The window is told the modifiers went down and came up, as it is for a real click: what
    // reads them from the window, not from the click, sees them too.
    let mut around = vec![changed(held)];
    around.extend(events);
    around.push(changed(Modifiers::default()));
    Some(around)
}

/// The modifiers a `hold:` token leaves held (`hold:alt-cmd`), or none for `release`; `None` for
/// any other token, or a `hold:` naming a key that is not a modifier.
fn held(token: &str) -> Option<Modifiers> {
    if token == "release" {
        return Some(Modifiers::default());
    }
    modifiers(token.strip_prefix("hold:")?)
}

/// The modifiers named in `alt-cmd`; `None` when a name is not a modifier.
fn modifiers(names: &str) -> Option<Modifiers> {
    let mut m = Modifiers::default();
    for name in names.split('-') {
        match name {
            "alt" | "option" => m.alt = true,
            "cmd" | "command" => m.platform = true,
            "ctrl" | "control" => m.control = true,
            "shift" => m.shift = true,
            "fn" => m.function = true,
            _ => return None,
        }
    }
    Some(m)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]
    use super::*;

    #[test]
    fn hold_names_the_modifiers_left_held_and_release_lets_go() {
        let m = held("hold:alt-cmd").unwrap_or_default();
        assert!(m.alt && m.platform && !m.control && !m.shift && !m.function);
        let all = held("hold:option-command-control-shift-fn").unwrap_or_default();
        assert!(all.alt && all.platform && all.control && all.shift && all.function);
        assert_eq!(held("release"), Some(Modifiers::default()));
        assert_eq!(held("hold:alt-k"), None, "a key is not a modifier");
        assert_eq!(held("cmd-k"), None, "a keystroke is not a hold");
        assert_eq!(held("hold:"), None);
    }

    fn held_at_each(token: &str) -> Vec<Modifiers> {
        mouse(token)
            .unwrap_or_default()
            .iter()
            .map(|e| match e {
                PlatformInput::ModifiersChanged(e) => e.modifiers,
                PlatformInput::MouseMove(e) => e.modifiers,
                PlatformInput::MouseDown(e) => e.modifiers,
                PlatformInput::MouseUp(e) => e.modifiers,
                _ => panic!("not an event a click is made of"),
            })
            .collect()
    }

    #[test]
    fn a_click_named_after_modifiers_is_made_with_them_held_and_lets_go_after() {
        let none = Modifiers::default();
        let cmd = Modifiers {
            platform: true,
            ..none
        };
        assert_eq!(held_at_each("click:10,20"), vec![none, none, none]);
        assert_eq!(
            held_at_each("cmd-click:10,20"),
            vec![cmd, cmd, cmd, cmd, none],
            "told of the modifier, moved, pressed, let go, told it is up"
        );
        let both = Modifiers { shift: true, ..cmd };
        assert_eq!(held_at_each("shift-cmd-click:10,20")[1], both);
        assert!(mouse("k-click:10,20").is_none(), "a key is not a modifier");
        assert!(
            mouse("cmd-click:10").is_none(),
            "a click needs both of x and y"
        );
    }
}
