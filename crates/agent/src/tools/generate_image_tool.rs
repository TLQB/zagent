use std::sync::Arc;

use crate::tools::slides_tool::first_worktree_dir;
use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use futures::{AsyncReadExt, FutureExt as _};
use gpui::{App, Entity, ImageFormat, Task};
use http_client::{AsyncBody, HttpClient, HttpClientWithUrl, http};
use language_model::{LanguageModelImage, LanguageModelImageExt, LanguageModelToolResultContent};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ui::prelude::*;
use util::markdown::MarkdownInlineCode;

/// Base URL of the gateway worker.
const IMAGES_BASE_URL: &str = "https://zai-proxy-worker.tranlequybaotk12.workers.dev";
/// Gateway bearer token (resolved by the glm crate: env vars
/// ZAI_WORKER_EMAIL/ZAI_WORKER_PASSWORD, or fallback ZAGENT_GLM_API_KEY).
async fn proxy_token(client: &dyn HttpClient) -> String {
    glm::gateway_token(client, glm::worker_base_url().as_str())
        .await
        .unwrap_or_default()
}

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
    /// Local copy saved into the project's `zagent-images/` directory.
    local_path: String,
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
                "Generated image ({} {}).
Saved to: {}
URL: {}",
                image.ratio, image.resolution, image.local_path, image.url
            )
            .into(),
            GenerateImageToolOutput::Error { error } => error.into(),
        }
    }
}

pub struct GenerateImageTool {
    http_client: Arc<HttpClientWithUrl>,
    project: Entity<Project>,
}

impl GenerateImageTool {
    pub fn new(http_client: Arc<HttpClientWithUrl>, project: Entity<Project>) -> Self {
        Self {
            http_client,
            project,
        }
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
        let project = self.project.clone();
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
                .header("Authorization", format!("Bearer {}", proxy_token(http_client.as_ref()).await))
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

            event_stream.update_fields(acp::ToolCallUpdateFields::new().title("Saving image…"));

            let out_dir = cx.update(|cx| first_worktree_dir(&project, cx));
            let images_dir = out_dir.join("zagent-images");
            std::fs::create_dir_all(&images_dir).map_err(|e| GenerateImageToolOutput::Error {
                error: e.to_string(),
            })?;
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| GenerateImageToolOutput::Error {
                    error: e.to_string(),
                })?
                .as_millis();
            let image_path = images_dir.join(format!("image-{stamp}.png"));

            let get_request = http::Request::builder()
                .method(http::Method::GET)
                .uri(url.as_str())
                .body(AsyncBody::default())
                .map_err(|e| GenerateImageToolOutput::Error {
                    error: e.to_string(),
                })?;
            let mut image_response = futures::select! {
                result = http_client.send(get_request).fuse() => result
                    .map_err(|e| GenerateImageToolOutput::Error { error: e.to_string() })?,
                _ = event_stream.cancelled_by_user().fuse() => {
                    return Err(GenerateImageToolOutput::Error {
                        error: "Image download cancelled by user".to_string(),
                    });
                }
            };
            let mut image_bytes = Vec::new();
            image_response
                .body_mut()
                .read_to_end(&mut image_bytes)
                .await
                .map_err(|e| GenerateImageToolOutput::Error {
                    error: e.to_string(),
                })?;
            if !image_response.status().is_success() {
                return Err(GenerateImageToolOutput::Error {
                    error: format!(
                        "downloading the generated image failed: {}",
                        image_response.status()
                    ),
                });
            }
            std::fs::write(&image_path, &image_bytes).map_err(|e| {
                GenerateImageToolOutput::Error {
                    error: e.to_string(),
                }
            })?;

            // Emit ảnh trực tiếp vào agent panel để render inline (giống
            // read_file_tool khi mở file ảnh). Không emit thì panel chỉ hiển
            // thị text "Saved to: ..." và người dùng tưởng tool fail.
            // Detect format từ magic bytes: image.z.ai có thể trả JPEG thay
            // vì PNG; nếu hardcode sai format, gpui::Image::from_bytes sẽ
            // giải mã fail silent và ảnh không hiển thị.
            let format = if image_bytes.starts_with(b"\x89PNG\r\n") {
                ImageFormat::Png
            } else if image_bytes.starts_with(b"\xff\xd8\xff") {
                ImageFormat::Jpeg
            } else {
                ImageFormat::Png
            };
            let gpui_image = Arc::new(gpui::Image::from_bytes(
                format,
                image_bytes.clone(),
            ));
            let language_model_image = cx
                .update(|cx| LanguageModelImage::from_image(gpui_image, cx))
                .await;
            let mime = match format {
                ImageFormat::Jpeg => "image/jpeg",
                _ => "image/png",
            };
            if let Some(lm_image) = language_model_image {
                event_stream.update_fields(
                    acp::ToolCallUpdateFields::new().content(vec![
                        acp::ToolCallContent::Content(acp::Content::new(
                            acp::ContentBlock::Image(acp::ImageContent::new(
                                lm_image.source.clone(),
                                mime,
                            )),
                        )),
                    ]),
                );
            }

            event_stream.update_fields(acp::ToolCallUpdateFields::new().title("Image generated"));

            Ok(GenerateImageToolOutput::Success(GeneratedImage {
                url,
                local_path: image_path.display().to_string(),
                ratio,
                resolution,
            }))
        })
    }
}
