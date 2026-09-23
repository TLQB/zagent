//! "Generate Slides" agent tool — drives the embedded proxy's /v1/slides
//! endpoint and packages the result as a native, text-editable .pptx.
//!
//! Running as an agent tool keeps the whole pipeline inside the agent panel:
//! the tool card title tracks every stage (request, authoring progress,
//! per-slide ops, packaging), and the final file paths land in the thread as
//! the tool result.

use std::path::PathBuf;
use std::sync::Arc;

use super::slides_pptx;
use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, bail};
use futures::{AsyncBufReadExt, StreamExt, io::BufReader};
use gpui::{App, Entity, SharedString, Task};
use http_client::{AsyncBody, HttpClient, HttpClientWithUrl, http};
use language_model::LanguageModelToolResultContent;
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Base URL of the embedded sidecar.
const SLIDES_BASE_URL: &str = "https://zai-proxy-worker.tranlequybaotk12.workers.dev";
/// Built-in sidecar auth password (mirrors the glm provider wiring).
fn proxy_token() -> String { std::env::var("ZAI_TOKEN").unwrap_or_default() }

/// Generate a slide deck from a text topic. The deck is authored by the GLM
/// model through the local proxy and packaged locally into a real PPTX whose
/// title/body text boxes stay editable in PowerPoint and LibreOffice.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GenerateSlidesToolInput {
    /// The topic describing the deck to generate. A complete 6-10 slide
    /// structure is requested unless the topic already dictates a slide count.
    prompt: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GeneratedDeck {
    slide_count: usize,
    html_path: String,
    pptx_path: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GenerateSlidesToolOutput {
    Success(GeneratedDeck),
    Error { error: String },
}

impl From<GenerateSlidesToolOutput> for LanguageModelToolResultContent {
    fn from(value: GenerateSlidesToolOutput) -> Self {
        match value {
            GenerateSlidesToolOutput::Success(deck) => format!(
                "Generated {} slides.\nHTML preview: {}\nPPTX: {}",
                deck.slide_count, deck.html_path, deck.pptx_path
            )
            .into(),
            GenerateSlidesToolOutput::Error { error } => error.into(),
        }
    }
}

/// The output directory for generated artifacts: the first visible worktree
/// of the project, falling back to the process working directory.
pub(crate) fn first_worktree_dir(project: &Entity<Project>, cx: &mut App) -> PathBuf {
    project
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| PathBuf::from(worktree.read(cx).abs_path().as_ref()))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub struct GenerateSlidesTool {
    http_client: Arc<HttpClientWithUrl>,
    project: Entity<Project>,
}

impl GenerateSlidesTool {
    pub fn new(http_client: Arc<HttpClientWithUrl>, project: Entity<Project>) -> Self {
        Self {
            http_client,
            project,
        }
    }
}

impl AgentTool for GenerateSlidesTool {
    type Input = GenerateSlidesToolInput;
    type Output = GenerateSlidesToolOutput;

    const NAME: &'static str = "generate_slides";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Fetch
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Generating slide deck".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let http_client = self.http_client.clone();
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|e| GenerateSlidesToolOutput::Error {
                    error: e.to_string(),
                })?;

            let preview_prompt: String = input.prompt.chars().take(80).collect();
            let authorize = cx.update(|cx| {
                let context =
                    crate::ToolPermissionContext::new(Self::NAME, vec![input.prompt.clone()]);
                event_stream.authorize(
                    format!("Generate a slide deck: {preview_prompt}"),
                    context,
                    cx,
                )
            });
            authorize
                .await
                .map_err(|e| GenerateSlidesToolOutput::Error {
                    error: e.to_string(),
                })?;

            event_stream.update_fields(
                acp::ToolCallUpdateFields::new().title("Requesting deck from the model…"),
            );

            let out_dir = cx.update(|cx| first_worktree_dir(&project, cx));

            let deck = collect_deck(
                http_client.clone(),
                enhance_topic(input.prompt.clone()),
                &event_stream,
            )
            .await
            .map_err(|e| GenerateSlidesToolOutput::Error {
                error: format!("slides pipeline failed: {e:#}"),
            })?;

            event_stream.update_fields(acp::ToolCallUpdateFields::new().title(format!(
                "Deck ready: {} slides — writing files…",
                deck.slides.len()
            )));

            std::fs::create_dir_all(&out_dir)
                .with_context(|| format!("create output dir {:?}", out_dir))
                .map_err(|e| GenerateSlidesToolOutput::Error {
                    error: e.to_string(),
                })?;

            let html_path = out_dir.join("slides-preview.html");
            std::fs::write(&html_path, render_preview_html(&deck))
                .with_context(|| format!("write {:?}", html_path))
                .map_err(|e| GenerateSlidesToolOutput::Error {
                    error: e.to_string(),
                })?;

            event_stream
                .update_fields(acp::ToolCallUpdateFields::new().title("Packaging editable PPTX…"));
            let pptx_bytes = slides_pptx::build(&html_to_pptx_slides(&deck), "presentation");
            let pptx_path = out_dir.join("slides.pptx");
            std::fs::write(&pptx_path, &pptx_bytes)
                .with_context(|| format!("write {:?}", pptx_path))
                .map_err(|e| GenerateSlidesToolOutput::Error {
                    error: e.to_string(),
                })?;

            Ok(GenerateSlidesToolOutput::Success(GeneratedDeck {
                slide_count: deck.slides.len(),
                html_path: html_path.display().to_string(),
                pptx_path: pptx_path.display().to_string(),
            }))
        })
    }
}

