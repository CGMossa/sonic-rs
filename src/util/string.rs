use crate::error::ErrorCode::{self, *};
use crate::util::unicode::{handle_unicode_codepoint, handle_unicode_codepoint_mut};
use packed_simd::u8x32;
use std::mem::MaybeUninit;
use std::slice::{from_raw_parts, from_raw_parts_mut};

pub const ESCAPED_TAB: [u8; 256] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, b'"', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'/', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    b'\\', 0, 0, 0, 0, 0, b'\x08', /*\b*/
    0, 0, 0, b'\x0c', /*\f*/
    0, 0, 0, 0, 0, 0, 0, b'\n', 0, 0, 0, b'\r', 0, b'\t', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

pub(crate) struct StringBlock {
    bs_bits: u32,
    quote_bits: u32,
    unescaped_bits: u32,
}

impl StringBlock {
    const LANS: usize = 32;

    #[inline(always)]
    pub fn find(ptr: *const u8) -> Self {
        let v = unsafe {
            let chunck = from_raw_parts(ptr, Self::LANS);
            u8x32::from_slice_unaligned_unchecked(chunck)
        };
        let bs_bits = (v.eq(u8x32::splat(b'\\'))).bitmask();
        let quote_bits = (v.eq(u8x32::splat(b'"'))).bitmask();
        let unescaped_bits = (v.le(u8x32::splat(0x1f))).bitmask();
        Self {
            bs_bits,
            quote_bits,
            unescaped_bits,
        }
    }

    #[inline(always)]
    pub fn has_unesacped(&self) -> bool {
        (self.quote_bits.wrapping_sub(1) & self.unescaped_bits) != 0
    }

    #[inline(always)]
    pub fn has_quote_first(&self) -> bool {
        (self.bs_bits.wrapping_sub(1) & self.quote_bits) != 0 && !self.has_unesacped()
    }

    #[inline(always)]
    pub fn has_backslash(&self) -> bool {
        (self.quote_bits.wrapping_sub(1) & self.bs_bits) != 0
    }

    #[inline(always)]
    pub fn quote_index(&self) -> usize {
        self.quote_bits.trailing_zeros() as usize
    }

    #[inline(always)]
    pub fn bs_index(&self) -> usize {
        self.bs_bits.trailing_zeros() as usize
    }
}

// return the size of the actual parsed string
#[inline(always)]
pub(crate) unsafe fn parse_string_inplace(
    src: &mut *mut u8,
) -> std::result::Result<usize, ErrorCode> {
    const LANS: usize = 32;
    let sdst = *src;
    let mut block;

    // loop for string without escaped chars
    loop {
        block = StringBlock::find(*src);
        if block.has_quote_first() {
            let idx = block.quote_index();
            *src = src.add(idx + 1);
            return Ok(src.offset_from(sdst) as usize - 1);
        }
        if block.has_unesacped() {
            return Err(ControlCharacterWhileParsingString);
        }
        if block.has_backslash() {
            break;
        }
        *src = src.add(LANS);
    }

    let bs_dist = block.bs_index();
    *src = src.add(bs_dist);
    let mut dst = *src;

    // loop for string with escaped chars
    loop {
        'esacpe: loop {
            let escaped_char: u8 = *src.add(1);
            if escaped_char == b'u' {
                if !handle_unicode_codepoint_mut(src, &mut dst) {
                    return Err(InvalidUnicodeCodePoint);
                }
            } else {
                *dst = ESCAPED_TAB[escaped_char as usize];
                if *dst == 0 {
                    return Err(InvalidEscape);
                }
                *src = src.add(2);
                dst = dst.add(1);
            }

            // fast path for continous escaped chars
            if **src == b'\\' {
                continue 'esacpe;
            }
            break 'esacpe;
        }

        'find_and_move: loop {
            let v = unsafe {
                let ptr = *src;
                let chunck = from_raw_parts(ptr, LANS);
                u8x32::from_slice_unaligned_unchecked(chunck)
            };
            let block = StringBlock {
                bs_bits: (v.eq(u8x32::splat(b'\\'))).bitmask(),
                quote_bits: (v.eq(u8x32::splat(b'"'))).bitmask(),
                unescaped_bits: (v.le(u8x32::splat(0x1f))).bitmask(),
            };
            if block.has_quote_first() {
                while **src != b'"' {
                    *dst = **src;
                    dst = dst.add(1);
                    *src = src.add(1);
                }
                *src = src.add(1); // skip ending quote
                return Ok(dst.offset_from(sdst) as usize);
            }
            if block.has_unesacped() {
                return Err(ControlCharacterWhileParsingString);
            }
            if !block.has_backslash() {
                let chunck = from_raw_parts_mut(dst, LANS);
                v.write_to_slice_unaligned_unchecked(chunck);
                *src = src.add(LANS);
                dst = dst.add(LANS);
                continue 'find_and_move;
            }
            // TODO: loop unrooling here
            while **src != b'\\' {
                *dst = **src;
                dst = dst.add(1);
                *src = src.add(1);
            }
            break 'find_and_move;
        }
    } // slow loop for escaped chars
}

