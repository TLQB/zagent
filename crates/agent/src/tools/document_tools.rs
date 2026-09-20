//! Document agent tools — read .docx, write .docx, read PDF.

use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;

use crate::tools::spreadsheet_tools::resolve_workspace_path;
use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result};
use gpui::{App, Entity, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const MAX_TEXT_CHARS: usize = 120_000;

fn unescape_xml_entities(mut text: String) -> String {
    for (entity, ch) in [
        ("&amp;", '&'),
        ("&lt;", '<'),
        ("&gt;", '>'),
        ("&quot;", '"'),
        ("&apos;", '\''),
        ("&#39;", '\''),
    ] {
        text = text.replace(entity, &ch.to_string());
    }
    text
}

fn cap_chars(mut text: String) -> String {
    if text.len() > MAX_TEXT_CHARS {
        let mut cut = MAX_TEXT_CHARS;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str(
            "\n\n[document truncated — ask again with a narrower target or split the file]",
        );
    }
    text
}

// ── read .docx ──────────────────────────────────────────────────────────────

/// Read the text content of a Word document (.docx). Use this when the user
/// asks about the contents of a Word file, or before writing one.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReadDocxToolInput {
    /// Path to the .docx file. Relative paths resolve inside the project.
    path: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReadDocxToolOutput {
    Success { paragraphs: usize, text: String },
    Error { error: String },
}

impl From<ReadDocxToolOutput> for LanguageModelToolResultContent {
    fn from(value: ReadDocxToolOutput) -> Self {
        match value {
            ReadDocxToolOutput::Success { paragraphs, text } => {
                format!("{paragraphs} non-empty paragraphs:\n\n{text}").into()
            }
            ReadDocxToolOutput::Error { error } => error.into(),
        }
    }
}

pub struct ReadDocxTool {
    project: Entity<Project>,
}

impl ReadDocxTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for ReadDocxTool {
    type Input = ReadDocxToolInput;
    type Output = ReadDocxToolOutput;

    const NAME: &'static str = "read_docx";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Reading document".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|e| ReadDocxToolOutput::Error {
                error: e.to_string(),
            })?;

            let authorize = cx.update(|cx| {
                let context =
                    crate::ToolPermissionContext::new(Self::NAME, vec![input.path.clone()]);
                event_stream.authorize(format!("Read document {}", input.path), context, cx)
            });
            authorize.await.map_err(|e| ReadDocxToolOutput::Error {
                error: e.to_string(),
            })?;

            let path = cx.update(|cx| resolve_workspace_path(&project, cx, &input.path));
            let task = cx.update(|cx| {
                cx.background_spawn(async move {
                    match read_docx_impl(&path) {
                        Ok((paragraphs, text)) => ReadDocxToolOutput::Success { paragraphs, text },
                        Err(error) => ReadDocxToolOutput::Error {
                            error: format!("{error:#}"),
                        },
                    }
                })
            });
            Ok(task.await)
        })
    }
}

fn read_docx_impl(path: &PathBuf) -> Result<(usize, String)> {
    let file = std::fs::File::open(path).with_context(|| format!("open {path:?}"))?;
    let mut archive = zip::ZipArchive::new(file).context("read as zip archive")?;
    let mut xml = String::new();
    archive
        .by_name("word/document.xml")
        .context("word/document.xml not found — is this a real .docx file?")?
        .read_to_string(&mut xml)?;

    let mut paragraphs = Vec::new();
    for chunk in xml.split("</w:p>") {
        let mut para = String::new();
        let mut rest = chunk;
        while let Some(start) = rest.find("<w:t") {
            let Some(gt) = rest[start..].find('>') else {
                break;
            };
            let body = &rest[start + gt + 1..];
            let Some(end) = body.find("</w:t>") else {
                break;
            };
            para.push_str(&body[..end]);
            rest = &body[end + 6..];
        }
        let para = unescape_xml_entities(para.trim().to_string());
        if !para.is_empty() {
            paragraphs.push(para);
        }
    }
    let count = paragraphs.len();
    let text = cap_chars(paragraphs.join("\n"));
    Ok((count, text))
}

// ── write .docx ─────────────────────────────────────────────────────────────

/// Create a Word document (.docx) from plain paragraphs. A paragraph starting
/// with "# " or "## " is styled as Heading 1 / Heading 2.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WriteDocxToolInput {
    /// Path for the new .docx file (".docx" is appended when missing).
    /// Relative paths resolve inside the project.
    path: String,
    /// Optional document title, styled as Heading 1.
    title: Option<String>,
    /// Paragraphs to write, in order.
    paragraphs: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WriteDocxToolOutput {
    Success { path: String, paragraphs: usize },
    Error { error: String },
}