// --------------------------------------------------------------- generation

/// Wraps the raw topic with quality requirements unless the user already
/// dictated a deck size (their text mentions "slide").
fn enhance_topic(topic: String) -> String {
    if topic.to_lowercase().contains("slide") {
        return topic;
    }
    format!(
        "{topic}\n\nPlease produce a complete presentation deck of 6 to 10 slides: \
         a title slide, an agenda slide, well-developed content slides (3-5 concise \
         bullets each, each bullet a full concrete statement, not a bare keyword), \
         and a closing summary slide."
    )
}

/// One page of a generated deck (mirror of the sidecar's deck event).
#[derive(Deserialize, Clone, Debug)]
pub struct Slide {
    pub position: usize,
    pub title: String,
    pub html: String,
}

/// Final deck event from /v1/slides.
#[derive(Deserialize, Clone, Debug)]
pub struct SlideDeckEvent {
    pub conversation_id: String,
    pub slides: Vec<Slide>,
    #[serde(default)]
    pub global_css: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SlidesEvent {
    #[serde(rename = "progress")]
    Progress {
        #[serde(default)]
        chars: Option<usize>,
    },
    #[serde(rename = "op")]
    Op {
        #[serde(default)]
        tool: Option<String>,
        #[serde(default)]
        position: Option<usize>,
    },
    #[serde(rename = "op_error")]
    OpError {
        #[allow(dead_code)]
        error: String,
    },
    #[serde(rename = "error")]
    Error {
        error: String,
    },
    Deck(SlideDeckEvent),
}

/// POSTs the authoring request and consumes the SSE stream until the final
/// deck event, updating the tool card title with authoring progress.
async fn collect_deck(
    client: Arc<dyn HttpClient>,
    topic: String,
    event_stream: &ToolCallEventStream,
) -> anyhow::Result<SlideDeckEvent> {
    let body = serde_json::json!({
        "model": "glm-5.3-flash",
        "stream": true,
        "conversation_id": format!(
            "zagent-slides-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis()
        ),
        "messages": [{"role": "user", "content": topic}],
    })
    .to_string();

    let request = http::Request::builder()
        .method(http::Method::POST)
        .uri(format!("{SLIDES_BASE_URL}/v1/slides"))
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", proxy_token()))
        .body(AsyncBody::from(body))?;

    let response = client.send(request).await?;
    if !response.status().is_success() {
        bail!("slides endpoint returned {}", response.status());
    }

    let reader = BufReader::new(response.into_body());
    let mut lines = reader.lines();
    let mut last_error = None;
    while let Some(line) = lines.next().await {
        let line = line?;
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        if payload == "[DONE]" {
            break;
        }
        match serde_json::from_str::<SlidesEvent>(payload) {
            Ok(SlidesEvent::Deck(deck)) => return Ok(deck),
            Ok(SlidesEvent::Progress { chars }) => {
                if let Some(chars) = chars {
                    event_stream.update_fields(
                        acp::ToolCallUpdateFields::new()
                            .title(format!("Authoring deck… {chars} chars")),
                    );
                }
            }
            Ok(SlidesEvent::Op { tool, position }) => {
                let stage = match (tool.as_deref(), position) {
                    (Some("insert_page"), Some(position)) => format!("Adding slide {position}…"),
                    (Some("update_page"), Some(position)) => format!("Updating slide {position}…"),
                    (Some("remove_slides"), _) => "Trimming slides…".to_string(),
                    _ => "Styling the deck…".to_string(),
                };
                event_stream.update_fields(acp::ToolCallUpdateFields::new().title(stage));
            }
            Ok(SlidesEvent::Error { error }) => last_error = Some(error),
            Ok(SlidesEvent::OpError { .. }) => {}
            Err(error) => log::debug!("unparsed slides SSE event: {error}"),
        }
    }
    bail!(last_error.unwrap_or_else(|| "no deck event received".to_string()))
}

// ------------------------------------------------------------ html -> pptx

/// Extracts the text content of every `<li>` in the fragment, falling back
/// to stripped body lines when the slide uses no list markup.
fn extract_bullets(html: &str) -> Vec<String> {
    let mut bullets = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<li") {
        let after_open = &rest[start..];
        let Some(body_start) = after_open.find('>') else {
            break;
        };
        let tail = &after_open[body_start + 1..];
        let Some(end) = tail.find("</li>") else {
            break;
        };
        let text = strip_tags(&tail[..end]);
        if !text.is_empty() {
            bullets.push(text);
        }
        rest = &tail[end + 4..];
    }
    if bullets.is_empty() {
        bullets = strip_tags(html)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .take(6)
            .map(ToString::to_string)
            .collect();
    }
    bullets.truncate(8);
    bullets
}

