use std::cmp::Ordering;
use std::collections::HashMap;
use smartstring::alias::String as SmartString;
use rle::HasLength;
use crate::causalgraph::agent_span::{AgentSpan, AgentVersion};
use crate::{AgentId, DTRange, LV};
use crate::rle::{KVPair, RleVec};

pub mod remote_ids;

#[derive(Clone, Debug)]
pub(crate) struct ClientData {
    /// Used to map from client's name / hash to its numerical ID.
    pub(crate) name: SmartString,

    /// This is a packed RLE in-order list of all operations from this client.
    ///
    /// Each entry in this list is grounded at the client's sequence number and maps to the span of
    /// local time entries.
    ///
    /// A single agent ID might be used to modify multiple concurrent branches. Because of this, and
    /// the propensity of diamond types to reorder operations for performance, the
    /// time spans here will *almost* always (but not always) be monotonically increasing. Eg, they
    /// might be ordered as (0, 2, 1). This will only happen when changes are concurrent. The order
    /// of time spans must always obey the partial order of changes. But it will not necessarily
    /// agree with the order amongst time spans.
    pub(crate) lv_for_seq: RleVec<KVPair<DTRange>>,
}

#[derive(Debug, Clone, Default)]
pub struct AgentAssignment {

    /// This is a bunch of ranges of (local version -> CRDT location span).
    /// The entries always have positive len.
    ///
    /// This is used to map Local versions to remote CRDT IDs.
    ///
    /// List is packed.
    pub(crate) client_with_lv: RleVec<KVPair<AgentSpan>>,
    // pub(crate) client_with_lv: RlePackedVec<>
    
    /// For each client, we store some data (above). This is indexed by AgentId.
    ///
    /// This is used to map external CRDT locations -> Order numbers.
    pub(crate) client_data: Vec<ClientData>,

    /// Agent name -> AgentId. Resolving a remote version starts by naming its
    /// author, and documents can accumulate a lot of authors (a peer that mints
    /// a fresh agent per session will have thousands), so this needs to not be
    /// a scan over client_data.
    agent_ids: HashMap<SmartString, AgentId>,
}


impl ClientData {
    pub fn get_next_seq(&self) -> usize {
        self.lv_for_seq.end()
    }

    pub fn is_empty(&self) -> bool {
        self.lv_for_seq.is_empty()
    }

    #[inline]
    pub(crate) fn try_seq_to_lv(&self, seq: usize) -> Option<LV> {
        let (entry, offset) = self.lv_for_seq.find_with_offset(seq)?;
        Some(entry.1.start + offset)
    }

    pub(crate) fn seq_to_lv(&self, seq: usize) -> LV {
        self.try_seq_to_lv(seq).unwrap()
    }

    /// Note the returned timespan might be shorter than seq_range.
    pub fn try_seq_to_lv_span(&self, seq_range: DTRange) -> Option<DTRange> {
        let (KVPair(_, entry), offset) = self.lv_for_seq.find_with_offset(seq_range.start)?;

        let start = entry.start + offset;
        let end = usize::min(entry.end, start + seq_range.len());
        Some(DTRange { start, end })
    }

    pub fn seq_to_time_span(&self, seq_range: DTRange) -> DTRange {
        self.try_seq_to_lv_span(seq_range).unwrap()
    }
}

pub const MAX_AGENT_NAME_LENGTH: usize = 50;

impl AgentAssignment {
    pub fn new() -> Self { Self::default() }

    pub fn get_agent_id(&self, name: &str) -> Option<AgentId> {
        self.agent_ids.get(name).copied()
    }