impl From<WriteDocxToolOutput> for LanguageModelToolResultContent {
    fn from(value: WriteDocxToolOutput) -> Self {
        match value {
            WriteDocxToolOutput::Success { path, paragraphs } => {
                format!("Wrote {paragraphs} paragraphs to {path}").into()
            }
            WriteDocxToolOutput::Error { error } => error.into(),
        }
    }
}

pub struct WriteDocxTool {
    project: Entity<Project>,
}

impl WriteDocxTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for WriteDocxTool {
    type Input = WriteDocxToolInput;
    type Output = WriteDocxToolOutput;

    const NAME: &'static str = "write_docx";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Edit
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Writing document".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|e| WriteDocxToolOutput::Error {
                error: e.to_string(),
            })?;

            let authorize = cx.update(|cx| {
                let context =
                    crate::ToolPermissionContext::new(Self::NAME, vec![input.path.clone()]);
                event_stream.authorize(format!("Write document {}", input.path), context, cx)
            });
            authorize.await.map_err(|e| WriteDocxToolOutput::Error {
                error: e.to_string(),
            })?;

            let path = cx.update(|cx| resolve_workspace_path(&project, cx, &input.path));
            let task = cx.update(|cx| {
                let input = input.clone();
                cx.background_spawn(async move {
                    match write_docx_impl(&path, &input) {
                        Ok(paragraphs) => WriteDocxToolOutput::Success {
                            path: path.display().to_string(),
                            paragraphs,
                        },
                        Err(error) => WriteDocxToolOutput::Error {
                            error: format!("{error:#}"),
                        },
                    }
                })
            });
            Ok(task.await)
        })
    }
}

fn write_docx_impl(path: &PathBuf, input: &WriteDocxToolInput) -> Result<usize> {
    let mut path = path.clone();
    if path.extension().is_none() {
        path.set_extension("docx");
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create directory {parent:?}"))?;
    }

    let mut docx = docx_rs::Docx::new();
    let mut count = 0usize;
    if let Some(title) = &input.title {
        docx = docx.add_paragraph(
            docx_rs::Paragraph::new().add_run(docx_rs::Run::new().text(title.as_str())),
        );
        count += 1;
    }
    for paragraph in &input.paragraphs {
        let (style, text) = if let Some(rest) = paragraph.strip_prefix("## ") {
            (Some("Heading2"), rest)
        } else if let Some(rest) = paragraph.strip_prefix("# ") {
            (Some("Heading1"), rest)
        } else {
            (None, paragraph.as_str())
        };
        let mut built = docx_rs::Paragraph::new();
        if let Some(style) = style {
            built = built.style(style);
        }
        docx = docx.add_paragraph(built.add_run(docx_rs::Run::new().text(text)));
        count += 1;
    }

    let mut buffer = std::io::Cursor::new(Vec::new());
    docx_rs::write_docx(&mut buffer, docx).context("serialize docx")?;
    std::fs::write(&path, buffer.into_inner()).with_context(|| format!("write {:?}", path))?;
    Ok(count)
}

// ── read .pdf ───────────────────────────────────────────────────────────────

/// Extract the text content of a PDF file. Use this when the user asks about
/// the contents of a PDF document.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReadPdfToolInput {
    /// Path to the PDF file. Relative paths resolve inside the project.
    path: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReadPdfToolOutput {
    Success { text: String },
    Error { error: String },
}

impl From<ReadPdfToolOutput> for LanguageModelToolResultContent {
    fn from(value: ReadPdfToolOutput) -> Self {
        match value {
            ReadPdfToolOutput::Success { text } => text.into(),
            ReadPdfToolOutput::Error { error } => error.into(),
        }
    }
}

pub struct ReadPdfTool {
    project: Entity<Project>,
}

impl ReadPdfTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for ReadPdfTool {
    type Input = ReadPdfToolInput;
    type Output = ReadPdfToolOutput;

    const NAME: &'static str = "read_pdf";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Reading PDF".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|e| ReadPdfToolOutput::Error {
                error: e.to_string(),
            })?;

            let authorize = cx.update(|cx| {
                let context =
                    crate::ToolPermissionContext::new(Self::NAME, vec![input.path.clone()]);
                event_stream.authorize(format!("Read PDF {}", input.path), context, cx)
            });
            authorize.await.map_err(|e| ReadPdfToolOutput::Error {
                error: e.to_string(),
            })?;

            let path = cx.update(|cx| resolve_workspace_path(&project, cx, &input.path));
            let task = cx.update(|cx| {
                cx.background_spawn(async move {
                    match pdf_extract::extract_text(path.to_string_lossy().as_ref()) {
                        Ok(text) => ReadPdfToolOutput::Success {
                            text: cap_chars(text),
                        },
                        Err(error) => ReadPdfToolOutput::Error {
                            error: format!("PDF extraction failed: {error}"),
                        },
                    }
                })
            });
            Ok(task.await)
        })
    }
}
