//! Zone resolution: which samples a note plays and with which generator and
//! modulator values (SF2.04 §8.5, §9.4, §9.5.1).
//!
//! Instrument generators are absolute: a local zone replaces the global
//! zone, which replaces the default. Preset generators are relative: the
//! local preset zone replaces the global preset zone and the result is added
//! to the instrument value, for value generators only. Key and velocity
//! ranges intersect: a note must fall in the preset zone's range and in the
//! instrument zone's range (a local range replaces the global one).
//! Modulators follow the same pattern: instrument zones replace identical
//! default and global modulators; preset modulators add their amount to an
//! identical instrument-level modulator or are appended.

use super::modulator::{DEFAULT_MODULATORS, GENERATORS, Modulator, is_value_generator};
use super::sf2::{SoundFont, Zone};

/// Generator defaults (SF2.04 §8.1.3).
pub fn default_generators() -> [i32; GENERATORS] {
    let mut g = [0; GENERATORS];
    g[8] = 13500;
    for delay_or_time in [21, 23, 25, 26, 27, 28, 30, 33, 34, 35, 36, 38] {
        g[delay_or_time] = -12000;
    }
    g[46] = -1;
    g[47] = -1;
    g[56] = 100;
    g[58] = -1;
    g
}

/// One sample a note plays, with its summed generators and modulators.
#[derive(Debug, Clone, PartialEq)]
pub struct Region {
    pub sample: usize,
    pub generators: [i32; GENERATORS],
    pub modulators: Vec<Modulator>,
}

fn in_range(range: Range, value: u8) -> bool {
    range.is_none_or(|(low, high)| low <= value && value <= high)
}

/// An inclusive key or velocity range; `None` is the full range.
type Range = Option<(u8, u8)>;

fn ranges(local: &Zone, global: Option<&Zone>) -> (Range, Range) {
    (
        local.key_range.or(global.and_then(|g| g.key_range)),
        local
            .velocity_range
            .or(global.and_then(|g| g.velocity_range)),
    )
}

/// Merges zone modulators over a base list, replacing identical ones.
fn replace_modulators(base: &mut Vec<Modulator>, zone: Option<&Zone>) {
    for modulator in zone.into_iter().flat_map(|z| &z.modulators) {
        match base.iter_mut().find(|m| m.same_as(modulator)) {
            Some(existing) => *existing = *modulator,
            None => base.push(*modulator),
        }
    }
}

impl SoundFont {
    /// The regions a note with this key and velocity plays on a preset, in
    /// file order of preset zones, then instrument zones.
    pub fn regions(&self, preset: usize, key: u8, velocity: u8) -> Vec<Region> {
        let preset = &self.presets[preset];
        let mut out = Vec::new();
        for preset_zone in &preset.zones {
            let (keys, velocities) = ranges(preset_zone, preset.global.as_ref());
            if !in_range(keys, key) || !in_range(velocities, velocity) {
                continue;
            }
            let instrument = &self.instruments[preset_zone.target.expect("checked zone")];
            for zone in &instrument.zones {
                let (keys, velocities) = ranges(zone, instrument.global.as_ref());
                if !in_range(keys, key) || !in_range(velocities, velocity) {
                    continue;
                }
                out.push(self.region(
                    preset.global.as_ref(),
                    preset_zone,
                    instrument.global.as_ref(),
                    zone,
                ));
            }
        }
        out
    }

    fn region(
        &self,
        preset_global: Option<&Zone>,
        preset_zone: &Zone,
        instrument_global: Option<&Zone>,
        zone: &Zone,
    ) -> Region {
        let mut generators = default_generators();
        for source in [instrument_global, Some(zone)].into_iter().flatten() {
            for (operator, amount) in source.generators.iter().enumerate() {
                if let Some(amount) = amount {
                    generators[operator] = i32::from(*amount);
                }
            }
        }
        for operator in (0..GENERATORS).filter(|&o| is_value_generator(o)) {
            let offset = preset_zone.generators[operator]
                .or(preset_global.and_then(|g| g.generators[operator]));
            if let Some(offset) = offset {
                generators[operator] += i32::from(offset);
            }
        }

        let mut modulators = DEFAULT_MODULATORS.to_vec();
        replace_modulators(&mut modulators, instrument_global);
        replace_modulators(&mut modulators, Some(zone));
        let mut preset_modulators = Vec::new();
        replace_modulators(&mut preset_modulators, preset_global);
        replace_modulators(&mut preset_modulators, Some(preset_zone));
        for modulator in preset_modulators {
            match modulators.iter_mut().find(|m| m.same_as(&modulator)) {
                Some(existing) => {
                    existing.amount = existing.amount.saturating_add(modulator.amount)
                }
                None => modulators.push(modulator),
            }
        }
        Region {
            sample: zone.target.expect("checked zone"),
            generators,
            modulators,
        }
    }
}
