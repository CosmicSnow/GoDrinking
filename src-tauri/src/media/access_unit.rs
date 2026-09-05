//! H.264 access-unit normalization for native transport.

use std::collections::VecDeque;
use std::fmt::{Display, Formatter};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const ANNEX_B_START_CODE: &[u8] = &[0, 0, 0, 1];
#[allow(dead_code)]
pub(crate) const H264_PROFILE_LEVEL_ID: &str = "42e02a";

/// BGRA8 to NV12 (BT.709 limited) conversion for hardware encoder input.
/// Only used on Windows (Media Foundation NV12 path); lives here so the
/// math is unit-tested on every platform. Viewers expect 709, not 601.
pub(crate) fn bgra_to_nv12(bgra: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
        return None;
    }
    if bgra.len() < w.checked_mul(h)?.checked_mul(4)? {
        return None;
    }
    let y_size = w * h;
    let mut nv12 = vec![0_u8; y_size + y_size / 2];
    for y in 0..h {
        for x in 0..w {
            let src = (y * w + x) * 4;
            let b = bgra[src] as i32;
            let g = bgra[src + 1] as i32;
            let r = bgra[src + 2] as i32;
            // BT.709 limited: Y 16–235, Cb/Cr 16–240.
            let y_val = (16 + ((47 * r + 157 * g + 16 * b + 128) >> 8)).clamp(16, 235) as u8;
            nv12[y * w + x] = y_val;
            if x % 2 == 0 && y % 2 == 0 {
                let u_val = (128 + ((-26 * r - 87 * g + 112 * b + 128) >> 8)).clamp(16, 240) as u8;
                let v_val = (128 + ((112 * r - 102 * g - 10 * b + 128) >> 8)).clamp(16, 240) as u8;
                let uv = y_size + (y / 2) * w + x;
                nv12[uv] = u_val;
                nv12[uv + 1] = v_val;
            }
        }
    }
    Some(nv12)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EncodedAccessUnit {
    pub(crate) data: Vec<u8>,
    pub(crate) timestamp_90khz: u64,
    pub(crate) keyframe: bool,
    pub(crate) profile_level_id: Option<String>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum AvccError {
    TruncatedLength,
    TruncatedNal,
    Empty,
    MissingParameterSets,
    IncompatibleProfileLevel,
}

impl Display for AvccError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::TruncatedLength => "truncated AVCC NAL length",
            Self::TruncatedNal => "truncated AVCC NAL payload",
            Self::Empty => "empty AVCC access unit",
            Self::MissingParameterSets => "IDR access unit is missing SPS or PPS",
            Self::IncompatibleProfileLevel => {
                "H.264 SPS profile-level-id is incompatible with 1080p60"
            }
        })
    }
}

#[derive(Default)]
pub(crate) struct AvccAnnexBConverter {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    allow_high_profile: bool,
}

impl AvccAnnexBConverter {
    /// Baseline-strict (default) rejects non-Baseline SPS; the High-profile
    /// variant additionally accepts High (0x64) SPS for H.264 High sessions.
    pub(crate) fn high_profile() -> Self {
        Self {
            allow_high_profile: true,
            ..Default::default()
        }
    }
}

