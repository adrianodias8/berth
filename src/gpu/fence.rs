//! Fenced requests waiting for the 3D renderer. A request the guest fenced
//! is answered only once the renderer has finished the work queued before
//! it: server.rs creates the fence and says what will signal it, device.rs
//! keeps the virtqueue element here, and the renderer's signals (from the
//! berth-virgl worker, posted back onto the device queue) release it.
//!
//! Two timelines, as in virglrenderer: the global one (ctx 0 fences, which
//! carry a host u32 sequence because virglrenderer's are 32-bit) and one
//! per (context, ring) for CONTEXT_INIT guests, keyed by the guest's own
//! u64 fence_id. A signal retires everything on its timeline up to and
//! including it, and only that: the guest's driver treats an answered
//! fence as signalling every earlier one, so nothing may be answered ahead
//! of an earlier fence on the same timeline.
//!
//! Generic over the element so it's tested here without
//! Virtualization.framework.

use std::collections::{HashMap, VecDeque};

/// A fence the renderer signals, or will.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Signal {
    /// A global fence: the host sequence number handed to the renderer.
    Global(u32),
    /// A per-context fence on one of its rings: the guest's fence_id.
    Ring { ctx: u32, ring: u32, fence: u64 },
}

/// Whether host sequence `seq` is at or before `signalled`, across the
/// u32 wrap (sequences are handed out in order, far fewer than 2^31 apart).
fn global_reached(seq: u32, signalled: u32) -> bool {
    (signalled.wrapping_sub(seq) as i32) >= 0
}

/// One timeline's waiting elements, oldest first: (fence, element, reply).
type Queue<F, E> = VecDeque<(F, E, Vec<u8>)>;

/// Elements waiting for their fence, each with the reply it gets.
pub struct Pending<E> {
    global: Queue<u32, E>,
    rings: HashMap<(u32, u32), Queue<u64, E>>,
}

impl<E> Default for Pending<E> {
    fn default() -> Self {
        Self { global: VecDeque::new(), rings: HashMap::new() }
    }
}

impl<E> Pending<E> {
    /// Hold `element` until `on` is signalled. Fences on a timeline are
    /// created in order, so each queue stays sorted.
    pub fn push(&mut self, on: Signal, element: E, reply: Vec<u8>) {
        match on {
            Signal::Global(seq) => self.global.push_back((seq, element, reply)),
            Signal::Ring { ctx, ring, fence } => self.rings.entry((ctx, ring)).or_default().push_back((fence, element, reply)),
        }
    }

    /// The elements `signals` release, oldest first on each timeline.
    pub fn complete(&mut self, signals: &[Signal]) -> Vec<(E, Vec<u8>)> {
        let mut done = Vec::new();
        for s in signals {
            match *s {
                Signal::Global(signalled) => {
                    while self.global.front().is_some_and(|(seq, _, _)| global_reached(*seq, signalled)) {
                        let (_, e, reply) = self.global.pop_front().unwrap();
                        done.push((e, reply));
                    }
                }
                Signal::Ring { ctx, ring, fence } => {
                    let Some(q) = self.rings.get_mut(&(ctx, ring)) else { continue };
                    while q.front().is_some_and(|(f, _, _)| *f <= fence) {
                        let (_, e, reply) = q.pop_front().unwrap();
                        done.push((e, reply));
                    }
                    if q.is_empty() {
                        self.rings.remove(&(ctx, ring));
                    }
                }
            }
        }
        done
    }

    /// Everything waiting on a destroyed context's rings, oldest first on
    /// each: the renderer drops those fences without signalling them.
    pub fn take_context(&mut self, ctx: u32) -> Vec<(E, Vec<u8>)> {
        let mut keys: Vec<_> = self.rings.keys().filter(|&&(c, _)| c == ctx).copied().collect();
        keys.sort_unstable();
        keys.into_iter().flat_map(|k| self.rings.remove(&k).unwrap()).map(|(_, e, r)| (e, r)).collect()
    }

    /// Everything still waiting, global first: for a device reset, which
    /// answers them all with an error.
    pub fn take_all(&mut self) -> Vec<(E, Vec<u8>)> {
        let mut all: Vec<_> = self.global.drain(..).map(|(_, e, r)| (e, r)).collect();
        for (_, q) in self.rings.drain() {
            all.extend(q.into_iter().map(|(_, e, r)| (e, r)));
        }
        all
    }

