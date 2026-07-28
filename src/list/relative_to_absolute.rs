//! relative_to_absolute_patches: re-expresses a sequence of edits, each
//! positioned against the text as it stood at that moment, as an ordered
//! set of range replacements against the original text.
//!
//! The conversion works by composing: edits that touch earlier edits
//! merge or cancel, so output patches don't correspond one-to-one with
//! the input. Ported exactly from braid-text's JS implementation,
//! including its merge behavior.
//!
//! The structure is a sequence of segments over the original text:
//! "kept" segments (original text that still survives) interleaved with
//! "replacement" segments (new content occupying `del` original chars).
//! Each incoming patch locates itself in current coordinates and splices.
//! Two interchangeable stores implement the sequence: a Vec (the simple
//! reference, used in tests) and a treap (O(log n), the one that ships).

use smartstring::alias::String as SmartString;
use crate::DTRange;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Replacement {
    /// For input, the range is in relative (current-text) coordinates;
    /// for output, in absolute (original-text) coordinates
    pub range: DTRange,
    pub content: SmartString,
}

impl Replacement {
    pub fn new(start: usize, end: usize, content: &str) -> Self {
        Replacement { range: (start..end).into(), content: content.into() }
    }
}

/// One segment. content: None = kept original text; content: Some =
/// a replacement covering `del` original chars
#[derive(Debug, Clone, Default)]
struct Segment {
    size: usize,               // current length in chars
    del: usize,                // original chars this replacement covers
    content: Option<SmartString>,
}

impl Segment {
    fn kept(size: usize) -> Self { Segment { size, del: 0, content: None } }
    fn repl(content: &str, del: usize) -> Self {
        Segment { size: 0, del, content: Some(content.into()) }
    }
    fn is_kept(&self) -> bool { self.content.is_none() }
    /// What a swallowed middle segment contributes to the deletion count
    fn del_or_size(&self) -> usize {
        if self.is_kept() { self.size } else { self.del }
    }
}

/// The sentinel tail: a huge kept segment standing in for "all the
/// original text". Real positions never get near it, so the exact value
/// only needs headroom. (usize::MAX / 4 keeps sums overflow-free.)
const HUGE: usize = usize::MAX / 4;

/// Char-boundary byte offset for a codepoint index
fn cp_to_byte(s: &str, cp: usize) -> usize {
    s.char_indices().nth(cp).map_or(s.len(), |(b, _)| b)
}

fn cp_len(s: &str) -> usize { s.chars().count() }

/// The store interface the splice logic runs against. Node ids are
/// stable for a node's lifetime.
trait SegStore {
    fn locate(&self, start: usize) -> (usize, usize);   // (id, offset within)
    fn next_of(&self, id: usize) -> usize;
    fn seg(&self, id: usize) -> &Segment;
    fn set_content(&mut self, id: usize, content: Option<SmartString>, del: usize);
    fn resize(&mut self, id: usize, new_size: usize);
    fn insert_before(&mut self, id: usize, seg: Segment) -> usize;
    fn insert_after(&mut self, id: usize, seg: Segment) -> usize;
    fn remove(&mut self, id: usize);
    fn emit(&self) -> Vec<Replacement>;
}

