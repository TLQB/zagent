use anyhow::{Context as _, Result, anyhow};
use futures::{AsyncBufReadExt, AsyncReadExt, StreamExt, io::BufReader, stream::BoxStream};
use http_client::{
    AsyncBody, CustomHeaders, HttpClient, HttpRequestExt, Method, Request as HttpRequest,
    RequestBuilderExt, http,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const GLM_API_URL: &str = "https://zai-proxy-worker.tranlequybaotk12.workers.dev";

const DEFAULT_CONTEXT_LENGTH: u64 = 4096;

/// A model exposed to the rest of Zed, after merging API discovery with
/// user-configured overrides.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Model {
    pub name: String,
    pub display_name: Option<String>,
    pub max_tokens: u64,
    pub supports_tools: bool,
    pub supports_images: bool,
    pub supports_thinking: bool,
}

impl Model {
    pub fn new(
        name: &str,
        display_name: Option<&str>,
        max_tokens: Option<u64>,
        supports_tools: bool,
        supports_images: bool,
        supports_thinking: bool,
    ) -> Self {
        Self {
            name: name.to_owned(),
            display_name: display_name.map(ToString::to_string),
            max_tokens: max_tokens.unwrap_or(DEFAULT_CONTEXT_LENGTH),
            supports_tools,
            supports_images,
            supports_thinking,
        }
    }

