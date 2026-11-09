//! `ROSEMARY_OVERLAY_IMAGE`: a small picture a launcher asks to see over the
//! window, copied onto every frame after the guest's own.
//!
//! The picture is a PAM (`P7`, `TUPLTYPE RGB_ALPHA`, maxval 255), so the
//! launcher draws it and this only copies pixels: those with alpha are copied,
//! row run by row run, and the rest of the frame is left as the guest drew it.
//! The file is read again when it changes; an empty or missing file hides the
//! picture. It slides in from its edge of the window and back out again.
//! `ROSEMARY_OVERLAY_POSITION` is `top` (the default), `bottom`, `top-left`,
//! `top-right`, `bottom-left` or `bottom-right`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use ash::vk;

pub(super) const IMAGE_ENV: &str = "ROSEMARY_OVERLAY_IMAGE";
pub(super) const POSITION_ENV: &str = "ROSEMARY_OVERLAY_POSITION";

/// How often the file is looked at for a change.
const RECHECK: Duration = Duration::from_millis(500);

/// How long the picture takes to slide in or out.
const SLIDE: Duration = Duration::from_millis(250);

/// Space between the picture and the window's edges, in pixels.
pub(super) const MARGIN: u32 = 12;

/// Bytes a texel of the swapchain formats this copies into takes.
pub(super) const TEXEL_BYTES: usize = 4;

/// Alpha from which a pixel counts as part of the picture.
const OPAQUE_ENOUGH: u8 = 128;

/// The picture as read from the file. `generation` changes whenever the file
/// does, so a swapchain knows to upload it again.
pub(super) struct Badge {
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    rgba: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Position {
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Position {
    fn enters_from_top(self) -> bool {
        matches!(self, Position::Top | Position::TopLeft | Position::TopRight)
    }
}

/// The picture and how far it has slid into view, from 0 (out of the window)
/// to 1 (in place).
pub(super) struct Shown {
    pub badge: Arc<Badge>,
    pub shown: f32,
}

struct State {
    checked: Option<Instant>,
    stamp: Option<(SystemTime, u64)>,
    generation: u64,
    badge: Option<Arc<Badge>>,
    since: Instant,
    /// The picture that was replaced by nothing, sliding out.
    leaving: Option<(Arc<Badge>, Instant)>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn path() -> Option<&'static std::path::Path> {
    static PATH: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        std::env::var_os(IMAGE_ENV)
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from)
    })
    .as_deref()
}

pub(super) fn position() -> Position {
    static POSITION: std::sync::OnceLock<Position> = std::sync::OnceLock::new();
    *POSITION.get_or_init(
        || match std::env::var(POSITION_ENV).as_deref().map(str::trim) {
            Ok("bottom") => Position::Bottom,
            Ok("top-left") => Position::TopLeft,
            Ok("top-right") => Position::TopRight,
            Ok("bottom-left") => Position::BottomLeft,
            Ok("bottom-right") => Position::BottomRight,
            Ok("top") | Err(_) => Position::Top,
            Ok(other) => {
                crate::gpu::trace_once(format!(
                    "{POSITION_ENV}={other} is not a position; using top"
                ));
                Position::Top
            }
        },
    )
}

/// Eased progress of a slide that began `since`, from 0 to 1.
fn eased(since: Instant) -> f32 {
    let t = (since.elapsed().as_secs_f32() / SLIDE.as_secs_f32()).min(1.0);
    1.0 - (1.0 - t).powi(3)
}

/// The picture to show now, if any.
pub(super) fn current() -> Option<Shown> {
    let path = path()?;
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let state = guard.get_or_insert_with(|| State {
        checked: None,
        stamp: None,
        generation: 0,
        badge: None,
        since: Instant::now(),
        leaving: None,
    });
    if state.checked.is_none_or(|at| at.elapsed() >= RECHECK) {
        state.checked = Some(Instant::now());
        let stamp = std::fs::metadata(path)
            .ok()
            .and_then(|meta| Some((meta.modified().ok()?, meta.len())));
        if stamp != state.stamp {
            state.stamp = stamp;
            state.generation += 1;
            let badge = read(path, stamp, state.generation);
            match (&state.badge, &badge) {
                (Some(old), None) => state.leaving = Some((old.clone(), Instant::now())),
                // A picture appearing slides in; one replacing another swaps.
                (None, Some(_)) => {
                    state.since = Instant::now();
                    state.leaving = None;
                }
                _ => {}
            }
            state.badge = badge;
        }
    }
    if let Some(badge) = &state.badge {
        return Some(Shown {
            badge: badge.clone(),
            shown: eased(state.since),
        });
    }
    let (badge, since) = state.leaving.clone()?;
    if since.elapsed() >= SLIDE {
        state.leaving = None;
        return None;
    }
    Some(Shown {
        badge,
        shown: 1.0 - eased(since),
    })
}

