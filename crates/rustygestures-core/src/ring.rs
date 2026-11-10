//! Fixed-capacity single-producer single-consumer queue.
//!
//! The hook thread pushes finished strokes. The engine thread pops them.
//! Push and pop do not allocate and do not lock.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::Stroke;

/// Slot count. Must be a power of two.
pub const RING_CAP: usize = 16;

/// Lock-free ring of [`Stroke`] values.
///
/// Only one thread may call [`SpscRing::push`], and only one other thread may
/// call [`SpscRing::pop`].
pub struct SpscRing {
    slots: [UnsafeCell<MaybeUninit<Stroke>>; RING_CAP],
    head: AtomicUsize,
    tail: AtomicUsize,
}

// SAFETY: the head and tail protocol gives the producer exclusive access to
// the slot it writes and the consumer exclusive access to the slot it reads.
unsafe impl Send for SpscRing {}
unsafe impl Sync for SpscRing {}

impl Default for SpscRing {
    fn default() -> Self {
        Self::new()
    }
}

impl SpscRing {
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| UnsafeCell::new(MaybeUninit::uninit())),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Push `stroke`. Returns `false` when the ring is full. The caller must
    /// not wait: the hook callback cannot block.
    pub fn push(&self, stroke: &Stroke) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail.wrapping_sub(head) >= RING_CAP {
            return false;
        }
        let index = tail & (RING_CAP - 1);
        // SAFETY: the slot is not occupied. The consumer will not read it
        // until the tail store below is released.
        unsafe {
            (*self.slots[index].get()).write(*stroke);
        }
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    #[must_use]
    pub fn pop(&self) -> Option<Stroke> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        let index = head & (RING_CAP - 1);
        // SAFETY: the producer has released this slot and will not write it
        // again until the head store below is visible.
        let stroke = unsafe { (*self.slots[index].get()).assume_init_read() };
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Some(stroke)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Relaxed);
        tail.wrapping_sub(head)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Button, Point, Stroke, MAX_STROKE_POINTS};
    use std::sync::Arc;
    use std::thread;

    fn stroke(len: u16, x: i32) -> Stroke {
        let mut points = [Point { x: 0, y: 0 }; MAX_STROKE_POINTS];
        for (i, point) in points.iter_mut().enumerate().take(len as usize) {
            *point = Point {
                x: x + i as i32,
                y: 1,
            };
        }
        Stroke {
            points,
            len,
            button: Button::Right,
        }
    }

    #[test]
    fn push_pop_preserves_order_and_wraps() {
        let ring = SpscRing::new();
        assert!(ring.is_empty());
        for i in 0..RING_CAP {
            assert!(ring.push(&stroke(2, i as i32)));
        }
        assert!(!ring.push(&stroke(2, 99)));
        assert_eq!(ring.len(), RING_CAP);
        let first = ring.pop().unwrap();
        assert_eq!(first.points[0].x, 0);
        assert!(ring.push(&stroke(2, 100)));
        for i in 1..RING_CAP {
            let item = ring.pop().unwrap();
            assert_eq!(item.points[0].x, i as i32);
        }
        let last = ring.pop().unwrap();
        assert_eq!(last.points[0].x, 100);
        assert!(ring.pop().is_none());
    }

    #[test]
    fn two_threads_move_every_stroke() {
        let ring = Arc::new(SpscRing::new());
        let producer = {
            let ring = Arc::clone(&ring);
            thread::spawn(move || {
                for i in 0..1000 {
                    let item = stroke(3, i);
                    while !ring.push(&item) {
                        thread::yield_now();
                    }
                }
            })
        };
        let consumer = {
            let ring = Arc::clone(&ring);
            thread::spawn(move || {
                let mut seen = 0;
                while seen < 1000 {
                    if let Some(item) = ring.pop() {
                        assert_eq!(item.points[0].x, seen);
                        seen += 1;
                    } else {
                        thread::yield_now();
                    }
                }
            })
        };
        producer.join().unwrap();
        consumer.join().unwrap();
        assert!(ring.is_empty());
    }
}
