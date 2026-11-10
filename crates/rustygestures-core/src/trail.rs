//! Lock-free snapshot of the stroke currently being drawn.
//!
//! The hook thread publishes. The overlay thread reads. A seqlock keeps the
//! publisher from ever waiting on the reader.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::{Point, MAX_STROKE_POINTS};

/// Latest in-progress stroke, or empty when nothing is captured.
pub struct LiveTrail {
    seq: AtomicU64,
    len: AtomicU32,
    points: UnsafeCell<[Point; MAX_STROKE_POINTS]>,
}

// SAFETY: `publish` is the only writer. `read` retries unless it observed a
// stable even sequence around its copy, so it never returns a torn snapshot
// and it never writes.
unsafe impl Send for LiveTrail {}
unsafe impl Sync for LiveTrail {}

impl Default for LiveTrail {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveTrail {
    #[must_use]
    pub fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            len: AtomicU32::new(0),
            points: UnsafeCell::new([Point { x: 0, y: 0 }; MAX_STROKE_POINTS]),
        }
    }

    /// Replace the published stroke. `points` longer than the buffer are truncated.
    pub fn publish(&self, points: &[Point]) {
        let n = points.len().min(MAX_STROKE_POINTS);
        let seq = self.seq.load(Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Release);
        // SAFETY: this thread is the only writer. Readers copy only while the
        // sequence is even, and retry if it changes.
        unsafe {
            let dst = &mut *self.points.get();
            dst[..n].copy_from_slice(&points[..n]);
        }
        self.len.store(n as u32, Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Release);
        self.seq.store(seq.wrapping_add(2), Ordering::Release);
    }

    /// Copy the latest stable snapshot into `out`. Returns the point count.
    /// Returns 0 if a writer kept the snapshot unstable for the whole spin.
    pub fn read(&self, out: &mut [Point; MAX_STROKE_POINTS]) -> usize {
        for _ in 0..64 {
            let start = self.seq.load(Ordering::Acquire);
            if start & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            std::sync::atomic::fence(Ordering::Acquire);
            let n = (self.len.load(Ordering::Relaxed) as usize).min(MAX_STROKE_POINTS);
            // SAFETY: the sequence was even, so a writer is not in the middle
            // of a publish that we observed. We still re-check the sequence
            // and discard the copy if it moved.
            unsafe {
                let src = &*self.points.get();
                out[..n].copy_from_slice(&src[..n]);
            }
            std::sync::atomic::fence(Ordering::Acquire);
            let end = self.seq.load(Ordering::Acquire);
            if start == end {
                return n;
            }
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn publish_then_read_round_trips() {
        let trail = LiveTrail::new();
        let points = [
            Point { x: 1, y: 2 },
            Point { x: 3, y: 4 },
            Point { x: 5, y: 6 },
        ];
        trail.publish(&points);
        let mut out = [Point { x: 0, y: 0 }; MAX_STROKE_POINTS];
        let n = trail.read(&mut out);
        assert_eq!(n, 3);
        assert_eq!(&out[..3], &points);
        trail.publish(&[]);
        assert_eq!(trail.read(&mut out), 0);
    }

    #[test]
    fn concurrent_reader_sees_complete_prefixes() {
        let trail = Arc::new(LiveTrail::new());
        let writer = {
            let trail = Arc::clone(&trail);
            thread::spawn(move || {
                for n in 1..=200 {
                    let points: Vec<Point> = (0..n).map(|i| Point { x: n, y: i }).collect();
                    trail.publish(&points);
                }
            })
        };
        let reader = {
            let trail = Arc::clone(&trail);
            thread::spawn(move || {
                let mut out = [Point { x: 0, y: 0 }; MAX_STROKE_POINTS];
                for _ in 0..200 {
                    let n = trail.read(&mut out);
                    if n > 0 {
                        let marker = out[0].x;
                        assert_eq!(n as i32, marker);
                        for (i, point) in out.iter().take(n).enumerate() {
                            assert_eq!(point.y, i as i32);
                        }
                    }
                }
            })
        };
        writer.join().unwrap();
        reader.join().unwrap();
    }
}
