use crossbeam_queue::ArrayQueue;
use rtrb::RingBuffer;
use std::sync::Arc;

/// The deterministic SPSC path used when producer and consumer have equal cadence.
pub type SpscProducer<T> = rtrb::Producer<T>;
pub type SpscConsumer<T> = rtrb::Consumer<T>;

#[must_use]
pub fn spsc_bus<T>(capacity: usize) -> (SpscProducer<T>, SpscConsumer<T>) {
    RingBuffer::new(capacity)
}

/// Bounded lock-free queue for bursty market data.
///
/// `force_push` implements the documented latest-wins policy: the oldest event
/// is returned to the caller whenever the fixed capacity is exhausted.
pub struct LatestEventBus<T> {
    queue: Arc<ArrayQueue<T>>,
}

impl<T> Clone for LatestEventBus<T> {
    fn clone(&self) -> Self {
        Self {
            queue: Arc::clone(&self.queue),
        }
    }
}

impl<T> LatestEventBus<T> {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "event bus capacity must be non-zero");
        Self {
            queue: Arc::new(ArrayQueue::new(capacity)),
        }
    }

    /// Pushes an event and returns the evicted oldest event, if any.
    pub fn push_latest(&self, event: T) -> Option<T> {
        self.queue.force_push(event)
    }

    pub fn pop(&self) -> Option<T> {
        self.queue.pop()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.queue.capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_event_wins_on_overflow() {
        let bus = LatestEventBus::new(2);
        assert_eq!(bus.push_latest(1), None);
        assert_eq!(bus.push_latest(2), None);
        assert_eq!(bus.push_latest(3), Some(1));
        assert_eq!(bus.pop(), Some(2));
        assert_eq!(bus.pop(), Some(3));
    }
}
