mod utils;

use wasm_bindgen::prelude::*;
// use serde_wasm_bindgen::Serializer;
// use serde::{Serialize};
use diamond_types::{AgentId, DTRange, LV};
use diamond_types::list::{ListBranch as DTBranch, ListCRDT, ListOpLog as DTOpLog};
use diamond_types::list::encoding::{ENCODE_FULL, ENCODE_PATCH};
use diamond_types::list::operation::TextOperation;

// When the `wee_alloc` feature is enabled, use `wee_alloc` as the global
// allocator.
#[cfg(feature = "wee_alloc")]
#[global_allocator]
static ALLOC: wee_alloc::WeeAlloc = wee_alloc::WeeAlloc::INIT;

type WasmResult<T = JsValue> = Result<T, serde_wasm_bindgen::Error>;

// The versions we consume from javascript cannot really represent ROOT_TIME. (Actually ROOT_TIME
// is sort of unnecessary internally in DT anyway). We'll map internal [ROOT_TIME] to [] in
// javascript.

#[wasm_bindgen]
pub struct Branch(DTBranch);

#[wasm_bindgen]
pub struct OpLog {
    inner: DTOpLog,
    agent_id: Option<AgentId>,
}

// pub fn checkout(&self) -> Branch {
//     let mut result = DTBranch::new();
//     result.merge(&self.0, &self.0.get_frontier());
//     Branch(result)
// }


pub fn get_ops(oplog: &DTOpLog) -> WasmResult {
    let ops = oplog.iter_ops().collect::<Vec<_>>();
    serde_wasm_bindgen::to_value(&ops)
}

pub fn get_ops_since(oplog: &DTOpLog, version: &[LV]) -> WasmResult {
    let ops = oplog.iter_range_since(version)
        .collect::<Box<[TextOperation]>>();
    serde_wasm_bindgen::to_value(&ops)
}

// pub fn to_ar

pub fn get_history(oplog: &DTOpLog) -> WasmResult {
    let txns = oplog.iter_history().collect::<Vec<_>>();
    serde_wasm_bindgen::to_value(&txns)
}

// pub fn get_local_frontier(frontier: &[Time]) -> Box<[Time]> {
//     frontier.iter().copied().collect::<Box<[Time]>>()
// }

// #[wasm_bindgen]
// pub fn local_to_remote_time(oplog: &DTOpLog, time: Time) -> Result<JsValue, serde_wasm_bindgen::Error> {
//     let remote_time = oplog.time_to_remote_id(time);
//     serde_wasm_bindgen::to_value(&remote_time)
// }
pub fn local_to_remote_version(oplog: &DTOpLog, local_version: &[LV]) -> WasmResult {
    let remote_version = oplog.cg.agent_assignment.local_to_remote_frontier(local_version);
    serde_wasm_bindgen::to_value(&remote_version)
}

pub fn oplog_version_to_remote_version(oplog: &DTOpLog) -> WasmResult {
    let version = oplog.local_frontier_ref();
    let frontier = oplog.cg.agent_assignment.local_to_remote_frontier(version);
    serde_wasm_bindgen::to_value(&frontier)
}

// This method adds 15kb to the wasm bundle, or 4kb to the brotli size. O_o.
pub fn to_bytes(oplog: &DTOpLog) -> Vec<u8> {
    let bytes = oplog.encode(&ENCODE_FULL);
    bytes
}

pub fn get_patch_since(oplog: &DTOpLog, from_version: &[LV]) -> Vec<u8> {
    // let from_version = map_parents(&version);
    let bytes = oplog.encode_from(&ENCODE_PATCH, from_version);
    bytes
}

pub fn decode_and_add(oplog: &mut DTOpLog, bytes: &[u8]) -> WasmResult {
    match oplog.decode_and_add(bytes) {
        Ok(version) => {
            serde_wasm_bindgen::to_value(&version)
        },
        Err(e) => {
            let s = format!("Error merging {:?}", e);
            let js: JsValue = s.into();
            Err(js.into())
        }
    }
}

/// One transformed operation, and the agent that wrote it.
#[derive(serde::Serialize)]
struct JsXfOp {
    agent: String,
    kind: &'static str,
    start: usize,
    end: usize,
    fwd: bool,
    content: String,
}

/// The operations between two versions, travelled into `from`'s frame: each
/// one positioned against the document as it stood at `from`, so applying
/// them in order turns that document into the one at `to`.
pub fn xf_between(oplog: &DTOpLog, from: &[LV], to: &[LV]) -> WasmResult {
    use diamond_types::list::operation::ListOpKind;
    let aa = &oplog.cg.agent_assignment;
    let xf = oplog.iter_xf_operations_from(from, to)
        .filter_map(|(range, op)| op.map(|op| (range, op)))
        .map(|(range, op)| JsXfOp {
            agent: aa.local_to_remote_version(range.start).0.to_string(),
            kind: if op.kind == ListOpKind::Ins { "Ins" } else { "Del" },
            start: op.start(),
            end: op.end(),
            fwd: op.loc.fwd,
            content: op.content_as_str().unwrap_or_default().to_string(),
        })
        .collect::<Vec<_>>();

    serde_wasm_bindgen::to_value(&xf)
}

