//! The Collapsing Time Machine's *Observed History* interface: reading this
//! oplog back out as the updates a peer exchanges.
//!
//! An update carries you across a span of time:
//!
//!     u = (op, parents, version)
//!
//! apply `op` to the value at `parents`, and you have the value at
//! `version`. Observed History is the set of updates a peer knows, each held
//! as its originating peer produced it — never re-expressed against some
//! other version, which is what Travel would return. That set is exactly
//! what this oplog holds, a causal graph plus the ops hung off it, but it
//! holds it in whatever run-length-encoded shape the storage layer found
//! convenient, which is not the shape a peer wants to receive. This module
//! is the projection back out.
//!
//! `updates_since(version)` yields the updates `version` has not seen:
//! everything in Observed History outside its ancestor cone.
//! `encode_at(version)` is the complement — Observed History restricted to
//! what `version` *had* seen, serialized.
//!
//! ## Granularity
//!
//! An update can span any amount of history, so the emitter gets to choose a
//! grain. The finest available here is one event: dt gives every single
//! character its own local version, and so its own `agent-seq`. Emitting at
//! that grain would be correct but needlessly verbose, so a run of
//! consecutive events is summarized into one update wherever that is
//! lossless — the same move as Summarize, at the smallest scale. A run
//! qualifies when its events are consecutive seqs by one agent, each one's
//! only parent is the previous, and their ops compose into a single op. The
//! summary then takes the parents of the run's first event and the version
//! of its last.
//!
//! A summary is only sound if the versions it swallows keep their meaning,
//! because other updates name them in their own parents. Forward delete runs
//! fail that test. Such a run deletes at the same position over and over,
//! each event removing whatever character has shifted into place; written as
//! one span delete it reads back as deleting that span in the other
//! direction, so every interior `agent-seq` would come to name a different
//! character. Forward deletes therefore stay at event grain, one update each.

use smartstring::alias::String as SmartString;
use rle::{HasLength, MergableSpan};
use crate::{Frontier, LV};
use crate::list::ListOpLog;
use crate::list::operation::{ListOpKind, TextOperation};

/// One update `u = (op, parents, version)`: apply `op` to the value at
/// `parents` to get the value at `version`. Spans a single event, or a
/// summarized run of them.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Update {
    /// `version`, as `agent-seq_end` — for a summarized run, the last
    /// event's seq; the run's earlier seqs count back from it through the
    /// length of the op. A version is in general a frontier, a *set* of
    /// event ids, but the version an update arrives at is always the single
    /// event it ends on. That is why one `agent-seq` says it here while
    /// `parents` below needs a set.
    pub agent: SmartString,
    pub seq_end: usize,
    /// `parents`, as local versions: the parents of the run's first event
    pub parents: Frontier,
    /// `op`: `Insert(pos_start, content)` when `content` is set, where
    /// `pos_end - pos_start` is the content's length; otherwise
    /// `Delete(pos_start, pos_end - pos_start)`.
    pub pos_start: usize,
    pub pos_end: usize,
    pub content: Option<SmartString>,
}

/// A run of events being accumulated into one update
struct Run {
    op: TextOperation,
    agent: SmartString,
    seq_start: usize,
    parents: Frontier,
    last_lv: LV,
}

/// Can this event join `prev`'s run? Every condition below has to hold for
/// the summary to be lossless; the run breaks wherever one fails.
fn chains(prev: &Run, agent: &str, seq_start: usize,
          parents: &Frontier, op: &TextOperation) -> bool {
    // Forward deletes stay at event grain, so they never join a run
    if op.kind == ListOpKind::Del && op.loc.fwd { return false }
    if prev.op.kind == ListOpKind::Del && prev.op.loc.fwd { return false }

    prev.agent == agent
        && seq_start == prev.seq_start + prev.op.len()
        && parents.as_ref() == [prev.last_lv]
        && prev.op.can_append(op)
}