    pub fn len(&self) -> usize {
        self.global.len() + self.rings.values().map(VecDeque::len).sum::<usize>()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(done: Vec<(u32, Vec<u8>)>) -> Vec<u32> {
        done.into_iter().map(|(e, _)| e).collect()
    }

    #[test]
    fn global_fences_release_in_order() {
        let mut p = Pending::default();
        for (i, seq) in [5u32, 6, 7].into_iter().enumerate() {
            p.push(Signal::Global(seq), i as u32, vec![i as u8]);
        }
        assert_eq!(ids(p.complete(&[Signal::Global(4)])), Vec::<u32>::new());
        // A signal retires its fence and every earlier one, never a later one.
        let done = p.complete(&[Signal::Global(6)]);
        assert_eq!(done, vec![(0, vec![0]), (1, vec![1])]);
        assert_eq!(p.len(), 1);
        // An old signal arriving late releases nothing.
        assert_eq!(ids(p.complete(&[Signal::Global(5)])), Vec::<u32>::new());
        assert_eq!(ids(p.complete(&[Signal::Global(7)])), vec![2]);
        assert!(p.is_empty());
    }

    #[test]
    fn global_sequence_wraps() {
        let mut p = Pending::default();
        p.push(Signal::Global(u32::MAX - 1), 1, vec![]);
        p.push(Signal::Global(u32::MAX), 2, vec![]);
        p.push(Signal::Global(1), 3, vec![]);
        p.push(Signal::Global(2), 4, vec![]);
        assert_eq!(ids(p.complete(&[Signal::Global(u32::MAX)])), vec![1, 2]);
        assert_eq!(ids(p.complete(&[Signal::Global(1)])), vec![3]);
        assert_eq!(ids(p.complete(&[Signal::Global(2)])), vec![4]);
    }

    #[test]
    fn rings_are_independent() {
        let mut p = Pending::default();
        let ring = |ctx, ring, fence| Signal::Ring { ctx, ring, fence };
        let big = (1u64 << 32) + 10;
        p.push(ring(1, 0, big), 10, vec![]);
        p.push(ring(1, 0, big + 1), 11, vec![]);
        p.push(ring(1, 1, 3), 20, vec![]);
        p.push(ring(2, 0, 3), 30, vec![]);
        p.push(Signal::Global(9), 40, vec![]);
        // Out of order across timelines is fine; within one it isn't.
        assert_eq!(ids(p.complete(&[ring(2, 0, 3)])), vec![30]);
        assert_eq!(ids(p.complete(&[ring(1, 0, big)])), vec![10]);
        assert_eq!(ids(p.complete(&[ring(1, 0, big - 1)])), Vec::<u32>::new());
        // Same fence id on another ring of the context isn't it.
        assert_eq!(ids(p.complete(&[ring(1, 2, 3), ring(3, 1, 3)])), Vec::<u32>::new());
        assert_eq!(ids(p.complete(&[ring(1, 1, 99), Signal::Global(9), ring(1, 0, big + 1)])), vec![20, 40, 11]);
        assert!(p.is_empty());
    }

    #[test]
    fn a_destroyed_context_lets_go_of_its_rings() {
        let mut p = Pending::default();
        let ring = |ctx, ring, fence| Signal::Ring { ctx, ring, fence };
        p.push(ring(1, 1, 5), 11, vec![]);
        p.push(ring(1, 0, 3), 10, vec![]);
        p.push(ring(1, 0, 4), 12, vec![]);
        p.push(ring(2, 0, 3), 20, vec![]);
        p.push(Signal::Global(1), 30, vec![]);
        assert_eq!(ids(p.take_context(1)), vec![10, 12, 11]);
        assert_eq!(p.len(), 2);
        assert_eq!(ids(p.take_context(1)), Vec::<u32>::new());
    }

    #[test]
    fn reset_takes_everything() {
        let mut p = Pending::default();
        p.push(Signal::Ring { ctx: 1, ring: 0, fence: 1 }, 2, vec![]);
        p.push(Signal::Global(1), 1, vec![]);
        assert_eq!(ids(p.take_all()), vec![1, 2]);
        assert!(p.is_empty());
    }
}