pub fn xf_since(oplog: &DTOpLog, version: &[LV]) -> WasmResult {
    xf_between(oplog, version, &oplog.local_frontier_ref())
}

pub fn merge_versions(oplog: &DTOpLog, a: &[LV], b: &[LV]) -> Box<[LV]> {
    let result = oplog.version_union(a, b);
    result.as_ref().into()
}

/// Every (agent, seq_start, len) run this oplog knows, in local-version
/// order. For building version indexes.
pub fn agent_seq_runs(oplog: &DTOpLog) -> WasmResult {
    let runs: Vec<(&str, usize, usize)> = oplog.iter_remote_mappings()
        .map(|s| (s.0, s.1.start, s.1.end - s.1.start))
        .collect();
    serde_wasm_bindgen::to_value(&runs)
}

/// Map "agent-seq" version strings to local version indexes
fn parse_remote_ids(oplog: &DTOpLog, ids: &[String], tolerant: bool) -> Result<Vec<LV>, String> {
    use diamond_types::causalgraph::agent_assignment::remote_ids::RemoteVersion;

    let aa = &oplog.cg.agent_assignment;
    let mut result: Vec<LV> = Vec::with_capacity(ids.len());
    for id in ids {
        // The seq comes after the last dash; agent names may contain dashes
        let parsed = id.rsplit_once('-')
            .and_then(|(name, seq)| Some((name, seq.parse::<usize>().ok()?)));
        let lv = parsed.and_then(|(name, seq)|
            aa.try_remote_to_local_version(RemoteVersion(name, seq)).ok());

        match lv {
            Some(lv) => result.push(lv),
            None if tolerant => {},
            None => return Err(format!("version not found: {:?}", id)),
        }
    }

    // Frontiers are sorted sets
    result.sort_unstable();
    Ok(result)
}

/// The inverse of localToRemoteVersion. With tolerant set, ids this oplog
/// doesn't know are skipped instead of raising an error.
pub fn remote_to_local_version(oplog: &DTOpLog, ids: JsValue, tolerant: bool) -> WasmResult {
    let ids: Vec<String> = serde_wasm_bindgen::from_value(ids)?;
    match parse_remote_ids(oplog, &ids, tolerant) {
        Ok(result) => serde_wasm_bindgen::to_value(&result),
        Err(s) => {
            let js: JsValue = s.into();
            Err(js.into())
        }
    }
}

/// A braid range patch: replace `range` of `unit` with `content`. Ranges are
/// measured in the document's text; see `span` for the other axis.
#[derive(serde::Serialize)]
struct RangePatch {
    unit: &'static str,
    range: String,
    content: String,
}

/// A braid update.
#[derive(serde::Serialize)]
struct BraidUpdate {
    /// A version is a set of event ids. The version an update arrives at is
    /// always the single event it ends on, so this always holds one.
    version: Vec<String>,
    /// The event `parents` belongs to. An update summarizes a run of
    /// consecutive events by one agent, so it covers `first_event` through
    /// the event `version` names, and nothing else states how far back the
    /// run reaches.
    first_event: String,
    /// The version this update applies to, which may be several events.
    parents: Vec<String>,
    patches: Vec<RangePatch>,
}

fn to_range_patch(r: diamond_types::list::relative_to_absolute::Replacement) -> RangePatch {
    RangePatch {
        unit: "text",
        range: format!("[{}:{}]", r.range.start, r.range.end),
        content: r.content.to_string(),
    }
}

fn to_update(u: diamond_types::list::observed_history::Update,
             oplog: &DTOpLog) -> BraidUpdate {
    let aa = &oplog.cg.agent_assignment;
    let mut parents: Vec<String> = u.parents.as_ref().iter().map(|lv| {
        let rv = aa.local_to_remote_version(*lv);
        format!("{}-{}", rv.0, rv.1)
    }).collect();
    // A version is a set, so it has no order of its own. Sorting gives one,
    // and two peers naming the same parents then produce the same list.
    parents.sort();

    let (s, e) = (u.pos_start, u.pos_end);
    // One event per character, so the run is as long as the text it inserts
    // or deletes, and counting back from its last event gives its first.
    let first_seq = u.seq_end + 1 - (e - s);

    BraidUpdate {
        version: vec![format!("{}-{}", u.agent, u.seq_end)],
        first_event: format!("{}-{}", u.agent, first_seq),
        parents,
        patches: vec![RangePatch {
            unit: "text",
            // An insert replaces the empty range at `s`; a delete replaces
            // the span of text it removes.
            range: if u.content.is_some() { format!("[{s}:{s}]") }
                   else { format!("[{s}:{e}]") },
            content: u.content.map(|c| c.to_string()).unwrap_or_default(),
        }],
    }
}

