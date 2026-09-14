//! Bounded byte buffer addressed by absolute offsets (bytes since the stream started).

use std::collections::VecDeque;

pub struct Ring {
    buf: VecDeque<u8>,
    cap: usize,
    /// Absolute offset of `buf[0]`.
    start: u64,
}

impl Ring {
    pub fn new(cap: usize) -> Ring {
        Ring { buf: VecDeque::new(), cap, start: 0 }
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend(data);
        if self.buf.len() > self.cap {
            let excess = self.buf.len() - self.cap;
            self.buf.drain(..excess);
            self.start += excess as u64;
        }
    }

    /// Offset of the oldest retained byte.
    pub fn start(&self) -> u64 {
        self.start
    }

    /// Offset one past the newest byte.
    pub fn end(&self) -> u64 {
        self.start + self.buf.len() as u64
    }

    /// Up to `max` bytes from `from` (clamped to the retained range): (offset of the first byte, bytes).
    pub fn read(&self, from: u64, max: usize) -> (u64, Vec<u8>) {
        let from = from.clamp(self.start, self.end());
        let i = (from - self.start) as usize;
        let n = (self.buf.len() - i).min(max);
        (from, self.buf.range(i..i + n).copied().collect())
    }

    /// The last `lines` lines, at most `max` bytes: (offset of the first byte, bytes).
    pub fn tail(&self, lines: usize, max: usize) -> (u64, Vec<u8>) {
        let len = self.buf.len();
        if lines == 0 {
            return (self.end(), Vec::new());
        }
        let lo = len.saturating_sub(max);
        let mut j = len;
        if j > lo && self.buf[j - 1] == b'\n' {
            j -= 1;
        }
        let mut seen = 0;
        while j > lo {
            if self.buf[j - 1] == b'\n' {
                seen += 1;
                if seen == lines {
                    break;
                }
            }
            j -= 1;
        }
        (self.start + j as u64, self.buf.range(j..len).copied().collect())
    }

    /// Absolute offset of the first occurrence of `needle` at or after `from`.
    pub fn find(&self, from: u64, needle: &[u8], ignore_case: bool) -> Option<u64> {
        let from = from.clamp(self.start, self.end());
        let i = (from - self.start) as usize;
        let hay: Vec<u8> = self.buf.range(i..).copied().collect();
        find_bytes(&hay, needle, ignore_case).map(|p| from + p as u64)
    }
}

pub fn find_bytes(hay: &[u8], needle: &[u8], ignore_case: bool) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > hay.len() {
        return None;
    }
    if ignore_case {
        let (h, n) = (hay.to_ascii_lowercase(), needle.to_ascii_lowercase());
        h.windows(n.len()).position(|w| w == n.as_slice())
    } else {
        hay.windows(needle.len()).position(|w| w == needle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_survive_overflow() {
        let mut r = Ring::new(8);
        r.push(b"0123456789");
        assert_eq!((r.start(), r.end()), (2, 10));
        assert_eq!(r.read(0, 100), (2, b"23456789".to_vec()), "clamped to retained data");
        assert_eq!(r.read(5, 2), (5, b"56".to_vec()));
        assert_eq!(r.read(99, 2), (10, Vec::new()));
        assert_eq!(r.find(0, b"78", false), Some(7));
        assert_eq!(r.find(8, b"78", false), None);
    }

    #[test]
    fn tail_counts_lines() {
        let mut r = Ring::new(1 << 10);
        r.push(b"a\nbb\nccc\n");
        assert_eq!(r.tail(2, 1000), (2, b"bb\nccc\n".to_vec()));
        assert_eq!(r.tail(9, 1000), (0, b"a\nbb\nccc\n".to_vec()));
        assert_eq!(r.tail(1, 3), (6, b"cc\n".to_vec()), "byte cap wins");
        assert_eq!(r.tail(0, 1000), (9, Vec::new()));
        r.push(b"partial");
        assert_eq!(r.tail(1, 1000), (9, b"partial".to_vec()));
    }

    #[test]
    fn find_ignores_case_on_request() {
        assert_eq!(find_bytes(b"Flash Disk", b"flash", false), None);
        assert_eq!(find_bytes(b"Flash Disk", b"DISK", true), Some(6));
    }
}
