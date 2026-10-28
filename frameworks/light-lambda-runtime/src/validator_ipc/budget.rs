//! Experimental aggregate reservations for controller-owned retained allocations.
//! Requested capacities only: not allocator metadata, RSS or foreign allocations.
use super::{Counter, FRAME, Failure};
use serde_json::Value;
use std::{
    mem::size_of,
    ops::Deref,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::SeqCst},
    },
};
#[derive(Debug)]
pub struct Budget {
    limit: usize,
    used: AtomicUsize,
    peak: AtomicUsize,
}
impl Budget {
    pub fn limit(&self) -> usize {
        self.limit
    }
    pub fn reservation(self: &Arc<Self>, bytes: usize) -> Result<Reservation, Failure> {
        self.reserve(bytes).map(Reservation)
    }
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        })
    }
    pub fn used(&self) -> usize {
        self.used.load(SeqCst)
    }
    pub fn peak(&self) -> usize {
        self.peak.load(SeqCst)
    }
    pub(crate) fn reserve(self: &Arc<Self>, bytes: usize) -> Result<Credit, Failure> {
        self.add(bytes)?;
        Ok(Credit {
            budget: self.clone(),
            bytes,
        })
    }
    fn add(&self, bytes: usize) -> Result<(), Failure> {
        let mut old = self.used.load(SeqCst);
        loop {
            let next = old
                .checked_add(bytes)
                .filter(|n| *n <= self.limit)
                .ok_or(Failure::ParentBudget)?;
            match self.used.compare_exchange(old, next, SeqCst, SeqCst) {
                Ok(_) => {
                    self.peak.fetch_max(next, SeqCst);
                    return Ok(());
                }
                Err(now) => old = now,
            }
        }
    }
}
#[derive(Debug)]
pub(crate) struct Credit {
    budget: Arc<Budget>,
    bytes: usize,
}
/// Public opaque ownership guard; data/control owners cannot refund by hand.
pub struct Reservation(Credit);
impl Reservation {
    pub fn bytes(&self) -> usize {
        self.0.bytes
    }
}
pub struct MetadataReservation {
    pub scratch: Reservation,
    pub retained: Reservation,
}
impl Credit {
    fn grow(&mut self, n: usize) -> Result<(), Failure> {
        self.budget.add(n)?;
        self.bytes += n;
        Ok(())
    }
    fn release(&mut self, n: usize) {
        self.bytes -= n;
        self.budget.used.fetch_sub(n, SeqCst);
    }
}
impl Drop for Credit {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, SeqCst);
    }
}
/// Retained data cannot be moved out independently of its aggregate credit.
#[derive(Debug)]
pub struct Retained<T> {
    pub(crate) value: T,
    pub(crate) _credit: Credit,
}
impl<T> Deref for Retained<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

// Upper requested size of a Rust BTree internal node for this pinned build:
// B=6 => 11 key/value slots and12 child edges, plus a conservative3-word header.
// The compiler/std layout dependency is explicit and needs requalification when
// the pinned build changes. It is not an arbitrary per-input node restriction.
const MAP_NODE: usize = 11 * (size_of::<String>() + size_of::<Value>()) + 15 * size_of::<usize>();
fn quote(s: &str) -> Result<usize, Failure> {
    if s.len() > FRAME {
        return Err(Failure::Frame);
    }
    s.bytes().try_fold(2usize, |n, b| {
        n.checked_add(match b {
            b'"' | b'\\' | 8 | 9 | 10 | 12 | 13 => 2,
            0..=31 => 6,
            _ => 1,
        })
        .filter(|n| *n <= FRAME)
        .ok_or(Failure::Frame)
    })
}
/// Measure existing capacities iteratively, without copying input or recursively
/// walking the parent's stack. Workspace growth is itself charged beforehand.
pub(crate) fn retain_event(budget: &Arc<Budget>, value: &Value) -> Result<Credit, Failure> {
    let mut retained = budget.reserve(size_of::<Value>() + 4096)?;
    let mut scratch = budget.reserve(size_of::<&Value>())?;
    let mut todo = Vec::new();
    todo.try_reserve_exact(1)
        .map_err(|_| Failure::ParentBudget)?;
    todo.push(value);
    let mut wire = 0usize;
    while let Some(value) = todo.pop() {
        let (bytes, capacity) = match value {
            Value::Null => (4, 0),
            Value::Bool(v) => (if *v { 4 } else { 5 }, 0),
            Value::Number(n) => {
                let mut count = Counter(0);
                serde_json::to_writer(&mut count, n).map_err(|_| Failure::Frame)?;
                (count.0, 0)
            }
            Value::String(s) => (quote(s)?, s.capacity()),
            Value::Array(a) => (
                2 + a.len().saturating_sub(1),
                a.capacity()
                    .checked_mul(size_of::<Value>())
                    .ok_or(Failure::ParentBudget)?,
            ),
            Value::Object(m) => {
                let mut bytes = 2 + m.len().saturating_sub(1);
                let mut capacity = m
                    .len()
                    .checked_add(1)
                    .and_then(|n| n.checked_mul(MAP_NODE))
                    .ok_or(Failure::ParentBudget)?;
                for (key, _) in m {
                    bytes = bytes
                        .checked_add(quote(key)? + 1)
                        .filter(|n| *n <= FRAME)
                        .ok_or(Failure::Frame)?;
                    capacity = capacity
                        .checked_add(key.capacity())
                        .ok_or(Failure::ParentBudget)?;
                }
                (bytes, capacity)
            }
        };
        wire = wire
            .checked_add(bytes)
            .filter(|n| *n <= FRAME)
            .ok_or(Failure::Frame)?;
        retained.grow(capacity)?;
        let children = match value {
            Value::Array(a) => a.len(),
            Value::Object(m) => m.len(),
            _ => 0,
        };
        let needed = todo
            .len()
            .checked_add(children)
            .ok_or(Failure::ParentBudget)?;
        if needed > todo.capacity() {
            let old = todo.capacity() * size_of::<&Value>();
            let new = needed
                .checked_mul(size_of::<&Value>())
                .ok_or(Failure::ParentBudget)?;
            scratch.grow(new)?; // Old+new held across realloc.
            todo.try_reserve_exact(needed - todo.len())
                .map_err(|_| Failure::ParentBudget)?;
            scratch.release(old);
        }
        match value {
            Value::Array(a) => todo.extend(a),
            Value::Object(m) => todo.extend(m.values()),
            _ => {}
        }
    }
    Ok(retained)
}
pub(crate) trait Response: for<'de> serde::Deserialize<'de> {
    fn parse_reservation(n: usize) -> Result<usize, Failure>;
    fn parse(bytes: &[u8]) -> Result<Self, Failure> {
        serde_json::from_slice(bytes).map_err(|_| Failure::Protocol)
    }
}
pub(crate) fn parse_reservation<T>(n: usize, roots: usize) -> Result<usize, Failure> {
    // Raw frame+nested string scratch, decoded owned strings, envelope reparse
    // and bounded error formatting. Root Vec realloc charges old+new capacity.
    n.checked_mul(16)
        .and_then(|v| v.checked_add(roots.checked_mul(3 * size_of::<String>())?))
        .and_then(|v| v.checked_add(size_of::<T>() + 4096))
        .ok_or(Failure::ParentBudget)
}
