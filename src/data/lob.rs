use crate::domain::{BookDelta, BookSnapshot, BookTop, Fixed, Level, Side, Symbol};
use std::collections::VecDeque;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Empty,
    Occupied,
    Deleted,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    level: Level,
    state: SlotState,
}

impl Slot {
    const EMPTY: Self = Self {
        level: Level::EMPTY,
        state: SlotState::Empty,
    };
}

/// Fixed-capacity, open-addressed price ladder. Updates are allocation-free and
/// O(1) on average; top-of-book extraction scans contiguous cache-friendly memory.
#[derive(Debug)]
struct PriceTable<const N: usize> {
    slots: [Slot; N],
    len: usize,
}

impl<const N: usize> PriceTable<N> {
    fn new() -> Self {
        assert!(N > 0, "price table capacity must be non-zero");
        Self {
            slots: [Slot::EMPTY; N],
            len: 0,
        }
    }

    fn clear(&mut self) {
        self.slots.fill(Slot::EMPTY);
        self.len = 0;
    }

    fn hash(price: Fixed) -> usize {
        let mut value = price.0 as u64;
        value ^= value >> 33;
        value = value.wrapping_mul(0xff51_afd7_ed55_8ccd);
        value ^= value >> 33;
        value as usize
    }

    fn update(&mut self, level: Level) -> Result<(), BookError> {
        let start = Self::hash(level.price) % N;
        let mut first_deleted = None;
        for offset in 0..N {
            let index = (start + offset) % N;
            match self.slots[index].state {
                SlotState::Occupied if self.slots[index].level.price == level.price => {
                    if level.quantity.0 == 0 {
                        self.slots[index].state = SlotState::Deleted;
                        self.slots[index].level = Level::EMPTY;
                        self.len -= 1;
                    } else {
                        self.slots[index].level.quantity = level.quantity;
                    }
                    return Ok(());
                }
                SlotState::Deleted => {
                    first_deleted.get_or_insert(index);
                }
                SlotState::Empty => {
                    if level.quantity.0 == 0 {
                        return Ok(());
                    }
                    let target = first_deleted.unwrap_or(index);
                    self.slots[target] = Slot {
                        level,
                        state: SlotState::Occupied,
                    };
                    self.len += 1;
                    return Ok(());
                }
                SlotState::Occupied => {}
            }
        }
        if level.quantity.0 == 0 {
            return Ok(());
        }
        if let Some(target) = first_deleted {
            self.slots[target] = Slot {
                level,
                state: SlotState::Occupied,
            };
            self.len += 1;
            Ok(())
        } else {
            Err(BookError::CapacityExceeded)
        }
    }

    fn top<const M: usize>(&self, side: Side) -> ([Level; M], usize) {
        let mut top = [Level::EMPTY; M];
        let mut count = 0_usize;
        for slot in &self.slots {
            if slot.state != SlotState::Occupied || M == 0 {
                continue;
            }
            let level = slot.level;
            let mut position = count.min(M);
            for (index, existing) in top.iter().take(count.min(M)).enumerate() {
                let better = match side {
                    Side::Buy => level.price > existing.price,
                    Side::Sell => level.price < existing.price,
                };
                if better {
                    position = index;
                    break;
                }
            }
            if position >= M {
                continue;
            }
            let upper = count.min(M.saturating_sub(1));
            for index in (position..upper).rev() {
                top[index + 1] = top[index];
            }
            top[position] = level;
            count = (count + 1).min(M);
        }
        (top, count)
    }
}

#[derive(Debug)]
pub struct LocalOrderBook<const N: usize = 2_048> {
    symbol: Symbol,
    bids: PriceTable<N>,
    asks: PriceTable<N>,
    last_update_id: u64,
    timestamp_ns: u64,
}

impl<const N: usize> LocalOrderBook<N> {
    #[must_use]
    pub fn new(symbol: Symbol) -> Self {
        Self {
            symbol,
            bids: PriceTable::new(),
            asks: PriceTable::new(),
            last_update_id: 0,
            timestamp_ns: 0,
        }
    }

