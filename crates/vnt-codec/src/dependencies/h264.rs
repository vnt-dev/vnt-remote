//! H.264 frame-picture I/P slice headers, reference lists and MMCO (7.3.3).
//! Field pictures, B/SP/SI and extensions fail closed; the low-delay encoders
//! used by this application normally emit frame-picture I/P access units.
use super::bits::{nals, rbsp, Bits};
use std::collections::BTreeMap;

#[derive(Clone)]
struct Sps {
    id: u32,
    origin: u64,
    frame_bits: u32,
    poc: u32,
    poc_bits: u32,
    delta_zero: bool,
    frame_only: bool,
    chroma: u32,
    max_refs: usize,
}
#[derive(Clone)]
struct Pps {
    id: u32,
    origin: u64,
    sps: u32,
    bottom_poc: bool,
    refs: usize,
    weighted: bool,
    redundant: bool,
}
#[derive(Clone)]
struct Picture {
    number: i32,
    id: u64,
    long: Option<u32>,
}
#[derive(Default)]
pub(super) struct Parser {
    sps: BTreeMap<u32, Sps>,
    pps: BTreeMap<u32, Pps>,
    dpb: Vec<Picture>,
    valid: bool,
}
#[derive(PartialEq, Eq)]
struct Marking {
    idr: bool,
    long_idr: bool,
    operations: Vec<(u32, u32, u32)>,
}
struct Slice {
    number: i32,
    references: Vec<u64>,
    marking: Marking,
    max_number: i32,
    max_refs: usize,
    reference: bool,
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
        let mut slices: Vec<Slice> = vec![];
        for nal in nals(data)? {
            let header = *nal.first()?;
            if header & 0x80 != 0 {
                return None;
            }
            let payload = rbsp(&nal[1..]);
            let mut b = Bits::new(&payload);
            match header & 31 {
                7 => {
                    let s = sps(&mut b, id)?;
                    self.sps.insert(s.id, s);
                }
                8 => {
                    let p = pps(&mut b, id)?;
                    self.pps.insert(p.id, p);
                }
                1 | 5 => slices.push(self.slice(&mut b, header, id)?),
                6 | 9 | 10 | 11 | 12 => {}
                _ => return None,
            }
        }
        let first = slices.first()?;
        if slices.iter().any(|s| {
            s.number != first.number || s.marking != first.marking || s.reference != first.reference
        }) {
            return None;
        }
        let mut references = slices
            .iter()
            .flat_map(|s| s.references.iter().copied())
            .collect::<Vec<_>>();
        if first.marking.idr {
            self.dpb.clear();
            self.valid = true;
        } else if !self.valid {
            return None;
        }
        if first.reference {
            let mut current_long = first.marking.long_idr.then_some(0);
            let mut current_number = first.number;
            for &(op, a, c) in &first.marking.operations {
                let target = first.number - (a as i32 + 1);
                match op {
                    1 => self.dpb.retain(|p| {
                        p.long.is_some() || wrap(p.number, first.number, first.max_number) != target
                    }),
                    2 => self.dpb.retain(|p| p.long != Some(a)),
                    3 => {
                        self.dpb.retain(|p| p.long != Some(c));
                        let p = self.dpb.iter_mut().find(|p| {
                            p.long.is_none()
                                && wrap(p.number, first.number, first.max_number) == target
                        })?;
                        p.long = Some(c);
                    }
                    4 => self
                        .dpb
                        .retain(|p| p.long.is_none_or(|idx| a != 0 && idx < a)),
                    5 => {
                        self.dpb.clear();
                        current_number = 0;
                    }
                    6 => {
                        self.dpb.retain(|p| p.long != Some(a));
                        current_long = Some(a);
                    }
                    _ => return None,
                }
            }
            if !first.marking.idr
                && first.marking.operations.is_empty()
                && self.dpb.len() >= first.max_refs
            {
                let oldest = self
                    .dpb
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| p.long.is_none())
                    .min_by_key(|(_, p)| wrap(p.number, first.number, first.max_number))
                    .map(|(i, _)| i)?;
                self.dpb.remove(oldest);
            }
            self.dpb.push(Picture {
                number: current_number,
                id,
                long: current_long,
            });
            if self.dpb.len() > 32 {
                return None;
            }
        }
        references.sort_unstable();
        references.dedup();
        let independent = first.marking.idr && references.is_empty();
        Some((independent, references))
    }
    fn slice(&self, b: &mut Bits<'_>, header: u8, id: u64) -> Option<Slice> {
        b.ue()?;
        let kind = b.bounded_ue(9)? % 5;
        if kind != 0 && kind != 2 {
            return None;
        }
        let p = self.pps.get(&b.ue()?)?;
        let s = self.sps.get(&p.sps)?;
        let number = b.read(s.frame_bits)? as i32;
        if !s.frame_only && b.flag()? {
            return None;
        }
        let idr = header & 31 == 5;
        if idr {
            b.ue()?;
        }
        match s.poc {
            0 => {
                b.read(s.poc_bits)?;
                if p.bottom_poc {
                    b.se()?;
                }
            }
            1 if !s.delta_zero => {
                b.se()?;
                if p.bottom_poc {
                    b.se()?;
                }
            }
            _ => {}
        }
        if p.redundant {
            b.ue()?;
        }
        let mut active = p.refs;
        if kind == 0 && b.flag()? {
            active = b.bounded_ue(31)? as usize + 1;
        }
        let max_number = 1i32 << s.frame_bits;
        let mut list: Vec<_> = self
            .dpb
            .iter()
            .filter(|p| p.long.is_none())
            .cloned()
            .collect();
        list.sort_by_key(|p| std::cmp::Reverse(wrap(p.number, number, max_number)));
        let mut long: Vec<_> = self
            .dpb
            .iter()
            .filter(|p| p.long.is_some())
            .cloned()
            .collect();
        long.sort_by_key(|p| p.long);
        list.extend(long);
        if kind == 0 && b.flag()? {
            let mut predicted = number;
            let mut index = 0;
            loop {
                let op = b.bounded_ue(3)?;
                if op == 3 {
                    break;
                }
                if index >= 32 {
                    return None;
                }
                let selected = if op < 2 {
                    let delta = b.bounded_ue(max_number as u32 - 1)? as i32 + 1;
                    predicted = if op == 0 {
                        (predicted - delta).rem_euclid(max_number)
                    } else {
                        (predicted + delta).rem_euclid(max_number)
                    };
                    self.dpb
                        .iter()
                        .find(|p| p.long.is_none() && p.number == predicted)?
                        .clone()
                } else {
                    let n = b.ue()?;
                    self.dpb.iter().find(|p| p.long == Some(n))?.clone()
                };
                list.insert(index.min(list.len()), selected.clone());
                let mut tail = list.split_off(index + 1);
                tail.retain(|p| p.id != selected.id);
                list.extend(tail);
                index += 1;
            }
        }
        if kind == 0 && p.weighted {
            b.bounded_ue(7)?;
            if s.chroma != 0 {
                b.bounded_ue(7)?;
            }
            for _ in 0..active {
                if b.flag()? {
                    b.se()?;
                    b.se()?;
                }
                if s.chroma != 0 && b.flag()? {
                    for _ in 0..2 {
                        b.se()?;
                        b.se()?;
                    }
                }
            }
        }
        let reference = header & 0x60 != 0;
        let mut marking = Marking {
            idr,
            long_idr: false,
            operations: vec![],
        };
        if reference {
            if idr {
                b.flag()?;
                marking.long_idr = b.flag()?;
            } else if b.flag()? {
                for _ in 0..64 {
                    let op = b.bounded_ue(6)?;
                    if op == 0 {
                        break;
                    }
                    let a = if matches!(op, 1 | 2 | 3 | 4 | 6) {
                        b.ue()?
                    } else {
                        0
                    };
                    let c = if op == 3 { b.ue()? } else { 0 };
                    marking.operations.push((op, a, c));
                    if marking.operations.len() == 64 {
                        return None;
                    }
                }
            }
        }
        let mut references = vec![];
        if kind == 0 {
            if list.len() < active {
                return None;
            }
            references.extend(list[..active].iter().map(|p| p.id));
        }
        for origin in [p.origin, s.origin] {
            if origin != id {
                references.push(origin);
            }
        }
        Some(Slice {
            number,
            references,
            marking,
            max_number,
            max_refs: s.max_refs,
            reference,
        })
    }
}
fn wrap(n: i32, current: i32, max: i32) -> i32 {
    if n > current {
        n - max
    } else {
        n
    }
}
fn scaling(b: &mut Bits<'_>, size: usize) -> Option<()> {
    let mut last = 8;
    let mut next = 8;
    for _ in 0..size {
        if next != 0 {
            next = (last + b.se()? + 256).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}
fn sps(b: &mut Bits<'_>, origin: u64) -> Option<Sps> {
    let profile = b.read(8)?;
    b.skip(16)?;
    let id = b.bounded_ue(31)?;
    let mut chroma = 1;
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma = b.bounded_ue(3)?;
        if chroma == 3 && b.flag()? {
            return None;
        }
        b.bounded_ue(6)?;
        b.bounded_ue(6)?;
        b.flag()?;
        if b.flag()? {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if b.flag()? {
                    scaling(b, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    let frame_bits = b.bounded_ue(12)? + 4;
    let poc = b.bounded_ue(2)?;
    let mut poc_bits = 0;
    let mut delta_zero = false;
    if poc == 0 {
        poc_bits = b.bounded_ue(12)? + 4;
    }
    if poc == 1 {
        delta_zero = b.flag()?;
        b.se()?;
        b.se()?;
        for _ in 0..b.bounded_ue(255)? {
            b.se()?;
        }
    }
    let max_refs = b.bounded_ue(16)? as usize;
    b.flag()?;
    b.ue()?;
    b.ue()?;
    let frame_only = b.flag()?;
    Some(Sps {
        id,
        origin,
        frame_bits,
        poc,
        poc_bits,
        delta_zero,
        frame_only,
        chroma,
        max_refs: max_refs.max(1),
    })
}
fn pps(b: &mut Bits<'_>, origin: u64) -> Option<Pps> {
    let id = b.bounded_ue(255)?;
    let sps = b.bounded_ue(31)?;
    b.flag()?;
    let bottom_poc = b.flag()?;
    if b.ue()? != 0 {
        return None;
    } // Flexible macroblock ordering.
    let refs = b.bounded_ue(31)? as usize + 1;
    b.bounded_ue(31)?;
    let weighted = b.flag()?;
    b.read(2)?;
    b.se()?;
    b.se()?;
    b.se()?;
    b.flag()?;
    b.flag()?;
    let redundant = b.flag()?;
    Some(Pps {
        id,
        origin,
        sps,
        bottom_poc,
        refs,
        weighted,
        redundant,
    })
}

#[cfg(test)]
mod tests {
    use super::super::bits::{pack, ue};
    use super::*;
    fn parser() -> Parser {
        let mut p = Parser {
            valid: true,
            ..Default::default()
        };
        p.sps.insert(
            0,
            Sps {
                id: 0,
                origin: 1,
                frame_bits: 4,
                poc: 2,
                poc_bits: 0,
                delta_zero: false,
                frame_only: true,
                chroma: 1,
                max_refs: 2,
            },
        );
        p.pps.insert(
            0,
            Pps {
                id: 0,
                origin: 1,
                sps: 0,
                bottom_poc: false,
                refs: 1,
                weighted: false,
                redundant: false,
            },
        );
        p.dpb.push(Picture {
            number: 0,
            id: 1,
            long: Some(3),
        });
        p.dpb.push(Picture {
            number: 2,
            id: 2,
            long: None,
        });
        p
    }
    fn nal(reference: bool, suffix: &str) -> Vec<u8> {
        // first_mb=0, P slice, PPS=0, frame_num=3, default active count.
        let mut out = vec![0, 0, 0, 1, if reference { 0x41 } else { 0x01 }];
        out.extend(pack(&format!("11100110{suffix}")));
        out
    }
    #[test]
    fn long_term_list_modification_and_mmco_manage_the_reference_bank() {
        let mut p = parser();
        // Select long-term picture 3, remove short picture 2, assign current to
        // long-term index 2. The old short picture is not a dependency.
        let suffix = format!(
            "1{}{}{}1{}{}{}{}{}",
            ue(2),
            ue(3),
            ue(3),
            ue(1),
            ue(0),
            ue(6),
            ue(2),
            ue(0)
        );
        let (_, refs) = p.frame(&nal(true, &suffix), 3).unwrap();
        assert_eq!(refs, vec![1]);
        assert!(p.dpb.iter().any(|p| p.id == 3 && p.long == Some(2)));
        assert!(!p.dpb.iter().any(|p| p.id == 2));
    }
    #[test]
    fn a_nonreference_picture_does_not_become_a_dependency_of_the_next_picture() {
        let mut p = parser();
        let (_, refs) = p.frame(&nal(false, "0"), 3).unwrap();
        assert_eq!(refs, vec![1, 2]);
        assert!(!p.dpb.iter().any(|p| p.id == 3));
        let (_, refs) = p.frame(&nal(true, "00"), 4).unwrap();
        assert_eq!(refs, vec![1, 2]);
        assert!(!refs.contains(&3));
    }
}