fn to_updates(updates: Vec<diamond_types::list::observed_history::Update>,
              oplog: &DTOpLog) -> WasmResult {
    let out: Vec<BraidUpdate> = updates.into_iter()
        .map(|u| to_update(u, oplog)).collect();
    serde_wasm_bindgen::to_value(&out)
}

/// The updates since `since` (event id strings, or null/undefined for all of
/// Observed History).
pub fn get_updates(oplog: &DTOpLog, since: JsValue) -> WasmResult {
    let frontier: Vec<LV> = if since.is_null() || since.is_undefined() {
        Vec::new()
    } else {
        let ids: Vec<String> = serde_wasm_bindgen::from_value(since)?;
        match parse_remote_ids(oplog, &ids, false) {
            Ok(p) => p,
            Err(s) => {
                let js: JsValue = s.into();
                return Err(js.into());
            }
        }
    };

    to_updates(oplog.updates_since(&frontier), oplog)
}

/// The transformed changes since `since`, as range patches against the current
/// text.
pub fn get_xf_patches(oplog: &DTOpLog, since: &[LV]) -> WasmResult {
    let patches: Vec<RangePatch> = oplog.xf_absolute_since(since)
        .into_iter().map(to_range_patch).collect();
    serde_wasm_bindgen::to_value(&patches)
}

/// Converts an array of "Relative" to "Absolute" patches:
///  - Relative: Each patch applies *on top of* the previous patche's mutations
///  - Absolute: Each patch references locations in *the prior document* before any mutations
pub fn relative_to_absolute_patches_js(patches: JsValue) -> WasmResult {
    use diamond_types::list::relative_to_absolute::{Replacement, relative_to_absolute_patches};

    #[derive(serde::Deserialize)]
    struct In { range: String, content: String }

    let input: Vec<In> = serde_wasm_bindgen::from_value(patches)?;
    let mut rel = Vec::with_capacity(input.len());
    for p in &input {
        let mut nums = p.range.split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<usize>().unwrap());
        let (Some(start), Some(end)) = (nums.next(), nums.next()) else {
            let js: JsValue = format!("bad range: {:?}", p.range).into();
            return Err(js.into());
        };
        rel.push(Replacement { range: (start..end).into(),
                               content: p.content.as_str().into() });
    }
    let patches: Vec<RangePatch> = relative_to_absolute_patches(&rel)
        .into_iter().map(to_range_patch).collect();
    serde_wasm_bindgen::to_value(&patches)
}

/// One remote edit as its author recorded it: an insert of `ins` at `pos`,
/// or a delete of `del` characters starting at `pos`, both positioned
/// against the document as it stood at `parents`.
#[derive(serde::Deserialize)]
struct RemoteOp {
    agent: String,
    seq: usize,
    parents: Vec<String>,
    pos: usize,
    #[serde(default)]
    del: usize,
    #[serde(default)]
    ins: Option<String>,
}

fn js_error(message: String) -> serde_wasm_bindgen::Error {
    let js: JsValue = message.into();
    js.into()
}

/// Record a remote edit in the oplog. The branch is left where it is, so a
/// caller adding a run of edits can pay for one merge at the end.
fn add_remote_op(oplog: &mut DTOpLog, op: &RemoteOp) -> Result<DTRange, String> {
    let parents = parse_remote_ids(oplog, &op.parents, false)?;
    let agent = oplog.get_or_create_agent_id(&op.agent);

    let text_op = if op.del > 0 {
        let mut text_op = TextOperation::new_delete(op.pos .. op.pos + op.del);
        // Sequence numbers within a delete run count backward from the end
        // of the range: the first character deleted is the range's last
        // one, as when holding down backspace.
        text_op.loc.fwd = false;
        text_op
    } else {
        TextOperation::new_insert(op.pos, op.ins.as_deref().unwrap_or(""))
    };

    Ok(oplog.add_operations_remote(agent, &parents, op.seq, &[text_op]))
}

fn unwrap_agentid(agent_id: Option<AgentId>) -> AgentId {
    agent_id.expect_throw("Agent missing. Set agent before modifying oplog.")
}


#[wasm_bindgen]
impl Branch {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        utils::set_panic_hook();

