//! How opaque the panels over a full-window wallpaper must be. The picture shows at full
//! strength in the gaps and through each panel at `1 - alpha`; instead of dimming it, the alpha
//! is the smallest one at which every text colour keeps its contrast over the picture's
//! brightest and darkest regions, so a dark picture on a dark theme barely needs a panel and a
//! white one needs nearly opaque ones.

use crate::wallpaper_fx::contrast_ratio;

/// The least and most opaque a panel gets: below the floor text starts to swim on any picture,
/// above the cap the picture is hardly there.
pub const MIN_ALPHA: f32 = 0.55;
pub const MAX_ALPHA: f32 = 0.92;
/// Colours sampled from the picture, darkest to brightest.
const SAMPLES: usize = 21;
const STEP: f32 = 0.01;

/// A panel surface and the text colours drawn on it, each with the contrast it must keep.
pub struct Panel<'a> {
    pub surface: [u8; 3],
    pub texts: &'a [([u8; 3], f32)],
}

fn pack(c: [u8; 3]) -> u32 {
    u32::from(c[0]) << 16 | u32::from(c[1]) << 8 | u32::from(c[2])
}

/// [`SAMPLES`] colours from darkest to brightest, taken at even steps between the 2nd and 98th
/// percentile of the picture's brightness, so a speck of white does not decide the panels.
pub fn backdrop(rgba: &[u8]) -> Vec<[u8; 3]> {
    let mut pixels: Vec<[u8; 3]> = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[3] >= 128)
        .map(|p| [p[0], p[1], p[2]])
        .collect();
    // Contrast against black rises with luminance, which is all the ordering needs.
    pixels.sort_by(|a, b| contrast_ratio(pack(*a), 0).total_cmp(&contrast_ratio(pack(*b), 0)));
    if pixels.is_empty() {
        return Vec::new();
    }
    (0..SAMPLES)
        .map(|i| {
            let p = 0.02 + 0.96 * i as f32 / (SAMPLES - 1) as f32;
            pixels[((pixels.len() - 1) as f32 * p).round() as usize]
        })
        .collect()
}

/// `surface` laid over `under` at `alpha`, in gamma-encoded sRGB as the screen composites it.
fn over(surface: [u8; 3], under: [u8; 3], alpha: f32) -> [u8; 3] {
    let ch = |i: usize| {
        (f32::from(surface[i]) * alpha + f32::from(under[i]) * (1.0 - alpha)).round() as u8
    };
    [ch(0), ch(1), ch(2)]
}

/// Whether every text keeps its contrast on its panel over every sampled picture colour.
fn holds(backdrop: &[[u8; 3]], panels: &[Panel], alpha: f32) -> bool {
    panels.iter().all(|panel| {
        backdrop.iter().all(|under| {
            let seen = pack(over(panel.surface, *under, alpha));
            panel
                .texts
                .iter()
                .all(|(text, minimum)| contrast_ratio(pack(*text), seen) >= *minimum)
        })
    })
}

/// A panel alpha remembered with the key it was worked out for, so a frame that asks again with
/// the same inputs does not repeat the search (the whole window asks several times a frame).
#[derive(Debug)]
pub struct Memo<K> {
    slot: std::cell::RefCell<Option<(K, f32)>>,
}

impl<K> Default for Memo<K> {
    fn default() -> Self {
        Self {
            slot: std::cell::RefCell::new(None),
        }
    }
}

impl<K> Memo<K> {
    /// The remembered alpha, if the key it was worked out for satisfies `is`.
    pub fn recall(&self, is: impl FnOnce(&K) -> bool) -> Option<f32> {
        let slot = self.slot.borrow();
        let (key, alpha) = slot.as_ref()?;
        is(key).then_some(*alpha)
    }

    /// Replaces what is remembered.
    pub fn remember(&self, key: K, alpha: f32) {
        *self.slot.borrow_mut() = Some((key, alpha));
    }
}