    pub fn display_name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoice {
    Auto,
    Required,
    None,
}

#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolDefinition {
    Function { function: FunctionDefinition },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: Option<String>,
    pub parameters: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatMessage {
    Assistant {
        #[serde(default)]
        content: Option<MessageContent>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    User {
        content: MessageContent,
    },
    System {
        content: MessageContent,
    },
    Tool {
        content: MessageContent,
        tool_call_id: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
#[serde(untagged)]
pub enum MessageContent {
    Plain(String),
    Multipart(Vec<MessagePart>),
}

impl MessageContent {
    pub fn push_part(&mut self, part: MessagePart) {
        match self {
            MessageContent::Plain(text) => {
                *self =
                    MessageContent::Multipart(vec![MessagePart::Text { text: text.clone() }, part]);
            }
            MessageContent::Multipart(parts) if parts.is_empty() => match part {
                MessagePart::Text { text } => *self = MessageContent::Plain(text),
                MessagePart::Image { .. } => *self = MessageContent::Multipart(vec![part]),
            },
            MessageContent::Multipart(parts) => parts.push(part),
        }
    }
}

impl From<Vec<MessagePart>> for MessageContent {
    fn from(mut parts: Vec<MessagePart>) -> Self {
        if let [MessagePart::Text { text }] = parts.as_mut_slice() {
            MessageContent::Plain(std::mem::take(text))
        } else {
            MessageContent::Multipart(parts)
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessagePart {
    Text {
        text: String,
    },
    #[serde(rename = "image_url")]
    Image {
        image_url: ImageUrl,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
pub struct ImageUrl {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(flatten)]
    pub content: ToolCallContent,
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolCallContent {
    Function { function: FunctionContent },
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
pub struct FunctionContent {
    pub name: String,
    pub arguments: String,
}

#[derive(Serialize, Debug)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// Asks the server to include a final `usage` chunk in the stream.
#[derive(Serialize, Debug)]
pub struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct GLMError {
    pub message: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
pub enum ResponseStreamResult {
    Ok(ResponseStreamEvent),
    Err { error: GLMError },
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ResponseStreamEvent {
    pub model: String,
    pub object: String,
    pub choices: Vec<ChoiceDelta>,
    pub usage: Option<Usage>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ChoiceDelta {
    pub index: u32,
    pub delta: ResponseMessageDelta,
    pub finish_reason: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
pub struct ResponseMessageDelta {
    pub content: Option<String>,
    /// `glm-server` emits reasoning as a dedicated `reasoning_content` field
    /// when started with a reasoning format (e.g. `--reasoning-format deepseek`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallChunk>>,
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
pub struct ToolCallChunk {
    pub index: usize,
    pub id: Option<String>,
    pub function: Option<FunctionChunk>,
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
pub struct FunctionChunk {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

/// Response of `GET /v1/models`.
///
/// In single-model mode `data` has exactly one entry describing the loaded
/// model; in router mode it lists every model the server knows about.
#[derive(Deserialize, Debug)]
pub struct ListModelsResponse {
    #[serde(default)]
    pub data: Vec<ModelEntry>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ModelEntry {
    pub id: String,
    /// Present in single-model mode; carries context information.
    #[serde(default)]
    pub meta: Option<ModelMeta>,
    /// Present in router mode; reports the modalities the model accepts.
    #[serde(default)]
    pub architecture: Option<Architecture>,
    /// Present in router mode; reports whether the model is currently loaded.
    #[serde(default)]
    pub status: Option<ModelStatus>,
    /// Sidecar `/models` entries advertise capability flags here (e.g.
    /// `vision`) instead of an `architecture` section.
    #[serde(default)]
    pub capabilities: Option<ModelCapabilities>,
}

impl ModelEntry {
    /// Whether this entry came from a server running in router mode.
    pub fn is_router_entry(&self) -> bool {
        self.status.is_some()
    }

    /// Whether the model is loaded and can be probed for capabilities without
    /// triggering a (potentially expensive) load.
    pub fn is_loaded(&self) -> bool {
        self.status
            .as_ref()
            .is_some_and(|status| status.value == "loaded")
    }

    /// Whether the model is currently loading, so a progress label applies.
    pub fn is_loading(&self) -> bool {
        self.status
            .as_ref()
            .is_some_and(|status| status.value == "loading")
    }

    /// Image support is advertised either via an lmstudio-style
    /// `architecture.input_modalities` section or, on the zai-proxy sidecar,
    /// via `capabilities: {"vision": true}`.
    pub fn supports_images_hint(&self) -> bool {
        let architecture_images = self
            .architecture
            .as_ref()
            .is_some_and(|architecture| architecture.input_modalities.iter().any(|m| m == "image"));
        architecture_images
            || self
                .capabilities
                .as_ref()
                .is_some_and(|capabilities| capabilities.vision == Some(true))
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct ModelMeta {
    /// The runtime per-slot context size.
    #[serde(default)]
    pub n_ctx: Option<u64>,
    /// The context size the model was trained with.
    #[serde(default)]
    pub n_ctx_train: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Architecture {
    #[serde(default)]
    pub input_modalities: Vec<String>,
}

/// Capability flags from the sidecar `/models` payload; unknown keys are
/// ignored by serde.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ModelCapabilities {
    /// Whether the model accepts image input.
    #[serde(default)]
    pub vision: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ModelStatus {
    /// One of `loaded`, `loading`, `unloaded`, `downloading`, `downloaded`, `sleeping`.
    pub value: String,
}

/// An event from the router's `/models/sse` feed, which the provider subscribes
/// to so model capabilities stay current as models load and unload. `model` is
/// `*` for events that aren't about a single model (e.g. the list reloading).
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ModelEvent {
    #[serde(default)]
    pub model: String,
    pub event: String,
    #[serde(default)]
    pub data: Option<ModelEventData>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct ModelEventData {
    #[serde(default)]
    pub status: Option<String>,
    /// Present on an `unloaded` status; non-zero means the model failed to load.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Present on a `loading` status, reporting per-stage load progress.
    #[serde(default)]
    pub progress: Option<LoadProgress>,
}

/// Per-stage load progress carried by a `loading` event. A model loads its
/// stages in order (the text model, plus an optional draft and/or multimodal
/// projector), each reporting a `0.0..=1.0` fraction.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct LoadProgress {
    #[serde(default)]
    pub stages: Vec<String>,
    #[serde(default)]
    pub current: String,
    #[serde(default)]
    pub value: f32,
}

impl LoadProgress {
    /// A human label for the stage currently loading, matching the GLM
    /// WebUI's labels. We report the current stage's own `value` rather than a
    /// blended overall percentage (as the WebUI does), so each stage runs
    /// `0→100%` and the label says which stage it is.
    pub fn stage_label(&self) -> &'static str {
        match self.current.as_str() {
            "text_model" => "Loading weights",
            "spec_model" => "Loading draft",
            "mmproj_model" => "Loading projector",
            _ => "Loading",
        }
    }

    /// The full load-status label shown in the model selector, e.g.
    /// `"Loading weights 42%"`: the current stage plus its own progress rounded
    /// to a percentage.
    pub fn progress_label(&self) -> String {
        format!(
            "{} {}%",
            self.stage_label(),
            (self.value * 100.0).round() as u32
        )
    }
}

impl ModelEvent {
    /// Whether this event means the set of models or their loaded state changed,
    /// so the provider should re-run discovery. Intermediate `loading` progress
    /// ticks return `false` — only terminal load/unload and list changes matter.
    pub fn changes_model_state(&self) -> bool {
        match self.event.as_str() {
            "models_reload" | "model_remove" => true,
            _ => matches!(
                self.data.as_ref().and_then(|data| data.status.as_deref()),
                Some("loaded" | "unloaded")
            ),
        }
    }

    /// The non-zero exit code of a failed load, if this event reports one.
    pub fn load_failure(&self) -> Option<i32> {
        let data = self.data.as_ref()?;
        if data.status.as_deref() == Some("unloaded") {
            data.exit_code.filter(|code| *code != 0)
        } else {
            None
        }
    }

    /// This event's load progress if it is a loading event carrying usable stage
    /// data, else `None`.
    pub fn load_progress(&self) -> Option<&LoadProgress> {
        let data = self.data.as_ref()?;
        if data.status.as_deref() != Some("loading") {
            return None;
        }
        let progress = data.progress.as_ref()?;
        // The server also emits bare stage-transition markers (e.g.
        // `{"stage": "mmproj_model"}`) with no `stages`/`current`/`value`. Skip
        // them so the indicator holds its last value rather than dropping to 0%.
        if progress.stages.is_empty() || progress.current.is_empty() {
            return None;
        }
        Some(progress)
    }
}

pub async fn stream_chat_completion(
    client: &dyn HttpClient,
    api_url: &str,
    api_key: Option<&str>,
    request: ChatCompletionRequest,
    extra_headers: &CustomHeaders,
) -> Result<BoxStream<'static, Result<ResponseStreamEvent>>> {
    let uri = format!("{api_url}/v1/chat/completions");
    let request_builder = http::Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("Content-Type", "application/json")
        .when_some(api_key, |builder, api_key| {
            builder.header("Authorization", format!("Bearer {api_key}"))
        });

    let request = request_builder
        .extra_headers(extra_headers)
        .body(AsyncBody::from(serde_json::to_string(&request)?))?;
    let mut response = client.send(request).await?;
    if response.status().is_success() {
        let reader = BufReader::new(response.into_body());
        Ok(reader
            .lines()
            .filter_map(|line| async move {
                match line {
                    Ok(line) => {
                        let line = line.strip_prefix("data: ")?;
                        if line == "[DONE]" {
                            None
                        } else {
                            match serde_json::from_str(line) {
                                Ok(ResponseStreamResult::Ok(response)) => Some(Ok(response)),
                                Ok(ResponseStreamResult::Err { error }) => {
                                    Some(Err(anyhow!(error.message)))
                                }
                                Err(error) => Some(Err(anyhow!(error))),
                            }
                        }
                    }
                    Err(error) => Some(Err(anyhow!(error))),
                }
            })
            .boxed())
    } else {
        let mut body = String::new();
        response.body_mut().read_to_string(&mut body).await?;
        anyhow::bail!(
            "Failed to connect to GLM API: {} {}",
            response.status(),
            body,
        );
    }
}

/// Lists the models the server is serving via `GET /v1/models`.
pub async fn get_models(
    client: &dyn HttpClient,
    api_url: &str,
    api_key: Option<&str>,
    extra_headers: &CustomHeaders,
) -> Result<Vec<ModelEntry>> {
    let uri = format!("{api_url}/v1/models");
    let request = HttpRequest::builder()
        .method(Method::GET)
        .uri(uri)
        .header("Accept", "application/json")
        .when_some(api_key, |builder, api_key| {
            builder.header("Authorization", format!("Bearer {api_key}"))
        })
        .extra_headers(extra_headers)
        .body(AsyncBody::default())?;

    let mut response = client.send(request).await?;

    let mut body = String::new();
    response.body_mut().read_to_string(&mut body).await?;

    anyhow::ensure!(
        response.status().is_success(),
        "Failed to connect to GLM API: {} {}",
        response.status(),
        body,
    );
    let response: ListModelsResponse =
        serde_json::from_str(&body).context("Unable to parse GLM models response")?;
    Ok(response.data)
}

/// Opens the router's `GET /models/sse` event stream. Each item is one parsed
/// event; the stream ends when the connection closes. Only available on builds
/// that expose `/models/sse` (router mode).
pub async fn stream_model_events(
    client: &dyn HttpClient,
    api_url: &str,
    api_key: Option<&str>,
    extra_headers: &CustomHeaders,
) -> Result<BoxStream<'static, Result<ModelEvent>>> {
    let uri = format!("{api_url}/models/sse");
    let request = HttpRequest::builder()
        .method(Method::GET)
        .uri(uri)
        .header("Accept", "text/event-stream")
        .when_some(api_key, |builder, api_key| {
            builder.header("Authorization", format!("Bearer {api_key}"))
        })
        .extra_headers(extra_headers)
        .body(AsyncBody::default())?;

    let mut response = client.send(request).await?;
    if !response.status().is_success() {
        let mut body = String::new();
        response.body_mut().read_to_string(&mut body).await?;
        anyhow::bail!(
            "Failed to open GLM model event stream: {} {}",
            response.status(),
            body,
        );
    }

    let reader = BufReader::new(response.into_body());
    Ok(reader
        .lines()
        .filter_map(|line| async move {
            // Each event is a single `data:` line carrying the JSON envelope;
            // other SSE lines (comments, blank separators) are ignored.
            let line = line.ok()?;
            let payload = line.strip_prefix("data:")?.trim_start();
            Some(serde_json::from_str::<ModelEvent>(payload).map_err(|error| anyhow!(error)))
        })
        .boxed())
}