fn emit(run: Run, out: &mut Vec<Update>) {
    let len = run.op.len();
    match (run.op.kind, run.op.loc.fwd) {
        (ListOpKind::Ins, _) => {
            out.push(Update {
                agent: run.agent,
                seq_end: run.seq_start + len - 1,
                parents: run.parents,
                pos_start: run.op.loc.span.start,
                pos_end: run.op.loc.span.end,
                content: run.op.content,
            });
        }
        (ListOpKind::Del, false) => {
            out.push(Update {
                agent: run.agent,
                seq_end: run.seq_start + len - 1,
                parents: run.parents,
                pos_start: run.op.loc.span.start,
                pos_end: run.op.loc.span.end,
                content: None,
            });
        }
        (ListOpKind::Del, true) => {
            // Unsummarized: one update per event, each chained to the last
            let pos = run.op.loc.span.start;
            let first_lv = run.last_lv + 1 - len;
            let mut parents = run.parents;
            for k in 0..len {
                out.push(Update {
                    agent: run.agent.clone(),
                    seq_end: run.seq_start + k,
                    parents,
                    pos_start: pos,
                    pos_end: pos + 1,
                    content: None,
                });
                parents = Frontier::new_1(first_lv + k);
            }
        }
    }
}

impl ListOpLog {
    /// The updates that `version` has not seen: everything in Observed
    /// History outside its ancestor cone, in causal order, with runs
    /// summarized wherever that is lossless.
    pub fn updates_since(&self, version: &[LV]) -> Vec<Update> {
        let ranges = self.cg.diff_since(version);
        let mut out = Vec::new();

        for range in ranges {
            let mut pending: Option<Run> = None;
            for (op, entry, rvs) in self.iter_full_range(range) {
                let agent = rvs.0;
                let seq_start = rvs.1.start;

                let can_chain = pending.as_ref().is_some_and(|p|
                    chains(p, agent, seq_start, &entry.parents, &op));

                if can_chain {
                    let p = pending.as_mut().unwrap();
                    p.last_lv = entry.span.last();
                    p.op.append(op);
                } else {
                    if let Some(p) = pending.take() { emit(p, &mut out); }
                    pending = Some(Run {
                        last_lv: entry.span.last(),
                        op,
                        agent: agent.into(),
                        seq_start,
                        parents: entry.parents,
                    });
                }
            }
            // Runs never continue across a diff-range boundary
            if let Some(p) = pending.take() { emit(p, &mut out); }
        }
        out
    }
}

use crate::list::encoding::EncodeOptions;

impl ListOpLog {
    /// Observed History restricted to what `version` had seen, encoded:
    /// byte-identical to what `encode` would have produced back when
    /// `version` was the tip. The complement of `encode_from`, which carries
    /// everything after a version.
    pub fn encode_at(&self, version: &[LV], opts: &EncodeOptions) -> Vec<u8> {
        // The ancestor cone of the version, as local-version ranges, in
        // ascending order so that parents always precede children. The cone
        // is closed under ancestry by construction, so every parent of an
        // encoded operation is encoded too.
        let (_, mut cone) = self.cg.graph.diff(&[], version);
        cone.sort_unstable_by_key(|r| r.start);

        self.encode_ranges(opts, &[], &cone, version)
    }

