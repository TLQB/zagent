use anyhow::{Context as _, Result, anyhow};
use futures::{AsyncBufReadExt, AsyncReadExt, StreamExt, io::BufReader, stream::BoxStream};
use http_client::{
    AsyncBody, CustomHeaders, HttpClient, Method, Request as HttpRequest,
    RequestBuilderExt, http,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const GLM_API_URL: &str = "https://zai-fly-gateway.tranlequybaotk12.workers.dev";

const DEFAULT_CONTEXT_LENGTH: u64 = 4096;

// ── Gateway worker authentication ──────────────────────────────────
// The Cloudflare worker gateway authenticates `/v1/*` requests with a
// HS256 JWT issued by `POST /auth/login`. The token currently expires
// after 7 days, so we cache it in-process and refresh lazily when a
// request fails with 401 (or when the cached entry is within the refresh
// skew window).
//
// Credential sources, first match wins:
//   1. Runtime credentials set from the provider settings UI (a pre-issued
//      token, or gateway account email + password) and mirrored to the OS
//      keychain by the provider.
//   2. ZAI_WORKER_EMAIL + ZAI_WORKER_PASSWORD env vars — auto-login.
//   3. ZAGENT_GLM_API_KEY env var (a pre-issued bearer) so headless
//      deployments that mint a long-lived token out-of-band keep working.
//
// ZAI_WORKER_URL (env) or the runtime base-URL override may point the
// client at a self-hosted gateway worker.

#[derive(Clone, Debug, serde::Deserialize)]
struct LoginResponse {
    token: String,
}

#[derive(Clone, Debug)]
struct CachedToken {
    base_url: String,
    token: String,
    /// Unix-epoch seconds at which the token was minted.
    minted_at: u64,
}

const TOKEN_TTL_SECS: u64 = 7 * 24 * 60 * 60;
const TOKEN_REFRESH_SKEW_SECS: u64 = 60 * 10;

static CACHED_TOKEN: parking_lot::Mutex<Option<CachedToken>> = parking_lot::Mutex::new(None);

// Credentials entered through the settings UI. They take precedence over the
// environment variables so an explicitly signed-in user is never ignored.
static RUNTIME_TOKEN: parking_lot::RwLock<Option<String>> = parking_lot::RwLock::new(None);
static RUNTIME_LOGIN: parking_lot::RwLock<Option<(String, String)>> =
    parking_lot::RwLock::new(None);
static RUNTIME_BASE_URL: parking_lot::RwLock<Option<String>> = parking_lot::RwLock::new(None);

/// Installs a pre-issued gateway bearer token from the settings UI. Pass
/// `None` to clear it.
pub fn set_runtime_token(token: Option<String>) {
    *RUNTIME_TOKEN.write() = token.filter(|token| !token.is_empty());
    *RUNTIME_LOGIN.write() = None;
    invalidate_gateway_token();
}

/// Installs gateway account credentials (email + password) from the settings
/// UI. Pass `None` for either side to clear them.
pub fn set_runtime_login(email: Option<String>, password: Option<String>) {
    let credentials = email
        .zip(password)
        .filter(|(email, password)| !email.is_empty() && !password.is_empty());
    *RUNTIME_LOGIN.write() = credentials;
    *RUNTIME_TOKEN.write() = None;
    invalidate_gateway_token();
}

/// Overrides the gateway worker base URL (e.g. a self-hosted deployment).
pub fn set_runtime_base_url(url: Option<String>) {
    let url = url.filter(|url| !url.is_empty()).map(|url| url.trim_end_matches('/').to_string());
    let mut current = RUNTIME_BASE_URL.write();
    if *current != url {
        *current = url;
        *RUNTIME_TOKEN.write() = None;
        *RUNTIME_LOGIN.write() = None;
        invalidate_gateway_token();
    }
}

/// The email of the account signed in through the settings UI, if any.
pub fn runtime_login_email() -> Option<String> {
    RUNTIME_LOGIN
        .read()
        .as_ref()
        .map(|(email, _)| email.clone())
}

/// Whether any settings-UI credentials are currently installed.
pub fn has_runtime_credentials() -> bool {
    RUNTIME_TOKEN.read().is_some() || RUNTIME_LOGIN.read().is_some()
}

/// Validates gateway account credentials by performing a login (which also
/// caches the resulting JWT). Used by the settings-UI sign-in flow.
pub async fn sign_in_with_password(
    client: &dyn HttpClient,
    base: &str,
    email: &str,
    password: &str,
) -> Result<String> {
    login_refresh(client, base, email, password).await
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Base URL of the gateway worker: the settings-UI override first, then the
/// `ZAI_WORKER_URL` env var, then the compiled-in `GLM_API_URL`.
pub fn worker_base_url() -> String {
    if let Some(url) = RUNTIME_BASE_URL.read().clone() {
        return url;
    }
    std::env::var("ZAI_WORKER_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| GLM_API_URL.to_string())
}

/// Returns a cached token if it is still valid (with a refresh skew), otherwise
/// None. Caller should follow up with [`login_refresh`] when this returns
/// None.
fn cached_token_if_valid(base: &str) -> Option<String> {
    let guard = CACHED_TOKEN.lock();
    let cached = guard.as_ref()?;
    if cached.base_url != base.trim_end_matches('/') {
        return None;
    }
    let age = now_secs().saturating_sub(cached.minted_at);
    if age + TOKEN_REFRESH_SKEW_SECS < TOKEN_TTL_SECS {
        Some(cached.token.clone())
    } else {
        None
    }
}

fn runtime_base_matches(base: &str) -> bool {
    RUNTIME_BASE_URL
        .read()
        .as_deref()
        .unwrap_or(GLM_API_URL)
        .trim_end_matches('/')
        == base.trim_end_matches('/')
}

/// Forces the next [`gateway_token`] call to re-login. Call this after
/// observing a 401 from the gateway.
pub fn invalidate_gateway_token() {
    *CACHED_TOKEN.lock() = None;
}

/// Logs in to the gateway worker and caches the returned JWT. Returns the
/// token on success.
async fn login_refresh(
    client: &dyn HttpClient,
    base: &str,
    email: &str,
    password: &str,
) -> Result<String> {
    let uri = format!("{base}/auth/login");
    let body = serde_json::json!({ "email": email, "password": password }).to_string();
    let request = HttpRequest::builder()
        .method(Method::POST)
        .uri(uri)
        .header("Content-Type", "application/json")
        .body(AsyncBody::from(body))?;
    let mut response = client.send(request).await?;
    let mut body = String::new();
    response.body_mut().read_to_string(&mut body).await?;
    anyhow::ensure!(
        response.status().is_success(),
        "gateway login failed: {} {}",
        response.status(),
        body,
    );
    let parsed: LoginResponse = serde_json::from_str(&body).context("parse login response")?;
    let token = parsed.token;
    *CACHED_TOKEN.lock() = Some(CachedToken {
        base_url: base.trim_end_matches('/').to_string(),
        token: token.clone(),
        minted_at: now_secs(),
    });
    Ok(token)
}

/// Resolves the bearer token to send to the gateway worker at `base`.
///
/// Resolution order:
/// 1. Runtime credentials from the settings UI: account email + password →
///    auto-login + cache, otherwise the pre-issued token.
/// 2. For a custom/local base URL, `ZAGENT_GLM_API_KEY`.
/// 3. `ZAI_WORKER_EMAIL` + `ZAI_WORKER_PASSWORD` env vars → auto-login + cache.
/// 4. `ZAGENT_GLM_API_KEY` fallback for the default base.
/// 5. Empty string (server rejects with 401, caller decides).
pub async fn gateway_token(client: &dyn HttpClient, base: &str) -> Result<String> {
    let env_token = std::env::var("ZAGENT_GLM_API_KEY").ok();
    if runtime_base_matches(base) {
        if let Some(token) = RUNTIME_TOKEN.read().clone() {
            return Ok(token);
        }
        if let Some((email, password)) = RUNTIME_LOGIN.read().clone() {
            if let Some(token) = cached_token_if_valid(base) {
                return Ok(token);
            }
            return login_refresh(client, base, &email, &password).await;
        }
    }
    if let Some(token) = cached_token_if_valid(base) {
        return Ok(token);
    }
    if let Some(token) = custom_gateway_bearer(base, env_token) {
        return Ok(token);
    }
    // Environment-variable fallbacks for headless deployments.
    let env_login = match (
        std::env::var("ZAI_WORKER_EMAIL"),
        std::env::var("ZAI_WORKER_PASSWORD"),
    ) {
        (Ok(email), Ok(password)) if !email.is_empty() && !password.is_empty() => {
            Some((email, password))
        }
        _ => None,
    };
    if let Some((email, password)) = env_login {
        if let Some(token) = cached_token_if_valid(base) {
            return Ok(token);
        }
        return login_refresh(client, base, &email, &password).await;
    }
    Ok(std::env::var("ZAGENT_GLM_API_KEY").unwrap_or_default())
}

fn custom_gateway_bearer(base: &str, env_token: Option<String>) -> Option<String> {
    let base = base.trim_end_matches('/');
    let default_base = GLM_API_URL.trim_end_matches('/');
    if base == default_base {
        return None;
    }
    env_token.filter(|token| !token.is_empty())
}

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
    /// Reasoning-effort tiers the server published for this model, in the
    /// server's own order (e.g. `["low","high","max"]`). Empty when the
    /// server did not report any, so the provider falls back to its table.
    #[serde(default)]
    pub effort_levels: Vec<String>,
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
            effort_levels: Vec::new(),
        }
    }

    /// Sets the server-reported reasoning-effort tiers (builder form).
    pub fn with_effort_levels(mut self, levels: Vec<String>) -> Self {
        self.effort_levels = levels;
        self
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
    /// Reasoning-effort tiers the model accepts, published by the zagent-proxy
    /// sidecar (e.g. `["low","high","max"]`). Empty when the server omits it,
    /// in which case the editor falls back to its built-in table.
    #[serde(default)]
    pub reasoning_effort_levels: Vec<String>,
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

    /// Whether the server advertises extended thinking for this model
    /// (`capabilities.think`). `None` when the server does not report it, so
    /// the caller can fall back to its built-in table.
    pub fn supports_thinking_hint(&self) -> Option<bool> {
        self.capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.think)
    }

    /// The reasoning-effort tiers the server reports, or an empty slice when
    /// it does not publish any.
    pub fn effort_levels_hint(&self) -> &[String] {
        &self.reasoning_effort_levels
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
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct ModelCapabilities {
    /// Whether the model accepts image input.
    #[serde(default)]
    pub vision: Option<bool>,
    /// Whether the model supports extended thinking (`capabilities.think`).
    /// The zagent-proxy sidecar reports this so the editor can show thinking
    /// controls without hardcoding a model list.
    #[serde(default)]
    pub think: Option<bool>,
    /// Whether the model accepts a `reasoning_effort` parameter.
    #[serde(default)]
    pub reasoning_effort: Option<bool>,
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
    request: ChatCompletionRequest,
    extra_headers: &CustomHeaders,
) -> Result<BoxStream<'static, Result<ResponseStreamEvent>>> {
    let uri = format!("{api_url}/v1/chat/completions");
    let body_bytes = serde_json::to_string(&request)?;

    let mut transient_retries = 0u8;
    for attempt in 0..4u8 {
        let token = gateway_token(client, api_url).await.unwrap_or_default();
        let request_builder = http::Request::builder()
            .method(Method::POST)
            .uri(&uri)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {token}"));
        let req = request_builder
            .extra_headers(extra_headers)
            .body(AsyncBody::from(body_bytes.clone()))?;
        let mut response = client.send(req).await?;

        if response.status().as_u16() == 401 && attempt == 0 {
            invalidate_gateway_token();
            continue;
        }

        // Transient gateway/upstream failures (503 capacity flare, 429 rate
        // limit, 502/504): the gateway normalizes upstream flares to
        // 503 + Retry-After exactly so clients can retry — honor it instead
        // of surfacing an error toast immediately (old behavior: instant
        // "Failed to connect to GLM API" on any 503).
        let status = response.status().as_u16();
        if matches!(status, 503 | 429 | 502 | 504) && transient_retries < 2 {
            transient_retries += 1;
            let retry_after = response
                .headers()
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(15)
                .min(30);
            let mut body = String::new();
            response.body_mut().read_to_string(&mut body).await?;
            log::warn!(
                "GLM API transient {} (retry {}/2) — retrying in {}s: {}",
                status,
                transient_retries,
                retry_after,
                body.chars().take(140).collect::<String>()
            );
            smol::Timer::after(std::time::Duration::from_secs(retry_after)).await;
            continue;
        }

        if response.status().is_success() {
            let reader = BufReader::new(response.into_body());
            return Ok(reader
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
                .boxed());
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
    anyhow::bail!("gateway auth failed after retry");
}

/// Lists the models the server is serving via `GET /v1/models`.
pub async fn get_models(
    client: &dyn HttpClient,
    api_url: &str,
    extra_headers: &CustomHeaders,
) -> Result<Vec<ModelEntry>> {
    let uri = format!("{api_url}/v1/models");
    for attempt in 0..2u8 {
        let token = gateway_token(client, api_url).await.unwrap_or_default();
        let request = HttpRequest::builder()
            .method(Method::GET)
            .uri(&uri)
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {token}"))
            .extra_headers(extra_headers)
            .body(AsyncBody::default())?;
        let mut response = client.send(request).await?;
        if response.status().as_u16() == 401 && attempt == 0 {
            invalidate_gateway_token();
            continue;
        }
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
        return Ok(response.data);
    }
    anyhow::bail!("gateway auth failed after retry");
}

#[cfg(test)]
mod tests {
    use super::{
        CACHED_TOKEN, CachedToken, GLM_API_URL, cached_token_if_valid, custom_gateway_bearer,
        has_runtime_credentials, now_secs, runtime_base_matches, set_runtime_base_url,
        set_runtime_token,
    };

    #[test]
    fn switching_endpoint_does_not_reuse_credentials_or_login_token() {
        let cloud = "https://gateway.example";
        let local = "http://127.0.0.1:3001";
        set_runtime_base_url(Some(cloud.to_string()));
        set_runtime_token(Some("cloud-secret".to_string()));
        assert!(runtime_base_matches(cloud));
        assert!(!runtime_base_matches(local));
        set_runtime_base_url(Some(local.to_string()));
        assert!(!has_runtime_credentials());
        *CACHED_TOKEN.lock() = Some(CachedToken {
            base_url: cloud.to_string(),
            token: "cloud-jwt".to_string(),
            minted_at: now_secs(),
        });
        assert!(cached_token_if_valid(local).is_none());
        assert_eq!(cached_token_if_valid(cloud).as_deref(), Some("cloud-jwt"));
        set_runtime_base_url(None);
    }

    #[test]
    fn custom_gateway_prefers_explicit_bearer() {
        assert_eq!(
            custom_gateway_bearer("http://127.0.0.1:3001", Some("local-token".to_string())),
            Some("local-token".to_string())
        );
        assert_eq!(
            custom_gateway_bearer(GLM_API_URL, Some("local-token".to_string())),
            None
        );
        assert_eq!(
            custom_gateway_bearer(&format!("{GLM_API_URL}/"), Some("local-token".to_string())),
            None
        );
        assert_eq!(
            custom_gateway_bearer("http://127.0.0.1:3001", Some(String::new())),
            None
        );
    }

    /// The zagent-proxy `/v1/models` payload carries capability flags and the
    /// reasoning-effort tiers; the decoder must surface them (they were being
    /// silently dropped, which stripped the thinking/effort controls).
    #[test]
    fn parses_sidecar_capabilities_and_effort_levels() {
        let body = r#"{
            "object": "list",
            "data": [{
                "id": "x-preview-l",
                "display_name": "GLM-5.3-Flash",
                "architecture": {"input_modalities": ["text","image"]},
                "capabilities": {"think": true, "reasoning_effort": true, "vision": true},
                "reasoning_effort_levels": ["low","high","max"]
            }, {
                "id": "glm-5.2",
                "capabilities": {"think": true, "vision": false}
            }]
        }"#;
        let parsed: super::ListModelsResponse = serde_json::from_str(body).expect("decode");
        assert_eq!(parsed.data.len(), 2);

        let flash = &parsed.data[0];
        assert_eq!(flash.supports_thinking_hint(), Some(true));
        assert!(flash.supports_images_hint());
        assert_eq!(flash.effort_levels_hint(), ["low", "high", "max"]);

        let glm52 = &parsed.data[1];
        assert_eq!(glm52.supports_thinking_hint(), Some(true));
        assert!(!glm52.supports_images_hint());
        assert!(glm52.effort_levels_hint().is_empty());
    }

    /// When the server omits capabilities entirely, the hints are `None` so the
    /// provider can fall back to its table instead of assuming "no thinking".
    #[test]
    fn absent_capabilities_yield_none_hints() {
        let body = r#"{"data":[{"id":"glm-5.3"}]}"#;
        let parsed: super::ListModelsResponse = serde_json::from_str(body).expect("decode");
        let entry = &parsed.data[0];
        assert_eq!(entry.supports_thinking_hint(), None);
        assert!(entry.effort_levels_hint().is_empty());
    }

    /// Decodes a LIVE capture from the zagent-proxy `/v1/models` endpoint
    /// (checked in as a fixture) to prove the decoder stays in step with the
    /// real payload the editor receives.
    #[test]
    fn decodes_live_proxy_models_capture() {
        let body = include_str!("../tests/fixtures/proxy_models.json");
        let parsed: super::ListModelsResponse =
            serde_json::from_str(body).expect("decode live capture");
        assert!(!parsed.data.is_empty());

        let flash = parsed
            .data
            .iter()
            .find(|m| m.id == "x-preview-l")
            .expect("x-preview-l present in the live capture");
        assert_eq!(flash.supports_thinking_hint(), Some(true));
        assert!(flash.supports_images_hint());
        assert_eq!(flash.effort_levels_hint(), ["low", "high", "max"]);
    }
}
