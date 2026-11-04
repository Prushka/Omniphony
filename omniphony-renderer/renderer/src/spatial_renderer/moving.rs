//! Object output: up to [`MAX_MOVING_SLOTS`] objects leave the renderer
//! unpanned, each in a slot after the speakers, with its position at the first
//! and the last frame of every block, for a host that places them itself
//! (Windows' spatial sound takes them as moving objects).
//!
//! A slot belongs to an object (its PCM channel) from the first block it is
//! heard in until the renderer is reset; an object beyond the slots is panned
//! onto the speakers as before. The renderer's test object takes a free slot
//! while it is on. Everything here is allocated before the first block: the
//! render path allocates nothing for it.

use super::components::{self, ChannelState};

/// The most a home Atmos stream carries.
pub const MAX_MOVING_SLOTS: usize = 16;
/// The slot ID of the renderer's test object; a stream object's is its PCM channel.
pub const TEST_OBJECT_ID: u32 = u32::MAX;
/// Input channels the renderer can be told are objects.
const MAX_INPUT_CHANNELS: usize = 64;

/// One slot of a block: which object holds it, and where it was.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MovingSlot {
    pub active: bool,
    pub id: u32,
    /// ADM cartesian `[x, y, z]` at the block's first frame…
    pub start: [f32; 3],
    /// …and at its last, ramps applied.
    pub end: [f32; 3],
}

/// The slots of one rendered block: `width` of them, the layout's tail.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MovingBlock {
    pub width: usize,
    pub slots: [MovingSlot; MAX_MOVING_SLOTS],
}

impl MovingBlock {
    pub fn slots(&self) -> &[MovingSlot] {
        &self.slots[..self.width]
    }
}

pub(super) struct MovingStage {
    /// Slots asked for; 0 renders as before.
    asked: usize,
    /// Which input channels are the stream's objects, as the host said for this block.
    object: [bool; MAX_INPUT_CHANNELS],
    /// The stream has had objects since the slots were set: it keeps them in
    /// its layout through a reset, whose first blocks can come before its
    /// objects do, so that the layout does not change under the host.
    seen_objects: bool,
    /// The object (PCM channel, or the test object) each slot belongs to.
    owner: [Option<u32>; MAX_MOVING_SLOTS],
    /// Each slot's gain as it slews towards its object's.
    gain: [f32; MAX_MOVING_SLOTS],
    /// The last block's slots.
    block: MovingBlock,
    /// The input with the slotted channels silenced, for the speaker mix.
    scratch: Vec<f32>,
    /// Each slot's samples for the block, interleaved `asked` wide.
    slot_pcm: Vec<f32>,
    /// The widened output, swapped with the speaker buffer every block.
    wide: Vec<f32>,
}

impl MovingStage {
    pub(super) fn new() -> Self {
        Self {
            asked: 0,
            object: [false; MAX_INPUT_CHANNELS],
            seen_objects: false,
            owner: [None; MAX_MOVING_SLOTS],
            gain: [0.0; MAX_MOVING_SLOTS],
            block: MovingBlock::default(),
            scratch: Vec::new(),
            slot_pcm: Vec::new(),
            wide: Vec::new(),
        }
    }

    pub(super) fn set_slots(&mut self, slots: usize) {
        self.asked = slots.min(MAX_MOVING_SLOTS);
        self.seen_objects = false;
        self.reset();
    }

    pub(super) fn slots(&self) -> usize {
        self.asked
    }

    pub(super) fn set_objects(&mut self, objects: impl IntoIterator<Item = bool>) {
        self.object = [false; MAX_INPUT_CHANNELS];
        for (slot, is_object) in self.object.iter_mut().zip(objects) {
            *slot = is_object;
        }
        self.seen_objects |= self.object.iter().any(|&o| o);
    }

    pub(super) fn reset(&mut self) {
        self.owner = [None; MAX_MOVING_SLOTS];
        self.gain = [0.0; MAX_MOVING_SLOTS];
        self.block = MovingBlock::default();
    }

    /// Whether this block goes out with slots: some asked, a stream that has
    /// had objects, and a path whose speakers come out as soon as the slots do.
    pub(super) fn on(&self, latency_samples: usize) -> bool {
        self.asked > 0 && latency_samples == 0 && self.seen_objects
    }

    pub(super) fn block(&self) -> &MovingBlock {
        &self.block
    }

    pub(super) fn holds_test_object(&self) -> bool {
        self.owner[..self.asked].contains(&Some(TEST_OBJECT_ID))
    }

