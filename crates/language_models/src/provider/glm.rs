use anyhow::Result;
use collections::{HashMap, HashSet};
use credentials_provider::CredentialsProvider;
use futures::Stream;
use futures::{FutureExt, StreamExt, future::BoxFuture, stream::BoxStream};
use gpui::{App, AsyncApp, Context, Entity, Task, SharedString};
use http_client::{CustomHeaders, HttpClient};
use language_model::util::parse_tool_arguments;
use language_model::{
    ApiKeyState, AuthenticateError, EnvVar, IconOrSvg, LanguageModel, ApiKeyConfiguration,
    LanguageModelCompletionError, LanguageModelCompletionEvent, LanguageModelEffortLevel,
    LanguageModelId, LanguageModelName, LanguageModelProvider, LanguageModelProviderId,
    LanguageModelProviderName, LanguageModelProviderState, LanguageModelRequest,
    LanguageModelToolChoice, LanguageModelToolResultContent, LanguageModelToolUse, MessageContent,
    ProviderSettingsView, RateLimiter, Role, StopReason, TokenUsage,
    env_var,
};
use glm::{
    GLM_API_URL, ModelEntry, get_models, stream_chat_completion,
    stream_model_events,
};
pub use settings::LlamaCppAvailableModel as AvailableModel;
use settings::{Settings, SettingsStore};
use std::pin::Pin;
use std::sync::LazyLock;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;
use ui::prelude::*;
use util::ResultExt;

use crate::AllLanguageModelSettings;

const GLM_DOWNLOAD_URL: &str = "https://github.com/izaart95-jpg/GLM-Free-API";
const GLM_MODELS_URL: &str = "https://openrouter.ai/z-ai";

const PROVIDER_ID: LanguageModelProviderId = LanguageModelProviderId::new("zagent.glm");
const PROVIDER_NAME: LanguageModelProviderName = LanguageModelProviderName::new("Zagent-GLM");

const API_KEY_ENV_VAR_NAME: &str = "ZAGENT_GLM_API_KEY";
static API_KEY_ENV_VAR: LazyLock<EnvVar> = env_var!(API_KEY_ENV_VAR_NAME);

const MODEL_EVENT_RECONNECT_INTERVAL: Duration = Duration::from_secs(5);
const ASSUMED_UNLOADED_CONTEXT: u64 = 131_072;

// ====================================================================
// Reasoning-effort configuration
// --------------------------------------------------------------------
// To add/remove a model's reasoning-effort support, edit this list.
// Matching is case-insensitive substring match on the model name.
//
// Tuple layout per entry:
//   (model_name_match, &[(effort_value, display_label), ...], default_effort_value)
// ====================================================================
const REASONING_EFFORT_MODELS: &[(&str, &[(&str, &str)], &str)] = &[
    ("glm-5.3-flash", &[("low", "Low"), ("high", "High")], "high"),
    ("glm-5.3", &[("low", "Low"), ("high", "High"), ("max", "Max")], "high"),
    ("glm-5.2", &[("high", "High"), ("max", "Max")], "high"),
];

/// Built-in GLM models always shown in the provider's model list, even when
/// auto-discovery doesn't report them. Auto-discovered entries and user-configured
/// available_models override these defaults.
const DEFAULT_MODELS: &[(&str, &str, u64)] = &[
    ("glm-5.3-flash", "GLM-5.3 Flash", 131_072),
    ("glm-5.3", "GLM-5.3", 131_072),
    ("glm-5.2", "GLM-5.2", 131_072),
];

fn reasoning_effort_config(model_name: &str) -> Option<&'static (&'static str, &'static [(&'static str, &'static str)], &'static str)> {
    REASONING_EFFORT_MODELS
        .iter()
        .find(|(match_str, _, _)| {
            model_name
                .to_lowercase()
                .contains(&match_str.to_lowercase())
        })
}

fn model_supports_reasoning_effort(model_name: &str) -> bool {
    reasoning_effort_config(model_name).is_some()
}

fn supported_effort_levels_for(model_name: &str) -> Vec<LanguageModelEffortLevel> {
    let Some((_, levels, default)) = reasoning_effort_config(model_name) else {
        return Vec::new();
    };
    levels
        .iter()
        .map(|(value, label)| LanguageModelEffortLevel {
            name: (*label).into(),
            value: (*value).into(),
            is_default: *value == *default,
        })
        .collect()
}