        Self(DTBranch::new())
    }

    #[wasm_bindgen]
    pub fn all(oplog: &OpLog) -> Self {
        let mut result = Self::new();
        result.0.merge(&oplog.inner, &oplog.inner.local_frontier_ref());
        result
    }

    #[wasm_bindgen]
    pub fn get(&self) -> String {
        self.0.content().to_string()
    }

    /// Merge in from some named point in time
    #[wasm_bindgen]
    pub fn merge(&mut self, ops: &OpLog, branch: Option<Box<[LV]>>) {
        if let Some(branch) = branch {
            self.0.merge(&ops.inner, &branch);
        } else {
            self.0.merge(&ops.inner, &ops.inner.local_frontier_ref());
        }
    }

    #[wasm_bindgen(js_name = getLocalVersion)]
    pub fn get_local_frontier(&self) -> Box<[LV]> {
        self.0.local_frontier_ref().into()
    }

    #[wasm_bindgen(js_name = wCharsToChars)]
    pub fn wchars_to_chars(&self, pos_wchars: usize) -> usize {
        self.0.content().borrow().wchars_to_chars(pos_wchars)
    }

    #[wasm_bindgen(js_name = charsToWchars)]
    pub fn chars_to_wchars(&self, pos_chars: usize) -> usize {
        self.0.content().borrow().chars_to_wchars(pos_chars)
    }
}

#[wasm_bindgen]
impl OpLog {
    #[wasm_bindgen(constructor)]
    pub fn new(agent_name: Option<String>) -> Self {
        utils::set_panic_hook();

        let mut inner = DTOpLog::new();
        let agent_id = agent_name.map(|name| {
            inner.get_or_create_agent_id(name.as_str())
        });

        Self { inner, agent_id }
    }

    #[wasm_bindgen(js_name = setAgent)]
    pub fn set_agent(&mut self, agent: &str) {
        self.agent_id = Some(self.inner.get_or_create_agent_id(agent));
    }

    #[wasm_bindgen(js_name = clone)]
    pub fn js_clone(&self) -> Self {
        // We can't trust the .clone() process to preserve the agent_id.
        let name = self.agent_id.map(|id| self.inner.get_agent_name(id));
        let mut new_oplog = self.inner.clone();
        let agent_id = name.map(|name| new_oplog.get_or_create_agent_id(name));

        Self {
            inner: new_oplog,
            agent_id
        }
    }

    #[wasm_bindgen(js_name = ins)]
    pub fn add_insert(&mut self, pos: usize, content: &str, parents_in: Option<Box<[usize]>>) -> usize {
        // let parents = parents_in.map_or_else(|| self.inner.local_version(), |p| {
        //     js_to_internal_version(&p)
        // });

        let parents = parents_in.unwrap_or_else(|| {
            // Its gross here - I'm converting the frontier into a smallvec then immediately
            // converting it to a slice again :p
            self.inner.local_frontier_ref().into()
        });
        // Safe because we're just adding [ROOT] if its set.
        self.inner.add_insert_at(unwrap_agentid(self.agent_id), &parents, pos, content)
    }

    #[wasm_bindgen(js_name = del)]
    pub fn add_delete(&mut self, pos: usize, len: usize, parents_in: Option<Box<[usize]>>) -> usize {
        let parents = parents_in.unwrap_or_else(|| {
            // And here :p
            self.inner.local_frontier_ref().into()
        });
        self.inner.add_delete_at(unwrap_agentid(self.agent_id), &parents, pos..pos + len)
    }

    // This adds like 70kb of size to the WASM binary.
    // #[wasm_bindgen]
    // pub fn apply_op(&mut self, op: JsValue) -> WasmResult<usize> {
    //     let op_inner: Operation = serde_wasm_bindgen::from_value(op)?;
    //     Ok(self.inner.push(self.agent_id, &[op_inner]))
    // }

    // #[wasm_bindgen]
    // pub fn apply_op(&mut self, isInsert: bool, start: usize, end: usize, fwd: bool, content: &str) -> WasmResult<usize> {
    //     let op_inner: Operation = Operation {
    //         span: (start..end).into(),
    //         tag: if isInsert { InsDelTag::Ins } else { InsDelTag::Del },
    //         content: Some(content.into())
    //     };
    //     Ok(self.inner.push(self.agent_id, &[op_inner]))
    // }

    #[wasm_bindgen]
    pub fn checkout(&self) -> Branch {
        Branch::all(self)
    }

    #[wasm_bindgen(js_name = getOps)]
    pub fn get_ops(&self) -> WasmResult {
        get_ops(&self.inner)
    }

    #[wasm_bindgen(js_name = getOpsSince)]
    pub fn get_ops_since(&self, frontier: &[LV]) -> WasmResult {
        get_ops_since(&self.inner,  frontier)
    }

    // pub fn to_ar

    #[wasm_bindgen(js_name = getHistory)]
    pub fn get_history(&self) -> WasmResult {
        get_history(&self.inner)
    }

    #[wasm_bindgen(js_name = getLocalVersion)]
    pub fn get_local_frontier(&self) -> Box<[LV]> {
        self.inner.local_frontier_ref().into()
    }

    // #[wasm_bindgen]
    // pub fn local_to_remote_time(&self, time: Time) -> Result<JsValue, serde_wasm_bindgen::Error> {
    //     let remote_time = self.inner.time_to_remote_id(time);
    //     serde_wasm_bindgen::to_value(&remote_time)
    // }
    #[wasm_bindgen(js_name = localToRemoteVersion)]
    pub fn local_to_remote_version(&self, version: &[LV]) -> WasmResult {
        local_to_remote_version(&self.inner, version)
    }