    /// Gives free slots to objects heard for the first time, and to the test
    /// object while it is on; frees the test object's when it is off.
    pub(super) fn assign(&mut self, states: &[ChannelState], channels: usize, test_object: bool) {
        let owners = &mut self.owner[..self.asked];
        if !test_object {
            for owner in owners.iter_mut() {
                if *owner == Some(TEST_OBJECT_ID) {
                    *owner = None;
                }
            }
        }
        let heard = (0..channels.min(MAX_INPUT_CHANNELS))
            .filter(|&c| self.object[c] && states.get(c).is_some_and(|s| s.initialized))
            .map(|c| c as u32)
            .chain(test_object.then_some(TEST_OBJECT_ID));
        for id in heard {
            if owners.contains(&Some(id)) {
                continue;
            }
            let Some(free) = owners.iter_mut().find(|o| o.is_none()) else {
                break;
            };
            *free = Some(id);
        }
    }

    /// A slot's object channel is in this block, and an object in it: a block
    /// of the same stream's plain PCM has no object there, whatever its width.
    fn holds(&self, channel: usize, channels: usize) -> bool {
        channel < channels && self.object.get(channel).copied().unwrap_or(false)
    }

    /// The input for the speaker mix: the slotted channels silenced, so that
    /// their ramps still advance there and nothing of them is panned.
    pub(super) fn without_slotted<'a>(&'a mut self, input: &[f32], channels: usize) -> &'a [f32] {
        self.scratch.clear();
        self.scratch.extend_from_slice(input);
        if channels == 0 {
            return &self.scratch;
        }
        for owner in self.owner[..self.asked].iter().flatten() {
            let c = *owner as usize;
            if !self.holds(c, channels) {
                continue;
            }
            for frame in self.scratch.chunks_exact_mut(channels) {
                frame[c] = 0.0;
            }
        }
        &self.scratch
    }

    /// Each slot's samples and positions for the block, after the speaker mix
    /// has advanced the ramps. `muted` says whether a stream object is muted;
    /// `test` is the test object's block and position; with `isolated`, the
    /// test object alone sounds.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn capture(
        &mut self,
        input: &[f32],
        channels: usize,
        frames: usize,
        states: &[ChannelState],
        muted: impl Fn(usize) -> bool,
        test: Option<(&[f32], [f32; 3])>,
        isolated: bool,
        total_gain: f32,
        slew_samples: f32,
    ) {
        let width = self.asked;
        self.slot_pcm.clear();
        self.slot_pcm.resize(frames * width, 0.0);
        let step = if slew_samples > 0.0 {
            1.0 / slew_samples
        } else {
            1.0
        };
        let previous = self.block;
        for slot in 0..width {
            // No object, or one not in this block (plain PCM of the same
            // stream): the slot is silent and holds nothing.
            let Some(id) = self.owner[slot]
                .filter(|&id| id == TEST_OBJECT_ID || self.holds(id as usize, channels))
            else {
                self.block.slots[slot] = MovingSlot::default();
                self.gain[slot] = 0.0;
                continue;
            };
            let (pcm, stride, offset, target, end): (&[f32], usize, usize, f32, [f32; 3]) = if id
                == TEST_OBJECT_ID
            {
                match test {
                    Some((pcm, position)) => (pcm, 1, 0, 1.0, position),
                    None => (&[], 1, 0, 0.0, previous.slots[slot].end),
                }
            } else {
                let c = id as usize;
                let state = states.get(c).filter(|s| s.initialized);
                let gain = match state {
                    Some(s) if !muted(c) && !isolated => components::gain_db_to_linear(s.gain_db),
                    _ => 0.0,
                };
                let end = state
                    .map(|s| s.ramp.output_position.map(|v| v as f32))
                    .unwrap_or(previous.slots[slot].end);
                (input, channels, c, gain, end)
            };
            let mut gain = self.gain[slot];
            for f in 0..frames {
                gain += (target - gain).clamp(-step, step);
                let sample = pcm.get(f * stride + offset).copied().unwrap_or(0.0);
                self.slot_pcm[f * width + slot] = sample * gain * total_gain;
            }
            self.gain[slot] = gain;
            let start = if previous.slots[slot].active && previous.slots[slot].id == id {
                previous.slots[slot].end
            } else {
                end
            };
            self.block.slots[slot] = MovingSlot {
                active: true,
                id,
                start,
                end,
            };
        }
        self.block.width = width;
    }

    /// The speakers followed by the slots, interleaved. The returned buffer is
    /// the one widened last block; `speakers` is kept for the next.
    pub(super) fn widen(
        &mut self,
        speakers: Vec<f32>,
        num_speakers: usize,
        frames: usize,
    ) -> Vec<f32> {
        let width = self.block.width;
        let channels = num_speakers + width;
        let mut wide = std::mem::replace(&mut self.wide, speakers);
        wide.clear();
        wide.resize(frames * channels, 0.0);
        for f in 0..frames {
            let out = &mut wide[f * channels..(f + 1) * channels];
            out[..num_speakers]
                .copy_from_slice(&self.wide[f * num_speakers..(f + 1) * num_speakers]);
            out[num_speakers..].copy_from_slice(&self.slot_pcm[f * width..(f + 1) * width]);
        }
        wide
    }

    /// No slots this block: the next block with slots starts afresh.
    pub(super) fn off(&mut self) {
        self.block.width = 0;
    }
}