/// Splice one relative patch into the segment sequence. A line-for-line
/// transcription of the JS splice cases.
fn apply_one<T: SegStore>(st: &mut T, start_abs: usize, del: usize, content: &str) {
    let (node, start) = st.locate(start_abs);
    let node_size = st.seg(node).size;

    if start + del < node_size {
        // The edit fits strictly inside this segment
        if st.seg(node).is_kept() {
            // Split the kept text around a new replacement
            if start > 0 {
                let left = st.insert_before(node, Segment::kept(0));
                st.resize(left, start);
            }
            let x = st.insert_before(node, Segment::repl(content, del));
            st.resize(x, cp_len(content));
            st.resize(node, node_size - (start + del));
        } else {
            // Splice within the replacement's content
            let c = st.seg(node).content.as_ref().unwrap();
            let mut s = String::with_capacity(c.len() + content.len());
            s.push_str(&c[..cp_to_byte(c, start)]);
            s.push_str(content);
            s.push_str(&c[cp_to_byte(c, start + del)..]);
            let del_keep = st.seg(node).del;
            st.set_content(node, Some(s.as_str().into()), del_keep);
            st.resize(node, cp_len(&s));
        }
    } else {
        // The edit runs past this segment: swallow whole middle segments,
        // then merge with the segment the edit ends in
        let mut remaining = start + del - node_size;
        let mut middle_del = 0;
        let next = loop {
            let next = st.next_of(node);
            if remaining >= st.seg(next).size {
                remaining -= st.seg(next).size;
                middle_del += st.seg(next).del_or_size();
                st.resize(next, 0);
                st.remove(next);
            } else {
                break next;
            }
        };

        let node_kept = st.seg(node).is_kept();
        let next_kept = st.seg(next).is_kept();
        let next_size = st.seg(next).size;

        if node_kept && next_kept {
            if start == 0 {
                // The kept segment becomes the replacement outright
                st.set_content(node, Some(content.into()),
                               node_size + middle_del + remaining);
                st.resize(node, cp_len(content));
            } else {
                st.resize(node, start);
                let x = st.insert_after(node, Segment::repl(content,
                    node_size - start + middle_del + remaining));
                st.resize(x, cp_len(content));
            }
            st.resize(next, next_size - remaining);
        } else if node_kept {
            // Prepend into the following replacement
            let nc = st.seg(next).content.as_ref().unwrap();
            let mut s = String::with_capacity(content.len() + nc.len());
            s.push_str(content);
            s.push_str(&nc[cp_to_byte(nc, remaining)..]);
            let new_del = st.seg(next).del + (node_size - start) + middle_del;
            st.set_content(next, Some(s.as_str().into()), new_del);
            st.resize(node, start);
            if st.seg(node).size == 0 { st.remove(node); }
            st.resize(next, cp_len(&s));
        } else if next_kept {
            // Append onto this replacement
            let c = st.seg(node).content.as_ref().unwrap();
            let mut s = String::with_capacity(c.len() + content.len());
            s.push_str(&c[..cp_to_byte(c, start)]);
            s.push_str(content);
            let new_del = st.seg(node).del + middle_del + remaining;
            st.set_content(node, Some(s.as_str().into()), new_del);
            st.resize(node, cp_len(&s));
            st.resize(next, next_size - remaining);
        } else {
            // Bridge two replacements into one
            let c = st.seg(node).content.as_ref().unwrap();
            let nc = st.seg(next).content.as_ref().unwrap();
            let mut s = String::with_capacity(c.len() + content.len() + nc.len());
            s.push_str(&c[..cp_to_byte(c, start)]);
            s.push_str(content);
            s.push_str(&nc[cp_to_byte(nc, remaining)..]);
            let new_del = st.seg(node).del + middle_del + st.seg(next).del;
            st.set_content(node, Some(s.as_str().into()), new_del);
            st.resize(node, cp_len(&s));
            st.resize(next, 0);
            st.remove(next);
        }
    }
}

fn run<T: SegStore>(st: &mut T, patches: &[Replacement]) -> Vec<Replacement> {
    for p in patches {
        apply_one(st, p.range.start, p.range.end - p.range.start, &p.content);
    }
    st.emit()
}

pub fn relative_to_absolute_patches(patches: &[Replacement]) -> Vec<Replacement> {
    run(&mut TreapStore::new(), patches)
}

// ── The Vec store: the simple reference implementation ──────────────────

struct VecStore {
    arena: Vec<Segment>,
    order: Vec<usize>,   // segment ids in sequence order
}

impl VecStore {
    fn new() -> Self {
        VecStore { arena: vec![Segment::kept(HUGE)], order: vec![0] }
    }
    fn pos_of(&self, id: usize) -> usize {
        self.order.iter().position(|&x| x == id).unwrap()
    }
}