    #[wasm_bindgen(js_name = remoteToLocalVersion)]
    pub fn remote_to_local_version_js(&self, ids: JsValue, tolerant: Option<bool>) -> WasmResult {
        remote_to_local_version(&self.inner, ids, tolerant.unwrap_or(false))
    }

    #[wasm_bindgen(js_name = getAgentSeqRuns)]
    pub fn get_agent_seq_runs(&self) -> WasmResult {
        agent_seq_runs(&self.inner)
    }

    #[wasm_bindgen(js_name = getUpdates)]
    pub fn get_updates_js(&self, since: JsValue) -> WasmResult {
        get_updates(&self.inner, since)
    }

    #[wasm_bindgen(js_name = getRemoteVersion)]
    pub fn get_remote_version(&self) -> WasmResult {
        oplog_version_to_remote_version(&self.inner)
    }

    // This method adds 15kb to the wasm bundle, or 4kb to the brotli size.
    #[wasm_bindgen(js_name = toBytes)]
    pub fn to_bytes(&self) -> Vec<u8> {
        to_bytes(&self.inner)
    }

    #[wasm_bindgen(js_name = getPatchSince)]
    pub fn get_patch_since(&self, from_version: &[usize]) -> Vec<u8> {
        get_patch_since(&self.inner, from_version)
    }

    // This method adds 17kb to the wasm bundle, or 5kb after brotli.
    #[wasm_bindgen(js_name = fromBytes)]
    pub fn from_bytes(bytes: &[u8], agent_name: Option<String>) -> Self {
        utils::set_panic_hook();

        let mut inner = DTOpLog::load_from(bytes).unwrap();
        let agent_id = agent_name.map(|name| {
            inner.get_or_create_agent_id(name.as_str())
        });

        Self { inner, agent_id }
    }

    /// Decode bytes, and add (merge in) any missing operations.
    #[wasm_bindgen(js_name = addFromBytes)]
    pub fn add_from_bytes(&mut self, bytes: &[u8]) -> WasmResult {
        decode_and_add(&mut self.inner, bytes)
    }

    // pub fn xf_since(&self, from_version: &[usize]) -> WasmResult {
    #[wasm_bindgen(js_name = getXF)]
    pub fn get_xf(&self) -> WasmResult {
        xf_since(&self.inner, &[])
    }

    #[wasm_bindgen(js_name = getXFSince)]
    pub fn get_xf_since(&self, from_version: &[LV]) -> WasmResult {
        xf_since(&self.inner, from_version)
    }

    #[wasm_bindgen(js_name = mergeVersions)]
    pub fn merge_versions(&self, a: &[LV], b: &[LV]) -> Box<[LV]> {
        merge_versions(&self.inner, a, b)
    }

    // pub fn merge_versions(&self, a: &[usize], b: &[usize]) ->
}

#[wasm_bindgen]
pub struct Doc {
    inner: ListCRDT,
    agent_id: Option<AgentId>,
}

impl Doc {
    /// Bring the branch up to the oplog's newest version, so that reading
    /// the document reflects everything in it.
    fn merge_to_tip(&mut self) {
        self.inner.branch.merge(&self.inner.oplog, self.inner.oplog.cg.version.as_ref());
    }
}


// #[wasm_bindgen]
// extern "C" {
//     // Use `js_namespace` here to bind `console.log(..)` instead of just
//     // `log(..)`
//     #[wasm_bindgen(js_namespace = console)]
//     fn log(s: &str);
// }

#[wasm_bindgen]
impl Doc {
    #[wasm_bindgen(constructor)]
    pub fn new(agent_name: Option<String>) -> Self {
        utils::set_panic_hook();

        let mut inner = ListCRDT::new();
        let agent_id = agent_name.map(|name| {
            inner.get_or_create_agent_id(name.as_str())
        });

        Doc { inner, agent_id }
    }

    #[wasm_bindgen]
    pub fn ins(&mut self, pos: usize, content: &str) {
        // let id = self.0.get_or_create_agent_id("seph");
        self.inner.insert(unwrap_agentid(self.agent_id), pos, content);
    }

    #[wasm_bindgen]
    pub fn del(&mut self, pos: usize, del_span: usize) {
        self.inner.delete(unwrap_agentid(self.agent_id), pos .. pos + del_span);
    }

    #[wasm_bindgen]
    pub fn len(&self) -> usize {
        self.inner.branch.len()
    }

    #[wasm_bindgen]
    pub fn is_empty(&self) -> bool { // To make clippy happy.
        self.inner.branch.is_empty()
    }

    #[wasm_bindgen]
    pub fn get(&self) -> String {
        self.inner.branch.content().to_string()
    }

