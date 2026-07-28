use rle::HasLength;

use crate::{DTRange, LV};
use crate::frontier::FrontierRef;
use crate::list::{ListBranch, ListOpLog};
use crate::list::op_metrics::ListOpMetrics;
use crate::list::operation::{ListOpKind, TextOperation};
use crate::list::relative_to_absolute::{Replacement, relative_to_absolute_patches};
use smartstring::alias::String as SmartString;
use crate::listmerge::merge::{reverse_str, TransformedOpsIterRaw, TransformedResultRaw, TransformedSimpleOp, TransformedSimpleOpsIter};
use crate::listmerge::merge::TransformedResult::{BaseMoved, DeleteAlreadyHappened};
use crate::listmerge::plan::M1PlanAction;
use crate::rle::KVPair;

impl ListOpLog {
    pub fn dbg_bench_make_plan(&self) {
        self.cg.graph.make_m1_plan(Some(&self.operations), &[], self.cg.version.as_ref(), false);
    }

    pub(crate) fn get_xf_operations_full(&self, from: FrontierRef, merging: FrontierRef) -> TransformedOpsIterRaw<'_> {
        TransformedOpsIterRaw::new(&self.cg.graph, &self.cg.agent_assignment,
                                &self.operation_ctx, &self.operations,
                                from, merging)
    }

    /// Iterate through all the *transformed* operations from some point in time. Internally, the
    /// OpLog stores all changes as they were when they were created. This makes a lot of sense from
    /// CRDT academic point of view (and makes signatures and all that easy). But its is rarely
    /// useful for a text editor.
    ///
    /// `get_xf_operations` returns an iterator over the *transformed changes*. That is, the set of
    /// changes that could be applied linearly to a document to bring it up to date.
    pub fn iter_xf_operations_from(&self, from: FrontierRef, merging: FrontierRef) -> impl Iterator<Item=(DTRange, Option<TextOperation>)> + '_ {
        let iter: TransformedSimpleOpsIter = self.get_xf_operations_full(from, merging).into();

        iter.map(|result| {
            match result {
                TransformedSimpleOp::Apply(KVPair(start, op)) => {
                    let content = op.get_content(&self.operation_ctx);
                    let len = op.len();
                    let text_op: TextOperation = (op, content).into();
                    ((start..start + len).into(), Some(text_op))
                }
                TransformedSimpleOp::DeleteAlreadyHappened(range) => (range, None)
            }
        })
    }

    /// Get all transformed operations from the start of time.
    ///
    /// This is a shorthand for `oplog.get_xf_operations(&[], oplog.local_version)`, but
    /// I hope that future optimizations make this method way faster.
    ///
    /// See [OpLog::iter_xf_operations_from](OpLog::iter_xf_operations_from) for more information.
    pub fn iter_xf_operations(&self) -> impl Iterator<Item=(DTRange, Option<TextOperation>)> + '_ {
        self.iter_xf_operations_from(&[], self.cg.version.as_ref())
    }

    /// The transformed changes since `frontier`, as replacements in
    /// absolute coordinates of the text at `frontier`. This is what
    /// braid-text broadcasts to simpleton clients.
    pub fn xf_absolute_since(&self, frontier: &[LV]) -> Vec<Replacement> {
        let rel: Vec<Replacement> = self
            .iter_xf_operations_from(frontier, self.cg.version.as_ref())
            .filter_map(|(_, op)| op)
            .map(|op| match op.kind {
                ListOpKind::Ins => Replacement {
                    range: (op.loc.span.start..op.loc.span.start).into(),
                    content: op.content.unwrap_or_default(),
                },
                ListOpKind::Del => Replacement {
                    range: op.loc.span,
                    content: SmartString::new(),
                },
            })
            .collect();
        relative_to_absolute_patches(&rel)
    }

    /// The document's length in characters as of some version.
    ///
    /// This walks the same transformed operations that a checkout would
    /// apply, but only tallies their lengths, so it never builds the text
    /// itself. Concurrent deletes of the same character are counted once,
    /// since the transform reports the redundant half as already deleted.
    pub fn len_at(&self, version: FrontierRef) -> usize {
        let mut len = 0usize;

        for xf in self.get_xf_operations_full(&[], version) {
            match xf {
                TransformedResultRaw::Apply { op: KVPair(_, op), .. } => {
                    match op.kind {
                        ListOpKind::Ins => len += op.len(),
                        ListOpKind::Del => len -= op.len(),
                    }
                }

                // A fast-forwarded span needs no transforming, so its ops
                // are read straight out of the oplog.
                TransformedResultRaw::FF(range) => {
                    for KVPair(_, op) in self.operations.iter_range_ctx(range, &self.operation_ctx) {
                        match op.kind {
                            ListOpKind::Ins => len += op.len(),
                            ListOpKind::Del => len -= op.len(),
                        }
                    }
                }

                TransformedResultRaw::DeleteAlreadyHappened(_) => {}
            }
        }

        len
    }

    #[cfg(feature = "merge_conflict_checks")]
    pub fn has_conflicts_when_merging(&self) -> bool {
        let mut iter = TransformedOpsIterRaw::new(&self.cg.graph, &self.cg.agent_assignment,
                                               &self.operation_ctx, &self.operations,
                                               &[], self.cg.version.as_ref());
        // let mut iter = TransformedOpsIter::new(&self.cg.graph, &self.cg.agent_assignment,
        //                                        &self.operation_ctx, &self.operations,
        //                                        &[], self.cg.version.as_ref());
        for _ in &mut iter {}
        iter.concurrent_inserts_collided()
    }

    pub fn dbg_iter_xf_operations_no_ff(&self) -> impl Iterator<Item=(DTRange, Option<TextOperation>)> + '_ {
        let (plan, _common) = self.cg.graph.make_m1_plan(Some(&self.operations), &[], self.cg.version.as_ref(), false);
        let iter: TransformedSimpleOpsIter = TransformedOpsIterRaw::from_plan(&self.cg.agent_assignment,
                                                                              &self.operation_ctx, &self.operations,
                                                                              plan)
            .into();

        iter.map(|result| {
            match result {
                TransformedSimpleOp::Apply(KVPair(start, op)) => {
                    let content = op.get_content(&self.operation_ctx);
                    let len = op.len();
                    let text_op: TextOperation = (op, content).into();
                    ((start..start + len).into(), Some(text_op))
                }
                TransformedSimpleOp::DeleteAlreadyHappened(range) => (range, None)
            }
        })

        // // allow_ff: false!
        // let (plan, common) = self.cg.graph.make_m1_plan(Some(&self.operations), &[], self.cg.version.as_ref(), false);
        // let iter = TransformedOpsIter::from_plan(&self.cg.graph, &self.cg.agent_assignment,
        //                                              &self.operation_ctx, &self.operations,
        //                                              plan, common);
        //
        // // Return the data in the same format as iter_xf_operations_from to make benchmarks fair.
        // iter.map(|(lv, mut origin_op, xf)| {
        //     let len = origin_op.len();
        //     let op: Option<TextOperation> = match xf {
        //         BaseMoved(base) => {
        //             origin_op.loc.span = (base..base+len).into();
        //             let content = origin_op.get_content(&self.operation_ctx);
        //             Some((origin_op, content).into())
        //         }
        //         DeleteAlreadyHappened => None,
        //     };
        //     ((lv..lv +len).into(), op)
        // })
    }

    pub fn get_ff_stats(&self) -> (usize, usize, usize) {
        let (plan, _common) = self.cg.graph.make_m1_plan(Some(&self.operations), &[], self.cg.version.as_ref(), true);

        let mut normal_advances = 0;
        let mut clears = 0;
        let mut ff = 0;

        for a in &plan.0 {
            match a {
                M1PlanAction::Apply(span) => { normal_advances += span.len(); }
                M1PlanAction::Clear => { clears += 1; }
                M1PlanAction::FF(span) => { ff += span.len(); }
                _ => {}
            }
        }

        (clears, normal_advances, ff)
    }
}