    /// encode_at by way of replaying the cone into a second oplog and
    /// serializing that. Kept as the reference the fast path is checked
    /// against; it renumbers local versions by round-tripping every parent
    /// through its remote name, which is what encode_ranges does directly.
    #[cfg(test)]
    fn encode_at_via_rebuild(&self, version: &[LV], opts: &EncodeOptions) -> Vec<u8> {
        let (_, mut cone) = self.cg.graph.diff(&[], version);
        cone.sort_unstable_by_key(|r| r.start);

        let mut rebuilt = ListOpLog::new();
        for range in cone.iter() {
            for (op, entry, rvs) in self.iter_full_range(*range) {
                let agent = rebuilt.get_or_create_agent_id(rvs.0);
                let parents: Vec<LV> = entry.parents.as_ref().iter().map(|lv| {
                    let rv = self.cg.agent_assignment.local_to_remote_version(*lv);
                    rebuilt.cg.agent_assignment
                        .try_remote_to_local_version(rv)
                        .expect("parent of an included version is included")
                }).collect();
                rebuilt.add_operations_remote(agent, &parents, rvs.1.start, &[op]);
            }
        }
        rebuilt.encode(opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two rle-merged inserts by one agent summarize into a single update;
    // a concurrent delete by another agent is its own
    #[test]
    fn summarizes_a_run_and_breaks_at_a_fork() {
        let mut oplog = ListOpLog::new();
        let a = oplog.get_or_create_agent_id("a");
        let b = oplog.get_or_create_agent_id("b");
        oplog.add_insert_at(a, &[], 0, "hh");   // lv 0..2
        oplog.add_insert_at(a, &[1], 2, "ddd"); // lv 2..5, chains with hh
        oplog.add_delete_at(b, &[1], 0..1);     // lv 5..6, concurrent

        let updates = oplog.updates_since(&[]);
        assert_eq!(updates.len(), 2);

        assert_eq!(updates[0].agent, "a");
        assert_eq!(updates[0].seq_end, 4);
        assert_eq!(updates[0].parents.as_ref(), &[] as &[LV]);
        // Inserts report the position span their content covers
        assert_eq!((updates[0].pos_start, updates[0].pos_end), (0, 5));
        assert_eq!(updates[0].content.as_deref(), Some("hhddd"));

        assert_eq!(updates[1].agent, "b");
        assert_eq!(updates[1].seq_end, 0);
        assert_eq!(updates[1].parents.as_ref(), &[1]);
        assert_eq!((updates[1].pos_start, updates[1].pos_end), (0, 1));
        assert_eq!(updates[1].content, None);
    }

    // Forward deletes (what dt's own local .delete() creates) stay
    // at event grain: one update per event, all at the same position
    #[test]
    fn forward_delete_emits_per_event() {
        let mut oplog = ListOpLog::new();
        let a = oplog.get_or_create_agent_id("a");
        oplog.add_insert_at(a, &[], 0, "abcd");  // lv 0..4
        oplog.add_delete_at(a, &[3], 1..4);      // lv 4..7, fwd delete

        let updates = oplog.updates_since(&[]);
        assert_eq!(updates.len(), 4);
        assert_eq!(updates[0].content.as_deref(), Some("abcd"));

        for (k, u) in updates[1..].iter().enumerate() {
            assert_eq!((u.pos_start, u.pos_end), (1, 2));
            assert_eq!(u.seq_end, 4 + k);
            assert_eq!(u.parents.as_ref(), &[3 + k]);
        }
    }

    // Rebuild a doc from its own updates and compare: where every run
    // summarizes losslessly (fwd inserts, backward deletes) the oplogs
    // must be byte-identical
    fn round_trips_bytes(oplog: &ListOpLog) {
        let updates = oplog.updates_since(&[]);

        let mut rebuilt = ListOpLog::new();
        for u in &updates {
            let agent = rebuilt.get_or_create_agent_id(&u.agent);
            let len = u.content.as_ref().map_or(u.pos_end - u.pos_start,
                                                |c| c.chars().count());
            let seq_start = u.seq_end + 1 - len;
            let op = if let Some(content) = &u.content {
                TextOperation::new_insert(u.pos_start, content)
            } else {
                let mut op = TextOperation::new_delete(u.pos_start .. u.pos_end);
                op.loc.fwd = false;
                op
            };
            // Emission order equals local order, so parents transfer as-is
            rebuilt.add_operations_remote(agent, u.parents.as_ref(), seq_start, &[op]);
        }

        assert_eq!(oplog.encode(&crate::list::encoding::ENCODE_FULL),
                   rebuilt.encode(&crate::list::encoding::ENCODE_FULL));
    }

    // The run-joining predicate, enumerated dimension by dimension: only the
    // all-good row may chain
    #[test]
    fn chains_dimension_table() {
        fn seg(agent: &str, seq_start: usize, len: usize, last_lv: LV) -> Run {
            Run {
                op: TextOperation::new_insert(0, &"x".repeat(len)),
                agent: agent.into(),
                seq_start,
                parents: Frontier::root(),
                last_lv,
            }
        }

        let prev = seg("a", 10, 2, 7);   // covers seqs 10..12, ends at lv 7
        let good_op = TextOperation::new_insert(2, "y");

        for (same_agent, seq_contig, parents_chain, pos_append, expect) in [
            (true,  true,  true,  true,  true),
            (false, true,  true,  true,  false),
            (true,  false, true,  true,  false),
            (true,  true,  false, true,  false),
            (true,  true,  true,  false, false),
        ] {
            let agent = if same_agent { "a" } else { "b" };
            let seq = if seq_contig { 12 } else { 13 };
            let parents = if parents_chain { Frontier::new_1(7) }
                          else { Frontier::new_1(3) };
            let op = if pos_append { good_op.clone() }
                     else { TextOperation::new_insert(9, "y") };
            assert_eq!(chains(&prev, agent, seq, &parents, &op), expect,
                       "agent={same_agent} seq={seq_contig} parents={parents_chain} pos={pos_append}");
        }

        // Forward deletes never chain, in either role
        let fwd_del = TextOperation::new_delete(2..3);
        assert!(!chains(&prev, "a", 12, &Frontier::new_1(7), &fwd_del));
        let mut prev_del = seg("a", 10, 1, 7);
        prev_del.op = TextOperation::new_delete(2..3);
        assert!(!chains(&prev_del, "a", 11, &Frontier::new_1(7),
                        &TextOperation::new_delete(2..3)));
    }

    // A positional run crossing an agent boundary must break there: events
    // by different agents are never one update. The validity
    // assertion: the ops list really did rle-merge the two ops into one
    // run, so the break is the predicate's doing
    #[test]
    fn cross_actor_run_splits() {
        let mut oplog = ListOpLog::new();
        let a = oplog.get_or_create_agent_id("a");
        let b = oplog.get_or_create_agent_id("b");
        oplog.add_insert_at(a, &[], 0, "ab");   // lv 0..2
        oplog.add_insert_at(b, &[1], 2, "cd");  // lv 2..4, positionally contiguous

        assert_eq!(oplog.iter_ops().count(), 1, "validity: ops rle-merged across agents");

        let updates = oplog.updates_since(&[]);
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].agent, "a");
        assert_eq!(updates[1].agent, "b");
        assert_eq!(updates[1].parents.as_ref(), &[1]);
    }

