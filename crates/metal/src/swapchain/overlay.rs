//! `ROSEMARY_OVERLAY_IMAGE`: a small picture a launcher asks to see over the
//! window, copied into a corner of every frame after the guest's own.
//!
//! The picture is a binary PPM (`P6`, maxval 255), the simplest format there
//! is, so the launcher draws the text and this only copies pixels. The file is
//! read again when it changes; an empty or missing file shows nothing.
//! `ROSEMARY_OVERLAY_CORNER` is `top-left`, `top-right` (the default),
//! `bottom-left` or `bottom-right`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use ash::vk;

pub(super) const IMAGE_ENV: &str = "ROSEMARY_OVERLAY_IMAGE";
pub(super) const CORNER_ENV: &str = "ROSEMARY_OVERLAY_CORNER";

/// How often the file is looked at for a change.
const RECHECK: Duration = Duration::from_millis(500);

/// Space between the picture and the window's edges, in pixels.
pub(super) const MARGIN: u32 = 12;

/// Bytes a texel of the swapchain formats this copies into takes.
const TEXEL_BYTES: usize = 4;

/// The picture as read from the file. `generation` changes whenever the file
/// does, so a swapchain knows to upload it again.
pub(super) struct Badge {
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    rgb: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

struct State {
    checked: Option<Instant>,
    stamp: Option<(SystemTime, u64)>,
    generation: u64,
    badge: Option<Arc<Badge>>,
}

static STATE: Mutex<State> = Mutex::new(State {
    checked: None,
    stamp: None,
    generation: 0,
    badge: None,
});

fn path() -> Option<&'static std::path::Path> {
    static PATH: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        std::env::var_os(IMAGE_ENV)
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from)
    })
    .as_deref()
}

pub(super) fn corner() -> Corner {
    static CORNER: std::sync::OnceLock<Corner> = std::sync::OnceLock::new();
    *CORNER.get_or_init(
        || match std::env::var(CORNER_ENV).as_deref().map(str::trim) {
            Ok("top-left") => Corner::TopLeft,
            Ok("bottom-left") => Corner::BottomLeft,
            Ok("bottom-right") => Corner::BottomRight,
            Ok("top-right") | Err(_) => Corner::TopRight,
            Ok(other) => {
                crate::gpu::trace_once(format!(
                    "{CORNER_ENV}={other} is not a corner; using top-right"
                ));
                Corner::TopRight
            }
        },
    )
}

/// The picture to show now, if any.
pub(super) fn current() -> Option<Arc<Badge>> {
    let path = path()?;
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if state.checked.is_none_or(|at| at.elapsed() >= RECHECK) {
        state.checked = Some(Instant::now());
        let stamp = std::fs::metadata(path)
            .ok()
            .and_then(|meta| Some((meta.modified().ok()?, meta.len())));
        if stamp != state.stamp {
            state.stamp = stamp;
            state.generation += 1;
            let generation = state.generation;
            state.badge = stamp
                .filter(|(_, length)| *length > 0)
                .and_then(|_| std::fs::read(path).ok())
                .and_then(|bytes| {
                    let parsed = parse_ppm(&bytes);
                    if parsed.is_none() {
                        crate::gpu::trace_once(format!(
                            "{IMAGE_ENV}: {} is not a binary PPM with maxval 255",
                            path.display()
                        ));
                    }
                    parsed
                })
                .map(|(width, height, rgb)| {
                    Arc::new(Badge {
                        generation,
                        width,
                        height,
                        rgb,
                    })
                });
        }
    }
    state.badge.clone()
}

/// Width, height and RGB bytes of a `P6` PPM with maxval 255.
fn parse_ppm(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut at = 0;
    let mut field = || -> Option<&[u8]> {
        loop {
            while bytes.get(at)?.is_ascii_whitespace() {
                at += 1;
            }
            if bytes[at] != b'#' {
                break;
            }
            while *bytes.get(at)? != b'\n' {
                at += 1;
            }
        }
        let start = at;
        while !bytes.get(at)?.is_ascii_whitespace() {
            at += 1;
        }
        Some(&bytes[start..at])
    };
    let number = |field: &[u8]| std::str::from_utf8(field).ok()?.parse::<u32>().ok();
    if field()? != b"P6" {
        return None;
    }
    let width = number(field()?)?;
    let height = number(field()?)?;
    if number(field()?)? != 255 || width == 0 || height == 0 {
        return None;
    }
    // One whitespace byte separates the header from the pixels.
    let start = at + 1;
    let length = width as usize * height as usize * 3;
    let rgb = bytes.get(start..start + length)?.to_vec();
    Some((width, height, rgb))
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
    let mut out = Vec::with_capacity(badge.rgb.len() / 3 * TEXEL_BYTES);
    for &[red, green, blue] in badge.rgb.as_chunks::<3>().0 {
        match blue_first {
            true => out.extend_from_slice(&[blue, green, red, u8::MAX]),
            false => out.extend_from_slice(&[red, green, blue, u8::MAX]),
        }
    }
    Some(out)
}

/// Where the picture's top-left corner goes in an image of `extent`, or `None`
/// when the picture and its margins do not fit.
pub(super) fn origin(
    corner: Corner,
    extent: vk::Extent2D,
    width: u32,
    height: u32,
) -> Option<(i32, i32)> {
    let right = extent.width.checked_sub(width + MARGIN)?;
    let bottom = extent.height.checked_sub(height + MARGIN)?;
    if right < MARGIN || bottom < MARGIN {
        return None;
    }
    let (x, y) = match corner {
        Corner::TopLeft => (MARGIN, MARGIN),
        Corner::TopRight => (right, MARGIN),
        Corner::BottomLeft => (MARGIN, bottom),
        Corner::BottomRight => (right, bottom),
    };
    Some((x as i32, y as i32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ppm_with_a_comment_is_read() {
        let mut file = b"P6\n# made by a launcher\n2 1\n255\n".to_vec();
        file.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(parse_ppm(&file), Some((2, 1, vec![1, 2, 3, 4, 5, 6])));
    }

    #[test]
    fn a_short_or_foreign_ppm_is_refused() {
        assert_eq!(parse_ppm(b"P6\n2 1\n255\n\x01\x02"), None);
        assert_eq!(parse_ppm(b"P3\n1 1\n255\n1 2 3"), None);
        assert_eq!(parse_ppm(b"P6\n1 1\n65535\n\0\0\0\0\0\0"), None);
    }

    #[test]
    fn texels_follow_the_swapchain_byte_order() {
        let badge = Badge {
            generation: 1,
            width: 1,
            height: 1,
            rgb: vec![10, 20, 30],
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
    fn each_corner_keeps_its_margin() {
        let extent = vk::Extent2D {
            width: 100,
            height: 50,
        };
        assert_eq!(origin(Corner::TopLeft, extent, 20, 10), Some((12, 12)));
        assert_eq!(origin(Corner::TopRight, extent, 20, 10), Some((68, 12)));
        assert_eq!(origin(Corner::BottomLeft, extent, 20, 10), Some((12, 28)));
        assert_eq!(origin(Corner::BottomRight, extent, 20, 10), Some((68, 28)));
        assert_eq!(origin(Corner::TopLeft, extent, 90, 10), None);
    }
}
