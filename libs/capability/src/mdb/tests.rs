//! Host tests for the mapping database: fixed scenarios, and randomized
//! operation sequences checked against a plain tree model after every step.

extern crate std;

use std::collections::{HashMap, HashSet};
use std::vec;
use std::vec::Vec;

use super::*;
use crate::{Capability, Rights};

/// A test object: an id, and whether it is an Untyped region (whose
/// capabilities may parent other objects).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Ob {
    id: u32,
    untyped: bool,
}

const CNODES: u16 = 3;
const SLOTS: u16 = 16;

struct Store(Vec<Vec<Option<Entry<Ob>>>>);

impl Store {
    fn new() -> Self {
        Self(vec![vec![None; SLOTS as usize]; CNODES as usize])
    }

    fn occupied(&self) -> Vec<SlotRef> {
        let mut out = Vec::new();
        for c in 0..CNODES {
            for i in 0..SLOTS {
                let r = SlotRef::new(c, i);
                if self.get(r).is_some() {
                    out.push(r);
                }
            }
        }
        out
    }

    fn empty(&self) -> Vec<SlotRef> {
        let mut out = Vec::new();
        for c in 0..CNODES {
            for i in 0..SLOTS {
                let r = SlotRef::new(c, i);
                if self.get(r).is_none() {
                    out.push(r);
                }
            }
        }
        out
    }
}

impl SlotStore<Ob> for Store {
    fn get(&self, at: SlotRef) -> Option<Entry<Ob>> {
        self.0[at.cnode as usize][at.index as usize]
    }
    fn set(&mut self, at: SlotRef, entry: Option<Entry<Ob>>) {
        self.0[at.cnode as usize][at.index as usize] = entry;
    }
}

fn cap(id: u32, untyped: bool) -> Capability<Ob> {
    Capability::new(Ob { id, untyped }, Rights::ALL)
}

fn s(c: u16, i: u16) -> SlotRef {
    SlotRef::new(c, i)
}

/// The derivation tree as plain parent pointers, keyed by slot.
#[derive(Default)]
struct Model {
    parent: HashMap<SlotRef, Option<SlotRef>>,
    object: HashMap<SlotRef, Ob>,
}

impl Model {
    fn root(&mut self, at: SlotRef, ob: Ob) {
        self.parent.insert(at, None);
        self.object.insert(at, ob);
    }

    fn child(&mut self, parent: SlotRef, at: SlotRef, ob: Ob) {
        self.parent.insert(at, Some(parent));
        self.object.insert(at, ob);
    }

    fn descendants(&self, at: SlotRef) -> HashSet<SlotRef> {
        let mut out = HashSet::new();
        for &r in self.parent.keys() {
            let mut p = self.parent[&r];
            while let Some(q) = p {
                if q == at {
                    out.insert(r);
                    break;
                }
                p = self.parent[&q];
            }
        }
        out
    }

    /// Delete one entry, lifting its children; returns whether it was the
    /// last capability to its object.
    fn delete(&mut self, at: SlotRef) -> bool {
        let up = self.parent.remove(&at).unwrap();
        let ob = self.object.remove(&at).unwrap();
        for p in self.parent.values_mut() {
            if *p == Some(at) {
                *p = up;
            }
        }
        !self.object.values().any(|&o| o == ob)
    }

    fn rename(&mut self, from: SlotRef, to: SlotRef) {
        let p = self.parent.remove(&from).unwrap();
        let o = self.object.remove(&from).unwrap();
        self.parent.insert(to, p);
        self.object.insert(to, o);
        for q in self.parent.values_mut() {
            if *q == Some(from) {
                *q = Some(to);
            }
        }
    }
}

