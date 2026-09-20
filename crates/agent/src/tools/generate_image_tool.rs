use std::sync::Arc;

use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use futures::{AsyncReadExt, FutureExt as _};
use gpui::{App, Task};
use http_client::{AsyncBody, HttpClient, HttpClientWithUrl, http};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ui::prelude::*;
use util::markdown::MarkdownInlineCode;

/// Base URL of the embedded sidecar (zai-proxy on 127.0.0.1:3001).
const IMAGES_BASE_URL: &str = "http://127.0.0.1:3001";
/// Built-in sidecar auth password (mirrors the glm provider wiring).
const PROXY_PASSWORD: &str = "Waguri";

/// Generate an image from a text description using the Z.AI image service
/// through the embedded local proxy.
/// Use this when the user asks to create, draw, or generate a picture,
/// illustration, logo, or artwork.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GenerateImageToolInput {
    /// Detailed description of the image to generate.
    prompt: String,
    /// Optional image size: "1024x1024" (square), "1792x1024" (landscape),
    /// or "1024x1792" (portrait). Defaults to a square image.
    size: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GeneratedImage {
    /// URL of the generated image (signed; expires after about a week).
    url: String,
    ratio: String,
    resolution: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GenerateImageToolOutput {
    Success(GeneratedImage),
    Error { error: String },
}

impl From<GenerateImageToolOutput> for LanguageModelToolResultContent {
    fn from(value: GenerateImageToolOutput) -> Self {
        match value {
            GenerateImageToolOutput::Success(image) => format!(
                "Generated image ({} {}): {}",
                image.ratio, image.resolution, image.url
            )
            .into(),
            GenerateImageToolOutput::Error { error } => error.into(),
        }
    }
}

pub struct GenerateImageTool {
    http_client: Arc<HttpClientWithUrl>,
}

impl GenerateImageTool {
    pub fn new(http_client: Arc<HttpClientWithUrl>) -> Self {
        Self { http_client }
    }
}

impl AgentTool for GenerateImageTool {
    type Input = GenerateImageToolInput;
    type Output = GenerateImageToolOutput;

    const NAME: &'static str = "generate_image";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Fetch
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Generating image".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let http_client = self.http_client.clone();
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|e| GenerateImageToolOutput::Error {
                    error: e.to_string(),
                })?;

            let authorize = cx.update(|cx| {
                let context =
                    crate::ToolPermissionContext::new(Self::NAME, vec![input.prompt.clone()]);
                event_stream.authorize(
                    format!(
                        "Generate an image for {}",
                        MarkdownInlineCode(&input.prompt)
                    ),
                    context,
                    cx,
                )
            });
            authorize
                .await
                .map_err(|e| GenerateImageToolOutput::Error {
                    error: e.to_string(),
                })?;

            event_stream.update_fields(acp::ToolCallUpdateFields::new().title("Generating image…"));

            let body = serde_json::json!({
                "prompt": input.prompt,
                "size": input.size.clone().unwrap_or_else(|| "1024x1024".to_string()),
                "n": 1,
            })
            .to_string();

            let request = http::Request::builder()
                .method(http::Method::POST)
                .uri(format!("{IMAGES_BASE_URL}/v1/images/generations"))
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {PROXY_PASSWORD}"))
                .body(AsyncBody::from(body))
                .map_err(|e| GenerateImageToolOutput::Error {
                    error: e.to_string(),
                })?;

            let send_fut = http_client.send(request);
            let mut response = futures::select! {
                result = send_fut.fuse() => result.map_err(|e| {
                    GenerateImageToolOutput::Error { error: e.to_string() }
                })?,
                _ = event_stream.cancelled_by_user().fuse() => {
                    return Err(GenerateImageToolOutput::Error {
                        error: "Image generation cancelled by user".to_string(),
                    });
                }
            };

            let mut text = String::new();
            response
                .body_mut()
                .read_to_string(&mut text)
                .await
                .map_err(|e| GenerateImageToolOutput::Error {
                    error: e.to_string(),
                })?;

            if !response.status().is_success() {
                return Err(GenerateImageToolOutput::Error {
                    error: format!("image generation failed: {} {}", response.status(), text),
                });
            }

            let parsed: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| GenerateImageToolOutput::Error {
                    error: format!("bad image response: {e}"),
                })?;
            let url = parsed["data"][0]["url"]
                .as_str()
                .ok_or_else(|| GenerateImageToolOutput::Error {
                    error: format!("no image url in response: {text}"),
                })?
                .to_string();
            let ratio = parsed["data"][0]["ratio"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let resolution = parsed["data"][0]["resolution"]
                .as_str()
                .unwrap_or_default()
                .to_string();

            event_stream.update_fields(acp::ToolCallUpdateFields::new().title("Image generated"));

            Ok(GenerateImageToolOutput::Success(GeneratedImage {
                url,
                ratio,
                resolution,
            }))
        })
    }
}