impl AvccAnnexBConverter {
    pub(crate) fn convert(
        &mut self,
        avcc: &[u8],
        timestamp_90khz: u64,
        keyframe: bool,
    ) -> Result<EncodedAccessUnit, AvccError> {
        if avcc.is_empty() {
            return Err(AvccError::Empty);
        }
        let mut offset = 0;
        let mut nals = Vec::new();
        while offset < avcc.len() {
            if avcc.len() - offset < 4 {
                return Err(AvccError::TruncatedLength);
            }
            let length = u32::from_be_bytes([
                avcc[offset],
                avcc[offset + 1],
                avcc[offset + 2],
                avcc[offset + 3],
            ]) as usize;
            offset += 4;
            let end = offset.checked_add(length).ok_or(AvccError::TruncatedNal)?;
            if end > avcc.len() || length == 0 {
                return Err(AvccError::TruncatedNal);
            }
            let nal = &avcc[offset..end];
            match nal[0] & 0x1f {
                7 => self.sps = Some(nal.to_vec()),
                8 => self.pps = Some(nal.to_vec()),
                _ => {}
            }
            nals.push(nal);
            offset = end;
        }

        let contains_idr = nals.iter().any(|nal| nal[0] & 0x1f == 5);
        if contains_idr && (self.sps.is_none() || self.pps.is_none()) {
            return Err(AvccError::MissingParameterSets);
        }
        let profile_level_id = self.sps.as_deref().and_then(sps_profile_level_id);
        let profile_ok = profile_level_id.as_deref().is_some_and(|id| {
            is_baseline_profile(id) || (self.allow_high_profile && is_high_profile(id))
        });
        if contains_idr && !profile_ok {
            return Err(AvccError::IncompatibleProfileLevel);
        }
        let inject_parameter_sets = keyframe || contains_idr;
        let mut data = Vec::with_capacity(avcc.len() + 64);
        if inject_parameter_sets {
            if let Some(sps) = &self.sps {
                data.extend_from_slice(ANNEX_B_START_CODE);
                data.extend_from_slice(sps);
            }
            if let Some(pps) = &self.pps {
                data.extend_from_slice(ANNEX_B_START_CODE);
                data.extend_from_slice(pps);
            }
        }
        for nal in nals {
            if inject_parameter_sets && matches!(nal[0] & 0x1f, 7 | 8) {
                continue;
            }
            data.extend_from_slice(ANNEX_B_START_CODE);
            data.extend_from_slice(nal);
        }
        Ok(EncodedAccessUnit {
            data,
            timestamp_90khz,
            keyframe: keyframe || contains_idr,
            profile_level_id,
        })
    }
}

/// HEVC NAL unit type from the two-byte header (forbidden(1) | type(6) | ...).
fn hevc_nal_unit_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|byte| (byte >> 1) & 0x3f)
}

/// IRAP pictures (BLA/IDR/CRA, types 16-23) are HEVC random-access points.
fn hevc_is_irap(nal_unit_type: u8) -> bool {
    (16..=23).contains(&nal_unit_type)
}

