//! AV1 OBU sequence/frame headers and the eight reference slots (spec 5.5/5.9).
use super::bits::Bits;
#[derive(Clone)]
struct Sequence {
    origin: u64,
    reduced: bool,
    equal_interval: bool,
    presentation_bits: u32,
    removal_bits: u32,
    models: Vec<(u32, bool)>,
    frame_id_bits: u32,
    delta_id_bits: u32,
    order_bits: u32,
    screen: u32,
    integer_mv: u32,
    film_grain: bool,
}
#[derive(Clone, Copy)]
struct Slot {
    id: u64,
    key: bool,
    frame_id: u32,
    order: u32,
}
#[derive(Default)]
pub(super) struct Parser {
    sequence: Option<Sequence>,
    slots: [Option<Slot>; 8],
    valid: bool,
}
impl Parser {
    pub fn frame(&mut self, data: &[u8], id: u64) -> Option<(bool, Vec<u64>)> {
        let result = self.parse(data, id);
        if result.is_none() {
            self.valid = false;
            self.slots.fill(None);
        }
        result
    }
    fn parse(&mut self, mut data: &[u8], id: u64) -> Option<(bool, Vec<u64>)> {
        let mut references = vec![];
        let mut saw_frame = false;
        let mut independent = false;
        while !data.is_empty() {
            let header = data[0];
            data = &data[1..];
            if header & 0x81 != 0 || header & 2 == 0 {
                return None;
            }
            let kind = (header >> 3) & 15;
            let mut temporal = 0;
            let mut spatial = 0;
            if header & 4 != 0 {
                let ext = *data.first()?;
                data = &data[1..];
                if ext & 7 != 0 {
                    return None;
                }
                temporal = ext >> 5;
                spatial = (ext >> 3) & 3;
            }
            // Multiple spatial layers need a separate slot bank per layer.
            if spatial != 0 {
                return None;
            }
            let (size, consumed) = leb(data)?;
            data = data.get(consumed..)?;
            let payload = data.get(..size)?;
            data = data.get(size..)?;
            match kind {
                1 => {
                    self.sequence = Some(sequence(&mut Bits::new(payload), id)?);
                }
                3 | 6 => {
                    let (reset, deps) =
                        self.header(&mut Bits::new(payload), id, temporal, spatial)?;
                    // A later reset does not erase dependencies of an earlier
                    // hidden frame in the same temporal-unit packet.
                    if reset && !saw_frame {
                        independent = true;
                    }
                    references.extend(deps);
                    saw_frame = true;
                }
                2 | 4 | 5 | 7 | 15 => {}
                _ => return None,
            }
        }
        if !saw_frame {
            return None;
        }
        references.retain(|&r| r != id);
        Some((independent && references.is_empty(), references))
    }
    fn header(
        &mut self,
        b: &mut Bits<'_>,
        id: u64,
        temporal: u8,
        spatial: u8,
    ) -> Option<(bool, Vec<u64>)> {
        let s = self.sequence.as_ref()?;
        if !s.reduced && b.flag()? {
            let index = b.read(3)? as usize;
            if s.presentation_bits != 0 && !s.equal_interval {
                b.skip(s.presentation_bits)?;
            }
            let slot = self.slots[index]?;
            if s.frame_id_bits > 0 && b.read(s.frame_id_bits)? != slot.frame_id {
                return None;
            }
            if !self.valid {
                return None;
            }
            // Showing an existing key picture refreshes every reference slot.
            if slot.key {
                self.slots.fill(Some(slot));
            }
            let mut deps = vec![slot.id];
            if s.origin != id {
                deps.push(s.origin);
            }
            return Some((false, deps));
        }
        let frame_type = if s.reduced { 0 } else { b.read(2)? };
        let show = s.reduced || b.flag()?;
        if show && s.presentation_bits != 0 && !s.equal_interval {
            b.skip(s.presentation_bits)?;
        }
        if !show {
            b.flag()?;
        }
        let resilient = frame_type == 3 || frame_type == 0 && show || s.reduced || b.flag()?;
        let intra = frame_type == 0 || frame_type == 2;
        b.flag()?; // disable_cdf_update
        let screen = if s.screen == 2 {
            b.flag()?
        } else {
            s.screen != 0
        };
        if screen && s.integer_mv == 2 {
            b.flag()?;
        }
        let frame_id = if s.frame_id_bits != 0 {
            b.read(s.frame_id_bits)?
        } else {
            0
        };
        if s.frame_id_bits != 0 {
            let modulus = 1u32 << s.frame_id_bits;
            let window = 1u32 << s.delta_id_bits;
            for slot in &mut self.slots {
                if slot.is_some_and(|r| (frame_id + modulus - r.frame_id) % modulus > window) {
                    *slot = None;
                }
            }
        }
        if frame_type != 3 && !s.reduced {
            b.flag()?;
        } // frame_size_override_flag
        let order = b.read(s.order_bits)?;
        let primary = if !intra && !resilient { b.read(3)? } else { 7 };
        if s.removal_bits != 0 && b.flag()? {
            for &(op, present) in &s.models {
                if present
                    && (op == 0 || op & (1 << temporal) != 0 && op & (1 << (spatial + 8)) != 0)
                {
                    b.skip(s.removal_bits)?;
                }
            }
        }
        let refresh = if frame_type == 3 || frame_type == 0 && show {
            255
        } else {
            b.read(8)?
        };
        if (!intra || refresh != 255) && resilient && s.order_bits > 0 {
            for slot in &mut self.slots {
                let hint = b.read(s.order_bits)?;
                if slot.is_some_and(|r| r.order != hint) {
                    *slot = None;
                }
            }
        }
        let mut references = vec![];
        if !intra {
            let short = s.order_bits > 0 && b.flag()?;
            let mut indices = if short {
                infer_refs(
                    &self.slots,
                    b.read(3)? as usize,
                    b.read(3)? as usize,
                    order,
                    s.order_bits,
                )?
            } else {
                [0; 7]
            };
            for index in &mut indices {
                if !short {
                    *index = b.read(3)? as usize;
                }
                let slot = self.slots[*index]?;
                if s.frame_id_bits > 0 {
                    let delta = b.read(s.delta_id_bits)? + 1;
                    let modulus = 1u32 << s.frame_id_bits;
                    if (frame_id + modulus - delta) % modulus != slot.frame_id {
                        return None;
                    }
                }
                references.push(slot.id);
            }
            if primary < 7 {
                references.push(self.slots[indices[primary as usize]]?.id);
            }
            // Film-grain reuse can address an additional reference slot later
            // in the header. Include every live slot conservatively.
            if s.film_grain {
                references.extend(self.slots.iter().flatten().map(|r| r.id));
            }
        }
        let reset = frame_type == 0 && show;
        if reset {
            self.valid = true;
            self.slots.fill(None);
        }
        if !self.valid {
            return None;
        }
        if s.origin != id {
            references.push(s.origin);
        }
        let current = Slot {
            id,
            key: frame_type == 0,
            frame_id,
            order,
        };
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if refresh & (1 << i) != 0 {
                *slot = Some(current);
            }
        }
        Some((reset && references.is_empty(), references))
    }
}
// AV1 specification, set frame refs process (7.8). Preserve index tie-breaking,
// particularly the >= comparisons for latest backward/forward candidates.
fn infer_refs(
    slots: &[Option<Slot>; 8],
    last: usize,
    gold: usize,
    order: u32,
    bits: u32,
) -> Option<[usize; 7]> {
    let current = 1i32 << (bits - 1);
    let mut hints = [0; 8];
    for (i, slot) in slots.iter().enumerate() {
        let diff = (*slot)?.order as i32 - order as i32;
        hints[i] = current + ((diff & (current - 1)) - (diff & current));
    }
    if hints[last] >= current || hints[gold] >= current {
        return None;
    }
    let mut refs = [None; 7];
    refs[0] = Some(last);
    refs[3] = Some(gold);
    let mut used = [false; 8];
    used[last] = true;
    used[gold] = true;
    for (index, latest) in [(6, true), (4, false), (5, false)] {
        let mut candidate = None;
        for i in 0..8 {
            if used[i] || hints[i] < current {
                continue;
            }
            if candidate.is_none_or(|old| {
                if latest {
                    hints[i] >= hints[old]
                } else {
                    hints[i] < hints[old]
                }
            }) {
                candidate = Some(i);
            }
        }
        if let Some(i) = candidate {
            refs[index] = Some(i);
            used[i] = true;
        }
    }
    for index in [1, 2, 4, 5, 6] {
        if refs[index].is_some() {
            continue;
        }
        let mut candidate = None;
        for i in 0..8 {
            if !used[i] && hints[i] < current && candidate.is_none_or(|old| hints[i] >= hints[old])
            {
                candidate = Some(i);
            }
        }
        if let Some(i) = candidate {
            refs[index] = Some(i);
            used[i] = true;
        }
    }
    let first = (0..8).min_by_key(|&i| hints[i])?;
    Some(refs.map(|r| r.unwrap_or(first)))
}
fn leb(data: &[u8]) -> Option<(usize, usize)> {
    let mut value = 0u64;
    for (i, &byte) in data.iter().take(8).enumerate() {
        value |= ((byte & 127) as u64) << (i * 7);
        if byte & 128 == 0 {
            return Some((usize::try_from(value).ok()?, i + 1));
        }
    }
    None
}
fn sequence(b: &mut Bits<'_>, origin: u64) -> Option<Sequence> {
    let profile = b.read(3)?;
    if profile > 2 {
        return None;
    }
    b.flag()?;
    let reduced = b.flag()?;
    let mut equal_interval = false;
    let mut presentation_bits = 0;
    let mut removal_bits = 0;
    let mut models = Vec::new();
    if reduced {
        b.read(5)?;
    } else {
        let timing = b.flag()?;
        let mut buffer_bits = 0;
        if timing {
            b.skip(64)?;
            equal_interval = b.flag()?;
            if equal_interval {
                b.ue()?;
            }
            if b.flag()? {
                buffer_bits = b.read(5)? + 1;
                b.skip(32)?;
                removal_bits = b.read(5)? + 1;
                presentation_bits = b.read(5)? + 1;
            }
        }
        let display_delay = b.flag()?;
        let count = b.read(5)? + 1;
        for _ in 0..count {
            let op = b.read(12)?;
            let level = b.read(5)?;
            if level > 7 {
                b.flag()?;
            }
            let present = buffer_bits > 0 && b.flag()?;
            if present {
                b.skip(2 * buffer_bits + 1)?;
            }
            models.push((op, present));
            if display_delay && b.flag()? {
                b.skip(4)?;
            }
        }
    }
    let width_bits = b.read(4)? + 1;
    let height_bits = b.read(4)? + 1;
    b.read(width_bits)?;
    b.read(height_bits)?;
    let mut frame_id_bits = 0;
    let mut delta_id_bits = 0;
    if !reduced && b.flag()? {
        delta_id_bits = b.read(4)? + 2;
        frame_id_bits = delta_id_bits + b.read(3)? + 1;
    }
    b.skip(3)?;
    let mut order_bits = 0;
    let mut screen = 2;
    let mut integer_mv = 2;
    if !reduced {
        b.skip(4)?;
        let order = b.flag()?;
        if order {
            b.skip(2)?;
        }
        if !b.flag()? {
            screen = b.read(1)?;
        }
        if screen > 0 && !b.flag()? {
            integer_mv = b.read(1)?;
        }
        if order {
            order_bits = b.read(3)? + 1;
        }
    }
    b.skip(3)?; // superres, CDEF, restoration
    let high = b.flag()?;
    let depth = if profile == 2 && high && b.flag()? {
        12
    } else if high {
        10
    } else {
        8
    };
    let monochrome = profile != 1 && b.flag()?;
    let (primaries, transfer, matrix) = if b.flag()? {
        (b.read(8)?, b.read(8)?, b.read(8)?)
    } else {
        (2, 2, 2)
    };
    if monochrome {
        b.flag()?;
    } else {
        if !(primaries == 1 && transfer == 13 && matrix == 0) {
            b.flag()?;
            let (x, y) = match profile {
                0 => (true, true),
                1 => (false, false),
                _ if depth == 12 => {
                    let x = b.flag()?;
                    (x, x && b.flag()?)
                }
                _ => (true, false),
            };
            if x && y {
                b.read(2)?;
            }
        }
        b.flag()?; // separate_uv_delta_q
    }
    let film_grain = b.flag()?;
    Some(Sequence {
        origin,
        reduced,
        equal_interval,
        presentation_bits,
        removal_bits,
        models,
        frame_id_bits,
        delta_id_bits,
        order_bits,
        screen,
        integer_mv,
        film_grain,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parser() -> Parser {
        Parser {
            sequence: Some(Sequence {
                origin: 1,
                reduced: false,
                equal_interval: false,
                presentation_bits: 0,
                removal_bits: 0,
                models: vec![],
                frame_id_bits: 0,
                delta_id_bits: 0,
                order_bits: 0,
                screen: 0,
                integer_mv: 0,
                film_grain: false,
            }),
            slots: [Some(Slot {
                id: 1,
                key: true,
                frame_id: 0,
                order: 0,
            }); 8],
            valid: true,
        }
    }
    #[test]
    fn show_existing_frame_depends_on_the_selected_slot_and_refreshes_key_slots() {
        let mut p = parser();
        p.slots[3] = Some(Slot {
            id: 2,
            key: false,
            frame_id: 0,
            order: 0,
        });
        let (_, deps) = p.header(&mut Bits::new(&[0b1011_0000]), 4, 0, 0).unwrap();
        assert_eq!(deps, vec![2, 1]);
        assert_eq!(p.slots[0].unwrap().id, 1);
        p.slots[3].as_mut().unwrap().key = true;
        p.header(&mut Bits::new(&[0b1011_0000]), 5, 0, 0).unwrap();
        assert!(p.slots.iter().all(|s| s.unwrap().id == 2));
        p.slots[3] = None;
        assert!(p.header(&mut Bits::new(&[0b1011_0000]), 6, 0, 0).is_none());
    }
    #[test]
    fn short_signalling_infers_backward_and_forward_references_with_wraparound() {
        let hints = [14, 13, 12, 11, 1, 2, 3, 10];
        let slots = hints.map(|order| {
            Some(Slot {
                id: order as u64 + 1,
                key: false,
                frame_id: 0,
                order,
            })
        });
        let refs = infer_refs(&slots, 0, 1, 0, 4).unwrap();
        assert_eq!(refs, [0, 2, 3, 1, 4, 5, 6]);
        assert!(infer_refs(&slots, 4, 1, 0, 4).is_none());
    }
}
