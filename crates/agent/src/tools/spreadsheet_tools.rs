//! Spreadsheet agent tools — read and edit Excel/ODS workbooks.
//!
//! Reading is backed by `calamine` (xlsx/xls/ods/xlsb, read-only), editing by
//! `umya-spreadsheet`, which parses the whole workbook and writes it back
//! with the user's formatting, formulas and charts preserved.

use std::path::PathBuf;
use std::sync::Arc;

use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result, bail};
use calamine::{Data, Reader, open_workbook_auto};
use gpui::{App, AppContext as _, Entity, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::slides_tool::first_worktree_dir;

const DEFAULT_MAX_ROWS: usize = 200;
const MAX_COLUMNS: usize = 60;

/// Resolves a user-supplied path: absolute paths are used as-is, relative
/// paths land inside the first visible worktree of the project.
pub(crate) fn resolve_workspace_path(
    project: &Entity<Project>,
    cx: &mut App,
    raw: &str,
) -> PathBuf {
    let candidate = PathBuf::from(raw);
    if candidate.is_absolute() {
        candidate
    } else {
        first_worktree_dir(project, cx).join(candidate)
    }
}

fn col_letters_to_index(letters: &str) -> Option<usize> {
    if letters.is_empty() {
        return None;
    }
    let mut index = 0usize;
    for ch in letters.chars() {
        let upper = ch.to_ascii_uppercase();
        if !upper.is_ascii_uppercase() {
            return None;
        }
        index = index * 26 + (upper as usize - 'A' as usize + 1);
    }
    Some(index - 1)
}

/// "B7" -> zero-based (row 6, col 1).
fn parse_cell_address(addr: &str) -> Option<(usize, usize)> {
    let split = addr.find(|c: char| c.is_ascii_digit())?;
    let (letters, digits) = addr.split_at(split);
    let col = col_letters_to_index(letters)?;
    let row: usize = digits.parse().ok()?;
    Some((row - 1, col))
}

fn parse_cell_range(range: &str) -> Option<((usize, usize), (usize, usize))> {
    let (start, end) = range.split_once(':')?;
    let (s, e) = (parse_cell_address(start)?, parse_cell_address(end)?);
    Some((s.min(e), s.max(e)))
}

fn index_to_col_letters(mut index: usize) -> String {
    let mut out = Vec::new();
    loop {
        out.push(b'A' + (index % 26) as u8);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

fn coord_to_address(row: usize, col: usize) -> String {
    format!("{}{}", index_to_col_letters(col), row + 1)
}

fn format_data(data: &Data) -> String {
    match data {
        Data::Empty => String::new(),
        Data::String(text) => text.clone(),
        Data::Int(value) => value.to_string(),
        Data::Float(value) => {
            if value.fract().abs() < f64::EPSILON && value.abs() < 1e15 {
                format!("{}", *value as i64)
            } else {
                value.to_string()
            }
        }
        Data::Bool(value) => value.to_string(),
        Data::DateTime(value) => {
            // Excel serial (1900 epoch). Convert with the 25569-day offset
            // to the Unix epoch instead of calamine's feature-gated helpers.
            let serial = value.as_f64();
            let (days, frac) = (serial.floor(), serial - serial.floor());
            let unix_days = days as i64 - 25569;
            let secs = unix_days * 86_400 + (frac * 86_400.0).round() as i64;
            match chrono::NaiveDateTime::from_timestamp_opt(secs, 0) {
                Some(datetime) => datetime.format("%Y-%m-%d %H:%M:%S").to_string(),
                None => serial.to_string(),
            }
        }
        Data::DateTimeIso(value) => value.clone(),
        Data::DurationIso(value) => value.clone(),
        Data::Error(value) => format!("#{value:?}"),
    }
}

fn escape_pipes(cell: &str) -> String {
    cell.replace('|', "\\|")
}

// ── read ────────────────────────────────────────────────────────────────────

/// Read rows from a spreadsheet file (.xlsx, .xls, .ods, .xlsb).
/// Use this when the user asks about the contents of an Excel file, or
/// before editing one. Returns a markdown table plus workbook metadata.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReadSpreadsheetToolInput {
    /// Path to the spreadsheet file. Relative paths resolve inside the project.
    path: String,
    /// Sheet name to read. Defaults to the first sheet.
    sheet: Option<String>,
    /// Cell range to read, e.g. "A1:F100". Defaults to the whole used range.
    range: Option<String>,
    /// Maximum number of rows to return (default 200). Read again with a
    /// `range` for rows beyond the cap.
    max_rows: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SpreadsheetRead {
    sheet: String,
    sheets: Vec<String>,
    rows_total: usize,
    rows_returned: usize,
    truncated: bool,
    table_markdown: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReadSpreadsheetToolOutput {
    Success(SpreadsheetRead),
    Error { error: String },
}

impl From<ReadSpreadsheetToolOutput> for LanguageModelToolResultContent {
    fn from(value: ReadSpreadsheetToolOutput) -> Self {
        match value {
            ReadSpreadsheetToolOutput::Success(read) => format!(
                "Sheets: {:?}. Reading `{}`: {} of {} rows{}.\n\n{}",
                read.sheets,
                read.sheet,
                read.rows_returned,
                read.rows_total,
                if read.truncated {
                    " (truncated — call again with a `range` for the rest)"
                } else {
                    ""
                },
                read.table_markdown
            )
            .into(),
            ReadSpreadsheetToolOutput::Error { error } => error.into(),
        }
    }
}

pub struct ReadSpreadsheetTool {
    project: Entity<Project>,
}

impl ReadSpreadsheetTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for ReadSpreadsheetTool {
    type Input = ReadSpreadsheetToolInput;
    type Output = ReadSpreadsheetToolOutput;

    const NAME: &'static str = "read_spreadsheet";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Reading spreadsheet".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|e| ReadSpreadsheetToolOutput::Error {
                    error: e.to_string(),
                })?;

            let authorize = cx.update(|cx| {
                let context =
                    crate::ToolPermissionContext::new(Self::NAME, vec![input.path.clone()]);
                event_stream.authorize(format!("Read spreadsheet {}", input.path), context, cx)
            });
            authorize
                .await
                .map_err(|e| ReadSpreadsheetToolOutput::Error {
                    error: e.to_string(),
                })?;

            let path = cx.update(|cx| resolve_workspace_path(&project, cx, &input.path));
            let task = cx.update(|cx| {
                let input = input.clone();
                cx.background_spawn(async move { read_impl(&path, &input) })
            });
            Ok(task.await)
        })
    }
}

