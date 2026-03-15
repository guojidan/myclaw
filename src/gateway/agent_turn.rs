//! HTTP `v1` forum runtime contract exposed by the gateway.
//!
//! This module is the stable single-turn execution surface that `claw-forum`
//! will call through `POST /api/v1/agent-turn`. The request/response structs
//! here define the frozen wire contract for Batch 1.

use super::{api::require_auth, AppState};
use crate::agent::dispatcher::{NativeToolDispatcher, ToolDispatcher, XmlToolDispatcher};
use crate::agent::memory_loader::DefaultMemoryLoader;
use crate::agent::prompt::SystemPromptBuilder;
use crate::agent::Agent;
use crate::config::Config;
use crate::providers::{self, ConversationMessage};
use crate::runtime;
use crate::security::SecurityPolicy;
use crate::tools::{self, Tool};
use async_trait::async_trait;
use axum::{
    extract::{rejection::JsonRejection, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json},
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const REQUEST_VERSION_V1: &str = "v1";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentTurnActionType {
    CreateThread,
    ReplyPost,
    ReactPost,
    SummarizeBoard,
    ModerateCandidate,
    Skip,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentTurnResultType {
    ThreadCandidate,
    ReplyCandidate,
    ReactionCandidate,
    SummaryCandidate,
    ModerationAssist,
    NoOp,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentTurnStatus {
    Completed,
    Rejected,
    Failed,
}

/// Request payload for the frozen HTTP `v1` forum runtime contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTurnRequestV1 {
    pub request_version: String,
    pub run_id: String,
    pub idempotency_key: String,
    pub agent_id: String,
    pub action_type: AgentTurnActionType,
    pub task: AgentTurnTask,
    pub forum_context: ForumContext,
    pub agent_profile: AgentProfile,
    pub execution: AgentTurnExecution,
    #[serde(default)]
    pub trace: Option<AgentTurnTrace>,
    #[serde(default)]
    pub runtime_hints: Option<AgentTurnRuntimeHints>,
    #[serde(default)]
    pub safety_policy: Option<AgentTurnSafetyPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTurnTask {
    pub goal: String,
    pub constraints: Vec<String>,
    pub output_rules: AgentTurnOutputRules,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTurnOutputRules {
    #[serde(default)]
    pub max_words: Option<usize>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub require_citations: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForumContext {
    pub board: BoardContext,
    #[serde(default)]
    pub thread_excerpt: Option<ThreadExcerpt>,
    pub recent_posts: Vec<PostExcerpt>,
    #[serde(default)]
    pub target_post: Option<PostExcerpt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardContext {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub topic_policy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadExcerpt {
    pub thread_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostExcerpt {
    pub post_id: String,
    #[serde(default)]
    pub author_id: Option<String>,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProfile {
    pub persona_summary: String,
    pub expertise_tags: Vec<String>,
    pub style_tags: Vec<String>,
    #[serde(default)]
    pub language_preferences: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTurnExecution {
    pub timeout_secs: u64,
    pub max_tool_iterations: usize,
    pub allow_tools: bool,
    pub expect_result_type: AgentTurnResultType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTurnTrace {
    #[serde(default)]
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub parent_run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTurnRuntimeHints {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub temperature: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTurnSafetyPolicy {
    #[serde(default)]
    pub require_review: Option<bool>,
}

/// Response payload for the frozen HTTP `v1` forum runtime contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentTurnResponseV1 {
    pub request_version: String,
    pub run_id: String,
    pub idempotency_key: String,
    pub action_type: AgentTurnActionType,
    pub result_type: AgentTurnResultType,
    pub status: AgentTurnStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<AgentTurnContent>,
    pub telemetry: AgentTurnTelemetry,
    pub safety: AgentTurnSafety,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<AgentTurnError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct AgentTurnContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opening_post_markdown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opening_post_plaintext: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_markdown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_plaintext: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reaction_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_post_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_markdown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_plaintext: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct AgentTurnTelemetry {
    pub duration_ms: u128,
    pub used_tools: Vec<String>,
    pub tool_call_count: usize,
    pub memory_hit_count: usize,
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct AgentTurnSafety {
    pub requires_review: bool,
    pub flags: Vec<String>,
    pub policy_events: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentTurnError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

#[derive(Debug, Clone)]
enum AgentTurnExecutionState {
    InFlight,
    Terminal(AgentTurnResponseV1),
}

#[derive(Debug, Clone)]
struct TimedExecutionState {
    seen_at: Instant,
    state: AgentTurnExecutionState,
}

#[derive(Debug)]
pub struct AgentTurnExecutionStore {
    ttl: Duration,
    max_keys: usize,
    entries: Mutex<HashMap<String, TimedExecutionState>>,
}

#[derive(Debug, Clone)]
enum AgentTurnExecutionReservation {
    Started,
    Replay(AgentTurnResponseV1),
    InFlight,
}

impl AgentTurnExecutionStore {
    pub fn new(ttl: Duration, max_keys: usize) -> Self {
        Self {
            ttl,
            max_keys: max_keys.max(1),
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn reserve(&self, key: &str) -> AgentTurnExecutionReservation {
        let now = Instant::now();
        let mut entries = self.entries.lock();
        entries.retain(|_, entry| now.duration_since(entry.seen_at) < self.ttl);

        if let Some(existing) = entries.get(key) {
            return match &existing.state {
                AgentTurnExecutionState::InFlight => AgentTurnExecutionReservation::InFlight,
                AgentTurnExecutionState::Terminal(response) => {
                    AgentTurnExecutionReservation::Replay(response.clone())
                }
            };
        }

        if entries.len() >= self.max_keys {
            let evict_key = entries
                .iter()
                .min_by_key(|(_, entry)| entry.seen_at)
                .map(|(key, _)| key.clone());
            if let Some(evict_key) = evict_key {
                entries.remove(&evict_key);
            }
        }

        entries.insert(
            key.to_owned(),
            TimedExecutionState {
                seen_at: now,
                state: AgentTurnExecutionState::InFlight,
            },
        );
        AgentTurnExecutionReservation::Started
    }

    fn complete(&self, key: &str, response: AgentTurnResponseV1) {
        let mut entries = self.entries.lock();
        entries.insert(
            key.to_owned(),
            TimedExecutionState {
                seen_at: Instant::now(),
                state: AgentTurnExecutionState::Terminal(response),
            },
        );
    }

    fn clear(&self, key: &str) {
        self.entries.lock().remove(key);
    }
}

#[derive(Debug, Clone)]
struct AgentTurnExecutionResult {
    output: String,
    telemetry: AgentTurnTelemetry,
    safety: AgentTurnSafety,
}

#[derive(Debug, Clone)]
struct AgentTurnExecutionError {
    status_code: StatusCode,
    error: AgentTurnError,
}

#[async_trait]
trait AgentTurnExecutor: Send + Sync {
    async fn execute(
        &self,
        state: &AppState,
        request: &AgentTurnRequestV1,
    ) -> Result<AgentTurnExecutionResult, AgentTurnExecutionError>;
}

struct DefaultAgentTurnExecutor;

pub async fn handle_api_agent_turn(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<AgentTurnRequestV1>, JsonRejection>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let request = match body {
        Ok(Json(request)) => request,
        Err(err) => {
            let response = response_for_error(
                None,
                AgentTurnError {
                    code: "invalid_request".into(),
                    message: format!("Invalid JSON request body: {err}"),
                    retryable: false,
                },
            );
            return (StatusCode::BAD_REQUEST, Json(response)).into_response();
        }
    };

    if let Err(error) = validate_request(&request) {
        let response = response_for_error(Some(&request), error);
        return (StatusCode::BAD_REQUEST, Json(response)).into_response();
    }

    process_agent_turn_request(state, request, Arc::new(DefaultAgentTurnExecutor)).await
}

#[async_trait]
impl AgentTurnExecutor for DefaultAgentTurnExecutor {
    async fn execute(
        &self,
        state: &AppState,
        request: &AgentTurnRequestV1,
    ) -> Result<AgentTurnExecutionResult, AgentTurnExecutionError> {
        let mut config = state.config.lock().clone();
        apply_request_overrides(&mut config, request);

        let mut agent = build_contract_agent(state, &config, request).map_err(|err| {
            AgentTurnExecutionError {
                status_code: StatusCode::INTERNAL_SERVER_ERROR,
                error: AgentTurnError {
                    code: "provider_failure".into(),
                    message: format!("Failed to initialize runtime turn: {err}"),
                    retryable: true,
                },
            }
        })?;

        let provider_label = config
            .default_provider
            .clone()
            .unwrap_or_else(|| "openrouter".to_string());
        let model_label = config
            .default_model
            .clone()
            .unwrap_or_else(|| "anthropic/claude-sonnet-4-20250514".to_string());

        let prompt = build_agent_turn_prompt(request);
        let started_at = Instant::now();
        let output = agent
            .run_single(&prompt)
            .await
            .map_err(|err| AgentTurnExecutionError {
                status_code: StatusCode::INTERNAL_SERVER_ERROR,
                error: AgentTurnError {
                    code: "provider_failure".into(),
                    message: format!("Agent turn failed: {err}"),
                    retryable: true,
                },
            })?;

        Ok(AgentTurnExecutionResult {
            output,
            telemetry: build_telemetry(
                &agent,
                started_at.elapsed().as_millis(),
                &provider_label,
                &model_label,
            ),
            safety: AgentTurnSafety {
                requires_review: request
                    .safety_policy
                    .as_ref()
                    .and_then(|policy| policy.require_review)
                    .unwrap_or(false),
                flags: Vec::new(),
                policy_events: Vec::new(),
            },
        })
    }
}

async fn process_agent_turn_request(
    state: AppState,
    request: AgentTurnRequestV1,
    executor: Arc<dyn AgentTurnExecutor>,
) -> axum::response::Response {
    match state
        .agent_turn_execution_store
        .reserve(&request.idempotency_key)
    {
        AgentTurnExecutionReservation::Replay(response) => {
            log_agent_turn_outcome(
                &request,
                &response.status,
                true,
                response.telemetry.duration_ms,
            );
            return (StatusCode::OK, Json(response)).into_response();
        }
        AgentTurnExecutionReservation::InFlight => {
            let response = response_for_error(
                Some(&request),
                AgentTurnError {
                    code: "duplicate_request".into(),
                    message: "Request is already in progress for this idempotency key".into(),
                    retryable: true,
                },
            );
            log_agent_turn_outcome(
                &request,
                &response.status,
                false,
                response.telemetry.duration_ms,
            );
            return (StatusCode::CONFLICT, Json(response)).into_response();
        }
        AgentTurnExecutionReservation::Started => {}
    }

    let outcome = if request.action_type == AgentTurnActionType::Skip {
        Ok(build_noop_response(&request))
    } else {
        execute_agent_turn(state.clone(), &request, executor).await
    };

    match outcome {
        Ok(response) => {
            state
                .agent_turn_execution_store
                .complete(&request.idempotency_key, response.clone());
            log_agent_turn_outcome(
                &request,
                &response.status,
                false,
                response.telemetry.duration_ms,
            );
            (StatusCode::OK, Json(response)).into_response()
        }
        Err((status_code, response)) => {
            state
                .agent_turn_execution_store
                .clear(&request.idempotency_key);
            log_agent_turn_outcome(
                &request,
                &response.status,
                false,
                response.telemetry.duration_ms,
            );
            (status_code, Json(response)).into_response()
        }
    }
}

async fn execute_agent_turn(
    state: AppState,
    request: &AgentTurnRequestV1,
    executor: Arc<dyn AgentTurnExecutor>,
) -> Result<AgentTurnResponseV1, (StatusCode, AgentTurnResponseV1)> {
    let execution = executor
        .execute(&state, request)
        .await
        .map_err(|execution_error| {
            (
                execution_error.status_code,
                response_for_error(Some(request), execution_error.error),
            )
        })?;

    let parsed_content = parse_agent_output(request, &execution.output).map_err(|error| {
        (
            StatusCode::BAD_GATEWAY,
            response_for_error(Some(request), error),
        )
    })?;

    Ok(AgentTurnResponseV1 {
        request_version: REQUEST_VERSION_V1.into(),
        run_id: request.run_id.clone(),
        idempotency_key: request.idempotency_key.clone(),
        action_type: request.action_type,
        result_type: request.execution.expect_result_type,
        status: AgentTurnStatus::Completed,
        content: Some(parsed_content),
        telemetry: execution.telemetry,
        safety: execution.safety,
        error: None,
    })
}

fn validate_request(request: &AgentTurnRequestV1) -> Result<(), AgentTurnError> {
    if request.request_version.trim() != REQUEST_VERSION_V1 {
        return Err(AgentTurnError {
            code: "invalid_request".into(),
            message: format!(
                "Unsupported request_version '{}'; expected '{REQUEST_VERSION_V1}'",
                request.request_version
            ),
            retryable: false,
        });
    }
    if request.run_id.trim().is_empty()
        || request.idempotency_key.trim().is_empty()
        || request.agent_id.trim().is_empty()
        || request.task.goal.trim().is_empty()
        || request.agent_profile.persona_summary.trim().is_empty()
        || request.forum_context.board.id.trim().is_empty()
        || request.forum_context.board.title.trim().is_empty()
    {
        return Err(AgentTurnError {
            code: "invalid_request".into(),
            message: "Required request fields must be non-empty".into(),
            retryable: false,
        });
    }

    if request.execution.timeout_secs == 0 {
        return Err(AgentTurnError {
            code: "invalid_request".into(),
            message: "execution.timeout_secs must be greater than 0".into(),
            retryable: false,
        });
    }

    if request.execution.expect_result_type != expected_result_type(request.action_type) {
        return Err(AgentTurnError {
            code: "invalid_request".into(),
            message: "execution.expect_result_type does not match action_type".into(),
            retryable: false,
        });
    }

    match request.action_type {
        AgentTurnActionType::ReplyPost
        | AgentTurnActionType::ReactPost
        | AgentTurnActionType::ModerateCandidate => {
            if request.forum_context.target_post.is_none() {
                return Err(AgentTurnError {
                    code: "invalid_request".into(),
                    message: "target_post is required for this action_type".into(),
                    retryable: false,
                });
            }
        }
        _ => {}
    }

    Ok(())
}

fn expected_result_type(action_type: AgentTurnActionType) -> AgentTurnResultType {
    match action_type {
        AgentTurnActionType::CreateThread => AgentTurnResultType::ThreadCandidate,
        AgentTurnActionType::ReplyPost => AgentTurnResultType::ReplyCandidate,
        AgentTurnActionType::ReactPost => AgentTurnResultType::ReactionCandidate,
        AgentTurnActionType::SummarizeBoard => AgentTurnResultType::SummaryCandidate,
        AgentTurnActionType::ModerateCandidate => AgentTurnResultType::ModerationAssist,
        AgentTurnActionType::Skip => AgentTurnResultType::NoOp,
    }
}

fn apply_request_overrides(config: &mut Config, request: &AgentTurnRequestV1) {
    config.memory.auto_save = false;
    config.agent.max_tool_iterations = request.execution.max_tool_iterations.max(1);

    if let Some(hints) = request.runtime_hints.as_ref() {
        if let Some(provider) = hints
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            config.default_provider = Some(provider.to_string());
        }
        if let Some(model) = hints
            .model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            config.default_model = Some(model.to_string());
        }
        if let Some(temperature) = hints.temperature {
            config.default_temperature = temperature;
        }
    }
}

fn build_contract_agent(
    state: &AppState,
    config: &Config,
    request: &AgentTurnRequestV1,
) -> anyhow::Result<Agent> {
    let runtime: Arc<dyn runtime::RuntimeAdapter> =
        Arc::from(runtime::create_runtime(&config.runtime)?);
    let security = Arc::new(SecurityPolicy::from_config(
        &config.autonomy,
        &config.workspace_dir,
    ));

    let tools: Vec<Box<dyn Tool>> = if request.execution.allow_tools {
        let (composio_key, composio_entity_id) = if config.composio.enabled {
            (
                config.composio.api_key.as_deref(),
                Some(config.composio.entity_id.as_str()),
            )
        } else {
            (None, None)
        };
        tools::all_tools_with_runtime(
            Arc::new(config.clone()),
            &security,
            runtime,
            state.mem.clone(),
            composio_key,
            composio_entity_id,
            &config.browser,
            &config.http_request,
            &config.web_fetch,
            &config.workspace_dir,
            &config.agents,
            config.api_key.as_deref(),
            config,
        )
    } else {
        Vec::new()
    };

    let provider_name = config.default_provider.as_deref().unwrap_or("openrouter");
    let model_name = config
        .default_model
        .as_deref()
        .unwrap_or("anthropic/claude-sonnet-4-20250514")
        .to_string();

    let provider_runtime_options = providers::ProviderRuntimeOptions {
        auth_profile_override: None,
        provider_api_url: config.api_url.clone(),
        zeroclaw_dir: config.config_path.parent().map(std::path::PathBuf::from),
        secrets_encrypt: config.secrets.encrypt,
        reasoning_enabled: config.runtime.reasoning_enabled,
    };

    let provider = providers::create_routed_provider_with_options(
        provider_name,
        config.api_key.as_deref(),
        config.api_url.as_deref(),
        &config.reliability,
        &config.model_routes,
        &model_name,
        &provider_runtime_options,
    )?;

    let dispatcher_choice = config.agent.tool_dispatcher.as_str();
    let tool_dispatcher: Box<dyn ToolDispatcher> = match dispatcher_choice {
        "native" => Box::new(NativeToolDispatcher),
        "xml" => Box::new(XmlToolDispatcher),
        _ if provider.supports_native_tools() => Box::new(NativeToolDispatcher),
        _ => Box::new(XmlToolDispatcher),
    };

    let route_model_by_hint: HashMap<String, String> = config
        .model_routes
        .iter()
        .map(|route| (route.hint.clone(), route.model.clone()))
        .collect();
    let available_hints: Vec<String> = route_model_by_hint.keys().cloned().collect();

    Agent::builder()
        .provider(provider)
        .tools(tools)
        .memory(state.mem.clone())
        .observer(state.observer.clone())
        .tool_dispatcher(tool_dispatcher)
        .memory_loader(Box::new(DefaultMemoryLoader::new(
            5,
            config.memory.min_relevance_score,
        )))
        .prompt_builder(SystemPromptBuilder::with_defaults())
        .config(config.agent.clone())
        .model_name(model_name)
        .temperature(config.default_temperature)
        .workspace_dir(config.workspace_dir.clone())
        .classification_config(config.query_classification.clone())
        .available_hints(available_hints)
        .route_model_by_hint(route_model_by_hint)
        .identity_config(config.identity.clone())
        .skills(crate::skills::load_skills_with_config(
            &config.workspace_dir,
            config,
        ))
        .skills_prompt_mode(config.skills.prompt_injection_mode)
        .auto_save(false)
        .build()
}

fn build_agent_turn_prompt(request: &AgentTurnRequestV1) -> String {
    let schema = expected_output_schema(request.action_type);
    let board_json = serde_json::to_string_pretty(&request.forum_context.board)
        .unwrap_or_else(|_| "{}".to_string());
    let thread_json = serde_json::to_string_pretty(&request.forum_context.thread_excerpt)
        .unwrap_or_else(|_| "null".to_string());
    let target_json = serde_json::to_string_pretty(&request.forum_context.target_post)
        .unwrap_or_else(|_| "null".to_string());
    let posts_json = serde_json::to_string_pretty(&request.forum_context.recent_posts)
        .unwrap_or_else(|_| "[]".to_string());
    let profile_json =
        serde_json::to_string_pretty(&request.agent_profile).unwrap_or_else(|_| "{}".to_string());

    format!(
        "You are executing one forum runtime task for agent_id `{agent_id}`.\n\
Return ONLY valid JSON. Do not wrap the response in markdown fences.\n\
Do not include commentary before or after the JSON object.\n\
Action type: `{action_type}`.\n\
Expected result type: `{result_type}`.\n\
Goal: {goal}\n\
Constraints:\n- {constraints}\n\
Output rules:\n- max_words: {max_words}\n- format: {format_rule}\n- require_citations: {require_citations}\n\
Tools allowed: {allow_tools}\n\
\n\
Output JSON schema:\n{schema}\n\
\n\
Board context:\n{board_json}\n\
\n\
Thread excerpt:\n{thread_json}\n\
\n\
Target post:\n{target_json}\n\
\n\
Recent posts:\n{posts_json}\n\
\n\
Agent profile:\n{profile_json}\n",
        agent_id = request.agent_id,
        action_type = serde_json::to_string(&request.action_type).unwrap_or_default(),
        result_type = serde_json::to_string(&request.execution.expect_result_type)
            .unwrap_or_default(),
        goal = request.task.goal,
        constraints = if request.task.constraints.is_empty() {
            "none".to_string()
        } else {
            request.task.constraints.join("\n- ")
        },
        max_words = request
            .task
            .output_rules
            .max_words
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unspecified".to_string()),
        format_rule = request
            .task
            .output_rules
            .format
            .clone()
            .unwrap_or_else(|| "unspecified".to_string()),
        require_citations = request
            .task
            .output_rules
            .require_citations
            .map(|value| value.to_string())
            .unwrap_or_else(|| "false".to_string()),
        allow_tools = request.execution.allow_tools,
    )
}

fn expected_output_schema(action_type: AgentTurnActionType) -> &'static str {
    match action_type {
        AgentTurnActionType::CreateThread => {
            r#"{"title":"string","opening_post_markdown":"string","opening_post_plaintext":"string"}"#
        }
        AgentTurnActionType::ReplyPost => {
            r#"{"reply_markdown":"string","reply_plaintext":"string"}"#
        }
        AgentTurnActionType::ReactPost => r#"{"reaction_type":"string","target_post_id":"string"}"#,
        AgentTurnActionType::SummarizeBoard => {
            r#"{"summary_markdown":"string","summary_plaintext":"string"}"#
        }
        AgentTurnActionType::ModerateCandidate => {
            r#"{"decision":"publish|hold_for_review|reject|rewrite_once","reasoning_summary":"string"}"#
        }
        AgentTurnActionType::Skip => r#"{}"#,
    }
}

fn parse_agent_output(
    request: &AgentTurnRequestV1,
    output: &str,
) -> Result<AgentTurnContent, AgentTurnError> {
    let payload = unwrap_json_payload(output)?;
    let json_value = parse_json_object(payload)?;
    let mut content: AgentTurnContent =
        serde_json::from_value(json_value).map_err(|err| AgentTurnError {
            code: "result_validation_failed".into(),
            message: format!("Runtime output did not match the expected result schema: {err}"),
            retryable: false,
        })?;

    fill_plaintext_fallbacks(&mut content);
    validate_content_for_result_type(request, &content)?;
    Ok(content)
}

fn unwrap_json_payload(raw: &str) -> Result<&str, AgentTurnError> {
    let trimmed = raw.trim();
    if let Some(stripped) = trimmed.strip_prefix("```json") {
        return strip_code_fence(stripped);
    }
    if let Some(stripped) = trimmed.strip_prefix("```JSON") {
        return strip_code_fence(stripped);
    }
    if let Some(stripped) = trimmed.strip_prefix("```") {
        return strip_code_fence(stripped);
    }
    Ok(trimmed)
}

fn strip_code_fence(raw: &str) -> Result<&str, AgentTurnError> {
    let inner = raw.trim();
    let body = inner.strip_suffix("```").ok_or_else(|| AgentTurnError {
        code: "result_validation_failed".into(),
        message: "Runtime output used an unterminated markdown code fence".into(),
        retryable: false,
    })?;
    Ok(body.trim())
}

fn parse_json_object(payload: &str) -> Result<Value, AgentTurnError> {
    let mut deserializer = serde_json::Deserializer::from_str(payload);
    let value = Value::deserialize(&mut deserializer).map_err(|err| AgentTurnError {
        code: "result_validation_failed".into(),
        message: format!("Runtime output was not valid JSON: {err}"),
        retryable: false,
    })?;
    deserializer.end().map_err(|err| AgentTurnError {
        code: "result_validation_failed".into(),
        message: format!("Runtime output must be exactly one JSON object: {err}"),
        retryable: false,
    })?;

    if !value.is_object() {
        return Err(AgentTurnError {
            code: "result_validation_failed".into(),
            message: "Runtime output must be a JSON object".into(),
            retryable: false,
        });
    }

    Ok(value)
}

fn fill_plaintext_fallbacks(content: &mut AgentTurnContent) {
    if content.opening_post_plaintext.is_none() {
        content.opening_post_plaintext = content.opening_post_markdown.clone();
    }
    if content.reply_plaintext.is_none() {
        content.reply_plaintext = content.reply_markdown.clone();
    }
    if content.summary_plaintext.is_none() {
        content.summary_plaintext = content.summary_markdown.clone();
    }
}

fn validate_content_for_result_type(
    request: &AgentTurnRequestV1,
    content: &AgentTurnContent,
) -> Result<(), AgentTurnError> {
    let required = |condition: bool, field_name: &str| {
        if condition {
            Ok(())
        } else {
            Err(AgentTurnError {
                code: "result_validation_failed".into(),
                message: format!("Missing or empty required field '{field_name}'"),
                retryable: false,
            })
        }
    };

    match request.execution.expect_result_type {
        AgentTurnResultType::ThreadCandidate => {
            required(has_text(content.title.as_deref()), "title")?;
            required(
                has_text(content.opening_post_markdown.as_deref()),
                "opening_post_markdown",
            )?;
            required(
                has_text(content.opening_post_plaintext.as_deref()),
                "opening_post_plaintext",
            )?;
        }
        AgentTurnResultType::ReplyCandidate => {
            required(
                has_text(content.reply_markdown.as_deref()),
                "reply_markdown",
            )?;
            required(
                has_text(content.reply_plaintext.as_deref()),
                "reply_plaintext",
            )?;
        }
        AgentTurnResultType::ReactionCandidate => {
            required(has_text(content.reaction_type.as_deref()), "reaction_type")?;
            required(
                has_text(content.target_post_id.as_deref()),
                "target_post_id",
            )?;
        }
        AgentTurnResultType::SummaryCandidate => {
            required(
                has_text(content.summary_markdown.as_deref()),
                "summary_markdown",
            )?;
            required(
                has_text(content.summary_plaintext.as_deref()),
                "summary_plaintext",
            )?;
        }
        AgentTurnResultType::ModerationAssist => {
            required(has_text(content.decision.as_deref()), "decision")?;
            required(
                matches!(
                    content.decision.as_deref(),
                    Some("publish" | "hold_for_review" | "reject" | "rewrite_once")
                ),
                "decision",
            )?;
            required(
                has_text(content.reasoning_summary.as_deref()),
                "reasoning_summary",
            )?;
        }
        AgentTurnResultType::NoOp => {}
    }

    Ok(())
}

fn has_text(value: Option<&str>) -> bool {
    value.map(str::trim).is_some_and(|value| !value.is_empty())
}

fn build_telemetry(
    agent: &Agent,
    duration_ms: u128,
    provider: &str,
    model: &str,
) -> AgentTurnTelemetry {
    let mut used_tools = Vec::new();
    let mut tool_call_count = 0;

    for message in agent.history() {
        if let ConversationMessage::AssistantToolCalls { tool_calls, .. } = message {
            tool_call_count += tool_calls.len();
            for call in tool_calls {
                if !used_tools.iter().any(|name| name == &call.name) {
                    used_tools.push(call.name.clone());
                }
            }
        }
    }

    AgentTurnTelemetry {
        duration_ms,
        used_tools,
        tool_call_count,
        memory_hit_count: 0,
        provider: provider.to_string(),
        model: model.to_string(),
    }
}

fn build_noop_response(request: &AgentTurnRequestV1) -> AgentTurnResponseV1 {
    AgentTurnResponseV1 {
        request_version: REQUEST_VERSION_V1.into(),
        run_id: request.run_id.clone(),
        idempotency_key: request.idempotency_key.clone(),
        action_type: request.action_type,
        result_type: AgentTurnResultType::NoOp,
        status: AgentTurnStatus::Completed,
        content: None,
        telemetry: AgentTurnTelemetry::default(),
        safety: AgentTurnSafety::default(),
        error: None,
    }
}

fn log_agent_turn_outcome(
    request: &AgentTurnRequestV1,
    status: &AgentTurnStatus,
    replayed: bool,
    duration_ms: u128,
) {
    tracing::info!(
        run_id = %request.run_id,
        idempotency_key = %request.idempotency_key,
        agent_id = %request.agent_id,
        action_type = ?request.action_type,
        result_type = ?request.execution.expect_result_type,
        status = ?status,
        replayed,
        duration_ms,
        "agent turn request handled"
    );
}

fn response_for_error(
    request: Option<&AgentTurnRequestV1>,
    error: AgentTurnError,
) -> AgentTurnResponseV1 {
    AgentTurnResponseV1 {
        request_version: REQUEST_VERSION_V1.into(),
        run_id: request.map(|req| req.run_id.clone()).unwrap_or_default(),
        idempotency_key: request
            .map(|req| req.idempotency_key.clone())
            .unwrap_or_default(),
        action_type: request
            .map(|req| req.action_type)
            .unwrap_or(AgentTurnActionType::Skip),
        result_type: request
            .map(|req| req.execution.expect_result_type)
            .unwrap_or(AgentTurnResultType::NoOp),
        status: AgentTurnStatus::Failed,
        content: None,
        telemetry: AgentTurnTelemetry::default(),
        safety: AgentTurnSafety::default(),
        error: Some(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{
        GatewayRateLimiter, IdempotencyStore, IDEMPOTENCY_MAX_KEYS_DEFAULT,
        RATE_LIMIT_MAX_KEYS_DEFAULT,
    };
    use crate::memory::{Memory, MemoryCategory, MemoryEntry};
    use crate::providers::Provider;
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    const FIXTURE_REQUEST_REPLY_SUCCESS: &str =
        include_str!("../../tests/fixtures/agent_turn/request_reply_success.json");
    const FIXTURE_REQUEST_THREAD_SUCCESS: &str =
        include_str!("../../tests/fixtures/agent_turn/request_thread_success.json");
    const FIXTURE_RESPONSE_REPLY_SUCCESS: &str =
        include_str!("../../tests/fixtures/agent_turn/response_reply_success.json");
    const FIXTURE_RESPONSE_THREAD_SUCCESS: &str =
        include_str!("../../tests/fixtures/agent_turn/response_thread_success.json");
    const FIXTURE_RESPONSE_DUPLICATE_REQUEST: &str =
        include_str!("../../tests/fixtures/agent_turn/response_error_duplicate_request.json");
    const FIXTURE_RESPONSE_PROVIDER_FAILURE: &str =
        include_str!("../../tests/fixtures/agent_turn/response_error_provider_failure.json");
    const FIXTURE_RESPONSE_RESULT_VALIDATION_FAILED: &str = include_str!(
        "../../tests/fixtures/agent_turn/response_error_result_validation_failed.json"
    );

    struct DummyProvider;

    #[async_trait]
    impl Provider for DummyProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    struct DummyMemory;

    #[async_trait]
    impl Memory for DummyMemory {
        fn name(&self) -> &str {
            "dummy"
        }

        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    enum ExecutionStep {
        Success(&'static str),
        Failure(&'static str),
        BlockingSuccess {
            output: &'static str,
            started: Arc<Notify>,
            release: Arc<Notify>,
        },
    }

    struct ScriptedExecutor {
        steps: Mutex<VecDeque<ExecutionStep>>,
        calls: AtomicUsize,
    }

    impl ScriptedExecutor {
        fn new(steps: Vec<ExecutionStep>) -> Self {
            Self {
                steps: Mutex::new(VecDeque::from(steps)),
                calls: AtomicUsize::new(0),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl AgentTurnExecutor for ScriptedExecutor {
        async fn execute(
            &self,
            _state: &AppState,
            _request: &AgentTurnRequestV1,
        ) -> Result<AgentTurnExecutionResult, AgentTurnExecutionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let step = self
                .steps
                .lock()
                .pop_front()
                .expect("missing scripted execution step");

            match step {
                ExecutionStep::Success(output) => Ok(AgentTurnExecutionResult {
                    output: output.to_string(),
                    telemetry: AgentTurnTelemetry {
                        duration_ms: 12,
                        used_tools: Vec::new(),
                        tool_call_count: 0,
                        memory_hit_count: 0,
                        provider: "test-provider".into(),
                        model: "test-model".into(),
                    },
                    safety: AgentTurnSafety::default(),
                }),
                ExecutionStep::Failure(message) => Err(AgentTurnExecutionError {
                    status_code: StatusCode::INTERNAL_SERVER_ERROR,
                    error: AgentTurnError {
                        code: "provider_failure".into(),
                        message: message.into(),
                        retryable: true,
                    },
                }),
                ExecutionStep::BlockingSuccess {
                    output,
                    started,
                    release,
                } => {
                    started.notify_waiters();
                    release.notified().await;
                    Ok(AgentTurnExecutionResult {
                        output: output.to_string(),
                        telemetry: AgentTurnTelemetry {
                            duration_ms: 25,
                            used_tools: Vec::new(),
                            tool_call_count: 0,
                            memory_hit_count: 0,
                            provider: "test-provider".into(),
                            model: "test-model".into(),
                        },
                        safety: AgentTurnSafety::default(),
                    })
                }
            }
        }
    }

    fn sample_request(action_type: AgentTurnActionType) -> AgentTurnRequestV1 {
        AgentTurnRequestV1 {
            request_version: REQUEST_VERSION_V1.into(),
            run_id: "run-1".into(),
            idempotency_key: "idem-1".into(),
            agent_id: "claw-1".into(),
            action_type,
            task: AgentTurnTask {
                goal: "Do the thing".into(),
                constraints: vec!["Stay on topic".into()],
                output_rules: AgentTurnOutputRules {
                    max_words: Some(200),
                    format: Some("json".into()),
                    require_citations: Some(false),
                },
            },
            forum_context: ForumContext {
                board: BoardContext {
                    id: "board-1".into(),
                    title: "Board".into(),
                    description: None,
                    topic_policy: None,
                },
                thread_excerpt: None,
                recent_posts: Vec::new(),
                target_post: Some(PostExcerpt {
                    post_id: "post-1".into(),
                    author_id: Some("claw-2".into()),
                    content: "hello".into(),
                }),
            },
            agent_profile: AgentProfile {
                persona_summary: "helpful".into(),
                expertise_tags: vec!["tech".into()],
                style_tags: vec!["direct".into()],
                language_preferences: vec!["en".into()],
            },
            execution: AgentTurnExecution {
                timeout_secs: 30,
                max_tool_iterations: 3,
                allow_tools: false,
                expect_result_type: expected_result_type(action_type),
            },
            trace: None,
            runtime_hints: None,
            safety_policy: None,
        }
    }

    fn sample_state() -> AppState {
        let (event_tx, _) = tokio::sync::broadcast::channel(8);
        AppState {
            config: Arc::new(Mutex::new(Config::default())),
            provider: Arc::new(DummyProvider),
            model: "test-model".into(),
            temperature: 0.0,
            mem: Arc::new(DummyMemory),
            auto_save: false,
            webhook_secret_hash: None,
            pairing: Arc::new(crate::security::pairing::PairingGuard::new(false, &[])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(60, 60, RATE_LIMIT_MAX_KEYS_DEFAULT)),
            idempotency_store: Arc::new(IdempotencyStore::new(
                std::time::Duration::from_secs(60),
                IDEMPOTENCY_MAX_KEYS_DEFAULT,
            )),
            agent_turn_execution_store: Arc::new(AgentTurnExecutionStore::new(
                std::time::Duration::from_secs(60),
                IDEMPOTENCY_MAX_KEYS_DEFAULT,
            )),
            whatsapp: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            wati: None,
            observer: Arc::new(crate::observability::NoopObserver),
            tools_registry: Arc::new(Vec::new()),
            cost_tracker: None,
            event_tx,
        }
    }

    async fn parse_response(
        response: axum::response::Response,
    ) -> (StatusCode, AgentTurnResponseV1) {
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        let parsed = serde_json::from_slice(&body).expect("json response");
        (status, parsed)
    }

    #[test]
    fn validate_request_rejects_invalid_request_version() {
        let mut request = sample_request(AgentTurnActionType::ReplyPost);
        request.request_version = "v2".into();

        let error = validate_request(&request).expect_err("request should be rejected");
        assert_eq!(error.code, "invalid_request");
    }

    #[test]
    fn validate_request_rejects_empty_required_fields() {
        let mut request = sample_request(AgentTurnActionType::ReplyPost);
        request.run_id.clear();

        let error = validate_request(&request).expect_err("request should be rejected");
        assert_eq!(error.code, "invalid_request");
    }

    #[test]
    fn validate_request_rejects_mismatched_result_type() {
        let mut request = sample_request(AgentTurnActionType::ReplyPost);
        request.execution.expect_result_type = AgentTurnResultType::SummaryCandidate;

        let error = validate_request(&request).expect_err("request should be rejected");
        assert_eq!(error.code, "invalid_request");
    }

    #[test]
    fn parse_agent_output_accepts_fenced_json() {
        let request = sample_request(AgentTurnActionType::ReplyPost);
        let output = "```json\n{\"reply_markdown\":\"hello\",\"reply_plaintext\":\"hello\"}\n```";

        let parsed = parse_agent_output(&request, output).expect("output should parse");
        assert_eq!(parsed.reply_markdown.as_deref(), Some("hello"));
        assert_eq!(parsed.reply_plaintext.as_deref(), Some("hello"));
    }

    #[test]
    fn response_serialization_matches_frozen_wire_shape() {
        let response = AgentTurnResponseV1 {
            request_version: REQUEST_VERSION_V1.into(),
            run_id: "run-1".into(),
            idempotency_key: "idem-1".into(),
            action_type: AgentTurnActionType::ReplyPost,
            result_type: AgentTurnResultType::ReplyCandidate,
            status: AgentTurnStatus::Completed,
            content: Some(AgentTurnContent {
                reply_markdown: Some("hello".into()),
                reply_plaintext: Some("hello".into()),
                ..AgentTurnContent::default()
            }),
            telemetry: AgentTurnTelemetry {
                duration_ms: 12,
                used_tools: vec!["web_fetch".into()],
                tool_call_count: 1,
                memory_hit_count: 0,
                provider: "openrouter".into(),
                model: "anthropic/claude-sonnet-4-20250514".into(),
            },
            safety: AgentTurnSafety {
                requires_review: false,
                flags: Vec::new(),
                policy_events: Vec::new(),
            },
            error: None,
        };

        let json = serde_json::to_value(response).expect("response should serialize");
        assert_eq!(json["request_version"], "v1");
        assert_eq!(json["action_type"], "reply_post");
        assert_eq!(json["result_type"], "reply_candidate");
        assert_eq!(json["status"], "completed");
        assert_eq!(json["content"]["reply_markdown"], "hello");
        assert_eq!(json["telemetry"]["memory_hit_count"], 0);
        assert_eq!(json["telemetry"]["provider"], "openrouter");
        assert!(json.get("error").is_none());
    }

    #[test]
    fn request_fixtures_match_current_v1_schema() {
        let reply_request: AgentTurnRequestV1 =
            serde_json::from_str(FIXTURE_REQUEST_REPLY_SUCCESS).expect("reply request fixture");
        validate_request(&reply_request).expect("reply request fixture should validate");
        assert_eq!(reply_request.action_type, AgentTurnActionType::ReplyPost);
        assert_eq!(
            reply_request.execution.expect_result_type,
            AgentTurnResultType::ReplyCandidate
        );
        assert!(reply_request.forum_context.target_post.is_some());

        let thread_request: AgentTurnRequestV1 =
            serde_json::from_str(FIXTURE_REQUEST_THREAD_SUCCESS).expect("thread request fixture");
        validate_request(&thread_request).expect("thread request fixture should validate");
        assert_eq!(
            thread_request.action_type,
            AgentTurnActionType::CreateThread
        );
        assert_eq!(
            thread_request.execution.expect_result_type,
            AgentTurnResultType::ThreadCandidate
        );
        assert!(thread_request.forum_context.target_post.is_none());
    }

    #[test]
    fn response_fixtures_match_current_v1_schema() {
        let reply_success: AgentTurnResponseV1 =
            serde_json::from_str(FIXTURE_RESPONSE_REPLY_SUCCESS).expect("reply response fixture");
        assert_eq!(reply_success.status, AgentTurnStatus::Completed);
        assert_eq!(
            reply_success.result_type,
            AgentTurnResultType::ReplyCandidate
        );
        assert_eq!(reply_success.action_type, AgentTurnActionType::ReplyPost);
        assert!(reply_success
            .content
            .as_ref()
            .and_then(|content| content.reply_plaintext.as_ref())
            .is_some());

        let thread_success: AgentTurnResponseV1 =
            serde_json::from_str(FIXTURE_RESPONSE_THREAD_SUCCESS).expect("thread response fixture");
        assert_eq!(thread_success.status, AgentTurnStatus::Completed);
        assert_eq!(
            thread_success.result_type,
            AgentTurnResultType::ThreadCandidate
        );
        assert_eq!(
            thread_success.action_type,
            AgentTurnActionType::CreateThread
        );
        assert!(thread_success
            .content
            .as_ref()
            .and_then(|content| content.title.as_ref())
            .is_some());
        assert!(thread_success
            .content
            .as_ref()
            .and_then(|content| content.opening_post_plaintext.as_ref())
            .is_some());

        let duplicate_error: AgentTurnResponseV1 =
            serde_json::from_str(FIXTURE_RESPONSE_DUPLICATE_REQUEST)
                .expect("duplicate response fixture");
        assert_eq!(duplicate_error.status, AgentTurnStatus::Failed);
        assert_eq!(
            duplicate_error.error.as_ref().map(|err| err.code.as_str()),
            Some("duplicate_request")
        );

        let provider_failure: AgentTurnResponseV1 =
            serde_json::from_str(FIXTURE_RESPONSE_PROVIDER_FAILURE)
                .expect("provider failure response fixture");
        assert_eq!(provider_failure.status, AgentTurnStatus::Failed);
        assert_eq!(
            provider_failure.error.as_ref().map(|err| err.code.as_str()),
            Some("provider_failure")
        );

        let result_validation_failed: AgentTurnResponseV1 =
            serde_json::from_str(FIXTURE_RESPONSE_RESULT_VALIDATION_FAILED)
                .expect("result validation response fixture");
        assert_eq!(result_validation_failed.status, AgentTurnStatus::Failed);
        assert_eq!(
            result_validation_failed
                .error
                .as_ref()
                .map(|err| err.code.as_str()),
            Some("result_validation_failed")
        );
    }

    #[tokio::test]
    async fn skip_requests_are_replayed_without_executor_calls() {
        let state = sample_state();
        let mut request = sample_request(AgentTurnActionType::Skip);
        request.forum_context.target_post = None;
        request.execution.expect_result_type = AgentTurnResultType::NoOp;
        let executor = Arc::new(ScriptedExecutor::new(Vec::new()));

        let first =
            process_agent_turn_request(state.clone(), request.clone(), executor.clone()).await;
        let second = process_agent_turn_request(state, request, executor.clone()).await;

        let (first_status, first_body) = parse_response(first).await;
        let (second_status, second_body) = parse_response(second).await;
        assert_eq!(first_status, StatusCode::OK);
        assert_eq!(second_status, StatusCode::OK);
        assert_eq!(first_body, second_body);
        assert_eq!(executor.call_count(), 0);
    }

    #[tokio::test]
    async fn completed_reply_responses_are_replayed_for_duplicate_keys() {
        let state = sample_state();
        let request = sample_request(AgentTurnActionType::ReplyPost);
        let executor = Arc::new(ScriptedExecutor::new(vec![ExecutionStep::Success(
            r#"{"reply_markdown":"hello","reply_plaintext":"hello"}"#,
        )]));

        let first =
            process_agent_turn_request(state.clone(), request.clone(), executor.clone()).await;
        let second = process_agent_turn_request(state, request, executor.clone()).await;

        let (first_status, first_body) = parse_response(first).await;
        let (second_status, second_body) = parse_response(second).await;
        assert_eq!(first_status, StatusCode::OK);
        assert_eq!(second_status, StatusCode::OK);
        assert_eq!(first_body, second_body);
        assert_eq!(executor.call_count(), 1);
    }

    #[tokio::test]
    async fn in_flight_duplicate_returns_conflict() {
        let state = sample_state();
        let request = sample_request(AgentTurnActionType::ReplyPost);
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let executor = Arc::new(ScriptedExecutor::new(vec![
            ExecutionStep::BlockingSuccess {
                output: r#"{"reply_markdown":"hello","reply_plaintext":"hello"}"#,
                started: started.clone(),
                release: release.clone(),
            },
        ]));

        let first_state = state.clone();
        let first_request = request.clone();
        let first_executor = executor.clone();
        let first_handle = tokio::spawn(async move {
            process_agent_turn_request(first_state, first_request, first_executor).await
        });

        started.notified().await;

        let second = process_agent_turn_request(state, request, executor.clone()).await;
        let (second_status, second_body) = parse_response(second).await;
        assert_eq!(second_status, StatusCode::CONFLICT);
        assert_eq!(
            second_body.error.as_ref().map(|error| error.code.as_str()),
            Some("duplicate_request")
        );
        assert_eq!(
            second_body.error.as_ref().map(|error| error.retryable),
            Some(true)
        );

        release.notify_waiters();
        let first = first_handle.await.expect("first task should complete");
        let (first_status, _) = parse_response(first).await;
        assert_eq!(first_status, StatusCode::OK);
        assert_eq!(executor.call_count(), 1);
    }

    #[tokio::test]
    async fn provider_failure_does_not_pin_idempotency_key() {
        let state = sample_state();
        let request = sample_request(AgentTurnActionType::ReplyPost);
        let executor = Arc::new(ScriptedExecutor::new(vec![
            ExecutionStep::Failure("boom"),
            ExecutionStep::Success(r#"{"reply_markdown":"ok","reply_plaintext":"ok"}"#),
        ]));

        let first =
            process_agent_turn_request(state.clone(), request.clone(), executor.clone()).await;
        let second = process_agent_turn_request(state, request, executor.clone()).await;

        let (first_status, first_body) = parse_response(first).await;
        let (second_status, second_body) = parse_response(second).await;
        assert_eq!(first_status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            first_body.error.as_ref().map(|error| error.code.as_str()),
            Some("provider_failure")
        );
        assert_eq!(second_status, StatusCode::OK);
        assert_eq!(second_body.status, AgentTurnStatus::Completed);
        assert_eq!(executor.call_count(), 2);
    }

    #[tokio::test]
    async fn extra_text_output_maps_to_result_validation_failed() {
        let state = sample_state();
        let request = sample_request(AgentTurnActionType::ReplyPost);
        let executor = Arc::new(ScriptedExecutor::new(vec![ExecutionStep::Success(
            "Here is the result:\n{\"reply_markdown\":\"hello\",\"reply_plaintext\":\"hello\"}",
        )]));

        let response = process_agent_turn_request(state, request, executor).await;
        let (status, body) = parse_response(response).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(
            body.error.as_ref().map(|error| error.code.as_str()),
            Some("result_validation_failed")
        );
    }

    #[tokio::test]
    async fn missing_required_fields_map_to_result_validation_failed() {
        let state = sample_state();
        let request = sample_request(AgentTurnActionType::ReplyPost);
        let executor = Arc::new(ScriptedExecutor::new(vec![ExecutionStep::Success("{}")]));

        let response = process_agent_turn_request(state, request, executor).await;
        let (status, body) = parse_response(response).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(
            body.error.as_ref().map(|error| error.code.as_str()),
            Some("result_validation_failed")
        );
    }

    #[tokio::test]
    async fn reaction_candidates_must_return_target_post_id() {
        let state = sample_state();
        let request = sample_request(AgentTurnActionType::ReactPost);
        let executor = Arc::new(ScriptedExecutor::new(vec![ExecutionStep::Success(
            r#"{"reaction_type":"like"}"#,
        )]));

        let response = process_agent_turn_request(state, request, executor).await;
        let (status, body) = parse_response(response).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(
            body.error.as_ref().map(|error| error.code.as_str()),
            Some("result_validation_failed")
        );
    }
}
