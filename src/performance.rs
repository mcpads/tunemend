//! The channel messages a composition produces once voices have MIDI channels.
//!
//! MIDI export and rendering both play this list, so a file and a preview of
//! the same revision send the same messages in the same order.

use std::collections::HashMap;

use crate::composition::{Composition, ControlKind};
use crate::export::VoiceAssignment;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelEvent {
    /// Position in the project's timebase unit.
    pub at: u64,
    pub channel: u8,
    pub message: ChannelMessage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelMessage {
    NoteOff { key: u8, velocity: u8 },
    NoteOn { key: u8, velocity: u8 },
    Controller { controller: u8, value: u8 },
    Program(u8),
    PitchBend(i16),
    ChannelPressure(u8),
}

impl ChannelMessage {
    /// At one position, notes end before channel state changes, and state
    /// changes before new notes start.
    fn order_class(&self) -> u8 {
        match self {
            Self::NoteOff { .. } => 0,
            Self::Controller { .. }
            | Self::Program(_)
            | Self::PitchBend(_)
            | Self::ChannelPressure(_) => 1,
            Self::NoteOn { .. } => 2,
        }
    }
}

/// Release velocity written when a note does not specify one, as the MIDI
/// specification recommends for receivers without release velocity.
pub const DEFAULT_RELEASE_VELOCITY: u8 = 64;

pub const CC_MODULATION: u8 = 1;
pub const CC_REVERB: u8 = 91;
pub const CC_CHORUS: u8 = 93;
pub const CC_VOLUME: u8 = 7;
pub const CC_PAN: u8 = 10;
pub const CC_EXPRESSION: u8 = 11;
pub const CC_DATA_ENTRY: u8 = 6;
pub const CC_DATA_ENTRY_LSB: u8 = 38;
pub const CC_RPN_LSB: u8 = 100;
pub const CC_RPN_MSB: u8 = 101;
pub const RPN_NULL: u8 = 127;

/// Expands notes and control lanes of assigned voices into ordered messages.
/// Voices without an assignment are skipped.
pub fn channel_events(
    composition: &Composition,
    assignments: &[VoiceAssignment],
) -> Vec<ChannelEvent> {
    let channels: HashMap<&str, u8> = assignments
        .iter()
        .map(|a| (a.voice.as_str(), a.channel))
        .collect();
    let mut events = Vec::new();
    for note in &composition.notes {
        let Some(&channel) = channels.get(note.voice.as_str()) else {
            continue;
        };
        events.push(ChannelEvent {
            at: note.start,
            channel,
            message: ChannelMessage::NoteOn {
                key: note.pitch,
                velocity: note.velocity,
            },
        });
        events.push(ChannelEvent {
            at: note.start + note.duration,
            channel,
            message: ChannelMessage::NoteOff {
                key: note.pitch,
                velocity: note.release_velocity.unwrap_or(DEFAULT_RELEASE_VELOCITY),
            },
        });
    }
    let mut lanes: Vec<_> = composition.control_lanes.iter().collect();
    lanes.sort_by_key(|lane| setup_order(lane.kind));
    for lane in lanes {
        let Some(&channel) = channels.get(lane.voice.as_str()) else {
            continue;
        };
        for point in &lane.points {
            let value = point.value;
            let messages: Vec<ChannelMessage> = match lane.kind {
                ControlKind::Program => vec![ChannelMessage::Program(value as u8)],
                ControlKind::Volume => vec![controller(CC_VOLUME, value)],
                ControlKind::Pan => vec![controller(CC_PAN, value)],
                ControlKind::Expression => vec![controller(CC_EXPRESSION, value)],
                ControlKind::Modulation => vec![controller(CC_MODULATION, value)],
                ControlKind::Reverb => vec![controller(CC_REVERB, value)],
                ControlKind::Chorus => vec![controller(CC_CHORUS, value)],
                ControlKind::PitchBend => vec![ChannelMessage::PitchBend(value as i16)],
                ControlKind::PitchBendRange => vec![
                    controller(CC_RPN_MSB, 0),
                    controller(CC_RPN_LSB, 0),
                    controller(CC_DATA_ENTRY, value),
                    controller(CC_DATA_ENTRY_LSB, 0),
                    controller(CC_RPN_MSB, i32::from(RPN_NULL)),
                    controller(CC_RPN_LSB, i32::from(RPN_NULL)),
                ],
            };
            events.extend(messages.into_iter().map(|message| ChannelEvent {
                at: point.at,
                channel,
                message,
            }));
        }
    }
    // The sort is stable, so the RPN sequence and the lane order stay intact.
    events.sort_by_key(|e| (e.at, e.message.order_class(), e.channel));
    events
}

/// At one position a sound is selected and its bend range set before it is
/// shaped and bent.
fn setup_order(kind: ControlKind) -> u8 {
    match kind {
        ControlKind::Program => 0,
        ControlKind::PitchBendRange => 1,
        ControlKind::Volume => 2,
        ControlKind::Pan => 3,
        ControlKind::Expression => 4,
        ControlKind::Modulation => 5,
        ControlKind::Reverb => 6,
        ControlKind::Chorus => 7,
        ControlKind::PitchBend => 8,
    }
}

fn controller(controller: u8, value: i32) -> ChannelMessage {
    ChannelMessage::Controller {
        controller,
        value: value as u8,
    }
}
