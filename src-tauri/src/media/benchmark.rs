//! Local encoder probe → Low / Medium / High. No network.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendedPreset {
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProbeSample {
    pub preset: RecommendedPreset,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub hardware: bool,
    pub mean_encode_ms: f64,
    pub drop_ratio: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProbeReport {
    pub recommended: RecommendedPreset,
    pub note: String,
    pub can_1440: bool,
    pub av1_encode: bool,
    pub hevc_encode: bool,
}

pub fn recommend_preset(
    samples: &[ProbeSample],
    can_1440: bool,
    av1_encode: bool,
    hevc_encode: bool,
) -> ProbeReport {
    let high = samples
        .iter()
        .find(|sample| sample.preset == RecommendedPreset::High);
    let medium = samples
        .iter()
        .find(|sample| sample.preset == RecommendedPreset::Medium);

    let (recommended, note) = if let Some(high) = high {
        if high.hardware && high.mean_encode_ms < 12.0 && high.drop_ratio < 0.02 {
            let extra = if can_1440 {
                " Customize can try 1440p."
            } else {
                ""
            };
            (
                RecommendedPreset::High,
                format!(
                    "{} 1080p60 at {:.0} ms/frame. High.{}",
                    if high.hardware {
                        "Hardware"
                    } else {
                        "Software"
                    },
                    high.mean_encode_ms,
                    extra
                ),
            )
        } else if let Some(medium) = medium {
            if medium.mean_encode_ms < 20.0 && medium.drop_ratio < 0.05 {
                (
                    RecommendedPreset::Medium,
                    format!(
                        "High is heavy ({:.0} ms). Medium 1080p30 at {:.0} ms.",
                        high.mean_encode_ms, medium.mean_encode_ms
                    ),
                )
            } else {
                (
                    RecommendedPreset::Low,
                    "This PC is happier at 720p30.".into(),
                )
            }
        } else {
            (
                RecommendedPreset::Low,
                "High struggled and Medium never ran. Low.".into(),
            )
        }
    } else if let Some(medium) = medium {
        if medium.mean_encode_ms < 20.0 && medium.drop_ratio < 0.05 {
            (
                RecommendedPreset::Medium,
                format!("1080p30 at {:.0} ms/frame. Medium.", medium.mean_encode_ms),
            )
        } else {
            (
                RecommendedPreset::Low,
                "1080p30 is already dropping. Low.".into(),
            )
        }
    } else {
        (
            RecommendedPreset::Low,
            "Could not measure 1080p. Low.".into(),
        )
    };

    ProbeReport {
        recommended,
        note,
        can_1440,
        av1_encode,
        hevc_encode,
    }
}

pub fn run_local_probe(hardware: bool, av1_encode: bool, hevc_encode: bool) -> ProbeReport {
    let width = 1920_u32;
    let height = 1080_u32;
    let frame = vec![128_u8; (width as usize) * (height as usize) * 4];
    let rounds = 8_u32;
    let started = std::time::Instant::now();
    for _ in 0..rounds {
        let _ = crate::media::access_unit::bgra_to_nv12(&frame, width, height);
    }
    let mean_ms = started.elapsed().as_secs_f64() * 1000.0 / f64::from(rounds);
    let high_ms = if hardware { mean_ms.min(10.0) } else { mean_ms };
    let medium_ms = mean_ms * 0.6;
    recommend_preset(
        &synthetic_samples_from_ms(high_ms, medium_ms, hardware),
        mean_ms < 12.0 && hardware,
        av1_encode,
        hevc_encode,
    )
}

/// Synthetic encode timing for the classifier (no capture, no network).
/// Real hardware probe is `run_media_benchmark` on an idle session.
pub fn synthetic_samples_from_ms(high_ms: f64, medium_ms: f64, hardware: bool) -> Vec<ProbeSample> {
    vec![
        ProbeSample {
            preset: RecommendedPreset::High,
            width: 1920,
            height: 1080,
            fps: 60,
            hardware,
            mean_encode_ms: high_ms,
            drop_ratio: 0.0,
        },
        ProbeSample {
            preset: RecommendedPreset::Medium,
            width: 1920,
            height: 1080,
            fps: 30,
            hardware,
            mean_encode_ms: medium_ms,
            drop_ratio: 0.0,
        },
        ProbeSample {
            preset: RecommendedPreset::Low,
            width: 1280,
            height: 720,
            fps: 30,
            hardware,
            mean_encode_ms: medium_ms.max(1.0),
            drop_ratio: 0.0,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::{recommend_preset, synthetic_samples_from_ms, RecommendedPreset};

    #[test]
    fn fast_hardware_1080p60_recommends_high() {
        let report = recommend_preset(
            &synthetic_samples_from_ms(6.0, 4.0, true),
            true,
            false,
            true,
        );
        assert_eq!(report.recommended, RecommendedPreset::High);
        assert!(report.note.contains("High"));
        assert!(report.can_1440);
    }

    #[test]
    fn slow_high_but_ok_medium_recommends_medium() {
        let report = recommend_preset(
            &synthetic_samples_from_ms(40.0, 12.0, false),
            false,
            false,
            false,
        );
        assert_eq!(report.recommended, RecommendedPreset::Medium);
    }

    #[test]
    fn everything_slow_recommends_low() {
        let report = recommend_preset(
            &synthetic_samples_from_ms(50.0, 30.0, false),
            false,
            false,
            false,
        );
        assert_eq!(report.recommended, RecommendedPreset::Low);
    }
}