/// Check every invariant of the store, and that it encodes the model's tree.
fn check(store: &Store, model: &Model) {
    let occupied = store.occupied();
    assert_eq!(occupied.len(), model.parent.len(), "slot count");
    // Links agree in both directions.
    for &r in &occupied {
        let e = store.get(r).unwrap();
        if let Some(p) = e.link.prev {
            assert_eq!(store.get(p).unwrap().link.next, Some(r), "prev.next");
        }
        if let Some(n) = e.link.next {
            assert_eq!(store.get(n).unwrap().link.prev, Some(r), "next.prev");
        }
        assert_eq!(e.cap.object, model.object[&r], "object at {r:?}");
    }
    // Walk every list from its head: each is a pre-order walk whose implied
    // parents are the model's, and a non-Untyped object's capabilities are
    // one contiguous run.
    let mut seen = HashSet::new();
    let mut ended: HashSet<Ob> = HashSet::new();
    for &head in occupied
        .iter()
        .filter(|&&r| store.get(r).unwrap().link.prev.is_none())
    {
        let mut stack: Vec<SlotRef> = Vec::new();
        let mut cur = Some(head);
        let mut run: Option<Ob> = None;
        while let Some(r) = cur {
            assert!(seen.insert(r), "{r:?} reached twice");
            let e = store.get(r).unwrap();
            let d = e.link.depth as usize;
            assert!(d <= stack.len(), "depth jumps at {r:?}");
            stack.truncate(d);
            let parent = stack.last().copied();
            assert_eq!(parent, model.parent[&r], "parent of {r:?}");
            stack.push(r);
            let ob = e.cap.object;
            if run != Some(ob) {
                if let Some(prev) = run {
                    if !prev.untyped {
                        ended.insert(prev);
                    }
                }
                assert!(!ended.contains(&ob), "{ob:?} split into two runs");
                run = Some(ob);
            }
            cur = e.link.next;
        }
        if let Some(prev) = run {
            if !prev.untyped {
                ended.insert(prev);
            }
        }
    }
    assert_eq!(seen.len(), occupied.len(), "an entry is in no list");
}

#[test]
fn child_follows_parent_one_level_down() {
    let mut st = Store::new();
    insert_root(&mut st, s(0, 0), cap(1, true)).unwrap();
    insert_child(&mut st, s(0, 0), s(1, 3), cap(2, false)).unwrap();
    let root = st.get(s(0, 0)).unwrap();
    let child = st.get(s(1, 3)).unwrap();
    assert_eq!(root.link.next, Some(s(1, 3)));
    assert_eq!(child.link.prev, Some(s(0, 0)));
    assert_eq!(child.link.depth, 1);
    assert!(has_descendants(&st, s(0, 0)).unwrap());
    assert!(!has_descendants(&st, s(1, 3)).unwrap());
}

#[test]
fn errors_change_nothing() {
    let mut st = Store::new();
    insert_root(&mut st, s(0, 0), cap(1, false)).unwrap();
    assert_eq!(
        insert_root(&mut st, s(0, 0), cap(2, false)),
        Err(MdbError::Occupied)
    );
    assert_eq!(
        insert_child(&mut st, s(0, 1), s(0, 2), cap(1, false)),
        Err(MdbError::Empty)
    );
    assert_eq!(
        insert_child(&mut st, s(0, 0), s(0, 0), cap(1, false)),
        Err(MdbError::Occupied)
    );
    assert_eq!(delete(&mut st, s(0, 5)), Err(MdbError::Empty));
    assert_eq!(move_entry(&mut st, s(0, 5), s(0, 6)), Err(MdbError::Empty));
    assert_eq!(
        move_entry(&mut st, s(0, 0), s(0, 0)),
        Err(MdbError::Occupied)
    );
    assert_eq!(revoke_step(&mut st, s(0, 5)), Err(MdbError::Empty));
    let mut m = Model::default();
    m.root(
        s(0, 0),
        Ob {
            id: 1,
            untyped: false,
        },
    );
    check(&st, &m);
}

#[test]
fn depth_is_bounded() {
    let mut st = Store::new();
    st.set(
        s(0, 0),
        Some(Entry {
            cap: cap(1, false),
            link: Link {
                prev: None,
                next: None,
                depth: u16::MAX,
            },
        }),
    );
    assert_eq!(
        insert_child(&mut st, s(0, 0), s(0, 1), cap(1, false)),
        Err(MdbError::TooDeep)
    );
    assert!(st.get(s(0, 1)).is_none());
    assert_eq!(st.get(s(0, 0)).unwrap().link.next, None);
}

#[test]
fn delete_lifts_descendants_to_the_parent() {
    // E -> A -> (A1, A2 -> A21), E -> B
    let mut st = Store::new();
    let mut m = Model::default();
    let e = Ob {
        id: 1,
        untyped: false,
    };
    insert_root(&mut st, s(0, 0), cap(1, false)).unwrap();
    m.root(s(0, 0), e);
    for (parent, at) in [
        (s(0, 0), s(0, 1)), // B
        (s(0, 0), s(0, 2)), // A
        (s(0, 2), s(0, 3)), // A2
        (s(0, 3), s(0, 4)), // A21
        (s(0, 2), s(0, 5)), // A1
    ] {
        insert_child(&mut st, parent, at, cap(1, false)).unwrap();
        m.child(parent, at, e);
        check(&st, &m);
    }
    let gone = delete(&mut st, s(0, 2)).unwrap();
    assert!(!gone.last);
    m.delete(s(0, 2));
    check(&st, &m);
    assert_eq!(st.get(s(0, 5)).unwrap().link.depth, 1);
    assert_eq!(st.get(s(0, 4)).unwrap().link.depth, 2);
}