    /// The document's text as of some historical (local) version
    #[wasm_bindgen(js_name = getStringAt)]
    pub fn get_string_at(&self, version: &[LV]) -> String {
        self.inner.oplog.checkout(version).content().to_string()
    }

    /// The document's length in characters as of some historical version.
    /// Cheaper than getStringAt, since it never builds the text.
    #[wasm_bindgen(js_name = lenAt)]
    pub fn len_at(&self, version: &[LV]) -> usize {
        self.inner.oplog.len_at(version)
    }

    /// How much longer the document is now than it was at `version`.
    ///
    /// A caller holding the current length can subtract this to learn the
    /// length at an earlier version without replaying the history, which is
    /// what positioning an edit from a peer needs. Only the operations
    /// between the two versions are walked, so an edit written against a
    /// version a few behind the current one costs almost nothing.
    #[wasm_bindgen(js_name = lenSince)]
    pub fn len_since(&self, version: &[LV]) -> f64 {
        self.inner.oplog.len_delta(version, self.inner.oplog.cg.version.as_ref()) as f64
    }


    #[wasm_bindgen(js_name = remoteToLocalVersion)]
    pub fn remote_to_local_version_js(&self, ids: JsValue, tolerant: Option<bool>) -> WasmResult {
        remote_to_local_version(&self.inner.oplog, ids, tolerant.unwrap_or(false))
    }

    #[wasm_bindgen(js_name = getAgentSeqRuns)]
    pub fn get_agent_seq_runs(&self) -> WasmResult {
        agent_seq_runs(&self.inner.oplog)
    }

    /// The updates covering local versions lo..hi. Local versions are
    /// topologically ordered, so this is a contiguous slice of the document's
    /// past, and it costs what the slice costs rather than what the whole
    /// history would. Adjacent spans tile: together they give what one span
    /// over both would, split at the seam.
    #[wasm_bindgen(js_name = getUpdatesInSpan)]
    pub fn get_updates_in_span(&self, lo: usize, hi: usize) -> WasmResult {
        to_updates(self.inner.oplog.updates_in_span((lo .. hi).into()),
                   &self.inner.oplog)
    }

    /// How many local versions this document holds, which is one per character
    /// ever inserted or deleted. This is the extent that `getUpdatesInSpan`
    /// indexes into, and answering it materializes nothing.
    #[wasm_bindgen(js_name = numLocalVersions)]
    pub fn num_local_versions(&self) -> usize {
        self.inner.oplog.cg.len()
    }

    /// The run of sequence numbers from `agent` that this document holds and
    /// that overlaps lo..=hi, returned as [first, last] inclusive. Undefined
    /// if the document holds none of that range.
    #[wasm_bindgen(js_name = knownSeqSpan)]
    pub fn known_seq_span_js(&self, agent: &str, lo: usize, hi: usize) -> Option<Box<[usize]>> {
        let aa = &self.inner.oplog.cg.agent_assignment;
        let id = aa.get_agent_id(agent)?;
        let span = aa.known_seq_span(id, (lo .. hi + 1).into())?;
        Some(Box::new([span.start, span.end - 1]))
    }

    /// Apply one remote edit, and bring the document up to date with it.
    #[wasm_bindgen(js_name = applyRemoteOp)]
    pub fn apply_remote_op_js(&mut self, agent: &str, start_seq: usize, parents: JsValue,
                              pos: usize, del: usize, ins: Option<String>) -> WasmResult<Box<[usize]>> {
        let op = RemoteOp {
            agent: agent.to_string(),
            seq: start_seq,
            parents: serde_wasm_bindgen::from_value(parents)?,
            pos, del, ins,
        };
        let range = add_remote_op(&mut self.inner.oplog, &op).map_err(js_error)?;
        self.merge_to_tip();
        Ok(Box::new([range.start, range.end]))
    }

    /// Apply a run of remote edits, and bring the document up to date once
    /// at the end.
    ///
    /// Merging is the expensive half of applying an edit, and its cost is
    /// set by how much concurrent history has to be transformed rather than
    /// by how many edits arrive. So merging a run of edits together costs
    /// about what merging one of them does, and applying them one at a time
    /// costs that much per edit.
    ///
    /// Each edit's parents must already be in the oplog, or belong to an
    /// earlier edit in the same run.
    #[wasm_bindgen(js_name = applyRemoteOps)]
    pub fn apply_remote_ops_js(&mut self, ops: JsValue) -> WasmResult<()> {
        let ops: Vec<RemoteOp> = serde_wasm_bindgen::from_value(ops)?;
        for op in &ops {
            add_remote_op(&mut self.inner.oplog, op).map_err(js_error)?;
        }
        self.merge_to_tip();
        Ok(())
    }

    #[wasm_bindgen(js_name = getUpdates)]
    pub fn get_updates_js(&self, since: JsValue) -> WasmResult {
        get_updates(&self.inner.oplog, since)
    }