    // A seq jump within one agent must break the run, even when everything
    // else chains: seqs count events, so a gap means the two sides are not
    // consecutive events and their seqs cannot be recovered from one version
    #[test]
    fn seq_jump_splits() {
        let mut oplog = ListOpLog::new();
        let a = oplog.get_or_create_agent_id("a");
        oplog.add_operations_remote(a, &[], 0, &[TextOperation::new_insert(0, "ab")]);
        oplog.add_operations_remote(a, &[1], 5, &[TextOperation::new_insert(2, "cd")]);

        assert_eq!(oplog.iter_ops().count(), 1, "validity: ops rle-merged across the gap");

        let updates = oplog.updates_since(&[]);
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].seq_end, 1);
        assert_eq!(updates[1].seq_end, 6);
    }

    // Summarizing leans on a dt invariant: reverse runs only exist for
    // deletes (see can_append_ops), so insert content is always in doc
    // order. Typing right to left stays two separate runs
    #[test]
    fn rev_insert_runs_cannot_form() {
        let mut oplog = ListOpLog::new();
        let a = oplog.get_or_create_agent_id("a");
        oplog.add_insert_at(a, &[], 0, "b");
        oplog.add_insert_at(a, &[0], 0, "a");

        let runs: Vec<_> = oplog.iter_ops().collect();
        assert_eq!(runs.len(), 2, "rev inserts must not merge into one run");

        // And each becomes its own update, chained
        let updates = oplog.updates_since(&[]);
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].content.as_deref(), Some("b"));
        assert_eq!(updates[1].content.as_deref(), Some("a"));
        assert_eq!(updates[1].parents.as_ref(), &[0]);
    }

    // Rebuild an arbitrary doc from its updates: the text and version
    // frontier must always survive. (Bytes can differ: fwd delete runs
    // rebuild as per-event backward deletes.)
    // Returns (had_fwd_multichar_del, had_cross_actor, had_rev_del)
    fn semantic_round_trip(oplog: &ListOpLog) -> (bool, bool, bool) {
        let had_fwd_del = oplog.iter_ops().any(|op|
            op.kind == ListOpKind::Del && op.loc.fwd && op.len() > 1);
        let had_rev_del = oplog.iter_ops().any(|op|
            op.kind == ListOpKind::Del && !op.loc.fwd && op.len() > 1);

        let mut prev_agent: Option<SmartString> = None;
        let mut had_cross_actor = false;
        for (_op, _entry, rvs) in oplog.iter_full_range((0..oplog.len()).into()) {
            if let Some(pa) = &prev_agent {
                if pa != rvs.0 { had_cross_actor = true; }
            }
            prev_agent = Some(rvs.0.into());
        }

        let updates = oplog.updates_since(&[]);
        let mut rebuilt = ListOpLog::new();
        for u in &updates {
            let agent = rebuilt.get_or_create_agent_id(&u.agent);
            let len = u.content.as_ref().map_or(u.pos_end - u.pos_start,
                                                |c| c.chars().count());
            let seq_start = u.seq_end + 1 - len;
            let op = if let Some(content) = &u.content {
                TextOperation::new_insert(u.pos_start, content)
            } else {
                let mut op = TextOperation::new_delete(u.pos_start .. u.pos_end);
                op.loc.fwd = false;
                op
            };
            rebuilt.add_operations_remote(agent, u.parents.as_ref(), seq_start, &[op]);
        }

        // Version frontiers must match unconditionally
        let f = |log: &ListOpLog| {
            let mut v: Vec<String> = log.remote_frontier().iter()
                .map(|rv| format!("{}-{}", rv.0, rv.1)).collect();
            v.sort();
            v
        };
        assert_eq!(f(oplog), f(&rebuilt), "remote frontier must survive");

        assert_eq!(oplog.checkout_tip().content().to_string(),
                   rebuilt.checkout_tip().content().to_string(),
                   "text must survive the round trip");
        (had_fwd_del, had_cross_actor, had_rev_del)
    }

    #[cfg(feature = "gen_test_data")]
    fn gen_oplog_corpus(seeds: std::ops::Range<u64>, steps: usize) {
        let mut stats = (0, 0, 0, 0);
        for seed in seeds {
            let oplog = crate::list::gen_oplog(seed, steps,
                                               seed % 2 == 0, seed % 3 != 0);
            check_encode_at(&oplog);
            let (fwd_del, cross, rev_del) = semantic_round_trip(&oplog);
            stats.0 += 1;
            if fwd_del { stats.1 += 1 }
            if cross { stats.2 += 1 }
            if rev_del { stats.3 += 1 }
        }
        // Validity: the corpus must actually exercise the nasty dimensions
        assert!(stats.1 > 0, "corpus never produced a fwd multichar delete");
        assert!(stats.2 > 0, "corpus never produced a cross-actor run");
        println!("corpus: {} docs, {} with fwd multichar dels, {} with cross-actor runs, {} with rev dels",
                 stats.0, stats.1, stats.2, stats.3);
    }

    #[cfg(feature = "gen_test_data")]
    #[test]
    fn gen_oplog_round_trip_quick() {
        gen_oplog_corpus(0..30, 20);
    }

    #[cfg(feature = "gen_test_data")]
    #[test]
    #[ignore]
    fn gen_oplog_round_trip_forever() {
        let mut seed = 0;
        loop {
            gen_oplog_corpus(seed..seed + 100, 50);
            seed += 100;
            println!("... through seed {seed}");
        }
    }

    // encode_at's contract, checked at every single-head frontier plus
    // the tip:
    //  - at the tip, byte-identical to encode()
    //  - trimmed doc's text and version match checkout at that frontier
    //  - trimmed doc + the patch since = the full doc (the partition)
    fn check_encode_at(oplog: &ListOpLog) {
        let opts = &crate::list::encoding::ENCODE_FULL;
        let full = oplog.encode(opts);
        assert_eq!(oplog.encode_at(oplog.local_frontier_ref(), opts), full,
                   "tip must be byte-identical");

        for lv in (0..oplog.len()).step_by(3) {
            let frontier = [lv];
            let bytes = oplog.encode_at(&frontier, opts);
            let mut trimmed = ListOpLog::load_from(&bytes).unwrap();

            assert_eq!(trimmed.checkout_tip().content().to_string(),
                       oplog.checkout(&frontier).content().to_string(),
                       "trimmed text at {lv}");

            trimmed.decode_and_add(
                &oplog.encode_from(&crate::list::encoding::ENCODE_PATCH, &frontier))
                .unwrap();
            assert_eq!(trimmed.checkout_tip().content().to_string(),
                       oplog.checkout_tip().content().to_string(),
                       "prefix + patch must rebuild the text at {lv}");
            let rf = |o: &ListOpLog| {
                let mut v: Vec<String> = o.remote_frontier().iter()
                    .map(|rv| format!("{}-{}", rv.0, rv.1)).collect();
                v.sort();
                v
            };
            assert_eq!(rf(&trimmed), rf(oplog),
                       "prefix + patch must rebuild the frontier at {lv}");
        }
    }

    #[test]
    fn round_trip_summarizable_quick() {
        summarizable_seeds(0..50);
    }

    #[test]
    #[ignore]
    fn round_trip_summarizable_forever() {
        let mut seed = 0;
        loop {
            summarizable_seeds(seed..seed + 500);
            seed += 500;
            println!("... through seed {seed}");
        }
    }

    // Deterministic multi-actor histories built only from fwd inserts and
    // backward deletes, with explicit seqs and partial concurrency: the
    // shape where every run summarizes losslessly, so the rebuild must come
    // back byte-exact
    fn summarizable_seeds(seeds: std::ops::Range<u64>) {
        for seed in seeds {
            let mut rng = seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(1);
            let mut rand = move |n: usize| {
                rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17;
                (rng as usize) % n
            };

            let mut oplog = ListOpLog::new();
            let agents = ["al-ice", "bob", "carol"];
            let mut seqs = [0usize; 3];
            let mut frontier: Frontier = Frontier::root();

            for _ in 0..30 {
                let ai = rand(3);
                let agent = oplog.get_or_create_agent_id(agents[ai]);
                let doc_len = oplog.checkout_tip().len();
                let last = if rand(4) == 0 || frontier.is_root() {
                    // sometimes branch from an older frontier
                    frontier.clone()
                } else {
                    Frontier::new_1(oplog.len() - 1)
                };

                let count = if doc_len > 2 && rand(3) == 0 {
                    let start = rand(doc_len - 1);
                    let len = 1 + rand((doc_len - start).min(3));
                    let mut op = TextOperation::new_delete(start .. start + len);
                    op.loc.fwd = false;
                    oplog.add_operations_remote(agent, last.as_ref(), seqs[ai], &[op]);
                    len
                } else {
                    let pos = rand(doc_len + 1);
                    let s = "abcdefgh"[rand(8)..].chars().next().unwrap()
                        .to_string().repeat(1 + rand(3));
                    let count = s.chars().count();
                    let op = TextOperation::new_insert(pos, &s);
                    oplog.add_operations_remote(agent, last.as_ref(), seqs[ai], &[op]);
                    count
                };
                seqs[ai] += count;
                frontier = oplog.local_frontier();
            }

            round_trips_bytes(&oplog);
            check_encode_at(&oplog);
        }
    }
}

