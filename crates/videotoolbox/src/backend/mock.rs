//! A decoder for tests: it reads which picture an access unit is from the
//! unit itself, holds pictures back to reorder them, and fails on request.
//!
//! An access unit's last NAL unit is `[header, display index]`. The picture
//! for display index `n` has luma `luma(n)` everywhere and the configured
//! chroma.

use std::sync::atomic::{AtomicBool, Ordering};

use super::{AccessUnit, Backend, Colour, DecodeError, Decoder, Picture, Planes, StreamConfig};

#[derive(Clone)]
pub(crate) struct MockBackend {
    pub name: &'static str,
    pub hardware: bool,
    /// How many pictures are held back before the next comes out.
    pub delay: usize,
    /// The access unit whose `send` reports bad data.
    pub bad_data_at: Option<u64>,
    /// The access unit whose `send` breaks the decoder.
    pub malfunction_at: Option<(u64, bool)>,
    /// The access unit the decoder takes and never gives a picture for.
    pub lose_at: Option<u64>,
    pub colour: Colour,
    /// Cb and Cr of every picture.
    pub chroma: (u8, u8),
    /// A luma for every picture instead of one from its display index.
    pub luma: Option<u8>,
    /// Emit I420 rather than NV12.
    pub i420: bool,
    pub unhealthy: &'static AtomicBool,
}

/// The luma of the picture shown `n`th.
pub(crate) fn luma(n: u64) -> u8 {
    (16 + 20 * n).min(235) as u8
}

impl MockBackend {
    pub fn new(name: &'static str) -> MockBackend {
        MockBackend {
            name,
            hardware: true,
            delay: 0,
            bad_data_at: None,
            malfunction_at: None,
            lose_at: None,
            colour: Colour::UNSPECIFIED,
            chroma: (90, 170),
            luma: None,
            i420: false,
            unhealthy: Box::leak(Box::new(AtomicBool::new(false))),
        }
    }

    pub fn software(self) -> MockBackend {
        MockBackend {
            hardware: false,
            ..self
        }
    }

    pub fn leak(self) -> &'static MockBackend {
        Box::leak(Box::new(self))
    }
}

impl Backend for MockBackend {
    fn name(&self) -> &'static str {
        self.name
    }

    fn is_hardware(&self) -> bool {
        self.hardware
    }

    fn available(&self) -> Result<(), String> {
        match self.unhealthy.load(Ordering::Relaxed) {
            true => Err("unhealthy".to_string()),
            false => Ok(()),
        }
    }

    fn open(&self, config: &StreamConfig) -> Result<Box<dyn Decoder>, String> {
        Ok(Box::new(MockDecoder {
            backend: self.clone(),
            width: config.sps.width() as usize,
            height: config.sps.height() as usize,
            held: Vec::new(),
        }))
    }

    fn set_unhealthy(&self) {
        self.unhealthy.store(true, Ordering::Relaxed);
    }
}

struct MockDecoder {
    backend: MockBackend,
    width: usize,
    height: usize,
    /// `(display index, token)` of each picture not yet out.
    held: Vec<(u64, u64)>,
}

impl MockDecoder {
    fn emit(&self, display: u64, token: u64, sink: &mut dyn FnMut(Picture<'_>)) {
        let (w, h) = (self.width, self.height);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let y = vec![self.backend.luma.unwrap_or(luma(display)); w * h];
        let (cb, cr) = self.backend.chroma;
        let colour = self.backend.colour;
        if self.backend.i420 {
            let (u, v) = (vec![cb; cw * ch], vec![cr; cw * ch]);
            sink(Picture {
                token,
                width: w,
                height: h,
                planes: Planes::I420 {
                    y: &y,
                    y_stride: w,
                    u: &u,
                    u_stride: cw,
                    v: &v,
                    v_stride: cw,
                },
                colour,
            });
        } else {
            let uv: Vec<u8> = (0..cw * ch).flat_map(|_| [cb, cr]).collect();
            sink(Picture {
                token,
                width: w,
                height: h,
                planes: Planes::Nv12 {
                    y: &y,
                    y_stride: w,
                    uv: &uv,
                    uv_stride: 2 * cw,
                },
                colour,
            });
        }
    }

    /// Emit the earliest-shown pictures until at most `keep` are held.
    fn release(&mut self, keep: usize, sink: &mut dyn FnMut(Picture<'_>)) {
        self.held.sort();
        while self.held.len() > keep {
            let (display, token) = self.held.remove(0);
            self.emit(display, token, sink);
        }
    }
}

impl Decoder for MockDecoder {
    fn send(&mut self, unit: &AccessUnit, token: u64) -> Result<(), DecodeError> {
        if let Some((at, global)) = self.backend.malfunction_at
            && at == token
        {
            return Err(DecodeError::Malfunction { global });
        }
        if self.backend.bad_data_at == Some(token) {
            return Err(DecodeError::BadData);
        }
        if self.backend.lose_at == Some(token) {
            return Ok(());
        }
        let units = crate::bitstream::split_annex_b(&unit.annex_b);
        let display = units
            .last()
            .and_then(|u| u.get(1))
            .ok_or(DecodeError::BadData)?;
        self.held.push((u64::from(*display), token));
        Ok(())
    }

    fn receive(&mut self, sink: &mut dyn FnMut(Picture<'_>)) -> Result<(), DecodeError> {
        self.release(self.backend.delay, sink);
        Ok(())
    }

    fn drain(&mut self, sink: &mut dyn FnMut(Picture<'_>)) -> Result<(), DecodeError> {
        self.release(0, sink);
        Ok(())
    }
}
