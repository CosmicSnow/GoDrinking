//! H.264 access-unit normalization for native transport.

use std::collections::VecDeque;
use std::fmt::{Display, Formatter};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const ANNEX_B_START_CODE: &[u8] = &[0, 0, 0, 1];
#[allow(dead_code)]
pub(crate) const H264_PROFILE_LEVEL_ID: &str = "42e02a";

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

    pub(crate) fn close(&self) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.closed = true;
            state.units.clear();
            self.inner.wake.notify_all();
        }
    }
}

impl Drop for AccessUnitQueue {
    fn drop(&mut self) {
        self.close();
    }
}

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
    use super::{AvccAnnexBConverter, AvccError, HevcAnnexBConverter, H264_PROFILE_LEVEL_ID};
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
        let unit = converter.convert(&avcc(&[&[0x65, 3]]), 3_000, true).expect("high IDR");
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

    // Two-byte HEVC headers: (type << 1) | layer bits, temporal_id_plus1 = 1.
    const VPS: &[u8] = &[0x40, 0x01, 0x0c];
    const SPS: &[u8] = &[0x42, 0x01, 0x0d];
    const PPS: &[u8] = &[0x44, 0x01, 0x0e];
    const IDR: &[u8] = &[0x26, 0x01, 0x11];
    const CRA: &[u8] = &[0x2a, 0x01, 0x12];

    #[test]
    fn hevc_injects_vps_sps_pps_before_irap() {
        let mut converter = HevcAnnexBConverter::default();
        converter.convert(&avcc(&[VPS, SPS, PPS]), 0, false).expect("sets");
        let unit = converter.convert(&avcc(&[IDR]), 3_000, true).expect("irap");
        assert!(unit.keyframe);
        assert_eq!(
            unit.data,
            vec![
                0, 0, 0, 1, 0x40, 0x01, 0x0c,
                0, 0, 0, 1, 0x42, 0x01, 0x0d,
                0, 0, 0, 1, 0x44, 0x01, 0x0e,
                0, 0, 0, 1, 0x26, 0x01, 0x11,
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
        converter.convert(&avcc(&[VPS, SPS, PPS]), 0, false).expect("sets");
        let unit = converter.convert(&avcc(&[CRA]), 3_000, false).expect("cra");
        assert!(unit.keyframe);
    }
}