// return the size of the actual parsed string, parse string into the dst
// len(dst) >= len(src) + 32
#[inline(always)]
pub(crate) unsafe fn parse_string_unchecked(
    src: &mut *const u8,
    mut dst: *mut u8,
) -> std::result::Result<usize, ErrorCode> {
    const LANS: usize = 32;
    let sdst = dst;

    // loop for string with escaped chars
    loop {
        'find_and_move: loop {
            let v = unsafe {
                let ptr = *src;
                let chunck = from_raw_parts(ptr, LANS);
                u8x32::from_slice_unaligned_unchecked(chunck)
            };
            let block = StringBlock {
                bs_bits: (v.eq(u8x32::splat(b'\\'))).bitmask(),
                quote_bits: (v.eq(u8x32::splat(b'"'))).bitmask(),
                unescaped_bits: (v.le(u8x32::splat(0x1f))).bitmask(),
            };
            if block.has_quote_first() {
                while **src != b'"' {
                    *dst = **src;
                    dst = dst.add(1);
                    *src = src.add(1);
                }
                *src = src.add(1); // skip ending quote
                return Ok(dst.offset_from(sdst) as usize);
            }
            if block.has_unesacped() {
                return Err(ControlCharacterWhileParsingString);
            }
            if !block.has_backslash() {
                let chunck = from_raw_parts_mut(dst, LANS);
                v.write_to_slice_unaligned_unchecked(chunck);
                *src = src.add(LANS);
                dst = dst.add(LANS);
                continue 'find_and_move;
            }
            // TODO: loop unrooling here
            while **src != b'\\' {
                *dst = **src;
                dst = dst.add(1);
                *src = src.add(1);
            }
            break 'find_and_move;
        }

        'esacpe: loop {
            let escaped_char: u8 = *src.add(1);
            if escaped_char == b'u' {
                if !handle_unicode_codepoint(src, &mut dst) {
                    return Err(InvalidUnicodeCodePoint);
                }
            } else {
                *dst = ESCAPED_TAB[escaped_char as usize];
                if *dst == 0 {
                    return Err(InvalidEscape);
                }
                *src = src.add(2);
                dst = dst.add(1);
            }

            // fast path for continous escaped chars
            if **src == b'\\' {
                continue 'esacpe;
            }
            break 'esacpe;
        }
    } // slow loop for escaped chars
}

// A very slow methods to parse escaped JSON strings
// json is always a format-well escaped string.
#[inline(always)]
pub(crate) fn parse_valid_escaped_string(
    json: &[u8],
    buf: &mut Vec<u8>,
) -> std::result::Result<usize, ErrorCode> {
    buf.reserve(json.len() + 32);
    unsafe {
        let mut src = json.as_ptr();
        let dst = buf.as_mut_ptr().add(buf.len());
        let new_len = parse_string_unchecked(&mut src, dst)?;
        buf.set_len(new_len);
        let cnt = src.offset_from(json.as_ptr()) as usize;
        Ok(cnt)
    }
}

pub const QUOTE_TAB: [(u8, [u8; 8]); 256] = [
    // 0x00 ~ 0x1f
    (6, *b"\\u0000\0\0"),
    (6, *b"\\u0001\0\0"),
    (6, *b"\\u0002\0\0"),
    (6, *b"\\u0003\0\0"),
    (6, *b"\\u0004\0\0"),
    (6, *b"\\u0005\0\0"),
    (6, *b"\\u0006\0\0"),
    (6, *b"\\u0007\0\0"),
    (2, *b"\\b\0\0\0\0\0\0"),
    (2, *b"\\t\0\0\0\0\0\0"),
    (2, *b"\\n\0\0\0\0\0\0"),
    (6, *b"\\u000b\0\0"),
    (2, *b"\\f\0\0\0\0\0\0"),
    (2, *b"\\r\0\0\0\0\0\0"),
    (6, *b"\\u000e\0\0"),
    (6, *b"\\u000f\0\0"),
    (6, *b"\\u0010\0\0"),
    (6, *b"\\u0011\0\0"),
    (6, *b"\\u0012\0\0"),
    (6, *b"\\u0013\0\0"),
    (6, *b"\\u0014\0\0"),
    (6, *b"\\u0015\0\0"),
    (6, *b"\\u0016\0\0"),
    (6, *b"\\u0017\0\0"),
    (6, *b"\\u0018\0\0"),
    (6, *b"\\u0019\0\0"),
    (6, *b"\\u001a\0\0"),
    (6, *b"\\u001b\0\0"),
    (6, *b"\\u001c\0\0"),
    (6, *b"\\u001d\0\0"),
    (6, *b"\\u001e\0\0"),
    (6, *b"\\u001f\0\0"),
    // 0x20 ~ 0x2f
    (0, [0; 8]),
    (0, [0; 8]),
    (2, *b"\\\"\0\0\0\0\0\0"),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    // 0x30 ~ 0x3f
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    // 0x40 ~ 0x4f
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    // 0x50 ~ 0x5f
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (2, *b"\\\\\0\0\0\0\0\0"),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    // 0x60 ~ 0xff
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
    (0, [0; 8]),
];

