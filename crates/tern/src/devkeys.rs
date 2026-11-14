//! Debug builds, and release builds made with the `dev-keys` feature (never shipped):
//! `TERN_DEV_KEYS="cmd-k s t b"` replays keystrokes into the window after start-up, so a scripted
//! visual check can drive the app without taking keyboard focus from whatever window the person at
//! the machine is using. Besides keys: `sleep-500` waits half a second (for a prompt to arrive);
//! `type:text` types; `click:x,y`, `dblclick:x,y`, `rclick:x,y`, `scroll:x,y,dy` and
//! `drag:x0,y0,x1,y1` act at window points; `hold:alt-cmd` leaves modifiers held (for what shows
//! only while they are, like the ⌥⌘ pane numbers, so a screenshot can catch it) until `release`.

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
                let event = PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                    modifiers,
                    capslock: Capslock::default(),
                });
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

fn down(p: Point<gpui::Pixels>, clicks: usize) -> PlatformInput {
    down_with(p, clicks, MouseButton::Left)
}

fn down_with(p: Point<gpui::Pixels>, clicks: usize, button: MouseButton) -> PlatformInput {
    PlatformInput::MouseDown(MouseDownEvent {
        button,
        position: p,
        modifiers: Modifiers::default(),
        click_count: clicks,
        first_mouse: false,
    })
}

fn up(p: Point<gpui::Pixels>, clicks: usize) -> PlatformInput {
    up_with(p, clicks, MouseButton::Left)
}

fn up_with(p: Point<gpui::Pixels>, clicks: usize, button: MouseButton) -> PlatformInput {
    PlatformInput::MouseUp(MouseUpEvent {
        button,
        position: p,
        modifiers: Modifiers::default(),
        click_count: clicks,
    })
}

fn moved(p: Point<gpui::Pixels>, pressed: bool) -> PlatformInput {
    PlatformInput::MouseMove(MouseMoveEvent {
        position: p,
        pressed_button: pressed.then_some(MouseButton::Left),
        modifiers: Modifiers::default(),
    })
}

/// The window events for a `click:`, `dblclick:` or `drag:` token.
fn mouse(token: &str) -> Option<Vec<PlatformInput>> {
    let (kind, args) = token.split_once(':')?;
    let n: Vec<f32> = args.split(',').filter_map(|v| v.parse().ok()).collect();
    match (kind, n.as_slice()) {
        ("rclick", [x, y]) => Some(vec![
            moved(at(*x, *y), false),
            down_with(at(*x, *y), 1, MouseButton::Right),
            up_with(at(*x, *y), 1, MouseButton::Right),
        ]),
        ("click", [x, y]) => Some(vec![
            moved(at(*x, *y), false),
            down(at(*x, *y), 1),
            up(at(*x, *y), 1),
        ]),
        ("dblclick", [x, y]) => Some(vec![
            moved(at(*x, *y), false),
            down(at(*x, *y), 1),
            up(at(*x, *y), 1),
            down(at(*x, *y), 2),
            up(at(*x, *y), 2),
        ]),
        ("scroll", [x, y, dy]) => Some(vec![
            moved(at(*x, *y), false),
            PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                position: at(*x, *y),
                delta: gpui::ScrollDelta::Pixels(point(px(0.), px(*dy))),
                modifiers: Modifiers::default(),
                touch_phase: gpui::TouchPhase::Moved,
            }),
        ]),
        ("drag", [x0, y0, x1, y1]) => {
            let mut events = vec![moved(at(*x0, *y0), false), down(at(*x0, *y0), 1)];
            for step in 1..=8 {
                let f = step as f32 / 8.0;
                events.push(moved(at(x0 + (x1 - x0) * f, y0 + (y1 - y0) * f), true));
            }
            events.push(up(at(*x1, *y1), 1));
            Some(events)
        }
        _ => None,
    }
}

/// The modifiers a `hold:` token leaves held (`hold:alt-cmd`), or none for `release`; `None` for
/// any other token, or a `hold:` naming a key that is not a modifier.
fn held(token: &str) -> Option<Modifiers> {
    if token == "release" {
        return Some(Modifiers::default());
    }
    let mut m = Modifiers::default();
    for name in token.strip_prefix("hold:")?.split('-') {
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
}