    #[wasm_bindgen(js_name = getXFPatches)]
    pub fn get_xf_patches_js(&self, since: &[LV]) -> WasmResult {
        get_xf_patches(&self.inner.oplog, since)
    }

    /// toBytes() as of a historical version: the trimmed oplog,
    /// complementing getPatchSince
    #[wasm_bindgen(js_name = toBytesAt)]
    pub fn to_bytes_at(&self, version: &[LV]) -> Vec<u8> {
        self.inner.oplog.encode_at(version, &ENCODE_FULL)
    }

    #[wasm_bindgen(js_name = relativeToAbsolutePatches)]
    pub fn relative_to_absolute_patches_js(patches: JsValue) -> WasmResult {
        relative_to_absolute_patches_js(patches)
    }

    #[wasm_bindgen]
    pub fn merge(&mut self, branch: &[LV]) {
        self.inner.branch.merge(&self.inner.oplog, &branch);
    }

    #[wasm_bindgen(js_name = toBytes)]
    pub fn to_bytes(&self) -> Vec<u8> {
        to_bytes(&self.inner.oplog)
    }

    #[wasm_bindgen(js_name = getPatchSince)]
    pub fn get_patch_since(&self, from_version: &[LV]) -> Vec<u8> {
        get_patch_since(&self.inner.oplog, from_version)
    }

    // TODO: Do better error handling here.
    // pub fn from_bytes(bytes: &[u8], agent_name: Option<String>) -> WasmResult<Doc> {
    #[wasm_bindgen(js_name = fromBytes)]
    pub fn from_bytes(bytes: &[u8], agent_name: Option<String>) -> Self {
        utils::set_panic_hook();

        // let mut inner = ListCRDT::load_from(bytes).map_err(|e| e.into())?;
        let mut inner = ListCRDT::load_from(bytes).unwrap();
        let agent_id = agent_name.map(|name| {
            inner.get_or_create_agent_id(name.as_str())
        });

        Self {
            inner,
            agent_id
        }
    }

    #[wasm_bindgen(js_name = mergeBytes)]
    pub fn merge_bytes(&mut self, bytes: &[u8]) -> WasmResult<Box<[usize]>> {
    // pub fn merge_bytes(&mut self, bytes: &[u8]) -> WasmResult {
        match self.inner.merge_data_and_ff(bytes) {
            Err(e) => {
                let s = format!("Error merging {:?}", e);
                let js: JsValue = s.into();
                Err(js.into())
            },
            Ok(frontier) => Ok(frontier.into_iter().collect())
        }
    }
    // #[wasm_bindgen(js_name = mergeBytes)]
    // pub fn merge_bytes(&mut self, bytes: &[u8]) -> WasmResult {
    //     match self.inner.merge_data_and_ff(bytes) {
    //         Err(e) => {
    //             let s = format!("Error merging {:?}", e);
    //             let js: JsValue = s.into();
    //             Err(js.into())
    //         },
    //         Ok(frontier) => serde_wasm_bindgen::to_value(&frontier),
    //     }
    // }

    // TODO: This is identical to get_ops_since in OpLog (above). Remove duplicate code here.
    #[wasm_bindgen(js_name = getOpsSince)]
    pub fn get_ops_since(&self, frontier: &[LV]) -> WasmResult {
        get_ops_since(&self.inner.oplog, frontier)
    }

    #[wasm_bindgen(js_name = getLocalVersion)]
    pub fn get_local_frontier(&self) -> Box<[LV]> {
        self.inner.branch.local_frontier_ref().into()
    }

    #[wasm_bindgen(js_name = localToRemoteVersion)]
    pub fn local_to_remote_version(&self, time: &[LV]) -> WasmResult {
        local_to_remote_version(&self.inner.oplog, time)
    }

    #[wasm_bindgen(js_name = getRemoteVersion)]
    pub fn get_remote_version(&self) -> WasmResult {
        local_to_remote_version(&self.inner.oplog, &self.inner.branch.local_frontier_ref())
    }

    #[wasm_bindgen(js_name = xfSince)]
    pub fn xf_since(&self, from_version: &[usize]) -> WasmResult {
        xf_since(&self.inner.oplog, from_version)
    }

    /// What happened between two versions, as operations against the document
    /// as it stood at `from`. This is the same travel `xfSince` performs, with
    /// somewhere other than the present as the destination.
    #[wasm_bindgen(js_name = xfBetween)]
    pub fn xf_between(&self, from: &[usize], to: &[usize]) -> WasmResult {
        xf_between(&self.inner.oplog, from, to)
    }

    #[wasm_bindgen(js_name = getHistory)]
    pub fn get_history(&self) -> WasmResult {
        get_history(&self.inner.oplog)
    }

    #[wasm_bindgen(js_name = mergeVersions)]
    pub fn merge_versions(&self, a: &[usize], b: &[usize]) -> Box<[usize]> {
        merge_versions(&self.inner.oplog, a, b)
    }

