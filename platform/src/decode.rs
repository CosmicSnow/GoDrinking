//! Optional native video decoding contract. No OS objects cross this boundary.
pub struct Nv12Picture {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

/// Created, used and dropped serially on the decoder worker thread.
pub trait VideoDecoder {
    fn decode(&mut self, annex_b: &[u8]) -> Result<Option<Nv12Picture>, String>;
    /// Last valid SPS/PPS, in Annex B form, for a software recovery at an IDR.
    fn parameter_sets(&self) -> Vec<u8>;
}

pub type DecoderFactory = fn() -> Result<Box<dyn VideoDecoder>, String>;