#[test]
fn last_capability_is_detected() {
    // An Untyped with two objects retyped from it, one of them copied.
    let mut st = Store::new();
    insert_root(&mut st, s(0, 0), cap(1, true)).unwrap();
    insert_child(&mut st, s(0, 0), s(0, 1), cap(2, false)).unwrap();
    insert_child(&mut st, s(0, 0), s(0, 2), cap(3, false)).unwrap();
    insert_child(&mut st, s(0, 2), s(1, 0), cap(3, false)).unwrap();
    assert!(!delete(&mut st, s(0, 2)).unwrap().last);
    assert!(delete(&mut st, s(1, 0)).unwrap().last);
    assert!(delete(&mut st, s(0, 1)).unwrap().last);
}

#[test]
fn revoke_takes_back_exactly_one_badged_branch() {
    // An endpoint, minted into two badged copies; one badge's copies are
    // passed on further. Revoking that badge leaves everything else.
    let ep = |badge| Capability {
        badge,
        ..cap(7, false)
    };
    let mut st = Store::new();
    insert_root(&mut st, s(0, 0), ep(0)).unwrap();
    insert_child(&mut st, s(0, 0), s(0, 1), ep(2)).unwrap(); // badge 2
    insert_child(&mut st, s(0, 0), s(0, 2), ep(1)).unwrap(); // badge 1
    insert_child(&mut st, s(0, 2), s(1, 0), ep(1)).unwrap();
    insert_child(&mut st, s(1, 0), s(2, 0), ep(1)).unwrap();
    insert_child(&mut st, s(0, 2), s(1, 1), ep(1)).unwrap();
    insert_child(&mut st, s(0, 1), s(2, 5), ep(2)).unwrap();
    let mut gone = Vec::new();
    while let Some((at, d)) = revoke_step(&mut st, s(0, 2)).unwrap() {
        assert_eq!(d.cap.badge, 1);
        assert!(!d.last, "the endpoint itself stays");
        gone.push(at);
    }
    gone.sort();
    assert_eq!(gone, [s(1, 0), s(1, 1), s(2, 0)]);
    assert_eq!(st.occupied(), [s(0, 0), s(0, 1), s(0, 2), s(2, 5)]);
    assert!(!has_descendants(&st, s(0, 2)).unwrap());
    assert!(has_descendants(&st, s(0, 1)).unwrap());
}

/// Build the same mixed tree every time: an Untyped, a copy of it, objects
/// retyped from both, and copies of those objects.
fn mixed_tree(st: &mut Store, m: &mut Model) {
    let steps: [(Option<SlotRef>, SlotRef, Ob); 9] = [
        (
            None,
            s(0, 0),
            Ob {
                id: 1,
                untyped: true,
            },
        ),
        (
            Some(s(0, 0)),
            s(0, 1),
            Ob {
                id: 1,
                untyped: true,
            },
        ),
        (
            Some(s(0, 0)),
            s(0, 2),
            Ob {
                id: 2,
                untyped: false,
            },
        ),
        (
            Some(s(0, 1)),
            s(1, 0),
            Ob {
                id: 3,
                untyped: false,
            },
        ),
        (
            Some(s(1, 0)),
            s(1, 1),
            Ob {
                id: 3,
                untyped: false,
            },
        ),
        (
            Some(s(1, 1)),
            s(2, 0),
            Ob {
                id: 3,
                untyped: false,
            },
        ),
        (
            Some(s(0, 2)),
            s(2, 1),
            Ob {
                id: 2,
                untyped: false,
            },
        ),
        (
            Some(s(0, 0)),
            s(2, 2),
            Ob {
                id: 4,
                untyped: false,
            },
        ),
        (
            Some(s(1, 0)),
            s(2, 3),
            Ob {
                id: 3,
                untyped: false,
            },
        ),
    ];
    for (parent, at, ob) in steps {
        let c = Capability::new(ob, Rights::ALL);
        match parent {
            None => {
                insert_root(st, at, c).unwrap();
                m.root(at, ob);
            }
            Some(p) => {
                insert_child(st, p, at, c).unwrap();
                m.child(p, at, ob);
            }
        }
    }
    check(st, m);
}