fn resolve_reasoning_effort(
    request: &LanguageModelRequest,
    model_name: &str,
) -> Option<String> {
    let (_, levels, default) = reasoning_effort_config(model_name)?;

    if !request.thinking_allowed {
        return None;
    }

    let chosen = request
        .thinking_effort
        .as_deref()
        .filter(|effort| levels.iter().any(|(v, _)| *v == *effort))
        .unwrap_or(default);

    Some(chosen.to_string())
}

#[derive(Default, Debug, Clone, PartialEq)]
pub struct GLMSettings {
    pub api_url: String,
    pub auto_discover: bool,
    pub available_models: Vec<AvailableModel>,
    pub context_window: Option<u64>,
    pub custom_headers: CustomHeaders,
}

pub struct GLMLanguageModelProvider {
    http_client: Arc<dyn HttpClient>,
    state: Entity<State>,
    capability_cells: CapabilityCells,
    loading_progress: LoadingProgress,
}

pub struct State {
    api_key_state: ApiKeyState,
    credentials_provider: Arc<dyn CredentialsProvider>,
    http_client: Arc<dyn HttpClient>,
    fetched_models: Vec<glm::Model>,
    fetch_model_task: Option<Task<Result<()>>>,
    model_event_task: Option<Task<()>>,
    capability_cells: CapabilityCells,
    loading_progress: LoadingProgress,
}

impl State {
    fn is_authenticated(&self) -> bool {
        !self.fetched_models.is_empty()
    }

