//! The protobuf wire format, read without decoding whole messages: field access touches only
//! the bytes of the field it reads.

/// One field's value on the wire.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Wire<'a> {
    Varint(u64),
    Fixed64(u64),
    Len(&'a [u8]),
    Fixed32(u32),
    /// A group's body, without its end tag.
    Group(&'a [u8]),
}

/// Malformed wire bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corrupt {
    /// A tag that doesn't parse. The emulator's reader takes it as the end of the message.
    Tag,
    /// The value of field `n`.
    Value(u32),
}

pub fn varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut out = 0u64;
    for shift in (0..70).step_by(7) {
        let b = *buf.get(*pos)?;
        *pos += 1;
        if shift == 63 && b > 1 {
            return None;
        }
        out |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(out);
        }
    }
    None
}

fn fixed<const N: usize>(buf: &[u8], pos: &mut usize) -> Option<[u8; N]> {
    let bytes = buf.get(*pos..*pos + N)?;
    *pos += N;
    bytes.try_into().ok()
}

/// The fields of a message in wire order.
pub struct Fields<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Fields<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Fields { buf, pos: 0 }
    }

    fn value(&mut self, number: u32, wire_type: u64, depth: u32) -> Option<Wire<'a>> {
        let buf = self.buf;
        Some(match wire_type {
            0 => Wire::Varint(varint(buf, &mut self.pos)?),
            1 => Wire::Fixed64(u64::from_le_bytes(fixed(buf, &mut self.pos)?)),
            2 => {
                let len = usize::try_from(varint(buf, &mut self.pos)?).ok()?;
                let end = self.pos.checked_add(len)?;
                let bytes = buf.get(self.pos..end)?;
                self.pos = end;
                Wire::Len(bytes)
            }
            3 => {
                if depth > 64 {
                    return None;
                }
                let start = self.pos;
                loop {
                    let body_end = self.pos;
                    let tag = varint(buf, &mut self.pos)?;
                    let (n, t) = (u32::try_from(tag >> 3).ok()?, tag & 7);
                    if t == 4 {
                        if n != number {
                            return None;
                        }
                        break Wire::Group(&buf[start..body_end]);
                    }
                    self.value(n, t, depth + 1)?;
                }
            }
            5 => Wire::Fixed32(u32::from_le_bytes(fixed(buf, &mut self.pos)?)),
            _ => return None,
        })
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = Result<(u32, Wire<'a>), Corrupt>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let Some(tag) = varint(self.buf, &mut self.pos) else {
            self.pos = self.buf.len();
            return Some(Err(Corrupt::Tag));
        };
        let number = (tag >> 3) as u32;
        let wire_type = tag & 7;
        if number == 0 || tag >> 3 > u64::from(u32::MAX >> 3) || wire_type == 4 {
            self.pos = self.buf.len();
            return Some(Err(Corrupt::Tag));
        }
        match self.value(number, wire_type, 0) {
            Some(w) => Some(Ok((number, w))),
            None => {
                self.pos = self.buf.len();
                Some(Err(Corrupt::Value(number)))
            }
        }
    }
}

/// The values of a packed repeated scalar field (wire type of each element: 0, 1 or 5).
pub fn packed(bytes: &[u8], element_wire: u64) -> Option<Vec<Wire<'_>>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        out.push(match element_wire {
            0 => Wire::Varint(varint(bytes, &mut pos)?),
            1 => Wire::Fixed64(u64::from_le_bytes(fixed(bytes, &mut pos)?)),
            5 => Wire::Fixed32(u32::from_le_bytes(fixed(bytes, &mut pos)?)),
            _ => return None,
        });
    }
    Some(out)
}

pub fn zigzag64(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

pub fn zigzag32(v: u32) -> i32 {
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_fields_and_reports_corruption() {
        let f: Vec<_> = Fields::new(b"\x08\x05\x12\x02hi\x1d\x01\x00\x00\x00").collect();
        assert_eq!(
            f,
            vec![
                Ok((1, Wire::Varint(5))),
                Ok((2, Wire::Len(b"hi"))),
                Ok((3, Wire::Fixed32(1)))
            ]
        );
        assert_eq!(
            Fields::new(b"\xff\xff").collect::<Vec<_>>(),
            vec![Err(Corrupt::Tag)]
        );
        assert_eq!(
            Fields::new(b"\x08").collect::<Vec<_>>(),
            vec![Err(Corrupt::Value(1))]
        );
        // A group: field 4 start, field 1 varint 7 inside, field 4 end.
        assert_eq!(
            Fields::new(b"\x23\x08\x07\x24").collect::<Vec<_>>(),
            vec![Ok((4, Wire::Group(b"\x08\x07")))]
        );
        assert_eq!(zigzag64(3), -2);
        assert_eq!(zigzag32(4), 2);
        assert_eq!(
            packed(b"\x01\x96\x01", 0),
            Some(vec![Wire::Varint(1), Wire::Varint(150)])
        );
    }
}