    pub fn clear(&mut self) {
        self.bids.clear();
        self.asks.clear();
        self.last_update_id = 0;
        self.timestamp_ns = 0;
    }

    pub fn install_snapshot(&mut self, snapshot: &BookSnapshot) -> Result<(), BookError> {
        if snapshot.symbol != self.symbol {
            return Err(BookError::WrongSymbol);
        }
        self.clear();
        for level in &snapshot.bids {
            self.bids.update(*level)?;
        }
        for level in &snapshot.asks {
            self.asks.update(*level)?;
        }
        self.last_update_id = snapshot.last_update_id;
        Ok(())
    }

    pub fn apply_delta(&mut self, delta: &BookDelta) -> Result<(), BookError> {
        if delta.symbol != self.symbol {
            return Err(BookError::WrongSymbol);
        }
        if self.last_update_id != 0 && delta.previous_final_update_id != self.last_update_id {
            return Err(BookError::SequenceGap {
                expected_previous: self.last_update_id,
                actual_previous: delta.previous_final_update_id,
            });
        }
        for level in &delta.bids {
            self.bids.update(*level)?;
        }
        for level in &delta.asks {
            self.asks.update(*level)?;
        }
        self.last_update_id = delta.final_update_id;
        self.timestamp_ns = delta.event_time_ns;
        Ok(())
    }

    #[must_use]
    pub fn top<const M: usize>(&self) -> BookTop<M> {
        let (bids, bid_count) = self.bids.top(Side::Buy);
        let (asks, ask_count) = self.asks.top(Side::Sell);
        BookTop {
            symbol: self.symbol,
            timestamp_ns: self.timestamp_ns,
            bids,
            asks,
            bid_count,
            ask_count,
        }
    }

    #[must_use]
    pub const fn last_update_id(&self) -> u64 {
        self.last_update_id
    }

