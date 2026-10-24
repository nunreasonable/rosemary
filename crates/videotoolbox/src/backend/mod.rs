//! What decodes the pictures behind a decompression session.
//!
//! A backend is a decoder implementation: FFmpeg with one hardware API each
//! (Vulkan Video, VA-API, CUDA, VDPAU), and a CPU decoder once there is one.
//! A session opens one [`Decoder`] from the first backend in the
//! [`Registry`] that can take its stream. Backends are probed lazily, once
//! per process, with a tiny clip whose pictures are known, so a host whose
//! driver accepts a stream it then decodes wrongly is not chosen.
//!
//! `ROSEMARY_VIDEO_DECODERS` orders the backends; see `docs/usage.md`.

pub(crate) mod cpu;
pub(crate) mod ffmpeg;
#[cfg(test)]
pub(crate) mod mock;

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use coremedia::h264;

/// The probe clip: High profile, 64x64, six frames coded `IDR P B B P B` and
/// shown `I B B P B P`. Shown frame `n` has luma `40 + 30n` everywhere, Cb 90
/// on the left half and 170 on the right, Cr 100 on the top half and 160 on
/// the bottom. Made with:
///
/// ```text
/// ffmpeg -f lavfi -i "nullsrc=s=64x64:r=30:d=0.2,format=yuv420p,geq=lum='40+N*30':cb='if(lt(X,32*SW),90,170)':cr='if(lt(Y,32*SH),100,160)'" -c:v libx264 -profile:v high -preset veryslow -qp 4 -x264-params "keyint=6:min-keyint=6:scenecut=0:b-pyramid=none:bframes=2:ref=2" -pix_fmt yuv420p -bsf:v h264_mp4toannexb,filter_units=remove_types=6 probe.h264
/// ```
pub(crate) const PROBE_CLIP: &[u8] = include_bytes!("probe.h264");

/// The display index of each of the probe clip's pictures, in coded order.
#[cfg(test)]
pub(crate) const PROBE_DISPLAY_ORDER: [u64; 6] = [0, 3, 1, 2, 5, 4];

/// The backends tried when `ROSEMARY_VIDEO_DECODERS` is not set.
const DEFAULT_ORDER: &str = "vulkan,vaapi,cuda,cpu";

/// What a decoder is opened for.
pub(crate) struct StreamConfig<'a> {
    pub sps: &'a h264::Sps,
    /// The SPSs, then the PPSs, as NAL units without start codes. FFmpeg
    /// gets them in band; a decoder that wants them up front reads them here.
    #[allow(dead_code)]
    pub parameter_sets: &'a [Vec<u8>],
}

/// One coded picture in Annex B form: start codes, no length prefixes.
pub(crate) struct AccessUnit {
    pub annex_b: Vec<u8>,
    /// Where a decoder that lost its references can start again.
    #[allow(dead_code)]
    pub idr: bool,
}

/// The planes of a decoded picture, borrowed until the sink returns.
pub(crate) enum Planes<'a> {
    Nv12 {
        y: &'a [u8],
        y_stride: usize,
        uv: &'a [u8],
        uv_stride: usize,
    },
    /// What a CPU decoder emits.
    #[allow(dead_code)]
    I420 {
        y: &'a [u8],
        y_stride: usize,
        u: &'a [u8],
        u_stride: usize,
        v: &'a [u8],
        v_stride: usize,
    },
}

/// The colour description a picture carries, as ITU-T H.273 code points;
/// 2 is "unspecified" for the three code points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Colour {
    pub full_range: Option<bool>,
    pub matrix: u8,
    pub primaries: u8,
    pub transfer: u8,
}

impl Colour {
    #[cfg(test)]
    pub const UNSPECIFIED: Colour = Colour {
        full_range: None,
        matrix: 2,
        primaries: 2,
        transfer: 2,
    };
}

/// A decoded picture. `token` is the one its access unit was sent with.
pub(crate) struct Picture<'a> {
    pub token: u64,
    pub width: usize,
    pub height: usize,
    pub planes: Planes<'a>,
    pub colour: Colour,
}

