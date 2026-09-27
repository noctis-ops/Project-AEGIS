use std::time::Instant;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestClass {
    Entry,
    Management,
    Emergency,
}

#[derive(Debug)]
pub struct TokenBucket {
    capacity: f64,
    emergency_reserve: f64,
    tokens: f64,
    refill_per_second: f64,
    last_refill: Instant,
}

impl TokenBucket {
    #[must_use]
    pub fn new(capacity: u32, refill_per_second: f64, emergency_reserve: u32) -> Self {
        assert!(
            capacity > emergency_reserve,
            "reserve must be below capacity"
        );
        Self {
            capacity: f64::from(capacity),
            emergency_reserve: f64::from(emergency_reserve),
            tokens: f64::from(capacity),
            refill_per_second,
            last_refill: Instant::now(),
        }
    }

    pub fn acquire(&mut self, weight: u32, class: RequestClass) -> Result<(), RateLimitError> {
        self.refill();
        let weight = f64::from(weight);
        let floor = if class == RequestClass::Emergency {
            0.0
        } else {
            self.emergency_reserve
        };
        if self.tokens - weight < floor {
            return Err(RateLimitError::Exhausted);
        }
        self.tokens -= weight;
        Ok(())
    }

    pub fn synchronize_used(&mut self, used: u32) {
        self.tokens = (self.capacity - f64::from(used)).clamp(0.0, self.capacity);
        self.last_refill = Instant::now();
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_second).min(self.capacity);
        self.last_refill = now;
    }

    #[must_use]
    pub fn remaining_fraction(&mut self) -> f64 {
        self.refill();
        self.tokens / self.capacity
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitError {
    #[error("local rate limit exhausted; emergency reserve preserved")]
    Exhausted,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_cannot_consume_emergency_reserve() {
        let mut bucket = TokenBucket::new(10, 0.0, 2);
        assert!(bucket.acquire(8, RequestClass::Entry).is_ok());
        assert_eq!(
            bucket.acquire(1, RequestClass::Entry),
            Err(RateLimitError::Exhausted)
        );
        assert!(bucket.acquire(2, RequestClass::Emergency).is_ok());
    }
}