    #[must_use]
    pub const fn symbol(&self) -> Symbol {
        self.symbol
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum BookError {
    #[error("market event belongs to another symbol")]
    WrongSymbol,
    #[error("fixed order-book capacity exceeded")]
    CapacityExceeded,
    #[error("sequence gap: expected pu={expected_previous}, received pu={actual_previous}")]
    SequenceGap {
        expected_previous: u64,
        actual_previous: u64,
    },
    #[error("snapshot buffer overflow; a fresh snapshot is required")]
    BufferOverflow,
    #[error("buffered stream does not bridge the REST snapshot")]
    SnapshotBridgeMissing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncState {
    AwaitingSnapshot,
    Synchronized,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    Buffered,
    Applied,
    IgnoredOld,
    ResyncRequired,
}

/// Enforces the Binance snapshot/buffer/bridge/`pu` synchronization protocol.
#[derive(Debug)]
pub struct LobSynchronizer<const LEVELS: usize = 2_048> {
    state: SyncState,
    book: LocalOrderBook<LEVELS>,
    buffer: VecDeque<BookDelta>,
    buffer_capacity: usize,
}

impl<const LEVELS: usize> LobSynchronizer<LEVELS> {
    #[must_use]
    pub fn new(symbol: Symbol, buffer_capacity: usize) -> Self {
        assert!(buffer_capacity > 0, "snapshot buffer must be non-zero");
        Self {
            state: SyncState::AwaitingSnapshot,
            book: LocalOrderBook::new(symbol),
            buffer: VecDeque::with_capacity(buffer_capacity),
            buffer_capacity,
        }
    }

    pub fn on_delta(&mut self, delta: BookDelta) -> Result<SyncOutcome, BookError> {
        match self.state {
            SyncState::AwaitingSnapshot => {
                if self.buffer.len() == self.buffer_capacity {
                    self.state = SyncState::Stale;
                    self.buffer.clear();
                    return Err(BookError::BufferOverflow);
                }
                self.buffer.push_back(delta);
                Ok(SyncOutcome::Buffered)
            }
            SyncState::Synchronized => match self.book.apply_delta(&delta) {
                Ok(()) => Ok(SyncOutcome::Applied),
                Err(BookError::SequenceGap { .. }) => {
                    self.invalidate();
                    Ok(SyncOutcome::ResyncRequired)
                }
                Err(error) => Err(error),
            },
            SyncState::Stale => Ok(SyncOutcome::ResyncRequired),
        }
    }

    pub fn install_snapshot(&mut self, snapshot: &BookSnapshot) -> Result<(), BookError> {
        self.book.install_snapshot(snapshot)?;
        while self
            .buffer
            .front()
            .is_some_and(|delta| delta.final_update_id < snapshot.last_update_id + 1)
        {
            self.buffer.pop_front();
        }
        let Some(first) = self.buffer.pop_front() else {
            self.state = SyncState::AwaitingSnapshot;
            return Err(BookError::SnapshotBridgeMissing);
        };
        let bridge_id = snapshot.last_update_id.saturating_add(1);
        if first.first_update_id > bridge_id || first.final_update_id < bridge_id {
            self.invalidate();
            return Err(BookError::SnapshotBridgeMissing);
        }

        // The first bridged event is special: its pu may precede the snapshot.
        for level in &first.bids {
            self.book.bids.update(*level)?;
        }
        for level in &first.asks {
            self.book.asks.update(*level)?;
        }
        self.book.last_update_id = first.final_update_id;
        self.book.timestamp_ns = first.event_time_ns;

        while let Some(delta) = self.buffer.pop_front() {
            if let Err(error) = self.book.apply_delta(&delta) {
                self.invalidate();
                return Err(error);
            }
        }
        self.state = SyncState::Synchronized;
        Ok(())
    }

    pub fn begin_resync(&mut self) {
        self.book.clear();
        self.buffer.clear();
        self.state = SyncState::AwaitingSnapshot;
    }

    fn invalidate(&mut self) {
        self.book.clear();
        self.buffer.clear();
        self.state = SyncState::Stale;
    }

    #[must_use]
    pub const fn state(&self) -> SyncState {
        self.state
    }

    #[must_use]
    pub const fn book(&self) -> &LocalOrderBook<LEVELS> {
        &self.book
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrayvec::ArrayVec;

    fn level(price: i64, quantity: i64) -> Level {
        Level {
            price: Fixed::from_raw(price),
            quantity: Fixed::from_raw(quantity),
        }
    }

    fn delta(symbol: Symbol, first: u64, last: u64, previous: u64) -> BookDelta {
        BookDelta {
            symbol,
            event_time_ns: last * 1_000,
            first_update_id: first,
            final_update_id: last,
            previous_final_update_id: previous,
            bids: ArrayVec::from_iter([level(100, 3)]),
            asks: ArrayVec::from_iter([level(101, 4)]),
        }
    }

    #[test]
    fn snapshot_bridge_and_gap_detection() {
        let symbol = Symbol::new("BTCUSDT").expect("symbol");
        let mut sync = LobSynchronizer::<32>::new(symbol, 8);
        sync.on_delta(delta(symbol, 10, 12, 9)).expect("buffer");
        sync.install_snapshot(&BookSnapshot {
            symbol,
            last_update_id: 10,
            bids: vec![],
            asks: vec![],
        })
        .expect("bridge");
        assert_eq!(sync.state(), SyncState::Synchronized);
        assert_eq!(sync.book().last_update_id(), 12);

        let outcome = sync.on_delta(delta(symbol, 14, 14, 13)).expect("outcome");
        assert_eq!(outcome, SyncOutcome::ResyncRequired);
        assert_eq!(sync.state(), SyncState::Stale);
    }

    #[test]
    fn top_levels_are_sorted() {
        let symbol = Symbol::new("ETHUSDT").expect("symbol");
        let mut book = LocalOrderBook::<16>::new(symbol);
        book.install_snapshot(&BookSnapshot {
            symbol,
            last_update_id: 1,
            bids: vec![level(99, 1), level(101, 1), level(100, 1)],
            asks: vec![level(104, 1), level(102, 1), level(103, 1)],
        })
        .expect("snapshot");
        let top = book.top::<2>();
        assert_eq!(top.bids[0].price.0, 101);
        assert_eq!(top.bids[1].price.0, 100);
        assert_eq!(top.asks[0].price.0, 102);
        assert_eq!(top.asks[1].price.0, 103);
    }
}
