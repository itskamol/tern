//! What the host system provides, in one place: its file manager's name, the bundled fonts, the
//! one tokio runtime for SSH, and the macOS display accessibility preferences.

use std::borrow::Cow;

use gpui::{App, Global};

/// What the "show this file" action is called: the file manager differs by system. The label
/// for `cx.reveal_path`'s button.
pub const REVEAL_LABEL: &str = if cfg!(target_os = "macos") {
    "Show in Finder"
} else if cfg!(target_os = "windows") {
    "Show in Explorer"
} else {
    "Show in Files"
};

// Bundled Geist faces (SIL OFL 1.1, `assets/fonts/licenses`), registered once at start-up.
const FACES: [&[u8]; 7] = [
    include_bytes!("../assets/fonts/Geist.ttf"),
    include_bytes!("../assets/fonts/Geist-Medium.ttf"),
    include_bytes!("../assets/fonts/Geist-SemiBold.ttf"),
    include_bytes!("../assets/fonts/GeistMono.ttf"),
    include_bytes!("../assets/fonts/GeistMono-Bold.ttf"),
    include_bytes!("../assets/fonts/GeistMono-Italic.ttf"),
    include_bytes!("../assets/fonts/GeistMono-BoldItalic.ttf"),
];

/// Falls back to system fonts if registration fails; the window still opens.
pub fn register_fonts(cx: &App) {
    let faces = FACES.iter().map(|f| Cow::Borrowed(*f)).collect();
    if let Err(e) = cx.text_system().add_fonts(faces) {
        tracing::warn!(error = %e, "fonts_register_failed");
    }
}

/// The one tokio runtime all SSH sessions share, on its own thread, so network work never
/// runs on the UI thread and there is no thread per connection.
pub struct SshRuntime(tokio::runtime::Runtime);

impl Global for SshRuntime {}

impl SshRuntime {
    pub fn install(cx: &mut App) -> std::io::Result<()> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("tern-ssh")
            .enable_all()
            .build()?;
        cx.set_global(Self(rt));
        Ok(())
    }

    pub fn handle(cx: &App) -> tokio::runtime::Handle {
        cx.global::<Self>().0.handle().clone()
    }
}

// Whether to hold animations still: tern's own "Reduce motion" setting, or macOS's
// (System Settings → Accessibility → Display → Reduce motion), as zeron reads it in
// crates/ui/src/motion.rs. objc2-app-kit exposes both calls as safe functions, so the
// workspace's `unsafe_code = "forbid"` holds.

/// macOS's Reduce motion preference.
#[cfg(target_os = "macos")]
pub fn system_reduce_motion() -> bool {
    objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}

#[cfg(not(target_os = "macos"))]
pub fn system_reduce_motion() -> bool {
    false
}

/// macOS's Increase contrast preference (Accessibility → Display).
#[cfg(target_os = "macos")]
pub fn system_increase_contrast() -> bool {
    objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldIncreaseContrast()
}

#[cfg(not(target_os = "macos"))]
pub fn system_increase_contrast() -> bool {
    false
}

/// macOS's Reduce transparency preference (Accessibility → Display).
#[cfg(target_os = "macos")]
pub fn system_reduce_transparency() -> bool {
    objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceTransparency()
}

#[cfg(not(target_os = "macos"))]
pub fn system_reduce_transparency() -> bool {
    false
}

/// Applies the effective value: either switch on holds motion still.
pub fn apply_reduce_motion(setting: bool, cx: &mut App) {
    let reduce = setting || system_reduce_motion();
    if cx.reduce_motion() != reduce {
        cx.set_reduce_motion(reduce);
    }
}

#[cfg(all(test, target_os = "macos"))]
#[allow(clippy::disallowed_methods, clippy::expect_used)]
mod tests {
    /// The safe binding reads the same preference `defaults` shows (absent means off).
    #[test]
    fn system_value_matches_the_preference_store() {
        let out = std::process::Command::new("defaults")
            .args(["read", "com.apple.universalaccess", "reduceMotion"])
            .output()
            .expect("defaults");
        let stored = String::from_utf8_lossy(&out.stdout).trim() == "1";
        assert_eq!(super::system_reduce_motion(), stored);
    }

    /// The display preferences are read the same way, and a missing key means off.
    #[test]
    fn display_preferences_match_the_preference_store() {
        for (key, read) in [
            (
                "increaseContrast",
                super::system_increase_contrast as fn() -> bool,
            ),
            ("reduceTransparency", super::system_reduce_transparency),
        ] {
            let out = std::process::Command::new("defaults")
                .args(["read", "com.apple.universalaccess", key])
                .output()
                .expect("defaults");
            assert_eq!(
                read(),
                String::from_utf8_lossy(&out.stdout).trim() == "1",
                "{key}"
            );
        }
    }
}
