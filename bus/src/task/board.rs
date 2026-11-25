//! `BOARD.tsv` parser and importer for `sommelier task import-board` (section 5.1, step 8).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// One row from a 7-column `BOARD.tsv` file (`id\ttopic\towner\tclass\tbranch\tfiles\tstate`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardRow {
    pub id: String,
    pub topic: String,
    pub owner: String,
    pub class: String,
    pub branch: String,
    pub files: String,
    pub state: String,
}

/// Parses a `BOARD.tsv` string into [`BoardRow`] entries.
///
/// Ignores UTF-8 BOM, blank lines, `#` comments, and an optional header row (`id\ttopic\t...`).
pub fn parse_board_tsv(text: &str) -> Result<Vec<BoardRow>, String> {
    let mut rows = Vec::new();
    for (idx, raw) in text.trim_start_matches('\u{feff}').lines().enumerate() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = raw.split('\t').map(str::trim).collect();
        if cols.len() != 7 {
            return Err(format!(
                "BOARD.tsv line {} needs 7 tab-separated columns (got {})",
                idx + 1,
                cols.len()
            ));
        }
        if idx == 0 && cols[0].eq_ignore_ascii_case("id") && cols[1].eq_ignore_ascii_case("topic") {
            continue;
        }
        if cols[0].is_empty() {
            return Err(format!("BOARD.tsv line {} has empty id", idx + 1));
        }
        rows.push(BoardRow {
            id: cols[0].to_owned(),
            topic: cols[1].to_owned(),
            owner: cols[2].to_owned(),
            class: cols[3].to_owned(),
            branch: cols[4].to_owned(),
            files: cols[5].to_owned(),
            state: cols[6].to_owned(),
        });
    }
    Ok(rows)
}

/// Maps a `BOARD.tsv` row into a `task.add` module-op JSON payload.
pub fn board_row_to_task_payload(row: &BoardRow) -> Value {
    let mapped_state = match row.state.trim().to_ascii_lowercase().as_str() {
        "done" | "landed" | "closed" => "closed",
        "cancelled" | "canceled" | "dropped" => "cancelled",
        "stuck" => "stuck",
        "draft" => "draft",
        _ => "ready",
    };
    let accepts = if row.class == "-" {
        String::new()
    } else {
        row.class.clone()
    };
    let branch = if row.branch == "-" {
        String::new()
    } else {
        row.branch.clone()
    };
    let scope = if row.files == "-" || row.files.is_empty() {
        row.topic.clone()
    } else {
        row.files.clone()
    };
    json!({
        "id": row.id,
        "title": row.topic,
        "kind": "fix",
        "priority": "normal",
        "state": mapped_state,
        "scope": scope,
        "accepts": accepts,
        "branch": branch,
        "done_when": format!("imported from BOARD.tsv ({})", row.topic),
    })
}
