//! The CPU decoder's place in the registry, for when there is one: a
//! [`Backend`] with `is_hardware() == false` that emits [`Planes::I420`]
//! pictures. Until then it reports itself unavailable, so naming `cpu` in
//! `ROSEMARY_VIDEO_DECODERS` changes nothing.
//!
//! [`Planes::I420`]: super::Planes::I420

use super::{Backend, Decoder, StreamConfig};

pub(crate) struct CpuBackend;

pub(crate) static CPU: CpuBackend = CpuBackend;

const NOT_YET: &str = "no CPU decoder yet";

impl Backend for CpuBackend {
    fn name(&self) -> &'static str {
        "cpu"
    }

    fn is_hardware(&self) -> bool {
        false
    }

    fn available(&self) -> Result<(), String> {
        Err(NOT_YET.to_string())
    }

    fn open(&self, _config: &StreamConfig) -> Result<Box<dyn Decoder>, String> {
        Err(NOT_YET.to_string())
    }

    fn set_unhealthy(&self) {}
}