    fn set_api_key(&mut self, api_key: Option<String>, cx: &mut Context<Self>) -> Task<Result<()>> {
        // Hand the personal token to the embedded proxy sidecar (writes the
        // user token file + restarts the proxy with it). No-op when the key
        // is unchanged / proxy already healthy with it.
        if let Some(key) = api_key.as_deref() {
        }
        let credentials_provider = self.credentials_provider.clone();
        let api_url = GLMLanguageModelProvider::api_url(cx);
        let task = self.api_key_state.store(
            api_url,
            api_key,
            |this| &mut this.api_key_state,
            credentials_provider,
            cx,
        );

        self.fetched_models.clear();
        self.model_event_task = None;
        write_recover(&self.loading_progress).clear();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| this.restart_fetch_models_task(cx))
                .ok();
            result
        })
    }

    fn authenticate(&mut self, cx: &mut Context<Self>) -> Task<Result<(), AuthenticateError>> {
        let credentials_provider = self.credentials_provider.clone();
        let api_url = GLMLanguageModelProvider::api_url(cx);
        let load_key_task = self.api_key_state.load_if_needed(
            api_url,
            |this| &mut this.api_key_state,
            credentials_provider,
            cx,
        );

        if self.is_authenticated() {
            return Task::ready(Ok(()));
        }

        cx.spawn(async move |this, cx| {
            match load_key_task.await {
                Ok(()) | Err(AuthenticateError::CredentialsNotFound) => {}
                Err(error) => {
                    log::warn!("failed to load GLM API key: {error}");
                }
            }
            let fetch_models_task = this.update(cx, |this, cx| this.fetch_models(cx))?;
            match fetch_models_task.await {
                Ok(()) => Ok(()),
                Err(err) => {
                    let connection_refused = err.chain().any(|cause| {
                        cause
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|io_err| {
                                io_err.kind() == std::io::ErrorKind::ConnectionRefused
                            })
                    });
                    if connection_refused {
                        Err(AuthenticateError::ConnectionRefused)
                    } else {
                        Err(AuthenticateError::Other(err))
                    }
                }
            }
        })
    }

    fn fetch_models(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let http_client = Arc::clone(&self.http_client);
        let settings = GLMLanguageModelProvider::settings(cx);
        let api_url = GLMLanguageModelProvider::api_url(cx);
        let extra_headers = settings.custom_headers.clone();

        cx.spawn(async move |this, cx| {
            let entries = match get_models(
                http_client.as_ref(),
                &api_url,
                &extra_headers,
            )
            .await
            {
                Ok(entries) => entries,
                Err(err) => {
                    this.update(cx, |this, cx| {
                        this.fetched_models.clear();
                        cx.notify();
                    })
                   .ok();
                   return Err(err);
                }
            };

            let is_router = entries.iter().any(ModelEntry::is_router_entry);

            let loading_ids: HashSet<String> = entries
                .iter()
                .filter(|entry| entry.is_loading())
                .map(|entry| entry.id.clone())
                .collect();

let models: Vec<glm::Model> = entries
    .into_iter()
    .map(|entry| model_from_entry(&entry, None))
    .collect();

            this.update(cx, |this, cx| {
                this.fetched_models = models;
                let effective = compute_effective_models(
                    &this.fetched_models,
                    GLMLanguageModelProvider::settings(cx),
                );
                sync_capability_cells(&this.capability_cells, &effective);
                write_recover(&this.loading_progress).retain(|id, _| loading_ids.contains(id));
                if is_router {
                    if this.model_event_task.is_none() {
                        this.start_model_event_stream(cx);
                    }
                } else {
                    this.model_event_task = None;
                }
                cx.notify();
            })
        })
    }

    fn start_model_event_stream(&mut self, cx: &mut Context<Self>) {
        let http_client = Arc::clone(&self.http_client);
        let api_url = GLMLanguageModelProvider::api_url(cx);
        let extra_headers = GLMLanguageModelProvider::settings(cx)
            .custom_headers
            .clone();

        self.model_event_task = Some(cx.spawn(async move |this, cx| {
            loop {
                match stream_model_events(
                    http_client.as_ref(),
                    &api_url,
                    &extra_headers,
                )
                .await
                {
                    Ok(mut events) => {
                        while let Some(event) = events.next().await {
                            let Some(event) = event.log_err() else {
                                continue;
                            };
                            if let Some(exit_code) = event.load_failure() {
                                log::error!(
                                    "GLM model {} failed to load (exit code {exit_code})",
                                    event.model
                                );
                            }
                            if let Some(progress) = event.load_progress() {
                                let label = SharedString::from(progress.progress_label());
                                if this
                                    .update(cx, |this, cx| {
                                        write_recover(&this.loading_progress)
                                            .insert(event.model.clone(), label);
                                        cx.notify();
                                    })
                                    .is_err()
                                {
                                    return;
                                }
                                continue;
                            }
                            if !event.changes_model_state() {
                                continue;
                            }
                            if this
                                .update(cx, |this, cx| {
                                    write_recover(&this.loading_progress).remove(&event.model);
                                    this.restart_fetch_models_task(cx);
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        log::warn!("GLM model event stream unavailable: {error:#}");
                    }
                }

                cx.background_executor()
                    .timer(MODEL_EVENT_RECONNECT_INTERVAL)
                    .await;
                if this.update(cx, |_, _| ()).is_err() {
                    return;
                }
            }
        }));
    }

    fn restart_fetch_models_task(&mut self, cx: &mut Context<Self>) {
        let task = self.fetch_models(cx);
        self.fetch_model_task.replace(task);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct LiveCapabilities {
    max_tokens: u64,
    supports_tools: bool,
    supports_thinking: bool,
}

impl LiveCapabilities {
    fn of(model: &glm::Model) -> Self {
        Self {
            max_tokens: model.max_tokens,
            supports_tools: model.supports_tools,
            supports_thinking: model.supports_thinking,
        }
    }
}

type CapabilityCells = Arc<RwLock<HashMap<String, LiveCapabilities>>>;
type LoadingProgress = Arc<RwLock<HashMap<String, SharedString>>>;

fn read_recover<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_recover<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn compute_effective_models(
    fetched_models: &[glm::Model],
    settings: &GLMSettings,
) -> HashMap<String, glm::Model> {
    let mut models: HashMap<String, glm::Model> = HashMap::default();
    if settings.auto_discover {
        for model in fetched_models {
            let mut model = model.clone();
            if let Some(context_window) = settings.context_window {
                model.max_tokens = context_window;
            }
            models.insert(model.name.clone(), model);
        }
    }
    // Always include built-in GLM models so they appear in the picker even
    // when auto-discovery hasn't reported them. Entries already in the map
    // (from auto-discover) and user-configured models (merged next) take
    // precedence over these defaults.
    for (name, display_name, max_tokens) in DEFAULT_MODELS {
        models.entry((*name).to_string()).or_insert(glm::Model::new(
            name,
            Some(display_name),
            Some(*max_tokens),
            true,
            false,
            true,
        ));
    }
    merge_settings_into_models(
        &mut models,
        &settings.available_models,
        settings.context_window,
    );
    models
}

fn sync_capability_cells(cells: &CapabilityCells, effective: &HashMap<String, glm::Model>) {
    let mut cells = write_recover(cells);
    for model in effective.values() {
        cells.insert(model.name.clone(), LiveCapabilities::of(model));
    }
}

fn model_from_entry(entry: &ModelEntry, _props: Option<&()>) -> glm::Model {
    let max_tokens = entry
        .meta
        .as_ref()
        .and_then(|meta| meta.n_ctx)
        .or_else(|| entry.meta.as_ref().and_then(|meta| meta.n_ctx_train))
        .unwrap_or(ASSUMED_UNLOADED_CONTEXT);
    let supports_tools = !entry.is_loaded();
    let supports_images = entry.supports_images_hint();
    let supports_thinking = false; // or whatever default makes sense

    glm::Model::new(
        &entry.id,
        Some(&display_name_for(&entry.id)),
        Some(max_tokens),
        supports_tools,
        supports_images,
        supports_thinking,
    )
}

fn display_name_for(id: &str) -> String {
    let base = id.rsplit(['/', '\\']).next().unwrap_or(id);
    base.strip_suffix(".gguf").unwrap_or(base).to_string()
}

fn telemetry_id_for(id: &str) -> String {
    format!("{PROVIDER_ID}/{}", display_name_for(id))
}

impl GLMLanguageModelProvider {
    pub fn new(
        http_client: Arc<dyn HttpClient>,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &mut App,
    ) -> Self {
        let capability_cells: CapabilityCells = Arc::new(RwLock::new(HashMap::default()));
        let loading_progress: LoadingProgress = Arc::new(RwLock::new(HashMap::default()));
        let this = Self {
            http_client: http_client.clone(),
            capability_cells: capability_cells.clone(),
            loading_progress: loading_progress.clone(),
            state: cx.new(|cx| {
                cx.observe_global::<SettingsStore>({
                    let mut last_settings = GLMLanguageModelProvider::settings(cx).clone();
                    move |this: &mut State, cx| {
                        let current_settings = GLMLanguageModelProvider::settings(cx);
                        let settings_changed = current_settings != &last_settings;
                        if settings_changed {
                            let url_changed = last_settings.api_url != current_settings.api_url;
                            last_settings = current_settings.clone();
                            if url_changed {
                                let credentials_provider = this.credentials_provider.clone();
                                let api_url = Self::api_url(cx);
                                this.api_key_state.handle_url_change(
                                    api_url,
                                    |this| &mut this.api_key_state,
                                    credentials_provider,
                                    cx,
                                );
                                this.model_event_task = None;
                                write_recover(&this.loading_progress).clear();
                                this.authenticate(cx).detach();
                            }
                            // Capability edits (supports_images/tools/thinking) are
                            // merged into the model list at build time, so the cached
                            // fetched models must be dropped on ANY settings change,
                            // not just a URL change — otherwise the old flags keep
                            // gating image attachments until restart.
                            this.fetched_models.clear();
                            this.fetch_model_task = None;
                            cx.notify();
                        }
                    }
                })
                .detach();

                State {
                    http_client,
                    fetched_models: Default::default(),
                    fetch_model_task: None,
                    model_event_task: None,
                    capability_cells,
                    loading_progress,
                    api_key_state: ApiKeyState::new(Self::api_url(cx), (*API_KEY_ENV_VAR).clone()),
                    credentials_provider,
                }
            }),
        };
        this.state
            .update(cx, |state, cx| state.restart_fetch_models_task(cx));
        this
    }

    fn settings(cx: &App) -> &GLMSettings {
        &AllLanguageModelSettings::get_global(cx).glm
    }

    fn api_url(cx: &App) -> SharedString {
        let api_url = &Self::settings(cx).api_url;
        if api_url.is_empty() {
            GLM_API_URL.into()
        } else {
            SharedString::new(api_url.as_str())
        }
    }

    fn has_custom_url(cx: &App) -> bool {
        let api_url = &Self::settings(cx).api_url;
        !api_url.is_empty() && api_url != GLM_API_URL
    }
}

impl LanguageModelProviderState for GLMLanguageModelProvider {
    type ObservableEntity = State;

    fn observable_entity(&self) -> Option<Entity<Self::ObservableEntity>> {
        Some(self.state.clone())
    }
}

impl LanguageModelProvider for GLMLanguageModelProvider {
    fn id(&self) -> LanguageModelProviderId {
        PROVIDER_ID
    }

    fn name(&self) -> LanguageModelProviderName {
        PROVIDER_NAME
    }

    fn icon(&self) -> IconOrSvg {
        IconOrSvg::Icon(IconName::Zagent)
    }

    fn default_model(&self, _: &App) -> Option<Arc<dyn LanguageModel>> {
        None
    }

    fn default_fast_model(&self, _: &App) -> Option<Arc<dyn LanguageModel>> {
        None
    }

    fn provided_models(&self, cx: &App) -> Vec<Arc<dyn LanguageModel>> {
        let settings = GLMLanguageModelProvider::settings(cx);
        let effective = compute_effective_models(&self.state.read(cx).fetched_models, settings);

        sync_capability_cells(&self.capability_cells, &effective);
        let mut models = effective
            .into_values()
            .map(|model| {
                Arc::new(GLMLanguageModel {
                    id: LanguageModelId::from(model.name.clone()),
                    name: model.name.clone(),
                    display_name: model.display_name().to_string(),
                    fallback_capabilities: LiveCapabilities::of(&model),
                    supports_images: model.supports_images,
                    capability_cells: self.capability_cells.clone(),
                    loading_progress: self.loading_progress.clone(),
                    http_client: self.http_client.clone(),
                    request_limiter: RateLimiter::new(4),
                    state: self.state.clone(),
                }) as Arc<dyn LanguageModel>
            })
            .collect::<Vec<_>>();
        models.sort_by_key(|model| model.name());
        models
    }

    fn is_authenticated(&self, cx: &App) -> bool {
        self.state.read(cx).is_authenticated()
    }

    fn authenticate(&self, cx: &mut App) -> Task<Result<(), AuthenticateError>> {
        self.state.update(cx, |state, cx| state.authenticate(cx))
    }

fn settings_view(&self, cx: &mut App) -> Option<ProviderSettingsView> {
    let state = self.state.read(cx);
    Some(ProviderSettingsView::ApiKey(ApiKeyConfiguration::new(
        state.api_key_state.has_key(),
        state.api_key_state.is_from_env_var(),
        state.api_key_state.env_var_name().clone(),
        GLM_API_URL.into(),
    )))
}
}

pub struct GLMLanguageModel {
    id: LanguageModelId,
    name: String,
    display_name: String,
    capability_cells: CapabilityCells,
    fallback_capabilities: LiveCapabilities,
    supports_images: bool,
    loading_progress: LoadingProgress,
    http_client: Arc<dyn HttpClient>,
    request_limiter: RateLimiter,
    state: Entity<State>,
}

impl GLMLanguageModel {
    fn capabilities(&self) -> LiveCapabilities {
        read_recover(&self.capability_cells)
            .get(&self.name)
            .copied()
            .unwrap_or(self.fallback_capabilities)
    }

    fn loading_label(&self) -> Option<SharedString> {
        read_recover(&self.loading_progress)
            .get(&self.name)
            .cloned()
    }

    fn to_glm_request(
        &self,
        request: LanguageModelRequest,
    ) -> Result<glm::ChatCompletionRequest> {
        build_glm_request(
            &self.name,
            self.supports_images,
            self.capabilities(),
            request,
        )
    }

    fn stream_completion(
        &self,
        request: glm::ChatCompletionRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<futures::stream::BoxStream<'static, Result<glm::ResponseStreamEvent>>>,
    > {
        let http_client = self.http_client.clone();
        let (api_url, extra_headers) = self.state.read_with(cx, |_state, cx| {
            let api_url = GLMLanguageModelProvider::api_url(cx);
            let extra_headers = GLMLanguageModelProvider::settings(cx)
                .custom_headers
                .clone();
            (api_url, extra_headers)
        });

        let future = self.request_limiter.stream(async move {
            let stream = stream_chat_completion(
                http_client.as_ref(),
                &api_url,
                request,
                &extra_headers,
            )
            .await?;
            Ok(stream)
        });

        async move { Ok(future.await?.boxed()) }.boxed()
    }
}

fn build_glm_request(
    model_name: &str,
    supports_images: bool,
    capabilities: LiveCapabilities,
    request: LanguageModelRequest,
) -> Result<glm::ChatCompletionRequest> {
    if request.contains_custom_tool_input() {
        anyhow::bail!("GLM does not support custom tools");
    }

    let supports_tools = capabilities.supports_tools;
    let supports_thinking =
        capabilities.supports_thinking || model_supports_reasoning_effort(model_name);    let mut messages = Vec::new();
    let reasoning_effort = resolve_reasoning_effort(&request, model_name);

    for message in request.messages {
        let mut reasoning_content: Option<String> = None;
        for content in message.content {
            match content {
                MessageContent::Text(text) => add_message_content_part(
                    glm::MessagePart::Text { text },
                    message.role,
                    &mut messages,
                    if supports_thinking && message.role == Role::Assistant {
                        reasoning_content.take()
                    } else {
                        None
                    },
                ),
                MessageContent::Thinking { text, .. } => {
                    if supports_thinking && message.role == Role::Assistant && !text.is_empty() {
                        reasoning_content.get_or_insert_default().push_str(&text);
                    }
                }
                MessageContent::RedactedThinking(_) => {}
                MessageContent::Compaction(_) => {}
                MessageContent::Image(image) => {
                    if supports_images {
                        add_message_content_part(
                            glm::MessagePart::Image {
                                image_url: glm::ImageUrl {
                                    url: image.to_base64_url(),
                                    detail: None,
                                },
                            },
                            message.role,
                            &mut messages,
                            if supports_thinking && message.role == Role::Assistant {
                                reasoning_content.take()
                            } else {
                                None
                            },
                        );
                    }
                }
                MessageContent::ToolUse(tool_use) => {
                    let input = tool_use.input.as_json().ok_or_else(|| {
                        anyhow::anyhow!("GLM does not support custom tool calls")
                    })?;
                    let tool_call = glm::ToolCall {
                        id: tool_use.id.to_string(),
                        content: glm::ToolCallContent::Function {
                            function: glm::FunctionContent {
                                name: tool_use.name.to_string(),
                                arguments: serde_json::to_string(input).unwrap_or_default(),
                            },
                        },
                    };

                    if let Some(glm::ChatMessage::Assistant {
                        tool_calls,
                        reasoning_content: message_reasoning_content,
                        ..
                    }) = messages.last_mut()
                    {
                        append_reasoning_content(
                            message_reasoning_content,
                            reasoning_content.take(),
                        );
                        tool_calls.push(tool_call);
                    } else {
                        messages.push(glm::ChatMessage::Assistant {
                            content: None,
                            reasoning_content: reasoning_content.take(),
                            tool_calls: vec![tool_call],
                        });
                    }
                }
                MessageContent::ToolResult(tool_result) => {
                    let content: Vec<glm::MessagePart> = tool_result
                        .content
                        .iter()
                        .filter_map(|part| match part {
                            LanguageModelToolResultContent::Text(text) => {
                                Some(glm::MessagePart::Text {
                                    text: text.to_string(),
                                })
                            }
                            LanguageModelToolResultContent::Image(image) => {
                                if supports_images {
                                    Some(glm::MessagePart::Image {
                                        image_url: glm::ImageUrl {
                                            url: image.to_base64_url(),
                                            detail: None,
                                        },
                                    })
                                } else {
                                    None
                                }
                            }
                        })
                        .collect();

                    messages.push(glm::ChatMessage::Tool {
                        content: content.into(),
                        tool_call_id: tool_result.tool_use_id.to_string(),
                    });
                }
            }
        }
    }

    let tools: Vec<glm::ToolDefinition> = if supports_tools {
        request
            .tools
            .into_iter()
            .map(|tool| {
                let input_schema = match tool.input {
                    language_model::LanguageModelRequestToolInput::Function {
                        input_schema,
                        ..
                    } => input_schema,
                    language_model::LanguageModelRequestToolInput::Custom { .. } => {
                        return Err(anyhow::anyhow!("GLM does not support custom tools"));
                    }
                };
                Ok(glm::ToolDefinition::Function {
                    function: glm::FunctionDefinition {
                        name: tool.name,
                        description: Some(tool.description),
                        parameters: Some(input_schema),
                    },
                })
            })
            .collect::<Result<_>>()?
    } else {
        Vec::new()
    };
    let tool_choice = if tools.is_empty() {
        None
    } else {
        request.tool_choice.map(|choice| match choice {
            LanguageModelToolChoice::Auto => glm::ToolChoice::Auto,
            LanguageModelToolChoice::Any => glm::ToolChoice::Required,
            LanguageModelToolChoice::None => glm::ToolChoice::None,
        })
    };

    Ok(glm::ChatCompletionRequest {
        model: model_name.to_string(),
        messages,
        stream: true,
        max_tokens: None,
        stop: if request.stop.is_empty() {
            None
        } else {
            Some(request.stop)
        },
        temperature: request.temperature,
        tools,
        tool_choice,
        stream_options: Some(glm::StreamOptions {
            include_usage: true,
        }),
        reasoning_effort,
    })
}

impl LanguageModel for GLMLanguageModel {
    fn id(&self) -> LanguageModelId {
        self.id.clone()
    }

    fn name(&self) -> LanguageModelName {
        match self.loading_label() {
            Some(label) => LanguageModelName::from(format!("{} · {}", self.display_name, label)),
            None => LanguageModelName::from(self.display_name.clone()),
        }
    }

    fn provider_id(&self) -> LanguageModelProviderId {
        PROVIDER_ID
    }

    fn provider_name(&self) -> LanguageModelProviderName {
        PROVIDER_NAME
    }

    fn supports_tools(&self) -> bool {
        self.capabilities().supports_tools
    }

    fn supports_tool_choice(&self, choice: LanguageModelToolChoice) -> bool {
        self.supports_tools()
            && match choice {
                LanguageModelToolChoice::Auto => true,
                LanguageModelToolChoice::Any => true,
                LanguageModelToolChoice::None => true,
            }
    }

    fn supports_images(&self) -> bool {
        self.supports_images
    }

    fn supports_thinking(&self) -> bool {
        self.capabilities().supports_thinking
            || model_supports_reasoning_effort(&self.name)
    }
    
    fn supported_effort_levels(&self) -> Vec<LanguageModelEffortLevel> {
        supported_effort_levels_for(&self.name)
    }

    fn telemetry_id(&self) -> String {
        telemetry_id_for(&self.name)
    }

    fn max_token_count(&self) -> u64 {
        self.capabilities().max_tokens
    }

    fn stream_completion(
        &self,
        request: LanguageModelRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<
            BoxStream<'static, Result<LanguageModelCompletionEvent, LanguageModelCompletionError>>,
            LanguageModelCompletionError,
        >,
    > {
        let request = match self.to_glm_request(request) {
            Ok(request) => request,
            Err(error) => return async move { Err(error.into()) }.boxed(),
        };
        let completions = self.stream_completion(request, cx);
        async move {
            let mapper = GLMEventMapper::new();
            Ok(mapper.map_stream(completions.await?).boxed())
        }
        .boxed()
    }
}

struct GLMEventMapper {
    tool_calls_by_index: HashMap<usize, RawToolCall>,
}

impl GLMEventMapper {
    fn new() -> Self {
        Self {
            tool_calls_by_index: HashMap::default(),
        }
    }

    pub fn map_stream(
        mut self,
        events: Pin<Box<dyn Send + Stream<Item = Result<glm::ResponseStreamEvent>>>>,
    ) -> impl Stream<Item = Result<LanguageModelCompletionEvent, LanguageModelCompletionError>>
    {
        events.flat_map(move |event| {
            futures::stream::iter(match event {
                Ok(event) => self.map_event(event),
                Err(error) => vec![Err(LanguageModelCompletionError::from(error))],
            })
        })
    }

    pub fn map_event(
        &mut self,
        event: glm::ResponseStreamEvent,
    ) -> Vec<Result<LanguageModelCompletionEvent, LanguageModelCompletionError>> {
        let mut events = Vec::new();

        if let Some(usage) = event.usage {
            events.push(Ok(LanguageModelCompletionEvent::UsageUpdate(TokenUsage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            })));
        }

        if let Some(choice) = event.choices.into_iter().next() {
            if let Some(reasoning_content) = choice.delta.reasoning_content {
                events.push(Ok(LanguageModelCompletionEvent::Thinking {
                    text: reasoning_content,
                    signature: None,
                }));
            }

            if let Some(content) = choice.delta.content {
                if !content.is_empty() {
                    events.push(Ok(LanguageModelCompletionEvent::Text(content)));
                }
            }

            if let Some(tool_calls) = choice.delta.tool_calls {
                for tool_call in tool_calls {
                    let entry = self.tool_calls_by_index.entry(tool_call.index).or_default();

                    if let Some(tool_id) = tool_call.id {
                        entry.id = tool_id;
                    }

                    if let Some(function) = tool_call.function {
                        if let Some(name) = function.name {
                            if !name.is_empty() {
                                entry.name = name;
                            }
                        }

                        if let Some(arguments) = function.arguments {
                            entry.arguments.push_str(&arguments);
                        }
                    }
                }
            }

            if let Some(finish_reason) = choice.finish_reason.as_deref() {
                match finish_reason {
                    "stop" => {
                        events.push(Ok(LanguageModelCompletionEvent::Stop(StopReason::EndTurn)));
                    }
                    "tool_calls" => {
                        events.extend(self.tool_calls_by_index.drain().map(|(_, tool_call)| {
                            match parse_tool_arguments(&tool_call.arguments) {
                                Ok(input) => Ok(LanguageModelCompletionEvent::ToolUse(
                                    LanguageModelToolUse {
                                        id: tool_call.id.into(),
                                        name: tool_call.name.into(),
                                        is_input_complete: true,
                                        input: language_model::LanguageModelToolUseInput::Json(
                                            input,
                                        ),
                                        raw_input: tool_call.arguments,
                                        thought_signature: None,
                                    },
                                )),
                                Err(error) => {
                                    Ok(LanguageModelCompletionEvent::ToolUseJsonParseError {
                                        id: tool_call.id.into(),
                                        tool_name: tool_call.name.into(),
                                        raw_input: tool_call.arguments.into(),
                                        json_parse_error: error.to_string(),
                                    })
                                }
                            }
                        }));

                        events.push(Ok(LanguageModelCompletionEvent::Stop(StopReason::ToolUse)));
                    }
                    "length" => {
                        events.push(Ok(LanguageModelCompletionEvent::Stop(
                            StopReason::MaxTokens,
                        )));
                    }
                    unexpected => {
                        log::warn!("Unexpected GLM finish_reason: {unexpected:?}");
                        events.push(Ok(LanguageModelCompletionEvent::Stop(StopReason::EndTurn)));
                    }
                }
            }
        }

        events
    }
}

#[derive(Default)]
struct RawToolCall {
    id: String,
    name: String,
    arguments: String,
}

fn add_message_content_part(
    new_part: glm::MessagePart,
    role: Role,
    messages: &mut Vec<glm::ChatMessage>,
    reasoning_content: Option<String>,
) {
    match (role, messages.last_mut()) {
        (Role::User, Some(glm::ChatMessage::User { content }))
        | (Role::System, Some(glm::ChatMessage::System { content })) => {
            content.push_part(new_part);
        }
        (
            Role::Assistant,
            Some(glm::ChatMessage::Assistant {
                content: Some(content),
                reasoning_content: message_reasoning_content,
                ..
            }),
        ) => {
            append_reasoning_content(message_reasoning_content, reasoning_content);
            content.push_part(new_part);
        }
        _ => {
            messages.push(match role {
                Role::User => glm::ChatMessage::User {
                    content: glm::MessageContent::from(vec![new_part]),
                },
                Role::Assistant => glm::ChatMessage::Assistant {
                    content: Some(glm::MessageContent::from(vec![new_part])),
                    reasoning_content,
                    tool_calls: Vec::new(),
                },
                Role::System => glm::ChatMessage::System {
                    content: glm::MessageContent::from(vec![new_part]),
                },
            });
        }
    }
}

fn append_reasoning_content(target: &mut Option<String>, content: Option<String>) {
    let Some(content) = content else {
        return;
    };
    if content.is_empty() {
        return;
    }
    target.get_or_insert_default().push_str(&content);
}

fn merge_settings_into_models(
    models: &mut HashMap<String, glm::Model>,
    available_models: &[AvailableModel],
    context_window: Option<u64>,
) {
    for setting_model in available_models {
        if let Some(model) = models.get_mut(&setting_model.name) {
            if context_window.is_none() {
                model.max_tokens = setting_model.max_tokens;
            }
            if setting_model.display_name.is_some() {
                model.display_name = setting_model.display_name.clone();
            }
            if let Some(supports_tools) = setting_model.supports_tools {
                model.supports_tools = supports_tools;
            }
            if let Some(supports_images) = setting_model.supports_images {
                model.supports_images = supports_images;
            }
            if let Some(supports_thinking) = setting_model.supports_thinking {
                model.supports_thinking = supports_thinking;
            }
        } else {
            models.insert(
                setting_model.name.clone(),
                glm::Model {
                    name: setting_model.name.clone(),
                    display_name: setting_model.display_name.clone(),
                    max_tokens: context_window.unwrap_or(setting_model.max_tokens),
                    supports_tools: setting_model.supports_tools.unwrap_or(false),
                    supports_images: setting_model.supports_images.unwrap_or(false),
                    supports_thinking: setting_model.supports_thinking.unwrap_or(false),
                },
            );
        }
    }
}