/// The smallest alpha in [[`MIN_ALPHA`], [`MAX_ALPHA`]] at which every text holds its contrast
/// on every panel over the (frosted) picture; the cap when none does (or the picture has no pixels).
pub fn panel_alpha(backdrop: &[[u8; 3]], panels: &[Panel]) -> f32 {
    if backdrop.is_empty() {
        return MAX_ALPHA;
    }
    let steps = ((MAX_ALPHA - MIN_ALPHA) / STEP).round() as u32;
    (0..=steps)
        .map(|i| MIN_ALPHA + STEP * i as f32)
        .find(|a| holds(backdrop, panels, *a))
        .unwrap_or(MAX_ALPHA)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(rgb: [u8; 3]) -> Vec<[u8; 3]> {
        vec![rgb; SAMPLES]
    }

    /// zeron dark: surface and the text, muted and faint colours drawn on it.
    const DARK: Panel = Panel {
        surface: [0x0d, 0x0d, 0x0d],
        texts: &[
            ([0xe8, 0xe8, 0xea], 4.5),
            ([0xa9, 0xa9, 0xae], 4.5),
            ([0x85, 0x85, 0x8a], 3.0),
        ],
    };
    const DARK_TEXT_AND_MUTED: Panel = Panel {
        surface: [0x0d, 0x0d, 0x0d],
        texts: &[([0xe8, 0xe8, 0xea], 4.5), ([0xa9, 0xa9, 0xae], 4.5)],
    };
    const FAINT_3: Panel = Panel {
        surface: [0x0d, 0x0d, 0x0d],
        texts: &[([0x85, 0x85, 0x8a], 3.0)],
    };
    const FAINT_45: Panel = Panel {
        surface: [0x0d, 0x0d, 0x0d],
        texts: &[([0x85, 0x85, 0x8a], 4.5)],
    };
    const LIGHT: Panel = Panel {
        surface: [0xf3, 0xf3, 0xf5],
        texts: &[([0x30, 0x30, 0x35], 4.5), ([0x62, 0x62, 0x6a], 4.5)],
    };

    #[test]
    fn a_white_picture_needs_the_most_opaque_panels() {
        let white = panel_alpha(&solid([255; 3]), &[DARK]);
        assert!(white >= 0.8, "{white}");
        assert!(white > panel_alpha(&solid([200; 3]), &[DARK]));
        // Held to 4.5 the faint text could not be reached at all: the cap.
        let strict = Panel {
            surface: DARK.surface,
            texts: &[([0x85, 0x85, 0x8a], 4.5)],
        };
        assert_eq!(panel_alpha(&solid([255; 3]), &[strict]), MAX_ALPHA);
    }

    #[test]
    fn a_black_picture_on_a_dark_theme_needs_only_the_floor() {
        assert_eq!(panel_alpha(&solid([0; 3]), &[DARK]), MIN_ALPHA);
    }

    #[test]
    fn light_grey_lands_between_and_is_the_smallest_alpha_that_holds() {
        let grey = solid([200; 3]);
        let panels = [DARK_TEXT_AND_MUTED];
        let a = panel_alpha(&grey, &panels);
        assert!(a > MIN_ALPHA + 0.1 && a < MAX_ALPHA, "{a}");
        assert!(holds(&grey, &panels, a));
        assert!(!holds(&grey, &panels, a - STEP), "{a} is not minimal");
    }

    #[test]
    fn faint_text_is_held_to_three_to_one_not_four_and_a_half() {
        let grey = solid([150; 3]);
        let (relaxed, strict) = (FAINT_3, FAINT_45);
        let (a3, a45) = (
            panel_alpha(&grey, &[relaxed]),
            panel_alpha(&grey, &[strict]),
        );
        assert!(a3 < a45, "{a3} {a45}");
        assert!(holds(&grey, &[FAINT_3], a3) && !holds(&grey, &[FAINT_45], a3));
    }

    #[test]
    fn yellow_needs_dark_panels_more_than_light_ones() {
        let yellow = solid([255, 220, 20]);
        let dark = panel_alpha(&yellow, &[DARK]);
        let light = panel_alpha(&yellow, &[LIGHT]);
        assert!(dark >= 0.75, "{dark}");
        assert_eq!(light, MIN_ALPHA);
        assert!(dark > light);
    }

    #[test]
    fn the_brightest_and_the_darkest_region_both_count() {
        // Half black, half white: the white half decides, as for an all-white picture.
        let mut split = solid([0; 3]);
        for c in split.iter_mut().skip(SAMPLES / 2) {
            *c = [255; 3];
        }
        let all_white = panel_alpha(&solid([255; 3]), &[DARK_TEXT_AND_MUTED]);
        assert_eq!(panel_alpha(&split, &[DARK_TEXT_AND_MUTED]), all_white);
        // And on a light theme the dark half is the threat.
        let light_text = panel_alpha(&solid([0; 3]), &[LIGHT]);
        assert_eq!(panel_alpha(&split, &[LIGHT]), light_text.max(MIN_ALPHA));
    }

    #[test]
    fn every_panel_must_hold_not_only_the_first() {
        // Black picture: the dark panel needs only the floor, the light one more.
        let black = solid([0; 3]);
        let panels = [DARK_TEXT_AND_MUTED, LIGHT];
        let both = panel_alpha(&black, &panels);
        assert_eq!(panel_alpha(&black, &[DARK_TEXT_AND_MUTED]), MIN_ALPHA);
        assert!(both > MIN_ALPHA, "{both}");
        assert!(holds(&black, &panels, both));
    }

    #[test]
    fn no_pixels_means_the_cap() {
        assert_eq!(panel_alpha(&[], &[DARK]), MAX_ALPHA);
    }

    #[test]
    fn backdrop_runs_dark_to_bright_and_ignores_a_speck() {
        let mut px = Vec::new();
        for i in 0..100u32 {
            let v = (i * 2) as u8;
            px.extend([v, v, v, 255]);
        }
        px.extend([255, 255, 255, 255]); // a lone bright pixel past the 98th percentile
        px.extend([9, 9, 9, 0]); // transparent: not a pixel
        let b = backdrop(&px);
        assert_eq!(b.len(), SAMPLES);
        assert!(b.windows(2).all(|w| w[0][0] <= w[1][0]));
        assert!(b[0][0] < 10 && b[SAMPLES - 1][0] < 255);
        assert!(backdrop(&[]).is_empty());
    }

    #[test]
    fn a_memo_answers_only_for_the_key_it_was_given() {
        let memo: Memo<(u8, Option<String>)> = Memo::default();
        assert_eq!(memo.recall(|_| true), None);
        memo.remember((1, Some("Nord".into())), 0.7);
        assert_eq!(
            memo.recall(|k| k.0 == 1 && k.1.as_deref() == Some("Nord")),
            Some(0.7)
        );
        assert_eq!(memo.recall(|k| k.0 == 2), None);
        assert_eq!(memo.recall(|k| k.1.is_none()), None);
        memo.remember((2, None), 0.9);
        assert_eq!(memo.recall(|k| k.0 == 1), None);
        assert_eq!(memo.recall(|k| k.0 == 2 && k.1.is_none()), Some(0.9));
    }

    /// What one `window_fill` costs when it is not remembered: run with
    /// `cargo +1.99.1 test -p tern --release -- --ignored --nocapture panel_alpha_cost`.
    #[test]
    #[ignore = "benchmark: run with --release --ignored --nocapture"]
    #[allow(clippy::print_stderr)]
    fn panel_alpha_cost() {
        let ramp = |lo: u8, hi: u8| -> Vec<[u8; 3]> {
            (0..SAMPLES)
                .map(|i| {
                    let v = lo + ((hi - lo) as usize * i / (SAMPLES - 1)) as u8;
                    [v, v, v]
                })
                .collect()
        };
        const TERMINAL: Panel = Panel {
            surface: [0x16, 0x18, 0x1a],
            texts: &[
                ([0xe8, 0xe8, 0xea], 4.5),
                ([0xa9, 0xa9, 0xae], 4.5),
                ([0x85, 0x85, 0x8a], 3.0),
                ([0xd0, 0xd0, 0xd0], 4.5),
            ],
        };
        for (name, backdrop) in [
            ("dark picture (floor holds at once)", ramp(5, 60)),
            ("mid picture", ramp(40, 200)),
            ("bright picture (no alpha holds: all steps)", ramp(180, 255)),
        ] {
            let runs = 2000;
            let started = std::time::Instant::now();
            let mut sum = 0.0;
            for _ in 0..runs {
                sum += panel_alpha(&backdrop, &[DARK, TERMINAL]);
            }
            let each = started.elapsed() / runs;
            eprintln!(
                "panel_alpha {name}: {each:?} per call (alpha {:.2})",
                sum / runs as f32
            );
        }
    }
}
