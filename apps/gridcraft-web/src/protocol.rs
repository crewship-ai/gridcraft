//! Host-authority bridge, protocol 1: message shapes and the editor's sync state, without the
//! browser (`authority` is the browser glue). Everything here is plain data, so it is tested
//! natively.
//!
//! The host is the authority: it validates every message strictly (exact keys, limits) and ends
//! the session on anything malformed, so this module produces exactly these shapes.
//!
//! Handshake (window messages):
//! - editor → parent, to `parent_origin` only, from the window's `load` event or later:
//!   `{type:"gridcraft:hello", protocol:1, bundle}`. `bundle` is the digest of the bundle this
//!   page was served from, taken from its own URL (`…/b/<sha256 of the bundle manifest>/…`,
//!   below any prefix of whole path segments such as same-host `/_editor`); the host compares it
//!   with the bundle it framed. It is `null` when the page wasn't served from a pinned bundle
//!   path (and a host then refuses the editor).
//! - parent → editor: `{type:"gridcraft:connect", protocol:1, …}` with a `MessagePort`.
//!
//! On the port, every message is a JSON string with `protocol: 1`:
//! - editor → host: `mutation {seq, base_rev, ops:[{op:"cell.set", sheet, cell, input}]}` and
//!   `resume {last_rev, pending:[{seq, base_rev, ops}]}`. `seq` is positive and increasing; a
//!   restatement repeats a mutation byte for byte. At most [`MAX_OPS`] ops per mutation, no cell
//!   twice, inputs up to [`MAX_INPUT_BYTES`], messages up to [`MAX_MESSAGE_BYTES`].
//! - host → editor, in revision order: `snapshot {rev, generation, snapshot, discarded}` (the
//!   workbook in GridCraft's native JSON; `discarded`: seqs of mutations the host dropped because
//!   the workbook was replaced), `committed {rev, actor_label, ops}` (somebody else's change),
//!   `ack {seq, rev}` (our mutation is durable as `rev`), `rejected {seq, code, current_rev,
//!   cells:[{sheet?, cell, input, rev, actor_label}]}`, `online`, `offline`.
use std::collections::BTreeMap;

use gridcraft_engine::model::Workbook;
use gridcraft_engine::remote::{LocalEdit, RemoteOp};
use serde_json::{Value as Json, json};

pub const PROTOCOL: u64 = 1;
/// Host limits for one mutation and one message.
pub const MAX_OPS: usize = 64;
pub const MAX_INPUT_BYTES: usize = 8 * 1024;
pub const MAX_SHEET_CHARS: usize = 128;
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// Most mutations one `resume` restates.
pub const MAX_RESUME_PENDING: usize = 256;
/// Room left in a message for the envelope around the ops.
const OPS_BUDGET: usize = MAX_MESSAGE_BYTES - 1024;

/// (sheet name, A1)
pub type CellKey = (String, String);

pub fn key(op: &RemoteOp) -> CellKey {
    (op.sheet.clone(), op.cell.a1())
}

/// The bundle digest in a page path that ends in `b/<64 lowercase hex>/index.html` (or
/// `b/<digest>/`). A host may serve the bundle below a prefix of whole path segments, e.g.
/// same-host `/_editor/b/<digest>/index.html` or a dedicated origin's `/b/<digest>/index.html`.
///
/// The path is the raw (still percent-encoded) `location.pathname`. Anything that is not plainly
/// such a path gives `None`: no query or fragment, no empty, `.` or `..` segment (so no `//`), and
/// prefix segments only of unreserved characters (`A-Z a-z 0-9 - . _ ~`), so no `%`-encoded or
/// foreign separators.
pub fn bundle_digest_from_path(path: &str) -> Option<String> {
    let segments: Vec<&str> = path.strip_prefix('/')?.split('/').collect();
    let [prefix @ .., b, digest, file] = segments.as_slice() else { return None };
    let hex = digest.len() == 64 && digest.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c));
    let plain = |s: &&str| !s.is_empty() && *s != "." && *s != ".." && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"-._~".contains(&c));
    (*b == "b" && hex && (*file == "index.html" || file.is_empty()) && prefix.iter().all(plain)).then(|| digest.to_string())
}

pub fn hello(bundle: Option<&str>) -> Json {
    json!({"type": "gridcraft:hello", "protocol": PROTOCOL, "bundle": bundle})
}