fn read_impl(path: &PathBuf, input: &ReadSpreadsheetToolInput) -> ReadSpreadsheetToolOutput {
    match read_inner(path, input) {
        Ok(read) => ReadSpreadsheetToolOutput::Success(read),
        Err(error) => ReadSpreadsheetToolOutput::Error {
            error: format!("{error:#}"),
        },
    }
}

fn read_inner(path: &PathBuf, input: &ReadSpreadsheetToolInput) -> Result<SpreadsheetRead> {
    let mut workbook = open_workbook_auto(path).with_context(|| format!("open {:?}", path))?;
    let sheet_names = workbook.sheet_names().to_vec();
    if sheet_names.is_empty() {
        bail!("the workbook contains no sheets");
    }
    let sheet_name = match &input.sheet {
        Some(name) => {
            if !sheet_names.iter().any(|existing| existing == name) {
                bail!("sheet `{name}` not found; available sheets: {sheet_names:?}");
            }
            name.clone()
        }
        None => sheet_names[0].clone(),
    };

    let full_range = workbook
        .worksheet_range(&sheet_name)
        .with_context(|| format!("read sheet {sheet_name}"))?;
    let (raw_start_row, raw_start_col) = full_range.start().unwrap_or((0, 0));
    let (start_row, start_col) = (raw_start_row as usize, raw_start_col as usize);
    let (row_start, col_start, row_end, col_end) = match &input.range {
        Some(range) => {
            let ((r1, c1), (r2, c2)) =
                parse_cell_range(range).with_context(|| format!("invalid range `{range}`"))?;
            (r1, c1, Some(r2), Some(c2))
        }
        None => (0, 0, None, None),
    };

    let max_rows = input.max_rows.unwrap_or(DEFAULT_MAX_ROWS).min(2000);
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut rows_total = 0usize;
    for (i, row) in full_range.rows().enumerate() {
        let abs_row = start_row + i;
        if let Some(r2) = row_end {
            if abs_row > r2 {
                break;
            }
        }
        if abs_row < row_start {
            continue;
        }
        rows_total += 1;
        if table_rows.len() >= max_rows {
            continue;
        }
        let mut values = Vec::new();
        for (j, cell) in row.iter().enumerate() {
            let abs_col = start_col + j;
            if abs_col < col_start {
                continue;
            }
            if let Some(c2) = col_end {
                if abs_col > c2 {
                    break;
                }
            }
            if values.len() >= MAX_COLUMNS {
                break;
            }
            values.push(format_data(cell));
        }
        table_rows.push(values);
    }

    let truncated = rows_total > table_rows.len();
    let mut table_markdown = String::new();
    if let Some(header) = table_rows.first() {
        markdown_row(&mut table_markdown, header);
        table_markdown.push('|');
        for _ in header {
            table_markdown.push_str(" --- |");
        }
        table_markdown.push('\n');
    }
    for row in table_rows.iter().skip(1) {
        markdown_row(&mut table_markdown, row);
    }

    Ok(SpreadsheetRead {
        sheet: sheet_name,
        sheets: sheet_names,
        rows_total,
        rows_returned: table_rows.len(),
        truncated,
        table_markdown,
    })
}

