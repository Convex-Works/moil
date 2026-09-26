//! The tray icon, drawn in code: a chip whose core shows what the machine
//! is doing (hollow when idle, filled while a job runs, two bars when
//! paused), with a badge when the owner is needed.
//!
//! The images are template images: pure black, with the shape carried by
//! alpha alone, so that macOS can tint them for light and dark menu bars
//! and for the highlighted state.

use crate::view::{Activity, IconState};

/// An RGBA image, row by row, 4 bytes per pixel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[cfg(test)]
impl Image {
    /// The alpha of the pixel at (`x`, `y`).
    pub fn alpha(&self, x: u32, y: u32) -> u8 {
        self.rgba[((y * self.width + x) * 4 + 3) as usize]
    }
}

/// The menu bar is 18 points tall, and 2× displays are the norm.
pub const TRAY_SIZE: u32 = 36;

/// Shapes are laid out on a 36 × 36 grid, scaled to the image size.
const GRID: f32 = 36.0;
/// Samples per pixel along each axis, for antialiasing.
const SAMPLES: u32 = 4;

/// Draws the icon for `state`, `size` pixels square.
pub fn draw(state: IconState, size: u32) -> Image {
    let scale = GRID / size as f32;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let mut covered = 0;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let px = (x as f32 + (sx as f32 + 0.5) / SAMPLES as f32) * scale;
                    let py = (y as f32 + (sy as f32 + 0.5) / SAMPLES as f32) * scale;
                    covered += u32::from(ink(state, px, py));
                }
            }
            let alpha = (covered * 255 / (SAMPLES * SAMPLES)) as u8;
            rgba.extend_from_slice(&[0, 0, 0, alpha]);
        }
    }
    Image {
        width: size,
        height: size,
        rgba,
    }
}

/// The badge's center and radius, and the clear gap around it.
const BADGE: (f32, f32) = (29.0, 7.0);
const BADGE_RADIUS: f32 = 5.5;
const BADGE_GAP: f32 = 2.5;

/// Whether the point (`x`, `y`) on the grid is inked.
fn ink(state: IconState, x: f32, y: f32) -> bool {
    let body = rounded_rect(x, y, (18.0, 18.0), (9.0, 9.0), 3.5).abs() <= 1.25;
    let pins = [14.5, 21.5].iter().any(|&at| {
        let across = |c: f32| (c - at).abs() <= 1.25;
        let outside = |c: f32| (3.5..=8.0).contains(&c) || (28.0..=32.5).contains(&c);
        (across(y) && outside(x)) || (across(x) && outside(y))
    });
    let core = match state.activity {
        Activity::Idle => false,
        Activity::Running => rounded_rect(x, y, (18.0, 18.0), (4.5, 4.5), 1.5) <= 0.0,
        Activity::Paused => [15.0, 21.0]
            .iter()
            .any(|&cx| rounded_rect(x, y, (cx, 18.0), (1.5, 4.5), 0.75) <= 0.0),
    };
    let chip = body || pins || core;
    if !state.attention {
        return chip;
    }
    let from_badge = (x - BADGE.0).hypot(y - BADGE.1);
    from_badge <= BADGE_RADIUS || (chip && from_badge > BADGE_RADIUS + BADGE_GAP)
}

/// The signed distance from (`x`, `y`) to a rectangle with rounded corners:
/// negative inside, positive outside.
fn rounded_rect(x: f32, y: f32, center: (f32, f32), half: (f32, f32), radius: f32) -> f32 {
    let qx = (x - center.0).abs() - half.0 + radius;
    let qy = (y - center.1).abs() - half.1 + radius;
    qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [IconState; 6] = [
        IconState {
            activity: Activity::Idle,
            attention: false,
        },
        IconState {
            activity: Activity::Running,
            attention: false,
        },
        IconState {
            activity: Activity::Paused,
            attention: false,
        },
        IconState {
            activity: Activity::Idle,
            attention: true,
        },
        IconState {
            activity: Activity::Running,
            attention: true,
        },
        IconState {
            activity: Activity::Paused,
            attention: true,
        },
    ];

    fn state(activity: Activity, attention: bool) -> IconState {
        IconState {
            activity,
            attention,
        }
    }

    #[test]
    fn icons_are_template_images() {
        for state in ALL {
            let image = draw(state, TRAY_SIZE);
            assert_eq!(image.rgba.len(), (TRAY_SIZE * TRAY_SIZE * 4) as usize);
            assert!(
                image.rgba.chunks(4).all(|px| px[..3] == [0, 0, 0]),
                "{state:?}: color would be lost when macOS tints the icon"
            );
            assert!(image.rgba.chunks(4).any(|px| px[3] == 255));
            assert!(image.rgba.chunks(4).any(|px| px[3] == 0));
        }
    }

    #[test]
    fn every_state_looks_different() {
        let images: Vec<_> = ALL.iter().map(|&s| draw(s, TRAY_SIZE)).collect();
        for (i, a) in images.iter().enumerate() {
            for b in &images[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn the_core_shows_the_activity() {
        let center = |activity| draw(state(activity, false), TRAY_SIZE).alpha(18, 18);
        assert_eq!(center(Activity::Idle), 0, "an idle chip is hollow");
        assert_eq!(center(Activity::Running), 255, "a busy one is filled");
        let paused = draw(state(Activity::Paused, false), TRAY_SIZE);
        assert_eq!(paused.alpha(18, 18), 0, "between the pause bars");
        assert_eq!(paused.alpha(15, 18), 255, "a pause bar");
        assert_eq!(paused.alpha(21, 18), 255, "the other bar");
    }

    #[test]
    fn the_badge_stands_apart_from_the_chip() {
        let calm = draw(state(Activity::Idle, false), TRAY_SIZE);
        let called = draw(state(Activity::Idle, true), TRAY_SIZE);
        let (bx, by) = (BADGE.0 as u32, BADGE.1 as u32);
        assert_eq!(calm.alpha(bx, by), 0);
        assert_eq!(called.alpha(bx, by), 255);
        // The chip's top edge is inked, but cleared around the badge.
        assert_eq!(calm.alpha(22, 8), 255);
        assert_eq!(called.alpha(22, 8), 0);
        // Away from the badge, the two are the same.
        for y in 16..TRAY_SIZE {
            for x in 0..TRAY_SIZE {
                assert_eq!(calm.alpha(x, y), called.alpha(x, y), "({x}, {y})");
            }
        }
    }

    #[test]
    fn icons_scale() {
        let small = draw(state(Activity::Running, true), 18);
        assert_eq!(
            (small.width, small.height, small.rgba.len()),
            (18, 18, 18 * 18 * 4)
        );
        assert_eq!(small.alpha(9, 9), 255);
    }
}