#[cfg(test)]
mod encode_at_tests {
    use crate::LV;
    use crate::list::ListOpLog;
    use crate::list::encoding::ENCODE_FULL;

    fn next(seed: &mut u64) -> usize {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (*seed >> 33) as usize
    }

    /// A document with concurrent branches, so ancestor cones have gaps and
    /// the encoded output really does need its local versions renumbered.
    fn tangled_oplog(rounds: usize, window: usize, seed: &mut u64) -> (ListOpLog, Vec<Vec<LV>>) {
        let mut oplog = ListOpLog::new();
        let agents: Vec<_> = ["a", "b", "c"].iter()
            .map(|n| oplog.get_or_create_agent_id(n)).collect();
        let mut frontier: Vec<LV> = vec![];
        let mut len = 0usize;
        let mut checkpoints = vec![];

        for round in 0..rounds {
            let round_start = frontier.clone();
            let round_len = len;
            let mut tips = vec![];

            // A different pair of agents branches each round
            for &agent in [agents[round % 3], agents[(round + 1) % 3]].iter() {
                let mut parents = round_start.clone();
                let mut branch_len = round_len;
                for _ in 0..window {
                    if branch_len > 4 && next(seed) % 4 == 0 {
                        let pos = next(seed) % (branch_len - 1);
                        branch_len -= 1;
                        parents = vec![oplog.add_delete_at(agent, &parents, pos..pos + 1)];
                    } else {
                        let pos = next(seed) % (branch_len + 1);
                        branch_len += 3;
                        parents = vec![oplog.add_insert_at(agent, &parents, pos, "abc")];
                    }
                }
                tips.push(parents[0]);
                checkpoints.push(parents.clone());
            }

            tips.sort_unstable();
            len = oplog.checkout(&tips).len();
            frontier = tips.clone();
            checkpoints.push(tips);
        }

        (oplog, checkpoints)
    }