/// A window message that hands over the port: `gridcraft:connect`, protocol 1.
pub fn is_connect(data: &Json) -> bool {
    data.get("type").and_then(Json::as_str) == Some("gridcraft:connect") && data.get("protocol").and_then(Json::as_u64) == Some(PROTOCOL)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Conflict {
    pub sheet: Option<String>,
    pub cell: String,
    pub input: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HostMsg {
    Snapshot { rev: u64, snapshot: Json, discarded: Vec<u64> },
    Committed { rev: u64, ops: Vec<RemoteOp> },
    Ack { seq: u64, rev: u64 },
    Rejected { seq: u64, code: String, cells: Vec<Conflict> },
    Online,
    Offline,
}

/// One port message from the host.
pub fn parse_host(text: &str) -> Result<HostMsg, String> {
    let m: Json = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    if m.get("protocol").and_then(Json::as_u64) != Some(PROTOCOL) {
        return Err("not protocol 1".into());
    }
    let n = |k: &str| m.get(k).and_then(Json::as_u64).ok_or_else(|| format!("missing `{k}`"));
    let s = |v: &Json, k: &str| v.get(k).and_then(Json::as_str).map(str::to_string);
    Ok(match m.get("type").and_then(Json::as_str).unwrap_or("") {
        "snapshot" => HostMsg::Snapshot {
            rev: n("rev")?,
            snapshot: m.get("snapshot").cloned().ok_or("missing `snapshot`")?,
            discarded: m.get("discarded").and_then(Json::as_array).map(|a| a.iter().filter_map(Json::as_u64).collect()).unwrap_or_default(),
        },
        "committed" => {
            let ops = m.get("ops").and_then(Json::as_array).ok_or("missing `ops`")?;
            HostMsg::Committed {
                rev: n("rev")?,
                ops: ops.iter().map(|o| RemoteOp::from_json(o).map_err(|e| e.to_string())).collect::<Result<_, _>>()?,
            }
        }
        "ack" => HostMsg::Ack { seq: n("seq")?, rev: n("rev")? },
        "rejected" => HostMsg::Rejected {
            seq: n("seq")?,
            code: s(&m, "code").unwrap_or_else(|| "rejected".into()),
            cells: m
                .get("cells")
                .and_then(Json::as_array)
                .map(|a| a.iter().filter_map(|c| Some(Conflict { sheet: s(c, "sheet"), cell: s(c, "cell")?, input: s(c, "input")? })).collect())
                .unwrap_or_default(),
        },
        "online" => HostMsg::Online,
        "offline" => HostMsg::Offline,
        other => return Err(format!("unknown message `{other}`")),
    })
}

/// The workbook in a snapshot (GridCraft's native JSON, as `.json` files hold it).
pub fn workbook_from_snapshot(snapshot: &Json) -> Result<Workbook, String> {
    let mut wb: Workbook = serde_json::from_value(snapshot.clone()).map_err(|e| format!("not a GridCraft workbook: {e}"))?;
    wb.styles.rebuild_index();
    if wb.sheets.is_empty() {
        wb = Workbook::new();
    }
    Ok(wb)
}

/// Every non-empty cell input of `wb`, by sheet name and A1.
pub fn inputs_of(wb: &Workbook) -> BTreeMap<CellKey, String> {
    let mut out = BTreeMap::new();
    for sh in &wb.sheets {
        for (c, _) in sh.cells.iter() {
            let t = sh.input_text(c);
            if !t.is_empty() {
                out.insert((sh.name.clone(), c.a1()), t);
            }
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    pub seq: u64,
    pub base_rev: u64,
    pub ops: Vec<RemoteOp>,
}

impl Pending {
    fn ops_json(&self) -> Vec<Json> {
        self.ops.iter().map(|o| json!({"op": "cell.set", "sheet": o.sheet, "cell": o.cell.a1(), "input": o.input})).collect()
    }
    pub fn message(&self) -> Json {
        json!({"type": "mutation", "protocol": PROTOCOL, "seq": self.seq, "base_rev": self.base_rev, "ops": self.ops_json()})
    }
    /// The form inside `resume.pending` (no envelope).
    fn restated(&self) -> Json {
        json!({"seq": self.seq, "base_rev": self.base_rev, "ops": self.ops_json()})
    }
}

fn op_bytes(op: &RemoteOp) -> usize {
    json!({"op": "cell.set", "sheet": op.sheet, "cell": op.cell.a1(), "input": op.input}).to_string().len() + 1
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Connecting,
    Saved,
    Saving,
    Offline,
    NotSaved(String),
}

/// What the browser glue does for the state machine.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Send this message on the port.
    Post(Json),
    /// Replace the document with the snapshot's workbook, then call [`Sync::loaded`].
    Load { rev: u64, snapshot: Json },
    /// Apply host-committed (or reconciled) cell values outside local history
    /// (`Session::apply_remote`).
    Apply { rev: u64, ops: Vec<RemoteOp> },
    /// Take the last local command back (`edit.undo`, dropping its redo step).
    Undo,
}

/// The editor's side of the bridge: what the host has confirmed, what waits for an ack.
#[derive(Debug, Default)]
pub struct Sync {
    pub loaded: bool,
    pub confirmed_rev: u64,
    /// The host's committed inputs.
    pub confirmed: BTreeMap<CellKey, String>,
    pub pending: Vec<Pending>,
    next_seq: u64,
    pub online: bool,
    pub rejected: Option<String>,
    /// A local command that was refused or only partly shared; shown until the next edit.
    pub notice: Option<String>,
    /// The cell being edited and the confirmed revision when its editor opened: the commit's
    /// precondition is "unchanged since I started typing", not "since I pressed Enter".
    edit_start: Option<(CellKey, u64)>,
}

impl Sync {
    pub fn new() -> Self {
        Self { next_seq: 1, ..Self::default() }
    }

    pub fn status(&self) -> Status {
        if let Some(why) = &self.rejected {
            return Status::NotSaved(why.clone());
        }
        if !self.loaded {
            return Status::Connecting;
        }
        if !self.online {
            return Status::Offline;
        }
        if self.pending.is_empty() { Status::Saved } else { Status::Saving }
    }

    /// The cell the user is editing now (each frame).
    pub fn editing(&mut self, cell: Option<CellKey>) {
        match cell {
            Some(k) if self.edit_start.as_ref().is_none_or(|(e, _)| *e != k) => self.edit_start = Some((k, self.confirmed_rev)),
            None => self.edit_start = None,
            _ => {}
        }
    }

    /// The snapshot from the last [`Effect::Load`] is now the document.
    pub fn loaded(&mut self, rev: u64, inputs: BTreeMap<CellKey, String>) {
        self.confirmed = inputs;
        self.confirmed_rev = rev;
        self.pending.clear();
        self.loaded = true;
    }

    fn pending_cells(&self) -> Vec<CellKey> {
        self.pending.iter().flat_map(|p| p.ops.iter().map(key)).collect()
    }

    /// Ask the host to replay from our last revision and restate what still waits for an ack
    /// (same seq and bytes: the host answers a restatement the way it answered the original).
    fn resume(&self) -> Effect {
        let mut pending = vec![];
        let mut size = 0;
        for p in self.pending.iter().take(MAX_RESUME_PENDING) {
            let r = p.restated();
            size += r.to_string().len() + 1;
            if size > OPS_BUDGET {
                break;
            }
            pending.push(r);
        }
        Effect::Post(json!({"type": "resume", "protocol": PROTOCOL, "last_rev": self.confirmed_rev, "pending": pending}))
    }

    pub fn on_host(&mut self, msg: HostMsg) -> Vec<Effect> {
        match msg {
            HostMsg::Snapshot { rev, snapshot, discarded } => {
                if self.loaded && (!discarded.is_empty() || !self.pending.is_empty()) {
                    let n = discarded.len().max(self.pending.len());
                    self.notice = Some(format!("the workbook was replaced: {n} unsaved change(s) discarded"));
                }
                self.rejected = None;
                self.pending.clear();
                self.online = true;
                vec![Effect::Load { rev, snapshot }]
            }
            HostMsg::Committed { rev, ops } => {
                if rev <= self.confirmed_rev {
                    return vec![]; // delivered twice
                }
                if rev != self.confirmed_rev + 1 {
                    return vec![self.resume()];
                }
                let pending = self.pending_cells();
                let mut apply = vec![];
                for op in ops {
                    self.confirmed.insert(key(&op), op.input.clone());
                    // A cell with an unacknowledged local change keeps showing it until the host
                    // decides (it rejects ours if they conflict).
                    if !pending.contains(&key(&op)) {
                        apply.push(op);
                    }
                }
                self.confirmed_rev = rev;
                if apply.is_empty() { vec![] } else { vec![Effect::Apply { rev, ops: apply }] }
            }
            HostMsg::Ack { seq, rev } => {
                let Some(i) = self.pending.iter().position(|p| p.seq == seq) else { return vec![] };
                let p = self.pending.remove(i);
                for op in &p.ops {
                    self.confirmed.insert(key(op), op.input.clone());
                }
                if rev == self.confirmed_rev + 1 {
                    self.confirmed_rev = rev;
                    vec![]
                } else if rev > self.confirmed_rev {
                    vec![self.resume()]
                } else {
                    vec![]
                }
            }
            HostMsg::Rejected { seq, code, cells } => {
                let Some(i) = self.pending.iter().position(|p| p.seq == seq) else { return vec![] };
                let p = self.pending.remove(i);
                for c in cells {
                    // The host names the sheet when it knows it; otherwise it is the sheet of our op on that cell.
                    let sheet = c.sheet.or_else(|| p.ops.iter().find(|o| o.cell.a1() == c.cell).map(|o| o.sheet.clone()));
                    if let Some(sheet) = sheet {
                        self.confirmed.insert((sheet, c.cell), c.input);
                    }
                }
                let still = self.pending_cells();
                let revert: Vec<RemoteOp> = p
                    .ops
                    .iter()
                    .filter(|o| !still.contains(&key(o)))
                    .map(|o| RemoteOp { input: self.confirmed.get(&key(o)).cloned().unwrap_or_default(), ..o.clone() })
                    .collect();
                let lost: Vec<String> = p.ops.iter().take(3).map(|o| format!("{} = \"{}\"", o.cell.a1(), o.input)).collect();
                let more = if p.ops.len() > 3 { format!(" and {} more", p.ops.len() - 3) } else { String::new() };
                self.rejected = Some(format!("{code}: {}{more}", lost.join(", ")));
                if revert.is_empty() { vec![] } else { vec![Effect::Apply { rev: self.confirmed_rev, ops: revert }] }
            }
            HostMsg::Offline => {
                self.online = false;
                vec![]
            }
            HostMsg::Online => {
                self.online = true;
                vec![self.resume()]
            }
        }
    }

    /// One local command's cell changes, as the engine recorded them.
    pub fn local_edit(&mut self, edit: LocalEdit) -> Vec<Effect> {
        if !edit.complete && edit.ops.is_empty() {
            // Its effect isn't expressible as cell ops (rows, columns, sheets, formatting): take
            // it back so the editor shows only what the host has.
            if edit.undo_step {
                let what = if edit.structural { "inserting or deleting rows, columns or sheets" } else { "formatting" };
                self.notice = Some(format!("{} isn't shared yet ({what}): undone", edit.command));
                return vec![Effect::Undo];
            }
            return vec![];
        }
        if edit.ops.is_empty() {
            return vec![];
        }
        self.notice = (!edit.complete).then(|| format!("{}: only cell contents are shared", edit.command));
        // The last value per cell, in first-touched order.
        let mut ops: Vec<RemoteOp> = vec![];
        for op in edit.ops {
            match ops.iter_mut().find(|o| key(o) == key(&op)) {
                Some(o) => o.input = op.input,
                None => ops.push(op),
            }
        }
        // What the host would refuse outright never leaves: show the host's value again.
        let (ops, refused): (Vec<_>, Vec<_>) =
            ops.into_iter().partition(|o| o.input.len() <= MAX_INPUT_BYTES && !o.sheet.is_empty() && o.sheet.chars().count() <= MAX_SHEET_CHARS);
        let mut effects = vec![];
        if !refused.is_empty() {
            let revert: Vec<RemoteOp> =
                refused.iter().map(|o| RemoteOp { input: self.confirmed.get(&key(o)).cloned().unwrap_or_default(), ..o.clone() }).collect();
            self.rejected = Some(format!("too long to share: {}", refused.iter().map(|o| o.cell.a1()).collect::<Vec<_>>().join(", ")));
            effects.push(Effect::Apply { rev: self.confirmed_rev, ops: revert });
        } else {
            // A new local edit after a rejection: the rejection notice has been seen.
            self.rejected = None;
        }
        let base_rev = match &self.edit_start {
            Some((k, rev)) if ops.iter().any(|o| key(o) == *k) => (*rev).min(self.confirmed_rev),
            _ => self.confirmed_rev,
        };
        // Within the host's limits: at most MAX_OPS ops and one message's worth of bytes each.
        let mut chunk: Vec<RemoteOp> = vec![];
        let mut size = 0;
        let mut chunks = vec![];
        for op in ops {
            let b = op_bytes(&op);
            if !chunk.is_empty() && (chunk.len() == MAX_OPS || size + b > OPS_BUDGET) {
                chunks.push(std::mem::take(&mut chunk));
                size = 0;
            }
            size += b;
            chunk.push(op);
        }
        if !chunk.is_empty() {
            chunks.push(chunk);
        }
        for ops in chunks {
            let p = Pending { seq: self.next_seq, base_rev, ops };
            self.next_seq += 1;
            effects.push(Effect::Post(p.message()));
            self.pending.push(p);
        }
        effects
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use gridcraft_engine::Session;

    fn op(sheet: &str, cell: &str, input: &str) -> RemoteOp {
        RemoteOp::set(sheet, cell, input).unwrap()
    }
    fn edit(command: &str, ops: Vec<RemoteOp>) -> LocalEdit {
        LocalEdit { command: command.into(), ops, complete: true, structural: false, undo_step: true }
    }
    fn keys(v: &Json) -> Vec<String> {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    }
    fn posted(effects: &[Effect]) -> Vec<Json> {
        effects.iter().filter_map(|e| if let Effect::Post(m) = e { Some(m.clone()) } else { None }).collect()
    }
    /// A loaded editor at `rev` with `B2 = 10` on Sheet1.
    fn live(rev: u64) -> Sync {
        let mut s = Sync::new();
        let e = s.on_host(HostMsg::Snapshot { rev, snapshot: json!({}), discarded: vec![] });
        assert!(matches!(e.as_slice(), [Effect::Load { .. }]));
        s.loaded(rev, BTreeMap::from([(("Sheet1".into(), "B2".into()), "10".into())]));
        s
    }
    /// The host's checks for an editor mutation (`parseMutation` in the host bridge).
    fn host_accepts_mutation(m: &Json, envelope: bool) {
        let mut want = vec!["base_rev", "ops", "seq"];
        if envelope {
            want.extend(["protocol", "type"]);
            assert_eq!(m["protocol"], 1);
            assert_eq!(m["type"], "mutation");
        }
        want.sort();
        assert_eq!(keys(m), want);
        assert!(m["seq"].as_u64().unwrap() > 0 && m["base_rev"].as_u64().unwrap() > 0);
        let ops = m["ops"].as_array().unwrap();
        assert!(!ops.is_empty() && ops.len() <= MAX_OPS);
        let mut cells = std::collections::HashSet::new();
        for o in ops {
            assert_eq!(keys(o), ["cell", "input", "op", "sheet"]);
            assert_eq!(o["op"], "cell.set");
            let cell = o["cell"].as_str().unwrap();
            let (letters, digits) = cell.split_at(cell.find(|c: char| c.is_ascii_digit()).unwrap());
            assert!((1..=3).contains(&letters.len()) && letters.bytes().all(|b| b.is_ascii_uppercase()), "{cell}");
            assert!(!digits.starts_with('0') && (1..=7).contains(&digits.len()), "{cell}");
            assert!(o["input"].as_str().unwrap().len() <= MAX_INPUT_BYTES);
            assert!(cells.insert((o["sheet"].as_str().unwrap().to_string(), cell.to_string())), "a cell twice");
        }
    }

    #[test]
    fn hello_has_exactly_the_keys_the_host_reads() {
        let digest = "0123456789abcdef".repeat(4);
        let h = hello(Some(&digest));
        assert_eq!(keys(&h), ["bundle", "protocol", "type"]);
        assert_eq!(h, json!({"type": "gridcraft:hello", "protocol": 1, "bundle": digest}));
        assert_eq!(hello(None)["bundle"], Json::Null);
    }

    #[test]
    fn bundle_digest_comes_only_from_a_pinned_bundle_path() {
        let d = "ab".repeat(32);
        assert_eq!(bundle_digest_from_path(&format!("/b/{d}/index.html")), Some(d.clone()));
        assert_eq!(bundle_digest_from_path(&format!("/b/{d}/")), Some(d.clone()));
        for bad in [
            format!("/b/{}/index.html", "AB".repeat(32)),
            format!("/b/{}/index.html", "ab".repeat(31)),
            format!("/b/{}/index.html", "ab".repeat(33)),
            format!("/b/{}g/index.html", "ab".repeat(31)),
            format!("/b/{d}/other.html"),
            format!("/b/{d}/sub/index.html"),
            format!("/b/{d}/index.html/"),
            format!("/b/{d}"),
            format!("b/{d}/index.html"),
            format!("/B/{d}/index.html"),
            "/index.html".to_string(),
            String::new(),
        ] {
            assert_eq!(bundle_digest_from_path(&bad), None, "{bad}");
        }
    }

    /// A host may serve the bundle below a prefix of whole path segments, e.g. same-host
    /// `/_editor/b/<digest>/index.html`.
    #[test]
    fn bundle_digest_is_taken_below_a_prefix_of_whole_segments() {
        let d = "0123456789abcdef".repeat(4);
        for good in [
            format!("/_editor/b/{d}/index.html"),
            format!("/_editor/b/{d}/"),
            format!("/x/b/{d}/index.html"),
            format!("/a/b-c/v1.2_~x/b/{d}/index.html"),
        ] {
            assert_eq!(bundle_digest_from_path(&good), Some(d.clone()), "{good}");
        }
        for bad in [
            // not lowercase / not 64 hex / not exactly `b/<digest>/index.html` at the end
            format!("/_editor/b/{}/index.html", d.to_uppercase()),
            format!("/_editor/b/{}/index.html", &d[..63]),
            format!("/_editor/b/{d}0/index.html"),
            format!("/_editor/b/{d}/index.html/x"),
            format!("/_editor/b/{d}/x/index.html"),
            format!("/_editor/b/{d}/index.htm"),
            format!("/_editor/xb/{d}/index.html"),
            format!("/_editor/{d}/index.html"),
            // dot segments, empty segments, authority-looking paths
            format!("/../b/{d}/index.html"),
            format!("/_editor/../b/{d}/index.html"),
            format!("/_editor/./b/{d}/index.html"),
            format!("/_editor//b/{d}/index.html"),
            format!("//b/{d}/index.html"),
            format!("//evil.example/b/{d}/index.html"),
            format!("/b//{d}/index.html"),
            format!("/_editor/b/{d}//index.html"),
            // encoded or foreign separators
            format!("/_editor%2Fb/{d}/index.html"),
            format!("/x%2F/b/{d}/index.html"),
            format!("/x%2f..%2f/b/{d}/index.html"),
            format!("/_editor/b%2F{d}/index.html"),
            format!("/_editor/b/{d}%2Findex.html"),
            format!("/x\\y/b/{d}/index.html"),
            format!("/_editor\\b/{d}/index.html"),
            format!("/x;y/b/{d}/index.html"),
            // query and fragment never belong to a path
            format!("/_editor/b/{d}/index.html?x=1"),
            format!("/_editor/b/{d}/index.html#x"),
            format!("/x?/b/{d}/index.html"),
            format!("/x#/b/{d}/index.html"),
            format!("/_editor/b/{d}/?"),
            // relative or absolute URLs instead of a path
            format!("_editor/b/{d}/index.html"),
            format!("https://evil.example/b/{d}/index.html"),
        ] {
            assert_eq!(bundle_digest_from_path(&bad), None, "{bad}");
        }
    }

    #[test]
    fn connect_is_recognised_by_type_and_protocol() {
        assert!(is_connect(
            &json!({"type": "gridcraft:connect", "protocol": 1, "session": "n", "page_label": "P", "rev": 3, "generation": 1, "client_id": "c"})
        ));
        assert!(!is_connect(&json!({"type": "gridcraft:connect", "protocol": 2})));
        assert!(!is_connect(&json!({"type": "gridcraft:hello", "protocol": 1})));
    }

    #[test]
    fn host_messages_parse_and_other_protocols_do_not() {
        let m = |v: Json| parse_host(&v.to_string());
        assert_eq!(m(json!({"type": "ack", "protocol": 1, "seq": 2, "rev": 5})), Ok(HostMsg::Ack { seq: 2, rev: 5 }));
        assert_eq!(
            m(
                json!({"type": "committed", "protocol": 1, "rev": 4, "actor_label": "Agent", "ops": [{"op": "cell.set", "sheet": "Notes", "cell": "A1", "input": "x"}]})
            ),
            Ok(HostMsg::Committed { rev: 4, ops: vec![op("Notes", "A1", "x")] })
        );
        assert_eq!(
            m(
                json!({"type": "rejected", "protocol": 1, "seq": 3, "code": "precondition_failed", "current_rev": 8, "cells": [{"cell": "B2", "input": "7", "rev": 8, "actor_label": "Agent"}]})
            ),
            Ok(HostMsg::Rejected {
                seq: 3,
                code: "precondition_failed".into(),
                cells: vec![Conflict { sheet: None, cell: "B2".into(), input: "7".into() }]
            })
        );
        assert!(matches!(
            m(json!({"type": "snapshot", "protocol": 1, "rev": 1, "generation": 1, "snapshot": {}, "discarded": [4]})),
            Ok(HostMsg::Snapshot { rev: 1, discarded, .. }) if discarded == vec![4]
        ));
        assert_eq!(m(json!({"type": "online", "protocol": 1})), Ok(HostMsg::Online));
        assert!(m(json!({"type": "ack", "protocol": 2, "seq": 1, "rev": 1})).is_err());
        assert!(m(json!({"type": "ack", "seq": 1, "rev": 1})).is_err());
        assert!(parse_host("not json").is_err());
    }

    #[test]
    fn a_local_edit_is_one_mutation_in_the_host_shape() {
        let mut s = live(3);
        let e = s.local_edit(edit("cell.set", vec![op("Sheet1", "B3", "21")]));
        let p = posted(&e);
        assert_eq!(p.len(), 1);
        host_accepts_mutation(&p[0], true);
        assert_eq!(
            p[0],
            json!({"type": "mutation", "protocol": 1, "seq": 1, "base_rev": 3, "ops": [{"op": "cell.set", "sheet": "Sheet1", "cell": "B3", "input": "21"}]})
        );
        assert_eq!(s.status(), Status::Saving);
        // Acked as the next revision: saved, and the value is confirmed.
        assert!(s.on_host(HostMsg::Ack { seq: 1, rev: 4 }).is_empty());
        assert_eq!((s.status(), s.confirmed_rev), (Status::Saved, 4));
        assert_eq!(s.confirmed[&("Sheet1".into(), "B3".into())], "21");
    }

    #[test]
    fn base_rev_is_the_revision_when_the_cell_editor_opened() {
        let mut s = live(2);
        s.editing(Some(("Sheet1".into(), "B5".into())));
        assert!(s.on_host(HostMsg::Committed { rev: 3, ops: vec![op("Sheet1", "B2", "99")] }).len() == 1);
        s.editing(Some(("Sheet1".into(), "B5".into())));
        let p = posted(&s.local_edit(edit("cell.set", vec![op("Sheet1", "B5", "draXft")])));
        assert_eq!(p[0]["base_rev"], 2, "opened at rev 2, committed at head 3");
        // Undo, paste, fill: the confirmed revision.
        s.editing(None);
        let p = posted(&s.local_edit(edit("edit.undo", vec![op("Sheet1", "B5", "")])));
        assert_eq!(p[0]["base_rev"], 3);
    }

    #[test]
    fn large_edits_are_split_within_the_host_limits_and_cells_are_not_repeated() {
        let mut s = live(1);
        let mut ops: Vec<RemoteOp> = (1..=150).map(|r| op("Sheet1", &format!("A{r}"), "v")).collect();
        ops.push(op("Sheet1", "A1", "last"));
        let p = posted(&s.local_edit(edit("edit.paste", ops)));
        assert_eq!(p.iter().map(|m| m["ops"].as_array().unwrap().len()).collect::<Vec<_>>(), [64, 64, 22]);
        for m in &p {
            host_accepts_mutation(m, true);
        }
        assert_eq!(p[0]["ops"][0]["input"], "last", "the last value of a cell, sent once");
        assert_eq!(p.iter().map(|m| m["seq"].as_u64().unwrap()).collect::<Vec<_>>(), [1, 2, 3]);
        // Big inputs: split by bytes so no message exceeds the host's limit.
        let mut s = live(1);
        let big = "x".repeat(MAX_INPUT_BYTES);
        let p = posted(&s.local_edit(edit("edit.paste", (1..=20).map(|r| op("Sheet1", &format!("C{r}"), &big)).collect())));
        assert!(p.len() > 1);
        for m in &p {
            assert!(m.to_string().len() <= MAX_MESSAGE_BYTES);
            host_accepts_mutation(m, true);
        }
    }

    #[test]
    fn an_input_the_host_would_refuse_is_not_sent_and_shows_the_host_value() {
        let mut s = live(5);
        let e = s.local_edit(edit("cell.set", vec![op("Sheet1", "B2", &"x".repeat(MAX_INPUT_BYTES + 1))]));
        assert!(posted(&e).is_empty());
        assert_eq!(e, vec![Effect::Apply { rev: 5, ops: vec![op("Sheet1", "B2", "10")] }]);
        assert!(matches!(s.status(), Status::NotSaved(_)));
    }

    #[test]
    fn remote_commit_on_a_pending_cell_waits_for_the_host_verdict() {
        let mut s = live(2);
        s.local_edit(edit("cell.set", vec![op("Sheet1", "B2", "500")]));
        // The agent's commit reaches the editor first: not shown over the pending value.
        assert!(
            s.on_host(HostMsg::Committed { rev: 3, ops: vec![op("Sheet1", "B2", "7"), op("Sheet1", "C1", "c")] })
                == vec![Effect::Apply { rev: 3, ops: vec![op("Sheet1", "C1", "c")] }]
        );
        // Refused (the host names no sheet): back to the host's value, "Not saved".
        let e = s.on_host(HostMsg::Rejected {
            seq: 1,
            code: "precondition_failed".into(),
            cells: vec![Conflict { sheet: None, cell: "B2".into(), input: "7".into() }],
        });
        assert_eq!(e, vec![Effect::Apply { rev: 3, ops: vec![op("Sheet1", "B2", "7")] }]);
        assert_eq!(s.status(), Status::NotSaved("precondition_failed: B2 = \"500\"".into()));
        // The next edit clears the notice.
        s.local_edit(edit("cell.set", vec![op("Sheet1", "D1", "d")]));
        assert_eq!(s.status(), Status::Saving);
    }

    #[test]
    fn refused_commands_are_taken_back_and_nothing_is_posted() {
        let mut s = live(1);
        let bold = LocalEdit { command: "home.bold".into(), ops: vec![], complete: false, structural: false, undo_step: true };
        assert_eq!(s.local_edit(bold), vec![Effect::Undo]);
        assert!(s.notice.as_deref().unwrap().contains("isn't shared"));
        let rows = LocalEdit { command: "rows.insert".into(), ops: vec![], complete: false, structural: true, undo_step: true };
        assert_eq!(s.local_edit(rows), vec![Effect::Undo]);
        assert!(s.pending.is_empty());
    }

    #[test]
    fn online_and_gaps_resume_with_the_host_shape() {
        let mut s = live(4);
        s.local_edit(edit("cell.set", vec![op("Sheet1", "B3", "1")]));
        s.local_edit(edit("cell.set", vec![op("Notes", "A1", "2")]));
        assert!(s.on_host(HostMsg::Offline).is_empty());
        assert_eq!(s.status(), Status::Offline);
        let p = posted(&s.on_host(HostMsg::Online));
        assert_eq!(keys(&p[0]), ["last_rev", "pending", "protocol", "type"]);
        assert_eq!((p[0]["type"].as_str(), p[0]["protocol"].as_u64(), p[0]["last_rev"].as_u64()), (Some("resume"), Some(1), Some(4)));
        let pending = p[0]["pending"].as_array().unwrap();
        assert_eq!(pending.len(), 2);
        for m in pending {
            host_accepts_mutation(m, false);
        }
        // A restatement is the same mutation: same seq, base_rev and ops as first sent.
        assert_eq!(pending[1], json!({"seq": 2, "base_rev": 4, "ops": [{"op": "cell.set", "sheet": "Notes", "cell": "A1", "input": "2"}]}));
        // A revision gap asks for a replay instead of applying out of order.
        let gap = posted(&s.on_host(HostMsg::Committed { rev: 9, ops: vec![op("Sheet1", "Z9", "z")] }));
        assert_eq!(gap[0]["type"], "resume");
        assert_eq!(s.confirmed_rev, 4);
        // Duplicates are no-ops.
        assert!(s.on_host(HostMsg::Committed { rev: 4, ops: vec![op("Sheet1", "Z9", "z")] }).is_empty());
    }

    #[test]
    fn a_new_snapshot_discards_pending_edits_and_says_so() {
        let mut s = live(4);
        s.local_edit(edit("cell.set", vec![op("Sheet1", "B3", "1")]));
        let e = s.on_host(HostMsg::Snapshot { rev: 9, snapshot: json!({"sheets": []}), discarded: vec![1] });
        assert!(matches!(e.as_slice(), [Effect::Load { rev: 9, .. }]));
        assert!(s.pending.is_empty());
        assert!(s.notice.as_deref().unwrap().contains("1 unsaved change"));
        s.loaded(9, BTreeMap::new());
        assert_eq!((s.status(), s.confirmed_rev), (Status::Saved, 9));
    }

    #[test]
    fn snapshot_is_the_native_workbook_json() {
        let mut session = Session::new();
        session.new_workbook();
        session.run("cell.set", json!({"cell": "B2", "input": "10"})).unwrap();
        session.run("cell.set", json!({"cell": "B3", "input": "=B2*2"})).unwrap();
        let snapshot = serde_json::to_value(&*session.active().unwrap().wb).unwrap();
        let wb = workbook_from_snapshot(&snapshot).unwrap();
        let inputs = inputs_of(&wb);
        assert_eq!(inputs[&("Sheet1".into(), "B2".into())], "10");
        assert_eq!(inputs[&("Sheet1".into(), "B3".into())], "=B2*2");
        assert!(workbook_from_snapshot(&json!({"cells": {"A1": "x"}})).is_err());
    }
}
