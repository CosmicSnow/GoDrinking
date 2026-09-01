//! Native access-unit transport boundary.
//!
//! This is intentionally only the boundary needed by a future
//! `webrtc-rs` `TrackLocalStaticSample`. It does not create a peer or send
//! network traffic.

use super::access_unit::EncodedAccessUnit;
use std::time::Duration;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrackSample {
    pub(crate) data: Vec<u8>,
    pub(crate) timestamp: Duration,
    pub(crate) duration: Duration,
    pub(crate) keyframe: bool,
}

pub(crate) trait TrackLocalStaticSampleSink: Send {
    fn write_sample(&mut self, sample: TrackSample) -> Result<(), String>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) enum TransportState {
    Ready,
    AwaitingKeyframe,
    Failed(String),
    Stopped,
}

pub(crate) struct TrackLocalStaticSampleTransport<S> {
    sink: S,
    frame_duration: Duration,
}

impl<S: TrackLocalStaticSampleSink> TrackLocalStaticSampleTransport<S> {
    #[allow(dead_code)]
    pub(crate) fn new(sink: S, frame_duration: Duration) -> Self {
        Self {
            sink,
            frame_duration,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn send(&mut self, unit: EncodedAccessUnit) -> Result<(), String> {
        self.sink.write_sample(TrackSample {
            data: unit.data,
            timestamp: Duration::from_nanos(
                unit.timestamp_90khz.saturating_mul(1_000_000_000) / 90_000,
            ),
            duration: self.frame_duration,
            keyframe: unit.keyframe,
        })
    }
}

/// Consumes normalized access units and enforces the transport-side GOP
/// contract. A real `webrtc-rs` TrackLocalStaticSample can implement
/// `TrackLocalStaticSampleSink`; this module deliberately does not construct a
/// peer or pretend to provide network transport.
#[allow(dead_code)]
pub(crate) struct AccessUnitConsumer<S> {
    transport: TrackLocalStaticSampleTransport<S>,
    state: TransportState,
}

#[allow(dead_code)]
impl<S: TrackLocalStaticSampleSink> AccessUnitConsumer<S> {
    pub(crate) fn new(sink: S, frame_duration: Duration) -> Self {
        Self {
            transport: TrackLocalStaticSampleTransport::new(sink, frame_duration),
            state: TransportState::Ready,
        }
    }

    pub(crate) fn state(&self) -> &TransportState {
        &self.state
    }

    pub(crate) fn consume(&mut self, unit: EncodedAccessUnit) -> bool {
        if matches!(
            self.state,
            TransportState::Stopped | TransportState::Failed(_)
        ) {
            return false;
        }
        if self.state == TransportState::AwaitingKeyframe && !unit.keyframe {
            return false;
        }
        match self.transport.send(unit) {
            Ok(()) => {
                self.state = TransportState::Ready;
                true
            }
            Err(error) => {
                self.state = TransportState::Failed(error);
                false
            }
        }
    }

    pub(crate) fn recover_at_keyframe(&mut self) {
        if matches!(self.state, TransportState::AwaitingKeyframe) {
            self.state = TransportState::Ready;
        }
    }

    pub(crate) fn mark_overflow(&mut self) {
        if self.state == TransportState::Ready {
            self.state = TransportState::AwaitingKeyframe;
        }
    }

    pub(crate) fn stop(&mut self) {
        self.state = TransportState::Stopped;
    }
}

#[cfg(test)]
mod tests {
    use super::{AccessUnitConsumer, TrackLocalStaticSampleSink, TrackSample, TransportState};
    use crate::media::access_unit::EncodedAccessUnit;
    use std::time::Duration;

    struct Sink {
        samples: Vec<TrackSample>,
        fail: bool,
    }

    impl TrackLocalStaticSampleSink for Sink {
        fn write_sample(&mut self, sample: TrackSample) -> Result<(), String> {
            if self.fail {
                Err("sink stopped".into())
            } else {
                self.samples.push(sample);
                Ok(())
            }
        }
    }

    fn unit(keyframe: bool) -> EncodedAccessUnit {
        EncodedAccessUnit {
            data: vec![1],
            timestamp_90khz: 90_000,
            keyframe,
            profile_level_id: Some("42e02a".into()),
        }
    }

    #[test]
    fn consumer_stops_after_sink_failure() {
        let mut consumer = AccessUnitConsumer::new(
            Sink {
                samples: Vec::new(),
                fail: true,
            },
            Duration::from_millis(33),
        );
        assert!(!consumer.consume(unit(true)));
        assert_eq!(
            consumer.state(),
            &TransportState::Failed("sink stopped".into())
        );
        assert!(!consumer.consume(unit(true)));
    }

    #[test]
    fn consumer_requires_a_keyframe_after_overflow() {
        let mut consumer = AccessUnitConsumer::new(
            Sink {
                samples: Vec::new(),
                fail: false,
            },
            Duration::from_millis(33),
        );
        consumer.mark_overflow();
        assert!(!consumer.consume(unit(false)));
        assert!(consumer.consume(unit(true)));
        assert_eq!(consumer.state(), &TransportState::Ready);
    }
}