impl SegStore for VecStore {
    fn locate(&self, start: usize) -> (usize, usize) {
        let mut cum = 0;
        for &id in &self.order {
            let seg = &self.arena[id];
            // Land here if inside, or at the very end of a replacement
            // (boundary edits attach to the replacement, like the JS)
            if start < cum + seg.size
                || (!seg.is_kept() && start == cum + seg.size) {
                return (id, start - cum);
            }
            cum += seg.size;
        }
        unreachable!("position past the sentinel tail")
    }
    fn next_of(&self, id: usize) -> usize {
        self.order[self.pos_of(id) + 1]
    }
    fn seg(&self, id: usize) -> &Segment { &self.arena[id] }
    fn set_content(&mut self, id: usize, content: Option<SmartString>, del: usize) {
        self.arena[id].content = content;
        self.arena[id].del = del;
    }
    fn resize(&mut self, id: usize, new_size: usize) {
        self.arena[id].size = new_size;
    }
    fn insert_before(&mut self, id: usize, seg: Segment) -> usize {
        let new_id = self.arena.len();
        self.arena.push(seg);
        let pos = self.pos_of(id);
        self.order.insert(pos, new_id);
        new_id
    }
    fn insert_after(&mut self, id: usize, seg: Segment) -> usize {
        let new_id = self.arena.len();
        self.arena.push(seg);
        let pos = self.pos_of(id);
        self.order.insert(pos + 1, new_id);
        new_id
    }
    fn remove(&mut self, id: usize) {
        let pos = self.pos_of(id);
        self.order.remove(pos);
    }
    fn emit(&self) -> Vec<Replacement> {
        let mut out = Vec::new();
        let mut offset = 0;
        for &id in &self.order {
            let seg = &self.arena[id];
            match &seg.content {
                None => offset += seg.size,
                Some(c) => {
                    out.push(Replacement {
                        range: (offset..offset + seg.del).into(),
                        content: c.clone(),
                    });
                    offset += seg.del;
                }
            }
        }
        out
    }
}

// ── The treap store: O(log n), what ships ───────────────────────────────

const NIL: usize = usize::MAX;

struct TreapNode {
    seg: Segment,
    prio: u64,
    left: usize,
    right: usize,
    parent: usize,
    left_size: usize,   // total size of the left subtree
}

pub(crate) struct TreapStore {
    nodes: Vec<TreapNode>,
    root: usize,
    rng: u64,
}

impl TreapStore {
    pub(crate) fn new() -> Self {
        let mut st = TreapStore { nodes: Vec::new(), root: 0, rng: 0x2545F4914F6CDD1D };
        st.alloc(Segment::kept(HUGE));
        st
    }

    fn alloc(&mut self, seg: Segment) -> usize {
        // splitmix-ish deterministic priorities
        self.rng = self.rng.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(0xBF58476D1CE4E5B9);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        self.nodes.push(TreapNode {
            seg, prio: z ^ (z >> 31),
            left: NIL, right: NIL, parent: NIL, left_size: 0,
        });
        self.nodes.len() - 1
    }

    /// Rotate `n` above its parent, keeping left_size right. The fixup
    /// needs only the two nodes' own fields (the JS on_rotate trick)
    fn rotate_up(&mut self, n: usize) {
        let p = self.nodes[n].parent;
        let g = self.nodes[p].parent;

        if self.nodes[p].left == n {
            // n's right subtree becomes p's left
            let moved = self.nodes[n].right;
            self.nodes[p].left = moved;
            if moved != NIL { self.nodes[moved].parent = p; }
            self.nodes[n].right = p;
            self.nodes[p].left_size -= self.nodes[n].left_size + self.nodes[n].seg.size;
        } else {
            let moved = self.nodes[n].left;
            self.nodes[p].right = moved;
            if moved != NIL { self.nodes[moved].parent = p; }
            self.nodes[n].left = p;
            self.nodes[n].left_size += self.nodes[p].left_size + self.nodes[p].seg.size;
        }
        self.nodes[p].parent = n;
        self.nodes[n].parent = g;
        if g == NIL { self.root = n; }
        else if self.nodes[g].left == p { self.nodes[g].left = n; }
        else { self.nodes[g].right = n; }
    }

    fn bubble_prio(&mut self, n: usize) {
        while self.nodes[n].parent != NIL
            && self.nodes[n].prio > self.nodes[self.nodes[n].parent].prio {
            self.rotate_up(n);
        }
    }

    /// Attach `new_id` (size 0) immediately before/after `id` in order.
    /// Size 0 means no sums change on attach; resize() does that later.
    fn attach(&mut self, id: usize, new_id: usize, before: bool) {
        debug_assert_eq!(self.nodes[new_id].seg.size, 0);
        if before {
            if self.nodes[id].left == NIL {
                self.nodes[id].left = new_id;
                self.nodes[new_id].parent = id;
            } else {
                let mut n = self.nodes[id].left;
                while self.nodes[n].right != NIL { n = self.nodes[n].right; }
                self.nodes[n].right = new_id;
                self.nodes[new_id].parent = n;
            }
        } else {
            if self.nodes[id].right == NIL {
                self.nodes[id].right = new_id;
                self.nodes[new_id].parent = id;
            } else {
                let mut n = self.nodes[id].right;
                while self.nodes[n].left != NIL { n = self.nodes[n].left; }
                self.nodes[n].left = new_id;
                self.nodes[new_id].parent = n;
            }
        }
        self.bubble_prio(new_id);
    }

