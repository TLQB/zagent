//! "Generate Slides" — drive the embedded proxy's /v1/slides endpoint and
//! package the result as a native, text-editable .pptx locally.
//!
//! The sidecar (zai-proxy on 127.0.0.1:3001) exposes POST /v1/slides: an
//! OpenAI-style request in, an SSE stream of progress events out, ending in
//! a deck event with one standalone HTML page per slide plus a global
//! stylesheet. The upstream Z.AI PPT export endpoints are NOT used: the old
//! /sandbox/html-to-ppt now answers 405, and the live /api/v1/convert/ppt/
//! stream only converts decks stored server-side per chatId (locally built
//! decks get "Failed to get PPT slides: 404" from upstream itself).
//! Instead `crate::pptx` renders the deck's text into real OOXML text boxes,
//! so the .pptx opens in PowerPoint/LibreOffice with editable content.
//!
//! Entry points:
//!   * the `slides` composer profile (agent panel) — the message text is the
//!     topic and this action is dispatched with it;
//!   * the command palette action, which falls back to the clipboard topic.
//!
//! Progress is reported by replacing a single toast at each stage: request
//! sent, deck size, packaging, done.

use std::path::PathBuf;
use std::sync::Arc;

use crate::pptx;
use anyhow::Context as _;
use futures::{AsyncBufReadExt, StreamExt, io::BufReader};
use gpui::AppContext as _;
use http_client::{AsyncBody, HttpClient};
use serde::Deserialize;
use workspace::Workspace;
use zed_actions::slides::GenerateSlides;

/// Base URL of the embedded sidecar.
const SLIDES_BASE_URL: &str = "http://127.0.0.1:3001";
/// Built-in sidecar auth password (mirrors the glm provider wiring).
const PROXY_PASSWORD: &str = "Waguri";

// ---------------------------------------------------------------- deck model

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
    Progress {},
    #[serde(rename = "op")]
    Op {},
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

/// POSTs the authoring request and consumes the SSE stream until the final
/// deck event (or an error event).
async fn collect_deck(
    client: Arc<dyn HttpClient>,
    topic: String,
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
        .header("Authorization", format!("Bearer {PROXY_PASSWORD}"))
        .body(AsyncBody::from(body))?;

    let response = client.send(request).await?;
    if !response.status().is_success() {
        anyhow::bail!("slides endpoint returned {}", response.status());
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
            Ok(SlidesEvent::Error { error }) => last_error = Some(error),
            Ok(_) => {}
            Err(error) => log::debug!("unparsed slides SSE event: {error}"),
        }
    }
    Err(anyhow::anyhow!(
        last_error.unwrap_or_else(|| "no deck event received".to_string())
    ))
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

fn html_to_pptx_slides(deck: &SlideDeckEvent) -> Vec<pptx::Slide> {
    deck.slides
        .iter()
        .map(|slide| pptx::Slide {
            title: strip_tags(&slide.title),
            bullets: extract_bullets(&slide.html),
        })
        .collect()
}

// -------------------------------------------------------------------- HTML

/// Renders a standalone preview document: one fixed-size page per slide,
/// vertically stacked, with the deck's global stylesheet inlined.
pub fn render_preview_html(deck: &SlideDeckEvent) -> String {
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
    let css_open = "<style>";
    let css_close = "</style>";
    format!(
        "<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<title>{title}</title>\n{css_open}\nbody {{ background: #2a2a2e; margin: 0; padding: 24px; display: flex; flex-direction: column; align-items: center; gap: 24px; }}\n.page-wrap {{ width: 1280px; height: 720px; overflow: hidden; box-shadow: 0 8px 32px rgba(0,0,0,.45); background: #fff; }}\n{global_css}\n{css_close}\n</head>\n<body>{pages}</body>\n</html>\n",
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

// ------------------------------------------------------------------ action

/// Action handler (registered on Workspace): the topic comes from the action
/// payload (slides composer mode) or, failing that, from the clipboard.
pub fn generate_slides(
    workspace: &mut Workspace,
    action: &GenerateSlides,
    _window: &mut gpui::Window,
    cx: &mut gpui::Context<Workspace>,
) {
    struct SlidesNotification;
    let notification_id = workspace::notifications::NotificationId::unique::<SlidesNotification>();
    let topic = action
        .topic
        .clone()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| current_topic(cx));
    if topic.is_empty() {
        workspace.show_toast(
            workspace::Toast::new(
                notification_id,
                "No topic: switch the composer to the Slides profile and type a request, \
                 or copy the text describing the deck and run Generate Slides again.",
            )
            .autohide(),
            cx,
        );
        return;
    }

    let out_dir = workspace
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| PathBuf::from(worktree.read(cx).abs_path().as_ref()))
        .unwrap_or_else(|| PathBuf::from("."));

    let client: Arc<dyn HttpClient> = workspace.app_state().client.http_client();
    let (progress_tx, progress_rx) = async_channel::unbounded::<String>();

    let pipeline_tx = progress_tx.clone();
    let pipeline = cx.background_spawn(async move {
        let progress_tx = pipeline_tx;
        progress_tx
            .send("Requesting deck from the model…".to_string())
            .await
            .ok();
        let deck = collect_deck(client.clone(), enhance_topic(topic)).await?;
        progress_tx
            .send(format!(
                "Deck ready: {} slides. Packaging PPTX…",
                deck.slides.len()
            ))
            .await
            .ok();

        std::fs::create_dir_all(&out_dir)
            .with_context(|| format!("create output dir {:?}", out_dir))?;

        let html_path = out_dir.join("slides-preview.html");
        std::fs::write(&html_path, render_preview_html(&deck))
            .with_context(|| format!("write {:?}", html_path))?;

        let pptx_bytes = pptx::build(&html_to_pptx_slides(&deck), "presentation");
        let pptx_path = out_dir.join("slides.pptx");
        std::fs::write(&pptx_path, &pptx_bytes)
            .with_context(|| format!("write {:?}", pptx_path))?;

        Ok((html_path, pptx_path, deck.slides.len()))
    });

    let pump_id = notification_id.clone();
    let pump = cx.spawn(async move |workspace, cx| {
        while let Ok(status) = progress_rx.recv().await {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.show_toast(
                        workspace::Toast::new(pump_id.clone(), status).autohide(),
                        cx,
                    );
                })
                .ok();
        }
    });

    cx.spawn(async move |workspace, cx| {
        let result = pipeline.await;
        drop(progress_tx);
        pump.await;
        workspace
            .update(cx, |workspace, cx| match result {
                Ok((html_path, pptx_path, count)) => {
                    let summary = format!(
                        "Generated {count} slides.\nPreview: {}\nPPTX: {}",
                        html_path.display(),
                        pptx_path.display()
                    );
                    workspace.show_toast(
                        workspace::Toast::new(notification_id, summary).autohide(),
                        cx,
                    );
                }
                Err(err) => {
                    workspace.show_toast(
                        workspace::Toast::new(notification_id, format!("Slides failed: {err:#}"))
                            .autohide(),
                        cx,
                    );
                }
            })
            .ok();
    })
    .detach();
}

fn current_topic(cx: &gpui::App) -> String {
    cx.read_from_clipboard()
        .and_then(|item| item.text())
        .map(|text| text.trim().to_string())
        .unwrap_or_default()
}
