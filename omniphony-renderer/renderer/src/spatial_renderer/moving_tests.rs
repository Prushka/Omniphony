//! Object output: up to 16 objects leave the renderer unpanned, in slots after
//! the speakers, each with its position at the edges of every block.

use super::*;
use crate::live_params::{ObjectTest, TestIsolation};
use crate::speaker_layout::SpeakerLayout;
use crate::test_support;

const SPEAKERS: usize = 12;
const BLOCK: usize = 480;

fn renderer() -> SpatialRenderer {
    let layout = SpeakerLayout::preset("7.1.4").unwrap();
    SpatialRenderer::new(test_support::spec(layout)).unwrap()
}

/// The LFE as a direct channel, then `objects` object channels.
fn object_stream(r: &mut SpatialRenderer, objects: usize) {
    let mut routes = vec![ChannelRoute::Direct(bridge_api::RChannelLabel::LFE)];
    routes.extend((0..objects).map(|_| ChannelRoute::Virtual));
    r.configure_channel_routing(&routes);
}

fn object_at(
    channel_idx: usize,
    position: [f64; 3],
    gain_db: f32,
    ramp: u32,
) -> SpatialChannelEvent {
    SpatialChannelEvent {
        channel_idx,
        is_bed: false,
        gain_db: Some(gain_db),
        ramp_length: Some(ramp),
        size: Some([0.0, 0.0, 0.0]),
        position: Some(position),
        sample_pos: Some(0),
    }
}

fn lfe() -> SpatialChannelEvent {
    SpatialChannelEvent {
        channel_idx: 0,
        is_bed: true,
        gain_db: Some(0.0),
        ramp_length: Some(0),
        size: None,
        position: None,
        sample_pos: Some(0),
    }
}

/// A block whose channel `c` is the constant `levels[c]`.
fn constant_block(levels: &[f32]) -> Vec<f32> {
    (0..BLOCK).flat_map(|_| levels.iter().copied()).collect()
}

fn channel(out: &RenderedFrame, c: usize) -> Vec<f32> {
    out.samples
        .chunks(out.n_channels)
        .map(|frame| frame[c])
        .collect()
}

fn speaker_energy(out: &RenderedFrame) -> f32 {
    out.samples
        .chunks(out.n_channels)
        .flat_map(|frame| frame[..SPEAKERS].iter())
        .map(|s| s.abs())
        .sum()
}

fn near(a: [f32; 3], b: [f32; 3], within: f32) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() <= within)
}

/// Blocks enough for the 20 ms gain slew to settle.
fn settled(
    r: &mut SpatialRenderer,
    pcm: &[f32],
    channels: usize,
    events: &[SpatialChannelEvent],
) -> RenderedFrame {
    let mut out = r
        .render_frame(pcm, channels, events, Vec::new(), false)
        .unwrap();
    for _ in 0..6 {
        out = r
            .render_frame(pcm, channels, &[], out.samples, false)
            .unwrap();
    }
    out
}

#[test]
fn slots_carry_objects_unpanned() {
    let mut r = renderer();
    object_stream(&mut r, 3);
    r.set_moving_slots(2);
    r.set_moving_channels([false, true, true, true]);
    let events = [
        lfe(),
        object_at(1, [-1.0, 1.0, 0.0], 0.0, 0),
        object_at(2, [0.0, 0.0, 1.0], 0.0, 0),
        object_at(3, [1.0, -1.0, 0.0], 0.0, 0),
    ];
    let out = settled(&mut r, &constant_block(&[0.0, 0.1, 0.2, 0.3]), 4, &events);
    assert_eq!(out.n_channels, SPEAKERS + 2);
    assert!(
        channel(&out, 12).iter().all(|s| (s - 0.1).abs() < 1e-5),
        "slot 0 is object one"
    );
    assert!(
        channel(&out, 13).iter().all(|s| (s - 0.2).abs() < 1e-5),
        "slot 1 is object two"
    );
    assert!(
        speaker_energy(&out) > 0.0,
        "object three is in the speakers"
    );
    let slots = r.moving_positions();
    assert_eq!(slots.len(), 2);
    assert!(slots[0].active && slots[0].id == 1 && near(slots[0].end, [-1.0, 1.0, 0.0], 1e-3));
    assert!(slots[1].active && slots[1].id == 2 && near(slots[1].end, [0.0, 0.0, 1.0], 1e-3));

    // With only the slotted objects sounding, the speakers are silent.
    let mut r = renderer();
    object_stream(&mut r, 3);
    r.set_moving_slots(2);
    r.set_moving_channels([false, true, true, true]);
    let out = settled(&mut r, &constant_block(&[0.0, 0.1, 0.2, 0.0]), 4, &events);
    assert!(
        speaker_energy(&out) < 1e-6,
        "slotted objects are not panned"
    );
}