/// Length-prefixed (HVCC) to Annex-B, mirroring AvccAnnexBConverter but for
/// HEVC: caches VPS/SPS/PPS and injects them before every IRAP so a viewer
/// joining mid-stream (or recovering after loss) can decode immediately.
#[derive(Default)]
pub(crate) struct HevcAnnexBConverter {
    vps: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl HevcAnnexBConverter {
    pub(crate) fn convert(
        &mut self,
        hvcc: &[u8],
        timestamp_90khz: u64,
        keyframe: bool,
    ) -> Result<EncodedAccessUnit, AvccError> {
        if hvcc.is_empty() {
            return Err(AvccError::Empty);
        }
        let mut offset = 0;
        let mut nals = Vec::new();
        while offset < hvcc.len() {
            if hvcc.len() - offset < 4 {
                return Err(AvccError::TruncatedLength);
            }
            let length = u32::from_be_bytes([
                hvcc[offset],
                hvcc[offset + 1],
                hvcc[offset + 2],
                hvcc[offset + 3],
            ]) as usize;
            offset += 4;
            let end = offset.checked_add(length).ok_or(AvccError::TruncatedNal)?;
            if end > hvcc.len() || length == 0 {
                return Err(AvccError::TruncatedNal);
            }
            let nal = &hvcc[offset..end];
            match hevc_nal_unit_type(nal) {
                Some(HEVC_VPS_NUT) => self.vps = Some(nal.to_vec()),
                Some(HEVC_SPS_NUT) => self.sps = Some(nal.to_vec()),
                Some(HEVC_PPS_NUT) => self.pps = Some(nal.to_vec()),
                _ => {}
            }
            nals.push(nal);
            offset = end;
        }

        let contains_irap = nals
            .iter()
            .any(|nal| hevc_nal_unit_type(nal).is_some_and(hevc_is_irap));
        if contains_irap && (self.vps.is_none() || self.sps.is_none() || self.pps.is_none()) {
            return Err(AvccError::MissingParameterSets);
        }
        let inject_parameter_sets = keyframe || contains_irap;
        let mut data = Vec::with_capacity(hvcc.len() + 96);
        if inject_parameter_sets {
            for set in [&self.vps, &self.sps, &self.pps].into_iter().flatten() {
                data.extend_from_slice(ANNEX_B_START_CODE);
                data.extend_from_slice(set);
            }
        }
        for nal in nals {
            let nut = hevc_nal_unit_type(nal).unwrap_or(0xff);
            if inject_parameter_sets && matches!(nut, HEVC_VPS_NUT | HEVC_SPS_NUT | HEVC_PPS_NUT) {
                continue;
            }
            data.extend_from_slice(ANNEX_B_START_CODE);
            data.extend_from_slice(nal);
        }
        Ok(EncodedAccessUnit {
            data,
            timestamp_90khz,
            keyframe: keyframe || contains_irap,
            profile_level_id: None,
        })
    }
}

fn sps_profile_level_id(sps: &[u8]) -> Option<String> {
    (sps.len() >= 4).then(|| format!("{:02x}{:02x}{:02x}", sps[1], sps[2], sps[3]))
}

const HEVC_VPS_NUT: u8 = 32;
const HEVC_SPS_NUT: u8 = 33;
const HEVC_PPS_NUT: u8 = 34;
pub(crate) fn is_baseline_profile(profile_level_id: &str) -> bool {
    let id = profile_level_id.to_ascii_lowercase();
    id.len() == 6 && id.starts_with("42")
}

pub(crate) fn is_high_profile(profile_level_id: &str) -> bool {
    let id = profile_level_id.to_ascii_lowercase();
    id.len() == 6 && id.starts_with("64")
}

/// Phase-2A bounded-queue contract: drop a whole GOP (capacity 16) and stay
/// in recovering-until-IDR. Matches the pipeline queue and the per-viewer
/// transport sample channel so no stage can publish a partial GOP.
pub(crate) const ACCESS_UNIT_QUEUE_CAPACITY: usize = 16;

/// NAL unit types for an Annex-B buffer (handles 3- and 4-byte start codes).
pub(crate) fn annexb_nal_types(data: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push((i, 3));
            i += 3;
        } else if i + 4 <= data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1
        {
            starts.push((i, 4));
            i += 4;
        } else {
            i += 1;
        }
    }
    // Trailing start code with no payload contributes no NAL.
    for (index, (offset, len)) in starts.iter().enumerate() {
        let payload_start = offset + len;
        let payload_end = if index + 1 < starts.len() {
            starts[index + 1].0
        } else {
            data.len()
        };
        if payload_end > payload_start {
            types.push(data[payload_start] & 0x1f);
        }
    }
    types
}

/// Phase-2A first-segment contract: a decodable segment opens with
/// SPS(7), PPS(8), IDR(5) in order so a late joiner decodes immediately.
pub(crate) fn annexb_starts_with_sps_pps_idr(data: &[u8]) -> bool {
    let types = annexb_nal_types(data);
    types.len() >= 3 && types[0] == 7 && types[1] == 8 && types[2] == 5
}

/// True when any VCL slice (NAL type 1/5) carries a B slice_type (1 or 6).
/// Baseline/CBP never emits B slices, so any hit means the encoder left
/// the contract (wrong profile or misconfiguration).
pub(crate) fn annexb_contains_b_slice(data: &[u8]) -> bool {
    annexb_nals(data).iter().any(|nal| {
        let nal_type = nal[0] & 0x1f;
        (nal_type == 1 || nal_type == 5) && slice_is_b(&nal[1..])
    })
}

/// True when a PPS NAL signals CAVLC only (`entropy_coding_mode_flag == 0`,
/// always true for Baseline/CBP; CABAC is a High-profile feature).
pub(crate) fn pps_is_cavlc_only(pps: &[u8]) -> bool {
    if pps.is_empty() {
        return false;
    }
    let body = &pps[1..];
    if body.is_empty() {
        return false;
    }
    let rbsp = remove_emulation_prevention(body);
    let mut reader = BitReader::new(&rbsp);
    // pic_parameter_set_id, seq_parameter_set_id, then the flag.
    if reader.read_ue().is_none() || reader.read_ue().is_none() {
        return false;
    }
    reader.read_bit() == Some(false)
}

fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut offsets = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if i + 3 < data.len() && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            offsets.push((i, 3));
            i += 3;
        } else if i + 4 <= data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1
        {
            offsets.push((i, 4));
            i += 4;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::new();
    for (index, (offset, len)) in offsets.iter().enumerate() {
        let start = offset + len;
        let end = if index + 1 < offsets.len() {
            offsets[index + 1].0
        } else {
            data.len()
        };
        if end > start {
            nals.push(&data[start..end]);
        }
    }
    nals
}

fn slice_is_b(slice_payload: &[u8]) -> bool {
    let rbsp = remove_emulation_prevention(slice_payload);
    let mut reader = BitReader::new(&rbsp);
    // first_mb_in_slice, slice_type.
    if reader.read_ue().is_none() {
        return false;
    }
    matches!(reader.read_ue(), Some(1) | Some(6))
}

fn remove_emulation_prevention(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &byte in data {
        if zeros == 2 && byte == 0x03 {
            zeros = 0;
            continue;
        }
        out.push(byte);
        zeros = if byte == 0x00 { zeros + 1 } else { 0 };
    }
    out
}

struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn read_bit(&mut self) -> Option<bool> {
        let byte = *self.data.get(self.bit / 8)?;
        let bit = (byte >> (7 - (self.bit % 8))) & 1 == 1;
        self.bit += 1;
        Some(bit)
    }

    fn read_ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while self.read_bit()? == false {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let mut value = 0u32;
        for _ in 0..zeros {
            value = (value << 1) | u32::from(self.read_bit()?);
        }
        Some((1u32 << zeros) - 1 + value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AccessUnitPushResult {
    Enqueued,
    DroppedUntilKeyframe,
    Closed,
}

struct QueueState {
    units: VecDeque<EncodedAccessUnit>,
    recovering: bool,
    closed: bool,
}

struct QueueInner {
    state: Mutex<QueueState>,
    wake: Condvar,
    capacity: usize,
}

/// A bounded access-unit queue which never publishes a partial GOP. On
/// overflow it discards the buffered GOP and waits for the next decodable IDR.
#[derive(Clone)]
pub(crate) struct AccessUnitQueue {
    inner: Arc<QueueInner>,
}

#[allow(dead_code)]
pub(crate) struct AccessUnitReceiver {
    inner: Arc<QueueInner>,
}

impl AccessUnitQueue {
    pub(crate) fn bounded(capacity: usize) -> (Self, AccessUnitReceiver) {
        assert!(capacity > 0);
        let inner = Arc::new(QueueInner {
            state: Mutex::new(QueueState {
                units: VecDeque::with_capacity(capacity),
                recovering: false,
                closed: false,
            }),
            wake: Condvar::new(),
            capacity,
        });
        (
            Self {
                inner: Arc::clone(&inner),
            },
            AccessUnitReceiver { inner },
        )
    }

    pub(crate) fn try_push(&self, unit: EncodedAccessUnit) -> AccessUnitPushResult {
        let Ok(mut state) = self.inner.state.lock() else {
            return AccessUnitPushResult::Closed;
        };
        if state.closed {
            return AccessUnitPushResult::Closed;
        }
        if state.recovering {
            if !unit.keyframe {
                return AccessUnitPushResult::DroppedUntilKeyframe;
            }
            state.units.clear();
            state.recovering = false;
            state.units.push_back(unit);
            self.inner.wake.notify_one();
            return AccessUnitPushResult::Enqueued;
        }
        if state.units.len() == self.inner.capacity {
            state.units.clear();
            if !unit.keyframe {
                state.recovering = true;
                return AccessUnitPushResult::DroppedUntilKeyframe;
            }
        }
        state.units.push_back(unit);
        self.inner.wake.notify_one();
        AccessUnitPushResult::Enqueued
    }

    /// Explicit shutdown only. Never close on drop: the queue is
    /// reference-counted (`Clone` shares the inner state), so a dropped
    /// short-lived clone would poison every other owner. That is exactly
    /// what black-screened Windows sessions: `create_windows_encoder`
    /// cloned the session queue into the encoder and the dropped parameter
    /// closed the shared state, so every `try_push` returned `Closed` and
    /// no viewer ever received a frame (connected ICE, zero stats, working
    /// host preview). Threads exit via shutdown flags and this call.
    pub(crate) fn close(&self) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.closed = true;
            state.units.clear();
            self.inner.wake.notify_all();
        }
    }
}

