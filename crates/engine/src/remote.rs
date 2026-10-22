//! Remote operations: cell changes another writer committed on a host that owns the document
//! (an embedding page with a server-side revision log).
//!
//! [`DocState::apply_remote`] applies them to the open document **without** making them local
//! history: no undo step, the redo stack stays, no journal entry, the selection and anything the
//! UI holds (an open cell editor) stay as they are.
//!
//! Undo/redo keep their snapshot model. Each [`crate::HistoryEntry`] remembers the remote sequence
//! number its snapshot was taken at; the document keeps the remote ops applied since the oldest
//! snapshot still in history. Restoring a snapshot (undo/redo) re-applies the remote ops that came
//! after it, so undoing a local edit never reverts somebody else's change. Cost: O(ops replayed)
//! per undo, O(1) per remote op; no snapshot is rewritten.
//!
//! Semantics are last-writer-wins per cell, as the host's cell-level precondition decides them:
//! when a remote op later overwrote a cell a local edit had set, undoing that edit leaves the
//! remote value. Only cell content ops exist (`cell.set`, `cell.clear`); structural edits (rows,
//! columns, sheets) aren't remote ops, the host refuses them for collaborative sessions.
use std::sync::Arc;

use gridcraft_core::CellRef;
use serde_json::Value as Json;

use crate::{DocState, EngineError, Result, Selection};

/// One remote cell change. `input` is what was typed (canonical, en-US formula syntax);
/// an empty input clears the cell's content (its format stays, like `cell.set` with "").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteOp {
    /// Sheet name (names are stable across sessions; indexes aren't).
    pub sheet: String,
    pub cell: CellRef,
    pub input: String,
}

impl RemoteOp {
    pub fn set(sheet: &str, cell: &str, input: &str) -> Result<RemoteOp> {
        let at = CellRef::parse(cell).ok_or_else(|| EngineError::Other(format!("remote op: bad cell `{cell}`")))?;
        Ok(RemoteOp { sheet: sheet.to_string(), cell: at, input: input.to_string() })
    }

    /// `{"op": "cell.set", "sheet": "Sheet1", "cell": "B2", "input": "42"}` or
    /// `{"op": "cell.clear", "sheet": "Sheet1", "cell": "B2"}`.
    pub fn from_json(v: &Json) -> Result<RemoteOp> {
        let s = |k: &str| v.get(k).and_then(Json::as_str);
        let op = s("op").unwrap_or("");
        let sheet = s("sheet").ok_or_else(|| EngineError::Other("remote op: missing `sheet`".into()))?;
        let cell = s("cell").ok_or_else(|| EngineError::Other("remote op: missing `cell`".into()))?;
        match op {
            "cell.set" => {
                let input = v.get("input").map(crate::cmd::edit::json_to_input).unwrap_or_default();
                RemoteOp::set(sheet, cell, &input)
            }
            "cell.clear" => RemoteOp::set(sheet, cell, ""),
            other => Err(EngineError::Other(format!("remote op: unsupported `{other}`"))),
        }
    }
}

/// Remote ops applied since the oldest history snapshot, by local sequence number.
#[derive(Clone, Debug, Default)]
pub struct RemoteLog {
    /// Sequence number of the last applied batch (0 = none). History entries record it.
    pub seq: u64,
    /// Host revision of the last applied batch (0 = none). Older revisions are ignored; the same
    /// revision applies again (a reconcile to the host's values at that revision).
    pub host_rev: u64,
    entries: Vec<(u64, Arc<[RemoteOp]>)>,
}