fn read(
    path: &std::path::Path,
    stamp: Option<(SystemTime, u64)>,
    generation: u64,
) -> Option<Arc<Badge>> {
    stamp.filter(|(_, length)| *length > 0)?;
    let bytes = std::fs::read(path).ok()?;
    let Some((width, height, rgba)) = parse_pam(&bytes) else {
        crate::gpu::trace_once(format!(
            "{IMAGE_ENV}: {} is not an RGB_ALPHA PAM with maxval 255",
            path.display()
        ));
        return None;
    };
    Some(Arc::new(Badge {
        generation,
        width,
        height,
        rgba,
    }))
}

/// Width, height and RGBA bytes of a `P7` `RGB_ALPHA` PAM with maxval 255.
fn parse_pam(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let end = bytes.windows(7).position(|w| w == b"ENDHDR\n")?;
    let header = std::str::from_utf8(&bytes[..end]).ok()?;
    let mut lines = header.lines().filter(|line| !line.starts_with('#'));
    if lines.next()?.trim() != "P7" {
        return None;
    }
    let (mut width, mut height, mut depth, mut maxval, mut tuple) = (0, 0, 0, 0, "");
    for line in lines {
        let (key, value) = line.trim().split_once(' ')?;
        match key {
            "WIDTH" => width = value.trim().parse().ok()?,
            "HEIGHT" => height = value.trim().parse().ok()?,
            "DEPTH" => depth = value.trim().parse().ok()?,
            "MAXVAL" => maxval = value.trim().parse().ok()?,
            "TUPLTYPE" => tuple = value.trim(),
            _ => {}
        }
    }
    if depth != 4 || maxval != 255 || tuple != "RGB_ALPHA" || width == 0 || height == 0 {
        return None;
    }
    let start = end + "ENDHDR\n".len();
    let length = width as usize * height as usize * TEXEL_BYTES;
    let rgba = bytes.get(start..start + length)?.to_vec();
    Some((width, height, rgba))
}

/// The picture's texels in `format`, or `None` for a format this does not
/// write: then nothing is shown rather than wrong colours.
pub(super) fn texels(badge: &Badge, format: vk::Format) -> Option<Vec<u8>> {
    let blue_first = match format {
        vk::Format::B8G8R8A8_UNORM | vk::Format::B8G8R8A8_SRGB => true,
        vk::Format::R8G8B8A8_UNORM | vk::Format::R8G8B8A8_SRGB => false,
        other => {
            crate::gpu::trace_once(format!(
                "{IMAGE_ENV}: not shown on a swapchain of format {other:?}"
            ));
            return None;
        }
    };
    let mut out = Vec::with_capacity(badge.rgba.len());
    for &[red, green, blue, _] in badge.rgba.as_chunks::<4>().0 {
        match blue_first {
            true => out.extend_from_slice(&[blue, green, red, u8::MAX]),
            false => out.extend_from_slice(&[red, green, blue, u8::MAX]),
        }
    }
    Some(out)
}

/// The visible pixels of each row as `(row, first column, length)`.
pub(super) fn runs(badge: &Badge) -> Vec<(u32, u32, u32)> {
    let mut runs = Vec::new();
    for row in 0..badge.height {
        let start = (row * badge.width) as usize * TEXEL_BYTES;
        let pixels = &badge.rgba[start..start + badge.width as usize * TEXEL_BYTES];
        let mut open: Option<u32> = None;
        for (column, pixel) in pixels.as_chunks::<4>().0.iter().enumerate() {
            let column = column as u32;
            match (pixel[3] >= OPAQUE_ENOUGH, open) {
                (true, None) => open = Some(column),
                (false, Some(first)) => {
                    runs.push((row, first, column - first));
                    open = None;
                }
                _ => {}
            }
        }
        if let Some(first) = open {
            runs.push((row, first, badge.width - first));
        }
    }
    runs
}

