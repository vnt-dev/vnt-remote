pub(super) struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}
impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    pub fn read(&mut self, n: u32) -> Option<u32> {
        if n > 32 || self.pos.checked_add(n as usize)? > self.data.len() * 8 {
            return None;
        }
        let mut out = 0;
        for _ in 0..n {
            out = (out << 1) | ((self.data[self.pos / 8] >> (7 - self.pos % 8)) & 1) as u32;
            self.pos += 1;
        }
        Some(out)
    }
    pub fn flag(&mut self) -> Option<bool> {
        Some(self.read(1)? != 0)
    }
    pub fn skip(&mut self, n: u32) -> Option<()> {
        self.read(n.min(32))?;
        if n > 32 {
            self.skip(n - 32)?;
        }
        Some(())
    }
    pub fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros > 30 {
                return None;
            }
        }
        Some(((1u32 << zeros) - 1) + self.read(zeros)?)
    }
    pub fn se(&mut self) -> Option<i32> {
        let n = self.ue()?;
        Some(if n & 1 != 0 {
            (n / 2 + 1) as i32
        } else {
            -(n as i32 / 2)
        })
    }
    pub fn bounded_ue(&mut self, max: u32) -> Option<u32> {
        let v = self.ue()?;
        (v <= max).then_some(v)
    }
}
pub(super) fn rbsp(data: &[u8]) -> Vec<u8> {
    // Only reference headers are parsed. Never copy/unescape megabytes of
    // entropy-coded slice data to read a few dozen header bits. Excessively
    // large headers exhaust Bits and safely become Unknown.
    let data = &data[..data.len().min(16 * 1024)];
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}
pub(super) fn nals(data: &[u8]) -> Option<Vec<&[u8]>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while let Some(offset) = memchr::memmem::find(&data[i..], &[0, 0, 1]) {
        let prefix = i + offset;
        let begin = if prefix > 0 && data[prefix - 1] == 0 {
            prefix - 1
        } else {
            prefix
        };
        starts.push((begin, prefix + 3));
        i = prefix + 3;
    }
    if starts.is_empty() {
        return None;
    }
    Some(
        starts
            .iter()
            .enumerate()
            .map(|(i, &(_, start))| &data[start..starts.get(i + 1).map_or(data.len(), |s| s.0)])
            .collect(),
    )
}

#[cfg(test)]
pub(super) fn pack(bits: &str) -> Vec<u8> {
    let mut bytes = vec![0; bits.len().div_ceil(8)];
    for (i, bit) in bits.bytes().enumerate() {
        if bit == b'1' {
            bytes[i / 8] |= 1 << (7 - i % 8);
        }
    }
    bytes
}
#[cfg(test)]
pub(super) fn ue(value: u32) -> String {
    let code = format!("{:b}", value + 1);
    format!("{}{}", "0".repeat(code.len() - 1), code)
}