impl ListBranch {
    #[inline(always)]
    fn apply_op_at(&mut self, oplog: &ListOpLog, op: ListOpMetrics) {
        // let xf_pos = op.loc.span.start;
        match op.kind {
            ListOpKind::Ins => {
                let content = oplog.operation_ctx.get_str(ListOpKind::Ins, op.content_pos.unwrap());
                // assert!(pos <= self.content.len_chars());
                if op.loc.fwd {
                    self.content.insert(op.loc.span.start, content);
                } else {
                    // We need to insert the content in reverse order.
                    let c = reverse_str(content);
                    self.content.insert(op.loc.span.start, &c);
                }
            }
            ListOpKind::Del => {
                self.content.remove(op.loc.span.into());
            }
        }
    }

    pub fn merge(&mut self, oplog: &ListOpLog, merge_frontier: &[LV]) {
        // let mut iter = oplog.get_xf_operations_full_raw(self.version.as_ref(), merge_frontier).merge_spans();
        let iter = oplog.get_xf_operations_full(self.version.as_ref(), merge_frontier);
        // println!("merge '{}' at {:?} + {:?}", self.content.to_string(), self.version, merge_frontier);

        for xf in iter {
            // dbg!(&xf);
            // dbg!(_lv, &origin_op, &xf);
            match xf {
                TransformedResultRaw::Apply { xf_pos, op: KVPair(_, mut op) } => {
                    // dbg!(&op);
                    op.transpose_to(xf_pos);
                    self.apply_op_at(oplog, op);
                }

                TransformedResultRaw::FF(range) => {
                    // Activate *SUPER FAST MODE*.
                    for KVPair(_, op) in oplog.operations.iter_range_ctx(range, &oplog.operation_ctx) {
                        // dbg!(&op);
                        self.apply_op_at(oplog, op);
                    }
                }

                TransformedResultRaw::DeleteAlreadyHappened(_) => {} // Discard.
            }
        }
        
        self.version = oplog.cg.graph.find_dominators_2(self.version.as_ref(), merge_frontier);
    }
}
#[cfg(test)]
mod test {
    use crate::list::ListOpLog;

    /// len_at tallies the transformed ops rather than replaying them, so it
    /// has to agree with a checkout at every version -- including ones where
    /// concurrent deletes overlap, and the same character is deleted twice.
    #[test]
    fn len_at_matches_checkout() {
        let mut oplog = ListOpLog::new();
        let a = oplog.get_or_create_agent_id("a");
        let b = oplog.get_or_create_agent_id("b");

        let base = oplog.add_insert_at(a, &[], 0, "hello world");

        // Both agents branch from `base` and delete overlapping ranges.
        let a_del = oplog.add_delete_at(a, &[base], 0..6);  // "hello "
        let b_del = oplog.add_delete_at(b, &[base], 3..8);  // "lo wo"

        // ...and then one of them types again on top of the merge.
        let tip = oplog.add_insert_at(a, &[a_del, b_del], 1, "XY");

        for version in [vec![base], vec![a_del], vec![b_del],
                        vec![a_del, b_del], vec![tip]] {
            assert_eq!(oplog.len_at(&version), oplog.checkout(&version).len(),
                       "length disagrees at {version:?}");
        }
    }
}