#[test]
fn revoke_cut_off_after_any_step_leaves_a_valid_tree() {
    let total = {
        let mut st = Store::new();
        let mut m = Model::default();
        mixed_tree(&mut st, &mut m);
        m.descendants(s(0, 0)).len()
    };
    assert_eq!(total, 8);
    for cut in 0..=total {
        let mut st = Store::new();
        let mut m = Model::default();
        mixed_tree(&mut st, &mut m);
        let want = m.descendants(s(0, 0));
        let mut gone = HashSet::new();
        for _ in 0..cut {
            let (at, d) = revoke_step(&mut st, s(0, 0)).unwrap().unwrap();
            assert!(m.descendants(at).is_empty(), "{at:?} was not a leaf");
            let last = m.delete(at);
            if !d.cap.object.untyped {
                assert_eq!(d.last, last, "last at {at:?}");
            }
            gone.insert(at);
            check(&st, &m);
        }
        // Continuing later finishes the job, and only the root is left.
        while let Some((at, _)) = revoke_step(&mut st, s(0, 0)).unwrap() {
            m.delete(at);
            gone.insert(at);
            check(&st, &m);
        }
        assert_eq!(gone, want);
        assert_eq!(st.occupied(), [s(0, 0)]);
    }
}

#[test]
fn move_keeps_the_place_in_the_tree() {
    let mut st = Store::new();
    let mut m = Model::default();
    mixed_tree(&mut st, &mut m);
    move_entry(&mut st, s(1, 0), s(2, 9)).unwrap();
    m.rename(s(1, 0), s(2, 9));
    check(&st, &m);
    move_entry(&mut st, s(0, 0), s(1, 15)).unwrap();
    m.rename(s(0, 0), s(1, 15));
    check(&st, &m);
}

/// A small deterministic generator, so a failing sequence replays.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> Option<T> {
        (!xs.is_empty()).then(|| xs[self.below(xs.len())])
    }
}

#[test]
fn random_operations_keep_every_invariant() {
    for seed in 1..=200u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut st = Store::new();
        let mut m = Model::default();
        let mut next_id = 1;
        for _ in 0..300 {
            let full = st.occupied();
            let free = st.empty();
            match rng.below(10) {
                // A new tree: an Untyped region or an object.
                0 => {
                    if let Some(at) = rng.pick(&free) {
                        let ob = Ob {
                            id: next_id,
                            untyped: rng.below(2) == 0,
                        };
                        next_id += 1;
                        insert_root(&mut st, at, Capability::new(ob, Rights::ALL)).unwrap();
                        m.root(at, ob);
                    }
                }
                // A copy, or a retype from an Untyped.
                1..=4 => {
                    if let (Some(p), Some(at)) = (rng.pick(&full), rng.pick(&free)) {
                        let parent = m.object[&p];
                        let ob = if parent.untyped && rng.below(2) == 0 {
                            next_id += 1;
                            Ob {
                                id: next_id - 1,
                                untyped: false,
                            }
                        } else {
                            parent
                        };
                        insert_child(&mut st, p, at, Capability::new(ob, Rights::ALL)).unwrap();
                        m.child(p, at, ob);
                    }
                }
                5 | 6 => {
                    if let Some(at) = rng.pick(&full) {
                        let d = delete(&mut st, at).unwrap();
                        let last = m.delete(at);
                        if !d.cap.object.untyped {
                            assert_eq!(d.last, last, "seed {seed}: last at {at:?}");
                        }
                    }
                }
                // A revoke, possibly cut short and finished later.
                7 | 8 => {
                    if let Some(at) = rng.pick(&full) {
                        let budget = rng.below(4);
                        for _ in 0..budget {
                            match revoke_step(&mut st, at).unwrap() {
                                Some((leaf, d)) => {
                                    assert!(m.descendants(leaf).is_empty());
                                    let last = m.delete(leaf);
                                    if !d.cap.object.untyped {
                                        assert_eq!(d.last, last, "seed {seed}");
                                    }
                                }
                                None => {
                                    assert!(m.descendants(at).is_empty());
                                    break;
                                }
                            }
                        }
                    }
                }
                _ => {
                    if let (Some(from), Some(to)) = (rng.pick(&full), rng.pick(&free)) {
                        move_entry(&mut st, from, to).unwrap();
                        m.rename(from, to);
                    }
                }
            }
            check(&st, &m);
        }
    }
}
