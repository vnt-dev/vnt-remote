//! HEVC RPS dependencies (including inter-RPS prediction). All signalled used
//! pictures are dependencies; list reordering cannot introduce another picture.
use super::bits::{nals, rbsp, Bits};
use std::collections::BTreeMap;
type Rps = Vec<(i32, bool)>;
struct Sps {
    id: u32,
    origin: u64,
    poc_bits: u32,
    sets: Vec<Rps>,
    long_term: bool,
    long_terms: Vec<(u32, bool)>,
    separate: bool,
    address_bits: u32,
}
struct Pps {
    id: u32,
    origin: u64,
    sps: u32,
    dependent: bool,
    output: bool,
    extra: u32,
}
#[derive(Default)]
pub(super) struct Parser {
    sps: BTreeMap<u32, Sps>,
    pps: BTreeMap<u32, Pps>,
    vps_origin: u64,
    dpb: BTreeMap<i32, u64>,
    previous_poc: i32,
    valid: bool,
}
struct Slice {
    poc: i32,
    used: Vec<i32>,
    retained: Vec<i32>,
    references: Vec<u64>,
    independent: bool,
    temporal: u8,
    nal: u8,
}
impl Parser {
    pub fn frame(&mut self, data: &[u8], id: u64) -> Option<(bool, Vec<u64>)> {
        let result = self.parse(data, id);
        if result.is_none() {
            self.valid = false;
            self.dpb.clear();
        }
        result
    }
    fn parse(&mut self, data: &[u8], id: u64) -> Option<(bool, Vec<u64>)> {
        let mut picture: Option<Slice> = None;
        for nal in nals(data)? {
            if nal.len() < 2 || nal[0] & 0x81 != 0 || nal[1] & 0xf8 != 0 || nal[1] & 7 == 0 {
                return None;
            }
            let kind = (nal[0] >> 1) & 63;
            let payload = rbsp(&nal[2..]);
            let mut b = Bits::new(&payload);
            match kind {
                32 => {
                    self.vps_origin = id;
                }
                33 => {
                    let s = sps(&mut b, id)?;
                    self.sps.insert(s.id, s);
                }
                34 => {
                    let p = pps(&mut b, id)?;
                    self.pps.insert(p.id, p);
                }
                0..=21 => {
                    let first = b.flag()?;
                    if (16..=23).contains(&kind) {
                        b.flag()?;
                    }
                    let p = self.pps.get(&b.bounded_ue(63)?)?;
                    let s = self.sps.get(&p.sps)?;
                    if !first {
                        let dependent = p.dependent && b.flag()?;
                        b.read(s.address_bits)?;
                        if dependent {
                            picture.as_ref()?;
                            continue;
                        }
                    }
                    b.skip(p.extra)?;
                    b.bounded_ue(2)?;
                    if p.output {
                        b.flag()?;
                    }
                    if s.separate {
                        return None;
                    }
                    let independent = matches!(kind, 19 | 20);
                    let mut long_term_retained = Vec::new();
                    let mut long_term_used = Vec::new();
                    let (poc, set) = if independent {
                        (0, vec![])
                    } else {
                        let lsb = b.read(s.poc_bits)? as i32;
                        let max = 1i32 << s.poc_bits;
                        let prev_lsb = self.previous_poc.rem_euclid(max);
                        let mut msb = self.previous_poc - prev_lsb;
                        if lsb < prev_lsb && prev_lsb - lsb >= max / 2 {
                            msb += max;
                        } else if lsb > prev_lsb && lsb - prev_lsb > max / 2 {
                            msb -= max;
                        }
                        // Broken-link access is not an independent decoder reset.
                        if matches!(kind, 16..=18) {
                            return None;
                        }
                        let poc = msb.checked_add(lsb)?;
                        let set = if b.flag()? {
                            if s.sets.is_empty() {
                                return None;
                            }
                            let index = if s.sets.len() > 1 {
                                b.read(ceil_log2(s.sets.len() as u32))? as usize
                            } else {
                                0
                            };
                            s.sets.get(index)?.clone()
                        } else {
                            rps(&mut b, &s.sets, true)?
                        };
                        if s.long_term {
                            let from_sps = if s.long_terms.is_empty() {
                                0
                            } else {
                                b.bounded_ue(s.long_terms.len() as u32)?
                            };
                            let additional = b.bounded_ue(32)?;
                            if from_sps + additional > 32 {
                                return None;
                            }
                            let mut previous_cycle = 0i32;
                            for i in 0..from_sps + additional {
                                let (lt_lsb, used) = if i < from_sps {
                                    let index = if s.long_terms.len() > 1 {
                                        b.read(ceil_log2(s.long_terms.len() as u32))? as usize
                                    } else {
                                        0
                                    };
                                    *s.long_terms.get(index)?
                                } else {
                                    (b.read(s.poc_bits)?, b.flag()?)
                                };
                                let long_poc = if b.flag()? {
                                    let mut cycle = b.bounded_ue(65535)? as i32;
                                    if i != 0 && i != from_sps {
                                        cycle = cycle.checked_add(previous_cycle)?;
                                    }
                                    previous_cycle = cycle;
                                    poc.checked_sub(cycle.checked_mul(max)?)?
                                        .checked_sub(lsb)?
                                        .checked_add(lt_lsb as i32)?
                                } else {
                                    previous_cycle = 0;
                                    let mut matches = self
                                        .dpb
                                        .keys()
                                        .filter(|poc| poc.rem_euclid(max) == lt_lsb as i32);
                                    let candidate = *matches.next()?;
                                    if matches.next().is_some() {
                                        return None;
                                    }
                                    candidate
                                };
                                long_term_retained.push(long_poc);
                                if used {
                                    long_term_used.push(long_poc);
                                }
                            }
                        }
                        (poc, set)
                    };
                    let mut retained: Vec<_> = set
                        .iter()
                        .map(|(d, _)| poc.checked_add(*d))
                        .collect::<Option<_>>()?;
                    let mut used = set
                        .iter()
                        .filter(|(_, used)| *used)
                        .map(|(d, _)| poc.checked_add(*d))
                        .collect::<Option<Vec<_>>>()?;
                    retained.extend(long_term_retained);
                    used.extend(long_term_used);
                    let mut references = Vec::new();
                    for origin in [p.origin, s.origin, self.vps_origin] {
                        if origin == 0 {
                            return None;
                        }
                        if origin != id {
                            references.push(origin);
                        }
                    }
                    let slice = Slice {
                        poc,
                        used,
                        retained,
                        references,
                        independent,
                        temporal: (nal[1] & 7) - 1,
                        nal: kind,
                    };
                    if let Some(previous) = &picture {
                        if previous.poc != slice.poc
                            || previous.used != slice.used
                            || previous.retained != slice.retained
                            || previous.independent != slice.independent
                        {
                            return None;
                        }
                    } else {
                        if !first {
                            return None;
                        }
                        picture = Some(slice);
                    }
                }
                35..=40 => {}
                _ => return None,
            }
        }
        let mut p = picture?;
        if p.independent {
            self.dpb.clear();
            self.valid = true;
            self.previous_poc = 0;
        }
        if !self.valid {
            return None;
        }
        for poc in &p.used {
            p.references.push(*self.dpb.get(poc)?);
        }
        self.dpb.retain(|poc, _| p.retained.contains(poc));
        // *_N pictures cannot be used as future references.
        if p.nal > 15 || p.nal % 2 == 1 {
            self.dpb.insert(p.poc, id);
        }
        if p.temporal == 0 && !matches!(p.nal, 6..=9) && (p.nal > 15 || p.nal % 2 == 1) {
            self.previous_poc = p.poc;
        }
        if self.dpb.len() > 32 {
            return None;
        }
        Some((p.independent && p.references.is_empty(), p.references))
    }
}
fn ceil_log2(v: u32) -> u32 {
    if v <= 1 {
        0
    } else {
        32 - (v - 1).leading_zeros()
    }
}
fn sps(b: &mut Bits<'_>, origin: u64) -> Option<Sps> {
    b.read(4)?;
    let layers = b.read(3)?;
    b.flag()?;
    b.skip(96)?;
    let mut profiles = Vec::new();
    for _ in 0..layers {
        profiles.push((b.flag()?, b.flag()?));
    }
    if layers > 0 {
        b.skip((8 - layers) * 2)?;
    }
    for (profile, level) in profiles {
        if profile {
            b.skip(88)?;
        }
        if level {
            b.skip(8)?;
        }
    }
    let id = b.bounded_ue(15)?;
    let chroma = b.bounded_ue(3)?;
    let separate = chroma == 3 && b.flag()?;
    let width = b.bounded_ue(32768)?;
    let height = b.bounded_ue(32768)?;
    if width == 0 || height == 0 {
        return None;
    }
    if b.flag()? {
        for _ in 0..4 {
            b.ue()?;
        }
    }
    b.bounded_ue(8)?;
    b.bounded_ue(8)?;
    let poc_bits = b.bounded_ue(12)? + 4;
    let all_layers = b.flag()?;
    for _ in if all_layers { 0 } else { layers }..=layers {
        b.bounded_ue(16)?;
        b.ue()?;
        b.ue()?;
    }
    let min_cb = b.bounded_ue(3)? + 3;
    let ctb = min_cb + b.bounded_ue(3)?;
    if ctb > 6 {
        return None;
    }
    for _ in 0..4 {
        b.bounded_ue(32)?;
    }
    if b.flag()? && b.flag()? {
        scaling(b)?;
    }
    b.flag()?;
    b.flag()?;
    if b.flag()? {
        b.skip(8)?;
        b.ue()?;
        b.ue()?;
        b.flag()?;
    }
    let count = b.bounded_ue(64)?;
    let mut sets = Vec::new();
    for _ in 0..count {
        sets.push(rps(b, &sets, false)?);
    }
    let long_term = b.flag()?;
    let mut long_terms = Vec::new();
    if long_term {
        for _ in 0..b.bounded_ue(32)? {
            long_terms.push((b.read(poc_bits)?, b.flag()?));
        }
    }
    let ctbs = width
        .div_ceil(1 << ctb)
        .checked_mul(height.div_ceil(1 << ctb))?;
    Some(Sps {
        id,
        origin,
        poc_bits,
        sets,
        long_term,
        long_terms,
        separate,
        address_bits: ceil_log2(ctbs),
    })
}
fn pps(b: &mut Bits<'_>, origin: u64) -> Option<Pps> {
    let id = b.bounded_ue(63)?;
    let sps = b.bounded_ue(15)?;
    let dependent = b.flag()?;
    let output = b.flag()?;
    let extra = b.read(3)?;
    Some(Pps {
        id,
        origin,
        sps,
        dependent,
        output,
        extra,
    })
}
fn scaling(b: &mut Bits<'_>) -> Option<()> {
    for size in 0..4 {
        for _ in (0..6).step_by(if size == 3 { 3 } else { 1 }) {
            if !b.flag()? {
                b.ue()?;
            } else {
                if size > 1 {
                    b.se()?;
                }
                for _ in 0..64.min(1 << (4 + 2 * size)) {
                    b.se()?;
                }
            }
        }
    }
    Some(())
}
fn rps(b: &mut Bits<'_>, sets: &[Rps], slice: bool) -> Option<Rps> {
    let mut out = Vec::new();
    if !sets.is_empty() && b.flag()? {
        let delta_index = if slice {
            b.bounded_ue(sets.len() as u32 - 1)? as usize + 1
        } else {
            1
        };
        let reference = sets.get(sets.len().checked_sub(delta_index)?)?;
        let sign = b.flag()?;
        let delta = (b.bounded_ue(32767)? + 1) as i32 * if sign { -1 } else { 1 };
        for offset in reference.iter().map(|(d, _)| *d).chain(std::iter::once(0)) {
            let used = b.flag()?;
            let keep = used || b.flag()?;
            let shifted = offset.checked_add(delta)?;
            if keep && shifted != 0 {
                out.push((shifted, used));
            }
        }
    } else {
        let negative = b.bounded_ue(16)?;
        let positive = b.bounded_ue(16)?;
        let mut delta = 0i32;
        for _ in 0..negative {
            delta = delta.checked_sub(b.bounded_ue(32767)? as i32 + 1)?;
            out.push((delta, b.flag()?));
        }
        delta = 0;
        for _ in 0..positive {
            delta = delta.checked_add(b.bounded_ue(32767)? as i32 + 1)?;
            out.push((delta, b.flag()?));
        }
    }
    if out.len() > 32 {
        return None;
    }
    out.sort_by_key(|(d, _)| if *d < 0 { (0, -*d) } else { (1, *d) });
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::super::bits::{pack, ue};
    use super::*;
    #[test]
    fn predicted_rps_preserves_unused_retained_pictures_and_sorts_used_references() {
        let sets = vec![vec![(-1, true), (-3, false), (2, true)]];
        // Prediction +1; shifted zero is discarded, -2 is retained but unused,
        // positive references +1 and +3 are both used.
        let bits = pack(&format!("10{}10111", ue(0)));
        assert_eq!(
            rps(&mut Bits::new(&bits), &sets, false),
            Some(vec![(-2, false), (1, true), (3, true)])
        );
        let slice_bits = pack(&format!("1{}0{}10111", ue(0), ue(0)));
        assert_eq!(
            rps(&mut Bits::new(&slice_bits), &sets, true),
            Some(vec![(-2, false), (1, true), (3, true)])
        );
    }
}