#[test]
fn slot_positions_follow_a_ramp() {
    let mut r = renderer();
    object_stream(&mut r, 1);
    r.set_moving_slots(1);
    r.set_moving_channels([false, true]);
    let pcm = constant_block(&[0.0, 0.1]);
    settled(
        &mut r,
        &pcm,
        2,
        &[lfe(), object_at(1, [-1.0, 1.0, 0.0], 0.0, 0)],
    );
    let mut previous = r.moving_positions()[0].end;
    let ramp = [object_at(1, [1.0, 1.0, 0.0], 0.0, 4800)];
    for block in 0..12 {
        let events: &[SpatialChannelEvent] = if block == 0 { &ramp } else { &[] };
        r.render_frame(&pcm, 2, events, Vec::new(), false).unwrap();
        let slot = r.moving_positions()[0];
        assert!(
            near(slot.start, previous, 1e-6),
            "block {block} starts where the last ended"
        );
        assert!(
            slot.end[0] >= previous[0] - 1e-6,
            "block {block} moves towards the right"
        );
        previous = slot.end;
    }
    assert!(
        near(previous, [1.0, 1.0, 0.0], 1e-3),
        "the ramp arrives: {previous:?}"
    );
}

#[test]
fn no_objects_keeps_the_layout_width() {
    let mut r = renderer();
    let routes: Vec<_> = (0..SPEAKERS).map(|_| ChannelRoute::Virtual).collect();
    r.configure_channel_routing(&routes);
    r.set_moving_slots(4);
    r.set_moving_channels([false; SPEAKERS]);
    let events: Vec<_> = (0..SPEAKERS)
        .map(|c| object_at(c, [0.0, 1.0, 0.0], 0.0, 0))
        .collect();
    let out = settled(&mut r, &constant_block(&[0.1; SPEAKERS]), SPEAKERS, &events);
    assert_eq!(out.n_channels, SPEAKERS);
    assert_eq!(r.moving_width(), 0);
    assert!(r.moving_positions().is_empty());
}

#[test]
fn slots_follow_objects_by_channel() {
    let mut r = renderer();
    object_stream(&mut r, 4);
    r.set_moving_slots(2);
    r.set_moving_channels([false, true, true, true, false]);
    let events = [
        lfe(),
        object_at(1, [-1.0, 1.0, 0.0], 0.0, 0),
        object_at(2, [1.0, 1.0, 0.0], 0.0, 0),
        object_at(3, [0.0, -1.0, 0.0], 0.0, 0),
    ];
    settled(
        &mut r,
        &constant_block(&[0.0, 0.1, 0.2, 0.3, 0.0]),
        5,
        &events,
    );
    // Object one goes silent: its slot stays its own.
    let silent = [object_at(1, [-1.0, 1.0, 0.0], GAIN_DB_NEG_INF_FOR_TESTS, 0)];
    settled(
        &mut r,
        &constant_block(&[0.0, 0.1, 0.2, 0.3, 0.0]),
        5,
        &silent,
    );
    assert_eq!(r.moving_positions()[0].id, 1);
    // A fourth object arrives while both slots are held: it goes into the speakers.
    r.set_moving_channels([false, true, true, true, true]);
    let out = settled(
        &mut r,
        &constant_block(&[0.0, 0.0, 0.0, 0.0, 0.4]),
        5,
        &[object_at(4, [0.0, 0.0, 1.0], 0.0, 0)],
    );
    let ids: Vec<u32> = r.moving_positions().iter().map(|s| s.id).collect();
    assert_eq!(ids, [1, 2]);
    assert!(speaker_energy(&out) > 0.0);
}

#[test]
fn the_test_object_takes_a_slot() {
    let mut r = renderer();
    object_stream(&mut r, 1);
    r.set_moving_slots(2);
    r.set_moving_channels([false, true]);
    r.renderer_control().live.write().object_test = Some(ObjectTest {
        position: [0.0, 1.0, 0.0],
        size: [0.0; 3],
        level: 0.5,
        isolation: TestIsolation::TestOnly,
        signal: Default::default(),
    });
    let out = settled(
        &mut r,
        &constant_block(&[0.0, 0.0]),
        2,
        &[lfe(), object_at(1, [-1.0, 1.0, 0.0], 0.0, 0)],
    );
    let slots = r.moving_positions();
    assert!(
        slots[1].active && slots[1].id == u32::MAX,
        "the test object has the free slot"
    );
    assert!(near(slots[1].end, [0.0, 1.0, 0.0], 1e-3));
    assert!(
        channel(&out, 13).iter().any(|s| s.abs() > 1e-4),
        "its signal is in its slot"
    );
    assert!(speaker_energy(&out) < 1e-6, "and not in the speakers");
}

#[test]
fn reset_keeps_the_slot_count() {
    let mut r = renderer();
    object_stream(&mut r, 1);
    r.set_moving_slots(20);
    assert_eq!(r.moving_slots(), 16, "at most 16");
    r.set_moving_slots(2);
    r.set_moving_channels([false, true]);
    let pcm = constant_block(&[0.0, 0.1]);
    settled(
        &mut r,
        &pcm,
        2,
        &[lfe(), object_at(1, [-1.0, 1.0, 0.0], 0.0, 0)],
    );
    assert!(r.moving_positions()[0].active);
    r.reset_runtime_state();
    r.render_frame(&pcm, 2, &[], Vec::new(), false).unwrap();
    assert_eq!(r.moving_slots(), 2);
    assert!(
        r.moving_positions().iter().all(|s| !s.active),
        "every slot is free"
    );
    settled(
        &mut r,
        &pcm,
        2,
        &[lfe(), object_at(1, [1.0, 1.0, 0.0], 0.0, 0)],
    );
    assert!(
        r.moving_positions()[0].active,
        "and objects take them again"
    );
}

const GAIN_DB_NEG_INF_FOR_TESTS: f32 = components::GAIN_DB_NEG_INF;