/// NOTE: no `Drop` impl on purpose (see `close`).

#[allow(dead_code)]
impl AccessUnitReceiver {
    pub(crate) fn is_closed(&self) -> bool {
        self.inner
            .state
            .lock()
            .map(|state| state.closed)
            .unwrap_or(true)
    }

    pub(crate) fn recv_timeout(&self, timeout: Duration) -> Option<EncodedAccessUnit> {
        let mut state = self.inner.state.lock().ok()?;
        loop {
            if let Some(unit) = state.units.pop_front() {
                return Some(unit);
            }
            if state.closed {
                return None;
            }
            let (next, result) = self.inner.wake.wait_timeout(state, timeout).ok()?;
            state = next;
            if result.timed_out() {
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        annexb_contains_b_slice, annexb_nal_types, annexb_starts_with_sps_pps_idr, bgra_to_nv12,
        is_baseline_profile, is_high_profile, pps_is_cavlc_only, AvccAnnexBConverter, AvccError,
        HevcAnnexBConverter, ACCESS_UNIT_QUEUE_CAPACITY, H264_PROFILE_LEVEL_ID,
    };
    use std::time::Duration;

    fn avcc(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|nal| {
                (nal.len() as u32)
                    .to_be_bytes()
                    .into_iter()
                    .chain(nal.iter().copied())
            })
            .collect()
    }

    #[test]
    fn converts_length_prefixed_nals_to_annex_b() {
        let mut converter = AvccAnnexBConverter::default();
        let unit = converter
            .convert(&avcc(&[&[0x41, 1, 2]]), 90_000, false)
            .expect("valid AVCC");
        assert_eq!(unit.data, vec![0, 0, 0, 1, 0x41, 1, 2]);
        assert_eq!(unit.timestamp_90khz, 90_000);
        assert!(!unit.keyframe);
        assert_eq!(unit.profile_level_id, None);
    }

    #[test]
    fn caches_and_injects_sps_pps_before_a_later_idr() {
        let mut converter = AvccAnnexBConverter::default();
        converter
            .convert(&avcc(&[&[0x67, 0x42, 0xe0, 0x2a, 1], &[0x68, 2]]), 0, false)
            .expect("parameter sets");
        let unit = converter
            .convert(&avcc(&[&[0x65, 3]]), 3_000, true)
            .expect("IDR");
        assert_eq!(
            unit.data,
            vec![0, 0, 0, 1, 0x67, 0x42, 0xe0, 0x2a, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3,]
        );
        assert!(unit.keyframe);
        assert_eq!(
            unit.profile_level_id.as_deref(),
            Some(H264_PROFILE_LEVEL_ID)
        );
    }

    #[test]
    fn injects_the_missing_parameter_set_before_an_idr() {
        let mut converter = AvccAnnexBConverter::default();
        converter
            .convert(&avcc(&[&[0x67, 0x42, 0xe0, 0x2a, 1], &[0x68, 2]]), 0, false)
            .expect("parameter sets");
        let unit = converter
            .convert(
                &avcc(&[&[0x67, 0x42, 0xe0, 0x2a, 9], &[0x65, 3]]),
                3_000,
                true,
            )
            .expect("IDR");
        assert_eq!(
            unit.data,
            vec![0, 0, 0, 1, 0x67, 0x42, 0xe0, 0x2a, 9, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3,]
        );
    }

    #[test]
    fn rejects_truncated_avcc() {
        let mut converter = AvccAnnexBConverter::default();
        assert_eq!(
            converter.convert(&[0, 0, 0], 0, false),
            Err(AvccError::TruncatedLength)
        );
        assert_eq!(
            converter.convert(&[0, 0, 0, 4, 1], 0, false),
            Err(AvccError::TruncatedNal)
        );
    }

    #[test]
    fn refuses_an_idr_without_both_parameter_sets() {
        let mut converter = AvccAnnexBConverter::default();
        assert_eq!(
            converter.convert(&avcc(&[&[0x65, 3]]), 0, true),
            Err(AvccError::MissingParameterSets)
        );
    }

    #[test]
    fn accepts_constrained_baseline_profiles_from_videotoolbox() {
        let mut converter = AvccAnnexBConverter::default();
        converter
            .convert(&avcc(&[&[0x67, 0x42, 0xc0, 0x2a], &[0x68, 2]]), 0, false)
            .expect("parameter sets");
        let unit = converter
            .convert(&avcc(&[&[0x65, 3]]), 3_000, true)
            .expect("constrained baseline IDR");
        assert_eq!(unit.profile_level_id.as_deref(), Some("42c02a"));
    }

    #[test]
    fn high_profile_converter_accepts_high_sps() {
        let mut converter = AvccAnnexBConverter::high_profile();
        converter
            .convert(&avcc(&[&[0x67, 0x64, 0x00, 0x2a], &[0x68, 2]]), 0, false)
            .expect("parameter sets");
        let unit = converter
            .convert(&avcc(&[&[0x65, 3]]), 3_000, true)
            .expect("high IDR");
        assert_eq!(unit.profile_level_id.as_deref(), Some("64002a"));
    }

    #[test]
    fn baseline_converter_still_rejects_high_sps() {
        let mut converter = AvccAnnexBConverter::default();
        converter
            .convert(&avcc(&[&[0x67, 0x64, 0x00, 0x2a], &[0x68, 2]]), 0, false)
            .expect("parameter sets");
        assert_eq!(
            converter.convert(&avcc(&[&[0x65, 3]]), 3_000, true),
            Err(AvccError::IncompatibleProfileLevel)
        );
    }

    #[test]
    fn bgra_to_nv12_converts_solid_colors() {
        // 2x2 solid red (BGRA 0,0,255) in BT.709 limited: Y≈63, V high, U low.
        let red = vec![0_u8, 0, 255, 255].repeat(4);
        let nv12 = bgra_to_nv12(&red, 2, 2).expect("2x2 converts");
        assert_eq!(nv12.len(), 6);
        assert!(
            nv12[0] >= 60 && nv12[0] <= 68,
            "red luma {} (want BT.709 ~63, not BT.601 ~81)",
            nv12[0]
        );
        assert_eq!(&nv12[0..4], &[nv12[0]; 4]);
        assert!(nv12[5] > 220, "red Cr {}", nv12[5]);
        assert!(nv12[4] < 120, "red Cb {}", nv12[4]);
        // 2x2 solid black: Y=16, UV=128.
        let black = vec![0_u8, 0, 0, 255].repeat(4);
        let nv12 = bgra_to_nv12(&black, 2, 2).expect("black converts");
        assert_eq!(&nv12[..], &[16, 16, 16, 16, 128, 128]);
        // Odd sizes and short buffers are rejected.
        assert_eq!(bgra_to_nv12(&red, 3, 2), None);
        assert_eq!(bgra_to_nv12(&[0_u8; 8], 2, 2), None);
    }

    #[test]
    fn rejects_an_sps_profile_that_does_not_match_the_negotiated_codec() {
        let mut converter = AvccAnnexBConverter::default();
        converter
            .convert(&avcc(&[&[0x67, 0x64, 0x00, 0x1f], &[0x68, 2]]), 0, false)
            .expect("parameter sets");
        assert_eq!(
            converter.convert(&avcc(&[&[0x65, 3]]), 3_000, true),
            Err(AvccError::IncompatibleProfileLevel)
        );
    }

    #[test]
    fn queue_drops_a_partial_gop_and_recovers_at_idr() {
        let (queue, receiver) = super::AccessUnitQueue::bounded(2);
        let p = |value| super::EncodedAccessUnit {
            data: vec![value],
            timestamp_90khz: value as u64,
            keyframe: false,
            profile_level_id: None,
        };
        assert_eq!(queue.try_push(p(1)), super::AccessUnitPushResult::Enqueued);
        assert_eq!(queue.try_push(p(2)), super::AccessUnitPushResult::Enqueued);
        assert_eq!(
            queue.try_push(p(3)),
            super::AccessUnitPushResult::DroppedUntilKeyframe
        );
        assert_eq!(
            queue.try_push(p(4)),
            super::AccessUnitPushResult::DroppedUntilKeyframe
        );
        assert_eq!(
            queue.try_push(super::EncodedAccessUnit {
                data: vec![5],
                timestamp_90khz: 5,
                keyframe: true,
                profile_level_id: Some(super::H264_PROFILE_LEVEL_ID.into()),
            }),
            super::AccessUnitPushResult::Enqueued
        );
        assert_eq!(
            receiver
                .recv_timeout(Duration::from_millis(1))
                .unwrap()
                .data,
            vec![5]
        );
    }

    #[test]
    fn sps_profile_predicates_cover_baseline_high_and_unknown() {
        // 42* = Baseline/CBP (session contract); 64* = High (rejected in a
        // Baseline session); anything else or malformed is unknown.
        for id in ["42e02a", "42c02a", "42e01f", "42C02A", "42001f"] {
            assert!(is_baseline_profile(id), "{id} must read as Baseline");
            assert!(!is_high_profile(id), "{id} must not read as High");
        }
        for id in ["64002a", "640c2a", "640033", "64C02A"] {
            assert!(is_high_profile(id), "{id} must read as High");
            assert!(!is_baseline_profile(id), "{id} must not read as Baseline");
        }
        for id in ["4d002a", "4d4028", "", "42e02", "42e02a00", "64"] {
            assert!(!is_baseline_profile(id), "{id:?} must not read as Baseline");
            assert!(!is_high_profile(id), "{id:?} must not read as High");
        }
        // `None` (no SPS parsed yet) is not a predicate hit: the sender gate
        // treats it as the pre-SPS window (see peer_transport
        // `sample_profile_accepted`), not as a Baseline SPS.
    }

    #[test]
    fn converter_output_opens_with_sps_pps_idr() {
        let mut converter = AvccAnnexBConverter::default();
        converter
            .convert(&avcc(&[&[0x67, 0x42, 0xe0, 0x2a, 1], &[0x68, 2]]), 0, false)
            .expect("parameter sets");
        let unit = converter
            .convert(&avcc(&[&[0x65, 3]]), 3_000, true)
            .expect("IDR");
        assert_eq!(annexb_nal_types(&unit.data), vec![7, 8, 5]);
        assert!(annexb_starts_with_sps_pps_idr(&unit.data));
        // A bare P-frame carries no parameter sets and must not claim the
        // first-segment contract.
        let mut plain = AvccAnnexBConverter::default();
        let p = plain
            .convert(&avcc(&[&[0x41, 0x9a, 0x22]]), 0, false)
            .expect("P slice");
        assert_eq!(annexb_nal_types(&p.data), vec![1]);
        assert!(!annexb_starts_with_sps_pps_idr(&p.data));
    }

    #[test]
    fn baseline_stream_has_no_b_slices_and_pps_is_cavlc_only() {
        // Synthetic slice payloads (after the NAL header byte):
        // P: first_mb=0 ue(0)="1", slice_type=0 ue(0)="1" -> 0xC0.
        // B: first_mb=0 "1", slice_type=1 ue(1)="010" -> 0xA0.
        // I: first_mb=0 "1", slice_type=2 ue(2)="011" -> 0xB0.
        let p_nal = [0x41u8, 0xC0];
        let b_nal = [0x41u8, 0xA0];
        let i_nal = [0x65u8, 0xB0];
        let annexb = |nals: &[&[u8]]| {
            nals.iter()
                .flat_map(|nal| [0, 0, 0, 1].into_iter().chain(nal.iter().copied()))
                .collect::<Vec<u8>>()
        };
        assert!(!annexb_contains_b_slice(&annexb(&[&p_nal])));
        assert!(!annexb_contains_b_slice(&annexb(&[&i_nal])));
        assert!(annexb_contains_b_slice(&annexb(&[&b_nal])));
        assert!(annexb_contains_b_slice(&annexb(&[&p_nal, &b_nal])));
        // PPS: pic_id=0 "1", seq_id=0 "1", entropy_coding_mode_flag.
        // 0xC0 = flag 0 (CAVLC, Baseline contract); 0xE0 = flag 1 (CABAC).
        assert!(pps_is_cavlc_only(&[0x68, 0xC0]));
        assert!(!pps_is_cavlc_only(&[0x68, 0xE0]));
        assert!(!pps_is_cavlc_only(&[]));
    }

    #[test]
    fn bounded_16_queue_recovers_at_idr_without_touching_other_links() {
        assert_eq!(ACCESS_UNIT_QUEUE_CAPACITY, 16);
        // Two per-viewer links: overflowing link A must not disturb link B.
        let (link_a, rx_a) = super::AccessUnitQueue::bounded(ACCESS_UNIT_QUEUE_CAPACITY);
        let (link_b, rx_b) = super::AccessUnitQueue::bounded(ACCESS_UNIT_QUEUE_CAPACITY);
        let unit = |seq: u8, keyframe: bool| super::EncodedAccessUnit {
            data: vec![seq],
            timestamp_90khz: u64::from(seq) * 1_500,
            keyframe,
            profile_level_id: Some(super::H264_PROFILE_LEVEL_ID.into()),
        };
        // Link A holds a full GOP (IDR + 15 P-frames = capacity); link B
        // holds a partial GOP so its head is observable after A's overflow.
        for seq in 0..16u8 {
            let keyframe = seq == 0;
            assert_eq!(
                link_a.try_push(unit(seq, keyframe)),
                super::AccessUnitPushResult::Enqueued
            );
        }
        assert_eq!(
            link_b.try_push(unit(0, true)),
            super::AccessUnitPushResult::Enqueued
        );
        assert_eq!(
            link_b.try_push(unit(1, false)),
            super::AccessUnitPushResult::Enqueued
        );
        // Overflow link A with P-frames: whole GOP dropped, recovering.
        for seq in 16..20u8 {
            assert_eq!(
                link_a.try_push(unit(seq, false)),
                super::AccessUnitPushResult::DroppedUntilKeyframe,
                "overflow P-frame {seq} must drop until IDR"
            );
        }
        // Link B still enqueues while A is recovering (per-link recovery).
        assert_eq!(
            link_b.try_push(unit(2, false)),
            super::AccessUnitPushResult::Enqueued
        );
        // An IDR within N=4 frames heals link A; only the IDR survives.
        assert_eq!(
            link_a.try_push(unit(20, true)),
            super::AccessUnitPushResult::Enqueued
        );
        assert_eq!(
            rx_a.recv_timeout(Duration::from_millis(10)).unwrap().data,
            vec![20]
        );
        // Link B drains its original GOP head first: unaffected by A.
        assert_eq!(
            rx_b.recv_timeout(Duration::from_millis(10)).unwrap().data,
            vec![0]
        );
        assert_eq!(
            rx_b.recv_timeout(Duration::from_millis(10)).unwrap().data,
            vec![1]
        );
    }

    // Two-byte HEVC headers: (type << 1) | layer bits, temporal_id_plus1 = 1.
    const VPS: &[u8] = &[0x40, 0x01, 0x0c];
    const SPS: &[u8] = &[0x42, 0x01, 0x0d];
    const PPS: &[u8] = &[0x44, 0x01, 0x0e];
    const IDR: &[u8] = &[0x26, 0x01, 0x11];
    const CRA: &[u8] = &[0x2a, 0x01, 0x12];

    #[test]
    fn hevc_injects_vps_sps_pps_before_irap() {
        let mut converter = HevcAnnexBConverter::default();
        converter
            .convert(&avcc(&[VPS, SPS, PPS]), 0, false)
            .expect("sets");
        let unit = converter.convert(&avcc(&[IDR]), 3_000, true).expect("irap");
        assert!(unit.keyframe);
        assert_eq!(
            unit.data,
            vec![
                0, 0, 0, 1, 0x40, 0x01, 0x0c, 0, 0, 0, 1, 0x42, 0x01, 0x0d, 0, 0, 0, 1, 0x44, 0x01,
                0x0e, 0, 0, 0, 1, 0x26, 0x01, 0x11,
            ]
        );
    }

    #[test]
    fn hevc_refuses_irap_without_parameter_sets() {
        let mut converter = HevcAnnexBConverter::default();
        assert_eq!(
            converter.convert(&avcc(&[IDR]), 0, true),
            Err(AvccError::MissingParameterSets)
        );
    }

    #[test]
    fn hevc_detects_cra_as_keyframe() {
        let mut converter = HevcAnnexBConverter::default();
        converter
            .convert(&avcc(&[VPS, SPS, PPS]), 0, false)
            .expect("sets");
        let unit = converter.convert(&avcc(&[CRA]), 3_000, false).expect("cra");
        assert!(unit.keyframe);
    }
}
