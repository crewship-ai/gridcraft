//! Temporary capability boundary for the cell-operation embedding. Default off:
//! standalone GridCraft keeps its full command surface. Expand this only when
//! the host protocol persists and synchronizes the additional operation.
use serde_json::Value as Json;

use crate::Session;

pub const UNSUPPORTED: &str = "This action is not supported in this editor yet. Only cell values and formulas are saved.";

impl Session {
    pub fn check_host_command(&self, id: &str, params: Option<&Json>) -> Result<(), String> {
        if !self.host_cells_only {
            return Ok(());
        }
        let allowed = matches!(
            id,
            "cell.set"
                | "range.setValues"
                | "range.fill"
                | "selection.set"
                | "selection.move"
                | "selection.next"
                | "selection.currentRegion"
                | "selection.row"
                | "selection.column"
                | "edit.selectAll"
                | "edit.goTo"
                | "edit.goToSpecial"
                | "edit.find"
                | "edit.beginEdit"
                | "edit.copy"
                | "edit.clearClipboard"
                | "edit.clearContents"
                | "edit.undo"
                | "edit.redo"
                | "sheet.activate"
                | "sheet.next"
                | "sheet.previous"
        );
        // Plain text paste is cell content. Rich HTML and the internal clipboard
        // can carry formatting, validation and other untransported workbook data.
        let plain_paste = id == "edit.paste" && params.is_none_or(|p| p.get("text").is_some_and(Json::is_string) && p.get("html").is_none());
        let array = id == "cell.set" && params.and_then(|p| p.get("array")).and_then(Json::as_bool) == Some(true);
        if (allowed || plain_paste) && !array { Ok(()) } else { Err(UNSUPPORTED.into()) }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unavailable_commands_cannot_mutate_hosted_workbooks() {
        let mut s = Session::new();
        s.new_workbook();
        s.host_cells_only = true;
        let before = s.active().unwrap().wb.clone();
        for id in [
            "home.bold",
            "home.insertSheet",
            "sheet.rename",
            "home.insertRows",
            "home.insertColumns",
            "insert.chart",
            "file.open",
            "file.save",
            "file.new",
            "edit.clearAll",
            "edit.pasteSpecial",
        ] {
            assert!(crate::find_command(id).is_some(), "test must name a real command: {id}");
            assert!(s.execute(id, json!({})).unwrap_err().to_string().contains(UNSUPPORTED), "{id}");
            assert_eq!(serde_json::to_value(&*s.active().unwrap().wb).unwrap(), serde_json::to_value(&*before).unwrap(), "{id}");
        }
        assert!(s.execute("edit.paste", json!({"html":"<b>x</b>", "text":"x"})).is_err());
        assert!(s.execute("cell.set", json!({"cell":"A1", "input":"=1", "array":true})).is_err());
        s.execute("cell.set", json!({"cell":"A1", "input":"=1+2"})).unwrap();
        s.execute("edit.paste", json!({"at":"B1", "text":"a\tb"})).unwrap();
        assert_eq!(s.active().unwrap().wb.active().unwrap().input_text(crate::core::CellRef::parse("C1").unwrap()), "b");
        assert!(s.commands().iter().find(|c| c.id == "home.bold").is_some_and(|c| !c.enabled && c.disabled_reason.as_deref() == Some(UNSUPPORTED)));
    }
    #[test]
    fn review_plain_text_paste_never_reuses_rich_internal_clipboard() {
        let mut s = Session::new();
        s.new_workbook();
        s.execute("cell.set", json!({"cell":"A1","input":"x"})).unwrap();
        s.execute("home.bold", json!({"range":"A1"})).unwrap();
        s.host_cells_only = true;
        let b1 = crate::core::CellRef::parse("B1").unwrap();
        let before = s.active().unwrap().wb.active().unwrap().style_id(b1);
        s.execute("edit.copy", json!({"range":"A1"})).unwrap();
        s.execute("edit.paste", json!({"text":"x","at":"B1"})).unwrap();
        let after = s.active().unwrap().wb.active().unwrap().style_id(b1);
        assert_eq!(after, before, "plain text paste silently copied untransported formatting");
    }

    #[test]
    fn review_clear_contents_keeps_checkbox_format_like_remote_cell_set() {
        let mut s = Session::new();
        s.new_workbook();
        s.execute("insert.checkbox", json!({"range":"A1"})).unwrap();
        s.host_cells_only = true;
        let a1 = crate::core::CellRef::parse("A1").unwrap();
        let before = s.active().unwrap().wb.active().unwrap().style_id(a1);
        s.execute("edit.clearContents", json!({"range":"A1"})).unwrap();
        let after = s.active().unwrap().wb.active().unwrap().style_id(a1);
        assert_eq!(after, before, "hosted Delete silently changed untransported checkbox formatting");
    }
}