    #[cfg(test)]
    fn dbg_check(&self) {
        // left_size must equal the left subtree's total size, everywhere
        fn total(st: &TreapStore, n: usize) -> usize {
            if n == NIL { return 0 }
            let left = total(st, st.nodes[n].left);
            assert_eq!(left, st.nodes[n].left_size, "left_size wrong at {n}");
            left + st.nodes[n].seg.size + total(st, st.nodes[n].right)
        }
        total(self, self.root);
    }
}

impl SegStore for TreapStore {
    fn locate(&self, mut start: usize) -> (usize, usize) {
        // The JS walk, clause for clause
        let mut n = self.root;
        loop {
            let node = &self.nodes[n];
            let ls = node.left_size;
            if start < ls
                || (node.left != NIL && node.seg.is_kept() && start == ls) {
                n = node.left;
            } else if start > ls + node.seg.size
                || (node.seg.is_kept() && start == ls + node.seg.size) {
                start -= ls + node.seg.size;
                n = node.right;
            } else {
                return (n, start - ls);
            }
        }
    }

    fn next_of(&self, id: usize) -> usize {
        if self.nodes[id].right != NIL {
            let mut n = self.nodes[id].right;
            while self.nodes[n].left != NIL { n = self.nodes[n].left; }
            n
        } else {
            let mut n = id;
            loop {
                let p = self.nodes[n].parent;
                debug_assert_ne!(p, NIL, "next past the sentinel tail");
                if self.nodes[p].left == n { return p }
                n = p;
            }
        }
    }

    fn seg(&self, id: usize) -> &Segment { &self.nodes[id].seg }

    fn set_content(&mut self, id: usize, content: Option<SmartString>, del: usize) {
        self.nodes[id].seg.content = content;
        self.nodes[id].seg.del = del;
    }

    fn resize(&mut self, id: usize, new_size: usize) {
        let old = self.nodes[id].seg.size;
        if old == new_size { return }
        self.nodes[id].seg.size = new_size;
        // Bubble the delta up through every ancestor we're left of
        let mut n = id;
        loop {
            let p = self.nodes[n].parent;
            if p == NIL { break }
            if self.nodes[p].left == n {
                self.nodes[p].left_size =
                    (self.nodes[p].left_size + new_size) - old;
            }
            n = p;
        }
    }

    fn insert_before(&mut self, id: usize, seg: Segment) -> usize {
        let size = seg.size;
        let mut seg = seg;
        seg.size = 0;
        let new_id = self.alloc(seg);
        self.attach(id, new_id, true);
        if size > 0 { self.resize(new_id, size); }
        new_id
    }

    fn insert_after(&mut self, id: usize, seg: Segment) -> usize {
        let size = seg.size;
        let mut seg = seg;
        seg.size = 0;
        let new_id = self.alloc(seg);
        self.attach(id, new_id, false);
        if size > 0 { self.resize(new_id, size); }
        new_id
    }

    fn remove(&mut self, id: usize) {
        // Zero the size first so no sums need fixing, then rotate the
        // node to a leaf and snip it
        self.resize(id, 0);
        while self.nodes[id].left != NIL || self.nodes[id].right != NIL {
            let child = if self.nodes[id].left != NIL
                && (self.nodes[id].right == NIL
                    || self.nodes[self.nodes[id].left].prio
                       >= self.nodes[self.nodes[id].right].prio) {
                self.nodes[id].left
            } else {
                self.nodes[id].right
            };
            self.rotate_up(child);
        }
        let p = self.nodes[id].parent;
        debug_assert_ne!(p, NIL, "never remove the last node");
        if self.nodes[p].left == id { self.nodes[p].left = NIL; }
        else { self.nodes[p].right = NIL; }
        self.nodes[id].parent = NIL;
    }