fn markdown_row(out: &mut String, row: &[String]) {
    out.push_str("| ");
    for cell in row {
        out.push_str(&escape_pipes(cell));
        out.push_str(" | ");
    }
    out.push('\n');
}

// ── edit ────────────────────────────────────────────────────────────────────

/// An edit to apply to a spreadsheet cell range or sheet.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SpreadsheetEdit {
    /// Set a cell's value. Numeric strings are stored as numbers and values
    /// starting with "=" as formulas.
    SetCell { cell: String, value: String },
    /// Append rows after the last used row of the sheet.
    AppendRows { rows: Vec<Vec<String>> },
    /// Make every cell in the range bold, e.g. "A1:F1" (handy for headers).
    SetBold { range: String },
    /// Create a new sheet with the given name (ignored if it already exists).
    CreateSheet { name: String },
}

/// Edit an .xlsx workbook in place: set cells, append rows, format ranges,
/// create sheets. The workbook is rewritten with the user's existing
/// formatting, formulas and charts preserved.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct EditSpreadsheetToolInput {
    /// Path to the .xlsx file. Created as a fresh workbook when missing.
    path: String,
    /// Target sheet name. Defaults to the first sheet (or the sheet created
    /// by a `create_sheet` edit).
    sheet: Option<String>,
    /// The edits to apply, in order.
    edits: Vec<SpreadsheetEdit>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EditSpreadsheetToolOutput {
    Success { summary: String },
    Error { error: String },
}

impl From<EditSpreadsheetToolOutput> for LanguageModelToolResultContent {
    fn from(value: EditSpreadsheetToolOutput) -> Self {
        match value {
            EditSpreadsheetToolOutput::Success { summary } => summary.into(),
            EditSpreadsheetToolOutput::Error { error } => error.into(),
        }
    }
}

pub struct EditSpreadsheetTool {
    project: Entity<Project>,
}

impl EditSpreadsheetTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for EditSpreadsheetTool {
    type Input = EditSpreadsheetToolInput;
    type Output = EditSpreadsheetToolOutput;