/// Why a decoder could not go on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DecodeError {
    /// The input was not decodable; the decoder carries on.
    BadData,
    /// The decoder is broken. `global` when the backend itself is, not just
    /// this stream on it.
    Malfunction { global: bool },
    /// A picture came out in a form the session cannot hand on.
    UnsupportedOutput,
}

pub(crate) trait Decoder: Send {
    fn send(&mut self, unit: &AccessUnit, token: u64) -> Result<(), DecodeError>;
    /// Frames ready now, in display order; `sink` copies before returning.
    fn receive(&mut self, sink: &mut dyn FnMut(Picture<'_>)) -> Result<(), DecodeError>;
    /// Emit every held frame, then accept input again (resumes at next IDR).
    fn drain(&mut self, sink: &mut dyn FnMut(Picture<'_>)) -> Result<(), DecodeError>;
}

pub(crate) trait Backend: Sync {
    fn name(&self) -> &'static str;
    fn is_hardware(&self) -> bool;
    /// Whether the backend works here: its cached probe, and its health.
    fn available(&self) -> Result<(), String>;
    fn open(&self, config: &StreamConfig) -> Result<Box<dyn Decoder>, String>;
    /// Stop offering this backend for the rest of the process, after it
    /// failed in a way no stream causes.
    fn set_unhealthy(&self);
    /// How the session announces it.
    fn description(&self) -> String {
        self.name().to_string()
    }
}

/// Which kinds of backend a decoder specification allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HwPolicy {
    pub hardware: bool,
    pub software: bool,
}

impl HwPolicy {
    pub const ANY: HwPolicy = HwPolicy {
        hardware: true,
        software: true,
    };

    fn allows(&self, backend: &dyn Backend) -> bool {
        match backend.is_hardware() {
            true => self.hardware,
            false => self.software,
        }
    }
}

/// The backends a session may use, in the order they are tried.
pub(crate) struct Registry {
    backends: Vec<&'static dyn Backend>,
}

/// The backend a name in `ROSEMARY_VIDEO_DECODERS` stands for. `Some(None)`
/// for a name that stands for nothing, on purpose.
fn named(name: &str) -> Option<Option<&'static dyn Backend>> {
    Some(Some(match name {
        "vulkan" => &ffmpeg::VULKAN,
        "vaapi" => &ffmpeg::VAAPI,
        "cuda" => &ffmpeg::CUDA,
        "vdpau" => &ffmpeg::VDPAU,
        "cpu" => &cpu::CPU,
        "none" => return Some(None),
        _ => return None,
    }))
}

/// The backends a comma-separated list names, without repeats, and the
/// names it has that stand for none.
pub(crate) fn parse_names(list: &str) -> (Vec<&'static dyn Backend>, Vec<String>) {
    let (mut backends, mut unknown): (Vec<&'static dyn Backend>, Vec<String>) = Default::default();
    for name in list.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        match named(&name.to_ascii_lowercase()) {
            Some(Some(backend)) => {
                if !backends.iter().any(|b| b.name() == backend.name()) {
                    backends.push(backend);
                }
            }
            Some(None) => {}
            None => unknown.push(name.to_string()),
        }
    }
    (backends, unknown)
}

impl Registry {
    pub fn new(backends: Vec<&'static dyn Backend>) -> Registry {
        Registry { backends }
    }

    /// The process's registry, from `ROSEMARY_VIDEO_DECODERS`.
    pub fn global() -> &'static Registry {
        static GLOBAL: OnceLock<Registry> = OnceLock::new();
        GLOBAL.get_or_init(|| {
            let list = std::env::var("ROSEMARY_VIDEO_DECODERS")
                .ok()
                .filter(|v| !v.trim().is_empty());
            Registry::from_names(list.as_deref().unwrap_or(DEFAULT_ORDER))
        })
    }

    /// The backends `list` names, in its order. Unknown names are reported
    /// and skipped.
    pub fn from_names(list: &str) -> Registry {
        let (backends, unknown) = parse_names(list);
        for name in unknown {
            report_once(format!(
                "rosemary: VideoToolbox: ROSEMARY_VIDEO_DECODERS names no decoder `{name}`; \
                 the names are vulkan, vaapi, cuda, vdpau, cpu and none"
            ));
        }
        Registry::new(backends)
    }

    #[cfg(test)]
    pub fn backends(&self) -> &[&'static dyn Backend] {
        &self.backends
    }

    /// A decoder for `config` from the first backend `policy` allows that
    /// works and takes the stream, or every backend's reason it did not.
    pub fn open(
        &self,
        config: &StreamConfig,
        policy: HwPolicy,
    ) -> Result<(Box<dyn Decoder>, &'static dyn Backend), String> {
        let mut reasons = Vec::new();
        for &backend in &self.backends {
            if !policy.allows(backend) {
                continue;
            }
            let opened = backend.available().and_then(|()| backend.open(config));
            match opened {
                Ok(decoder) => return Ok((decoder, backend)),
                Err(why) => reasons.push(format!("{}: {why}", backend.name())),
            }
        }
        if reasons.is_empty() {
            reasons.push(match self.backends.is_empty() {
                true => "none is enabled (ROSEMARY_VIDEO_DECODERS)".to_string(),
                false => "the decoder specification allows none of the enabled ones".to_string(),
            });
        }
        Err(reasons.join("; "))
    }

    /// `VTIsHardwareDecodeSupported`: H.264 when a hardware backend works.
    pub fn hardware_supports(&self, codec: u32) -> bool {
        codec == coremedia::CODEC_H264
            && self
                .backends
                .iter()
                .any(|b| b.is_hardware() && b.available().is_ok())
    }
}

/// Print `message` on stderr, unless it has been already.
pub(crate) fn report_once(message: String) {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(Default::default);
    let fresh = seen
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(message.clone());
    if fresh {
        eprintln!("{message}");
    }
}

/// Whether a backend can decode this stream at all: 8-bit 4:2:0 in the
/// Baseline, Main or High profile, at most 4096 square.
pub(crate) fn check_stream(sps: &h264::Sps) -> Result<(), String> {
    if sps.chroma_format_idc != 1 {
        return Err(format!(
            "chroma_format_idc {} is not 4:2:0",
            sps.chroma_format_idc
        ));
    }
    if sps.bit_depth_luma_minus8 != 0 || sps.bit_depth_chroma_minus8 != 0 {
        return Err(format!(
            "{}-bit pictures are not supported",
            8 + sps.bit_depth_luma_minus8.max(sps.bit_depth_chroma_minus8)
        ));
    }
    if !matches!(sps.profile_idc, 66 | 77 | 100) {
        return Err(format!("profile_idc {} is not supported", sps.profile_idc));
    }
    if sps.width() > 4096 || sps.height() > 4096 {
        return Err(format!(
            "{}x{} is larger than 4096x4096",
            sps.width(),
            sps.height()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::mock::MockBackend;
    use super::*;

    fn names(registry: &Registry) -> Vec<&'static str> {
        registry.backends().iter().map(|b| b.name()).collect()
    }

    fn probe_sps() -> (h264::Sps, Vec<Vec<u8>>) {
        let units = crate::bitstream::split_annex_b(PROBE_CLIP);
        let sets: Vec<Vec<u8>> = units[..2].iter().map(|u| u.to_vec()).collect();
        (h264::parse_sps(&sets[0]).expect("the probe's SPS"), sets)
    }

    #[test]
    fn registry_order_comes_from_the_list() {
        assert_eq!(
            names(&Registry::from_names("cuda, vulkan,cuda,VAAPI")),
            ["cuda", "vulkan", "vaapi"]
        );
        assert_eq!(
            names(&Registry::from_names(DEFAULT_ORDER)),
            ["vulkan", "vaapi", "cuda", "cpu"]
        );
    }

    #[test]
    fn none_gives_an_empty_registry() {
        let registry = Registry::from_names("none");
        assert!(registry.backends().is_empty());
        let (sps, sets) = probe_sps();
        let config = StreamConfig {
            sps: &sps,
            parameter_sets: &sets,
        };
        let why = registry.open(&config, HwPolicy::ANY).err().unwrap();
        assert!(why.contains("none is enabled"), "{why}");
        assert!(!registry.hardware_supports(coremedia::CODEC_H264));
    }

    #[test]
    fn unknown_names_are_reported_and_skipped() {
        let (backends, unknown) = parse_names("vulkan,nvdec,,none,cpu");
        let names: Vec<_> = backends.iter().map(|b| b.name()).collect();
        assert_eq!(names, ["vulkan", "cpu"]);
        assert_eq!(unknown, ["nvdec"]);
        assert_eq!(
            Registry::from_names("vulkan,nvdec").backends().len(),
            1,
            "the unknown name is skipped"
        );
    }

    #[test]
    fn require_hardware_excludes_cpu() {
        let software = MockBackend::new("soft").software().leak();
        let hardware = MockBackend::new("hard").leak();
        let registry = Registry::new(vec![software, hardware]);
        let (sps, sets) = probe_sps();
        let config = StreamConfig {
            sps: &sps,
            parameter_sets: &sets,
        };
        let only_hardware = HwPolicy {
            hardware: true,
            software: false,
        };
        let (_, chosen) = registry.open(&config, only_hardware).ok().unwrap();
        assert_eq!(chosen.name(), "hard");
        let (_, chosen) = registry.open(&config, HwPolicy::ANY).ok().unwrap();
        assert_eq!(chosen.name(), "soft", "in list order otherwise");
        let software_only = Registry::new(vec![software]);
        assert!(software_only.open(&config, only_hardware).is_err());
        assert!(registry.hardware_supports(coremedia::CODEC_H264));
        assert!(!software_only.hardware_supports(coremedia::CODEC_H264));
    }

    #[test]
    fn enable_false_excludes_hardware() {
        let hardware = MockBackend::new("hard").leak();
        let software = MockBackend::new("soft").software().leak();
        let registry = Registry::new(vec![hardware, software]);
        let (sps, sets) = probe_sps();
        let config = StreamConfig {
            sps: &sps,
            parameter_sets: &sets,
        };
        let no_hardware = HwPolicy {
            hardware: false,
            software: true,
        };
        let (_, chosen) = registry.open(&config, no_hardware).ok().unwrap();
        assert_eq!(chosen.name(), "soft");
        let hardware_only = Registry::new(vec![hardware]);
        let why = hardware_only.open(&config, no_hardware).err().unwrap();
        assert!(why.contains("allows none"), "{why}");
    }

    #[test]
    fn an_unhealthy_backend_is_passed_over() {
        let first = MockBackend::new("first").leak();
        let second = MockBackend::new("second").leak();
        let registry = Registry::new(vec![first, second]);
        let (sps, sets) = probe_sps();
        let config = StreamConfig {
            sps: &sps,
            parameter_sets: &sets,
        };
        first.set_unhealthy();
        let (_, chosen) = registry.open(&config, HwPolicy::ANY).ok().unwrap();
        assert_eq!(chosen.name(), "second");
    }

    #[test]
    fn missing_library_is_unavailable_not_fatal() {
        let backend = ffmpeg::FfmpegBackend::bogus();
        let why = backend.available().unwrap_err();
        assert!(why.contains("not installed"), "{why}");
        let registry = Registry::new(vec![backend]);
        assert!(!registry.hardware_supports(coremedia::CODEC_H264));
        let (sps, sets) = probe_sps();
        let config = StreamConfig {
            sps: &sps,
            parameter_sets: &sets,
        };
        assert!(registry.open(&config, HwPolicy::ANY).is_err());
    }

    #[test]
    fn the_cpu_placeholder_decodes_nothing_yet() {
        assert!(!cpu::CPU.is_hardware());
        assert!(cpu::CPU.available().is_err());
    }

    #[test]
    fn streams_outside_8_bit_4_2_0_baseline_main_high_are_refused() {
        let (sps, _) = probe_sps();
        assert_eq!(check_stream(&sps), Ok(()));
        let ten_bit = h264::Sps {
            bit_depth_luma_minus8: 2,
            ..sps
        };
        assert!(check_stream(&ten_bit).is_err());
        let four_two_two = h264::Sps {
            chroma_format_idc: 2,
            ..sps
        };
        assert!(check_stream(&four_two_two).is_err());
        let high_10 = h264::Sps {
            profile_idc: 110,
            ..sps
        };
        assert!(check_stream(&high_10).is_err());
        let huge = h264::Sps {
            coded_width: 8192,
            ..sps
        };
        assert!(check_stream(&huge).is_err());
    }
}