    /// encode_ranges must produce exactly what replaying the cone into a fresh
    /// oplog produced, at every version -- this is a wire format, so "close
    /// enough" isn't.
    #[test]
    fn encode_at_matches_the_rebuild() {
        let mut seed = 5150;
        let (oplog, checkpoints) = tangled_oplog(12, 7, &mut seed);

        for version in checkpoints.iter().chain(std::iter::once(&oplog.cg.version.as_ref().to_vec())) {
            let fast = oplog.encode_at(version, &ENCODE_FULL);
            let reference = oplog.encode_at_via_rebuild(version, &ENCODE_FULL);
            assert_eq!(fast, reference, "encoded bytes differ at {version:?}");
        }
    }

    /// ...and the bytes have to load back into the document that version saw.
    #[test]
    fn encode_at_round_trips() {
        let mut seed = 9001;
        let (oplog, checkpoints) = tangled_oplog(10, 5, &mut seed);

        for version in checkpoints.iter() {
            let bytes = oplog.encode_at(version, &ENCODE_FULL);
            let loaded = ListOpLog::load_from(&bytes).unwrap();
            assert_eq!(loaded.checkout_tip().content().to_string(),
                       oplog.checkout(version).content().to_string(),
                       "reloaded document differs at {version:?}");
        }
    }
}
