//! The second crossing of the audio↔control boundary: per-block cost samples over
//! a preallocated `rtrb` SPSC ring.
//!
//! ## Why a ring and not just atomics
//!
//! [`crate::counters`] answers "how many, how bad" — sums and extrema. It cannot
//! answer "what was the distribution": a mean of 8 µs is compatible with a stable
//! 8 µs and with 7.9 µs plus a 2 ms spike every tenth block, and only the second one
//! clicks. The ring carries every sample so the control thread can sort them and
//! take a real median/p99 ([`crate::cost_summary`]).
//!
//! The producer moves into the cpal closure; the consumer lives on the control
//! thread. That is exactly the CONVENTION §2.3 topology: a lock-free SPSC queue, no
//! `Mutex` on the audio path.
//!
//! ## The overflow policy, stated
//!
//! A full ring **loses the sample and counts the loss**. It never blocks, never
//! grows, and the audio thread does nothing more expensive than an atomic add. If
//! the control thread stops draining (a suspended debugger, a descheduled main
//! thread), the honest report says `ring_drops = N` instead of stalling the driver.

use std::sync::Arc;

use rtrb::{Consumer, RingBuffer};

use crate::counters::Counters;

/// Ring capacity: 16,384 samples ≈ 22 s of 64-frame blocks at 48 kHz without the
/// control thread draining at all (750 blocks/s). Chosen as a power of two so the
/// queue's index wrap is free.
pub const DEFAULT_CAPACITY: usize = 16_384;

/// The producer half. Moved into the callback.
pub struct BlockCostRing {
    producer: rtrb::Producer<u64>,
    counters: Arc<Counters>,
}

impl BlockCostRing {
    /// Builds the pair with an explicit capacity. `Pump::setup` passes
    /// [`DEFAULT_CAPACITY`]; the tests use tiny rings to reach the drop path.
    ///
    /// LEGAL ALLOCATION: setup thread — `RingBuffer::new` is the ring's one and only
    /// reservation, and it happens before the stream exists.
    pub fn with_capacity(capacity: usize, counters: Arc<Counters>) -> (Self, Consumer<u64>) {
        let (producer, consumer) = RingBuffer::new(capacity.max(1));
        (Self { producer, counters }, consumer)
    }

    /// Publishes one chunk cost in ns. The only thing the audio thread does here is
    /// a bounded write; `push` on a full ring returns the value back and we count it.
    pub fn publish(&mut self, cost_ns: u64) {
        if self.producer.push(cost_ns).is_err() {
            self.counters.note_ring_drop();
        }
    }

    /// The ring's declared capacity.
    pub fn capacity(&self) -> usize {
        self.producer.buffer().capacity()
    }
}

/// The consumer half plus the control thread's accumulator.
pub struct TimingDrain {
    consumer: Consumer<u64>,
    samples: Vec<u64>,
}

impl TimingDrain {
    /// Wraps a consumer with room for `hint` samples (LEGITIMATE ALLOCATION: control
    /// thread, before the run; the Vec may still grow if the run outlives the hint).
    pub fn new(consumer: Consumer<u64>, hint: usize) -> Self {
        Self {
            consumer,
            samples: Vec::with_capacity(hint),
        }
    }

    /// Moves everything currently in the ring into the sample vector. Returns how many
    /// were moved. Called from the control thread only, so the growth is legal.
    pub fn drain(&mut self) -> usize {
        let before = self.samples.len();
        while let Ok(v) = self.consumer.pop() {
            self.samples.push(v);
        }
        self.samples.len() - before
    }

    /// Number of samples held.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// True when nothing has been drained yet.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Consumes the drain and hands the samples to the report.
    pub fn into_samples(self) -> Vec<u64> {
        self.samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_cross_the_ring_in_order() {
        let c = Arc::new(Counters::new());
        let (mut ring, consumer) = BlockCostRing::with_capacity(8, Arc::clone(&c));
        for ns in [10u64, 20, 30] {
            ring.publish(ns);
        }
        let mut drain = TimingDrain::new(consumer, 8);
        assert_eq!(drain.drain(), 3, "three samples moved");
        assert_eq!(
            drain.into_samples(),
            vec![10, 20, 30],
            "FIFO, nothing reordered"
        );

        assert_eq!(c.view().ring_drops, 0, "a ring with room loses nothing");
    }

    #[test]
    fn a_full_ring_drops_samples_and_counts_them_exactly() {
        let c = Arc::new(Counters::new());
        let (mut ring, consumer) = BlockCostRing::with_capacity(4, Arc::clone(&c));
        assert_eq!(ring.capacity(), 4);
        for ns in 0..10u64 {
            ring.publish(ns);
        }
        let mut drain = TimingDrain::new(consumer, 8);
        let got = drain.drain();
        assert_eq!(got, 4, "capacity is the truth: 4 accepted, 6 refused");
        assert_eq!(
            c.view().ring_drops,
            6,
            "every refused sample is one counted drop, not a silent loss"
        );
        assert_eq!(
            drain.into_samples(),
            vec![0, 1, 2, 3],
            "the oldest four survive"
        );
    }

    #[test]
    fn draining_an_empty_ring_is_a_no_op() {
        let c = Arc::new(Counters::new());
        let (ring, consumer) = BlockCostRing::with_capacity(4, Arc::clone(&c));
        drop(ring);
        let mut drain = TimingDrain::new(consumer, 0);
        assert_eq!(drain.drain(), 0);
        assert!(drain.is_empty());
        assert_eq!(c.view().ring_drops, 0);
    }

    #[test]
    fn drain_survives_more_samples_than_its_initial_hint() {
        let c = Arc::new(Counters::new());
        let (mut ring, consumer) = BlockCostRing::with_capacity(64, Arc::clone(&c));
        for ns in 0..50u64 {
            ring.publish(ns);
        }
        let mut drain = TimingDrain::new(consumer, 4);
        assert_eq!(
            drain.drain(),
            50,
            "the hint is a reservation, not a capacity limit"
        );
        assert_eq!(drain.len(), 50);
    }

    #[test]
    fn interleaved_publish_and_drain_keep_every_sample() {
        let c = Arc::new(Counters::new());
        let (mut ring, consumer) = BlockCostRing::with_capacity(4, Arc::clone(&c));
        let mut drain = TimingDrain::new(consumer, 16);
        for round in 0..20u64 {
            ring.publish(round);
            ring.publish(round + 100);
            drain.drain();
        }
        let s = drain.into_samples();
        assert_eq!(s.len(), 40, "no loss when the consumer keeps up");
        assert_eq!(c.view().ring_drops, 0);
        assert_eq!(s[0], 0);
        assert_eq!(s[39], 119, "the last published sample is the last drained");
    }

    #[test]
    fn the_producer_is_send_because_the_ring_holds_u64() {
        fn assert_send<T: Send>() {}
        assert_send::<BlockCostRing>();
        assert_send::<TimingDrain>();
    }
}