impl RemoteLog {
    /// Ops applied after sequence number `seq`, oldest first.
    fn since(&self, seq: u64) -> impl Iterator<Item = &RemoteOp> {
        self.entries.iter().filter(move |(s, _)| *s > seq).flat_map(|(_, ops)| ops.iter())
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Writes `ops` into `wb` as `cell.set` would (style inheritance, auto number format, formula
/// parsing), then recalculates the changed cells. `calc` must match `wb` on entry.
/// `strict`: an op on a missing sheet is an error (live apply); lenient replay into an old
/// snapshot skips it (the sheet didn't exist yet at that point of history).
fn write_ops<'a>(
    wb: &mut gridcraft_model::Workbook,
    calc: &mut gridcraft_calc::Calc,
    ops: impl Iterator<Item = &'a RemoteOp>,
    strict: bool,
) -> Result<()> {
    let mut sel = Selection::default();
    let mut cx = crate::cmd::Ctx {
        wb: std::mem::take(wb),
        changed: Vec::new(),
        structural: false,
        sel: &mut sel,
        fit_rows: Vec::new(),
        protection_checked: true,
    };
    let mut err = None;
    for op in ops {
        let Some(si) = cx.wb.sheet_index(&op.sheet) else {
            if strict {
                err = Some(EngineError::Other(format!("remote op: no sheet `{}`", op.sheet)));
                break;
            }
            continue;
        };
        match crate::cmd::edit::input_to_cell(&op.input, si, op.cell, &mut cx.wb) {
            Ok(cell) => {
                let Ok(sh) = cx.sheet_mut(si) else { continue };
                match cell {
                    Some(c) => sh.set_cell(op.cell, c),
                    None => {
                        sh.remove_cell(op.cell);
                    }
                }
                cx.touch(si, op.cell);
            }
            Err(e) if strict => {
                err = Some(e);
                break;
            }
            Err(_) => {}
        }
    }
    if err.is_none() {
        crate::tables::sync_headers(&mut cx);
    }
    let crate::cmd::Ctx { wb: mut out, changed, .. } = cx;
    if let Some(e) = err {
        // Callers pass a scratch copy; nothing of it is kept on error.
        *wb = out;
        return Err(e);
    }
    calc.cells_changed(&mut out, &changed);
    let mut rows: Vec<(usize, u32)> = changed.iter().map(|(s, c)| (*s, c.row)).collect();
    rows.sort_unstable();
    rows.dedup();
    for (si, row) in rows {
        crate::cmd::auto_row_height(&mut out, si, row);
    }
    *wb = out;
    Ok(())
}

impl DocState {
    /// Applies a batch of remote ops committed on the host as revision `rev`.
    ///
    /// Not an undo step, doesn't clear redo, isn't journaled, keeps the selection. A document
    /// that had no local changes stays clean (the host already has this state). Returns
    /// `Ok(false)` without changes for a `rev` below the last applied one (a late delivery). The
    /// same `rev` applies again: ops are absolute values, so a duplicate is harmless and a
    /// reconcile after a rejected local write uses the host's current revision. On error (unknown sheet, unparsable formula) nothing changes and the caller
    /// should resync from a host snapshot.
    pub fn apply_remote(&mut self, rev: u64, ops: &[RemoteOp]) -> Result<bool> {
        if rev < self.remote.host_rev {
            return Ok(false);
        }
        let mut wb = (*self.wb).clone();
        let mut calc = self.calc.clone();
        write_ops(&mut wb, &mut calc, ops.iter(), true)?;
        let clean = !self.is_dirty();
        self.wb = Arc::new(wb);
        self.calc = calc;
        if clean {
            self.saved = self.wb.clone();
        }
        self.remote.host_rev = rev;
        self.remote.seq += 1;
        if !ops.is_empty() {
            let seq = self.remote.seq;
            self.remote.entries.push((seq, ops.to_vec().into()));
        }
        self.prune_remote_log();
        // Views key their caches on the revision.
        self.revision += 1;
        Ok(true)
    }

    /// After undo/redo restored a snapshot taken at remote sequence `seq`: re-applies the remote
    /// ops that came later. `self.calc` must already match the restored snapshot.
    pub(crate) fn replay_remote_since(&mut self, seq: u64) {
        if seq >= self.remote.seq {
            return;
        }
        let ops: Vec<RemoteOp> = self.remote.since(seq).cloned().collect();
        if ops.is_empty() {
            return;
        }
        let mut wb = (*self.wb).clone();
        let mut calc = self.calc.clone();
        if write_ops(&mut wb, &mut calc, ops.iter(), false).is_ok() {
            self.wb = Arc::new(wb);
            self.calc = calc;
        }
    }

    /// Drops remote ops that every snapshot in history already contains.
    pub(crate) fn prune_remote_log(&mut self) {
        let oldest = self.undo.iter().chain(self.redo.iter()).map(|h| h.remote_seq).min();
        match oldest {
            None => self.remote.entries.clear(),
            Some(min) => self.remote.entries.retain(|(s, _)| *s > min),
        }
    }
}

impl crate::Session {
    /// [`DocState::apply_remote`] on the active document.
    pub fn apply_remote(&mut self, rev: u64, ops: &[RemoteOp]) -> Result<bool> {
        self.doc_mut()?.apply_remote(rev, ops)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Session;
    use serde_json::json;

    fn session() -> Session {
        let mut s = Session::new();
        s.new_workbook();
        s
    }
    fn input(s: &Session, a1: &str) -> String {
        let d = s.doc().unwrap();
        d.wb.sheet(0).unwrap().input_text(CellRef::parse(a1).unwrap())
    }
    fn value(s: &Session, a1: &str) -> String {
        let d = s.doc().unwrap();
        d.wb.sheet(0).unwrap().value(CellRef::parse(a1).unwrap()).display()
    }
    fn set(s: &mut Session, a1: &str, v: &str) {
        s.execute("cell.set", json!({"cell": a1, "input": v})).unwrap();
    }
    fn remote(s: &mut Session, rev: u64, a1: &str, v: &str) {
        assert!(s.apply_remote(rev, &[RemoteOp::set("Sheet1", a1, v).unwrap()]).unwrap());
    }
    fn undo(s: &mut Session) {
        s.execute("edit.undo", json!({})).unwrap();
    }
    fn redo(s: &mut Session) {
        s.execute("edit.redo", json!({})).unwrap();
    }

    #[test]
    fn remote_op_is_not_an_undo_step_and_keeps_redo_journal_and_selection() {
        let mut s = session();
        set(&mut s, "A1", "1");
        set(&mut s, "A2", "2");
        undo(&mut s);
        s.execute("selection.set", json!({"range": "C3:D4", "active": "D4"})).unwrap();
        let sel = s.doc().unwrap().selection.clone();
        let journal = s.journal.len();
        let (undo_len, redo_len) = (s.doc().unwrap().undo.len(), s.doc().unwrap().redo.len());

        remote(&mut s, 5, "B2", "99");

        let d = s.doc().unwrap();
        assert_eq!(input(&s, "B2"), "99");
        assert_eq!((d.undo.len(), d.redo.len()), (undo_len, redo_len), "no undo step, redo kept");
        assert_eq!(d.selection, sel, "selection unchanged");
        assert_eq!(s.journal.len(), journal, "not journaled");
    }

    #[test]
    fn undo_and_redo_of_local_edits_never_revert_a_remote_op() {
        let mut s = session();
        set(&mut s, "A1", "local1");
        remote(&mut s, 2, "B2", "99");
        set(&mut s, "A2", "local2");
        remote(&mut s, 4, "B3", "agent");

        undo(&mut s);
        assert_eq!(input(&s, "A2"), "");
        assert_eq!((input(&s, "B2").as_str(), input(&s, "B3").as_str()), ("99", "agent"));
        undo(&mut s);
        assert_eq!(input(&s, "A1"), "");
        assert_eq!((input(&s, "B2").as_str(), input(&s, "B3").as_str()), ("99", "agent"));
        assert!(s.doc().unwrap().undo.is_empty(), "the remote ops were never undo steps");

        redo(&mut s);
        redo(&mut s);
        assert_eq!((input(&s, "A1").as_str(), input(&s, "A2").as_str()), ("local1", "local2"));
        assert_eq!((input(&s, "B2").as_str(), input(&s, "B3").as_str()), ("99", "agent"));
    }

    #[test]
    fn multi_step_undo_replays_remote_ops_once() {
        let mut s = session();
        set(&mut s, "A1", "1");
        set(&mut s, "A2", "2");
        remote(&mut s, 3, "B1", "r");
        set(&mut s, "A3", "3");
        s.execute("edit.undo", json!({"steps": 3})).unwrap();
        assert_eq!((input(&s, "A1").as_str(), input(&s, "A2").as_str(), input(&s, "A3").as_str()), ("", "", ""));
        assert_eq!(input(&s, "B1"), "r");
        s.execute("edit.redo", json!({"steps": 3})).unwrap();
        assert_eq!((input(&s, "A1").as_str(), input(&s, "A3").as_str(), input(&s, "B1").as_str()), ("1", "3", "r"));
    }

    #[test]
    fn dependents_recalculate_after_remote_ops_and_after_replay() {
        let mut s = session();
        set(&mut s, "A1", "1");
        set(&mut s, "B1", "=A1*10");
        remote(&mut s, 3, "A1", "5");
        assert_eq!(value(&s, "B1"), "50");
        set(&mut s, "C1", "x");
        undo(&mut s);
        assert_eq!(value(&s, "B1"), "50", "formula over a remote input stays current after undo");
        undo(&mut s); // B1's formula goes; the remote A1 stays
        assert_eq!((input(&s, "A1").as_str(), input(&s, "B1").as_str()), ("5", ""));
        redo(&mut s);
        assert_eq!(value(&s, "B1"), "50");
    }

    #[test]
    fn remote_write_after_a_local_write_of_the_same_cell_wins_over_undo() {
        let mut s = session();
        set(&mut s, "B2", "5");
        remote(&mut s, 2, "B2", "7");
        undo(&mut s);
        assert_eq!(input(&s, "B2"), "7", "undo doesn't resurrect the value the host overwrote");
    }

    #[test]
    fn old_revision_is_ignored_same_revision_reconciles() {
        let mut s = session();
        remote(&mut s, 3, "A1", "x");
        let rev = s.doc().unwrap().revision;
        assert!(!s.apply_remote(2, &[RemoteOp::set("Sheet1", "A1", "z").unwrap()]).unwrap());
        assert_eq!(input(&s, "A1"), "x");
        assert_eq!(s.doc().unwrap().revision, rev);
        // A rejected local write is reverted to the host's value at the current revision.
        set(&mut s, "A2", "mine");
        assert!(s.apply_remote(3, &[RemoteOp::set("Sheet1", "A2", "host").unwrap()]).unwrap());
        assert_eq!(input(&s, "A2"), "host");
        undo(&mut s);
        assert_eq!(input(&s, "A2"), "host", "undoing the rejected write doesn't bring it back");
    }

    #[test]
    fn bad_batch_changes_nothing() {
        let mut s = session();
        set(&mut s, "A1", "keep");
        let before = s.doc().unwrap().wb.clone();
        let ops = [RemoteOp::set("Sheet1", "A2", "ok").unwrap(), RemoteOp::set("Nope", "A3", "x").unwrap()];
        assert!(s.apply_remote(2, &ops).is_err());
        assert!(Arc::ptr_eq(&s.doc().unwrap().wb, &before));
        assert_eq!(s.doc().unwrap().remote.host_rev, 0);
        assert!(s.apply_remote(2, &[RemoteOp::set("Sheet1", "A2", "=SUM(1").unwrap()]).is_ok(), "parens are closed like typing");
        assert!(s.apply_remote(3, &[RemoteOp::set("Sheet1", "A2", "=1+").unwrap()]).is_err());
    }

    #[test]
    fn clean_document_stays_clean_dirty_stays_dirty() {
        let mut s = session();
        remote(&mut s, 1, "A1", "x");
        assert!(!s.doc().unwrap().is_dirty(), "the host already has it");
        set(&mut s, "A2", "mine");
        remote(&mut s, 2, "A3", "y");
        assert!(s.doc().unwrap().is_dirty());
    }

    #[test]
    fn remote_ops_reach_protected_cells_the_host_accepted() {
        let mut s = session();
        s.execute("review.protectSheet", json!({})).unwrap();
        assert!(s.execute("cell.set", json!({"cell": "A1", "input": "1"})).is_err());
        remote(&mut s, 1, "A1", "host");
        assert_eq!(input(&s, "A1"), "host");
    }

    #[test]
    fn json_ops_and_clear() {
        let mut s = session();
        set(&mut s, "A1", "x");
        let ops: Vec<RemoteOp> =
            [json!({"op": "cell.set", "sheet": "sheet1", "cell": "B1", "input": 42}), json!({"op": "cell.clear", "sheet": "Sheet1", "cell": "A1"})]
                .iter()
                .map(|v| RemoteOp::from_json(v).unwrap())
                .collect();
        s.apply_remote(1, &ops).unwrap();
        assert_eq!((input(&s, "A1").as_str(), value(&s, "B1").as_str()), ("", "42"));
        assert!(RemoteOp::from_json(&json!({"op": "rows.insert", "sheet": "Sheet1", "cell": "A1"})).is_err());
    }

    #[test]
    fn log_holds_only_ops_newer_than_the_oldest_snapshot() {
        let mut s = session();
        for rev in 1..=50 {
            remote(&mut s, rev, "A1", &rev.to_string());
        }
        assert!(s.doc().unwrap().remote.is_empty(), "no history: nothing to replay into");
        set(&mut s, "B1", "x");
        remote(&mut s, 51, "A1", "51");
        assert_eq!(s.doc().unwrap().remote.len(), 1);
        undo(&mut s);
        // The restored state was replayed up to rev 51 and the redo entry is the current state:
        // no snapshot predates rev 51 any more.
        assert!(s.doc().unwrap().remote.is_empty());
        redo(&mut s);
        assert_eq!(input(&s, "A1"), "51");
        set(&mut s, "B2", "y");
        s.doc_mut().unwrap().undo.clear();
        remote(&mut s, 52, "A1", "52");
        assert!(s.doc().unwrap().remote.is_empty());
        assert_eq!(input(&s, "A1"), "52");
    }
}