    pub fn get_or_create_agent_id(&mut self, name: &str) -> AgentId {
        // TODO: -> Result or something so this can be handled.
        if name == "ROOT" { panic!("Agent ID 'ROOT' is reserved"); }

        assert!(name.len() < MAX_AGENT_NAME_LENGTH, "Agent name cannot exceed {MAX_AGENT_NAME_LENGTH} UTF8 bytes");

        if let Some(id) = self.get_agent_id(name) {
            id
        } else {
            // Create a new id.
            self.client_data.push(ClientData {
                name: SmartString::from(name),
                lv_for_seq: RleVec::new()
            });
            let id = (self.client_data.len() - 1) as AgentId;
            self.agent_ids.insert(SmartString::from(name), id);
            id
        }
    }

    /// The run of sequence numbers from `agent` that this document holds and
    /// that overlaps `seq_range`, or None if it holds none of that range.
    ///
    /// The returned span is the whole contiguous run, which may extend past
    /// `seq_range` in both directions. Note an author's sequence numbers stay
    /// contiguous even when the local versions they map to do not, which
    /// happens whenever their edits were interleaved with somebody else's, so
    /// a single run can span several stored entries.
    pub fn known_seq_span(&self, agent: AgentId, seq_range: DTRange) -> Option<DTRange> {
        if seq_range.is_empty() { return None; }
        let entries = &self.client_data.get(agent as usize)?.lv_for_seq.0;

        let seq_end = |e: &KVPair<DTRange>| e.0 + e.1.len();

        // The last entry beginning at or before the end of the query
        let after = entries.partition_point(|e| e.0 <= seq_range.last());
        if after == 0 { return None; }
        let mut i = after - 1;
        if seq_range.start >= seq_end(&entries[i]) { return None; }

        let mut start = entries[i].0;
        let mut end = seq_end(&entries[i]);
        while i > 0 && seq_end(&entries[i - 1]) == start {
            i -= 1;
            start = entries[i].0;
        }
        let mut j = after;
        while j < entries.len() && entries[j].0 == end {
            end = seq_end(&entries[j]);
            j += 1;
        }

        Some((start..end).into())
    }

    /// Drop every agent from `keep` onward. Used when a partly-decoded file is
    /// rolled back.
    pub(crate) fn truncate_agents(&mut self, keep: usize) {
        for client in self.client_data.drain(keep..) {
            self.agent_ids.remove(&client.name);
        }
    }

    /// Returns the agent name (as a &str) for a given agent_id. This is fast (O(1)).
    pub fn get_agent_name(&self, agent: AgentId) -> &str {
        self.client_data[agent as usize].name.as_str()
    }

    /// Iterates over the local version mappings for the specified agent. The iterator returns
    /// triples of (seq_start, lv_start, length).
    ///
    /// So, seq_start..seq_start+len maps to lv_start..lv_start+len
    ///
    /// The items returned will always be in sequence order.
    pub fn iter_lv_map_for_agent(&self, agent: AgentId) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
        self.client_data[agent as usize].lv_for_seq.iter()
            .map(|KVPair(seq, lv_range)| { (*seq, lv_range.start, lv_range.len()) })
    }

    pub fn len(&self) -> usize {
        self.client_with_lv.end()
    }

    pub fn is_empty(&self) -> bool {
        self.client_with_lv.is_empty()
    }

    pub fn local_to_agent_version(&self, version: LV) -> AgentVersion {
        debug_assert_ne!(version, usize::MAX);
        self.client_with_lv.get(version)
    }

    pub(crate) fn local_span_to_agent_span(&self, version: DTRange) -> AgentSpan {
        debug_assert_ne!(version.start, usize::MAX);

        let (loc, offset) = self.client_with_lv.find_packed_with_offset(version.start);
        let start = loc.1.seq_range.start + offset;
        let end = usize::min(loc.1.seq_range.end, start + version.len());
        AgentSpan {
            agent: loc.1.agent,
            seq_range: DTRange { start, end }
        }
    }

    pub(crate) fn try_agent_version_to_lv(&self, (agent, seq): AgentVersion) -> Option<LV> {
        debug_assert_ne!(agent, AgentId::MAX);

        self.client_data.get(agent as usize).and_then(|c| {
            c.try_seq_to_lv(seq)
        })
    }

    /// span is the local versions we're assigning to the named agent.
    pub(crate) fn assign_lv_to_client_next_seq(&mut self, agent: AgentId, span: DTRange) {
        debug_assert_eq!(span.start, self.len());

        let client_data = &mut self.client_data[agent as usize];

        let next_seq = client_data.get_next_seq();
        client_data.lv_for_seq.push(KVPair(next_seq, span));

        self.client_with_lv.push(KVPair(span.start, AgentSpan {
            agent,
            seq_range: DTRange { start: next_seq, end: next_seq + span.len() },
        }));
    }

    /// This is used to break ties.
    pub fn tie_break_agent_versions(&self, v1: AgentVersion, v2: AgentVersion) -> Ordering {
        if v1 == v2 { Ordering::Equal }
        else {
            let c1 = &self.client_data[v1.0 as usize];
            let c2 = &self.client_data[v2.0 as usize];

            c1.name.cmp(&c2.name)
                .then(v1.1.cmp(&v2.1))
        }
    }

    pub fn tie_break_versions(&self, v1: LV, v2: LV) -> Ordering {
        if v1 == v2 { Ordering::Equal }
        else {
            self.tie_break_agent_versions(
                self.local_to_agent_version(v1),
                self.local_to_agent_version(v2)
            )
        }
    }
}