    const NAME: &'static str = "edit_spreadsheet";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Edit
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Editing spreadsheet".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|e| EditSpreadsheetToolOutput::Error {
                    error: e.to_string(),
                })?;

            let authorize = cx.update(|cx| {
                let context =
                    crate::ToolPermissionContext::new(Self::NAME, vec![input.path.clone()]);
                event_stream.authorize(format!("Edit spreadsheet {}", input.path), context, cx)
            });
            authorize
                .await
                .map_err(|e| EditSpreadsheetToolOutput::Error {
                    error: e.to_string(),
                })?;

            let path = cx.update(|cx| resolve_workspace_path(&project, cx, &input.path));
            let task = cx.update(|cx| {
                let input = input.clone();
                cx.background_spawn(async move { edit_impl(&path, &input) })
            });
            Ok(task.await)
        })
    }
}

fn edit_impl(path: &PathBuf, input: &EditSpreadsheetToolInput) -> EditSpreadsheetToolOutput {
    match edit_inner(path, input) {
        Ok(summary) => EditSpreadsheetToolOutput::Success { summary },
        Err(error) => EditSpreadsheetToolOutput::Error {
            error: format!("{error:#}"),
        },
    }
}

fn edit_inner(path: &PathBuf, input: &EditSpreadsheetToolInput) -> Result<String> {
    let path_str = path.to_str().context("path is not valid UTF-8")?;
    let mut book = if path.exists() {
        umya_spreadsheet::reader::xlsx::read(path_str)
            .with_context(|| format!("open {path_str}"))?
    } else {
        umya_spreadsheet::new_file()
    };

    // CreateSheet ops run first (in order) so later edits can target the new
    // sheet when no explicit `sheet` was given.
    let mut applied: Vec<String> = Vec::new();
    for edit in &input.edits {
        if let SpreadsheetEdit::CreateSheet { name } = edit {
            let exists = book
                .get_sheet_collection()
                .iter()
                .any(|sheet| sheet.get_name() == name.as_str());
            if !exists {
                book.new_sheet(name.as_str())
                    .map_err(|e| anyhow::anyhow!("create sheet `{name}`: {e}"))?;
            }
            applied.push(format!("created sheet `{name}`"));
        }
    }

    let target_name = input
        .sheet
        .clone()
        .or_else(|| {
            input.edits.iter().rev().find_map(|edit| match edit {
                SpreadsheetEdit::CreateSheet { name } => Some(name.clone()),
                _ => None,
            })
        })
        .unwrap_or_else(|| {
            book.get_sheet_collection()
                .first()
                .map(|sheet| sheet.get_name().to_string())
                .unwrap_or_else(|| "Sheet1".to_string())
        });

    for edit in &input.edits {
        let sheet = book
            .get_sheet_collection_mut()
            .iter_mut()
            .find(|sheet| sheet.get_name() == target_name.as_str())
            .ok_or_else(|| anyhow::anyhow!("sheet `{target_name}` not found"))?;
        match edit {
            SpreadsheetEdit::CreateSheet { .. } => {}
            SpreadsheetEdit::SetCell { cell, value } => {
                sheet.get_cell_mut(cell.as_str()).set_value(value.as_str());
                applied.push(format!("set {cell}"));
            }
            SpreadsheetEdit::AppendRows { rows } => {
                let start = sheet.get_highest_row() as usize;
                for (ri, row) in rows.iter().enumerate() {
                    for (ci, value) in row.iter().enumerate() {
                        let address = coord_to_address(start + ri, ci);
                        sheet
                            .get_cell_mut(address.as_str())
                            .set_value(value.as_str());
                    }
                }
                applied.push(format!("appended {} row(s)", rows.len()));
            }
            SpreadsheetEdit::SetBold { range } => {
                let ((r1, c1), (r2, c2)) =
                    parse_cell_range(range).with_context(|| format!("invalid range `{range}`"))?;
                for r in r1..=r2 {
                    for c in c1..=c2 {
                        let cell = sheet.get_cell_mut(coord_to_address(r, c).as_str());
                        let mut style = cell.get_style().clone();
                        style.get_font_mut().set_bold(true);
                        cell.set_style(style);
                    }
                }
                applied.push(format!("bolded {range}"));
            }
        }
    }

    umya_spreadsheet::writer::xlsx::write(&book, path_str)
        .with_context(|| format!("save {path_str}"))?;
    applied.push(format!("saved {path_str}"));
    Ok(applied.join("; "))
}