    #[wasm_bindgen(js_name = wCharsToChars)]
    pub fn wchars_to_chars(&self, pos_wchars: usize) -> usize {
        self.inner.branch.content().borrow().wchars_to_chars(pos_wchars)
    }

    #[wasm_bindgen(js_name = charsToWchars)]
    pub fn chars_to_wchars(&self, pos_chars: usize) -> usize {
        self.inner.branch.content().borrow().chars_to_wchars(pos_chars)
    }

    // #[wasm_bindgen]
    // pub fn get_next_order(&self) -> Result<JsValue, JsValue> {
    //     serde_wasm_bindgen::to_value(&self.inner.get_next_time())
    //         .map_err(|err| err.into())
    // }

    // #[wasm_bindgen]
    // pub fn get_txn_since(&self, version: JsValue) -> Result<JsValue, JsValue> {
    //     let txns = if version.is_null() || version.is_undefined() {
    //         self.inner.get_all_txns::<Vec<_>>()
    //     } else {
    //         let clock: VectorClock = serde_wasm_bindgen::from_value(version)?;
    //         self.inner.get_all_txns_since::<Vec<_>>(&clock)
    //     };
    //
    //     serde_wasm_bindgen::to_value(&txns)
    //         .map_err(|err| err.into())
    // }
    //
    // #[wasm_bindgen]
    // pub fn positional_ops_since(&self, order: u32) -> Result<JsValue, JsValue> {
    //     let changes = self.inner.positional_changes_since(order);
    //
    //     serde_wasm_bindgen::to_value(&changes)
    //         .map_err(|err| err.into())
    // }
    //
    // #[wasm_bindgen]
    // pub fn traversal_ops_since(&self, order: u32) -> Result<JsValue, JsValue> {
    //     let changes = self.inner.traversal_changes_since(order);
    //
    //     serde_wasm_bindgen::to_value(&changes)
    //         .map_err(|err| err.into())
    // }
    //
    // #[wasm_bindgen]
    // pub fn traversal_ops_flat(&self, order: u32) -> Result<JsValue, JsValue> {
    //     let changes = self.inner.flat_traversal_since(order);
    //
    //     serde_wasm_bindgen::to_value(&changes)
    //         .map_err(|err| err.into())
    // }
    //
    // #[wasm_bindgen]
    // pub fn attributed_patches_since(&self, order: u32) -> Result<JsValue, JsValue> {
    //     let (changes, attr) = self.inner.remote_attr_patches_since(order);
    //
    //     // Using serialize_maps_as_objects here to flatten RemoteSpan into {agent, seq, len}.
    //     (changes, attr)
    //         .serialize(&Serializer::new().serialize_maps_as_objects(true))
    //         .map_err(|err| err.into())
    // }
    //
    // #[wasm_bindgen]
    // pub fn traversal_ops_since_branch(&self, branch: JsValue) -> Result<JsValue, JsValue> {
    //     let branch: Branch = if branch.is_null() || branch.is_undefined() {
    //         smallvec![ROOT_TIME]
    //     } else {
    //         let b: Vec<RemoteId> = serde_wasm_bindgen::from_value(branch)?;
    //         self.inner.remote_ids_to_branch(&b)
    //     };
    //
    //     let changes = self.inner.traversal_changes_since_branch(branch.as_slice());
    //
    //     serde_wasm_bindgen::to_value(&changes)
    //         .map_err(|err| err.into())
    // }
    //
    // #[wasm_bindgen]
    // pub fn merge_remote_txns(&mut self, txns: JsValue) -> Result<(), JsValue> {
    //     let txns: Vec<RemoteTxn> = serde_wasm_bindgen::from_value(txns)?;
    //     for txn in txns.iter() {
    //         self.inner.apply_remote_txn(txn);
    //     }
    //     Ok(())
    // }
    //
    // #[wasm_bindgen]
    // pub fn ins_at_order(&mut self, pos: usize, content: &str, order: u32, is_left: bool) {
    //     // let id = self.0.get_or_create_agent_id("seph");
    //     self.inner.insert_at_ot_order(self.agent_id, pos, content, order, is_left);
    // }
    //
    // #[wasm_bindgen]
    // pub fn del_at_order(&mut self, pos: usize, del_span: usize, order: u32) {
    //     self.inner.delete_at_ot_order(self.agent_id, pos, del_span, order, true);
    // }
    //
    // #[wasm_bindgen]
    // pub fn get_internal_list_entries(&self) -> Result<JsValue, JsValue> {
    //     let entries = self.inner.get_internal_list_entries().collect::<Vec<_>>();
    //     serde_wasm_bindgen::to_value(&entries)
    //         .map_err(|err| err.into())
    // }
    //
    // #[wasm_bindgen]
    // pub fn as_positional_patch(&self) -> Result<JsValue, JsValue> {
    //     let patch = self.inner.as_external_patch();
    //     serde_wasm_bindgen::to_value(&patch)
    //         .map_err(|err| err.into())
    // }
}
