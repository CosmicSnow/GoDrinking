//! Per-app selection and 20 ms stereo mix. No OS calls.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use golive_platform::app_excluded_by_token;

pub const FRAME_SAMPLES: usize = 960 * 2;
pub const MAX_INCLUDE: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppNode {
    pub node_id: u32,
    pub name: String,
    pub binary: String,
    pub node_name: String,
    pub pid: i32,
}

pub fn node_excluded(node: &AppNode, tokens: &[String]) -> bool {
    tokens.iter().any(|token| {
        app_excluded_by_token(&node.name, Some(&node.binary), token)
            || app_excluded_by_token(&node.node_name, Some(&node.binary), token)
            || app_excluded_by_token(&node.binary, None, token)
    })
}

/// Node ids to capture: everyone except the host process and excluded apps.
/// Stable order, capped at [`MAX_INCLUDE`].
pub fn select_node_ids(nodes: &[AppNode], tokens: &[String], self_pid: i32) -> Vec<u32> {
    let mut out = Vec::new();
    for node in nodes {
        if node.pid == self_pid || node.pid <= 0 {
            continue;
        }
        if node_excluded(node, tokens) {
            continue;
        }
        if out.contains(&node.node_id) {
            continue;
        }
        out.push(node.node_id);
        if out.len() == MAX_INCLUDE {
            break;
        }
    }
    out
}

pub fn select_pid(nodes: &[AppNode], pid: i32) -> Vec<u32> {
    if pid <= 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for node in nodes {
        if node.pid != pid {
            continue;
        }
        if out.contains(&node.node_id) {
            continue;
        }
        out.push(node.node_id);
        if out.len() == MAX_INCLUDE {
            break;
        }
    }
    out
}

/// Mix every queue that already holds a full frame. A quiet stream does not
/// stall the others.
pub fn mix_ready(streams: &mut [VecDeque<f32>]) -> Option<Vec<f32>> {
    let gating: Vec<usize> = streams
        .iter()
        .enumerate()
        .filter(|(_, queue)| queue.len() >= FRAME_SAMPLES)
        .map(|(index, _)| index)
        .collect();
    if gating.is_empty() {
        return None;
    }
    let mut frame = vec![0.0f32; FRAME_SAMPLES];
    for index in gating {
        for sample in &mut frame {
            *sample += streams[index].pop_front().unwrap_or(0.0);
        }
    }
    for sample in &mut frame {
        *sample = sample.clamp(-1.0, 1.0);
    }
    Some(frame)
}

pub fn trim_latency(pcm: &mut VecDeque<f32>) -> bool {
    let cap = FRAME_SAMPLES * 15;
    if pcm.len() <= cap {
        return false;
    }
    let keep = FRAME_SAMPLES * 5;
    let drop = pcm.len() - keep;
    pcm.drain(..drop);
    true
}

pub fn declick(prev: &mut [f32; 2], frame: &mut [f32], force: bool) {
    if frame.len() < 4 {
        return;
    }
    let jump = (frame[0] - prev[0]).abs().max((frame[1] - prev[1]).abs());
    if force || jump > 0.2 {
        let frames = (frame.len() / 2).min(48);
        for index in 0..frames {
            let gain = (index as f32 + 1.0) / frames as f32;
            let base = index * 2;
            frame[base] = prev[0] + (frame[base] - prev[0]) * gain;
            frame[base + 1] = prev[1] + (frame[base + 1] - prev[1]) * gain;
        }
    }
    let last = frame.len() - 2;
    prev[0] = frame[last];
    prev[1] = frame[last + 1];
}

pub fn pace_next(next_emit: Instant, now: Instant) -> Instant {
    let next = next_emit + Duration::from_millis(20);
    if next + Duration::from_millis(40) < now {
        now + Duration::from_millis(20)
    } else {
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u32, pid: i32, name: &str, binary: &str) -> AppNode {
        AppNode {
            node_id: id,
            name: name.into(),
            binary: binary.into(),
            node_name: name.into(),
            pid,
        }
    }

    #[test]
    fn discord_and_self_are_left_out_of_the_mix() {
        let nodes = vec![
            node(1, 10, "Discord", "Discord"),
            node(2, 20, "goDrinking", "goDrinking"),
            node(3, 30, "Chromium", "electron"),
            node(4, 40, "golive-video", "golive-video"),
        ];
        let tokens = vec![
            "Discord".into(),
            "goDrinking".into(),
            "golive-video".into(),
        ];
        assert_eq!(select_node_ids(&nodes, &tokens, 20), vec![3]);
    }

    #[test]
    fn include_pid_ignores_everyone_else() {
        let nodes = vec![node(1, 10, "Chromium", "electron"), node(2, 10, "Chromium", "electron"), node(3, 11, "Firefox", "firefox")];
        assert_eq!(select_pid(&nodes, 10), vec![1, 2]);
        assert!(select_pid(&nodes, 0).is_empty());
    }

    #[test]
    fn quiet_stream_does_not_block_a_ready_one() {
        let mut quiet = VecDeque::new();
        quiet.push_back(0.5);
        let ready = VecDeque::from(vec![0.25f32; FRAME_SAMPLES]);
        let mixed = mix_ready(&mut [quiet, ready]).unwrap();
        assert_eq!(mixed.len(), FRAME_SAMPLES);
        assert!((mixed[0] - 0.25).abs() < 0.001);
    }

    #[test]
    fn two_streams_sum_and_clamp() {
        let left = VecDeque::from(vec![0.8f32; FRAME_SAMPLES]);
        let right = VecDeque::from(vec![0.8f32; FRAME_SAMPLES]);
        let mixed = mix_ready(&mut [left, right]).unwrap();
        assert_eq!(mixed[0], 1.0);
    }

    #[test]
    fn declick_ramps_a_pop() {
        let mut prev = [0.0f32; 2];
        let mut popped = vec![0.8f32, -0.8, 0.8, -0.8, 0.8, -0.8, 0.8, -0.8];
        declick(&mut prev, &mut popped, false);
        assert!(popped[0].abs() < 0.25, "{}", popped[0]);
        assert!((popped[6] - 0.8).abs() < 0.05, "{}", popped[6]);
    }
}