const NEED_ESCAPED: [u8; 256] = [
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

#[inline]
fn escaped_mask(v: u8x32) -> u32 {
    let _x20 = u8x32::splat(32); // 0x00 ~ 0x20
    let blash = u8x32::splat(b'\\');
    let quote = u8x32::splat(b'"');
    (v.lt(_x20) | v.eq(blash) | v.eq(quote)).bitmask()
}

// only check the src length.
#[inline(always)]
unsafe fn escape_unchecked(src: &mut *const u8, nb: &mut usize, dst: &mut *mut u8) {
    assert!(*nb >= 1);
    loop {
        let ch = *(*src);
        let cnt = QUOTE_TAB[ch as usize].0 as usize;
        assert!(
            cnt != 0,
            "char is {}, cnt is {},  NEED_ESCAPED is {}",
            ch as char,
            cnt,
            NEED_ESCAPED[ch as usize]
        );
        std::ptr::copy_nonoverlapping(QUOTE_TAB[ch as usize].1.as_ptr(), *dst, 8);
        (*dst) = (*dst).add(cnt);
        (*src) = (*src).add(1);
        (*nb) -= 1;
        if (*nb) == 0 || NEED_ESCAPED[*(*src) as usize] == 0 {
            return;
        }
    }
}

fn cross_page(ptr: *const u8, step: usize) -> bool {
    ((ptr as usize & 4095) + step) > 4096
}

pub fn quote(value: &str, dst: &mut [MaybeUninit<u8>]) -> usize {
    assert!(dst.len() >= value.len() * 6 + 32 + 3);
    const LANS: usize = 32;
    unsafe {
        let slice = value.as_bytes();
        let mut sptr = slice.as_ptr();
        let mut dptr = dst.as_mut_ptr() as *mut u8;
        let dstart = dptr;
        let mut nb: usize = slice.len();

        *dptr = b'"';
        dptr = dptr.add(1);
        while nb >= LANS {
            let v = {
                let raw = std::slice::from_raw_parts(sptr, LANS);
                u8x32::from_slice_unaligned_unchecked(raw)
            };
            v.write_to_slice_unaligned_unchecked(std::slice::from_raw_parts_mut(dptr, LANS));
            let mask = escaped_mask(v);
            if mask != 0 {
                let cn = mask.trailing_zeros() as usize;
                nb -= cn;
                dptr = dptr.add(cn);
                sptr = sptr.add(cn);
                escape_unchecked(&mut sptr, &mut nb, &mut dptr);
            } else {
                nb -= LANS;
                dptr = dptr.add(LANS);
                sptr = sptr.add(LANS);
            }
        }

        let mut temp: [u8; 64] = [0u8; 64];
        while nb > 0 {
            let v = if cross_page(sptr, LANS) {
                std::ptr::copy_nonoverlapping(sptr, temp[..].as_mut_ptr(), nb);
                u8x32::from_slice_unaligned_unchecked(&temp[..])
            } else {
                let raw = std::slice::from_raw_parts(sptr, LANS);
                u8x32::from_slice_unaligned_unchecked(raw)
            };
            v.write_to_slice_unaligned_unchecked(std::slice::from_raw_parts_mut(dptr, LANS));

            let mask = escaped_mask(v) & (0xFFFFFFFFu32 >> (LANS - nb));
            if mask != 0 {
                let cn = mask.trailing_zeros() as usize;
                nb -= cn;
                dptr = dptr.add(cn);
                sptr = sptr.add(cn);
                escape_unchecked(&mut sptr, &mut nb, &mut dptr);
            } else {
                dptr = dptr.add(nb);
                break;
            }
        }
        *dptr = b'"';
        dptr = dptr.add(1);
        dptr as usize - dstart as usize
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_quote() {
        let mut dst = [0u8; 1000];
        let dst_ref = unsafe { std::mem::transmute::<&mut [u8], &mut [MaybeUninit<u8>]>(&mut dst) };
        assert_eq!(quote("", dst_ref), 2);
        assert_eq!(dst[..2], *b"\"\"");
        assert_eq!(quote("\x00", dst_ref), 8);
        assert_eq!(dst[..8], *b"\"\\u0000\"");
        assert_eq!(quote("test", dst_ref), 6);
        assert_eq!(dst[..6], *b"\"test\"");
        assert_eq!(quote("test\"test", dst_ref), 12);
        assert_eq!(dst[..12], *b"\"test\\\"test\"");
        assert_eq!(quote("\\testtest\"", dst_ref), 14);
        assert_eq!(dst[..14], *b"\"\\\\testtest\\\"\"");

        let long_str = "this is a long string that should be \\\"quoted and escaped multiple times to test the performance and correctness of the function.";
        assert_eq!(quote(long_str, dst_ref), 129 + 4);
        assert_eq!(dst[..133], *b"\"this is a long string that should be \\\\\\\"quoted and escaped multiple times to test the performance and correctness of the function.\"");

        // TODO: add cross page test
    }
}