fn strip_tags(fragment: &str) -> String {
    let mut out = String::with_capacity(fragment.len());
    let mut depth = 0usize;
    for ch in fragment.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    unescape_entities(&out)
}

fn unescape_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

fn html_to_pptx_slides(deck: &SlideDeckEvent) -> Vec<slides_pptx::Slide> {
    deck.slides
        .iter()
        .map(|slide| slides_pptx::Slide {
            title: strip_tags(&slide.title),
            bullets: extract_bullets(&slide.html),
        })
        .collect()
}

// -------------------------------------------------------------------- HTML

/// Renders a standalone preview document: one fixed-size page per slide,
/// vertically stacked, with the deck's global stylesheet inlined.
fn render_preview_html(deck: &SlideDeckEvent) -> String {
    let mut pages = String::new();
    for slide in &deck.slides {
        pages.push_str(&format!(
            "\n<div class=\"page-wrap\">\n{}\n</div>\n",
            slide.html
        ));
    }
    let title = deck
        .slides
        .first()
        .map(|s| html_escape(&strip_tags(&s.title)))
        .unwrap_or_else(|| "Slides".to_string());
    let style_open = "<style>";
    let style_close = "</style>";
    format!(
        "<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<title>{title}</title>\n{style_open}\nbody {{ background: #2a2a2e; margin: 0; padding: 24px; display: flex; flex-direction: column; align-items: center; gap: 24px; }}\n.page-wrap {{ width: 1280px; height: 720px; overflow: hidden; box-shadow: 0 8px 32px rgba(0,0,0,.45); background: #fff; }}\n{global_css}\n{style_close}\n</head>\n<body>{pages}</body>\n</html>\n",
        title = title,
        global_css = deck.global_css,
        pages = pages,
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