#[cfg(test)]
mod known_seq_span_tests {
    use crate::causalgraph::agent_span::AgentSpan;
    use crate::CausalGraph;

    /// An author's sequence numbers stay contiguous even when his edits are
    /// interleaved with somebody else's, which splits them across several
    /// stored entries. known_seq_span has to report the whole run regardless.
    #[test]
    fn merges_across_interleaved_entries() {
        let mut cg = CausalGraph::new();
        let a = cg.get_or_create_agent_id("a");
        let b = cg.get_or_create_agent_id("b");

        // a writes seqs 0..5, then b interrupts, then a continues at seq 5
        cg.merge_and_assign(&[], AgentSpan { agent: a, seq_range: (0..5).into() });
        cg.merge_and_assign(&[], AgentSpan { agent: b, seq_range: (0..3).into() });
        cg.merge_and_assign(&[], AgentSpan { agent: a, seq_range: (5..9).into() });

        let aa = &cg.agent_assignment;
        // a's stored entries are split, but the run is 0..9 throughout
        for probe in 0..9usize {
            let span = aa.known_seq_span(a, (probe..probe + 1).into())
                .unwrap_or_else(|| panic!("missing seq {probe}"));
            assert_eq!((span.start, span.end), (0, 9), "at seq {probe}");
        }
        assert_eq!(aa.known_seq_span(a, (9..10).into()), None);
        assert_eq!(aa.known_seq_span(b, (0..1).into()).map(|s| (s.start, s.end)), Some((0, 3)));
        assert_eq!(aa.known_seq_span(b, (3..4).into()), None);
    }

    /// A query overlapping the run only at its far end still finds it. Asking
    /// "have I already seen any of these seqs?" depends on it: a caller that
    /// missed a partial overlap would take an edit it already holds for a new
    /// one.
    #[test]
    fn finds_a_run_the_query_only_touches() {
        let mut cg = CausalGraph::new();
        let a = cg.get_or_create_agent_id("a");
        cg.merge_and_assign(&[], AgentSpan { agent: a, seq_range: (8..12).into() });
        let aa = &cg.agent_assignment;

        // query spans a gap and then reaches the run
        assert_eq!(aa.known_seq_span(a, (5..11).into()).map(|s| (s.start, s.end)), Some((8, 12)));
        // query entirely below it
        assert_eq!(aa.known_seq_span(a, (0..8).into()), None);
        // query entirely above it
        assert_eq!(aa.known_seq_span(a, (12..20).into()), None);
        // unknown agent
        assert_eq!(aa.known_seq_span(99, (0..1).into()), None);
    }
}