    fn emit(&self) -> Vec<Replacement> {
        let mut out = Vec::new();
        let mut offset = 0;
        let mut stack = Vec::new();
        let mut n = self.root;
        while n != NIL || !stack.is_empty() {
            while n != NIL { stack.push(n); n = self.nodes[n].left; }
            n = stack.pop().unwrap();
            let seg = &self.nodes[n].seg;
            match &seg.content {
                None => offset += seg.size,
                Some(c) => {
                    out.push(Replacement {
                        range: (offset..offset + seg.del).into(),
                        content: c.clone(),
                    });
                    offset += seg.del;
                }
            }
            n = self.nodes[n].right;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert_both(patches: &[Replacement]) -> Vec<Replacement> {
        let vec_out = run(&mut VecStore::new(), patches);
        let mut treap = TreapStore::new();
        let treap_out = run(&mut treap, patches);
        treap.dbg_check();
        assert_eq!(vec_out, treap_out, "stores disagree on {patches:?}");
        treap_out
    }

    fn r(s: usize, e: usize, c: &str) -> Replacement { Replacement::new(s, e, c) }

    // The five receipt cases, captured from the JS implementation
    #[test]
    fn matches_the_js_receipts() {
        assert_eq!(convert_both(&[r(5,5,"x"), r(6,6,"y")]),
                   vec![r(5,5,"xy")]);
        assert_eq!(convert_both(&[r(5,5,"abc"), r(6,7,"")]),
                   vec![r(5,5,"ac")]);
        assert_eq!(convert_both(&[r(2,2,"x"), r(9,9,"y")]),
                   vec![r(2,2,"x"), r(8,8,"y")]);
        assert_eq!(convert_both(&[r(3,6,""), r(3,3,"new")]),
                   vec![r(3,6,"new")]);
        // The vestigial zero-length scar, faithfully reproduced
        assert_eq!(convert_both(&[r(5,5,"abc"), r(5,8,"")]),
                   vec![r(5,5,"")]);
    }

    // The self-oracle: applying the absolute output to a base text must
    // equal applying the relative inputs step by step
    fn apply_relative(base: &str, patches: &[Replacement]) -> String {
        let mut text: Vec<char> = base.chars().collect();
        for p in patches {
            let insert: Vec<char> = p.content.chars().collect();
            text.splice(p.range.start..p.range.end, insert);
        }
        text.into_iter().collect()
    }

    fn apply_absolute(base: &str, patches: &[Replacement]) -> String {
        let mut text: Vec<char> = base.chars().collect();
        let mut offset: isize = 0;
        for p in patches {
            let s = (p.range.start as isize + offset) as usize;
            let e = (p.range.end as isize + offset) as usize;
            let insert: Vec<char> = p.content.chars().collect();
            offset += insert.len() as isize - (e - s) as isize;
            text.splice(s..e, insert);
        }
        text.into_iter().collect()
    }

    #[test]
    fn fuzz_apply_equivalence_quick() {
        fuzz_apply_equivalence(0..2000);
    }

    #[test]
    #[ignore]
    fn fuzz_apply_equivalence_forever() {
        let mut seed = 0;
        loop {
            fuzz_apply_equivalence(seed..seed + 100_000);
            seed += 100_000;
            println!("... through seed {seed}");
        }
    }

    fn fuzz_apply_equivalence(seeds: std::ops::Range<u64>) {
        let alphabet: Vec<char> = "abYZ é✓𐍈\u{1F600}".chars()
            .filter(|c| *c != ' ').collect();
        for seed in seeds {
            let mut rng = seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(7);
            let mut rand = move |n: usize| -> usize {
                rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17;
                if n == 0 { 0 } else { (rng as usize) % n }
            };

            let base: String = (0..rand(24)).map(|_|
                alphabet[rand(alphabet.len())]).collect();
            let base_len = base.chars().count();

            let mut cur_len = base_len;
            let mut patches = Vec::new();
            for _ in 0..rand(10) {
                let s = rand(cur_len + 1);
                let d = rand((cur_len - s).min(5) + 1);
                let content: String = (0..rand(5)).map(|_|
                    alphabet[rand(alphabet.len())]).collect();
                cur_len = cur_len - d + content.chars().count();
                patches.push(Replacement::new(s, s + d, &content));
            }

            let absolute = convert_both(&patches);
            assert_eq!(apply_relative(&base, &patches),
                       apply_absolute(&base, &absolute),
                       "seed {seed}: patches {patches:?} -> {absolute:?}");

            // Output invariants: ascending, non-overlapping, within base
            let mut last_end = 0;
            for p in &absolute {
                assert!(p.range.start >= last_end, "seed {seed}: out of order");
                assert!(p.range.end <= base_len + 5 + HUGE / 2, "sane range");
                last_end = p.range.end;
            }
        }
    }
}