/// Where the picture's top-left corner goes in an image of `extent` when it
/// has slid `shown` of the way in, or `None` when it does not fit across. The
/// row may be above or below the image while it slides.
pub(super) fn origin(
    position: Position,
    extent: vk::Extent2D,
    width: u32,
    height: u32,
    shown: f32,
) -> Option<(i32, i32)> {
    let right = extent.width.checked_sub(width + MARGIN)?;
    let bottom = extent.height.checked_sub(height + MARGIN)?;
    if right < MARGIN || bottom < MARGIN {
        return None;
    }
    let x = match position {
        Position::Top | Position::Bottom => (extent.width - width) / 2,
        Position::TopLeft | Position::BottomLeft => MARGIN,
        Position::TopRight | Position::BottomRight => right,
    } as i32;
    let (rest, out) = match position.enters_from_top() {
        true => (MARGIN as i32, -(height as i32)),
        false => (bottom as i32, extent.height as i32),
    };
    let y = out + ((rest - out) as f32 * shown).round() as i32;
    Some((x, y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pam(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut file = format!(
            "P7\nWIDTH {width}\nHEIGHT {height}\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n"
        )
        .into_bytes();
        file.extend_from_slice(rgba);
        file
    }

    #[test]
    fn a_pam_is_read() {
        let pixels = [1, 2, 3, 255, 4, 5, 6, 0];
        assert_eq!(
            parse_pam(&pam(2, 1, &pixels)),
            Some((2, 1, pixels.to_vec()))
        );
    }

    #[test]
    fn a_short_or_foreign_pam_is_refused() {
        assert_eq!(parse_pam(&pam(2, 1, &[1, 2, 3, 4])), None);
        assert_eq!(parse_pam(b"P6\n1 1\n255\n\0\0\0"), None);
        let rgb = b"P7\nWIDTH 1\nHEIGHT 1\nDEPTH 3\nMAXVAL 255\nTUPLTYPE RGB\nENDHDR\n\0\0\0";
        assert_eq!(parse_pam(rgb), None);
    }

    #[test]
    fn texels_follow_the_swapchain_byte_order() {
        let badge = Badge {
            generation: 1,
            width: 1,
            height: 1,
            rgba: vec![10, 20, 30, 200],
        };
        assert_eq!(
            texels(&badge, vk::Format::B8G8R8A8_UNORM),
            Some(vec![30, 20, 10, 255])
        );
        assert_eq!(
            texels(&badge, vk::Format::R8G8B8A8_SRGB),
            Some(vec![10, 20, 30, 255])
        );
        assert_eq!(texels(&badge, vk::Format::A2B10G10R10_UNORM_PACK32), None);
    }

    #[test]
    fn only_visible_pixels_are_copied() {
        let clear = [0, 0, 0, 0];
        let solid = [9, 9, 9, 255];
        let rgba = [clear, solid, solid, clear, solid, clear, solid, solid].concat();
        let badge = Badge {
            generation: 1,
            width: 4,
            height: 2,
            rgba,
        };
        assert_eq!(runs(&badge), vec![(0, 1, 2), (1, 0, 1), (1, 2, 2)]);
    }

    #[test]
    fn each_position_keeps_its_margin_once_in_place() {
        let extent = vk::Extent2D {
            width: 100,
            height: 50,
        };
        let at = |position| origin(position, extent, 20, 10, 1.0);
        assert_eq!(at(Position::Top), Some((40, 12)));
        assert_eq!(at(Position::Bottom), Some((40, 28)));
        assert_eq!(at(Position::TopLeft), Some((12, 12)));
        assert_eq!(at(Position::TopRight), Some((68, 12)));
        assert_eq!(at(Position::BottomLeft), Some((12, 28)));
        assert_eq!(at(Position::BottomRight), Some((68, 28)));
        assert_eq!(origin(Position::Top, extent, 90, 10, 1.0), None);
    }

    #[test]
    fn a_picture_slides_in_from_its_edge() {
        let extent = vk::Extent2D {
            width: 100,
            height: 50,
        };
        assert_eq!(origin(Position::Top, extent, 20, 10, 0.0), Some((40, -10)));
        assert_eq!(
            origin(Position::Bottom, extent, 20, 10, 0.0),
            Some((40, 50))
        );
        assert_eq!(origin(Position::Top, extent, 20, 10, 0.5), Some((40, 1)));
    }
}
